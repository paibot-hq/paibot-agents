//! agy（Antigravity CLI）の会話の記録を読む
//!
//! ~/.gemini/antigravity-cli/conversations/<会話ID>.db（SQLite）。中身は Protocol Buffers で、
//! 設計図は公開されていないので、文字列の部分だけを拾って読む。
//! - 会話の情報（trajectory_metadata_blob）の 1.1: 動いたフォルダ（file:///…）
//! - steps.step_type 14: 人からの入力（19.2）
//! - 15: agy の返事（20.1 発言 / 20.7 道具の呼び出し: 2 名前, 3 引数）
//! - 132: 道具の実行（31.2 エラー）
//! - 101: ほかのエージェントからの知らせ（114.1 に sender=<会話ID>、114.2.1 見出し、114.2.10.1 本文）
//! agy が立てた子エージェントは別の会話になり、最初の 101 に親の会話 ID が入っている。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::children::{Child, Kind, Status, Step, StepKind};

pub fn root() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    home.join(".gemini/antigravity-cli/conversations")
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut r = 0u64;
    let mut s = 0;
    loop {
        let c = *b.get(*i)?;
        *i += 1;
        r |= ((c & 0x7f) as u64) << s;
        if c < 0x80 {
            return Some(r);
        }
        s += 7;
        if s > 63 {
            return None;
        }
    }
}

/// Protocol Buffers を読み、文字列を (場所, 文字列) で集める。入れ子に見えるものは中に入る
fn strings(b: &[u8], path: &mut Vec<u32>, out: &mut Vec<(String, String)>, depth: usize) -> bool {
    let mut i = 0;
    while i < b.len() {
        let Some(key) = varint(b, &mut i) else { return false };
        let field = (key >> 3) as u32;
        match key & 7 {
            0 => {
                if varint(b, &mut i).is_none() {
                    return false;
                }
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let Some(len) = varint(b, &mut i) else { return false };
                let len = len as usize;
                if i + len > b.len() {
                    return false;
                }
                let sub = &b[i..i + len];
                i += len;
                path.push(field);
                let mut tmp = Vec::new();
                let nested = depth < 14 && len > 0 && strings(sub, path, &mut tmp, depth + 1);
                if nested {
                    out.extend(tmp);
                } else if let Ok(s) = std::str::from_utf8(sub) {
                    if !s.trim().is_empty() {
                        out.push((path.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("."), s.to_string()));
                    }
                }
                path.pop();
            }
            _ => return false,
        }
        if field == 0 {
            return false;
        }
    }
    i == b.len()
}

fn fields(b: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    strings(b, &mut Vec::new(), &mut out, 0);
    out
}

fn get<'a>(f: &'a [(String, String)], path: &str) -> Option<&'a str> {
    f.iter().find(|(p, _)| p == path).map(|(_, s)| s.as_str())
}

pub struct Conv {
    pub id: String,
    pub cwd: Option<PathBuf>,
    /// agy が立てた子エージェントなら親の会話 ID
    pub parent: Option<String>,
    pub steps: Vec<Step>,
    pub done: bool,
    /// 最後が失敗した道具で、そこで止まっている
    pub failed: bool,
    /// 子エージェントの種類（親が受け取った報告から）: 子の会話 ID → 名前
    pub sub_names: HashMap<String, String>,
}

/// 書き込み中でも読めるように、写しを取ってから開く
fn read_rows(db: &Path) -> Option<(Vec<u8>, Vec<(i64, i64, i64, Vec<u8>)>)> {
    let tmp = std::env::temp_dir().join(format!("paispace-agy-{}-{}", std::process::id(), db.file_stem()?.to_string_lossy()));
    let _ = std::fs::create_dir_all(&tmp);
    let name = db.file_name()?.to_string_lossy().into_owned();
    std::fs::copy(db, tmp.join(&name)).ok()?;
    for ext in ["-wal", "-shm"] {
        let side = db.with_file_name(format!("{name}{ext}"));
        if side.exists() {
            let _ = std::fs::copy(&side, tmp.join(format!("{name}{ext}")));
        }
    }
    let res = (|| {
        let c = rusqlite::Connection::open(tmp.join(&name)).ok()?;
        let meta: Vec<u8> = c.query_row("select data from trajectory_metadata_blob limit 1", [], |r| r.get(0)).unwrap_or_default();
        let mut st = c.prepare("select idx, step_type, status, step_payload from steps order by idx").ok()?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default())))
            .ok()?
            .filter_map(Result::ok)
            .collect();
        Some((meta, rows))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

fn clip(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if t.chars().count() < s.chars().count() { format!("{t}…") } else { t }
}

pub fn read(db: &Path) -> Option<Conv> {
    let (meta, rows) = read_rows(db)?;
    let id = db.file_stem()?.to_string_lossy().into_owned();
    let mf = fields(&meta);
    let cwd = get(&mf, "1.1").or_else(|| get(&mf, "7")).and_then(|u| u.strip_prefix("file://")).map(PathBuf::from);
    let mut parent = None;
    let mut steps = Vec::new();
    let mut sub_names = HashMap::new();
    let mut last_was_plain_reply = false;
    // 段の状態: 3 = 終わった、7 = 失敗（権限で断られたなど）
    let failed = rows.last().is_some_and(|(_, ty, status, _)| *ty == 132 && *status == 7);
    for (n, (_, ty, _status, payload)) in rows.iter().enumerate() {
        let f = fields(payload);
        last_was_plain_reply = false;
        match ty {
            14 => {
                if let Some(t) = get(&f, "19.2") {
                    steps.push(Step { kind: StepKind::Prompt, text: t.trim().to_string() });
                }
            }
            15 => {
                let say = get(&f, "20.1").or_else(|| get(&f, "20.8"));
                if let Some(t) = say {
                    steps.push(Step { kind: StepKind::Say, text: t.trim().to_string() });
                }
                match get(&f, "20.7.2") {
                    Some(name) => {
                        let args = get(&f, "20.7.3").unwrap_or("");
                        // 引数は JSON。よく使う項目は中身の要点だけ
                        let brief = serde_json::from_str::<serde_json::Value>(args)
                            .ok()
                            .and_then(|v| ["CommandLine", "Prompt", "AbsolutePath", "Query", "Url"].iter().find_map(|k| v.get(*k).and_then(|x| x.as_str()).map(str::to_string)))
                            .unwrap_or_else(|| args.to_string());
                        steps.push(Step { kind: StepKind::Tool, text: format!("{name}  {}", clip(brief.trim(), 1200)) });
                    }
                    None => last_was_plain_reply = say.is_some(),
                }
            }
            132 => {
                if let Some(e) = get(&f, "31.2") {
                    steps.push(Step { kind: StepKind::Output, text: clip(e.trim(), 1500) });
                }
            }
            101 => {
                let head = get(&f, "114.2.1").unwrap_or("");
                let body = get(&f, "114.2.10.1").unwrap_or("");
                let sender = get(&f, "114.1").and_then(|m| m.split("sender=").nth(1)).map(|s| s.split_whitespace().next().unwrap_or("").to_string());
                if n == 0 && head.contains("Parent") {
                    // 子エージェント: 親からの頼みごと
                    parent = sender;
                    steps.push(Step { kind: StepKind::Prompt, text: body.trim().to_string() });
                } else {
                    if let (Some(s), Some(open)) = (sender, head.rfind('(')) {
                        let name = head[open + 1..].trim_end_matches(')').to_string();
                        if !name.is_empty() {
                            sub_names.insert(s, name);
                        }
                    }
                    steps.push(Step { kind: StepKind::Output, text: clip(format!("{head}\n{body}").trim(), 1500) });
                }
            }
            _ => {}
        }
    }
    // 最後が「道具を呼ばない返事」なら終わり
    let done = last_was_plain_reply;
    if done {
        if let Some(s) = steps.iter_mut().rev().find(|s| s.kind == StepKind::Say) {
            s.kind = StepKind::Answer;
        }
    }
    Some(Conv { id, cwd, parent, steps, done, failed, sub_names })
}

/// since より後に始まった agy の会話を集める。dirs の中で動いたもの（と、その子エージェント）だけ
pub fn scan(root: &Path, since: SystemTime, dirs: &[PathBuf], cache: &mut HashMap<PathBuf, (SystemTime, Option<Child>, HashMap<String, String>)>) -> Vec<Child> {
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut found: Vec<Child> = Vec::new();
    let mut names: HashMap<String, String> = HashMap::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "db") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let created = md.created().or_else(|_| md.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if created < since {
            continue;
        }
        // 書き込みは -wal に先に入るので、そちらの時刻も見る
        let wal = p.with_file_name(format!("{}-wal", p.file_name().unwrap_or_default().to_string_lossy()));
        let mtime = md.modified().unwrap_or(created).max(std::fs::metadata(&wal).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH));
        if let Some((t, c, n)) = cache.get(&p) {
            if *t == mtime {
                found.extend(c.clone());
                names.extend(n.clone());
                continue;
            }
        }
        let Some(conv) = read(&p) else { continue };
        let fresh = SystemTime::now().duration_since(mtime).unwrap_or_default() < Duration::from_secs(90);
        let status = if conv.done {
            Status::Done
        } else if conv.failed || !fresh {
            Status::Stopped
        } else {
            Status::Running
        };
        let child = Child {
            id: conv.id.clone(),
            kind: Kind::Agy,
            label: "agy".into(),
            parent: None,
            parent_agent: conv.parent.clone(),
            file: p.clone(),
            cwd: conv.cwd.clone().unwrap_or_default(),
            started: created,
            updated: mtime,
            prompt: conv.steps.iter().find(|s| s.kind == StepKind::Prompt).map(|s| s.text.clone()).unwrap_or_default(),
            last: conv.steps.iter().rev().find(|s| matches!(s.kind, StepKind::Say | StepKind::Answer)).map(|s| s.text.clone()).unwrap_or_default(),
            status,
            tools: conv.steps.iter().filter(|s| s.kind == StepKind::Tool).count(),
            commands: conv.steps.iter().filter(|s| s.kind == StepKind::Tool).map(|s| s.text.chars().take(600).collect()).collect(),
        };
        if status != Status::Running {
            cache.insert(p.clone(), (mtime, Some(child.clone()), conv.sub_names.clone()));
        }
        names.extend(conv.sub_names);
        found.push(child);
    }
    // 子エージェントは親のフォルダを受け継ぎ、名前を付ける
    let by_id: HashMap<String, PathBuf> = found.iter().map(|c| (c.id.clone(), c.cwd.clone())).collect();
    for c in &mut found {
        if let Some(pid) = &c.parent_agent {
            if c.cwd.as_os_str().is_empty() {
                c.cwd = by_id.get(pid).cloned().unwrap_or_default();
            }
            c.label = names.get(&c.id).map(|n| format!("agy · {n}")).unwrap_or_else(|| "agy · sub".into());
        }
    }
    found.retain(|c| !c.cwd.as_os_str().is_empty() && dirs.iter().any(|d| c.cwd.starts_with(d)));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 文字列フィールドを1つ書く
    fn put(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
        let mut key = ((field << 3) | 2) as u64;
        loop {
            let b = (key & 0x7f) as u8;
            key >>= 7;
            if key == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        let mut len = bytes.len() as u64;
        loop {
            let b = (len & 0x7f) as u8;
            len >>= 7;
            if len == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        out.extend(bytes);
    }

    #[test]
    fn reads_nested_strings() {
        let mut inner = Vec::new();
        put(&mut inner, 2, "画像を作って".as_bytes());
        let mut outer = Vec::new();
        put(&mut outer, 19, &inner);
        let f = fields(&outer);
        assert_eq!(get(&f, "19.2"), Some("画像を作って"));
    }

    #[test]
    fn reads_a_conversation_db() {
        let dir = std::env::temp_dir().join(format!("paispace-agy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("conv-1.db");
        let _ = std::fs::remove_file(&db);
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "create table trajectory_metadata_blob (id text, data blob);
             create table steps (idx integer, step_type integer, status integer, step_payload blob);",
        )
        .unwrap();
        let mut meta = Vec::new();
        let mut m1 = Vec::new();
        put(&mut m1, 1, b"file:///w/app");
        put(&mut meta, 1, &m1);
        c.execute("insert into trajectory_metadata_blob values ('main', ?1)", [&meta]).unwrap();
        let step = |ty: i64, payload: Vec<u8>, i: i64| {
            c.execute("insert into steps values (?1, ?2, 3, ?3)", rusqlite::params![i, ty, payload]).unwrap();
        };
        let mut p0 = Vec::new();
        let mut p0i = Vec::new();
        put(&mut p0i, 2, "テストを直して".as_bytes());
        put(&mut p0, 19, &p0i);
        step(14, p0, 0);
        let mut call = Vec::new();
        put(&mut call, 2, b"run_command");
        put(&mut call, 3, br#"{"CommandLine":"cargo test"}"#);
        let mut p1i = Vec::new();
        put(&mut p1i, 7, &call);
        let mut p1 = Vec::new();
        put(&mut p1, 20, &p1i);
        step(15, p1, 1);
        let mut p2i = Vec::new();
        put(&mut p2i, 1, "直しました".as_bytes());
        let mut p2 = Vec::new();
        put(&mut p2, 20, &p2i);
        step(15, p2, 2);
        drop(c);

        let conv = read(&db).unwrap();
        assert_eq!(conv.cwd, Some(PathBuf::from("/w/app")));
        assert!(conv.done);
        let kinds: Vec<_> = conv.steps.iter().map(|s| s.kind.clone()).collect();
        assert_eq!(kinds, vec![StepKind::Prompt, StepKind::Tool, StepKind::Answer]);
        assert_eq!(conv.steps[1].text, "run_command  cargo test");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
