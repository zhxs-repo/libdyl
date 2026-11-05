//! wgpu 未捕获错误的接管。
//!
//! 存在的唯一理由：**wgpu 默认把任何未捕获错误当成致命错误，直接 panic
//! （abort）掉整个进程**。而 release 构建里它只会打一行
//! `Handling wgpu errors as fatal by default`，**真正的错误内容一个字都不打**。
//!
//! 一次真机实测（XT2125-4 / Android 12 / GLES 后端）撞的就是这个：换个分辨率
//! 的视频，首帧提交后进程 SIGABRT，日志里除了那句通用文案之外没有任何线索，
//! 错误本体根本没被打印出来，只能靠猜是纹理超限还是 surface 配置不合法。
//!
//! 所以在 device 建好之后立刻装上自己的 handler：把完整错误打进日志，并且
//! **不再 abort**。渲染出问题应该退化成黑屏或者掉一帧，而不是把播放器和
//! 宿主 app 一起杀掉 —— 在 Android 上这意味着整个进程被系统回收，用户的
//! app 直接闪退。
//!
//! 注意 wgpu 29 的 API 位置：这个回调挂在 **Device** 上（`device.on_uncaptured_error`），
//! 不是 `InstanceDescriptor` 上 —— 后者在 29 里已经没有这个字段了。

use std::sync::Arc;

/// 把 device 的未捕获错误处理换成"记日志但不致命"。
///
/// 必须在 `request_device` 之后、任何渲染调用之前调用，否则首个错误就已经
/// panic 了。
pub fn install_nonfatal_error_handler(device: &wgpu::Device) {
    device.on_uncaptured_error(Arc::new(|error: wgpu::Error| {
        // 用 ERROR 级别 + `{:?}`：`Error` 的 Display 只给一句话，Debug 才带上
        // validation 的具体字段（超限的尺寸、对不上的 format、缺失的能力位）。
        tracing::error!("wgpu uncaptured error: {error:?}");
    }));
}
