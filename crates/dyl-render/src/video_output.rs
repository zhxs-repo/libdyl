//! 视频输出抽象与 OutputSlot — pipeline 与呈现层之间的唯一交换点
//!
//! Pipeline 只认识 [`OutputSlot`]：它把同步后的帧提交进去，不关心背后是
//! 外部 Surface 渲染器、宿主合成器还是无 Surface 时的 [`NullOutput`]。
//! Surface 的创建、替换、分离由引擎在 slot 上原子换出，因此 load 与 surface
//! 的先后顺序互不依赖。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dyl_core::Result;
use dyl_ffmpeg::VideoFrame;
use parking_lot::Mutex;

use crate::overlay::Overlay;

/// 呈现统计。由输出实现与宿主合成器共享同一份原子计数。
#[derive(Debug, Default)]
pub struct OutputStats {
    rendered_frames: AtomicU64,
    dropped_frames: AtomicU64,
}

impl OutputStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_rendered(&self) {
        self.rendered_frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_dropped(&self) {
        self.dropped_frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_dropped(&self, count: u64) {
        self.dropped_frames.fetch_add(count, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> OutputStatsSnapshot {
        OutputStatsSnapshot {
            rendered_frames: self.rendered_frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
        }
    }
}

/// `OutputStats` 某一时刻的值，用于快照上报。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputStatsSnapshot {
    pub rendered_frames: u64,
    pub dropped_frames: u64,
}

/// 平台原生 Surface 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSurfaceKind {
    AndroidNativeWindow,
    WindowsHwnd,
    MacosNsview,
    X11Window,
    WaylandSurface,
}

/// 平台原生 Surface 句柄。
///
/// 字段含义随 `kind` 变化：
/// - Android: `native_window` = `ANativeWindow*`
/// - Windows: `native_window` = `HWND`
/// - macOS: `native_window` = `NSView*`
/// - X11: `native_window` = `Window`，`native_display` = `Display*`
/// - Wayland: `native_window` = `wl_surface*`，`native_display` = `wl_display*`
#[derive(Debug, Clone, Copy)]
pub struct SurfaceHandle {
    pub kind: NativeSurfaceKind,
    pub native_window: u64,
    pub native_display: u64,
    pub width: u32,
    pub height: u32,
    /// 逻辑像素 → 物理像素倍率（宿主窗口的 DPR）；1.0 = 宿主没告知
    ///
    /// `width`/`height` 是物理像素，而这个倍率是给**固定参考字号**用的：弹幕基准
    /// 字号按逻辑像素定义，上屏要乘它，否则高分屏上字只有一半大。
    pub scale_factor: f32,
}

impl SurfaceHandle {
    pub fn new(
        kind: NativeSurfaceKind,
        native_window: u64,
        native_display: u64,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            kind,
            native_window,
            native_display,
            width,
            height,
            scale_factor: 1.0,
        }
    }

    /// 补上宿主 DPR；非有限值或非正数按 1.0 处理
    pub fn with_scale_factor(mut self, scale_factor: f32) -> Self {
        self.scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        self
    }

    pub fn android(native_window: u64, width: u32, height: u32) -> Self {
        Self::new(
            NativeSurfaceKind::AndroidNativeWindow,
            native_window,
            0,
            width,
            height,
        )
    }

    pub fn windows_hwnd(hwnd: u64, width: u32, height: u32) -> Self {
        Self::new(NativeSurfaceKind::WindowsHwnd, hwnd, 0, width, height)
    }

    pub fn macos_nsview(view: u64, width: u32, height: u32) -> Self {
        Self::new(NativeSurfaceKind::MacosNsview, view, 0, width, height)
    }

    pub fn x11_window(window: u64, display: u64, width: u32, height: u32) -> Self {
        Self::new(NativeSurfaceKind::X11Window, window, display, width, height)
    }

    pub fn wayland_surface(surface: u64, display: u64, width: u32, height: u32) -> Self {
        Self::new(
            NativeSurfaceKind::WaylandSurface,
            surface,
            display,
            width,
            height,
        )
    }
}

/// 视频输出抽象。
///
/// 只负责配置、提交、统计与生命周期；窗口、事件循环、标题栏都归 shell。
pub trait VideoOutput: Send {
    /// 提交一帧；实现自行决定排队/上屏节拍。
    fn submit_frame(&mut self, frame: &VideoFrame) -> Result<()>;

    /// 宿主窗口尺寸变化。
    fn resize(&mut self, width: u32, height: u32) -> Result<()>;

    /// 替换叠加层栈（字幕、弹幕）。
    fn set_overlays(&mut self, _overlays: Vec<Box<dyn Overlay>>) {}

    /// 冻结叠加层挂钟（暂停/EOF）。
    fn freeze_overlay_clock(&self) {}

    /// 强制重绘一帧（宿主窗口内容被系统擦除时）。默认无操作。
    ///
    /// 暂停/EOF 后渲染线程不再 present，宿主若被 GDI/DWM 擦除画面就没有
    /// 下一次上屏可依赖；宿主可调用此方法把 `last_frame` 重新合成一次。
    fn redraw(&mut self) -> Result<()> {
        Ok(())
    }

    fn stats(&self) -> OutputStatsSnapshot {
        OutputStatsSnapshot::default()
    }

    /// 同步释放原生 Surface 引用；返回 Ok 表示宿主可以销毁窗口。
    fn detach(&mut self) -> Result<()> {
        Ok(())
    }
}

/// 无 Surface 时的输出：丢帧，音频与主时钟照常前进。
#[derive(Debug, Default)]
pub struct NullOutput {
    dropped_frames: u64,
}

impl NullOutput {
    pub fn new() -> Self {
        Self::default()
    }
}

impl VideoOutput for NullOutput {
    fn submit_frame(&mut self, _frame: &VideoFrame) -> Result<()> {
        self.dropped_frames += 1;
        Ok(())
    }

    fn resize(&mut self, _width: u32, _height: u32) -> Result<()> {
        Ok(())
    }

    fn stats(&self) -> OutputStatsSnapshot {
        OutputStatsSnapshot {
            rendered_frames: 0,
            dropped_frames: self.dropped_frames,
        }
    }
}

/// Pipeline 与呈现层之间的唯一交换点。
///
/// 引擎持有 slot，Surface attach/detach 通过 [`OutputSlot::replace`] 原子换出；
/// pipeline 只在本对象上提交帧，不感知 Surface 生命周期。
pub struct OutputSlot {
    output: Mutex<Box<dyn VideoOutput>>,
}

impl OutputSlot {
    pub fn new(output: Box<dyn VideoOutput>) -> Arc<Self> {
        Arc::new(Self {
            output: Mutex::new(output),
        })
    }

    pub fn null() -> Arc<Self> {
        Self::new(Box::new(NullOutput::new()))
    }

    pub fn submit(&self, frame: &VideoFrame) -> Result<()> {
        self.output.lock().submit_frame(frame)
    }

    pub fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.output.lock().resize(width, height)
    }

    pub fn set_overlays(&self, overlays: Vec<Box<dyn Overlay>>) {
        self.output.lock().set_overlays(overlays);
    }

    pub fn freeze_overlay_clock(&self) {
        self.output.lock().freeze_overlay_clock();
    }

    pub fn redraw(&self) -> Result<()> {
        self.output.lock().redraw()
    }

    pub fn stats(&self) -> OutputStatsSnapshot {
        self.output.lock().stats()
    }

    /// 换出新输出。返回的旧输出由调用方决定 `detach` 与 drop 时机。
    pub fn replace(&self, next: Box<dyn VideoOutput>) -> Box<dyn VideoOutput> {
        std::mem::replace(&mut *self.output.lock(), next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyl_ffmpeg::{ColorPrimaries, ColorRange, ColorSpace, PixelFormat, VideoFrame};

    fn frame() -> VideoFrame {
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
            pts: Some(0),
            time_base: dyl_ffmpeg::time_base(1, 1000),
        }
    }

    #[test]
    fn null_output_drops_frames_and_counts_them() {
        let f = frame();
        let mut out = NullOutput::new();
        for _ in 0..3 {
            out.submit_frame(&f).unwrap();
        }
        assert_eq!(
            out.stats(),
            OutputStatsSnapshot {
                rendered_frames: 0,
                dropped_frames: 3
            }
        );
    }

    #[test]
    fn slot_replaces_output_atomically() {
        let slot = OutputSlot::null();
        slot.submit(&frame()).unwrap();
        assert_eq!(slot.stats().dropped_frames, 1);

        let old = slot.replace(Box::new(NullOutput::new()));
        assert_eq!(old.stats().dropped_frames, 1);
        slot.submit(&frame()).unwrap();
        assert_eq!(slot.stats().dropped_frames, 1);
    }

    #[test]
    fn surface_handle_carries_kind_and_geometry() {
        let h = SurfaceHandle::windows_hwnd(0x1234, 1280, 720);
        assert_eq!(h.kind, NativeSurfaceKind::WindowsHwnd);
        assert_eq!(h.native_window, 0x1234);
        assert_eq!((h.width, h.height), (1280, 720));
    }
}
