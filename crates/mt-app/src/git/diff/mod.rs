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
use mt_remote::GitContext;
use mt_ui::icons::FileIcon;
use mt_ui::tooltip::{Tooltip, TooltipExt as _};

use crate::git::ui::{
    self as git_ui, ButtonKind, ChangeTreeRow, ICON_ARROW_DOWN, ICON_ARROW_UP, ICON_CHEVRON_LEFT,
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

mod commit_diff;
mod file_diff;
mod flat;
mod rows;

pub(crate) use commit_diff::CommitFileDiffPane;
use commit_diff::*;
use file_diff::*;
pub use file_diff::{DiffFile, FileDiffHooks, FilesDone, OnChanged, OnSwitch, ReloadFiles};
use flat::*;
use rows::*;

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
    /// 「并排 / 内联」段控件的滑动选中块(见 `segment_slider`)。
    view_slider: crate::segment_slider::IndicatorSlider,
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
            view_slider: crate::segment_slider::IndicatorSlider::new(),
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
    let (current, slider) = {
        let s = state.read(cx);
        (s.view, s.view_slider.clone())
    };
    // 选中底色是一块滑动的绝对定位块(见 `segment_slider`):两段宽度随文字变,先量再滑
    let active = usize::from(matches!(current, ViewMode::Inline));
    let own_bg = !slider.ready(active);
    let mut seg = git_ui::segmented().child(git_ui::segmented_slider(&slider, active));
    for (ix, (mode, label)) in [
        (ViewMode::SideBySide, side_label),
        (ViewMode::Inline, inline_label),
    ]
    .into_iter()
    .enumerate()
    {
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
                own_bg,
            )
            .child(slider.probe(ix))
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

#[cfg(test)]
mod tests;
