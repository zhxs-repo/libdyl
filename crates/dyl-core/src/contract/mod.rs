//! 契约类型：命令、事件、快照、Surface、能力、错误码、状态矩阵。
//!
//! 本模块与 v4 类型完全独立，是 `libdyl/contract/` golden fixtures
//! 的 Rust 侧镜像，当前只用于解析/序列化测试。

use serde::{Deserialize, Serialize};

pub mod capabilities;
pub mod command;
pub mod errors;
pub mod event;
pub mod matrix;
pub mod snapshot;
pub mod surface;

pub use capabilities::{
    AbiVersion, Capabilities, FeatureCapabilities, Limits, PresentationCapabilities,
};
pub use command::{Command, CommandEnvelope};
pub use errors::{error_string, ErrorCatalog, ErrorCatalogEntry, ErrorCode};
pub use event::{
    CommandCompleted, ErrorEvent, Event, EventEnvelope, FileLoaded, PositionChanged, SeekCompleted,
    SettingsChanged, StateChanged, SurfaceAttached, SurfaceDetached, SurfaceResized, Warning,
};
pub use matrix::{MatrixCell, MatrixStatus, RejectSpec, SameTarget, StateMatrix, TransitionTarget};
pub use snapshot::{
    AudioStreamInfo, DanmakuSnapshot, DecodeSnapshot, MediaInfo, Snapshot, SnapshotError,
    StatsSnapshot, SubtitleSnapshot, SurfaceSnapshot, TrackInfo, TrackKind, VideoStreamInfo,
};
pub use surface::SurfaceDescriptor;

/// 所有契约 JSON 的版本。
pub const SCHEMA_VERSION: u32 = 1;

/// 播放器状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerState {
    Idle,
    Loading,
    Ready,
    Playing,
    Paused,
    Ended,
    Stopped,
    Error,
}

impl PlayerState {
    pub const ALL: [Self; 8] = [
        Self::Idle,
        Self::Loading,
        Self::Ready,
        Self::Playing,
        Self::Paused,
        Self::Ended,
        Self::Stopped,
        Self::Error,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Ended => "ended",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }
}

/// `state_changed` 事件的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateChangeReason {
    Load,
    Play,
    Pause,
    Stop,
    Seek,
    Eof,
    Error,
    Restart,
}

/// Seek 模式：`exact` 帧精确、`keyframe` 落在目标前最近的关键帧。
/// 两者都由能力位 `seek_modes` 声明；未声明的模式在同步阶段返回 `not_supported`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeekMode {
    Exact,
    Keyframe,
}

/// 解码方式：`software` 只用 CPU，`auto` 在能力位允许时优先硬件、失败回落 CPU。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeMode {
    Software,
    Auto,
}

/// 当前实际生效的解码后端。`software` 时 `device` 为空。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeBackend {
    Software,
    Hardware,
}

/// 命令 `op` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandOp {
    LoadFile,
    Play,
    Pause,
    Stop,
    Seek,
    SeekRelative,
    SetVolume,
    SetMute,
    SetSubtitleVisible,
    LoadSubtitle,
    SetDanmakuVisible,
    SetDanmakuOpacity,
    LoadDanmaku,
    SelectTrack,
    SetDecodeMode,
}

impl CommandOp {
    pub const ALL: [Self; 15] = [
        Self::LoadFile,
        Self::Play,
        Self::Pause,
        Self::Stop,
        Self::Seek,
        Self::SeekRelative,
        Self::SetVolume,
        Self::SetMute,
        Self::SetSubtitleVisible,
        Self::LoadSubtitle,
        Self::SetDanmakuVisible,
        Self::SetDanmakuOpacity,
        Self::LoadDanmaku,
        Self::SelectTrack,
        Self::SetDecodeMode,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LoadFile => "load_file",
            Self::Play => "play",
            Self::Pause => "pause",
            Self::Stop => "stop",
            Self::Seek => "seek",
            Self::SeekRelative => "seek_relative",
            Self::SetVolume => "set_volume",
            Self::SetMute => "set_mute",
            Self::SetSubtitleVisible => "set_subtitle_visible",
            Self::LoadSubtitle => "load_subtitle",
            Self::SetDanmakuVisible => "set_danmaku_visible",
            Self::SetDanmakuOpacity => "set_danmaku_opacity",
            Self::LoadDanmaku => "load_danmaku",
            Self::SelectTrack => "select_track",
            Self::SetDecodeMode => "set_decode_mode",
        }
    }
}

/// `command_completed.status`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Ok,
    Error,
    Cancelled,
    Superseded,
}

/// Surface 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    WindowsHwnd,
    MacosNsview,
    X11Window,
    WaylandSurface,
    AndroidNativeWindow,
}

/// 快照中的 Surface 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceStatus {
    None,
    Attached,
    Detached,
    Lost,
    Error,
}

/// `command_completed.error` 与矩阵拒绝对象：只含字符串码与数字码。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    pub code_num: i32,
}

impl ErrorInfo {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            code_num: code.as_i32(),
        }
    }
}
