//! 非方形像素（SAR≠1）的宽高比回归。
//!
//! `sar_9x16.mp4` 是 320x320 编码、SAR 9:16 的竖屏素材：显示尺寸必须是
//! 180x320。只按编码宽高走几何会把它摆成一个正方形（用户报告的缺陷），
//! 只改容器信息而不改解码帧则会让 HUD 与画面不一致。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use dyl_ffmpeg::hwaccel::Policy;
use dyl_ffmpeg::{Demuxer, FfmpegDemuxer, FfmpegVideoDecoder, TrackSelection, VideoDecoder};

mod common;

#[test]
fn anamorphic_stream_and_frame_report_display_dimensions() {
    let Some(path) = common::asset_or_skip("sar_9x16.mp4", common::HOW_SAR) else {
        return;
    };
    ffmpeg_next::init().expect("ffmpeg init");
    let mut demuxer = FfmpegDemuxer::new();
    demuxer
        .open(
            &path,
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
            // 帧线程会把不满一个线程组的短素材全部压在解码器里，必须送 EOF
            // 才会吐出；10 帧的样片在 threads=auto 下正是这种情况
            decoder.send_eof().expect("send eof");
            while decoded.is_none() {
                match decoder.receive_frame() {
                    Ok(Some(frame)) => {
                        decoded = Some(frame);
                    }
                    // FFmpeg 在真正排空后以 AVERROR_EOF 结束
                    Ok(None) | Err(_) => break,
                }
            }
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
