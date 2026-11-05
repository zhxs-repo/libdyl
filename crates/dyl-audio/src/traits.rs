//! 音频输出抽象
//!
//! trait 单独成模块，是为了让平台实现可以被 cfg 掉：
//! `CpalOutput` 需要 cpal（cpal 在 Android 上会拖进 oboe + CMake 构建），
//! `AaudioOutput` 只在 Android 编译。两边共用这里的 trait 签名，
//! 于是 `cpal_output` 整个模块可以加 `#[cfg(not(target_os = "android"))]`，
//! 而 `dyl-pipeline` 等调用方不需要任何条件编译。

use dyl_core::Result;

use crate::clock::AudioClock;
use dyl_ffmpeg::AudioFrame;

/// AudioOutput trait — 音频输出抽象
pub trait AudioOutput: Send {
    /// 初始化音频输出
    fn init(&mut self, sample_rate: u32, channels: u32) -> Result<()>;

    /// 写入音频
    fn write(&mut self, frame: &AudioFrame) -> Result<()>;

    /// 设置音量（0.0 ~ 1.0）
    fn set_volume(&mut self, volume: f32);

    /// 获取当前音量
    fn volume(&self) -> f32;

    /// 现在写入这一帧放得下吗？
    ///
    /// 供调用方做背压：装不下就先不取新帧，而不是在 `write` 里丢采样。
    /// 默认 `true`：无法预知缓冲容量的实现不拦写入。
    fn accepts(&self, _frame: &AudioFrame) -> bool {
        true
    }

    /// 音频主时钟句柄；输出实现无法提供位置信息时返回 `None`
    ///
    /// 返回的句柄与输出侧共享同一份状态，`AVSync` 用它替代挂钟做主时钟。
    fn clock(&self) -> Option<AudioClock> {
        None
    }

    /// 冻结设备消费（暂停播放时必须调用，否则设备会把缓冲里的采样继续放完）
    fn pause(&mut self) -> Result<()> {
        Ok(())
    }

    /// 恢复设备消费
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }

    /// 丢弃缓冲里尚未播放的采样（seek / stop）
    ///
    /// 同时清空主时钟的未播计数：这些采样永远不会到达扬声器。
    fn flush(&mut self) {}

    /// 是否还有已写入但尚未交给设备的采样？
    ///
    /// 音频流读到末尾时调用方要等它排空再退出：立刻 drop 掉输出会把缓冲里
    /// 最后的采样连同设备一起丢掉，结尾被截断，主时钟也停在片长之前。
    /// 默认 `false`：不维护缓冲的实现无需等待。
    fn has_pending_audio(&self) -> bool {
        false
    }
}
