use super::*;

// ─── 行内(词级)差异 ─────────────────────────────────────────
//
// 配对成功的那对 delete/add 行再跑一次词级 diff,只把**真正变了的片段**涂实
// (`ui::diff_*_word_bg`)。原版没有这一层:一行里改一个字符,整行都是红/绿。

/// 参与词级 diff 的单行长度上限(字节)。再长就整行涂 —— 压缩过的 JS / base64
/// 这类超长行本来也没有词界,而 DP 是 O(m·n),放开会直接卡帧。
pub(super) const INTRA_LINE_MAX_BYTES: usize = 2000;

/// 剥掉公共前后缀**之后**还允许的 DP 规模(格子数)。
pub(super) const INTRA_LINE_MAX_CELLS: usize = 120_000;

/// 变动词元占比超过它就不细分 —— 整行都变了的时候,细分出来的高亮全是噪音,
/// 还不如原版那样整行一个底色干净。
pub(super) use super::word_diff::INTRA_LINE_NOISE_RATIO;

/// 词元类别。相邻同类字符并成一个词元;`Other`(标点/符号)一个字符一个词元。
///
/// ⚠️ 只有 **ASCII** 字母数字算 `Word`:中日韩文字按 `Other` 逐字切,否则一整句
/// 中文会并成一个词元,改一个字 = 整句高亮,等于没细分。
#[derive(PartialEq, Eq, Clone, Copy)]
pub(super) enum CharClass {
    Word,
    Space,
    Other,
}

pub(super) fn char_class(c: char) -> CharClass {
    if c.is_ascii_alphanumeric() || c == '_' {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Other
    }
}

/// 一行 → 词元序列(字节区间 + 文本)。
pub(super) fn tokenize(s: &str) -> Vec<(Range<usize>, &str)> {
    let mut out: Vec<(Range<usize>, &str)> = Vec::new();
    let mut it = s.char_indices().peekable();
    while let Some((start, ch)) = it.next() {
        let class = char_class(ch);
        let mut end = start + ch.len_utf8();
        if class != CharClass::Other {
            while let Some(&(i, c)) = it.peek() {
                if char_class(c) != class {
                    break;
                }
                end = i + c.len_utf8();
                it.next();
            }
        }
        out.push((start..end, &s[start..end]));
    }
    out
}

/// 相邻/相接的区间并成一段(高亮块之间不留缝)。
pub(super) fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if last.end >= r.start => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// 一对删 / 增行各自「变了的字节区间」。
pub(super) type PairMarks = (Vec<Range<usize>>, Vec<Range<usize>>);

/// 不限预算的 [`intra_line_marks_within`](单测用)。
#[cfg(test)]
pub(super) fn intra_line_marks(old: &str, new: &str) -> Option<PairMarks> {
    let mut unlimited = usize::MAX;
    intra_line_marks_within(old, new, &mut unlimited)
}

/// 一对 delete/add 行的词级差异 → 两侧各自「变了的字节区间」。按块比放弃时的退路
/// (见 [`word_diff`]),DP 格子数从这份 diff 的预算 `budget` 里扣。
///
/// `None` = 不细分(两行相同 / 太长 / 改动占比过高 / 预算用完)。
pub(super) fn intra_line_marks_within(
    old: &str,
    new: &str,
    budget: &mut usize,
) -> Option<PairMarks> {
    if old == new || old.is_empty() || new.is_empty() {
        return None;
    }
    if old.len() > INTRA_LINE_MAX_BYTES || new.len() > INTRA_LINE_MAX_BYTES {
        return None;
    }

    let a = tokenize(old);
    let b = tokenize(new);

    // 公共前后缀先剥掉:绝大多数行只改中间一小段,DP 表能小一两个量级
    let mut head = 0usize;
    while head < a.len() && head < b.len() && a[head].1 == b[head].1 {
        head += 1;
    }
    let mut tail = 0usize;
    while tail < a.len() - head
        && tail < b.len() - head
        && a[a.len() - 1 - tail].1 == b[b.len() - 1 - tail].1
    {
        tail += 1;
    }
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    if am.is_empty() && bm.is_empty() {
        return None;
    }
    let cells = am.len() * bm.len();
    if cells > INTRA_LINE_MAX_CELLS || cells > *budget {
        return None;
    }
    *budget -= cells;

    // LCS DP(u32 足够:词元数被 INTRA_LINE_MAX_CELLS 间接钳住)
    let (m, n) = (am.len(), bm.len());
    let stride = n + 1;
    let mut dp = vec![0u32; (m + 1) * stride];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i * stride + j] = if am[i].1 == bm[j].1 {
                dp[(i + 1) * stride + j + 1] + 1
            } else {
                dp[(i + 1) * stride + j].max(dp[i * stride + j + 1])
            };
        }
    }

    let mut del: Vec<Range<usize>> = Vec::new();
    let mut add: Vec<Range<usize>> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < m || j < n {
        if i < m && j < n && am[i].1 == bm[j].1 {
            i += 1;
            j += 1;
        } else if j < n && (i >= m || dp[i * stride + j + 1] >= dp[(i + 1) * stride + j]) {
            add.push(bm[j].0.clone());
            j += 1;
        } else {
            del.push(am[i].0.clone());
            i += 1;
        }
    }

    // 改动占比:按字节算(词元数会被一串标点带偏)
    let changed: usize = del.iter().chain(add.iter()).map(|r| r.len()).sum();
    if changed as f32 > (old.len() + new.len()) as f32 * INTRA_LINE_NOISE_RATIO {
        return None;
    }
    if del.is_empty() && add.is_empty() {
        return None;
    }

    Some((merge_ranges(del), merge_ranges(add)))
}

// ─── 拍平后的行模型 ───────────────────────────────────────────

/// inline 视图的一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InlineRow {
    /// 第 n 个 hunk 的 `@@` 头
    Head(usize),
    /// [`Flat::lines`] 的下标
    Line(usize),
    /// 折着的上下文([`Flat::folds`] 的下标)
    Fold(usize),
}

/// hunk 头(只有行内视图有,并排视图没有头行)。
pub(super) struct HunkHead {
    pub(super) inline: SharedString,
}

pub(super) fn hunk_head(h: &DiffHunk) -> HunkHead {
    let (old, new) = (
        head_start(h.old_start, h.old_lines),
        head_start(h.new_start, h.new_lines),
    );
    HunkHead {
        inline: format!("@@ -{old},{} +{new},{} @@", h.old_lines, h.new_lines).into(),
    }
}

/// hunk 头里某一侧的起始行号,按 git 的写法:那一侧一行都没有(纯新增的旧侧、纯删除的
/// 新侧)时写**插入点之前那一行**,文件开头为 0 —— `@@ -0,0 +1,3 @@`、`@@ -4,0 +5,2 @@`。
/// 后端 `DiffHunk` 里存的是「插在第几行之前」(按块操作按它拼文本),**只改显示**。
pub(super) fn head_start(start: u32, lines: u32) -> u32 {
    if lines == 0 {
        start.saturating_sub(1)
    } else {
        start
    }
}

/// 拍平后的一行(两个视图共用)。
pub(super) struct FlatLine {
    pub(super) kind: String,
    /// **tab 展开后**的显示文本。存成 `SharedString`:行回调每帧给每个可见行建文字元素,
    /// clone 只加引用计数 —— 压缩过的单行 JS / JSON 一行就几百 KB,每帧整行复制一份太亏。
    pub(super) content: SharedString,
    pub(super) old_lineno: Option<u32>,
    pub(super) new_lineno: Option<u32>,
}

/// 一次 diff 结果算好的**全部派生数据**。结果一到就算一次,不在每帧的
/// uniform_list 回调里重算;展开折叠只重排行模型([`Flat::relayout`]),不重算词级高亮。
#[derive(Default)]
pub(super) struct Flat {
    /// 拍平的行(两个视图共用一份)。`content` 是 **tab 展开后**的显示文本。前
    /// [`Flat::hunk_lines`] 行出自块,其后是展开出来的上下文行(每次重排重建)
    pub(super) lines: Vec<FlatLine>,
    /// 与 [`Flat::lines`] 等长:含 tab 的行展开前的原文(复制要给它),没有 tab 的是 `None`
    pub(super) raw: Vec<Option<String>>,
    /// 与 [`Flat::lines`] 等长:每行的行内改动片段。空 = 整行一个底色
    pub(super) marks: Vec<Vec<Range<usize>>>,
    /// [`Flat::lines`] 里前多少行出自块。勾选与块操作只认这些行,展开出来的上下文行在它之后
    pub(super) hunk_lines: usize,
    /// 第 k 块的第一行在 [`Flat::lines`] 里的下标;末尾多一个 = [`Flat::hunk_lines`]。
    /// 块里第 i 行就是 `DiffHunk::lines[i]`(按行操作的行号就按它换算)
    pub(super) hunk_base: Vec<usize>,
    pub(super) heads: Vec<HunkHead>,
    /// 折掉的上下文,共 `heads.len() + 1` 段(见 [`fold`])。拿着两侧全文拍平时才有
    pub(super) gaps: Vec<fold::Gap>,
    /// 眼下仍折着的段,视图里各占一条分隔行
    pub(super) folds: Vec<fold::Fold>,
    pub(super) inline_rows: Vec<InlineRow>,
    /// 行内视图「上一处/下一处改动」的落点 = 每个 hunk 头所在的行下标(并排视图的落点见
    /// [`Split::hunk_pos`])
    pub(super) inline_jumps: Vec<usize>,
    /// 行内视图给 uniform_list 量宽用的「最宽的那一行」(并排两栏的在 [`Split::widest`])
    pub(super) widest_inline: usize,
    /// 并排视图的两栏行序、各处改动与滚动映射(见 [`split`])
    pub(super) split: Split,
    /// 标题栏「+12 −3」的两个数
    pub(super) adds: usize,
    pub(super) deletes: usize,
}

/// 并排视图一处改动的身份:所在的块、两侧第一行的行号(删除行在旧侧、新增行在新侧;该侧
/// 没有行时 `None`)、两侧行数。
///
/// 按钮回调里记的改动序号是渲染时的:静默重取刚落地、还没重画的那一两帧里点下去,同一个
/// 序号可能已经指着别处。回调连同身份一起记下,点下去时与眼下的行模型核对(见
/// [`DiffState::change_is`]),对不上就作罢。行内视图的块头按钮同理,身份是块头
/// (见 [`Flat::hunk_key`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ChangeKey {
    pub(super) hunk: usize,
    pub(super) first: [Option<u32>; 2],
    pub(super) len: [usize; 2],
}

/// 行宽的近似列数:CJK/全角按 2,其余按 1。
///
/// 只用来在一堆行里挑出最宽的那一条喂给 `with_width_from_item`,不求精确 ——
/// 挑错了顶多横向滚动范围差一点,不影响正确性。
pub(super) fn text_cols(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if matches!(c as u32,
                0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3
                | 0xF900..=0xFAFF | 0xFE30..=0xFE6F | 0xFF00..=0xFF60
                | 0xFFE0..=0xFFE6 | 0x20000..=0x3FFFD)
            {
                2
            } else {
                1
            }
        })
        .sum()
}

/// 一行原文 → 显示用的 [`FlatLine`] + 复制用的原文。
///
/// `\t` 在 gpui 文本里是零宽(见 `tab_expansion` 模块注释),显示前按制表位展开。
/// 词级高亮与量宽都在展开后的文本上算,字节区间才对得上;原文另存一份给复制用。
/// 显示文本在这里一次转成 `SharedString`(见 [`FlatLine::content`])
pub(super) fn flat_line(
    kind: String,
    content: String,
    old_lineno: Option<u32>,
    new_lineno: Option<u32>,
) -> (FlatLine, Option<String>) {
    let (content, raw) = match expand_line(&content, TAB_WIDTH) {
        Cow::Owned(expanded) => (expanded, Some(content)),
        Cow::Borrowed(_) => (content, None),
    };
    let line = FlatLine {
        kind,
        content: content.into(),
        old_lineno,
        new_lineno,
    };
    (line, raw)
}

/// hunk 列表 → [`Flat`] 里与折叠无关的那一半(块里的行、词级高亮、hunk 头),行模型
/// 还没排([`Flat::relayout`] 排)。词级高亮按改动块算([`mark_blocks`],不跨 hunk),整份
/// diff 共用一份工作量预算。
pub(super) fn flatten_hunks(hunks: &[DiffHunk]) -> Flat {
    let mut f = Flat::default();
    let mut budget = word_diff::DIFF_BUDGET;

    for hunk in hunks {
        let base = f.lines.len();
        f.heads.push(hunk_head(hunk));
        f.hunk_base.push(base);
        f.marks.extend((0..hunk.lines.len()).map(|_| Vec::new()));
        for line in &hunk.lines {
            let (line, raw) = flat_line(
                line.kind.clone(),
                line.content.clone(),
                line.old_lineno,
                line.new_lineno,
            );
            f.lines.push(line);
            f.raw.push(raw);
        }
        let end = f.lines.len();
        mark_blocks(&mut f, base..end, &mut budget);
    }

    f.hunk_lines = f.lines.len();
    f.hunk_base.push(f.hunk_lines);
    f.adds = f.lines.iter().filter(|l| l.kind == "add").count();
    f.deletes = f.lines.iter().filter(|l| l.kind == "delete").count();
    f
}

/// hunk 列表 → [`Flat`](没有两侧全文,不折叠:块与块直接相接)。
#[cfg(test)]
pub(super) fn flatten(hunks: &[DiffHunk]) -> Flat {
    let mut f = flatten_hunks(hunks);
    f.relayout("", &fold::Revealed::default());
    f
}

/// 一份 diff 结果 → [`Flat`],块与块之间没改动的行按 `revealed` 展开 / 折叠(见 [`fold`])。
/// 一并返回重新认过的展开记忆(只留这一次真认出来的行,见 [`Flat::relayout`])。
pub(super) fn flatten_diff(
    diff: &GitDiffResult,
    revealed: &fold::Revealed,
) -> (Flat, fold::Revealed) {
    let mut f = flatten_hunks(&diff.hunks);
    f.gaps = fold::gaps(
        &diff.hunks,
        diff.old_content.lines().count(),
        diff.new_content.lines().count(),
    );
    let kept = f.relayout(&diff.old_content, revealed);
    (f, kept)
}

impl Flat {
    /// 排行模型:折叠段([`Flat::gaps`])与块交替排下去 —— 每段 gap 按 `revealed` 切成展开的行
    /// 与分隔行,每块在行内视图是 hunk 头 + 块里的行,在并排视图是两栏各自的行(见
    /// [`Flat::layout_hunk_split`])。展开出来的上下文行追加在 [`Flat::lines`] 末尾
    /// (块里的行下标不变,勾选不受影响)。跳转落点、量宽一并重算。
    ///
    /// `old_text` 是旧侧全文(gap 里的行两侧逐行相等,取旧侧那份)。返回这一次真认出来的
    /// 展开记忆:对不上的旧条目丢掉(见 [`fold`] 模块注释);只隔一行而直接显示的不算展开过。
    pub(super) fn relayout(&mut self, old_text: &str, revealed: &fold::Revealed) -> fold::Revealed {
        self.lines.truncate(self.hunk_lines);
        self.raw.truncate(self.hunk_lines);
        self.marks.truncate(self.hunk_lines);
        self.folds.clear();
        self.inline_rows.clear();
        self.inline_jumps.clear();
        self.split = Split::default();
        let old_lines: Vec<&str> = if self.gaps.is_empty() {
            Vec::new()
        } else {
            old_text.lines().collect()
        };
        let mut kept = fold::Revealed::default();
        let hunks = self.heads.len();
        for g in 0..=hunks {
            if let Some(gap) = self.gaps.get(g).copied() {
                self.layout_gap(g, gap, &old_lines, revealed, &mut kept);
            }
            if g == hunks {
                break;
            }
            self.inline_jumps.push(self.inline_rows.len());
            self.inline_rows.push(InlineRow::Head(g));
            self.inline_rows
                .extend((self.hunk_base[g]..self.hunk_base[g + 1]).map(InlineRow::Line));
            self.layout_hunk_split(g);
        }
        self.measure();
        kept
    }

    /// 第 `g` 块排进并排视图:上下文行两栏各一行;连续的增删行是一处改动,删除行只进左栏、
    /// 新增行只进右栏(没有占位空行)。认不出的种类不进任何一栏(原版配对时的 else 分支)。
    pub(super) fn layout_hunk_split(&mut self, g: usize) {
        self.split.begin_hunk();
        let (from, to) = (self.hunk_base[g], self.hunk_base[g + 1]);
        let mut at = from;
        while at < to {
            if self.lines[at].kind == "context" {
                self.split.push_line(at);
                at += 1;
                continue;
            }
            let start = at;
            let (mut old, mut new) = (Vec::new(), Vec::new());
            while at < to && self.lines[at].kind != "context" {
                match self.lines[at].kind.as_str() {
                    "delete" => old.push(at),
                    "add" => new.push(at),
                    _ => {}
                }
                at += 1;
            }
            if !old.is_empty() || !new.is_empty() {
                self.split.push_change(g, start..at, &old, &new);
            }
        }
    }

    /// 第 `g` 段 gap 排成行:展开的行逐行一行(两栏同一行),折着的一段一条分隔行。
    pub(super) fn layout_gap(
        &mut self,
        g: usize,
        gap: fold::Gap,
        old_lines: &[&str],
        revealed: &fold::Revealed,
        kept: &mut fold::Revealed,
    ) {
        if gap.len == 0 {
            return;
        }
        let text = |off: u32| {
            old_lines
                .get((gap.old_from + off) as usize - 1)
                .copied()
                .unwrap_or("")
        };
        // 一段几千行的 gap 一次都没展开过(绝大多数情况):不必逐行查
        let shown: Vec<bool> = if revealed.is_empty() {
            Vec::new()
        } else {
            (0..gap.len)
                .map(|off| revealed.contains(gap.old_from + off, gap.new_from + off, text(off)))
                .collect()
        };
        let is_shown = |off: u32| shown.get(off as usize).copied().unwrap_or(false);
        let hunks = self.heads.len();
        for run in fold::gap_runs(gap.len, is_shown) {
            match run {
                fold::Run::Shown(range) => {
                    for off in range {
                        let (old, new, raw) = (gap.old_from + off, gap.new_from + off, text(off));
                        if is_shown(off) {
                            kept.insert(old, new, raw);
                        }
                        self.push_context(old, new, raw);
                    }
                }
                fold::Run::Folded(range) => {
                    let ix = self.folds.len();
                    let inline_row = self.inline_rows.len();
                    self.inline_rows.push(InlineRow::Fold(ix));
                    let [old_row, new_row] = self.split.push_fold(ix);
                    self.folds.push(fold::Fold {
                        gap: g,
                        start: range.start,
                        len: range.end - range.start,
                        above: range.start > 0 || g > 0,
                        below: range.end < gap.len || g < hunks,
                        inline_row,
                        old_row,
                        new_row,
                    });
                }
            }
        }
    }

    /// 展开第 `gap` 段里从偏移 `start` 起的那条分隔行:露出来的行记进 `revealed`,再重排
    /// ([`Flat::relayout`],`revealed` 换成重新认过的那份)。返回被展开的那条分隔行(重排前
    /// 的样子,调用方据它平移选区);没有这条分隔行(行模型已经换过)是 `None`,什么都不动。
    pub(super) fn expand_fold(
        &mut self,
        old_text: &str,
        revealed: &mut fold::Revealed,
        gap: usize,
        start: u32,
        how: fold::Expand,
    ) -> Option<fold::Fold> {
        let target = self
            .folds
            .iter()
            .copied()
            .find(|f| f.gap == gap && f.start == start)?;
        let g = self.gaps.get(gap).copied()?;
        let old_lines: Vec<&str> = old_text.lines().collect();
        for off in fold::reveal_range(&target, how) {
            let (old, new) = (g.old_from + off, g.new_from + off);
            let raw = old_lines.get(old as usize - 1).copied().unwrap_or("");
            revealed.insert(old, new, raw);
        }
        *revealed = self.relayout(old_text, revealed);
        Some(target)
    }

    /// 展开出来的一行上下文:追加进 [`Flat::lines`](没有词级高亮),两个视图各一行。
    pub(super) fn push_context(&mut self, old_no: u32, new_no: u32, raw: &str) {
        let ix = self.lines.len();
        let (line, raw) = flat_line(
            "context".to_string(),
            raw.to_string(),
            Some(old_no),
            Some(new_no),
        );
        self.lines.push(line);
        self.raw.push(raw);
        self.marks.push(Vec::new());
        self.inline_rows.push(InlineRow::Line(ix));
        self.split.push_line(ix);
    }

    /// 三个量宽下标(行内视图一个、并排两栏各一个,展开出来的行也算)。一起算完再写回 ——
    /// 闭包借着 `self`,不能边借边写。hunk 头与分隔行不参与:它们钉在左缘不随横向滚动
    /// (见 `hunk_row` / `fold_row`),整行内容是绝对定位的、量出来宽度为 0
    pub(super) fn measure(&mut self) {
        let (inline_ix, sides) = {
            let cols_of = |ix: usize| text_cols(&self.lines[ix].content);
            let inline = widest(&self.inline_rows, |row| match row {
                InlineRow::Head(_) | InlineRow::Fold(_) => 0,
                InlineRow::Line(i) => cols_of(*i),
            });
            let sides = [LEFT, RIGHT].map(|side| {
                widest(&self.split.rows[side], |row| match row {
                    SideRow::Fold(_) => 0,
                    SideRow::Line(i) => cols_of(*i),
                })
            });
            (inline, sides)
        };
        self.widest_inline = inline_ix;
        self.split.widest = sides;
    }

    /// 块里各行的 `(kind, 原文)`,按拍平顺序(勾选的内容键按它算,见 [`picks`])。
    pub(super) fn pick_lines(&self) -> Vec<(&str, &str)> {
        (0..self.hunk_lines)
            .map(|ix| (self.lines[ix].kind.as_str(), self.raw_line(ix)))
            .collect()
    }

    /// 第 `hunk` 块里能打勾的行(行内视图块头的勾选框管这些)。
    pub(super) fn pickable_in(&self, hunk: usize) -> Vec<usize> {
        let (Some(&from), Some(&to)) = (self.hunk_base.get(hunk), self.hunk_base.get(hunk + 1))
        else {
            return Vec::new();
        };
        self.pickable_of(from..to)
    }

    /// 并排视图第 `change` 处改动里能打勾的行(中间栏那颗勾选框管这些)。
    pub(super) fn change_lines(&self, change: usize) -> Vec<usize> {
        self.split
            .changes
            .get(change)
            .map_or_else(Vec::new, |c| self.pickable_of(c.lines.clone()))
    }

    /// 并排视图第 `change` 处改动的身份(见 [`ChangeKey`])。
    pub(super) fn change_key(&self, change: usize) -> Option<ChangeKey> {
        let c = self.split.changes.get(change)?;
        let first = |side: usize| {
            let row = c.rows[side].clone().next()?;
            let SideRow::Line(ix) = self.split.rows[side].get(row)? else {
                return None;
            };
            let line = self.lines.get(*ix)?;
            if side == LEFT {
                line.old_lineno
            } else {
                line.new_lineno
            }
        };
        Some(ChangeKey {
            hunk: c.hunk,
            first: [first(LEFT), first(RIGHT)],
            len: [c.rows[LEFT].len(), c.rows[RIGHT].len()],
        })
    }

    /// 第 `hunk` 块的身份:块头 `@@ -a,b +c,d @@`(两侧起点与行数)。
    pub(super) fn hunk_key(&self, hunk: usize) -> Option<SharedString> {
        self.heads.get(hunk).map(|head| head.inline.clone())
    }

    /// `lines`(拍平行区间)里的增删行。
    pub(super) fn pickable_of(&self, lines: Range<usize>) -> Vec<usize> {
        lines
            .filter(|&ix| picks::pickable_kind(&self.lines[ix].kind).is_some())
            .collect()
    }

    /// 一次块操作真正落下去的行([`Flat::lines`] 下标):整块 = 块里全部增删行,按行 = 勾上的。
    pub(super) fn applied_lines(&self, picks: &[HunkPick]) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        for pick in picks {
            let Some(&base) = self.hunk_base.get(pick.hunk) else {
                continue;
            };
            match &pick.lines {
                Some(lines) => out.extend(lines.iter().map(|i| base + i)),
                None => out.extend(self.pickable_in(pick.hunk)),
            }
        }
        out
    }
}

pub(super) fn widest<T>(rows: &[T], cols: impl Fn(&T) -> usize) -> usize {
    rows.iter()
        .enumerate()
        .max_by_key(|(_, row)| cols(row))
        .map_or(0, |(ix, _)| ix)
}

/// 一个 hunk 的词级标记(`lines` 是它在 [`Flat::lines`] 里的区间):连续的增删行算一块,
/// 整块交给 [`word_diff::block_marks`];块太大 / 编辑太多时退回逐对配对(块里第 i 条删除行
/// 配第 i 条新增行,还是老算法)。纯新增 / 纯删除的块整行一个底色。
pub(super) fn mark_blocks(f: &mut Flat, lines: Range<usize>, budget: &mut usize) {
    let mut at = lines.start;
    while at < lines.end {
        if f.lines[at].kind == "context" {
            at += 1;
            continue;
        }
        let (mut dels, mut adds) = (Vec::new(), Vec::new());
        while at < lines.end && f.lines[at].kind != "context" {
            match f.lines[at].kind.as_str() {
                "delete" => dels.push(at),
                "add" => adds.push(at),
                _ => {}
            }
            at += 1;
        }
        if dels.is_empty() || adds.is_empty() {
            continue;
        }
        let text = |ix: &usize| f.lines[*ix].content.as_ref();
        let old_text: Vec<&str> = dels.iter().map(text).collect();
        let new_text: Vec<&str> = adds.iter().map(text).collect();
        match word_diff::block_marks(&old_text, &new_text, budget) {
            word_diff::BlockMarks::Lines { old, new } => {
                for (ix, marks) in dels.iter().zip(old).chain(adds.iter().zip(new)) {
                    f.marks[*ix] = marks;
                }
            }
            word_diff::BlockMarks::Fallback => {
                for (&del, &add) in dels.iter().zip(&adds) {
                    if let Some((old, new)) = intra_line_marks_within(
                        &f.lines[del].content,
                        &f.lines[add].content,
                        budget,
                    ) {
                        f.marks[del] = old;
                        f.marks[add] = new;
                    }
                }
            }
        }
    }
}
