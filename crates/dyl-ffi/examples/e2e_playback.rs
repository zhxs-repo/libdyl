//! 无头 E2E 播放验证 — 无 Surface（NullOutput）下的完整状态机链路
//!
//! 验证: Load → Play → Mute → Pause → Seek → Resume → EOF → Stop
//! 运行: `cargo run --example e2e_playback -p dyl-ffi -- test_video.mp4`

use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandCompleted, CommandEnvelope, CommandStatus, Event, EventEnvelope, FileLoaded,
    PlayerState, SeekCompleted, SeekMode, StateChangeReason,
};
use dyl_engine::Engine;

const WAIT: Duration = Duration::from_secs(10);

fn wait_completion(
    events: &Receiver<EventEnvelope>,
    request_id: u64,
    mut observe: impl FnMut(&Event),
) -> CommandCompleted {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for command_completed");
        let envelope = events
            .recv_timeout(left)
            .expect("event channel closed before command_completed");
        observe(&envelope.event);
        if let Event::CommandCompleted(completed) = envelope.event {
            if completed.request_id == request_id {
                return completed;
            }
        }
    }
}

fn command(
    engine: &mut Engine,
    events: &Receiver<EventEnvelope>,
    command: Command,
) -> CommandCompleted {
    let request_id = engine
        .command(CommandEnvelope::new(command))
        .expect("command must be accepted synchronously");
    wait_completion(events, request_id, |_| {})
}

fn wait_event(
    events: &Receiver<EventEnvelope>,
    mut predicate: impl FnMut(&Event) -> bool,
) -> Event {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for event");
        let envelope = events
            .recv_timeout(left)
            .expect("event channel closed while waiting");
        if predicate(&envelope.event) {
            return envelope.event;
        }
    }
}

fn main() {
    std::env::set_var("RUST_BACKTRACE", "1");
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "dyl=warn");
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = std::env::args().collect();
    let video_path = if args.len() > 1 {
        args[1].clone()
    } else {
        "test_video.mp4".into()
    };

    println!("=== Dyl E2E Playback Verification (headless) ===");
    println!("Video: {video_path}\n");

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

    // ================================================================
    // Step 1: Load file
    // ================================================================
    println!("\n--- Step 1: Load File ---");
    let request_id = engine
        .command(CommandEnvelope::new(Command::LoadFile {
            url: video_path.clone(),
        }))
        .expect("load_file must be accepted");
    let mut file_loaded: Option<FileLoaded> = None;
    let completed = wait_completion(&events, request_id, |event| {
        if let Event::FileLoaded(info) = event {
            file_loaded = Some(info.clone());
        }
    });
    check!(
        &format!("load_file completes ok ({:?})", completed.status),
        completed.status == CommandStatus::Ok
    );
    let Some(loaded) = file_loaded else {
        eprintln!("FATAL: file_loaded not received, aborting.");
        std::process::exit(1);
    };
    check!(
        &format!("duration ~= 5000ms (got {}ms)", loaded.duration_ms),
        (loaded.duration_ms as i64 - 5000).abs() < 1000
    );
    let (width, height) = loaded
        .media
        .video
        .as_ref()
        .map(|video| (video.width, video.height))
        .unwrap_or((0, 0));
    check!(
        &format!("video 640x360 (got {width}x{height})"),
        width == 640 && height == 360
    );
    let snapshot = engine.snapshot();
    check!("snapshot is ready", snapshot.state == PlayerState::Ready);
    check!("snapshot carries media info", snapshot.media.is_some());

    // ================================================================
    // Step 2: Play
    // ================================================================
    println!("\n--- Step 2: Play ---");
    let completed = command(&mut engine, &events, Command::Play);
    check!(
        &format!("play completes ok ({:?})", completed.status),
        completed.status == CommandStatus::Ok
    );
    std::thread::sleep(Duration::from_millis(1500));
    let snapshot = engine.snapshot();
    check!(
        &format!("state is playing (got {:?})", snapshot.state),
        snapshot.state == PlayerState::Playing
    );
    check!(
        &format!("position advanced (got {}ms)", snapshot.position_ms),
        snapshot.position_ms > 0
    );

    // ================================================================
    // Step 2.5: Mute（静音只压增益，不能停主时钟）
    // ================================================================
    println!("\n--- Step 2.5: Mute ---");
    let completed = command(&mut engine, &events, Command::SetMute { muted: true });
    check!(
        "set_mute(true) completes ok",
        completed.status == CommandStatus::Ok
    );
    let pos_before_mute = engine.snapshot().position_ms;
    std::thread::sleep(Duration::from_millis(700));
    let snapshot = engine.snapshot();
    check!("snapshot is muted", snapshot.muted);
    check!(
        &format!(
            "position keeps advancing while muted ({}ms -> {}ms)",
            pos_before_mute, snapshot.position_ms
        ),
        snapshot.position_ms > pos_before_mute
    );
    let completed = command(&mut engine, &events, Command::SetMute { muted: false });
    check!(
        "set_mute(false) completes ok",
        completed.status == CommandStatus::Ok
    );

    // ================================================================
    // Step 3: Pause
    // ================================================================
    println!("\n--- Step 3: Pause ---");
    let completed = command(&mut engine, &events, Command::Pause);
    check!("pause completes ok", completed.status == CommandStatus::Ok);
    let paused = engine.snapshot();
    check!(
        &format!("state is paused (got {:?})", paused.state),
        paused.state == PlayerState::Paused
    );
    std::thread::sleep(Duration::from_millis(300));
    let after_wait = engine.snapshot();
    check!(
        &format!(
            "position frozen while paused ({}ms -> {}ms)",
            paused.position_ms, after_wait.position_ms
        ),
        after_wait.position_ms == paused.position_ms
    );

    // 看门狗回归：暂停比停滞预算（2s）更久不能被判成卡死
    std::thread::sleep(Duration::from_millis(2500));
    let mut watchdog_error = None;
    while let Ok(envelope) = events.try_recv() {
        if let Event::Error(error) = envelope.event {
            watchdog_error = Some(error.message);
        }
    }
    check!(
        &format!("long pause does not trip the stall watchdog (got {watchdog_error:?})"),
        watchdog_error.is_none()
    );
    check!(
        "state still paused after long pause",
        engine.snapshot().state == PlayerState::Paused
    );

    // ================================================================
    // Step 4: Seek to 2.0s (while paused)
    // ================================================================
    println!("\n--- Step 4: Seek to 2000ms ---");
    let request_id = engine
        .command(CommandEnvelope::new(Command::Seek {
            position_ms: 2000,
            mode: SeekMode::Exact,
        }))
        .expect("seek must be accepted");
    let mut seek_completed: Option<SeekCompleted> = None;
    let completed = wait_completion(&events, request_id, |event| {
        if let Event::SeekCompleted(seek) = event {
            seek_completed = Some(seek.clone());
        }
    });
    check!(
        &format!("seek completes ok ({:?})", completed.status),
        completed.status == CommandStatus::Ok
    );
    match seek_completed {
        Some(seek) => {
            check!(
                &format!("seek_completed reports 2000ms (got {}ms)", seek.actual_ms),
                (seek.actual_ms as i64 - 2000).abs() < 100
            );
            check!(
                "seek_completed echoes request_id",
                seek.request_id == request_id
            );
        }
        None => {
            println!("  [FAIL] seek_completed not emitted");
            failed += 1;
        }
    }
    check!(
        &format!(
            "snapshot position is 2000ms (got {}ms)",
            engine.snapshot().position_ms
        ),
        (engine.snapshot().position_ms as i64 - 2000).abs() < 100
    );

    // ================================================================
    // Step 5: Resume play from seek position
    // ================================================================
    println!("\n--- Step 5: Resume Play ---");
    let completed = command(&mut engine, &events, Command::Play);
    check!(
        "play (resume) completes ok",
        completed.status == CommandStatus::Ok
    );
    std::thread::sleep(Duration::from_millis(800));
    let snapshot = engine.snapshot();
    check!(
        &format!("state is playing after resume (got {:?})", snapshot.state),
        snapshot.state == PlayerState::Playing
    );

    // exact seek 丢弃目标之前的 pre-roll：恢复后从目标处继续，不应跳回关键帧
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut crossed = false;
    while Instant::now() < deadline {
        if engine.snapshot().position_ms >= 2000 {
            crossed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    check!(
        &format!(
            "playback resumes at the seek target (got {}ms)",
            engine.snapshot().position_ms
        ),
        crossed
    );

    // 回归：seek 之前在途的 packet/frame 已被 generation 拦下，位置不得回跳
    let mut positions = vec![engine.snapshot().position_ms];
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(50));
        positions.push(engine.snapshot().position_ms);
    }
    let max_regress = positions
        .windows(2)
        .map(|window| window[0].saturating_sub(window[1]))
        .max()
        .unwrap_or(0);
    check!(
        &format!("position never regresses after seek (worst drop {max_regress}ms)"),
        max_regress < 100
    );
    check!(
        &format!(
            "position keeps advancing after seek ({}ms -> {}ms)",
            positions[0],
            positions[positions.len() - 1]
        ),
        positions[positions.len() - 1] >= positions[0]
    );

    // ================================================================
    // Step 5.5: Keyframe seek（落在目标前最近的关键帧，不裁剪 pre-roll）
    // ================================================================
    println!("\n--- Step 5.5: Keyframe Seek to 3500ms ---");
    let completed = command(&mut engine, &events, Command::Pause);
    check!(
        "pause before keyframe seek completes ok",
        completed.status == CommandStatus::Ok
    );

    let request_id = engine
        .command(CommandEnvelope::new(Command::Seek {
            position_ms: 3500,
            mode: SeekMode::Keyframe,
        }))
        .expect("keyframe seek must be accepted");
    let mut keyframe_seek: Option<SeekCompleted> = None;
    let completed = wait_completion(&events, request_id, |event| {
        if let Event::SeekCompleted(seek) = event {
            keyframe_seek = Some(seek.clone());
        }
    });
    check!(
        &format!("keyframe seek completes ok ({:?})", completed.status),
        completed.status == CommandStatus::Ok
    );
    match keyframe_seek {
        Some(seek) => {
            check!(
                &format!("keyframe seek echoes its mode ({:?})", seek.mode),
                seek.mode == SeekMode::Keyframe
            );
            check!(
                &format!(
                    "keyframe seek lands at or before the target ({}ms <= 3500ms)",
                    seek.actual_ms
                ),
                seek.actual_ms <= 3500
            );
            check!(
                &format!(
                    "keyframe seek stays within one GOP ({}ms before target)",
                    3500u64.saturating_sub(seek.actual_ms)
                ),
                3500u64.saturating_sub(seek.actual_ms) <= 1000
            );
            check!(
                &format!(
                    "snapshot position equals the keyframe ({}ms vs {}ms)",
                    engine.snapshot().position_ms,
                    seek.actual_ms
                ),
                engine.snapshot().position_ms == seek.actual_ms
            );
        }
        None => {
            println!("  [FAIL] keyframe seek_completed not emitted");
            failed += 1;
        }
    }

    let completed = command(&mut engine, &events, Command::Play);
    check!(
        "play after keyframe seek completes ok",
        completed.status == CommandStatus::Ok
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut crossed = false;
    while Instant::now() < deadline {
        if engine.snapshot().position_ms >= 3500 {
            crossed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    check!(
        &format!(
            "playback crosses the keyframe seek target (got {}ms)",
            engine.snapshot().position_ms
        ),
        crossed
    );

    // ================================================================
    // Step 6: 自然播放到末尾
    // ================================================================
    println!("\n--- Step 6: Play to EOF ---");
    match wait_event(&events, |event| {
        matches!(
            event,
            Event::StateChanged(changed)
                if changed.to == PlayerState::Ended
                    && changed.reason == StateChangeReason::Eof
        )
    }) {
        Event::StateChanged(changed) => {
            check!(
                &format!(
                    "state_changed ends playback with eof (from {:?})",
                    changed.from
                ),
                changed.to == PlayerState::Ended && changed.reason == StateChangeReason::Eof
            );
        }
        _ => unreachable!(),
    }
    let position = engine.snapshot().position_ms;
    check!(
        &format!("EOF fired near the real end (position {position}ms)"),
        position > 4500
    );
    check!(
        &format!(
            "position equals duration at EOF ({}ms)",
            engine.snapshot().duration_ms
        ),
        position == engine.snapshot().duration_ms
    );
    check!(
        "snapshot state is ended",
        engine.snapshot().state == PlayerState::Ended
    );

    // ================================================================
    // Step 7: Stop from Ended
    // ================================================================
    println!("\n--- Step 7: Stop ---");
    let completed = command(&mut engine, &events, Command::Stop);
    check!(
        &format!("stop completes ok ({:?})", completed.status),
        completed.status == CommandStatus::Ok
    );
    let snapshot = engine.snapshot();
    check!(
        &format!("state is stopped (got {:?})", snapshot.state),
        snapshot.state == PlayerState::Stopped
    );
    check!(
        &format!("position reset to 0 (got {}ms)", snapshot.position_ms),
        snapshot.position_ms == 0
    );

    // ================================================================
    // Step 8: Clean shutdown
    // ================================================================
    println!("\n--- Step 8: Clean Shutdown ---");
    engine.destroy();
    check!("engine destroyed without panic", true);

    // ================================================================
    // Summary
    // ================================================================
    println!("\n{}", "=".repeat(40));
    println!("=== Results: {passed} passed, {failed} failed ===");
    if failed > 0 {
        println!("E2E VERIFICATION FAILED");
        std::process::exit(1);
    } else {
        println!("E2E VERIFICATION PASSED");
    }
}
