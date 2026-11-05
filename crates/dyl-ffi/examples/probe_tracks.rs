//! 临时探针：验证软解路径与内嵌音轨/字幕轨自动选择。
//!
//! 用法: `cargo run --example probe_tracks -p dyl-ffi -- "<媒体路径>" [秒数]`
//! 以 host sink 接收 CPU 帧，因此不需要 GPU/Surface，可观察解码帧的真实格式。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dyl_core::contract::{Command, CommandEnvelope, Event, EventEnvelope, PlayerState};
use dyl_engine::{Engine, HostFrameSink, VideoFrame};
use parking_lot::Mutex;

#[derive(Default)]
struct Sink {
    count: AtomicU64,
    first: Mutex<Option<String>>,
    last: Mutex<Option<String>>,
}

impl HostFrameSink for Sink {
    fn push_frame(&self, frame: VideoFrame) {
        let n = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        let planes = frame
            .planes
            .iter()
            .map(|p| format!("{}B/stride{}", p.data.len(), p.stride))
            .collect::<Vec<_>>()
            .join(" ");
        let desc = format!(
            "pts={:?} tb={}/{} {}x{} display={}x{} {:?} color={:?}/{:?}/{:?} planes=[{}]",
            frame.pts,
            frame.time_base.0,
            frame.time_base.1,
            frame.width,
            frame.height,
            frame.display_width,
            frame.display_height,
            frame.format,
            frame.color_space,
            frame.color_primaries,
            frame.color_range,
            planes
        );
        if n == 1 {
            *self.first.lock() = Some(desc.clone());
        }
        *self.last.lock() = Some(desc);
    }

    fn request_redraw(&self) {}
}

fn drain(events: &Receiver<EventEnvelope>, verbose: bool) {
    while let Ok(envelope) = events.try_recv() {
        if verbose {
            println!("  event seq={} {:?}", envelope.seq, envelope.event);
        }
    }
}

fn wait_completion(
    events: &Receiver<EventEnvelope>,
    request_id: u64,
    timeout: Duration,
) -> Option<Event> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(envelope) => {
                println!("  event seq={} {:?}", envelope.seq, envelope.event);
                if let Event::CommandCompleted(completed) = &envelope.event {
                    if completed.request_id == request_id {
                        return Some(envelope.event);
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => return None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn main() {
    std::env::set_var("RUST_BACKTRACE", "1");
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "dyl=info,dyl_pipeline=info,dyl_ffmpeg=info");
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "test_video.mp4".to_string());
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8).max(1);

    println!("=== probe: {} ({}s) ===", path, seconds);

    let (mut engine, events) = Engine::new().expect("engine start");
    let sink = Arc::new(Sink::default());
    engine.set_host_frame_sink(sink.clone());

    let rid = engine
        .command(CommandEnvelope::new(Command::LoadFile { url: path }))
        .expect("load_file accepted");
    let loaded = wait_completion(&events, rid, Duration::from_secs(30));
    println!("load result: {:?}", loaded);

    let snapshot = engine.snapshot();
    println!(
        "state={:?} duration_ms={} source={:?}",
        snapshot.state, snapshot.duration_ms, snapshot.source
    );
    if let Some(media) = &snapshot.media {
        if let Some(video) = &media.video {
            println!(
                "selected VIDEO: codec={} {}x{} fps={}",
                video.codec, video.width, video.height, video.fps
            );
        }
        if let Some(audio) = &media.audio {
            println!(
                "selected AUDIO: codec={} {}Hz {}ch",
                audio.codec, audio.sample_rate, audio.channels
            );
        }
        println!("enumerated tracks ({}):", media.tracks.len());
        for track in &media.tracks {
            println!(
                "  #{} {:?} codec={} language={:?} title={:?} default={} selected={}",
                track.index,
                track.kind,
                track.codec,
                track.language,
                track.title,
                track.default,
                track.selected
            );
        }
    } else {
        println!("selected: no media info");
    }

    let overlays = engine.overlay_handles();
    let subtitle_state = overlays.subtitle.lock().shared_state();

    if snapshot.state != PlayerState::Ready && snapshot.state != PlayerState::Paused {
        println!("unexpected state after load: {:?}", snapshot.state);
    }

    let rid = engine
        .command(CommandEnvelope::new(Command::Play))
        .expect("play accepted");
    wait_completion(&events, rid, Duration::from_secs(10));

    let started = Instant::now();
    let mut last_position = 0u64;
    while started.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_millis(1000));
        drain(&events, true);
        let snap = engine.snapshot();
        let st = subtitle_state.lock();
        println!(
            "t={:>2}s state={:?} position_ms={} video_frames={} rendered={} dropped={} subtitle_events={} danmaku_events={}",
            started.elapsed().as_secs(),
            snap.state,
            snap.position_ms,
            sink.count.load(Ordering::Relaxed),
            snap.stats.rendered_frames,
            snap.stats.dropped_frames,
            st.events.len(),
            st.current_text.len(),
        );
        last_position = snap.position_ms;
    }

    {
        let st = subtitle_state.lock();
        println!(
            "subtitle events (first 5 of {}, ass_events={}, styles={}, play_res={:.0}x{:.0}):",
            st.events.len(),
            st.ass_events.len(),
            st.styles.len(),
            st.play_res.0,
            st.play_res.1
        );
        for event in st.events.iter().take(5) {
            println!(
                "  {:.2}-{:.2} style={} text={:?}",
                event.start_time, event.end_time, event.style, event.text
            );
        }
        let style_names: Vec<&str> = st.styles.keys().take(12).map(|s| s.as_str()).collect();
        println!("  style names: {style_names:?}");
        println!(
            "subtitle external_source={} current_text={:?}",
            st.external_source, st.current_text
        );
    }
    println!("first decoded frame: {:?}", sink.first.lock().as_deref());
    println!("last  decoded frame: {:?}", sink.last.lock().as_deref());
    println!(
        "summary: video_frames={} position_ms={} subtitle_events={}",
        sink.count.load(Ordering::Relaxed),
        last_position,
        subtitle_state.lock().events.len()
    );

    drain(&events, true);
    engine.destroy();
    println!("=== probe done ===");
}
