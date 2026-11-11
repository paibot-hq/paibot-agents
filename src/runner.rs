//! paibot.app から仕事を受け取って、この Mac のエージェントで動かす
//!
//! Mac の側から paibot.app の /api/bots/runner に WebSocket でつなぎに行き、つないだまま待つ。
//! Mac に外からの入口は開けない。頼まれた仕事は、設定（config.toml の [[bots]]）に書いた Bot の、
//! 書いたフォルダとエージェントでだけ動かす。それ以外のことはしない。
//!
//! やりとり（JSON）
//! - Mac → paibot: hello {runner, bots}, progress {id, line}, done {id, ok, output}, pong
//! - paibot → Mac: task {id, bot, prompt}, ping

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::config::BotDef;
use crate::session::Wake;

const MAX_OUTPUT: usize = 20_000;
const TIMEOUT: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub connected: bool,
    pub running: usize,
    pub done: usize,
    pub last_error: Option<String>,
    /// 最近の仕事（Bot の名前と頼まれたこと）
    pub recent: Vec<String>,
}

#[derive(Clone)]
pub struct Settings {
    pub url: String,
    pub token: String,
    pub bots: Vec<BotDef>,
}

fn runner_name() -> String {
    Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Mac".into())
}

pub fn ws_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    let b = if let Some(r) = b.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = b.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        b.to_string()
    };
    format!("{b}/api/bots/runner")
}

fn hello(bots: &[BotDef]) -> String {
    let list: Vec<Value> = bots
        .iter()
        .map(|b| json!({ "name": b.name, "agent": b.agent, "note": if b.note.is_empty() { Value::Null } else { Value::String(b.note.clone()) } }))
        .collect();
    json!({ "type": "hello", "runner": runner_name(), "bots": list }).to_string()
}

fn clip(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if t.chars().count() < s.chars().count() { format!("{t}…") } else { t }
}

/// 頼まれた文はコマンドの文字列に埋め込まず、環境変数で渡す（シェルの解釈を受けないように）
/// claude が読むだけのときに読ませないファイル（秘密が入りがちなもの）
const CLAUDE_DENY: &str = r#"{"permissions":{"deny":["Read(**/.env*)","Read(**/.dev.vars*)","Read(**/*.pem)","Read(**/*.key)","Read(**/id_rsa*)","Read(**/id_ed25519*)","Read(~/.ssh/**)","Read(~/.aws/**)","Read(~/.config/**)","Read(~/.claude/**)","Read(~/.codex/**)","Read(~/.gemini/**)"]}}"#;

/// Bot を動かすコマンド。write でない Bot は読むだけ:
/// claude は --restricted（コマンドを実行する道具と自分の設定の許可を外す）で Read/Grep/Glob だけ、MCP なし、秘密のファイルは読めない。
/// codex は read-only のサンドボックス、agy は計画だけ（plan）でサンドボックス
pub fn command_for(bot: &BotDef) -> Option<String> {
    let extra = bot.args.trim();
    let ro = !bot.write;
    let line = match bot.agent.as_str() {
        "claude" => {
            let limit = if ro { format!(" --restricted --strict-mcp-config --tools Read,Grep,Glob --settings '{CLAUDE_DENY}'") } else { String::new() };
            format!("claude -p \"$PAIBOT_PROMPT\" --output-format stream-json --verbose{limit} {extra}")
        }
        "codex" => {
            let sandbox = if ro { "read-only" } else { "workspace-write" };
            format!("codex exec --skip-git-repo-check --sandbox {sandbox} -o \"$PAIBOT_OUT\" {extra} \"$PAIBOT_PROMPT\"")
        }
        "agy" => format!("agy -p \"$PAIBOT_PROMPT\"{} {extra}", if ro { " --mode plan --sandbox" } else { "" }),
        "echo" => "printf 'echo: %s' \"$PAIBOT_PROMPT\"".to_string(),
        _ => return None,
    };
    Some(line.trim().to_string())
}

/// claude の stream-json の1行から、途中の様子と最後の答えを取り出す
pub fn claude_event(line: &str) -> (Option<String>, Option<(bool, String)>) {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return (None, None) };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            let mut parts = Vec::new();
            if let Some(items) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                for it in items {
                    match it.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = it.get("text").and_then(|t| t.as_str()) {
                                if !t.trim().is_empty() {
                                    parts.push(clip(t.trim(), 200));
                                }
                            }
                        }
                        Some("tool_use") => {
                            let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                            let input = it.get("input").cloned().unwrap_or(Value::Null);
                            let brief = ["command", "file_path", "pattern", "url", "description"]
                                .iter()
                                .find_map(|k| input.get(*k).and_then(|x| x.as_str()).map(str::to_string))
                                .unwrap_or_default();
                            parts.push(clip(&format!("🔧 {name} {brief}"), 200));
                        }
                        _ => {}
                    }
                }
            }
            ((!parts.is_empty()).then(|| parts.join(" / ")), None)
        }
        Some("result") => {
            let ok = !v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let text = v.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string();
            (None, Some((ok, text)))
        }
        _ => (None, None),
    }
}

/// 1つの仕事を動かす。途中の様子は progress へ、最後に (成功したか, 答え)
pub fn run_task(bot: &BotDef, prompt: &str, progress: &dyn Fn(String)) -> (bool, String) {
    let Some(cmd) = command_for(bot) else {
        return (false, format!("この Mac では、エージェント「{}」を動かせません", bot.agent));
    };
    let folder = crate::util::expand(&bot.folder);
    if !folder.is_dir() {
        return (false, format!("フォルダがありません: {}", bot.folder));
    }
    let out_file = std::env::temp_dir().join(format!("paispace-task-{}.txt", std::process::id() ^ Instant::now().elapsed().subsec_nanos()));
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = match Command::new(&shell)
        .args(["-l", "-i", "-c", &cmd])
        .current_dir(&folder)
        .env("PAIBOT_PROMPT", prompt)
        .env("PAIBOT_OUT", &out_file)
        // paispace を Claude Code の中から起動しても、その印を子に引き継がない
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_CHILD_SESSION")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (false, format!("起動できませんでした: {e}")),
    };
    let started = Instant::now();
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }
    });
    let mut text = String::new();
    let mut result: Option<(bool, String)> = None;
    let mut last_sent = Instant::now() - Duration::from_secs(10);
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                if bot.agent == "claude" {
                    let (p, r) = claude_event(&line);
                    if let Some(p) = p {
                        progress(p);
                    }
                    if r.is_some() {
                        result = r;
                    }
                } else {
                    if text.len() < MAX_OUTPUT {
                        text.push_str(&line);
                        text.push('\n');
                    }
                    // それ以外は、出力の行を時々知らせる
                    if !line.trim().is_empty() && last_sent.elapsed() > Duration::from_secs(1) {
                        progress(clip(line.trim(), 200));
                        last_sent = Instant::now();
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if started.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    return (false, "時間がかかりすぎたので止めました（20分）".into());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = reader.join();
    let status = child.wait().ok();
    let exited_ok = status.is_some_and(|s| s.success());
    let (ok, output) = match result {
        Some(r) => r,
        None => {
            let from_file = std::fs::read_to_string(&out_file).unwrap_or_default();
            let out = if from_file.trim().is_empty() { text } else { from_file };
            (exited_ok, out.trim().to_string())
        }
    };
    let _ = std::fs::remove_file(&out_file);
    (ok, clip(&output, MAX_OUTPUT))
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

fn connect(settings: &Settings) -> Result<Socket, String> {
    let mut req = ws_url(&settings.url).into_client_request().map_err(|e| e.to_string())?;
    let auth = format!("Bearer {}", settings.token.trim());
    req.headers_mut().insert("Authorization", auth.parse().map_err(|_| "鍵の形がおかしいです".to_string())?);
    let (mut ws, resp) = tungstenite::connect(req).map_err(|e| match e {
        tungstenite::Error::Http(r) if r.status() == 401 => "鍵が違います（paibot.app でつなぎ直してください）".to_string(),
        other => other.to_string(),
    })?;
    if resp.status().as_u16() != 101 {
        return Err(format!("つなげませんでした（{}）", resp.status()));
    }
    // 読むのを短く区切って、その合間に送る
    let timeout = Some(Duration::from_millis(200));
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(timeout);
        }
        MaybeTlsStream::NativeTls(t) => {
            let _ = t.get_mut().set_read_timeout(timeout);
        }
        _ => {}
    }
    Ok(ws)
}

/// 1回つないで、切れるまで仕事を受ける
fn session(settings: &Settings, status: &Arc<Mutex<Status>>, wake: &Wake) -> Result<(), String> {
    let mut ws = connect(settings)?;
    ws.send(Message::text(hello(&settings.bots))).map_err(|e| e.to_string())?;
    {
        let mut s = status.lock().unwrap();
        s.connected = true;
        s.last_error = None;
    }
    wake();
    let (out_tx, out_rx) = mpsc::channel::<String>();
    let mut last_ping = Instant::now();
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                match v.get("type").and_then(|x| x.as_str()) {
                    Some("task") => {
                        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let name = v.get("bot").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let prompt = v.get("prompt").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let bot = settings.bots.iter().find(|b| b.name == name).cloned();
                        let tx = out_tx.clone();
                        let st = status.clone();
                        let wk = wake.clone();
                        {
                            let mut s = st.lock().unwrap();
                            s.running += 1;
                            s.recent.insert(0, format!("@{name}  {}", clip(&prompt, 60)));
                            s.recent.truncate(5);
                        }
                        wk();
                        std::thread::spawn(move || {
                            let (ok, output) = match bot {
                                Some(b) => {
                                    let ptx = tx.clone();
                                    let pid = id.clone();
                                    run_task(&b, &prompt, &move |line| {
                                        let _ = ptx.send(json!({ "type": "progress", "id": pid, "line": line }).to_string());
                                    })
                                }
                                // 設定に無い Bot は動かさない
                                None => (false, format!("この Mac に「{name}」という Bot はありません")),
                            };
                            let _ = tx.send(json!({ "type": "done", "id": id, "ok": ok, "output": output }).to_string());
                            let mut s = st.lock().unwrap();
                            s.running = s.running.saturating_sub(1);
                            s.done += 1;
                            drop(s);
                            wk();
                        });
                    }
                    Some("ping") => {
                        let _ = ws.send(Message::text(r#"{"type":"pong"}"#));
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(_)) => return Err("paibot.app が接続を閉じました".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.to_string()),
        }
        while let Ok(msg) = out_rx.try_recv() {
            ws.send(Message::text(msg)).map_err(|e| e.to_string())?;
        }
        if last_ping.elapsed() > Duration::from_secs(25) {
            ws.send(Message::text(r#"{"type":"ping"}"#)).map_err(|e| e.to_string())?;
            last_ping = Instant::now();
        }
    }
}

/// 裏でつなぎ続ける。切れたら少し待ってつなぎ直す
pub fn spawn(settings: Settings, status: Arc<Mutex<Status>>, wake: Wake) {
    std::thread::spawn(move || {
        let mut wait = Duration::from_secs(2);
        loop {
            let started = Instant::now();
            let err = session(&settings, &status, &wake).err();
            {
                let mut s = status.lock().unwrap();
                s.connected = false;
                s.last_error = err;
            }
            wake();
            // 長くつながっていたなら、すぐにつなぎ直す
            if started.elapsed() > Duration::from_secs(60) {
                wait = Duration::from_secs(2);
            }
            std::thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_secs(60));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(agent: &str) -> BotDef {
        BotDef { name: "t".into(), agent: agent.into(), folder: std::env::temp_dir().to_string_lossy().into_owned(), note: String::new(), args: String::new(), write: false }
    }

    #[test]
    fn urls_and_commands() {
        assert_eq!(ws_url("https://paibot.app/"), "wss://paibot.app/api/bots/runner");
        assert_eq!(ws_url("http://localhost:3002"), "ws://localhost:3002/api/bots/runner");
        assert!(command_for(&bot("claude")).unwrap().starts_with("claude -p \"$PAIBOT_PROMPT\""));
        assert!(command_for(&bot("rm")).is_none());
        // 初めは読むだけ
        let claude = command_for(&bot("claude")).unwrap();
        assert!(claude.contains("--restricted") && claude.contains("--tools Read,Grep,Glob") && claude.contains("Read(**/.env*)"));
        assert!(command_for(&bot("codex")).unwrap().contains("--sandbox read-only"));
        assert!(command_for(&bot("agy")).unwrap().contains("--mode plan --sandbox"));
        // write = true にしたときだけ外れる
        let mut w = bot("claude");
        w.write = true;
        assert!(!command_for(&w).unwrap().contains("--restricted"));
        w.agent = "codex".into();
        assert!(command_for(&w).unwrap().contains("--sandbox workspace-write"));
    }

    #[test]
    fn reads_claude_stream() {
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert_eq!(claude_event(tool).0.as_deref(), Some("🔧 Bash ls"));
        let done = r#"{"type":"result","is_error":false,"result":"3 files"}"#;
        assert_eq!(claude_event(done).1, Some((true, "3 files".into())));
        assert_eq!(claude_event("not json"), (None, None));
    }

    #[test]
    fn echo_bot_runs_without_shell_interpretation() {
        // 頼まれた文にシェルの記号があっても、そのまま文字として渡る
        let (ok, out) = run_task(&bot("echo"), "hi $(touch /tmp/should-not-exist) `id`", &|_| {});
        assert!(ok);
        assert_eq!(out, "echo: hi $(touch /tmp/should-not-exist) `id`");
        assert!(!std::path::Path::new("/tmp/should-not-exist").exists());
    }
}
