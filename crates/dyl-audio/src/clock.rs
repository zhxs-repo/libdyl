//! 音频主时钟 — 由输出设备的实际消费进度反推媒体位置
//!
//! 挂钟主时钟只按真实时间推进，与解码快慢、输出缓冲、设备延迟无关，因此一旦
//! 出现欠载或卡顿，它与视频 PTS 就永久错位：要么画面狂奔，要么持续丢帧追时钟。
//!
//! 音频主时钟直接回答"扬声器现在放到第几秒"：
//!
//! ```text
//! position = 最近写入帧的结束 PTS − ring buffer 里设备尚未取走的时长
//! ```
//!
//! 设备取不走时 backlog 归零，position 随之停住（那一刻放的是静音，不是内容），
//! 视频于是等待而不是丢帧 —— 这正是挂钟做不到的。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 音频主时钟的一次读数及其出处
///
/// 光有 `pts` 不足以判断它能不能用来推动主时钟：设备回调迟到、flush 之后残留
/// 的旧位置采样、seek 前入队的包都可能给出一个"看起来合法"的读数。读数必须
/// 连同它的批次序号、seek generation、输出 epoch 和推进时效一起交给消费方门控。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioClockObservation {
    /// 扬声器当前的媒体位置（秒）
    pub pts: f64,
    /// 产生该读数的写入批次序号（每次带 pts 的写入 +1）
    pub sequence: u64,
    /// 写入侧观察到的 seek generation
    pub generation: u64,
    /// 输出 epoch：每次 flush / 重建输出后 +1，跨 epoch 的读数不可比
    pub epoch: u64,
    /// 距时钟最后一次真正推进过了多久
    pub age: Duration,
}

/// 音频主时钟句柄 — 可克隆，写入侧、设备回调、AV 同步各持一份
#[derive(Clone)]
pub struct AudioClock {
    /// 设备尚未取走的帧数（写入 +1，回调 -1，下限 0）
    backlog: Arc<AtomicI64>,
    /// 最近一次写入帧的结束 PTS（媒体时间，秒），以 f64 bits 存放
    end_pts: Arc<AtomicU64>,
    /// 设备采样率，帧数换算时长用
    rate: Arc<AtomicU32>,
    /// 是否已写入过带时间戳的帧
    ready: Arc<AtomicBool>,
    /// 带 pts 的写入次数，用于识别读数是否来自新的批次
    sequence: Arc<AtomicU64>,
    /// 写入侧同步过来的 seek generation
    generation: Arc<AtomicU64>,
    /// `reset` 次数：区分 flush 前后的两段互不相干的播放
    epoch: Arc<AtomicU64>,
    /// 时效基准（构造时刻）与最后一次推进的毫秒偏移
    created: Arc<Instant>,
    last_move_ms: Arc<AtomicU64>,
}

impl Default for AudioClock {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioClock {
    pub fn new() -> Self {
        Self {
            backlog: Arc::new(AtomicI64::new(0)),
            end_pts: Arc::new(AtomicU64::new(0.0f64.to_bits())),
            rate: Arc::new(AtomicU32::new(0)),
            ready: Arc::new(AtomicBool::new(false)),
            sequence: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(0)),
            epoch: Arc::new(AtomicU64::new(0)),
            created: Arc::new(Instant::now()),
            last_move_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 设备实际采样率（`init` 时设置）
    pub fn set_rate(&self, rate: u32) {
        self.rate.store(rate, Ordering::Relaxed);
    }

    /// 写入侧标记当前 seek generation（与 `Pipeline::seek_generation` 同步）
    ///
    /// 读数带着它，消费方才能拒绝"flush 之前入队的旧位置采样"。
    pub fn set_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Release);
    }

    fn elapsed_ms(&self) -> u64 {
        self.created.elapsed().as_millis() as u64
    }

    /// 记录"时钟确实前进了"，供读数时效判定
    fn mark_moved(&self) {
        self.last_move_ms
            .store(self.elapsed_ms(), Ordering::Release);
    }

    /// 写入侧：向 ring buffer 推入 `frames` 个设备帧
    ///
    /// `end_pts` 为这批采样的**结束**媒体时间；无时间戳的帧传 `None`，
    /// 只增加 backlog 而不移动时钟基准。
    pub fn on_write(&self, frames: u64, end_pts: Option<f64>) {
        self.backlog.fetch_add(frames as i64, Ordering::AcqRel);
        if let Some(pts) = end_pts {
            self.end_pts.store(pts.to_bits(), Ordering::Release);
            self.ready.store(true, Ordering::Release);
            self.sequence.fetch_add(1, Ordering::Release);
            self.mark_moved();
        }
    }

    /// 设备回调取走了 `frames` 帧（欠载补零的那些也算）
    ///
    /// 停在 0 不再往下走：欠载时设备在放静音，不能让它把媒体时钟拖回过去。
    pub fn on_consume(&self, frames: u64) {
        if self.saturating_sub(frames) {
            self.mark_moved();
        }
    }

    /// ring buffer 溢出丢弃了最老的 `frames` 帧：这些采样再也不会被播放，
    /// 必须从 backlog 里扣掉，否则时钟会以为自己还在放已经丢掉的内容
    pub fn on_overflow_drop(&self, frames: u64) {
        if self.saturating_sub(frames) {
            self.mark_moved();
        }
    }

    /// 从 backlog 扣减，返回是否真的减掉了（已为 0 时什么都没发生）
    fn saturating_sub(&self, frames: u64) -> bool {
        self.backlog
            .try_update(Ordering::AcqRel, Ordering::Relaxed, |b| {
                Some((b - frames as i64).max(0))
            })
            .map(|before| before > 0)
            .unwrap_or(false)
    }

    /// 当前媒体位置（秒）。尚无带时间戳的音频写入时返回 `None`，
    /// 调用方据此回退到挂钟。
    pub fn position(&self) -> Option<f64> {
        if !self.ready.load(Ordering::Acquire) {
            return None;
        }
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return None;
        }
        let pts = f64::from_bits(self.end_pts.load(Ordering::Acquire));
        let backlog = self.backlog.load(Ordering::Acquire).max(0);
        Some(pts - backlog as f64 / rate as f64)
    }

    /// 清空状态（seek / stop 时与输出缓冲一起 flush）
    ///
    /// epoch 前进：flush 前后的两段播放互不相干，旧位置的读数不得和新位置比较
    /// 单调性，也不能被当成"时钟还在正常推进"。
    pub fn reset(&self) {
        self.backlog.store(0, Ordering::Release);
        self.ready.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::Release);
        self.mark_moved();
    }

    /// 带出处的读数 — 主时钟消费方据此做有效性门控
    ///
    /// 从未有过带时间戳的写入时返回 `None`（无音频轨 / flush 后尚未补写），
    /// 调用方据此回退挂钟。
    pub fn observe(&self) -> Option<AudioClockObservation> {
        let pts = self.position()?;
        Some(AudioClockObservation {
            pts,
            sequence: self.sequence.load(Ordering::Acquire),
            generation: self.generation.load(Ordering::Acquire),
            epoch: self.epoch.load(Ordering::Acquire),
            age: Duration::from_millis(
                self.elapsed_ms()
                    .saturating_sub(self.last_move_ms.load(Ordering::Acquire)),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_none_until_first_timestamped_write() {
        let clock = AudioClock::new();
        clock.set_rate(48000);
        assert_eq!(clock.position(), None);

        clock.on_write(4800, Some(1.0));
        assert!((clock.position().unwrap() - 0.9).abs() < 1e-9);
    }

    #[test]
    fn backlog_shrinks_as_device_consumes() {
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.on_write(1000, Some(10.0));
        assert!((clock.position().unwrap() - 9.0).abs() < 1e-9);

        clock.on_consume(400);
        assert!((clock.position().unwrap() - 9.4).abs() < 1e-9);

        clock.on_consume(600);
        assert!((clock.position().unwrap() - 10.0).abs() < 1e-9);
    }

    /// 欠载：设备取走的比写入的多时停在 0，时钟不得倒退越过已写入的位置
    #[test]
    fn underflow_freezes_clock_at_written_position() {
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.on_write(100, Some(5.0));
        clock.on_consume(100);
        assert!((clock.position().unwrap() - 5.0).abs() < 1e-9);

        // 设备继续要求采样但已无内容：位置保持，不由挂钟推着走
        clock.on_consume(500);
        assert!((clock.position().unwrap() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn overflow_drop_reduces_backlog() {
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.on_write(1000, Some(10.0));
        clock.on_overflow_drop(600);
        assert!((clock.position().unwrap() - 9.6).abs() < 1e-9);
    }

    /// 无时间戳的帧只堆积，不移动基准
    #[test]
    fn timestampless_write_keeps_baseline() {
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.on_write(500, Some(3.0));
        clock.on_write(500, None);
        // 无 pts 的写入只堆积 backlog：基准仍是 3.0，未播时长变成 1.0s
        assert!((clock.position().unwrap() - 2.0).abs() < 1e-9);

        let fresh = AudioClock::new();
        fresh.set_rate(1000);
        fresh.on_write(500, None);
        assert_eq!(fresh.position(), None, "从未有过带 pts 的写入时不可用");
    }

    #[test]
    fn reset_requires_new_anchor() {
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.on_write(100, Some(7.0));
        clock.reset();
        assert_eq!(clock.position(), None);
        clock.on_write(50, Some(2.0));
        assert!((clock.position().unwrap() - 1.95).abs() < 1e-9);
    }

    /// 句柄克隆后共享同一份状态（写入侧 / 回调 / AVSync 各持一份）
    #[test]
    fn clones_share_state() {
        let clock = AudioClock::new();
        clock.set_rate(1000);
        let writer = clock.clone();
        let reader = clock.clone();
        writer.on_write(1000, Some(4.0));
        assert!((reader.position().unwrap() - 3.0).abs() < 1e-9);
        reader.on_consume(1000);
        assert!((clock.position().unwrap() - 4.0).abs() < 1e-9);
    }

    /// 读数必须带出处，否则消费侧无法拒绝迟到的回调与旧位置的残留
    #[test]
    fn observation_carries_provenance() {
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.set_generation(3);
        clock.on_write(100, Some(5.0));
        let obs = clock.observe().expect("有带 pts 的写入后必须能读数");
        assert!((obs.pts - 4.9).abs() < 1e-9);
        assert_eq!(obs.sequence, 1);
        assert_eq!(obs.generation, 3);
        assert_eq!(obs.epoch, 0);
        assert!(obs.age < Duration::from_millis(500));

        // flush（seek）：读数失效，且之后的读数带着新 epoch
        clock.reset();
        assert_eq!(clock.observe(), None);
        clock.on_write(50, Some(20.0));
        let obs = clock.observe().unwrap();
        assert_eq!(obs.epoch, 1, "flush 之后的读数必须带着新 epoch");
        assert_eq!(obs.sequence, 2);
    }

    /// 时钟停止推进时时效必须增长，消费侧据此判设备停摆
    #[test]
    fn age_grows_when_clock_stalls() {
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.on_write(100, Some(1.0));
        clock.on_consume(100);
        std::thread::sleep(Duration::from_millis(30));
        assert!(clock.observe().unwrap().age >= Duration::from_millis(20));
        // 欠载后的取用没减掉任何帧，不得被记成"时钟又前进了"
        clock.on_consume(100);
        assert!(clock.observe().unwrap().age >= Duration::from_millis(20));
    }
}
