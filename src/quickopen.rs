//! ⌘P: 名前の一部を打ってファイルを開く

use std::path::{Path, PathBuf};
use std::process::Command;

use eframe::egui::{self, Align, Frame, Id, Key, Label, Layout, Margin, Modifiers, RichText, ScrollArea, Sense, TextEdit};

use crate::theme::*;

const SKIP: &[&str] = &[".git", "node_modules", "target", "dist", "build", ".next", ".venv", "venv", "__pycache__", ".turbo", ".cache"];

pub struct QuickOpen {
    /// (表示する名前, 本当の場所)
    files: Vec<(String, PathBuf)>,
    query: String,
    hi: usize,
    first: bool,
}

pub enum Outcome {
    Keep,
    Cancel,
    Open(PathBuf),
}

pub fn list_files(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-co", "--exclude-standard"])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            return String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect();
        }
    }
    let mut v = Vec::new();
    walk(root, root, &mut v);
    v
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    if out.len() > 30_000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = e.path();
        if p.is_dir() {
            if !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                walk(root, &p, out);
            }
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}

/// 文字が順に含まれていれば一致。ファイル名での一致・連続・先頭を高く
pub fn score(query: &str, path: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let p: Vec<char> = path.to_lowercase().chars().collect();
    let name_start = path.rfind('/').map(|i| path[..=i].chars().count()).unwrap_or(0);
    let mut s = 0i32;
    let mut pi = 0usize;
    let mut prev: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        if qc == ' ' {
            continue;
        }
        let found = (pi..p.len()).find(|&i| p[i] == qc)?;
        s += 1;
        if prev == Some(found.wrapping_sub(1)) {
            s += 5;
        }
        if found >= name_start {
            s += 3;
        }
        if found == name_start || (found > 0 && matches!(p[found - 1], '/' | '_' | '-' | '.')) {
            s += 4;
        }
        prev = Some(found);
        pi = found + 1;
    }
    Some(s * 100 - p.len() as i32)
}

impl QuickOpen {
    /// ワークスペースなら複数のフォルダから集め、頭にフォルダ名を付ける
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let multi = roots.len() > 1;
        let mut files = Vec::new();
        for r in &roots {
            let name = r.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            for f in list_files(r) {
                let shown = if multi { format!("{name}/{f}") } else { f.clone() };
                files.push((shown, r.join(f)));
            }
        }
        Self { files, query: String::new(), hi: 0, first: true }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        let (down, up) = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::ArrowUp)));
        let enter = ctx.input(|i| i.key_pressed(Key::Enter));
        let mut hits: Vec<(i32, &String, &PathBuf)> =
            self.files.iter().filter_map(|(f, p)| score(&self.query, f).map(|s| (s, f, p))).collect();
        if !self.query.is_empty() {
            hits.sort_by(|a, b| b.0.cmp(&a.0));
        }
        hits.truncate(60);
        if down && !hits.is_empty() {
            self.hi = (self.hi + 1).min(hits.len() - 1);
        }
        if up {
            self.hi = self.hi.saturating_sub(1);
        }
        let hi = self.hi.min(hits.len().saturating_sub(1));
        let mut open: Option<PathBuf> = None;
        let screen = ctx.screen_rect();
        let modal = egui::Modal::new(Id::new("quickopen")).show(ctx, |ui| {
            ui.set_width(620.0_f32.min(screen.width() - 80.0));
            let r = ui.add(
                TextEdit::singleline(&mut self.query)
                    .hint_text("ファイル名の一部（例: appmain → src/app/main.rs）")
                    .desired_width(f32::INFINITY)
                    .font(egui::FontId::proportional(15.0))
                    .margin(Margin::symmetric(10, 8)),
            );
            if self.first {
                r.request_focus();
                self.first = false;
            }
            if r.changed() {
                self.hi = 0;
            }
            ui.add_space(6.0);
            ScrollArea::vertical().max_height(380.0).auto_shrink([false, true]).show(ui, |ui| {
                if hits.is_empty() {
                    ui.label(RichText::new("見つかりません").color(MUTED));
                }
                for (i, (_, f, abs)) in hits.iter().enumerate() {
                    let (dir, name) = match f.rfind('/') {
                        Some(p) => (&f[..p], &f[p + 1..]),
                        None => ("", f.as_str()),
                    };
                    let on = i == hi;
                    let inner = Frame::new()
                        .fill(if on { CARD_SEL } else { egui::Color32::TRANSPARENT })
                        .corner_radius(5)
                        .inner_margin(Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(name).color(TEXT));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.add(Label::new(RichText::new(dir).size(11.0).color(FAINT)).truncate());
                                });
                            });
                        });
                    let resp = ui.interact(inner.response.rect, Id::new(("qo", i)), Sense::click());
                    if on && (down || up) {
                        resp.scroll_to_me(None);
                    }
                    if resp.clicked() {
                        open = Some((*abs).clone());
                    }
                }
            });
            ui.add_space(4.0);
            ui.label(RichText::new(format!("{} ファイル · ↑↓ 選ぶ · Enter 開く · Esc やめる", self.files.len())).size(11.0).color(FAINT));
        });
        if enter && open.is_none() {
            open = hits.get(hi).map(|(_, _, p)| (*p).clone());
        }
        if let Some(p) = open {
            return Outcome::Open(p);
        }
        if modal.should_close() {
            return Outcome::Cancel;
        }
        Outcome::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn prefers_file_name() {
        let a = score("main", "src/main.rs").unwrap();
        let b = score("main", "src/m/a/i/n.rs").unwrap();
        assert!(a > b);
        assert!(score("xyz", "src/main.rs").is_none());
        assert!(score("smr", "src/main.rs").is_some());
    }
}
