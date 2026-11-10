use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

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
    let root = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"]).map_err(|_| {
        anyhow::anyhow!("{} はgitリポジトリではありません", dir.display())
    })?.trim());
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

const MAX_LINES: usize = 20_000;

/// 状態と差分（コミットしていない分すべて）を色付きの行にする
pub fn diff_lines(dir: &Path) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let status = match git(dir, &["status", "--short", "--branch"]) {
        Ok(s) => s,
        Err(e) => return vec![Line::styled(format!("git の状態を取れませんでした: {e}"), dim)],
    };
    let mut lines = Vec::new();
    for l in status.lines() {
        let style = if l.starts_with("##") {
            Style::default().add_modifier(Modifier::BOLD)
        } else if l.starts_with("??") {
            dim
        } else {
            Style::default().fg(Color::Yellow)
        };
        lines.push(Line::styled(l.to_string(), style));
    }
    lines.push(Line::raw(""));

    let has_head = git(dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok();
    let base: &[&str] = if has_head { &["diff", "HEAD", "--no-color", "-M"] } else { &["diff", "--no-color"] };
    let stat = git(dir, &[base, &["--stat"]].concat()).unwrap_or_default();
    let diff = git(dir, base).unwrap_or_default();
    if diff.trim().is_empty() {
        lines.push(Line::styled("（コミットしていない差分はありません）", dim));
        return lines;
    }
    for l in stat.lines() {
        lines.push(Line::styled(l.replace('\t', "    "), dim));
    }
    lines.push(Line::raw(""));
    for l in diff.lines() {
        if lines.len() >= MAX_LINES {
            lines.push(Line::styled("……（長いので省略）", dim));
            break;
        }
        let style = if l.starts_with("diff --git") {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else if l.starts_with("+++") || l.starts_with("---") {
            Style::default().add_modifier(Modifier::BOLD)
        } else if l.starts_with('+') {
            Style::default().fg(Color::Green)
        } else if l.starts_with('-') {
            Style::default().fg(Color::Red)
        } else if l.starts_with("@@") {
            Style::default().fg(Color::Cyan)
        } else if l.starts_with("index ") {
            dim
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(l.replace('\t', "    "), style)));
    }
    lines
}
