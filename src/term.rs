use eframe::egui::{
    self, Align2, Color32, Event, FontFamily, FontId, ImeEvent, Painter, Pos2, Rect, Sense, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::keys;
use crate::session::Session;
use crate::theme::*;

type Cell = (u16, u16);

pub struct TermState {
    /// (行, 桁)
    pub size: (u16, u16),
    pub font_size: f32,
    sel: Option<(Cell, Cell)>,
    preedit: String,
    scroll_acc: f32,
}

impl TermState {
    pub fn new(font_size: f32) -> Self {
        Self { size: (40, 120), font_size, sel: None, preedit: String::new(), scroll_acc: 0.0 }
    }

    pub fn reset(&mut self) {
        self.sel = None;
        self.preedit.clear();
        self.scroll_acc = 0.0;
    }
}

fn ordered(a: Cell, b: Cell) -> (Cell, Cell) {
    if a <= b { (a, b) } else { (b, a) }
}

fn in_sel(sel: Option<(Cell, Cell)>, p: Cell) -> bool {
    match sel {
        Some((a, b)) if a != b => {
            let (a, b) = ordered(a, b);
            p >= a && p <= b
        }
        _ => false,
    }
}

/// ターミナルを描いて入力を送る。大きさが変わったら新しい (行, 桁) を返す
/// accept_input: キーを受け付ける / auto_focus: ほかに何も選ばれていなければ自分が取る / grab: 必ず取る
pub fn show(ui: &mut Ui, s: &mut Session, st: &mut TermState, accept_input: bool, auto_focus: bool, grab: bool) -> Option<(u16, u16)> {
    let font = FontId::monospace(st.font_size);
    let bold = FontId::new(st.font_size, FontFamily::Name("bold".into()));
    let (cw, ch) = ui.fonts(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));

    let outer = ui.available_rect_before_wrap();
    let inner = outer.shrink2(vec2(12.0, 8.0));
    let cols = (inner.width() / cw).floor().max(10.0) as u16;
    let rows = (inner.height() / ch).floor().max(3.0) as u16;

    let id = ui.id().with("terminal");
    let resp = ui.interact(outer, id, Sense::click_and_drag());
    ui.advance_cursor_after_rect(outer);
    if resp.clicked() || resp.drag_started() {
        resp.request_focus();
    }
    if accept_input && (grab || (auto_focus && ui.memory(|m| m.focused().is_none()))) {
        ui.memory_mut(|m| m.request_focus(id));
    }
    let focused = ui.memory(|m| m.has_focus(id));
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
    }

    // マウスで選ぶ
    let to_cell = |p: Pos2| -> Cell {
        let r = ((p.y - inner.top()) / ch).floor().clamp(0.0, (rows - 1) as f32) as u16;
        let c = ((p.x - inner.left()) / cw).round().clamp(0.0, cols as f32) as u16;
        (r, c)
    };
    if resp.drag_started() {
        if let Some(p) = resp.interact_pointer_pos() {
            let c = to_cell(p);
            st.sel = Some((c, c));
        }
    } else if resp.dragged() {
        if let (Some(p), Some(sel)) = (resp.interact_pointer_pos(), st.sel.as_mut()) {
            sel.1 = to_cell(p);
        }
    } else if resp.clicked() {
        st.sel = None;
    }

    // ホイールで履歴をさかのぼる
    if resp.hovered() {
        let dy = ui.input(|i| i.smooth_scroll_delta.y);
        if dy != 0.0 {
            st.scroll_acc += dy;
            let lines = (st.scroll_acc / ch).trunc();
            if lines != 0.0 {
                s.scroll(lines as isize);
                st.scroll_acc -= lines * ch;
                st.sel = None;
            }
        }
    }

    if focused && accept_input {
        input(ui, s, st);
    }

    // 描く
    let painter = ui.painter_at(outer);
    painter.rect_filled(outer, 0.0, TERM_BG);
    let parser = s.parser.lock().unwrap();
    let screen = parser.screen();
    let (srows, scols) = screen.size();
    let cell_rect = |r: u16, c: u16, w: u16| {
        Rect::from_min_size(pos2(inner.left() + c as f32 * cw, inner.top() + r as f32 * ch), vec2(cw * w as f32, ch))
    };

    for r in 0..srows.min(rows) {
        let mut run = String::new();
        let mut run_at = 0u16;
        let mut run_style: Option<(Color32, bool, bool)> = None;
        let mut c = 0u16;
        while c < scols.min(cols) {
            let Some(cell) = screen.cell(r, c) else {
                c += 1;
                continue;
            };
            if cell.is_wide_continuation() {
                c += 1;
                continue;
            }
            let w = if cell.is_wide() { 2 } else { 1 };
            let mut fg = color(cell.fgcolor(), TERM_FG);
            let mut bg = color(cell.bgcolor(), TERM_BG);
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            if in_sel(st.sel, (r, c)) {
                bg = SELECTION;
            }
            if bg != TERM_BG {
                painter.rect_filled(cell_rect(r, c, w), 0.0, bg);
            }
            let mut text = cell.contents();
            if text.is_empty() {
                text.push(' ');
            }
            let style = (fg, cell.bold(), cell.underline());
            let ascii = text.len() == 1 && text.is_ascii();
            if ascii && run_style == Some(style) {
                run.push_str(&text);
            } else {
                flush(&painter, &run, cell_rect(r, run_at, 1).min, run_style, &font, &bold, cw, ch);
                run.clear();
                run_style = None;
                if ascii {
                    run.push_str(&text);
                    run_at = c;
                    run_style = Some(style);
                } else {
                    flush(&painter, &text, cell_rect(r, c, 1).min, Some(style), &font, &bold, cw * w as f32 / text.chars().count().max(1) as f32, ch);
                }
            }
            c += w;
        }
        flush(&painter, &run, cell_rect(r, run_at, 1).min, run_style, &font, &bold, cw, ch);
    }

    // カーソルと変換中の文字
    let scrolled = screen.scrollback();
    let (cr, cc) = screen.cursor_position();
    let cursor = cell_rect(cr, cc, 1);
    if scrolled == 0 && !screen.hide_cursor() {
        if focused {
            painter.rect_filled(cursor, 2.0, CURSOR);
            if let Some(cell) = screen.cell(cr, cc) {
                let t = cell.contents();
                if !t.is_empty() {
                    painter.text(cursor.min, Align2::LEFT_TOP, t, font.clone(), TERM_BG);
                }
            }
        } else {
            painter.rect_stroke(cursor, 2.0, Stroke::new(1.0, CURSOR), StrokeKind::Inside);
        }
    }
    if focused && !st.preedit.is_empty() {
        let galley = painter.layout_no_wrap(st.preedit.clone(), font.clone(), TERM_FG);
        let r = Rect::from_min_size(cursor.min, vec2(galley.size().x.max(cw), ch));
        painter.rect_filled(r.expand(1.0), 2.0, PREEDIT_BG);
        painter.galley(r.min, galley, TERM_FG);
        painter.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.5, ACCENT));
    }
    if scrolled > 0 {
        let label = format!("↑ {scrolled} 行前  ・ 入力すると戻ります");
        let g = painter.layout_no_wrap(label, FontId::proportional(12.0), Color32::BLACK);
        let r = Rect::from_min_size(pos2(outer.right() - g.size().x - 24.0, outer.top() + 8.0), g.size() + vec2(12.0, 6.0));
        painter.rect_filled(r, 6.0, WAITING);
        painter.galley(r.min + vec2(6.0, 3.0), g, Color32::BLACK);
    }
    drop(parser);

    if focused {
        ui.ctx().output_mut(|o| o.ime = Some(egui::output::IMEOutput { rect: outer, cursor_rect: cursor }));
    }

    if st.size != (rows, cols) {
        st.size = (rows, cols);
        return Some((rows, cols));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn flush(
    painter: &Painter,
    text: &str,
    at: Pos2,
    style: Option<(Color32, bool, bool)>,
    font: &FontId,
    bold: &FontId,
    cw: f32,
    ch: f32,
) {
    let Some((fg, is_bold, underline)) = style else { return };
    if text.trim().is_empty() && !underline {
        return;
    }
    painter.text(at, Align2::LEFT_TOP, text, font.clone(), fg);
    if is_bold {
        // 太さが1つしかないフォントなので、少しずらして重ねて太く見せる
        let _ = bold;
        painter.text(at + vec2(0.45, 0.0), Align2::LEFT_TOP, text, font.clone(), fg);
    }
    if underline {
        let w = cw * text.chars().count() as f32;
        painter.line_segment([pos2(at.x, at.y + ch - 1.0), pos2(at.x + w, at.y + ch - 1.0)], Stroke::new(1.0, fg));
    }
}

fn input(ui: &Ui, s: &mut Session, st: &mut TermState) {
    let events = ui.input(|i| i.events.clone());
    // 変換を確定したEnterで、そのまま送信してしまわないように
    let committed = events.iter().any(|e| matches!(e, Event::Ime(ImeEvent::Commit(_))));
    let app_cursor = s.parser.lock().unwrap().screen().application_cursor();
    for ev in events {
        match ev {
            Event::Ime(ImeEvent::Preedit(p)) => st.preedit = p,
            Event::Ime(ImeEvent::Commit(t)) => {
                st.preedit.clear();
                if !t.is_empty() {
                    s.write(t.as_bytes());
                    st.sel = None;
                }
            }
            Event::Text(t) => {
                if st.preedit.is_empty() {
                    s.write(t.as_bytes());
                    st.sel = None;
                }
            }
            Event::Key { key, pressed: true, modifiers, .. } => {
                if modifiers.mac_cmd || !st.preedit.is_empty() {
                    continue;
                }
                if key == egui::Key::Enter && committed {
                    continue;
                }
                if let Some(b) = keys::encode(key, modifiers, app_cursor) {
                    s.write(&b);
                    st.sel = None;
                }
            }
            Event::Paste(t) => {
                s.paste(&t);
                st.sel = None;
            }
            Event::Copy | Event::Cut => {
                if let Some((a, b)) = st.sel.filter(|(a, b)| a != b) {
                    let (a, b) = ordered(a, b);
                    let text = s.parser.lock().unwrap().screen().contents_between(a.0, a.1, b.0, b.1);
                    ui.ctx().copy_text(text);
                }
            }
            _ => {}
        }
    }
}

fn color(c: vt100::Color, default: Color32) -> Color32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        vt100::Color::Idx(i) => indexed(i),
    }
}

fn indexed(i: u8) -> Color32 {
    match i {
        0..16 => ANSI[i as usize],
        16..232 => {
            let i = i - 16;
            let v = |x: u8| if x == 0 { 0 } else { 55 + 40 * x };
            Color32::from_rgb(v(i / 36), v(i / 6 % 6), v(i % 6))
        }
        _ => {
            let g = 8 + 10 * (i - 232);
            Color32::from_rgb(g, g, g)
        }
    }
}
