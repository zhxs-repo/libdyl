//! 引擎控制线程 — 串行处理命令、pipeline 内部事件、load 结果与 10Hz tick。
//!
//! 这里只做状态校验、非阻塞调用与事件/快照发布；打开文件、加载字体、创建 GPU
//! 输出都在 worker 线程，避免把 UI 线程的控制通道堵在 I/O 上。

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandCompleted, CommandOp, CommandStatus, DanmakuSnapshot, DecodeBackend,
    DecodeMode, DecodeSnapshot, ErrorCode, ErrorEvent, ErrorInfo, Event, FileLoaded, MediaInfo,
    PlayerState, PositionChanged, SeekCompleted, SeekMode, SettingsChanged, Snapshot,
    SnapshotError, StateChangeReason, StateChanged, StatsSnapshot, SubtitleSnapshot, TrackKind,
    Warning, SCHEMA_VERSION,
};
use dyl_ffmpeg::TrackSelection;
use dyl_pipeline::pipeline::SeekRequest;
use dyl_pipeline::{Pipeline, PipelineEvent, PipelineEventMsg};

use crate::engine::{ControlMsg, EngineShared, OverlayOutcome};
use crate::error::EngineError;
use crate::events::EventBus;
use crate::load::{spawn as spawn_load, LoadJob, LoadOutcome, LoadPurpose};
use crate::overlays::OverlayKind;
use crate::state;

const TICK: Duration = Duration::from_millis(100);

struct PendingLoad {
    request_id: u64,
    op: CommandOp,
}

struct PendingRestart {
    request_id: u64,
    op: CommandOp,
}

#[derive(Clone)]
struct PendingSeek {
    request_id: u64,
    op: CommandOp,
    tag: u64,
    from_ms: u64,
    to_ms: u64,
    mode: SeekMode,
}

/// 换轨后待恢复的播放状态。
///
/// `seek_tag` 非 `None` 时位置钉在这里，直到新管线把恢复用的 seek 真正应用：
/// 刚重开的管线一个画面都没出，`live_position_ms()` 会读到 0，不钉住的话进度条
/// 会在换轨瞬间掉回 00:00 再弹回来——正是 seek storm 盯着的那个回跳。
#[derive(Clone)]
struct PendingSwitch {
    resume: PlayerState,
    position_ms: u64,
    seek_tag: Option<u64>,
}

pub(crate) struct ControlState {
    shared: Arc<EngineShared>,
    events: EventBus,

    state: PlayerState,
    source: Option<String>,
    media: Option<MediaInfo>,
    duration_ms: u64,
    position_ms: u64,
    volume: u8,
    muted: bool,
    /// 解码方式设置；实际后端由活管线的 `decode_device()` 决定
    decode_mode: DecodeMode,
    subtitle_visible: bool,
    danmaku_visible: bool,
    danmaku_opacity: u8,
    subtitle_loaded: bool,
    subtitle_source: Option<String>,
    danmaku_loaded: bool,
    danmaku_source: Option<String>,
    danmaku_count: u64,
    last_error: Option<SnapshotError>,

    pipeline: Option<Arc<Pipeline>>,
    session: u64,
    next_session: u64,
    load_generation: u64,
    seek_tag_seq: u64,

    pending_load: Option<PendingLoad>,
    pending_restart: Option<PendingRestart>,
    pending_seek: Option<PendingSeek>,
    pending_switch: Option<PendingSwitch>,
    superseded_seeks: Vec<PendingSeek>,
}

impl ControlState {
    fn new(
        shared: Arc<EngineShared>,
        event_tx: crossbeam_channel::Sender<dyl_core::contract::EventEnvelope>,
    ) -> Self {
        Self {
            shared,
            events: EventBus::new(event_tx),
            state: PlayerState::Idle,
            source: None,
            media: None,
            duration_ms: 0,
            position_ms: 0,
            volume: 100,
            muted: false,
            // 默认软解：与硬解接入前的行为逐字节一致，用户显式切到 auto 才试硬件
            decode_mode: DecodeMode::Software,
            subtitle_visible: true,
            danmaku_visible: true,
            danmaku_opacity: 100,
            subtitle_loaded: false,
            subtitle_source: None,
            danmaku_loaded: false,
            danmaku_source: None,
            danmaku_count: 0,
            last_error: None,
            pipeline: None,
            session: 0,
            next_session: 0,
            load_generation: 0,
            seek_tag_seq: 0,
            pending_load: None,
            pending_restart: None,
            pending_seek: None,
            pending_switch: None,
            superseded_seeks: Vec::new(),
        }
    }

    fn run(mut self, cmd_rx: Receiver<ControlMsg>, pipe_rx: Receiver<PipelineEventMsg>) {
        loop {
            crossbeam_channel::select! {
                recv(cmd_rx) -> msg => match msg {
                    Ok(ControlMsg::Shutdown) => break,
                    Ok(msg) => self.handle(msg),
                    Err(_) => break,
                },
                recv(pipe_rx) -> msg => {
                    if let Ok(msg) = msg {
                        self.handle_pipeline_event(msg);
                    }
                }
                default(TICK) => self.on_tick(),
            }
        }
        self.retire_current_pipeline();
    }

    fn handle(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::Command {
                request_id,
                command,
            } => self.handle_command(request_id, command),
            ControlMsg::LoadDone(outcome) => self.handle_load_done(outcome),
            ControlMsg::OverlayDone(outcome) => self.handle_overlay_done(outcome),
            ControlMsg::SurfaceChanged { event, ack } => {
                self.publish();
                if let Some(event) = event {
                    self.events.emit(event);
                }
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
            ControlMsg::Shutdown => {}
        }
    }

    // ── 命令 ────────────────────────────────────────────────────

    fn handle_command(&mut self, request_id: u64, command: Command) {
        let op = command.op();
        if !state::accepts(op, self.state) {
            let error = EngineError::invalid_state(format!(
                "{} is not allowed in {:?}",
                op.as_str(),
                self.state
            ));
            self.complete_error(request_id, op, &error);
            return;
        }

        match command {
            Command::LoadFile { url } => self.cmd_load_file(request_id, url),
            Command::Play => self.cmd_play(request_id),
            Command::Pause => self.cmd_pause(request_id),
            Command::Stop => self.cmd_stop(request_id),
            Command::Seek { position_ms, mode } => {
                self.cmd_seek(request_id, CommandOp::Seek, Some(position_ms), None, mode)
            }
            Command::SeekRelative { delta_ms, mode } => self.cmd_seek(
                request_id,
                CommandOp::SeekRelative,
                None,
                Some(delta_ms),
                mode,
            ),
            Command::SetVolume { volume } => {
                self.volume = volume;
                if let Some(pipeline) = &self.pipeline {
                    pipeline.set_volume(volume);
                }
                self.publish();
                self.emit_settings();
                self.complete_ok(request_id, CommandOp::SetVolume);
            }
            Command::SetMute { muted } => {
                self.muted = muted;
                if let Some(pipeline) = &self.pipeline {
                    pipeline.set_mute(muted);
                }
                self.publish();
                self.emit_settings();
                self.complete_ok(request_id, CommandOp::SetMute);
            }
            Command::SetSubtitleVisible { visible } => {
                self.subtitle_visible = visible;
                self.shared.overlays.set_subtitle_visible(visible);
                self.publish();
                self.emit_settings();
                self.complete_ok(request_id, CommandOp::SetSubtitleVisible);
            }
            Command::SetDanmakuVisible { visible } => {
                self.danmaku_visible = visible;
                self.shared.overlays.set_danmaku_visible(visible);
                self.publish();
                self.emit_settings();
                self.complete_ok(request_id, CommandOp::SetDanmakuVisible);
            }
            Command::SetDanmakuOpacity { opacity } => {
                self.danmaku_opacity = opacity;
                self.shared.overlays.set_danmaku_opacity(opacity);
                self.publish();
                self.emit_settings();
                self.complete_ok(request_id, CommandOp::SetDanmakuOpacity);
            }
            Command::LoadSubtitle { path } => self.cmd_load_overlay(
                request_id,
                CommandOp::LoadSubtitle,
                path,
                OverlayKind::Subtitle,
            ),
            Command::LoadDanmaku { path } => self.cmd_load_overlay(
                request_id,
                CommandOp::LoadDanmaku,
                path,
                OverlayKind::Danmaku,
            ),
            Command::SelectTrack { kind, index } => self.cmd_select_track(request_id, kind, index),
            Command::SetDecodeMode { mode } => self.cmd_set_decode_mode(request_id, mode),
        }
    }

    fn cmd_load_file(&mut self, request_id: u64, url: String) {
        self.cancel_pending_load(CommandStatus::Superseded);
        self.cancel_pending_restart(CommandStatus::Cancelled);
        self.cancel_pending_seek(CommandStatus::Cancelled);
        self.retire_current_pipeline();

        self.load_generation += 1;
        self.source = Some(url.clone());
        self.media = None;
        self.duration_ms = 0;
        self.position_ms = 0;
        self.subtitle_loaded = false;
        self.subtitle_source = None;
        self.danmaku_loaded = false;
        self.danmaku_source = None;
        self.danmaku_count = 0;

        self.pending_load = Some(PendingLoad {
            request_id,
            op: CommandOp::LoadFile,
        });
        self.start_load_job(
            request_id,
            CommandOp::LoadFile,
            url,
            LoadPurpose::LoadFile,
            TrackSelection::default(),
        );

        self.set_state(PlayerState::Loading, StateChangeReason::Load);
        self.publish();
    }

    fn cmd_play(&mut self, request_id: u64) {
        match self.state {
            PlayerState::Playing => self.complete_ok(request_id, CommandOp::Play),
            PlayerState::Ready | PlayerState::Paused => {
                if let Some(pipeline) = &self.pipeline {
                    pipeline.resume();
                }
                self.set_state(PlayerState::Playing, StateChangeReason::Play);
                self.publish();
                self.complete_ok(request_id, CommandOp::Play);
            }
            PlayerState::Ended | PlayerState::Stopped => {
                let Some(source) = self.source.clone() else {
                    let error = EngineError::invalid_state("cannot restart: no source loaded");
                    self.complete_error(request_id, CommandOp::Play, &error);
                    return;
                };
                self.cancel_pending_restart(CommandStatus::Superseded);
                self.retire_current_pipeline();
                self.load_generation += 1;
                self.pending_restart = Some(PendingRestart {
                    request_id,
                    op: CommandOp::Play,
                });
                self.start_load_job(
                    request_id,
                    CommandOp::Play,
                    source,
                    LoadPurpose::RestartPlay,
                    current_selection(self.media.as_ref()),
                );
            }
            _ => unreachable!("matrix already rejected play in {:?}", self.state),
        }
    }

    fn cmd_pause(&mut self, request_id: u64) {
        if self.state == PlayerState::Paused {
            self.complete_ok(request_id, CommandOp::Pause);
            return;
        }
        let position_ms = self.live_position_ms();
        if let Some(pipeline) = &self.pipeline {
            pipeline.pause();
        }
        self.shared.output.freeze_overlay_clock();
        self.position_ms = position_ms;
        self.set_state(PlayerState::Paused, StateChangeReason::Pause);
        self.publish();
        self.emit_position(position_ms);
        self.complete_ok(request_id, CommandOp::Pause);
    }

    fn cmd_stop(&mut self, request_id: u64) {
        let was_loading = self.state == PlayerState::Loading;
        let had_pipeline = self.pipeline.is_some();

        self.cancel_pending_load(CommandStatus::Cancelled);
        self.cancel_pending_restart(CommandStatus::Cancelled);
        self.cancel_pending_seek(CommandStatus::Cancelled);
        self.load_generation += 1;
        self.retire_current_pipeline();

        if was_loading {
            self.source = None;
            self.media = None;
            self.duration_ms = 0;
        }
        self.position_ms = 0;

        let changed = !matches!(self.state, PlayerState::Idle | PlayerState::Stopped);
        if self.state != PlayerState::Idle {
            self.set_state(PlayerState::Stopped, StateChangeReason::Stop);
        }
        self.publish();
        if changed || had_pipeline {
            self.emit_position(0);
        }
        self.complete_ok(request_id, CommandOp::Stop);
    }

    fn cmd_seek(
        &mut self,
        request_id: u64,
        op: CommandOp,
        position_ms: Option<u64>,
        delta_ms: Option<i64>,
        mode: SeekMode,
    ) {
        let target = self.seek_target(position_ms, delta_ms);

        match self.state {
            PlayerState::Ready | PlayerState::Playing | PlayerState::Paused => {
                let Some(pipeline) = self.pipeline.clone() else {
                    let error = EngineError::invalid_state("no pipeline for seek");
                    self.complete_error(request_id, op, &error);
                    return;
                };
                let from_ms = self.live_position_ms();
                if let Some(previous) = self.pending_seek.take() {
                    self.superseded_seeks.push(previous);
                }
                let tag = self.next_seek_tag();
                *pipeline.seek_request.lock() = Some(SeekRequest {
                    target: target as f64 / 1000.0,
                    from: from_ms as f64 / 1000.0,
                    tag,
                    exact: mode == SeekMode::Exact,
                });
                self.pending_seek = Some(PendingSeek {
                    request_id,
                    op,
                    tag,
                    from_ms,
                    to_ms: target,
                    mode,
                });
            }
            PlayerState::Ended | PlayerState::Stopped => {
                let Some(source) = self.source.clone() else {
                    let error = EngineError::invalid_state("cannot seek: no source loaded");
                    self.complete_error(request_id, op, &error);
                    return;
                };
                self.cancel_pending_restart(CommandStatus::Superseded);
                self.retire_current_pipeline();
                self.load_generation += 1;

                let tag = self.next_seek_tag();
                let from_ms = self.position_ms;
                self.pending_seek = Some(PendingSeek {
                    request_id,
                    op,
                    tag,
                    from_ms,
                    to_ms: target,
                    mode,
                });
                self.pending_restart = Some(PendingRestart { request_id, op });
                self.start_load_job(
                    request_id,
                    op,
                    source,
                    LoadPurpose::RestartSeek,
                    current_selection(self.media.as_ref()),
                );
            }
            _ => unreachable!("matrix already rejected seek in {:?}", self.state),
        }
    }

    fn cmd_load_overlay(
        &mut self,
        request_id: u64,
        op: CommandOp,
        path: String,
        kind: OverlayKind,
    ) {
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("dyl-overlay-load".into())
            .spawn(move || {
                let result = match kind {
                    OverlayKind::Subtitle => shared.overlays.load_subtitle(&path),
                    OverlayKind::Danmaku => shared.overlays.load_danmaku(&path),
                };
                let _ = shared
                    .control_tx
                    .send(ControlMsg::OverlayDone(OverlayOutcome {
                        request_id,
                        op,
                        path,
                        kind,
                        result,
                    }));
            });
        if let Err(e) = spawned {
            let error = EngineError::new(ErrorCode::Internal, format!("spawn overlay loader: {e}"));
            self.complete_error(request_id, op, &error);
        }
    }

    /// 换轨 = 带着新的轨道组合重开管线。不想换的那几条必须显式带上，否则 demuxer
    /// 会按 default 标记重新选一遍、把画面悄悄换掉。
    fn cmd_select_track(&mut self, request_id: u64, kind: TrackKind, index: u32) {
        let op = CommandOp::SelectTrack;
        let Some(media) = self.media.clone() else {
            let error = EngineError::invalid_state("cannot switch tracks: no media info");
            self.complete_error(request_id, op, &error);
            return;
        };
        let Some(source) = self.source.clone() else {
            let error = EngineError::invalid_state("cannot switch tracks: no source loaded");
            self.complete_error(request_id, op, &error);
            return;
        };
        let Some(target) = media
            .tracks
            .iter()
            .find(|track| track.index == index && track.kind == kind)
        else {
            // 轨道表是加载时才有的动态状态，按契约只能在异步阶段拒绝
            let error = EngineError::invalid_argument(format!(
                "select_track: no {:?} track with index {} in this file",
                kind, index
            ));
            self.complete_error(request_id, op, &error);
            return;
        };
        if target.selected {
            // 幂等：重复选同一条轨不重开管线，也不重发 file_loaded
            self.complete_ok(request_id, op);
            return;
        }

        let mut selection = current_selection(self.media.as_ref());
        match kind {
            TrackKind::Video => unreachable!("validate rejects the video kind synchronously"),
            TrackKind::Audio => selection.audio = Some(index as usize),
            TrackKind::Subtitle => selection.subtitle = Some(index as usize),
        }

        // ended/stopped 的位置就是终点，重开只回到 0 才合理（与 play 重启同义）
        let (resume, position_ms) = match self.state {
            PlayerState::Ended | PlayerState::Stopped => (PlayerState::Paused, 0),
            current => (current, self.live_position_ms()),
        };

        self.cancel_pending_seek(CommandStatus::Cancelled);
        self.cancel_pending_restart(CommandStatus::Superseded);
        self.retire_current_pipeline();
        self.load_generation += 1;
        self.pending_switch = Some(PendingSwitch {
            resume,
            position_ms,
            seek_tag: None,
        });
        self.pending_restart = Some(PendingRestart { request_id, op });
        self.start_load_job(request_id, op, source, LoadPurpose::SwitchTrack, selection);

        self.set_state(PlayerState::Loading, StateChangeReason::Load);
        self.publish();
    }

    /// 切解码方式：有活管线（ready/playing/paused）时按新策略重开并落回原状态；
    /// 其余状态只持久化设置，下一次打开或重开时生效。
    ///
    /// 重开不重发 `file_loaded`（文件与轨道都没变），位置由 PendingSwitch 钉住，
    /// 恢复用内部 exact seek，客户端只为这条命令看到一个 `command_completed`。
    fn cmd_set_decode_mode(&mut self, request_id: u64, mode: DecodeMode) {
        let op = CommandOp::SetDecodeMode;
        if mode == self.decode_mode {
            // 幂等：重复设置同一种方式不重开管线
            self.complete_ok(request_id, op);
            return;
        }
        self.decode_mode = mode;

        let reopen = matches!(
            self.state,
            PlayerState::Ready | PlayerState::Playing | PlayerState::Paused
        );
        if !reopen {
            self.publish();
            self.emit_settings();
            self.complete_ok(request_id, op);
            return;
        }

        let Some(source) = self.source.clone() else {
            let error = EngineError::invalid_state("cannot switch decode mode: no source loaded");
            self.complete_error(request_id, op, &error);
            return;
        };
        let resume = self.state;
        let position_ms = self.live_position_ms();

        self.cancel_pending_seek(CommandStatus::Cancelled);
        self.cancel_pending_restart(CommandStatus::Superseded);
        self.retire_current_pipeline();
        self.load_generation += 1;
        self.pending_switch = Some(PendingSwitch {
            resume,
            position_ms,
            seek_tag: None,
        });
        self.pending_restart = Some(PendingRestart { request_id, op });
        self.start_load_job(
            request_id,
            op,
            source,
            LoadPurpose::SwitchDecode,
            current_selection(self.media.as_ref()),
        );

        self.set_state(PlayerState::Loading, StateChangeReason::Load);
        self.publish();
    }

    // ── Worker 回调 ─────────────────────────────────────────────

    fn handle_load_done(&mut self, outcome: LoadOutcome) {
        match outcome {
            LoadOutcome::Ready(ready) => {
                if ready.generation != self.load_generation || !self.pending_matches(&ready) {
                    self.shared.retire_pipeline(ready.pipeline);
                    return;
                }
                self.adopt_pipeline(&ready);
                match ready.purpose {
                    LoadPurpose::LoadFile => {
                        self.pending_load = None;
                        self.position_ms = 0;
                        self.last_error = None;
                        self.events.emit(Event::FileLoaded(FileLoaded {
                            request_id: ready.request_id,
                            source: ready.source.clone(),
                            duration_ms: ready.duration_ms,
                            media: ready.media.clone(),
                        }));
                        if ready.font_missing {
                            self.events.emit(Event::Warning(Warning {
                                code: "font_missing".into(),
                                message: "no system font available: subtitles and danmaku will not render"
                                    .into(),
                            }));
                        }
                        self.set_state(PlayerState::Ready, StateChangeReason::Load);
                        self.publish();
                        self.complete_ok(ready.request_id, ready.op);
                    }
                    LoadPurpose::RestartPlay => {
                        self.pending_restart = None;
                        if let Some(pipeline) = &self.pipeline {
                            pipeline.resume();
                        }
                        self.position_ms = 0;
                        self.set_state(PlayerState::Playing, StateChangeReason::Restart);
                        self.publish();
                        self.emit_position(0);
                        self.complete_ok(ready.request_id, ready.op);
                    }
                    LoadPurpose::RestartSeek => {
                        self.pending_restart = None;
                        self.position_ms = self
                            .pending_seek
                            .as_ref()
                            .map(|pending| pending.from_ms)
                            .unwrap_or(self.position_ms);
                        self.set_state(PlayerState::Paused, StateChangeReason::Seek);
                        self.publish();

                        let request = self.pending_seek.as_ref().map(|pending| SeekRequest {
                            target: pending.to_ms as f64 / 1000.0,
                            from: 0.0,
                            tag: pending.tag,
                            exact: pending.mode == SeekMode::Exact,
                        });
                        match (request, self.pipeline.as_ref()) {
                            (Some(request), Some(pipeline)) => {
                                *pipeline.seek_request.lock() = Some(request);
                            }
                            _ => {
                                let error = EngineError::invalid_state(
                                    "restart seek lost its pending request",
                                );
                                self.complete_error(ready.request_id, ready.op, &error);
                            }
                        }
                    }
                    LoadPurpose::SwitchTrack => {
                        self.pending_restart = None;
                        let Some(switch) = self.pending_switch.clone() else {
                            let error =
                                EngineError::invalid_state("track switch lost its pending request");
                            self.complete_error(ready.request_id, ready.op, &error);
                            return;
                        };
                        // 换轨后的真相全在 media 里，所以复用 file_loaded 而不是新增事件类型
                        self.events.emit(Event::FileLoaded(FileLoaded {
                            request_id: ready.request_id,
                            source: ready.source.clone(),
                            duration_ms: ready.duration_ms,
                            media: ready.media.clone(),
                        }));
                        self.restore_switched_pipeline(&ready, switch);
                    }
                    LoadPurpose::SwitchDecode => {
                        self.pending_restart = None;
                        let Some(switch) = self.pending_switch.clone() else {
                            let error = EngineError::invalid_state(
                                "decode switch lost its pending request",
                            );
                            self.complete_error(ready.request_id, ready.op, &error);
                            return;
                        };
                        // 文件、轨道、时长全都没变，没有 file_loaded 可发；新真相是
                        // 快照里的 decode.backend
                        self.restore_switched_pipeline(&ready, switch);
                    }
                }
            }
            LoadOutcome::Failed {
                generation,
                request_id,
                op,
                purpose,
                error,
            } => {
                if generation != self.load_generation {
                    return;
                }
                match purpose {
                    LoadPurpose::LoadFile => {
                        if !self
                            .pending_load
                            .as_ref()
                            .is_some_and(|pending| pending.request_id == request_id)
                        {
                            return;
                        }
                        self.pending_load = None;
                        self.source = None;
                        self.media = None;
                        self.duration_ms = 0;
                        self.position_ms = 0;
                        self.record_error(&error);
                        self.set_state(PlayerState::Error, StateChangeReason::Error);
                        self.publish();
                        self.complete_error(request_id, op, &error);
                    }
                    LoadPurpose::RestartPlay
                    | LoadPurpose::RestartSeek
                    | LoadPurpose::SwitchTrack
                    | LoadPurpose::SwitchDecode => {
                        if !self
                            .pending_restart
                            .as_ref()
                            .is_some_and(|pending| pending.request_id == request_id)
                        {
                            return;
                        }
                        self.pending_restart = None;
                        self.pending_seek = None;
                        // 新管线没起来，位置钉再无兑现可能；旧管线已退役，只能落 error
                        self.pending_switch = None;
                        self.record_error(&error);
                        self.set_state(PlayerState::Error, StateChangeReason::Error);
                        self.publish();
                        self.complete_error(request_id, op, &error);
                    }
                }
            }
        }
    }

    /// 重开（换轨/切解码方式）完成后的共同收尾：落回原状态、恢复位置、按需恢复播放。
    /// 调用方负责 `file_loaded` 之类与用途相关的语义事件。
    fn restore_switched_pipeline(&mut self, ready: &crate::load::LoadReady, switch: PendingSwitch) {
        let landing = switch.resume;
        self.position_ms = switch.position_ms;
        self.set_state(
            landing,
            if landing == PlayerState::Playing {
                StateChangeReason::Restart
            } else {
                StateChangeReason::Load
            },
        );
        // 位置恢复只是内部管道：注入 exact seek 但不建 pending_seek，
        // 客户端为这条命令只看到一个 command_completed
        if switch.position_ms > 0 {
            let tag = self.next_seek_tag();
            if let Some(pending) = self.pending_switch.as_mut() {
                pending.seek_tag = Some(tag);
            }
            if let Some(pipeline) = &self.pipeline {
                *pipeline.seek_request.lock() = Some(SeekRequest {
                    target: switch.position_ms as f64 / 1000.0,
                    from: 0.0,
                    tag,
                    exact: true,
                });
            } else {
                self.pending_switch = None;
            }
        } else {
            self.pending_switch = None;
        }
        if landing == PlayerState::Playing {
            if let Some(pipeline) = &self.pipeline {
                pipeline.resume();
            }
        }
        // 仍在钉住位置时发布：新管线一个画面都没出，live 读到的是 0
        self.publish();
        self.complete_ok(ready.request_id, ready.op);
    }

    fn pending_matches(&self, ready: &crate::load::LoadReady) -> bool {
        match ready.purpose {
            LoadPurpose::LoadFile => self
                .pending_load
                .as_ref()
                .is_some_and(|pending| pending.request_id == ready.request_id),
            LoadPurpose::RestartPlay
            | LoadPurpose::RestartSeek
            | LoadPurpose::SwitchTrack
            | LoadPurpose::SwitchDecode => self
                .pending_restart
                .as_ref()
                .is_some_and(|pending| pending.request_id == ready.request_id),
        }
    }

    fn adopt_pipeline(&mut self, ready: &crate::load::LoadReady) {
        self.session = ready.session;
        self.pipeline = Some(ready.pipeline.clone());
        self.source = Some(ready.source.clone());
        self.media = Some(ready.media.clone());
        self.duration_ms = ready.duration_ms;
        if let Some(pipeline) = &self.pipeline {
            pipeline.set_volume(self.volume);
            pipeline.set_mute(self.muted);
        }
    }

    fn handle_overlay_done(&mut self, outcome: OverlayOutcome) {
        match outcome.result {
            Ok(count) => {
                // 加载即展示：用户点了"加载字幕"却看不到内容会更困惑。
                // 只有可见性真的从 false 变 true 才算设置变化，避免多发 settings_changed
                let mut visibility_changed = false;
                match outcome.kind {
                    OverlayKind::Subtitle => {
                        self.subtitle_loaded = true;
                        self.subtitle_source = Some(outcome.path.clone());
                        if !self.subtitle_visible {
                            self.subtitle_visible = true;
                            self.shared.overlays.set_subtitle_visible(true);
                            visibility_changed = true;
                        }
                    }
                    OverlayKind::Danmaku => {
                        self.danmaku_loaded = true;
                        self.danmaku_source = Some(outcome.path.clone());
                        self.danmaku_count = count as u64;
                        if !self.danmaku_visible {
                            self.danmaku_visible = true;
                            self.shared.overlays.set_danmaku_visible(true);
                            visibility_changed = true;
                        }
                    }
                }
                self.publish();
                if visibility_changed {
                    self.emit_settings();
                }
                let result = match outcome.kind {
                    OverlayKind::Subtitle => serde_json::json!({ "events": count }),
                    OverlayKind::Danmaku => serde_json::json!({ "count": count }),
                };
                self.complete_with_result(outcome.request_id, outcome.op, result);
            }
            Err(error) => self.complete_error(outcome.request_id, outcome.op, &error),
        }
    }

    // ── Pipeline 内部事件 ───────────────────────────────────────

    fn handle_pipeline_event(&mut self, msg: PipelineEventMsg) {
        if msg.session != self.session {
            return;
        }
        match msg.event {
            PipelineEvent::Ended => self.on_ended(),
            PipelineEvent::SeekApplied {
                tag, position_secs, ..
            } => self.on_seek_applied(tag, position_secs),
            PipelineEvent::SeekFailed { tag, message } => self.on_seek_failed(tag, message),
            PipelineEvent::Error { code, message } => self.on_pipeline_error(code, message),
        }
    }

    fn on_ended(&mut self) {
        if self.state != PlayerState::Playing {
            return;
        }
        self.position_ms = self.duration_ms;
        self.set_state(PlayerState::Ended, StateChangeReason::Eof);
        self.publish();
        self.emit_position(self.duration_ms);
        if let Some(pending) = self.pending_seek.take() {
            let error = EngineError::new(ErrorCode::Pipeline, "seek did not complete before EOF");
            self.complete_error(pending.request_id, pending.op, &error);
        }
        self.flush_superseded();
        self.retire_current_pipeline();
    }

    fn on_seek_applied(&mut self, tag: u64, position_secs: f64) {
        // 换轨的位置恢复走的是内部 seek（不建 pending_seek）：它一落地就解除位置钉，
        // 快照改读真实位置。这条命令自己只有那一个 command_completed。
        if let Some(pending) = self.pending_switch.as_mut() {
            if pending.seek_tag == Some(tag) {
                // seek 落地：钉住目标位置而非 pipeline 报告的关键帧位置，
                // 等 publish 时 live 追上目标才解除位置钉
                pending.seek_tag = None;
                self.position_ms = pending.position_ms;
                self.publish();
            }
        }

        let Some(pending) = self
            .pending_seek
            .as_ref()
            .filter(|pending| pending.tag == tag)
            .cloned()
        else {
            return;
        };
        self.pending_seek = None;

        let mut actual_ms = secs_to_ms(position_secs);
        if self.duration_ms > 0 {
            actual_ms = actual_ms.min(self.duration_ms);
        }
        self.position_ms = actual_ms;
        self.events.emit(Event::SeekCompleted(SeekCompleted {
            request_id: pending.request_id,
            from_ms: pending.from_ms,
            to_ms: pending.to_ms,
            actual_ms,
            mode: pending.mode,
        }));
        self.complete_ok(pending.request_id, pending.op);
        self.flush_superseded();
        self.publish();
        self.emit_position(actual_ms);
    }

    fn on_seek_failed(&mut self, tag: u64, message: String) {
        let Some(pending) = self
            .pending_seek
            .as_ref()
            .filter(|pending| pending.tag == tag)
            .cloned()
        else {
            return;
        };
        self.pending_seek = None;
        let error = EngineError::new(ErrorCode::Pipeline, message);
        self.complete_error(pending.request_id, pending.op, &error);
        self.flush_superseded();
    }

    fn on_pipeline_error(&mut self, code: ErrorCode, message: String) {
        if !matches!(
            self.state,
            PlayerState::Ready | PlayerState::Playing | PlayerState::Paused | PlayerState::Ended
        ) {
            return;
        }
        self.position_ms = self.live_position_ms();
        self.set_state(PlayerState::Error, StateChangeReason::Error);
        self.record_error(&EngineError::new(code, message.clone()));
        self.events.emit(Event::Error(ErrorEvent {
            code,
            code_num: code.as_i32(),
            message,
            fatal: true,
        }));
        if let Some(pending) = self.pending_seek.take() {
            let error =
                EngineError::new(ErrorCode::Pipeline, "pipeline failed before seek completed");
            self.complete_error(pending.request_id, pending.op, &error);
        }
        self.flush_superseded();
        self.retire_current_pipeline();
        self.publish();
    }

    fn on_tick(&mut self) {
        if self.state != PlayerState::Playing || self.pipeline.is_none() {
            return;
        }
        self.publish();
        self.emit_position(self.position_ms);
    }

    // ── 待处理请求与 pipeline 生命周期 ─────────────────────────

    fn start_load_job(
        &mut self,
        request_id: u64,
        op: CommandOp,
        source: String,
        purpose: LoadPurpose,
        selection: TrackSelection,
    ) {
        self.next_session += 1;
        self.session = self.next_session;
        let job = LoadJob {
            generation: self.load_generation,
            request_id,
            op,
            source,
            purpose,
            session: self.session,
            selection,
            decode_mode: self.decode_mode,
        };
        spawn_load(self.shared.clone(), job);
    }

    fn retire_current_pipeline(&mut self) {
        // 退役的管线不可能再报告它在途的 seek，位置钉也就没了兑现对象
        self.pending_switch = None;
        if let Some(pipeline) = self.pipeline.take() {
            self.shared.retire_pipeline(pipeline);
        }
    }

    fn next_seek_tag(&mut self) -> u64 {
        self.seek_tag_seq += 1;
        self.seek_tag_seq
    }

    fn cancel_pending_load(&mut self, status: CommandStatus) {
        if let Some(pending) = self.pending_load.take() {
            self.complete(pending.request_id, pending.op, status);
        }
    }

    fn cancel_pending_restart(&mut self, status: CommandStatus) {
        if let Some(pending) = self.pending_restart.take() {
            if self
                .pending_seek
                .as_ref()
                .is_some_and(|seek| seek.request_id == pending.request_id)
            {
                self.pending_seek = None;
            }
            self.complete(pending.request_id, pending.op, status);
        }
    }

    fn cancel_pending_seek(&mut self, status: CommandStatus) {
        if let Some(pending) = self.pending_seek.take() {
            self.complete(pending.request_id, pending.op, status);
        }
        let superseded = std::mem::take(&mut self.superseded_seeks);
        for pending in superseded {
            self.complete(pending.request_id, pending.op, status);
        }
    }

    fn flush_superseded(&mut self) {
        let pending = std::mem::take(&mut self.superseded_seeks);
        for seek in pending {
            self.complete(seek.request_id, seek.op, CommandStatus::Superseded);
        }
    }

    // ── 快照与事件 ─────────────────────────────────────────────

    fn live_position_ms(&self) -> u64 {
        self.pipeline
            .as_ref()
            .map(|pipeline| secs_to_ms(pipeline.current_position_secs()))
            .unwrap_or(self.position_ms)
    }

    fn seek_target(&self, position_ms: Option<u64>, delta_ms: Option<i64>) -> u64 {
        let upper: i128 = if self.duration_ms > 0 {
            self.duration_ms as i128
        } else {
            i64::MAX as i128
        };
        let raw: i128 = match (position_ms, delta_ms) {
            (Some(position), _) => position as i128,
            (None, Some(delta)) => self.live_position_ms() as i128 + delta as i128,
            _ => 0,
        };
        raw.clamp(0, upper) as u64
    }

    fn publish(&mut self) {
        let position_ms = match self.state {
            PlayerState::Ended => self.duration_ms,
            // 换轨刚重开时新管线还没出任何画面，live 会读到 0，进度条就会掉回开头
            // 再弹回来；位置钉一直到恢复用的 seek 落地才解除
            PlayerState::Ready | PlayerState::Playing | PlayerState::Paused
                if self.pending_switch.is_some() =>
            {
                // seek 已落地后，live 追上目标（或到 EOF）才解除位置钉，
                // 避免 exact seek 到关键帧的过渡中快照暴露关键帧位置
                let seek_landed = self
                    .pending_switch
                    .as_ref()
                    .is_some_and(|p| p.seek_tag.is_none());
                if seek_landed {
                    let live = self.live_position_ms();
                    let at_eof = self.duration_ms > 0 && live >= self.duration_ms;
                    if live >= self.position_ms || at_eof {
                        self.pending_switch = None;
                        self.position_ms = live;
                        live
                    } else {
                        self.position_ms
                    }
                } else {
                    self.position_ms
                }
            }
            PlayerState::Ready | PlayerState::Playing | PlayerState::Paused => {
                let live = self.live_position_ms();
                self.position_ms = live;
                live
            }
            _ => self.position_ms,
        };
        let surface = self.shared.surface.lock().snapshot();
        let stats = self.shared.output.stats();
        let (backend, device) = match self
            .pipeline
            .as_ref()
            .and_then(|pipeline| pipeline.decode_device())
        {
            Some(device) => (DecodeBackend::Hardware, Some(device.to_string())),
            None => (DecodeBackend::Software, None),
        };
        let snapshot = Snapshot {
            schema: SCHEMA_VERSION,
            state: self.state,
            position_ms,
            duration_ms: self.duration_ms,
            volume: self.volume,
            muted: self.muted,
            decode: DecodeSnapshot {
                mode: self.decode_mode,
                backend,
                device,
            },
            source: self.source.clone(),
            media: self.media.clone(),
            subtitle: SubtitleSnapshot {
                loaded: self.subtitle_loaded,
                visible: self.subtitle_visible,
                source: self.subtitle_source.clone(),
            },
            danmaku: DanmakuSnapshot {
                loaded: self.danmaku_loaded,
                visible: self.danmaku_visible,
                opacity: self.danmaku_opacity,
                count: self.danmaku_count,
                source: self.danmaku_source.clone(),
            },
            surface,
            stats: StatsSnapshot {
                rendered_frames: stats.rendered_frames,
                dropped_frames: stats.dropped_frames,
            },
            last_error: self.last_error.clone(),
        };
        self.shared.publish(snapshot);
    }

    fn set_state(&mut self, to: PlayerState, reason: StateChangeReason) {
        let from = self.state;
        if from == to {
            return;
        }
        self.state = to;
        self.events
            .emit(Event::StateChanged(StateChanged { from, to, reason }));
    }

    fn emit_position(&mut self, position_ms: u64) {
        self.events
            .emit(Event::PositionChanged(PositionChanged { position_ms }));
    }

    fn emit_settings(&mut self) {
        self.events.emit(Event::SettingsChanged(SettingsChanged {
            volume: self.volume,
            muted: self.muted,
            decode_mode: self.decode_mode,
            subtitle_visible: self.subtitle_visible,
            danmaku_visible: self.danmaku_visible,
            danmaku_opacity: self.danmaku_opacity,
        }));
    }

    fn record_error(&mut self, error: &EngineError) {
        self.last_error = Some(SnapshotError {
            code: error.code,
            code_num: error.code.as_i32(),
            message: error.message.clone(),
        });
    }

    // ── command_completed ──────────────────────────────────────

    fn complete(&mut self, request_id: u64, op: CommandOp, status: CommandStatus) {
        self.shared.finish_command(request_id);
        self.events.emit(Event::CommandCompleted(CommandCompleted {
            request_id,
            op,
            status,
            error: None,
            result: None,
        }));
    }

    fn complete_ok(&mut self, request_id: u64, op: CommandOp) {
        self.complete(request_id, op, CommandStatus::Ok);
    }

    fn complete_with_result(&mut self, request_id: u64, op: CommandOp, result: serde_json::Value) {
        self.shared.finish_command(request_id);
        self.events.emit(Event::CommandCompleted(CommandCompleted {
            request_id,
            op,
            status: CommandStatus::Ok,
            error: None,
            result: Some(result),
        }));
    }

    fn complete_error(&mut self, request_id: u64, op: CommandOp, error: &EngineError) {
        self.shared.finish_command(request_id);
        self.events.emit(Event::CommandCompleted(CommandCompleted {
            request_id,
            op,
            status: CommandStatus::Error,
            error: Some(ErrorInfo::new(error.code)),
            result: None,
        }));
    }
}

fn secs_to_ms(secs: f64) -> u64 {
    if !secs.is_finite() || secs <= 0.0 {
        0
    } else {
        (secs * 1000.0).round() as u64
    }
}

/// 快照里当前选中的轨道索引，用于把不想换的那几条原样带回新的 `open`
fn selected_index(media: &MediaInfo, kind: TrackKind) -> Option<usize> {
    media
        .tracks
        .iter()
        .find(|track| track.kind == kind && track.selected)
        .map(|track| track.index as usize)
}

/// 从 ended/stopped 重开（play/seek 重启）时也要带上当前选择：给 `default()`
/// 会让 demuxer 按 default 标记重选一遍，把用户选的音轨/字幕悄悄换掉。
fn current_selection(media: Option<&MediaInfo>) -> TrackSelection {
    match media {
        Some(media) => TrackSelection {
            video: selected_index(media, TrackKind::Video),
            audio: selected_index(media, TrackKind::Audio),
            subtitle: selected_index(media, TrackKind::Subtitle),
        },
        None => TrackSelection::default(),
    }
}

pub(crate) fn spawn_control(
    shared: Arc<EngineShared>,
    cmd_rx: Receiver<ControlMsg>,
    pipe_rx: Receiver<PipelineEventMsg>,
    event_tx: crossbeam_channel::Sender<dyl_core::contract::EventEnvelope>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("dyl-engine".into())
        .spawn(move || ControlState::new(shared, event_tx).run(cmd_rx, pipe_rx))
}
