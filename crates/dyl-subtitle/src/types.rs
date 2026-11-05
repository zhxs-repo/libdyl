//! 字幕数据模型

use serde::{Deserialize, Serialize};

/// 字幕事件 — 一条字幕的显示时间范围和内容
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleEvent {
    /// 开始时间（秒）
    pub start_time: f64,
    /// 结束时间（秒）
    pub end_time: f64,
    /// 字幕文本（已清除格式标签）
    pub text: String,
    /// 样式名称（ASS 格式中的 Style 字段，SRT 默认为 "Default"）
    pub style: String,
}

/// 字幕轨道信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleTrack {
    /// 轨道索引
    pub index: usize,
    /// 语言标签（如 "eng", "jpn", "chi"）
    pub language: String,
    /// 轨道标题
    pub title: String,
    /// 是否为内嵌字幕（来自容器）或外挂字幕
    pub embedded: bool,
}

/// 字幕配置
#[derive(Debug, Clone)]
pub struct SubtitleConfig {
    /// 画布宽度（像素）
    pub canvas_width: u32,
    /// 画布高度（像素）
    pub canvas_height: u32,
    /// 默认字体大小
    pub default_font_size: u32,
    /// 默认颜色（0xRRGGBB，白色 = 0xFFFFFF）
    pub default_color: u32,
    /// 字幕底部边距（像素）
    pub bottom_margin: u32,
    /// 是否启用字幕
    pub enabled: bool,
}

impl Default for SubtitleConfig {
    fn default() -> Self {
        Self {
            canvas_width: 1920,
            canvas_height: 1080,
            default_font_size: 40,
            default_color: 0xFFFFFF,
            bottom_margin: 50,
            enabled: true,
        }
    }
}

/// ASS 时间格式解析: H:MM:SS.cc → 秒
pub fn parse_ass_time(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hours: f64 = parts[0].parse().ok()?;
    let minutes: f64 = parts[1].parse().ok()?;
    // SS.cc
    let sec_parts: Vec<&str> = parts[2].split('.').collect();
    let seconds: f64 = if sec_parts.len() == 2 {
        let sec: f64 = sec_parts[0].parse().ok()?;
        let centisec: f64 = sec_parts[1].parse().ok()?;
        sec + centisec / 100.0
    } else {
        parts[2].parse().ok()?
    };
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// SRT 时间格式解析: HH:MM:SS,mmm → 秒
pub fn parse_srt_time(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hours: f64 = parts[0].parse().ok()?;
    let minutes: f64 = parts[1].parse().ok()?;
    // SS,mmm
    let sec_parts: Vec<&str> = parts[2].split(',').collect();
    let seconds: f64 = if sec_parts.len() == 2 {
        let sec: f64 = sec_parts[0].parse().ok()?;
        let millisec: f64 = sec_parts[1].parse().ok()?;
        sec + millisec / 1000.0
    } else {
        parts[2].parse().ok()?
    };
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// 清除 ASS 格式标签（{\...} 和大括号内容）
pub fn strip_ass_tags(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '{' => in_tag = true,
            '}' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}

/// ASS 空白规整：连续空白折成单个空格，行首行尾空白去掉
///
/// 内封轨的正文常被 muxer 用真实 TAB 包裹（`\t\t正文\t\N\t\t第二行`），而 TAB 在
/// ASS 排版里没有意义：留着会让渲染端取到缺字形（多数字体下零宽），两个词直接
/// 贴在一起；行首行尾空白还会让居中/右对齐的排版整体偏移。ASS 规格里除 `\h` 外
/// 所有空白字符等价于一个空格，这里就按规格折叠。
pub fn normalize_whitespace(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let mut out = String::with_capacity(line.len());
            let mut pending_space = false;
            for ch in line.chars() {
                if ch == ' ' || ch == '\t' || ch == '\r' {
                    // 行首空白丢弃（out 为空），其余先记账，遇到正文再补一个空格
                    pending_space = !out.is_empty();
                } else {
                    if pending_space {
                        out.push(' ');
                        pending_space = false;
                    }
                    out.push(ch);
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ass_time() {
        assert_eq!(parse_ass_time("0:00:01.00"), Some(1.0));
        assert_eq!(parse_ass_time("1:30:00.50"), Some(5400.5));
        assert_eq!(parse_ass_time("0:05:23.45"), Some(323.45));
    }

    #[test]
    fn test_parse_srt_time() {
        assert_eq!(parse_srt_time("00:00:01,000"), Some(1.0));
        assert_eq!(parse_srt_time("01:30:00,500"), Some(5400.5));
    }

    #[test]
    fn test_strip_ass_tags() {
        assert_eq!(strip_ass_tags(r"{\b1}hello{\b0}"), "hello");
        assert_eq!(strip_ass_tags("no tags"), "no tags");
        assert_eq!(strip_ass_tags(r"{\an8\fs40}text"), "text");
    }

    /// 内封轨正文里的真 TAB 是 muxer padding，不是排版；行内连续空白折一个空格
    #[test]
    fn normalize_whitespace_collapses_control_padding() {
        assert_eq!(
            normalize_whitespace("\t\t祝大家新年快乐\t\n\t\tHappy New Year\t "),
            "祝大家新年快乐\nHappy New Year"
        );
        assert_eq!(normalize_whitespace("a  b\tc"), "a b c");
        assert_eq!(normalize_whitespace("   "), "");
    }

    #[test]
    fn test_subtitle_event() {
        let event = SubtitleEvent {
            start_time: 1.0,
            end_time: 5.0,
            text: "Hello".to_string(),
            style: "Default".to_string(),
        };
        assert_eq!(event.text, "Hello");
    }
}
