//! 由 `dyl-danmaku` 声明的弹幕能力值（规范 ADR-13）。

/// `features.danmaku_formats` 的值：B 站 XML（DFM 布局的输入）。
///
/// `parser_for_extension` 里还有一个 `JsonParser`，但它没有真文件验收过，也不在
/// DFM 对齐范围内，所以不写进能力位 —— 声明的是已验证集合，不是"代码里存在过的分支"。
pub fn formats() -> Vec<String> {
    vec!["bilibili_xml".to_string()]
}

/// XML 文本实体是否已反转义（`&amp;` / `&lt;` / `&#39;` / `&#x4F60;`）。
///
/// 实现见 `parser::unescape_xml_text`；下面的测试用真实解析路径钉住这条声明。
pub const XML_ENTITIES_UNESCAPED: bool = true;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{BilibiliXmlParser, DanmakuParser};

    #[test]
    fn declared_format_is_the_parser_that_unescapes_entities() {
        let xml =
            r#"<i><d p="1.0,1,25,16777215,0,0,u,0">a &amp; b &lt;tag&gt; &#x4F60;&#22909;</d></i>"#;
        let items = BilibiliXmlParser::new().parse(xml.as_bytes());
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].text, "a & b <tag> 你好",
            "能力位声明实体已反转义，解析结果必须真的反转义"
        );
    }

    #[test]
    fn formats_are_declared_in_the_contract_vocabulary() {
        assert_eq!(formats(), vec!["bilibili_xml".to_string()]);
    }
}
