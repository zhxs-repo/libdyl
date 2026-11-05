//! C ABI 边界测试 — 直接调用 `#[no_mangle] extern "C"` 导出函数
//!
//! 覆盖句柄所有权、null/畸形参数、同步校验、异步 `command_completed`、
//! 快照/属性、Surface 入口以及事件回调与 poll 的互斥。全部不依赖 GPU 或
//! 媒体文件（load_file 只走失败路径）。

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dyl::{
    dyl_command, dyl_create, dyl_destroy, dyl_free_string, dyl_get_property, dyl_get_snapshot,
    dyl_last_error, dyl_poll_event, dyl_set_event_callback, dyl_surface_attach, dyl_surface_detach,
    dyl_surface_redraw, dyl_surface_resize,
};
use dyl_core::contract::{Snapshot, SurfaceKind};
use parking_lot::Mutex;
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(5);

// ═══════════════════════════════════════════════════
// 测试辅助
// ═══════════════════════════════════════════════════

struct TestHandle(*mut dyl::DylHandle);

impl TestHandle {
    fn new() -> Self {
        let handle = dyl_create();
        assert!(!handle.is_null(), "dyl_create returned null");
        Self(handle)
    }

    fn command(&self, json: &str) -> i64 {
        let text = c(json);
        dyl_command(self.0, text.as_ptr())
    }

    fn snapshot(&self) -> Snapshot {
        let text = take_string(dyl_get_snapshot(self.0)).expect("snapshot");
        serde_json::from_str(&text).expect("snapshot is valid contract JSON")
    }

    fn poll(&self) -> Option<Value> {
        take_string(dyl_poll_event(self.0)).map(|text| serde_json::from_str(&text).unwrap())
    }

    fn wait_event(&self, what: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(event) = self.poll() {
                if predicate(&event) {
                    return event;
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for TestHandle {
    fn drop(&mut self) {
        dyl_destroy(self.0);
    }
}

fn c(text: &str) -> CString {
    CString::new(text).unwrap()
}

/// 读取 FFI 返回的堆字符串并释放；null → None
fn take_string(text: *mut c_char) -> Option<String> {
    if text.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned();
    dyl_free_string(text);
    Some(value)
}

fn completion(event: &Value) -> Option<&Value> {
    (event["type"] == "command_completed").then_some(&event["data"])
}

fn wait_completed(handle: &TestHandle, request_id: i64) -> Value {
    handle.wait_event("command_completed", |event| {
        completion(event).is_some_and(|data| data["request_id"] == request_id)
    })
}

fn unsupported_surface_json() -> String {
    let supported = dyl_engine::capabilities::capabilities()
        .presentation
        .surface_kinds;
    let candidates = [
        (SurfaceKind::WindowsHwnd, "windows_hwnd"),
        (SurfaceKind::MacosNsview, "macos_nsview"),
        (SurfaceKind::X11Window, "x11_window"),
        (SurfaceKind::WaylandSurface, "wayland_surface"),
        (SurfaceKind::AndroidNativeWindow, "android_native_window"),
    ];
    let (_, kind) = candidates
        .into_iter()
        .find(|(kind, _)| !supported.contains(kind))
        .expect("every surface kind cannot be supported on one platform");
    match kind {
        "x11_window" => format!(
            r#"{{"schema":1,"kind":"{kind}","handle":"0x1","display":"0x2","width":640,"height":360,"scale_factor":1.0}}"#
        ),
        "wayland_surface" => format!(
            r#"{{"schema":1,"kind":"{kind}","surface":"0x1","display":"0x2","width":640,"height":360,"scale_factor":1.0}}"#
        ),
        _ => format!(
            r#"{{"schema":1,"kind":"{kind}","handle":"0x1","width":640,"height":360,"scale_factor":1.0}}"#
        ),
    }
}

// ═══════════════════════════════════════════════════
// 1. null 参数安全矩阵
// ═══════════════════════════════════════════════════

#[test]
fn null_handle_and_args_never_panic() {
    let handle = TestHandle::new();
    let name = c("volume");

    assert_eq!(dyl_command(std::ptr::null_mut(), c("{}").as_ptr()), -1);
    assert_eq!(dyl_command(handle.0, std::ptr::null()), -1);

    assert!(dyl_get_snapshot(std::ptr::null_mut()).is_null());
    assert!(dyl_get_property(std::ptr::null_mut(), name.as_ptr()).is_null());
    assert!(dyl_get_property(handle.0, std::ptr::null()).is_null());

    assert_eq!(
        dyl_surface_attach(std::ptr::null_mut(), std::ptr::null()),
        -1
    );
    assert_eq!(dyl_surface_detach(std::ptr::null_mut()), -1);
    assert_eq!(
        dyl_surface_resize(std::ptr::null_mut(), std::ptr::null()),
        -1
    );
    assert_eq!(dyl_surface_redraw(std::ptr::null_mut()), -1);

    assert!(dyl_poll_event(std::ptr::null_mut()).is_null());
    assert!(dyl_last_error(std::ptr::null_mut()).is_null());
    assert_eq!(
        dyl_set_event_callback(std::ptr::null_mut(), None, std::ptr::null_mut()),
        -1
    );
    assert_eq!(
        dyl_set_event_callback(handle.0, None, std::ptr::null_mut()),
        -1
    );

    dyl_free_string(std::ptr::null_mut());
    dyl_destroy(std::ptr::null_mut());
}

// ═══════════════════════════════════════════════════
// 2. 句柄与默认快照
// ═══════════════════════════════════════════════════

#[test]
fn create_returns_idle_snapshot_with_contract_defaults() {
    let handle = TestHandle::new();
    let snapshot = handle.snapshot();

    assert_eq!(snapshot.schema, 1);
    assert_eq!(snapshot.state, dyl_core::contract::PlayerState::Idle);
    assert_eq!(snapshot.volume, 100);
    assert!(!snapshot.muted);
    assert_eq!(snapshot.danmaku.opacity, 100);
    assert_eq!(
        snapshot.surface.status,
        dyl_core::contract::SurfaceStatus::None
    );
    assert!(dyl_poll_event(handle.0).is_null(), "没有事件时应返回 null");
}

#[test]
fn multiple_handles_are_independent() {
    let a = TestHandle::new();
    let b = TestHandle::new();
    assert_ne!(a.0, b.0);

    assert_eq!(
        a.command(r#"{"schema":1,"op":"set_volume","volume":30}"#),
        1
    );
    wait_completed(&a, 1);
    assert_eq!(a.snapshot().volume, 30);
    assert_eq!(b.snapshot().volume, 100);
}

// ═══════════════════════════════════════════════════
// 3. 畸形 JSON 与 last_error
// ═══════════════════════════════════════════════════

#[test]
fn malformed_json_returns_invalid_argument_and_records_error() {
    let handle = TestHandle::new();

    for payload in [
        "not json at all",
        "[1,2,3]",
        r#"{"op":"play"}"#,                  // 缺 schema
        r#"{"schema":1,"op":"bogus"}"#,      // 未知 op
        r#"{"schema":1,"op":"set_volume"}"#, // 缺参数
    ] {
        assert_eq!(handle.command(payload), -1, "payload: {payload}");
        let error = take_string(dyl_last_error(handle.0)).expect("last_error");
        assert!(
            error.contains("parse command"),
            "错误应说明解析失败: {error}"
        );
        assert!(
            take_string(dyl_last_error(handle.0)).is_none(),
            "last_error 取后应清空"
        );
    }
}

#[test]
fn invalid_utf8_pointer_returns_invalid_argument() {
    let handle = TestHandle::new();
    let bytes: [u8; 2] = [0xFF, 0x00];
    assert_eq!(dyl_command(handle.0, bytes.as_ptr() as *const c_char), -1);
}

#[test]
fn synchronous_validation_rejects_bad_values() {
    let handle = TestHandle::new();

    let cases = [
        (r#"{"schema":2,"op":"play"}"#, -1i64),
        (r#"{"schema":1,"request_id":0,"op":"play"}"#, -1),
        (r#"{"schema":1,"op":"load_file","url":"  "}"#, -1),
        (r#"{"schema":1,"op":"set_volume","volume":101}"#, -1),
        (
            r#"{"schema":1,"op":"set_danmaku_opacity","opacity":101}"#,
            -1,
        ),
        (
            r#"{"schema":1,"op":"seek","position_ms":1000,"mode":"frame"}"#,
            -1,
        ),
    ];

    for (payload, expected) in cases {
        assert_eq!(handle.command(payload), expected, "payload: {payload}");
    }
    assert!(handle.poll().is_none(), "同步拒绝不得产生任何事件");
}

#[test]
fn keyframe_seek_is_accepted_and_fails_asynchronously_in_idle() {
    let handle = TestHandle::new();

    let id = handle
        .command(r#"{"schema":1,"request_id":9,"op":"seek","position_ms":1000,"mode":"keyframe"}"#);
    assert_eq!(id, 9, "keyframe 已实现，必须通过同步校验");

    let event = wait_completed(&handle, 9);
    let data = &event["data"];
    assert_eq!(data["op"], "seek");
    assert_eq!(data["status"], "error");
    assert_eq!(data["error"]["code"], "invalid_state");
    assert_eq!(data["error"]["code_num"], -2);
}

#[test]
fn select_track_video_is_rejected_synchronously_and_other_kinds_need_media() {
    let handle = TestHandle::new();

    let rejected = handle.command(r#"{"schema":1,"op":"select_track","kind":"video","index":0}"#);
    assert_eq!(
        rejected, -9,
        "视频轨切换不在声明的能力范围内，必须在同步阶段 not_supported"
    );
    assert!(handle.poll().is_none(), "同步拒绝不得产生任何事件");

    // 音轨/字幕轨的索引属于加载后才知晓的动态状态，同步阶段只能受理
    let id = handle
        .command(r#"{"schema":1,"request_id":12,"op":"select_track","kind":"audio","index":0}"#);
    assert_eq!(id, 12);
    let event = wait_completed(&handle, 12);
    let data = completion(&event).expect("command_completed");
    assert_eq!(data["op"], "select_track");
    assert_eq!(data["status"], "error");
    assert_eq!(data["error"]["code"], "invalid_state");
    assert_eq!(data["error"]["code_num"], -2);
}

/// `protocols` 是静态能力：不在表里的 scheme 必须在同步阶段拒绝，
/// 不能落到 load worker 里被文件存在性预检误报成 file_not_found
#[test]
fn unsupported_source_scheme_is_rejected_synchronously() {
    let handle = TestHandle::new();

    let rejected =
        handle.command(r#"{"schema":1,"op":"load_file","url":"rtmp://example.com/live"}"#);
    assert_eq!(
        rejected, -9,
        "rtmp 不在 protocols 里，必须同步 not_supported"
    );
    assert!(handle.poll().is_none(), "同步拒绝不得产生任何事件");
    assert_eq!(
        handle.snapshot().state,
        dyl_core::contract::PlayerState::Idle,
        "同步拒绝不得改状态"
    );

    // 空 URL 仍是 invalid_argument 的优先级更高的一类
    let rejected = handle.command(r#"{"schema":1,"op":"load_file","url":"  "}"#);
    assert_eq!(rejected, -1);
}

// ═══════════════════════════════════════════════════
// 4. 命令受理与异步完成
// ═══════════════════════════════════════════════════
#[test]
fn accepted_command_returns_request_id_and_completes_asynchronously() {
    let handle = TestHandle::new();

    let id = handle.command(r#"{"schema":1,"request_id":7,"op":"play"}"#);
    assert_eq!(id, 7, "受理命令应返回 request_id");

    let event = wait_completed(&handle, 7);
    assert_eq!(event["schema"], 1);
    assert!(event["seq"].as_u64().unwrap() >= 1);
    let data = &event["data"];
    assert_eq!(data["op"], "play");
    assert_eq!(data["status"], "error");
    assert_eq!(data["error"]["code"], "invalid_state");
    assert_eq!(data["error"]["code_num"], -2);
    assert_eq!(
        handle.snapshot().state,
        dyl_core::contract::PlayerState::Idle
    );
}

#[test]
fn auto_request_id_is_positive_and_stable_in_event() {
    let handle = TestHandle::new();

    let id = handle.command(r#"{"schema":1,"op":"set_mute","muted":true}"#);
    assert!(id > 0, "省略 request_id 时由引擎分配正数");
    let event = wait_completed(&handle, id);
    assert_eq!(event["data"]["status"], "ok");
    assert!(handle.snapshot().muted);
}

#[test]
fn settings_changed_precedes_completion_and_snapshot_updates() {
    let handle = TestHandle::new();

    assert_eq!(
        handle.command(r#"{"schema":1,"request_id":1,"op":"set_volume","volume":55}"#),
        1
    );

    let mut types = Vec::new();
    let deadline = Instant::now() + WAIT;
    while !types.iter().any(|kind| kind == "command_completed") {
        let event = handle.wait_event("set_volume events", |_| true);
        types.push(event["type"].as_str().unwrap().to_string());
        if event["type"] == "settings_changed" {
            assert_eq!(event["data"]["volume"], 55);
            assert_eq!(event["data"]["muted"], false);
            assert_eq!(event["data"]["danmaku_opacity"], 100);
        }
        assert!(Instant::now() < deadline);
    }
    assert_eq!(
        types,
        ["settings_changed", "command_completed"],
        "设置事件必须先于命令完成"
    );
    assert_eq!(handle.snapshot().volume, 55);
}

#[test]
fn event_seq_increases_monotonically() {
    let handle = TestHandle::new();

    handle.command(r#"{"schema":1,"request_id":1,"op":"set_volume","volume":40}"#);
    handle.command(r#"{"schema":1,"request_id":2,"op":"set_mute","muted":true}"#);

    let mut last_seq = 0;
    let mut completions = 0;
    while completions < 2 {
        let event = handle.wait_event("two completions", |_| true);
        let seq = event["seq"].as_u64().unwrap();
        assert!(seq > last_seq, "seq 必须单调递增: {last_seq} -> {seq}");
        last_seq = seq;
        if event["type"] == "command_completed" {
            completions += 1;
        }
    }
}

#[test]
fn missing_file_load_reports_asynchronous_error() {
    let handle = TestHandle::new();
    let missing = std::env::temp_dir().join("dyl_ffi_missing_媒体.mp4");
    let missing = missing.to_string_lossy().to_string();
    let payload = format!(
        r#"{{"schema":1,"request_id":42,"op":"load_file","url":{}}}"#,
        serde_json::to_string(&missing).unwrap()
    );

    assert_eq!(handle.command(&payload), 42);

    let loading = handle.wait_event("state_changed(loading)", |event| {
        event["type"] == "state_changed" && event["data"]["to"] == "loading"
    });
    assert_eq!(loading["data"]["from"], "idle");

    let error_state = handle.wait_event("state_changed(error)", |event| {
        event["type"] == "state_changed" && event["data"]["to"] == "error"
    });
    assert_eq!(error_state["data"]["from"], "loading");

    let completed = wait_completed(&handle, 42);
    assert_eq!(completed["data"]["status"], "error");
    assert_eq!(completed["data"]["error"]["code"], "file_not_found");
    assert_eq!(completed["data"]["error"]["code_num"], -3);

    let snapshot = handle.snapshot();
    assert_eq!(snapshot.state, dyl_core::contract::PlayerState::Error);
    assert!(snapshot.source.is_none());
    let last_error = snapshot.last_error.expect("last_error recorded");
    assert!(last_error.message.contains("dyl_ffi_missing_媒体.mp4"));
}

// ═══════════════════════════════════════════════════
// 5. 快照与属性
// ═══════════════════════════════════════════════════

#[test]
fn get_property_exposes_top_level_fields_only() {
    let handle = TestHandle::new();

    assert_eq!(
        take_string(dyl_get_property(handle.0, c("volume").as_ptr())).as_deref(),
        Some("100")
    );
    assert_eq!(
        take_string(dyl_get_property(handle.0, c("state").as_ptr())).as_deref(),
        Some("\"idle\"")
    );
    assert_eq!(
        take_string(dyl_get_property(handle.0, c("source").as_ptr())).as_deref(),
        Some("null")
    );

    let surface = take_string(dyl_get_property(handle.0, c("surface").as_ptr())).expect("surface");
    let surface: Value = serde_json::from_str(&surface).unwrap();
    assert_eq!(surface["status"], "none");

    assert!(
        dyl_get_property(handle.0, c("no_such_field").as_ptr()).is_null(),
        "未知字段返回 null"
    );
    let error = take_string(dyl_last_error(handle.0)).expect("last_error");
    assert!(error.contains("unknown property"));
}

// ═══════════════════════════════════════════════════
// 6. 事件回调与 poll 互斥
// ═══════════════════════════════════════════════════

struct CallbackLog(Mutex<Vec<Value>>);

extern "C" fn record_event(json: *const c_char, user_data: *mut c_void) {
    assert!(!json.is_null());
    let text = unsafe { CStr::from_ptr(json) }.to_string_lossy();
    let event: Value = serde_json::from_str(&text).expect("callback receives valid JSON");
    unsafe { &*(user_data as *const CallbackLog) }
        .0
        .lock()
        .push(event);
}

static DESTROY_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);

extern "C" fn count_event(_json: *const c_char, _user_data: *mut c_void) {
    DESTROY_CALLBACK_COUNT.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn callback_mode_takes_over_event_stream_and_destroy_joins() {
    let log = Box::into_raw(Box::new(CallbackLog(Mutex::new(Vec::new()))));
    let handle = TestHandle::new();

    assert_eq!(
        dyl_set_event_callback(handle.0, Some(record_event), log as *mut c_void),
        0
    );
    assert_eq!(
        dyl_set_event_callback(handle.0, Some(record_event), log as *mut c_void),
        -2,
        "重复设置回调应被拒绝"
    );
    assert!(
        dyl_poll_event(handle.0).is_null(),
        "回调模式接管后 poll 永久返回 null"
    );

    assert_eq!(
        handle.command(r#"{"schema":1,"request_id":3,"op":"set_volume","volume":66}"#),
        3
    );

    let deadline = Instant::now() + WAIT;
    loop {
        let saw_completion = unsafe { &*log }
            .0
            .lock()
            .iter()
            .any(|event| completion(event).is_some_and(|data| data["request_id"] == 3));
        if saw_completion {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "回调线程应收到 command_completed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    drop(handle);
    // destroy 返回后回调线程已 join，日志不会再增长
    let count = unsafe { &*log }.0.lock().len();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(unsafe { &*log }.0.lock().len(), count);
    unsafe { drop(Box::from_raw(log)) };
}

#[test]
fn destroy_waits_for_callback_thread() {
    let before = DESTROY_CALLBACK_COUNT.load(Ordering::SeqCst);
    let handle = TestHandle::new();
    dyl_set_event_callback(handle.0, Some(count_event), std::ptr::null_mut());

    handle.command(r#"{"schema":1,"op":"set_mute","muted":true}"#);
    drop(handle);

    assert!(
        DESTROY_CALLBACK_COUNT.load(Ordering::SeqCst) > before,
        "destroy 返回前回调线程必须收到过事件并退出"
    );
}

static DESTROY_FROM_CALLBACK_RAN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

extern "C" fn destroy_from_callback(_json: *const c_char, user_data: *mut c_void) {
    dyl_destroy(user_data as *mut dyl::DylHandle);
    DESTROY_FROM_CALLBACK_RAN.store(true, Ordering::SeqCst);
}

#[test]
fn destroy_from_callback_is_rejected_without_self_join() {
    let handle = TestHandle::new();
    assert_eq!(
        dyl_set_event_callback(
            handle.0,
            Some(destroy_from_callback),
            handle.0 as *mut c_void
        ),
        0
    );

    handle.command(r#"{"schema":1,"op":"set_mute","muted":true}"#);

    let deadline = Instant::now() + WAIT;
    while !DESTROY_FROM_CALLBACK_RAN.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "回调线程应执行过一次 destroy 尝试"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // 句柄被拒绝销毁后仍然可用；从这里读 last_error 会取走它
    let snapshot = take_string(dyl_get_snapshot(handle.0)).expect("handle still alive");
    let snapshot: Snapshot = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(snapshot.state, dyl_core::contract::PlayerState::Idle);

    let error = take_string(dyl_last_error(handle.0)).expect("rejection recorded");
    assert!(
        error.contains("event callback thread"),
        "last_error 应说明回调线程内禁止 destroy: {error}"
    );

    // 主线程 destroy 正常退出，证明回调线程没有自我 join 卡死
    drop(handle);
}

// ═══════════════════════════════════════════════════
// 7. Surface 入口
// ═══════════════════════════════════════════════════

#[test]
fn surface_operations_require_attachment() {
    let handle = TestHandle::new();
    let desc = c(
        r#"{"schema":1,"kind":"windows_hwnd","handle":"0x1","width":640,"height":360,"scale_factor":1.0}"#,
    );

    assert_eq!(
        dyl_surface_detach(handle.0),
        0,
        "未 attach 时 detach 为 no-op"
    );
    assert_eq!(
        dyl_surface_resize(handle.0, desc.as_ptr()),
        -2,
        "resize 要求已有 attach"
    );
    assert_eq!(
        dyl_surface_redraw(handle.0),
        0,
        "未 attach 时 redraw 为 no-op，不能因宿主重绘通知报错"
    );
}

#[test]
fn unsupported_surface_kind_is_rejected_without_touching_gpu() {
    let handle = TestHandle::new();
    let desc = c(&unsupported_surface_json());

    assert_eq!(dyl_surface_attach(handle.0, desc.as_ptr()), -9);
    let error = take_string(dyl_last_error(handle.0)).expect("last_error");
    assert!(error.contains("not supported"), "{error}");
}

#[test]
fn malformed_surface_descriptor_is_invalid_argument() {
    let handle = TestHandle::new();

    assert_eq!(dyl_surface_attach(handle.0, c("not json").as_ptr()), -1);
    assert_eq!(
        dyl_surface_attach(handle.0, c(r#"{"kind":"windows_hwnd"}"#).as_ptr()),
        -1
    );
    assert!(take_string(dyl_last_error(handle.0)).is_some());
}
