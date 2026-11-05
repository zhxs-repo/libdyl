//! 硬件解码 v1：设备选择、`get_format` 接入、GPU→CPU 回传
//!
//! 决策与实测依据在 [`docs/硬件解码方案.md`](../../../docs/硬件解码方案.md)：v1 只做
//! "hwaccel 解码 + `av_hwframe_transfer_data` 回传 CPU"，渲染通路仍然吃 CPU YUV，
//! `OutputSlot`、`FrameCompositor` 与契约都不动。
//!
//! 本机这套 FFmpeg 到底能硬解什么**不要猜**：先跑
//! `cargo run -p dyl-ffmpeg --example hwaccel_probe`。

use std::ffi::CStr;
use std::os::raw::c_int;
use std::ptr::NonNull;

use dyl_core::{DylError, Result};
use ffmpeg_next::ffi::{
    self, AVCodecContext, AVCodecID, AVHWDeviceContext, AVHWDeviceType, AVHWFramesContext,
    AVPixelFormat,
};
use ffmpeg_next::format::Pixel;
use ffmpeg_next::frame::Video;

/// 硬解开关的环境变量名，取值见 [`Policy::parse`]，缺省为关。
pub const HW_DECODE_ENV: &str = "DYL_HW_DECODE";

/// 本内核认得的设备名。两个名字表外：
/// - `mediacodec`：它的 `hwcontext_mediacodec` 既没实现 `transfer_data` 也没实现
///   `map`，GPU 帧无路可回传（方案第 3 节），所以 `Auto` 也不会去枚举它；
/// - `v4l2` / `v4l2m2m`：本地 7.1 头文件的公开 `enum AVHWDeviceType` 里没有这两条
///   （`D:\Devs\vcpkg\...\libavutil\hwcontext.h` 实测），名字解析不出来也就无从验证，
///   v1 不承诺没跑过的设备。真机上若某构建报出这个名字，`listed_device_names_round_trip`
///   与 `auto_candidates_follow_preference_and_skip_unlisted` 会把它挡在候选外。
const DEVICE_NAMES: &[&CStr] = &[
    c"d3d11va",
    c"d3d12va",
    c"dxva2",
    c"vaapi",
    c"vdpau",
    c"videotoolbox",
    c"cuda",
    c"vulkan",
];

/// `Policy::Auto` 的平台首选顺序。依据是探针报告的可建性与回传成熟度：
/// Windows 上 d3d11va 的回传路径最成熟，dxva2 连帧约束都查不出来（本机返回 NULL）。
#[cfg(target_os = "windows")]
const PREFERRED: &[&CStr] = &[c"d3d11va", c"d3d12va", c"dxva2"];
#[cfg(target_os = "macos")]
const PREFERRED: &[&CStr] = &[c"videotoolbox"];
/// Android 的 v1 不硬解：见 [`DEVICE_NAMES`] 的说明
#[cfg(target_os = "android")]
const PREFERRED: &[&CStr] = &[];
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "android")))]
const PREFERRED: &[&CStr] = &[c"vaapi", c"vdpau", c"cuda"];

/// `get_format` 的候选列表长度上限。列表本身以 `AV_PIX_FMT_NONE` 结尾，这里只是
/// 回调里不动态分配所需的固定窗口（实测最长 4 项）。
const MAX_FORMATS: usize = 16;

/// 要不要给这一路视频上硬解
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// 只用软解（今天的默认）
    SoftwareOnly,
    /// 按 [`PREFERRED`] 顺序，再按 FFmpeg 自己的枚举顺序，取第一个
    /// "该编解码器在这个设备上有硬件配置**且设备真建得出来**"的设备
    Auto,
    /// 只试这一个设备；建不出来仍然软解
    Device(AVHWDeviceType),
}

impl Policy {
    /// 读 `DYL_HW_DECODE`
    pub fn from_env() -> Self {
        match std::env::var(HW_DECODE_ENV) {
            Ok(value) => Self::parse(&value),
            Err(_) => Self::SoftwareOnly,
        }
    }

    /// `0`/`off`/`no`/空 → 关；`1`/`auto`/`on`/`yes` → [`Policy::Auto`]；
    /// 设备名 → 只试它。名字不认识时退回关并 `warn`：拼错一个字母就悄悄启用硬解，
    /// 比拼错一个字母什么都不发生更难查。
    pub fn parse(value: &str) -> Self {
        let value = value.trim().to_ascii_lowercase();
        match value.as_str() {
            "" | "0" | "off" | "no" => Self::SoftwareOnly,
            "1" | "auto" | "on" | "yes" => Self::Auto,
            other => match type_by_name(other) {
                Some(device_type) => Self::Device(device_type),
                None => {
                    tracing::warn!(
                        "{HW_DECODE_ENV}={other} 不是认得的设备名（{}），按软解处理",
                        device_list()
                    );
                    Self::SoftwareOnly
                }
            },
        }
    }
}

/// 一台真的建出来的 `AVHWDeviceContext`，本结构持有一份引用计数
pub struct HwDevice {
    buffer: NonNull<ffi::AVBufferRef>,
    device_type: AVHWDeviceType,
}

// 随所有者（解码器）移动到某一个线程独占使用，之后不存在并发——与 `SwsScaler`
// （decoder.rs）同一理由。`AVBufferRef` 的引用计数本身是原子的。
unsafe impl Send for HwDevice {}

impl HwDevice {
    /// 打开该类型的默认设备。探针实测：建不出来就是这台机器/驱动没有，换下一个，
    /// 不要重试（方案 5.3：一次会话内不反复试硬件）。
    fn open(device_type: AVHWDeviceType) -> Option<Self> {
        let mut buffer: *mut ffi::AVBufferRef = std::ptr::null_mut();
        let code = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut buffer,
                device_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if code < 0 || buffer.is_null() {
            tracing::debug!(
                "hwaccel: {} 建不出设备上下文 {}",
                type_name(device_type),
                strerror(code)
            );
            return None;
        }
        NonNull::new(buffer).map(|buffer| Self {
            buffer,
            device_type,
        })
    }

    /// 交给 `avctx->hw_device_ctx` 的一份新引用。设过一次之后就归 libavcodec 释放
    /// （`avcodec_free_context` 会 unref），本结构仍持有自己那一份。
    fn reference(&self) -> Option<*mut ffi::AVBufferRef> {
        let pointer = unsafe { ffi::av_buffer_ref(self.buffer.as_ptr()) };
        (!pointer.is_null()).then_some(pointer)
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        let mut pointer = self.buffer.as_ptr();
        unsafe { ffi::av_buffer_unref(&mut pointer) };
    }
}

/// 一路视频的硬解方案：设备 + 解码输出格式 + 回传目标格式
pub struct HwPlan {
    device: HwDevice,
    /// 解码器将产出的 GPU 驻留格式（`d3d11`/`vaapi`/…）
    hw_format: Pixel,
    /// 回传目标的**预测值**：从设备帧约束的 `valid_sw_formats` 里挑。
    /// 权威值是每个帧池的 `sw_format`，见 [`pool_sw_format`]——8-bit 池通常是 NV12、
    /// 10-bit 池是 P010LE，而 `get_format` 要到第一次解帧才被调用，那时才有帧池。
    cpu_format: Pixel,
    /// 该编解码器在这个设备上只有 `INTERNAL` 配置（没有可显式启用的那条）
    internal_only: bool,
}

impl HwPlan {
    pub fn cpu_format(&self) -> Pixel {
        self.cpu_format
    }

    /// 实际采用的设备名（`d3d11va`/`vaapi`/…），快照的 `decode.device` 用它
    pub fn device_name(&self) -> String {
        type_name(self.device.device_type)
    }

    /// 日志用的一句话：`设备 → 解码输出 → 回传目标`
    ///
    /// 回传目标写的是**预测值**（本机 1080p HEVC Main10 实测：设备约束把 nv12 排在
    /// 首位，而 10-bit 的帧池实际是 p010le）。真正回传时以帧池 [`pool_sw_format`]
    /// 为准，所以这句只当"我们打算怎么走"，不当"实际走了哪一步"。
    pub fn describe(&self) -> String {
        format!(
            "{}{} → {} → {}（预测）",
            type_name(self.device.device_type),
            if self.internal_only {
                "（内部硬解）"
            } else {
                ""
            },
            pixel_name(self.hw_format),
            pixel_name(self.cpu_format)
        )
    }
}

/// 该不该给这个编解码器上硬解。`None` 就是软解，本会话内不重试。
///
/// 三步都和探针一样：编解码器的 `avcodec_get_hw_config` 里必须有一条设备类型匹配的
/// 配置；然后**真的**建一次设备；最后从设备帧约束里挑回传目标。
///
/// 有两条配置都算数，但接法不同（Linux 实测才分清）：
/// - `HW_DEVICE_CTX`＝"可显式启用"，这是 FFmpeg 公开的同一条通路，装 `hw_device_ctx`
///   就够，不用抢 `get_format`；
/// - 只有 `INTERNAL`＝FFmpeg 8 把原来 `AD_HOC` 的"解码器自己换成 GPU 格式"改成了
///   "必须解码器内部的 `get_format` 选中才生效"。`hwaccel_device` 参数在
///   `AVOptionsCodec.hw_device_setup` 里设 `hw_device_ctx`（`options.c`），而
///   `avctx->hw_device_ctx` 非空会让 `avcodec_default_get_format` 停止尝试 INTERNAL
///   （`decode.c`），所以我们必须自己实现 `get_format`，且**只**认 INTERNAL 条目——
///   连 AD_HOC 都不认（方案 5.1 要拦的就是它）。
pub fn select(codec: AVCodecID, policy: Policy) -> Option<HwPlan> {
    let device_types = match policy {
        Policy::SoftwareOnly => return None,
        Policy::Auto => candidate_types(),
        Policy::Device(device_type) => vec![device_type],
    };
    // 先查配置再开设备：为用不上的设备建一次 D3D 上下文是纯浪费
    let codec_ptr = unsafe { ffi::avcodec_find_decoder(codec) };
    if codec_ptr.is_null() {
        return None;
    }
    for device_type in device_types {
        let Some((hw_format, explicit)) = hardware_config(codec_ptr, device_type) else {
            continue;
        };
        let Some(device) = HwDevice::open(device_type) else {
            continue;
        };
        let plan = HwPlan {
            cpu_format: cpu_target(&device),
            device,
            hw_format,
            internal_only: !explicit,
        };
        tracing::info!("hwaccel: 采用 {}", plan.describe());
        return Some(plan);
    }
    tracing::debug!(
        "hwaccel: 策略 {policy:?} 下没有可用设备（编解码器 {}）",
        codec_name(codec)
    );
    None
}

/// 把方案装进解码上下文。
///
/// # Safety
///
/// `ctx` 必须是活的 `AVCodecContext`（本项目里是调用方独占的
/// `Context::as_mut_ptr()`），且这次调用**早于** `avcodec_open2()`：
/// `hw_device_ctx` 与 `get_format` 都在 open 时被读取，open 之后再改这两个字段
/// 是未定义行为，类型系统表达不了这个先后，所以由调用方保证。
///
/// `get_format` 只在"这个设备只能靠解码器内部启用"时装（见 [`select`]）：有
/// `HW_DEVICE_CTX` 配置时 FFmpeg 自己的选择就是我们想要的，多装一层反而会把
/// INTERNAL 候选一起吸进来。
pub unsafe fn install(ctx: *mut AVCodecContext, plan: &HwPlan) -> Result<()> {
    let reference = plan
        .device
        .reference()
        .ok_or_else(|| DylError::Decode("hwaccel: av_buffer_ref(hw_device_ctx) 失败".into()))?;
    // SAFETY: 见本函数的 `# Safety`——非空、独占、且在 open 之前
    unsafe {
        (*ctx).hw_device_ctx = reference;
        if plan.internal_only {
            (*ctx).get_format = Some(choose_format);
        }
    }
    Ok(())
}

/// `AVCodecContext.get_format` 的实现（仅 `HwPlan::internal_only` 时装上）
///
/// 不借 `avctx->opaque` 传方案：那个字段自 FFmpeg 7 起归库内部使用。想要的硬件格式
/// 本来就能现算——设备类型从 `avctx->hw_device_ctx` 反查，编解码器从 `avctx->codec`
/// 拿（那是公开字段），再与 FFmpeg 给的候选列表取交集。候选里没有匹配的 INTERNAL
/// 条目就交回 `avcodec_default_get_format`：那即"这台机器给不了硬解"的软解回退，
/// 不需要额外的错误通道（方案 5.5 第二行）。
///
/// 只在 open 前后被调用，跑在视频解码线程上；FFmpeg 保证同一时刻只有一个线程进来。
unsafe extern "C" fn choose_format(
    ctx: *mut AVCodecContext,
    fmt: *const AVPixelFormat,
) -> AVPixelFormat {
    let device_type = device_type_of(ctx);
    if device_type == AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
        // 没有设备上下文——装 `get_format` 时是一起装的，所以这只在 FFmpeg 之后
        // 把 `hw_device_ctx` 换成别的（例如内部重建）时出现。交回默认选择
        return unsafe { ffi::avcodec_default_get_format(ctx, fmt) };
    }
    let mut supported = [AVPixelFormat::AV_PIX_FMT_NONE; MAX_FORMATS];
    let supported_count = collect_internal(ctx, device_type, &mut supported);
    let mut candidates = [AVPixelFormat::AV_PIX_FMT_NONE; MAX_FORMATS];
    let candidate_count = collect_formats(fmt, &mut candidates);
    let chosen = choose_among(
        &supported[..supported_count],
        &candidates[..candidate_count],
    );
    match chosen {
        Some(format) => {
            tracing::info!(
                "hwaccel: get_format 选中 {}（候选 {} 项，设备 {}）",
                pixel_name(Pixel::from(format)),
                candidate_count,
                type_name(device_type)
            );
            format
        }
        None => {
            // 候选列表里没有本方案的硬件格式：可能是这套构建没编进该 hwaccel，
            // 也可能是解码器这一配置只支持软解。落回软解，不报错
            tracing::warn!(
                "hwaccel: 候选 [{}] 里没有 {} 的硬件格式，退回软解",
                format_list(&candidates[..candidate_count]),
                type_name(device_type)
            );
            ffi::avcodec_default_get_format(ctx, fmt)
        }
    }
}

/// 从 `avctx->hw_device_ctx` 反查设备类型；没有设备上下文时返回 `NONE`
fn device_type_of(ctx: *mut AVCodecContext) -> AVHWDeviceType {
    if ctx.is_null() {
        return AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
    }
    // SAFETY: ctx 非空且 `hw_device_ctx->data` 指向 `AVHWDeviceContext`
    unsafe {
        let reference = (*ctx).hw_device_ctx;
        if reference.is_null() {
            return AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
        }
        (*((*reference).data as *const AVHWDeviceContext)).type_
    }
}

/// 本设备在该编解码器上"只能由解码器内部启用"的格式，按 `avcodec_get_hw_config`
/// 的索引顺序
///
/// 只收 `AV_CODEC_HW_CONFIG_METHOD_INTERNAL`。`AD_HOC` 一律排除：那条通路不需要
/// 设备上下文也会换 GPU 格式，正是方案 5.1 要拦的东西；把它放进白名单等于让
/// 我们自己写的 `get_format` 主动去选它。
unsafe fn collect_internal(
    ctx: *mut AVCodecContext,
    device_type: AVHWDeviceType,
    out: &mut [AVPixelFormat],
) -> usize {
    let mut count = 0;
    let mut index = 0;
    while count < out.len() {
        let config = unsafe { ffi::avcodec_get_hw_config((*ctx).codec, index) };
        if config.is_null() {
            break;
        }
        index += 1;
        let methods = unsafe { (*config).methods };
        let internal = methods & ffi::AV_CODEC_HW_CONFIG_METHOD_INTERNAL as c_int != 0;
        if !internal || unsafe { (*config).device_type } != device_type {
            continue;
        }
        out[count] = unsafe { (*config).pix_fmt };
        count += 1;
    }
    count
}

/// 把以 `AV_PIX_FMT_NONE` 结尾的候选列表抄进固定数组（回调里不动态分配）
unsafe fn collect_formats(list: *const AVPixelFormat, out: &mut [AVPixelFormat]) -> usize {
    let mut count = 0;
    while count < out.len() {
        let format = unsafe { *list.add(count) };
        if format == AVPixelFormat::AV_PIX_FMT_NONE {
            break;
        }
        out[count] = format;
        count += 1;
    }
    count
}

/// 按**候选列表的顺序**取第一个本设备支持的格式
///
/// 顺序交给 FFmpeg 是有意的：候选列表本身就表达"这一配置下解码器能接受的格式"，
/// 而我们要的只是"别让我不知道地用上 GPU 格式"。纯函数，便于脱离真机回归。
fn choose_among(
    supported: &[AVPixelFormat],
    candidates: &[AVPixelFormat],
) -> Option<AVPixelFormat> {
    candidates
        .iter()
        .find(|candidate| supported.contains(candidate))
        .copied()
}

/// 向设备问一次帧约束，挑出回传目标（挑选规则见 [`pick_sw_target`]）
///
/// 这只是 `HwPlan.cpu_format` 的**预测值**；权威值是每个帧池的 `sw_format`，
/// 见 [`pool_sw_format`]——`get_format` 要到第一次解帧才被调用，那时才有帧池。
fn cpu_target(device: &HwDevice) -> Pixel {
    let constraints = unsafe {
        ffi::av_hwdevice_get_hwframe_constraints(device.buffer.as_ptr(), std::ptr::null())
    };
    if constraints.is_null() {
        return Pixel::from(pick_sw_target(&[]));
    }
    let mut formats = [AVPixelFormat::AV_PIX_FMT_NONE; MAX_FORMATS];
    let count = unsafe {
        collect_formats(
            (*constraints).valid_sw_formats as *const AVPixelFormat,
            &mut formats,
        )
    };
    let chosen = pick_sw_target(&formats[..count]);
    // SAFETY: constraints 由 av_hwdevice_get_hwframe_constraints 返回，必须显式释放
    unsafe {
        let mut raw = constraints;
        ffi::av_hwframe_constraints_free(&mut raw);
    }
    Pixel::from(chosen)
}

/// 从设备帧约束给的软件格式清单里挑回传目标
///
/// 清单表达"这个设备能回传成什么"，我们的偏好是 NV12（渲染通路原生支持）>
/// P010LE（10-bit，走既有 swscale 通路）> YUV420P；都不在清单里才退回清单首项。
/// 空清单（查不到约束，本机 dxva2 就是这样）按 NV12 猜——8-bit 池几乎都是它。
fn pick_sw_target(available: &[AVPixelFormat]) -> AVPixelFormat {
    const PREFERENCES: [AVPixelFormat; 3] = [
        AVPixelFormat::AV_PIX_FMT_NV12,
        AVPixelFormat::AV_PIX_FMT_P010LE,
        AVPixelFormat::AV_PIX_FMT_YUV420P,
    ];
    PREFERENCES
        .iter()
        .find(|wanted| available.contains(wanted))
        .copied()
        .or_else(|| available.first().copied())
        .unwrap_or(AVPixelFormat::AV_PIX_FMT_NV12)
}

/// 编解码器在该设备上的硬件配置：`(GPU 输出格式, 有没有可显式启用的那条)`
///
/// `None`＝这个设备对这个编解码器什么都没有。`Some((_, true))`＝有
/// `HW_DEVICE_CTX`，装 `hw_device_ctx` 就够。`Some((_, false))`＝只有
/// `INTERNAL`，得靠 [`choose_format`] 选中才生效（见 [`select`] 的两条接法）。
fn hardware_config(
    codec: *const ffi::AVCodec,
    device_type: AVHWDeviceType,
) -> Option<(Pixel, bool)> {
    let mut internal_only = None;
    let mut index = 0;
    loop {
        let config = unsafe { ffi::avcodec_get_hw_config(codec, index) };
        if config.is_null() {
            return internal_only;
        }
        index += 1;
        // SAFETY: config 由 avcodec_get_hw_config 返回，在该解码器注册期间一直有效
        let (methods, config_device, pix_fmt) =
            unsafe { ((*config).methods, (*config).device_type, (*config).pix_fmt) };
        if config_device != device_type || pix_fmt == AVPixelFormat::AV_PIX_FMT_NONE {
            continue;
        }
        let explicit = methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int != 0;
        if explicit {
            return Some((Pixel::from(pix_fmt), true));
        }
        // AD_HOC 单独存在时不算数：它不需要设备上下文就会换 GPU 格式
        if methods & ffi::AV_CODEC_HW_CONFIG_METHOD_INTERNAL as c_int != 0
            && internal_only.is_none()
        {
            internal_only = Some((Pixel::from(pix_fmt), false));
        }
    }
}

/// `Policy::Auto` 的尝试顺序：平台首选在前，FFmpeg 的枚举顺序在后，
/// 且只留 [`DEVICE_NAMES`] 认识的（`mediacodec` 因此被排除）。
fn candidate_types() -> Vec<AVHWDeviceType> {
    let mut types: Vec<AVHWDeviceType> = Vec::new();
    let mut push = |device_type: AVHWDeviceType| {
        if device_type != AVHWDeviceType::AV_HWDEVICE_TYPE_NONE && !types.contains(&device_type) {
            types.push(device_type);
        }
    };
    for name in PREFERRED {
        push(type_by_cstr(name));
    }
    let mut previous = AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
    loop {
        let device_type = unsafe { ffi::av_hwdevice_iterate_types(previous) };
        if device_type == AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            break;
        }
        previous = device_type;
        if known_type(device_type) {
            push(device_type);
        }
    }
    types
}

/// 名字 → 设备类型，只认 [`DEVICE_NAMES`]
///
/// 表外一律当不认识：`mediacodec` 在 FFmpeg 的名字表里查得到，但它的 hwaccel 既没
/// 实现 `transfer_data` 也没实现 `map`，`DYL_HW_DECODE=mediacodec` 只会换回一条
/// 回传失败的 `decode` 错误——与其让拼错的人拿到一个注定失败的方案，
/// 不如按"没开"处理并把原因写在 warn 里。
fn type_by_name(name: &str) -> Option<AVHWDeviceType> {
    DEVICE_NAMES
        .iter()
        .find(|device_name| device_name.to_str().ok() == Some(name))
        .and_then(|device_name| known(type_by_cstr(device_name)))
}

fn type_by_cstr(name: &CStr) -> AVHWDeviceType {
    unsafe { ffi::av_hwdevice_find_type_by_name(name.as_ptr()) }
}

fn known(device_type: AVHWDeviceType) -> Option<AVHWDeviceType> {
    (device_type != AVHWDeviceType::AV_HWDEVICE_TYPE_NONE).then_some(device_type)
}

fn known_type(device_type: AVHWDeviceType) -> bool {
    DEVICE_NAMES
        .iter()
        .any(|name| *name == type_name_cstr(device_type))
}

fn type_name_cstr(device_type: AVHWDeviceType) -> &'static CStr {
    let ptr = unsafe { ffi::av_hwdevice_get_type_name(device_type) };
    if ptr.is_null() {
        // 只会在 NONE 或未知枚举值上发生，而这两者都进不到 [`DEVICE_NAMES`] 的比较里
        return c"(null)";
    }
    unsafe { CStr::from_ptr(ptr) }
}

fn type_name(device_type: AVHWDeviceType) -> String {
    type_name_cstr(device_type).to_string_lossy().into_owned()
}

fn device_list() -> String {
    DEVICE_NAMES
        .iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("|")
}

/// 像素格式名（`av_get_pix_fmt_name`），日志与错误消息用
pub fn pixel_name(pixel: Pixel) -> String {
    let raw = AVPixelFormat::from(pixel);
    let ptr = unsafe { ffi::av_get_pix_fmt_name(raw) };
    if ptr.is_null() {
        return format!("#{raw:?}");
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn format_list(formats: &[AVPixelFormat]) -> String {
    formats
        .iter()
        .map(|format| pixel_name(Pixel::from(*format)))
        .collect::<Vec<_>>()
        .join(",")
}

fn codec_name(codec: AVCodecID) -> String {
    let ptr = unsafe { ffi::avcodec_get_name(codec) };
    if ptr.is_null() {
        return "unknown".into();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn strerror(code: c_int) -> String {
    let mut buffer = [0u8; 256];
    unsafe { ffi::av_strerror(code, buffer.as_mut_ptr() as *mut _, buffer.len()) };
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let text = String::from_utf8_lossy(&buffer[..end]).into_owned();
    if text.is_empty() {
        format!("(码 {code})")
    } else {
        format!("({text})")
    }
}

/// 这个 GPU 帧的帧池实际是什么布局（`sw_format`）——回传目标的权威值
///
/// [`HwPlan::cpu_format`] 是 open 时的预测，而 10-bit 视频的池是 P010LE、
/// 8-bit 是 NV12；只有帧上的 `hw_frames_ctx` 说得准。
pub fn pool_sw_format(frame: &Video) -> Option<Pixel> {
    let frames = frames_context(frame)?;
    // SAFETY: frames 由解码器挂在帧上，随帧有效
    let format = unsafe { (*frames).sw_format };
    (format != AVPixelFormat::AV_PIX_FMT_NONE).then(|| Pixel::from(format))
}

fn frames_context(frame: &Video) -> Option<*const AVHWFramesContext> {
    // SAFETY: frame 是活的 `AVFrame`
    let reference = unsafe { (*frame.as_ptr()).hw_frames_ctx };
    if reference.is_null() {
        return None;
    }
    // SAFETY: `hw_frames_ctx` 的 `data` 指向 `AVHWFramesContext`
    Some(unsafe { (*reference).data as *const AVHWFramesContext })
}

/// 备好可复用的 CPU 回传帧：格式与尺寸没变就沿用现有缓冲
///
/// 1080p NV12 一帧约 3 MB，每帧重新分配没必要；`av_hwframe_transfer_data` 允许对
/// 已分配的 `dst` 直接写。尺寸用**显示尺寸**，与"传一个子矩形"不同：hwcontext.h
/// 只要求 `dst` 不超过帧池的分配尺寸，而 `AVFrame` 没有独立的分配尺寸字段，
/// 所以补齐区（例如 1080p 的 1088 行）根本不进我们的缓冲，也就读不到未定义内容。
pub fn prepare_cpu_frame(frame: &mut Video, target: Pixel, width: u32, height: u32) -> Result<()> {
    let ready = frame.format() == target && frame.width() == width && frame.height() == height;
    // SAFETY: frame 是本解码器拥有的空/活 `AVFrame`
    let allocated = ready && !unsafe { (*frame.as_ptr()).data[0] }.is_null();
    if allocated {
        return Ok(());
    }
    unsafe { ffi::av_frame_unref(frame.as_mut_ptr()) };
    frame.set_format(target);
    frame.set_width(width);
    frame.set_height(height);
    let code = unsafe { ffi::av_frame_get_buffer(frame.as_mut_ptr(), 32) };
    if code < 0 {
        return Err(DylError::Decode(format!(
            "hwaccel: 回传缓冲分配失败 {}x{} {}: {}",
            width,
            height,
            pixel_name(target),
            strerror(code)
        )));
    }
    tracing::debug!(
        "hwaccel: 回传缓冲就绪 {}x{} {}",
        width,
        height,
        pixel_name(target)
    );
    Ok(())
}

/// GPU 帧 → CPU 帧
///
/// `dst` 必须已由 [`prepare_cpu_frame`] 按目标格式备好。属性搬运（PTS/色彩）在
/// hwcontext.h 里没写清，所以调用方**只从 GPU 帧取 PTS 与色彩元数据**，不依赖这里。
pub fn transfer(dst: &mut Video, src: &Video) -> Result<()> {
    let code = unsafe { ffi::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0) };
    if code < 0 {
        return Err(DylError::Decode(format!(
            "av_hwframe_transfer_data: {}",
            strerror(code)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::types::is_hw_pixel;

    use super::*;

    /// 开关只认表内、且本构建认识的设备：`mediacodec` 在 FFmpeg 的名字表里查得到，
    /// 但它的 GPU 帧无路可回传，允许它等于给用户一条必然失败的 `decode` 错误。
    /// 表内但本构建没编进来的名字（Windows 上的 `v4l2`）同样按"没开"处理——
    /// 这时 `DYL_HW_DECODE` 的意图无法执行，报软解是对的，只是不能悄悄当成启用成功。
    #[test]
    fn env_switch_maps_to_policy() {
        ffmpeg_next::init().expect("ffmpeg init");
        for value in [
            "",
            "0",
            "off",
            "no",
            " NO ",
            "mediacodec",
            "d3d11",
            "v4l2",
            "v4l2m2m",
            "bogus",
        ] {
            assert_eq!(
                Policy::parse(value),
                Policy::SoftwareOnly,
                "{value} 应当按软解处理"
            );
        }
        for value in ["1", "auto", "on", "yes", " AUTO "] {
            assert_eq!(Policy::parse(value), Policy::Auto, "{value} 应当启用自动");
        }
        for name in DEVICE_NAMES {
            let value = name.to_string_lossy().into_owned();
            let in_this_build = type_by_cstr(name) != AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
            if in_this_build {
                assert!(
                    matches!(Policy::parse(&value), Policy::Device(_)),
                    "{value} 是表内设备名且本构建认识，应当解析成指定设备"
                );
            } else {
                assert_eq!(Policy::parse(&value), Policy::SoftwareOnly, "{value}");
            }
        }
        // `d3d11` 是像素格式名（`AV_PIX_FMT_D3D11`），设备名是 `d3d11va`；
        // 只写一半不能启用硬解，也不能被读成"启用了某个别的设备"
        assert_eq!(Policy::parse("d3d11"), Policy::SoftwareOnly);
    }

    /// 表内设备名必须能原样往返：`known_type` 用 `av_hwdevice_get_type_name` 的返回值
    /// 与表比对，名字对不上（例如别名解析成另一条）就会让 `Auto` 静默漏掉该设备
    #[test]
    fn listed_device_names_round_trip() {
        ffmpeg_next::init().expect("ffmpeg init");
        for name in DEVICE_NAMES {
            let device_type = type_by_cstr(name);
            if device_type == AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
                continue; // 本构建没编这个 hwaccel
            }
            assert_eq!(
                type_name_cstr(device_type),
                *name,
                "设备名表与 av_hwdevice_get_type_name 不一致"
            );
            assert!(known_type(device_type), "{name:?} 解析出来了却不被表承认");
        }
    }

    /// `Auto` 的枚举顺序与去重：平台首选在前，且只留 [`DEVICE_NAMES`]——
    /// 表外的设备（`mediacodec`、`v4l2m2m`）不得混进来
    #[test]
    fn auto_candidates_follow_preference_and_skip_unlisted() {
        ffmpeg_next::init().expect("ffmpeg init");
        let types = candidate_types();
        assert!(!types.is_empty(), "FFmpeg 至少要报告一种设备类型");
        assert!(
            !types.contains(&AVHWDeviceType::AV_HWDEVICE_TYPE_NONE),
            "NONE 不该出现在候选里"
        );
        let names: Vec<String> = types
            .iter()
            .map(|device_type| type_name(*device_type))
            .collect();
        for (index, name) in names.iter().enumerate() {
            assert!(
                DEVICE_NAMES
                    .iter()
                    .any(|known| known.to_str() == Ok(name.as_str())),
                "{name} 不在可启用设备表里"
            );
            assert!(
                !names[index + 1..].contains(name),
                "{name} 在候选里出现了两次"
            );
        }
        #[cfg(target_os = "windows")]
        assert_eq!(names.first().map(String::as_str), Some("d3d11va"));
        #[cfg(target_os = "linux")]
        assert_eq!(names.first().map(String::as_str), Some("vaapi"));
        #[cfg(target_os = "macos")]
        assert_eq!(names.first().map(String::as_str), Some("videotoolbox"));
    }

    /// `get_format` 的选择以**候选列表的顺序**为准——那是解码器自己报的可接受顺序
    #[test]
    fn get_format_keeps_candidate_order() {
        let supported = [
            AVPixelFormat::AV_PIX_FMT_VAAPI,
            AVPixelFormat::AV_PIX_FMT_D3D11,
        ];
        assert_eq!(
            choose_among(
                &supported,
                &[
                    AVPixelFormat::AV_PIX_FMT_YUV420P,
                    AVPixelFormat::AV_PIX_FMT_D3D11
                ]
            ),
            Some(AVPixelFormat::AV_PIX_FMT_D3D11),
            "候选里的硬件格式必须被选中"
        );
        assert_eq!(
            choose_among(&supported, &[AVPixelFormat::AV_PIX_FMT_NV12]),
            None,
            "候选里没有硬件格式时不选，让默认实现落回软解"
        );
    }

    /// 回传目标：NV12 最省（渲染通路原生支持），10-bit 走 P010LE，都不在清单里
    /// 才按设备给的顺序取第一个
    #[test]
    fn read_back_target_prefers_nv12() {
        let full = [
            AVPixelFormat::AV_PIX_FMT_YUV420P,
            AVPixelFormat::AV_PIX_FMT_NV12,
            AVPixelFormat::AV_PIX_FMT_P010LE,
        ];
        assert_eq!(pick_sw_target(&full), AVPixelFormat::AV_PIX_FMT_NV12);
        let ten_bit = [
            AVPixelFormat::AV_PIX_FMT_YUV420P10LE,
            AVPixelFormat::AV_PIX_FMT_P010LE,
        ];
        assert_eq!(pick_sw_target(&ten_bit), AVPixelFormat::AV_PIX_FMT_P010LE);
        let planar = [AVPixelFormat::AV_PIX_FMT_YUV420P];
        assert_eq!(pick_sw_target(&planar), AVPixelFormat::AV_PIX_FMT_YUV420P);
        let other = [AVPixelFormat::AV_PIX_FMT_BGRA];
        assert_eq!(pick_sw_target(&other), AVPixelFormat::AV_PIX_FMT_BGRA);
        // 查不到帧约束（本机 dxva2 就是这样）时按 NV12 猜
        assert_eq!(pick_sw_target(&[]), AVPixelFormat::AV_PIX_FMT_NV12);
    }

    /// 回传缓冲按解码器复用：布局不变就沿用同一块内存，变了才重分配。
    /// 这里不需要真设备——`av_frame_get_buffer` 对 CPU 格式同样工作
    #[test]
    fn read_back_buffer_is_reused_until_layout_changes() {
        ffmpeg_next::init().expect("ffmpeg init");
        let nv12 = Pixel::from(AVPixelFormat::AV_PIX_FMT_NV12);
        let mut frame = Video::empty();
        prepare_cpu_frame(&mut frame, nv12, 64, 48).expect("first allocation");
        let first = buffer_ref(&frame);
        assert!(!first.is_null(), "缓冲必须已分配");
        let first_size = buffer_size(&frame);

        prepare_cpu_frame(&mut frame, nv12, 64, 48).expect("reuse must not fail");
        assert_eq!(
            buffer_ref(&frame),
            first,
            "同一布局不该重新分配（1080p NV12 每帧约 3 MB，每帧分配一次没必要）"
        );

        prepare_cpu_frame(&mut frame, nv12, 128, 96).expect("resize must re-allocate");
        assert!(
            buffer_size(&frame) > first_size,
            "尺寸变大必须换一块更大的缓冲，否则回传会越界写"
        );
        assert_eq!(frame.width(), 128);
        assert_eq!(frame.height(), 96);

        // 换格式同样要重分配：NV12 的两平面布局与 YUV420P 的三平面不通用
        prepare_cpu_frame(
            &mut frame,
            Pixel::from(AVPixelFormat::AV_PIX_FMT_YUV420P),
            128,
            96,
        )
        .expect("format change must re-allocate");
        assert_eq!(
            frame.format(),
            Pixel::from(AVPixelFormat::AV_PIX_FMT_YUV420P)
        );
        assert!(!buffer_ref(&frame).is_null());
    }

    fn buffer_ref(frame: &Video) -> *const ffi::AVBufferRef {
        // SAFETY: frame 是本测试创建的活 `AVFrame`，`buf[0]` 是它自己的缓冲引用
        unsafe { (*frame.as_ptr()).buf[0] }
    }

    fn buffer_size(frame: &Video) -> usize {
        // SAFETY: 调用点保证 `buf[0]` 非空
        unsafe { (*(*frame.as_ptr()).buf[0]).size }
    }

    /// 没有帧池的帧问不出回传目标，也不是 GPU 帧
    #[test]
    fn plain_frame_has_no_pool_and_is_not_hardware() {
        ffmpeg_next::init().expect("ffmpeg init");
        let mut frame = Video::empty();
        frame.set_format(Pixel::from(AVPixelFormat::AV_PIX_FMT_YUV420P));
        assert!(pool_sw_format(&frame).is_none());
        assert!(!is_hw_pixel(frame.format()));
    }

    /// 声称"可显式启用"或"内部启用"的格式必须是硬件格式；没有硬解配置的编解码器要报
    /// `None`，而不是 `Pixel::None` 之类的假阳性（那样会把 CPU 帧当 GPU 帧去回传）
    #[test]
    fn enabled_formats_are_always_hardware() {
        ffmpeg_next::init().expect("ffmpeg init");
        let types = candidate_types();
        let h264 = unsafe { ffi::avcodec_find_decoder(AVCodecID::AV_CODEC_ID_H264) };
        if !h264.is_null() {
            for device_type in &types {
                if let Some((pixel, _)) = hardware_config(h264, *device_type) {
                    assert!(
                        is_hw_pixel(pixel),
                        "h264/{device_type:?} 报了个 CPU 格式 {pixel:?}"
                    );
                }
            }
            // 探针实测本机（Windows）与 CI 的两个 8.1 构建都至少给 h264 报一条
            // `HW_DEVICE_CTX` 配置；一条都没有说明枚举或配置查询断了
            let found = types
                .iter()
                .filter(|device_type| hardware_config(h264, **device_type).is_some())
                .count();
            assert!(found > 0, "h264 在 {:?} 上没有一条硬件配置", types);
        }
        // 纯软件编解码器必须一条都不报——这是"该不该上硬解"的唯一判据
        let pcm = unsafe { ffi::avcodec_find_decoder(AVCodecID::AV_CODEC_ID_PCM_S16LE) };
        if !pcm.is_null() {
            for device_type in &types {
                assert!(
                    hardware_config(pcm, *device_type).is_none(),
                    "PCM 不该有硬件输出格式 {device_type:?}"
                );
            }
        }
    }

    /// `internal_only` 必须由配置表本身决定：这条标志直接决定装不装 `get_format`，
    /// 判错方向就是"该装的没装（悄悄软解）"或"不该装的装了（吸走 INTERNAL 候选）"
    #[test]
    fn internal_only_flag_matches_configs() {
        ffmpeg_next::init().expect("ffmpeg init");
        let types = candidate_types();
        for codec_id in [
            AVCodecID::AV_CODEC_ID_H264,
            AVCodecID::AV_CODEC_ID_HEVC,
            AVCodecID::AV_CODEC_ID_AV1,
            AVCodecID::AV_CODEC_ID_VP9,
        ] {
            let codec = unsafe { ffi::avcodec_find_decoder(codec_id) };
            if codec.is_null() {
                continue;
            }
            for device_type in &types {
                let Some((_, explicit)) = hardware_config(codec, *device_type) else {
                    continue;
                };
                let (mut explicit_in_table, mut internal_in_table) = (false, false);
                let mut index = 0;
                loop {
                    let config = unsafe { ffi::avcodec_get_hw_config(codec, index) };
                    if config.is_null() {
                        break;
                    }
                    index += 1;
                    if unsafe { (*config).device_type } != *device_type {
                        continue;
                    }
                    let methods = unsafe { (*config).methods };
                    explicit_in_table |=
                        methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int != 0;
                    internal_in_table |=
                        methods & ffi::AV_CODEC_HW_CONFIG_METHOD_INTERNAL as c_int != 0;
                }
                assert_eq!(
                    explicit, explicit_in_table,
                    "{}/{device_type:?} 的 explicit 判定与配置表不符（表里 INTERNAL={internal_in_table}）",
                    codec_name(codec_id)
                );
            }
        }
    }

    /// 方案说得清"用哪个设备、解码出什么、回传成什么"——日志与 A/B 都靠它
    #[test]
    fn plan_description_names_all_three_stages() {
        ffmpeg_next::init().expect("ffmpeg init");
        let Some(plan) = select(AVCodecID::AV_CODEC_ID_H264, Policy::Auto) else {
            return; // CI 的无 GPU runner：没有方案就没有日志可查
        };
        let text = plan.describe();
        let stages: Vec<&str> = text.split(" → ").collect();
        assert_eq!(
            stages.len(),
            3,
            "描述应当是 设备 → 解码输出 → 回传目标: {text}"
        );
        assert!(
            stages.iter().all(|stage| !stage.is_empty()),
            "描述里有空段: {text}"
        );
        assert_ne!(plan.cpu_format(), Pixel::None, "{text}");
    }
}
