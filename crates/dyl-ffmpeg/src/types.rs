use serde::{Deserialize, Serialize};

/// 像素格式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Yuv420p,
    Nv12,
    Rgb24,
    Bgra,
    Unknown,
}

impl From<ffmpeg_next::format::Pixel> for PixelFormat {
    fn from(p: ffmpeg_next::format::Pixel) -> Self {
        match p {
            ffmpeg_next::format::Pixel::YUV420P => PixelFormat::Yuv420p,
            ffmpeg_next::format::Pixel::NV12 => PixelFormat::Nv12,
            ffmpeg_next::format::Pixel::RGB24 => PixelFormat::Rgb24,
            ffmpeg_next::format::Pixel::BGRA => PixelFormat::Bgra,
            _ => PixelFormat::Unknown,
        }
    }
}

impl From<PixelFormat> for ffmpeg_next::format::Pixel {
    fn from(p: PixelFormat) -> Self {
        match p {
            PixelFormat::Yuv420p => ffmpeg_next::format::Pixel::YUV420P,
            PixelFormat::Nv12 => ffmpeg_next::format::Pixel::NV12,
            PixelFormat::Rgb24 => ffmpeg_next::format::Pixel::RGB24,
            PixelFormat::Bgra => ffmpeg_next::format::Pixel::BGRA,
            PixelFormat::Unknown => ffmpeg_next::format::Pixel::None,
        }
    }
}

/// FFmpeg 像素格式是否是硬件（GPU 驻留）格式
///
/// 硬件格式的 `AVFrame.data[i]` 装的是平台句柄（`ID3D11Texture2D*`、
/// `AV_DRM_OBJECT*`…）而不是像素字节，按 CPU 平面拷出去只会得到垃圾或崩溃，
/// 而且一路都不会报错。所以判定不能靠手抄格式清单（`PixelFormat::from` 把它们
/// 折叠成 `Unknown`，正是这个坑），要读 `AVPixFmtDescriptor.flags` 的
/// `AV_PIX_FMT_FLAG_HWACCEL`——FFmpeg 新增的硬件格式因此自动被覆盖。
pub fn is_hw_pixel(pixel: ffmpeg_next::format::Pixel) -> bool {
    // `AVPixFmtDescriptor.flags` 的整型宽度随 FFmpeg 版本变化（unsigned int → uint64_t），
    // 统一加宽到 i64 再位与；HWACCEL 是第 3 位，不会溢出
    const AV_PIX_FMT_FLAG_HWACCEL: i64 = ffmpeg_next::ffi::AV_PIX_FMT_FLAG_HWACCEL as i64;
    match pixel.descriptor() {
        Some(descriptor) => unsafe {
            (*descriptor.as_ptr()).flags as i64 & AV_PIX_FMT_FLAG_HWACCEL != 0
        },
        None => false,
    }
}

/// 视频编解码器 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodecId {
    H264,
    H265,
    Vp9,
    Av1,
    Unknown,
}

impl From<ffmpeg_next::codec::Id> for VideoCodecId {
    fn from(id: ffmpeg_next::codec::Id) -> Self {
        match id {
            ffmpeg_next::codec::Id::H264 => VideoCodecId::H264,
            // ffmpeg_next::codec::Id::H265 只是 AV_CODEC_ID_HEVC 的反向别名，
            // `From<AVCodecID>` 永远不会产出它；匹配它会漏掉所有真实 HEVC 流
            ffmpeg_next::codec::Id::HEVC => VideoCodecId::H265,
            ffmpeg_next::codec::Id::VP9 => VideoCodecId::Vp9,
            ffmpeg_next::codec::Id::AV1 => VideoCodecId::Av1,
            _ => VideoCodecId::Unknown,
        }
    }
}

impl From<VideoCodecId> for ffmpeg_next::codec::Id {
    fn from(id: VideoCodecId) -> Self {
        match id {
            VideoCodecId::H264 => ffmpeg_next::codec::Id::H264,
            VideoCodecId::H265 => ffmpeg_next::codec::Id::HEVC,
            VideoCodecId::Vp9 => ffmpeg_next::codec::Id::VP9,
            VideoCodecId::Av1 => ffmpeg_next::codec::Id::AV1,
            VideoCodecId::Unknown => ffmpeg_next::codec::Id::None,
        }
    }
}

impl VideoCodecId {
    /// 契约里的 codec 字符串
    pub fn label(self) -> &'static str {
        match self {
            VideoCodecId::H264 => "h264",
            VideoCodecId::H265 => "h265",
            VideoCodecId::Vp9 => "vp9",
            VideoCodecId::Av1 => "av1",
            VideoCodecId::Unknown => "unknown",
        }
    }
}

/// 音频编解码器 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodecId {
    Aac,
    Mp3,
    Opus,
    Flac,
    Unknown,
}

impl From<ffmpeg_next::codec::Id> for AudioCodecId {
    fn from(id: ffmpeg_next::codec::Id) -> Self {
        match id {
            ffmpeg_next::codec::Id::AAC => AudioCodecId::Aac,
            ffmpeg_next::codec::Id::MP3 => AudioCodecId::Mp3,
            ffmpeg_next::codec::Id::OPUS => AudioCodecId::Opus,
            ffmpeg_next::codec::Id::FLAC => AudioCodecId::Flac,
            _ => AudioCodecId::Unknown,
        }
    }
}

impl From<AudioCodecId> for ffmpeg_next::codec::Id {
    fn from(id: AudioCodecId) -> Self {
        match id {
            AudioCodecId::Aac => ffmpeg_next::codec::Id::AAC,
            AudioCodecId::Mp3 => ffmpeg_next::codec::Id::MP3,
            AudioCodecId::Opus => ffmpeg_next::codec::Id::OPUS,
            AudioCodecId::Flac => ffmpeg_next::codec::Id::FLAC,
            AudioCodecId::Unknown => ffmpeg_next::codec::Id::None,
        }
    }
}

impl AudioCodecId {
    /// 契约里的 codec 字符串
    pub fn label(self) -> &'static str {
        match self {
            AudioCodecId::Aac => "aac",
            AudioCodecId::Mp3 => "mp3",
            AudioCodecId::Opus => "opus",
            AudioCodecId::Flac => "flac",
            AudioCodecId::Unknown => "unknown",
        }
    }
}

/// 采样格式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SampleFormat {
    U8,
    I16,
    I32,
    F32,
    F64,
    U8Planar,
    I16Planar,
    I32Planar,
    F32Planar,
    F64Planar,
    Unknown,
}

impl From<ffmpeg_next::format::Sample> for SampleFormat {
    fn from(s: ffmpeg_next::format::Sample) -> Self {
        use ffmpeg_next::format::sample::Type;
        match s {
            ffmpeg_next::format::Sample::U8(Type::Packed) => SampleFormat::U8,
            ffmpeg_next::format::Sample::I16(Type::Packed) => SampleFormat::I16,
            ffmpeg_next::format::Sample::I32(Type::Packed) => SampleFormat::I32,
            ffmpeg_next::format::Sample::F32(Type::Packed) => SampleFormat::F32,
            ffmpeg_next::format::Sample::F64(Type::Packed) => SampleFormat::F64,
            ffmpeg_next::format::Sample::U8(Type::Planar) => SampleFormat::U8Planar,
            ffmpeg_next::format::Sample::I16(Type::Planar) => SampleFormat::I16Planar,
            ffmpeg_next::format::Sample::I32(Type::Planar) => SampleFormat::I32Planar,
            ffmpeg_next::format::Sample::F32(Type::Planar) => SampleFormat::F32Planar,
            ffmpeg_next::format::Sample::F64(Type::Planar) => SampleFormat::F64Planar,
            _ => SampleFormat::Unknown,
        }
    }
}

/// 视频帧平面数据
#[derive(Debug, Clone)]
pub struct PlaneData {
    pub data: Vec<u8>,
    pub stride: usize,
}

/// Y′CbCr → R′G′B′ 矩阵系数（帧上未标注时为 `Unspecified`，见 `VideoFrame::yuv_matrix`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSpace {
    #[default]
    Unspecified,
    Bt601,
    Bt709,
    Bt2020,
}

/// 色域 primaries — 只保留矩阵回退判定需要的区分
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorPrimaries {
    #[default]
    Unspecified,
    Bt601,
    Bt709,
    Bt2020,
    /// 其余（Film / SMPTE 240M / P3 等）：不足以决定矩阵，交给分辨率启发式
    Other,
}

/// 采样量程
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorRange {
    #[default]
    Unspecified,
    /// 16..235 / 128±112（MPEG / TV）
    Limited,
    /// 0..255（JPEG / PC）
    Full,
}

/// 解析后的矩阵 — 渲染端唯一需要的形式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl YuvMatrix {
    /// 与 `yuv_to_rgb.wgsl` 里 `ColorParams.matrix` 的取值一一对应
    pub fn shader_index(self) -> u32 {
        match self {
            YuvMatrix::Bt601 => 0,
            YuvMatrix::Bt709 => 1,
            YuvMatrix::Bt2020 => 2,
        }
    }
}

impl From<ffmpeg_next::color::Space> for ColorSpace {
    fn from(space: ffmpeg_next::color::Space) -> Self {
        use ffmpeg_next::color::Space as S;
        match space {
            S::BT709 => ColorSpace::Bt709,
            S::BT470BG | S::SMPTE170M | S::FCC => ColorSpace::Bt601,
            S::BT2020NCL | S::BT2020CL => ColorSpace::Bt2020,
            _ => ColorSpace::Unspecified,
        }
    }
}

impl From<ffmpeg_next::color::Primaries> for ColorPrimaries {
    fn from(p: ffmpeg_next::color::Primaries) -> Self {
        use ffmpeg_next::color::Primaries as P;
        match p {
            P::BT709 => ColorPrimaries::Bt709,
            P::BT470BG | P::SMPTE170M => ColorPrimaries::Bt601,
            P::BT2020 => ColorPrimaries::Bt2020,
            P::Unspecified => ColorPrimaries::Unspecified,
            _ => ColorPrimaries::Other,
        }
    }
}

impl From<ffmpeg_next::color::Range> for ColorRange {
    fn from(r: ffmpeg_next::color::Range) -> Self {
        match r {
            ffmpeg_next::color::Range::MPEG => ColorRange::Limited,
            ffmpeg_next::color::Range::JPEG => ColorRange::Full,
            _ => ColorRange::Unspecified,
        }
    }
}

/// 显示宽高：编码宽高按像素宽高比（SAR）横向拉伸
///
/// SAR 是"单个像素的宽高比"，不一定是方形。容器里常见 `1080x1080` + SAR
/// `9:16` 的竖屏视频，编码分辨率是正方形，但每个像素窄而高；播放器必须
/// 横向缩到显示宽度，否则画面会被拉成正方形。
///
/// 只缩放宽度、高度保持编码高度：渲染目标按方形像素呈现，纵向拉伸只会
/// 改变字幕/弹幕的字号基准，且对应不上任何真实显示设备。SAR 无效
///（分子/分母 ≤ 0）时原样返回编码宽高。
pub fn display_size(width: u32, height: u32, sar_num: i32, sar_den: i32) -> (u32, u32) {
    if width == 0 || height == 0 || sar_num <= 0 || sar_den <= 0 {
        return (width, height);
    }
    let num = sar_num as u64;
    let den = sar_den as u64;
    let scaled = (width as u64 * num + den / 2) / den;
    (scaled.max(1).min(u32::MAX as u64) as u32, height)
}

/// 解码后的视频帧
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// PTS（`time_base` 单位的 tick）；`None` 表示无时间戳（AV_NOPTS_VALUE）
    pub pts: Option<i64>,
    /// `pts` 的时间基 — 视频解码帧透传输入 Packet 的时间基
    pub time_base: ffmpeg_next::Rational,
    /// 编码宽度（平面数据的实际宽度）
    pub width: u32,
    /// 编码高度（平面数据的实际高度）
    pub height: u32,
    /// 显示宽度（编码宽度 × SAR）；渲染几何必须用它，而不是 `width`
    pub display_width: u32,
    /// 显示高度（当前恒等于编码高度）
    pub display_height: u32,
    /// 像素格式
    pub format: PixelFormat,
    /// Y′CbCr 矩阵系数（未标注为 `Unspecified`）
    pub color_space: ColorSpace,
    /// 色域 primaries（未标注为 `Unspecified`）
    pub color_primaries: ColorPrimaries,
    /// 采样量程（未标注为 `Unspecified`）
    pub color_range: ColorRange,
    /// 平面数据（Y, U, V 或 Y, UV）
    pub planes: Vec<PlaneData>,
}

/// 解码后的音频帧
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// PTS（`time_base` 单位的 tick）；`None` 表示无时间戳（AV_NOPTS_VALUE）
    pub pts: Option<i64>,
    /// `pts` 的时间基 — FFmpeg 音频解码器以 `1/sample_rate` 输出帧时间戳，
    /// 与 Packet 的时间基无关
    pub time_base: ffmpeg_next::Rational,
    /// 采样率
    pub sample_rate: u32,
    /// 声道数
    pub channels: u32,
    /// 采样格式
    pub format: SampleFormat,
    /// 交错采样数据（f32）
    pub samples: Vec<f32>,
}

/// 压缩数据包
#[derive(Debug, Clone)]
pub struct Packet {
    /// 原始数据
    pub data: Vec<u8>,
    /// 流索引
    pub stream_index: usize,
    /// PTS（`time_base` 单位的 tick）；`None` 表示无时间戳（AV_NOPTS_VALUE）
    pub pts: Option<i64>,
    /// DTS（`time_base` 单位的 tick）；`None` 表示无时间戳（AV_NOPTS_VALUE）
    pub dts: Option<i64>,
    /// `time_base` 单位的持续时长；`None` 表示容器没给出（AV_DURATION_UNKNOWN）
    pub duration: Option<i64>,
    /// `pts`/`dts` 的时间基（流的 time_base）
    pub time_base: ffmpeg_next::Rational,
    /// 是否为关键帧
    pub is_key: bool,
}

/// 字幕编解码器 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleCodecId {
    /// 文本字幕（SRT / SubRip）
    SubRip,
    /// MP4 容器内文本字幕
    MovText,
    /// ASS 字幕
    Ass,
    /// SSA（ASS v4.0 前身，对话行字段布局相同）
    Ssa,
    /// 其他 / 未知
    Unknown,
}

impl From<ffmpeg_next::codec::Id> for SubtitleCodecId {
    fn from(id: ffmpeg_next::codec::Id) -> Self {
        match id {
            ffmpeg_next::codec::Id::SUBRIP => SubtitleCodecId::SubRip,
            ffmpeg_next::codec::Id::MOV_TEXT => SubtitleCodecId::MovText,
            ffmpeg_next::codec::Id::ASS => SubtitleCodecId::Ass,
            ffmpeg_next::codec::Id::SSA => SubtitleCodecId::Ssa,
            _ => SubtitleCodecId::Unknown,
        }
    }
}

impl SubtitleCodecId {
    /// 契约里的 codec 字符串
    pub fn label(self) -> &'static str {
        match self {
            SubtitleCodecId::SubRip => "subrip",
            SubtitleCodecId::MovText => "mov_text",
            SubtitleCodecId::Ass => "ass",
            SubtitleCodecId::Ssa => "ssa",
            SubtitleCodecId::Unknown => "unknown",
        }
    }

    /// 内核只做文本字幕；PGS/DVB 之类位图轨选了解码线程也出不了字
    pub fn is_text(self) -> bool {
        !matches!(self, SubtitleCodecId::Unknown)
    }

    /// 对话行带 ASS 字段布局（`Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`）
    pub fn is_ass_like(self) -> bool {
        matches!(self, SubtitleCodecId::Ass | SubtitleCodecId::Ssa)
    }
}

/// 轨道种类（容器里的流类型）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// 容器里的一条可播放轨道
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTrack {
    /// 容器内的流索引
    pub index: usize,
    pub kind: TrackKind,
    /// 契约 codec 标签（"h265" / "aac" / "ass"…）
    pub codec: String,
    /// 语言标签（Matroska/MP4 的 `language`），缺失或 `und` 时为 `None`
    pub language: Option<String>,
    pub title: Option<String>,
    /// 容器里的 default 标记（AV_DISPOSITION_DEFAULT）
    pub default: bool,
    /// 本次播放实际选中的轨道
    pub selected: bool,
}

/// 媒体文件信息
#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub duration: f64,
    pub video_stream_index: Option<usize>,
    pub audio_stream_index: Option<usize>,
    pub subtitle_stream_index: Option<usize>,
    pub video_codec_id: Option<VideoCodecId>,
    pub audio_codec_id: Option<AudioCodecId>,
    pub subtitle_codec_id: Option<SubtitleCodecId>,
    pub width: u32,
    pub height: u32,
    /// 显示宽度（编码宽度 × SAR）；容器信息与快照都报这个尺寸
    pub display_width: u32,
    /// 显示高度（当前恒等于编码高度）
    pub display_height: u32,
    pub pixel_format: PixelFormat,
    pub fps: f64,
    pub sample_rate: u32,
    pub channels: u32,
    pub sample_format: SampleFormat,
    /// 容器里全部视频/音频/字幕轨道（含未选中的），索引顺序与容器一致
    pub tracks: Vec<StreamTrack>,
    /// 所选字幕流的 CodecPrivate：ASS/SSA 轨在这里带完整脚本头（`[V4+ Styles]` 与
    /// PlayResX/Y）。外挂字幕与纯文本字幕没有它，为 `None`。
    ///
    /// 只喂给字幕引擎做样式表，不进快照 —— 契约里没有任何字段承载原始脚本。
    pub subtitle_script: Option<Vec<u8>>,
}

/// AV 时间基转换
pub fn av_time_to_seconds(ts: i64, time_base: ffmpeg_next::Rational) -> f64 {
    ts as f64 * time_base.numerator() as f64 / time_base.denominator() as f64
}

/// 构造时间基（num/den），避免下游 crate 直接依赖 ffmpeg-next 类型
pub fn time_base(num: i32, den: i32) -> ffmpeg_next::Rational {
    ffmpeg_next::Rational::new(num, den)
}

/// tick → 秒；时间基无效（分子或分母为 0）时返回 `None`
pub fn ticks_to_seconds(ticks: i64, time_base: ffmpeg_next::Rational) -> Option<f64> {
    if time_base.numerator() == 0 || time_base.denominator() == 0 {
        return None;
    }
    Some(av_time_to_seconds(ticks, time_base))
}

/// 秒 → tick；时间基无效时返回 `None`
pub fn seconds_to_ticks(secs: f64, time_base: ffmpeg_next::Rational) -> Option<i64> {
    if time_base.numerator() == 0 || time_base.denominator() == 0 {
        return None;
    }
    Some((secs * time_base.denominator() as f64 / time_base.numerator() as f64) as i64)
}

impl VideoFrame {
    /// 以秒表示的 PTS；无时间戳或时间基无效时为 `None`
    pub fn pts_secs(&self) -> Option<f64> {
        self.pts.and_then(|t| ticks_to_seconds(t, self.time_base))
    }

    /// 渲染用矩阵：帧上的 matrix → primaries → 分辨率，逐层回退
    ///
    /// 码流经常只写 primaries 不写 matrix，或两者都不写（老 DVD、多数 MP4）。
    /// 这时按 ITU-R BT.1358 的惯例：高清用 BT.709，标清用 BT.601。
    pub fn yuv_matrix(&self) -> YuvMatrix {
        match self.color_space {
            ColorSpace::Bt601 => YuvMatrix::Bt601,
            ColorSpace::Bt709 => YuvMatrix::Bt709,
            ColorSpace::Bt2020 => YuvMatrix::Bt2020,
            ColorSpace::Unspecified => match self.color_primaries {
                ColorPrimaries::Bt601 => YuvMatrix::Bt601,
                ColorPrimaries::Bt709 => YuvMatrix::Bt709,
                ColorPrimaries::Bt2020 => YuvMatrix::Bt2020,
                ColorPrimaries::Unspecified | ColorPrimaries::Other => {
                    if self.width >= 1280 || self.height >= 720 {
                        YuvMatrix::Bt709
                    } else {
                        YuvMatrix::Bt601
                    }
                }
            },
        }
    }

    /// 是否 full range（0..255）。YUV 内容未标注时按 limited（16..235），
    /// 与 FFmpeg / ITU-R 的默认一致
    pub fn is_full_range(&self) -> bool {
        matches!(self.color_range, ColorRange::Full)
    }
}

impl AudioFrame {
    /// 以秒表示的 PTS；无时间戳或时间基无效时为 `None`
    pub fn pts_secs(&self) -> Option<f64> {
        self.pts.and_then(|t| ticks_to_seconds(t, self.time_base))
    }
}

impl Packet {
    /// 以秒表示的 PTS；无时间戳或时间基无效时为 `None`
    pub fn pts_secs(&self) -> Option<f64> {
        self.pts.and_then(|t| ticks_to_seconds(t, self.time_base))
    }

    /// 以秒表示的 DTS；无时间戳或时间基无效时为 `None`
    pub fn dts_secs(&self) -> Option<f64> {
        self.dts.and_then(|t| ticks_to_seconds(t, self.time_base))
    }

    /// 以秒表示的持续时长；容器未给出或时间基无效时为 `None`
    pub fn duration_secs(&self) -> Option<f64> {
        self.duration
            .and_then(|d| ticks_to_seconds(d, self.time_base))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非方形像素必须改变显示宽度而不是显示高度：1080x1080 + SAR 9:16 是
    /// 竖屏视频的正确表达（ffprobe 的 DAR 同为 9:16），只按编码宽高摆放会变正方形
    #[test]
    fn display_size_applies_non_square_sar() {
        assert_eq!(display_size(1080, 1080, 9, 16), (608, 1080));
        assert_eq!(display_size(720, 576, 16, 15), (768, 576));
        assert_eq!(display_size(1920, 1080, 1, 1), (1920, 1080));
        // 0 分子 / 0 分母是 FFmpeg 表达“未知”的方式，必须退回编码尺寸
        assert_eq!(display_size(1920, 1080, 0, 1), (1920, 1080));
        assert_eq!(display_size(1920, 1080, 1, 0), (1920, 1080));
        assert_eq!(display_size(0, 0, 9, 16), (0, 0));
    }

    #[test]
    fn test_video_codec_id_conversion() {
        assert_eq!(
            VideoCodecId::from(ffmpeg_next::codec::Id::H264),
            VideoCodecId::H264
        );
        assert_eq!(
            VideoCodecId::from(ffmpeg_next::codec::Id::HEVC),
            VideoCodecId::H265
        );
        assert_eq!(
            VideoCodecId::from(ffmpeg_next::codec::Id::None),
            VideoCodecId::Unknown
        );
    }

    /// 真实 HEVC 流给出的是 `AV_CODEC_ID_HEVC`，它在 ffmpeg_next 里映射成
    /// `Id::HEVC`；只按名字猜（`Id::H265` 是反向别名）会让所有 HEVC 流退化成
    /// `Unknown`，必须从 ffi 枚举出发断言
    #[test]
    fn hevc_stream_id_maps_to_h265() {
        let from_stream =
            ffmpeg_next::codec::Id::from(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_HEVC);
        assert_eq!(from_stream, ffmpeg_next::codec::Id::HEVC);
        assert_eq!(VideoCodecId::from(from_stream), VideoCodecId::H265);
    }

    #[test]
    fn test_reverse_video_codec_id_conversion() {
        let id: ffmpeg_next::codec::Id = VideoCodecId::H264.into();
        assert_eq!(id, ffmpeg_next::codec::Id::H264);

        let hevc: ffmpeg_next::codec::Id = VideoCodecId::H265.into();
        assert_eq!(
            ffmpeg_next::ffi::AVCodecID::from(hevc),
            ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_HEVC
        );
    }

    #[test]
    fn test_audio_codec_id_conversion() {
        assert_eq!(
            AudioCodecId::from(ffmpeg_next::codec::Id::AAC),
            AudioCodecId::Aac
        );
        assert_eq!(
            AudioCodecId::from(ffmpeg_next::codec::Id::MP3),
            AudioCodecId::Mp3
        );
    }

    #[test]
    fn test_reverse_audio_codec_id_conversion() {
        let id: ffmpeg_next::codec::Id = AudioCodecId::Aac.into();
        assert_eq!(id, ffmpeg_next::codec::Id::AAC);
    }

    #[test]
    fn test_pixel_format_conversion() {
        assert_eq!(
            PixelFormat::from(ffmpeg_next::format::Pixel::YUV420P),
            PixelFormat::Yuv420p
        );
        assert_eq!(
            PixelFormat::from(ffmpeg_next::format::Pixel::NV12),
            PixelFormat::Nv12
        );
    }

    #[test]
    fn test_sample_format_conversion() {
        assert_eq!(
            SampleFormat::from(ffmpeg_next::format::Sample::F32(
                ffmpeg_next::format::sample::Type::Packed
            )),
            SampleFormat::F32
        );
        assert_eq!(
            SampleFormat::from(ffmpeg_next::format::Sample::F32(
                ffmpeg_next::format::sample::Type::Planar
            )),
            SampleFormat::F32Planar
        );
    }

    #[test]
    fn test_av_time_to_seconds() {
        let tb = ffmpeg_next::Rational::new(1, 1000);
        let secs = av_time_to_seconds(5000, tb);
        assert!((secs - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_av_time_to_seconds_zero() {
        let tb = ffmpeg_next::Rational::new(1, 90000);
        assert!((av_time_to_seconds(0, tb)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_reverse_pixel_format() {
        let p: ffmpeg_next::format::Pixel = PixelFormat::Yuv420p.into();
        assert_eq!(p, ffmpeg_next::format::Pixel::YUV420P);
        let p2: ffmpeg_next::format::Pixel = PixelFormat::Unknown.into();
        assert_eq!(p2, ffmpeg_next::format::Pixel::None);
    }

    #[test]
    fn test_subtitle_codec_id_conversion() {
        assert_eq!(
            SubtitleCodecId::from(ffmpeg_next::codec::Id::SUBRIP),
            SubtitleCodecId::SubRip
        );
        assert_eq!(
            SubtitleCodecId::from(ffmpeg_next::codec::Id::MOV_TEXT),
            SubtitleCodecId::MovText
        );
        assert_eq!(
            SubtitleCodecId::from(ffmpeg_next::codec::Id::None),
            SubtitleCodecId::Unknown
        );
    }

    /// 选轨只接受能解出文本的字幕；位图字幕（PGS/DVB/VOBSUB）不在此列
    #[test]
    fn only_text_subtitle_codecs_are_selectable() {
        for id in [
            ffmpeg_next::codec::Id::SUBRIP,
            ffmpeg_next::codec::Id::MOV_TEXT,
            ffmpeg_next::codec::Id::ASS,
            ffmpeg_next::codec::Id::SSA,
        ] {
            assert!(SubtitleCodecId::from(id).is_text(), "{id:?} 是文本字幕");
        }
        for id in [
            ffmpeg_next::codec::Id::DVD_SUBTITLE,
            ffmpeg_next::codec::Id::HDMV_PGS_SUBTITLE,
            ffmpeg_next::codec::Id::WEBVTT,
        ] {
            assert!(!SubtitleCodecId::from(id).is_text(), "{id:?} 不可选");
        }
        // SSA 与 ASS 共用对话行字段布局，走同一条解析路径
        assert!(SubtitleCodecId::from(ffmpeg_next::codec::Id::SSA).is_ass_like());
        assert!(!SubtitleCodecId::from(ffmpeg_next::codec::Id::SUBRIP).is_ass_like());
    }

    #[test]
    fn test_video_frame_construction() {
        let frame = VideoFrame {
            pts: Some(18432),
            time_base: ffmpeg_next::Rational::new(1, 12288),
            width: 1920,
            height: 1080,
            display_width: 1920,
            display_height: 1080,
            format: PixelFormat::Yuv420p,
            color_space: ColorSpace::Bt709,
            color_primaries: ColorPrimaries::Bt709,
            color_range: ColorRange::Limited,
            planes: vec![
                PlaneData {
                    data: vec![0; 1920 * 1080],
                    stride: 1920,
                },
                PlaneData {
                    data: vec![0; 960 * 540],
                    stride: 960,
                },
                PlaneData {
                    data: vec![0; 960 * 540],
                    stride: 960,
                },
            ],
        };
        assert_eq!(frame.planes.len(), 3);
        assert_eq!(frame.width, 1920);
        assert!((frame.pts_secs().unwrap() - 1.5).abs() < 0.001);
    }

    #[test]
    fn test_audio_frame_construction() {
        let frame = AudioFrame {
            pts: Some(4410),
            time_base: ffmpeg_next::Rational::new(1, 44100),
            sample_rate: 44100,
            channels: 2,
            format: SampleFormat::F32,
            samples: vec![0.0; 1024],
        };
        assert_eq!(frame.channels, 2);
        assert_eq!(frame.samples.len(), 1024);
        assert!((frame.pts_secs().unwrap() - 0.1).abs() < 0.001);
    }

    #[test]
    fn test_packet_construction() {
        let pkt = Packet {
            data: vec![1, 2, 3],
            stream_index: 0,
            pts: Some(6144),
            dts: Some(5120),
            duration: Some(1024),
            time_base: ffmpeg_next::Rational::new(1, 12288),
            is_key: true,
        };
        assert!(pkt.is_key);
        assert_eq!(pkt.data.len(), 3);
        assert!((pkt.pts_secs().unwrap() - 0.5).abs() < 0.001);
        assert!((pkt.dts_secs().unwrap() - 5120.0 / 12288.0).abs() < 0.001);
        assert!((pkt.duration_secs().unwrap() - 1024.0 / 12288.0).abs() < 0.001);
    }

    /// 回归：NOPTS 帧不能被兜底成 ts=0
    #[test]
    fn nopts_frame_has_no_seconds() {
        let frame = VideoFrame {
            pts: None,
            time_base: ffmpeg_next::Rational::new(1, 90000),
            width: 320,
            height: 240,
            display_width: 320,
            display_height: 240,
            format: PixelFormat::Yuv420p,
            color_space: ColorSpace::Unspecified,
            color_primaries: ColorPrimaries::Unspecified,
            color_range: ColorRange::Unspecified,
            planes: vec![],
        };
        assert_eq!(frame.pts, None);
        assert_eq!(frame.pts_secs(), None);

        let pkt = Packet {
            data: vec![0xAA],
            stream_index: 0,
            pts: None,
            dts: None,
            duration: None,
            time_base: ffmpeg_next::Rational::new(1, 90000),
            is_key: false,
        };
        assert_eq!(pkt.pts_secs(), None);
        assert_eq!(pkt.dts_secs(), None);
        assert_eq!(pkt.duration_secs(), None);
    }

    /// 真实 0 时间戳必须与 NOPTS 区分开
    #[test]
    fn zero_timestamp_is_not_nopts() {
        let frame = VideoFrame {
            pts: Some(0),
            time_base: ffmpeg_next::Rational::new(1, 90000),
            width: 320,
            height: 240,
            display_width: 320,
            display_height: 240,
            format: PixelFormat::Yuv420p,
            color_space: ColorSpace::Unspecified,
            color_primaries: ColorPrimaries::Unspecified,
            color_range: ColorRange::Unspecified,
            planes: vec![],
        };
        assert_eq!(frame.pts_secs(), Some(0.0));
    }

    /// 时间基无效（0/1，裸流常见）时不得产出 NaN/伪秒数
    #[test]
    fn invalid_time_base_yields_none() {
        let tb = ffmpeg_next::Rational::new(0, 1);
        assert_eq!(ticks_to_seconds(1000, tb), None);
        assert_eq!(seconds_to_ticks(1.0, tb), None);

        let pkt = Packet {
            data: vec![],
            stream_index: 0,
            pts: Some(1000),
            dts: Some(1000),
            duration: Some(500),
            time_base: tb,
            is_key: false,
        };
        assert_eq!(pkt.pts_secs(), None);
        assert_eq!(pkt.dts_secs(), None);
        assert_eq!(pkt.duration_secs(), None);
    }

    #[test]
    fn ticks_seconds_roundtrip() {
        let tb = ffmpeg_next::Rational::new(1, 15360);
        let secs = ticks_to_seconds(1536, tb).unwrap();
        assert!((secs - 0.1).abs() < 1e-9);
        assert_eq!(seconds_to_ticks(secs, tb), Some(1536));
    }

    #[test]
    fn test_pixel_format_serde_roundtrip() {
        let fmt = PixelFormat::Nv12;
        let json = serde_json::to_string(&fmt).unwrap();
        let fmt2: PixelFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(fmt, fmt2);
    }

    #[test]
    fn test_video_codec_id_serde_roundtrip() {
        let id = VideoCodecId::H264;
        let json = serde_json::to_string(&id).unwrap();
        let id2: VideoCodecId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn test_unknown_codec_fallback() {
        let id = VideoCodecId::from(ffmpeg_next::codec::Id::MPEG4);
        assert_eq!(id, VideoCodecId::Unknown);
    }

    fn frame_with(
        width: u32,
        height: u32,
        space: ColorSpace,
        primaries: ColorPrimaries,
        range: ColorRange,
    ) -> VideoFrame {
        VideoFrame {
            pts: None,
            time_base: time_base(1, 90000),
            width,
            height,
            display_width: width,
            display_height: height,
            format: PixelFormat::Yuv420p,
            color_space: space,
            color_primaries: primaries,
            color_range: range,
            planes: vec![],
        }
    }

    /// 帧上写了 matrix 就直接用，不被 primaries 或分辨率覆盖
    #[test]
    fn yuv_matrix_prefers_frame_matrix() {
        let f = frame_with(
            1920,
            1080,
            ColorSpace::Bt601,
            ColorPrimaries::Bt709,
            ColorRange::Limited,
        );
        assert_eq!(f.yuv_matrix(), YuvMatrix::Bt601);
    }

    /// 缺 matrix 时按 primaries 回退（HD 流常只写 primaries）
    #[test]
    fn yuv_matrix_falls_back_to_primaries() {
        let f = frame_with(
            640,
            360,
            ColorSpace::Unspecified,
            ColorPrimaries::Bt709,
            ColorRange::Unspecified,
        );
        assert_eq!(f.yuv_matrix(), YuvMatrix::Bt709);

        let f = frame_with(
            1920,
            1080,
            ColorSpace::Unspecified,
            ColorPrimaries::Bt2020,
            ColorRange::Unspecified,
        );
        assert_eq!(f.yuv_matrix(), YuvMatrix::Bt2020);
    }

    /// matrix 与 primaries 都没写时用分辨率启发式：高清 709、标清 601
    #[test]
    fn yuv_matrix_falls_back_to_resolution() {
        let hd = frame_with(
            1280,
            720,
            ColorSpace::Unspecified,
            ColorPrimaries::Unspecified,
            ColorRange::Unspecified,
        );
        assert_eq!(hd.yuv_matrix(), YuvMatrix::Bt709);

        let sd = frame_with(
            640,
            480,
            ColorSpace::Unspecified,
            ColorPrimaries::Unspecified,
            ColorRange::Unspecified,
        );
        assert_eq!(sd.yuv_matrix(), YuvMatrix::Bt601);
    }

    /// 未标注量程按 limited（与 FFmpeg / ITU-R 默认一致），只有明确标 JPEG 才是 full
    #[test]
    fn full_range_only_when_marked() {
        for range in [ColorRange::Unspecified, ColorRange::Limited] {
            let f = frame_with(640, 480, ColorSpace::Bt601, ColorPrimaries::Bt601, range);
            assert!(!f.is_full_range(), "{range:?} 应按 limited 解释");
        }
        let f = frame_with(
            640,
            480,
            ColorSpace::Bt601,
            ColorPrimaries::Bt601,
            ColorRange::Full,
        );
        assert!(f.is_full_range());
    }

    /// FFmpeg 色彩枚举到内部枚举的映射：未覆盖的一律 Unspecified，交给回退链
    #[test]
    fn ffmpeg_color_enums_map_to_internal() {
        use ffmpeg_next::color::{Primaries as FP, Range as FR, Space as FS};
        assert_eq!(ColorSpace::from(FS::BT709), ColorSpace::Bt709);
        assert_eq!(ColorSpace::from(FS::SMPTE170M), ColorSpace::Bt601);
        assert_eq!(ColorSpace::from(FS::BT470BG), ColorSpace::Bt601);
        assert_eq!(ColorSpace::from(FS::BT2020NCL), ColorSpace::Bt2020);
        assert_eq!(ColorSpace::from(FS::BT2020CL), ColorSpace::Bt2020);
        assert_eq!(ColorSpace::from(FS::SMPTE240M), ColorSpace::Unspecified);
        assert_eq!(ColorSpace::from(FS::YCGCO), ColorSpace::Unspecified);

        assert_eq!(
            ColorPrimaries::from(FP::BT470BG),
            ColorPrimaries::Bt601,
            "PAL primaries 属 BT.601 系"
        );
        assert_eq!(ColorPrimaries::from(FP::SMPTE432), ColorPrimaries::Other);
        assert_eq!(
            ColorPrimaries::from(FP::Unspecified),
            ColorPrimaries::Unspecified
        );

        assert_eq!(ColorRange::from(FR::MPEG), ColorRange::Limited);
        assert_eq!(ColorRange::from(FR::JPEG), ColorRange::Full);
        assert_eq!(ColorRange::from(FR::Unspecified), ColorRange::Unspecified);
    }

    /// shader_index 是与 WGSL `ColorParams.matrix` 的约定，取值不得改动
    #[test]
    fn shader_index_matches_wgsl_contract() {
        assert_eq!(YuvMatrix::Bt601.shader_index(), 0);
        assert_eq!(YuvMatrix::Bt709.shader_index(), 1);
        assert_eq!(YuvMatrix::Bt2020.shader_index(), 2);
    }
}
