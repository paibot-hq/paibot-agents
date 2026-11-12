//! エージェントごとの利用枠（5時間・1週間）を読み、新しいセッションで使うエージェントを選ぶ。
//!
//! - claude: 起動するときに `--settings` でそのセッションだけのステータス表示を差し込み、
//!   claude が渡してくる rate_limits を書き留める。ユーザー自身のステータス表示はそのまま呼んで出す
//! - codex: `codex app-server` に account/rateLimits/read を問い合わせる（だめなら会話の記録から）
//! - そのほか: 分からない（優先度の順番だけで扱う）
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Agent, Config};
use crate::resume::{self, Kind};
use crate::session::Wake;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// 使った割合（0〜100）
    pub used: f64,
    /// 枠が戻る時刻（UNIX 秒）
    pub resets_at: u64,
}

impl Window {
    /// 残りの割合（0〜100）
    pub fn left(&self) -> f64 {
        (100.0 - self.used).clamp(0.0, 100.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// いつの値か（UNIX 秒）
    pub at: u64,
}

pub type Usages = HashMap<Kind, Usage>;

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Usage {
    /// 戻る時刻を過ぎた枠は 0% とみなす
    pub fn live(w: Option<Window>, now: u64) -> Option<Window> {
        w.map(|w| if w.resets_at <= now { Window { used: 0.0, ..w } } else { w })
    }

    /// limit（%）以上使っている枠があれば、全部が戻る時刻
    pub fn blocked_until(&self, limit: f64, now: u64) -> Option<u64> {
        [Self::live(self.five_hour, now), Self::live(self.seven_day, now)]
            .into_iter()
            .flatten()
            .filter(|w| w.used >= limit)
            .map(|w| w.resets_at)
            .max()
    }

    /// 短い表示。残りの割合で出す（例: 残り 5h 60% · 1w 88%）
    pub fn label(&self, now: u64) -> String {
        let mut parts = Vec::new();
        if let Some(w) = Self::live(self.five_hour, now) {
            parts.push(format!("5h {:.0}%", w.left()));
        }
        if let Some(w) = Self::live(self.seven_day, now) {
            parts.push(format!("1w {:.0}%", w.left()));
        }
        if parts.is_empty() { String::new() } else { lf!("残り {}", "{} left", parts.join(" · ")) }
    }
}

/// 戻る時刻までの残り（例: 2時間後）
pub fn until(t: u64, now: u64) -> String {
    let s = t.saturating_sub(now);
    match s {
        0..3600 => lf!("{}分後", "in {}m", s / 60 + 1),
        3600..86400 => lf!("{}時間後", "in {}h", s / 3600),
        _ => lf!("{}日後", "in {}d", s / 86400),
    }
}

/// 新しいセッションで使うエージェント（agents の番号）。
/// 優先度の順に見て、枠が残っている最初のもの。全部使い切っていたら一番早く戻るもの
pub fn choose(cfg: &Config, usages: &Usages) -> usize {
    let now = now_secs();
    let order = cfg.priority_order();
    let blocked = |i: usize| usages.get(&resume::kind(&cfg.agents[i].cmd)).and_then(|u| u.blocked_until(cfg.limit_percent, now));
    if let Some(&i) = order.iter().find(|&&i| blocked(i).is_none()) {
        return i;
    }
    order.iter().copied().min_by_key(|&i| blocked(i).unwrap_or(0)).unwrap_or_else(|| cfg.default_agent_index())
}

/// 引き継ぎ先: from と別の種類のエージェントから、choose と同じ決め方で選ぶ
pub fn choose_other(cfg: &Config, usages: &Usages, from: Kind) -> Option<usize> {
    let now = now_secs();
    let order: Vec<usize> = cfg.priority_order().into_iter().filter(|&i| from == Kind::Other || resume::kind(&cfg.agents[i].cmd) != from).collect();
    let blocked = |i: usize| usages.get(&resume::kind(&cfg.agents[i].cmd)).and_then(|u| u.blocked_until(cfg.limit_percent, now));
    order.iter().copied().find(|&i| blocked(i).is_none()).or_else(|| order.iter().copied().min_by_key(|&i| blocked(i).unwrap_or(0)))
}

/// そのエージェントの利用枠（分かれば）
pub fn of<'a>(usages: &'a Usages, agent: &Agent) -> Option<&'a Usage> {
    usages.get(&resume::kind(&agent.cmd))
}

// ---------------------------------------------------------------- 読む

/// 入っているエージェントの利用枠をまとめて読む（codex は数秒かかることがある）
pub fn snapshot(agents: &[Agent]) -> Usages {
    let mut out = Usages::new();
    for a in agents {
        let k = resume::kind(&a.cmd);
        if out.contains_key(&k) {
            continue;
        }
        let u = match k {
            Kind::Claude => read_claude(),
            Kind::Codex => codex_app_server(&a.cmd).or_else(codex_from_logs),
            Kind::Agy => agy_usage(&a.cmd),
            Kind::Other => None,
        };
        if let Some(u) = u {
            out.insert(k, u);
        }
    }
    out
}

fn claude_file() -> PathBuf {
    crate::config::dir().join("usage").join("claude.json")
}

fn read_claude() -> Option<Usage> {
    serde_json::from_str(&std::fs::read_to_string(claude_file()).ok()?).ok()
}

/// rate_limits の1つの枠。claude は used_percentage、codex は used_percent / usedPercent
fn window(v: Option<&Value>) -> Option<Window> {
    let v = v?;
    let used = ["used_percentage", "used_percent", "usedPercent"].iter().find_map(|k| v.get(*k)?.as_f64())?;
    let resets_at = ["resets_at", "resetsAt"].iter().find_map(|k| v.get(*k)?.as_u64())?;
    Some(Window { used, resets_at })
}

/// codex の rateLimits / rate_limits（primary が5時間、secondary が1週間）
fn codex_usage(rl: &Value) -> Option<Usage> {
    let u = Usage { five_hour: window(rl.get("primary")), seven_day: window(rl.get("secondary")), at: now_secs() };
    (u.five_hour.is_some() || u.seven_day.is_some()).then_some(u)
}

/// codex app-server に問い合わせる（読むだけなので枠は減らない）
fn codex_app_server(cmd: &str) -> Option<Usage> {
    let bin = cmd.split_whitespace().next()?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    // .zshrc の PATH やエイリアスも効くように、対話シェル経由で起動する
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", &format!("{bin} app-server")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into()))
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let out = child.stdout.take()?;
    let hello = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"clientInfo": {"name": "paibot-agents", "version": env!("CARGO_PKG_VERSION")}}}),
        json!({"jsonrpc": "2.0", "method": "initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read"}),
    ];
    for m in hello {
        let _ = writeln!(stdin, "{m}");
    }
    let _ = stdin.flush();
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got = None;
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        // .zshrc が何か出しても、JSON でない行は飛ばす
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("id").and_then(Value::as_u64) == Some(2) {
            got = v.pointer("/result/rateLimits").and_then(codex_usage);
            break;
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    got
}

/// agy の枠: `agy -p /usage --output-format json`（メッセージは送らないので枠は使わない）。
/// agy の枠はモデルのグループごとにあるので、ふだん使う Gemini のグループを agy の枠とする
fn agy_usage(cmd: &str) -> Option<Usage> {
    let bin = cmd.split_whitespace().next()?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", &format!("{bin} -p /usage --output-format json")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into()))
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    let text = rx.recv_timeout(Duration::from_secs(25)).ok();
    let _ = child.kill();
    let _ = child.wait();
    // .zshrc が何か出しても、JSON の行だけ読む
    text?.lines().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).find_map(|v| agy_from_json(&v))
}

pub fn agy_from_json(v: &Value) -> Option<Usage> {
    let groups = v.pointer("/command/data/groups")?.as_array()?;
    let g = groups.iter().find(|g| g.get("name").and_then(Value::as_str).is_some_and(|n| n.starts_with("Gemini Models"))).or_else(|| groups.first())?;
    let mut u = Usage { at: now_secs(), ..Default::default() };
    for b in g.get("buckets")?.as_array()? {
        let left = b.get("remaining_fraction").and_then(Value::as_f64).unwrap_or(1.0);
        let Some(resets_at) = b.get("reset_time").and_then(Value::as_str).and_then(parse_utc) else { continue };
        let w = Window { used: ((1.0 - left) * 100.0).clamp(0.0, 100.0), resets_at };
        match b.get("window").and_then(Value::as_str) {
            Some("5h") => u.five_hour = Some(w),
            Some("weekly") => u.seven_day = Some(w),
            _ => {}
        }
    }
    (u.five_hour.is_some() || u.seven_day.is_some()).then_some(u)
}

/// 「2026-11-14T04:17:10Z」を UNIX 秒に
fn parse_utc(s: &str) -> Option<u64> {
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':').map(|x| x.split('.').next().unwrap_or("0").parse::<i64>());
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next().and_then(|x| x.ok()).unwrap_or(0));
    // 日付から日数（1970-01-01 から）
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

/// codex の一番新しい会話の記録から、最後に書かれた利用枠を読む
fn codex_from_logs() -> Option<Usage> {
    let home = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))?;
    let newest = newest_jsonl(&home.join("sessions"), 4)?;
    let mut text = String::new();
    std::fs::File::open(&newest.0).ok()?.read_to_string(&mut text).ok()?;
    let line = text.lines().rev().find(|l| l.contains("\"rate_limits\""))?;
    let v: Value = serde_json::from_str(line).ok()?;
    let mut u = codex_usage(v.pointer("/payload/rate_limits")?)?;
    u.at = newest.1;
    Some(u)
}

/// フォルダの下（depth 段まで）で一番新しい .jsonl と、その更新時刻
fn newest_jsonl(dir: &Path, depth: usize) -> Option<(PathBuf, u64)> {
    let mut best: Option<(PathBuf, u64)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        let cand = if p.is_dir() {
            if depth == 0 {
                continue;
            }
            newest_jsonl(&p, depth - 1)
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            let t = e.metadata().ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
            Some((p, t))
        } else {
            None
        };
        if let Some(c) = cand {
            if best.as_ref().is_none_or(|b| c.1 > b.1) {
                best = Some(c);
            }
        }
    }
    best
}

/// 裏で利用枠を読み直し続ける（5分ごと、または頼まれたとき）
pub struct Monitor {
    pub usages: Arc<Mutex<Usages>>,
    kick: mpsc::Sender<()>,
}

impl Monitor {
    pub fn spawn(agents: Vec<Agent>, wake: Wake) -> Self {
        let usages = Arc::new(Mutex::new(Usages::new()));
        let (kick, rx) = mpsc::channel::<()>();
        let u = usages.clone();
        thread::spawn(move || {
            loop {
                let fresh = snapshot(&agents);
                {
                    let mut m = u.lock().unwrap();
                    for (k, v) in fresh {
                        m.insert(k, v);
                    }
                }
                wake();
                match rx.recv_timeout(Duration::from_secs(300)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                // まとめて頼まれても1回で済ませる
                while rx.try_recv().is_ok() {}
            }
        });
        Self { usages, kick }
    }

    /// すぐ読み直す（新しいセッションの画面を開いたときなど）
    pub fn refresh(&self) {
        let _ = self.kick.send(());
    }

    pub fn get(&self) -> Usages {
        // claude の記録はファイルを読むだけなので、いつも新しいものを使う
        let mut m = self.usages.lock().unwrap().clone();
        if let Some(c) = read_claude() {
            m.insert(Kind::Claude, c);
        }
        m
    }
}

// ---------------------------------------------------------------- claude のステータス表示

fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// claude の設定ファイル（強いものから）。--settings より下のもの
fn claude_settings_files(cwd: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))
        .unwrap_or_default();
    vec![cwd.join(".claude/settings.local.json"), cwd.join(".claude/settings.json"), home.join("settings.json")]
}

fn is_ours(cmd: &str) -> bool {
    cmd.contains("paibot-agents") && cmd.ends_with(" statusline")
}

/// ユーザー自身のステータス表示の設定（あれば）
fn user_statusline(cwd: &Path) -> Option<Value> {
    for f in claude_settings_files(cwd) {
        let Some(v) = std::fs::read_to_string(&f).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
        if let Some(sl) = v.get("statusLine").filter(|sl| sl.is_object()) {
            if sl.get("command").and_then(Value::as_str).is_some_and(is_ours) {
                continue;
            }
            return Some(sl.clone());
        }
    }
    None
}

/// claude にそのセッションだけ差し込むステータス表示（利用枠を書き留める）
pub fn claude_statusline(cwd: &Path) -> Option<Value> {
    let exe = std::env::current_exe().ok()?;
    // ユーザーの設定の padding などは引き継ぎ、コマンドだけ差し替える
    let mut sl = user_statusline(cwd).filter(|v| v.is_object()).unwrap_or_else(|| json!({}));
    sl["type"] = json!("command");
    sl["command"] = json!(format!("{} statusline", shq(&exe.to_string_lossy())));
    Some(sl)
}

/// claude が呼ぶステータス表示（paibot-agents statusline）。
/// 渡された rate_limits を書き留めて、ユーザー自身のステータス表示があればそれを呼んで出す
pub fn statusline() -> anyhow::Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let v: Value = serde_json::from_slice(&input).unwrap_or(Value::Null);
    if let Some(rl) = v.get("rate_limits") {
        let u = Usage { five_hour: window(rl.get("five_hour")), seven_day: window(rl.get("seven_day")), at: now_secs() };
        if u.five_hour.is_some() || u.seven_day.is_some() {
            let f = claude_file();
            if let Some(d) = f.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let tmp = f.with_extension(format!("json.{}", std::process::id()));
            if std::fs::write(&tmp, serde_json::to_vec(&u)?).is_ok() {
                let _ = std::fs::rename(&tmp, &f);
            }
        }
    }
    let cwd = ["/workspace/current_dir", "/cwd"]
        .iter()
        .find_map(|p| v.pointer(p).and_then(Value::as_str).map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let Some(cmd) = user_statusline(&cwd).and_then(|sl| sl.get("command").and_then(Value::as_str).map(String::from)) else { return Ok(()) };
    let mut child = Command::new("/bin/sh").args(["-c", &cmd]).current_dir(&cwd).stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn()?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(&input);
    }
    let st = child.wait()?;
    std::process::exit(st.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(agents: &[(&str, &str)], priority: &[&str]) -> Config {
        Config {
            agents: agents.iter().map(|(n, c)| Agent { name: n.to_string(), cmd: c.to_string() }).collect(),
            priority: priority.iter().map(|s| s.to_string()).collect(),
            default_agent: agents[0].0.into(),
            ..Config::default()
        }
    }

    fn used(p: f64, in_secs: u64) -> Option<Window> {
        Some(Window { used: p, resets_at: now_secs() + in_secs })
    }

    #[test]
    fn reads_agy_usage() {
        assert_eq!(parse_utc("1970-01-02T00:00:01Z"), Some(86401));
        assert_eq!(parse_utc("2026-11-14T04:17:10Z"), Some(1794629830));
        let v = json!({"command": {"data": {"groups": [
            {"name": "Gemini Models", "buckets": [
                {"window": "weekly", "remaining_fraction": 0.75, "reset_time": "2026-11-14T04:17:10Z"},
                {"window": "5h", "remaining_fraction": 1, "reset_time": "2026-11-12T14:39:48Z"}]},
            {"name": "Claude and GPT models", "buckets": [{"window": "5h", "remaining_fraction": 0.1, "reset_time": "2026-11-12T14:39:48Z"}]}
        ]}}});
        let u = agy_from_json(&v).unwrap();
        assert_eq!(u.seven_day.unwrap().used, 25.0);
        assert_eq!(u.five_hour.unwrap().used, 0.0);
    }

    #[test]
    fn picks_the_first_agent_with_room_left() {
        let c = cfg(&[("claude", "claude"), ("codex", "codex"), ("shell", "")], &["claude", "codex"]);
        let mut u = Usages::new();
        assert_eq!(choose(&c, &u), 0, "分からなければ優先度どおり");
        u.insert(Kind::Claude, Usage { five_hour: used(95.0, 3600), seven_day: used(20.0, 86400), at: 0 });
        assert_eq!(choose(&c, &u), 1, "claude の5時間の枠が尽きたら codex");
        u.insert(Kind::Codex, Usage { five_hour: used(10.0, 3600), seven_day: used(99.0, 5 * 86400), at: 0 });
        assert_eq!(choose(&c, &u), 0, "全部尽きたら一番早く戻るもの");
        u.insert(Kind::Claude, Usage { five_hour: Some(Window { used: 100.0, resets_at: now_secs() - 1 }), seven_day: None, at: 0 });
        assert_eq!(choose(&c, &u), 0, "戻る時刻を過ぎた枠は空いている");
    }

    #[test]
    fn priority_falls_back_to_default_then_list_and_skips_shell() {
        let mut c = cfg(&[("shell", ""), ("claude", "claude"), ("codex", "codex")], &[]);
        c.default_agent = "codex".into();
        assert_eq!(c.priority_order(), vec![2, 1]);
        c.priority = vec!["claude".into(), "nope".into()];
        assert_eq!(c.priority_order(), vec![1, 2]);
    }

    #[test]
    fn reads_both_kinds_of_rate_limits() {
        let claude: Value = serde_json::from_str(r#"{"five_hour":{"used_percentage":41.5,"resets_at":100},"seven_day":{"used_percentage":12,"resets_at":200}}"#).unwrap();
        assert_eq!(window(claude.get("five_hour")), Some(Window { used: 41.5, resets_at: 100 }));
        let codex: Value = serde_json::from_str(r#"{"primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":7},"secondary":{"used_percent":4.0,"resets_at":9}}"#).unwrap();
        let u = codex_usage(&codex).unwrap();
        assert_eq!(u.five_hour, Some(Window { used: 0.0, resets_at: 7 }));
        assert_eq!(u.seven_day, Some(Window { used: 4.0, resets_at: 9 }));
    }

    #[test]
    fn probe_keeps_users_statusline_options() {
        let dir = std::env::temp_dir().join(format!("pa-usage-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/settings.json"), r#"{"statusLine":{"type":"command","command":"echo hi","padding":2}}"#).unwrap();
        let sl = claude_statusline(&dir).unwrap();
        assert_eq!(sl["padding"], 2);
        assert!(sl["command"].as_str().unwrap().ends_with(" statusline"), "{sl}");
        assert_eq!(user_statusline(&dir).unwrap()["command"], "echo hi");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
