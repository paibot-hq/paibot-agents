use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    pub name: String,
    /// 空ならログインシェルを開く
    #[serde(default)]
    pub cmd: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 入力中から一覧に戻るキー（ctrl-<文字>）
    pub prefix: String,
    /// 手が空いたときにmacOSの通知を出す
    pub notify: bool,
    /// 出力がこの秒数止まったら「止まった」とみなす
    pub idle_secs: f64,
    /// 最後の入力からこの秒数以上動いてから止まったら「待ち」にする
    pub min_busy_secs: f64,
    pub scrollback: usize,
    pub sidebar_width: u16,
    pub agents: Vec<Agent>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: "ctrl-]".into(),
            notify: true,
            idle_secs: 2.0,
            min_busy_secs: 5.0,
            scrollback: 5000,
            sidebar_width: 30,
            agents: default_agents(),
        }
    }
}

fn default_agents() -> Vec<Agent> {
    vec![
        Agent { name: "claude".into(), cmd: "claude".into() },
        Agent { name: "codex".into(), cmd: "codex".into() },
        Agent { name: "shell".into(), cmd: String::new() },
    ]
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("mure").join("config.toml")
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
            c.sidebar_width = c.sidebar_width.clamp(16, 80);
            (c, None)
        }
        Err(e) => (
            Config::default(),
            Some(format!("{} を読めませんでした: {}", p.display(), e.message())),
        ),
    }
}
