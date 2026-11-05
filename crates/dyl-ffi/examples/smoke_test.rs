//! Smoke test — 命令校验、异步完成与状态收敛
//!
//! 不需要真实视频文件或 GPU 窗口，仅验证引擎门面。
//! 运行: `cargo run --example smoke_test -p dyl-ffi`

use std::time::Duration;

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandCompleted, CommandEnvelope, CommandStatus, ErrorCode, Event, EventEnvelope,
    PlayerState,
};
use dyl_engine::Engine;

const WAIT: Duration = Duration::from_secs(5);

fn next_completed(events: &Receiver<EventEnvelope>) -> CommandCompleted {
    loop {
        let envelope = events.recv_timeout(WAIT).expect("expected an event");
        if let Event::CommandCompleted(completed) = envelope.event {
            return completed;
        }
    }
}

fn main() {
    println!("=== Dyl Smoke Test ===\n");
    let mut passed = 0u32;
    let mut failed = 0u32;

    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                println!("  [PASS] {}", $name);
                passed += 1;
            } else {
                println!("  [FAIL] {}", $name);
                failed += 1;
            }
        };
    }

    let (mut engine, events) = Engine::new().expect("engine must start");

    // --- 1. 初始快照 ---
    println!("\n--- 1. New Engine ---");
    let snapshot = engine.snapshot();
    check!("starts idle", snapshot.state == PlayerState::Idle);
    check!("volume defaults to 100", snapshot.volume == 100);
    check!("source is empty", snapshot.source.is_none());

    // --- 2. Idle 下的异步状态拒绝 ---
    println!("\n--- 2. Play in Idle ---");
    let id = engine
        .command(CommandEnvelope::with_request_id(1, Command::Play))
        .expect("play enters the queue; state is checked asynchronously");
    check!("play is accepted into the queue", id == 1);
    let completed = next_completed(&events);
    check!(
        &format!("play completes with invalid_state ({:?})", completed.status),
        completed.status == CommandStatus::Error
            && completed.error.map(|error| error.code) == Some(ErrorCode::InvalidState)
    );
    check!(
        "state stays idle",
        engine.snapshot().state == PlayerState::Idle
    );

    // --- 3. 全局设置命令 ---
    println!("\n--- 3. Settings ---");
    engine
        .command(CommandEnvelope::with_request_id(
            2,
            Command::SetVolume { volume: 80 },
        ))
        .unwrap();
    let completed = next_completed(&events);
    check!(
        &format!("set_volume completes ok ({:?})", completed.status),
        completed.status == CommandStatus::Ok
    );
    check!("snapshot volume is 80", engine.snapshot().volume == 80);

    platform_settings(&mut engine, &events, &mut passed, &mut failed);

    // --- 4. 文件不存在 ---
    println!("\n--- 4. Missing File ---");
    engine
        .command(CommandEnvelope::with_request_id(
            3,
            Command::LoadFile {
                url: "C:\\nonexistent\\path\\dyl_smoke.mp4".into(),
            },
        ))
        .expect("load_file passes synchronous validation");

    let mut saw_loading = false;
    let mut saw_error_state = false;
    let completed = loop {
        let envelope = events.recv_timeout(WAIT).expect("expected an event");
        match envelope.event {
            Event::StateChanged(changed) if changed.to == PlayerState::Loading => {
                saw_loading = true;
            }
            Event::StateChanged(changed) if changed.to == PlayerState::Error => {
                saw_error_state = true;
            }
            Event::CommandCompleted(completed) => break completed,
            _ => {}
        }
    };
    check!("enters loading first", saw_loading);
    check!("converges to error", saw_error_state);
    check!(
        &format!("fails with file_not_found ({:?})", completed.error),
        completed.error.as_ref().map(|error| error.code) == Some(ErrorCode::FileNotFound)
    );
    let snapshot = engine.snapshot();
    check!(
        "snapshot state is error",
        snapshot.state == PlayerState::Error
    );
    check!(
        "snapshot records last_error",
        snapshot.last_error.as_ref().is_some_and(|error| {
            error.code == ErrorCode::FileNotFound && error.message.contains("dyl_smoke.mp4")
        })
    );

    // --- 5. Error 态恢复 ---
    println!("\n--- 5. Stop from Error ---");
    engine
        .command(CommandEnvelope::with_request_id(4, Command::Stop))
        .unwrap();
    let completed = next_completed(&events);
    check!(
        &format!("stop completes ok ({:?})", completed.status),
        completed.status == CommandStatus::Ok
    );
    check!(
        "state becomes stopped",
        engine.snapshot().state == PlayerState::Stopped
    );

    // --- 汇总 ---
    println!("\n=== Results: {} passed, {} failed ===", passed, failed);
    if failed > 0 {
        std::process::exit(1);
    }
    println!("All smoke tests passed!");
}

fn platform_settings(
    engine: &mut Engine,
    events: &Receiver<EventEnvelope>,
    passed: &mut u32,
    failed: &mut u32,
) {
    let mut check = |name: &str, ok: bool| {
        if ok {
            println!("  [PASS] {name}");
            *passed += 1;
        } else {
            println!("  [FAIL] {name}");
            *failed += 1;
        }
    };

    engine
        .command(CommandEnvelope::with_request_id(
            5,
            Command::SetMute { muted: true },
        ))
        .unwrap();
    let completed = next_completed(events);
    check(
        "set_mute completes ok",
        completed.status == CommandStatus::Ok,
    );
    check("snapshot is muted", engine.snapshot().muted);

    engine
        .command(CommandEnvelope::with_request_id(
            6,
            Command::SetSubtitleVisible { visible: false },
        ))
        .unwrap();
    next_completed(events);
    check(
        "subtitle visibility is reflected in the snapshot",
        !engine.snapshot().subtitle.visible,
    );

    engine
        .command(CommandEnvelope::with_request_id(
            7,
            Command::SetDanmakuOpacity { opacity: 40 },
        ))
        .unwrap();
    next_completed(events);
    check(
        "danmaku opacity is reflected in the snapshot",
        engine.snapshot().danmaku.opacity == 40,
    );
}
