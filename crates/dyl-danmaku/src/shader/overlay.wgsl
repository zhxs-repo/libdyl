// R8 Alpha Mask overlay shader — 弹幕叠加层
//
// 纹理采样 R8 (单通道 alpha mask)，颜色由实例属性传入。
// 每个实例是一个 glyph：位置/UV/颜色都走 instance-rate 顶点属性，
// 因此一整帧弹幕只需 1 个实例缓冲 + 2 次 draw call（outline、fill 各一次）。
// 选 instance-rate vertex buffer 而非 storage buffer：GLES/WebGL 后端也支持。

struct FrameUniforms {
    viewport: vec2<f32>,   // surface 尺寸 [w, h]
    _pad: vec2<f32>,
};

@group(0) @binding(0) var overlay_tex: texture_2d<f32>;
@group(0) @binding(1) var overlay_sampler: sampler;
@group(0) @binding(2) var<uniform> frame: FrameUniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
    @location(1) color: vec4<f32>,
};

// @location(0..2) 为 instance-rate 属性，与 renderer::GpuGlyph 的内存布局一一对应
@vertex
fn vs_main(
    @builtin(vertex_index) vertex_id: u32,
    @location(0) rect: vec4<f32>,
    @location(1) tex_rect: vec4<f32>,
    @location(2) color: vec4<f32>,
) -> VertexOutput {
    // 单位 quad 的四个角（TriangleStrip 顺序），同时用作 UV 角
    let unit = vec2<f32>(
        f32(vertex_id & 1u),
        f32((vertex_id >> 1u) & 1u),
    );

    // 像素坐标 → NDC（Y 轴翻转，与 wgpu 纹理坐标一致）
    let pixel = rect.xy + unit * rect.zw;
    let vp = vec2<f32>(max(frame.viewport.x, 1.0), max(frame.viewport.y, 1.0));
    let ndc = vec2<f32>(
        pixel.x / vp.x * 2.0 - 1.0,
        1.0 - pixel.y / vp.y * 2.0,
    );

    var out: VertexOutput;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    // tex_rect: [u, v, uw, vh] → 映射到 atlas UV
    out.tex_coord = tex_rect.xy + unit * tex_rect.zw;
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // R8 纹理：采样 .r 通道作为 alpha mask
    let alpha = textureSample(overlay_tex, overlay_sampler, in.tex_coord).r;
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
