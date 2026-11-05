use serde::{Deserialize, Serialize};

use super::{SeekMode, SurfaceKind};

/// C ABI 版本。major 不匹配时客户端必须拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbiVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl AbiVersion {
    pub const V0_4_0: Self = Self {
        major: 0,
        minor: 4,
        patch: 0,
    };

    /// `0.5.0`：`Capabilities` 增加 `protocols`（`load_file` 接受的 URL 协议）。
    pub const V0_5_0: Self = Self {
        major: 0,
        minor: 5,
        patch: 0,
    };

    /// `0.6.0`：`Snapshot` 增加 `decode`，`settings_changed` 增加 `decode_mode`，
    /// 新命令 `set_decode_mode`，`features` 增加 `hw_decode`。
    pub const V0_6_0: Self = Self {
        major: 0,
        minor: 6,
        patch: 0,
    };

    /// `(major<<16)|(minor<<8)|patch`，与 `dyl_abi_version()` 一致。
    pub const fn packed(self) -> u32 {
        ((self.major as u32) << 16) | ((self.minor as u32) << 8) | self.patch as u32
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub schema: u32,
    pub abi: AbiVersion,
    pub platform: String,
    pub gpu_backends: Vec<String>,
    /// `load_file` 接受的 **URL** 协议，如 `http`、`https`。
    ///
    /// 值是"本构建实际编译且内核放行"的集合（ADR-13）。它确实决定
    /// `load_file` 放行哪些 scheme（其余同步 `not_supported`），因此必须如实：
    /// 没有 TLS 后端的构建（Android）只有 `http`。本地路径不是协议，不在表内；
    /// `file://` URL 同样被拒绝，本地文件请直接传路径。
    pub protocols: Vec<String>,
    pub presentation: PresentationCapabilities,
    pub features: FeatureCapabilities,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationCapabilities {
    pub external_surface: bool,
    pub texture_sharing: bool,
    pub attach_before_load: bool,
    pub embedded: bool,
    pub detached: bool,
    pub surface_kinds: Vec<SurfaceKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureCapabilities {
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    pub subtitle_formats: Vec<String>,
    /// 内嵌 ASS/SSA 轨是否走自建样式路径（对话行解析 + 容器脚本头样式表）。
    /// 仍然只代表 ASS 子集：`\t`/`\blur`/`\k` 与行内 `\r` 换样式不在其中。
    pub embedded_ass_styling: bool,
    /// 是否提供 `select_track`。只覆盖音轨与字幕轨：视频轨切换未实现。
    pub track_switching: bool,
    /// 是否实测过硬件解码。为 `false` 时 `set_decode_mode {mode: auto}` 同步
    /// `not_supported`，客户端不应显示硬解开关。为 `true` 也只说明该平台的硬解
    /// 路径可用，不构成解码白名单。
    pub hw_decode: bool,
    pub danmaku_formats: Vec<String>,
    pub danmaku_xml_entities: bool,
    pub seek_modes: Vec<SeekMode>,
    pub overlay_in_core: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// 本构建**保证**的纹理边长下界，不是产品天花板，也不是拒收大帧的依据。
    /// 实际上限由 `dyl-render` 按所选 adapter 上调（规范 ADR-14）。
    pub max_texture_dimension_2d: u32,
}

// 能力**值**不在本 crate 声明（规范 ADR-13）：后端与 Surface 句柄归 `dyl-render`，
// 编解码名归 `dyl-ffmpeg`，字幕文件格式归 `dyl-subtitle`，弹幕格式归 `dyl-danmaku`，
// seek 模式归 `dyl-pipeline`，`select_track` 与 attach/load 时序归 `dyl-engine`。
// 装配点是 `dyl_engine::capabilities()`；这里原先那张按 `env::consts::OS` 手写的
// `for_current_build()` 表就是漂移源（HEVC/FLAC 已实测可用却没被声明），故删除。
// `AbiVersion` 留在本 crate：ABI 描述的是形状，不是能力（ADR-16）。
