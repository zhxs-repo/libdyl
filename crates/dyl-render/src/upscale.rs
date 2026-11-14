//! FSR 1.0 上采样阶段（EASU + RCAS）—— 两个渲染路径共用的后处理。
//!
//! 现有 `yuv_pipeline` 的采样器是双线性：1080p 内容放到 4K 窗口就是把每个
//! 像素拉成 2x2 的软块。这里加一条独立的链：
//!
//! ```text
//! Y/UV ──yuv_to_rgb──▶ src（RGBA16F，视频尺寸）
//!        ──EASU──▶ mid（RGBA16F，显示矩形尺寸）
//!        ──RCAS──▶ 目标视图（视口 = 显示矩形）
//! ```
//!
//! 必须分三趟：EASU 的抽头在 RGB 空间里做（YUV 上做不了方向性重建），而
//! RCAS 要读 EASU 输出的邻域，不能和它合进同一个 pass。
//!
//! 中间纹理用 `Rgba16Float`：可被过滤采样，且不受目标格式是 8 位还是 10 位
//! 影响——8 位中间体会让 HDR 内容在两步之间掉精度。
//!
//! 默认关闭；放大不到 1.15 倍（线性）时也不启用：多跑两趟全屏 pass 换不来
//! 可见收益。

/// 上采样器选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upscaler {
    /// 关闭：沿用原有的单趟双线性绘制
    Off,
    /// FSR 1.0：EASU 空间上采样 + RCAS 锐化
    Fsr1,
}

/// 上采样配置。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UpscaleConfig {
    pub upscaler: Upscaler,
    /// RCAS 强度，单位是 2 的幂次（0 = 最锐，2 = 温和，AMD 默认档在 2 附近）
    pub rcas_stops: f32,
}

impl Default for UpscaleConfig {
    fn default() -> Self {
        Self {
            upscaler: Upscaler::Off,
            rcas_stops: 2.0,
        }
    }
}

impl UpscaleConfig {
    /// 锐化参数必须有限且落在 0..=4，否则 shader 里的 `exp2` 会发散
    pub fn sanitized(self) -> Self {
        Self {
            upscaler: self.upscaler,
            rcas_stops: if self.rcas_stops.is_finite() {
                self.rcas_stops.clamp(0.0, 4.0)
            } else {
                2.0
            },
        }
    }
}

/// 放大倍数低于这个值就不值得多跑两趟全屏 pass。
const MIN_UPSCALE_RATIO: f32 = 1.15;

/// 是否值得走上采样链。
///
/// 两个方向都要明显放大：显示矩形保持视频宽高比，正常情况下两个倍数相同；
/// 只有一个方向变大说明矩形被裁剪/异常，交给原路径更稳。
pub(crate) fn should_upscale(src: (u32, u32), dst: (u32, u32)) -> bool {
    let (sw, sh) = (src.0 as f32, src.1 as f32);
    let (dw, dh) = (dst.0 as f32, dst.1 as f32);
    if sw < 16.0 || sh < 16.0 || dw < 16.0 || dh < 16.0 {
        return false;
    }
    dw >= sw * MIN_UPSCALE_RATIO && dh >= sh * MIN_UPSCALE_RATIO
}

/// `FsrParams` uniform 的字节数：一个 f32 + 12 字节填充
const FSR_PARAMS_SIZE: u64 = 16;

/// 中间纹理格式：可过滤、16 位浮点、与目标格式解耦
const INTERMEDIATE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// 一张中间纹理及其绑定组
struct Slot {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
    size: (u32, u32),
}

/// FSR 阶段持有的 GPU 资源。中间纹理按尺寸懒建。
pub(crate) struct FsrStage {
    bind_layout: wgpu::BindGroupLayout,
    easu: wgpu::RenderPipeline,
    rcas: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    src: Option<Slot>,
    mid: Option<Slot>,
    /// 已上传的锐化参数，相同就不重复写 buffer
    stops: Option<f32>,
}

fn create_slot(
    device: &wgpu::Device,
    bind_layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    params: &wgpu::Buffer,
    label: &str,
    size: (u32, u32),
) -> Slot {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0.max(1),
            height: size.1.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: INTERMEDIATE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout: bind_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: params.as_entire_binding(),
            },
        ],
    });
    Slot {
        _texture: texture,
        view,
        bind,
        size,
    }
}

fn color_target(format: wgpu::TextureFormat) -> Option<wgpu::ColorTargetState> {
    Some(wgpu::ColorTargetState {
        format,
        blend: None,
        write_mask: wgpu::ColorWrites::ALL,
    })
}

impl FsrStage {
    /// `target_format` 是目标视图的格式：RCAS 直接写目标，管线格式必须一致。
    pub(crate) fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fsr_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader/fsr.wgsl").into()),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fsr_bind_layout"),
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
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(FSR_PARAMS_SIZE),
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fsr_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let make = |entry: &str, label: &str, format: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    targets: &[color_target(format)],
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
            })
        };

        let easu = make("fs_easu", "fsr_easu_pipeline", INTERMEDIATE_FORMAT);
        let rcas = make("fs_rcas", "fsr_rcas_pipeline", target_format);

        Self {
            bind_layout,
            easu,
            rcas,
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("fsr_sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            params: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("fsr_params"),
                size: FSR_PARAMS_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            src: None,
            mid: None,
            stops: None,
        }
    }

    fn write_params(&mut self, queue: &wgpu::Queue, stops: f32) {
        if self.stops == Some(stops) {
            return;
        }
        let mut bytes = [0u8; FSR_PARAMS_SIZE as usize];
        bytes[0..4].copy_from_slice(&stops.to_ne_bytes());
        queue.write_buffer(&self.params, 0, &bytes);
        self.stops = Some(stops);
    }

    fn ensure_src(&mut self, device: &wgpu::Device, size: (u32, u32)) {
        if self.src.as_ref().map(|s| s.size) == Some(size) {
            return;
        }
        let slot = create_slot(
            device,
            &self.bind_layout,
            &self.sampler,
            &self.params,
            "fsr_src",
            size,
        );
        self.src = Some(slot);
    }

    fn ensure_mid(&mut self, device: &wgpu::Device, size: (u32, u32)) {
        if self.mid.as_ref().map(|s| s.size) == Some(size) {
            return;
        }
        let slot = create_slot(
            device,
            &self.bind_layout,
            &self.sampler,
            &self.params,
            "fsr_mid",
            size,
        );
        self.mid = Some(slot);
    }

    /// ② EASU（src → mid）+ ③ RCAS（mid → 目标视口的显示矩形）
    fn easu_rcas(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        src_bind: &wgpu::BindGroup,
        rect: (f32, f32, f32, f32),
    ) {
        let Some(mid) = self.mid.as_ref() else {
            return;
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fsr_easu_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &mid.view,
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
            pass.set_pipeline(&self.easu);
            pass.set_bind_group(0, src_bind, &[]);
            let (w, h) = (mid.size.0 as f32, mid.size.1 as f32);
            pass.set_viewport(0.0, 0.0, w, h, 0.0, 1.0);
            pass.draw(0..4, 0..1);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fsr_rcas_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
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
            pass.set_pipeline(&self.rcas);
            pass.set_bind_group(0, &mid.bind, &[]);
            pass.set_viewport(rect.0, rect.1, rect.2, rect.3, 0.0, 1.0);
            pass.draw(0..4, 0..1);
        }
    }

    /// 跑完整条链：YUV → src → EASU → mid → RCAS → `target`。
    ///
    /// `yuv_pipeline` / `yuv_bind` 由调用方提供（它们由 `yuv_pipeline` 模块
    /// 持有）；`rect` 是目标视图里的显示矩形 `(x, y, w, h)`。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        yuv_pipeline: &wgpu::RenderPipeline,
        yuv_bind: &wgpu::BindGroup,
        src_size: (u32, u32),
        rect: (f32, f32, f32, f32),
        stops: f32,
    ) {
        self.write_params(queue, stops);
        self.ensure_src(device, src_size);
        self.ensure_mid(device, mid_size(rect));

        let Some(src) = self.src.as_ref() else {
            return;
        };
        // ① YUV → src（全视口，源分辨率）
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fsr_yuv_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &src.view,
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
            pass.set_pipeline(yuv_pipeline);
            pass.set_bind_group(0, yuv_bind, &[]);
            pass.set_viewport(0.0, 0.0, src_size.0 as f32, src_size.1 as f32, 0.0, 1.0);
            pass.draw(0..4, 0..1);
        }

        self.easu_rcas(encoder, target, &src.bind, rect);
    }

    /// 输入已经是 RGB（调用方自备绑定组）时只跑 EASU + RCAS。
    ///
    /// 供离屏验证使用：真实播放路径永远走 [`Self::render`]。
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_with_source(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        src_bind: &wgpu::BindGroup,
        rect: (f32, f32, f32, f32),
        stops: f32,
    ) {
        self.write_params(queue, stops);
        self.ensure_mid(device, mid_size(rect));
        self.easu_rcas(encoder, target, src_bind, rect);
    }
}

/// 显示矩形的整数尺寸（至少 1 像素）
fn mid_size(rect: (f32, f32, f32, f32)) -> (u32, u32) {
    (
        rect.2.round().max(1.0) as u32,
        rect.3.round().max(1.0) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upscale_requires_both_axes() {
        assert!(should_upscale((1920, 1080), (3840, 2160)));
        assert!(should_upscale((1280, 720), (1920, 1080)));
        // 1080p 铺满 1440p 窗口：1.33 倍，值得
        assert!(should_upscale((1920, 1080), (2560, 1440)));
        // 原尺寸（1:1）不该多跑两趟
        assert!(!should_upscale((1920, 1080), (1920, 1080)));
        // 轻微放大（<1.15）不值得
        assert!(!should_upscale((1920, 1080), (2000, 1125)));
        // 只有一个方向变大：矩形异常，走原路径
        assert!(!should_upscale((1920, 1080), (3840, 1080)));
        // 缩小时不启用（缩小的双线性已经够好）
        assert!(!should_upscale((3840, 2160), (1920, 1080)));
    }

    #[test]
    fn tiny_frames_never_upscale() {
        assert!(!should_upscale((0, 0), (3840, 2160)));
        assert!(!should_upscale((8, 8), (3840, 2160)));
        assert!(!should_upscale((1920, 1080), (0, 0)));
        assert!(!should_upscale((1920, 1080), (8, 8)));
    }

    #[test]
    fn config_sanitizes_sharpness() {
        let cfg = UpscaleConfig {
            upscaler: Upscaler::Fsr1,
            rcas_stops: f32::NAN,
        };
        assert_eq!(cfg.sanitized().rcas_stops, 2.0);
        let cfg = UpscaleConfig {
            upscaler: Upscaler::Fsr1,
            rcas_stops: -5.0,
        };
        assert_eq!(cfg.sanitized().rcas_stops, 0.0);
        let cfg = UpscaleConfig {
            upscaler: Upscaler::Fsr1,
            rcas_stops: 9.0,
        };
        assert_eq!(cfg.sanitized().rcas_stops, 4.0);
    }

    /// WGSL 只在创建管线时编译，CI 的无 GPU runner 跑不到那一步；
    /// 用 naga 解析 + 校验，把语法/类型错误挡在门禁里（与 yuv shader 同法）
    #[test]
    fn fsr_shader_parses_and_validates() {
        let source = include_str!("shader/fsr.wgsl");
        let module = naga::front::wgsl::parse_str(source).expect("WGSL 必须能被解析");
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );
        validator.validate(&module).expect("WGSL 模块必须通过校验");
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::*;

    /// 拿一个真实适配器；无 GPU 的 CI 上返回 None（测试跳过）。
    fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok()?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("fsr_test_device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            experimental_features: Default::default(),
            memory_hints: Default::default(),
            trace: Default::default(),
        }))
        .ok()?;
        Some((device, queue))
    }

    /// 在离屏纹理上跑 EASU + RCAS，返回读回的 RGBA8 像素。
    ///
    /// 目标用 `Rgba8Unorm`（RCAS 管线按目标格式建），中间纹理仍是 RGBA16F。
    fn run_fsr(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        src: &[u8],
        src_size: u32,
        dst_size: u32,
    ) -> Vec<[u8; 4]> {
        let src_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test_src"),
            size: wgpu::Extent3d {
                width: src_size,
                height: src_size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &src_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            src,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(src_size * 4),
                rows_per_image: Some(src_size),
            },
            wgpu::Extent3d {
                width: src_size,
                height: src_size,
                depth_or_array_layers: 1,
            },
        );

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test_target"),
            size: wgpu::Extent3d {
                width: dst_size,
                height: dst_size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut stage = FsrStage::new(device, wgpu::TextureFormat::Rgba8Unorm);
        // 直接喂 RGB：把 src_tex 当成 EASU 的输入，跳过 YUV 那一趟

        let src_view = src_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("test_src_bind"),
            layout: &stage.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&stage.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: stage.params.as_entire_binding(),
                },
            ],
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("test_encoder"),
        });
        stage.render_with_source(
            device,
            queue,
            &mut encoder,
            &target_view,
            &bind,
            (0.0, 0.0, dst_size as f32, dst_size as f32),
            2.0,
        );

        let padded = (dst_size as usize * 4).div_ceil(256) * 256;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * dst_size as usize) as u64,
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
                    bytes_per_row: Some(padded as u32),
                    rows_per_image: Some(dst_size),
                },
            },
            wgpu::Extent3d {
                width: dst_size,
                height: dst_size,
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
        let mut out = Vec::with_capacity(dst_size as usize * dst_size as usize);
        for row in 0..dst_size as usize {
            for col in 0..dst_size as usize {
                let o = row * padded + col * 4;
                out.push([data[o], data[o + 1], data[o + 2], data[o + 3]]);
            }
        }
        out
    }

    /// 平坦画面经 EASU + RCAS 后仍是同一颜色：不该引入振铃或色偏。
    #[test]
    fn flat_source_stays_flat() {
        let Some((device, queue)) = gpu() else {
            eprintln!("SKIP: 没有可用的 GPU 适配器");
            return;
        };
        let src = vec![128u8; 4 * 4 * 4];
        let out = run_fsr(&device, &queue, &src, 4, 8);
        for px in &out {
            assert!((px[0] as i32 - 128).abs() <= 2, "平坦输入被改变：{px:?}");
            assert_eq!(px[0], px[1], "灰阶不该出现色偏：{px:?}");
        }
    }

    /// 2x 放大的黑白竖直边缘：边缘两侧应保持接近 0 / 255，而不是被双线性
    /// 抹成两列平均灰。
    #[test]
    fn sharp_edge_survives_upscale() {
        let Some((device, queue)) = gpu() else {
            eprintln!("SKIP: 没有可用的 GPU 适配器");
            return;
        };
        let n = 8u32;
        let mut src = vec![0u8; (n * n * 4) as usize];
        for y in 0..n {
            for x in 0..n {
                let v = if x < n / 2 { 0u8 } else { 255u8 };
                let o = ((y * n + x) * 4) as usize;
                src[o] = v;
                src[o + 1] = v;
                src[o + 2] = v;
                src[o + 3] = 255;
            }
        }
        let out = run_fsr(&device, &queue, &src, n, n * 2);
        let row = &out[(n as usize) * (n as usize * 2)..(n as usize + 1) * (n as usize * 2)];
        // 中间两列是边缘：至少一侧要接近极值，不能全落在 64..192 的灰带里
        let mid_l = row[n as usize - 1][0];
        let mid_r = row[n as usize][0];
        assert!(
            mid_l < 100 && mid_r > 155,
            "边缘被抹平：左 {mid_l}，右 {mid_r}（双线性的典型结果是两侧都约 128）"
        );
    }
}
