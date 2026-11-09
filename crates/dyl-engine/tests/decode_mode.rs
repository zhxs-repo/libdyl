//! `set_decode_mode` 回归 — 快照默认值、`auto` 的普适接受与切解码方式的重开语义。
//!
//! 真实文件部分用 `libdyl/test_video.mp4`（工作区自带）。`auto` 在所有平台都被
//! 接受（硬解优先、失败/未实测回退软解），所以重开测试只在 Windows 上编译——
//! 那里 `auto` 会真的尝试设备。断言不要求这台机器真有可用硬件：没有设备时
//! FFmpeg 自己回退软解，测试只钉住"backend 与 device 一致、位置不回跳、只发一个
//! command_completed"这些与设备无关的性质。

#[cfg(target_os = "windows")]
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
#[cfg(target_os = "windows")]
use dyl_core::contract::StateChangeReason;
use dyl_core::contract::{
    Command, CommandEnvelope, CommandOp, CommandStatus, DecodeBackend, DecodeMode, Event,
    EventEnvelope, PlayerState,
};
use dyl_engine::Engine;

const WAIT: Duration = Duration::from_secs(20);

// 与 asset_path 同 cfg：否则非 Windows 上这个辅助模块没人用，会报 dead_code
#[cfg(target_os = "windows")]
mod common;

#[cfg(target_os = "windows")]
fn asset_path() -> Option<PathBuf> {
    common::asset("test_video.mp4")
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

#[cfg(target_os = "windows")]
fn flush(rx: &Receiver<EventEnvelope>) {
    while rx.try_recv().is_ok() {}
}

#[test]
fn new_engine_defaults_to_auto_decode() {
    let (mut engine, _rx) = Engine::new().expect("engine must start");
    let snapshot = engine.snapshot();
    // 没有管线时 backend 只报软解；mode 是请求设置，默认 auto
    assert_eq!(snapshot.decode.mode, DecodeMode::Auto);
    assert_eq!(snapshot.decode.backend, DecodeBackend::Software);
    assert!(snapshot.decode.device.is_none());
    engine.destroy();
}

#[test]
fn software_mode_without_pipeline_only_persists() {
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
        "无活管线时切解码方式不得推进 loading：{events:?}"
    );
    assert_eq!(engine.snapshot().decode.mode, DecodeMode::Software);
    assert_eq!(engine.snapshot().state, PlayerState::Idle);

    // 重设当前值幂等：不推进任何状态
    engine
        .command(CommandEnvelope::with_request_id(
            2,
            Command::SetDecodeMode {
                mode: DecodeMode::Software,
            },
        ))
        .expect("repeating the current mode is accepted");
    let (status, events) = drain_until_completed(&rx, 2, CommandOp::SetDecodeMode);
    assert_eq!(status, CommandStatus::Ok);
    assert!(events.is_empty(), "幂等重设不得产生事件：{events:?}");
    engine.destroy();
}

#[cfg(not(target_os = "windows"))]
#[test]
fn auto_is_accepted_and_falls_back_to_software() {
    let (mut engine, rx) = Engine::new().expect("engine must start");
    engine
        .command(CommandEnvelope::with_request_id(
            2,
            Command::SetDecodeMode {
                mode: DecodeMode::Auto,
            },
        ))
        .expect("auto is accepted on every platform (hardware is merely not attempted)");
    let (status, events) = drain_until_completed(&rx, 2, CommandOp::SetDecodeMode);
    assert_eq!(status, CommandStatus::Ok, "{events:?}");
    assert_eq!(engine.snapshot().decode.mode, DecodeMode::Auto);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::StateChanged(changed) if changed.to == PlayerState::Loading
        )),
        "无活管线时切解码方式不得推进 loading：{events:?}"
    );
    assert_eq!(engine.snapshot().decode.backend, DecodeBackend::Software);
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

/// 播放中切解码方式：默认 `auto` 下先切 `software`、再切回 `auto`，两次都必须
/// 重开管线、落回 playing，且不重发 `file_loaded`。硬件是否真的可用不在这里
/// 断言——没有设备时 FFmpeg 回退软解，此时 `backend` 必须是 software 且
/// `device` 为空。
#[cfg(target_os = "windows")]
#[test]
fn switching_decode_while_playing_keeps_the_position_and_events_unique() {
    let Some(asset) = asset_path() else {
        eprintln!(
            "SKIP: test_video.mp4 不存在（根目录或 tmp_verify/，可用 DYL_TEST_ASSET_DIR 指定）"
        );
        return;
    };
    let (mut engine, rx) = Engine::new().expect("engine must start");
    assert_eq!(engine.snapshot().decode.mode, DecodeMode::Auto);
    let before_position = load_and_play(&mut engine, &rx, &asset);
    flush(&rx);

    // 默认就是 auto：先切 software，验证重开语义
    engine
        .command(CommandEnvelope::with_request_id(
            3,
            Command::SetDecodeMode {
                mode: DecodeMode::Software,
            },
        ))
        .expect("software is always accepted");

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
    assert_eq!(engine.snapshot().decode.mode, DecodeMode::Software);
    assert_eq!(engine.snapshot().decode.backend, DecodeBackend::Software);
    assert!(engine.snapshot().decode.device.is_none());

    // 再切回 auto：重开一次，实际后端必须自洽（有设备才报 hardware）
    engine
        .command(CommandEnvelope::with_request_id(
            4,
            Command::SetDecodeMode {
                mode: DecodeMode::Auto,
            },
        ))
        .expect("auto is a declared capability on Windows");
    let (status, events) = drain_until_completed(&rx, 4, CommandOp::SetDecodeMode);
    assert_eq!(status, CommandStatus::Ok, "{events:?}");
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

    // 位置钉：切解码方式后位置恢复走 exact seek，FFmpeg 先到关键帧再精确 seek
    // 到目标帧。过渡中 pipeline 的 current_position 可能短暂落到关键帧（最多一个
    // GOP ≈ 1s），引擎的位置钉应遮蔽这个过渡。但某些平台（如 CI 的 dxva2 回退）
    // 时序可能让一两个采样漏过，所以断言允许短暂回跳但要求最终位置恢复且继续推进。
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
    let final_position = *positions.last().unwrap_or(&0);
    assert!(
        worst_drop < 1100,
        "切解码方式后位置从 {before_position}ms 回跳了 {worst_drop}ms（超过一个 GOP）：{positions:?}"
    );
    assert!(
        final_position >= before_position.saturating_sub(100),
        "切解码方式后最终位置 {final_position}ms 回跳到 before_position {before_position}ms 以下：{positions:?}"
    );
    assert_eq!(engine.snapshot().state, PlayerState::Playing);
    engine.destroy();
}

/// ended/stopped 没有活管线：只持久化设置，不重开、不进 loading，
/// 下一次 play 重启时按新设置建解码器。
#[cfg(target_os = "windows")]
#[test]
fn decode_mode_is_only_persisted_when_no_pipeline_is_alive() {
    let Some(asset) = asset_path() else {
        eprintln!(
            "SKIP: test_video.mp4 不存在（根目录或 tmp_verify/，可用 DYL_TEST_ASSET_DIR 指定）"
        );
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
                mode: DecodeMode::Software,
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
    assert_eq!(snapshot.decode.mode, DecodeMode::Software);
    assert_eq!(snapshot.decode.backend, DecodeBackend::Software);

    engine
        .command(CommandEnvelope::with_request_id(7, Command::Play))
        .expect("play from stopped restarts the pipeline");
    let (status, _events) = drain_until_completed(&rx, 7, CommandOp::Play);
    assert_eq!(status, CommandStatus::Ok);
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.state, PlayerState::Playing);
    assert_eq!(snapshot.decode.mode, DecodeMode::Software);
    assert_eq!(
        snapshot.decode.backend,
        DecodeBackend::Software,
        "设置的是 software，重启后的实际后端必须是软解"
    );
    assert!(snapshot.decode.device.is_none());
    engine.destroy();
}
