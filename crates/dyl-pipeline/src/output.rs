use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dyl_audio::AudioOutput;
use dyl_render::OutputSlot;

use crate::avsync::VideoSyncDecision;
use crate::event::PipelineEvent;
use crate::pipeline::Pipeline;
use crate::seek::{recv_current, Stamped};

/// Fallback idle limit when no video frames have been displayed (audio-only / zero-frame)
const END_IDLE_LIMIT: u32 = 20; // ~1s at 50ms/cycle

/// 看门狗预算 · 音频输出：设备连续这么久不接手任何采样即视为停摆
///
/// 10 s 而不是更短：USB/蓝牙设备重连、系统休眠唤醒都会短暂停止消费，
/// 这类恢复不该被判成错误。
///
/// 数的是**正在播放**的时长：暂停期间设备被主动冻结、时钟本就不推进，
/// 那段不能算进预算（见 `audio_output_loop` 里的 `playing_since`）。
const OUTPUT_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// 帧已尽（取帧超时或 rx 已关闭）时的播放结束判定，返回 `true` 表示循环应退出
///
/// demux 尚未完成时返回 `false`：帧只是暂时没到，继续等下一个周期。
/// 主时钟还没走到片长说明设备里仍有内容要放（`eof_due` 以它为准），
/// 此时靠 `END_IDLE_LIMIT` 兜底，避免设备停摆把线程永远卡在结束判定上。
///
/// `idle_fallback` 只对视频轨成立：纯音频的包通道能装下几十秒，demux 往往在
/// 播放开始前就已完成，此刻"没有新帧"完全不代表播放结束。音频只认 `eof_due`，
/// 时钟失灵时由其中的挂钟判据接管。
fn no_more_frames(
    eof_due: bool,
    idle_count: &mut u32,
    pipeline: &Pipeline,
    idle_fallback: bool,
) -> bool {
    if !pipeline.demux_complete.load(Ordering::Acquire) {
        return false;
    }
    if !eof_due {
        if !idle_fallback {
            return false;
        }
        *idle_count += 1;
        if *idle_count < END_IDLE_LIMIT {
            return false;
        }
        tracing::info!("render_loop: idle limit reached, EOF");
    } else {
        tracing::info!("render_loop: EOF (media clock reached duration)");
    }
    // 置位"已播完"：此后所有工作线程都会退出，seek 请求没人消费。
    // 引擎靠它判断要不要重载文件才能响应"播完后往回拖"。
    pipeline.ended.store(true, Ordering::Release);
    pipeline.emit(PipelineEvent::Ended);
    true
}

/// exact seek 的 pre-roll 判定（视频）：PTS 早于目标即丢弃。
///
/// 1ms 容差吸收时间基换算的浮点误差，避免丢掉恰好落在目标上的帧。
fn is_pre_roll_frame(pts: f64, target: f64) -> bool {
    target > 0.0 && pts + 0.001 < target
}

/// exact seek 的 pre-roll 判定（音频）：整帧都在目标之前才丢。
///
/// 跨过目标边界的帧保留 —— 切掉它会丢掉目标处的采样，-21ms 的偏差
/// 比静音破音更可接受。
fn is_pre_roll_audio(frame: &dyl_ffmpeg::AudioFrame, target: f64) -> bool {
    if target <= 0.0 {
        return false;
    }
    let Some(pts) = frame.pts_secs() else {
        return false;
    };
    let duration = if frame.sample_rate > 0 && frame.channels > 0 {
        frame.samples.len() as f64 / (frame.channels as f64 * frame.sample_rate as f64)
    } else {
        0.0
    };
    pts + duration <= target + 0.001
}

/// Video render loop — 从 video_frame_rx 接收帧，经 AV 同步后提交到 VideoOutput
pub fn render_loop(
    pipeline: Arc<Pipeline>,
    output: Arc<OutputSlot>,
    video_frame_rx: crossbeam_channel::Receiver<Stamped<dyl_ffmpeg::VideoFrame>>,
    duration_secs: f64,
    has_video: bool,
) -> dyl_core::Result<()> {
    tracing::info!(
        "render_loop started, duration={:.2}s, has_video={}",
        duration_secs,
        has_video
    );

    // 累计的"正在播放"时长（不含暂停），作为 EOF 的挂钟兜底
    let mut playing = Duration::ZERO;
    let mut play_tick = std::time::Instant::now();
    let duration = Duration::from_secs_f64(duration_secs);
    let mut idle_count: u32 = 0;
    let mut first_frame = true;
    let mut rendered_frames: u64 = 0;
    let mut dropped_frames: u64 = 0;

    'outer: loop {
        if pipeline.shutdown.load(Ordering::Acquire) {
            tracing::info!("render_loop: shutdown");
            break;
        }

        // 暂停时冻结叠加层挂钟：渲染线程会在两个视频帧之间按挂钟外推叠加层时间，
        // 不冻结就会出现"画面静止、弹幕继续滑动"
        if pipeline.is_paused() {
            output.freeze_overlay_clock();
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }

        // EOF 截止只累计"正在播放"的时间：暂停期间画面本就静止，挂钟若继续走，
        // 暂停久了回来会被直接判定为播放结束
        //
        // 纯音频没有首帧可锚：线程一开始跑就算播放已开始，否则时钟一旦失灵就
        // 再也等不到 EOF
        let started = !first_frame || !has_video;
        if started {
            playing += play_tick.elapsed();
        }
        play_tick = std::time::Instant::now();

        // EOF 以媒体主时钟为准：它是"扬声器现在放到第几秒"（position 也报它），
        // 按上屏帧 PTS 判结束会把设备里尚未放完的结尾音频连同线程一起切掉。
        // 挂钟仅在无音频时兜底，并留 2 秒余量防止设备停摆永远等不到
        let master_clock = pipeline.avsync.master_clock();
        let eof_due = started
            && ((duration_secs > 0.0 && master_clock + 0.05 >= duration_secs)
                || playing >= duration + Duration::from_secs(2));

        // 纯音频文件没有视频帧来写 position：主时钟就是当前位置。不补这一步，
        // 前端进度条会一直停在 0，暂停后 resume 也会把时钟重置回起点
        if !has_video {
            pipeline
                .position_us
                .store((master_clock * 1_000_000.0) as u64, Ordering::Release);
        }

        // 只接受当前 seek generation 的帧；seek 之前入队的残留帧直接丢弃，
        // 否则会把旧位置的画面/声音播出来，并把 position 往回拽
        match recv_current(
            &video_frame_rx,
            pipeline.seek_generation.load(Ordering::Acquire),
            Duration::from_millis(50),
        ) {
            Ok(stamped) => {
                let frame = stamped.item;
                idle_count = 0;

                // 无时间戳（NOPTS）帧：直接上屏，但不参与 AV 同步、不推进 position，
                // 否则会被当成 ts=0 的落后帧全部丢弃
                let Some(pts) = frame.pts_secs() else {
                    tracing::debug!("render_loop: frame without pts, displaying untimed");
                    let _ = output.submit(&frame);
                    continue;
                };

                // exact seek 从目标之前的关键帧解起：目标之前的帧是 pre-roll，
                // 丢弃它们，否则画面先跳回关键帧、进度条往回拽（契约规范 §seek）
                if is_pre_roll_frame(pts, pipeline.seek_target_secs()) {
                    continue;
                }

                // 首帧：设置 AVSync 基准
                if first_frame {
                    first_frame = false;
                    pipeline.avsync.set_first_frame_time(pts);
                    // 音频输出等这一帧再启动设备：音频主时钟先跑会让视频只能丢帧追赶
                    pipeline.mark_first_frame_ready();
                    play_tick = std::time::Instant::now();
                }

                match pipeline.avsync.video_sync_decision(pts) {
                    VideoSyncDecision::Display => {
                        rendered_frames += 1;
                        if rendered_frames.is_multiple_of(30) {
                            tracing::info!("render_loop: rendered {} frames", rendered_frames);
                        }
                        // position 报主时钟（正在听到的位置），不是上屏帧 PTS：
                        // 视频帧可领先设备里未播完的音频最多约 1 秒，按帧 PTS 报
                        // 会让进度条冲到声音前面
                        pipeline.position_us.store(
                            (pipeline.avsync.master_clock() * 1_000_000.0) as u64,
                            Ordering::Release,
                        );
                        let _ = output.submit(&frame);
                        pipeline.avsync.update_video_clock(pts);
                    }
                    VideoSyncDecision::Wait(mut secs) => {
                        // sleep 后重新决策：若主时钟已超前很多，该帧可能已过时
                        let mut attempts = 0;
                        loop {
                            std::thread::sleep(Duration::from_secs_f64(secs));
                            // 等待期间可能发生 seek（换轨/切解码档位后的恢复 seek 尤其
                            // 容易撞上）：这一帧已是旧位置的内容，再拿去决策只会被判成
                            // "落后太多"而强制上屏 —— 画面回跳 + `AVSync: re-sync` 告警。
                            // 入口处的 generation / pre-roll 两道闸门必须在这里重验
                            if stamped.generation < pipeline.seek_generation.load(Ordering::Acquire)
                                || is_pre_roll_frame(pts, pipeline.seek_target_secs())
                            {
                                continue 'outer;
                            }
                            match pipeline.avsync.video_sync_decision(pts) {
                                VideoSyncDecision::Display => break,
                                VideoSyncDecision::Wait(s) => {
                                    secs = s;
                                    attempts += 1;
                                    if attempts >= 3 {
                                        // 避免无限等待，强制显示
                                        break;
                                    }
                                }
                                VideoSyncDecision::Drop => {
                                    dropped_frames += 1;
                                    pipeline.position_us.store(
                                        (pipeline.avsync.master_clock() * 1_000_000.0) as u64,
                                        Ordering::Release,
                                    );
                                    // 跳过渲染，直接进入下一帧
                                    continue 'outer;
                                }
                            }
                        }
                        rendered_frames += 1;
                        pipeline.position_us.store(
                            (pipeline.avsync.master_clock() * 1_000_000.0) as u64,
                            Ordering::Release,
                        );
                        let _ = output.submit(&frame);
                        pipeline.avsync.update_video_clock(pts);
                    }
                    VideoSyncDecision::Drop => {
                        dropped_frames += 1;
                        if dropped_frames.is_multiple_of(30) {
                            tracing::warn!(
                                "render_loop: {} frames dropped (rendered={}, pts={:.3})",
                                dropped_frames,
                                rendered_frames,
                                pts
                            );
                        }
                        // 即使丢帧也更新 position，让前端进度条持续推进
                        pipeline.position_us.store(
                            (pipeline.avsync.master_clock() * 1_000_000.0) as u64,
                            Ordering::Release,
                        );
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if no_more_frames(eof_due, &mut idle_count, &pipeline, has_video) {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                tracing::debug!("video_frame_rx disconnected");
                // 解码线程结束不等于播放结束：设备里还排着最后一秒音频。转入与
                // "超时无帧"相同的判定，等主时钟走到片长再报 EOF
                std::thread::sleep(Duration::from_millis(50));
                if no_more_frames(eof_due, &mut idle_count, &pipeline, has_video) {
                    break;
                }
                if !has_video {
                    // 无视频轨时 tx 从建线起就没人持有，这里的"断开"不代表任何事，
                    // 只能继续等设备把音频放完
                    continue;
                }
                // rx 关闭且 demux 未完成 = 管线被拆（stop / 重载），正常退出
                break;
            }
        }
    }

    // EOF / shutdown 等所有退出路径都要冻结叠加层挂钟，
    // 否则渲染线程会继续按挂钟把弹幕推到超出最后一帧的位置
    output.freeze_overlay_clock();
    tracing::info!("render_loop exiting");
    Ok(())
}

/// Audio output loop — 从 audio_frame_rx 接收帧，写入 AudioOutput
/// 设备停摆时长：只数**正在播放**的那段，不足预算返回 `None`
///
/// 时钟读数的 `age` 是"距上次推进过了多久"，而暂停是主动冻结设备、时钟本就该停，
/// 所以整段暂停都在 `age` 里 —— 直接拿它比预算会把"暂停 30 秒后正常恢复"判成
/// 设备故障（`report_stall` 是致命错误，画面就此再也放不出来）。恢复时刻之前的
/// 那段必须扣除，`playing_since` 就是扣除的依据。
fn device_stall(
    reading_age: Duration,
    playing_since: Instant,
    budget: Duration,
) -> Option<Duration> {
    let stalled = playing_since.elapsed().min(reading_age);
    (stalled >= budget).then_some(stalled)
}

/// 起播对齐等待上限：等首帧视频超过它就先放音频，宁可让视频追一次也别静音
const FIRST_FRAME_ALIGN_TIMEOUT: Duration = Duration::from_millis(1500);

pub fn audio_output_loop(
    pipeline: Arc<Pipeline>,
    mut audio_output: Box<dyn AudioOutput>,
    audio_frame_rx: crossbeam_channel::Receiver<Stamped<dyl_ffmpeg::AudioFrame>>,
    has_video: bool,
) -> dyl_core::Result<()> {
    tracing::info!("audio_output_loop started");

    let mut device_paused = false;
    let mut align_start: Option<Instant> = None;
    let mut last_seek_gen = pipeline.seek_generation.load(Ordering::Acquire);
    let mut last_speed_gen = pipeline.speed_generation.load(Ordering::Acquire);
    // 主时钟读数要带着它属于哪一次 seek，视频线程才能拒绝 flush 前入队的旧位置采样
    let clock = audio_output.clock();
    if let Some(clock) = &clock {
        clock.set_generation(last_seek_gen);
    }
    // 设备一时装不下的那一帧留在这里，下一轮重试
    let mut pending: Option<dyl_ffmpeg::AudioFrame> = None;
    // 音频流读完（发送端关闭）后等设备把缓冲排空，见循环末尾
    let mut stream_eof = false;
    let mut drain_deadline: Option<std::time::Instant> = None;
    // 设备停摆一次只报一次，写入成功即重新武装
    let mut stall_reported = false;
    // 本轮"正在播放"的起点，恢复时重新计时
    let mut playing_since = Instant::now();

    loop {
        if pipeline.shutdown.load(Ordering::Acquire) {
            break;
        }

        // 暂停检查 — 同时冻结设备消费
        //
        // ring buffer 里最多积压 1 秒采样，不暂停 stream 的话这些内容会在
        // "已暂停"期间继续放完，音频主时钟也会跟着走完这段并不存在的时间
        if pipeline.is_paused() {
            if !device_paused {
                if let Err(e) = audio_output.pause() {
                    tracing::warn!("audio_output pause error: {}", e);
                }
                device_paused = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        if device_paused {
            if let Err(e) = audio_output.resume() {
                tracing::warn!("audio_output resume error: {}", e);
            }
            device_paused = false;
            // 停摆预算从恢复这一刻重新数起：暂停期间设备是被主动冻结的，
            // 时钟 age 里那段暂停不是设备故障
            playing_since = Instant::now();
        }

        // 起播对齐：解码器出首帧之前不启动音频设备。音频主时钟先跑到首帧
        // 前面时，视频只能靠连续丢帧追回（4K 硬解实测起播 2.5s 内 12 次
        // re-sync）；首帧迟迟不来（解码卡住）也不能一直静音，超时先放音频
        if has_video && !pipeline.first_frame_ready() {
            let started = *align_start.get_or_insert_with(Instant::now);
            if started.elapsed() < FIRST_FRAME_ALIGN_TIMEOUT {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
        }

        // seek：缓冲里剩下的是旧位置的音频，既会先响一小段，
        // 也会把音频主时钟锚在刚跳走的时间上
        let current_gen = pipeline.seek_generation.load(Ordering::Acquire);
        if current_gen != last_seek_gen {
            last_seek_gen = current_gen;
            audio_output.flush();
            if let Some(clock) = &clock {
                clock.set_generation(current_gen);
            }
            pending = None;
        }

        // 变速：缓冲里是按旧速率重采样过的 PCM，放完它们之前新速率听不见，
        // 主时钟也会继续按旧速率走。flush 掉旧采样（clock 随之 reset），
        // pending 是源采样率的原始帧，留着按新速率重采样后写入
        let current_speed_gen = pipeline.speed_generation.load(Ordering::Acquire);
        if current_speed_gen != last_speed_gen {
            last_speed_gen = current_speed_gen;
            audio_output.flush();
        }

        // 背压：设备没空间就攥着这一帧等它，既不丢采样（丢采样会让音频一路冲到
        // 文件末尾、主时钟越过视频好几秒），也不在 write 里长阻塞
        // （暂停 / seek / shutdown 必须能被及时看到）
        if pending.is_none() && !stream_eof {
            match recv_current(&audio_frame_rx, current_gen, Duration::from_millis(50)) {
                Ok(stamped) => pending = Some(stamped.item),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    tracing::debug!("audio_frame_rx disconnected, 等待设备排空缓冲");
                    stream_eof = true;
                    drain_deadline = Some(std::time::Instant::now() + Duration::from_millis(2000));
                }
            }
        }

        if let Some(frame) = &pending {
            // seek 后解码器从关键帧重解，先吐一段目标之前的采样：不丢就会
            // 先响一小段本该跳过、而且主时钟地板会拒绝它的声音
            if is_pre_roll_audio(frame, pipeline.seek_target_secs()) {
                pending = None;
                continue;
            }
            if !audio_output.accepts(frame) {
                // 看门狗：设备不收采样时主时钟也不会走，画面就此静止。
                // 但只有"正在播放却没人消费"才是故障，见 `device_stall`
                if !stall_reported {
                    if let Some(age) = clock.as_ref().and_then(|c| c.observe()).map(|o| o.age) {
                        if let Some(stalled) =
                            device_stall(age, playing_since, OUTPUT_STALL_TIMEOUT)
                        {
                            stall_reported = true;
                            pipeline.report_stall("audio output device", stalled);
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            // 读取当前音量并应用（静音时增益为 0，主时钟照常推进）
            audio_output.set_volume(pipeline.effective_volume());
            if let Err(e) = audio_output.write(frame) {
                tracing::warn!("audio_output write error: {}", e);
            }
            pending = None;
            stall_reported = false;
        }

        // 音频流读到末尾不等于播放结束：设备里还排着最多 1 秒采样，立刻退出会
        // 连同输出设备一起丢掉它们（结尾被截断，主时钟也停在片长之前，EOF 判据
        // 永远等不到）。排空后再走，超时兜底防止设备停摆把线程卡在这里
        if stream_eof {
            let drained = !audio_output.has_pending_audio();
            let timed_out = drain_deadline
                .map(|deadline| std::time::Instant::now() >= deadline)
                .unwrap_or(false);
            if drained || timed_out {
                tracing::debug!(
                    "audio_output_loop: stream EOF，缓冲{}（等待排空）",
                    if drained { "已排空" } else { "排空超时" }
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    tracing::info!("audio_output_loop exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PipelineEventMsg;
    use crossbeam_channel::bounded;

    fn pipeline() -> (Pipeline, crossbeam_channel::Receiver<PipelineEventMsg>) {
        let (tx, rx) = bounded::<PipelineEventMsg>(16);
        (Pipeline::new(tx, 0), rx)
    }

    /// 纯音频的包通道能装下几十秒，demux 先读完不代表播放结束：只认主时钟
    #[test]
    fn audio_only_waits_for_the_media_clock() {
        let (pipeline, rx) = pipeline();
        pipeline.demux_complete.store(true, Ordering::Release);
        let mut idle = 0u32;
        for _ in 0..END_IDLE_LIMIT {
            assert!(
                !no_more_frames(false, &mut idle, &pipeline, false),
                "主时钟还没走到片长就报了 EOF"
            );
        }
        assert!(rx.try_recv().is_err());

        assert!(no_more_frames(true, &mut idle, &pipeline, false));
        match rx.try_recv() {
            Ok(PipelineEventMsg {
                event: PipelineEvent::Ended,
                ..
            }) => {}
            other => panic!("期望 Ended，got {other:?}"),
        }
    }

    /// 视频轨保留 idle 计数兜底：设备停摆时时钟永不推进，不能让线程卡在结束判定上
    #[test]
    fn video_track_keeps_idle_fallback() {
        let (pipeline, rx) = pipeline();
        pipeline.demux_complete.store(true, Ordering::Release);
        let mut idle = 0u32;
        for _ in 0..END_IDLE_LIMIT - 1 {
            assert!(!no_more_frames(false, &mut idle, &pipeline, true));
        }
        assert!(no_more_frames(false, &mut idle, &pipeline, true));
        assert!(rx.try_recv().is_ok());
    }

    /// demux 未完成时帧只是暂时没到
    #[test]
    fn no_eof_before_demux_completes() {
        let (pipeline, rx) = pipeline();
        let mut idle = 0u32;
        assert!(!no_more_frames(true, &mut idle, &pipeline, true));
        assert!(rx.try_recv().is_err());
    }

    fn audio_frame(pts_secs: f64, frame_secs: f64) -> dyl_ffmpeg::AudioFrame {
        let sample_rate = 48_000u32;
        let channels = 2u32;
        dyl_ffmpeg::AudioFrame {
            pts: Some((pts_secs * sample_rate as f64).round() as i64),
            time_base: dyl_ffmpeg::time_base(1, sample_rate as i32),
            sample_rate,
            channels,
            format: dyl_ffmpeg::SampleFormat::F32,
            samples: vec![0.0; (frame_secs * sample_rate as f64 * channels as f64) as usize],
        }
    }

    /// exact seek：关键帧到目标之间的视频帧不得上屏，否则进度条往回跳
    #[test]
    fn pre_roll_video_frames_are_dropped() {
        assert!(is_pre_roll_frame(0.400, 2.183));
        assert!(is_pre_roll_frame(2.180, 2.183));
        assert!(!is_pre_roll_frame(2.183, 2.183), "目标帧必须保留");
        assert!(!is_pre_roll_frame(2.200, 2.183));
        assert!(!is_pre_roll_frame(0.400, 0.0), "未 seek 时不过滤");
    }

    /// exact seek：整帧早于目标才丢，跨过目标边界的帧必须保留
    #[test]
    fn pre_roll_audio_keeps_the_straddling_frame() {
        // 21.3ms 一帧：2.170 起的那帧覆盖 [2.170, 2.191)，跨过目标
        assert!(is_pre_roll_audio(&audio_frame(0.400, 0.0213), 2.183));
        assert!(is_pre_roll_audio(&audio_frame(2.160, 0.0213), 2.183));
        assert!(
            !is_pre_roll_audio(&audio_frame(2.170, 0.0213), 2.183),
            "跨过目标的帧保留"
        );
        assert!(!is_pre_roll_audio(&audio_frame(2.190, 0.0213), 2.183));
        assert!(
            !is_pre_roll_audio(&audio_frame(0.400, 0.0213), 0.0),
            "未 seek 时不过滤"
        );
    }

    /// 只记录上屏 PTS 的输出：管线级断言认的是"到底交给宿主什么"
    struct RecordingOutput {
        submitted: Arc<parking_lot::Mutex<Vec<f64>>>,
    }

    impl dyl_render::VideoOutput for RecordingOutput {
        fn submit_frame(&mut self, frame: &dyl_ffmpeg::VideoFrame) -> dyl_core::Result<()> {
            self.submitted.lock().push(frame.pts_secs().unwrap_or(-1.0));
            Ok(())
        }

        fn resize(&mut self, _width: u32, _height: u32) -> dyl_core::Result<()> {
            Ok(())
        }
    }

    fn video_frame(pts_secs: f64) -> dyl_ffmpeg::VideoFrame {
        dyl_ffmpeg::VideoFrame {
            pts: Some((pts_secs * 1000.0).round() as i64),
            time_base: dyl_ffmpeg::time_base(1, 1000),
            width: 2,
            height: 2,
            display_width: 2,
            display_height: 2,
            format: dyl_ffmpeg::PixelFormat::Yuv420p,
            color_space: dyl_ffmpeg::ColorSpace::Unspecified,
            color_primaries: dyl_ffmpeg::ColorPrimaries::Unspecified,
            color_range: dyl_ffmpeg::ColorRange::Unspecified,
            color_transfer: dyl_ffmpeg::ColorTransfer::Sdr,
            planes: vec![],
        }
    }

    /// 管线级：seek 已发布后，新 generation 的 pre-roll 帧与旧 generation 的残留帧
    /// 都不得到达输出，只有目标帧上屏。裁剪值与 generation 的发布顺序由
    /// `demux::tests::exact_seek_publishes_target_before_generation` 守住。
    #[test]
    fn pre_roll_of_the_new_generation_never_reaches_the_output() {
        let (pipeline, _events) = pipeline();
        let submitted = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let slot = OutputSlot::new(Box::new(RecordingOutput {
            submitted: submitted.clone(),
        }));
        let (tx, rx) = bounded::<Stamped<dyl_ffmpeg::VideoFrame>>(8);

        pipeline.seek_target_us.store(2_000_000, Ordering::Release);
        pipeline.avsync.reset(2.0);
        pipeline.seek_generation.store(1, Ordering::Release);

        tx.send(Stamped::new(0, video_frame(9.0))).unwrap();
        tx.send(Stamped::new(1, video_frame(0.4))).unwrap();
        tx.send(Stamped::new(1, video_frame(1.99))).unwrap();
        tx.send(Stamped::new(1, video_frame(2.0))).unwrap();
        drop(tx);

        render_loop(Arc::new(pipeline), slot, rx, 10.0, true).unwrap();

        assert_eq!(*submitted.lock(), vec![2.0], "只有目标帧可以上屏");
    }

    /// 回归：帧在 `Wait` 里睡着时发生 seek，醒来不得再上屏。
    ///
    /// 换轨恢复实测撞的就是这条：新管线首帧（pts 0.040）比刚起步的音频时钟超前
    /// 40ms 进入 Wait，恢复 seek 在这 40ms 内把主时钟抬到 2.224s；醒来重新决策变成
    /// "落后 2.184s"，防死亡螺旋于是强制上屏 —— 画面回跳 + `AVSync: re-sync` 告警。
    #[test]
    fn a_seek_during_the_wait_invalidates_the_frame() {
        let (pipeline, _events) = pipeline();
        let pipeline = Arc::new(pipeline);
        let submitted = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let slot = OutputSlot::new(Box::new(RecordingOutput {
            submitted: submitted.clone(),
        }));
        let (tx, rx) = bounded::<Stamped<dyl_ffmpeg::VideoFrame>>(8);

        // 首帧立即上屏（惰性锚定），第二帧超前挂钟 150ms → 进入 Wait（每次最多 50ms、
        // 最多 3 次），这段就是 seek 可以插进来的窗口
        tx.send(Stamped::new(0, video_frame(0.05))).unwrap();
        tx.send(Stamped::new(0, video_frame(0.2))).unwrap();

        let p = pipeline.clone();
        let handle = std::thread::spawn(move || render_loop(p, slot, rx, 10.0, true).unwrap());

        let deadline = Instant::now() + Duration::from_secs(2);
        while submitted.lock().is_empty() {
            assert!(Instant::now() < deadline, "首帧没有上屏");
            std::hint::spin_loop();
        }
        // 让渲染线程真的收下第二帧并睡进 Wait：立刻发布 seek 的话第二帧还没被取走，
        // 走的是 generation 陈旧丢弃那条路，测不到本回归要守的分支。
        // Wait 每次最多 50ms、最多 3 次，10ms 落在窗口内且留了 140ms 余量
        std::thread::sleep(Duration::from_millis(10));
        // 按 demux 的发布顺序落一次 exact seek：裁剪值/position → avsync → generation
        pipeline.seek_target_us.store(5_000_000, Ordering::Release);
        pipeline.position_us.store(5_000_000, Ordering::Release);
        pipeline.avsync.reset(5.0);
        pipeline.seek_generation.store(1, Ordering::Release);

        tx.send(Stamped::new(1, video_frame(5.0))).unwrap();
        drop(tx);
        handle.join().unwrap();

        assert_eq!(
            *submitted.lock(),
            vec![0.05, 5.0],
            "等待中的旧位置帧不得被强制上屏"
        );
    }

    /// 暂停免责：恢复的那一瞬间，时钟 age 里装着整段暂停，但设备没坏
    #[test]
    fn paused_time_is_not_an_output_stall() {
        let resumed_now = Instant::now();
        assert_eq!(
            device_stall(Duration::from_secs(60), resumed_now, OUTPUT_STALL_TIMEOUT),
            None,
            "暂停 60 秒后刚恢复，不得判成设备停摆"
        );
        // 恢复后预算内也不报：暂停 60s + 播放 3s
        let three_seconds_ago = resumed_now - Duration::from_secs(3);
        assert_eq!(
            device_stall(
                Duration::from_secs(63),
                three_seconds_ago,
                OUTPUT_STALL_TIMEOUT
            ),
            None
        );
    }

    /// 真停摆仍要报：一直在播放、设备 10 秒不接采样
    #[test]
    fn a_real_output_stall_is_still_reported() {
        let playing_for = Instant::now() - Duration::from_secs(11);
        let stalled = device_stall(Duration::from_secs(30), playing_for, OUTPUT_STALL_TIMEOUT);
        assert!(stalled.is_some(), "播放中设备 11 秒不接采样必须报停摆");

        // 时钟比播放起点更新（刚消费过又停住）时以 age 为准，还没到预算就不报
        assert_eq!(
            device_stall(Duration::from_secs(4), playing_for, OUTPUT_STALL_TIMEOUT),
            None,
            "时钟 4 秒前还在推进，不是停摆"
        );

        // 反过来：时钟很旧但这段本来就没在播放（刚恢复），也不报
        let long_ago = Instant::now() - Duration::from_secs(30);
        assert_eq!(
            device_stall(Duration::from_secs(11), long_ago, OUTPUT_STALL_TIMEOUT),
            Some(Duration::from_secs(11)),
            "上报的是时钟的旧度，不是挂钟"
        );
    }
}
