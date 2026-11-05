//! 引擎门面 — 创建控制线程、命令入队、Surface 串行化与销毁。
//!
//! 控制线程独占状态写入；本文件只做同步校验、通道投递和 Surface 生命周期。
//! Surface 操作会阻塞在 GPU 初始化 / 渲染线程 ack 上，因此必须与命令通道
//! 分离：`attach_surface` 创建成功后才换入 `OutputSlot`，失败保留旧输出。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use dyl_core::contract::{
    Command, CommandEnvelope, CommandOp, DanmakuSnapshot, DecodeBackend, DecodeMode,
    DecodeSnapshot, ErrorCode, Event, EventEnvelope, PlayerState, Snapshot, StatsSnapshot,
    SubtitleSnapshot, SurfaceAttached, SurfaceDescriptor, SurfaceDetached, SurfaceResized,
    SurfaceStatus, TrackKind, SCHEMA_VERSION,
};
use dyl_pipeline::{Pipeline, PipelineEventMsg};
use dyl_render::{NullOutput, OutputSlot, OutputStats, OverlayClock, VideoOutput};
use parking_lot::{Mutex, RwLock};

use crate::control::spawn_control;
use crate::error::EngineError;
use crate::load::LoadOutcome;
use crate::output::{
    create_output, ensure_supported, surface_handle, HostFrameSink, HostOutputHandles,
    HostVideoOutput, SurfaceRuntime,
};
use crate::overlays::{OverlayEngines, OverlayHandles, OverlayKind};

/// Surface 事件发布后等待控制线程确认的预算
const SURFACE_PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) enum ControlMsg {
    Command {
        request_id: u64,
        command: Command,
    },
    LoadDone(LoadOutcome),
    OverlayDone(OverlayOutcome),
    /// Surface 操作已改完 `OutputSlot` 与运行时状态；控制线程负责发布快照与事件
    SurfaceChanged {
        event: Option<Event>,
        ack: Option<Sender<()>>,
    },
    Shutdown,
}

pub(crate) struct OverlayOutcome {
    pub request_id: u64,
    pub op: CommandOp,
    pub path: String,
    pub kind: OverlayKind,
    pub result: Result<usize, EngineError>,
}

pub(crate) struct EngineShared {
    pub(crate) control_tx: Sender<ControlMsg>,
    pub(crate) pipe_tx: Sender<PipelineEventMsg>,
    pub(crate) output: Arc<OutputSlot>,
    pub(crate) overlays: OverlayEngines,
    pub(crate) font_loaded: AtomicBool,
    /// host 模式下 engine 与 shell 共享的叠加层时钟与统计
    host_clock: Arc<OverlayClock>,
    host_stats: Arc<OutputStats>,
    snapshot: RwLock<Arc<Snapshot>>,
    pub(crate) surface: Mutex<SurfaceRuntime>,
    /// attach/detach/resize 必须串行：三者都改 `OutputSlot` 与 Surface 运行时状态
    surface_lock: Mutex<()>,
    in_flight: Mutex<HashSet<u64>>,
    next_request_id: AtomicU64,
    retired: Mutex<Vec<JoinHandle<()>>>,
    /// 起不了 retire 线程时的兜底：销毁阶段统一 stop，绝不在控制线程上 join
    orphan_pipelines: Mutex<Vec<Arc<Pipeline>>>,
    /// detach 失败时原生句柄仍归内核所有，留到销毁阶段再释放
    retired_outputs: Mutex<Vec<Box<dyn VideoOutput>>>,
    shutting_down: AtomicBool,
}

impl EngineShared {
    fn new(control_tx: Sender<ControlMsg>, pipe_tx: Sender<PipelineEventMsg>) -> Self {
        let snapshot = Snapshot {
            schema: SCHEMA_VERSION,
            state: PlayerState::Idle,
            position_ms: 0,
            duration_ms: 0,
            volume: 100,
            muted: false,
            decode: DecodeSnapshot {
                mode: DecodeMode::Software,
                backend: DecodeBackend::Software,
                device: None,
            },
            source: None,
            media: None,
            subtitle: SubtitleSnapshot {
                loaded: false,
                visible: true,
                source: None,
            },
            danmaku: DanmakuSnapshot {
                loaded: false,
                visible: true,
                opacity: 100,
                count: 0,
                source: None,
            },
            surface: SurfaceRuntime::default().snapshot(),
            stats: StatsSnapshot {
                rendered_frames: 0,
                dropped_frames: 0,
            },
            last_error: None,
        };
        Self {
            control_tx,
            pipe_tx,
            output: OutputSlot::null(),
            overlays: OverlayEngines::new(),
            font_loaded: AtomicBool::new(false),
            host_clock: Arc::new(OverlayClock::new()),
            host_stats: Arc::new(OutputStats::new()),
            snapshot: RwLock::new(Arc::new(snapshot)),
            surface: Mutex::new(SurfaceRuntime::default()),
            surface_lock: Mutex::new(()),
            in_flight: Mutex::new(HashSet::new()),
            next_request_id: AtomicU64::new(0),
            retired: Mutex::new(Vec::new()),
            orphan_pipelines: Mutex::new(Vec::new()),
            retired_outputs: Mutex::new(Vec::new()),
            shutting_down: AtomicBool::new(false),
        }
    }

    pub(crate) fn publish(&self, snapshot: Snapshot) {
        *self.snapshot.write() = Arc::new(snapshot);
    }

    pub(crate) fn finish_command(&self, request_id: u64) {
        self.in_flight.lock().remove(&request_id);
    }

    /// 在后台线程停止旧管线，控制线程不做阻塞 join
    pub(crate) fn retire_pipeline(&self, pipeline: Arc<Pipeline>) {
        let worker = pipeline.clone();
        let spawned = std::thread::Builder::new()
            .name("dyl-retire".into())
            .spawn(move || worker.stop());
        match spawned {
            Ok(handle) => {
                let mut retired = self.retired.lock();
                retired.retain(|handle| !handle.is_finished());
                retired.push(handle);
            }
            Err(e) => {
                tracing::warn!("spawn pipeline retire thread failed: {e}");
                self.orphan_pipelines.lock().push(pipeline);
            }
        }
    }
}

pub struct Engine {
    shared: Arc<EngineShared>,
    control: Mutex<Option<JoinHandle<()>>>,
}

impl Engine {
    /// 创建引擎并返回公开事件流；事件流由 FFI / shell 独占消费
    pub fn new() -> Result<(Self, Receiver<EventEnvelope>), EngineError> {
        let (control_tx, control_rx) = bounded(64);
        let (pipe_tx, pipe_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();

        let shared = Arc::new(EngineShared::new(control_tx, pipe_tx));
        let control =
            spawn_control(shared.clone(), control_rx, pipe_rx, event_tx).map_err(|e| {
                EngineError::new(
                    ErrorCode::Internal,
                    format!("spawn engine control thread: {e}"),
                )
            })?;

        Ok((
            Self {
                shared,
                control: Mutex::new(Some(control)),
            },
            event_rx,
        ))
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.shared.snapshot.read().clone()
    }

    /// 切换到 host compositor：pipeline 的帧交给 `sink`，内核不再负责 present。
    ///
    /// 若已有 external Surface，会先拆掉它并发布 `surface_detached`；
    /// 两种呈现模式互斥。
    pub fn set_host_frame_sink(&self, sink: Arc<dyn HostFrameSink>) {
        if self.shared.shutting_down.load(Ordering::Acquire) {
            tracing::warn!("set_host_frame_sink ignored: engine is shutting down");
            return;
        }
        let _guard = self.shared.surface_lock.lock();
        let output = HostVideoOutput::new(
            sink,
            self.shared.host_clock.clone(),
            self.shared.host_stats.clone(),
        );
        let old = self.shared.output.replace(Box::new(output));
        let was_attached = {
            let mut runtime = self.shared.surface.lock();
            if runtime.status == SurfaceStatus::Attached {
                runtime.status = SurfaceStatus::Detached;
                true
            } else {
                false
            }
        };
        let kind = self.shared.surface.lock().kind;
        self.retire_replaced_output(old, "host_sink");
        if was_attached {
            if let Some(kind) = kind {
                self.notify_control(Some(Event::SurfaceDetached(SurfaceDetached { kind })));
            }
        }
    }

    /// host 模式下 shell 与 `HostVideoOutput` 共用的时钟与统计句柄。
    pub fn host_output_handles(&self) -> HostOutputHandles {
        HostOutputHandles {
            overlay_clock: self.shared.host_clock.clone(),
            stats: self.shared.host_stats.clone(),
        }
    }

    /// shell 构建 OverlayStack 用的共享字幕/弹幕引擎。
    pub fn overlay_handles(&self) -> OverlayHandles {
        self.shared.overlays.handles()
    }

    pub fn output_stats(&self) -> Arc<OutputStats> {
        self.shared.host_stats.clone()
    }

    /// 命令入队。同步只校验形状、范围与静态能力；状态错误异步经 `command_completed` 报告。
    pub fn command(&self, envelope: CommandEnvelope) -> Result<u64, EngineError> {
        if self.shared.shutting_down.load(Ordering::Acquire) {
            return Err(EngineError::invalid_state("engine is shutting down"));
        }
        if envelope.schema != SCHEMA_VERSION {
            return Err(EngineError::invalid_argument(format!(
                "schema must be {SCHEMA_VERSION}, got {}",
                envelope.schema
            )));
        }
        let command = envelope.command;
        validate(&command)?;
        let request_id = self.reserve_request_id(envelope.request_id)?;

        match self.shared.control_tx.try_send(ControlMsg::Command {
            request_id,
            command,
        }) {
            Ok(()) => Ok(request_id),
            Err(TrySendError::Full(_)) => {
                self.shared.finish_command(request_id);
                Err(EngineError::new(ErrorCode::Busy, "command queue is full"))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.shared.finish_command(request_id);
                Err(EngineError::invalid_state("engine control thread is gone"))
            }
        }
    }

    fn reserve_request_id(&self, requested: Option<u64>) -> Result<u64, EngineError> {
        let mut in_flight = self.shared.in_flight.lock();
        let request_id = match requested {
            Some(id) => {
                if id == 0 || id > i64::MAX as u64 {
                    return Err(EngineError::invalid_argument(
                        "request_id must be a positive int64",
                    ));
                }
                if in_flight.contains(&id) {
                    return Err(EngineError::invalid_argument(format!(
                        "request_id {id} is already in flight"
                    )));
                }
                id
            }
            None => loop {
                let id = self
                    .shared
                    .next_request_id
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1);
                if id != 0 && !in_flight.contains(&id) {
                    break id;
                }
            },
        };
        in_flight.insert(request_id);
        Ok(request_id)
    }

    /// 事务性 attach：先建新输出，成功后再换入并 detach 旧输出
    pub fn attach_surface(&self, desc: SurfaceDescriptor) -> Result<(), EngineError> {
        if self.shared.shutting_down.load(Ordering::Acquire) {
            return Err(EngineError::invalid_state("engine is shutting down"));
        }
        let kind = desc.kind();
        ensure_supported(kind)?;
        let handle = surface_handle(&desc)?;
        let (width, height) = desc.size();
        let scale_factor = desc.scale_factor();

        let _guard = self.shared.surface_lock.lock();
        let renderer = create_output(handle, &self.shared.overlays)?;
        let old = self.shared.output.replace(Box::new(renderer));
        *self.shared.surface.lock() = SurfaceRuntime::attached(kind, width, height, scale_factor);
        self.retire_replaced_output(old, "attach");

        self.notify_control(Some(Event::SurfaceAttached(SurfaceAttached {
            kind,
            width,
            height,
            scale_factor,
        })));
        Ok(())
    }

    /// 同步 detach：返回 Ok 表示渲染线程已释放 `wgpu::Surface`
    pub fn detach_surface(&self) -> Result<(), EngineError> {
        let _guard = self.shared.surface_lock.lock();
        if self.shared.surface.lock().status != SurfaceStatus::Attached {
            return Ok(());
        }

        let mut old = self.shared.output.replace(Box::new(NullOutput::new()));
        match old.detach() {
            Ok(()) => {
                drop(old);
                let kind = {
                    let mut runtime = self.shared.surface.lock();
                    runtime.status = SurfaceStatus::Detached;
                    runtime.kind
                };
                if let Some(kind) = kind {
                    self.notify_control(Some(Event::SurfaceDetached(SurfaceDetached { kind })));
                }
                Ok(())
            }
            Err(e) => {
                self.shared.surface.lock().status = SurfaceStatus::Error;
                // detach 失败意味着渲染线程没有确认释放，句柄仍归内核
                self.shared.retired_outputs.lock().push(old);
                self.notify_control(None);
                Err(detach_error(e))
            }
        }
    }

    pub fn resize_surface(&self, width: u32, height: u32) -> Result<(), EngineError> {
        if width == 0 || height == 0 {
            return Err(EngineError::invalid_argument(format!(
                "surface size must be non-zero, got {width}x{height}"
            )));
        }
        let _guard = self.shared.surface_lock.lock();
        if self.shared.surface.lock().status != SurfaceStatus::Attached {
            return Err(EngineError::invalid_state(
                "surface_resize requires an attached surface",
            ));
        }

        self.shared
            .output
            .resize(width, height)
            .map_err(EngineError::from)?;
        {
            let mut runtime = self.shared.surface.lock();
            runtime.width = width;
            runtime.height = height;
        }
        self.notify_control(Some(Event::SurfaceResized(SurfaceResized {
            width,
            height,
        })));
        Ok(())
    }

    /// 请求把当前画面重新上屏一次。
    ///
    /// 暂停/EOF 后渲染线程不再 present；宿主窗口若被系统擦除（Windows 子窗口
    /// 重绘、窗口恢复等），需要显式触发一次重绘才能恢复静止画面。未 attach
    /// Surface 时为 no-op。
    pub fn redraw_surface(&self) -> Result<(), EngineError> {
        let _guard = self.shared.surface_lock.lock();
        if self.shared.surface.lock().status != SurfaceStatus::Attached {
            return Ok(());
        }
        self.shared.output.redraw().map_err(EngineError::from)
    }

    /// 停止控制线程、拆除输出并回收所有后台线程（幂等）
    pub fn destroy(&mut self) {
        if self.shared.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.shared.control_tx.send(ControlMsg::Shutdown);
        if let Some(handle) = self.control.lock().take() {
            let _ = handle.join();
        }

        {
            let _guard = self.shared.surface_lock.lock();
            let mut old = self.shared.output.replace(Box::new(NullOutput::new()));
            if let Err(e) = old.detach() {
                tracing::warn!("destroy: detach current output failed: {e}");
            }
            drop(old);
            self.shared.surface.lock().status = SurfaceStatus::Detached;
        }

        let retired: Vec<_> = self.shared.retired.lock().drain(..).collect();
        for handle in retired {
            let _ = handle.join();
        }
        let orphans: Vec<_> = self.shared.orphan_pipelines.lock().drain(..).collect();
        for pipeline in orphans {
            pipeline.stop();
        }
        let mut outputs = self.shared.retired_outputs.lock();
        for mut output in outputs.drain(..) {
            if let Err(e) = output.detach() {
                tracing::warn!("destroy: retired output detach failed: {e}");
            }
        }
    }

    fn retire_replaced_output(&self, mut old: Box<dyn VideoOutput>, what: &str) {
        match old.detach() {
            Ok(()) => drop(old),
            Err(e) => {
                tracing::warn!("{what}: old output detach failed: {e}; retaining until destroy");
                self.shared.retired_outputs.lock().push(old);
            }
        }
    }

    /// Surface 操作返回前让控制线程发布快照与事件；超时只告警，操作本身已生效
    fn notify_control(&self, event: Option<Event>) {
        let (ack_tx, ack_rx) = bounded(1);
        let sent = self.shared.control_tx.send(ControlMsg::SurfaceChanged {
            event,
            ack: Some(ack_tx),
        });
        if sent.is_err() {
            return;
        }
        if ack_rx.recv_timeout(SURFACE_PUBLISH_TIMEOUT).is_err() {
            tracing::warn!("surface change: control thread did not publish within 5s");
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn validate(command: &Command) -> Result<(), EngineError> {
    match command {
        Command::LoadFile { url } if url.trim().is_empty() => Err(EngineError::invalid_argument(
            "load_file.url must not be empty",
        )),
        // 来源协议是静态能力（`capabilities.protocols`），和 seek 模式一样在同步阶段
        // 拒绝：不认识的 scheme / 本构建没有 TLS 时报 not_supported，而不是等到
        // load worker 里被文件存在性预检误报成 file_not_found
        Command::LoadFile { url } => match dyl_ffmpeg::classify_source(url) {
            Ok(_) => Ok(()),
            Err(e) => Err(e.into()),
        },
        Command::LoadSubtitle { path } if path.trim().is_empty() => Err(
            EngineError::invalid_argument("load_subtitle.path must not be empty"),
        ),
        Command::LoadDanmaku { path } if path.trim().is_empty() => Err(
            EngineError::invalid_argument("load_danmaku.path must not be empty"),
        ),
        Command::SetVolume { volume } if *volume > 100 => Err(EngineError::invalid_argument(
            format!("volume must be 0-100, got {volume}"),
        )),
        Command::SetDanmakuOpacity { opacity } if *opacity > 100 => Err(
            EngineError::invalid_argument(format!("opacity must be 0-100, got {opacity}")),
        ),
        Command::Seek { mode, .. } | Command::SeekRelative { mode, .. } => {
            if !dyl_pipeline::capabilities::SEEK_MODES.contains(mode) {
                Err(EngineError::new(
                    ErrorCode::NotSupported,
                    format!("seek mode {mode:?} is not declared by dyl-pipeline"),
                ))
            } else {
                Ok(())
            }
        }
        // 换视频轨要连带动分辨率、像素格式、Surface 尺寸与 `OverlayArea`，当前构建
        // 没有声明这个能力，按 `seek` 模式的先例在同步阶段拒绝
        Command::SelectTrack { kind, .. } if *kind == TrackKind::Video => Err(EngineError::new(
            ErrorCode::NotSupported,
            "select_track kind video is not implemented",
        )),
        // `auto` 是静态能力（`features.hw_decode`）：没有实测过硬解的平台在同步阶段
        // 拒绝，客户端因此不会摆出一个切过去也永远软解的开关。`software` 永远可用。
        Command::SetDecodeMode {
            mode: DecodeMode::Auto,
        } if !dyl_ffmpeg::capabilities::HW_DECODE_VERIFIED => Err(EngineError::new(
            ErrorCode::NotSupported,
            "hardware decoding is not verified on this platform",
        )),
        _ => Ok(()),
    }
}

fn detach_error(e: dyl_core::DylError) -> EngineError {
    let mut error = EngineError::from(e);
    // 渲染线程未在预算内确认 = 超时；句柄所有权仍在内核
    if error.message.contains("did not acknowledge within") {
        error.code = ErrorCode::Timeout;
    }
    error
}
