use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;

use crate::config::Config;
use crate::keys::{self, KeySpec};
use crate::session::Session;
use crate::{git, notify};

pub struct NewForm {
    pub agent: usize,
    pub dir: String,
    pub branch: String,
    /// 0=エージェント 1=フォルダ 2=ブランチ
    pub field: usize,
}

pub struct DiffView {
    pub lines: Vec<Line<'static>>,
    pub scroll: usize,
}

pub enum Mode {
    /// 一覧を操作している
    Nav,
    /// 選んだセッションにキーを送っている
    Term,
    New(NewForm),
    Rename(String),
    ConfirmKill,
    ConfirmQuit,
    Diff(DiffView),
    Help,
}

pub struct App {
    pub cfg: Config,
    pub prefix: KeySpec,
    pub sessions: Vec<Session>,
    pub sel: usize,
    pub mode: Mode,
    pub quit: bool,
    pub dirty: Arc<AtomicBool>,
    pub flash: Option<(String, Instant)>,
    /// (行, 桁) 右側の端末の大きさ
    pub pane: (u16, u16),
    pub focused: bool,
    last_dir: String,
    last_git: Instant,
}

impl App {
    pub fn new(cfg: Config, dir: PathBuf, (w, h): (u16, u16)) -> Self {
        let mut flash = None;
        let prefix = KeySpec::parse(&cfg.prefix).unwrap_or_else(|| {
            flash = Some((format!("prefix \"{}\" は使えないので Ctrl-] にします", cfg.prefix), Instant::now()));
            KeySpec::parse("ctrl-]").unwrap()
        });
        let mut app = Self {
            prefix,
            sessions: Vec::new(),
            sel: 0,
            mode: Mode::Nav,
            quit: false,
            dirty: Arc::new(AtomicBool::new(true)),
            flash,
            pane: (1, 1),
            focused: true,
            last_dir: tilde(&dir),
            last_git: Instant::now(),
            cfg,
        };
        app.pane = app.pane_size(w, h);
        app
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    pub fn current(&self) -> Option<&Session> {
        self.sessions.get(self.sel)
    }

    fn current_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.sel)
    }

    fn pane_size(&self, w: u16, h: u16) -> (u16, u16) {
        // 下に状態行、右側の上に見出し行
        (h.saturating_sub(2).max(1), w.saturating_sub(self.cfg.sidebar_width).max(1))
    }

    fn resize(&mut self, w: u16, h: u16) {
        self.pane = self.pane_size(w, h);
        let (r, c) = self.pane;
        for s in &mut self.sessions {
            s.resize(r, c);
        }
    }

    fn select(&mut self, i: usize) {
        if self.sessions.is_empty() {
            return;
        }
        self.sel = i.min(self.sessions.len() - 1);
        self.sessions[self.sel].attention = false;
    }

    pub fn alive(&self) -> usize {
        self.sessions.iter().filter(|s| s.exit.is_none()).count()
    }

    /// 定期処理: 終了の検出、実行中/待ちの判定、ブランチ名の更新
    pub fn tick(&mut self) {
        let now = Instant::now();
        let idle = Duration::from_secs_f64(self.cfg.idle_secs.max(0.2));
        let min_busy = Duration::from_secs_f64(self.cfg.min_busy_secs.max(0.0));
        let refresh_git = now.duration_since(self.last_git) > Duration::from_secs(3);
        let watching_term = matches!(self.mode, Mode::Term);
        let mut changed = false;

        for (i, s) in self.sessions.iter_mut().enumerate() {
            // 入力中のセッションを、ウィンドウを前にして見ている
            let watching = i == self.sel && watching_term;
            let in_view = watching && self.focused;
            if s.poll_exit() {
                changed = true;
                if !watching {
                    s.attention = true;
                }
                if self.cfg.notify && !in_view {
                    let code = s.exit.unwrap_or(0);
                    notify::send(&format!("{} が終了しました（{}）", s.name, code), &s.agent);
                }
                continue;
            }
            if s.exit.is_some() {
                continue;
            }
            let last = s.last_output();
            let busy = now.duration_since(last) < idle && s.work_after_input() > Duration::from_millis(300);
            if s.busy && !busy && s.work_after_input() >= min_busy {
                // 動いていたのが止まった＝こちらの番
                if !watching {
                    s.attention = true;
                }
                if self.cfg.notify && !in_view {
                    notify::send(&format!("{} が待っています", s.name), &s.agent);
                }
            }
            if s.busy != busy {
                s.busy = busy;
                changed = true;
            }
            if refresh_git {
                let b = git::current_branch(&s.cwd);
                if b != s.branch {
                    s.branch = b;
                    changed = true;
                }
            }
        }
        if refresh_git {
            self.last_git = now;
        }
        if let Some((_, t)) = &self.flash {
            if t.elapsed() > Duration::from_secs(5) {
                self.flash = None;
                changed = true;
            }
        }
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Resize(w, h) => self.resize(w, h),
            Event::FocusGained => self.focused = true,
            Event::FocusLost => self.focused = false,
            Event::Paste(text) => self.paste(&text),
            Event::Key(k) if k.kind != KeyEventKind::Release => self.key(k),
            _ => {}
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn paste(&mut self, text: &str) {
        let one_line = || text.replace(['\r', '\n'], "");
        match &mut self.mode {
            Mode::Term => {
                if let Some(s) = self.sessions.get_mut(self.sel) {
                    s.paste(text);
                }
            }
            Mode::New(f) => match f.field {
                1 => f.dir.push_str(&one_line()),
                2 => f.branch.push_str(&one_line()),
                _ => {}
            },
            Mode::Rename(s) => s.push_str(&one_line()),
            _ => {}
        }
    }

    fn key(&mut self, k: KeyEvent) {
        let mode = std::mem::replace(&mut self.mode, Mode::Nav);
        self.mode = match mode {
            Mode::Term => self.key_term(k),
            Mode::Nav => self.key_nav(k),
            Mode::New(f) => self.key_new(f, k),
            Mode::Rename(s) => self.key_rename(s, k),
            Mode::ConfirmKill => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.remove_current();
                }
                Mode::Nav
            }
            Mode::ConfirmQuit => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.quit = true;
                }
                Mode::Nav
            }
            Mode::Diff(d) => self.key_diff(d, k),
            Mode::Help => Mode::Nav,
        };
    }

    fn key_term(&mut self, k: KeyEvent) -> Mode {
        if self.prefix.matches(&k) {
            return Mode::Nav;
        }
        let Some(s) = self.current_mut() else { return Mode::Nav };
        if s.exit.is_some() {
            return Mode::Nav;
        }
        let app_cursor = s.parser.lock().unwrap().screen().application_cursor();
        if let Some(b) = keys::encode(&k, app_cursor) {
            s.write(&b);
        }
        Mode::Term
    }

    fn enter_term(&mut self) -> Mode {
        let alive = self.current().is_some_and(|s| s.exit.is_none());
        self.select(self.sel);
        if alive {
            if let Some(s) = self.current_mut() {
                s.scroll(isize::MIN / 2);
            }
            Mode::Term
        } else {
            if self.current().is_some() {
                self.flash("このセッションは終了しています（x で片付け）");
            }
            Mode::Nav
        }
    }

    fn key_nav(&mut self, k: KeyEvent) -> Mode {
        let has = !self.sessions.is_empty();
        if self.prefix.matches(&k) {
            // もう一度押すとそのキー自体を送る
            let byte = self.prefix.byte();
            if let Some(s) = self.current_mut() {
                s.write(&[byte]);
            }
            return self.enter_term();
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.pane.0 / 2).max(1) as isize;
        match k.code {
            KeyCode::Char('d') if ctrl => self.scroll(-half),
            KeyCode::Char('u') if ctrl => self.scroll(half),
            KeyCode::Char('j') | KeyCode::Down => self.select(self.sel + 1),
            KeyCode::Char('k') | KeyCode::Up => self.select(self.sel.saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char('i') | KeyCode::Char('l') | KeyCode::Right if has => return self.enter_term(),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < self.sessions.len() {
                    self.select(i);
                    return self.enter_term();
                }
            }
            KeyCode::Char('n') | KeyCode::Char('c') => {
                return Mode::New(NewForm { agent: 0, dir: self.last_dir.clone(), branch: String::new(), field: 0 });
            }
            KeyCode::Char('r') if has => return Mode::Rename(self.sessions[self.sel].name.clone()),
            KeyCode::Char('x') if has => {
                if self.sessions[self.sel].exit.is_some() {
                    self.remove_current();
                } else {
                    return Mode::ConfirmKill;
                }
            }
            KeyCode::Char('d') if has => {
                let lines = git::diff_lines(&self.sessions[self.sel].cwd);
                return Mode::Diff(DiffView { lines, scroll: 0 });
            }
            KeyCode::PageUp => self.scroll(half * 2),
            KeyCode::PageDown => self.scroll(-half * 2),
            KeyCode::Esc => self.scroll(isize::MIN / 2),
            KeyCode::Char('q') => {
                if self.alive() > 0 {
                    return Mode::ConfirmQuit;
                }
                self.quit = true;
            }
            KeyCode::Char('?') => return Mode::Help,
            _ => {}
        }
        Mode::Nav
    }

    fn scroll(&mut self, delta: isize) {
        if let Some(s) = self.current_mut() {
            s.scroll(delta);
        }
    }

    fn key_new(&mut self, mut f: NewForm, k: KeyEvent) -> Mode {
        let n = self.cfg.agents.len();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return Mode::Nav,
            KeyCode::Tab | KeyCode::Down => f.field = (f.field + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => f.field = (f.field + 2) % 3,
            KeyCode::Left if f.field == 0 => f.agent = (f.agent + n - 1) % n,
            KeyCode::Right | KeyCode::Char(' ') if f.field == 0 => f.agent = (f.agent + 1) % n,
            KeyCode::Char(c @ '1'..='9') if f.field == 0 => {
                let i = c as usize - '1' as usize;
                if i < n {
                    f.agent = i;
                }
            }
            KeyCode::Enter => match self.create(&f) {
                Ok(()) => return Mode::Term,
                Err(e) => self.flash(format!("{e:#}")),
            },
            KeyCode::Char('u') if ctrl => match f.field {
                1 => f.dir.clear(),
                2 => f.branch.clear(),
                _ => {}
            },
            KeyCode::Backspace => match f.field {
                1 => {
                    f.dir.pop();
                }
                2 => {
                    f.branch.pop();
                }
                _ => {}
            },
            KeyCode::Char(c) if !ctrl => match f.field {
                1 => f.dir.push(c),
                2 => f.branch.push(c),
                _ => {}
            },
            _ => {}
        }
        Mode::New(f)
    }

    fn create(&mut self, f: &NewForm) -> Result<()> {
        let agent = self.cfg.agents[f.agent].clone();
        let dir = expand(f.dir.trim());
        if !dir.is_dir() {
            bail!("フォルダがありません: {}", dir.display());
        }
        let dir = std::fs::canonicalize(&dir)?;
        let branch = f.branch.trim();
        let cwd = if branch.is_empty() { dir.clone() } else { git::ensure_worktree(&dir, branch)? };
        let name = if branch.is_empty() {
            cwd.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into())
        } else {
            branch.to_string()
        };
        let s = Session::spawn(name, &agent, cwd, self.pane, self.cfg.scrollback, self.dirty.clone())?;
        self.last_dir = tilde(&dir);
        self.sessions.push(s);
        self.sel = self.sessions.len() - 1;
        Ok(())
    }

    fn key_rename(&mut self, mut s: String, k: KeyEvent) -> Mode {
        match k.code {
            KeyCode::Esc => return Mode::Nav,
            KeyCode::Enter => {
                let name = s.trim().to_string();
                if !name.is_empty() {
                    if let Some(cur) = self.current_mut() {
                        cur.name = name;
                    }
                }
                return Mode::Nav;
            }
            KeyCode::Backspace => {
                s.pop();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => s.clear(),
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => s.push(c),
            _ => {}
        }
        Mode::Rename(s)
    }

    fn key_diff(&mut self, mut d: DiffView, k: KeyEvent) -> Mode {
        let page = self.pane.0.max(2) as usize - 1;
        let max = d.lines.len().saturating_sub(1);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('d') if !ctrl => return Mode::Nav,
            KeyCode::Char('j') | KeyCode::Down => d.scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => d.scroll = d.scroll.saturating_sub(1),
            KeyCode::Char(' ') | KeyCode::PageDown => d.scroll += page,
            KeyCode::Char('d') => d.scroll += page / 2,
            KeyCode::Char('u') if ctrl => d.scroll = d.scroll.saturating_sub(page / 2),
            KeyCode::Char('b') | KeyCode::PageUp => d.scroll = d.scroll.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => d.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => d.scroll = max,
            KeyCode::Char('R') => {
                if let Some(s) = self.current() {
                    d.lines = git::diff_lines(&s.cwd);
                }
            }
            _ => {}
        }
        d.scroll = d.scroll.min(max);
        Mode::Diff(d)
    }

    fn remove_current(&mut self) {
        if self.sel < self.sessions.len() {
            let mut s = self.sessions.remove(self.sel);
            s.kill();
            if self.sel >= self.sessions.len() {
                self.sel = self.sessions.len().saturating_sub(1);
            }
        }
    }

    pub fn shutdown(&mut self) {
        for s in &mut self.sessions {
            s.kill();
        }
    }
}

fn expand(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix('~') {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest.trim_start_matches('/'));
        }
    }
    PathBuf::from(s)
}

pub fn tilde(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().into_owned();
        if !home.is_empty() && s.starts_with(&home) {
            return format!("~{}", &s[home.len()..]);
        }
    }
    s
}
