//! 长时间暂停后恢复播放的验收工具
//!
//! 暂停期间音频设备是被主动冻结的，时钟不再推进；恢复时如果把这些时长当成设备
//! 停摆，`audio output device: stalled` 会以致命错误收掉整条管线（暂停越久越必然
//! 触发）。这条用例用真实播放验证"暂停 N 秒 → 恢复"仍能继续放，且没有 error 事件。
//!
//! 运行: `cargo run --example long_pause_resume -p dyl-ffi -- test_video.mp4 30`

use std::time::{Duration, Instant};

use dyl_core::contract::{
    Command, CommandEnvelope, CommandStatus, Event, EventEnvelope, PlayerState,
};
use dyl_engine::Engine;

fn main() {
    std::env::set_var("RUST_BACKTRACE", "1");
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "dyl=warn");
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = std::env::args().collect();
    let source = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "test_video.mp4".into());
    let pause_secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);

    let (mut engine, events) = Engine::new().expect("engine must start");
    let mut failed = 0u32;
    let mut check = |name: &str, ok: bool| {
        println!("  [{}] {}", if ok { "PASS" } else { "FAIL" }, name);
        if !ok {
            failed += 1;
        }
    };

    let request = engine
        .command(CommandEnvelope::new(Command::LoadFile {
            url: source.clone(),
        }))
        .expect("load_file must be accepted");
    let loaded = wait_completion(&events, request);
    check(
        &format!("load_file ok ({:?})", loaded.status),
        loaded.status == CommandStatus::Ok,
    );
    if loaded.status != CommandStatus::Ok {
        eprintln!("FATAL: cannot load {source}");
        std::process::exit(1);
    }

    engine
        .command(CommandEnvelope::new(Command::Play))
        .expect("play must be accepted");
    std::thread::sleep(Duration::from_millis(1500));
    let snapshot = engine.snapshot();
    check(
        &format!("playing before pause (state={:?})", snapshot.state),
        snapshot.state == PlayerState::Playing && snapshot.position_ms > 0,
    );

    engine
        .command(CommandEnvelope::new(Command::Pause))
        .expect("pause must be accepted");
    std::thread::sleep(Duration::from_millis(200));
    check("paused", engine.snapshot().state == PlayerState::Paused);
    let paused_position = engine.snapshot().position_ms;

    println!("--- 保持暂停 {pause_secs}s ---");
    std::thread::sleep(Duration::from_secs(pause_secs));
    check(
        &format!("position pinned while paused ({paused_position})"),
        engine.snapshot().position_ms == paused_position,
    );
    let mut errors = drain_errors(&events);
    check(
        &format!("no error while paused (errors={errors:?})"),
        errors.is_empty(),
    );

    engine
        .command(CommandEnvelope::new(Command::Play))
        .expect("resume must be accepted");
    let resumed_at = Instant::now();
    let deadline = resumed_at + Duration::from_secs(8);
    let mut moved = false;
    while Instant::now() < deadline {
        if let Ok(msg) = events.recv_timeout(Duration::from_millis(100)) {
            if let Event::Error(error) = &msg.event {
                errors.push(error.message.clone());
            }
        }
        let snapshot = engine.snapshot();
        if snapshot.state == PlayerState::Ended {
            moved = true;
            break;
        }
        if snapshot.position_ms > paused_position {
            moved = true;
            // 再放一小段，确认不是一帧的抖动
            std::thread::sleep(Duration::from_millis(600));
            break;
        }
    }
    let snapshot = engine.snapshot();
    check(
        &format!(
            "no stall error after resume (errors={errors:?}, took {:.1}s)",
            resumed_at.elapsed().as_secs_f64()
        ),
        errors.is_empty(),
    );
    check(
        &format!(
            "playback continues after resume (state={:?} {}ms -> {}ms)",
            snapshot.state, paused_position, snapshot.position_ms
        ),
        moved
            && matches!(snapshot.state, PlayerState::Playing | PlayerState::Ended)
            && snapshot.position_ms > paused_position,
    );

    engine.destroy();
    if failed == 0 {
        println!("LONG PAUSE PASSED");
    } else {
        eprintln!("LONG PAUSE FAILED ({failed} checks)");
        std::process::exit(1);
    }
}

fn drain_errors(events: &crossbeam_channel::Receiver<EventEnvelope>) -> Vec<String> {
    let mut errors = Vec::new();
    while let Ok(msg) = events.try_recv() {
        if let Event::Error(error) = &msg.event {
            errors.push(error.message.clone());
        }
    }
    errors
}

/// 取回 `request_id` 对应的 `command_completed`，顺带丢弃中间事件
fn wait_completion(
    events: &crossbeam_channel::Receiver<EventEnvelope>,
    request_id: u64,
) -> dyl_core::contract::CommandCompleted {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("command completion timed out");
        let msg = events
            .recv_timeout(left)
            .expect("event channel closed while waiting");
        if let Event::CommandCompleted(completed) = msg.event {
            if completed.request_id == request_id {
                return completed;
            }
        }
    }
}
