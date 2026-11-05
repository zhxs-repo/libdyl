//! 长播稳定性（soak）— 内存/线程/事件账目随时间的走势
//!
//! 连续播放（短素材到 EOF 后自动重播）+ 周期 seek，定时采样 RSS 与线程数。
//! 判定：无异步 error、受理命令全部 ok、RSS 与线程数不随播放时长单调增长。
//!
//! 运行:
//!   `cargo run --release --example soak -p dyl-ffi -- test_video.mp4 120`
//!   `cargo run --release --example soak -p dyl-ffi -- long_2h.mp4 7200`
//!
//! 2 小时素材用 `scripts/make_soak_asset.sh` 生成。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandEnvelope, CommandStatus, ErrorCode, Event, EventEnvelope, PlayerState, SeekMode,
};
use dyl_engine::Engine;

const SAMPLE_PERIOD: Duration = Duration::from_secs(10);
const SEEK_PERIOD: Duration = Duration::from_secs(15);
const PER_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

struct ProcessSample {
    rss_bytes: Option<u64>,
    threads: Option<u32>,
}

#[cfg(windows)]
fn sample_process() -> ProcessSample {
    mod sys {
        use std::ffi::c_void;

        #[repr(C)]
        #[derive(Default)]
        pub struct ProcessMemoryCounters {
            pub cb: u32,
            pub page_fault_count: u32,
            pub peak_working_set_size: usize,
            pub working_set_size: usize,
            pub quota_peak_paged_pool_usage: usize,
            pub quota_paged_pool_usage: usize,
            pub quota_peak_non_paged_pool_usage: usize,
            pub quota_non_paged_pool_usage: usize,
            pub pagefile_usage: usize,
            pub peak_pagefile_usage: usize,
        }

        /// `THREADENTRY32`
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        pub struct ThreadEntry32 {
            pub dw_size: u32,
            pub cnt_usage: u32,
            pub th32_thread_id: u32,
            pub th32_owner_process_id: u32,
            pub tp_base_pri: i32,
            pub tp_priority_delta: i32,
            pub dw_flags: u32,
        }

        pub const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
        pub const INVALID_HANDLE_VALUE: isize = -1;

        #[link(name = "kernel32")]
        extern "system" {
            pub fn GetCurrentProcess() -> *mut c_void;
            pub fn GetCurrentProcessId() -> u32;
            pub fn K32GetProcessMemoryInfo(
                process: *mut c_void,
                counters: *mut ProcessMemoryCounters,
                cb: u32,
            ) -> i32;
            pub fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> isize;
            pub fn Thread32First(snapshot: isize, entry: *mut ThreadEntry32) -> i32;
            pub fn Thread32Next(snapshot: isize, entry: *mut ThreadEntry32) -> i32;
            pub fn CloseHandle(handle: isize) -> i32;
        }
    }

    let rss_bytes = unsafe {
        let mut counters = sys::ProcessMemoryCounters {
            cb: std::mem::size_of::<sys::ProcessMemoryCounters>() as u32,
            ..Default::default()
        };
        if sys::K32GetProcessMemoryInfo(sys::GetCurrentProcess(), &mut counters, counters.cb) != 0 {
            Some(counters.working_set_size as u64)
        } else {
            None
        }
    };

    let threads = unsafe {
        let snapshot = sys::CreateToolhelp32Snapshot(sys::TH32CS_SNAPTHREAD, 0);
        if snapshot == sys::INVALID_HANDLE_VALUE || snapshot == 0 {
            None
        } else {
            let pid = sys::GetCurrentProcessId();
            let mut entry = sys::ThreadEntry32 {
                dw_size: std::mem::size_of::<sys::ThreadEntry32>() as u32,
                ..Default::default()
            };
            let mut count = 0u32;
            let mut ok = sys::Thread32First(snapshot, &mut entry) != 0;
            while ok {
                if entry.th32_owner_process_id == pid {
                    count += 1;
                }
                entry.dw_size = std::mem::size_of::<sys::ThreadEntry32>() as u32;
                ok = sys::Thread32Next(snapshot, &mut entry) != 0;
            }
            sys::CloseHandle(snapshot);
            Some(count)
        }
    };

    ProcessSample { rss_bytes, threads }
}

#[cfg(target_os = "linux")]
fn sample_process() -> ProcessSample {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next()? != name {
                return None;
            }
            parts.next()?.parse::<u64>().ok()
        })
    };
    ProcessSample {
        rss_bytes: field("VmRSS:").map(|kb| kb * 1024),
        threads: field("Threads:").map(|value| value as u32),
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn sample_process() -> ProcessSample {
    // macOS 需要 task_info 才能拿到 RSS；本机验收平台不含 macOS，如实报告不支持
    ProcessSample {
        rss_bytes: None,
        threads: None,
    }
}

fn submit(engine: &Engine, command: Command) -> u64 {
    let deadline = Instant::now() + PER_COMMAND_TIMEOUT;
    loop {
        match engine.command(CommandEnvelope::new(command.clone())) {
            Ok(request_id) => return request_id,
            Err(err) if err.code == ErrorCode::Busy => {
                assert!(
                    Instant::now() < deadline,
                    "command queue stayed full for {PER_COMMAND_TIMEOUT:?}"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(err) => panic!(
                "command rejected synchronously: {:?}: {}",
                err.code, err.message
            ),
        }
    }
}

fn mib(bytes: Option<u64>) -> String {
    match bytes {
        Some(bytes) => format!("{:.1}", bytes as f64 / (1024.0 * 1024.0)),
        None => "n/a".into(),
    }
}

#[derive(Default)]
struct EventTotals {
    position_changed: u64,
    completions: HashMap<u64, CommandStatus>,
    seek_completed: u64,
    errors: Vec<String>,
    warnings: Vec<String>,
}

fn drain(
    events: &Receiver<EventEnvelope>,
    totals: &mut EventTotals,
    pending: &mut HashMap<u64, &'static str>,
) {
    while let Ok(envelope) = events.try_recv() {
        match envelope.event {
            Event::CommandCompleted(completed) => {
                if let Some(op) = pending.remove(&completed.request_id) {
                    if completed.status != CommandStatus::Ok {
                        totals.errors.push(format!(
                            "{op} #{} completed with {:?}",
                            completed.request_id, completed.status
                        ));
                    }
                }
                totals
                    .completions
                    .insert(completed.request_id, completed.status);
            }
            Event::SeekCompleted(_) => totals.seek_completed += 1,
            Event::PositionChanged(_) => totals.position_changed += 1,
            Event::Error(error) => {
                totals
                    .errors
                    .push(format!("{}: {}", error.code.as_str(), error.message))
            }
            Event::Warning(warning) => totals.warnings.push(warning.message),
            _ => {}
        }
    }
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
    let seconds: u64 = args
        .get(2)
        .and_then(|value| value.parse().ok())
        .or_else(|| {
            std::env::var("DYL_SOAK_SECONDS")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(120);
    let duration = Duration::from_secs(seconds);

    println!("=== Dyl Soak ({seconds}s) ===");
    println!("Video: {video_path}\n");

    let mut failed = 0u32;
    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                println!("  [PASS] {}", $name);
            } else {
                println!("  [FAIL] {}", $name);
                failed += 1;
            }
        };
    }

    let (mut engine, events) = Engine::new().expect("engine must start");
    let mut totals = EventTotals::default();
    let mut pending: HashMap<u64, &'static str> = HashMap::new();

    let load_id = submit(
        &engine,
        Command::LoadFile {
            url: video_path.clone(),
        },
    );
    pending.insert(load_id, "load_file");

    // 加载是异步的：Play 必须等 file_loaded 落地，否则会在 loading 态被拒
    let load_deadline = Instant::now() + Duration::from_secs(30);
    while !totals.completions.contains_key(&load_id) {
        drain(&events, &mut totals, &mut pending);
        assert!(Instant::now() < load_deadline, "load_file timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    if totals.completions[&load_id] != CommandStatus::Ok {
        panic!("load_file failed: {:?}", totals.errors);
    }

    let play_id = submit(&engine, Command::Play);
    pending.insert(play_id, "play");

    let started = Instant::now();
    let deadline = started + duration;
    // 预热完成后再取基线，避免把首次分配算成泄漏
    let warmup =
        Duration::from_secs((seconds / 5).clamp(5, 30)).min(Duration::from_secs(seconds.max(1)));
    let mut baseline: Option<ProcessSample> = None;
    let mut samples: Vec<(u64, ProcessSample)> = Vec::new();
    let mut next_sample = started + Duration::from_secs(2);
    let mut next_seek = started + SEEK_PERIOD;
    let mut seeks_sent = 0u64;
    let mut restarts = 0u64;
    let mut restart_id: Option<u64> = None;
    let mut seek_pending: Option<u64> = None;

    println!("  time    rss(MiB)  threads  state      seeks  restarts  pos(ms)");
    while Instant::now() < deadline {
        drain(&events, &mut totals, &mut pending);

        // 短素材会自然播到 EOF；长播验收里由 Play 从头重播，等价于循环播放
        if engine.snapshot().state == PlayerState::Ended && restart_id.is_none() {
            let id = submit(&engine, Command::Play);
            pending.insert(id, "play(restart)");
            restart_id = Some(id);
            restarts += 1;
        }

        // 上一次 seek 已出结果才发下一次，避免自己造 seek 风暴
        if seek_pending.is_none() && Instant::now() >= next_seek {
            let duration_ms = engine.snapshot().duration_ms.max(1);
            let target =
                (started.elapsed().as_millis() as u64 % 7) * duration_ms / 10 + duration_ms / 10;
            let target = target.min(duration_ms.saturating_sub(1));
            let id = submit(
                &engine,
                Command::Seek {
                    position_ms: target,
                    mode: SeekMode::Exact,
                },
            );
            pending.insert(id, "seek");
            seek_pending = Some(id);
            seeks_sent += 1;
            next_seek = Instant::now() + SEEK_PERIOD;
        }

        if seek_pending.is_some_and(|id| totals.completions.contains_key(&id)) {
            seek_pending = None;
        }
        if restart_id.is_some_and(|id| totals.completions.contains_key(&id)) {
            restart_id = None;
        }

        if Instant::now() >= next_sample {
            let elapsed = started.elapsed().as_secs();
            let sample = sample_process();
            let snapshot = engine.snapshot();
            println!(
                "  {:>4}s  {:>8}  {:>7}  {:<9}  {:>5}  {:>8}  {:>7}",
                elapsed,
                mib(sample.rss_bytes),
                sample
                    .threads
                    .map(|threads| threads.to_string())
                    .unwrap_or_else(|| "n/a".into()),
                format!("{:?}", snapshot.state),
                seeks_sent,
                restarts,
                snapshot.position_ms,
            );
            if baseline.is_none() && started.elapsed() >= warmup {
                baseline = Some(ProcessSample {
                    rss_bytes: sample.rss_bytes,
                    threads: sample.threads,
                });
            }
            samples.push((elapsed, sample));
            next_sample = Instant::now() + SAMPLE_PERIOD;
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    // 收尾：等所有在途命令完成，再静默 300ms 以捕获迟到的 error / 重复完成
    let settle_deadline = Instant::now() + Duration::from_secs(10);
    let mut quiet_since: Option<Instant> = None;
    loop {
        drain(&events, &mut totals, &mut pending);
        if pending.is_empty() {
            let quiet = quiet_since.get_or_insert_with(Instant::now);
            if quiet.elapsed() >= Duration::from_millis(300) {
                break;
            }
        } else {
            quiet_since = None;
        }
        if Instant::now() >= settle_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let snapshot = engine.snapshot();
    let last = samples.last().map(|(_, sample)| sample);
    let elapsed_secs = started.elapsed().as_secs_f64();

    println!("\n--- Summary ---");
    println!(
        "  elapsed {:.0}s, {} seeks, {} EOF restarts, {} completions, {} seek_completed, {} position_changed",
        elapsed_secs,
        seeks_sent,
        restarts,
        totals.completions.len(),
        totals.seek_completed,
        totals.position_changed
    );
    println!(
        "  final state: {:?} at {}ms / {}ms",
        snapshot.state, snapshot.position_ms, snapshot.duration_ms
    );

    println!("\n--- Checks ---");
    check!(
        &format!("no asynchronous errors ({:?})", totals.errors),
        totals.errors.is_empty()
    );
    if !totals.warnings.is_empty() {
        println!(
            "  info: {} warnings (last: {:?})",
            totals.warnings.len(),
            totals.warnings.last()
        );
    }
    check!(
        &format!(
            "no accepted command left uncompleted ({} pending)",
            pending.len()
        ),
        pending.is_empty()
    );
    let non_ok: Vec<_> = totals
        .completions
        .iter()
        .filter(|(_, status)| **status != CommandStatus::Ok)
        .collect();
    check!(
        &format!("every command completed ok (non-ok: {non_ok:?})"),
        non_ok.is_empty()
    );
    check!(
        &format!("engine is still healthy (state {:?})", snapshot.state),
        !matches!(
            snapshot.state,
            PlayerState::Idle | PlayerState::Loading | PlayerState::Error
        )
    );
    check!(
        &format!(
            "position events keep flowing ({} over {:.0}s)",
            totals.position_changed, elapsed_secs
        ),
        totals.position_changed as f64 >= elapsed_secs * 5.0
    );
    if seeks_sent > 0 {
        check!(
            &format!(
                "every seek produced seek_completed ({} of {seeks_sent})",
                totals.seek_completed
            ),
            totals.seek_completed == seeks_sent
        );
    }

    let baseline_sample = baseline.expect("baseline sample must exist");
    match (
        baseline_sample.rss_bytes,
        last.and_then(|sample| sample.rss_bytes),
    ) {
        (Some(base), Some(end)) => {
            let growth = end.saturating_sub(base);
            let allowed = (base / 5).max(64 * 1024 * 1024);
            println!(
                "  info: rss {}MiB -> {}MiB (growth {:.1}MiB, allowed {:.1}MiB)",
                mib(Some(base)),
                mib(Some(end)),
                growth as f64 / (1024.0 * 1024.0),
                allowed as f64 / (1024.0 * 1024.0),
            );
            check!(
                &format!(
                    "rss growth {:.1}MiB stays bounded",
                    growth as f64 / (1024.0 * 1024.0)
                ),
                growth <= allowed
            );
        }
        _ => println!("  [SKIP] rss growth (not measurable on this platform)"),
    }

    match (
        baseline_sample.threads,
        last.and_then(|sample| sample.threads),
    ) {
        (Some(base), Some(end)) => {
            let peak = samples
                .iter()
                .filter_map(|(_, sample)| sample.threads)
                .max()
                .unwrap_or(end);
            println!("  info: threads {base} -> {end} (peak {peak})");
            check!(
                &format!("no thread growth (baseline {base}, peak {peak})"),
                peak <= base + 6
            );
        }
        _ => println!("  [SKIP] thread count (not measurable on this platform)"),
    }

    engine.destroy();
    check!("engine destroyed without panic", true);

    println!("\n{}", "=".repeat(44));
    if failed > 0 {
        println!("=== SOAK FAILED: {failed} check(s) ===");
        std::process::exit(1);
    }
    println!("=== SOAK PASSED ===");
}
