//! 两个 diff 弹窗:
//!
//! - [`open_file_diff`] —— 工作区 / 暂存区的**单文件** diff(`src/components/DiffModal.tsx`)
//! - [`open_commit_diff`] —— 某次 commit 的**多文件** diff(`src/components/CommitDiffModal.tsx`)
//!
//! 两个视图(`InlineView` / `SideBySideView`)在原版是 `DiffModal.tsx` 导出、
//! 由 `CommitDiffModal` 复用的;这里同样只有一份([`render_inline`] / [`render_side_by_side`])。
//!
//! # 与原版的偏差(全部是有意的)
//!
//! 1. **行渲染走 [`gpui::uniform_list`](gpui::uniform_list) 虚拟化**。原版 `rows.map` 全量建 DOM ——
//!    1MB 上限(`MAX_DIFF_BYTES`)挡住了最坏情况,但一个 900KB 的文本文件仍能出
//!    ~20k 行,gpui 全量建元素会明显卡。行高恒定 = `round(fontSize*1.6)`,
//!    天然适配 uniform_list。这是**改进**而非偏差(规格 §6.6 明写)。
//! 2. **`staged` 依赖漏项顺修**。原版 effect 的依赖数组是 `[open, projectPath, status.path]`
//!    (`DiffModal.tsx:179`),**漏了 `staged`** —— 同一路径先点 staged 行再点 unstaged 行
//!    不会重新拉 diff。这里每次打开都是一个新弹窗、按 `(repo, path, staged)` 三元组
//!    取一次数,那个 bug 自然不存在(规格 §11 第 17 条建议顺修并注明)。
//! 3. **长行可横向滚动**。原版内容区是 `overflow-auto` + `whitespace-pre`,长行拖着看;
//!    `uniform_list` 默认只竖滚、且文字会回绕后被固定行高裁掉 —— 靠
//!    [`ListHorizontalSizingBehavior::Unconstrained`] + 「按最宽那一行量宽」
//!    ([`Flat::widest_inline`])补回来。⚠️ uniform_list **只量一行**来定内容宽
//!    (`uniform_list.rs:336`),量错行横向滚动范围就不够。
//! 4. **hunk 之间有 `@@ -a,b +c,d @@` 头,工具栏有「上一处/下一处改动」**。
//!    原版两样都没有,几处改动连在一起看不出断点、几千行的文件里也只能自己拖。
//! 5. **并排视图两栏纵向同步**([`DiffState::sync_columns`])。原版两栏各滚各的,
//!    对照着看要来回对齐。
//! 6. **配对成功的删/增行做词级高亮**([`intra_line_marks`])。原版一行里改一个字符
//!    也是整行涂红/绿。
//! 7. **正文用等宽字体**(与文件查看器同一条 mono 链)。原版是 `font-mono`,
//!    GPUI 侧字族不自己挂就会继承界面字体,列对不齐。
//! 8. **两个弹窗的外框与用量统计面板同尺寸**(见 [`modal_size`])。原版是
//!    `90vw×80vh` / `92vw×85vh` 两个值。
//!
//! # 判定顺序不能换
//!
//! `loading → error → isBinary → tooLarge → 正常`(`DiffModal.tsx:233-259`)。
//! 二进制文件的 `hunks` 是空的,先判 `tooLarge` 会把它显示成「文件过大」。

use std::borrow::Cow;
use std::ops::Range;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, HighlightStyle,
    InteractiveElement, IntoElement, ListHorizontalSizingBehavior, ParentElement, Pixels, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, StyledText,
    UniformListScrollHandle, Window, div, point, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use mt_project::git::{CommitFileInfo, DiffHunk, DiffLine, GitDiffResult};
use mt_ui::tooltip::Tooltip;

use crate::i18n::{t, tr};
use crate::prompt::{kind, open_guarded};
use crate::store::AppStore;
use crate::tab_expansion::{TAB_WIDTH, expand_line};
use crate::ui;

/// 视图模式。**组件态,不落盘**;默认 side-by-side(`DiffModal.tsx:158`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    SideBySide,
    Inline,
}

// ─── 行配对(SideBySideView 的核心算法) ────────────────────────

/// 把全部 hunk 的行拍平,并算出左右两栏的配对。
///
/// 逐条移植 `DiffModal.tsx:63-97`:
///
/// ```text
/// context        → 左右同一行
/// delete         → 连续吃掉所有 delete,再连续吃掉紧随的 add,按下标配对
///                  (短的一侧留空)
/// add            → 只出现在右栏
/// ```
///
/// 返回 `(拍平的行, 配对下标)`。配对**不跨 hunk**(原版每个 hunk 重置 `i`)。
pub fn pair_rows(hunks: &[DiffHunk]) -> (Vec<DiffLine>, Vec<(Option<usize>, Option<usize>)>) {
    let mut lines: Vec<DiffLine> = Vec::new();
    let mut rows: Vec<(Option<usize>, Option<usize>)> = Vec::new();

    for hunk in hunks {
        let base = lines.len();
        lines.extend(hunk.lines.iter().cloned());
        let hunk_lines = &hunk.lines;

        let mut i = 0usize;
        while i < hunk_lines.len() {
            match hunk_lines[i].kind.as_str() {
                "context" => {
                    rows.push((Some(base + i), Some(base + i)));
                    i += 1;
                }
                "delete" => {
                    let mut deletes = Vec::new();
                    while i < hunk_lines.len() && hunk_lines[i].kind == "delete" {
                        deletes.push(base + i);
                        i += 1;
                    }
                    let mut adds = Vec::new();
                    while i < hunk_lines.len() && hunk_lines[i].kind == "add" {
                        adds.push(base + i);
                        i += 1;
                    }
                    let max_len = deletes.len().max(adds.len());
                    for j in 0..max_len {
                        rows.push((deletes.get(j).copied(), adds.get(j).copied()));
                    }
                }
                "add" => {
                    rows.push((None, Some(base + i)));
                    i += 1;
                }
                // 认不出的 kind 直接跳过(原版的 else 分支)
                _ => i += 1,
            }
        }
    }

    (lines, rows)
}

// ─── 行内(词级)差异 ─────────────────────────────────────────
//
// 配对成功的那对 delete/add 行再跑一次词级 diff,只把**真正变了的片段**涂实
// (`ui::diff_*_word_bg`)。原版没有这一层:一行里改一个字符,整行都是红/绿。

/// 参与词级 diff 的单行长度上限(字节)。再长就整行涂 —— 压缩过的 JS / base64
/// 这类超长行本来也没有词界,而 DP 是 O(m·n),放开会直接卡帧。
const INTRA_LINE_MAX_BYTES: usize = 2000;

/// 剥掉公共前后缀**之后**还允许的 DP 规模(格子数)。
const INTRA_LINE_MAX_CELLS: usize = 120_000;

/// 变动词元占比超过它就不细分 —— 整行都变了的时候,细分出来的高亮全是噪音,
/// 还不如原版那样整行一个底色干净。
const INTRA_LINE_NOISE_RATIO: f32 = 0.6;

/// 词元类别。相邻同类字符并成一个词元;`Other`(标点/符号)一个字符一个词元。
///
/// ⚠️ 只有 **ASCII** 字母数字算 `Word`:中日韩文字按 `Other` 逐字切,否则一整句
/// 中文会并成一个词元,改一个字 = 整句高亮,等于没细分。
#[derive(PartialEq, Eq, Clone, Copy)]
enum CharClass {
    Word,
    Space,
    Other,
}

fn char_class(c: char) -> CharClass {
    if c.is_ascii_alphanumeric() || c == '_' {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Other
    }
}

/// 一行 → 词元序列(字节区间 + 文本)。
fn tokenize(s: &str) -> Vec<(Range<usize>, &str)> {
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
fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
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

/// 一对 delete/add 行的词级差异 → 两侧各自「变了的字节区间」。
///
/// `None` = 不细分(两行相同 / 太长 / 改动占比过高)。
fn intra_line_marks(old: &str, new: &str) -> Option<(Vec<Range<usize>>, Vec<Range<usize>>)> {
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
    while tail < a.len() - head && tail < b.len() - head
        && a[a.len() - 1 - tail].1 == b[b.len() - 1 - tail].1
    {
        tail += 1;
    }
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    if am.is_empty() && bm.is_empty() {
        return None;
    }
    if am.len() * bm.len() > INTRA_LINE_MAX_CELLS {
        return None;
    }

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
#[derive(Clone, Copy)]
enum InlineRow {
    /// 第 n 个 hunk 的 `@@` 头
    Head(usize),
    /// [`Flat::lines`] 的下标
    Line(usize),
}

/// 并排视图的一行。两栏行数必须**完全一致**,否则左右对不上。
#[derive(Clone, Copy)]
enum PairRow {
    Head(usize),
    Line(Option<usize>, Option<usize>),
}

/// hunk 头的三种写法:inline 一整串,并排视图左右各半(各显示自己那侧的区间)。
struct HunkHead {
    inline: SharedString,
    left: SharedString,
    right: SharedString,
}

fn hunk_head(h: &DiffHunk) -> HunkHead {
    HunkHead {
        inline: format!(
            "@@ -{},{} +{},{} @@",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        )
        .into(),
        left: format!("@@ -{},{} @@", h.old_start, h.old_lines).into(),
        right: format!("@@ +{},{} @@", h.new_start, h.new_lines).into(),
    }
}

/// 一次 diff 结果算好的**全部派生数据**。结果一到就算一次,不在每帧的
/// uniform_list 回调里重算。
#[derive(Default)]
struct Flat {
    /// 拍平的行(两个视图共用一份)
    lines: Vec<DiffLine>,
    /// 与 [`Flat::lines`] 等长:每行的行内改动片段。空 = 整行一个底色
    marks: Vec<Vec<Range<usize>>>,
    heads: Vec<HunkHead>,
    inline_rows: Vec<InlineRow>,
    pair_rows: Vec<PairRow>,
    /// 「上一处/下一处改动」的落点 = 每个 hunk 头所在的行下标
    inline_jumps: Vec<usize>,
    pair_jumps: Vec<usize>,
    /// 给 uniform_list 量宽用的「最宽的那一行」。三个视图各一份
    widest_inline: usize,
    widest_left: usize,
    widest_right: usize,
}

/// 行宽的近似列数:CJK/全角按 2,其余按 1。
///
/// 只用来在一堆行里挑出最宽的那一条喂给 `with_width_from_item`,不求精确 ——
/// 挑错了顶多横向滚动范围差一点,不影响正确性。
fn text_cols(s: &str) -> usize {
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

/// hunk 列表 → [`Flat`]。配对本身仍走 [`pair_rows`](逐 hunk 调用,保持
/// 「配对不跨 hunk」那条不变量)。
fn flatten(hunks: &[DiffHunk]) -> Flat {
    let mut f = Flat::default();

    for (hi, hunk) in hunks.iter().enumerate() {
        let base = f.lines.len();
        let (lines, pairs) = pair_rows(std::slice::from_ref(hunk));
        // `\t` 在 gpui 文本里是零宽(见 `tab_expansion` 模块注释),显示前按制表位展开。
        // 词级高亮与量宽都在展开后的文本上算,字节区间才对得上
        let lines: Vec<DiffLine> = lines
            .into_iter()
            .map(|mut line| {
                if let Cow::Owned(expanded) = expand_line(&line.content, TAB_WIDTH) {
                    line.content = expanded;
                }
                line
            })
            .collect();

        f.heads.push(hunk_head(hunk));
        f.inline_jumps.push(f.inline_rows.len());
        f.inline_rows.push(InlineRow::Head(hi));
        f.pair_jumps.push(f.pair_rows.len());
        f.pair_rows.push(PairRow::Head(hi));

        f.inline_rows
            .extend((0..lines.len()).map(|i| InlineRow::Line(base + i)));
        f.marks.extend((0..lines.len()).map(|_| Vec::new()));
        f.lines.extend(lines);

        for (left, right) in pairs {
            let (left, right) = (left.map(|i| i + base), right.map(|i| i + base));
            if let (Some(li), Some(ri)) = (left, right)
                && f.lines[li].kind == "delete"
                && f.lines[ri].kind == "add"
                && let Some((del, add)) =
                    intra_line_marks(&f.lines[li].content, &f.lines[ri].content)
            {
                f.marks[li] = del;
                f.marks[ri] = add;
            }
            f.pair_rows.push(PairRow::Line(left, right));
        }
    }

    // 三个量宽下标一起算完再写回 —— 闭包借着 `f`,不能边借边写
    let (inline_ix, left_ix, right_ix) = {
        let cols_of = |ix: Option<usize>| ix.map_or(0, |i| text_cols(&f.lines[i].content));
        (
            widest(&f.inline_rows, |row| match row {
                InlineRow::Head(h) => text_cols(&f.heads[*h].inline),
                InlineRow::Line(i) => cols_of(Some(*i)),
            }),
            widest(&f.pair_rows, |row| match row {
                PairRow::Head(h) => text_cols(&f.heads[*h].left),
                PairRow::Line(l, _) => cols_of(*l),
            }),
            widest(&f.pair_rows, |row| match row {
                PairRow::Head(h) => text_cols(&f.heads[*h].right),
                PairRow::Line(_, r) => cols_of(*r),
            }),
        )
    };
    f.widest_inline = inline_ix;
    f.widest_left = left_ix;
    f.widest_right = right_ix;
    f
}

fn widest<T>(rows: &[T], cols: impl Fn(&T) -> usize) -> usize {
    rows.iter()
        .enumerate()
        .max_by_key(|(_, row)| cols(row))
        .map_or(0, |(ix, _)| ix)
}

fn line_bg(kind: &str) -> Option<gpui::Hsla> {
    match kind {
        "add" => Some(ui::diff_add_bg()),
        "delete" => Some(ui::diff_del_bg()),
        _ => None,
    }
}

fn line_fg(kind: &str) -> gpui::Hsla {
    match kind {
        "add" => ui::diff_add_text(),
        "delete" => ui::diff_del_text(),
        _ => ui::text_primary(),
    }
}

/// 行号列宽(`DiffModal.tsx:38,116` 的 `w-[48px]`)。
const GUTTER: f32 = 48.0;

/// 行内高亮的底色(与整行底色叠在一起,所以要更实)。
fn word_bg(kind: &str) -> gpui::Hsla {
    if kind == "add" {
        ui::diff_add_word_bg()
    } else {
        ui::diff_del_word_bg()
    }
}

/// 一行:行号列 + 内容列。`gutter` 是行号列要显示的文本,`marks` 是这一行里
/// **真正变了的片段**(空 = 整行一个底色,与原版一致)。
fn diff_line_row(
    line: &DiffLine,
    gutter: String,
    line_height: f32,
    marks: &[Range<usize>],
) -> AnyElement {
    let kind = line.kind.as_str();
    let content = SharedString::from(line.content.clone());
    // `whitespace-pre`:diff 行不换行 —— 少了它 gpui 会把长行回绕,而行高是恒定的
    // (uniform_list 的前提),回绕出来的第二行直接被裁掉,看着像「缺了半行」
    let mut text = div()
        .flex_1()
        .px(px(8.0))
        .whitespace_nowrap()
        .text_color(line_fg(kind));
    if marks.is_empty() {
        text = text.child(content);
    } else {
        let style = HighlightStyle {
            background_color: Some(word_bg(kind)),
            ..Default::default()
        };
        text = text.child(
            StyledText::new(content)
                .with_highlights(marks.iter().map(|r| (r.clone(), style))),
        );
    }

    div()
        .flex()
        .h(px(line_height))
        .when_some(line_bg(kind), |el, bg| el.bg(bg))
        .child(
            div()
                .w(px(GUTTER))
                .flex_none()
                .pr(px(8.0))
                .text_right()
                .text_color(ui::text_muted())
                .opacity(0.5)
                .child(gutter),
        )
        .child(text)
        .into_any_element()
}

/// hunk 头(`@@ -a,b +c,d @@`)。
///
/// ⚠️ 高度必须**恰好**是 `line_height`:uniform_list 按「量到的那一行」的高度
/// 摆所有行,这里多一像素边框,整份 diff 的行位就会与滚动条对不上。分隔靠底色。
fn hunk_row(text: SharedString, line_height: f32) -> AnyElement {
    div()
        .flex()
        .h(px(line_height))
        .bg(ui::bg_elevated())
        .child(div().w(px(GUTTER)).flex_none())
        .child(
            div()
                .flex_1()
                .px(px(8.0))
                .whitespace_nowrap()
                .text_color(ui::accent())
                .opacity(0.7)
                .child(text),
        )
        .into_any_element()
}

// ─── 弹窗外框尺寸 ─────────────────────────────────────────────
//
// 两个 diff 弹窗与**用量统计面板**同一口径(`main.rs` 的 `usage_layer`:
// 左右各留 10vw、顶 10vh、底 5vh → 80vw × 85vh)。`Dialog` 的默认顶距正好是
// `视口高/10`(`dialog.rs:367`),所以只要宽取 80vw、正文高取 85vh,两个弹窗
// 的外框就与用量面板完全重合。
//
// ⚠️ 配套必须 `p_0()`:`Dialog` 默认 `pt/pb 24 + 左右 24`,不清零的话面板实际
// 高是 85vh+48px(底边越过 95vh),工具栏也不会像原版那样贴着面板边。
//
// 原版这两个弹窗各是 `w-[90vw] h-[80vh]` / `w-[92vw] h-[85vh]`
// (`DiffModal.tsx:186`、`CommitDiffModal.tsx:87`),此处按「与统计弹窗一致」
// 的要求统一收口,是**有意偏差**。
const MODAL_W_RATIO: f32 = 0.8;
const MODAL_H_RATIO: f32 = 0.85;

/// 返回 `(面板宽, 正文高)`。builder 每帧重跑,拖窗口改大小时跟着变。
fn modal_size(viewport: gpui::Size<gpui::Pixels>) -> (gpui::Pixels, gpui::Pixels) {
    (
        ui::clamp_dialog_width(viewport.width * MODAL_W_RATIO, viewport),
        // `chrome = 0`:正文即整块面板(标题/页脚都没有),下限交给 helper
        ui::clamp_dialog_body_height(viewport.height, viewport, MODAL_H_RATIO, px(0.0)),
    )
}

/// 空格子(`DiffModal.tsx:100-107`)。
fn empty_cell(line_height: f32) -> AnyElement {
    div()
        .flex()
        .h(px(line_height))
        .bg(ui::bg_base())
        .opacity(0.3)
        .child(div().w(px(GUTTER)).flex_none())
        .child(div().flex_1())
        .into_any_element()
}

// ─── 弹窗内的状态实体 ─────────────────────────────────────────

/// 两个弹窗共用的 diff 内容状态。
struct DiffState {
    loading: bool,
    error: Option<String>,
    result: Option<GitDiffResult>,
    /// 结果一到就算好的派生数据,不在每帧的 uniform_list 回调里重算。
    flat: Flat,
    view: ViewMode,
    font_size: f32,
    /// 请求令牌:换文件时旧响应不许覆盖。
    request: u64,
    split: Entity<ResizableState>,
    /// 三个列表各自的滚动句柄:跳转要用,并排两栏还要靠它对齐。
    inline_scroll: UniformListScrollHandle,
    left_scroll: UniformListScrollHandle,
    right_scroll: UniformListScrollHandle,
    /// 两栏纵向同步的基准值,见 [`DiffState::sync_columns`]。
    synced_y: Pixels,
    /// 「上一处/下一处改动」走到第几个 hunk。`None` = 还没跳过。
    jump: Option<usize>,
}

impl DiffState {
    fn new(font_size: f32, cx: &mut App) -> Self {
        Self {
            loading: true,
            error: None,
            result: None,
            flat: Flat::default(),
            view: ViewMode::SideBySide,
            font_size,
            request: 0,
            split: cx.new(|_| ResizableState::default()),
            inline_scroll: UniformListScrollHandle::new(),
            left_scroll: UniformListScrollHandle::new(),
            right_scroll: UniformListScrollHandle::new(),
            synced_y: px(0.0),
            jump: None,
        }
    }

    fn line_height(&self) -> f32 {
        (self.font_size * 1.6).round()
    }

    fn apply(&mut self, result: anyhow::Result<GitDiffResult>) {
        self.loading = false;
        self.jump = None;
        self.synced_y = px(0.0);
        match result {
            Ok(diff) => {
                self.flat = flatten(&diff.hunks);
                self.result = Some(diff);
                self.error = None;
            }
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                self.result = None;
                self.flat = Flat::default();
            }
        }
    }

    /// 并排两栏的纵向同步。
    ///
    /// `Dialog` 的 builder 每帧重跑(滚轮改了偏移就 `notify`),这里比对两个句柄
    /// 与上一帧基准:谁动了谁当主,另一边跟上。
    ///
    /// **只同步纵向**:横向内容宽是两栏各自按「最宽那一行」量出来的,共享 x 会被
    /// 各自的钳制来回推(窄的那栏一到边就把 x 抹回去),越同步越抖。
    ///
    /// 两栏行数与行高完全一致 ⇒ 纵向的可滚范围也一致,不会互相钳。
    fn sync_columns(&mut self) {
        let left = self.left_scroll.0.borrow().base_handle.offset();
        let right = self.right_scroll.0.borrow().base_handle.offset();
        let target = if left.y != self.synced_y {
            left.y
        } else if right.y != self.synced_y {
            right.y
        } else {
            return;
        };
        if left.y != target {
            self.left_scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(left.x, target));
        }
        if right.y != target {
            self.right_scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(right.x, target));
        }
        self.synced_y = target;
    }

    /// 跳到上/下一处改动(= 上/下一个 hunk 头)。`delta` 取 ±1,到头绕回。
    fn jump_by(&mut self, delta: isize) {
        let total = self.flat.heads.len();
        if total == 0 {
            return;
        }
        let next = match self.jump {
            None if delta > 0 => 0,
            None => total - 1,
            Some(cur) => (cur as isize + delta).rem_euclid(total as isize) as usize,
        };
        self.jump = Some(next);
        match self.view {
            // strict:哪怕目标已在视野里也要把它顶到最上面 —— 连着点「下一处」
            // 时,不这么做的话相邻两个 hunk 会看不出画面变化
            ViewMode::Inline => self
                .inline_scroll
                .scroll_to_item_strict(self.flat.inline_jumps[next], ScrollStrategy::Top),
            ViewMode::SideBySide => {
                let ix = self.flat.pair_jumps[next];
                self.left_scroll
                    .scroll_to_item_strict(ix, ScrollStrategy::Top);
                self.right_scroll
                    .scroll_to_item_strict(ix, ScrollStrategy::Top);
            }
        }
    }
}

impl Render for DiffState {
    /// `DiffState` 只当状态盒子用(Dialog 的 builder 是 `Fn`,每帧重跑,
    /// 编辑中的状态不能藏在闭包捕获里)。它自己不画东西。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

// ─── 内容区渲染 ───────────────────────────────────────────────

fn centered(text: impl Into<SharedString>, color: gpui::Hsla) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(ui::font_px(13.0))
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

/// 内容区的五选一(顺序不能换,见模块注释)。`labels` 是四条文案
/// (loading / binary / tooLarge / 额外的空态),两个弹窗各给各的命名空间。
fn render_body(
    state: &Entity<DiffState>,
    loading: &'static str,
    binary: &'static str,
    too_large: &'static str,
    cx: &mut App,
) -> AnyElement {
    let s = state.read(cx);
    if s.loading {
        return centered(loading, ui::text_muted());
    }
    if let Some(err) = &s.error {
        return centered(err.clone(), ui::color_error());
    }
    let Some(result) = &s.result else {
        return div().into_any_element();
    };
    if result.is_binary {
        return centered(binary, ui::text_muted());
    }
    if result.too_large {
        return centered(too_large, ui::text_muted());
    }
    let view = match s.view {
        ViewMode::Inline => render_inline(state, cx),
        ViewMode::SideBySide => render_side_by_side(state, cx),
    };
    mono_body(view)
}

/// diff 正文的等宽字体壳。
///
/// 原版是 `font-mono`(`--app-font-mono`:JetBrains Mono → Cascadia Code →
/// Consolas);gpui 的字族是单值 + fallback 链,这里与文件查看器
/// (`file_viewer.rs` 的编辑器分支)用同一条链,连「用户配过 uiFontFamily 就让它
/// 优先」的口径也一致(原版 `fontManager.ts:8-18` 会一并覆盖 `--app-font-mono`)。
///
/// 不挂字族的话 diff 会继承界面字体 —— 比例字体下行号与代码列全对不齐。
fn mono_body(body: AnyElement) -> AnyElement {
    let mut wrap = div().size_full();
    let ts = wrap.text_style().get_or_insert_default();
    ts.font_family = Some(ui::ui_font_family().unwrap_or_else(|| "Cascadia Code".into()));
    ts.font_fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
        "Cascadia Mono".into(),
        "Consolas".into(),
        "JetBrains Mono".into(),
        "Microsoft YaHei".into(),
        "Segoe UI Emoji".into(),
    ]));
    wrap.child(body).into_any_element()
}

/// `InlineView`(`DiffModal.tsx:22-58`),外加 hunk 头与横向滚动。
fn render_inline(state: &Entity<DiffState>, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let count = s.flat.inline_rows.len();
    let line_height = s.line_height();
    let font_size = s.font_size;
    let widest_ix = s.flat.widest_inline;
    let scroll = s.inline_scroll.clone();
    let state = state.clone();
    uniform_list(
        "git-diff-inline",
        count,
        move |range, _window, cx: &mut App| {
            let s = state.read(cx);
            range
                .map(|i| match s.flat.inline_rows[i] {
                    InlineRow::Head(h) => hunk_row(s.flat.heads[h].inline.clone(), line_height),
                    InlineRow::Line(ix) => {
                        let line = &s.flat.lines[ix];
                        let gutter = match line.kind.as_str() {
                            "add" => "+".to_string(),
                            "delete" => "-".to_string(),
                            _ => line.old_lineno.map(|n| n.to_string()).unwrap_or_default(),
                        };
                        diff_line_row(line, gutter, line_height, &s.flat.marks[ix])
                    }
                })
                .collect::<Vec<_>>()
        },
    )
    .size_full()
    .text_size(ui::font_px(font_size))
    .track_scroll(scroll)
    // 见模块注释第 3 条:量宽只量这一行,量错了长行就滚不到头
    .with_width_from_item(Some(widest_ix))
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .into_any_element()
}

/// `SideBySideView`(`DiffModal.tsx:62-152`),外加 hunk 头、横向滚动与纵向同步。
fn render_side_by_side(state: &Entity<DiffState>, cx: &mut App) -> AnyElement {
    // 每帧一次:上一帧谁滚了,另一边这一帧跟上(见 [`DiffState::sync_columns`])
    state.update(cx, |s, _| s.sync_columns());

    let s = state.read(cx);
    let count = s.flat.pair_rows.len();
    let line_height = s.line_height();
    let font_size = s.font_size;
    let split = s.split.clone();
    let (widest_left, widest_right) = (s.flat.widest_left, s.flat.widest_right);
    let (left_scroll, right_scroll) = (s.left_scroll.clone(), s.right_scroll.clone());

    let column = |side_left: bool, widest_ix: usize, scroll: UniformListScrollHandle| {
        let state = state.clone();
        uniform_list(
            if side_left {
                "git-diff-left"
            } else {
                "git-diff-right"
            },
            count,
            move |range, _window, cx: &mut App| {
                let s = state.read(cx);
                range
                    .map(|i| match s.flat.pair_rows[i] {
                        // 两栏各显示自己那侧的区间,行数仍一一对应
                        PairRow::Head(h) => {
                            let head = &s.flat.heads[h];
                            let text = if side_left {
                                head.left.clone()
                            } else {
                                head.right.clone()
                            };
                            hunk_row(text, line_height)
                        }
                        PairRow::Line(left, right) => {
                            let index = if side_left { left } else { right };
                            match index {
                                None => empty_cell(line_height),
                                Some(index) => {
                                    let line = &s.flat.lines[index];
                                    // 左栏显示 oldLineno、右栏显示 newLineno
                                    let no = if side_left {
                                        line.old_lineno
                                    } else {
                                        line.new_lineno
                                    };
                                    diff_line_row(
                                        line,
                                        no.map(|n| n.to_string()).unwrap_or_default(),
                                        line_height,
                                        &s.flat.marks[index],
                                    )
                                }
                            }
                        }
                    })
                    .collect::<Vec<_>>()
            },
        )
        .size_full()
        .track_scroll(scroll)
        .with_width_from_item(Some(widest_ix))
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    };

    div()
        .size_full()
        .text_size(ui::font_px(font_size))
        .child(
            h_resizable("git-diff-columns")
                .with_state(&split)
                .child(
                    resizable_panel().child(
                        div()
                            .size_full()
                            .child(column(true, widest_left, left_scroll)),
                    ),
                )
                .child(
                    resizable_panel().child(
                        div()
                            .size_full()
                            .child(column(false, widest_right, right_scroll)),
                    ),
                ),
        )
        .into_any_element()
}

/// 工具栏右上角那组控件的文案。两个弹窗各有各的命名空间(`diffModal` /
/// `commitDiff`),所以只能由调用方喂进来。
struct ToolbarLabels {
    side: &'static str,
    inline: &'static str,
    prev: &'static str,
    next: &'static str,
}

/// 「上一处 / 下一处改动」按钮。
fn jump_button(
    state: &Entity<DiffState>,
    id: SharedString,
    glyph: &'static str,
    tip: &'static str,
    delta: isize,
) -> AnyElement {
    let state = state.clone();
    div()
        .id(id)
        .w(px(22.0))
        .h(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_muted())
        .cursor_pointer()
        .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .child(glyph)
        .on_click(move |_: &ClickEvent, _window, cx| {
            state.update(cx, |s, cx| {
                s.jump_by(delta);
                cx.notify();
            });
        })
        .into_any_element()
}

/// 改动跳转组(↑ / 计数 / ↓)。没有 hunk(空 diff / 还在加载)时整组不画。
fn jump_group(
    state: &Entity<DiffState>,
    id_prefix: &'static str,
    labels: &ToolbarLabels,
    cx: &App,
) -> Option<AnyElement> {
    let s = state.read(cx);
    let total = s.flat.heads.len();
    if total == 0 {
        return None;
    }
    // 还没跳过时不硬说「第 1 处」—— 视野里那处是哪一处,滚动位置说了不算
    let counter = match s.jump {
        Some(cur) => format!("{}/{}", cur + 1, total),
        None => format!("–/{}", total),
    };
    Some(
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .mr(px(8.0))
            .child(jump_button(
                state,
                SharedString::from(format!("{id_prefix}-jump-prev")),
                "↑",
                labels.prev,
                -1,
            ))
            .child(
                div()
                    .px(px(2.0))
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(counter),
            )
            .child(jump_button(
                state,
                SharedString::from(format!("{id_prefix}-jump-next")),
                "↓",
                labels.next,
                1,
            ))
            .into_any_element(),
    )
}

/// 跳转组 + 视图段控件 + ✕(两个弹窗共用的右上角)。
fn view_toggle(
    state: &Entity<DiffState>,
    id_prefix: &'static str,
    labels: ToolbarLabels,
    close_kind: &'static str,
    cx: &mut App,
) -> AnyElement {
    let (side_label, inline_label) = (labels.side, labels.inline);
    let jumps = jump_group(state, id_prefix, &labels, cx);
    let current = state.read(cx).view;
    let mut seg = div()
        .flex()
        .rounded(px(4.0))
        .overflow_hidden()
        .border_1()
        .border_color(ui::border_default());
    for (mode, label) in [
        (ViewMode::SideBySide, side_label),
        (ViewMode::Inline, inline_label),
    ] {
        let active = mode == current;
        let state = state.clone();
        seg = seg.child(
            div()
                .id(SharedString::from(format!(
                    "{id_prefix}-view-{}",
                    if matches!(mode, ViewMode::Inline) {
                        "inline"
                    } else {
                        "side"
                    }
                )))
                .px(px(12.0))
                .py(px(4.0))
                .text_size(ui::font_px(13.0))
                .cursor_pointer()
                .when(active, |el| {
                    el.bg(ui::accent_subtle()).text_color(ui::accent())
                })
                .when(!active, |el| {
                    el.text_color(ui::text_muted())
                        .hover(|el| el.text_color(ui::text_primary()))
                })
                .child(label)
                .on_click(move |_: &ClickEvent, _window, cx| {
                    state.update(cx, |s, cx| {
                        s.view = mode;
                        cx.notify();
                    });
                }),
        );
    }

    div()
        .flex()
        .items_center()
        .children(jumps)
        .child(seg)
        .child(
            div()
                .id(SharedString::from(format!("{id_prefix}-close")))
                .ml(px(8.0))
                .text_size(ui::font_px(18.0))
                .text_color(ui::text_muted())
                .cursor_pointer()
                .hover(|el| el.text_color(ui::color_error()))
                .child("✕")
                .on_click(move |_: &ClickEvent, window, cx| {
                    crate::prompt::close_guarded(close_kind, window, cx);
                }),
        )
        .into_any_element()
}

// ─── DiffModal(工作区/暂存区单文件) ──────────────────────────

/// 打开单文件 diff。`staged` 决定取暂存区还是工作区那一侧。
pub fn open_file_diff(
    store: Entity<AppStore>,
    repo_path: String,
    file_path: String,
    staged: bool,
    status_label: String,
    window: &mut Window,
    cx: &mut App,
) {
    if repo_path.is_empty() {
        return;
    }
    let font_size = store.read(cx).config().terminal_font_size as f32;
    let state = cx.new(|cx| DiffState::new(font_size, cx));

    // 取数:`(repo, path, staged)` 三元组一次到位(原版漏了 staged,见模块注释)
    {
        let (repo, path) = (repo_path.clone(), file_path.clone());
        let state = state.clone();
        cx.spawn(async move |cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    mt_project::git::get_git_diff(
                        std::path::Path::new(&repo),
                        &path,
                        Some(staged),
                    )
                })
                .await;
            let _ = state.update(cx, |s: &mut DiffState, cx| {
                s.apply(result);
                cx.notify();
            });
        })
        .detach();
    }

    let file_name = file_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&file_path)
        .to_string();

    open_guarded(kind::GIT_DIFF, window, cx, move |dialog, window, cx| {
        let viewport = window.viewport_size();
        let body = render_body(
            &state,
            t("diffModal", "loading"),
            t("diffModal", "binaryNotSupported"),
            t("diffModal", "tooLarge"),
            cx,
        );
        let toolbar = div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(16.0))
            .py(px(12.0))
            .flex_none()
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_size(ui::font_px(15.0))
                            .text_color(ui::accent())
                            .child(file_name.clone()),
                    )
                    .child(
                        div()
                            .max_w(px(300.0))
                            .truncate()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_muted())
                            .child(file_path.clone()),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(ui::bg_elevated())
                            .border_1()
                            .border_color(ui::border_subtle())
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(status_label.clone()),
                    ),
            )
            .child(view_toggle(
                &state,
                "git-diff",
                ToolbarLabels {
                    side: t("diffModal", "sideBySide"),
                    inline: t("diffModal", "inline"),
                    prev: t("diffModal", "prevChange"),
                    next: t("diffModal", "nextChange"),
                },
                kind::GIT_DIFF,
                cx,
            ));

        let (width, body_h) = modal_size(viewport);
        dialog
            .w(width)
            // 见 [`modal_size`]:不清零内边距的话面板会比统计弹窗高出 48px
            .p_0()
            // `Dialog` 自带的 ✕ 画 `IconName::Close`,0.5.1 没有 svg 资产 →
            // 渲染成空白,等于在右上角埋一块看不见的可点区(`p_0()` 之下它更是
            // 贴到 8,8,正压着工具栏这边自绘的 ✕ 与视图切换)。与设置面板同一取舍
            .close_button(false)
            .child(
                div()
                    .w_full()
                    .h(body_h)
                    .flex()
                    .flex_col()
                    // 工具栏/正文各自有底色,不裁会盖掉面板的圆角
                    .overflow_hidden()
                    .child(toolbar)
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_hidden()
                            .bg(ui::bg_base())
                            .child(body),
                    ),
            )
    });
}

// ─── CommitDiffModal(某次 commit 的多文件) ──────────────────

/// 左栏文件列表的状态字母表(`CommitDiffModal.tsx:21-26`)。
/// 查不到(conflicted / untracked 之类)回落 `?` + muted。
fn commit_file_badge(status: &str) -> (&'static str, gpui::Hsla) {
    match status {
        "added" => ("A", ui::color_success()),
        "modified" => ("M", ui::color_warning()),
        "deleted" => ("D", ui::color_error()),
        "renamed" => ("R", ui::color_info()),
        _ => ("?", ui::text_muted()),
    }
}

/// 左栏选中项。放进实体是因为 Dialog 的 builder 每帧重跑。
struct CommitPick {
    selected: String,
}

impl Render for CommitPick {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 打开某次 commit 的多文件 diff。
pub fn open_commit_diff(
    store: Entity<AppStore>,
    repo_path: String,
    commit_hash: String,
    commit_message: String,
    files: Vec<CommitFileInfo>,
    window: &mut Window,
    cx: &mut App,
) {
    let font_size = store.read(cx).config().terminal_font_size as f32;
    let state = cx.new(|cx| DiffState::new(font_size, cx));
    let first = files.first().map(|f| f.path.clone()).unwrap_or_default();
    let pick = cx.new(|_| CommitPick {
        selected: first.clone(),
    });

    if !first.is_empty() {
        load_commit_file(&state, &repo_path, &commit_hash, &files, &first, cx);
    } else {
        state.update(cx, |s, _| {
            s.loading = false;
        });
    }

    let short_hash: String = commit_hash.chars().take(7).collect();

    open_guarded(
        kind::GIT_COMMIT_DIFF,
        window,
        cx,
        move |dialog, window, cx| {
            let viewport = window.viewport_size();
            let selected = pick.read(cx).selected.clone();

            // 左栏
            let mut file_list = div()
                .id("git-commit-files")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll();
            for (idx, file) in files.iter().enumerate() {
                let (letter, color) = commit_file_badge(&file.status);
                let active = file.path == selected;
                let name = file
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&file.path)
                    .to_string();
                let (pick, state) = (pick.clone(), state.clone());
                let (repo, hash, files_for_click, path) = (
                    repo_path.clone(),
                    commit_hash.clone(),
                    files.clone(),
                    file.path.clone(),
                );
                file_list = file_list.child(
                    div()
                        .id(SharedString::from(format!("git-commit-file-{idx}")))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(12.0))
                        .py(px(6.0))
                        .cursor_pointer()
                        .text_size(ui::font_px(13.0))
                        .when(active, |el| {
                            el.bg(ui::accent_subtle()).text_color(ui::accent())
                        })
                        .when(!active, |el| {
                            el.text_color(ui::text_primary())
                                .hover(|el| el.bg(ui::border_subtle()))
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_size(ui::font_px(11.0))
                                .text_color(color)
                                .child(letter),
                        )
                        .child(div().truncate().child(name))
                        .on_click(move |_: &ClickEvent, _window, cx| {
                            if pick.read(cx).selected == path {
                                return;
                            }
                            pick.update(cx, |p, cx| {
                                p.selected = path.clone();
                                cx.notify();
                            });
                            load_commit_file(&state, &repo, &hash, &files_for_click, &path, cx);
                        }),
                );
            }

            let left = div()
                .w(px(224.0))
                .flex_none()
                .h_full()
                .flex()
                .flex_col()
                .border_r_1()
                .border_color(ui::border_subtle())
                .bg(ui::bg_elevated())
                .child(
                    div()
                        .px(px(12.0))
                        .py(px(12.0))
                        .flex_none()
                        .border_b_1()
                        .border_color(ui::border_subtle())
                        .child(
                            div()
                                .truncate()
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::accent())
                                .child(commit_message.clone()),
                        )
                        .child(
                            div()
                                .mt(px(4.0))
                                .text_size(ui::font_px(11.0))
                                .text_color(ui::text_muted())
                                .child(short_hash.clone()),
                        ),
                )
                .child(file_list)
                .child(
                    div()
                        .px(px(12.0))
                        .py(px(8.0))
                        .flex_none()
                        .border_t_1()
                        .border_color(ui::border_subtle())
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(tr!(
                            "commitDiff",
                            "fileCount",
                            count = files.len().to_string()
                        )),
                );

            // 右栏
            let body = if files.is_empty() {
                centered(t("commitDiff", "noChanges"), ui::text_muted())
            } else {
                render_body(
                    &state,
                    t("commitDiff", "loading"),
                    t("commitDiff", "binaryFile"),
                    t("commitDiff", "tooLarge"),
                    cx,
                )
            };
            let right = div()
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(16.0))
                        .py(px(12.0))
                        .flex_none()
                        .border_b_1()
                        .border_color(ui::border_subtle())
                        .child(
                            div()
                                .max_w(px(400.0))
                                .truncate()
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::text_primary())
                                .child(selected.clone()),
                        )
                        .child(view_toggle(
                            &state,
                            "git-commit-diff",
                            ToolbarLabels {
                                side: t("commitDiff", "sideBySide"),
                                inline: t("commitDiff", "inline"),
                                prev: t("commitDiff", "prevChange"),
                                next: t("commitDiff", "nextChange"),
                            },
                            kind::GIT_COMMIT_DIFF,
                            cx,
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_hidden()
                        .bg(ui::bg_base())
                        .child(body),
                );

            let (width, body_h) = modal_size(viewport);
            dialog.w(width).p_0().close_button(false).child(
                div()
                    .w_full()
                    .h(body_h)
                    .flex()
                    .overflow_hidden()
                    .child(left)
                    .child(right),
            )
        },
    );
}

/// 取一个文件在该 commit 里的 diff。
///
/// ⚠️ **重命名文件必须传 `oldPath`**(`CommitDiffModal.tsx:57`),否则父树里查不到
/// 旧内容,diff 会显示成「整文件新增」。
fn load_commit_file(
    state: &Entity<DiffState>,
    repo_path: &str,
    commit_hash: &str,
    files: &[CommitFileInfo],
    path: &str,
    cx: &mut App,
) {
    let old_path = files
        .iter()
        .find(|f| f.path == path)
        .and_then(|f| f.old_path.clone());
    let req = state.update(cx, |s, cx| {
        s.loading = true;
        s.error = None;
        s.result = None;
        s.flat = Flat::default();
        // 换文件 = 换一份 diff:跳转计数与两栏同步基准都要归零,否则新文件
        // 一进来就显示「3/12」,而两栏还停在上一份的偏移上
        s.jump = None;
        s.synced_y = px(0.0);
        s.request += 1;
        cx.notify();
        s.request
    });

    let (repo, hash, path) = (
        repo_path.to_string(),
        commit_hash.to_string(),
        path.to_string(),
    );
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                mt_project::git::get_commit_file_diff(
                    std::path::Path::new(&repo),
                    &hash,
                    &path,
                    old_path.as_deref(),
                )
            })
            .await;
        let _ = state.update(cx, |s: &mut DiffState, cx| {
            if s.request != req {
                return;
            }
            s.apply(result);
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, old: Option<u32>, new: Option<u32>) -> DiffLine {
        DiffLine {
            kind: kind.to_string(),
            content: format!("{kind}{old:?}{new:?}"),
            old_lineno: old,
            new_lineno: new,
        }
    }

    fn hunk(lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 0,
            lines,
        }
    }

    /// context 行左右同格。
    #[test]
    fn 配对_纯上下文() {
        let h = hunk(vec![
            line("context", Some(1), Some(1)),
            line("context", Some(2), Some(2)),
        ]);
        let (lines, pairs) = pair_rows(&[h]);
        assert_eq!(lines.len(), 2);
        assert_eq!(pairs, vec![(Some(0), Some(0)), (Some(1), Some(1))]);
    }

    /// 纯新增只出现在右栏。
    #[test]
    fn 配对_纯新增() {
        let h = hunk(vec![
            line("add", None, Some(1)),
            line("add", None, Some(2)),
        ]);
        let (_, pairs) = pair_rows(&[h]);
        assert_eq!(pairs, vec![(None, Some(0)), (None, Some(1))]);
    }

    /// delete 后紧跟 add:按下标配对,**长度不等时短的一侧留空**。
    #[test]
    fn 配对_删增不等长() {
        // 3 删 2 增 → 3 行,第三行右侧空
        let h = hunk(vec![
            line("delete", Some(1), None),
            line("delete", Some(2), None),
            line("delete", Some(3), None),
            line("add", None, Some(1)),
            line("add", None, Some(2)),
        ]);
        let (_, pairs) = pair_rows(&[h]);
        assert_eq!(
            pairs,
            vec![(Some(0), Some(3)), (Some(1), Some(4)), (Some(2), None)]
        );

        // 1 删 3 增 → 3 行,后两行左侧空
        let h = hunk(vec![
            line("delete", Some(1), None),
            line("add", None, Some(1)),
            line("add", None, Some(2)),
            line("add", None, Some(3)),
        ]);
        let (_, pairs) = pair_rows(&[h]);
        assert_eq!(
            pairs,
            vec![(Some(0), Some(1)), (None, Some(2)), (None, Some(3))]
        );
    }

    /// 配对**不跨 hunk**:上一 hunk 末尾的 delete 不会与下一 hunk 开头的 add 配对。
    #[test]
    fn 配对不跨_hunk() {
        let a = hunk(vec![line("delete", Some(1), None)]);
        let b = hunk(vec![line("add", None, Some(9))]);
        let (lines, pairs) = pair_rows(&[a, b]);
        assert_eq!(lines.len(), 2);
        assert_eq!(pairs, vec![(Some(0), None), (None, Some(1))]);
    }

    /// 认不出的 kind 直接跳过(原版的 else 分支),不产生行也不打乱下标。
    #[test]
    fn 配对_未知种类跳过() {
        let h = hunk(vec![
            line("weird", None, None),
            line("context", Some(1), Some(1)),
        ]);
        let (lines, pairs) = pair_rows(&[h]);
        assert_eq!(lines.len(), 2, "拍平的行仍含未知种类,只是不进配对");
        assert_eq!(pairs, vec![(Some(1), Some(1))]);
    }

    fn line_with(kind: &str, content: &str) -> DiffLine {
        DiffLine {
            kind: kind.to_string(),
            content: content.to_string(),
            old_lineno: Some(1),
            new_lineno: Some(1),
        }
    }

    /// 只改了一个字符,就只点出那一个字符 —— 这是词级高亮存在的全部理由。
    #[test]
    fn 词级高亮_只点出改了的片段() {
        let (del, add) = intra_line_marks("let x = 1;", "let x = 2;").expect("应当细分");
        assert_eq!(del, vec![8..9]);
        assert_eq!(add, vec![8..9]);
        assert_eq!(&"let x = 1;"[del[0].clone()], "1");
        assert_eq!(&"let x = 2;"[add[0].clone()], "2");
    }

    /// 相邻的变动词元并成一段,不留缝。
    #[test]
    fn 词级高亮_相邻片段合并() {
        let (del, _) = intra_line_marks("foo(bar)", "foo(baz, qux)").expect("应当细分");
        assert_eq!(del.len(), 1, "`bar` 是一整段,不该拆成几块");
    }

    /// 整行都变了就不细分 —— 全高亮 = 没高亮,还不如整行一个底色干净。
    #[test]
    fn 词级高亮_改动过多时放弃() {
        assert!(intra_line_marks("aaaa bbbb", "cccc dddd").is_none());
        assert!(intra_line_marks("same", "same").is_none(), "两行相同");
        assert!(intra_line_marks("", "x").is_none(), "空行");
        let long = "x".repeat(INTRA_LINE_MAX_BYTES + 1);
        assert!(intra_line_marks(&long, "x").is_none(), "超长行");
    }

    /// CJK 按 `Other` 逐字切:改一个字不该把整句涂了。
    #[test]
    fn 词级高亮_中文逐字() {
        let (del, add) = intra_line_marks("这是一行中文", "这是两行中文").expect("应当细分");
        assert_eq!(del.len(), 1);
        assert_eq!(add.len(), 1);
        assert_eq!(&"这是一行中文"[del[0].clone()], "一");
        assert_eq!(&"这是两行中文"[add[0].clone()], "两");
    }

    /// 每个 hunk 前插一行 `@@` 头,跳转落点就是这些头的下标。
    #[test]
    fn 拍平_每个_hunk_一个头() {
        let a = hunk(vec![
            line_with("context", "a"),
            line_with("delete", "b"),
            line_with("add", "c"),
        ]);
        let b = hunk(vec![line_with("context", "d")]);
        let f = flatten(&[a, b]);

        assert_eq!(f.heads.len(), 2);
        // 头 + 3 行,头 + 1 行
        assert_eq!(f.inline_rows.len(), 6);
        assert_eq!(f.inline_jumps, vec![0, 4]);
        // 并排视图里 delete/add 配成一行 ⇒ 头 + 2 行,头 + 1 行
        assert_eq!(f.pair_rows.len(), 5);
        assert_eq!(f.pair_jumps, vec![0, 3]);
        assert!(matches!(f.inline_rows[0], InlineRow::Head(0)));
        assert!(matches!(f.pair_rows[3], PairRow::Head(1)));
        assert!(f.heads[0].inline.contains("@@"));
    }

    /// 配对成功的删/增行才做词级高亮,单侧的行不做。
    #[test]
    fn 拍平_只给配对行算词级高亮() {
        let h = hunk(vec![
            line_with("delete", "value = 1"),
            line_with("add", "value = 2"),
            line_with("add", "brand new"),
        ]);
        let f = flatten(&[h]);
        assert!(!f.marks[0].is_empty(), "配对的 delete 行应有高亮");
        assert!(!f.marks[1].is_empty(), "配对的 add 行应有高亮");
        assert!(f.marks[2].is_empty(), "没配上的 add 行整行涂色即可");
    }

    /// uniform_list 只量一行来定内容宽,量宽下标必须指向**最宽**那一行。
    #[test]
    fn 拍平_量宽指向最宽的行() {
        let h = hunk(vec![
            line_with("context", "short"),
            line_with("context", "a much much longer line"),
        ]);
        let f = flatten(&[h]);
        // 行 0 是 hunk 头,行 1/2 是两条 context —— 最宽的是第 2 行
        assert_eq!(f.widest_inline, 2);
        assert!(matches!(f.inline_rows[2], InlineRow::Line(1)));
        assert_eq!(f.widest_left, 2);
        assert_eq!(f.widest_right, 2);
    }

    /// `\t` 在 gpui 文本里零宽,拍平时按制表位展开;词级高亮在展开后的文本上算。
    #[test]
    fn 拍平_tab缩进按制表位展开() {
        let h = hunk(vec![
            line_with("delete", "\tvalue := 1"),
            line_with("add", "\tvalue := 2"),
            line_with("context", "a\tb"),
        ]);
        let f = flatten(&[h]);
        assert_eq!(f.lines[0].content, "    value := 1");
        assert_eq!(f.lines[1].content, "    value := 2");
        assert_eq!(f.lines[2].content, "a   b");
        // 高亮区间落在展开后的 `1` / `2` 上(下标 13),而不是原文里的 10
        assert_eq!(f.marks[0], vec![13..14]);
        assert_eq!(f.marks[1], vec![13..14]);
    }

    #[test]
    fn 行宽_全角按两列() {
        assert_eq!(text_cols("abc"), 3);
        assert_eq!(text_cols("中文"), 4);
        assert_eq!(text_cols(""), 0);
    }

    /// commit 文件的状态字母表:四种已知 + 回落 `?`。
    #[test]
    fn commit_文件状态字母() {
        ui::set_palette(ui::Palette::dark());
        assert_eq!(commit_file_badge("added").0, "A");
        assert_eq!(commit_file_badge("modified").0, "M");
        assert_eq!(commit_file_badge("deleted").0, "D");
        assert_eq!(commit_file_badge("renamed").0, "R");
        assert_eq!(commit_file_badge("conflicted").0, "?");
        assert_eq!(commit_file_badge("").1, ui::text_muted());
    }
}
