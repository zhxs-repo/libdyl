//! 桌面 CLI 播放器 — winit 窗口 + external_surface 输出
//!
//! 用法: `cargo run --example cli_player -p dyl-ffi -- test_video.mp4
//!        [--subtitle <srt/ass>] [--danmaku <xml>]`
//!
//! 键盘: Space 播放/暂停 · ←/→ ±5s · ↑/↓ 音量 · M 静音 · S 字幕 · D 弹幕
//!       O 弹幕不透明度 · Q/Esc 退出
//! 控制台: `s <path>` 载入字幕 · `d <path>` 载入弹幕 · `t` 字幕开关 · `b` 弹幕开关
//!         `o <0-100>` 不透明度 · `p` 播放/暂停 · `l <ms>` 跳转 · `q` 退出

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandCompleted, CommandEnvelope, CommandStatus, Event, EventEnvelope, PlayerState,
    SurfaceDescriptor, SCHEMA_VERSION,
};
use dyl_engine::Engine;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

const SEEK_STEP_MS: i64 = 5_000;
const VOLUME_STEP: u8 = 5;
const HUD_INTERVAL: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
enum UserCmd {
    LoadSubtitle(String),
    LoadDanmaku(String),
    ToggleSubtitle,
    ToggleDanmaku,
    SetDanmakuOpacity(u8),
    TogglePlay,
    SeekAbsolute(u64),
    Quit,
}

enum UserEvent {
    Command(UserCmd),
}

struct Hud {
    last_update: Instant,
    last_frames: u64,
    rfps: f64,
}

struct PlayerApp {
    engine: Engine,
    events: Receiver<EventEnvelope>,
    window: Option<Arc<Window>>,
    surface_attached: bool,
    subtitle_path: Option<String>,
    danmaku_path: Option<String>,
    hud: Hud,
}

impl PlayerApp {
    fn new(
        engine: Engine,
        events: Receiver<EventEnvelope>,
        subtitle_path: Option<String>,
        danmaku_path: Option<String>,
    ) -> Self {
        Self {
            engine,
            events,
            window: None,
            surface_attached: false,
            subtitle_path,
            danmaku_path,
            hud: Hud {
                last_update: Instant::now(),
                last_frames: 0,
                rfps: 0.0,
            },
        }
    }

    fn send(&self, command: Command) {
        match self.engine.command(CommandEnvelope::new(command)) {
            Ok(_) => {}
            Err(error) => eprintln!("  command rejected: {error}"),
        }
    }

    fn toggle_play(&self) {
        let state = self.engine.snapshot().state;
        match state {
            PlayerState::Playing => self.send(Command::Pause),
            PlayerState::Paused
            | PlayerState::Ready
            | PlayerState::Ended
            | PlayerState::Stopped => self.send(Command::Play),
            other => eprintln!("  cannot toggle playback in state {other:?}"),
        }
    }

    fn toggle_subtitle(&self) {
        let visible = self.engine.snapshot().subtitle.visible;
        self.send(Command::SetSubtitleVisible { visible: !visible });
    }

    fn toggle_danmaku(&self) {
        let visible = self.engine.snapshot().danmaku.visible;
        self.send(Command::SetDanmakuVisible { visible: !visible });
    }

    fn step_volume(&self, delta: i16) {
        let volume = self.engine.snapshot().volume as i16;
        let next = (volume + delta).clamp(0, 100) as u8;
        self.send(Command::SetVolume { volume: next });
    }

    fn cycle_danmaku_opacity(&self) {
        let opacity = self.engine.snapshot().danmaku.opacity;
        let next = match opacity {
            76..=100 => 75,
            51..=75 => 50,
            26..=50 => 25,
            1..=25 => 0,
            _ => 100,
        };
        self.send(Command::SetDanmakuOpacity { opacity: next });
    }

    fn seek_relative(&self, delta_ms: i64) {
        self.send(Command::SeekRelative {
            delta_ms,
            mode: dyl_core::contract::SeekMode::Exact,
        });
    }

    fn handle_user_command(&mut self, event_loop: &ActiveEventLoop, command: UserCmd) {
        match command {
            UserCmd::LoadSubtitle(path) => {
                println!("  loading subtitle: {path}");
                self.send(Command::LoadSubtitle { path });
            }
            UserCmd::LoadDanmaku(path) => {
                println!("  loading danmaku: {path}");
                self.send(Command::LoadDanmaku { path });
            }
            UserCmd::ToggleSubtitle => self.toggle_subtitle(),
            UserCmd::ToggleDanmaku => self.toggle_danmaku(),
            UserCmd::SetDanmakuOpacity(opacity) => {
                self.send(Command::SetDanmakuOpacity {
                    opacity: opacity.min(100),
                });
            }
            UserCmd::TogglePlay => self.toggle_play(),
            UserCmd::SeekAbsolute(position_ms) => self.send(Command::Seek {
                position_ms,
                mode: dyl_core::contract::SeekMode::Exact,
            }),
            UserCmd::Quit => event_loop.exit(),
        }
    }

    fn handle_engine_event(&mut self, envelope: EventEnvelope) {
        match envelope.event {
            Event::FileLoaded(info) => {
                println!(
                    "[file] {} | {}ms | {:?}",
                    info.source, info.duration_ms, info.media
                );
                self.send(Command::Play);
                if let Some(path) = self.subtitle_path.take() {
                    self.send(Command::LoadSubtitle { path });
                }
                if let Some(path) = self.danmaku_path.take() {
                    self.send(Command::LoadDanmaku { path });
                }
            }
            Event::StateChanged(changed) => {
                println!(
                    "[state] {:?} -> {:?} ({:?})",
                    changed.from, changed.to, changed.reason
                );
            }
            Event::SeekCompleted(seek) => {
                println!("[seek] {}ms", seek.actual_ms);
            }
            Event::Error(error) => {
                eprintln!(
                    "[error] {} ({}): {}",
                    error.code.as_str(),
                    error.code_num,
                    error.message
                );
            }
            Event::Warning(warning) => {
                eprintln!("[warning] {}: {}", warning.code, warning.message);
            }
            Event::CommandCompleted(completed) => {
                self.report_failure(&completed);
            }
            Event::SurfaceAttached(attached) => {
                println!(
                    "[surface] {:?} {}x{} @{}x",
                    attached.kind, attached.width, attached.height, attached.scale_factor
                );
            }
            Event::SurfaceDetached(detached) => {
                println!("[surface] detached {:?}", detached.kind);
            }
            Event::SurfaceResized(resized) => {
                println!("[surface] resized {}x{}", resized.width, resized.height);
            }
            _ => {}
        }
    }

    fn report_failure(&self, completed: &CommandCompleted) {
        if completed.status == CommandStatus::Ok {
            return;
        }
        let detail = completed
            .error
            .as_ref()
            .map(|error| error.code.as_str().to_string())
            .unwrap_or_else(|| {
                self.engine
                    .snapshot()
                    .last_error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "no detail".into())
            });
        eprintln!(
            "[command] {:?} {:?}: {}",
            completed.op, completed.status, detail
        );
    }

    fn drain_engine_events(&mut self) {
        while let Ok(envelope) = self.events.try_recv() {
            self.handle_engine_event(envelope);
        }
    }

    fn refresh_hud(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let now = Instant::now();
        let snapshot = self.engine.snapshot();
        let since_update = now.duration_since(self.hud.last_update);
        if since_update >= HUD_INTERVAL {
            let elapsed = since_update.as_secs_f64();
            let frames = snapshot.stats.rendered_frames;
            if elapsed > 0.0 && frames >= self.hud.last_frames {
                self.hud.rfps = (frames - self.hud.last_frames) as f64 / elapsed;
            }
            self.hud.last_frames = frames;
            self.hud.last_update = now;
        }

        let media = snapshot
            .media
            .as_ref()
            .map(|media| {
                let video = media
                    .video
                    .as_ref()
                    .map(|video| {
                        format!(
                            "{} {}x{}@{:.1} SW",
                            video.codec, video.width, video.height, video.fps
                        )
                    })
                    .unwrap_or_else(|| "no video".into());
                let audio = media
                    .audio
                    .as_ref()
                    .map(|audio| {
                        format!(
                            "{} {}Hz {}ch",
                            audio.codec, audio.sample_rate, audio.channels
                        )
                    })
                    .unwrap_or_else(|| "no audio".into());
                format!("{video} | {audio}")
            })
            .unwrap_or_else(|| "no media".into());

        let state = match snapshot.state {
            PlayerState::Idle => "IDLE",
            PlayerState::Loading => "LOAD",
            PlayerState::Ready => "READY",
            PlayerState::Playing => "PLAY",
            PlayerState::Paused => "PAUSE",
            PlayerState::Ended => "ENDED",
            PlayerState::Stopped => "STOP",
            PlayerState::Error => "ERROR",
        };
        let position = format!(
            "{:02}:{:05.2}",
            snapshot.position_ms / 60_000,
            (snapshot.position_ms % 60_000) as f64 / 1000.0
        );
        let duration = format!(
            "{:02}:{:05.2}",
            snapshot.duration_ms / 60_000,
            (snapshot.duration_ms % 60_000) as f64 / 1000.0
        );
        let muted = if snapshot.muted { " MUTE" } else { "" };
        let title = format!(
            "Dyl | {state} {position}/{duration} | {:.1}rfps | {media} | vol {}{} | SUB {} | DM {}({})",
            self.hud.rfps,
            snapshot.volume,
            muted,
            if snapshot.subtitle.visible { "ON" } else { "OFF" },
            if snapshot.danmaku.visible { "ON" } else { "OFF" },
            snapshot.danmaku.count,
        );
        window.set_title(&title);
    }

    fn cleanup(&mut self) {
        if self.surface_attached {
            if let Err(error) = self.engine.detach_surface() {
                eprintln!("  detach surface failed: {error}");
            }
            self.surface_attached = false;
        }
        self.engine.destroy();
    }
}

impl ApplicationHandler<UserEvent> for PlayerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("Dyl")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                eprintln!("FATAL: create window failed: {error}");
                event_loop.exit();
                return;
            }
        };

        match surface_descriptor(&window) {
            Ok(desc) => match self.engine.attach_surface(desc) {
                Ok(()) => self.surface_attached = true,
                Err(error) => eprintln!(
                    "warning: attach surface failed ({error}); playback continues headless"
                ),
            },
            Err(error) => eprintln!("warning: no native surface ({error}); playback is headless"),
        }

        self.window = Some(window);
        println!("Keyboard: Space play/pause, ←/→ seek, ↑/↓ volume, M mute, S subtitle, D danmaku, O opacity, Q quit");
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Command(command) => self.handle_user_command(event_loop, command),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if self.surface_attached && size.width > 0 && size.height > 0 {
                    if let Err(error) = self.engine.resize_surface(size.width, size.height) {
                        eprintln!("  resize surface failed: {error}");
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed || event.repeat {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                match code {
                    KeyCode::Escape | KeyCode::KeyQ => event_loop.exit(),
                    KeyCode::Space => self.toggle_play(),
                    KeyCode::ArrowLeft => self.seek_relative(-SEEK_STEP_MS),
                    KeyCode::ArrowRight => self.seek_relative(SEEK_STEP_MS),
                    KeyCode::ArrowUp => self.step_volume(VOLUME_STEP as i16),
                    KeyCode::ArrowDown => self.step_volume(-(VOLUME_STEP as i16)),
                    KeyCode::KeyM => {
                        let muted = self.engine.snapshot().muted;
                        self.send(Command::SetMute { muted: !muted });
                    }
                    KeyCode::KeyS => self.toggle_subtitle(),
                    KeyCode::KeyD => self.toggle_danmaku(),
                    KeyCode::KeyO => self.cycle_danmaku_opacity(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_engine_events();
        self.refresh_hud();
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.cleanup();
    }
}

#[cfg(target_os = "windows")]
fn surface_descriptor(window: &Window) -> Result<SurfaceDescriptor, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let size = window.inner_size();
    let handle = window.window_handle().map_err(|error| error.to_string())?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return Err("window is not a Win32 window".into());
    };
    Ok(SurfaceDescriptor::WindowsHwnd {
        schema: SCHEMA_VERSION,
        handle: format!("0x{:x}", win32.hwnd.get() as usize),
        width: size.width.max(1),
        height: size.height.max(1),
        scale_factor: window.scale_factor(),
    })
}

#[cfg(target_os = "macos")]
fn surface_descriptor(window: &Window) -> Result<SurfaceDescriptor, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let size = window.inner_size();
    let handle = window.window_handle().map_err(|error| error.to_string())?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return Err("window is not an AppKit view".into());
    };
    Ok(SurfaceDescriptor::MacosNsview {
        schema: SCHEMA_VERSION,
        handle: format!("0x{:x}", appkit.ns_view.as_ptr() as usize),
        width: size.width.max(1),
        height: size.height.max(1),
        scale_factor: window.scale_factor(),
    })
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
fn surface_descriptor(window: &Window) -> Result<SurfaceDescriptor, String> {
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};

    let size = window.inner_size();
    let width = size.width.max(1);
    let height = size.height.max(1);
    let scale_factor = window.scale_factor();
    let handle = window.window_handle().map_err(|error| error.to_string())?;
    let display = window.display_handle().map_err(|error| error.to_string())?;
    match (handle.as_raw(), display.as_raw()) {
        (RawWindowHandle::Xlib(xlib), RawDisplayHandle::Xlib(display)) => {
            Ok(SurfaceDescriptor::X11Window {
                schema: SCHEMA_VERSION,
                handle: format!("0x{:x}", xlib.window),
                display: format!(
                    "0x{:x}",
                    display.display.map(|n| n.as_ptr() as usize).unwrap_or(0)
                ),
                width,
                height,
                scale_factor,
            })
        }
        (RawWindowHandle::Wayland(surface), RawDisplayHandle::Wayland(display)) => {
            Ok(SurfaceDescriptor::WaylandSurface {
                schema: SCHEMA_VERSION,
                surface: format!(
                    "0x{:x}",
                    surface.surface.map(|n| n.as_ptr() as usize).unwrap_or(0)
                ),
                display: format!(
                    "0x{:x}",
                    display.display.map(|n| n.as_ptr() as usize).unwrap_or(0)
                ),
                width,
                height,
                scale_factor,
            })
        }
        _ => Err("unsupported window system (expected X11 or Wayland)".into()),
    }
}

#[cfg(any(target_os = "android", not(any(unix, target_os = "windows"))))]
fn surface_descriptor(_window: &Window) -> Result<SurfaceDescriptor, String> {
    Err("external surface is not supported on this platform by cli_player".into())
}

fn parse_args() -> Option<(String, Option<String>, Option<String>)> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: cli_player <video_file> [--subtitle <srt/ass>] [--danmaku <xml>]");
        return None;
    }
    let source = args[1].clone();
    let mut subtitle = None;
    let mut danmaku = None;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--subtitle" | "-s" if index + 1 < args.len() => {
                subtitle = Some(args[index + 1].clone());
                index += 2;
            }
            "--danmaku" | "-d" if index + 1 < args.len() => {
                danmaku = Some(args[index + 1].clone());
                index += 2;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                index += 1;
            }
        }
    }
    Some((source, subtitle, danmaku))
}

fn spawn_stdin(proxy: EventLoopProxy<UserEvent>, input_done: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("cli-input".into())
        .spawn(move || {
            use std::io::{self, BufRead, Write};

            let stdin = io::stdin();
            let mut reader = stdin.lock();
            loop {
                if input_done.load(Ordering::Acquire) {
                    break;
                }
                print!("> ");
                let _ = io::stdout().flush();
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let (name, argument) = match trimmed.split_once(' ') {
                            Some((name, rest)) => (name, Some(rest.trim())),
                            None => (trimmed, None),
                        };
                        let command = match (name, argument) {
                            ("s", Some(path)) => Some(UserCmd::LoadSubtitle(path.to_string())),
                            ("d", Some(path)) => Some(UserCmd::LoadDanmaku(path.to_string())),
                            ("t", _) => Some(UserCmd::ToggleSubtitle),
                            ("b", _) => Some(UserCmd::ToggleDanmaku),
                            ("o", Some(value)) => value
                                .parse::<u8>()
                                .ok()
                                .map(|value| UserCmd::SetDanmakuOpacity(value.min(100))),
                            ("p", _) => Some(UserCmd::TogglePlay),
                            ("l", Some(value)) => {
                                value.parse::<u64>().ok().map(UserCmd::SeekAbsolute)
                            }
                            ("q", _) => Some(UserCmd::Quit),
                            ("h" | "?", _) => {
                                println!(
                                    "  s <path> 载入字幕 · d <path> 载入弹幕 · t/b 字幕/弹幕开关"
                                );
                                println!(
                                    "  o <0-100> 不透明度 · p 播放/暂停 · l <ms> 跳转 · q 退出"
                                );
                                None
                            }
                            _ => {
                                eprintln!("  unknown command: {name}");
                                None
                            }
                        };
                        if let Some(command) = command {
                            let quit = matches!(command, UserCmd::Quit);
                            if proxy.send_event(UserEvent::Command(command)).is_err() {
                                break;
                            }
                            if quit {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("spawn stdin thread");
}

fn main() {
    std::env::set_var("RUST_BACKTRACE", "1");
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "dyl=info");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("dyl=info".parse().unwrap()),
        )
        .init();

    let Some((source, subtitle_path, danmaku_path)) = parse_args() else {
        std::process::exit(1);
    };

    println!("╔══════════════════════════════════════════╗");
    println!("║         Dyl CLI Player                ║");
    println!("╚══════════════════════════════════════════╝");
    println!("Video: {source}");
    if let Some(path) = &subtitle_path {
        println!("Subtitle: {path}");
    }
    if let Some(path) = &danmaku_path {
        println!("Danmaku: {path}");
    }

    let (engine, events) = Engine::new().expect("engine must start");
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("create event loop");
    let input_done = Arc::new(AtomicBool::new(false));
    spawn_stdin(event_loop.create_proxy(), input_done.clone());

    let mut app = PlayerApp::new(engine, events, subtitle_path, danmaku_path);

    // 启动后先入队 load_file；resumed() 只负责建窗与 attach
    if let Err(error) = app
        .engine
        .command(CommandEnvelope::new(Command::LoadFile { url: source }))
    {
        eprintln!("FATAL: load_file rejected: {error}");
        std::process::exit(1);
    }

    event_loop.run_app(&mut app).expect("event loop");
    input_done.store(true, Ordering::Release);
    println!("Playback finished.");
}
