//! 見た目をそろえるための小さな部品

use eframe::egui::{self, Align2, Color32, FontFamily, FontId, Rect, Response, Sense, Stroke, Ui, vec2};

pub use egui_phosphor::regular as icon;

use crate::session::Session;
use crate::theme::*;

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("ui-bold".into()))
}

/// 枠のないアイコンだけのボタン
pub fn icon_button(ui: &mut Ui, glyph: &str, tip: &str, active: bool) -> Response {
    icon_button_colored(ui, glyph, tip, active, None)
}

pub fn icon_button_colored(ui: &mut Ui, glyph: &str, tip: &str, active: bool, tint: Option<Color32>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let p = ui.painter();
    let bg = if active {
        ACCENT.gamma_multiply(0.16)
    } else if resp.hovered() {
        CARD_SEL
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, 6.0, bg);
    let color = match (active, resp.hovered(), tint) {
        (true, _, _) => ACCENT,
        (_, true, Some(t)) => t,
        (_, true, None) => TEXT,
        (_, false, Some(t)) => t.gamma_multiply(0.75),
        _ => MUTED,
    };
    p.text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(16.0), color);
    resp.on_hover_text(tip)
}

/// セッションの状態を小さな印で描く
pub fn status_dot(ui: &mut Ui, s: &Session) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    match s.exit {
        Some(code) => {
            let (g, col) = if code == 0 { (icon::CHECK, OK) } else { (icon::X, BAD) };
            p.text(c, Align2::CENTER_CENTER, g, FontId::proportional(12.0), col);
        }
        None if s.attention => {
            p.circle_filled(c, 4.5, WAITING);
            p.circle_stroke(c, 6.5, Stroke::new(1.0, WAITING.gamma_multiply(0.35)));
        }
        None if s.busy => {
            // ゆっくり明滅させる
            let t = ui.input(|i| i.time);
            let a = 0.55 + 0.45 * ((t * 3.0).sin() as f32 + 1.0) / 2.0;
            p.circle_filled(c, 4.0, ACCENT.gamma_multiply(a));
            ui.ctx().request_repaint();
        }
        None => {
            p.circle_stroke(c, 3.5, Stroke::new(1.2, FAINT));
        }
    }
    resp
}

pub fn status_text(s: &Session) -> (&'static str, Color32) {
    match s.exit {
        Some(0) => (l!("終了", "Exited"), OK),
        Some(_) => (l!("エラーで終了", "Exited with error"), BAD),
        None if s.attention => (l!("待っています", "Waiting"), WAITING),
        None if s.busy => (l!("動いています", "Running"), ACCENT),
        None => (l!("静か", "Idle"), MUTED),
    }
}

/// ウィンドウをつかんで動かせる場所（タイトルバーの代わり）
pub fn drag_area(ui: &mut Ui, rect: Rect, id: &str) {
    let resp = ui.interact(rect, ui.id().with(id), Sense::click_and_drag());
    if resp.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if resp.double_clicked() {
        let max = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
    }
}

/// 細い区切り線
pub fn hairline(ui: &mut Ui) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, HAIRLINE));
    ui.add_space(1.0);
}
