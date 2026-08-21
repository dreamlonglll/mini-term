//! TUI 行文本的装饰剥离 —— AI 感知(`mt-ai`)与 AI 任务标记(`mt-app`)共用。
//!
//! 两边都在回答同一个问题:**屏幕上这一行,剥掉边框和提示符之后,正文是什么**。
//!
//! - `mt-ai` 拿它从输入框那一行里剥出「用户即将发出去的那句」(TUI 自己回填内容时,
//!   终端只收到一个裸 Enter,本地输入缓冲是空的,屏幕是唯一线索);
//! - `mt-app` 拿它在 scrollback 里找那条消息究竟落在哪一行。
//!
//! 放在这里是为了**字符集只有一份**:两边是配对使用的,剥法差一点就永远对不上,
//! 而那种失配是静默的 —— 标记不会报错,只会永远不出现。

/// 行首要剥掉的框线与提示符。
///
/// **刻意只收纯装饰字符**,不认任何一家 agent 的具体 UI:认死一家,换个 agent
/// 或者它改一次渲染器就失效(这与 `mt_app::markers` 里「指纹不问是谁发的」
/// 同一条理由)。
const HEAD: &[char] = &['│', '┃', '╎', '┆', '|', '>', '❯', '›', '»', '⏵'];

/// 行尾**只剥框线**。
///
/// `>` / `»` 这类提示符出现在行尾时多半是正文自己的一部分(`看看这段 >>>`),
/// 跟着剥会把内容改掉 —— 而内容一旦改掉,拿它去屏幕上比对就再也对不上了。
const TAIL: &[char] = &['│', '┃', '╎', '┆', '|'];

/// 剥掉行首行尾的框线与提示符,交回中间那段正文。
///
/// 逐层剥(TUI 常常套两层,如 `│ ❯ 正文 │`),剥不动就原样返回;
/// 整行都是装饰时交回空串,调用方自己判空。
pub fn strip_tui_decoration(line: &str) -> &str {
    let mut s = line.trim();
    loop {
        let next = s.trim_start_matches(HEAD).trim_end_matches(TAIL).trim();
        if next.len() == s.len() {
            return s;
        }
        s = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_head_and_tail_decoration() {
        assert_eq!(strip_tui_decoration("  > hi"), "hi");
        assert_eq!(strip_tui_decoration("│ > 帮我看看这段 │"), "帮我看看这段");
        assert_eq!(strip_tui_decoration("│ ❯ hi │"), "hi", "TUI 常常套两层");
        assert_eq!(strip_tui_decoration("⏵ 继续"), "继续");
    }

    /// 行尾的提示符是正文的一部分,不能跟着剥 —— 剥掉之后拿它去屏幕上比对
    /// 就再也对不上了。
    #[test]
    fn keeps_trailing_prompt_chars() {
        assert_eq!(strip_tui_decoration("看看这段 >>>"), "看看这段 >>>");
        assert_eq!(strip_tui_decoration("> a > b"), "a > b", "只剥行首那一个");
    }

    #[test]
    fn empty_when_all_decoration() {
        assert_eq!(strip_tui_decoration(""), "");
        assert_eq!(strip_tui_decoration("   "), "");
        assert_eq!(strip_tui_decoration("│   │"), "");
        assert_eq!(strip_tui_decoration(">>>"), "");
    }

    /// 没有装饰就原样返回(只裁两头空白)。
    #[test]
    fn plain_text_untouched() {
        assert_eq!(strip_tui_decoration("  帮我看看这段代码  "), "帮我看看这段代码");
    }
}
