//! 打开阶段的探测策略回归：封面图流与帧率取信、探测升级的触发条件。
//!
//! 资产在本机用系统 `ffmpeg` 生成（找不到 ffmpeg 就 SKIP）：
//!
//! ```sh
//! ffmpeg -f lavfi -i sine=frequency=440:duration=1 \
//!        -f lavfi -i color=c=red:s=64x64:d=1 \
//!        -map 0:a -map 1:v -c:a aac -c:v mjpeg -frames:v 1 \
//!        -disposition:v attached_pic cover_aac.m4a
//! ```
//!
//! 覆盖的缺陷：带封面的 m4a/mp3/flac 在 FFmpeg 眼里是「一条 video 流
//! （一张 JPEG）+ 一条 audio 流」。选轨时如果不排除 `attached_pic`，播放器
//! 会把一张静止图当成视频轨，宽高、帧率、时长全部对不上。

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use dyl_ffmpeg::{Demuxer, FfmpegDemuxer, TrackSelection};

mod common;

/// 找一个能用的 ffmpeg：`FFMPEG` 环境变量 → `PATH` → `$FFMPEG_DIR/bin`。
fn ffmpeg_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    if Command::new(exe)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some(PathBuf::from(exe));
    }
    if let Ok(dir) = std::env::var("FFMPEG_DIR") {
        let p = PathBuf::from(dir).join("bin").join(exe);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn asset_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DYL_TEST_ASSET_DIR") {
        return PathBuf::from(dir);
    }
    let tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tmp_verify");
    let _ = std::fs::create_dir_all(&tmp);
    tmp
}

/// 生成（或复用）一个带封面图流的音频文件，返回路径。
fn cover_asset(name: &str, audio_codec: &str) -> Option<String> {
    let dir = asset_dir();
    let path = dir.join(name);
    if path.is_file() {
        return Some(path.to_string_lossy().into_owned());
    }
    let ffmpeg = ffmpeg_bin()?;
    let status = Command::new(ffmpeg)
        .args(["-hide_banner", "-y", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
        .args(["-f", "lavfi", "-i", "color=c=red:s=64x64:d=1"])
        .args(["-map", "0:a", "-map", "1:v"])
        .args(["-c:a", audio_codec, "-c:v", "mjpeg", "-frames:v", "1"])
        .args(["-disposition:v", "attached_pic"])
        .arg(&path)
        .status()
        .ok()?;
    status
        .success()
        .then(|| path.to_string_lossy().into_owned())
}

fn open(path: &str) -> FfmpegDemuxer {
    ffmpeg_next::init().expect("ffmpeg init");
    let mut demuxer = FfmpegDemuxer::new();
    demuxer
        .open(
            path,
            &TrackSelection::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("open asset");
    demuxer
}

/// 封面图不是可播放视频轨：选轨必须跳过它，媒体信息里也不该出现视频。
#[test]
fn cover_art_is_not_a_video_track() {
    let Some(path) = cover_asset("cover_aac.m4a", "aac") else {
        eprintln!("SKIP: 找不到 ffmpeg，无法生成带封面的样本");
        return;
    };
    let demuxer = open(&path);
    let info = demuxer.info().expect("media info");

    assert!(
        info.video_stream_index.is_none(),
        "封面图被选成了视频轨（索引 {:?}）",
        info.video_stream_index
    );
    assert!(info.audio_stream_index.is_some(), "音频轨必须被选中");
    assert_eq!(info.width, 0, "封面图的宽高不该进媒体信息");
    assert_eq!(info.height, 0);
    assert!(
        info.tracks
            .iter()
            .all(|t| t.kind != dyl_ffmpeg::TrackKind::Video),
        "轨道表里不该出现封面图：{:?}",
        info.tracks
    );
}

/// mp3 的封面同样是 `attached_pic`，容器不同不影响判断。
#[test]
fn mp3_cover_art_is_not_a_video_track() {
    let Some(path) = cover_asset("cover_mp3.mp3", "libmp3lame") else {
        eprintln!("SKIP: 找不到 ffmpeg，无法生成带封面的样本");
        return;
    };
    let demuxer = open(&path);
    let info = demuxer.info().expect("media info");
    assert!(info.video_stream_index.is_none());
    assert!(info.audio_stream_index.is_some());
}

/// 正常视频的帧率必须落在可信区间内。
///
/// 1000/1 这类「容器时基伪装成帧率」的值一旦进了快照，播放器会按 1000 fps
/// 拉帧，画面会飞快跑完。
#[test]
fn video_fps_is_sane() {
    let Some(path) = common::asset_or_skip("test_video.mp4", common::HOW_TEST_VIDEO) else {
        return;
    };
    let demuxer = open(&path);
    let info = demuxer.info().expect("media info");
    assert!(info.video_stream_index.is_some(), "样本必须有视频轨");
    assert!(
        (1.0..=240.0).contains(&info.fps),
        "帧率不在可信区间：{}",
        info.fps
    );
    assert!(info.width > 0 && info.height > 0, "宽高不该是 0");
}
