use serde::{Deserialize, Serialize};

use super::{CommandOp, DecodeMode, SeekMode, TrackKind, SCHEMA_VERSION};

/// 命令统一信封。
///
/// 序列化为扁平对象：`{"schema":1,"request_id":101,"op":"play"}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    #[serde(flatten)]
    pub command: Command,
}

impl CommandEnvelope {
    pub fn new(command: Command) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            request_id: None,
            command,
        }
    }

    pub fn with_request_id(request_id: u64, command: Command) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            request_id: Some(request_id),
            command,
        }
    }
}

/// 命令集合。与旧版 v4 的外部标签不同，本契约使用 `op` 内部标签。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    LoadFile {
        url: String,
    },
    Play,
    Pause,
    Stop,
    Seek {
        position_ms: u64,
        mode: SeekMode,
    },
    SeekRelative {
        delta_ms: i64,
        mode: SeekMode,
    },
    SetVolume {
        volume: u8,
    },
    SetMute {
        muted: bool,
    },
    SetSubtitleVisible {
        visible: bool,
    },
    LoadSubtitle {
        path: String,
    },
    SetDanmakuVisible {
        visible: bool,
    },
    SetDanmakuOpacity {
        opacity: u8,
    },
    LoadDanmaku {
        path: String,
    },
    /// 换用另一条音轨或字幕轨。`index` 是 `media.tracks[].index`（容器流索引）。
    ///
    /// `kind: video` 未声明支持，同步返回 `not_supported`；选到已经选中的轨是幂等
    /// 的 `ok`，不会重开管线。关字幕走 `set_subtitle_visible`，这里没有「无」的选择。
    SelectTrack {
        kind: TrackKind,
        index: u32,
    },
    /// 切换解码方式。`auto` 只在本构建声明了 `features.hw_decode` 时可用，否则同步
    /// 返回 `not_supported`；切换本身是重开管线，快照里的 `decode.backend` 是实际结果。
    SetDecodeMode {
        mode: DecodeMode,
    },
}

impl Command {
    pub const fn op(&self) -> CommandOp {
        match self {
            Self::LoadFile { .. } => CommandOp::LoadFile,
            Self::Play => CommandOp::Play,
            Self::Pause => CommandOp::Pause,
            Self::Stop => CommandOp::Stop,
            Self::Seek { .. } => CommandOp::Seek,
            Self::SeekRelative { .. } => CommandOp::SeekRelative,
            Self::SetVolume { .. } => CommandOp::SetVolume,
            Self::SetMute { .. } => CommandOp::SetMute,
            Self::SetSubtitleVisible { .. } => CommandOp::SetSubtitleVisible,
            Self::LoadSubtitle { .. } => CommandOp::LoadSubtitle,
            Self::SetDanmakuVisible { .. } => CommandOp::SetDanmakuVisible,
            Self::SetDanmakuOpacity { .. } => CommandOp::SetDanmakuOpacity,
            Self::LoadDanmaku { .. } => CommandOp::LoadDanmaku,
            Self::SelectTrack { .. } => CommandOp::SelectTrack,
            Self::SetDecodeMode { .. } => CommandOp::SetDecodeMode,
        }
    }
}
