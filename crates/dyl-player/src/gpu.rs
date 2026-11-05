//! GPU / Surface 初始化与每帧呈现的底层资源。
//!
//! host compositor 模式下 Surface 归 shell：这里创建 device/queue、配置
//! swapchain，并把 `FrameCompositor`（YUV 上传 + 叠加层）与 egui 渲染器
//! 绑到同一份 surface 上。GPU 资源只允许在本结构所在的线程访问。

use std::sync::Arc;

use dyl_engine::{HostOutputHandles, OverlayHandles};
use dyl_render::{install_nonfatal_error_handler, FrameCompositor, OverlayArea, SharedOverlay};
use winit::window::Window;

pub struct GpuContext {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    max_dimension: u32,
    /// 窗口最小化时保持为 true：不上屏、不重建 1x1 swapchain
    suspended: bool,
    pub compositor: FrameCompositor,
    pub egui_ctx: egui::Context,
    pub egui_state: egui_winit::State,
    pub egui_renderer: egui_wgpu::Renderer,
}

impl GpuContext {
    pub fn new(
        window: Arc<Window>,
        handles: HostOutputHandles,
        overlays: OverlayHandles,
    ) -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("create surface: {e}"))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|_| "no suitable GPU adapter".to_string())?;

        // 分辨率上限跟 adapter 走：能力位里的 2048 只是保证下界（ADR-14），
        // 两条呈现路径都必须按设备真实能力放行大帧
        let limits = wgpu::Limits::default().using_resolution(adapter.limits());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("dyl_player_device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits.clone(),
            experimental_features: Default::default(),
            memory_hints: Default::default(),
            trace: Default::default(),
        }))
        .map_err(|e| format!("request device: {e}"))?;

        // 必须在任何渲染调用之前装：wgpu 默认对未捕获错误直接 abort
        install_nonfatal_error_handler(&device);

        let caps = surface.get_capabilities(&adapter);
        tracing::info!(
            "shell surface formats={:?} present_modes={:?} alpha_modes={:?}",
            caps.formats,
            caps.present_modes,
            caps.alpha_modes
        );

        // 非 sRGB 目标：视频 shader 与 egui 的输出都已在 gamma 空间，
        // 再套一次 sRGB 编码会把画面洗白。
        let format = egui_wgpu::preferred_framebuffer_format(&caps.formats)
            .map_err(|e| format!("no usable surface format: {e}"))?;
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
            wgpu::PresentMode::Fifo
        } else {
            wgpu::PresentMode::AutoVsync
        };
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto);
        let max_dimension = limits.max_texture_dimension_2d;

        let size = window.inner_size();
        let suspended = size.width == 0 || size.height == 0;
        let (width, height) = clamp_size(size.width, size.height, max_dimension);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            alpha_mode,
            view_formats: vec![],
            // 留一个额外在飞帧，让下一帧的编码/提交与本帧扫描输出重叠
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut compositor = FrameCompositor::new(
            &device,
            &queue,
            format,
            handles.overlay_clock,
            handles.stats,
        );
        compositor.set_overlays(vec![
            Box::new(SharedOverlay::new(overlays.subtitle)),
            Box::new(SharedOverlay::new(overlays.danmaku)),
        ]);

        let egui_ctx = egui::Context::default();
        crate::ui::install_cjk_font(&egui_ctx);
        crate::ui::apply_env_theme(&egui_ctx);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(max_dimension as usize),
        );
        let egui_renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());

        tracing::info!(
            "shell GPU ready: {}x{} format={:?} present={:?}",
            width,
            height,
            format,
            present_mode
        );

        Ok(Self {
            surface,
            device,
            queue,
            config,
            max_dimension,
            suspended,
            compositor,
            egui_ctx,
            egui_state,
            egui_renderer,
        })
    }

    /// 可渲染尺寸；窗口最小化时为 `None`
    pub fn render_size(&self) -> Option<(u32, u32)> {
        if self.suspended {
            None
        } else {
            Some((self.config.width, self.config.height))
        }
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            // 最小化：保留旧 config，恢复时会再收到一次非零 Resized
            self.suspended = true;
            return;
        }
        let (width, height) = clamp_size(width, height, self.max_dimension);
        if !self.suspended && width == self.config.width && height == self.config.height {
            return;
        }
        self.suspended = false;
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        // 尺寸变了必须重绘，哪怕没有新视频帧
        self.compositor.invalidate();
        tracing::debug!("shell surface resized to {width}x{height}");
    }

    pub fn invalidate(&mut self) {
        self.compositor.invalidate();
    }

    /// 画布几何：video_rect 由 compositor 与叠加层共用同一份 fit 结果
    ///
    /// `scale_factor` 是窗口的 DPR：弹幕按固定参考字号（逻辑像素）排版，上屏要乘它。
    pub fn overlay_area(
        &self,
        surface_width: u32,
        surface_height: u32,
        scale_factor: f32,
    ) -> OverlayArea {
        let (video_width, video_height) = self.compositor.video_size();
        OverlayArea::new(surface_width, surface_height, video_width, video_height)
            .with_scale_factor(scale_factor)
    }

    pub fn acquire(&mut self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => Some(texture),
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => Some(texture),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.reconfigure();
                None
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => None,
        }
    }

    fn reconfigure(&mut self) {
        if self.suspended {
            return;
        }
        tracing::warn!("shell surface outdated/lost, reconfiguring");
        self.surface.configure(&self.device, &self.config);
    }
}

fn clamp_size(width: u32, height: u32, max: u32) -> (u32, u32) {
    (width.clamp(1, max), height.clamp(1, max))
}
