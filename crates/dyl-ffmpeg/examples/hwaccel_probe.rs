//! 硬解能力探针：把"本机这套 FFmpeg 到底能硬解什么"问出来，不靠文档猜。
//!
//! 输出三件事：链接的 FFmpeg 版本与 configure 串里的 hwaccel 项；每个设备类型
//! 能否真的建出 `AVHWDeviceContext` 及其帧约束（宽高上限、可用像素格式）；每个
//! 编解码器 `avcodec_get_hw_config` 报出的方法与设备类型。
//!
//! 运行：`cargo run -p dyl-ffmpeg --example hwaccel_probe`

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint};

use ffi::{AVHWDeviceType, AVPixelFormat};
use ffmpeg_next::ffi;

fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return "(null)".to_string();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn strerror(rc: c_int) -> String {
    let mut buffer = [0u8; 256];
    unsafe { ffi::av_strerror(rc, buffer.as_mut_ptr() as *mut c_char, buffer.len()) };
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    let text = String::from_utf8_lossy(&buffer[..end]).into_owned();
    if text.is_empty() {
        format!("(码 {rc})")
    } else {
        format!("({text})")
    }
}

/// configure 串里只留下与硬件解码有关的项，整串太长且噪声大。
fn hw_config_flags(configuration: &str) -> Vec<String> {
    const NAMES: &[&str] = &[
        "hwaccels",
        "vaapi",
        "vdpau",
        "cuda",
        "cuvid",
        "nvdec",
        "v4l2m2m",
        "videotoolbox",
        "videocodecbox",
        "d3d11va",
        "d3d12va",
        "dxva2",
        "mediacodec",
        "amf",
        "qsv",
        "libvpl",
        "jnih",
    ];
    configuration
        .split_whitespace()
        .filter(|flag| NAMES.iter().any(|name| *flag == format!("--enable-{name}")))
        .map(|flag| flag.to_string())
        .collect()
}

fn split_version(packed: c_uint) -> String {
    format!(
        "{}.{}.{}",
        packed >> 16,
        (packed >> 8) & 0xff,
        packed & 0xff
    )
}

fn pix_fmt_name(fmt: AVPixelFormat) -> String {
    cstr(unsafe { ffi::av_get_pix_fmt_name(fmt) })
}

fn is_hw_pix_fmt(fmt: AVPixelFormat) -> bool {
    let desc = unsafe { ffi::av_pix_fmt_desc_get(fmt) };
    if desc.is_null() {
        return false;
    }
    let hwaccel = ffi::AV_PIX_FMT_FLAG_HWACCEL as u64;
    unsafe { (((*desc).flags as u64) & hwaccel) != 0 }
}

/// 像素格式数组（以 `AV_PIX_FMT_NONE` 结尾）转成可读清单。
fn pix_fmt_list(ptr: *mut AVPixelFormat) -> Vec<String> {
    let mut out = Vec::new();
    if ptr.is_null() {
        return out;
    }
    for i in 0..64usize {
        let fmt = unsafe { *ptr.add(i) };
        if fmt == AVPixelFormat::AV_PIX_FMT_NONE {
            break;
        }
        let marker = if is_hw_pix_fmt(fmt) { "(GPU)" } else { "" };
        out.push(format!("{}{marker}", pix_fmt_name(fmt)));
    }
    out
}

fn method_names(methods: c_int) -> String {
    let named: Vec<&str> = [
        (
            ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX,
            "HW_DEVICE_CTX",
        ),
        (
            ffi::AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX,
            "HW_FRAMES_CTX",
        ),
        (ffi::AV_CODEC_HW_CONFIG_METHOD_INTERNAL, "INTERNAL"),
        (ffi::AV_CODEC_HW_CONFIG_METHOD_AD_HOC, "AD_HOC"),
    ]
    .iter()
    .filter(|(bit, _)| (methods & (*bit as c_int)) != 0)
    .map(|(_, name)| *name)
    .collect();
    if named.is_empty() {
        format!("0x{methods:x}")
    } else {
        named.join("|")
    }
}

fn iterate_device_types() -> Vec<AVHWDeviceType> {
    let mut types = Vec::new();
    let mut prev = AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
    loop {
        let next = unsafe { ffi::av_hwdevice_iterate_types(prev) };
        if next == AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            break;
        }
        types.push(next);
        prev = next;
        if types.len() >= 32 {
            break;
        }
    }
    types
}

/// 建一个默认设备上下文，看它是否真的可用，并读它的帧约束。
fn probe_device(device_type: AVHWDeviceType) -> String {
    let name = cstr(unsafe { ffi::av_hwdevice_get_type_name(device_type) });
    let mut buf: *mut ffi::AVBufferRef = std::ptr::null_mut();
    let rc = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut buf,
            device_type,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 {
        return format!(
            "{name}: 建不出设备上下文 {}（本机器/驱动不可用）",
            strerror(rc)
        );
    }
    let mut detail = format!("{name}: 设备可创建");
    // 不传 hwconfig，拿的是该设备的最大能力（含尺寸上限与格式清单）
    let constraints = unsafe { ffi::av_hwdevice_get_hwframe_constraints(buf, std::ptr::null()) };
    if constraints.is_null() {
        detail.push_str("；帧约束查询返回 NULL");
    } else {
        let (hw, sw, min_w, min_h, max_w, max_h) = unsafe {
            (
                pix_fmt_list((*constraints).valid_hw_formats),
                pix_fmt_list((*constraints).valid_sw_formats),
                (*constraints).min_width,
                (*constraints).min_height,
                (*constraints).max_width,
                (*constraints).max_height,
            )
        };
        detail.push_str(&format!(
            "；帧约束 尺寸 {min_w}x{min_h}..{max_w}x{max_h} hw=[{}] sw=[{}]",
            hw.join(","),
            sw.join(",")
        ));
        let mut raw = constraints;
        unsafe { ffi::av_hwframe_constraints_free(&mut raw) };
    }
    unsafe { ffi::av_buffer_unref(&mut buf) };
    detail
}

fn probe_codec(id: ffi::AVCodecID, label: &str) -> Vec<String> {
    let codec = unsafe { ffi::avcodec_find_decoder(id) };
    if codec.is_null() {
        return vec![format!("{label}: 本构建没有这个解码器")];
    }
    let mut lines = Vec::new();
    for index in 0..16i32 {
        let cfg = unsafe { ffi::avcodec_get_hw_config(codec, index) };
        if cfg.is_null() {
            break;
        }
        let methods = unsafe { (*cfg).methods };
        let device = cstr(unsafe { ffi::av_hwdevice_get_type_name((*cfg).device_type) });
        let pix = unsafe { (*cfg).pix_fmt };
        // 只有 HW_DEVICE_CTX 是"我们能显式选择并检查"的路径；INTERNAL / AD_HOC
        // 会让软解上下文静默换成 GPU 像素格式，那是必须拦下来的行为。
        let explicit = (methods & (ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int)) != 0;
        lines.push(format!(
            "{label}: {device} 输出 {}{} 方法 [{}]{}",
            pix_fmt_name(pix),
            if is_hw_pix_fmt(pix) { "(GPU)" } else { "" },
            method_names(methods),
            if explicit { " ← 可显式启用" } else { "" },
        ));
    }
    if lines.is_empty() {
        lines.push(format!("{label}: 没有 hwaccel 配置（只能软解）"));
    }
    lines
}

fn main() {
    ffmpeg_next::init().expect("ffmpeg init");
    println!("=== 链接的 FFmpeg ===");
    println!(
        "运行库版本 avcodec={} avformat={}（avutil 编译期={})",
        split_version(unsafe { ffi::avcodec_version() }),
        split_version(unsafe { ffi::avformat_version() }),
        cstr(unsafe { ffi::av_version_info() }),
    );
    println!(
        "avcodec configure 的硬件项: [{}]",
        hw_config_flags(&cstr(unsafe { ffi::avcodec_configuration() })).join(" ")
    );
    println!("（空列表意味着这套构建没编译 hwaccels，硬解无从谈起）");

    println!("\n=== 设备类型（遍历 + 实际创建 + 帧约束）===");
    for device_type in iterate_device_types() {
        println!("{}", probe_device(device_type));
    }

    println!("\n=== 解码器的 hwaccel 配置 ===");
    let codecs: Vec<(ffi::AVCodecID, &str)> = vec![
        (ffi::AVCodecID::AV_CODEC_ID_H264, "h264"),
        (ffi::AVCodecID::AV_CODEC_ID_HEVC, "hevc"),
        (ffi::AVCodecID::AV_CODEC_ID_AV1, "av1"),
        (ffi::AVCodecID::AV_CODEC_ID_VP9, "vp9"),
        (ffi::AVCodecID::AV_CODEC_ID_MPEG2VIDEO, "mpeg2video"),
        (ffi::AVCodecID::AV_CODEC_ID_VVC, "vvc"),
    ];
    for (id, label) in codecs {
        for line in probe_codec(id, label) {
            println!("{line}");
        }
    }
}
