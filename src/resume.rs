//! セッションを覚えておき、次に開いたときに会話の続きから再開する
//!
//! - claude: 始めるときに --session-id で ID を決め、claude --resume <ID> で戻る
//! - codex: 始めたあとに ~/.codex/sessions からそのセッションの ID を探し、codex resume <ID> で戻る
//! - そのほか: 同じフォルダで新しく開き直す

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Claude,
    Codex,
    Other,
}

pub fn kind(cmd: &str) -> Kind {
    let first = cmd.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    match base {
        "claude" => Kind::Claude,
        "codex" => Kind::Codex,
        _ => Kind::Other,
    }
}

fn quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

/// ワークスペースの残りのフォルダもエージェントに見せる指定を足す
pub fn add_dirs(cmd: &str, dirs: &[PathBuf]) -> String {
    if dirs.is_empty() {
        return cmd.to_string();
    }
    let first = cmd.split_whitespace().next().unwrap_or("");
    let flag = match first.rsplit('/').next().unwrap_or(first) {
        "claude" | "codex" => "--add-dir",
        "gemini" => "--include-directories",
        _ => return cmd.to_string(),
    };
    let mut out = cmd.to_string();
    for d in dirs {
        out.push_str(&format!(" {flag} {}", quote(d)));
    }
    out
}

/// ランダムな UUID（v4）
pub fn new_uuid() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// 新しく始めるときのコマンドと、そのとき決まる ID
pub fn start_cmd(cmd: &str) -> (String, Option<String>) {
    match kind(cmd) {
        Kind::Claude => {
            let id = new_uuid();
            (format!("{cmd} --session-id {id}"), Some(id))
        }
        _ => (cmd.to_string(), None),
    }
}

/// 続きから再開するコマンド。続きを持てないものは None（新しく開き直す）
pub fn resume_cmd(cmd: &str, id: &str) -> Option<String> {
    match kind(cmd) {
        Kind::Claude => Some(format!("{cmd} --resume {id}")),
        Kind::Codex => Some(format!("{cmd} resume {id}")),
        Kind::Other => None,
    }
}

#[derive(Deserialize)]
struct CodexLine {
    payload: CodexMeta,
}

#[derive(Deserialize)]
struct CodexMeta {
    id: String,
    cwd: String,
}

/// そのフォルダで started より後に始まった codex の会話の ID（ほかのセッションが使っているものは除く）
pub fn find_codex_id(cwd: &Path, started: SystemTime, taken: &[String]) -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let root = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex")).join("sessions");
    find_codex_id_in(&root, cwd, started, taken)
}

fn find_codex_id_in(root: &Path, cwd: &Path, started: SystemTime, taken: &[String]) -> Option<String> {
    // 日付のフォルダ（年/月/日）のうち新しい2日分だけ見る
    let mut days: Vec<PathBuf> = Vec::new();
    for y in sorted_dirs(root).into_iter().rev().take(2) {
        for m in sorted_dirs(&y).into_iter().rev().take(2) {
            days.extend(sorted_dirs(&m).into_iter().rev().take(2));
        }
    }
    let since = started.checked_sub(Duration::from_secs(5)).unwrap_or(started);
    let mut best: Option<(SystemTime, String)> = None;
    for d in days {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let meta = e.metadata().ok();
            let created = meta.as_ref().and_then(|m| m.created().ok().or_else(|| m.modified().ok()));
            let Some(created) = created.filter(|c| *c >= since) else { continue };
            let Ok(f) = std::fs::File::open(&p) else { continue };
            let mut first = String::new();
            if BufReader::new(f).read_line(&mut first).is_err() {
                continue;
            }
            let Ok(line) = serde_json::from_str::<CodexLine>(&first) else { continue };
            if Path::new(&line.payload.cwd) != cwd || taken.contains(&line.payload.id) {
                continue;
            }
            if best.as_ref().is_none_or(|(t, _)| created < *t) {
                best = Some((created, line.payload.id));
            }
        }
    }
    best.map(|(_, id)| id)
}

fn sorted_dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// 覚えておくセッション
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub name: String,
    pub agent: String,
    /// 始めたときのコマンド（--session-id などを付ける前）
    pub cmd: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub resume_id: Option<String>,
    /// 開いていたファイル
    #[serde(default)]
    pub tabs: Vec<PathBuf>,
    /// ワークスペースのフォルダ（ワークスペースで始めたときだけ）
    #[serde(default)]
    pub roots: Vec<PathBuf>,
}

/// claude がその会話を保存しているか（一度も話しかけていないと保存されない）
pub fn claude_has_transcript(id: &str) -> bool {
    let Some(home) = std::env::var_os("HOME") else { return false };
    let base = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(home).join(".claude"));
    let file = format!("{id}.jsonl");
    std::fs::read_dir(base.join("projects"))
        .map(|rd| rd.flatten().any(|e| e.path().join(&file).exists()))
        .unwrap_or(false)
}

impl Saved {
    /// 会話の続きから戻れるか（だめなら同じフォルダで開き直す）
    pub fn resumable(&self) -> bool {
        match (kind(&self.cmd), &self.resume_id) {
            (Kind::Claude, Some(id)) => claude_has_transcript(id),
            (Kind::Codex, Some(_)) => true,
            _ => false,
        }
    }
}

fn path() -> PathBuf {
    config::dir().join("sessions.json")
}

pub fn load() -> Vec<Saved> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<Saved>>(&t).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.cwd.is_dir())
        .collect()
}

pub fn save(list: &[Saved]) {
    let _ = std::fs::create_dir_all(config::dir());
    if let Ok(t) = serde_json::to_string_pretty(list) {
        let tmp = path().with_extension("json.tmp");
        if std::fs::write(&tmp, t).is_ok() {
            let _ = std::fs::rename(&tmp, path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        let (c, id) = start_cmd("claude");
        let id = id.unwrap();
        assert_eq!(c, format!("claude --session-id {id}"));
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_eq!(resume_cmd("claude", "x").unwrap(), "claude --resume x");
        assert_eq!(resume_cmd("codex", "x").unwrap(), "codex resume x");
        assert_eq!(start_cmd("codex").1, None);
        assert!(resume_cmd("", "x").is_none());
        assert_eq!(kind("/Users/a/.claude/local/claude --model opus"), Kind::Claude);
        let d = vec![PathBuf::from("/w/b"), PathBuf::from("/w/it's")];
        assert_eq!(add_dirs("claude", &d), "claude --add-dir '/w/b' --add-dir '/w/it'\\''s'");
        assert_eq!(add_dirs("gemini", &d[..1]), "gemini --include-directories '/w/b'");
        assert_eq!(add_dirs("", &d), "");
    }

    #[test]
    fn finds_codex_session() {
        let root = std::env::temp_dir().join(format!("paispace-codex-{}", std::process::id()));
        let day = root.join("2026/11/10");
        std::fs::create_dir_all(&day).unwrap();
        let started = SystemTime::now();
        let line = |id: &str, cwd: &str| format!(r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}"}}}}"#);
        std::fs::write(day.join("rollout-a.jsonl"), line("other-dir", "/x/other") + "\n").unwrap();
        std::fs::write(day.join("rollout-b.jsonl"), line("mine", "/x/proj") + "\n").unwrap();
        let cwd = Path::new("/x/proj");
        assert_eq!(find_codex_id_in(&root, cwd, started, &[]), Some("mine".into()));
        // ほかのセッションが使っている ID は選ばない
        assert_eq!(find_codex_id_in(&root, cwd, started, &["mine".into()]), None);
        // 始める前の会話は選ばない
        let later = started + Duration::from_secs(60);
        assert_eq!(find_codex_id_in(&root, cwd, later, &[]), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
