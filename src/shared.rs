//! エージェント同士の共有メモ。
//!
//! `paibot-agents mcp` が小さな MCP サーバーになり、同じリポジトリで動くエージェント
//! （claude・codex、同時でも順番でも）が同じメモを読み書きする。メモは設定フォルダの SQLite に置く。
//! エージェントにはそのセッションを起動するときだけ組み込み、ユーザーの設定ファイルは触らない
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::resume::Kind;

const SERVER: &str = "paibot";

pub fn db_path() -> PathBuf {
    crate::config::dir().join("shared.db")
}

fn open(path: &Path) -> Result<Connection> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS notes (
           id INTEGER PRIMARY KEY,
           project TEXT NOT NULL,
           agent TEXT NOT NULL,
           session TEXT NOT NULL,
           kind TEXT NOT NULL,
           text TEXT NOT NULL,
           created INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS notes_project ON notes(project, id);",
    )?;
    Ok(c)
}

/// メモを分ける単位: git のリポジトリ（worktree も同じ）。git でなければそのフォルダ
pub fn project_key(cwd: &Path) -> String {
    let common = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    match common {
        // .git の親（リポジトリの場所）
        Some(c) => Path::new(&c).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or(c),
        None => std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()).to_string_lossy().into_owned(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub agent: String,
    pub session: String,
    pub kind: String,
    pub text: String,
    pub created: u64,
}

pub const KINDS: [&str; 6] = ["status", "decision", "todo", "done", "note", "handoff"];

pub fn write_at(db: &Path, project: &str, agent: &str, session: &str, kind: &str, text: &str) -> Result<()> {
    let c = open(db)?;
    c.execute(
        "INSERT INTO notes(project, agent, session, kind, text, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![project, agent, session, kind, text, crate::usage::now_secs() as i64],
    )?;
    Ok(())
}

/// 新しいものから limit 件（返すのは古い順）
pub fn read_at(db: &Path, project: &str, limit: usize, kind: Option<&str>, query: Option<&str>) -> Result<Vec<Note>> {
    let c = open(db)?;
    let mut st = c.prepare(
        "SELECT agent, session, kind, text, created FROM notes
         WHERE project = ?1 AND (?2 IS NULL OR kind = ?2) AND (?3 IS NULL OR text LIKE '%' || ?3 || '%')
         ORDER BY id DESC LIMIT ?4",
    )?;
    let mut v: Vec<Note> = st
        .query_map(params![project, kind, query, limit as i64], |r| {
            Ok(Note { agent: r.get(0)?, session: r.get(1)?, kind: r.get(2)?, text: r.get(3)?, created: r.get::<_, i64>(4)? as u64 })
        })?
        .collect::<rusqlite::Result<_>>()?;
    v.reverse();
    Ok(v)
}

pub fn write(project: &str, agent: &str, session: &str, kind: &str, text: &str) -> Result<()> {
    write_at(&db_path(), project, agent, session, kind, text)
}

pub fn read(project: &str, limit: usize) -> Result<Vec<Note>> {
    read_at(&db_path(), project, limit, None, None)
}

fn stamp(secs: u64) -> String {
    // 端末の時刻帯に合わせる
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

pub fn format(notes: &[Note]) -> String {
    notes.iter().map(|n| format!("[{}] {} ({}) {}: {}", stamp(n.created), n.agent, n.session, n.kind, n.text)).collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------- MCP サーバー

const INSTRUCTIONS: &str = "Shared notes for this project. Other coding agents (claude, codex, ...) work in the same repository, \
sometimes at the same time as you and sometimes after you, and they read and write these same notes. \
At the start of a task, call notes_read to see what others are doing, what was decided and what is left. \
Call notes_write with a short note when you start something others should know about (kind=status), make a decision (decision), \
finish a step (done), leave work unfinished (todo), or are about to stop. Keep notes short and concrete: what, where (files), why.";

fn tools() -> Value {
    json!([
        {
            "name": "notes_read",
            "description": "Read the shared notes for this project, written by you and other coding agents. Call this at the start of a task.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "How many recent notes (default 30, max 200)"},
                    "kind": {"type": "string", "enum": KINDS, "description": "Only this kind"},
                    "query": {"type": "string", "description": "Only notes containing this text"}
                }
            }
        },
        {
            "name": "notes_write",
            "description": "Add a short note for the other coding agents working in this project: what you are doing (status), a decision, a finished step (done), or unfinished work (todo).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["status", "decision", "todo", "done", "note"]},
                    "text": {"type": "string", "description": "The note. Short and concrete: what, which files, why."}
                },
                "required": ["kind", "text"]
            }
        }
    ])
}

pub struct Server {
    pub db: PathBuf,
    pub project: String,
    pub agent: String,
    pub session: String,
}

impl Server {
    /// JSON-RPC の1通に答える（通知なら None）
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id")?.clone();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let p = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": p.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER, "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => Ok(self.call(p.get("name").and_then(Value::as_str).unwrap_or(""), p.get("arguments").unwrap_or(&Value::Null))),
            _ => Err(json!({"code": -32601, "message": format!("unknown method {method}")})),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        })
    }

    fn call(&self, name: &str, args: &Value) -> Value {
        let text = |t: String, err: bool| json!({"content": [{"type": "text", "text": t}], "isError": err});
        match name {
            "notes_read" => {
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(30).clamp(1, 200) as usize;
                let kind = args.get("kind").and_then(Value::as_str);
                let query = args.get("query").and_then(Value::as_str).filter(|q| !q.is_empty());
                match read_at(&self.db, &self.project, limit, kind, query) {
                    Ok(v) if v.is_empty() => text("No notes yet for this project.".into(), false),
                    Ok(v) => text(format(&v), false),
                    Err(e) => text(format!("Could not read notes: {e:#}"), true),
                }
            }
            "notes_write" => {
                let kind = args.get("kind").and_then(Value::as_str).filter(|k| KINDS.contains(k)).unwrap_or("note");
                let Some(t) = args.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) else {
                    return text("text is required".into(), true);
                };
                let t: String = t.chars().take(4000).collect();
                match write_at(&self.db, &self.project, &self.agent, &self.session, kind, &t) {
                    Ok(()) => text("Saved.".into(), false),
                    Err(e) => text(format!("Could not save: {e:#}"), true),
                }
            }
            _ => text(format!("unknown tool {name}"), true),
        }
    }
}

/// paibot-agents mcp --project P --agent A --session S（標準入出力で話す MCP サーバー）
pub fn serve(args: &[String]) -> Result<()> {
    let mut project = None;
    let mut agent = "agent".to_string();
    // Paibot Agents から起動したセッションなら、その名前が環境変数で来る（agy は引数を変えられないので）
    let mut session = std::env::var("PAIBOT_AGENTS_SESSION").unwrap_or_default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--project" => project = it.next().cloned(),
            "--agent" => agent = it.next().cloned().unwrap_or(agent),
            "--session" => session = it.next().cloned().unwrap_or_default(),
            _ => {}
        }
    }
    let project = project.unwrap_or_else(|| project_key(&std::env::current_dir().unwrap_or_default()));
    let srv = Server { db: db_path(), project, agent, session };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(reply) = srv.handle(&msg) {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- agy への登録
//
// agy は起動のときだけ MCP サーバーを足す方法がないので、ユーザーが選んだときだけ
// agy の設定に `paibot` を登録する（agy mcp add / remove。agy mcp list で見える）

/// agy のコマンドを、.zshrc の PATH が効く対話シェル経由で実行する
fn agy(args: &[&str]) -> Result<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let line = std::iter::once("agy".to_string()).chain(args.iter().map(|a| shq(a))).collect::<Vec<_>>().join(" ");
    let o = std::process::Command::new(shell).args(["-l", "-i", "-c", &line]).stdin(std::process::Stdio::null()).output()?;
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    if !o.status.success() {
        anyhow::bail!("{}", text.trim());
    }
    Ok(text)
}

/// agy に共有メモが登録されているか（agy が動かなければ None）
pub fn agy_registered() -> Option<bool> {
    let out = agy(&["mcp", "list"]).ok()?;
    Some(out.lines().any(|l| l.split_whitespace().next() == Some(SERVER)))
}

/// agy に共有メモを登録する（もうあれば今のバイナリの場所で入れ直す）
pub fn agy_install() -> Result<()> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    agy(&["mcp", "add", SERVER, &exe, "mcp", "--agent", "agy"])?;
    Ok(())
}

pub fn agy_uninstall() -> Result<()> {
    agy(&["mcp", "remove", SERVER])?;
    Ok(())
}

// ---------------------------------------------------------------- 起動のときに足す引数

fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn mcp_args(cwd: &Path, agent: &str, session: &str) -> Vec<String> {
    vec!["mcp".into(), "--project".into(), project_key(cwd), "--agent".into(), agent.into(), "--session".into(), session.into()]
}

/// エージェントを起動するコマンドに足す引数（利用枠のステータス表示と共有メモ）。
/// どちらも、そのセッションだけのもので設定ファイルは書き換えない
pub fn launch_args(cfg: &crate::config::Config, kind: Kind, cwd: &Path, agent: &str, session: &str) -> String {
    let Ok(exe) = std::env::current_exe() else { return String::new() };
    let exe = exe.to_string_lossy().into_owned();
    let notes = cfg.shared_notes;
    match kind {
        Kind::Claude => {
            let mut out = String::new();
            let mut settings = serde_json::Map::new();
            if cfg.track_usage {
                if let Some(sl) = crate::usage::claude_statusline(cwd) {
                    settings.insert("statusLine".into(), sl);
                }
            }
            if notes {
                // 自分のメモを読み書きするだけの道具なので、毎回の確認は出さない
                settings.insert("permissions".into(), json!({"allow": [format!("mcp__{SERVER}__notes_read"), format!("mcp__{SERVER}__notes_write")]}));
                let mcp = json!({"mcpServers": {SERVER: {"type": "stdio", "command": exe, "args": mcp_args(cwd, agent, session)}}});
                out.push_str(&format!(" --mcp-config {}", shq(&mcp.to_string())));
            }
            if !settings.is_empty() {
                out.push_str(&format!(" --settings {}", shq(&Value::Object(settings).to_string())));
            }
            out
        }
        Kind::Codex if notes => {
            // -c で ~/.codex/config.toml の値をそのセッションだけ上書きする（値は TOML）
            let args = Value::from(mcp_args(cwd, agent, session)).to_string();
            format!(
                " -c {} -c {}",
                shq(&format!("mcp_servers.{SERVER}.command={}", Value::from(exe))),
                shq(&format!("mcp_servers.{SERVER}.args={args}"))
            )
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> PathBuf {
        std::env::temp_dir().join(format!("pa-shared-{}-{}.db", std::process::id(), crate::resume::new_uuid()))
    }

    #[test]
    fn notes_are_shared_within_a_project() {
        let d = db();
        write_at(&d, "/repo", "claude", "a", "status", "ログイン画面を直している").unwrap();
        write_at(&d, "/repo", "codex", "b", "decision", "セッションは cookie で持つ").unwrap();
        write_at(&d, "/other", "codex", "c", "status", "別の場所").unwrap();
        let v = read_at(&d, "/repo", 30, None, None).unwrap();
        assert_eq!(v.iter().map(|n| n.agent.as_str()).collect::<Vec<_>>(), ["claude", "codex"]);
        assert_eq!(read_at(&d, "/repo", 30, Some("decision"), None).unwrap().len(), 1);
        assert_eq!(read_at(&d, "/repo", 30, None, Some("cookie")).unwrap().len(), 1);
        assert_eq!(read_at(&d, "/repo", 1, None, None).unwrap()[0].agent, "codex", "新しいものから");
        let _ = std::fs::remove_file(&d);
    }

    #[test]
    fn speaks_mcp() {
        let d = db();
        let srv = Server { db: d.clone(), project: "/repo".into(), agent: "codex".into(), session: "s".into() };
        let init = srv.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(init["result"]["instructions"].as_str().unwrap().contains("notes_read"));
        assert!(srv.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        let list = srv.handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 2);
        let w = srv.handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "notes_write", "arguments": {"kind": "todo", "text": "テストを足す"}}})).unwrap();
        assert_eq!(w["result"]["isError"], false);
        let r = srv.handle(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "notes_read", "arguments": {}}})).unwrap();
        let t = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(t.contains("codex (s) todo: テストを足す"), "{t}");
        let bad = srv.handle(&json!({"jsonrpc": "2.0", "id": 5, "method": "nope"})).unwrap();
        assert_eq!(bad["error"]["code"], -32601);
        let _ = std::fs::remove_file(&d);
    }

    #[test]
    fn worktrees_share_the_project() {
        let base = std::env::temp_dir().join(format!("pa-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str], dir: &Path| assert!(std::process::Command::new("git").args(args).current_dir(dir).output().unwrap().status.success(), "{args:?}");
        git(&["init", "-q"], &repo);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "x"], &repo);
        git(&["worktree", "add", "-q", "../wt", "-b", "wt"], &repo);
        assert_eq!(project_key(&repo), project_key(&base.join("wt")));
        assert_eq!(project_key(&repo.join(".")), project_key(&repo));
        let _ = std::fs::remove_dir_all(&base);
    }
}
