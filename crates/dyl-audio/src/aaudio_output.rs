//! AaudioOutput — Android AAudio 原生音频输出实现
//!
//! 使用 Android NDK 的 AAudio C API 实现低延迟音频输出。
//! 仅在 `target_os = "android"` 上编译；其他平台此模块不可用。
//!
//! 与 `CpalOutput` 的对比：
//! - **延迟**: AAudio 原生支持低延迟模式（AAUDIO_PERFORMANCE_MODE_LOW_LATENCY）
//! - **音频焦点**: 可通过 `setAudioFocus` 管理（Android 8.0+）
//! - **兼容性**: AAudio 需要 Android 8.0 (API 26+)，因此 `build.gradle.kts` 的
//!   minSdk 与 `.cargo/config.toml` 的 NDK target 都锁在 26；Android 上不再引入
//!   cpal/oboe（见 `dyl-audio/Cargo.toml` 的 target 门控）

#[cfg(target_os = "android")]
mod aaudio_sys {
    //! Raw FFI bindings to Android AAudio C API
    //!
    //! These are defined in `<aaudio/AAudio.h>` in the Android NDK.
    //! Link with `-laaudio`.

    #![allow(non_camel_case_types, non_snake_case, dead_code)]

    use std::os::raw::c_void;

    pub type aaudio_result_t = i32;
    pub type aaudio_stream_state_t = i32;
    pub type aaudio_direction_t = i32;
    pub type aaudio_format_t = i32;
    pub type aaudio_performance_mode_t = i32;
    pub type aaudio_sharing_mode_t = i32;
    pub type aaudio_data_callback_result_t = i32;

    pub const AAUDIO_OK: aaudio_result_t = 0;
    pub const AAUDIO_ERROR_BASE: aaudio_result_t = -900;

    pub const AAUDIO_DIRECTION_OUTPUT: aaudio_direction_t = 0;

    pub const AAUDIO_FORMAT_PCM_I16: aaudio_format_t = 1;
    pub const AAUDIO_FORMAT_PCM_FLOAT: aaudio_format_t = 2;

    pub const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: aaudio_performance_mode_t = 12;
    pub const AAUDIO_PERFORMANCE_MODE_NONE: aaudio_performance_mode_t = 10;

    pub const AAUDIO_SHARING_MODE_SHARED: aaudio_sharing_mode_t = 0;

    pub const AAUDIO_STREAM_STATE_OPEN: aaudio_stream_state_t = 1;
    pub const AAUDIO_STREAM_STATE_STARTED: aaudio_stream_state_t = 4;
    pub const AAUDIO_STREAM_STATE_PAUSED: aaudio_stream_state_t = 5;
    pub const AAUDIO_STREAM_STATE_STOPPED: aaudio_stream_state_t = 7;
    pub const AAUDIO_STREAM_STATE_CLOSING: aaudio_stream_state_t = 8;
    pub const AAUDIO_STREAM_STATE_CLOSED: aaudio_stream_state_t = 9;

    pub const AAUDIO_CALLBACK_RESULT_CONTINUE: aaudio_data_callback_result_t = 0;
    pub const AAUDIO_CALLBACK_RESULT_STOP: aaudio_data_callback_result_t = 1;

    pub const AAUDIO_UNSPECIFIED: i32 = 0;

    /// AAudio data callback function type
    pub type AAudioStream_dataCallback = Option<
        unsafe extern "C" fn(
            stream: *mut AAudioStreamStruct,
            userData: *mut c_void,
            audioData: *mut c_void,
            numFrames: i32,
        ) -> aaudio_data_callback_result_t,
    >;

    /// AAudio error callback function type
    pub type AAudioStream_errorCallback = Option<
        unsafe extern "C" fn(
            stream: *mut AAudioStreamStruct,
            userData: *mut c_void,
            error: aaudio_result_t,
        ),
    >;

    /// Opaque stream struct
    #[repr(C)]
    pub struct AAudioStreamStruct {
        _opaque: [u8; 0],
    }

    /// Opaque builder struct
    #[repr(C)]
    pub struct AAudioStreamBuilderStruct {
        _opaque: [u8; 0],
    }

    pub type AAudioStream = AAudioStreamStruct;
    pub type AAudioStreamBuilder = AAudioStreamBuilderStruct;

    // FFI function declarations
    //
    // `libaaudio.so` ships in the NDK's sysroot from API 26 onwards, so the
    // link attribute is what actually resolves these symbols at cdylib link
    // time. `-Wl,--no-undefined` (see dyl-ffmpeg/build.rs) makes a name
    // typo here a *build* failure instead of a runtime dlopen crash.
    #[link(name = "aaudio")]
    extern "C" {
        pub fn AAudio_createStreamBuilder(
            builder: *mut *mut AAudioStreamBuilder,
        ) -> aaudio_result_t;

        pub fn AAudioStreamBuilder_delete(builder: *mut AAudioStreamBuilder);

        pub fn AAudioStreamBuilder_setDirection(
            builder: *mut AAudioStreamBuilder,
            direction: aaudio_direction_t,
        );

        pub fn AAudioStreamBuilder_setSampleRate(
            builder: *mut AAudioStreamBuilder,
            sampleRate: i32,
        );

        pub fn AAudioStreamBuilder_setChannelCount(
            builder: *mut AAudioStreamBuilder,
            channelCount: i32,
        );

        // NOTE: the NDK spells this `setFormat`, not `setSampleFormat`. A wrong
        // name here would link silently (undefined symbols are allowed in a
        // cdylib) and only fail on the device as
        // `dlopen failed: undefined symbol: AAudioStreamBuilder_setSampleFormat`.
        pub fn AAudioStreamBuilder_setFormat(
            builder: *mut AAudioStreamBuilder,
            format: aaudio_format_t,
        );

        pub fn AAudioStreamBuilder_setPerformanceMode(
            builder: *mut AAudioStreamBuilder,
            mode: aaudio_performance_mode_t,
        );

        pub fn AAudioStreamBuilder_setSharingMode(
            builder: *mut AAudioStreamBuilder,
            sharingMode: aaudio_sharing_mode_t,
        );

        pub fn AAudioStreamBuilder_setDataCallback(
            builder: *mut AAudioStreamBuilder,
            callback: AAudioStream_dataCallback,
            userData: *mut c_void,
        );

        pub fn AAudioStreamBuilder_setErrorCallback(
            builder: *mut AAudioStreamBuilder,
            callback: AAudioStream_errorCallback,
            userData: *mut c_void,
        );

        pub fn AAudioStreamBuilder_openStream(
            builder: *mut AAudioStreamBuilder,
            stream: *mut *mut AAudioStream,
        ) -> aaudio_result_t;

        pub fn AAudioStream_requestStart(stream: *mut AAudioStream) -> aaudio_result_t;

        pub fn AAudioStream_requestPause(stream: *mut AAudioStream) -> aaudio_result_t;

        pub fn AAudioStream_requestStop(stream: *mut AAudioStream) -> aaudio_result_t;

        pub fn AAudioStream_close(stream: *mut AAudioStream) -> aaudio_result_t;

        pub fn AAudioStream_getState(stream: *mut AAudioStream) -> aaudio_stream_state_t;

        pub fn AAudioStream_getSampleRate(stream: *mut AAudioStream) -> i32;

        pub fn AAudioStream_getChannelCount(stream: *mut AAudioStream) -> i32;

        pub fn AAudioStream_getFramesPerBurst(stream: *mut AAudioStream) -> i32;

        pub fn AAudioStream_write(
            stream: *mut AAudioStream,
            buffer: *const c_void,
            numFrames: i32,
            timeoutNanoseconds: i64,
        ) -> i32; // returns frames written or negative error
    }
}

use std::collections::VecDeque;
use std::sync::Arc;

use dyl_core::{DylError, Result};
use dyl_ffmpeg::AudioFrame;
use parking_lot::Mutex;

use crate::traits::AudioOutput;

/// AAudio ring buffer 容量上限：约 1 秒的立体声 48kHz 采样
const MAX_BUFFER_SAMPLES: usize = 48000 * 2;

/// AAudio 音频输出实现（仅 Android）
///
/// 使用 AAudio C API 的 push 模式（`AAudioStream_write`）输出音频。
/// 与 `CpalOutput` 的 pull 模式（回调）不同，push 模式由调用方主动写入数据。
///
/// 优势：
/// - 低延迟（`AAUDIO_PERFORMANCE_MODE_LOW_LATENCY`）
/// - 原生音频焦点管理
/// - 无需 cpal 中间层
#[cfg(target_os = "android")]
pub struct AaudioOutput {
    stream: Option<*mut aaudio_sys::AAudioStream>,
    /// 设备声道布局的待播采样：写入 AAudio 前的本地缓冲
    buffer: Arc<Mutex<VecDeque<f32>>>,
    volume: Arc<std::sync::atomic::AtomicU32>,
    /// 源采样率 / 声道数（来自媒体）
    sample_rate: u32,
    channels: u32,
    /// 流的实际采样率 / 声道数：AAudio 可能拒绝单声道（-889）后自行改开立体声，
    /// 与源布局不一致时必须在这里转换，否则按单声道解释立体声流会越界读
    device_rate: u32,
    device_channels: u32,
}

// SAFETY: AaudioOutput is only used from a dedicated audio thread.
// The AAudioStream pointer is not accessed concurrently.
#[cfg(target_os = "android")]
unsafe impl Send for AaudioOutput {}

#[cfg(target_os = "android")]
impl AaudioOutput {
    pub fn new() -> Self {
        Self {
            stream: None,
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            volume: Arc::new(std::sync::atomic::AtomicU32::new(1.0f32.to_bits())),
            sample_rate: 0,
            channels: 0,
            device_rate: 0,
            device_channels: 0,
        }
    }
}

#[cfg(target_os = "android")]
impl Default for AaudioOutput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "android")]
impl AudioOutput for AaudioOutput {
    fn init(&mut self, sample_rate: u32, channels: u32) -> Result<()> {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.device_rate = sample_rate;
        self.device_channels = channels;

        tracing::info!("AaudioOutput::init: {}Hz {}ch", sample_rate, channels);

        unsafe {
            let mut builder: *mut aaudio_sys::AAudioStreamBuilder = std::ptr::null_mut();
            let result = aaudio_sys::AAudio_createStreamBuilder(&mut builder);
            if result != aaudio_sys::AAUDIO_OK {
                return Err(DylError::Audio(format!(
                    "AAudio_createStreamBuilder failed: {}",
                    result
                )));
            }

            aaudio_sys::AAudioStreamBuilder_setDirection(
                builder,
                aaudio_sys::AAUDIO_DIRECTION_OUTPUT,
            );
            aaudio_sys::AAudioStreamBuilder_setSampleRate(builder, sample_rate as i32);
            aaudio_sys::AAudioStreamBuilder_setChannelCount(builder, channels as i32);
            aaudio_sys::AAudioStreamBuilder_setFormat(builder, aaudio_sys::AAUDIO_FORMAT_PCM_FLOAT);
            aaudio_sys::AAudioStreamBuilder_setPerformanceMode(
                builder,
                aaudio_sys::AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
            );
            aaudio_sys::AAudioStreamBuilder_setSharingMode(
                builder,
                aaudio_sys::AAUDIO_SHARING_MODE_SHARED,
            );

            let mut stream: *mut aaudio_sys::AAudioStream = std::ptr::null_mut();
            let result = aaudio_sys::AAudioStreamBuilder_openStream(builder, &mut stream);

            // 清理 builder（无论成功失败）
            aaudio_sys::AAudioStreamBuilder_delete(builder);

            if result != aaudio_sys::AAUDIO_OK || stream.is_null() {
                return Err(DylError::Audio(format!(
                    "AAudioStreamBuilder_openStream failed: {}",
                    result
                )));
            }

            // 获取实际采样率（可能与请求的不同）
            let actual_rate = aaudio_sys::AAudioStream_getSampleRate(stream);
            if actual_rate > 0 {
                self.device_rate = actual_rate as u32;
                tracing::info!(
                    "AaudioOutput: actual sample rate = {}Hz (requested {}Hz)",
                    actual_rate,
                    sample_rate
                );
            }

            // 单声道流在部分设备上会被 AAudio 自行改开立体声（openStream 返回
            // -889 后 MONO → STEREO 重试）。写入按流的真实声道排布，否则
            // AAudioStream_write 会按立体声解释单声道缓冲，多读出一倍采样。
            let actual_channels = aaudio_sys::AAudioStream_getChannelCount(stream);
            if actual_channels > 0 {
                self.device_channels = actual_channels as u32;
                if actual_channels as u32 != channels {
                    tracing::info!(
                        "AaudioOutput: actual channel count = {} (requested {})",
                        actual_channels,
                        channels
                    );
                }
            }

            // 启动流
            let result = aaudio_sys::AAudioStream_requestStart(stream);
            if result != aaudio_sys::AAUDIO_OK {
                aaudio_sys::AAudioStream_close(stream);
                return Err(DylError::Audio(format!(
                    "AAudioStream_requestStart failed: {}",
                    result
                )));
            }

            self.stream = Some(stream);
            tracing::info!("AaudioOutput: stream started");
        }

        Ok(())
    }

    fn write(&mut self, frame: &AudioFrame) -> Result<()> {
        let mut buf = self.buffer.lock();

        let src_ch = self.channels.max(1) as usize;
        let out_ch = self.device_channels.max(1) as usize;
        let src_frames = frame.samples.len() / src_ch;
        let resample = self.device_rate > 0 && self.device_rate != self.sample_rate;
        let out_frames = if resample {
            (src_frames as f64 * self.device_rate as f64 / self.sample_rate as f64).ceil() as usize
        } else {
            src_frames
        };

        // 推入采样到本地缓冲：重采样（线性插值）与声道映射一起做，
        // 缓冲里始终是设备格式的采样
        for i in 0..out_frames {
            let (src_idx, frac) = if resample {
                let src_pos = i as f64 * self.sample_rate as f64 / self.device_rate as f64;
                let idx = src_pos.floor() as usize;
                (idx, (src_pos - idx as f64) as f32)
            } else {
                (i, 0.0)
            };

            for ch in 0..out_ch {
                // 源声道不足时复制最后一个声道（单声道 → 立体声即左右双写）
                let source_ch = ch.min(src_ch - 1);
                let s0 = frame
                    .samples
                    .get(src_idx * src_ch + source_ch)
                    .copied()
                    .unwrap_or(0.0);
                let s = if frac > 0.0 {
                    let s1 = frame
                        .samples
                        .get((src_idx + 1) * src_ch + source_ch)
                        .copied()
                        .unwrap_or(0.0);
                    s0 + (s1 - s0) * frac
                } else {
                    s0
                };
                buf.push_back(s);
            }
        }

        // Ring buffer 上限保护
        while buf.len() > MAX_BUFFER_SAMPLES {
            buf.pop_front();
        }

        // 从 buffer 写入 AAudio stream：
        // 带超时写入并只消费成功写入的部分，未写完的采样留在 buffer 等下轮，
        // 避免旧实现"全量 pop + timeout=0 非阻塞写"造成的数据丢弃与周期性静音。
        // 必须在一次调用里把装得下的都写进去：解码帧（FLAC 4096 采样/帧）远大于
        // 一次 burst，只写 burst*4 会让设备长期欠载，最终被 AudioFlinger 以
        // BUFFER TIMEOUT 移出活跃列表而彻底停摆。
        if let Some(stream) = self.stream {
            let vol = f32::from_bits(self.volume.load(std::sync::atomic::Ordering::Relaxed));

            let burst =
                unsafe { aaudio_sys::AAudioStream_getFramesPerBurst(stream) }.max(1) as usize;
            let max_frames = burst * 4;

            loop {
                let writable_frames = (buf.len() / out_ch).min(max_frames);
                if writable_frames == 0 {
                    break;
                }
                let sample_count = writable_frames * out_ch;
                let mut scratch: Vec<f32> = Vec::with_capacity(sample_count);
                for s in buf.iter().take(sample_count) {
                    scratch.push(*s * vol);
                }

                // 100ms 超时：阻塞至写完或超时，partial write 由残余保留兜底
                let written = unsafe {
                    aaudio_sys::AAudioStream_write(
                        stream,
                        scratch.as_ptr() as *const std::ffi::c_void,
                        writable_frames as i32,
                        100_000_000,
                    )
                };
                if written > 0 {
                    let frames = (written as usize).min(writable_frames);
                    for _ in 0..frames * out_ch {
                        buf.pop_front();
                    }
                    // 只写了一部分说明设备已满，剩下的留到下一轮
                    if frames < writable_frames {
                        break;
                    }
                } else {
                    if written < 0 {
                        tracing::warn!("AaudioOutput: AAudioStream_write error: {written}");
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    fn set_volume(&mut self, volume: f32) {
        let v = volume.clamp(0.0, 1.0);
        self.volume
            .store(v.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// 本地 ring buffer 装得下这一帧（重采样、按设备声道布局后）吗
    fn accepts(&self, frame: &AudioFrame) -> bool {
        let src_ch = self.channels.max(1) as usize;
        let out_ch = self.device_channels.max(1) as usize;
        let src_frames = frame.samples.len() / src_ch;
        let out_frames =
            if self.sample_rate > 0 && self.device_rate > 0 && self.device_rate != self.sample_rate
            {
                (src_frames as f64 * (self.device_rate as f64 / self.sample_rate as f64)).ceil()
                    as usize
            } else {
                src_frames
            };
        self.buffer.lock().len() + out_frames * out_ch <= MAX_BUFFER_SAMPLES
    }

    /// 冻结设备消费：不暂停的话缓冲里的旧采样会在"已暂停"期间继续放完
    fn pause(&mut self) -> Result<()> {
        if let Some(stream) = self.stream {
            let result = unsafe { aaudio_sys::AAudioStream_requestPause(stream) };
            if result != aaudio_sys::AAUDIO_OK {
                return Err(DylError::Audio(format!(
                    "AAudioStream_requestPause failed: {result}"
                )));
            }
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if let Some(stream) = self.stream {
            let result = unsafe { aaudio_sys::AAudioStream_requestStart(stream) };
            if result != aaudio_sys::AAUDIO_OK {
                return Err(DylError::Audio(format!(
                    "AAudioStream_requestStart failed: {result}"
                )));
            }
        }
        Ok(())
    }

    /// 丢弃未播采样（seek / flush 语义）
    ///
    /// AAudio 没有独立的 flush 入口：`requestStop` 会同步停流并丢弃设备
    /// 内部队列里尚未播放的采样，随后 `requestStart` 复位供继续使用。
    /// 本地 ring buffer 同样要清空——两层里存的都是旧位置的采样。
    fn flush(&mut self) {
        self.buffer.lock().clear();
        if let Some(stream) = self.stream {
            unsafe {
                let stop = aaudio_sys::AAudioStream_requestStop(stream);
                if stop != aaudio_sys::AAUDIO_OK {
                    tracing::warn!("AaudioOutput: flush requestStop failed: {stop}");
                }
                let start = aaudio_sys::AAudioStream_requestStart(stream);
                if start != aaudio_sys::AAUDIO_OK {
                    tracing::warn!("AaudioOutput: flush requestStart failed: {start}");
                }
            }
        }
    }

    /// EOF 排空判定：本地缓冲是否还有未写入设备的采样
    fn has_pending_audio(&self) -> bool {
        !self.buffer.lock().is_empty()
    }
}

#[cfg(target_os = "android")]
impl Drop for AaudioOutput {
    fn drop(&mut self) {
        tracing::info!("AaudioOutput::drop");
        if let Some(stream) = self.stream.take() {
            unsafe {
                aaudio_sys::AAudioStream_requestStop(stream);
                aaudio_sys::AAudioStream_close(stream);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "android")]
    use super::*;

    #[test]
    #[cfg(target_os = "android")]
    fn test_aaudio_output_new() {
        let output = AaudioOutput::new();
        assert!((output.volume() - 1.0).abs() < 0.001);
        assert_eq!(output.sample_rate, 0);
        assert!(output.stream.is_none());
    }

    #[test]
    #[cfg(target_os = "android")]
    fn test_aaudio_set_volume() {
        let mut output = AaudioOutput::new();
        output.set_volume(0.5);
        assert!((output.volume() - 0.5).abs() < 0.001);
        output.set_volume(1.5);
        assert!((output.volume() - 1.0).abs() < 0.001);
    }
}
