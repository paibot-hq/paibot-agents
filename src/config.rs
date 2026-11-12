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
    /// 新規作成で最初に選ばれているエージェント（priority が空なら、自動で選ぶときの一番手）
    pub default_agent: String,
    /// 新しいセッションでエージェントを自動で選ぶときの順番。利用枠が残っている最初のものを使う。
    /// 空なら default_agent、そのあとは agents の順
    pub priority: Vec<String>,
    /// 5時間・1週間の利用枠をこの割合（%）まで使っていたら、次のエージェントにする
    pub limit_percent: f64,
    /// claude の利用枠を知るために、そのセッションだけのステータス表示を差し込む
    /// （ユーザー自身のステータス表示はそのまま呼んで出す。設定ファイルは書き換えない）
    pub track_usage: bool,
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
    /// paibot.app から仕事を受け取る（paibot-agents connect で設定する）
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
            priority: Vec::new(),
            limit_percent: 90.0,
            track_usage: true,
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

    /// 自動で選ぶときの順番（agents の番号）。シェル（コマンドが空）は入れない
    pub fn priority_order(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        let mut push = |i: usize| {
            if !out.contains(&i) && !self.agents[i].cmd.trim().is_empty() {
                out.push(i);
            }
        };
        for name in self.priority.iter().chain(std::iter::once(&self.default_agent)) {
            if let Some(i) = self.agents.iter().position(|a| &a.name == name) {
                push(i);
            }
        }
        for i in 0..self.agents.len() {
            push(i);
        }
        out
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
    if let Some(d) = std::env::var_os("PAIBOT_AGENTS_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    let dir = base.join("paibot-agents");
    // 以前の名前（paispace、その前は mure）の設定とセッションの記録を引き継ぐ
    for old in ["paispace", "mure"] {
        let old = base.join(old);
        if !dir.exists() && old.is_dir() {
            let _ = std::fs::rename(&old, &dir);
        }
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
    std::fs::write(&file, lf!("# Paibot Agents の設定。Paibot Agents の設定画面（⌘,）からも変えられます。\n\n{body}", "# Paibot Agents settings. You can also change these in Paibot Agents (⌘,).\n\n{body}"))?;
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
