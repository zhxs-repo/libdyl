//! Surface 运行时状态、平台句柄转换与 host compositor 输出。

use std::sync::Arc;

use dyl_core::contract::{
    ErrorCode, SurfaceDescriptor, SurfaceKind, SurfaceSnapshot, SurfaceStatus, SCHEMA_VERSION,
};
use dyl_core::Result as DylResult;
use dyl_ffmpeg::VideoFrame;
use dyl_render::{
    ExternalRenderer, OutputStats, OutputStatsSnapshot, OverlayClock, SurfaceHandle, VideoOutput,
};

use crate::error::EngineError;
use crate::overlays::OverlayEngines;

/// 宿主合成器的帧投递口。
///
/// 只允许持有有界队列的发送端或唤醒回调，**不得持有 GPU 资源**：
/// engine 线程可能在任意时刻 drop sink，GPU 资源必须在 shell 主线程释放。
pub trait HostFrameSink: Send + Sync {
    /// 非阻塞投递；队列满时由实现丢最旧帧
    fn push_frame(&self, frame: VideoFrame);
    /// 唤醒 shell 主循环重绘
    fn request_redraw(&self);
}

/// host 模式下 engine 与 shell 共用的时钟与计数句柄。
pub struct HostOutputHandles {
    pub overlay_clock: Arc<OverlayClock>,
    pub stats: Arc<OutputStats>,
}

/// host compositor：pipeline 的帧进入 sink，时钟与统计与 shell 共享。
pub(crate) struct HostVideoOutput {
    sink: Arc<dyn HostFrameSink>,
    overlay_clock: Arc<OverlayClock>,
    stats: Arc<OutputStats>,
}

impl HostVideoOutput {
    pub(crate) fn new(
        sink: Arc<dyn HostFrameSink>,
        overlay_clock: Arc<OverlayClock>,
        stats: Arc<OutputStats>,
    ) -> Self {
        Self {
            sink,
            overlay_clock,
            stats,
        }
    }
}

impl VideoOutput for HostVideoOutput {
    fn submit_frame(&mut self, frame: &VideoFrame) -> DylResult<()> {
        // 锚定由输出层负责：shell 收到帧后只入 compositor 队列，不重复 reanchor
        if let Some(secs) = frame.pts_secs() {
            self.overlay_clock.reanchor(secs);
        }
        self.sink.push_frame(frame.clone());
        self.sink.request_redraw();
        Ok(())
    }

    fn resize(&mut self, _width: u32, _height: u32) -> DylResult<()> {
        // host 的 Surface 归 shell，engine 只转发尺寸；由 shell 自己重新配置
        Ok(())
    }

    fn set_overlays(&mut self, _overlays: Vec<Box<dyn dyl_render::Overlay>>) {
        // 叠加层引擎由 shell 通过 overlay_handles 共享，替换栈后重绘一帧
        self.sink.request_redraw();
    }

    fn freeze_overlay_clock(&self) {
        self.overlay_clock.freeze();
    }

    fn stats(&self) -> OutputStatsSnapshot {
        self.stats.snapshot()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SurfaceRuntime {
    pub status: SurfaceStatus,
    pub kind: Option<SurfaceKind>,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
}

impl Default for SurfaceRuntime {
    fn default() -> Self {
        Self {
            status: SurfaceStatus::None,
            kind: None,
            width: 0,
            height: 0,
            scale_factor: 1.0,
        }
    }
}

impl SurfaceRuntime {
    pub(crate) fn attached(kind: SurfaceKind, width: u32, height: u32, scale_factor: f64) -> Self {
        Self {
            status: SurfaceStatus::Attached,
            kind: Some(kind),
            width,
            height,
            scale_factor,
        }
    }

    pub(crate) fn snapshot(&self) -> SurfaceSnapshot {
        SurfaceSnapshot {
            status: self.status,
            kind: self.kind,
            width: self.width,
            height: self.height,
            scale_factor: self.scale_factor,
        }
    }
}

fn parse_handle(text: &str, what: &str) -> Result<u64, EngineError> {
    let trimmed = text.trim();
    let parsed = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse::<u64>().ok()
    };
    parsed
        .filter(|value| *value != 0)
        .ok_or_else(|| EngineError::invalid_argument(format!("invalid {what} handle: {text}")))
}

pub(crate) fn ensure_supported(kind: SurfaceKind) -> Result<(), EngineError> {
    // 句柄种类由 `dyl-render` 声明：它才是能把句柄翻译成 `wgpu::Surface` 的那个 crate
    if dyl_render::capabilities::supported_surface_kinds().contains(&kind) {
        Ok(())
    } else {
        Err(EngineError::new(
            ErrorCode::NotSupported,
            format!(
                "surface kind {kind:?} is not supported on {}",
                std::env::consts::OS
            ),
        ))
    }
}

/// 校验描述符并转换成 `dyl-render` 的平台原生句柄
pub(crate) fn surface_handle(desc: &SurfaceDescriptor) -> Result<SurfaceHandle, EngineError> {
    if desc.schema() != SCHEMA_VERSION {
        return Err(EngineError::invalid_argument(format!(
            "surface schema must be {SCHEMA_VERSION}, got {}",
            desc.schema()
        )));
    }
    let (width, height) = desc.size();
    if width == 0 || height == 0 {
        return Err(EngineError::invalid_argument(format!(
            "surface size must be non-zero, got {width}x{height}"
        )));
    }
    let scale_factor = desc.scale_factor();
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err(EngineError::invalid_argument(format!(
            "invalid scale_factor: {scale_factor}"
        )));
    }
    // DPR 要一路带到叠加层：弹幕的固定参考字号按逻辑像素定义，上屏得乘它
    let physical_scale = scale_factor as f32;

    Ok(match desc {
        SurfaceDescriptor::WindowsHwnd { handle, .. } => {
            SurfaceHandle::windows_hwnd(parse_handle(handle, "hwnd")?, width, height)
                .with_scale_factor(physical_scale)
        }
        SurfaceDescriptor::MacosNsview { handle, .. } => {
            SurfaceHandle::macos_nsview(parse_handle(handle, "nsview")?, width, height)
                .with_scale_factor(physical_scale)
        }
        SurfaceDescriptor::X11Window {
            handle, display, ..
        } => SurfaceHandle::x11_window(
            parse_handle(handle, "window")?,
            parse_handle(display, "display")?,
            width,
            height,
        )
        .with_scale_factor(physical_scale),
        SurfaceDescriptor::WaylandSurface {
            surface, display, ..
        } => SurfaceHandle::wayland_surface(
            parse_handle(surface, "surface")?,
            parse_handle(display, "display")?,
            width,
            height,
        )
        .with_scale_factor(physical_scale),
        SurfaceDescriptor::AndroidNativeWindow { handle, .. } => {
            SurfaceHandle::android(parse_handle(handle, "native window")?, width, height)
                .with_scale_factor(physical_scale)
        }
    })
}

pub(crate) fn create_output(
    handle: SurfaceHandle,
    overlays: &OverlayEngines,
) -> Result<ExternalRenderer, EngineError> {
    let mut renderer = ExternalRenderer::new();
    renderer.init_with_surface(handle).map_err(|e| {
        EngineError::new(
            ErrorCode::Surface,
            format!("create external surface output: {e}"),
        )
    })?;
    renderer.set_overlays(overlays.overlays());
    Ok(renderer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyl_ffmpeg::{ColorPrimaries, ColorRange, ColorSpace, PixelFormat};
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct TestSink {
        frames: Mutex<Vec<VideoFrame>>,
        redraws: AtomicUsize,
    }

    impl HostFrameSink for TestSink {
        fn push_frame(&self, frame: VideoFrame) {
            self.frames.lock().push(frame);
        }

        fn request_redraw(&self) {
            self.redraws.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn frame(pts_ms: i64) -> VideoFrame {
        VideoFrame {
            width: 2,
            height: 2,
            display_width: 2,
            display_height: 2,
            format: PixelFormat::Nv12,
            color_space: ColorSpace::Unspecified,
            color_primaries: ColorPrimaries::Unspecified,
            color_range: ColorRange::Unspecified,
            planes: vec![],
            pts: Some(pts_ms),
            time_base: dyl_ffmpeg::time_base(1, 1000),
        }
    }

    #[test]
    fn host_output_reanchors_clock_and_requests_redraw() {
        let sink = Arc::new(TestSink::default());
        let clock = Arc::new(OverlayClock::new());
        let stats = Arc::new(OutputStats::new());
        let mut output = HostVideoOutput::new(sink.clone(), clock.clone(), stats.clone());

        output.submit_frame(&frame(1_234)).unwrap();
        assert_eq!(sink.frames.lock().len(), 1);
        assert_eq!(sink.redraws.load(Ordering::Relaxed), 1);
        assert!(clock.is_live());
        let pts = clock.now_pts();
        assert!((pts - 1.234).abs() < 0.2, "clock pts={pts}");

        output.freeze_overlay_clock();
        assert!(!clock.is_live());

        output.set_overlays(Vec::new());
        assert_eq!(sink.redraws.load(Ordering::Relaxed), 2);

        // 渲染帧数由 shell 的 FrameCompositor 计数，engine 侧只共享句柄
        assert_eq!(output.stats().rendered_frames, 0);
        assert_eq!(stats.snapshot().rendered_frames, 0);
    }

    #[test]
    fn host_output_without_pts_does_not_reset_clock() {
        let sink = Arc::new(TestSink::default());
        let clock = Arc::new(OverlayClock::new());
        let stats = Arc::new(OutputStats::new());
        let mut output = HostVideoOutput::new(sink, clock.clone(), stats);
        let mut no_pts = frame(0);
        no_pts.pts = None;

        output.submit_frame(&no_pts).unwrap();
        assert!(!clock.is_live());
        assert_eq!(clock.now_pts(), 0.0);
    }
}
