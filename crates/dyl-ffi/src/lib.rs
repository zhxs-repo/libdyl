//! Dyl C ABI — 薄适配层
//!
//! 所有 `#[no_mangle] pub extern "C"` 函数集中在此 crate。编排在
//! `dyl-engine`，本 crate 只负责 opaque 句柄、JSON 编解码、事件线程与
//! panic 守卫。这里没有播放逻辑，也不缓存引擎状态。

// FFI 边界函数接收 raw pointer 参数是 C ABI 要求，由调用方保证有效性
#![allow(clippy::not_unsafe_ptr_arg_deref)]

/// C ABI panic 守卫：捕获闭包内 panic，避免 unwind 跨越 extern "C" 边界（UB/abort）
macro_rules! cabi {
    ($name:expr, $fallback:expr, $body:expr) => {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe($body)) {
            Ok(v) => v,
            Err(_) => {
                tracing::error!("{}: panic caught at FFI boundary", $name);
                $fallback
            }
        }
    };
}

#[cfg(target_os = "android")]
mod android_log;

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::thread::JoinHandle;

thread_local! {
    /// 事件回调线程标记。在该线程上调用 `dyl_destroy` 会让
    /// `DylHandle::drop` join 自己（平台相关地死锁或 abort），必须拒绝。
    static ON_EVENT_CALLBACK_THREAD: Cell<bool> = const { Cell::new(false) };
}

use crossbeam_channel::Receiver;
use dyl_core::contract::{CommandEnvelope, ErrorCode, EventEnvelope, SurfaceDescriptor};
use dyl_engine::{Engine, EngineError};
use parking_lot::Mutex;

// ── ABI 常量（由 cbindgen 写进 dyl.h） ──────────────────────────
pub const DYL_ABI_VERSION_MAJOR: u32 = 0;
pub const DYL_ABI_VERSION_MINOR: u32 = 6;
pub const DYL_ABI_VERSION_PATCH: u32 = 0;
pub const DYL_ABI_VERSION: u32 =
    (DYL_ABI_VERSION_MAJOR << 16) | (DYL_ABI_VERSION_MINOR << 8) | DYL_ABI_VERSION_PATCH;

pub const DYL_OK: i32 = 0;
pub const DYL_ERR_INVALID_ARGUMENT: i32 = -1;
pub const DYL_ERR_INVALID_STATE: i32 = -2;
pub const DYL_ERR_FILE_NOT_FOUND: i32 = -3;
pub const DYL_ERR_DEMUX: i32 = -4;
pub const DYL_ERR_DECODE: i32 = -5;
pub const DYL_ERR_RENDER: i32 = -6;
pub const DYL_ERR_AUDIO: i32 = -7;
pub const DYL_ERR_PIPELINE: i32 = -8;
pub const DYL_ERR_NOT_SUPPORTED: i32 = -9;
pub const DYL_ERR_SURFACE: i32 = -10;
pub const DYL_ERR_BUSY: i32 = -11;
pub const DYL_ERR_TIMEOUT: i32 = -12;
pub const DYL_ERR_IO: i32 = -13;
pub const DYL_ERR_CANCELLED: i32 = -14;
pub const DYL_ERR_INTERNAL: i32 = -99;

/// 事件回调函数类型
///
/// 仅供 Rust 侧签名使用；`#[no_mangle]` 导出函数里的回调参数必须写成匿名
/// `Option<extern "C" fn(..)>`，否则 cbindgen 只输出占位 struct，C 头文件里
/// 无法声明回调。
pub type DylEventCallback = extern "C" fn(*const c_char, *mut c_void);

struct EventSink {
    /// poll 模式持有；回调线程接管后为 None
    rx: Option<Receiver<EventEnvelope>>,
    callback_thread: Option<JoinHandle<()>>,
}

/// 不透明句柄 — Dart 侧只看到指针
pub struct DylHandle {
    engine: Engine,
    events: Mutex<EventSink>,
    last_error: Mutex<Option<String>>,
}

impl DylHandle {
    fn set_last_error(&self, message: impl Into<String>) {
        *self.last_error.lock() = Some(message.into());
    }

    fn fail(&self, error: &EngineError) -> c_int {
        self.set_last_error(error.message.clone());
        error.code.as_i32()
    }
}

impl Drop for DylHandle {
    fn drop(&mut self) {
        // 先停引擎：控制线程退出会丢弃事件发送端，回调线程随之结束
        self.engine.destroy();

        let thread = self.events.lock().callback_thread.take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

// ═══════════════════════════════════════════════════
// 内部实现
// ═══════════════════════════════════════════════════

unsafe fn read_str<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        None
    } else {
        CStr::from_ptr(text).to_str().ok()
    }
}

fn json_to_c_string(json: String) -> *mut c_char {
    match CString::new(json) {
        Ok(text) => text.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

fn init_logging() {
    // Android 上没有 RUST_LOG，stdout 也不通往任何有用的地方，所以改写到
    // logcat（`adb logcat -s dyl`）。见 android_log 模块的说明。
    #[cfg(target_os = "android")]
    crate::android_log::init();

    #[cfg(not(target_os = "android"))]
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

fn dyl_create_impl() -> *mut DylHandle {
    init_logging();

    let (engine, event_rx) = match Engine::new() {
        Ok(pair) => pair,
        Err(error) => {
            tracing::error!("dyl_create: engine failed to start: {error}");
            return ptr::null_mut();
        }
    };

    Box::into_raw(Box::new(DylHandle {
        engine,
        events: Mutex::new(EventSink {
            rx: Some(event_rx),
            callback_thread: None,
        }),
        last_error: Mutex::new(None),
    }))
}

fn dyl_destroy_impl(handle: *mut DylHandle) {
    if handle.is_null() {
        return;
    }
    if ON_EVENT_CALLBACK_THREAD.with(Cell::get) {
        let handle = unsafe { &*handle };
        handle.set_last_error(
            "dyl_destroy must not be called from the event callback thread; handle not destroyed",
        );
        tracing::error!("dyl_destroy rejected: called on the event callback thread");
        return;
    }
    // Drop 顺序：engine.destroy() 停控制线程与输出，再 join 事件回调线程
    drop(unsafe { Box::from_raw(handle) });
}

fn dyl_command_impl(handle: *mut DylHandle, command_json: *const c_char) -> i64 {
    if handle.is_null() {
        return ErrorCode::InvalidArgument.as_i32() as i64;
    }
    let handle = unsafe { &*handle };

    let Some(text) = (unsafe { read_str(command_json) }) else {
        handle.set_last_error("command must be a valid UTF-8 JSON string");
        return ErrorCode::InvalidArgument.as_i32() as i64;
    };
    let envelope: CommandEnvelope = match serde_json::from_str(text) {
        Ok(envelope) => envelope,
        Err(error) => {
            handle.set_last_error(format!("parse command: {error}"));
            return ErrorCode::InvalidArgument.as_i32() as i64;
        }
    };

    match handle.engine.command(envelope) {
        Ok(request_id) => request_id as i64,
        Err(error) => handle.fail(&error) as i64,
    }
}

fn dyl_get_snapshot_impl(handle: *mut DylHandle) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle = unsafe { &*handle };
    match serde_json::to_string(handle.engine.snapshot().as_ref()) {
        Ok(json) => json_to_c_string(json),
        Err(error) => {
            handle.set_last_error(format!("serialize snapshot: {error}"));
            ptr::null_mut()
        }
    }
}

fn dyl_get_property_impl(handle: *mut DylHandle, name: *const c_char) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle = unsafe { &*handle };
    let Some(name) = (unsafe { read_str(name) }) else {
        handle.set_last_error("property name must be valid UTF-8");
        return ptr::null_mut();
    };

    let Ok(serde_json::Value::Object(mut fields)) =
        serde_json::to_value(handle.engine.snapshot().as_ref())
    else {
        handle.set_last_error("serialize snapshot");
        return ptr::null_mut();
    };

    match fields.remove(name) {
        // 已知字段即使值为 null 也返回 "null"，只有未知字段才走错误路径
        Some(value) => json_to_c_string(value.to_string()),
        None => {
            handle.set_last_error(format!("unknown property: {name}"));
            ptr::null_mut()
        }
    }
}

fn parse_surface(handle: &DylHandle, desc_json: *const c_char) -> Result<SurfaceDescriptor, c_int> {
    let Some(text) = (unsafe { read_str(desc_json) }) else {
        handle.set_last_error("surface descriptor must be a valid UTF-8 JSON string");
        return Err(ErrorCode::InvalidArgument.as_i32());
    };
    serde_json::from_str(text).map_err(|error| {
        handle.set_last_error(format!("parse surface descriptor: {error}"));
        ErrorCode::InvalidArgument.as_i32()
    })
}

fn dyl_surface_attach_impl(handle: *mut DylHandle, desc_json: *const c_char) -> c_int {
    if handle.is_null() {
        return ErrorCode::InvalidArgument.as_i32();
    }
    let handle = unsafe { &*handle };
    let desc = match parse_surface(handle, desc_json) {
        Ok(desc) => desc,
        Err(code) => return code,
    };
    match handle.engine.attach_surface(desc) {
        Ok(()) => ErrorCode::Ok.as_i32(),
        Err(error) => handle.fail(&error),
    }
}

fn dyl_surface_detach_impl(handle: *mut DylHandle) -> c_int {
    if handle.is_null() {
        return ErrorCode::InvalidArgument.as_i32();
    }
    let handle = unsafe { &*handle };
    match handle.engine.detach_surface() {
        Ok(()) => ErrorCode::Ok.as_i32(),
        Err(error) => handle.fail(&error),
    }
}

fn dyl_surface_resize_impl(handle: *mut DylHandle, desc_json: *const c_char) -> c_int {
    if handle.is_null() {
        return ErrorCode::InvalidArgument.as_i32();
    }
    let handle = unsafe { &*handle };
    let desc = match parse_surface(handle, desc_json) {
        Ok(desc) => desc,
        Err(code) => return code,
    };
    let (width, height) = desc.size();
    match handle.engine.resize_surface(width, height) {
        Ok(()) => ErrorCode::Ok.as_i32(),
        Err(error) => handle.fail(&error),
    }
}

fn dyl_surface_redraw_impl(handle: *mut DylHandle) -> c_int {
    if handle.is_null() {
        return ErrorCode::InvalidArgument.as_i32();
    }
    let handle = unsafe { &*handle };
    match handle.engine.redraw_surface() {
        Ok(()) => ErrorCode::Ok.as_i32(),
        Err(error) => handle.fail(&error),
    }
}

fn dyl_set_event_callback_impl(
    handle: *mut DylHandle,
    callback: Option<DylEventCallback>,
    user_data: *mut c_void,
) -> c_int {
    if handle.is_null() {
        return ErrorCode::InvalidArgument.as_i32();
    }
    let Some(callback) = callback else {
        return ErrorCode::InvalidArgument.as_i32();
    };
    let handle = unsafe { &*handle };

    let mut sink = handle.events.lock();
    if sink.callback_thread.is_some() {
        handle.set_last_error("event callback is already set");
        return ErrorCode::InvalidState.as_i32();
    }
    let Some(rx) = sink.rx.as_ref().cloned() else {
        handle.set_last_error("event stream is owned by a callback");
        return ErrorCode::InvalidState.as_i32();
    };

    let user_data = user_data as usize;
    let spawned = std::thread::Builder::new()
        .name("dyl-events".into())
        .spawn(move || {
            ON_EVENT_CALLBACK_THREAD.with(|flag| flag.set(true));
            while let Ok(event) = rx.recv() {
                let Ok(json) = serde_json::to_string(&event) else {
                    continue;
                };
                let Ok(text) = CString::new(json) else {
                    continue;
                };
                // 指针只在回调调用期间有效，回调必须自行拷贝
                callback(text.as_ptr(), user_data as *mut c_void);
            }
        });

    match spawned {
        Ok(thread) => {
            sink.rx = None;
            sink.callback_thread = Some(thread);
            ErrorCode::Ok.as_i32()
        }
        Err(error) => {
            handle.set_last_error(format!("spawn event thread: {error}"));
            ErrorCode::Internal.as_i32()
        }
    }
}

fn dyl_poll_event_impl(handle: *mut DylHandle) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle = unsafe { &*handle };
    let sink = handle.events.lock();
    // 回调模式接管事件流后，poll 永久返回 null
    let Some(rx) = sink.rx.as_ref() else {
        return ptr::null_mut();
    };
    match rx.try_recv() {
        Ok(event) => match serde_json::to_string(&event) {
            Ok(json) => json_to_c_string(json),
            Err(error) => {
                tracing::warn!("event serialize error: {error}");
                ptr::null_mut()
            }
        },
        Err(_) => ptr::null_mut(),
    }
}

fn dyl_capabilities_impl() -> *mut c_char {
    match serde_json::to_string(&dyl_engine::capabilities::capabilities()) {
        Ok(json) => json_to_c_string(json),
        Err(_) => ptr::null_mut(),
    }
}

/// 错误码的静态字符串；调用方不得释放。
fn dyl_error_string_impl(code: c_int) -> *const c_char {
    let text: &'static [u8] = match code {
        0 => b"ok\0",
        -1 => b"invalid_argument\0",
        -2 => b"invalid_state\0",
        -3 => b"file_not_found\0",
        -4 => b"demux\0",
        -5 => b"decode\0",
        -6 => b"render\0",
        -7 => b"audio\0",
        -8 => b"pipeline\0",
        -9 => b"not_supported\0",
        -10 => b"surface\0",
        -11 => b"busy\0",
        -12 => b"timeout\0",
        -13 => b"io\0",
        -14 => b"cancelled\0",
        -99 => b"internal\0",
        _ => b"unknown\0",
    };
    text.as_ptr().cast()
}

fn dyl_free_string_impl(text: *mut c_char) {
    if !text.is_null() {
        unsafe {
            let _ = CString::from_raw(text);
        }
    }
}

fn dyl_last_error_impl(handle: *mut DylHandle) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle = unsafe { &*handle };
    match handle.last_error.lock().take() {
        Some(message) => json_to_c_string(message),
        None => ptr::null_mut(),
    }
}

// ═══════════════════════════════════════════════════
// C ABI 导出 — 每个入口都在 panic 守卫内
// ═══════════════════════════════════════════════════

/// 返回 ABI 版本 `(major<<16)|(minor<<8)|patch`。
#[no_mangle]
pub extern "C" fn dyl_abi_version() -> u32 {
    cabi!("dyl_abi_version", DYL_ABI_VERSION, || { DYL_ABI_VERSION })
}

/// 返回当前构建的能力 JSON；调用方用 `dyl_free_string` 释放。
#[no_mangle]
pub extern "C" fn dyl_capabilities() -> *mut c_char {
    cabi!("dyl_capabilities", ptr::null_mut(), || {
        dyl_capabilities_impl()
    })
}

/// 错误码的静态字符串；调用方不得释放。
#[no_mangle]
pub extern "C" fn dyl_error_string(code: c_int) -> *const c_char {
    cabi!("dyl_error_string", c"unknown".as_ptr(), || {
        dyl_error_string_impl(code)
    })
}

/// 创建播放器实例；失败返回 null。
#[no_mangle]
// cabi! 需要闭包（AssertUnwindSafe 包裹的必须是 FnOnce，直接传函数名编译不过）
#[allow(clippy::redundant_closure)]
pub extern "C" fn dyl_create() -> *mut DylHandle {
    cabi!("dyl_create", ptr::null_mut(), || dyl_create_impl())
}

/// 销毁播放器实例；null 为 no-op，重复调用同一指针是未定义行为。
#[no_mangle]
pub extern "C" fn dyl_destroy(handle: *mut DylHandle) {
    cabi!("dyl_destroy", (), || dyl_destroy_impl(handle))
}

/// 发送命令信封。
///
/// 返回 `request_id > 0` 表示已入队；返回负数错误码表示同步拒绝。
/// 状态错误、文件错误等异步结果通过 `command_completed` 事件报告。
#[no_mangle]
pub extern "C" fn dyl_command(handle: *mut DylHandle, command_json: *const c_char) -> i64 {
    cabi!("dyl_command", ErrorCode::Internal.as_i32() as i64, || {
        dyl_command_impl(handle, command_json)
    })
}

/// 返回只读快照 JSON；调用方用 `dyl_free_string` 释放。
#[no_mangle]
pub extern "C" fn dyl_get_snapshot(handle: *mut DylHandle) -> *mut c_char {
    cabi!("dyl_get_snapshot", ptr::null_mut(), || {
        dyl_get_snapshot_impl(handle)
    })
}

/// 读取快照的顶层字段（返回 JSON 值）；未知字段返回 null 并写入 `last_error`。
#[no_mangle]
pub extern "C" fn dyl_get_property(handle: *mut DylHandle, name: *const c_char) -> *mut c_char {
    cabi!("dyl_get_property", ptr::null_mut(), || {
        dyl_get_property_impl(handle, name)
    })
}

/// 接入外部 Surface；描述符为 JSON，句柄用字符串。
#[no_mangle]
pub extern "C" fn dyl_surface_attach(handle: *mut DylHandle, desc_json: *const c_char) -> c_int {
    cabi!("dyl_surface_attach", ErrorCode::Internal.as_i32(), || {
        dyl_surface_attach_impl(handle, desc_json)
    })
}

/// 同步分离 Surface；返回 Ok 后宿主才能释放原生句柄。
#[no_mangle]
pub extern "C" fn dyl_surface_detach(handle: *mut DylHandle) -> c_int {
    cabi!("dyl_surface_detach", ErrorCode::Internal.as_i32(), || {
        dyl_surface_detach_impl(handle)
    })
}

/// 调整已 attach 的 Surface 尺寸；描述符只需携带尺寸字段。
#[no_mangle]
pub extern "C" fn dyl_surface_resize(handle: *mut DylHandle, desc_json: *const c_char) -> c_int {
    cabi!("dyl_surface_resize", ErrorCode::Internal.as_i32(), || {
        dyl_surface_resize_impl(handle, desc_json)
    })
}

/// 请求把当前画面重新上屏一次（宿主窗口被系统擦除时）。
///
/// 暂停/EOF 后内核不再主动 present，宿主收到重绘通知（如 Windows `WM_PAINT`）
/// 时应调用本函数恢复静止画面。未 attach Surface 时为 no-op。
#[no_mangle]
pub extern "C" fn dyl_surface_redraw(handle: *mut DylHandle) -> c_int {
    cabi!("dyl_surface_redraw", ErrorCode::Internal.as_i32(), || {
        dyl_surface_redraw_impl(handle)
    })
}

/// 设置事件回调；回调在 `dyl-events` 线程上执行。
///
/// 设置后 `dyl_poll_event` 永久返回 null。回调内的字符串指针只在调用期间
/// 有效。在该回调线程内调用 `dyl_destroy` 会被拒绝（句柄保持有效）并把
/// 原因写进 `dyl_last_error`。
#[no_mangle]
pub extern "C" fn dyl_set_event_callback(
    handle: *mut DylHandle,
    callback: Option<extern "C" fn(*const c_char, *mut c_void)>,
    user_data: *mut c_void,
) -> c_int {
    cabi!(
        "dyl_set_event_callback",
        ErrorCode::Internal.as_i32(),
        || { dyl_set_event_callback_impl(handle, callback, user_data) }
    )
}

/// 非阻塞轮询事件 JSON；无事件或已设置回调时返回 null。
#[no_mangle]
pub extern "C" fn dyl_poll_event(handle: *mut DylHandle) -> *mut c_char {
    cabi!("dyl_poll_event", ptr::null_mut(), || {
        dyl_poll_event_impl(handle)
    })
}

/// 释放由 dyl 分配的字符串。
#[no_mangle]
pub extern "C" fn dyl_free_string(text: *mut c_char) {
    cabi!("dyl_free_string", (), || dyl_free_string_impl(text))
}

/// 取走并清空最近一次同步错误信息；无错误时返回 null。
#[no_mangle]
pub extern "C" fn dyl_last_error(handle: *mut DylHandle) -> *mut c_char {
    cabi!("dyl_last_error", ptr::null_mut(), || {
        dyl_last_error_impl(handle)
    })
}

#[cfg(test)]
mod abi_tests {
    use super::*;
    use dyl_core::contract::{AbiVersion, Capabilities, ErrorCode as ContractErrorCode};
    use std::ffi::CStr;

    #[test]
    fn abi_version_is_packed_0_6_0() {
        assert_eq!(DYL_ABI_VERSION_MAJOR, 0);
        assert_eq!(DYL_ABI_VERSION_MINOR, 6);
        assert_eq!(DYL_ABI_VERSION_PATCH, 0);
        assert_eq!(DYL_ABI_VERSION, 0x0000_0600);
        assert_eq!(
            dyl_abi_version(),
            dyl_core::contract::AbiVersion::V0_6_0.packed()
        );
    }

    #[test]
    fn error_strings_match_contract_table() {
        for code in ContractErrorCode::ALL {
            let ptr = dyl_error_string(code.as_i32());
            assert!(!ptr.is_null());
            let text = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap();
            assert_eq!(text, code.as_str());
        }
        let unknown = unsafe { CStr::from_ptr(dyl_error_string(4242)) }
            .to_str()
            .unwrap();
        assert_eq!(unknown, "unknown");
    }

    #[test]
    fn capabilities_json_is_a_valid_contract_payload() {
        let ptr = dyl_capabilities();
        assert!(!ptr.is_null());
        let json = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
        dyl_free_string_impl(ptr);

        let caps: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(caps.abi, AbiVersion::V0_6_0);
        assert!(!caps.presentation.texture_sharing);
        assert!(caps.features.danmaku_xml_entities);
        assert_eq!(
            caps.features.seek_modes,
            vec![
                dyl_core::contract::SeekMode::Exact,
                dyl_core::contract::SeekMode::Keyframe
            ]
        );
    }
}
