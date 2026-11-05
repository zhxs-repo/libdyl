use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, Sender};
use dyl_audio::{create_default_output, AudioOutput};
use dyl_core::contract::ErrorCode;
use dyl_core::{AudioInfo, VideoInfo};
use dyl_ffmpeg::{AudioDecoder, Demuxer, FfmpegAudioDecoder, FfmpegVideoDecoder, VideoDecoder};
use dyl_render::OutputSlot;
use dyl_subtitle::SubtitleEngine;
use parking_lot::Mutex;

use crate::avsync::AVSync;
use crate::decode::{audio_decode_loop, video_decode_loop};
use crate::demux::demux_loop;
use crate::event::{PipelineEvent, PipelineEventMsg};
use crate::output::{audio_output_loop, render_loop};
use crate::seek::Stamped;
use crate::subtitle::subtitle_decode_loop;

/// Seek 请求
pub struct SeekRequest {
    pub target: f64,
    pub from: f64,
    /// 引擎分配的不透明标记：SeekApplied/SeekFailed 原样带回，
    /// 合并 seek 时用于丢弃已被新请求覆盖的旧结局
    pub tag: u64,
    /// `true` = 精确 seek：解到目标并丢弃 pre-roll；
    /// `false` = 关键帧 seek：停在目标前最近的关键帧并完整播放
    pub exact: bool,
}

/// 看门狗预算 · 输入停滞（本地来源）
pub(crate) const INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// 看门狗预算 · 输入停滞（网络来源）
///
/// http 读包要经过 CDN、TLS 和 TCP 重连，缓冲几秒是常态；用本地的 2s
/// 会把一次正常抖动误判成卡死并终止播放。读取本身有 `rw_timeout` 兜底，
/// 这里放宽不会让真正的断网一直挂着不报。
pub(crate) const NETWORK_INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(20);

/// 按来源挑输入停滞预算
pub(crate) fn input_stall_grace(url: &str) -> Duration {
    if dyl_ffmpeg::source::is_network_url(url) {
        NETWORK_INPUT_STALL_TIMEOUT
    } else {
        INPUT_STALL_TIMEOUT
    }
}

/// Pipeline — 管理所有播放线程和共享状态
pub struct Pipeline {
    pub avsync: Arc<AVSync>,
    pub seek_request: Arc<Mutex<Option<SeekRequest>>>,
    pub seek_generation: Arc<AtomicU64>,
    pub position_us: Arc<AtomicU64>,
    /// 最近一次 seek 的目标（微秒）；`0` 表示不丢 pre-roll（keyframe seek）
    ///
    /// exact seek 从目标之前的关键帧解起，解码器会吐出目标之前的 pre-roll。
    /// 呈现端据此丢弃这些帧/采样，否则画面会先跳回关键帧、进度条往回拽，
    /// 声音也会先响一段本不该播的内容。
    pub seek_target_us: Arc<AtomicU64>,
    pub duration_us: Arc<AtomicU64>,
    pub demux_complete: Arc<AtomicBool>,
    /// 已跑到自然结尾（render_loop 判定 EOF 后置位）
    ///
    /// 与 `shutdown` 分开：shutdown 是外部要求停，ended 是自己播完了。
    /// 播完之后所有工作线程都退出了，seek 请求没有消费者。
    pub ended: Arc<AtomicBool>,
    /// demux 读出的包总数 — 看门狗的"上游还在动"心跳
    ///
    /// 解码线程只看自己收不到数据，分不清是"这条流恰好没数据了"还是"上游
    /// read_packet 卡死了"。计数不变且自己也无进展才算真卡死。
    pub demux_progress: Arc<AtomicU64>,
    pub shutdown: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    /// 输入停滞看门狗的预算；`start` 按 URL 是本地还是网络选值
    pub(crate) input_stall_timeout: Duration,
    pub volume: Arc<AtomicU32>,
    pub muted: Arc<AtomicBool>,
    pub event_tx: Sender<PipelineEventMsg>,
    /// 本次播放会话标识：引擎据此丢弃重载后迟到的旧管线事件
    pub session: u64,
    /// 本次管线实际启用的硬件设备名；`None` = 软解
    decode_device: Option<String>,
    thread_handles: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}

impl Pipeline {
    pub fn new(event_tx: Sender<PipelineEventMsg>, session: u64) -> Self {
        let seek_generation = Arc::new(AtomicU64::new(0));
        let paused = Arc::new(AtomicBool::new(false));
        let avsync = Arc::new(AVSync::new());
        // 主时钟门控要实时观察播放状态与 seek generation
        avsync.set_clock_context(paused.clone(), seek_generation.clone());
        Self {
            avsync,
            seek_request: Arc::new(Mutex::new(None)),
            seek_generation,
            position_us: Arc::new(AtomicU64::new(0)),
            seek_target_us: Arc::new(AtomicU64::new(0)),
            duration_us: Arc::new(AtomicU64::new(0)),
            demux_complete: Arc::new(AtomicBool::new(false)),
            ended: Arc::new(AtomicBool::new(false)),
            demux_progress: Arc::new(AtomicU64::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
            paused,
            input_stall_timeout: INPUT_STALL_TIMEOUT,
            volume: Arc::new(AtomicU32::new(100)),
            muted: Arc::new(AtomicBool::new(false)),
            event_tx,
            session,
            decode_device: None,
            thread_handles: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 本次管线实际启用的硬件设备名；`None` = 软解（或没有视频轨）
    pub fn decode_device(&self) -> Option<&str> {
        self.decode_device.as_deref()
    }

    /// 向引擎报告内部事件
    pub fn emit(&self, event: PipelineEvent) {
        let _ = self.event_tx.send(PipelineEventMsg {
            session: self.session,
            event,
        });
    }

    /// 获取当前播放位置（秒）
    pub fn current_position_secs(&self) -> f64 {
        self.position_us.load(Ordering::Acquire) as f64 / 1_000_000.0
    }

    /// 最近一次 seek 的目标（秒）；0 表示尚未 seek
    pub fn seek_target_secs(&self) -> f64 {
        self.seek_target_us.load(Ordering::Acquire) as f64 / 1_000_000.0
    }

    /// 获取总时长（秒）
    pub fn duration_secs(&self) -> f64 {
        self.duration_us.load(Ordering::Acquire) as f64 / 1_000_000.0
    }

    /// 暂停 pipeline
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        tracing::info!("pipeline: paused");
    }

    /// 恢复 pipeline
    pub fn resume(&self) {
        let current_pos = self.current_position_secs();
        self.avsync.reset(current_pos);
        self.paused.store(false, Ordering::Release);
        tracing::info!("pipeline: resumed at {:.3}s", current_pos);
    }

    /// 检查是否暂停
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// 管线是否已经跑到自然结尾（render_loop 判定 EOF 并退出）
    ///
    /// 播完之后线程都退了，seek 请求没人消费 —— 调用方需要知道这一点才能
    /// 决定是重载文件还是直接拒绝。
    pub fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// 设置音量 (0-100)
    pub fn set_volume(&self, vol: u8) {
        self.volume.store(vol as u32, Ordering::Relaxed);
    }

    /// 获取音量 (0-100)
    pub fn get_volume(&self) -> u32 {
        self.volume.load(Ordering::Relaxed)
    }

    /// 设置静音开关（不影响已存的音量值，取消静音即回到原音量）
    pub fn set_mute(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// 当前是否静音
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// 实际送给他音频输出的增益：静音时为 0，否则为音量的 0.0-1.0 形式
    pub fn effective_volume(&self) -> f32 {
        if self.is_muted() {
            0.0
        } else {
            self.get_volume() as f32 / 100.0
        }
    }

    /// 启动 pipeline — 创建 demux/decode/render/audio/subtitle 线程
    ///
    /// 接受已打开的 demuxer，避免重复打开文件。
    /// `subtitle_engine` 用于内嵌字幕流的解码和渲染。
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        url: &str,
        mut demuxer: Box<dyn Demuxer>,
        output: Arc<OutputSlot>,
        duration_secs: f64,
        video_info: Option<VideoInfo>,
        audio_info: Option<AudioInfo>,
        subtitle_engine: Option<Arc<Mutex<SubtitleEngine>>>,
        decode_policy: dyl_ffmpeg::hwaccel::Policy,
    ) -> dyl_core::Result<()> {
        tracing::info!(
            "Pipeline::start: url={}, duration={:.2}s",
            url,
            duration_secs
        );

        // 设置 duration
        self.duration_us
            .store((duration_secs * 1_000_000.0) as u64, Ordering::Release);

        // 网络来源的缓冲/重连不该触发本地的 2s 停滞判定
        self.input_stall_timeout = input_stall_grace(url);

        // 重置状态
        self.shutdown.store(false, Ordering::Release);
        self.demux_complete.store(false, Ordering::Release);
        self.demux_progress.store(0, Ordering::Release);
        self.seek_generation.store(0, Ordering::Release);
        self.position_us.store(0, Ordering::Release);
        self.decode_device = None;
        // 主时钟按新文件从零开始：AVSync 与本 pipeline 同生命周期，但 start 可能
        // 被同一实例再次调用，不重置就会拿着上一个文件的锚点与已接受的音频读数
        self.avsync.reset(0.0);

        let info = demuxer
            .info()
            .cloned()
            .ok_or_else(|| dyl_core::DylError::Demux("no media info".into()))?;

        // 获取 codec contexts
        let video_codec_ctx = demuxer.take_video_codec_context();
        let audio_codec_ctx = demuxer.take_audio_codec_context();
        let _subtitle_codec_ctx = demuxer.take_subtitle_codec_context();
        let has_video = video_codec_ctx.is_some() || video_info.is_some();
        let has_subtitle = info.subtitle_stream_index.is_some() && subtitle_engine.is_some();

        // 创建 channels
        // 载荷带 seek generation：seek 后在途的旧 generation 数据由消费端直接丢弃，
        // 无需排空通道也无法把旧位置的画面/声音播出来
        let (video_pkt_tx, video_pkt_rx) = bounded::<Stamped<dyl_ffmpeg::Packet>>(512);
        let (audio_pkt_tx, audio_pkt_rx) = bounded::<Stamped<dyl_ffmpeg::Packet>>(1024);
        let (video_frame_tx, video_frame_rx) = bounded::<Stamped<dyl_ffmpeg::VideoFrame>>(32);
        let (audio_frame_tx, audio_frame_rx) = bounded::<Stamped<dyl_ffmpeg::AudioFrame>>(64);

        // 字幕 packet channel（仅在有内嵌字幕时创建）
        let (subtitle_pkt_tx, subtitle_pkt_rx) = if has_subtitle {
            let (tx, rx) = bounded::<Stamped<dyl_ffmpeg::Packet>>(256);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // 构建线程共享的 Pipeline 引用
        // thread_handles 通过 Arc<Mutex<Vec>> 共享，stop() 可从 &self 调用
        let pipeline = Arc::new(Pipeline {
            avsync: self.avsync.clone(),
            seek_request: self.seek_request.clone(),
            seek_generation: self.seek_generation.clone(),
            position_us: self.position_us.clone(),
            seek_target_us: self.seek_target_us.clone(),
            duration_us: self.duration_us.clone(),
            demux_complete: self.demux_complete.clone(),
            ended: self.ended.clone(),
            demux_progress: self.demux_progress.clone(),
            shutdown: self.shutdown.clone(),
            paused: self.paused.clone(),
            input_stall_timeout: self.input_stall_timeout,
            volume: self.volume.clone(),
            muted: self.muted.clone(),
            event_tx: self.event_tx.clone(),
            session: self.session,
            // 只给引擎读（publish 时取 `decode_device()`），线程内不需要这份副本
            decode_device: None,
            thread_handles: self.thread_handles.clone(),
        });

        // 1. Demux 线程
        let p = pipeline.clone();
        let reporter = pipeline.clone();
        let h = std::thread::Builder::new()
            .name("dyl-demux".into())
            .spawn(move || {
                Self::run_thread(|| {
                    demux_loop(p, demuxer, video_pkt_tx, audio_pkt_tx, subtitle_pkt_tx)
                })
                .unwrap_or_else(|e| reporter.report_failure("demux thread", &e));
            })
            .map_err(|e| dyl_core::DylError::Internal(format!("spawn demux: {e}")))?;
        self.thread_handles.lock().push(h);

        // 2. Video decode 线程
        if has_video {
            let decoder = if let Some(ctx) = video_codec_ctx {
                FfmpegVideoDecoder::with_policy(ctx, decode_policy)?
            } else {
                FfmpegVideoDecoder::new()
            };
            self.decode_device = decoder.hardware_device();
            let video_decoder: Box<dyn VideoDecoder> = Box::new(decoder);
            let p = pipeline.clone();
            let reporter = pipeline.clone();
            let h = std::thread::Builder::new()
                .name("dyl-vdecode".into())
                .spawn(move || {
                    Self::run_thread(|| {
                        video_decode_loop(p, video_decoder, video_pkt_rx, video_frame_tx)
                    })
                    .unwrap_or_else(|e| reporter.report_failure("video decode thread", &e));
                })
                .map_err(|e| dyl_core::DylError::Internal(format!("spawn vdecode: {e}")))?;
            self.thread_handles.lock().push(h);
        }

        // 3. Audio decode 线程
        if audio_codec_ctx.is_some() || audio_info.is_some() {
            let audio_decoder: Box<dyn AudioDecoder> = if let Some(ctx) = audio_codec_ctx {
                Box::new(FfmpegAudioDecoder::from_stream_context(ctx)?)
            } else {
                Box::new(FfmpegAudioDecoder::new())
            };
            let p = pipeline.clone();
            let reporter = pipeline.clone();
            let h = std::thread::Builder::new()
                .name("dyl-adecode".into())
                .spawn(move || {
                    Self::run_thread(|| {
                        audio_decode_loop(p, audio_decoder, audio_pkt_rx, audio_frame_tx)
                    })
                    .unwrap_or_else(|e| reporter.report_failure("audio decode thread", &e));
                })
                .map_err(|e| dyl_core::DylError::Internal(format!("spawn adecode: {e}")))?;
            self.thread_handles.lock().push(h);
        }

        // 4. Render 线程
        {
            let p = pipeline.clone();
            let reporter = pipeline.clone();
            let output = output.clone();
            let h = std::thread::Builder::new()
                .name("dyl-render".into())
                .spawn(move || {
                    Self::run_thread(|| {
                        render_loop(p, output, video_frame_rx, duration_secs, has_video)
                    })
                    .unwrap_or_else(|e| reporter.report_failure("render thread", &e));
                })
                .map_err(|e| dyl_core::DylError::Internal(format!("spawn render: {e}")))?;
            self.thread_handles.lock().push(h);
        }

        // 5. Audio output 线程
        {
            tracing::info!("Pipeline::start: creating audio output...");
            let mut audio_output: Box<dyn AudioOutput> = create_default_output();
            if let Some(ref ai) = audio_info {
                audio_output.init(ai.sample_rate, ai.channels)?;
            } else if info.sample_rate > 0 {
                audio_output.init(info.sample_rate, info.channels)?;
            }
            tracing::info!("Pipeline::start: audio output initialized, spawning thread...");
            // 音频输出即主时钟来源；无音频轨时时钟未 ready，AVSync 自动回退挂钟
            self.avsync.set_audio_clock(audio_output.clock());
            let p = pipeline.clone();
            let reporter = pipeline.clone();
            let h = std::thread::Builder::new()
                .name("dyl-audio".into())
                .spawn(move || {
                    tracing::info!("audio thread closure entered");
                    Self::run_thread(|| audio_output_loop(p, audio_output, audio_frame_rx))
                        .unwrap_or_else(|e| reporter.report_failure("audio output thread", &e));
                })
                .map_err(|e| dyl_core::DylError::Internal(format!("spawn audio: {e}")))?;
            self.thread_handles.lock().push(h);
            tracing::info!("Pipeline::start: audio thread spawned");
        }

        // 6. Subtitle decode 线程（仅在有内嵌字幕时启动）
        if let (Some(subtitle_pkt_rx), Some(sub_engine)) = (subtitle_pkt_rx, subtitle_engine) {
            // 内封 ASS/SSA 的样式表与 PlayRes 在字幕流的 CodecPrivate（脚本头）里，
            // 事件本身由解码线程逐包喂入。不先装样式，自建渲染器只能落回 config
            // 默认值 —— 字号、颜色、描边、边距就不是字幕作者设定的那套。
            if let Some(script) = info.subtitle_script.as_deref() {
                sub_engine.lock().apply_embedded_script(script);
            }
            let codec = info.subtitle_codec_id;
            let p = pipeline.clone();
            let reporter = pipeline.clone();
            let h = std::thread::Builder::new()
                .name("dyl-subtitle".into())
                .spawn(move || {
                    Self::run_thread(|| {
                        subtitle_decode_loop(p, subtitle_pkt_rx, sub_engine, codec)
                    })
                    .unwrap_or_else(|e| reporter.report_failure("subtitle thread", &e));
                })
                .map_err(|e| dyl_core::DylError::Internal(format!("spawn subtitle: {e}")))?;
            self.thread_handles.lock().push(h);
            tracing::info!("Pipeline::start: subtitle thread spawned");
        }

        tracing::info!(
            "Pipeline started: {} threads",
            self.thread_handles.lock().len()
        );
        Ok(())
    }

    /// 仅置位 shutdown 标志，不 join 线程
    ///
    /// 供 FFI 层使用：position reporter 等后台线程依赖 shutdown 退出，必须在
    /// join 它们之前发出信号，否则 join 会永久阻塞（stop() 内部才置位就太晚了）。
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// 停止 pipeline — 设置 shutdown 标志并等待所有线程退出
    ///
    /// 可通过 `&self` 调用，因为 thread_handles 通过 `Arc<Mutex<Vec>>` 共享。
    pub fn stop(&self) {
        tracing::info!("Pipeline::stop");
        self.request_shutdown();

        // 取出 thread handles 并逐一 join
        let handles: Vec<_> = self.thread_handles.lock().drain(..).collect();
        for handle in handles {
            let _ = handle.join();
        }
        tracing::info!("Pipeline::stop: all threads joined");
    }

    /// 线程运行包装器 — 捕获 panic
    fn run_thread<F>(f: F) -> dyl_core::Result<()>
    where
        F: FnOnce() -> dyl_core::Result<()>,
    {
        match std::panic::catch_unwind(AssertUnwindSafe(f)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(panic_payload) => {
                let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                tracing::error!("thread panic: {}", msg);
                Err(dyl_core::DylError::Internal(format!(
                    "thread panic: {}",
                    msg
                )))
            }
        }
    }

    /// 工作线程失败上报
    ///
    /// 只写日志的话，前端看到的状态还停在 `Playing`，画面却早已静止；错误必须
    /// 作为事件出去，GUI 才能显示错误界面并把状态收敛到 `Error`。
    fn report_failure(&self, stage: &str, e: &dyl_core::DylError) {
        tracing::error!("{stage} error: {e}");
        self.emit(PipelineEvent::Error {
            code: ErrorCode::from(e),
            message: format!("{stage}: {e}"),
        });
    }

    /// 看门狗上报：某级流水线在较长时间内毫无进展
    ///
    /// 卡死在 GUI 上的表现只是"画面静止"：线程还活着、状态还是 `Playing`、命令
    /// 也都返回成功。停滞必须自己成为错误事件，否则前端只能永远等下去。
    pub(crate) fn report_stall(&self, stage: &str, stalled: std::time::Duration) {
        let message = format!("{stage}: stalled for {:.1}s", stalled.as_secs_f64());
        tracing::error!("watchdog: {message}");
        self.emit(PipelineEvent::Error {
            code: ErrorCode::Pipeline,
            message,
        });
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // stop() 已经是 &self 方法，直接调用
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pipeline() -> Pipeline {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        Pipeline::new(tx, 0)
    }

    /// 线程失败必须成为事件：日志只给开发者，前端需要能看见
    #[test]
    fn report_failure_emits_error_event() {
        let (tx, rx) = crossbeam_channel::bounded(8);
        let p = Pipeline::new(tx, 7);
        p.report_failure(
            "video decode thread",
            &dyl_core::DylError::Decode("boom".into()),
        );
        match rx.recv().expect("必须发出一个事件") {
            PipelineEventMsg {
                session: 7,
                event: PipelineEvent::Error { code, message },
            } => {
                assert_eq!(code, ErrorCode::Decode);
                assert!(message.contains("video decode thread"), "{message}");
                assert!(message.contains("boom"), "{message}");
            }
            other => panic!("期望 Error 事件，got {other:?}"),
        }
    }

    /// 看门狗上报：停滞同样要成为可见的错误事件
    #[test]
    fn report_stall_emits_pipeline_error_event() {
        let (tx, rx) = crossbeam_channel::bounded(8);
        let p = Pipeline::new(tx, 0);
        p.report_stall("video decode", std::time::Duration::from_millis(2_100));
        match rx.recv().expect("必须发出一个事件") {
            PipelineEventMsg {
                event: PipelineEvent::Error { code, message },
                ..
            } => {
                assert_eq!(code, ErrorCode::Pipeline);
                assert!(message.contains("video decode"), "{message}");
                assert!(message.contains("2.1"), "{message}");
            }
            other => panic!("期望 Error 事件，got {other:?}"),
        }
    }

    #[test]
    fn test_new_pipeline_defaults() {
        let p = make_pipeline();
        assert!((p.current_position_secs() - 0.0).abs() < f64::EPSILON);
        assert!((p.duration_secs() - 0.0).abs() < f64::EPSILON);
        assert!(!p.is_paused());
        assert_eq!(p.get_volume(), 100);
        assert!(!p.is_muted());
        assert!((p.effective_volume() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_set_mute_keeps_volume() {
        let p = make_pipeline();
        p.set_volume(60);
        p.set_mute(true);
        assert!(p.is_muted());
        assert_eq!(p.get_volume(), 60);
        assert_eq!(p.effective_volume(), 0.0);

        p.set_mute(false);
        assert!(!p.is_muted());
        assert!((p.effective_volume() - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn test_pause_resume() {
        let p = make_pipeline();
        assert!(!p.is_paused());

        p.pause();
        assert!(p.is_paused());

        p.resume();
        assert!(!p.is_paused());
    }

    #[test]
    fn test_set_volume() {
        let p = make_pipeline();
        p.set_volume(50);
        assert_eq!(p.get_volume(), 50);

        p.set_volume(0);
        assert_eq!(p.get_volume(), 0);

        p.set_volume(100);
        assert_eq!(p.get_volume(), 100);
    }

    #[test]
    fn test_position_tracking() {
        let p = make_pipeline();
        // 模拟位置更新 (5秒 = 5_000_000 微秒)
        p.position_us.store(5_000_000, Ordering::Release);
        assert!((p.current_position_secs() - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_duration_tracking() {
        let p = make_pipeline();
        // 模拟时长 (120秒 = 120_000_000 微秒)
        p.duration_us.store(120_000_000, Ordering::Release);
        assert!((p.duration_secs() - 120.0).abs() < 0.001);
    }

    #[test]
    fn test_seek_request() {
        let p = make_pipeline();
        assert!(p.seek_request.lock().is_none());

        *p.seek_request.lock() = Some(SeekRequest {
            target: 30.0,
            from: 10.0,
            tag: 7,
            exact: true,
        });
        let req = p.seek_request.lock().take();
        let req = req.expect("seek request must be stored");
        assert!((req.target - 30.0).abs() < f64::EPSILON);
        assert_eq!(req.tag, 7);
    }

    #[test]
    fn test_seek_generation() {
        let p = make_pipeline();
        assert_eq!(p.seek_generation.load(Ordering::Acquire), 0);

        p.seek_generation.fetch_add(1, Ordering::Release);
        assert_eq!(p.seek_generation.load(Ordering::Acquire), 1);

        p.seek_generation.fetch_add(1, Ordering::Release);
        assert_eq!(p.seek_generation.load(Ordering::Acquire), 2);
    }

    #[test]
    fn test_shutdown_flag() {
        let p = make_pipeline();
        assert!(!p.shutdown.load(Ordering::Acquire));

        p.shutdown.store(true, Ordering::Release);
        assert!(p.shutdown.load(Ordering::Acquire));
    }

    #[test]
    fn test_stop_with_no_threads() {
        // 没有线程的 pipeline 调用 stop 不应 panic
        let p = make_pipeline();
        p.stop();
    }

    #[test]
    fn test_resume_resets_avsync() {
        let p = make_pipeline();
        p.position_us.store(10_000_000, Ordering::Release); // 10s
        p.resume(); // 应重置 avsync 到当前位置
                    // resume 后 is_paused 应为 false
        assert!(!p.is_paused());
    }
}
