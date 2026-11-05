use serde::{Deserialize, Serialize};

use super::{DecodeBackend, DecodeMode, ErrorCode, PlayerState, SurfaceKind, SurfaceStatus};

/// 只读快照。所有修改必须走命令。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: u32,
    pub state: PlayerState,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub volume: u8,
    pub muted: bool,
    pub decode: DecodeSnapshot,
    pub source: Option<String>,
    pub media: Option<MediaInfo>,
    pub subtitle: SubtitleSnapshot,
    pub danmaku: DanmakuSnapshot,
    pub surface: SurfaceSnapshot,
    pub stats: StatsSnapshot,
    pub last_error: Option<SnapshotError>,
}

/// 解码设置与实际生效的后端。
///
/// `mode` 是用户设置（`software` / `auto`）；`backend` 是本次管线真实使用的后端。
/// 没有打开管线时 `backend` 为 `software`：此刻没有任何解放在跑，硬解无从谈起。
/// 硬解生效时 `device` 是实际设备名（如 `d3d11va`），软解时为 `null`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeSnapshot {
    pub mode: DecodeMode,
    pub backend: DecodeBackend,
    pub device: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub video: Option<VideoStreamInfo>,
    pub audio: Option<AudioStreamInfo>,
    /// 容器里全部视频/音频/字幕轨道；`selected` 标出本次实际使用的那几条。
    /// `select_track` 换轨后内核重发 `file_loaded`，所以这个列表也是切轨的真相来源：
    /// 视频轨不可切换，位图字幕轨会以 `selected: false` 出现在这里。
    pub tracks: Vec<TrackInfo>,
}

/// 轨道种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// 一条可播放轨道的元数据
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackInfo {
    /// 容器内的流索引
    pub index: u32,
    pub kind: TrackKind,
    pub codec: String,
    /// 语言标签；容器未标注时为 `null`
    pub language: Option<String>,
    pub title: Option<String>,
    /// 容器自带的 default 标记
    pub default: bool,
    /// 本次播放实际选中的轨道
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoStreamInfo {
    /// 显示宽度（像素宽高比 SAR 已应用）
    pub width: u32,
    /// 显示高度（SAR 已应用；当前恒等于编码高度）
    pub height: u32,
    pub codec: String,
    pub fps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioStreamInfo {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleSnapshot {
    pub loaded: bool,
    pub visible: bool,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DanmakuSnapshot {
    pub loaded: bool,
    pub visible: bool,
    pub opacity: u8,
    pub count: u64,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceSnapshot {
    pub status: SurfaceStatus,
    pub kind: Option<SurfaceKind>,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub rendered_frames: u64,
    pub dropped_frames: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotError {
    pub code: ErrorCode,
    pub code_num: i32,
    pub message: String,
}
