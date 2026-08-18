//! 键盘 / 粘贴 → PTY 字节。
//!
//! xterm.js 白送的一块:把 `KeyDownEvent` 翻成终端认识的转义序列。这里按
//! xterm 的编码约定实现,覆盖日常够用的范围;Kitty keyboard protocol
//! (`TermMode::DISAMBIGUATE_ESC_CODES` 那一族)本轮**不做**,alacritty 会
//! 老老实实按 legacy 编码工作。
//!
//! 放在 `mt-ui` 而不是 `mt-terminal`,是因为入参是 gpui 的 [`Keystroke`],
//! 而 `mt-terminal` 明确不依赖 gpui。

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// 一次按键要写进 PTY 的字节。`None` 表示这个键终端不消费(交给上层做快捷键)。
pub fn keystroke_to_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    let key = keystroke.key.as_str();

    // Ctrl+Shift+X 一律留给应用层快捷键(复制/粘贴/新建标签…),不进 PTY。
    if m.control && m.shift {
        return None;
    }
    // Win/Cmd 键组合同理。
    if m.platform {
        return None;
    }

    // 方向键 / Home / End 在 DECCKM(APP_CURSOR)下换 SS3 前缀。
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let cursor_seq = |final_byte: char| -> Vec<u8> {
        let prefix = if app_cursor { "\x1bO" } else { "\x1b[" };
        format!("{prefix}{final_byte}").into_bytes()
    };
    // 带修饰键的方向键走 CSI 1;<mod><final>。
    let modifier_param = modifier_param(m.shift, m.alt, m.control);
    let cursor_seq_mod = |final_byte: char| -> Vec<u8> {
        match modifier_param {
            Some(p) => format!("\x1b[1;{p}{final_byte}").into_bytes(),
            None => cursor_seq(final_byte),
        }
    };
    let tilde_seq = |num: u8| -> Vec<u8> {
        match modifier_param {
            Some(p) => format!("\x1b[{num};{p}~").into_bytes(),
            None => format!("\x1b[{num}~").into_bytes(),
        }
    };

    let bytes: Vec<u8> = match key {
        "up" => cursor_seq_mod('A'),
        "down" => cursor_seq_mod('B'),
        "right" => cursor_seq_mod('C'),
        "left" => cursor_seq_mod('D'),
        "home" => cursor_seq_mod('H'),
        "end" => cursor_seq_mod('F'),
        "insert" => tilde_seq(2),
        "delete" => tilde_seq(3),
        "pageup" => tilde_seq(5),
        "pagedown" => tilde_seq(6),
        "f1" => ss3_or_csi('P', modifier_param),
        "f2" => ss3_or_csi('Q', modifier_param),
        "f3" => ss3_or_csi('R', modifier_param),
        "f4" => ss3_or_csi('S', modifier_param),
        "f5" => tilde_seq(15),
        "f6" => tilde_seq(17),
        "f7" => tilde_seq(18),
        "f8" => tilde_seq(19),
        "f9" => tilde_seq(20),
        "f10" => tilde_seq(21),
        "f11" => tilde_seq(23),
        "f12" => tilde_seq(24),
        "enter" => vec![b'\r'],
        "tab" => {
            if m.shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        "escape" => vec![0x1b],
        "backspace" => {
            // 现代终端默认 DEL(0x7f);Ctrl+Backspace 发 0x08(删词)。
            if m.control { vec![0x08] } else { vec![0x7f] }
        }
        "space" => {
            if m.control {
                vec![0x00] // Ctrl+Space = NUL
            } else {
                vec![b' ']
            }
        }
        _ => {
            if m.control {
                control_code(key)?
            } else {
                // 普通可打印字符:用 key_char —— 它才带布局与 Shift 的结果
                // (`shift-1` 的 key 是 "1",key_char 才是 "!")。
                let text = keystroke.key_char.as_deref().unwrap_or(key);
                if text.is_empty() {
                    return None;
                }
                text.as_bytes().to_vec()
            }
        }
    };

    // Alt(Meta)前缀:ESC + 序列。方向键那类自己已经把 modifier 编进去了,
    // 只有这里的「普通字符 + Alt」需要补 ESC。
    if m.alt && !is_escape_sequence(&bytes) {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bytes);
        return Some(out);
    }

    Some(bytes)
}

fn is_escape_sequence(bytes: &[u8]) -> bool {
    bytes.first() == Some(&0x1b)
}

fn ss3_or_csi(final_byte: char, modifier_param: Option<u8>) -> Vec<u8> {
    match modifier_param {
        Some(p) => format!("\x1b[1;{p}{final_byte}").into_bytes(),
        None => format!("\x1bO{final_byte}").into_bytes(),
    }
}

/// xterm 的修饰键参数:1 + shift(1) + alt(2) + ctrl(4)。无修饰返回 `None`。
fn modifier_param(shift: bool, alt: bool, control: bool) -> Option<u8> {
    let mut v = 0;
    if shift {
        v |= 1;
    }
    if alt {
        v |= 2;
    }
    if control {
        v |= 4;
    }
    if v == 0 { None } else { Some(v + 1) }
}

/// Ctrl+字母 / Ctrl+符号 → C0 控制码。
fn control_code(key: &str) -> Option<Vec<u8>> {
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None; // 多字符键名(已在上面处理过)不走这条
    }
    let byte = match c.to_ascii_lowercase() {
        c @ 'a'..='z' => (c as u8) - b'a' + 1,
        '@' => 0x00,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
    Some(vec![byte])
}

/// 粘贴文本 → PTY 字节。开了 bracketed paste 就包上 `ESC[200~ … ESC[201~`。
///
/// 无论哪种模式都要先把 `\r\n` / `\n` 归一成 `\r`:PTY 那头把 `\n` 当作
/// 「换行但不回车」,粘多行会出阶梯。
pub fn paste_to_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if mode.contains(TermMode::BRACKETED_PASTE) {
        // bracketed paste 里不许出现结束标记本身,否则能被粘贴内容劫持。
        let sanitized = normalized.replace("\x1b[201~", "");
        let mut out = Vec::with_capacity(sanitized.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        normalized.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn key(name: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: name.to_string(),
            key_char: Some(name.to_string()),
        }
    }

    #[test]
    fn 方向键跟随_decckm() {
        let k = key("up", Modifiers::default());
        assert_eq!(
            keystroke_to_bytes(&k, TermMode::empty()).unwrap(),
            b"\x1b[A".to_vec()
        );
        assert_eq!(
            keystroke_to_bytes(&k, TermMode::APP_CURSOR).unwrap(),
            b"\x1bOA".to_vec()
        );
    }

    #[test]
    fn ctrl_c_是_0x03() {
        let k = key("c", Modifiers::control());
        assert_eq!(keystroke_to_bytes(&k, TermMode::empty()).unwrap(), vec![3]);
    }

    #[test]
    fn ctrl_shift_留给应用层() {
        let k = key(
            "c",
            Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
        );
        assert!(keystroke_to_bytes(&k, TermMode::empty()).is_none());
    }

    #[test]
    fn 粘贴归一换行并按需加括号() {
        assert_eq!(paste_to_bytes("a\r\nb\nc", TermMode::empty()), b"a\rb\rc");
        assert_eq!(
            paste_to_bytes("ab", TermMode::BRACKETED_PASTE),
            b"\x1b[200~ab\x1b[201~".to_vec()
        );
    }

    #[test]
    fn 粘贴内容不能劫持结束标记() {
        let out = paste_to_bytes("a\x1b[201~b", TermMode::BRACKETED_PASTE);
        assert_eq!(out, b"\x1b[200~ab\x1b[201~".to_vec());
    }
}
