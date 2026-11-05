// YUV420P/NV12 → RGB 转换 shader
// 矩阵系数与量程由 uniform 提供：BT.601 / BT.709 / BT.2020 与 limited/full range
// 写死一组系数会让 HD 内容整体偏色（肤色过红）

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
    _pad: vec2<u32>,
};

@group(0) @binding(3) var<uniform> color: ColorParams;

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

    return vec4<f32>(r, g, b, 1.0);
}
