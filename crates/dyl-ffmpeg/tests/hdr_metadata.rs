//! HDR（PQ）色彩元数据传输回归 — 解码帧必须把 VUI 的传输函数带到 `VideoFrame`
//!
//! 资产不入库，本机生成后放 `tmp_verify/`（CI 没有则 SKIP）：
//!
//! ```sh
//! ffmpeg -f lavfi -i "testsrc2=duration=2:size=320x180:rate=30" \
//!   -c:v libx265 -preset medium -pix_fmt yuv420p10le -an tmp.mkv
//! # libx265 写出的 MKV 不带 Colour 元素，重封一次打上标签
//! ffmpeg -i tmp.mkv -c copy -color_trc smpte2084 -color_primaries bt2020 \
//!   -colorspace bt2020nc -color_range tv hevc10_pq_320p.mkv
//! ```
//!
//! 缺资产 SKIP；资产存在但解不出 PQ 元数据才是失败。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use dyl_ffmpeg::hwaccel::Policy;
use dyl_ffmpeg::{
    ColorPrimaries, ColorRange, ColorSpace, ColorTransfer, Demuxer, FfmpegDemuxer,
    FfmpegVideoDecoder, TrackSelection, VideoDecoder, YuvMatrix,
};

mod common;

/// 渲染端靠 `color_transfer` 决定 PQ 色调映射；这一位在帧上丢掉，HDR 画面
/// 就会被当 SDR 直出（整体发灰）。软解路径钉住它，硬解路径在回传前读同一
/// 组属性（`receive_frame`），元数据来源不分叉。
#[test]
fn pq_stream_carries_transfer_metadata_to_frames() {
    let Some(path) = common::asset_or_skip(
        "hevc10_pq_320p.mkv",
        "用系统 ffmpeg（libx265 10-bit）生成 320x180 PQ 样本，命令见 tests/hdr_metadata.rs 文件头",
    ) else {
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
        .expect("open PQ sample");
    let info = demuxer.info().cloned().expect("media info");
    let video_idx = info.video_stream_index.expect("video stream");
    let ctx = demuxer.take_video_codec_context().expect("video ctx");
    let mut decoder =
        FfmpegVideoDecoder::with_policy(ctx, Policy::SoftwareOnly).expect("video decoder");

    let mut checked = 0usize;
    while checked < 10 {
        let Some(pkt) = demuxer.read_packet().expect("read packet") else {
            break;
        };
        if pkt.stream_index != video_idx {
            continue;
        }
        decoder.submit_packet(&pkt).expect("submit packet");
        while let Some(frame) = decoder.receive_frame().expect("receive frame") {
            assert_eq!(
                frame.color_transfer,
                ColorTransfer::Pq,
                "PQ 必须从 VUI 带到帧上（pts={:?}）",
                frame.pts
            );
            assert_eq!(frame.color_primaries, ColorPrimaries::Bt2020);
            assert_eq!(frame.color_space, ColorSpace::Bt2020);
            assert_eq!(frame.color_range, ColorRange::Limited);
            assert_eq!(frame.yuv_matrix(), YuvMatrix::Bt2020);
            assert!(!frame.is_full_range());
            checked += 1;
            if checked >= 10 {
                break;
            }
        }
    }
    assert!(checked > 0, "no video frames decoded");
}
