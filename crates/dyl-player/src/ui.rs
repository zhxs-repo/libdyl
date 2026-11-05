//! egui 控件层：HUD、播放控制条、拖放/路径加载提示。
//!
//! 只读 `Snapshot`，所有修改通过 `UiAction::Command` 交回 App，不直接碰引擎。

use std::sync::Arc;
use std::time::{Duration, Instant};

use dyl_core::contract::{
    Command, DecodeBackend, DecodeMode, DecodeSnapshot, PlayerState, SeekMode, Snapshot, TrackInfo,
    TrackKind,
};

const STATUS_TTL: Duration = Duration::from_secs(6);

pub struct UiState {
    /// 拖动进度条时的预览位置；松手后清空
    pub seek_preview_ms: Option<u64>,
    pub path_input: String,
    pub status: String,
    pub status_until: Option<Instant>,
    pub rfps: f32,
    pub stats_at: Instant,
    pub stats_frames: u64,
    pub fullscreen: bool,
    pub show_help: bool,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            seek_preview_ms: None,
            path_input: String::new(),
            status: String::new(),
            status_until: None,
            rfps: 0.0,
            stats_at: Instant::now(),
            stats_frames: 0,
            fullscreen: false,
            show_help: false,
        }
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.status_until = Some(Instant::now() + STATUS_TTL);
    }

    pub fn status_text(&self) -> Option<&str> {
        if self.status.is_empty() {
            return None;
        }
        match self.status_until {
            Some(deadline) if Instant::now() >= deadline => None,
            _ => Some(&self.status),
        }
    }
}

pub enum UiAction {
    Command(Command),
    ToggleFullscreen,
    /// 换主题必须顺带请求一次重绘：暂停时没有下一帧，面板色会停在上一个主题。
    SetTheme(egui::ThemePreference),
}

pub fn draw(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    hw_decode: bool,
    state: &mut UiState,
    actions: &mut Vec<UiAction>,
) {
    let palette = Palette::from(ui.visuals());
    egui::Panel::top("dyl_hud")
        .frame(palette.panel_frame())
        .show(ui, |ui| draw_hud(ui, snapshot, state, &palette));
    egui::Panel::bottom("dyl_controls")
        .frame(palette.panel_frame())
        .show(ui, |ui| {
            draw_controls(ui, snapshot, hw_decode, state, actions)
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| draw_center(ui, snapshot, state, &palette));
}

/// 面板与文字颜色。
///
/// 面板压在画面上，底色和文字必须成套：写死的半透明黑底配上浅色主题的深字，
/// 就是「浅色主题下文本不好识别」的直接原因。
struct Palette {
    dark: bool,
    panel_fill: egui::Color32,
    panel_stroke: egui::Color32,
    text: egui::Color32,
    status: egui::Color32,
    error: egui::Color32,
    /// 画面中央提示的底色：亮画面和暗画面都要能读，所以自带一层不透明底
    scrim: egui::Color32,
    scrim_text: egui::Color32,
}

impl From<&egui::Visuals> for Palette {
    fn from(visuals: &egui::Visuals) -> Self {
        let dark = visuals.dark_mode;
        // 半透明是为了控制条压住画面时仍能看见内容；浅色下不透明度要更高，
        // 否则亮画面会把浅底冲淡。
        let [r, g, b, _] = visuals.panel_fill.to_srgba_unmultiplied();
        let alpha = if dark { 200 } else { 235 };
        let panel_fill = egui::Color32::from_rgba_unmultiplied(r, g, b, alpha);
        Self {
            dark,
            panel_fill,
            panel_stroke: visuals.widgets.noninteractive.bg_stroke.color,
            text: visuals.text_color(),
            status: away_from_panel(visuals.warn_fg_color, dark),
            error: away_from_panel(visuals.error_fg_color, dark),
            scrim: if dark {
                egui::Color32::from_black_alpha(190)
            } else {
                egui::Color32::from_white_alpha(225)
            },
            scrim_text: visuals.strong_text_color(),
        }
    }
}

impl Palette {
    fn panel_frame(&self) -> egui::Frame {
        egui::Frame::new()
            .fill(self.panel_fill)
            // 细边：浅底贴着亮画面时，靠边框才分得清面板和画面
            .stroke(egui::Stroke::new(1.0, self.panel_stroke))
            .inner_margin(egui::Margin::symmetric(10, 6))
    }

    /// 状态色本来就是为深色面板挑的亮色：深色底原样保留，浅色底才压暗。
    fn tint(&self, color: egui::Color32) -> egui::Color32 {
        if self.dark {
            color
        } else {
            away_from_panel(color, false)
        }
    }
}

/// 把语义色推到面板底色的另一侧。
///
/// egui 的 `error_fg_color` 是纯红：浅灰面板上读得清，落在深灰面板上亮度只差 0.19，
/// 深色主题同样看不清，所以两个方向都要拉开。
fn away_from_panel(color: egui::Color32, dark: bool) -> egui::Color32 {
    if dark {
        color.lerp_to_gamma(egui::Color32::from_gray(245), 0.3)
    } else {
        color.lerp_to_gamma(egui::Color32::from_gray(25), 0.4)
    }
}

fn draw_hud(ui: &mut egui::Ui, snapshot: &Snapshot, state: &UiState, palette: &Palette) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(state_label(snapshot.state))
                .strong()
                .color(state_color(palette, snapshot.state)),
        );
        ui.separator();
        ui.monospace(format!(
            "{} / {}",
            format_time(snapshot.position_ms),
            format_time(snapshot.duration_ms)
        ));

        if let Some(media) = &snapshot.media {
            if let Some(video) = &media.video {
                ui.separator();
                ui.label(format!(
                    "{}x{} {:.2}fps {}",
                    video.width, video.height, video.fps, video.codec
                ));
            }
            if let Some(audio) = &media.audio {
                ui.separator();
                ui.label(format!(
                    "{} {}Hz {}ch",
                    audio.codec, audio.sample_rate, audio.channels
                ));
            }
        }

        ui.separator();
        ui.label(decode_label(&snapshot.decode));
        ui.separator();
        ui.label(format!("rfps {:.1}", state.rfps));
        ui.separator();
        ui.label(format!(
            "渲染 {} / 丢帧 {}",
            snapshot.stats.rendered_frames, snapshot.stats.dropped_frames
        ));

        if snapshot.subtitle.loaded {
            ui.separator();
            ui.label(if snapshot.subtitle.visible {
                "字幕开"
            } else {
                "字幕关"
            });
        }
        if snapshot.danmaku.loaded {
            ui.separator();
            ui.label(format!("弹幕 {} 条", snapshot.danmaku.count));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let source = snapshot
                .source
                .as_deref()
                .map(file_name)
                .unwrap_or_else(|| "未加载".to_string());
            ui.add(egui::Label::new(source).truncate());
        });
    });

    if let Some(status) = state.status_text() {
        ui.label(egui::RichText::new(status).small().color(palette.status));
    } else if let Some(error) = &snapshot.last_error {
        ui.label(
            egui::RichText::new(format!("{}: {}", error.code.as_str(), error.message))
                .small()
                .color(palette.error),
        );
    }
}

fn draw_controls(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    hw_decode: bool,
    state: &mut UiState,
    actions: &mut Vec<UiAction>,
) {
    let duration_ms = snapshot.duration_ms;
    let seekable = duration_ms > 0;
    let mut preview = state
        .seek_preview_ms
        .unwrap_or(snapshot.position_ms)
        .min(duration_ms);
    let mut value = preview as f64;

    ui.horizontal(|ui| {
        ui.monospace(format_time(preview));
        let slider_width = (ui.available_width() - 96.0).max(80.0);
        ui.spacing_mut().slider_width = slider_width;
        let response = ui.add_enabled(
            seekable,
            egui::Slider::new(&mut value, 0.0..=duration_ms as f64).show_value(false),
        );
        if response.changed() {
            preview = value.round() as u64;
            state.seek_preview_ms = Some(preview);
        }
        if response.drag_stopped() && seekable {
            state.seek_preview_ms = None;
            actions.push(UiAction::Command(Command::Seek {
                position_ms: value.round() as u64,
                mode: SeekMode::Exact,
            }));
        }
        ui.monospace(format_time(duration_ms));
    });

    ui.horizontal_wrapped(|ui| {
        let playing = snapshot.state == PlayerState::Playing;
        let can_play = !matches!(
            snapshot.state,
            PlayerState::Idle | PlayerState::Loading | PlayerState::Error
        );
        if ui
            .add_enabled(
                can_play,
                egui::Button::new(if playing { "暂停" } else { "播放" }),
            )
            .clicked()
        {
            actions.push(UiAction::Command(if playing {
                Command::Pause
            } else {
                Command::Play
            }));
        }
        if ui.button("停止").clicked() {
            actions.push(UiAction::Command(Command::Stop));
        }

        ui.separator();

        let mut volume = snapshot.volume as i32;
        if ui
            .add(egui::Slider::new(&mut volume, 0..=100).text("音量"))
            .changed()
        {
            actions.push(UiAction::Command(Command::SetVolume {
                volume: volume as u8,
            }));
        }
        let mut muted = snapshot.muted;
        if ui.checkbox(&mut muted, "静音").changed() {
            actions.push(UiAction::Command(Command::SetMute { muted }));
        }

        ui.separator();

        let mut subtitle = snapshot.subtitle.visible;
        if ui.checkbox(&mut subtitle, "字幕").changed() {
            actions.push(UiAction::Command(Command::SetSubtitleVisible {
                visible: subtitle,
            }));
        }
        let mut danmaku = snapshot.danmaku.visible;
        if ui.checkbox(&mut danmaku, "弹幕").changed() {
            actions.push(UiAction::Command(Command::SetDanmakuVisible {
                visible: danmaku,
            }));
        }
        let mut opacity = snapshot.danmaku.opacity as i32;
        if ui
            .add(egui::Slider::new(&mut opacity, 0..=100).text("不透明度"))
            .changed()
        {
            actions.push(UiAction::Command(Command::SetDanmakuOpacity {
                opacity: opacity as u8,
            }));
        }

        ui.separator();
        draw_track_menus(ui, snapshot, actions);

        ui.separator();
        draw_decode_menu(ui, snapshot, hw_decode, actions);

        ui.separator();
        let theme = ui.ctx().options(|options| options.theme_preference);
        if ui.button(theme_label(theme)).clicked() {
            actions.push(UiAction::SetTheme(next_theme(theme)));
        }
        if ui
            .button(if state.fullscreen {
                "退出全屏"
            } else {
                "全屏"
            })
            .clicked()
        {
            actions.push(UiAction::ToggleFullscreen);
        }
    });

    ui.horizontal(|ui| {
        ui.label("文件");
        let width = (ui.available_width() - 72.0).max(140.0);
        let edit = ui.add_sized(
            [width, 22.0],
            egui::TextEdit::singleline(&mut state.path_input)
                .hint_text("拖入 MP4 / ASS / XML，或输入本地路径"),
        );
        let submitted = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("加载").clicked() || submitted {
            let path = state.path_input.trim().to_string();
            if !path.is_empty() {
                actions.push(UiAction::Command(Command::LoadFile { url: path }));
            }
        }
    });
}

/// 音轨与字幕轨菜单，数据源就是 `media.tracks`。
///
/// 视频轨不列：内核对 `kind: video` 直接 `not_supported`，列出来等于撒谎。
/// 位图字幕轨列出来但禁用：内核只解码文本字幕。
fn draw_track_menus(ui: &mut egui::Ui, snapshot: &Snapshot, actions: &mut Vec<UiAction>) {
    let Some(media) = &snapshot.media else {
        return;
    };
    // 换轨要重开解码管线并经过 loading，中途再点只会拿到 superseded/busy
    let switching = snapshot.state == PlayerState::Loading;

    for (kind, title) in [(TrackKind::Audio, "音轨"), (TrackKind::Subtitle, "字幕轨")] {
        let tracks: Vec<&TrackInfo> = media
            .tracks
            .iter()
            .filter(|track| track.kind == kind)
            .collect();
        if tracks.len() > 1 {
            draw_track_menu(ui, title, &tracks, switching, actions);
        }
    }
}

fn draw_track_menu(
    ui: &mut egui::Ui,
    title: &str,
    tracks: &[&TrackInfo],
    switching: bool,
    actions: &mut Vec<UiAction>,
) {
    let selected = tracks
        .iter()
        .find(|track| track.selected)
        .map(|track| track_label(track))
        .unwrap_or_else(|| "未选".to_string());

    let combo = egui::ComboBox::from_id_salt(("dyl_track_menu", title)).selected_text(selected);
    ui.add_enabled_ui(!switching, |ui| {
        combo.show_ui(ui, |ui| {
            ui.label(egui::RichText::new(title).small().weak());
            for track in tracks {
                let switchable = track_switchable(track);
                let response = ui.add_enabled(
                    switchable,
                    egui::Button::selectable(track.selected, track_label(track)),
                );
                if response.clicked() {
                    actions.push(UiAction::Command(Command::SelectTrack {
                        kind: track.kind,
                        index: track.index,
                    }));
                } else if !switchable {
                    response.on_hover_text("位图字幕：内核不解码，无法切换");
                }
            }
        });
    });
}

/// 解码菜单。`hw_decode` 为假时整个菜单不出现：内核的同步校验会直接拒绝
/// `set_decode_mode auto`（`not_supported`），摆出来等于撒谎。
fn draw_decode_menu(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    hw_decode: bool,
    actions: &mut Vec<UiAction>,
) {
    if !hw_decode {
        return;
    }
    // 换解码方式要重开管线并经过 loading，中途再点只会被矩阵拒绝
    let loading = snapshot.state == PlayerState::Loading;
    let selected = decode_mode_label(snapshot.decode.mode);
    ui.add_enabled_ui(!loading, |ui| {
        egui::ComboBox::from_id_salt("dyl_decode_menu")
            .selected_text(format!("解码: {selected}"))
            .show_ui(ui, |ui| {
                ui.label(egui::RichText::new("解码").small().weak());
                for mode in [DecodeMode::Software, DecodeMode::Auto] {
                    let response = ui.add(egui::Button::selectable(
                        snapshot.decode.mode == mode,
                        decode_mode_label(mode),
                    ));
                    if response.clicked() {
                        actions.push(UiAction::Command(Command::SetDecodeMode { mode }));
                    }
                }
            });
    });
}

fn decode_mode_label(mode: DecodeMode) -> &'static str {
    match mode {
        DecodeMode::Software => "软解",
        DecodeMode::Auto => "硬解（自动）",
    }
}

/// HUD 只陈述事实：`backend` 是活管线实际走的路径，没开管线时就是软解。
fn decode_label(decode: &DecodeSnapshot) -> String {
    match (decode.backend, decode.device.as_deref()) {
        (DecodeBackend::Hardware, Some(device)) => format!("硬解 {device}"),
        (DecodeBackend::Hardware, None) => "硬解".to_string(),
        (DecodeBackend::Software, _) => "软解".to_string(),
    }
}

/// 内核的文本字幕门（`SubtitleCodecId::is_text`）对认不出的 codec 统一报 `unknown`，
/// 所以这里按 codec 标签判断，位图字幕（PGS/DVD/DVB/…）一律不可切。
fn track_switchable(track: &TrackInfo) -> bool {
    match track.kind {
        TrackKind::Video => false,
        TrackKind::Audio => true,
        TrackKind::Subtitle => {
            matches!(
                track.codec.as_str(),
                "ass" | "ssa" | "subrip" | "srt" | "mov_text"
            )
        }
    }
}

fn track_label(track: &TrackInfo) -> String {
    let mut parts = vec![format!("#{}", track.index), track.codec.clone()];
    if let Some(language) = track.language.as_deref().filter(|lang| !lang.is_empty()) {
        parts.push(language.to_string());
    }
    if track.default {
        parts.push("默认".to_string());
    }
    let meta = parts.join(" · ");
    match track
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        Some(title) => format!("{title}（{meta}）"),
        None => meta,
    }
}

fn theme_label(theme: egui::ThemePreference) -> &'static str {
    match theme {
        egui::ThemePreference::System => "主题: 系统",
        egui::ThemePreference::Light => "主题: 浅色",
        egui::ThemePreference::Dark => "主题: 深色",
    }
}

/// 面板压在画面上，深色还是浅色能读取决于画面亮度，所以给一次手动切换的机会。
fn next_theme(theme: egui::ThemePreference) -> egui::ThemePreference {
    match theme {
        egui::ThemePreference::System => egui::ThemePreference::Light,
        egui::ThemePreference::Light => egui::ThemePreference::Dark,
        egui::ThemePreference::Dark => egui::ThemePreference::System,
    }
}

fn draw_center(ui: &mut egui::Ui, snapshot: &Snapshot, state: &UiState, palette: &Palette) {
    let hint = match snapshot.state {
        PlayerState::Idle | PlayerState::Stopped => {
            Some("拖入 MP4 视频文件，或在下方的路径框输入后点击「加载」".to_string())
        }
        PlayerState::Loading => Some("加载中…".to_string()),
        PlayerState::Error => snapshot
            .last_error
            .as_ref()
            .map(|e| format!("{}: {}", e.code.as_str(), e.message)),
        _ => None,
    };
    if let Some(text) = hint {
        centered_text(ui, &text, 18.0, palette);
    }

    if state.show_help {
        centered_text(
            ui,
            "空格 播放/暂停 · ←/→ 快退/快进 5s · F 全屏 · M 静音 · H 帮助 · Esc 退出",
            15.0,
            palette,
        );
    }
}

fn centered_text(ui: &mut egui::Ui, text: &str, size: f32, palette: &Palette) {
    let rect = ui.available_rect_before_wrap();
    let center = rect.center();
    let font = egui::FontId::proportional(size);
    // 这段文字直接压在画面上，所以自带一层底：底色和文字都取自当前主题，
    // 亮画面和暗画面都能读。
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font.clone(), palette.scrim_text);
    let backdrop = egui::Rect::from_center_size(center, galley.size() + egui::vec2(24.0, 14.0));
    ui.painter().rect_filled(backdrop, 6.0, palette.scrim);
    ui.painter().text(
        center,
        egui::Align2::CENTER_CENTER,
        text,
        font,
        palette.scrim_text,
    );
}

fn state_label(state: PlayerState) -> &'static str {
    match state {
        PlayerState::Idle => "空闲",
        PlayerState::Loading => "加载中",
        PlayerState::Ready => "就绪",
        PlayerState::Playing => "播放中",
        PlayerState::Paused => "已暂停",
        PlayerState::Ended => "播放结束",
        PlayerState::Stopped => "已停止",
        PlayerState::Error => "错误",
    }
}

/// 状态色只在深色底上直接用亮版本；浅色底要压深，否则浅灰面板上的黄/绿几乎糊成一片。
fn state_color(palette: &Palette, state: PlayerState) -> egui::Color32 {
    match state {
        PlayerState::Playing => palette.tint(egui::Color32::from_rgb(120, 220, 140)),
        PlayerState::Paused | PlayerState::Ready => {
            palette.tint(egui::Color32::from_rgb(236, 198, 96))
        }
        PlayerState::Loading => palette.tint(egui::Color32::from_rgb(130, 200, 240)),
        PlayerState::Error => palette.tint(egui::Color32::from_rgb(250, 120, 120)),
        _ => palette.text,
    }
}

pub fn format_time(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn file_name(source: &str) -> String {
    std::path::Path::new(source)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.to_string())
}

/// `DYL_UI_THEME=light|dark|system` 强制主题，默认跟随系统。
///
/// 系统主题未必是你要看的那一套；面板配色改动需要能分别跑一遍。
pub fn apply_env_theme(ctx: &egui::Context) {
    let Ok(value) = std::env::var("DYL_UI_THEME") else {
        return;
    };
    let theme = match value.trim().to_ascii_lowercase().as_str() {
        "light" => egui::ThemePreference::Light,
        "dark" => egui::ThemePreference::Dark,
        "system" | "" => egui::ThemePreference::System,
        other => {
            tracing::warn!("DYL_UI_THEME={other:?} 无法识别，改为跟随系统");
            egui::ThemePreference::System
        }
    };
    tracing::info!("UI theme: {theme:?}");
    ctx.set_theme(theme);
}

/// egui 自带字体没有 CJK 字形；找一份系统字体装到两个族的最前面。
///
/// 优先读 `DYL_UI_FONT`，便于在没有中文字体的机器上指定。
pub fn install_cjk_font(ctx: &egui::Context) {
    let mut candidates: Vec<(String, u32)> = Vec::new();
    if let Ok(path) = std::env::var("DYL_UI_FONT") {
        candidates.push((path, 0));
    }
    candidates.extend(
        [
            ("C:\\Windows\\Fonts\\msyh.ttc", 0),
            ("C:\\Windows\\Fonts\\msyh.ttf", 0),
            ("C:\\Windows\\Fonts\\simhei.ttf", 0),
            ("C:\\Windows\\Fonts\\simsun.ttc", 0),
            ("/System/Library/Fonts/PingFang.ttc", 0),
            ("/System/Library/Fonts/STHeiti Light.ttc", 0),
            ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
            ("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc", 0),
        ]
        .iter()
        .map(|(path, index)| (path.to_string(), *index)),
    );

    for (path, index) in candidates {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let mut font = egui::FontData::from_owned(bytes);
        font.index = index;
        let mut definitions = egui::FontDefinitions::default();
        definitions
            .font_data
            .insert("dyl_cjk".to_owned(), Arc::new(font));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            definitions
                .families
                .entry(family)
                .or_default()
                .insert(0, "dyl_cjk".to_owned());
        }
        ctx.set_fonts(definitions);
        tracing::info!("UI font loaded: {path} (face {index})");
        return;
    }
    tracing::warn!("no CJK font found; Chinese text may render as placeholder boxes");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(kind: TrackKind, codec: &str) -> TrackInfo {
        TrackInfo {
            index: 3,
            kind,
            codec: codec.to_string(),
            language: None,
            title: None,
            default: false,
            selected: false,
        }
    }

    /// 面板文字必须和面板底色拉开亮度，否则就是「浅色主题下文本不好识别」。
    #[test]
    fn panel_text_contrast_holds_in_both_themes() {
        for visuals in [egui::Visuals::light(), egui::Visuals::dark()] {
            let palette = Palette::from(&visuals);
            let panel = palette.panel_fill.intensity();
            assert!(
                (palette.text.intensity() - panel).abs() > 0.3,
                "{:?} 主题下面板底色与正文亮度太接近",
                if visuals.dark_mode { "dark" } else { "light" }
            );
            for (name, color) in [("status", palette.status), ("error", palette.error)] {
                assert!(
                    (color.intensity() - panel).abs() > 0.25,
                    "{:?} 主题下 {name} 文字与面板底色对比不足",
                    if visuals.dark_mode { "dark" } else { "light" }
                );
            }
            for state in [
                PlayerState::Playing,
                PlayerState::Paused,
                PlayerState::Loading,
                PlayerState::Error,
            ] {
                let color = state_color(&palette, state);
                assert!(
                    (color.intensity() - panel).abs() > 0.25,
                    "{:?} 主题下 {state:?} 状态色与面板底色对比不足",
                    if visuals.dark_mode { "dark" } else { "light" }
                );
            }
        }
    }

    #[test]
    fn light_panel_is_light_and_dark_panel_is_dark() {
        assert!(
            Palette::from(&egui::Visuals::light())
                .panel_fill
                .intensity()
                > 0.5
        );
        assert!(Palette::from(&egui::Visuals::dark()).panel_fill.intensity() < 0.5);
    }

    #[test]
    fn only_text_subtitle_tracks_are_switchable() {
        for codec in ["ass", "ssa", "subrip", "srt", "mov_text"] {
            assert!(track_switchable(&track(TrackKind::Subtitle, codec)));
        }
        // 内核把认不出的字幕 codec 统一报 unknown，位图字幕（PGS/DVD/DVB）就在这里
        assert!(!track_switchable(&track(TrackKind::Subtitle, "unknown")));
        assert!(!track_switchable(&track(
            TrackKind::Subtitle,
            "hdmv_pgs_subtitle"
        )));
        assert!(track_switchable(&track(TrackKind::Audio, "flac")));
        assert!(!track_switchable(&track(TrackKind::Video, "h264")));
    }

    #[test]
    fn track_label_prefers_title_and_keeps_metadata() {
        let mut info = track(TrackKind::Audio, "flac");
        info.index = 2;
        info.language = Some("jpn".to_string());
        info.default = true;
        assert_eq!(track_label(&info), "#2 · flac · jpn · 默认");

        info.title = Some("评论音轨".to_string());
        assert_eq!(track_label(&info), "评论音轨（#2 · flac · jpn · 默认）");
    }

    #[test]
    fn decode_label_reports_the_actual_backend_not_the_setting() {
        let hardware = DecodeSnapshot {
            mode: DecodeMode::Auto,
            backend: DecodeBackend::Hardware,
            device: Some("d3d11va".to_string()),
        };
        assert_eq!(decode_label(&hardware), "硬解 d3d11va");

        let anonymous = DecodeSnapshot {
            mode: DecodeMode::Auto,
            backend: DecodeBackend::Hardware,
            device: None,
        };
        assert_eq!(decode_label(&anonymous), "硬解");

        // 设了 auto 但实际回落软件：HUD 必须说实话
        let fallback = DecodeSnapshot {
            mode: DecodeMode::Auto,
            backend: DecodeBackend::Software,
            device: None,
        };
        assert_eq!(decode_label(&fallback), "软解");
        assert_eq!(decode_mode_label(DecodeMode::Software), "软解");
        assert_eq!(decode_mode_label(DecodeMode::Auto), "硬解（自动）");
    }

    #[test]
    fn theme_choice_cycles_back_to_system() {
        let mut theme = egui::ThemePreference::System;
        for expected in [
            egui::ThemePreference::Light,
            egui::ThemePreference::Dark,
            egui::ThemePreference::System,
        ] {
            theme = next_theme(theme);
            assert_eq!(theme, expected);
        }
    }
}
