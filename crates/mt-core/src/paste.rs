//! bracketed paste 的编码 —— 桌面端粘贴(mt-ui `paste_to_bytes`)与移动端指令
//! (mt-relay `mobile_command_body`)共用这一份。
//!
//! 程序开了 bracketed paste(`CSI ? 2004 h`)时,粘贴内容要包成
//! `ESC[200~ … ESC[201~`,shell / TUI 据此把中间整段当文本而不是按键。正文里要是
//! 自带结束标记,粘贴就会在那里提前结束,后半段被当成按键执行 —— 所以正文里的
//! 开始 / 结束标记都得去掉。
//!
//! 只做一次 `replace` 是不够的:`ESC[201ESC[201~~` 删掉中间那个标记后,前后两段
//! 正好重新拼成一个 `ESC[201~`。这里改成逐字符写出、每写一个字符就看输出末尾是否
//! 凑成了标记,凑成就截掉:拼出来的标记会在写入它最后一个字符时同样被截掉。
//! 整个过程单遍、线性 —— 不能写成「删一次再从头查」的循环,构造好的输入能让那种
//! 写法在界面线程上跑成平方级。
//!
//! 移植自 Winc(Apache-2.0)`crates/winc-pty/src/host/input.rs` 的 `paste`。

/// bracketed paste 的开始标记 `ESC[200~`。
pub const BRACKETED_PASTE_START: &str = "\x1b[200~";
/// bracketed paste 的结束标记 `ESC[201~`。
pub const BRACKETED_PASTE_END: &str = "\x1b[201~";

/// 两个标记同长,前 4 字节相同,只差第 5 字节的 `0` / `1`。
const MARKER_LEN: usize = 6;

/// 粘贴正文 → bracketed paste 编码:换行归一成 `\r`(`\r\n` 与单个 `\n` 都算一次
/// 回车,PTY 那头把 `\n` 当「换行不回车」,多行会出阶梯),去掉正文里的开始 /
/// 结束标记(含去掉之后重新拼出来的),再包上 `ESC[200~ … ESC[201~`。
pub fn bracketed_paste(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2 * MARKER_LEN);
    out.push_str(BRACKETED_PASTE_START);
    // 截断只许发生在正文里,不能吃到上面刚写的开始标记
    let body_start = out.len();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let c = match c {
            '\r' if chars.peek() == Some(&'\n') => continue,
            '\n' => '\r',
            c => c,
        };
        out.push(c);
        if c == '~' {
            strip_trailing_marker(&mut out, body_start);
        }
    }
    out.push_str(BRACKETED_PASTE_END);
    out
}

/// `out` 的末尾(不早于 `body_start`)正好是一个开始 / 结束标记时把它截掉。
///
/// 末尾 6 字节等于标记时,截断点上是 `ESC`(ASCII),一定落在字符边界上。
fn strip_trailing_marker(out: &mut String, body_start: usize) {
    let Some(tail) = out.len().checked_sub(MARKER_LEN) else {
        return;
    };
    if tail < body_start {
        return;
    }
    let last = &out.as_bytes()[tail..];
    if last == BRACKETED_PASTE_START.as_bytes() || last == BRACKETED_PASTE_END.as_bytes() {
        out.truncate(tail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(body: &str) -> String {
        format!("{BRACKETED_PASTE_START}{body}{BRACKETED_PASTE_END}")
    }

    /// 包装层之外正文里不许再出现任何标记。
    fn assert_body_clean(encoded: &str) {
        let body = encoded
            .strip_prefix(BRACKETED_PASTE_START)
            .and_then(|s| s.strip_suffix(BRACKETED_PASTE_END))
            .expect("必须包着开始 / 结束标记");
        assert!(
            !body.contains(BRACKETED_PASTE_START) && !body.contains(BRACKETED_PASTE_END),
            "正文里残留标记: {body:?}"
        );
    }

    #[test]
    fn 普通文本原样保留() {
        assert_eq!(bracketed_paste("echo hi"), wrap("echo hi"));
        assert_eq!(bracketed_paste(""), wrap(""));
        assert_eq!(bracketed_paste("中文 ~ 波浪号~"), wrap("中文 ~ 波浪号~"));
        // 只是像标记的半截不动
        assert_eq!(bracketed_paste("\x1b[201"), wrap("\x1b[201"));
        assert_eq!(bracketed_paste("\x1b[202~"), wrap("\x1b[202~"));
        assert_eq!(bracketed_paste("[201~"), wrap("[201~"));
    }

    #[test]
    fn 换行归一成回车() {
        assert_eq!(bracketed_paste("a\r\nb\nc\rd"), wrap("a\rb\rc\rd"));
        assert_eq!(bracketed_paste("\r\r\n"), wrap("\r\r"));
    }

    #[test]
    fn 去掉结束标记() {
        assert_eq!(bracketed_paste("x\x1b[201~y"), wrap("xy"));
        assert_eq!(bracketed_paste("\x1b[201~"), wrap(""));
    }

    #[test]
    fn 去掉开始标记() {
        assert_eq!(bracketed_paste("x\x1b[200~y"), wrap("xy"));
        assert_eq!(bracketed_paste("\x1b[200~\x1b[201~"), wrap(""));
    }

    /// 只 `replace` 一次时,删掉中间的标记后前后两段会重新拼出 `ESC[201~`。
    #[test]
    fn 删掉一个标记后拼出来的标记也去掉() {
        let encoded = bracketed_paste("a\x1b[201\x1b[201~~; rm -rf ~");
        assert_eq!(encoded, wrap("a; rm -rf ~"));
        assert_body_clean(&encoded);

        // 开始 / 结束交叉嵌套
        let encoded = bracketed_paste("\x1b[201\x1b[200~~b");
        assert_eq!(encoded, wrap("b"));
        assert_body_clean(&encoded);
    }

    #[test]
    fn 多层嵌套全部去掉() {
        // 每一层都是在上一层外面再套一个被拆开的标记:
        // ESC[201 ESC[201 ESC[201~ ~ ~
        let mut nested = String::from(BRACKETED_PASTE_END);
        for _ in 0..50 {
            nested = format!("\x1b[201{nested}~");
        }
        let encoded = bracketed_paste(&format!("前{nested}后"));
        assert_eq!(encoded, wrap("前后"));
        assert_body_clean(&encoded);
    }

    #[test]
    fn 换行归一不会制造或掩护标记() {
        // `\r\n` 归一成一个 `\r` 后仍夹在中间,拼不出标记
        let encoded = bracketed_paste("\x1b[20\r\n1~");
        assert_eq!(encoded, wrap("\x1b[20\r1~"));
        let encoded = bracketed_paste("ok\r\n\x1b[201~\r\nnext");
        assert_eq!(encoded, wrap("ok\r\rnext"));
    }

    #[test]
    fn 不会截到包装层的开始标记() {
        // 正文开头的 `~` 紧跟在开始标记后面,截断只看正文范围,包装层不会被吃掉
        assert_eq!(bracketed_paste("~"), wrap("~"));
        assert_eq!(bracketed_paste("~~~"), wrap("~~~"));
    }

    /// 线性:大量嵌套也是一遍过(平方级写法在这个量级会明显卡住)。
    #[test]
    fn 长输入单遍完成() {
        let n = 200_000;
        let mut text = "\x1b[201".repeat(n);
        text.push_str(BRACKETED_PASTE_END);
        text.push_str(&"~".repeat(n));
        let encoded = bracketed_paste(&text);
        assert_eq!(encoded, wrap(""));
    }
}
