//! dyl 侧性能/兼容性测量端：跑 make_compat_corpus.sh 的 manifest，产出可对账 JSON
//!
//! 运行: `cargo run --release --example bench_matrix -p dyl-ffi -- \
//!          tmp_verify/corpus/manifest.json tmp_verify/bench/dyl.json`
//!
//! 观测点是 `HostFrameSink::push_frame`，不是 `snapshot.stats.rendered_frames`：
//! host compositor 是内核"这一帧交给宿主呈现"的那一刻，无窗口也能跑到；而
//! `stats.rendered_frames` 由 shell 侧记账，无头跑永远是 0，拿它测首帧会全灭。
//!
//! 三个必须记住的口径：
//!   - 每个用例新建 Engine：泄漏与状态串味要在跨用例时被看见，而不是被复用掩盖。
//!   - seek 用 `exact`：语料是 ultrafast 编码，3 秒片子的关键帧基本只有第 0 帧，
//!     keyframe seek 会落到片头，测出来的"延迟"是假数字。
//!   - 本进程 RSS/CPU 就是内核开销的下界（含 example 自身），与 mpv 进程对照时
//!     只比增量，不比绝对值。
#![allow(clippy::too_many_arguments)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use dyl_core::contract::{
    Command, CommandEnvelope, CommandStatus, Event, EventEnvelope, SeekMode, TrackKind,
};
use dyl_engine::{Engine, HostFrameSink, VideoFrame};

const MANIFEST_DEFAULT: &str = "tmp_verify/corpus/manifest.json";
const OUT_DEFAULT: &str = "tmp_verify/bench/dyl.json";

fn started() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

fn mono_secs() -> f64 {
    started().elapsed().as_secs_f64()
}

/// 内核的 `tracing::warn!` 不经事件通道，`Event::Warning` 收不到它。
/// seek 发布顺序（T-20）这类缺陷的唯一症状就是一行 re-sync 告警，
/// 不把它收进 JSON，跑批就只能靠肉眼翻 stderr。
#[derive(Clone, Default)]
struct WarnLog {
    entries: Arc<Mutex<Vec<Value>>>,
}

impl WarnLog {
    fn len(&self) -> usize {
        self.entries.lock().len()
    }

    fn since(&self, from: usize) -> Vec<Value> {
        self.entries.lock()[from..].to_vec()
    }
}

fn warn_log() -> &'static WarnLog {
    static LOG: OnceLock<WarnLog> = OnceLock::new();
    LOG.get_or_init(WarnLog::default)
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnLog {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = *event.metadata().level();
        if level > tracing::Level::WARN {
            return;
        }
        let mut message = String::new();
        event.record(&mut MessageVisitor(&mut message));
        self.entries.lock().push(json!({
            "at": mono_secs(),
            "level": level.as_str().to_ascii_lowercase(),
            "target": event.metadata().target(),
            "message": message,
        }));
    }
}

struct MessageVisitor<'a>(&'a mut String);

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.push_str(&format!("{value:?}"));
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            self.0.push_str(&format!(" {}={value}", field.name()));
        }
    }
}

/// 呈现帧观测点：pts + 到达时刻，供首帧/seek 延迟与 AV 漂移使用
struct Sink {
    count: AtomicU64,
    marks: Mutex<Vec<(f64, f64)>>,
    cleared_at: Mutex<f64>,
}

impl Default for Sink {
    fn default() -> Self {
        Self {
            count: AtomicU64::new(0),
            marks: Mutex::new(Vec::new()),
            cleared_at: Mutex::new(0.0),
        }
    }
}

impl Sink {
    /// 丢弃此前记录，使"从这一刻起"的计数与延迟独立于上一阶段
    fn rebase(&self) {
        *self.marks.lock() = Vec::new();
        *self.cleared_at.lock() = mono_secs();
    }

    fn frames_since(&self, from_pts_ms: Option<f64>) -> Vec<(f64, f64)> {
        self.marks
            .lock()
            .iter()
            .filter(|(pts, _)| from_pts_ms.is_none_or(|p| *pts >= p - 60.0))
            .copied()
            .collect()
    }

    fn total(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

impl HostFrameSink for Sink {
    fn push_frame(&self, frame: VideoFrame) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if let Some(secs) = frame.pts_secs() {
            let mut marks = self.marks.lock();
            marks.push((secs * 1000.0, mono_secs()));
            if marks.len() > 60_000 {
                let cut = marks.len() - 30_000;
                marks.drain(..cut);
            }
        }
    }

    fn request_redraw(&self) {}
}

struct Args {
    manifest: PathBuf,
    out: PathBuf,
    hold: f64,
    decode: String,
    only: HashSet<String>,
    http_base: Option<String>,
    cases_limit: usize,
}

fn main() {
    std::env::set_var("RUST_BACKTRACE", "1");
    // 本 harness 自己就是被测量的负载：内核日志逐行写进 stdout 会把延迟数字抖散，
    // 只留 warn 以上，失败原因仍然可见（错误也同时进 JSON）
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "warn");
    }
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(warn_log().clone())
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("bench_matrix: {msg}");
            eprintln!(
                "用法: bench_matrix [manifest.json] [out.json] \
                 [--hold S] [--decode software|auto] [--only id,id] [--http-base URL] [--limit N]"
            );
            exit(2);
        }
    };

    let manifest_raw = std::fs::read_to_string(&args.manifest).unwrap_or_else(|e| {
        eprintln!("bench_matrix: 读不到 manifest {:?}: {e}", args.manifest);
        exit(2);
    });
    let manifest: Value = serde_json::from_str(&manifest_raw).expect("manifest 不是合法 JSON");
    let corpus_dir = args
        .manifest
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| {
            PathBuf::from(MANIFEST_DEFAULT)
                .parent()
                .unwrap()
                .to_path_buf()
        });

    let cases = manifest["cases"].as_array().cloned().unwrap_or_default();
    println!(
        "==> dyl bench_matrix: {} 个用例（hold {}s, decode {}）",
        cases.len(),
        args.hold,
        args.decode
    );

    let mut results = Vec::new();
    let mut run = 0usize;
    for case in &cases {
        let id = case["id"].as_str().unwrap_or("?").to_string();
        if !args.only.is_empty() && !args.only.contains(&id) {
            continue;
        }
        run += 1;
        if run > args.cases_limit {
            println!(
                "  (达到 --limit {}/{}，其余跳过)",
                args.cases_limit,
                cases.len()
            );
            break;
        }
        let row = run_case(&id, case, &corpus_dir, &args);
        println!(
            "  {:<22} {:<14} open={:>6} first={:>6} fps={:>5} seek={}",
            id,
            row["verdict"].as_str().unwrap_or("?"),
            ms_field(&row, "open_ms"),
            ms_field(&row, "first_frame_ms"),
            row["presented_fps"]
                .as_f64()
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "-".into()),
            row["seek"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|s| {
                            s["present_ms"]
                                .as_f64()
                                .map(|v| format!("{v:.0}"))
                                .unwrap_or_else(|| "-".into())
                        })
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default(),
        );
        results.push(row);
    }

    let out = json!({
        "side": "dyl",
        "generated_at": chrono_free_utc_now(),
        "kernel": {
            "schema": dyl_core::contract::SCHEMA_VERSION,
            "capabilities": serde_json::to_value(dyl_engine::capabilities::capabilities()).unwrap_or(Value::Null),
        },
        "hold_seconds": args.hold,
        "filter": {
            "only": args.only.iter().cloned().collect::<Vec<_>>(),
            "limit": if args.cases_limit == usize::MAX {
                Value::Null
            } else {
                Value::from(args.cases_limit as u64)
            },
        },
        "decode_mode": args.decode,
        "seek_mode": "exact",
        "cases": results,
    });
    if let Some(parent) = args.out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&args.out, serde_json::to_string_pretty(&out).unwrap()).unwrap_or_else(|e| {
        eprintln!("bench_matrix: 写不出 {:?}: {e}", args.out);
        exit(1);
    });
    println!("==> 写出 {} 条 -> {:?}", results.len(), args.out);
}

fn ms_field(row: &Value, key: &str) -> String {
    row[key]
        .as_f64()
        .map(|v| format!("{v:>6.0}ms"))
        .unwrap_or_else(|| "     -".into())
}

fn parse_args() -> Result<Args, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = argv.iter().filter(|a| !a.starts_with('-')).cloned();
    let mut args = Args {
        manifest: PathBuf::from(MANIFEST_DEFAULT),
        out: PathBuf::from(OUT_DEFAULT),
        hold: 1.2,
        decode: "software".into(),
        only: HashSet::new(),
        http_base: None,
        cases_limit: usize::MAX,
    };
    if let Some(m) = positional.next() {
        args.manifest = PathBuf::from(m);
    }
    if let Some(o) = positional.next() {
        args.out = PathBuf::from(o);
    }
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let value = argv.get(i + 1).cloned().unwrap_or_default();
        match flag {
            "--hold" => {
                args.hold = value.parse().map_err(|_| "--hold 需要秒数")?;
                i += 2;
            }
            "--decode" => {
                if value != "software" && value != "auto" {
                    return Err("--decode 只能是 software|auto".into());
                }
                args.decode = value;
                i += 2;
            }
            "--only" => {
                args.only = value.split(',').map(|s| s.trim().to_string()).collect();
                i += 2;
            }
            "--http-base" => {
                args.http_base = Some(value.trim_end_matches('/').to_string());
                i += 2;
            }
            "--limit" => {
                args.cases_limit = value.parse().map_err(|_| "--limit 需要整数")?;
                i += 2;
            }
            _ => i += 1,
        }
    }
    Ok(args)
}

/// 单个用例：新建 Engine → load → play → 稳态窗口 → 三点 seek →（可选）换轨
///
/// 外面包一层只为把本用例期间的 tracing 告警切出来：早退路径（素材缺失、load 失败）
/// 同样要带上告警，否则"失败时内核喊了什么"就丢了。
fn run_case(id: &str, case: &Value, corpus_dir: &Path, args: &Args) -> Value {
    let warn_from = warn_log().len();
    let mut row = run_case_inner(id, case, corpus_dir, args);
    row["tracing_warnings"] = json!(warn_log().since(warn_from));
    row
}

fn run_case_inner(id: &str, case: &Value, corpus_dir: &Path, args: &Args) -> Value {
    let expect = case["expect"].as_str().unwrap_or("supported");
    let source = match resolve_source(case, corpus_dir, args.http_base.as_deref()) {
        Some(s) => s,
        None => {
            return json!({
                "id": id, "expect": expect, "verdict": "skipped",
                "error": { "message": "素材缺失或没有可用的 http base" },
                "note": case["note"], "tags": case["tags"],
            })
        }
    };

    let before = sample_process();
    let sink = Arc::new(Sink::default());
    let (mut engine, events) = match Engine::new() {
        Ok(pair) => pair,
        Err(e) => {
            return json!({
                "id": id, "expect": expect, "verdict": "engine_failed",
                "error": { "message": e.to_string() }, "source": source,
                "note": case["note"], "tags": case["tags"],
            })
        }
    };
    engine.set_host_frame_sink(sink.clone());

    let mut col = Collected::default();
    sink.rebase();
    let load_mono = mono_secs();
    let opened = Instant::now();
    let load_id = match engine.command(CommandEnvelope::new(Command::LoadFile {
        url: source.clone(),
    })) {
        Ok(req) => req,
        Err(e) => {
            // 同步拒绝：契约层的 not_supported / invalid_state，同样是可测的结论
            let row = json!({
                "id": id, "expect": expect, "verdict": "rejected", "source": source,
                "open_ms": opened.elapsed().as_secs_f64() * 1000.0,
                "error": { "code": e.code.as_str(), "message": e.message },
                "note": case["note"], "tags": case["tags"],
            });
            engine.destroy();
            return with_resources(row, &before);
        }
    };

    let completed = match wait_completion(&events, load_id, Duration::from_secs(25), &mut col) {
        Some(c) => c,
        None => {
            let row = json!({
                "id": id, "expect": expect, "verdict": "load_timeout", "source": source,
                "open_ms": opened.elapsed().as_secs_f64() * 1000.0,
                "error": { "message": "loadfile 25s 未返回完成事件" },
                "errors": col.errors, "warnings": col.warnings,
                "note": case["note"], "tags": case["tags"],
            });
            engine.destroy();
            return with_resources(row, &before);
        }
    };
    let open_ms = opened.elapsed().as_secs_f64() * 1000.0;

    if completed.status != CommandStatus::Ok {
        // CommandCompleted.error 只有 code，message 走 snapshot.last_error / error 事件
        let snap = engine.snapshot();
        let message = snap
            .last_error
            .as_ref()
            .map(|e| e.message.clone())
            .or_else(|| col.errors.last().cloned())
            .unwrap_or_else(|| format!("status={:?}", completed.status));
        let row = json!({
            "id": id, "expect": expect, "verdict": "load_failed", "source": source,
            "open_ms": open_ms,
            "error": {
                "code": completed.error.as_ref().map(|e| e.code.as_str().to_string()),
                "message": message,
            },
            "errors": col.errors, "warnings": col.warnings,
            "note": case["note"], "tags": case["tags"],
        });
        engine.destroy();
        return with_resources(row, &before);
    }

    let snapshot = engine.snapshot();
    let media = serde_json::to_value(snapshot.media.clone()).unwrap_or(Value::Null);
    let duration_ms = snapshot.duration_ms;
    let has_video = snapshot
        .media
        .as_ref()
        .map(|m| m.video.is_some())
        .unwrap_or(false);

    // 解码方式在 load 之后设置：切换本身是重开管线，会影响首帧口径，
    // 所以只在 --decode auto 时执行，并把实际后端记下来
    let mut decode_backend = snapshot.decode.backend;
    if args.decode == "auto" {
        let req = engine
            .command(CommandEnvelope::new(Command::SetDecodeMode {
                mode: dyl_core::contract::DecodeMode::Auto,
            }))
            .ok();
        if let Some(req) = req {
            let _ = wait_completion(&events, req, Duration::from_secs(20), &mut col);
            decode_backend = engine.snapshot().decode.backend;
        }
    }

    // 外挂字幕 / 弹幕：命令成功不等于内容吃进去了。事件数、样式表、PlayRes 是引擎
    // 内部真相，直接读 overlay_handles，而不是只看 snapshot.loaded
    let mut overlays = serde_json::Map::new();
    for field in ["subtitle", "danmaku"] {
        let Some(rel) = case[field].as_str() else {
            continue;
        };
        let path = corpus_dir.join(rel).to_string_lossy().replace('\\', "/");
        let cmd = if field == "subtitle" {
            Command::LoadSubtitle { path }
        } else {
            Command::LoadDanmaku { path }
        };
        let t = Instant::now();
        let ok = match engine.command(CommandEnvelope::new(cmd)) {
            Ok(req) => wait_completion(&events, req, Duration::from_secs(10), &mut col)
                .map(|c| c.status == CommandStatus::Ok)
                .unwrap_or(false),
            Err(e) => {
                col.warnings
                    .push(format!("{field} rejected: {}", e.message));
                false
            }
        };
        overlays.insert(
            field.to_string(),
            json!({
                "command_ok": ok,
                "ms": (t.elapsed().as_secs_f64() * 1000.0).round() / 10.0,
                "loaded": match field {
                    "subtitle" => engine.snapshot().subtitle.loaded,
                    _ => engine.snapshot().danmaku.loaded,
                },
                "state": overlay_state(&engine, field),
            }),
        );
    }

    // 播放与首帧。首帧可能在 Play 之前就到（load 完成即呈现 poster），
    // 所以同时记"相对 load"和"相对 Play"两个基准，并把 poster 情况单独标出来
    let play_mono = mono_secs();
    let play_at = Instant::now();
    let play_id = engine.command(CommandEnvelope::new(Command::Play));
    let mut first_frame_ms = None;
    let mut first_frame_from_load_ms = None;
    let mut poster_before_play = false;
    let mut first_position_ms = None;
    if let Ok(req) = play_id {
        let _ = wait_completion(&events, req, Duration::from_secs(5), &mut col);
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if has_video {
                if let Some(at) = sink.marks.lock().first().map(|(_, t)| *t) {
                    first_frame_from_load_ms = Some((at - load_mono) * 1000.0);
                    let from_play = (at - play_mono) * 1000.0;
                    poster_before_play = from_play < 0.0;
                    first_frame_ms = Some(from_play.max(0.0));
                    break;
                }
            } else {
                let pos = engine.snapshot().position_ms;
                if pos > 0 {
                    first_position_ms = Some(pos);
                    first_frame_ms = Some(play_at.elapsed().as_secs_f64() * 1000.0);
                    first_frame_from_load_ms =
                        Some(opened.elapsed().as_secs_f64() * 1000.0 + first_frame_ms.unwrap());
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    // 稳态窗口：呈现帧率与 AV 漂移。漂移用"首帧锚点"外推，与 mpv 的
    // video-out-pts 对时口径一致
    sink.rebase();
    let steady_from = mono_secs();
    std::thread::sleep(Duration::from_secs_f64(args.hold));
    let steady_to = mono_secs();
    let steady_frames = sink.frames_since(None);
    let steady_count = steady_frames.len() as f64;
    let presented_fps = if steady_to > steady_from {
        steady_count / (steady_to - steady_from)
    } else {
        0.0
    };
    let av_drift_max_ms = if has_video && steady_frames.len() >= 3 {
        let (base_pts, base_t) = steady_frames[0];
        steady_frames
            .iter()
            .map(|(pts, t)| (pts - (base_pts + (t - base_t) * 1000.0)).abs())
            .fold(0.0f64, f64::max)
    } else {
        0.0
    };
    let position_ms = engine.snapshot().position_ms;

    // seek：exact + 三个点，每点测"命令下发 → 目标位置的呈现帧到达"
    let mut seeks = Vec::new();
    if matches!(
        engine.snapshot().state,
        dyl_core::contract::PlayerState::Playing
    ) {
        for target in case["seek_ms"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_u64())
        {
            if target >= duration_ms.saturating_sub(50) {
                continue;
            }
            sink.rebase();
            let issued = Instant::now();
            let req = match engine.command(CommandEnvelope::new(Command::Seek {
                position_ms: target,
                mode: SeekMode::Exact,
            })) {
                Ok(req) => req,
                Err(e) => {
                    seeks.push(json!({
                        "target_ms": target,
                        "error": e.message,
                        "present_ms": Value::Null,
                    }));
                    continue;
                }
            };
            wait_completion(&events, req, Duration::from_secs(10), &mut col);
            let completed_ms = issued.elapsed().as_secs_f64() * 1000.0;
            let actual_ms = col.seek_actual_ms.take();

            let deadline = Instant::now() + Duration::from_secs(10);
            let mut present_ms = None;
            while Instant::now() < deadline {
                if !has_video {
                    // 邻域判定，理由同 mpv 侧：主时钟可能还停在 seek 前的位置
                    let pos = engine.snapshot().position_ms;
                    if (target.saturating_sub(80)..=target + 500).contains(&pos) {
                        present_ms = Some(issued.elapsed().as_secs_f64() * 1000.0);
                        break;
                    }
                } else {
                    let hit = sink
                        .frames_since(Some(target as f64))
                        .into_iter()
                        .map(|(_, t)| t)
                        .reduce(f64::min);
                    if let Some(t) = hit {
                        let base = *sink.cleared_at.lock();
                        present_ms = Some((t - base) * 1000.0);
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            seeks.push(json!({
                "target_ms": target,
                "completed_ms": (completed_ms * 10.0).round() / 10.0,
                "actual_ms": actual_ms,
                "basis": if has_video { "presented_frame" } else { "audio_clock" },
                "present_ms": present_ms.map(|v| (v * 10.0).round() / 10.0),
            }));
        }
    }

    // 字幕时间轴自检：按 manifest 声明的 (时刻, 期望条数) 问引擎"此刻该有几条字幕"。
    //
    // 不 seek——播放结束后 seek 会走重启语义，位置就不是探针要的那个位置了。
    // 期望值来自语料脚本里写死的 cue 表，所以时间轴解析错位（1.5s 被读成 1500s、
    // start/end 丢失、内嵌轨没喂进引擎）都会在这里变成可判定的不一致，而不需要人眼看帧。
    let timeline_probe = match case["cue_expect"].as_array() {
        None => Value::Null,
        Some(points) => {
            let mut entries = Vec::new();
            let mut mismatches = 0usize;
            for pt in points {
                let at_ms = pt[0].as_u64().unwrap_or(0);
                let want = pt[1].as_u64().unwrap_or(0);
                let mut entry = active_cues(&engine, at_ms as f64 / 1000.0);
                let got = entry["active_text_events"].as_u64().unwrap_or(0);
                if got != want {
                    mismatches += 1;
                }
                entry["at_ms"] = json!(at_ms);
                entry["expected"] = json!(want);
                entry["measured"] = json!(got);
                entries.push(entry);
            }
            json!({ "points": entries, "mismatches": mismatches })
        }
    };

    // 换轨：能力位声明支持，测"选中另一条音轨后继续播"
    let mut track_switch = Value::Null;
    if case["track_switch"].as_bool().unwrap_or(false) {
        let current = engine.snapshot();
        let audio_tracks: Vec<u32> = current
            .media
            .iter()
            .flat_map(|m| m.tracks.iter())
            .filter(|t| t.kind == TrackKind::Audio)
            .map(|t| t.index)
            .collect();
        if let Some(next) = audio_tracks.iter().copied().find(|i| {
            !current
                .media
                .as_ref()
                .map(|m| m.tracks.iter().any(|t| t.index == *i && t.selected))
                .unwrap_or(false)
        }) {
            let t = Instant::now();
            let ok = engine
                .command(CommandEnvelope::new(Command::SelectTrack {
                    kind: TrackKind::Audio,
                    index: next,
                }))
                .ok()
                .and_then(|req| wait_completion(&events, req, Duration::from_secs(15), &mut col))
                .map(|c| c.status == CommandStatus::Ok)
                .unwrap_or(false);
            sink.rebase();
            std::thread::sleep(Duration::from_millis(600));
            track_switch = json!({
                "to_index": next,
                "ok": ok,
                "ms": t.elapsed().as_secs_f64() * 1000.0,
                "frames_after": sink.total(),
                "position_ms": engine.snapshot().position_ms,
                "audio_codec": engine.snapshot().media.as_ref().and_then(|m| m.audio.as_ref().map(|a| a.codec.clone())),
            });
        }
    }

    engine
        .command(CommandEnvelope::new(Command::Stop))
        .ok()
        .map(|req| wait_completion(&events, req, Duration::from_secs(5), &mut col));
    engine.destroy();

    let row = json!({
        "id": id,
        "expect": expect,
        "verdict": if first_frame_ms.is_some() { "ok" } else { "no_first_frame" },
        "source": source,
        "tags": case["tags"],
        "note": case["note"],
        "duration_ms": duration_ms,
        "open_ms": (open_ms * 10.0).round() / 10.0,
        "first_frame_ms": first_frame_ms.map(|v| (v * 10.0).round() / 10.0),
        "first_frame_from_load_ms": first_frame_from_load_ms.map(|v| (v * 10.0).round() / 10.0),
        "poster_before_play": poster_before_play,
        "first_position_ms": first_position_ms,
        "presented_fps": (presented_fps * 100.0).round() / 100.0,
        "presented_frames": steady_count as u64,
        "av_drift_max_ms": (av_drift_max_ms * 10.0).round() / 10.0,
        "position_ms": position_ms,
        "has_video": has_video,
        "decode_backend": format!("{decode_backend:?}"),
        "media": media,
        "overlays": Value::Object(overlays),
        "subtitle_timeline": timeline_probe,
        "seek": seeks,
        "track_switch": track_switch,
        "errors": col.errors,
        "warnings": col.warnings,
    });
    with_resources(row, &before)
}

/// 叠加层引擎的内部真相：字幕取事件表与样式表，弹幕取解析条数与布局实例
fn overlay_state(engine: &Engine, kind: &str) -> Value {
    let handles = engine.overlay_handles();
    if kind == "subtitle" {
        let shared = handles.subtitle.lock().shared_state();
        let st = shared.lock();
        json!({
            "events": st.events.len(),
            "ass_events": st.ass_events.len(),
            "styles": st.styles.len(),
            "play_res": [st.play_res.0, st.play_res.1],
            "external_source": st.external_source,
            "enabled": st.enabled,
        })
    } else {
        let shared = handles.danmaku.lock().shared_state();
        let st = shared.lock();
        json!({
            "items": st.items.len(),
            "instances": st.instances.len(),
            "current_pts": st.current_pts,
            "prepared": st.prepared.is_some(),
            "revision": st.prepared_revision,
            "enabled": st.enabled,
        })
    }
}

/// 目标时刻命中的字幕事件（文本视图 + ASS 视图）。
///
/// 不能用 `state.current_text`：那是 `render()` 里才写的字段，无窗口跑永远是空串，
/// 用它判"字幕没出来"是假阳性。这里按 start/end 判定，测的是解析与计时。
fn active_cues(engine: &Engine, at_secs: f64) -> Value {
    let handles = engine.overlay_handles();
    let shared = handles.subtitle.lock().shared_state();
    let st = shared.lock();
    let hit = |e: &dyl_subtitle::SubtitleEvent| e.start_time <= at_secs && at_secs <= e.end_time;
    let texts: Vec<String> = st
        .events
        .iter()
        .filter(|e| hit(e))
        .map(|e| e.text.clone())
        .collect();
    json!({
        "active_text_events": texts.len(),
        "active_ass_events": st
            .ass_events
            .iter()
            .filter(|a| hit(&a.base))
            .count(),
        "sample_texts": texts,
    })
}

fn resolve_source(case: &Value, corpus_dir: &Path, http_base: Option<&str>) -> Option<String> {
    if let Some(rel) = case["http_path"].as_str() {
        let base = http_base?;
        return Some(format!("{base}/{rel}"));
    }
    if let Some(absent) = case["file_absent"].as_str() {
        return Some(corpus_dir.join(absent).to_string_lossy().replace('\\', "/"));
    }
    let file = case["file"].as_str()?;
    let path = corpus_dir.join(file);
    if !path.exists() {
        return None;
    }
    Some(path.to_string_lossy().replace('\\', "/"))
}

fn with_resources(mut row: Value, before: &ProcessSample) -> Value {
    let after = sample_process();
    let delta_mib = match (before.rss_bytes, after.rss_bytes) {
        (Some(b), Some(a)) => Some((a as f64 - b as f64) / 1048576.0),
        _ => None,
    };
    let cpu_ms = match (before.cpu_ms, after.cpu_ms) {
        (Some(b), Some(a)) => a - b,
        _ => 0.0,
    };
    row["rss_end_mib"] = json!(after
        .rss_bytes
        .map(|b| (b as f64 / 1048576.0 * 10.0).round() / 10.0));
    row["rss_delta_mib"] = json!(delta_mib.map(|v| (v * 10.0).round() / 10.0));
    row["cpu_ms_case"] = json!((cpu_ms * 10.0).round() / 10.0);
    row["threads"] = json!(after.threads);
    row
}

/// 跑一条命令期间从事件通道收集到的旁路信息
#[derive(Default)]
struct Collected {
    errors: Vec<String>,
    warnings: Vec<String>,
    seek_actual_ms: Option<u64>,
}

/// 等 `request_id` 的完成事件，顺带把期间出现的 error / warning / seek_completed 收走
fn wait_completion(
    events: &Receiver<EventEnvelope>,
    request_id: u64,
    timeout: Duration,
    out: &mut Collected,
) -> Option<dyl_core::contract::CommandCompleted> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match events.recv_timeout(left) {
            Ok(msg) => match &msg.event {
                Event::CommandCompleted(c) if c.request_id == request_id => return Some(c.clone()),
                Event::Error(e) => out.errors.push(e.message.clone()),
                Event::Warning(w) => out.warnings.push(format!("{}: {}", w.code, w.message)),
                Event::SeekCompleted(s) => out.seek_actual_ms = Some(s.actual_ms),
                _ => {}
            },
            Err(_) => return None,
        }
    }
}

// ── 进程自采样（RSS / CPU 时间 / 线程数）──────────────────────────────

struct ProcessSample {
    rss_bytes: Option<u64>,
    cpu_ms: Option<f64>,
    threads: Option<u64>,
}

fn sample_process() -> ProcessSample {
    #[cfg(windows)]
    {
        mod sys {
            use std::os::raw::c_void;

            #[repr(C)]
            #[derive(Default, Clone, Copy)]
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

            #[repr(C)]
            #[derive(Default, Clone, Copy)]
            pub struct FileTime {
                pub low: u32,
                pub high: u32,
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
                pub fn GetProcessTimes(
                    process: *mut c_void,
                    creation: *mut FileTime,
                    exit_time: *mut FileTime,
                    kernel: *mut FileTime,
                    user: *mut FileTime,
                ) -> i32;
                pub fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> isize;
                pub fn Thread32First(snapshot: isize, entry: *mut ThreadEntry32) -> i32;
                pub fn Thread32Next(snapshot: isize, entry: *mut ThreadEntry32) -> i32;
                pub fn CloseHandle(handle: isize) -> i32;
            }
        }

        let to_ms = |ft: sys::FileTime| {
            let ticks = ((ft.high as u64) << 32) | ft.low as u64;
            ticks as f64 / 10_000.0
        };

        let (rss_bytes, cpu_ms) = unsafe {
            let handle = sys::GetCurrentProcess();
            let mut counters = sys::ProcessMemoryCounters {
                cb: std::mem::size_of::<sys::ProcessMemoryCounters>() as u32,
                ..Default::default()
            };
            let rss = if sys::K32GetProcessMemoryInfo(handle, &mut counters, counters.cb) != 0 {
                Some(counters.working_set_size as u64)
            } else {
                None
            };
            let (mut creation, mut exit_time, mut kernel, mut user) = (
                sys::FileTime::default(),
                sys::FileTime::default(),
                sys::FileTime::default(),
                sys::FileTime::default(),
            );
            let cpu = if sys::GetProcessTimes(
                handle,
                &mut creation,
                &mut exit_time,
                &mut kernel,
                &mut user,
            ) != 0
            {
                Some(to_ms(kernel) + to_ms(user))
            } else {
                None
            };
            (rss, cpu)
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
                let mut count = 0u64;
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

        ProcessSample {
            rss_bytes,
            cpu_ms,
            threads,
        }
    }
    #[cfg(not(windows))]
    {
        let read = |p: &str| -> Option<String> { std::fs::read_to_string(p).ok() };
        let statm = read("/proc/self/statm")?;
        let rss_pages = statm
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let stat = read("/proc/self/stat")?;
        let fields: Vec<&str> = stat.split_whitespace().collect();
        let ticks: f64 = fields.get(13).and_then(|v| v.parse().ok()).unwrap_or(0) as f64
            + fields.get(14).and_then(|v| v.parse().ok()).unwrap_or(0) as f64;
        let threads = fields.get(18).and_then(|v| v.parse::<u64>().ok());
        ProcessSample {
            rss_bytes: Some(rss_pages * 4096),
            cpu_ms: Some(ticks * 10.0),
            threads,
        }
    }
}

fn chrono_free_utc_now() -> String {
    // 不引 chrono：ISO 时间戳由 shell 侧写入，这里只带一个可排序的本地秒
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}
