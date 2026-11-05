//! 字幕字体解析 —— 把 ASS 样式里的 `Fontname` 落到系统里的具体 face
//!
//! 引擎原本只有一份 [`crate::font::load_system_font`] 挑出来的内置回退字体，
//! 样式表里的 `Fontname`（"微软雅黑"、"Source Han Sans CN Medium"…）解析出来却
//! 没人消费。这里补上名称查找：系统字体索引 + face 缓存 + 逐字覆盖兜底。
//!
//! 索引构建是**惰性**的：只有真的出现带 `Fontname` 的样式才会扫系统字体目录。
//! engine 的加载线程会调 [`FontLibrary::prepare_system_fonts`] 预热，避免第一次
//! 光栅化时在渲染线程上花几百毫秒建索引。

use std::collections::{HashMap, VecDeque};

use ab_glyph::{Font, FontArc, FontVec};
use dyl_render::font_util::{extract_ttc_face, extract_ttc_preferred, CJK_SC_PREFER};

/// face 缓存上限：ASS 脚本通常只用 2-3 个 `Fontname`，而 CJK 字体一个 face 就有
/// 十几 MB，必须有界。超出按 FIFO 淘汰（仍在用的下一帧会重新解析）。
const MAX_CACHED_FACES: usize = 8;

/// 名称匹配用的 face 视图（可脱离字体文件单测）
///
/// `families` 是一个 face 的全部 family 名条目（英文名 + 各语言本地化名），
/// `postscript` 是 PostScript 名 —— ASS 的 `Fontname` 三种写法都可能出现。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FaceDesc<'a, I> {
    pub id: I,
    pub families: &'a [&'a str],
    pub postscript: &'a str,
    /// 字重（CSS 数值，400 = Regular）
    pub weight: u16,
    pub italic: bool,
}

/// 从 `Fontname` 拆出的期望：家族名 + 名字里带的字重/斜体
#[derive(Debug, PartialEq)]
pub(crate) struct NameRequest<'a> {
    /// 去掉字重/样式后缀词之后的家族名（原样大小写）
    pub family: &'a str,
    /// 名字里显式写了字重词时才有值
    pub weight: Option<u16>,
    pub italic: bool,
}

/// 名字里表示字重/样式的后缀词
///
/// ASS 作者常把 face 名整段写进 `Fontname`（"Microsoft YaHei Bold"），而字体索引里
/// family 只到 "Microsoft YaHei"、字重另有一列 —— 不对齐就全都命中不了。
const WEIGHT_WORDS: &[(&str, u16)] = &[
    ("thin", 100),
    ("hairline", 100),
    ("ultralight", 200),
    ("extralight", 200),
    ("light", 300),
    ("semilight", 350),
    ("book", 380),
    ("regular", 400),
    ("roman", 400),
    ("normal", 400),
    ("medium", 500),
    ("semibold", 600),
    ("demibold", 600),
    ("bold", 700),
    ("extrabold", 800),
    ("ultrabold", 800),
    ("black", 900),
    ("heavy", 900),
];

/// 比较用的归一化：小写 + 去掉空格/连字符/下划线
///
/// "MS Gothic" 与 "MSGothic"、"Source Han Sans CN" 与 "source-han-sans-cn" 在脚本
/// 与字体里的写法经常不一致，归一化之后才谈得上等值匹配。
fn norm(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .collect()
}

/// 拆出家族名与名字尾部带的字重/斜体词
pub(crate) fn parse_fontname(name: &str) -> NameRequest<'_> {
    let mut family = name.trim();
    let mut weight = None;
    let mut italic = false;
    while let Some(pos) = family.rfind(char::is_whitespace) {
        let (head, tail) = family.split_at(pos);
        let head = head.trim_end();
        if head.is_empty() {
            // 家族名本身就叫 "Bold Foo" 这类情况：剥空了就不剥
            break;
        }
        let tail = tail.trim().to_lowercase();
        if tail == "italic" || tail == "oblique" {
            italic = true;
            family = head;
            continue;
        }
        if let Some((_, w)) = WEIGHT_WORDS.iter().find(|(word, _)| *word == tail) {
            weight = Some(*w);
            family = head;
            continue;
        }
        break;
    }
    NameRequest {
        family,
        weight,
        italic,
    }
}

/// 贴合度：字重越接近越好，其次不要意外的斜体，同等距离时偏向更细的那一个
fn score<I>(f: &FaceDesc<'_, I>, req: &NameRequest<'_>) -> (i32, u8, i32) {
    let target = req.weight.unwrap_or(400) as i32;
    let gap = f.weight as i32 - target;
    let italic_penalty = if f.italic == req.italic { 0 } else { 1 };
    (gap.abs(), italic_penalty, gap.max(0))
}

/// 在候选里挑最贴合请求的 face
fn select<I: Clone>(hits: &[&FaceDesc<'_, I>], req: &NameRequest<'_>) -> Option<I> {
    let mut best: Option<&FaceDesc<'_, I>> = None;
    for candidate in hits {
        match best {
            Some(current) if score(*candidate, req) >= score(current, req) => {}
            _ => best = Some(*candidate),
        }
    }
    best.map(|f| f.id.clone())
}

/// 名称 → face 的匹配主逻辑
///
/// 对「完整名」与「剥掉字重词的家族名」各做一轮 等值 → 前缀/被前缀 → 包含，
/// 包含那一轮同时看 PostScript 名。全部落空返回 `None`，由调用方兜底。
pub(crate) fn pick_face<I: Clone>(faces: &[FaceDesc<'_, I>], want: &str) -> Option<(I, String)> {
    let raw = parse_fontname(want);
    // 名字里没写字重时，"Foo Bold" 那样的 face 不该压过 "Foo"
    let req = NameRequest {
        family: raw.family,
        weight: Some(raw.weight.unwrap_or(400)),
        italic: raw.italic,
    };
    let want_norm = norm(want);
    let family_norm = norm(raw.family);

    for key in [want_norm.as_str(), family_norm.as_str()] {
        if key.is_empty() {
            continue;
        }
        let exact: Vec<&FaceDesc<'_, I>> = faces
            .iter()
            .filter(|f| f.families.iter().any(|name| norm(name) == key))
            .collect();
        if let Some(id) = select(&exact, &req) {
            return Some((id, key.to_string()));
        }

        let prefixed: Vec<&FaceDesc<'_, I>> = faces
            .iter()
            .filter(|f| {
                f.families
                    .iter()
                    .any(|name| norm(name).starts_with(key) || key.starts_with(&norm(name)))
            })
            .collect();
        if let Some(id) = select(&prefixed, &req) {
            return Some((id, key.to_string()));
        }

        let contains: Vec<&FaceDesc<'_, I>> = faces
            .iter()
            .filter(|f| {
                f.families
                    .iter()
                    .any(|name| norm(name).contains(key) || key.contains(&norm(name)))
                    || norm(f.postscript).contains(key)
            })
            .collect();
        if let Some(id) = select(&contains, &req) {
            return Some((id, key.to_string()));
        }
    }
    None
}

/// 系统字体索引 + `Fontname` → [`FontArc`] 缓存
#[derive(Default)]
pub(crate) struct FontLibrary {
    /// 内置回退字体（`set_font` 灌进来的那一份），也是字形覆盖的兜底
    fallback: Option<FontArc>,
    /// 惰性构建的系统字体索引
    db: Option<fontdb::Database>,
    /// `Fontname` → face；未命中也记 `None`，避免每帧重复查找与重复告警
    resolved: HashMap<String, Option<FontArc>>,
    /// `resolved` 的插入顺序（FIFO 淘汰）
    order: VecDeque<String>,
}

impl FontLibrary {
    /// 装入回退字体（TTC 会先按简体中文优先挑一个 face）
    pub fn set_fallback(&mut self, data: Vec<u8>) {
        let owned = match extract_ttc_preferred(&data, CJK_SC_PREFER) {
            Some(face) => {
                tracing::info!(
                    "subtitle font: 回退字体取 TTC face #{} '{}'",
                    face.index,
                    face.family
                );
                face.data
            }
            None => data,
        };
        self.fallback = FontArc::try_from_vec(owned).ok();
        if self.fallback.is_none() {
            tracing::warn!("subtitle font: 回退字体数据无法解析，字幕将没有字体");
        }
    }

    pub fn fallback(&self) -> Option<&FontArc> {
        self.fallback.as_ref()
    }

    /// 构建系统字体索引（已构建则直接返回）
    pub fn prepare_system_fonts(&mut self) {
        if self.db.is_some() {
            return;
        }
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        // fontdb 在 Android 上不扫任何目录（它的 unix 分支排除了 android），显式补上
        for dir in android_font_dirs() {
            if std::path::Path::new(dir).is_dir() {
                db.load_fonts_dir(dir);
            }
        }
        tracing::info!("subtitle font: 系统字体索引 {} 个 face", db.faces().count());
        self.db = Some(db);
    }

    /// `Fontname` → face
    ///
    /// 返回 `None` 表示没找到（空名字也算），调用方用回退字体。
    pub fn face(&mut self, name: &str) -> Option<FontArc> {
        let key = name.trim();
        if key.is_empty() {
            return None;
        }
        if let Some(hit) = self.resolved.get(key) {
            return hit.clone();
        }
        self.prepare_system_fonts();
        let found = self.lookup(key);
        match &found {
            Some(_) => tracing::info!("subtitle font: Fontname {key:?} 命中"),
            None => tracing::info!("subtitle font: Fontname {key:?} 系统里没有，回退内置字体"),
        }
        self.order.push_back(key.to_string());
        self.resolved.insert(key.to_string(), found.clone());
        while self.order.len() > MAX_CACHED_FACES {
            match self.order.pop_front() {
                Some(stale) => {
                    self.resolved.remove(&stale);
                }
                None => break,
            }
        }
        found
    }

    fn lookup(&self, name: &str) -> Option<FontArc> {
        let db = self.db.as_ref()?;
        let rows: Vec<(fontdb::ID, Vec<&str>, &str, u16, bool)> = db
            .faces()
            .map(|f| {
                (
                    f.id,
                    f.families.iter().map(|(n, _)| n.as_str()).collect(),
                    f.post_script_name.as_str(),
                    f.weight.0,
                    matches!(f.style, fontdb::Style::Italic | fontdb::Style::Oblique),
                )
            })
            .collect();
        let descs: Vec<FaceDesc<'_, fontdb::ID>> = rows
            .iter()
            .map(|row| FaceDesc {
                id: row.0,
                families: row.1.as_slice(),
                postscript: row.2,
                weight: row.3,
                italic: row.4,
            })
            .collect();

        let id = match pick_face(&descs, name) {
            Some((id, matched)) => {
                tracing::debug!("subtitle font: {name:?} → 匹配 {matched:?}");
                id
            }
            // 自建匹配全落空时，让 fontdb 按 CSS 语义再试一次（本地化名、通用族名等）
            None => {
                let req = parse_fontname(name);
                let family = match norm(req.family).as_str() {
                    "sansserif" => fontdb::Family::SansSerif,
                    "serif" => fontdb::Family::Serif,
                    "monospace" => fontdb::Family::Monospace,
                    "cursive" => fontdb::Family::Cursive,
                    "fantasy" => fontdb::Family::Fantasy,
                    _ => fontdb::Family::Name(req.family),
                };
                // 刻意不追加通用族兜底：查不到就该回内置 CJK 字体，而不是随便抓一个
                // 拉丁字体 —— 那会让中文整行变成豆腐块（逐字回退救不回来时更糟）。
                db.query(&fontdb::Query {
                    families: &[family],
                    weight: fontdb::Weight(req.weight.unwrap_or(400)),
                    style: if req.italic {
                        fontdb::Style::Italic
                    } else {
                        fontdb::Style::Normal
                    },
                    ..Default::default()
                })?
            }
        };
        db.with_face_data(id, build_face).flatten()
    }
}

/// 打开一个 face 的字节
///
/// `ab_glyph` 只认单字体，所以 TTC 要先按字体索引取那一个 face（不能像回退字体
/// 那样靠 family 关键字猜）。
fn build_face(data: &[u8], index: u32) -> Option<FontArc> {
    if data.starts_with(b"ttcf") {
        if let Some(face) = extract_ttc_face(data, index as usize) {
            if let Ok(arc) = FontArc::try_from_vec(face.data) {
                return Some(arc);
            }
        }
    }
    FontVec::try_from_vec_and_index(data.to_vec(), index)
        .ok()
        .map(FontArc::new)
}

/// face 画得出这个字符吗
///
/// `GlyphId(0)` 是 `.notdef`，也就是豆腐块 —— 样式点了 Arial 却要显示日文时，
/// 逐字回退到内置 CJK 字体比整行变成方块好得多。
pub(crate) fn covers(font: &FontArc, ch: char) -> bool {
    font.glyph_id(ch) != ab_glyph::GlyphId(0)
}

/// Android 上的字体目录（fontdb 的系统扫描在该平台是空操作）
fn android_font_dirs() -> &'static [&'static str] {
    #[cfg(target_os = "android")]
    {
        &["/system/fonts", "/data/fonts"]
    }
    #[cfg(not(target_os = "android"))]
    {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Row<'a> = (usize, &'a [&'a str], &'a str, u16, bool);

    fn descs<'a>(list: &'a [Row<'a>]) -> Vec<FaceDesc<'a, usize>> {
        list.iter()
            .map(|row| FaceDesc {
                id: row.0,
                families: row.1,
                postscript: row.2,
                weight: row.3,
                italic: row.4,
            })
            .collect()
    }

    fn picked(list: &[Row<'_>], want: &str) -> Option<usize> {
        pick_face(&descs(list), want).map(|(id, _)| id)
    }

    #[test]
    fn fontname_strips_weight_and_style_suffixes() {
        let req = parse_fontname("Microsoft YaHei Bold");
        assert_eq!(req.family, "Microsoft YaHei");
        assert_eq!(req.weight, Some(700));
        assert!(!req.italic);

        let req = parse_fontname("Segoe UI Italic");
        assert_eq!(req.family, "Segoe UI");
        assert_eq!(req.weight, None);
        assert!(req.italic);

        let req = parse_fontname("Bold");
        assert_eq!(req.family, "Bold", "只剩一个词时不能把名字剥空");
    }

    #[test]
    fn exact_family_match_wins_regardless_of_case_hyphen_or_language() {
        let list: [Row<'_>; 3] = [
            (0, &["Arial Narrow"], "ArialNarrow", 400, false),
            (1, &["Arial"], "Arial", 400, false),
            (2, &["微软雅黑", "Microsoft YaHei"], "msyh", 400, false),
        ];
        assert_eq!(picked(&list, "Arial"), Some(1));
        assert_eq!(picked(&list, "微软雅黑"), Some(2));
        assert_eq!(
            picked(&list, "MICROSOFT-YA-HEI"),
            Some(2),
            "大小写/连字符/中文名差异不得让查找落空"
        );
    }

    #[test]
    fn weight_suffix_selects_the_matching_face() {
        let list: [Row<'_>; 3] = [
            (0, &["Foo"], "", 400, false),
            (1, &["Foo"], "", 700, false),
            (2, &["Foo"], "", 700, true),
        ];
        assert_eq!(picked(&list, "Foo Bold"), Some(1), "写了 Bold 就该拿 Bold");
        assert_eq!(
            picked(&list, "Foo Bold Italic"),
            Some(2),
            "同时写了 Italic 就该拿斜体"
        );
        assert_eq!(picked(&list, "Foo"), Some(0), "没写字重时不得优先选到 Bold");
    }

    #[test]
    fn substring_and_postscript_names_are_accepted() {
        let list: [Row<'_>; 2] = [
            (
                0,
                &["Source Han Sans CN Medium"],
                "SourceHanSansCN-Medium",
                500,
                false,
            ),
            (1, &["DejaVu Sans"], "DejaVuSans", 400, false),
        ];
        assert_eq!(
            picked(&list, "Source Han Sans CN"),
            Some(0),
            "ASS 常写完整 face 名的前缀"
        );
        assert_eq!(
            picked(&list, "DejaVuSans"),
            Some(1),
            "PostScript 写法也要认得"
        );
        assert_eq!(picked(&list, "NoSuchFamily"), None);
        assert_eq!(picked(&list, ""), None);
    }

    #[test]
    fn face_cache_stays_bounded_and_caches_misses() {
        let mut lib = FontLibrary::default();
        for i in 0..MAX_CACHED_FACES + 4 {
            // 索引里没有这种名字，查找一律落空 —— 但缓存必须有界且不重复告警
            assert!(lib.face(&format!("AbsentFont{i}")).is_none());
        }
        assert!(
            lib.resolved.len() <= MAX_CACHED_FACES,
            "face 缓存必须有界，实际 {}",
            lib.resolved.len()
        );
        assert!(lib.face("").is_none(), "空 Fontname 表示用回退字体");
    }

    /// 真机器上的系统字体索引必须能按 `Fontname` 拿到 face
    ///
    /// CI 里可能一个系统字体都没有，所以只在索引非空时断言命中结果的字形覆盖。
    #[test]
    fn system_fontname_resolves_to_a_real_face() {
        let mut lib = FontLibrary::default();
        lib.prepare_system_fonts();
        let db = lib.db.as_ref().expect("prepare 之后必有索引");
        if db.faces().next().is_none() {
            return;
        }
        let hit = [
            "Arial",
            "Microsoft YaHei",
            "DejaVu Sans",
            "Liberation Sans",
            "Noto Sans",
        ]
        .iter()
        .find_map(|name| lib.face(name).map(|face| (*name, face)));
        if let Some((name, face)) = hit {
            assert!(covers(&face, 'A'), "{name} 解析出来了却没有拉丁字形");
        }
        assert!(
            lib.face("Font Nobody Whatsoever Ships").is_none(),
            "系统里没有的 Fontname 必须报未命中，让渲染回退内置字体"
        );
    }

    #[test]
    fn glyph_coverage_detects_missing_glyphs() {
        let data = crate::font::load_system_font().expect("system font");
        let Ok(font) = FontArc::try_from_vec(data) else {
            return;
        };
        assert!(covers(&font, '日'), "内置 CJK 回退字体必须画得出日文");
        assert!(
            !covers(&font, '\u{E000}'),
            "私有区字符不应被当作有覆盖，否则逐字回退永远不会触发"
        );
    }

    #[test]
    fn unusable_fallback_data_is_rejected() {
        let mut lib = FontLibrary::default();
        lib.set_fallback(b"\x00\x01\x00\x00not a font".to_vec());
        assert!(lib.fallback().is_none(), "坏数据不得装作加载成功");
        assert!(build_face(b"\x00\x01\x00\x00not a font", 0).is_none());
    }
}
