//! ExternalRenderer — 外部 Surface 注入渲染器（Android / 嵌入式平台）
//!
//! `ExternalRenderer` 接受外部传入的平台原生 Surface 句柄（如 Android
//! `ANativeWindow*`），通过 wgpu 创建 GPU surface 进行渲染；它从不创建窗口。
//!
//! 使用场景：
//! - Android: Flutter `SurfaceView` 提供 `ANativeWindow*`，通过 FFI 传给 Rust
//! - 嵌入式: 任何提供原生窗口句柄的平台
//!
//! 渲染线程独立运行，通过 channel 接收命令。Surface 可以在运行时 detach/reattach，
//! 支持 Android Activity 生命周期（onPause 时 Surface 销毁，onResume 时重建）。

use crate::clock::OverlayClock;
use crate::compositor::{frame_size_error, FrameCompositor, FRAME_QUEUE_CAPACITY};
use crate::overlay::OverlayArea;
use crate::video_output::{NativeSurfaceKind, OutputStats, OutputStatsSnapshot, SurfaceHandle};
use dyl_core::Result;
use dyl_ffmpeg::VideoFrame;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use super::video_output::VideoOutput;

/// 无 vsync 平台上的兜底渲染周期（60Hz）
const EXT_FALLBACK_FRAME_PERIOD: Duration = Duration::from_micros(16_667);
/// FIFO（vsync）模式下的轮询预算：节拍由 present 阻塞决定，此值只保证命令响应性
const EXT_VSYNC_POLL_BUDGET: Duration = Duration::from_millis(4);

// ─── Render thread commands ──────────────────────────────────────

enum ExtCmd {
    /// 替换 Overlay 栈
    SetOverlays(Vec<Box<dyn crate::overlay::Overlay>>),
    /// 分离 Surface（Surface 被销毁）— ack 回传"原生窗口引用已释放"
    DetachSurface(AckTx),
    /// 调整 swapchain 尺寸（屏幕旋转 / 窗口缩放）
    ResizeSurface { width: u32, height: u32, ack: AckTx },
    /// 强制把上一帧重新合成并 present（宿主窗口内容被系统擦除）
    Redraw(AckTx),
    /// 关闭渲染线程
    Shutdown,
}

type AckTx = mpsc::Sender<std::result::Result<(), String>>;

/// Surface 操作确认超时
///
/// 宿主（Android `surfaceDestroyed`）在 FFI 返回后会立即释放 `ANativeWindow`，
/// 因此 detach 必须等渲染线程真正 drop 掉 `wgpu::Surface` 才能返回成功；
/// 拿不到确认时必须报错，不能静默 Ok。
const SURFACE_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn wait_surface_ack(
    ack_rx: mpsc::Receiver<std::result::Result<(), String>>,
    what: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    match ack_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(dyl_core::DylError::Render(format!("{what}: {e}"))),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(dyl_core::DylError::Render(format!(
            "{what}: render thread did not acknowledge within {timeout:?}"
        ))),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(dyl_core::DylError::Render(format!(
            "{what}: render thread exited before acknowledging"
        ))),
    }
}

// ─── Internal render state ───────────────────────────────────────

/// 选 swapchain / 管线目标格式：优先非 sRGB
///
/// YUV→RGB shader 输出的数值已经躺在 gamma 空间里（视频采样本身就是编码后的值），
/// 写进 `*Srgb` 目标会让硬件再做一次线性→sRGB 编码，画面整体发白、饱和度过低。
/// GLES 后端的 Windows/Linux 把 `Rgba8UnormSrgb` 排在列表首位，所以不能直接取
/// `formats.first()`；与宿主通路 `dyl-player` 的选择保持同一判据。
fn pick_surface_format(caps: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    caps.formats
        .iter()
        .copied()
        .find(|format| !format.is_srgb())
        .unwrap_or(wgpu::TextureFormat::Bgra8Unorm)
}

/// external 路径请求的设备上限。
///
/// 保留 `downlevel_defaults()` 的特性与尺寸基线 —— GLES 后端要求的最小能力集（含
/// `R8Unorm`/`Rg8Unorm` 可绑定），但把**纹理分辨率**上调到所选 adapter 的真实能力：
/// `downlevel_defaults()` 的 `max_texture_dimension_2d` 是 2048，照原样请求会让 1080p
/// 及以上的 Y 平面创建失败，使这条路径凭空多出一个 host_compositor 没有的帧尺寸天花板
/// （规范 ADR-14：两条呈现路径的帧尺寸上限必须同源）。
///
/// 分辨率是唯一被放开的维度，因此能力 JSON 里的 `max_texture_dimension_2d` 表达的是
/// 「本构建保证的下界」，不是产品上限。
///
/// 参数是 `wgpu::Adapter::limits()` 的返回值；写成纯函数是为了能在没有 GPU 的测试里
/// 断言这条推导。
pub(crate) fn device_limits(adapter_limits: wgpu::Limits) -> wgpu::Limits {
    wgpu::Limits::downlevel_defaults().using_resolution(adapter_limits)
}

/// swapchain 尺寸落在设备纹理上限内：窗口可以比任何一帧视频大，超大尺寸夹紧即可，
/// 不该因为拖大窗口而让 attach/resize 报错。
fn clamp_to_limits(width: u32, height: u32, max_dimension: u32) -> (u32, u32) {
    (
        width.clamp(1, max_dimension),
        height.clamp(1, max_dimension),
    )
}

struct ExtRenderState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// 当前 attached 的平台 Surface；detach 时必须在本线程同步 drop，
    /// 否则它会继续持有宿主已销毁的 `ANativeWindow`
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<wgpu::SurfaceConfiguration>,
    /// YUV 上传、叠加层与帧队列；GPU 资源只在本渲染线程访问
    compositor: FrameCompositor,
    /// 目标渲染周期（仅在无 vsync 阻塞的软件节流路径下生效）
    frame_period: Duration,
    /// present 是否由 vsync 阻塞（FIFO 下上屏速率由显示器决定）
    vsync_locked: bool,
    /// 宿主窗口 DPR，随 `OverlayArea` 交给叠加层（弹幕的固定参考字号要乘它）
    scale_factor: f32,
}

impl ExtRenderState {
    /// 从 SurfaceHandle 创建完整的渲染状态
    fn new(
        handle: &SurfaceHandle,
        stats: Arc<OutputStats>,
        overlay_clock: Arc<OverlayClock>,
        frame_ceiling: &AtomicU32,
    ) -> std::result::Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });

        let surface = unsafe { Self::create_surface_from_handle(&instance, handle)? };

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|_| "no suitable GPU adapter".to_string())?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("dyl_ext_device"),
            required_features: wgpu::Features::empty(),
            required_limits: device_limits(adapter.limits()),
            experimental_features: Default::default(),
            memory_hints: Default::default(),
            trace: Default::default(),
        }))
        .map_err(|e| format!("device: {e}"))?;

        // 必须在任何渲染调用之前装，否则首个未捕获错误会直接 abort 掉进程
        crate::wgpu_instance::install_nonfatal_error_handler(&device);

        let caps = surface.get_capabilities(&adapter);
        let max_dimension = device.limits().max_texture_dimension_2d;
        // 提交线程靠它拒绝放不下的帧（ADR-14）；必须在 ready 发出之前写好
        frame_ceiling.store(max_dimension, Ordering::Release);
        let surface_format = pick_surface_format(&caps);
        let surface_config = Self::make_surface_config(&caps, handle, max_dimension);
        surface.configure(&device, &surface_config);

        let compositor =
            FrameCompositor::new(&device, &queue, surface_format, overlay_clock, stats);

        let vsync_locked = surface_config.present_mode == wgpu::PresentMode::Fifo;

        tracing::info!(
            "ExtRenderState initialized: {}x{}, format={:?}, present={:?}, max_texture_2d={}",
            handle.width,
            handle.height,
            surface_format,
            surface_config.present_mode,
            max_dimension
        );

        Ok(Self {
            device,
            queue,
            surface: Some(surface),
            surface_config: Some(surface_config),
            compositor,
            frame_period: EXT_FALLBACK_FRAME_PERIOD,
            vsync_locked,
            scale_factor: handle.scale_factor,
        })
    }

    fn make_surface_config(
        caps: &wgpu::SurfaceCapabilities,
        handle: &SurfaceHandle,
        max_dimension: u32,
    ) -> wgpu::SurfaceConfiguration {
        // 优先 FIFO：present 阻塞到 vblank，叠加层动画自然对齐显示器刷新率
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
            wgpu::PresentMode::Fifo
        } else {
            wgpu::PresentMode::Mailbox
        };
        let (width, height) = clamp_to_limits(handle.width, handle.height, max_dimension);
        wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: pick_surface_format(caps),
            width,
            height,
            present_mode,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            // 留 1 个额外在飞帧，让下一帧的编码/提交与本帧扫描输出重叠，
            // FIFO 锁定下不会引入撕裂
            desired_maximum_frame_latency: 2,
        }
    }

    /// 释放 Surface — 在渲染线程上同步 drop，归还平台原生窗口引用
    ///
    /// 先等待在途提交结束，避免 swapchain 纹理仍被 GPU 引用时窗口已销毁。
    fn detach_surface(&mut self) {
        if self.surface.is_none() {
            return;
        }
        let _ = self.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_millis(500)),
        });
        self.surface = None;
        self.surface_config = None;
        self.compositor.clear_frame();
    }

    fn reconfigure_surface(&mut self) {
        if let (Some(surface), Some(config)) = (&self.surface, &self.surface_config) {
            surface.configure(&self.device, config);
        }
    }

    /// 调整 swapchain 尺寸并立即 configure；返回是否真的重建了 Surface。
    fn resize_surface(&mut self, width: u32, height: u32) -> std::result::Result<(), String> {
        let Some(config) = self.surface_config.as_mut() else {
            return Err("surface not attached".to_string());
        };
        let max_dimension = self.device.limits().max_texture_dimension_2d;
        let (width, height) = clamp_to_limits(width, height, max_dimension);
        if config.width == width && config.height == height {
            return Ok(());
        }
        config.width = width;
        config.height = height;
        self.reconfigure_surface();
        Ok(())
    }

    /// 从平台原生 SurfaceHandle 创建 wgpu Surface
    ///
    /// # Safety
    /// `handle` 中的原生窗口/显示指针必须在本 Surface 生命周期内保持有效。
    unsafe fn create_surface_from_handle(
        instance: &wgpu::Instance,
        handle: &SurfaceHandle,
    ) -> std::result::Result<wgpu::Surface<'static>, String> {
        use raw_window_handle::{
            AndroidDisplayHandle, AndroidNdkWindowHandle, AppKitDisplayHandle, AppKitWindowHandle,
            RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
            Win32WindowHandle, WindowsDisplayHandle, XlibDisplayHandle, XlibWindowHandle,
        };
        use std::num::NonZeroIsize;
        use std::ptr::NonNull;

        let window = NonNull::new(handle.native_window as *mut std::ffi::c_void)
            .ok_or_else(|| "native window pointer is null".to_string())?;
        let display = NonNull::new(handle.native_display as *mut std::ffi::c_void);

        let (raw_window, raw_display) = match handle.kind {
            NativeSurfaceKind::AndroidNativeWindow => (
                RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(window)),
                RawDisplayHandle::Android(AndroidDisplayHandle::new()),
            ),
            NativeSurfaceKind::WindowsHwnd => {
                let hwnd = NonZeroIsize::new(handle.native_window as isize)
                    .ok_or_else(|| "HWND is null".to_string())?;
                (
                    RawWindowHandle::Win32(Win32WindowHandle::new(hwnd)),
                    RawDisplayHandle::Windows(WindowsDisplayHandle::new()),
                )
            }
            NativeSurfaceKind::MacosNsview => (
                RawWindowHandle::AppKit(AppKitWindowHandle::new(window)),
                RawDisplayHandle::AppKit(AppKitDisplayHandle::new()),
            ),
            NativeSurfaceKind::X11Window => (
                RawWindowHandle::Xlib(XlibWindowHandle::new(handle.native_window as _)),
                RawDisplayHandle::Xlib(XlibDisplayHandle::new(display, 0)),
            ),
            NativeSurfaceKind::WaylandSurface => {
                let display =
                    display.ok_or_else(|| "Wayland display pointer is null".to_string())?;
                (
                    RawWindowHandle::Wayland(WaylandWindowHandle::new(window)),
                    RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display)),
                )
            }
        };

        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(raw_display),
            raw_window_handle: raw_window,
        };

        instance
            .create_surface_unsafe(target)
            .map_err(|e| format!("create surface: {e}"))
    }

    /// 一个渲染节拍：有新帧或叠加层推进时 acquire → 合成 → present
    fn render_tick(&mut self) {
        if self.surface.is_none() || !self.compositor.needs_present() {
            return;
        }
        self.present_frame();
    }

    /// 获取 surface 纹理 → FrameCompositor 合成 → present
    fn present_frame(&mut self) {
        let Some(surface) = self.surface.as_ref() else {
            // Surface 已 detach：没有原生窗口可写
            return;
        };
        let surface_texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.reconfigure_surface();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                tracing::error!("ExternalRenderer: GPU surface lost");
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => {
                tracing::warn!("ExternalRenderer: surface texture unavailable");
                return;
            }
        };

        let surf_w = surface_texture.texture.width();
        let surf_h = surface_texture.texture.height();
        let (video_w, video_h) = self.compositor.video_size();
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        if !self.compositor.render_tick(
            &mut encoder,
            &view,
            OverlayArea::new(surf_w, surf_h, video_w, video_h).with_scale_factor(self.scale_factor),
        ) {
            return;
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();
    }
}

// ─── Render thread loop ──────────────────────────────────────────

// 线程入口：帧/命令/ready 三个通道 + 初始句柄 + 四份与提交线程共享的原子状态
#[allow(clippy::too_many_arguments)]
fn ext_render_loop(
    frame_rx: mpsc::Receiver<VideoFrame>,
    ctrl_rx: mpsc::Receiver<ExtCmd>,
    ready_tx: mpsc::Sender<std::result::Result<(), String>>,
    initial_handle: SurfaceHandle,
    stats: Arc<OutputStats>,
    overlay_clock: Arc<OverlayClock>,
    surface_attached: Arc<AtomicBool>,
    frame_ceiling: Arc<AtomicU32>,
) {
    // 创建初始渲染状态
    let mut state = match ExtRenderState::new(&initial_handle, stats, overlay_clock, &frame_ceiling)
    {
        Ok(s) => {
            let _ = ready_tx.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e.clone()));
            tracing::error!("ExtRenderState init failed: {e}");
            return;
        }
    };

    surface_attached.store(true, Ordering::Release);
    tracing::info!("ExternalRenderer: render thread started");

    let mut next_tick = Instant::now();
    let mut last_stats = Instant::now();
    let mut stats_frames = 0u64;
    'outer: loop {
        // 控制命令优先于视频帧处理：detach 必须抢在积压帧之前生效，
        // 否则会在宿主即将销毁的原生窗口上继续上屏
        loop {
            match ctrl_rx.try_recv() {
                Ok(ExtCmd::SetOverlays(overlays)) => {
                    state.compositor.set_overlays(overlays);
                    tracing::info!("ExternalRenderer: overlay stack updated");
                    // 叠加层内容变了但挂钟可能没推进（如暂停中加载弹幕）：立即重绘一次
                    if state.compositor.has_frame() && state.surface.is_some() {
                        state.render_tick();
                    }
                }
                Ok(ExtCmd::DetachSurface(ack)) => {
                    tracing::info!("ExternalRenderer: detaching surface");
                    state.detach_surface();
                    surface_attached.store(false, Ordering::Release);
                    let _ = ack.send(Ok(()));
                }
                Ok(ExtCmd::ResizeSurface { width, height, ack }) => {
                    let result = state.resize_surface(width, height);
                    if let Err(e) = &result {
                        tracing::error!("ExternalRenderer: resize failed: {e}");
                    }
                    let _ = ack.send(result);
                }
                Ok(ExtCmd::Redraw(ack)) => {
                    // invalidate 让 render_tick 绕过"静止不 present"的判定：
                    // 暂停/EOF 后 last_pts 与冻结挂钟相同，正常路径会直接跳过。
                    state.compositor.invalidate();
                    state.render_tick();
                    let _ = ack.send(Ok(()));
                }
                Ok(ExtCmd::Shutdown) => {
                    tracing::info!("ExternalRenderer: shutdown requested");
                    break 'outer;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    tracing::info!("ExternalRenderer: control channel closed");
                    break 'outer;
                }
            }
        }

        // FIFO 下 present 自身阻塞到下一个 vblank，节拍由显示器决定；
        // 无 vsync 的后端按 60Hz 兜底做软件节流
        let wait = if state.vsync_locked {
            EXT_VSYNC_POLL_BUDGET
        } else {
            next_tick.saturating_duration_since(Instant::now())
        };
        match frame_rx.recv_timeout(wait) {
            Ok(frame) => {
                if let Err(e) = state.compositor.enqueue_frame(frame) {
                    tracing::error!("ExternalRenderer: frame rejected: {e}");
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::info!("ExternalRenderer: frame channel closed");
                break;
            }
        }

        // 命令在节拍到期前到达时不占用本节拍，但 pending 已满时必须让出一次上屏，
        // 否则片源帧率高于刷新率会把渲染饿死
        if !state.vsync_locked
            && Instant::now() < next_tick
            && state.compositor.pending_len() < FRAME_QUEUE_CAPACITY
        {
            continue;
        }

        let frames_before = state.compositor.stats().rendered_frames;
        state.render_tick();
        stats_frames += state.compositor.stats().rendered_frames - frames_before;

        // 上屏帧率心跳：真机上没有别的方式能确认渲染线程真的在 present
        // （SurfaceView 在别的 layer 上，screencap 也不一定抓得到）。
        if last_stats.elapsed() >= Duration::from_secs(2) {
            let elapsed = last_stats.elapsed().as_secs_f64();
            tracing::info!(
                "ExternalRenderer: presenting {:.1} fps (total {}), pending={}, surface={}",
                stats_frames as f64 / elapsed,
                state.compositor.stats().rendered_frames,
                state.compositor.pending_len(),
                if state.surface.is_some() {
                    "attached"
                } else {
                    "detached"
                },
            );
            stats_frames = 0;
            last_stats = Instant::now();
        }

        if !state.vsync_locked {
            next_tick += state.frame_period;
            let now = Instant::now();
            if next_tick < now {
                next_tick = now;
            }
        }
    }

    // 线程退出前归还原生窗口引用，避免 Surface 晚于宿主窗口销毁
    state.detach_surface();
    surface_attached.store(false, Ordering::Release);
    tracing::info!("ExternalRenderer: render thread exiting");
}

// ─── Public API ──────────────────────────────────────────────────

/// ExternalRenderer — 外部 Surface 注入渲染器
///
/// 用于 Android 等嵌入式平台，宿主应用（Flutter）提供原生 Surface，
/// ExternalRenderer 在其上创建 wgpu surface 进行视频渲染。
///
/// 特点：
/// - 不创建自己的窗口（无 winit 依赖）
/// - Surface 可在运行时 detach/reattach（支持 Activity 生命周期）
/// - 使用 `SurfaceHandle` 而非 winit Window
pub struct ExternalRenderer {
    width: u32,
    height: u32,
    /// 视频帧通道（bounded，满时丢帧做背压）
    frame_tx: Option<mpsc::SyncSender<VideoFrame>>,
    /// 控制通道（unbounded：detach/reattach 必须能立即入队，不被帧背压阻塞）
    ctrl_tx: Option<mpsc::Sender<ExtCmd>>,
    /// 渲染线程句柄，drop 时 join 以确保 Surface 在自身销毁前被释放
    render_thread: Option<std::thread::JoinHandle<()>>,
    /// 当前是否有有效的 Surface attached
    surface_attached: Arc<AtomicBool>,
    /// 渲染统计：与渲染线程共享同一份原子计数
    stats: Arc<OutputStats>,
    /// 叠加层挂钟时钟：本结构按上屏帧锚定，渲染线程按挂钟外推
    overlay_clock: Arc<OverlayClock>,
    /// 设备纹理边长上限，由渲染线程在创建 device 后写入（0 = 还没有设备）。
    /// 提交线程据此在入队前拒绝放不下的帧，让 ADR-14 的超限成为 `render` 错误
    frame_ceiling: Arc<AtomicU32>,
    initialized: bool,
}

unsafe impl Send for ExternalRenderer {}

impl ExternalRenderer {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            frame_tx: None,
            ctrl_tx: None,
            render_thread: None,
            surface_attached: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(OutputStats::new()),
            overlay_clock: Arc::new(OverlayClock::new()),
            frame_ceiling: Arc::new(AtomicU32::new(0)),
            initialized: false,
        }
    }

    /// 使用指定的 SurfaceHandle 初始化渲染器
    ///
    /// 创建渲染线程并等待 GPU 资源就绪。
    pub fn init_with_surface(&mut self, handle: SurfaceHandle) -> Result<()> {
        tracing::info!(
            "ExternalRenderer::init_with_surface({}x{}, native_window=0x{:x})",
            handle.width,
            handle.height,
            handle.native_window
        );

        self.width = handle.width;
        self.height = handle.height;

        let (frame_tx, frame_rx) = mpsc::sync_channel::<VideoFrame>(4);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<ExtCmd>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let surface_attached = Arc::clone(&self.surface_attached);
        let stats = Arc::clone(&self.stats);
        let overlay_clock = Arc::clone(&self.overlay_clock);
        let frame_ceiling = Arc::clone(&self.frame_ceiling);

        let thread = std::thread::Builder::new()
            .name("dyl-ext-render".into())
            .spawn(move || {
                ext_render_loop(
                    frame_rx,
                    ctrl_rx,
                    ready_tx,
                    handle,
                    stats,
                    overlay_clock,
                    surface_attached,
                    frame_ceiling,
                );
            })
            .map_err(|e| dyl_core::DylError::Internal(format!("spawn ext render thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.frame_tx = Some(frame_tx);
                self.ctrl_tx = Some(ctrl_tx);
                self.render_thread = Some(thread);
                self.initialized = true;
                tracing::info!("ExternalRenderer ready: {}x{}", self.width, self.height);
                Ok(())
            }
            Ok(Err(e)) => Err(dyl_core::DylError::Render(format!("ext render init: {e}"))),
            Err(_) => Err(dyl_core::DylError::Internal(
                "ext render thread failed to start".into(),
            )),
        }
    }
}

impl Default for ExternalRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoOutput for ExternalRenderer {
    fn submit_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        if !self.initialized || !self.surface_attached.load(Ordering::Acquire) {
            return Ok(());
        }
        // ADR-14：帧尺寸必须整张落进设备纹理。在这里就报错，pipeline 才能把它当
        // 呈现失败上报；留到渲染线程只会变成无声丢帧或 wgpu 建纹理失败
        if let Some(err) = frame_size_error(
            frame.width,
            frame.height,
            self.frame_ceiling.load(Ordering::Acquire),
        ) {
            return Err(err);
        }
        // 以本帧 PTS 重新锚定叠加层挂钟（无时间戳帧不覆盖，避免时钟被重置为 0）
        if let Some(secs) = frame.pts_secs() {
            self.overlay_clock.reanchor(secs);
        }
        if let Some(tx) = &self.frame_tx {
            match tx.try_send(frame.clone()) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    self.stats.record_dropped();
                    tracing::debug!("ExternalRenderer: frame channel full, dropping frame");
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    self.surface_attached.store(false, Ordering::Release);
                }
            }
        }
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.width = width;
        self.height = height;
        let Some(tx) = &self.ctrl_tx else {
            // 尚未 attach Surface：尺寸只影响后续 Surface 创建
            return Ok(());
        };
        let (ack_tx, ack_rx) = mpsc::channel();
        tx.send(ExtCmd::ResizeSurface {
            width,
            height,
            ack: ack_tx,
        })
        .map_err(|_| dyl_core::DylError::Render("resize surface: render thread exited".into()))?;
        wait_surface_ack(ack_rx, "ExternalRenderer::resize", SURFACE_ACK_TIMEOUT)
    }

    fn freeze_overlay_clock(&self) {
        self.overlay_clock.freeze();
    }

    /// 把上一帧重新 present 一次；宿主窗口被擦除后靠它恢复静止画面。
    fn redraw(&mut self) -> Result<()> {
        let Some(tx) = &self.ctrl_tx else {
            // 未 attach Surface：没有可上屏的目标
            return Ok(());
        };
        let (ack_tx, ack_rx) = mpsc::channel();
        tx.send(ExtCmd::Redraw(ack_tx))
            .map_err(|_| dyl_core::DylError::Render("redraw: render thread exited".into()))?;
        wait_surface_ack(ack_rx, "ExternalRenderer::redraw", SURFACE_ACK_TIMEOUT)
    }

    fn stats(&self) -> OutputStatsSnapshot {
        self.stats.snapshot()
    }

    /// 分离 Surface
    ///
    /// 同步语义：返回 Ok 表示渲染线程已经把 `wgpu::Surface`（及其持有的
    /// 原生窗口引用）释放完毕，宿主可以安全销毁原生窗口。
    fn detach(&mut self) -> Result<()> {
        tracing::info!("ExternalRenderer::detach");
        let Some(tx) = &self.ctrl_tx else {
            // 未初始化：没有 Surface 需要释放
            return Ok(());
        };
        let (ack_tx, ack_rx) = mpsc::channel();
        tx.send(ExtCmd::DetachSurface(ack_tx)).map_err(|_| {
            self.surface_attached.store(false, Ordering::Release);
            dyl_core::DylError::Render("detach surface: render thread exited".into())
        })?;
        wait_surface_ack(ack_rx, "ExternalRenderer::detach", SURFACE_ACK_TIMEOUT)
    }

    fn set_overlays(&mut self, overlays: Vec<Box<dyn crate::overlay::Overlay>>) {
        if let Some(tx) = &self.ctrl_tx {
            if let Err(e) = tx.send(ExtCmd::SetOverlays(overlays)) {
                tracing::error!("VideoOutput::set_overlays FAILED: {}", e);
            }
        } else {
            tracing::warn!("VideoOutput::set_overlays: not initialized, overlays dropped");
        }
    }
}

impl Drop for ExternalRenderer {
    fn drop(&mut self) {
        tracing::info!("ExternalRenderer::drop");
        // 先请求释放 Surface，再关闭线程：线程退出路径也会兜底 detach，
        // 但显式确认可以保证 join 返回前原生窗口引用已归还
        if let Some(tx) = self.ctrl_tx.take() {
            let (ack_tx, ack_rx) = mpsc::channel();
            if tx.send(ExtCmd::DetachSurface(ack_tx)).is_ok() {
                let _ = ack_rx.recv_timeout(SURFACE_ACK_TIMEOUT);
            }
            let _ = tx.send(ExtCmd::Shutdown);
        }
        self.frame_tx = None;
        if let Some(thread) = self.render_thread.take() {
            // 渲染线程持有 Surface，必须 join 到它真正退出
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_surface_handle_new() {
        let h = SurfaceHandle::android(0x1234, 1920, 1080);
        assert_eq!(h.kind, NativeSurfaceKind::AndroidNativeWindow);
        assert_eq!(h.native_window, 0x1234);
        assert_eq!(h.native_display, 0);
        assert_eq!(h.width, 1920);
        assert_eq!(h.height, 1080);
    }

    /// 回归 ADR-14：external 路径的帧尺寸上限来自 adapter，不再是 `downlevel_defaults()`
    /// 顺带带来的 2048 天花板。
    #[test]
    fn device_limits_adopts_adapter_resolution() {
        let adapter = wgpu::Limits::default();
        let limits = device_limits(adapter.clone());
        assert_eq!(
            limits.max_texture_dimension_2d, adapter.max_texture_dimension_2d,
            "adapter 能给更大的纹理时，external 路径不得把自己锁在 2048"
        );
        assert!(
            adapter.max_texture_dimension_2d > 2048,
            "前提失效：wgpu 升级后 default 的 2d 上限不再大于 2048，需重读这条断言"
        );

        // 特性/尺寸基线必须仍然是 downlevel 的，否则弱 adapter（ GLES、软件 adapter）
        // 会在 request_device 阶段直接失败——那才是真正回归。
        let floor = wgpu::Limits::downlevel_defaults();
        assert_eq!(limits.max_bind_groups, floor.max_bind_groups);
        assert_eq!(
            limits.max_storage_buffers_per_shader_stage,
            floor.max_storage_buffers_per_shader_stage
        );
        assert_eq!(limits.max_buffer_size, floor.max_buffer_size);
        assert_eq!(
            limits.min_uniform_buffer_offset_alignment,
            floor.min_uniform_buffer_offset_alignment
        );
        assert_eq!(limits.max_color_attachments, floor.max_color_attachments);
    }

    /// adapter 只给到 2048（低端 GLES / 软件后端）时，请求的上限仍是可创建的。
    #[test]
    fn device_limits_falls_back_to_downlevel_resolution() {
        assert_eq!(
            device_limits(wgpu::Limits::downlevel_defaults()),
            wgpu::Limits::downlevel_defaults()
        );
    }

    #[test]
    fn surface_size_is_clamped_instead_of_failing() {
        assert_eq!(clamp_to_limits(3840, 2160, 8192), (3840, 2160));
        assert_eq!(clamp_to_limits(16384, 900, 8192), (8192, 900));
        assert_eq!(clamp_to_limits(0, 0, 2048), (1, 1));
    }

    #[test]
    fn test_external_renderer_new() {
        let r = ExternalRenderer::new();
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
        assert!(!r.initialized);
        assert!(!r.surface_attached.load(Ordering::Relaxed));
    }

    #[test]
    fn test_external_renderer_default() {
        let r = ExternalRenderer::default();
        assert_eq!(r.width, 0);
    }

    #[test]
    fn test_detach_without_init() {
        let mut r = ExternalRenderer::new();
        // 未初始化时 detach 不应 panic
        assert!(r.detach().is_ok());
    }

    /// 回归：渲染线程已消失时 detach 不得静默返回 Ok
    ///
    /// 宿主（Android surfaceDestroyed）依据返回值决定是否释放 ANativeWindow，
    /// 假成功会让渲染线程继续操作已释放的原生窗口。
    #[test]
    fn test_detach_reports_error_when_render_thread_gone() {
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<ExtCmd>();
        drop(ctrl_rx);
        let mut r = ExternalRenderer::new();
        r.ctrl_tx = Some(ctrl_tx);
        r.initialized = true;
        let err = r.detach().unwrap_err();
        assert!(
            matches!(err, dyl_core::DylError::Render(_)),
            "expected Render error, got {err}"
        );
    }

    #[test]
    fn test_resize_reports_error_when_render_thread_gone() {
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<ExtCmd>();
        drop(ctrl_rx);
        let mut r = ExternalRenderer::new();
        r.ctrl_tx = Some(ctrl_tx);
        r.initialized = true;
        let err = r.resize(1920, 1080).unwrap_err();
        assert!(
            matches!(err, dyl_core::DylError::Render(_)),
            "expected Render error, got {err}"
        );
    }

    /// ack 协议三种结局：确认成功 / 通道关闭 / 超时未确认，后两者都必须报错
    #[test]
    fn test_surface_ack_requires_confirmation() {
        let timeout = std::time::Duration::from_millis(100);

        let (ack_tx, ack_rx) = mpsc::channel();
        ack_tx.send(Ok(())).unwrap();
        assert!(wait_surface_ack(ack_rx, "ack_ok", timeout).is_ok());

        let (ack_tx, ack_rx) = mpsc::channel();
        ack_tx.send(Err("attach failed".into())).unwrap();
        assert!(wait_surface_ack(ack_rx, "ack_err", timeout).is_err());

        let (ack_tx, ack_rx) = mpsc::channel();
        drop(ack_tx);
        assert!(wait_surface_ack(ack_rx, "ack_closed", timeout).is_err());

        let (_ack_tx, ack_rx) = mpsc::channel();
        assert!(wait_surface_ack(ack_rx, "ack_timeout", timeout).is_err());
    }

    #[test]
    fn test_submit_frame_without_init() {
        let mut r = ExternalRenderer::new();
        let frame = dyl_ffmpeg::VideoFrame {
            width: 320,
            height: 240,
            display_width: 320,
            display_height: 240,
            format: dyl_ffmpeg::PixelFormat::Nv12,
            color_space: dyl_ffmpeg::ColorSpace::Unspecified,
            color_primaries: dyl_ffmpeg::ColorPrimaries::Unspecified,
            color_range: dyl_ffmpeg::ColorRange::Unspecified,
            planes: vec![],
            pts: Some(0),
            time_base: dyl_ffmpeg::time_base(1, 90000),
        };
        // 未初始化时 submit_frame 应静默返回 Ok
        assert!(r.submit_frame(&frame).is_ok());
    }
}
