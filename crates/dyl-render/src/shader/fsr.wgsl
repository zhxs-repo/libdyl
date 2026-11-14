// FSR 1.0：EASU（边沿自适应空间上采样）+ RCAS（对比度自适应锐化）
//
// 算法移植自 AMD FidelityFX Super Resolution 1.0（ffx_fsr1.h，
// Copyright (c) 2021 Advanced Micro Devices, Inc.，MIT License），
// 结构参照 ffx_fsr1 的 libplacebo/mpv 移植版本。
//
// 为什么在播放器里做：`yuv_pipeline` 的采样器是纯双线性，1080p 内容放到
// 4K 窗口就是把每个像素拉成 2x2 的软块。EASU 按局部梯度方向做 12 抽头重建，
// 边缘不会糊；RCAS 再把上采样后略微发软的细节拉回来。
//
// 坐标约定（与 `yuv_to_rgb.wgsl` 一致）：uv=(0,0) 在画面的左上角，
// 源纹理第 0 行就是画面顶行。两个 pass 都按「像素中心」取点，
// 即像素 i 的中心在 uv = (i + 0.5) / size。

struct FsrParams {
    /// RCAS 锐化强度，单位是 2 的幂次（0 = 最锐，2 = 温和）
    rcas_stops: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;
@group(0) @binding(2) var<uniform> params: FsrParams;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
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
    out.position = vec4<f32>(pos[idx], 0.0, 1.0);
    out.uv = uv[idx];
    return out;
}

/// 源纹理尺寸（像素）。两个 pass 都用它换算像素坐标，省掉一个 uniform。
fn src_size() -> vec2<f32> {
    return vec2<f32>(textureDimensions(src_tex, 0));
}

/// 取源纹理某个像素中心（p 为像素坐标，整数即像素中心）
fn tap(p: vec2<f32>) -> vec3<f32> {
    let uv = (p + vec2<f32>(0.5)) / src_size();
    return textureSampleLevel(src_tex, src_sampler, uv, 0.0).rgb;
}

fn luma(c: vec3<f32>) -> f32 {
    return c.b * 0.5 + (c.r * 0.5 + c.g);
}

/// 单个 2x2 象限的梯度方向与长度贡献。
///
/// 返回 `(dirX*w, dirY*w, (lenX + lenY)*w)`：把两个方向累加进调用方的
/// `dir` 与 `len`，避免 WGSL 里传指针。
fn easu_set(w: f32, lA: f32, lB: f32, lC: f32, lD: f32, lE: f32) -> vec3<f32> {
    let dc = lD - lC;
    let cb = lC - lB;
    var len_x = max(abs(dc), abs(cb));
    len_x = 1.0 / max(len_x, 1.0e-8);
    let dir_x = lD - lB;
    len_x = clamp(abs(dir_x) * len_x, 0.0, 1.0);
    len_x = len_x * len_x;

    let ec = lE - lC;
    let ca = lC - lA;
    var len_y = max(abs(ec), abs(ca));
    len_y = 1.0 / max(len_y, 1.0e-8);
    let dir_y = lE - lA;
    len_y = clamp(abs(dir_y) * len_y, 0.0, 1.0);
    len_y = len_y * len_y;

    return vec3<f32>(dir_x * w, dir_y * w, (len_x + len_y) * w);
}

/// 一个采样的权重与贡献。`w` 是 5 次多项式近似的高斯核（AMD 的原始拟合）。
fn easu_tap(
    off: vec2<f32>,
    dir: vec2<f32>,
    len: vec2<f32>,
    lob: f32,
    clp: f32,
    c: vec3<f32>,
) -> vec4<f32> {
    var v: vec2<f32>;
    v.x = off.x * dir.x + off.y * dir.y;
    v.y = off.x * (-dir.y) + off.y * dir.x;
    v = v * len;
    var d2 = v.x * v.x + v.y * v.y;
    d2 = min(d2, clp);
    var wb = (2.0 / 5.0) * d2 - 1.0;
    var wa = lob * d2 - 1.0;
    wb = wb * wb;
    wa = wa * wa;
    wb = (25.0 / 16.0) * wb - (25.0 / 16.0 - 1.0);
    let w = wb * wa;
    return vec4<f32>(c * w, w);
}

@fragment
fn fs_easu(in: VertexOutput) -> @location(0) vec4<f32> {
    // 输出像素中心在源像素坐标系里的位置
    var pp = in.uv * src_size() - vec2<f32>(0.5);
    let fp = floor(pp);
    pp = pp - fp;

    //    b c
    //  e f g h
    //  i j k l
    //    n o
    let b = tap(fp + vec2<f32>(0.0, -1.0));
    let c = tap(fp + vec2<f32>(1.0, -1.0));
    let e = tap(fp + vec2<f32>(-1.0, 0.0));
    let f = tap(fp);
    let g = tap(fp + vec2<f32>(1.0, 0.0));
    let h = tap(fp + vec2<f32>(2.0, 0.0));
    let i = tap(fp + vec2<f32>(-1.0, 1.0));
    let j = tap(fp + vec2<f32>(0.0, 1.0));
    let k = tap(fp + vec2<f32>(1.0, 1.0));
    let l = tap(fp + vec2<f32>(2.0, 1.0));
    let n = tap(fp + vec2<f32>(0.0, 2.0));
    let o = tap(fp + vec2<f32>(1.0, 2.0));

    let b_l = luma(b);
    let c_l = luma(c);
    let e_l = luma(e);
    let f_l = luma(f);
    let g_l = luma(g);
    let h_l = luma(h);
    let i_l = luma(i);
    let j_l = luma(j);
    let k_l = luma(k);
    let l_l = luma(l);
    let n_l = luma(n);
    let o_l = luma(o);

    let acc = easu_set((1.0 - pp.x) * (1.0 - pp.y), b_l, e_l, f_l, g_l, j_l)
        + easu_set(pp.x * (1.0 - pp.y), c_l, f_l, g_l, h_l, k_l)
        + easu_set((1.0 - pp.x) * pp.y, f_l, i_l, j_l, k_l, n_l)
        + easu_set(pp.x * pp.y, g_l, j_l, k_l, l_l, o_l);

    var dir = vec2<f32>(acc.x, acc.y);
    var len = acc.z;

    let dir2 = dir * dir;
    var dir_r = dir2.x + dir2.y;
    let zero = dir_r < (1.0 / 32768.0);
    dir_r = inverseSqrt(max(dir_r, 1.0e-12));
    dir_r = select(dir_r, 1.0, zero);
    dir.x = select(dir.x, 1.0, zero);
    dir = dir * dir_r;
    len = len * 0.5;
    len = len * len;

    let stretch = (dir.x * dir.x + dir.y * dir.y) / max(abs(dir.x), abs(dir.y));
    let len2 = vec2<f32>(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    let lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    let clp = 1.0 / lob;

    let min4 = min(min(f, g), min(j, k));
    let max4 = max(max(f, g), max(j, k));

    var colors = array<vec3<f32>, 12>(b, c, i, j, f, e, k, l, h, g, o, n);
    var offsets = array<vec2<f32>, 12>(
        vec2<f32>(0.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(-1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(2.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 2.0),
        vec2<f32>(0.0, 2.0),
    );

    var a_c = vec3<f32>(0.0);
    var a_w = 0.0;
    for (var t = 0; t < 12; t = t + 1) {
        let r = easu_tap(offsets[t] - pp, dir, len2, lob, clp, colors[t]);
        a_c += r.xyz;
        a_w += r.w;
    }

    // 结果夹在 2x2 邻域内：上采样不该造出源像素里不存在的极值（振铃）
    let pix = min(max4, max(min4, a_c / a_w));
    return vec4<f32>(pix, 1.0);
}

/// AMD 的 RCAS 锐化上限（0.25 - 1/16）
const FSR_RCAS_LIMIT: f32 = 0.25 - (1.0 / 16.0);

@fragment
fn fs_rcas(in: VertexOutput) -> @location(0) vec4<f32> {
    let ts = 1.0 / src_size();
    let b = textureSampleLevel(src_tex, src_sampler, in.uv + vec2<f32>(0.0, -1.0) * ts, 0.0).rgb;
    let d = textureSampleLevel(src_tex, src_sampler, in.uv + vec2<f32>(-1.0, 0.0) * ts, 0.0).rgb;
    let ee = textureSampleLevel(src_tex, src_sampler, in.uv, 0.0);
    let e = ee.rgb;
    let f = textureSampleLevel(src_tex, src_sampler, in.uv + vec2<f32>(1.0, 0.0) * ts, 0.0).rgb;
    let h = textureSampleLevel(src_tex, src_sampler, in.uv + vec2<f32>(0.0, 1.0) * ts, 0.0).rgb;

    // 去噪项（FSR_RCAS_DENOISE）保持关闭：AMD 默认与 Lossless Scaling 都关
    let mn4 = min(min(b, d), min(f, h));
    let mx4 = max(max(b, d), max(f, h));
    let peak_c = vec2<f32>(1.0, -4.0);
    let hit_min = min(mn4, e) / (4.0 * mx4 + 1.0e-8);
    let hit_max = (peak_c.x - max(mx4, e)) / (4.0 * mn4 + peak_c.y);
    let lobe_rgb = max(-hit_min, hit_max);
    let sharp = exp2(-params.rcas_stops);
    let lobe = max(
        -FSR_RCAS_LIMIT,
        min(max(max(lobe_rgb.r, lobe_rgb.g), lobe_rgb.b), 0.0),
    ) * sharp;
    let rcp_l = 1.0 / (4.0 * lobe + 1.0);
    let pix = (lobe * b + lobe * d + lobe * h + lobe * f + e) * rcp_l;
    return vec4<f32>(clamp(pix, vec3<f32>(0.0), vec3<f32>(1.0)), ee.a);
}
