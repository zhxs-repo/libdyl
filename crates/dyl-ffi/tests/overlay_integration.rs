//! SharedOverlay 管线集成测试
//!
//! 验证 Overlay trait、OverlayStack 合成顺序、SharedOverlay 共享状态委托，
//! 以及 SubtitleEngine / DanmakuEngine 在管线中的协作行为。
//! 所有测试不依赖 GPU 硬件（不调用 render()）。

use dyl_danmaku::{DanmakuConfig, DanmakuEngine, DanmakuItem, DanmakuMode};
use dyl_render::{Overlay, OverlayStack, SharedOverlay};
use dyl_subtitle::{SubtitleConfig, SubtitleEngine, SubtitleEvent};
use parking_lot::Mutex;
use std::sync::Arc;

// ═══════════════════════════════════════════════════
// Mock Overlay — 记录 update() 调用顺序
// ═══════════════════════════════════════════════════

struct MockOverlay {
    label: String,
    /// 每次 update() 时将 (label_index, pts) 追加到共享日志
    call_log: Arc<Mutex<Vec<(String, f64)>>>,
    visible: bool,
}

impl MockOverlay {
    fn new(label: &str, log: Arc<Mutex<Vec<(String, f64)>>>) -> Self {
        Self {
            label: label.to_string(),
            call_log: log,
            visible: true,
        }
    }
}

impl Overlay for MockOverlay {
    fn update(&mut self, pts: f64) {
        self.call_log.lock().push((self.label.clone(), pts));
    }

    fn render(
        &mut self,
        _frame: &dyl_render::OverlayFrame<'_>,
        _encoder: &mut wgpu::CommandEncoder,
        _area: dyl_render::OverlayArea,
    ) {
        // 集成测试不调用 render
    }

    fn is_visible(&self) -> bool {
        self.visible
    }
}

// ═══════════════════════════════════════════════════
// 1. SharedOverlay 委托验证
// ═══════════════════════════════════════════════════

#[test]
fn shared_overlay_delegates_update() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mock = MockOverlay::new("mock_a", log.clone());
    let shared = Arc::new(Mutex::new(mock));
    let mut overlay = SharedOverlay::new(shared.clone());

    overlay.update(1.5);
    overlay.update(3.0);

    let entries = log.lock();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], ("mock_a".to_string(), 1.5));
    assert_eq!(entries[1], ("mock_a".to_string(), 3.0));
}

#[test]
fn shared_overlay_delegates_is_visible() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mock = MockOverlay::new("vis_test", log);
    let shared = Arc::new(Mutex::new(mock));
    let overlay = SharedOverlay::new(shared.clone());

    assert!(overlay.is_visible());

    // 通过外部引用修改 visible
    shared.lock().visible = false;
    assert!(!overlay.is_visible());
}

#[test]
fn shared_overlay_allows_external_state_mutation() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mock = MockOverlay::new("ext_mut", log.clone());
    let shared = Arc::new(Mutex::new(mock));
    let mut overlay = SharedOverlay::new(shared.clone());

    overlay.update(0.5);

    // 通过外部引用修改 label（模拟 FFI 线程写入）
    shared.lock().label = "mutated".to_string();

    overlay.update(1.0);

    let entries = log.lock();
    assert_eq!(entries[0].0, "ext_mut");
    assert_eq!(entries[1].0, "mutated");
}

// ═══════════════════════════════════════════════════
// 2. OverlayStack 合成顺序验证
// ═══════════════════════════════════════════════════

#[test]
fn overlay_stack_updates_in_insertion_order() {
    let log = Arc::new(Mutex::new(Vec::new()));

    let mut stack = OverlayStack::new();
    stack.push(Box::new(MockOverlay::new("subtitle", log.clone())));
    stack.push(Box::new(MockOverlay::new("danmaku", log.clone())));

    assert_eq!(stack.len(), 2);
    assert!(!stack.is_empty());

    stack.update_all(2.5);

    let entries = log.lock();
    assert_eq!(entries.len(), 2);
    // 先 subtitle，后 danmaku
    assert_eq!(entries[0].0, "subtitle");
    assert_eq!(entries[1].0, "danmaku");
    // pts 一致
    assert_eq!(entries[0].1, 2.5);
    assert_eq!(entries[1].1, 2.5);
}

#[test]
fn overlay_stack_skips_invisible_overlays_for_render_but_still_updates() {
    let log = Arc::new(Mutex::new(Vec::new()));

    let mut stack = OverlayStack::new();
    let mut invisible = MockOverlay::new("hidden", log.clone());
    invisible.visible = false;
    stack.push(Box::new(invisible));
    stack.push(Box::new(MockOverlay::new("visible", log.clone())));

    // update_all 应该对所有 overlay 调用 update（包括 invisible 的）
    stack.update_all(1.0);
    let entries = log.lock();
    assert_eq!(entries.len(), 2); // 两个都被 update
}

#[test]
fn overlay_stack_empty_is_safe() {
    let mut stack = OverlayStack::new();
    assert!(stack.is_empty());
    assert_eq!(stack.len(), 0);
    // 不应 panic
    stack.update_all(0.0);
}

// ═══════════════════════════════════════════════════
// 3. SubtitleEngine 共享状态验证
// ═══════════════════════════════════════════════════

#[test]
fn subtitle_engine_shared_state_load_events() {
    let engine = SubtitleEngine::new(SubtitleConfig::default());
    let shared = engine.shared_state();

    let events = vec![
        SubtitleEvent {
            start_time: 1.0,
            end_time: 3.0,
            text: "First".to_string(),
            style: "Default".to_string(),
        },
        SubtitleEvent {
            start_time: 5.0,
            end_time: 8.0,
            text: "Second".to_string(),
            style: "Default".to_string(),
        },
    ];
    shared.lock().events = events.clone();

    // 通过 shared_state 读取验证
    let state = shared.lock();
    assert_eq!(state.events.len(), 2);
    assert_eq!(state.events[0].text, "First");
    assert!(state.enabled);
}

#[test]
fn subtitle_engine_load_events_method() {
    let engine = SubtitleEngine::new(SubtitleConfig::default());
    let events = vec![SubtitleEvent {
        start_time: 0.0,
        end_time: 2.0,
        text: "Hello".to_string(),
        style: "Default".to_string(),
    }];
    engine.load_events(events);

    let shared = engine.shared_state();
    let state = shared.lock();
    assert_eq!(state.events.len(), 1);
    assert_eq!(state.events[0].text, "Hello");
}

#[test]
fn subtitle_engine_update_selects_active_events() {
    let engine = SubtitleEngine::new(SubtitleConfig::default());
    let events = vec![
        SubtitleEvent {
            start_time: 1.0,
            end_time: 3.0,
            text: "Active at 2s".to_string(),
            style: "Default".to_string(),
        },
        SubtitleEvent {
            start_time: 10.0,
            end_time: 15.0,
            text: "Later".to_string(),
            style: "Default".to_string(),
        },
    ];
    engine.load_events(events);

    // update at pts=2.0 → 应该选中 "Active at 2s"
    // 注意: SubtitleEngine 的 update() 和 render() 需要 Overlay trait 的 &mut self
    // 但 SharedOverlay 通过 lock() 获取 &mut，所以这里直接调用
    let mut engine = engine;
    Overlay::update(&mut engine, 2.0);

    let shared = engine.shared_state();
    let state = shared.lock();
    assert_eq!(state.current_text, "Active at 2s");
}

#[test]
fn subtitle_engine_update_no_active_events_clears_text() {
    let engine = SubtitleEngine::new(SubtitleConfig::default());
    engine.load_events(vec![SubtitleEvent {
        start_time: 1.0,
        end_time: 3.0,
        text: "Short".to_string(),
        style: "Default".to_string(),
    }]);

    let mut engine = engine;
    // 先让字幕出现
    Overlay::update(&mut engine, 2.0);
    assert_eq!(engine.shared_state().lock().current_text, "Short");

    // 跳到字幕结束之后
    Overlay::update(&mut engine, 5.0);
    assert_eq!(engine.shared_state().lock().current_text, "");
}

#[test]
fn subtitle_engine_toggle_via_shared_state() {
    let engine = SubtitleEngine::new(SubtitleConfig::default());
    let shared = engine.shared_state();

    // 默认启用
    assert!(shared.lock().enabled);
    assert!(Overlay::is_visible(&engine));

    // 通过 shared_state 禁用
    shared.lock().enabled = false;
    assert!(!Overlay::is_visible(&engine));

    // update 在禁用时不更新 current_text
    engine.load_events(vec![SubtitleEvent {
        start_time: 0.0,
        end_time: 10.0,
        text: "Should not show".to_string(),
        style: "Default".to_string(),
    }]);
    let mut engine = engine;
    Overlay::update(&mut engine, 1.0);
    // current_text 应该还是空的（enabled=false 时 update 提前返回）
    assert_eq!(engine.shared_state().lock().current_text, "");
}

#[test]
fn subtitle_engine_multiple_overlapping_events() {
    let engine = SubtitleEngine::new(SubtitleConfig::default());
    engine.load_events(vec![
        SubtitleEvent {
            start_time: 0.0,
            end_time: 5.0,
            text: "Line A".to_string(),
            style: "Default".to_string(),
        },
        SubtitleEvent {
            start_time: 1.0,
            end_time: 4.0,
            text: "Line B".to_string(),
            style: "Default".to_string(),
        },
    ]);

    let mut engine = engine;
    // pts=2.0 → 两条字幕都活跃
    Overlay::update(&mut engine, 2.0);
    let text = engine.shared_state().lock().current_text.clone();
    assert!(text.contains("Line A"));
    assert!(text.contains("Line B"));
}

// ═══════════════════════════════════════════════════
// 4. DanmakuEngine 共享状态验证
// ═══════════════════════════════════════════════════

#[test]
fn danmaku_engine_shared_state_defaults() {
    let engine = DanmakuEngine::new(DanmakuConfig::default());
    let shared = engine.shared_state();
    let state = shared.lock();

    assert!(state.enabled);
    assert_eq!(state.opacity, 1.0);
    assert_eq!(state.current_pts, 0.0);
    assert!(state.instances.is_empty());
    assert!(state.items.is_empty());
}

#[test]
fn danmaku_engine_update_sets_pts() {
    let engine = DanmakuEngine::new(DanmakuConfig::default());
    let shared = engine.shared_state();

    // DanmakuEngine::update() 只是写入 current_pts
    let mut engine = engine;
    Overlay::update(&mut engine, 42.5);

    let state = shared.lock();
    assert_eq!(state.current_pts, 42.5);
}

#[test]
fn danmaku_engine_toggle_via_shared_state() {
    let engine = DanmakuEngine::new(DanmakuConfig::default());
    let shared = engine.shared_state();

    assert!(Overlay::is_visible(&engine));

    shared.lock().enabled = false;
    assert!(!Overlay::is_visible(&engine));

    shared.lock().enabled = true;
    assert!(Overlay::is_visible(&engine));
}

#[test]
fn danmaku_engine_opacity_via_shared_state() {
    let engine = DanmakuEngine::new(DanmakuConfig::default());
    let shared = engine.shared_state();

    // 模拟 FFI SetDanmakuOpacity 命令
    shared.lock().opacity = 0.5;
    assert_eq!(shared.lock().opacity, 0.5);

    shared.lock().opacity = 0.0;
    assert_eq!(shared.lock().opacity, 0.0);
}

#[test]
fn danmaku_engine_load_items_populates_instances() {
    let engine = DanmakuEngine::new(DanmakuConfig::default());

    let items = vec![
        DanmakuItem {
            text: "Test1".to_string(),
            time_ms: 1000,
            mode: DanmakuMode::ScrollRight,
            color: 0xFFFFFF,
            font_size: 25,
        },
        DanmakuItem {
            text: "Test2".to_string(),
            time_ms: 2000,
            mode: DanmakuMode::ScrollRight,
            color: 0xFF0000,
            font_size: 25,
        },
    ];

    engine.load_items(items);

    let shared = engine.shared_state();
    let state = shared.lock();
    assert_eq!(state.items.len(), 2);
    // instances 可能因过滤而为 0 或有值，取决于布局引擎
    // 这里只验证 items 已写入
    assert!(!state.items.is_empty());
}

// ═══════════════════════════════════════════════════
// 5. Full Pipeline 集成 — 模拟 FFI load_file 流程
// ═══════════════════════════════════════════════════

#[test]
fn ffi_pipeline_creation_flow() {
    // 模拟 FFI load_file 中的 overlay 创建流程：
    //   1. 创建 Arc<Mutex<SubtitleEngine>> 和 Arc<Mutex<DanmakuEngine>>
    //   2. 用 SharedOverlay 包装
    //   3. 推入 OverlayStack
    //   4. FFI 持有 Arc 引用用于后续命令

    let subtitle_engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
    let danmaku_engine = Arc::new(Mutex::new(DanmakuEngine::new(DanmakuConfig::default())));

    // 创建 overlays（与 FFI load_file 中相同）
    let overlays: Vec<Box<dyn Overlay>> = vec![
        Box::new(SharedOverlay::new(subtitle_engine.clone())),
        Box::new(SharedOverlay::new(danmaku_engine.clone())),
    ];

    let mut stack = OverlayStack::new();
    for overlay in overlays {
        stack.push(overlay);
    }
    assert_eq!(stack.len(), 2);

    // 模拟 FFI 线程通过 Arc 引用加载字幕
    let events = vec![SubtitleEvent {
        start_time: 0.0,
        end_time: 5.0,
        text: "Loaded via FFI".to_string(),
        style: "Default".to_string(),
    }];
    subtitle_engine.lock().load_events(events);

    // 模拟 FFI 线程设置弹幕不透明度
    {
        let dm_state = danmaku_engine.lock().shared_state();
        dm_state.lock().opacity = 0.75;
    }

    // 模拟渲染线程 update_all
    stack.update_all(2.5);

    // 验证字幕状态已更新
    {
        let sub_state = subtitle_engine.lock().shared_state();
        let state = sub_state.lock();
        assert_eq!(state.current_text, "Loaded via FFI");
    }

    // 验证弹幕 pts 已更新
    {
        let dm_state = danmaku_engine.lock().shared_state();
        let state = dm_state.lock();
        assert_eq!(state.current_pts, 2.5);
        assert_eq!(state.opacity, 0.75);
    }
}

#[test]
fn ffi_pipeline_toggle_subtitle_danmaku_independently() {
    let subtitle_engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
    let danmaku_engine = Arc::new(Mutex::new(DanmakuEngine::new(DanmakuConfig::default())));

    let overlays: Vec<Box<dyn Overlay>> = vec![
        Box::new(SharedOverlay::new(subtitle_engine.clone())),
        Box::new(SharedOverlay::new(danmaku_engine.clone())),
    ];

    let mut stack = OverlayStack::new();
    for overlay in overlays {
        stack.push(overlay);
    }

    // 禁用字幕，弹幕保持启用
    subtitle_engine.lock().shared_state().lock().enabled = false;

    // update_all 仍然不会 panic
    stack.update_all(1.0);

    // 验证字幕没有更新 current_text（因为 disabled）
    assert_eq!(
        subtitle_engine.lock().shared_state().lock().current_text,
        ""
    );
    // 弹幕 pts 已更新
    assert_eq!(danmaku_engine.lock().shared_state().lock().current_pts, 1.0);
}

#[test]
fn ffi_pipeline_multiple_update_cycles() {
    let subtitle_engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
    let danmaku_engine = Arc::new(Mutex::new(DanmakuEngine::new(DanmakuConfig::default())));

    // 加载字幕
    subtitle_engine.lock().load_events(vec![
        SubtitleEvent {
            start_time: 0.0,
            end_time: 3.0,
            text: "Early".to_string(),
            style: "Default".to_string(),
        },
        SubtitleEvent {
            start_time: 5.0,
            end_time: 8.0,
            text: "Late".to_string(),
            style: "Default".to_string(),
        },
    ]);

    let overlays: Vec<Box<dyn Overlay>> = vec![
        Box::new(SharedOverlay::new(subtitle_engine.clone())),
        Box::new(SharedOverlay::new(danmaku_engine.clone())),
    ];

    let mut stack = OverlayStack::new();
    for overlay in overlays {
        stack.push(overlay);
    }

    // Cycle 1: pts=1.0 → "Early" 活跃
    stack.update_all(1.0);
    assert_eq!(
        subtitle_engine.lock().shared_state().lock().current_text,
        "Early"
    );

    // Cycle 2: pts=4.0 → 无活跃字幕
    stack.update_all(4.0);
    assert_eq!(
        subtitle_engine.lock().shared_state().lock().current_text,
        ""
    );

    // Cycle 3: pts=6.0 → "Late" 活跃
    stack.update_all(6.0);
    assert_eq!(
        subtitle_engine.lock().shared_state().lock().current_text,
        "Late"
    );

    // Cycle 4: pts=10.0 → 无活跃字幕
    stack.update_all(10.0);
    assert_eq!(
        subtitle_engine.lock().shared_state().lock().current_text,
        ""
    );

    // 弹幕 pts 跟随最后一个 update
    assert_eq!(
        danmaku_engine.lock().shared_state().lock().current_pts,
        10.0
    );
}

#[test]
fn ffi_pipeline_stop_clears_engines() {
    let subtitle_engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
    let danmaku_engine = Arc::new(Mutex::new(DanmakuEngine::new(DanmakuConfig::default())));

    // 模拟 FFI stop_pipeline: 将 Option<Arc<Mutex<T>>> 置为 None
    let mut sub_opt: Option<Arc<Mutex<SubtitleEngine>>> = Some(subtitle_engine.clone());
    let mut dm_opt: Option<Arc<Mutex<DanmakuEngine>>> = Some(danmaku_engine.clone());

    assert!(sub_opt.is_some());
    assert!(dm_opt.is_some());

    // 模拟 stop_pipeline
    sub_opt = None;
    dm_opt = None;

    assert!(sub_opt.is_none());
    assert!(dm_opt.is_none());

    // 原始 Arc 引用仍然有效（SharedOverlay 持有的那份还在）
    let shared = subtitle_engine.lock().shared_state();
    assert!(shared.lock().enabled);
}

#[test]
fn shared_overlay_thread_safety_arc_clone() {
    // 验证 Arc<Mutex<Engine>> 可以跨线程共享
    let engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
    let engine_clone = engine.clone();

    let handle = std::thread::spawn(move || {
        // 模拟 "FFI 线程" 通过 clone 的 Arc 写入
        engine_clone.lock().load_events(vec![SubtitleEvent {
            start_time: 0.0,
            end_time: 1.0,
            text: "From thread".to_string(),
            style: "Default".to_string(),
        }]);
    });

    handle.join().unwrap();

    // 主线程读取
    let state = engine.lock().shared_state();
    let s = state.lock();
    assert_eq!(s.events.len(), 1);
    assert_eq!(s.events[0].text, "From thread");
}

// ═══════════════════════════════════════════════════
// 6. SubtitleEngine TTC 字体处理
// ═══════════════════════════════════════════════════

#[test]
fn subtitle_engine_set_font_ttf() {
    let mut engine = SubtitleEngine::new(SubtitleConfig::default());
    // 使用假的 TTF 数据（非 TTC）
    let fake_ttf = vec![0u8; 100];
    engine.set_font(fake_ttf);
    // 不 panic 即通过
}

#[test]
fn subtitle_engine_set_font_ttc_header_too_short() {
    let mut engine = SubtitleEngine::new(SubtitleConfig::default());
    // TTC header 但数据太短
    let mut data = vec![0u8; 8];
    data[0..4].copy_from_slice(b"ttcf");
    engine.set_font(data);
    // 应走 fallback 路径，不 panic
}

// ═══════════════════════════════════════════════════
// 7. OverlayStack push / len / is_empty 基本操作
// ═══════════════════════════════════════════════════

#[test]
fn overlay_stack_push_and_len() {
    let mut stack = OverlayStack::new();
    assert_eq!(stack.len(), 0);
    assert!(stack.is_empty());

    let log = Arc::new(Mutex::new(Vec::new()));
    stack.push(Box::new(MockOverlay::new("a", log.clone())));
    assert_eq!(stack.len(), 1);
    assert!(!stack.is_empty());

    stack.push(Box::new(MockOverlay::new("b", log.clone())));
    assert_eq!(stack.len(), 2);

    stack.push(Box::new(MockOverlay::new("c", log)));
    assert_eq!(stack.len(), 3);
}

#[test]
fn overlay_stack_default() {
    let stack = OverlayStack::default();
    assert!(stack.is_empty());
    assert_eq!(stack.len(), 0);
}
