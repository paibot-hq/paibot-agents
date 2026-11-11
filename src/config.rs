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
    /// 画面の言語: "auto"（macOS の設定に合わせる）/ "ja" / "en"
    pub language: String,
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
    /// paibot.app から仕事を受け取る（paispace connect で設定する）
    pub paibot_url: String,
    pub paibot_token: String,
    /// paibot.app から頼める Bot。ここに書いたものだけが、書いたフォルダで動く
    pub bots: Vec<BotDef>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct BotDef {
    pub name: String,
    /// claude / codex / agy / echo（echo は試し用。頼まれた文をそのまま返す）
    pub agent: String,
    /// この Bot が動くフォルダ
    pub folder: String,
    /// 一覧に出す説明
    #[serde(default)]
    pub note: String,
    /// エージェントに足す引数（例: claude の --model）
    #[serde(default)]
    pub args: String,
    /// true にすると、ファイルを書いたりコマンドを実行したりできる（そのフォルダのエージェントの設定どおり）。
    /// 初めは false: paibot.app から頼まれた仕事は読むだけで、.env や鍵のファイルも読めない
    #[serde(default)]
    pub write: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            project_roots: Vec::new(),
            projects: Vec::new(),
            workspaces: Vec::new(),
            vscode_recent: true,
            default_agent: "claude".into(),
            language: "auto".into(),
            editor_app: "Visual Studio Code".into(),
            notify: true,
            font_size: 13.0,
            idle_secs: 2.0,
            min_busy_secs: 5.0,
            scrollback: 10000,
            agents: default_agents(),
            paibot_url: "https://paibot.app".into(),
            paibot_token: String::new(),
            bots: Vec::new(),
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
        Err(e) => (Config::default(), Some(lf!("{} を読めませんでした: {}", "Could not read {}: {}", p.display(), e.message()))),
    }
}

pub fn save(c: &Config) -> Result<()> {
    std::fs::create_dir_all(dir())?;
    let body = toml::to_string_pretty(c)?;
    let file = path();
    std::fs::write(&file, lf!("# paispace の設定。paispace の設定画面（⌘,）からも変えられます。\n\n{body}", "# paispace settings. You can also change these in paispace (⌘,).\n\n{body}"))?;
    // paibot.app とつなぐ鍵が入るので、自分だけが読めるようにする
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
    }
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
