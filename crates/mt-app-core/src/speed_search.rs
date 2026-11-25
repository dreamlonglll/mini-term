//! 列表「速搜」(type-to-select,IntelliJ 叫 Speed Search):焦点停在列表行上时直接敲字,
//! 查询词逐字累加,焦点跳到命中的行;↑ / ↓ 在命中行之间跳,Backspace 退一个字,Esc 收起。
//!
//! 这里只管查询词与「该跳到第几行」的下标算法,焦点、滚动与画都由各列表自己落
//! (与 [`crate::focus_nav::row_nav_target`] 同一个分工)。眼下的用户是文件树。
//!
//! # 口径
//!
//! - **只搜眼前这份列表**:调用方传进来的是当前可见的行(文件树即按展开态拍平的行),
//!   折叠着的目录里的不翻;
//! - 命中 = 行名**大小写不敏感的子串**,取第一处(高亮也只涂这一处)。不做模糊 / 驼峰缩写:
//!   子串的命中一眼就能对上,连续一段也好高亮;
//! - 查询词变了:从当前行(**含**)往下找,到底绕回顶 —— 当前行仍命中就不动,所以退格
//!   (子串变短,原来命中的仍命中)不会让焦点乱跳;
//! - ↑ / ↓:不含当前行,往上 / 往下找下一个命中的,到头绕回;Home / End:第一个 / 最后一个;
//! - 能开启 / 续上速搜的字:字母(**含中文等各国文字**)、数字,外加文件名里常见的
//!   `.` `_` `-`。空格不算 —— 行上的空格是「打开 / 折叠」;
//! - 字从哪儿来由调用方定:文件树挂的是文本输入处理器,英文直敲(WM_CHAR)与输入法上屏
//!   是同一条路([`SpeedSearch::push_text`]),不从按键事件里取字(两边都取会记两遍)。

use mt_project::search::find_substring_char_ranges;

/// 速搜的查询词。空 = 没在速搜。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpeedSearch {
    query: String,
    /// `query` 的小写形,命中判断每一行都要用,存一份免得每行现折。
    query_lower: String,
}

impl SpeedSearch {
    pub fn is_active(&self) -> bool {
        !self.query.is_empty()
    }

    /// 原样的查询词(底部条上画的就是它)。
    pub fn query(&self) -> &str {
        &self.query
    }

    /// 小写化的查询词,交给 [`match_range`] / [`hits`]。
    pub fn query_lower(&self) -> &str {
        &self.query_lower
    }

    /// 追加一个字。
    pub fn push(&mut self, ch: char) {
        self.query.push(ch);
        self.query_lower = self.query.to_lowercase();
    }

    /// 追加一段上屏的文字(一个字母,或输入法一次提交的一个词),只收 [`accepts_char`]
    /// 认的字,其余(空格、标点……)丢掉。返回是否真收进了字 —— 一个都没收时调用方
    /// 什么都不做(不开速搜、不跳行)。
    pub fn push_text(&mut self, text: &str) -> bool {
        let before = self.query.len();
        self.query
            .extend(text.chars().filter(|&ch| accepts_char(ch)));
        if self.query.len() == before {
            return false;
        }
        self.query_lower = self.query.to_lowercase();
        true
    }

    /// 退一个字。返回退完之后**是否还在速搜**(退空了即收起)。
    pub fn pop(&mut self) -> bool {
        self.query.pop();
        self.query_lower = self.query.to_lowercase();
        self.is_active()
    }

    /// 清空。返回清之前是否在速搜(调用方据此决定要不要播收起、要不要重画)。
    pub fn clear(&mut self) -> bool {
        let was_active = self.is_active();
        self.query.clear();
        self.query_lower.clear();
        was_active
    }
}

/// 这个字能不能开启 / 续上速搜(见模块注释「口径」末条)。
pub fn accepts_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

/// 名字里第一处命中的 **char** 区间 `(start, end)`;查询词为空或不命中为 `None`。
pub fn match_range(name: &str, query_lower: &str) -> Option<(usize, usize)> {
    find_substring_char_ranges(name, query_lower)
        .into_iter()
        .next()
}

/// 逐行的命中表。查询词为空时全是 `false`。
pub fn hits<'a>(names: impl IntoIterator<Item = &'a str>, query_lower: &str) -> Vec<bool> {
    names
        .into_iter()
        .map(|name| match_range(name, query_lower).is_some())
        .collect()
}

/// 往哪儿找。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seek {
    /// 查询词变了:从当前行(含)往下,到底绕回。
    From,
    /// ↓:当前行之后的下一个,到底绕回。
    Next,
    /// ↑:当前行之前的上一个,到顶绕回。
    Prev,
    /// Home:第一个命中的。
    First,
    /// End:最后一个命中的。
    Last,
}

/// 按命中表算出该跳到的行号。一行都不命中(或列表为空)时为 `None`,调用方原地不动。
/// `current` 是眼下拿着焦点的行;没有(焦点不在任何行上)时 `From` / `Next` 从顶找、
/// `Prev` 从底找。只有当前行命中时,`Next` / `Prev` 绕一圈回到它自己。
pub fn seek(hits: &[bool], current: Option<usize>, seek: Seek) -> Option<usize> {
    let n = hits.len();
    if n == 0 {
        return None;
    }
    let current = current.filter(|&ix| ix < n);
    let scan_down = |start: usize| (0..n).map(|step| (start + step) % n).find(|&ix| hits[ix]);
    let scan_up = |start: usize| {
        (0..n)
            .map(|step| (start + n - step) % n)
            .find(|&ix| hits[ix])
    };
    match seek {
        Seek::From => scan_down(current.unwrap_or(0)),
        Seek::Next => scan_down(current.map_or(0, |ix| ix + 1)),
        Seek::Prev => scan_up(current.map_or(n - 1, |ix| ix + n - 1)),
        Seek::First => hits.iter().position(|&hit| hit),
        Seek::Last => hits.iter().rposition(|&hit| hit),
    }
}

/// 底部条上的「第几个 / 共几个」:`(当前行在命中行里排第几(从 1 数), 命中总数)`。
/// 当前行不命中(比如 ← 回到了父目录行)时序号为 `None`。
pub fn ordinal(hits: &[bool], current: Option<usize>) -> (Option<usize>, usize) {
    let total = hits.iter().filter(|&&hit| hit).count();
    let ordinal = current
        .filter(|&ix| hits.get(ix).copied().unwrap_or(false))
        .map(|ix| hits[..=ix].iter().filter(|&&hit| hit).count());
    (ordinal, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 查询词累加与退格() {
        let mut search = SpeedSearch::default();
        assert!(!search.is_active());
        search.push('M');
        search.push('a');
        assert!(search.is_active());
        assert_eq!(search.query(), "Ma");
        assert_eq!(search.query_lower(), "ma");
        assert!(search.pop());
        assert_eq!(search.query(), "M");
        assert!(!search.pop(), "退空即收起");
        assert!(!search.pop(), "空了再退也不出错");
        search.push('x');
        assert!(search.clear());
        assert!(!search.clear(), "没在速搜时 clear 返回 false");
    }

    #[test]
    fn 只收字母数字与文件名常见符号() {
        for ch in ['a', 'Z', '0', '9', '.', '_', '-', 'é', '文', 'か'] {
            assert!(accepts_char(ch), "{ch:?}");
        }
        for ch in [' ', '/', '\\', '*', '\t', '\n', '?', '。', '，', '\u{8}'] {
            assert!(!accepts_char(ch), "{ch:?}");
        }
    }

    #[test]
    fn 上屏文字只收认得的字() {
        let mut search = SpeedSearch::default();
        assert!(!search.push_text(" "), "空格不开速搜");
        assert!(!search.push_text("。"), "中文标点不开速搜");
        assert!(!search.is_active());
        assert!(search.push_text("文件"), "输入法一次提交一个词");
        assert_eq!(search.query(), "文件");
        assert!(search.push_text("Tree."));
        assert_eq!(search.query(), "文件Tree.");
        assert_eq!(search.query_lower(), "文件tree.");
        assert!(search.push_text("a b"), "夹着空格的只收字");
        assert_eq!(search.query(), "文件Tree.ab");
        assert!(search.pop());
        assert_eq!(search.query(), "文件Tree.a", "退格按字退,中文一个字一退");
        let mut zh = SpeedSearch::default();
        zh.push_text("说明");
        assert!(zh.pop());
        assert_eq!(zh.query(), "说");
    }

    #[test]
    fn 命中是大小写不敏感的第一处子串() {
        assert_eq!(match_range("Cargo.toml", "car"), Some((0, 3)));
        assert_eq!(match_range("main.rs", "n.r"), Some((3, 6)));
        assert_eq!(match_range("aXbXc", "x"), Some((1, 2)), "取第一处");
        assert_eq!(match_range("README.md", "xyz"), None);
        assert_eq!(match_range("README.md", ""), None);
        // 非 ASCII 名字按 char 下标
        assert_eq!(match_range("说明.md", "md"), Some((3, 5)));
        assert_eq!(
            match_range("开发说明文档.md", "说明"),
            Some((2, 4)),
            "中文查询"
        );
        assert_eq!(
            hits(["src", "Cargo.toml", "cargo.lock"], "cargo"),
            vec![false, true, true]
        );
    }

    #[test]
    fn 查询词变了从当前行往下找_含当前行_到底绕回() {
        let h = [false, true, false, true, false];
        assert_eq!(seek(&h, Some(1), Seek::From), Some(1), "当前行命中就不动");
        assert_eq!(seek(&h, Some(2), Seek::From), Some(3));
        assert_eq!(seek(&h, Some(4), Seek::From), Some(1), "到底绕回顶");
        assert_eq!(seek(&h, None, Seek::From), Some(1));
        assert_eq!(seek(&[false, false], Some(0), Seek::From), None);
        assert_eq!(seek(&[], None, Seek::From), None);
    }

    #[test]
    fn 上下在命中行间跳_绕回() {
        let h = [true, false, true, false, true];
        assert_eq!(seek(&h, Some(0), Seek::Next), Some(2));
        assert_eq!(seek(&h, Some(4), Seek::Next), Some(0), "到底绕回");
        assert_eq!(
            seek(&h, Some(1), Seek::Next),
            Some(2),
            "当前行不命中也能往下找"
        );
        assert_eq!(seek(&h, Some(2), Seek::Prev), Some(0));
        assert_eq!(seek(&h, Some(0), Seek::Prev), Some(4), "到顶绕回");
        assert_eq!(seek(&h, None, Seek::Prev), Some(4));
        assert_eq!(seek(&h, None, Seek::Next), Some(0));
        let only = [false, true, false];
        assert_eq!(
            seek(&only, Some(1), Seek::Next),
            Some(1),
            "只有自己命中就绕回自己"
        );
        assert_eq!(seek(&only, Some(1), Seek::Prev), Some(1));
    }

    #[test]
    fn 首尾命中() {
        let h = [false, true, false, true, false];
        assert_eq!(seek(&h, Some(4), Seek::First), Some(1));
        assert_eq!(seek(&h, Some(0), Seek::Last), Some(3));
        assert_eq!(seek(&[false], Some(0), Seek::First), None);
    }

    #[test]
    fn 越界的当前行当作没有() {
        let h = [false, true];
        assert_eq!(seek(&h, Some(9), Seek::From), Some(1));
        assert_eq!(ordinal(&h, Some(9)), (None, 1));
    }

    #[test]
    fn 第几个与共几个() {
        let h = [true, false, true, true];
        assert_eq!(ordinal(&h, Some(0)), (Some(1), 3));
        assert_eq!(ordinal(&h, Some(3)), (Some(3), 3));
        assert_eq!(ordinal(&h, Some(1)), (None, 3), "当前行不命中只给总数");
        assert_eq!(ordinal(&h, None), (None, 3));
        assert_eq!(ordinal(&[false, false], Some(0)), (None, 0));
    }
}
