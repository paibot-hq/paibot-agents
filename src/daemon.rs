//! エージェントを持ち続ける裏のプロセス（デーモン）と、そこにつなぐための道具。
//!
//! 画面（GUI でもターミナルでも）を閉じてもエージェントは止まらず、あとからつなぎ直せる。
//! やりとりは Unix ソケットで、1つの枠は「長さ（4バイト）・種類（1バイト）・中身」。
use std::io::{self, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::pty::Pty;

/// やりとりの決まりの版。変えたら上げる（HELLO・LIST・SHUTDOWN の形は変えない）
pub const PROTO: u32 = 1;

/// 枠の種類
pub mod k {
    pub const HELLO: u8 = 1;
    pub const LIST: u8 = 2;
    pub const SPAWN: u8 = 3;
    pub const ATTACH: u8 = 4;
    pub const INPUT: u8 = 5;
    pub const RESIZE: u8 = 6;
    pub const CLOSE: u8 = 7;
    pub const META: u8 = 8;
    pub const SHUTDOWN: u8 = 9;

    pub const REPLY: u8 = 100;
    pub const OUTPUT: u8 = 101;
    pub const EXIT: u8 = 102;
    pub const ERROR: u8 = 103;
}

pub type Frame = (u8, Vec<u8>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    pub version: String,
    pub pid: u32,
}

/// セッション1つの様子
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Info {
    pub id: u64,
    pub name: String,
    pub agent: String,
    pub cwd: PathBuf,
    /// 始めた時刻（UNIX 秒）
    pub created: u64,
    pub exit: Option<u32>,
    /// 画面の側が覚えておいてほしいもの（再開用の ID など）
    #[serde(default)]
    pub meta: serde_json::Value,
    /// つないでいる画面の数
    pub clients: usize,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnReq {
    pub name: String,
    pub agent: String,
    pub cmd: String,
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
    pub scrollback: usize,
    #[serde(default)]
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachReq {
    pub id: u64,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdReq {
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaReq {
    pub id: u64,
    pub name: String,
    pub meta: serde_json::Value,
}

pub fn write_frame(w: &mut impl Write, kind: u8, data: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + data.len());
    buf.extend(((data.len() + 1) as u32).to_be_bytes());
    buf.push(kind);
    buf.extend(data);
    w.write_all(&buf)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<Frame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > 256 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    let kind = buf.remove(0);
    Ok((kind, buf))
}

pub fn send_json(w: &mut impl Write, kind: u8, v: &impl Serialize) -> io::Result<()> {
    write_frame(w, kind, &serde_json::to_vec(v).map_err(io::Error::other)?)
}

/// 次の REPLY を受け取る（ERROR ならそれをエラーにする）
pub fn read_reply<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> Result<T> {
    loop {
        match read_frame(r)? {
            (k::REPLY, d) => return Ok(serde_json::from_slice(&d)?),
            (k::ERROR, d) => bail!("{}", String::from_utf8_lossy(&d)),
            _ => continue,
        }
    }
}

pub fn sock_path() -> PathBuf {
    crate::config::dir().join("agents.sock")
}

// ---------------------------------------------------------------- デーモンの側

struct State {
    sessions: Mutex<Vec<Arc<Pty>>>,
    next_id: AtomicU64,
    next_conn: AtomicU64,
    clients: AtomicUsize,
    /// 最後に「誰もいない・何も動いていない」でなくなった時刻
    busy_at: Mutex<Instant>,
    path: PathBuf,
}

impl State {
    fn find(&self, id: u64) -> Result<Arc<Pty>> {
        self.sessions.lock().unwrap().iter().find(|p| p.id == id).cloned().ok_or_else(|| anyhow!("{}", lf!("セッション {id} はありません", "No session {id}")))
    }
}

/// デーモンとして動く。idle_exit があれば、動いているエージェントもつないでいる画面もない状態がその時間続いたら終わる
pub fn serve(path: &Path, idle_exit: Option<Duration>) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    if UnixStream::connect(path).is_ok() {
        bail!("{}", l!("デーモンはもう動いています", "The daemon is already running"));
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let st = Arc::new(State {
        sessions: Mutex::new(Vec::new()),
        next_id: AtomicU64::new(1),
        next_conn: AtomicU64::new(1),
        clients: AtomicUsize::new(0),
        busy_at: Mutex::new(Instant::now()),
        path: path.to_path_buf(),
    });
    if let Some(idle) = idle_exit {
        let st = st.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(1));
                let live = st.sessions.lock().unwrap().iter().any(|p| p.exit().is_none());
                if live || st.clients.load(Ordering::SeqCst) > 0 {
                    *st.busy_at.lock().unwrap() = Instant::now();
                } else if st.busy_at.lock().unwrap().elapsed() >= idle {
                    eprintln!("{}", l!("動いているセッションがないので終わります", "No sessions left; exiting"));
                    let _ = std::fs::remove_file(&st.path);
                    std::process::exit(0);
                }
            }
        });
    }
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let st = st.clone();
        st.clients.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            let _ = handle(&st, conn);
            st.clients.fetch_sub(1, Ordering::SeqCst);
            *st.busy_at.lock().unwrap() = Instant::now();
        });
    }
    Ok(())
}

fn handle(st: &State, conn: UnixStream) -> Result<()> {
    let conn_id = st.next_conn.fetch_add(1, Ordering::SeqCst);
    let mut rd = BufReader::new(conn.try_clone()?);
    let (tx, rx) = mpsc::channel::<Frame>();
    let mut w = conn.try_clone()?;
    // 書き出しは1本のスレッドにまとめる（出力と返事の順番が入れ替わらないように）
    thread::spawn(move || {
        for (kind, data) in rx {
            if write_frame(&mut w, kind, &data).is_err() {
                break;
            }
        }
        let _ = w.shutdown(Shutdown::Write);
    });
    let reply = |v: &dyn erased::Ser| {
        let _ = tx.send((k::REPLY, v.to_json()));
    };
    let mut attached: Option<Arc<Pty>> = None;
    loop {
        let Ok((kind, data)) = read_frame(&mut rd) else { break };
        let res: Result<()> = (|| {
            match kind {
                k::HELLO => reply(&Hello { proto: PROTO, version: env!("CARGO_PKG_VERSION").into(), pid: std::process::id() }),
                k::LIST => {
                    let list: Vec<Info> = st.sessions.lock().unwrap().iter().map(|p| p.info()).collect();
                    reply(&list);
                }
                k::SPAWN => {
                    let req: SpawnReq = serde_json::from_slice(&data)?;
                    let id = st.next_id.fetch_add(1, Ordering::SeqCst);
                    let pty = Pty::spawn(id, req)?;
                    st.sessions.lock().unwrap().push(pty.clone());
                    pty.subscribe(conn_id, &tx, None);
                    attached = Some(pty);
                }
                k::ATTACH => {
                    let req: AttachReq = serde_json::from_slice(&data)?;
                    let pty = st.find(req.id)?;
                    if let Some(old) = attached.take() {
                        old.unsubscribe(conn_id);
                    }
                    pty.subscribe(conn_id, &tx, Some((req.rows, req.cols)));
                    attached = Some(pty);
                }
                k::INPUT => {
                    if let Some(p) = &attached {
                        p.write(data);
                    }
                }
                k::RESIZE => {
                    let s: Size = serde_json::from_slice(&data)?;
                    if let Some(p) = &attached {
                        p.resize(s.rows, s.cols);
                    }
                }
                k::CLOSE => {
                    // 止めて、一覧からも外す
                    let req: IdReq = serde_json::from_slice(&data)?;
                    let mut list = st.sessions.lock().unwrap();
                    if let Some(i) = list.iter().position(|p| p.id == req.id) {
                        list.remove(i).kill();
                    }
                }
                k::META => {
                    let req: MetaReq = serde_json::from_slice(&data)?;
                    st.find(req.id)?.set_label(req.name, req.meta);
                }
                k::SHUTDOWN => {
                    for p in st.sessions.lock().unwrap().drain(..) {
                        p.kill();
                    }
                    let _ = std::fs::remove_file(&st.path);
                    eprintln!("{}", l!("止めるよう頼まれたので終わります", "Asked to stop; exiting"));
                    std::process::exit(0);
                }
                _ => bail!("unknown frame {kind}"),
            }
            Ok(())
        })();
        if let Err(e) = res {
            let _ = tx.send((k::ERROR, format!("{e:#}").into_bytes()));
        }
    }
    if let Some(p) = attached {
        p.unsubscribe(conn_id);
    }
    Ok(())
}

/// 返事をいろいろな型のまま渡すための小さな仕掛け
mod erased {
    pub trait Ser {
        fn to_json(&self) -> Vec<u8>;
    }
    impl<T: serde::Serialize> Ser for T {
        fn to_json(&self) -> Vec<u8> {
            serde_json::to_vec(self).unwrap_or_default()
        }
    }
}

// ---------------------------------------------------------------- つなぐ側

/// デーモンにつないで挨拶する。版が違えば Err(Some(相手の挨拶))
fn hello(path: &Path, timeout: Duration) -> std::result::Result<UnixStream, Option<Hello>> {
    let mut s = UnixStream::connect(path).map_err(|_| None)?;
    s.set_read_timeout(Some(timeout)).map_err(|_| None)?;
    send_json(&mut s, k::HELLO, &Hello { proto: PROTO, version: env!("CARGO_PKG_VERSION").into(), pid: std::process::id() }).map_err(|_| None)?;
    let h: Hello = read_reply(&mut s).map_err(|_| None)?;
    s.set_read_timeout(None).map_err(|_| None)?;
    if h.proto != PROTO {
        return Err(Some(h));
    }
    Ok(s)
}

/// デーモンにつなぐ。autostart なら、動いていなければ起動する
pub fn connect_at(path: &Path, autostart: bool) -> Result<UnixStream> {
    match hello(path, Duration::from_secs(3)) {
        Ok(s) => return Ok(s),
        Err(Some(old)) => replace_old(path, &old)?,
        Err(None) if !autostart => bail!("{}", l!("デーモンが動いていません", "The daemon is not running")),
        Err(None) => {}
    }
    start_daemon()?;
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        if let Ok(s) = hello(path, Duration::from_secs(3)) {
            return Ok(s);
        }
    }
    bail!("{}", lf!("デーモンを起動できませんでした（{} を見てください）", "Could not start the daemon (see {})", log_path().display()))
}

pub fn connect() -> Result<UnixStream> {
    connect_at(&sock_path(), true)
}

/// 版の違う古いデーモンが動いているとき: 何も動かしていなければ止めて入れ替える
fn replace_old(path: &Path, old: &Hello) -> Result<()> {
    let mut s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    send_json(&mut s, k::HELLO, &Hello { proto: old.proto, version: env!("CARGO_PKG_VERSION").into(), pid: std::process::id() })?;
    let _: Hello = read_reply(&mut s)?;
    write_frame(&mut s, k::LIST, &[])?;
    let list: Vec<Info> = read_reply(&mut s)?;
    let live = list.iter().filter(|i| i.exit.is_none()).count();
    if live > 0 {
        bail!(
            "{}",
            lf!(
                "古い版（{}）のデーモンでセッションが {live} 個動いています。終わってから開き直すか、paibot-agents daemon stop で止めてください",
                "An older daemon ({}) is running {live} sessions. Reopen after they finish, or stop it with paibot-agents daemon stop",
                old.version
            )
        );
    }
    write_frame(&mut s, k::SHUTDOWN, &[])?;
    for _ in 0..30 {
        if UnixStream::connect(path).is_err() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn log_path() -> PathBuf {
    crate::config::dir().join("daemon.log")
}

/// 自分自身を `daemon` で、端末から切り離して起動する
fn start_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let dir = crate::config::dir();
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_path())?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .current_dir(std::env::var_os("HOME").map(PathBuf::from).unwrap_or(dir));
    // 起動した画面やターミナルが閉じても道連れにならないように
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// 一覧。デーモンが動いていなければ起動しない
pub fn list_at(path: &Path) -> Result<Vec<Info>> {
    let mut s = hello(path, Duration::from_secs(1)).map_err(|_| anyhow!("{}", l!("デーモンが動いていません", "The daemon is not running")))?;
    s.set_read_timeout(Some(Duration::from_secs(1)))?;
    write_frame(&mut s, k::LIST, &[])?;
    read_reply(&mut s)
}

pub fn list() -> Result<Vec<Info>> {
    list_at(&sock_path())
}

/// 返事の要らない頼みごとを1つ送る
pub fn send_at(path: &Path, kind: u8, v: &impl Serialize) -> Result<()> {
    let mut s = connect_at(path, false)?;
    send_json(&mut s, kind, v)?;
    // 読み終わるまで待つ（相手が閉じるのを待たずに終わると取りこぼすことがある）
    let _ = s.shutdown(Shutdown::Write);
    let mut sink = Vec::new();
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    let _ = s.read_to_end(&mut sink);
    Ok(())
}

/// デーモンを止める（動いているエージェントも止まる）
pub fn shutdown() -> Result<bool> {
    let path = sock_path();
    let Ok(mut s) = UnixStream::connect(&path) else { return Ok(false) };
    write_frame(&mut s, k::SHUTDOWN, &[])?;
    let mut sink = Vec::new();
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    let _ = s.read_to_end(&mut sink);
    Ok(true)
}

/// 名前か番号でセッションを選ぶ
pub fn pick<'a>(list: &'a [Info], key: &str) -> Result<&'a Info> {
    if let Ok(id) = key.parse::<u64>() {
        if let Some(i) = list.iter().find(|i| i.id == id) {
            return Ok(i);
        }
    }
    let by_name: Vec<&Info> = list.iter().filter(|i| i.name == key).collect();
    match by_name.as_slice() {
        [one] => Ok(one),
        [] => bail!("{}", lf!("「{key}」というセッションはありません（paibot-agents ls）", "No session \"{key}\" (paibot-agents ls)")),
        _ => bail!("{}", lf!("「{key}」は複数あります。番号で選んでください（paibot-agents ls）", "\"{key}\" matches several sessions; use its number (paibot-agents ls)")),
    }
}
