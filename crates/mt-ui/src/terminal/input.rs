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

/// 这个按键该不该**让给平台的文本输入通道**(IME / `WM_CHAR`)。
///
/// # 为什么必须有这道分流
///
/// gpui 在 Windows 上是这样走的:`WM_KEYDOWN` 先派发成 `KeyDownEvent`,**只有在
/// 没人 `stop_propagation` 时**才调 `TranslateMessage`,进而产生 `WM_CHAR` /
/// IME 的 `WM_IME_COMPOSITION`。于是同一次按键有两条可能的出口:
///
/// - 走 `KeyDownEvent`:拿得到 `key_char`,但 **IME 完全被绕过** —— 中文输入法下
///   按 `n` 会既写一个 `n` 进 PTY,又在候选框里开始组合,一个字变两个;
/// - 走 `replace_text_in_range`:IME 组合、候选、上屏全都正常,代价是必须让
///   `KeyDownEvent` 原样冒泡出去。
///
/// 所以「可打印字符」一律走后者,其余(方向键 / 功能键 / Ctrl 组合 / Enter / Tab /
/// Esc / Backspace)走前者并 `stop_propagation`。这条分界与 gpui 的
/// `parse_char_message` 正好对齐:它会把控制字符(0x00-0x1F、0x7F)过滤掉,
/// 也就是说 Enter/Tab/Esc/Ctrl+字母 **本来就不会**从文本通道回来,不存在漏键。
///
/// 判据:单字符键名 + 无 Ctrl / Alt / Win。
/// - `space` / `enter` / `up` 这类**多字符键名**归 [`keystroke_to_bytes`];
/// - Alt 组合要发 `ESC` 前缀(Meta 语义),文本通道给不出来,也归按键路径;
/// - Ctrl 组合要发 C0 控制码,同上。
pub fn is_text_input_key(keystroke: &Keystroke) -> bool {
    let m = &keystroke.modifiers;
    if m.platform || m.function {
        return false;
    }
    // AltGr 的例外:Windows 把 AltGr 报成 Ctrl+Alt,而德语的 `@`(AltGr+Q)、
    // 波兰语的 `ą`(AltGr+A)、法语的 `€`(AltGr+E) 全走这条。判据是 gpui 有没有
    // 给出 `key_char` —— 它是 `ToUnicode` 的结果并**已经把控制字符过滤掉**,
    // 所以真正的 Ctrl+组合(结果是 0x01..0x1A)在这里一律拿不到 key_char,
    // 不会被误判成文本。
    if m.control && m.alt {
        return keystroke
            .key_char
            .as_deref()
            .is_some_and(|s| !s.is_empty() && !s.chars().any(char::is_control));
    }
    if m.control || m.alt {
        return false;
    }
    let mut chars = keystroke.key.chars();
    chars.next().is_some() && chars.next().is_none()
}

/// 一次按键要写进 PTY 的字节。`None` 表示这个键终端不消费(交给上层做快捷键)。
///
/// **注意**:这个函数对可打印字符仍然会给出字节(键盘直通语义,给不接 IME 的
/// 调用方兜底)。接了 IME 的调用方必须先过 [`is_text_input_key`] 分流,
/// 否则可打印字符会被写两遍 —— 一遍这里,一遍 `replace_text_in_range`。
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

    // 方向键 / Home / End 在 DECCKM(APP_CURSOR)下换 SS3 前缀。编码收口在
    // [`cursor_key_bytes`],这里只是把 `app_cursor` 绑进去。
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let cursor_seq = |final_byte: char| -> Vec<u8> { cursor_key_bytes(final_byte, app_cursor) };
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

/// 一个方向键。见 [`arrow_bytes`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrow {
    Up,
    Down,
    Left,
    Right,
}

/// 光标键序列的编码:CSI `ESC [ <final>`,DECCKM(`APP_CURSOR`)下换 SS3
/// `ESC O <final>`。方向键与 Home / End 共用这一条规则。
///
/// **全仓只有这一处知道 SS3 那件事**。此前它散在三个地方各写一份
/// (`keystroke_to_bytes` 的 `cursor_seq` 闭包、[`arrow_bytes`]、
/// [`super::mouse::alt_screen_scroll_bytes`] 的四个硬编码字面量),
/// 谁改了 DECCKM 的判据都不会波及另外两处 —— 收口到这里。
pub fn cursor_key_bytes(final_byte: char, app_cursor: bool) -> Vec<u8> {
    let prefix = if app_cursor { "\x1bO" } else { "\x1b[" };
    format!("{prefix}{final_byte}").into_bytes()
}

/// 一个**无修饰**方向键的字节序列。编码规则见 [`cursor_key_bytes`]。
pub fn arrow_bytes(dir: Arrow, app_cursor: bool) -> Vec<u8> {
    let final_byte = match dir {
        Arrow::Up => 'A',
        Arrow::Down => 'B',
        Arrow::Right => 'C',
        Arrow::Left => 'D',
    };
    cursor_key_bytes(final_byte, app_cursor)
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

    /// 方向键的编码只有 [`cursor_key_bytes`] 一处知道 SS3 那件事,三个调用方必须
    /// 逐字节一致。此前它散成三份(这里的 `cursor_seq` 闭包、[`arrow_bytes`]、
    /// `mouse::alt_screen_scroll_bytes` 的硬编码字面量),改一处波及不到另两处 ——
    /// 上游评审(PR #59)指出过。这条把三条路交叉钉死,DECCKM 两态都比。
    #[test]
    fn 三处方向键编码逐字节一致() {
        for (app_cursor, mode) in [(false, TermMode::empty()), (true, TermMode::APP_CURSOR)] {
            for (name, dir, expect_tail) in [
                ("up", Arrow::Up, 'A'),
                ("down", Arrow::Down, 'B'),
                ("right", Arrow::Right, 'C'),
                ("left", Arrow::Left, 'D'),
            ] {
                let want = cursor_key_bytes(expect_tail, app_cursor);
                assert_eq!(arrow_bytes(dir, app_cursor), want, "arrow_bytes {name}");
                assert_eq!(
                    keystroke_to_bytes(&key(name, Modifiers::default()), mode).unwrap(),
                    want,
                    "keystroke_to_bytes {name} (app_cursor={app_cursor})"
                );
            }
            // 滚轮那条路只用上下两个方向
            assert_eq!(
                crate::terminal::mouse::alt_screen_scroll_bytes(1, app_cursor),
                cursor_key_bytes('A', app_cursor),
                "alt_screen 上滚 (app_cursor={app_cursor})"
            );
            assert_eq!(
                crate::terminal::mouse::alt_screen_scroll_bytes(-1, app_cursor),
                cursor_key_bytes('B', app_cursor),
                "alt_screen 下滚 (app_cursor={app_cursor})"
            );
        }
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
    fn 可打印字符让给_ime_通道() {
        // 裸字母 / 数字 / 符号:交给 replace_text_in_range,否则中文输入法下一个字变两个
        for name in ["a", "Z", "1", "!", "你"] {
            assert!(
                is_text_input_key(&key(name, Modifiers::default())),
                "{name} 应走文本通道"
            );
        }
        // 多字符键名一律走按键路径(gpui 的 parse_char_message 会把控制码滤掉,
        // 指望文本通道送 Enter/Tab/Esc 是收不到的)
        for name in ["enter", "tab", "escape", "backspace", "space", "up", "f5"] {
            assert!(
                !is_text_input_key(&key(name, Modifiers::default())),
                "{name} 应走按键路径"
            );
        }
    }

    #[test]
    fn 带修饰键的字符不走文本通道() {
        // Ctrl+c 要发 0x03、Alt+a 要发 ESC a,文本通道都给不出来
        assert!(!is_text_input_key(&key("c", Modifiers::control())));
        assert!(!is_text_input_key(&key("a", Modifiers::alt())));
        assert!(!is_text_input_key(&key(
            "v",
            Modifiers {
                platform: true,
                ..Default::default()
            }
        )));
        // Shift 不影响:大写字母仍然是文本
        assert!(is_text_input_key(&key(
            "a",
            Modifiers {
                shift: true,
                ..Default::default()
            }
        )));
    }

    #[test]
    fn altgr_算文本_ctrl_alt_组合不算() {
        let ctrl_alt = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        // AltGr+Q 在德语布局上是 `@`:gpui 给得出 key_char,该走文本通道
        let altgr = Keystroke {
            modifiers: ctrl_alt,
            key: "q".into(),
            key_char: Some("@".into()),
        };
        assert!(is_text_input_key(&altgr));

        // 真正的 Ctrl+Alt 组合:ToUnicode 的结果是控制码,已被 gpui 过滤成 None
        let combo = Keystroke {
            modifiers: ctrl_alt,
            key: "a".into(),
            key_char: None,
        };
        assert!(!is_text_input_key(&combo));
    }

    #[test]
    fn 粘贴内容不能劫持结束标记() {
        let out = paste_to_bytes("a\x1b[201~b", TermMode::BRACKETED_PASTE);
        assert_eq!(out, b"\x1b[200~ab\x1b[201~".to_vec());
    }
}
