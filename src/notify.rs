use std::process::{Command, Stdio};

/// macOS の通知を出す（ほかのOSでは何もしない）
pub fn send(title: &str, body: &str) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"{}\" subtitle \"Paibot Agents\" sound name \"Glass\"",
        esc(body),
        esc(title)
    );
    if let Ok(mut child) = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
