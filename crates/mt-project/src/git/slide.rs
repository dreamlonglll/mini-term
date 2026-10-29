//! diff 块的「滑动压缩」:改动块落在哪,与 git(xdiff 的 `xdl_change_compact`)同口径。
//!
//! # 为什么要滑
//!
//! LCS / Myers 只保证编辑最短,不管一块纯新增 / 纯删除**落在哪** —— 同样的改动常有好几种
//! 等价摆法。文件末尾追加一个函数,自家 LCS 先剥公共前后缀,原文最后那个 `}` 被当成后缀
//! 吃掉,新增块整体上移一行,把没改的 `}` 画成新增(E2E 报的「18–21 行」,git 是 19–22):
//!
//! ```text
//!   自家(滑之前)        git
//!    17   }              17   }
//!  + 18   }              18   }
//!  + 19                + 19
//!  + 20 fn b() {       + 20 fn b() {
//!  + 21   …            + 21   …
//!    22   }            + 22   }
//! ```
//!
//! # 规则(逐条照 xdiff)
//!
//! 1. 一块能上下滑:块尾行与块前一行相同 → 上滑一格;块首行与块后一行相同 → 下滑一格。
//!    先滑到最上、再滑到最下,途中碰到相邻的块就并成一块;
//! 2. 滑动途中能与**另一侧**的改动对齐(删 + 增并成一组「修改」)的,停在最后一个对齐位置;
//! 3. 否则走缩进启发式(git 默认开着的 `diff.indentHeuristic`):在可滑范围里给每个位置
//!    按「块两端切在哪、切口两边的缩进与空行」打分,取分最低的,平手取最靠下。
//!
//! 输入输出都是父模块的编辑序列(`('=', 旧, 新)` / `('-', 旧, 新位置)` / `('+', 旧位置, 新)`)。
//! 压缩只换「哪几行算改动」,相等行仍两两相等、个数不变 —— 行号照样逐行可信,按块暂存 /
//! 丢弃拿行号拼文本的前提不受影响。同一段相等行之间的改动一律先删后增(与 git 的输出、
//! 并排视图的配对口径一致)。

/// 编辑序列里的一条:`(种类, 旧侧下标, 新侧下标)`。
type Op = (char, usize, usize);

/// 把编辑序列里的改动块按 git 的口径滑到位(见模块注释)。
pub(super) fn compact_ops(ops: &[Op], old: &[&str], new: &[&str]) -> Vec<Op> {
    let mut del = vec![false; old.len()];
    let mut add = vec![false; new.len()];
    for &(kind, oi, ni) in ops {
        match kind {
            '-' if oi < old.len() => del[oi] = true,
            '+' if ni < new.len() => add[ni] = true,
            _ => {}
        }
    }
    // 两侧的「未改动行」必须一样多(一一配对),否则输入本身不是合法的编辑序列:原样交回
    let kept = |flags: &[bool]| flags.iter().filter(|c| !**c).count();
    if kept(&del) != kept(&add) {
        return ops.to_vec();
    }
    // 与 xdiff 同序:先压旧侧(参照新侧的块对齐),再压新侧(参照压过的旧侧)
    change_compact(old, &mut del, &add);
    change_compact(new, &mut add, &del);
    rebuild(&del, &add)
}

/// 由两侧的改动标记重建编辑序列:相等行两两配对,中间夹着的改动先删后增。
fn rebuild(del: &[bool], add: &[bool]) -> Vec<Op> {
    let (m, n) = (del.len(), add.len());
    let mut ops = Vec::with_capacity(m.max(n));
    let (mut i, mut j) = (0, 0);
    while i < m || j < n {
        if i < m && j < n && !del[i] && !add[j] {
            ops.push(('=', i, j));
            i += 1;
            j += 1;
            continue;
        }
        let before = (i, j);
        while i < m && del[i] {
            ops.push(('-', i, j));
            i += 1;
        }
        while j < n && add[j] {
            ops.push(('+', i, j));
            j += 1;
        }
        // 一侧已经走完、另一侧还剩未改动行(合法输入到不了这里):剩下的按改动收尾
        if (i, j) == before {
            if i < m {
                ops.push(('-', i, j));
                i += 1;
            } else {
                ops.push(('+', i, j));
                j += 1;
            }
        }
    }
    ops
}

/// 一侧文件里的一组连续改动行 `[start, end)`。两侧的组一一对应(组与组之间隔一行
/// 未改动行;两侧未改动行一样多 ⇒ 组数一样多),空组表示这一处只有另一侧有改动。
#[derive(Clone, Copy, Debug)]
struct Group {
    start: usize,
    end: usize,
}

impl Group {
    fn first(rchg: &[bool]) -> Self {
        let mut end = 0;
        while end < rchg.len() && rchg[end] {
            end += 1;
        }
        Self { start: 0, end }
    }

    /// 挪到下一组;已经是最后一组时返回 `false`。
    fn next(&mut self, rchg: &[bool]) -> bool {
        if self.end == rchg.len() {
            return false;
        }
        self.start = self.end + 1;
        self.end = self.start;
        while self.end < rchg.len() && rchg[self.end] {
            self.end += 1;
        }
        true
    }

    /// 挪到上一组;已经是第一组时返回 `false`。
    fn previous(&mut self, rchg: &[bool]) -> bool {
        if self.start == 0 {
            return false;
        }
        self.end = self.start - 1;
        self.start = self.end;
        while self.start > 0 && rchg[self.start - 1] {
            self.start -= 1;
        }
        true
    }

    /// 下滑一格(块首行与块后一行相同才滑得动),顺手并上紧接着的下一块。
    fn slide_down(&mut self, lines: &[&str], rchg: &mut [bool]) -> bool {
        if self.end < lines.len() && lines[self.start] == lines[self.end] {
            rchg[self.start] = false;
            self.start += 1;
            rchg[self.end] = true;
            self.end += 1;
            while self.end < rchg.len() && rchg[self.end] {
                self.end += 1;
            }
            true
        } else {
            false
        }
    }

    /// 上滑一格(块尾行与块前一行相同才滑得动),顺手并上紧挨着的上一块。
    fn slide_up(&mut self, lines: &[&str], rchg: &mut [bool]) -> bool {
        if self.start > 0 && lines[self.start - 1] == lines[self.end - 1] {
            self.start -= 1;
            rchg[self.start] = true;
            self.end -= 1;
            rchg[self.end] = false;
            while self.start > 0 && rchg[self.start - 1] {
                self.start -= 1;
            }
            true
        } else {
            false
        }
    }
}

/// `xdl_change_compact`:压一侧(`lines` / `rchg`),`other` 是另一侧的改动标记(只读,
/// 用来认「能与对面的改动对齐」)。
fn change_compact(lines: &[&str], rchg: &mut [bool], other: &[bool]) {
    let mut g = Group::first(rchg);
    let mut go = Group::first(other);
    loop {
        if g.end != g.start {
            let mut earliest_end;
            let mut end_matching_other;
            // 上下滑到头;途中并进了别的块(块变大了)就再来一轮
            loop {
                let size = g.end - g.start;
                end_matching_other = None;
                while g.slide_up(lines, rchg) {
                    go.previous(other);
                }
                earliest_end = g.end;
                if go.end > go.start {
                    end_matching_other = Some(g.end);
                }
                while g.slide_down(lines, rchg) {
                    go.next(other);
                    if go.end > go.start {
                        end_matching_other = Some(g.end);
                    }
                }
                if size == g.end - g.start {
                    break;
                }
            }

            if g.end == earliest_end {
                // 滑不动,留在原位
            } else if end_matching_other.is_some() {
                // 退回到最后一个与对面改动对齐的位置
                while go.end == go.start {
                    if !g.slide_up(lines, rchg) {
                        break;
                    }
                    go.previous(other);
                }
            } else {
                let best = best_shift(lines, earliest_end, g.end, g.end - g.start);
                while g.end > best {
                    if !g.slide_up(lines, rchg) {
                        break;
                    }
                    go.previous(other);
                }
            }
        }
        if !g.next(rchg) {
            break;
        }
        go.next(other);
    }
}

// ─── 缩进启发式(xdiff 的 `XDF_INDENT_HEURISTIC`,常数照抄) ───────────

const MAX_INDENT: i32 = 200;
const MAX_BLANKS: i32 = 20;
const START_OF_FILE_PENALTY: i32 = 1;
const END_OF_FILE_PENALTY: i32 = 21;
const TOTAL_BLANK_WEIGHT: i32 = -30;
const POST_BLANK_WEIGHT: i32 = 6;
const RELATIVE_INDENT_PENALTY: i32 = -4;
const RELATIVE_INDENT_WITH_BLANK_PENALTY: i32 = 10;
const RELATIVE_OUTDENT_PENALTY: i32 = 24;
const RELATIVE_OUTDENT_WITH_BLANK_PENALTY: i32 = 17;
const RELATIVE_DEDENT_PENALTY: i32 = 23;
const RELATIVE_DEDENT_WITH_BLANK_PENALTY: i32 = 17;
const INDENT_WEIGHT: i32 = 60;
/// 最多往上试这么多个位置(块能滑很远时只看靠下的这一段)。
const INDENT_HEURISTIC_MAX_SLIDING: usize = 100;

/// 行首缩进(Tab 按 8 列制表位);整行都是空白时为 `-1`。
fn get_indent(line: &str) -> i32 {
    let mut ret = 0;
    for c in line.bytes() {
        // C 的 isspace:空格、\t、\n、\v、\f、\r
        if !matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
            return ret;
        }
        if c == b' ' {
            ret += 1;
        } else if c == b'\t' {
            ret += 8 - ret % 8;
        }
        if ret >= MAX_INDENT {
            return MAX_INDENT;
        }
    }
    -1
}

/// 在第 `split` 行之前切一刀,切口附近的样子。
struct SplitMeasurement {
    end_of_file: bool,
    /// 切口下面那一行的缩进(空白行 / 文件尾为 -1)
    indent: i32,
    /// 切口上方连着几行空白
    pre_blank: i32,
    /// 切口上方第一条非空白行的缩进(没有为 -1)
    pre_indent: i32,
    /// 切口下面那一行之后连着几行空白
    post_blank: i32,
    /// 再往下第一条非空白行的缩进(没有为 -1)
    post_indent: i32,
}

fn measure_split(lines: &[&str], split: usize) -> SplitMeasurement {
    let (end_of_file, indent) = if split >= lines.len() {
        (true, -1)
    } else {
        (false, get_indent(lines[split]))
    };
    let mut pre_blank = 0;
    let mut pre_indent = -1;
    for i in (0..split.min(lines.len())).rev() {
        pre_indent = get_indent(lines[i]);
        if pre_indent != -1 {
            break;
        }
        pre_blank += 1;
        if pre_blank == MAX_BLANKS {
            pre_indent = 0;
            break;
        }
    }
    let mut post_blank = 0;
    let mut post_indent = -1;
    for line in lines.iter().skip(split + 1) {
        post_indent = get_indent(line);
        if post_indent != -1 {
            break;
        }
        post_blank += 1;
        if post_blank == MAX_BLANKS {
            post_indent = 0;
            break;
        }
    }
    SplitMeasurement {
        end_of_file,
        indent,
        pre_blank,
        pre_indent,
        post_blank,
        post_indent,
    }
}

#[derive(Clone, Copy, Default)]
struct SplitScore {
    effective_indent: i32,
    penalty: i32,
}

fn score_add_split(m: &SplitMeasurement, s: &mut SplitScore) {
    if m.pre_indent == -1 && m.pre_blank == 0 {
        s.penalty += START_OF_FILE_PENALTY;
    }
    if m.end_of_file {
        s.penalty += END_OF_FILE_PENALTY;
    }
    let post_blank = if m.indent == -1 { 1 + m.post_blank } else { 0 };
    let total_blank = m.pre_blank + post_blank;
    s.penalty += TOTAL_BLANK_WEIGHT * total_blank;
    s.penalty += POST_BLANK_WEIGHT * post_blank;
    let indent = if m.indent != -1 {
        m.indent
    } else {
        m.post_indent
    };
    let any_blanks = total_blank != 0;
    s.effective_indent += indent;
    if indent == -1 || m.pre_indent == -1 || indent == m.pre_indent {
        // 不加减
    } else if indent > m.pre_indent {
        s.penalty += if any_blanks {
            RELATIVE_INDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_INDENT_PENALTY
        };
    } else if m.post_indent != -1 && m.post_indent > indent {
        s.penalty += if any_blanks {
            RELATIVE_OUTDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_OUTDENT_PENALTY
        };
    } else {
        s.penalty += if any_blanks {
            RELATIVE_DEDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_DEDENT_PENALTY
        };
    }
}

/// `< 0` = `a` 更好。
fn score_cmp(a: SplitScore, b: SplitScore) -> i32 {
    let cmp_indents = (a.effective_indent > b.effective_indent) as i32
        - (a.effective_indent < b.effective_indent) as i32;
    INDENT_WEIGHT * cmp_indents + (a.penalty - b.penalty)
}

/// 块(`size` 行,当前滑在最下面、结束于 `end`)放在哪儿最好:返回块的结束位置。
fn best_shift(lines: &[&str], earliest_end: usize, end: usize, size: usize) -> usize {
    // 只试靠下的 `size + 1` 个位置、最多 100 个(xdiff 同款上限)
    let from = earliest_end
        .max(end.saturating_sub(size + 1))
        .max(end.saturating_sub(INDENT_HEURISTIC_MAX_SLIDING));
    let mut best: Option<(usize, SplitScore)> = None;
    for shift in from..=end {
        let mut score = SplitScore::default();
        score_add_split(&measure_split(lines, shift), &mut score);
        score_add_split(&measure_split(lines, shift - size), &mut score);
        if best.is_none_or(|(_, b)| score_cmp(score, b) <= 0) {
            best = Some((shift, score));
        }
    }
    best.map_or(end, |(shift, _)| shift)
}

#[cfg(test)]
#[path = "slide_tests.rs"]
mod tests;
