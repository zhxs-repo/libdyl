//! Overlay trait and OverlayStack — 视频帧之上的叠加层抽象
//!
//! 合成顺序：video → subtitle → danmaku → (OSD, 预留)
//! 每个 Overlay 在每帧渲染时被调用，使用 wgpu encoder 进行叠加绘制。

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use wgpu;

/// 画面矩形 `(x, y, w, h)` — surface 物理像素坐标
///
/// 等比适配（letterbox / pillarbox）后的视频实际显示区域。
pub type VideoRect = (f32, f32, f32, f32);

/// 把视频等比适配到 surface，返回画面矩形
///
/// 视频 pass 与 overlay **必须共用这一份结果**：一旦两边各自算，字幕/弹幕就会
/// 与画面错位（pillarbox 时叠加层落到黑边上）。
///
/// `video_w`/`video_h` 是**显示尺寸**（像素宽高比 SAR 已应用），不是编码宽高：
/// 1080x1080 + SAR 9:16 的竖屏视频必须按约 608x1080 摆放，否则画面会被拉成
/// 正方形。
///
/// 视频尺寸未知（首帧之前）时退化为整个 surface。
pub fn fit_video_rect(surf_w: u32, surf_h: u32, video_w: u32, video_h: u32) -> VideoRect {
    let (sw, sh) = (surf_w as f32, surf_h as f32);
    if video_w == 0 || video_h == 0 || sw <= 0.0 || sh <= 0.0 {
        return (0.0, 0.0, sw, sh);
    }
    let (vw, vh) = (video_w as f32, video_h as f32);
    // 等比：取两轴较小值，保证画面既不变形也完整可见
    let scale = (sw / vw).min(sh / vh);
    let w = (vw * scale).round();
    let h = (vh * scale).round();
    (((sw - w) / 2.0).round(), ((sh - h) / 2.0).round(), w, h)
}

/// 一帧的画布几何信息
///
/// 刻意区分三类尺寸，字幕与弹幕各自取用：
///
/// - **surface 尺寸**：整个渲染目标的物理像素。弹幕按它布局并且不裁剪 —— 弹幕属于
///   **窗口**，横穿 letterbox / pillarbox 的黑边是预期行为。所有 overlay shader 的
///   "像素 → NDC"也用它。
/// - **画面矩形**：视频等比适配后的区域。字幕的**位置**必须映射到这里，并用
///   [`OverlayArea::scissor`] 裁剪，否则长句会画到黑边上。
/// - **视频显示分辨率**：字幕字号的基准（[`OverlayArea::content_scale`]）。弹幕不用
///   它，字号走固定参考，只有 [`OverlayArea::scale_factor`] 参与换算。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayArea {
    /// surface 实际尺寸（物理像素）
    pub surface_width: u32,
    pub surface_height: u32,
    /// 视频画面矩形（letterbox 之后）
    pub video_rect: VideoRect,
    /// 视频显示分辨率（SAR 已应用）；0 = 未知（首帧之前）
    pub video_width: u32,
    pub video_height: u32,
    /// 逻辑像素 → 物理像素的倍率（窗口 DPR）；1.0 = 未知
    ///
    /// 固定参考字号（弹幕的基准字号）是按逻辑像素定的，上屏必须乘这个倍率，否则
    /// 高分屏上字只有一半大。
    pub scale_factor: f32,
}

impl OverlayArea {
    /// 由 surface 与视频显示分辨率构造，画面矩形自动等比计算
    ///
    /// `scale_factor` 默认 1.0；知道窗口 DPR 时用 [`OverlayArea::with_scale_factor`]
    /// 补上，弹幕的固定参考字号才会随高分屏放大。
    pub fn new(
        surface_width: u32,
        surface_height: u32,
        video_width: u32,
        video_height: u32,
    ) -> Self {
        Self {
            surface_width,
            surface_height,
            video_rect: fit_video_rect(surface_width, surface_height, video_width, video_height),
            video_width,
            video_height,
            scale_factor: 1.0,
        }
    }

    /// 全屏退化：无视频信息时（或测试里）让画面矩形等于 surface
    pub fn full_surface(surface_width: u32, surface_height: u32) -> Self {
        Self {
            surface_width,
            surface_height,
            video_rect: (0.0, 0.0, surface_width as f32, surface_height as f32),
            video_width: surface_width,
            video_height: surface_height,
            scale_factor: 1.0,
        }
    }

    /// 补上窗口 DPR；非有限值或非正数按 1.0 处理
    pub fn with_scale_factor(mut self, scale_factor: f32) -> Self {
        self.scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        self
    }

    /// **弹幕**的布局画布：整个 surface，不受画面矩形约束
    pub fn surface_rect(&self) -> VideoRect {
        (
            0.0,
            0.0,
            self.surface_width as f32,
            self.surface_height as f32,
        )
    }

    /// 画面矩形宽度
    pub fn rect_width(&self) -> f32 {
        self.video_rect.2
    }

    /// 画面矩形高度
    pub fn rect_height(&self) -> f32 {
        self.video_rect.3
    }

    /// 画面矩形在 surface 像素里的裁剪区间 `(x, y, w, h)`；空矩形返回 None
    ///
    /// 字幕用它：长句换行时会伸出画面矩形，不裁剪就画到 letterbox / pillarbox 的
    /// 黑边上。弹幕**不**用它 —— 弹幕的画布是整个 surface（[`OverlayArea::surface_rect`]），
    /// 绘制时不设 scissor。
    pub fn scissor(&self) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = self.video_rect;
        let sx = (x.max(0.0).round() as u32).min(self.surface_width);
        let sy = (y.max(0.0).round() as u32).min(self.surface_height);
        let sw = (w.max(0.0).round() as u32).min(self.surface_width - sx);
        let sh = (h.max(0.0).round() as u32).min(self.surface_height - sy);
        (sw > 0 && sh > 0).then_some((sx, sy, sw, sh))
    }

    /// **内容基准**的字号缩放系数：只取决于视频原生高度，与窗口尺寸无关
    ///
    /// 这是"拖动窗口时字幕/弹幕字形大小不变"的唯一来源。用窗口高度当基准会让
    /// 字号跟着窗口缩放；用画面矩形高度当基准则会在 letterbox（竖屏手机播横屏
    /// 视频）时把文字放大到超出画面。
    ///
    /// 视频分辨率未知时返回 1.0（视为参考分辨率）——此时按 surface 推算只会得到
    /// 一个立刻会被作废的中间值，不如直接用基准值稳定一帧。
    pub fn content_scale(&self, reference_height: f32) -> f32 {
        if reference_height <= 0.0 || self.video_height == 0 {
            return 1.0;
        }
        self.video_height as f32 / reference_height
    }
}

/// GPU 上下文代号：每次 `FrameCompositor` 创建（= 每次创建设备）分配一个新值
///
/// 叠加层引擎按"设备"惰性缓存渲染管线与纹理。`surface_attach` 替换 Surface 时
/// 会换一个 `wgpu::Device`，旧设备上创建的管线在新设备上使用会让 wgpu 直接报
/// `RenderPipeline does not exist`，所以缓存必须带上设备代号。
///
/// 不能用 `wgpu::Device` 的 `Eq`/`Hash` 做这件事：它们代理到 wgpu-core 的设备
/// ID，而 ID 只在单个 `wgpu::Instance` 内唯一 —— `ExternalRenderer` 每次 attach
/// 都新建 Instance，新设备的 ID 会和旧设备完全相同。由合成器显式发号才可靠。
pub fn next_gpu_epoch() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 一帧叠加层渲染共用的 GPU 上下文
pub struct OverlayFrame<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    /// 本帧的渲染目标
    pub target: &'a wgpu::TextureView,
    pub surface_format: wgpu::TextureFormat,
    /// 设备代号（见 [next_gpu_epoch]）
    pub gpu_epoch: u64,
}

/// Overlay trait — 叠加层抽象
///
/// 实现者需要在 `render()` 中使用 alpha blending 将自己的内容绘制到 target 上。
/// `update()` 在渲染线程每帧调用，用于更新内部状态（如字幕 bitmap、弹幕位置）。
///
/// `frame.gpu_epoch` 标识本次调用所用的设备。实现方若缓存了管线/纹理等设备绑定
/// 资源，必须在该值与上次不同时全部丢弃重建。
pub trait Overlay: Send {
    /// 每帧更新状态（在 render 之前调用，渲染线程上）
    fn update(&mut self, pts: f64);

    /// 渲染叠加层
    ///
    /// 实现者应该使用 alpha blending 绘制到 target 上。
    ///
    /// `area` 携带本帧的画布几何：surface 尺寸供 shader 做像素 → NDC，画面矩形
    /// 供定位，视频原生分辨率供计算字号（见 [OverlayArea] 的字段说明）。
    fn render(
        &mut self,
        frame: &OverlayFrame<'_>,
        encoder: &mut wgpu::CommandEncoder,
        area: OverlayArea,
    );

    /// 是否可见（不可见时跳过 render）
    fn is_visible(&self) -> bool {
        true
    }
}

/// OverlayStack — 管理多个叠加层，按序合成
///
/// 合成顺序与添加顺序一致（先添加的先绘制，后添加的覆盖在上面）。
pub struct OverlayStack {
    overlays: Vec<Box<dyn Overlay>>,
}

impl OverlayStack {
    pub fn new() -> Self {
        Self {
            overlays: Vec::new(),
        }
    }

    /// 添加一个叠加层（后添加的在更上层）
    pub fn push(&mut self, overlay: Box<dyn Overlay>) {
        self.overlays.push(overlay);
    }

    /// 获取叠加层数量
    pub fn len(&self) -> usize {
        self.overlays.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.overlays.is_empty()
    }

    /// 更新所有叠加层
    pub fn update_all(&mut self, pts: f64) {
        for overlay in &mut self.overlays {
            overlay.update(pts);
        }
    }

    /// 渲染所有可见的叠加层
    pub fn render_all(
        &mut self,
        frame: &OverlayFrame<'_>,
        encoder: &mut wgpu::CommandEncoder,
        area: OverlayArea,
    ) {
        for overlay in &mut self.overlays {
            if overlay.is_visible() {
                overlay.render(frame, encoder, area);
            }
        }
    }
}

impl Default for OverlayStack {
    fn default() -> Self {
        Self::new()
    }
}

/// SharedOverlay — 共享所有权 Overlay 包装器
///
/// 将 `Arc<Mutex<T>>` 包装为 Overlay，允许多个持有者共享同一个引擎实例。
/// FFI 层持有 `Arc<Mutex<T>>` 用于运行时更新（加载文件、切换开关），
/// 同时 SharedOverlay 被移入 OverlayStack 供渲染线程使用。
pub struct SharedOverlay<T: Overlay + 'static> {
    inner: Arc<Mutex<T>>,
}

impl<T: Overlay + 'static> SharedOverlay<T> {
    pub fn new(inner: Arc<Mutex<T>>) -> Self {
        Self { inner }
    }
}

impl<T: Overlay + 'static> Overlay for SharedOverlay<T> {
    fn update(&mut self, pts: f64) {
        self.inner.lock().update(pts);
    }

    fn render(
        &mut self,
        frame: &OverlayFrame<'_>,
        encoder: &mut wgpu::CommandEncoder,
        area: OverlayArea,
    ) {
        self.inner.lock().render(frame, encoder, area);
    }

    fn is_visible(&self) -> bool {
        self.inner.lock().is_visible()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16:9 视频铺满 16:9 surface：画面矩形 = 整个 surface
    #[test]
    fn rect_matches_surface_when_aspects_agree() {
        let r = fit_video_rect(1920, 1080, 1920, 1080);
        assert_eq!(r, (0.0, 0.0, 1920.0, 1080.0));
    }

    /// 超宽窗口放高视频：左右留黑边（pillarbox），画面必须垂直居中
    #[test]
    fn pillarbox_centers_vertically() {
        // 1920x600 的扁窗口播 1080p：受高度限制，画面不满宽 → 左右黑边
        let (x, y, w, h) = fit_video_rect(1920, 600, 1920, 1080);
        assert_eq!(h, 600.0, "受高度限制时画面应占满高度");
        assert!(w < 1920.0, "宽视频在扁窗口应左右留边，实际 w={w}");
        // 两侧留边最多差 1px：偏移是对留白宽度取整后落地的
        assert!((x - (1920.0 - x - w)).abs() <= 1.0, "x={x} w={w}");
        assert_eq!(y, 0.0);
    }

    /// 竖屏 surface 播横屏视频：上下留黑边（letterbox）
    #[test]
    fn letterbox_centers_vertically() {
        let (_x, y, w, h) = fit_video_rect(1080, 2340, 1920, 1080);
        assert!(y > 0.0, "横视频在竖屏应上下留边，实际 y={y}");
        assert!((y - (2340.0 - y - h)).abs() <= 1.0, "y={y} h={h}");
        // 宽高比必须保持 —— 分轴缩放会把画面压扁。
        // 容忍 ±1px：fit_video_rect 对边长取整，绝对误差至多半个像素。
        let aspect = w / h;
        let expected = 1920.0 / 1080.0;
        assert!(
            (aspect - expected).abs() < 0.005,
            "aspect={aspect} expected≈{expected}（误差应仅为取整）"
        );
    }

    /// 画面矩形必须完整落在 surface 内
    #[test]
    fn rect_always_inside_surface() {
        for (sw, sh) in [(1920u32, 1080u32), (1080, 2340), (2560, 1080), (640, 480)] {
            for (vw, vh) in [(1920u32, 1080u32), (1280, 720), (640, 480), (3840, 2160)] {
                let (x, y, w, h) = fit_video_rect(sw, sh, vw, vh);
                assert!(
                    x >= 0.0 && y >= 0.0,
                    "surface {sw}x{sh} video {vw}x{vh}: x={x} y={y}"
                );
                assert!(x + w <= sw as f32 + 0.5, "surface {sw}x{sh}: x+w={}", x + w);
                assert!(y + h <= sh as f32 + 0.5, "surface {sw}x{sh}: y+h={}", y + h);
            }
        }
    }

    /// 视频尺寸未知（首帧之前）时退化到全屏，不能返回 0 尺寸
    #[test]
    fn zero_video_falls_back_to_full_surface() {
        let r = fit_video_rect(1080, 2340, 0, 0);
        assert_eq!(r, (0.0, 0.0, 1080.0, 2340.0));
    }

    /// 裁剪区间必须正好是画面矩形，且不能超出 surface（wgpu 会校验 scissor）
    #[test]
    fn scissor_matches_video_rect() {
        // 宽窗口 + 16:9 视频 → 左右 pillarbox
        let area = OverlayArea::new(1264, 421, 640, 360);
        let (x, y, w, h) = area.video_rect;
        assert!(x > 0.0 && w < 1264.0);
        assert_eq!(
            area.scissor(),
            Some((x as u32, y as u32, w as u32, h as u32))
        );

        // 无视频信息 → 整块 surface
        assert_eq!(
            OverlayArea::full_surface(800, 600).scissor(),
            Some((0, 0, 800, 600))
        );

        // 空 surface 不能产生 0 宽的 scissor
        assert_eq!(OverlayArea::full_surface(0, 0).scissor(), None);
    }

    /// 非方形像素（SAR≠1）必须按显示尺寸摆画面：1080x1080 + SAR 9:16 是
    /// 竖屏视频，若用编码尺寸会得到一个错误的正方形画面
    #[test]
    fn anamorphic_video_uses_display_dimensions_for_geometry() {
        let area = OverlayArea::new(1920, 1080, 608, 1080);
        assert_eq!(area.video_rect, (656.0, 0.0, 608.0, 1080.0));
        assert!(area.rect_width() / area.rect_height() < 0.6);

        let coded = OverlayArea::new(1920, 1080, 1080, 1080);
        assert_eq!(coded.video_rect, (420.0, 0.0, 1080.0, 1080.0));
    }

    /// 核心约束：字号只跟视频原生分辨率走，窗口怎么变都不动
    #[test]
    fn content_scale_is_independent_of_surface_and_rect() {
        let reference = 1080.0;

        // 同一个 1080p 视频，放在各种尺寸的窗口/画面矩形里
        let windows = [(1920u32, 1080u32), (1080, 2340), (2560, 1440), (640, 360)];
        let scales: Vec<f32> = windows
            .iter()
            .map(|&(sw, sh)| OverlayArea::new(sw, sh, 1920, 1080).content_scale(reference))
            .collect();
        for s in &scales {
            assert!(
                (*s - 1.0).abs() < 1e-6,
                "1080p 视频在任意窗口下都应得到 1.0，实际 {s}"
            );
        }

        // 4K 视频在任何窗口下都是 2.0（放大显示也不会变大）
        assert!(
            (OverlayArea::new(1920, 1080, 3840, 2160).content_scale(reference) - 2.0).abs() < 1e-6
        );
        // 720p 视频在任何窗口下都是 720/1080
        let s720 = OverlayArea::new(3840, 2160, 1280, 720).content_scale(reference);
        assert!((s720 - 720.0 / 1080.0).abs() < 1e-6, "s720={s720}");
    }

    /// 视频分辨率未知时按基准分辨率处理，不能除零也不能退化到跟随窗口
    #[test]
    fn content_scale_guards_unknown_video() {
        let area = OverlayArea::new(1920, 2340, 0, 0);
        assert_eq!(area.content_scale(1080.0), 1.0);
        assert_eq!(area.content_scale(0.0), 1.0, "reference <= 0 必须防除零");
    }
}
