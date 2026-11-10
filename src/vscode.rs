//! VSCode（と Cursor などの仲間）のワークスペースと、最近開いたものを読む。書き込みはしない

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct CodeWorkspace {
    pub name: String,
    pub folders: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct WsFile {
    #[serde(default)]
    folders: Vec<WsFolder>,
}

#[derive(Deserialize)]
struct WsFolder {
    path: Option<String>,
    uri: Option<String>,
}

/// コメントと末尾のカンマを取り除いて、ふつうの JSON にする（.code-workspace は JSONC）
fn strip_jsonc(s: &str) -> String {
    strip_trailing_commas(&strip_comments(s))
}

/// 文字列の外にある // と /* */ を消す
fn strip_comments(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
        } else if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// 文字列の外で、すぐ後ろが } か ] のカンマを消す
fn strip_trailing_commas(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == ',' {
            let next = b[i + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// file:///a/b → /a/b（リモートなどは None）
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(|p| PathBuf::from(percent_decode(p)))
}

pub fn parse_workspace(file: &Path) -> Option<CodeWorkspace> {
    let text = std::fs::read_to_string(file).ok()?;
    let ws: WsFile = serde_json::from_str(&strip_jsonc(&text)).ok()?;
    let base = file.parent().unwrap_or(Path::new("/"));
    let folders = ws
        .folders
        .into_iter()
        .filter_map(|f| match (f.path, f.uri) {
            (Some(p), _) => {
                let p = PathBuf::from(p);
                Some(if p.is_absolute() { p } else { base.join(p) })
            }
            (None, Some(u)) => uri_to_path(&u),
            _ => None,
        })
        .filter_map(|p| std::fs::canonicalize(&p).ok())
        .filter(|p| p.is_dir())
        .collect();
    let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    Some(CodeWorkspace { name, folders })
}

/// フォルダの中（1段だけ）にある .code-workspace
pub fn find_workspace_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "code-workspace") && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

pub enum Recent {
    Folder(PathBuf),
    Workspace(PathBuf),
}

const APPS: &[&str] = &["Code", "Cursor", "Windsurf", "VSCodium", "Code - Insiders"];

#[derive(Deserialize)]
struct WsJson {
    folder: Option<String>,
    workspace: Option<String>,
}

/// VSCode などで最近開いたフォルダとワークスペース（新しい順）
pub fn recent(limit: usize) -> Vec<Recent> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let mut items: Vec<(SystemTime, PathBuf, bool)> = Vec::new();
    for app in APPS {
        let dir = home.join("Library/Application Support").join(app).join("User/workspaceStorage");
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let d = e.path();
            let Ok(text) = std::fs::read_to_string(d.join("workspace.json")) else { continue };
            let Ok(w) = serde_json::from_str::<WsJson>(&text) else { continue };
            let when = std::fs::metadata(d.join("state.vscdb"))
                .or_else(|_| std::fs::metadata(&d))
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if let Some(p) = w.folder.as_deref().and_then(uri_to_path) {
                items.push((when, p, false));
            } else if let Some(p) = w.workspace.as_deref().and_then(uri_to_path) {
                items.push((when, p, true));
            }
        }
    }
    items.sort_by(|a, b| b.0.cmp(&a.0));
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (_, p, is_ws) in items {
        if out.len() >= limit {
            break;
        }
        if !p.exists() || !seen.insert(p.clone()) {
            continue;
        }
        out.push(if is_ws { Recent::Workspace(p) } else { Recent::Folder(p) });
    }
    out
}

/// フォルダたちの共通の親（ホームや / になるときは None）
pub fn common_parent(folders: &[PathBuf]) -> Option<PathBuf> {
    let first = folders.first()?;
    let mut base = first.clone();
    for f in &folders[1..] {
        while !f.starts_with(&base) {
            base = base.parent()?.to_path_buf();
        }
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if base == Path::new("/") || base == home || base.components().count() <= 2 {
        return None;
    }
    Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc() {
        let s = r#"{
  // フォルダ
  "folders": [
    { "name": "a", "path": "a//b" }, /* x */
  ],
}"#;
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(s)).unwrap();
        assert_eq!(v["folders"][0]["path"], "a//b");
    }

    #[test]
    fn uri() {
        assert_eq!(uri_to_path("file:///Users/x/%E6%97%A5%E6%9C%AC"), Some(PathBuf::from("/Users/x/日本")));
        assert_eq!(uri_to_path("vscode-remote://ssh/x"), None);
    }

    #[test]
    fn parent() {
        let f = vec![PathBuf::from("/Volumes/SSD/work/a"), PathBuf::from("/Volumes/SSD/work/b/c")];
        assert_eq!(common_parent(&f), Some(PathBuf::from("/Volumes/SSD/work")));
    }
}
