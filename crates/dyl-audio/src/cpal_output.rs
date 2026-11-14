use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::clock::AudioClock;
use crate::resample::AudioConverter;
use crate::traits::AudioOutput;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use dyl_core::{DylError, Result};
use dyl_ffmpeg::AudioFrame;
use parking_lot::Mutex;

/// cpal 音频输出实现
///
/// 使用共享 `VecDeque<f32>` 作为 ring buffer，
/// audio_output_loop 写入，cpal 回调读取。
/// ring buffer 有固定上限；写满时由 `accepts` 拦住调用方（背压），
/// 只有设备长时间不取采样时才丢最老的采样兜底。
///
/// 采样率转换与声道映射不在这里做：`write` 把源帧交给 [`AudioConverter`]，
/// 拿回设备格式的采样后原样推入 ring buffer。
pub struct CpalOutput {
    stream: Option<cpal::Stream>,
    buffer: Arc<Mutex<VecDeque<f32>>>,
    volume: Arc<std::sync::atomic::AtomicU32>,
    device_channels: usize,
    clock: AudioClock,
    speed: Arc<AtomicU32>,
    converter: AudioConverter,
    /// 转换结果的复用缓冲：音频路径上不允许逐帧分配
    scratch: Vec<f32>,
}

/// Ring buffer 容量上限：约 1 秒的立体声 48kHz 采样
const MAX_BUFFER_SAMPLES: usize = 48000 * 2;

// SAFETY: CpalOutput is only used from a dedicated audio thread.
// The cpal::Stream is not accessed concurrently.
unsafe impl Send for CpalOutput {}

impl CpalOutput {
    pub fn new() -> Self {
        Self {
            stream: None,
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            volume: Arc::new(std::sync::atomic::AtomicU32::new(1.0f32.to_bits())),
            device_channels: 0,
            clock: AudioClock::new(),
            speed: Arc::new(AtomicU32::new(1000)),
            // 未 init 前按 1:1 透传（单测直接写帧，不经过设备）
            converter: AudioConverter::new(0, 1, 0, 1),
            scratch: Vec::new(),
        }
    }

    /// 获取共享 buffer 引用（供 audio_output_loop 直接写入）
    pub fn buffer(&self) -> Arc<Mutex<VecDeque<f32>>> {
        self.buffer.clone()
    }

    /// 这一帧写入后会占用多少**设备帧**（含声道映射、重采样与变速）
    ///
    /// 与 `write` 实际写入的数严格一致：背压、主时钟和播放速度都靠它记账。
    fn device_frames_for(&self, frame: &AudioFrame) -> usize {
        let src_ch = self.converter.mix().in_ch().max(1);
        let src_frames = frame.samples.len() / src_ch;
        self.converter
            .frames_for(src_frames, self.speed.load(Ordering::Relaxed))
    }

    /// ring buffer 里每个设备帧占多少采样（未 init 时按源声道数）
    fn out_channels(&self, frame: &AudioFrame) -> usize {
        if self.device_channels == 0 {
            frame.channels.max(1) as usize
        } else {
            self.device_channels
        }
    }
}

impl Default for CpalOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioOutput for CpalOutput {
    fn init(&mut self, sample_rate: u32, channels: u32) -> Result<()> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| DylError::Audio("no default audio output device".into()))?;

        // 查询设备默认输出配置，使用设备支持的格式
        let supported_config = device
            .default_output_config()
            .map_err(|e| DylError::Audio(format!("get default output config: {e}")))?;

        let device_sample_rate = supported_config.sample_rate();
        let device_channels = supported_config.channels();
        let device_format = supported_config.sample_format();

        tracing::info!(
            "CpalOutput::init: source={}Hz {}ch, device={}Hz {}ch {:?}, device_name={}",
            sample_rate,
            channels,
            device_sample_rate,
            device_channels,
            device_format,
            device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_default()
        );

        self.device_channels = device_channels.max(1) as usize;
        // 重采样与声道映射都交给转换器：设备侧只关心"设备格式的采样"
        self.converter = AudioConverter::new(
            sample_rate,
            channels,
            device_sample_rate,
            self.device_channels as u32,
        );
        self.clock.set_rate(device_sample_rate);

        let config: cpal::StreamConfig = supported_config.into();
        let buffer = self.buffer.clone();
        let volume = self.volume.clone();
        // 设备每次取走的帧数记入主时钟：位置 = 写入终点 − 尚未播放的时长
        let out_channels = config.channels.max(1) as usize;
        let clock_f32 = self.clock.clone();
        let clock_i16 = self.clock.clone();

        // 根据设备支持的采样格式构建流
        let stream = match device_format {
            SampleFormat::F32 => device.build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let vol = f32::from_bits(volume.load(std::sync::atomic::Ordering::Relaxed));
                    let mut buf = buffer.lock();
                    for sample in data.iter_mut() {
                        *sample = buf.pop_front().unwrap_or(0.0) * vol;
                    }
                    clock_f32.on_consume((data.len() / out_channels) as u64);
                },
                move |err| tracing::error!("cpal output stream error: {}", err),
                None,
            ),
            SampleFormat::I16 => device.build_output_stream(
                config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let vol = f32::from_bits(volume.load(std::sync::atomic::Ordering::Relaxed));
                    let mut buf = buffer.lock();
                    for sample in data.iter_mut() {
                        let s = buf.pop_front().unwrap_or(0.0) * vol;
                        *sample = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                    clock_i16.on_consume((data.len() / out_channels) as u64);
                },
                move |err| tracing::error!("cpal output stream error: {}", err),
                None,
            ),
            _ => {
                return Err(DylError::Audio(format!(
                    "unsupported device sample format: {:?}",
                    device_format
                )));
            }
        }
        .map_err(|e| DylError::Audio(format!("build output stream: {e}")))?;

        stream
            .play()
            .map_err(|e| DylError::Audio(format!("play stream: {e}")))?;

        self.stream = Some(stream);
        Ok(())
    }

    fn write(&mut self, frame: &AudioFrame) -> Result<()> {
        let src_ch = self.converter.mix().in_ch().max(1);
        let src_frames = frame.samples.len() / src_ch;
        let dev_ch = self.converter.mix().out_ch();

        // 声道映射 + 重采样 + 限幅都收在转换器里。ring buffer 里始终是
        // **设备帧**的交错布局，否则设备回调取走的帧数与这里的记账不一致，
        // 主时钟和播放速度都会错乱。
        let pushed_device_frames =
            self.converter
                .convert(frame, self.speed.load(Ordering::Relaxed), &mut self.scratch);

        let mut buf = self.buffer.lock();
        buf.extend(self.scratch.iter().copied());

        // 兜底上限：调用方已用 `accepts` 做背压，正常不会走到这里。
        // 真走到（设备被系统挂起等）就丢最老的采样以限制内存占用，
        // 按整帧丢弃保持交错布局的声道对齐，并同步扣掉主时钟的未播计数
        let excess = buf.len().saturating_sub(MAX_BUFFER_SAMPLES);
        let dropped_samples = excess - excess % dev_ch;
        if dropped_samples > 0 {
            buf.drain(..dropped_samples);
            self.clock
                .on_overflow_drop((dropped_samples / dev_ch) as u64);
            tracing::warn!("CpalOutput: 缓冲超限，丢弃 {dropped_samples} 个采样");
        }
        drop(buf);

        // 主时钟：这批设备帧播完后到达的媒体位置。
        //
        // 重采样器会压着最后 `TAPS` 个源采样等右侧上下文（约 1.3 ms @48k），
        // 所以 `end_pts` 比实际排进缓冲的内容乐观一点点。这是一个**常量**
        // 偏移，不随播放时长累积。
        if pushed_device_frames > 0 {
            let end_pts = match (frame.pts_secs(), frame.sample_rate) {
                (Some(pts), rate) if rate > 0 => Some(pts + src_frames as f64 / rate as f64),
                _ => None,
            };
            self.clock.on_write(pushed_device_frames as u64, end_pts);
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

    /// 现在写入这一帧放得下吗？
    ///
    /// 供调用方做背压：缓冲快满时先不取新帧，而不是在 `write` 里丢采样。
    /// 丢采样会让音频一路冲到文件末尾，音频主时钟随之越过视频好几秒。
    fn accepts(&self, frame: &AudioFrame) -> bool {
        let needed = self.device_frames_for(frame) * self.out_channels(frame);
        self.buffer.lock().len() + needed <= MAX_BUFFER_SAMPLES
    }

    fn clock(&self) -> Option<AudioClock> {
        Some(self.clock.clone())
    }

    /// 暂停设备消费
    ///
    /// 不暂停的话，缓冲里最多 1 秒的采样会在"已暂停"期间继续放完，
    /// 音频主时钟也会跟着走完这段并不存在的内容。
    fn pause(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.as_mut() {
            stream
                .pause()
                .map_err(|e| DylError::Audio(format!("pause stream: {e}")))?;
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.as_mut() {
            stream
                .play()
                .map_err(|e| DylError::Audio(format!("play stream: {e}")))?;
        }
        Ok(())
    }

    fn flush(&mut self) {
        self.buffer.lock().clear();
        // 重采样器的历史必须一起清：seek 后带着旧位置的尾巴会让新位置
        // 开头混进几十毫秒的旧音频
        self.converter.reset();
        self.clock.reset();
    }

    fn has_pending_audio(&self) -> bool {
        !self.buffer.lock().is_empty()
    }

    fn set_speed_arc(&mut self, speed: Arc<AtomicU32>) {
        self.speed = speed.clone();
        self.clock.set_speed_arc(speed);
    }
}

impl Drop for CpalOutput {
    fn drop(&mut self) {
        tracing::info!("CpalOutput::drop");
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpal_output_new() {
        let output = CpalOutput::new();
        assert!((output.volume() - 1.0).abs() < 0.001);
        assert_eq!(output.device_channels, 0);
        assert!(output.stream.is_none());
    }

    #[test]
    fn test_set_volume() {
        let mut output = CpalOutput::new();
        output.set_volume(0.5);
        assert!((output.volume() - 0.5).abs() < 0.001);
        output.set_volume(1.5);
        assert!((output.volume() - 1.0).abs() < 0.001);
        output.set_volume(-0.1);
        assert!((output.volume() - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_apply_volume() {
        let mut samples = vec![0.5f32, 1.0, -0.5];
        crate::apply_volume(&mut samples, 0.5);
        assert!((samples[0] - 0.25).abs() < 0.001);
        assert!((samples[1] - 0.5).abs() < 0.001);
        assert!((samples[2] - (-0.25)).abs() < 0.001);
    }

    fn frame(pts_ticks: Option<i64>, samples: Vec<f32>) -> AudioFrame {
        AudioFrame {
            pts: pts_ticks,
            time_base: dyl_ffmpeg::time_base(1, 1000),
            sample_rate: 1000,
            channels: 1,
            format: dyl_ffmpeg::SampleFormat::F32,
            samples,
        }
    }

    /// 写入侧记账：结束 PTS、未播帧数与设备取走量共同决定主时钟读数
    ///
    /// `init` 需要真实输出设备，这里直接给时钟设采样率，只验证 write/flush 的换算。
    #[test]
    fn write_and_flush_drive_audio_clock() {
        let mut output = CpalOutput::new();
        let clock = output.clock().expect("cpal 输出必须提供主时钟");
        clock.set_rate(1000);

        // pts=2.0s + 1000 采样 @1kHz → 这批采样的结束位置 3.0s，尚未播放 1.0s
        output
            .write(&frame(Some(2000), vec![0.0; 1000]))
            .expect("write");
        assert!((clock.position().unwrap() - 2.0).abs() < 1e-9);

        // 设备取走一半 → 播放位置向 3.0 推进
        clock.on_consume(500);
        assert!((clock.position().unwrap() - 2.5).abs() < 1e-9);

        // flush（seek）：缓冲内容永远不会发声，时钟必须回到不可用状态
        output.flush();
        assert_eq!(clock.position(), None);
        assert!(output.buffer().lock().is_empty());
    }

    /// 缓冲已满时必须拦住新帧：靠丢弃补帧会让写入侧跑到设备前面
    #[test]
    fn accepts_blocks_when_buffer_full() {
        let mut output = CpalOutput::new();
        let big = frame(Some(0), vec![0.0; MAX_BUFFER_SAMPLES]);
        assert!(output.accepts(&big));
        output.write(&big).unwrap();
        assert!(
            !output.accepts(&frame(Some(5000), vec![0.0; 10])),
            "缓冲已满时必须拦住新帧"
        );
    }

    /// 源声道与设备声道不同（单声道内容接立体声设备）时，缓冲必须按设备帧存放：
    /// 否则设备回调把两个源采样算成一帧取走，主时钟与播放速度都会偏离一倍
    #[test]
    fn mono_frame_expands_to_stereo_device() {
        let mut output = CpalOutput::new();
        output.device_channels = 2;
        output.converter = AudioConverter::new(1000, 1, 1000, 2);
        let clock = output.clock().unwrap();
        clock.set_rate(1000);

        output
            .write(&frame(Some(0), vec![0.25; 512]))
            .expect("write");
        let buf: Vec<f32> = output.buffer().lock().iter().copied().collect();
        assert_eq!(buf.len() % 2, 0, "缓冲必须按 2 声道交错存放");
        assert!(
            buf.len() >= 800,
            "512 个源采样应产出约 512 个设备帧，got {}",
            buf.len() / 2
        );
        // 跳过重采样器的启动瞬态，逐对检查左右相同
        for pair in buf[200..].chunks(2) {
            assert!((pair[0] - pair[1]).abs() < 1e-6, "左右声道应一致：{pair:?}");
        }

        // 排空后主时钟读数必须正好落在这一批的结束位置
        let frames = buf.len() / 2;
        clock.on_consume(frames as u64);
        assert!((clock.position().unwrap() - 512.0 / 1000.0).abs() < 1e-9);
    }

    /// 5.1 只有中置有声（对白轨）时，立体声设备上必须听得到。
    ///
    /// 这是「某些片源没有对白」的回归：旧实现按 `c % src_ch` 取前两个声道。
    #[test]
    fn five_one_dialogue_reaches_stereo_device() {
        let mut output = CpalOutput::new();
        output.device_channels = 2;
        output.converter = AudioConverter::new(48000, 6, 48000, 2);

        let mut samples = vec![0.0f32; 1024 * 6];
        for i in 0..1024 {
            samples[i * 6 + 2] = 0.5; // FC
        }
        output.write(&frame(Some(0), samples)).unwrap();

        let buf: Vec<f32> = output.buffer().lock().iter().copied().collect();
        assert!(!buf.is_empty(), "必须推入设备帧");
        assert!(
            buf.iter().filter(|s| s.abs() > 0.1).count() > 1000,
            "中置人声必须出现在左右声道"
        );
    }

    /// 无时间戳的帧不得把主时钟拽回 0
    #[test]
    fn timestampless_frame_keeps_clock_anchor() {
        let mut output = CpalOutput::new();
        let clock = output.clock().unwrap();
        clock.set_rate(1000);
        output.write(&frame(Some(5000), vec![0.0; 500])).unwrap();
        let before = clock.position().unwrap();
        output.write(&frame(None, vec![0.0; 500])).unwrap();
        let after = clock.position().unwrap();
        assert!(
            after < before,
            "新增未播采样只推迟位置，基准 PTS 不得变成 0：{before} → {after}"
        );
        assert!(after > 4.0, "基准仍是 5.5s 结束位置，got {after}");
    }
}
