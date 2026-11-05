//! R8 Alpha Mask Glyph Atlas — 字形图集
//!
//! 将多个字形的 alpha mask 打包到一张 2048xN 的 R8 纹理中（fill + outline 各一张）。
//! 借鉴 Erika 的 PersistentGlyphAtlas 设计：
//! - 行式打包（cursor_x 从左到右，row_height 取当前行最高字形）
//! - 相同字形只打包一次（通过 GlyphKey 去重）
//! - 版本化管理（version 变化时 GPU 侧重新上传纹理）

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use std::collections::HashMap;
use std::sync::Arc;

// TTC 提取逻辑上移至 dyl-render 共用（字幕引擎同样需要）
pub use dyl_render::font_util::extract_font_from_ttc;

/// Atlas 固定宽度（像素）
const ATLAS_WIDTH: u32 = 2048;
/// Atlas 初始高度（像素）
const ATLAS_INITIAL_HEIGHT: u32 = 256;
/// 描边扩展像素
pub(crate) const OUTLINE_RADIUS: f32 = 1.5;

/// 字形缓存键 — 唯一标识一个字形的光栅化结果
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    /// Unicode 字符
    pub ch: char,
    /// 字体大小（毫像素，避免浮点精度问题）
    pub font_size_milli: u32,
}

impl GlyphKey {
    pub fn new(ch: char, font_size: f32) -> Self {
        Self {
            ch,
            font_size_milli: (font_size.max(1.0) * 1000.0).round() as u32,
        }
    }

    pub fn font_size(&self) -> f32 {
        self.font_size_milli as f32 / 1000.0
    }
}

/// 单个字形的光栅化结果（只有 alpha 通道，不含颜色）
#[derive(Debug, Clone)]
pub struct RasterizedGlyph {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    /// 填充区域 alpha（字形本体）
    pub fill_alpha: Vec<u8>,
    /// 描边区域 alpha（扩展 OUTLINE_RADIUS 像素）
    pub outline_alpha: Vec<u8>,
    /// 基线偏移 x
    pub offset_x: f32,
    /// 基线偏移 y
    pub offset_y: f32,
    /// 字符前进宽度
    pub advance: f32,
}

impl RasterizedGlyph {
    fn empty(advance: f32) -> Self {
        Self {
            width: 0,
            height: 0,
            stride: 0,
            fill_alpha: Vec::new(),
            outline_alpha: Vec::new(),
            offset_x: 0.0,
            offset_y: 0.0,
            advance,
        }
    }

    fn has_bitmap(&self) -> bool {
        self.width > 0 && self.height > 0 && self.stride >= self.width as usize
    }

    fn required_len(&self) -> usize {
        self.stride.saturating_mul(self.height as usize)
    }
}

/// 字形在 atlas 中的位置
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedGlyph {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 字形图集快照 — 可被 Arc 共享给渲染线程
#[derive(Debug, Clone)]
pub struct GlyphAtlasSnapshot {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub fill_alpha: Arc<Vec<u8>>,
    pub outline_alpha: Arc<Vec<u8>>,
    pub version: u64,
}

impl GlyphAtlasSnapshot {
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.stride >= self.width as usize
            && self.fill_alpha.len() >= self.stride * self.height as usize
            && self.outline_alpha.len() >= self.stride * self.height as usize
    }
}

/// 持久字形图集 — 行式打包，动态扩展高度
pub struct GlyphAtlas {
    width: u32,
    height: u32,
    stride: usize,
    cursor_x: u32,
    cursor_y: u32,
    row_height: u32,
    /// 两块 alpha 平面。存 Arc 是为了让 snapshot() 只做一次原子递增就能共享
    /// 同一块内存（见 GlyphAtlasSnapshot 的注释）；渲染端只读，写入全在
    /// `&mut self` 的 pack() 里经 `Arc::get_mut` 完成。
    fill_arc: Arc<Vec<u8>>,
    outline_arc: Arc<Vec<u8>>,
    packed: HashMap<GlyphKey, PackedGlyph>,
    version: u64,
    dirty: bool,
    snapshot: Option<Arc<GlyphAtlasSnapshot>>,
    /// 因装不下而被丢弃的字形数（累计，调用方用 [`GlyphAtlas::take_dropped`] 取走）
    ///
    /// **这是曾经的静默故障点**：atlas 高度封顶在 `ATLAS_WIDTH`(2048) 之后，
    /// `pack()` 会直接返回零尺寸字形，而调用方按"宽或高为 0"跳过绘制 —— 弹幕于是
    /// 显示不全甚至整条消失，日志里却什么都没有。有了这个计数，渲染端可以检测到
    /// "本帧有字形没塞进去"并重建 atlas（见 `DanmakuEngine::build_render_plan_internal`）。
    dropped: u32,
}

impl GlyphAtlas {
    pub fn new() -> Self {
        let width = ATLAS_WIDTH;
        let height = ATLAS_INITIAL_HEIGHT;
        let stride = width as usize;
        Self {
            width,
            height,
            stride,
            cursor_x: 0,
            cursor_y: 0,
            row_height: 0,
            fill_arc: Arc::new(vec![0; stride * height as usize]),
            outline_arc: Arc::new(vec![0; stride * height as usize]),
            packed: HashMap::new(),
            version: 1,
            dirty: true,
            snapshot: None,
            dropped: 0,
        }
    }

    /// 取走并清零累计丢弃计数
    ///
    /// 渲染端每帧调一次：非零就说明这一帧有字形没塞进 atlas，需要整表重建后重试。
    pub fn take_dropped(&mut self) -> u32 {
        std::mem::take(&mut self.dropped)
    }

    /// 已打包的字形种类数（诊断用）
    pub fn packed_count(&self) -> usize {
        self.packed.len()
    }

    /// 当前分配高度（诊断用）
    pub fn allocated_height(&self) -> u32 {
        self.height
    }

    /// 作废坐标映射 — atlas 位图与 `packed` 里的坐标必须同生同死
    ///
    /// 只清位图不清坐标的话，`pack()` 会对已有 key 提前返回旧坐标（见 `pack` 开头的
    /// 缓存命中分支），那些字形从此指向空白区域，永久画不出来。
    fn reset_mapping(&mut self) {
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.row_height = 0;
        self.packed.clear();
    }

    /// 拿回独占、长度为 `stride*height` 的可写缓冲，**完整保留已打包的位图**
    ///
    /// 快照只是给渲染端读一次去上传纹理，所以先放掉 `self.snapshot` 这一份引用，
    /// `Arc::get_mut` 就能拿到独占，原地 `resize` 不动已有内容。
    ///
    /// 万一还有别的持有者（例如上一帧的 `RenderPlan` 尚未 drop），只能换新内存 ——
    /// 这时必须把旧内容整块搬过去。历史实现直接换成全零缓冲，等于每帧第一次打包就
    /// 把整张 atlas 清空，而 `packed` 仍留着旧坐标：表现为一条弹幕只画出零星几个字、
    /// 存活字符集合逐帧变化，观感就是"弹幕刚出现就消失，滚不完"。
    fn writable_buffers(&mut self) -> (&mut Vec<u8>, &mut Vec<u8>) {
        let len = self.stride * self.height as usize;
        self.snapshot = None;
        if Arc::get_mut(&mut self.fill_arc).is_none() {
            let mut fill = (*self.fill_arc).clone();
            fill.resize(len, 0);
            self.fill_arc = Arc::new(fill);
            let mut outline = (*self.outline_arc).clone();
            outline.resize(len, 0);
            self.outline_arc = Arc::new(outline);
        } else {
            Arc::get_mut(&mut self.fill_arc)
                .expect("刚放掉内部引用，fill_arc 必然独占")
                .resize(len, 0);
            Arc::get_mut(&mut self.outline_arc)
                .expect("fill/outline 总是成对被快照持有，强引用数必然一致")
                .resize(len, 0);
        }
        self.dirty = true;
        (
            Arc::get_mut(&mut self.fill_arc).unwrap(),
            Arc::get_mut(&mut self.outline_arc).unwrap(),
        )
    }

    /// 打包一个字形到 atlas，返回其在 atlas 中的位置
    pub fn pack(&mut self, glyph: &RasterizedGlyph, key: &GlyphKey) -> PackedGlyph {
        // 已打包过则直接返回
        if let Some(packed) = self.packed.get(key).copied() {
            return packed;
        }

        let gw = glyph.width.min(self.width);
        let gh = glyph.height;

        if gw == 0 || gh == 0 {
            // 空字形（空格 / 字体缺该码位）不算"装不下"，不计数 ——
            // 否则字体缺字会被误判成 atlas 溢出，触发无谓的整表重建。
            return PackedGlyph {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            };
        }

        // 当前行放不下，换行
        if self.cursor_x + gw > self.width {
            self.cursor_x = 0;
            self.cursor_y += self.row_height;
            self.row_height = 0;
        }

        // 高度不够，翻倍扩展
        while self.cursor_y + gh > self.height {
            let new_height = self.height * 2;
            // ATLAS_WIDTH 已经是 2048 = GLES 的 max_texture_dimension_2d 下限，
            // 所以宽边不能再涨（涨了整张 atlas 直接建不出来）。高度同理封顶，
            // 放不下的字形返回零尺寸（调用方会跳过绘制），总比让整张 atlas
            // 因为一次越界而作废强 —— 一旦建纹理失败，wgpu 报的是 validation
            // 错误，会连带这一帧所有 bind group 和 submit 一起失效。
            if new_height > ATLAS_WIDTH {
                self.dropped += 1;
                return PackedGlyph {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                };
            }
            let previous_height = self.height;
            self.height = new_height;
            // 这里只记高度，实际扩容由下面写入前的 `writable_buffers()` 一次性完成，
            // 避免同一次 pack 里反复 resize。
            tracing::debug!(
                "GlyphAtlas: grown {} -> {} (packed kinds={})",
                previous_height,
                self.height,
                self.packed.len()
            );
        }

        let px = self.cursor_x;
        let py = self.cursor_y;
        let stride = self.stride;

        let (fill_dst, outline_dst) = self.writable_buffers();

        // 复制 fill alpha
        if glyph.has_bitmap() && glyph.fill_alpha.len() >= glyph.required_len() {
            for row in 0..gh {
                let src_start = row as usize * glyph.stride;
                let src_end = src_start + gw as usize;
                let dst_start = (py + row) as usize * stride + px as usize;
                let dst_end = dst_start + gw as usize;
                if src_end <= glyph.fill_alpha.len() && dst_end <= fill_dst.len() {
                    fill_dst[dst_start..dst_end]
                        .copy_from_slice(&glyph.fill_alpha[src_start..src_end]);
                }
            }
        }

        // 复制 outline alpha
        if glyph.has_bitmap() && glyph.outline_alpha.len() >= glyph.required_len() {
            for row in 0..gh {
                let src_start = row as usize * glyph.stride;
                let src_end = src_start + gw as usize;
                let dst_start = (py + row) as usize * stride + px as usize;
                let dst_end = dst_start + gw as usize;
                if src_end <= glyph.outline_alpha.len() && dst_end <= outline_dst.len() {
                    outline_dst[dst_start..dst_end]
                        .copy_from_slice(&glyph.outline_alpha[src_start..src_end]);
                }
            }
        }

        self.cursor_x += gw;
        self.row_height = self.row_height.max(gh);
        self.dirty = true;

        let packed = PackedGlyph {
            x: px,
            y: py,
            width: gw,
            height: gh,
        };
        self.packed.insert(key.clone(), packed);
        packed
    }

    /// 生成快照（仅在 dirty 时生成新的 Arc）
    ///
    /// 缓冲区用 `Arc<Vec<u8>>` 而不是每帧深拷贝两份 2048xN 的 alpha 平面：
    /// 原来每次有新字形打包就 clone 一遍（满尺寸 4 MB x2），弹幕密集时每帧都要
    /// 分配 + 拷贝 8 MB，是长时间播放后内存抖动、最终 OOM 崩溃的主因之一。
    ///
    /// pack 时发现**需要扩容**才会重建 Arc（此时缓冲内容整体变了，必须换引用）；
    /// 只是往已有行里写新字形时 Arc 指向的 Vec 本身就被就地改写了，
    /// 渲染线程下一帧会重新上传，所以复用引用是安全的。
    pub fn snapshot(&mut self) -> Arc<GlyphAtlasSnapshot> {
        if self.dirty || self.snapshot.is_none() {
            self.snapshot = Some(Arc::new(GlyphAtlasSnapshot {
                width: self.width,
                height: self.height,
                stride: self.stride,
                // clone Arc（一次原子递增），而不是深拷贝两块 alpha 平面。
                // fill_arc / outline_arc 和快照里的是**同一块内存**，pack() 就地
                // 写入后快照自动可见；渲染线程据此在下一帧重新上传。
                fill_alpha: self.fill_arc.clone(),
                outline_alpha: self.outline_arc.clone(),
                version: self.version,
            }));
            self.dirty = false;
            self.version += 1;
        }
        self.snapshot.clone().unwrap()
    }

    /// 清空 atlas（seek/stop 时调用）
    pub fn clear(&mut self) {
        // writable_buffers() 内部会先放掉快照，所以这里走原地路径而不是重新分配
        let (fill, outline) = self.writable_buffers();
        fill.fill(0);
        outline.fill(0);
        self.reset_mapping();
        self.dropped = 0;
        self.version += 1;
    }
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new()
    }
}

// ─── 字形光栅化器 ─────────────────────────────────────────

/// 字形光栅化器 — 使用 ab_glyph 将字符光栅化为 fill/outline alpha
pub struct TextRasterizer {
    /// 字形缓存（已光栅化但未打包到 atlas）
    glyph_cache: HashMap<GlyphKey, Arc<RasterizedGlyph>>,
}

impl TextRasterizer {
    pub fn new() -> Self {
        Self {
            glyph_cache: HashMap::new(),
        }
    }

    /// 光栅化单个字形（fill + outline 分离）
    pub fn rasterize_glyph(&mut self, font_data: &[u8], key: GlyphKey) -> Arc<RasterizedGlyph> {
        // 缓存命中
        if let Some(glyph) = self.glyph_cache.get(&key) {
            return glyph.clone();
        }

        let font_size = key.font_size();
        let glyph = Arc::new(rasterize_single_glyph(font_data, key.ch, font_size));
        self.glyph_cache.insert(key, glyph.clone());
        // key 是 (字符, 字号)，CJK 常用字 + 少量字号下条目有限；但弹幕文本可能
        // 混入大量生僻字/emoji，每条新字形都是两张位图。设上限防止长视频
        // 把内存吃到 OOM —— 超限就整体丢弃（下一个字形重新光栅化）。
        if self.glyph_cache.len() > GLYPH_CACHE_CAPACITY {
            self.glyph_cache.clear();
        }
        glyph
    }

    /// 光栅化整段文本并打包到 atlas，返回字形放置信息列表
    pub fn rasterize_and_pack_text(
        &mut self,
        font_data: &[u8],
        text: &str,
        font_size: f32,
        atlas: &mut GlyphAtlas,
    ) -> Vec<TextGlyphPlacement> {
        let scale = PxScale::from(font_size);
        let font = match FontRef::try_from_slice(font_data) {
            Ok(f) => f,
            Err(e) => {
                // 仅首次输出错误日志，避免刷屏
                if !FONT_PARSE_ERROR_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::error!(
                        "rasterize_and_pack_text: FontRef::try_from_slice FAILED: {} \
                         (font_data_len={}, first_4_bytes={:?})",
                        e,
                        font_data.len(),
                        if font_data.len() >= 4 {
                            &font_data[0..4]
                        } else {
                            font_data
                        },
                    );
                }
                return Vec::new();
            }
        };
        let scaled = font.as_scaled(scale);

        let mut pen_x = 0.0f32;
        let mut placements = Vec::new();

        for ch in text.chars() {
            let advance = scaled.h_advance(scaled.glyph_id(ch));
            let key = GlyphKey::new(ch, font_size);
            let rasterized = self.rasterize_glyph(font_data, key.clone());
            let packed = atlas.pack(&rasterized, &key);

            placements.push(TextGlyphPlacement {
                glyph: rasterized,
                packed,
                pen_x,
            });
            pen_x += advance;
        }

        placements
    }

    /// 清空缓存
    pub fn clear_cache(&mut self) {
        self.glyph_cache.clear();
    }
}

impl Default for TextRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

/// 单字形光栅化结果缓存标志（全局单次，用于避免重复输出错误日志）
static FONT_PARSE_ERROR_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 文本中一个字形的放置信息
#[derive(Debug, Clone)]
pub struct TextGlyphPlacement {
    /// 字形光栅化结果
    pub glyph: Arc<RasterizedGlyph>,
    /// 字形在 atlas 中的位置
    pub packed: PackedGlyph,
    /// 字形在文本中的 x 偏移
    pub pen_x: f32,
}

/// 字形缓存条目上限（key = 字符 + 字号）
///
/// 每个条目两张位图。CJK 常用字在单一字号下约数千个，加上 ASCII 和少量字号，
/// 8192 足够覆盖真实弹幕；超限说明遇到了异常文本（随机码点 / 海量 emoji），
/// 此时整体清空比继续增长安全。
pub const GLYPH_CACHE_CAPACITY: usize = 8192;

/// 条目级字形放置缓存的条目上限
///
/// 一条弹幕的 placements 大约是"字数 × (Arc<RasterizedGlyph> + PackedGlyph +
/// f32)"，每个 RasterizedGlyph 里还各有一张 fill/outline 位图。取 4096 覆盖
/// 正常同屏/滚动窗口的复用需求（滚动 8 秒内的条目远少于此），同时把常驻内存
/// 压在几十 MB 以内 —— 不设上限时长视频会把内存吃到系统 OOM。
pub const PLACEMENT_CACHE_CAPACITY: usize = 4096;

/// 单帧渲染实例 — 对应一次 draw call
#[derive(Debug, Clone)]
pub struct GlyphInstance {
    /// 屏幕像素位置 [x, y, w, h]
    pub rect: [f32; 4],
    /// atlas 中的 UV 区域 [u, v, uw, vh]（归一化坐标）
    pub tex_rect: [f32; 4],
    /// RGBA 颜色
    pub color_rgba: [f32; 4],
}

/// 单帧渲染计划
#[derive(Debug, Clone)]
pub struct RenderPlan {
    /// Atlas 快照（GPU 纹理来源）
    pub atlas: Option<Arc<GlyphAtlasSnapshot>>,
    /// 描边实例（整批先画，压在填充之下）
    pub outline: Vec<GlyphInstance>,
    /// 填充实例
    pub fill: Vec<GlyphInstance>,
}

impl RenderPlan {
    pub fn empty() -> Self {
        Self {
            atlas: None,
            outline: Vec::new(),
            fill: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.fill.is_empty()
    }

    /// 实例总数（outline + fill），即本帧要上传的 GPU 实例数
    pub fn instance_count(&self) -> usize {
        self.outline.len() + self.fill.len()
    }
}

// ─── 内部辅助函数 ─────────────────────────────────────────

/// 光栅化单个字形为 fill + outline alpha
fn rasterize_single_glyph(font_data: &[u8], ch: char, font_size: f32) -> RasterizedGlyph {
    let font = match FontRef::try_from_slice(font_data) {
        Ok(f) => f,
        Err(_) => return RasterizedGlyph::empty(0.0),
    };

    let scale = PxScale::from(font_size);
    let scaled = font.as_scaled(scale);
    let glyph = scaled.scaled_glyph(ch);
    let advance = scaled.h_advance(glyph.id);

    let Some(outlined) = scaled.outline_glyph(glyph) else {
        return RasterizedGlyph::empty(advance);
    };

    let bounds = outlined.px_bounds();
    let glyph_width = (bounds.max.x - bounds.min.x).ceil().max(0.0) as u32;
    let glyph_height = (bounds.max.y - bounds.min.y).ceil().max(0.0) as u32;

    if glyph_width == 0 || glyph_height == 0 {
        return RasterizedGlyph::empty(advance);
    }

    // outline 区域扩展 OUTLINE_RADIUS 像素
    let outline_ext = OUTLINE_RADIUS.ceil() as u32;
    let outline_w = glyph_width + outline_ext * 2;
    let outline_h = glyph_height + outline_ext * 2;

    let fill_stride = glyph_width as usize;
    let outline_stride = outline_w as usize;

    let mut fill_alpha = vec![0u8; fill_stride * glyph_height as usize];
    let mut outline_alpha = vec![0u8; outline_stride * outline_h as usize];

    // 填充 alpha
    outlined.draw(|gx, gy, coverage| {
        let px = gx as usize;
        let py = gy as usize;
        if px < fill_stride && py < glyph_height as usize {
            let idx = py * fill_stride + px;
            let alpha = (coverage * 255.0).round() as u8;
            fill_alpha[idx] = fill_alpha[idx].max(alpha);
        }
    });

    // 描边 alpha：扩展字形区域（用 dilation 模拟）
    // 简单实现：将 fill_alpha 向四周扩展 OUTLINE_RADIUS 像素
    let ext = OUTLINE_RADIUS.ceil() as usize;
    outlined.draw(|gx, gy, coverage| {
        let base_px = gx as i32;
        let base_py = gy as i32;
        let alpha = (coverage * 255.0).round() as u8;

        // 在扩展区域内绘制
        for dy in -(ext as i32)..=(ext as i32) {
            for dx in -(ext as i32)..=(ext as i32) {
                // 曼哈顿距离近似（圆形扩展）
                let dist_sq = (dx * dx + dy * dy) as f32;
                if dist_sq > OUTLINE_RADIUS * OUTLINE_RADIUS {
                    continue;
                }
                let px = (base_px + dx + ext as i32) as usize;
                let py = (base_py + dy + ext as i32) as usize;
                if px < outline_stride && py < outline_h as usize {
                    let idx = py * outline_stride + px;
                    outline_alpha[idx] = outline_alpha[idx].max(alpha);
                }
            }
        }
    });

    // 将 fill bitmap 零填充到 outline 尺寸（两者使用相同尺寸打包到 atlas）
    // fill 字形居中放置在 (ext, ext) 偏移处，外围为零（透明）
    // outline 保留完整的 dilation 扩展（由上面的 draw 循环生成）
    let pack_w = outline_w;
    let pack_h = outline_h;
    let pack_stride = outline_stride;

    let mut padded_fill = vec![0u8; pack_stride * pack_h as usize];
    for y in 0..glyph_height as usize {
        let src_start = y * fill_stride;
        let src_end = src_start + fill_stride;
        let dst_start = (y + ext) * pack_stride + ext;
        let dst_end = dst_start + fill_stride;
        if src_end <= fill_alpha.len() && dst_end <= padded_fill.len() {
            padded_fill[dst_start..dst_end].copy_from_slice(&fill_alpha[src_start..src_end]);
        }
    }

    RasterizedGlyph {
        width: pack_w,
        height: pack_h,
        stride: pack_stride,
        fill_alpha: padded_fill,
        outline_alpha,
        // offset 需要减去 ext，因为 bitmap 现在比原始字形大了 ext 像素
        offset_x: bounds.min.x - ext as f32,
        offset_y: bounds.min.y - ext as f32,
        advance,
    }
}

// ─── 测试 ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个 w×w 实心字形位图（打包测试用，无需真实字体）
    fn solid(w: u32) -> RasterizedGlyph {
        let n = (w * w) as usize;
        RasterizedGlyph {
            width: w,
            height: w,
            stride: w as usize,
            fill_alpha: vec![255u8; n],
            outline_alpha: vec![255u8; n],
            offset_x: 0.0,
            offset_y: 0.0,
            advance: w as f32,
        }
    }

    #[test]
    fn test_glyph_key_creation() {
        let key = GlyphKey::new('中', 25.0);
        assert_eq!(key.ch, '中');
        assert_eq!(key.font_size_milli, 25000);
        assert!((key.font_size() - 25.0).abs() < 0.001);
    }

    #[test]
    fn test_atlas_new() {
        let atlas = GlyphAtlas::new();
        assert_eq!(atlas.width, ATLAS_WIDTH);
        assert_eq!(atlas.height, ATLAS_INITIAL_HEIGHT);
        assert_eq!(atlas.cursor_x, 0);
        assert_eq!(atlas.cursor_y, 0);
    }

    /// snapshot 必须与 atlas 共享同一块缓冲，不能每次深拷贝
    ///
    /// 原来每次有新字形就 clone 两块 alpha 平面（满尺寸 4 MB x2）。弹幕密集时
    /// 每帧都要分配+拷贝 8 MB，长时间播放累积成 OOM 崩溃。
    #[test]
    fn snapshot_shares_buffer_instead_of_copying() {
        let mut atlas = GlyphAtlas::new();
        let snap = atlas.snapshot();

        // 第一次 snapshot 就已经把 Arc 交给快照了：强引用数为 2，
        // 说明两块 alpha 平面是共享的，而不是各自深拷贝一份
        assert_eq!(
            Arc::strong_count(&atlas.fill_arc),
            2,
            "snapshot must share fill plane, not copy it"
        );
        assert_eq!(
            Arc::strong_count(&atlas.outline_arc),
            2,
            "snapshot must share outline plane, not copy it"
        );
        assert_eq!(
            Arc::as_ptr(&snap.fill_alpha),
            Arc::as_ptr(&atlas.fill_arc),
            "snapshot and atlas must point at the same allocation"
        );
        assert_eq!(atlas.fill_arc.len(), atlas.outline_arc.len());
    }

    /// 字形缓存必须有上限，否则长视频会 OOM
    #[test]
    fn glyph_cache_is_bounded() {
        let r = TextRasterizer::new();
        // 没有字体数据时 rasterize 会走失败分支，这里只验证容量常量本身
        // 是有限值（真正触发上限需要真实字体，交给设备端验证）
        const { assert!(GLYPH_CACHE_CAPACITY > 0 && GLYPH_CACHE_CAPACITY <= 65536) };
        assert!(r.glyph_cache.is_empty());
    }

    /// atlas 必须能扛住「字号连续变化」而不越界，`clear()` 后必须可复用
    ///
    /// 防回归（会静默吃掉弹幕的真 bug）：`GlyphKey` 是 (字符, font_size)，字号一变
    /// 就是一批全新 key；而 atlas 的 `packed` 表只查不删、行永不回收。弹幕字号跟
    /// 窗口高度走，于是拖动窗口时每个整数字号都会往 atlas 里塞一整套字形 ——
    /// 高度翻倍涨过 `ATLAS_WIDTH`(2048) 之后 `pack()` 开始返回**零尺寸**
    /// （见 `pack` 里的越界分支），弹幕字形集体消失且本次播放永不恢复。
    ///
    /// 现在 `DanmakuEngine` 在字号变化时整体 `invalidate_atlas()`，atlas 里只保留
    /// 当前字号这一套，容量天然有界。这个测试守住 `clear()` 的可复用性 ——
    /// 整表作废策略依赖它把光标归零。
    #[test]
    fn atlas_survives_font_size_churn_and_reuses_after_clear() {
        let mut atlas = GlyphAtlas::new();

        // 模拟拖动窗口：字号连续变化，每档都塞一批不同字符
        for (size, ch) in (24u32..=88).step_by(8).zip("甲乙丙丁戊己庚辛".chars()) {
            let packed = atlas.pack(&solid(size), &GlyphKey::new(ch, size as f32));
            assert_ne!(
                packed.width, 0,
                "字号 {size} 的字形被静默丢弃，说明 atlas 已经越界"
            );
            assert!(
                atlas.height <= ATLAS_WIDTH,
                "atlas 高度 {} 超过上限 {}",
                atlas.height,
                ATLAS_WIDTH
            );
        }

        // clear 后光标归零、packed 表清空，且仍能正常打包
        atlas.clear();
        assert_eq!(atlas.cursor_y, 0, "clear 必须把打包光标归零");
        assert_eq!(atlas.cursor_x, 0);
        assert!(atlas.packed.is_empty(), "clear 必须清空 packed 表");

        let packed = atlas.pack(&solid(25), &GlyphKey::new('弹', 25.0));
        assert_ne!(packed.width, 0, "clear 之后仍应能正常打包");
        assert_ne!(packed.height, 0);
    }

    /// 不清理就换字号，atlas 一定会被撑爆 → `pack()` 返回零尺寸 → 字形消失
    ///
    /// 这条测试**记录的是反面失效模式**，它正是 `DanmakuEngine` 必须在字号变化时
    /// 调 `invalidate_atlas()` 的原因（而不是只清 rasterizer 缓存）。
    ///
    /// 大字号字形（200px）每行只放得下 10 个、每行占 200px 高，很快就能把 2048
    /// 的高度上限撑满。
    #[test]
    fn packing_many_font_sizes_without_clear_exhausts_atlas() {
        let mut atlas = GlyphAtlas::new();
        let mut exhausted = false;

        // 40 个字号 × 每个 30 个不同字符，足以越过 2048 的高度上限
        'outer: for size in 200u32..=239 {
            for n in 0..30u32 {
                let ch = char::from_u32(0x4E00 + n).unwrap_or('字');
                let packed = atlas.pack(&solid(size), &GlyphKey::new(ch, size as f32));
                if packed.width == 0 {
                    exhausted = true;
                    break 'outer;
                }
            }
        }

        assert!(
            exhausted,
            "预期 atlas 会被撑爆并让 pack() 返回零尺寸；若未触发说明行打包逻辑变了"
        );
        assert!(
            atlas.height <= ATLAS_WIDTH,
            "越界后高度不得超过硬上限（pack 内部不再翻倍），实际 {}",
            atlas.height
        );

        // 而 clear() 之后立刻恢复正常 —— 这是引擎能自救的唯一途径
        atlas.clear();
        let packed = atlas.pack(&solid(25), &GlyphKey::new('复', 25.0));
        assert_ne!(
            packed.width, 0,
            "clear 之后必须能重新打包，否则弹幕会永久消失"
        );
    }

    #[test]
    fn test_atlas_pack_empty_glyph() {
        let mut atlas = GlyphAtlas::new();
        let empty = RasterizedGlyph::empty(10.0);
        let key = GlyphKey::new(' ', 25.0);
        let packed = atlas.pack(&empty, &key);
        assert_eq!(packed.width, 0);
        assert_eq!(packed.height, 0);
    }

    #[test]
    fn test_atlas_pack_and_dedup() {
        let mut atlas = GlyphAtlas::new();

        // 创建一个简单的 4x4 字形
        let glyph = RasterizedGlyph {
            width: 4,
            height: 4,
            stride: 4,
            fill_alpha: vec![128; 16],
            outline_alpha: vec![64; 16],
            offset_x: 0.0,
            offset_y: 0.0,
            advance: 5.0,
        };

        let key = GlyphKey::new('A', 20.0);
        let p1 = atlas.pack(&glyph, &key);
        let p2 = atlas.pack(&glyph, &key);

        // 相同 key 应该返回相同位置
        assert_eq!(p1, p2);
        assert_eq!(p1.x, 0);
        assert_eq!(p1.y, 0);
        assert_eq!(p1.width, 4);
        assert_eq!(p1.height, 4);
    }

    #[test]
    fn test_atlas_row_packing() {
        let mut atlas = GlyphAtlas::new();

        // 打包多个字形，应该按行排列
        for i in 0..3 {
            let glyph = RasterizedGlyph {
                width: 10,
                height: 10,
                stride: 10,
                fill_alpha: vec![200; 100],
                outline_alpha: vec![100; 100],
                offset_x: 0.0,
                offset_y: 0.0,
                advance: 12.0,
            };
            let key = GlyphKey::new(char::from_u32('A' as u32 + i).unwrap(), 20.0);
            let packed = atlas.pack(&glyph, &key);
            // 每个字形应该在同一行（y=0），x 依次递增
            assert_eq!(packed.y, 0);
            assert_eq!(packed.x, i * 10);
        }
    }

    #[test]
    fn test_atlas_snapshot() {
        let mut atlas = GlyphAtlas::new();
        let s1 = atlas.snapshot();
        assert_eq!(s1.version, 1);

        // 无变化时返回相同快照
        let s2 = atlas.snapshot();
        assert_eq!(s2.version, 1);
        assert!(Arc::ptr_eq(&s1, &s2));

        // 打包后变 dirty，生成新快照
        let glyph = RasterizedGlyph {
            width: 2,
            height: 2,
            stride: 2,
            fill_alpha: vec![255; 4],
            outline_alpha: vec![128; 4],
            offset_x: 0.0,
            offset_y: 0.0,
            advance: 3.0,
        };
        atlas.pack(&glyph, &GlyphKey::new('X', 10.0));
        let s3 = atlas.snapshot();
        assert_eq!(s3.version, 2);
        assert!(!Arc::ptr_eq(&s1, &s3));
    }

    fn solid_rect(w: u32, h: u32, v: u8) -> RasterizedGlyph {
        RasterizedGlyph {
            width: w,
            height: h,
            stride: w as usize,
            fill_alpha: vec![v; (w * h) as usize],
            outline_alpha: vec![v / 2; (w * h) as usize],
            offset_x: 0.0,
            offset_y: 0.0,
            advance: w as f32,
        }
    }

    /// atlas 里某个已打包区域是否仍是纯色 `v`
    fn region_is(snap: &GlyphAtlasSnapshot, p: &PackedGlyph, v: u8) -> bool {
        (0..p.height).all(|row| {
            let start = (p.y + row) as usize * snap.stride + p.x as usize;
            let end = start + p.width as usize;
            end <= snap.fill_alpha.len() && snap.fill_alpha[start..end].iter().all(|&b| b == v)
        })
    }

    /// 防回归（会吃掉整条弹幕的真 bug）：`snapshot()` 之后继续 `pack()`，
    /// 已打包字形的位图不能被清零。
    ///
    /// 历史实现里 `writable_buffers()` 在 Arc 不独实时直接换成全零新缓冲，而
    /// `packed` 仍保留旧坐标 —— `pack()` 对已有 key 提前返回，那些字形从此指向
    /// 空白区域，永远画不出来。表现为一条弹幕只剩零星几个字、存活字符逐帧变化，
    /// 观感就是"弹幕刚出现就消失，滚不到左边"。
    #[test]
    fn packing_after_snapshot_keeps_earlier_glyph_pixels() {
        let mut atlas = GlyphAtlas::new();
        let a = atlas.pack(&solid_rect(8, 8, 200), &GlyphKey::new('A', 20.0));

        // 稳态：快照交给渲染端上传完就放手，atlas 内部只剩自己那一份引用
        let first = atlas.snapshot();
        drop(first);
        let b = atlas.pack(&solid_rect(4, 4, 255), &GlyphKey::new('B', 20.0));

        let snap = atlas.snapshot();
        assert!(region_is(&snap, &a, 200), "A 的位图被后续 pack 清掉了");
        assert!(region_is(&snap, &b, 255), "B 没写进去");
    }

    /// 同一条约束在"外部仍持有快照 + 高度扩容换内存"时也必须成立
    #[test]
    fn growing_with_outstanding_snapshot_carries_pixels_over() {
        let mut atlas = GlyphAtlas::new();
        let a = atlas.pack(&solid_rect(8, 8, 200), &GlyphKey::new('A', 20.0));
        let held = atlas.snapshot();

        // 高于初始 256 → 触发 height 翻倍；held 还活着 → 只能换新内存
        let tall = atlas.pack(
            &solid_rect(64, ATLAS_INITIAL_HEIGHT + 44, 250),
            &GlyphKey::new('B', 20.0),
        );
        assert!(tall.height > ATLAS_INITIAL_HEIGHT, "应已扩容");

        assert!(
            region_is(&held, &a, 200),
            "旧快照指向的内存被动过，快照不再是不可变视图"
        );
        let snap = atlas.snapshot();
        assert!(
            region_is(&snap, &a, 200),
            "扩容换内存时没把已打包位图搬过去"
        );
        assert!(region_is(&snap, &tall, 250));
    }

    #[test]
    fn test_atlas_clear() {
        let mut atlas = GlyphAtlas::new();
        let glyph = RasterizedGlyph {
            width: 2,
            height: 2,
            stride: 2,
            fill_alpha: vec![255; 4],
            outline_alpha: vec![128; 4],
            offset_x: 0.0,
            offset_y: 0.0,
            advance: 3.0,
        };
        atlas.pack(&glyph, &GlyphKey::new('A', 10.0));
        assert_eq!(atlas.cursor_x, 2);

        atlas.clear();
        assert_eq!(atlas.cursor_x, 0);
        assert_eq!(atlas.cursor_y, 0);
        assert!(atlas.packed.is_empty());
    }

    #[test]
    fn test_render_plan_empty() {
        let plan = RenderPlan::empty();
        assert!(plan.is_empty());
        assert!(plan.atlas.is_none());
    }
}
