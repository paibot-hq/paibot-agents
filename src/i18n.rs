//! 画面の言語（日本語 / English）

use std::sync::atomic::{AtomicBool, Ordering};

static EN: AtomicBool = AtomicBool::new(false);

pub fn en() -> bool {
    EN.load(Ordering::Relaxed)
}

/// 設定の language（"auto" / "ja" / "en"）から決める。auto なら macOS の言語設定に合わせる
pub fn apply(setting: &str) {
    let en = match setting {
        "ja" => false,
        "en" => true,
        _ => !system_is_japanese(),
    };
    EN.store(en, Ordering::Relaxed);
}

fn system_is_japanese() -> bool {
    for k in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() && v != "C" && v != "C.UTF-8" {
                return v.starts_with("ja");
            }
        }
    }
    // Finder から開いたときは LANG が無いので、macOS の言語設定を見る
    std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.split('"').nth(1).is_some_and(|first| first.starts_with("ja"))
        })
        .unwrap_or(false)
}

/// 日本語と英語の組から、今の言語のほうを選ぶ
#[macro_export]
macro_rules! l {
    ($ja:literal, $en:literal) => {
        if $crate::i18n::en() { $en } else { $ja }
    };
}

/// format! の日本語と英語の組
#[macro_export]
macro_rules! lf {
    ($ja:literal, $en:literal $(, $a:expr)* $(,)?) => {
        if $crate::i18n::en() { format!($en $(, $a)*) } else { format!($ja $(, $a)*) }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn picks_language_and_captures_names() {
        let n = 3;
        super::EN.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(crate::lf!("{n} 個", "{n} items"), "3 items");
        assert_eq!(crate::lf!("{} 個", "{} items", n + 1), "4 items");
        assert_eq!(crate::l!("閉じる", "Close"), "Close");
        super::EN.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(crate::lf!("{n} 個", "{n} items"), "3 個");
    }
}
