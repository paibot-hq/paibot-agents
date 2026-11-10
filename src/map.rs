//! エージェントマップ: セッションと、そこから呼ばれたエージェントを時間の流れに並べる

use std::time::{Duration, SystemTime};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Shape, Stroke, Ui, epaint::CubicBezierShape, pos2,
    vec2,
};

use crate::theme::*;
use crate::ui;

pub const CLAUDE: Color32 = Color32::from_rgb(0xe8, 0x93, 0x72);
pub const CODEX: Color32 = Color32::from_rgb(0x5c, 0xc8, 0xb0);
pub const SUB: Color32 = Color32::from_rgb(0xb3, 0x9b, 0xff);
pub const OTHER: Color32 = Color32::from_rgb(0x9b, 0xa3, 0xb5);

#[derive(Clone, Copy, PartialEq)]
pub enum State {
    Running,
    Waiting,
    Quiet,
    Done,
    Stopped,
}

pub struct Kid {
    pub id: String,
    pub label: String,
    pub prompt: String,
    pub last: String,
    pub start: SystemTime,
    pub end: Option<SystemTime>,
    pub state: State,
    pub tools: usize,
    pub color: Color32,
}

pub struct Lane {
    pub name: String,
    pub agent: String,
    pub color: Color32,
    pub start: SystemTime,
    pub state: State,
    pub kids: Vec<Kid>,
}

/// グラフ（点と線）か、時間の流れか
#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Graph,
    Timeline,
}

/// マップでクリックされたもの
pub enum Click {
    Kid(String),
    Lane(usize),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Window {
    All,
    Min30,
    Min5,
}

fn secs(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}秒"),
        60..3600 => format!("{}分{}秒", s / 60, s % 60),
        _ => format!("{}時間{}分", s / 3600, s / 60 % 60),
    }
}

fn clip(s: &str, n: usize) -> String {
    let one = s.replace('\n', " ");
    let t: String = one.chars().take(n).collect();
    if t.chars().count() < one.chars().count() { format!("{t}…") } else { t }
}

/// 3次ベジェ曲線の途中の点
fn bezier(p: [Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    let a = u * u * u;
    let b = 3.0 * u * u * t;
    let c = 3.0 * u * t * t;
    let d = t * t * t;
    pos2(
        a * p[0].x + b * p[1].x + c * p[2].x + d * p[3].x,
        a * p[0].y + b * p[1].y + c * p[2].y + d * p[3].y,
    )
}

/// 帯を描く。動いているものは光が走る
fn bar(p: &egui::Painter, r: Rect, color: Color32, state: State, t: f64) {
    let radius = CornerRadius::same((r.height() / 2.0) as u8);
    let (fill, edge) = match state {
        State::Done => (color.gamma_multiply(0.45), color.gamma_multiply(0.6)),
        State::Stopped => (WAITING.gamma_multiply(0.25), WAITING.gamma_multiply(0.7)),
        State::Quiet => (color.gamma_multiply(0.28), color.gamma_multiply(0.45)),
        State::Waiting => (WAITING.gamma_multiply(0.55), WAITING),
        State::Running => (color.gamma_multiply(0.75), color),
    };
    p.rect_filled(r, radius, fill);
    // 上半分を少し明るくして立体感を出す
    let top = Rect::from_min_max(r.min + vec2(2.0, 1.0), pos2(r.right() - 2.0, r.center().y));
    if top.width() > 0.0 {
        p.rect_filled(top, CornerRadius::same((top.height() / 2.0) as u8), Color32::WHITE.gamma_multiply(0.07));
    }
    p.rect_stroke(r, radius, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
    if state == State::Running {
        // ぼんやり光る
        p.rect_stroke(r.expand(2.0), radius, Stroke::new(2.0, color.gamma_multiply(0.18)), egui::StrokeKind::Outside);
    }
    if state == State::Running && r.width() > 6.0 {
        // 左から右へ走る光
        let w = (r.width() * 0.35).clamp(16.0, 120.0);
        let phase = ((t * 0.6).fract() as f32) * (r.width() + w) - w;
        let steps = 10;
        for i in 0..steps {
            let x0 = r.left() + phase + w * i as f32 / steps as f32;
            let x1 = x0 + w / steps as f32;
            let seg = Rect::from_min_max(pos2(x0.max(r.left()), r.top()), pos2(x1.min(r.right()), r.bottom()));
            if seg.width() <= 0.0 {
                continue;
            }
            let k = 1.0 - ((i as f32 / steps as f32) * 2.0 - 1.0).abs();
            p.rect_filled(seg, 0.0, Color32::WHITE.gamma_multiply(0.28 * k));
        }
        // 右端で脈打つ点
        let pulse = 0.5 + 0.5 * ((t * 4.0).sin() as f32);
        p.circle_filled(pos2(r.right() - 1.0, r.center().y), 2.5 + 2.0 * pulse, color.gamma_multiply(0.35 + 0.4 * pulse));
    }
    if state == State::Waiting {
        let pulse = 0.5 + 0.5 * ((t * 3.0).sin() as f32);
        p.rect_stroke(r.expand(2.0 + pulse * 1.5), radius, Stroke::new(1.0, WAITING.gamma_multiply(0.25 + 0.3 * pulse)), egui::StrokeKind::Outside);
    }
}

/// マップを描く。クリックされた子の ID を返す
pub fn show(ui: &mut Ui, lanes: &[Lane], window: &mut Window, view: &mut View) -> Option<Click> {
    let mut clicked = None;
    let now = SystemTime::now();
    let t = ui.input(|i| i.time);

    // 見出し
    let running = lanes.iter().filter(|l| l.state == State::Running).count()
        + lanes.iter().flat_map(|l| &l.kids).filter(|k| k.state == State::Running).count();
    let called = lanes.iter().map(|l| l.kids.len()).sum::<usize>();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(ui::icon::TREE_STRUCTURE).size(15.0).color(ACCENT));
        ui.label(egui::RichText::new("エージェントマップ").font(ui::bold(13.5)).color(TEXT));
        if running > 0 {
            ui.label(egui::RichText::new(format!("動作中 {running}")).size(11.0).color(ACCENT));
        }
        ui.label(egui::RichText::new(format!("呼ばれた {called}")).size(11.0).color(FAINT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if *view == View::Timeline {
                for (w, label) in [(Window::Min5, "5分"), (Window::Min30, "30分"), (Window::All, "全部")] {
                    if ui.selectable_label(*window == w, egui::RichText::new(label).size(11.0)).clicked() {
                        *window = w;
                    }
                }
                ui.add_space(10.0);
            }
            for (v, g, label) in [(View::Timeline, ui::icon::CHART_BAR_HORIZONTAL, "時間"), (View::Graph, ui::icon::GRAPH, "グラフ")] {
                if ui.selectable_label(*view == v, egui::RichText::new(format!("{g} {label}")).size(11.5)).clicked() {
                    *view = v;
                }
            }
        });
    });
    ui.add_space(8.0);
    // 大きな数字の帯
    let tools: usize = lanes.iter().flat_map(|l| &l.kids).map(|k| k.tools).sum();
    let busy_kids = lanes.iter().flat_map(|l| &l.kids).filter(|k| k.state == State::Running).count();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let w = ((ui.available_width() - 24.0) / 4.0).max(60.0);
        for (n, label, c) in [
            (lanes.len(), "セッション", TEXT),
            (called, "呼ばれた", SUB),
            (running, "動作中", if running > 0 { ACCENT } else { FAINT }),
            (tools, "道具", MUTED),
        ] {
            let (r, _) = ui.allocate_exact_size(vec2(w, 54.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(r, 10.0, RAISED);
            p.rect_stroke(r, 10.0, Stroke::new(1.0, HAIRLINE), egui::StrokeKind::Inside);
            p.text(r.left_top() + vec2(12.0, 8.0), Align2::LEFT_TOP, label, FontId::proportional(10.5), FAINT);
            p.text(r.left_bottom() + vec2(12.0, -8.0), Align2::LEFT_BOTTOM, n.to_string(), ui::bold(22.0), c);
            if label == "動作中" && n > 0 {
                let pulse = 0.5 + 0.5 * ((t * 3.0).sin() as f32);
                p.circle_filled(r.right_top() + vec2(-14.0, 14.0), 3.0 + pulse * 1.5, ACCENT.gamma_multiply(0.5 + 0.5 * pulse));
            }
        }
    });
    let _ = busy_kids;
    ui.add_space(10.0);
    // 凡例
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        for (c, name) in [(CLAUDE, "claude"), (CODEX, "codex"), (SUB, "子エージェント"), (OTHER, "そのほか")] {
            let (r, _) = ui.allocate_exact_size(vec2(18.0, 10.0), Sense::hover());
            ui.painter().rect_filled(r.shrink2(vec2(0.0, 1.5)), 4.0, c.gamma_multiply(0.8));
            ui.label(egui::RichText::new(name).size(11.0).color(MUTED));
        }
    });
    ui.add_space(10.0);

    if lanes.is_empty() {
        ui.label(egui::RichText::new("セッションがありません").color(FAINT));
        return None;
    }
    if *view == View::Graph {
        let c = graph(ui, lanes, t, now);
        if running > 0 {
            ui.ctx().request_repaint();
        }
        return c;
    }

    // 時間の範囲
    let earliest = lanes.iter().map(|l| l.start).chain(lanes.iter().flat_map(|l| l.kids.iter().map(|k| k.start))).min().unwrap_or(now);
    let t0 = match window {
        Window::All => earliest,
        Window::Min30 => now - Duration::from_secs(30 * 60),
        Window::Min5 => now - Duration::from_secs(5 * 60),
    };
    let span = now.duration_since(t0).unwrap_or(Duration::from_secs(1)).as_secs_f32().max(10.0);

    let label_w = 168.0;
    let lane_h = 34.0;
    let kid_h = 26.0;
    let total_h: f32 = lanes.iter().map(|l| lane_h + l.kids.len() as f32 * kid_h + 10.0).sum::<f32>() + 28.0;

    egui::ScrollArea::vertical().id_salt("map").auto_shrink([false, false]).show(ui, |ui| {
        let (area, _) = ui.allocate_exact_size(vec2(ui.available_width(), total_h), Sense::hover());
        let p = ui.painter_at(area);
        let x0 = area.left() + label_w;
        let x1 = area.right() - 56.0;
        let xw = (x1 - x0).max(40.0);
        let x_of = |s: SystemTime| -> f32 {
            let d = if s < t0 { 0.0 } else { s.duration_since(t0).unwrap_or_default().as_secs_f32() };
            x0 + (d / span).clamp(0.0, 1.0) * xw
        };

        // 時間の目盛り
        let axis_y = area.top() + 10.0;
        let nice = [10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0];
        let step = nice.iter().copied().find(|s| span / s <= 6.0).unwrap_or(14400.0);
        let mut k = 0.0;
        while k <= span {
            let x = x1 - (k / span) * xw;
            p.line_segment([pos2(x, axis_y + 8.0), pos2(x, area.bottom())], Stroke::new(1.0, HAIRLINE.gamma_multiply(0.7)));
            let label = if k == 0.0 { "今".to_string() } else { format!("−{}", secs(Duration::from_secs_f32(k))) };
            p.text(pos2(x, axis_y), Align2::CENTER_CENTER, label, FontId::proportional(10.0), FAINT);
            k += step;
        }
        // 「今」の線
        let now_x = x1;
        p.line_segment([pos2(now_x, axis_y + 8.0), pos2(now_x, area.bottom())], Stroke::new(3.0, ACCENT.gamma_multiply(0.12)));
        p.line_segment([pos2(now_x, axis_y + 8.0), pos2(now_x, area.bottom())], Stroke::new(1.0, ACCENT.gamma_multiply(0.6)));

        let mut y = area.top() + 28.0;
        for (li, lane) in lanes.iter().enumerate() {
            // セッションの帯
            let cy = y + lane_h / 2.0;
            let dot = match lane.state {
                State::Running => lane.color,
                State::Waiting => WAITING,
                State::Done => OK,
                State::Stopped => BAD,
                State::Quiet => FAINT,
            };
            p.circle_filled(pos2(area.left() + 8.0, cy), 4.0, dot);
            p.text(pos2(area.left() + 20.0, cy - 6.0), Align2::LEFT_CENTER, clip(&lane.name, 16), ui::bold(12.5), TEXT);
            p.text(pos2(area.left() + 20.0, cy + 8.0), Align2::LEFT_CENTER, &lane.agent, FontId::proportional(10.5), FAINT);
            let lr = Rect::from_min_max(pos2(x_of(lane.start), cy - 6.0), pos2(x1, cy + 6.0));
            bar(&p, lr, lane.color, lane.state, t + li as f64 * 0.37);
            let parent_y = cy + 6.0;
            y += lane_h;

            for (ki, kid) in lane.kids.iter().enumerate() {
                let ky = y + kid_h / 2.0;
                let end = kid.end.unwrap_or(now);
                let sx = x_of(kid.start);
                let ex = x_of(end).max(sx + 6.0);
                let kr = Rect::from_min_max(pos2(sx, ky - 5.0), pos2(ex, ky + 5.0));

                // 親から子への線
                let a = pos2(sx, parent_y);
                let b = pos2(sx, ky - 4.0);
                let pts = [a, pos2(a.x - 10.0, a.y + (b.y - a.y) * 0.6), pos2(b.x - 10.0, b.y - 2.0), pos2(b.x + 2.0, b.y + 4.0)];
                let line_c = if kid.state == State::Running { kid.color.gamma_multiply(0.75) } else { kid.color.gamma_multiply(0.35) };
                p.add(Shape::CubicBezier(CubicBezierShape::from_points_stroke(pts, false, Color32::TRANSPARENT, Stroke::new(1.2, line_c))));
                if kid.state == State::Running {
                    // 線の上を流れる光の粒
                    for n in 0..3 {
                        let f = ((t * 0.9 + n as f64 / 3.0).fract()) as f32;
                        p.circle_filled(bezier(pts, f), 2.2, kid.color.gamma_multiply(0.9 - 0.25 * n as f32));
                    }
                }
                p.circle_filled(a, 2.5, kid.color.gamma_multiply(0.8));

                // 名前と頼まれたこと
                p.text(pos2(area.left() + 22.0, ky), Align2::LEFT_CENTER, ui::icon::ARROW_ELBOW_DOWN_RIGHT, FontId::proportional(11.0), FAINT);
                p.text(pos2(area.left() + 38.0, ky), Align2::LEFT_CENTER, clip(&kid.label, 12), ui::bold(11.0), kid.color);
                let lw = p.layout_no_wrap(clip(&kid.label, 12), ui::bold(11.0), kid.color).size().x;
                p.text(pos2(area.left() + 44.0 + lw, ky), Align2::LEFT_CENTER, clip(&kid.prompt, ((label_w - 60.0 - lw) / 6.5).max(4.0) as usize), FontId::proportional(10.5), FAINT);

                bar(&p, kr, kid.color, kid.state, t + (li * 7 + ki) as f64 * 0.29);
                // 帯の右に、使った道具の数と時間
                let dur = end.duration_since(kid.start).unwrap_or_default();
                let mark = match kid.state {
                    State::Done => ui::icon::CHECK,
                    State::Stopped => ui::icon::WARNING,
                    _ => "",
                };
                let info = format!("{mark} {} {}  {}", ui::icon::WRENCH, kid.tools, secs(dur));
                p.text(pos2(ex + 6.0, ky), Align2::LEFT_CENTER, info.trim(), FontId::proportional(10.0), MUTED);

                let hit = Rect::from_min_max(pos2(area.left(), ky - kid_h / 2.0), pos2(area.right(), ky + kid_h / 2.0));
                let resp = ui.interact(hit, ui.id().with(("kid", &kid.id)), Sense::click());
                if resp.hovered() {
                    p.rect_filled(hit, 4.0, Color32::WHITE.gamma_multiply(0.02));
                }
                let tip = format!(
                    "{}\n\n頼まれたこと: {}\n\n{}",
                    kid.label,
                    clip(&kid.prompt, 300),
                    if kid.last.is_empty() { String::new() } else { format!("答え: {}", clip(&kid.last, 300)) }
                );
                if resp.on_hover_text(tip).clicked() {
                    clicked = Some(kid.id.clone());
                }
                y += kid_h;
            }
            y += 10.0;
        }
    });
    if running > 0 {
        ui.ctx().request_repaint();
    }
    clicked.map(Click::Kid)
}

fn state_mark(state: State) -> (&'static str, Color32, &'static str) {
    match state {
        State::Running => (ui::icon::CIRCLE_NOTCH, ACCENT, "動作中"),
        State::Waiting => (ui::icon::HAND, WAITING, "待っています"),
        State::Quiet => (ui::icon::CIRCLE, FAINT, "静か"),
        State::Done => (ui::icon::CHECK_CIRCLE, OK, "終わった"),
        State::Stopped => (ui::icon::WARNING_CIRCLE, WAITING, "止まった"),
    }
}

/// 幅に収まるように後ろを「…」で切る
fn fit(p: &egui::Painter, text: &str, font: &FontId, max_w: f32) -> String {
    let one = text.replace('\n', " ");
    if p.layout_no_wrap(one.clone(), font.clone(), TEXT).size().x <= max_w {
        return one;
    }
    let chars: Vec<char> = one.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s: String = chars[..mid].iter().collect::<String>() + "…";
        if p.layout_no_wrap(s, font.clone(), TEXT).size().x <= max_w { lo = mid } else { hi = mid - 1 }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

/// 箱を1つ描く
#[allow(clippy::too_many_arguments)]
fn node(p: &egui::Painter, r: Rect, color: Color32, state: State, title: &str, sub: &str, foot: &str, t: f64, selected: bool, hovered: bool) {
    let radius = CornerRadius::same(10);
    if state == State::Running {
        let pulse = 0.5 + 0.5 * ((t * 3.0).sin() as f32);
        p.rect_stroke(r.expand(3.0 + pulse * 2.0), CornerRadius::same(13), Stroke::new(2.0, color.gamma_multiply(0.10 + 0.15 * pulse)), egui::StrokeKind::Outside);
    }
    p.rect_filled(r, radius, if hovered { CARD_SEL } else { RAISED });
    // 左の色の帯
    let stripe = Rect::from_min_max(r.min, pos2(r.left() + 4.0, r.bottom()));
    p.rect_filled(stripe, CornerRadius { nw: 10, sw: 10, ne: 0, se: 0 }, color.gamma_multiply(if state == State::Done { 0.55 } else { 0.95 }));
    let edge = if selected {
        ACCENT
    } else if state == State::Running {
        color.gamma_multiply(0.7)
    } else if state == State::Waiting {
        WAITING.gamma_multiply(0.8)
    } else {
        BORDER
    };
    p.rect_stroke(r, radius, Stroke::new(if selected { 1.5 } else { 1.0 }, edge), egui::StrokeKind::Inside);

    let (g, gc, word) = state_mark(state);
    // 動作中の印はくるくる回す代わりに明滅
    let gc = if state == State::Running { gc.gamma_multiply(0.55 + 0.45 * (0.5 + 0.5 * (t * 4.0).sin() as f32)) } else { gc };
    let mark = p.text(r.right_top() + vec2(-12.0, 14.0), Align2::RIGHT_CENTER, format!("{word} {g}"), FontId::proportional(10.5), gc);
    let inner = r.width() - 26.0;
    let tf = ui::bold(12.5);
    p.text(r.left_top() + vec2(14.0, 14.0), Align2::LEFT_CENTER, fit(p, title, &tf, inner - mark.width() - 8.0), tf, TEXT);
    let sf = FontId::proportional(11.0);
    p.text(r.left_top() + vec2(14.0, 33.0), Align2::LEFT_CENTER, fit(p, sub, &sf, inner), sf, MUTED);
    if !foot.is_empty() {
        let ff = FontId::proportional(10.0);
        p.text(r.left_bottom() + vec2(14.0, -11.0), Align2::LEFT_CENTER, fit(p, foot, &ff, inner), ff, FAINT);
    }
}

/// 点と線のグラフ。左にセッション、右に呼ばれたエージェント
fn graph(ui: &mut Ui, lanes: &[Lane], t: f64, now: SystemTime) -> Option<Click> {
    let mut clicked = None;
    let w0 = 200.0;
    let h0 = 66.0;
    let h1 = 66.0;
    let gap = 12.0;
    let between = 72.0;
    let block_gap = 26.0;
    let heights: Vec<f32> = lanes
        .iter()
        .map(|l| {
            let kids = l.kids.len() as f32;
            (kids * h1 + (kids - 1.0).max(0.0) * gap).max(h0)
        })
        .collect();
    let total_h = heights.iter().sum::<f32>() + block_gap * lanes.len() as f32 + 10.0;

    egui::ScrollArea::both().id_salt("graph").auto_shrink([false, false]).show(ui, |ui| {
        let width = ui.available_width().max(w0 + between + 240.0);
        let (area, _) = ui.allocate_exact_size(vec2(width, total_h), Sense::hover());
        let p = ui.painter_at(area);
        let x0 = area.left() + 4.0;
        let x1 = x0 + w0 + between;
        let w1 = (area.right() - x1 - 8.0).clamp(220.0, 360.0);
        let mut y = area.top() + 4.0;

        for (li, (lane, bh)) in lanes.iter().zip(&heights).enumerate() {
            // セッションの箱（子の並びの真ん中に）
            let r0 = Rect::from_min_size(pos2(x0, y + (bh - h0) / 2.0), vec2(w0, h0));
            let resp0 = ui.interact(r0, ui.id().with(("lane", li)), Sense::click());
            let age = now.duration_since(lane.start).unwrap_or_default();
            let foot = if lane.kids.is_empty() { secs(age) } else { format!("{} を呼んだ · {}", lane.kids.len(), secs(age)) };
            node(&p, r0, lane.color, lane.state, &lane.name, &lane.agent, &foot, t + li as f64 * 0.4, false, resp0.hovered());
            if resp0.on_hover_text("クリックでこのセッションへ").clicked() {
                clicked = Some(Click::Lane(li));
            }

            // 呼ばれたエージェント
            let mut ky = y + (bh - (lane.kids.len() as f32 * h1 + (lane.kids.len() as f32 - 1.0).max(0.0) * gap)).max(0.0) / 2.0;
            for (ki, kid) in lane.kids.iter().enumerate() {
                let r1 = Rect::from_min_size(pos2(x1, ky), vec2(w1, h1));
                // 線
                let a = pos2(r0.right(), r0.center().y);
                let b = pos2(r1.left(), r1.center().y);
                let dx = (b.x - a.x) * 0.5;
                let pts = [a, pos2(a.x + dx, a.y), pos2(b.x - dx, b.y), b];
                let running = kid.state == State::Running;
                let lc = if running { kid.color.gamma_multiply(0.8) } else { kid.color.gamma_multiply(0.35) };
                p.add(Shape::CubicBezier(CubicBezierShape::from_points_stroke(pts, false, Color32::TRANSPARENT, Stroke::new(if running { 2.0 } else { 1.3 }, lc))));
                // 矢印の先
                p.add(Shape::convex_polygon(vec![b, b + vec2(-7.0, -4.0), b + vec2(-7.0, 4.0)], lc, Stroke::NONE));
                if running {
                    for n in 0..3 {
                        let f = ((t * 0.8 + (n as f64) / 3.0 + ki as f64 * 0.13).fract()) as f32;
                        p.circle_filled(bezier(pts, f), 2.6, kid.color.gamma_multiply(0.95 - 0.25 * n as f32));
                    }
                }
                p.circle_filled(a, 3.0, lane.color.gamma_multiply(0.8));

                let resp = ui.interact(r1, ui.id().with(("node", &kid.id)), Sense::click());
                let end = kid.end.unwrap_or(now);
                let dur = end.duration_since(kid.start).unwrap_or_default();
                let foot = format!("{} 道具 {}  ·  {}", ui::icon::WRENCH, kid.tools, secs(dur));
                node(&p, r1, kid.color, kid.state, &kid.label, &kid.prompt, &foot, t + (li * 7 + ki) as f64 * 0.31, false, resp.hovered());
                let tip = format!(
                    "{}\n\n頼まれたこと: {}\n\n{}",
                    kid.label,
                    clip(&kid.prompt, 300),
                    if kid.last.is_empty() { String::new() } else { format!("答え: {}", clip(&kid.last, 300)) }
                );
                if resp.on_hover_text(tip).clicked() {
                    clicked = Some(Click::Kid(kid.id.clone()));
                }
                ky += h1 + gap;
            }
            y += bh + block_gap;
        }
    });
    clicked
}
