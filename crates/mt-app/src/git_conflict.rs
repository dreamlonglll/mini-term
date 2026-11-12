//! 冲突文件的「逐段解决」弹窗:列出文件里的每一段冲突,两侧并排显示(diff3 / zdiff3 写法
//! 还有折叠着的共同祖先),每段选采用哪边,最后一次性写回工作区,可接着标记为已解决。
//!
//! 入口在更改列表的冲突行:右键菜单「逐段解决…」与行内按钮(`git_changes`)。能不能用
//! 随列表一起判好([`mt_project::git::conflict_segment_status`]):删除 / 修改冲突、文件里
//! 没有成对标记、二进制、编码认不出(只读)、过大时菜单项禁用并写明原因([`blocker_key`])。
//! 标记规则、防陈旧、编码与原子写的口径见 `mt_project::git::conflict_blocks` 的模块注释。
//! 远程(SSH)项目没有 Git 面板(`git_panel` 显示「暂不支持」),这里也只管本地仓库。
//!
//! # 方向文案
//!
//! 变基时 git 的 ours / theirs 与直觉相反(ours = 变基目标,theirs = 正在重放的你的提交),
//! 两侧标题与四个选择按钮换一套写清方向的文案([`side_key`] / [`pick_key`]),与整文件
//! 「采用一侧」的 `git_changes::take_side_key` 同一口径。标记行上的名字(`<<<<<<< HEAD`
//! 里的 `HEAD`)照原样显示在方向文案旁边。
//!
//! # 两侧原文
//!
//! 每侧一个只读的 `Textarea`(等宽、能选中复制、折行;超过 [`CODE_MAX_ROWS`] 行在框里
//! 滚)。显示时 CRLF 收成 LF、Tab 按制表位展开(GPUI 的整形器把 `\t` 画成零宽,见
//! `tab_expansion`)、去掉末尾那个换行 —— 只影响显示,写回用的是原文字节。
//!
//! # 结果就地显示
//!
//! toast 在弹窗遮罩下看不见:写回失败、文件已变化(防陈旧拒绝)、写操作闸被占都**就地**
//! 显示在底部按钮上方;成功才关窗并推 toast。等待期间弹窗被 Esc 关掉的话,失败退到
//! [`git_ui::show_git_error`]。写完(成败都算)回调 `on_written`:更改列表重取、文件树的
//! git 标记重算。开着这个文件的文件页靠目录监听自己跟上(写回是「临时文件 + 改名」,
//! 改名事件落在目标路径上:干净的页签静默重载,有未保存修改的挂「外部已修改」提示条)。
//!
//! 「刷新」重读文件:各段内容没变的,已选的保留(按内容对上,不按序号 —— 终端里解决掉
//! 前面一段之后序号会挪)。
//!
//! # 防重入
//!
//! 种类唯一(`kind::GIT_CONFLICT_SEGMENTS`),连点两次不叠开;写回走仓库的写操作闸
//! ([`git_ui::spawn_gated`]),进行中两颗写入按钮禁用、按下的那颗换「写入中…」。已经选过
//! 段时点遮罩不关窗(免得误触丢掉选择),Esc 与「取消」照常关。
//!
//! # 键盘
//!
//! F7 / Shift+F7 跳下一段 / 上一段(与 diff 页签跳改动同一对键);Esc 关闭。回车**不**
//! 写入 —— 改工作区文件的动作只认明确的点击。

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Div, Entity, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Pixels, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Textarea, TextareaState};
use mt_project::encoding::TextEncoding;
use mt_project::git::{
    self, ConflictBlock, ConflictFile, ConflictPick, RepoOpKind, SegmentBlocker, StaleConflictFile,
};
use mt_ui::tooltip::TooltipExt as _;

use crate::git_diff;
use crate::git_dir_watch::RepoObservation;
use crate::git_ui::{
    self, ButtonKind, ICON_ARROW_DOWN, ICON_ARROW_UP, ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT,
    ICON_CLOSE, ICON_MERGE, ICON_REFRESH, Tone, mono,
};
use crate::i18n::{t, tr};
use crate::prompt::{close_guarded, is_open, kind, open_guarded};
use crate::store::AppStore;
use crate::tab_expansion::{TAB_WIDTH, expand_line};
use crate::ui;

gpui::actions!(
    mini_term,
    [
        /// 逐段解决弹窗:下一段(F7)
        GitConflictNextSegment,
        /// 逐段解决弹窗:上一段(Shift+F7)
        GitConflictPrevSegment,
    ]
);

/// 逐段解决弹窗的 key context(F7 / Shift+F7 绑在 `hotkeys.rs` 的表外段)。
pub const SEGMENTS_CONTEXT: &str = "GitConflictSegments";

/// 写回之后(成败都算)让更改列表重载。参数是写完当场读的仓库签名(读不到为 `None`),
/// 面板拿它当「已知」,紧接着那次 `.git` 检查就不再重刷一遍(同 `GitChangesEvent::IndexWritten`)。
pub type OnWritten = Rc<dyn Fn(Option<RepoObservation>, &mut App)>;

/// 原文框最多长到几行,再多就在框里滚。
const CODE_MAX_ROWS: usize = 12;
/// 原文的字号(正文档)。
const CODE_TEXT: f32 = 13.0;
/// 四种选择,按钮从左到右的顺序。
const PICKS: [ConflictPick; 4] = [
    ConflictPick::Ours,
    ConflictPick::Theirs,
    ConflictPick::OursThenTheirs,
    ConflictPick::TheirsThenOurs,
];

// ─── 文案 ─────────────────────────────────────────────────────

/// 一侧的标题(`gitCommit` 命名空间)。变基时换成写清方向的那一套(见模块注释)。
fn side_key(theirs: bool, rebasing: bool) -> &'static str {
    match (theirs, rebasing) {
        (false, false) => "segments.ours",
        (true, false) => "segments.theirs",
        (false, true) => "segments.oursRebase",
        (true, true) => "segments.theirsRebase",
    }
}

/// 选择按钮(与已选胶囊)的文案。
fn pick_key(pick: ConflictPick, rebasing: bool) -> &'static str {
    match (pick, rebasing) {
        (ConflictPick::Ours, false) => "segments.pickOurs",
        (ConflictPick::Theirs, false) => "segments.pickTheirs",
        (ConflictPick::OursThenTheirs, false) => "segments.pickOursThenTheirs",
        (ConflictPick::TheirsThenOurs, false) => "segments.pickTheirsThenOurs",
        (ConflictPick::Ours, true) => "segments.pickOursRebase",
        (ConflictPick::Theirs, true) => "segments.pickTheirsRebase",
        (ConflictPick::OursThenTheirs, true) => "segments.pickOursThenTheirsRebase",
        (ConflictPick::TheirsThenOurs, true) => "segments.pickTheirsThenOursRebase",
    }
}

/// 不能逐段解决的原因(菜单项的禁用说明、弹窗里的就地提示)。
pub(crate) fn blocker_key(blocker: SegmentBlocker) -> &'static str {
    match blocker {
        SegmentBlocker::DeleteModify => "segments.blockedDeleteModify",
        SegmentBlocker::NotConflicted => "segments.blockedNotConflicted",
        SegmentBlocker::Unreadable => "segments.blockedUnreadable",
        SegmentBlocker::TooLarge => "segments.blockedTooLarge",
        SegmentBlocker::Binary => "segments.blockedBinary",
        SegmentBlocker::Lossy => "segments.blockedLossy",
        SegmentBlocker::NoMarkers => "segments.blockedNoMarkers",
    }
}

/// 原文框里显示的文字:CRLF 收成 LF、Tab 按制表位展开、去掉末尾那个换行(见模块注释)。
fn shown_text(text: &str) -> String {
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n')
        .map(|line| expand_line(line.strip_suffix('\r').unwrap_or(line), TAB_WIDTH))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一段的三块内容(拿来比「刷新前后是不是同一段」)。
fn block_texts<'a>(
    file: &'a ConflictFile,
    block: &ConflictBlock,
) -> (&'a str, Option<&'a str>, &'a str) {
    (
        file.ours_text(block),
        file.base_text(block),
        file.theirs_text(block),
    )
}

/// 刷新之后沿用已选的:新文件里的每一段按内容(两侧 + base)找旧文件里还没被认领的同一段,
/// 找到就沿用它的选择。按内容不按序号:终端里解决掉前面一段,后面几段的序号都会挪。
fn carry_picks(
    old: Option<&ConflictFile>,
    old_picks: &[Option<ConflictPick>],
    new: &ConflictFile,
) -> Vec<Option<ConflictPick>> {
    let Some(old) = old else {
        return vec![None; new.blocks.len()];
    };
    let mut claimed = vec![false; old.blocks.len()];
    new.blocks
        .iter()
        .map(|block| {
            let wanted = block_texts(new, block);
            let found =
                old.blocks.iter().enumerate().position(|(i, candidate)| {
                    !claimed[i] && block_texts(old, candidate) == wanted
                })?;
            claimed[found] = true;
            old_picks.get(found).copied().flatten()
        })
        .collect()
}

// ─── 状态 ─────────────────────────────────────────────────────

/// 一段的原文框(两侧,有 base 段时还有共同祖先)。
struct BlockView {
    ours: Entity<TextareaState>,
    base: Option<Entity<TextareaState>>,
    theirs: Entity<TextareaState>,
}

/// 正文区画什么。
enum Body {
    Loading,
    /// 不能逐段解决(原因的文案 key)
    Blocked(&'static str),
    /// 读取失败的原文
    Failed(String),
    Ready {
        file: Arc<ConflictFile>,
        views: Vec<BlockView>,
    },
}

/// 底部按钮上方的就地提示。
enum Notice {
    /// 写回前重读发现文件变了(带「刷新」)
    Stale,
    /// 写操作闸被别的区占着
    Gate,
    /// 写回时才发现不能逐段解决(已不在冲突中…)
    Blocked(&'static str),
    /// 失败原文(可选中复制)
    Error(Entity<TextareaState>),
}

struct SegmentDialog {
    store: Entity<AppStore>,
    repo_path: String,
    /// 仓库内相对路径
    path: String,
    rebasing: bool,
    on_written: OnWritten,
    body: Body,
    /// 各段的选择,与 `Body::Ready` 里的段一一对应
    picks: Vec<Option<ConflictPick>>,
    /// 上一段 / 下一段从哪一段走(点过的、选过的那段)
    current: usize,
    /// 展开着共同祖先的段
    base_open: HashSet<usize>,
    /// 正在重读文件
    loading: bool,
    /// 读取的请求令牌(连按刷新时迟到的旧结果丢掉)
    request: u64,
    /// 正在写回;`Some(true)` = 写入并标记已解决
    writing: Option<bool>,
    notice: Option<Notice>,
    scroll: ScrollHandle,
    focus: FocusHandle,
}

impl SegmentDialog {
    fn ready(&self) -> Option<&ConflictFile> {
        match &self.body {
            Body::Ready { file, .. } => Some(file),
            _ => None,
        }
    }

    /// (已选段数, 总段数);没读好时为 `(0, 0)`。
    fn progress(&self) -> (usize, usize) {
        match self.ready() {
            Some(file) => (
                self.picks.iter().filter(|p| p.is_some()).count(),
                file.blocks.len(),
            ),
            None => (0, 0),
        }
    }
}

// ─── 入口 ─────────────────────────────────────────────────────

/// 打开某个冲突文件的逐段解决弹窗。`repo_path` 是仓库根(Git 面板当前选中的仓库),
/// `path` 是暂存区里的相对路径,`op` 是进行中的合并 / 变基…(方向文案随它翻转)。
pub fn open(
    store: Entity<AppStore>,
    repo_path: String,
    path: String,
    op: Option<RepoOpKind>,
    on_written: OnWritten,
    window: &mut Window,
    cx: &mut App,
) {
    // 守卫要在建实体之前判:已经开着一个的话,下面的读取与聚焦都是白干
    if repo_path.is_empty() || is_open(kind::GIT_CONFLICT_SEGMENTS) {
        return;
    }
    let state = cx.new(|cx| SegmentDialog {
        store,
        repo_path,
        path,
        rebasing: op == Some(RepoOpKind::Rebase),
        on_written,
        body: Body::Loading,
        picks: Vec::new(),
        current: 0,
        base_open: HashSet::new(),
        loading: false,
        request: 0,
        writing: None,
        notice: None,
        scroll: ScrollHandle::new(),
        focus: cx.focus_handle(),
    });
    reload(&state, window, cx);
    let focus = state.read(cx).focus.clone();
    open_guarded(
        kind::GIT_CONFLICT_SEGMENTS,
        window,
        cx,
        move |dialog, window, cx| {
            let (width, body_h) = git_diff::modal_size(window.viewport_size());
            let s = state.read(cx);
            let keep = s.writing.is_some() || s.picks.iter().any(Option::is_some);
            dialog
                .w(width)
                // 正文自己排工具栏 / 列表 / 底部按钮,与贮藏弹窗同一手法
                .p_0()
                .overlay_closable(!keep)
                .child(render(&state, body_h, cx))
        },
    );
    // 打开即可按 F7,不必先点一下(时机讲究见 `prompt::autofocus`)
    window.defer(cx, move |window, cx| window.focus(&focus, cx));
}

/// 读(或重读)文件。期间正文照旧,刷新按钮转圈;读回来按请求令牌认领。
fn reload(state: &Entity<SegmentDialog>, window: &mut Window, cx: &mut App) {
    let (repo, path, request) = state.update(cx, |s, cx| {
        s.request += 1;
        s.loading = true;
        cx.notify();
        (PathBuf::from(&s.repo_path), s.path.clone(), s.request)
    });
    let state = state.clone();
    window
        .spawn(cx, async move |cx| {
            let result = cx
                .background_executor()
                .spawn(async move { git::load_conflict_file(&repo, &path) })
                .await;
            let _ = cx.update(|window, cx| apply_load(&state, request, result, window, cx));
        })
        .detach();
}

fn apply_load(
    state: &Entity<SegmentDialog>,
    request: u64,
    result: anyhow::Result<ConflictFile>,
    window: &mut Window,
    cx: &mut App,
) {
    if state.read(cx).request != request {
        return;
    }
    let file = match result {
        Ok(file) => file,
        Err(err) => {
            let body = match err.downcast_ref::<SegmentBlocker>() {
                Some(&blocker) => Body::Blocked(blocker_key(blocker)),
                None => Body::Failed(git_ui::error_text(&err)),
            };
            state.update(cx, |s, cx| {
                s.body = body;
                s.picks.clear();
                s.loading = false;
                s.notice = None;
                cx.notify();
            });
            return;
        }
    };
    // 原文框要 `window` 才建得出来,读回来当场建好(builder 每帧重跑,不能在那里建)
    let views = file
        .blocks
        .iter()
        .map(|block| BlockView {
            ours: code_view(file.ours_text(block), window, cx),
            base: file
                .base_text(block)
                .map(|text| code_view(text, window, cx)),
            theirs: code_view(file.theirs_text(block), window, cx),
        })
        .collect();
    state.update(cx, |s, cx| {
        s.picks = carry_picks(s.ready(), &s.picks, &file);
        s.current = s.current.min(file.blocks.len().saturating_sub(1));
        let count = file.blocks.len();
        s.base_open.retain(|&i| i < count);
        s.body = Body::Ready {
            file: Arc::new(file),
            views,
        };
        s.loading = false;
        // 重读过了:「文件已变化」与上一次的失败都不再作数
        s.notice = None;
        cx.notify();
    });
}

/// 一侧原文的只读框(见模块注释「两侧原文」)。
fn code_view(text: &str, window: &mut Window, cx: &mut App) -> Entity<TextareaState> {
    let shown = shown_text(text);
    cx.new(|cx| {
        TextareaState::new(window, cx)
            .auto_grow(1, CODE_MAX_ROWS)
            .default_value(shown)
    })
}

// ─── 操作 ─────────────────────────────────────────────────────

/// 上一段 / 下一段:换当前段(卡片描 accent 边)并把它滚进视野 —— 已经整张看得见就不动,
/// 比视野高的顶对齐(gpui `scroll_to_item` 的默认策略)。
fn step(state: &Entity<SegmentDialog>, delta: isize, cx: &mut App) {
    state.update(cx, |s, cx| {
        let Some(count) = s.ready().map(|file| file.blocks.len()) else {
            return;
        };
        if count == 0 {
            return;
        }
        let next = (s.current as isize + delta).clamp(0, count as isize - 1) as usize;
        s.current = next;
        // 滚动容器的直接子元素就是一段一张卡片,下标对得上
        s.scroll.scroll_to_item(next);
        cx.notify();
    });
}

/// 点到哪段,上一段 / 下一段就从哪段走。
fn focus_segment(state: &Entity<SegmentDialog>, index: usize, cx: &mut App) {
    state.update(cx, |s, cx| {
        if s.current != index {
            s.current = index;
            cx.notify();
        }
    });
}

fn set_pick(state: &Entity<SegmentDialog>, index: usize, pick: Option<ConflictPick>, cx: &mut App) {
    state.update(cx, |s, cx| {
        if s.writing.is_some() {
            return;
        }
        if let Some(slot) = s.picks.get_mut(index) {
            *slot = pick;
        }
        s.current = index;
        cx.notify();
    });
}

fn toggle_base(state: &Entity<SegmentDialog>, index: usize, cx: &mut App) {
    state.update(cx, |s, cx| {
        if !s.base_open.remove(&index) {
            s.base_open.insert(index);
        }
        s.current = index;
        cx.notify();
    });
}

/// 写回。`mark` = 写入并标记已解决(每段都选了才行);否则至少选了一段。
fn write(state: &Entity<SegmentDialog>, mark: bool, window: &mut Window, cx: &mut App) {
    let Some((repo, file, picks)) = state.update(cx, |s, cx| {
        if s.writing.is_some() {
            return None;
        }
        let file = match &s.body {
            Body::Ready { file, .. } => file.clone(),
            _ => return None,
        };
        let (done, total) = s.progress();
        if done == 0 || (mark && done < total) {
            return None;
        }
        s.writing = Some(mark);
        s.notice = None;
        cx.notify();
        Some((s.repo_path.clone(), file, s.picks.clone()))
    }) else {
        return;
    };
    let path = file.path.clone();
    let remaining = picks.iter().filter(|p| p.is_none()).count();
    let done_state = state.clone();
    let started = git_ui::spawn_gated(
        &repo,
        move |repo| {
            let result = git::write_conflict_resolution(repo, &file, &picks, mark);
            // 写完当场读一份签名(成败都读:标记已解决失败时文件已经写了),面板拿它当「已知」
            Ok((result, RepoObservation::read(repo)))
        },
        move |outcome, window, cx| {
            finish_write(&done_state, mark, &path, remaining, outcome, window, cx);
        },
        window,
        cx,
    );
    if !started {
        state.update(cx, |s, cx| {
            s.writing = None;
            s.notice = Some(Notice::Gate);
            cx.notify();
        });
    }
}

type WriteOutcome = anyhow::Result<(anyhow::Result<()>, RepoObservation)>;

fn finish_write(
    state: &Entity<SegmentDialog>,
    mark: bool,
    path: &str,
    remaining: usize,
    outcome: WriteOutcome,
    window: &mut Window,
    cx: &mut App,
) {
    // 外层只是包装,实际恒为 `Ok`
    let (result, observed) = match outcome {
        Ok((result, observed)) => (result, Some(observed)),
        Err(err) => (Err(err), None),
    };
    let open = is_open(kind::GIT_CONFLICT_SEGMENTS);
    let (store, on_written) = {
        let s = state.read(cx);
        (s.store.clone(), s.on_written.clone())
    };
    match result {
        Ok(()) => {
            state.update(cx, |s, cx| {
                s.writing = None;
                cx.notify();
            });
            // 上面还压着别的弹窗(关不掉,`close_guarded` 只关栈顶):文件已经变了,重读一遍,
            // 免得留着的是写回之前的段
            if open && !close_guarded(kind::GIT_CONFLICT_SEGMENTS, window, cx) {
                reload(state, window, cx);
            }
            let message = if mark {
                tr!("gitCommit", "segments.resolved", path = path)
            } else if remaining > 0 {
                tr!(
                    "gitCommit",
                    "segments.written",
                    path = path,
                    count = remaining
                )
            } else {
                tr!("gitCommit", "segments.writtenAll", path = path)
            };
            // 弹窗已经关了,toast 看得见
            git_ui::toast(&store, message, Tone::Info, cx);
        }
        Err(err) => {
            let detail = git_ui::error_text(&err);
            eprintln!("[git] 逐段解决 {path} 失败: {detail}");
            let notice = if err.is::<StaleConflictFile>() {
                Notice::Stale
            } else if let Some(&blocker) = err.downcast_ref::<SegmentBlocker>() {
                Notice::Blocked(blocker_key(blocker))
            } else {
                Notice::Error(git_ui::error_text_state(&detail, 8, window, cx))
            };
            // 等待期间弹窗被关掉了:原因不能丢
            if !open {
                let text = match &notice {
                    Notice::Stale => t("gitCommit", "segments.stale").to_string(),
                    Notice::Blocked(key) => t("gitCommit", key).to_string(),
                    _ => detail,
                };
                git_ui::show_git_error(t("gitCommit", "segments.failedTitle"), text, window, cx);
            }
            state.update(cx, |s, cx| {
                s.writing = None;
                s.notice = Some(notice);
                cx.notify();
            });
        }
    }
    // 失败也可能动过工作区(写了文件、标记已解决那一步没成):列表一律重载
    on_written(observed, cx);
}

// ─── 渲染 ─────────────────────────────────────────────────────

fn render(state: &Entity<SegmentDialog>, body_h: Pixels, cx: &App) -> AnyElement {
    let s = state.read(cx);
    let (next, prev) = (state.clone(), state.clone());
    let scroller = div()
        .id("git-conflict-segments-body")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .track_scroll(&s.scroll)
        .bg(ui::bg_base());
    // 一段一张卡片,**直接**挂在滚动容器下:上一段 / 下一段按子元素下标滚(`step`)
    let body = match &s.body {
        Body::Loading => scroller.child(git_ui::loading_state(t("gitCommit", "segments.loading"))),
        Body::Blocked(key) => scroller.child(git_ui::error_state(t("gitCommit", key))),
        Body::Failed(text) => scroller.child(git_ui::error_state(text.clone())),
        Body::Ready { file, views } => scroller
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .children(
                views
                    .iter()
                    .enumerate()
                    .map(|(index, view)| render_card(state, s, file, view, index)),
            ),
    };
    div()
        .w_full()
        .h(body_h)
        .flex()
        .flex_col()
        // 工具栏 / 正文各自有底色,不裁会盖掉面板的圆角
        .overflow_hidden()
        .key_context(SEGMENTS_CONTEXT)
        .track_focus(&s.focus)
        .on_action(move |_: &GitConflictNextSegment, _window, cx| step(&next, 1, cx))
        .on_action(move |_: &GitConflictPrevSegment, _window, cx| step(&prev, -1, cx))
        .child(render_toolbar(state, s))
        .children(render_hint(s))
        .child(body)
        .children(render_notice(state, s))
        .child(render_footer(state, s))
        .into_any_element()
}

/// 工具栏:图标 + 标题 + 文件路径 … 进度胶囊、上一段 / 下一段、刷新、关闭。
fn render_toolbar(state: &Entity<SegmentDialog>, s: &SegmentDialog) -> Div {
    let (done, total) = s.progress();
    let can_step = total > 1;
    let (prev, next, refresh) = (state.clone(), state.clone(), state.clone());
    let nav = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.0))
        .child(
            git_ui::icon_button(
                "git-conflict-prev",
                ICON_ARROW_UP,
                t("gitCommit", "segments.prev"),
                can_step,
            )
            .when(can_step, |el| {
                el.on_click(move |_: &ClickEvent, _window, cx| step(&prev, -1, cx))
            }),
        )
        .child(
            git_ui::icon_button(
                "git-conflict-next",
                ICON_ARROW_DOWN,
                t("gitCommit", "segments.next"),
                can_step,
            )
            .when(can_step, |el| {
                el.on_click(move |_: &ClickEvent, _window, cx| step(&next, 1, cx))
            }),
        )
        .child(
            git_ui::busy_icon_button(
                "git-conflict-reload",
                ICON_REFRESH,
                t("gitCommit", "segments.reload"),
                s.loading,
                s.writing.is_some(),
            )
            .when(!s.loading && s.writing.is_none(), |el| {
                el.on_click(move |_: &ClickEvent, window, cx| reload(&refresh, window, cx))
            }),
        )
        .child(
            git_ui::icon_button(
                "git-conflict-close",
                ICON_CLOSE,
                t("gitCommit", "segments.close"),
                true,
            )
            .ml(px(8.0))
            .on_click(|_: &ClickEvent, window, cx| {
                close_guarded(kind::GIT_CONFLICT_SEGMENTS, window, cx);
            }),
        );
    let progress_color = if total > 0 && done == total {
        ui::color_success()
    } else {
        ui::color_warning()
    };
    div()
        .flex_none()
        .h(px(git_diff::TOOLBAR_H))
        .px(px(16.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(git_ui::icon(ICON_MERGE, git_ui::ICON_SIZE, ui::accent()))
        .child(
            div()
                .flex_none()
                .text_size(ui::font_px(14.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui::text_primary())
                .child(t("gitCommit", "segments.title")),
        )
        .child(
            mono(div())
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(s.path.clone()),
        )
        .when(total > 0, |el| {
            el.child(git_ui::pill(
                tr!("gitCommit", "segments.progress", done = done, total = total),
                progress_color,
            ))
        })
        .child(nav)
}

/// 工具栏下面的一行说明:两侧的方向(变基时另一套),非 UTF-8 文件再写一句按什么编码写回。
fn render_hint(s: &SegmentDialog) -> Option<Div> {
    let file = s.ready()?;
    let direction = if s.rebasing {
        t("gitCommit", "segments.hintRebase")
    } else {
        t("gitCommit", "segments.hintMerge")
    };
    let encoding = (file.encoding != TextEncoding::Utf8).then(|| {
        tr!(
            "gitCommit",
            "segments.encoding",
            encoding = file.encoding.label()
        )
    });
    Some(
        div()
            .flex_none()
            .px(px(16.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(git_ui::hint_line(direction))
            .children(encoding.map(git_ui::hint_line)),
    )
}

/// 一段一张卡片:标题行(第几段 · 第几行 … 选了什么)+ 两侧原文 + 共同祖先(折叠)+ 选择按钮。
fn render_card(
    state: &Entity<SegmentDialog>,
    s: &SegmentDialog,
    file: &ConflictFile,
    view: &BlockView,
    index: usize,
) -> AnyElement {
    let block = &file.blocks[index];
    let pick = s.picks.get(index).copied().flatten();
    let rebasing = s.rebasing;
    let status = match pick {
        Some(pick) => git_ui::pill(
            t("gitCommit", pick_key(pick, rebasing)),
            ui::color_success(),
        ),
        None => git_ui::pill(t("gitCommit", "segments.unpicked"), ui::color_warning()),
    };
    let header = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .text_size(ui::font_px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui::text_primary())
                .child(tr!("gitCommit", "segments.segment", index = index + 1)),
        )
        .child(
            div()
                .flex_none()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(tr!(
                    "gitCommit",
                    "segments.line",
                    line = block.start_line + 1
                )),
        )
        .child(div().flex_1())
        .child(status);
    let columns = div()
        .flex()
        .items_start()
        .gap(px(12.0))
        .child(side_column(
            t("gitCommit", side_key(false, rebasing)),
            &block.ours_label,
            block.ours_lines(),
            &view.ours,
            file.ours_text(block).is_empty(),
        ))
        .child(side_column(
            t("gitCommit", side_key(true, rebasing)),
            &block.theirs_label,
            block.theirs_lines(),
            &view.theirs,
            file.theirs_text(block).is_empty(),
        ));
    let base = view.base.as_ref().map(|base_view| {
        render_base(
            state,
            block,
            base_view,
            file.base_text(block).is_some_and(str::is_empty),
            s.base_open.contains(&index),
            index,
        )
    });
    let card_state = state.clone();
    div()
        .id(SharedString::from(format!("git-conflict-seg-{index}")))
        .flex()
        .flex_col()
        .gap(px(10.0))
        .p(px(12.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(if s.current == index {
            ui::accent()
        } else {
            ui::border_default()
        })
        .bg(ui::bg_surface())
        .on_click(move |_: &ClickEvent, _window, cx| focus_segment(&card_state, index, cx))
        .child(header)
        .child(columns)
        .children(base)
        .child(render_picks(
            state,
            pick,
            rebasing,
            s.writing.is_some(),
            index,
        ))
        .into_any_element()
}

/// 一侧:方向文案 + 标记行上的名字 … 行数,下面是原文框。
fn side_column(
    title: &'static str,
    label: &str,
    lines: usize,
    view: &Entity<TextareaState>,
    empty: bool,
) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_none()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_secondary())
                        .child(title),
                )
                .child(
                    mono(div())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(tr!("gitCommit", "segments.lines", count = lines)),
                ),
        )
        .child(code_box(view, empty))
}

/// 原文框的外壳(细边框、正文底色);这一侧是空的(把这几行删了)时换成一句弱化说明。
fn code_box(view: &Entity<TextareaState>, empty: bool) -> AnyElement {
    let frame = || {
        div()
            .w_full()
            .rounded(px(4.0))
            .border_1()
            .border_color(ui::border_subtle())
            .bg(ui::bg_base())
    };
    if empty {
        return frame()
            .px(px(10.0))
            .py(px(6.0))
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_muted())
            .child(t("gitCommit", "segments.sideEmpty"))
            .into_any_element();
    }
    mono(frame())
        .child(
            Textarea::new(view)
                .readonly(true)
                .bordered(false)
                .text_size(ui::font_px(CODE_TEXT)),
        )
        .into_any_element()
}

/// 共同祖先:默认折着(一行「▸ 共同祖先(base) · 名字 · N 行」),点开看原文。
fn render_base(
    state: &Entity<SegmentDialog>,
    block: &ConflictBlock,
    view: &Entity<TextareaState>,
    empty: bool,
    open: bool,
    index: usize,
) -> Div {
    let toggle_state = state.clone();
    let toggle = div()
        .id(SharedString::from(format!("git-conflict-seg-{index}-base")))
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(24.0))
        .cursor_pointer()
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_secondary())
        .hover(|el| el.text_color(ui::text_primary()))
        .child(git_ui::icon(
            if open {
                ICON_CHEVRON_DOWN
            } else {
                ICON_CHEVRON_RIGHT
            },
            git_ui::ICON_SIZE_SM,
            ui::text_muted(),
        ))
        .child(div().flex_none().child(t("gitCommit", "segments.base")))
        .child(
            mono(div())
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(block.base_label.clone().unwrap_or_default()),
        )
        .child(
            div()
                .flex_none()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(tr!(
                    "gitCommit",
                    "segments.lines",
                    count = block.base_lines().unwrap_or(0)
                )),
        )
        .on_click(move |_: &ClickEvent, _window, cx| {
            cx.stop_propagation();
            toggle_base(&toggle_state, index, cx);
        });
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(toggle)
        .when(open, |el| el.child(code_box(view, empty)))
}

/// 四个选择按钮(选中的那颗实心)+ 选过之后的「清除选择」。写回进行中全部禁用。
fn render_picks(
    state: &Entity<SegmentDialog>,
    picked: Option<ConflictPick>,
    rebasing: bool,
    writing: bool,
    index: usize,
) -> Div {
    let enabled = !writing;
    let mut row = div().flex().flex_wrap().items_center().gap(px(8.0));
    for pick in PICKS {
        let kind = if picked == Some(pick) {
            ButtonKind::Primary
        } else {
            ButtonKind::Ghost
        };
        let pick_state = state.clone();
        row = row.child(
            git_ui::action_button(
                SharedString::from(format!("git-conflict-seg-{index}-{pick:?}")),
                t("gitCommit", pick_key(pick, rebasing)),
                kind,
                enabled,
            )
            .when(enabled, |el| {
                el.on_click(move |_: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                    set_pick(&pick_state, index, Some(pick), cx);
                })
            }),
        );
    }
    if picked.is_some() {
        let clear_state = state.clone();
        row = row.child(div().flex_1()).child(
            git_ui::text_button(
                SharedString::from(format!("git-conflict-seg-{index}-clear")),
                t("gitCommit", "segments.clearPick"),
                enabled,
                false,
            )
            .h(px(26.0))
            .when(enabled, |el| {
                el.on_click(move |_: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                    set_pick(&clear_state, index, None, cx);
                })
            }),
        );
    }
    row
}

/// 底部按钮上方的就地提示(见模块注释「结果就地显示」)。
fn render_notice(state: &Entity<SegmentDialog>, s: &SegmentDialog) -> Option<Div> {
    let notice = s.notice.as_ref()?;
    let content: AnyElement = match notice {
        Notice::Stale => {
            let reload_state = state.clone();
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().flex_1().min_w_0().child(git_ui::notice_line(
                    t("gitCommit", "segments.stale"),
                    ui::color_warning(),
                )))
                .child(
                    git_ui::action_button(
                        "git-conflict-stale-reload",
                        t("gitCommit", "segments.reload"),
                        ButtonKind::Ghost,
                        !s.loading,
                    )
                    .when(!s.loading, |el| {
                        el.on_click(move |_: &ClickEvent, window, cx| {
                            reload(&reload_state, window, cx)
                        })
                    }),
                )
                .into_any_element()
        }
        Notice::Gate => {
            git_ui::notice_line(git_ui::gate_busy_text(), ui::color_warning()).into_any_element()
        }
        Notice::Blocked(key) => {
            git_ui::notice_line(t("gitCommit", key), ui::color_error()).into_any_element()
        }
        Notice::Error(text) => git_ui::error_text_box(text),
    };
    Some(
        div()
            .flex_none()
            .px(px(16.0))
            .pt(px(12.0))
            .border_t_1()
            .border_color(ui::border_subtle())
            .child(content),
    )
}

/// 底部:还剩几段没选 … 取消 / 仅写入文件 / 写入并标记已解决。
fn render_footer(state: &Entity<SegmentDialog>, s: &SegmentDialog) -> Div {
    let (done, total) = s.progress();
    let idle = s.writing.is_none();
    let can_write = idle && done > 0;
    let can_mark = idle && total > 0 && done == total;
    let summary = if total == 0 {
        String::new()
    } else if done == total {
        t("gitCommit", "segments.allPicked").to_string()
    } else {
        tr!("gitCommit", "segments.remaining", count = total - done)
    };
    let label = |busy: bool, key: &'static str| {
        if busy {
            t("gitCommit", "segments.writing")
        } else {
            t("gitCommit", key)
        }
    };
    let (write_state, mark_state) = (state.clone(), state.clone());
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(16.0))
        .py(px(12.0))
        // 有就地提示时它自己带上边线,这里不再画第二条
        .when(s.notice.is_none(), |el| {
            el.border_t_1().border_color(ui::border_subtle())
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(summary),
        )
        .child(
            git_ui::action_button(
                "git-conflict-cancel",
                t("gitCommit", "segments.cancel"),
                ButtonKind::Ghost,
                true,
            )
            .on_click(|_: &ClickEvent, window, cx| {
                close_guarded(kind::GIT_CONFLICT_SEGMENTS, window, cx);
            }),
        )
        .child(
            git_ui::action_button(
                "git-conflict-write",
                label(s.writing == Some(false), "segments.writeOnly"),
                ButtonKind::Ghost,
                can_write,
            )
            .tip(t("gitCommit", "segments.writeOnlyTip"))
            .when(can_write, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| {
                    write(&write_state, false, window, cx)
                })
            }),
        )
        .child(
            git_ui::action_button(
                "git-conflict-mark",
                label(s.writing == Some(true), "segments.writeAndMark"),
                ButtonKind::Primary,
                can_mark,
            )
            .when(!can_mark && idle, |el| {
                el.tip(t("gitCommit", "segments.writeAndMarkTip"))
            })
            .when(can_mark, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| write(&mark_state, true, window, cx))
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 变基时两侧与四个选择换成写清方向的文案(ours = 变基目标,theirs = 你的改动);
    /// 合并 / 拣选 / 还原时是「当前 / 对方」。
    #[test]
    fn 变基时方向文案翻转() {
        assert_eq!(side_key(false, false), "segments.ours");
        assert_eq!(side_key(true, false), "segments.theirs");
        assert_eq!(side_key(false, true), "segments.oursRebase");
        assert_eq!(side_key(true, true), "segments.theirsRebase");
        let zh = |key: &'static str| mt_i18n::t_in(mt_i18n::Locale::Zh, "gitCommit", key);
        assert!(zh(side_key(true, true)).contains("你的改动"));
        assert!(zh(pick_key(ConflictPick::Theirs, true)).contains("你的改动"));
        assert!(zh(pick_key(ConflictPick::Ours, true)).contains("变基目标"));
        assert!(zh(pick_key(ConflictPick::TheirsThenOurs, true)).contains("你的改动在前"));
        assert!(zh(pick_key(ConflictPick::Ours, false)).contains("当前"));
        // 八个选择文案、七个原因各不相同,且两种语言都在
        let mut keys: Vec<&str> = [false, true]
            .into_iter()
            .flat_map(|rebasing| PICKS.map(|pick| pick_key(pick, rebasing)))
            .collect();
        keys.extend(
            [
                SegmentBlocker::DeleteModify,
                SegmentBlocker::NotConflicted,
                SegmentBlocker::Unreadable,
                SegmentBlocker::TooLarge,
                SegmentBlocker::Binary,
                SegmentBlocker::Lossy,
                SegmentBlocker::NoMarkers,
            ]
            .map(blocker_key),
        );
        let unique: HashSet<&str> = keys.iter().copied().collect();
        assert_eq!(unique.len(), keys.len());
        for key in keys {
            for locale in mt_i18n::Locale::ALL {
                assert!(mt_i18n::lookup(locale, "gitCommit", key).is_some(), "{key}");
            }
        }
    }

    /// 显示用的文字:CRLF 收成 LF、Tab 展开、末尾换行去掉;空行照样占一行。
    #[test]
    fn 原文框显示的文字() {
        assert_eq!(shown_text("a\r\nb\r\n"), "a\nb");
        assert_eq!(shown_text("\tx\n"), "    x");
        assert_eq!(shown_text("no eol"), "no eol");
        assert_eq!(shown_text("\n"), "");
        assert_eq!(shown_text("a\n\nb\n"), "a\n\nb");
    }

    fn parsed(text: &str) -> ConflictFile {
        let root = std::env::temp_dir().join(format!(
            "mini-term-segments-carry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@e"])
                .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
                .args(args)
                .current_dir(&root)
                .env("GIT_EDITOR", "true")
                .output()
                .unwrap();
            out.status.success()
        };
        assert!(run(&["init", "-q"]));
        std::fs::write(root.join("f.txt"), "base\n").unwrap();
        assert!(run(&["add", "f.txt"]));
        assert!(run(&["commit", "-q", "-m", "base", "--no-verify"]));
        assert!(run(&["checkout", "-q", "-b", "other"]));
        std::fs::write(root.join("f.txt"), "theirs\n").unwrap();
        assert!(run(&["commit", "-q", "-am", "t", "--no-verify"]));
        assert!(run(&["checkout", "-q", "-"]));
        std::fs::write(root.join("f.txt"), "ours\n").unwrap();
        assert!(run(&["commit", "-q", "-am", "o", "--no-verify"]));
        assert!(!run(&["-c", "merge.conflictStyle=merge", "merge", "other"]));
        // 冲突在暂存区里;工作区内容换成要测的文本
        std::fs::write(root.join("f.txt"), text).unwrap();
        let file = git::load_conflict_file(&root, "f.txt").unwrap();
        std::fs::remove_dir_all(&root).ok();
        file
    }

    /// 刷新后沿用已选:按内容认段,不按序号(前面一段在终端里解决掉了,后面那段照样认得)。
    #[test]
    fn 刷新后按内容沿用已选() {
        let two = "<<<<<<< a\n1\n=======\n2\n>>>>>>> b\nmid\n<<<<<<< a\n3\n=======\n4\n>>>>>>> b\n";
        let old = parsed(two);
        let picks = [Some(ConflictPick::Ours), Some(ConflictPick::TheirsThenOurs)];
        assert_eq!(carry_picks(Some(&old), &picks, &old), picks.to_vec());
        let one = "1\nmid\n<<<<<<< a\n3\n=======\n4\n>>>>>>> b\n";
        let new = parsed(one);
        assert_eq!(
            carry_picks(Some(&old), &picks, &new),
            vec![Some(ConflictPick::TheirsThenOurs)]
        );
        // 内容变了的段不沿用;没有旧文件时全是没选
        let changed = parsed("<<<<<<< a\n1\n=======\n2!\n>>>>>>> b\n");
        assert_eq!(carry_picks(Some(&old), &picks, &changed), vec![None]);
        assert_eq!(carry_picks(None, &[], &new), vec![None]);
    }
}
