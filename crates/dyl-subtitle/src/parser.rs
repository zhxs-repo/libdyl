//! 字幕解析器 — 支持 SRT 和 ASS/SSA 格式

use crate::ass::{
    parse_ass_colour, split_override_segments, Alignment, AssDocument, AssEvent, AssStyle,
};
use crate::types::{
    normalize_whitespace, parse_ass_time, parse_srt_time, strip_ass_tags, SubtitleEvent,
};

/// 字幕解析器 trait
pub trait SubtitleParser: Send {
    fn parse(&self, data: &[u8]) -> Vec<SubtitleEvent>;
}

/// SRT 字幕解析器
///
/// 格式：
/// ```text
/// 1
/// 00:00:01,000 --> 00:00:05,000
/// 第一行字幕
/// 第二行字幕
///
/// 2
/// ...
/// ```
pub struct SrtParser;

impl SrtParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SrtParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SubtitleParser for SrtParser {
    fn parse(&self, data: &[u8]) -> Vec<SubtitleEvent> {
        let text = match std::str::from_utf8(data) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let mut events = Vec::new();
        let mut blocks = text.split("\n\n").peekable();

        for block in &mut blocks {
            let lines: Vec<&str> = block.lines().collect();
            if lines.len() < 3 {
                continue;
            }

            // 第 1 行：序号（可忽略）
            // 第 2 行：时间轴 "HH:MM:SS,mmm --> HH:MM:SS,mmm"
            let time_line = lines[1];
            let time_parts: Vec<&str> = time_line.split("-->").collect();
            if time_parts.len() != 2 {
                continue;
            }

            let start = match parse_srt_time(time_parts[0].trim()) {
                Some(t) => t,
                None => continue,
            };
            let end = match parse_srt_time(time_parts[1].trim()) {
                Some(t) => t,
                None => continue,
            };

            // 第 3 行及之后：字幕文本（可能多行）
            let subtitle_text: String = lines[2..].join("\n");
            if !subtitle_text.is_empty() {
                events.push(SubtitleEvent {
                    start_time: start,
                    end_time: end,
                    text: subtitle_text,
                    style: "Default".to_string(),
                });
            }
        }

        tracing::info!("SRT parser: {} events parsed", events.len());
        events
    }
}

/// ASS/SSA 字幕解析器
///
/// 完整解析 [Script Info]（PlayResX/Y）、[V4+ Styles]（样式表）与
/// [Events]（Dialogue 行，含覆盖标签）；见 `AssParser::parse_document`。
pub struct AssParser;

#[derive(PartialEq)]
enum Section {
    None,
    ScriptInfo,
    Styles,
    Events,
}

fn default_style_fields() -> Vec<String> {
    "Name,Fontname,Fontsize,PrimaryColour,SecondaryColour,OutlineColour,BackColour,\
     Bold,Italic,Underline,StrikeOut,ScaleX,ScaleY,Spacing,Angle,BorderStyle,Outline,\
     Shadow,Alignment,MarginL,MarginR,MarginV,Encoding"
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .collect()
}

fn default_event_fields() -> Vec<String> {
    "Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text"
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .collect()
}

fn parse_field_list(rest: &str) -> Vec<String> {
    rest.split(',').map(|s| s.trim().to_lowercase()).collect()
}

fn parse_style_line(rest: &str, format: &[String]) -> Option<AssStyle> {
    let fields: Vec<&str> = rest.splitn(format.len().max(1), ',').collect();
    let get = |name: &str| {
        format
            .iter()
            .position(|f| f == name)
            .and_then(|i| fields.get(i).map(|s| s.trim()))
    };

    let alignment = get("alignment")
        .and_then(|v| v.parse::<u8>().ok())
        .map(Alignment)
        .filter(|a| a.is_valid())
        .unwrap_or_else(Alignment::default_bottom_center);

    let fallback = AssStyle::default();
    Some(AssStyle {
        name: get("name")?.to_string(),
        font_name: get("fontname").unwrap_or("").to_string(),
        font_size: get("fontsize").and_then(|v| v.parse().ok()).unwrap_or(0.0),
        primary_colour: get("primarycolour")
            .and_then(parse_ass_colour)
            .unwrap_or([1.0, 1.0, 1.0, 1.0]),
        outline_colour: get("outlinecolour")
            .and_then(parse_ass_colour)
            .unwrap_or(fallback.outline_colour),
        outline: get("outline")
            .and_then(|v| v.parse().ok())
            .unwrap_or(fallback.outline),
        shadow: get("shadow")
            .and_then(|v| v.parse().ok())
            .unwrap_or(fallback.shadow),
        border_style: get("borderstyle").and_then(|v| v.parse().ok()).unwrap_or(1),
        alignment,
        margin_l: get("marginl").and_then(|v| v.parse().ok()).unwrap_or(0.0),
        margin_r: get("marginr").and_then(|v| v.parse().ok()).unwrap_or(0.0),
        margin_v: get("marginv").and_then(|v| v.parse().ok()).unwrap_or(0.0),
    })
}

fn parse_dialogue(rest: &str, format: &[String]) -> Option<AssEvent> {
    let fields: Vec<&str> = rest.splitn(10.max(format.len()), ',').collect();
    let get = |name: &str, fallback: usize| -> &str {
        let i = format.iter().position(|f| f == name).unwrap_or(fallback);
        fields.get(i).copied().unwrap_or("").trim()
    };

    let start = parse_ass_time(get("start", 1))?;
    let end = parse_ass_time(get("end", 2))?;
    let style = get("style", 3).to_string();
    let raw_text = get("text", 9);
    assemble_event(start, end, style, get("layer", 0), raw_text)
        .map(|ev| ev.with_margins(get("marginl", 5), get("marginr", 6), get("marginv", 7)))
}

/// 一条对话行的公共构造：去标签纯文本 + 覆盖标签段 + layer
///
/// layer/边距都写成字符串由调用方提供，因为文件里的 `.ass` 与容器包里的对话行
/// 字段数不同（见 [`parse_embedded_dialogue`]），索引只有调用方知道。
fn assemble_event(
    start: f64,
    end: f64,
    style: String,
    layer: &str,
    raw_text: &str,
) -> Option<AssEvent> {
    let plain = normalize_whitespace(&strip_ass_tags(raw_text).replace("\\N", "\n"));
    if plain.trim().is_empty() {
        return None;
    }
    Some(AssEvent {
        base: SubtitleEvent {
            start_time: start,
            end_time: end,
            text: plain,
            style,
        },
        segments: split_override_segments(raw_text),
        layer: layer.trim().parse().unwrap_or(0),
        margin_l: 0.0,
        margin_r: 0.0,
        margin_v: 0.0,
    })
}

/// ASS 的 Margin 字段允许留空（空 = 用样式表的值）
fn parse_margin(field: &str) -> f32 {
    field.trim().parse().unwrap_or(0.0)
}

impl AssEvent {
    /// 补上事件级边距（0 = 未设置，渲染时回退到样式/config）
    fn with_margins(mut self, margin_l: &str, margin_r: &str, margin_v: &str) -> Self {
        self.margin_l = parse_margin(margin_l);
        self.margin_r = parse_margin(margin_r);
        self.margin_v = parse_margin(margin_v);
        self
    }
}

/// 前导字段：Layer 是整数，Start/End 是 `H:MM:SS.cc`
fn is_lead_field(field: &str) -> bool {
    let t = field.trim();
    !t.is_empty() && (t.parse::<i32>().is_ok() || parse_ass_time(t).is_some())
}

/// 解析容器内封的 ASS/SSA 对话行（`Dialogue:` 前缀可有可无）
///
/// 时间轴在包本身（pts + duration），所以 payload 里没有 Start/End；而不同 muxer
/// 还会额外塞一个事件序号 —— 实测同一轨的 payload 形如 `0,0,Default,,0,0,0,,正文`，
/// 第一个 `0` 是随包递增的序号，第二个才是 Layer。前导字段数因此**不可靠**，
/// 但 Text 之前固定隔着 `Name, MarginL, MarginR, MarginV, Effect` 五个字段，
/// 而 Layer/Start/End 都是纯数字或时间戳，所以按形状定位：跳过前导数字字段，
/// 第一个非数字字段即 Style。Text 里允许逗号，因此只能从头对齐、尾段重新拼回。
///
/// 校验失败（Margin 不是数字或空）时返回 `None`：muxer 会把 `[Events] Format:`
/// 这类脚本头也当字幕包投递，那种包没有正文，不能当字幕显示。
pub fn parse_embedded_dialogue(raw: &str, start: f64, end: f64) -> Option<AssEvent> {
    let line = raw.trim();
    let line = match line.get(..9) {
        Some(prefix) if prefix.eq_ignore_ascii_case("Dialogue:") => &line[9..],
        _ => line,
    }
    .trim_start();

    let fields: Vec<&str> = line.split(',').collect();
    let style = fields.iter().position(|f| !is_lead_field(f))?;
    // Style 之后是 Name, MarginL, MarginR, MarginV, Effect, Text
    if style + 6 >= fields.len() {
        return None;
    }
    let margins = [&fields[style + 2], &fields[style + 3], &fields[style + 4]];
    if !margins
        .iter()
        .all(|f| f.trim().is_empty() || f.trim().parse::<f32>().is_ok())
    {
        return None;
    }
    // Layer 紧挨 Style（无时间戳的容器布局）；有 Start/End 时 Layer 在首字段
    let has_times = style >= 3
        && fields[1..3]
            .iter()
            .all(|f| parse_ass_time(f.trim()).is_some());
    let layer = if has_times {
        fields[0]
    } else {
        fields.get(style.saturating_sub(1)).copied().unwrap_or("0")
    };
    let text = fields[style + 6..].join(",");
    let ev = assemble_event(
        start,
        end,
        fields[style].trim().to_string(),
        layer,
        text.trim(),
    )?;
    Some(ev.with_margins(fields[style + 2], fields[style + 3], fields[style + 4]))
}

impl AssParser {
    pub fn new() -> Self {
        Self
    }

    /// 解析完整 ASS 文档：画布分辨率、样式表、事件（含覆盖标签段）
    pub fn parse_document(&self, data: &[u8]) -> AssDocument {
        let text = match std::str::from_utf8(data) {
            Ok(s) => s,
            Err(_) => return AssDocument::default(),
        };

        let mut doc = AssDocument::default();
        let mut section = Section::None;
        let mut style_format = default_style_fields();
        let mut event_format = default_event_fields();

        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with(';') {
                continue;
            }

            if trimmed.starts_with('[') {
                let lower = trimmed.to_lowercase();
                section = if lower == "[script info]" {
                    Section::ScriptInfo
                } else if lower.starts_with("[v4") && lower.contains("styles") {
                    Section::Styles
                } else if lower == "[events]" {
                    Section::Events
                } else {
                    Section::None
                };
                continue;
            }

            match section {
                Section::ScriptInfo => {
                    if let Some((key, value)) = trimmed.split_once(':') {
                        let value = value.trim();
                        match key.trim().to_lowercase().as_str() {
                            "playresx" => doc.play_res_x = value.parse().unwrap_or(0.0),
                            "playresy" => doc.play_res_y = value.parse().unwrap_or(0.0),
                            _ => {}
                        }
                    }
                }
                Section::Styles => {
                    if let Some(rest) = trimmed.strip_prefix("Format:") {
                        style_format = parse_field_list(rest);
                    } else if let Some(rest) = trimmed.strip_prefix("Style:") {
                        if let Some(style) = parse_style_line(rest, &style_format) {
                            doc.styles.insert(style.name.clone(), style);
                        }
                    }
                }
                Section::Events => {
                    if let Some(rest) = trimmed.strip_prefix("Format:") {
                        event_format = parse_field_list(rest);
                    } else if let Some(rest) = trimmed.strip_prefix("Dialogue:") {
                        if let Some(event) = parse_dialogue(rest, &event_format) {
                            doc.events.push(event);
                        }
                    }
                }
                Section::None => {}
            }
        }

        tracing::info!(
            "ASS parser: {} events, {} styles, play_res={:.0}x{:.0}",
            doc.events.len(),
            doc.styles.len(),
            doc.play_res_x,
            doc.play_res_y
        );
        doc
    }
}

impl Default for AssParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SubtitleParser for AssParser {
    /// 向后兼容入口：只返回去标签的纯文本事件
    fn parse(&self, data: &[u8]) -> Vec<SubtitleEvent> {
        self.parse_document(data)
            .events
            .into_iter()
            .map(|e| e.base)
            .collect()
    }
}

/// `load_subtitle` 认的文件格式（按扩展名，规范 5.2 的 `features.subtitle_formats`）。
///
/// 值写在这里是因为下面这个函数就是实现：新增格式要同时改分支和这张表，
/// `capabilities` 测试会拿每种格式的样例真正走一遍解析器，缺样例或走不通就红。
///
/// 注意这与 `media.tracks[].codec` 是两套词汇：后者是容器内轨的**编码名**
/// （`subrip` / `mov_text` / `ass` / `ssa`），由 `dyl-ffmpeg` 声明。
pub const SUPPORTED_FILE_FORMATS: &[&str] = &["srt", "ass", "ssa"];

/// 根据文件扩展名选择解析器
pub fn parser_for_extension(ext: &str) -> Box<dyn SubtitleParser> {
    match ext.to_lowercase().as_str() {
        "srt" => Box::new(SrtParser::new()),
        "ass" | "ssa" => Box::new(AssParser::new()),
        _ => Box::new(SrtParser::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_srt_parser() {
        let srt = "1\n00:00:01,000 --> 00:00:05,000\nHello World\n\n2\n00:00:06,000 --> 00:00:10,000\nSecond subtitle\n";
        let parser = SrtParser::new();
        let events = parser.parse(srt.as_bytes());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].start_time, 1.0);
        assert_eq!(events[0].end_time, 5.0);
        assert_eq!(events[0].text, "Hello World");
        assert_eq!(events[1].text, "Second subtitle");
    }

    #[test]
    fn test_srt_parser_multiline() {
        let srt = "1\n00:00:01,000 --> 00:00:05,000\nLine 1\nLine 2\n";
        let parser = SrtParser::new();
        let events = parser.parse(srt.as_bytes());
        assert_eq!(events.len(), 1);
        assert!(events[0].text.contains("Line 1"));
        assert!(events[0].text.contains("Line 2"));
    }

    #[test]
    fn test_ass_parser() {
        let ass = r#"[Script Info]
Title: Test

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour
Style: Default,Arial,20,&H00FFFFFF

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,Hello World
Dialogue: 0,0:00:06.00,0:00:10.00,Default,,0,0,0,,{\b1}Bold text
"#;
        let parser = AssParser::new();
        let events = parser.parse(ass.as_bytes());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].text, "Hello World");
        assert_eq!(events[1].text, "Bold text"); // tags stripped
        assert_eq!(events[0].style, "Default");
    }

    #[test]
    fn test_ass_parser_line_break() {
        let ass = r#"[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,Line 1\NLine 2
"#;
        let parser = AssParser::new();
        let events = parser.parse(ass.as_bytes());
        assert_eq!(events.len(), 1);
        assert!(events[0].text.contains('\n'));
    }

    #[test]
    fn test_style_outline_shadow_colour_fields() {
        let ass = r#"[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,40,&H00FFFFFF,&H000000FF,&H00FF0000,&H80000000,0,0,0,0,100,100,0,0,1,4,2,2,10,10,10,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,x
"#;
        let doc = AssParser::new().parse_document(ass.as_bytes());
        let st = doc.styles.get("Default").expect("style present");
        assert!((st.outline - 4.0).abs() < f32::EPSILON);
        assert!((st.shadow - 2.0).abs() < f32::EPSILON);
        assert_eq!(st.border_style, 1);
        // &H00FF0000 = BBGGRR → B=FF → blue
        assert_eq!(st.outline_colour, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn test_style_missing_outline_fields_fall_back_to_readable_default() {
        // 只给前 4 列的简陋样式表：描边宽度必须回退为可读性默认（>0），
        // 而不是 0（白字白边在浅色画面上不可见）
        let ass = r#"[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour
Style: Sparse,Arial,40,&H00FFFFFF

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Sparse,,0,0,0,,x
"#;
        let doc = AssParser::new().parse_document(ass.as_bytes());
        let st = doc.styles.get("Sparse").unwrap();
        assert!(st.outline > 0.0, "default outline should be non-zero");
        assert_eq!(st.outline_colour, [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn test_parser_for_extension() {
        let _srt = parser_for_extension("srt");
        let _ass = parser_for_extension("ass");
        let _ssa = parser_for_extension("ssa");
    }

    #[test]
    fn test_srt_parser_chinese() {
        let srt = "1\n00:00:01,000 --> 00:00:05,000\n你好世界\n";
        let parser = SrtParser::new();
        let events = parser.parse(srt.as_bytes());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "你好世界");
    }

    /// 内封轨 payload：去掉 muxer 序号与字段，留下正文与真实样式名
    #[test]
    fn embedded_dialogue_keeps_style_and_clean_text() {
        let raw = "0,0,Default,,0,0,0,,\t\t祝大家新年快乐\t\\N{\\rDefault-JP}\t\tHappy New Year";
        let ev = parse_embedded_dialogue(raw, 4.1, 7.86).expect("parses");
        assert_eq!(ev.base.style, "Default");
        assert_eq!(ev.base.text, "祝大家新年快乐\nHappy New Year");
        assert_eq!(ev.base.start_time, 4.1);
        assert_eq!(ev.base.end_time, 7.86);
        assert_eq!(ev.layer, 0);
        // 覆盖标签段保留给渲染端求值（样式复位标签暂不逐段生效，但不能再出现在文本里）
        assert!(!ev.segments.is_empty(), "覆盖标签段应保留");
    }

    /// 带 `Dialogue:` 前缀与内联时间戳的布局（如 .ass 裸流经 FFmpeg 封包）
    #[test]
    fn embedded_dialogue_handles_canonical_prefix_and_times() {
        let raw = "Dialogue: 2,0:00:01.00,0:00:05.00,Graphics,,0,0,0,,第一行";
        // 时间轴以包为准（容器给的 pts/duration），Layer 仍取首字段
        let ev = parse_embedded_dialogue(raw, 9.0, 11.5).expect("parses");
        assert_eq!(ev.layer, 2);
        assert_eq!(ev.base.style, "Graphics");
        assert_eq!(ev.base.text, "第一行");
        assert_eq!((ev.base.start_time, ev.base.end_time), (9.0, 11.5));
    }

    /// 事件级边距要带给渲染端（0 = 交给样式表）
    #[test]
    fn embedded_dialogue_reads_event_margins() {
        let raw = "0,0,Default,,10,20,30,,正文";
        let ev = parse_embedded_dialogue(raw, 0.0, 1.0).expect("parses");
        assert_eq!((ev.margin_l, ev.margin_r, ev.margin_v), (10.0, 20.0, 30.0));
    }

    /// 正文里的逗号属于文本，不能被当字段分隔
    #[test]
    fn embedded_dialogue_keeps_commas_in_text() {
        let raw = "3,0,Default,,0,0,0,,你好，世界,再见";
        let ev = parse_embedded_dialogue(raw, 0.0, 1.0).expect("parses");
        assert_eq!(ev.base.text, "你好，世界,再见");
    }

    /// muxer 会把脚本头也当字幕包投递：字段对不上就不许当字幕
    #[test]
    fn embedded_dialogue_rejects_script_header_lines() {
        for raw in [
            "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text",
            "[Events]",
            "Title: 只有一行",
            "0,0",
        ] {
            assert!(
                parse_embedded_dialogue(raw, 0.0, 1.0).is_none(),
                "脚本头/残行不得被解析成字幕：{raw:?}"
            );
        }
    }

    /// 纯覆盖标签（无正文）的事件不应进入事件表
    #[test]
    fn embedded_dialogue_drops_empty_text() {
        assert!(parse_embedded_dialogue("0,0,Default,,0,0,0,,{\\pos(1,2)}", 0.0, 1.0).is_none());
    }
}
