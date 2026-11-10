use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// ctrl-<文字> のキー
#[derive(Debug, Clone, Copy)]
pub struct KeySpec {
    ch: char,
}

impl KeySpec {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        let rest = s.strip_prefix("ctrl-").or_else(|| s.strip_prefix("ctrl+"))?;
        let mut it = rest.chars();
        let ch = it.next()?;
        if it.next().is_some() || ctrl_byte(ch).is_none() {
            return None;
        }
        Some(Self { ch })
    }

    pub fn label(&self) -> String {
        format!("Ctrl-{}", self.ch.to_ascii_uppercase())
    }

    pub fn byte(&self) -> u8 {
        ctrl_byte(self.ch).unwrap_or(0x1d)
    }

    pub fn matches(&self, k: &KeyEvent) -> bool {
        if !k.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match k.code {
            // 古い端末では Ctrl-\ ] ^ _ が Ctrl-4..7 として届く
            KeyCode::Char(c) => ctrl_byte(c) == Some(self.byte()),
            _ => false,
        }
    }
}

fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        ' ' | '@' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// キー入力を端末に送るバイト列にする
pub fn encode(k: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let m = k.modifiers;
    let shift = m.contains(KeyModifiers::SHIFT);
    let alt = m.contains(KeyModifiers::ALT);
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let modnum = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;

    let csi_mod = |final_: char| -> Vec<u8> {
        if modnum > 1 {
            format!("\x1b[1;{modnum}{final_}").into_bytes()
        } else if app_cursor && "ABCDHF".contains(final_) {
            format!("\x1bO{final_}").into_bytes()
        } else {
            format!("\x1b[{final_}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if modnum > 1 {
            format!("\x1b[{n};{modnum}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    // 修飾キーをCSIの中に含めるもの（ESCを前に付けない）
    let with_mods = match k.code {
        KeyCode::Up => Some(csi_mod('A')),
        KeyCode::Down => Some(csi_mod('B')),
        KeyCode::Right => Some(csi_mod('C')),
        KeyCode::Left => Some(csi_mod('D')),
        KeyCode::Home => Some(csi_mod('H')),
        KeyCode::End => Some(csi_mod('F')),
        KeyCode::PageUp => Some(tilde(5)),
        KeyCode::PageDown => Some(tilde(6)),
        KeyCode::Delete => Some(tilde(3)),
        KeyCode::Insert => Some(tilde(2)),
        KeyCode::F(n) => Some(match n {
            1..=4 if modnum == 1 => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
            1..=4 => format!("\x1b[1;{modnum}{}", (b'P' + n - 1) as char).into_bytes(),
            5 => tilde(15),
            6 => tilde(17),
            7 => tilde(18),
            8 => tilde(19),
            9 => tilde(20),
            10 => tilde(21),
            11 => tilde(23),
            12 => tilde(24),
            _ => return None,
        }),
        _ => None,
    };
    if with_mods.is_some() {
        return with_mods;
    }

    let body: Vec<u8> = match k.code {
        KeyCode::Char(c) if ctrl => match ctrl_byte(c) {
            Some(b) => vec![b],
            None => c.to_string().into_bytes(),
        },
        KeyCode::Char(c) => c.to_string().into_bytes(),
        // Shift+Enter は Claude Code などで改行になる ESC CR を送る
        KeyCode::Enter if shift => b"\x1b\r".to_vec(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace if ctrl => vec![0x08],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        _ => return None,
    };
    if alt && !matches!(k.code, KeyCode::Enter if shift) {
        let mut v = vec![0x1b];
        v.extend(body);
        Some(v)
    } else {
        Some(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn prefix_matches_both_forms() {
        let p = KeySpec::parse("ctrl-]").unwrap();
        assert!(p.matches(&key(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(p.matches(&key(KeyCode::Char('5'), KeyModifiers::CONTROL)));
        assert!(!p.matches(&key(KeyCode::Char(']'), KeyModifiers::NONE)));
        assert_eq!(p.byte(), 0x1d);
    }

    #[test]
    fn encodes_common_keys() {
        assert_eq!(encode(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), false).unwrap(), vec![3]);
        assert_eq!(encode(&key(KeyCode::Up, KeyModifiers::NONE), false).unwrap(), b"\x1b[A");
        assert_eq!(encode(&key(KeyCode::Up, KeyModifiers::NONE), true).unwrap(), b"\x1bOA");
        assert_eq!(encode(&key(KeyCode::Left, KeyModifiers::ALT), false).unwrap(), b"\x1b[1;3D");
        assert_eq!(encode(&key(KeyCode::Enter, KeyModifiers::SHIFT), false).unwrap(), b"\x1b\r");
        assert_eq!(encode(&key(KeyCode::Char('b'), KeyModifiers::ALT), false).unwrap(), b"\x1bb");
        assert_eq!(encode(&key(KeyCode::Char('あ'), KeyModifiers::NONE), false).unwrap(), "あ".as_bytes());
    }
}
