//! Integration tests for dyl-ffmpeg demuxer and decoder
//!
//! These tests exercise the Demuxer/Decoder traits with real FFmpeg calls.
//! Some tests require reference media files from the Erika third_party directory.

use dyl_core::DylError;
use dyl_ffmpeg::{Demuxer, FfmpegDemuxer, MediaInfo, TrackSelection};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn running() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// 工作区根目录下的样例文件（相对 manifest 定位，避免绝对路径）
fn sample_video() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test_video.mp4")
        .to_string_lossy()
        .to_string()
}

fn open_sample(selection: &TrackSelection) -> MediaInfo {
    ffmpeg_next::init().expect("ffmpeg init");
    let mut demuxer = FfmpegDemuxer::new();
    demuxer
        .open(&sample_video(), selection, running())
        .expect("open test_video.mp4");
    demuxer.info().cloned().expect("media info after open")
}

/// 切轨的实现是「带着显式索引重开」，所以重开绝不能改变画面：显式选中自动
/// 选中的那几条时，视频/音频参数与 selected 标记必须与自动选择完全一致
#[test]
fn explicit_selection_reproduces_auto_selection() {
    let auto = open_sample(&TrackSelection::default());
    let forced = open_sample(&TrackSelection {
        video: auto.video_stream_index,
        audio: auto.audio_stream_index,
        subtitle: auto.subtitle_stream_index,
    });

    assert_eq!(forced.video_stream_index, auto.video_stream_index);
    assert_eq!(forced.audio_stream_index, auto.audio_stream_index);
    assert_eq!(forced.subtitle_stream_index, auto.subtitle_stream_index);
    assert_eq!((forced.width, forced.height), (auto.width, auto.height));
    assert_eq!(forced.fps, auto.fps);
    assert_eq!(
        (forced.sample_rate, forced.channels),
        (auto.sample_rate, auto.channels)
    );
    let selected = |info: &MediaInfo| {
        info.tracks
            .iter()
            .filter(|t| t.selected)
            .map(|t| t.index)
            .collect::<Vec<_>>()
    };
    assert_eq!(selected(&forced), selected(&auto));
}

/// 指到别的类型的流必须报错，不能让内核擅自改选一条（那会把画面换掉）
#[test]
fn explicit_selection_rejects_stream_of_the_other_kind() {
    let auto = open_sample(&TrackSelection::default());
    let video = auto.video_stream_index.expect("sample has a video stream");
    let err = open_sample_err(TrackSelection {
        video: None,
        audio: Some(video),
        subtitle: None,
    });
    assert!(matches!(err, DylError::InvalidArgument(_)), "{err}");
}

#[test]
fn explicit_selection_rejects_stream_that_is_not_in_the_file() {
    let err = open_sample_err(TrackSelection {
        video: None,
        audio: Some(9_999),
        subtitle: None,
    });
    assert!(matches!(err, DylError::InvalidArgument(_)), "{err}");
}

fn open_sample_err(selection: TrackSelection) -> DylError {
    ffmpeg_next::init().expect("ffmpeg init");
    let mut demuxer = FfmpegDemuxer::new();
    demuxer
        .open(&sample_video(), &selection, running())
        .expect_err("selection must be rejected")
}

/// Path to the h264 reference file in the Erika third_party directory
fn h264_ref_path() -> String {
    // Try relative to workspace root
    let candidates = [
        "D:/Devs/github/dyl-player/Erika/third_party/src/ffmpeg-7.1.1/tests/ref/lavf-fate/h264.mp4",
    ];
    for path in &candidates {
        if std::path::Path::new(path).exists() {
            return path.to_string();
        }
    }
    String::new()
}

#[test]
fn demuxer_open_nonexistent_file() {
    let mut demuxer = FfmpegDemuxer::new();
    let result = demuxer.open(
        "C:\\nonexistent\\fake.mp4",
        &TrackSelection::default(),
        running(),
    );
    assert!(result.is_err());
    if let Err(DylError::Demux(msg)) = result {
        assert!(msg.contains("failed to open"));
    } else {
        panic!("Expected Demux error");
    }
}

#[test]
fn demuxer_info_before_open() {
    let demuxer = FfmpegDemuxer::new();
    assert!(demuxer.info().is_none());
}

#[test]
fn demuxer_read_packet_before_open() {
    let mut demuxer = FfmpegDemuxer::new();
    let result = demuxer.read_packet();
    assert!(result.is_err());
}

#[test]
fn demuxer_seek_before_open() {
    let mut demuxer = FfmpegDemuxer::new();
    let result = demuxer.seek(1.0);
    assert!(result.is_err());
}

#[test]
fn demuxer_open_h264_ref() {
    let path = h264_ref_path();
    if path.is_empty() {
        eprintln!("SKIP: h264.mp4 reference file not found");
        return;
    }

    let mut demuxer = FfmpegDemuxer::new();
    // The FATE reference file is very small (161 bytes), may fail to open
    // but should at least parse the header without a crash
    match demuxer.open(&path, &TrackSelection::default(), running()) {
        Ok(()) => {
            let info = demuxer.info().expect("info should be available after open");
            println!("MediaInfo: {:?}", info);
            // Verify basic info is populated
            assert!(
                info.video_stream_index.is_some() || info.audio_stream_index.is_some(),
                "At least one stream should be detected"
            );
        }
        Err(e) => {
            // It's acceptable if the tiny reference file can't be fully parsed
            eprintln!("h264.mp4 open failed (expected for tiny ref files): {}", e);
        }
    }
}

#[test]
fn demuxer_default_constructor() {
    let demuxer = FfmpegDemuxer::default();
    assert!(demuxer.info().is_none());
}
