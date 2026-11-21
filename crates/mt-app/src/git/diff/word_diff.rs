//! 词级高亮:按整个改动块算,再按行切回(见 [`block_marks`])。
//!
//! 移植自 Winc(Apache-2.0):`crates/winc-editor/src/diff_view.rs` 的 `inline_marks`(一块
//! 的两侧各拼成一段、换行也参与比较、比完按行切回)与 `crates/winc-core/src/diff.rs` 的
//! `words` / `intern` / `diff`(分词、换成编号、Myers 差分)。与原版的差别:
//!
//! - 区间是**字节**偏移(正文按 `StyledText` 的字节区间涂色),原版是字符偏移;
//! - **中日韩文字一个字一个词**(原版连成一个词):整句中文并成一个词,改一个字就是整句
//!   高亮,等于没细分 —— mini-term 逐行高亮时定下的口径,沿用;
//! - 上限更紧,超限交回逐对配对的老算法(`intra_line_marks_within`,它有自己的单行上限):
//!   结果一到就在 UI 线程上算,不能卡帧;
//! - 每行再过一道噪音判定(改动盖住这一行非空白内容的六成以上就整行一个底色),沿用老
//!   算法的六成口径;只差空白的行(缩进、拆行留下的空格)不受影响。
//!
//! # 为什么按块
//!
//! 逐行配对是「第 i 条删除行配第 i 条新增行」:AI 常见的一行拆两行、两行并一行、中间插
//! 一行,配对一错位,每对都「几乎全变了」,整行涂色,等于没有高亮。按块比就没有配对这一步
//! —— 删除行拼成一段、新增行拼成一段,换行本身也是词元,拆行就是「这里多了个换行和缩进」,
//! 高亮正好落在那几个空白上;中间插的那一行整行都是新的,整行一个底色,上下两行照常只点出
//! 真正改了的词。

use std::collections::HashMap;
use std::ops::Range;

use super::INTRA_LINE_NOISE_RATIO;

/// 一侧超过这么多行就不按块比(取自 Winc 的 `INLINE_LIMIT`):改动段这么长时通常整段都
/// 改了,逐词标记没有区分度。
const BLOCK_MAX_LINES: usize = 200;
/// 一侧拼起来超过这么多字节同样不按块比(压缩过的长行 / 整段粘贴)。
const BLOCK_MAX_BYTES: usize = 64 * 1024;
/// Myers 允许的最大编辑步数(词元)。超过 = 大面积重写,细分出来的也全是噪音。回溯要留
/// 每一步的对角线终点(≈ D² 个 `i32`),400 步约 640 KB。
const MAX_EDITS: usize = 400;
/// 一块 Myers 的工作量上限(每条对角线 1 步 + 每次相等比较 1 步)。
const MAX_WORK: usize = 2_000_000;
/// 一份 diff 的词级高亮总工作量(各块的 Myers 步数 + 逐对配对的 DP 格子数)。用完了剩下的
/// 行就整行一个底色 —— 几万行的大改动不能在 UI 线程上卡住一帧。
pub(super) const DIFF_BUDGET: usize = 20_000_000;

/// 一块的词级标记。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BlockMarks {
    /// 按块比出来了:两侧逐行的改动片段(显示文本的字节区间;空 = 整行一个底色)
    Lines {
        old: Vec<Vec<Range<usize>>>,
        new: Vec<Vec<Range<usize>>>,
    },
    /// 块太大 / 编辑太多 / 预算用完:交回逐对配对
    Fallback,
}

/// 一块改动(连续的删除行 `old` + 新增行 `new`,都是显示文本)按块比出词级标记。纯新增 /
/// 纯删除没得比,两侧都给空。`budget` 是这份 diff 剩下的工作量(见 [`DIFF_BUDGET`])。
pub(super) fn block_marks(old: &[&str], new: &[&str], budget: &mut usize) -> BlockMarks {
    if old.is_empty() || new.is_empty() {
        return BlockMarks::Lines {
            old: vec![Vec::new(); old.len()],
            new: vec![Vec::new(); new.len()],
        };
    }
    let bytes = |lines: &[&str]| lines.iter().map(|l| l.len() + 1).sum::<usize>();
    if old.len() > BLOCK_MAX_LINES
        || new.len() > BLOCK_MAX_LINES
        || bytes(old) > BLOCK_MAX_BYTES
        || bytes(new) > BLOCK_MAX_BYTES
    {
        return BlockMarks::Fallback;
    }
    // 两侧各拼成一段,每行后面跟一个换行:拆行 / 并行就是多了 / 少了这个换行
    let (a, a_starts) = joined(old);
    let (b, b_starts) = joined(new);
    let (wa, wb) = (words(&a), words(&b));
    let (ia, ib) = intern(&a, &wa, &b, &wb);
    let Some(edits) = diff(&ia, &ib, budget) else {
        return BlockMarks::Fallback;
    };
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for (ra, rb) in edits {
        push_span(&mut left, &wa, ra);
        push_span(&mut right, &wb, rb);
    }
    BlockMarks::Lines {
        old: split_lines(&left, &a_starts, old),
        new: split_lines(&right, &b_starts, new),
    }
}

/// 各行拼成一段(每行后面一个 `\n`),连同各行在段里的起点。
fn joined(lines: &[&str]) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    let mut starts = Vec::with_capacity(lines.len());
    for line in lines {
        starts.push(out.len());
        out.push_str(line);
        out.push('\n');
    }
    (out, starts)
}

/// 词元区间 `range`(下标)换成字节区间追加进 `out`,与上一段相接就并起来。
fn push_span(out: &mut Vec<Range<usize>>, words: &[Range<usize>], range: Range<usize>) {
    if range.is_empty() {
        return;
    }
    let (Some(first), Some(last)) = (words.get(range.start), words.get(range.end - 1)) else {
        return;
    };
    let span = first.start..last.end;
    match out.last_mut() {
        Some(prev) if prev.end == span.start => prev.end = span.end,
        _ => out.push(span),
    }
}

/// 整段里的字节区间按行切回(落在换行上的部分去掉),每行再过一道噪音判定([`noisy`])。
fn split_lines(
    ranges: &[Range<usize>],
    starts: &[usize],
    lines: &[&str],
) -> Vec<Vec<Range<usize>>> {
    lines
        .iter()
        .zip(starts)
        .map(|(line, &start)| {
            let end = start + line.len();
            let marks: Vec<Range<usize>> = ranges
                .iter()
                .filter_map(|r| {
                    let (from, to) = (r.start.max(start), r.end.min(end));
                    (from < to).then(|| from - start..to - start)
                })
                .collect();
            if noisy(line, &marks) {
                Vec::new()
            } else {
                marks
            }
        })
        .collect()
}

/// 改动盖住了这一行非空白内容的六成以上:整行都变了,细分出来全是噪音,还不如整行一个
/// 底色。只差空白的行(缩进、拆行留下的空格)非空白内容一个都没盖住,照常细分。
fn noisy(line: &str, marks: &[Range<usize>]) -> bool {
    let solid = |range: Range<usize>| {
        line[range]
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(char::len_utf8)
            .sum::<usize>()
    };
    let total = solid(0..line.len());
    if total == 0 || marks.is_empty() {
        return false;
    }
    let marked: usize = marks.iter().map(|r| solid(r.clone())).sum();
    marked as f32 > total as f32 * INTRA_LINE_NOISE_RATIO
}

/// 词元类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Break,
    Space,
    Other,
}

/// 中日韩文字(含假名、谚文、全角形式)。这些字一个字一个词(见模块注释)。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF | 0x2E80..=0x9FFF | 0xA960..=0xA97F | 0xAC00..=0xD7FF
        | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

fn class(c: char) -> Class {
    if c == '_' || (c.is_alphanumeric() && !is_cjk(c)) {
        Class::Word
    } else if c == '\n' || c == '\r' {
        Class::Break
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// 切词(移植自 Winc `words`):字母、数字与 `_` 连成一个词(中日韩文字除外,一字一词),
/// 连续的空白连成一段,连续的换行连成一段,其余字符各自成词。返回各词的字节区间。
fn words(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let kind = class(c);
        let mut end = start + c.len_utf8();
        if kind != Class::Other {
            while let Some(&(i, d)) = chars.peek() {
                if class(d) != kind {
                    break;
                }
                end = i + d.len_utf8();
                chars.next();
            }
        }
        out.push(start..end);
    }
    out
}

/// 两侧的词换成编号,内容相同编号相同,之后只比整数(移植自 Winc `intern`)。
fn intern<'t>(
    a: &'t str,
    wa: &[Range<usize>],
    b: &'t str,
    wb: &[Range<usize>],
) -> (Vec<u32>, Vec<u32>) {
    let mut ids: HashMap<&'t str, u32> = HashMap::new();
    let mut id = |word: &'t str| {
        let next = ids.len() as u32;
        *ids.entry(word).or_insert(next)
    };
    let ia = wa.iter().map(|r| id(&a[r.clone()])).collect();
    let ib = wb.iter().map(|r| id(&b[r.clone()])).collect();
    (ia, ib)
}

/// 两侧不同的一段:`a` 的下标区间换成 `b` 的(左闭右开)。
type Edit = (Range<usize>, Range<usize>);

/// Myers 差分,返回两侧不同的区间(从前往后)。移植自 Winc `diff`,多了一份全局预算:
/// 编辑步数超过 [`MAX_EDITS`]、工作量超过 [`MAX_WORK`] 或剩下的 `budget` 时返回 `None`。
/// 花掉的工作量从 `budget` 里扣。
fn diff(a: &[u32], b: &[u32], budget: &mut usize) -> Option<Vec<Edit>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let limit = (a.len() + b.len()).min(MAX_EDITS) as isize;
    let offset = limit + 1;
    let cap = MAX_WORK.min(*budget);
    let mut v = vec![0isize; 2 * limit as usize + 3];
    // 依次保存第 `d` 步之后各对角线 `k`(-d 至 d)的终点 `x`,第 `d` 步的数据从下标 `d²` 开始
    let mut trace: Vec<i32> = Vec::new();
    let mut work = 0usize;
    let mut found = None;
    for d in 0..=limit {
        for k in (-d..=d).step_by(2) {
            let index = (k + offset) as usize;
            let down = k == -d || (k != d && v[index - 1] < v[index + 1]);
            let start = if down { v[index + 1] } else { v[index - 1] + 1 };
            let (mut x, mut y) = (start, start - k);
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            work += (x - start) as usize + 1;
            v[index] = x;
            if x >= n && y >= m {
                found = Some(d);
            }
        }
        trace.extend((-d..=d).map(|k| v[(k + offset) as usize] as i32));
        if found.is_some() {
            break;
        }
        if work > cap {
            break;
        }
    }
    *budget = budget.saturating_sub(work);
    let edits = found?;

    // 从终点回溯,每一步对应删除 `a` 的一个词或插入 `b` 的一个词
    let mut steps = Vec::with_capacity(edits as usize);
    let (mut x, mut y) = (n, m);
    for d in (1..=edits).rev() {
        let previous = |k: isize| trace[((d - 1) * (d - 1) + k + d - 1) as usize] as isize;
        let k = x - y;
        let down = k == -d || (k != d && previous(k - 1) < previous(k + 1));
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = previous(prev_k);
        let prev_y = prev_x - prev_k;
        steps.push((prev_x as usize, prev_y as usize, down));
        (x, y) = (prev_x, prev_y);
    }
    let mut out: Vec<Edit> = Vec::new();
    for (x, y, insert) in steps.into_iter().rev() {
        let (end_a, end_b) = if insert { (x, y + 1) } else { (x + 1, y) };
        match out.last_mut() {
            Some((ra, rb)) if ra.end == x && rb.end == y => {
                ra.end = end_a;
                rb.end = end_b;
            }
            _ => out.push((x..end_a, y..end_b)),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两侧逐行的改动片段。
    type Sides = (Vec<Vec<Range<usize>>>, Vec<Vec<Range<usize>>>);

    fn none() -> Vec<Range<usize>> {
        Vec::new()
    }

    fn marks(old: &[&str], new: &[&str]) -> Sides {
        let mut budget = DIFF_BUDGET;
        match block_marks(old, new, &mut budget) {
            BlockMarks::Lines { old, new } => (old, new),
            BlockMarks::Fallback => panic!("不该退回逐对配对"),
        }
    }

    /// 一行拆成两行:内容一个没变,高亮只落在拆开处 —— 旧行那个空格、新第二行的缩进。
    #[test]
    fn 拆行只点出拆开处() {
        let (old, new) = marks(&["foo(a, b, c)"], &["foo(a,", "    b, c)"]);
        assert_eq!(old, vec![vec![6..7]]);
        assert_eq!(new, vec![vec![], vec![0..4]]);
    }

    /// 两行并成一行:反过来,旧第二行的缩进与新行里那个空格。
    #[test]
    fn 合行只点出合并处() {
        let (old, new) = marks(&["foo(a,", "    b, c)"], &["foo(a, b, c)"]);
        assert_eq!(old, vec![vec![], vec![0..4]]);
        assert_eq!(new, vec![vec![6..7]]);
    }

    /// 中间插了一行、上下两行各改了个数:逐行配对会把「let c = 3;」配给插进来的「let b = 2;」,
    /// 点出 c/b、3/2 两处假改动;按块比,旧侧两行都只点出那个数(「1」「3」在新侧没有对应
    /// 的词,最短编辑里恰好删掉它俩,与 Myers 怎么摆平局无关)。
    #[test]
    fn 中间插行不错位() {
        let (old, new) = marks(
            &["let a = 1;", "let c = 3;"],
            &["let a = 10;", "let b = 2;", "let c = 30;"],
        );
        assert_eq!(old, vec![vec![8..9], vec![8..9]]);
        assert_eq!(new.len(), 3);
    }

    /// 只差空白:缩进、行尾空格照样点出来(空白也算改动),不会被噪音判定吞掉。
    #[test]
    fn 仅空白变化() {
        let (old, new) = marks(&["  foo();"], &["    foo();"]);
        assert_eq!(old, vec![vec![0..2]]);
        assert_eq!(new, vec![vec![0..4]]);
        let (old, new) = marks(&["x = 1;"], &["x = 1;  "]);
        assert_eq!(old, vec![none()]);
        assert_eq!(new, vec![vec![6..8]]);
    }

    /// 中文一字一词:改一个字只点出那一个字。
    #[test]
    fn 中文逐字() {
        let (old, new) = marks(&["这是一行中文"], &["这是两行中文"]);
        assert_eq!(old, vec![vec![6..9]]);
        assert_eq!(new, vec![vec![6..9]]);
        assert_eq!(&"这是一行中文"[6..9], "一");
    }

    /// 整行都换了:六成以上的非空白内容是改动,整行一个底色;纯新增 / 纯删除没得比。
    #[test]
    fn 整行重写不细分() {
        let (old, new) = marks(&["aaaa bbbb"], &["cccc dddd"]);
        assert_eq!(old, vec![none()]);
        assert_eq!(new, vec![none()]);
        let (old, new) = marks(&[], &["x", "y"]);
        assert!(old.is_empty());
        assert_eq!(new, vec![none(), none()]);
    }

    /// 超大块(行数 / 字节越限)、编辑过多、预算用完:交回逐对配对,不在这里耗着。
    #[test]
    fn 超大块退回逐对配对() {
        let old: Vec<String> = (0..BLOCK_MAX_LINES + 1)
            .map(|i| format!("v = {i};"))
            .collect();
        let new: Vec<String> = (0..BLOCK_MAX_LINES + 1)
            .map(|i| format!("v = {};", i + 1))
            .collect();
        let old: Vec<&str> = old.iter().map(String::as_str).collect();
        let new: Vec<&str> = new.iter().map(String::as_str).collect();
        let mut budget = DIFF_BUDGET;
        assert_eq!(block_marks(&old, &new, &mut budget), BlockMarks::Fallback);
        let long = "x".repeat(BLOCK_MAX_BYTES + 1);
        assert_eq!(
            block_marks(&[long.as_str()], &["y"], &mut budget),
            BlockMarks::Fallback
        );
        // 编辑步数越限:两侧几百个互不相同的词
        let a: Vec<String> = (0..300).map(|i| format!("a{i}")).collect();
        let b: Vec<String> = (0..300).map(|i| format!("b{i}")).collect();
        let (a, b) = (a.join(" "), b.join(" "));
        assert_eq!(
            block_marks(&[a.as_str()], &[b.as_str()], &mut budget),
            BlockMarks::Fallback
        );
        // 预算用完
        let mut empty = 0;
        assert_eq!(
            block_marks(&["let x = 1;"], &["let x = 2;"], &mut empty),
            BlockMarks::Fallback
        );
    }

    /// 切词:字母数字连成词(含非 ASCII 字母),中日韩一字一词,空白 / 换行各连成一段。
    #[test]
    fn 切词规则() {
        let text = "fn café_1(x)  中文\n\n";
        let got: Vec<&str> = words(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(
            got,
            vec!["fn", " ", "café_1", "(", "x", ")", "  ", "中", "文", "\n\n"]
        );
    }
}
