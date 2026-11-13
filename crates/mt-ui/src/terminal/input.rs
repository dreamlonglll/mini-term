//! 键盘 / 粘贴 → PTY 字节。
//!
//! xterm.js 白送的一块:把 `KeyDownEvent` 翻成终端认识的转义序列。这里按
//! xterm 的编码约定实现,覆盖日常够用的范围;Kitty keyboard protocol
//! (`TermMode::DISAMBIGUATE_ESC_CODES` 那一族)本轮**不做**,alacritty 会
//! 老老实实按 legacy 编码工作。
//!
//! Windows 还有一条:ConPTY 请求 `CSI ? 9001h`(win32-input-mode)之后,
//! `Shift+Enter` / `Ctrl+J` 和 conhost 会挂起的传统前缀(`Esc`、`Alt+Backspace`、
//! `Alt+[` 等)改由 [`win32_input_bytes`] 编码成 `CSI Vk;Sc;Uc;Kd;Cs;Rc _`。其余按键
//! 继续走 legacy 字节,`Enter` / `Ctrl+C` / 方向键等行为不变。文本输入仍走平台通道。
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
    // macOS 的 ⌘← / ⌘→ 对应 Emacs 行首 / 行尾。不能直接换成 Home / End:
    // 干净的 zsh Emacs keymap 没有绑定那些 CSI / SS3 序列(issue #98)。
    if cfg!(target_os = "macos")
        && m.platform
        && !m.control
        && !m.alt
        && !m.shift
        && !m.function
    {
        match key {
            "left" => return control_code("a"),
            "right" => return control_code("e"),
            _ => {}
        }
    }
    // 其他 Win/Cmd 组合留给应用层快捷键。
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

/// win32-input-mode 的按键记录:`CSI Vk;Sc;Uc;Kd;Cs;Rc _`。`None` 表示这个键走传统字节。
///
/// 只有两类键发记录,其余键继续走原有字节,`Enter` / `Ctrl+C` / 方向键 /
/// `ReadConsoleInput` 消费者的行为都不变:
///
/// - 传统编码表达不了的 `Shift+Enter` / `Ctrl+J`。`Ctrl+J` 的 `Uc` 取 `0x0A`(LF):
///   Node raw mode / `_getwch` / .NET 这类直接读控制台记录的程序依赖非零 `Uc`。
///   OMP 的 WSI 解码器需要上游放宽 `(shift || uc === 0)` 才会把它还原成 `ctrl+j`。
/// - 传统字节会被 conhost 挂起的键,见 `held_prefix_record`。
pub fn win32_input_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    if !cfg!(windows) {
        return None;
    }
    win32_input_bytes_inner(keystroke)
}

/// 平台无关的编码主体;测试在非 Windows 上也能覆盖这些记录的字节形态。
fn win32_input_bytes_inner(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    if m.function || m.platform {
        return None;
    }
    match (keystroke.key.as_str(), m.shift, m.control, m.alt) {
        // Shift+Enter:VK_RETURN / Sc 0x1C / Uc CR / Shift。
        ("enter", true, false, false) => Some(b"\x1b[13;28;13;1;16;1_".to_vec()),
        // Ctrl+J:VK_J / Sc 0x24 / Uc LF / Ctrl。Uc 不能填 0,否则 Node raw 等
        // 直接读控制台记录的程序会丢键。
        ("j", false, true, false) => Some(b"\x1b[74;36;10;1;8;1_".to_vec()),
        // mode 只决定方向键用 CSI 还是 SS3,那些序列本来就完整,这里用不上。
        _ => keystroke_to_bytes(keystroke, TermMode::empty())
            .and_then(|legacy| held_prefix_record(&legacy))
            .map(<[u8]>::to_vec),
    }
}

/// 传统字节 → 等价的 WSI 记录,只收 conhost 处理不好的前缀形态。
///
/// conhost 收到过一条 WSI 记录后,就不再把写入末尾的不完整序列当按键冲掉。裸 `ESC`
/// 与 `ESC` + `[ O DEL` 会挂到下一键,把它吞掉或变成 Alt+下一键;`ESC` + `] P X ^ _`
/// 更会吞掉之后的所有按键,直到下一条 Esc 记录才恢复。`ESC \` 不挂起,但便携 conhost
/// 会把它拆成两条没有 VK 的记录,一并换掉。
///
/// 这张表是把 `ESC` + 每个 ASCII 字节喂给便携与系统 conhost 扫出来的,没列的形态
/// (`Alt+.`、`Alt+字母` 等)与没见过 WSI 记录时一致。
///
/// 按传统字节查表而不是按键名,`Esc` / `Ctrl+[` / `Shift+Esc` / `Ctrl+Alt+[` 这些
/// 同字节的键才不会漏。VK / Sc 按美式布局写死,其他布局上读控制台记录的程序看到的
/// VK 可能与实际键位不符。
fn held_prefix_record(legacy: &[u8]) -> Option<&'static [u8]> {
    let record: &'static [u8] = match legacy {
        // VK_ESCAPE / Sc 0x01 / Uc ESC / 无修饰。
        b"\x1b" => b"\x1b[27;1;27;1;0;1_",
        // 以下都带 LEFT_ALT(Cs=2),要按 Shift 才打得出的再加 SHIFT(Cs=18)。
        b"\x1b[" => b"\x1b[219;26;91;1;2;1_",
        b"\x1b]" => b"\x1b[221;27;93;1;2;1_",
        b"\x1b\\" => b"\x1b[220;43;92;1;2;1_",
        b"\x1bO" => b"\x1b[79;24;79;1;18;1_",
        b"\x1bP" => b"\x1b[80;25;80;1;18;1_",
        b"\x1bX" => b"\x1b[88;45;88;1;18;1_",
        b"\x1b^" => b"\x1b[54;7;94;1;18;1_",
        b"\x1b_" => b"\x1b[189;12;95;1;18;1_",
        // Alt+Backspace:Uc 与真实键盘一样是 BS,conhost 自己翻成 `ESC DEL`。
        b"\x1b\x7f" => b"\x1b[8;14;8;1;2;1_",
        _ => return None,
    };
    Some(record)
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
///
/// bracketed 编码(含剔除正文里的开始 / 结束标记,防粘贴内容提前「结束粘贴」、
/// 后半段被当按键执行)与移动端指令共用 [`mt_core::paste::bracketed_paste`]。
pub fn paste_to_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        mt_core::paste::bracketed_paste(text).into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
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
    fn command_左右箭头仅在_macos_发送行首行尾控制码() {
        for mode in [TermMode::empty(), TermMode::APP_CURSOR] {
            for (name, byte) in [("left", 0x01), ("right", 0x05)] {
                let k = key(name, Modifiers { platform: true, ..Default::default() });
                assert!(!is_text_input_key(&k));
                let expected = if cfg!(target_os = "macos") { Some(vec![byte]) } else { None };
                assert_eq!(keystroke_to_bytes(&k, mode), expected, "{name} {mode:?}");
            }
        }
    }

    #[test]
    fn command_左右箭头带额外修饰键不触发行首行尾() {
        for bits in 1..16 {
            let modifiers = Modifiers {
                platform: true,
                control: bits & 1 != 0,
                alt: bits & 2 != 0,
                shift: bits & 4 != 0,
                function: bits & 8 != 0,
            };
            for name in ["left", "right"] {
                for mode in [TermMode::empty(), TermMode::APP_CURSOR] {
                    assert_eq!(keystroke_to_bytes(&key(name, modifiers), mode), None,
                        "{name} modifiers={modifiers:?} {mode:?}");
                }
            }
        }
    }

    #[test]
    fn command_其他按键仍留给应用层() {
        for name in ["up", "down", "home", "end", "a", "e", "c", "v", "t", "tab", "enter"] {
            let k = key(name, Modifiers { platform: true, ..Default::default() });
            assert_eq!(keystroke_to_bytes(&k, TermMode::empty()), None, "{name}");
        }
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
        // 删掉一个标记后前后两段会重新拼出 `ESC[201~`:也得去掉
        let out = paste_to_bytes("a\x1b[201\x1b[201~~; id\n", TermMode::BRACKETED_PASTE);
        assert_eq!(out, b"\x1b[200~a; id\r\x1b[201~".to_vec());
        // 开始标记同样剔掉
        let out = paste_to_bytes("a\x1b[200~b", TermMode::BRACKETED_PASTE);
        assert_eq!(out, b"\x1b[200~ab\x1b[201~".to_vec());
    }

    fn mods(control: bool, shift: bool, alt: bool, platform: bool) -> Modifiers {
        Modifiers {
            control,
            shift,
            alt,
            platform,
            ..Default::default()
        }
    }

    /// 传统编码表达不了的两个键。
    #[test]
    fn wsi_编码_shift_enter_与_ctrl_j() {
        assert_eq!(
            win32_input_bytes_inner(&key("enter", mods(false, true, false, false))).unwrap(),
            b"\x1b[13;28;13;1;16;1_".to_vec()
        );
        assert_eq!(
            win32_input_bytes_inner(&key("j", Modifiers::control())).unwrap(),
            b"\x1b[74;36;10;1;8;1_".to_vec()
        );
    }

    /// 传统字节同为裸 ESC 的键都发 Esc 记录,否则 conhost 会把它挂到下一键。
    #[test]
    fn wsi_裸_esc_一律发记录() {
        for (label, name, m) in [
            ("Esc", "escape", Modifiers::default()),
            ("Shift+Esc", "escape", mods(false, true, false, false)),
            ("Alt+Esc", "escape", Modifiers::alt()),
            ("Ctrl+[", "[", Modifiers::control()),
            ("Ctrl+Alt+[", "[", mods(true, false, true, false)),
        ] {
            assert_eq!(
                win32_input_bytes_inner(&key(name, m)).unwrap(),
                b"\x1b[27;1;27;1;0;1_".to_vec(),
                "{label}"
            );
        }
    }

    /// conhost 处理不好的 Alt 前缀(名单见 `held_prefix_record`)。键名与 key_char 按
    /// gpui 的给法写:字母键 key 是小写、key_char 才是大写;符号键的 Shift 已折进 key。
    #[test]
    fn wsi_alt_前缀发带真实_vk_的记录() {
        let alt_shift_letter = |name: &str, ch: &str| Keystroke {
            modifiers: mods(false, true, true, false),
            key: name.into(),
            key_char: Some(ch.into()),
        };
        for (label, keystroke, record) in [
            ("Alt+[", key("[", Modifiers::alt()), "\x1b[219;26;91;1;2;1_"),
            ("Alt+]", key("]", Modifiers::alt()), "\x1b[221;27;93;1;2;1_"),
            (
                "Alt+\\",
                key("\\", Modifiers::alt()),
                "\x1b[220;43;92;1;2;1_",
            ),
            (
                "Alt+Shift+O",
                alt_shift_letter("o", "O"),
                "\x1b[79;24;79;1;18;1_",
            ),
            (
                "Alt+Shift+P",
                alt_shift_letter("p", "P"),
                "\x1b[80;25;80;1;18;1_",
            ),
            (
                "Alt+Shift+X",
                alt_shift_letter("x", "X"),
                "\x1b[88;45;88;1;18;1_",
            ),
            ("Alt+^", key("^", Modifiers::alt()), "\x1b[54;7;94;1;18;1_"),
            (
                "Alt+_",
                key("_", Modifiers::alt()),
                "\x1b[189;12;95;1;18;1_",
            ),
            (
                "Alt+Backspace",
                key("backspace", Modifiers::alt()),
                "\x1b[8;14;8;1;2;1_",
            ),
        ] {
            assert_eq!(
                win32_input_bytes_inner(&keystroke).unwrap(),
                record.as_bytes(),
                "{label}"
            );
        }
    }

    /// 其余键必须保持传统字节，尤其 Enter / Ctrl+C / 方向键 / Tab / Alt。
    #[test]
    fn wsi_其余按键全部回落传统编码() {
        for (name, m) in [
            ("enter", mods(false, false, false, false)),
            ("enter", mods(true, false, false, false)),
            ("tab", Modifiers::default()),
            ("c", Modifiers::control()),
            ("up", Modifiers::default()),
            ("backspace", Modifiers::default()),
            ("b", Modifiers::alt()),
            // 小写 `ESC o` / `ESC p` 是完整序列,大写才是前缀。
            ("o", Modifiers::alt()),
            ("p", Modifiers::alt()),
            (".", Modifiers::alt()),
            ("p", mods(true, true, false, false)),
            ("c", mods(false, false, false, true)),
            ("unmapped-key", Modifiers::default()),
        ] {
            assert!(
                win32_input_bytes_inner(&key(name, m)).is_none(),
                "{name} 不该走 WSI"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn wsi_非_windows_永不编码() {
        assert!(win32_input_bytes(&key("j", Modifiers::control())).is_none());
        assert!(win32_input_bytes(&key("enter", mods(false, true, false, false))).is_none());
        assert!(win32_input_bytes(&key("escape", Modifiers::default())).is_none());
    }
}
