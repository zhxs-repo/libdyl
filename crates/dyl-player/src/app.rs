//! winit 事件循环、host compositor 接线与播放控制。
//!
//! 线程约定：
//! - 引擎在 pipeline 线程里提交帧，经 `ShellSink` 进入本线程，GPU 只在这里碰；
//! - 所有命令都走 `Engine::command`，本模块不直接改引擎状态；
//! - 关闭顺序是 engine.destroy() → 释放 Surface → 释放窗口。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandEnvelope, CommandOp, CommandStatus, Event, EventEnvelope, PlayerState, SeekMode,
};
use dyl_engine::{Engine, HostFrameSink, VideoFrame};
use dyl_render::{OutputStats, FRAME_QUEUE_CAPACITY};
use parking_lot::Mutex;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::gpu::GpuContext;
use crate::ui::{self, UiAction, UiState};

/// 内核 → shell 的唯一唤醒信号。主循环被唤醒后自己读快照，事件不携带状态。
#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    Wake,
}

pub struct Startup {
    pub video: Option<PathBuf>,
    pub subtitle: Option<PathBuf>,
    pub danmaku: Option<PathBuf>,
    pub autoplay: bool,
}

/// pipeline 线程 → shell 主线程的有界帧队列。
///
/// 只搬运 CPU 帧，不持有任何 GPU 资源；满时丢最旧帧并计入共享统计。
struct ShellSink {
    frames: Arc<Mutex<VecDeque<VideoFrame>>>,
    proxy: EventLoopProxy<UserEvent>,
    stats: Arc<OutputStats>,
}

impl HostFrameSink for ShellSink {
    fn push_frame(&self, frame: VideoFrame) {
        let mut frames = self.frames.lock();
        if frames.len() >= FRAME_QUEUE_CAPACITY {
            frames.pop_front();
            self.stats.record_dropped();
        }
        frames.push_back(frame);
    }

    fn request_redraw(&self) {
        let _ = self.proxy.send_event(UserEvent::Wake);
    }
}

pub struct App {
    engine: Option<Engine>,
    events: Receiver<EventEnvelope>,
    frames: Arc<Mutex<VecDeque<VideoFrame>>>,
    window: Option<Arc<Window>>,
    gpu: Option<GpuContext>,
    ui: UiState,
    startup: Option<Startup>,
    autoplay: bool,
    pending_subtitle: Option<PathBuf>,
    pending_danmaku: Option<PathBuf>,
    occluded: bool,
    /// 引擎事件轮询的下一次唤醒：事件没有异步唤醒，最长 100ms 主动取一次
    poll_due: Instant,
    /// egui 自己要求的下一次重绘（弹层淡入、悬停渐变）。`None` 表示没有动画在跑，
    /// 不能因为到了轮询时间就顺手重绘，否则暂停后会变成常驻 present。
    egui_wake: Option<Instant>,
    /// 本构建是否实测可用硬件解码；为假时 shell 不摆解码菜单（`auto` 会被同步拒绝）
    hw_decode: bool,
}

const ENGINE_POLL: Duration = Duration::from_millis(100);

impl App {
    pub fn new(mut startup: Startup, proxy: EventLoopProxy<UserEvent>) -> Result<Self, String> {
        let (engine, events) = Engine::new().map_err(|e| format!("create engine: {e}"))?;
        let frames = Arc::new(Mutex::new(VecDeque::with_capacity(FRAME_QUEUE_CAPACITY)));
        let sink = Arc::new(ShellSink {
            frames: frames.clone(),
            proxy,
            stats: engine.output_stats(),
        });
        engine.set_host_frame_sink(sink);

        let mut ui = UiState::new();
        if let Some(path) = &startup.video {
            ui.path_input = path.display().to_string();
        }
        ui.set_status("拖入 MP4 / ASS / XML，或输入本地路径后点击「加载」");

        Ok(Self {
            autoplay: startup.autoplay,
            pending_subtitle: startup.subtitle.take(),
            pending_danmaku: startup.danmaku.take(),
            startup: Some(startup),
            engine: Some(engine),
            events,
            frames,
            window: None,
            gpu: None,
            ui,
            occluded: false,
            poll_due: Instant::now(),
            egui_wake: None,
            hw_decode: dyl_engine::capabilities::capabilities().features.hw_decode,
        })
    }

    fn send(&mut self, command: Command) {
        let result = {
            let Some(engine) = self.engine.as_ref() else {
                return;
            };
            engine.command(CommandEnvelope::new(command))
        };
        match result {
            Ok(request_id) => tracing::debug!("shell command accepted: request_id={request_id}"),
            Err(error) => self.ui.set_status(format!("命令被拒绝: {error}")),
        }
    }

    fn send_seek_relative(&mut self, delta_ms: i64) {
        self.send(Command::SeekRelative {
            delta_ms,
            mode: SeekMode::Exact,
        });
    }

    fn load_video(&mut self, path: PathBuf) {
        if !path.is_file() {
            self.ui
                .set_status(format!("文件不存在: {}", path.display()));
            return;
        }
        let path = path.canonicalize().unwrap_or(path);
        self.ui.path_input = path.display().to_string();
        self.send(Command::LoadFile {
            url: path.display().to_string(),
        });
    }

    fn queue_or_load_overlay(&mut self, path: PathBuf, subtitle: bool) {
        let path = path.canonicalize().unwrap_or(path);
        let state = self.engine.as_ref().map(|engine| engine.snapshot().state);
        let accepted = matches!(
            state,
            Some(
                PlayerState::Ready
                    | PlayerState::Playing
                    | PlayerState::Paused
                    | PlayerState::Ended
                    | PlayerState::Stopped
            )
        );
        if accepted {
            if subtitle {
                self.send(Command::LoadSubtitle {
                    path: path.display().to_string(),
                });
            } else {
                self.send(Command::LoadDanmaku {
                    path: path.display().to_string(),
                });
            }
        } else {
            if subtitle {
                self.pending_subtitle = Some(path);
                self.ui.set_status("字幕已排队，视频就绪后加载");
            } else {
                self.pending_danmaku = Some(path);
                self.ui.set_status("弹幕已排队，视频就绪后加载");
            }
        }
    }

    fn handle_dropped(&mut self, path: PathBuf) {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "srt" | "ass" | "ssa" => self.queue_or_load_overlay(path, true),
            "xml" => self.queue_or_load_overlay(path, false),
            _ => self.load_video(path),
        }
    }

    fn apply_pending_overlays(&mut self) {
        if let Some(path) = self.pending_subtitle.take() {
            self.send(Command::LoadSubtitle {
                path: path.display().to_string(),
            });
        }
        if let Some(path) = self.pending_danmaku.take() {
            self.send(Command::LoadDanmaku {
                path: path.display().to_string(),
            });
        }
    }

    fn handle_engine_event(&mut self, envelope: EventEnvelope) {
        match envelope.event {
            Event::StateChanged(change) => {
                if change.to == PlayerState::Ready {
                    // 外挂字幕/弹幕只能在 ready 之后加载
                    self.apply_pending_overlays();
                    if self.autoplay {
                        self.send(Command::Play);
                    }
                }
            }
            Event::FileLoaded(loaded) => {
                self.ui.set_status(format!(
                    "已加载 {}（时长 {}）",
                    short_name(&loaded.source),
                    ui::format_time(loaded.duration_ms)
                ));
            }
            Event::SeekCompleted(_) => {
                self.ui.seek_preview_ms = None;
            }
            Event::Warning(warning) => {
                self.ui.set_status(format!("警告: {}", warning.message));
            }
            Event::Error(error) => {
                self.ui.set_status(format!("错误: {}", error.message));
            }
            Event::CommandCompleted(done) if done.status == CommandStatus::Error => {
                let code = done
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str())
                    .unwrap_or("internal");
                self.ui
                    .set_status(format!("{} 失败: {code}", op_label(done.op)));
            }
            _ => {}
        }
    }

    fn toggle_fullscreen(&mut self, window: &Window) {
        self.ui.fullscreen = !self.ui.fullscreen;
        window.set_fullscreen(if self.ui.fullscreen {
            Some(Fullscreen::Borderless(None))
        } else {
            None
        });
    }

    fn handle_key(&mut self, window: &Window, event: &winit::event::KeyEvent) {
        if event.repeat {
            return;
        }
        let snapshot = self.engine.as_ref().map(|engine| engine.snapshot());
        let state = snapshot.as_ref().map(|snapshot| snapshot.state);

        match &event.logical_key {
            Key::Named(NamedKey::Space) => match state {
                Some(PlayerState::Playing) => self.send(Command::Pause),
                Some(
                    PlayerState::Ready
                    | PlayerState::Paused
                    | PlayerState::Ended
                    | PlayerState::Stopped,
                ) => self.send(Command::Play),
                _ => {}
            },
            Key::Named(NamedKey::ArrowLeft) => self.send_seek_relative(-5_000),
            Key::Named(NamedKey::ArrowRight) => self.send_seek_relative(5_000),
            Key::Named(NamedKey::Escape) => {
                // 非全屏的 Esc 在 window_event 里直接退出，这里只负责退出全屏
                if self.ui.fullscreen {
                    self.toggle_fullscreen(window);
                }
            }
            Key::Character(text) => match text.as_str() {
                "f" | "F" => self.toggle_fullscreen(window),
                "h" | "H" => self.ui.show_help = !self.ui.show_help,
                "m" | "M" => {
                    let muted = snapshot.as_ref().map(|s| s.muted).unwrap_or(false);
                    self.send(Command::SetMute { muted: !muted });
                }
                _ => {}
            },
            _ => {}
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attributes = Window::default_attributes()
            .with_title("Dyl Player")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(480.0, 270.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                tracing::error!("create window failed: {error}");
                event_loop.exit();
                return;
            }
        };

        let Some(engine) = self.engine.as_ref() else {
            event_loop.exit();
            return;
        };
        let gpu = match GpuContext::new(
            window.clone(),
            engine.host_output_handles(),
            engine.overlay_handles(),
        ) {
            Ok(gpu) => gpu,
            Err(error) => {
                tracing::error!("GPU init failed: {error}");
                event_loop.exit();
                return;
            }
        };

        self.window = Some(window);
        self.gpu = Some(gpu);
        if let Some(window) = &self.window {
            // 首帧必须先清一次：否则在视频加载前窗口内容未定义
            window.request_redraw();
        }

        let Some(startup) = self.startup.take() else {
            return;
        };
        if let Some(video) = startup.video {
            self.load_video(video);
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: UserEvent) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.clone() else {
            return;
        };

        // egui 先处理：文本框焦点、拖拽、滚轮等由它决定是否独占
        let (consumed, repaint) = match self.gpu.as_mut() {
            Some(gpu) if egui_handles(&event) => {
                let response = gpu.egui_state.on_window_event(&window, &event);
                (response.consumed, response.repaint)
            }
            _ => (false, false),
        };
        if repaint {
            window.request_redraw();
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                }
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.invalidate();
                }
                window.request_redraw();
            }
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                if !occluded {
                    window.request_redraw();
                }
            }
            WindowEvent::DroppedFile(path) => self.handle_dropped(path),
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !consumed =>
            {
                if event.logical_key == Key::Named(NamedKey::Escape) && !self.ui.fullscreen {
                    event_loop.exit();
                } else {
                    self.handle_key(&window, &event);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let mut received = false;
        while let Ok(envelope) = self.events.try_recv() {
            received = true;
            self.handle_engine_event(envelope);
        }
        let Some(window) = self.window.clone() else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };
        if received {
            window.request_redraw();
        }

        let now = Instant::now();
        let playing = self
            .engine
            .as_ref()
            .map(|engine| engine.snapshot().state == PlayerState::Playing)
            .unwrap_or(false);
        let visible = self
            .gpu
            .as_ref()
            .and_then(|gpu| gpu.render_size())
            .is_some();

        if playing && visible && !self.occluded {
            // FIFO present 自己会阻塞到下一个 vblank，这里只负责持续排下一帧
            window.request_redraw();
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }

        // egui 主动要的帧（弹层淡入、悬停渐变）到点就真的画一次。render() 会重新记账，
        // 动画跑完就不再请求，所以暂停/EOF 后不会变成常驻 present。
        if self.egui_wake.is_some_and(|due| due <= now) {
            self.egui_wake = None;
            window.request_redraw();
        }

        // 引擎事件没有异步唤醒：每 100ms 主动取一次，取完就把下次推到未来，
        // 否则 deadline 一直落在过去，事件循环退化成 1ms 轮询。
        if now >= self.poll_due {
            self.poll_due = now + ENGINE_POLL;
        }
        let deadline = self
            .poll_due
            .min(self.egui_wake.unwrap_or(self.poll_due))
            .max(now + Duration::from_millis(1));
        event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // 先停内核：control 线程 join 之后不会再有帧进入 sink
        if let Some(mut engine) = self.engine.take() {
            engine.destroy();
        }
        // Surface 必须先于窗口句柄释放
        self.gpu = None;
        self.window = None;
        self.frames.lock().clear();
    }
}

impl App {
    fn render(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(snapshot) = self.engine.as_ref().map(|engine| engine.snapshot()) else {
            return;
        };
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        let Some((_surface_width, _surface_height)) = gpu.render_size() else {
            return;
        };

        // 1. pipeline 帧 → compositor（时钟已在 HostVideoOutput 中锚定）
        {
            let mut frames = self.frames.lock();
            while let Some(frame) = frames.pop_front() {
                if let Err(error) = gpu.compositor.enqueue_frame(frame) {
                    // ADR-14：超限帧不进队列。同一尺寸会连续失败，只在第一次写
                    // HUD 与日志，其余静默丢弃
                    let message = format!("错误: {error}");
                    if self.ui.status != message {
                        tracing::error!("host compositor rejected frame: {error}");
                        self.ui.set_status(message);
                    }
                    frames.clear();
                    break;
                }
            }
        }

        // 2. 渲染帧率心跳：与画面上屏同步，暂停后自然归零
        let now = Instant::now();
        let stats = gpu.compositor.stats();
        let elapsed = now.saturating_duration_since(self.ui.stats_at);
        if elapsed >= Duration::from_millis(500) {
            let delta = stats.rendered_frames.saturating_sub(self.ui.stats_frames);
            self.ui.rfps = delta as f32 / elapsed.as_secs_f32();
            self.ui.stats_frames = stats.rendered_frames;
            self.ui.stats_at = now;
        }

        // 3. egui：只读快照，改动通过 actions 回到引擎
        let device = gpu.device().clone();
        let queue = gpu.queue().clone();
        let raw_input = gpu.egui_state.take_egui_input(&window);
        let mut actions = Vec::new();
        let hw_decode = self.hw_decode;
        let output = {
            let ui_state = &mut self.ui;
            gpu.egui_ctx.run_ui(raw_input, |ui| {
                ui::draw(ui, &snapshot, hw_decode, ui_state, &mut actions);
            })
        };
        let pixels_per_point = output.pixels_per_point;
        let paint_jobs = gpu
            .egui_ctx
            .tessellate(output.shapes, output.pixels_per_point);
        gpu.egui_state
            .handle_platform_output(&window, output.platform_output);
        for (id, delta) in &output.textures_delta.set {
            gpu.egui_renderer
                .update_texture(&device, &queue, *id, delta);
        }

        // 4. 合成 + 上屏
        present(gpu, &paint_jobs, pixels_per_point);
        for id in &output.textures_delta.free {
            gpu.egui_renderer.free_texture(id);
        }

        // 5. egui 主动要求重绘（弹层淡入、悬停渐变）才记一次到点时间；
        //    没有动画时 repaint_delay 是 MAX，不记，暂停/EOF 后就完全不重绘
        let mut repaint_delay = Duration::MAX;
        for viewport in output.viewport_output.values() {
            repaint_delay = repaint_delay.min(viewport.repaint_delay);
        }
        self.egui_wake = (repaint_delay < Duration::MAX).then(|| Instant::now() + repaint_delay);

        for action in actions {
            match action {
                UiAction::Command(command) => self.send(command),
                UiAction::ToggleFullscreen => self.toggle_fullscreen(&window),
                UiAction::SetTheme(theme) => {
                    // 暂停/EOF 时没有下一帧，换主题必须主动重绘一次，
                    // 否则面板还停在上一个主题的配色。
                    if let Some(gpu) = self.gpu.as_ref() {
                        gpu.egui_ctx.set_theme(theme);
                    }
                    window.request_redraw();
                }
            }
        }

        if snapshot.state == PlayerState::Playing {
            window.request_redraw();
        }
    }
}

fn present(gpu: &mut GpuContext, paint_jobs: &[egui::ClippedPrimitive], ppp: f32) {
    let Some(texture) = gpu.acquire() else {
        return;
    };
    let device = gpu.device().clone();
    let queue = gpu.queue().clone();

    let surface_width = texture.texture.width();
    let surface_height = texture.texture.height();
    // DPR 给叠加层：弹幕的固定参考字号按逻辑像素定义，上屏要乘它
    let area = gpu.overlay_area(surface_width, surface_height, ppp);
    let view = texture
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("dyl_player_frame"),
    });

    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [surface_width, surface_height],
        pixels_per_point: ppp,
    };
    let user_commands =
        gpu.egui_renderer
            .update_buffers(&device, &queue, &mut encoder, paint_jobs, &screen);

    // 没有新帧时也重画上一帧：egui 的透明区域不能露出未初始化的 swapchain。
    // invalidate 让 render_tick 用 last_frame + 当前挂钟重绘，而不是返回 false。
    if gpu.compositor.has_frame() {
        gpu.compositor.invalidate();
    }
    let composited = gpu.compositor.render_tick(&mut encoder, &view, area);

    if !composited {
        let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("dyl_player_clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
    }

    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("dyl_player_egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            })
            .forget_lifetime();
        gpu.egui_renderer.render(&mut pass, paint_jobs, &screen);
    }

    queue.submit(
        user_commands
            .into_iter()
            .chain(std::iter::once(encoder.finish())),
    );
    texture.present();
}

/// `RedrawRequested` 是输出事件，不是输入事件。喂给 egui-winit 会得到
/// `repaint=true`，与本壳末尾的 `request_redraw` 组成 60fps 自激循环，
/// 暂停/EOF 后也不会停。
fn egui_handles(event: &WindowEvent) -> bool {
    !matches!(event, WindowEvent::RedrawRequested)
}

fn op_label(op: CommandOp) -> &'static str {
    op.as_str()
}

fn short_name(source: &str) -> String {
    std::path::Path::new(source)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redraw_requested_is_not_forwarded_to_egui() {
        assert!(!egui_handles(&WindowEvent::RedrawRequested));
        assert!(egui_handles(&WindowEvent::Focused(true)));
    }
}
