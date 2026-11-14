//! 声道映射与采样率转换：两个输出后端共用的唯一实现。
//!
//! 这条路径此前是 `CpalOutput` 与 `AaudioOutput` 各自一份的线性插值，且声道
//! 映射是「取前 N 个源声道」（cpal 用 `c % src_ch`，AAudio 用 `ch.min(src_ch-1)`）。
//! 两种写法在**下混**上都是错的：
//!
//! - 5.1 → 立体声设备（Windows 上 `default_output_config()` 就是 2ch）只保留
//!   FL/FR，中置的人声与 LFE 被整条丢弃，表现为「没有对白」；
//! - 立体声 → 单声道只取左声道，右声道消失；
//! - 源声道少于设备声道时把 FL/FR 复制到错误的喇叭位置，而不是静音。
//!
//! 这里按「扬声器身份」建一张混音矩阵（ITU-R BS.775 的系数），再接一个带
//! 跨帧状态的多相 sinc 重采样器，最后过一级软限幅。下混必然抬高峰值
//! （5.1 三个满幅声道折到一个喇叭上是 2.41），没有限幅就是硬削波。
//!
//! 声道布局按声道数推断（见 [`default_layout`]），与 FFmpeg 的
//! `av_channel_layout_default` 同序。`AudioFrame` 目前不带 `AVChannelLayout`，
//! 对 5.1/7.1 这类标准布局这个推断完全等价；真遇到奇异布局（例如只有
//! FC+LFE 的 2ch）要等 `AudioFrame` 携带真实布局后才能区分。

use dyl_ffmpeg::AudioFrame;

/// 扬声器身份。顺序与 FFmpeg 的 `AVChannel` 一致，便于和容器元数据对照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    FrontLeft,
    FrontRight,
    FrontCenter,
    Lfe,
    BackLeft,
    BackRight,
    BackCenter,
    SideLeft,
    SideRight,
}

/// 按声道数推断默认布局（等价于 `av_channel_layout_default`）。
///
/// 超过 8 声道返回 `None`：那种布局没有公认的默认值，调用方退回按位置的
/// 逐声道对应，而不是猜。
pub fn default_layout(channels: usize) -> Option<&'static [Speaker]> {
    use Speaker::*;
    const MONO: &[Speaker] = &[FrontCenter];
    const STEREO: &[Speaker] = &[FrontLeft, FrontRight];
    const TWO_ONE: &[Speaker] = &[FrontLeft, FrontRight, Lfe];
    const QUAD: &[Speaker] = &[FrontLeft, FrontRight, BackLeft, BackRight];
    const FIVE_ZERO: &[Speaker] = &[FrontLeft, FrontRight, FrontCenter, BackLeft, BackRight];
    const FIVE_ONE: &[Speaker] = &[FrontLeft, FrontRight, FrontCenter, Lfe, BackLeft, BackRight];
    const SIX_ONE: &[Speaker] = &[
        FrontLeft,
        FrontRight,
        FrontCenter,
        Lfe,
        BackCenter,
        SideLeft,
        SideRight,
    ];
    const SEVEN_ONE: &[Speaker] = &[
        FrontLeft,
        FrontRight,
        FrontCenter,
        Lfe,
        BackLeft,
        BackRight,
        SideLeft,
        SideRight,
    ];
    match channels {
        1 => Some(MONO),
        2 => Some(STEREO),
        3 => Some(TWO_ONE),
        4 => Some(QUAD),
        5 => Some(FIVE_ZERO),
        6 => Some(FIVE_ONE),
        7 => Some(SIX_ONE),
        8 => Some(SEVEN_ONE),
        _ => None,
    }
}

/// −3 dB：ITU-R BS.775 对中置、环绕折入前左/前右的标准系数。
const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// 多声道 → 前左 / 前右 / 其余扬声器的折叠系数。
///
/// LFE 不参与折入（Dolby 与 FFmpeg 的默认行为一致）——它承载效果声道，
/// 折进立体声只会让低频糊掉。
///
/// 注意这里**不含**「折到中置」的规则：目标含中置不代表要下混到单声道，
/// 上混到 5.1 时把 FL 按 0.5 塞进 C 会让对白轨凭空多出一份串音。
fn fold_weight(dst: Speaker, src: Speaker) -> f32 {
    use Speaker::*;
    if dst == src {
        return 1.0;
    }
    match (dst, src) {
        (FrontLeft, FrontCenter) | (FrontRight, FrontCenter) => M3DB,
        (FrontLeft, BackLeft) | (FrontLeft, SideLeft) => M3DB,
        (FrontRight, BackRight) | (FrontRight, SideRight) => M3DB,
        (FrontLeft, BackCenter) | (FrontRight, BackCenter) => M3DB,
        _ => 0.0,
    }
}

/// 折到单声道时每个源扬声器的系数（0.5 / 0.354 为经验值：折完的听感电平
/// 与立体声接近）。
fn mono_weight(src: Speaker) -> f32 {
    use Speaker::*;
    match src {
        FrontCenter => 1.0,
        FrontLeft | FrontRight => 0.5,
        BackLeft | BackRight | SideLeft | SideRight => 0.354,
        BackCenter => 0.5,
        Lfe => 0.0,
    }
}

/// 单声道上混到多声道：目标有中置就放中置，否则复制到左右。
fn mono_upmix_weight(dst: Speaker, dst_layout: &[Speaker]) -> f32 {
    use Speaker::*;
    if dst_layout.contains(&FrontCenter) {
        if dst == FrontCenter {
            1.0
        } else {
            0.0
        }
    } else if dst == FrontLeft || dst == FrontRight {
        1.0
    } else {
        0.0
    }
}

/// 声道混音矩阵（`out_ch × in_ch`）。
#[derive(Debug, Clone)]
pub struct ChannelMix {
    in_ch: usize,
    out_ch: usize,
    coeffs: Vec<f32>,
    identity: bool,
}

impl ChannelMix {
    pub fn new(in_ch: usize, out_ch: usize) -> Self {
        let in_ch = in_ch.max(1);
        let out_ch = out_ch.max(1);
        let mut coeffs = vec![0.0f32; out_ch * in_ch];
        let mut identity = in_ch == out_ch;

        let src = default_layout(in_ch);
        let dst = default_layout(out_ch);
        if in_ch == out_ch {
            // 声道数相同即视为同一布局：位置一一对应，不做任何系数运算。
            // 这条捷径避免「5.0 源接 5.1 设备」被当成下混再上混。
            for c in 0..out_ch {
                coeffs[c * in_ch + c] = 1.0;
            }
        } else if let (Some(src), Some(dst)) = (src, dst) {
            let mono_src = in_ch == 1;
            let mono_dst = out_ch == 1;
            for (o, dst_sp) in dst.iter().enumerate() {
                for (i, src_sp) in src.iter().enumerate() {
                    let w = if mono_src {
                        mono_upmix_weight(*dst_sp, dst)
                    } else if mono_dst {
                        mono_weight(*src_sp)
                    } else {
                        fold_weight(*dst_sp, *src_sp)
                    };
                    coeffs[o * in_ch + i] = w;
                }
            }
            identity = false;
        } else {
            // 未知布局（>8 声道）：按位置一一对应，缺的补静音。绝不取模回绕。
            for c in 0..out_ch.min(in_ch) {
                coeffs[c * in_ch + c] = 1.0;
            }
            identity = false;
        }

        Self {
            in_ch,
            out_ch,
            coeffs,
            identity,
        }
    }

    pub fn in_ch(&self) -> usize {
        self.in_ch
    }

    pub fn out_ch(&self) -> usize {
        self.out_ch
    }

    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// 输出第 `o` 个声道的系数行。
    pub fn row(&self, o: usize) -> &[f32] {
        &self.coeffs[o * self.in_ch..(o + 1) * self.in_ch]
    }

    /// 单个输出声道可能出现的峰值增益（系数绝对值之和的最大值）。
    pub fn worst_case_gain(&self) -> f32 {
        (0..self.out_ch)
            .map(|o| self.row(o).iter().map(|c| c.abs()).sum::<f32>())
            .fold(0.0f32, f32::max)
    }
}

// ============================================================================
// 软限幅
// ============================================================================

/// 软限幅拐点：−1 dBFS。低于它的采样逐位透传，高于它的按 tanh 压到 1.0 以内。
const LIMIT_KNEE: f32 = 0.891_251;

/// 软限幅：|x| ≤ 拐点时恒等，之上平滑地趋近 1.0。
///
/// 硬 `clamp` 会把越界采样削成方波，听感是刺耳的失真；这里超过拐点的部分
/// 走 tanh 曲线，转折处一阶连续，满幅 5.1 折成立体声时最多压掉 1 dB。
pub fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= LIMIT_KNEE {
        return x;
    }
    let span = 1.0 - LIMIT_KNEE;
    let y = LIMIT_KNEE + span * ((a - LIMIT_KNEE) / span).tanh();
    y.copysign(x)
}

// ============================================================================
// 重采样
// ============================================================================

/// 单边抽头数。核长 `2 * TAPS = 64`，Kaiser β=8 下过渡带约 ±1.5 kHz
/// （48 kHz 采样），阻带在过渡带之外约 −70 dB。
const TAPS: usize = 32;
/// 相位表分辨率：系数在小数相位之间线性插值。
const PHASES: usize = 256;
const KAISER_BETA: f64 = 8.0;
/// 保留的历史帧数。卷积右侧还需要 `TAPS` 帧上下文，2 倍留出余量。
const HISTORY: usize = TAPS * 2;

/// 零阶修正贝塞尔函数（Kaiser 窗用）。
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    let half = x / 2.0;
    for k in 1..=32 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

/// 归一化 sinc，`sinc(0) = 1`。
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

/// 位置 → 本块可产出的输出帧数。
///
/// 输出位置 `p` 需要源索引 `[floor(p) - TAPS + 1, floor(p) + TAPS]`。右侧
/// 上下文在块内最多到 `n - 1`，所以 `floor(p) <= n - TAPS - 1`，即
/// `p < n - TAPS`。最后 `TAPS` 帧留给下一块——那里的右侧上下文才是真实的，
/// 不能拿零补齐（逐帧补零正是旧实现每帧末尾衰减的成因）。
///
/// `frames_for` 与 `convert` 共用这一个公式，差一帧就会累积成音画漂移。
fn frames_at(pos: f64, step: f64, src_frames: usize) -> usize {
    let limit = src_frames as f64 - TAPS as f64 - pos;
    if limit <= 0.0 {
        0
    } else {
        (limit / step).ceil() as usize
    }
}

/// 多相窗 sinc 重采样器，支持任意比例（含变速）与跨帧状态。
///
/// 比例 = 输出采样数 / 源采样数。播放速率通过比例表达：2× 时比例为一半，
/// 相当于降采样，音调随之升高（这是变速的预期行为，不是缺陷）。
pub struct Resampler {
    channels: usize,
    /// 每个输出采样前进的源采样数
    step: f64,
    /// 归一化到源 Nyquist 的截止频率
    cutoff: f64,
    /// `(PHASES + 1) * 2 * TAPS` 的系数表
    table: Vec<f32>,
    /// 下一个输出采样在 `hist ++ input` 坐标系里的位置
    pos: f64,
    /// 最近 `HISTORY` 个源帧（交错，`channels` 声道）
    hist: Vec<f32>,
    /// 复用的组合缓冲（历史 + 本块），避免音频路径逐帧分配
    combined: Vec<f32>,
    weights: Vec<f32>,
}

impl std::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resampler")
            .field("channels", &self.channels)
            .field("step", &self.step)
            .field("cutoff", &self.cutoff)
            .field("pos", &self.pos)
            .finish()
    }
}

impl Resampler {
    pub fn new(channels: usize) -> Self {
        let channels = channels.max(1);
        let mut r = Self {
            channels,
            step: 1.0,
            cutoff: 1.0,
            table: Vec::new(),
            pos: 0.0,
            hist: vec![0.0; HISTORY * channels],
            combined: Vec::new(),
            weights: vec![0.0; 2 * TAPS],
        };
        r.rebuild(1.0);
        r
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// 比例 = 输出采样数 / 源采样数。
    pub fn set_ratio(&mut self, ratio: f64) {
        let ratio = if ratio.is_finite() && ratio > 0.0 {
            ratio
        } else {
            1.0
        };
        let step = 1.0 / ratio;
        if (step - self.step).abs() < 1e-12 {
            return;
        }
        self.rebuild(step);
    }

    pub fn ratio(&self) -> f64 {
        1.0 / self.step
    }

    /// 比例恰为 1 且无历史残留时可以逐位透传。
    pub fn is_passthrough(&self) -> bool {
        (self.step - 1.0).abs() < 1e-9 && self.pos.abs() < 1e-9
    }

    /// 清空跨帧状态（seek / 换轨）。不清理会让新位置的音频带着旧位置的尾巴。
    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.hist.iter_mut().for_each(|s| *s = 0.0);
    }

    fn rebuild(&mut self, step: f64) {
        self.step = step;
        // 降采样时把截止频率压到新 Nyquist 以内，否则镜像会折回可闻频段。
        self.cutoff = if step > 1.0 { 0.95 / step } else { 0.95 };
        self.build_table();
    }

    fn build_table(&mut self) {
        let rows = PHASES + 1;
        self.table.clear();
        self.table.reserve(rows * 2 * TAPS);
        let i0 = bessel_i0(KAISER_BETA);
        for p in 0..rows {
            let frac = p as f64 / PHASES as f64;
            let base = self.table.len();
            for m in 0..2 * TAPS {
                let k = m as f64 - TAPS as f64;
                let u = frac + k;
                let w = if u.abs() >= TAPS as f64 {
                    0.0
                } else {
                    let r = u / TAPS as f64;
                    let window = bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / i0;
                    self.cutoff * sinc(self.cutoff * u) * window
                };
                self.table.push(w as f32);
            }
            // 逐相位归一化到直流增益 1：截断的窗 sinc 直接求和会差 2~3%
            // （表现为整体音量偏低），归一把每个小数相位都拉回单位增益。
            let sum: f32 = self.table[base..].iter().sum();
            if sum.abs() > 1e-6 {
                let scale = 1.0 / sum;
                for w in &mut self.table[base..] {
                    *w *= scale;
                }
            }
        }
    }

    /// 给定本块源帧数，`convert` 会产出多少个输出帧。
    pub fn frames_for(&self, src_frames: usize) -> usize {
        frames_at(self.pos, self.step, src_frames)
    }

    /// 重采样一块源帧。`input` 为 `src_frames * channels` 的交错采样。
    ///
    /// `pos` 是本块首帧相对坐标（0 = `input[0]`），可以略小于 0：上一块末尾
    /// 那些缺右侧上下文的输出会带着负位置留到本块。返回值即实际写入 `out`
    /// 的帧数，与 [`Self::frames_for`] 相等。
    pub fn convert(&mut self, input: &[f32], src_frames: usize, out: &mut Vec<f32>) -> usize {
        out.clear();
        if src_frames == 0 {
            return 0;
        }
        let ch = self.channels;
        let out_frames = self.frames_for(src_frames);

        // 组合缓冲：历史 + 本块。本块 input[0] 落在索引 HISTORY 上，
        // 于是源索引 j 对应组合索引 j + HISTORY。复用同一个 Vec，避免
        // 实时路径上的分配抖动。
        let total = HISTORY + src_frames;
        self.combined.clear();
        self.combined.reserve(total * ch);
        self.combined.extend_from_slice(&self.hist);
        let take = (src_frames * ch).min(input.len());
        self.combined.extend_from_slice(&input[..take]);
        // input 短于声明长度时补静音，保持索引运算的边界简单
        self.combined.resize(total * ch, 0.0);

        if out_frames > 0 {
            out.reserve(out_frames * ch);
            let mut pos = self.pos;
            for _ in 0..out_frames {
                let center = pos.floor();
                let frac = pos - center;
                self.weights(frac);
                let c = center as isize;
                for o in 0..ch {
                    let mut acc = 0.0f64;
                    // 抽头源索引 i = c + (m - TAPS)；组合索引 = i + HISTORY。
                    // 越界即落在窗函数边缘（权重已衰减到 0），跳过。
                    for m in 0..2 * TAPS {
                        let i = c + (m as isize - TAPS as isize);
                        let idx = i + HISTORY as isize;
                        if idx < 0 {
                            continue;
                        }
                        let idx = idx as usize;
                        if idx >= total {
                            break;
                        }
                        acc += self.weights[m] as f64 * self.combined[idx * ch + o] as f64;
                    }
                    out.push(acc as f32);
                }
                pos += self.step;
            }
            // 坐标系平移：下一块从本块之后开始，位置减去本块长度
            self.pos = pos - src_frames as f64;
        } else {
            self.pos -= src_frames as f64;
        }

        // 历史 = 组合缓冲的最后 HISTORY 帧（恒为新块首帧之前的 HISTORY 帧）
        let start = (total - HISTORY) * ch;
        self.hist
            .copy_from_slice(&self.combined[start..start + HISTORY * ch]);

        out_frames
    }

    fn weights(&mut self, frac: f64) {
        let x = frac * PHASES as f64;
        let base = (x.floor() as usize).min(PHASES - 1);
        let t = (x - base as f64) as f32;
        let len = 2 * TAPS;
        let a = base * len;
        let b = a + len;
        for m in 0..len {
            let lo = self.table[a + m];
            let hi = self.table[b + m];
            self.weights[m] = lo + (hi - lo) * t;
        }
    }
}

// ============================================================================
// 组合：映射 → 重采样 → 限幅
// ============================================================================

/// 源帧 → 设备格式的完整转换。
///
/// 进程内只有一份状态机，两个后端只是把结果写进各自的设备缓冲。
#[derive(Debug)]
pub struct AudioConverter {
    src_rate: u32,
    dst_rate: u32,
    mix: ChannelMix,
    resampler: Resampler,
    speed_milli: u32,
    /// 混音矩阵最坏情况下会越过满幅，才需要限幅。
    ///
    /// 上混（2 → 6）与折成单声道的峰值增益都恰好是 1，此时必须逐位透传：
    /// 无条件限幅会把满幅素材整体压低 0.26 dB。
    limit: bool,
    /// 混音后的中间缓冲（复用，避免音频路径上每帧分配）
    mixed: Vec<f32>,
}

impl AudioConverter {
    pub fn new(src_rate: u32, src_ch: u32, dst_rate: u32, dst_ch: u32) -> Self {
        let src_ch = src_ch.max(1) as usize;
        let dst_ch = dst_ch.max(1) as usize;
        let mix = ChannelMix::new(src_ch, dst_ch);
        let limit = mix.worst_case_gain() > 1.0 + 1e-6;
        Self {
            src_rate,
            dst_rate,
            mix,
            resampler: Resampler::new(dst_ch),
            speed_milli: 1000,
            limit,
            mixed: Vec::new(),
        }
    }

    pub fn mix(&self) -> &ChannelMix {
        &self.mix
    }

    /// 下混的最大峰值增益（>1 时说明依赖软限幅兜底）。
    pub fn worst_case_gain(&self) -> f32 {
        self.mix.worst_case_gain()
    }

    /// 输出采样数 / 源采样数。
    fn ratio(&self, speed_milli: u32) -> f64 {
        let speed = (speed_milli.max(1) as f64) / 1000.0;
        let rate = if self.src_rate == 0 {
            1.0
        } else {
            self.dst_rate as f64 / self.src_rate as f64
        };
        rate / speed
    }

    /// 逐位透传的条件：布局相同、比例 1、且重采样器没有历史相位。
    ///
    /// `frames_for` 与 `convert` 必须用同一个判据，否则背压记账会和实际
    /// 写入错位——主时钟靠这个数走。
    fn fast_path(&self, ratio: f64) -> bool {
        self.mix.is_identity() && (ratio - 1.0).abs() < 1e-9 && self.resampler.is_passthrough()
    }

    /// 本帧将产出多少设备帧。
    pub fn frames_for(&self, src_frames: usize, speed_milli: u32) -> usize {
        let ratio = self.ratio(speed_milli);
        if self.fast_path(ratio) {
            return src_frames;
        }
        frames_at(self.resampler.pos, 1.0 / ratio, src_frames)
    }

    /// 清空跨帧状态与中间缓冲（seek / 换轨）。
    pub fn reset(&mut self) {
        self.resampler.reset();
        self.mixed.clear();
    }

    /// 转换一帧，结果写入 `out`（交错、`dst_ch` 声道），返回帧数。
    pub fn convert(&mut self, frame: &AudioFrame, speed_milli: u32, out: &mut Vec<f32>) -> usize {
        let src_ch = self.mix.in_ch();
        let dst_ch = self.mix.out_ch();
        let src_frames = frame.samples.len() / src_ch;
        out.clear();
        if src_frames == 0 {
            return 0;
        }

        if !self.mix.is_identity() {
            self.mixed.clear();
            self.mixed.reserve(src_frames * dst_ch);
            for i in 0..src_frames {
                let base = i * src_ch;
                for o in 0..dst_ch {
                    let mut acc = 0.0f32;
                    for (c, w) in self.mix.row(o).iter().enumerate() {
                        if *w != 0.0 {
                            acc += frame.samples[base + c] * w;
                        }
                    }
                    self.mixed.push(acc);
                }
            }
        }

        let ratio = self.ratio(speed_milli);
        if self.fast_path(ratio) {
            out.extend_from_slice(&frame.samples[..src_frames * src_ch]);
            return src_frames;
        }
        if speed_milli != self.speed_milli {
            self.speed_milli = speed_milli;
        }
        self.resampler.set_ratio(ratio);

        let produced = if self.mix.is_identity() {
            self.resampler
                .convert(&frame.samples[..src_frames * src_ch], src_frames, out)
        } else {
            self.resampler.convert(&self.mixed, src_frames, out)
        };
        // 限幅只在链条末端做一次：混音阶段已经压过一遍的话，满幅信号会被
        // 压两次（0.26 + 0.26 dB），5.1 折立体声的素材听起来会莫名偏小。
        if self.limit {
            for s in out.iter_mut() {
                *s = soft_limit(*s);
            }
        }
        produced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(rate: u32, channels: u32, samples: Vec<f32>) -> AudioFrame {
        AudioFrame {
            pts: Some(0),
            time_base: dyl_ffmpeg::time_base(1, rate as i32),
            sample_rate: rate,
            channels,
            format: dyl_ffmpeg::SampleFormat::F32,
            samples,
        }
    }

    /// `frames` 帧的常量块：每帧的声道内容都是 `per_frame`。
    fn flat(rate: u32, per_frame: &[f32], frames: usize) -> AudioFrame {
        let mut s = Vec::with_capacity(frames * per_frame.len());
        for _ in 0..frames {
            s.extend_from_slice(per_frame);
        }
        frame(rate, per_frame.len() as u32, s)
    }

    fn rms(v: &[f32]) -> f64 {
        (v.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
    }

    // ---------- 声道映射 ----------

    #[test]
    fn default_layout_matches_ffmpeg_order() {
        assert_eq!(default_layout(6).unwrap()[2], Speaker::FrontCenter);
        assert_eq!(default_layout(6).unwrap()[3], Speaker::Lfe);
        assert_eq!(default_layout(8).unwrap()[7], Speaker::SideRight);
        assert!(default_layout(12).is_none());
    }

    /// 5.1 → 立体声：中置（对白）与环绕必须折入，LFE 必须不折入。
    #[test]
    fn five_one_downmix_matrix() {
        let mix = ChannelMix::new(6, 2);
        assert_eq!(mix.row(0), &[1.0, 0.0, M3DB, 0.0, M3DB, 0.0]);
        assert_eq!(mix.row(1), &[0.0, 1.0, M3DB, 0.0, 0.0, M3DB]);
        assert!((mix.worst_case_gain() - (1.0 + 2.0 * M3DB)).abs() < 1e-6);
    }

    /// 立体声 → 单声道：左右都要按 0.5 计入。
    ///
    /// 旧实现只取前 N 个源声道，右声道会整条丢掉。
    #[test]
    fn stereo_to_mono_downmix_matrix() {
        let mix = ChannelMix::new(2, 1);
        assert_eq!(mix.row(0), &[0.5, 0.5]);
    }

    /// 上混：源里没有的喇叭必须静音，不能取模回绕到 FL/FR。
    #[test]
    fn upmix_leaves_extra_speakers_silent() {
        let mix = ChannelMix::new(2, 6);
        assert_eq!(mix.row(0), &[1.0, 0.0]); // FL
        assert_eq!(mix.row(1), &[0.0, 1.0]); // FR
        for o in 2..6 {
            assert_eq!(mix.row(o), &[0.0, 0.0], "第 {o} 个喇叭应为静音");
        }
    }

    /// 单声道上混：左右各 1.0（与既有双写行为一致，避免音量突变）。
    #[test]
    fn mono_upmix_duplicates_at_unity() {
        let mix = ChannelMix::new(1, 2);
        assert_eq!(mix.row(0), &[1.0]);
        assert_eq!(mix.row(1), &[1.0]);
        // 目标有中置时放进中置，而不是复制到左右
        let mix = ChannelMix::new(1, 6);
        assert_eq!(mix.row(2), &[1.0]);
        assert_eq!(mix.row(0), &[0.0]);
    }

    #[test]
    fn same_channel_count_is_identity() {
        let mix = ChannelMix::new(6, 6);
        assert!(mix.is_identity(), "声道数相同不应触发混音");
        let mix = ChannelMix::new(2, 2);
        assert!(mix.is_identity());
    }

    /// 未知布局（>8 声道）按位置对应，缺的补静音。
    #[test]
    fn unknown_layout_maps_positionally() {
        let mix = ChannelMix::new(12, 2);
        assert_eq!(
            mix.row(0),
            &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(mix.row(1)[1], 1.0);
    }

    // ---------- 软限幅 ----------

    #[test]
    fn soft_limit_is_transparent_below_knee() {
        for x in [-0.5f32, -0.1, 0.0, 0.3, LIMIT_KNEE] {
            assert_eq!(soft_limit(x), x, "拐点以下必须逐位透传");
        }
        assert!(soft_limit(1.2) < 1.0, "越过拐点后被压进满幅以内");
        assert!(soft_limit(2.4) <= 1.0, "极端峰值最多顶到满幅，不越界");
        assert!(soft_limit(-1.2) > -1.0);
    }

    /// 满幅 5.1 折成立体声：峰值 2.41，必须被限幅压住而不是硬削波。
    #[test]
    fn full_scale_five_one_is_limited_not_clipped() {
        let mut conv = AudioConverter::new(48000, 6, 48000, 2);
        let mut out = Vec::new();
        conv.convert(&flat(48000, &[1.0; 6], 512), 1000, &mut out);
        for v in &out {
            assert!(v.abs() <= 1.0, "下混后越过满幅会被硬削波：{v}");
        }
        assert!(
            out.iter().skip(200).all(|v| *v > 0.5),
            "限幅不应把满幅信号压得听不见"
        );
    }

    // ---------- 端到端：下混 ----------

    /// 5.1 只有中置有声（对白轨的典型情形）时，立体声输出必须非静音。
    ///
    /// 旧实现只取 FL/FR，这一帧会输出全零 —— 也就是「没有对白」。
    #[test]
    fn center_only_frame_survives_downmix() {
        let mut conv = AudioConverter::new(48000, 6, 48000, 2);
        let mut out = Vec::new();
        conv.convert(
            &flat(48000, &[0.0, 0.0, 0.8, 0.0, 0.0, 0.0], 512),
            1000,
            &mut out,
        );
        // 跳过启动瞬态
        let tail = &out[200..];
        assert!(
            tail.iter().all(|v| (*v - 0.8 * M3DB).abs() < 1e-3),
            "中置应折入左右各 −3 dB，got {:?}",
            &tail[..4]
        );
    }

    /// 上混到 6 声道时，源里没有的喇叭必须静音。旧实现 c % src_ch 会把
    /// FL/FR 复制到 BL/BR。
    #[test]
    fn upmix_extra_channels_stay_silent_end_to_end() {
        let mut conv = AudioConverter::new(48000, 2, 48000, 6);
        let mut out = Vec::new();
        conv.convert(&flat(48000, &[1.0, -1.0], 512), 1000, &mut out);
        let frame0 = &out[200 * 6..200 * 6 + 6];
        assert!(
            (frame0[0] - 1.0).abs() < 1e-4 && (frame0[1] + 1.0).abs() < 1e-4,
            "前左/前右直通：{frame0:?}"
        );
        assert!(
            frame0[2..].iter().all(|v| v.abs() < 1e-3),
            "其余喇叭应为静音：{frame0:?}"
        );
    }

    #[test]
    fn mono_to_stereo_duplicates() {
        let mut conv = AudioConverter::new(48000, 1, 48000, 2);
        let mut out = Vec::new();
        conv.convert(&flat(48000, &[0.25], 512), 1000, &mut out);
        let f = &out[200 * 2..200 * 2 + 2];
        assert!(
            (f[0] - 0.25).abs() < 1e-3 && (f[1] - 0.25).abs() < 1e-3,
            "{f:?}"
        );
    }

    // ---------- 重采样 ----------

    /// 频响：44.1 → 48 kHz 的 11 kHz 正弦必须原样通过。
    ///
    /// 线性插值在这里是 −1.8 dB（`sinc²` 滚降），足以把这条断言打红。
    #[test]
    fn passband_is_flat_on_upsampling() {
        let src_rate = 44_100u32;
        let dst_rate = 48_000u32;
        let freq = 11_000.0f64;
        let samples: Vec<f32> = (0..src_rate)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / src_rate as f64).sin() as f32)
            .collect();

        let mut conv = AudioConverter::new(src_rate, 1, dst_rate, 1);
        let mut all = Vec::new();
        let mut out = Vec::new();
        for chunk in samples.chunks(1024) {
            conv.convert(&frame(src_rate, 1, chunk.to_vec()), 1000, &mut out);
            all.extend_from_slice(&out);
        }
        let steady = &all[4096..all.len().saturating_sub(1024)];
        let db = 20.0 * (rms(steady) / (1.0 / std::f64::consts::SQRT_2)).log10();
        assert!(
            db.abs() < 0.5,
            "11 kHz 通带偏差 {db:.2} dB（线性插值约为 −1.8 dB）"
        );
    }

    /// 抗镜像：48 → 44.1 kHz 时，23 kHz 输入（高于目标 Nyquist）必须被滤掉。
    #[test]
    fn out_of_band_tone_is_filtered_when_downsampling() {
        let src_rate = 48_000u32;
        let dst_rate = 44_100u32;
        let freq = 23_000.0f64;
        let samples: Vec<f32> = (0..src_rate)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / src_rate as f64).sin() as f32)
            .collect();

        let mut conv = AudioConverter::new(src_rate, 1, dst_rate, 1);
        let mut all = Vec::new();
        let mut out = Vec::new();
        for chunk in samples.chunks(1024) {
            conv.convert(&frame(src_rate, 1, chunk.to_vec()), 1000, &mut out);
            all.extend_from_slice(&out);
        }
        let steady = &all[4096..all.len().saturating_sub(1024)];
        let db = 20.0 * (rms(steady) / (1.0 / std::f64::consts::SQRT_2)).log10();
        assert!(db < -40.0, "23 kHz 未被滤除：{db:.1} dB");
    }

    /// 常量输入必须收敛到同一常量（直流增益 1）。逐相位归一化没做到就会
    /// 整体音量偏低 —— 这是窗 sinc 截断的典型症状。
    #[test]
    fn unity_dc_gain() {
        let mut r = Resampler::new(1);
        let mut out = Vec::new();
        r.convert(&vec![0.5f32; 512], 512, &mut out);
        let steady = &out[200..];
        assert!(
            steady.iter().all(|v| (v - 0.5).abs() < 1e-5),
            "直流增益偏离 1：{:?}",
            &steady[..4]
        );
    }

    /// `frames_for` 必须与实际产出逐块一致，否则主时钟会漂移。
    #[test]
    fn frames_for_matches_actual_output() {
        let mut conv = AudioConverter::new(44_100, 2, 48_000, 2);
        let mut predicted = 0usize;
        let mut produced = 0usize;
        for _ in 0..200 {
            let f = frame(44_100, 2, vec![0.1; 1024 * 2]);
            predicted += conv.frames_for(1024, 1000);
            let mut out = Vec::new();
            produced += conv.convert(&f, 1000, &mut out);
        }
        assert_eq!(predicted, produced, "背压预测与实际产出一致");
        let expected = 200.0 * 1024.0 * 48_000.0 / 44_100.0;
        let drift = (produced as f64 - expected) / expected;
        assert!(
            drift.abs() < 0.002,
            "200 块后累计偏差 {:.4}%（应 < 0.2%）",
            drift * 100.0
        );
    }

    #[test]
    fn speed_changes_output_length() {
        let mut conv = AudioConverter::new(48_000, 2, 48_000, 2);
        let mut out = Vec::new();
        let mut total = 0usize;
        for _ in 0..4 {
            let f = frame(48_000, 2, vec![0.25; 4800 * 2]);
            total += conv.convert(&f, 2000, &mut out); // 2×：一半的帧
        }
        // 4 × 4800 源帧 @2× = 9600 设备帧，误差只有右侧上下文预留
        assert!(
            (total as i64 - 9600).abs() <= 40,
            "2× 变速应产出约一半设备帧，got {total}"
        );
    }

    #[test]
    fn reset_clears_cross_frame_state() {
        let mut conv = AudioConverter::new(48_000, 2, 44_100, 2);
        let f = frame(48_000, 2, vec![0.5; 1024 * 2]);
        let mut out = Vec::new();
        conv.convert(&f, 1000, &mut out);
        conv.reset();
        let mut after = Vec::new();
        conv.convert(&f, 1000, &mut after);
        assert!(!after.is_empty(), "reset 后仍应正常产出");
    }
}
