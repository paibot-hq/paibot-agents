//! 途中までの作業を、別のエージェントに引き継ぐ。
//! 会話の記録から「頼まれたこと・これまでの流れ・変えたファイル・最後の状態」を取り出して、
//! 新しいエージェントの最初の入力にする
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::children::{self, Step, StepKind};
use crate::resume::{self, Kind};

/// 最初の入力の長さの目安（長すぎると読むだけで枠を使う）
const BUDGET: usize = 16_000;

/// そのセッションの会話の記録
pub fn transcript(kind: Kind, cwd: &Path, id: &str) -> Option<PathBuf> {
    match kind {
        Kind::Claude => {
            let f = children::claude_project_dir(&children::claude_root(), cwd).join(format!("{id}.jsonl"));
            if f.is_file() {
                return Some(f);
            }
            // フォルダの名前の付け方が違っても見つかるように
            std::fs::read_dir(children::claude_root()).ok()?.flatten().map(|d| d.path().join(format!("{id}.jsonl"))).find(|f| f.is_file())
        }
        Kind::Codex => find_named(&children::codex_root(), id, 3),
        Kind::Other => None,
    }
}

/// 名前に id を含む .jsonl を探す（新しい日付のフォルダから）
fn find_named(dir: &Path, id: &str, depth: usize) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries.into_iter().rev() {
        if p.is_dir() {
            if depth > 0 {
                if let Some(f) = find_named(&p, id, depth - 1) {
                    return Some(f);
                }
            }
        } else if p.extension().is_some_and(|x| x == "jsonl") && p.file_name().is_some_and(|n| n.to_string_lossy().contains(id)) {
            return Some(p);
        }
    }
    None
}

fn clip(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if t.chars().count() < s.chars().count() { format!("{t}…") } else { t }
}

/// ファイルを変える道具なら、そのファイル
fn changed_file(step: &Step) -> Option<String> {
    let (name, rest) = step.text.split_once("  ")?;
    match name {
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => Some(rest.trim().to_string()),
        "apply_patch" => rest.lines().find_map(|l| ["*** Update File: ", "*** Add File: ", "*** Delete File: "].iter().find_map(|p| l.strip_prefix(p)).map(|f| f.trim().to_string())),
        _ => None,
    }
}

fn git_status(cwd: &Path) -> String {
    std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().take(60).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// 引き継ぎの文面を作る
pub fn build(from: &str, cwd: &Path, steps: &[Step]) -> String {
    let prompts: Vec<&Step> = steps.iter().filter(|s| s.kind == StepKind::Prompt).collect();
    let mut files: Vec<String> = Vec::new();
    for f in steps.iter().filter_map(changed_file) {
        if !files.contains(&f) {
            files.push(f);
        }
    }
    let last = steps.iter().rev().find(|s| matches!(s.kind, StepKind::Answer | StepKind::Say));

    let mut out = String::new();
    out.push_str(&lf!(
        "これは、別のコーディングエージェント（{from}）がこのフォルダ（{}）でやっていた作業の続きです。前のエージェントは利用枠などの都合で止まりました。以下はその会話の記録から取り出した経緯です。\n\n",
        "This continues work that another coding agent ({from}) was doing in this folder ({}). It stopped (for example, it ran out of usage). Below is what happened, taken from its conversation log.\n\n",
        cwd.display()
    ));

    out.push_str(l!("## 頼まれたこと\n\n", "## What was asked\n\n"));
    // 最初の頼みごとと、最近の頼みごとを優先する
    let mut asked = String::new();
    for (n, p) in prompts.iter().enumerate() {
        let keep = n == 0 || n + 4 >= prompts.len();
        if keep {
            asked.push_str(&format!("- {}\n", clip(&p.text.replace('\n', "\n  "), 1500)));
        } else if n == 1 {
            asked.push_str(&lf!("- （途中の {} 件は省略）\n", "- ({} more in between, omitted)\n", prompts.len().saturating_sub(5)));
        }
    }
    out.push_str(&asked);
    out.push('\n');

    if !files.is_empty() {
        out.push_str(l!("## 前のエージェントが変えたファイル\n\n", "## Files the previous agent changed\n\n"));
        for f in files.iter().take(40) {
            out.push_str(&format!("- {f}\n"));
        }
        out.push('\n');
    }
    let st = git_status(cwd);
    if !st.is_empty() {
        out.push_str(l!("## いまの git status --short\n\n```\n", "## Current git status --short\n\n```\n"));
        out.push_str(&st);
        out.push_str("\n```\n\n");
    }

    // これまでの流れ: 新しいものから、予算に収まるだけ（ツールの出力は省く）
    let head_len = out.len() + 2000;
    let mut flow: Vec<String> = Vec::new();
    let mut used = 0usize;
    for s in steps.iter().rev() {
        let line = match s.kind {
            StepKind::Prompt => format!("[{}] {}", l!("頼まれた", "asked"), clip(&s.text, 600)),
            StepKind::Say | StepKind::Answer => format!("[{from}] {}", clip(&s.text, 1200)),
            StepKind::Tool => format!("  ({})", clip(s.text.lines().next().unwrap_or(""), 200)),
            StepKind::Output => continue,
        };
        if head_len + used + line.len() > BUDGET {
            flow.push(l!("…（これより前は省略）", "… (earlier steps omitted)").to_string());
            break;
        }
        used += line.len() + 1;
        flow.push(line);
    }
    flow.reverse();
    out.push_str(l!("## これまでの流れ（古い順）\n\n", "## What happened (oldest first)\n\n"));
    out.push_str(&flow.join("\n"));
    out.push_str("\n\n");

    if let Some(l) = last {
        out.push_str(l!("## 前のエージェントの最後の発言\n\n", "## The previous agent's last message\n\n"));
        out.push_str(&clip(&l.text, 3000));
        out.push_str("\n\n");
    }
    out.push_str(l!(
        "共有メモ（notes_read）が使えるなら、ほかのエージェントが残したメモも読んでください。まず git の状態と上のファイルを見て、どこまで終わっているかを確かめてから、残りの作業を続けてください。終わっていることはやり直さないでください。分からないことがあれば聞いてください。",
        "If shared notes (notes_read) are available, read what other agents left there too. First check git and the files above to see how far the work got, then carry on with what is left. Don't redo finished work. Ask if anything is unclear."
    ));
    out
}

/// セッションの記録を読んで、引き継ぎの文面を作る
pub fn from_session(agent_name: &str, base_cmd: &str, cwd: &Path, resume_id: Option<&str>) -> Result<String> {
    let kind = resume::kind(base_cmd);
    let Some(id) = resume_id else {
        bail!("{}", l!("このセッションの会話の記録が見つかりません（claude と codex だけ引き継げます）", "Could not find this session's conversation log (only claude and codex can be handed off)"))
    };
    let Some(file) = transcript(kind, cwd, id) else { bail!("{}", l!("会話の記録が見つかりません（まだ一度も話していないかもしれません）", "Conversation log not found (maybe nothing was said yet)")) };
    let ck = match kind {
        Kind::Claude => children::Kind::Claude,
        _ => children::Kind::Codex,
    };
    let (steps, _) = children::read_steps(ck, &file);
    if !steps.iter().any(|s| s.kind == StepKind::Prompt) {
        bail!("{}", l!("まだ何も頼んでいないので、引き継ぐことがありません", "Nothing was asked yet, so there is nothing to hand off"));
    }
    Ok(build(agent_name, cwd, &steps))
}

/// 最初に頼んだことから、一覧に出す短い名前を作る（引き継ぎの文面は飛ばす）
pub fn title(base_cmd: &str, cwd: &Path, resume_id: &str) -> Option<String> {
    let kind = resume::kind(base_cmd);
    let file = transcript(kind, cwd, resume_id)?;
    let ck = match kind {
        Kind::Claude => children::Kind::Claude,
        Kind::Codex => children::Kind::Codex,
        Kind::Other => return None,
    };
    let (steps, _) = children::read_steps(ck, &file);
    steps.iter().filter(|s| s.kind == StepKind::Prompt).map(|s| title_of(&s.text)).find(|t| !t.is_empty() && !is_handoff(t))
}

fn is_handoff(t: &str) -> bool {
    t.starts_with("これは、別のコーディングエージェント") || t.starts_with("This continues work that another coding agent")
}

/// 文の最初の行を、一覧に収まる長さにする
pub fn title_of(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    clip(line, 40)
}

/// 最初の入力を付けて起動するコマンド。付け方が分からないエージェントは None（あとから貼り付ける）
pub fn with_prompt(cmd: &str, prompt: &str) -> Option<String> {
    let q = resume::quote(Path::new(prompt));
    let first = cmd.split_whitespace().next().unwrap_or("");
    match first.rsplit('/').next().unwrap_or(first) {
        "claude" | "codex" => Some(format!("{cmd} {q}")),
        "agy" => Some(format!("{cmd} -i {q}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(kind: StepKind, t: &str) -> Step {
        Step { kind, text: t.into() }
    }

    #[test]
    fn handoff_has_the_ask_files_and_last_word() {
        let steps = vec![
            step(StepKind::Prompt, "ログイン画面にパスワードを忘れたリンクを足して"),
            step(StepKind::Say, "見てみます"),
            step(StepKind::Tool, "Read  src/login.tsx"),
            step(StepKind::Output, "export function Login() {}"),
            step(StepKind::Tool, "Edit  src/login.tsx"),
            step(StepKind::Tool, "apply_patch  *** Begin Patch\n*** Add File: src/forgot.tsx\n+x"),
            step(StepKind::Say, "リンクは足しました。次はリセットの画面です"),
        ];
        let t = build("claude", Path::new("/nowhere"), &steps);
        assert!(t.contains("ログイン画面にパスワードを忘れたリンクを足して"), "{t}");
        assert!(t.contains("- src/login.tsx\n- src/forgot.tsx"), "{t}");
        assert!(t.contains("次はリセットの画面です"), "{t}");
        assert!(!t.contains("export function Login"), "ツールの出力は入れない: {t}");
    }

    #[test]
    fn long_logs_stay_within_budget() {
        let mut steps = vec![step(StepKind::Prompt, "大きな作業")];
        for n in 0..2000 {
            steps.push(step(StepKind::Say, &format!("手順 {n} {}", "あ".repeat(100))));
        }
        let t = build("codex", Path::new("/nowhere"), &steps);
        assert!(t.len() < BUDGET + 8000, "{}", t.len());
        assert!(t.contains("手順 1999"), "新しい手順は残る");
        assert!(!t.contains("手順 5 "), "古い手順は省く");
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(title_of("\n  ログイン画面を直して\n詳しくは…"), "ログイン画面を直して");
        assert_eq!(title_of(&"あ".repeat(50)).chars().count(), 41);
        assert!(is_handoff(&title_of(&build("claude", Path::new("/nowhere"), &[step(StepKind::Prompt, "x")]))));
    }

    #[test]
    fn prompt_goes_where_each_agent_expects_it() {
        assert_eq!(with_prompt("claude --session-id x", "it's").unwrap(), "claude --session-id x 'it'\\''s'");
        assert_eq!(with_prompt("/usr/local/bin/codex", "hi").unwrap(), "/usr/local/bin/codex 'hi'");
        assert_eq!(with_prompt("agy", "hi").unwrap(), "agy -i 'hi'");
        assert!(with_prompt("gemini", "hi").is_none());
    }
}
