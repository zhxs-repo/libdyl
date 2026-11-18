use std::time::Instant;

use crate::hwaccel::{self, HwPlan, Policy};
use crate::types::*;
use dyl_core::{DylError, Result};
use ffmpeg_next::codec::packet::Ref;

/// VideoDecoder trait — 视频解码器抽象
pub trait VideoDecoder: Send {
    /// 提交数据包
    fn submit_packet(&mut self, packet: &Packet) -> Result<()>;

    /// 接收解码帧
    fn receive_frame(&mut self) -> Result<Option<VideoFrame>>;

    /// 结束流：送入空包让解码器进入 drain 模式，之后 `receive_frame` 才能吐出
    /// 内部滞留的尾部帧（B 帧重排缓冲）
    fn send_eof(&mut self) -> Result<()>;

    /// 刷新解码器（seek 后调用）
    fn flush(&mut self);

    /// seek pre-roll 期间跳过非参考帧（B帧）的解码，加速 seek 响应。
    /// `true` = 设置 AVDISCARD_NONREF，`false` = 恢复 AVDISCARD_DEFAULT。
    fn set_skip_nonref(&mut self, _skip: bool) {}

    /// 本次解码当前实际使用的硬件设备名；运行中回退软解后变 `None`
    fn hardware_device(&self) -> Option<String> {
        None
    }

    /// 取走一条运行时通知（如硬解吞吐不足已回退软解），取后清空
    fn take_notice(&mut self) -> Option<DecoderNotice> {
        None
    }
}

/// 解码器运行时通知 — 由管线转成契约 `Event::Warning`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderNotice {
    pub code: &'static str,
    pub message: String,
}

/// AudioDecoder trait — 音频解码器抽象
pub trait AudioDecoder: Send {
    /// 提交数据包
    fn submit_packet(&mut self, packet: &Packet) -> Result<()>;

    /// 接收解码帧
    fn receive_frame(&mut self) -> Result<Option<AudioFrame>>;

    /// 结束流：送入空包让解码器进入 drain 模式。AAC 等解码器会压着最后
    /// 1~2 帧（约 1024 采样），不 drain 就取不到
    fn send_eof(&mut self) -> Result<()>;

    /// 刷新解码器
    fn flush(&mut self);
}

/// swscale 上下文的线程所有权包装
///
/// ffmpeg_next 给 `software::resampling::Context` 标了 `Send`，却没给
/// `software::scaling::Context` 标，而 `VideoDecoder: Send` 需要它。
/// `SwsContext` 只经 `&mut self` 路径被所属解码器独占使用，随解码器一起
/// 移动到视频解码线程后不存在并发访问，因此转移所有权是安全的。
struct SwsScaler(ffmpeg_next::software::scaling::Context);

unsafe impl Send for SwsScaler {}

/// 解码帧 → 渲染通路输入格式的转换方式
///
/// 只做位深下调的两种格式走位移，不进 swscale：4K 的 10→8 单线程 swscale
/// 实测 ~16ms/帧，正值 25fps 内容只有 40ms 预算；按采样右移是纯内存拷贝，
/// 快一个数量级。4:2:2/4:4:4 等真正的重采样仍交给 swscale。
enum Conversion {
    Sws(SwsScaler),
    /// YUV420P10LE → YUV420P（每采样右移 2）
    Depth10Planar,
    /// P010LE → NV12（每采样右移 8；UV 平面本就交织）
    P010ToNv12,
}

/// 硬解吞吐看护 — 判断依据是"解一帧要多久"，帧率从 PTS 间隔学习
///
/// 解码持续跟不上时 AVSync 只能丢帧或强显，音频主时钟不会被视频改写，漂移
/// 越积越大（旧 swscale 通路在 4K10 上忙时 45ms > 40ms 预算，40s 漂到 9.7s）。
/// 本机加完位移降位深后硬解忙时 29.7ms、软解 7.0ms，都回到预算内；但更慢的
/// 机器/更高码率仍可能超预算，与其让画面持续丢帧，不如运行中换回软解。
///
/// 只统计解码器调用内部的忙时（送包/取帧/回传/转换），下游通道阻塞不算：
/// 那是渲染跟不上，不是解码器跟不上。
struct FallbackWatch {
    /// 重建软解所需的流参数；`None` = 这一路不参与自动回退（软解 /
    /// 显式指定的设备 —— 用户点名要的设备不偷偷换掉）
    parameters: Option<ffmpeg_next::codec::Parameters>,
    /// 预热帧数：首帧含帧池/设备初始化，不能算进窗口
    seen: u32,
    busy: f64,
    content: f64,
    interval: Option<f64>,
    last_pts: Option<f64>,
    done: bool,
}

const FALLBACK_WARMUP_FRAMES: u32 = 24;
const FALLBACK_WINDOW_SECS: f64 = 2.0;
/// 窗口内解码忙时必须低于内容时长的这一比例才算跟得上
///
/// 1.05 而不是更宽松：旧 swscale 通路 4K 忙时 45ms 对 40ms 预算只超 12%，
/// 1.15 的阈值会把"每 10 秒丢约 1 秒画面"判成健康。持续超过预算 5% 就已
/// 经意味着 AVSync 必须靠丢帧维持，画面必然顿。
const FALLBACK_SLOWDOWN: f64 = 1.05;

impl FallbackWatch {
    fn new(parameters: Option<ffmpeg_next::codec::Parameters>) -> Self {
        Self {
            parameters,
            seen: 0,
            busy: 0.0,
            content: 0.0,
            interval: None,
            last_pts: None,
            done: false,
        }
    }

    /// 新窗口从零开始（seek 后卡顿不该累加到旧账上）
    fn reset_window(&mut self) {
        self.busy = 0.0;
        self.content = 0.0;
    }

    /// 记录一帧产出；返回 `true` 表示持续跟不上，应回退软解
    fn note_frame(&mut self, pts: Option<f64>) -> bool {
        if self.parameters.is_none() || self.done {
            return false;
        }
        if let Some(pts) = pts {
            if let Some(last) = self.last_pts {
                let delta = pts - last;
                // 跨 seek 的跳变与异常间隔不参与帧率估计
                if delta > 0.0 && delta < 0.5 {
                    self.interval = Some(match self.interval {
                        Some(ema) => ema * 0.9 + delta * 0.1,
                        None => delta,
                    });
                }
            }
            self.last_pts = Some(pts);
        }
        self.seen += 1;
        if self.seen <= FALLBACK_WARMUP_FRAMES {
            self.reset_window();
            return false;
        }
        let Some(interval) = self.interval else {
            return false;
        };
        self.content += interval;
        if self.content < FALLBACK_WINDOW_SECS {
            return false;
        }
        let slow = self.busy > self.content * FALLBACK_SLOWDOWN;
        tracing::debug!(
            "fallback watch window: busy={:.3}s content={:.3}s interval={:.4}s slow={slow}",
            self.busy,
            self.content,
            interval
        );
        self.reset_window();
        self.done = slow;
        slow
    }
}

/// FFmpeg 视频解码器
pub struct FfmpegVideoDecoder {
    decoder: Option<ffmpeg_next::decoder::Video>,
    pixel_format: PixelFormat,
    /// 解码输出格式不被渲染通路支持时（10-bit HEVC、4:2:2/4:4:4 等）在此挂载，
    /// 统一转成渲染通路认得的格式；`None` 表示解码帧原样输出
    conversion: Option<Conversion>,
    /// Packet/Frame 时间基 — 视频解码帧 PTS 直接沿用输入 Packet 的时间基
    pkt_timebase: ffmpeg_next::Rational,
    /// 硬解方案；`None` = 这一路是软解
    hw: Option<HwPlan>,
    /// GPU 帧回传的落点，缓冲跨帧复用（见 `hwaccel::prepare_cpu_frame`）
    cpu_frame: ffmpeg_next::frame::Video,
    /// 当前输出通路是按哪一路"源格式 + 尺寸"配的；帧上一变就重建
    configured: Option<(ffmpeg_next::format::Pixel, u32, u32)>,
    fallback: FallbackWatch,
    /// 已决定回退软解：硬解继续出帧直到下一个关键帧，在那里换解码器从 IDR 重起
    await_key: bool,
    /// 重建软解失败的告警只发一次（每个关键帧都会重试，不能逐次刷屏）
    fallback_fail_notified: bool,
    notice: Option<DecoderNotice>,
}

impl FfmpegVideoDecoder {
    pub fn new() -> Self {
        Self {
            decoder: None,
            pixel_format: PixelFormat::Unknown,
            conversion: None,
            pkt_timebase: ffmpeg_next::Rational::new(0, 1),
            hw: None,
            cpu_frame: ffmpeg_next::frame::Video::empty(),
            configured: None,
            fallback: FallbackWatch::new(None),
            await_key: false,
            fallback_fail_notified: false,
            notice: None,
        }
    }

    /// 从 CodecContext 创建已配置的解码器（策略取自 `DYL_HW_DECODE`，缺省纯软解）
    pub fn from_stream_context(ctx: ffmpeg_next::codec::Context) -> Result<Self> {
        Self::with_policy(ctx, Policy::from_env())
    }

    /// 按指定策略创建
    ///
    /// [`Policy::SoftwareOnly`] 与硬解接入之前的行为完全一致；其余策略只在
    /// `avcodec_open2()` 之前多装两样东西——`hw_device_ctx` 与 `get_format`，
    /// 而打开解码器正是那一次 `avcodec_open2`。
    ///
    /// AV1 的软解是唯一需要显式选解码器的编码：FFmpeg 内置 `av1` 只做硬件
    /// 解码，软解必须挑 `libdav1d` / `libaom-av1`（见 [`open_software_av1`]）。
    /// [`Policy::Auto`] 下硬解打开失败时回退软解：`avcodec_get_hw_config` 只说明
    /// 解码器**声明**了该设备，GPU 是否真支持这个编码要到 `avcodec_open2` 才知道。
    pub fn with_policy(mut ctx: ffmpeg_next::codec::Context, policy: Policy) -> Result<Self> {
        let is_av1 = ctx.id() == ffmpeg_next::codec::Id::AV1;
        // FFmpeg 的 `threads` 默认是 1；CLI 的"自动"其实是它自己另设的。4K10
        // 单线程解码只有 ~20fps，帧线程按 CPU 数开（0 = auto）后软解快 3× 以上，
        // 硬解也不再逐包同步等 GPU
        ctx.set_threading(ffmpeg_next::codec::threading::Config {
            kind: ffmpeg_next::codec::threading::Type::Frame,
            count: 0,
        });
        let mut plan = hwaccel::select(ffmpeg_next::ffi::AVCodecID::from(ctx.id()), policy);
        if let Some(plan) = plan.as_ref() {
            // SAFETY: 拿到的是本函数独占的 `Context` 的原始指针，且在打开解码器
            // （那一次 `avcodec_open2`）之前用完
            unsafe { hwaccel::install(ctx.as_mut_ptr(), plan) }?;
        }
        // 硬解打开失败 / 运行中跟不上时都靠这份参数重建软解上下文；
        // 显式点名的设备不参与自动回退（用户点名要的设备不偷偷换掉）
        let fallback_parameters = if plan.is_some() && matches!(policy, Policy::Auto) {
            Some(codec_parameters(&ctx)?)
        } else {
            None
        };
        let video = if is_av1 && plan.is_none() {
            open_software_av1(ctx)?
        } else {
            match ctx.decoder().video() {
                Ok(video) => video,
                Err(e) => match fallback_parameters.as_ref() {
                    Some(parameters) => {
                        tracing::warn!("硬解打开失败（{e}），回退软件解码");
                        plan = None;
                        let mut fresh =
                            ffmpeg_next::codec::Context::from_parameters(parameters.clone())
                                .map_err(|e| {
                                    DylError::Decode(format!("rebuild software context: {e}"))
                                })?;
                        fresh.set_threading(ffmpeg_next::codec::threading::Config {
                            kind: ffmpeg_next::codec::threading::Type::Frame,
                            count: 0,
                        });
                        if is_av1 {
                            open_software_av1(fresh)?
                        } else {
                            fresh.decoder().video().map_err(|e| {
                                DylError::Decode(format!("open software decoder: {e}"))
                            })?
                        }
                    }
                    None => {
                        return Err(DylError::Decode(format!("video decoder open: {e}")));
                    }
                },
            }
        };
        let open_source = (video.format(), video.width(), video.height());
        let (pixel_format, conversion) =
            configure_output(open_source.0, open_source.1, open_source.2)?;
        // avctx.pkt_timebase 打开时通常为 0/1（未设置），实际时间基由首个 Packet 携带
        let pkt_timebase = video.packet_time_base();
        let threading = video.threading();
        tracing::info!(
            "FfmpegVideoDecoder from_stream_context: {}x{} {:?} (解码输出 {:?}, 转换={}, 硬解={}, 线程={:?}x{}), pkt_timebase={}",
            video.width(),
            video.height(),
            pixel_format,
            video.format(),
            conversion_name(conversion.as_ref()),
            plan.as_ref().map(HwPlan::describe).unwrap_or_else(|| "关".into()),
            threading.kind,
            threading.count,
            pkt_timebase
        );
        Ok(Self {
            decoder: Some(video),
            pixel_format,
            conversion,
            pkt_timebase,
            cpu_frame: ffmpeg_next::frame::Video::empty(),
            // 装了硬解时打开所报告的格式并不作数（`get_format` 要到第一次解帧
            // 才被调，候选不匹配还会静默落回软解），留 `None` 由首帧把通路配对；
            // 纯软解保持原样，打开时就定下，行为与接硬解之前逐字节一致
            configured: plan.is_none().then_some(open_source),
            hw: plan,
            fallback: FallbackWatch::new(fallback_parameters),
            await_key: false,
            fallback_fail_notified: false,
            notice: None,
        })
    }
}

impl Default for FfmpegVideoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// 在打开解码器之前把流参数复制出来，供硬解失败后重建上下文
fn codec_parameters(ctx: &ffmpeg_next::codec::Context) -> Result<ffmpeg_next::codec::Parameters> {
    let mut parameters = ffmpeg_next::codec::Parameters::new();
    // SAFETY: `parameters` 是本函数独占的已分配 AVCodecParameters，`ctx` 仍活着；
    // 只做一次复制，不转移任何所有权
    let ret = unsafe {
        ffmpeg_next::ffi::avcodec_parameters_from_context(parameters.as_mut_ptr(), ctx.as_ptr())
    };
    if ret < 0 {
        return Err(DylError::Internal(format!(
            "avcodec_parameters_from_context: {}",
            ffmpeg_next::Error::from(ret)
        )));
    }
    Ok(parameters)
}

/// 用构建里编入的 AV1 软件解码器打开上下文
///
/// 内置 `av1` 解码器（`av1dec.c`）只在 `avctx->hwaccel` 非空时工作，而
/// `avcodec_find_decoder(AV_CODEC_ID_AV1)` 选到哪一个解码器取决于构建的注册顺序
/// （没有 dav1d 时只有内置那个），所以 AV1 软解必须按名字挑
/// `libdav1d` / `libaom-av1`，不能走 `ctx.decoder().video()`。
fn open_software_av1(ctx: ffmpeg_next::codec::Context) -> Result<ffmpeg_next::decoder::Video> {
    let name = crate::capabilities::software_av1_decoder().ok_or_else(|| {
        DylError::NotSupported(
            "this FFmpeg build has no AV1 software decoder (libdav1d or libaom-av1); \
             the built-in `av1` decoder is hardware-only"
                .into(),
        )
    })?;
    let codec = ffmpeg_next::codec::decoder::find_by_name(name)
        .ok_or_else(|| DylError::Internal(format!("AV1 software decoder `{name}` vanished")))?;
    tracing::info!("AV1 software decoder: {name}");
    ctx.decoder()
        .open_as(codec)
        .and_then(|opened| opened.video())
        .map_err(|e| DylError::Decode(format!("open AV1 software decoder `{name}`: {e}")))
}

impl FfmpegVideoDecoder {
    /// 本次解码实际启用的硬件设备名；`None` 表示软解。
    ///
    /// 只在 `HW_DEVICE_CTX` 可显式启用的平台上等于"帧真的来自 GPU"（Windows 实测
    /// 路径）；`internal_only` 平台即便装上了方案，FFmpeg 仍可能自行落回软解，
    /// 所以产品层只在已实测平台暴露硬解开关（见 `HW_DECODE_VERIFIED`）。
    pub fn hardware_device(&self) -> Option<String> {
        self.hw.as_ref().map(HwPlan::device_name)
    }

    /// 运行时回退软解：用打开前保存的流参数重建上下文
    ///
    /// 与打开时的回退同一条路：参数里带着编码格式与 extradata，重建后从下一个
    /// 关键帧继续解码（解码器对象在整个 GOP 中途换不了，换完必须由 IDR 重新
    /// 起头）。重建失败不致命 —— 保留原硬解，下一个关键帧再试（看护已触发过，
    /// 不会重复累加）。
    fn rebuild_software(&mut self) -> Result<()> {
        let parameters = self.fallback.parameters.clone().ok_or_else(|| {
            DylError::Internal("runtime fallback without stream parameters".into())
        })?;
        let device = self
            .hw
            .as_ref()
            .map(HwPlan::describe)
            .unwrap_or_else(|| "hardware".into());
        let is_av1 = parameters.id() == ffmpeg_next::codec::Id::AV1;
        let mut fresh = ffmpeg_next::codec::Context::from_parameters(parameters)
            .map_err(|e| DylError::Decode(format!("rebuild software context: {e}")))?;
        fresh.set_threading(ffmpeg_next::codec::threading::Config {
            kind: ffmpeg_next::codec::threading::Type::Frame,
            count: 0,
        });
        let video = if is_av1 {
            open_software_av1(fresh)?
        } else {
            fresh
                .decoder()
                .video()
                .map_err(|e| DylError::Decode(format!("open software decoder: {e}")))?
        };
        tracing::info!(
            "FfmpegVideoDecoder 回退软解: {}x{} {:?}",
            video.width(),
            video.height(),
            video.format()
        );
        self.pkt_timebase = video.packet_time_base();
        self.decoder = Some(video);
        self.hw = None;
        // 源格式随后端改变（P010LE → YUV420P10LE 等），首帧会重新配对输出通路
        self.configured = None;
        self.notice = Some(DecoderNotice {
            code: "hardware_decode_too_slow",
            message: format!(
                "{device} cannot keep up with the stream's frame rate; \
                 switched to software decode"
            ),
        });
        Ok(())
    }

    /// 解码器上下文上的色彩元数据 — 帧未标注时的回退来源
    fn context_color(&self) -> (ColorSpace, ColorPrimaries, ColorRange, ColorTransfer) {
        match self.decoder.as_ref() {
            Some(d) => (
                ColorSpace::from(d.color_space()),
                ColorPrimaries::from(d.color_primaries()),
                ColorRange::from(d.color_range()),
                ColorTransfer::from(d.color_transfer_characteristic()),
            ),
            None => Default::default(),
        }
    }

    /// 输出通路按"真正要上屏的这一帧"配好，并把像素取成 CPU 平面
    ///
    /// 硬解时 `avcodec_open2` 报告的还是软件格式（`get_format` 要到第一次解帧
    /// 才被调），而候选里没有匹配的硬件格式会静默落回软解——两种情况下都只有
    /// 帧上的格式是真的，所以通路不能只在打开时配一次。
    ///
    /// 转换器只搬像素、不搬 AVFrame 属性，所以宽高与平面取转换结果，PTS 与
    /// 色彩元数据仍由调用方从解码帧上读。
    fn take_planes(
        &mut self,
        pixels: &ffmpeg_next::frame::Video,
    ) -> Result<(u32, u32, Vec<PlaneData>)> {
        let source = (pixels.format(), pixels.width(), pixels.height());
        if self.configured != Some(source) {
            let (pixel_format, conversion) = configure_output(source.0, source.1, source.2)?;
            tracing::info!(
                "FfmpegVideoDecoder 输出通路改为 {:?} {}x{} → {:?} (转换={})",
                source.0,
                source.1,
                source.2,
                pixel_format,
                conversion_name(conversion.as_ref())
            );
            self.pixel_format = pixel_format;
            self.conversion = conversion;
            self.configured = Some(source);
        }
        let planes = match self.conversion.as_mut() {
            Some(Conversion::Sws(scaler)) => {
                let mut dst = ffmpeg_next::frame::Video::empty();
                scaler
                    .0
                    .run(pixels, &mut dst)
                    .map_err(|e| DylError::Decode(format!("swscale: {e}")))?;
                return Ok((
                    dst.width(),
                    dst.height(),
                    video_planes(&dst, self.pixel_format),
                ));
            }
            Some(Conversion::Depth10Planar) => depth10_planes(pixels),
            Some(Conversion::P010ToNv12) => p010_planes(pixels),
            None => video_planes(pixels, self.pixel_format),
        };
        Ok((pixels.width(), pixels.height(), planes))
    }
}

/// 帧的像素宽高比（SAR）；帧上没有（0/1）时退回解码器上下文
///
/// 容器（如 MP4 的 `pasp`）与 SPS VUI 都可能声明非方形像素。FFmpeg 通常把
/// 流级 SAR 复制进 `AVCodecContext.sample_aspect_ratio` 再落到帧上，但不是
/// 所有解复用器都这么做，所以两级都查。
fn frame_aspect_ratio(
    frame: &ffmpeg_next::frame::Video,
    decoder: &ffmpeg_next::decoder::Video,
) -> (i32, i32) {
    let from_frame = frame.aspect_ratio();
    if from_frame.numerator() > 0 && from_frame.denominator() > 0 {
        return (from_frame.numerator(), from_frame.denominator());
    }
    let from_context = decoder.aspect_ratio();
    if from_context.numerator() > 0 && from_context.denominator() > 0 {
        (from_context.numerator(), from_context.denominator())
    } else {
        (1, 1)
    }
}

/// GPU 帧 → CPU 帧；没有本会话的硬解方案就报错
///
/// 这里是"硬件帧不许当 CPU 字节读"那条守卫的落点。查的是帧而不是上下文：
/// `AV_CODEC_HW_CONFIG_METHOD_AD_HOC` / `INTERNAL` 允许解码器在运行中静默换成
/// GPU 驻留格式，而硬件帧的 `data[i]` 是平台句柄——按 CPU 平面拷出去会渲染出
/// 垃圾数据甚至崩溃，且整条通路无人报错。
///
/// 回传目标取帧池的 `sw_format` 而不是 `HwPlan::cpu_format`：后者是 open 时从设备帧约束
/// 里挑的预测值，而 8-bit 池是 NV12、10-bit 池是 P010LE，猜错的直接后果是
/// `av_hwframe_transfer_data` 报 `EINVAL`。
fn read_back_frame(
    plan: Option<&HwPlan>,
    decoded: &ffmpeg_next::frame::Video,
    dst: &mut ffmpeg_next::frame::Video,
) -> Result<()> {
    let format = decoded.format();
    if !is_hw_pixel(format) {
        return Ok(());
    }
    let Some(plan) = plan else {
        return Err(DylError::Decode(format!(
            "decoder produced a hardware frame ({format:?}) with no hwaccel plan for this session; \
             hardware frames carry platform handles, not pixels"
        )));
    };
    let target = hwaccel::pool_sw_format(decoded).unwrap_or_else(|| plan.cpu_format());
    hwaccel::prepare_cpu_frame(dst, target, decoded.width(), decoded.height())?;
    hwaccel::transfer(dst, decoded)
}

impl VideoDecoder for FfmpegVideoDecoder {
    fn submit_packet(&mut self, packet: &Packet) -> Result<()> {
        // 已决定回退软解：硬解继续喂包出帧，画面不断；到下一个关键帧才换解码器
        // 从 IDR 重起（解码器对象在 GOP 中途换不了）。跳过非关键帧的旧做法会把
        // 切换点前的整段画面冻住，GOP 越长冻得越久
        if self.await_key && packet.is_key {
            self.await_key = false;
            if let Err(e) = self.rebuild_software() {
                tracing::warn!("hardware decode fallback failed ({e}); keeping hardware path");
                if !self.fallback_fail_notified {
                    self.fallback_fail_notified = true;
                    self.notice = Some(DecoderNotice {
                        code: "hardware_decode_fallback_failed",
                        message: format!(
                            "could not switch to software decode ({e}); staying on hardware, \
                             will retry at the next keyframe"
                        ),
                    });
                }
                self.await_key = true;
            }
        }
        let started = Instant::now();
        let result = self.send_packet_inner(packet);
        self.fallback.busy += started.elapsed().as_secs_f64();
        result
    }

    fn receive_frame(&mut self) -> Result<Option<VideoFrame>> {
        let started = Instant::now();
        let result = self.receive_frame_inner();
        self.fallback.busy += started.elapsed().as_secs_f64();
        if let Ok(Some(frame)) = &result {
            let pts_secs = frame.pts.map(|pts| {
                pts as f64 * frame.time_base.numerator() as f64
                    / frame.time_base.denominator() as f64
            });
            if self.fallback.note_frame(pts_secs) {
                tracing::warn!(
                    "hardware decode can't keep up ({:.1}ms/frame budget); \
                     switching to software at the next keyframe",
                    self.fallback.interval.unwrap_or(0.0) * 1000.0
                );
                self.await_key = true;
            }
        }
        result
    }

    fn send_eof(&mut self) -> Result<()> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| DylError::InvalidState("video decoder not configured".into()))?;
        decoder
            .send_eof()
            .map_err(|e| DylError::Decode(format!("send_eof: {e}")))?;
        Ok(())
    }

    fn flush(&mut self) {
        if let Some(dec) = self.decoder.as_mut() {
            dec.flush();
        }
        // seek 造成的卡顿不该和旧窗口累计到一起
        self.fallback.reset_window();
    }

    fn set_skip_nonref(&mut self, skip: bool) {
        if let Some(dec) = self.decoder.as_mut() {
            dec.skip_frame(if skip {
                ffmpeg_next::Discard::NonReference
            } else {
                ffmpeg_next::Discard::Default
            });
        }
    }

    fn hardware_device(&self) -> Option<String> {
        self.hw.as_ref().map(HwPlan::device_name)
    }

    fn take_notice(&mut self) -> Option<DecoderNotice> {
        self.notice.take()
    }
}

impl FfmpegVideoDecoder {
    fn send_packet_inner(&mut self, packet: &Packet) -> Result<()> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| DylError::InvalidState("video decoder not configured".into()))?;
        // Packet 携带原始 tick + 时间基，直接同步给解码器，不做秒↔tick 往返
        if needs_timebase_update(self.pkt_timebase, packet.time_base) {
            decoder.set_packet_time_base(packet.time_base);
            self.pkt_timebase = packet.time_base;
        }
        let borrow = BorrowWithPts::new(&packet.data, packet.pts, packet.dts);
        decoder
            .send_packet(&borrow)
            .map_err(|e| DylError::Decode(format!("send_packet: {e}")))?;
        Ok(())
    }

    fn receive_frame_inner(&mut self) -> Result<Option<VideoFrame>> {
        let (ctx_space, ctx_primaries, ctx_range, ctx_transfer) = self.context_color();
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| DylError::InvalidState("video decoder not configured".into()))?;
        let mut frame = ffmpeg_next::frame::Video::empty();
        match decoder.receive_frame(&mut frame) {
            Ok(()) => {
                // 像素宽高比要在借用 `decoder` 的这段里读完（后面 take_planes
                // 会再借 `&mut self`），显示尺寸等像素落地后再算
                let (sar_num, sar_den) = frame_aspect_ratio(&frame, decoder);
                // 硬件帧先回传成 CPU 字节：渲染通路只认 CPU 平面（方案 5.2）
                let from_hw = is_hw_pixel(frame.format());
                if from_hw {
                    read_back_frame(self.hw.as_ref(), &frame, &mut self.cpu_frame)?;
                }
                // 把复用的回传目标从 self 借出来当局部源，用完立刻放回去：
                // `&self.cpu_frame` 与 `self.conversion.as_mut()` 同时存在过不了借用检查
                let cpu = from_hw.then(|| {
                    std::mem::replace(&mut self.cpu_frame, ffmpeg_next::frame::Video::empty())
                });
                // 转换失败也要把回传缓冲放回去，否则下一帧只能重新分配
                let result = self.take_planes(cpu.as_ref().unwrap_or(&frame));
                if let Some(cpu) = cpu {
                    self.cpu_frame = cpu;
                }
                let (width, height, planes) = result?;

                // 视频解码帧 PTS 是输入 Packet PTS 的透传，时间基同为 pkt_timebase；
                // 无时间戳（NOPTS）时保持 None，不伪装成 0。硬解也一律取 GPU 帧：
                // 实测（方案 5.2）`av_hwframe_transfer_data` 既不搬 PTS 也不搬色彩属性，
                // GPU 帧 pts=Some(0) 回传后是 None、BT709/MPEG 回传后变 Unspecified，
                // 所以属性必须在回传之前从 GPU 帧读，否则会出现"帧能出、时间戳全 0"这类静默错误
                let pts = frame.pts();
                // 色彩元数据：帧上没写就退回解码器上下文（H.264/HEVC 的 SPS VUI
                // 解析结果通常只落在 ctx 上，帧未必继承）
                let color_space = match ColorSpace::from(frame.color_space()) {
                    ColorSpace::Unspecified => ctx_space,
                    s => s,
                };
                let color_primaries = match ColorPrimaries::from(frame.color_primaries()) {
                    ColorPrimaries::Unspecified => ctx_primaries,
                    p => p,
                };
                let color_range = match ColorRange::from(frame.color_range()) {
                    ColorRange::Unspecified => ctx_range,
                    r => r,
                };
                // 传输函数沿用同一条"帧优先、上下文兜底"的回退：HEVC 的 VUI
                // 解析结果通常只落在 ctx 上
                let color_transfer = match frame.color_transfer_characteristic() {
                    ffmpeg_next::color::TransferCharacteristic::Unspecified => ctx_transfer,
                    t => ColorTransfer::from(t),
                };
                // 编码宽高之外还要给渲染几何一份显示尺寸：非方形像素（SAR≠1）
                // 的视频如果只按编码宽高等比适配，画面会被拉变形
                let (display_width, display_height) = display_size(width, height, sar_num, sar_den);

                Ok(Some(VideoFrame {
                    pts,
                    time_base: self.pkt_timebase,
                    width,
                    height,
                    display_width,
                    display_height,
                    format: self.pixel_format,
                    color_space,
                    color_primaries,
                    color_range,
                    color_transfer,
                    planes,
                }))
            }
            Err(e) if is_eagain(&e) => Ok(None),
            Err(e) => Err(DylError::Decode(format!("receive_frame: {e}"))),
        }
    }
}

/// FFmpeg 音频解码器
pub struct FfmpegAudioDecoder {
    decoder: Option<ffmpeg_next::decoder::Audio>,
    sample_format: SampleFormat,
    sample_rate: u32,
    channels: u32,
    /// Packet 时间基 — 由首个有效 Packet.time_base 同步到 avctx.pkt_timebase
    pkt_timebase: ffmpeg_next::Rational,
}

impl FfmpegAudioDecoder {
    pub fn new() -> Self {
        Self {
            decoder: None,
            sample_format: SampleFormat::Unknown,
            sample_rate: 0,
            channels: 0,
            pkt_timebase: ffmpeg_next::Rational::new(0, 1),
        }
    }

    /// 从 CodecContext 创建已配置的解码器
    pub fn from_stream_context(ctx: ffmpeg_next::codec::Context) -> Result<Self> {
        let audio = ctx
            .decoder()
            .audio()
            .map_err(|e| DylError::Decode(format!("audio decoder open: {e}")))?;
        let rate = audio.rate();
        let ch = audio.channels() as u32;
        let sf = SampleFormat::from(audio.format());
        let pkt_timebase = audio.packet_time_base();
        tracing::info!(
            "FfmpegAudioDecoder from_stream_context: {}Hz {}ch {:?}, pkt_timebase={}",
            rate,
            ch,
            sf,
            pkt_timebase
        );
        Ok(Self {
            decoder: Some(audio),
            sample_format: sf,
            sample_rate: rate,
            channels: ch,
            pkt_timebase,
        })
    }

    /// 解码音频帧的时间基 = Packet 时间基
    ///
    /// FFmpeg 解码器把 `AVFrame.pts` 写在 `avctx.pkt_timebase` 上（实测 MP4 的
    /// `-1024/0/1024…` 与 MKV 的 `0/23/46…` 都与输入包逐 tick 相同），采样单位只是
    /// MP4 音频轨道 timescale 恰好等于采样率时的巧合。MKV 的时间基永远是 1/1000，
    /// 按采样率换算会把 4.617s 的帧读成 0.105s，音频主时钟的锚点随之变成负数，
    /// 整条播放被拖到两成速。时间基无效时才退回 1/sample_rate。
    fn frame_timebase(
        sample_rate: u32,
        pkt_timebase: ffmpeg_next::Rational,
    ) -> ffmpeg_next::Rational {
        if pkt_timebase.numerator() != 0 && pkt_timebase.denominator() != 0 {
            pkt_timebase
        } else {
            ffmpeg_next::Rational::new(1, sample_rate.max(1) as i32)
        }
    }
}

impl Default for FfmpegAudioDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDecoder for FfmpegAudioDecoder {
    fn submit_packet(&mut self, packet: &Packet) -> Result<()> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| DylError::InvalidState("audio decoder not configured".into()))?;
        // 解码器需要正确的 pkt_timebase：帧 PTS 就是按它输出的
        if needs_timebase_update(self.pkt_timebase, packet.time_base) {
            decoder.set_packet_time_base(packet.time_base);
            self.pkt_timebase = packet.time_base;
        }
        let borrow = BorrowWithPts::new(&packet.data, packet.pts, packet.dts);
        decoder
            .send_packet(&borrow)
            .map_err(|e| DylError::Decode(format!("send_packet: {e}")))?;
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Option<AudioFrame>> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| DylError::InvalidState("audio decoder not configured".into()))?;
        let mut frame = ffmpeg_next::frame::Audio::empty();
        match decoder.receive_frame(&mut frame) {
            Ok(()) => {
                // 帧 PTS 沿用 Packet 时间基（见 frame_timebase），无时间戳保持 None
                let pts = frame.pts();
                // AAC 等解码器输出的 AVFrame 可能不填 ch_layout / sample_rate，
                // 此时必须以解码器上下文为准，否则声道数算成 0、整帧采样被当成空
                let channels = match frame.channels() as usize {
                    0 => self.channels as usize,
                    n => n,
                };
                let sample_rate = match frame.rate() {
                    0 => self.sample_rate,
                    r => r,
                };
                let time_base = Self::frame_timebase(sample_rate, self.pkt_timebase);
                let samples = extract_audio_samples(&frame, channels);
                Ok(Some(AudioFrame {
                    pts,
                    time_base,
                    sample_rate,
                    channels: channels as u32,
                    format: self.sample_format,
                    samples,
                }))
            }
            Err(e) if is_eagain(&e) => Ok(None),
            Err(e) => Err(DylError::Decode(format!("receive_frame: {e:?}"))),
        }
    }

    fn send_eof(&mut self) -> Result<()> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| DylError::InvalidState("audio decoder not configured".into()))?;
        decoder
            .send_eof()
            .map_err(|e| DylError::Decode(format!("send_eof: {e}")))?;
        Ok(())
    }

    fn flush(&mut self) {
        if let Some(dec) = self.decoder.as_mut() {
            dec.flush();
        }
    }
}

/// Packet 时间基有效且与解码器已记录值不同 → 需要同步到 avctx.pkt_timebase
fn needs_timebase_update(current: ffmpeg_next::Rational, incoming: ffmpeg_next::Rational) -> bool {
    incoming.numerator() != 0
        && incoming.denominator() != 0
        && (current.numerator() != incoming.numerator()
            || current.denominator() != incoming.denominator())
}

/// 决定解码输出格式与转换器
///
/// 硬件（GPU 驻留）像素格式在这里就被拒绝，而不是让它落进
/// `PixelFormat::Unknown`：`video_planes` 对未知格式走"单平面"分支，会把
/// `AVFrame.data[0]` 里的平台句柄当像素字节拷走——画面全绿或直接崩，
/// 而且整条通路不报错。回传后的帧已经是 CPU 格式，正常走不到这条分支；
/// 它挡住的是"新增了一条产出 GPU 帧的通路却忘了在调用前回传"。
fn configure_output(
    src_format: ffmpeg_next::format::Pixel,
    width: u32,
    height: u32,
) -> Result<(PixelFormat, Option<Conversion>)> {
    use ffmpeg_next::format::Pixel;
    if is_hw_pixel(src_format) {
        return Err(DylError::Decode(format!(
            "output path asked for hardware pixel format {src_format:?}; \
             read it back with av_hwframe_transfer_data first"
        )));
    }
    // 10-bit 只降位深：4K 单线程 swscale 要 ~16ms/帧，位移是纯内存拷贝
    match src_format {
        Pixel::YUV420P10LE => {
            return Ok((PixelFormat::Yuv420p, Some(Conversion::Depth10Planar)));
        }
        Pixel::P010LE => {
            return Ok((PixelFormat::Nv12, Some(Conversion::P010ToNv12)));
        }
        _ => {}
    }
    match PixelFormat::from(src_format) {
        PixelFormat::Unknown => match build_scaler(src_format, width, height) {
            Some(scaler) => Ok((PixelFormat::Yuv420p, Some(Conversion::Sws(scaler)))),
            None => Ok((PixelFormat::Unknown, None)),
        },
        pixel_format => Ok((pixel_format, None)),
    }
}

fn conversion_name(conversion: Option<&Conversion>) -> &'static str {
    match conversion {
        Some(Conversion::Sws(_)) => "swscale",
        Some(Conversion::Depth10Planar) => "位深10→8",
        Some(Conversion::P010ToNv12) => "P010→NV12",
        None => "无",
    }
}

/// YUV420P10LE → 三个 8-bit 平面（每采样右移 2）
fn depth10_planes(frame: &ffmpeg_next::frame::Video) -> Vec<PlaneData> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let (chroma_width, chroma_height) = (width / 2, height / 2);
    vec![
        PlaneData {
            data: shift16_to_8(frame.data(0), frame.stride(0), width, height, 2),
            stride: width,
        },
        PlaneData {
            data: shift16_to_8(
                frame.data(1),
                frame.stride(1),
                chroma_width,
                chroma_height,
                2,
            ),
            stride: chroma_width,
        },
        PlaneData {
            data: shift16_to_8(
                frame.data(2),
                frame.stride(2),
                chroma_width,
                chroma_height,
                2,
            ),
            stride: chroma_width,
        },
    ]
}

/// P010LE → NV12（每采样右移 8；UV 平面本就是交织的，逐行照搬）
fn p010_planes(frame: &ffmpeg_next::frame::Video) -> Vec<PlaneData> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    vec![
        PlaneData {
            data: shift16_to_8(frame.data(0), frame.stride(0), width, height, 8),
            stride: width,
        },
        PlaneData {
            // 色度：每行 width/2 个像素 × U/V 两个分量 = width 个 u16
            data: shift16_to_8(frame.data(1), frame.stride(1), width, height / 2, 8),
            stride: width,
        },
    ]
}

/// 把 16-bit 采样行右移成 8-bit 紧凑平面
///
/// 源行可能带对齐填充（`src_stride` 一般大于有效宽度），输出不带填充——
/// 渲染端会自行补齐 wgpu 的行对齐。缓冲比声明尺寸短时其余行补零，长度恒为
/// `out_row_bytes * rows`。
fn shift16_to_8(
    src: &[u8],
    src_stride: usize,
    out_row_bytes: usize,
    rows: usize,
    shift: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; out_row_bytes * rows];
    for row in 0..rows {
        let Some(line) = src.get(row * src_stride..) else {
            break;
        };
        let usable = (out_row_bytes * 2).min(line.len());
        let (pairs, _) = line[..usable].as_chunks::<2>();
        for (index, pair) in pairs.iter().enumerate() {
            out[row * out_row_bytes + index] = (u16::from_le_bytes(*pair) >> shift) as u8;
        }
    }
    out
}

/// 渲染通路只认 YUV420P / NV12 / RGB24 / BGRA；其它格式（10-bit、16-bit、
/// 4:2:2、4:4:4…）必须在解码器内转成 YUV420P，否则平面拷贝只取第 0 平面，
/// 色度整条丢失且不会报错。硬件像素格式在 `read_back_frame` 里就先回传成 CPU
/// 字节，不会走到这里；其余转换失败返回 `None`，让帧保持原生布局。
fn build_scaler(
    src_format: ffmpeg_next::format::Pixel,
    width: u32,
    height: u32,
) -> Option<SwsScaler> {
    if src_format == ffmpeg_next::format::Pixel::None {
        tracing::warn!("video decoder: unknown pixel format, no conversion set up");
        return None;
    }
    match ffmpeg_next::software::scaling::Context::get(
        src_format,
        width,
        height,
        ffmpeg_next::format::Pixel::YUV420P,
        width,
        height,
        ffmpeg_next::software::scaling::flag::Flags::BILINEAR,
    ) {
        Ok(context) => Some(SwsScaler(context)),
        Err(e) => {
            tracing::warn!(
                "video decoder: swscale {src_format:?} -> YUV420P unavailable ({e}); \
                 frames will keep their native layout"
            );
            None
        }
    }
}

/// 按输出像素格式取出解码帧的各平面（含 FFmpeg 的行对齐填充）
fn video_planes(frame: &ffmpeg_next::frame::Video, format: PixelFormat) -> Vec<PlaneData> {
    let num_planes = match format {
        PixelFormat::Yuv420p => 3,
        PixelFormat::Nv12 => 2,
        _ => 1,
    };
    (0..num_planes)
        .map(|i| PlaneData {
            data: frame.data(i).to_vec(),
            stride: frame.stride(i),
        })
        .collect()
}

/// BorrowWithPts — 类似 ffmpeg_next::packet::Borrow，但在 AVPacket 中设置 PTS/DTS
///
/// ffmpeg_next 的 Borrow::new() 只传递数据字节，将 AVPacket.pts 初始化为 0
/// （不等于 AV_NOPTS_VALUE），导致解码帧的 PTS 始终为 0。
/// 此结构体额外设置 pts/dts，确保解码帧继承正确的时间戳。
struct BorrowWithPts<'a> {
    packet: ffmpeg_next::ffi::AVPacket,
    _data: &'a [u8],
}

impl<'a> BorrowWithPts<'a> {
    fn new(data: &'a [u8], pts: Option<i64>, dts: Option<i64>) -> Self {
        use ffmpeg_next::ffi::*;
        unsafe {
            let mut packet: AVPacket = std::mem::zeroed();
            packet.data = data.as_ptr() as *mut _;
            packet.size = data.len() as i32;
            packet.pts = pts.unwrap_or(AV_NOPTS_VALUE);
            packet.dts = dts.unwrap_or(AV_NOPTS_VALUE);
            BorrowWithPts {
                packet,
                _data: data,
            }
        }
    }
}

impl<'a> Ref for BorrowWithPts<'a> {
    fn as_ptr(&self) -> *const ffmpeg_next::ffi::AVPacket {
        &self.packet
    }
}

impl<'a> Drop for BorrowWithPts<'a> {
    fn drop(&mut self) {
        unsafe {
            self.packet.data = std::ptr::null_mut();
            self.packet.size = 0;
            ffmpeg_next::ffi::av_packet_unref(&mut self.packet);
        }
    }
}

/// Check if ffmpeg error is EAGAIN (resource temporarily unavailable)
fn is_eagain(err: &ffmpeg_next::Error) -> bool {
    matches!(err, ffmpeg_next::Error::Other { errno } if *errno == ffmpeg_next::error::EAGAIN)
}

/// 从 FFmpeg 音频帧提取 f32 采样
///
/// `ctx_channels`：解码器上下文的声道数。AAC 等解码器输出的 AVFrame 不填
/// `ch_layout`，只按帧上的字段算声道会得到 0，整帧采样就被当成空；帧的安全
/// 访问器（`planes()` / `data()`）同样依赖 `ch_layout`，因此直接读取
/// `AVFrame` 的 `data[]` / `linesize[]`。
fn extract_audio_samples(frame: &ffmpeg_next::frame::Audio, ctx_channels: usize) -> Vec<f32> {
    let channels = match frame.channels() as usize {
        0 => ctx_channels.max(1),
        n => n,
    };
    unsafe {
        let p = frame.as_ptr();
        extract_from_planes(
            &(*p).data,
            &(*p).linesize,
            frame.format(),
            frame.samples(),
            channels,
        )
    }
}

/// 从 `AVFrame` 的数据平面提取 f32 采样
///
/// 处理 planar（每声道独立 buffer）和 packed（声道交错在 data[0]）两种布局。
/// 支持 F32/I16/I32/U8/F64 格式，统一归一化到 [-1.0, 1.0] f32 范围。
///
/// # Safety
/// `plane_ptrs` / `linesize` 必须是某个有效 `AVFrame` 的 8 项数据平面与行宽。
unsafe fn extract_from_planes(
    plane_ptrs: &[*mut u8; 8],
    linesize: &[i32; 8],
    format: ffmpeg_next::format::Sample,
    samples: usize,
    channels: usize,
) -> Vec<f32> {
    let channels = channels.max(1);
    let is_planar = format.is_planar();
    let plane_bytes = if is_planar {
        samples * format.bytes()
    } else {
        samples * channels * format.bytes()
    };
    let plane_data = |index: usize| -> &[u8] {
        match plane_ptrs.get(index).copied() {
            Some(ptr) if !ptr.is_null() => {
                // 音频帧只有 `linesize[0]` 有效（planar 时它是单个平面的字节数），
                // 其余声道平面恒为 0；把 0 当长度截断会让右声道整帧静音
                let line = linesize.get(index).copied().unwrap_or(0);
                let len = if line > 0 {
                    plane_bytes.min(line as usize)
                } else {
                    plane_bytes
                };
                std::slice::from_raw_parts(ptr as *const u8, len)
            }
            _ => &[],
        }
    };

    let mut output = Vec::with_capacity(samples * channels);

    if is_planar {
        // Planar: 每个声道有独立的 buffer，逐声道逐采样交错输出
        let planes: Vec<&[u8]> = (0..channels).map(plane_data).collect();
        for s in 0..samples {
            for data in planes.iter() {
                let sample = match format {
                    ffmpeg_next::format::Sample::F32(ffmpeg_next::format::sample::Type::Planar) => {
                        let offset = s * 4;
                        if offset + 4 <= data.len() {
                            f32::from_ne_bytes([
                                data[offset],
                                data[offset + 1],
                                data[offset + 2],
                                data[offset + 3],
                            ])
                        } else {
                            0.0
                        }
                    }
                    ffmpeg_next::format::Sample::F64(ffmpeg_next::format::sample::Type::Planar) => {
                        let offset = s * 8;
                        if offset + 8 <= data.len() {
                            f64::from_ne_bytes([
                                data[offset],
                                data[offset + 1],
                                data[offset + 2],
                                data[offset + 3],
                                data[offset + 4],
                                data[offset + 5],
                                data[offset + 6],
                                data[offset + 7],
                            ]) as f32
                        } else {
                            0.0
                        }
                    }
                    ffmpeg_next::format::Sample::I16(ffmpeg_next::format::sample::Type::Planar) => {
                        let offset = s * 2;
                        if offset + 2 <= data.len() {
                            i16::from_ne_bytes([data[offset], data[offset + 1]]) as f32
                                / i16::MAX as f32
                        } else {
                            0.0
                        }
                    }
                    ffmpeg_next::format::Sample::I32(ffmpeg_next::format::sample::Type::Planar) => {
                        let offset = s * 4;
                        if offset + 4 <= data.len() {
                            i32::from_ne_bytes([
                                data[offset],
                                data[offset + 1],
                                data[offset + 2],
                                data[offset + 3],
                            ]) as f32
                                / i32::MAX as f32
                        } else {
                            0.0
                        }
                    }
                    ffmpeg_next::format::Sample::U8(ffmpeg_next::format::sample::Type::Planar)
                        if s < data.len() =>
                    {
                        (data[s] as f32 - 128.0) / 128.0
                    }
                    _ => 0.0,
                };
                output.push(sample);
            }
        }
    } else {
        // Packed: 所有声道交错存储在 data(0)
        let data = plane_data(0);
        let total = samples * channels;
        match format {
            ffmpeg_next::format::Sample::F32(ffmpeg_next::format::sample::Type::Packed) => {
                for i in 0..total {
                    let offset = i * 4;
                    if offset + 4 <= data.len() {
                        output.push(f32::from_ne_bytes([
                            data[offset],
                            data[offset + 1],
                            data[offset + 2],
                            data[offset + 3],
                        ]));
                    } else {
                        output.push(0.0);
                    }
                }
            }
            ffmpeg_next::format::Sample::F64(ffmpeg_next::format::sample::Type::Packed) => {
                for i in 0..total {
                    let offset = i * 8;
                    if offset + 8 <= data.len() {
                        output.push(f64::from_ne_bytes([
                            data[offset],
                            data[offset + 1],
                            data[offset + 2],
                            data[offset + 3],
                            data[offset + 4],
                            data[offset + 5],
                            data[offset + 6],
                            data[offset + 7],
                        ]) as f32);
                    } else {
                        output.push(0.0);
                    }
                }
            }
            ffmpeg_next::format::Sample::I16(ffmpeg_next::format::sample::Type::Packed) => {
                for i in 0..total {
                    let offset = i * 2;
                    if offset + 2 <= data.len() {
                        output.push(
                            i16::from_ne_bytes([data[offset], data[offset + 1]]) as f32
                                / i16::MAX as f32,
                        );
                    } else {
                        output.push(0.0);
                    }
                }
            }
            ffmpeg_next::format::Sample::I32(ffmpeg_next::format::sample::Type::Packed) => {
                for i in 0..total {
                    let offset = i * 4;
                    if offset + 4 <= data.len() {
                        output.push(
                            i32::from_ne_bytes([
                                data[offset],
                                data[offset + 1],
                                data[offset + 2],
                                data[offset + 3],
                            ]) as f32
                                / i32::MAX as f32,
                        );
                    } else {
                        output.push(0.0);
                    }
                }
            }
            ffmpeg_next::format::Sample::U8(ffmpeg_next::format::sample::Type::Packed) => {
                for i in 0..total {
                    if i < data.len() {
                        output.push((data[i] as f32 - 128.0) / 128.0);
                    } else {
                        output.push(0.0);
                    }
                }
            }
            _ => {
                // 未知格式：输出零采样，避免噪声
                tracing::warn!(
                    "extract_audio_samples: unsupported packed format {:?}",
                    format
                );
                output.resize(total, 0.0);
            }
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MKV 的时间基是 1/1000，采样率换算会把 4.617s 读成 0.105s；
    /// 只有 packet 无效（0/1）时才允许退回 1/sample_rate
    #[test]
    fn audio_frame_timebase_prefers_packet() {
        let ms = ffmpeg_next::Rational::new(1, 1000);
        assert_eq!(FfmpegAudioDecoder::frame_timebase(44100, ms), ms);
        assert_eq!(
            FfmpegAudioDecoder::frame_timebase(48000, ffmpeg_next::Rational::new(0, 1)),
            ffmpeg_next::Rational::new(1, 48000)
        );
    }

    /// 声道数缺失的解码帧（AAC 不填 ch_layout）必须按上下文声道数提取，
    /// 否则整帧采样被当成空，播放端只有静音
    #[test]
    fn extract_from_planes_ignores_frame_channel_layout() {
        use ffmpeg_next::format::sample::Type;
        use ffmpeg_next::format::Sample;

        let left: [f32; 2] = [0.5, -0.25];
        let right: [f32; 2] = [0.125, -0.5];
        let mut plane_ptrs: [*mut u8; 8] = [std::ptr::null_mut(); 8];
        let mut linesize: [i32; 8] = [0; 8];
        let plane_bytes = (left.len() * std::mem::size_of::<f32>()) as i32;
        plane_ptrs[0] = left.as_ptr() as *mut u8;
        linesize[0] = plane_bytes;

        let mono =
            unsafe { extract_from_planes(&plane_ptrs, &linesize, Sample::F32(Type::Planar), 2, 1) };
        assert_eq!(mono, vec![0.5, -0.25]);

        plane_ptrs[1] = right.as_ptr() as *mut u8;
        linesize[1] = plane_bytes;
        let stereo =
            unsafe { extract_from_planes(&plane_ptrs, &linesize, Sample::F32(Type::Planar), 2, 2) };
        assert_eq!(stereo, vec![0.5, 0.125, -0.25, -0.5], "planar 应交错输出");
    }

    /// 真实解码帧的 planar 音频只有 `linesize[0]` 非零（planar 时它是单平面
    /// 字节数），其余声道平面恒为 0 —— 按 `linesize[i]` 截断会把右声道读成
    /// 静音。单声道素材（test_video.mp4 / multitrack.mkv）测不出来，这里用
    /// `av_frame_get_buffer` 造一帧真实布局的 FLTP 立体声把缺陷钉住。
    #[test]
    fn planar_stereo_keeps_channels_whose_linesize_is_zero() {
        let mut frame = ffmpeg_next::frame::Audio::empty();
        unsafe {
            let p = frame.as_mut_ptr();
            (*p).format = ffmpeg_next::ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as _;
            (*p).nb_samples = 4;
            ffmpeg_next::ffi::av_channel_layout_default(&mut (*p).ch_layout, 2);
            assert_eq!(ffmpeg_next::ffi::av_frame_get_buffer(p, 0), 0);
            assert_eq!(
                (*p).linesize[1],
                0,
                "测试前提：真实解码帧只有 linesize[0] 有效"
            );
            for (channel, value) in [(0usize, 0.5f32), (1, -0.25)] {
                let plane = (*p).extended_data.add(channel).read() as *mut f32;
                assert!(!plane.is_null());
                for i in 0..4 {
                    *plane.add(i) = value;
                }
            }
        }

        let samples = extract_audio_samples(&frame, 2);
        assert_eq!(samples.len(), 8, "立体声 4 采样应展平为 8 个 f32");
        let (pairs, remainder) = samples.as_chunks::<2>();
        assert!(remainder.is_empty(), "交错数据必须按声道对齐");
        for pair in pairs {
            assert_eq!(pair, &[0.5, -0.25], "左右声道都必须保留");
        }
    }

    /// 10-bit HEVC 等高位深帧必须经 swscale 落成 3 个 8-bit 平面；
    /// 直接按原生布局取平面会只剩亮度（色度整条丢失）且不报错
    #[test]
    fn scaler_converts_10bit_yuv420p10le_to_three_planes() {
        use ffmpeg_next::format::Pixel;
        let _ = ffmpeg_next::init();

        let mut scaler = build_scaler(Pixel::YUV420P10LE, 192, 108).expect("sws context");
        let mut src = ffmpeg_next::frame::Video::new(Pixel::YUV420P10LE, 192, 108);
        // 10-bit 存的是左移 2 位的值：luma 940 → 8-bit 235，chroma 512 → 128
        for chunk in src.data_mut(0).as_chunks_mut::<2>().0 {
            *chunk = (235u16 << 2).to_ne_bytes();
        }
        for plane in [1, 2] {
            for chunk in src.data_mut(plane).as_chunks_mut::<2>().0 {
                *chunk = (128u16 << 2).to_ne_bytes();
            }
        }

        let mut dst = ffmpeg_next::frame::Video::empty();
        scaler.0.run(&src, &mut dst).expect("sws_scale");
        assert_eq!(dst.format(), Pixel::YUV420P);

        let planes = video_planes(&dst, PixelFormat::Yuv420p);
        assert_eq!(planes.len(), 3, "转换后必须带齐 Y/U/V");
        assert_eq!(planes[0].data.len(), planes[0].stride * 108);
        assert_eq!(planes[1].data.len(), planes[1].stride * 54);
        assert!(
            planes[0].data.iter().all(|&v| v == 235),
            "luma 未落回 8-bit: {:?}",
            &planes[0].data[..planes[0].stride.min(8)]
        );
        assert!(
            planes[1].data.iter().all(|&v| v == 128) && planes[2].data.iter().all(|&v| v == 128),
            "chroma 未落回 8-bit: u={:?} v={:?}",
            &planes[1].data[..4],
            &planes[2].data[..4]
        );
    }

    /// 已经是 YUV420P 的格式不需要转换，由调用方判断后传 None
    #[test]
    fn scaler_rejects_unspecified_format() {
        assert!(build_scaler(ffmpeg_next::format::Pixel::None, 1920, 1080).is_none());
    }

    #[test]
    fn test_video_decoder_new() {
        let d = FfmpegVideoDecoder::new();
        assert!(d.decoder.is_none());
        assert!(d.conversion.is_none());
    }

    #[test]
    fn test_video_decoder_receive_frame_not_ready() {
        let mut d = FfmpegVideoDecoder::new();
        let result = d.receive_frame();
        assert!(result.is_err());
    }

    #[test]
    fn test_video_decoder_submit_packet_not_ready() {
        let mut d = FfmpegVideoDecoder::new();
        let pkt = Packet {
            data: vec![],
            stream_index: 0,
            pts: None,
            dts: None,
            duration: None,
            time_base: ffmpeg_next::Rational::new(1, 90000),
            is_key: false,
        };
        let result = d.submit_packet(&pkt);
        assert!(result.is_err());
    }

    #[test]
    fn test_video_decoder_flush_no_panic() {
        let mut d = FfmpegVideoDecoder::new();
        d.flush();
    }

    #[test]
    fn test_audio_decoder_new() {
        let d = FfmpegAudioDecoder::new();
        assert!(d.decoder.is_none());
    }

    #[test]
    fn test_audio_decoder_flush_no_panic() {
        let mut d = FfmpegAudioDecoder::new();
        d.flush();
    }

    #[test]
    fn test_needs_timebase_update() {
        let tb = ffmpeg_next::Rational::new(1, 15360);
        // 未设置（0/1）→ 需要更新
        assert!(needs_timebase_update(ffmpeg_next::Rational::new(0, 1), tb));
        // 相同 → 不更新
        assert!(!needs_timebase_update(tb, tb));
        // 无效时间基 → 不更新（不得用 0/1 覆盖已有正确基）
        assert!(!needs_timebase_update(tb, ffmpeg_next::Rational::new(0, 1)));
        assert!(!needs_timebase_update(tb, ffmpeg_next::Rational::new(5, 0)));
    }

    /// 回归：NOPTS Packet 进入 AVPacket 时必须是 AV_NOPTS_VALUE，而不是 0
    #[test]
    fn test_borrow_with_pts_keeps_nopts() {
        use ffmpeg_next::ffi::AV_NOPTS_VALUE;
        let data = vec![1u8, 2, 3];

        let borrow = BorrowWithPts::new(&data, None, None);
        let raw = unsafe { &*borrow.as_ptr() };
        assert_eq!(raw.pts, AV_NOPTS_VALUE);
        assert_eq!(raw.dts, AV_NOPTS_VALUE);

        // 真实 0 时间戳必须原样保留，与 NOPTS 区分
        let zero = BorrowWithPts::new(&data, Some(0), Some(0));
        let zero_raw = unsafe { &*zero.as_ptr() };
        assert_eq!(zero_raw.pts, 0);
        assert_eq!(zero_raw.dts, 0);
    }

    /// 只改 `AVFrame.format`，不分配缓冲：硬件帧本来就没有 CPU 数据可分配
    fn frame_with_format(pixel: ffmpeg_next::format::Pixel) -> ffmpeg_next::frame::Video {
        let mut frame = ffmpeg_next::frame::Video::empty();
        unsafe {
            (*frame.as_mut_ptr()).format = ffmpeg_next::ffi::AVPixelFormat::from(pixel) as _;
        }
        frame
    }

    /// 硬件格式必须被识别——`PixelFormat::from` 把它们全折叠成 `Unknown`，
    /// 靠格式清单挑出来迟早漏掉新后端
    #[test]
    fn hw_pixel_flag_drives_detection() {
        for pixel in [
            ffmpeg_next::format::Pixel::DXVA2_VLD,
            ffmpeg_next::format::Pixel::D3D11,
            ffmpeg_next::format::Pixel::VAAPI,
            ffmpeg_next::format::Pixel::VIDEOTOOLBOX,
            ffmpeg_next::format::Pixel::MEDIACODEC,
            ffmpeg_next::format::Pixel::CUDA,
            ffmpeg_next::format::Pixel::VULKAN,
            ffmpeg_next::format::Pixel::D3D12,
        ] {
            assert!(is_hw_pixel(pixel), "{pixel:?} must be reported as hardware");
        }
        for pixel in [
            ffmpeg_next::format::Pixel::YUV420P,
            ffmpeg_next::format::Pixel::NV12,
            ffmpeg_next::format::Pixel::YUV420P10LE,
            ffmpeg_next::format::Pixel::RGB24,
            ffmpeg_next::format::Pixel::None,
        ] {
            assert!(!is_hw_pixel(pixel), "{pixel:?} is CPU-readable");
        }
    }

    /// 硬件帧不许退化成"单平面 CPU 帧"上传：句柄当字节拷走会花屏/崩溃且不报错
    ///
    /// 这里只测"没有方案就拒绝"那一半；有方案的那一半要真的设备上下文和 GPU
    /// 帧，由 `hw_decode_ab` 例子在真机上比对。
    #[test]
    fn hardware_frame_without_plan_is_refused() {
        for pixel in [
            ffmpeg_next::format::Pixel::DXVA2_VLD,
            ffmpeg_next::format::Pixel::D3D11,
            ffmpeg_next::format::Pixel::VULKAN,
        ] {
            let err = read_back_frame(
                None,
                &frame_with_format(pixel),
                &mut ffmpeg_next::frame::Video::empty(),
            )
            .unwrap_err();
            assert!(matches!(err, DylError::Decode(_)), "{pixel:?}: {err}");
        }
        for pixel in [
            ffmpeg_next::format::Pixel::YUV420P,
            ffmpeg_next::format::Pixel::NV12,
        ] {
            assert!(
                read_back_frame(
                    None,
                    &frame_with_format(pixel),
                    &mut ffmpeg_next::frame::Video::empty()
                )
                .is_ok(),
                "{pixel:?} 是 CPU 格式，不该报错"
            );
        }
    }

    /// 配置阶段同样拒绝硬件格式；10-bit 等 CPU 格式的转换通路不受影响
    #[test]
    fn configure_output_refuses_hardware_formats() {
        ffmpeg_next::init().expect("ffmpeg init");
        for pixel in [
            ffmpeg_next::format::Pixel::DXVA2_VLD,
            ffmpeg_next::format::Pixel::D3D11,
        ] {
            let err = match configure_output(pixel, 64, 64) {
                Ok(_) => panic!("{pixel:?} must be refused, not configured"),
                Err(err) => err,
            };
            assert!(matches!(err, DylError::Decode(_)), "{pixel:?}: {err}");
        }
        let (format, conversion) =
            configure_output(ffmpeg_next::format::Pixel::YUV420P, 64, 64).expect("yuv420p config");
        assert_eq!(format, PixelFormat::Yuv420p);
        assert!(conversion.is_none(), "native format needs no conversion");

        let (format, conversion) =
            configure_output(ffmpeg_next::format::Pixel::YUV420P10LE, 64, 64)
                .expect("10-bit config");
        assert_eq!(format, PixelFormat::Yuv420p);
        assert!(
            matches!(conversion, Some(Conversion::Depth10Planar)),
            "10-bit must convert, not lose chroma"
        );

        let (format, conversion) =
            configure_output(ffmpeg_next::format::Pixel::P010LE, 64, 64).expect("p010 config");
        assert_eq!(format, PixelFormat::Nv12);
        assert!(
            matches!(conversion, Some(Conversion::P010ToNv12)),
            "P010 回传帧必须落成渲染端认得的 NV12"
        );
    }

    /// 位深下调只是右移：10-bit 的 940/512 与 P010 的 240/128 都必须精确落回
    /// 8-bit 的 235/128，且输出是不带源行填充的紧凑平面
    #[test]
    fn depth_conversions_shift_to_tight_8bit_planes() {
        use ffmpeg_next::format::Pixel;

        let mut planar = ffmpeg_next::frame::Video::new(Pixel::YUV420P10LE, 64, 32);
        for row in 0..32usize {
            let stride = planar.stride(0);
            for x in 0..64 {
                let value = (200u16 << 2).to_ne_bytes();
                planar.data_mut(0)[row * stride + x * 2..row * stride + x * 2 + 2]
                    .copy_from_slice(&value);
            }
        }
        for plane in [1, 2] {
            let stride = planar.stride(plane);
            for row in 0..16usize {
                for x in 0..32 {
                    let value = (128u16 << 2).to_ne_bytes();
                    planar.data_mut(plane)[row * stride + x * 2..row * stride + x * 2 + 2]
                        .copy_from_slice(&value);
                }
            }
        }
        let planes = depth10_planes(&planar);
        assert_eq!(planes.len(), 3);
        assert_eq!(planes[0].stride, 64);
        assert_eq!(planes[0].data.len(), 64 * 32, "输出必须紧凑、逐行无填充");
        assert!(planes[0].data.iter().all(|&v| v == 200));
        assert_eq!(planes[1].stride, 32);
        assert_eq!(planes[1].data.len(), 32 * 16);
        assert!(planes[1].data.iter().all(|&v| v == 128));

        let mut p010 = ffmpeg_next::frame::Video::new(Pixel::P010LE, 64, 32);
        for row in 0..32usize {
            let stride = p010.stride(0);
            for x in 0..64 {
                let value = (235u16 << 8).to_ne_bytes();
                p010.data_mut(0)[row * stride + x * 2..row * stride + x * 2 + 2]
                    .copy_from_slice(&value);
            }
        }
        let stride = p010.stride(1);
        for row in 0..16usize {
            for x in 0..64 {
                let value = (128u16 << 8).to_ne_bytes();
                p010.data_mut(1)[row * stride + x * 2..row * stride + x * 2 + 2]
                    .copy_from_slice(&value);
            }
        }
        let planes = p010_planes(&p010);
        assert_eq!(planes.len(), 2, "P010 落成 NV12 只有 Y + 交织 UV");
        assert_eq!(planes[0].data.len(), 64 * 32);
        assert!(planes[0].data.iter().all(|&v| v == 235));
        assert_eq!(planes[1].stride, 64, "UV 每行 32 像素 × 2 分量 = 64 字节");
        assert_eq!(planes[1].data.len(), 64 * 16);
        assert!(planes[1].data.iter().all(|&v| v == 128));
    }

    /// 看护窗口：预热帧不计账；窗口内忙时超过内容时长 1.05 倍才回退，且只回退一次
    #[test]
    fn fallback_watch_needs_sustained_slowdown() {
        let mut watch = FallbackWatch::new(Some(ffmpeg_next::codec::Parameters::new()));
        // 40ms/帧 = 25fps；预热期怎么慢都不算
        for frame in 0..=FALLBACK_WARMUP_FRAMES {
            watch.busy += 0.100;
            assert!(!watch.note_frame(Some(frame as f64 * 0.04)));
        }
        // 窗口 50 帧 ≈ 2s：0.100s/帧忙时 → 5s ≫ 2.1s，触发
        let mut triggered = false;
        for frame in FALLBACK_WARMUP_FRAMES + 1..=FALLBACK_WARMUP_FRAMES + 60 {
            watch.busy += 0.100;
            if watch.note_frame(Some(frame as f64 * 0.04)) {
                triggered = true;
                break;
            }
        }
        assert!(triggered, "持续 2.5× 超预算必须触发回退");
        assert!(!watch.note_frame(Some(99.0)), "回退只触发一次");

        // 忙时 50% 预算：一直不触发
        let mut watch = FallbackWatch::new(Some(ffmpeg_next::codec::Parameters::new()));
        for frame in 0..400 {
            watch.busy += 0.020;
            assert!(!watch.note_frame(Some(frame as f64 * 0.04)));
        }

        // 没有流参数（软解 / 显式设备）就不回退
        let mut watch = FallbackWatch::new(None);
        for frame in 0..400 {
            watch.busy += 0.100;
            assert!(!watch.note_frame(Some(frame as f64 * 0.04)));
        }
    }

    /// 编码一段 mpeg4（gop=25、52 帧 → 三个关键帧），供回退路径回归用
    fn mpeg4_test_packets() -> Vec<Packet> {
        fn drain(
            encoder: &mut ffmpeg_next::codec::encoder::video::Encoder,
            packets: &mut Vec<Packet>,
        ) {
            loop {
                let mut pkt = ffmpeg_next::Packet::empty();
                match encoder.receive_packet(&mut pkt) {
                    Ok(()) => packets.push(Packet {
                        data: pkt.data().unwrap_or_default().to_vec(),
                        stream_index: 0,
                        pts: pkt.pts(),
                        dts: pkt.dts(),
                        duration: None,
                        time_base: ffmpeg_next::Rational::new(1, 25),
                        is_key: pkt.is_key(),
                    }),
                    Err(e) if is_eagain(&e) => break,
                    Err(ffmpeg_next::Error::Eof) => break,
                    Err(e) => panic!("receive_packet: {e}"),
                }
            }
        }

        let codec = ffmpeg_next::codec::encoder::find_by_name("mpeg4").expect("mpeg4 encoder");
        let mut ctx = ffmpeg_next::codec::Context::new_with_codec(codec);
        ctx.set_time_base(ffmpeg_next::Rational::new(1, 25));
        let mut builder = ctx.encoder().video().expect("video encoder builder");
        builder.set_width(64);
        builder.set_height(64);
        builder.set_format(ffmpeg_next::format::Pixel::YUV420P);
        builder.set_gop(25);
        let mut encoder = builder.open().expect("open mpeg4 encoder");

        let mut packets = Vec::new();
        for i in 0..52i64 {
            let mut frame =
                ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, 64, 64);
            frame.set_pts(Some(i));
            for (idx, byte) in frame.data_mut(0).iter_mut().enumerate() {
                *byte = ((idx as i64 + i * 7) % 256) as u8;
            }
            encoder.send_frame(&frame).expect("send_frame");
            drain(&mut encoder, &mut packets);
        }
        encoder.send_eof().expect("send_eof");
        drain(&mut encoder, &mut packets);
        packets
    }

    fn mpeg4_parameters() -> ffmpeg_next::codec::Parameters {
        let mut parameters = ffmpeg_next::codec::Parameters::new();
        unsafe {
            let p = parameters.as_mut_ptr();
            (*p).codec_type = ffmpeg_next::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*p).codec_id = ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_MPEG4;
            (*p).width = 64;
            (*p).height = 64;
            (*p).format = ffmpeg_next::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as _;
        }
        parameters
    }

    /// 帧线程有流水线延迟：喂包直到出一帧，返回下一个待喂包的下标
    fn feed_until_frame(
        decoder: &mut FfmpegVideoDecoder,
        packets: &[Packet],
        mut index: usize,
    ) -> usize {
        while index < packets.len() {
            decoder.submit_packet(&packets[index]).expect("submit");
            index += 1;
            match decoder.receive_frame() {
                Ok(Some(_)) => return index,
                Ok(None) => continue,
                Err(e) => panic!("receive_frame: {e}"),
            }
        }
        panic!("包喂完了也没出帧");
    }

    /// 决定回退后硬解继续出帧直到关键帧：非关键包不再被跳过（旧实现会把切换点
    /// 前的整段画面冻住），关键帧处换解码器并通知引擎
    #[test]
    fn fallback_keeps_frames_flowing_until_keyframe() {
        ffmpeg_next::init().expect("ffmpeg init");
        let packets = mpeg4_test_packets();
        let key_indices: Vec<usize> = packets
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_key)
            .map(|(i, _)| i)
            .collect();
        assert!(
            key_indices.len() >= 2,
            "需要两个关键帧，实际 {}",
            key_indices.len()
        );

        let parameters = mpeg4_parameters();
        let ctx = ffmpeg_next::codec::Context::from_parameters(parameters.clone()).expect("ctx");
        let mut decoder =
            FfmpegVideoDecoder::with_policy(ctx, Policy::SoftwareOnly).expect("software decoder");
        // 喂满帧线程流水线，此后一包进一帧出
        let next = feed_until_frame(&mut decoder, &packets, 0);

        // 模拟 Auto 硬解看护触发后的状态：挂上重建参数、等待关键帧
        decoder.fallback.parameters = Some(parameters);
        decoder.await_key = true;

        assert!(!packets[next].is_key, "预热不该跨过关键帧");
        decoder
            .submit_packet(&packets[next])
            .expect("submit non-key");
        assert!(
            matches!(decoder.receive_frame(), Ok(Some(_))),
            "等待回退期间非关键包必须照常出帧"
        );
        assert!(decoder.await_key, "关键帧之前不得换解码器");

        let key = key_indices[1];
        decoder
            .submit_packet(&packets[key])
            .expect("submit second key");
        assert!(!decoder.await_key, "关键帧处必须完成切换");
        let notice = decoder.take_notice().expect("回退必须通知引擎");
        assert_eq!(notice.code, "hardware_decode_too_slow");
        assert!(decoder.take_notice().is_none(), "通知只发一次");
        // 新解码器同样有帧线程延迟，喂到出帧为止
        feed_until_frame(&mut decoder, &packets, key + 1);
    }

    /// 重建失败保留原解码路径：通知只发一次，下一个关键帧重试
    #[test]
    fn fallback_failure_notifies_once_and_retries() {
        ffmpeg_next::init().expect("ffmpeg init");
        let packets = mpeg4_test_packets();
        let key_indices: Vec<usize> = packets
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_key)
            .map(|(i, _)| i)
            .collect();
        assert!(
            key_indices.len() >= 2,
            "需要两个关键帧，实际 {}",
            key_indices.len()
        );

        let ctx = ffmpeg_next::codec::Context::from_parameters(mpeg4_parameters()).expect("ctx");
        let mut decoder =
            FfmpegVideoDecoder::with_policy(ctx, Policy::SoftwareOnly).expect("software decoder");
        // 不挂重建参数：rebuild_software 必然失败
        decoder.await_key = true;

        decoder
            .submit_packet(&packets[key_indices[0]])
            .expect("submit first key");
        let notice = decoder.take_notice().expect("失败必须通知引擎");
        assert_eq!(notice.code, "hardware_decode_fallback_failed");
        assert!(decoder.await_key, "失败后应在下一个关键帧重试");

        let key = key_indices[1];
        decoder
            .submit_packet(&packets[key])
            .expect("submit second key");
        assert!(decoder.take_notice().is_none(), "失败通知只发一次");
        assert!(decoder.await_key);
        // 原解码路径不受重建失败影响
        feed_until_frame(&mut decoder, &packets, key + 1);
    }
}
