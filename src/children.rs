//! ほかのエージェント（claude など）がコマンドで呼んだ codex を、codex 自身の記録から拾う
//!
//! codex は呼ばれるたびに ~/.codex/sessions/年/月/日/rollout-*.jsonl を書く。
//! 1行目の session_meta に cwd と source（"exec" ならコマンドで呼ばれたもの）が入っている。

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

#[derive(Debug, Clone)]
pub struct Child {
    pub id: String,
    pub file: PathBuf,
    pub cwd: PathBuf,
    pub started: SystemTime,
    pub prompt: String,
    pub last: String,
    pub status: Status,
    pub tools: usize,
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

/// 1つの記録を読んで、頼まれたこと・やったこと・終わったかを取り出す
pub fn read_steps(file: &Path) -> (Vec<Step>, bool) {
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
pub fn scan(root: &Path, since: SystemTime, cache: &mut HashMap<PathBuf, (SystemTime, Option<Child>)>) -> Vec<Child> {
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
                let (steps, done) = read_steps(&p);
                let fresh = SystemTime::now().duration_since(mtime).unwrap_or_default() < Duration::from_secs(90);
                Child {
                    id: m.id,
                    file: p.clone(),
                    cwd: m.cwd,
                    started: m.started,
                    prompt: steps.iter().find(|s| s.kind == StepKind::Prompt).map(|s| s.text.clone()).unwrap_or_default(),
                    last: steps.iter().rev().find(|s| matches!(s.kind, StepKind::Say | StepKind::Answer)).map(|s| s.text.clone()).unwrap_or_default(),
                    status: if done { Status::Done } else if fresh { Status::Running } else { Status::Stopped },
                    tools: steps.iter().filter(|s| s.kind == StepKind::Tool).count(),
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

/// 裏で2秒ごとに集め直す。since は動いているセッションのうち一番古い開始時刻
pub fn spawn_watcher(since: Arc<Mutex<Option<SystemTime>>>, out: Arc<Mutex<Vec<Child>>>, wake: Wake) {
    std::thread::spawn(move || {
        let mut cache = HashMap::new();
        let root = codex_root();
        loop {
            let s = *since.lock().unwrap();
            if let Some(s) = s {
                let found = scan(&root, s, &mut cache);
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
        let found = scan(&root, since, &mut cache);
        assert_eq!(found.len(), 1);
        let c = &found[0];
        assert_eq!(c.id, "c1");
        assert_eq!(c.prompt, "テストを直して");
        assert_eq!(c.last, "直しました");
        assert_eq!(c.status, Status::Done);
        assert_eq!(c.tools, 1);
        let (steps, done) = read_steps(&c.file);
        assert!(done);
        let kinds: Vec<_> = steps.iter().map(|s| s.kind.clone()).collect();
        assert_eq!(kinds, vec![StepKind::Prompt, StepKind::Tool, StepKind::Output, StepKind::Answer]);
        // 始める前のものは拾わない
        assert!(scan(&root, SystemTime::now() + Duration::from_secs(60), &mut HashMap::new()).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
