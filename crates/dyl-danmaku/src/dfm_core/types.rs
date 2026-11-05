/// Danmaku type-specific position computation.
/// Ported from R2LDanmaku, L2RDanmaku, FTDanmaku, FBDanmaku.
use crate::dfm_core::model::{DanmakuItem, DanmakuType, GlobalFlags};

/// Compute the X position for a danmaku at a given time.
pub fn get_x_at_time(
    item: &DanmakuItem,
    view_width: f32,
    time_ms: i64,
    flags: &GlobalFlags,
) -> f32 {
    match item.danmaku_type {
        DanmakuType::ScrollRL => get_r2l_x(item, view_width, time_ms, flags),
        DanmakuType::ScrollLR => get_l2r_x(item, view_width, time_ms, flags),
        DanmakuType::FixTop | DanmakuType::FixBottom => get_fixed_x(item, view_width),
    }
}

/// R2L: x = view_width - elapsed * step_x
/// Ported from R2LDanmaku.getAccurateLeft().
pub fn get_r2l_x(item: &DanmakuItem, view_width: f32, time_ms: i64, flags: &GlobalFlags) -> f32 {
    let actual_time = item.get_actual_time(flags);
    let elapsed = time_ms - actual_time;
    if elapsed >= item.duration_ms {
        -item.paint_width
    } else {
        view_width - elapsed as f32 * item.step_x
    }
}

/// L2R: x = step_x * elapsed - paint_width
/// Ported from L2RDanmaku.getAccurateLeft().
pub fn get_l2r_x(item: &DanmakuItem, view_width: f32, time_ms: i64, flags: &GlobalFlags) -> f32 {
    let actual_time = item.get_actual_time(flags);
    let elapsed = time_ms - actual_time;
    if elapsed >= item.duration_ms {
        view_width
    } else {
        elapsed as f32 * item.step_x - item.paint_width
    }
}

/// FT/FB: centered horizontally.
/// Ported from FTDanmaku.getLeft().
pub fn get_fixed_x(item: &DanmakuItem, view_width: f32) -> f32 {
    (view_width - item.paint_width) / 2.0
}

/// Layout a danmaku item: compute position and set visibility.
/// Ported from R2LDanmaku.layout().
pub fn layout_item(
    item: &mut DanmakuItem,
    view_width: f32,
    _x: f32,
    y: f32,
    timer_ms: i64,
    flags: &GlobalFlags,
) {
    let actual_time = item.get_actual_time(flags);
    let delta = timer_ms - actual_time;

    if delta > 0 && delta < item.duration_ms {
        item.x = get_x_at_time(item, view_width, timer_ms, flags);
        if !item.is_shown_state(flags) {
            item.y = y;
            item.is_shown = true;
            item.flags.visible = flags.visible_flag;
        }
    } else {
        item.is_shown = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dfm_core::model::DanmakuItem;

    #[test]
    fn test_r2l_x_at_start() {
        let flags = GlobalFlags::default();
        let mut item = DanmakuItem::new(
            0,
            "test".into(),
            0xFFFFFFFF,
            25.0,
            DanmakuType::ScrollRL,
            5000,
        );
        item.measure(1920.0, 1080.0, &flags);
        let x = get_r2l_x(&item, 1920.0, 0, &flags);
        assert!((x - 1920.0).abs() < 1.0);
    }

    #[test]
    fn test_r2l_x_at_end() {
        let flags = GlobalFlags::default();
        let mut item = DanmakuItem::new(
            0,
            "test".into(),
            0xFFFFFFFF,
            25.0,
            DanmakuType::ScrollRL,
            5000,
        );
        item.measure(1920.0, 1080.0, &flags);
        let x = get_r2l_x(&item, 1920.0, 5000, &flags);
        assert!(x <= 0.0);
    }

    #[test]
    fn test_l2r_x_at_start() {
        let flags = GlobalFlags::default();
        let mut item = DanmakuItem::new(
            0,
            "test".into(),
            0xFFFFFFFF,
            25.0,
            DanmakuType::ScrollLR,
            5000,
        );
        item.measure(1920.0, 1080.0, &flags);
        let x = get_l2r_x(&item, 1920.0, 0, &flags);
        assert!(x <= 0.0); // starts offscreen left
    }

    #[test]
    fn test_fixed_centered() {
        let item = DanmakuItem {
            paint_width: 100.0,
            ..DanmakuItem::new(
                0,
                "test".into(),
                0xFFFFFFFF,
                25.0,
                DanmakuType::FixTop,
                3800,
            )
        };
        let x = get_fixed_x(&item, 1920.0);
        assert!((x - 910.0).abs() < 1.0); // (1920 - 100) / 2
    }
}
