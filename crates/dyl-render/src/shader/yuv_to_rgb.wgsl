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

/// 色彩参数 — 与 Rust 侧 ColorParams 的 16 字节布局一致
struct ColorParams {
    matrix: u32,     // 0 = BT.601, 1 = BT.709, 2 = BT.2020
    full_range: u32, // 非 0 = full range(0..255)，否则 limited(16..235)
    transfer: u32,   // 0 = SDR（gamma 直通）, 1 = PQ（ST 2084）
    primaries: u32,  // 0 = 其它/未标注, 2 = BT.2020（PQ 路径下做 2020→709）
};

@group(0) @binding(3) var<uniform> color: ColorParams;

// HDR→SDR 的假设：没有静态元数据时按 1000 nit 内容峰值、100 nit SDR 目标。
// 常量为 PQ 编码值（PQ(nits / 10000)），取 BT.2390 附录的 10-bit 代表值
const SRC_PEAK_PQ: f32 = 0.7518271;
const DST_PEAK_PQ: f32 = 0.5080784;
// 膝点：以下恒等，以上用 Hermite 压到目标峰值
const KNEE_PQ: f32 = 1.5 * DST_PEAK_PQ - 0.5 * SRC_PEAK_PQ;

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

/// BT.2390 EETF — PQ 域亮度压缩
fn eetf(e: f32) -> f32 {
    if (e <= KNEE_PQ) {
        return e;
    }
    let t = clamp((e - KNEE_PQ) / (SRC_PEAK_PQ - KNEE_PQ), 0.0, 1.0);
    let t2 = t * t;
    let t3 = t2 * t;
    // 三次 Hermite：起点值 = 膝点、起点斜率 = 1（与线性段 C1 连续）、
    // 终点值 = 目标峰值、终点斜率 = 0
    let p = (2.0 * t3 - 3.0 * t2 + 1.0) * KNEE_PQ
        + (t3 - 2.0 * t2 + t) * (SRC_PEAK_PQ - KNEE_PQ)
        + (-2.0 * t3 + 3.0 * t2) * DST_PEAK_PQ;
    return min(p, DST_PEAK_PQ);
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

    // limited range 的 Y 占 16..235、色度占 ±112，先还原到满量程再套矩阵
    let yy = (y - select(16.0 / 255.0, 0.0, full)) * select(255.0 / 219.0, 1.0, full);
    let cb = (uv.r - 0.5) * select(255.0 / 224.0, 1.0, full);
    let cr = (uv.g - 0.5) * select(255.0 / 224.0, 1.0, full);

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
    let max_pq = max(r, max(g, b));
    let linear = vec3<f32>(pq_eotf(r), pq_eotf(g), pq_eotf(b));
    let max_linear = max(linear.r, max(linear.g, linear.b));
    // 亮度压缩系数：EETF 后的峰值 / 原峰值，再按 100 nit SDR 目标归一。
    // 全黑帧没有峰值可压，系数取任意值即可（乘 0 仍是 0）
    var scale = 1.0;
    if (max_linear > 0.0) {
        scale = pq_eotf(eetf(max_pq)) / max_linear / pq_eotf(DST_PEAK_PQ);
    }
    var mapped = linear * scale;
    if (color.primaries == 2u) {
        mapped = BT2020_TO_709 * mapped;
    }
    return vec4<f32>(oetf_709(mapped.r), oetf_709(mapped.g), oetf_709(mapped.b), 1.0);
}
