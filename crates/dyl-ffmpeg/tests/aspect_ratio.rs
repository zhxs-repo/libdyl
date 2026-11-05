//! 非方形像素（SAR≠1）的宽高比回归。
//!
//! `sar_9x16.mp4` 是 320x320 编码、SAR 9:16 的竖屏素材：显示尺寸必须是
//! 180x320。只按编码宽高走几何会把它摆成一个正方形（用户报告的缺陷），
//! 只改容器信息而不改解码帧则会让 HUD 与画面不一致。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use dyl_ffmpeg::hwaccel::Policy;
use dyl_ffmpeg::{Demuxer, FfmpegDemuxer, FfmpegVideoDecoder, TrackSelection, VideoDecoder};

fn sar_asset() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sar_9x16.mp4")
        .to_string_lossy()
        .to_string()
}

#[test]
fn anamorphic_stream_and_frame_report_display_dimensions() {
    ffmpeg_next::init().expect("ffmpeg init");
    let mut demuxer = FfmpegDemuxer::new();
    demuxer
        .open(
            &sar_asset(),
            &TrackSelection::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("open sar_9x16.mp4");

    let info = demuxer.info().cloned().expect("media info");
    assert_eq!((info.width, info.height), (320, 320), "编码尺寸");
    assert_eq!(
        (info.display_width, info.display_height),
        (180, 320),
        "容器信息必须应用 SAR"
    );

    let video_idx = info.video_stream_index.expect("video stream");
    let ctx = demuxer.take_video_codec_context().expect("video context");
    let mut decoder =
        FfmpegVideoDecoder::with_policy(ctx, Policy::SoftwareOnly).expect("video decoder");

    let mut decoded = None;
    while decoded.is_none() {
        let Some(packet) = demuxer.read_packet().expect("read packet") else {
            break;
        };
        if packet.stream_index != video_idx {
            continue;
        }
        decoder.submit_packet(&packet).expect("submit packet");
        while let Some(frame) = decoder.receive_frame().expect("receive frame") {
            decoded.get_or_insert(frame);
        }
    }

    let frame = decoded.expect("decoded a frame");
    assert_eq!((frame.width, frame.height), (320, 320), "平面仍是编码尺寸");
    assert_eq!(
        (frame.display_width, frame.display_height),
        (180, 320),
        "解码帧的显示尺寸必须带上 SAR"
    );
}
