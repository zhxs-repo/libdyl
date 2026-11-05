pub mod clock;
pub mod traits;

#[cfg(not(target_os = "android"))]
pub mod cpal_output;

#[cfg(target_os = "android")]
pub mod aaudio_output;

pub use clock::{AudioClock, AudioClockObservation};
pub use traits::AudioOutput;

#[cfg(not(target_os = "android"))]
pub use cpal_output::CpalOutput;

#[cfg(target_os = "android")]
pub use aaudio_output::AaudioOutput;

/// 根据平台选择默认音频输出
///
/// - Android: 优先使用 AAudio（低延迟）
/// - 其他平台: 使用 cpal
pub fn create_default_output() -> Box<dyn AudioOutput> {
    #[cfg(target_os = "android")]
    {
        Box::new(AaudioOutput::new())
    }
    #[cfg(not(target_os = "android"))]
    {
        Box::new(CpalOutput::new())
    }
}

/// 应用音量（线性）到 f32 采样
pub fn apply_volume(samples: &mut [f32], volume: f32) {
    let vol = volume.clamp(0.0, 1.0);
    for s in samples.iter_mut() {
        *s *= vol;
    }
}
