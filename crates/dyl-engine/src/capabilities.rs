//! 能力 JSON 的装配点（规范 ADR-13）。
//!
//! `dyl-core::contract` 只定义 `Capabilities` 的**形状**；每个值都来自实现它的 crate：
//!
//! | 值 | 声明方 |
//! | :--- | :--- |
//! | `gpu_backends`、`surface_kinds`、`embedded`/`detached`、`overlay_in_core`、`limits` | `dyl-render` |
//! | `protocols`（`load_file` 接受的 URL 协议） | `dyl-ffmpeg` |
//! | `video_codecs`、`audio_codecs`、`hw_decode` | `dyl-ffmpeg` |
//! | `subtitle_formats`、`embedded_ass_styling` | `dyl-subtitle` |
//! | `danmaku_formats`、`danmaku_xml_entities` | `dyl-danmaku` |
//! | `seek_modes` | `dyl-pipeline` |
//! | `track_switching`、`attach_before_load` | 本模块（编排在 engine 里实现） |
//!
//! engine 是唯一的装配点，因为它是同时依赖上述所有 crate 的最上层：`dyl-ffi` 只准依赖
//! `dyl-core` + `dyl-engine`，所以它不能自己去取 `dyl-render` 的后端清单。
//! 装配后的清单里不该再出现任何一个手抄的字符串常量——那正是这次改动要消掉的漂移源。

use dyl_core::contract::{
    AbiVersion, Capabilities, Limits, PresentationCapabilities, SCHEMA_VERSION,
};

/// `select_track` 是否可用。**只覆盖音轨与文本字幕轨**：
/// `kind: video` 在 `engine::validate_command` 里同步返回 `not_supported`（规范 2.3 把它
/// 记为暂缺项，理由清单就是待办工作项），客户端不得据此摆出视频轨菜单。
pub const TRACK_SWITCHING: bool = true;

/// `load_file` 与 `surface_attach` 无先后要求：没有 Surface 时视频帧丢弃、音频与时钟继续
/// （`dyl-render::NullOutput`），attach 之后再换回真实输出。
pub const ATTACH_BEFORE_LOAD: bool = true;

/// 当前构建的能力清单。
///
/// 值随代码走：关掉 workspace 的 `gles` feature，`gpu_backends` 会自动少掉一项；给
/// `VideoCodecId` 加一条已验收的编解码（或让 FFmpeg 构建带上 `libdav1d`，AV1 软解
/// 随之出现），`video_codecs` 会跟着变。这些变化**不升 ABI**（规范 ADR-16：ABI
/// 只描述形状）。
pub fn capabilities() -> Capabilities {
    Capabilities {
        schema: SCHEMA_VERSION,
        abi: AbiVersion::V0_6_0,
        platform: std::env::consts::OS.to_string(),
        gpu_backends: dyl_render::capabilities::gpu_backends(),
        protocols: dyl_ffmpeg::capabilities::source_protocols(),
        presentation: PresentationCapabilities {
            external_surface: dyl_render::capabilities::EXTERNAL_SURFACE,
            texture_sharing: dyl_render::capabilities::TEXTURE_SHARING,
            attach_before_load: ATTACH_BEFORE_LOAD,
            embedded: dyl_render::capabilities::EMBEDS_NATIVE_VIEW,
            detached: dyl_render::capabilities::ALLOWS_DETACHED_WINDOW,
            surface_kinds: dyl_render::capabilities::supported_surface_kinds(),
        },
        features: dyl_core::contract::FeatureCapabilities {
            video_codecs: dyl_ffmpeg::capabilities::video_codecs(),
            audio_codecs: dyl_ffmpeg::capabilities::audio_codecs(),
            subtitle_formats: dyl_subtitle::capabilities::file_formats(),
            embedded_ass_styling: dyl_subtitle::capabilities::EMBEDDED_ASS_STYLING,
            track_switching: TRACK_SWITCHING,
            hw_decode: dyl_ffmpeg::capabilities::HW_DECODE_VERIFIED,
            danmaku_formats: dyl_danmaku::capabilities::formats(),
            danmaku_xml_entities: dyl_danmaku::capabilities::XML_ENTITIES_UNESCAPED,
            seek_modes: dyl_pipeline::capabilities::seek_modes(),
            overlay_in_core: dyl_render::capabilities::KERNEL_RENDERS_OVERLAYS,
        },
        limits: Limits {
            max_texture_dimension_2d: dyl_render::capabilities::GUARANTEED_MAX_TEXTURE_DIMENSION_2D,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyl_core::contract::SurfaceKind;

    /// 能力位必须与实现同源：装配结果的每一项都要能追溯到声明方，且值如实。
    #[test]
    fn assembled_values_come_from_the_implementing_crates() {
        let caps = capabilities();
        assert_eq!(caps.schema, SCHEMA_VERSION);
        assert_eq!(caps.abi, AbiVersion::V0_6_0);
        assert_eq!(caps.platform, std::env::consts::OS);

        assert_eq!(caps.gpu_backends, dyl_render::capabilities::gpu_backends());
        assert_eq!(caps.protocols, dyl_ffmpeg::capabilities::source_protocols());
        assert_eq!(
            caps.presentation.surface_kinds,
            dyl_render::capabilities::supported_surface_kinds()
        );
        assert_eq!(
            caps.features.video_codecs,
            dyl_ffmpeg::capabilities::video_codecs()
        );
        assert_eq!(
            caps.features.audio_codecs,
            dyl_ffmpeg::capabilities::audio_codecs()
        );
        assert_eq!(
            caps.features.hw_decode,
            dyl_ffmpeg::capabilities::HW_DECODE_VERIFIED
        );
        assert_eq!(
            caps.features.subtitle_formats,
            dyl_subtitle::capabilities::file_formats()
        );
        assert_eq!(
            caps.features.danmaku_formats,
            dyl_danmaku::capabilities::formats()
        );
        assert_eq!(
            caps.features.seek_modes,
            dyl_pipeline::capabilities::seek_modes()
        );
        assert_eq!(
            caps.limits.max_texture_dimension_2d,
            dyl_render::capabilities::GUARANTEED_MAX_TEXTURE_DIMENSION_2D
        );
    }

    #[test]
    fn protocols_match_what_this_build_can_open() {
        let caps = capabilities();
        assert!(caps.protocols.iter().any(|p| p == "http"));
        assert!(
            !caps.protocols.iter().any(|p| p == "file"),
            "本地路径不是 URL 协议"
        );
        // https 只能在本构建的 FFmpeg 带 TLS 时声明（Android 静态构建没有）
        assert_eq!(
            caps.protocols.iter().any(|p| p == "https"),
            dyl_ffmpeg::capabilities::protocol_available("https"),
            "{:?}",
            caps.protocols
        );
    }

    #[test]
    fn presentation_rules_are_still_honest() {
        let caps = capabilities();
        assert!(caps.presentation.external_surface);
        assert!(
            !caps.presentation.texture_sharing,
            "Flutter Texture 共享是 2.2 的产品决策，能力位必须恒为 false"
        );
        assert!(caps.presentation.attach_before_load);
        assert!(caps.presentation.embedded);
        assert!(caps.features.overlay_in_core);
        assert!(
            caps.features.track_switching,
            "select_track 已能换音轨与字幕轨；视频轨不在声明范围内（见 2.3）"
        );
    }

    #[test]
    fn code_and_subtitle_vocabularies_do_not_collapse() {
        let caps = capabilities();
        // 两套词汇是事实（规范 5.2）：文件格式 srt/ass/ssa 与容器内轨编码 subrip/…
        assert!(caps.features.subtitle_formats.contains(&"srt".to_string()));
        assert!(
            !caps
                .features
                .subtitle_formats
                .contains(&"subrip".to_string()),
            "`subtitle_formats` 描述文件格式，不能混入容器轨的编码名"
        );
        assert!(caps
            .features
            .video_codecs
            .iter()
            .all(|name| name != "unknown" && !name.is_empty()));
    }

    #[test]
    fn supported_surface_kinds_matches_the_platform_expectation() {
        let caps = capabilities();
        let expected = match std::env::consts::OS {
            "windows" => Some(SurfaceKind::WindowsHwnd),
            "macos" => Some(SurfaceKind::MacosNsview),
            "linux" => Some(SurfaceKind::X11Window),
            "android" => Some(SurfaceKind::AndroidNativeWindow),
            _ => None,
        };
        match expected {
            Some(kind) => assert_eq!(caps.presentation.surface_kinds, vec![kind]),
            None => assert!(caps.presentation.surface_kinds.is_empty()),
        }
    }

    #[test]
    fn capabilities_round_trip_through_json() {
        let caps = capabilities();
        let json = serde_json::to_string(&caps).unwrap();
        let parsed: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, caps);
    }
}
