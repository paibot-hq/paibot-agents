use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};

use crate::session::Wake;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn current_branch(dir: &Path) -> Option<String> {
    let b = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    let b = b.trim();
    (!b.is_empty()).then(|| b.to_string())
}

/// <repo>/../<repo名>.worktrees/<ブランチ> にworktreeを用意する（あれば使い回す）
pub fn ensure_worktree(dir: &Path, branch: &str) -> Result<PathBuf> {
    let root = git(dir, &["rev-parse", "--show-toplevel"])
        .map_err(|_| anyhow::anyhow!("{} はgitリポジトリではありません", dir.display()))?;
    let root = PathBuf::from(root.trim());
    let name = root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let base = root.parent().unwrap_or(&root).join(format!("{name}.worktrees"));
    let path = base.join(branch.replace('/', "-"));
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(&base)?;
    let p = path.to_string_lossy().into_owned();
    let exists = git(&root, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    if exists {
        git(&root, &["worktree", "add", &p, branch])?;
    } else {
        git(&root, &["worktree", "add", "-b", branch, &p])?;
    }
    Ok(path)
}

fn diff_base(dir: &Path) -> Vec<&'static str> {
    if git(dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        vec!["diff", "HEAD", "--no-color", "-M"]
    } else {
        vec!["diff", "--no-color"]
    }
}

/// 一覧のカードに出す小さな情報
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub branch: Option<String>,
    pub added: usize,
    pub removed: usize,
    pub files: usize,
}

pub fn info(dir: &Path) -> Info {
    let mut i = Info { branch: current_branch(dir), ..Default::default() };
    if i.branch.is_none() {
        return i;
    }
    let mut args = diff_base(dir);
    args.push("--numstat");
    for l in git(dir, &args).unwrap_or_default().lines() {
        let mut it = l.split('\t');
        i.added += it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        i.removed += it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        i.files += 1;
    }
    i.files += git(dir, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default().lines().count();
    i
}

/// 見ているフォルダのgit情報を裏で3秒ごとに取り直す
pub fn spawn_watcher(dirs: Arc<Mutex<Vec<PathBuf>>>, out: Arc<Mutex<HashMap<PathBuf, Info>>>, wake: Wake) {
    std::thread::spawn(move || {
        loop {
            let list = dirs.lock().unwrap().clone();
            for d in list {
                let inf = info(&d);
                out.lock().unwrap().insert(d, inf);
            }
            wake();
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    File,
    Meta,
    Hunk,
    Add,
    Del,
    Plain,
}

pub struct DiffLine {
    pub kind: Kind,
    pub text: String,
    /// このまとまりのファイル（リポジトリからの場所）
    pub file: String,
    /// まとまりの見出し（@@）なら、変更後の行の範囲
    pub range: Option<(usize, usize)>,
}

/// "@@ -1,5 +12,7 @@" → (12, 18)
fn hunk_range(l: &str) -> Option<(usize, usize)> {
    let plus = l.split_whitespace().find(|w| w.starts_with('+'))?;
    let mut it = plus[1..].split(',');
    let start: usize = it.next()?.parse().ok()?;
    let len: usize = it.next().map(|n| n.parse().unwrap_or(1)).unwrap_or(1);
    Some((start.max(1), (start + len.max(1) - 1).max(1)))
}

pub struct Diff {
    pub status: Vec<String>,
    pub lines: Vec<DiffLine>,
    pub error: Option<String>,
}

const MAX_LINES: usize = 50_000;

/// 状態と、コミットしていない差分すべて
pub fn diff(dir: &Path) -> Diff {
    let status = match git(dir, &["status", "--short"]) {
        Ok(s) => s.lines().map(str::to_string).collect(),
        Err(e) => return Diff { status: vec![], lines: vec![], error: Some(format!("{e}")) },
    };
    let args = diff_base(dir);
    let text = git(dir, &args).unwrap_or_default();
    let lines = parse(&text);
    Diff { status, lines, error: None }
}

fn parse(text: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut file = String::new();
    for l in text.lines().take(MAX_LINES) {
        if let Some(f) = l.strip_prefix("+++ b/") {
            file = f.to_string();
        } else if l.starts_with("diff --git") {
            file = l.split(" b/").last().unwrap_or("").to_string();
        }
        let kind = if l.starts_with("diff --git") {
            Kind::File
        } else if l.starts_with("+++")
            || l.starts_with("---")
            || l.starts_with("index ")
            || l.starts_with("new file")
            || l.starts_with("deleted file")
            || l.starts_with("similarity")
            || l.starts_with("rename ")
        {
            Kind::Meta
        } else if l.starts_with("@@") {
            Kind::Hunk
        } else if l.starts_with('+') {
            Kind::Add
        } else if l.starts_with('-') {
            Kind::Del
        } else {
            Kind::Plain
        };
        let text = if kind == Kind::File { l.split(" b/").last().unwrap_or(l).to_string() } else { l.replace('\t', "    ") };
        let range = if kind == Kind::Hunk { hunk_range(l) } else { None };
        lines.push(DiffLine { kind, text, file: file.clone(), range });
    }
    lines
}

#[cfg(test)]
mod tests {
    #[test]
    fn hunk_range() {
        assert_eq!(super::hunk_range("@@ -1,5 +12,7 @@ fn x()"), Some((12, 18)));
        assert_eq!(super::hunk_range("@@ -0,0 +1 @@"), Some((1, 1)));
    }
}
