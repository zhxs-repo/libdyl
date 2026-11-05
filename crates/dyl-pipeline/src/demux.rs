use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossbeam_channel::{SendTimeoutError, Sender};
use dyl_ffmpeg::{Demuxer, Packet};

use crate::event::PipelineEvent;
use crate::pipeline::Pipeline;
use crate::seek::Stamped;

/// 探测关键帧时最多读多少个包；超过就退回请求目标，避免异常文件把 seek 卡住
const KEYFRAME_PROBE_LIMIT: usize = 512;

/// 通道满时的发送轮询间隔：暂停时下游不消费，阻塞期间仍须能观察 seek/shutdown
const SEND_POLL: std::time::Duration = std::time::Duration::from_millis(20);

enum SendOutcome {
    Sent,
    Closed,
    /// 送到一半发现 seek 请求或 shutdown：这一包属于旧位置，直接丢弃
    Interrupted,
}

/// 带超时投递：暂停 → 解码线程不消费 → 通道很快写满，普通 `send` 会让 demux
/// 永久阻塞在发送上，seek 请求（从 demo 循环顶部取）再也没有消费者。
fn send_interruptible<T>(
    pipeline: &Pipeline,
    tx: &Sender<Stamped<T>>,
    mut stamped: Stamped<T>,
) -> SendOutcome {
    loop {
        match tx.send_timeout(stamped, SEND_POLL) {
            Ok(()) => return SendOutcome::Sent,
            Err(SendTimeoutError::Disconnected(_)) => return SendOutcome::Closed,
            Err(SendTimeoutError::Timeout(v)) => {
                stamped = v;
                if pipeline.shutdown.load(Ordering::Acquire)
                    || pipeline.seek_request.lock().is_some()
                {
                    return SendOutcome::Interrupted;
                }
            }
        }
    }
}

/// seek 之后读包直到第一个视频关键帧，读到的包按原顺序缓存，稍后照常投递。
///
/// 返回关键帧 PTS（秒）。找不到关键帧（无视频流、EOF、时间戳不可用）时退回请求目标，
/// 此时行为等价于 exact seek。
fn probe_keyframe(
    pipeline: &Pipeline,
    demuxer: &mut dyn Demuxer,
    pending: &mut VecDeque<Packet>,
    target: f64,
) -> f64 {
    let Some(video_index) = demuxer.info().and_then(|info| info.video_stream_index) else {
        return target;
    };

    for _ in 0..KEYFRAME_PROBE_LIMIT {
        if pipeline.shutdown.load(Ordering::Acquire) {
            break;
        }
        match demuxer.read_packet() {
            Ok(Some(pkt)) => {
                let secs = pkt_secs(&pkt);
                let is_keyframe = pkt.stream_index == video_index && pkt.is_key;
                pending.push_back(pkt);
                if is_keyframe {
                    if let Some(secs) = secs {
                        return secs;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("demux: keyframe probe stopped: {e}");
                break;
            }
        }
    }

    tracing::warn!(
        "demux: no video keyframe found after seeking to {target:.3}s, reporting the target"
    );
    target
}

fn pkt_secs(pkt: &Packet) -> Option<f64> {
    let tb = pkt.time_base;
    if tb.numerator() == 0 || tb.denominator() == 0 {
        return None;
    }
    pkt.pts
        .map(|ticks| ticks as f64 * tb.numerator() as f64 / tb.denominator() as f64)
}

/// Demux loop — 从 Demuxer 读取 Packet，分发到视频/音频/字幕解码通道
#[allow(clippy::collapsible_if)]
pub fn demux_loop(
    pipeline: Arc<Pipeline>,
    mut demuxer: Box<dyn Demuxer>,
    video_pkt_tx: Sender<Stamped<Packet>>,
    audio_pkt_tx: Sender<Stamped<Packet>>,
    subtitle_pkt_tx: Option<Sender<Stamped<Packet>>>,
) -> dyl_core::Result<()> {
    tracing::info!("demux_loop started");

    let mut pkt_count: u64 = 0;
    // keyframe seek 探测期间读到的包：它们属于 seek 之后的新世代，不能丢
    let mut pending: VecDeque<Packet> = VecDeque::new();
    loop {
        if pipeline.shutdown.load(Ordering::Acquire) {
            tracing::info!("demux_loop: shutdown requested");
            break;
        }

        // 检查 seek 请求
        if let Some(req) = {
            let mut lock = pipeline.seek_request.lock();
            lock.take()
        } {
            // seek 会让解码位置失效，旧位置读出的包一律作废
            pending.clear();
            tracing::info!(
                "demux: seeking to {:.3}s ({})",
                req.target,
                if req.exact { "exact" } else { "keyframe" }
            );
            let seek_result = demuxer.seek(req.target);
            if let Err(e) = &seek_result {
                tracing::error!("demux: seek failed: {}", e);
            }
            // keyframe 模式停在目标之前最近的关键帧，actual 是该关键帧 PTS；
            // exact 模式仍以请求目标为提交点，pre-roll 由呈现端裁剪
            let actual = if seek_result.is_ok() && !req.exact {
                probe_keyframe(&pipeline, demuxer.as_mut(), &mut pending, req.target)
            } else {
                req.target
            };
            // 递增 seek_generation：下游按 generation 丢弃 seek 之前入队的残留数据，
            // 解码线程同时据此触发 decoder.flush()
            let generation = pipeline.seek_generation.fetch_add(1, Ordering::Release) + 1;
            // 重置 AVSync
            pipeline.avsync.reset(actual);
            // position 立即对齐到落点，避免残留旧帧把进度条往回拽
            pipeline
                .position_us
                .store((actual * 1_000_000.0) as u64, Ordering::Release);
            // 只有 exact 要丢 pre-roll；keyframe 必须从关键帧完整播放
            let drop_before = if req.exact { actual } else { 0.0 };
            pipeline
                .seek_target_us
                .store((drop_before * 1_000_000.0) as u64, Ordering::Release);
            // seek 之后还要继续读到目标位置之后的数据，EOF 标记不能沿用
            if seek_result.is_ok() {
                pipeline.demux_complete.store(false, Ordering::Release);
            }
            // 每条被接受的 seek 命令都必须有唯一结局，引擎据此完成 command_completed；
            // tag 原样带回，便于引擎识别已被更新请求覆盖的旧结局
            match seek_result {
                Ok(()) => pipeline.emit(PipelineEvent::SeekApplied {
                    tag: req.tag,
                    generation,
                    position_secs: actual,
                }),
                Err(e) => pipeline.emit(PipelineEvent::SeekFailed {
                    tag: req.tag,
                    message: format!("seek to {:.3}s failed: {e}", req.target),
                }),
            }
        }

        // 先记录 generation 再读包：只有 seek 之后读到的包才属于新的 generation，
        // seek 之前读到、之后才入队的包仍带旧标记，会被下游丢弃
        let generation = pipeline.seek_generation.load(Ordering::Acquire);

        // 探测阶段缓存下来的包先投递，保持原始顺序
        let next_packet = match pending.pop_front() {
            Some(pkt) => Ok(Some(pkt)),
            None => demuxer.read_packet(),
        };
        match next_packet {
            Ok(Some(pkt)) => {
                pkt_count += 1;
                // 看门狗心跳：只要这里在动，下游收不到数据就只是"这条流没数据了"
                pipeline.demux_progress.store(pkt_count, Ordering::Release);
                if pkt_count.is_multiple_of(100) {
                    tracing::info!("demux: read {} packets", pkt_count);
                }
                let info = demuxer.info().unwrap();
                let stream_idx = pkt.stream_index;
                let stamped = Stamped::new(generation, pkt);
                let outcome = if Some(stream_idx) == info.video_stream_index {
                    Some((
                        "video",
                        send_interruptible(&pipeline, &video_pkt_tx, stamped),
                    ))
                } else if Some(stream_idx) == info.audio_stream_index {
                    Some((
                        "audio",
                        send_interruptible(&pipeline, &audio_pkt_tx, stamped),
                    ))
                } else if Some(stream_idx) == info.subtitle_stream_index {
                    subtitle_pkt_tx
                        .as_ref()
                        .map(|tx| ("subtitle", send_interruptible(&pipeline, tx, stamped)))
                } else {
                    None
                };
                match outcome {
                    Some((name, SendOutcome::Closed)) => {
                        tracing::debug!("demux: {}_pkt_tx closed", name);
                        break;
                    }
                    Some((_, SendOutcome::Interrupted)) => {
                        if pipeline.shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        // seek 请求已置位：回到循环顶部消费它
                        continue;
                    }
                    _ => {}
                }
            }
            Ok(None) => {
                // EOF — 设置 demux_complete flag，由 render_loop 判定结束。
                // 线程不能退出：短文件往往在播放结束前就读空，退出后 seek_request
                // 再无人消费，暂停/播放中 seek 会永远等不到 SeekApplied。
                tracing::info!("demux: EOF");
                pipeline.demux_complete.store(true, Ordering::Release);
                loop {
                    if pipeline.shutdown.load(Ordering::Acquire) {
                        tracing::info!("demux_loop exiting");
                        return Ok(());
                    }
                    if pipeline.seek_request.lock().is_some() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            Err(e) => {
                tracing::error!("demux: read_packet error: {}", e);
                break;
            }
        }
    }

    tracing::info!("demux_loop exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use crossbeam_channel::{bounded, Receiver as CReceiver, Sender as CSender};
    use dyl_ffmpeg::{MediaInfo, PixelFormat, SampleFormat, TrackSelection};

    use crate::event::PipelineEventMsg;

    /// 按脚本供货的 demuxer：每次 `read_packet` 都先报告"已进入"再等待放行，
    /// 使"seek 请求何时置位""generation 何时读取"在测试里完全确定
    struct ScriptedDemuxer {
        info: MediaInfo,
        entered: CSender<usize>,
        release: CReceiver<()>,
        calls: usize,
    }

    impl Demuxer for ScriptedDemuxer {
        fn open(
            &mut self,
            _url: &str,
            _selection: &TrackSelection,
            _shutdown: Arc<std::sync::atomic::AtomicBool>,
        ) -> dyl_core::Result<()> {
            Ok(())
        }

        fn read_packet(&mut self) -> dyl_core::Result<Option<Packet>> {
            self.calls += 1;
            let call = self.calls;
            if call > 2 {
                return Ok(None); // EOF
            }
            let _ = self.entered.send(call);
            let _ = self.release.recv();
            Ok(Some(pkt(call)))
        }

        fn seek(&mut self, _target: f64) -> dyl_core::Result<()> {
            Ok(())
        }

        fn info(&self) -> Option<&MediaInfo> {
            Some(&self.info)
        }

        fn take_video_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn take_audio_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn take_subtitle_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn close(&mut self) {}
    }

    fn pkt(call: usize) -> Packet {
        Packet {
            data: vec![call as u8],
            stream_index: 0,
            pts: Some(call as i64 * 1000),
            dts: Some(call as i64 * 1000),
            duration: None,
            time_base: dyl_ffmpeg::time_base(1, 1000),
            is_key: true,
        }
    }

    fn video_info() -> MediaInfo {
        MediaInfo {
            duration: 100.0,
            video_stream_index: Some(0),
            audio_stream_index: None,
            subtitle_stream_index: None,
            video_codec_id: None,
            audio_codec_id: None,
            subtitle_codec_id: None,
            width: 320,
            height: 180,
            display_width: 320,
            display_height: 180,
            pixel_format: PixelFormat::Yuv420p,
            fps: 25.0,
            sample_rate: 0,
            channels: 0,
            sample_format: SampleFormat::Unknown,
            tracks: vec![],
            subtitle_script: None,
        }
    }

    /// seek 之后先吐两个非关键帧再吐关键帧，用于验证 keyframe 探测
    struct KeyframeDemuxer {
        info: MediaInfo,
        calls: usize,
    }

    impl Demuxer for KeyframeDemuxer {
        fn open(
            &mut self,
            _url: &str,
            _selection: &TrackSelection,
            _shutdown: Arc<std::sync::atomic::AtomicBool>,
        ) -> dyl_core::Result<()> {
            Ok(())
        }

        fn read_packet(&mut self) -> dyl_core::Result<Option<Packet>> {
            self.calls += 1;
            let pkt = match self.calls {
                1 => Some(pkt_with(1, 500, false)),
                2 => Some(pkt_with(2, 900, false)),
                3 => Some(pkt_with(3, 1000, true)),
                _ => None,
            };
            Ok(pkt)
        }

        fn seek(&mut self, _target: f64) -> dyl_core::Result<()> {
            Ok(())
        }

        fn info(&self) -> Option<&MediaInfo> {
            Some(&self.info)
        }

        fn take_video_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn take_audio_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn take_subtitle_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn close(&mut self) {}
    }

    fn pkt_with(call: usize, pts_ms: i64, is_key: bool) -> Packet {
        Packet {
            data: vec![call as u8],
            stream_index: 0,
            pts: Some(pts_ms),
            dts: Some(pts_ms),
            duration: None,
            time_base: dyl_ffmpeg::time_base(1, 1000),
            is_key,
        }
    }

    /// 先 EOF，seek 之后的下一次 read 阻塞，用于断言 seek 被消费且 EOF 标记已清
    struct EofParkDemuxer {
        info: MediaInfo,
        calls: usize,
        entered: CSender<usize>,
        release: CReceiver<()>,
    }
    impl Demuxer for EofParkDemuxer {
        fn open(
            &mut self,
            _url: &str,
            _selection: &TrackSelection,
            _shutdown: Arc<std::sync::atomic::AtomicBool>,
        ) -> dyl_core::Result<()> {
            Ok(())
        }

        fn read_packet(&mut self) -> dyl_core::Result<Option<Packet>> {
            self.calls += 1;
            if self.calls == 1 {
                return Ok(None);
            }
            let _ = self.entered.send(self.calls);
            let _ = self.release.recv();
            Ok(Some(pkt(self.calls)))
        }

        fn seek(&mut self, _target: f64) -> dyl_core::Result<()> {
            Ok(())
        }

        fn info(&self) -> Option<&MediaInfo> {
            Some(&self.info)
        }

        fn take_video_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn take_audio_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn take_subtitle_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn close(&mut self) {}
    }

    /// 回归：demux 读空后不能退出，否则暂停/播放中的 seek 永远没有消费者
    #[test]
    fn seek_after_eof_is_serviced() {
        let (event_tx, event_rx) = bounded::<PipelineEventMsg>(16);
        let pipeline = Arc::new(Pipeline::new(event_tx, 42));
        let (entered_tx, entered_rx) = bounded::<usize>(4);
        let (release_tx, release_rx) = bounded::<()>(4);
        let demuxer: Box<dyn Demuxer> = Box::new(EofParkDemuxer {
            info: video_info(),
            calls: 0,
            entered: entered_tx,
            release: release_rx,
        });
        let (video_pkt_tx, video_pkt_rx) = bounded::<Stamped<Packet>>(8);
        let (audio_pkt_tx, _audio_pkt_rx) = bounded::<Stamped<Packet>>(8);

        let p = pipeline.clone();
        let handle = std::thread::spawn(move || {
            demux_loop(p, demuxer, video_pkt_tx, audio_pkt_tx, None).unwrap()
        });

        // 第一次 read 直接 EOF，线程应停在等待 seek 的循环里
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pipeline.demux_complete.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(pipeline.demux_complete.load(Ordering::Acquire));

        *pipeline.seek_request.lock() = Some(crate::pipeline::SeekRequest {
            target: 2.0,
            from: 1.0,
            tag: 11,
            exact: true,
        });
        match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            PipelineEventMsg {
                session: 42,
                event:
                    PipelineEvent::SeekApplied {
                        tag,
                        generation,
                        position_secs,
                    },
            } => {
                assert_eq!(tag, 11);
                assert_eq!(generation, 1);
                assert_eq!(position_secs, 2.0);
            }
            other => panic!("期望 SeekApplied，got {other:?}"),
        }
        // seek 成功后 EOF 标记必须清掉，否则后续数据会被判成已播完
        assert!(!pipeline.demux_complete.load(Ordering::Acquire));
        assert_eq!(pipeline.position_us.load(Ordering::Acquire), 2_000_000);

        // 解除 seek 后的阻塞读取，再置 shutdown 让线程退出
        assert_eq!(entered_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        release_tx.send(()).unwrap();
        pipeline.shutdown.store(true, Ordering::Release);
        handle.join().unwrap();
        assert_eq!(video_pkt_rx.try_recv().unwrap().generation, 1);
    }

    /// 回归：packet 的 generation 取自"读取之前"观察到的值，seek 之后读到的包才属于新 generation
    #[test]
    fn stamps_packets_with_generation_observed_before_read() {
        let (event_tx, event_rx) = bounded::<PipelineEventMsg>(16);
        let pipeline = Arc::new(Pipeline::new(event_tx, 1));
        let (entered_tx, entered_rx) = bounded::<usize>(4);
        let (release_tx, release_rx) = bounded::<()>(4);
        let demuxer: Box<dyn Demuxer> = Box::new(ScriptedDemuxer {
            info: video_info(),
            entered: entered_tx,
            release: release_rx,
            calls: 0,
        });
        let (video_pkt_tx, video_pkt_rx) = bounded::<Stamped<Packet>>(8);
        let (audio_pkt_tx, _audio_pkt_rx) = bounded::<Stamped<Packet>>(8);

        let p = pipeline.clone();
        let handle = std::thread::spawn(move || {
            demux_loop(p, demuxer, video_pkt_tx, audio_pkt_tx, None).unwrap()
        });

        // 第 1 次 read 发生在 seek 之前 → 必须标记为旧的 generation 0
        assert_eq!(entered_rx.recv().unwrap(), 1);
        // 循环正阻塞在 read_packet 内部，此时置位的 seek 只会在下一轮顶部被处理
        *pipeline.seek_request.lock() = Some(crate::pipeline::SeekRequest {
            target: 42.0,
            from: 1.0,
            tag: 9,
            exact: true,
        });
        release_tx.send(()).unwrap();
        let first = video_pkt_rx.recv().unwrap();
        assert_eq!(first.generation, 0);
        assert_eq!(first.item.data, vec![1]);

        // 第 2 次 read 时循环顶部已处理完 seek → 属于新的 generation 1
        assert_eq!(entered_rx.recv().unwrap(), 2);
        assert_eq!(pipeline.seek_generation.load(Ordering::Acquire), 1);
        release_tx.send(()).unwrap();
        let second = video_pkt_rx.recv().unwrap();
        assert_eq!(second.generation, 1);
        assert_eq!(second.item.data, vec![2]);

        // EOF 后 demux 线程停在"等 seek/shutdown"上，不再自行退出
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pipeline.demux_complete.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(pipeline.demux_complete.load(Ordering::Acquire));
        pipeline.shutdown.store(true, Ordering::Release);
        handle.join().unwrap();
        // seek 立即把 position 对齐到目标，残留旧帧无法再把进度往回拽
        assert_eq!(pipeline.position_us.load(Ordering::Acquire), 42_000_000);
        match event_rx.recv().unwrap() {
            PipelineEventMsg {
                session: 1,
                event:
                    PipelineEvent::SeekApplied {
                        tag,
                        generation,
                        position_secs,
                    },
            } => {
                assert_eq!(tag, 9);
                assert_eq!(generation, 1);
                assert_eq!(position_secs, 42.0);
            }
            other => panic!("期望 SeekApplied，got {other:?}"),
        }
    }

    /// keyframe seek：落点必须报告关键帧 PTS，探测期间读到的包不能丢，
    /// 且不得设置 pre-roll 裁剪目标
    #[test]
    fn keyframe_seek_reports_the_keyframe_position() {
        let (event_tx, event_rx) = bounded::<PipelineEventMsg>(16);
        let pipeline = Arc::new(Pipeline::new(event_tx, 7));
        let demuxer: Box<dyn Demuxer> = Box::new(KeyframeDemuxer {
            info: video_info(),
            calls: 0,
        });
        let (video_pkt_tx, video_pkt_rx) = bounded::<Stamped<Packet>>(8);
        let (audio_pkt_tx, _audio_pkt_rx) = bounded::<Stamped<Packet>>(8);

        *pipeline.seek_request.lock() = Some(crate::pipeline::SeekRequest {
            target: 3.0,
            from: 0.0,
            tag: 21,
            exact: false,
        });

        let p = pipeline.clone();
        let handle = std::thread::spawn(move || {
            demux_loop(p, demuxer, video_pkt_tx, audio_pkt_tx, None).unwrap()
        });

        match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            PipelineEventMsg {
                session: 7,
                event:
                    PipelineEvent::SeekApplied {
                        tag,
                        generation,
                        position_secs,
                    },
            } => {
                assert_eq!(tag, 21);
                assert_eq!(generation, 1);
                assert!(
                    (position_secs - 1.0).abs() < f64::EPSILON,
                    "keyframe seek 必须报告关键帧 PTS，got {position_secs}"
                );
            }
            other => panic!("期望 SeekApplied，got {other:?}"),
        }
        assert_eq!(pipeline.position_us.load(Ordering::Acquire), 1_000_000);
        assert_eq!(
            pipeline.seek_target_us.load(Ordering::Acquire),
            0,
            "keyframe seek 不得裁剪 pre-roll"
        );

        // 探测阶段读到的两个非关键帧和关键帧本身都要按原顺序投递
        let mut delivered = Vec::new();
        for _ in 0..3 {
            let stamped = video_pkt_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(stamped.generation, 1);
            delivered.push(pkt_secs(&stamped.item).unwrap());
        }
        assert_eq!(delivered, vec![0.5, 0.9, 1.0]);

        pipeline.shutdown.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    /// 只要通道有空位就一直供货，不产生 EOF
    struct FloodDemuxer {
        info: MediaInfo,
    }

    impl Demuxer for FloodDemuxer {
        fn open(
            &mut self,
            _url: &str,
            _selection: &TrackSelection,
            _shutdown: Arc<std::sync::atomic::AtomicBool>,
        ) -> dyl_core::Result<()> {
            Ok(())
        }

        fn read_packet(&mut self) -> dyl_core::Result<Option<Packet>> {
            Ok(Some(pkt(1)))
        }

        fn seek(&mut self, _target: f64) -> dyl_core::Result<()> {
            Ok(())
        }

        fn info(&self) -> Option<&MediaInfo> {
            Some(&self.info)
        }

        fn take_video_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn take_audio_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn take_subtitle_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
            None
        }

        fn close(&mut self) {}
    }

    /// 回归：暂停时解码线程不消费，demux 会把 packet 通道写满并阻塞在发送上；
    /// 此时置位的 seek（RestartSeek 路径）必须仍被消费并发出 SeekApplied，
    /// 否则命令只能等满 10s 超时
    #[test]
    fn seek_is_serviced_while_packet_channel_is_full() {
        let (event_tx, event_rx) = bounded::<PipelineEventMsg>(16);
        let pipeline = Arc::new(Pipeline::new(event_tx, 7));
        pipeline.pause();
        let demuxer: Box<dyn Demuxer> = Box::new(FloodDemuxer { info: video_info() });
        let (video_pkt_tx, video_pkt_rx) = bounded::<Stamped<Packet>>(2);
        let (audio_pkt_tx, _audio_pkt_rx) = bounded::<Stamped<Packet>>(2);

        let p = pipeline.clone();
        let handle = std::thread::spawn(move || {
            demux_loop(p, demuxer, video_pkt_tx, audio_pkt_tx, None).unwrap()
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while video_pkt_rx.len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(video_pkt_rx.len(), 2, "视频通道应先被写满");

        *pipeline.seek_request.lock() = Some(crate::pipeline::SeekRequest {
            target: 3.0,
            from: 0.0,
            tag: 21,
            exact: true,
        });
        match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            PipelineEventMsg {
                session: 7,
                event:
                    PipelineEvent::SeekApplied {
                        tag,
                        generation,
                        position_secs,
                    },
            } => {
                assert_eq!(tag, 21);
                assert_eq!(generation, 1);
                assert_eq!(position_secs, 3.0);
            }
            other => panic!("期望 SeekApplied，got {other:?}"),
        }

        pipeline.shutdown.store(true, Ordering::Release);
        handle.join().unwrap();
    }
}
