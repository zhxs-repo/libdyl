//! 引擎集成测试 — 不需要 GPU、音频设备或媒体文件。
//!
//! 覆盖同步校验（形状/范围/静态能力）、异步状态拒绝、快照默认值、
//! 事件信封顺序与 command_completed 的唯一性。

use std::time::Duration;

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandEnvelope, CommandOp, CommandStatus, DecodeBackend, DecodeMode, ErrorCode,
    Event, EventEnvelope, PlayerState, SeekMode, SurfaceDescriptor, SurfaceKind, SurfaceStatus,
    SCHEMA_VERSION,
};
use dyl_engine::{Engine, HostFrameSink};
use dyl_ffmpeg::VideoFrame;

fn make_engine() -> (Engine, Receiver<EventEnvelope>) {
    Engine::new().expect("engine must start")
}

fn next_event(rx: &Receiver<EventEnvelope>) -> EventEnvelope {
    rx.recv_timeout(Duration::from_secs(5))
        .expect("expected an engine event")
}

/// 事件可能在命令处理中途产生，命令完成本身一定在最后
fn next_completed(rx: &Receiver<EventEnvelope>) -> dyl_core::contract::CommandCompleted {
    loop {
        let envelope = next_event(rx);
        if let Event::CommandCompleted(completed) = envelope.event {
            return completed;
        }
    }
}

#[test]
fn new_engine_starts_idle_with_contract_defaults() {
    let (engine, _rx) = make_engine();
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.schema, SCHEMA_VERSION);
    assert_eq!(snapshot.state, PlayerState::Idle);
    assert_eq!(snapshot.position_ms, 0);
    assert_eq!(snapshot.duration_ms, 0);
    assert_eq!(snapshot.volume, 100);
    assert!(!snapshot.muted);
    assert_eq!(snapshot.decode.mode, DecodeMode::Software);
    assert_eq!(snapshot.decode.backend, DecodeBackend::Software);
    assert!(snapshot.decode.device.is_none());
    assert!(snapshot.source.is_none());
    assert!(snapshot.media.is_none());
    assert!(snapshot.subtitle.visible);
    assert!(!snapshot.subtitle.loaded);
    assert!(snapshot.danmaku.visible);
    assert_eq!(snapshot.danmaku.opacity, 100);
    assert_eq!(snapshot.danmaku.count, 0);
    assert_eq!(snapshot.surface.status, SurfaceStatus::None);
    assert!(snapshot.surface.kind.is_none());
    assert_eq!(snapshot.surface.scale_factor, 1.0);
    assert!(snapshot.last_error.is_none());
}

#[test]
fn state_rejection_is_asynchronous() {
    let (engine, rx) = make_engine();

    let id = engine
        .command(CommandEnvelope::with_request_id(7, Command::Play))
        .expect("play is accepted into the queue; state is checked asynchronously");
    assert_eq!(id, 7);

    let completed = next_completed(&rx);
    assert_eq!(completed.request_id, 7);
    assert_eq!(completed.op, CommandOp::Play);
    assert_eq!(completed.status, CommandStatus::Error);
    assert_eq!(
        completed.error.as_ref().map(|error| error.code),
        Some(ErrorCode::InvalidState)
    );
    assert_eq!(engine.snapshot().state, PlayerState::Idle);
}

#[test]
fn set_volume_updates_snapshot_before_completion() {
    let (engine, rx) = make_engine();

    engine
        .command(CommandEnvelope::with_request_id(
            1,
            Command::SetVolume { volume: 55 },
        ))
        .expect("set_volume is always accepted");

    match next_event(&rx).event {
        Event::SettingsChanged(settings) => {
            assert_eq!(settings.volume, 55);
            assert!(!settings.muted);
            assert_eq!(settings.decode_mode, DecodeMode::Software);
            assert!(settings.subtitle_visible);
            assert!(settings.danmaku_visible);
            assert_eq!(settings.danmaku_opacity, 100);
        }
        other => panic!("expected settings_changed, got {other:?}"),
    }
    let completed = next_completed(&rx);
    assert_eq!(completed.op, CommandOp::SetVolume);
    assert_eq!(completed.status, CommandStatus::Ok);
    assert_eq!(engine.snapshot().volume, 55);
}

#[test]
fn synchronous_validation_rejects_bad_shape_and_ranges() {
    let (engine, rx) = make_engine();

    let cases = [
        CommandEnvelope {
            schema: 2,
            request_id: Some(1),
            command: Command::Play,
        },
        CommandEnvelope::with_request_id(0, Command::Play),
        CommandEnvelope::with_request_id(2, Command::LoadFile { url: "  ".into() }),
        CommandEnvelope::with_request_id(3, Command::SetVolume { volume: 101 }),
        CommandEnvelope::with_request_id(4, Command::SetDanmakuOpacity { opacity: 101 }),
    ];

    for (index, envelope) in cases.into_iter().enumerate() {
        let error = engine
            .command(envelope)
            .expect_err("synchronous validation must reject");
        assert_eq!(
            error.code,
            ErrorCode::InvalidArgument,
            "case {index}: {error}"
        );
    }

    assert!(
        rx.try_recv().is_err(),
        "synchronous rejection must not emit any event"
    );
}

#[test]
fn keyframe_seek_passes_synchronous_validation() {
    let (engine, rx) = make_engine();
    assert!(
        dyl_engine::capabilities::capabilities()
            .features
            .seek_modes
            .contains(&SeekMode::Keyframe),
        "keyframe 已实现，能力位必须声明"
    );

    let id = engine
        .command(CommandEnvelope::with_request_id(
            5,
            Command::Seek {
                position_ms: 1000,
                mode: SeekMode::Keyframe,
            },
        ))
        .expect("keyframe seek must pass synchronous validation");
    assert_eq!(id, 5);

    let completed = next_completed(&rx);
    assert_eq!(completed.request_id, 5);
    assert_eq!(completed.op, CommandOp::Seek);
    assert_eq!(completed.status, CommandStatus::Error);
    assert_eq!(
        completed.error.as_ref().map(|error| error.code),
        Some(ErrorCode::InvalidState),
        "idle 中的 seek 只能在异步阶段报 invalid_state"
    );
}

#[test]
fn request_id_is_allocated_when_omitted() {
    let (engine, rx) = make_engine();

    let id = engine
        .command(CommandEnvelope::new(Command::SetMute { muted: true }))
        .expect("command without request_id is accepted");
    assert!(id > 0);

    let completed = next_completed(&rx);
    assert_eq!(completed.request_id, id);
    assert_eq!(completed.status, CommandStatus::Ok);
    assert!(engine.snapshot().muted);
}

#[test]
fn missing_file_load_fails_asynchronously_and_clears_source() {
    let (engine, rx) = make_engine();
    let missing = std::env::temp_dir().join("dyl_engine_missing_media.mp4");
    let missing = missing.to_string_lossy().to_string();

    engine
        .command(CommandEnvelope::with_request_id(
            42,
            Command::LoadFile {
                url: missing.clone(),
            },
        ))
        .expect("load_file passes synchronous validation");

    match next_event(&rx).event {
        Event::StateChanged(changed) => {
            assert_eq!(changed.from, PlayerState::Idle);
            assert_eq!(changed.to, PlayerState::Loading);
        }
        other => panic!("expected state_changed(loading), got {other:?}"),
    }

    match next_event(&rx).event {
        Event::StateChanged(changed) => {
            assert_eq!(changed.from, PlayerState::Loading);
            assert_eq!(changed.to, PlayerState::Error);
        }
        other => panic!("expected state_changed(error), got {other:?}"),
    }

    let completed = next_completed(&rx);
    assert_eq!(completed.request_id, 42);
    assert_eq!(completed.op, CommandOp::LoadFile);
    assert_eq!(completed.status, CommandStatus::Error);
    assert_eq!(
        completed.error.as_ref().map(|error| error.code),
        Some(ErrorCode::FileNotFound)
    );

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.state, PlayerState::Error);
    assert!(snapshot.source.is_none());
    assert!(snapshot.media.is_none());
    let last_error = snapshot
        .last_error
        .as_ref()
        .expect("error must be recorded");
    assert_eq!(last_error.code, ErrorCode::FileNotFound);
    assert!(last_error.message.contains("dyl_engine_missing_media"));
}

#[test]
fn stop_in_idle_is_idempotent_without_state_change() {
    let (engine, rx) = make_engine();

    engine
        .command(CommandEnvelope::with_request_id(9, Command::Stop))
        .expect("stop is accepted in idle");

    let completed = next_completed(&rx);
    assert_eq!(completed.status, CommandStatus::Ok);
    assert_eq!(engine.snapshot().state, PlayerState::Idle);
}

#[test]
fn surface_resize_requires_attachment_and_detach_is_idempotent() {
    let (engine, _rx) = make_engine();

    engine
        .detach_surface()
        .expect("detach without a surface is a no-op");
    let error = engine
        .resize_surface(640, 360)
        .expect_err("resize requires an attached surface");
    assert_eq!(error.code, ErrorCode::InvalidState);

    let error = engine
        .resize_surface(0, 360)
        .expect_err("zero size is invalid");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
fn unsupported_surface_kind_is_rejected_before_creating_gpu_resources() {
    let (engine, _rx) = make_engine();
    let supported = dyl_engine::capabilities::capabilities()
        .presentation
        .surface_kinds;

    let desc = if supported.contains(&SurfaceKind::WindowsHwnd) {
        SurfaceDescriptor::X11Window {
            schema: SCHEMA_VERSION,
            handle: "0x1".into(),
            display: "0x2".into(),
            width: 640,
            height: 360,
            scale_factor: 1.0,
        }
    } else {
        SurfaceDescriptor::WindowsHwnd {
            schema: SCHEMA_VERSION,
            handle: "0x1".into(),
            width: 640,
            height: 360,
            scale_factor: 1.0,
        }
    };

    let error = engine
        .attach_surface(desc)
        .expect_err("unsupported surface kind must be rejected");
    assert_eq!(error.code, ErrorCode::NotSupported);
    assert_eq!(engine.snapshot().surface.status, SurfaceStatus::None);
}

#[test]
fn invalid_surface_handles_are_rejected() {
    let (engine, _rx) = make_engine();
    let supported = dyl_engine::capabilities::capabilities()
        .presentation
        .surface_kinds;
    if !supported.contains(&SurfaceKind::WindowsHwnd) {
        return;
    }

    let desc = SurfaceDescriptor::WindowsHwnd {
        schema: SCHEMA_VERSION,
        handle: "0x0".into(),
        width: 640,
        height: 360,
        scale_factor: 1.0,
    };
    let error = engine
        .attach_surface(desc)
        .expect_err("zero handle must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidArgument);

    let desc = SurfaceDescriptor::WindowsHwnd {
        schema: SCHEMA_VERSION,
        handle: "0x1".into(),
        width: 0,
        height: 360,
        scale_factor: 1.0,
    };
    let error = engine
        .attach_surface(desc)
        .expect_err("zero size must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

struct SilentSink;

impl HostFrameSink for SilentSink {
    fn push_frame(&self, _frame: VideoFrame) {}

    fn request_redraw(&self) {}
}

#[test]
fn host_sink_handles_share_one_clock_and_stats() {
    let (engine, _rx) = make_engine();
    engine.set_host_frame_sink(std::sync::Arc::new(SilentSink));

    let first = engine.host_output_handles();
    let second = engine.host_output_handles();
    assert!(std::sync::Arc::ptr_eq(
        &first.overlay_clock,
        &second.overlay_clock
    ));
    assert!(std::sync::Arc::ptr_eq(&first.stats, &second.stats));
    assert!(std::sync::Arc::ptr_eq(&first.stats, &engine.output_stats()));

    let overlays = engine.overlay_handles();
    let again = engine.overlay_handles();
    assert!(std::sync::Arc::ptr_eq(&overlays.subtitle, &again.subtitle));
    assert!(std::sync::Arc::ptr_eq(&overlays.danmaku, &again.danmaku));

    // host 模式不占用 external Surface 状态位
    assert_eq!(engine.snapshot().surface.status, SurfaceStatus::None);
}

#[test]
fn event_seq_increases_monotonically() {
    let (engine, rx) = make_engine();

    engine
        .command(CommandEnvelope::with_request_id(
            1,
            Command::SetVolume { volume: 40 },
        ))
        .unwrap();
    engine
        .command(CommandEnvelope::with_request_id(
            2,
            Command::SetMute { muted: true },
        ))
        .unwrap();

    let mut last_seq = 0;
    let mut count = 0;
    while count < 4 {
        let envelope = next_event(&rx);
        assert_eq!(envelope.schema, SCHEMA_VERSION);
        assert!(envelope.seq > last_seq, "seq must increase");
        last_seq = envelope.seq;
        count += 1;
    }
}
