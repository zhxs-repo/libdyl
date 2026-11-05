//! AV1 软解回归。
//!
//! FFmpeg 内置的 `av1` 解码器只有硬件路径：没有 hwaccel 时
//! `get_pixel_format` 直接返回 `ENOSYS`。构建里编入 `libdav1d` 后，
//! `avcodec_find_decoder(AV_CODEC_ID_AV1)` 会优先返回 dav1d（本机
//! `codec_list.c` 里它排在 `av1` 之前），所以 `Auto` 在无 AV1 硬解的设备上
//! 也落到软解。这条用例保证两条策略都真的把 `av1_test.mp4` 解出帧来。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use dyl_ffmpeg::hwaccel::Policy;
use dyl_ffmpeg::{
    Demuxer, FfmpegDemuxer, FfmpegVideoDecoder, TrackSelection, VideoCodecId, VideoDecoder,
};

fn asset() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../av1_test.mp4")
}

fn decode_all(policy: Policy) -> Option<usize> {
    if dyl_ffmpeg::capabilities::software_av1_decoder().is_none() {
        eprintln!("跳过：本构建的 FFmpeg 没有 libdav1d / libaom-av1");
        return None;
    }
    if !asset().exists() {
        eprintln!("跳过：缺少 av1_test.mp4");
        return None;
    }
    ffmpeg_next::init().expect("ffmpeg init");

    let mut demuxer = FfmpegDemuxer::new();
    demuxer
        .open(
            &asset().to_string_lossy(),
            &TrackSelection::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("open av1_test.mp4");
    let info = demuxer.info().cloned().expect("media info");
    assert_eq!(
        info.video_codec_id,
        Some(VideoCodecId::Av1),
        "容器应报告 av1"
    );
    assert_eq!((info.width, info.height), (320, 180));

    let video_idx = info.video_stream_index.expect("video stream");
    let ctx = demuxer.take_video_codec_context().expect("video context");
    let mut decoder = FfmpegVideoDecoder::with_policy(ctx, policy).expect("AV1 decoder must open");
    if policy == Policy::SoftwareOnly {
        assert!(
            decoder.hardware_device().is_none(),
            "SoftwareOnly 策略下不应启用硬解"
        );
    }

    let mut frames = 0usize;
    while let Some(packet) = demuxer.read_packet().expect("read packet") {
        if packet.stream_index != video_idx {
            continue;
        }
        decoder.submit_packet(&packet).expect("submit packet");
        while let Some(frame) = decoder.receive_frame().expect("receive frame") {
            assert_eq!((frame.width, frame.height), (320, 180));
            frames += 1;
        }
    }
    decoder.send_eof().expect("send eof");
    // EOF 后 FFmpeg 用 AVERROR_EOF（映射成 Err）而不是 Ok(None) 表示排空完毕，
    // 与 `drain_decoder` 的收尾判断一致：Err 即停止，帧数由下面的断言兜底
    while let Ok(Some(_frame)) = decoder.receive_frame() {
        frames += 1;
    }
    Some(frames)
}

#[test]
fn software_av1_decodes_real_frames() {
    let Some(frames) = decode_all(Policy::SoftwareOnly) else {
        return;
    };
    assert_eq!(frames, 60, "2 秒 30fps 的素材应解出 60 帧");
}

#[test]
fn auto_policy_still_decodes_av1() {
    // 有 AV1 硬解的设备会走硬件，没有的设备（本机）由 dav1d 接管；
    // 两种情况的共同底线是帧数完整，不能因为 auto 就解不出画面
    let Some(frames) = decode_all(Policy::Auto) else {
        return;
    };
    assert_eq!(frames, 60, "auto 策略下 AV1 也必须解出全部 60 帧");
}
