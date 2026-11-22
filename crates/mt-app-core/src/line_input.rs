//! 终端内的行输入编辑器:SSH 认证阶段(主机密钥确认 / 密码 / OTP)在终端里读一行。
//!
//! 宿主把用户的每次写入(按键 / 粘贴 / IME 上屏)原样喂给 [`LineInput::feed`],把返回的
//! [`LineStep::echo`] 画进终端,拿到 [`LineOutcome`] 就交给 SSH 层。这里只是纯逻辑:
//! 不碰终端、不碰网络。
//!
//! # 口径
//!
//! - 可打印字符(UTF-8,含组合符号与 emoji)追加;`echo` 开着时原样回显;
//! - `\x7f` / `\x08` 删最后一个**字位簇**(`é` 的分解形式、带肤色的 emoji 整个删),
//!   回显按它的显示宽度擦除(`BS…` + 空格 + `BS…`);
//! - `\r` 或 `\n` 提交,回显 `\r\n`;同一次写入里提交之后的字节丢弃;
//! - `\x03`(Ctrl+C)取消,回显 `^C\r\n`;空行上的 `\x04`(Ctrl+D)取消,回显 `\r\n`,
//!   非空行上的 Ctrl+D 不理;
//! - `\x15`(Ctrl+U)清空本行;
//! - 转义序列整条忽略:方向键 / 功能键(CSI、SS3)、焦点事件 `CSI I` / `CSI O`、
//!   win32-input-mode 的 `CSI … _` 记录、Alt+键、OSC / DCS 之类的字符串序列;
//!   写入末尾单独一个 ESC 是 Esc 键,不会吞掉下一次写入;
//! - bracketed paste(`CSI 200~` … `CSI 201~`)里的文本当普通输入,其中的退格、Ctrl+C
//!   这类控制字符一律不认(粘贴是文本,不是按键);粘贴里遇到换行即提交,其后的字节丢弃;
//! - 其余控制字符(Tab、Ctrl+W……)不认;一行最多 [`MAX_LINE_BYTES`] 字节,超出的不收。
//!
//! 提交 / 取消之后编辑器回到空行,可以接着读下一行。
//!
//! ⚠️ 宿主要保证认证期间 win32-input-mode 是关的(`TerminalEmulator::begin_takeover`
//! 会关;远程项目 pane 的 emulator 本来就没开):开着的话回车也是一条 `CSI … _` 记录,
//! 会被当成转义序列忽略掉,用户就提交不了。
//!
//! # 显示宽度
//!
//! 按 Unicode 东亚宽度(`unicode-width` 的字符串口径:CJK 2 列、组合符号 0 列,
//! 认得 emoji 呈现 / 修饰 / ZWJ 序列)。这是本地回显,用户看的是「删掉一个字」,
//! 与 mt-terminal 的落格口径在 ASCII、CJK、常见 emoji 上一致;个别文字(印度系合体等)
//! 可能差一列,只影响回显的擦除位置,不影响交出去的文本。擦除不跨行:回显折到下一行
//! 之后再退格,终端的 BS 退不回上一行(与多数 shell 的 `stty` 回显同一局限)。
//! `unicode-width` / `unicode-segmentation` 都是依赖树里已有的(mt-terminal 在用)。
//!
//! # 明文
//!
//! 缓冲里可能是密码:删掉的字节当场覆写成 0;缓冲自己换更大的一块(旧块先清零),
//! 不让 `String` 扩容把旧块原样还给分配器;取消、清行、Drop 都 `zeroize`。
//! 提交时缓冲**整块移交**给调用方,这边不留副本 —— 移交出去的 `String` 用完怎么清,
//! 是调用方的事(SSH 层可以套 `zeroize::Zeroizing`)。`Debug` 不打印内容。

use std::fmt;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use zeroize::Zeroize;

/// 一行最多收多少字节(UTF-8)。超出的输入不收 —— 给「粘贴了一大段没有换行的东西」兜底。
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// 缓冲的起始容量:一般的密码 / OTP / `yes` 都装得下,不必扩容。
const INITIAL_CAPACITY: usize = 128;

/// 一行读完了。
#[derive(Clone, PartialEq, Eq)]
pub enum LineOutcome {
    /// 回车提交,带着这一行(可能是空串)。
    Submit(String),
    /// Ctrl+C,或空行上的 Ctrl+D。
    Cancel,
}

impl fmt::Debug for LineOutcome {
    /// 不打印内容:里面可能是密码。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Submit(_) => f.write_str("Submit(..)"),
            Self::Cancel => f.write_str("Cancel"),
        }
    }
}

/// 喂一次之后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineStep {
    /// 要画进终端的字节。`echo = false` 时只有换行(以及取消时的 `^C`)。
    pub echo: Vec<u8>,
    /// `Some` = 这一行读完了;这次写入里其后的字节已经丢掉。
    pub outcome: Option<LineOutcome>,
}

/// 转义序列解析到哪了。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Escape {
    /// 不在序列里。
    Ground,
    /// 刚收到 ESC。
    Esc,
    /// `ESC [` 之后,等结束字节。参数只在意是不是纯数字(认 `200~` / `201~`)。
    Csi(CsiParam),
    /// `ESC O` 之后,再一个字节就完。
    Ss3,
    /// OSC / DCS / APC / PM / SOS 正文,等 ST(`ESC \`);OSC 另认 BEL。
    /// `esc` = 上一个字节是 ESC(可能是 ST 的前半)。
    Text { osc: bool, esc: bool },
}

/// CSI 的参数:纯数字时记下数值,掺了别的(`;`、`?`、中间字节)就只记「别的」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CsiParam {
    Number(u32),
    Other,
}

impl CsiParam {
    fn push(self, byte: u8) -> Self {
        match self {
            Self::Number(n) if byte.is_ascii_digit() => n
                .checked_mul(10)
                .and_then(|n| n.checked_add(u32::from(byte - b'0')))
                .map_or(Self::Other, Self::Number),
            _ => Self::Other,
        }
    }
}

/// 逐字节拼 UTF-8。半个字符可以跨两次写入。
#[derive(Default)]
struct Utf8 {
    bytes: [u8; 4],
    len: usize,
    need: usize,
}

impl Utf8 {
    /// 收一个非 ASCII 字节;拼完一个字符就交出来。非法序列整段丢掉。
    fn push(&mut self, byte: u8) -> Option<char> {
        if self.len == 0 {
            self.need = match byte {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                // 孤立的续字节 / 非法首字节
                _ => return None,
            };
        } else if byte & 0xc0 != 0x80 {
            // 上一个字符没收完就来了新的首字节:丢掉旧的,从这个字节重新开始
            self.clear();
            return self.push(byte);
        }
        self.bytes[self.len] = byte;
        self.len += 1;
        if self.len < self.need {
            return None;
        }
        let c = std::str::from_utf8(&self.bytes[..self.len])
            .ok()
            .and_then(|s| s.chars().next());
        self.clear();
        c
    }

    fn clear(&mut self) {
        self.bytes.zeroize();
        self.len = 0;
        self.need = 0;
    }
}

/// 终端内的行输入编辑器,见模块注释。
pub struct LineInput {
    /// 键入的字符要不要回显(密码 / OTP 不回显)。
    echo: bool,
    buffer: String,
    utf8: Utf8,
    escape: Escape,
    /// 在 bracketed paste 里。
    paste: bool,
}

impl fmt::Debug for LineInput {
    /// 不打印缓冲:里面可能是密码。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LineInput")
            .field("echo", &self.echo)
            .field("paste", &self.paste)
            .finish_non_exhaustive()
    }
}

impl Drop for LineInput {
    fn drop(&mut self) {
        self.buffer.zeroize();
        self.utf8.clear();
    }
}

impl LineInput {
    /// `echo = false` 用于密码 / OTP:键入的字符不回显,只回显换行。
    pub fn new(echo: bool) -> Self {
        Self {
            echo,
            buffer: String::with_capacity(INITIAL_CAPACITY),
            utf8: Utf8::default(),
            escape: Escape::Ground,
            paste: false,
        }
    }

    /// 喂一次写入的原始字节(用户按键 / 粘贴)。`echo` 是要画进终端的字节
    /// (`echo = false` 时只有回车换行)。
    pub fn feed(&mut self, bytes: &[u8]) -> LineStep {
        let mut step = LineStep {
            echo: Vec::new(),
            outcome: None,
        };
        for &byte in bytes {
            if let Some(outcome) = self.byte(byte, &mut step.echo) {
                // 同一次写入里提交 / 取消之后的字节丢弃,编辑器回到空行
                step.outcome = Some(outcome);
                self.escape = Escape::Ground;
                self.paste = false;
                self.utf8.clear();
                return step;
            }
        }
        // 写入末尾单独一个 ESC 是 Esc 键本身,不是序列的开头
        if self.escape == Escape::Esc {
            self.escape = Escape::Ground;
        }
        step
    }

    fn byte(&mut self, byte: u8, echo: &mut Vec<u8>) -> Option<LineOutcome> {
        match self.escape {
            Escape::Ground => {}
            Escape::Esc => {
                self.escape = match byte {
                    b'[' => Escape::Csi(CsiParam::Number(0)),
                    b'O' => Escape::Ss3,
                    b']' => Escape::Text {
                        osc: true,
                        esc: false,
                    },
                    b'P' | b'_' | b'^' | b'X' => Escape::Text {
                        osc: false,
                        esc: false,
                    },
                    0x1b => Escape::Esc,
                    // Alt+键:连同这个字节一起丢(多字节字符的续字节随后被当成孤立续字节丢掉)
                    _ => Escape::Ground,
                };
                return None;
            }
            Escape::Csi(param) => match byte {
                // 参数字节
                0x30..=0x3f => {
                    self.escape = Escape::Csi(param.push(byte));
                    return None;
                }
                // 中间字节
                0x20..=0x2f => {
                    self.escape = Escape::Csi(CsiParam::Other);
                    return None;
                }
                // 结束字节
                0x40..=0x7e => {
                    self.escape = Escape::Ground;
                    match (param, byte) {
                        (CsiParam::Number(200), b'~') => self.paste = true,
                        (CsiParam::Number(201), b'~') => self.paste = false,
                        _ => {}
                    }
                    return None;
                }
                // 坏序列:就此作罢,这个字节按普通输入重新处理
                _ => self.escape = Escape::Ground,
            },
            Escape::Ss3 => {
                self.escape = Escape::Ground;
                if (0x20..=0x7e).contains(&byte) {
                    return None;
                }
            }
            Escape::Text { osc, esc } => {
                self.escape = match byte {
                    b'\\' if esc => Escape::Ground,
                    0x07 if osc => Escape::Ground,
                    _ => Escape::Text {
                        osc,
                        esc: byte == 0x1b,
                    },
                };
                return None;
            }
        }

        if byte >= 0x80 {
            if let Some(c) = self.utf8.push(byte) {
                self.printable(c, echo);
            }
            return None;
        }
        // ASCII 打断没拼完的多字节字符
        self.utf8.clear();
        match byte {
            0x1b => self.escape = Escape::Esc,
            b'\r' | b'\n' => {
                echo.extend_from_slice(b"\r\n");
                let line =
                    std::mem::replace(&mut self.buffer, String::with_capacity(INITIAL_CAPACITY));
                return Some(LineOutcome::Submit(line));
            }
            0x20..=0x7e => self.printable(char::from(byte), echo),
            // 粘贴是文本:退格、Ctrl+C 之类的控制字符不当按键
            _ if self.paste => {}
            0x7f | 0x08 => self.backspace(echo),
            0x03 => {
                echo.extend_from_slice(b"^C\r\n");
                self.buffer.zeroize();
                return Some(LineOutcome::Cancel);
            }
            0x04 if self.buffer.is_empty() => {
                echo.extend_from_slice(b"\r\n");
                return Some(LineOutcome::Cancel);
            }
            0x15 => self.clear_line(echo),
            _ => {}
        }
        None
    }

    /// 追加一个可打印字符。C1 控制字符(U+0080..U+009F)不收;满了不收。
    fn printable(&mut self, c: char, echo: &mut Vec<u8>) {
        let needed = self.buffer.len() + c.len_utf8();
        if c.is_control() || needed > MAX_LINE_BYTES {
            return;
        }
        if needed > self.buffer.capacity() {
            // 自己换一块更大的:让 String 自己扩容的话,旧块原样还给分配器,明文还躺在里面
            let mut grown = String::with_capacity(needed.max(self.buffer.capacity() * 2));
            grown.push_str(&self.buffer);
            self.buffer.zeroize();
            self.buffer = grown;
        }
        self.buffer.push(c);
        if self.echo {
            let mut utf8 = [0; 4];
            echo.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
        }
    }

    /// 删最后一个字位簇,删掉的字节当场覆写成 0。
    fn backspace(&mut self, echo: &mut Vec<u8>) {
        let Some((start, cluster)) = self.buffer.grapheme_indices(true).next_back() else {
            return;
        };
        let width = cluster.width();
        let removed = self.buffer.len() - start;
        // 等长替换就地覆写,不会重新分配
        self.buffer.replace_range(start.., &"\0".repeat(removed));
        self.buffer.truncate(start);
        if self.echo {
            erase(echo, width);
        }
    }

    /// Ctrl+U:清空本行。
    fn clear_line(&mut self, echo: &mut Vec<u8>) {
        if self.echo {
            let width = self
                .buffer
                .graphemes(true)
                .map(UnicodeWidthStr::width)
                .sum();
            erase(echo, width);
        }
        self.buffer.zeroize();
    }
}

/// 擦掉光标左边 `width` 列:退回去、写空格盖掉、再退回去。
fn erase(echo: &mut Vec<u8>, width: usize) {
    echo.extend(std::iter::repeat_n(b'\x08', width));
    echo.extend(std::iter::repeat_n(b' ', width));
    echo.extend(std::iter::repeat_n(b'\x08', width));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submitted(step: &LineStep) -> Option<&str> {
        match &step.outcome {
            Some(LineOutcome::Submit(line)) => Some(line.as_str()),
            _ => None,
        }
    }

    /// `width` 列的擦除序列。
    fn erased(width: usize) -> Vec<u8> {
        let mut out = Vec::new();
        erase(&mut out, width);
        out
    }

    #[test]
    fn 可打印字符回显并回车提交() {
        let mut input = LineInput::new(true);
        let step = input.feed(b"yes\r");
        assert_eq!(step.echo, b"yes\r\n");
        assert_eq!(submitted(&step), Some("yes"));

        // `\n` 同样提交;中文照收
        let step = input.feed("确认\n".as_bytes());
        assert_eq!(step.echo, "确认\r\n".as_bytes());
        assert_eq!(submitted(&step), Some("确认"));
    }

    #[test]
    fn 不回显时只回显换行() {
        let mut input = LineInput::new(false);
        let step = input.feed(b"hunter2");
        assert!(step.echo.is_empty());
        assert_eq!(step.outcome, None);
        let step = input.feed(b"\r");
        assert_eq!(step.echo, b"\r\n");
        assert_eq!(submitted(&step), Some("hunter2"));
    }

    #[test]
    fn 回车打在空行上提交空串() {
        let mut input = LineInput::new(true);
        let step = input.feed(b"\r");
        assert_eq!(submitted(&step), Some(""));
    }

    #[test]
    fn 退格删最后一个字符并按宽度擦除() {
        let mut input = LineInput::new(true);
        let step = input.feed(b"abx\x7f");
        let mut expected = b"abx".to_vec();
        expected.extend(erased(1));
        assert_eq!(step.echo, expected);
        // `\x08` 也是退格
        let step = input.feed(b"y\x08c\r");
        let mut expected = b"y".to_vec();
        expected.extend(erased(1));
        expected.extend(b"c\r\n");
        assert_eq!(step.echo, expected);
        assert_eq!(submitted(&step), Some("abc"));
    }

    #[test]
    fn 退格按东亚宽度擦除且删整个字位簇() {
        let mut input = LineInput::new(true);
        input.feed("中文".as_bytes());
        assert_eq!(input.feed(b"\x7f").echo, erased(2), "CJK 占 2 列");
        // 带肤色修饰符的 emoji 是一个簇、2 列;分解形式的 é 是一个簇、1 列
        input.feed("👍🏽e\u{301}".as_bytes());
        assert_eq!(input.feed(b"\x7f").echo, erased(1));
        assert_eq!(input.feed(b"\x7f").echo, erased(2));
        assert_eq!(submitted(&input.feed(b"\r")), Some("中"));
    }

    #[test]
    fn 空行上退格什么都不做() {
        let mut input = LineInput::new(true);
        let step = input.feed(b"\x7f\x08");
        assert!(step.echo.is_empty());
        assert_eq!(step.outcome, None);
    }

    #[test]
    fn ctrl_c_取消且丢掉其后的字节() {
        let mut input = LineInput::new(true);
        let step = input.feed(b"ab\x03xyz\r");
        assert_eq!(step.echo, b"ab^C\r\n");
        assert_eq!(step.outcome, Some(LineOutcome::Cancel));
        // 回到空行:之前的 ab 与被丢掉的 xyz 都不在
        assert_eq!(submitted(&input.feed(b"q\r")), Some("q"));

        let mut secret = LineInput::new(false);
        assert_eq!(secret.feed(b"pw\x03").echo, b"^C\r\n");
    }

    #[test]
    fn ctrl_d_只在空行上取消() {
        let mut input = LineInput::new(true);
        let step = input.feed(b"\x04");
        assert_eq!(step.echo, b"\r\n");
        assert_eq!(step.outcome, Some(LineOutcome::Cancel));

        let step = input.feed(b"a\x04b\r");
        assert_eq!(submitted(&step), Some("ab"), "非空行上的 Ctrl+D 不理");
    }

    #[test]
    fn ctrl_u_清空本行() {
        let mut input = LineInput::new(true);
        let step = input.feed("ab中\x15d\r".as_bytes());
        let mut expected = "ab中".as_bytes().to_vec();
        expected.extend(erased(4));
        expected.extend(b"d\r\n");
        assert_eq!(step.echo, expected);
        assert_eq!(submitted(&step), Some("d"));

        let mut secret = LineInput::new(false);
        let step = secret.feed(b"wrong\x15right\r");
        assert_eq!(step.echo, b"\r\n");
        assert_eq!(submitted(&step), Some("right"));
    }

    #[test]
    fn 转义序列整条忽略() {
        let mut input = LineInput::new(true);
        // 方向键、F1(SS3)、F5、焦点进出、win32-input-mode 记录、Alt+x、应用光标键、OSC
        let step = input
            .feed(b"o\x1b[A\x1bOP\x1b[15~\x1b[I\x1b[Ok\x1b[65;30;97;1;0;1_\x1bx\x1bOD\x1b]0;t\x07");
        assert_eq!(step.echo, b"ok");
        assert_eq!(step.outcome, None);
        // 写入末尾单独一个 ESC 是 Esc 键,不吞下一次写入的第一个字节
        assert!(input.feed(b"\x1b").echo.is_empty());
        let step = input.feed(b"!\r");
        assert_eq!(submitted(&step), Some("ok!"));
    }

    #[test]
    fn 跨两次写入的序列与字符照样认得() {
        let mut input = LineInput::new(true);
        input.feed(b"\x1b[1;5");
        input.feed(b"Ca");
        let wide = "中".as_bytes();
        input.feed(&wide[..1]);
        input.feed(&wide[1..]);
        assert_eq!(submitted(&input.feed(b"\r")), Some("a中"));
    }

    #[test]
    fn 非法字节与_c1_控制字符不收() {
        let mut input = LineInput::new(true);
        // 孤立续字节、非法首字节、没拼完就被 ASCII 打断、C1 的 NEL(U+0085)
        let step = input.feed(b"\x80a\xffb\xe4\xb8c\xc2\x85d\r");
        assert_eq!(step.echo, b"abcd\r\n");
        assert_eq!(submitted(&step), Some("abcd"));
    }

    #[test]
    fn 其余控制字符不收() {
        let mut input = LineInput::new(true);
        let step = input.feed(b"a\tb\x17\x01c\r");
        assert_eq!(submitted(&step), Some("abc"));
    }

    #[test]
    fn bracketed_paste_里的文本当普通输入() {
        let mut input = LineInput::new(true);
        // 粘贴里的退格与 Ctrl+C 是文本不是按键,不认;粘贴结束后照常编辑
        let step = input.feed(b"\x1b[200~pa\x7fs\x03te\x1b[201~d\r");
        assert_eq!(step.echo, b"pasted\r\n");
        assert_eq!(submitted(&step), Some("pasted"));

        // 开始标记跨两次写入
        input.feed(b"\x1b[20");
        let step = input.feed(b"0~x\x08\x1b[201~\r");
        assert_eq!(submitted(&step), Some("x"));
    }

    #[test]
    fn 粘贴里遇到换行即提交_其后丢弃() {
        let mut input = LineInput::new(false);
        let step = input.feed(b"\x1b[200~line1\r\nline2\x1b[201~");
        assert_eq!(step.echo, b"\r\n");
        assert_eq!(submitted(&step), Some("line1"));
        // 粘贴状态随提交复位:下一行的退格照常是退格
        let step = input.feed(b"ab\x7f\r");
        assert_eq!(submitted(&step), Some("a"));
    }

    #[test]
    fn 同一次写入里提交之后的字节丢弃() {
        let mut input = LineInput::new(true);
        let step = input.feed(b"a\rbcd");
        assert_eq!(step.echo, b"a\r\n");
        assert_eq!(submitted(&step), Some("a"));
        assert_eq!(submitted(&input.feed(b"\r")), Some(""));
    }

    #[test]
    fn 超长输入封顶且扩容后内容不乱() {
        let mut input = LineInput::new(false);
        // 越过起始容量,走自己换块的那条路
        let long: String = (0..INITIAL_CAPACITY * 3)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let step = input.feed(format!("{long}\r").as_bytes());
        assert_eq!(submitted(&step), Some(long.as_str()));

        let flood = vec![b'x'; MAX_LINE_BYTES + 100];
        input.feed(&flood);
        let step = input.feed(b"\r");
        assert_eq!(submitted(&step).map(str::len), Some(MAX_LINE_BYTES));
    }

    #[test]
    fn debug_不打印内容() {
        let mut input = LineInput::new(false);
        input.feed(b"topsecret");
        assert!(!format!("{input:?}").contains("topsecret"));
        let step = input.feed(b"\r");
        assert!(!format!("{step:?}").contains("topsecret"));
    }
}
