use std::path::{Path, PathBuf};

pub fn expand(s: &str) -> PathBuf {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('~') {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest.trim_start_matches('/'));
        }
    }
    PathBuf::from(s)
}

pub fn tilde(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().into_owned();
        if !home.is_empty() && (s == home || s.starts_with(&format!("{home}/"))) {
            return format!("~{}", &s[home.len()..]);
        }
    }
    s
}

/// エージェントに伝える場所: 作業フォルダの中なら相対、外なら絶対
pub fn rel_for_agent(cwd: &Path, path: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(r) => r.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

pub fn open_in(app: &str, path: &Path) -> bool {
    std::process::Command::new("open").arg("-a").arg(app).arg(path).status().is_ok_and(|s| s.success())
}
