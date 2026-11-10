use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// 画面を描き直してほしいときに呼ぶ
pub type Wake = Arc<dyn Fn() + Send + Sync>;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::config::Agent;

struct Shared {
    last_output: Mutex<Instant>,
    eof: AtomicBool,
}

pub struct Session {
    pub name: String,
    pub agent: String,
    pub cwd: PathBuf,
    pub branch: Option<String>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub created: Instant,
    pub last_input: Instant,
    /// 出力が続いている（入力のこだまではない）
    pub busy: bool,
    /// 手が空いた・終わったのを、まだ見ていない
    pub attention: bool,
    /// 終了していれば Some(終了コード)
    pub exit: Option<u32>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    shared: Arc<Shared>,
}

impl Session {
    pub fn spawn(
        name: String,
        agent: &Agent,
        cwd: PathBuf,
        (rows, cols): (u16, u16),
        scrollback: usize,
        wake: Wake,
    ) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("擬似端末を開けませんでした")?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut cmd = CommandBuilder::new(&shell);
        if agent.cmd.trim().is_empty() {
            cmd.arg("-l");
        } else {
            // .zshrc のPATHやエイリアスも効くように、対話シェル経由で起動する
            // （exec を付けるとエイリアスが展開されない）
            cmd.args(["-l", "-i", "-c", &agent.cmd]);
        }
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "mure");
        // mure を Claude Code などの中から起動したとき、その印を子に引き継がない
        for k in ["CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT", "TERM_PROGRAM_VERSION", "VSCODE_INJECTION"] {
            cmd.env_remove(k);
        }
        cmd.env("MURE", "1");
        cmd.env("MURE_SESSION", &name);

        let child = pair.slave.spawn_command(cmd).with_context(|| format!("{} を起動できませんでした", agent.name))?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pair.master.take_writer()?));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, scrollback)));
        let shared = Arc::new(Shared { last_output: Mutex::new(Instant::now()), eof: AtomicBool::new(false) });

        {
            let parser = parser.clone();
            let writer = writer.clone();
            let shared = shared.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let data = &buf[..n];
                            let reply = {
                                let mut p = parser.lock().unwrap();
                                p.process(data);
                                replies(data, p.screen())
                            };
                            if !reply.is_empty() {
                                let mut w = writer.lock().unwrap();
                                let _ = w.write_all(&reply);
                                let _ = w.flush();
                            }
                            *shared.last_output.lock().unwrap() = Instant::now();
                            wake();
                        }
                    }
                }
                shared.eof.store(true, Ordering::Relaxed);
                wake();
            });
        }

        let now = Instant::now();
        Ok(Self {
            branch: crate::git::current_branch(&cwd),
            name,
            agent: agent.name.clone(),
            cwd,
            parser,
            created: now,
            last_input: now,
            busy: false,
            attention: false,
            exit: None,
            writer,
            master: pair.master,
            child,
            shared,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.exit.is_some() {
            return;
        }
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
        drop(w);
        self.last_input = Instant::now();
        self.parser.lock().unwrap().set_scrollback(0);
    }

    pub fn paste(&mut self, text: &str) {
        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if bracketed {
            let mut v = b"\x1b[200~".to_vec();
            v.extend(text.as_bytes());
            v.extend(b"\x1b[201~");
            self.write(&v);
        } else {
            self.write(text.as_bytes());
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        self.parser.lock().unwrap().set_size(rows, cols);
    }

    pub fn scroll(&mut self, delta: isize) {
        let mut p = self.parser.lock().unwrap();
        let cur = p.screen().scrollback() as isize;
        p.set_scrollback((cur + delta).max(0) as usize);
    }

    /// 一覧のカードに出す、画面の中の意味のありそうな最後の1行
    pub fn preview(&self) -> String {
        let text = self.parser.lock().unwrap().screen().contents();
        for line in text.lines().rev() {
            let t = line.trim();
            if t.is_empty() || is_chrome(t) {
                continue;
            }
            return t.chars().take(120).collect();
        }
        String::new()
    }

    pub fn last_output(&self) -> Instant {
        *self.shared.last_output.lock().unwrap()
    }

    /// 出力が終わっていて子プロセスも終わっていれば終了コードを記録する。変化があれば true
    pub fn poll_exit(&mut self) -> bool {
        if self.exit.is_some() || !self.shared.eof.load(Ordering::Relaxed) {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(st)) => {
                self.exit = Some(st.exit_code());
                self.busy = false;
                true
            }
            Ok(None) => false,
            Err(_) => {
                self.exit = Some(1);
                true
            }
        }
    }

    /// 最後の入力のあと、どれだけ出力が続いたか
    pub fn work_after_input(&self) -> Duration {
        self.last_output().saturating_duration_since(self.last_input)
    }

    pub fn kill(&mut self) {
        if self.exit.is_none() {
            let _ = self.child.kill();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 端末への問い合わせに答える（codex などは返事がないと起動で止まる）
fn replies(data: &[u8], screen: &vt100::Screen) -> Vec<u8> {
    let has = |n: &[u8]| data.windows(n.len()).any(|w| w == n);
    let mut out = Vec::new();
    if has(b"\x1b[6n") {
        let (r, c) = screen.cursor_position();
        out.extend(format!("\x1b[{};{}R", r + 1, c + 1).bytes());
    }
    if has(b"\x1b[c") || has(b"\x1b[0c") {
        out.extend(b"\x1b[?62;22c");
    }
    if has(b"\x1b[>c") || has(b"\x1b[>0c") {
        out.extend(b"\x1b[>0;10;1c");
    }
    out
}

/// 入力欄の枠や操作の説明など、中身ではない行
fn is_chrome(t: &str) -> bool {
    let n = t.chars().count();
    let boxy = t.chars().filter(|c| ('\u{2500}'..='\u{259F}').contains(c)).count();
    boxy * 2 > n
        || t.starts_with('❯')
        || t.starts_with('›')
        || t.starts_with('>')
        || t.contains("⏵⏵")
        || t.contains("for shortcuts")
        || t.contains("to cycle")
}
