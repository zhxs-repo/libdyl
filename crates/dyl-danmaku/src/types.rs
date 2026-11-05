//! 弹幕数据模型 — 参考 Bilibili DanmakuFlameMaster (DFM) 的弹幕类型定义

use serde::{Deserialize, Serialize};

/// 弹幕运动模式 — 对应 DFM mode 字段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DanmakuMode {
    /// 右→左滚动（DFM mode 1/2/3，速度略有不同）
    ScrollRight,
    /// 左→右滚动
    ScrollLeft,
    /// 顶部固定（DFM mode 5）
    TopFixed,
    /// 底部固定（DFM mode 4）
    BottomFixed,
    /// 高级弹幕（DFM mode 7，指定坐标 + 运动轨迹）
    Advanced,
}

impl DanmakuMode {
    /// 从 B 站 XML p 属性的 mode 字段解析
    pub fn from_bilibili_mode(mode: u8) -> Self {
        match mode {
            1..=3 => DanmakuMode::ScrollRight,
            4 => DanmakuMode::BottomFixed,
            5 => DanmakuMode::TopFixed,
            6 => DanmakuMode::ScrollLeft,
            7 => DanmakuMode::Advanced,
            _ => DanmakuMode::ScrollRight,
        }
    }
}

/// 弹幕条目 — 一条弹幕的完整数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DanmakuItem {
    /// 出现时间（毫秒）
    pub time_ms: u64,
    /// 运动模式
    pub mode: DanmakuMode,
    /// 字体大小（逻辑像素，默认 25）
    pub font_size: u32,
    /// 颜色（0xRRGGBB，白色 = 0xFFFFFF）
    pub color: u32,
    /// 弹幕文本
    pub text: String,
}

/// 弹幕配置 — 控制布局、过滤和渲染行为
#[derive(Debug, Clone)]
pub struct DanmakuConfig {
    /// 布局画布宽度（surface 物理像素，即**整个窗口**；渲染时每帧按实际尺寸覆盖）
    pub viewport_width: f32,
    /// 布局画布高度（surface 物理像素，即**整个窗口**；渲染时每帧按实际尺寸覆盖）
    pub viewport_height: f32,
    /// 滚动弹幕穿越整个屏幕的时间（秒，默认 8.0）
    pub scroll_duration: f64,
    /// 固定弹幕显示时间（秒，默认 4.0）
    pub fixed_duration: f64,
    /// 全局不透明度（0.0 ~ 1.0，默认 1.0）
    pub opacity: f32,
    /// 是否启用弹幕
    pub enabled: bool,
    /// 显示区域比例（0.1 ~ 1.0，默认 1.0）—— 只压缩**滚动**弹幕的行池，
    /// 顶/底固定弹幕保持整窗几何（贴窗口上/下边缘）
    pub display_area: f32,
    /// 行高间距比例（0.0 ~ 2.0，默认 0.5）：一条弹幕占
    /// `ceil(paint_height × (1 + gap) / GRATING)` 个 10px 栅格行
    pub track_gap_ratio: f32,
    /// 每种类型最大行数（None = 不限制）
    pub max_lines: Option<u32>,
    /// 同屏最大弹幕数（None = 不限制）
    pub max_quantity: Option<u32>,
    /// 字号缩放系数（由渲染层按"固定参考字号"策略算出，调用方一般不用管）
    ///
    /// 弹幕文件的 `font_size` 是 B 站 XML 里的源像素（25 = 标准一档），最终像素字号
    /// = `源字号 × font_scale`，而 `font_scale = 基准字号 / 25 × 窗口 DPR`
    /// （见 [`crate::layout::reference_font_scale`]）。它**与画布尺寸无关**：缩放窗口
    /// 不会改字号，只有改基准字号或换到不同 DPR 的屏幕才会变。
    ///
    /// 布局（占位宽度、轨道高度）和光栅化必须用**同一个**系数，否则轨道碰撞
    /// 检测会按一个宽度算、实际按另一个宽度画，表现为弹幕重叠或异常空隙。
    pub font_scale: f32,
    /// 基准字号（逻辑像素，默认 30）—— 源字号 25 的弹幕上屏时的物理大小
    pub base_font_size: f32,
    /// 屏蔽的弹幕类型
    pub blocked_types: Vec<DanmakuMode>,
    /// 屏蔽关键词列表
    pub block_words: Vec<String>,
    /// 启用弹幕去重合并
    pub merge_duplicates: bool,
    /// 允许堆叠放置（按条目哈希把弹幕散布到栅格行，跳过碰撞检测）
    pub allow_stacking: bool,
    /// 允许滚动弹幕软重叠（栅格满时挑最空的一行压着放，而不是丢弃）
    pub allow_scroll_overwrite: bool,
    /// 描边宽度倍率（默认 1.0，0 = 无描边）
    pub outline_multiplier: f32,
}

impl Default for DanmakuConfig {
    fn default() -> Self {
        Self {
            viewport_width: 1920.0,
            viewport_height: 1080.0,
            scroll_duration: 8.0,
            fixed_duration: 4.0,
            opacity: 1.0,
            enabled: true,
            display_area: 1.0,
            track_gap_ratio: 0.5,
            max_lines: None,
            max_quantity: None,
            base_font_size: crate::layout::DEFAULT_BASE_FONT_SIZE,
            font_scale: crate::layout::reference_font_scale(
                1.0,
                crate::layout::DEFAULT_BASE_FONT_SIZE,
            ),
            blocked_types: Vec::new(),
            block_words: Vec::new(),
            merge_duplicates: false,
            allow_stacking: false,
            allow_scroll_overwrite: true,
            outline_multiplier: 1.0,
        }
    }
}

/// 布局后的弹幕实例 — 包含屏幕坐标和生命周期
#[derive(Debug, Clone)]
pub struct DanmakuInstance {
    /// 原始弹幕数据
    pub item: DanmakuItem,
    /// 分配的轨道索引（-1 表示未分配）
    pub track: i32,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bilibili_mode_mapping() {
        assert_eq!(DanmakuMode::from_bilibili_mode(1), DanmakuMode::ScrollRight);
        assert_eq!(DanmakuMode::from_bilibili_mode(2), DanmakuMode::ScrollRight);
        assert_eq!(DanmakuMode::from_bilibili_mode(3), DanmakuMode::ScrollRight);
        assert_eq!(DanmakuMode::from_bilibili_mode(4), DanmakuMode::BottomFixed);
        assert_eq!(DanmakuMode::from_bilibili_mode(5), DanmakuMode::TopFixed);
        assert_eq!(DanmakuMode::from_bilibili_mode(6), DanmakuMode::ScrollLeft);
        assert_eq!(DanmakuMode::from_bilibili_mode(7), DanmakuMode::Advanced);
        assert_eq!(
            DanmakuMode::from_bilibili_mode(99),
            DanmakuMode::ScrollRight
        );
    }

    #[test]
    fn test_danmaku_item_clone() {
        let item = DanmakuItem {
            time_ms: 1000,
            mode: DanmakuMode::ScrollRight,
            font_size: 25,
            color: 0xFFFFFF,
            text: "hello".to_string(),
        };
        let cloned = item.clone();
        assert_eq!(cloned.time_ms, 1000);
        assert_eq!(cloned.text, "hello");
    }

    #[test]
    fn test_config_default() {
        let cfg = DanmakuConfig::default();
        assert_eq!(cfg.viewport_width, 1920.0);
        assert_eq!(cfg.scroll_duration, 8.0);
        assert!(cfg.enabled);
        assert_eq!(cfg.display_area, 1.0);
        assert_eq!(cfg.track_gap_ratio, 0.5);
        assert!(cfg.max_lines.is_none());
        assert!(cfg.blocked_types.is_empty());
        assert!(cfg.block_words.is_empty());
        assert!(!cfg.merge_duplicates);
        assert!(cfg.allow_scroll_overwrite);
        assert_eq!(cfg.outline_multiplier, 1.0);
    }
}
