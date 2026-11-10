//! フォルダのツリーとファイルの中身

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Align, Color32, FontId, Frame, Id, Label, Layout, Margin, RichText, ScrollArea, Sense, Ui};
use egui_extras::syntax_highlighting::{self, CodeTheme};

use crate::git::{self, DiffLine, Kind};
use crate::theme::*;

const MAX_BYTES: u64 = 2 * 1024 * 1024;
const HIGHLIGHT_LINES: usize = 4000;
const HIDE: &[&str] = &[".git", ".DS_Store"];
const HEAVY: &[&str] = &["node_modules", "target", "dist", "build", ".next", ".venv", "venv", "__pycache__", ".turbo", ".cache"];

struct Entry {
    name: String,
    path: PathBuf,
    dir: bool,
}

enum Body {
    Text { text: String, lines: usize },
    Message(String),
}

struct OpenFile {
    path: PathBuf,
    body: Body,
    modified: Option<SystemTime>,
    show_diff: bool,
    diff: Vec<DiffLine>,
    checked: Instant,
}

pub struct FilePanel {
    pub root: PathBuf,
    expanded: HashSet<PathBuf>,
    children: HashMap<PathBuf, Vec<Entry>>,
    /// 変更のあるファイル → 状態の記号（M, A, D, ?）
    status: HashMap<PathBuf, char>,
    changed_dirs: HashSet<PathBuf>,
    open: Option<OpenFile>,
    refreshed: Instant,
    show_hidden: bool,
}

pub enum FileAction {
    OpenInEditor(PathBuf),
}

impl FilePanel {
    pub fn new(root: PathBuf) -> Self {
        let mut p = Self {
            root,
            expanded: HashSet::new(),
            children: HashMap::new(),
            status: HashMap::new(),
            changed_dirs: HashSet::new(),
            open: None,
            refreshed: Instant::now(),
            show_hidden: true,
        };
        p.reload();
        p
    }

    fn reload(&mut self) {
        let mut dirs: Vec<PathBuf> = self.expanded.iter().cloned().collect();
        dirs.push(self.root.clone());
        self.children.clear();
        for d in dirs {
            self.load_dir(&d);
        }
        self.load_status();
        self.refreshed = Instant::now();
    }

    fn load_dir(&mut self, dir: &Path) {
        let mut v: Vec<Entry> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if HIDE.contains(&name.as_str()) {
                            return None;
                        }
                        let path = e.path();
                        let dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
                        Some(Entry { name, path, dir })
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        self.children.insert(dir.to_path_buf(), v);
    }

    fn load_status(&mut self) {
        self.status.clear();
        self.changed_dirs.clear();
        let Ok(top) = Command::new("git").arg("-C").arg(&self.root).args(["rev-parse", "--show-toplevel"]).output() else { return };
        if !top.status.success() {
            return;
        }
        let top = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim());
        let Ok(out) = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["status", "--porcelain", "-z", "--untracked-files=all"])
            .output()
        else {
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut it = text.split('\0');
        while let Some(rec) = it.next() {
            if rec.len() < 4 {
                continue;
            }
            let code = &rec[..2];
            let path = top.join(&rec[3..]);
            if code.starts_with('R') || code.starts_with('C') {
                it.next(); // 元の名前
            }
            let c = if code.contains('?') {
                '?'
            } else if code.contains('D') {
                'D'
            } else if code.contains('A') {
                'A'
            } else {
                'M'
            };
            let mut p = path.parent();
            while let Some(d) = p {
                if !self.changed_dirs.insert(d.to_path_buf()) || d == top {
                    break;
                }
                p = d.parent();
            }
            self.status.insert(path, c);
        }
    }

    /// 確認用: そのファイルを開き、親のフォルダを展開する
    pub fn open_path(&mut self, path: &Path) {
        let mut p = path.parent();
        while let Some(d) = p {
            if !d.starts_with(&self.root) || d == self.root {
                break;
            }
            self.expanded.insert(d.to_path_buf());
            p = d.parent();
        }
        self.reload();
        self.open_file(path.to_path_buf());
    }

    fn open_file(&mut self, path: PathBuf) {
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let body = read_body(&path);
        let show_diff = self.open.as_ref().is_some_and(|o| o.path == path && o.show_diff);
        let diff = if self.status.contains_key(&path) { git::diff_file(&self.root, &path) } else { Vec::new() };
        self.open = Some(OpenFile { path, body, modified, show_diff, diff, checked: Instant::now() });
    }

    /// 開いているファイルが書き換わっていたら読み直す（エージェントが編集しているので）
    fn refresh_open(&mut self) {
        let Some(o) = &self.open else { return };
        if o.checked.elapsed() < Duration::from_millis(800) {
            return;
        }
        let now = std::fs::metadata(&o.path).and_then(|m| m.modified()).ok();
        if now != o.modified {
            let p = o.path.clone();
            self.open_file(p);
        } else if let Some(o) = &mut self.open {
            o.checked = Instant::now();
        }
    }

    pub fn show(&mut self, ui: &mut Ui) -> Vec<FileAction> {
        let mut acts = Vec::new();
        if self.refreshed.elapsed() > Duration::from_secs(3) {
            self.reload();
            if let Some(o) = &self.open {
                let p = o.path.clone();
                let had = !o.diff.is_empty();
                if had != self.status.contains_key(&p) {
                    self.open_file(p);
                }
            }
        }
        self.refresh_open();

        let tree_h = if self.open.is_some() { ui.available_height() * 0.42 } else { ui.available_height() };
        egui::TopBottomPanel::top("file-tree")
            .resizable(self.open.is_some())
            .default_height(tree_h)
            .height_range(80.0..=ui.available_height() - 60.0)
            .frame(Frame::new())
            .show_separator_line(self.open.is_some())
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(self.root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()).strong().color(TEXT));
                    if !self.status.is_empty() {
                        ui.label(RichText::new(format!("変更 {}", self.status.len())).size(11.0).color(WAITING));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("読み直す").clicked() {
                            self.reload();
                        }
                        ui.checkbox(&mut self.show_hidden, RichText::new("隠しファイル").size(11.0));
                    });
                });
                ui.add_space(4.0);
                ScrollArea::vertical().id_salt("tree").auto_shrink([false, false]).show(ui, |ui| {
                    let root = self.root.clone();
                    self.tree(ui, &root, 0);
                });
            });

        let mut close = false;
        if let Some(o) = &mut self.open {
            egui::CentralPanel::default().frame(Frame::new().inner_margin(Margin { top: 8, ..Default::default() })).show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    let rel = o.path.strip_prefix(&self.root).unwrap_or(&o.path).to_string_lossy().into_owned();
                    ui.add(Label::new(RichText::new(rel).strong().color(TEXT)).truncate());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("閉じる").clicked() {
                            close = true;
                        }
                        if ui.small_button("エディタ").clicked() {
                            acts.push(FileAction::OpenInEditor(o.path.clone()));
                        }
                        if !o.diff.is_empty() {
                            ui.selectable_value(&mut o.show_diff, true, RichText::new("差分").size(12.0));
                            ui.selectable_value(&mut o.show_diff, false, RichText::new("中身").size(12.0));
                        }
                    });
                });
                ui.add_space(4.0);
                if o.show_diff && !o.diff.is_empty() {
                    diff_view(ui, &o.diff, "file-diff");
                } else {
                    match &o.body {
                        Body::Message(m) => {
                            ui.add_space(12.0);
                            ui.label(RichText::new(m).color(MUTED));
                        }
                        Body::Text { text, lines } => code_view(ui, &o.path, text, *lines),
                    }
                }
            });
        }
        if close {
            self.open = None;
        }
        acts
    }

    fn tree(&mut self, ui: &mut Ui, dir: &Path, depth: usize) {
        if !self.children.contains_key(dir) {
            self.load_dir(dir);
        }
        let entries: Vec<(String, PathBuf, bool)> =
            self.children[dir].iter().map(|e| (e.name.clone(), e.path.clone(), e.dir)).collect();
        for (name, path, is_dir) in entries {
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            let open = self.expanded.contains(&path);
            let selected = self.open.as_ref().is_some_and(|o| o.path == path);
            let st = self.status.get(&path).copied();
            let changed_dir = is_dir && self.changed_dirs.contains(&path);
            let color = match st {
                Some('?') => OK.gamma_multiply(0.8),
                Some('D') => BAD,
                Some('A') => OK,
                Some(_) => WAITING,
                None if changed_dir => WAITING.gamma_multiply(0.8),
                None if name.starts_with('.') || HEAVY.contains(&name.as_str()) => FAINT,
                None => TERM_FG,
            };
            let id = Id::new(("tree", &path));
            let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
            let fill = if selected { CARD_SEL } else if hovered { CARD_HOVER } else { Color32::TRANSPARENT };
            let inner = Frame::new().fill(fill).corner_radius(4).inner_margin(Margin::symmetric(4, 1)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add_space(depth as f32 * 14.0);
                    let arrow = if is_dir { if open { "▾" } else { "▸" } } else { " " };
                    ui.label(RichText::new(arrow).font(FontId::monospace(12.0)).color(FAINT));
                    let label = if is_dir { format!("{name}/") } else { name.clone() };
                    ui.add(Label::new(RichText::new(label).size(13.0).color(color)).truncate());
                    if let Some(c) = st {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(c.to_string()).font(FontId::monospace(11.0)).color(color));
                        });
                    }
                });
            });
            let resp = ui.interact(inner.response.rect, id, Sense::click());
            if resp.clicked() {
                if is_dir {
                    if open {
                        self.expanded.remove(&path);
                    } else {
                        self.expanded.insert(path.clone());
                        self.load_dir(&path);
                    }
                } else {
                    self.open_file(path.clone());
                }
            }
            if is_dir && open {
                self.tree(ui, &path, depth + 1);
            }
        }
    }
}

fn read_body(path: &Path) -> Body {
    let Ok(meta) = std::fs::metadata(path) else {
        return Body::Message("ファイルがありません（消されたかもしれません）".into());
    };
    if meta.len() > MAX_BYTES {
        return Body::Message(format!("大きいので表示しません（{} KB）", meta.len() / 1024));
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.iter().take(8000).any(|b| *b == 0) {
                return Body::Message(format!("バイナリなので表示しません（{} KB）", bytes.len() / 1024));
            }
            let text = String::from_utf8_lossy(&bytes).replace('\t', "    ");
            let lines = text.lines().count().max(1);
            Body::Text { text, lines }
        }
        Err(e) => Body::Message(format!("読めませんでした: {e}")),
    }
}

fn code_view(ui: &mut Ui, path: &Path, text: &str, lines: usize) {
    let font = FontId::monospace(12.0);
    let row_h = ui.fonts(|f| f.row_height(&font));
    let digits = lines.to_string().len();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let lang = match ext.as_str() {
        "" => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        "tsx" | "jsx" | "mjs" | "cjs" => "js".into(),
        "toml" => "ini".into(),
        _ => ext,
    };
    if lines <= HIGHLIGHT_LINES {
        let theme = CodeTheme::dark(12.0);
        let job = syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, text, &lang);
        let numbers: String = (1..=lines).map(|n| format!("{n:>digits$}\n")).collect();
        ScrollArea::both().id_salt(("code", path)).auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.label(RichText::new(numbers).font(font.clone()).color(FAINT));
                ui.add(Label::new(job).extend());
            });
        });
    } else {
        // 長いファイルは色付けせず、見えている行だけ描く
        let all: Vec<&str> = text.lines().collect();
        ScrollArea::both().id_salt(("code", path)).auto_shrink([false, false]).show_rows(ui, row_h, all.len(), |ui, range| {
            for i in range {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{:>digits$}", i + 1)).font(font.clone()).color(FAINT));
                    ui.add(Label::new(RichText::new(all[i]).font(font.clone()).color(TERM_FG)).extend());
                });
            }
        });
    }
}

/// 色付きの差分（差分パネルとファイルの差分で使う）
pub fn diff_view(ui: &mut Ui, lines: &[DiffLine], id: &str) {
    let font = FontId::monospace(12.0);
    let row_h = ui.fonts(|f| f.row_height(&font)) + 2.0;
    ScrollArea::both().id_salt(id).auto_shrink([false, false]).show_rows(ui, row_h, lines.len(), |ui, range| {
        for l in &lines[range] {
            let (fg, bg) = match l.kind {
                Kind::File => (TEXT, CARD_SEL),
                Kind::Meta => (FAINT, Color32::TRANSPARENT),
                Kind::Hunk => (ACCENT, Color32::TRANSPARENT),
                Kind::Add => (OK, Color32::from_rgba_unmultiplied(0x8f, 0xd1, 0x7f, 18)),
                Kind::Del => (BAD, Color32::from_rgba_unmultiplied(0xf0, 0x74, 0x7c, 18)),
                Kind::Plain => (TERM_FG.gamma_multiply(0.85), Color32::TRANSPARENT),
            };
            let mut text = RichText::new(&l.text).font(font.clone()).color(fg);
            if l.kind == Kind::File {
                text = text.strong();
            }
            Frame::new().fill(bg).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.add(Label::new(text).extend());
            });
        }
    });
}
