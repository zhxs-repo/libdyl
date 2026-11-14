// YUV420P/NV12 → RGB 转换 shader
// 矩阵系数与量程由 uniform 提供：BT.601 / BT.709 / BT.2020 与 limited/full range
// 写死一组系数会让 HD 内容整体偏色（肤色过红）
//
// SDR 帧在矩阵之后直接上屏（与加 HDR 支持之前逐字节相同）。PQ 帧（HDR10）
// 先做 ST 2084 EOTF 还原线性光，按 BT.2390 EETF 把内容峰值压到 100 nit SDR
// 目标，再转 BT.709 色域并做 709 OETF——少了这几步，PQ 编码值会被当成 gamma
// 值直接显示，画面整体发灰。

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // 全屏三角形 strip（4 个顶点）
    var pos = array<vec2<f32>, 4>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var uv = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
    );
    var out: VertexOutput;
    out.position = vec4<f32>(pos[vertex_index], 0.0, 1.0);
    out.uv = uv[vertex_index];
    return out;
}

@group(0) @binding(0) var y_texture: texture_2d<f32>;
@group(0) @binding(1) var uv_texture: texture_2d<f32>;
@group(0) @binding(2) var yuv_sampler: sampler;

/// 色彩参数 — 与 Rust 侧 `ColorParams` 的 32 字节布局一致
struct ColorParams {
    matrix: u32,     // 0 = BT.601, 1 = BT.709, 2 = BT.2020
    full_range: u32, // 非 0 = full range(0..255)，否则 limited(16..235)
    transfer: u32,   // 0 = SDR（gamma 直通）, 1 = PQ（ST 2084）
    primaries: u32,  // 0 = 其它/未标注, 2 = BT.2020（PQ 路径下做 2020→709）
    /// 内容峰值映射到显示端的电平（nit，相对 SDR 白；默认 85 = 留 15% 余量）
    dst_peak_nits: f32,
    /// 无静态元数据时假定的内容峰值（默认 1000）
    src_peak_nits: f32,
    _pad0: f32,
    _pad1: f32,
};

@group(0) @binding(3) var<uniform> color: ColorParams;

/// SDR 白的线性亮度：100 nit / 10000，即归一化的固定基准。
///
/// 它必须是常量。早先把 `dst_peak_nits` 同时当作"压缩终点"和"归一化点"，
/// 于是 scale ∝ 1/pq_eotf(dst_peak)：调高目标峰值反而让整幅画变暗，
/// 且内容峰值恒被顶到 255 —— 300 nit 以上全部挤在最后几个码值里，
/// 高光糊成一片。归一化与压缩终点必须分开。
const SDR_WHITE_LINEAR: f32 = 0.01;

// HDR→SDR：没有静态元数据时按 `src_peak_nits` 的内容峰值，用 BT.2390 EETF
// 压到 `dst_peak_nits` 的显示电平，再按 SDR 白归一。

/// BT.709 OETF — 线性光 → 显示编码值
fn oetf_709(l: f32) -> f32 {
    let v = clamp(l, 0.0, 1.0);
    if (v < 0.018) {
        return 4.5 * v;
    }
    return 1.099 * pow(v, 0.45) - 0.099;
}

/// PQ EOTF（ST 2084）单通道 — 编码值(0..1) → 绝对亮度(0..1 = 0..10000 nit)
fn pq_eotf(e: f32) -> f32 {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = pow(max(e, 0.0), 1.0 / m2);
    return pow(max(p - c1, 0.0) / (c2 - c3 * p), 1.0 / m1);
}

/// PQ OETF（ST 2084）单通道 — 绝对亮度(0..1 = 0..10000 nit) → 编码值
fn pq_oetf(l: f32) -> f32 {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = pow(clamp(l, 0.0, 1.0), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

/// BT.2390 EETF — PQ 域亮度压缩
///
/// `knee` 以下恒等，以上用三次 Hermite 压到 `dst_peak`：起点值与斜率承接
/// 线性段（C1 连续），终点值为目标峰值、斜率为 0。
fn eetf(e: f32, src_peak: f32, dst_peak: f32) -> f32 {
    let knee = 1.5 * dst_peak - 0.5 * src_peak;
    if (e <= knee) {
        return e;
    }
    let t = clamp((e - knee) / (src_peak - knee), 0.0, 1.0);
    let t2 = t * t;
    let t3 = t2 * t;
    let p = (2.0 * t3 - 3.0 * t2 + 1.0) * knee
        + (t3 - 2.0 * t2 + t) * (src_peak - knee)
        + (-2.0 * t3 + 3.0 * t2) * dst_peak;
    return min(p, dst_peak);
}

/// BT.2020 → BT.709 线性色域转换（BT.2087 系数，列主序）
const BT2020_TO_709: mat3x3<f32> = mat3x3<f32>(
    vec3<f32>( 1.6605, -0.1246, -0.0182),
    vec3<f32>(-0.5876,  1.1329, -0.1006),
    vec3<f32>(-0.0728, -0.0083,  1.1187),
);

/// 矩阵系数来源的 Kr / Kb（ITU-R BT.601 / 709 / 2020）
fn kr_kb(matrix: u32) -> vec2<f32> {
    switch matrix {
        case 1u: { return vec2<f32>(0.2126, 0.0722); }
        case 2u: { return vec2<f32>(0.2627, 0.0593); }
        default: { return vec2<f32>(0.2990, 0.1140); }
    }
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let y = textureSample(y_texture, yuv_sampler, in.uv).r;
    let uv = textureSample(uv_texture, yuv_sampler, in.uv).rg;

    let full = color.full_range != 0u;
    let kk = kr_kb(color.matrix);
    let kr = kk.x;
    let kb = kk.y;

    // limited range 的 Y 占 16..235、色度占 16..240，先还原到满量程再套矩阵。
    //
    // 色度的中性点是 **128/255**，不是 0.5。用 0.5 会让每个像素带上约 +0.4%
    // 的偏置：中性灰被推亮约 4 个码值，暗部还会微微偏蓝。
    let yy = (y - select(16.0 / 255.0, 0.0, full)) * select(255.0 / 219.0, 1.0, full);
    let neutral = select(128.0 / 255.0, 0.5, full);
    let cb = (uv.r - neutral) * select(255.0 / 224.0, 1.0, full);
    let cr = (uv.g - neutral) * select(255.0 / 224.0, 1.0, full);

    let denom = 1.0 - kr - kb;
    var r = yy + 2.0 * (1.0 - kr) * cr;
    var g = yy - (2.0 * kb * (1.0 - kb) / denom) * cb - (2.0 * kr * (1.0 - kr) / denom) * cr;
    var b = yy + 2.0 * (1.0 - kb) * cb;

    r = clamp(r, 0.0, 1.0);
    g = clamp(g, 0.0, 1.0);
    b = clamp(b, 0.0, 1.0);

    if (color.transfer != 1u) {
        // SDR：gamma 编码值直接上屏（与旧路径逐字节相同）
        return vec4<f32>(r, g, b, 1.0);
    }

    // HDR10（PQ）：r/g/b 仍是 PQ 编码值
    let src_peak = pq_oetf(color.src_peak_nits / 10000.0);
    let dst_peak = pq_oetf(color.dst_peak_nits / 10000.0);
    let max_pq = max(r, max(g, b));
    let linear = vec3<f32>(pq_eotf(r), pq_eotf(g), pq_eotf(b));
    let max_linear = max(linear.r, max(linear.g, linear.b));
    // 亮度压缩系数：EETF 后的峰值 / 原峰值，再按固定的 SDR 白归一。
    // 归一化点不随 `dst_peak_nits` 变，所以调高它只会更亮，不会反向。
    // 全黑帧没有峰值可压，系数取任意值即可（乘 0 仍是 0）
    var scale = 1.0;
    if (max_linear > 0.0) {
        scale = pq_eotf(eetf(max_pq, src_peak, dst_peak)) / max_linear / SDR_WHITE_LINEAR;
    }
    var mapped = linear * scale;
    if (color.primaries == 2u) {
        mapped = BT2020_TO_709 * mapped;
    }
    return vec4<f32>(oetf_709(mapped.r), oetf_709(mapped.g), oetf_709(mapped.b), 1.0);
}
