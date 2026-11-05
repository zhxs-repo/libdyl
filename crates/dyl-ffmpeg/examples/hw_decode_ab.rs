//! 硬解 A/B：同一条包流，软解与硬解逐帧对齐比较。
//!
//! 这是方案 9 步骤 2 的验收工具，回答三个只能实测、不能靠文档的问题：
//!
//! 1. **帧对不对得上**：同一 PTS 的硬解帧与软解帧差多少。NV12 与 YUV420P 不能直接
//!    比字节，所以两条路径都先拆成不带对齐填充的平面 Y/U/V，再算每平面 max/mean
//!    误差与 PSNR。
//! 2. **时间戳与色彩元数据对不对得上**：两条路径给出的 PTS 与 `color_space` /
//!    `color_primaries` / `color_range` 必须一致——不一致就是"帧能出、时间戳全 0"
//!    那一类静默错误。
//! 3. **`av_hwframe_transfer_data` 到底搬不搬属性**：hwcontext.h 没写（方案 5.2 的
//!    待验证项）。这里单独探一次，比较同一帧的 GPU 侧与回传侧属性。解码器一律从
//!    GPU 帧取属性，所以答案只决定"将来能不能改成从回传帧取"。
//!
//! 运行：
//! `cargo run -p dyl-ffmpeg --release --example hw_decode_ab -- [file] [auto|设备名]`
//!
//! 没有可用硬解设备（CI 的无 GPU runner）时以"这台机器没什么可验"退出非零——
//! 那种环境该跑的是 `hwaccel_probe`，不要让 A/B 假装通过。

use std::process::ExitCode;
use std::time::Instant;

use dyl_core::{DylError, Result};
use dyl_ffmpeg::hwaccel::{self, Policy};
use dyl_ffmpeg::{
    is_hw_pixel, Demuxer, FfmpegDemuxer, FfmpegVideoDecoder, Packet, PixelFormat, TrackSelection,
    VideoDecoder, VideoFrame,
};

/// A/B 只在本地文件上跑，不需要中断：给 demuxer 一个永不置位的 shutdown 标志
fn running() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
}

/// 判定阈值。硬件解码器与软解不是位精确一致（IDCT 与去块实现不同），但差到看得见
/// 就说明回传或平面拆解错了。40 dB 约合 1% 的平均误差：宽松到容纳 ±1 的抖动，
/// 又足够挡住"色度整条丢失""平面错位"这类错误。
const MIN_PSNR_DB: f64 = 40.0;
const MAX_MEAN_ABS: f64 = 2.0;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let file = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "test_video.mp4".into());
    let wanted = args.get(2).cloned().unwrap_or_else(|| "auto".into());
    let policy = match wanted.as_str() {
        "off" | "0" => Policy::SoftwareOnly,
        other => Policy::parse(other),
    };
    if policy == Policy::SoftwareOnly {
        eprintln!("第二参数 {wanted} 解析成了软解；A/B 需要 auto 或一个硬件设备名");
        return ExitCode::FAILURE;
    }
    ffmpeg_next::init().expect("ffmpeg init");
    // 装与不装方案的区别只在 tracing 日志里（get_format 的选中/回退、帧池实际格式），
    // 所以这个工具自己带一个订阅者，按 RUST_LOG 输出
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    println!(
        "avcodec {} / {} / {} @ {file} (策略 {policy:?})",
        major(unsafe { ffmpeg_next::ffi::avcodec_version() }),
        ffmpeg_next::util::configuration(),
        ffmpeg_next::util::license(),
    );
    match run(&file, policy) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("A/B 失败: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(file: &str, policy: Policy) -> Result<()> {
    let plan = describe_plan(file, policy)?;
    println!("硬解方案: {plan}");
    let diff = compare(file, policy)?;
    probe_transfer_props(file, policy)?;
    println!("{}", diff.report());
    match diff.failure() {
        Some(reason) => Err(DylError::Decode(reason)),
        None => {
            println!("判定: 通过（PSNR ≥ {MIN_PSNR_DB} dB 且 mean ≤ {MAX_MEAN_ABS}）");
            Ok(())
        }
    }
}

/// 用与解码器相同的入口问一次方案，只为了把设备名打进输出并提前退出
fn describe_plan(file: &str, policy: Policy) -> Result<String> {
    let codec = probe_context(file, |context| {
        ffmpeg_next::ffi::AVCodecID::from(context.id())
    })?;
    match hwaccel::select(codec, policy) {
        Some(plan) => Ok(plan.describe()),
        None => Err(DylError::NotSupported(format!(
            "这台机器/这套构建在 {policy:?} 下给不出硬解方案（先跑 hwaccel_probe 看有什么）"
        ))),
    }
}

fn probe_context<T>(file: &str, take: impl FnOnce(&ffmpeg_next::codec::Context) -> T) -> Result<T> {
    let mut demuxer = FfmpegDemuxer::new();
    demuxer.open(file, &TrackSelection::default(), running())?;
    let context = demuxer
        .take_video_codec_context()
        .ok_or_else(|| DylError::Demux("文件里没有选中的视频流".into()))?;
    Ok(take(&context))
}

/// 一条解码通路：自己的 demuxer + 解码器 + 已解出但还没比对的帧
struct Lane {
    demuxer: FfmpegDemuxer,
    decoder: FfmpegVideoDecoder,
    video: usize,
    pending: std::collections::VecDeque<VideoFrame>,
    drained: bool,
    frames: usize,
    /// 送包 + 取帧的墙钟累计（不含读包与平面比对），见 [`Lane::ms_per_frame`]
    work: f64,
}

impl Lane {
    fn open(file: &str, policy: Policy) -> Result<Self> {
        let mut demuxer = FfmpegDemuxer::new();
        demuxer.open(file, &TrackSelection::default(), running())?;
        let video = video_index(&demuxer)?;
        let context = demuxer
            .take_video_codec_context()
            .ok_or_else(|| DylError::Demux("拿不到视频 CodecContext".into()))?;
        let decoder = FfmpegVideoDecoder::with_policy(context, policy)?;
        Ok(Self {
            demuxer,
            decoder,
            video,
            pending: std::collections::VecDeque::new(),
            drained: false,
            frames: 0,
            work: 0.0,
        })
    }

    /// 下一帧；`None` 表示这条路径解完了
    fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }
        if self.drained {
            return Ok(None);
        }
        loop {
            match self.demuxer.read_packet()? {
                Some(packet) => {
                    if packet.stream_index != self.video {
                        continue;
                    }
                    let start = Instant::now();
                    self.decoder.submit_packet(&packet)?;
                    self.drain(false)?;
                    self.work += start.elapsed().as_secs_f64();
                    if let Some(frame) = self.pending.pop_front() {
                        return Ok(Some(frame));
                    }
                }
                None => {
                    self.drained = true;
                    let start = Instant::now();
                    self.decoder.send_eof()?;
                    self.drain(true)?;
                    self.work += start.elapsed().as_secs_f64();
                    return Ok(self.pending.pop_front());
                }
            }
        }
    }

    /// 取空解码器当前的输出
    ///
    /// `at_eof` 只放宽一件事：`send_eof()` 之后 FFmpeg 用 `AVERROR_EOF` 结束排空，
    /// 而本项目的 `receive_frame` 把它包成错误上抛（`dyl-pipeline` 的 `drain_decoder`
    /// 同样按正常收尾处理）。在那之前任何错误都算失败——包括 GPU 回传失败，
    /// 把它咽掉就等于"硬解路径少了几帧也没人报"。
    fn drain(&mut self, at_eof: bool) -> Result<()> {
        loop {
            match self.decoder.receive_frame() {
                Ok(Some(frame)) => {
                    self.frames += 1;
                    self.pending.push_back(frame);
                }
                Ok(None) => return Ok(()),
                Err(err) if at_eof && is_eof_error(&err) => return Ok(()),
                Err(err) => return Err(err),
            }
        }
    }

    fn ms_per_frame(&self) -> f64 {
        if self.frames == 0 {
            return 0.0;
        }
        self.work * 1000.0 / self.frames as f64
    }
}

fn video_index(demuxer: &FfmpegDemuxer) -> Result<usize> {
    demuxer
        .info()
        .and_then(|info| info.video_stream_index)
        .ok_or_else(|| DylError::Demux("文件里没有选中的视频流".into()))
}

/// 两条路径的比对累计量
struct Diff {
    frames: usize,
    software_frames: usize,
    hardware_frames: usize,
    plane_sse: [f64; 3],
    plane_samples: [usize; 3],
    plane_mean_abs: [f64; 3],
    /// 带符号的平均误差：`max 1 / mean 0.5` 这种数只看绝对值分不清"随机取整抖动"
    /// 与"整平面偏了一档"，后者才是回传或平面拆解错了
    plane_signed_mean: [f64; 3],
    plane_max_abs: [i32; 3],
    psnr: [f64; 3],
    software_format: String,
    hardware_format: String,
    pts_mismatch: usize,
    props_mismatch: usize,
    ms_sw: f64,
    ms_hw: f64,
}

const PLANE_NAMES: [&str; 3] = ["Y", "U", "V"];

impl Diff {
    fn new() -> Self {
        Self {
            frames: 0,
            software_frames: 0,
            hardware_frames: 0,
            plane_sse: [0.0; 3],
            plane_samples: [0; 3],
            plane_mean_abs: [0.0; 3],
            plane_signed_mean: [0.0; 3],
            plane_max_abs: [0; 3],
            psnr: [0.0; 3],
            software_format: String::new(),
            hardware_format: String::new(),
            pts_mismatch: 0,
            props_mismatch: 0,
            ms_sw: 0.0,
            ms_hw: 0.0,
        }
    }

    fn report(&self) -> String {
        let mut lines = vec![format!(
            "帧数 软解 {} / 硬解 {} / 逐帧比对 {}",
            self.software_frames, self.hardware_frames, self.frames
        )];
        lines.push(format!(
            "输出格式 软解 {} / 硬解 {}",
            self.software_format, self.hardware_format
        ));
        lines.push(format!(
            "单帧解码墙钟（送包+取帧，不含读包与比对）软解 {:.2} ms / 硬解 {:.2} ms",
            self.ms_sw, self.ms_hw
        ));
        for (index, name) in PLANE_NAMES.iter().enumerate() {
            lines.push(format!(
                "{}: max {:>3} mean {:.4} signed {:+.4} PSNR {:>6.1} dB",
                name,
                self.plane_max_abs[index],
                self.plane_mean_abs[index],
                self.plane_signed_mean[index],
                self.psnr[index]
            ));
        }
        lines.push(format!(
            "PTS 不一致 {} 帧 / 色彩元数据不一致 {} 帧",
            self.pts_mismatch, self.props_mismatch
        ));
        lines.join("\n")
    }

    /// `None` 表示通过
    fn failure(&self) -> Option<String> {
        let report = self.report();
        if self.frames == 0 {
            return Some("一条帧都没解出来".into());
        }
        if self.software_frames != self.hardware_frames {
            return Some(format!(
                "两路帧数不同（软解 {} / 硬解 {}）\n{report}",
                self.software_frames, self.hardware_frames
            ));
        }
        if self.pts_mismatch > 0 {
            return Some(format!(
                "{} 帧的 PTS 与软解不一致\n{report}",
                self.pts_mismatch
            ));
        }
        if self.props_mismatch > 0 {
            return Some(format!(
                "{} 帧的色彩元数据与软解不一致\n{report}",
                self.props_mismatch
            ));
        }
        for ((name, psnr), mean_abs) in PLANE_NAMES.iter().zip(self.psnr).zip(self.plane_mean_abs) {
            if psnr < MIN_PSNR_DB {
                return Some(format!(
                    "{name} 平面 PSNR {psnr:.1} dB 低于 {MIN_PSNR_DB} dB\n{report}"
                ));
            }
            if mean_abs > MAX_MEAN_ABS {
                return Some(format!(
                    "{name} 平面平均绝对误差 {mean_abs:.4} 超过 {MAX_MEAN_ABS}\n{report}"
                ));
            }
        }
        None
    }
}

/// 逐帧对齐比较软解与指定策略的解码结果
fn compare(file: &str, policy: Policy) -> Result<Diff> {
    let mut software = Lane::open(file, Policy::SoftwareOnly)?;
    let mut hardware = Lane::open(file, policy)?;
    let mut diff = Diff::new();
    let mut mean_abs = [0.0f64; 3];
    let mut signed = [0.0f64; 3];
    loop {
        let left = software.next_frame()?;
        let right = hardware.next_frame()?;
        match (left, right) {
            (Some(a), Some(b)) => {
                if a.pts != b.pts {
                    diff.pts_mismatch += 1;
                    println!("PTS 不一致 @ {:?} vs {:?}", a.pts, b.pts);
                }
                if (a.color_space, a.color_primaries, a.color_range)
                    != (b.color_space, b.color_primaries, b.color_range)
                {
                    diff.props_mismatch += 1;
                    println!(
                        "色彩元数据不一致 @ {:?}: 软解 {:?}/{:?}/{:?} vs 硬解 {:?}/{:?}/{:?}",
                        a.pts,
                        a.color_space,
                        a.color_primaries,
                        a.color_range,
                        b.color_space,
                        b.color_primaries,
                        b.color_range
                    );
                }
                if diff.software_format.is_empty() {
                    diff.software_format = format!("{:?} {}x{}", a.format, a.width, a.height);
                    diff.hardware_format = format!("{:?} {}x{}", b.format, b.width, b.height);
                }
                if a.width != b.width || a.height != b.height {
                    return Err(DylError::Decode(format!(
                        "帧尺寸不一致 @ {:?}: {}x{} vs {}x{}",
                        a.pts, a.width, a.height, b.width, b.height
                    )));
                }
                let left_planes = planar(&a).ok_or_else(|| {
                    DylError::NotSupported(format!("软解输出 {:?} 拆不成平面 Y/U/V", a.format))
                })?;
                let right_planes = planar(&b).ok_or_else(|| {
                    DylError::NotSupported(format!("硬解输出 {:?} 拆不成平面 Y/U/V", b.format))
                })?;
                for index in 0..3 {
                    let (l, r) = (&left_planes[index], &right_planes[index]);
                    if l.len() != r.len() {
                        return Err(DylError::Decode(format!(
                            "{} 平面长度不一致 @ {:?}: {} vs {}",
                            PLANE_NAMES[index],
                            a.pts,
                            l.len(),
                            r.len()
                        )));
                    }
                    for (x, y) in l.iter().zip(r.iter()) {
                        let d = *x as i32 - *y as i32;
                        mean_abs[index] += d.abs() as f64;
                        signed[index] += d as f64;
                        diff.plane_sse[index] += (d * d) as f64;
                        diff.plane_max_abs[index] = diff.plane_max_abs[index].max(d.abs());
                    }
                    diff.plane_samples[index] += l.len();
                }
                diff.frames += 1;
            }
            (None, None) => break,
            (None, Some(_)) | (Some(_), None) => break,
        }
    }
    for index in 0..3 {
        let samples = diff.plane_samples[index].max(1) as f64;
        diff.plane_mean_abs[index] = mean_abs[index] / samples;
        diff.plane_signed_mean[index] = signed[index] / samples;
        let mse = diff.plane_sse[index] / samples;
        diff.psnr[index] = if mse <= 0.0 {
            f64::INFINITY
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        };
    }
    diff.ms_sw = software.ms_per_frame();
    diff.ms_hw = hardware.ms_per_frame();
    // 两条路径各自产出多少帧——比对只走配对成功的部分，所以"某一路提前结束"只能
    // 从这里看出来（回传失败被咽掉时也是这里露馅）
    diff.software_frames = software.frames;
    diff.hardware_frames = hardware.frames;
    Ok(diff)
}

/// 把帧拆成不带对齐填充的平面 Y/U/V
///
/// YUV420P 是三个平面；NV12 是 Y + 交错 UV，色度必须按偶/奇字节拆开才能与前者对齐
/// ——方案 9 步骤 2 的验收里"先统一格式再比"就是这一步。
fn planar(frame: &VideoFrame) -> Option<[Vec<u8>; 3]> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let chroma_w = width / 2;
    let chroma_h = frame.height as usize / 2;
    let y = rows(frame.planes.first()?, width, height)?;
    match frame.format {
        PixelFormat::Yuv420p => {
            let u = rows(frame.planes.get(1)?, chroma_w, chroma_h)?;
            let v = rows(frame.planes.get(2)?, chroma_w, chroma_h)?;
            Some([y, u, v])
        }
        PixelFormat::Nv12 => {
            let plane = frame.planes.get(1)?;
            let stride = plane.stride;
            if stride < 2 * chroma_w {
                return None;
            }
            let mut u = Vec::with_capacity(chroma_w * chroma_h);
            let mut v = Vec::with_capacity(chroma_w * chroma_h);
            for row in 0..chroma_h {
                let start = row * stride;
                let line = plane.data.get(start..start + 2 * chroma_w)?;
                u.extend(line.iter().step_by(2));
                v.extend(line.iter().skip(1).step_by(2));
            }
            Some([y, u, v])
        }
        _ => None,
    }
}

fn rows(plane: &dyl_ffmpeg::PlaneData, width: usize, height: usize) -> Option<Vec<u8>> {
    if plane.stride < width {
        return None;
    }
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = row * plane.stride;
        out.extend(plane.data.get(start..start + width)?);
    }
    Some(out)
}

/// 实测 `av_hwframe_transfer_data` 是否搬运 PTS 与色彩属性（方案 5.2 的待验证项）
///
/// 走解码器之外的裸通路：自己装 `HwPlan`、自己 open、自己回传，再比较 GPU 帧与
/// 回传帧的属性。解码器一律从 GPU 帧取属性，所以这里的答案只决定
/// "以后能不能改成从回传帧取"。
fn probe_transfer_props(file: &str, policy: Policy) -> Result<()> {
    let mut demuxer = FfmpegDemuxer::new();
    demuxer.open(file, &TrackSelection::default(), running())?;
    let video = video_index(&demuxer)?;
    let mut context = demuxer
        .take_video_codec_context()
        .ok_or_else(|| DylError::Demux("拿不到视频 CodecContext".into()))?;
    let codec = ffmpeg_next::ffi::AVCodecID::from(context.id());
    let plan = hwaccel::select(codec, policy)
        .ok_or_else(|| DylError::NotSupported("没有硬解方案".into()))?;
    // SAFETY: context 是本函数独占的活 `AVCodecContext`，且此处早于下面的 `video()` open
    unsafe { hwaccel::install(context.as_mut_ptr(), &plan) }?;
    let mut decoder = context
        .decoder()
        .video()
        .map_err(|e| DylError::Decode(format!("video open: {e}")))?;

    let mut packets: Vec<Packet> = Vec::new();
    while packets.len() < 32 {
        match demuxer.read_packet()? {
            Some(packet) if packet.stream_index == video => packets.push(packet),
            Some(_) => {}
            None => break,
        }
    }
    let mut cpu = ffmpeg_next::frame::Video::empty();
    if let Some(first) = packets.first() {
        decoder.set_packet_time_base(first.time_base);
    }
    for packet in &packets {
        let borrow = RawPacket::new(&packet.data, packet.pts, packet.dts);
        decoder
            .send_packet(&borrow)
            .map_err(|e| DylError::Decode(format!("send_packet: {e}")))?;
        loop {
            let mut frame = ffmpeg_next::frame::Video::empty();
            match decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    if !is_hw_pixel(frame.format()) {
                        continue;
                    }
                    let pool = hwaccel::pool_sw_format(&frame);
                    let target = pool.unwrap_or(plan.cpu_format());
                    println!(
                        "回传目标：帧池 sw_format = {}，方案预测 = {}{}",
                        pool.map(hwaccel::pixel_name)
                            .unwrap_or_else(|| "(问不出)".into()),
                        hwaccel::pixel_name(plan.cpu_format()),
                        if pool == Some(plan.cpu_format()) {
                            String::new()
                        } else {
                            // 预测只来自设备帧约束的清单（本机把 nv12 排在首位），
                            // 10-bit 的池实际是 p010le——所以解码器以帧池为准
                            "（不一致，以帧池为准）".into()
                        }
                    );
                    hwaccel::prepare_cpu_frame(&mut cpu, target, frame.width(), frame.height())?;
                    hwaccel::transfer(&mut cpu, &frame)?;
                    report_props(&frame, &cpu);
                    return Ok(());
                }
                Err(err) if is_eagain(&err) => break,
                Err(err) => return Err(DylError::Decode(format!("receive_frame: {err}"))),
            }
        }
    }
    Err(DylError::Decode(
        "装了方案却没产出 GPU 帧（FFmpeg 落回软解；查 get_format 的 warn 日志）".into(),
    ))
}

/// 打印 `av_hwframe_transfer_data` 对属性的处理结果（方案 5.2 的待验证项）
///
/// 只看"回传帧有没有拿到 GPU 帧的那个值"，所以逐条打出实际取值再判一致与否：
/// 两边都是默认值时"一致"什么也没证明（本机 8-bit H.264 的色彩属性就是这样），
/// 只有 GPU 帧上确实有值、回传帧也拿到才算搬运输入。`pix_fmt` 不参与判定——
/// 按定义就是 GPU 格式 → CPU 格式；尺寸由 `prepare_cpu_frame` 自己设，也不是搬运。
fn report_props(gpu: &ffmpeg_next::frame::Video, cpu: &ffmpeg_next::frame::Video) {
    let mut lines = Vec::new();
    let mut compare = |name: &str, a: String, b: String| {
        lines.push(format!(
            "transfer {name}: GPU 帧 {a} → 回传帧 {b} ({})",
            if a == b { "一致" } else { "不一致" }
        ));
    };
    compare(
        "size",
        format!("{}x{}", gpu.width(), gpu.height()),
        format!("{}x{}", cpu.width(), cpu.height()),
    );
    compare(
        "pts",
        format!("{:?}", gpu.pts()),
        format!("{:?}", cpu.pts()),
    );
    compare(
        "color_space",
        format!("{:?}", gpu.color_space()),
        format!("{:?}", cpu.color_space()),
    );
    compare(
        "color_primaries",
        format!("{:?}", gpu.color_primaries()),
        format!("{:?}", cpu.color_primaries()),
    );
    compare(
        "color_range",
        format!("{:?}", gpu.color_range()),
        format!("{:?}", cpu.color_range()),
    );
    println!("{}", lines.join("\n"));
    // 三个属性都停在默认值上时，"一致"只是两个默认值相等，说明不了搬运
    let bare = |value: String| value == "Unspecified" || value == "Unknown";
    if bare(format!("{:?}", gpu.color_space()))
        && bare(format!("{:?}", gpu.color_primaries()))
        && bare(format!("{:?}", gpu.color_range()))
    {
        println!(
            "（注：GPU 帧本身没带色彩属性，上面三行的\"一致\"不构成证据；\
             产品通路靠解码器上下文的 SPS VUI 回退，逐帧比对已覆盖）"
        );
    }
}

fn is_eagain(err: &ffmpeg_next::Error) -> bool {
    matches!(err, ffmpeg_next::Error::Other { errno } if *errno == ffmpeg_next::error::EAGAIN)
}

/// 排空结束时 FFmpeg 给的是 `AVERROR_EOF`，ffmpeg-next 的 Display 是 "End of file"
///
/// 本项目的 `receive_frame` 把它包成 `DylError::Decode`，原始错误类型已经丢失，
/// 所以只能按消息判——比"排空阶段任何错误都放行"精确，回传失败仍会上抛。
fn is_eof_error(err: &DylError) -> bool {
    matches!(err, DylError::Decode(text) if text.contains("End of file"))
}

/// 直接喂 `AVPacket`，带上真实 PTS/DTS
///
/// `ffmpeg_next::codec::packet::Borrow` 只搬数据字节，把 `pts` 留成 0（而不是
/// `AV_NOPTS_VALUE`），帧时间戳会全 0——解码器内部用同一理由的 `BorrowWithPts`，
/// 那个类型是私有的，这里为属性探针重写一份最小的
struct RawPacket<'a> {
    packet: ffmpeg_next::ffi::AVPacket,
    _data: &'a [u8],
}

impl<'a> RawPacket<'a> {
    fn new(data: &'a [u8], pts: Option<i64>, dts: Option<i64>) -> Self {
        use ffmpeg_next::ffi::AV_NOPTS_VALUE;
        let mut packet: ffmpeg_next::ffi::AVPacket = unsafe { std::mem::zeroed() };
        packet.data = data.as_ptr() as *mut _;
        packet.size = data.len() as i32;
        packet.pts = pts.unwrap_or(AV_NOPTS_VALUE);
        packet.dts = dts.unwrap_or(AV_NOPTS_VALUE);
        Self {
            packet,
            _data: data,
        }
    }
}

impl<'a> ffmpeg_next::codec::packet::Ref for RawPacket<'a> {
    fn as_ptr(&self) -> *const ffmpeg_next::ffi::AVPacket {
        &self.packet
    }
}

impl<'a> Drop for RawPacket<'a> {
    fn drop(&mut self) {
        // SAFETY: 这里的 `AVPacket` 从未持有引用计数的缓冲，unref 只清 data/size
        self.packet.data = std::ptr::null_mut();
        self.packet.size = 0;
        unsafe { ffmpeg_next::ffi::av_packet_unref(&mut self.packet) };
    }
}

fn major(packed: u32) -> u32 {
    packed >> 16
}
