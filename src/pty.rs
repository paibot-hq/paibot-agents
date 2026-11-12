//! デーモンの中で動くエージェント1つ分（擬似端末と、その画面の状態）
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::daemon::{Frame, Info, SpawnReq, k};

struct Inner {
    parser: vt100::Parser,
    /// つないでいる画面（接続ごとの番号と送り先）
    subs: Vec<(u64, mpsc::Sender<Frame>)>,
    exit: Option<u32>,
}

pub struct Pty {
    pub id: u64,
    pub agent: String,
    pub cwd: PathBuf,
    /// 始めた時刻（UNIX 秒）
    pub created: u64,
    /// 名前と、画面の側が覚えておいてほしいもの
    label: Mutex<(String, serde_json::Value)>,
    inner: Mutex<Inner>,
    /// エージェントへの入力は専用のスレッドで書く（相手が読まなくても止まらないように）
    input: mpsc::Sender<Vec<u8>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl Pty {
    pub fn spawn(id: u64, req: SpawnReq) -> Result<Arc<Self>> {
        let (rows, cols) = (req.rows.max(2), req.cols.max(2));
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context(l!("擬似端末を開けませんでした", "Could not open a pseudo terminal"))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut cmd = CommandBuilder::new(&shell);
        if req.cmd.trim().is_empty() {
            cmd.arg("-l");
        } else {
            // .zshrc のPATHやエイリアスも効くように、対話シェル経由で起動する
            // （exec を付けるとエイリアスが展開されない）
            cmd.args(["-l", "-i", "-c", &req.cmd]);
        }
        cmd.cwd(&req.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "paibot-agents");
        // Paibot Agents を Claude Code などの中から起動したとき、その印を子に引き継がない
        for k in ["CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT", "TERM_PROGRAM_VERSION", "VSCODE_INJECTION"] {
            cmd.env_remove(k);
        }
        cmd.env("PAIBOT_AGENTS", "1");
        cmd.env("PAIBOT_AGENTS_SESSION", &req.name);

        let child = pair.slave.spawn_command(cmd).with_context(|| lf!("{} を起動できませんでした", "Could not start {}", req.agent))?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (input, input_rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            for bytes in input_rx {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });

        let pty = Arc::new(Self {
            id,
            agent: req.agent,
            cwd: req.cwd,
            created: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            label: Mutex::new((req.name, req.meta)),
            inner: Mutex::new(Inner { parser: vt100::Parser::new(rows, cols, req.scrollback), subs: Vec::new(), exit: None }),
            input,
            master: Mutex::new(pair.master),
            child: Mutex::new(child),
        });
        let p = pty.clone();
        thread::spawn(move || p.pump(reader));
        Ok(pty)
    }

    /// エージェントの出力を読み、画面の状態を更新して、つないでいる画面に配る
    fn pump(&self, mut reader: Box<dyn Read + Send>) {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = &buf[..n];
                    let reply = {
                        let mut i = self.inner.lock().unwrap();
                        i.parser.process(data);
                        i.subs.retain(|(_, tx)| tx.send((k::OUTPUT, data.to_vec())).is_ok());
                        replies(data, i.parser.screen())
                    };
                    // 端末への問い合わせには、画面がつながっていなくてもここで答える
                    if !reply.is_empty() {
                        let _ = self.input.send(reply);
                    }
                }
            }
        }
        let code = loop {
            match self.child.lock().unwrap().try_wait() {
                Ok(Some(st)) => break st.exit_code(),
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => break 1,
            }
        };
        let mut i = self.inner.lock().unwrap();
        i.exit = Some(code);
        let body = serde_json::to_vec(&code).unwrap_or_default();
        for (_, tx) in i.subs.drain(..) {
            let _ = tx.send((k::EXIT, body.clone()));
        }
    }

    pub fn info(&self) -> Info {
        let i = self.inner.lock().unwrap();
        self.info_locked(&i)
    }

    fn info_locked(&self, i: &Inner) -> Info {
        let (name, meta) = self.label.lock().unwrap().clone();
        let (rows, cols) = i.parser.screen().size();
        Info { id: self.id, name, agent: self.agent.clone(), cwd: self.cwd.clone(), created: self.created, exit: i.exit, meta, clients: i.subs.len(), rows, cols }
    }

    pub fn exit(&self) -> Option<u32> {
        self.inner.lock().unwrap().exit
    }

    /// 画面をつなぐ。今の画面を送ってから、続きの出力を流す
    pub fn subscribe(&self, conn: u64, tx: &mpsc::Sender<Frame>, size: Option<(u16, u16)>) {
        let mut i = self.inner.lock().unwrap();
        if let Some((rows, cols)) = size.filter(|&(r, c)| r >= 2 && c >= 2 && i.parser.screen().size() != (r, c)) {
            if i.exit.is_none() {
                let _ = self.master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
            }
            i.parser.set_size(rows, cols);
        }
        let mut info = self.info_locked(&i);
        info.clients += 1;
        let _ = tx.send((k::REPLY, serde_json::to_vec(&info).unwrap_or_default()));
        let _ = tx.send((k::OUTPUT, snapshot(&mut i.parser)));
        match i.exit {
            Some(code) => {
                let _ = tx.send((k::EXIT, serde_json::to_vec(&code).unwrap_or_default()));
            }
            None => i.subs.push((conn, tx.clone())),
        }
    }

    pub fn unsubscribe(&self, conn: u64) {
        self.inner.lock().unwrap().subs.retain(|(c, _)| *c != conn);
    }

    pub fn write(&self, bytes: Vec<u8>) {
        if self.exit().is_none() {
            let _ = self.input.send(bytes);
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        if rows < 2 || cols < 2 {
            return;
        }
        let mut i = self.inner.lock().unwrap();
        if i.parser.screen().size() == (rows, cols) {
            return;
        }
        if i.exit.is_none() {
            let _ = self.master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
        i.parser.set_size(rows, cols);
    }

    pub fn set_label(&self, name: String, meta: serde_json::Value) {
        *self.label.lock().unwrap() = (name, meta);
    }

    pub fn kill(&self) {
        if self.exit().is_none() {
            let _ = self.child.lock().unwrap().kill();
        }
    }
}

/// 今の画面を、別の端末で同じ見た目に組み立て直すためのバイト列にする。
/// 遡れる行（スクロールバック）も先に流しておく
pub fn snapshot(parser: &mut vt100::Parser) -> Vec<u8> {
    let mut out = Vec::new();
    if parser.screen().alternate_screen() {
        out.extend(b"\x1b[?1049h");
    } else {
        let (rows, cols) = parser.screen().size();
        let keep = parser.screen().scrollback();
        parser.set_scrollback(usize::MAX);
        let total = parser.screen().scrollback().min(usize::from(u16::MAX - rows));
        if total > 0 {
            // vt100 0.15 は画面の行数より深く遡ると落ちるので、いったん画面を縦に伸ばして一度に読む。
            // 伸ばした分は下に足した空行なので、元の大きさに戻せば今の画面はそのまま
            parser.set_size(rows + total as u16, cols);
            parser.set_scrollback(total);
            for row in parser.screen().rows_formatted(0, cols).take(total) {
                out.extend(b"\x1b[0m");
                out.extend(row);
                out.extend(b"\x1b[0m\r\n");
            }
            parser.set_scrollback(0);
            parser.set_size(rows, cols);
        }
        parser.set_scrollback(keep.min(rows as usize));
        if total > 0 {
            // 画面に残っている分も、遡れる行へ押し出す（カーソルの行は空いているので rows - 1 回）
            for _ in 1..rows {
                out.extend(b"\r\n");
            }
        }
    }
    out.extend(parser.screen().state_formatted());
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_rebuilds_screen_and_history() {
        let mut p = vt100::Parser::new(5, 20, 100);
        for n in 0..12 {
            p.process(format!("\x1b[1mline {n}\x1b[0m\r\n").as_bytes());
        }
        p.process(b"\x1b[31mprompt> ");
        let snap = snapshot(&mut p);
        let mut q = vt100::Parser::new(5, 20, 100);
        q.process(&snap);
        assert_eq!(q.screen().contents(), p.screen().contents());
        assert_eq!(q.screen().cursor_position(), p.screen().cursor_position());
        assert_eq!(q.screen().cell(4, 0).unwrap().fgcolor(), vt100::Color::Idx(1));
        // 遡れる行も同じ（もう一度書き出して比べる）
        assert_eq!(String::from_utf8_lossy(&snapshot(&mut q)), String::from_utf8_lossy(&snapshot(&mut p)));
        assert!(String::from_utf8_lossy(&snapshot(&mut q)).contains("line 0"));
    }
}
