//! 由 `dyl-ffmpeg` 声明的解码能力值（规范 ADR-13）。
//!
//! 值是 `CodecId` 枚举本身，不是手抄的字符串：能力位里的每个名字都取自 `label()`，
//! 与快照里的 `media.video.codec` / `media.tracks[].codec` 共用同一个词表，
//! 因此不可能出现「解码器认得 h265，能力位只写 h264」这类漂移。
//!
//! **这是「已实测」清单，不是解码白名单。** `FfmpegDemuxer::open` 对 FFmpeg 认识的
//! 任何流都会建上下文并尝试解码；清单只表示本项目已用真实文件验收过。

use crate::types::{AudioCodecId, SubtitleCodecId, VideoCodecId};

/// `features.hw_decode` 的值：本构建的硬件解码是否已被产品路径实测。
///
/// 目前只有 Windows：`d3d11va`/`d3d12va`/`dxva2` 都提供 `HW_DEVICE_CTX`，装上方案
/// 就等于帧真的来自 GPU，`FfmpegVideoDecoder::hardware_device()` 报告的就是实际
/// 后端。`internal_only` 平台（Linux vaapi 等）即便装上方案，FFmpeg 仍可能在第一次
/// 解帧时自行落回软解，而解码器侧看不到那一步——在实测之前不能声称“硬解中”。
/// 为 `false` 时 `set_decode_mode {mode: auto}` 同步 `not_supported`，客户端不应
/// 显示硬解开关。这是**已实测**标志，不是解码白名单。
pub const HW_DECODE_VERIFIED: bool = cfg!(target_os = "windows");

/// 内核接受为 `load_file` **URL** 的协议（ADR-13）。本地路径不是 URL，
/// 也不经过 scheme 分流，所以 `file` 不在表里：`file://` 这类 URL 会以
/// “只接受 http(s)” 被拒绝（见 [`crate::source`]），本地文件请直接传路径。
pub const ACCEPTED_SOURCE_PROTOCOLS: &[&str] = &["http", "https"];

/// 本构建链接的 FFmpeg 实际编译进来的输入协议。
///
/// 用 `avio_enum_protocols` 现场枚举，而不是照抄构建脚本：Windows 的 vcpkg
/// 构建带 TLS，Android 的静态构建 `--disable-autodetect` 后没有 TLS，
/// 同名协议在不同平台上的可用性只有运行时才知道。
pub fn compiled_input_protocols() -> Vec<String> {
    let mut protocols = Vec::new();
    let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
    loop {
        // Safety: opaque 由 FFmpeg 在两次调用之间维护；返回指向静态字符串的指针
        let name = unsafe { ffmpeg_next::ffi::avio_enum_protocols(&mut opaque, 0) };
        if name.is_null() {
            break;
        }
        protocols.push(
            unsafe { std::ffi::CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned(),
        );
    }
    protocols.sort();
    protocols.dedup();
    protocols
}

/// 本构建是否真的带某个协议。
pub fn protocol_available(name: &str) -> bool {
    compiled_input_protocols().iter().any(|p| p == name)
}

/// `capabilities.protocols` 的值：内核接受的 URL 协议 ∩ 本构建实际编译。
///
/// 顺序跟随 [`ACCEPTED_SOURCE_PROTOCOLS`]。与 codec 清单不同，这个列表**确实**
/// 是内核的放行依据（[`crate::source::classify`] 按它拒绝 scheme），所以它只应
/// 陈述本构建真的能打开的 URL 类型：Android 没有 TLS 时 `https` 就不在其中。
pub fn source_protocols() -> Vec<String> {
    let compiled = compiled_input_protocols();
    ACCEPTED_SOURCE_PROTOCOLS
        .iter()
        .filter(|name| compiled.iter().any(|c| c == *name))
        .map(|name| (*name).to_string())
        .collect()
}

/// 已在真实文件上播放验收过的视频编解码器。
///
/// H.264 与 HEVC（含 10-bit：由 swscale 统一转 YUV420P 再上传）各有一条回归。
/// AV1 不写死在这里：FFmpeg 内置的 `av1` 解码器**只有硬件路径**（没有 hwaccel
/// 时 `get_pixel_format` 直接 `ENOSYS`），软解取决于构建里是否编入了
/// `libdav1d` / `libaom-av1`。因此 AV1 由 [`video_codecs`] 按本构建实况追加。
pub const VERIFIED_VIDEO_CODECS: &[VideoCodecId] = &[VideoCodecId::H264, VideoCodecId::H265];

/// AV1 软解解码器的查找顺序。
///
/// FFmpeg 的 `libdav1d` 是首选，`libaom-av1` 是兜底；内置 `av1` 不能软解，
/// 只能靠 hwaccel 工作（软解分支见 `decoder.rs::open_software_av1`）。
pub const SOFTWARE_AV1_DECODERS: &[&str] = &["libdav1d", "libaom-av1"];

/// 本构建可用的 AV1 软解解码器名。
pub fn software_av1_decoder() -> Option<&'static str> {
    SOFTWARE_AV1_DECODERS
        .iter()
        .copied()
        .find(|name| ffmpeg_next::codec::decoder::find_by_name(name).is_some())
}

/// 已在真实文件上验收过的音频编解码器。FLAC 同时是 `select_track` 换轨回归的目标
/// （44.1k 单声道轨，验证音频设备按新轨重新初始化）。
pub const VERIFIED_AUDIO_CODECS: &[AudioCodecId] = &[AudioCodecId::Aac, AudioCodecId::Flac];

/// 能被选为文本字幕轨的容器内编码。位图字幕（PGS/DVB）不在其中，
/// 由 `SubtitleCodecId::is_text` 与 `resolve_track` 拒绝。
pub const TEXT_SUBTITLE_CODECS: &[SubtitleCodecId] = &[
    SubtitleCodecId::SubRip,
    SubtitleCodecId::MovText,
    SubtitleCodecId::Ass,
    SubtitleCodecId::Ssa,
];

fn labels<T: Copy>(ids: &[T], label: impl Fn(T) -> &'static str) -> Vec<String> {
    ids.iter().map(|id| label(*id).to_string()).collect()
}

/// `features.video_codecs` 的值。
///
/// AV1 只在构建里确有软解解码器时声明：能力位表达的是“客户端可以预期这项
/// 能播”，而内置 `av1` 解码器在无 AV1 硬解的设备上必然失败。
pub fn video_codecs() -> Vec<String> {
    let mut codecs = labels(VERIFIED_VIDEO_CODECS, VideoCodecId::label);
    if software_av1_decoder().is_some() {
        codecs.push(VideoCodecId::Av1.label().to_string());
    }
    codecs
}

/// `features.audio_codecs` 的值。
pub fn audio_codecs() -> Vec<String> {
    labels(VERIFIED_AUDIO_CODECS, AudioCodecId::label)
}

/// `media.tracks[].codec` 对文本字幕轨输出的词表。
pub fn text_subtitle_codec_labels() -> Vec<String> {
    labels(TEXT_SUBTITLE_CODECS, SubtitleCodecId::label)
}

/// 与 `TEXT_SUBTITLE_CODECS` 一致的那组编码是否都被 `is_text()` 认下 ——
/// 换轨与自动选轨都靠这个谓词，清单和谓词必须不漂移。
pub fn text_subtitle_codecs_are_selectable() -> bool {
    TEXT_SUBTITLE_CODECS.iter().all(|id| id.is_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_codecs_carry_real_labels() {
        for list in [video_codecs(), audio_codecs(), text_subtitle_codec_labels()] {
            assert!(!list.is_empty());
            for label in list {
                assert!(!label.is_empty(), "能力位不能出现空串");
                assert_ne!(label, "unknown", "`unknown` 是识别失败的兜底，不是能力");
            }
        }
    }

    /// 能力位的字符串与快照里的 codec 名字必然同源：同一个 `label()`。
    #[test]
    fn labels_round_trip_through_ffmpeg_ids() {
        for id in VERIFIED_VIDEO_CODECS {
            let from_ffmpeg = VideoCodecId::from(ffmpeg_next::codec::Id::from(*id));
            assert_eq!(&from_ffmpeg, id, "{} 的 FFmpeg 映射断了", id.label());
        }
        for id in VERIFIED_AUDIO_CODECS {
            let from_ffmpeg = AudioCodecId::from(ffmpeg_next::codec::Id::from(*id));
            assert_eq!(&from_ffmpeg, id, "{} 的 FFmpeg 映射断了", id.label());
        }
    }

    /// `Id::H265` 只是反向别名，`From<AVCodecID>` 永不产出它；这条断言把那次
    /// 「HEVC 全被标成 unknown」的缺陷钉住。
    #[test]
    fn hevc_is_recognised_from_the_real_ffmpeg_id() {
        assert_eq!(
            VideoCodecId::from(ffmpeg_next::codec::Id::HEVC),
            VideoCodecId::H265
        );
        assert!(video_codecs().iter().any(|l| l == "h265"));
    }

    /// AV1 的声明必须跟构建里的软解解码器一致：没有 libdav1d/libaom 时
    /// 声明 av1 等于骗客户端（内置 `av1` 只能硬解）。
    #[test]
    fn av1_is_declared_iff_a_software_decoder_exists() {
        let declared = video_codecs().iter().any(|c| c == "av1");
        assert_eq!(declared, software_av1_decoder().is_some());
        assert_eq!(VideoCodecId::Av1.label(), "av1");
    }

    #[test]
    fn text_subtitle_list_matches_the_selectability_predicate() {
        assert!(
            text_subtitle_codecs_are_selectable(),
            "`TEXT_SUBTITLE_CODECS` 里出现了 `is_text()` 不认的编码"
        );
    }

    /// http 是内核的最小承诺：所有受支持平台的 FFmpeg 都带它
    #[test]
    fn http_is_always_available() {
        let protocols = source_protocols();
        assert!(protocols.iter().any(|p| p == "http"), "{protocols:?}");
        assert!(
            !protocols.iter().any(|p| p == "file"),
            "本地路径不是 URL 协议，不能出现在这里：{protocols:?}"
        );
    }

    /// 能力值必须来自编译器事实：声明的每个协议都能在实际清单里找到，
    /// 且不会出现接受范围之外的名字
    #[test]
    fn declared_protocols_are_compiled_and_accepted() {
        let compiled = compiled_input_protocols();
        let declared = source_protocols();
        assert!(!declared.is_empty());
        for name in &declared {
            assert!(compiled.contains(name), "{name} 未编译进 FFmpeg");
            assert!(ACCEPTED_SOURCE_PROTOCOLS.contains(&name.as_str()));
        }
        // https 只在本构建带 TLS 时声明
        assert_eq!(
            declared.iter().any(|p| p == "https"),
            compiled.iter().any(|p| p == "https")
        );
    }
}
