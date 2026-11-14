//! YUV→RGB blit 管线与 UV 平面交织 — WgpuRenderer / ExternalRenderer 共享实现
//!
//! 两个渲染器只有 surface 来源不同（自管窗口 vs 宿主注入），
//! 视频上传/绘制的 GPU 资源构造完全一致，统一收敛到这里避免双份维护。

use dyl_ffmpeg::VideoFrame;

/// `ColorParams` uniform 的字节数：
/// `matrix + full_range + transfer + primaries`（16 字节）
/// + `dst_peak_nits + src_peak_nits`（8 字节）+ 8 字节填充（uniform 结构 16 字节对齐）
const COLOR_PARAMS_SIZE: u64 = 32;

/// HDR→SDR 映射参数。
///
/// 归一化基准恒为 SDR 白（100 nit），[`Self::dst_peak_nits`] 只决定**内容峰值
/// 落在哪个电平**：85 表示最亮的内容压在 85% 白电平上，留 15% 余量，与
/// mpv/libplacebo 的默认观感接近；设到 100 则内容峰值顶到白，高光会被裁掉。
/// 调高它一定更亮，不会反向。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrTarget {
    /// 内容峰值在显示端落到的亮度（nit，相对 SDR 白 100）。默认 85
    pub dst_peak_nits: f32,
    /// 无静态元数据时假定的内容峰值（nit）。默认 1000
    pub src_peak_nits: f32,
}

impl Default for HdrTarget {
    fn default() -> Self {
        Self {
            dst_peak_nits: 85.0,
            src_peak_nits: 1000.0,
        }
    }
}

impl HdrTarget {
    /// 夹到有意义的区间：低于 40 nit 会把整幅画压成灰雾；高于 100 没有意义
    /// —— SDR 白就在 100，再高只会让内容峰值越过白电平被裁掉。
    pub fn sanitized(self) -> Self {
        let clamp = |v: f32, lo: f32, hi: f32, fallback: f32| {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                fallback
            }
        };
        Self {
            dst_peak_nits: clamp(self.dst_peak_nits, 40.0, 100.0, 85.0),
            src_peak_nits: clamp(self.src_peak_nits, 100.0, 10000.0, 1000.0),
        }
    }
}

/// 由帧的色彩元数据生成 uniform 内容
///
/// 布局必须与 `yuv_to_rgb.wgsl` 的 `ColorParams` 一致；uniform 按宿主字节序读取，
/// 所以用 `to_ne_bytes`
pub(crate) fn color_params_bytes(
    frame: &VideoFrame,
    hdr: HdrTarget,
) -> [u8; COLOR_PARAMS_SIZE as usize] {
    let matrix = frame.yuv_matrix().shader_index().to_ne_bytes();
    let full = u32::from(frame.is_full_range()).to_ne_bytes();
    let transfer = frame.color_transfer.shader_index().to_ne_bytes();
    let primaries = frame.color_primaries.shader_index().to_ne_bytes();
    let mut bytes = [0u8; COLOR_PARAMS_SIZE as usize];
    bytes[0..4].copy_from_slice(&matrix);
    bytes[4..8].copy_from_slice(&full);
    bytes[8..12].copy_from_slice(&transfer);
    bytes[12..16].copy_from_slice(&primaries);
    bytes[16..20].copy_from_slice(&hdr.dst_peak_nits.to_ne_bytes());
    bytes[20..24].copy_from_slice(&hdr.src_peak_nits.to_ne_bytes());
    bytes
}

/// 构建 YUV 渲染所需的管线、绑定组与纹理。
///
/// `label_prefix` 用于区分调试标签来源（如 "ext"），空串则不加前缀。
/// 返回的色彩 uniform 初始为 BT.601 limited，每帧由 `color_params_bytes` 更新。
pub(crate) fn setup_yuv_pipeline(
    device: &wgpu::Device,
    surface_format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    label_prefix: &str,
) -> (
    wgpu::RenderPipeline,
    wgpu::BindGroup,
    wgpu::Texture,
    wgpu::Texture,
    wgpu::Buffer,
) {
    let lbl = |name: &str| -> String {
        if label_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{label_prefix}_{name}")
        }
    };

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(&lbl("yuv_to_rgb")),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader/yuv_to_rgb.wgsl").into()),
    });

    let y_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(&lbl("y_texture")),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let uv_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(&lbl("uv_texture")),
        size: wgpu::Extent3d {
            width: (width / 2).max(1),
            height: (height / 2).max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rg8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(&lbl("yuv_sampler")),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let color_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(&lbl("yuv_color_params")),
        size: COLOR_PARAMS_SIZE,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(&lbl("yuv_bind_group_layout")),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(COLOR_PARAMS_SIZE),
                },
                count: None,
            },
        ],
    });
    let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(&lbl("yuv_bind_group")),
        layout: &bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&y_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&uv_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: color_buffer.as_entire_binding(),
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(&lbl("yuv_pipeline_layout")),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(&lbl("yuv_render_pipeline")),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    (
        render_pipeline,
        bind_group,
        y_texture,
        uv_texture,
        color_buffer,
    )
}

/// 把平面 U/V 数据按 stride 交织成 RG8 纹理所需的行主序字节
pub(crate) fn interleave_uv_planes(
    u_data: &[u8],
    u_stride: usize,
    v_data: &[u8],
    v_stride: usize,
    uv_width: usize,
    uv_height: usize,
) -> Vec<u8> {
    let mut result = Vec::with_capacity(uv_width * uv_height * 2);
    for row in 0..uv_height {
        for col in 0..uv_width {
            let u = if row * u_stride + col < u_data.len() {
                u_data[row * u_stride + col]
            } else {
                128
            };
            let v = if row * v_stride + col < v_data.len() {
                v_data[row * v_stride + col]
            } else {
                128
            };
            result.push(u);
            result.push(v);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyl_ffmpeg::{ColorPrimaries, ColorRange, ColorSpace, PixelFormat};

    fn frame(space: ColorSpace, range: ColorRange, width: u32, height: u32) -> VideoFrame {
        VideoFrame {
            pts: Some(0),
            time_base: dyl_ffmpeg::time_base(1, 90000),
            width,
            height,
            display_width: width,
            display_height: height,
            format: PixelFormat::Yuv420p,
            color_space: space,
            color_primaries: ColorPrimaries::Unspecified,
            color_range: range,
            color_transfer: dyl_ffmpeg::ColorTransfer::Sdr,
            planes: vec![],
        }
    }

    /// uniform 布局是 Rust ↔ WGSL 的约定：matrix / full_range / transfer /
    /// primaries 各占 4 字节，之后是 HDR 的两个峰值亮度
    #[test]
    fn color_params_bytes_layout() {
        let hdr = HdrTarget::default();
        let bytes =
            color_params_bytes(&frame(ColorSpace::Bt709, ColorRange::Full, 1920, 1080), hdr);
        assert_eq!(bytes.len(), 32);
        assert_eq!(u32::from_ne_bytes(bytes[0..4].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(bytes[8..12].try_into().unwrap()), 0);
        assert_eq!(u32::from_ne_bytes(bytes[12..16].try_into().unwrap()), 0);
        assert_eq!(f32::from_ne_bytes(bytes[16..20].try_into().unwrap()), 85.0);
        assert_eq!(
            f32::from_ne_bytes(bytes[20..24].try_into().unwrap()),
            1000.0
        );

        // 未标注：矩阵走分辨率回退，量程走 limited
        let sd = color_params_bytes(
            &frame(ColorSpace::Unspecified, ColorRange::Unspecified, 640, 360),
            hdr,
        );
        assert_eq!(u32::from_ne_bytes(sd[0..4].try_into().unwrap()), 0);
        assert_eq!(u32::from_ne_bytes(sd[4..8].try_into().unwrap()), 0);

        // HDR10：PQ + BT.2020 必须分别落到 transfer/primaries 槽位
        let mut pq = frame(ColorSpace::Bt2020, ColorRange::Limited, 3840, 1608);
        pq.color_primaries = ColorPrimaries::Bt2020;
        pq.color_transfer = dyl_ffmpeg::ColorTransfer::Pq;
        let bytes = color_params_bytes(
            &pq,
            HdrTarget {
                dst_peak_nits: 100.0,
                src_peak_nits: 4000.0,
            },
        );
        assert_eq!(u32::from_ne_bytes(bytes[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_ne_bytes(bytes[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_ne_bytes(bytes[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(bytes[12..16].try_into().unwrap()), 2);
        // 目标峰值必须真的进了 uniform，否则 shader 还是按默认值压
        assert_eq!(f32::from_ne_bytes(bytes[16..20].try_into().unwrap()), 100.0);
        assert_eq!(
            f32::from_ne_bytes(bytes[20..24].try_into().unwrap()),
            4000.0
        );
    }

    #[test]
    fn hdr_target_is_clamped_to_physical_range() {
        let d = HdrTarget::default().sanitized();
        assert_eq!(d.dst_peak_nits, 85.0);

        let low = HdrTarget {
            dst_peak_nits: 10.0,
            src_peak_nits: 10.0,
        }
        .sanitized();
        assert_eq!(low.dst_peak_nits, 40.0, "低于 40 nit 会把整幅画压成灰雾");
        assert_eq!(low.src_peak_nits, 100.0);

        // 目标峰值上限是 SDR 白：再高只会让内容峰值越过白电平被裁掉
        let high = HdrTarget {
            dst_peak_nits: 9000.0,
            src_peak_nits: 90_000.0,
        }
        .sanitized();
        assert_eq!(high.dst_peak_nits, 100.0);
        assert_eq!(high.src_peak_nits, 10_000.0);

        let nan = HdrTarget {
            dst_peak_nits: f32::NAN,
            src_peak_nits: f32::INFINITY,
        }
        .sanitized();
        assert_eq!(nan.dst_peak_nits, 85.0);
        assert_eq!(nan.src_peak_nits, 1000.0);
    }

    #[test]
    fn test_interleave_uv_planes() {
        let u = vec![10u8, 20, 30, 40];
        let v = vec![50u8, 60, 70, 80];
        let result = interleave_uv_planes(&u, 2, &v, 2, 2, 2);
        assert_eq!(result, vec![10, 50, 20, 60, 30, 70, 40, 80]);
    }

    /// WGSL 只有创建 pipeline 时才编译，CI 的无 GPU runner 跑不到那一步；
    /// 用 naga 直接解析 + 校验源码，把语法/类型错误挡在门禁里
    #[test]
    fn yuv_shader_parses_and_validates() {
        let source = include_str!("shader/yuv_to_rgb.wgsl");
        let module = naga::front::wgsl::parse_str(source).expect("WGSL 必须能被解析");
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );
        validator.validate(&module).expect("WGSL 模块必须通过校验");
    }

    #[test]
    fn test_interleave_uv_empty() {
        let result = interleave_uv_planes(&[], 0, &[], 0, 0, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn test_interleave_uv_out_of_range_fills_neutral() {
        // 数据短于 stride 覆盖范围时应以 128（中性色度）填充
        let u = vec![10u8];
        let v = vec![20u8];
        let result = interleave_uv_planes(&u, 2, &v, 2, 2, 1);
        assert_eq!(result, vec![10, 20, 128, 128]);
    }

    /// 在真实 GPU 上跑一遍 `yuv_to_rgb.wgsl` 的灰阶斜坡，把 PQ→SDR 的色调
    /// 曲线钉住。
    ///
    /// 只做 naga 语法校验抓不到"曲线形状错了"这类问题：内容峰值被顶到 255、
    /// 归一化点跟着 `dst_peak_nits` 漂移，都是数学错误而不是编译错误，必须
    /// 真渲染一次读回码值才能断言。无适配器的 CI 上跳过。
    mod gpu_tone_curve {
        use super::*;

        const W: u32 = 256;
        /// 斜坡的 8-bit limited-range Y 起点/步进：覆盖 16..235 全量程
        fn ramp_y() -> Vec<u8> {
            (0..W).map(|i| (16 + 219 * i / (W - 1)) as u8).collect()
        }

        fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::all(),
                flags: wgpu::InstanceFlags::default(),
                memory_budget_thresholds: Default::default(),
                backend_options: Default::default(),
                display: None,
            });
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                }))
                .ok()?;
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("tone_curve_test_device"),
                    required_features: wgpu::Features::empty(),
                    required_limits:
                        wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                    experimental_features: Default::default(),
                    memory_hints: Default::default(),
                    trace: Default::default(),
                }))
                .ok()?;
            Some((device, queue))
        }

        /// 把灰阶斜坡按 PQ（limited range, BT.2020 原色）送进 shader，
        /// 返回读回的 R 通道码值。
        fn render(hdr: HdrTarget) -> Vec<u8> {
            let (device, queue) = gpu().expect("此测试需要可用的 GPU 适配器");

            let (pipeline, bind_group, y_tex, uv_tex, params) =
                setup_yuv_pipeline(&device, wgpu::TextureFormat::Rgba8Unorm, W, 1, "tone_curve");
            let _ = &uv_tex;

            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &y_tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &ramp_y(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(W),
                    rows_per_image: Some(1),
                },
                wgpu::Extent3d {
                    width: W,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            // 中性色度：U/V 各 128，灰阶不产生色差
            let neutral = vec![128u8; (W / 2) as usize * 2];
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &uv_tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &neutral,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(W / 2 * 2),
                    rows_per_image: Some(1),
                },
                wgpu::Extent3d {
                    width: W / 2,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );

            // BT.2020 矩阵 + PQ 传输 + BT.2020 原色
            let mut bytes = [0u8; 32];
            bytes[0..4].copy_from_slice(&2u32.to_ne_bytes());
            bytes[4..8].copy_from_slice(&0u32.to_ne_bytes());
            bytes[8..12].copy_from_slice(&1u32.to_ne_bytes());
            bytes[12..16].copy_from_slice(&2u32.to_ne_bytes());
            bytes[16..20].copy_from_slice(&hdr.dst_peak_nits.to_ne_bytes());
            bytes[20..24].copy_from_slice(&hdr.src_peak_nits.to_ne_bytes());
            queue.write_buffer(&params, 0, &bytes);

            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("tone_curve_target"),
                size: wgpu::Extent3d {
                    width: W,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = target.create_view(&wgpu::TextureViewDescriptor::default());

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tone_curve_encoder"),
            });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tone_curve_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    ..Default::default()
                });
                pass.set_pipeline(&pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.set_viewport(0.0, 0.0, W as f32, 1.0, 0.0, 1.0);
                pass.draw(0..4, 0..1);
            }

            let padded = W * 4; // 1024 字节，天然满足 256 对齐
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tone_curve_readback"),
                size: padded as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &target,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(1),
                    },
                },
                wgpu::Extent3d {
                    width: W,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            queue.submit([encoder.finish()]);

            buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            let _ = device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            });
            let data = buffer.slice(..).get_mapped_range();
            (0..W as usize).map(|i| data[i * 4]).collect()
        }

        // ST 2084 常数（与 WGSL 同值；m1/c2 的十进制位数超过 f32 精度，
        // 写成能精确落到同一个 f32 的短形式）
        const M1: f32 = 0.159_301_76;
        const M2: f32 = 78.843_75;
        const C1: f32 = 0.835_937_5;
        const C2: f32 = 18.851_562;
        const C3: f32 = 18.687_5;

        /// shader 里那套公式的独立实现，用来算期望值。
        ///
        /// 与 WGSL 重复是有意的：这条测试的目的就是"锁行为"，公式改了必须
        /// 显式改这里，不能悄悄漂移。
        fn pq_eotf(e: f32) -> f32 {
            let p = e.max(0.0).powf(1.0 / M2);
            ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1)
        }

        fn pq_oetf(l: f32) -> f32 {
            let p = l.clamp(0.0, 1.0).powf(M1);
            ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
        }

        fn eetf(e: f32, src_peak: f32, dst_peak: f32) -> f32 {
            let knee = 1.5 * dst_peak - 0.5 * src_peak;
            if e <= knee {
                return e;
            }
            let t = ((e - knee) / (src_peak - knee).max(1e-9)).clamp(0.0, 1.0);
            let (t2, t3) = (t * t, t * t * t);
            let p = (2.0 * t3 - 3.0 * t2 + 1.0) * knee
                + (t3 - 2.0 * t2 + t) * (src_peak - knee)
                + (-2.0 * t3 + 3.0 * t2) * dst_peak;
            p.min(dst_peak)
        }

        fn oetf_709(l: f32) -> f32 {
            let v = l.clamp(0.0, 1.0);
            if v < 0.018 {
                4.5 * v
            } else {
                1.099 * v.powf(0.45) - 0.099
            }
        }

        /// 某个 8-bit Y 码值经 shader 后期望的输出码值
        fn expected(y8: u8, hdr: HdrTarget) -> f32 {
            let yy = (y8 as f32 / 255.0 - 16.0 / 255.0) * (255.0 / 219.0);
            let sp = pq_oetf(hdr.src_peak_nits / 10000.0);
            let dp = pq_oetf(hdr.dst_peak_nits / 10000.0);
            let linear = pq_eotf(yy);
            let scale = if linear > 0.0 {
                pq_eotf(eetf(yy, sp, dp)) / linear / 0.01
            } else {
                1.0
            };
            oetf_709(linear * scale) * 255.0
        }

        /// 某个 8-bit Y 码值对应的输入亮度（nit）
        fn nits_of(y8: u8) -> f32 {
            let yy = (y8 as f32 / 255.0 - 16.0 / 255.0) * (255.0 / 219.0);
            pq_eotf(yy) * 10000.0
        }

        #[test]
        fn tone_curve_matches_reference_math() {
            let hdr = HdrTarget::default();
            let out = render(hdr);
            // 8-bit 量化 + GPU 与 CPU 的 powf 末位差异，容差 1 个码值；
            // 曲线形状错（比如归一化点写错、色度中性点取 0.5）会差出 4 个以上
            let mut worst = (0.0f32, 0usize);
            for (i, &got) in out.iter().enumerate() {
                let dev = (got as f32 - expected(ramp_y()[i], hdr)).abs();
                if dev > worst.0 {
                    worst = (dev, i);
                }
            }
            assert!(
                worst.0 <= 1.0,
                "第 {} 列（Y={}）偏差 {:.1} 码值，超过 1",
                worst.1,
                ramp_y()[worst.1],
                worst.0
            );
        }

        /// 内容峰值必须落在 `dst_peak_nits`，不能顶到 255。
        ///
        /// 这条正是改造后出问题的点：归一化点跟着 `dst_peak_nits` 走时，
        /// 1000 nit 恒等于 255，300 nit 以上就全糊成白色。
        #[test]
        fn content_peak_lands_on_target_not_white() {
            let hdr = HdrTarget::default();
            let out = render(hdr);
            let col = (0..W as usize)
                .min_by_key(|&i| (nits_of(ramp_y()[i]) - 1000.0).abs() as u64)
                .unwrap();
            let got = out[col];
            // 85 nit 是线性光 85%（编码后约 235），不是码值 85%
            let want = oetf_709(0.85) * 255.0;
            assert!(
                (got as f32 - want).abs() <= 8.0,
                "1000 nit 应落在 85 nit 电平（码值≈{want:.0}），实际 {got}"
            );
            assert!(
                out.iter().all(|&v| v < 250),
                "默认参数下不应有任何列被顶到白：最大 {}",
                out.iter().max().unwrap()
            );
        }

        /// 单调：亮度越高输出不能越低，否则会出现色带或反转的灰阶
        #[test]
        fn tone_curve_is_monotonic() {
            let out = render(HdrTarget::default());
            // EETF 在峰值处斜率为 0，加上 8-bit 量化允许 1 个码值的回退；
            // 真正的非单调（色带、反转灰阶）会是十几个码值
            for w in out.windows(2) {
                assert!(w[1] + 1 >= w[0], "曲线非单调：{} -> {}", w[0], w[1]);
            }
        }

        /// 调高 `dst_peak_nits` 必须更亮（旧实现的语义是反的）
        #[test]
        fn raising_target_peak_brightens() {
            let dim = render(HdrTarget {
                dst_peak_nits: 60.0,
                ..Default::default()
            });
            let bright = render(HdrTarget {
                dst_peak_nits: 100.0,
                ..Default::default()
            });
            let mid = dim.len() * 3 / 4;
            assert!(
                bright[mid] > dim[mid],
                "调高目标峰值应更亮：60 nit -> {}, 100 nit -> {}",
                dim[mid],
                bright[mid]
            );
        }
    }
}
