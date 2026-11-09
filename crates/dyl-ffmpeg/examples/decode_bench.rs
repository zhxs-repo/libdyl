//! 解码吞吐基准：单文件、单策略，测“送包 + 取帧”的每帧墙钟。
//!
//! 回答的问题只有一个：这条解码通路能不能跟上素材帧率。`hw_decode_ab` 也报
//! 单帧墙钟，但它是逐帧 A/B（整文件、双路、全平面比对），跑一条 24 分钟 4K
//! 素材要按小时计；诊断"4K 硬解回传是否拖垮实时播放"只需要前 N 秒。
//!
//! 运行：
//! `cargo run -p dyl-ffmpeg --release --example decode_bench -- <file> [auto|soft|设备名] [seconds]`
//!
//! 口径：忙时包含 FFmpeg 解码、（硬解时）`av_hwframe_transfer_data` 回传、
//! 10-bit 降位深与 `VideoFrame.planes` 的 `to_vec()` 拷贝——即管线解码线程
//! 在把帧交给渲染之前要付的全部成本；读包是 demux 线程的事，单独计入墙钟。

use std::process::ExitCode;
use std::time::Instant;

use dyl_core::{DylError, Result};
use dyl_ffmpeg::hwaccel::Policy;
use dyl_ffmpeg::{Demuxer, FfmpegDemuxer, FfmpegVideoDecoder, TrackSelection, VideoDecoder};

fn is_eof_error(err: &DylError) -> bool {
    matches!(err, DylError::Decode(message) if message.contains("End of file"))
}

fn running() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let file = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "test_video.mp4".into());
    let wanted = args.get(2).cloned().unwrap_or_else(|| "auto".into());
    let seconds: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let policy = match wanted.as_str() {
        "off" | "soft" | "software" | "0" => Policy::SoftwareOnly,
        other => Policy::parse(other),
    };
    ffmpeg_next::init().expect("ffmpeg init");
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    match run(&file, policy, seconds) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("decode_bench 失败: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(file: &str, policy: Policy, seconds: f64) -> Result<()> {
    let mut demuxer = FfmpegDemuxer::new();
    demuxer.open(file, &TrackSelection::default(), running())?;
    let info = demuxer
        .info()
        .cloned()
        .ok_or_else(|| DylError::Demux("no media info".into()))?;
    let video = info
        .video_stream_index
        .ok_or_else(|| DylError::Demux("no video stream".into()))?;
    let context = demuxer
        .take_video_codec_context()
        .ok_or_else(|| DylError::Demux("no video codec context".into()))?;
    let mut decoder = FfmpegVideoDecoder::with_policy(context, policy)?;
    println!(
        "{} {}x{} {:.2}fps @ {file} (策略 {policy:?}, 时长上限 {seconds}s)",
        info.video_codec_id.map(|c| c.label()).unwrap_or("unknown"),
        info.width,
        info.height,
        info.fps,
    );

    let mut per_frame_ms: Vec<f64> = Vec::new();
    let mut finished_media = 0.0f64;
    let mut wall = 0.0f64;
    // 送包 + 取帧的总忙时：管线里 demux 是独立线程，解码线程是否跟得上只看这个
    let mut busy = 0.0f64;
    let mut frames = 0usize;

    'read: loop {
        let packet = match demuxer.read_packet()? {
            Some(packet) => packet,
            None => {
                decoder.send_eof()?;
                let start = Instant::now();
                loop {
                    match decoder.receive_frame() {
                        Ok(Some(_)) => {}
                        // FFmpeg 排空结束时用 AVERROR_EOF 报错而不是 None
                        Ok(None) => break,
                        Err(err) if is_eof_error(&err) => break,
                        Err(err) => return Err(err),
                    }
                }
                wall += start.elapsed().as_secs_f64();
                break;
            }
        };
        if packet.stream_index != video {
            continue;
        }
        let start = Instant::now();
        decoder.submit_packet(&packet)?;
        let mut packet_frames = 0usize;
        let mut reached_limit = false;
        while let Some(frame) = decoder.receive_frame()? {
            packet_frames += 1;
            frames += 1;
            if let Some(pts) = frame.pts_secs() {
                finished_media = pts;
                if pts >= seconds {
                    reached_limit = true;
                    break;
                }
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        wall += elapsed;
        busy += elapsed;
        // 一个包可能吐出多帧（B 帧重排），忙时按帧均摊，别把同一段等待记两遍
        if packet_frames > 0 {
            per_frame_ms.extend(std::iter::repeat_n(
                elapsed * 1000.0 / packet_frames as f64,
                packet_frames,
            ));
        }
        if reached_limit {
            break 'read;
        }
    }

    if per_frame_ms.is_empty() {
        return Err(DylError::Decode("no frames decoded".into()));
    }
    per_frame_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = per_frame_ms.iter().sum::<f64>() / per_frame_ms.len() as f64;
    let p95_index = ((per_frame_ms.len() - 1) as f64 * 0.95).round() as usize;
    let p95 = per_frame_ms[p95_index];
    println!(
        "帧数 {}，媒体时长 {:.2}s：墙钟（含读包）{:.2}s → {:.1} fps，解码忙时 {:.2}s → {:.1} fps",
        frames,
        finished_media,
        wall,
        frames as f64 / wall.max(1e-9),
        busy,
        frames as f64 / busy.max(1e-9),
    );
    println!(
        "每帧忙时（送包+取帧，含回传/转换/平面拷贝）mean {:.2} ms / p95 {:.2} ms / max {:.2} ms，预算 {:.1} ms",
        mean,
        p95,
        per_frame_ms.last().copied().unwrap_or(0.0),
        1000.0 / info.fps.max(1e-9),
    );
    Ok(())
}
