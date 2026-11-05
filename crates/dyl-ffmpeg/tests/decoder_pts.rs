//! 解码时间戳语义回归测试 — 使用工作区内的真实样例文件
//!
//! 覆盖两个历史缺陷：
//! 1. NOPTS 帧被兜底成 ts=0（"无时间戳" 与 "0 秒" 混淆）
//! 2. 音频帧 PTS 的时间基与 Packet 时间基不一致（MKV 是 1/1000，按采样率换算
//!    会把秒数缩小几十倍，音频主时钟锚点变负，播放整体拖慢）

use dyl_ffmpeg::{
    AudioDecoder, Demuxer, FfmpegAudioDecoder, FfmpegDemuxer, FfmpegVideoDecoder, Packet,
    TrackSelection, VideoDecoder,
};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// 工作区根目录下的样例文件（相对 manifest 定位，避免绝对路径）
fn sample_video() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_video.mp4")
}

fn open_sample() -> FfmpegDemuxer {
    ffmpeg_next::init().expect("ffmpeg init");
    let path = sample_video();
    if !path.exists() {
        panic!("sample file missing: {}", path.display());
    }
    let mut demuxer = FfmpegDemuxer::new();
    demuxer
        .open(
            &path.to_string_lossy(),
            &TrackSelection::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("open sample file");
    demuxer
}

/// 时间基有效时，Packet 的 tick 与秒必须一一对应；NOPTS 必须保持 None
#[test]
fn demuxer_preserves_ticks_and_nopts() {
    let mut demuxer = open_sample();
    let info = demuxer.info().cloned().unwrap();
    let audio_idx = info.audio_stream_index.expect("sample has audio stream");

    let mut seen = 0usize;
    let mut nopts = 0usize;
    let mut timed = 0usize;
    while seen < 40 {
        let pkt = match demuxer.read_packet().expect("read_packet") {
            Some(p) => p,
            None => break,
        };
        if pkt.stream_index != audio_idx {
            continue;
        }
        seen += 1;
        match pkt.pts {
            // 关键：无时间戳的包必须是 None，而不是被写成 Some(0)
            None => {
                nopts += 1;
                assert_eq!(pkt.pts_secs(), None);
            }
            Some(ticks) => {
                timed += 1;
                // tick→秒 换算必须使用包自身携带的流时间基
                let expected = ticks as f64 * pkt.time_base.numerator() as f64
                    / pkt.time_base.denominator() as f64;
                assert!((pkt.pts_secs().unwrap() - expected).abs() < 1e-9);
            }
        }
    }
    assert!(seen > 0, "no audio packets read");
    assert_eq!(
        nopts + timed,
        seen,
        "each packet must be either timed or NOPTS"
    );
}

/// 音频帧 PTS 沿用它所属 Packet 的时间基（FFmpeg 解码器约定，见 `frame_timebase`）。
/// MP4 的音频轨道 timescale 恰好等于采样率，所以这里两种换算结果相同；
/// 增量仍必须等于帧内采样数 / 采样率
#[test]
fn audio_frame_pts_follows_packet_timebase() {
    let mut demuxer = open_sample();
    let info = demuxer.info().cloned().unwrap();
    let audio_idx = info.audio_stream_index.unwrap();
    let sample_rate = info.sample_rate;
    let ctx = demuxer.take_audio_codec_context().expect("audio ctx");
    let mut decoder = FfmpegAudioDecoder::from_stream_context(ctx).expect("audio decoder");

    let mut prev_frame: Option<(f64, usize)> = None;
    let mut checked = 0usize;
    let mut frames_total = 0usize;

    while checked < 20 && frames_total < 64 {
        let pkt = match demuxer.read_packet().expect("read") {
            Some(p) => p,
            None => break,
        };
        if pkt.stream_index != audio_idx {
            continue;
        }
        decoder.submit_packet(&pkt).expect("submit");
        while let Some(frame) = decoder.receive_frame().expect("receive") {
            frames_total += 1;
            // 静音缺陷回归点：AAC 输出帧不填 ch_layout，早期实现按 0 声道计算，
            // 每帧的 samples 都是空的，声音通路整条链路静默
            assert!(
                frame.channels > 0 && !frame.samples.is_empty(),
                "decoded audio frame must carry samples (pts={:?}, channels={}, len={})",
                frame.pts,
                frame.channels,
                frame.samples.len()
            );
            assert_eq!(
                (frame.time_base.numerator(), frame.time_base.denominator()),
                (pkt.time_base.numerator(), pkt.time_base.denominator()),
                "audio frame time_base must be the packet's stream time_base"
            );
            let samples_in_frame = if frame.channels > 0 {
                frame.samples.len() / frame.channels as usize
            } else {
                0
            };
            if let Some(secs) = frame.pts_secs() {
                if let Some((prev, prev_samples)) = prev_frame {
                    assert!(secs > prev, "audio pts must advance: {prev} -> {secs}");
                    // 增量由上一帧的时长决定；空帧（无采样）不参与严格校验
                    if prev_samples > 0 {
                        let expected = prev_samples as f64 / sample_rate as f64;
                        assert!(
                            (secs - prev - expected).abs() < 1e-6,
                            "audio pts delta {:+.6}s != prev samples/rate {:.6}s (pts drift)",
                            secs - prev,
                            expected
                        );
                    }
                    checked += 1;
                }
                prev_frame = Some((secs, samples_in_frame));
            }
        }
    }
    assert!(
        frames_total >= 20,
        "expected many decoded audio frames, got {frames_total}"
    );
    assert!(
        checked >= 10,
        "expected timestamped audio frames, only checked {checked}"
    );
    // 首帧之后的时间戳必须已进入真实的百毫秒级推进，而不是恒为 0
    let (last_secs, _) = prev_frame.expect("timestamped audio frames");
    assert!(last_secs > 0.2, "audio pts stuck near zero: {last_secs:?}");
}

/// 视频帧 PTS 是 Packet PTS 的透传：tick 值必须来自输入包，时间基必须是流时间基
/// （旧实现把 avctx.pkt_timebase 缺失时硬编码为 1/90000，换算出的秒数是错的）
#[test]
fn video_frame_pts_inherits_packet_timebase() {
    let mut demuxer = open_sample();
    let info = demuxer.info().cloned().unwrap();
    let video_idx = info.video_stream_index.unwrap();

    // 先收集一批视频包，记录其 pts tick 集合
    let mut packets: Vec<Packet> = Vec::new();
    while packets.len() < 64 {
        match demuxer.read_packet().expect("read") {
            Some(p) if p.stream_index == video_idx => packets.push(p),
            Some(_) => {}
            None => break,
        }
    }
    let stream_tb = packets.first().expect("video packets").time_base;
    let packet_pts: std::collections::HashSet<i64> = packets.iter().filter_map(|p| p.pts).collect();
    assert!(!packet_pts.is_empty(), "video packets should carry pts");

    let ctx = demuxer.take_video_codec_context().expect("video ctx");
    let mut decoder = FfmpegVideoDecoder::from_stream_context(ctx).expect("video decoder");

    let mut prev: Option<i64> = None;
    let mut timed = 0usize;
    for pkt in &packets {
        decoder.submit_packet(pkt).expect("submit");
        // seek 到文件末尾式的 drain：不主动 flush，只取当前可用帧
        while let Some(frame) = decoder.receive_frame().expect("receive") {
            assert_eq!(
                frame.time_base, stream_tb,
                "video frame time_base must follow the packet's stream time_base"
            );
            if let Some(pts) = frame.pts {
                assert!(
                    packet_pts.contains(&pts),
                    "frame pts {pts} not present in input packet pts set"
                );
                if let Some(p) = prev {
                    assert!(
                        pts >= p,
                        "video pts must not go backwards in display order: {p} -> {pts}"
                    );
                }
                prev = Some(pts);
                timed += 1;
                let secs = frame.pts_secs().unwrap();
                let expected =
                    pts as f64 * stream_tb.numerator() as f64 / stream_tb.denominator() as f64;
                assert!((secs - expected).abs() < 1e-9);
            } else {
                assert_eq!(frame.pts_secs(), None, "NOPTS must not yield seconds");
            }
        }
    }
    decoder.flush();
    while let Some(frame) = decoder.receive_frame().expect("drain") {
        if frame.pts.is_some() {
            timed += 1;
        }
    }
    assert!(timed >= 8, "decoded only {timed} timestamped video frames");
    assert!(prev.unwrap_or(0) > 0, "video pts should advance");
}
