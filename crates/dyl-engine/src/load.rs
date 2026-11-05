//! `dyl-load` worker — 打开文件、读媒体信息、首次加载字体、创建并启动 pipeline。
//!
//! 控制线程只接收结果；文件 I/O、进程内解码线程的创建与 GPU 无关的初始化都在
//! 这里完成，因此 `load_file` 命令永不阻塞控制线程。

use std::sync::Arc;

use dyl_core::contract::{
    AudioStreamInfo, CommandOp, DecodeMode, MediaInfo, TrackInfo, TrackKind as ContractTrackKind,
    VideoStreamInfo,
};
use dyl_core::{AudioInfo, VideoInfo};
use dyl_ffmpeg::{
    AudioCodecId, Demuxer, FfmpegDemuxer, StreamTrack, TrackKind, TrackSelection, VideoCodecId,
};
use dyl_pipeline::Pipeline;

use crate::engine::{ControlMsg, EngineShared};
use crate::error::EngineError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadPurpose {
    /// `load_file` 命令：换新文件，字幕/弹幕全部重置
    LoadFile,
    /// `play` 从 ended/stopped 重启：同一文件，保留外挂叠加层
    RestartPlay,
    /// `seek` 从 ended/stopped 重开管线：加载完成后由控制线程下发 seek
    RestartSeek,
    /// `select_track`：同一文件按新的轨道组合重开，加载完成后由控制线程恢复位置与状态
    SwitchTrack,
    /// `set_decode_mode`：同一文件按新的解码策略重开。不重发 `file_loaded`，
    /// 完成后由控制线程恢复位置与状态
    SwitchDecode,
}

pub(crate) struct LoadJob {
    pub generation: u64,
    pub request_id: u64,
    pub op: CommandOp,
    pub source: String,
    pub purpose: LoadPurpose,
    pub session: u64,
    /// 重开时用哪几条轨。全 `None` 即自动选轨；切轨必须把不想换的那几条显式带上，
    /// 否则 default 标记会让重开悄悄改掉画面
    pub selection: TrackSelection,
    /// 本 job 解视频用什么策略；每个 job 都显式携带，切解码方式立即生效
    pub decode_mode: DecodeMode,
}

pub(crate) struct LoadReady {
    pub generation: u64,
    pub request_id: u64,
    pub op: CommandOp,
    pub purpose: LoadPurpose,
    pub source: String,
    pub session: u64,
    pub pipeline: Arc<Pipeline>,
    pub media: MediaInfo,
    pub duration_ms: u64,
    pub font_missing: bool,
}

pub(crate) enum LoadOutcome {
    Ready(LoadReady),
    Failed {
        generation: u64,
        request_id: u64,
        op: CommandOp,
        purpose: LoadPurpose,
        error: EngineError,
    },
}

pub(crate) fn spawn(shared: Arc<EngineShared>, job: LoadJob) {
    let fallback = (job.generation, job.request_id, job.op, job.purpose);
    let worker_shared = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("dyl-load".into())
        .spawn(move || {
            let outcome = match build(&worker_shared, &job) {
                Ok(ready) => LoadOutcome::Ready(ready),
                Err(error) => LoadOutcome::Failed {
                    generation: job.generation,
                    request_id: job.request_id,
                    op: job.op,
                    purpose: job.purpose,
                    error,
                },
            };
            let _ = worker_shared.control_tx.send(ControlMsg::LoadDone(outcome));
        });

    if let Err(e) = spawned {
        let _ = shared
            .control_tx
            .send(ControlMsg::LoadDone(LoadOutcome::Failed {
                generation: fallback.0,
                request_id: fallback.1,
                op: fallback.2,
                purpose: fallback.3,
                error: EngineError::new(
                    dyl_core::contract::ErrorCode::Internal,
                    format!("spawn load worker: {e}"),
                ),
            }));
    }
}

fn build(shared: &Arc<EngineShared>, job: &LoadJob) -> Result<LoadReady, EngineError> {
    // 只有本地路径才做存在性预检：URL 由 FFmpeg 的协议层打开，连不上会在
    // `demuxer.open` 里变成 demux 错误；不认识的 scheme 在这里就是 not_supported，
    // 不会被 `Path::exists()` 误报成 file_not_found
    match dyl_ffmpeg::classify_source(&job.source) {
        Ok(dyl_ffmpeg::SourceKind::LocalPath) => {
            if !std::path::Path::new(&job.source).exists() {
                return Err(EngineError::new(
                    dyl_core::contract::ErrorCode::FileNotFound,
                    format!("file not found: {}", job.source),
                ));
            }
        }
        Ok(_) => {}
        Err(e) => return Err(e.into()),
    }

    // 先建 pipeline 再打开来源：demuxer 的阻塞 IO 观察它的 shutdown 标志，
    // 播放中 stop/destroy 不会被卡到 rw_timeout。加载阶段这次 open 还没有
    // 控制线程可知的 pipeline，中断位无人置位，网络连接由 rw_timeout（15s）兜底
    let mut pipeline = Pipeline::new(shared.pipe_tx.clone(), job.session);
    pipeline.pause();

    let mut demuxer = FfmpegDemuxer::new();
    demuxer.open(&job.source, &job.selection, pipeline.shutdown.clone())?;
    let info = demuxer
        .info()
        .cloned()
        .ok_or_else(|| EngineError::new(dyl_core::contract::ErrorCode::Demux, "no media info"))?;

    match job.purpose {
        LoadPurpose::LoadFile => shared.overlays.reset_for_new_media(),
        // 同一文件重开：外挂字幕与弹幕留着，内嵌字幕事件由新管线重新喂
        LoadPurpose::RestartPlay
        | LoadPurpose::RestartSeek
        | LoadPurpose::SwitchTrack
        | LoadPurpose::SwitchDecode => shared.overlays.reset_embedded_subtitle(),
    }
    let font_missing = shared.overlays.ensure_font(&shared.font_loaded);
    shared.output.set_overlays(shared.overlays.overlays());

    let media = build_media(&info);
    let duration_secs = if info.duration.is_finite() {
        info.duration.max(0.0)
    } else {
        0.0
    };
    let duration_ms = (duration_secs * 1000.0).round() as u64;
    let video_info = info.video_stream_index.map(|_| VideoInfo {
        // 显示尺寸（SAR 已应用）：客户端拿 width/height 算宽高比时应当得到
        // 真实显示比例，而不是非方形像素流的编码尺寸
        width: info.display_width,
        height: info.display_height,
        codec: video_codec_name(info.video_codec_id).to_string(),
        fps: info.fps,
    });
    let audio_info = info.audio_stream_index.map(|_| AudioInfo {
        codec: audio_codec_name(info.audio_codec_id).to_string(),
        channels: info.channels,
        sample_rate: info.sample_rate,
    });

    pipeline.start(
        &job.source,
        Box::new(demuxer) as Box<dyn Demuxer>,
        shared.output.clone(),
        duration_secs,
        video_info,
        audio_info,
        Some(shared.overlays.subtitle.clone()),
        decode_policy(job.decode_mode),
    )?;

    Ok(LoadReady {
        generation: job.generation,
        request_id: job.request_id,
        op: job.op,
        purpose: job.purpose,
        source: job.source.clone(),
        session: job.session,
        pipeline: Arc::new(pipeline),
        media,
        duration_ms,
        font_missing,
    })
}

fn build_media(info: &dyl_ffmpeg::MediaInfo) -> MediaInfo {
    MediaInfo {
        video: info.video_stream_index.map(|_| VideoStreamInfo {
            // 契约里的 width/height 是显示尺寸（SAR 已应用）。容器编码分辨率
            // 对客户端没有几何意义，非方形像素流（如 1080x1080 + SAR 9:16）
            // 若上报编码尺寸，UI 会按正方形展示并给出错误的宽高比
            width: info.display_width,
            height: info.display_height,
            codec: video_codec_name(info.video_codec_id).to_string(),
            fps: info.fps,
        }),
        audio: info.audio_stream_index.map(|_| AudioStreamInfo {
            codec: audio_codec_name(info.audio_codec_id).to_string(),
            channels: info.channels,
            sample_rate: info.sample_rate,
        }),
        tracks: info.tracks.iter().map(contract_track).collect(),
    }
}

fn contract_track(track: &StreamTrack) -> TrackInfo {
    TrackInfo {
        index: track.index as u32,
        kind: match track.kind {
            TrackKind::Video => ContractTrackKind::Video,
            TrackKind::Audio => ContractTrackKind::Audio,
            TrackKind::Subtitle => ContractTrackKind::Subtitle,
        },
        codec: track.codec.clone(),
        language: track.language.clone(),
        title: track.title.clone(),
        default: track.default,
        selected: track.selected,
    }
}

fn video_codec_name(codec: Option<VideoCodecId>) -> &'static str {
    codec.map_or("unknown", VideoCodecId::label)
}

fn decode_policy(mode: DecodeMode) -> dyl_ffmpeg::hwaccel::Policy {
    match mode {
        DecodeMode::Software => dyl_ffmpeg::hwaccel::Policy::SoftwareOnly,
        DecodeMode::Auto => dyl_ffmpeg::hwaccel::Policy::Auto,
    }
}

fn audio_codec_name(codec: Option<AudioCodecId>) -> &'static str {
    codec.map_or("unknown", AudioCodecId::label)
}
