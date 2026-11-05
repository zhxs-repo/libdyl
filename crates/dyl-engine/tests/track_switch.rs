//! `select_track` 的真实文件回归 — 需要一个多轨素材。
//!
//! 生成：`bash scripts/make_multitrack_asset.sh`（默认产出 `libdyl/multitrack.mkv`），
//! 或用 `DYL_MULTITRACK_ASSET` 指向别的文件。素材不存在时整个文件跳过，
//! 因为工作区不入库这个产物；CI 里先生成再跑 `cargo test --workspace`。
//!
//! 这里守的是三条会静默退化的性质：
//! - 换轨的实现是「带着显式索引重开管线」，所以画面绝不能因为重开而改变。
//! - 重开会让新管线从 0 开始出位置，`PendingSwitch` 的位置钉必须撑到内部
//!   seek 落地，否则进度条会跳回开头（历史 bug）。
//! - 内部的位置 seek 只是实现细节：客户端为这条命令只能看到一个 `command_completed`，
//!   不能多出一条 `seek_completed`。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    Command, CommandEnvelope, CommandOp, CommandStatus, Event, EventEnvelope, MediaInfo,
    PlayerState, SeekMode, StateChangeReason, TrackKind,
};
use dyl_engine::Engine;

const WAIT: Duration = Duration::from_secs(20);

fn asset_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("DYL_MULTITRACK_ASSET") {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../multitrack.mkv");
    path.exists().then_some(path)
}

fn skip_if_absent(asset: &Option<PathBuf>) -> bool {
    if asset.is_none() {
        eprintln!("SKIP: multitrack.mkv 不存在，运行 bash scripts/make_multitrack_asset.sh 生成");
    }
    asset.is_none()
}

fn asset_url(asset: &std::path::Path) -> String {
    asset.to_string_lossy().to_string()
}

/// 收集到指定 request_id 的 command_completed 为止，返回途中收到的全部事件
fn drain_until_completed(
    rx: &Receiver<EventEnvelope>,
    request_id: u64,
) -> (CommandStatus, Vec<Event>) {
    let deadline = Instant::now() + WAIT;
    let mut events = Vec::new();
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for command_completed");
        let envelope = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("event channel closed before request {request_id}"));
        if let Event::CommandCompleted(completed) = &envelope.event {
            if completed.request_id == request_id {
                assert_eq!(completed.op, CommandOp::SelectTrack);
                return (completed.status, events);
            }
        }
        events.push(envelope.event);
    }
}

fn file(media: &Option<MediaInfo>) -> &MediaInfo {
    media.as_ref().expect("media info must be present")
}

/// 丢掉此前命令（load/play）留下的事件，让后续断言只看这条切轨
fn flush(rx: &Receiver<EventEnvelope>) {
    while rx.try_recv().is_ok() {}
}

/// 被拒绝或幂等的切轨必须毫无痕迹：既没有这条请求的 command_completed，
/// 也没有进 loading、没有重发 file_loaded。留出一点时间让在途事件落地。
fn assert_no_switch_trace(rx: &Receiver<EventEnvelope>, request_id: u64) {
    std::thread::sleep(Duration::from_millis(150));
    while let Ok(envelope) = rx.try_recv() {
        match &envelope.event {
            Event::CommandCompleted(completed) => assert_ne!(
                completed.request_id, request_id,
                "被拒绝的请求不该有 command_completed"
            ),
            Event::StateChanged(changed) => assert_ne!(
                changed.to,
                PlayerState::Loading,
                "被拒绝的切轨不能把状态推进 loading"
            ),
            Event::FileLoaded(_) => panic!("被拒绝的切轨不得重发 file_loaded"),
            _ => {}
        }
    }
}

fn selected(media: &MediaInfo, kind: TrackKind) -> Vec<u32> {
    media
        .tracks
        .iter()
        .filter(|track| track.kind == kind && track.selected)
        .map(|track| track.index)
        .collect()
}

/// 加载素材并播到 1s 之后，返回 (engine, events, 首帧位置)
fn load_and_play(
    engine: &mut Engine,
    rx: &Receiver<EventEnvelope>,
    asset: &std::path::Path,
) -> u64 {
    let request_id = engine
        .command(CommandEnvelope::with_request_id(
            1,
            Command::LoadFile {
                url: asset_url(asset),
            },
        ))
        .expect("load_file is accepted");
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("load timed out");
        let envelope = rx.recv_timeout(left).expect("no events while loading");
        if let Event::CommandCompleted(completed) = &envelope.event {
            assert_eq!(completed.request_id, request_id);
            assert_eq!(completed.status, CommandStatus::Ok, "{completed:?}");
            break;
        }
    }

    engine
        .command(CommandEnvelope::with_request_id(2, Command::Play))
        .expect("play is accepted in ready");
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = engine.snapshot();
        assert_ne!(
            snapshot.state,
            PlayerState::Error,
            "{:?}",
            snapshot.last_error
        );
        if snapshot.position_ms >= 1_000 {
            return snapshot.position_ms;
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("playback did not advance");
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

#[test]
fn video_track_switch_is_rejected_synchronously() {
    let asset = asset_path();
    if skip_if_absent(&asset) {
        return;
    }
    let (mut engine, rx) = Engine::new().expect("engine must start");
    load_and_play(&mut engine, &rx, asset.as_ref().unwrap());
    flush(&rx);

    let error = engine
        .command(CommandEnvelope::with_request_id(
            50,
            Command::SelectTrack {
                kind: TrackKind::Video,
                index: 0,
            },
        ))
        .expect_err("video switching is not a declared capability");
    assert_eq!(error.code, dyl_core::contract::ErrorCode::NotSupported);
    assert!(error.message.contains("video"), "{error}");
    assert_no_switch_trace(&rx, 50);
    assert_eq!(engine.snapshot().state, PlayerState::Playing);
    engine.destroy();
}

#[test]
fn unknown_track_index_is_rejected_asynchronously_without_loading() {
    let asset = asset_path();
    if skip_if_absent(&asset) {
        return;
    }
    let (mut engine, rx) = Engine::new().expect("engine must start");
    load_and_play(&mut engine, &rx, asset.as_ref().unwrap());

    engine
        .command(CommandEnvelope::with_request_id(
            51,
            Command::SelectTrack {
                kind: TrackKind::Audio,
                index: 9_999,
            },
        ))
        .expect("the index is only known once media is loaded");

    let (status, events) = drain_until_completed(&rx, 51);
    assert_eq!(status, CommandStatus::Error);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::StateChanged(changed) if changed.to == PlayerState::Loading)),
        "被拒绝的切轨不能把状态推进 loading"
    );
    assert_eq!(engine.snapshot().state, PlayerState::Playing);
    engine.destroy();
}

#[test]
fn reselecting_the_current_track_is_idempotent() {
    let asset = asset_path();
    if skip_if_absent(&asset) {
        return;
    }
    let (mut engine, rx) = Engine::new().expect("engine must start");
    load_and_play(&mut engine, &rx, asset.as_ref().unwrap());

    let before = engine.snapshot();
    let current = selected(file(&before.media), TrackKind::Audio)[0];
    flush(&rx);
    engine
        .command(CommandEnvelope::with_request_id(
            52,
            Command::SelectTrack {
                kind: TrackKind::Audio,
                index: current,
            },
        ))
        .expect("already-selected track is accepted");

    let (status, events) = drain_until_completed(&rx, 52);
    assert_eq!(status, CommandStatus::Ok);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::FileLoaded(_))),
        "幂等换轨不重开管线，不得重发 file_loaded：{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::StateChanged(changed) if changed.to == PlayerState::Loading
        )),
        "幂等换轨不得把状态推进 loading：{events:?}"
    );
    assert!(
        engine.snapshot().position_ms >= before.position_ms,
        "幂等换轨后位置只能原地或继续前进"
    );
    engine.destroy();
}

#[test]
fn audio_switch_keeps_the_video_and_never_moves_the_position_back() {
    let asset = asset_path();
    if skip_if_absent(&asset) {
        return;
    }
    let (mut engine, rx) = Engine::new().expect("engine must start");
    let before_position = load_and_play(&mut engine, &rx, asset.as_ref().unwrap());

    let before = engine.snapshot();
    let before_media = file(&before.media).clone();
    assert_eq!(selected(&before_media, TrackKind::Audio), vec![1]);
    assert_eq!(before_media.audio.as_ref().expect("audio").codec, "aac");

    engine
        .command(CommandEnvelope::with_request_id(
            53,
            Command::SelectTrack {
                kind: TrackKind::Audio,
                index: 2,
            },
        ))
        .expect("switch to the 44.1k mono flac track");

    let mut reloaded: Option<MediaInfo> = None;
    let mut saw_loading = 0;
    let mut landed = None;
    let (status, events) = drain_until_completed(&rx, 53);
    for event in &events {
        match event {
            Event::StateChanged(changed) => match changed.to {
                PlayerState::Loading => saw_loading += 1,
                PlayerState::Playing | PlayerState::Paused => {
                    landed = Some((changed.from, changed.to, changed.reason))
                }
                other => panic!("unexpected landing state {other:?}"),
            },
            Event::FileLoaded(loaded) => reloaded = Some(loaded.media.clone()),
            Event::SeekCompleted(_) => panic!("位置恢复是内部 seek，不得对外发 seek_completed"),
            _ => {}
        }
    }
    assert_eq!(status, CommandStatus::Ok);
    assert_eq!(saw_loading, 1, "换轨恰好经过一次 loading");
    let after = reloaded.expect("switch re-issues file_loaded");
    let (from, to, reason) = landed.expect("switch lands back on the running state");
    assert_eq!(from, PlayerState::Loading);
    assert_eq!(to, PlayerState::Playing);
    assert_eq!(reason, StateChangeReason::Restart);

    // 重开管线最容易悄悄改掉的就是画面：视频轨、分辨率、帧率、编解码都必须原样
    assert_eq!(after.video, before_media.video, "换音轨绝不能改变画面");
    assert_eq!(
        selected(&after, TrackKind::Video),
        selected(&before_media, TrackKind::Video)
    );
    assert_eq!(selected(&after, TrackKind::Audio), vec![2]);
    let audio = after.audio.as_ref().expect("audio after switch");
    assert_eq!(
        (audio.codec.as_str(), audio.channels, audio.sample_rate),
        ("flac", 1, 44_100)
    );
    // 没被要求换的那几条必须原样保留选中，字幕轨不许被重置成自动选择
    assert_eq!(
        selected(&after, TrackKind::Subtitle),
        selected(&before_media, TrackKind::Subtitle)
    );

    // 位置钉：切换瞬间与之后采样都不许回跳到更早的位置
    let mut positions = vec![engine.snapshot().position_ms];
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        positions.push(engine.snapshot().position_ms);
        std::thread::sleep(Duration::from_millis(50));
    }
    let worst_drop = positions
        .windows(2)
        .map(|window| window[0].saturating_sub(window[1]))
        .max()
        .unwrap_or(0);
    assert!(
        worst_drop < 100,
        "换轨后位置从 {before_position}ms 起跳回了 {worst_drop}ms：{positions:?}"
    );
    assert!(
        *positions.last().unwrap() >= before_position,
        "换轨后位置必须继续前进：{positions:?}"
    );
    assert_eq!(engine.snapshot().state, PlayerState::Playing);
    engine.destroy();
}

#[test]
fn switch_from_paused_lands_back_on_paused_at_the_same_position() {
    let asset = asset_path();
    if skip_if_absent(&asset) {
        return;
    }
    let (mut engine, rx) = Engine::new().expect("engine must start");
    load_and_play(&mut engine, &rx, asset.as_ref().unwrap());

    engine
        .command(CommandEnvelope::with_request_id(3, Command::Pause))
        .expect("pause is accepted");
    let deadline = Instant::now() + WAIT;
    while engine.snapshot().state != PlayerState::Paused {
        assert!(Instant::now() < deadline, "pause never landed");
        std::thread::sleep(Duration::from_millis(20));
    }
    let paused = engine.snapshot();

    engine
        .command(CommandEnvelope::with_request_id(
            54,
            Command::SelectTrack {
                kind: TrackKind::Audio,
                index: 2,
            },
        ))
        .expect("switching while paused is accepted");
    let (status, events) = drain_until_completed(&rx, 54);
    assert_eq!(status, CommandStatus::Ok);
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::StateChanged(changed)
                if changed.from == PlayerState::Loading && changed.to == PlayerState::Paused
        )),
        "暂停中换轨必须落回暂停：{events:?}"
    );

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.state, PlayerState::Paused);
    assert!(
        (snapshot.position_ms as i64 - paused.position_ms as i64).abs() < 100,
        "换轨前后位置应停在同一处：{}ms vs {}ms",
        snapshot.position_ms,
        paused.position_ms
    );
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        engine.snapshot().position_ms,
        snapshot.position_ms,
        "换轨后不能自己跑起来"
    );
    engine.destroy();
}

#[test]
fn subtitle_switch_keeps_an_external_subtitle_in_charge() {
    let asset = asset_path();
    if skip_if_absent(&asset) {
        return;
    }
    let external = std::env::temp_dir().join("dyl_track_switch_external.srt");
    std::fs::write(
        &external,
        "1\n00:00:01,000 --> 00:00:04,000\nexternal subtitle\n\n",
    )
    .expect("write external srt");

    let (mut engine, rx) = Engine::new().expect("engine must start");
    load_and_play(&mut engine, &rx, asset.as_ref().unwrap());

    engine
        .command(CommandEnvelope::with_request_id(
            4,
            Command::LoadSubtitle {
                path: external.to_string_lossy().to_string(),
            },
        ))
        .expect("load_subtitle is accepted");
    let deadline = Instant::now() + WAIT;
    while !engine.snapshot().subtitle.loaded {
        assert!(Instant::now() < deadline, "external subtitle never loaded");
        std::thread::sleep(Duration::from_millis(20));
    }
    let before = engine.snapshot();
    assert_eq!(
        before.subtitle.source.as_deref(),
        Some(external.to_string_lossy().as_ref())
    );

    engine
        .command(CommandEnvelope::with_request_id(
            55,
            Command::SelectTrack {
                kind: TrackKind::Subtitle,
                index: 4,
            },
        ))
        .expect("switch between two text subtitle tracks");
    let (status, events) = drain_until_completed(&rx, 55);
    assert_eq!(status, CommandStatus::Ok, "{events:?}");

    let snapshot = engine.snapshot();
    assert_eq!(
        selected(file(&snapshot.media), TrackKind::Subtitle),
        vec![4]
    );
    // 内嵌轨换选不能把外挂字幕挤掉：external_source 决定内嵌事件一律不采纳
    assert!(snapshot.subtitle.loaded, "换内嵌字幕轨不得踢掉外挂字幕");
    assert_eq!(snapshot.subtitle.source, before.subtitle.source);
    assert_eq!(snapshot.state, PlayerState::Playing);
    engine.destroy();
    let _ = std::fs::remove_file(&external);
}

/// 等到指定请求的 command_completed，返回途中收到的全部事件
fn wait_ok(rx: &Receiver<EventEnvelope>, request_id: u64) -> Vec<Event> {
    let deadline = Instant::now() + WAIT;
    let mut events = Vec::new();
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for command_completed");
        let envelope = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("event channel closed before request {request_id}"));
        if let Event::CommandCompleted(completed) = &envelope.event {
            if completed.request_id == request_id {
                assert_eq!(completed.status, CommandStatus::Ok, "{completed:?}");
                return events;
            }
        }
        events.push(envelope.event);
    }
}

/// 回归：从 ended/stopped 重开（play/seek 重启）必须带上当前显式选择。
/// 传 `TrackSelection::default()` 会让 demuxer 按 default 标记重选，把用户选的
/// FLAC 音轨和 SRT 字幕轨悄悄换回 AAC/ASS，而且不重发 file_loaded，界面毫无知觉。
#[test]
fn restart_from_stopped_keeps_the_selected_tracks() {
    let asset = asset_path();
    if skip_if_absent(&asset) {
        return;
    }
    let (mut engine, rx) = Engine::new().expect("engine must start");
    load_and_play(&mut engine, &rx, asset.as_ref().unwrap());

    engine
        .command(CommandEnvelope::with_request_id(
            60,
            Command::SelectTrack {
                kind: TrackKind::Audio,
                index: 2,
            },
        ))
        .expect("switch to the flac track");
    wait_ok(&rx, 60);
    engine
        .command(CommandEnvelope::with_request_id(
            61,
            Command::SelectTrack {
                kind: TrackKind::Subtitle,
                index: 4,
            },
        ))
        .expect("switch to the srt track");
    wait_ok(&rx, 61);

    engine
        .command(CommandEnvelope::with_request_id(62, Command::Stop))
        .expect("stop is accepted while playing");
    wait_ok(&rx, 62);
    assert_eq!(engine.snapshot().state, PlayerState::Stopped);

    engine
        .command(CommandEnvelope::with_request_id(63, Command::Play))
        .expect("play from stopped restarts the pipeline");
    wait_ok(&rx, 63);
    let snapshot = engine.snapshot();
    let media = file(&snapshot.media);
    assert_eq!(
        selected(media, TrackKind::Audio),
        vec![2],
        "play 重开丢了音轨选择"
    );
    assert_eq!(
        selected(media, TrackKind::Subtitle),
        vec![4],
        "play 重开丢了字幕选择"
    );
    assert_eq!(media.audio.as_ref().expect("audio").codec, "flac");

    engine
        .command(CommandEnvelope::with_request_id(64, Command::Stop))
        .expect("stop again");
    wait_ok(&rx, 64);
    engine
        .command(CommandEnvelope::with_request_id(
            65,
            Command::Seek {
                position_ms: 1_000,
                mode: SeekMode::Exact,
            },
        ))
        .expect("seek from stopped reopens the pipeline");
    let events = wait_ok(&rx, 65);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::SeekCompleted(_))),
        "从 stopped seek 必须发 seek_completed：{events:?}"
    );
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.state, PlayerState::Paused);
    let media = file(&snapshot.media);
    assert_eq!(
        selected(media, TrackKind::Audio),
        vec![2],
        "seek 重开丢了音轨选择"
    );
    assert_eq!(
        selected(media, TrackKind::Subtitle),
        vec![4],
        "seek 重开丢了字幕选择"
    );
    engine.destroy();
}
