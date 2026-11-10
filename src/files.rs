//! フォルダのツリーとファイルの中身

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use eframe::egui::{vec2, Align, Color32, FontId, Frame, Id, Label, Layout, Margin, RichText, ScrollArea, Sense, Ui};

use crate::git::{DiffLine, Kind};
use crate::theme::*;
use crate::ui;

const HIDE: &[&str] = &[".git", ".DS_Store"];
const HEAVY: &[&str] = &["node_modules", "target", "dist", "build", ".next", ".venv", "venv", "__pycache__", ".turbo", ".cache"];

struct Entry {
    name: String,
    path: PathBuf,
    dir: bool,
}

pub struct FilePanel {
    /// 1つ目のフォルダ（エージェントが動いている場所）
    pub root: PathBuf,
    /// 見せるフォルダ（ワークスペースなら複数）
    pub roots: Vec<PathBuf>,
    /// ワークスペースの名前（あれば見出しに出す）
    pub title: Option<String>,
    expanded: HashSet<PathBuf>,
    children: HashMap<PathBuf, Vec<Entry>>,
    /// 変更のあるファイル → 状態の記号（M, A, D, ?）
    status: HashMap<PathBuf, char>,
    changed_dirs: HashSet<PathBuf>,
    refreshed: Instant,
    show_hidden: bool,
    pub scroll_to_active: bool,
}

pub enum FileAction {
    Open(PathBuf),
}

/// 差分のまとまりから「聞く」が押された
pub struct AskHunk {
    pub file: String,
    pub range: (usize, usize),
}

impl FilePanel {
    pub fn new(roots: Vec<PathBuf>, title: Option<String>) -> Self {
        let root = roots[0].clone();
        let mut expanded = HashSet::new();
        if roots.len() > 1 {
            expanded.extend(roots.iter().cloned());
        }
        let mut p = Self {
            root,
            roots,
            title,
            expanded,
            children: HashMap::new(),
            status: HashMap::new(),
            changed_dirs: HashSet::new(),
            refreshed: Instant::now(),
            show_hidden: true,
            scroll_to_active: false,
        };
        p.reload();
        p
    }

    fn reload(&mut self) {
        let mut dirs: Vec<PathBuf> = self.expanded.iter().cloned().collect();
        dirs.extend(self.roots.iter().cloned());
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
        for r in self.roots.clone() {
            self.load_status_of(&r);
        }
    }

    fn load_status_of(&mut self, root: &Path) {
        let Ok(top) = Command::new("git").arg("-C").arg(root).args(["rev-parse", "--show-toplevel"]).output() else { return };
        if !top.status.success() {
            return;
        }
        let top = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim());
        let Ok(out) = Command::new("git")
            .arg("-C")
            .arg(root)
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

    /// そのファイルが見えるように親のフォルダを開く
    pub fn reveal(&mut self, path: &Path) {
        let mut p = path.parent();
        while let Some(d) = p {
            if !self.roots.iter().any(|r| d.starts_with(r)) || self.roots.iter().any(|r| r == d) {
                break;
            }
            if self.expanded.insert(d.to_path_buf()) {
                self.load_dir(d);
            }
            p = d.parent();
        }
    }

    pub fn show(&mut self, ui: &mut Ui, active: Option<&Path>) -> Vec<FileAction> {
        let mut acts = Vec::new();
        if self.refreshed.elapsed() > Duration::from_secs(3) {
            self.reload();
        }
        ui.horizontal(|ui| {
            let title = self.title.clone().unwrap_or_else(|| self.root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
            if self.title.is_some() {
                ui.label(RichText::new(ui::icon::CODE).size(13.0).color(ACCENT));
            }
            ui.label(RichText::new(title).strong().color(TEXT));
            if !self.status.is_empty() {
                ui.label(RichText::new(lf!("変更 {}", "{} changed", self.status.len())).size(11.0).color(WAITING));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui::icon_button(ui, ui::icon::ARROW_CLOCKWISE, l!("読み直す", "Reload"), false).clicked() {
                    self.reload();
                }
                let (g, tip) = if self.show_hidden { (ui::icon::EYE, l!("隠しファイルを隠す", "Hide hidden files")) } else { (ui::icon::EYE_SLASH, l!("隠しファイルも出す", "Show hidden files")) };
                if ui::icon_button(ui, g, tip, false).clicked() {
                    self.show_hidden = !self.show_hidden;
                }
            });
        });
        ui.add_space(4.0);
        ScrollArea::vertical().id_salt("tree").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.spacing_mut().interact_size.y = 18.0;
            if self.roots.len() == 1 {
                let root = self.root.clone();
                self.tree(ui, &root, 0, active, &mut acts);
            } else {
                // ワークスペース: フォルダごとに1つずつ（VSCode と同じ）
                for root in self.roots.clone() {
                    let open = self.expanded.contains(&root);
                    let name = root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let id = Id::new(("root", &root));
                    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
                    let changed = self.changed_dirs.contains(&root) || self.status.keys().any(|p| p.starts_with(&root));
                    let inner = Frame::new()
                        .fill(if hovered { CARD_HOVER } else { Color32::TRANSPARENT })
                        .corner_radius(5)
                        .inner_margin(Margin { left: 4, right: 6, top: 6, bottom: 4 })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                let caret = if open { ui::icon::CARET_DOWN } else { ui::icon::CARET_RIGHT };
                                ui.add_sized(vec2(10.0, 14.0), Label::new(RichText::new(caret).size(10.0).color(FAINT)));
                                ui.label(RichText::new(name.to_uppercase()).font(ui::bold(11.0)).color(if changed { WAITING } else { MUTED }));
                            });
                        });
                    if ui.interact(inner.response.rect, id, Sense::click()).on_hover_text(root.to_string_lossy()).clicked() {
                        if open {
                            self.expanded.remove(&root);
                        } else {
                            self.expanded.insert(root.clone());
                        }
                    }
                    if open {
                        self.tree(ui, &root, 0, active, &mut acts);
                    }
                    ui.add_space(4.0);
                }
            }
        });
        acts
    }

    fn tree(&mut self, ui: &mut Ui, dir: &Path, depth: usize, active: Option<&Path>, acts: &mut Vec<FileAction>) {
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
            let selected = active == Some(path.as_path());
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
            let inner = Frame::new().fill(fill).corner_radius(5).inner_margin(Margin::symmetric(6, 3)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    ui.add_space(depth as f32 * 14.0);
                    let caret = if is_dir { if open { ui::icon::CARET_DOWN } else { ui::icon::CARET_RIGHT } } else { " " };
                    ui.add_sized(vec2(10.0, 14.0), Label::new(RichText::new(caret).size(10.0).color(FAINT)));
                    let g = if is_dir { if open { ui::icon::FOLDER_OPEN } else { ui::icon::FOLDER_SIMPLE } } else { ui::icon::FILE };
                    let gc = if is_dir { ACCENT.gamma_multiply(0.7) } else { FAINT };
                    ui.label(RichText::new(g).size(13.0).color(gc));
                    ui.add(Label::new(RichText::new(&name).size(12.5).color(color)).truncate());
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
                    acts.push(FileAction::Open(path.clone()));
                }
            }
            if selected && self.scroll_to_active {
                resp.scroll_to_me(Some(Align::Center));
                self.scroll_to_active = false;
            }
            if is_dir && open {
                self.tree(ui, &path, depth + 1, active, acts);
            }
        }
    }
}

/// 色付きの差分（差分パネルとファイルの差分で使う）
pub fn diff_view(ui: &mut Ui, lines: &[DiffLine], id: &str) -> Option<AskHunk> {
    let mut ask = None;
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
                ui.horizontal(|ui| {
                    ui.add(Label::new(text).extend());
                    if let Some(range) = l.range {
                        if ui.small_button(RichText::new(l!("聞く", "Ask")).size(11.0)).on_hover_text(l!("このまとまりについてエージェントに聞く", "Ask the agent about this hunk")).clicked() {
                            ask = Some(AskHunk { file: l.file.clone(), range });
                        }
                    }
                });
            });
        }
    });
    ask
}
