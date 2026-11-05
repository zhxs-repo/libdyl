//! 回归：拖动进度条式的 seek 风暴 — 合并语义与请求记账
//!
//! 连续压入大量 `seek`（默认 1000 次），验证:
//!   - 每个被受理的 request_id 恰好一个 `command_completed`
//!   - 状态只出现 `ok` / `superseded`，且 ok 的请求各有一个 `seek_completed`
//!   - 最后一个请求必然是 ok（不会被后续请求合并掉）
//!   - 播放中：位置收敛到最后一个 actual_ms 且不回跳
//!   - 暂停中：位置精确等于最后一个 actual_ms 且冻结
//!
//! 运行: `cargo run --release --example seek_storm -p dyl-ffi -- test_video.mp4 [count]`

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandEnvelope, CommandStatus, ErrorCode, Event, EventEnvelope, PlayerState, SeekMode,
};
use dyl_engine::Engine;

const DRAIN_IDLE: Duration = Duration::from_millis(300);
const COLLECT_TIMEOUT: Duration = Duration::from_secs(60);
const PER_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// 固定种子的 LCG：失败可复现，不用引入 rand
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[derive(Default)]
struct StormStats {
    completions: HashMap<u64, usize>,
    statuses: HashMap<u64, CommandStatus>,
    seek_completed: HashMap<u64, usize>,
    last_seek_actual: Option<(u64, u64)>,
    seek_events: usize,
    seq_monotonic: bool,
    errors: Vec<String>,
}

fn submit_seek(engine: &Engine, position_ms: u64) -> u64 {
    let deadline = Instant::now() + PER_COMMAND_TIMEOUT;
    loop {
        match engine.command(CommandEnvelope::new(Command::Seek {
            position_ms,
            mode: SeekMode::Exact,
        })) {
            Ok(request_id) => return request_id,
            // 队列只有 64 格：风暴期间 `try_send` 满了就退避重试，
            // 这本身就是验收项的一部分（Busy 不能变成丢命令）。
            Err(err) if err.code == ErrorCode::Busy => {
                assert!(
                    Instant::now() < deadline,
                    "command queue stayed full for {PER_COMMAND_TIMEOUT:?}"
                );
                std::thread::sleep(Duration::from_micros(200));
            }
            Err(err) => panic!(
                "seek rejected synchronously: {:?}: {}",
                err.code, err.message
            ),
        }
    }
}

fn wait_completion(events: &Receiver<EventEnvelope>, request_id: u64) -> CommandStatus {
    let deadline = Instant::now() + PER_COMMAND_TIMEOUT;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for command_completed");
        let envelope = events
            .recv_timeout(left)
            .expect("event channel closed before command_completed");
        if let Event::CommandCompleted(completed) = envelope.event {
            if completed.request_id == request_id {
                return completed.status;
            }
        }
    }
}

/// 收齐所有受理请求的完成事件；全部到齐后再静默 `DRAIN_IDLE` 以捕获重复完成。
fn collect(
    events: &Receiver<EventEnvelope>,
    accepted: &HashSet<u64>,
    mut seq: u64,
) -> (StormStats, u64) {
    let mut stats = StormStats {
        seq_monotonic: true,
        ..Default::default()
    };
    let deadline = Instant::now() + COLLECT_TIMEOUT;
    let mut quiet_since: Option<Instant> = None;

    loop {
        if accepted.iter().all(|id| stats.completions.contains_key(id)) {
            let quiet = quiet_since.get_or_insert_with(Instant::now);
            if quiet.elapsed() >= DRAIN_IDLE {
                break;
            }
        } else {
            quiet_since = None;
        }
        if Instant::now() >= deadline {
            let missing: Vec<_> = accepted
                .iter()
                .filter(|id| !stats.completions.contains_key(id))
                .collect();
            stats.errors.push(format!(
                "timed out with {} requests uncompleted: {missing:?}",
                missing.len()
            ));
            break;
        }

        let Ok(envelope) = events.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        if envelope.seq <= seq {
            stats.seq_monotonic = false;
        }
        seq = envelope.seq;

        match envelope.event {
            Event::CommandCompleted(completed) => {
                *stats.completions.entry(completed.request_id).or_insert(0) += 1;
                stats
                    .statuses
                    .insert(completed.request_id, completed.status);
                if completed.status == CommandStatus::Error {
                    stats.errors.push(format!(
                        "request {} failed: {:?}",
                        completed.request_id, completed.error
                    ));
                }
            }
            Event::SeekCompleted(seek) => {
                stats.seek_events += 1;
                *stats.seek_completed.entry(seek.request_id).or_insert(0) += 1;
                stats.last_seek_actual = Some((seek.request_id, seek.actual_ms));
            }
            Event::Error(error) => stats.errors.push(format!(
                "async error {}: {}",
                error.code.as_str(),
                error.message
            )),
            _ => {}
        }
    }

    (stats, seq)
}

fn main() {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "dyl=warn");
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = std::env::args().collect();
    let video_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "test_video.mp4".into());
    let count: usize = args
        .get(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1000);

    println!("=== Dyl Seek Storm ({count} seeks, headless) ===");
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

    // ── 加载 ────────────────────────────────────────────────────────
    println!("\n--- Load ---");
    let load_id = engine
        .command(CommandEnvelope::new(Command::LoadFile {
            url: video_path.clone(),
        }))
        .expect("load_file must be accepted");
    let load_status = wait_completion(&events, load_id);
    if load_status != CommandStatus::Ok {
        eprintln!("FATAL: load_file failed ({load_status:?})");
        std::process::exit(1);
    }
    let duration_ms = engine.snapshot().duration_ms;
    check!(
        &format!("file loaded with duration {duration_ms}ms"),
        duration_ms > 0
    );

    let play_id = engine
        .command(CommandEnvelope::new(Command::Play))
        .expect("play must be accepted");
    assert_eq!(wait_completion(&events, play_id), CommandStatus::Ok);
    std::thread::sleep(Duration::from_millis(400));
    check!(
        "playback is running before the storm",
        engine.snapshot().state == PlayerState::Playing
    );

    // ── 风暴 1：播放中 ──────────────────────────────────────────────
    println!("\n--- Phase 1: {count} seeks while playing ---");
    let mut rng = Lcg(0x5EED_5EED);
    // 只在片头 60% 内跳，避免风暴本身把播放带到 EOF
    let storm_range = (duration_ms * 3 / 5).max(1);
    let mut accepted: HashSet<u64> = HashSet::new();
    let mut order: Vec<u64> = Vec::with_capacity(count);
    let storm_start = Instant::now();
    for _ in 0..count {
        let id = submit_seek(&engine, rng.next() % storm_range);
        accepted.insert(id);
        order.push(id);
    }
    let submit_elapsed = storm_start.elapsed();
    check!(
        &format!(
            "all {count} seeks accepted in {:.2}s ({} unique ids)",
            submit_elapsed.as_secs_f64(),
            accepted.len()
        ),
        accepted.len() == count
    );

    let (stats, seq_after) = collect(&events, &accepted, 0);
    check!("event seq is strictly increasing", stats.seq_monotonic);
    check!(
        &format!("no errors during storm ({:?})", stats.errors),
        stats.errors.is_empty()
    );

    let exactly_once = stats.completions.values().all(|count| *count == 1)
        && stats.completions.len() == accepted.len();
    check!(
        &format!(
            "every accepted request completed exactly once ({}/{} ids)",
            stats.completions.len(),
            accepted.len()
        ),
        exactly_once
    );

    let unexpected: Vec<_> = stats
        .statuses
        .iter()
        .filter(|(_, status)| !matches!(status, CommandStatus::Ok | CommandStatus::Superseded))
        .map(|(id, status)| format!("{id}: {status:?}"))
        .collect();
    check!(
        &format!("statuses are only ok/superseded (unexpected: {unexpected:?})"),
        unexpected.is_empty()
    );

    let superseded = stats
        .statuses
        .values()
        .filter(|status| **status == CommandStatus::Superseded)
        .count();
    let ok_count = stats
        .statuses
        .values()
        .filter(|status| **status == CommandStatus::Ok)
        .count();
    println!(
        "  info: {ok_count} ok, {superseded} superseded, {} seek_completed",
        stats.seek_events
    );
    check!(
        &format!(
            "exactly one seek_completed per ok request ({ok_count} ok / {} events)",
            stats.seek_events
        ),
        stats.seek_events == ok_count
    );
    let stray_seek_events: Vec<_> = stats
        .seek_completed
        .iter()
        .filter(|(id, _)| stats.statuses.get(*id) != Some(&CommandStatus::Ok))
        .map(|(id, _)| *id)
        .collect();
    check!(
        &format!("no seek_completed for superseded requests ({stray_seek_events:?})"),
        stray_seek_events.is_empty()
    );

    let last_id = *order.last().expect("non-empty storm");
    check!(
        &format!(
            "last request is ok (got {:?})",
            stats.statuses.get(&last_id)
        ),
        stats.statuses.get(&last_id) == Some(&CommandStatus::Ok)
    );

    let (last_actual_id, last_actual_ms) = stats
        .last_seek_actual
        .expect("at least one seek must complete");
    check!(
        &format!("final seek_completed belongs to the last request ({last_actual_id})"),
        last_actual_id == last_id
    );
    check!(
        &format!("final actual_ms {last_actual_ms} within duration {duration_ms}"),
        last_actual_ms <= duration_ms
    );

    // 播放中 seek 完成后时钟继续走：位置应 >= 落点且在合理窗口内
    std::thread::sleep(Duration::from_millis(200));
    let snapshot = engine.snapshot();
    let drift = snapshot.position_ms.saturating_sub(last_actual_ms);
    check!(
        &format!(
            "position converges to the final seek ({}ms vs {}ms, drift {}ms)",
            snapshot.position_ms, last_actual_ms, drift
        ),
        snapshot.position_ms >= last_actual_ms && drift < 1500
    );
    check!(
        "state is still playing after the storm",
        snapshot.state == PlayerState::Playing
    );

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
        &format!("position never regresses after the storm (worst drop {max_regress}ms)"),
        max_regress < 100
    );
    check!(
        &format!(
            "position keeps advancing ({}ms -> {}ms)",
            positions[0],
            positions[positions.len() - 1]
        ),
        positions[positions.len() - 1] > positions[0]
    );

    // ── 暂停 + 风暴 2：落点必须精确收敛并冻结 ──────────────────────
    println!("\n--- Phase 2: seek storm while paused ---");
    let pause_id = engine
        .command(CommandEnvelope::new(Command::Pause))
        .expect("pause must be accepted");
    assert_eq!(wait_completion(&events, pause_id), CommandStatus::Ok);
    check!(
        "state is paused",
        engine.snapshot().state == PlayerState::Paused
    );

    let paused_count = (count / 4).max(50);
    let mut paused_accepted: HashSet<u64> = HashSet::new();
    let mut paused_order: Vec<u64> = Vec::with_capacity(paused_count);
    for _ in 0..paused_count {
        let id = submit_seek(&engine, rng.next() % storm_range);
        paused_accepted.insert(id);
        paused_order.push(id);
    }
    let (paused_stats, _) = collect(&events, &paused_accepted, seq_after);
    check!(
        &format!("paused storm has no errors ({:?})", paused_stats.errors),
        paused_stats.errors.is_empty()
    );
    check!(
        "paused storm: every request completed exactly once",
        paused_stats.completions.len() == paused_accepted.len()
            && paused_stats.completions.values().all(|count| *count == 1)
    );
    let paused_last = *paused_order.last().expect("non-empty storm");
    let (paused_actual_id, paused_actual_ms) = paused_stats
        .last_seek_actual
        .expect("paused seek must complete");
    check!(
        "paused storm: final request ok and final seek_completed matches",
        paused_stats.statuses.get(&paused_last) == Some(&CommandStatus::Ok)
            && paused_actual_id == paused_last
    );

    let frozen = engine.snapshot();
    check!(
        &format!(
            "paused position equals the final actual_ms ({}ms vs {}ms)",
            frozen.position_ms, paused_actual_ms
        ),
        frozen.position_ms == paused_actual_ms
    );
    std::thread::sleep(Duration::from_millis(400));
    check!(
        &format!(
            "paused position stays frozen ({}ms)",
            engine.snapshot().position_ms
        ),
        engine.snapshot().position_ms == paused_actual_ms
    );

    // 风暴后仍可正常继续播放
    let resume_id = engine
        .command(CommandEnvelope::new(Command::Play))
        .expect("play must be accepted");
    assert_eq!(wait_completion(&events, resume_id), CommandStatus::Ok);
    std::thread::sleep(Duration::from_millis(500));
    let resumed = engine.snapshot();
    check!(
        &format!(
            "playback advances after the storm ({}ms -> {}ms)",
            paused_actual_ms, resumed.position_ms
        ),
        resumed.state == PlayerState::Playing && resumed.position_ms > paused_actual_ms
    );

    // ── 收尾 ────────────────────────────────────────────────────────
    engine.destroy();
    check!("engine destroyed without panic", true);

    println!("\n{}", "=".repeat(44));
    println!("=== Results: {passed} passed, {failed} failed ===");
    if failed > 0 {
        println!("SEEK STORM FAILED");
        std::process::exit(1);
    }
    println!("SEEK STORM PASSED");
}
