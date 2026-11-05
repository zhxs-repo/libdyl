//! 契约 golden 测试：`contract/` 夹具必须能被类型化解析并无损往返。

use std::fs;
use std::path::{Path, PathBuf};

use dyl_core::contract::{
    AbiVersion, AudioStreamInfo, Capabilities, Command, CommandEnvelope, CommandOp, CommandStatus,
    DecodeBackend, DecodeMode, ErrorCatalog, ErrorCode, Event, EventEnvelope, MatrixStatus,
    MediaInfo, PlayerState, SameTarget, SeekMode, Snapshot, StateMatrix, SurfaceDescriptor,
    SurfaceKind, SurfaceStatus, TrackInfo, TrackKind, TransitionTarget, VideoStreamInfo,
    SCHEMA_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

fn contract_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract")
}

fn read_json(rel: &str) -> Value {
    let path = contract_dir().join(rel);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse fixture {}: {e}", path.display()))
}

fn round_trip<T: DeserializeOwned + Serialize>(rel: &str) -> T {
    let path = contract_dir().join(rel);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    let typed: T = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("deserialize fixture {}: {e}", path.display()));
    let actual = serde_json::to_value(&typed).unwrap();
    let expected: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(actual, expected, "fixture {rel} 往返后结构不一致");
    typed
}

fn json_files(dir: &str) -> Vec<String> {
    let mut files: Vec<String> = fs::read_dir(contract_dir().join(dir))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect();
    files.sort();
    files
}

fn error_info(code: ErrorCode) -> dyl_core::contract::ErrorInfo {
    dyl_core::contract::ErrorInfo::new(code)
}

fn same() -> Option<TransitionTarget> {
    Some(TransitionTarget::Same(SameTarget::Same))
}

fn one(state: PlayerState) -> Option<TransitionTarget> {
    Some(TransitionTarget::One(state))
}

fn many(states: &[PlayerState]) -> Option<TransitionTarget> {
    Some(TransitionTarget::Many(states.to_vec()))
}

fn assert_cell(
    matrix: &StateMatrix,
    op: CommandOp,
    state: PlayerState,
    expected: Option<TransitionTarget>,
) {
    let cell = matrix
        .cell(op, state)
        .unwrap_or_else(|| panic!("state_matrix 缺少 {op:?} × {state:?}"));
    match expected {
        Some(target) => {
            assert_eq!(cell.status, MatrixStatus::Accepted, "{op:?} × {state:?}");
            assert_eq!(cell.to.as_ref(), Some(&target), "{op:?} × {state:?}");
        }
        None => {
            assert_eq!(cell.status, MatrixStatus::Rejected, "{op:?} × {state:?}");
            assert!(cell.to.is_none(), "{op:?} × {state:?} 不应有落点");
        }
    }
}

#[test]
fn commands_round_trip_and_cover_every_op() {
    let mut seen = Vec::new();
    for file in json_files("commands") {
        let envelope: CommandEnvelope = round_trip(&format!("commands/{file}"));
        assert_eq!(envelope.schema, SCHEMA_VERSION, "{file}");
        assert!(envelope.request_id.is_some_and(|id| id > 0), "{file}");
        seen.push(envelope.command.op());
    }

    seen.sort();
    seen.dedup();
    let mut expected = CommandOp::ALL.to_vec();
    expected.sort();
    assert_eq!(seen, expected, "commands/ 未覆盖全部 op");
}

#[test]
fn command_json_is_internally_tagged() {
    let envelope = CommandEnvelope::with_request_id(
        101,
        Command::LoadFile {
            url: r"D:\video.mp4".into(),
        },
    );
    let value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(value["op"], "load_file");
    assert_eq!(value["url"], r"D:\video.mp4");
    assert!(value.get("LoadFile").is_none());

    let seek = CommandEnvelope::with_request_id(
        104,
        Command::Seek {
            position_ms: 90_000,
            mode: SeekMode::Exact,
        },
    );
    assert_eq!(seek.command.op(), CommandOp::Seek);
    assert_eq!(serde_json::to_value(seek).unwrap()["mode"], "exact");
}

#[test]
fn event_sequences_round_trip_and_are_monotonic() {
    let files = json_files("events");
    assert_eq!(files.len(), 7, "事件序列夹具数量变化，需同步更新契约");

    for file in files {
        let events: Vec<EventEnvelope> = round_trip(&format!("events/{file}"));
        assert!(!events.is_empty(), "{file}");
        assert_eq!(events[0].seq, 1, "{file} 的 seq 必须从 1 开始");
        assert!(
            events.windows(2).all(|w| w[0].seq < w[1].seq),
            "{file} 的 seq 必须严格递增"
        );
        assert!(events.iter().all(|e| e.schema == SCHEMA_VERSION), "{file}");
    }
}

#[test]
fn load_sequence_orders_semantic_events_before_completion() {
    let events: Vec<EventEnvelope> = round_trip("events/load_success_sequence.json");

    let Event::StateChanged(loading) = &events[0].event else {
        panic!("首个事件应为 state_changed");
    };
    assert_eq!(loading.from, PlayerState::Idle);
    assert_eq!(loading.to, PlayerState::Loading);

    let Event::FileLoaded(loaded) = &events[1].event else {
        panic!("第二个事件应为 file_loaded");
    };
    assert_eq!(loaded.request_id, 101);
    assert_eq!(loaded.duration_ms, 120_000);
    let video = loaded.media.video.as_ref().expect("video");
    assert_eq!((video.width, video.height), (1920, 1080));

    let Event::StateChanged(ready) = &events[2].event else {
        panic!("第三个事件应为 state_changed");
    };
    assert_eq!(ready.to, PlayerState::Ready);

    let Event::CommandCompleted(done) = &events[3].event else {
        panic!("最后的事件应为 command_completed");
    };
    assert_eq!(done.request_id, loaded.request_id);
    assert_eq!(done.status, CommandStatus::Ok);
    assert!(done.error.is_none());
}

#[test]
fn seek_sequence_reports_latest_and_supersedes_old() {
    let events: Vec<EventEnvelope> = round_trip("events/seek_superseded_sequence.json");

    let Event::SeekCompleted(seek) = &events[0].event else {
        panic!("首个事件应为 seek_completed");
    };
    assert_eq!(seek.request_id, 115);
    assert_eq!((seek.from_ms, seek.to_ms), (12_340, 90_000));
    assert!(seek.actual_ms >= seek.to_ms);
    assert_eq!(seek.mode, SeekMode::Exact);

    let Event::CommandCompleted(ok) = &events[1].event else {
        panic!();
    };
    assert_eq!((ok.request_id, ok.status), (115, CommandStatus::Ok));

    let Event::CommandCompleted(old) = &events[2].event else {
        panic!();
    };
    assert_eq!(
        (old.request_id, old.status),
        (104, CommandStatus::Superseded)
    );
}

#[test]
fn eof_and_error_sequences_match_state_contract() {
    let eof: Vec<EventEnvelope> = round_trip("events/eof_sequence.json");
    let Event::StateChanged(ended) = &eof[0].event else {
        panic!();
    };
    assert_eq!(
        (ended.from, ended.to),
        (PlayerState::Playing, PlayerState::Ended)
    );
    let Event::PositionChanged(pos) = &eof[1].event else {
        panic!();
    };
    assert_eq!(pos.position_ms, 120_000);

    let failure: Vec<EventEnvelope> = round_trip("events/pipeline_error_sequence.json");
    let Event::StateChanged(error_state) = &failure[0].event else {
        panic!();
    };
    assert_eq!(error_state.to, PlayerState::Error);
    let Event::Error(err) = &failure[1].event else {
        panic!();
    };
    assert_eq!(
        (err.code, err.code_num, err.fatal),
        (ErrorCode::Decode, -5, true)
    );
}

#[test]
fn settings_and_surface_sequences_are_complete_objects() {
    let settings: Vec<EventEnvelope> = round_trip("events/settings_sequence.json");
    let Event::SettingsChanged(changed) = &settings[0].event else {
        panic!();
    };
    assert_eq!(changed.volume, 80);
    assert!(changed.muted);
    assert_eq!(changed.decode_mode, DecodeMode::Software);
    assert_eq!(changed.danmaku_opacity, 80);
    let Event::CommandCompleted(done) = &settings[1].event else {
        panic!();
    };
    assert_eq!((done.op, done.request_id), (CommandOp::SetMute, 107));

    let surface: Vec<EventEnvelope> = round_trip("events/surface_sequence.json");
    let Event::SurfaceAttached(attached) = &surface[0].event else {
        panic!();
    };
    assert_eq!(attached.kind, SurfaceKind::WindowsHwnd);
    assert_eq!((attached.width, attached.height), (1280, 720));
    let Event::SurfaceResized(resized) = &surface[1].event else {
        panic!();
    };
    assert_eq!((resized.width, resized.height), (1600, 900));
    let Event::SurfaceDetached(detached) = &surface[2].event else {
        panic!();
    };
    assert_eq!(detached.kind, SurfaceKind::WindowsHwnd);
}

#[test]
fn snapshots_round_trip_and_match_semantics() {
    let idle: Snapshot = round_trip("snapshot/idle.json");
    assert_eq!(idle.state, PlayerState::Idle);
    assert!(idle.source.is_none());
    assert!(idle.media.is_none());
    assert_eq!(idle.surface.status, SurfaceStatus::None);
    assert!(idle.surface.kind.is_none());
    assert_eq!((idle.surface.width, idle.surface.height), (0, 0));
    assert!(!idle.subtitle.loaded);
    assert_eq!(idle.danmaku.count, 0);
    assert_eq!(idle.decode.mode, DecodeMode::Software);
    assert_eq!(idle.decode.backend, DecodeBackend::Software);
    assert!(idle.decode.device.is_none());

    let playing: Snapshot = round_trip("snapshot/playing.json");
    assert_eq!(playing.state, PlayerState::Playing);
    assert!(playing.position_ms < playing.duration_ms);
    assert_eq!(playing.surface.status, SurfaceStatus::Attached);
    assert!(playing.last_error.is_none());
    // 硬解生效时 backend 必须是实际后端，device 指出真实设备
    assert_eq!(playing.decode.mode, DecodeMode::Auto);
    assert_eq!(playing.decode.backend, DecodeBackend::Hardware);
    assert_eq!(playing.decode.device.as_deref(), Some("d3d11va"));

    let ended: Snapshot = round_trip("snapshot/ended.json");
    assert_eq!(ended.state, PlayerState::Ended);
    assert_eq!(ended.position_ms, ended.duration_ms);

    let error: Snapshot = round_trip("snapshot/error.json");
    assert_eq!(error.state, PlayerState::Error);
    // 没有存活管线时 backend 只能是 software：此时没有解码在跑
    assert_eq!(error.decode.backend, DecodeBackend::Software);
    assert!(error.decode.device.is_none());
    let last = error
        .last_error
        .as_ref()
        .expect("error 快照必须保留 last_error");
    assert_eq!((last.code, last.code_num), (ErrorCode::Decode, -5));
}

#[test]
fn surface_descriptors_round_trip() {
    let cases = [
        ("surface/windows_hwnd.json", SurfaceKind::WindowsHwnd),
        ("surface/macos_nsview.json", SurfaceKind::MacosNsview),
        ("surface/x11_window.json", SurfaceKind::X11Window),
        ("surface/wayland_surface.json", SurfaceKind::WaylandSurface),
        (
            "surface/android_native_window.json",
            SurfaceKind::AndroidNativeWindow,
        ),
    ];

    for (file, expected_kind) in cases {
        let descriptor: SurfaceDescriptor = round_trip(file);
        assert_eq!(descriptor.kind(), expected_kind, "{file}");
        let (width, height) = descriptor.size();
        assert!(width > 0 && height > 0, "{file}");
    }
}

#[test]
fn capabilities_fixtures_round_trip_and_respect_presentation_rules() {
    let cases = ["android", "linux", "macos", "windows"];
    for platform in cases {
        let caps: Capabilities = round_trip(&format!("capabilities/{platform}.json"));
        assert_eq!(caps.schema, SCHEMA_VERSION);
        assert_eq!(caps.abi, AbiVersion::V0_6_0);
        assert_eq!(caps.platform, platform);
        assert!(caps.presentation.external_surface);
        assert!(
            !caps.presentation.texture_sharing,
            "契约不支持 Flutter Texture 共享"
        );
        assert!(caps.presentation.attach_before_load);
        assert!(caps.presentation.embedded);
        assert_eq!(
            caps.presentation.detached,
            platform != "android",
            "Android 只声明 embedded=true"
        );
        assert_eq!(caps.presentation.surface_kinds.len(), 1);
        assert_eq!(
            caps.limits.max_texture_dimension_2d, 2048,
            "`max_texture_dimension_2d` 是本构建保证的下界（ADR-14），不是产品天花板"
        );
        assert!(caps.features.overlay_in_core);
        assert_eq!(
            caps.features.seek_modes,
            vec![SeekMode::Exact, SeekMode::Keyframe]
        );
        assert!(
            caps.features.embedded_ass_styling,
            "内嵌 ASS 走自建样式路径，能力位必须如实报 true"
        );
        assert!(
            caps.features.track_switching,
            "select_track 已实现（音轨与字幕轨），能力位必须如实报 true"
        );
        // 硬解只在实测过的平台声明；夹具不是实现，dyl-engine 测试与实现比对
        assert_eq!(
            caps.features.hw_decode,
            platform == "windows",
            "{platform} 的 hw_decode 与已实测平台不符"
        );
        assert!(!caps.gpu_backends.is_empty());
    }
}

/// `dyl-core` 只定义能力的**形状**；值由实现它的 crate 声明、`dyl-engine` 装配（ADR-13），
/// 相应断言在 `dyl-engine/tests/capabilities.rs`。这里只钉住两件事：ABI 描述形状，
/// 以及每台平台的夹具都能被同一套类型无损解析。
#[test]
fn capabilities_shape_is_stable_across_platforms() {
    for platform in ["android", "linux", "macos", "windows"] {
        let caps: Capabilities = round_trip(&format!("capabilities/{platform}.json"));
        assert_eq!(caps.schema, SCHEMA_VERSION);
        assert_eq!(caps.abi, AbiVersion::V0_6_0);
        assert_eq!(caps.abi.packed(), 6 << 8);
        assert_eq!(caps.platform, platform);

        // 值不在本 crate 声明，但形状必须齐全：缺字段会让 Dart / C 客户端解析失败
        assert!(!caps.gpu_backends.is_empty());
        assert!(!caps.protocols.is_empty());
        assert!(!caps.features.video_codecs.is_empty());
        assert!(!caps.features.audio_codecs.is_empty());
        assert!(!caps.features.subtitle_formats.is_empty());
        assert!(!caps.features.danmaku_formats.is_empty());
        assert!(!caps.features.seek_modes.is_empty());
        assert!(!caps.presentation.surface_kinds.is_empty());
    }
}

#[test]
fn error_catalog_matches_code_table() {
    let catalog: ErrorCatalog = round_trip("errors.json");
    assert_eq!(catalog.schema, SCHEMA_VERSION);
    assert_eq!(catalog.errors.len(), ErrorCode::ALL.len());

    for entry in &catalog.errors {
        assert_eq!(entry.code.as_i32(), entry.code_num);
        assert_eq!(entry.constant, entry.code.constant_name());
        assert_eq!(
            serde_json::to_value(entry.code).unwrap(),
            Value::String(entry.code.as_str().into()),
            "serde 名称与 as_str 不一致"
        );
    }

    for code in ErrorCode::ALL {
        assert_eq!(
            catalog.errors.iter().filter(|e| e.code == code).count(),
            1,
            "{code:?} 在 errors.json 中应恰好出现一次"
        );
    }

    assert_eq!(dyl_core::contract::error_string(-5), "decode");
    assert_eq!(dyl_core::contract::error_string(12345), "unknown");
    assert_eq!(ErrorCode::from_i32(-2), Some(ErrorCode::InvalidState));
}

#[test]
fn state_matrix_is_complete_and_matches_contract_semantics() {
    let matrix: StateMatrix = round_trip("state_matrix.json");
    assert_eq!(matrix.schema, SCHEMA_VERSION);
    assert_eq!(matrix.states, PlayerState::ALL);
    assert_eq!(matrix.matrix.len(), CommandOp::ALL.len());
    assert!(matrix.missing_cells().is_empty(), "状态矩阵存在缺失格子");
    assert_eq!(matrix.reject.channel, "command_completed");
    assert_eq!(matrix.reject.status, CommandStatus::Error);
    assert_eq!(matrix.reject.error, error_info(ErrorCode::InvalidState));

    let loading_ready_error = [PlayerState::Loading, PlayerState::Ready, PlayerState::Error];
    for state in PlayerState::ALL {
        assert_cell(
            &matrix,
            CommandOp::LoadFile,
            state,
            many(&loading_ready_error),
        );
    }
    assert_eq!(
        matrix
            .cell(CommandOp::LoadFile, PlayerState::Loading)
            .unwrap()
            .cancels_inflight_load,
        Some(true),
        "loading 中再次 load_file 必须取消在途加载"
    );

    for (state, expected) in [
        (PlayerState::Idle, None),
        (PlayerState::Loading, None),
        (PlayerState::Ready, one(PlayerState::Playing)),
        (PlayerState::Playing, same()),
        (PlayerState::Paused, one(PlayerState::Playing)),
        (PlayerState::Ended, one(PlayerState::Playing)),
        (PlayerState::Stopped, one(PlayerState::Playing)),
        (PlayerState::Error, None),
    ] {
        assert_cell(&matrix, CommandOp::Play, state, expected);
    }

    for state in PlayerState::ALL {
        let expected = match state {
            PlayerState::Playing => one(PlayerState::Paused),
            PlayerState::Paused => same(),
            _ => None,
        };
        assert_cell(&matrix, CommandOp::Pause, state, expected);
    }

    for state in PlayerState::ALL {
        let expected = match state {
            PlayerState::Idle | PlayerState::Stopped => same(),
            _ => one(PlayerState::Stopped),
        };
        assert_cell(&matrix, CommandOp::Stop, state, expected);
    }

    for op in [CommandOp::Seek, CommandOp::SeekRelative] {
        for state in PlayerState::ALL {
            let expected = match state {
                PlayerState::Ready | PlayerState::Playing | PlayerState::Paused => same(),
                PlayerState::Ended | PlayerState::Stopped => one(PlayerState::Paused),
                PlayerState::Idle | PlayerState::Loading | PlayerState::Error => None,
            };
            assert_cell(&matrix, op, state, expected);
        }
    }

    for op in [
        CommandOp::SetVolume,
        CommandOp::SetMute,
        CommandOp::SetSubtitleVisible,
        CommandOp::SetDanmakuVisible,
        CommandOp::SetDanmakuOpacity,
    ] {
        for state in PlayerState::ALL {
            assert_cell(&matrix, op, state, same());
        }
    }

    for op in [CommandOp::LoadSubtitle, CommandOp::LoadDanmaku] {
        for state in PlayerState::ALL {
            let expected = match state {
                PlayerState::Ready
                | PlayerState::Playing
                | PlayerState::Paused
                | PlayerState::Ended
                | PlayerState::Stopped => same(),
                PlayerState::Idle | PlayerState::Loading | PlayerState::Error => None,
            };
            assert_cell(&matrix, op, state, expected);
        }
    }

    // 换轨是按新轨道组合重开管线，落点必然经过 loading；ended/stopped 只能回到 paused
    for state in PlayerState::ALL {
        let expected = match state {
            PlayerState::Ready => many(&[PlayerState::Loading, PlayerState::Ready]),
            PlayerState::Playing => many(&[PlayerState::Loading, PlayerState::Playing]),
            PlayerState::Paused | PlayerState::Ended | PlayerState::Stopped => {
                many(&[PlayerState::Loading, PlayerState::Paused])
            }
            PlayerState::Idle | PlayerState::Loading | PlayerState::Error => None,
        };
        assert_cell(&matrix, CommandOp::SelectTrack, state, expected);
    }

    // 切解码方式在活管线状态重开并落回原状态；ended/stopped/idle/error 只改设置
    for state in PlayerState::ALL {
        let expected = match state {
            PlayerState::Ready => many(&[PlayerState::Loading, PlayerState::Ready]),
            PlayerState::Playing => many(&[PlayerState::Loading, PlayerState::Playing]),
            PlayerState::Paused => many(&[PlayerState::Loading, PlayerState::Paused]),
            PlayerState::Idle | PlayerState::Ended | PlayerState::Stopped | PlayerState::Error => {
                same()
            }
            PlayerState::Loading => None,
        };
        assert_cell(&matrix, CommandOp::SetDecodeMode, state, expected);
    }
}

/// 换轨的成功序列必须与 `load_file` 同构：语义事件先于 `command_completed`，
/// 且 `file_loaded` 携带的 `media.tracks[].selected` 就是切换后的真相
#[test]
fn track_switch_sequence_carries_the_updated_selection() {
    let events: Vec<EventEnvelope> = round_trip("events/track_switch_sequence.json");

    let Event::StateChanged(loading) = &events[0].event else {
        panic!("首个事件应为 state_changed");
    };
    assert_eq!(loading.from, PlayerState::Paused);
    assert_eq!(loading.to, PlayerState::Loading);

    let Event::FileLoaded(loaded) = &events[1].event else {
        panic!("第二个事件应为 file_loaded");
    };
    assert_eq!(loaded.request_id, 114);
    let selected = loaded
        .media
        .tracks
        .iter()
        .filter(|track| track.selected)
        .map(|track| track.index)
        .collect::<Vec<_>>();
    assert_eq!(
        selected,
        vec![0, 2],
        "换轨后 selected 必须指向新选中的那条音轨"
    );
    assert_eq!(
        loaded.media.audio.as_ref().expect("audio").codec,
        "dts",
        "media.audio 必须描述新选中的音轨"
    );

    let Event::StateChanged(paused) = &events[2].event else {
        panic!("第三个事件应为 state_changed");
    };
    assert_eq!(paused.from, PlayerState::Loading);
    assert_eq!(paused.to, PlayerState::Paused);

    let Event::CommandCompleted(done) = &events[3].event else {
        panic!("最后的事件应为 command_completed");
    };
    assert_eq!(done.op, CommandOp::SelectTrack);
    assert_eq!((done.request_id, done.status), (114, CommandStatus::Ok));
    assert!(done.error.is_none());
}

#[test]
fn media_types_round_trip_through_serde() {
    let media = MediaInfo {
        video: Some(VideoStreamInfo {
            width: 1920,
            height: 1080,
            codec: "h264".into(),
            fps: 23.976,
        }),
        audio: Some(AudioStreamInfo {
            codec: "aac".into(),
            channels: 2,
            sample_rate: 48_000,
        }),
        tracks: vec![
            TrackInfo {
                index: 0,
                kind: TrackKind::Video,
                codec: "h264".into(),
                language: None,
                title: None,
                default: true,
                selected: true,
            },
            TrackInfo {
                index: 1,
                kind: TrackKind::Audio,
                codec: "aac".into(),
                language: Some("eng".into()),
                title: None,
                default: true,
                selected: true,
            },
            TrackInfo {
                index: 2,
                kind: TrackKind::Audio,
                codec: "aac".into(),
                language: Some("jpn".into()),
                title: None,
                default: false,
                selected: false,
            },
            TrackInfo {
                index: 3,
                kind: TrackKind::Subtitle,
                codec: "ass".into(),
                language: Some("jpn".into()),
                title: Some("双语".into()),
                default: false,
                selected: true,
            },
        ],
    };
    let value = serde_json::to_value(&media).unwrap();
    assert_eq!(value["video"]["width"], 1920);
    assert_eq!(value["audio"]["sample_rate"], 48_000);
    // 没被选中的轨也要留在列表里：UI 靠它列可选项，并说明位图字幕轨切不动
    assert_eq!(value["tracks"][2]["selected"], false);
    assert_eq!(value["tracks"][3]["kind"], "subtitle");
    assert_eq!(read_json("snapshot/playing.json")["media"], value);
}
