//! 无设备音频输出 — headless / CI 等无音频设备环境的降级实现
//!
//! `init` 永远成功，`write` 丢弃采样，`clock` 返回 `None` 让 AVSync 回退挂钟。
//! 当 `CpalOutput::init` 因找不到设备失败时，`Pipeline::start` 换成此实现，
//! 视频仍能播放，load_file 不会因环境缺音频设备而失败。

use dyl_core::Result;
use dyl_ffmpeg::AudioFrame;

use crate::traits::AudioOutput;

pub struct NullAudioOutput {
    volume: f32,
}

impl NullAudioOutput {
    pub fn new() -> Self {
        Self { volume: 1.0 }
    }
}

impl Default for NullAudioOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioOutput for NullAudioOutput {
    fn init(&mut self, _sample_rate: u32, _channels: u32) -> Result<()> {
        Ok(())
    }

    fn write(&mut self, _frame: &AudioFrame) -> Result<()> {
        Ok(())
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    fn volume(&self) -> f32 {
        self.volume
    }
}