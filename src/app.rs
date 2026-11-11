use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Id, Key, Label, Layout, Margin, Modifiers, Rect, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, vec2,
};

use crate::children;
use crate::config::{self, Agent, Config};
use crate::resume::{self, Saved};
use crate::editor::{self, Doc, DocAction};
use crate::quickopen::{self, QuickOpen};
use crate::search::Search;
use crate::files::{self, FileAction, FilePanel};
use crate::git;
use crate::notify;
use crate::picker::{self, Picker};
use crate::session::{Session, Wake};
use crate::setup::{self, Setup};
use crate::term::{self, TermState};
use crate::theme::*;
use crate::ui;
use crate::util::{expand, open_in};
pub use crate::util::tilde;

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Files,
    Diff,
    Search,
    /// ほかのエージェントに呼ばれた codex の中身
    Child,
    /// 誰が誰を呼んだかを時間の流れで見る
    Map,
}

/// 右のパネルで見ている「呼ばれた codex」
struct ChildView {
    id: String,
    kind: children::Kind,
    label: String,
    file: PathBuf,
    steps: Vec<children::Step>,
    done: bool,
    mtime: Option<std::time::SystemTime>,
}

impl ChildView {
    fn open(c: &children::Child) -> Self {
        let mut v = Self { id: c.id.clone(), kind: c.kind, label: c.label.clone(), file: c.file.clone(), steps: Vec::new(), done: c.status == children::Status::Done, mtime: None };
        v.refresh();
        v
    }

    fn refresh(&mut self) {
        let m = std::fs::metadata(&self.file).and_then(|m| m.modified()).ok();
        if m != self.mtime || self.mtime.is_none() {
            let (steps, done) = children::read_steps(self.kind, &self.file);
            self.steps = steps;
            // 子エージェントは meta.json の finished が確か
            if self.kind != children::Kind::ClaudeSub || done {
                self.done = done || self.done;
            }
            self.mtime = m;
        }
    }
}

enum Confirm {
    Kill(usize),
    Quit,
    /// 保存していないファイルを閉じる（セッションのフォルダ, タブ）
    CloseDoc(PathBuf, usize),
}

/// セッションのフォルダごとの、ツリー・開いているファイル・検索
struct Workspace {
    tree: FilePanel,
    docs: Vec<Doc>,
    /// None ならエージェントのタブ
    active: Option<usize>,
    search: Search,
}

impl Workspace {
    fn for_session(s: &Session) -> Self {
        Self { tree: FilePanel::new(s.view_roots(), s.ws_title()), docs: Vec::new(), active: None, search: Search::default() }
    }

    fn open(&mut self, path: PathBuf, line: Option<usize>) {
        let i = match self.docs.iter().position(|d| d.path == path) {
            Some(i) => i,
            None => {
                self.docs.push(Doc::open(path.clone()));
                self.docs.len() - 1
            }
        };
        self.docs[i].goto = line.or(self.docs[i].goto);
        self.active = Some(i);
        self.tree.reveal(&path);
        self.tree.scroll_to_active = true;
    }

    fn close(&mut self, i: usize) {
        if i < self.docs.len() {
            self.docs.remove(i);
        }
        self.active = match self.active {
            Some(a) if a == i => if self.docs.is_empty() { None } else { Some(i.min(self.docs.len() - 1)) },
            Some(a) if a > i => Some(a - 1),
            other => other,
        };
    }

    fn dirty(&self) -> usize {
        self.docs.iter().filter(|d| d.dirty()).count()
    }
}

/// エージェントに聞く（入力欄に入れる文を作る）
struct Ask {
    rel: String,
    lines: Option<(usize, usize)>,
    comment: String,
    first: bool,
}

impl Ask {
    fn text(&self) -> String {
        let place = match self.lines {
            Some((a, b)) if a != b => lf!("{} の {a}〜{b} 行目", "{} lines {a}–{b}", self.rel),
            Some((a, _)) => lf!("{} の {a} 行目", "{} line {a}", self.rel),
            None => self.rel.clone(),
        };
        let c = self.comment.trim();
        if c.is_empty() { format!("{place}: ") } else { format!("{place}: {c}") }
    }
}

enum TabAct {
    Select(Option<usize>),
    Close(usize),
}

struct DiffPanel {
    roots: Vec<PathBuf>,
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
    Search(usize),
    Open(PathBuf, Option<usize>),
    Editor(usize),
    Finder(usize),
    Settings,
    Resume(usize),
    ResumeAll,
    Forget(usize),
    Child(String),
}

pub struct PaibotAgentsApp {
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
    ws: HashMap<PathBuf, Workspace>,
    quick: Option<QuickOpen>,
    ask: Option<Ask>,
    /// ファイルを開いているとき、エージェントを下に出す
    dock: bool,
    grab_term: bool,
    /// 前回のセッション（まだ再開していないもの）
    dormant: Vec<Saved>,
    persisted: Instant,
    launch_dir: PathBuf,
    egui_ctx: egui::Context,
    flash: Option<(String, Instant)>,
    recent: Vec<String>,
    git_dirs: Arc<Mutex<Vec<PathBuf>>>,
    git_info: Arc<Mutex<HashMap<PathBuf, git::Info>>>,
    children: Arc<Mutex<Vec<children::Child>>>,
    children_watch: Arc<Mutex<children::Watch>>,
    session_cmds: children::SessionCommands,
    child_view: Option<ChildView>,
    map_window: crate::map::Window,
    map_view: crate::map::View,
    wake: Wake,
    allow_close: bool,
    shot: Option<crate::shot::Shot>,
    /// paibot.app とのつながり（つないでいなければ None）
    runner: Option<Arc<Mutex<crate::runner::Status>>>,
    window_focused: bool,
}

impl PaibotAgentsApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: Config,
        cfg_err: Option<String>,
        dir: PathBuf,
        start: Option<String>,
        workspace: Option<PathBuf>,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        let wake: Wake = Arc::new(move || ctx.request_repaint());
        let git_dirs = Arc::new(Mutex::new(Vec::new()));
        let git_info = Arc::new(Mutex::new(HashMap::new()));
        git::spawn_watcher(git_dirs.clone(), git_info.clone(), wake.clone());
        let kids = Arc::new(Mutex::new(Vec::new()));
        let kids_watch = Arc::new(Mutex::new(children::Watch::default()));
        let session_cmds: children::SessionCommands = Arc::new(Mutex::new(HashMap::new()));
        children::spawn_watcher(kids_watch.clone(), kids.clone(), session_cmds.clone(), wake.clone());
        let mut recent = config::load_recent();
        if recent.is_empty() && workspace.is_none() {
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
            ws: HashMap::new(),
            quick: None,
            ask: None,
            dock: true,
            grab_term: false,
            dormant: resume::load(),
            persisted: Instant::now(),
            launch_dir: dir.clone(),
            egui_ctx: cc.egui_ctx.clone(),
            flash: cfg_err.map(|e| (e, Instant::now())),
            recent,
            git_dirs,
            git_info,
            children: kids,
            children_watch: kids_watch,
            session_cmds,
            child_view: None,
            map_window: crate::map::Window::All,
            map_view: crate::map::View::Graph,
            wake,
            allow_close: false,
            shot: crate::shot::Shot::from_env(),
            runner: None,
            window_focused: true,
        };
        // paibot.app とつないでいれば、裏で仕事を受ける（確認用に画面を撮るときはつながない）
        if !app.cfg.paibot_token.is_empty() && app.shot.is_none() {
            let st = Arc::new(Mutex::new(crate::runner::Status::default()));
            crate::runner::spawn(
                crate::runner::Settings { url: app.cfg.paibot_url.clone(), token: app.cfg.paibot_token.clone(), bots: app.cfg.bots.clone() },
                st.clone(),
                app.wake.clone(),
            );
            app.runner = Some(st);
        }
        // 初めて起動したときは設定から
        let shot_ui = app.shot.as_ref().map(|s| s.ui.clone()).unwrap_or_default();
        if (!config::exists() && app.shot.is_none()) || shot_ui == "setup" {
            app.setup = Some(Setup::new(&app.cfg, !config::exists(), &dir, &cc.egui_ctx));
        }
        if let Some(name) = start {
            match app.cfg.agents.iter().position(|a| a.name == name) {
                Some(agent) => {
                    if let Err(e) = app.create(&tilde(&dir), agent, "", Vec::new(), None) {
                        app.flash(format!("{e:#}"));
                    }
                }
                None => app.flash(lf!("エージェント「{name}」は設定にありません", "Agent \"{name}\" is not in the settings")),
            }
        }
        // ワークスペースを指定して開いたら、その項目を先頭で選んだ状態にする
        if let Some(f) = workspace {
            app.open_new();
            if let Some(p) = app.picker.as_mut() {
                p.open_workspace(f);
                p.opened_workspace = None;
            }
        }
        // 確認用: ワークスペースのセッションを1つ始める
        if let (Ok(f), true) = (std::env::var("PAIBOT_AGENTS_SHOT_START_WS"), app.shot.is_some()) {
            if let Some(p) = crate::projects::workspace_projects(std::path::Path::new(&f)).into_iter().find(|p| !p.roots.is_empty()) {
                let agent = app.cfg.default_agent_index();
                let name = p.name.trim_end_matches(l!("（ワークスペース）", " (workspace)")).to_string();
                if let Err(e) = app.create(&p.path, agent, "", p.roots, Some(name)) {
                    app.flash(format!("{e:#}"));
                }
            }
        }
        if let (Ok(list), true) = (std::env::var("PAIBOT_AGENTS_SHOT_AGENTS"), app.shot.is_some()) {
            for item in list.split(',') {
                // 「エージェント@フォルダ」でフォルダも選べる
                let (name, at) = item.split_once('@').map(|(a, b)| (a, b.to_string())).unwrap_or((item, tilde(&dir)));
                if let Some(agent) = app.cfg.agents.iter().position(|a| a.name == name.trim()) {
                    if let Err(e) = app.create(&at, agent, "", Vec::new(), None) {
                        app.flash(format!("{e:#}"));
                    }
                }
            }
        }
        match shot_ui.as_str() {
            "map" => app.side = Some(Side::Map),
            "timeline" => {
                app.side = Some(Side::Map);
                app.map_view = crate::map::View::Timeline;
            }
            "resume" => app.apply(Action::ResumeAll),
            "new" => {
                app.open_new();
                if let (Ok(f), Some(p)) = (std::env::var("PAIBOT_AGENTS_SHOT_WORKSPACE"), app.picker.as_mut()) {
                    p.open_workspace(PathBuf::from(f));
                }
            }
            "diff" if !app.sessions.is_empty() => app.apply(Action::Diff(0)),
            "files" | "ask" | "quick" | "search" if !app.sessions.is_empty() => {
                app.apply(Action::Files(0));
                if let Ok(f) = std::env::var("PAIBOT_AGENTS_SHOT_OPEN") {
                    let p = app.sessions[0].cwd.join(f);
                    app.apply(Action::Open(p, Some(30)));
                }
                if shot_ui == "ask" {
                    if let Some(w) = app.cur_ws() {
                        if let Some(d) = w.active.map(|i| &w.docs[i]) {
                            let rel = crate::util::rel_for_agent(&w.tree.root, &d.path);
                            app.ask = Some(Ask { rel, lines: Some((30, 42)), comment: l!("ここ、もっと短く書ける？", "Can this be shorter?").into(), first: true });
                        }
                    }
                }
                if shot_ui == "quick" {
                    app.quick = Some(QuickOpen::new(app.sessions[0].view_roots()));
                }
                if shot_ui == "search" {
                    app.side = Some(Side::Search);
                    let q = std::env::var("PAIBOT_AGENTS_SHOT_QUERY").unwrap_or_else(|_| "fn ".into());
                    let roots = app.sessions[0].view_roots();
                    if let Some(w) = app.cur_ws() {
                        w.search.set_and_run(&q, &roots);
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

    /// 今のセッションのワークスペース（なければ作る）
    fn cur_ws(&mut self) -> Option<&mut Workspace> {
        let i = self.sel;
        let cwd = self.sessions.get(i)?.cwd.clone();
        Some(self.ws.entry(cwd).or_insert_with(|| Workspace::for_session(&self.sessions[i])))
    }

    fn dirty_docs(&self) -> usize {
        self.ws.values().map(|w| w.dirty()).sum()
    }

    /// 今開いているファイルについて聞く
    fn ask_active(&mut self) {
        let Some(w) = self.cur_ws() else { return };
        let Some(i) = w.active else { return };
        let d = &w.docs[i];
        let rel = crate::util::rel_for_agent(&w.tree.root, &d.path);
        let lines = d.selection;
        self.ask = Some(Ask { rel, lines, comment: String::new(), first: true });
    }

    fn save_active(&mut self) {
        let Some(w) = self.cur_ws() else { return };
        let Some(i) = w.active else { return };
        let r = w.docs[i].save();
        match r {
            Ok(()) => {}
            Err(e) => self.flash(lf!("保存できませんでした: {e}", "Could not save: {e}")),
        }
    }

    fn dialog_open(&self) -> bool {
        self.picker.is_some()
            || self.setup.is_some()
            || self.rename.is_some()
            || self.confirm.is_some()
            || self.quick.is_some()
            || self.ask.is_some()
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
            let roots = s.view_roots();
            self.diff = Some(DiffPanel { diff: git::diff_multi(&roots), roots });
        }
    }

    fn create(&mut self, dir: &str, agent: usize, branch: &str, roots: Vec<PathBuf>, name: Option<String>) -> Result<()> {
        let Some(agent) = self.cfg.agents.get(agent).cloned() else { bail!("{}", lf!("エージェントがありません", "No agent")) };
        if !roots.is_empty() {
            // ワークスペース: 1つ目のフォルダで起動し、残りはエージェントに足す
            let roots: Vec<PathBuf> = roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).filter(|r| r.is_dir()).collect();
            let Some(first) = roots.first().cloned() else { bail!("{}", lf!("ワークスペースのフォルダが見つかりません", "None of the workspace folders were found")) };
            let name = name.unwrap_or_else(|| first.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
            return self.spawn(name, &agent, first, None, roots);
        }
        let dir = expand(dir);
        if !dir.is_dir() {
            bail!("{}", lf!("フォルダがありません: {}", "Folder not found: {}", dir.display()));
        }
        let dir = std::fs::canonicalize(&dir)?;
        let branch = branch.trim();
        let cwd = if branch.is_empty() { dir.clone() } else { git::ensure_worktree(&dir, branch)? };
        let name = if branch.is_empty() {
            cwd.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into())
        } else {
            branch.to_string()
        };
        let d = tilde(&dir);
        self.recent.retain(|r| *r != d);
        self.recent.insert(0, d);
        self.recent.truncate(8);
        config::save_recent(&self.recent);
        self.spawn(name, &agent, cwd, None, Vec::new())
    }

    /// エージェントを起動して一覧に加える。saved があればその続きから
    fn spawn(&mut self, name: String, agent: &Agent, cwd: PathBuf, saved: Option<&Saved>, roots: Vec<PathBuf>) -> Result<()> {
        let base = agent.cmd.clone();
        let resumed = saved
            .filter(|s| s.resumable())
            .and_then(|s| s.resume_id.as_deref().and_then(|id| resume::resume_cmd(&base, id).map(|c| (c, Some(id.to_string())))));
        let (cmd, id) = resumed.unwrap_or_else(|| resume::start_cmd(&base));
        let cmd = if roots.len() > 1 { resume::add_dirs(&cmd, &roots[1..]) } else { cmd };
        let run = Agent { name: agent.name.clone(), cmd };
        let mut s = Session::spawn(name, &run, cwd.clone(), self.term.size, self.cfg.scrollback, self.wake.clone())?;
        s.base_cmd = base;
        s.resume_id = id;
        s.roots = roots;
        self.sessions.push(s);
        self.sel = self.sessions.len() - 1;
        self.term.reset();
        self.sync_git_dirs();
        if self.side == Some(Side::Diff) {
            self.open_diff(self.sel);
        }
        if let Some(saved) = saved.filter(|s| !s.tabs.is_empty()) {
            let i = self.sessions.len() - 1;
            let w = self.ws.entry(cwd).or_insert_with(|| Workspace::for_session(&self.sessions[i]));
            for t in &saved.tabs {
                if t.is_file() && !w.docs.iter().any(|d| d.path == *t) {
                    w.docs.push(Doc::open(t.clone()));
                }
            }
        }
        self.persist();
        Ok(())
    }

    fn resume_dormant(&mut self, i: usize) {
        if i >= self.dormant.len() {
            return;
        }
        let saved = self.dormant.remove(i);
        let agent = self
            .cfg
            .agents
            .iter()
            .find(|a| a.name == saved.agent)
            .cloned()
            .unwrap_or_else(|| Agent { name: saved.agent.clone(), cmd: saved.cmd.clone() });
        if let Err(e) = self.spawn(saved.name.clone(), &agent, saved.cwd.clone(), Some(&saved), saved.roots.clone()) {
            self.flash(lf!("{} を再開できませんでした: {e:#}", "Could not resume {}: {e:#}", saved.name));
            self.dormant.insert(i, saved);
        }
    }

    /// 今のセッションを覚える（次に開いたとき「前回のセッション」として出す）
    fn persist(&mut self) {
        self.persisted = Instant::now();
        // codex は始めたあとに会話の ID を探す
        let mut taken: Vec<String> = self.sessions.iter().filter_map(|s| s.resume_id.clone()).collect();
        for s in self.sessions.iter_mut() {
            if s.exit.is_none() && s.resume_id.is_none() && resume::kind(&s.base_cmd) == resume::Kind::Codex {
                if let Some(id) = resume::find_codex_id(&s.cwd, s.started_at, &taken) {
                    taken.push(id.clone());
                    s.resume_id = Some(id);
                }
            }
        }
        let mut list: Vec<Saved> = self
            .sessions
            .iter()
            .filter(|s| s.exit.is_none())
            .map(|s| Saved {
                name: s.name.clone(),
                agent: s.agent.clone(),
                cmd: s.base_cmd.clone(),
                cwd: s.cwd.clone(),
                resume_id: s.resume_id.clone(),
                tabs: self.ws.get(&s.cwd).map(|w| w.docs.iter().map(|d| d.path.clone()).collect()).unwrap_or_default(),
                roots: s.roots.clone(),
            })
            .collect();
        list.extend(self.dormant.iter().cloned());
        if self.shot.is_none() || std::env::var_os("PAIBOT_AGENTS_CONFIG_DIR").is_some() {
            resume::save(&list);
        }
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
            self.persist();
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
                    self.cur_ws();
                    self.side = Some(Side::Files);
                }
            }
            Action::Search(i) => {
                self.select(i);
                if self.side == Some(Side::Search) {
                    self.side = None;
                } else if let Some(w) = self.cur_ws() {
                    w.search.focus = true;
                    self.side = Some(Side::Search);
                }
            }
            Action::Open(p, line) => {
                if let Some(w) = self.cur_ws() {
                    w.open(p, line);
                }
            }
            Action::Editor(i) => {
                let cwd = self.sessions[i].cwd.clone();
                if !open_in(&self.cfg.editor_app, &cwd) {
                    self.flash(lf!("{} で開けませんでした", "Could not open with {}", self.cfg.editor_app));
                }
            }
            Action::Resume(i) => self.resume_dormant(i),
            Action::ResumeAll => {
                while !self.dormant.is_empty() {
                    let before = self.dormant.len();
                    self.resume_dormant(0);
                    if self.dormant.len() == before {
                        break;
                    }
                }
            }
            Action::Child(id) => {
                if self.side == Some(Side::Child) && self.child_view.as_ref().is_some_and(|v| v.id == id) {
                    self.side = None;
                } else if let Some(c) = self.children.lock().unwrap().iter().find(|c| c.id == id) {
                    self.child_view = Some(ChildView::open(c));
                    self.side = Some(Side::Child);
                }
            }
            Action::Forget(i) => {
                if i < self.dormant.len() {
                    self.dormant.remove(i);
                    self.persist();
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
    /// セッションごとの呼び出しのツリー（呼ばれたものが、さらに呼んだものも）
    fn call_tree(&self) -> Vec<Vec<children::Node>> {
        let cmds = self.session_cmds.lock().unwrap().clone();
        let refs: Vec<children::SessionRef> = self
            .sessions
            .iter()
            .map(|s| children::SessionRef {
                conv_id: s.resume_id.clone(),
                roots: s.view_roots(),
                started: s.started_at,
                commands: s.resume_id.as_ref().and_then(|id| cmds.get(id).cloned()),
            })
            .collect();
        children::build_tree(&self.children.lock().unwrap(), &refs)
    }

    fn tick(&mut self) {
        // 呼ばれた codex を探す範囲（動いているセッションのうち一番古い開始時刻から）
        {
            let alive: Vec<&Session> = self.sessions.iter().filter(|s| s.exit.is_none()).collect();
            let mut w = self.children_watch.lock().unwrap();
            w.since = alive.iter().map(|s| s.started_at).min();
            w.dirs = alive.iter().flat_map(|s| s.view_roots()).collect();
            w.claude_ids = alive
                .iter()
                .filter(|s| resume::kind(&s.base_cmd) == resume::Kind::Claude)
                .filter_map(|s| s.resume_id.clone().map(|id| (id, s.cwd.clone())))
                .collect();
        }
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
                        notify::send(&lf!("{} が終了しました（{}）", "{} exited ({})", s.name, s.exit.unwrap_or(0)), &s.agent);
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
                    notify::send(&lf!("{} が待っています", "{} is waiting for you", s.name), &s.agent);
                }
            }
            s.busy = busy;
        }
        if self.persisted.elapsed() > Duration::from_secs(10) {
            self.persist();
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
        let shift_cmd = Modifiers::COMMAND | Modifiers::SHIFT;
        let alt_cmd = Modifiers::COMMAND | Modifiers::ALT;
        let has = !self.sessions.is_empty();
        if ctx.input_mut(|i| i.consume_key(shift_cmd, Key::F)) && has {
            if let Some(w) = self.cur_ws() {
                w.search.focus = true;
            }
            self.side = Some(Side::Search);
        }
        // ⌘⌥← → でタブを移る
        let left = ctx.input_mut(|i| i.consume_key(alt_cmd, Key::ArrowLeft));
        let right = ctx.input_mut(|i| i.consume_key(alt_cmd, Key::ArrowRight));
        if (left || right) && has {
            if let Some(w) = self.cur_ws() {
                let n = w.docs.len() + 1;
                let cur = w.active.map(|a| a + 1).unwrap_or(0);
                let next = if right { (cur + 1) % n } else { (cur + n - 1) % n };
                w.active = if next == 0 { None } else { Some(next - 1) };
            }
        }
        let take = |k: Key| ctx.input_mut(|i| i.consume_key(cmd, k));
        if take(Key::S) && has {
            self.save_active();
        }
        if take(Key::P) && has {
            self.quick = Some(QuickOpen::new(self.sessions[self.sel].view_roots()));
        }
        if take(Key::L) && has {
            self.ask_active();
        }
        if take(Key::J) {
            self.dock = !self.dock;
        }
        if take(Key::N) {
            self.open_new();
        }
        if take(Key::O) {
            self.open_new();
            if let Some(p) = &mut self.picker {
                p.open_workspace_dialog();
            }
        }
        if take(Key::D) && !self.sessions.is_empty() {
            self.apply(Action::Diff(self.sel));
        }
        if take(Key::E) && !self.sessions.is_empty() {
            self.apply(Action::Files(self.sel));
        }
        if take(Key::G) && !self.sessions.is_empty() {
            self.side = if self.side == Some(Side::Map) { None } else { Some(Side::Map) };
        }
        if take(Key::Comma) {
            self.apply(Action::Settings);
        }
        if take(Key::W) && has {
            // ファイルのタブならそれを閉じる。エージェントのタブならセッションを止める
            let cwd = self.sessions[self.sel].cwd.clone();
            let doc = self.cur_ws().and_then(|w| w.active.map(|a| (a, w.docs[a].dirty())));
            match doc {
                Some((a, true)) => self.confirm = Some(Confirm::CloseDoc(cwd, a)),
                Some((a, false)) => {
                    if let Some(w) = self.cur_ws() {
                        w.close(a);
                    }
                }
                None => {
                    let i = self.sel;
                    self.apply(if self.sessions[i].exit.is_some() { Action::Remove(i) } else { Action::Kill(i) });
                }
            }
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
        // 上の帯（左に信号機ボタンが乗る）
        let top = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover()).0;
        ui::drag_area(ui, top, "side-drag");
        let mut top_ui = ui.new_child(egui::UiBuilder::new().max_rect(top).layout(Layout::right_to_left(Align::Center)));
        if ui::icon_button(&mut top_ui, ui::icon::PLUS, l!("新しいセッション  ⌘N", "New session  ⌘N"), false).clicked() {
            acts.push(Action::New);
        }
        ui.add_space(14.0);

        // 下: 前回のセッションと設定
        egui::TopBottomPanel::bottom("sidebar-foot")
            .frame(Frame::new().inner_margin(Margin { top: 6, ..Default::default() }))
            .show_separator_line(false)
            .show_inside(ui, |ui| {
                if !self.dormant.is_empty() {
                    ui.horizontal(|ui| {
                        ui::section(ui, l!("前回のセッション", "Last time"));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let r = ui.add(egui::Button::new(RichText::new(l!("全部再開", "Resume all")).size(11.0).color(MUTED)).frame(false));
                            if r.clicked() {
                                acts.push(Action::ResumeAll);
                            }
                        });
                    });
                    ui.add_space(2.0);
                    ScrollArea::vertical().id_salt("dormant-list").max_height(220.0).show(ui, |ui| {
                        ui.spacing_mut().interact_size.y = 18.0;
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for (i, d) in self.dormant.iter().enumerate() {
                            let id = Id::new(("dormant", i));
                            let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
                            let inner = Frame::new()
                                .fill(if hovered { CARD_HOVER } else { Color32::TRANSPARENT })
                                .corner_radius(7)
                                .inner_margin(Margin::symmetric(8, 5))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        let g = if d.resumable() { ui::icon::ARROW_COUNTER_CLOCKWISE } else { ui::icon::PLAY };
                                        ui.label(RichText::new(g).size(13.0).color(if hovered { ACCENT } else { FAINT }));
                                        ui.add(Label::new(RichText::new(&d.name).size(13.0).color(MUTED)).truncate());
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            if hovered
                                                && ui.add(egui::Button::new(RichText::new(ui::icon::X).size(12.0).color(FAINT)).frame(false)).on_hover_text(l!("忘れる", "Forget")).clicked()
                                            {
                                                acts.push(Action::Forget(i));
                                            }
                                            ui.label(RichText::new(&d.agent).size(11.0).color(FAINT));
                                        });
                                    });
                                });
                            let how = if d.resumable() { l!("会話の続きから再開", "Resumes the conversation") } else { l!("同じフォルダで開き直す", "Starts fresh in the same folder") };
                            let r = ui.interact(inner.response.rect, id, Sense::click()).on_hover_text(format!("{}\n{how}", tilde(&d.cwd)));
                            if r.clicked() {
                                acts.push(Action::Resume(i));
                            }
                        }
                    });
                    ui.add_space(6.0);
                }
                if let Some(st) = &self.runner {
                    let s = st.lock().unwrap().clone();
                    ui.horizontal(|ui| {
                        let (dot, text) = if s.connected {
                            (OK, if s.running > 0 { lf!("paibot · 動作中 {}", "paibot · running {}", s.running) } else { l!("paibot につながっています", "Connected to paibot").to_string() })
                        } else {
                            (FAINT, l!("paibot につながっていません", "Not connected to paibot").to_string())
                        };
                        ui.label(RichText::new("●").size(9.0).color(dot));
                        let tip = s.last_error.clone().unwrap_or_else(|| s.recent.join("\n"));
                        let r = ui.label(RichText::new(text).size(11.5).color(MUTED));
                        if !tip.is_empty() {
                            r.on_hover_text(tip);
                        }
                    });
                    ui.add_space(2.0);
                }
                ui.horizontal(|ui| {
                    let r = ui.add(egui::Button::new(RichText::new(lf!("{}  設定", "{}  Settings", ui::icon::GEAR_SIX)).size(12.0).color(FAINT)).frame(false));
                    if r.on_hover_text("⌘,").clicked() {
                        acts.push(Action::Settings);
                    }
                });
            });

        ui.horizontal(|ui| {
            ui::section(ui, l!("セッション", "Sessions"));
            let busy = self.sessions.iter().filter(|s| s.busy).count();
            let waiting = self.sessions.iter().filter(|s| s.attention).count();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if waiting > 0 {
                    ui.label(RichText::new(lf!("待ち {waiting}", "{waiting} waiting")).size(11.0).color(WAITING));
                }
                if busy > 0 {
                    ui.label(RichText::new(lf!("動作中 {busy}", "{busy} running")).size(11.0).color(ACCENT));
                }
            });
        });
        ui.add_space(4.0);

        if self.sessions.is_empty() {
            ui.add_space(6.0);
            ui.label(RichText::new(l!("まだありません", "None yet")).size(12.0).color(FAINT));
            return acts;
        }

        let info = self.git_info.lock().unwrap().clone();
        let kids = self.call_tree();
        let viewing_child = if self.side == Some(Side::Child) { self.child_view.as_ref().map(|v| v.id.clone()) } else { None };
        let now = Instant::now();
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, s) in self.sessions.iter().enumerate() {
                let selected = i == self.sel;
                let id = Id::new(("card", i));
                let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
                let fill = if selected { CARD_SEL } else if hovered { CARD_HOVER } else { CARD };
                let inner = Frame::new().fill(fill).corner_radius(CornerRadius::same(8)).inner_margin(Margin { left: 10, right: 10, top: 8, bottom: 8 }).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.spacing_mut().interact_size.y = 16.0;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui::status_dot(ui, s);
                        let col = if s.exit.is_some() { MUTED } else { TEXT };
                        ui.add(Label::new(RichText::new(&s.name).font(ui::bold(13.5)).color(col)).truncate());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if i < 9 && (hovered || selected) {
                                ui.label(RichText::new(format!("⌘{}", i + 1)).size(10.5).color(FAINT));
                            }
                        });
                    });
                    let g = info.get(&s.cwd);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let mut meta = s.agent.clone();
                        if let Some(b) = g.and_then(|g| g.branch.clone()).or_else(|| s.branch.clone()) {
                            if b != s.name {
                                meta.push_str(&format!("  {} {b}", ui::icon::GIT_BRANCH));
                            }
                        }
                        let t = match s.exit {
                            Some(code) => lf!("終了 {code}", "Exited {code}"),
                            None => elapsed(now.duration_since(s.created).as_secs()),
                        };
                        meta.push_str(&format!("  ·  {t}"));
                        ui.add(Label::new(RichText::new(meta).size(11.5).color(FAINT)).truncate());
                    });
                    let pv = s.preview();
                    if !pv.is_empty() {
                        ui.horizontal(|ui| {
                            ui.add_space(20.0);
                            ui.add(Label::new(RichText::new(pv).font(FontId::monospace(10.5)).color(MUTED.gamma_multiply(0.8))).truncate());
                        });
                    }
                    if let Some(g) = g.filter(|g| g.files > 0) {
                        ui.horizontal(|ui| {
                            ui.add_space(20.0);
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(RichText::new(format!("+{}", g.added)).font(FontId::monospace(10.5)).color(OK.gamma_multiply(0.85)));
                            ui.label(RichText::new(format!("−{}", g.removed)).font(FontId::monospace(10.5)).color(BAD.gamma_multiply(0.85)));
                            ui.label(RichText::new(lf!("{} ファイル", "{} files", g.files)).size(10.5).color(FAINT));
                        });
                    }
                });
                let rect = inner.response.rect;
                if selected {
                    let bar = Rect::from_min_size(rect.left_top() + vec2(0.0, 9.0), vec2(2.5, rect.height() - 18.0));
                    ui.painter().rect_filled(bar, 2.0, ACCENT);
                } else if s.attention {
                    let bar = Rect::from_min_size(rect.left_top() + vec2(0.0, 9.0), vec2(2.5, rect.height() - 18.0));
                    ui.painter().rect_filled(bar, 2.0, WAITING);
                }
                let resp = ui.interact(rect, id, Sense::click());
                if resp.clicked() {
                    acts.push(Action::Select(i));
                }
                if resp.double_clicked() {
                    acts.push(Action::Rename(i));
                }
                resp.context_menu(|ui| {
                    let item = |ui: &mut Ui, g: &str, t: &str| ui.button(format!("{g}  {t}")).clicked();
                    if item(ui, ui::icon::PENCIL_SIMPLE, l!("名前を変える", "Rename")) {
                        acts.push(Action::Rename(i));
                        ui.close();
                    }
                    if item(ui, ui::icon::FOLDER_SIMPLE, l!("ファイル", "Files")) {
                        acts.push(Action::Files(i));
                        ui.close();
                    }
                    if item(ui, ui::icon::GIT_DIFF, l!("差分", "Diff")) {
                        acts.push(Action::Diff(i));
                        ui.close();
                    }
                    if item(ui, ui::icon::CODE, &lf!("{} で開く", "Open in {}", self.cfg.editor_app)) {
                        acts.push(Action::Editor(i));
                        ui.close();
                    }
                    if item(ui, ui::icon::FOLDER_OPEN, l!("Finder で開く", "Show in Finder")) {
                        acts.push(Action::Finder(i));
                        ui.close();
                    }
                    ui.separator();
                    let label = if s.exit.is_some() { l!("一覧から外す", "Remove from list") } else { l!("止める…", "Stop…") };
                    if ui.button(RichText::new(format!("{}  {label}", ui::icon::STOP)).color(BAD)).clicked() {
                        acts.push(if s.exit.is_some() { Action::Remove(i) } else { Action::Kill(i) });
                        ui.close();
                    }
                });
                // 呼んだエージェントを下にぶら下げる（さらに呼んだものは字下げ。新しいものから8つ）
                let list = children::flatten(&kids[i]);
                let skip = list.len().saturating_sub(8);
                if skip > 0 {
                    ui.horizontal(|ui| {
                        ui.add_space(30.0);
                        ui.label(RichText::new(lf!("ほか {skip} 件", "{skip} more")).size(10.5).color(FAINT));
                    });
                }
                for (depth, node) in list.iter().skip(skip) {
                    let c = &node.child;
                    let id = Id::new(("child", &c.id));
                    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
                    let on = viewing_child.as_deref() == Some(c.id.as_str());
                    let inner = Frame::new()
                        .fill(if on { CARD_SEL } else if hovered { CARD_HOVER } else { Color32::TRANSPARENT })
                        .corner_radius(6)
                        .inner_margin(Margin { left: 22 + (*depth as i8).min(6) * 12, right: 8, top: 3, bottom: 3 })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().interact_size.y = 14.0;
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                let arrow = if node.guessed { ui::icon::DOTS_THREE_OUTLINE_VERTICAL } else { ui::icon::ARROW_ELBOW_DOWN_RIGHT };
                                ui.label(RichText::new(arrow).size(11.0).color(FAINT));
                                let (g, col) = match c.status {
                                    children::Status::Running => (ui::icon::CIRCLE_NOTCH, ACCENT),
                                    children::Status::Done => (ui::icon::CHECK, OK),
                                    children::Status::Stopped => (ui::icon::WARNING, WAITING),
                                };
                                ui.label(RichText::new(g).size(11.0).color(col));
                                ui.label(RichText::new(&c.label).font(ui::bold(11.0)).color(MUTED));
                                ui.add(Label::new(RichText::new(c.prompt.replace('\n', " ")).size(11.0).color(FAINT)).truncate());
                            });
                        });
                    let how = if node.guessed { l!("\n\n（呼び出し元は推測：同じフォルダで、上の段が動いている間に始まった）", "\n\n(Caller is a guess: it started in the same folder while the row above was running)") } else { "" };
                    let r = ui.interact(inner.response.rect, id, Sense::click()).on_hover_text(format!("{}{how}", clip_text(&c.prompt, 300)));
                    if r.clicked() {
                        acts.push(Action::Child(c.id.clone()));
                    }
                    if c.status == children::Status::Running {
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                    }
                }
                ui.add_space(2.0);
            }
        });
        acts
    }

    /// 上の帯: タブ（エージェント＋ファイル）と、右側の操作
    fn top_bar(&mut self, ui: &mut Ui) -> (Vec<Action>, Option<TabAct>) {
        let mut acts = Vec::new();
        let mut tab_act = None;
        let i = self.sel;
        let cwd = self.sessions[i].cwd.clone();
        let w = self.ws.entry(cwd.clone()).or_insert_with(|| Workspace::for_session(&self.sessions[i]));
        let s = &self.sessions[i];
        let branch = self.git_info.lock().unwrap().get(&s.cwd).and_then(|g| g.branch.clone()).or_else(|| s.branch.clone());
        let bar = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 38.0));
        ui.painter().rect_filled(bar, 0.0, TOPBAR_BG);
        ui.painter().hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, HAIRLINE));
        ui::drag_area(ui, bar, "top-drag");
        let mut bar_ui = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(8.0, 0.0))).layout(Layout::right_to_left(Align::Center)));
        ui.advance_cursor_after_rect(bar);
        let ui = &mut bar_ui;

        // 右側の操作
        if s.exit.is_some() {
            if ui::icon_button(ui, ui::icon::X, l!("一覧から外す", "Remove from list"), false).clicked() {
                acts.push(Action::Remove(i));
            }
        } else if ui::icon_button_colored(ui, ui::icon::STOP, l!("止める  ⌘W", "Stop  ⌘W"), false, Some(BAD)).clicked() {
            acts.push(Action::Kill(i));
        }
        if ui::icon_button(ui, ui::icon::CODE, &lf!("{} で開く", "Open in {}", self.cfg.editor_app), false).clicked() {
            acts.push(Action::Editor(i));
        }
        ui.add_space(4.0);
        ui.painter().vline(ui.cursor().right() - 2.0, bar.y_range().shrink(11.0), Stroke::new(1.0, BORDER));
        ui.add_space(6.0);
        if ui::icon_button(ui, ui::icon::TREE_STRUCTURE, l!("エージェントマップ  ⌘G", "Agent map  ⌘G"), matches!(self.side, Some(Side::Map) | Some(Side::Child))).clicked() {
            self.side = if self.side == Some(Side::Map) { None } else { Some(Side::Map) };
        }
        if ui::icon_button(ui, ui::icon::GIT_DIFF, l!("差分  ⌘D", "Diff  ⌘D"), self.side == Some(Side::Diff)).clicked() {
            acts.push(Action::Diff(i));
        }
        if ui::icon_button(ui, ui::icon::MAGNIFYING_GLASS, l!("検索  ⌘⇧F", "Search  ⌘⇧F"), self.side == Some(Side::Search)).clicked() {
            acts.push(Action::Search(i));
        }
        if ui::icon_button(ui, ui::icon::FOLDER_SIMPLE, l!("ファイル  ⌘E", "Files  ⌘E"), self.side == Some(Side::Files)).clicked() {
            acts.push(Action::Files(i));
        }
        ui.add_space(8.0);

        // 左側のタブ
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ScrollArea::horizontal().id_salt("tabs").scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let (st, sc) = ui::status_text(s);
                    let mut label = format!("{}  {}", ui::icon::TERMINAL_WINDOW, s.name);
                    if let Some(b) = &branch {
                        if *b != s.name {
                            label.push_str(&format!("   {} {b}", ui::icon::GIT_BRANCH));
                        }
                    }
                    let r = tab(ui, w.active.is_none(), &label, if s.attention { Some(WAITING) } else { None }, false)
                        .on_hover_text(format!("{} · {}\n{}\n{}", s.agent, st, tilde(&s.cwd), if s.attention { l!("こちらを待っています", "Waiting for you") } else { "" }));
                    let _ = sc;
                    if r.clicked() {
                        tab_act = Some(TabAct::Select(None));
                    }
                    for (di, d) in w.docs.iter().enumerate() {
                        let on = w.active == Some(di);
                        let r = tab(ui, on, &d.title(), None, d.dirty()).on_hover_text(d.path.to_string_lossy());
                        if r.clicked() {
                            tab_act = Some(TabAct::Select(Some(di)));
                        }
                        if r.middle_clicked() {
                            tab_act = Some(TabAct::Close(di));
                        }
                        if (on || r.hovered()) && ui.add(egui::Button::new(RichText::new(ui::icon::X).size(11.0).color(FAINT)).frame(false)).on_hover_text(l!("閉じる  ⌘W", "Close  ⌘W")).clicked() {
                            tab_act = Some(TabAct::Close(di));
                        }
                    }
                });
            });
        });
        (acts, tab_act)
    }

    /// マップに並べるもの
    fn map_lanes(&self) -> Vec<crate::map::Lane> {
        use crate::map::{self, Kid, Lane, State};
        fn to_kid(n: children::Node) -> Kid {
            let c = n.child;
            Kid {
                state: match c.status {
                    children::Status::Running => State::Running,
                    children::Status::Done => State::Done,
                    children::Status::Stopped => State::Stopped,
                },
                end: (c.status != children::Status::Running).then_some(c.updated),
                color: match c.kind {
                    children::Kind::Codex => map::CODEX,
                    children::Kind::Claude => map::CLAUDE,
                    children::Kind::ClaudeSub => map::SUB,
                    children::Kind::Agy => map::AGY,
                },
                id: c.id,
                label: c.label,
                prompt: c.prompt,
                last: c.last,
                start: c.started,
                tools: c.tools,
                guessed: n.guessed,
                kids: n.kids.into_iter().map(to_kid).collect(),
            }
        }
        let kids = self.call_tree();
        self.sessions
            .iter()
            .zip(kids)
            .map(|(s, ks)| {
                let state = match s.exit {
                    Some(0) => State::Done,
                    Some(_) => State::Stopped,
                    None if s.attention => State::Waiting,
                    None if s.busy => State::Running,
                    None => State::Quiet,
                };
                let color = match resume::kind(&s.base_cmd) {
                    resume::Kind::Claude => map::CLAUDE,
                    resume::Kind::Codex => map::CODEX,
                    resume::Kind::Other => map::OTHER,
                };
                let start = s.started_at;
                Lane {
                    name: s.name.clone(),
                    agent: s.agent.clone(),
                    color,
                    start,
                    state,
                    kids: ks
                        .into_iter()
                        .map(to_kid)
                        .collect(),
                }
            })
            .collect()
    }

    fn child_panel(&mut self, ui: &mut Ui) {
        let Some(v) = &mut self.child_view else {
            self.side = None;
            return;
        };
        v.refresh();
        // 終わったかどうかは、見張り役が持っている最新の状態に合わせる（子エージェントは meta.json の finished）
        if self.children.lock().unwrap().iter().any(|c| c.id == v.id && c.status == children::Status::Done) {
            v.done = true;
        }
        let running = !v.done && v.mtime.is_some_and(|m| m.elapsed().unwrap_or_default() < std::time::Duration::from_secs(90));
        let mut close = false;
        let mut to_map = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(ui::icon::ROBOT).size(15.0).color(ACCENT));
            ui.label(RichText::new(&v.label).font(ui::bold(13.5)).color(TEXT));
            let how = match v.kind {
                children::Kind::Codex => l!("コマンドで呼ばれた codex", "codex called by a command"),
                children::Kind::Claude => l!("コマンドで呼ばれた claude", "claude called by a command"),
                children::Kind::ClaudeSub => l!("claude の子エージェント", "claude subagent"),
                children::Kind::Agy => l!("agy（Antigravity）", "agy (Antigravity)"),
            };
            ui.label(RichText::new(how).size(11.0).color(FAINT));
            let (t, c) = if v.done { (l!("終わった", "Done"), OK) } else if running { (l!("動いています", "Running"), ACCENT) } else { (l!("途中で止まった", "Stopped midway"), WAITING) };
            ui.label(RichText::new(t).size(11.0).color(c));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui::icon_button(ui, ui::icon::X, l!("閉じる", "Close"), false).clicked() {
                    close = true;
                }
                if ui::icon_button(ui, ui::icon::TREE_STRUCTURE, l!("エージェントマップへ", "Back to agent map"), false).clicked() {
                    to_map = true;
                }
                if ui::icon_button(ui, ui::icon::FILE_TEXT, l!("記録のファイルを開く", "Reveal the log file"), false).clicked() {
                    let _ = std::process::Command::new("open").arg("-R").arg(&v.file).status();
                }
            });
        });
        ui.add_space(6.0);
        if running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(700));
        }
        let mono = FontId::monospace(11.5);
        ScrollArea::vertical().id_salt("child-steps").auto_shrink([false, false]).stick_to_bottom(running).show(ui, |ui| {
            for (n, st) in v.steps.iter().enumerate() {
                match st.kind {
                    children::StepKind::Prompt => {
                        ui.label(RichText::new(l!("頼まれたこと", "Asked to")).size(11.0).color(FAINT));
                        Frame::new().fill(RAISED).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(Label::new(RichText::new(&st.text).size(12.5).color(TEXT)).wrap());
                        });
                        ui.add_space(8.0);
                    }
                    children::StepKind::Say => {
                        ui.add(Label::new(RichText::new(&st.text).size(12.5).color(TERM_FG)).wrap());
                        ui.add_space(6.0);
                    }
                    children::StepKind::Tool => {
                        let (name, rest) = st.text.split_once("  ").unwrap_or((st.text.as_str(), ""));
                        egui::CollapsingHeader::new(RichText::new(format!("{}  {name}  {}", ui::icon::TERMINAL, clip_text(rest.lines().next().unwrap_or(""), 60))).size(11.5).color(MUTED))
                            .id_salt(("tool", n))
                            .show(ui, |ui| {
                                ui.add(Label::new(RichText::new(rest).font(mono.clone()).color(TERM_FG)).wrap());
                            });
                    }
                    children::StepKind::Output => {
                        egui::CollapsingHeader::new(RichText::new(lf!("{}  結果  {}", "{}  Result  {}", ui::icon::ARROW_BEND_DOWN_RIGHT, clip_text(st.text.lines().next().unwrap_or(""), 60))).size(11.0).color(FAINT))
                            .id_salt(("out", n))
                            .show(ui, |ui| {
                                ui.add(Label::new(RichText::new(&st.text).font(mono.clone()).color(MUTED)).wrap());
                            });
                        ui.add_space(4.0);
                    }
                    children::StepKind::Answer => {
                        ui.add_space(4.0);
                        ui.label(RichText::new(l!("答え", "Answer")).size(11.0).color(FAINT));
                        Frame::new().fill(OK.gamma_multiply(0.08)).stroke(Stroke::new(1.0, OK.gamma_multiply(0.35))).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(Label::new(RichText::new(&st.text).size(12.5).color(TEXT)).wrap());
                        });
                    }
                }
            }
            if v.steps.is_empty() {
                ui.label(RichText::new(l!("まだ記録がありません", "Nothing recorded yet")).color(FAINT));
            }
        });
        if close {
            self.side = None;
        } else if to_map {
            self.side = Some(Side::Map);
        }
    }

    fn diff_panel(&mut self, ui: &mut Ui) {
        let Some(d) = &mut self.diff else { return };
        let mut ask = None;
        let mut close = false;
        let mut reload = false;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(l!("差分", "Diff")).size(15.0).strong().color(TEXT));
            ui.label(RichText::new(l!("コミットしていない変更", "Uncommitted changes")).size(12.0).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button(l!("閉じる", "Close")).on_hover_text("⌘D").clicked() {
                    close = true;
                }
                if ui.button(l!("読み直す", "Reload")).clicked() {
                    reload = true;
                }
            });
        });
        ui.add_space(6.0);
        if let Some(e) = &d.diff.error {
            ui.label(RichText::new(lf!("git の状態を取れませんでした: {e}", "Could not read git status: {e}")).color(MUTED));
        } else if d.diff.status.is_empty() {
            ui.label(RichText::new(l!("変更はありません", "No changes")).color(MUTED));
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
                ui.label(RichText::new(lf!("ほか {} 件", "{} more", d.diff.status.len() - 30)).size(12.0).color(MUTED));
            }
            ui.add_space(6.0);
            ui.separator();
            if let Some(h) = files::diff_view(ui, &d.diff.lines, "diff-all") {
                ask = Some(Ask { rel: h.file, lines: Some(h.range), comment: String::new(), first: true });
            }
        }
        if ask.is_some() {
            self.ask = ask;
        }
        if close {
            self.side = None;
        } else if reload {
            d.diff = git::diff_multi(&d.roots);
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(mut p) = self.picker.take() {
            let shown = p.show(ctx, &self.cfg);
            // 開いたワークスペースは設定に覚えて、次から一覧に出す
            if let Some(f) = p.opened_workspace.take() {
                let t = tilde(&f);
                if !self.cfg.workspaces.contains(&t) {
                    self.cfg.workspaces.insert(0, t);
                    if config::exists() {
                        let _ = config::save(&self.cfg);
                    }
                }
            }
            match shown {
                picker::Outcome::Keep => self.picker = Some(p),
                picker::Outcome::Cancel => {}
                picker::Outcome::Start { dir, agent, branch, roots, name } => {
                    if let Err(e) = self.create(&dir, agent, &branch, roots, name) {
                        self.flash(format!("{e:#}"));
                        self.picker = Some(p);
                    }
                }
            }
        }

        if let Some(mut st) = self.setup.take() {
            match st.show(ctx) {
                setup::Outcome::Keep => self.setup = Some(st),
                setup::Outcome::Cancel => crate::i18n::apply(&self.cfg.language),
                setup::Outcome::Save(c) => match config::save(&c) {
                    Ok(()) => {
                        crate::i18n::apply(&c.language);
                        self.term.font_size = c.font_size;
                        self.cfg = c;
                        self.flash(l!("設定を保存しました", "Settings saved"));
                    }
                    Err(e) => {
                        self.flash(lf!("設定を保存できませんでした: {e:#}", "Could not save settings: {e:#}"));
                        self.setup = Some(st);
                    }
                },
            }
        }

        if let Some(mut q) = self.quick.take() {
            match q.show(ctx) {
                quickopen::Outcome::Keep => self.quick = Some(q),
                quickopen::Outcome::Cancel => {}
                quickopen::Outcome::Open(p) => self.apply(Action::Open(p, None)),
            }
        }

        if let Some(mut a) = self.ask.take() {
            let mut keep = true;
            let mut send: Option<bool> = None;
            let modal = egui::Modal::new(Id::new("ask")).show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(RichText::new(l!("エージェントに聞く", "Ask the agent")).size(16.0).strong());
                ui.add_space(4.0);
                ui.label(RichText::new(a.text()).font(FontId::monospace(12.0)).color(MUTED));
                ui.add_space(8.0);
                let r = ui.add(
                    TextEdit::multiline(&mut a.comment)
                        .hint_text(l!("聞きたいこと・直してほしいこと", "What to ask or fix"))
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                if a.first {
                    r.request_focus();
                    a.first = false;
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new(l!("送る", "Send")).strong()).fill(ACCENT.gamma_multiply(0.35))).on_hover_text("⌘Enter").clicked() {
                        send = Some(true);
                    }
                    if ui.button(l!("入力欄に入れるだけ", "Just put it in the input")).on_hover_text("Enter").clicked() {
                        send = Some(false);
                    }
                    if ui.button(l!("やめる", "Cancel")).clicked() {
                        keep = false;
                    }
                });
            });
            // Enter は入力欄に入れるだけ、⌘Enter で送る（Shift+Enter は改行）
            let (cmd_enter, enter) = ctx.input_mut(|i| {
                (i.consume_key(Modifiers::COMMAND, Key::Enter), i.consume_key(Modifiers::NONE, Key::Enter))
            });
            if cmd_enter {
                send = Some(true);
            } else if enter {
                send = Some(false);
            }
            if modal.should_close() {
                keep = false;
            }
            if let Some(submit) = send {
                let text = a.text();
                if let Some(s) = self.sessions.get_mut(self.sel) {
                    s.paste(&text);
                    if submit {
                        s.write(b"\r");
                    }
                }
                let dock = self.dock;
                if let Some(w) = self.cur_ws() {
                    if !dock {
                        w.active = None;
                    }
                }
                self.grab_term = true;
                keep = false;
            }
            if keep {
                self.ask = Some(a);
            }
        }

        if let Some((i, mut name)) = self.rename.take() {
            let mut keep = true;
            let modal = egui::Modal::new(Id::new("rename")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(RichText::new(l!("名前を変える", "Rename")).size(16.0).strong());
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
                    l!("止める", "Stop").to_string(),
                    lf!("「{}」を止めます。worktree やファイルはそのまま残ります。", "Stop \"{}\"? The worktree and files are kept.", self.sessions.get(*i).map(|s| s.name.as_str()).unwrap_or("")),
                ),
                Confirm::Quit => {
                    let dirty = self.dirty_docs();
                    let extra = if dirty > 0 { lf!("保存していないファイルが {dirty} 個あります（保存されません）。", "{dirty} unsaved files will not be saved. ") } else { String::new() };
                    (l!("終わる", "Quit").to_string(), lf!("動いているセッションが {} 個あります。{extra}全部止めて終わりますか？\n\n次に開いたとき、サイドバーの「前回のセッション」から再開できます（claude と codex は会話の続きから）。", "{} sessions are running. {extra}Stop them all and quit?\n\nNext time, you can resume them from \"Last time\" in the sidebar (claude and codex continue the conversation).", self.alive()))
                }
                Confirm::CloseDoc(cwd, i) => (
                    l!("閉じる", "Close").to_string(),
                    lf!(
                        "「{}」に保存していない変更があります。捨てて閉じますか？", "\"{}\" has unsaved changes. Discard and close?",
                        self.ws.get(cwd).and_then(|w| w.docs.get(*i)).map(|d| d.title()).unwrap_or_default()
                    ),
                ),
            };
            let modal = egui::Modal::new(Id::new("confirm")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(title).size(16.0).strong());
                ui.add_space(8.0);
                ui.label(body);
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new(l!("はい", "Yes")).color(BAD).strong())).clicked() {
                        yes = true;
                    }
                    if ui.button(l!("やめる", "Cancel")).clicked() {
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
                    Confirm::CloseDoc(cwd, i) => {
                        if let Some(w) = self.ws.get_mut(&cwd) {
                            w.close(i);
                        }
                    }
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

impl eframe::App for PaibotAgentsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(120));
        self.window_focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if let Some(shot) = &mut self.shot {
            if shot.update(ctx) {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            self.persist();
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && (self.alive() > 0 || self.dirty_docs() > 0) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Quit);
        }

        self.shortcuts(ctx);
        self.tick();
        // 確認用: 呼ばれた codex が見つかったら、その中身を開く
        if self.shot.as_ref().is_some_and(|s| s.ui == "child") && self.side.is_none() {
            let first = self.children.lock().unwrap().first().map(|c| c.id.clone());
            if let Some(id) = first {
                self.apply(Action::Child(id));
            }
        }

        let mut acts = Vec::new();
        egui::SidePanel::left("sessions")
            .resizable(true)
            .default_width(272.0)
            .width_range(210.0..=480.0)
            .show_separator_line(false)
            .frame(Frame::new().fill(SIDEBAR_BG).inner_margin(Margin { left: 10, right: 10, top: 4, bottom: 10 }).stroke(Stroke::new(1.0, HAIRLINE)))
            .show(ctx, |ui| acts.extend(self.sidebar(ui)));

        if let Some(side) = self.side.filter(|_| !self.sessions.is_empty()) {
            egui::SidePanel::right("side")
                .resizable(true)
                .default_width(600.0)
                .width_range(280.0..=1400.0)
                .show_separator_line(false)
                .frame(Frame::new().fill(SIDEBAR_BG).inner_margin(Margin { left: 12, right: 12, top: 6, bottom: 10 }).stroke(Stroke::new(1.0, HAIRLINE)))
                .show(ctx, |ui| match side {
                    Side::Diff => self.diff_panel(ui),
                    Side::Child => self.child_panel(ui),
                    Side::Map => {
                        let lanes = self.map_lanes();
                        match crate::map::show(ui, &lanes, &mut self.map_window, &mut self.map_view) {
                            Some(crate::map::Click::Kid(id)) => acts.push(Action::Child(id)),
                            Some(crate::map::Click::Lane(i)) => acts.push(Action::Select(i)),
                            None => {}
                        }
                    }
                    Side::Files | Side::Search => {
                        let cwd = self.sessions[self.sel].cwd.clone();
                        let mut close = false;
                        ui.horizontal(|ui| {
                            let title = if side == Side::Files { l!("ファイル", "Files") } else { l!("検索", "Search") };
                            ui.label(RichText::new(title).font(ui::bold(13.5)).color(TEXT));
                            if side == Side::Files {
                                ui.label(RichText::new(l!("⌘P で名前から開く", "⌘P to open by name")).size(11.0).color(FAINT));
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui::icon_button(ui, ui::icon::X, l!("閉じる", "Close"), false).clicked() {
                                    close = true;
                                }
                            });
                        });
                        ui.add_space(6.0);
                        let sel = self.sel;
                        let roots = self.sessions[sel].view_roots();
                        let w = self.ws.entry(cwd.clone()).or_insert_with(|| Workspace::for_session(&self.sessions[sel]));
                        if side == Side::Files {
                            let active = w.active.map(|i| w.docs[i].path.clone());
                            for a in w.tree.show(ui, active.as_deref()) {
                                match a {
                                    FileAction::Open(p) => w.open(p, None),
                                }
                            }
                        } else if let Some((p, line)) = w.search.show(ui, &roots) {
                            w.open(p, Some(line));
                        }
                        if close {
                            self.side = None;
                        }
                    }
                });
        }

        egui::CentralPanel::default().frame(Frame::new().fill(TERM_BG)).show(ctx, |ui| {
            if self.sessions.is_empty() {
                let full = ui.max_rect();
                ui::drag_area(ui, Rect::from_min_size(full.min, vec2(full.width(), 38.0)), "empty-drag");
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.28).max(40.0));
                    ui.label(RichText::new("Paibot Agents").font(ui::bold(30.0)).color(TEXT));
                    ui.add_space(2.0);
                    ui.label(RichText::new(l!("エージェントと一緒に書く", "Write code with your agents")).size(13.0).color(FAINT));
                    ui.add_space(26.0);
                    let row = |ui: &mut Ui, g: &str, t: &str, k: &str| -> bool {
                        let (rect, r) = ui.allocate_exact_size(vec2(340.0, 38.0), Sense::click());
                        let p = ui.painter();
                        if r.hovered() {
                            p.rect_filled(rect, 8.0, CARD_SEL);
                        }
                        p.text(rect.left_center() + vec2(14.0, 0.0), egui::Align2::LEFT_CENTER, g, FontId::proportional(16.0), ACCENT);
                        p.text(rect.left_center() + vec2(42.0, 0.0), egui::Align2::LEFT_CENTER, t, FontId::proportional(13.5), TEXT);
                        p.text(rect.right_center() - vec2(14.0, 0.0), egui::Align2::RIGHT_CENTER, k, FontId::proportional(12.0), FAINT);
                        r.clicked()
                    };
                    if row(ui, ui::icon::PLUS, l!("新しいセッション", "New session"), "⌘N") {
                        acts.push(Action::New);
                    }
                    if !self.dormant.is_empty() {
                        let n = self.dormant.len();
                        if row(ui, ui::icon::ARROW_COUNTER_CLOCKWISE, &lf!("前回の {n} セッションを再開", "Resume {n} sessions from last time"), "") {
                            acts.push(Action::ResumeAll);
                        }
                    }
                    if row(ui, ui::icon::GEAR_SIX, l!("設定", "Settings"), "⌘,") {
                        acts.push(Action::Settings);
                    }
                    ui.add_space(28.0);
                    ui.label(RichText::new(l!("⌘P ファイルを開く   ·   ⌘⇧F 検索   ·   ⌘L 選んだ行をエージェントに聞く", "⌘P open a file   ·   ⌘⇧F search   ·   ⌘L ask the agent about the selection")).size(11.5).color(FAINT));
                });
                return;
            }
            let (top_acts, tab_act) = self.top_bar(ui);
            acts.extend(top_acts);
            let accept = !self.dialog_open();
            let sel = self.sel;
            let cwd = self.sessions[sel].cwd.clone();
            let w = self.ws.entry(cwd.clone()).or_insert_with(|| Workspace::for_session(&self.sessions[sel]));
            if let Some(t) = tab_act {
                match t {
                    TabAct::Select(a) => {
                        w.active = a;
                        if a.is_none() {
                            self.grab_term = true;
                        }
                    }
                    TabAct::Close(i) if w.docs[i].dirty() => self.confirm = Some(Confirm::CloseDoc(cwd.clone(), i)),
                    TabAct::Close(i) => w.close(i),
                }
            }
            let grab = std::mem::take(&mut self.grab_term);
            let mut resized = None;
            let mut doc_acts = Vec::new();
            if let Some(di) = w.active {
                if self.dock {
                    let max_h = (ui.available_height() - 140.0).max(120.0);
                    egui::TopBottomPanel::bottom("agent-dock")
                        .resizable(true)
                        .default_height(260.0)
                        .height_range(100.0..=max_h)
                        .frame(Frame::new().fill(TERM_BG))
                        .show_inside(ui, |ui| {
                            ui::hairline(ui);
                            ui.horizontal(|ui| {
                                ui.add_space(12.0);
                                ui::status_dot(ui, &self.sessions[sel]);
                                ui.label(RichText::new(format!("{}  ·  {}", self.sessions[sel].name, self.sessions[sel].agent)).size(11.5).color(MUTED));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.add_space(6.0);
                                    if ui::icon_button(ui, ui::icon::CARET_DOWN, l!("隠す  ⌘J", "Hide  ⌘J"), false).clicked() {
                                        self.dock = false;
                                    }
                                });
                            });
                            resized = term::show(ui, &mut self.sessions[sel], &mut self.term, accept, false, grab);
                        });
                }
                egui::CentralPanel::default().frame(Frame::new().fill(TERM_BG)).show_inside(ui, |ui| {
                    doc_acts = editor::show(ui, &mut w.docs[di], &cwd, self.term.font_size);
                });
                for a in doc_acts {
                    match a {
                        DocAction::Save => {
                            if let Err(e) = w.docs[di].save() {
                                self.flash = Some((lf!("保存できませんでした: {e}", "Could not save: {e}"), Instant::now()));
                            }
                        }
                        DocAction::Ask => {
                            let d = &w.docs[di];
                            let rel = crate::util::rel_for_agent(&cwd, &d.path);
                            self.ask = Some(Ask { rel, lines: d.selection, comment: String::new(), first: true });
                        }
                        DocAction::OpenInEditor => {
                            if !open_in(&self.cfg.editor_app, &w.docs[di].path) {
                                self.flash = Some((lf!("{} で開けませんでした", "Could not open with {}", self.cfg.editor_app), Instant::now()));
                            }
                        }
                    }
                }
            } else {
                resized = term::show(ui, &mut self.sessions[sel], &mut self.term, accept, true, grab);
            }
            if let Some((rows, cols)) = resized {
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

/// タブ1つ。選ばれているものは中身と同じ色で、つながって見えるようにする
fn tab(ui: &mut Ui, on: bool, text: &str, tint: Option<Color32>, dirty: bool) -> egui::Response {
    let color = tint.unwrap_or(if on { TEXT } else { MUTED });
    let mut t = RichText::new(text).size(12.5).color(color);
    if dirty {
        t = RichText::new(format!("{text}  ●")).size(12.5).color(color);
    }
    let b = egui::Button::new(t)
        .fill(if on { TERM_BG } else { Color32::TRANSPARENT })
        .stroke(Stroke::NONE)
        .corner_radius(6.0)
        .min_size(vec2(0.0, 28.0));
    ui.add(b)
}

fn clip_text(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if t.chars().count() < s.chars().count() { format!("{t}…") } else { t }
}

fn elapsed(secs: u64) -> String {
    match secs {
        0..60 => lf!("{secs}秒", "{secs}s"),
        60..3600 => lf!("{}分", "{}m", secs / 60),
        _ => lf!("{}時間{}分", "{}h {}m", secs / 3600, secs / 60 % 60),
    }
}
