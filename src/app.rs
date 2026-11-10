use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Id, Key, Label, Layout, Margin, Modifiers, RichText, ScrollArea,
    Sense, Stroke, TextEdit, Ui, vec2,
};

use crate::config::{self, Config};
use crate::files::{self, FileAction, FilePanel};
use crate::git;
use crate::notify;
use crate::picker::{self, Picker};
use crate::session::{Session, Wake};
use crate::setup::{self, Setup};
use crate::term::{self, TermState};
use crate::theme::*;
use crate::util::{expand, open_in};
pub use crate::util::tilde;

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Files,
    Diff,
}

enum Confirm {
    Kill(usize),
    Quit,
}

struct DiffPanel {
    cwd: PathBuf,
    diff: git::Diff,
}

/// 一覧のカードなどで押されたこと（描き終わってからまとめて実行する）
enum Action {
    Select(usize),
    New,
    Rename(usize),
    Kill(usize),
    Remove(usize),
    Diff(usize),
    Files(usize),
    Editor(usize),
    Finder(usize),
    Settings,
}

pub struct MureApp {
    cfg: Config,
    sessions: Vec<Session>,
    sel: usize,
    term: TermState,
    picker: Option<Picker>,
    setup: Option<Setup>,
    rename: Option<(usize, String)>,
    confirm: Option<Confirm>,
    side: Option<Side>,
    diff: Option<DiffPanel>,
    files: HashMap<PathBuf, FilePanel>,
    launch_dir: PathBuf,
    egui_ctx: egui::Context,
    flash: Option<(String, Instant)>,
    recent: Vec<String>,
    git_dirs: Arc<Mutex<Vec<PathBuf>>>,
    git_info: Arc<Mutex<HashMap<PathBuf, git::Info>>>,
    wake: Wake,
    allow_close: bool,
    shot: Option<crate::shot::Shot>,
    window_focused: bool,
}

impl MureApp {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, cfg_err: Option<String>, dir: PathBuf, start: Option<String>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let wake: Wake = Arc::new(move || ctx.request_repaint());
        let git_dirs = Arc::new(Mutex::new(Vec::new()));
        let git_info = Arc::new(Mutex::new(HashMap::new()));
        git::spawn_watcher(git_dirs.clone(), git_info.clone(), wake.clone());
        let mut recent = config::load_recent();
        if recent.is_empty() {
            recent.push(tilde(&dir));
        }
        let mut app = Self {
            term: TermState::new(cfg.font_size),
            cfg,
            sessions: Vec::new(),
            sel: 0,
            picker: None,
            setup: None,
            rename: None,
            confirm: None,
            side: None,
            diff: None,
            files: HashMap::new(),
            launch_dir: dir.clone(),
            egui_ctx: cc.egui_ctx.clone(),
            flash: cfg_err.map(|e| (e, Instant::now())),
            recent,
            git_dirs,
            git_info,
            wake,
            allow_close: false,
            shot: crate::shot::Shot::from_env(),
            window_focused: true,
        };
        // 初めて起動したときは設定から
        let shot_ui = app.shot.as_ref().map(|s| s.ui.clone()).unwrap_or_default();
        if (!config::exists() && app.shot.is_none()) || shot_ui == "setup" {
            app.setup = Some(Setup::new(&app.cfg, !config::exists(), &dir, &cc.egui_ctx));
        }
        if let Some(name) = start {
            match app.cfg.agents.iter().position(|a| a.name == name) {
                Some(agent) => {
                    if let Err(e) = app.create(&tilde(&dir), agent, "") {
                        app.flash(format!("{e:#}"));
                    }
                }
                None => app.flash(format!("エージェント「{name}」は設定にありません")),
            }
        }
        match shot_ui.as_str() {
            "new" => app.open_new(),
            "diff" if !app.sessions.is_empty() => app.apply(Action::Diff(0)),
            "files" if !app.sessions.is_empty() => {
                app.apply(Action::Files(0));
                if let Ok(f) = std::env::var("MURE_SHOT_OPEN") {
                    let cwd = app.sessions[0].cwd.clone();
                    if let Some(p) = app.files.get_mut(&cwd) {
                        p.open_path(&cwd.join(f));
                    }
                }
            }
            _ => {}
        }
        app
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    fn alive(&self) -> usize {
        self.sessions.iter().filter(|s| s.exit.is_none()).count()
    }

    fn dialog_open(&self) -> bool {
        self.picker.is_some() || self.setup.is_some() || self.rename.is_some() || self.confirm.is_some()
    }

    fn select(&mut self, i: usize) {
        if i < self.sessions.len() && i != self.sel {
            self.sel = i;
            self.term.reset();
            if self.side == Some(Side::Diff) {
                self.open_diff(i);
            }
        }
    }

    fn open_new(&mut self) {
        self.picker = Some(Picker::new(&self.cfg, &self.recent));
    }

    fn open_diff(&mut self, i: usize) {
        if let Some(s) = self.sessions.get(i) {
            self.diff = Some(DiffPanel { cwd: s.cwd.clone(), diff: git::diff(&s.cwd) });
        }
    }

    fn create(&mut self, dir: &str, agent: usize, branch: &str) -> Result<()> {
        let Some(agent) = self.cfg.agents.get(agent).cloned() else { bail!("エージェントがありません") };
        let dir = expand(dir);
        if !dir.is_dir() {
            bail!("フォルダがありません: {}", dir.display());
        }
        let dir = std::fs::canonicalize(&dir)?;
        let branch = branch.trim();
        let cwd = if branch.is_empty() { dir.clone() } else { git::ensure_worktree(&dir, branch)? };
        let name = if branch.is_empty() {
            cwd.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into())
        } else {
            branch.to_string()
        };
        let s = Session::spawn(name, &agent, cwd, self.term.size, self.cfg.scrollback, self.wake.clone())?;
        let d = tilde(&dir);
        self.recent.retain(|r| *r != d);
        self.recent.insert(0, d);
        self.recent.truncate(8);
        config::save_recent(&self.recent);
        self.sessions.push(s);
        self.sel = self.sessions.len() - 1;
        self.term.reset();
        self.sync_git_dirs();
        if self.side == Some(Side::Diff) {
            self.open_diff(self.sel);
        }
        Ok(())
    }

    fn sync_git_dirs(&self) {
        let mut dirs: Vec<PathBuf> = self.sessions.iter().map(|s| s.cwd.clone()).collect();
        dirs.dedup();
        *self.git_dirs.lock().unwrap() = dirs;
    }

    fn remove(&mut self, i: usize) {
        if i < self.sessions.len() {
            let mut s = self.sessions.remove(i);
            s.kill();
            if self.sel >= self.sessions.len() {
                self.sel = self.sessions.len().saturating_sub(1);
            }
            self.term.reset();
            self.sync_git_dirs();
            if self.sessions.is_empty() {
                self.side = None;
            }
        }
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::Select(i) => self.select(i),
            Action::New => self.open_new(),
            Action::Rename(i) => self.rename = Some((i, self.sessions[i].name.clone())),
            Action::Kill(i) => self.confirm = Some(Confirm::Kill(i)),
            Action::Remove(i) => self.remove(i),
            Action::Diff(i) => {
                self.select(i);
                if self.side == Some(Side::Diff) {
                    self.side = None;
                } else {
                    self.open_diff(i);
                    self.side = Some(Side::Diff);
                }
            }
            Action::Files(i) => {
                self.select(i);
                if self.side == Some(Side::Files) {
                    self.side = None;
                } else {
                    let cwd = self.sessions[i].cwd.clone();
                    self.files.entry(cwd.clone()).or_insert_with(|| FilePanel::new(cwd));
                    self.side = Some(Side::Files);
                }
            }
            Action::Editor(i) => {
                let cwd = self.sessions[i].cwd.clone();
                if !open_in(&self.cfg.editor_app, &cwd) {
                    self.flash(format!("{} で開けませんでした", self.cfg.editor_app));
                }
            }
            Action::Settings => {
                self.setup = Some(Setup::new(&self.cfg, false, &self.launch_dir, &self.egui_ctx));
            }
            Action::Finder(i) => {
                let _ = Command::new("open").arg(&self.sessions[i].cwd).status();
            }
        }
    }

    /// 終了の検出と、実行中／待ちの判定
    fn tick(&mut self) {
        let now = Instant::now();
        let idle = Duration::from_secs_f64(self.cfg.idle_secs.max(0.2));
        let min_busy = Duration::from_secs_f64(self.cfg.min_busy_secs.max(0.0));
        let viewing = self.window_focused && !self.dialog_open();
        for (i, s) in self.sessions.iter_mut().enumerate() {
            let watching = viewing && i == self.sel;
            if watching {
                s.attention = false;
            }
            if s.poll_exit() {
                if !watching {
                    s.attention = true;
                    if self.cfg.notify {
                        notify::send(&format!("{} が終了しました（{}）", s.name, s.exit.unwrap_or(0)), &s.agent);
                    }
                }
                continue;
            }
            if s.exit.is_some() {
                continue;
            }
            let busy = now.duration_since(s.last_output()) < idle && s.work_after_input() > Duration::from_millis(300);
            if s.busy && !busy && s.work_after_input() >= min_busy && !watching {
                s.attention = true;
                if self.cfg.notify {
                    notify::send(&format!("{} が待っています", s.name), &s.agent);
                }
            }
            s.busy = busy;
        }
        if self.flash.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(6)) {
            self.flash = None;
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.dialog_open() {
            return;
        }
        let cmd = Modifiers::COMMAND;
        let take = |k: Key| ctx.input_mut(|i| i.consume_key(cmd, k));
        if take(Key::N) {
            self.open_new();
        }
        if take(Key::D) && !self.sessions.is_empty() {
            self.apply(Action::Diff(self.sel));
        }
        if take(Key::E) && !self.sessions.is_empty() {
            self.apply(Action::Files(self.sel));
        }
        if take(Key::Comma) {
            self.apply(Action::Settings);
        }
        if take(Key::W) && !self.sessions.is_empty() {
            let i = self.sel;
            self.apply(if self.sessions[i].exit.is_some() { Action::Remove(i) } else { Action::Kill(i) });
        }
        if take(Key::OpenBracket) {
            self.select(self.sel.saturating_sub(1));
        }
        if take(Key::CloseBracket) {
            self.select(self.sel + 1);
        }
        if take(Key::Equals) || take(Key::Plus) {
            self.term.font_size = (self.term.font_size + 1.0).min(32.0);
        }
        if take(Key::Minus) {
            self.term.font_size = (self.term.font_size - 1.0).max(8.0);
        }
        if take(Key::Num0) {
            self.term.font_size = self.cfg.font_size;
        }
        let nums = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
        for (i, k) in nums.into_iter().enumerate() {
            if take(k) {
                self.select(i);
            }
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) -> Vec<Action> {
        let mut acts = Vec::new();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("mure").size(18.0).strong().color(TEXT));
            let busy = self.sessions.iter().filter(|s| s.busy).count();
            let waiting = self.sessions.iter().filter(|s| s.attention).count();
            if busy > 0 {
                ui.label(RichText::new(format!("実行 {busy}")).size(12.0).color(ACCENT));
            }
            if waiting > 0 {
                ui.label(RichText::new(format!("待ち {waiting}")).size(12.0).color(WAITING));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("＋ 新規").size(13.0)).corner_radius(6.0)).on_hover_text("⌘N").clicked() {
                    acts.push(Action::New);
                }
            });
        });
        ui.add_space(10.0);

        egui::TopBottomPanel::bottom("sidebar-foot").frame(Frame::new().inner_margin(Margin { top: 8, ..Default::default() })).show_separator_line(false).show_inside(ui, |ui| {
            if ui.add(egui::Button::new(RichText::new("設定").size(12.0).color(MUTED)).frame(false)).on_hover_text("⌘,").clicked() {
                acts.push(Action::Settings);
            }
        });

        if self.sessions.is_empty() {
            ui.label(RichText::new("セッションはまだありません").color(MUTED));
            return acts;
        }

        let info = self.git_info.lock().unwrap().clone();
        let now = Instant::now();
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, s) in self.sessions.iter().enumerate() {
                let selected = i == self.sel;
                let id = Id::new(("card", i));
                let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
                let fill = if selected { CARD_SEL } else if hovered { CARD_HOVER } else { CARD };
                let stroke = if s.attention && !selected {
                    Stroke::new(1.0, WAITING)
                } else if selected {
                    Stroke::new(1.0, ACCENT.gamma_multiply(0.5))
                } else {
                    Stroke::new(1.0, BORDER)
                };
                let inner = Frame::new()
                    .fill(fill)
                    .stroke(stroke)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            status_icon(ui, s);
                            ui.add(Label::new(RichText::new(&s.name).size(14.0).strong().color(if s.exit.is_some() { MUTED } else { TEXT })).truncate());
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if i < 9 {
                                    ui.label(RichText::new(format!("⌘{}", i + 1)).size(11.0).color(FAINT));
                                }
                            });
                        });
                        let g = info.get(&s.cwd);
                        let mut meta = vec![s.agent.clone()];
                        if let Some(b) = g.and_then(|g| g.branch.clone()).or_else(|| s.branch.clone()) {
                            if b != s.name {
                                meta.push(b);
                            }
                        }
                        meta.push(match s.exit {
                            Some(code) => format!("終了 ({code})"),
                            None => elapsed(now.duration_since(s.created).as_secs()),
                        });
                        ui.add(Label::new(RichText::new(meta.join(" · ")).size(12.0).color(MUTED)).truncate());
                        let pv = s.preview();
                        if !pv.is_empty() {
                            ui.add_space(2.0);
                            ui.add(Label::new(RichText::new(pv).font(FontId::monospace(11.0)).color(FAINT)).truncate());
                        }
                        if let Some(g) = g.filter(|g| g.files > 0) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.label(RichText::new(format!("+{}", g.added)).size(11.0).color(OK));
                                ui.label(RichText::new(format!("−{}", g.removed)).size(11.0).color(BAD));
                                ui.label(RichText::new(format!("{} ファイル", g.files)).size(11.0).color(FAINT));
                            });
                        }
                    });
                let resp = ui.interact(inner.response.rect, id, Sense::click());
                if resp.clicked() {
                    acts.push(Action::Select(i));
                }
                if resp.double_clicked() {
                    acts.push(Action::Rename(i));
                }
                resp.context_menu(|ui| {
                    if ui.button("名前を変える").clicked() {
                        acts.push(Action::Rename(i));
                        ui.close();
                    }
                    if ui.button("ファイルを見る").clicked() {
                        acts.push(Action::Files(i));
                        ui.close();
                    }
                    if ui.button("差分を見る").clicked() {
                        acts.push(Action::Diff(i));
                        ui.close();
                    }
                    if ui.button(format!("{} で開く", self.cfg.editor_app)).clicked() {
                        acts.push(Action::Editor(i));
                        ui.close();
                    }
                    if ui.button("Finder で開く").clicked() {
                        acts.push(Action::Finder(i));
                        ui.close();
                    }
                    ui.separator();
                    let label = if s.exit.is_some() { "一覧から外す" } else { "止める…" };
                    if ui.button(RichText::new(label).color(BAD)).clicked() {
                        acts.push(if s.exit.is_some() { Action::Remove(i) } else { Action::Kill(i) });
                        ui.close();
                    }
                });
                ui.add_space(8.0);
            }
        });
        acts
    }

    fn header(&mut self, ui: &mut Ui) -> Vec<Action> {
        let mut acts = Vec::new();
        let i = self.sel;
        let s = &self.sessions[i];
        let branch = self.git_info.lock().unwrap().get(&s.cwd).and_then(|g| g.branch.clone()).or_else(|| s.branch.clone());
        Frame::new().fill(HEADER_BG).inner_margin(Margin::symmetric(14, 9)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            // ボタンを右から先に置き、残りの幅に名前やフォルダを詰める（狭いときはフォルダを省略）
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if s.exit.is_some() {
                    if ui.button("一覧から外す").clicked() {
                        acts.push(Action::Remove(i));
                    }
                } else if ui.button(RichText::new("止める").color(BAD)).on_hover_text("⌘W").clicked() {
                    acts.push(Action::Kill(i));
                }
                if ui.button("エディタ").on_hover_text(format!("{} で開く", self.cfg.editor_app)).clicked() {
                    acts.push(Action::Editor(i));
                }
                if ui.selectable_label(self.side == Some(Side::Diff), "差分").on_hover_text("⌘D").clicked() {
                    acts.push(Action::Diff(i));
                }
                if ui.selectable_label(self.side == Some(Side::Files), "ファイル").on_hover_text("⌘E").clicked() {
                    acts.push(Action::Files(i));
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    status_chip(ui, s);
                    ui.add(Label::new(RichText::new(&s.name).size(15.0).strong().color(TEXT)).truncate());
                    ui.label(RichText::new(&s.agent).size(12.0).color(MUTED));
                    if let Some(b) = branch {
                        ui.label(RichText::new(b).size(12.0).color(MUTED));
                    }
                    ui.add(Label::new(RichText::new(tilde(&s.cwd)).size(12.0).color(FAINT)).truncate());
                });
            });
        });
        acts
    }

    fn diff_panel(&mut self, ui: &mut Ui) {
        let Some(d) = &mut self.diff else { return };
        let mut close = false;
        let mut reload = false;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("差分").size(15.0).strong().color(TEXT));
            ui.label(RichText::new("コミットしていない変更").size(12.0).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("閉じる").on_hover_text("⌘D").clicked() {
                    close = true;
                }
                if ui.button("読み直す").clicked() {
                    reload = true;
                }
            });
        });
        ui.add_space(6.0);
        if let Some(e) = &d.diff.error {
            ui.label(RichText::new(format!("git の状態を取れませんでした: {e}")).color(MUTED));
        } else if d.diff.status.is_empty() {
            ui.label(RichText::new("変更はありません").color(MUTED));
        } else {
            for l in d.diff.status.iter().take(30) {
                let (code, path) = l.split_at(l.len().min(3));
                let c = if code.contains('?') { FAINT } else if code.contains('D') { BAD } else if code.contains('A') { OK } else { WAITING };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(code.trim()).font(FontId::monospace(12.0)).color(c));
                    ui.label(RichText::new(path).size(12.0).color(TEXT));
                });
            }
            if d.diff.status.len() > 30 {
                ui.label(RichText::new(format!("ほか {} 件", d.diff.status.len() - 30)).size(12.0).color(MUTED));
            }
            ui.add_space(6.0);
            ui.separator();
            files::diff_view(ui, &d.diff.lines, "diff-all");
        }
        if close {
            self.side = None;
        } else if reload {
            let cwd = d.cwd.clone();
            d.diff = git::diff(&cwd);
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(mut p) = self.picker.take() {
            match p.show(ctx, &self.cfg) {
                picker::Outcome::Keep => self.picker = Some(p),
                picker::Outcome::Cancel => {}
                picker::Outcome::Start { dir, agent, branch } => {
                    if let Err(e) = self.create(&dir, agent, &branch) {
                        self.flash(format!("{e:#}"));
                        self.picker = Some(p);
                    }
                }
            }
        }

        if let Some(mut st) = self.setup.take() {
            match st.show(ctx) {
                setup::Outcome::Keep => self.setup = Some(st),
                setup::Outcome::Cancel => {}
                setup::Outcome::Save(c) => match config::save(&c) {
                    Ok(()) => {
                        self.term.font_size = c.font_size;
                        self.cfg = c;
                        self.flash("設定を保存しました");
                    }
                    Err(e) => {
                        self.flash(format!("設定を保存できませんでした: {e:#}"));
                        self.setup = Some(st);
                    }
                },
            }
        }

        if let Some((i, mut name)) = self.rename.take() {
            let mut keep = true;
            let modal = egui::Modal::new(Id::new("rename")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(RichText::new("名前を変える").size(16.0).strong());
                ui.add_space(8.0);
                ui.add(TextEdit::singleline(&mut name).desired_width(f32::INFINITY)).request_focus();
            });
            if ctx.input(|i| i.key_pressed(Key::Enter)) {
                if let Some(s) = self.sessions.get_mut(i) {
                    if !name.trim().is_empty() {
                        s.name = name.trim().to_string();
                    }
                }
                keep = false;
            }
            if modal.should_close() {
                keep = false;
            }
            if keep {
                self.rename = Some((i, name));
            }
        }

        if let Some(c) = self.confirm.take() {
            let mut keep = true;
            let mut yes = false;
            let (title, body) = match &c {
                Confirm::Kill(i) => (
                    "止める".to_string(),
                    format!("「{}」を止めます。worktree やファイルはそのまま残ります。", self.sessions.get(*i).map(|s| s.name.as_str()).unwrap_or("")),
                ),
                Confirm::Quit => ("終わる".to_string(), format!("動いているセッションが {} 個あります。全部止めて終わりますか？", self.alive())),
            };
            let modal = egui::Modal::new(Id::new("confirm")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(title).size(16.0).strong());
                ui.add_space(8.0);
                ui.label(body);
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("はい").color(BAD).strong())).clicked() {
                        yes = true;
                    }
                    if ui.button("やめる").clicked() {
                        keep = false;
                    }
                });
            });
            if ctx.input(|i| i.key_pressed(Key::Enter)) {
                yes = true;
            }
            if modal.should_close() {
                keep = false;
            }
            if yes {
                match c {
                    Confirm::Kill(i) => self.remove(i),
                    Confirm::Quit => {
                        self.allow_close = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            } else if keep {
                self.confirm = Some(c);
            }
        }
    }
}

impl eframe::App for MureApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(120));
        self.window_focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if let Some(shot) = &mut self.shot {
            if shot.update(ctx) {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && self.alive() > 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Quit);
        }

        self.shortcuts(ctx);
        self.tick();

        let mut acts = Vec::new();
        egui::SidePanel::left("sessions")
            .resizable(true)
            .default_width(280.0)
            .width_range(200.0..=480.0)
            .frame(Frame::new().fill(SIDEBAR_BG).inner_margin(Margin::same(12)))
            .show(ctx, |ui| acts.extend(self.sidebar(ui)));

        if let Some(side) = self.side.filter(|_| !self.sessions.is_empty()) {
            egui::SidePanel::right("side")
                .resizable(true)
                .default_width(600.0)
                .width_range(280.0..=1400.0)
                .frame(Frame::new().fill(SIDEBAR_BG).inner_margin(Margin::same(12)))
                .show(ctx, |ui| match side {
                    Side::Diff => self.diff_panel(ui),
                    Side::Files => {
                        let cwd = self.sessions[self.sel].cwd.clone();
                        let mut close = false;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("ファイル").size(15.0).strong().color(TEXT));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("閉じる").on_hover_text("⌘E").clicked() {
                                    close = true;
                                }
                            });
                        });
                        ui.add_space(6.0);
                        let panel = self.files.entry(cwd.clone()).or_insert_with(|| FilePanel::new(cwd));
                        for a in panel.show(ui) {
                            match a {
                                FileAction::OpenInEditor(p) => {
                                    if !open_in(&self.cfg.editor_app, &p) {
                                        self.flash = Some((format!("{} で開けませんでした", self.cfg.editor_app), Instant::now()));
                                    }
                                }
                            }
                        }
                        if close {
                            self.side = None;
                        }
                    }
                });
        }

        egui::CentralPanel::default().frame(Frame::new().fill(TERM_BG)).show(ctx, |ui| {
            if self.sessions.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.32);
                    ui.label(RichText::new("mure").size(34.0).strong().color(TEXT));
                    ui.add_space(6.0);
                    ui.label(RichText::new("エージェントを並べて動かす").size(14.0).color(MUTED));
                    ui.add_space(22.0);
                    if ui.add(egui::Button::new(RichText::new("＋ 新しいセッション").size(16.0)).min_size(vec2(220.0, 40.0)).corner_radius(8.0)).clicked() {
                        acts.push(Action::New);
                    }
                    ui.add_space(8.0);
                    ui.label(RichText::new("⌘N").size(12.0).color(FAINT));
                });
                return;
            }
            acts.extend(self.header(ui));
            let accept = !self.dialog_open();
            let sel = self.sel;
            if let Some((rows, cols)) = term::show(ui, &mut self.sessions[sel], &mut self.term, accept) {
                for s in &mut self.sessions {
                    s.resize(rows, cols);
                }
            }
        });

        if let Some((msg, _)) = &self.flash {
            egui::Area::new(Id::new("flash")).anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -24.0)).show(ctx, |ui| {
                Frame::new()
                    .fill(Color32::from_rgb(0x3a, 0x30, 0x1c))
                    .stroke(Stroke::new(1.0, WAITING))
                    .corner_radius(8)
                    .inner_margin(Margin::symmetric(14, 8))
                    .show(ui, |ui| ui.label(RichText::new(msg).color(WAITING)));
            });
        }

        for a in acts {
            self.apply(a);
        }
        self.dialogs(ctx);
    }
}

fn status_icon(ui: &mut Ui, s: &Session) {
    if let Some(code) = s.exit {
        let (t, c) = if code == 0 { ("✓", OK) } else { ("✕", BAD) };
        ui.label(RichText::new(t).color(c).strong());
    } else if s.attention {
        ui.label(RichText::new("●").color(WAITING));
    } else if s.busy {
        ui.add(egui::Spinner::new().size(12.0).color(ACCENT));
    } else {
        ui.label(RichText::new("○").color(FAINT));
    }
}

fn status_chip(ui: &mut Ui, s: &Session) {
    let (t, c) = match s.exit {
        Some(0) => ("終了".to_string(), OK),
        Some(code) => (format!("終了 {code}"), BAD),
        None if s.attention => ("待ち".into(), WAITING),
        None if s.busy => ("実行中".into(), ACCENT),
        None => ("静か".into(), MUTED),
    };
    Frame::new().fill(c.gamma_multiply(0.18)).corner_radius(10).inner_margin(Margin::symmetric(8, 2)).show(ui, |ui| {
        ui.label(RichText::new(t).size(11.0).color(c).strong());
    });
}

fn elapsed(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}秒"),
        60..3600 => format!("{}分", secs / 60),
        _ => format!("{}時間{}分", secs / 3600, secs / 60 % 60),
    }
}
