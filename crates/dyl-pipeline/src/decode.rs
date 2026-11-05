use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, SendTimeoutError, Sender};
use dyl_ffmpeg::{AudioDecoder, AudioFrame, Packet, VideoDecoder, VideoFrame};

use crate::pipeline::Pipeline;
use crate::seek::{recv_current, Stamped};

const RECV_TIMEOUT: Duration = Duration::from_millis(50);

/// 看门狗预算 · 解码排空：`send_eof()` 后取回尾部帧的总时限
///
/// 尾帧要经有界通道交给渲染线程，渲染停摆时 `send` 会永久阻塞，`stop()` 的
/// join 也跟着卡死 —— 排空必须在有限时间内结束。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// 看门狗预算 · 帧交接：把解码帧交给下游的最长等待
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(2);

/// 看门狗状态 — "最近一次有进展的时刻"加上游心跳，并保证一次停滞只报一次
struct StallWatch {
    last_progress: Instant,
    demux_progress: u64,
    demux_tick: Instant,
    reported: bool,
    stage: &'static str,
    pipeline: Arc<Pipeline>,
}

impl StallWatch {
    fn new(pipeline: Arc<Pipeline>, stage: &'static str) -> Self {
        Self {
            last_progress: Instant::now(),
            demux_progress: pipeline.demux_progress.load(Ordering::Acquire),
            demux_tick: Instant::now(),
            reported: false,
            stage,
            pipeline,
        }
    }

    /// 收到可用包 / 交出帧都算进展
    fn progress(&mut self) {
        self.last_progress = Instant::now();
        self.reported = false;
    }

    /// 这段"没进展"是预期行为，不算停滞：暂停期间本就无输入、解码跑在渲染前面
    fn excuse(&mut self) {
        self.last_progress = Instant::now();
    }

    /// 检查是否卡死：解码线程自己收不到数据，可能是这条流恰好没数据了（例如
    /// 视频轨比音频轨先结束），只有 demux 同时也没有产出才是上游卡死
    fn check(&mut self) {
        let grace = self.pipeline.input_stall_timeout;
        self.check_until(grace);
    }

    fn check_until(&mut self, timeout: Duration) {
        let demux_now = self.pipeline.demux_progress.load(Ordering::Acquire);
        if demux_now != self.demux_progress {
            self.demux_progress = demux_now;
            self.demux_tick = Instant::now();
        }
        if self.reported || self.pipeline.demux_complete.load(Ordering::Acquire) {
            return;
        }
        let stalled = self.last_progress.elapsed();
        if stalled >= timeout && self.demux_tick.elapsed() >= timeout {
            self.reported = true;
            self.pipeline.report_stall(self.stage, stalled);
        }
    }
}

/// 帧交接结果
enum Handoff {
    /// 已交给下游
    Delivered,
    /// 下游通道已关闭 — 管线正在拆除，不是故障
    Closed,
    /// 放弃这一帧（卡死时 `handoff` 已经上报过）
    Abandoned,
}

/// 把解码帧交给下游：暂停期间攥着帧继续等，真的停滞才在预算内报一次
///
/// 下游在暂停时本来就不消费，所以这段时间既不算卡死也不能丢帧 —— 丢了就是
/// 一段画面/声音的缺口。预算只在"正在播放"时消耗，`shutdown` 随时可以打断，
/// 否则 `Pipeline::stop()` 的 join 会跟着卡住。
fn handoff<T>(
    pipeline: &Pipeline,
    tx: &Sender<Stamped<T>>,
    mut stamped: Stamped<T>,
    stage: &str,
    timeout: Duration,
) -> Handoff {
    let poll = Duration::from_millis(50);
    let mut spent = Duration::ZERO;
    loop {
        match tx.send_timeout(stamped, poll) {
            Ok(()) => return Handoff::Delivered,
            Err(SendTimeoutError::Disconnected(_)) => return Handoff::Closed,
            Err(SendTimeoutError::Timeout(v)) => {
                stamped = v;
                if pipeline.shutdown.load(Ordering::Acquire) {
                    return Handoff::Abandoned;
                }
                // 暂停期间下游本就不消费，这段等待不计入停滞时长
                if !pipeline.is_paused() {
                    spent += poll;
                }
                if spent >= timeout {
                    pipeline.report_stall(stage, spent);
                    return Handoff::Abandoned;
                }
            }
        }
    }
}

/// Video decode loop — 从 video_pkt_rx 接收数据包，解码后送入 video_frame_tx
///
/// **节流机制**：当输出 buffer 超过 `MAX_BUFFER_FRAMES` 帧时，decode 线程
/// 短暂睡眠等待 render 线程消费，避免解码远超渲染导致 AVSync 大量丢帧。
const MAX_VIDEO_BUFFER_FRAMES: usize = 8;

pub fn video_decode_loop(
    pipeline: Arc<Pipeline>,
    mut decoder: Box<dyn VideoDecoder>,
    video_pkt_rx: Receiver<Stamped<Packet>>,
    video_frame_tx: Sender<Stamped<VideoFrame>>,
) -> dyl_core::Result<()> {
    tracing::info!("video_decode_loop started");

    let mut last_seek_gen: u64 = 0;
    let mut frame_count: u64 = 0;
    let mut watch = StallWatch::new(pipeline.clone(), "video decode");

    loop {
        if pipeline.shutdown.load(Ordering::Acquire) {
            break;
        }

        // 暂停检查
        if pipeline.is_paused() {
            std::thread::sleep(std::time::Duration::from_millis(10));
            watch.excuse();
            continue;
        }

        watch.check();

        // 节流：输出 buffer 过高时等待 render 线程消费
        if video_frame_tx.len() >= MAX_VIDEO_BUFFER_FRAMES {
            std::thread::sleep(std::time::Duration::from_millis(5));
            // 解码跑在渲染前面是健康状态，低帧率视频下可能连续几轮都被挡在这里
            watch.excuse();
            continue;
        }

        // 检查 seek generation，有变化则 flush 解码器
        let seek_gen = pipeline.seek_generation.load(Ordering::Acquire);
        if seek_gen != last_seek_gen {
            last_seek_gen = seek_gen;
            decoder.flush();
            tracing::debug!("video decoder flushed (seek_gen={})", seek_gen);
        }

        match recv_current(&video_pkt_rx, seek_gen, RECV_TIMEOUT) {
            Ok(stamped) => {
                // 输入到了就是输入到了，即便这一包因刚发生的 seek 被丢掉
                watch.progress();
                // 等待期间可能又发生了一次 seek：该包同样属于旧 generation，丢弃
                let gen_now = pipeline.seek_generation.load(Ordering::Acquire);
                if stamped.generation != gen_now {
                    continue;
                }

                if let Err(e) = decoder.submit_packet(&stamped.item) {
                    tracing::warn!("video decode submit_packet: {}", e);
                    continue;
                }
                loop {
                    match decoder.receive_frame() {
                        Ok(Some(frame)) => {
                            frame_count += 1;
                            if frame_count.is_multiple_of(50) {
                                tracing::info!("video_decode: decoded {} frames", frame_count);
                            }
                            let stamped = Stamped::new(gen_now, frame);
                            match handoff(
                                &pipeline,
                                &video_frame_tx,
                                stamped,
                                "video decode handoff",
                                HANDOFF_TIMEOUT,
                            ) {
                                Handoff::Delivered => watch.progress(),
                                Handoff::Closed => {
                                    tracing::debug!("video_frame_tx closed");
                                    return Ok(());
                                }
                                // 放弃这一帧，回到循环顶部重新观察暂停 / 退出
                                Handoff::Abandoned => {
                                    watch.excuse();
                                    break;
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!("video receive_frame: {}", e);
                            break;
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                tracing::debug!("video_pkt_rx disconnected");
                break;
            }
        }
    }

    // 包流读完不等于解码结束：解码器内部还压着 B 帧重排的尾部帧。不排空就退出，
    // 屏幕上留的是最后 0.2 秒之前的画面
    if !pipeline.shutdown.load(Ordering::Acquire) {
        let gen = pipeline.seek_generation.load(Ordering::Acquire);
        if decoder.send_eof().is_ok() {
            drain_decoder(
                &pipeline,
                &video_frame_tx,
                gen,
                "video",
                DRAIN_TIMEOUT,
                || decoder.receive_frame(),
            );
        }
    }

    tracing::info!("video_decode_loop exiting");
    Ok(())
}

/// 排空解码器内部积压的尾帧，总时限由调用方给出（`DRAIN_TIMEOUT`）
///
/// 交接走 `handoff`：渲染线程停摆时这里不能跟着无限等待，否则 `Pipeline::stop()`
/// 的 join 会一起卡死；暂停则不是停摆，时限要跟着停表。
fn drain_decoder<T>(
    pipeline: &Pipeline,
    tx: &Sender<Stamped<T>>,
    generation: u64,
    stage: &str,
    timeout: Duration,
    mut receive_frame: impl FnMut() -> dyl_core::Result<Option<T>>,
) {
    let label = format!("{stage} decode drain");
    let mut deadline = Instant::now() + timeout;
    let mut tick = Instant::now();
    loop {
        let now = Instant::now();
        if !pipeline.is_paused() {
            deadline += now - tick;
        }
        tick = now;
        if now >= deadline {
            pipeline.report_stall(&label, timeout);
            return;
        }
        match receive_frame() {
            Ok(Some(frame)) => match handoff(
                pipeline,
                tx,
                Stamped::new(generation, frame),
                &label,
                timeout,
            ) {
                Handoff::Delivered => {}
                // Closed / Abandoned：下游没了或已经报过停滞
                Handoff::Closed | Handoff::Abandoned => return,
            },
            // 解码器已经没有更多输出；ffmpeg 在真正的 EOF 后是返回错误而不是
            // None，所以这条属于正常收尾，不该按告警记
            Ok(None) => return,
            Err(e) => {
                tracing::debug!("{stage} drain stopped: {e}");
                return;
            }
        }
    }
}

/// Audio decode loop — 从 audio_pkt_rx 接收数据包，解码后送入 audio_frame_tx
pub fn audio_decode_loop(
    pipeline: Arc<Pipeline>,
    mut decoder: Box<dyn AudioDecoder>,
    audio_pkt_rx: Receiver<Stamped<Packet>>,
    audio_frame_tx: Sender<Stamped<AudioFrame>>,
) -> dyl_core::Result<()> {
    tracing::info!("audio_decode_loop started");

    let mut last_seek_gen: u64 = 0;
    let mut frame_count: u64 = 0;
    let mut watch = StallWatch::new(pipeline.clone(), "audio decode");

    loop {
        if pipeline.shutdown.load(Ordering::Acquire) {
            break;
        }

        // 暂停检查
        if pipeline.is_paused() {
            std::thread::sleep(std::time::Duration::from_millis(10));
            watch.excuse();
            continue;
        }

        watch.check();

        let seek_gen = pipeline.seek_generation.load(Ordering::Acquire);
        if seek_gen != last_seek_gen {
            last_seek_gen = seek_gen;
            decoder.flush();
            tracing::debug!("audio decoder flushed (seek_gen={})", seek_gen);
        }

        match recv_current(&audio_pkt_rx, seek_gen, RECV_TIMEOUT) {
            Ok(stamped) => {
                watch.progress();
                let gen_now = pipeline.seek_generation.load(Ordering::Acquire);
                if stamped.generation != gen_now {
                    continue;
                }

                if let Err(e) = decoder.submit_packet(&stamped.item) {
                    tracing::warn!("audio decode submit_packet: {}", e);
                    continue;
                }
                loop {
                    match decoder.receive_frame() {
                        Ok(Some(frame)) => {
                            frame_count += 1;
                            if frame_count.is_multiple_of(100) {
                                tracing::info!("audio_decode: decoded {} frames", frame_count);
                            }
                            let stamped = Stamped::new(gen_now, frame);
                            match handoff(
                                &pipeline,
                                &audio_frame_tx,
                                stamped,
                                "audio decode handoff",
                                HANDOFF_TIMEOUT,
                            ) {
                                Handoff::Delivered => watch.progress(),
                                Handoff::Closed => {
                                    tracing::debug!("audio_frame_tx closed");
                                    return Ok(());
                                }
                                Handoff::Abandoned => {
                                    watch.excuse();
                                    break;
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!("audio receive_frame: {}", e);
                            break;
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                tracing::debug!("audio_pkt_rx disconnected");
                break;
            }
        }
    }

    // 尾部滞留的采样必须排空：AAC 解码器压着最后 1~2 帧（约 1024 采样），
    // 不取出来音频主时钟就停在片长之前，播放结束判定永远等不到，
    // 听感上则是结尾被切掉一小截
    if !pipeline.shutdown.load(Ordering::Acquire) {
        let gen = pipeline.seek_generation.load(Ordering::Acquire);
        if decoder.send_eof().is_ok() {
            drain_decoder(
                &pipeline,
                &audio_frame_tx,
                gen,
                "audio",
                DRAIN_TIMEOUT,
                || decoder.receive_frame(),
            );
        }
    }

    tracing::info!("audio_decode_loop exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{PipelineEvent, PipelineEventMsg};
    use crossbeam_channel::bounded;
    use dyl_core::contract::ErrorCode;

    /// 测试用的短预算：真实预算是 2s，这里只验证判定逻辑本身
    const BUDGET: Duration = Duration::from_millis(20);
    const WAIT: Duration = Duration::from_millis(40);

    fn pipeline_with_events() -> (Arc<Pipeline>, crossbeam_channel::Receiver<PipelineEventMsg>) {
        let (tx, rx) = bounded::<PipelineEventMsg>(16);
        (Arc::new(Pipeline::new(tx, 0)), rx)
    }

    /// demux 还在产出时不该报"上游卡死"：视频轨比音频轨先结束时，视频解码
    /// 线程本来就收不到数据
    #[test]
    fn input_stall_needs_demux_to_be_quiet_too() {
        let (pipeline, rx) = pipeline_with_events();
        let mut watch = StallWatch::new(pipeline.clone(), "video decode");
        std::thread::sleep(WAIT);
        pipeline.demux_progress.store(7, Ordering::Release);
        watch.check_until(BUDGET);
        assert!(rx.try_recv().is_err(), "demux 心跳新鲜时不应报停滞");

        // demux 也静止超过预算 → 判定卡死，报一次
        std::thread::sleep(WAIT);
        watch.check_until(BUDGET);
        match rx.recv().expect("上游静止应报停滞") {
            PipelineEventMsg {
                event: PipelineEvent::Error { code, message },
                ..
            } => {
                assert_eq!(code, ErrorCode::Pipeline);
                assert!(message.contains("video decode"), "{message}");
            }
            other => panic!("期望 Error 事件，got {other:?}"),
        }
        watch.check_until(BUDGET);
        assert!(rx.try_recv().is_err(), "同一次停滞不应重复上报");
    }

    /// 有进展后重新武装；demux 已完成时不再监督
    #[test]
    fn progress_rearms_watch_and_demux_complete_exempts_it() {
        let (pipeline, rx) = pipeline_with_events();
        let mut watch = StallWatch::new(pipeline.clone(), "audio decode");
        std::thread::sleep(WAIT);
        watch.check_until(BUDGET);
        assert!(rx.recv().is_ok(), "首次停滞应上报");

        watch.progress();
        std::thread::sleep(WAIT);
        watch.check_until(BUDGET);
        assert!(rx.recv().is_ok(), "恢复之后再次停滞应重新上报");

        watch.progress();
        pipeline.demux_complete.store(true, Ordering::Release);
        std::thread::sleep(WAIT);
        watch.check_until(BUDGET);
        assert!(rx.try_recv().is_err(), "demux 完成后收不到输入是正常收尾");
    }

    /// 排空必须在预算内返回：下游不接手时尾帧只能放弃，并把停滞报出去
    #[test]
    fn drain_gives_up_at_deadline() {
        let (pipeline, rx) = pipeline_with_events();
        let (frame_tx, _frame_rx) = bounded::<Stamped<u8>>(1);
        let mut produced = 0usize;
        drain_decoder(&pipeline, &frame_tx, 3, "video", BUDGET, || {
            produced += 1;
            Ok(Some(produced as u8))
        });
        assert_eq!(produced, 2, "第二帧交接不到就该在时限内停止");
        assert_eq!(frame_tx.len(), 1, "通道里只留下成功交接的那一帧");
        match rx.recv().expect("排空超时应上报") {
            PipelineEventMsg {
                event: PipelineEvent::Error { message, .. },
                ..
            } => {
                assert!(message.contains("video decode drain"), "{message}")
            }
            other => panic!("期望 Error 事件，got {other:?}"),
        }
    }

    /// 下游被拆（stop / 重载）不是故障：正常退出不该冒出错误事件
    #[test]
    fn drain_is_silent_when_downstream_is_gone() {
        let (pipeline, rx) = pipeline_with_events();
        let (frame_tx, frame_rx) = bounded::<Stamped<u8>>(1);
        drop(frame_rx);
        drain_decoder(&pipeline, &frame_tx, 0, "audio", BUDGET, || Ok(Some(1u8)));
        assert!(rx.try_recv().is_err(), "管线拆除时的退出不该报停滞");
    }
}
