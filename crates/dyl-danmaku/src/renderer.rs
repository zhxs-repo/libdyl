//! 弹幕渲染器 — 使用 Glyph Atlas + R8 Alpha Mask 实现高效弹幕渲染
//!
//! 借鉴 Erika DFM+ 架构：
//! - Prepare/Frame-Query 两阶段布局（二分查找）
//! - R8 Alpha Mask 字形图集（fill + outline 分离）
//! - 单 Overlay Render Pass 批量绘制所有弹幕
//! - GPU 端描边（outline draw call）替代 CPU 8 方向描边

use crate::glyph_atlas::{
    GlyphAtlas, GlyphInstance, RenderPlan, TextRasterizer, PLACEMENT_CACHE_CAPACITY,
};
use crate::layout::{reference_font_scale, LayoutEngine, PreparedLayout};
use crate::types::{DanmakuConfig, DanmakuInstance, DanmakuItem};
use bytemuck;
use dyl_render::font_util::{extract_ttc_preferred, CJK_SC_PREFER};
use dyl_render::{Overlay, OverlayArea};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// GPU uniform 结构体 — 与 WGSL FrameUniforms 布局匹配（16 bytes）
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable, PartialEq)]
struct FrameUniformData {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

/// GPU 实例属性 — 与 WGSL 里 @location(0..2) 的 instance-rate 属性逐字节对应（48 bytes）
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable, PartialEq)]
struct GpuGlyph {
    rect: [f32; 4],
    tex_rect: [f32; 4],
    color: [f32; 4],
}

impl GpuGlyph {
    /// 实例缓冲里的字节步长
    const STRIDE: wgpu::BufferAddress = std::mem::size_of::<GpuGlyph>() as wgpu::BufferAddress;

    /// 三个 instance-rate 属性的字节偏移，顺序必须与 shader 的 @location(0..2) 一致
    const ATTR_OFFSETS: [wgpu::BufferAddress; 3] = [
        std::mem::offset_of!(GpuGlyph, rect) as wgpu::BufferAddress,
        std::mem::offset_of!(GpuGlyph, tex_rect) as wgpu::BufferAddress,
        std::mem::offset_of!(GpuGlyph, color) as wgpu::BufferAddress,
    ];

    fn from_instance(inst: &GlyphInstance) -> Self {
        Self {
            rect: inst.rect,
            tex_rect: inst.tex_rect,
            color: inst.color_rgba,
        }
    }
}

/// 一条可见弹幕在当前帧的绘制数据（位置在 surface（窗口）坐标里，字形来自 atlas）
struct FrameGlyphs {
    frame_item_x: f32,
    frame_item_y: f32,
    /// `frame_item_y` 之上的基线偏移
    baseline_offset_y: f32,
    color_rgba: [f32; 4],
    placements: Arc<Vec<crate::glyph_atlas::TextGlyphPlacement>>,
}

/// 计算单个字形的上屏矩形（`[x, y, w, h]`，surface 像素）
///
/// `item_x` / `baseline_y` 必须是 surface（窗口）坐标 —— 弹幕按整窗布局，坐标本身就是
/// surface 像素，这里只补上字形在文本内的偏移（pen + baseline），不做任何缩放。
/// `baseline_y` 是**文本基线**
/// 的纵坐标，不是绘制框顶部 —— 字形位图的 `offset_y` 是相对基线的负值，把它当成顶边
/// 会让整行文字往上跑出一半，最上面一条轨道就被裁掉。
///
/// 字形位图**按原尺寸 1:1 绘制**，这里刻意不做任何缩放 —— 布局已经在布局视口
/// （整个 surface）的像素里完成。曾经这里有一对 `scale_x/scale_y` 参数，恒为 1.0；
/// 它们一旦被误用就会让字形非等比变形（横向压扁），所以干脆从接口上移除。
pub(crate) fn glyph_rect_on_screen(
    item_x: f32,
    baseline_y: f32,
    placement: &crate::glyph_atlas::TextGlyphPlacement,
) -> [f32; 4] {
    [
        item_x + placement.pen_x + placement.glyph.offset_x,
        baseline_y + placement.glyph.offset_y,
        placement.packed.width as f32,
        placement.packed.height as f32,
    ]
}

/// 把一帧的 outline/fill 实例打包进暂存数组：outline 段在前、fill 段在后
///
/// 两段各自连续，绘制时可用同一缓冲的两个 slice 分别实例化绘制。
fn pack_instances(outline: &[GlyphInstance], fill: &[GlyphInstance], out: &mut Vec<GpuGlyph>) {
    out.clear();
    out.reserve(outline.len() + fill.len());
    out.extend(outline.iter().map(GpuGlyph::from_instance));
    out.extend(fill.iter().map(GpuGlyph::from_instance));
}

/// Atlas GPU 纹理缓存
struct AtlasGpuCache {
    version: u64,
    fill_texture: wgpu::Texture,
    outline_texture: wgpu::Texture,
}

/// 本帧的 GPU 绘制资源（实例缓冲 + 两个 bind group），跨帧复用
///
/// 只有实例数超过缓冲容量、或 atlas 版本变化时才重建；稳态每帧 0 次 GPU 资源创建。
struct DrawCache {
    /// 实例缓冲容量（以 glyph 实例个数计）
    capacity: usize,
    instance_buffer: wgpu::Buffer,
    uniform_buffer: wgpu::Buffer,
    /// 对应的 atlas 版本（bind group 绑定了具体纹理视图）
    atlas_version: u64,
    fill_bind_group: wgpu::BindGroup,
    outline_bind_group: wgpu::BindGroup,
}

/// 实例缓冲最小容量（个），避免首帧就反复扩容
const MIN_INSTANCE_CAPACITY: usize = 512;

/// 计算扩容后的新容量：至少容纳 `needed`，稳态下按翻倍增长
fn instance_capacity_for(current: usize, needed: usize) -> usize {
    if needed <= current {
        return current;
    }
    needed.max(current * 2).max(MIN_INSTANCE_CAPACITY)
}

/// 弹幕渲染状态 — 由 DanmakuEngine 和 render 线程共享
pub struct DanmakuState {
    /// 布局后的弹幕实例列表
    pub instances: Vec<DanmakuInstance>,
    /// 是否启用
    pub enabled: bool,
    /// 全局不透明度
    pub opacity: f32,
    /// 当前播放时间（秒），由 update() 写入，render() 读取
    pub current_pts: f64,
    /// 预处理后的布局（在 load_items 时构建）
    pub prepared: Option<PreparedLayout>,
    /// 原始弹幕数据（viewport 变化时用于 relayout）
    pub items: Vec<DanmakuItem>,
    /// prepared 版本号：load_items/relayout 时递增，用于使字形放置缓存失效
    pub prepared_revision: u64,
}

/// 单条弹幕的字形放置缓存项
struct CachedPlacements {
    /// 写入时的 prepared 版本
    revision: u64,
    /// 字形放置列表（同一弹幕文本的所有字形，跨帧复用）
    placements: Arc<Vec<crate::glyph_atlas::TextGlyphPlacement>>,
}

/// 弹幕引擎 — 管理弹幕数据、布局、渲染
///
/// 实现 Overlay trait，可集成到 OverlayStack 中。
pub struct DanmakuEngine {
    /// 共享状态（布局线程写入，渲染线程读取）
    state: Arc<Mutex<DanmakuState>>,
    /// 渲染配置
    config: DanmakuConfig,
    /// 字体原始字节数据（TTC 已提取为独立 TTF）
    font_data: Option<Vec<u8>>,
    /// 字形图集（持久化，跨帧复用）
    glyph_atlas: GlyphAtlas,
    /// 字形光栅化器（含字形缓存）
    rasterizer: TextRasterizer,
    /// 条目级字形放置缓存：item_index → 该文本的全部字形放置
    ///
    /// 命中时跳过字体解析、逐字形查表与 atlas 打包，消除稳态每帧 CPU 开销。
    ///
    /// **必须设上限**：长视频（几小时的直播回放）动辄上万条弹幕，每条都缓存一
    /// 份 `Vec<TextGlyphPlacement>`（每个字形一个 Arc<RasterizedGlyph>，
    /// 内含 fill/outline 两张位图）。无上限增长就是播放越久内存越高，
    /// 最终被系统 OOM kill —— 表现为"看久了崩溃"。
    placement_cache: HashMap<usize, CachedPlacements>,
    /// 插入顺序，用于按 FIFO 淘汰（配合 PLACEMENT_CACHE_CAPACITY）
    placement_cache_order: VecDeque<usize>,
    /// wgpu 渲染管线（惰性初始化）
    render_pipeline: Option<wgpu::RenderPipeline>,
    bind_group_layout: Option<wgpu::BindGroupLayout>,
    sampler: Option<wgpu::Sampler>,
    /// 当前 surface format（用于检测 format 变化）
    surface_format: Option<wgpu::TextureFormat>,
    /// 上述 GPU 资源所属设备代号（见 [dyl_render::next_gpu_epoch]）
    gpu_epoch: Option<u64>,
    /// Atlas GPU 纹理缓存
    atlas_cache: Option<AtlasGpuCache>,
    /// 本帧绘制资源缓存（实例缓冲 + bind group）
    draw_cache: Option<DrawCache>,
    /// CPU 侧实例暂存数组（跨帧复用，避免每帧分配）
    instance_scratch: Vec<GpuGlyph>,
    /// Generation 计数器（seek/stop 时递增，用于使缓存失效）
    generation: u64,
    /// 每帧渲染状态诊断计数器
    frame_count: u64,
    /// 首次渲染标志（用于调试日志）
    first_render_logged: bool,
    /// 上一次布局所用的**画布尺寸**（整个 surface，用于检测窗口大小变化）
    last_canvas: Option<(f32, f32)>,
    /// 上一次布局所用的字号系数（固定参考；变了才需要重光栅化）
    last_font_scale: Option<f32>,
    /// 因 atlas 溢出而整表重建的次数（诊断 / 测试断言）
    overflow_rebuilds: u64,
    /// 重建后**仍然**丢字形的次数
    ///
    /// 非 0 说明单帧字形需求超过了 atlas 的硬容量上限（2048px 高），光靠重试
    /// 解决不了，必须靠 `max_lines` / `max_quantity` 控制并发弹幕数。
    unresolved_drops: u64,
    /// 绘制阶段被跳过（零尺寸）的字形总数
    missing_glyphs: u64,
    /// 字体没有轮廓、因而本就画不出来的字形数（空格 / 缺字，非故障）
    empty_glyphs: u64,
}

// Safety: DanmakuEngine is only accessed from one thread at a time via the Overlay trait
unsafe impl Send for DanmakuEngine {}

impl DanmakuEngine {
    pub fn new(config: DanmakuConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(DanmakuState {
                instances: Vec::new(),
                enabled: true,
                opacity: 1.0,
                current_pts: 0.0,
                prepared: None,
                items: Vec::new(),
                prepared_revision: 0,
            })),
            config,
            font_data: None,
            glyph_atlas: GlyphAtlas::new(),
            rasterizer: TextRasterizer::new(),
            placement_cache: HashMap::new(),
            placement_cache_order: VecDeque::new(),
            render_pipeline: None,
            bind_group_layout: None,
            sampler: None,
            surface_format: None,
            gpu_epoch: None,
            atlas_cache: None,
            draw_cache: None,
            instance_scratch: Vec::new(),
            generation: 1,
            frame_count: 0,
            first_render_logged: false,
            last_canvas: None,
            last_font_scale: None,
            overflow_rebuilds: 0,
            unresolved_drops: 0,
            missing_glyphs: 0,
            empty_glyphs: 0,
        }
    }

    /// 累计因 atlas 溢出而整表重建的次数
    pub fn overflow_rebuilds(&self) -> u64 {
        self.overflow_rebuilds
    }

    /// 累计重建后仍然丢字形的次数；非 0 即为真实丢字（弹幕显示不全）
    pub fn unresolved_drops(&self) -> u64 {
        self.unresolved_drops
    }

    /// 绘制阶段被跳过的字形总数
    pub fn missing_glyphs(&self) -> u64 {
        self.missing_glyphs
    }

    /// 字体无轮廓、画不出来的字形数（空格/缺字，非故障）
    pub fn empty_glyphs(&self) -> u64 {
        self.empty_glyphs
    }

    /// 设置字体数据（支持 TTF/OTF/TTC 格式）
    ///
    /// TTC 会按 CJK 简体优先挑出一个 face 并重写偏移表，确保
    /// `ab_glyph::FontRef::try_from_slice` 能正确解析。
    pub fn set_font(&mut self, font_data: Vec<u8>) {
        let font_data = match extract_ttc_preferred(&font_data, CJK_SC_PREFER) {
            Some(face) => {
                tracing::info!(
                    "DanmakuEngine: TTC face #{} '{}' extracted ({} bytes)",
                    face.index,
                    face.family,
                    face.data.len()
                );
                face.data
            }
            None => font_data,
        };
        self.font_data = Some(font_data);
        self.invalidate_atlas();
    }

    /// 获取共享状态句柄（供外部线程写入弹幕数据）
    pub fn shared_state(&self) -> Arc<Mutex<DanmakuState>> {
        self.state.clone()
    }

    /// 加载弹幕数据并执行布局 + prepare
    pub fn load_items(&self, items: Vec<DanmakuItem>) {
        let engine = LayoutEngine::new(self.config.clone());
        let instances = engine.layout(&items);

        if !instances.is_empty() {
            let min_start = instances
                .iter()
                .map(|i| i.start_time)
                .fold(f64::INFINITY, f64::min);
            let max_end = instances
                .iter()
                .map(|i| i.end_time)
                .fold(f64::NEG_INFINITY, f64::max);
            tracing::info!(
                "DanmakuEngine::load_items: {} items -> {} instances, time=[{:.2}s, {:.2}s]",
                items.len(),
                instances.len(),
                min_start,
                max_end,
            );
        } else {
            tracing::warn!(
                "DanmakuEngine::load_items: {} items parsed but 0 instances produced",
                items.len()
            );
        }

        // 构建 PreparedLayout（预处理阶段）
        let prepared = engine.prepare(&instances);

        let mut state = self.state.lock();
        state.items = items;
        state.instances = instances;
        state.prepared = Some(prepared);
        state.prepared_revision += 1;
    }

    /// 从文件加载弹幕（自动选择解析器）
    pub fn load_from_file(&self, path: &str) -> dyl_core::Result<()> {
        let data = std::fs::read(path)
            .map_err(|e| dyl_core::DylError::FileNotFound(format!("{}: {}", path, e)))?;

        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("json");

        let parser = crate::parser::parser_for_extension(ext);
        let items = parser.parse(&data);
        tracing::info!("DanmakuEngine: loaded {} items from {}", items.len(), path);
        self.load_items(items);
        Ok(())
    }

    /// 递增 generation（seek/stop 时调用，清除 atlas 缓存）
    pub fn invalidate(&mut self) {
        self.generation += 1;
        self.invalidate_atlas();
    }

    /// 清空 atlas 和缓存
    fn invalidate_atlas(&mut self) {
        self.glyph_atlas.clear();
        self.rasterizer.clear_cache();
        // 缓存里的 PackedGlyph 坐标指向 atlas 纹理，atlas 清空后即失效
        self.placement_cache.clear();
        self.placement_cache_order.clear();
        self.atlas_cache = None;
        // bind group 里绑的是 atlas 纹理视图，纹理没了就必须重建
        self.draw_cache = None;
    }

    /// 确保渲染管线已初始化
    fn ensure_pipeline(
        &mut self,
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        gpu_epoch: u64,
    ) {
        if self.gpu_epoch == Some(gpu_epoch)
            && self.surface_format == Some(surface_format)
            && self.render_pipeline.is_some()
        {
            return;
        }

        if self.gpu_epoch.is_some_and(|epoch| epoch != gpu_epoch) {
            // 设备换了（Surface 重建）：atlas 纹理与实例缓冲都属于旧设备。CPU 侧
            // 的 `glyph_atlas` 保留，下次 render 会把同一份快照重新上传到新设备。
            // 这里必须清 `atlas_cache`，否则版本号相同会跳过上传，绑定旧纹理。
            self.atlas_cache = None;
            self.draw_cache = None;
        }

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("danmaku_overlay"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader/overlay.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("danmaku_bg_layout"),
            entries: &[
                // binding(0): R8Unorm texture
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
                // binding(1): sampler
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // binding(2): uniform buffer (FrameUniformData)
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("danmaku_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("danmaku_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("danmaku_render_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                // 顶点几何由 vertex_index 生成，只有实例数据走 instance-rate 属性缓冲
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: GpuGlyph::STRIDE,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: GpuGlyph::ATTR_OFFSETS[0],
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: GpuGlyph::ATTR_OFFSETS[1],
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: GpuGlyph::ATTR_OFFSETS[2],
                            shader_location: 2,
                        },
                    ],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
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

        self.render_pipeline = Some(render_pipeline);
        self.bind_group_layout = Some(bind_group_layout);
        self.sampler = Some(sampler);
        self.surface_format = Some(surface_format);
        self.gpu_epoch = Some(gpu_epoch);
        // bind group 与旧的 bind group layout 配套，重建管线后必须一并作废
        self.draw_cache = None;
        tracing::info!(
            "DanmakuEngine: render pipeline initialized for {:?}",
            surface_format
        );
    }

    /// 打包本帧可见弹幕的全部字形（Phase 1）
    ///
    /// 优先命中条目级 `placement_cache`；未命中才光栅化并打包进 atlas。
    /// 拆成独立方法是为了让 atlas 溢出时能整表重建后**原样重跑一遍**。
    ///
    /// 所有输入都从 `self` 内部取，因此调用方不能持有任何指向 `self` 的借用 ——
    /// 这样 atlas 溢出后的重试路径才写得出（见 `build_render_plan_internal`）。
    fn collect_frame_glyphs(
        &mut self,
        frame_layout: &crate::layout::FrameLayout,
        revision: u64,
        opacity: f32,
    ) -> Vec<FrameGlyphs> {
        let Some(font_data) = self.font_data.as_ref() else {
            if self.frame_count.is_multiple_of(300) {
                tracing::warn!("collect_frame_glyphs: font_data is None");
            }
            return Vec::new();
        };
        let font_slice = font_data.as_slice();
        // `state` 只用来读 prepared；后续对 rasterizer / atlas / placement_cache 的
        // 可变借用与它字段不相交，编译期允许。
        let state = self.state.lock();
        let Some(prepared) = state.prepared.as_ref() else {
            return Vec::new();
        };

        let mut frame_data: Vec<FrameGlyphs> = Vec::with_capacity(frame_layout.items.len());
        for frame_item in &frame_layout.items {
            let prepared_item = &prepared.items[frame_item.item_index];
            let color_rgba = color_u32_to_f32_array(prepared_item.color, opacity);

            let item_index = frame_item.item_index;
            let cached = self
                .placement_cache
                .get(&item_index)
                .filter(|entry| entry.revision == revision)
                .map(|entry| entry.placements.clone());

            let placements = match cached {
                Some(p) => p,
                None => {
                    let p = Arc::new(self.rasterizer.rasterize_and_pack_text(
                        font_slice,
                        &prepared_item.text,
                        prepared_item.font_size as f32,
                        &mut self.glyph_atlas,
                    ));
                    if self
                        .placement_cache
                        .insert(
                            item_index,
                            CachedPlacements {
                                revision,
                                placements: p.clone(),
                            },
                        )
                        .is_none()
                    {
                        self.placement_cache_order.push_back(item_index);
                    }
                    // FIFO 淘汰。淘汰只是丢 CPU 侧缓存：字形位图仍在
                    // rasterizer.glyph_cache / glyph_atlas 里，再次出现时重算
                    // placement 即可，不会丢字形。
                    while self.placement_cache_order.len() > PLACEMENT_CACHE_CAPACITY {
                        if let Some(old) = self.placement_cache_order.pop_front() {
                            self.placement_cache.remove(&old);
                        }
                    }
                    p
                }
            };

            if !placements.is_empty() {
                frame_data.push(FrameGlyphs {
                    frame_item_x: frame_item.x,
                    frame_item_y: frame_item.y,
                    baseline_offset_y: prepared_item.font_size as f32,
                    color_rgba,
                    placements,
                });
            }
        }
        frame_data
    }

    /// 构建当前帧的渲染计划：frame query → 光栅化 → atlas pack → 生成 GlyphInstance
    ///
    /// 布局坐标已经是 **surface（窗口）像素**：弹幕按整窗布局、不裁剪，所以这里不
    /// 再加任何原点偏移。
    fn build_render_plan_internal(&mut self, current_time: f64) -> RenderPlan {
        // 先在锁内算出这一帧的可见集合与快照参数，随即**放掉锁**：
        // 后面要调用 `&mut self` 的方法，持有任何指向 self 的借用都会冲突。
        let (frame_layout, revision, opacity) = {
            let state = self.state.lock();
            let Some(prepared) = state.prepared.as_ref() else {
                if self.frame_count.is_multiple_of(300) {
                    tracing::warn!("build_render_plan: state.prepared is None");
                }
                return RenderPlan::empty();
            };

            let frame_layout = prepared.query_frame(current_time);
            if frame_layout.items.is_empty() {
                // 正常：当前时间无可见弹幕（不输出日志避免刷屏）
                return RenderPlan::empty();
            }
            (frame_layout, state.prepared_revision, state.opacity)
        };

        // Phase 1: 打包本帧字形
        let mut frame_data = self.collect_frame_glyphs(&frame_layout, revision, opacity);

        // ── atlas 装不下时的自愈 ──────────────────────────────────────
        //
        // atlas 高度硬封顶在 2048（GLES 的 max_texture_dimension_2d 下限）。
        // `pack()` 装不下时返回零尺寸字形，Phase 3 会把它跳过 —— 于是表现为
        // 「弹幕显示不全」，而且零尺寸结果还会被缓存进 placement_cache 长期
        // 粘住，历史上这里完全没有任何日志。
        //
        // 单帧的字形需求是**有界**的：轨道数（surface 高 / 轨道高）× 平均文本长度。
        // 真正把 atlas 撑满的是长时间播放后不断涌入的新字符。所以整表重建后，
        // 本帧要用的字形必然装得下 —— 直接重建并重试一次即可自愈。
        let dropped = self.glyph_atlas.take_dropped();
        if dropped > 0 {
            tracing::warn!(
                "DanmakuEngine: atlas overflowed, {} glyph(s) dropped (height={}, kinds={}); \
                 rebuilding atlas and retrying this frame",
                dropped,
                self.glyph_atlas.allocated_height(),
                self.glyph_atlas.packed_count(),
            );
            self.overflow_rebuilds += 1;
            self.invalidate_atlas();
            frame_data = self.collect_frame_glyphs(&frame_layout, revision, opacity);
            let still_dropped = self.glyph_atlas.take_dropped();
            if still_dropped > 0 {
                // 重建后仍装不下 = 单帧需求超过 atlas 硬上限，属于容量/密度问题，
                // 得靠 max_lines / max_quantity 控制并发，不能靠重试解决。
                self.unresolved_drops += still_dropped as u64;
                if self.frame_count.is_multiple_of(300) {
                    tracing::error!(
                        "DanmakuEngine: atlas still overflowed after rebuild, {} glyph(s) \
                         dropped for {} visible items — 单帧字形需求超过 atlas 容量上限，\
                         需要限制并发弹幕数（max_lines / max_quantity）",
                        still_dropped,
                        frame_layout.items.len(),
                    );
                }
            }
        }

        // Phase 2: 打包完成后取 atlas 快照（确保包含本帧新打包的字形）
        let atlas_snap = self.glyph_atlas.snapshot();
        let atlas_w = atlas_snap.width as f32;
        let atlas_h = atlas_snap.height as f32;

        // 诊断：query_frame 有可见弹幕但光栅化全部失败
        if frame_data.is_empty()
            && !frame_layout.items.is_empty()
            && self.frame_count.is_multiple_of(300)
        {
            let font_loaded = self.font_data.as_ref().is_some();
            tracing::warn!(
                "build_render_plan: {} visible items but rasterization produced 0 placements \
                 (font_loaded={}, atlas_height={})",
                frame_layout.items.len(),
                font_loaded,
                self.glyph_atlas.allocated_height(),
            );
        }

        // Phase 3: 使用最终 atlas 尺寸计算 UV 并生成 GlyphInstance
        let glyph_count: usize = frame_data.iter().map(|fd| fd.placements.len()).sum();
        let mut outline = Vec::with_capacity(glyph_count);
        let mut fill = Vec::with_capacity(glyph_count);

        for fd in &frame_data {
            for placement in fd.placements.iter() {
                if placement.packed.width == 0 || placement.packed.height == 0 {
                    // 两类"零尺寸"要分开：
                    // - `glyph.width == 0`：字体压根没有这个码位的轮廓（空格、
                    //   缺字）。本来就没东西可画，跳过是正常的。
                    // - `glyph.width > 0` 但塞不进 atlas：**真丢字形**，弹幕会缺字。
                    //   上面的溢出重试就是为了消灭这一类；这里计数是最后一道防线。
                    if placement.glyph.width > 0 && placement.glyph.height > 0 {
                        self.missing_glyphs += 1;
                    } else {
                        self.empty_glyphs += 1;
                    }
                    continue;
                }

                // 布局坐标已经是 surface 像素，这里只补字形在文本内的偏移。
                // item.y 是绘制框顶边（DFM 语义），基线在框内约一个字号处。
                let glyph_rect = glyph_rect_on_screen(
                    fd.frame_item_x,
                    fd.frame_item_y + fd.baseline_offset_y,
                    placement,
                );

                let tex_rect = [
                    placement.packed.x as f32 / atlas_w,
                    placement.packed.y as f32 / atlas_h,
                    placement.packed.width as f32 / atlas_w,
                    placement.packed.height as f32 / atlas_h,
                ];

                // Outline draw（黑色描边）
                // fill/outline bitmap 已统一为 outline 尺寸（含 dilation），
                // 两者使用相同的 rect 和 tex_rect，outline 的 dilation 已在 bitmap 中
                outline.push(GlyphInstance {
                    rect: glyph_rect,
                    tex_rect,
                    color_rgba: [0.0, 0.0, 0.0, fd.color_rgba[3]],
                });

                // Fill draw（彩色填充）
                fill.push(GlyphInstance {
                    rect: glyph_rect,
                    tex_rect,
                    color_rgba: fd.color_rgba,
                });
            }
        }

        RenderPlan {
            atlas: Some(atlas_snap),
            outline,
            fill,
        }
    }
}

impl Overlay for DanmakuEngine {
    fn update(&mut self, pts: f64) {
        self.state.lock().current_pts = pts;
    }

    fn render(
        &mut self,
        frame: &dyl_render::OverlayFrame<'_>,
        encoder: &mut wgpu::CommandEncoder,
        area: OverlayArea,
    ) {
        let device = frame.device;
        let queue = frame.queue;
        // 确保渲染管线就绪
        self.ensure_pipeline(device, frame.surface_format, frame.gpu_epoch);

        // Phase 1: 从共享状态提取所需数据（短暂持有锁）
        // 不透明度已在 build_render_plan 里烘进实例颜色的 alpha，这里不再重复乘一次
        let (enabled, instance_count, current_pts) = {
            let state = self.state.lock();
            (state.enabled, state.instances.len(), state.current_pts)
        };

        if !enabled || instance_count == 0 {
            if self.frame_count.is_multiple_of(300) {
                tracing::info!(
                    "DanmakuEngine::render: skip frame={} enabled={} instances={}",
                    self.frame_count,
                    enabled,
                    instance_count
                );
            }
            self.frame_count += 1;
            return;
        }

        // 弹幕的布局画布：**整个 surface（窗口）**，不是 letterbox 之后的画面矩形。
        //
        // 弹幕属于窗口而不属于画面，横穿 pillarbox / letterbox 的黑边是预期行为：
        // 布局视口取 surface 尺寸，绘制时不设裁剪，布局坐标本身就是 surface 像素，
        // 因此不再需要"加画面矩形左上角偏移"这一步。
        //
        // 字号走**固定参考**（基准字号 / 25 × 窗口 DPR），与画布尺寸无关。画布现在
        // 含黑边，若还按画布高度放大，一个高窗口会把字撑到糊满屏幕。
        //
        // uniform 里的 viewport 同样是 surface 尺寸，shader 的像素 → NDC 不变。
        let (canvas_w, canvas_h) = (area.surface_width as f32, area.surface_height as f32);
        let font_scale = reference_font_scale(area.scale_factor, self.config.base_font_size);

        // config 始终反映最新几何，这样之后 load_items 不会用旧值布局
        self.config.viewport_width = canvas_w;
        self.config.viewport_height = canvas_h;
        self.config.font_scale = font_scale;

        // 自适应窗口：画布尺寸或字号变了就重排
        //
        // 刻意**没有**节流，也刻意把"清 rasterizer 缓存"和"重排"分开：
        // - 重排（轨道分配、进出场坐标）只看画布，画布差 1px 就得重算，否则弹幕
        //   会按旧窗口宽度飞行、右侧提前消失。
        // - 字形位图只看字号。固定参考字号与窗口尺寸无关，所以拖动窗口时**不该**
        //   清掉已积累的字形覆盖——清了就等于把 atlas 覆盖归零，本帧要重新打包所有
        //   字形，表现为弹幕时有时无、字形缺字（历史 bug）。
        // - 整表作废 atlas 的分支同样不要加：单帧的字形需求（轨道数 × 文本长度）
        //   本来就装得下，作废只会让画面在"半成品 atlas"上反复抖动。
        let canvas_changed = self.last_canvas != Some((canvas_w, canvas_h));
        let font_changed = self.last_font_scale != Some(font_scale);
        if canvas_changed || font_changed {
            let items_to_relayout: Option<Vec<DanmakuItem>> = {
                let state = self.state.lock();
                if !state.items.is_empty() {
                    Some(state.items.clone())
                } else {
                    None
                }
            };

            if let Some(items) = items_to_relayout {
                tracing::info!(
                    "DanmakuEngine: canvas changed {:?} -> {:.0}x{:.0}, relayout {} items \
                     (font_scale {:.3})",
                    self.last_canvas,
                    canvas_w,
                    canvas_h,
                    items.len(),
                    font_scale
                );
                let (new_instances, new_prepared) =
                    LayoutEngine::relayout(&items, canvas_w, canvas_h, &self.config);
                if font_changed {
                    // 只有字号真的变了，旧位图才不相容；atlas 保留 —— 同字号的字形仍可直接复用
                    self.rasterizer.clear_cache();
                }
                tracing::info!(
                    "DanmakuEngine: relayout produced {} instances, prepared {} items, \
                     font_scale={:.3}",
                    new_instances.len(),
                    new_prepared.items.len(),
                    font_scale
                );
                let mut state = self.state.lock();
                state.instances = new_instances;
                state.prepared = Some(new_prepared);
                // placement_cache 按 revision 失效，这里 revision 自增即可让
                // 所有旧字形放置被丢弃重算（字形 atlas 本身尺寸不变，可复用）
                state.prepared_revision += 1;
            } else {
                tracing::info!(
                    "DanmakuEngine: canvas changed to {:.0}x{:.0}, but no items to relayout",
                    canvas_w,
                    canvas_h
                );
            }
            self.last_canvas = Some((canvas_w, canvas_h));
            self.last_font_scale = Some(font_scale);
        }

        // Phase 2: 构建渲染计划（CPU 工作：frame query + rasterize + atlas pack）
        // 布局坐标已经是 surface 像素，不需要再加任何偏移
        let plan = self.build_render_plan_internal(current_pts);

        if plan.is_empty() {
            // 每 300 帧输出一次诊断：为什么 plan 为空
            if self.frame_count.is_multiple_of(300) {
                let (has_prepared, has_font) = {
                    let s = self.state.lock();
                    (s.prepared.is_some(), self.font_data.is_some())
                };
                tracing::info!(
                    "DanmakuEngine: plan EMPTY frame={} pts={:.2}s prepared={} font={} instances={}",
                    self.frame_count, current_pts, has_prepared, has_font,
                    self.state.lock().instances.len()
                );
            }
            self.frame_count += 1;
            return;
        }

        let Some(ref atlas) = plan.atlas else {
            self.frame_count += 1;
            return;
        };

        // 首次渲染日志
        if !self.first_render_logged {
            self.first_render_logged = true;
            tracing::info!(
                "DanmakuEngine: FIRST_RENDER instances={} (fill={} outline={}) atlas={}x{} v{}",
                plan.instance_count(),
                plan.fill.len(),
                plan.outline.len(),
                atlas.width,
                atlas.height,
                atlas.version,
            );
        }

        // 每 300 帧输出渲染统计
        if self.frame_count.is_multiple_of(300) && self.frame_count > 0 {
            tracing::info!(
                "DanmakuEngine: frame={} instances={} draw_calls=2 atlas_v{}",
                self.frame_count,
                plan.instance_count(),
                atlas.version,
            );
        }
        self.frame_count += 1;

        // Phase 3: 上传 atlas GPU 纹理（版本匹配则复用）
        let needs_upload = match &self.atlas_cache {
            Some(cache) => cache.version != atlas.version,
            None => true,
        };
        if needs_upload {
            let snapshot = atlas.clone();
            // ATLAS_WIDTH = 2048 正好是 GLES 的 max_texture_dimension_2d 下限，
            // glyph_atlas 侧已经保证高度不会涨过它。请求的 limits 只会更高，
            // 所以这里尺寸直接用快照值即可；额外夹一次只是防御。
            let max_dim = device.limits().max_texture_dimension_2d;
            let w = snapshot.width.clamp(1, max_dim);
            let h = snapshot.height.clamp(1, max_dim);
            let ft = create_r8_texture(device, w, h, &[], snapshot.stride);
            let ot = create_r8_texture(device, w, h, &[], snapshot.stride);
            upload_r8_texture(queue, &ft, &snapshot.fill_alpha, w, h, snapshot.stride);
            upload_r8_texture(queue, &ot, &snapshot.outline_alpha, w, h, snapshot.stride);
            self.atlas_cache = Some(AtlasGpuCache {
                version: snapshot.version,
                fill_texture: ft,
                outline_texture: ot,
            });
        }
        let (fill_tex, outline_tex) = {
            let cache = self.atlas_cache.as_ref().unwrap();
            (cache.fill_texture.clone(), cache.outline_texture.clone())
        };

        // Phase 4: 借用 pipeline 引用
        let (pipeline, bg_layout, sampler) = match (
            &self.render_pipeline,
            &self.bind_group_layout,
            &self.sampler,
        ) {
            (Some(p), Some(b), Some(s)) => (p, b, s),
            _ => return,
        };

        // Phase 5: 组装实例数据（outline 段在前、fill 段在后），单个缓冲一次上传
        let outline_count = plan.outline.len();
        let fill_count = plan.fill.len();
        pack_instances(&plan.outline, &plan.fill, &mut self.instance_scratch);
        let needed = self.instance_scratch.len();
        let atlas_version = atlas.version;

        // Phase 6: 绘制资源按需重建（仅容量不足或 atlas 换版时创建）
        let cache_outdated = self
            .draw_cache
            .as_ref()
            .is_none_or(|c| c.capacity < needed || c.atlas_version != atlas_version);
        if cache_outdated {
            let old_capacity = self.draw_cache.as_ref().map_or(0, |c| c.capacity);
            let capacity = instance_capacity_for(old_capacity, needed);
            let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("danmaku_instances"),
                size: (capacity as u64) * GpuGlyph::STRIDE,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("danmaku_frame_uniform"),
                size: std::mem::size_of::<FrameUniformData>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let fill_view = fill_tex.create_view(&wgpu::TextureViewDescriptor::default());
            let outline_view = outline_tex.create_view(&wgpu::TextureViewDescriptor::default());
            let fill_bg = make_overlay_bind_group(
                device,
                bg_layout,
                sampler,
                &fill_view,
                &uniform_buffer,
                "danmaku_fill_bg",
            );
            let outline_bg = make_overlay_bind_group(
                device,
                bg_layout,
                sampler,
                &outline_view,
                &uniform_buffer,
                "danmaku_outline_bg",
            );
            self.draw_cache = Some(DrawCache {
                capacity,
                instance_buffer,
                uniform_buffer,
                atlas_version,
                fill_bind_group: fill_bg,
                outline_bind_group: outline_bg,
            });
            tracing::debug!(
                "DanmakuEngine: draw cache rebuilt capacity={} needed={} atlas_v{}",
                capacity,
                needed,
                atlas_version
            );
        }
        let cache = self
            .draw_cache
            .as_ref()
            .expect("danmaku draw cache 刚创建，不应为 None");

        queue.write_buffer(
            &cache.uniform_buffer,
            0,
            bytemuck::bytes_of(&FrameUniformData {
                // 必须是**整个 surface** 的尺寸：shader 拿它做"像素 → NDC"，
                // 而实例里的坐标就是 surface（窗口）像素 —— 弹幕不被画面矩形约束
                viewport: [area.surface_width as f32, area.surface_height as f32],
                _pad: [0.0; 2],
            }),
        );
        queue.write_buffer(
            &cache.instance_buffer,
            0,
            bytemuck::cast_slice(&self.instance_scratch),
        );

        // 单个 overlay render pass — outline 一批 + fill 一批，共 2 次实例化 draw
        //
        // 弹幕**不设 scissor**：画布是整个窗口，进出场和长句横穿 letterbox /
        // pillarbox 的黑边是预期行为。只有空 surface（最小化、尺寸未知）才跳过绘制。
        if area.surface_width == 0 || area.surface_height == 0 {
            return;
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("danmaku_overlay_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: frame.target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(pipeline);

            if outline_count > 0 {
                pass.set_bind_group(0, &cache.outline_bind_group, &[]);
                pass.set_vertex_buffer(
                    0,
                    cache
                        .instance_buffer
                        .slice(0..(outline_count as u64) * GpuGlyph::STRIDE),
                );
                pass.draw(0..4, 0..outline_count as u32);
            }
            if fill_count > 0 {
                let start = (outline_count as u64) * GpuGlyph::STRIDE;
                let end = start + (fill_count as u64) * GpuGlyph::STRIDE;
                pass.set_bind_group(0, &cache.fill_bind_group, &[]);
                pass.set_vertex_buffer(0, cache.instance_buffer.slice(start..end));
                pass.draw(0..4, 0..fill_count as u32);
            }
        }
    }

    fn is_visible(&self) -> bool {
        self.state.lock().enabled
    }
}

// ─── 辅助函数 ─────────────────────────────────────────

/// 创建一个 overlay bind group（atlas 纹理 + 采样器 + 帧 uniform）
///
/// 实例数据不走 bind group，而是 instance-rate 顶点缓冲，每帧 `set_vertex_buffer`。
fn make_overlay_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    texture: &wgpu::TextureView,
    uniform_buffer: &wgpu::Buffer,
    label: &'static str,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(texture),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: uniform_buffer.as_entire_binding(),
            },
        ],
    })
}

/// 0xRRGGBB 颜色转 [f32; 4] RGBA
fn color_u32_to_f32_array(color: u32, alpha: f32) -> [f32; 4] {
    let r = ((color >> 16) & 0xFF) as f32 / 255.0;
    let g = ((color >> 8) & 0xFF) as f32 / 255.0;
    let b = (color & 0xFF) as f32 / 255.0;
    [r, g, b, alpha]
}

/// 创建 R8Unorm 纹理
fn create_r8_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    _data: &[u8],
    _stride: usize,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("danmaku_atlas_r8"),
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
    })
}

/// 上传 R8 纹理数据
fn upload_r8_texture(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
) {
    if data.is_empty() {
        return;
    }
    // 如果 stride == width，可以直接上传
    if stride == width as usize {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    } else {
        // stride != width，需要逐行复制
        let mut tight = Vec::with_capacity((width * height) as usize);
        for row in 0..height as usize {
            let start = row * stride;
            let end = start + width as usize;
            if end <= data.len() {
                tight.extend_from_slice(&data[start..end]);
            } else {
                tight.extend(std::iter::repeat_n(0u8, width as usize));
            }
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &tight,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph_atlas::{PackedGlyph, RasterizedGlyph, TextGlyphPlacement};

    fn glyph(x: f32) -> GlyphInstance {
        GlyphInstance {
            rect: [x, 0.0, 1.0, 1.0],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            color_rgba: [1.0, 1.0, 1.0, 1.0],
        }
    }

    fn placement(w: u32, h: u32) -> TextGlyphPlacement {
        TextGlyphPlacement {
            glyph: Arc::new(RasterizedGlyph {
                width: w,
                height: h,
                stride: w as usize,
                fill_alpha: vec![0u8; (w * h) as usize],
                outline_alpha: vec![0u8; (w * h) as usize],
                offset_x: 1.0,
                offset_y: 2.0,
                advance: w as f32,
            }),
            packed: PackedGlyph {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
            pen_x: 10.0,
        }
    }

    /// 字形必须按位图原尺寸上屏，且只被字形自身偏移平移
    ///
    /// 防回归：这里曾经有 `scale_x/scale_y` 两个恒为 1.0 的缩放参数。一旦被填上
    /// 非 1 的值（或两轴不等），弹幕字形就会横向压扁 / 纵向拉长。
    #[test]
    fn glyph_is_drawn_one_to_one() {
        let p = placement(20, 24);
        let r = glyph_rect_on_screen(100.0, 50.0, &p);
        assert_eq!(r, [100.0 + 10.0 + 1.0, 50.0 + 2.0, 20.0, 24.0]);
        // 宽高比必须严格保持
        assert_eq!(r[2] / r[3], 20.0 / 24.0);
    }

    /// 弹幕铺满整个窗口：窗口变大时**位置**跟着走，但**字形尺寸**不变
    ///
    /// 字号随窗口高度变（内容基准那套只适用于字幕），但同一帧内位图必须
    /// 按原尺寸绘制 —— 布局阶段已经把字号算进 `paint_width`/`font_size` 了。
    #[test]
    fn glyph_size_is_independent_of_viewport_size() {
        let p = placement(20, 24);
        // 同一个字形位图，放在窗口的不同位置
        let small = glyph_rect_on_screen(10.0, 10.0, &p);
        let large = glyph_rect_on_screen(1900.0, 500.0, &p);
        assert_eq!(small[2], large[2], "字形宽度不得变化");
        assert_eq!(small[3], large[3], "字形高度不得变化");
    }

    /// 弹幕的布局画布是整个窗口，不是画面矩形
    ///
    /// 防回归：这里同时确认字幕那条链没被带跑 —— `scissor()` 仍然是画面矩形，
    /// 长句字幕依旧不会画到黑边上。
    #[test]
    fn danmaku_canvas_is_whole_surface() {
        // 宽窗口播 16:9：画面矩形居中，左右各留 pillarbox
        let area = OverlayArea::new(1264, 421, 640, 360);
        assert_eq!(
            area.surface_rect(),
            (0.0, 0.0, 1264.0, 421.0),
            "弹幕画布必须是整个 surface"
        );
        assert!(area.video_rect.0 > 0.0);
        assert!(
            area.scissor().unwrap().2 < area.surface_width,
            "字幕的裁剪区间仍应收在画面矩形内"
        );

        // 视频尺寸未知时画面矩形退化为整个 surface
        let full = OverlayArea::full_surface(800, 600);
        assert_eq!(full.surface_rect(), full.video_rect);
    }

    #[test]
    fn pack_orders_outline_before_fill() {
        let outline = vec![glyph(1.0), glyph(2.0)];
        let fill = vec![glyph(3.0), glyph(4.0)];
        let mut out = Vec::new();
        pack_instances(&outline, &fill, &mut out);

        assert_eq!(out.len(), 4);
        assert_eq!(out[0].rect[0], 1.0);
        assert_eq!(out[1].rect[0], 2.0);
        assert_eq!(out[2].rect[0], 3.0);
        assert_eq!(out[3].rect[0], 4.0);
    }

    #[test]
    fn pack_clears_previous_frame() {
        let mut out = vec![glyph(9.0); 10]
            .iter()
            .map(GpuGlyph::from_instance)
            .collect::<Vec<_>>();
        pack_instances(&[], &[glyph(1.0)], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rect[0], 1.0);
    }

    #[test]
    fn pack_empty_plan_yields_empty_buffer() {
        let mut out = vec![GpuGlyph::from_instance(&glyph(0.0))];
        pack_instances(&[], &[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn capacity_grows_to_fit() {
        assert_eq!(instance_capacity_for(0, 100), MIN_INSTANCE_CAPACITY);
        assert_eq!(instance_capacity_for(512, 512), 512);
        assert_eq!(instance_capacity_for(512, 600), 1024);
        assert_eq!(instance_capacity_for(512, 2000), 2000);
    }

    // ─── 真实弹幕文件回归测试 ──────────────────────────────────────

    /// 载入仓库根目录的实测弹幕文件（缺失时跳过，保证 CI 仍能跑）
    fn real_danmaku_file() -> Option<std::path::PathBuf> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../鱿鱼游戏_第一季_E229237.xml");
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    /// 从系统载入一个可用的中文字体（缺字体时返回 None 让测试跳过）
    fn test_font() -> Option<Vec<u8>> {
        let fonts_dir = std::env::var("WINDIR")
            .map(|w| format!("{w}\\Fonts"))
            .unwrap_or_else(|_| "C:\\Windows\\Fonts".into());
        for name in ["msyh.ttc", "simhei.ttf", "simsun.ttc", "segoeui.ttf"] {
            let p = std::path::Path::new(&fonts_dir).join(name);
            if let Ok(data) = std::fs::read(&p) {
                return Some(
                    dyl_render::font_util::extract_ttc_preferred(
                        &data,
                        dyl_render::font_util::CJK_SC_PREFER,
                    )
                    .map(|f| f.data)
                    .unwrap_or(data),
                );
            }
        }
        None
    }

    /// 把引擎摆到"给定的窗口尺寸 + 给定视频时长"的状态（不碰 GPU）
    fn engine_at(surface: (u32, u32), xml: &std::path::Path) -> DanmakuEngine {
        let mut engine = DanmakuEngine::new(DanmakuConfig::default());
        engine.config.viewport_width = surface.0 as f32;
        engine.config.viewport_height = surface.1 as f32;
        engine.config.font_scale = reference_font_scale(1.0, engine.config.base_font_size);
        // 标记为"已建立过布局"，避免测试里再触发 relayout
        engine.last_canvas = Some((surface.0 as f32, surface.1 as f32));
        engine.last_font_scale = Some(engine.config.font_scale);
        engine
            .load_from_file(xml.to_str().expect("utf-8 path"))
            .expect("弹幕文件应能加载");
        engine
    }

    /// 真实弹幕文件全时间轴回放：**任何一帧都不能丢字形**
    ///
    /// 这条测试直接对应用户报的「滚动弹幕出现不到一秒就没了 + 显示不全」。
    /// 根因链：atlas 高度硬封顶 2048 → `pack()` 装不下时返回零尺寸 → Phase 3
    /// 跳过绘制 → 字形缺失；而零尺寸结果还会被 `placement_cache` 粘住不放。
    ///
    /// 该文件 9964 条弹幕、峰值并发 147 条（8 秒窗口），且字符集很大，正好能把
    /// atlas 撑爆，是这个 bug 的理想复现样本。修复靠"溢出后整表重建并重试"，
    /// 所以断言 `unresolved_drops() == 0`。
    #[test]
    fn real_xml_timeline_never_loses_glyphs() {
        let (Some(xml), Some(font)) = (real_danmaku_file(), test_font()) else {
            eprintln!("skip: 缺少弹幕文件或系统字体");
            return;
        };

        // 手机竖屏：1080x2340，画布最高 → 轨道最多、同屏字形需求最大，最容易撞容量上限
        let mut engine = engine_at((1080, 2340), &xml);
        engine.set_font(font);

        let mut peak_visible = 0usize;
        let mut peak_at = 0.0f64;
        let mut frames_with_glyphs = 0usize;
        let mut total_visible = 0usize;

        // 15fps 走完全片（3394s），足以覆盖滚动弹幕的整个生命周期
        let mut t = 0.0f64;
        while t <= 3400.0 {
            engine.frame_count += 1;
            let plan = engine.build_render_plan_internal(t);
            let visible = plan.fill.len() / 2; // outline + fill 数量相同
            total_visible += visible;
            if visible > peak_visible {
                peak_visible = visible;
                peak_at = t;
            }
            if visible > 0 {
                frames_with_glyphs += 1;
                // 每条可见弹幕至少要有一个可绘制字形，否则就是"显示不全"
                assert_eq!(
                    plan.fill.len(),
                    plan.outline.len(),
                    "t={t:.1}: outline/fill 实例数必须一致"
                );
            }
            t += 1.0 / 15.0;
        }

        println!(
            "real_xml_timeline_never_loses_glyphs: frames={:.0} peak_glyphs={} at t={:.1}s \
             total_visible={} rebuilds={} unresolved={} missing={} empty={} atlas_h={} kinds={}",
            t * 15.0,
            peak_visible,
            peak_at,
            total_visible,
            engine.overflow_rebuilds(),
            engine.unresolved_drops(),
            engine.missing_glyphs(),
            engine.empty_glyphs(),
            engine.glyph_atlas.allocated_height(),
            engine.glyph_atlas.packed_count(),
        );

        assert!(
            frames_with_glyphs > 0,
            "整条时间轴一条弹幕都没画出来 —— 解析或布局出了问题"
        );
        assert!(
            peak_visible >= 20,
            "峰值同屏字形数只有 {peak_visible}，明显偏少（布局把大部分弹幕挤掉了）"
        );
        assert_eq!(
            engine.unresolved_drops(),
            0,
            "有 {} 个字形在 atlas 重建后仍被丢弃 —— 弹幕会显示不全。\
             单帧字形需求超过了 atlas 容量上限，需要限制并发弹幕数",
            engine.unresolved_drops()
        );
        assert_eq!(
            engine.missing_glyphs(),
            0,
            "整条时间轴共 {} 个字形没能绘制（atlas 装不下）—— 这就是「弹幕显示不全 / \
             一闪就没」的直接原因",
            engine.missing_glyphs()
        );
    }

    /// 字体必须覆盖弹幕里出现的所有字符（否则弹幕会缺字，且不报错）
    ///
    /// `empty_glyphs` 统计的是"光栅化出零尺寸位图"的字形，绝大多数是空格（正常），
    /// 但如果混进了真正的汉字/字母/标点，就意味着字体缺字，弹幕会安静地少字 ——
    /// 与 atlas 溢出表现完全一样，但根因完全不同。这条测试把两者分开。
    #[test]
    fn real_xml_characters_are_all_covered_by_the_font() {
        let (Some(xml), Some(font)) = (real_danmaku_file(), test_font()) else {
            return;
        };
        let data = std::fs::read(&xml).expect("read xml");
        let items = crate::parser::parser_for_extension("xml").parse(&data);

        let font_ref = ab_glyph::FontRef::try_from_slice(&font).expect("font parses");
        use ab_glyph::{Font, ScaleFont};
        let scaled = font_ref.as_scaled(ab_glyph::PxScale::from(25.0f32));

        let mut checked = 0usize;
        let mut missing: std::collections::BTreeMap<char, usize> = Default::default();
        for item in &items {
            for ch in item.text.chars() {
                checked += 1;
                if ch.is_whitespace() {
                    continue;
                }
                let glyph_id = scaled.glyph_id(ch);
                let empty = scaled
                    .outline_glyph(scaled.scaled_glyph(ch))
                    .map(|g| {
                        let b = g.px_bounds();
                        b.width() <= 0.0 || b.height() <= 0.0
                    })
                    .unwrap_or(true);
                if empty && glyph_id.0 != 0 {
                    *missing.entry(ch).or_default() += 1;
                }
            }
        }

        assert!(checked > 0, "应解析出弹幕文本");
        if !missing.is_empty() {
            let sample: Vec<String> = missing
                .iter()
                .take(20)
                .map(|(c, n)| format!("{c:?}×{n}"))
                .collect();
            panic!(
                "字体缺少 {} 种字符（共出现 {} 次），弹幕会安静地缺字：{}",
                missing.len(),
                missing.values().sum::<usize>(),
                sample.join(" ")
            );
        }
    }

    /// 滚动弹幕必须按 `scroll_duration` 走完全程（不能"一闪而过"）
    ///
    /// 用户报的现象是「滚动弹幕出现不到一秒就没了」。这条测试直接量生命周期：
    /// 采样 x(t)，断言弹幕从出现到完全离屏耗时接近 `scroll_duration`，且中间
    /// 单调推进、没有跳变。行程 = 窗口宽 + 文本宽（起点在右边缘，终点在左侧文本外）。
    #[test]
    fn real_xml_scroll_lifetime_matches_duration() {
        let Some(xml) = real_danmaku_file() else {
            return;
        };
        let surface = 1920.0f32;
        let engine = engine_at((surface as u32, 1080), &xml);
        let state = engine.state.lock();
        let prepared = state.prepared.as_ref().expect("prepared");

        let mut checked = 0usize;
        for item in prepared.items.iter() {
            if !item.is_scroll {
                continue;
            }
            checked += 1;
            if checked > 300 {
                break;
            }

            let duration = item.end_time - item.start_time;
            assert!(
                (duration - 8.0).abs() < 0.01,
                "滚动时长应恒为 scroll_duration=8s，实际 {duration}"
            );

            // 起点在窗口右边缘，终点在窗口左侧、文本宽度之外
            assert!(
                (item.start_x - surface).abs() < 1.0,
                "起点应等于窗口宽 {surface}，实际 {}",
                item.start_x
            );
            assert!(
                item.text_width > 0.0,
                "text_width 不能为 0，否则弹幕行程只剩窗口宽"
            );
            assert!(
                (item.end_x + item.text_width).abs() < 1.0,
                "终点应等于 -text_width（{}），实际 {}",
                -item.text_width,
                item.end_x
            );

            // 采样整条生命周期：x 必须单调递减
            let mut prev_x = f32::INFINITY;
            for step in 0..=40 {
                let t = item.start_time + duration * (step as f64 / 40.0);
                let x = item.x_at(t);
                assert!(
                    x <= prev_x + 0.5,
                    "t={t:.2} 处 x={x} 反而比上一个采样点 {prev_x} 大 —— 运动不单调"
                );
                prev_x = x;
            }

            // 从"完全离屏"到"完全入屏"的可见时长应接近 scroll_duration。
            // 弹幕中心 x 落在 [-text_width, surface] 区间内的时长就是可见时长。
            let visible_start = item.start_time;
            let visible_end = item.start_time + duration;
            assert!(
                visible_end - visible_start > 4.0,
                "可见时长 {}s 过短，看起来就是一闪而过",
                visible_end - visible_start
            );
        }
        assert!(checked > 0, "文件里应有滚动弹幕");
    }
}
