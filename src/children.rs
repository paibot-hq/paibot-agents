//! エージェントがほかのエージェントを呼んだものを、呼ばれた側の記録から拾う
//!
//! - codex: 呼ばれるたびに ~/.codex/sessions/年/月/日/rollout-*.jsonl。1行目に cwd と source（"exec"）
//! - claude -p: ~/.claude/projects/<フォルダ>/<ID>.jsonl。entrypoint が "sdk-cli"
//! - claude の子エージェント: ~/.claude/projects/<フォルダ>/<親の会話ID>/subagents/agent-*.jsonl と .meta.json

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::session::Wake;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Running,
    Done,
    /// 終わりの印がないまま止まった
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// コマンドで呼ばれた codex
    Codex,
    /// コマンドで呼ばれた claude（claude -p）
    Claude,
    /// claude が自分の中で立てた子エージェント
    ClaudeSub,
}

#[derive(Debug, Clone)]
pub struct Child {
    pub id: String,
    pub kind: Kind,
    /// 一覧に出す名前（codex / claude / 子エージェントの種類）
    pub label: String,
    /// 子エージェントなら親の会話の ID（そのセッションに確実に付ける）
    pub parent: Option<String>,
    /// 親が子エージェントや呼ばれた claude なら、その ID（記録に書いてあるので確か）
    pub parent_agent: Option<String>,
    pub file: PathBuf,
    pub cwd: PathBuf,
    pub started: SystemTime,
    /// 記録が最後に書かれた時刻（終わったものはだいたい終わった時刻）
    pub updated: SystemTime,
    pub prompt: String,
    pub last: String,
    pub status: Status,
    pub tools: usize,
    /// 実行したコマンド（ほかのエージェントを呼んだかどうかの手がかり）
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepKind {
    Prompt,
    Say,
    Tool,
    Output,
    Answer,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub kind: StepKind,
    pub text: String,
}

pub fn codex_root() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex")).join("sessions")
}

fn sorted_dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// 新しい日付のフォルダ（年/月/日）を n 個
fn recent_days(root: &Path, n: usize) -> Vec<PathBuf> {
    let mut days = Vec::new();
    for y in sorted_dirs(root).into_iter().rev().take(2) {
        for m in sorted_dirs(&y).into_iter().rev().take(2) {
            for d in sorted_dirs(&m).into_iter().rev() {
                days.push(d);
            }
        }
    }
    days.sort();
    days.into_iter().rev().take(n).collect()
}

fn text_of(content: &Value) -> String {
    content
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn clip(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if t.len() < s.len() { format!("{t}…") } else { t }
}

/// 記録を読んで、頼まれたこと・やったこと・終わったかを取り出す
pub fn read_steps(kind: Kind, file: &Path) -> (Vec<Step>, bool) {
    match kind {
        Kind::Codex => read_codex_steps(file),
        Kind::Claude | Kind::ClaudeSub => read_claude_steps(file),
    }
}

fn read_codex_steps(file: &Path) -> (Vec<Step>, bool) {
    let mut steps = Vec::new();
    let mut done = false;
    let Ok(f) = std::fs::File::open(file) else { return (steps, done) };
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let p = v.get("payload").cloned().unwrap_or(Value::Null);
        let pt = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match (ty, pt) {
            ("response_item", "message") => {
                let role = p.get("role").and_then(|r| r.as_str()).unwrap_or("");
                let text = text_of(p.get("content").unwrap_or(&Value::Null));
                let text = text.trim();
                if text.is_empty() || text.starts_with('<') {
                    continue; // 環境の説明や指示の前置きは出さない
                }
                match role {
                    "user" => steps.push(Step { kind: StepKind::Prompt, text: text.to_string() }),
                    "assistant" => steps.push(Step { kind: StepKind::Say, text: text.to_string() }),
                    _ => {}
                }
            }
            ("response_item", "custom_tool_call") | ("response_item", "function_call") => {
                let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let input = p.get("input").or_else(|| p.get("arguments")).map(|i| i.as_str().map(str::to_string).unwrap_or_else(|| i.to_string())).unwrap_or_default();
                steps.push(Step { kind: StepKind::Tool, text: format!("{name}  {}", clip(input.trim(), 1200)) });
            }
            ("response_item", "custom_tool_call_output") | ("response_item", "function_call_output") => {
                let out = p.get("output").map(|o| if o.is_array() { text_of(o) } else { o.as_str().map(str::to_string).unwrap_or_else(|| o.to_string()) }).unwrap_or_default();
                if !out.trim().is_empty() {
                    steps.push(Step { kind: StepKind::Output, text: clip(out.trim(), 1500) });
                }
            }
            ("event_msg", "task_complete") => {
                done = true;
                if let Some(m) = p.get("last_agent_message").and_then(|m| m.as_str()) {
                    // 最後の発言と同じなら「答え」に置き換える
                    if steps.last().is_some_and(|s| s.kind == StepKind::Say && s.text.trim() == m.trim()) {
                        steps.pop();
                    }
                    steps.push(Step { kind: StepKind::Answer, text: m.trim().to_string() });
                }
            }
            _ => {}
        }
    }
    (steps, done)
}

#[derive(Clone)]
struct Meta {
    id: String,
    cwd: PathBuf,
    started: SystemTime,
    called: bool,
}

fn read_meta(file: &Path) -> Option<Meta> {
    let f = std::fs::File::open(file).ok()?;
    let mut first = String::new();
    BufReader::new(f).read_line(&mut first).ok()?;
    let v: Value = serde_json::from_str(&first).ok()?;
    let p = v.get("payload")?;
    let source = p.get("source").and_then(|s| s.as_str()).unwrap_or("");
    let originator = p.get("originator").and_then(|s| s.as_str()).unwrap_or("");
    let started = std::fs::metadata(file).ok().and_then(|m| m.created().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
    Some(Meta {
        id: p.get("id")?.as_str()?.to_string(),
        cwd: PathBuf::from(p.get("cwd")?.as_str()?),
        started,
        // codex exec をコマンドで呼んだもの（VSCode の codex 拡張が内部で使うものは除く）
        called: source == "exec" && originator == "codex_exec",
    })
}

/// since より後に始まった「呼ばれた codex」を集める
pub fn scan_codex(root: &Path, since: SystemTime, cache: &mut HashMap<PathBuf, (SystemTime, Option<Child>)>) -> Vec<Child> {
    let mut out = Vec::new();
    for day in recent_days(root, 2) {
        let Ok(rd) = std::fs::read_dir(&day) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            let created = md.created().or_else(|_| md.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            if created < since {
                continue;
            }
            let mtime = md.modified().unwrap_or(created);
            if let Some((t, c)) = cache.get(&p) {
                if *t == mtime {
                    out.extend(c.clone());
                    continue;
                }
            }
            let child = read_meta(&p).filter(|m| m.called).map(|m| {
                let (steps, done) = read_codex_steps(&p);
                let fresh = SystemTime::now().duration_since(mtime).unwrap_or_default() < Duration::from_secs(90);
                Child {
                    id: m.id,
                    kind: Kind::Codex,
                    label: "codex".into(),
                    parent: None,
                    parent_agent: None,
                    file: p.clone(),
                    cwd: m.cwd,
                    started: m.started,
                    updated: mtime,
                    prompt: steps.iter().find(|s| s.kind == StepKind::Prompt).map(|s| s.text.clone()).unwrap_or_default(),
                    last: steps.iter().rev().find(|s| matches!(s.kind, StepKind::Say | StepKind::Answer)).map(|s| s.text.clone()).unwrap_or_default(),
                    status: if done { Status::Done } else if fresh { Status::Running } else { Status::Stopped },
                    tools: steps.iter().filter(|s| s.kind == StepKind::Tool).count(),
                    commands: tool_texts(&steps),
                }
            });
            // 動いている間は、止まったかどうかを時間で判定し直すので覚えない
            let keep = child.as_ref().is_none_or(|c| c.status != Status::Running);
            if keep {
                cache.insert(p.clone(), (mtime, child.clone()));
            }
            out.extend(child);
        }
    }
    out.sort_by_key(|c| c.started);
    out
}

pub fn claude_root() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude")).join("projects")
}

/// claude がフォルダごとに記録を置く場所の名前（英数字以外は - になる）
pub fn claude_project_dir(root: &Path, cwd: &Path) -> PathBuf {
    let name: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    root.join(name)
}

fn content_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| match x.get("type").and_then(|t| t.as_str()) {
                Some("text") => x.get("text").and_then(|t| t.as_str()).map(str::to_string),
                Some("tool_result") => Some(content_text(x.get("content").unwrap_or(&Value::Null))),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn read_claude_steps(file: &Path) -> (Vec<Step>, bool) {
    let mut steps = Vec::new();
    let mut last_stop: Option<String> = None;
    let Ok(f) = std::fs::File::open(file) else { return (steps, false) };
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                if let Value::Array(items) = &content {
                    for it in items {
                        if it.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                            let t = content_text(it.get("content").unwrap_or(&Value::Null));
                            if !t.trim().is_empty() {
                                steps.push(Step { kind: StepKind::Output, text: clip(t.trim(), 1500) });
                            }
                        }
                    }
                }
                let t = match &content {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a.iter().filter(|x| x.get("type").and_then(|t| t.as_str()) == Some("text")).filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"),
                    _ => String::new(),
                };
                let t = t.trim();
                if !t.is_empty() && !t.starts_with('<') {
                    steps.push(Step { kind: StepKind::Prompt, text: t.to_string() });
                }
            }
            Some("assistant") => {
                last_stop = msg.get("stop_reason").and_then(|r| r.as_str()).map(str::to_string);
                if let Value::Array(items) = &content {
                    for it in items {
                        match it.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                let t = it.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
                                if !t.is_empty() {
                                    steps.push(Step { kind: StepKind::Say, text: t });
                                }
                            }
                            Some("tool_use") => {
                                let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                                // 子エージェントが親に返す報告は「答え」として出す
                                if name == "SubagentHandback" {
                                    if let Some(m) = it.get("input").and_then(|i| i.get("message")).and_then(|m| m.as_str()) {
                                        steps.push(Step { kind: StepKind::Answer, text: m.trim().to_string() });
                                        continue;
                                    }
                                }
                                let input = it.get("input").map(|i| {
                                    // よく使う道具は中身の要点だけ
                                    ["command", "file_path", "pattern", "url", "description", "prompt"]
                                        .iter()
                                        .find_map(|k| i.get(*k).and_then(|x| x.as_str()).map(str::to_string))
                                        .unwrap_or_else(|| i.to_string())
                                });
                                steps.push(Step { kind: StepKind::Tool, text: format!("{name}  {}", clip(input.unwrap_or_default().trim(), 1200)) });
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let done = last_stop.as_deref() == Some("end_turn");
    if done {
        if let Some(i) = steps.iter().rposition(|s| s.kind == StepKind::Say) {
            if i == steps.len() - 1 {
                steps[i].kind = StepKind::Answer;
            }
        }
    }
    (steps, done)
}

fn first_entrypoint(file: &Path) -> Option<String> {
    let f = std::fs::File::open(file).ok()?;
    for line in BufReader::new(f).lines().map_while(Result::ok).take(40) {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(e) = v.get("entrypoint").and_then(|e| e.as_str()) {
                return Some(e.to_string());
            }
        }
    }
    None
}

fn summarize(kind: Kind, label: String, id: String, parent: Option<String>, file: &Path, cwd: &Path, started: SystemTime, mtime: SystemTime, finished: Option<bool>, desc: Option<String>) -> Child {
    let (steps, done) = read_steps(kind, file);
    let done = finished.unwrap_or(done);
    let fresh = SystemTime::now().duration_since(mtime).unwrap_or_default() < Duration::from_secs(90);
    let prompt = desc.filter(|d| !d.is_empty()).or_else(|| steps.iter().find(|s| s.kind == StepKind::Prompt).map(|s| s.text.clone())).unwrap_or_default();
    Child {
        id,
        kind,
        label,
        parent,
        parent_agent: None,
        file: file.to_path_buf(),
        cwd: cwd.to_path_buf(),
        started,
        updated: mtime,
        prompt,
        last: steps.iter().rev().find(|s| matches!(s.kind, StepKind::Say | StepKind::Answer)).map(|s| s.text.clone()).unwrap_or_default(),
        status: if done { Status::Done } else if fresh { Status::Running } else { Status::Stopped },
        tools: steps.iter().filter(|s| s.kind == StepKind::Tool).count(),
        commands: tool_texts(&steps),
    }
}

type Cache = HashMap<PathBuf, (SystemTime, Option<Child>)>;

fn cached(cache: &mut Cache, p: &Path, mtime: SystemTime, make: impl FnOnce() -> Option<Child>) -> Option<Child> {
    if let Some((t, c)) = cache.get(p) {
        if *t == mtime {
            return c.clone();
        }
    }
    let c = make();
    if c.as_ref().is_none_or(|c| c.status != Status::Running) {
        cache.insert(p.to_path_buf(), (mtime, c.clone()));
    }
    c
}

/// claude -p で呼ばれた claude と、claude の子エージェントを集める
pub fn scan_claude(root: &Path, watch: &Watch, cache: &mut Cache) -> Vec<Child> {
    let Some(since) = watch.since else { return Vec::new() };
    let mut out = Vec::new();
    let mut seen_dirs = std::collections::HashSet::new();
    for cwd in &watch.dirs {
        let dir = claude_project_dir(root, cwd);
        if !seen_dirs.insert(dir.clone()) {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            let created = md.created().or_else(|_| md.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            if created < since {
                continue;
            }
            let mtime = md.modified().unwrap_or(created);
            let id = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if watch.claude_ids.iter().any(|(sid, _)| *sid == id) {
                continue; // paispace が開いた claude そのもの
            }
            let c = cached(cache, &p, mtime, || {
                (first_entrypoint(&p).as_deref() == Some("sdk-cli"))
                    .then(|| summarize(Kind::Claude, "claude".into(), id.clone(), None, &p, cwd, created, mtime, None, None))
            });
            out.extend(c);
        }
    }
    // 子エージェント: 親の会話 ID のフォルダの中
    for (sid, cwd) in &watch.claude_ids {
        out.extend(scan_subagents(root, cwd, sid, Some(sid.clone()), None, cache));
    }
    // 呼ばれた claude がさらに立てた子エージェント
    let called: Vec<(String, PathBuf)> = out.iter().filter(|c| c.kind == Kind::Claude).map(|c| (c.id.clone(), c.cwd.clone())).collect();
    for (cid, cwd) in called {
        out.extend(scan_subagents(root, &cwd, &cid, None, Some(cid.clone()), cache));
    }
    out
}

/// 会話 conv の子エージェントを集める。owner はセッションの会話 ID、default_parent は親が呼ばれた claude のとき
fn scan_subagents(root: &Path, cwd: &Path, conv: &str, owner: Option<String>, default_parent: Option<String>, cache: &mut Cache) -> Vec<Child> {
    let mut out = Vec::new();
    let dir = claude_project_dir(root, cwd).join(conv).join("subagents");
    let Ok(rd) = std::fs::read_dir(&dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let created = md.created().or_else(|_| md.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        let meta_path = p.with_extension("meta.json");
        let meta_mtime = std::fs::metadata(&meta_path).and_then(|m| m.modified()).ok();
        let mtime = md.modified().unwrap_or(created).max(meta_mtime.unwrap_or(SystemTime::UNIX_EPOCH));
        let id = p.file_stem().map(|s| s.to_string_lossy().trim_start_matches("agent-").to_string()).unwrap_or_default();
        let c = cached(cache, &p, mtime, || {
            let meta: Value = std::fs::read_to_string(&meta_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
            let label = meta.get("agentType").and_then(|a| a.as_str()).unwrap_or("subagent").to_string();
            let finished = meta.get("finished").and_then(|f| f.as_bool());
            let desc = meta.get("description").and_then(|d| d.as_str()).map(str::to_string);
            let mut c = summarize(Kind::ClaudeSub, label, id.clone(), owner.clone(), &p, cwd, created, mtime, finished, desc);
            // 子エージェントがさらに立てた子は、親の子エージェントの ID が書いてある
            c.parent_agent = meta.get("parentAgentId").and_then(|a| a.as_str()).map(str::to_string).or_else(|| default_parent.clone());
            Some(c)
        });
        out.extend(c);
    }
    out
}

/// 呼び出しのツリーの1つ
#[derive(Debug, Clone)]
pub struct Node {
    pub child: Child,
    /// 親を推測でつないだ（記録に書いていない）
    pub guessed: bool,
    pub kids: Vec<Node>,
}

/// ツリーを作るためのセッションの情報
pub struct SessionRef {
    pub conv_id: Option<String>,
    pub roots: Vec<PathBuf>,
    pub started: SystemTime,
    /// そのセッションが実行したコマンド（claude なら記録から読める。読めないものは None）
    pub commands: Option<Vec<String>>,
}

fn tool_texts(steps: &[Step]) -> Vec<String> {
    steps.iter().filter(|s| s.kind == StepKind::Tool).map(|s| s.text.chars().take(600).collect()).collect()
}

/// commands が c を呼んだ手がかりの強さ。2: 頼んだ文そのものがある / 1: エージェント名だけ / 0: なし
fn evidence(commands: &[String], c: &Child) -> u8 {
    let prompt: String = c.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let snippet: String = prompt.chars().take(16).collect();
    if snippet.chars().count() >= 6 && commands.iter().any(|cmd| cmd.split_whitespace().collect::<Vec<_>>().join(" ").contains(&snippet)) {
        return 2;
    }
    let name = match c.kind {
        Kind::Codex => "codex",
        Kind::Claude | Kind::ClaudeSub => "claude",
    };
    if commands.iter().any(|cmd| cmd.contains(name)) { 1 } else { 0 }
}

/// 子をセッションごとのツリーにする。
/// 記録に親が書いてあればそれに。なければ、同じフォルダで動いていた候補のうち、
/// 実行したコマンドに「その子を呼んだ手がかり」があるものの下に付ける
pub fn build_tree(all: &[Child], sessions: &[SessionRef]) -> Vec<Vec<Node>> {
    #[derive(Clone)]
    enum Up {
        Session(usize),
        Child(String),
    }
    let ids: std::collections::HashSet<&str> = all.iter().map(|c| c.id.as_str()).collect();
    let session_of_conv = |id: &str| sessions.iter().position(|s| s.conv_id.as_deref() == Some(id));
    let mut up: HashMap<String, (Up, bool)> = HashMap::new();
    for c in all {
        if let Some(pid) = c.parent_agent.as_deref().filter(|p| ids.contains(p) && *p != c.id) {
            up.insert(c.id.clone(), (Up::Child(pid.to_string()), false));
            continue;
        }
        if let Some(i) = c.parent.as_deref().and_then(session_of_conv) {
            up.insert(c.id.clone(), (Up::Session(i), false));
            continue;
        }
        if c.parent.is_some() {
            continue; // 親のセッションがもういない
        }
        // その子が始まったとき、同じフォルダで動いていた候補のうち、実際にその子を呼んだ手がかりがあるもの。
        // 頼んだ文そのものがコマンドにあれば確か（実線）、エージェント名だけなら推測（点線）
        let grace = Duration::from_secs(2);
        let mut best: Option<(u8, SystemTime, Up)> = None;
        let mut consider = |score: u8, started: SystemTime, up: Up| {
            if best.as_ref().is_none_or(|(s, t, _)| score > *s || (score == *s && started > *t)) {
                best = Some((score, started, up));
            }
        };
        for k in all.iter().filter(|k| k.id != c.id && k.started < c.started && c.cwd.starts_with(&k.cwd)) {
            if k.status == Status::Running || k.updated + grace >= c.started {
                let e = evidence(&k.commands, c);
                if e > 0 {
                    consider(e, k.started, Up::Child(k.id.clone()));
                }
            }
        }
        for (i, s) in sessions.iter().enumerate() {
            if s.conv_id.as_deref() == Some(c.id.as_str()) || s.started > c.started + grace || !s.roots.iter().any(|r| c.cwd.starts_with(r)) {
                continue;
            }
            match &s.commands {
                // 記録が読めるセッションは、手がかりがあるときだけ
                Some(cmds) => {
                    let e = evidence(cmds, c);
                    if e > 0 {
                        consider(e, s.started, Up::Session(i));
                    }
                }
                // 読めないセッション（シェルなど）は、手がかりなしの候補
                None => consider(0, s.started, Up::Session(i)),
            }
        }
        if let Some((score, _, u)) = best {
            let guessed = score < 2 && matches!(u, Up::Child(_));
            up.insert(c.id.clone(), (u, guessed));
        }
    }
    fn grow(id: &str, all: &[Child], up: &HashMap<String, (Up, bool)>, depth: usize) -> Vec<Node> {
        if depth > 16 {
            return Vec::new(); // 念のため（記録が壊れて輪になっていても止まる）
        }
        let mut v: Vec<Node> = all
            .iter()
            .filter(|c| matches!(up.get(&c.id), Some((Up::Child(p), _)) if p == id))
            .map(|c| Node { child: c.clone(), guessed: up[&c.id].1, kids: grow(&c.id, all, up, depth + 1) })
            .collect();
        v.sort_by_key(|n| n.child.started);
        v
    }
    let mut out = vec![Vec::new(); sessions.len()];
    for c in all {
        if let Some((Up::Session(i), g)) = up.get(&c.id) {
            out[*i].push(Node { child: c.clone(), guessed: *g, kids: grow(&c.id, all, &up, 1) });
        }
    }
    for v in &mut out {
        v.sort_by_key(|n| n.child.started);
    }
    out
}

/// ツリーを上から順に並べる（深さつき）
pub fn flatten(nodes: &[Node]) -> Vec<(usize, &Node)> {
    fn go<'a>(n: &'a [Node], d: usize, out: &mut Vec<(usize, &'a Node)>) {
        for x in n {
            out.push((d, x));
            go(&x.kids, d + 1, out);
        }
    }
    let mut out = Vec::new();
    go(nodes, 0, &mut out);
    out
}

/// 何を見張るか（app が更新する）
#[derive(Default, Clone)]
pub struct Watch {
    /// 動いているセッションのうち一番古い開始時刻
    pub since: Option<SystemTime>,
    /// 見張るフォルダ（セッションのフォルダ）
    pub dirs: Vec<PathBuf>,
    /// paispace が開いた claude の会話 ID とそのフォルダ（子エージェントを探す）
    pub claude_ids: Vec<(String, PathBuf)>,
}

/// 裏で2秒ごとに集め直す
/// paispace が開いた claude のセッションが実行したコマンド（会話 ID → コマンド）
pub type SessionCommands = Arc<Mutex<HashMap<String, Vec<String>>>>;

pub fn spawn_watcher(watch: Arc<Mutex<Watch>>, out: Arc<Mutex<Vec<Child>>>, session_cmds: SessionCommands, wake: Wake) {
    std::thread::spawn(move || {
        let mut codex_cache = HashMap::new();
        let mut claude_cache = HashMap::new();
        let mut cmd_cache: HashMap<PathBuf, (SystemTime, Vec<String>)> = HashMap::new();
        let croot = codex_root();
        let claude = claude_root();
        loop {
            let w = watch.lock().unwrap().clone();
            if let Some(since) = w.since {
                let mut found = scan_codex(&croot, since, &mut codex_cache);
                found.extend(scan_claude(&claude, &w, &mut claude_cache));
                found.sort_by_key(|c| c.started);
                // セッション自身のコマンド（どの子を呼んだかの手がかり）
                let mut cmds = HashMap::new();
                for (sid, cwd) in &w.claude_ids {
                    let f = claude_project_dir(&claude, cwd).join(format!("{sid}.jsonl"));
                    let Some(m) = std::fs::metadata(&f).and_then(|m| m.modified()).ok() else { continue };
                    let list = match cmd_cache.get(&f) {
                        Some((t, l)) if *t == m => l.clone(),
                        _ => {
                            let l = tool_texts(&read_claude_steps(&f).0);
                            cmd_cache.insert(f.clone(), (m, l.clone()));
                            l
                        }
                    };
                    cmds.insert(sid.clone(), list);
                }
                *session_cmds.lock().unwrap() = cmds;
                let changed = {
                    let mut cur = out.lock().unwrap();
                    let same = cur.len() == found.len()
                        && cur.iter().zip(&found).all(|(a, b)| a.id == b.id && a.status == b.status && a.last == b.last && a.tools == b.tools);
                    if !same {
                        *cur = found;
                    }
                    !same
                };
                if changed {
                    wake();
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_called_codex() {
        let root = std::env::temp_dir().join(format!("paispace-children-{}", std::process::id()));
        let day = root.join("2026/11/10");
        std::fs::create_dir_all(&day).unwrap();
        let since = SystemTime::now() - Duration::from_secs(5);
        let lines = [
            r#"{"type":"session_meta","payload":{"id":"c1","cwd":"/w/app","source":"exec","originator":"codex_exec"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"テストを直して"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"cargo test"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"ok"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"直しました"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"直しました"}}"#,
        ];
        std::fs::write(day.join("rollout-a.jsonl"), lines.join("\n")).unwrap();
        // VSCode の拡張が内部で使ったものと、自分で開いた codex は数えない
        std::fs::write(day.join("rollout-b.jsonl"), r#"{"type":"session_meta","payload":{"id":"v","cwd":"/w/app","source":"exec","originator":"codex_vscode"}}"#).unwrap();
        std::fs::write(day.join("rollout-c.jsonl"), r#"{"type":"session_meta","payload":{"id":"i","cwd":"/w/app","source":"cli","originator":"codex_cli_rs"}}"#).unwrap();

        let mut cache = HashMap::new();
        let found = scan_codex(&root, since, &mut cache);
        assert_eq!(found.len(), 1);
        let c = &found[0];
        assert_eq!(c.id, "c1");
        assert_eq!(c.prompt, "テストを直して");
        assert_eq!(c.last, "直しました");
        assert_eq!(c.status, Status::Done);
        assert_eq!(c.tools, 1);
        let (steps, done) = read_steps(Kind::Codex, &c.file);
        assert!(done);
        let kinds: Vec<_> = steps.iter().map(|s| s.kind.clone()).collect();
        assert_eq!(kinds, vec![StepKind::Prompt, StepKind::Tool, StepKind::Output, StepKind::Answer]);
        // 始める前のものは拾わない
        assert!(scan_codex(&root, SystemTime::now() + Duration::from_secs(60), &mut HashMap::new()).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn child(id: &str, cwd: &str, start: u64, end: u64, status: Status) -> Child {
        let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + s);
        Child {
            id: id.into(),
            kind: Kind::Codex,
            label: id.into(),
            parent: None,
            parent_agent: None,
            file: PathBuf::new(),
            cwd: cwd.into(),
            started: t(start),
            updated: t(end),
            prompt: String::new(),
            last: String::new(),
            status,
            tools: 0,
            commands: Vec::new(),
        }
    }

    #[test]
    fn builds_call_tree() {
        let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + s);
        let sessions = vec![
            // claude のセッション（記録が読める。子エージェントしか立てていない）
            SessionRef { conv_id: Some("conv-a".into()), roots: vec!["/w".into()], started: t(0), commands: Some(vec!["Agent  数えて".into()]) },
            // シェルのセッション（記録が読めない）
            SessionRef { conv_id: None, roots: vec!["/w".into()], started: t(1), commands: None },
        ];
        // a: 子エージェント s1 → その子 s2（記録に親が書いてある）
        let mut s1 = child("s1", "/w", 10, 50, Status::Done);
        s1.kind = Kind::ClaudeSub;
        s1.parent = Some("conv-a".into());
        s1.commands = vec!["Bash  ls app".into()];
        let mut s2 = child("s2", "/w", 20, 30, Status::Done);
        s2.kind = Kind::ClaudeSub;
        s2.parent = Some("conv-a".into());
        s2.parent_agent = Some("s1".into());
        // シェルから claude -p（y）。y が codex exec（z）を、頼んだ文ごと呼んだ
        let mut y = child("y", "/w", 12, 40, Status::Done);
        y.kind = Kind::Claude;
        y.prompt = "Bash で codex を呼んで".into();
        y.commands = vec!["Bash  codex exec --skip-git-repo-check \"web フォルダ直下の項目数を数えて\"".into()];
        let mut z = child("z", "/w", 25, 30, Status::Done);
        z.prompt = "web フォルダ直下の項目数を数えて".into();
        // y の中で名前だけ出てくる codex（推測）
        let mut y2 = child("y2", "/w", 26, 45, Status::Done);
        y2.kind = Kind::Claude;
        y2.commands = vec!["Bash  codex exec --help".into()];
        let mut z2 = child("z2", "/w", 30, 35, Status::Done);
        z2.prompt = "まったく別の頼みごと".into();
        let tree = build_tree(&[s1, s2, y, z, y2, z2], &sessions);

        // claude のセッションには子エージェントだけ。s1 が同じ時間に動いていても z は付かない
        assert_eq!(tree[0].len(), 1);
        assert_eq!(tree[0][0].child.id, "s1");
        assert_eq!(tree[0][0].kids.len(), 1);
        assert_eq!(tree[0][0].kids[0].child.id, "s2");
        assert!(!tree[0][0].kids[0].guessed);

        // シェルのセッションに y と y2。z は頼んだ文があるので y の下（実線）、z2 は名前だけなので y2 の下（点線）
        let ids: Vec<&str> = tree[1].iter().map(|n| n.child.id.as_str()).collect();
        assert_eq!(ids, vec!["y", "y2"]);
        assert_eq!(tree[1][0].kids[0].child.id, "z");
        assert!(!tree[1][0].kids[0].guessed);
        assert_eq!(tree[1][1].kids[0].child.id, "z2");
        assert!(tree[1][1].kids[0].guessed);
        let flat: Vec<(usize, &str)> = flatten(&tree[1]).into_iter().map(|(d, n)| (d, n.child.id.as_str())).collect();
        assert_eq!(flat, vec![(0, "y"), (1, "z"), (0, "y2"), (1, "z2")]);
    }

    #[test]
    fn reads_called_claude_and_subagents() {
        let root = std::env::temp_dir().join(format!("paispace-claude-{}", std::process::id()));
        let cwd = PathBuf::from("/w/my.app");
        let dir = claude_project_dir(&root, &cwd);
        assert!(dir.ends_with("-w-my-app"));
        std::fs::create_dir_all(dir.join("parent-1/subagents")).unwrap();
        let since = SystemTime::now() - Duration::from_secs(5);
        let called = [
            r#"{"type":"session-owner","sessionId":"x"}"#,
            r#"{"type":"user","entrypoint":"sdk-cli","message":{"role":"user","content":"翻訳して"}}"#,
            r#"{"type":"assistant","entrypoint":"sdk-cli","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/w/a.md"}}],"stop_reason":"tool_use"}}"#,
            r#"{"type":"user","entrypoint":"sdk-cli","message":{"role":"user","content":[{"type":"tool_result","content":"hello"}]}}"#,
            r#"{"type":"assistant","entrypoint":"sdk-cli","message":{"role":"assistant","content":[{"type":"text","text":"こんにちは"}],"stop_reason":"end_turn"}}"#,
        ];
        std::fs::write(dir.join("called-1.jsonl"), called.join("\n")).unwrap();
        // 普通に開いた claude（cli）と、paispace が開いた claude そのものは数えない
        std::fs::write(dir.join("mine.jsonl"), r#"{"type":"user","entrypoint":"cli","message":{"role":"user","content":"x"}}"#).unwrap();
        std::fs::write(dir.join("parent-1.jsonl"), r#"{"type":"user","entrypoint":"sdk-cli","message":{"role":"user","content":"x"}}"#).unwrap();
        std::fs::write(dir.join("parent-1/subagents/agent-s1.jsonl"), r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"調べて"}}"#).unwrap();
        std::fs::write(dir.join("parent-1/subagents/agent-s1.meta.json"), r#"{"agentType":"Explore","description":"API を調べる","finished":true}"#).unwrap();

        let watch = Watch { since: Some(since), dirs: vec![cwd.clone()], claude_ids: vec![("parent-1".into(), cwd.clone())] };
        let mut found = scan_claude(&root, &watch, &mut HashMap::new());
        found.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(found.len(), 2, "{found:?}");
        let c = &found[0];
        assert_eq!((c.id.as_str(), c.kind, c.status), ("called-1", Kind::Claude, Status::Done));
        assert_eq!(c.prompt, "翻訳して");
        assert_eq!(c.last, "こんにちは");
        assert_eq!(c.tools, 1);
        let (steps, _) = read_steps(Kind::Claude, &c.file);
        let kinds: Vec<_> = steps.iter().map(|s| s.kind.clone()).collect();
        assert_eq!(kinds, vec![StepKind::Prompt, StepKind::Tool, StepKind::Output, StepKind::Answer]);
        let sub = &found[1];
        assert_eq!((sub.id.as_str(), sub.kind, sub.label.as_str(), sub.status), ("s1", Kind::ClaudeSub, "Explore", Status::Done));
        assert_eq!(sub.prompt, "API を調べる");
        assert_eq!(sub.parent.as_deref(), Some("parent-1"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
