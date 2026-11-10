use std::time::Instant;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Widget, Wrap};

use crate::app::{App, Mode, tilde};
use crate::session::Session;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &mut App) {
    let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let [side, main] =
        Layout::horizontal([Constraint::Length(app.cfg.sidebar_width), Constraint::Min(1)]).areas(body);

    sidebar(f, side, app);
    main_pane(f, main, app);
    status_bar(f, status, app);

    match &app.mode {
        Mode::New(form) => {
            let names: Vec<Span> = app
                .cfg
                .agents
                .iter()
                .enumerate()
                .flat_map(|(i, a)| {
                    let st = if i == form.agent {
                        Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    };
                    [Span::styled(format!(" {} ", a.name), st), Span::raw(" ")]
                })
                .collect();
            let label = |i: usize, s: &str| {
                let st = if form.field == i { Style::default().fg(ACCENT).add_modifier(Modifier::BOLD) } else { Style::default() };
                let pad = 14usize.saturating_sub(unicode_width(s));
                Span::styled(format!("{s}{}", " ".repeat(pad)), st)
            };
            let caret = |i: usize| if form.field == i { "▏" } else { "" };
            let mut agent_line = vec![label(0, "エージェント")];
            agent_line.extend(names);
            let lines = vec![
                Line::from(agent_line),
                Line::raw(""),
                Line::from(vec![label(1, "フォルダ"), Span::raw(form.dir.clone()), Span::styled(caret(1), Style::default().fg(ACCENT))]),
                Line::from(vec![
                    label(2, "ブランチ"),
                    Span::raw(form.branch.clone()),
                    Span::styled(caret(2), Style::default().fg(ACCENT)),
                    if form.branch.is_empty() {
                        Span::styled("（入れるとworktreeを作る・空ならそのフォルダで）", Style::default().fg(DIM))
                    } else {
                        Span::raw("")
                    },
                ]),
                Line::raw(""),
                Line::styled("Tab 次の欄   ←→ エージェント   Enter 作る   Esc やめる", Style::default().fg(DIM)),
            ];
            popup(f, " 新しいセッション ", lines, 72, 8);
        }
        Mode::Rename(name) => {
            let lines = vec![Line::from(vec![Span::raw(name.clone()), Span::styled("▏", Style::default().fg(ACCENT))])];
            popup(f, " 名前を変える ", lines, 48, 3);
        }
        Mode::ConfirmKill => {
            let name = app.current().map(|s| s.name.clone()).unwrap_or_default();
            let lines = vec![
                Line::raw(format!("「{name}」を止めて一覧から外します。")),
                Line::styled("worktreeやファイルはそのまま残ります。  y はい / ほかのキー やめる", Style::default().fg(DIM)),
            ];
            popup(f, " 止める ", lines, 64, 4);
        }
        Mode::ConfirmQuit => {
            let lines = vec![
                Line::raw(format!("動いているセッションが{}個あります。全部止めて終わりますか？", app.alive())),
                Line::styled("y はい / ほかのキー やめる", Style::default().fg(DIM)),
            ];
            popup(f, " 終わる ", lines, 64, 4);
        }
        Mode::Help => help(f, app),
        _ => {}
    }
}

fn sidebar(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::RIGHT).border_style(Style::default().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [head, list_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);

    let waiting = app.sessions.iter().filter(|s| s.attention).count();
    let busy = app.sessions.iter().filter(|s| s.busy).count();
    let mut head_spans = vec![Span::styled(" mure ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))];
    if busy > 0 {
        head_spans.push(Span::styled(format!(" 実行{busy}"), Style::default().fg(ACCENT)));
    }
    if waiting > 0 {
        head_spans.push(Span::styled(format!(" 待ち{waiting}"), Style::default().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(Line::from(head_spans)), head);

    if app.sessions.is_empty() {
        let p = Paragraph::new(vec![
            Line::styled(" セッションがありません", Style::default().fg(DIM)),
            Line::raw(""),
            Line::from(vec![Span::styled(" n", Style::default().fg(ACCENT)), Span::raw(" で新しく作る")]),
            Line::from(vec![Span::styled(" ?", Style::default().fg(ACCENT)), Span::raw(" でキー一覧")]),
        ]);
        f.render_widget(p, list_area);
        return;
    }

    let now = Instant::now();
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (mark, mstyle) = status_mark(s, now);
            let num = if i < 9 { format!(" {} ", i + 1) } else { "   ".into() };
            let name_style = if s.exit.is_some() {
                Style::default().fg(DIM)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let l1 = Line::from(vec![
                Span::styled(num, Style::default().fg(DIM)),
                Span::styled(mark, mstyle),
                Span::raw(" "),
                Span::styled(s.name.clone(), name_style),
            ]);
            let mut meta = vec![s.agent.clone()];
            if let Some(b) = &s.branch {
                if *b != s.name {
                    meta.push(b.clone());
                }
            }
            meta.push(match s.exit {
                Some(code) => format!("終了({code})"),
                None => elapsed(now.duration_since(s.created).as_secs()),
            });
            let l2 = Line::styled(format!("     {}", meta.join(" · ")), Style::default().fg(DIM));
            let mut item = ListItem::new(vec![l1, l2]);
            if i == app.sel {
                item = item.style(Style::default().bg(Color::Indexed(236)));
            }
            item
        })
        .collect();
    f.render_widget(List::new(items), list_area);
}

fn status_mark(s: &Session, now: Instant) -> (String, Style) {
    if let Some(code) = s.exit {
        let c = if code == 0 { Color::Green } else { Color::Red };
        return ((if code == 0 { "✓" } else { "✕" }).into(), Style::default().fg(c));
    }
    if s.attention {
        return ("●".into(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    }
    if s.busy {
        let i = (now.duration_since(s.created).as_millis() / 100) as usize % SPINNER.len();
        return (SPINNER[i].into(), Style::default().fg(ACCENT));
    }
    ("·".into(), Style::default().fg(DIM))
}

fn elapsed(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}秒"),
        60..3600 => format!("{}分", secs / 60),
        _ => format!("{}時間{}分", secs / 3600, secs / 60 % 60),
    }
}

fn main_pane(f: &mut Frame, area: Rect, app: &mut App) {
    let term = matches!(app.mode, Mode::Term);
    let Some(s) = app.sessions.get(app.sel) else {
        let p = Paragraph::new(vec![
            Line::raw(""),
            Line::styled("n で、エージェントを1つ起動します", Style::default().fg(DIM)),
        ])
        .alignment(Alignment::Center);
        f.render_widget(p, area);
        return;
    };

    let parser = s.parser.lock().unwrap();
    let screen = parser.screen();
    let scrolled = screen.scrollback();

    let mut title = vec![
        Span::styled(format!(" {} ", s.name), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("{} · {} ", s.agent, tilde(&s.cwd)), Style::default().fg(DIM)),
    ];
    if scrolled > 0 {
        title.push(Span::styled(format!(" ↑{scrolled}行前 "), Style::default().fg(Color::Black).bg(Color::Yellow)));
    }
    if matches!(app.mode, Mode::Diff(_)) {
        title.push(Span::styled(" 差分 ", Style::default().fg(Color::Black).bg(ACCENT)));
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(if term { ACCENT } else { DIM }))
        .title(Line::from(title));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Mode::Diff(d) = &app.mode {
        let p = Paragraph::new(d.lines.clone()).scroll((d.scroll.min(u16::MAX as usize) as u16, 0));
        f.render_widget(p, inner);
        return;
    }

    f.render_widget(TermView { screen }, inner);
    if term && scrolled == 0 && !screen.hide_cursor() {
        let (r, c) = screen.cursor_position();
        if r < inner.height && c < inner.width {
            f.set_cursor_position((inner.x + c, inner.y + r));
        }
    }
}

struct TermView<'a> {
    screen: &'a vt100::Screen,
}

impl Widget for TermView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.screen.size();
        for r in 0..rows.min(area.height) {
            for c in 0..cols.min(area.width) {
                let Some(cell) = self.screen.cell(r, c) else { continue };
                let Some(out) = buf.cell_mut((area.x + c, area.y + r)) else { continue };
                if cell.is_wide_continuation() {
                    out.reset();
                    continue;
                }
                let text = cell.contents();
                out.set_symbol(if text.is_empty() { " " } else { &text });
                let mut st = Style::default().fg(color(cell.fgcolor())).bg(color(cell.bgcolor()));
                if cell.bold() {
                    st = st.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    st = st.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    st = st.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    st = st.add_modifier(Modifier::REVERSED);
                }
                out.set_style(st);
            }
        }
    }
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn status_bar(f: &mut Frame, area: Rect, app: &App) {
    let key = |k: &str| Span::styled(format!(" {k} "), Style::default().fg(Color::Black).bg(DIM));
    let txt = |t: &str| Span::raw(format!(" {t}  "));
    let p = app.prefix.label();
    let spans: Vec<Span> = match &app.mode {
        Mode::Term => vec![
            Span::styled(" 入力中 ", Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            key(&p),
            txt("一覧に戻る"),
        ],
        Mode::Diff(_) => vec![
            Span::styled(" 差分 ", Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            key("j/k"),
            txt("動かす"),
            key("Space/b"),
            txt("ページ"),
            key("R"),
            txt("読み直す"),
            key("Esc"),
            txt("閉じる"),
        ],
        _ => vec![
            Span::styled(" 一覧 ", Style::default().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            key("Enter"),
            txt("入る"),
            key("n"),
            txt("新規"),
            key("d"),
            txt("差分"),
            key("x"),
            txt("止める"),
            key("PgUp"),
            txt("履歴"),
            key("?"),
            txt("ヘルプ"),
            key("q"),
            txt("終わる"),
        ],
    };
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    if let Some((msg, _)) = &app.flash {
        let w = (unicode_width(msg) as u16 + 2).min(area.width);
        let r = Rect { x: area.right().saturating_sub(w), width: w, ..area };
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(Span::styled(format!(" {msg} "), Style::default().fg(Color::Black).bg(Color::Yellow))), r);
    }
}

fn unicode_width(s: &str) -> usize {
    Line::raw(s).width()
}

fn help(f: &mut Frame, app: &App) {
    let k = |a: &str, b: &str| {
        let pad = 16usize.saturating_sub(unicode_width(a));
        Line::from(vec![Span::styled(format!("  {a}{}", " ".repeat(pad)), Style::default().fg(ACCENT)), Span::raw(b.to_string())])
    };
    let p = app.prefix.label();
    let lines = vec![
        Line::styled("一覧で", Style::default().add_modifier(Modifier::BOLD)),
        k("j / k", "選ぶ"),
        k("Enter", "選んだセッションに入る（キーをそのまま送る）"),
        k("1〜9", "その番号に入る"),
        k("n", "新しいセッション（ブランチ名でworktreeも）"),
        k("d", "コミットしていない差分を見る"),
        k("r", "名前を変える"),
        k("x", "止める／終わったものを片付ける"),
        k("PgUp / PgDn", "画面の履歴をさかのぼる（Escで戻る）"),
        k("q", "終わる"),
        Line::raw(""),
        Line::styled("入力中に", Style::default().add_modifier(Modifier::BOLD)),
        k(&p, "一覧に戻る"),
        k(&format!("{p} {p}"), format!("{p} そのものを送る").as_str()),
        k(&format!("{p} 1〜9"), "別のセッションへ移る"),
        Line::raw(""),
        Line::styled("印", Style::default().add_modifier(Modifier::BOLD)),
        Line::from(vec![Span::styled("  ⠹", Style::default().fg(ACCENT)), Span::raw(" 動いている  "), Span::styled("●", Style::default().fg(Color::Yellow)), Span::raw(" 止まってこちらを待っている  "), Span::styled("·", Style::default().fg(DIM)), Span::raw(" 静か")]),
        Line::raw(""),
        Line::styled(format!("設定: {}", tilde(&crate::config::path())), Style::default().fg(DIM)),
    ];
    let h = lines.len() as u16 + 2;
    popup(f, " キー ", lines, 64, h);
}

fn popup(f: &mut Frame, title: &str, lines: Vec<Line>, w: u16, h: u16) {
    let area = f.area();
    let w = w.min(area.width);
    let h = h.min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 3, width: w, height: h };
    f.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(title.to_string(), Style::default().add_modifier(Modifier::BOLD)));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), r);
}
