//! `set_decode_mode` 回归 — 快照默认值、同步能力门与切解码方式的重开语义。
//!
//! 真实文件部分用 `libdyl/test_video.mp4`（工作区自带）；切到 `auto` 在未实测平台
//! 会被同步拒绝，所以重开测试只在 Windows 上编译。断言不要求这台机器真有可用
//! 硬件：没有设备时 FFmpeg 自己回退软解，测试只钉住"backend 与 device 一致、
//! 位置不回跳、只发一个 command_completed"这些与设备无关的性质。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandEnvelope, CommandOp, CommandStatus, DecodeBackend, DecodeMode, Event,
    EventEnvelope, PlayerState, StateChangeReason,
};
use dyl_engine::Engine;

const WAIT: Duration = Duration::from_secs(20);

fn asset_path() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_video.mp4");
    path.exists().then_some(path)
}

fn drain_until_completed(
    rx: &Receiver<EventEnvelope>,
    request_id: u64,
    op: CommandOp,
) -> (CommandStatus, Vec<Event>) {
    let deadline = Instant::now() + WAIT;
    let mut events = Vec::new();
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for command_completed");
        let envelope = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("event channel closed before request {request_id}"));
        if let Event::CommandCompleted(completed) = &envelope.event {
            if completed.request_id == request_id {
                assert_eq!(completed.op, op);
                return (completed.status, events);
            }
        }
        events.push(envelope.event);
    }
}

fn flush(rx: &Receiver<EventEnvelope>) {
    while rx.try_recv().is_ok() {}
}

#[test]
fn new_engine_reports_software_decode() {
    let (mut engine, _rx) = Engine::new().expect("engine must start");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.decode.mode, DecodeMode::Software);
    assert_eq!(snapshot.decode.backend, DecodeBackend::Software);
    assert!(snapshot.decode.device.is_none());
    engine.destroy();
}

#[test]
fn software_mode_is_idempotent_and_never_reopens() {
    let (mut engine, rx) = Engine::new().expect("engine must start");
    engine
        .command(CommandEnvelope::with_request_id(
            1,
            Command::SetDecodeMode {
                mode: DecodeMode::Software,
            },
        ))
        .expect("software is always accepted");
    let (status, events) = drain_until_completed(&rx, 1, CommandOp::SetDecodeMode);
    assert_eq!(status, CommandStatus::Ok);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::StateChanged(changed) if changed.to == PlayerState::Loading
        )),
        "同一种解码方式不得推进 loading：{events:?}"
    );
    assert_eq!(engine.snapshot().state, PlayerState::Idle);
    engine.destroy();
}

#[cfg(not(target_os = "windows"))]
#[test]
fn auto_is_synchronously_rejected_without_verified_hardware() {
    let (mut engine, rx) = Engine::new().expect("engine must start");
    let error = engine
        .command(CommandEnvelope::with_request_id(
            2,
            Command::SetDecodeMode {
                mode: DecodeMode::Auto,
            },
        ))
        .expect_err("auto is not a declared capability on this platform");
    assert_eq!(error.code, dyl_core::contract::ErrorCode::NotSupported);
    assert!(rx.try_recv().is_err(), "同步拒绝不得产生任何事件");
    assert_eq!(engine.snapshot().decode.mode, DecodeMode::Software);
    engine.destroy();
}

#[cfg(target_os = "windows")]
fn load_and_play(
    engine: &mut Engine,
    rx: &Receiver<EventEnvelope>,
    asset: &std::path::Path,
) -> u64 {
    let request_id = engine
        .command(CommandEnvelope::with_request_id(
            1,
            Command::LoadFile {
                url: asset.to_string_lossy().to_string(),
            },
        ))
        .expect("load_file is accepted");
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("load timed out");
        let envelope = rx.recv_timeout(left).expect("no events while loading");
        if let Event::CommandCompleted(completed) = &envelope.event {
            assert_eq!(completed.request_id, request_id);
            assert_eq!(completed.status, CommandStatus::Ok, "{completed:?}");
            break;
        }
    }

    engine
        .command(CommandEnvelope::with_request_id(2, Command::Play))
        .expect("play is accepted in ready");
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = engine.snapshot();
        assert_ne!(
            snapshot.state,
            PlayerState::Error,
            "{:?}",
            snapshot.last_error
        );
        if snapshot.position_ms >= 1_000 {
            return snapshot.position_ms;
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("playback did not advance");
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// 播放中切 `auto`：必须重开管线、落回 playing，且不重发 `file_loaded`。
/// 硬件是否真的可用不在这里断言——没有设备时 FFmpeg 回退软解，此时
/// `backend` 必须是 software 且 `device` 为空。
#[cfg(target_os = "windows")]
#[test]
fn switching_decode_while_playing_keeps_the_position_and_events_unique() {
    let Some(asset) = asset_path() else {
        eprintln!("SKIP: test_video.mp4 不存在");
        return;
    };
    let (mut engine, rx) = Engine::new().expect("engine must start");
    let before_position = load_and_play(&mut engine, &rx, &asset);
    flush(&rx);

    engine
        .command(CommandEnvelope::with_request_id(
            3,
            Command::SetDecodeMode {
                mode: DecodeMode::Auto,
            },
        ))
        .expect("auto is a declared capability on Windows");

    let mut saw_loading = 0;
    let mut landed = None;
    let (status, events) = drain_until_completed(&rx, 3, CommandOp::SetDecodeMode);
    assert_eq!(status, CommandStatus::Ok, "{events:?}");
    for event in &events {
        match event {
            Event::StateChanged(changed) => match changed.to {
                PlayerState::Loading => saw_loading += 1,
                PlayerState::Playing => landed = Some((changed.from, changed.to, changed.reason)),
                other => panic!("unexpected landing state {other:?}"),
            },
            Event::FileLoaded(_) => {
                panic!("切解码方式没有换文件，不得重发 file_loaded：{events:?}")
            }
            Event::SeekCompleted(_) => {
                panic!("位置恢复是内部 seek，不得对外发 seek_completed：{events:?}")
            }
            _ => {}
        }
    }
    assert_eq!(saw_loading, 1, "切解码方式恰好经过一次 loading");
    let (from, to, reason) = landed.expect("switch lands back on playing");
    assert_eq!(
        (from, to, reason),
        (
            PlayerState::Loading,
            PlayerState::Playing,
            StateChangeReason::Restart
        )
    );

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.decode.mode, DecodeMode::Auto);
    eprintln!(
        "decode after auto: backend={:?} device={:?}",
        snapshot.decode.backend, snapshot.decode.device
    );
    match snapshot.decode.backend {
        DecodeBackend::Hardware => assert!(
            snapshot.decode.device.is_some(),
            "硬解生效时必须报告实际设备"
        ),
        DecodeBackend::Software => assert!(snapshot.decode.device.is_none(), "软解时不得报告设备"),
    }

    // 位置钉：切换瞬间与之后采样都不许回跳到更早的位置
    let mut positions = vec![snapshot.position_ms];
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        positions.push(engine.snapshot().position_ms);
        std::thread::sleep(Duration::from_millis(50));
    }
    let worst_drop = positions
        .windows(2)
        .map(|window| window[0].saturating_sub(window[1]))
        .max()
        .unwrap_or(0);
    assert!(
        worst_drop < 100,
        "切解码方式后位置从 {before_position}ms 回跳了 {worst_drop}ms：{positions:?}"
    );

    // 再切回 software：实际后端必须立刻回到 software
    engine
        .command(CommandEnvelope::with_request_id(
            4,
            Command::SetDecodeMode {
                mode: DecodeMode::Software,
            },
        ))
        .expect("switching back to software is accepted");
    let (status, _events) = drain_until_completed(&rx, 4, CommandOp::SetDecodeMode);
    assert_eq!(status, CommandStatus::Ok);
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.decode.mode, DecodeMode::Software);
    assert_eq!(snapshot.decode.backend, DecodeBackend::Software);
    assert!(snapshot.decode.device.is_none());
    assert_eq!(snapshot.state, PlayerState::Playing);
    engine.destroy();
}

/// ended/stopped 没有活管线：只持久化设置，不重开、不进 loading，
/// 下一次 play 重启时按新设置建解码器。
#[cfg(target_os = "windows")]
#[test]
fn decode_mode_is_only_persisted_when_no_pipeline_is_alive() {
    let Some(asset) = asset_path() else {
        eprintln!("SKIP: test_video.mp4 不存在");
        return;
    };
    let (mut engine, rx) = Engine::new().expect("engine must start");
    load_and_play(&mut engine, &rx, &asset);

    engine
        .command(CommandEnvelope::with_request_id(5, Command::Stop))
        .expect("stop is accepted");
    let (status, _events) = drain_until_completed(&rx, 5, CommandOp::Stop);
    assert_eq!(status, CommandStatus::Ok);
    assert_eq!(engine.snapshot().state, PlayerState::Stopped);
    flush(&rx);

    engine
        .command(CommandEnvelope::with_request_id(
            6,
            Command::SetDecodeMode {
                mode: DecodeMode::Auto,
            },
        ))
        .expect("setting a decode mode without a pipeline is accepted");
    let (status, events) = drain_until_completed(&rx, 6, CommandOp::SetDecodeMode);
    assert_eq!(status, CommandStatus::Ok);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::StateChanged(changed) if changed.to == PlayerState::Loading
        )),
        "无活管线时切解码方式不得推进 loading：{events:?}"
    );
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.state, PlayerState::Stopped);
    assert_eq!(snapshot.decode.mode, DecodeMode::Auto);
    assert_eq!(snapshot.decode.backend, DecodeBackend::Software);

    engine
        .command(CommandEnvelope::with_request_id(7, Command::Play))
        .expect("play from stopped restarts the pipeline");
    let (status, _events) = drain_until_completed(&rx, 7, CommandOp::Play);
    assert_eq!(status, CommandStatus::Ok);
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.state, PlayerState::Playing);
    assert_eq!(snapshot.decode.mode, DecodeMode::Auto);
    match snapshot.decode.backend {
        DecodeBackend::Hardware => assert!(snapshot.decode.device.is_some()),
        DecodeBackend::Software => assert!(snapshot.decode.device.is_none()),
    }
    engine.destroy();
}
