//! IME 预编辑（组合中文本）的状态机。
//!
//! # 终端没有「文档」这件事
//!
//! `EntityInputHandler` 是照编辑器设计的：它假定视图背后有一段可编辑文本，
//! 所有偏移都是相对那段文本的 UTF-16 下标。终端**没有**这样的文档 —— 已经吐出去
//! 的字节归 shell 管，我们只有一个「正在组合、还没提交」的临时串。
//!
//! 所以这里把**预编辑串本身当作那个文档**：长度就是它的 UTF-16 长度，marked range
//! 永远是 `0..len`，选区落在串内。提交（`replace_text_in_range`）之后串清空，
//! 文档长度回到 0，字节交给 PTY —— 从此与我们无关。
//!
//! # UTF-16 与字节偏移
//!
//! 平台给的全是 UTF-16 偏移（Windows 的 IMM32、macOS 的 NSTextInputClient 都是），
//! 而 Rust 的 `String` 按字节切。中文每字 1 个 UTF-16 单元、3 个字节，emoji 更是
//! 2 个 UTF-16 单元、4 个字节 —— 混着用就会在候选框里劈开一个字。这个模块里
//! 所有对外的 `Range<usize>` 一律是 UTF-16，转换只在 [`utf16_to_byte`] 一处发生。

use std::ops::Range;

/// 正在组合的预编辑串。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preedit {
    pub text: String,
    /// 光标在串内的位置（UTF-16 偏移）。IME 用它决定候选框贴在哪。
    pub cursor_utf16: usize,
}

impl Preedit {
    pub fn len_utf16(&self) -> usize {
        self.text.encode_utf16().count()
    }

    /// 光标处的字节偏移（渲染时要用它算插入符的 x）。
    pub fn cursor_byte(&self) -> usize {
        utf16_to_byte(&self.text, self.cursor_utf16)
    }
}

/// 一个 pane 的 IME 状态。
#[derive(Debug, Default)]
pub struct ImeState {
    preedit: Option<Preedit>,
}

impl ImeState {
    pub fn is_composing(&self) -> bool {
        self.preedit.is_some()
    }

    pub fn preedit(&self) -> Option<&Preedit> {
        self.preedit.as_ref()
    }

    /// marked text 的范围。**平台靠这个判断「正在组合吗」** ——
    /// Windows 的 `handle_keydown_msg` 会先问它，非 `None` 就直接把按键让给 IME，
    /// 完全不派发 KeyDown。这条是「组合期间不会有半个字漏进 PTY」的保证。
    pub fn marked_range_utf16(&self) -> Option<Range<usize>> {
        self.preedit.as_ref().map(|p| 0..p.len_utf16())
    }

    /// 当前选区（UTF-16）。没在组合时返回 `0..0` 而不是 `None` ——
    /// 有些 IME 拿到 `None` 会认为控件不接受输入，直接不弹候选框。
    pub fn selected_range_utf16(&self) -> Range<usize> {
        match &self.preedit {
            Some(p) => p.cursor_utf16..p.cursor_utf16,
            None => 0..0,
        }
    }

    /// 取一段文本给 IME 看（重转换、候选窗预览会问）。
    pub fn text_for_range_utf16(&self, range: Range<usize>) -> Option<String> {
        let preedit = self.preedit.as_ref()?;
        let start = utf16_to_byte(&preedit.text, range.start);
        let end = utf16_to_byte(&preedit.text, range.end.max(range.start));
        Some(preedit.text[start..end].to_string())
    }

    /// 更新预编辑串（`replace_and_mark_text_in_range`）。
    ///
    /// `range` 是要被替换的 UTF-16 区间；`None` = 换掉整串（Windows 走这条）。
    /// `new_selection` 是新的光标位置，`None` 时落在新文本末尾。
    pub fn set_marked(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selection: Option<Range<usize>>,
    ) {
        let current = self.preedit.take().unwrap_or_default();
        let text = match range {
            Some(r) => splice_utf16(&current.text, r, new_text),
            None => new_text.to_string(),
        };
        if text.is_empty() {
            // 空串 = 组合结束（退格删光候选、或 IME 主动取消）。
            // 必须真的清掉而不是留一个空 Preedit：留着的话 marked_range 仍非 None，
            // 平台会继续把所有按键喂给 IME，终端从此再也收不到键。
            self.preedit = None;
            return;
        }
        let len = text.encode_utf16().count();
        let cursor = new_selection.map(|s| s.start.min(len)).unwrap_or(len);
        self.preedit = Some(Preedit {
            text,
            cursor_utf16: cursor,
        });
    }

    /// 提交（`replace_text_in_range`）：清空预编辑并返回要写进 PTY 的文本。
    ///
    /// 返回 `None` 表示这次提交不该产生任何输入（空串提交 —— 日文 IME 取消组合时
    /// 会发一次 lparam=0 的空提交）。
    pub fn commit(&mut self, text: &str) -> Option<String> {
        self.preedit = None;
        if text.is_empty() {
            return None;
        }
        Some(text.to_string())
    }

    /// 平台要求丢弃组合（切窗口 / 点别处）。
    pub fn clear(&mut self) {
        self.preedit = None;
    }
}

/// UTF-16 偏移 → 字节偏移。越界钳到串尾；落在代理对中间时向下取到该字符起点。
pub fn utf16_to_byte(text: &str, offset_utf16: usize) -> usize {
    if offset_utf16 == 0 {
        return 0;
    }
    let mut seen = 0usize;
    for (byte_ix, ch) in text.char_indices() {
        if seen >= offset_utf16 {
            return byte_ix;
        }
        let next = seen + ch.len_utf16();
        if next > offset_utf16 {
            // 偏移落在代理对中间（emoji 的一半）：取这个字符的起点。
            // 切在中间会 panic —— Rust 的 `&str[a..b]` 不接受非字符边界。
            return byte_ix;
        }
        seen = next;
    }
    text.len()
}

/// 按 UTF-16 区间替换子串。区间越界一律钳进合法范围。
fn splice_utf16(text: &str, range: Range<usize>, replacement: &str) -> String {
    let start = utf16_to_byte(text, range.start);
    let end = utf16_to_byte(text, range.end.max(range.start));
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(&text[..start]);
    out.push_str(replacement);
    out.push_str(&text[end..]);
    out
}

/// 提交文本 → PTY 字节。
///
/// 与粘贴同一条规矩：`\r\n` / `\n` 归一成 `\r`。IME 提交里出现换行是少见但真实的
/// （日文 IME 的「変換」候选可以含换行，某些手写板也会），不归一会在 PTY 那头
/// 走出阶梯形。**不加 bracketed paste 包裹** —— 这是键入不是粘贴，
/// 包起来会让 shell 把它当成粘贴块（zsh 的 bracketed-paste-magic 会拒绝执行）。
pub fn commit_to_bytes(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 未组合时不占用按键通道() {
        let ime = ImeState::default();
        assert!(!ime.is_composing());
        assert_eq!(ime.marked_range_utf16(), None);
        // 选区必须是 Some(0..0) 语义，不能是「没有选区」
        assert_eq!(ime.selected_range_utf16(), 0..0);
    }

    #[test]
    fn 拼音组合到提交的完整一轮() {
        let mut ime = ImeState::default();

        // 键入 n → i → 组合串 "ni"
        ime.set_marked(None, "n", None);
        assert_eq!(ime.marked_range_utf16(), Some(0..1));
        ime.set_marked(None, "ni", None);
        assert_eq!(ime.preedit().unwrap().text, "ni");
        assert_eq!(ime.preedit().unwrap().cursor_utf16, 2);

        // IME 换成中文候选预览
        ime.set_marked(None, "你好", Some(1..1));
        let preedit = ime.preedit().unwrap();
        assert_eq!(preedit.text, "你好");
        assert_eq!(preedit.cursor_utf16, 1);
        // 中文每字 1 个 UTF-16 单元、3 个字节
        assert_eq!(preedit.len_utf16(), 2);
        assert_eq!(preedit.cursor_byte(), 3);

        // 敲空格上屏
        assert_eq!(ime.commit("你好").as_deref(), Some("你好"));
        assert!(!ime.is_composing());
        assert_eq!(commit_to_bytes("你好"), "你好".as_bytes().to_vec());
    }

    #[test]
    fn 组合串删光等于结束组合() {
        let mut ime = ImeState::default();
        ime.set_marked(None, "ni", None);
        assert!(ime.is_composing());
        // 退格删光：留一个空 Preedit 会让平台永远把按键喂给 IME
        ime.set_marked(None, "", None);
        assert!(!ime.is_composing());
        assert_eq!(ime.marked_range_utf16(), None);
    }

    #[test]
    fn 空提交不产生输入() {
        let mut ime = ImeState::default();
        ime.set_marked(None, "ni", None);
        // 日文 IME 取消组合时会发一次 lparam=0 的空提交
        assert_eq!(ime.commit(""), None);
        assert!(!ime.is_composing());
    }

    #[test]
    fn 按区间替换走_utf16_偏移() {
        let mut ime = ImeState::default();
        ime.set_marked(None, "你好世界", None);
        // 换掉第 2..4 个 UTF-16 单元（"世界"）
        ime.set_marked(Some(2..4), "朋友", None);
        assert_eq!(ime.preedit().unwrap().text, "你好朋友");
        // 越界的区间钳住，不 panic
        ime.set_marked(Some(3..99), "！", None);
        assert_eq!(ime.preedit().unwrap().text, "你好朋！");
    }

    #[test]
    fn emoji_的_utf16_与字节偏移不同步() {
        // 😀 = 1 个 char、2 个 UTF-16 单元、4 个字节
        let text = "a😀b";
        assert_eq!(utf16_to_byte(text, 0), 0);
        assert_eq!(utf16_to_byte(text, 1), 1); // 'a' 之后
        assert_eq!(utf16_to_byte(text, 3), 5); // emoji 之后
        assert_eq!(utf16_to_byte(text, 4), 6); // 'b' 之后 = 串尾
        assert_eq!(utf16_to_byte(text, 99), 6); // 越界钳到串尾
        // 落在代理对中间：取到该字符起点，绝不切出半个 char
        assert_eq!(utf16_to_byte(text, 2), 1);
    }

    #[test]
    fn 取文本片段按_utf16_切() {
        let mut ime = ImeState::default();
        ime.set_marked(None, "你好世界", None);
        assert_eq!(ime.text_for_range_utf16(0..2).as_deref(), Some("你好"));
        assert_eq!(ime.text_for_range_utf16(2..4).as_deref(), Some("世界"));
        // 倒序区间不 panic
        assert_eq!(ime.text_for_range_utf16(3..1).as_deref(), Some(""));
        ime.clear();
        assert_eq!(ime.text_for_range_utf16(0..1), None);
    }

    #[test]
    fn 提交文本归一换行且不加粘贴括号() {
        assert_eq!(commit_to_bytes("a\r\nb\nc"), b"a\rb\rc".to_vec());
        // 不该出现 ESC[200~
        assert!(!commit_to_bytes("ab").starts_with(b"\x1b"));
    }
}
