mod app;
mod children;
mod config;
mod editor;
mod files;
mod git;
mod keys;
mod map;
mod notify;
mod picker;
mod projects;
mod quickopen;
mod resume;
mod search;
mod session;
mod setup;
mod shot;
mod term;
mod theme;
mod ui;
mod util;
mod vscode;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

const HELP: &str = "\
paispace — コーディングエージェントを並べて動かすデスクトップアプリ

使い方: paispace [-a エージェント] [フォルダ | ワークスペース.code-workspace]

  フォルダ          新しいセッションの最初のフォルダ（省略すると今いる場所）
  .code-workspace   VSCode のワークスペースを開き、そのフォルダから選ぶ
  -a エージェント   起動と同時にそのエージェントを1つ開く（claude / codex / shell など）

設定は ~/.config/paispace/config.toml";

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
                println!("paispace {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-a" | "--agent" => start = Some(args.next().ok_or_else(|| anyhow::anyhow!("-a のあとにエージェント名が要ります"))?),
            _ if dir.is_some() => anyhow::bail!("フォルダは1つだけ指定できます（paispace --help）"),
            _ => dir = Some(PathBuf::from(a)),
        }
    }
    let mut workspace = None;
    if let Some(d) = dir.clone().filter(|d| d.extension().is_some_and(|x| x == "code-workspace")) {
        let f = std::fs::canonicalize(&d).map_err(|_| anyhow::anyhow!("ワークスペースが見つかりません: {}", d.display()))?;
        dir = f.parent().map(|p| p.to_path_buf());
        workspace = Some(f);
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
    let (mut cfg, cfg_err) = config::load();
    if let Some(w) = &workspace {
        let t = util::tilde(w);
        if !cfg.workspaces.contains(&t) {
            cfg.workspaces.insert(0, t);
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("paispace")
            // タイトルバーを消して、中身を一番上まで広げる（信号機ボタンだけ残る）
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false)
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([720.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "paispace",
        options,
        Box::new(move |cc| {
            setup(&cc.egui_ctx);
            Ok(Box::new(app::PaispaceApp::new(cc, cfg, cfg_err, dir, start, workspace)))
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
    let defaults_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let defaults_mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();

    const W3: &str = "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc";
    const W6: &str = "/System/Library/Fonts/ヒラギノ角ゴシック W6.ttc";
    // 画面の文字: 英数字は SF、日本語はヒラギノ（高さを SF にそろえる）
    let jp_ui = egui::FontTweak { y_offset_factor: 0.24, ..Default::default() };
    // ターミナル: SF Mono、日本語は2桁分の幅に近づけて高さもそろえる
    let jp_mono = egui::FontTweak { scale: 1.08, y_offset_factor: 0.16, ..Default::default() };

    let mut ui_fonts: Vec<String> = Vec::new();
    let mut ui_bold: Vec<String> = Vec::new();
    let mut mono: Vec<String> = Vec::new();
    for (name, path, idx, tweak, list) in [
        ("sf", "/System/Library/Fonts/SFNS.ttf", 0, None, 0),
        ("hira", W3, 0, Some(jp_ui), 0),
        ("hira-bold", W6, 0, Some(jp_ui), 1),
        ("sf-mono", "/System/Library/Fonts/SFNSMono.ttf", 0, None, 2),
        ("menlo", "/System/Library/Fonts/Menlo.ttc", 0, None, 2),
        ("hira-mono", W3, 0, Some(jp_mono), 2),
    ] {
        if load(&mut fonts, name, path, idx, tweak) {
            match list {
                0 => ui_fonts.push(name.into()),
                1 => ui_bold.push(name.into()),
                _ => mono.push(name.into()),
            }
        }
    }
    // 記号の予備（SF Mono にない ✻ ⏺ などのため Menlo と Apple Symbols を後ろに置く）
    if load(&mut fonts, "symbols", "/System/Library/Fonts/Apple Symbols.ttf", 0, None) {
        mono.push("symbols".into());
        ui_fonts.push("symbols".into());
    }
    fonts.font_data.insert("phosphor".into(), Arc::new(egui_phosphor::Variant::Regular.font_data()));

    let mut p = ui_fonts.clone();
    p.push("phosphor".into());
    p.extend(defaults_prop.clone());
    fonts.families.insert(FontFamily::Proportional, p);

    // 太字: 英数字は SF（可変フォントの既定の太さ）、日本語はヒラギノ W6
    let mut b = vec!["sf".to_string()];
    b.extend(ui_bold);
    b.extend(ui_fonts);
    b.push("phosphor".into());
    b.extend(defaults_prop);
    fonts.families.insert(FontFamily::Name("ui-bold".into()), b);

    let mut m = mono.clone();
    m.push("phosphor".into());
    m.extend(defaults_mono.clone());
    fonts.families.insert(FontFamily::Monospace, m.clone());
    fonts.families.insert(FontFamily::Name("bold".into()), m);
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = theme::SIDEBAR_BG;
    v.window_fill = theme::RAISED;
    v.window_stroke = egui::Stroke::new(1.0, theme::BORDER);
    v.window_corner_radius = egui::CornerRadius::same(12);
    v.menu_corner_radius = egui::CornerRadius::same(8);
    v.window_shadow = egui::Shadow { offset: [0, 12], blur: 32, spread: 0, color: egui::Color32::from_black_alpha(140) };
    v.popup_shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: egui::Color32::from_black_alpha(120) };
    v.extreme_bg_color = theme::INPUT_BG;
    v.faint_bg_color = theme::HEADER_BG;
    v.override_text_color = None;
    v.hyperlink_color = theme::ACCENT;
    v.selection.bg_fill = theme::ACCENT.gamma_multiply(0.30);
    v.selection.stroke = egui::Stroke::new(1.0, theme::ACCENT);
    v.text_cursor.stroke = egui::Stroke::new(2.0, theme::ACCENT);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = egui::Stroke::new(1.0, theme::HAIRLINE);
    w.noninteractive.fg_stroke = egui::Stroke::new(1.0, theme::TEXT);
    for (st, fill, fg) in [
        (&mut w.inactive, theme::CARD_SEL, theme::TEXT),
        (&mut w.hovered, egui::Color32::from_rgb(0x2b, 0x2f, 0x3a), theme::TEXT),
        (&mut w.active, egui::Color32::from_rgb(0x33, 0x38, 0x46), theme::TEXT),
        (&mut w.open, theme::CARD_SEL, theme::TEXT),
    ] {
        st.weak_bg_fill = fill;
        st.bg_fill = fill;
        st.bg_stroke = egui::Stroke::NONE;
        st.fg_stroke = egui::Stroke::new(1.0, fg);
        st.corner_radius = egui::CornerRadius::same(6);
        st.expansion = 0.0;
    }
    w.inactive.fg_stroke = egui::Stroke::new(1.0, theme::TEXT.gamma_multiply(0.92));
    w.hovered.bg_stroke = egui::Stroke::NONE;
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        use egui::{FontId, TextStyle};
        s.text_styles = [
            (TextStyle::Heading, FontId::new(18.0, FontFamily::Name("ui-bold".into()))),
            (TextStyle::Body, FontId::proportional(13.0)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.interact_size.y = 26.0;
        s.spacing.menu_margin = egui::Margin::same(6);
        s.spacing.window_margin = egui::Margin::same(18);
        s.spacing.scroll = egui::style::ScrollStyle::floating();
        s.interaction.selectable_labels = false;
    });
}
