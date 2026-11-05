//! 弹幕布局引擎 — 基于 DFM+ 的栅格碰撞算法
//!
//! 核心策略：
//! - 四种弹幕类型独立栅格系统 (ScrollRL, ScrollLR, FixTop, FixBottom)
//! - 屏幕固定 10px 行栅格（行高与字号解耦），混排字号不会上下重叠
//! - 二维碰撞：竖向像素重叠 × 横向深度（含追尾预测），按重叠面积挑最空的行
//! - 滚动弹幕按**时间序**入场投递（retainer 的前提），满屏时软重叠而非清轨
//! - 自适应窗口尺寸（布局跟随窗口大小变化，viewport 变化时可 relayout）
//! - FilterSystem 过滤链：类型屏蔽 / 数量密度 / 关键词 / 去重合并
//! - max_lines 行数限制

use crate::dfm_core::filters::{FilterContext, FilterSystem};
use crate::dfm_core::model::{
    DanmakuItem as DfmItem, DanmakuType as DfmType, Duration, GlobalFlags,
};
use crate::dfm_core::retainer::{row_span_for, total_rows_for, DanmakuRetainer, GRATING};
use crate::types::{DanmakuConfig, DanmakuInstance, DanmakuItem, DanmakuMode};

/// 弹幕区上下留白（像素），同时是行栅格的原点偏移
const MARGIN: f32 = 2.0;

// ─── 外部类型 ↔ DFM 内部类型转换 ────────────────────────────

fn mode_to_dfm_type(mode: DanmakuMode) -> DfmType {
    match mode {
        DanmakuMode::ScrollRight => DfmType::ScrollRL,
        DanmakuMode::ScrollLeft => DfmType::ScrollLR,
        DanmakuMode::TopFixed => DfmType::FixTop,
        DanmakuMode::BottomFixed => DfmType::FixBottom,
        DanmakuMode::Advanced => DfmType::ScrollRL, // fallback, will be skipped
    }
}

/// B 站 XML 的"标准"字号：`fontsize=25` 就是一档，其余值按它相对缩放
pub const SOURCE_REFERENCE_FONT_SIZE: f32 = 25.0;

/// 弹幕基准字号（逻辑像素）—— 25 号源弹幕上屏时的物理大小
///
/// 与画布尺寸**无关**：这是"固定参考字号"策略，和 B 站客户端/参考实现
/// (`effective_font_size(source, base, scale)`) 一致。以前的做法是按画面矩形高度
/// 换算（`area_height / 1080`），弹幕改成整窗布局后那条基准不再成立——窗口高度
/// 里含有 letterbox 黑边，按它放大只会让字在高窗口里糊满屏幕。
pub const DEFAULT_BASE_FONT_SIZE: f32 = 30.0;

/// 字号缩放系数下限（超小基准字号的兜底）
const FONT_SCALE_MIN: f32 = 0.4;
/// 字号缩放系数上限（超大基准字号的兜底，避免轨道数爆炸）
const FONT_SCALE_MAX: f32 = 4.0;

/// 固定参考字号策略下的缩放系数：`基准字号 / 25 × 窗口 DPR`
///
/// 布局（占位宽度、轨道高度）和光栅化必须用**同一个**系数，否则碰撞检测按一个
/// 宽度算、实际按另一个宽度画，表现为弹幕重叠或异常空隙。
///
/// `scale_factor` 是非有限值或非正数时按 1.0；`base_font_size` 同理回落到默认基准。
pub fn reference_font_scale(scale_factor: f32, base_font_size: f32) -> f32 {
    let base = if base_font_size.is_finite() && base_font_size > 0.0 {
        base_font_size
    } else {
        DEFAULT_BASE_FONT_SIZE
    };
    let factor = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    (base / SOURCE_REFERENCE_FONT_SIZE * factor).clamp(FONT_SCALE_MIN, FONT_SCALE_MAX)
}

fn item_to_dfm(
    item: &DanmakuItem,
    index: u32,
    scroll_dur_ms: i64,
    fixed_dur_ms: i64,
    font_scale: f32,
) -> DfmItem {
    let dfm_type = mode_to_dfm_type(item.mode);
    let duration_ms = match item.mode {
        DanmakuMode::ScrollRight | DanmakuMode::ScrollLeft => scroll_dur_ms,
        DanmakuMode::TopFixed | DanmakuMode::BottomFixed => fixed_dur_ms,
        DanmakuMode::Advanced => scroll_dur_ms,
    };
    let mut dfm = DfmItem::new(
        item.time_ms as i64,
        item.text.clone(),
        item.color,
        item.font_size as f32 * font_scale,
        dfm_type,
        duration_ms,
    );
    dfm.index = index;
    dfm
}

// ─── 布局引擎 ───────────────────────────────────────────────

/// 布局引擎 — 将弹幕列表转换为带坐标的实例列表
pub struct LayoutEngine {
    config: DanmakuConfig,
}

impl LayoutEngine {
    pub fn new(config: DanmakuConfig) -> Self {
        Self { config }
    }

    /// 估算文本宽度（像素）
    ///
    /// 使用 DFM+ 的估算公式：CJK=1.0em, ASCII=0.55em, whitespace=0.35em, *1.15 系数
    pub fn estimate_text_width(text: &str, font_size: u32) -> f32 {
        crate::dfm_core::model::measure_text_width(text, font_size as f32) * 1.15
    }

    /// 对弹幕列表进行布局，生成实例列表
    ///
    /// 使用 DFM+ DanmakuRetainer 做栅格碰撞避让，支持：
    /// - 屏幕固定行栅格 + 二维重叠面积碰撞
    /// - 满屏软重叠（可关）；本路径不送 `is_me`，因此不会挤掉已上屏弹幕
    /// - 四种弹幕类型独立栅格
    /// - FilterSystem 过滤链
    /// - max_lines 行数限制
    pub fn layout(&self, items: &[DanmakuItem]) -> Vec<DanmakuInstance> {
        let view_w = self.config.viewport_width;
        let view_h = self.config.viewport_height;
        let scroll_dur_ms = (self.config.scroll_duration * 1000.0) as i64;
        let fixed_dur_ms = (self.config.fixed_duration * 1000.0) as i64;
        let display_area = self.config.display_area.clamp(0.1, 1.0);
        let track_gap_ratio = self.config.track_gap_ratio.clamp(0.0, 2.0);
        let outline_multiplier = self.config.outline_multiplier.clamp(0.0, 4.0);

        let flags = GlobalFlags::default();
        // 布局和光栅化共用这一个系数，否则占位宽度与实绘宽度不一致，
        // 轨道碰撞检测会算错（弹幕重叠 / 留异常空隙）
        let font_scale = self.config.font_scale;

        // ── 1. 构建 DFM 内部 item 列表 ──
        let mut dfm_items: Vec<(u32, DfmItem)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                if matches!(item.mode, DanmakuMode::Advanced) {
                    return None;
                }
                let mut dfm = item_to_dfm(item, i as u32, scroll_dur_ms, fixed_dur_ms, font_scale);
                let outline_px =
                    resolve_outline_px(item.font_size as f32 * font_scale, outline_multiplier);
                dfm.measure_with_outline(view_w, view_h, &flags, outline_px);
                Some((i as u32, dfm))
            })
            .collect();

        // ── 2. 设置 FilterSystem ──
        let mut filter_sys = FilterSystem::default();
        if let Some(q) = self.config.max_quantity {
            filter_sys.max_quantity = Some(q);
        }
        if let Some(max) = self.config.max_lines {
            for ty in [
                DfmType::ScrollRL,
                DfmType::ScrollLR,
                DfmType::FixTop,
                DfmType::FixBottom,
            ] {
                filter_sys.max_lines.insert(ty, max);
            }
        }
        for mode in &self.config.blocked_types {
            filter_sys.blocked_types.insert(mode_to_dfm_type(*mode));
        }
        filter_sys.duplicate_merge = self.config.merge_duplicates;
        filter_sys.set_block_words(&self.config.block_words);

        // ── 3. 运行主过滤器 ──
        let scroll_duration = Duration::new(scroll_dur_ms);
        let mut ctx = FilterContext {
            timer_ms: 0,
            index_in_screen: 0,
            screen_size: dfm_items.len(),
            frame_elapsed_ms: 0,
            global_flags: flags,
            scroll_duration,
        };
        for (i, (_, item)) in dfm_items.iter_mut().enumerate() {
            if item.is_filtered {
                continue;
            }
            ctx.index_in_screen = i;
            filter_sys.filter_primary(item, &ctx);
        }

        // ── 4. 栅格碰撞避让 ──
        let mut retainer = DanmakuRetainer::new(MARGIN, track_gap_ratio);

        let mut instances: Vec<(usize, DanmakuInstance)> = Vec::with_capacity(dfm_items.len());
        // 挤掉的旧弹幕索引（原始 item 索引），最终从实例列表中移除
        let mut displaced_set: std::collections::HashSet<usize> = std::collections::HashSet::new();

        // retainer 的前提：按时间序入场投递（`recent` 靠投递顺序做摊销压缩，
        // 碰撞面积预测假设先投的弹幕不会晚于后投的开始）
        let mut feed_order: Vec<usize> = (0..dfm_items.len()).collect();
        feed_order.sort_by_key(|&pos| (dfm_items[pos].1.time_ms, dfm_items[pos].0));

        for pos in feed_order {
            let (orig_idx, dfm) = &mut dfm_items[pos];
            let orig_idx = *orig_idx as usize;
            if dfm.is_filtered {
                continue;
            }

            let (placed, displaced) = retainer.fix_with_options(
                dfm,
                view_w,
                view_h,
                &flags,
                display_area,
                false,
                self.config.allow_stacking,
                self.config.allow_scroll_overwrite,
            );

            if !placed {
                tracing::debug!(
                    "layout: item {} dropped (no free row), type={:?}",
                    orig_idx,
                    dfm.danmaku_type
                );
                continue;
            }

            // max_lines 检查：把栅格行号折算成"第几行弹幕"（一条弹幕占
            // span 个栅格行），底部弹幕从窗口下边缘往上数
            if let Some(max) = self.config.max_lines {
                if max > 0 {
                    let is_bottom = dfm.danmaku_type == DfmType::FixBottom;
                    let effective_height = if dfm.danmaku_type.is_scroll() {
                        view_h * display_area
                    } else {
                        view_h
                    };
                    let total_rows = total_rows_for(effective_height);
                    let span = row_span_for(dfm.paint_height, track_gap_ratio, total_rows);
                    let line_height = (span as f32 * GRATING).max(GRATING);
                    let line = if is_bottom {
                        ((effective_height - MARGIN - (dfm.y + dfm.paint_height)) / line_height)
                            .floor()
                            .max(0.0) as u32
                    } else {
                        ((dfm.y - MARGIN) / line_height).floor().max(0.0) as u32
                    };
                    if line >= max {
                        continue;
                    }
                }
            }

            // 收集被挤掉的弹幕索引，循环后统一回写移除
            displaced_set.extend(displaced);

            // 从原始索引取 DanmakuItem
            let orig_item = &items[orig_idx];
            let start_time = orig_item.time_ms as f64 / 1000.0;
            let text_width = dfm.paint_width;
            let grating_row = (dfm.y / GRATING).floor() as i32;

            let instance = match orig_item.mode {
                DanmakuMode::ScrollRight => {
                    let end_time = start_time + self.config.scroll_duration;
                    DanmakuInstance {
                        item: orig_item.clone(),
                        track: grating_row,
                        start_time,
                        end_time,
                        start_x: view_w,
                        end_x: -text_width,
                        y: dfm.y,
                        text_width,
                    }
                }
                DanmakuMode::ScrollLeft => {
                    let end_time = start_time + self.config.scroll_duration;
                    DanmakuInstance {
                        item: orig_item.clone(),
                        track: grating_row,
                        start_time,
                        end_time,
                        start_x: -text_width,
                        end_x: view_w,
                        y: dfm.y,
                        text_width,
                    }
                }
                DanmakuMode::TopFixed => {
                    let end_time = start_time + self.config.fixed_duration;
                    let center_x = (view_w - text_width) / 2.0;
                    DanmakuInstance {
                        item: orig_item.clone(),
                        track: grating_row,
                        start_time,
                        end_time,
                        start_x: center_x,
                        end_x: center_x,
                        y: dfm.y,
                        text_width,
                    }
                }
                DanmakuMode::BottomFixed => {
                    let end_time = start_time + self.config.fixed_duration;
                    let center_x = (view_w - text_width) / 2.0;
                    DanmakuInstance {
                        item: orig_item.clone(),
                        track: grating_row,
                        start_time,
                        end_time,
                        start_x: center_x,
                        end_x: center_x,
                        y: dfm.y,
                        text_width,
                    }
                }
                DanmakuMode::Advanced => unreachable!(),
            };

            instances.push((orig_idx, instance));
        }

        // displaced 回写：被后来弹幕挤掉行的旧弹幕不再上屏，否则同一条带上
        // 会有两条弹幕互相重叠
        let placed_total = instances.len();
        instances.sort_by_key(|(orig_idx, _)| *orig_idx);
        let instances: Vec<DanmakuInstance> = instances
            .into_iter()
            .filter(|(orig_idx, _)| !displaced_set.contains(orig_idx))
            .map(|(_, inst)| inst)
            .collect();
        let displaced_removed = placed_total - instances.len();
        if displaced_removed > 0 {
            tracing::debug!(
                "layout: {} items displaced (removed) by later danmaku",
                displaced_removed
            );
        }

        tracing::info!(
            "layout: {} items -> {} instances (displaced={}, viewport={}x{}, display_area={:.1}, filters={})",
            items.len(),
            instances.len(),
            displaced_removed,
            view_w,
            view_h,
            display_area,
            !self.config.block_words.is_empty() || self.config.max_quantity.is_some()
        );

        instances
    }

    /// 获取给定时间点的活跃弹幕
    pub fn active_at(instances: &[DanmakuInstance], time_secs: f64) -> Vec<&DanmakuInstance> {
        instances
            .iter()
            .filter(|inst| time_secs >= inst.start_time && time_secs < inst.end_time)
            .collect()
    }

    /// 计算滚动弹幕在当前时间的 x 坐标
    pub fn current_x(instance: &DanmakuInstance, time_secs: f64) -> f32 {
        let elapsed = time_secs - instance.start_time;
        let duration = instance.end_time - instance.start_time;
        if duration <= 0.0 {
            return instance.start_x;
        }
        let progress = (elapsed / duration) as f32;
        instance.start_x + (instance.end_x - instance.start_x) * progress
    }

    /// 从已布局的实例列表构建预处理布局（两阶段架构）
    pub fn prepare(&self, instances: &[DanmakuInstance]) -> PreparedLayout {
        PreparedLayout::from_instances(instances, &self.config)
    }

    /// 使用新 viewport 尺寸重新布局（自适应窗口）
    ///
    /// 当窗口大小变化时调用，使用原始弹幕数据重新执行 layout + prepare。
    pub fn relayout(
        items: &[DanmakuItem],
        new_width: f32,
        new_height: f32,
        config: &DanmakuConfig,
    ) -> (Vec<DanmakuInstance>, PreparedLayout) {
        let new_config = DanmakuConfig {
            viewport_width: new_width,
            viewport_height: new_height,
            // font_scale 由调用方按固定参考字号给出（见 reference_font_scale）：
            // 它与画布尺寸无关，这里再按 new_height 重算等于把字号绑回窗口大小。
            font_scale: config.font_scale,
            ..config.clone()
        };
        let engine = LayoutEngine::new(new_config);
        let instances = engine.layout(items);
        let prepared = engine.prepare(&instances);
        (instances, prepared)
    }
}

// ─── Prepare / Frame-Query 两阶段架构 ───────────────────────────────

/// 预处理后的布局项 — 从 DanmakuInstance 提取渲染所需信息
#[derive(Debug, Clone)]
pub struct PreparedItem {
    /// 原始 DanmakuInstance 在列表中的索引
    pub index: usize,
    /// 出现时间（秒）
    pub start_time: f64,
    /// 消失时间（秒）
    pub end_time: f64,
    /// 起始 x 坐标
    pub start_x: f32,
    /// 终止 x 坐标（滚动弹幕有效）
    pub end_x: f32,
    /// y 坐标
    pub y: f32,
    /// 估算文本宽度（像素）
    pub text_width: f32,
    /// 字体大小
    pub font_size: u32,
    /// 颜色 (0xRRGGBB)
    pub color: u32,
    /// 是否为滚动弹幕
    pub is_scroll: bool,
    /// 弹幕文本
    pub text: String,
}

/// 预处理后的布局 — 只构建一次，支持多次 frame query
#[derive(Debug, Clone)]
pub struct PreparedLayout {
    /// 按时间排序的布局项
    pub items: Vec<PreparedItem>,
    /// start_time 数组（供二分查找）
    pub item_times: Vec<f64>,
    /// 滚动弹幕持续时间（秒）
    pub scroll_duration: f64,
    /// 固定弹幕持续时间（秒）
    pub static_duration: f64,
    /// 布局时使用的 viewport 尺寸
    pub viewport_width: f32,
    pub viewport_height: f32,
}

/// 单帧查询结果
#[derive(Debug, Clone, Default)]
pub struct FrameLayout {
    /// 当前帧可见的弹幕
    pub items: Vec<FrameItem>,
}

/// 单帧中一条弹幕的屏幕位置
#[derive(Debug, Clone, Copy)]
pub struct FrameItem {
    /// 对应 PreparedLayout.items 中的索引
    pub item_index: usize,
    /// 当前帧的 x 坐标（像素）
    pub x: f32,
    /// y 坐标（像素）
    pub y: f32,
}

impl PreparedLayout {
    /// 创建空的预处理布局
    pub fn empty(scroll_duration: f64, static_duration: f64) -> Self {
        Self {
            items: Vec::new(),
            item_times: Vec::new(),
            scroll_duration,
            static_duration,
            viewport_width: 0.0,
            viewport_height: 0.0,
        }
    }

    /// 从 DanmakuInstance 列表构建预处理布局
    pub fn from_instances(instances: &[DanmakuInstance], config: &DanmakuConfig) -> Self {
        let mut items: Vec<PreparedItem> = instances
            .iter()
            .enumerate()
            .map(|(i, inst)| {
                let is_scroll = matches!(
                    inst.item.mode,
                    DanmakuMode::ScrollRight | DanmakuMode::ScrollLeft
                );
                PreparedItem {
                    index: i,
                    start_time: inst.start_time,
                    end_time: inst.end_time,
                    start_x: inst.start_x,
                    end_x: inst.end_x,
                    y: inst.y,
                    text_width: inst.text_width,
                    // 必须存**缩放后**的字号：光栅化直接拿这个值来选字形尺寸。
                    // 存原始值会让"占位宽度按缩放算、实际字形按原始像素画"，
                    // 两者不一致 —— 这正是文字看起来被压窄的直接原因。
                    font_size: (inst.item.font_size as f32 * config.font_scale).round() as u32,
                    color: inst.item.color,
                    is_scroll,
                    text: inst.item.text.clone(),
                }
            })
            .collect();

        // 按 start_time 排序
        items.sort_by(|a, b| {
            a.start_time
                .partial_cmp(&b.start_time)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let item_times: Vec<f64> = items.iter().map(|item| item.start_time).collect();

        tracing::info!(
            "PreparedLayout: {} items prepared, scroll_dur={:.1}s static_dur={:.1}s viewport={}x{}",
            items.len(),
            config.scroll_duration,
            config.fixed_duration,
            config.viewport_width,
            config.viewport_height,
        );

        Self {
            items,
            item_times,
            scroll_duration: config.scroll_duration,
            static_duration: config.fixed_duration,
            viewport_width: config.viewport_width,
            viewport_height: config.viewport_height,
        }
    }

    /// 二分查找：查询当前时间可见的弹幕 — O(log n + k)
    pub fn query_frame(&self, current_time: f64) -> FrameLayout {
        if self.items.is_empty() {
            return FrameLayout::default();
        }

        let max_dur = self.scroll_duration.max(self.static_duration);
        let window_start = current_time - max_dur;

        // 二分查找：第一个 start_time >= window_start 的索引
        let start_idx = lower_bound(&self.item_times, window_start);
        // 二分查找：第一个 start_time > current_time 的索引
        let end_idx = upper_bound(&self.item_times, current_time);

        let mut frame_items = Vec::with_capacity(end_idx.saturating_sub(start_idx));

        for i in start_idx..end_idx {
            let item = &self.items[i];
            let elapsed = current_time - item.start_time;

            // 尚未开始
            if elapsed < 0.0 {
                continue;
            }

            // 固定弹幕已过期
            if !item.is_scroll && elapsed > (item.end_time - item.start_time) {
                continue;
            }

            // 滚动弹幕已离开屏幕
            if item.is_scroll && item.x_at(current_time) < -item.text_width {
                continue;
            }

            frame_items.push(FrameItem {
                item_index: i,
                x: item.x_at(current_time),
                y: item.y,
            });
        }

        FrameLayout { items: frame_items }
    }
}

impl PreparedItem {
    /// 计算当前时间的 x 坐标
    ///
    /// 线性插值等价于 DFM 的 get_r2l_x / get_l2r_x 公式：
    /// - ScrollRL: x = viewport_width - elapsed * step_x
    /// - ScrollLR: x = elapsed * step_x - paint_width
    /// - Fixed: x = center (恒定)
    pub fn x_at(&self, current_time: f64) -> f32 {
        let elapsed = current_time - self.start_time;
        let duration = self.end_time - self.start_time;
        if duration <= 0.0 {
            return self.start_x;
        }
        let progress = (elapsed / duration) as f32;
        self.start_x + (self.end_x - self.start_x) * progress
    }
}

/// Compute outline width in pixels based on font size and multiplier.
fn resolve_outline_px(font_size: f32, outline_multiplier: f32) -> f32 {
    if outline_multiplier <= 0.0 || !outline_multiplier.is_finite() {
        return 0.0;
    }
    (font_size * 0.06).clamp(1.0, 2.6) * outline_multiplier
}

/// 二分查找：第一个 >= target 的索引
fn lower_bound(times: &[f64], target: f64) -> usize {
    times.partition_point(|&t| t < target)
}

/// 二分查找：第一个 > target 的索引
fn upper_bound(times: &[f64], target: f64) -> usize {
    times.partition_point(|&t| t <= target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DanmakuMode;

    fn make_item(time_ms: u64, mode: DanmakuMode, text: &str) -> DanmakuItem {
        DanmakuItem {
            time_ms,
            mode,
            font_size: 25,
            color: 0xFFFFFF,
            text: text.to_string(),
        }
    }

    #[test]
    fn test_estimate_text_width_ascii() {
        let w = LayoutEngine::estimate_text_width("hello", 25);
        // DFM+ formula: 5 ASCII * 0.55 * 25 * 1.15 ≈ 79.1
        assert!(w > 60.0 && w < 100.0, "w={}", w);
    }

    #[test]
    fn test_estimate_text_width_cjk() {
        let w = LayoutEngine::estimate_text_width("你好", 25);
        // DFM+ formula: 2 CJK * 1.0 * 25 * 1.15 = 57.5
        assert!(w > 45.0 && w < 70.0, "w={}", w);
    }

    #[test]
    fn test_estimate_text_width_mixed() {
        let w = LayoutEngine::estimate_text_width("hi你", 25);
        // DFM+: (2*0.55 + 1*1.0) * 25 * 1.15 = 2.1 * 25 * 1.15 = 60.4
        assert!(w > 50.0 && w < 75.0, "w={}", w);
    }

    #[test]
    fn test_layout_scroll_right() {
        let config = DanmakuConfig::default();
        let engine = LayoutEngine::new(config);
        let items = vec![make_item(0, DanmakuMode::ScrollRight, "test")];
        let instances = engine.layout(&items);
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].start_x, 1920.0);
        assert!(instances[0].end_x < 0.0);
        assert_eq!(instances[0].start_time, 0.0);
        assert_eq!(instances[0].end_time, 8.0);
    }

    #[test]
    fn test_layout_top_fixed() {
        let config = DanmakuConfig::default();
        let engine = LayoutEngine::new(config);
        let items = vec![make_item(1000, DanmakuMode::TopFixed, "fixed")];
        let instances = engine.layout(&items);
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].start_time, 1.0);
        assert_eq!(instances[0].end_time, 5.0); // 1.0 + 4.0
        assert_eq!(instances[0].start_x, instances[0].end_x); // 固定弹幕不移动
    }

    #[test]
    fn test_layout_track_allocation() {
        let config = DanmakuConfig {
            viewport_width: 100.0,
            viewport_height: 200.0,
            scroll_duration: 2.0,
            ..Default::default()
        };
        let engine = LayoutEngine::new(config);
        // 同时发出多条弹幕，应分配到不同轨道
        let items = vec![
            make_item(0, DanmakuMode::ScrollRight, "a"),
            make_item(100, DanmakuMode::ScrollRight, "b"),
            make_item(200, DanmakuMode::ScrollRight, "c"),
        ];
        let instances = engine.layout(&items);
        assert_eq!(instances.len(), 3);
        // DFM+ retainer 应为前几条同时弹幕分配不同 y 坐标
        assert_ne!(instances[0].y, instances[1].y);
    }

    #[test]
    fn test_active_at() {
        let config = DanmakuConfig::default();
        let engine = LayoutEngine::new(config);
        let items = vec![
            make_item(0, DanmakuMode::ScrollRight, "early"),
            make_item(5000, DanmakuMode::ScrollRight, "mid"),
            make_item(10000, DanmakuMode::ScrollRight, "late"),
        ];
        let instances = engine.layout(&items);

        // scroll_duration = 8.0s, so:
        // "early": [0.0, 8.0), "mid": [5.0, 13.0), "late": [10.0, 18.0)

        // 只有 "early" 在时间 2.0 活跃
        let active = LayoutEngine::active_at(&instances, 2.0);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].item.text, "early");

        // 时间 6.0: "early" (0~8) 和 "mid" (5~13) 都活跃
        let active = LayoutEngine::active_at(&instances, 6.0);
        assert_eq!(active.len(), 2);

        // 时间 9.0: 只有 "mid" (5~13) 活跃
        let active = LayoutEngine::active_at(&instances, 9.0);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].item.text, "mid");

        // 时间 20.0: 无活跃弹幕
        let active = LayoutEngine::active_at(&instances, 20.0);
        assert!(active.is_empty());
    }

    #[test]
    fn test_current_x_scroll() {
        let instance = DanmakuInstance {
            item: make_item(0, DanmakuMode::ScrollRight, "test"),
            track: 0,
            start_time: 0.0,
            end_time: 8.0,
            start_x: 1920.0,
            end_x: -100.0,
            y: 0.0,
            text_width: 100.0,
        };
        // 在起始时间，x 应该等于 start_x
        let x = LayoutEngine::current_x(&instance, 0.0);
        assert!((x - 1920.0).abs() < 1.0);
        // 在中间时间，x 应该在中间
        let x = LayoutEngine::current_x(&instance, 4.0);
        assert!(x > -100.0 && x < 1920.0);
    }

    // ─── Prepare / Frame-Query 测试 ───────────────────────────────

    #[test]
    fn test_prepared_layout_from_instances() {
        let config = DanmakuConfig::default();
        let engine = LayoutEngine::new(config.clone());
        let items = vec![
            make_item(0, DanmakuMode::ScrollRight, "first"),
            make_item(5000, DanmakuMode::ScrollRight, "second"),
            make_item(10000, DanmakuMode::TopFixed, "third"),
        ];
        let instances = engine.layout(&items);
        let prepared = engine.prepare(&instances);

        assert_eq!(prepared.items.len(), 3);
        // 应该按时间排序
        assert!(prepared.item_times.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(prepared.scroll_duration, config.scroll_duration);
        assert_eq!(prepared.static_duration, config.fixed_duration);
    }

    #[test]
    fn test_prepared_layout_query_frame() {
        let config = DanmakuConfig::default();
        let engine = LayoutEngine::new(config);
        let items = vec![
            make_item(0, DanmakuMode::ScrollRight, "early"),
            make_item(5000, DanmakuMode::ScrollRight, "mid"),
            make_item(10000, DanmakuMode::ScrollRight, "late"),
        ];
        let instances = engine.layout(&items);
        let prepared = engine.prepare(&instances);

        // 时间 2.0: 只有 "early" (0~8s)
        let frame = prepared.query_frame(2.0);
        assert_eq!(frame.items.len(), 1);

        // 时间 6.0: "early" (0~8) 和 "mid" (5~13)
        let frame = prepared.query_frame(6.0);
        assert_eq!(frame.items.len(), 2);

        // 时间 9.0: 只有 "mid" (5~13)
        let frame = prepared.query_frame(9.0);
        assert_eq!(frame.items.len(), 1);

        // 时间 20.0: 无活跃弹幕
        let frame = prepared.query_frame(20.0);
        assert!(frame.items.is_empty());
    }

    #[test]
    fn test_prepared_layout_empty() {
        let prepared = PreparedLayout::empty(8.0, 4.0);
        let frame = prepared.query_frame(5.0);
        assert!(frame.items.is_empty());
    }

    /// 弹幕字号随**弹幕区域（画面矩形）高度**缩放，且布局与光栅化用同一个系数
    ///
    /// 弹幕只在画面矩形内排布，所以字号基准是画面高度而不是窗口/内容分辨率：
    /// 文字相对**画面**的比例因此与窗口比例无关（宽窗口的左右 pillarbox 不会把
    /// 弹幕拉出去，竖屏 letterbox 也不会让字号相对画面突然变大）。
    ///
    /// 另一条独立约束：`PreparedItem.font_size` 必须存**缩放后**的值。存原始值会让
    /// 弹幕字号走**固定参考**：基准字号 / 25 × 窗口 DPR，与画布尺寸无关
    ///
    /// 画布现在是整个窗口（含 letterbox 黑边），按画布高度放大会让高窗口里的字糊满
    /// 屏幕；按 DPR 放大才能在高分屏上保持同一种观感。基准 30 对应 B 站源字号 25，
    /// 与参考实现 `effective_font_size(source, base, scale)` 一致。
    ///
    /// 另一条独立约束：`PreparedItem.font_size` 必须存**缩放后**的值。存原始值会让
    /// 「占位宽度按缩放算、实际字形按原始像素画」不一致，轨道碰撞检测因此算错，
    /// 表现为弹幕被压窄。
    #[test]
    fn font_size_is_fixed_reference_and_shared_by_layout_and_raster() {
        let items = vec![make_item(0, DanmakuMode::ScrollRight, "测试弹幕")];

        // 默认：基准 30、DPR 1 → 源字号 25 的弹幕就是 30px
        let prepared = LayoutEngine::new(DanmakuConfig::default())
            .prepare(&LayoutEngine::new(DanmakuConfig::default()).layout(&items));
        assert_eq!(prepared.items[0].font_size, 30, "源 25 应落到基准 30");

        // 画布变大只增加轨道与飞行距离，字号不变
        let cfg_small = DanmakuConfig {
            viewport_width: 960.0,
            viewport_height: 540.0,
            ..Default::default()
        };
        let engine_small = LayoutEngine::new(cfg_small);
        let prepared_small = engine_small.prepare(&engine_small.layout(&items));
        assert_eq!(
            prepared_small.items[0].font_size, 30,
            "小窗口不得缩小字号（got {}）",
            prepared_small.items[0].font_size
        );

        let cfg_tall = DanmakuConfig {
            viewport_width: 1080.0,
            viewport_height: 2340.0,
            ..Default::default()
        };
        let engine_tall = LayoutEngine::new(cfg_tall);
        let prepared_tall = engine_tall.prepare(&engine_tall.layout(&items));
        assert_eq!(
            prepared_tall.items[0].font_size, 30,
            "高窗口不得放大字号（got {}）",
            prepared_tall.items[0].font_size
        );

        // DPR 2 的物理尺寸必须翻倍，否则高分屏上字只有一半大
        assert!((reference_font_scale(2.0, 30.0) - 2.4).abs() < 1e-6);
        assert!((25.0 * reference_font_scale(2.0, 30.0) - 60.0).abs() < 0.01);

        // 源字号相对 25 缩放：50 号弹幕在基准 30 下是 60
        let mut big = make_item(0, DanmakuMode::ScrollRight, "大字");
        big.font_size = 50;
        let engine = LayoutEngine::new(DanmakuConfig::default());
        let prepared_big = engine.prepare(&engine.layout(&[big]));
        assert_eq!(prepared_big.items[0].font_size, 60);
    }

    /// 弹幕按**整个窗口**布局，横穿黑边是预期行为
    ///
    /// 取代旧的"必须落在画面矩形内"约束：弹幕属于窗口而不属于画面。这里守住的是
    /// 新的边界 —— 坐标不得越出 surface，且轨道/进出场都用 surface 尺寸。
    ///
    /// 几何选 letterbox（1080x2340 播 16:9 → 画面约 1080x608，居中在 y≈866..1474）：
    /// 画面上下都留得出黑边，垂直方向的"越出画面"才可观测。pillarbox 的窗口高度
    /// 恒等于画面高度，用它断言垂直边界只会得到假失败。
    #[test]
    fn layout_spans_the_whole_window() {
        let area = dyl_render::OverlayArea::new(1080, 2340, 1920, 1080);
        let (canvas_x, canvas_y, canvas_w, canvas_h) = area.surface_rect();
        assert_eq!((canvas_x, canvas_y), (0.0, 0.0));
        let (_pic_x, pic_y, pic_w, pic_h) = area.video_rect;
        let picture_bottom = pic_y + pic_h;
        assert!(
            picture_bottom < canvas_h,
            "这组几何必须在画面下方留出黑边，picture_bottom={picture_bottom} canvas_h={canvas_h}"
        );
        assert!(
            pic_w <= canvas_w && pic_h <= canvas_h,
            "画面矩形不得越出窗口"
        );

        let cfg = DanmakuConfig {
            viewport_width: canvas_w,
            viewport_height: canvas_h,
            ..Default::default()
        };
        let engine = LayoutEngine::new(cfg);

        // 滚动弹幕从**窗口**右边缘进场，不是画面右边缘
        let scroll = engine.layout(&[make_item(0, DanmakuMode::ScrollRight, "滚动弹幕")]);
        assert_eq!(
            scroll[0].start_x, canvas_w,
            "滚动起点应等于窗口宽度，不是画面矩形宽度"
        );

        // 底部固定弹幕贴着窗口底边 —— 也就是画在下方黑边里
        let bottom = engine.layout(&[make_item(0, DanmakuMode::BottomFixed, "底部弹幕")]);
        assert!(
            bottom[0].y > picture_bottom,
            "底部固定弹幕应进入画面下方的黑边，y={} picture_bottom={}",
            bottom[0].y,
            picture_bottom
        );

        // 顶部固定弹幕贴着窗口顶边（画面上方的黑边里）
        let top = engine.layout(&[make_item(0, DanmakuMode::TopFixed, "顶部弹幕")]);
        assert!(
            top[0].y < pic_y,
            "顶部固定弹幕应进入画面上方的黑边，y={} pic_y={}",
            top[0].y,
            pic_y
        );

        // 唯一的硬边界：不得越出窗口
        for inst in scroll.iter().chain(bottom.iter()).chain(top.iter()) {
            assert!(
                inst.start_x >= 0.0 && inst.start_x <= canvas_w,
                "起点越出窗口：x={} w={}",
                inst.start_x,
                canvas_w
            );
            assert!(
                inst.y >= 0.0 && inst.y < canvas_h,
                "轨道越出窗口：y={} h={}",
                inst.y,
                canvas_h
            );
        }
    }

    /// relayout 只改位置，字号由调用方给的系数决定
    #[test]
    fn relayout_changes_positions_and_uses_caller_font_scale() {
        let items = vec![make_item(0, DanmakuMode::ScrollRight, "test")];

        let cfg = DanmakuConfig {
            font_scale: reference_font_scale(1.0, DEFAULT_BASE_FONT_SIZE),
            ..Default::default()
        };

        let engine = LayoutEngine::new(cfg.clone());
        let before = engine.prepare(&engine.layout(&items));

        // 把窗口缩小一半
        let (instances, after) = LayoutEngine::relayout(&items, 960.0, 540.0, &cfg);

        // 调用方给的系数被原样保留（窗口变了不等于字号要变）
        assert_eq!(
            after.items[0].font_size, before.items[0].font_size,
            "relayout 不得自行改写 font_scale"
        );
        // 位置必须跟着新窗口尺寸走
        assert_eq!(instances[0].start_x, 960.0, "滚动起点应等于新的窗口宽度");
    }

    /// 固定参考字号的系数：异常输入回落基准，极端值必须夹住
    #[test]
    fn reference_font_scale_clamps_and_guards_bad_input() {
        // DPR 未知（0 / NaN）按 1.0；基准字号非法回落默认 30
        let default_scale = DEFAULT_BASE_FONT_SIZE / SOURCE_REFERENCE_FONT_SIZE;
        for scale in [
            reference_font_scale(0.0, 30.0),
            reference_font_scale(f32::NAN, 30.0),
        ] {
            assert!(
                (scale - default_scale).abs() < 1e-6,
                "DPR 异常应回落 1.0，got {scale}"
            );
        }
        for scale in [
            reference_font_scale(1.0, 0.0),
            reference_font_scale(1.0, f32::NAN),
        ] {
            assert!(
                (scale - default_scale).abs() < 1e-6,
                "基准字号异常应回落默认值，got {scale}"
            );
        }
        // 夹住上下限，否则小字变大字、轨道数爆炸
        assert_eq!(reference_font_scale(1000.0, 30.0), 4.0);
        assert_eq!(reference_font_scale(0.001, 1.0), 0.4);
        // DPR 2 必须把物理字号翻倍
        assert!((reference_font_scale(2.0, 30.0) - 2.4).abs() < 1e-6);
    }

    #[test]
    fn test_frame_item_x_coordinate() {
        let config = DanmakuConfig::default();
        let engine = LayoutEngine::new(config);
        let items = vec![make_item(0, DanmakuMode::ScrollRight, "test")];
        let instances = engine.layout(&items);
        let prepared = engine.prepare(&instances);

        // 起始时刻 x 应该接近 start_x (1920)
        let frame = prepared.query_frame(0.0);
        assert_eq!(frame.items.len(), 1);
        assert!((frame.items[0].x - 1920.0).abs() < 1.0);

        // 中间时刻 x 应该在中间
        let frame = prepared.query_frame(4.0);
        assert_eq!(frame.items.len(), 1);
        assert!(frame.items[0].x > -200.0 && frame.items[0].x < 1920.0);
    }

    #[test]
    fn test_relayout_viewport_change() {
        let config = DanmakuConfig::default();
        let items = vec![make_item(0, DanmakuMode::ScrollRight, "test")];

        // 初始 layout
        let engine = LayoutEngine::new(config.clone());
        let instances = engine.layout(&items);
        assert_eq!(instances[0].start_x, 1920.0);

        // relayout with new viewport
        let (new_instances, _prepared) = LayoutEngine::relayout(&items, 1280.0, 720.0, &config);
        assert_eq!(new_instances.len(), 1);
        assert_eq!(new_instances[0].start_x, 1280.0);
    }

    /// 满屏时的降级策略：软重叠，而不是挤掉已上屏的弹幕。
    /// 150px 高 → 15 栅格行；36px 弹幕占 6 行 → 最多 2 条零重叠，其余落到
    /// 最空的一行压着放。全部 10 条都必须留在实例列表里，且文本各出现一次。
    #[test]
    fn test_scroll_overload_soft_overlaps_without_displacing() {
        let config = DanmakuConfig {
            viewport_width: 1920.0,
            viewport_height: 150.0,
            scroll_duration: 8.0,
            ..Default::default()
        };
        let engine = LayoutEngine::new(config);
        let items: Vec<DanmakuItem> = (0..10)
            .map(|i| make_item(i * 10, DanmakuMode::ScrollRight, &format!("item{}", i)))
            .collect();
        let instances = engine.layout(&items);

        assert_eq!(
            instances.len(),
            items.len(),
            "overload must soft-overlap, never drop or displace"
        );
        let texts: Vec<&str> = instances.iter().map(|i| i.item.text.as_str()).collect();
        assert_eq!(
            texts.iter().collect::<std::collections::HashSet<_>>().len(),
            items.len(),
            "each danmaku must appear exactly once, got {:?}",
            texts
        );
        // y 必须落在栅格上（margin + n * GRATING），重叠只是行号相同，不是半行错位
        for inst in &instances {
            let row = (inst.y - MARGIN) / GRATING;
            assert!(
                (row - row.round()).abs() < 1e-3,
                "y={} is not on the {}px grating",
                inst.y,
                GRATING
            );
        }
    }

    /// 关掉软重叠后，满屏的后来者只能被丢弃：幸存实例的 y 必须两两不同，
    /// 且明显少于投递数（栅格装不下）。
    #[test]
    fn test_scroll_overload_without_overwrite_drops() {
        let config = DanmakuConfig {
            viewport_width: 1920.0,
            viewport_height: 150.0,
            scroll_duration: 8.0,
            allow_scroll_overwrite: false,
            ..Default::default()
        };
        let engine = LayoutEngine::new(config);
        let items: Vec<DanmakuItem> = (0..10)
            .map(|i| make_item(i * 10, DanmakuMode::ScrollRight, &format!("item{}", i)))
            .collect();
        let instances = engine.layout(&items);

        assert!(
            instances.len() >= 2,
            "15 rows / 6-row span must fit at least 2, got {}",
            instances.len()
        );
        assert!(
            instances.len() < items.len(),
            "overflow must be dropped when soft overlap is off, got all {}",
            instances.len()
        );
        let mut ys: Vec<f32> = instances.iter().map(|i| i.y).collect();
        ys.sort_by(f32::total_cmp);
        let dup = ys.windows(2).filter(|w| w[0] == w[1]).count();
        assert_eq!(
            dup, 0,
            "dropped-only mode must leave no shared row, ys={:?}",
            ys
        );
    }
}
