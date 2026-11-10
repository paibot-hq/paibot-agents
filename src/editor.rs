//! ファイルを編集するタブ

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use eframe::egui::{
    self, Align, FontId, Frame, Galley, Id, Label, Layout, Margin, RichText, ScrollArea, TextBuffer, TextEdit, Ui,
    text::{CCursor, CCursorRange, LayoutJob},
};
use egui_extras::syntax_highlighting::{self, CodeTheme};

use crate::theme::*;
use crate::ui;

const MAX_EDIT_BYTES: u64 = 1024 * 1024;
const MAX_VIEW_BYTES: u64 = 8 * 1024 * 1024;
const HIGHLIGHT_LINES: usize = 6000;

pub struct Doc {
    pub path: PathBuf,
    pub text: String,
    saved: String,
    mtime: Option<SystemTime>,
    /// 保存していない変更があるのに、ディスク側でも書き換わった
    pub external: bool,
    /// 次に描くときにこの行（1始まり）へ移る
    pub goto: Option<usize>,
    /// 編集できない理由（大きい・バイナリなど）
    pub readonly: Option<String>,
    lang: String,
    checked: Instant,
    /// 選んでいる行の範囲（1始まり）
    pub selection: Option<(usize, usize)>,
}

pub enum DocAction {
    Save,
    Ask,
    OpenInEditor,
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn lang_of(path: &Path) -> String {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "" => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        "tsx" | "jsx" | "mjs" | "cjs" | "ts" | "mts" => "js".into(),
        "toml" => "ini".into(),
        "astro" | "vue" | "svelte" => "html".into(),
        "zsh" | "bash" => "sh".into(),
        _ => ext,
    }
}

impl Doc {
    pub fn open(path: PathBuf) -> Self {
        let mut d = Self {
            lang: lang_of(&path),
            path,
            text: String::new(),
            saved: String::new(),
            mtime: None,
            external: false,
            goto: None,
            readonly: None,
            checked: Instant::now(),
            selection: None,
        };
        d.load();
        d
    }

    fn load(&mut self) {
        self.mtime = mtime(&self.path);
        self.external = false;
        self.readonly = None;
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_VIEW_BYTES {
            self.text.clear();
            self.readonly = Some(format!("大きいので開けません（{} MB）", size / 1024 / 1024));
        } else {
            match std::fs::read(&self.path) {
                Ok(bytes) if bytes.iter().take(8000).any(|b| *b == 0) => {
                    self.text.clear();
                    self.readonly = Some(format!("バイナリなので開けません（{} KB）", bytes.len() / 1024));
                }
                Ok(bytes) => {
                    self.text = String::from_utf8_lossy(&bytes).into_owned();
                    if size > MAX_EDIT_BYTES {
                        self.readonly = Some("大きいので読むだけにしています".into());
                    } else if std::str::from_utf8(&bytes).is_err() {
                        self.readonly = Some("UTF-8 ではないので読むだけにしています".into());
                    }
                }
                Err(e) => {
                    self.text.clear();
                    self.readonly = Some(format!("読めませんでした: {e}"));
                }
            }
        }
        self.saved = self.text.clone();
    }

    pub fn title(&self) -> String {
        self.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn dirty(&self) -> bool {
        self.readonly.is_none() && self.text != self.saved
    }

    pub fn save(&mut self) -> Result<()> {
        if self.readonly.is_some() {
            return Ok(());
        }
        std::fs::write(&self.path, &self.text)?;
        self.saved = self.text.clone();
        self.mtime = mtime(&self.path);
        self.external = false;
        Ok(())
    }

    /// エージェントが書き換えたら読み直す。こちらに未保存の変更があれば知らせるだけ
    pub fn poll_disk(&mut self) {
        if self.checked.elapsed() < Duration::from_millis(700) {
            return;
        }
        self.checked = Instant::now();
        let now = mtime(&self.path);
        if now == self.mtime {
            return;
        }
        if self.dirty() {
            self.external = true;
            self.mtime = now;
        } else {
            self.load();
        }
    }

    pub fn reload(&mut self) {
        self.load();
    }
}

fn line_start(text: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    let mut n = 1;
    for (i, c) in text.chars().enumerate() {
        if c == '\n' {
            n += 1;
            if n == line {
                return i + 1;
            }
        }
    }
    text.chars().count()
}

fn line_of(text: &str, char_idx: usize) -> usize {
    1 + text.chars().take(char_idx).filter(|c| *c == '\n').count()
}

pub fn show(ui: &mut Ui, doc: &mut Doc, root: &Path, font_size: f32) -> Vec<DocAction> {
    let mut acts = Vec::new();
    doc.poll_disk();
    let rel = doc.path.strip_prefix(root).unwrap_or(&doc.path).to_string_lossy().into_owned();

    Frame::new().fill(TERM_BG).inner_margin(Margin { left: 14, right: 8, top: 5, bottom: 5 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.add(Label::new(RichText::new(&rel).size(12.0).color(MUTED)).truncate());
            if doc.dirty() {
                ui.label(RichText::new("● 未保存").size(11.0).color(WAITING));
            }
            if let Some(r) = &doc.readonly {
                ui.label(RichText::new(r).size(11.0).color(FAINT));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui::icon_button(ui, ui::icon::CODE, "エディタで開く", false).clicked() {
                    acts.push(DocAction::OpenInEditor);
                }
                let ask = match doc.selection {
                    Some((a, b)) if a != b => format!("{a}〜{b} 行目を聞く"),
                    Some((a, _)) => format!("{a} 行目を聞く"),
                    None => "このファイルについて聞く".into(),
                };
                let b = egui::Button::new(RichText::new(format!("{}  {ask}", ui::icon::SPARKLE)).size(12.0).color(ACCENT))
                    .fill(ACCENT.gamma_multiply(0.12))
                    .corner_radius(6.0);
                if ui.add(b).on_hover_text("⌘L  エージェントの入力欄に入れる").clicked() {
                    acts.push(DocAction::Ask);
                }
                if doc.readonly.is_none() && doc.dirty() {
                    let b = egui::Button::new(RichText::new(format!("{}  保存", ui::icon::FLOPPY_DISK)).size(12.0)).corner_radius(6.0);
                    if ui.add(b).on_hover_text("⌘S").clicked() {
                        acts.push(DocAction::Save);
                    }
                }
            });
        });
    });

    if doc.external {
        Frame::new().fill(WAITING.gamma_multiply(0.15)).inner_margin(Margin::symmetric(12, 6)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("このファイルは外で書き換わりました（エージェントかもしれません）").color(WAITING));
                if ui.small_button("読み直す（こちらの変更を捨てる）").clicked() {
                    doc.reload();
                }
                if ui.small_button("こちらを残す").clicked() {
                    doc.external = false;
                }
            });
        });
    }

    if doc.readonly.is_some() && doc.text.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| ui.label(RichText::new(doc.readonly.clone().unwrap_or_default()).color(MUTED)));
        return acts;
    }

    let font = FontId::monospace(font_size);
    let row_h = ui.fonts(|f| f.row_height(&font));
    let lines = doc.text.split('\n').count().max(1);
    let digits = lines.to_string().len().max(3);
    let id = Id::new(("doc", &doc.path));

    let mut area = ScrollArea::both().id_salt(("doc-scroll", &doc.path)).auto_shrink([false, false]);
    if let Some(line) = doc.goto.take() {
        area = area.vertical_scroll_offset(((line.saturating_sub(1)) as f32 * row_h - 120.0).max(0.0));
        let idx = line_start(&doc.text, line);
        let mut st = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
        st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(idx))));
        st.store(ui.ctx(), id);
        ui.memory_mut(|m| m.request_focus(id));
    }

    let highlight = lines <= HIGHLIGHT_LINES;
    let theme = CodeTheme::dark(font_size);
    let lang = doc.lang.clone();
    let mut layouter = |ui: &Ui, buf: &dyn TextBuffer, _wrap: f32| -> Arc<Galley> {
        let mut job = if highlight {
            syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, buf.as_str(), &lang)
        } else {
            LayoutJob::simple(buf.as_str().to_owned(), font.clone(), TERM_FG, f32::INFINITY)
        };
        job.wrap.max_width = f32::INFINITY;
        ui.fonts(|f| f.layout_job(job))
    };

    Frame::new().fill(TERM_BG).inner_margin(Margin { left: 8, right: 0, top: 6, bottom: 0 }).show(ui, |ui| {
        area.show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                let numbers: String = (1..=lines).map(|n| format!("{n:>digits$}\n")).collect();
                ui.add(Label::new(RichText::new(numbers).font(font.clone()).color(FAINT)).selectable(false));
                let editable = doc.readonly.is_none();
                let out = if editable {
                    TextEdit::multiline(&mut doc.text)
                        .id(id)
                        .code_editor()
                        .font(font.clone())
                        .frame(false)
                        .margin(Margin::ZERO)
                        .desired_width(f32::INFINITY)
                        .desired_rows(lines)
                        .lock_focus(true)
                        .layouter(&mut layouter)
                        .show(ui)
                } else {
                    let mut view = doc.text.as_str();
                    TextEdit::multiline(&mut view)
                        .id(id)
                        .code_editor()
                        .font(font.clone())
                        .frame(false)
                        .margin(Margin::ZERO)
                        .desired_width(f32::INFINITY)
                        .desired_rows(lines)
                        .layouter(&mut layouter)
                        .show(ui)
                };
                if out.response.has_focus() {
                    doc.selection = out.cursor_range.map(|r| {
                        let a = line_of(&doc.text, r.primary.index.min(r.secondary.index));
                        let mut b = line_of(&doc.text, r.primary.index.max(r.secondary.index));
                        // 行末の改行まで選んだときは次の行を含めない
                        let hi = r.primary.index.max(r.secondary.index);
                        if b > a && hi > 0 && doc.text.chars().nth(hi - 1) == Some('\n') {
                            b -= 1;
                        }
                        (a, b)
                    });
                }
            });
        });
    });
    acts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mure-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn save_and_detect_outside_change() {
        let p = tmp("a.rs", "fn a() {}\n");
        let mut d = Doc::open(p.clone());
        assert!(!d.dirty());
        d.text.push_str("fn b() {}\n");
        assert!(d.dirty());
        d.save().unwrap();
        assert!(!d.dirty());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "fn a() {}\nfn b() {}\n");

        // 未保存の変更がないときは、外の変更をそのまま読み込む
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&p, "fn c() {}\n").unwrap();
        d.checked = Instant::now() - Duration::from_secs(5);
        d.poll_disk();
        assert_eq!(d.text, "fn c() {}\n");
        assert!(!d.external);

        // 未保存の変更があるときは、上書きせずに知らせる
        d.text.push_str("// mine\n");
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&p, "fn d() {}\n").unwrap();
        d.checked = Instant::now() - Duration::from_secs(5);
        d.poll_disk();
        assert!(d.external);
        assert!(d.text.contains("// mine"));
    }

    #[test]
    fn lines() {
        let t = "a\nbb\nccc\n";
        assert_eq!(line_start(t, 1), 0);
        assert_eq!(line_start(t, 2), 2);
        assert_eq!(line_start(t, 3), 5);
        assert_eq!(line_of(t, 0), 1);
        assert_eq!(line_of(t, 3), 2);
    }

    #[test]
    fn binary_is_readonly() {
        let p = tmp("b.bin", "a\0b");
        let d = Doc::open(p);
        assert!(d.readonly.is_some());
        assert!(!d.dirty());
    }
}
