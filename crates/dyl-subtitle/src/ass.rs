//! ASS 样式表与覆盖标签（{\...}）解析
//!
//! 覆盖常见特效子集：`\pos`、`\move`、`\fad`、`\an`、`\c`/`\1c`、`\fs`、
//! `\bord`、`\shad`、`\3c`，以及 `[V4+ Styles]` 样式行（含 BorderStyle/
//! Outline/Shadow/OutlineColour）与 `[Script Info]` PlayResX/PlayResY。
//! 不支持的标签（`\t`/`\fr`/`\blur`/`\be`/`\k` 等）安全忽略（文本仍会显示）。

use std::collections::HashMap;

/// ASS 对齐编号（numpad 布局）：1=左下 … 9=右上
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alignment(pub u8);

impl Alignment {
    pub fn default_bottom_center() -> Self {
        Self(2)
    }

    /// 水平锚点：0=左 1=中 2=右
    pub fn h(self) -> u8 {
        (self.0 - 1) % 3
    }

    /// 垂直锚点：0=下 1=中 2=上
    pub fn v(self) -> u8 {
        (self.0 - 1) / 3
    }

    pub fn is_valid(self) -> bool {
        (1..=9).contains(&self.0)
    }
}

/// 解析 &HAABBGGRR（或 &HBBGGRR、十进制）ASS 颜色 → RGBA（0..1，直通 alpha）
///
/// ASS 的 alpha 字节是反义的：0x00 = 不透明，0xFF = 完全透明。
pub fn parse_ass_colour(s: &str) -> Option<[f32; 4]> {
    let s = s.trim().trim_end_matches('&').trim();
    let hex = if let Some(h) = s
        .strip_prefix("&H")
        .or_else(|| s.strip_prefix("&h"))
        .or_else(|| s.strip_prefix("H"))
    {
        h
    } else if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        h
    } else if s.chars().all(|c| c.is_ascii_digit()) {
        // 纯十进制：与 6 位十六进制同一编码（低位字节 = R）
        let v: u32 = s.parse().ok()?;
        return Some([
            (v & 0xFF) as f32 / 255.0,
            ((v >> 8) & 0xFF) as f32 / 255.0,
            ((v >> 16) & 0xFF) as f32 / 255.0,
            1.0,
        ]);
    } else {
        return None;
    };
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) || hex.len() > 8 {
        return None;
    }
    let v: u32 = u32::from_str_radix(hex, 16).ok()?;
    let (a, b, g, r) = if hex.len() >= 8 {
        (
            ((v >> 24) & 0xFF) as u8,
            ((v >> 16) & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        )
    } else {
        (
            0u8,
            ((v >> 16) & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        )
    };
    Some([
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        (255 - a) as f32 / 255.0,
    ])
}

/// `\move` 参数
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AssMove {
    pub from: (f32, f32),
    pub to: (f32, f32),
    /// 移动起止时间（相对事件开始，毫秒；默认 0..事件时长）
    pub t1_ms: f64,
    pub t2_ms: f64,
}

/// 一个覆盖块 `{...}` 中本引擎支持的标签
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssTags {
    pub pos: Option<(f32, f32)>,
    pub move_: Option<AssMove>,
    /// (\fad 淡入时长 ms, 淡出时长 ms)
    pub fade: Option<(f64, f64)>,
    pub alignment: Option<Alignment>,
    /// 主颜色（RGBA 0..1）
    pub color: Option<[f32; 4]>,
    /// 描边颜色（\3c，RGBA 0..1）
    pub outline_colour: Option<[f32; 4]>,
    pub font_size: Option<f32>,
    /// 描边宽度（\bord，画布像素）
    pub border: Option<f32>,
    /// 阴影深度（\shad，画布像素）
    pub shadow: Option<f32>,
    /// `{\r样式名}`：从此刻起改用该样式渲染
    ///
    /// 空串是 `{\r}` 的合法形式，表示回到脚本的默认样式，所以用 `Some(String)`
    /// 而不是 `Option<非空>`；两者在 [`layout_lines`] 里的落点不同。
    pub reset_style: Option<String>,
}

impl AssTags {
    /// 用后出现的块覆盖先前值（后写赢）
    pub fn apply_overlay(&mut self, other: &AssTags) {
        macro_rules! overwrite {
            ($($field:ident),* $(,)?) => {
                $(if other.$field.is_some() { self.$field = other.$field; })*
            };
        }
        overwrite!(
            pos,
            move_,
            fade,
            alignment,
            color,
            outline_colour,
            font_size,
            border,
            shadow
        );
        if other.reset_style.is_some() {
            self.reset_style = other.reset_style.clone();
        }
    }

    pub fn is_empty(&self) -> bool {
        self == &AssTags::default()
    }

    /// 只含样式标签的空块（`{\r名字}` 之外一个覆盖都没有）
    pub fn is_empty_style(&self) -> bool {
        let mut without = self.clone();
        without.reset_style = None;
        without.is_empty()
    }

    /// 相对上一个累计状态**新出现或变化**的字段
    ///
    /// `split_override_segments` 给的标签是累计值，逐段差分才能判断"这一段带来了
    /// 什么新覆盖"，进而让 `{\r}` 把上一个样式的临时覆盖丢掉。
    pub fn changed_since(&self, prev: &AssTags) -> AssTags {
        let mut out = AssTags::default();
        macro_rules! pick {
            ($($field:ident),* $(,)?) => {
                $(if self.$field != prev.$field { out.$field = self.$field.clone(); })*
            };
        }
        pick!(
            pos,
            move_,
            fade,
            alignment,
            color,
            outline_colour,
            font_size,
            border,
            shadow,
            reset_style
        );
        out
    }
}

fn split_args(args: &str) -> Vec<&str> {
    args.split(',').map(|s| s.trim()).collect()
}

/// 解析单个覆盖块内容（不含花括号）
pub fn parse_tag_block(inner: &str) -> AssTags {
    let mut tags = AssTags::default();
    for chunk in inner.split('\\') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        // 标签名可带数字前缀（\1c \2c \3c \4c \k1..\k7）；纯数字开头无字母则不是标签
        //
        // `\r` 例外：它的参数是样式名，而样式名本身就是字母串（`\rSign`、`\r宋体`），
        // 按"连续字母 = 标签名"贪婪切会得到一个不存在的 `rSign`，样式切换会被静默丢掉。
        let name_end = if chunk
            .as_bytes()
            .first()
            .is_some_and(|b| *b == b'r' || *b == b'R')
        {
            1
        } else if chunk.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            let after = &chunk[1..];
            if after
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_alphabetic())
            {
                1 + after
                    .find(|c: char| !c.is_ascii_alphabetic())
                    .unwrap_or(after.len())
            } else {
                0
            }
        } else {
            chunk
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(chunk.len())
        };
        let (name, rest) = chunk.split_at(name_end);
        // 参数取第一个 ')' 之前的内容：容忍 Danmaku2ASS 等生成器在 ')' 后附加的残缺标签
        let args = match rest.strip_prefix('(') {
            Some(inner) => inner.split(')').next().unwrap_or(inner),
            None => rest,
        };
        match name.to_ascii_lowercase().as_str() {
            "an" => {
                if let Ok(n) = args.trim().parse::<u8>() {
                    let a = Alignment(n);
                    if a.is_valid() {
                        tags.alignment = Some(a);
                    }
                }
            }
            "pos" => {
                let a = split_args(args);
                if a.len() >= 2 {
                    if let (Ok(x), Ok(y)) = (a[0].parse::<f32>(), a[1].parse::<f32>()) {
                        tags.pos = Some((x, y));
                    }
                }
            }
            "move" => {
                let a = split_args(args);
                if a.len() >= 4 {
                    let nums: Option<Vec<f32>> =
                        a[..4].iter().map(|s| s.parse::<f32>().ok()).collect();
                    if let Some(n) = nums {
                        let t1 = a.get(4).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                        let t2 = a
                            .get(5)
                            .and_then(|s| s.parse::<f64>().ok())
                            .unwrap_or(f64::NAN);
                        tags.move_ = Some(AssMove {
                            from: (n[0], n[1]),
                            to: (n[2], n[3]),
                            t1_ms: t1,
                            t2_ms: t2,
                        });
                    }
                }
            }
            "fad" => {
                let a = split_args(args);
                if a.len() >= 2 {
                    if let (Ok(t1), Ok(t2)) = (a[0].parse::<f64>(), a[1].parse::<f64>()) {
                        tags.fade = Some((t1.max(0.0), t2.max(0.0)));
                    }
                }
            }
            "c" | "1c" => {
                if let Some(c) = parse_ass_colour(args) {
                    tags.color = Some(c);
                }
            }
            "3c" => {
                if let Some(c) = parse_ass_colour(args) {
                    tags.outline_colour = Some(c);
                }
            }
            "bord" | "xbord" | "ybord" => {
                if let Ok(v) = args.trim().parse::<f32>() {
                    if v >= 0.0 {
                        tags.border = Some(v);
                    }
                }
            }
            "shad" | "xshad" | "yshad" => {
                if let Ok(v) = args.trim().parse::<f32>() {
                    if v >= 0.0 {
                        tags.shadow = Some(v);
                    }
                }
            }
            "fs" => {
                if let Ok(v) = args.trim().parse::<f32>() {
                    if v > 0.0 {
                        tags.font_size = Some(v);
                    }
                }
            }
            // `{\r样式名}`：样式名里可以有空格，整段参数都是名字
            "r" => tags.reset_style = Some(args.trim().to_string()),
            _ => {
                // 其余标签（\b \i \fn \fr \t \blur \k …）忽略，文本照常渲染
            }
        }
    }
    tags
}

/// 把带覆盖块的原始文本切成 (标签, 文本) 段序列
///
/// 块只对其后文本生效；相邻多个块按“后写赢”合并进同一段。
pub fn split_override_segments(raw: &str) -> Vec<(AssTags, String)> {
    let mut segments = Vec::new();
    let mut pending = AssTags::default();
    let mut text = String::new();
    let mut chars = raw.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        match ch {
            '{' if idx == 0 || !raw[..idx].ends_with('\\') => {
                if !text.is_empty() {
                    segments.push((pending.clone(), std::mem::take(&mut text)));
                }
                // 找到匹配的 '}'（缺失则把剩余当文本）
                let rest = &raw[idx + 1..];
                match rest.find('}') {
                    Some(end) => {
                        pending.apply_overlay(&parse_tag_block(&rest[..end]));
                        // 跳过块内容
                        let skip = idx + 1 + end + 1;
                        while chars.peek().is_some_and(|&(next_idx, _)| next_idx < skip) {
                            chars.next();
                        }
                    }
                    None => text.push(ch),
                }
            }
            '}' => {
                if !text.is_empty() {
                    segments.push((pending.clone(), std::mem::take(&mut text)));
                }
            }
            _ => text.push(ch),
        }
    }
    if !text.is_empty() || segments.is_empty() {
        segments.push((pending, text));
    }
    segments
}

/// `[V4+ Styles]` 样式行
#[derive(Debug, Clone)]
pub struct AssStyle {
    pub name: String,
    pub font_name: String,
    pub font_size: f32,
    pub primary_colour: [f32; 4],
    /// 描边颜色（OutlineColour），默认黑
    pub outline_colour: [f32; 4],
    /// 描边宽度（Outline，画布像素）
    pub outline: f32,
    /// 阴影深度（Shadow，画布像素）
    pub shadow: f32,
    /// BorderStyle：1=描边+阴影，3=不透明底框（暂按 1 处理）
    pub border_style: u8,
    pub alignment: Alignment,
    pub margin_l: f32,
    pub margin_r: f32,
    pub margin_v: f32,
}

impl Default for AssStyle {
    fn default() -> Self {
        Self {
            name: "Default".to_string(),
            font_name: String::new(),
            font_size: 0.0, // 0 = 使用 config.default_font_size
            primary_colour: [1.0, 1.0, 1.0, 1.0],
            outline_colour: [0.0, 0.0, 0.0, 1.0],
            // 可读性默认：白字必须带黑描边，否则浅色画面上直接消失
            outline: 2.0,
            shadow: 0.0,
            border_style: 1,
            alignment: Alignment::default_bottom_center(),
            margin_l: 0.0,
            margin_r: 0.0,
            margin_v: 0.0, // 0 = 使用 config.bottom_margin
        }
    }
}

/// 一条 Dialogue（保留覆盖标签信息；base.text 为去标签纯文本，向后兼容）
#[derive(Debug, Clone)]
pub struct AssEvent {
    pub base: crate::types::SubtitleEvent,
    pub segments: Vec<(AssTags, String)>,
    pub layer: i32,
    pub margin_l: f32,
    pub margin_r: f32,
    pub margin_v: f32,
}

impl AssEvent {
    /// 无特效的纯文本事件（SRT、内嵌字幕走这条路）
    pub fn plain(base: crate::types::SubtitleEvent) -> Self {
        Self {
            base,
            segments: Vec::new(),
            layer: 0,
            margin_l: 0.0,
            margin_r: 0.0,
            margin_v: 0.0,
        }
    }

    pub fn has_effects(&self) -> bool {
        self.segments.iter().any(|(t, _)| !t.is_empty())
    }
}

/// 事件拆出来的一行：行首生效的样式、该行的覆盖标签、已归一化的文本
#[derive(Debug, Clone, PartialEq)]
pub struct AssLine {
    pub style: String,
    pub tags: AssTags,
    pub text: String,
}

/// 把事件按行拆开，并确定每一行生效的样式
///
/// 这是行内 `{\r样式名}` 的支持入口，规则刻意保守：
///
/// - `{\r名字}` 只在**行首**（`\N` 之后立刻出现）才能让整行换样式。一行之内换
///   样式仍按该行行首的样式整行渲染 —— 同行混排要逐 run 量测步进并把多 run 合成
///   一张位图，属于 libass 式 render tree，不在本引擎范围内。
/// - `{\r}`（无名字）回到脚本的默认样式 `default_style`。
/// - 样式切换会丢弃此前的 `\fs`/`\c` 等临时覆盖，新样式带自己的外观；出现在行中的
///   `{\r}` 只影响它**之后**的行，本行仍带着已经生效过的覆盖（那部分字确实是那么画的）。
/// - 覆盖标签跨行延续：`\N` 不清空已生效的覆盖（ASS 语义如此）。
pub fn layout_lines(ev: &AssEvent, default_style: &str) -> Vec<AssLine> {
    let mut lines: Vec<AssLine> = Vec::new();

    // 纯文本事件（SRT、无标签内嵌轨）：整段用事件自身样式，只认真换行
    if ev.segments.is_empty() {
        lines.extend(ev.base.text.split('\n').map(|piece| AssLine {
            style: ev.base.style.clone(),
            tags: AssTags::default(),
            text: piece.to_string(),
        }));
        return lines;
    }

    // `split_override_segments` 的标签是**累计**的（块只增不减），所以逐段取差分
    // 才能知道"本段新出现的覆盖"，进而让 `{\r}` 丢弃上一个样式的临时覆盖。
    let mut prev = AssTags::default();
    let mut cur_style = ev.base.style.clone();
    let mut cur_tags = AssTags::default();
    let mut line_style = cur_style.clone();
    let mut line_tags = AssTags::default();
    let mut buf = String::new();

    let finish_line = |lines: &mut Vec<AssLine>, style: &str, tags: &AssTags, buf: &mut String| {
        lines.push(AssLine {
            style: style.to_string(),
            tags: tags.clone(),
            text: crate::types::normalize_whitespace(buf),
        });
        buf.clear();
    };

    for (seg_tags, seg_text) in &ev.segments {
        let changed = seg_tags.changed_since(&prev);
        prev = seg_tags.clone();

        if let Some(name) = &changed.reset_style {
            cur_style = if name.is_empty() {
                default_style.to_string()
            } else {
                name.clone()
            };
            // `{\r}` 丢掉的是**此后**的临时覆盖；本行已经按老覆盖画过了，
            // 所以只清状态机那份，行内累计那份留着（否则 `{\fs60}甲{\rB}\N乙`
            // 的第一行会丢字号）。
            cur_tags = AssTags::default();
            // 行首切换才影响本行；行中切换只从下一行起生效
            if buf.trim().is_empty() {
                line_style = cur_style.clone();
            }
        }
        if !changed.is_empty_style() {
            let mut overlay = changed.clone();
            overlay.reset_style = None;
            cur_tags.apply_overlay(&overlay);
            line_tags.apply_overlay(&overlay);
        }

        for piece in split_line_breaks(seg_text) {
            match piece {
                Piece::Text(t) => buf.push_str(t),
                Piece::Break => {
                    finish_line(&mut lines, &line_style, &line_tags, &mut buf);
                    line_style = cur_style.clone();
                    // 新行从"此刻生效的覆盖"起算：跨行延续，或被上面的 `{\r}` 清空
                    line_tags = cur_tags.clone();
                }
            }
        }
    }
    // 末行始终落地：与旧的"整段 join 后换行切分"逐字对齐（结尾换行会留一个空行）
    finish_line(&mut lines, &line_style, &line_tags, &mut buf);
    lines
}

/// 一行文本被 ASS 换行符切出来的片段
enum Piece<'a> {
    Text(&'a str),
    Break,
}

/// 按 `\N`、`\n` 与真换行切分一段字幕文本
///
/// 只匹配 ASCII 字节，多字节字符的后续字节都 ≥ 0x80，所以切片点必然落在字符边界上。
fn split_line_breaks(text: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let hit = match bytes[i] {
            b'\\' if bytes.get(i + 1) == Some(&b'N') || bytes.get(i + 1) == Some(&b'n') => Some(2),
            b'\n' => Some(1),
            _ => None,
        };
        let Some(len) = hit else {
            i += 1;
            continue;
        };
        if i > start {
            out.push(Piece::Text(&text[start..i]));
        }
        out.push(Piece::Break);
        i += len;
        start = i;
    }
    if start < text.len() {
        out.push(Piece::Text(&text[start..]));
    }
    out
}

impl AssEvent {
    /// 按行拆开本事件（见 [`layout_lines`]）
    pub fn lines(&self, default_style: &str) -> Vec<AssLine> {
        layout_lines(self, default_style)
    }
}

/// 完整 ASS 文档（样式表 + 画布分辨率 + 事件）
#[derive(Debug, Clone, Default)]
pub struct AssDocument {
    pub play_res_x: f32,
    pub play_res_y: f32,
    pub styles: HashMap<String, AssStyle>,
    pub events: Vec<AssEvent>,
}

/// `\move` 在 pts 时刻的插值位置（画布坐标）
pub fn eval_move(mv: &AssMove, pts: f64, event_start: f64, event_end: f64) -> (f32, f32) {
    let duration_ms = ((event_end - event_start) * 1000.0).max(0.0);
    let t1 = mv.t1_ms;
    let t2 = if mv.t2_ms.is_nan() {
        duration_ms
    } else {
        mv.t2_ms
    };
    let rel = (pts - event_start) * 1000.0;
    let u: f32 = if t2 <= t1 {
        1.0
    } else {
        ((rel - t1) / (t2 - t1)).clamp(0.0, 1.0) as f32
    };
    (
        mv.from.0 + (mv.to.0 - mv.from.0) * u,
        mv.from.1 + (mv.to.1 - mv.from.1) * u,
    )
}

/// `\fad` 在 pts 时刻的整体 alpha（0..1，1=完全不透明）
pub fn eval_fade(fade: (f64, f64), pts: f64, event_start: f64, event_end: f64) -> f32 {
    let (t_in, t_out) = fade;
    let mut alpha = 1.0f64;
    if t_in > 0.0 {
        alpha = alpha.min((pts - event_start) * 1000.0 / t_in);
    }
    if t_out > 0.0 {
        alpha = alpha.min((event_end - pts) * 1000.0 / t_out);
    }
    alpha.clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_colour() {
        // &HAABBGGRR：alpha=00 不透明
        assert_eq!(parse_ass_colour("&H00FFFFFF&"), Some([1.0, 1.0, 1.0, 1.0]));
        // R=FF → red
        assert_eq!(parse_ass_colour("&H000000FF"), Some([1.0, 0.0, 0.0, 1.0]));
        // B=FF → blue
        assert_eq!(parse_ass_colour("&H00FF0000"), Some([0.0, 0.0, 1.0, 1.0]));
        // alpha=7F 半透明
        let c = parse_ass_colour("&H7F0000FF").unwrap();
        assert!((c[3] - 0.506).abs() < 0.01);
        // 6 位无 alpha
        assert_eq!(parse_ass_colour("&H0000FF"), Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_ass_colour("16711680"), Some([0.0, 0.0, 1.0, 1.0]));
        assert_eq!(parse_ass_colour("garbage"), None);
        assert_eq!(parse_ass_colour(""), None);
    }

    #[test]
    fn test_alignment_mapping() {
        assert_eq!(Alignment(1).h(), 0); // 左
        assert_eq!(Alignment(2).h(), 1); // 中
        assert_eq!(Alignment(3).h(), 2); // 右
        assert_eq!(Alignment(2).v(), 0); // 下
        assert_eq!(Alignment(5).v(), 1); // 中
        assert_eq!(Alignment(9).v(), 2); // 右上
        assert!(Alignment(0).0 == 0 && !Alignment(0).is_valid());
        assert!(!Alignment(10).is_valid());
    }

    #[test]
    fn test_tag_block_supported_tags() {
        let t = parse_tag_block(r"an8\pos(100,200)\fs36");
        assert_eq!(t.alignment, Some(Alignment(8)));
        assert_eq!(t.pos, Some((100.0, 200.0)));
        assert_eq!(t.font_size, Some(36.0));

        let t = parse_tag_block(r"move(0,50,800,50)");
        let mv = t.move_.unwrap();
        assert_eq!(mv.from, (0.0, 50.0));
        assert_eq!(mv.to, (800.0, 50.0));
        assert!(mv.t2_ms.is_nan()); // 缺省 = 事件时长

        let t = parse_tag_block(r"move(0,0,10,10,500,1500)");
        let mv = t.move_.unwrap();
        assert_eq!((mv.t1_ms, mv.t2_ms), (500.0, 1500.0));

        let t = parse_tag_block(r"fad(500,800)");
        assert_eq!(t.fade, Some((500.0, 800.0)));

        let t = parse_tag_block(r"c&H0000FF&");
        assert_eq!(t.color, Some([1.0, 0.0, 0.0, 1.0]));

        // 未知标签安全忽略
        let t = parse_tag_block(r"frz45\t(0,100,\b1)\blur3");
        assert_eq!(t, AssTags::default());

        // Danmaku2ASS 残缺标签：')' 后带噪声也应解析出 move
        let t = parse_tag_block(r"move(2640,120,-720,120)&H1BFBFF");
        let mv = t.move_.unwrap();
        assert_eq!((mv.from, mv.to), ((2640.0, 120.0), (-720.0, 120.0)));
    }

    #[test]
    fn test_outline_shadow_tags() {
        let t = parse_tag_block(r"bord3\shad2\3c&HFF0000");
        assert_eq!(t.border, Some(3.0));
        assert_eq!(t.shadow, Some(2.0));
        // &HFF0000 = 6 位 BBGGRR → B=FF → blue
        assert_eq!(t.outline_colour, Some([0.0, 0.0, 1.0, 1.0]));

        // \bord0 显式关闭描边（Some(0.0)，不能当成"未设置"）
        let t = parse_tag_block(r"xbord0\yshad0");
        assert_eq!(t.border, Some(0.0));
        assert_eq!(t.shadow, Some(0.0));

        // 后写赢
        let mut acc = AssTags::default();
        acc.apply_overlay(&parse_tag_block(r"bord1"));
        acc.apply_overlay(&parse_tag_block(r"bord4"));
        assert_eq!(acc.border, Some(4.0));
    }

    #[test]
    fn test_apply_overlay_last_wins() {
        let mut acc = AssTags::default();
        acc.apply_overlay(&parse_tag_block(r"an5\fs20\c&H00FF00&"));
        acc.apply_overlay(&parse_tag_block(r"an7"));
        assert_eq!(acc.alignment, Some(Alignment(7)));
        assert_eq!(acc.font_size, Some(20.0));
        assert_eq!(acc.color, Some([0.0, 1.0, 0.0, 1.0]));
    }

    #[test]
    fn test_split_override_segments() {
        let segs = split_override_segments(r"{\an8\pos(10,20)}hello{\c&HFF0000&} world");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].1, "hello");
        assert_eq!(segs[0].0.alignment, Some(Alignment(8)));
        assert_eq!(segs[1].1, " world");
        // &HFF0000 = 6 位 BGR → blue
        assert_eq!(segs[1].0.color, Some([0.0, 0.0, 1.0, 1.0]));

        let segs = split_override_segments("no tags here");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].1, "no tags here");
        assert_eq!(segs[0].0, AssTags::default());

        // 未闭合大括号 → 不当作标签块
        let segs = split_override_segments(r"{\an8 broken");
        assert_eq!(segs.len(), 1);
        assert!(segs[0].1.contains("an8"));
    }

    #[test]
    fn test_eval_move_interpolation() {
        let mv = AssMove {
            from: (0.0, 100.0),
            to: (400.0, 100.0),
            t1_ms: 0.0,
            t2_ms: f64::NAN,
        };
        // 事件 10s..20s：中点 → 插值一半
        let (x, y) = eval_move(&mv, 15.0, 10.0, 20.0);
        assert!((x - 200.0).abs() < 0.01);
        assert!((y - 100.0).abs() < 0.01);
        // 前段之前 → 起点
        assert_eq!(eval_move(&mv, 5.0, 10.0, 20.0), (0.0, 100.0));
        // 之后 → 终点
        assert_eq!(eval_move(&mv, 30.0, 10.0, 20.0), (400.0, 100.0));

        // 显式时间段 2000..4000ms
        let mv2 = AssMove {
            from: (0.0, 0.0),
            to: (100.0, 0.0),
            t1_ms: 2000.0,
            t2_ms: 4000.0,
        };
        assert_eq!(eval_move(&mv2, 11.0, 10.0, 20.0), (0.0, 0.0));
        let (x, _) = eval_move(&mv2, 13.0, 10.0, 20.0);
        assert!((x - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_eval_fade() {
        // 500ms 淡入 / 1000ms 淡出，事件 10..20s
        let f = (500.0, 1000.0);
        assert_eq!(eval_fade(f, 10.0, 10.0, 20.0), 0.0);
        let a = eval_fade(f, 10.25, 10.0, 20.0);
        assert!((a - 0.5).abs() < 0.01);
        assert_eq!(eval_fade(f, 11.0, 10.0, 20.0), 1.0);
        let a = eval_fade(f, 19.5, 10.0, 20.0);
        assert!((a - 0.5).abs() < 0.01);
        assert_eq!(eval_fade(f, 20.0, 10.0, 20.0), 0.0);
    }

    /// 造一条带覆盖块的 dialogue：`base.text` 是去标签纯文本，`segments` 保留样式信息
    fn event_with(raw: &str) -> AssEvent {
        AssEvent {
            base: crate::types::SubtitleEvent {
                start_time: 0.0,
                end_time: 5.0,
                text: crate::types::strip_ass_tags(raw)
                    .replace("\\N", "\n")
                    .replace("\\n", "\n"),
                style: "Default".to_string(),
            },
            segments: split_override_segments(raw),
            layer: 0,
            margin_l: 0.0,
            margin_r: 0.0,
            margin_v: 0.0,
        }
    }

    /// `\r` 的参数是样式名，本身可能是纯字母（`\rSign`）或中文，所以标签名只能取首字母
    #[test]
    fn reset_tag_takes_the_rest_as_style_name() {
        assert_eq!(
            parse_tag_block(r"\rSign").reset_style,
            Some("Sign".to_string())
        );
        assert_eq!(parse_tag_block(r"\r").reset_style, Some(String::new()));
        assert_eq!(
            parse_tag_block(r"\r宋体 Bold").reset_style,
            Some("宋体 Bold".to_string())
        );
        // 与其他标签混在一个块里
        let t = parse_tag_block(r"an8\rTitle\fs40");
        assert_eq!(t.alignment, Some(Alignment(8)));
        assert_eq!(t.reset_style, Some("Title".to_string()));
        assert_eq!(t.font_size, Some(40.0));
    }

    #[test]
    fn reset_at_line_start_switches_style_for_that_line() {
        let lines = event_with(r"第一行{\rJP}\N第二行").lines("Default");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].style, "Default");
        assert_eq!(lines[0].text, "第一行");
        assert_eq!(lines[1].style, "JP");
        assert_eq!(lines[1].text, "第二行");
    }

    /// `{\r}` 不带名字回脚本默认样式，而不是保留上一个
    #[test]
    fn bare_reset_falls_back_to_default_style() {
        let lines = event_with(r"{\rA}甲{\r}\N乙").lines("Default");
        assert_eq!(lines[0].style, "A");
        assert_eq!(lines[1].style, "Default");
    }

    /// 换样式把上一个样式的临时覆盖丢掉
    #[test]
    fn style_switch_discards_previous_overrides() {
        let lines = event_with(r"{\fs60\c&H0000FF&}甲{\rB}\N乙").lines("Default");
        assert_eq!(lines[0].tags.font_size, Some(60.0));
        assert!(lines[0].tags.color.is_some());
        assert_eq!(lines[1].style, "B");
        assert_eq!(lines[1].tags, AssTags::default());
    }

    /// 覆盖跨 `\N` 延续：换行不清空已生效的标签
    #[test]
    fn overrides_continue_across_line_breaks() {
        let lines = event_with(r"{\fs50}甲\N乙").lines("Default");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].tags.font_size, Some(50.0));
        assert_eq!(lines[1].tags.font_size, Some(50.0));
    }

    /// 行中换样式只从下一行起生效，本行仍按行首样式整行渲染
    #[test]
    fn mid_line_reset_applies_from_next_line_only() {
        let lines = event_with(r"甲{\rB}乙\N丙").lines("Default");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].style, "Default");
        assert_eq!(lines[0].text, "甲乙");
        assert_eq!(lines[1].style, "B");
        assert_eq!(lines[1].text, "丙");
    }

    /// 纯文本事件（SRT、无标签内封轨）按真换行拆开，样式用事件自己的
    #[test]
    fn plain_event_splits_on_newlines() {
        let ev = AssEvent::plain(crate::types::SubtitleEvent {
            start_time: 0.0,
            end_time: 5.0,
            text: "第一行\n第二行".to_string(),
            style: "Sign".to_string(),
        });
        let lines = ev.lines("Default");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.style == "Sign"));
        assert!(lines.iter().all(|l| l.tags.is_empty()));
        assert_eq!(lines[1].text, "第二行");
    }
}
