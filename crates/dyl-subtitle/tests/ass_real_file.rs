//! 真实 Danmaku2ASS 素材回归：001.ass 必须解析出样式表、PlayRes 与 \move 特效段

use dyl_render::Overlay;
use dyl_subtitle::{AssParser, SubtitleConfig, SubtitleEngine, SubtitleParser};

fn sample_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../001.ass")
        .canonicalize()
        .unwrap_or_default()
}

#[test]
fn test_real_danmaku_ass_document() {
    let path = sample_path();
    if !path.exists() {
        eprintln!("skip: {} not present", path.display());
        return;
    }
    let data = std::fs::read(&path).unwrap();
    let doc = AssParser::new().parse_document(&data);

    assert_eq!(doc.play_res_x, 1920.0);
    assert_eq!(doc.play_res_y, 1080.0);
    let style = doc.styles.get("Default").expect("Default style");
    assert_eq!(style.font_size, 40.0);
    assert!(!doc.events.is_empty());

    // 每行几乎都带 \move；残缺标签 {\move(..)&H1BFBFF} 也必须解析成功
    let moved = doc
        .events
        .iter()
        .filter(|e| e.segments.iter().any(|(t, _)| t.move_.is_some()))
        .count();
    assert!(
        moved >= doc.events.len() * 9 / 10,
        "moved={moved} of {}",
        doc.events.len()
    );
}

#[test]
fn test_real_danmaku_ass_engine_update_no_panic() {
    let path = sample_path();
    if !path.exists() {
        eprintln!("skip: {} not present", path.display());
        return;
    }
    let engine = SubtitleEngine::new(SubtitleConfig::default());
    engine
        .load_from_file(path.to_str().unwrap())
        .expect("load 001.ass");

    let mut engine = engine;
    let mut with_text = 0usize;
    for i in 0..20 {
        let pts = 1.0 + i as f64;
        Overlay::update(&mut engine, pts);
        if !engine.shared_state().lock().current_text.is_empty() {
            with_text += 1;
        }
    }
    assert!(with_text > 0, "expected active subtitles in 1..21s window");
}

/// SRT 解析器向后兼容：AssParser::parse 仍返回纯文本事件
#[test]
fn test_ass_parser_plain_view() {
    let src = r#"[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,{\move(1,2,3,4)\an7}hello{\c&HFF0000&} world
"#;
    let events = AssParser::new().parse(src.as_bytes());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].text, "hello world");
}
