use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Agent {
    pub name: String,
    /// 空ならログインシェルを開く
    #[serde(default)]
    pub cmd: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// この中のフォルダを「プロジェクト」として新規作成の一覧に出す
    pub project_roots: Vec<String>,
    /// 一覧に出したい個別のフォルダ
    pub projects: Vec<String>,
    /// 一覧に出す VSCode のワークスペース（.code-workspace）
    pub workspaces: Vec<String>,
    /// VSCode / Cursor などで最近開いたフォルダとワークスペースも一覧に出す
    pub vscode_recent: bool,
    /// 新規作成で最初に選ばれているエージェント
    pub default_agent: String,
    /// 「エディタで開く」に使うアプリ
    pub editor_app: String,
    /// 手が空いたときにmacOSの通知を出す
    pub notify: bool,
    pub font_size: f32,
    /// 出力がこの秒数止まったら「止まった」とみなす
    pub idle_secs: f64,
    /// 最後の入力からこの秒数以上動いてから止まったら「待ち」にする
    pub min_busy_secs: f64,
    pub scrollback: usize,
    pub agents: Vec<Agent>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            project_roots: Vec::new(),
            projects: Vec::new(),
            workspaces: Vec::new(),
            vscode_recent: true,
            default_agent: "claude".into(),
            editor_app: "Visual Studio Code".into(),
            notify: true,
            font_size: 13.0,
            idle_secs: 2.0,
            min_busy_secs: 5.0,
            scrollback: 10000,
            agents: default_agents(),
        }
    }
}

impl Config {
    pub fn default_agent_index(&self) -> usize {
        self.agents.iter().position(|a| a.name == self.default_agent).unwrap_or(0)
    }
}

fn default_agents() -> Vec<Agent> {
    vec![
        Agent { name: "claude".into(), cmd: "claude".into() },
        Agent { name: "codex".into(), cmd: "codex".into() },
        Agent { name: "shell".into(), cmd: String::new() },
    ]
}

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PAISPACE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    let dir = base.join("paispace");
    // 以前の名前（mure）の設定とセッションの記録を引き継ぐ
    let old = base.join("mure");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    dir
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

pub fn exists() -> bool {
    path().exists()
}

/// 設定を読む。壊れていたら既定値で動かし、理由を返す。
pub fn load() -> (Config, Option<String>) {
    let p = path();
    let Ok(text) = std::fs::read_to_string(&p) else {
        return (Config::default(), None);
    };
    match toml::from_str::<Config>(&text) {
        Ok(mut c) => {
            if c.agents.is_empty() {
                c.agents = default_agents();
            }
            c.font_size = c.font_size.clamp(8.0, 32.0);
            (c, None)
        }
        Err(e) => (Config::default(), Some(format!("{} を読めませんでした: {}", p.display(), e.message()))),
    }
}

pub fn save(c: &Config) -> Result<()> {
    std::fs::create_dir_all(dir())?;
    let body = toml::to_string_pretty(c)?;
    std::fs::write(path(), format!("# paispace の設定。paispace の設定画面（⌘,）からも変えられます。\n\n{body}"))?;
    Ok(())
}

/// 最近使ったフォルダ（新しい順）
pub fn load_recent() -> Vec<String> {
    std::fs::read_to_string(dir().join("recent.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn save_recent(list: &[String]) {
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(dir().join("recent.txt"), list.join("\n"));
}
