//! ⌘⇧F: フォルダ全体を検索する

use std::path::{Path, PathBuf};
use std::process::Command;

use eframe::egui::{self, Color32, FontId, Frame, Id, Label, Margin, RichText, ScrollArea, Sense, TextEdit, Ui};

use crate::theme::*;

const MAX_HITS: usize = 3000;

struct Hit {
    file: String,
    line: usize,
    text: String,
}

#[derive(Default)]
pub struct Search {
    query: String,
    hits: Vec<Hit>,
    searched: Option<String>,
    error: Option<String>,
    pub focus: bool,
    truncated: bool,
}

fn has_rg() -> bool {
    Command::new("rg").arg("--version").output().is_ok_and(|o| o.status.success())
}

impl Search {
    pub fn set_and_run(&mut self, q: &str, root: &Path) {
        self.query = q.to_string();
        self.run(root);
    }

    fn run(&mut self, root: &Path) {
        let q = self.query.trim().to_string();
        self.hits.clear();
        self.error = None;
        self.truncated = false;
        if q.is_empty() {
            self.searched = None;
            return;
        }
        let out = if has_rg() {
            Command::new("rg")
                .current_dir(root)
                .args(["--line-number", "--no-heading", "--color", "never", "--smart-case", "--fixed-strings", "--max-columns", "400", "--", &q, "."])
                .output()
        } else {
            Command::new("git").current_dir(root).args(["grep", "-n", "-I", "-i", "-F", "--", &q]).output()
        };
        match out {
            Ok(o) => {
                for l in String::from_utf8_lossy(&o.stdout).lines() {
                    if self.hits.len() >= MAX_HITS {
                        self.truncated = true;
                        break;
                    }
                    let mut it = l.splitn(3, ':');
                    let (Some(f), Some(n), Some(t)) = (it.next(), it.next(), it.next()) else { continue };
                    let Ok(line) = n.parse() else { continue };
                    self.hits.push(Hit { file: f.trim_start_matches("./").to_string(), line, text: t.trim().to_string() });
                }
                self.hits.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
            }
            Err(e) => self.error = Some(format!("検索できませんでした: {e}")),
        }
        self.searched = Some(q);
    }

    /// クリックされたら (ファイル, 行) を返す
    pub fn show(&mut self, ui: &mut Ui, root: &Path) -> Option<(PathBuf, usize)> {
        let mut open = None;
        let r = ui.add(
            TextEdit::singleline(&mut self.query)
                .hint_text("検索して Enter")
                .desired_width(f32::INFINITY)
                .font(FontId::proportional(14.0))
                .margin(Margin::symmetric(8, 6)),
        );
        if self.focus {
            r.request_focus();
            self.focus = false;
        }
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            self.run(root);
            r.request_focus();
        }
        ui.add_space(6.0);
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(BAD));
        }
        if let Some(q) = &self.searched {
            let files = {
                let mut v: Vec<&str> = self.hits.iter().map(|h| h.file.as_str()).collect();
                v.dedup();
                v.len()
            };
            let more = if self.truncated { "（多いので途中まで）" } else { "" };
            ui.label(RichText::new(format!("「{q}」 {} 件 · {files} ファイル{more}", self.hits.len())).size(12.0).color(MUTED));
        }
        ui.add_space(4.0);
        let q = self.searched.clone().unwrap_or_default();
        ScrollArea::vertical().id_salt("search-results").auto_shrink([false, false]).show(ui, |ui| {
            let mut last = "";
            for (i, h) in self.hits.iter().enumerate() {
                if h.file != last {
                    ui.add_space(6.0);
                    ui.add(Label::new(RichText::new(&h.file).size(12.0).strong().color(TEXT)).truncate());
                    last = &h.file;
                }
                let id = Id::new(("hit", i));
                let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
                let inner = Frame::new()
                    .fill(if hovered { CARD_HOVER } else { Color32::TRANSPARENT })
                    .corner_radius(4)
                    .inner_margin(Margin::symmetric(6, 1))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{:>4}", h.line)).font(FontId::monospace(11.0)).color(FAINT));
                            ui.add(Label::new(highlight(&h.text, &q)).truncate());
                        });
                    });
                if ui.interact(inner.response.rect, id, Sense::click()).clicked() {
                    open = Some((root.join(&h.file), h.line));
                }
            }
        });
        open
    }
}

/// 一致したところに色を付ける
fn highlight(text: &str, q: &str) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let font = FontId::monospace(12.0);
    let plain = TextFormat { font_id: font.clone(), color: TERM_FG, ..Default::default() };
    let mark = TextFormat { font_id: font, color: Color32::BLACK, background: WAITING, ..Default::default() };
    let mut job = LayoutJob::default();
    let lower = text.to_lowercase();
    let ql = q.to_lowercase();
    // 大文字小文字で長さが変わる文字があるとずれるので、そのときは色を付けない
    if ql.is_empty() || lower.len() != text.len() {
        job.append(text, 0.0, plain);
        return job;
    }
    let mut i = 0;
    while let Some(p) = lower[i..].find(&ql) {
        let s = i + p;
        if !text.is_char_boundary(s) || !text.is_char_boundary(s + ql.len()) {
            break;
        }
        job.append(&text[i..s], 0.0, plain.clone());
        job.append(&text[s..s + ql.len()], 0.0, mark.clone());
        i = s + ql.len();
    }
    job.append(&text[i..], 0.0, plain);
    job
}
