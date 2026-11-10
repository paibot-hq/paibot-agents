mod app;
mod config;
mod git;
mod keys;
mod notify;
mod session;
mod shot;
mod term;
mod theme;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

const HELP: &str = "\
mure — コーディングエージェントを並べて動かすデスクトップアプリ

使い方: mure [-a エージェント] [フォルダ]

  フォルダ          新しいセッションの最初のフォルダ（省略すると今いる場所）
  -a エージェント   起動と同時にそのエージェントを1つ開く（claude / codex / shell など）

設定は ~/.config/mure/config.toml";

fn main() -> Result<()> {
    let mut dir = None;
    let mut start = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("mure {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-a" | "--agent" => start = Some(args.next().ok_or_else(|| anyhow::anyhow!("-a のあとにエージェント名が要ります"))?),
            _ if dir.is_some() => anyhow::bail!("フォルダは1つだけ指定できます（mure --help）"),
            _ => dir = Some(PathBuf::from(a)),
        }
    }
    let dir = match dir {
        Some(d) => std::fs::canonicalize(&d)
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| anyhow::anyhow!("フォルダが見つかりません: {}", d.display()))?,
        None => {
            // Finder から開くと「/」になるので、そのときはホームにする
            let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
            if cwd == PathBuf::from("/") { std::env::var_os("HOME").map(PathBuf::from).unwrap_or(cwd) } else { cwd }
        }
    };
    let (cfg, cfg_err) = config::load();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mure")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([720.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "mure",
        options,
        Box::new(move |cc| {
            setup(&cc.egui_ctx);
            Ok(Box::new(app::MureApp::new(cc, cfg, cfg_err, dir, start)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("ウィンドウを開けませんでした: {e}"))
}

fn load(fonts: &mut FontDefinitions, name: &str, path: &str, index: u32, tweak: Option<egui::FontTweak>) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut data = FontData::from_owned(bytes);
            data.index = index;
            if let Some(t) = tweak {
                data.tweak = t;
            }
            fonts.font_data.insert(name.into(), Arc::new(data));
            true
        }
        Err(_) => false,
    }
}

fn setup(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut mono = Vec::new();
    let mut mono_bold = Vec::new();
    let mut ui_fonts = Vec::new();
    if load(&mut fonts, "menlo", "/System/Library/Fonts/Menlo.ttc", 0, None) {
        mono.push("menlo".to_string());
    }
    if load(&mut fonts, "menlo-bold", "/System/Library/Fonts/Menlo.ttc", 1, None) {
        mono_bold.push("menlo-bold".to_string());
    }
    // ターミナルの中では、日本語を英数字の高さに合わせ、2桁分の幅に少し近づける
    let cjk = egui::FontTweak { scale: 1.1, y_offset_factor: 0.2, ..Default::default() };
    const W3: &str = "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc";
    const W6: &str = "/System/Library/Fonts/ヒラギノ角ゴシック W6.ttc";
    if load(&mut fonts, "hira", W3, 0, None) {
        ui_fonts.push("hira".to_string());
    }
    if load(&mut fonts, "hira-mono", W3, 0, Some(cjk)) {
        mono.push("hira-mono".into());
    }
    if load(&mut fonts, "hira-mono-bold", W6, 0, Some(cjk)) {
        mono_bold.push("hira-mono-bold".into());
    }
    let defaults_mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let defaults_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();

    let mut m = mono.clone();
    m.extend(defaults_mono.clone());
    fonts.families.insert(FontFamily::Monospace, m);

    let mut b = mono_bold;
    b.extend(mono);
    b.extend(defaults_mono);
    fonts.families.insert(FontFamily::Name("bold".into()), b);

    // 画面の文字はヒラギノを先に（日本語が読みやすい）
    let mut p = ui_fonts;
    p.extend(defaults_prop);
    fonts.families.insert(FontFamily::Proportional, p);
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = theme::SIDEBAR_BG;
    v.window_fill = theme::CARD;
    v.window_stroke = egui::Stroke::new(1.0, theme::BORDER);
    v.override_text_color = Some(theme::TEXT);
    v.selection.bg_fill = theme::ACCENT.gamma_multiply(0.35);
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    v.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
    v.widgets.active.corner_radius = egui::CornerRadius::same(6);
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.interaction.selectable_labels = false;
    });
}
