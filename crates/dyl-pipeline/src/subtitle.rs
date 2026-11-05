//! 字幕解码线程 — 从 subtitle packet 解析事件并边解码边注入 SubtitleEngine
//!
//! 内封 ASS/SSA 轨的 payload 是**对话行字段**（`Layer,Style,Name,MarginL,MarginR,
//! MarginV,Effect,Text`），不是可显示文本，必须按 ASS 语义解析后才能交给自建渲染器
//! （去 `{...}` 覆盖标签、`\N` 转换行、带真实样式名）。纯文本轨（SRT / mov_text）
//! 仍按裸文本处理。

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossbeam_channel::Receiver;
use dyl_ffmpeg::{Packet, SubtitleCodecId};
use dyl_subtitle::{AssEvent, SubtitleEngine, SubtitleEvent};
use parking_lot::Mutex;

use crate::seek::{recv_current, Stamped};

/// 字幕默认显示时长（秒）—— 仅用于容器没给 duration 的包
const DEFAULT_SUBTITLE_DURATION: f64 = 3.0;

/// 用后一条字幕的 pts 来完成前一条时，允许的最大间隔（秒）
const MAX_GAP_FROM_NEXT: f64 = 30.0;

/// 字幕包接收超时
const RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// 字幕 payload 的解析方式，由所选字幕流的编码决定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitlePacketMode {
    /// ASS/SSA 对话行字段
    AssDialogue,
    /// 裸 UTF-8 文本（SRT、mov_text 等）
    PlainText,
    /// 位图字幕等本引擎解不出文本的编码
    Unsupported,
}

impl SubtitlePacketMode {
    /// 与 demuxer 选轨判据同源：位图字幕选进来也解不出文本
    pub fn for_codec(codec: Option<SubtitleCodecId>) -> Self {
        match codec {
            Some(id) if id.is_ass_like() => Self::AssDialogue,
            Some(id) if id.is_text() => Self::PlainText,
            _ => Self::Unsupported,
        }
    }
}

/// 字幕解码线程 — 读取 subtitle packets，解析事件，送入 SubtitleEngine
pub fn subtitle_decode_loop(
    pipeline: Arc<crate::pipeline::Pipeline>,
    subtitle_pkt_rx: Receiver<Stamped<Packet>>,
    subtitle_engine: Arc<Mutex<SubtitleEngine>>,
    codec: Option<SubtitleCodecId>,
) -> dyl_core::Result<()> {
    let mode = SubtitlePacketMode::for_codec(codec);
    tracing::info!(
        "subtitle_decode_loop started (codec={:?} mode={:?})",
        codec,
        mode
    );

    let mut prev: Option<AssEvent> = None;
    let mut last_seek_gen: u64 = pipeline.seek_generation.load(Ordering::Acquire);
    // 容器没给 duration 时，一条字幕的结束时间要等**后一条**的 pts 才能定；因此它
    // 在收到后一条时才完成 —— 完成后立刻注入引擎，不能等循环退出（EOF 之后）
    let mut buffered: Vec<AssEvent> = Vec::new();
    // seek 回退会把同一段字幕重新投递一遍，用已注入集合去重，
    // 否则事件表随 seek 次数无限增长且渲染端重复匹配
    let mut pushed: HashSet<(u64, String)> = HashSet::new();

    loop {
        if pipeline.shutdown.load(Ordering::Acquire) {
            break;
        }

        let seek_gen = pipeline.seek_generation.load(Ordering::Acquire);
        if seek_gen != last_seek_gen {
            last_seek_gen = seek_gen;
            // 上一条字幕的结束时间本应由下一个包决定，seek 后新位置的包不属于它；
            // 按默认时长收尾即可，直接丢弃会少一条字幕
            if let Some(ev) = prev.take() {
                let start = ev.base.start_time;
                complete(
                    &mut buffered,
                    &mut pushed,
                    ev,
                    start + DEFAULT_SUBTITLE_DURATION,
                );
            }
        }

        match recv_current(&subtitle_pkt_rx, seek_gen, RECV_TIMEOUT) {
            Ok(stamped) => on_packet(stamped.item, mode, &mut prev, &mut buffered, &mut pushed),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        // 超时、跳包等分支也必须走到这里：seek 收尾的事件若等下一个包才注入，
        // seek 之后就出现一段无字幕的空窗（极端情况下要等到 EOF 才显示）
        flush_buffered(&subtitle_engine, &mut buffered);
    }

    // flush 最后一条字幕
    if let Some(ev) = prev {
        let start = ev.base.start_time;
        complete(
            &mut buffered,
            &mut pushed,
            ev,
            start + DEFAULT_SUBTITLE_DURATION,
        );
    }
    flush_buffered(&subtitle_engine, &mut buffered);

    tracing::info!("subtitle_decode_loop exiting");
    Ok(())
}

/// 处理一个字幕包
///
/// 带 duration 的包（Matroska 字幕轨一定有）自身就是完整的时间轴，立刻完成；
/// 没带的仍要等**下一个**包到达才能确定结束时间，于是它成为新的"上一条"。
fn on_packet(
    pkt: Packet,
    mode: SubtitlePacketMode,
    prev: &mut Option<AssEvent>,
    buffered: &mut Vec<AssEvent>,
    pushed: &mut HashSet<(u64, String)>,
) {
    // 无时间戳的字幕包无法定位显示时间，直接跳过（不得当成 0 秒）
    let Some(start) = pkt.pts_secs() else {
        tracing::debug!("subtitle packet without pts, skipped");
        return;
    };
    // 包给的是**时长**，绝对结束时间要加上起始时间
    let end = pkt.duration_secs().filter(|d| *d > 0.0).map(|d| start + d);

    // 上一条字幕在此完成（它没带 duration，才轮得到本包的起始时间来收尾）
    if let Some(pending) = prev.take() {
        let default_end = pending.base.start_time + DEFAULT_SUBTITLE_DURATION;
        let end = if start > pending.base.start_time
            && start - pending.base.start_time < MAX_GAP_FROM_NEXT
        {
            start
        } else {
            default_end
        };
        complete(buffered, pushed, pending, end);
    }

    let Some(event) = parse_packet(
        &pkt,
        mode,
        start,
        end.unwrap_or(start + DEFAULT_SUBTITLE_DURATION),
    ) else {
        return;
    };
    match end {
        // 时间轴完整：立刻完成。仍要走 `complete`，因为去重对这条路径同样必要 ——
        // seek 回退会把同一批带 duration 的包重新投递一遍（Matroska 一定有 duration）
        Some(end) => complete(buffered, pushed, event, end),
        None => *prev = Some(event),
    }
}

/// 解析一个字幕包的 payload
fn parse_packet(pkt: &Packet, mode: SubtitlePacketMode, start: f64, end: f64) -> Option<AssEvent> {
    if pkt.data.is_empty() {
        return None;
    }
    match mode {
        SubtitlePacketMode::AssDialogue => {
            // 容器里的字幕文本按 UTF-8 存放；个别 muxer 会留下非法字节，
            // 坏一个字符远好过整条字幕消失
            let raw = String::from_utf8_lossy(&pkt.data);
            dyl_subtitle::parse_embedded_dialogue(&raw, start, end)
        }
        SubtitlePacketMode::PlainText => extract_text_from_subtitle_packet(pkt).map(|text| {
            AssEvent::plain(SubtitleEvent {
                start_time: start,
                end_time: end,
                text,
                style: "Default".to_string(),
            })
        }),
        // 非文本字幕格式（如 DVB/PGS bitmap），暂不支持
        SubtitlePacketMode::Unsupported => None,
    }
}

/// 定下结束时间并记录一条字幕事件；重复的（seek 回退后重新投递）直接忽略
fn complete(
    buffered: &mut Vec<AssEvent>,
    pushed: &mut HashSet<(u64, String)>,
    mut event: AssEvent,
    end_time: f64,
) {
    event.base.end_time = end_time;
    // 去重键不含 end_time：重新投递时相邻包序一致，结束时间必然相同，
    // 若把 end_time 也纳入键则 seek 后会插入视觉重复的同一条字幕
    let key = (event.base.start_time.to_bits(), event.base.text.clone());
    if !pushed.insert(key) {
        return;
    }
    buffered.push(event);
}

/// 把已完成的事件送入引擎，使其立即可被渲染端取到
fn flush_buffered(engine: &Arc<Mutex<SubtitleEngine>>, buffered: &mut Vec<AssEvent>) {
    if buffered.is_empty() {
        return;
    }
    let events = std::mem::take(buffered);
    let count = events.len();
    engine.lock().append_ass_events(events);
    tracing::debug!("subtitle_decode_loop: appended {} events", count);
}

/// 从纯文本字幕 packet 中提取文本内容
///
/// 支持格式：
/// - **mov_text**: 前 2 字节为 big-endian 长度前缀，后接 UTF-8 文本
/// - **subrip / SRT**: 直接 UTF-8 文本
fn extract_text_from_subtitle_packet(pkt: &Packet) -> Option<String> {
    if pkt.data.is_empty() {
        return None;
    }

    // 尝试 mov_text 格式（2 字节长度前缀）
    if pkt.data.len() >= 2 {
        let len = ((pkt.data[0] as u16) << 8 | pkt.data[1] as u16) as usize;
        if len > 0 && len + 2 <= pkt.data.len() {
            if let Ok(text) = std::str::from_utf8(&pkt.data[2..2 + len]) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }

    // 回退：尝试直接解析为 UTF-8 文本
    if let Ok(text) = std::str::from_utf8(&pkt.data) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyl_subtitle::SubtitleConfig;

    /// 构造测试用字幕包（时间基 1/1000，pts/duration 单位为毫秒）
    fn sub_packet(pts: Option<i64>, duration: Option<i64>, data: Vec<u8>) -> Packet {
        Packet {
            data,
            stream_index: 0,
            pts,
            dts: pts,
            duration,
            time_base: dyl_ffmpeg::time_base(1, 1000),
            is_key: false,
        }
    }

    /// 真实内封轨 payload：字段间无空格，正文被真 TAB 包裹并带覆盖标签与换行符
    const EMBEDDED_ASS_LINE: &[u8] =
        "0,0,Default,,0,0,0,,\t\t祝大家新年快乐\t\\N{\\rDefault-JP}\t\tHappy New Year".as_bytes();

    /// 轮询等待条件成立（用于观察"流未结束时的引擎状态"）
    fn wait_for(mut cond: impl FnMut() -> bool, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cond()
    }

    fn spawn_loop(
        pipeline: Arc<crate::pipeline::Pipeline>,
        pkt_rx: Receiver<Stamped<Packet>>,
        engine: Arc<Mutex<SubtitleEngine>>,
        codec: Option<SubtitleCodecId>,
    ) -> std::thread::JoinHandle<dyl_core::Result<()>> {
        std::thread::spawn(move || subtitle_decode_loop(pipeline, pkt_rx, engine, codec))
    }

    fn test_pipeline() -> Arc<crate::pipeline::Pipeline> {
        let (event_tx, _event_rx) =
            crossbeam_channel::unbounded::<crate::event::PipelineEventMsg>();
        Arc::new(crate::pipeline::Pipeline::new(event_tx, 0))
    }

    /// 防回归：内封 ASS 必须走自建 ASS 解析路径 —— 显示的是正文，不是裸对话字段
    #[test]
    fn embedded_ass_payload_is_parsed_as_dialogue() {
        let pipeline = test_pipeline();
        let (pkt_tx, pkt_rx) = crossbeam_channel::bounded::<Stamped<Packet>>(8);
        let engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
        let state = engine.lock().shared_state();

        let handle = spawn_loop(
            pipeline.clone(),
            pkt_rx,
            engine.clone(),
            Some(SubtitleCodecId::Ass),
        );

        // pts 4.100s + duration 3.760s：Matroska 字幕轨自带结束时间
        pkt_tx
            .send(Stamped::new(
                0,
                sub_packet(Some(4100), Some(3760), EMBEDDED_ASS_LINE.to_vec()),
            ))
            .unwrap();
        assert!(wait_for(
            || !state.lock().ass_events.is_empty(),
            std::time::Duration::from_secs(2)
        ));

        drop(pkt_tx);
        handle.join().unwrap().unwrap();

        let s = state.lock();
        assert_eq!(s.ass_events.len(), 1, "{:?}", s.events);
        let ev = &s.ass_events[0];
        assert_eq!(ev.base.style, "Default", "样式名取自对话行第 3 字段");
        assert!((ev.base.start_time - 4.1).abs() < 1e-9);
        assert!(
            (ev.base.end_time - 7.86).abs() < 1e-6,
            "结束时间 = pts + 包 duration，got {:?}",
            ev.base.end_time
        );
        assert_eq!(
            ev.base.text, "祝大家新年快乐\nHappy New Year",
            "覆盖标签要剥掉、\\N 要转换行"
        );
        assert!(
            !ev.base.text.contains("Default,,") && !ev.base.text.contains('{'),
            "不得残留字段或标签：{:?}",
            ev.base.text
        );
        // 渲染端只读 ass_events，纯文本视图必须同步
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].text, ev.base.text);
    }

    /// muxer 会把 `[Events] Format:` 这类脚本头也当字幕包投递：没有正文，不能上屏
    #[test]
    fn script_header_packet_is_not_shown() {
        let mode = SubtitlePacketMode::for_codec(Some(SubtitleCodecId::Ass));
        let header =
            b"Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";
        let pkt = sub_packet(Some(1000), Some(1000), header.to_vec());
        assert!(
            parse_packet(&pkt, mode, 1.0, 2.0).is_none(),
            "字段校验必须挡掉脚本头"
        );
    }

    /// 内封 ASS 的 payload 允许出现在正文里的逗号
    #[test]
    fn ass_text_field_keeps_commas() {
        let mode = SubtitlePacketMode::for_codec(Some(SubtitleCodecId::Ass));
        let pkt = sub_packet(
            Some(0),
            Some(1000),
            b"7,0,Default,,0,0,0,,\xe4\xbd\xa0\xe5\xa5\xbd,\xe4\xb8\x96\xe7\x95\x8c".to_vec(),
        );
        let ev = parse_packet(&pkt, mode, 0.0, 1.0).expect("parses");
        assert_eq!(ev.base.text, "你好,世界");
        assert_eq!(ev.base.style, "Default");
    }

    /// 位图字幕不会被误当文本（与 demuxer 选轨判据同源）
    #[test]
    fn codec_modes_are_dispatched_from_capability() {
        assert_eq!(
            SubtitlePacketMode::for_codec(Some(SubtitleCodecId::Ass)),
            SubtitlePacketMode::AssDialogue
        );
        assert_eq!(
            SubtitlePacketMode::for_codec(Some(SubtitleCodecId::Ssa)),
            SubtitlePacketMode::AssDialogue
        );
        assert_eq!(
            SubtitlePacketMode::for_codec(Some(SubtitleCodecId::SubRip)),
            SubtitlePacketMode::PlainText
        );
        assert_eq!(
            SubtitlePacketMode::for_codec(Some(SubtitleCodecId::MovText)),
            SubtitlePacketMode::PlainText
        );
        assert_eq!(
            SubtitlePacketMode::for_codec(Some(SubtitleCodecId::Unknown)),
            SubtitlePacketMode::Unsupported
        );
        assert_eq!(
            SubtitlePacketMode::for_codec(None),
            SubtitlePacketMode::Unsupported
        );
    }

    /// 防回归：内嵌字幕必须在 **流结束之前** 就进入引擎。
    /// 旧实现把事件累积到局部 Vec、仅在循环退出后 load_events，
    /// 导致内嵌字幕轨在整个播放过程中一条都不显示。
    #[test]
    fn test_events_reach_engine_before_stream_end() {
        let pipeline = test_pipeline();
        let (pkt_tx, pkt_rx) = crossbeam_channel::bounded::<Stamped<Packet>>(8);
        let engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
        let state = engine.lock().shared_state();

        let handle = spawn_loop(
            pipeline.clone(),
            pkt_rx,
            engine.clone(),
            Some(SubtitleCodecId::SubRip),
        );

        // 无 duration 的包：第一条的结束时间由第二条的 pts 决定
        pkt_tx
            .send(Stamped::new(
                0,
                sub_packet(Some(1000), None, b"first".to_vec()),
            ))
            .unwrap();
        pkt_tx
            .send(Stamped::new(
                0,
                sub_packet(Some(2000), None, b"second".to_vec()),
            ))
            .unwrap();

        let arrived = wait_for(
            || !state.lock().events.is_empty(),
            std::time::Duration::from_secs(2),
        );
        assert!(
            arrived,
            "内嵌字幕必须在 EOF 之前就进入引擎，而不是等解码循环退出"
        );

        // 此时流仍未结束
        drop(pkt_tx);
        handle.join().unwrap().unwrap();

        let s = state.lock();
        assert_eq!(
            s.events.len(),
            2,
            "两条字幕都应注入（含最后一条按默认时长收尾）"
        );
        assert_eq!(s.events[0].start_time, 1.0);
        assert_eq!(s.events[0].end_time, 2.0, "结束时间应取后一条的 pts");
        assert_eq!(
            s.ass_events.len(),
            2,
            "渲染端以 ass_events 为数据源，必须同步追加"
        );
    }

    /// 带 duration 的包不等后一条：自身时间轴完整，立刻可显示
    #[test]
    fn timed_packet_completes_immediately() {
        let pipeline = test_pipeline();
        let (pkt_tx, pkt_rx) = crossbeam_channel::bounded::<Stamped<Packet>>(8);
        let engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
        let state = engine.lock().shared_state();

        let handle = spawn_loop(
            pipeline.clone(),
            pkt_rx,
            engine.clone(),
            Some(SubtitleCodecId::SubRip),
        );

        pkt_tx
            .send(Stamped::new(
                0,
                sub_packet(Some(1000), Some(2500), b"only".to_vec()),
            ))
            .unwrap();
        assert!(wait_for(
            || !state.lock().events.is_empty(),
            std::time::Duration::from_secs(2)
        ));

        let first = state.lock().events[0].end_time;
        assert_eq!(first, 3.5, "结束时间来自包 duration");

        // 后一条的 pts 不得回头改写已经完成的那条
        pkt_tx
            .send(Stamped::new(
                0,
                sub_packet(Some(9000), Some(1000), b"next".to_vec()),
            ))
            .unwrap();
        assert!(wait_for(
            || state.lock().events.len() >= 2,
            std::time::Duration::from_secs(2)
        ));
        assert_eq!(state.lock().events[0].end_time, first);

        drop(pkt_tx);
        handle.join().unwrap().unwrap();
    }

    /// 防回归：seek 回退后重新投递的同一条字幕不得重复堆积
    #[test]
    fn test_seek_reattach_does_not_duplicate_events() {
        let pipeline = test_pipeline();
        let (pkt_tx, pkt_rx) = crossbeam_channel::bounded::<Stamped<Packet>>(16);
        let engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
        let state = engine.lock().shared_state();

        let handle = spawn_loop(
            pipeline.clone(),
            pkt_rx,
            engine.clone(),
            Some(SubtitleCodecId::SubRip),
        );

        // 位置 A：1s → 2s
        for (gen, pts, text) in [(0u64, 1000i64, "first"), (0, 2000, "second")] {
            pipeline.seek_generation.store(gen, Ordering::Release);
            pkt_tx
                .send(Stamped::new(
                    gen,
                    sub_packet(Some(pts), None, text.as_bytes().to_vec()),
                ))
                .unwrap();
        }
        assert!(wait_for(
            || !state.lock().events.is_empty(),
            std::time::Duration::from_secs(2)
        ));

        // seek 回退：同一段字幕被重新投递（generation 递增）
        for (gen, pts, text) in [(1u64, 1000i64, "first"), (1, 2000, "second")] {
            pipeline.seek_generation.store(gen, Ordering::Release);
            pkt_tx
                .send(Stamped::new(
                    gen,
                    sub_packet(Some(pts), None, text.as_bytes().to_vec()),
                ))
                .unwrap();
        }

        // 等两轮都处理完
        assert!(wait_for(
            || state.lock().events.len() >= 2,
            std::time::Duration::from_millis(400)
        ));
        let dup = state.lock().events.len();
        drop(pkt_tx);
        handle.join().unwrap().unwrap();

        let s = state.lock();
        assert_eq!(
            s.events.len(),
            dup,
            "seek 后重新投递不应继续增加事件（去重生效）"
        );
        assert_eq!(s.events.len(), 2, "只应保留两条唯一字幕");
    }

    /// 防回归：带 duration 的包走"立刻完成"分支，去重必须同样生效 ——
    /// Matroska 字幕轨每个包都带 duration，seek 回退重新投递时若绕过去重就会
    /// 让事件表随 seek 次数翻倍增长
    #[test]
    fn timed_packets_are_deduped_after_seek() {
        let pipeline = test_pipeline();
        let (pkt_tx, pkt_rx) = crossbeam_channel::bounded::<Stamped<Packet>>(16);
        let engine = Arc::new(Mutex::new(SubtitleEngine::new(SubtitleConfig::default())));
        let state = engine.lock().shared_state();

        let handle = spawn_loop(
            pipeline.clone(),
            pkt_rx,
            engine.clone(),
            Some(SubtitleCodecId::Ass),
        );

        let packets = [
            (0u64, 1000i64, "0,0,Default,,0,0,0,,第一条"),
            (0, 5000, "1,0,Default,,0,0,0,,第二条"),
        ];
        for (gen, pts, payload) in packets {
            pipeline.seek_generation.store(gen, Ordering::Release);
            pkt_tx
                .send(Stamped::new(
                    gen,
                    sub_packet(Some(pts), Some(1000), payload.as_bytes().to_vec()),
                ))
                .unwrap();
        }
        assert!(wait_for(
            || state.lock().events.len() >= 2,
            std::time::Duration::from_secs(2)
        ));

        // seek 回退：同一批带 duration 的包重新投递
        for (gen, pts, payload) in [(1u64, 1000i64, packets[0].2), (1, 5000, packets[1].2)] {
            pipeline.seek_generation.store(gen, Ordering::Release);
            pkt_tx
                .send(Stamped::new(
                    gen,
                    sub_packet(Some(pts), Some(1000), payload.as_bytes().to_vec()),
                ))
                .unwrap();
        }
        // 让循环把第二轮处理完（多跑几帧确保没有迟到追加）
        std::thread::sleep(std::time::Duration::from_millis(400));
        let total = state.lock().events.len();
        drop(pkt_tx);
        handle.join().unwrap().unwrap();

        let s = state.lock();
        assert_eq!(s.events.len(), total, "重投递不应继续增加事件");
        assert_eq!(s.events.len(), 2, "只应保留两条唯一字幕: {:?}", s.events);
        assert_eq!(s.ass_events.len(), 2);
    }

    #[test]
    fn test_extract_mov_text() {
        // mov_text: 2 bytes length (big-endian) + text
        let text = "Hello World";
        let len = text.len() as u16;
        let mut data = vec![(len >> 8) as u8, (len & 0xFF) as u8];
        data.extend_from_slice(text.as_bytes());

        let pkt = sub_packet(Some(1000), None, data);

        let result = extract_text_from_subtitle_packet(&pkt);
        assert_eq!(result, Some("Hello World".to_string()));
        assert!((pkt.pts_secs().unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_extract_plain_text() {
        let pkt = sub_packet(Some(2000), None, b"plain subtitle text".to_vec());

        let result = extract_text_from_subtitle_packet(&pkt);
        assert_eq!(result, Some("plain subtitle text".to_string()));
    }

    #[test]
    fn test_extract_empty_packet() {
        let pkt = sub_packet(None, None, vec![]);

        let result = extract_text_from_subtitle_packet(&pkt);
        assert_eq!(result, None);
        // NOPTS 包不得被解释成 0 秒
        assert_eq!(pkt.pts_secs(), None);
    }
}
