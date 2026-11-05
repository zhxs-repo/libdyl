//! 由 `dyl-subtitle` 声明的字幕能力值（规范 ADR-13）。
//!
//! `features.subtitle_formats` 描述 `load_subtitle` 能解析的**文件格式**；容器内字幕轨的
//! 编码名是另一套词表，由 `dyl-ffmpeg` 的 `SubtitleCodecId::label()` 给出。两者不要互相比对。

use crate::parser::SUPPORTED_FILE_FORMATS;

/// `features.subtitle_formats` 的值。
pub fn file_formats() -> Vec<String> {
    SUPPORTED_FILE_FORMATS
        .iter()
        .map(|f| (*f).to_string())
        .collect()
}

/// 内嵌 ASS/SSA 轨是否走自建样式路径（按对话行字段解析 + 套用字幕流 CodecPrivate
/// 里的脚本头样式表）。
///
/// 它只代表本项目的 ASS 子集：`\t` / `\blur` / `\k`、同一行内混排样式、`\fn` 逐段换字体
/// 与内嵌字体附件都不在其中（规范 2.3 记为暂缺项）。真值钉在
/// [`parse_embedded_dialogue`](crate::parse_embedded_dialogue) 与
/// [`SubtitleEngine::apply_embedded_script`](crate::SubtitleEngine::apply_embedded_script)
/// 这条实现链上，两者任一失效都必须把它改成 `false`。
pub const EMBEDDED_ASS_STYLING: bool = true;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parser_for_extension;

    /// 每种声明的格式都要能真的解析出一条事件：能力值不是抄来的清单。
    #[test]
    fn every_declared_format_parses_its_own_sample() {
        const SRT_SAMPLE: &str = "1\n00:00:01,000 --> 00:00:02,000\nhello srt\n";
        const ASS_SAMPLE: &str = "[Script Info]\nPlayResX: 640\nPlayResY: 360\n\n\
[V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default,Arial,20\n\n\
[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,hello ass\n";

        for format in SUPPORTED_FILE_FORMATS {
            let sample = match *format {
                "srt" => SRT_SAMPLE,
                "ass" | "ssa" => ASS_SAMPLE,
                // 新加进能力表却没有样例：测试红，逼着补上再声明
                other => panic!("未为声明的格式 {other} 准备解析样例"),
            };
            let events = parser_for_extension(format).parse(sample.as_bytes());
            assert_eq!(events.len(), 1, "格式 {format} 应解析出 1 条事件");
            assert!(
                events[0].text.contains("hello"),
                "格式 {format} 解析出的文本丢了：{:?}",
                events[0].text
            );
        }
    }

    #[test]
    fn declared_formats_are_unique_and_non_empty() {
        for format in SUPPORTED_FILE_FORMATS {
            assert!(!format.is_empty());
        }
        let mut sorted = file_formats();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), SUPPORTED_FILE_FORMATS.len());
    }
}
