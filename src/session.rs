//! 画面の側から見たセッション。エージェント自体はデーモン（daemon.rs）が持っていて、
//! ここはそこにつないで画面の状態を写し、入力を送るだけ。閉じてもエージェントは止まらない
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::config::Agent;
use crate::daemon::{self, AttachReq, Frame, IdReq, Info, MetaReq, Size, SpawnReq, k};

/// 画面を描き直してほしいときに呼ぶ
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// 画面を閉じるときにエージェントも止める（確認用の画面撮りのときだけ）
static CLOSE_ON_DROP: AtomicBool = AtomicBool::new(false);

pub fn close_on_drop(on: bool) {
    CLOSE_ON_DROP.store(on, Ordering::Relaxed);
}

/// デーモンとのつながりが切れたときの終了コード
pub const LOST: u32 = 255;

struct Shared {
    last_output: Mutex<Instant>,
    /// デーモンから届いた終了コード（つながりが切れたら LOST）
    ended: Mutex<Option<u32>>,
}

pub struct Session {
    /// デーモンの中での番号
    pub id: u64,
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
    /// 再開用の会話の ID（claude は始めるときに決める、codex はあとで探す）
    pub resume_id: Option<String>,
    /// --session-id などを付ける前のコマンド
    pub base_cmd: String,
    pub started_at: SystemTime,
    /// ワークスペースで始めたなら、そのフォルダたち（cwd は1つ目）
    pub roots: Vec<PathBuf>,
    /// デーモンへ送る枠（書き出しは専用のスレッドで。相手が詰まっても画面が止まらないように）
    tx: mpsc::Sender<Frame>,
    shared: Arc<Shared>,
    path: PathBuf,
    /// 最後にデーモンへ送った名前と覚え書き
    meta_sent: String,
}

impl Session {
    /// デーモンでエージェントを起動して、つなぐ（デーモンが動いていなければ起動する）
    pub fn spawn(name: String, agent: &Agent, cwd: PathBuf, size: (u16, u16), scrollback: usize, wake: Wake) -> Result<Self> {
        Self::spawn_at(&daemon::sock_path(), true, name, agent, cwd, size, scrollback, wake)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_at(path: &Path, autostart: bool, name: String, agent: &Agent, cwd: PathBuf, (rows, cols): (u16, u16), scrollback: usize, wake: Wake) -> Result<Self> {
        let mut s = daemon::connect_at(path, autostart)?;
        let req = SpawnReq { name, agent: agent.name.clone(), cmd: agent.cmd.clone(), cwd, rows, cols, scrollback, meta: serde_json::Value::Null };
        daemon::send_json(&mut s, k::SPAWN, &req)?;
        let info: Info = daemon::read_reply(&mut s)?;
        let mut me = Self::from_conn(path, s, &info, scrollback, wake)?;
        me.base_cmd = agent.cmd.clone();
        Ok(me)
    }

    /// デーモンで動いているセッションにつなぐ
    pub fn attach(info: &Info, size: (u16, u16), scrollback: usize, wake: Wake) -> Result<Self> {
        Self::attach_at(&daemon::sock_path(), info.id, size, scrollback, wake)
    }

    fn attach_at(path: &Path, id: u64, (rows, cols): (u16, u16), scrollback: usize, wake: Wake) -> Result<Self> {
        let mut s = daemon::connect_at(path, false)?;
        daemon::send_json(&mut s, k::ATTACH, &AttachReq { id, rows, cols })?;
        let info: Info = daemon::read_reply(&mut s)?;
        let mut me = Self::from_conn(path, s, &info, scrollback, wake)?;
        me.load_meta(&info.meta);
        me.meta_sent = me.meta_key();
        Ok(me)
    }

    fn from_conn(path: &Path, stream: UnixStream, info: &Info, scrollback: usize, wake: Wake) -> Result<Self> {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(info.rows.max(2), info.cols.max(2), scrollback)));
        let now = Instant::now();
        let shared = Arc::new(Shared { last_output: Mutex::new(now), ended: Mutex::new(info.exit) });

        let (tx, rx) = mpsc::channel::<Frame>();
        let mut w = stream.try_clone()?;
        thread::spawn(move || {
            for (kind, data) in rx {
                if daemon::write_frame(&mut w, kind, &data).is_err() {
                    break;
                }
            }
            // 送り残しを書き終えてから切る（画面を閉じる直前の名前の変更なども届くように）
            let _ = w.shutdown(Shutdown::Both);
        });
        {
            let mut r = std::io::BufReader::new(stream.try_clone()?);
            let parser = parser.clone();
            let shared = shared.clone();
            thread::spawn(move || {
                // 最初の出力はつないだときの画面なので、「動いている」の判定には数えない
                let mut first = true;
                loop {
                    match daemon::read_frame(&mut r) {
                        Ok((k::OUTPUT, data)) => {
                            parser.lock().unwrap().process(&data);
                            if !first {
                                *shared.last_output.lock().unwrap() = Instant::now();
                            }
                            first = false;
                        }
                        Ok((k::EXIT, data)) => {
                            *shared.ended.lock().unwrap() = Some(serde_json::from_slice(&data).unwrap_or(1));
                        }
                        Ok(_) => continue,
                        Err(_) => {
                            shared.ended.lock().unwrap().get_or_insert(LOST);
                            wake();
                            break;
                        }
                    }
                    wake();
                }
            });
        }

        let unix_now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let age = Duration::from_secs(unix_now.saturating_sub(info.created));
        let cwd = info.cwd.clone();
        Ok(Self {
            id: info.id,
            branch: crate::git::current_branch(&cwd),
            name: info.name.clone(),
            agent: info.agent.clone(),
            cwd,
            parser,
            created: now.checked_sub(age).unwrap_or(now),
            last_input: now,
            busy: false,
            attention: false,
            exit: None,
            resume_id: None,
            base_cmd: String::new(),
            started_at: UNIX_EPOCH + Duration::from_secs(info.created),
            roots: Vec::new(),
            tx,
            shared,
            path: path.to_path_buf(),
            meta_sent: String::new(),
        })
    }

    fn meta(&self) -> serde_json::Value {
        serde_json::json!({
            "base_cmd": self.base_cmd,
            "resume_id": self.resume_id,
            "roots": self.roots,
        })
    }

    fn load_meta(&mut self, m: &serde_json::Value) {
        if let Some(c) = m.get("base_cmd").and_then(|v| v.as_str()) {
            self.base_cmd = c.to_string();
        }
        self.resume_id = m.get("resume_id").and_then(|v| v.as_str()).map(String::from);
        self.roots = m.get("roots").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
    }

    fn meta_key(&self) -> String {
        format!("{}\n{}", self.name, self.meta())
    }

    /// 名前や再開用の ID が変わっていたらデーモンにも覚えさせる（あとでつなぎ直したときのため）
    pub fn sync_meta(&mut self) {
        let key = self.meta_key();
        if key == self.meta_sent {
            return;
        }
        let req = MetaReq { id: self.id, name: self.name.clone(), meta: self.meta() };
        if let Ok(body) = serde_json::to_vec(&req) {
            if self.tx.send((k::META, body)).is_ok() {
                self.meta_sent = key;
            }
        }
    }

    /// ファイルのツリーなどで見せるフォルダ
    pub fn view_roots(&self) -> Vec<PathBuf> {
        if self.roots.len() > 1 { self.roots.clone() } else { vec![self.cwd.clone()] }
    }

    /// ワークスペースならその名前
    pub fn ws_title(&self) -> Option<String> {
        (self.roots.len() > 1).then(|| self.name.clone())
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.exit.is_some() {
            return;
        }
        let _ = self.tx.send((k::INPUT, bytes.to_vec()));
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
        if let Ok(body) = serde_json::to_vec(&Size { rows, cols }) {
            let _ = self.tx.send((k::RESIZE, body));
        }
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

    /// 終わったと知らせが来ていれば終了コードを記録する。変化があれば true
    pub fn poll_exit(&mut self) -> bool {
        if self.exit.is_some() {
            return false;
        }
        match *self.shared.ended.lock().unwrap() {
            Some(code) => {
                self.exit = Some(code);
                self.busy = false;
                true
            }
            None => false,
        }
    }

    /// 最後の入力のあと、どれだけ出力が続いたか
    pub fn work_after_input(&self) -> Duration {
        self.last_output().saturating_duration_since(self.last_input)
    }

    /// エージェントを止めて、デーモンの一覧からも外す
    pub fn kill(&mut self) {
        let _ = daemon::send_at(&self.path, k::CLOSE, &IdReq { id: self.id });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if CLOSE_ON_DROP.load(Ordering::Relaxed) {
            self.kill();
        }
        // つなぐのをやめるだけ。エージェントはデーモンの中で動き続ける。
        // 送り先を閉じると、書き出しのスレッドが残りを送ってから接続を切る
        self.tx = mpsc::channel().0;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// テスト用のデーモン（このプロセスの中で1つだけ動かす）
    fn daemon() -> &'static Path {
        static PATH: OnceLock<PathBuf> = OnceLock::new();
        PATH.get_or_init(|| {
            let p = std::env::temp_dir().join(format!("pa-test-{}.sock", std::process::id()));
            let q = p.clone();
            thread::spawn(move || daemon::serve(&q, None).unwrap());
            for _ in 0..50 {
                if UnixStream::connect(&p).is_ok() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            p
        })
    }

    fn spawn(cmd: &str) -> Session {
        let agent = Agent { name: "t".into(), cmd: cmd.into() };
        Session::spawn_at(daemon(), false, "t".into(), &agent, std::env::temp_dir(), (24, 80), 100, Arc::new(|| {})).unwrap()
    }

    fn wait_for(s: &Session, text: &str) -> bool {
        for _ in 0..50 {
            if s.parser.lock().unwrap().screen().contents().contains(text) {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        false
    }

    #[test]
    fn input_reaches_the_program() {
        let mut s = spawn("cat");
        thread::sleep(Duration::from_millis(1500));
        s.write("こんにちは abc\r".as_bytes());
        assert!(wait_for(&s, "こんにちは abc"), "画面: {}", s.parser.lock().unwrap().screen().contents());
        s.kill();
    }

    #[test]
    fn writing_to_a_program_that_never_reads_does_not_block() {
        let mut s = spawn("sleep 30");
        let big = vec![b'x'; 200 * 1024];
        let t = Instant::now();
        s.write(&big);
        s.write(b"more");
        assert!(t.elapsed() < Duration::from_millis(200), "{:?}", t.elapsed());
        s.kill();
    }

    #[test]
    fn keeps_running_after_the_screen_goes_away_and_can_be_reattached() {
        let mut s = spawn("cat");
        thread::sleep(Duration::from_millis(1500));
        s.write(b"before\r");
        assert!(wait_for(&s, "before"));
        s.name = "renamed".into();
        s.resume_id = Some("abc".into());
        s.sync_meta();
        let id = s.id;
        drop(s);
        thread::sleep(Duration::from_millis(200));

        let info = daemon::list_at(daemon()).unwrap().into_iter().find(|i| i.id == id).expect("一覧に残っている");
        assert_eq!(info.exit, None);
        assert_eq!(info.name, "renamed");
        let mut again = Session::attach_at(daemon(), id, (24, 80), 100, Arc::new(|| {})).unwrap();
        assert_eq!(again.resume_id.as_deref(), Some("abc"));
        assert!(wait_for(&again, "before"), "画面: {}", again.parser.lock().unwrap().screen().contents());
        again.write(b"after\r");
        assert!(wait_for(&again, "after"));
        again.kill();
        thread::sleep(Duration::from_millis(200));
        assert!(!daemon::list_at(daemon()).unwrap().iter().any(|i| i.id == id), "止めたら一覧から消える");
    }

    #[test]
    fn exit_code_reaches_the_screen() {
        let mut s = spawn("exit 3");
        for _ in 0..50 {
            if s.poll_exit() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(s.exit, Some(3));
        s.kill();
    }
}
