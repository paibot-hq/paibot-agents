//! プロジェクトの一覧、入っているエージェントとエディタを探す

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::util::{expand, tilde};

#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
    /// ~ で始まる表示用の場所
    pub path: String,
    pub git: bool,
    pub recent: bool,
    modified: SystemTime,
}

fn project(p: &Path, recent: bool) -> Option<Project> {
    let meta = std::fs::metadata(p).ok()?;
    if !meta.is_dir() {
        return None;
    }
    let git_meta = std::fs::metadata(p.join(".git")).ok();
    let modified = git_meta
        .as_ref()
        .and_then(|_| std::fs::metadata(p.join(".git").join("index")).ok())
        .and_then(|m| m.modified().ok())
        .or_else(|| meta.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Some(Project {
        name: p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| tilde(p)),
        path: tilde(p),
        git: git_meta.is_some(),
        recent,
        modified,
    })
}

/// 最近使ったもの → 個別に登録したもの → 置き場の中（最近さわった順）
pub fn list(roots: &[String], pinned: &[String], recent: &[String]) -> Vec<Project> {
    let mut out: Vec<Project> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in recent {
        let p = expand(r);
        if seen.insert(p.clone()) {
            out.extend(project(&p, true));
        }
    }
    let mut rest = Vec::new();
    for r in pinned {
        let p = expand(r);
        if seen.insert(p.clone()) {
            rest.extend(project(&p, false));
        }
    }
    for root in roots {
        let Ok(rd) = std::fs::read_dir(expand(root)) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let hidden = p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'));
            if hidden || !seen.insert(p.clone()) {
                continue;
            }
            rest.extend(project(&p, false));
        }
    }
    rest.sort_by(|a, b| b.modified.cmp(&a.modified));
    out.extend(rest);
    out
}

/// 置き場の候補（子にgitリポジトリがあるもの）
pub fn suggest_roots(launch: &Path) -> Vec<String> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let mut cands: Vec<PathBuf> = Vec::new();
    if launch.join(".git").exists() {
        if let Some(p) = launch.parent() {
            cands.push(p.to_path_buf());
        }
    }
    cands.push(launch.to_path_buf());
    if let Some(p) = launch.parent() {
        cands.push(p.to_path_buf());
    }
    for n in ["work", "src", "dev", "code", "projects", "Projects", "Developer", "repos", "github", "git"] {
        cands.push(home.join(n));
    }
    let mut scored: Vec<(usize, String)> = Vec::new();
    for c in cands {
        let t = tilde(&c);
        if scored.iter().any(|(_, s)| *s == t) {
            continue;
        }
        let repos = std::fs::read_dir(&c)
            .map(|rd| rd.flatten().take(500).filter(|e| e.path().join(".git").exists()).count())
            .unwrap_or(0);
        if repos > 0 {
            scored.push((repos, t));
        }
    }
    // gitリポジトリが多く入っている順
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, t)| t).collect()
}

pub const KNOWN_AGENTS: &[&str] =
    &["claude", "codex", "gemini", "cursor-agent", "agy", "aider", "opencode", "amp", "qwen", "goose", "copilot"];

/// ログインシェルで見つかるコマンド（エイリアスも含む）
pub fn detect_agents() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!(
        "for c in {}; do command -v $c >/dev/null 2>&1 && echo $c; done",
        KNOWN_AGENTS.join(" ")
    );
    let Ok(out) = Command::new(shell).args(["-l", "-i", "-c", &script]).stdin(std::process::Stdio::null()).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    KNOWN_AGENTS.iter().filter(|a| text.lines().any(|l| l.trim() == **a)).map(|s| s.to_string()).collect()
}

pub const KNOWN_EDITORS: &[&str] = &[
    "Visual Studio Code",
    "Cursor",
    "Zed",
    "Windsurf",
    "Sublime Text",
    "Nova",
    "IntelliJ IDEA",
    "RustRover",
    "WebStorm",
    "Xcode",
    "TextEdit",
];

pub fn detect_editors() -> Vec<String> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    KNOWN_EDITORS
        .iter()
        .filter(|n| {
            let app = format!("{n}.app");
            Path::new("/Applications").join(&app).exists()
                || home.join("Applications").join(&app).exists()
                || Path::new("/System/Applications").join(&app).exists()
        })
        .map(|s| s.to_string())
        .collect()
}
