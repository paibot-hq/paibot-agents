use eframe::egui::{Key, Modifiers};

/// 文字以外のキーを端末に送るバイト列にする（文字そのものは Text イベントで来る）
pub fn encode(key: Key, m: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let modnum = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
    let csi = |f: char| -> Vec<u8> {
        if modnum > 1 {
            format!("\x1b[1;{modnum}{f}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{f}").into_bytes()
        } else {
            format!("\x1b[{f}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if modnum > 1 { format!("\x1b[{n};{modnum}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
    };
    let ss3 = |f: char| -> Vec<u8> {
        if modnum > 1 { format!("\x1b[1;{modnum}{f}").into_bytes() } else { format!("\x1bO{f}").into_bytes() }
    };
    let with_alt = |b: u8| if m.alt { vec![0x1b, b] } else { vec![b] };

    Some(match key {
        Key::ArrowUp => csi('A'),
        Key::ArrowDown => csi('B'),
        Key::ArrowRight => csi('C'),
        Key::ArrowLeft => csi('D'),
        Key::Home => csi('H'),
        Key::End => csi('F'),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::Delete => tilde(3),
        Key::Insert => tilde(2),
        Key::F1 => ss3('P'),
        Key::F2 => ss3('Q'),
        Key::F3 => ss3('R'),
        Key::F4 => ss3('S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        // Shift/Option+Enter は Claude Code などで改行になる ESC CR
        Key::Enter if m.shift || m.alt => b"\x1b\r".to_vec(),
        Key::Enter => b"\r".to_vec(),
        Key::Tab if m.shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Backspace => with_alt(if m.ctrl { 0x08 } else { 0x7f }),
        Key::Escape => b"\x1b".to_vec(),
        _ if m.ctrl => with_alt(ctrl_byte(key)?),
        _ => return None,
    })
}

fn ctrl_byte(key: Key) -> Option<u8> {
    let name = key.name();
    let mut it = name.chars();
    if let (Some(c), None) = (it.next(), it.next()) {
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_uppercase() as u8 - b'A' + 1);
        }
    }
    Some(match key {
        Key::Space => 0,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        Key::Minus | Key::Slash => 0x1f,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_common_keys() {
        let none = Modifiers::NONE;
        assert_eq!(encode(Key::C, Modifiers::CTRL, false).unwrap(), vec![3]);
        assert_eq!(encode(Key::ArrowUp, none, false).unwrap(), b"\x1b[A");
        assert_eq!(encode(Key::ArrowUp, none, true).unwrap(), b"\x1bOA");
        assert_eq!(encode(Key::ArrowLeft, Modifiers::ALT, false).unwrap(), b"\x1b[1;3D");
        assert_eq!(encode(Key::Enter, Modifiers::SHIFT, false).unwrap(), b"\x1b\r");
        assert_eq!(encode(Key::Tab, Modifiers::SHIFT, false).unwrap(), b"\x1b[Z");
        assert!(encode(Key::A, none, false).is_none());
    }
}
