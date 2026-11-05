//! Overlay 挂钟插值时钟
//!
//! 视频帧通常只有 24–30fps，而显示器刷新率可达 60/120/144Hz。若叠加层（字幕、
//! 弹幕）只在收到新视频帧时拿到新的 PTS，其动画就永远被量化到视频帧率。
//! `OverlayClock` 以"最新上屏帧的 PTS + 该时刻的挂钟"为锚点，渲染线程每个节拍
//! 按挂钟外推，使叠加层以显示器刷新率连续推进；暂停或帧流停滞时冻结外推。

use parking_lot::Mutex;
use std::time::Instant;

/// 外推下限（秒）：容纳投递抖动，避免叠加层在两个视频帧之间出现"停顿—跳变"
const MIN_EXTRAPOLATE_SECS: f64 = 0.1;
/// 外推上限（秒）：解码卡顿时不允许叠加层无限滑出画面
const MAX_EXTRAPOLATE_SECS: f64 = 0.5;
/// 首个锚点之前假定的视频帧间隔（秒）
const DEFAULT_FRAME_INTERVAL_SECS: f64 = 1.0 / 30.0;

struct Anchor {
    /// 锚定时刻的媒体时间（秒）
    pts_secs: f64,
    /// 锚定时刻的挂钟
    wall: Instant,
    /// 相邻视频帧的 PTS 间隔（秒），决定外推上限
    frame_interval_secs: f64,
    /// 是否允许按挂钟外推（暂停/冻结时为 false）
    live: bool,
}

/// 允许的最大外推时长：随视频帧间隔自适应，并夹在安全区间内
fn extrapolation_limit(frame_interval_secs: f64) -> f64 {
    (frame_interval_secs * 3.0).clamp(MIN_EXTRAPOLATE_SECS, MAX_EXTRAPOLATE_SECS)
}

/// 叠加层挂钟时钟 — 可跨线程共享（pipeline 线程锚定/冻结，渲染线程读取）
#[derive(Default)]
pub struct OverlayClock {
    anchor: Mutex<Option<Anchor>>,
}

impl OverlayClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用刚上屏视频帧的 PTS 重新锚定，并恢复外推
    ///
    /// PTS 回跳（seek）或跳变过大时沿用上一次的帧间隔，不把 seek 当成 0 帧时长。
    pub fn reanchor(&self, pts_secs: f64) {
        let mut anchor = self.anchor.lock();
        let frame_interval_secs = match anchor.as_ref() {
            Some(prev) => {
                let delta = pts_secs - prev.pts_secs;
                if delta > 0.0 && delta < 1.0 {
                    delta
                } else {
                    prev.frame_interval_secs
                }
            }
            None => DEFAULT_FRAME_INTERVAL_SECS,
        };
        *anchor = Some(Anchor {
            pts_secs,
            wall: Instant::now(),
            frame_interval_secs,
            live: true,
        });
    }

    /// 冻结外推（暂停/EOF/停止）：把已外推的时间固化进 PTS，画面静止时叠加层也静止
    pub fn freeze(&self) {
        let mut anchor = self.anchor.lock();
        let Some(a) = anchor.as_mut() else {
            return;
        };
        if !a.live {
            return;
        }
        let limit = extrapolation_limit(a.frame_interval_secs);
        a.pts_secs += a.wall.elapsed().as_secs_f64().min(limit);
        a.wall = Instant::now();
        a.live = false;
    }

    /// 当前叠加层应使用的媒体时间（秒）
    pub fn now_pts(&self) -> f64 {
        let anchor = self.anchor.lock();
        let Some(a) = anchor.as_ref() else {
            return 0.0;
        };
        if !a.live {
            return a.pts_secs;
        }
        let limit = extrapolation_limit(a.frame_interval_secs);
        a.pts_secs + a.wall.elapsed().as_secs_f64().min(limit)
    }

    /// 时钟是否仍在推进（决定叠加层是否需要重绘）
    pub fn is_live(&self) -> bool {
        self.anchor.lock().as_ref().is_some_and(|a| a.live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unanchored_clock_is_zero_and_idle() {
        let clock = OverlayClock::new();
        assert_eq!(clock.now_pts(), 0.0);
        assert!(!clock.is_live());
    }

    #[test]
    fn reanchor_extrapolates_with_wall_clock() {
        let clock = OverlayClock::new();
        clock.reanchor(10.0);
        assert!(clock.is_live());
        std::thread::sleep(Duration::from_millis(20));
        let advanced = clock.now_pts();
        assert!(
            advanced > 10.01 && advanced < 10.2,
            "expected wall-clock advance, got {advanced}"
        );
    }

    #[test]
    fn freeze_stops_extrapolation_and_is_idempotent() {
        let clock = OverlayClock::new();
        clock.reanchor(5.0);
        std::thread::sleep(Duration::from_millis(20));
        clock.freeze();
        assert!(!clock.is_live());
        let frozen = clock.now_pts();
        assert!(frozen > 5.0 && frozen < 5.2, "frozen={frozen}");
        std::thread::sleep(Duration::from_millis(20));
        clock.freeze();
        assert_eq!(clock.now_pts(), frozen);
    }

    #[test]
    fn extrapolation_is_clamped() {
        let clock = OverlayClock::new();
        clock.reanchor(0.0);
        clock.reanchor(0.04); // 25fps 片源，外推上限随帧间隔自适应，不随挂钟无限增长
        std::thread::sleep(Duration::from_millis(250));
        let limit = extrapolation_limit(0.04);
        assert!(limit < 0.25, "上限应由帧间隔决定，实际 {limit}");
        assert!(clock.now_pts() <= 0.04 + limit + 0.001, "外推不应超过上限");
    }

    #[test]
    fn seek_backward_keeps_previous_frame_interval() {
        let clock = OverlayClock::new();
        clock.reanchor(0.0);
        clock.reanchor(0.04);
        clock.reanchor(60.0); // seek 前跳：间隔不可信，沿用 40ms
        let interval = clock.anchor.lock().as_ref().unwrap().frame_interval_secs;
        assert!((interval - 0.04).abs() < 1e-9);
        assert_eq!(clock.anchor.lock().as_ref().unwrap().pts_secs, 60.0);
    }

    #[test]
    fn limit_scales_with_frame_interval() {
        assert_eq!(extrapolation_limit(1.0 / 240.0), MIN_EXTRAPOLATE_SECS);
        assert!((extrapolation_limit(0.05) - 0.15).abs() < 1e-9);
        assert_eq!(extrapolation_limit(5.0), MAX_EXTRAPOLATE_SECS);
    }
}
