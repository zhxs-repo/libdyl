//! 字幕/弹幕引擎归 engine 所有：pipeline 只拿字幕引擎做 feed，
//! 输出层通过 `SharedOverlay` 共享同一实例。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dyl_danmaku::{DanmakuConfig, DanmakuEngine};
use dyl_render::{Overlay, SharedOverlay};
use dyl_subtitle::{SubtitleConfig, SubtitleEngine};
use parking_lot::Mutex;

use crate::error::EngineError;

/// shell 侧渲染叠加层用的共享引擎句柄。
///
/// 与 engine 是同一实例：加载/开关变化立即对渲染可见，无需换栈。
#[derive(Clone)]
pub struct OverlayHandles {
    pub subtitle: Arc<Mutex<SubtitleEngine>>,
    pub danmaku: Arc<Mutex<DanmakuEngine>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayKind {
    Subtitle,
    Danmaku,
}

pub(crate) struct OverlayEngines {
    pub subtitle: Arc<Mutex<SubtitleEngine>>,
    pub danmaku: Arc<Mutex<DanmakuEngine>>,
}

impl OverlayEngines {
    pub(crate) fn new() -> Self {
        Self {
            subtitle: Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default()))),
            danmaku: Arc::new(Mutex::new(DanmakuEngine::new(DanmakuConfig::default()))),
        }
    }

    /// 首次加载系统字体；返回 `true` 表示系统里找不到可用字体。
    pub(crate) fn ensure_font(&self, loaded: &AtomicBool) -> bool {
        if loaded.load(Ordering::Acquire) {
            return false;
        }
        match dyl_subtitle::load_system_font() {
            Some(font) => {
                self.subtitle.lock().set_font(font.clone());
                self.danmaku.lock().set_font(font);
                loaded.store(true, Ordering::Release);
                // 样式表里的 `Fontname` 要查系统字体索引，扫目录是几十到几百毫秒的
                // 活。这里是加载线程，必须在这儿预热，不能让渲染线程第一次命中时卡一帧。
                self.subtitle.lock().prepare_system_fonts();
                false
            }
            None => true,
        }
    }

    /// 换文件：字幕与弹幕都回到未加载状态（可见性设置保留）。
    pub(crate) fn reset_for_new_media(&self) {
        {
            let state = self.subtitle.lock().shared_state();
            let mut state = state.lock();
            state.events.clear();
            state.ass_events.clear();
            state.styles.clear();
            state.play_res = (0.0, 0.0);
            state.current_text.clear();
            state.external_source = false;
        }
        // 走 load_items 而不是直接改 state：revision 递增让字形放置缓存一起失效
        self.danmaku.lock().load_items(Vec::new());
    }

    /// 同一文件重开：外挂字幕保留，内嵌字幕事件清空后由新管线重新喂入。
    pub(crate) fn reset_embedded_subtitle(&self) {
        let state = self.subtitle.lock().shared_state();
        let mut state = state.lock();
        if state.external_source {
            return;
        }
        state.events.clear();
        state.ass_events.clear();
        state.styles.clear();
        state.play_res = (0.0, 0.0);
        state.current_text.clear();
    }

    pub(crate) fn overlays(&self) -> Vec<Box<dyn Overlay>> {
        vec![
            Box::new(SharedOverlay::new(self.subtitle.clone())),
            Box::new(SharedOverlay::new(self.danmaku.clone())),
        ]
    }

    pub(crate) fn handles(&self) -> OverlayHandles {
        OverlayHandles {
            subtitle: self.subtitle.clone(),
            danmaku: self.danmaku.clone(),
        }
    }

    pub(crate) fn set_subtitle_visible(&self, visible: bool) {
        self.subtitle.lock().shared_state().lock().enabled = visible;
    }

    pub(crate) fn set_danmaku_visible(&self, visible: bool) {
        self.danmaku.lock().shared_state().lock().enabled = visible;
    }

    pub(crate) fn set_danmaku_opacity(&self, opacity: u8) {
        self.danmaku.lock().shared_state().lock().opacity = opacity as f32 / 100.0;
    }

    pub(crate) fn load_subtitle(&self, path: &str) -> Result<usize, EngineError> {
        self.subtitle
            .lock()
            .load_from_file(path)
            .map_err(EngineError::from)?;
        Ok(self.subtitle.lock().shared_state().lock().events.len())
    }

    pub(crate) fn load_danmaku(&self, path: &str) -> Result<usize, EngineError> {
        self.danmaku
            .lock()
            .load_from_file(path)
            .map_err(EngineError::from)?;
        Ok(self.danmaku.lock().shared_state().lock().items.len())
    }
}
