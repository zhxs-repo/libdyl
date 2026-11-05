use dyl_audio::{AudioClock, AudioClockObservation};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 视频同步决策
#[derive(Debug, Clone, Copy)]
pub enum VideoSyncDecision {
    /// 正常显示
    Display,
    /// 等待指定秒数后显示
    Wait(f64),
    /// 丢弃该帧（太落后）
    Drop,
}

/// 上一次通过门控的音频读数
#[derive(Debug, Clone, Copy)]
struct AcceptedAudio {
    pts: f64,
    epoch: u64,
}

/// 音频观测的门控阈值
#[derive(Debug, Clone, Copy)]
struct GateConfig {
    /// 读数超过此时长未推进即视为设备停摆，不再采信
    max_age: Duration,
    /// 倒退超过此幅度才算回退；更小的是 backlog 记账的正常抖动
    regression_tolerance: f64,
    /// 地板余量：ring buffer 最多积压约 1 秒未播采样，暂停/恢复后扬声器位置
    /// 天然落后最后上屏帧这么多，是事实而不是异常，不能被地板拒掉
    floor_slack: f64,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            max_age: Duration::from_millis(500),
            regression_tolerance: 0.05,
            floor_slack: 1.0,
        }
    }
}

/// 读数被拒绝的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectReason {
    /// 未在播放（暂停 / 加载完成前）
    NotPlaying,
    /// 属于更早的 seek generation（旧位置的残留采样）
    Generation,
    /// 低于当前时间基准（旧位置的采样跨越了 seek）
    BelowFloor,
    /// 时钟停止推进超过时效上限（设备停摆）
    Stale,
    /// 相对上一次通过的读数倒退
    Regressed,
}

/// 门控结论
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// 采纳，并作为新基准（首个读数或新输出 epoch 的首个读数）
    Baseline,
    /// 采纳，可继续与上一次读数比较
    Advance,
    /// 拒绝
    Reject(RejectReason),
}

/// 六重有效性门控：state / generation / 地板 / epoch / 时效 / 单调
///
/// 音频读数由写入线程与设备回调在别处推进，任何一路迟到、回退或跨 flush 混用
/// 都会把主时钟猛推一下，画面随之跳或连续丢帧。判定写成纯函数以便逐条覆盖。
fn gate_observation(
    obs: &AudioClockObservation,
    last: Option<AcceptedAudio>,
    playing: bool,
    generation: u64,
    floor: f64,
    config: &GateConfig,
) -> Verdict {
    if !playing {
        return Verdict::Reject(RejectReason::NotPlaying);
    }
    if obs.generation != generation {
        return Verdict::Reject(RejectReason::Generation);
    }
    if obs.pts + config.floor_slack < floor {
        return Verdict::Reject(RejectReason::BelowFloor);
    }
    let Some(last) = last else {
        return Verdict::Baseline;
    };
    // 新输出 epoch（flush / 重建输出）：这段播放与之前互不相干，先建基线，
    // 不与旧 epoch 比单调性，也不受旧读数的停摆判定牵连
    if obs.epoch != last.epoch {
        return Verdict::Baseline;
    }
    if obs.age >= config.max_age {
        return Verdict::Reject(RejectReason::Stale);
    }
    if obs.pts < last.pts - config.regression_tolerance {
        return Verdict::Reject(RejectReason::Regressed);
    }
    Verdict::Advance
}

/// 时钟内部状态 — 单一 Mutex 保护，避免多锁死锁
struct ClockState {
    start_instant: Option<Instant>,
    base_pts: f64,
    video_clock: f64,
    consecutive_drops: u32,
    /// 主时钟地板：seek 目标或起播基准，比它低出 `floor_slack` 的读数不予采信
    floor_pts: f64,
    /// 最后一次通过门控的音频读数
    accepted: Option<AcceptedAudio>,
    /// 最后一次被拒绝的原因（连续相同不重复记日志）
    last_reject: Option<RejectReason>,
}

/// AV 同步器 — 主时钟优先取音频时钟，无音频时回退挂钟
///
/// **音频主时钟**：`AudioClock::position()` = 最近写入帧的结束 PTS −
/// 设备尚未播完的缓冲时长，即"扬声器现在放到第几秒"。解码卡顿、输出欠载时
/// 它会停住，视频随之等待而不是追着挂钟丢帧。
///
/// **六重门控**：音频读数必须通过 [`gate_observation`] 才推动主时钟；被拒绝时
/// 主时钟停在最后一次通过的读数上，既不回到挂钟也不跟着坏读数跳。
///
/// **挂钟回退**：主时钟 = `Instant::now() - start_instant + base_pts`。
/// 无音频轨（或首帧音频尚未写入 / seek 后已 flush）时使用，保证以真实时间推进。
///
/// 视频帧 PTS 与主时钟对比，决定显示/等待/丢弃。
///
/// **防丢帧死亡螺旋**：当连续丢帧超过阈值时自动打破 —— 挂钟模式重新同步到当前
/// 视频 PTS，音频模式则强制显示一帧（音频时钟是客观事实，不能由视频去改写）。
pub struct AVSync {
    state: Mutex<ClockState>,
    /// 音频主时钟句柄（由 pipeline 在启动音频输出后注入）
    audio_clock: Mutex<Option<AudioClock>>,
    /// 门控的 state / generation 两轴输入（pipeline 的 paused、seek_generation）
    clock_context: Mutex<Option<ClockContext>>,
    gate: GateConfig,
    /// 最大同步阈值（秒），视频落后超过此值则丢帧
    max_sync_threshold: f64,
    /// 连续丢帧上限，超过后强制重新同步
    max_consecutive_drops: u32,
    /// 主时钟与视频 PTS 最大允许漂移（秒），超过则重新同步
    max_drift_secs: f64,
}

/// 门控要观察的 pipeline 共享状态
#[derive(Clone)]
struct ClockContext {
    paused: Arc<AtomicBool>,
    seek_generation: Arc<AtomicU64>,
}

impl AVSync {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ClockState {
                start_instant: None,
                base_pts: 0.0,
                video_clock: 0.0,
                consecutive_drops: 0,
                floor_pts: 0.0,
                accepted: None,
                last_reject: None,
            }),
            audio_clock: Mutex::new(None),
            clock_context: Mutex::new(None),
            gate: GateConfig::default(),
            max_sync_threshold: 0.1,
            max_consecutive_drops: 5,
            max_drift_secs: 2.0,
        }
    }

    /// 注入门控上下文；不调用时按"正在播放、generation 0"判定（单测与无
    /// pipeline 的用法保持原行为）
    pub fn set_clock_context(&self, paused: Arc<AtomicBool>, seek_generation: Arc<AtomicU64>) {
        *self.clock_context.lock() = Some(ClockContext {
            paused,
            seek_generation,
        });
    }

    /// 门控输入：是否正在播放 + 当前 seek generation
    fn gate_inputs(&self) -> (bool, u64) {
        match self.clock_context.lock().as_ref() {
            Some(ctx) => (
                !ctx.paused.load(Ordering::Acquire),
                ctx.seek_generation.load(Ordering::Acquire),
            ),
            None => (true, 0),
        }
    }

    /// 注入音频主时钟；传 `None` 表示该次播放不使用音频时钟
    pub fn set_audio_clock(&self, clock: Option<AudioClock>) {
        *self.audio_clock.lock() = clock;
    }

    /// 通过六重门控的音频主时钟位置
    ///
    /// 读数被拒绝时返回最后一次通过的读数（时钟"停住"），一次都没通过时返回
    /// `None`，调用方回退挂钟。
    fn gated_audio(&self, state: &mut ClockState) -> Option<f64> {
        // 先取句柄并立即释放 audio_clock 锁，避免与 state 锁嵌套
        let clock = self.audio_clock.lock().clone()?;
        let obs = clock.observe()?;
        let (playing, generation) = self.gate_inputs();
        let verdict = gate_observation(
            &obs,
            state.accepted,
            playing,
            generation,
            state.floor_pts,
            &self.gate,
        );
        match verdict {
            Verdict::Baseline | Verdict::Advance => {
                if verdict == Verdict::Baseline {
                    tracing::info!(
                        "AVSync: audio clock baseline {:.3}s (epoch={}, gen={}, seq={})",
                        obs.pts,
                        obs.epoch,
                        obs.generation,
                        obs.sequence
                    );
                }
                state.accepted = Some(AcceptedAudio {
                    pts: obs.pts,
                    epoch: obs.epoch,
                });
                state.last_reject = None;
                Some(obs.pts)
            }
            Verdict::Reject(reason) => {
                if state.last_reject != Some(reason) {
                    tracing::debug!(
                        "AVSync: reject audio reading {reason:?} at {:.3}s (epoch={}, gen={}, seq={})",
                        obs.pts,
                        obs.epoch,
                        obs.generation,
                        obs.sequence
                    );
                }
                state.last_reject = Some(reason);
                state.accepted.map(|a| a.pts)
            }
        }
    }

    /// 挂钟读数（音频时钟不可用时的主时钟）
    fn wall_clock(state: &ClockState) -> f64 {
        match state.start_instant {
            Some(instant) => instant.elapsed().as_secs_f64() + state.base_pts,
            None => 0.0,
        }
    }

    /// 设置首帧时间基准 — 在收到第一个视频帧时调用
    pub fn set_first_frame_time(&self, pts: f64) {
        let mut state = self.state.lock();
        if state.start_instant.is_none() {
            state.start_instant = Some(Instant::now());
            state.base_pts = pts;
            state.floor_pts = pts;
            tracing::info!("AVSync: first frame pts={:.3}s", pts);
        }
    }

    /// 获取当前播放位置（秒）— 音频主时钟通过门控时以它为准
    pub fn master_clock(&self) -> f64 {
        let mut state = self.state.lock();
        match self.gated_audio(&mut state) {
            Some(pts) => pts,
            None => Self::wall_clock(&state),
        }
    }

    /// 视频帧同步决策
    ///
    /// 当视频落后于主时钟超过 `max_sync_threshold` 时返回 Drop。
    /// 但如果连续丢帧超过 `max_consecutive_drops`，强制显示一帧以打破死亡螺旋。
    pub fn video_sync_decision(&self, frame_pts: f64) -> VideoSyncDecision {
        let mut state = self.state.lock();
        // 只采信通过六重门控的音频读数
        let audio = self.gated_audio(&mut state);
        let audio_master = audio.is_some();

        if !audio_master && state.start_instant.is_none() {
            // seek/pause-resume 后（reset 将基准置 None）以首帧惰性重锚定，
            // 之后正常按挂钟推进，避免视频以解码速度狂奔
            state.start_instant = Some(Instant::now());
            state.base_pts = frame_pts;
            state.floor_pts = frame_pts;
            state.consecutive_drops = 0;
            return VideoSyncDecision::Display;
        }

        let clock = audio.unwrap_or_else(|| Self::wall_clock(&state));
        let diff = frame_pts - clock;

        if diff > 0.01 {
            // 帧比时钟超前 > 10ms → 等待
            state.consecutive_drops = 0;
            VideoSyncDecision::Wait(diff.min(0.05))
        } else if diff < -self.max_sync_threshold {
            // 帧落后超过阈值
            state.consecutive_drops += 1;

            // 防死亡螺旋：连续丢帧过多 或 漂移过大 → 重新同步
            if state.consecutive_drops >= self.max_consecutive_drops
                || (-diff) > self.max_drift_secs
            {
                tracing::warn!(
                    "AVSync: re-sync after {} consecutive drops, drift={:.3}s, \
                     frame_pts={:.3}s, clock={:.3}s (audio={})",
                    state.consecutive_drops,
                    -diff,
                    frame_pts,
                    clock,
                    audio_master
                );
                if !audio_master {
                    // 挂钟是自己维护的估计值，可以重设基准让它 ≈ frame_pts
                    state.start_instant = Some(Instant::now());
                    state.base_pts = frame_pts;
                }
                // 音频时钟是设备的实际进度，视频无权改写它；强制显示一帧即可
                state.consecutive_drops = 0;
                return VideoSyncDecision::Display;
            }

            VideoSyncDecision::Drop
        } else {
            // 正常范围
            state.consecutive_drops = 0;
            VideoSyncDecision::Display
        }
    }

    /// 更新视频时钟（诊断用）
    pub fn update_video_clock(&self, pts: f64) {
        self.state.lock().video_clock = pts;
    }

    /// 重置（seek / resume 后调用）
    pub fn reset(&self, seek_target: f64) {
        let mut state = self.state.lock();
        state.start_instant = None;
        state.base_pts = seek_target;
        state.video_clock = 0.0;
        state.consecutive_drops = 0;
        // 地板抬到新目标：旧位置的读数（flush 前入队的残留采样）不得推动主时钟
        state.floor_pts = seek_target;
        state.accepted = None;
        state.last_reject = None;
        tracing::info!("AVSync: reset to seek_target={:.3}s", seek_target);
    }

    /// 获取视频时钟（诊断）
    pub fn video_clock(&self) -> f64 {
        self.state.lock().video_clock
    }
}

impl Default for AVSync {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锚定在 `pts` 且无未播缓冲的音频时钟
    fn audio_at(pts: f64) -> AudioClock {
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.on_write(0, Some(pts));
        clock
    }

    /// 主时钟必须取音频位置，而不是挂钟
    #[test]
    fn audio_clock_takes_precedence_over_wall() {
        let sync = AVSync::new();
        sync.set_first_frame_time(0.0);
        std::thread::sleep(std::time::Duration::from_millis(120));
        // 挂钟已推进到 ~0.12s
        assert!(sync.master_clock() > 0.1);

        sync.set_audio_clock(Some(audio_at(7.5)));
        let clock = sync.master_clock();
        assert!(
            (clock - 7.5).abs() < 1e-9,
            "音频时钟生效时挂钟读数不得参与主时钟，got {clock}"
        );
    }

    /// 未注入音频时钟 / 时钟尚未 ready 时回退挂钟（无音频轨文件的现状）
    #[test]
    fn falls_back_to_wall_without_audio() {
        let sync = AVSync::new();
        sync.set_audio_clock(Some(AudioClock::new()));
        // 从未有过带 pts 的写入 → 无可读数，主时钟只能由挂钟推进
        sync.set_first_frame_time(2.0);
        assert!(sync.master_clock() >= 2.0);
    }

    /// 回归：音频欠载时主时钟停住，视频等待而不是追着挂钟狂奔
    ///
    /// 旧实现按挂钟推进，音频一卡就是持续丢帧追时钟（画面跳、音频仍静音）。
    #[test]
    fn stalled_audio_makes_video_wait() {
        let sync = AVSync::new();
        sync.set_audio_clock(Some(audio_at(1.0)));
        // 视频解码远快于播放：帧 PTS 在未来 → Wait，不显示
        assert!(matches!(
            sync.video_sync_decision(50.0),
            VideoSyncDecision::Wait(_)
        ));
        // 连续快速决策也不会变成 Drop：音频时钟没动
        assert!(matches!(
            sync.video_sync_decision(50.0),
            VideoSyncDecision::Wait(_)
        ));
    }

    /// 音频超前时视频落后 → Drop；但死亡螺旋保护不得改写音频时钟
    #[test]
    fn audio_master_drop_and_spiral_break() {
        let sync = AVSync::new();
        let clock = audio_at(1.0);
        sync.set_audio_clock(Some(clock.clone()));

        // 落后 0.15s（阈值 0.1）：前 4 次 Drop
        for _ in 0..4 {
            assert!(matches!(
                sync.video_sync_decision(0.85),
                VideoSyncDecision::Drop
            ));
        }
        // 第 5 次触发保护：强制显示，且音频位置不被视频"重锚定"
        assert!(matches!(
            sync.video_sync_decision(0.85),
            VideoSyncDecision::Display
        ));
        assert!(
            (clock.position().unwrap() - 1.0).abs() < 1e-9,
            "音频时钟是设备事实，视频不得把它改到 0.85"
        );
    }

    #[test]
    fn test_avsync_new() {
        let sync = AVSync::new();
        assert_eq!(sync.master_clock(), 0.0);
        assert_eq!(sync.video_clock(), 0.0);
    }

    #[test]
    fn test_set_first_frame_time() {
        let sync = AVSync::new();
        sync.set_first_frame_time(1.5);
        let clock = sync.master_clock();
        // 刚设置，时钟应接近 1.5
        assert!((clock - 1.5).abs() < 0.1);
    }

    #[test]
    fn test_video_sync_decision_display() {
        let sync = AVSync::new();
        sync.set_first_frame_time(0.0);
        // 帧 PTS 与当前时钟接近 → Display
        let decision = sync.video_sync_decision(0.0);
        assert!(matches!(decision, VideoSyncDecision::Display));
    }

    #[test]
    fn test_video_sync_decision_wait() {
        let sync = AVSync::new();
        sync.set_first_frame_time(0.0);
        // 帧 PTS 远在未来 → Wait
        let decision = sync.video_sync_decision(10.0);
        assert!(matches!(decision, VideoSyncDecision::Wait(_)));
    }

    #[test]
    fn test_video_sync_decision_drop() {
        let sync = AVSync::new();
        sync.set_first_frame_time(0.0);
        // 帧 PTS 远在过去 → Drop
        std::thread::sleep(std::time::Duration::from_millis(200));
        let decision = sync.video_sync_decision(0.0);
        assert!(matches!(decision, VideoSyncDecision::Drop));
    }

    #[test]
    fn test_reset() {
        let sync = AVSync::new();
        sync.set_first_frame_time(1.0);
        sync.update_video_clock(1.5);
        sync.reset(5.0);
        assert_eq!(sync.video_clock(), 0.0);
        // reset 后 start_instant 为 None，master_clock 应为 0
        assert_eq!(sync.master_clock(), 0.0);
    }

    #[test]
    fn test_reset_reanchors_and_paces() {
        let sync = AVSync::new();
        sync.set_first_frame_time(0.0);
        sync.reset(5.0);
        // 首帧重锚定并显示
        assert!(matches!(
            sync.video_sync_decision(5.0),
            VideoSyncDecision::Display
        ));
        // 之后的超前帧必须 Wait（回归：旧实现 reset 后无条件 Display）
        assert!(matches!(
            sync.video_sync_decision(15.0),
            VideoSyncDecision::Wait(_)
        ));
    }

    #[test]
    fn default_matches_new() {
        let a = AVSync::new();
        let b = AVSync::default();
        assert_eq!(a.master_clock(), b.master_clock());
        assert_eq!(a.video_clock(), b.video_clock());
    }

    #[test]
    fn display_decision_before_start() {
        let sync = AVSync::new();
        // 未设置首帧，master_clock == 0.0 → 始终 Display
        let decision = sync.video_sync_decision(5.0);
        assert!(matches!(decision, VideoSyncDecision::Display));
    }

    #[test]
    fn first_frame_only_set_once() {
        let sync = AVSync::new();
        sync.set_first_frame_time(1.0);
        sync.set_first_frame_time(99.0); // 第二次调用应被忽略
        let clock = sync.master_clock();
        // 应接近 1.0 而非 99.0
        assert!((clock - 1.0).abs() < 0.5);
    }

    #[test]
    fn update_video_clock_records_pts() {
        let sync = AVSync::new();
        sync.update_video_clock(2.71);
        assert!((sync.video_clock() - 2.71).abs() < f64::EPSILON);
    }

    #[test]
    fn consecutive_drops_trigger_resync() {
        let sync = AVSync::new();
        sync.set_first_frame_time(0.0);

        // 等待足够久让帧"落后"超过阈值
        std::thread::sleep(std::time::Duration::from_millis(300));

        // 连续发送落后帧，前 4 次应该 Drop
        for _ in 0..4 {
            let decision = sync.video_sync_decision(0.0);
            assert!(matches!(decision, VideoSyncDecision::Drop));
        }

        // 第 5 次触发 re-sync，应返回 Display（打破死亡螺旋）
        let decision = sync.video_sync_decision(0.0);
        assert!(
            matches!(decision, VideoSyncDecision::Display),
            "Expected Display after 5 consecutive drops, got {:?}",
            decision
        );
    }

    fn obs(pts: f64, epoch: u64, generation: u64, age_ms: u64) -> AudioClockObservation {
        AudioClockObservation {
            pts,
            sequence: 1,
            epoch,
            generation,
            age: Duration::from_millis(age_ms),
        }
    }

    /// 门控 1/2：未在播放、或读数属于别的 seek generation 时一律不采信
    #[test]
    fn gate_rejects_paused_and_foreign_generation() {
        let reading = obs(10.0, 0, 0, 0);
        assert_eq!(
            gate_observation(&reading, None, false, 0, 0.0, &GateConfig::default()),
            Verdict::Reject(RejectReason::NotPlaying)
        );
        assert_eq!(
            gate_observation(&reading, None, true, 1, 0.0, &GateConfig::default()),
            Verdict::Reject(RejectReason::Generation)
        );
    }

    /// 门控 6：跨越 seek 的旧位置读数被地板拦下，但缓冲积压量内的是事实
    #[test]
    fn gate_rejects_reading_far_below_floor() {
        let cfg = GateConfig::default();
        assert_eq!(
            gate_observation(&obs(5.0, 0, 0, 0), None, true, 0, 30.0, &cfg),
            Verdict::Reject(RejectReason::BelowFloor)
        );
        // 未播采样最多积压约 1 秒，低 0.8s 属于正常
        assert_eq!(
            gate_observation(&obs(29.2, 0, 0, 0), None, true, 0, 30.0, &cfg),
            Verdict::Baseline
        );
    }

    /// 门控 3：新输出 epoch 的读数先建基线，不与旧 epoch 比单调性
    #[test]
    fn gate_new_epoch_reestablishes_baseline() {
        let last = AcceptedAudio {
            pts: 10.0,
            epoch: 3,
        };
        assert_eq!(
            gate_observation(
                &obs(2.0, 4, 0, 0),
                Some(last),
                true,
                0,
                0.0,
                &GateConfig::default()
            ),
            Verdict::Baseline
        );
    }

    /// 门控 4/5：同 epoch 内停摆的读数与回退的读数都被拒绝，小抖动放行
    #[test]
    fn gate_rejects_stale_and_regressed_within_epoch() {
        let cfg = GateConfig::default();
        let last = AcceptedAudio {
            pts: 10.0,
            epoch: 3,
        };
        assert_eq!(
            gate_observation(&obs(10.1, 3, 0, 600), Some(last), true, 0, 0.0, &cfg),
            Verdict::Reject(RejectReason::Stale)
        );
        assert_eq!(
            gate_observation(&obs(9.5, 3, 0, 0), Some(last), true, 0, 0.0, &cfg),
            Verdict::Reject(RejectReason::Regressed)
        );
        assert_eq!(
            gate_observation(&obs(9.98, 3, 0, 0), Some(last), true, 0, 0.0, &cfg),
            Verdict::Advance
        );
    }

    /// 坏读数不得推动主时钟：被拒绝时停在最后一次通过的读数上
    #[test]
    fn rejected_reading_holds_master_clock() {
        let sync = AVSync::new();
        sync.set_first_frame_time(0.0);
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.on_write(0, Some(5.0));
        sync.set_audio_clock(Some(clock.clone()));
        assert!((sync.master_clock() - 5.0).abs() < 1e-9);

        // 同 epoch 内倒退 3s（迟到的旧批次）：主时钟保持 5.0，画面不能跟着回跳
        clock.on_write(0, Some(2.0));
        assert!(
            (sync.master_clock() - 5.0).abs() < 1e-9,
            "倒退的读数把主时钟拽回去了"
        );

        // flush 让 epoch 前进 → 新位置的读数作为基线被采纳
        clock.reset();
        clock.on_write(0, Some(20.0));
        assert!((sync.master_clock() - 20.0).abs() < 1e-9);
    }

    /// 暂停期间设备仍可能被系统取走采样，陈旧读数不得反映到主时钟
    #[test]
    fn paused_freezes_master_clock_at_last_accepted() {
        let sync = AVSync::new();
        let paused = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        sync.set_clock_context(paused.clone(), generation.clone());
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.on_write(0, Some(3.0));
        sync.set_audio_clock(Some(clock.clone()));
        assert!((sync.master_clock() - 3.0).abs() < 1e-9);

        paused.store(true, Ordering::Release);
        clock.on_write(0, Some(9.0));
        assert!(
            (sync.master_clock() - 3.0).abs() < 1e-9,
            "暂停期间的读数变化不应推进主时钟"
        );
    }

    /// seek 之后：旧 generation 的残留读数被拦下，新 generation 的读数照常采纳
    #[test]
    fn generation_gate_blocks_stale_writes() {
        let sync = AVSync::new();
        let paused = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        sync.set_clock_context(paused.clone(), generation.clone());
        let clock = AudioClock::new();
        clock.set_rate(1000);
        clock.set_generation(0);
        clock.on_write(0, Some(3.0));
        sync.set_audio_clock(Some(clock.clone()));
        assert!((sync.master_clock() - 3.0).abs() < 1e-9);

        // seek 到 60s：flush 前入队的 3s 读数既低于地板也不属于新 generation
        generation.store(1, Ordering::Release);
        sync.reset(60.0);
        assert!(
            sync.master_clock() < 59.0,
            "旧 generation 的残留读数不得推动主时钟"
        );

        clock.set_generation(1);
        clock.on_write(0, Some(60.2));
        assert!((sync.master_clock() - 60.2).abs() < 1e-9);
    }
}
