//! diff 的内容与渲染。原版是两个弹窗(`src/components/DiffModal.tsx` /
//! `CommitDiffModal.tsx`),GPUI 版改成**工作区页签区里的 diff 页签**(宿主、身份与复用、
//! 焦点、防陈旧见 [`tab`] 模块注释,三种正文的外框见 [`pages`]):
//!
//! - [`open_file_diff`] —— 工作区 / 暂存区的**单文件** diff
//! - [`open_commit_diff_loading`] —— 某次 commit 的**多文件** diff;历史区多选时同一种页签
//!   也装「一组提交的合并变更」([`open_combined_diff_loading`],左栏是合并后的文件,右栏是
//!   两个版本之间的 diff)
//! - [`open_text_diff`] —— 文件页「磁盘上的版本 ↔ 未保存的修改」
//! - 单文件历史(`git_file_history`)的右栏同样是这里的正文
//!
//! 两个视图(`InlineView` / `SideBySideView`)在原版是 `DiffModal.tsx` 导出、
//! 由 `CommitDiffModal` 复用的;这里同样只有一份([`render_inline`] /
//! [`split::render_side_by_side`],并排视图的行模型、滚动映射见 [`split`],中间栏见 [`ribbon`])。
//! 贮藏弹窗与单文件历史嵌的「某个提交里的一个文件」见 [`CommitFileDiffPane`]。
//!
//! # 与原版的偏差(全部是有意的)
//!
//! 1. **行渲染走 [`gpui::uniform_list`](gpui::uniform_list) 虚拟化**。原版 `rows.map` 全量建 DOM ——
//!    1MB 上限(`MAX_DIFF_BYTES`)挡住了最坏情况,但一个 900KB 的文本文件仍能出
//!    ~20k 行,gpui 全量建元素会明显卡。行高恒定 = `round(fontSize*1.6)`,
//!    天然适配 uniform_list。这是**改进**而非偏差(规格 §6.6 明写)。
//! 2. **`staged` 依赖漏项顺修**。原版 effect 的依赖数组是 `[open, projectPath, status.path]`
//!    (`DiffModal.tsx:179`),**漏了 `staged`** —— 同一路径先点 staged 行再点 unstaged 行
//!    不会重新拉 diff。这里页签的身份就带着侧别(见 [`tab`]),同一文件的已暂存 / 未暂存
//!    是两个页签,各按 `(repo, path, side)` 取数,那个 bug 自然不存在(规格 §11 第 17 条
//!    建议顺修并注明)。
//! 3. **长行可横向滚动**。原版内容区是 `overflow-auto` + `whitespace-pre`,长行拖着看;
//!    `uniform_list` 默认只竖滚、且文字会回绕后被固定行高裁掉 —— 靠
//!    [`ListHorizontalSizingBehavior::Unconstrained`] + 「按最宽那一行量宽」
//!    ([`Flat::widest_inline`])补回来。⚠️ uniform_list **只量一行**来定内容宽
//!    (`uniform_list.rs:336`),量错行横向滚动范围就不够;字号也得挂在列表**外面**
//!    (它量宽时还没套上自己的样式)。横向滚动时行号列不动、只有正文滚:行内视图的行号列
//!    与 hunk 头(连同块操作按钮)钉在左缘([`pinned`]),并排视图的行号本来就在两栏之间的
//!    中间栏里;有行超出视野时底部常显一条横向滚动条,截在视野外的部分一眼看得出、也能直接拖。
//! 4. **行内视图 hunk 之间有 `@@ -a,b +c,d @@` 头,工具栏有「上一处/下一处改动」**。
//!    原版两样都没有,几处改动连在一起看不出断点、几千行的文件里也只能自己拖。
//!    头里纯新增 / 纯删除那一侧的起点按 git 的写法(插入点之前那一行,见 [`head_start`])。
//!    并排视图没有头行:改动之间的断点由中间栏的带子与配色标出(第 14 条)。
//! 5. **并排视图两栏共用一个滚动位置**(对标 IDEA / Winc,见 [`split`]):两栏各自连续排行、
//!    没有占位空行,滚任一栏两栏一起动 —— 改动以外逐行对应,改动里两侧行数不同时按比例走,
//!    同一处改动在两栏里一起进入、一起离开视口;纵向只有一条共用的滚动条。横向两栏同步、
//!    各按自己的范围钳住(只有一栏有长行时另一栏停在自己的尽头)。原版两栏各滚各的,
//!    对照着看要来回对齐。F7 跳到某块时两栏都把它滚进视野。
//! 6. **改动块做词级高亮**([`word_diff`]):一块连续的删 / 增行两侧各拼成一段(换行也
//!    参与比较)做一次词级 diff,再按行切回 —— 一行拆两行、中间插一行时不会因逐行配对
//!    错位而整行涂色;块太大时退回逐对配对([`intra_line_marks_within`])。原版一行里改
//!    一个字符也是整行涂红/绿。
//! 7. **正文用等宽字体**(与文件查看器同一条 mono 链)。原版是 `font-mono`,
//!    GPUI 侧字族不自己挂就会继承界面字体,列对不齐。
//! 8. **不是弹窗,是工作区页签**(见 [`tab`]):正文铺满页签区,能与 Git 抽屉并排看
//!    (停在 diff 页签上时抽屉挤窄页签区而不是盖在上面)。原版是 `90vw×80vh` /
//!    `92vw×85vh` 两个弹窗;[`modal_size`] 留给仍是弹窗的贮藏 / 逐段解决冲突。
//! 9. **单文件 diff 能按块暂存 / 取消暂存 / 丢弃**(行内视图在 hunk 头上,并排视图在中间栏
//!    每处改动上,见 [`HunkUi`] / [`ribbon`]):
//!    后端 `mt_project::git::{stage_hunk, unstage_hunk, discard_hunk}` 拿 diff 两侧全文
//!    与该块的精确行号做文本拼接,执行前重读两侧防陈旧。操作后页签**原地**重取 diff、
//!    视图模式不变、滚到原位置上的下一块(并排视图按一处改动操作、这块还留着别的改动时不跳,
//!    停在原地);同时回调 `on_changed` 让更改列表重载。
//!    从更改列表打开的走 `reload_files`:列表重载落地后文件序列跟着换,当前文件离开了
//!    分组(取消暂存 / 丢弃了最后一块)就换到原位置上的下一个(见 [`resync_index`])。
//! 10. **能在打开它的那个分组里前后换文件**(Alt+← / Alt+→ 或标题栏按钮,在当前页签里换,
//!     页签的标题与身份跟着变),F7 / Shift+F7 跳下一处 / 上一处改动。原版两样都没有。
//!     提交 diff 左栏 ↑ / ↓ 换文件(按左栏看得见的行序走,见 [`step_file`])。
//! 11. **正文能拖选复制**(对标 IDEA):拖选 / 双击选词 / 三击选行 / Shift+单击延伸,
//!     拖出视口自动滚动;Ctrl/Cmd+C 复制、Ctrl/Cmd+A 全选、右键「复制 / 全选」。并排视图
//!     只选起拖的那一栏,行内视图不带行号与 `+`/`-`,tab 按原文复制。原版的行是 DOM,
//!     浏览器白给选区;这里行是虚拟化的自绘元素,整套在 [`selection`] 里。
//! 12. **单文件 diff 能按行暂存 / 取消暂存 / 丢弃**(对标 IDEA 的行勾选,见 [`picks`]):
//!     允许块操作的文件,增删行的行号旁多一个勾选框 —— 行内视图在行号栏最左,并排视图
//!     删除行勾在左栏行号、新增行勾在右栏行号,都贴着行号靠正文一侧。一下勾上 / 取消一批:
//!     行内视图是 hunk 头上的勾选框(整块),并排视图是中间栏每处改动上的三态勾选框(这一处的
//!     增删行,见 [`ribbon`])。勾了行之后块头按钮(行内)/ 中间栏的箭头(并排)只对勾上的行
//!     生效、文案写明行数;工具栏另有跨块的一组,一次交给多块后端
//!     (`mt_project::git::{stage_hunks, unstage_hunks, discard_hunks}`),不逐块落盘。
//!     原地重取后勾选按内容认回来(见 [`picks`] 模块注释);Esc / 「清除勾选」清空,换文件清空。
//! 13. **折叠的上下文能展开**(对标 GitHub,见 [`fold`]):块与块之间、文件头尾没改动的行
//!     折成一条分隔行,写明折了多少行;点它整段展开,行上另有「向下 / 向上展开 20 行」。
//!     展开纯前端做(diff 结果带两侧全文),五个用到 diff 正文的地方都能展开;展开出来的行
//!     只是上下文 —— 没有勾选框、不属于任何块、F7 不停在上面,能拖选复制。并排视图里两栏的
//!     分隔行由中间栏一条淡色带子相连(两栏的分隔行不一定在同一高度),点行号栏那一段也展开。
//! 14. **并排视图的中间栏与按改动种类上色**(对标 Winc,见 [`ribbon`]):左栏正文 | 左栏行号 |
//!     连接带 | 右栏行号 | 右栏正文。每处改动(块里连续的增删行)一条带子,两端分别是两栏
//!     那几行的上下沿,中间按 S 形曲线过渡;一侧没有行时该侧画一道插入线。配色按改动种类:
//!     两侧都有行的修改是蓝、只有右侧的新增是绿、只有左侧的删除是灰,改了的词同色系深一档。
//!     块操作按钮也挪进了连接带:每处改动一组「»」(左 → 右)/「«」(右 → 左)+ 三态勾选框,
//!     作用于**这一处改动**而不是整个 hunk(没有头行之后看不出 hunk 的边界,一个 hunk 里的
//!     几处改动各有各的带子与按钮,对标 IDEA)。行内视图不变:`@@` 头、头上的按钮、红绿配色。
//!
//! # 判定顺序不能换
//!
//! `loading → error → isBinary → tooLarge → 正常`(`DiffModal.tsx:233-259`)。
//! 二进制文件的 `hunks` 是空的,先判 `tooLarge` 会把它显示成「文件过大」。

mod fold;
mod pages;
mod picks;
mod ribbon;
mod selection;
mod split;
mod tab;
mod word_diff;

use std::borrow::Cow;
use std::collections::{BTreeSet, HashSet};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, ElementId, Entity,
    InteractiveElement, IntoElement, ListHorizontalSizingBehavior, ParentElement, Pixels, Point,
    Render, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, div, point, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use mt_project::encoding::TextEncoding;
pub use mt_project::git::DiffSide;
use mt_project::git::{
    CombinedFileChange, CommitFileInfo, DiffHunk, FileRevision, GitDiffResult, HunkPick, StaleDiff,
};
use mt_ui::icons::FileIcon;
use mt_ui::tooltip::{Tooltip, TooltipExt as _};

use crate::git_ui::{
    self, ButtonKind, ChangeTreeRow, ICON_ARROW_DOWN, ICON_ARROW_UP, ICON_CHEVRON_LEFT,
    ICON_CHEVRON_RIGHT, ICON_CLEAN, ICON_CLOSE, ICON_COPY, ICON_MINUS, ICON_REFRESH, action_button,
    icon_button, mono, pill, text_button,
};
use crate::i18n::{t, tr};
use crate::prompt::Confirm;
use crate::store::AppStore;
use crate::tab_expansion::{TAB_WIDTH, expand_line};
use crate::ui;
use selection::{Pane, RowText, SelectState};
use split::{LEFT, RIGHT, SideRow, Split};
pub(crate) use tab::open_file_history;
pub use tab::{
    DiffKey, DiffTab, DiffTabEvent, TextDiffSpec, notify_repo_changed, open_combined_diff,
    open_combined_diff_loading, open_commit_diff_loading, open_file_diff, open_file_diff_with,
    open_text_diff,
};

gpui::actions!(
    mini_term,
    [
        /// 单文件 diff:下一处改动(F7)
        GitDiffNextChange,
        /// 单文件 diff:上一处改动(Shift+F7)
        GitDiffPrevChange,
        /// 单文件 diff:分组里的下一个文件(Alt+→)
        GitDiffNextFile,
        /// 单文件 diff:分组里的上一个文件(Alt+←)
        GitDiffPrevFile,
        /// 提交 diff:左栏的下一个文件(↓)
        GitCommitDiffNextFile,
        /// 提交 diff:左栏的上一个文件(↑)
        GitCommitDiffPrevFile,
        /// diff 页签(与贮藏弹窗里嵌的 diff):复制正文选区(Ctrl/Cmd+C)
        GitDiffCopy,
        /// diff 页签(与贮藏弹窗里嵌的 diff):全选当前栏(Ctrl/Cmd+A)
        GitDiffSelectAll,
        /// 单文件 diff:清空行勾选(Esc;没有勾选时放行 —— 页签里没有弹窗可关,按键照常往外走)
        GitDiffClearPicks,
    ]
);

/// 单文件 diff 页签(与磁盘对比页签)的 key context(键位绑在 `hotkeys.rs` 的表外段)。
pub const FILE_DIFF_CONTEXT: &str = "GitFileDiff";

/// 提交 diff / 合并变更页签的 key context(左栏换文件的 ↑ / ↓、正文的 Ctrl/Cmd+C / A
/// 挂在它上面)。
pub const COMMIT_DIFF_CONTEXT: &str = "GitCommitDiff";

/// 视图模式。**组件态,不落盘**;默认 side-by-side(`DiffModal.tsx:158`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    SideBySide,
    Inline,
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

/// 一对删 / 增行各自「变了的字节区间」。
type PairMarks = (Vec<Range<usize>>, Vec<Range<usize>>);

/// 不限预算的 [`intra_line_marks_within`](单测用)。
#[cfg(test)]
fn intra_line_marks(old: &str, new: &str) -> Option<PairMarks> {
    let mut unlimited = usize::MAX;
    intra_line_marks_within(old, new, &mut unlimited)
}

/// 一对 delete/add 行的词级差异 → 两侧各自「变了的字节区间」。按块比放弃时的退路
/// (见 [`word_diff`]),DP 格子数从这份 diff 的预算 `budget` 里扣。
///
/// `None` = 不细分(两行相同 / 太长 / 改动占比过高 / 预算用完)。
fn intra_line_marks_within(old: &str, new: &str, budget: &mut usize) -> Option<PairMarks> {
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
enum InlineRow {
    /// 第 n 个 hunk 的 `@@` 头
    Head(usize),
    /// [`Flat::lines`] 的下标
    Line(usize),
    /// 折着的上下文([`Flat::folds`] 的下标)
    Fold(usize),
}

/// hunk 头(只有行内视图有,并排视图没有头行)。
struct HunkHead {
    inline: SharedString,
}

fn hunk_head(h: &DiffHunk) -> HunkHead {
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
fn head_start(start: u32, lines: u32) -> u32 {
    if lines == 0 {
        start.saturating_sub(1)
    } else {
        start
    }
}

/// 拍平后的一行(两个视图共用)。
struct FlatLine {
    kind: String,
    /// **tab 展开后**的显示文本。存成 `SharedString`:行回调每帧给每个可见行建文字元素,
    /// clone 只加引用计数 —— 压缩过的单行 JS / JSON 一行就几百 KB,每帧整行复制一份太亏。
    content: SharedString,
    old_lineno: Option<u32>,
    new_lineno: Option<u32>,
}

/// 一次 diff 结果算好的**全部派生数据**。结果一到就算一次,不在每帧的
/// uniform_list 回调里重算;展开折叠只重排行模型([`Flat::relayout`]),不重算词级高亮。
#[derive(Default)]
struct Flat {
    /// 拍平的行(两个视图共用一份)。`content` 是 **tab 展开后**的显示文本。前
    /// [`Flat::hunk_lines`] 行出自块,其后是展开出来的上下文行(每次重排重建)
    lines: Vec<FlatLine>,
    /// 与 [`Flat::lines`] 等长:含 tab 的行展开前的原文(复制要给它),没有 tab 的是 `None`
    raw: Vec<Option<String>>,
    /// 与 [`Flat::lines`] 等长:每行的行内改动片段。空 = 整行一个底色
    marks: Vec<Vec<Range<usize>>>,
    /// [`Flat::lines`] 里前多少行出自块。勾选与块操作只认这些行,展开出来的上下文行在它之后
    hunk_lines: usize,
    /// 第 k 块的第一行在 [`Flat::lines`] 里的下标;末尾多一个 = [`Flat::hunk_lines`]。
    /// 块里第 i 行就是 `DiffHunk::lines[i]`(按行操作的行号就按它换算)
    hunk_base: Vec<usize>,
    heads: Vec<HunkHead>,
    /// 折掉的上下文,共 `heads.len() + 1` 段(见 [`fold`])。拿着两侧全文拍平时才有
    gaps: Vec<fold::Gap>,
    /// 眼下仍折着的段,视图里各占一条分隔行
    folds: Vec<fold::Fold>,
    inline_rows: Vec<InlineRow>,
    /// 行内视图「上一处/下一处改动」的落点 = 每个 hunk 头所在的行下标(并排视图的落点见
    /// [`Split::hunk_pos`])
    inline_jumps: Vec<usize>,
    /// 行内视图给 uniform_list 量宽用的「最宽的那一行」(并排两栏的在 [`Split::widest`])
    widest_inline: usize,
    /// 并排视图的两栏行序、各处改动与滚动映射(见 [`split`])
    split: Split,
    /// 标题栏「+12 −3」的两个数
    adds: usize,
    deletes: usize,
}

/// 并排视图一处改动的身份:所在的块、两侧第一行的行号(删除行在旧侧、新增行在新侧;该侧
/// 没有行时 `None`)、两侧行数。
///
/// 按钮回调里记的改动序号是渲染时的:静默重取刚落地、还没重画的那一两帧里点下去,同一个
/// 序号可能已经指着别处。回调连同身份一起记下,点下去时与眼下的行模型核对(见
/// [`DiffState::change_is`]),对不上就作罢。行内视图的块头按钮同理,身份是块头
/// (见 [`Flat::hunk_key`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChangeKey {
    hunk: usize,
    first: [Option<u32>; 2],
    len: [usize; 2],
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

/// 一行原文 → 显示用的 [`FlatLine`] + 复制用的原文。
///
/// `\t` 在 gpui 文本里是零宽(见 `tab_expansion` 模块注释),显示前按制表位展开。
/// 词级高亮与量宽都在展开后的文本上算,字节区间才对得上;原文另存一份给复制用。
/// 显示文本在这里一次转成 `SharedString`(见 [`FlatLine::content`])
fn flat_line(
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
fn flatten_hunks(hunks: &[DiffHunk]) -> Flat {
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
fn flatten(hunks: &[DiffHunk]) -> Flat {
    let mut f = flatten_hunks(hunks);
    f.relayout("", &fold::Revealed::default());
    f
}

/// 一份 diff 结果 → [`Flat`],块与块之间没改动的行按 `revealed` 展开 / 折叠(见 [`fold`])。
/// 一并返回重新认过的展开记忆(只留这一次真认出来的行,见 [`Flat::relayout`])。
fn flatten_diff(diff: &GitDiffResult, revealed: &fold::Revealed) -> (Flat, fold::Revealed) {
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
    fn relayout(&mut self, old_text: &str, revealed: &fold::Revealed) -> fold::Revealed {
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
    fn layout_hunk_split(&mut self, g: usize) {
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
    fn layout_gap(
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
    fn expand_fold(
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
    fn push_context(&mut self, old_no: u32, new_no: u32, raw: &str) {
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
    fn measure(&mut self) {
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
    fn pick_lines(&self) -> Vec<(&str, &str)> {
        (0..self.hunk_lines)
            .map(|ix| (self.lines[ix].kind.as_str(), self.raw_line(ix)))
            .collect()
    }

    /// 第 `hunk` 块里能打勾的行(行内视图块头的勾选框管这些)。
    fn pickable_in(&self, hunk: usize) -> Vec<usize> {
        let (Some(&from), Some(&to)) = (self.hunk_base.get(hunk), self.hunk_base.get(hunk + 1))
        else {
            return Vec::new();
        };
        self.pickable_of(from..to)
    }

    /// 并排视图第 `change` 处改动里能打勾的行(中间栏那颗勾选框管这些)。
    fn change_lines(&self, change: usize) -> Vec<usize> {
        self.split
            .changes
            .get(change)
            .map_or_else(Vec::new, |c| self.pickable_of(c.lines.clone()))
    }

    /// 并排视图第 `change` 处改动的身份(见 [`ChangeKey`])。
    fn change_key(&self, change: usize) -> Option<ChangeKey> {
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
    fn hunk_key(&self, hunk: usize) -> Option<SharedString> {
        self.heads.get(hunk).map(|head| head.inline.clone())
    }

    /// `lines`(拍平行区间)里的增删行。
    fn pickable_of(&self, lines: Range<usize>) -> Vec<usize> {
        lines
            .filter(|&ix| picks::pickable_kind(&self.lines[ix].kind).is_some())
            .collect()
    }

    /// 一次块操作真正落下去的行([`Flat::lines`] 下标):整块 = 块里全部增删行,按行 = 勾上的。
    fn applied_lines(&self, picks: &[HunkPick]) -> BTreeSet<usize> {
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

fn widest<T>(rows: &[T], cols: impl Fn(&T) -> usize) -> usize {
    rows.iter()
        .enumerate()
        .max_by_key(|(_, row)| cols(row))
        .map_or(0, |(ix, _)| ix)
}

/// 一个 hunk 的词级标记(`lines` 是它在 [`Flat::lines`] 里的区间):连续的增删行算一块,
/// 整块交给 [`word_diff::block_marks`];块太大 / 编辑太多时退回逐对配对(块里第 i 条删除行
/// 配第 i 条新增行,还是老算法)。纯新增 / 纯删除的块整行一个底色。
fn mark_blocks(f: &mut Flat, lines: Range<usize>, budget: &mut usize) {
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

/// 并排视图一栏行号列的宽(`DiffModal.tsx:38,116` 的 `w-[48px]`),在中间栏里(见 [`ribbon`])。
const GUTTER: f32 = 48.0;

/// inline 视图的两列行号(旧 / 新)各自的宽度与 `+`/`-` 符号列宽。
/// 两列合起来与 hunk 头的留白一致,头里的 `@@` 才与正文左缘对齐。
const INLINE_NUM: f32 = 44.0;
const INLINE_SIGN: f32 = 16.0;
const INLINE_GUTTER: f32 = INLINE_NUM * 2.0 + INLINE_SIGN;

/// 勾选框那一列的宽(只在允许按行操作时出现):行内视图在行号栏最左,并排视图贴着中间栏里
/// 的行号、靠正文一侧。
const CHECK_W: f32 = 24.0;

/// 行内高亮的底色(与整行底色叠在一起,所以要更实)。
fn word_bg(kind: &str) -> gpui::Hsla {
    if kind == "add" {
        ui::diff_add_word_bg()
    } else {
        ui::diff_del_word_bg()
    }
}

/// 横向滚动时**钉在左缘**的一层(行号列、hunk 头连同块操作按钮)。
///
/// 横向滚动是整份内容一起平移的(uniform_list 只有一个滚动偏移),要让某一块不动,
/// 就把它绝对定位、再往右挪回「已经滚出去的那一段」:`scroll_x` 是列表当前的横向偏移
/// (≤ 0),`left = -scroll_x`。偏移是每帧画行时现读的(滚轮改了偏移就重画),不落后一帧。
///
/// 绝对定位的元素不计入行的内容宽,uniform_list 量宽时只量正文 —— 横向可滚范围不变。
fn pinned(scroll_x: Pixels) -> gpui::Div {
    div().absolute().top_0().left(-scroll_x).h_full()
}

/// 一行:若干行号列 + 内容列。`gutters` 是各行号列的 `(文本, 宽)`(并排视图一列,
/// inline 视图旧 / 新两列 + 符号列),`marks` 是这一行里**真正变了的片段**
/// (空 = 整行一个底色,与原版一致)。`check` 是行号栏最左那一列勾选框(宽 [`CHECK_W`],
/// 只在允许按行操作时给;不能勾的行给空格子占住列宽)。
///
/// 行号列钉在左缘([`pinned`]),正文从它底下横向滚过:行号列先铺一层不透明的正文底、
/// 再叠这一行的增删底色,滚过去的字不会透出来;正文左侧留出行号列的宽。勾选框是行号列的
/// 一部分,随它一起钉住;按在上面不起拖选(行号列本来就不可选,见 [`selection`])。
///
/// 行根 `w_full()`:横向不受限的 uniform_list 里行宽 = 内容区宽,少了它增删行的底色
/// 只涂到文字为止,长短行参差不齐。
///
/// 正文可拖选(见 [`selection`]):`row` 带着这一行的选区与排版登记;正文区是 I 形光标,
/// 行号列照旧是箭头。行高同时设成文字行高 —— 高亮底色按文字行高画,两者不一致时
/// 跨行选区的底色会一条压一条、叠出深浅相间的横纹。
fn diff_line_row(
    line: &FlatLine,
    gutters: &[(String, f32)],
    check: Option<AnyElement>,
    line_height: f32,
    marks: &[Range<usize>],
    scroll: Point<Pixels>,
    row: RowText,
) -> AnyElement {
    let kind = line.kind.as_str();
    // 只加引用计数,不复制整行(见 [`FlatLine::content`])
    let content = line.content.clone();
    let check_w = if check.is_some() { CHECK_W } else { 0.0 };
    let gutter_w: f32 = check_w + gutters.iter().map(|(_, width)| *width).sum::<f32>();
    // `whitespace-pre`:diff 行不换行 —— 少了它 gpui 会把长行回绕,而行高是恒定的
    // (uniform_list 的前提),回绕出来的第二行直接被裁掉,看着像「缺了半行」。
    // `flex_none`:宽度就是整行文字的宽,uniform_list 按它量横向可滚范围
    let text = div()
        .flex_none()
        .flex()
        .pl(px(gutter_w + 8.0))
        .pr(px(8.0))
        .whitespace_nowrap()
        .text_color(line_fg(kind))
        .child(selection::line_text(
            content,
            marks,
            word_bg(kind),
            &row,
            scroll,
        ));
    let numbers = pinned(scroll.x).w(px(gutter_w)).bg(ui::bg_base()).child(
        div()
            .size_full()
            .flex()
            // 行号列不可选:光标照旧是箭头
            .cursor_default()
            .when_some(line_bg(kind), |el, bg| el.bg(bg))
            .children(check)
            // 行号直接用 muted,不再叠 0.6 的透明度:叠上之后深浅两套主题里都只剩
            // 2:1 上下的对比,行号基本读不出来
            .children(gutters.iter().map(|(no, width)| {
                div()
                    .w(px(*width))
                    .flex_none()
                    .pr(px(8.0))
                    .text_right()
                    .text_color(ui::text_muted())
                    .child(no.clone())
            })),
    );

    div()
        .relative()
        .w_full()
        .flex()
        .h(px(line_height))
        .line_height(px(line_height))
        .cursor_text()
        .when_some(line_bg(kind), |el, bg| el.bg(bg))
        .child(text)
        .child(numbers)
        .into_any_element()
}

/// hunk 头(`@@ -a,b +c,d @@`),后面可以跟一组块操作按钮。
///
/// 整块钉在左缘([`pinned`]):横向滚到长行的尽头时,`@@` 与「暂存此块 / 丢弃此块」
/// 仍在视野左侧,不用先滚回去才点得到;底色铺满整行,分隔照旧。
///
/// ⚠️ 高度必须**恰好**是 `line_height`:uniform_list 按「量到的那一行」的高度
/// 摆所有行,这里多一像素边框,整份 diff 的行位就会与滚动条对不上。分隔靠底色;
/// 按钮比行高矮 4px、居中摆。
///
/// `gutter` 是整个行号栏的宽(含勾选框列);`check` 是块头的勾选框(一下勾上 / 取消这一块,
/// 宽 [`CHECK_W`]),占在行号栏最左。
fn hunk_row(
    text: SharedString,
    line_height: f32,
    gutter: f32,
    check: Option<AnyElement>,
    actions: Option<AnyElement>,
    scroll_x: Pixels,
) -> AnyElement {
    let check_w = if check.is_some() { CHECK_W } else { 0.0 };
    let head = pinned(scroll_x)
        .flex()
        .items_center()
        .children(check)
        .child(div().w(px(gutter - check_w)).flex_none())
        .child(
            div()
                .flex_none()
                .px(px(8.0))
                .whitespace_nowrap()
                .text_color(ui::text_muted())
                .child(text),
        )
        .children(actions.map(|actions| {
            div()
                .flex_none()
                .h(px((line_height - 4.0).max(16.0)))
                .ml(px(8.0))
                .flex()
                .items_stretch()
                .gap(px(8.0))
                .child(actions)
        }));
    div()
        .relative()
        .w_full()
        .h(px(line_height))
        .bg(ui::bg_elevated())
        .child(head)
        .into_any_element()
}

/// 折着的上下文:一条可点的分隔行(点它 = 整段展开),写明折了多少行;`buttons` 时后面再跟
/// 「向下展开 20 行」「向上展开 20 行」「全部展开」(哪几颗见 [`fold::step_buttons`])。
/// 并排视图只有右栏带按钮(同一段别出两组),两栏的分隔行(连同中间栏里那一段)都能点;
/// 并排视图的行号在中间栏,这里 `gutter` 给 0。
///
/// 与 [`hunk_row`] 同一套:整块钉在左缘、高度**恰好**一行。分隔行按 `(gap, start)` 认是
/// 哪一段(见 [`DiffState::expand`]),点下去时行模型换过了也不会展开错。
fn fold_row(
    state: &Entity<DiffState>,
    fold: &fold::Fold,
    pane: Pane,
    line_height: f32,
    gutter: f32,
    buttons: bool,
    scroll_x: Pixels,
) -> AnyElement {
    let (gap, start) = (fold.gap, fold.start);
    let expand = |how: fold::Expand| {
        let state = state.clone();
        move |cx: &mut App| {
            state.update(cx, |s, cx| {
                s.expand(gap, start, how);
                cx.notify();
            })
        }
    };
    let button = |key: &str, label: SharedString, how: fold::Expand| {
        let run = expand(how);
        text_button(
            SharedString::from(format!("git-diff-fold-{key}-{gap}-{start}")),
            label,
            true,
            false,
        )
        .on_click(move |_: &ClickEvent, _window, cx| {
            // 别让分隔行自己的点击(整段展开)也跟着触发
            cx.stop_propagation();
            run(cx);
        })
    };
    let actions = buttons.then(|| {
        let (down, up) = fold::step_buttons(fold);
        let step = fold::EXPAND_STEP.to_string();
        div()
            .flex_none()
            .h(px((line_height - 4.0).max(16.0)))
            .ml(px(8.0))
            .flex()
            .items_stretch()
            .gap(px(8.0))
            .when(down, |el| {
                el.child(button(
                    "down",
                    tr!("gitCommit", "expandDown", count = step.clone()).into(),
                    fold::Expand::Down,
                ))
            })
            .when(up, |el| {
                el.child(button(
                    "up",
                    tr!("gitCommit", "expandUp", count = step.clone()).into(),
                    fold::Expand::Up,
                ))
            })
            .child(button(
                "all",
                t("gitCommit", "expandAll").into(),
                fold::Expand::All,
            ))
    });
    let head = pinned(scroll_x)
        .flex()
        .items_center()
        .child(div().w(px(gutter)).flex_none())
        .child(
            div()
                .flex_none()
                .px(px(8.0))
                .whitespace_nowrap()
                .text_color(ui::text_secondary())
                .child(tr!(
                    "gitCommit",
                    "foldedLines",
                    count = fold.len.to_string()
                )),
        )
        .children(actions);
    let all = expand(fold::Expand::All);
    let pane_key = match pane {
        Pane::Inline => "inline",
        Pane::Old => "old",
        Pane::New => "new",
    };
    div()
        .id(SharedString::from(format!(
            "git-diff-fold-{pane_key}-{gap}-{start}"
        )))
        .relative()
        .w_full()
        .h(px(line_height))
        .cursor_pointer()
        .bg(fold_bg())
        .hover(|el| el.bg(fold_hover_bg()))
        .on_click(move |_: &ClickEvent, _window, cx| all(cx))
        .child(head)
        .into_any_element()
}

/// 折叠分隔行的淡色底(并排视图里连接两栏分隔行的带子、中间栏行号栏那一段同色)。
fn fold_bg() -> gpui::Hsla {
    ui::with_alpha(ui::accent(), 0.07)
}

/// 悬停在分隔行上时的底色。
fn fold_hover_bg() -> gpui::Hsla {
    ui::with_alpha(ui::accent(), 0.14)
}

/// 勾选框的三种样子(块头的勾选框会有「勾了一部分」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tick {
    Off,
    Partial,
    On,
}

impl Tick {
    /// 块里能勾的 `total` 行勾了 `picked` 行。
    fn of(picked: usize, total: usize) -> Self {
        if picked == 0 {
            Tick::Off
        } else if picked >= total {
            Tick::On
        } else {
            Tick::Partial
        }
    }
}

/// 勾选框本身(14px 的方框;勾了一部分画成「−」)。
fn tick_mark(tick: Tick) -> gpui::Div {
    match tick {
        Tick::On => git_ui::check_box(true, ui::accent()),
        Tick::Off => git_ui::check_box(false, ui::accent()),
        Tick::Partial => div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(14.0))
            .rounded(px(3.0))
            .border_1()
            .border_color(ui::accent())
            .bg(ui::accent())
            .child(git_ui::icon(ICON_MINUS, 11.0, ui::bg_base())),
    }
}

/// 勾选框那一格(宽 [`CHECK_W`]、高一整行)。点它只翻勾选:停止冒泡,不碰块操作与拖选。
fn check_cell(
    id: impl Into<ElementId>,
    tick: Tick,
    on_toggle: impl Fn(&mut App) + 'static,
) -> AnyElement {
    let mark = tick_mark(tick);
    div()
        .id(id)
        .w(px(CHECK_W))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .child(mark)
        .on_click(move |_: &ClickEvent, _window, cx| {
            cx.stop_propagation();
            on_toggle(cx);
        })
        .into_any_element()
}

/// 勾选框列里不能勾的格子(上下文行、展开出来的行、块里没有这一种行的块头):占住列宽对齐。
fn check_blank() -> AnyElement {
    div().w(px(CHECK_W)).h_full().flex_none().into_any_element()
}

/// 增删行的勾选框;不能勾的行(见 [`DiffState::pickable_line`])给空格子。
fn line_check(state: &Entity<DiffState>, s: &DiffState, ix: usize) -> AnyElement {
    if !s.pickable_line(ix) {
        return check_blank();
    }
    let state = state.clone();
    check_cell(
        ("git-diff-pick", ix),
        Tick::of(usize::from(s.picks.contains(&ix)), 1),
        move |cx| {
            state.update(cx, |s, cx| {
                s.toggle_pick(ix);
                cx.notify();
            })
        },
    )
}

/// 行内视图块头的勾选框:一下勾上 / 取消这一块能勾的行,勾了一部分时画成「−」。块里没有
/// 增删行(不会出现)给空格子。并排视图的对应物在中间栏(见 [`ribbon`])。
fn hunk_check(state: &Entity<DiffState>, s: &DiffState, hunk: usize) -> AnyElement {
    let lines = s.flat.pickable_in(hunk);
    if lines.is_empty() {
        return check_blank();
    }
    let picked = lines.iter().filter(|ix| s.picks.contains(ix)).count();
    let state = state.clone();
    let head = s.flat.hunk_key(hunk);
    check_cell(
        SharedString::from(format!("git-diff-pick-hunk-{hunk}")),
        Tick::of(picked, lines.len()),
        move |cx| {
            state.update(cx, |s, cx| {
                // 块号还指着渲染时那一块才翻(见 [`ChangeKey`])
                if head.as_ref().is_some_and(|head| s.hunk_is(hunk, head)) {
                    s.toggle_lines(s.flat.pickable_in(hunk));
                }
                cx.notify();
            })
        },
    )
}

// ─── 弹窗外框尺寸 ─────────────────────────────────────────────
//
// diff 本身已经改成页签(见 [`tab`]),这一口径留给仍是弹窗、里面也嵌着 diff 正文的
// 贮藏列表与逐段解决冲突:与**用量统计面板**同一口径(`main.rs` 的 `usage_layer`:
// 左右各留 10vw、顶 10vh、底 5vh → 80vw × 85vh)。`Dialog` 的默认顶距正好是
// `视口高/10`(`dialog.rs:367`),所以只要宽取 80vw、正文高取 85vh,弹窗
// 的外框就与用量面板完全重合。
//
// ⚠️ 配套必须 `p_0()`:`Dialog` 默认 `pt/pb 24 + 左右 24`,不清零的话面板实际
// 高是 85vh+48px(底边越过 95vh),工具栏也不会贴着面板边。
//
// 原版两个 diff 弹窗各是 `w-[90vw] h-[80vh]` / `w-[92vw] h-[85vh]`
// (`DiffModal.tsx:186`、`CommitDiffModal.tsx:87`)。
const MODAL_W_RATIO: f32 = 0.8;
const MODAL_H_RATIO: f32 = 0.85;

/// diff 页签(单文件 / 提交 / 单文件历史 / 磁盘对比)与贮藏、冲突弹窗里右栏顶部工具栏的
/// 统一高度:标题区、跳转组、视图段控件、刷新(页签)或关闭(弹窗)按钮都在这一条上垂直居中。
pub(crate) const TOOLBAR_H: f32 = 48.0;
/// 提交 diff / 单文件历史 / 贮藏左栏的头部与页脚的左右内边距。
pub(crate) const SIDE_PAD: f32 = 12.0;

/// 返回 `(面板宽, 正文高)`。builder 每帧重跑,拖窗口改大小时跟着变。
pub(crate) fn modal_size(viewport: gpui::Size<gpui::Pixels>) -> (gpui::Pixels, gpui::Pixels) {
    (
        ui::clamp_dialog_width(viewport.width * MODAL_W_RATIO, viewport),
        // `chrome = 0`:正文即整块面板(标题/页脚都没有),下限交给 helper
        ui::clamp_dialog_body_height(viewport.height, viewport, MODAL_H_RATIO, px(0.0)),
    )
}

/// 两份 diff 结果是不是同一份:两侧全文、二进制 / 过大标记、编码都一样。块是由两侧全文
/// 算出来的(同样的输入算出同样的块),不必再逐块比。
fn same_diff(a: &GitDiffResult, b: &GitDiffResult) -> bool {
    a.old_content == b.old_content
        && a.new_content == b.new_content
        && a.is_binary == b.is_binary
        && a.too_large == b.too_large
        && a.encoding == b.encoding
}

/// 静默重取([`Fetch::Refresh`])拿回来的 `result` 与屏幕上这份(`current` 是正文、`error`
/// 是就地的错误原文)是不是一回事:同一份 diff,或者同一句错误。
fn refresh_unchanged(
    current: Option<&GitDiffResult>,
    error: Option<&str>,
    result: &anyhow::Result<GitDiffResult>,
) -> bool {
    match (result, current, error) {
        (Ok(new), Some(old), None) => same_diff(old, new),
        (Err(err), None, Some(old)) => format!("{err:#}") == old,
        _ => false,
    }
}

// ─── diff 内容的状态实体 ───────────────────────────────────────

/// 各种 diff 页签(与贮藏弹窗里嵌的 diff)共用的 diff 内容状态。
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
    /// 静默重取([`Fetch::Refresh`])在途:页签切回来、仓库变了、点刷新会凑到一起,在途时
    /// 不再叠一趟。被换文件 / 块操作后的重取顶掉、或块操作开跑时被作废
    /// ([`DiffState::cancel_refresh`]),随之清掉
    refreshing: bool,
    /// 三个列表各自的滚动句柄:跳转要用,并排两栏还要靠它摆位(纵向偏移由共用的滚动位置
    /// 每帧算出来写回,见 [`DiffState::sync_split`])。
    inline_scroll: UniformListScrollHandle,
    left_scroll: UniformListScrollHandle,
    right_scroll: UniformListScrollHandle,
    /// 并排两栏共用的纵向滚动位置(纵向滚动条也拿着它,见 [`split::SplitScroll`])
    split_scroll: split::SplitScroll,
    /// 并排两栏上一帧同步后的横向偏移(谁跟这个不一样谁就是这一帧动过的)
    synced_x: [Pixels; 2],
    /// 「上一处/下一处改动」走到第几个 hunk。`None` = 还没跳过。
    jump: Option<usize>,
    /// 正文的选区、拖选与排版登记(见 [`selection`])
    select: SelectState,
    /// 展开过的上下文行(见 [`fold`])。换一份 diff 时清空;原地重取时按行号 + 内容重新认
    revealed: fold::Revealed,
    /// 允不允许按行勾选:单文件 diff 且该文件支持块操作时由 [`load_file`] 打开,其余 diff
    /// 恒为 `false`。真正画不画勾选框还要看这份 diff 本身(见 [`DiffState::can_pick`])
    pickable: bool,
    /// 勾上的增删行([`Flat::lines`] 的下标,只落在块里,见 [`picks`])
    picks: BTreeSet<usize>,
    /// 上一次块操作已经落下去的行:原地重取时数勾选的「第几次出现」要先剔掉它们
    /// (见 [`picks::pick_keys`]),用过即清
    pick_skip: BTreeSet<usize>,
}

impl DiffState {
    fn new(font_size: f32) -> Self {
        Self {
            loading: true,
            error: None,
            result: None,
            flat: Flat::default(),
            view: ViewMode::SideBySide,
            font_size,
            request: 0,
            refreshing: false,
            inline_scroll: UniformListScrollHandle::new(),
            left_scroll: UniformListScrollHandle::new(),
            right_scroll: UniformListScrollHandle::new(),
            split_scroll: split::SplitScroll::default(),
            synced_x: [px(0.0); 2],
            jump: None,
            select: SelectState::default(),
            revealed: fold::Revealed::default(),
            pickable: false,
            picks: BTreeSet::new(),
            pick_skip: BTreeSet::new(),
        }
    }

    fn line_height(&self) -> f32 {
        (self.font_size * 1.6).round()
    }

    /// 换一份 diff(换文件 / 首次打开):勾选与展开都不带过去。原地重取不调它 —— 那时两样
    /// 都要按内容 / 行号认回来(见 [`DiffState::apply`])。
    fn forget_lines(&mut self) {
        self.picks.clear();
        self.pick_skip.clear();
        self.revealed = fold::Revealed::default();
    }

    fn apply(&mut self, result: anyhow::Result<GitDiffResult>) {
        self.loading = false;
        self.jump = None;
        // 行换了一套(换文件 / 块操作后原地重取):旧选区的行下标已对不上
        self.clear_selection();
        // 原地重取:勾选按内容键认回来(换文件时调用方已先清空,这里是空操作)
        let skip = std::mem::take(&mut self.pick_skip);
        let keys = picks::pick_keys(&self.flat.pick_lines(), &self.picks, &skip);
        match result {
            Ok(diff) => {
                let (flat, revealed) = flatten_diff(&diff, &self.revealed);
                self.flat = flat;
                self.revealed = revealed;
                self.result = Some(diff);
                self.error = None;
                self.picks = if self.can_pick() {
                    picks::remap_picks(&self.flat.pick_lines(), &keys)
                } else {
                    BTreeSet::new()
                };
            }
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                self.result = None;
                self.flat = Flat::default();
                self.forget_lines();
            }
        }
    }

    /// 发起一趟取数:按 [`Fetch`] 复位(`Fresh` 清空并回到顶部),令牌 +1。`Refresh` 撞上
    /// 已经在途的一趟(加载中 / 另一趟静默重取)就不发,返回 `false`。
    fn begin_fetch(&mut self, fetch: Fetch) -> bool {
        if fetch == Fetch::Refresh {
            if self.loading || self.refreshing {
                return false;
            }
            self.refreshing = true;
        } else {
            // 换文件 / 块操作后的重取顶掉在途的静默重取(令牌一换,它回来也落不了地)
            self.refreshing = false;
        }
        if fetch == Fetch::Fresh {
            self.loading = true;
            self.error = None;
            self.result = None;
            self.flat = Flat::default();
            // 换文件 = 换一份 diff:跳转计数与滚动位置(连同两栏同步基准)都要归零,否则新文件
            // 一进来就显示「3/12」,而两栏还停在上一份的偏移上;选区与展开过的上下文同理
            self.jump = None;
            self.clear_selection();
            self.forget_lines();
            self.reset_scroll();
        }
        self.request += 1;
        true
    }

    /// 一趟取数回来了:令牌对不上(被更新的一趟顶掉)就丢;`Refresh` 拿回来的与屏幕上这份
    /// 一样也不动 —— 选区、跳转计数、滚动位置都留着(页签切回来时绝大多数是这种)。真落了地
    /// 返回 `true`。
    fn land_fetch(
        &mut self,
        req: u64,
        fetch: Fetch,
        result: anyhow::Result<GitDiffResult>,
    ) -> bool {
        if self.request != req {
            return false;
        }
        self.refreshing = false;
        if fetch == Fetch::Refresh
            && refresh_unchanged(self.result.as_ref(), self.error.as_deref(), &result)
        {
            return false;
        }
        self.apply(result);
        true
    }

    /// 作废在途的静默重取(块操作开跑时调,见 [`run_hunk_op`]):它要是在操作期间落地,行模型
    /// 就换了一套,操作成功后 [`DiffState::mark_applied`] 再按点按钮时的块号去扣勾选就错位了。
    ///
    /// 只在确有一趟 `Refresh` 在途时动令牌:`refreshing` 为真说明最新的一趟就是它(`Fresh` /
    /// `AfterOp` 发起时会把它清掉),令牌 +1 只作废它一个。操作后的那趟 `AfterOp` 在这之后
    /// 才发起、拿的是更新的令牌,不受影响。
    fn cancel_refresh(&mut self) {
        if self.refreshing {
            self.refreshing = false;
            self.request += 1;
        }
    }

    /// 这份 diff 能不能按行勾选:打开方允许(见 [`DiffState::pickable`]),且不是二进制 /
    /// 过大 / 非 UTF-8(块操作按 UTF-8 文本拼接,别的编码一律只能整文件操作)。
    fn can_pick(&self) -> bool {
        self.pickable
            && self
                .result
                .as_ref()
                .is_some_and(|r| !r.is_binary && !r.too_large && r.encoding.is_none())
    }

    /// 勾选框列的宽:能勾时 [`CHECK_W`],否则 0。
    fn check_w(&self) -> f32 {
        if self.can_pick() { CHECK_W } else { 0.0 }
    }

    /// 第 `ix` 行([`Flat::lines`] 下标)能不能打勾:块里的增删行。展开出来的上下文行不能。
    fn pickable_line(&self, ix: usize) -> bool {
        self.can_pick()
            && ix < self.flat.hunk_lines
            && picks::pickable_kind(&self.flat.lines[ix].kind).is_some()
    }

    fn toggle_pick(&mut self, ix: usize) {
        if self.pickable_line(ix) && !self.picks.remove(&ix) {
            self.picks.insert(ix);
        }
    }

    /// 并排视图中间栏那颗勾选框:第 `change` 处改动能勾的行一下勾上 / 取消。
    fn toggle_change_picks(&mut self, change: usize) {
        self.toggle_lines(self.flat.change_lines(change));
    }

    /// 一批能勾的行(块头 / 中间栏的勾选框管的那些)全勾上了就全取消,否则全勾上。
    fn toggle_lines(&mut self, lines: Vec<usize>) {
        if !self.can_pick() {
            return;
        }
        if lines.iter().all(|ix| self.picks.contains(ix)) {
            for ix in lines {
                self.picks.remove(&ix);
            }
        } else {
            self.picks.extend(lines);
        }
    }

    /// 清空勾选(Esc / 工具栏「清除勾选」)。本来就没有勾选返回 `false`(Esc 据此放行)。
    fn clear_picks(&mut self) -> bool {
        let had = !self.picks.is_empty();
        self.picks.clear();
        had
    }

    /// 第 `hunk` 块里勾了几行。
    fn picks_in_hunk(&self, hunk: usize) -> usize {
        match (
            self.flat.hunk_base.get(hunk),
            self.flat.hunk_base.get(hunk + 1),
        ) {
            (Some(&from), Some(&to)) => self.picks.range(from..to).count(),
            _ => 0,
        }
    }

    /// 并排视图第 `change` 处改动里勾了几行。
    fn picks_in_change(&self, change: usize) -> usize {
        self.flat
            .split
            .changes
            .get(change)
            .map_or(0, |c| self.picks.range(c.lines.clone()).count())
    }

    /// 按钮回调里记的第 `change` 处改动(身份 `key`)是不是还在原处(见 [`ChangeKey`])。
    fn change_is(&self, change: usize, key: Option<ChangeKey>) -> bool {
        key.is_some() && self.flat.change_key(change) == key
    }

    /// 同上,行内视图的第 `hunk` 块(身份是块头)。
    fn hunk_is(&self, hunk: usize, key: &SharedString) -> bool {
        self.flat.hunk_key(hunk).as_ref() == Some(key)
    }

    /// 点块操作按钮那一刻要交给后端的东西:屏幕上的这份 diff + 要动的块与行(见 [`OpTarget`])。
    /// 没有 diff、块号越界、跨块操作却一行都没勾 → `None`。
    fn op_target(&self, scope: OpScope) -> Option<OpTarget> {
        let diff = self.result.clone()?;
        match scope {
            OpScope::Hunk(hunk) => {
                if hunk >= diff.hunks.len() {
                    return None;
                }
                let mut picks = picks::group_picks(&self.flat.hunk_base, &self.picks, Some(hunk));
                let lines = picks
                    .first()
                    .and_then(|pick| pick.lines.as_ref())
                    .map(|lines| lines.len());
                if picks.is_empty() {
                    picks.push(HunkPick::whole(hunk));
                }
                Some(OpTarget {
                    diff,
                    picks,
                    busy: Busy::Hunk(hunk),
                    land: Some(hunk),
                    lines,
                    change: false,
                })
            }
            // 一处改动 = 所在 hunk 里的这几行增删行,按行交给后端。这一处里勾了行就只动勾上的。
            // 这几行就是这块剩下的全部增删行时与整块逐字节一致;这一处是块里最后一处改动、块又
            // 碰到文件末尾时,末尾换行跟随来源侧(否则那处看不见的差异会被留下,见
            // `mt_project::git::hunk` 模块注释「按行操作」)
            OpScope::Change(change) => {
                let hunk = self.flat.split.changes.get(change)?.hunk;
                if hunk >= diff.hunks.len() {
                    return None;
                }
                let block: BTreeSet<usize> = self.flat.change_lines(change).into_iter().collect();
                let picked: BTreeSet<usize> = block.intersection(&self.picks).copied().collect();
                let whole = picked.is_empty();
                let lines = if whole { block } else { picked };
                let picks = picks::group_picks(&self.flat.hunk_base, &lines, None);
                if picks.is_empty() {
                    return None;
                }
                // 这块整个落下去了(动的是它剩下的全部增删行)才按块的口径落到下一块;块还在
                // 就停在原地 —— 跳回块头的话,长 hunk 里点了第 25 处会被拽回上百行之外
                let gone = lines.iter().copied().eq(self.flat.pickable_in(hunk));
                Some(OpTarget {
                    diff,
                    picks,
                    busy: Busy::Change(change),
                    land: gone.then_some(hunk),
                    lines: Some(lines.len()),
                    change: whole,
                })
            }
            OpScope::Picked => {
                let picks = picks::group_picks(&self.flat.hunk_base, &self.picks, None);
                let land = picks.first()?.hunk;
                let lines = picks
                    .iter()
                    .filter_map(|pick| pick.lines.as_ref())
                    .map(|lines| lines.len())
                    .sum();
                Some(OpTarget {
                    diff,
                    picks,
                    busy: Busy::Lines,
                    land: Some(land),
                    lines: Some(lines),
                    change: false,
                })
            }
        }
    }

    /// 块操作成功、重取之前:落下去的行从勾选里拿掉(它们马上就从 diff 里消失了),并记进
    /// [`DiffState::pick_skip`],好让剩下的勾选在新 diff 里按内容认得准。
    fn mark_applied(&mut self, picks: &[HunkPick]) {
        let applied = self.flat.applied_lines(picks);
        self.picks.retain(|ix| !applied.contains(ix));
        self.pick_skip = applied;
    }

    /// 展开一条分隔行(按 `(gap, start)` 认是哪一段:点击回调里记的下标可能已过期)。
    /// 只重排行模型,块里的行、词级高亮、勾选都不动;选区按行号平移过去(见
    /// [`DiffState::shift_selection`])。
    fn expand(&mut self, gap: usize, start: u32, how: fold::Expand) {
        let Some(diff) = self.result.as_ref() else {
            return;
        };
        let rows = |f: &Flat| {
            [
                f.inline_rows.len(),
                f.split.rows[LEFT].len(),
                f.split.rows[RIGHT].len(),
            ]
        };
        let before = rows(&self.flat);
        let Some(target) =
            self.flat
                .expand_fold(&diff.old_content, &mut self.revealed, gap, start, how)
        else {
            return;
        };
        let after = rows(&self.flat);
        let delta = |k: usize| after[k] as isize - before[k] as isize;
        self.shift_selection([
            (target.inline_row, delta(0)),
            (target.old_row, delta(1)),
            (target.new_row, delta(2)),
        ]);
    }

    /// 三个列表都回到顶部、左缘(换文件时用,否则新文件一进来停在上一份的偏移上)。
    fn reset_scroll(&mut self) {
        self.inline_scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.0), px(0.0)));
        self.reset_split();
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
        self.jump_to(next);
    }

    /// 直接落到第 `ix` 个 hunk 头;越界取最后一块。按块操作后原地重取 diff 时用它
    /// 停在「原位置上的下一块」(刚处理掉的那块消失了,后面的块顶上来占了它的号)。
    fn jump_to(&mut self, ix: usize) {
        let total = self.flat.heads.len();
        if total == 0 {
            return;
        }
        let next = ix.min(total - 1);
        self.jump = Some(next);
        match self.view {
            // strict:哪怕目标已在视野里也要把它顶到最上面 —— 连着点「下一处」
            // 时,不这么做的话相邻两个 hunk 会看不出画面变化
            ViewMode::Inline => self
                .inline_scroll
                .scroll_to_item_strict(self.flat.inline_jumps[next], ScrollStrategy::Top),
            // 两栏共用滚动位置:这一块的第一行顶到上缘,两栏都把它滚进视野(见 [`split`])
            ViewMode::SideBySide => self.split_jump(next),
        }
    }
}

impl Render for DiffState {
    /// `DiffState` 只当状态盒子用:正文由宿主(diff 页签 / 贮藏弹窗的 builder)按它现画,
    /// 行回调、按钮回调里拿着它的句柄改状态。它自己不画东西。
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
/// (loading / binary / tooLarge / 额外的空态),单文件与提交两种 diff 各给各的命名空间。
///
/// `empty` 是「没有任何 hunk」时的空态文案(单文件 diff 按块处理完最后一块就落到这里);
/// `None` 时照旧画一张空列表(提交 diff 的口径不变)。`hunks` 是块操作按钮的上下文,
/// 只有单文件 diff 且该文件支持按块操作时才给。
#[allow(clippy::too_many_arguments)]
fn render_body(
    state: &Entity<DiffState>,
    loading: &'static str,
    binary: &'static str,
    too_large: &'static str,
    empty: Option<&'static str>,
    hunks: Option<HunkUi>,
    cx: &mut App,
) -> AnyElement {
    let s = state.read(cx);
    if s.loading {
        return git_ui::loading_state(loading)
            .size_full()
            .into_any_element();
    }
    if let Some(err) = &s.error {
        return div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .px(px(24.0))
            .child(git_ui::notice_line(err.clone(), ui::color_error()))
            .into_any_element();
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
    if let Some(empty) = empty.filter(|_| result.hunks.is_empty()) {
        return git_ui::empty_state(ICON_CLEAN, empty, None)
            .size_full()
            .into_any_element();
    }
    let view = match s.view {
        ViewMode::Inline => render_inline(state, hunks, cx),
        ViewMode::SideBySide => split::render_side_by_side(state, hunks, cx),
    };
    mono_body(view)
}

/// diff 正文的等宽字体壳。
///
/// 原版是 `font-mono`(`--app-font-mono`:JetBrains Mono → Cascadia Code →
/// Consolas);gpui 的字族是单值 + fallback 链,这里与文件查看器
/// (`file_viewer/mod.rs` 的编辑器分支)用同一条链,连「用户配过 uiFontFamily 就让它
/// 优先」的口径也一致(原版 `fontManager.ts:8-18` 会一并覆盖 `--app-font-mono`)。
///
/// 不挂字族的话 diff 会继承界面字体 —— 比例字体下行号与代码列全对不齐。
fn mono_body(body: AnyElement) -> AnyElement {
    let mut wrap = div().size_full();
    let ts = wrap.text_style();
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
///
/// 行号是旧 / 新两列(原版只有一列:增删行在那一列里显示 `+` / `-`,上下文行显示旧行号,
/// 改了一段之后新文件的行号就对不上了);符号单独一列。
fn render_inline(state: &Entity<DiffState>, hunks: Option<HunkUi>, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let count = s.flat.inline_rows.len();
    let line_height = s.line_height();
    let font_size = s.font_size;
    let widest_ix = s.flat.widest_inline;
    let scroll = s.inline_scroll.clone();
    let rows_state = state.clone();
    let list = uniform_list(
        "git-diff-inline",
        count,
        move |range, _window, cx: &mut App| {
            let selection_bg = selection::selection_bg(cx);
            let s = rows_state.read(cx);
            s.retain_layouts(Pane::Inline, &range);
            let scroll = s.inline_scroll.0.borrow().base_handle.offset();
            let can_pick = s.can_pick();
            let gutter = INLINE_GUTTER + s.check_w();
            range
                .map(|i| match s.flat.inline_rows[i] {
                    InlineRow::Head(h) => hunk_row(
                        s.flat.heads[h].inline.clone(),
                        line_height,
                        gutter,
                        can_pick.then(|| hunk_check(&rows_state, s, h)),
                        hunks.as_ref().map(|ui| {
                            let head = s.flat.heads[h].inline.clone();
                            hunk_actions(ui, h, head, s.picks_in_hunk(h))
                        }),
                        scroll.x,
                    ),
                    InlineRow::Fold(f) => fold_row(
                        &rows_state,
                        &s.flat.folds[f],
                        Pane::Inline,
                        line_height,
                        gutter,
                        true,
                        scroll.x,
                    ),
                    InlineRow::Line(ix) => {
                        let line = &s.flat.lines[ix];
                        let no = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
                        let sign = match line.kind.as_str() {
                            "add" => "+",
                            "delete" => "-",
                            _ => "",
                        };
                        diff_line_row(
                            line,
                            &[
                                (no(line.old_lineno), INLINE_NUM),
                                (no(line.new_lineno), INLINE_NUM),
                                (sign.to_string(), INLINE_SIGN),
                            ],
                            can_pick.then(|| line_check(&rows_state, s, ix)),
                            line_height,
                            &s.flat.marks[ix],
                            scroll,
                            s.row_text(Pane::Inline, i, ix, selection_bg),
                        )
                    }
                })
                .collect::<Vec<_>>()
        },
    )
    .size_full()
    .track_scroll(&scroll)
    // 见模块注释第 3 条:量宽只量这一行,量错了长行就滚不到头
    .with_width_from_item(Some(widest_ix))
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained);
    // 字号挂在外面一层:uniform_list 在 prepaint 里先量宽、后才套上自己的样式,
    // 字号挂在它自己身上的话量宽用的是继承来的界面字号,长行滚不到头
    let body = div()
        .relative()
        .size_full()
        .text_size(ui::font_px(font_size))
        .child(list)
        .child(h_scrollbar("git-diff-inline-hbar", &scroll));
    selection::text_mouse(body, state, Pane::Inline)
        .child(selection::drag_listener(state))
        .into_any_element()
}

/// 横向滚动条(只在有行比视野宽时出现,常显):超长行有没有截在视野外,一眼看得出,
/// 也能直接拖。要自己套一层 `absolute` + 四边贴 0 的壳,理由见 `menu.rs` 同款注释。
fn h_scrollbar(id: &'static str, handle: &UniformListScrollHandle) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .child(
            Scrollbar::horizontal(handle)
                .id(id)
                .mode(ScrollbarMode::Always),
        )
}

/// 工具栏右上角那组控件的文案。单文件与提交两种 diff 各有各的命名空间(`diffModal` /
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
    icon: &'static [mt_ui::icons::Shape],
    tip: &'static str,
    delta: isize,
) -> AnyElement {
    let state = state.clone();
    icon_button(id, icon, tip, true)
        .on_click(move |_: &ClickEvent, _window, cx| {
            state.update(cx, |s, cx| {
                s.jump_by(delta);
                cx.notify();
            });
        })
        .into_any_element()
}

/// 跳转组中间的计数:跳过之后是 `2/5`,还没跳过是「共 5 处」。
fn counter_text(jump: Option<usize>, total: usize) -> String {
    match jump {
        Some(cur) => format!("{}/{}", cur + 1, total),
        None => tr!("gitCommit", "changeTotal", count = total.to_string()),
    }
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
    // 还没跳过时不硬说「第 1 处」—— 视野里那处是哪一处,滚动位置说了不算;也不写
    // 「–/N」(看着像漏了个数),只报总数。跳过一次之后才是「第几处 / 共几处」
    let counter = counter_text(s.jump, total);
    Some(
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .mr(px(8.0))
            .child(jump_button(
                state,
                SharedString::from(format!("{id_prefix}-jump-prev")),
                ICON_ARROW_UP,
                labels.prev,
                -1,
            ))
            .child(
                mono(div())
                    .px(px(2.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(counter),
            )
            .child(jump_button(
                state,
                SharedString::from(format!("{id_prefix}-jump-next")),
                ICON_ARROW_DOWN,
                labels.next,
                1,
            ))
            .into_any_element(),
    )
}

/// 工具栏最右端那一颗。
pub(crate) enum ToolbarTail {
    /// 嵌在弹窗里(贮藏列表):✕ 关掉这种弹窗(值是它的覆盖物种类)
    Close(&'static str),
    /// diff 页签:「刷新」(页签自己的 × 在页签条上,中键 / Ctrl+W 也关得掉)
    Refresh(AnyElement),
}

/// 页签工具栏上的「刷新」图标按钮(`on_refresh` 是点下去要做的事)。
pub(crate) fn refresh_button(
    id: impl Into<ElementId>,
    on_refresh: impl Fn(&mut App) + 'static,
) -> AnyElement {
    icon_button(id, ICON_REFRESH, t("gitCommit", "refresh"), true)
        .on_click(move |_: &ClickEvent, _window, cx| on_refresh(cx))
        .into_any_element()
}

/// 跳转组 + 视图段控件 + 最右端那一颗(✕ 或刷新,见 [`ToolbarTail`])。
fn view_toggle(
    state: &Entity<DiffState>,
    id_prefix: &'static str,
    labels: ToolbarLabels,
    tail: ToolbarTail,
    cx: &mut App,
) -> AnyElement {
    let (side_label, inline_label) = (labels.side, labels.inline);
    let jumps = jump_group(state, id_prefix, &labels, cx);
    let current = state.read(cx).view;
    let mut seg = git_ui::segmented();
    for (mode, label) in [
        (ViewMode::SideBySide, side_label),
        (ViewMode::Inline, inline_label),
    ] {
        let state = state.clone();
        seg = seg.child(
            git_ui::segment(
                SharedString::from(format!(
                    "{id_prefix}-view-{}",
                    if matches!(mode, ViewMode::Inline) {
                        "inline"
                    } else {
                        "side"
                    }
                )),
                label,
                mode == current,
            )
            .on_click(move |_: &ClickEvent, _window, cx| {
                state.update(cx, |s, cx| {
                    // 两个视图的行下标不是一回事:换视图就清掉选区
                    if s.view != mode {
                        s.view = mode;
                        s.clear_selection();
                    }
                    cx.notify();
                });
            }),
        );
    }

    let tail = match tail {
        ToolbarTail::Close(kind) => icon_button(
            SharedString::from(format!("{id_prefix}-close")),
            ICON_CLOSE,
            t("prompt", "close"),
            true,
        )
        .on_click(move |_: &ClickEvent, window, cx| {
            crate::prompt::close_guarded(kind, window, cx);
        })
        .into_any_element(),
        ToolbarTail::Refresh(button) => button,
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .children(jumps)
        .child(seg)
        .child(div().flex_none().ml(px(8.0)).child(tail))
        .into_any_element()
}

// ─── 单文件 diff(工作区 / 暂存区,原版 DiffModal) ─────────────────

/// 单文件 diff 页签里能前后切换的一个文件。
#[derive(Debug, Clone)]
pub struct DiffFile {
    /// 相对 `repo_path` 的路径
    pub path: String,
    /// 重命名前的路径(已暂存的重命名才有):HEAD 那一侧按它取,否则新路径在 HEAD 里
    /// 查不到、整份显示成新增(见 `mt_project::git::get_git_diff_from`)
    pub old_path: Option<String>,
    pub side: DiffSide,
    /// 标题栏状态胶囊里的字母(`M` / `A` / …)
    pub status_label: String,
    /// 支不支持按块操作。由调用方按状态判(未跟踪 / 冲突 / 重命名 / 删除只支持整文件
    /// 操作);[`DiffSide::Working`] 一律不支持。二进制 / 过大由 diff 结果自己挡掉。
    pub hunk_actions: bool,
}

/// 按块操作成功后的回调(更改列表据此重载)。
pub type OnChanged = Rc<dyn Fn(&mut App)>;

/// Alt+← / Alt+→(或标题栏按钮)换到了序列里的另一个文件(更改列表据此把选中项跟过去)。
pub type OnSwitch = Rc<dyn Fn(&DiffFile, &mut App)>;

/// 按块操作成功后重取文件序列:打开方重载自己的列表,落地后把**打开时那个分组**的新
/// 序列交给回执(见 [`FilesDone`])。给了它就不必再给 `on_changed`(重载就是它)。
pub type ReloadFiles = Rc<dyn Fn(FilesDone, &mut App)>;

/// [`ReloadFiles`] 的回执:`None` = 没取到(重载失败、换了仓库),沿用旧序列。
/// **必须调一次** —— 页签的块操作按钮等它回来才放开(页签已经关了就什么都不做)。
pub type FilesDone = Box<dyn FnOnce(Option<Vec<DiffFile>>, &mut App)>;

/// [`open_file_diff_with`] 的回调。
#[derive(Default)]
pub struct FileDiffHooks {
    pub on_changed: Option<OnChanged>,
    pub on_switch: Option<OnSwitch>,
    pub reload_files: Option<ReloadFiles>,
}

/// 三种块操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HunkOp {
    Stage,
    Unstage,
    Discard,
}

/// 进行中的块操作是从哪颗按钮点的(「处理中…」画在哪)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    /// 行内视图某一块头上的按钮(整块,或块里勾上的行)
    Hunk(usize),
    /// 并排视图中间栏里第 n 处改动的按钮(这一处,或这一处里勾上的行)
    Change(usize),
    /// 工具栏上跨块的「所选行」
    Lines,
}

/// 块操作作用在哪:某一块(块里勾了行就只动勾上的,没勾就整块)、并排视图里的某一处改动
/// (同理)、或跨块的全部勾选。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpScope {
    Hunk(usize),
    Change(usize),
    Picked,
}

/// 一次块操作交给后端的全部东西,在**点按钮那一刻**取好(见 [`DiffState::op_target`]):
/// 丢弃要先确认,确认框开着时屏幕上的 diff 与勾选不会变,落下去的就是用户看到的那些;
/// 后端再重读两侧防陈旧。
#[derive(Clone)]
struct OpTarget {
    diff: GitDiffResult,
    /// 要动的块(块号递增)与各块生效的行(`None` = 整块)
    picks: Vec<HunkPick>,
    busy: Busy,
    /// 操作后原地重取时停在第几块(原位置上的那一块 / 下一块);`None` = 不跳,停在原地
    land: Option<usize>,
    /// 按行操作时一共几行(确认框文案用);整块是 `None`
    lines: Option<usize>,
    /// 并排视图里整处改动(没勾行)的操作:确认框说「这一处改动」而不是「勾选的行」
    change: bool,
}

/// 单文件 diff 页签的控制面:文件序列、当前下标、进行中的块操作与就地错误。
///
/// 与 [`DiffState`] 分开:后者是各种 diff 共用的「diff 内容」,这些是单文件 diff 独有的。
/// 页签观察它:换了文件(`current` / `files` 变了)就跟着改身份与标题(见 [`tab`])。
struct FileDiff {
    repo_path: String,
    files: Vec<DiffFile>,
    current: usize,
    on_changed: Option<OnChanged>,
    on_switch: Option<OnSwitch>,
    reload_files: Option<ReloadFiles>,
    /// 进行中的块操作。`Some` 时全部块按钮(连同工具栏的「所选行」)禁用,防重复点击
    busy: Option<Busy>,
    /// 块操作失败的原文。就地显示在工具栏下方(多行原文,toast 装不下)
    op_error: Option<String>,
    /// 失败是因为 diff 过期:错误条上给「刷新」
    stale: bool,
}

impl FileDiff {
    fn file(&self) -> &DiffFile {
        &self.files[self.current]
    }
}

impl Render for FileDiff {
    /// 同 [`DiffState`]:只当状态盒子用。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 块操作按钮要的上下文。只有单文件 diff 且该文件支持按块操作时才有。
#[derive(Clone)]
struct HunkUi {
    state: Entity<DiffState>,
    ctl: Entity<FileDiff>,
    side: DiffSide,
    /// 进行中的块操作
    busy: Option<Busy>,
    /// 文件不是 UTF-8 时按钮一律禁用,悬停给这句原因(块操作按 UTF-8 拼接写回,
    /// 会把 GBK / UTF-16 文件写坏;整文件暂存不受影响)
    blocked: Option<SharedString>,
}

/// 「处理中…」(块头 / 工具栏上正在跑的那颗按钮换成它)。
fn working_label() -> gpui::Div {
    div()
        .flex()
        .items_center()
        .px(px(8.0))
        .text_size(ui::font_px(11.0))
        .text_color(ui::text_muted())
        .child(t("gitCommit", "hunkWorking"))
}

/// 块操作按钮上的字:块里勾了行(`picked > 0`)就是「暂存所选 N 行」这类,只动勾上的行;
/// 没勾就是「暂存此块」这类整块操作。
fn op_label(op: HunkOp, picked: usize) -> SharedString {
    if picked == 0 {
        return match op {
            HunkOp::Stage => t("gitCommit", "stageHunk"),
            HunkOp::Unstage => t("gitCommit", "unstageHunk"),
            HunkOp::Discard => t("gitCommit", "discardHunk"),
        }
        .into();
    }
    let count = picked.to_string();
    match op {
        HunkOp::Stage => tr!("gitCommit", "stageLines", count = count),
        HunkOp::Unstage => tr!("gitCommit", "unstageLines", count = count),
        HunkOp::Discard => tr!("gitCommit", "discardLines", count = count),
    }
    .into()
}

/// 这一侧给哪几种块操作:未暂存侧「暂存 / 丢弃」,已暂存侧「取消暂存」。
fn side_ops(side: DiffSide) -> &'static [HunkOp] {
    match side {
        DiffSide::Unstaged => &[HunkOp::Stage, HunkOp::Discard],
        DiffSide::Staged => &[HunkOp::Unstage],
        DiffSide::Working => &[],
    }
}

fn op_key(op: HunkOp) -> &'static str {
    match op {
        HunkOp::Stage => "stage",
        HunkOp::Unstage => "unstage",
        HunkOp::Discard => "discard",
    }
}

/// hunk 头上的块操作按钮:未暂存侧「暂存此块 / 丢弃此块」,已暂存侧「取消暂存此块」;
/// 块里勾了 `picked` 行时换成「暂存所选 N 行」这类、只对勾上的行生效。
/// 正在处理的那一块换成「处理中…」,其余块的按钮一并禁用。`head` 是这一块的块头(点下去时
/// 核对块号还指不指着它,见 [`ChangeKey`])。
fn hunk_actions(hunk_ui: &HunkUi, hunk: usize, head: SharedString, picked: usize) -> AnyElement {
    if hunk_ui.busy == Some(Busy::Hunk(hunk)) {
        return working_label().into_any_element();
    }
    let enabled = hunk_ui.busy.is_none() && hunk_ui.blocked.is_none();
    let blocked = hunk_ui.blocked.clone();
    let button = |op: HunkOp| {
        let (state, ctl) = (hunk_ui.state.clone(), hunk_ui.ctl.clone());
        let head = head.clone();
        text_button(
            SharedString::from(format!("git-hunk-{}-{hunk}", op_key(op))),
            op_label(op, picked),
            enabled,
            op == HunkOp::Discard,
        )
        .when_some(blocked.clone(), |el, reason| el.tip(reason))
        .when(enabled, move |el| {
            el.on_click(move |_: &ClickEvent, window, cx| {
                cx.stop_propagation();
                if still_there(&state, cx, |s| s.hunk_is(hunk, &head)) {
                    request_hunk_op(&state, &ctl, op, OpScope::Hunk(hunk), window, cx);
                }
            })
        })
    };
    div()
        .h_full()
        .flex()
        .items_stretch()
        .gap(px(8.0))
        .children(side_ops(hunk_ui.side).iter().map(|&op| button(op)))
        .into_any_element()
}

/// 工具栏上跨块的「暂存所选 N 行 / 丢弃所选 N 行 / 取消暂存所选 N 行」+「清除勾选」。只在
/// 勾了行时出现(没勾时不占工具栏);跨块一次交给多块后端,不逐块落盘(见 [`picks`])。
fn picks_toolbar(
    state: &Entity<DiffState>,
    ctl: &Entity<FileDiff>,
    side: DiffSide,
    busy: Option<Busy>,
    cx: &App,
) -> Option<AnyElement> {
    let s = state.read(cx);
    if !s.can_pick() || s.picks.is_empty() {
        return None;
    }
    let group = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .mr(px(16.0));
    if busy == Some(Busy::Lines) {
        return Some(group.child(working_label()).into_any_element());
    }
    let enabled = busy.is_none();
    let picked = s.picks.len();
    let button = |op: HunkOp| {
        let (state, ctl) = (state.clone(), ctl.clone());
        let kind = if op == HunkOp::Discard {
            ButtonKind::Danger
        } else {
            ButtonKind::Ghost
        };
        action_button(
            SharedString::from(format!("git-diff-picks-{}", op_key(op))),
            op_label(op, picked),
            kind,
            enabled,
        )
        .when(enabled, move |el| {
            el.on_click(move |_: &ClickEvent, window, cx| {
                request_hunk_op(&state, &ctl, op, OpScope::Picked, window, cx);
            })
        })
    };
    let clear = {
        let state = state.clone();
        action_button(
            "git-diff-picks-clear",
            t("gitCommit", "clearPicks"),
            ButtonKind::Ghost,
            true,
        )
        .tip(t("gitCommit", "clearPicksTip"))
        .on_click(move |_: &ClickEvent, _window, cx| {
            state.update(cx, |s, cx| {
                if s.clear_picks() {
                    cx.notify();
                }
            });
        })
    };
    Some(
        group
            .children(side_ops(side).iter().map(|&op| button(op)))
            .child(clear)
            .into_any_element(),
    )
}

/// 非 UTF-8 文件的块操作按钮为什么是灰的(悬停说明)。
fn non_utf8_hunk_reason(encoding: TextEncoding) -> SharedString {
    let label = match encoding.label() {
        "" => t("gitCommit", "hunkUnknownEncoding"),
        label => label,
    };
    tr!("gitCommit", "hunkNonUtf8", encoding = label).into()
}

/// 按钮点下去时,它记的作用对象还在不在原处(`check` 拿眼下的行模型核对,见 [`ChangeKey`])。
/// 不在就重画一帧换上新按钮、返回 `false`,这次点击作罢。
fn still_there(
    state: &Entity<DiffState>,
    cx: &mut App,
    check: impl FnOnce(&DiffState) -> bool,
) -> bool {
    if check(state.read(cx)) {
        return true;
    }
    state.update(cx, |_, cx| cx.notify());
    false
}

/// 点了块操作按钮(行内视图块头上的、并排视图中间栏里的,或工具栏上跨块的「所选行」)。
/// 要动的块与行在这一刻取好
/// ([`DiffState::op_target`])。丢弃先确认(不可恢复),确认框开着时屏幕上的 diff 与勾选
/// 不会变。按行丢弃的确认文案写明行数。
fn request_hunk_op(
    state: &Entity<DiffState>,
    ctl: &Entity<FileDiff>,
    op: HunkOp,
    scope: OpScope,
    window: &mut Window,
    cx: &mut App,
) {
    if ctl.read(cx).busy.is_some() {
        return;
    }
    let Some(target) = state.read(cx).op_target(scope) else {
        return;
    };
    if op != HunkOp::Discard {
        run_hunk_op(state, ctl, op, target, cx);
        return;
    }
    let (title, message): (&str, SharedString) = match target.lines {
        None => (
            t("gitCommit", "discardHunkTitle"),
            t("gitCommit", "discardHunkConfirm").into(),
        ),
        Some(lines) if target.change => (
            t("gitCommit", "discardChangeTitle"),
            tr!(
                "gitCommit",
                "discardChangeConfirm",
                count = lines.to_string()
            )
            .into(),
        ),
        Some(lines) => (
            t("gitCommit", "discardLinesTitle"),
            tr!(
                "gitCommit",
                "discardLinesConfirm",
                count = lines.to_string()
            )
            .into(),
        ),
    };
    let (state, ctl) = (state.clone(), ctl.clone());
    Confirm::new(title, message)
        .ok_text(t("gitChanges", "discardOk"))
        .danger()
        .open(
            move |_window, cx| run_hunk_op(&state, &ctl, op, target.clone(), cx),
            window,
            cx,
        );
}

/// 真正执行块操作:拿点按钮时那份 diff(两侧全文 + 块)与要动的块 / 行交给多块后端,后端
/// 重读两侧比对,对不上就拒绝(见 `mt_project::git::hunk` 模块注释)。几块一趟拼完、只落
/// 一次盘。成功后通知更改列表并原地重取 diff;失败就地红字。
fn run_hunk_op(
    state: &Entity<DiffState>,
    ctl: &Entity<FileDiff>,
    op: HunkOp,
    target: OpTarget,
    cx: &mut App,
) {
    let OpTarget {
        diff,
        picks,
        busy,
        land,
        ..
    } = target;
    let (repo, path) = {
        let c = ctl.read(cx);
        if c.busy.is_some() {
            return;
        }
        (c.repo_path.clone(), c.file().path.clone())
    };
    // 按块操作同样写暂存区 / 工作区:与面板上的其它写操作共用一道闸,被占时就地说明
    let Some(guard) = git_ui::gate::try_acquire(&repo) else {
        ctl.update(cx, |c, cx| {
            c.op_error = Some(git_ui::gate_busy_text().to_string());
            c.stale = false;
            cx.notify();
        });
        return;
    };
    cx.refresh_windows();
    ctl.update(cx, |c, cx| {
        c.busy = Some(busy);
        c.op_error = None;
        c.stale = false;
        cx.notify();
    });
    // 操作期间屏幕上这份 diff 不能被在途的静默重取换掉(`busy` 只挡得住新发起的)
    state.update(cx, |s, _| s.cancel_refresh());
    let (state, ctl) = (state.clone(), ctl.clone());
    let applied = picks.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let repo = std::path::Path::new(&repo);
                match op {
                    HunkOp::Stage => mt_project::git::stage_hunks(repo, &path, &diff, &picks),
                    HunkOp::Unstage => mt_project::git::unstage_hunks(repo, &path, &diff, &picks),
                    HunkOp::Discard => mt_project::git::discard_hunks(repo, &path, &diff, &picks),
                }
            })
            .await;
        drop(guard);
        cx.update(|cx| cx.refresh_windows());
        cx.update(|cx| match result {
            Ok(()) => {
                // 落下去的行从勾选里拿掉,剩下的勾选在重取后按内容认回来
                state.update(cx, |s, cx| {
                    s.mark_applied(&applied);
                    cx.notify();
                });
                let (on_changed, reload_files) = {
                    let c = ctl.read(cx);
                    (c.on_changed.clone(), c.reload_files.clone())
                };
                if let Some(on_changed) = on_changed {
                    on_changed(cx);
                }
                // `busy` 留到重取的 diff 落地才清(见 `load_file`):这中间屏幕上还是
                // 操作前的那份,放开按钮的话下一次点击拿的就是过期的块
                match reload_files {
                    // 文件序列跟着更改列表走:取消暂存最后一块后文件就离开了「已暂存」组,
                    // 序号与 Alt+←/→ 不能还按打开时的快照
                    // 回执只拿弱引用:列表重载落地前页签可能已经被关掉,那就什么都不做
                    Some(reload) => {
                        let (state, ctl) = (state.downgrade(), ctl.downgrade());
                        reload(
                            Box::new(move |files, cx| {
                                if let (Some(state), Some(ctl)) = (state.upgrade(), ctl.upgrade()) {
                                    resync_files(&state, &ctl, files, land, cx);
                                }
                            }),
                            cx,
                        );
                    }
                    None => load_file(&state, &ctl, Fetch::AfterOp(land), cx),
                }
            }
            Err(err) => ctl.update(cx, |c, cx| {
                eprintln!("[git] 按块操作失败: {err:#}");
                c.busy = None;
                c.stale = err.is::<StaleDiff>();
                c.op_error = Some(if c.stale {
                    t("gitCommit", "hunkStale").to_string()
                } else {
                    tr!("gitCommit", "hunkFailed", detail = format!("{err:#}"))
                });
                cx.notify();
            }),
        });
    })
    .detach();
}

/// 取一份 diff 的三种时机(决定落地时动不动旧内容、滚动位置与勾选)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fetch {
    /// 换文件 / 首次打开:清空(勾选与展开一并清掉)、回「加载中」、回到顶部
    Fresh,
    /// 按块操作后原地重取(错误条上的「刷新」也走这条):不清空旧内容、不回「加载中」(否则
    /// 整页闪一下),`Some(k)` 落到第 k 块(原位置上的下一块)、`None` 不动滚动位置,并清掉
    /// 块操作的 `busy`
    AfterOp(Option<usize>),
    /// 页签切回来 / 工具栏「刷新」/ 仓库变了:静默原地重取。拿回来的与屏幕上的一样就什么都
    /// 不动(见 [`DiffState::refresh_unchanged`]);变了照旧按内容认回勾选、按行号认回展开,
    /// 滚动位置不动、不跳块。已经有一趟在途(加载中 / 另一趟静默重取)就不叠
    Refresh,
}

/// 取当前文件的 diff([`Fetch`] 决定时机)。后两种的行勾选按内容认回来、展开过的上下文
/// 按行号认回来(见 [`DiffState::apply`])。
fn load_file(state: &Entity<DiffState>, ctl: &Entity<FileDiff>, fetch: Fetch, cx: &mut App) {
    let (repo, file) = {
        let c = ctl.read(cx);
        (c.repo_path.clone(), c.file().clone())
    };
    let req = state.update(cx, |s, cx| {
        if !s.begin_fetch(fetch) {
            return None;
        }
        // 按行勾选与块操作同一口径(见 `DiffFile::hunk_actions`):换了文件跟着换
        s.pickable = file.hunk_actions && file.side != DiffSide::Working;
        cx.notify();
        Some(s.request)
    });
    let Some(req) = req else {
        return;
    };
    let (state, ctl) = (state.clone(), ctl.clone());
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                mt_project::git::get_git_diff_from(
                    std::path::Path::new(&repo),
                    &file.path,
                    file.old_path.as_deref(),
                    file.side,
                )
            })
            .await;
        state.update(cx, |s: &mut DiffState, cx| {
            if !s.land_fetch(req, fetch, result) {
                return;
            }
            if let Fetch::AfterOp(Some(next)) = fetch {
                s.jump_to(next);
            }
            cx.notify();
        });
        // 被更新的请求顶掉了也照样放开:最坏是下一次点击拿到过期的块,后端会按陈旧拒绝
        if matches!(fetch, Fetch::AfterOp(_)) {
            ctl.update(cx, |c, cx| {
                c.busy = None;
                cx.notify();
            });
        }
    })
    .detach();
}

/// 块操作后的新序列里该停在哪(下标进 `new`):
///
/// - 当前文件还在组里 → 还是它(序列里别的文件进出了,下标跟着变);
/// - 离开了(取消暂存 / 丢弃了最后一块)→ 原位置上的下一个:旧序列里排在它后面、
///   还留在组里的第一个;后面一个都不剩就取前面最近的那个;
/// - 组空了 → `None`。
fn resync_index(old: &[DiffFile], current: usize, new: &[DiffFile]) -> Option<usize> {
    let find = |file: &DiffFile| new.iter().position(|f| f.path == file.path);
    old.get(current)
        .and_then(find)
        .or_else(|| old.iter().skip(current + 1).find_map(find))
        .or_else(|| old.iter().take(current).rev().find_map(find))
        // 旧序列里的一个都不在了(外部又改过一轮):按原位置落,越界取末尾
        .or_else(|| new.len().checked_sub(1).map(|last| current.min(last)))
}

/// 块操作后打开方重载出了新序列(见 [`ReloadFiles`]):换上它,按 [`resync_index`]
/// 决定停在哪。当前文件还在 → 原地重取(`hunk` 为 `Some` 时落到原位置上的下一块,`None`
/// 停在原地);换了文件 → 告诉打开方、从头取那个文件;组空了 → 留下当前文件原地重取,正文是
/// 「没有更多改动」的空态、换文件的按钮随之收起。`None`(没取到)沿用旧序列原地重取。
fn resync_files(
    state: &Entity<DiffState>,
    ctl: &Entity<FileDiff>,
    files: Option<Vec<DiffFile>>,
    hunk: Option<usize>,
    cx: &mut App,
) {
    let Some(files) = files else {
        load_file(state, ctl, Fetch::AfterOp(hunk), cx);
        return;
    };
    let switched = ctl.update(cx, |c, cx| {
        let path = c.file().path.clone();
        match resync_index(&c.files, c.current, &files) {
            Some(index) => {
                c.files = files;
                c.current = index;
            }
            None => {
                c.files = vec![c.file().clone()];
                c.current = 0;
            }
        }
        cx.notify();
        let file = c.file().clone();
        (file.path != path).then(|| {
            // 换了文件:不走 `load_file` 的「操作后」分支,块按钮在这里放开
            c.busy = None;
            (file, c.on_switch.clone())
        })
    });
    match switched {
        Some((file, on_switch)) => {
            if let Some(on_switch) = on_switch {
                on_switch(&file, cx);
            }
            load_file(state, ctl, Fetch::Fresh, cx);
        }
        None => load_file(state, ctl, Fetch::AfterOp(hunk), cx),
    }
}

/// 在文件序列里前后换一个(到头不绕回)。块操作进行中不换 —— 回包会落到别的文件上。
/// 换成了就告诉打开方(更改列表让选中项跟过去)。
fn switch_file(state: &Entity<DiffState>, ctl: &Entity<FileDiff>, delta: isize, cx: &mut App) {
    let moved = ctl.update(cx, |c, cx| {
        let next = c.current as isize + delta;
        if c.busy.is_some() || next < 0 || next as usize >= c.files.len() {
            return None;
        }
        c.current = next as usize;
        c.op_error = None;
        c.stale = false;
        cx.notify();
        Some((c.file().clone(), c.on_switch.clone()))
    });
    if let Some((file, on_switch)) = moved {
        if let Some(on_switch) = on_switch {
            on_switch(&file, cx);
        }
        load_file(state, ctl, Fetch::Fresh, cx);
    }
}

/// 单文件 diff 没有改动块时的空态文案。纯重命名(内容一字未动)单说一句 —— 写「没有
/// 更多改动」读着像刚把改动处理完了。
fn empty_text(file: &DiffFile) -> &'static str {
    if file.old_path.as_deref().is_some_and(|old| old != file.path) {
        t("gitCommit", "renameOnly")
    } else {
        t("gitCommit", "noMoreChanges")
    }
}

/// 当前侧的说明(「暂存区 → 工作区」这类)。
fn side_label(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Staged => t("gitCommit", "sideStaged"),
        DiffSide::Unstaged => t("gitCommit", "sideUnstaged"),
        DiffSide::Working => t("gitCommit", "sideWorking"),
    }
}

/// 提交 diff 左栏的宽度(放得下「文件名 + 弱化目录 + 状态字母」一行)。
const COMMIT_LIST_W: f32 = 280.0;

/// diff 工具栏左半的文件抬头:文件图标 + 文件名(14px 半粗)+ 弱化的目录 + 状态胶囊。
/// 各种 diff 页签共用(单文件 diff 在后面再接「+12 −3」与当前侧)。
fn file_heading(path: &str, status: Option<(&str, gpui::Hsla)>) -> gpui::Div {
    let (dir, name) = match path.rsplit_once(['/', '\\']) {
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), path.to_string()),
    };
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .flex_1()
        .min_w_0()
        .child(FileIcon::new(&name, false, false).size(px(16.0)))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(14.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(ui::text_primary())
                .child(name),
        )
        .when(!dir.is_empty(), |el| {
            el.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(dir),
            )
        })
        .when_some(
            status.filter(|(letter, _)| !letter.trim().is_empty()),
            |el, (letter, color)| el.child(pill(letter.to_string(), color)),
        )
}

/// 标题栏左半:[`file_heading`] +「+12 −3」+ 当前侧。
fn title_block(file: &DiffFile, state: &Entity<DiffState>, cx: &App) -> AnyElement {
    let s = state.read(cx);
    let stats = s
        .result
        .as_ref()
        .filter(|r| !r.is_binary && !r.too_large)
        .map(|_| (s.flat.adds, s.flat.deletes));
    let status_color = git_ui::status_color_for_label(&file.status_label);
    file_heading(&file.path, Some((file.status_label.as_str(), status_color)))
        .when_some(stats, |el, (adds, deletes)| {
            el.child(
                mono(div())
                    .flex_none()
                    .flex()
                    .gap(px(6.0))
                    .text_size(ui::font_px(12.0))
                    .child(
                        div()
                            .text_color(ui::color_success())
                            .child(format!("+{adds}")),
                    )
                    .child(
                        div()
                            .text_color(ui::color_error())
                            .child(format!("−{deletes}")),
                    ),
            )
        })
        .child(
            div()
                .flex_none()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(side_label(file.side)),
        )
        .into_any_element()
}

/// 「上一个文件 / 2 / 7 / 下一个文件」。只有一个文件时不画。
fn file_nav(
    state: &Entity<DiffState>,
    ctl: &Entity<FileDiff>,
    current: usize,
    total: usize,
) -> Option<AnyElement> {
    if total <= 1 {
        return None;
    }
    let nav = |id: &'static str, icon, tip: String, delta: isize, enabled: bool| {
        let (state, ctl) = (state.clone(), ctl.clone());
        icon_button(id, icon, tip, enabled).when(enabled, move |el| {
            el.on_click(move |_: &ClickEvent, _window, cx| {
                switch_file(&state, &ctl, delta, cx);
            })
        })
    };
    Some(
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .mr(px(16.0))
            .child(nav(
                "git-diff-prev-file",
                ICON_CHEVRON_LEFT,
                tr!(
                    "gitCommit",
                    "prevFile",
                    shortcut = crate::menu::hotkey_label(false, false, true, "←")
                ),
                -1,
                current > 0,
            ))
            .child(
                mono(div())
                    .px(px(2.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(format!("{} / {}", current + 1, total)),
            )
            .child(nav(
                "git-diff-next-file",
                ICON_CHEVRON_RIGHT,
                tr!(
                    "gitCommit",
                    "nextFile",
                    shortcut = crate::menu::hotkey_label(false, false, true, "→")
                ),
                1,
                current + 1 < total,
            ))
            .into_any_element(),
    )
}

/// 块操作失败的就地错误条:红字原文 + 复制 + (过期时)刷新 + 关掉。
fn error_bar(
    state: &Entity<DiffState>,
    ctl: &Entity<FileDiff>,
    err: String,
    stale: bool,
) -> AnyElement {
    let copy_text = err.clone();
    let (refresh_state, refresh_ctl) = (state.clone(), ctl.clone());
    let dismiss = ctl.clone();
    div()
        .flex_none()
        .flex()
        .items_start()
        .gap(px(8.0))
        .px(px(16.0))
        .py(px(4.0))
        .bg(ui::with_alpha(ui::color_error(), 0.08))
        .border_b_1()
        .border_color(ui::with_alpha(ui::color_error(), 0.35))
        .child(div().flex_none().pt(px(6.0)).child(git_ui::icon(
            git_ui::ICON_ALERT,
            git_ui::ICON_SIZE,
            ui::color_error(),
        )))
        .child(
            div()
                .id("git-diff-op-error")
                .py(px(4.0))
                .flex_1()
                .min_w_0()
                .max_h(px(120.0))
                .overflow_y_scroll()
                .text_size(ui::font_px(12.0))
                .text_color(ui::color_error())
                .child(err),
        )
        .child(
            icon_button(
                "git-diff-op-error-copy",
                ICON_COPY,
                t("gitCommit", "copyError"),
                true,
            )
            .on_click(move |_: &ClickEvent, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
            }),
        )
        .when(stale, |el| {
            el.child(
                icon_button(
                    "git-diff-op-error-refresh",
                    ICON_REFRESH,
                    t("gitCommit", "refresh"),
                    true,
                )
                .on_click(move |_: &ClickEvent, _window, cx| {
                    refresh_ctl.update(cx, |c, cx| {
                        c.op_error = None;
                        c.stale = false;
                        cx.notify();
                    });
                    let at = refresh_state.read(cx).jump.unwrap_or(0);
                    load_file(&refresh_state, &refresh_ctl, Fetch::AfterOp(Some(at)), cx);
                }),
            )
        })
        .child(
            icon_button(
                "git-diff-op-error-close",
                ICON_CLOSE,
                t("gitCommit", "dismiss"),
                true,
            )
            .on_click(move |_: &ClickEvent, _window, cx| {
                dismiss.update(cx, |c, cx| {
                    c.op_error = None;
                    c.stale = false;
                    cx.notify();
                });
            }),
        )
        .into_any_element()
}

// ─── 提交 diff(某次 commit / 一组提交合并后的多文件,原版 CommitDiffModal) ────

// 左栏文件列表的状态字母表(`CommitDiffModal.tsx:21-26`)收在
// `git_ui::commit_file_status`:提交详情与单文件历史用的是同一张表。

/// 提交 diff 页签左栏的一个文件。单提交与合并变更两种来源在这里拍成同一种形状,
/// 页签正文只有一份(见 [`pages`] 的 `FilesPage`)。
#[derive(Clone)]
struct ModalFile {
    path: String,
    status: String,
    /// 重命名的旧路径(tooltip 写 `old → new`;单提交取 diff 时要用)。
    old_path: Option<String>,
    source: ModalFileSource,
}

/// 右栏的 diff 从哪儿取。
#[derive(Clone)]
enum ModalFileSource {
    /// 单提交:该提交相对第一父([`mt_project::git::get_commit_file_diff`])。
    Commit(String),
    /// 合并变更:两个版本之间([`mt_project::git::get_revision_file_diff`])。
    Revisions {
        base: Option<FileRevision>,
        head: Option<FileRevision>,
    },
}

impl ModalFile {
    fn from_commit(hash: &str, file: CommitFileInfo) -> Self {
        Self {
            path: file.path,
            status: file.status,
            old_path: file.old_path,
            source: ModalFileSource::Commit(hash.to_string()),
        }
    }

    fn from_combined(file: CombinedFileChange) -> Self {
        Self {
            path: file.path,
            status: file.status,
            old_path: file.old_path,
            source: ModalFileSource::Revisions {
                base: file.base,
                head: file.head,
            },
        }
    }
}

/// 左栏头部:标题 + 一行等宽的副标题(单提交是短哈希,合并变更是「最新 … 最旧」)。
struct ModalHeading {
    title: String,
    subtitle: String,
    /// 副标题的 tooltip(合并变更写合并口径)。
    subtitle_tip: Option<&'static str>,
    /// 一个文件都没有时右栏的说明。
    empty: &'static str,
}

/// 左栏选中项与视图状态。放进实体:行回调、↑ / ↓ 拿着它的句柄改选中项。
struct CommitPick {
    /// 选中第几个文件。按下标认而不是按路径:合并变更在极端的非连续选择下可能出现
    /// 同一路径的两条(见 `mt_project::git::combined` 的模块注释)。
    selected: usize,
    /// 树形 / 平铺(打开时取配置 `gitCommitFilesViewMode`,点切换按钮时一并写回)。
    tree: bool,
    /// 树形里收起的目录(完整路径)。
    collapsed: HashSet<String>,
    /// 左栏拍平后的行(见 [`Self::rows`])。切视图、收起 / 展开目录时作废。
    rows: Option<Rc<Vec<ChangeTreeRow>>>,
}

impl CommitPick {
    /// 左栏的行。文件列表在页签的一生里不变,行只随视图与收起的目录变,所以缓存着:
    /// 页签每帧都重画(右栏滚动、拖选),大提交几千个文件、合并查看两百个提交时不能每帧
    /// 重新建一遍树。选中项不在行里(画的时候才比),换选中不必重算。
    fn rows(&mut self, files: &[ModalFile]) -> Rc<Vec<ChangeTreeRow>> {
        self.rows
            .get_or_insert_with(|| Rc::new(modal_rows(files, self.tree, &self.collapsed)))
            .clone()
    }

    fn set_tree(&mut self, tree: bool) {
        self.tree = tree;
        self.rows = None;
    }

    /// 收起 / 展开一个目录。
    fn toggle_dir(&mut self, full_path: &str) {
        if !self.collapsed.remove(full_path) {
            self.collapsed.insert(full_path.to_string());
        }
        self.rows = None;
    }
}

impl Render for CommitPick {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 单提交的左栏头部:标题是提交信息,副标题是短哈希。
fn commit_heading(hash: &str, message: String) -> ModalHeading {
    ModalHeading {
        title: message,
        subtitle: hash.chars().take(7).collect(),
        subtitle_tip: None,
        empty: t("commitDiff", "noChanges"),
    }
}

/// 合并变更的左栏头部:标题写明「N 个提交的合并变更」,副标题是最新 … 最旧两个短哈希
/// (`hashes` 按历史列表的顺序,新 → 旧)。
fn combined_heading(hashes: &[String]) -> ModalHeading {
    let short = |hash: Option<&String>| -> String {
        hash.map(|h| h.chars().take(7).collect())
            .unwrap_or_default()
    };
    ModalHeading {
        title: tr!(
            "commitDiff",
            "combinedTitle",
            count = hashes.len().to_string()
        ),
        subtitle: format!("{} … {}", short(hashes.first()), short(hashes.last())),
        subtitle_tip: Some(t("gitLog", "multi.rangeTip")),
        empty: t("gitLog", "multi.noFiles"),
    }
}

/// 左栏的行:树形按目录分组(共用的 [`git_ui::build_change_tree`]),平铺一个文件一行。
fn modal_rows(files: &[ModalFile], tree: bool, collapsed: &HashSet<String>) -> Vec<ChangeTreeRow> {
    let paths: Vec<(usize, &str)> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (i, f.path.as_str()))
        .collect();
    git_ui::change_rows(&paths, tree, &|p| collapsed.contains(p))
}

/// 第 `ix` 个文件在左栏的第几行(目录全展开时;树形里行序与文件序不同)。
fn file_row_of(files: &[ModalFile], tree: bool, ix: usize) -> Option<usize> {
    modal_rows(files, tree, &HashSet::new())
        .iter()
        .position(|r| matches!(r, ChangeTreeRow::File { index, .. } if *index == ix))
}

/// 左栏 ↑ / ↓ 换到哪个文件:眼下**看得见**的行里,选中那一行的上一个 / 下一个文件行(目录
/// 行跳过,收起的目录里的不算;到头停住、不绕回)。选中的文件藏在收起的目录里时 ↓ 落到
/// 第一个、↑ 落到最后一个(与「更改」列表的 ↑↓ 同一口径)。返回 `(文件下标, 行号)`;
/// 没得换(到头了、一个文件行都没有)是 `None`。
fn step_file(rows: &[ChangeTreeRow], selected: usize, delta: isize) -> Option<(usize, usize)> {
    let files: Vec<(usize, usize)> = rows
        .iter()
        .enumerate()
        .filter_map(|(row, r)| match r {
            ChangeTreeRow::File { index, .. } => Some((*index, row)),
            ChangeTreeRow::Dir { .. } => None,
        })
        .collect();
    let last = files.len().checked_sub(1)?;
    let current = files.iter().position(|&(index, _)| index == selected);
    let next = match current {
        None if delta > 0 => 0,
        None => last,
        Some(pos) => (pos as isize + delta).clamp(0, last as isize) as usize,
    };
    (Some(next) != current).then(|| files[next])
}

/// 左栏选中第 `index` 个文件,右栏换成它的 diff(单击行与 ↑ / ↓ 共用)。已经是它就什么都不做。
fn choose_modal_file(
    pick: &Entity<CommitPick>,
    state: &Entity<DiffState>,
    files: &[ModalFile],
    repo_path: &str,
    index: usize,
    cx: &mut App,
) {
    if pick.read(cx).selected == index {
        return;
    }
    pick.update(cx, |p, cx| {
        p.selected = index;
        cx.notify();
    });
    if let Some(file) = files.get(index) {
        load_modal_file(state, repo_path, file, Fetch::Fresh, cx);
    }
}

/// ↑ / ↓:左栏换到上一个 / 下一个文件(见 [`step_file`]),选中行滚到看得见。
fn step_modal_file(
    pick: &Entity<CommitPick>,
    state: &Entity<DiffState>,
    files: &[ModalFile],
    repo_path: &str,
    scroll: &UniformListScrollHandle,
    delta: isize,
    cx: &mut App,
) {
    let (rows, selected) = pick.update(cx, |p, _| (p.rows(files), p.selected));
    let Some((index, row)) = step_file(&rows, selected, delta) else {
        return;
    };
    scroll.scroll_to_item(row, ScrollStrategy::Nearest);
    choose_modal_file(pick, state, files, repo_path, index, cx);
}

/// 左栏文件行的 tooltip:完整路径,重命名写 `旧 → 新`。
fn modal_file_tip(file: &ModalFile) -> String {
    match &file.old_path {
        Some(old) => format!("{old} → {}", file.path),
        None => file.path.clone(),
    }
}

/// 左栏的一行:目录行点击收起 / 展开,文件行点击换右栏的 diff。只给可视区里的行建
/// (左栏是 uniform_list,行等高 [`git_ui::ROW_H`])。
fn modal_row(
    row: &ChangeTreeRow,
    files: &Rc<Vec<ModalFile>>,
    pick: &Entity<CommitPick>,
    state: &Entity<DiffState>,
    repo_path: &SharedString,
    selected: usize,
    tree: bool,
) -> Option<AnyElement> {
    match row {
        ChangeTreeRow::Dir {
            name,
            full_path,
            depth,
            collapsed,
            file_count,
        } => {
            let pick = pick.clone();
            let id = SharedString::from(format!("git-commit-dir-{full_path}"));
            let full_path = full_path.clone();
            let toggle = move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                pick.update(cx, |p, cx| {
                    p.toggle_dir(&full_path);
                    cx.notify();
                });
            };
            Some(
                git_ui::tree_dir_row(id, name.clone(), *file_count, *depth, *collapsed)
                    // 列表项按根布局排,不像块容器那样自动撑满:不补 w_full 悬停底只有字那么宽
                    .w_full()
                    .on_click(toggle)
                    .into_any_element(),
            )
        }
        &ChangeTreeRow::File { index, depth } => {
            let file = files.get(index)?;
            // 提示文字在气泡弹出时才拼:每帧给可视区每一行都先拼好没必要
            let tip = {
                let files = files.clone();
                move |window: &mut Window, cx: &mut App| {
                    let text = files.get(index).map(modal_file_tip).unwrap_or_default();
                    Tooltip::new(text).build(window, cx)
                }
            };
            let (pick, state, files) = (pick.clone(), state.clone(), files.clone());
            let repo = repo_path.clone();
            let choose = move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                choose_modal_file(&pick, &state, &files, &repo, index, cx);
            };
            Some(
                git_ui::commit_file_row_at(
                    SharedString::from(format!("git-commit-file-{index}")),
                    &file.path,
                    &file.status,
                    index == selected,
                    tree.then_some(depth),
                )
                .tip_with(tip)
                .on_click(choose)
                .into_any_element(),
            )
        }
    }
}

/// 取左栏一个文件的 diff(按它的来源:单提交 / 两个版本之间)。`fetch` 是 [`Fetch::Fresh`]
/// (换文件)或 [`Fetch::Refresh`](刷新按钮:原地重取,内容没变就不动)。
///
/// ⚠️ **单提交里重命名的文件必须带旧路径**(`CommitDiffModal.tsx:57`),否则父树里查
/// 不到旧内容,diff 会显示成「整文件新增」。
fn load_modal_file(
    state: &Entity<DiffState>,
    repo_path: &str,
    file: &ModalFile,
    fetch: Fetch,
    cx: &mut App,
) {
    match &file.source {
        ModalFileSource::Commit(hash) => {
            let old_path = file.old_path.clone();
            load_commit_file_at(state, repo_path, hash, &file.path, old_path, fetch, cx);
        }
        ModalFileSource::Revisions { base, head } => {
            let (repo, base, head) = (repo_path.to_string(), base.clone(), head.clone());
            load_diff_with(state, fetch, cx, move || {
                mt_project::git::get_revision_file_diff(
                    std::path::Path::new(&repo),
                    base.as_ref(),
                    head.as_ref(),
                )
            });
        }
    }
}

/// 某个提交里一个文件的 diff:旧路径由调用方给(单文件历史没有整份文件列表)。
fn load_commit_file_at(
    state: &Entity<DiffState>,
    repo_path: &str,
    commit_hash: &str,
    path: &str,
    old_path: Option<String>,
    fetch: Fetch,
    cx: &mut App,
) {
    let (repo, hash, path) = (
        repo_path.to_string(),
        commit_hash.to_string(),
        path.to_string(),
    );
    load_diff_with(state, fetch, cx, move || {
        mt_project::git::get_commit_file_diff(
            std::path::Path::new(&repo),
            &hash,
            &path,
            old_path.as_deref(),
        )
    });
}

/// 取一份 diff:按 [`Fetch`] 复位、令牌 +1,`load` 丢后台跑,回来时令牌对得上才落地
/// (见 [`DiffState::begin_fetch`] / [`DiffState::land_fetch`])。
fn load_diff_with(
    state: &Entity<DiffState>,
    fetch: Fetch,
    cx: &mut App,
    load: impl FnOnce() -> anyhow::Result<GitDiffResult> + Send + 'static,
) {
    let req = state.update(cx, |s, cx| {
        if !s.begin_fetch(fetch) {
            return None;
        }
        cx.notify();
        Some(s.request)
    });
    let Some(req) = req else {
        return;
    };

    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = cx.background_executor().spawn(async move { load() }).await;
        state.update(cx, |s: &mut DiffState, cx| {
            if s.land_fetch(req, fetch, result) {
                cx.notify();
            }
        });
    })
    .detach();
}

// ─── 嵌入式单文件提交 diff(单文件历史页签 / 贮藏弹窗用) ──────

/// 「某个提交里的某一个文件」的 diff 面板,给别的视图嵌进去用(单文件历史页签的右栏、
/// 贮藏弹窗的右栏)。
///
/// 只是把提交 diff 右栏那一套(状态 + 取数 + 正文 + 右上角工具组)包了一层,行为与那边
/// 一致;文案沿用 `commitDiff` 命名空间。
#[derive(Clone)]
pub(crate) struct CommitFileDiffPane {
    state: Entity<DiffState>,
    /// 当前这份是不是改了名的文件(各克隆共用):纯重命名没有改动块,正文要说一句
    renamed: Rc<std::cell::Cell<bool>>,
}

impl CommitFileDiffPane {
    pub(crate) fn new(store: &Entity<AppStore>, cx: &mut App) -> Self {
        let font_size = store.read(cx).config().terminal_font_size as f32;
        Self {
            state: cx.new(|_| DiffState::new(font_size)),
            renamed: Rc::default(),
        }
    }

    /// 换成另一个(提交, 文件)。重命名过的文件要给 `old_path`,否则父树里查不到旧内容。
    pub(crate) fn load(
        &self,
        repo_path: &str,
        commit_hash: &str,
        path: &str,
        old_path: Option<String>,
        cx: &mut App,
    ) {
        self.renamed
            .set(old_path.as_deref().is_some_and(|old| old != path));
        load_commit_file_at(
            &self.state,
            repo_path,
            commit_hash,
            path,
            old_path,
            Fetch::Fresh,
            cx,
        );
    }

    /// 正文(加载中 / 出错 / 二进制 / 过大 / 正常 五选一)。纯重命名与提交 diff 页签
    /// 同一句说明,别留一片空白。
    pub(crate) fn body(&self, cx: &mut App) -> AnyElement {
        let rename_only = self.renamed.get().then(|| t("gitCommit", "renameOnly"));
        render_body(
            &self.state,
            t("commitDiff", "loading"),
            t("commitDiff", "binaryFile"),
            t("commitDiff", "tooLarge"),
            rename_only,
            None,
            cx,
        )
    }

    /// 给宿主的焦点容器挂上正文的 Ctrl/Cmd+C、Ctrl/Cmd+A(键位按宿主的 key context
    /// 绑在 `hotkeys.rs`)。
    pub(crate) fn text_actions(&self, el: gpui::Div) -> gpui::Div {
        selection::text_actions(el, &self.state)
    }

    /// 右上角:改动跳转 + 并排 / 行内切换 + 最右端那一颗(弹窗里是关掉它的 ✕,页签里是刷新,
    /// 见 [`ToolbarTail`])。
    pub(crate) fn toolbar(
        &self,
        id_prefix: &'static str,
        tail: ToolbarTail,
        cx: &mut App,
    ) -> AnyElement {
        view_toggle(
            &self.state,
            id_prefix,
            ToolbarLabels {
                side: t("commitDiff", "sideBySide"),
                inline: t("commitDiff", "inline"),
                prev: t("commitDiff", "prevChange"),
                next: t("commitDiff", "nextChange"),
            },
            tail,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use mt_project::git::DiffLine;

    use super::*;

    fn hunk(lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 0,
            lines,
        }
    }

    fn staged(paths: &[&str]) -> Vec<DiffFile> {
        paths
            .iter()
            .map(|p| DiffFile {
                path: p.to_string(),
                old_path: None,
                side: DiffSide::Staged,
                status_label: "M".into(),
                hunk_actions: true,
            })
            .collect()
    }

    /// 块操作后换序列:当前文件还在就停在它(下标跟着别的文件进出变);离开了就落到原
    /// 位置上的下一个,后面没有了取前面最近的;组空了是 `None`。
    #[test]
    fn 块操作后序列跟着更改列表走() {
        let old = staged(&["a", "app.ts", "c", "d", "e"]);
        // 还在:前面的 a 走了,app.ts 从第 2 个变成第 1 个
        assert_eq!(
            resync_index(&old, 1, &staged(&["app.ts", "c", "d", "e"])),
            Some(0)
        );
        // app.ts 离开了已暂存组:停在原位置上的下一个 c(新序列里第 2 个)
        assert_eq!(
            resync_index(&old, 1, &staged(&["a", "c", "d", "e"])),
            Some(1)
        );
        // 原位置后面的也一起走了:跳过不在组里的,落到 e
        assert_eq!(resync_index(&old, 1, &staged(&["a", "e"])), Some(1));
        // 末尾那个离开:后面没有了,取前面最近的 d
        assert_eq!(
            resync_index(&old, 4, &staged(&["a", "app.ts", "c", "d"])),
            Some(3)
        );
        // 旧序列里一个都不剩(外部又改过一轮):按原位置落,越界取末尾
        assert_eq!(resync_index(&old, 3, &staged(&["x", "y"])), Some(1));
        // 组空了
        assert_eq!(resync_index(&old, 1, &[]), None);
    }

    /// 提交 diff 左栏:先选中的文件要滚到它在树里那一行(目录行在前、单链压缩);
    /// 平铺时就是文件序;合并变更的文件按自己的路径进树(重命名的挂在新路径下)。
    #[test]
    fn 左栏初始选中文件所在的行() {
        let commit = |path: &str| {
            ModalFile::from_commit(
                "abc",
                CommitFileInfo {
                    path: path.into(),
                    status: "modified".into(),
                    old_path: None,
                },
            )
        };
        let files = [commit("z.md"), commit("src/ui/a.rs"), commit("src/ui/b.rs")];
        // 树形:src/ui(压缩成一行)、a.rs、b.rs、z.md
        assert_eq!(file_row_of(&files, true, 0), Some(3));
        assert_eq!(file_row_of(&files, true, 2), Some(2));
        assert_eq!(file_row_of(&files, false, 0), Some(0));
        assert_eq!(file_row_of(&files, true, 9), None);

        let renamed = ModalFile::from_combined(CombinedFileChange {
            path: "new/dir/x.rs".into(),
            status: "renamed".into(),
            old_path: Some("old/x.rs".into()),
            base: None,
            head: None,
        });
        assert_eq!(renamed.old_path.as_deref(), Some("old/x.rs"));
        let rows = modal_rows(std::slice::from_ref(&renamed), true, &HashSet::new());
        assert!(
            matches!(&rows[0], ChangeTreeRow::Dir { name, .. } if name == "new/dir"),
            "{rows:?}"
        );
        assert_eq!(modal_file_tip(&renamed), "old/x.rs → new/dir/x.rs");
        assert_eq!(modal_file_tip(&files[1]), "src/ui/a.rs");
    }

    /// 左栏的行缓存着:与每帧现算的一致;换选中不动它,收起目录 / 切视图之后作废重算。
    #[test]
    fn 左栏行缓存随折叠与视图作废() {
        let commit = |path: &str| {
            ModalFile::from_commit(
                "abc",
                CommitFileInfo {
                    path: path.into(),
                    status: "modified".into(),
                    old_path: None,
                },
            )
        };
        let files: Vec<ModalFile> = (0..50)
            .map(|i| commit(&format!("src/m{}/f{i}.rs", i % 4)))
            .collect();
        let mut pick = CommitPick {
            selected: 0,
            tree: true,
            collapsed: HashSet::new(),
            rows: None,
        };
        let first = pick.rows(&files);
        assert_eq!(*first, modal_rows(&files, true, &HashSet::new()));
        pick.selected = 7;
        assert!(Rc::ptr_eq(&first, &pick.rows(&files)), "换选中不重算");

        pick.toggle_dir("src/m2");
        let folded = pick.rows(&files);
        assert!(!Rc::ptr_eq(&first, &folded));
        assert_eq!(*folded, modal_rows(&files, true, &pick.collapsed));
        assert!(folded.len() < first.len());
        pick.toggle_dir("src/m2");
        assert_eq!(*pick.rows(&files), *first, "再点一下展开回来");

        pick.set_tree(false);
        assert_eq!(
            *pick.rows(&files),
            modal_rows(&files, false, &pick.collapsed)
        );
        assert_eq!(pick.rows(&files).len(), files.len());
    }

    /// 左栏 ↑ / ↓ 换文件:按眼下看得见的行序走,目录行跳过、收起的目录里的不算,到头停住;
    /// 选中的文件藏在收起的目录里时 ↓ 落到第一个、↑ 落到最后一个。返回(文件下标, 行号)。
    #[test]
    fn 左栏上下键按可见行序换文件() {
        let commit = |path: &str| {
            ModalFile::from_commit(
                "abc",
                CommitFileInfo {
                    path: path.into(),
                    status: "modified".into(),
                    old_path: None,
                },
            )
        };
        let files = [
            commit("z.md"),
            commit("src/ui/a.rs"),
            commit("src/ui/b.rs"),
            commit("docs/c.md"),
        ];
        // 树形:docs(0)、c.md(1)、src/ui(2)、a.rs(3)、b.rs(4)、z.md(5)
        let tree = modal_rows(&files, true, &HashSet::new());
        assert_eq!(step_file(&tree, 3, 1), Some((1, 3)), "跳过 src/ui 目录行");
        assert_eq!(step_file(&tree, 1, -1), Some((3, 1)), "往回也跳过目录行");
        assert_eq!(step_file(&tree, 2, 1), Some((0, 5)));
        assert_eq!(step_file(&tree, 0, 1), None, "最后一个:到头不绕回");
        assert_eq!(step_file(&tree, 3, -1), None, "第一个:到头不绕回");

        // 收起 src/ui:里面的 a.rs / b.rs 不算
        let folded = modal_rows(&files, true, &HashSet::from(["src/ui".to_string()]));
        assert_eq!(step_file(&folded, 3, 1), Some((0, 3)));
        assert_eq!(step_file(&folded, 0, -1), Some((3, 1)));
        // 选中的文件就在收起的目录里:↓ 落到第一个,↑ 落到最后一个
        assert_eq!(step_file(&folded, 1, 1), Some((3, 1)));
        assert_eq!(step_file(&folded, 1, -1), Some((0, 3)));

        // 平铺:就是文件序
        let flat = modal_rows(&files, false, &HashSet::new());
        assert_eq!(step_file(&flat, 0, 1), Some((1, 1)));
        assert_eq!(step_file(&flat, 3, -1), Some((2, 2)));
        // 一个文件行都没有(全收起 / 空提交)
        let all_folded = modal_rows(
            &files,
            true,
            &HashSet::from(["src/ui".to_string(), "docs".to_string()]),
        );
        assert_eq!(step_file(&all_folded[..2], 1, 1), None);
        assert_eq!(step_file(&[], 0, 1), None);
    }

    /// 没有改动块时的空态:纯重命名单说一句,其余照旧「没有更多改动」。
    #[test]
    fn 纯重命名的空态文案() {
        let mut file = staged(&["new.ts"]).remove(0);
        assert_eq!(empty_text(&file), t("gitCommit", "noMoreChanges"));
        file.old_path = Some("old.ts".into());
        assert_eq!(empty_text(&file), t("gitCommit", "renameOnly"));
        // 重命名前后同名(不该出现,防御):不当重命名说
        file.old_path = Some("new.ts".into());
        assert_eq!(empty_text(&file), t("gitCommit", "noMoreChanges"));
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
        // 并排视图没有头行,两栏各自连续排:左栏 a b d,右栏 a c d;跳转落点是各块第一行的
        // 滚动位置
        assert_eq!(f.split.rows[LEFT].len(), 3);
        assert_eq!(f.split.rows[RIGHT].len(), 3);
        assert_eq!((f.split.hunk_pos(0), f.split.hunk_pos(1)), (0.0, 2.0));
        assert!(matches!(f.inline_rows[0], InlineRow::Head(0)));
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

    /// 词级高亮按整块算:中间插了一行时,「let c = 3;」不再配给插进来的那行、点出 c/b
    /// 两处假改动,只点出真正改了的「3」;插进来的那行整行一个底色。
    #[test]
    fn 拍平_词级高亮按块不因插行错位() {
        let h = hunk(vec![
            line_with("delete", "let a = 1;"),
            line_with("delete", "let c = 3;"),
            line_with("add", "let a = 10;"),
            line_with("add", "let b = 2;"),
            line_with("add", "let c = 30;"),
        ]);
        let f = flatten(&[h]);
        assert_eq!(f.marks[0], vec![8..9]);
        assert_eq!(f.marks[1], vec![8..9]);
        // 逐对配对的老算法在这里会点出两处
        let (old, _) = intra_line_marks("let c = 3;", "let b = 2;").unwrap();
        assert_eq!(old.len(), 2);
    }

    /// 超大块退回逐对配对:照样点出每对里改了的那个数,不会整块放弃。
    #[test]
    fn 拍平_超大块退回逐对配对() {
        let n = 300;
        let mut lines: Vec<DiffLine> = (0..n)
            .map(|i| line_with("delete", &format!("value = {i};")))
            .collect();
        lines.extend((0..n).map(|i| line_with("add", &format!("value = {i}x;"))));
        let f = flatten(&[hunk(lines)]);
        // 「value = 0;」→「value = 0x;」:旧侧点出 `0`,新侧点出 `0x`
        assert_eq!(f.marks[0], vec![8..9]);
        assert_eq!(f.marks[n], vec![8..10]);
    }

    /// uniform_list 只量一行来定内容宽,量宽下标必须指向**最宽**那一行。
    #[test]
    fn 拍平_量宽指向最宽的行() {
        let h = hunk(vec![
            line_with("context", "short"),
            line_with("context", "a much much longer line"),
        ]);
        let f = flatten(&[h]);
        // 行 0 是 hunk 头(钉在左缘,不参与量宽),行 1/2 是两条 context —— 最宽的是第 2 行
        assert_eq!(f.widest_inline, 2);
        assert!(matches!(f.inline_rows[2], InlineRow::Line(1)));
        // 并排两栏没有头行:最宽的是各栏第 1 行
        assert_eq!(f.split.widest, [1, 1]);
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

    /// hunk 头按 git 的写法:某一侧一行都没有时,起点写「插入点之前那一行」(文件开头
    /// 为 0)—— `@@ -0,0 +1,2 @@`、`@@ -4,0 +5,2 @@`;两侧都有行时照旧。
    #[test]
    fn hunk_头空范围按_git_写法() {
        let head = |old_start, old_lines, new_start, new_lines| {
            hunk_head(&DiffHunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
                lines: Vec::new(),
            })
        };
        // 新文件:后端记的是「插在旧侧第 1 行之前」
        let new_file = head(1, 0, 1, 2);
        assert_eq!(new_file.inline.as_ref(), "@@ -0,0 +1,2 @@");
        // 第 4 行后插两行(无上下文)、删光文件
        assert_eq!(head(5, 0, 5, 2).inline.as_ref(), "@@ -4,0 +5,2 @@");
        assert_eq!(head(1, 3, 1, 0).inline.as_ref(), "@@ -1,3 +0,0 @@");
        // 普通块不动
        assert_eq!(head(6, 7, 6, 8).inline.as_ref(), "@@ -6,7 +6,8 @@");
    }

    /// 跳转组的计数:跳过之后「第几处 / 共几处」,还没跳过只报总数(不再是「–/N」)。
    #[test]
    fn 跳转计数没跳过时只报总数() {
        assert_eq!(counter_text(Some(1), 3), "2/3");
        let total = counter_text(None, 3);
        assert!(total.contains('3') && !total.contains('/'), "{total}");
    }

    // ── 折叠的上下文 / 按行勾选 ─────────────────────────────

    use mt_project::git::{HunkDirection, diff_two_texts, splice_hunks};

    fn numbered(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("l{i}")).collect()
    }

    fn text(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    /// 100 行的文件改了第 10、80 行:两块 + 三段折叠(首块之前 6 行、块间 63 行、末块之后 17 行)。
    fn two_far_changes() -> GitDiffResult {
        let old = numbered(100);
        let mut new = old.clone();
        new[9] = "TEN".into();
        new[79] = "EIGHTY".into();
        diff_two_texts(text(&old), text(&new))
    }

    fn fold_lens(f: &Flat) -> Vec<u32> {
        f.folds.iter().map(|fold| fold.len).collect()
    }

    /// 拍平时块与块之间没改动的行折成分隔行;跳转落点只落在 hunk 头上;分隔行两个视图各一行、
    /// 记着自己在两个视图里的行号;首段只能向上展开、末段只能向下。
    #[test]
    fn 折叠_行模型与跳转() {
        let diff = two_far_changes();
        let (f, kept) = flatten_diff(&diff, &fold::Revealed::default());
        assert!(kept.is_empty());
        assert_eq!(fold_lens(&f), vec![6, 63, 17]);
        assert_eq!(f.inline_rows[0], InlineRow::Fold(0));
        assert_eq!(f.inline_rows[1], InlineRow::Head(0));
        assert_eq!(f.split.rows[LEFT][0], SideRow::Fold(0));
        for &j in &f.inline_jumps {
            assert!(matches!(f.inline_rows[j], InlineRow::Head(_)));
        }
        // 并排视图的落点:块一在分隔行之后(滚动位置 1),块二在块一的 7 行与块间分隔行之后
        assert_eq!((f.split.hunk_pos(0), f.split.hunk_pos(1)), (1.0, 9.0));
        for (ix, fold) in f.folds.iter().enumerate() {
            assert_eq!(f.inline_rows[fold.inline_row], InlineRow::Fold(ix));
            assert_eq!(f.split.rows[LEFT][fold.old_row], SideRow::Fold(ix));
            assert_eq!(f.split.rows[RIGHT][fold.new_row], SideRow::Fold(ix));
        }
        assert_eq!((f.folds[0].above, f.folds[0].below), (false, true));
        assert_eq!((f.folds[1].above, f.folds[1].below), (true, true));
        assert_eq!((f.folds[2].above, f.folds[2].below), (true, false));
        // 折起来的行不进 lines:只有块里的行
        assert_eq!(f.lines.len(), f.hunk_lines);
        // 没有两侧全文的老口径(`flatten`)不折叠
        assert!(flatten(&diff.hunks).folds.is_empty());
    }

    /// 向下展开露出紧挨上一块的 20 行、向上展开露出紧挨下一块的 20 行,剩下的仍折着;
    /// 全部展开之后分隔行消失,露出来的行号连续、两栏同一行。
    #[test]
    fn 折叠_分段展开与全部展开() {
        let diff = two_far_changes();
        let (mut f, mut revealed) = flatten_diff(&diff, &fold::Revealed::default());
        let extra = f.inline_rows.len() - f.split.rows[RIGHT].len();
        let mid = f.folds[1];
        assert_eq!((mid.start, mid.len), (0, 63));

        let target = f
            .expand_fold(&diff.old_content, &mut revealed, 1, 0, fold::Expand::Down)
            .expect("有这条分隔行");
        assert_eq!(target.len, 63);
        assert_eq!(fold_lens(&f), vec![6, 43, 17]);
        // 块一占 7..=13:露出来的是 14..=33,紧跟在块一后面
        let first = f.hunk_lines;
        assert_eq!(f.lines[first].old_lineno, Some(14));
        assert_eq!(f.lines.last().unwrap().old_lineno, Some(33));
        assert_eq!(f.lines.len() - f.hunk_lines, 20);

        let mid = f.folds[1];
        assert_eq!((mid.start, mid.len), (20, 43));
        f.expand_fold(&diff.old_content, &mut revealed, 1, 20, fold::Expand::Up);
        assert_eq!(fold_lens(&f), vec![6, 23, 17]);
        // 向上展开的 20 行紧挨着块二(77..=83):57..=76
        let block_two = f.inline_jumps[1];
        let InlineRow::Line(ix) = f.inline_rows[block_two - 1] else {
            panic!("块二的头上面应是露出来的行");
        };
        assert_eq!(f.lines[ix].old_lineno, Some(76));

        f.expand_fold(&diff.old_content, &mut revealed, 1, 20, fold::Expand::All);
        assert_eq!(fold_lens(&f), vec![6, 17]);
        // 块一与块二之间的行连续(14..=76 全露出来了)、两栏同一行
        let shown = |side: usize| -> Vec<u32> {
            f.split.rows[side]
                .iter()
                .filter_map(|row| match row {
                    SideRow::Line(ix) if *ix >= f.hunk_lines => f.lines[*ix].old_lineno,
                    _ => None,
                })
                .collect()
        };
        assert_eq!(shown(LEFT), (14..=76).collect::<Vec<u32>>());
        assert_eq!(shown(RIGHT), shown(LEFT));
        // 行内视图比并排右栏多出来的只是 hunk 头与删除行:展开只加上下文行,两个视图加得
        // 一样多,差值不变
        assert_eq!(f.inline_rows.len() - f.split.rows[RIGHT].len(), extra);
        // 已经展开掉的分隔行再点(过期的回调):什么都不动
        assert!(
            f.expand_fold(&diff.old_content, &mut revealed, 1, 0, fold::Expand::All)
                .is_none()
        );
    }

    /// 展开出来的行只是上下文:没有块归属、不能打勾、整块操作也不把它算进去;F7 落点不变。
    #[test]
    fn 折叠_展开行不参与块操作() {
        let diff = two_far_changes();
        let (mut f, mut revealed) = flatten_diff(&diff, &fold::Revealed::default());
        let jumps_before = f.heads.len();
        for (gap, start) in [(0, 0), (1, 0), (2, 0)] {
            f.expand_fold(
                &diff.old_content,
                &mut revealed,
                gap,
                start,
                fold::Expand::All,
            );
        }
        assert!(f.folds.is_empty(), "全展开了");
        assert_eq!(f.inline_jumps.len(), jumps_before);
        for ix in f.hunk_lines..f.lines.len() {
            assert_eq!(f.lines[ix].kind, "context");
            assert_eq!(picks::hunk_of(&f.hunk_base, ix), None);
        }
        for hunk in 0..f.heads.len() {
            assert!(f.pickable_in(hunk).iter().all(|&ix| ix < f.hunk_lines));
        }
        let whole = f.applied_lines(&[HunkPick::whole(0), HunkPick::whole(1)]);
        assert_eq!(whole.len(), 4, "两块各一删一增");
        assert!(whole.iter().all(|&ix| ix < f.hunk_lines));
        // 勾选里混进块外的下标(不该出现):分组时跳过,不会交给后端
        let stray = BTreeSet::from([f.hunk_lines + 3]);
        assert!(picks::group_picks(&f.hunk_base, &stray, None).is_empty());
    }

    /// 量宽算上展开出来的行:折着时最宽的是块里的行,展开后换成那条超长的上下文行。
    #[test]
    fn 折叠_量宽算上展开的行() {
        let mut old = numbered(40);
        old[24] = "x".repeat(300);
        let mut new = old.clone();
        new[4] = "FIVE".into();
        let diff = diff_two_texts(text(&old), text(&new));
        let (mut f, mut revealed) = flatten_diff(&diff, &fold::Revealed::default());
        let cols = |f: &Flat, row: InlineRow| match row {
            InlineRow::Line(ix) => text_cols(&f.lines[ix].content),
            _ => 0,
        };
        assert!(cols(&f, f.inline_rows[f.widest_inline]) < 300);
        let tail = f.folds.last().copied().unwrap();
        f.expand_fold(
            &diff.old_content,
            &mut revealed,
            tail.gap,
            tail.start,
            fold::Expand::All,
        );
        assert_eq!(cols(&f, f.inline_rows[f.widest_inline]), 300);
        for side in [LEFT, RIGHT] {
            let SideRow::Line(ix) = f.split.rows[side][f.split.widest[side]] else {
                panic!("量宽要落在正文行上");
            };
            assert_eq!(text_cols(&f.lines[ix].content), 300);
        }
    }

    /// 原地重取后展开状态按行号 + 内容认回来:暂存了前面那块(旧侧行号整体挪了)照样按新侧
    /// 行号认出;丢弃了那块(新侧挪了)按旧侧认出;认不出的收回去。
    #[test]
    fn 折叠_重取后按行号重新对齐() {
        let old = numbered(60);
        let mut new = old.clone();
        new.insert(5, "a".into());
        new.insert(6, "b".into());
        new[51] = "FIFTY".into(); // 旧侧第 50 行
        let diff = diff_two_texts(text(&old), text(&new));
        let (mut f, mut revealed) = flatten_diff(&diff, &fold::Revealed::default());
        // 块间那段:旧侧 9..、新侧 11..;向下露 20 行 = 旧侧 l9..l28
        let mid = f.folds.iter().copied().find(|fold| fold.gap == 1).unwrap();
        f.expand_fold(
            &diff.old_content,
            &mut revealed,
            1,
            mid.start,
            fold::Expand::Down,
        );
        let shown = |f: &Flat| -> Vec<SharedString> {
            (f.hunk_lines..f.lines.len())
                .map(|ix| f.lines[ix].content.clone())
                .collect()
        };
        let expect: Vec<SharedString> = (9..=28).map(|i| format!("l{i}").into()).collect();
        assert_eq!(shown(&f), expect);

        // 暂存了插入那块:暂存区多了 a、b,旧侧行号 +2,新侧不变
        let staged = {
            let mut lines = old.clone();
            lines.insert(5, "a".into());
            lines.insert(6, "b".into());
            text(&lines)
        };
        let after_stage = diff_two_texts(staged, text(&new));
        let (f2, kept) = flatten_diff(&after_stage, &revealed);
        assert_eq!(shown(&f2), expect, "按新侧行号认回来");
        assert!(!kept.is_empty());

        // 丢弃了插入那块:工作区没了 a、b,新侧行号 -2,旧侧不变
        let discarded = {
            let mut lines = old.clone();
            lines[49] = "FIFTY".into();
            text(&lines)
        };
        let after_discard = diff_two_texts(text(&old), discarded.clone());
        let (f3, _) = flatten_diff(&after_discard, &revealed);
        assert_eq!(shown(&f3), expect, "按旧侧行号认回来");

        // 行号没动、内容变了(外部改动把 l10..l29 两侧都改成了 m10..m29):对不上的收回去,
        // 只剩内容还对得上的 l9;重新认过的记忆里也只剩它
        let rewrite = |s: String| s.replace("\nl1", "\nm1").replace("\nl2", "\nm2");
        let elsewhere = diff_two_texts(rewrite(text(&old)), rewrite(discarded));
        let (f4, kept4) = flatten_diff(&elsewhere, &revealed);
        assert_eq!(shown(&f4), vec![SharedString::from("l9")]);
        assert!(kept4.contains(9, 9, "l9"));
        assert!(!kept4.contains(10, 10, "l10"));
    }

    /// 按行暂存的整条链路:勾了两块里各一行,只暂存第一块里勾的那行(块头按钮的口径)→ 后端
    /// 拼接 → 重取;第二块里的勾选按内容认回来,落下去的那行不再勾着。
    #[test]
    fn 按行勾选_暂存后剩下的勾选认回来() {
        let old = numbered(30);
        let mut new = old.clone();
        new[4] = "X5".into();
        new[5] = "X6".into();
        new[19] = "X20".into();
        let diff = diff_two_texts(text(&old), text(&new));
        let (f, _) = flatten_diff(&diff, &fold::Revealed::default());
        let find = |f: &Flat, kind: &str, content: &str| {
            (0..f.hunk_lines)
                .find(|&ix| f.lines[ix].kind == kind && f.lines[ix].content.as_ref() == content)
                .unwrap()
        };
        let (x6, x20) = (find(&f, "add", "X6"), find(&f, "add", "X20"));
        let mut picked = BTreeSet::from([x6, x20]);

        // 第一块的按钮只拿第一块里勾的那一行
        let first = picks::group_picks(&f.hunk_base, &picked, Some(0));
        assert_eq!(first.len(), 1);
        let pick = &first[0];
        let lines = pick.lines.as_ref().unwrap();
        assert_eq!(lines.len(), 1);
        let staged = splice_hunks(
            &diff.old_content,
            &diff.new_content,
            &[(&diff.hunks[0], Some(lines))],
            HunkDirection::Apply,
            "\n",
        )
        .unwrap();
        assert!(
            staged.contains("l5\nl6\nX6\nl7"),
            "只搬进勾上的 X6:{staged}"
        );

        // 成功后:落下去的行拿掉、记进 skip,再按内容认回剩下的
        let applied = f.applied_lines(&first);
        picked.retain(|ix| !applied.contains(ix));
        let keys = picks::pick_keys(&f.pick_lines(), &picked, &applied);
        let after = diff_two_texts(staged, text(&new));
        let (f2, _) = flatten_diff(&after, &fold::Revealed::default());
        let remapped = picks::remap_picks(&f2.pick_lines(), &keys);
        assert_eq!(remapped, BTreeSet::from([find(&f2, "add", "X20")]));
        // 第一块剩下的增删行(没勾的 X5、两行删除)都还在,只是没勾
        assert!((0..f2.hunk_lines).any(|ix| f2.lines[ix].content.as_ref() == "X5"));
    }

    /// 勾选范围:行内视图块头的勾选框管整块的增删行,并排视图中间栏的勾选框只管这一处改动的;
    /// 删除行的勾选框在左栏、新增行的在右栏(各在自己那一栏的行上)。
    #[test]
    fn 勾选范围_块头与每处改动() {
        let h = hunk(vec![
            line_with("context", "a"),
            line_with("delete", "b"),
            line_with("delete", "c"),
            line_with("add", "B"),
            line_with("context", "d"),
            line_with("add", "E"),
        ]);
        let f = flatten(&[h]);
        assert_eq!(f.pickable_in(0), vec![1, 2, 3, 5]);
        assert!(f.pickable_in(5).is_empty(), "块号越界");
        assert_eq!(f.change_lines(0), vec![1, 2, 3]);
        assert_eq!(f.change_lines(1), vec![5]);
        assert!(f.change_lines(9).is_empty());
        let on = |side: usize, ix: usize| f.split.rows[side].contains(&SideRow::Line(ix));
        assert!(on(LEFT, 1) && on(LEFT, 2) && !on(RIGHT, 1));
        assert!(on(RIGHT, 3) && on(RIGHT, 5) && !on(LEFT, 3));
        assert_eq!(Tick::of(0, 3), Tick::Off);
        assert_eq!(Tick::of(2, 3), Tick::Partial);
        assert_eq!(Tick::of(3, 3), Tick::On);
        // 按行操作的块内下标换回拍平下标
        assert_eq!(
            f.applied_lines(&[HunkPick::lines(0, [2usize].into())]),
            BTreeSet::from([2])
        );
    }

    /// 中间栏的箭头作用于这一处改动:没勾行时交给后端的是这一处的全部增删行(同一 hunk 里的
    /// 另一处不动),勾了行时只给勾上的;确认框据此说「这一处」还是「勾选的行」。
    #[test]
    fn 每处改动的块操作只动这一处() {
        let diff = diff_two_texts("a\nb\nc\nd\ne\n".into(), "a\nB\nc\nd\nE\nF\n".into());
        assert_eq!(diff.hunks.len(), 1, "两处改动挨得近,同在一个 hunk 里");
        let mut s = DiffState::new(13.0);
        s.pickable = true;
        s.apply(Ok(diff));
        assert_eq!(s.flat.split.changes.len(), 2);
        let second = s.op_target(OpScope::Change(1)).expect("有这一处");
        assert_eq!(
            (second.land, second.lines, second.change),
            (None, Some(3), true),
            "块里还有另一处:操作后停在原地"
        );
        assert_eq!(second.busy, Busy::Change(1));
        let lines = second.picks[0].lines.as_ref().expect("按行交给后端");
        let expect: Vec<usize> = s
            .flat
            .change_lines(1)
            .iter()
            .map(|ix| ix - s.flat.hunk_base[0])
            .collect();
        assert_eq!(lines.iter().copied().collect::<Vec<_>>(), expect);
        // 这一处里勾了一行:只动那一行
        let picked = s.flat.change_lines(1)[0];
        s.toggle_pick(picked);
        let only = s.op_target(OpScope::Change(1)).unwrap();
        assert_eq!((only.lines, only.change), (Some(1), false));
        assert_eq!(s.picks_in_change(1), 1);
        assert_eq!(s.picks_in_change(0), 0);
        // 中间栏的勾选框:一下勾满这一处,再点一下全取消
        s.toggle_change_picks(0);
        assert_eq!(s.picks_in_change(0), 2);
        s.toggle_change_picks(0);
        assert_eq!(s.picks_in_change(0), 0);
        assert_eq!(s.picks_in_change(1), 1, "另一处的勾选不受影响");
    }

    /// 按钮回调记的改动 / 块的身份:同一份 diff 里认得出;重取之后同一个序号指着别处时对不上
    /// (这次点击作罢),原来那一处换了序号也认得出是它。
    #[test]
    fn 按钮记的改动序号过期就不认() {
        let mut s = DiffState::new(13.0);
        s.pickable = true;
        s.apply(Ok(diff_two_texts(
            "a\nb\nc\nd\ne\n".into(),
            "a\nB\nc\nd\nE\nF\n".into(),
        )));
        let (first, second) = (s.flat.change_key(0), s.flat.change_key(1));
        assert!(first.is_some() && first != second);
        assert!(s.change_is(1, second) && !s.change_is(0, second));
        let head = s.flat.hunk_key(0).unwrap();
        assert!(s.hunk_is(0, &head));
        // 第 0 处暂存掉之后重取:原来的第 1 处成了第 0 处,块头也变了
        s.apply(Ok(diff_two_texts(
            "a\nB\nc\nd\ne\n".into(),
            "a\nB\nc\nd\nE\nF\n".into(),
        )));
        assert!(!s.change_is(1, second), "序号 1 已经没有了");
        assert!(!s.change_is(0, first), "序号 0 指着原来的第 1 处");
        assert!(s.change_is(0, second));
        assert!(!s.hunk_is(0, &head));
        assert!(!s.change_is(0, None));
    }

    /// 按一处改动操作后落在哪:块里还剩别的改动时停在原地(不跳回块头);动的是块里剩下的
    /// 全部增删行(没勾行,或这一处全勾上了)时块整个落下去,按块的口径落到原位置上的下一块。
    /// 行内视图的块头按钮照旧落到下一块。
    #[test]
    fn 按改动操作后停在原地或落到下一块() {
        let mut s = DiffState::new(13.0);
        s.pickable = true;
        s.apply(Ok(diff_two_texts(
            "a\nb\nc\nd\ne\n".into(),
            "a\nB\nc\nd\nE\nF\n".into(),
        )));
        assert_eq!(s.op_target(OpScope::Change(0)).unwrap().land, None);
        assert_eq!(s.op_target(OpScope::Hunk(0)).unwrap().land, Some(0));

        let mut new = numbered(40);
        new[4] = "FIVE".into();
        new[30] = "THIRTY".into();
        s.apply(Ok(diff_two_texts(text(&numbered(40)), text(&new))));
        assert_eq!((s.flat.heads.len(), s.flat.split.changes.len()), (2, 2));
        assert_eq!(s.op_target(OpScope::Change(1)).unwrap().land, Some(1));
        s.toggle_change_picks(1);
        assert_eq!(s.op_target(OpScope::Change(1)).unwrap().land, Some(1));
        // 只勾了一部分:块还在
        s.toggle_pick(s.flat.change_lines(1)[0]);
        assert_eq!(s.op_target(OpScope::Change(1)).unwrap().land, None);
    }

    /// 按中间栏的箭头把 `cur` 的第 `change` 处改动往 `goal` 那边搬一步(暂存 = `Apply`,丢弃 =
    /// `Revert`),返回写下去的那一侧。
    fn step_change(
        s: &mut DiffState,
        (old, new): (&str, &str),
        change: usize,
        dir: HunkDirection,
    ) -> String {
        s.apply(Ok(diff_two_texts(old.into(), new.into())));
        let target = s.op_target(OpScope::Change(change)).expect("有这一处");
        let picks: Vec<_> = target
            .picks
            .iter()
            .map(|pick| (&target.diff.hunks[pick.hunk], pick.lines.as_ref()))
            .collect();
        splice_hunks(old, new, &picks, dir, "\n").unwrap()
    }

    /// 同一 hunk 里两处改动、后一处碰到文件末尾且末尾换行不同(这处差异 diff 里看不见):不管先
    /// 点哪一处,两处都暂存 / 丢弃之后两侧逐字节相同 —— 末尾换行不会被孤零零留下、文件也就不会
    /// 「没有改动却一直挂在未暂存里」。
    #[test]
    fn 按改动操作不漏掉末尾换行() {
        use HunkDirection::{Apply, Revert};
        let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
        let new = "l1\nl2\nL3\nl4\nl5\nl6\nl7\nl8\nl9\nL10";
        assert_eq!(diff_two_texts(old.into(), new.into()).hunks.len(), 1);
        let mut s = DiffState::new(13.0);
        s.pickable = true;
        for first in [1, 0] {
            // 暂存:先动 `first` 那一处,剩下的那处这时是第 0 处
            let staged = step_change(&mut s, (old, new), first, Apply);
            let staged = step_change(&mut s, (&staged, new), 0, Apply);
            assert_eq!(staged, new, "先暂存第 {first} 处");
            // 丢弃
            let kept = step_change(&mut s, (old, new), first, Revert);
            let kept = step_change(&mut s, (old, &kept), 0, Revert);
            assert_eq!(kept, old, "先丢弃第 {first} 处");
        }
    }

    /// 块头按钮上的字:没勾 = 整块,勾了 = 「所选 N 行」;按侧给哪几种操作。
    #[test]
    fn 块操作按钮的文案与种类() {
        assert_eq!(
            op_label(HunkOp::Stage, 0).as_ref(),
            t("gitCommit", "stageHunk")
        );
        let lines = op_label(HunkOp::Discard, 3);
        assert!(lines.contains('3'), "{lines}");
        assert_ne!(lines.as_ref(), t("gitCommit", "discardHunk"));
        assert_eq!(
            side_ops(DiffSide::Unstaged),
            &[HunkOp::Stage, HunkOp::Discard]
        );
        assert_eq!(side_ops(DiffSide::Staged), &[HunkOp::Unstage]);
        assert!(side_ops(DiffSide::Working).is_empty());
    }

    /// 静默重取(页签切回来 / 刷新 / 仓库变了):拿回来的与屏幕上一样(同样的两侧全文、同一句
    /// 错误)就什么都不动 —— 选区、跳转计数、滚动位置都留着;内容或标记变了、正文与错误互换、
    /// 屏幕上还什么都没有,才落地。
    #[test]
    fn 静默重取没变就不动() {
        let a = diff_two_texts("x\ny\n".into(), "x\nz\n".into());
        let same = diff_two_texts("x\ny\n".into(), "x\nz\n".into());
        let other = diff_two_texts("x\ny\n".into(), "x\nw\n".into());
        assert!(same_diff(&a, &same));
        assert!(!same_diff(&a, &other));
        let mut binary = same.clone();
        binary.is_binary = true;
        assert!(!same_diff(&a, &binary), "标记变了也算变");

        assert!(refresh_unchanged(Some(&a), None, &Ok(same)));
        assert!(!refresh_unchanged(Some(&a), None, &Ok(other)));
        let err = || anyhow::anyhow!("文件不存在");
        assert!(refresh_unchanged(None, Some("文件不存在"), &Err(err())));
        assert!(!refresh_unchanged(None, Some("权限不足"), &Err(err())));
        assert!(!refresh_unchanged(Some(&a), None, &Err(err())));
        assert!(!refresh_unchanged(None, Some("文件不存在"), &Ok(a.clone())));
        assert!(!refresh_unchanged(None, None, &Ok(a)), "还在加载:照常落地");
    }
}
