//! 能力位的装配回归：`dyl_engine::capabilities()` 必须与本次构建所在平台的
//! contract 夹具逐字段一致，且每个值都能追溯到实现它的 crate（规范 ADR-13 / 9.4）。

use std::path::PathBuf;

use dyl_core::contract::Capabilities;

fn fixture_path(platform: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contract/capabilities")
        .join(format!("{platform}.json"))
}

fn platform_fixture_name() -> Option<&'static str> {
    match std::env::consts::OS {
        "windows" => Some("windows"),
        "macos" => Some("macos"),
        "linux" => Some("linux"),
        "android" => Some("android"),
        _ => None,
    }
}

/// 本平台夹具是能力 JSON 的 golden 值：实现漂移（例如给 HEVC 加了验收却没声明、
/// 或把 `max_texture_dimension_2d` 又锁回 2048 天花板）会让这里红。
#[test]
fn live_capabilities_match_the_platform_fixture() {
    let Some(platform) = platform_fixture_name() else {
        return; // 未覆盖的平台：能力位为空，没有夹具可比
    };
    let raw = std::fs::read_to_string(fixture_path(platform))
        .unwrap_or_else(|e| panic!("read capabilities fixture for {platform}: {e}"));
    let fixture: Capabilities = serde_json::from_str(&raw).expect("fixture must parse");

    let live = dyl_engine::capabilities::capabilities();
    assert_eq!(
        serde_json::to_value(&live).unwrap(),
        serde_json::to_value(&fixture).unwrap(),
        "能力位与夹具不一致：改了实现就要同步 `contract/capabilities/{platform}.json`"
    );
}

/// 能力位里的 H.264/HEVC、AAC/FLAC 必须和快照里的 codec 名同一套词表：
/// 这些值来自 `dyl-ffmpeg::capabilities`，而它对每种编码调用的就是快照用的 `label()`。
#[test]
fn codec_capabilities_use_the_snapshot_vocabulary() {
    let caps = dyl_engine::capabilities::capabilities();
    for expected in ["h264", "h265"] {
        assert!(
            caps.features.video_codecs.iter().any(|c| c == expected),
            "video_codecs 少了 {expected}：HEVC 已实测可用，能力位必须如实声明"
        );
    }
    for expected in ["aac", "flac"] {
        assert!(
            caps.features.audio_codecs.iter().any(|c| c == expected),
            "audio_codecs 少了 {expected}"
        );
    }
    // AV1 只在构建里确有软解解码器（libdav1d / libaom-av1）时声明：
    // 内置 `av1` 解码器只能硬解，能力位不能凭它承诺软解。
    assert_eq!(
        caps.features.video_codecs.iter().any(|c| c == "av1"),
        dyl_ffmpeg::capabilities::software_av1_decoder().is_some(),
        "av1 的声明必须与构建里的 AV1 软解解码器一致"
    );
    assert!(caps
        .features
        .seek_modes
        .contains(&dyl_core::contract::SeekMode::Keyframe));
    assert!(
        caps.limits.max_texture_dimension_2d >= 2048,
        "保证下界不得低于 2048（ADR-14）"
    );
}
