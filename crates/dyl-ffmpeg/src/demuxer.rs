use std::ffi::{c_int, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::source::{classify, SourceKind};
use crate::types::*;
use dyl_core::{DylError, Result};
use ffmpeg_next::format::stream::Disposition;

/// 网络读取的 I/O 超时（微秒）：连接建立、读包、seek 都受它约束。
///
/// 中断回调保证 `Pipeline::stop()` 不会被这个超时拖住（shutdown 一置位，
/// 阻塞中的 FFmpeg IO 立刻返回），所以它可以给得比本地读取宽得多，
/// 让 CDN 的短暂抖动有机会自己恢复。
const NETWORK_RW_TIMEOUT_US: i64 = 15_000_000;

/// 网络错误后的重连间隔上限（秒）。配合 `reconnect_on_network_error`，
/// 连接中途断开时 FFmpeg 会带 Range 头重连，播放器不必从 0 重开。
const NETWORK_RECONNECT_DELAY_MAX: i64 = 4;

/// 打开时的选轨意图。
///
/// 每一项 `None` 表示沿用内核自动选择（容器 default 标记优先，其次容器顺序）；
/// `Some(index)` 是容器内的流索引，必须与该流的实际类型相符且内核能解码。
/// [`TrackSelection::default`]（全 `None`）就是自动选轨，切轨时由引擎把「保持不变的
/// 那两条」显式填回来，否则重开会按 default 标记重新选一遍、把画面换掉。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackSelection {
    pub video: Option<usize>,
    pub audio: Option<usize>,
    pub subtitle: Option<usize>,
}

/// Demuxer trait — 解复用器抽象
pub trait Demuxer: Send {
    /// 打开媒体来源并按 `selection` 选轨。
    ///
    /// `shutdown` 是管线的停止标志：实现方在阻塞 IO（网络打开/读包/seek）
    /// 期间必须观察它，置位后尽快返回，否则 `Pipeline::stop()` 会卡在
    /// `read_packet` 里等网络超时。
    fn open(
        &mut self,
        url: &str,
        selection: &TrackSelection,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()>;

    /// 读取下一个数据包
    fn read_packet(&mut self) -> Result<Option<Packet>>;

    /// 跳转到指定时间（秒）
    fn seek(&mut self, target: f64) -> Result<()>;

    /// 获取媒体信息（open 后可用）
    fn info(&self) -> Option<&MediaInfo>;

    /// 获取视频流的 CodecContext（用于解码器初始化）
    fn take_video_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context>;

    /// 获取音频流的 CodecContext
    fn take_audio_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context>;

    /// 获取字幕流的 CodecContext
    fn take_subtitle_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context>;

    /// 关闭文件
    fn close(&mut self);
}

/// FFmpeg 解复用器实现
///
/// 使用 `Packet::read()` API 直接读取数据包，无需 PacketIter 生命周期 transmute。
pub struct FfmpegDemuxer {
    info: Option<MediaInfo>,
    video_codec_context: Option<ffmpeg_next::codec::Context>,
    audio_codec_context: Option<ffmpeg_next::codec::Context>,
    subtitle_codec_context: Option<ffmpeg_next::codec::Context>,
    input: Option<ffmpeg_next::format::context::Input>,
    /// 中断回调闭包的所有权：`AVFormatContext` 只存裸指针，闭包必须活到
    /// context 关闭之后（字段顺序保证 `input` 先 drop）
    interrupt: Option<InterruptHandle>,
    /// 缓存每个流的 time_base，避免每个 packet 查找
    time_bases: Vec<ffmpeg_next::Rational>,
}

// Safety: FfmpegDemuxer is only used from one thread at a time
unsafe impl Send for FfmpegDemuxer {}

impl FfmpegDemuxer {
    pub fn new() -> Self {
        Self {
            info: None,
            video_codec_context: None,
            audio_codec_context: None,
            subtitle_codec_context: None,
            input: None,
            interrupt: None,
            time_bases: Vec::new(),
        }
    }
}

impl Default for FfmpegDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl Demuxer for FfmpegDemuxer {
    fn open(
        &mut self,
        url: &str,
        selection: &TrackSelection,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()> {
        tracing::info!("FfmpegDemuxer::open({}) {:?}", url, selection);

        let kind = classify(url)?;
        let (input, interrupt) = open_input(url, kind, shutdown)?;

        // duration is in AV_TIME_BASE (microseconds)
        let duration = input.duration() as f64 / 1_000_000.0;

        // 第一遍：只看元数据，把轨道表整个建出来（UI 要能枚举，选轨要能读 default 标记）
        let mut probes: Vec<StreamProbe> = Vec::new();
        for (i, stream) in input.streams().enumerate() {
            let params = stream.parameters();
            let kind = match params.medium() {
                ffmpeg_next::media::Type::Video => TrackKind::Video,
                ffmpeg_next::media::Type::Audio => TrackKind::Audio,
                ffmpeg_next::media::Type::Subtitle => TrackKind::Subtitle,
                // 附件（字体/封面）与数据流不是可播放轨道
                _ => continue,
            };
            probes.push(StreamProbe {
                index: i,
                kind,
                codec_id: params.id(),
                default: stream.disposition().contains(Disposition::DEFAULT),
                language: dict_text(&stream, "language"),
                title: dict_text(&stream, "title"),
            });
        }

        // 第二遍：显式指定的轨必须校验，未指定的按 default 标记优先、其余容器顺序；
        // 内核只渲染文本字幕，位图字幕（PGS/DVB）选了解码线程也出不了字，宁可不选
        let video_stream_index = resolve_track(&probes, TrackKind::Video, selection.video)?;
        let audio_stream_index = resolve_track(&probes, TrackKind::Audio, selection.audio)?;
        let subtitle_stream_index =
            resolve_track(&probes, TrackKind::Subtitle, selection.subtitle)?;
        if subtitle_stream_index.is_none() {
            if let Some(p) = probes.iter().find(|p| p.kind == TrackKind::Subtitle) {
                tracing::info!(
                    "demuxer: subtitle stream {} ({:?}) is not a text format, skipped",
                    p.index,
                    p.codec_id
                );
            }
        }

        let mut width = 0u32;
        let mut height = 0u32;
        let mut display_width = 0u32;
        let mut display_height = 0u32;
        let mut pixel_format = PixelFormat::Unknown;
        let mut fps = 0.0;
        let mut video_codec_id = None;
        let mut audio_codec_id = None;
        let mut subtitle_codec_id = None;
        let mut sample_rate = 0u32;
        let mut channels = 0u32;
        let mut sample_format = SampleFormat::Unknown;
        let mut subtitle_script: Option<Vec<u8>> = None;

        // 第三遍：只为选中的流取参数并建解码上下文
        for (i, stream) in input.streams().enumerate() {
            let params = stream.parameters();
            match selected_kind(
                i,
                video_stream_index,
                audio_stream_index,
                subtitle_stream_index,
            ) {
                Some(TrackKind::Video) => {
                    video_codec_id = Some(VideoCodecId::from(params.id()));

                    // Extract video params from raw AVCodecParameters
                    unsafe {
                        let codecpar = (*stream.as_ptr()).codecpar;
                        width = (*codecpar).width as u32;
                        height = (*codecpar).height as u32;
                        // 像素宽高比：容器级 AVStream 优先，其次码流级 codecpar
                        //（与 av_guess_sample_aspect_ratio 的取值顺序一致）
                        let stream_sar = (*stream.as_ptr()).sample_aspect_ratio;
                        let codec_sar = (*codecpar).sample_aspect_ratio;
                        let sar = if stream_sar.num > 0 && stream_sar.den > 0 {
                            stream_sar
                        } else {
                            codec_sar
                        };
                        (display_width, display_height) =
                            display_size(width, height, sar.num, sar.den);
                        let av_fmt: ffmpeg_next::ffi::AVPixelFormat =
                            std::mem::transmute((*codecpar).format);
                        pixel_format = PixelFormat::from(ffmpeg_next::format::Pixel::from(av_fmt));
                    }

                    fps = stream.avg_frame_rate().numerator() as f64
                        / stream.avg_frame_rate().denominator().max(1) as f64;

                    // Create codec context for decoder initialization
                    let ctx = ffmpeg_next::codec::Context::from_parameters(params)
                        .map_err(|e| DylError::Demux(format!("video context: {e}")))?;
                    self.video_codec_context = Some(ctx);
                }
                Some(TrackKind::Audio) => {
                    audio_codec_id = Some(AudioCodecId::from(params.id()));

                    // Extract audio params from raw AVCodecParameters
                    unsafe {
                        let codecpar = (*stream.as_ptr()).codecpar;
                        sample_rate = (*codecpar).sample_rate as u32;
                        channels = (*codecpar).ch_layout.nb_channels as u32;
                        let av_fmt: ffmpeg_next::ffi::AVSampleFormat =
                            std::mem::transmute((*codecpar).format);
                        sample_format =
                            SampleFormat::from(ffmpeg_next::format::Sample::from(av_fmt));
                    }

                    let ctx = ffmpeg_next::codec::Context::from_parameters(params)
                        .map_err(|e| DylError::Demux(format!("audio context: {e}")))?;
                    self.audio_codec_context = Some(ctx);
                }
                Some(TrackKind::Subtitle) => {
                    subtitle_codec_id = Some(SubtitleCodecId::from(params.id()));

                    // ASS/SSA 轨的 CodecPrivate 是完整脚本头（样式表 + PlayResX/Y），
                    // 字幕引擎要靠它把内封事件按作者样式排版；ffmpeg-next 没有
                    // 安全接口，取法与上面的视频/音频参数一致
                    unsafe {
                        let codecpar = (*stream.as_ptr()).codecpar;
                        let size = (*codecpar).extradata_size;
                        let ptr = (*codecpar).extradata;
                        if !ptr.is_null() && size > 0 {
                            subtitle_script = Some(
                                std::slice::from_raw_parts(ptr as *const u8, size as usize)
                                    .to_vec(),
                            );
                        }
                    }

                    let ctx = ffmpeg_next::codec::Context::from_parameters(params)
                        .map_err(|e| DylError::Demux(format!("subtitle context: {e}")))?;
                    self.subtitle_codec_context = Some(ctx);

                    tracing::info!(
                        "demuxer: detected subtitle stream {} ({:?}, script={}B)",
                        i,
                        subtitle_codec_id,
                        subtitle_script.as_ref().map_or(0, |s| s.len())
                    );
                }
                _ => {}
            }
        }

        let selected = [
            video_stream_index,
            audio_stream_index,
            subtitle_stream_index,
        ];
        let tracks: Vec<StreamTrack> = probes
            .iter()
            .map(|p| StreamTrack {
                index: p.index,
                kind: p.kind,
                codec: track_codec_label(p.kind, p.codec_id).to_string(),
                language: p.language.clone(),
                title: p.title.clone(),
                default: p.default,
                selected: selected.contains(&Some(p.index)),
            })
            .collect();

        let info = MediaInfo {
            duration,
            video_stream_index,
            audio_stream_index,
            subtitle_stream_index,
            video_codec_id,
            audio_codec_id,
            subtitle_codec_id,
            width,
            height,
            display_width,
            display_height,
            pixel_format,
            fps,
            sample_rate,
            channels,
            sample_format,
            tracks,
            subtitle_script,
        };

        tracing::info!(
            "opened media: duration={:.2}s, video={}x{} (display {}x{}), audio={}Hz {}ch",
            info.duration,
            if video_stream_index.is_some() {
                width
            } else {
                0
            },
            if video_stream_index.is_some() {
                height
            } else {
                0
            },
            if video_stream_index.is_some() {
                display_width
            } else {
                0
            },
            if video_stream_index.is_some() {
                display_height
            } else {
                0
            },
            if audio_stream_index.is_some() {
                sample_rate
            } else {
                0
            },
            if audio_stream_index.is_some() {
                channels
            } else {
                0
            },
        );

        self.info = Some(info);

        // 缓存每个流的 time_base
        self.time_bases = input.streams().map(|s| s.time_base()).collect();

        self.input = Some(input);
        self.interrupt = Some(interrupt);
        Ok(())
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| DylError::InvalidState("demuxer not opened".into()))?;

        let mut av_packet = ffmpeg_next::Packet::empty();
        match av_packet.read(input) {
            Ok(()) => {
                let stream_idx = av_packet.stream();
                // 流的 time_base 未知（0/x）时保留无效基，让换算返回 None 而非伪造秒数
                let time_base = self
                    .time_bases
                    .get(stream_idx)
                    .copied()
                    .unwrap_or_else(|| ffmpeg_next::Rational::new(0, 1));

                // 保留原始 tick 与 NOPTS 语义：无时间戳的包不得伪装成 ts=0
                let pkt = Packet {
                    data: av_packet.data().unwrap_or_default().to_vec(),
                    stream_index: stream_idx,
                    pts: av_packet.pts(),
                    dts: av_packet.dts(),
                    // duration <= 0 是 FFmpeg 的"未知"约定（字幕轨靠它给出显示时长）
                    duration: Some(av_packet.duration()).filter(|d| *d > 0),
                    time_base,
                    is_key: av_packet.is_key(),
                };
                Ok(Some(pkt))
            }
            Err(ffmpeg_next::Error::Eof) => Ok(None),
            Err(e) => Err(DylError::Demux(format!("read_packet: {e}"))),
        }
    }

    fn seek(&mut self, target: f64) -> Result<()> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| DylError::InvalidState("demuxer not opened".into()))?;

        let ts = (target * 1_000_000.0) as i64;
        input
            .seek(ts, i64::MIN..i64::MAX)
            .map_err(|e| DylError::Demux(format!("seek: {e}")))?;
        Ok(())
    }

    fn info(&self) -> Option<&MediaInfo> {
        self.info.as_ref()
    }

    fn take_video_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
        self.video_codec_context.take()
    }

    fn take_audio_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
        self.audio_codec_context.take()
    }

    fn take_subtitle_codec_context(&mut self) -> Option<ffmpeg_next::codec::Context> {
        self.subtitle_codec_context.take()
    }

    fn close(&mut self) {
        tracing::info!("FfmpegDemuxer::close");
        // 先关 context，再释放中断闭包：回调的裸指针只在 context 存活期间有效
        self.input = None;
        self.interrupt = None;
        self.info = None;
        self.video_codec_context = None;
        self.audio_codec_context = None;
        self.subtitle_codec_context = None;
        self.time_bases.clear();
    }
}

/// 中断回调闭包的所有权。
///
/// ffmpeg-next 的 `interrupt::new` 会 `Box::into_raw` 之后永不回收 —— 对一次性
/// 进程无所谓，但切轨/重开每次都会打开一个新 demuxer，闭包必须跟着 context
/// 一起释放。
struct InterruptHandle {
    closure: *mut Box<dyn FnMut() -> bool + Send>,
}

// Safety: 闭包只捕获 `Arc<AtomicBool>`，从 demux 线程调用；context 归 demuxer 独占
unsafe impl Send for InterruptHandle {}

impl InterruptHandle {
    fn new(shutdown: Arc<AtomicBool>) -> Self {
        let closure: Box<dyn FnMut() -> bool + Send> =
            Box::new(move || shutdown.load(Ordering::Acquire));
        Self {
            closure: Box::into_raw(Box::new(closure)),
        }
    }

    fn callback(&self) -> ffmpeg_next::ffi::AVIOInterruptCB {
        ffmpeg_next::ffi::AVIOInterruptCB {
            callback: Some(interrupt_trampoline),
            opaque: self.closure as *mut c_void,
        }
    }
}

impl Drop for InterruptHandle {
    fn drop(&mut self) {
        // Safety: 只在本结构体里创建并在 drop 时回收一次，且 context 已先关闭
        unsafe { drop(Box::from_raw(self.closure)) };
    }
}

extern "C" fn interrupt_trampoline(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    // 回调跨 FFI 边界：panic 不得展开进 C；真出了异常按"要求中断"处理
    let closure = unsafe { &mut *(opaque as *mut Box<dyn FnMut() -> bool + Send>) };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(closure)) {
        Ok(stop) => stop as c_int,
        Err(_) => 1,
    }
}

/// 网络来源的 AVOptions。`rw_timeout` 同时约束连接建立与每次读；
/// `reconnect` 让中途断开的连接带 Range 头续上，而不是把整场播放判死。
///
/// 刻意不开 `reconnect_on_network_error`：它连"首次连接失败"也重试，实测一个
/// 拒绝连接的 URL 从 5.5s 变成 26s（4 次尝试 0/1/3s 退避）才报错，加载阶段
/// 等待过久。中途硬断由 `rw_timeout` 兜底成 demux 错误。
fn open_options(kind: SourceKind) -> Vec<(&'static str, String)> {
    if !kind.is_network() {
        return Vec::new();
    }
    vec![
        ("rw_timeout", NETWORK_RW_TIMEOUT_US.to_string()),
        ("reconnect", "1".to_string()),
        (
            "reconnect_delay_max",
            NETWORK_RECONNECT_DELAY_MAX.to_string(),
        ),
    ]
}

/// 打开来源并安装中断回调。
///
/// ffmpeg-next 只提供 `input_with_dictionary`（无中断）或 `input_with_interrupt`
/// （无选项），网络来源两样都需要，所以这里直接走一遍
/// `avformat_open_input` + `avformat_find_stream_info`。
fn open_input(
    url: &str,
    kind: SourceKind,
    shutdown: Arc<AtomicBool>,
) -> Result<(ffmpeg_next::format::context::Input, InterruptHandle)> {
    use ffmpeg_next::ffi;

    let url_c = CString::new(url)
        .map_err(|_| DylError::InvalidArgument("source URL contains NUL".into()))?;

    let mut dict: *mut ffi::AVDictionary = std::ptr::null_mut();
    for (key, value) in open_options(kind) {
        let key = CString::new(key).expect("option key has no NUL");
        let value = CString::new(value).expect("option value has no NUL");
        unsafe { ffi::av_dict_set(&mut dict, key.as_ptr(), value.as_ptr(), 0) };
    }

    let interrupt = InterruptHandle::new(shutdown);
    let mut ctx = unsafe { ffi::avformat_alloc_context() };
    if ctx.is_null() {
        unsafe { ffi::av_dict_free(&mut dict) };
        return Err(DylError::Internal("avformat_alloc_context failed".into()));
    }
    unsafe { (*ctx).interrupt_callback = interrupt.callback() };

    let opened =
        unsafe { ffi::avformat_open_input(&mut ctx, url_c.as_ptr(), std::ptr::null(), &mut dict) };
    // 未识别的选项会留在 dict 里；这里直接释放，FFmpeg 不会因此失败
    unsafe { ffi::av_dict_free(&mut dict) };
    if opened < 0 {
        // 失败时 avformat_open_input 已释放传入的 context（含 interrupt_callback）
        return Err(open_error(url, opened));
    }

    let probed = unsafe { ffi::avformat_find_stream_info(ctx, std::ptr::null_mut()) };
    if probed < 0 {
        unsafe { ffi::avformat_close_input(&mut ctx) };
        return Err(open_error(url, probed));
    }

    let input = unsafe { ffmpeg_next::format::context::Input::wrap(ctx) };
    Ok((input, interrupt))
}

fn open_error(url: &str, code: c_int) -> DylError {
    let error = ffmpeg_next::Error::from(code);
    // 中断回调触发的"错误"其实是 shutdown：给日志一个能看懂的原因
    if matches!(error, ffmpeg_next::Error::Exit) {
        return DylError::Demux(format!("interrupted while opening {url}"));
    }
    DylError::Demux(format!("failed to open {url}: {error}"))
}

/// open() 第一遍收集的选轨依据
#[derive(Debug, Clone)]
struct StreamProbe {
    index: usize,
    kind: TrackKind,
    codec_id: ffmpeg_next::codec::Id,
    default: bool,
    language: Option<String>,
    title: Option<String>,
}

/// 同类轨道里选一条：容器标了 default 的优先，否则取顺序上的第一条
fn pick(
    probes: &[StreamProbe],
    kind: TrackKind,
    accept: impl Fn(&StreamProbe) -> bool,
) -> Option<usize> {
    let candidates: Vec<&StreamProbe> = probes
        .iter()
        .filter(|p| p.kind == kind && accept(p))
        .collect();
    candidates
        .iter()
        .find(|p| p.default)
        .or_else(|| candidates.first())
        .map(|p| p.index)
}

/// 显式指定的轨道必须真实存在、类型对得上、内核解得动；`None` 回退自动选择。
///
/// 字幕的文本门（[`SubtitleCodecId::is_text`]）对显式选择和自动选择是同一条规则：
/// 切到 PGS/DVB 位图轨会得到 `not_supported`，而不是一个静默不出字的管线。
fn resolve_track(
    probes: &[StreamProbe],
    kind: TrackKind,
    requested: Option<usize>,
) -> Result<Option<usize>> {
    let accepts = |p: &StreamProbe| match kind {
        TrackKind::Subtitle => SubtitleCodecId::from(p.codec_id).is_text(),
        _ => true,
    };
    let Some(index) = requested else {
        return Ok(pick(probes, kind, accepts));
    };
    let Some(probe) = probes.iter().find(|p| p.index == index) else {
        return Err(DylError::InvalidArgument(format!(
            "stream {index} is not a selectable track in this file"
        )));
    };
    if probe.kind != kind {
        return Err(DylError::InvalidArgument(format!(
            "stream {index} is {} but {} selection was requested",
            kind_label(probe.kind),
            kind_label(kind)
        )));
    }
    if !accepts(probe) {
        return Err(DylError::NotSupported(format!(
            "stream {index} subtitle codec {} is not a text format",
            track_codec_label(probe.kind, probe.codec_id)
        )));
    }
    Ok(Some(index))
}

fn kind_label(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
        TrackKind::Subtitle => "subtitle",
    }
}

/// 该流是否为本次选中的轨道，是哪一类
fn selected_kind(
    index: usize,
    video: Option<usize>,
    audio: Option<usize>,
    subtitle: Option<usize>,
) -> Option<TrackKind> {
    if Some(index) == video {
        Some(TrackKind::Video)
    } else if Some(index) == audio {
        Some(TrackKind::Audio)
    } else if Some(index) == subtitle {
        Some(TrackKind::Subtitle)
    } else {
        None
    }
}

/// 轨道标签，与快照里 `media.video.codec` / `media.audio.codec` 用同一套映射
fn track_codec_label(kind: TrackKind, id: ffmpeg_next::codec::Id) -> &'static str {
    match kind {
        TrackKind::Video => VideoCodecId::from(id).label(),
        TrackKind::Audio => AudioCodecId::from(id).label(),
        TrackKind::Subtitle => SubtitleCodecId::from(id).label(),
    }
}

/// 读流元数据里的一个键；空值与 `und`（未标注语言）按缺失处理
fn dict_text(stream: &ffmpeg_next::format::stream::Stream<'_>, key: &str) -> Option<String> {
    let metadata = stream.metadata();
    let value = metadata.get(key)?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("und") {
        return None;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_demuxer_new() {
        let d = FfmpegDemuxer::new();
        assert!(d.info.is_none());
        assert!(d.input.is_none());
    }

    #[test]
    fn test_demuxer_default() {
        let d = FfmpegDemuxer::default();
        assert!(d.info.is_none());
    }

    #[test]
    fn test_demuxer_open_nonexistent() {
        let mut d = FfmpegDemuxer::new();
        let result = d.open(
            "nonexistent_file_12345.mp4",
            &TrackSelection::default(),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_demuxer_read_packet_not_ready() {
        let mut d = FfmpegDemuxer::new();
        let result = d.read_packet();
        assert!(result.is_err());
    }

    /// 中断回调必须真的接到 pipeline 的 shutdown 位：这是网络读卡住时
    /// stop/destroy 能返回的唯一通道
    #[test]
    fn interrupt_callback_reflects_the_shutdown_flag() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = InterruptHandle::new(shutdown.clone());
        let cb = handle.callback();
        let trip = || unsafe { (cb.callback.expect("callback installed"))(cb.opaque) };
        assert_eq!(trip(), 0);
        shutdown.store(true, Ordering::Release);
        assert_eq!(trip(), 1);
    }

    /// 本地文件不吃网络 AVOptions；http(s) 必须带超时与重连
    #[test]
    fn network_options_are_only_set_for_urls() {
        assert!(open_options(SourceKind::LocalPath).is_empty());
        for kind in [SourceKind::Http, SourceKind::Https] {
            let options = open_options(kind);
            assert!(
                options
                    .iter()
                    .any(|(k, v)| *k == "rw_timeout" && v == "15000000"),
                "{kind:?} 缺少 rw_timeout"
            );
            assert!(
                options.iter().any(|(k, _)| *k == "reconnect"),
                "{kind:?} 缺少 reconnect"
            );
        }
    }

    /// 走一遍自建的 avformat_open_input 路径：连接被拒绝要变成 demux 错误，
    /// 而不是 panic 或 file_not_found
    #[test]
    fn refused_http_port_reports_a_demux_error() {
        if !crate::capabilities::protocol_available("http") {
            return;
        }
        let mut d = FfmpegDemuxer::new();
        let err = d
            .open(
                "http://127.0.0.1:1/never.mp4",
                &TrackSelection::default(),
                Arc::new(AtomicBool::new(false)),
            )
            .expect_err("nothing listens on port 1");
        assert!(matches!(err, DylError::Demux(_)), "{err}");
        assert!(err.to_string().contains("failed to open"), "{err}");
    }

    /// 已经置位的 shutdown 让打开立刻中断，日志里是可读的原因
    #[test]
    fn already_stopped_open_is_reported_as_interrupted() {
        if !crate::capabilities::protocol_available("http") {
            return;
        }
        let mut d = FfmpegDemuxer::new();
        let started = std::time::Instant::now();
        let err = d
            .open(
                "http://127.0.0.1:1/never.mp4",
                &TrackSelection::default(),
                Arc::new(AtomicBool::new(true)),
            )
            .expect_err("shutdown flag is already set");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "中断没有生效，耗时 {:?}",
            started.elapsed()
        );
        assert!(matches!(err, DylError::Demux(_)), "{err}");
    }

    #[test]
    fn test_demuxer_seek_not_ready() {
        let mut d = FfmpegDemuxer::new();
        let result = d.seek(1.0);
        assert!(result.is_err());
    }

    fn probe(index: usize, kind: TrackKind, default: bool) -> StreamProbe {
        StreamProbe {
            index,
            kind,
            codec_id: ffmpeg_next::codec::Id::AAC,
            default,
            language: None,
            title: None,
        }
    }

    fn probe_codec(index: usize, kind: TrackKind, codec_id: ffmpeg_next::codec::Id) -> StreamProbe {
        StreamProbe {
            index,
            kind,
            codec_id,
            default: false,
            language: None,
            title: None,
        }
    }

    /// 容器标了 default 的轨必须赢过容器顺序，否则多音轨文件会随机命中
    #[test]
    fn pick_prefers_default_track_over_container_order() {
        let probes = vec![
            probe(0, TrackKind::Audio, false),
            probe(1, TrackKind::Audio, true),
            probe(2, TrackKind::Audio, false),
        ];
        assert_eq!(pick(&probes, TrackKind::Audio, |_| true), Some(1));
    }

    #[test]
    fn pick_falls_back_to_first_track_without_default_flag() {
        let probes = vec![
            probe(0, TrackKind::Subtitle, false),
            probe(1, TrackKind::Subtitle, false),
        ];
        assert_eq!(pick(&probes, TrackKind::Subtitle, |_| true), Some(0));
        assert_eq!(pick(&[], TrackKind::Subtitle, |_| true), None);
    }

    /// accept 过滤掉的轨道即使带 default 也不能选（位图字幕选了也出不了字）
    #[test]
    fn pick_skips_rejected_tracks_even_when_default() {
        let probes = vec![
            probe(3, TrackKind::Subtitle, true),
            probe(4, TrackKind::Subtitle, false),
        ];
        assert_eq!(
            pick(&probes, TrackKind::Subtitle, |p| p.index != 3),
            Some(4)
        );
        assert_eq!(pick(&probes, TrackKind::Subtitle, |_| false), None);
    }

    /// 显式选轨就是切轨的入口：必须用调用方给的索引，不能重跑 default 优先
    #[test]
    fn resolve_track_honours_explicit_index_over_default_flag() {
        let probes = vec![
            probe(0, TrackKind::Audio, true),
            probe(1, TrackKind::Audio, false),
        ];
        assert_eq!(
            resolve_track(&probes, TrackKind::Audio, Some(1)).unwrap(),
            Some(1)
        );
        // 只有未指定时才回退自动选择
        assert_eq!(
            resolve_track(&probes, TrackKind::Audio, None).unwrap(),
            Some(probes[0].index)
        );
        assert_eq!(
            resolve_track(&probes, TrackKind::Video, None).unwrap(),
            None
        );
    }

    /// 指到别的类型的流是调用方的错，内核不能擅自改选一条
    #[test]
    fn resolve_track_rejects_wrong_kind_and_missing_stream() {
        let probes = vec![probe(0, TrackKind::Video, true)];
        let err = resolve_track(&probes, TrackKind::Audio, Some(0)).unwrap_err();
        assert!(matches!(err, DylError::InvalidArgument(_)), "{err}");
        assert!(err.to_string().contains("video"), "{err}");
        assert!(matches!(
            resolve_track(&probes, TrackKind::Audio, Some(7)).unwrap_err(),
            DylError::InvalidArgument(_)
        ));
    }

    /// 位图字幕选了解码线程也出不了字：必须报 not_supported，不能建静默管线
    #[test]
    fn resolve_track_rejects_bitmap_subtitle_as_not_supported() {
        let probes = vec![probe(0, TrackKind::Subtitle, true)];
        let err = resolve_track(&probes, TrackKind::Subtitle, Some(0)).unwrap_err();
        assert!(matches!(err, DylError::NotSupported(_)), "{err}");
        assert!(err.to_string().contains("text format"), "{err}");
    }

    /// 文本字幕（ASS/SSA/SRT）是唯一切得动的字幕轨
    #[test]
    fn resolve_track_accepts_text_subtitle_explicitly() {
        let probes = vec![
            probe_codec(2, TrackKind::Subtitle, ffmpeg_next::codec::Id::ASS),
            probe_codec(3, TrackKind::Subtitle, ffmpeg_next::codec::Id::SUBRIP),
        ];
        assert_eq!(
            resolve_track(&probes, TrackKind::Subtitle, Some(3)).unwrap(),
            Some(3)
        );
    }

    #[test]
    fn selected_kind_marks_only_chosen_streams() {
        assert_eq!(
            selected_kind(1, Some(1), Some(2), Some(3)),
            Some(TrackKind::Video)
        );
        assert_eq!(
            selected_kind(2, Some(1), Some(2), Some(3)),
            Some(TrackKind::Audio)
        );
        assert_eq!(
            selected_kind(3, Some(1), Some(2), Some(3)),
            Some(TrackKind::Subtitle)
        );
        assert_eq!(selected_kind(9, Some(1), Some(2), Some(3)), None);
        // 未选中的字幕流不能被误认成选中
        assert_eq!(selected_kind(3, Some(1), Some(2), None), None);
    }

    #[test]
    fn track_labels_match_snapshot_codec_strings() {
        assert_eq!(
            track_codec_label(
                TrackKind::Video,
                ffmpeg_next::codec::Id::from(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_HEVC)
            ),
            "h265"
        );
        assert_eq!(
            track_codec_label(TrackKind::Audio, ffmpeg_next::codec::Id::AAC),
            "aac"
        );
        assert_eq!(
            track_codec_label(TrackKind::Subtitle, ffmpeg_next::codec::Id::ASS),
            "ass"
        );
    }
}
