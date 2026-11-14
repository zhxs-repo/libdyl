//! `set_ab_loop` 回归 — 参数校验、循环播放的内部 seek 记账、新文件清除。
//!
//! AB 循环的跳转是内部 seek：不对应任何命令，因此绝不能产生
//! `command_completed`（历史 bug：request_id=0 的幻影完成）或 `seek_completed`。
//! 真实文件部分用 `test_video.mp4`（5s），缺失时跳过。

use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dyl_core::contract::{
    AbLoop, Command, CommandCompleted, CommandEnvelope, CommandOp, CommandStatus, ErrorCode, Event,
    EventEnvelope, SeekMode,
};
use dyl_engine::Engine;

const WAIT: Duration = Duration::from_secs(20);

mod common;

fn drain_until_completed(
    rx: &Receiver<EventEnvelope>,
    request_id: u64,
    op: CommandOp,
) -> (CommandCompleted, Vec<Event>) {
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
                assert_eq!(completed.op, op);
                return (completed.clone(), events);
            }
        }
        events.push(envelope.event);
    }
}

fn send(engine: &Engine, request_id: u64, command: Command) {
    engine
        .command(CommandEnvelope::with_request_id(request_id, command))
        .expect("command accepted into queue");
}

#[test]
fn set_ab_loop_validates_arguments() {
    let (mut engine, rx) = Engine::new().expect("engine must start");

    send(
        &engine,
        1,
        Command::SetAbLoop {
            a_ms: Some(1000),
            b_ms: Some(2000),
        },
    );
    let (completed, _) = drain_until_completed(&rx, 1, CommandOp::SetAbLoop);
    assert_eq!(completed.status, CommandStatus::Ok);
    assert_eq!(
        engine.snapshot().ab_loop,
        Some(AbLoop {
            a_ms: 1000,
            b_ms: 2000
        })
    );

    // 只给一端、a == b、a > b 都是非法参数，且不得改动已生效的区间
    let invalid = [
        (Some(1000), None),
        (None, Some(2000)),
        (Some(2000), Some(2000)),
        (Some(3000), Some(2000)),
    ];
    for (index, (a_ms, b_ms)) in invalid.into_iter().enumerate() {
        let request_id = 10 + index as u64;
        send(&engine, request_id, Command::SetAbLoop { a_ms, b_ms });
        let (completed, _) = drain_until_completed(&rx, request_id, CommandOp::SetAbLoop);
        assert_eq!(
            completed.status,
            CommandStatus::Error,
            "case {index}: ({a_ms:?}, {b_ms:?})"
        );
        assert_eq!(
            completed.error.as_ref().map(|error| error.code),
            Some(ErrorCode::InvalidArgument),
            "case {index}: ({a_ms:?}, {b_ms:?})"
        );
        assert_eq!(
            engine.snapshot().ab_loop,
            Some(AbLoop {
                a_ms: 1000,
                b_ms: 2000
            }),
            "case {index}: 非法参数不得改动已生效的区间"
        );
    }

    send(
        &engine,
        20,
        Command::SetAbLoop {
            a_ms: None,
            b_ms: None,
        },
    );
    let (completed, _) = drain_until_completed(&rx, 20, CommandOp::SetAbLoop);
    assert_eq!(completed.status, CommandStatus::Ok);
    assert_eq!(engine.snapshot().ab_loop, None);
    engine.destroy();
}

#[test]
fn ab_loop_playback_has_no_phantom_completions() {
    let Some(asset) = common::asset("test_video.mp4") else {
        eprintln!("SKIP: test_video.mp4 不存在");
        return;
    };
    let (mut engine, rx) = Engine::new().expect("engine must start");

    send(
        &engine,
        1,
        Command::LoadFile {
            url: asset.to_string_lossy().to_string(),
        },
    );
    let (completed, _) = drain_until_completed(&rx, 1, CommandOp::LoadFile);
    assert_eq!(completed.status, CommandStatus::Ok);

    send(
        &engine,
        2,
        Command::SetAbLoop {
            a_ms: Some(500),
            b_ms: Some(1500),
        },
    );
    let (completed, _) = drain_until_completed(&rx, 2, CommandOp::SetAbLoop);
    assert_eq!(completed.status, CommandStatus::Ok);

    send(&engine, 3, Command::Play);
    let (completed, _) = drain_until_completed(&rx, 3, CommandOp::Play);
    assert_eq!(completed.status, CommandStatus::Ok);

    // 观察 4 秒（循环周期约 1s）：内部 seek 不得产生任何命令完成 / seek 完成事件，
    // 位置不得越过 B 点太多（tick 100ms + seek 落地延迟），且至少回跳两次
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut wraps = 0;
    let mut last_pos = 0u64;
    let mut saw_loop = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(envelope) => match envelope.event {
                Event::CommandCompleted(completed) => {
                    panic!("AB 循环不得产生 command_completed: {completed:?}")
                }
                Event::SeekCompleted(seek) => {
                    panic!("AB 内部 seek 不得发 seek_completed: {seek:?}")
                }
                _ => {}
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                panic!("event channel closed during AB playback")
            }
        }
        let pos = engine.snapshot().position_ms;
        assert!(pos <= 2300, "位置越过 B 点太多：{pos}ms");
        if saw_loop {
            assert!(pos >= 200, "回跳后位置低于 A 点太多：{pos}ms");
        }
        if last_pos >= 1200 && pos + 500 < last_pos {
            wraps += 1;
            saw_loop = true;
        }
        last_pos = pos;
    }
    assert!(wraps >= 2, "4 秒内至少回跳两次，实际 {wraps} 次");
    assert_eq!(
        engine.snapshot().ab_loop,
        Some(AbLoop {
            a_ms: 500,
            b_ms: 1500
        })
    );

    // AB 生效期间用户 seek 照常完成（内部 seek 不占用命令记账）
    send(
        &engine,
        4,
        Command::Seek {
            position_ms: 3000,
            mode: SeekMode::Exact,
        },
    );
    let (completed, events) = drain_until_completed(&rx, 4, CommandOp::Seek);
    assert_eq!(completed.status, CommandStatus::Ok);
    let seek_completed = events
        .iter()
        .filter(|event| matches!(event, Event::SeekCompleted(seek) if seek.request_id == 4))
        .count();
    assert_eq!(seek_completed, 1, "用户 seek 恰好一个 seek_completed");

    // 清除循环后再 seek：位置继续推进，不再被拉回 A 点
    send(
        &engine,
        5,
        Command::SetAbLoop {
            a_ms: None,
            b_ms: None,
        },
    );
    let (completed, _) = drain_until_completed(&rx, 5, CommandOp::SetAbLoop);
    assert_eq!(completed.status, CommandStatus::Ok);
    send(
        &engine,
        6,
        Command::Seek {
            position_ms: 3000,
            mode: SeekMode::Exact,
        },
    );
    let (completed, _) = drain_until_completed(&rx, 6, CommandOp::Seek);
    assert_eq!(completed.status, CommandStatus::Ok);
    std::thread::sleep(Duration::from_millis(1200));
    let pos = engine.snapshot().position_ms;
    assert!(pos > 3500, "清除循环后位置应继续推进，got {pos}ms");
    engine.destroy();
}

#[test]
fn ab_loop_cleared_on_new_file_load() {
    let Some(asset) = common::asset("test_video.mp4") else {
        eprintln!("SKIP: test_video.mp4 不存在");
        return;
    };
    let (mut engine, rx) = Engine::new().expect("engine must start");
    let url = asset.to_string_lossy().to_string();

    send(&engine, 1, Command::LoadFile { url: url.clone() });
    let (completed, _) = drain_until_completed(&rx, 1, CommandOp::LoadFile);
    assert_eq!(completed.status, CommandStatus::Ok);

    send(
        &engine,
        2,
        Command::SetAbLoop {
            a_ms: Some(500),
            b_ms: Some(1500),
        },
    );
    let (completed, _) = drain_until_completed(&rx, 2, CommandOp::SetAbLoop);
    assert_eq!(completed.status, CommandStatus::Ok);
    assert!(engine.snapshot().ab_loop.is_some());

    // A/B 点属于旧文件的时间轴：加载新文件必须清除
    send(&engine, 3, Command::LoadFile { url });
    let (completed, _) = drain_until_completed(&rx, 3, CommandOp::LoadFile);
    assert_eq!(completed.status, CommandStatus::Ok);
    assert_eq!(engine.snapshot().ab_loop, None);
    engine.destroy();
}
