//! 折叠的上下文:两块之间、第一块之前、最后一块之后没改动的那几段([`Gap`]),以及按需
//! 把其中一部分展开出来。
//!
//! # 口径
//!
//! - diff 只带每块附近 3 行上下文,块与块之间(以及文件头尾)没改动的行折成一条分隔行
//!   ([`Fold`]),写明折了多少行。点分隔行 = 全部展开;行上另有「向下展开 20 行」(接着
//!   上面的内容往下露)、「向上展开 20 行」(接着下面的内容往上露)两颗按钮,对标 GitHub
//!   的分段展开。文件开头那一段上面没有内容,只能向上展开;末尾那一段只能向下展开。
//! - 展开纯在前端做:diff 结果带两侧全文(`GitDiffResult::{old_content, new_content}`),
//!   单文件 diff、提交 diff、合并变更、单文件历史、两段文本对比五处都有,不必再调 git。
//! - 只隔一行的折叠直接显示那一行:分隔行与那一行一样高,折起来什么也没省下。移植自
//!   Winc(Apache-2.0)`crates/winc-editor/src/diff_view.rs` 的 `layout`(「两个显示区间
//!   只相隔一行时直接显示该行,不单独折叠」)。
//!
//! # 展开状态记行号 + 内容
//!
//! 展开过哪些行记成 [`Revealed`]:两侧行号各一张表,值是那一行的原文。块操作后弹窗
//! 原地重取 diff,块的位置变了(暂存改的是旧侧、丢弃 / 取消暂存改的是新侧,总有一侧的行号
//! 没动),按「任一侧行号对得上、且那一行的内容没变」重新认出展开过的行;对不上的收回去,
//! 不报错。重新认完由调用方按新的行模型把表重建一遍(见 `Flat::revealed_now`),认不出的
//! 旧条目随之丢掉,不会在以后某次重取时又冒出来。

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use mt_project::git::DiffHunk;

/// 「向上 / 向下展开」一次露出多少行(GitHub 的口径)。
pub(super) const EXPAND_STEP: u32 = 20;

/// 两块之间没改动的一段:两侧起始行号(1 起)+ 行数。两侧行数必然相同 —— 中间全是
/// 两侧逐行相等的行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Gap {
    pub old_from: u32,
    pub new_from: u32,
    pub len: u32,
}

/// 一块在某一侧占的行区间 `[start, end)`(1 起)。优先按块里各行的行号算(逐行都是准的);
/// 那一侧一行都没有(纯插入 / 纯删除)时按 hunk 头:头里的起点是「插在第几行之前」。
fn side_span(hunk: &DiffHunk, old: bool) -> (u32, u32) {
    let nos = hunk.lines.iter().filter_map(|line| {
        if old {
            line.old_lineno
        } else {
            line.new_lineno
        }
    });
    let (mut first, mut last) = (None, None);
    for no in nos {
        first.get_or_insert(no);
        last = Some(no);
    }
    match (first, last) {
        (Some(first), Some(last)) => (first, last + 1),
        _ => {
            let start = if old { hunk.old_start } else { hunk.new_start }.max(1);
            (start, start)
        }
    }
}

/// hunk 列表 + 两侧总行数(按 `str::lines()` 数)→ 每一段折掉的上下文,共 `hunks.len() + 1`
/// 段:第 k 段在第 k 块之前,最后一段在最后一块之后。没有块时一段都没有(没有改动的
/// diff 不画「N 行未改动」)。
///
/// 防御:两侧算出来的行数理应相同,不同时取小的;越出全文的部分截掉。
pub(super) fn gaps(hunks: &[DiffHunk], old_len: usize, new_len: usize) -> Vec<Gap> {
    if hunks.is_empty() {
        return Vec::new();
    }
    let (old_len, new_len) = (old_len as u32, new_len as u32);
    let mut out = Vec::with_capacity(hunks.len() + 1);
    let (mut old_at, mut new_at) = (1u32, 1u32);
    for k in 0..=hunks.len() {
        let (old_to, new_to, next) = match hunks.get(k) {
            Some(hunk) => {
                let (old, new) = (side_span(hunk, true), side_span(hunk, false));
                (old.0, new.0, Some((old.1, new.1)))
            }
            None => (old_len + 1, new_len + 1, None),
        };
        let len = old_to
            .saturating_sub(old_at)
            .min(new_to.saturating_sub(new_at))
            .min((old_len + 1).saturating_sub(old_at))
            .min((new_len + 1).saturating_sub(new_at));
        out.push(Gap {
            old_from: old_at,
            new_from: new_at,
            len,
        });
        if let Some((old_end, new_end)) = next {
            old_at = old_end.max(old_at);
            new_at = new_end.max(new_at);
        }
    }
    out
}

/// 一段 gap 切出来的显示段(区间是 gap 内的偏移)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Run {
    /// 展开着的连续行
    Shown(Range<u32>),
    /// 仍折着的一段(画成一条分隔行)
    Folded(Range<u32>),
}

/// 按「gap 里第 i 行展开了没有」把一段 gap 切成显示段。只有一行的折叠直接显示(见模块注释)。
pub(super) fn gap_runs(len: u32, shown: impl Fn(u32) -> bool) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    let mut at = 0u32;
    while at < len {
        let open = shown(at);
        let mut end = at + 1;
        while end < len && shown(end) == open {
            end += 1;
        }
        // 只折了一行:当成展开的,与前后的展开段并成一段
        let open = open || end - at == 1;
        match out.last_mut() {
            Some(Run::Shown(prev)) if open => prev.end = end,
            _ if open => out.push(Run::Shown(at..end)),
            _ => out.push(Run::Folded(at..end)),
        }
        at = end;
    }
    out
}

/// 眼下仍折着的一段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Fold {
    /// 属于第几段 gap([`gaps`] 的下标)
    pub gap: usize,
    /// 折着的行在 gap 里的偏移区间 `[start, start + len)`
    pub start: u32,
    pub len: u32,
    /// 上面挨着内容(上一块,或这段 gap 里已展开的行):能「向下展开」
    pub above: bool,
    /// 下面挨着内容:能「向上展开」
    pub below: bool,
    /// 这条分隔行在行内视图 / 并排视图里是第几行(展开后据此平移选区)
    pub inline_row: usize,
    pub pair_row: usize,
}

/// 分隔行上的三种展开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Expand {
    /// 露出最上面 [`EXPAND_STEP`] 行(接着上面的内容往下)
    Down,
    /// 露出最下面 [`EXPAND_STEP`] 行(接着下面的内容往上)
    Up,
    /// 整段展开(点分隔行本身)
    All,
}

/// 这一次展开露出 gap 里的哪几行(偏移区间)。
pub(super) fn reveal_range(fold: &Fold, how: Expand) -> Range<u32> {
    let end = fold.start + fold.len;
    let step = EXPAND_STEP.min(fold.len);
    match how {
        Expand::Down => fold.start..fold.start + step,
        Expand::Up => end - step..end,
        Expand::All => fold.start..end,
    }
}

/// 分隔行上该画哪几颗展开按钮:`(向下, 向上)`。剩的行不多于一步时只给「全部展开」——
/// 向上 / 向下各露 20 行与全部展开是同一回事。
pub(super) fn step_buttons(fold: &Fold) -> (bool, bool) {
    if fold.len <= EXPAND_STEP {
        return (false, false);
    }
    (fold.above, fold.below)
}

/// 展开过的上下文行(见模块注释「展开状态记行号 + 内容」)。
#[derive(Debug, Default, Clone)]
pub(super) struct Revealed {
    by_old: HashMap<u32, Rc<str>>,
    by_new: HashMap<u32, Rc<str>>,
}

impl Revealed {
    pub(super) fn is_empty(&self) -> bool {
        self.by_old.is_empty() && self.by_new.is_empty()
    }

    /// 记下一行:两侧行号 + 原文。
    pub(super) fn insert(&mut self, old: u32, new: u32, text: &str) {
        let text: Rc<str> = Rc::from(text);
        self.by_old.insert(old, text.clone());
        self.by_new.insert(new, text);
    }

    /// 这一行(两侧行号 + 原文)展开过没有:任一侧行号对得上、且那一行的内容没变。
    pub(super) fn contains(&self, old: u32, new: u32, text: &str) -> bool {
        let same =
            |map: &HashMap<u32, Rc<str>>, no: u32| map.get(&no).is_some_and(|t| **t == *text);
        same(&self.by_old, old) || same(&self.by_new, new)
    }
}

#[cfg(test)]
mod tests {
    use mt_project::git::diff_two_texts;

    use super::*;

    fn numbered(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("l{i}")).collect()
    }

    fn text(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    /// 两处改动隔得很远:首块之前、两块之间、末块之后各一段,行号与行数都对得上全文。
    #[test]
    fn 折叠区间_首尾与块间() {
        let old = numbered(40);
        let mut new = old.clone();
        new[9] = "TEN".into();
        new[29] = "THIRTY".into();
        let diff = diff_two_texts(text(&old), text(&new));
        assert_eq!(diff.hunks.len(), 2);
        let g = gaps(&diff.hunks, 40, 40);
        // 第 10 行改了、上下文 3 行:块一占 7..=13,块二占 27..=33
        assert_eq!(
            g,
            vec![
                Gap {
                    old_from: 1,
                    new_from: 1,
                    len: 6
                },
                Gap {
                    old_from: 14,
                    new_from: 14,
                    len: 13
                },
                Gap {
                    old_from: 34,
                    new_from: 34,
                    len: 7
                },
            ]
        );
    }

    /// 新增 / 删除了几行之后,块后面那几段两侧行号错开,行数仍相同。
    #[test]
    fn 折叠区间_两侧行号错开() {
        let old = numbered(30);
        let mut new = old.clone();
        new.insert(5, "a".into());
        new.insert(6, "b".into());
        let diff = diff_two_texts(text(&old), text(&new));
        let g = gaps(&diff.hunks, 30, 32);
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].old_from, g[0].new_from, g[0].len), (1, 1, 2));
        // 块占旧侧 3..=8、新侧 3..=10:之后旧侧从 9、新侧从 11 起,各 22 行
        assert_eq!((g[1].old_from, g[1].new_from, g[1].len), (9, 11, 22));
        // 两段 + 块 = 全文
        let hunk_old: usize = diff.hunks.iter().map(|h| h.old_lines as usize).sum();
        assert_eq!(
            g.iter().map(|g| g.len as usize).sum::<usize>() + hunk_old,
            30
        );
    }

    /// 没有改动不出分隔行;改动贴着文件头 / 尾时那一段是空的;新文件(整份新增)没有可折的。
    #[test]
    fn 折叠区间_空段与无改动() {
        assert!(gaps(&[], 10, 10).is_empty());
        let old = numbered(5);
        let mut new = old.clone();
        new[0] = "X".into();
        let diff = diff_two_texts(text(&old), text(&new));
        let g = gaps(&diff.hunks, 5, 5);
        assert_eq!(g[0].len, 0, "改动在第一行:前面没有可折的");
        assert_eq!(g[1].len, 1, "第 5 行在块(1..=4)之后");
        let new_file = diff_two_texts(String::new(), "a\nb\n".into());
        assert!(gaps(&new_file.hunks, 0, 2).iter().all(|g| g.len == 0));
    }

    /// 展开状态切显示段:连续的展开 / 折叠各成一段;只折一行的直接显示并与相邻展开段合并。
    #[test]
    fn 显示段_只折一行的直接显示() {
        assert_eq!(gap_runs(10, |_| false), vec![Run::Folded(0..10)]);
        assert_eq!(gap_runs(1, |_| false), vec![Run::Shown(0..1)]);
        assert_eq!(gap_runs(0, |_| false), vec![]);
        assert_eq!(
            gap_runs(10, |i| i < 3 || i >= 8),
            vec![Run::Shown(0..3), Run::Folded(3..8), Run::Shown(8..10)]
        );
        // 第 3 行单独折着:并进前后的展开段
        assert_eq!(gap_runs(6, |i| i != 3), vec![Run::Shown(0..6)]);
        assert_eq!(
            gap_runs(8, |i| i == 2),
            vec![Run::Folded(0..2), Run::Shown(2..3), Run::Folded(3..8)]
        );
    }

    fn fold(start: u32, len: u32, above: bool, below: bool) -> Fold {
        Fold {
            gap: 0,
            start,
            len,
            above,
            below,
            inline_row: 0,
            pair_row: 0,
        }
    }

    /// 向下展开露最上面 20 行、向上展开露最下面 20 行、全部展开整段;不满一步的只给「全部」。
    #[test]
    fn 展开范围与按钮() {
        let f = fold(5, 50, true, true);
        assert_eq!(reveal_range(&f, Expand::Down), 5..25);
        assert_eq!(reveal_range(&f, Expand::Up), 35..55);
        assert_eq!(reveal_range(&f, Expand::All), 5..55);
        assert_eq!(step_buttons(&f), (true, true));
        // 文件开头那段:上面没有内容,只能向上
        assert_eq!(step_buttons(&fold(0, 50, false, true)), (false, true));
        assert_eq!(step_buttons(&fold(0, 50, true, false)), (true, false));
        let small = fold(0, 7, true, true);
        assert_eq!(step_buttons(&small), (false, false));
        assert_eq!(reveal_range(&small, Expand::Down), 0..7);
    }

    /// 展开过的行按任一侧行号 + 内容认:一侧行号错开了另一侧照样认得出,内容变了就不认。
    #[test]
    fn 展开记忆按行号与内容认() {
        let mut r = Revealed::default();
        assert!(r.is_empty());
        r.insert(10, 12, "foo");
        assert!(r.contains(10, 12, "foo"));
        assert!(r.contains(10, 99, "foo"), "旧侧行号没动");
        assert!(r.contains(7, 12, "foo"), "新侧行号没动");
        assert!(!r.contains(10, 12, "bar"), "内容变了");
        assert!(!r.contains(11, 13, "foo"), "两侧都对不上");
    }
}
