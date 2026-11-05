use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    CommandOp, CommandStatus, DecodeMode, ErrorCode, ErrorInfo, MediaInfo, PlayerState, SeekMode,
    StateChangeReason, SurfaceKind,
};

/// 事件统一信封：`{"schema":1,"seq":1,"type":"...","data":{...}}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema: u32,
    pub seq: u64,
    #[serde(flatten)]
    pub event: Event,
}

/// 固定事件集合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    StateChanged(StateChanged),
    FileLoaded(FileLoaded),
    SeekCompleted(SeekCompleted),
    SettingsChanged(SettingsChanged),
    PositionChanged(PositionChanged),
    SurfaceAttached(SurfaceAttached),
    SurfaceDetached(SurfaceDetached),
    SurfaceResized(SurfaceResized),
    CommandCompleted(CommandCompleted),
    Warning(Warning),
    Error(ErrorEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateChanged {
    pub from: PlayerState,
    pub to: PlayerState,
    pub reason: StateChangeReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileLoaded {
    pub request_id: u64,
    pub source: String,
    pub duration_ms: u64,
    pub media: MediaInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeekCompleted {
    pub request_id: u64,
    pub from_ms: u64,
    pub to_ms: u64,
    pub actual_ms: u64,
    pub mode: SeekMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsChanged {
    pub volume: u8,
    pub muted: bool,
    pub decode_mode: DecodeMode,
    pub subtitle_visible: bool,
    pub danmaku_visible: bool,
    pub danmaku_opacity: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionChanged {
    pub position_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceAttached {
    pub kind: SurfaceKind,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceDetached {
    pub kind: SurfaceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceResized {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandCompleted {
    pub request_id: u64,
    pub op: CommandOp,
    pub status: CommandStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub code: ErrorCode,
    pub code_num: i32,
    pub message: String,
    pub fatal: bool,
}
