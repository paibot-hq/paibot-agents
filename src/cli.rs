//! ターミナルから使うコマンド: ls / attach / new / kill / daemon
use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};

use crate::daemon::{self, AttachReq, IdReq, Info, MetaReq, Size, SpawnReq, k};

/// 切り離すキー（Ctrl+\）
const DETACH: u8 = 0x1c;

pub fn run(cmd: &str, args: &[String]) -> Result<()> {
    match cmd {
        "ls" => ls(),
        "attach" | "a" => attach(args.first().map(String::as_str)),
        "new" => new(args),
        "kill" => kill(args.first().map(String::as_str)),
        "notes" => notes(args.first().map(String::as_str)),
        "daemon" => match args.first().map(String::as_str) {
            None => daemon::serve(&daemon::sock_path(), Some(Duration::from_secs(30))),
            Some("stop") => {
                if daemon::shutdown()? {
                    println!("{}", l!("デーモンを止めました", "Stopped the daemon"));
                } else {
                    println!("{}", l!("デーモンは動いていません", "The daemon is not running"));
                }
                Ok(())
            }
            Some(x) => bail!("{}", lf!("daemon のあとに {x} は使えません（stop だけ）", "Unknown: daemon {x} (only stop)")),
        },
        _ => unreachable!(),
    }
}

fn list_or_empty() -> Vec<Info> {
    daemon::list().unwrap_or_default()
}

fn ls() -> Result<()> {
    let list = list_or_empty();
    if list.is_empty() {
        println!("{}", l!("動いているセッションはありません", "No sessions"));
        return Ok(());
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let width = list.iter().map(|i| i.name.chars().count()).max().unwrap_or(4).max(4);
    for i in &list {
        let state = match i.exit {
            None => l!("動作中", "running").to_string(),
            Some(c) => lf!("終了({c})", "exited({c})"),
        };
        let seen = if i.clients > 0 { lf!("  画面{}", "  {} attached", i.clients) } else { String::new() };
        println!(
            "{:>3}  {:<width$}  {:<7} {:<10} {:>6}  {}{}",
            i.id,
            i.name,
            i.agent,
            state,
            ago(now.saturating_sub(i.created)),
            crate::util::tilde(&i.cwd),
            seen,
        );
    }
    Ok(())
}

fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// 引数がなければ、動いているのが1つだけのときそれを選ぶ
fn choose(key: Option<&str>) -> Result<Info> {
    let list = list_or_empty();
    if let Some(key) = key {
        return daemon::pick(&list, key).cloned();
    }
    let live: Vec<&Info> = list.iter().filter(|i| i.exit.is_none()).collect();
    match live.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("{}", l!("動いているセッションはありません（paibot-agents new で始められます）", "No running sessions (start one with paibot-agents new)")),
        _ => bail!("{}", l!("どれにつなぐか、番号か名前で選んでください（paibot-agents ls）", "Several sessions are running; pick one by number or name (paibot-agents ls)")),
    }
}

/// 共有メモを見る（paibot-agents notes [フォルダ]）
fn notes(dir: Option<&str>) -> Result<()> {
    let cwd = std::fs::canonicalize(dir.map(PathBuf::from).unwrap_or(std::env::current_dir()?))?;
    let list = crate::shared::read(&crate::shared::project_key(&cwd), 50)?;
    if list.is_empty() {
        println!("{}", l!("このプロジェクトの共有メモはまだありません", "No shared notes for this project yet"));
    } else {
        println!("{}", crate::shared::format(&list));
    }
    Ok(())
}

fn kill(key: Option<&str>) -> Result<()> {
    let Some(key) = key else { bail!("{}", l!("止めるセッションの番号か名前が要ります", "Give the number or name of the session to stop")) };
    let list = list_or_empty();
    let info = daemon::pick(&list, key)?;
    daemon::send_at(&daemon::sock_path(), k::CLOSE, &IdReq { id: info.id })?;
    println!("{}", lf!("{} を止めました", "Stopped {}", info.name));
    Ok(())
}

/// paibot-agents new [-d] [-a エージェント] [フォルダ]
fn new(args: &[String]) -> Result<()> {
    let (cfg, _) = crate::config::load();
    let mut detached = false;
    let mut agent = None;
    let mut dir = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-d" => detached = true,
            "-a" | "--agent" => agent = Some(it.next().cloned().ok_or_else(|| anyhow::anyhow!(l!("-a のあとにエージェント名が要ります", "-a needs an agent name")))?),
            _ if dir.is_none() => dir = Some(PathBuf::from(a)),
            _ => bail!("{}", l!("フォルダは1つだけ指定できます", "Only one folder can be given")),
        }
    }
    let agent = match agent {
        Some(name) => cfg.agents.iter().find(|a| a.name == name).cloned().ok_or_else(|| anyhow::anyhow!("{}", lf!("エージェント「{name}」は設定にありません", "Agent \"{name}\" is not in the settings")))?,
        // 指定がなければ、優先度と残りの利用枠で決める
        None => {
            let usages = crate::usage::snapshot(&cfg.agents);
            let a = cfg.agents.get(crate::usage::choose(&cfg, &usages)).cloned().ok_or_else(|| anyhow::anyhow!(l!("エージェントが設定にありません", "No agents in the settings")))?;
            let left = crate::usage::of(&usages, &a).map(|u| u.label(crate::usage::now_secs())).filter(|l| !l.is_empty());
            match left {
                Some(l) => eprintln!("{}", lf!("{} で始めます（{l}）", "Starting with {} ({l})", a.name)),
                None => eprintln!("{}", lf!("{} で始めます", "Starting with {}", a.name)),
            }
            a
        }
    };
    let cwd = std::fs::canonicalize(dir.unwrap_or(std::env::current_dir()?))?;
    if !cwd.is_dir() {
        bail!("{}", lf!("フォルダがありません: {}", "Folder not found: {}", cwd.display()));
    }
    let name = cwd.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
    let (mut cmd, resume_id) = crate::resume::start_cmd(&agent.cmd);
    cmd.push_str(&crate::shared::launch_args(&cfg, crate::resume::kind(&agent.cmd), &cwd, &agent.name, &name));
    let (rows, cols) = term_size().unwrap_or((40, 120));
    let meta = serde_json::json!({ "base_cmd": agent.cmd, "resume_id": resume_id, "roots": [] });
    let mut s = daemon::connect()?;
    let req = SpawnReq { name: name.clone(), agent: agent.name.clone(), cmd, cwd, rows, cols, scrollback: cfg.scrollback, meta: meta.clone() };
    daemon::send_json(&mut s, k::SPAWN, &req)?;
    let info: Info = daemon::read_reply(&mut s)?;
    daemon::send_json(&mut s, k::META, &MetaReq { id: info.id, name: info.name.clone(), meta })?;
    if detached {
        println!("{}", lf!("{} を始めました（{}）。paibot-agents attach {} でつなげます", "Started {} ({}). Attach with paibot-agents attach {}", name, agent.name, info.id));
        return Ok(());
    }
    drop(s);
    attach_to(&info)
}

fn attach(key: Option<&str>) -> Result<()> {
    let info = choose(key)?;
    attach_to(&info)
}

/// いまのターミナルでセッションにつなぐ。Ctrl+\ で切り離す
fn attach_to(info: &Info) -> Result<()> {
    if unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1 {
        bail!("{}", l!("ターミナルの中で実行してください", "Run this inside a terminal"));
    }
    let (rows, cols) = term_size().unwrap_or((24, 80));
    let mut s = daemon::connect_at(&daemon::sock_path(), false)?;
    daemon::send_json(&mut s, k::ATTACH, &AttachReq { id: info.id, rows, cols })?;
    let info: Info = daemon::read_reply(&mut s)?;

    let raw = RawMode::enter()?;
    let detached = Arc::new(AtomicBool::new(false));
    // キー入力を送る
    {
        let mut w = s.try_clone()?;
        let detached = detached.clone();
        thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let data = &buf[..n];
                if let Some(p) = data.iter().position(|&b| b == DETACH) {
                    if p > 0 {
                        let _ = daemon::write_frame(&mut w, k::INPUT, &data[..p]);
                    }
                    detached.store(true, Ordering::SeqCst);
                    let _ = w.shutdown(Shutdown::Both);
                    break;
                }
                if daemon::write_frame(&mut w, k::INPUT, data).is_err() {
                    break;
                }
            }
        });
    }
    // ターミナルの大きさが変わったら知らせる
    {
        let mut w = s.try_clone()?;
        thread::spawn(move || {
            let mut last = (rows, cols);
            loop {
                thread::sleep(Duration::from_millis(250));
                if let Some(now) = term_size() {
                    if now != last {
                        last = now;
                        if daemon::send_json(&mut w, k::RESIZE, &Size { rows: now.0, cols: now.1 }).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    // 出力をそのまま流す
    let mut out = std::io::stdout().lock();
    let mut r = std::io::BufReader::new(s.try_clone()?);
    let mut exit = None;
    loop {
        match daemon::read_frame(&mut r) {
            Ok((k::OUTPUT, data)) => {
                let _ = out.write_all(&data);
                let _ = out.flush();
            }
            Ok((k::EXIT, data)) => {
                exit = Some(serde_json::from_slice::<u32>(&data).unwrap_or(1));
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    // エージェントが変えた端末のモードを元に戻す
    let _ = out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1l\x1b>\x1b[?1049l\r\n");
    let _ = out.flush();
    drop(out);
    drop(raw);
    match exit {
        Some(code) => println!("{}", lf!("{} は終了しました（{code}）", "{} exited ({code})", info.name)),
        None if detached.load(Ordering::SeqCst) => {
            println!("{}", lf!("切り離しました。{} は動き続けます（paibot-agents attach {} でまたつなげます）", "Detached. {} keeps running (paibot-agents attach {} to come back)", info.name, info.id))
        }
        None => println!("{}", l!("デーモンとのつながりが切れました", "Lost the connection to the daemon")),
    }
    Ok(())
}

fn term_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
    (ok && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
}

/// 打ったキーをそのまま送れるように、ターミナルを生のモードにする（戻すのは drop で）
struct RawMode(libc::termios);

impl RawMode {
    fn enter() -> Result<Self> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            bail!("{}", l!("ターミナルの設定を読めませんでした", "Could not read the terminal settings"));
        }
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        Ok(Self(saved))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}
