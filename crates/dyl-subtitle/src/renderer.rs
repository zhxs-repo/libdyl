//! 字幕渲染器 — 实现 Overlay trait，使用 ab_glyph + wgpu 渲染字幕文本
//!
//! ASS 特效路径：解析层保留覆盖标签段（`crates/dyl-subtitle/src/ass.rs`），
//! `update(pts)` 每帧为每个活跃事件求值出 `RenderItem`（位置/字号/颜色/淡入淡出），
//! `render()` 逐 item 光栅化（带缓存）并按锚点+对齐上屏。
//! 纯文本事件（SRT / 内嵌字幕）走相同的 item 管线，锚点为底部居中，
//! 行为与旧实现一致。

use std::num::NonZeroU64;

use crate::ass::{eval_fade, eval_move, Alignment, AssEvent, AssStyle, AssTags};
use crate::fonts::{covers, FontLibrary};
use crate::types::{SubtitleConfig, SubtitleEvent};
use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use dyl_render::{Overlay, OverlayArea};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// 单帧最多渲染的字幕条数（也等于 alpha uniform 的槽位数）
///
/// Danmaku2ASS 风格的滚动字幕同屏常有 20~40 条事件，上限低于实际密度时，
/// 溢出裁剪会把仍在屏上滚动的老字幕整条抽走（表现为"滚到左侧突然消失"）。
/// 取 128 作为容量兜底，正常弹幕密度下不会触发裁剪。
const MAX_ITEMS: usize = 128;
/// uniform 槽位间隔（min_uniform_buffer_offset_alignment）
const UNIFORM_SLOT: u64 = 256;

/// 字幕位图缓存容量（张）
///
/// 一条长台词的光栅化位图可达 2000×2000，缓存必须有界，否则长时间播放会一路
/// 涨到 OOM。超出后按 FIFO 淘汰，本帧仍在用的条目不动。
const TEXTURE_CACHE_CAPACITY: usize = 192;

/// 字幕状态 — 由外部线程写入，渲染线程读取
pub struct SubtitleState {
    /// 字幕事件列表（纯文本视图，SRT/内嵌字幕及诊断用）
    pub events: Vec<SubtitleEvent>,
    /// ASS 事件（含覆盖标签段）；非空时作为渲染数据源
    pub ass_events: Vec<AssEvent>,
    /// ASS 样式表（[V4+ Styles]）
    pub styles: HashMap<String, AssStyle>,
    /// ASS 脚本分辨率（PlayResX/Y）；0 = 回退到 config 画布尺寸
    pub play_res: (f32, f32),
    /// 当前显示的字幕文本（空 = 不显示；兼容旧接口）
    pub current_text: String,
    /// 是否启用
    pub enabled: bool,
    /// 事件源是否为用户显式加载的字幕文件
    ///
    /// 为 true 时忽略内嵌字幕轨的脚本头与实时追加：`select_track` 换字幕轨会重开
    /// 管线并重新喂内嵌事件，两个来源同时写入会叠着显示两套字幕。
    pub external_source: bool,
}

/// 缓存的纹理
struct CachedTexture {
    texture: wgpu::Texture,
    width: u32,
    height: u32,
}

/// 一次光栅化的输入：一段文本连同它自己的样式
///
/// 整条字幕通常共用一份样式（`BlockSpec` 直接来自 [`RenderItem`]），只有事件里真的
/// 出现行内 `{\r样式名}` 时才会按行拆出多个 `BlockSpec`，逐行单独光栅化再拼接。
#[derive(Clone, Debug, PartialEq)]
struct BlockSpec {
    text: String,
    /// 样式表里的 `Fontname`；空 = 用引擎回退字体
    font_name: String,
    font_size: f32,
    /// 样式/覆盖颜色 RGBA（0..1，直通 alpha）
    colour: [f32; 4],
    /// 描边宽度（画布像素，样式 Outline 或 \bord）
    outline: f32,
    /// 阴影深度（画布像素，样式 Shadow 或 \shad）
    shadow: f32,
    /// 描边颜色 RGBA（0..1）
    outline_colour: [f32; 4],
}

impl BlockSpec {
    /// 样式表基线 + 本块生效的覆盖标签（后写赢）→ 外观
    ///
    /// `style.font_size == 0.0` 是"样式表没写"的约定，此时 `\fs` 之外再用 config
    /// 默认字号兜底；`> 0` 时它是作者设定的基线，仍可被 `\fs` 覆盖。
    fn resolve(style: &AssStyle, tags: &AssTags, text: String, default_size: f32) -> Self {
        Self {
            text,
            font_name: style.font_name.clone(),
            font_size: tags
                .font_size
                .or(if style.font_size > 0.0 {
                    Some(style.font_size)
                } else {
                    None
                })
                .unwrap_or(default_size),
            colour: tags.color.unwrap_or(style.primary_colour),
            outline: tags.border.unwrap_or(style.outline),
            shadow: tags.shadow.unwrap_or(style.shadow),
            outline_colour: tags.outline_colour.unwrap_or(style.outline_colour),
        }
    }

    /// 外观是否一致（不含文本）
    ///
    /// 逐行样式全等于整条字幕的外观时走单张位图的老路径，输出与不拆行时逐像素一致。
    fn looks_same(&self, other: &BlockSpec) -> bool {
        self.font_name == other.font_name
            && self.font_size == other.font_size
            && self.colour == other.colour
            && self.outline == other.outline
            && self.shadow == other.shadow
            && self.outline_colour == other.outline_colour
    }
}

/// 一帧中一条字幕的求值结果（画布坐标系）
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RenderItem {
    text: String,
    font_size: f32,
    /// 样式/覆盖颜色 RGBA（0..1，直通 alpha）
    colour: [f32; 4],
    /// 描边宽度（画布像素，样式 Outline 或 \bord）
    outline: f32,
    /// 阴影深度（画布像素，样式 Shadow 或 \shad）
    shadow: f32,
    /// 描边颜色 RGBA（0..1）
    outline_colour: [f32; 4],
    /// \fad 求值得到的整体不透明度乘子（0..1）
    alpha: f32,
    /// 锚点（画布像素）
    anchor: (f32, f32),
    /// 对齐（锚点相对文本框的位置）
    align: Alignment,
    /// 定位所用画布尺寸
    canvas: (f32, f32),
    /// 整条字幕样式的 `Fontname`
    font_name: String,
    /// 逐行样式：事件里出现行内 `{\r样式名}` 时才非空
    ///
    /// 每项是一行的 [`BlockSpec`]（含该行文本），顺序与 `text` 按行拆分后一致。
    /// 空表示整条字幕共用 `font_name/font_size/colour/…`，走单张位图路径。
    lines: Vec<BlockSpec>,
}

impl RenderItem {
    /// 整条字幕共用的光栅化输入
    fn block_spec(&self) -> BlockSpec {
        BlockSpec {
            text: self.text.clone(),
            font_name: self.font_name.clone(),
            font_size: self.font_size,
            colour: self.colour,
            outline: self.outline,
            shadow: self.shadow,
            outline_colour: self.outline_colour,
        }
    }
}

/// 光栅化出来的位图，连同逐行拼接要用的排版量
struct Rasterized {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    /// 位图四周留白（描边/阴影外扩 + 余量），光栅像素
    pad: f32,
    /// 正文最宽一行的实际宽度（不含 pad），用于横向居中对齐
    content_w: f32,
    /// 本块正文占用的高度（`行距 × 行数`，不含 pad），纵向拼接的推进量
    content_h: f32,
}

/// 一行的排版结果：字形序列与总宽
#[derive(Default)]
struct LineRuns {
    glyphs: Vec<GlyphRun>,
    width: f32,
}

/// 一个字形在行内的位置，以及它落在哪一份字体上
///
/// `font_idx` 0 = 样式 `Fontname` 命中的 face，1 = 内置回退字体（主字体缺这个字时）。
#[derive(Clone)]
struct GlyphRun {
    glyph: ab_glyph::Glyph,
    x: f32,
    font_idx: u8,
}

/// 字幕引擎 — 管理字幕数据、解析、渲染
///
/// 实现 Overlay trait，可集成到 OverlayStack 中。
pub struct SubtitleEngine {
    /// 共享状态
    state: Arc<Mutex<SubtitleState>>,
    /// 渲染配置
    config: SubtitleConfig,
    /// 回退字体 + `Fontname` → face 解析（含系统字体索引）
    fonts: FontLibrary,
    /// 缓存的纹理（key = 文本+字号+颜色哈希）
    texture_cache: HashMap<u64, CachedTexture>,
    /// `texture_cache` 的插入顺序（FIFO 近似 LRU）：本帧用到的 key 会被挪到末尾，
    /// 超出容量时从头淘汰
    texture_order: VecDeque<u64>,
    /// 本帧活跃字幕的求值结果（update 写入，render 读取）
    active_items: Vec<RenderItem>,
    /// wgpu 渲染管线（惰性初始化）
    render_pipeline: Option<wgpu::RenderPipeline>,
    bind_group_layout: Option<wgpu::BindGroupLayout>,
    sampler: Option<wgpu::Sampler>,
    surface_format: Option<wgpu::TextureFormat>,
    /// 上述 GPU 资源所属设备代号（见 [dyl_render::next_gpu_epoch]）
    gpu_epoch: Option<u64>,
    /// per-item 淡出 alpha（uniform buffer，MAX_ITEMS 个槽位）
    alpha_buffer: Option<wgpu::Buffer>,
    /// 诊断计数器
    frame_count: u64,
}

// Safety: SubtitleEngine is only accessed from one thread at a time via Overlay
unsafe impl Send for SubtitleEngine {}

impl SubtitleEngine {
    pub fn new(config: SubtitleConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(SubtitleState {
                events: Vec::new(),
                ass_events: Vec::new(),
                styles: HashMap::new(),
                play_res: (0.0, 0.0),
                current_text: String::new(),
                enabled: true,
                external_source: false,
            })),
            config,
            fonts: FontLibrary::default(),
            texture_cache: HashMap::new(),
            texture_order: VecDeque::new(),
            active_items: Vec::new(),
            render_pipeline: None,
            bind_group_layout: None,
            sampler: None,
            surface_format: None,
            gpu_epoch: None,
            alpha_buffer: None,
            frame_count: 0,
        }
    }

    /// 设置字体数据（支持 TTF/OTF/TTC 格式）
    ///
    /// 这是**回退字体**：样式表没写 `Fontname`、或写了但系统里找不到时用它；
    /// 它同时是逐字覆盖兜底 —— 样式点了没有 CJK 字形的拉丁字体时，缺的字回到这里。
    pub fn set_font(&mut self, font_data: Vec<u8>) {
        self.fonts.set_fallback(font_data);
        self.texture_cache.clear();
        self.texture_order.clear();
    }

    /// 预热系统字体索引
    ///
    /// 扫一遍系统字体目录要几十到几百毫秒，必须由加载线程调，不能留到渲染线程
    /// 第一次解析 `Fontname` 时才做。
    pub fn prepare_system_fonts(&mut self) {
        self.fonts.prepare_system_fonts();
    }

    /// 获取共享状态句柄
    pub fn shared_state(&self) -> Arc<Mutex<SubtitleState>> {
        self.state.clone()
    }

    /// 加载字幕文件（.ass/.ssa 保留样式表与特效标签；其余按纯文本事件）
    pub fn load_from_file(&self, path: &str) -> dyl_core::Result<()> {
        let data = std::fs::read(path)
            .map_err(|e| dyl_core::DylError::FileNotFound(format!("{}: {}", path, e)))?;

        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("srt");

        let mut state = self.state.lock();
        if ext.eq_ignore_ascii_case("ass") || ext.eq_ignore_ascii_case("ssa") {
            let parser = crate::parser::AssParser::new();
            let doc = parser.parse_document(&data);
            tracing::info!(
                "SubtitleEngine: loaded {} ASS events ({} styles, play_res={:.0}x{:.0}) from {}",
                doc.events.len(),
                doc.styles.len(),
                doc.play_res_x,
                doc.play_res_y,
                path
            );
            state.events = doc.events.iter().map(|e| e.base.clone()).collect();
            state.ass_events = doc.events;
            state.styles = doc.styles;
            state.play_res = (doc.play_res_x, doc.play_res_y);
        } else {
            let parser = crate::parser::parser_for_extension(ext);
            let events = parser.parse(&data);
            tracing::info!(
                "SubtitleEngine: loaded {} events from {}",
                events.len(),
                path
            );
            state.ass_events = events.iter().cloned().map(AssEvent::plain).collect();
            state.events = events;
            state.styles = HashMap::new();
            state.play_res = (0.0, 0.0);
        }
        state.external_source = true;
        Ok(())
    }

    /// 从字幕事件列表加载（纯文本，无特效）
    pub fn load_events(&self, events: Vec<SubtitleEvent>) {
        let mut state = self.state.lock();
        state.ass_events = events.iter().cloned().map(AssEvent::plain).collect();
        state.events = events;
        state.styles = HashMap::new();
        state.play_res = (0.0, 0.0);
        state.external_source = false;
    }

    /// 追加字幕事件（不清空已有内容），供纯文本内嵌字幕边解码边注入使用
    pub fn append_events(&self, events: Vec<SubtitleEvent>) {
        self.append_ass_events(events.into_iter().map(AssEvent::plain).collect());
    }

    /// 追加带覆盖标签段的 ASS 事件（内嵌 ASS/SSA 轨走这条路）
    ///
    /// `state.events` 同步写入纯文本视图：HUD 与只读文本的消费方看它，渲染端只读
    /// `ass_events`。两者必须一起增长，否则只读视图会漏掉正在显示的那条。
    pub fn append_ass_events(&self, events: Vec<AssEvent>) {
        if events.is_empty() {
            return;
        }
        let mut state = self.state.lock();
        if state.external_source {
            tracing::debug!("subtitle: 忽略内嵌轨追加（用户已加载外部字幕文件）");
            return;
        }
        for ev in &events {
            state.events.push(ev.base.clone());
        }
        state.ass_events.extend(events);
    }

    /// 安装容器内封字幕流的脚本头（CodecPrivate 里的 ASS 脚本）
    ///
    /// 只取样式表与 PlayResX/Y：脚本里的事件时间轴是相对脚本自身写的，与容器
    /// 时间轴无关，真正的事件由 [`append_ass_events`] 边解码边喂进来。
    /// 有了样式表，`Default`/`银色子弹思源体简` 这类轨内样式名才会命中，
    /// 字号、颜色、描边、对齐与边距才是作者设定的值而不是 config 默认。
    ///
    /// 返回是否装上了样式（外部字幕文件生效期间、或脚本没有样式时不装）。
    pub fn apply_embedded_script(&self, data: &[u8]) -> bool {
        let doc = crate::parser::AssParser::new().parse_document(data);
        if doc.styles.is_empty() {
            tracing::debug!("subtitle: 内嵌脚本头无样式表，沿用默认样式");
            return false;
        }
        let mut state = self.state.lock();
        if state.external_source {
            tracing::debug!("subtitle: 内嵌脚本头不覆盖外部字幕样式表");
            return false;
        }
        tracing::info!(
            "SubtitleEngine: 内嵌字幕样式表安装 {} 条 (play_res={:.0}x{:.0})",
            doc.styles.len(),
            doc.play_res_x,
            doc.play_res_y
        );
        state.styles = doc.styles;
        state.play_res = (doc.play_res_x, doc.play_res_y);
        true
    }

    /// config 派生的默认样式（无 ASS 样式表时使用，行为与旧实现一致）
    fn config_default_style(&self) -> AssStyle {
        let c = self.config.default_color;
        AssStyle {
            name: "Default".to_string(),
            primary_colour: [
                ((c >> 16) & 0xFF) as f32 / 255.0,
                ((c >> 8) & 0xFF) as f32 / 255.0,
                (c & 0xFF) as f32 / 255.0,
                1.0,
            ],
            ..AssStyle::default()
        }
    }

    /// 合并样式表 + 覆盖标签 + 边距，在 pts 时刻求值出一条字幕的渲染参数
    fn make_item(&self, ev: &AssEvent, state: &SubtitleState, pts: f64) -> Option<RenderItem> {
        // 折叠所有段的标签（后写赢）
        let mut tags = AssTags::default();
        for (seg_tags, _) in &ev.segments {
            tags.apply_overlay(seg_tags);
        }

        let text = if ev.segments.is_empty() {
            ev.base.text.clone()
        } else {
            let joined: String = ev.segments.iter().map(|(_, t)| t.as_str()).collect();
            crate::types::normalize_whitespace(&joined.replace("\\N", "\n").replace("\\n", "\n"))
        };
        if text.trim().is_empty() {
            return None;
        }

        let file_style = state.styles.get(&ev.base.style);
        let default = self.config_default_style();
        let style = file_style.unwrap_or(&default);

        // 整条字幕的外观：样式表 + 折叠后的覆盖标签
        let default_size = self.config.default_font_size as f32;
        let block = BlockSpec::resolve(style, &tags, String::new(), default_size);
        let align = tags.alignment.unwrap_or(style.alignment);

        // 行内 `{\r样式名}`：按行确定样式，外观真的不同的行才逐行光栅化
        //
        // `{\r}`（不带名字）按 ASS 语义回到脚本里名为 Default 的样式；表里没有
        // 就回到事件自己的样式。
        let reset_target = state
            .styles
            .keys()
            .find(|name| name.eq_ignore_ascii_case("Default"))
            .map(String::as_str)
            .unwrap_or(ev.base.style.as_str());
        let lines: Vec<BlockSpec> = ev
            .lines(reset_target)
            .into_iter()
            .map(|line| {
                // 行样式查不到时退回事件自己的样式（与整条字幕的解析一致）
                let line_style = state.styles.get(&line.style).unwrap_or(style);
                BlockSpec::resolve(line_style, &line.tags, line.text, default_size)
            })
            .collect();
        // 各行外观与整条字幕一致 → 留空，走单张位图的老路径（逐像素不变）
        let mixed = lines.len() > 1 && !lines.iter().all(|line| line.looks_same(&block));

        // 边距：事件字段 > 样式 > config 默认（0 视为未设置）
        let pick_margin = |ev_m: f32, st_m: f32, cfg_default: f32| -> f32 {
            if ev_m > 0.0 {
                ev_m
            } else if st_m > 0.0 {
                st_m
            } else {
                cfg_default
            }
        };
        let canvas = if state.play_res.0 > 0.0 && state.play_res.1 > 0.0 {
            state.play_res
        } else {
            (
                self.config.canvas_width as f32,
                self.config.canvas_height as f32,
            )
        };
        let margin_l = pick_margin(ev.margin_l, style.margin_l, 0.0);
        let margin_r = pick_margin(ev.margin_r, style.margin_r, 0.0);
        let margin_v = pick_margin(
            ev.margin_v,
            style.margin_v,
            self.config.bottom_margin as f32,
        );

        // 锚点：\move 插值 > \pos > 对齐+边距推出的默认锚点
        let anchor = if let Some(mv) = tags.move_ {
            eval_move(&mv, pts, ev.base.start_time, ev.base.end_time)
        } else if let Some(pos) = tags.pos {
            pos
        } else {
            let (w, h) = canvas;
            let x = match align.h() {
                0 => margin_l,
                1 => w / 2.0,
                _ => w - margin_r,
            };
            let y = match align.v() {
                0 => h - margin_v,
                1 => h / 2.0,
                _ => margin_v,
            };
            (x, y)
        };

        let alpha = tags
            .fade
            .map(|f| eval_fade(f, pts, ev.base.start_time, ev.base.end_time))
            .unwrap_or(1.0);

        Some(RenderItem {
            text,
            font_size: block.font_size,
            colour: block.colour,
            outline: block.outline,
            shadow: block.shadow,
            outline_colour: block.outline_colour,
            alpha,
            anchor,
            align,
            canvas,
            font_name: block.font_name,
            lines: if mixed { lines } else { Vec::new() },
        })
    }

    /// 确保渲染管线就绪
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
            // 设备换了（Surface 重建）：旧设备上的纹理随设备一起作废。不清掉的话
            // 下面会命中 `texture_cache`，把旧纹理绑进新设备的 bind group 里。
            self.texture_cache.clear();
            self.texture_order.clear();
        }

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("subtitle_overlay"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader/overlay.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("subtitle_bg_layout"),
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
                        // 与 shader 中 vec4<f32> 对齐（16 字节）
                        min_binding_size: NonZeroU64::new(16),
                    },
                    count: None,
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("subtitle_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let alpha_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("subtitle_alpha"),
            size: (MAX_ITEMS as u64) * UNIFORM_SLOT,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("subtitle_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("subtitle_render_pipeline"),
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
        self.alpha_buffer = Some(alpha_buffer);
        self.surface_format = Some(surface_format);
        self.gpu_epoch = Some(gpu_epoch);
    }

    /// 缓存 key：文本 + 外观 + 字体名 + 画布尺寸 + 字号系数
    ///
    /// 最后一项是**光栅化用的字号系数**（内容基准，见 [OverlayArea::content_scale]）：
    /// 同样一句话在不同系数下光栅出的位图尺寸不同，所以必须参与。
    ///
    /// 字体名与逐行外观也必须参与：换 face 就是换一张位图，否则样式改了 `Fontname`
    /// 还会命中旧字体的纹理；行内 `{\r样式名}` 时每行各自成图，拼出来的位图同样不同。
    ///
    /// 刻意**不含** surface 尺寸：字号改成内容基准后，位图只跟视频原生分辨率有关，
    /// 拖窗口 / 旋转屏幕 / 分屏都不会让这句话的位图失效，缓存因此可以跨 resize 复用。
    fn item_hash(item: &RenderItem, font_scale: f32) -> u64 {
        let mut hash: u64 = 5381;
        for byte in item.text.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
        }
        for bits in [
            item.canvas.0.to_bits(),
            item.canvas.1.to_bits(),
            font_scale.to_bits(),
        ] {
            hash = hash.wrapping_mul(31).wrapping_add(bits as u64);
        }
        mix_spec(
            &mut hash,
            &BlockSpec {
                text: String::new(),
                font_name: item.font_name.clone(),
                font_size: item.font_size,
                colour: item.colour,
                outline: item.outline,
                shadow: item.shadow,
                outline_colour: item.outline_colour,
            },
        );
        for line in &item.lines {
            mix_spec(&mut hash, line);
        }
        hash
    }

    /// 光栅化一条字幕为 RGBA 位图（指定字号系数与纹理边长上限）
    ///
    /// **单样式**（`item.lines` 为空）时整条字幕一次成图：所有行共用一张覆盖率掩码，
    /// 与拆分之前的老实现逐像素一致。**行内 `{\r样式名}`** 时每行是一块，各自按自己
    /// 的字体/字号/描边/颜色成图，再纵向拼成一张位图 —— 仍然只占一个 viewport、
    /// 一次 draw，GPU 侧看不出区别。
    ///
    /// [max_dim] 是设备允许的最大纹理边长（`max_texture_dimension_2d`）。
    /// 字幕字号来自 ASS 的 `\fs`，不受播放器控制：一条长台词在 1920x1080 的
    /// PlayRes 上按 fs=40 排出来轻易就是 3500+ 像素宽，而 GLES 后端的
    /// downlevel 上限只有 2048。硬建纹理会被 wgpu 判为 validation 错误，
    /// 进而让整帧的 bind group / submit 全部失效（黑屏，历史上直接 abort）。
    ///
    /// 所以先按可用边长反推一个缩放系数再光栅化。文字仍然按锚点落在画面矩形里
    /// 该落的位置（`render()` 用位置映射算 viewport），只是采样分辨率降低 ——
    /// 糊一点，远好过整条字幕画不出来。
    ///
    /// 描边/阴影在 CPU 光栅化阶段完成（纹理本身就是 RGBA，颜色已烘焙）：
    /// 先写覆盖率掩码，再按到掩码的欧氏距离把字形外扩 `outline` px（描边）
    /// 与再外扩 `shadow` px（阴影），按 阴影→描边→正文 的顺序 over 合成。
    /// 没有描边/阴影时走单层快速路径，与旧行为逐字节一致。
    fn rasterize_text(
        &mut self,
        item: &RenderItem,
        font_scale: f32,
        max_dim: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let blocks: Vec<BlockSpec> = if item.lines.is_empty() {
            vec![item.block_spec()]
        } else {
            item.lines.clone()
        };
        // 预算：位图（含描边外扩与四周 padding）必须落在纹理边长上限内
        let budget = max_dim.saturating_sub(8).max(1) as f32;

        // 逐行拼接时整张画布可能比任何单块都高，超限就整体降一档重量一次。
        // 常规字幕第一趟就满足，只有极端长句/超多行才会重画。
        let mut scale = font_scale;
        let mut parts: Vec<Rasterized> = Vec::with_capacity(blocks.len());
        for _ in 0..3 {
            parts.clear();
            for block in &blocks {
                // `None` = 没有可用字体：整条字幕放弃（与单块时的老行为一致）
                let part = self.raster_block(block, scale, budget, max_dim)?;
                parts.push(part);
            }
            let (w, h) = composite_size(&parts);
            if w as f32 <= budget && h as f32 <= budget {
                break;
            }
            scale *= budget / (w.max(h)) as f32;
        }

        if parts.len() == 1 {
            let one = parts.pop().expect("len 1");
            return Some((one.pixels, one.width, one.height));
        }
        Some(composite(&parts, max_dim))
    }

    /// 本块用哪个 face：`Fontname` 命中就用它，否则用内置回退字体
    ///
    /// 第二项是**逐字兜底**：样式点了没有 CJK 字形的拉丁字体时，缺的那些字回到
    /// 回退字体，而不是整行变成豆腐块。没按 `Fontname` 换 face 时不需要兜底。
    fn block_fonts(&mut self, block: &BlockSpec) -> Option<(FontArc, Option<FontArc>)> {
        let styled = self.fonts.face(&block.font_name);
        let main = match &styled {
            Some(face) => face.clone(),
            None => self.fonts.fallback().cloned()?,
        };
        let alt = if styled.is_some() {
            self.fonts.fallback().cloned()
        } else {
            None
        };
        Some((main, alt))
    }

    /// 一个块的光栅化：同字体的多行画进同一张掩码（与老实现逐像素一致）
    fn raster_block(
        &mut self,
        block: &BlockSpec,
        font_scale: f32,
        budget: f32,
        max_dim: u32,
    ) -> Option<Rasterized> {
        let (main, alt) = self.block_fonts(block)?;
        let lines: Vec<&str> = block.text.lines().collect();
        if lines.is_empty() {
            return None;
        }

        // ASS 的 \fs 是画布像素，乘上**内容基准**系数才是实际绘制像素。
        // 这个系数只跟视频原生分辨率有关，所以位图在拖窗口、旋转、分屏时都不会
        // 失效 —— 缓存 key 里也只放它，不放 surface 尺寸。
        let px_size = (block.font_size * font_scale).max(1.0);
        // 没有兜底字体时就拿主字体占位，逐字符只切索引，不额外加一条分支
        let alt_font: &FontArc = alt.as_ref().unwrap_or(&main);

        // 边缘尺寸（光栅像素）：描边/阴影宽度与字号同源缩放
        let edge_sizes = |scale_px: f32| -> (f32, f32) {
            let k = scale_px / block.font_size.max(1.0);
            (
                (block.outline * k).clamp(0.0, MAX_EDGE_PX),
                (block.shadow * k).clamp(0.0, MAX_EDGE_PX),
            )
        };
        let pad_for = |scale_px: f32| -> f32 {
            let (b, s) = edge_sizes(scale_px);
            (b + s).ceil() + 4.0
        };

        // 量一次：行距、最宽行、逐字形序列（含每个字落到哪一份字体上）
        let measure = |scale_px: f32| -> (f32, f32, Vec<LineRuns>) {
            let main_scaled = main.as_scaled(PxScale::from(scale_px));
            let alt_scaled = alt_font.as_scaled(PxScale::from(scale_px));
            let line_height = main_scaled.ascent() - main_scaled.descent() + 4.0;
            let mut max_width = 0.0f32;
            let mut runs: Vec<LineRuns> = Vec::new();
            for line in &lines {
                let mut line_runs = LineRuns::default();
                for ch in line.chars() {
                    // 主字体画不出这个字（GlyphId(0) = .notdef 豆腐块）就用兜底字体
                    let idx = if !covers(&main, ch) && covers(alt_font, ch) {
                        1
                    } else {
                        0
                    };
                    let scaled = if idx == 0 { &main_scaled } else { &alt_scaled };
                    let glyph = scaled.scaled_glyph(ch);
                    let advance = scaled.h_advance(glyph.id);
                    line_runs.glyphs.push(GlyphRun {
                        glyph,
                        x: line_runs.width,
                        font_idx: idx,
                    });
                    line_runs.width += advance;
                }
                max_width = max_width.max(line_runs.width);
                runs.push(line_runs);
            }
            (line_height, max_width, runs)
        };

        // 先按目标字号量一次，只有超限才回退（避免误伤正常字号）
        let (line_height, max_width, runs) = measure(px_size);
        let pad = pad_for(px_size);
        let need_w = max_width + 2.0 * pad;
        let need_h = line_height * lines.len() as f32 + 2.0 * pad;
        let overshoot = (need_w.max(need_h) / budget).max(1.0);
        let (line_height, max_width, runs, px_final) = if overshoot > 1.0 {
            let px = px_size / overshoot;
            let (lh, mw, runs) = measure(px);
            (lh, mw, runs, px)
        } else {
            (line_height, max_width, runs, px_size)
        };
        let pad = pad_for(px_final);
        let (border_px, shadow_px) = edge_sizes(px_final);

        let scaled = [
            main.as_scaled(PxScale::from(px_final)),
            alt_font.as_scaled(PxScale::from(px_final)),
        ];
        let ascent = scaled[0].ascent();
        let width_px = (max_width.ceil() as u32 + (2.0 * pad).ceil() as u32)
            .min(max_dim)
            .max(1);
        let height_px = (((line_height * lines.len() as f32).ceil() as u32)
            + (2.0 * pad).ceil() as u32)
            .min(max_dim)
            .max(1);

        // ── 覆盖率掩码 ────────────────────────────────────────────
        let mut cover = vec![0f32; (width_px * height_px) as usize];
        for (line_idx, line_runs) in runs.iter().enumerate() {
            let line_y = pad + line_idx as f32 * line_height + ascent;
            // 行内水平居中（位图宽度含左右 padding，居中基准仍是整张位图）
            let line_offset_x = (width_px as f32 - line_runs.width) / 2.0;

            for run in &line_runs.glyphs {
                let font = &scaled[run.font_idx as usize];
                if let Some(outlined) = font.outline_glyph(run.glyph.clone()) {
                    let bounds = outlined.px_bounds();
                    let base_x = line_offset_x + run.x;
                    outlined.draw(|gx, gy, coverage| {
                        // bbox.min.y 为负（基线上方），必须加回而非减去；
                        // 先做浮点边界检查，避免负值 `as u32` 饱和成 0 写错行
                        let fx = base_x + gx as f32;
                        let fy = line_y + bounds.min.y + gy as f32;
                        if fx < 0.0 || fy < 0.0 || fx >= width_px as f32 || fy >= height_px as f32 {
                            return;
                        }
                        let (px, py) = (fx as u32, fy as u32);
                        let cell = (py * width_px + px) as usize;
                        cover[cell] = cover[cell].max(coverage);
                    });
                }
            }
        }

        // ── 合成为 RGBA（直通 alpha）──────────────────────────────
        let pixels = if border_px < 0.5 && shadow_px < 0.5 {
            // 快速路径：无描边/阴影，单层
            let r = (block.colour[0] * 255.0) as u8;
            let g = (block.colour[1] * 255.0) as u8;
            let b = (block.colour[2] * 255.0) as u8;
            let mut px = vec![0u8; cover.len() * 4];
            for (i, &a) in cover.iter().enumerate() {
                let idx = i * 4;
                px[idx] = r;
                px[idx + 1] = g;
                px[idx + 2] = b;
                px[idx + 3] = (a * 255.0) as u8;
            }
            px
        } else {
            // 三层：阴影（d ≤ b+s）→ 描边（d ≤ b）→ 正文覆盖率
            // d 为到字形内部的欧氏距离；1px 线性边缘做抗锯齿
            let d_sq = edt_sq(&cover, width_px as usize, height_px as usize);
            let oc = block.outline_colour;
            let sr = oc[0];
            let sg = oc[1];
            let sb = oc[2];
            let sa_base = oc[3];
            let fr = block.colour[0];
            let fg = block.colour[1];
            let fb = block.colour[2];

            let mut px = vec![0u8; cover.len() * 4];
            for (i, &a_fill) in cover.iter().enumerate() {
                let d = d_sq[i].sqrt();
                let a_shadow = if shadow_px > 0.5 {
                    (border_px + shadow_px + 0.5 - d).clamp(0.0, 1.0) * sa_base
                } else {
                    0.0
                };
                let a_border = (border_px + 0.5 - d).clamp(0.0, 1.0) * sa_base;

                // source-over 累加（预乘），最后还原直通 alpha。
                // 正文 alpha 不乘 colour[3]：与快速路径一致，整体透明度由
                // uniform 的 `alpha * colour[3]` 统一施加，乘两次会偏暗。
                let layers = [
                    ([sr, sg, sb], a_shadow),
                    ([sr, sg, sb], a_border),
                    ([fr, fg, fb], a_fill),
                ];
                let (mut pr, mut pg, mut pb, mut pa) = (0f32, 0f32, 0f32, 0f32);
                for (c, a) in layers {
                    pr = c[0] * a + pr * (1.0 - a);
                    pg = c[1] * a + pg * (1.0 - a);
                    pb = c[2] * a + pb * (1.0 - a);
                    pa = a + pa * (1.0 - a);
                }
                let idx = i * 4;
                if pa > 1.0 / 255.0 / 2.0 {
                    let inv = 1.0 / pa;
                    px[idx] = (pr * inv * 255.0).clamp(0.0, 255.0) as u8;
                    px[idx + 1] = (pg * inv * 255.0).clamp(0.0, 255.0) as u8;
                    px[idx + 2] = (pb * inv * 255.0).clamp(0.0, 255.0) as u8;
                    px[idx + 3] = (pa * 255.0).clamp(0.0, 255.0) as u8;
                }
            }
            px
        };

        Some(Rasterized {
            pixels,
            width: width_px,
            height: height_px,
            pad,
            content_w: max_width,
            content_h: line_height * lines.len() as f32,
        })
    }
}

/// 把一个块的外观折进哈希（文本单独计）
///
/// alpha 通道不参与：整体透明度走 uniform，不烘焙进位图，改了也不影响纹理。
fn mix_spec(hash: &mut u64, spec: &BlockSpec) {
    for byte in spec.font_name.bytes() {
        *hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
    }
    for bits in [
        spec.font_size.to_bits(),
        spec.colour[0].to_bits(),
        spec.colour[1].to_bits(),
        spec.colour[2].to_bits(),
        spec.outline.to_bits(),
        spec.shadow.to_bits(),
        spec.outline_colour[0].to_bits(),
        spec.outline_colour[1].to_bits(),
        spec.outline_colour[2].to_bits(),
    ] {
        *hash = hash.wrapping_mul(31).wrapping_add(bits as u64);
    }
}

/// 逐行拼接后的画布尺寸
///
/// 公式与单块时逐项对齐（`ceil(最宽内容) + ceil(2·pad)`），所以单块时
/// `composite` 根本不会被调用，两趟的结果也一致。
fn composite_size(parts: &[Rasterized]) -> (u32, u32) {
    let pad = parts.iter().map(|p| p.pad).fold(0.0f32, f32::max);
    let max_w = parts.iter().map(|p| p.content_w).fold(0.0f32, f32::max);
    let sum_h: f32 = parts.iter().map(|p| p.content_h).sum();
    (
        max_w.ceil() as u32 + (2.0 * pad).ceil() as u32,
        sum_h.ceil() as u32 + (2.0 * pad).ceil() as u32,
    )
}

/// 把逐行成图的块纵向拼成一张位图
///
/// 每块带自己的四周留白，所以相邻块的外扩区会重叠（描边压到上一行的留白里）；
/// 重叠处按 source-over 混合，而不是取最大值 —— 两块的颜色/描边不同，最大值会把
/// 描边颜色和正文颜色混成一个不该存在的边。
fn composite(parts: &[Rasterized], max_dim: u32) -> (Vec<u8>, u32, u32) {
    let (w, h) = composite_size(parts);
    let (w, h) = (w.min(max_dim).max(1), h.min(max_dim).max(1));
    let pad = parts.iter().map(|p| p.pad).fold(0.0f32, f32::max);
    let mut canvas = vec![0u8; (w * h) as usize * 4];

    let mut top = 0.0f32;
    for part in parts {
        // 本块正文顶部要落在 pad + 已累计行高 处，而块内正文从它自己的 pad 开始
        let y0 = (pad + top - part.pad).max(0.0) as u32;
        let x0 = (w - part.width.min(w)) / 2;
        for sy in 0..part.height {
            let dy = y0 + sy;
            if dy >= h {
                break;
            }
            for sx in 0..part.width {
                let dx = x0 + sx;
                if dx >= w {
                    break;
                }
                let s = ((sy * part.width + sx) * 4) as usize;
                let a_src = part.pixels[s + 3] as f32 / 255.0;
                if a_src == 0.0 {
                    continue;
                }
                let d = ((dy * w + dx) * 4) as usize;
                let a_dst = canvas[d + 3] as f32 / 255.0;
                let a_out = a_src + a_dst * (1.0 - a_src);
                if a_out <= 0.0 {
                    continue;
                }
                let keep = 1.0 - a_src;
                for c in 0..3 {
                    let v = (part.pixels[s + c] as f32 * a_src
                        + canvas[d + c] as f32 * a_dst * keep)
                        / a_out;
                    canvas[d + c] = v.clamp(0.0, 255.0) as u8;
                }
                canvas[d + 3] = (a_out * 255.0).clamp(0.0, 255.0) as u8;
            }
        }
        top += part.content_h;
    }

    (canvas, w, h)
}

/// 描边/阴影的外扩上限（光栅像素），防极端 `\bord` 把位图撑爆
const MAX_EDGE_PX: f32 = 64.0;

/// Felzenszwalb–Huttenlocher 精确欧氏距离变换（平方距离）
///
/// 输入覆盖率掩码（>=0.5 视为字形内部），输出每个像素到最近内部像素的**平方**
/// 距离。两趟 1D DT（先列后行），O(w·h)。空掩码时全部返回 `f32::INFINITY`。
fn edt_sq(cover: &[f32], w: usize, h: usize) -> Vec<f32> {
    const INF: f32 = 1e20;
    let big = w.max(h).max(1);
    let mut f = vec![INF; big];
    let mut d = vec![0f32; big];
    let mut col = vec![INF; w * h];

    // 列方向：每个 x 的竖直距离²
    for x in 0..w {
        for y in 0..h {
            f[y] = if cover[y * w + x] >= 0.5 { 0.0 } else { INF };
        }
        dt1d(&f[..h], &mut d[..h]);
        for y in 0..h {
            col[y * w + x] = d[y];
        }
    }

    // 行方向：把列距离当作输入
    let mut out = vec![INF; w * h];
    for y in 0..h {
        for x in 0..w {
            f[x] = col[y * w + x];
        }
        dt1d(&f[..w], &mut d[..w]);
        for x in 0..w {
            out[y * w + x] = d[x];
        }
    }
    out
}

/// 一维平方距离变换：d[q] = min_p ( f[p] + (q-p)² )
fn dt1d(f: &[f32], d: &mut [f32]) {
    const INF: f32 = 1e20;
    let n = f.len();
    if n == 0 {
        return;
    }
    let sq = |x: usize| (x * x) as f32;
    let intersect =
        |p: usize, q: usize| ((f[q] + sq(q)) - (f[p] + sq(p))) / (2.0 * q as f32 - 2.0 * p as f32);

    let mut v = vec![0usize; n];
    let mut z = vec![-INF; n + 1];
    let mut k = 0usize;
    z[0] = -INF;
    z[1] = INF;
    for q in 1..n {
        let mut s = intersect(v[k], q);
        while s <= z[k] {
            if k == 0 {
                break;
            }
            k -= 1;
            s = intersect(v[k], q);
        }
        if k == 0 && s <= z[0] {
            // 旧抛物线处处更差（f 全 INF 段）：直接替换顶点
            v[0] = q;
            z[1] = INF;
            continue;
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = INF;
    }

    let mut k = 0usize;
    for (q, dq) in d.iter_mut().enumerate().take(n) {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let diff = q as f32 - v[k] as f32;
        *dq = diff * diff + f[v[k]];
    }
}

impl Overlay for SubtitleEngine {
    fn update(&mut self, pts: f64) {
        let state = self.state.lock();
        if !state.enabled {
            self.active_items.clear();
            return;
        }

        // 数据源：ASS 事件优先（含特效）；无 ASS 数据时回退到直接写入的 events
        // （外部测试会绕过 load_* 直接赋值 state.events）
        let fallback: Vec<AssEvent>;
        let source: &[AssEvent] = if state.ass_events.is_empty() {
            fallback = state
                .events
                .iter()
                .filter(|e| pts >= e.start_time && pts < e.end_time)
                .cloned()
                .map(AssEvent::plain)
                .collect();
            &fallback
        } else {
            &state.ass_events
        };

        let mut refs: Vec<&AssEvent> = source
            .iter()
            .filter(|e| {
                pts >= e.base.start_time && pts < e.base.end_time && !e.base.text.trim().is_empty()
            })
            .collect();

        // layer 升序：高 layer 后绘制（压在上层）。同层按开始时间倒序：
        // 超出 MAX_ITEMS 时优先保留最新事件——最老的滚动字幕已接近出画，
        // 丢弃它们比把新事件整个挤掉（等旧批滚完才上场、一上场就在终点）好得多。
        refs.sort_by(|a, b| {
            a.layer
                .cmp(&b.layer)
                .then(b.base.start_time.total_cmp(&a.base.start_time))
        });

        let items: Vec<RenderItem> = refs
            .iter()
            .take(MAX_ITEMS)
            .filter_map(|e| self.make_item(e, &state, pts))
            .collect();

        // 兼容旧接口：拼接纯文本
        let new_text = if refs.is_empty() {
            String::new()
        } else {
            refs.iter()
                .map(|e| e.base.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };

        if self.frame_count.is_multiple_of(300) {
            let preview: String = new_text.chars().take(40).collect();
            tracing::info!(
                "SubtitleEngine::update: frame={} pts={:.2}s events={} active={} text={:?}",
                self.frame_count,
                pts,
                source.len(),
                items.len(),
                preview
            );
        }
        self.frame_count += 1;

        self.active_items = items;
        drop(state);
        let mut state = self.state.lock();
        state.current_text = new_text;
    }

    fn render(
        &mut self,
        frame: &dyl_render::OverlayFrame<'_>,
        encoder: &mut wgpu::CommandEncoder,
        area: OverlayArea,
    ) {
        if self.active_items.is_empty() {
            return;
        }

        let device = frame.device;
        let queue = frame.queue;
        self.ensure_pipeline(device, frame.surface_format, frame.gpu_epoch);

        // 单条字幕纹理不能超过这个边长，超了就是 validation 错误（见 rasterize_text）
        let max_dim = device.limits().max_texture_dimension_2d;

        // GPU 资源都是 Arc 句柄，克隆出来用：光栅化要改 `Fontname` → face 缓存
        // （`&mut self`），继续借 `self.xxx` 会让两者打架。
        let (Some(pipeline), Some(bg_layout), Some(sampler), Some(alpha_buffer)) = (
            self.render_pipeline.clone(),
            self.bind_group_layout.clone(),
            self.sampler.clone(),
            self.alpha_buffer.clone(),
        ) else {
            return;
        };

        let items = std::mem::take(&mut self.active_items);
        let (_rect_x, _rect_y, rect_w, rect_h) = area.video_rect;

        // ── 两条缩放链刻意分开 ──────────────────────────────────────
        //
        // `pos_scale`：画布（ASS PlayRes）→ 画面矩形的仿射映射。位置必须跟着
        //   画面走（pillarbox 时才能贴住视频而不是黑边）。
        // `font_scale`：光栅化时的实际像素字号，**只跟视频原生分辨率有关**
        //   （见 OverlayArea::content_scale）。拖窗口 / 旋转 / 分屏时它不变，
        //   于是文字的物理大小保持恒定，缓存也不会被打掉。
        //
        // 位图按 font_scale 光栅化后**原尺寸 1:1 上屏**，绝不能再乘 pos_scale：
        // 那样会让同一个系数同时作用于位置和尺寸，长句在窗口变大时字反而变小。
        let pos_scale_of = |item: &RenderItem| {
            uniform_canvas_scale(item.canvas.0, item.canvas.1, rect_w as u32, rect_h as u32)
        };
        let font_scale_of = |item: &RenderItem| {
            let s = area.content_scale(item.canvas.1);
            if s.is_finite() && s > 0.0 {
                s
            } else {
                1.0
            }
        };

        let hash_of = |item: &RenderItem| Self::item_hash(item, font_scale_of(item));

        // 本帧用到的纹理 key 集合：本帧在用的条目永不淘汰
        let used_hashes: HashSet<u64> = items.iter().map(&hash_of).collect();

        let mut prepared: Vec<(u64, f32)> = Vec::with_capacity(items.len());
        for item in &items {
            let hash = hash_of(item);
            if !self.texture_cache.contains_key(&hash) {
                if let Some((pixels, w, h)) =
                    self.rasterize_text(item, font_scale_of(item), max_dim)
                {
                    let texture = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("subtitle_text"),
                        size: wgpu::Extent3d {
                            // rasterize_text 已经按 max_dim 夹过了，这里再兜一层：
                            // 纹理一旦超限，wgpu 报的是 validation 错误而非本地 panic，
                            // 而 validation 错误会污染整条命令缓冲（后续 bind group、
                            // submit 全部失败），代价远大于丢掉这一条字幕。
                            width: w.clamp(1, max_dim),
                            height: h.clamp(1, max_dim),
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
                            texture: &texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &pixels,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            // 上传尺寸必须和上面 clamp 后的纹理尺寸完全一致，
                            // 否则 wgpu 会因为数据量对不上而报 validation 错误
                            bytes_per_row: Some(w.clamp(1, max_dim) * 4),
                            rows_per_image: Some(h.clamp(1, max_dim)),
                        },
                        wgpu::Extent3d {
                            width: w.clamp(1, max_dim),
                            height: h.clamp(1, max_dim),
                            depth_or_array_layers: 1,
                        },
                    );
                    // 只登记插入顺序，淘汰统一放在本帧 item 处理完之后
                    self.texture_order.push_back(hash);
                    self.texture_cache.insert(
                        hash,
                        CachedTexture {
                            texture,
                            width: w,
                            height: h,
                        },
                    );
                }
            }
            let mult = item.alpha * item.colour[3];
            prepared.push((hash, mult));
        }

        // 本帧在用的条目刷到顺序末尾，再从头部淘汰直到回到容量上限。
        // 旧实现只在"新建纹理且 len > 256"时整表 retain：拖窗口期间每帧都要为每个
        // 活跃条目新建纹理，缓存表持续churn，而上限本身其实靠得住 —— 换成真正的
        // 容量裁剪后，既不再整表清空，也不会让长期播放的会话无限增长。
        self.texture_order
            .retain(|k| self.texture_cache.contains_key(k));
        for hash in &used_hashes {
            if let Some(pos) = self.texture_order.iter().position(|k| k == hash) {
                let k = self.texture_order.remove(pos).expect("position 刚取到");
                self.texture_order.push_back(k);
            }
        }
        while self.texture_order.len() > TEXTURE_CACHE_CAPACITY {
            match self.texture_order.pop_front() {
                Some(k) if !used_hashes.contains(&k) => {
                    self.texture_cache.remove(&k);
                }
                Some(k) => {
                    // 本帧还在用，不能丢；挪到末尾等下一帧再淘汰
                    self.texture_order.push_back(k);
                    break;
                }
                None => break,
            }
        }

        // 一个 render pass 内逐 item 切换 viewport + bind group
        //
        // 先裁到画面矩形：viewport 只决定映射范围，不负责裁剪，长句/大字号
        // 的位图会越过画面边界画到 letterbox 黑边上。
        let Some(scissor) = area.scissor() else {
            self.active_items = items;
            return;
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("subtitle_pass"),
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
        pass.set_pipeline(&pipeline);
        pass.set_scissor_rect(scissor.0, scissor.1, scissor.2, scissor.3);

        for (slot, (item, (hash, mult))) in items.iter().zip(prepared.iter()).enumerate() {
            let Some(cached) = self.texture_cache.get(hash) else {
                continue;
            };

            // 画布坐标 → 画面矩形内的像素 → surface 像素
            //
            // 位置用 `pos_scale`（等比，取两轴较小值）：ASS 的 PlayRes 通常是
            // 1920x1080，而画面矩形可能是 1080x608 的横条。x/y 各自缩放会把 16:9
            // 的字幕横向压扁 —— 字形变形就是这么来的。
            //
            // 尺寸**不**用 pos_scale：位图已经按内容基准的字号光栅化好了，
            // 按原尺寸 1:1 放置即可（`tex_w/tex_h` 直接取缓存尺寸）。这样
            // 拖窗口时位置自适应而字形大小恒定，两者互不干扰。
            let pos_scale = pos_scale_of(item);
            let (x, y, tex_w, tex_h) = subtitle_placement(
                item,
                area.video_rect,
                pos_scale,
                cached.width as f32,
                cached.height as f32,
            );

            // alpha 写入本 slot 的 uniform 区间
            let offset = (slot as u64) * UNIFORM_SLOT;
            queue.write_buffer(&alpha_buffer, offset, &alpha_slot_bytes(*mult));

            let view = cached
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("subtitle_bg"),
                layout: &bg_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &alpha_buffer,
                            offset,
                            size: NonZeroU64::new(16),
                        }),
                    },
                ],
            });

            pass.set_bind_group(0, &bind_group, &[]);
            // set_viewport 使用像素坐标，vertex shader 的 NDC (-1..1) 会自动映射到此矩形
            pass.set_viewport(x, y, tex_w, tex_h, 0.0, 1.0);
            pass.draw(0..4, 0..1);
        }

        self.active_items = items;
    }

    fn is_visible(&self) -> bool {
        self.state.lock().enabled
    }
}

/// 等比**位置**映射系数（contain）：取两轴较小值，保证画布完整放进目标矩形且不变形
///
/// 分开按 x/y 缩放是字幕在竖屏上被压扁的直接原因 —— 1920x1080 的 PlayRes
/// 映射到窄条画面时，x 方向要缩 0.56、y 方向要缩 1.37，比例失守。
///
/// 只用于**位置**。字号走 [`dyl_render::OverlayArea::content_scale`]（内容基准），
/// 两者刻意分开：位置跟画面走，字号跟内容走。
pub fn uniform_canvas_scale(canvas_w: f32, canvas_h: f32, target_w: u32, target_h: u32) -> f32 {
    if canvas_w <= 0.0 || canvas_h <= 0.0 || target_w == 0 || target_h == 0 {
        return 1.0;
    }
    (target_w as f32 / canvas_w).min(target_h as f32 / canvas_h)
}

/// 一条字幕的上屏矩形（surface 像素，左上角 + 宽高）
///
/// 这里刻意把**位置**与**尺寸**分成两个互不相干的输入：
///
/// - `pos_scale`：画布（ASS PlayRes）→ 画面矩形的等比映射，**随窗口变化**。
///   位置必须跟着画面走，pillarbox 时才能贴住视频而不是黑边。
/// - `tex_w/tex_h`：位图尺寸，光栅化时已按**内容基准**字号算好，**随窗口不变**。
///   直接原样使用，绝不再乘 `pos_scale` —— 否则同一个系数会同时作用于位置和
///   尺寸，窗口一变长句的字号反而跟着变。
pub(crate) fn subtitle_placement(
    item: &RenderItem,
    rect: (f32, f32, f32, f32),
    pos_scale: f32,
    tex_w: f32,
    tex_h: f32,
) -> (f32, f32, f32, f32) {
    let (rx, ry, rw, rh) = rect;
    // contain：等比映射后可能有一侧留白，需要在画面矩形内居中
    let offset_x = rx + (rw - item.canvas.0 * pos_scale) / 2.0;
    let offset_y = ry + (rh - item.canvas.1 * pos_scale) / 2.0;

    let ax = offset_x + item.anchor.0 * pos_scale;
    let ay = offset_y + item.anchor.1 * pos_scale;
    let x = match item.align.h() {
        0 => ax,
        1 => ax - tex_w / 2.0,
        _ => ax - tex_w,
    };
    let y = match item.align.v() {
        0 => ay - tex_h,
        1 => ay - tex_h / 2.0,
        _ => ay,
    };
    (x, y, tex_w, tex_h)
}

/// uniform 槽位内容：[alpha, 0, 0, 0]
fn alpha_slot_bytes(mult: f32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&mult.clamp(0.0, 1.0).to_le_bytes());
    bytes
}

#[cfg(test)]
mod rasterize_regression {
    use super::*;

    /// 防回归：ab_glyph 字形 bbox 的 min.y 为负（基线上方）。
    /// 行坐标必须是 line_y + bounds.min.y + gy；用减法会把全部像素推出纹理
    /// （修复前字幕一个像素都画不出）。
    #[test]
    fn rasterize_paints_glyph_pixels() {
        let mut engine = SubtitleEngine::new(SubtitleConfig::default());
        let font = crate::font::load_system_font().expect("system font");
        engine.set_font(font);
        engine.load_events(vec![SubtitleEvent {
            start_time: 1.0,
            end_time: 5.0,
            text: "字幕测试".to_string(),
            style: "Default".to_string(),
        }]);
        engine.update(2.0);
        assert_eq!(engine.active_items.len(), 1);
        let item = engine.active_items[0].clone();
        let (px, w, h) = engine
            .rasterize_text(&item, 1.0, 2048)
            .expect("rasterize should succeed");
        let painted = px.chunks(4).filter(|p| p[3] > 0).count();
        assert!(
            painted > 50,
            "expected glyph coverage, got painted={painted} ({w}x{h})"
        );
    }

    /// 超长台词必须被压回纹理上限内，而不是建出超限纹理
    ///
    /// GLES 后端的 max_texture_dimension_2d 是 2048。ASS 里一条长台词按
    /// fs=40 排版轻易超过这个宽度，之前会直接触发 wgpu validation 错误，
    /// 连带整帧命令缓冲失效（真机上表现为 SIGABRT）。
    /// 光栅化字号必须随等比缩放变化，否则窗口尺寸变了字形大小对不上
    #[test]
    fn rasterized_bitmap_scales_with_canvas_scale() {
        let mut engine = SubtitleEngine::new(SubtitleConfig::default());
        let font = crate::font::load_system_font().expect("system font");
        engine.set_font(font);
        engine.load_events(vec![SubtitleEvent {
            start_time: 1.0,
            end_time: 5.0,
            text: "缩放测试".to_string(),
            style: "Default".to_string(),
        }]);
        engine.update(2.0);
        let item = engine.active_items[0].clone();

        let (_, w_small, _) = engine.rasterize_text(&item, 0.5, 2048).expect("rasterize");
        let (_, w_big, _) = engine.rasterize_text(&item, 1.0, 2048).expect("rasterize");

        assert!(
            w_big > w_small,
            "2x canvas scale should rasterize wider: {w_small} -> {w_big}"
        );
        let ratio = w_big as f32 / w_small as f32;
        assert!(
            (1.8..2.2).contains(&ratio),
            "expected ~2x width ratio, got {ratio}"
        );
    }

    /// 字幕必须按 ASS 的 PlayRes 等比映射到**画面矩形**
    ///
    /// 分轴缩放会把 16:9 的字幕压扁（这正是"文字被压窄"），所以取两轴的较小值。
    /// 映射目标从"整个 surface"改成"letterbox 之后的画面矩形"后，字幕才会贴住
    /// 视频而不是落到黑边上。
    #[test]
    fn pos_scale_is_uniform_and_fits_video_rect() {
        let (canvas_w, canvas_h) = (1920.0f32, 1080.0f32);

        // (surface 宽, surface 高, 视频宽, 视频高)
        for (sw, sh, vw, vh) in [
            (1920.0f32, 1080.0f32, 1920.0f32, 1080.0f32),
            (1080.0, 2340.0, 1920.0, 1080.0), // 竖屏播横屏视频 → letterbox
            (1920.0, 600.0, 1920.0, 1080.0),  // 扁窗口播高视频 → pillarbox
            (2400.0, 1080.0, 1920.0, 1080.0),
        ] {
            let rect = dyl_render::fit_video_rect(sw as u32, sh as u32, vw as u32, vh as u32);
            let rw = rect.2;
            let rh = rect.3;
            let uniform = (rw / canvas_w).min(rh / canvas_h);

            // 映射后必须完整落在画面矩形内（contain，不是 cover）
            assert!(
                canvas_w * uniform <= rw + 0.5,
                "scaled width {} exceeds rect {rw}",
                canvas_w * uniform
            );
            assert!(
                canvas_h * uniform <= rh + 0.5,
                "scaled height {} exceeds rect {rh}",
                canvas_h * uniform
            );
            // 宽高比必须保持 —— 分别缩放的话这个等式不成立
            let aspect_before = canvas_w / canvas_h;
            let aspect_after = (canvas_w * uniform) / (canvas_h * uniform);
            assert!(
                (aspect_before - aspect_after).abs() < 1e-4,
                "aspect changed: {aspect_before} -> {aspect_after} (rect {rw}x{rh})"
            );
        }
    }

    /// 核心保证：拖动窗口时字幕**位置自适应**但**尺寸恒定**
    ///
    /// 用户诉求原文是"不会跟随窗口拉伸或压缩，能自适应位置等属性，但大小不变"。
    /// 这两个性质由两个独立系数实现：
    /// - `content_scale`（字号）只看视频原生分辨率 → 拖窗口不变
    /// - `pos_scale`（位置映射）看画面矩形 → 拖窗口跟着走
    #[test]
    fn subtitle_keeps_glyph_size_across_windows_but_tracks_video_rect() {
        let mut engine = SubtitleEngine::new(SubtitleConfig::default());
        let font = crate::font::load_system_font().expect("system font");
        engine.set_font(font);
        engine.load_events(vec![SubtitleEvent {
            start_time: 1.0,
            end_time: 5.0,
            text: "大小不变".to_string(),
            style: "Default".to_string(),
        }]);
        engine.update(2.0);
        let item = engine.active_items[0].clone();

        // 同一个 1080p 视频，铺在各种尺寸的窗口里
        let windows = [
            (1920u32, 1080u32),
            (1080, 2340), // 竖屏手机
            (2560, 1440),
            (640, 480),
        ];

        let mut glyph_sizes = Vec::new();
        for (sw, sh) in windows {
            let area = dyl_render::OverlayArea::new(sw, sh, 1920, 1080);

            // 字号系数：与窗口无关
            let font_scale = area.content_scale(item.canvas.1);
            assert!(
                (font_scale - 1.0).abs() < 1e-6,
                "1080p 视频在 {sw}x{sh} 窗口下字号系数应为 1.0，实际 {font_scale}"
            );

            // 位图尺寸按这个系数光栅化
            let (_, tw, th) = engine
                .rasterize_text(&item, font_scale, 2048)
                .expect("rasterize");

            // 位置映射看画面矩形
            let rect = area.video_rect;
            let pos_scale =
                uniform_canvas_scale(item.canvas.0, item.canvas.1, rect.2 as u32, rect.3 as u32);
            let (x, y, _, _) = subtitle_placement(&item, rect, pos_scale, tw as f32, th as f32);

            // 字幕必须落在画面矩形内（不能跑到黑边上）
            assert!(
                x >= rect.0 - 1.0 && y >= rect.1 - 1.0,
                "{sw}x{sh}: 字幕左上角 ({x}, {y}) 跑到画面矩形 {rect:?} 之外"
            );
            assert!(
                x + tw as f32 <= rect.0 + rect.2 + 1.0,
                "{sw}x{sh}: 字幕右边缘越过画面矩形"
            );

            glyph_sizes.push((tw, th));
        }

        // 所有窗口下的位图尺寸必须完全一致
        for (w, h) in &glyph_sizes[1..] {
            assert_eq!(
                (*w, *h),
                glyph_sizes[0],
                "字号跟着窗口变了：{:?} → {:?}",
                glyph_sizes[0],
                (w, h)
            );
        }
    }

    #[test]
    fn oversized_subtitle_is_clamped_to_texture_limit() {
        let mut engine = SubtitleEngine::new(SubtitleConfig::default());
        let font = crate::font::load_system_font().expect("system font");
        engine.set_font(font);

        // 约 200 个汉字，fs=40 在 1920 画布上远超 2048 像素宽
        let long: String = "超长字幕".repeat(50);
        engine.load_events(vec![SubtitleEvent {
            start_time: 1.0,
            end_time: 5.0,
            text: long,
            style: "Default".to_string(),
        }]);
        engine.update(2.0);

        let item = engine.active_items[0].clone();
        let (_, w, h) = engine
            .rasterize_text(&item, 1.0, 2048)
            .expect("rasterize should succeed");
        assert!(w <= 2048, "width {w} exceeds texture limit");
        assert!(h <= 2048, "height {h} exceeds texture limit");
        assert!(w > 0 && h > 0, "clamped size must stay non-zero");
    }

    /// 可读性核心保证：默认样式必须烘焙出黑色描边
    ///
    /// 白字直接压在浅色画面上等于消失（用户报"字幕看不清"的根因）。
    /// 描边在 CPU 光栅化阶段合成进 RGBA 位图，位图因此比无描边版更大。
    #[test]
    fn outline_bakes_black_border_around_glyphs() {
        let mut engine = SubtitleEngine::new(SubtitleConfig::default());
        let font = crate::font::load_system_font().expect("system font");
        engine.set_font(font);
        engine.load_events(vec![SubtitleEvent {
            start_time: 1.0,
            end_time: 5.0,
            text: "描边测试".to_string(),
            style: "Default".to_string(),
        }]);
        engine.update(2.0);
        let item = engine.active_items[0].clone();
        assert!(
            (item.outline - 2.0).abs() < f32::EPSILON,
            "默认样式必须带描边"
        );

        let (px_border, wb, hb) = engine.rasterize_text(&item, 1.0, 2048).unwrap();
        let mut plain = item.clone();
        plain.outline = 0.0;
        plain.shadow = 0.0;
        let (px_none, w0, h0) = engine.rasterize_text(&plain, 1.0, 2048).unwrap();

        let dark = |px: &[u8]| {
            px.chunks(4)
                .filter(|p| p[3] > 128 && (p[0] as u32 + p[1] as u32 + p[2] as u32) < 96)
                .count()
        };
        assert!(
            dark(&px_border) > 100,
            "描边版应含大量黑描边像素，got {}",
            dark(&px_border)
        );
        assert_eq!(dark(&px_none), 0, "无描边时必须还是单层白字");

        // 正文仍是主色（白）
        let white = px_border
            .chunks(4)
            .filter(|p| p[3] > 200 && p[0] > 200 && p[1] > 200 && p[2] > 200)
            .count();
        assert!(white > 100, "字形正文像素必须保留");

        assert!(
            wb > w0 && hb > h0,
            "外扩描边后位图必须更大: {wb}x{hb} vs {w0}x{h0}"
        );
    }

    /// \shad 在描边之外再垫一层同色阴影带
    #[test]
    fn shadow_extends_band_beyond_outline() {
        let mut engine = SubtitleEngine::new(SubtitleConfig::default());
        let font = crate::font::load_system_font().expect("system font");
        engine.set_font(font);
        engine.load_events(vec![SubtitleEvent {
            start_time: 1.0,
            end_time: 5.0,
            text: "阴影".to_string(),
            style: "Default".to_string(),
        }]);
        engine.update(2.0);
        let item = engine.active_items[0].clone();

        let mut only_border = item.clone();
        only_border.shadow = 0.0;
        let mut with_shadow = item.clone();
        with_shadow.shadow = 6.0;

        let (px_b, wb, hb) = engine.rasterize_text(&only_border, 1.0, 2048).unwrap();
        let (px_s, ws, hs) = engine.rasterize_text(&with_shadow, 1.0, 2048).unwrap();
        assert!(
            ws > wb && hs > hb,
            "阴影必须把位图再撑大: {ws}x{hs} vs {wb}x{hb}"
        );
        let dark = |px: &[u8]| {
            px.chunks(4)
                .filter(|p| p[3] > 64 && (p[0] as u32 + p[1] as u32 + p[2] as u32) < 96)
                .count()
        };
        assert!(dark(&px_s) > dark(&px_b), "阴影带应增加深色外围像素");
    }

    /// EDT 必须与暴力法逐像素一致（描边半径的直接依据）
    #[test]
    fn edt_sq_matches_brute_force() {
        let (w, h) = (23usize, 17usize);
        let mut cover = vec![0f32; w * h];
        let mut seed = 0x9E3779B9u32;
        for c in cover.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            if (seed >> 20).is_multiple_of(5) {
                *c = 1.0;
            }
        }
        cover[5 * w + 5] = 1.0; // 保证非空

        let got = edt_sq(&cover, w, h);
        for y in 0..h {
            for x in 0..w {
                let mut best = f32::INFINITY;
                for yy in 0..h {
                    for xx in 0..w {
                        if cover[yy * w + xx] >= 0.5 {
                            let d = (x as f32 - xx as f32).powi(2) + (y as f32 - yy as f32).powi(2);
                            best = best.min(d);
                        }
                    }
                }
                assert!(
                    (got[y * w + x] - best).abs() < 1e-3,
                    "pixel ({x},{y}): got {} expected {best}",
                    got[y * w + x]
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ass::AssDocument;

    fn event(start: f64, end: f64, text: &str) -> SubtitleEvent {
        SubtitleEvent {
            start_time: start,
            end_time: end,
            text: text.to_string(),
            style: "Default".to_string(),
        }
    }

    fn engine_with_doc(doc: AssDocument) -> SubtitleEngine {
        let engine = SubtitleEngine::new(SubtitleConfig::default());
        {
            let mut state = engine.state.lock();
            state.ass_events = doc.events;
            state.styles = doc.styles;
            state.play_res = (doc.play_res_x, doc.play_res_y);
            state.events = Vec::new();
        }
        engine
    }

    fn ass(src: &str) -> AssDocument {
        AssParser::new().parse_document(src.as_bytes())
    }

    use crate::parser::AssParser;

    /// 用户显式加载字幕文件后，内嵌轨的实时追加不得再混入同一事件表
    #[test]
    fn append_events_ignored_after_external_file_load() {
        let dir = std::env::temp_dir().join(format!("dyl_sub_ext_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ext.ass");
        std::fs::write(
            &path,
            "[Script Info]\nPlayResX: 1920\nPlayResY: 1080\n\n\
             [V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour\n\
             Style: Default,Arial,60,&H00FFFFFF\n\n\
             [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
             Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,外部字幕\n",
        )
        .unwrap();

        let engine = SubtitleEngine::new(SubtitleConfig::default());
        engine.load_from_file(path.to_str().unwrap()).unwrap();
        let state = engine.shared_state();
        let after_load = state.lock().events.len();

        engine.append_events(vec![event(1.0, 2.0, "内嵌轨")]);
        assert_eq!(
            state.lock().events.len(),
            after_load,
            "外部字幕文件生效期间应忽略内嵌轨追加"
        );

        // 换回事件列表加载（如内嵌轨自身重新载入）则恢复追加能力
        engine.load_events(vec![event(1.0, 2.0, "内嵌轨")]);
        engine.append_events(vec![event(5.0, 6.0, "内嵌轨2")]);
        assert_eq!(state.lock().events.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_plain_event_anchors_bottom_center() {
        let mut engine = SubtitleEngine::new(SubtitleConfig::default());
        engine.load_events(vec![event(1.0, 5.0, "Hello")]);
        engine.update(2.0);
        assert_eq!(engine.active_items.len(), 1);
        let item = &engine.active_items[0];
        assert_eq!(item.text, "Hello");
        // config 画布 1920x1080，底部居中：锚点 (960, 1080-50)
        assert_eq!(item.anchor, (960.0, 1030.0));
        assert_eq!(item.align, Alignment(2));
        // current_text 兼容语义保持
        assert_eq!(engine.state.lock().current_text, "Hello");
    }

    #[test]
    fn test_an8_moves_anchor_to_top() {
        let doc = ass(r#"[Script Info]
PlayResX: 640
PlayResY: 360

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,{\an8}top line
"#);
        let mut engine = engine_with_doc(doc);
        engine.update(2.0);
        let item = &engine.active_items[0];
        assert_eq!(item.canvas, (640.0, 360.0));
        assert_eq!(item.align, Alignment(8));
        // an8：顶部，y = margin_v（无样式 → config bottom_margin 50）
        assert_eq!(item.anchor, (320.0, 50.0));
    }

    #[test]
    fn test_pos_and_fad_and_colour() {
        let doc = ass(r#"[Script Info]
PlayResX: 1000
PlayResY: 500

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:11.00,Default,,0,0,0,,{\pos(120,80)\fad(1000,0)\c&H0000FF&}fx
"#);
        let mut engine = engine_with_doc(doc);

        engine.update(1.5); // 淡入中点
        let item = &engine.active_items[0];
        assert_eq!(item.anchor, (120.0, 80.0));
        assert_eq!(item.colour, [1.0, 0.0, 0.0, 1.0]);
        assert!((item.alpha - 0.5).abs() < 0.01);

        engine.update(5.0); // 淡入完成
        assert!((engine.active_items[0].alpha - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_move_interpolates_per_frame() {
        let doc = ass(r#"[Script Info]
PlayResX: 1920
PlayResY: 1080

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:10.00,0:00:20.00,Default,,0,0,0,,{\move(100,100,500,100)}scroll
"#);
        let mut engine = engine_with_doc(doc);
        engine.update(12.5);
        assert_eq!(engine.active_items[0].anchor, (200.0, 100.0));
        engine.update(15.0);
        assert_eq!(engine.active_items[0].anchor, (300.0, 100.0));
        engine.update(19.0);
        assert_eq!(engine.active_items[0].anchor, (460.0, 100.0));
    }

    #[test]
    fn test_style_fontsize_colour_margin_from_table() {
        let doc = ass(r#"[Script Info]
PlayResX: 320
PlayResY: 240

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Big,MS PGothic,36,&H0000FF00&,&H00000000,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,6,10,10,20

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Big,,0,0,0,,styled
"#);
        let mut engine = engine_with_doc(doc);
        engine.update(2.0);
        let item = &engine.active_items[0];
        assert_eq!(item.font_size, 36.0);
        assert_eq!(item.colour, [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(item.align, Alignment(6)); // 右中
                                              // an6 右中：x = 320 - margin_r(10)，y = 240/2
        assert_eq!(item.anchor, (310.0, 120.0));
    }

    #[test]
    fn test_multi_event_layer_ordering_and_cap() {
        let mut src = String::from(
            "[Script Info]\nPlayResX: 1920\nPlayResY: 1080\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
        );
        for i in 0..12 {
            src.push_str(&format!(
                "Dialogue: {},0:00:01.00,0:00:05.00,Default,,0,0,0,,line {}\n",
                i % 3,
                i
            ));
        }
        let doc = ass(&src);
        let mut engine = engine_with_doc(doc);
        engine.update(2.0);
        // 12 条低于上限，全部保留
        assert_eq!(engine.active_items.len(), 12);
        // layer 升序绘制：首个 item 来自最低 layer（i % 3 == 0 → i=0）
        assert_eq!(engine.active_items[0].text, "line 0");
    }

    #[test]
    fn test_overflow_keeps_newest_events() {
        // 模拟 Danmaku2ASS 滚动弹幕：超出上限的事件错峰开始、同层、持续 8s
        let n = MAX_ITEMS + 18;
        let mut src = String::from(
            "[Script Info]\nPlayResX: 1920\nPlayResY: 1080\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
        );
        for i in 0..n {
            let start = i as f64 * 0.02;
            src.push_str(&format!(
                "Dialogue: 0,{:},{:},Default,,0,0,0,,{{\\move(2040,40,-120,40)}}dan {}\n",
                fmt_s(start),
                fmt_s(start + 8.0),
                i
            ));
        }
        let doc = ass(&src);
        let mut engine = engine_with_doc(doc);
        engine.update(3.0);
        assert_eq!(engine.active_items.len(), MAX_ITEMS);
        let texts: Vec<&str> = engine
            .active_items
            .iter()
            .map(|i| i.text.as_str())
            .collect();
        // 保留的是最新的 MAX_ITEMS 条：最后一条在、最老的 d0 被丢弃
        let last = format!("dan {}", n - 1);
        assert!(texts.contains(&last.as_str()), "texts={texts:?}");
        assert!(
            !texts.contains(&"dan 0"),
            "oldest must be dropped: {texts:?}"
        );
        // 最新一条刚上场，锚点应仍在移动起点（画布右侧）附近，而非终点
        let newest = engine.active_items.iter().find(|i| i.text == last).unwrap();
        assert!(
            newest.anchor.0 > 1900.0,
            "newest anchor should be near move start, got {:?}",
            newest.anchor
        );
    }

    fn fmt_s(secs: f64) -> String {
        let total = secs.max(0.0);
        let h = (total / 3600.0) as u32;
        let m = ((total / 60.0) as u32) % 60;
        let s = (total % 60.0) as u32;
        let cs = ((total * 100.0) as u32) % 100;
        format!("{h}:{m:02}:{s:02}.{cs:02}")
    }

    #[test]
    fn test_fs_override() {
        let doc = ass(r#"[Script Info]
PlayResX: 1920
PlayResY: 1080

[V4+ Styles]
Format: Name,Fontname,Fontsize,PrimaryColour,SecondaryColour,OutlineColour,BackColour,Bold,Italic,Underline,StrikeOut,ScaleX,ScaleY,Spacing,Angle,BorderStyle,Outline,Shadow,Alignment,MarginL,MarginR,MarginV,Encoding
Style: Default,Arial,20,&H00FFFFFF,0,0,0,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,{\fs48}big{\fs20} small
"#);
        let mut engine = engine_with_doc(doc);
        engine.update(2.0);
        // 后写赢：整条事件用最后出现的 \fs
        assert_eq!(engine.active_items[0].font_size, 20.0);
        assert_eq!(engine.active_items[0].text, "big small");
    }

    /// 内封轨的样式表在字幕流的 CodecPrivate 里：装上后事件才按作者设定的
    /// 字号/颜色/边距渲染，而不是回退到 config 默认
    #[test]
    fn embedded_script_styles_apply_to_appended_events() {
        use crate::parser::parse_embedded_dialogue;

        let script = "[Script Info]\nPlayResX: 1280\nPlayResY: 720\n\n\
             [V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, OutlineColour, Outline, Shadow, Alignment, MarginL, MarginR, MarginV\n\
             Style: Default,MS PGothic,62,&H0000FFFF,&H00000000,3,0,2,10,10,60\n\n\
             [Events]\nDialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,脚本头里的事件不该被采用\n";

        let mut engine = SubtitleEngine::new(SubtitleConfig::default());
        assert!(engine.apply_embedded_script(script.as_bytes()));

        let ev =
            parse_embedded_dialogue("0,0,Default,,0,0,0,,\t\t正文\t", 1.0, 4.0).expect("parses");
        engine.append_ass_events(vec![ev]);
        engine.update(2.0);

        let item = &engine.active_items[0];
        assert_eq!(item.text, "正文");
        assert_eq!(item.font_size, 62.0, "字号来自样式表");
        assert_eq!(item.colour, [1.0, 1.0, 0.0, 1.0], "&H0000FFFF = 黄");
        assert_eq!(item.outline, 3.0);
        // PlayRes 来自脚本头，底部居中对齐 + MarginV 60
        assert_eq!(item.canvas, (1280.0, 720.0));
        assert_eq!(item.anchor, (640.0, 660.0));
        assert_eq!(
            engine.state.lock().events.len(),
            1,
            "脚本头只提供样式表，其事件时间轴与容器无关"
        );
    }

    /// 用户显式加载外部字幕后，内嵌脚本头不得覆盖外挂样式表
    #[test]
    fn embedded_script_ignored_after_external_file_load() {
        let dir = std::env::temp_dir().join(format!("dyl_sub_script_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ext.ass");
        std::fs::write(
            &path,
            "[Script Info]\nPlayResX: 640\nPlayResY: 360\n\n\
             [V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default,Arial,30\n",
        )
        .unwrap();

        let engine = SubtitleEngine::new(SubtitleConfig::default());
        engine.load_from_file(path.to_str().unwrap()).unwrap();

        let script = "[V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default,Arial,90\n";
        assert!(
            !engine.apply_embedded_script(script.as_bytes()),
            "外部字幕生效期间不覆盖样式表"
        );
        assert_eq!(engine.state.lock().play_res, (640.0, 360.0));

        // 没有样式表的脚本头（如纯文本轨的 CodecPrivate）报告失败，保持默认样式
        assert!(!engine.apply_embedded_script(b"[Script Info]\nTitle: x\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 内嵌 ASS 追加的事件同时进入渲染数据源与纯文本视图
    #[test]
    fn append_ass_events_updates_both_views() {
        use crate::parser::parse_embedded_dialogue;

        let engine = SubtitleEngine::new(SubtitleConfig::default());
        let ev = parse_embedded_dialogue("0,0,Graphics,,0,0,0,,OP59", 1.0, 2.0).expect("parses");
        engine.append_ass_events(vec![ev]);
        engine.append_ass_events(Vec::new());

        let state = engine.state.lock();
        assert_eq!(state.ass_events.len(), 1);
        assert_eq!(state.events.len(), 1);
        assert_eq!(state.events[0].style, "Graphics");
    }

    /// 外部字幕生效期间，内嵌轨的实时追加继续被忽略
    #[test]
    fn append_ass_events_ignored_after_external_file_load() {
        let dir = std::env::temp_dir().join(format!("dyl_sub_extass_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ext.srt");
        std::fs::write(&path, "1\n00:00:01,000 --> 00:00:03,000\n外挂字幕\n").unwrap();

        let engine = SubtitleEngine::new(SubtitleConfig::default());
        engine.load_from_file(path.to_str().unwrap()).unwrap();
        let after_load = engine.state.lock().ass_events.len();

        use crate::parser::parse_embedded_dialogue;
        let ev = parse_embedded_dialogue("0,0,Default,,0,0,0,,内嵌轨", 1.0, 2.0).expect("parses");
        engine.append_ass_events(vec![ev]);
        let state = engine.state.lock();
        assert_eq!(state.ass_events.len(), after_load);
        assert_eq!(state.events.len(), after_load);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两条样式不同（字号/颜色/字体）的默认样式表，配合 `\N` 做行内换样式测试
    fn two_style_doc(dialogue_text: &str) -> AssDocument {
        ass(&format!(
            r#"[Script Info]
PlayResX: 1920
PlayResY: 1080

[V4+ Styles]
Format: Name,Fontname,Fontsize,PrimaryColour,SecondaryColour,OutlineColour,BackColour,Bold,Italic,Underline,StrikeOut,ScaleX,ScaleY,Spacing,Angle,BorderStyle,Outline,Shadow,Alignment,MarginL,MarginR,MarginV,Encoding
Style: Default,Arial,40,&H00FFFFFF,0,0,0,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10
Style: Sign,Times New Roman,72,&H000000FF,0,0,0,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,{dialogue_text}
"#
        ))
    }

    /// 行首 `{\r样式名}`：那一行整行改用被点名的样式
    ///
    /// 只支持行首切换（`\N` 之后立刻出现）；行内混排要逐 run 量测步进，属于
    /// libass 式 render tree，不在本引擎范围。
    #[test]
    fn inline_style_switch_splits_the_item_into_line_blocks() {
        let mut engine = engine_with_doc(two_style_doc(r"第一行{\rSign}\N第二行"));
        engine.update(2.0);
        let item = &engine.active_items[0];
        assert_eq!(item.text, "第一行\n第二行");
        assert_eq!(item.lines.len(), 2, "外观不同的行必须逐行成图");
        assert_eq!(item.lines[0].font_name, "Arial");
        assert_eq!(item.lines[0].font_size, 40.0);
        assert_eq!(item.lines[1].font_name, "Times New Roman");
        assert_eq!(item.lines[1].font_size, 72.0);
        assert_eq!(
            item.lines[1].colour,
            [1.0, 0.0, 0.0, 1.0],
            "&H000000FF = 红"
        );
        // 整条字幕仍按事件自己的样式定位
        assert_eq!(item.font_size, 40.0);
    }

    /// `{\r}`（不带名字）回到脚本默认样式；各行外观一样时保持单张位图
    #[test]
    fn identical_line_styles_keep_the_single_bitmap_path() {
        let mut engine = engine_with_doc(two_style_doc(r"甲{\r}\N乙"));
        engine.update(2.0);
        assert!(
            engine.active_items[0].lines.is_empty(),
            "回默认样式的两行外观一致，不该拆成多块"
        );

        // 单纯换行（无样式切换）同样走老路径，且 `\fs` 跨行延续
        let mut engine = engine_with_doc(two_style_doc(r"{\fs48}甲\N乙"));
        engine.update(2.0);
        let item = &engine.active_items[0];
        assert!(item.lines.is_empty(), "两行外观一致 → 单张位图");
        assert_eq!(item.font_size, 48.0);
    }

    /// 逐行样式的位图必须同时含两行的正文颜色
    ///
    /// 防回归：如果拼接时只取第一块的颜色（或第二块被画在同一位置），
    /// 换样式在画面上就是没生效。
    #[test]
    fn mixed_line_bitmap_paints_both_styles() {
        let mut engine = engine_with_doc(two_style_doc(r"上白{\rSign}下红"));
        // 行内（非行首）切换不改本行 —— 这里刻意用 `\N` 让它落在行首
        engine.update(2.0);
        assert!(
            engine.active_items[0].lines.is_empty(),
            "同一行没有换行，仍是单样式单块"
        );

        let mut engine = engine_with_doc(two_style_doc(r"上白{\rSign}\N下红"));
        engine.set_font(crate::font::load_system_font().expect("system font"));
        engine.update(2.0);
        let item = engine.active_items[0].clone();
        let (px, w, h) = engine.rasterize_text(&item, 1.0, 2048).expect("rasterize");

        let solid = |r: u8, g: u8, b: u8| {
            px.chunks(4)
                .filter(|p| {
                    p[3] > 200
                        && p[0].abs_diff(r) < 24
                        && p[1].abs_diff(g) < 24
                        && p[2].abs_diff(b) < 24
                })
                .count()
        };
        assert!(solid(255, 0, 0) > 30, "第二行的红字没被烘焙进去 ({w}x{h})");
        assert!(
            solid(255, 255, 255) > 30,
            "第一行的白字没被烘焙进去 ({w}x{h})"
        );

        // 两行必须纵向堆叠：位图高度约等于两块之和，而不是只装得下一行
        let upper = px
            .chunks(4)
            .take((w * h / 2) as usize)
            .filter(|p| p[3] > 0)
            .count();
        let lower = px
            .chunks(4)
            .skip((w * h / 2) as usize)
            .filter(|p| p[3] > 0)
            .count();
        assert!(
            upper > 30 && lower > 30,
            "两行应分居上下半区: {upper}/{lower}"
        );
    }

    /// 逐行样式改变外观 → 纹理缓存 key 必须不同，否则换样式后会复用旧纹理
    #[test]
    fn style_switch_changes_the_texture_cache_key() {
        let mut plain = engine_with_doc(two_style_doc(r"甲\N乙"));
        plain.update(2.0);
        let mut styled = engine_with_doc(two_style_doc(r"甲{\rSign}\N乙"));
        styled.update(2.0);

        let plain_item = plain.active_items[0].clone();
        let styled_item = styled.active_items[0].clone();
        assert!(!styled_item.lines.is_empty());
        assert_ne!(
            SubtitleEngine::item_hash(&plain_item, 1.0),
            SubtitleEngine::item_hash(&styled_item, 1.0),
            "同一文本换了行样式，缓存 key 必须变"
        );
    }

    /// 样式的 `Fontname` 要传到渲染项
    #[test]
    fn style_fontname_reaches_the_render_item() {
        let mut engine = engine_with_doc(two_style_doc(r"正文{\rSign}\N换行"));
        engine.update(2.0);
        let item = &engine.active_items[0];
        assert_eq!(item.font_name, "Arial");
        assert_eq!(item.lines[1].font_name, "Times New Roman");
    }

    /// 系统里没有 `Fontname` 点名的字体时回退内置字体，字幕不能整条消失
    ///
    /// 这是 `Fontname` 支持的底线：作者写的字体名在用户机器上不存在是常态。
    #[test]
    fn unknown_fontname_falls_back_and_still_paints() {
        let doc = ass(r#"[Script Info]
PlayResX: 1920
PlayResY: 1080

[V4+ Styles]
Format: Name,Fontname,Fontsize
Style: Default,Dyl Font That Does Not Exist,40

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,回退字体测试
"#);
        let mut engine = engine_with_doc(doc);
        engine.set_font(crate::font::load_system_font().expect("system font"));
        engine.update(2.0);
        assert_eq!(
            engine.active_items[0].font_name,
            "Dyl Font That Does Not Exist"
        );

        let item = engine.active_items[0].clone();
        let (px, w, h) = engine
            .rasterize_text(&item, 1.0, 2048)
            .expect("unknown Fontname must fall back");
        let painted = px.chunks(4).filter(|p| p[3] > 0).count();
        assert!(
            painted > 50,
            "回退字体没画上字 ({w}x{h}, painted={painted})"
        );
    }

    /// 逐行拼接后的整张位图同样受纹理边长上限约束
    ///
    /// 两块各自合规不代表拼起来合规：拼接高度是两块之和，超上限会直接触发
    /// wgpu validation 错误并废掉整帧命令缓冲。
    #[test]
    fn mixed_lines_are_clamped_to_the_texture_limit() {
        let long: String = "超长字幕".repeat(30);
        let doc = two_style_doc(&format!(r"{long}{{\rSign}}\N{long}"));
        let mut engine = engine_with_doc(doc);
        engine.set_font(crate::font::load_system_font().expect("system font"));
        engine.update(2.0);
        assert_eq!(engine.active_items[0].lines.len(), 2);

        let item = engine.active_items[0].clone();
        let (_, w, h) = engine
            .rasterize_text(&item, 1.0, 2048)
            .expect("rasterize should succeed");
        assert!(w <= 2048, "width {w} exceeds texture limit");
        assert!(h <= 2048, "height {h} exceeds texture limit");
        assert!(w > 0 && h > 0, "clamped size must stay non-zero");
    }

    /// 真实双语字幕的行首 `{\r样式名}` 回归（素材不在仓库，用环境变量指路）
    ///
    /// fan-sub 手写脚本最常用的形态就是 `中文\N{\rDefault-JP}日文`：整条事件的
    /// Style 只有一行样式，第二行的字体/字号靠 `{\r}` 换。这种写法不是我们自己造的
    /// 样式表能覆盖全的，所以留一个可选回归：
    /// `DYL_ASS_RESET_SAMPLE=<path.ass> cargo test -p dyl-subtitle -- --nocapture`
    #[test]
    fn real_bilingual_ass_resets_style_at_line_start() {
        let Ok(path) = std::env::var("DYL_ASS_RESET_SAMPLE") else {
            return;
        };
        let mut engine = SubtitleEngine::new(SubtitleConfig::default());
        engine.set_font(crate::font::load_system_font().expect("system font"));
        engine.load_from_file(&path).expect("parse sample");

        // 只看真的写了 `{\r…}` 的事件；每条取开头 50ms 处求值
        let targets: Vec<f64> = {
            let state = engine.state.lock();
            state
                .ass_events
                .iter()
                .filter(|ev| {
                    ev.segments
                        .iter()
                        .any(|(tags, _)| tags.reset_style.is_some())
                })
                .map(|ev| ev.base.start_time + 0.05)
                .take(40)
                .collect()
        };
        assert!(
            !targets.is_empty(),
            "{path} 里没有任何行首样式复位，素材不对"
        );

        let mut mixed = 0usize;
        let mut first_mixed: Option<RenderItem> = None;
        let sampled = targets.len();
        for pts in targets {
            engine.update(pts);
            for item in &engine.active_items {
                if item.lines.is_empty() {
                    continue;
                }
                mixed += 1;
                assert!(
                    item.lines.iter().any(|l| !l.looks_same(&item.block_spec())),
                    "lines 非空但没有一行与整条字幕外观不同（pts={pts}）"
                );
                if first_mixed.is_none() {
                    first_mixed = Some(item.clone());
                }
            }
        }
        eprintln!("real_bilingual_ass: {sampled} 条事件写了 {{\\r…}}，其中 {mixed} 条按行拆成多块",);
        assert!(
            mixed > 0,
            "素材里写了样式复位却没有一条真的换掉外观，检查样式表解析"
        );

        // 拆出来的行样式要真能成图：逐行光栅化 + 纵向拼接，仍受纹理边长上限约束
        let item = first_mixed.expect("mixed item");
        let (_, w, h) = engine
            .rasterize_text(&item, 1.0, 2048)
            .expect("real mixed-style line must rasterize");
        assert!(w > 0 && h > 0, "empty bitmap");
        assert!(
            w <= 2048 && h <= 2048,
            "bitmap {w}x{h} exceeds texture limit"
        );
    }
}
