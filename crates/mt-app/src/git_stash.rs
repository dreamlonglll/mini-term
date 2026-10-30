//! 贮藏(git stash)弹窗,对标 IDEA 的 Stash / Unstash:
//!
//! - [`open_list`]:贮藏列表(左栏)+ 所选贮藏的改动文件与 diff(右栏);应用 / 弹出 /
//!   恢复暂存区状态 / 从此新建分支 / 删除 / 全部清除;没有贮藏时收成一个小空态框;
//! - [`open_save`]:新建贮藏(消息 / 保留暂存区 / 含未跟踪文件);
//! - [`open_smart_checkout`]:检出被本地改动挡住时的「贮藏后检出」确认框(IDEA 的
//!   Smart Checkout),后端是 [`git::checkout_with_stash`]。
//!
//! 入口都在 Git 面板:仓库栏的贮藏按钮([`open_list`])、更改区提交按钮旁 ▾ 菜单的
//! 「贮藏当前改动…」([`open_save`])、分支弹窗检出前的后台预检([`open_smart_checkout`])。
//! 本模块不碰面板;操作之后调 `on_changed` 让面板重载 —— 失败也调,冲突 / 半途失败同样
//! 动过工作区。
//!
//! # 视觉
//!
//! 件一律取自 [`git_ui`],尺寸与单文件历史弹窗(`git_file_history`)、推送对话框
//! (`git_push`)同一套:列表弹窗左栏 320、左栏头部与右栏工具栏同为 48px、文件行 28
//! (贮藏行两行 52,与单文件历史的提交行同高)、字号 11 / 12 / 13 / 14 四档;小弹窗的
//! 宽度走 `git_ui::DIALOG_W_*` 三档(确认 420 / 表单 460 / 内容多的 560)。
//!
//! # 结果就地显示
//!
//! toast 在弹窗遮罩下看不见,弹窗里的操作结果一律**就地**显示:成功一行说明;冲突黄字并
//! 写明「贮藏已保留」;失败是可选中复制的 git 原文。等待期间弹窗被 Esc 关掉的话,失败
//! 退到 [`git_ui::show_git_error`]。
//!
//! # 防重入与迟到
//!
//! 同一时刻只跑一个改写操作(`busy`;进行中按钮置灰、菜单项禁用)。改写操作都占着仓库的
//! 写操作闸(`git_ui::gate`):面板在拉取、更改区在提交时这里的按钮同样置灰,硬点进来
//! 也只就地说明、不执行。列表与文件列表的读取
//! 各带请求令牌,迟到的旧结果直接丢弃(连按刷新、快速 ↑↓ 切换贮藏时不会被旧结果盖掉)。
//! 改写操作都带上所选贮藏的哈希,后端核对 `stash@{n}` 没有漂移(终端里又贮藏过)。
//!
//! # 键盘
//!
//! 列表弹窗:↑/↓ 换贮藏、Enter 应用(Dialog 的 Confirm)、Delete 删除(先确认)、Esc 关闭;
//! 没有贮藏时 Enter = 贮藏当前改动。其余弹窗 Enter 确认 / Esc 取消。

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Div, Entity, FocusHandle,
    FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Render, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, Subscription,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::dialog::Dialog;
use gpui_component::input::{Input, InputEvent, InputState, TextareaState};
use mt_project::git::{
    self, BranchNameIssue, GitStatus, SmartCheckoutOutcome, StashApplyOutcome, StashFileInfo,
    StashInfo, StashSaveOptions, StashSaveOutcome,
};
use mt_ui::icons::Shape;
use mt_ui::tooltip::TooltipExt as _;

use crate::git_diff::{self, CommitFileDiffPane};
use crate::git_history::{format_absolute_time, format_relative_time};
use crate::git_ui::{
    self, ButtonKind, FormOp, ICON_ALERT, ICON_CHECK, ICON_CLEAN, ICON_PLUS, ICON_REFRESH,
    ICON_STASH, ICON_TRASH, SectionHeader, Tone, error_text, mono,
};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{autofocus, close_guarded, is_open, kind, open_guarded};
use crate::store::AppStore;
use crate::ui;

gpui::actions!(
    mini_term,
    [
        /// 贮藏列表:选中上一个贮藏(↑)
        GitStashPrev,
        /// 贮藏列表:选中下一个贮藏(↓)
        GitStashNext,
        /// 贮藏列表:删除选中的贮藏(Delete,先确认)
        GitStashDrop,
    ]
);

/// 贮藏列表弹窗的键位上下文(`hotkeys.rs` 按它绑 ↑ / ↓ / Delete)。
pub const STASH_CONTEXT: &str = "GitStash";

/// 操作之后让 Git 面板重载的回调。
type OnChanged = Rc<dyn Fn(&mut App)>;
/// 贮藏成功之后的回调。参数是新贮藏的引用 `stash@{0}`,以及「已贮藏、但有文件未能清理」
/// 时 git 的原文(见 [`StashSaveOutcome::SavedWithWarnings`])。
type OnSaved = Rc<dyn Fn(String, Option<String>, &mut Window, &mut App)>;

/// 贮藏成功的说明;带警告时把「有文件未能清理」与 git 原文一起写进去。
fn saved_text(reference: &str, warning: Option<&str>) -> String {
    match warning {
        Some(detail) => tr!(
            "gitStash",
            "save.savedWithWarnings",
            reference = reference,
            detail = detail
        ),
        None => tr!("gitStash", "save.saved", reference = reference),
    }
}

/// 左栏宽度(与单文件历史弹窗同宽)。
const LIST_WIDTH: f32 = 320.0;
/// 贮藏行高(两行布局,与单文件历史的提交行同高)。uniform_list 要求等高。
const ROW_HEIGHT: f32 = 52.0;
/// 改动文件区最多露出几行,再多就滚动 —— 下面的 diff 才是主角。
const FILE_ROWS_VISIBLE: usize = 7;
/// 冲突 / 挡住检出的文件最多列几个,其余收成「…以及另外 N 个」。
const PREVIEW_FILES: usize = 6;

// ─── 状态 ─────────────────────────────────────────────────────

/// 进行中的改写操作。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Busy {
    Apply,
    Pop,
    Drop,
    Clear,
}

impl Busy {
    fn text_key(self) -> &'static str {
        match self {
            Busy::Apply => "busy.apply",
            Busy::Pop => "busy.pop",
            Busy::Drop => "busy.drop",
            Busy::Clear => "busy.clear",
        }
    }
}

/// 一次操作的结果,显示在右栏操作条下面。
enum Notice {
    Info(String),
    /// 冲突:说明 + 冲突文件。
    Attention {
        text: String,
        files: Vec<String>,
    },
    /// 失败:可选中复制的 git 原文。
    Error(Entity<TextareaState>),
}

struct StashDialog {
    store: Entity<AppStore>,
    repo_path: String,
    on_changed: OnChanged,
    stashes: Vec<StashInfo>,
    list_loading: bool,
    list_error: Option<String>,
    /// 列表读取的请求令牌:只认最后一次发出的那个。
    list_request: u64,
    /// 下一次列表回来时选中最新的那个(刚在本弹窗里贮藏过)。
    select_newest: bool,
    selected: Option<usize>,
    /// 当前文件列表属于哪个贮藏(哈希)。
    files_hash: Option<String>,
    files: Vec<StashFileInfo>,
    files_loading: bool,
    files_error: Option<String>,
    files_request: u64,
    selected_file: Option<usize>,
    /// 「恢复暂存区状态」(`--index`)。
    reinstate_index: bool,
    busy: Option<Busy>,
    notice: Option<Notice>,
    diff: CommitFileDiffPane,
    scroll: UniformListScrollHandle,
    files_scroll: UniformListScrollHandle,
    focus: FocusHandle,
}

impl Render for StashDialog {
    /// 只当状态盒子用(Dialog 的 builder 每帧重跑)。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl StashDialog {
    fn selected_stash(&self) -> Option<&StashInfo> {
        self.selected.and_then(|i| self.stashes.get(i))
    }

    /// 改写类按钮要不要锁住:自己有操作在跑,或别的区占着仓库的写操作闸。
    fn locked(&self) -> bool {
        self.busy.is_some() || git_ui::gate::is_busy(&self.repo_path)
    }
}

/// 小弹窗(新建贮藏 / 新建分支 / 贮藏后检出)的表单状态。
struct Form {
    /// 执行中 / 上一次执行失败的 git 原文(可选中复制)。
    op: FormOp<Entity<TextareaState>>,
    /// 非错误的就地提示(「没有可贮藏的改动」)。
    hint: Option<String>,
    /// 点过确认(此后空的必填项才报错)。
    submitted: bool,
    /// 用户动过名字输入框(此后清空也就地报错,与分支弹窗的新建 / 重命名同一口径)。
    touched: bool,
    keep_index: bool,
    include_untracked: bool,
    /// 贮藏后检出的终态(已检出、但恢复不顺):说明 + 文件 + 原文。
    outcome: Option<(String, Vec<String>, Option<Entity<TextareaState>>)>,
    /// 盯名字输入框的改动(置 `touched`)。只有「从贮藏新建分支」挂。
    _sub: Option<Subscription>,
}

impl Form {
    fn new() -> Self {
        Self {
            op: FormOp::default(),
            hint: None,
            submitted: false,
            touched: false,
            keep_index: false,
            include_untracked: true,
            outcome: None,
            _sub: None,
        }
    }
}

impl Render for Form {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

// ─── 入口 ─────────────────────────────────────────────────────

/// 打开贮藏列表弹窗。`repo_path` 是仓库根(Git 面板当前选中的仓库)。
///
/// 先在后台把列表读回来再开窗:空列表是个小空态框、非空是大弹窗,开窗后再变尺寸会闪一下。
pub fn open_list(
    store: Entity<AppStore>,
    repo_path: String,
    on_changed: Rc<dyn Fn(&mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    if repo_path.is_empty() || is_open(kind::GIT_STASH) {
        return;
    }
    let diff = CommitFileDiffPane::new(&store, cx);
    let state = cx.new(|cx| StashDialog {
        store,
        repo_path: repo_path.clone(),
        on_changed,
        stashes: Vec::new(),
        list_loading: true,
        list_error: None,
        list_request: 1,
        select_newest: false,
        selected: None,
        files_hash: None,
        files: Vec::new(),
        files_loading: false,
        files_error: None,
        files_request: 0,
        selected_file: None,
        reinstate_index: false,
        busy: None,
        notice: None,
        diff,
        scroll: UniformListScrollHandle::new(),
        files_scroll: UniformListScrollHandle::new(),
        focus: cx.focus_handle(),
    });
    let repo = PathBuf::from(repo_path);
    window
        .spawn(cx, async move |cx| {
            let result = cx
                .background_executor()
                .spawn(async move { git::list_stashes(&repo) })
                .await;
            let _ = cx.update(|window, cx| {
                // 读取期间入口又被点了一次,那一次已经把弹窗开出来了
                if is_open(kind::GIT_STASH) {
                    return;
                }
                apply_list(&state, 1, result, cx);
                show_list_dialog(state, window, cx);
            });
        })
        .detach();
}

/// 打开「贮藏改动」弹窗。成功后 toast「已贮藏为 stash@{0}」并调 `on_changed`;
/// 没有可贮藏的改动时就地提示、弹窗留着。
pub fn open_save(
    store: Entity<AppStore>,
    repo_path: String,
    on_changed: Rc<dyn Fn(&mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    let toast_store = store.clone();
    let on_saved = Rc::new(
        move |reference: String, warning: Option<String>, _window: &mut Window, cx: &mut App| {
            // 有文件没清掉:需要留意那一档,别让人以为工作区已经干净了
            let tone = if warning.is_some() {
                Tone::Attention
            } else {
                Tone::Info
            };
            git_ui::toast(
                &toast_store,
                saved_text(&reference, warning.as_deref()),
                tone,
                cx,
            );
            on_changed(cx);
        },
    );
    open_save_with(repo_path, on_saved, window, cx);
}

/// 检出 `branch` 被本地改动挡住时的「贮藏后检出」确认框。
///
/// `conflicts` 是挡住检出的文件(调用方事先用 [`git::local_changes_conflict_with`] 在
/// 后台算好;空的话正文换成一句笼统的说明)。确认后后台跑 [`git::checkout_with_stash`]:
/// 顺利 → 关框 + toast;恢复时冲突 / 失败 → 框里显示结果(写明贮藏已保留)只剩「知道了」;
/// 检出本身失败 → 就地红字(贮藏已恢复回去)。每种结局都调 `on_changed`。
pub fn open_smart_checkout(
    store: Entity<AppStore>,
    repo_path: String,
    branch: String,
    conflicts: Vec<String>,
    on_changed: Rc<dyn Fn(&mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    if repo_path.is_empty() || is_open(kind::GIT_STASH_CHECKOUT) {
        return;
    }
    let form = cx.new(|_| Form::new());
    open_guarded(
        kind::GIT_STASH_CHECKOUT,
        window,
        cx,
        move |dialog, _window, cx| {
            let title = tr!("gitStash", "checkout.title", branch = branch.as_str());
            let f = form.read(cx);
            if let Some((text, files, detail)) = f.outcome.clone() {
                // 终态:已经检出,但恢复不顺 —— 只剩「知道了」
                return dialog
                    .title(title)
                    .w(px(git_ui::DIALOG_W_LG))
                    .overlay_closable(true)
                    .footer(git_ui::dialog_footer(
                        t("gitStash", "checkout.gotIt"),
                        None,
                        false,
                        false,
                    ))
                    .child(
                        git_ui::form_body()
                            .child(git_ui::notice_line(text, ui::color_warning()))
                            .when(!files.is_empty(), |el| el.child(file_list(&files)))
                            .when_some(detail, |el, detail| {
                                el.child(git_ui::error_text_box(&detail))
                            }),
                    )
                    .on_ok(|_, _, _| true);
            }
            let (busy, error) = (f.op.busy, f.op.error.clone());
            let lead = if conflicts.is_empty() {
                t("gitStash", "checkout.leadUnknown")
            } else {
                t("gitStash", "checkout.lead")
            };
            let (form_ok, store, repo, target, on_changed) = (
                form.clone(),
                store.clone(),
                repo_path.clone(),
                branch.clone(),
                on_changed.clone(),
            );
            let form_cancel = form.clone();
            dialog
                .title(title)
                .w(px(git_ui::DIALOG_W_LG))
                // 执行中(贮藏 → 检出 → 恢复,三条改仓库的命令)框一律关不掉:遮罩、Esc、
                // 「取消」都不行 —— 结局(恢复冲突 / 失败)要留在框里给人看。并发的写操作
                // 由仓库的写操作闸挡住(这一段不经面板的 busy,但占着闸),不会撞 index.lock
                .overlay_closable(!busy)
                .keyboard(!busy)
                .on_cancel(move |_: &ClickEvent, _window, cx| !form_cancel.read(cx).op.busy)
                .footer(git_ui::dialog_footer(
                    if busy {
                        t("gitStash", "checkout.working")
                    } else {
                        t("gitStash", "checkout.ok")
                    },
                    Some(t("gitStash", "confirm.cancel").into()),
                    false,
                    busy,
                ))
                .child(
                    git_ui::form_body()
                        .child(
                            div()
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::text_primary())
                                .child(lead),
                        )
                        .when(!conflicts.is_empty(), |el| el.child(file_list(&conflicts)))
                        .child(git_ui::hint_line(tr!(
                            "gitStash",
                            "checkout.explain",
                            branch = branch.as_str()
                        )))
                        .when(busy, |el| {
                            el.child(git_ui::busy_line(t("gitStash", "checkout.working")))
                        })
                        .when_some(error, |el, error| el.child(git_ui::error_text_box(&error))),
                )
                .on_ok(move |_: &ClickEvent, window, cx| {
                    submit_smart_checkout(
                        &form_ok,
                        store.clone(),
                        repo.clone(),
                        target.clone(),
                        on_changed.clone(),
                        window,
                        cx,
                    );
                    false
                })
        },
    );
}

// ─── 列表弹窗:数据 ───────────────────────────────────────────

/// 重新读列表(操作之后 / 刷新钮)。
fn reload(state: &Entity<StashDialog>, cx: &mut App) {
    let (repo, request) = state.update(cx, |s, cx| {
        s.list_request += 1;
        s.list_loading = true;
        cx.notify();
        (PathBuf::from(&s.repo_path), s.list_request)
    });
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { git::list_stashes(&repo) })
            .await;
        cx.update(|cx| apply_list(&state, request, result, cx));
    })
    .detach();
}

/// 收下一次列表读取的结果(令牌对不上的迟到结果直接丢)。
fn apply_list(
    state: &Entity<StashDialog>,
    request: u64,
    result: anyhow::Result<Vec<StashInfo>>,
    cx: &mut App,
) {
    let picked = state.update(cx, |s, cx| {
        if s.list_request != request {
            return None;
        }
        s.list_loading = false;
        let keep = s.selected_stash().map(|st| st.hash.clone());
        match result {
            Ok(list) => {
                s.stashes = list;
                s.list_error = None;
            }
            Err(err) => {
                eprintln!("[git] 读取贮藏失败: {err:#}");
                s.list_error = Some(format!("{err:#}"));
            }
        }
        let prefer_newest = std::mem::take(&mut s.select_newest);
        s.selected = reselect(&s.stashes, keep.as_deref(), s.selected, prefer_newest);
        if let Some(ix) = s.selected {
            s.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
        Some(s.selected_stash().map(|st| st.hash.clone()))
    });
    if let Some(hash) = picked {
        load_files(state, hash, cx);
    }
}

/// 列表刷新后选中谁:刚贮藏过选最新的;原先选中的还在就跟着它(下标会漂移);
/// 没了就落在原位置附近;列表空了就是 `None`。
fn reselect(
    stashes: &[StashInfo],
    keep_hash: Option<&str>,
    prev: Option<usize>,
    prefer_newest: bool,
) -> Option<usize> {
    if stashes.is_empty() {
        return None;
    }
    if prefer_newest {
        return Some(0);
    }
    keep_hash
        .and_then(|hash| stashes.iter().position(|s| s.hash == hash))
        .or_else(|| Some(prev.unwrap_or(0).min(stashes.len() - 1)))
}

/// 换成 `hash` 这个贮藏的文件列表(同一个贮藏不重取 —— 贮藏提交不可变)。
fn load_files(state: &Entity<StashDialog>, hash: Option<String>, cx: &mut App) {
    let Some((repo, request)) = state.update(cx, |s, cx| {
        if s.files_hash == hash {
            return None;
        }
        s.files_hash = hash.clone();
        s.files.clear();
        s.selected_file = None;
        s.files_error = None;
        s.files_request += 1;
        s.files_loading = hash.is_some();
        cx.notify();
        Some((PathBuf::from(&s.repo_path), s.files_request))
    }) else {
        return;
    };
    let Some(hash) = hash else {
        return;
    };
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { git::stash_files(&repo, &hash) })
            .await;
        cx.update(|cx| {
            let any = state.update(cx, |s, cx| {
                if s.files_request != request {
                    return false;
                }
                s.files_loading = false;
                match result {
                    Ok(files) => s.files = files,
                    Err(err) => {
                        eprintln!("[git] 读取贮藏内容失败: {err:#}");
                        s.files_error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
                !s.files.is_empty()
            });
            if any {
                select_file(&state, 0, cx);
            }
        });
    })
    .detach();
}

/// 选中第 `index` 个改动文件:右下换成它的 diff。
fn select_file(state: &Entity<StashDialog>, index: usize, cx: &mut App) {
    let target = state.update(cx, |s, cx| {
        let file = s.files.get(index)?;
        s.selected_file = Some(index);
        s.files_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        cx.notify();
        Some((
            s.diff.clone(),
            s.repo_path.clone(),
            file.diff_hash.clone(),
            file.file.path.clone(),
            file.file.old_path.clone(),
        ))
    });
    if let Some((diff, repo, hash, path, old_path)) = target {
        diff.load(&repo, &hash, &path, old_path, cx);
    }
}

/// 选中第 `index` 个贮藏。
fn select(state: &Entity<StashDialog>, index: usize, cx: &mut App) {
    let hash = state.update(cx, |s, cx| {
        let hash = s.stashes.get(index)?.hash.clone();
        s.selected = Some(index);
        s.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        cx.notify();
        Some(hash)
    });
    if hash.is_some() {
        load_files(state, hash, cx);
    }
}

/// ↑/↓ 之后的下标(到头不绕圈)。空列表为 `None`。
fn step(len: usize, current: Option<usize>, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        None => 0,
        Some(cur) => (cur as isize + delta).clamp(0, len as isize - 1) as usize,
    })
}

fn move_selection(state: &Entity<StashDialog>, delta: isize, cx: &mut App) {
    let (len, cur) = {
        let s = state.read(cx);
        (s.stashes.len(), s.selected)
    };
    if let Some(next) = step(len, cur, delta)
        && Some(next) != cur
    {
        select(state, next, cx);
    }
}

/// 把焦点还给列表(叠在上面的确认框 / 输入框关掉之后,↑↓ 才接得着)。
fn refocus(state: &Entity<StashDialog>, window: &mut Window, cx: &mut App) {
    let focus = state.read(cx).focus.clone();
    window.defer(cx, move |window, cx| window.focus(&focus, cx));
}

// ─── 列表弹窗:操作 ───────────────────────────────────────────

/// 跑一个改写操作:防重入 → 占闸、后台执行([`git_ui::spawn_gated`])→ 结果写成
/// [`Notice`] → 通知面板、重读列表。闸被别的区占着时不执行,结果行就地说明。
fn run_op<T: Send + 'static>(
    state: &Entity<StashDialog>,
    busy: Busy,
    op: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
    on_ok: impl FnOnce(T) -> Notice + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let (running, repo) = {
        let s = state.read(cx);
        (s.busy.is_some(), s.repo_path.clone())
    };
    if running {
        return;
    }
    let done_state = state.clone();
    let started = git_ui::spawn_gated(
        &repo,
        op,
        move |result, window, cx| {
            let state = done_state;
            let notice = match result {
                Ok(value) => on_ok(value),
                Err(err) => {
                    let detail = error_text(&err);
                    eprintln!("[git] 贮藏操作 {busy:?} 失败: {detail}");
                    // 等待期间弹窗被关掉了:原因不能丢
                    if !is_open(kind::GIT_STASH) {
                        git_ui::show_git_error(
                            t("gitStash", "result.failedTitle"),
                            detail.clone(),
                            window,
                            cx,
                        );
                    }
                    Notice::Error(git_ui::error_text_state(&detail, 8, window, cx))
                }
            };
            // 等待期间弹窗被关掉了:冲突(贮藏已保留)这句不能丢,退到 toast
            if !is_open(kind::GIT_STASH)
                && let Notice::Attention { text, .. } = &notice
            {
                let store = state.read(cx).store.clone();
                git_ui::toast(&store, text.clone(), Tone::Attention, cx);
            }
            let on_changed = state.update(cx, |s, cx| {
                s.busy = None;
                s.notice = Some(notice);
                cx.notify();
                s.on_changed.clone()
            });
            // 失败也可能动过工作区(应用到一半):面板一律重载
            on_changed(cx);
            reload(&state, cx);
        },
        window,
        cx,
    );
    let notice = if started {
        None
    } else {
        Some(Notice::Error(git_ui::error_text_state(
            git_ui::gate_busy_text(),
            2,
            window,
            cx,
        )))
    };
    state.update(cx, |s, cx| {
        if started {
            s.busy = Some(busy);
        }
        s.notice = notice;
        cx.notify();
    });
    refocus(state, window, cx);
}

/// 应用 / 弹出所选贮藏。
fn apply_selected(state: &Entity<StashDialog>, pop: bool, window: &mut Window, cx: &mut App) {
    let s = state.read(cx);
    let reinstate = s.reinstate_index;
    let Some((index, hash, reference)) = s
        .selected_stash()
        .map(|st| (st.index, st.hash.clone(), st.reference.clone()))
    else {
        return;
    };
    let busy = if pop { Busy::Pop } else { Busy::Apply };
    run_op(
        state,
        busy,
        move |repo| {
            if pop {
                git::stash_pop(repo, index, reinstate, Some(&hash))
            } else {
                git::stash_apply(repo, index, reinstate, Some(&hash))
            }
        },
        move |outcome| match outcome {
            StashApplyOutcome::Applied => Notice::Info(tr!(
                "gitStash",
                if pop {
                    "result.popped"
                } else {
                    "result.applied"
                },
                reference = reference
            )),
            StashApplyOutcome::Conflicts { files } => Notice::Attention {
                text: t("gitStash", "result.conflicts").to_string(),
                files,
            },
        },
        window,
        cx,
    );
}

/// 删除所选贮藏(先确认)。
fn confirm_drop(state: &Entity<StashDialog>, window: &mut Window, cx: &mut App) {
    let s = state.read(cx);
    if s.busy.is_some() {
        return;
    }
    let Some(stash) = s.selected_stash().cloned() else {
        return;
    };
    let state = state.clone();
    danger_confirm(
        t("gitStash", "confirm.dropTitle").into(),
        tr!(
            "gitStash",
            "confirm.dropMessage",
            reference = stash.reference.as_str(),
            message = display_summary(&stash)
        ),
        t("gitStash", "confirm.dropOk").into(),
        move |window, cx| {
            let (index, hash, reference) =
                (stash.index, stash.hash.clone(), stash.reference.clone());
            run_op(
                &state,
                Busy::Drop,
                move |repo| git::stash_drop(repo, index, Some(&hash)),
                move |()| Notice::Info(tr!("gitStash", "result.dropped", reference = reference)),
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

/// 全部清除(确认框写明数量)。
fn confirm_clear(state: &Entity<StashDialog>, window: &mut Window, cx: &mut App) {
    let s = state.read(cx);
    if s.busy.is_some() || s.stashes.is_empty() {
        return;
    }
    let count = s.stashes.len();
    let state = state.clone();
    danger_confirm(
        t("gitStash", "confirm.clearTitle").into(),
        tr!(
            "gitStash",
            "confirm.clearMessage",
            count = count.to_string()
        ),
        t("gitStash", "confirm.clearOk").into(),
        move |window, cx| {
            run_op(
                &state,
                Busy::Clear,
                git::stash_clear,
                |()| Notice::Info(t("gitStash", "result.cleared").to_string()),
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

/// 复制所选贮藏的完整哈希。
fn copy_hash(state: &Entity<StashDialog>, cx: &mut App) {
    if let Some(hash) = state.read(cx).selected_stash().map(|s| s.hash.clone()) {
        cx.write_to_clipboard(ClipboardItem::new_string(hash));
    }
}

/// 列表弹窗里的回车:有贮藏 = 应用所选;没有 = 贮藏当前改动。
fn on_enter(state: &Entity<StashDialog>, window: &mut Window, cx: &mut App) {
    let s = state.read(cx);
    if s.busy.is_some() {
        return;
    }
    if s.stashes.is_empty() {
        open_save_from_list(state, window, cx);
    } else {
        apply_selected(state, false, window, cx);
    }
}

/// 从列表弹窗里打开「贮藏改动」:成功后面板重载、列表重读并选中新贮藏。
/// toast 在列表弹窗的遮罩下看不见,结果写进列表的结果行。
fn open_save_from_list(state: &Entity<StashDialog>, window: &mut Window, cx: &mut App) {
    let repo = state.read(cx).repo_path.clone();
    let list = state.clone();
    let on_saved = Rc::new(
        move |reference: String, warning: Option<String>, window: &mut Window, cx: &mut App| {
            let text = saved_text(&reference, warning.as_deref());
            let on_changed = list.update(cx, |s, cx| {
                s.select_newest = true;
                s.notice = Some(if warning.is_some() {
                    Notice::Attention {
                        text,
                        files: Vec::new(),
                    }
                } else {
                    Notice::Info(text)
                });
                cx.notify();
                s.on_changed.clone()
            });
            on_changed(cx);
            reload(&list, cx);
            refocus(&list, window, cx);
        },
    );
    open_save_with(repo, on_saved, window, cx);
}

// ─── 列表弹窗:渲染 ───────────────────────────────────────────

fn show_list_dialog(state: Entity<StashDialog>, window: &mut Window, cx: &mut App) {
    let focus = state.read(cx).focus.clone();
    open_guarded(kind::GIT_STASH, window, cx, move |dialog, window, cx| {
        let ok_state = state.clone();
        let dialog = dialog.on_ok(move |_: &ClickEvent, window, cx| {
            on_enter(&ok_state, window, cx);
            false
        });
        let s = state.read(cx);
        // 没有贮藏(读失败除外)收成小空态框。不看 `list_loading`:空列表里贮藏一次之后的
        // 那次重读,会先撑成大框、读回来再变 —— 闪一下
        if s.stashes.is_empty() && s.list_error.is_none() {
            return render_empty_dialog(dialog, &state, cx);
        }
        let (prev, next, drop) = (state.clone(), state.clone(), state.clone());
        let focus = s.focus.clone();
        let (width, body_h) = git_diff::modal_size(window.viewport_size());
        dialog.w(width).p_0().child(
            div()
                .w_full()
                .h(body_h)
                .flex()
                .overflow_hidden()
                .key_context(STASH_CONTEXT)
                .track_focus(&focus)
                .on_action(move |_: &GitStashPrev, _window, cx| move_selection(&prev, -1, cx))
                .on_action(move |_: &GitStashNext, _window, cx| move_selection(&next, 1, cx))
                .on_action(move |_: &GitStashDrop, window, cx| confirm_drop(&drop, window, cx))
                .child(render_list_column(&state, cx))
                .child(render_detail_column(&state, cx)),
        )
    });
    // 方向键要落在弹窗里:等弹窗抢完焦点再交给列表(同 `prompt::autofocus`)
    window.defer(cx, move |window, cx| window.focus(&focus, cx));
}

/// 没有贮藏:小框(右上角 ✕)+ 空态 +「贮藏当前改动…」。
fn render_empty_dialog(dialog: Dialog, state: &Entity<StashDialog>, cx: &mut App) -> Dialog {
    let focus = state.read(cx).focus.clone();
    let save_state = state.clone();
    dialog
        .w(px(git_ui::DIALOG_W_SM))
        .title(git_ui::dialog_title(
            kind::GIT_STASH,
            t("gitStash", "title"),
        ))
        .overlay_closable(true)
        .child(
            div()
                .key_context(STASH_CONTEXT)
                .track_focus(&focus)
                .child(git_ui::empty_state(
                    ICON_STASH,
                    t("gitStash", "empty"),
                    Some(
                        git_ui::action_button(
                            "git-stash-empty-save",
                            t("gitStash", "stashChanges"),
                            ButtonKind::Primary,
                            true,
                        )
                        .on_click(move |_: &ClickEvent, window, cx| {
                            open_save_from_list(&save_state, window, cx)
                        })
                        .into_any_element(),
                    ),
                )),
        )
}

fn render_list_column(state: &Entity<StashDialog>, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let count = s.stashes.len();
    let busy = s.locked();
    let can_clear = !busy && count > 0;

    // 头部与右栏工具栏同高(48):图标 + 14px 半粗标题 + 数量胶囊,右端三颗图标按钮
    let (new_state, refresh_state, clear_state) = (state.clone(), state.clone(), state.clone());
    let buttons = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.0))
        .child(
            git_ui::icon_button(
                "git-stash-new",
                ICON_PLUS,
                t("gitStash", "stashChanges"),
                !busy,
            )
            .when(!busy, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| {
                    open_save_from_list(&new_state, window, cx)
                })
            }),
        )
        .child(
            git_ui::busy_icon_button(
                "git-stash-refresh",
                ICON_REFRESH,
                t("gitStash", "refresh"),
                s.list_loading,
                false,
            )
            .when(!s.list_loading, |el| {
                el.on_click(move |_: &ClickEvent, _window, cx| reload(&refresh_state, cx))
            }),
        )
        .child(
            git_ui::danger_icon_button(
                "git-stash-clear",
                ICON_TRASH,
                t("gitStash", "clearAll"),
                can_clear,
            )
            .when(can_clear, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| {
                    confirm_clear(&clear_state, window, cx)
                })
            }),
        );
    let header = div()
        .flex_none()
        .h(px(git_diff::TOOLBAR_H))
        .pl(px(git_diff::SIDE_PAD))
        .pr(px(8.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(git_ui::icon(ICON_STASH, git_ui::ICON_SIZE, ui::accent()))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(14.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui::text_primary())
                .child(t("gitStash", "title")),
        )
        .child(git_ui::count_pill(count, ui::text_muted()))
        .child(div().flex_1())
        .child(buttons);

    let body: AnyElement = match s.list_error.clone() {
        Some(err) if count == 0 => div()
            .flex_1()
            .min_h(px(0.0))
            .child(git_ui::error_state(tr!(
                "gitStash",
                "loadFailed",
                detail = err
            )))
            .into_any_element(),
        _ => {
            let list_state = state.clone();
            let scroll = s.scroll.clone();
            let now = chrono::Utc::now().timestamp();
            div()
                .flex_1()
                .min_h(px(0.0))
                .p(px(4.0))
                .child(
                    uniform_list(
                        "git-stash-list",
                        count,
                        move |range, _window, cx: &mut App| {
                            range
                                .map(|i| render_row(&list_state, i, now, cx))
                                .collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&scroll)
                    .size_full(),
                )
                .into_any_element()
        }
    };

    let footer = div()
        .flex_none()
        .px(px(git_diff::SIDE_PAD))
        .py(px(8.0))
        .border_t_1()
        .border_color(ui::border_subtle())
        .text_size(ui::font_px(11.0))
        .text_color(ui::text_muted())
        .child(t("gitStash", "footerHint"));

    div()
        .w(px(LIST_WIDTH))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(ui::border_subtle())
        .bg(ui::bg_elevated())
        .child(header)
        .child(body)
        .child(footer)
        .into_any_element()
}

/// 行里显示的摘要:消息为空时给个占位。
fn display_summary(stash: &StashInfo) -> String {
    if stash.summary.is_empty() {
        t("gitStash", "noMessage").to_string()
    } else {
        stash.summary.clone()
    }
}

/// 7 位短哈希。
fn short_hash(hash: &str) -> &str {
    &hash[..7.min(hash.len())]
}

/// 「未跟踪」的颜色:与更改区的未跟踪分组同色。
fn untracked_color() -> Hsla {
    git_ui::status_color(Some(&GitStatus::Untracked))
}

fn render_row(state: &Entity<StashDialog>, index: usize, now: i64, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let Some(stash) = s.stashes.get(index) else {
        return div().into_any_element();
    };
    let selected = s.selected == Some(index);
    let busy = s.locked();
    let mut meta = vec![stash.reference.clone()];
    meta.extend(stash.branch.clone());
    meta.push(format_relative_time(stash.timestamp, now));
    let tip = format!(
        "{}\n{} · {}",
        stash.message,
        format_absolute_time(stash.timestamp),
        short_hash(&stash.hash)
    );
    let (click_state, menu_state) = (state.clone(), state.clone());

    // 共用列表行(选中 = accent 淡底 + 左侧竖条、悬停薄纱底),两行布局与单文件历史的
    // 提交行同一口径:摘要(+ WIP / 未跟踪胶囊)/ 引用 · 分支 · 时间
    git_ui::list_row(
        SharedString::from(format!("git-stash-row-{index}")),
        selected,
    )
    .h(px(ROW_HEIGHT))
    .flex_col()
    .items_stretch()
    .justify_center()
    .gap(px(4.0))
    .px(px(10.0))
    .child(
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_primary())
                    .child(display_summary(stash)),
            )
            // git 自动生成的消息(贮藏时没写说明)
            .when(stash.is_wip, |el| {
                el.child(git_ui::pill("WIP", ui::text_muted()))
            })
            .when(stash.has_untracked, |el| {
                el.child(git_ui::pill(t("gitStash", "untracked"), untracked_color()))
            }),
    )
    .child(
        mono(div())
            .min_w_0()
            .truncate()
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_muted())
            .child(meta.join(" · ")),
    )
    .tip(tip)
    .on_click(move |_: &ClickEvent, window, cx| {
        let focus = click_state.read(cx).focus.clone();
        window.focus(&focus, cx);
        if click_state.read(cx).selected != Some(index) {
            select(&click_state, index, cx);
        }
    })
    .on_mouse_down(
        MouseButton::Right,
        move |event: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            if menu_state.read(cx).selected != Some(index) {
                select(&menu_state, index, cx);
            }
            let state = menu_state.clone();
            let item = |label: &'static str, f: fn(&Entity<StashDialog>, &mut Window, &mut App)| {
                let state = state.clone();
                MenuItem::new(t("gitStash", label))
                    .disabled(busy)
                    .on_click(move |window, cx| f(&state, window, cx))
            };
            let copy_state = state.clone();
            let entries: Vec<MenuEntry> = vec![
                item("action.apply", |s, w, cx| apply_selected(s, false, w, cx))
                    .shortcut("Enter")
                    .into(),
                item("action.pop", |s, w, cx| apply_selected(s, true, w, cx)).into(),
                item("action.newBranch", open_branch_dialog).into(),
                menu::separator(),
                menu::item(t("gitStash", "copyHash"), move |_window, cx| {
                    copy_hash(&copy_state, cx)
                }),
                menu::separator(),
                item("action.drop", confirm_drop)
                    .shortcut("Delete")
                    .danger()
                    .into(),
                item("clearAll", confirm_clear).danger().into(),
            ];
            menu::show(event.position, entries, window, cx);
        },
    )
    .into_any_element()
}

fn render_detail_column(state: &Entity<StashDialog>, cx: &mut App) -> AnyElement {
    let diff = state.read(cx).diff.clone();
    let toolbar = diff.toolbar("git-stash-diff", kind::GIT_STASH, cx);
    let has_file = state.read(cx).selected_file.is_some();
    let diff_body = has_file.then(|| diff.body(cx));
    let s = state.read(cx);
    let stash = s.selected_stash().cloned();
    let busy = s.busy;
    let enabled = !s.locked() && stash.is_some();

    // ── 工具栏:所选贮藏 + 右上角 diff 工具组(含 ✕),与单文件历史 / 提交 diff 同高同距
    let title = div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(8.0))
        .when_some(stash.as_ref(), |el, stash| {
            el.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(ui::font_px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui::text_primary())
                    .child(display_summary(stash)),
            )
            .child(
                mono(div())
                    .flex_none()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(format!("{} · {}", stash.reference, short_hash(&stash.hash))),
            )
        });
    let head = div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(16.0))
        .h(px(git_diff::TOOLBAR_H))
        .px(px(16.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(title)
        .child(toolbar);

    // ── 操作条
    let (apply_state, pop_state, index_state, branch_state, drop_state) = (
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
    );
    let actions = div()
        .flex_none()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(8.0))
        .px(px(16.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(
            git_ui::action_button(
                "git-stash-apply",
                t("gitStash", "action.apply"),
                ButtonKind::Primary,
                enabled,
            )
            .tip(t("gitStash", "action.applyTip"))
            .when(enabled, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| {
                    apply_selected(&apply_state, false, window, cx)
                })
            }),
        )
        .child(
            git_ui::action_button(
                "git-stash-pop",
                t("gitStash", "action.pop"),
                ButtonKind::Ghost,
                enabled,
            )
            .tip(t("gitStash", "action.popTip"))
            .when(enabled, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| {
                    apply_selected(&pop_state, true, window, cx)
                })
            }),
        )
        .child(
            git_ui::check_row(
                "git-stash-reinstate",
                s.reinstate_index,
                t("gitStash", "action.reinstateIndex"),
                false,
            )
            .tip(t("gitStash", "action.reinstateIndexTip"))
            .on_click(move |_: &ClickEvent, _window, cx| {
                index_state.update(cx, |s, cx| {
                    s.reinstate_index = !s.reinstate_index;
                    cx.notify();
                })
            }),
        )
        .child(div().flex_1())
        .when_some(busy, |el, busy| {
            el.child(git_ui::busy_line(t("gitStash", busy.text_key())))
        })
        .child(
            git_ui::action_button(
                "git-stash-branch",
                t("gitStash", "action.newBranch"),
                ButtonKind::Ghost,
                enabled,
            )
            .tip(t("gitStash", "action.newBranchTip"))
            .when(enabled, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| {
                    open_branch_dialog(&branch_state, window, cx)
                })
            }),
        )
        .child(
            git_ui::action_button(
                "git-stash-drop",
                t("gitStash", "action.drop"),
                ButtonKind::Danger,
                enabled,
            )
            .tip(t("gitStash", "action.dropTip"))
            .when(enabled, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| confirm_drop(&drop_state, window, cx))
            }),
        );

    // ── 结果行
    let notice = s.notice.as_ref().map(render_notice);

    // ── 改动文件:二级分组标题 + 文件行(与提交 diff 左栏同一种文件行)
    let file_count = s.files.len();
    let mut files_header =
        SectionHeader::new("git-stash-files-header", t("gitStash", "files.title")).group();
    if !s.files_loading {
        files_header = files_header.count(file_count);
    }
    let files_body: Option<AnyElement> = if s.files_loading {
        Some(
            div()
                .px(px(12.0))
                .pb(px(8.0))
                .child(git_ui::busy_line(t("gitStash", "files.loading")))
                .into_any_element(),
        )
    } else if let Some(err) = s.files_error.clone() {
        Some(git_ui::error_state(tr!("gitStash", "files.failed", detail = err)).into_any_element())
    } else if file_count == 0 {
        None
    } else {
        let list_state = state.clone();
        let visible = file_count.min(FILE_ROWS_VISIBLE);
        Some(
            div()
                .h(px(visible as f32 * git_ui::ROW_H + 4.0))
                .px(px(4.0))
                .pb(px(4.0))
                .child(
                    uniform_list(
                        "git-stash-files",
                        file_count,
                        move |range, _window, cx: &mut App| {
                            range
                                .map(|i| render_file_row(&list_state, i, cx))
                                .collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&s.files_scroll)
                    .size_full(),
                )
                .into_any_element(),
        )
    };
    let files = div()
        .flex_none()
        .flex()
        .flex_col()
        .pt(px(4.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        // 标题字与文件行的图标对齐(外 4 + 行内 8)
        .child(div().px(px(4.0)).child(files_header.build()))
        .children(files_body);

    // ── diff
    let diff_area: AnyElement = match diff_body {
        Some(body) => body,
        None if stash.is_none() || s.files_loading || s.files_error.is_some() => {
            div().into_any_element()
        }
        None if file_count == 0 => {
            git_ui::empty_state(ICON_CLEAN, t("gitStash", "files.empty"), None)
                .size_full()
                .into_any_element()
        }
        None => git_ui::empty_state(ICON_STASH, t("gitStash", "files.selectHint"), None)
            .size_full()
            .into_any_element(),
    };

    div()
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .flex()
        .flex_col()
        .child(head)
        .child(actions)
        .children(notice)
        .child(files)
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .bg(ui::bg_base())
                .child(diff_area),
        )
        .into_any_element()
}

fn render_file_row(state: &Entity<StashDialog>, index: usize, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let Some(file) = s.files.get(index) else {
        return div().into_any_element();
    };
    let selected = s.selected_file == Some(index);
    let path = file.file.path.as_str();
    let (letter, color) = status_badge(&file.file.status, file.untracked);
    let tip = match (&file.file.old_path, file.untracked) {
        (_, true) => format!("{path}\n{}", t("gitStash", "files.untrackedTip")),
        (Some(old), false) => format!("{old} → {path}"),
        (None, false) => path.to_string(),
    };
    let click_state = state.clone();

    git_ui::list_row(
        SharedString::from(format!("git-stash-file-{index}")),
        selected,
    )
    .child(git_ui::file_label(
        path,
        color,
        file.file.status == "deleted",
        true,
    ))
    .child(git_ui::status_letter(letter, color))
    .tip(tip)
    .on_click(move |_: &ClickEvent, _window, cx| {
        if click_state.read(cx).selected_file != Some(index) {
            select_file(&click_state, index, cx);
        }
    })
    .into_any_element()
}

/// 文件状态 → 字母与颜色,与提交 diff 的文件行同一张表;未跟踪文件在贮藏里是「新增」,
/// 按更改区的口径标 `?`(与「未跟踪」分组同色)。
fn status_badge(status: &str, untracked: bool) -> (&'static str, Hsla) {
    if untracked {
        let status = Some(&GitStatus::Untracked);
        return (
            git_ui::status_label_for(status),
            git_ui::status_color(status),
        );
    }
    git_ui::commit_file_status(status)
}

fn render_notice(notice: &Notice) -> AnyElement {
    let (shapes, color, body): (&'static [Shape], Hsla, Div) = match notice {
        Notice::Info(text) => (
            ICON_CHECK,
            ui::color_success(),
            div()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_secondary())
                .child(text.clone()),
        ),
        Notice::Attention { text, files } => (
            ICON_ALERT,
            ui::color_warning(),
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::color_warning())
                        .child(text.clone()),
                )
                .when(!files.is_empty(), |el| el.child(file_list(files))),
        ),
        Notice::Error(detail) => (
            ICON_ALERT,
            ui::color_error(),
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::color_error())
                        .child(t("gitStash", "result.failed")),
                )
                .child(git_ui::error_text_box(detail)),
        ),
    };
    // 图标与字号同 `git_ui::notice_line`;整条带一层语义色淡底,贴着操作条
    div()
        .flex_none()
        .flex()
        .items_start()
        .gap(px(8.0))
        .px(px(16.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .bg(ui::with_alpha(color, 0.06))
        .child(div().flex_none().pt(px(1.0)).child(git_ui::icon(
            shapes,
            git_ui::ICON_SIZE_SM,
            color,
        )))
        .child(body.flex_1().min_w_0())
        .into_any_element()
}

// ─── 从贮藏新建分支 ───────────────────────────────────────────

/// 分支名问题对应的文案 key。复用分支弹窗的文案。
fn branch_issue_key(issue: BranchNameIssue) -> &'static str {
    match issue {
        BranchNameIssue::Empty => "nameEmpty",
        BranchNameIssue::Whitespace => "nameWhitespace",
        BranchNameIssue::IllegalChar => "nameIllegalChar",
        BranchNameIssue::LeadingDash => "nameLeadingDash",
        BranchNameIssue::Reserved => "nameReserved",
        BranchNameIssue::BadFormat => "nameBadFormat",
    }
}

/// 名字框下就地显示的问题(`None` = 显示命名提示)。边打边校验;空名要等用户**动过**
/// 输入框(打过字又清空)或点过「创建」才报 —— 一打开就红一行不友好。与分支弹窗的新建 /
/// 重命名同一口径(E2E r4 C9:这里原先清空不报、回车才报,那边清空即红字)。
fn branch_name_notice(name: &str, touched: bool, submitted: bool) -> Option<&'static str> {
    match git::validate_branch_name(name) {
        Err(issue) if !name.is_empty() => Some(branch_issue_key(issue)),
        Err(_) => (touched || submitted).then_some("nameEmpty"),
        Ok(()) => None,
    }
}

fn open_branch_dialog(state: &Entity<StashDialog>, window: &mut Window, cx: &mut App) {
    if is_open(kind::GIT_STASH_BRANCH) {
        return;
    }
    let s = state.read(cx);
    if s.busy.is_some() {
        return;
    }
    let Some(stash) = s.selected_stash().cloned() else {
        return;
    };
    let repo = PathBuf::from(&s.repo_path);
    let input =
        cx.new(|cx| InputState::new(window, cx).placeholder(t("gitBranch", "namePlaceholder")));
    let form = cx.new(|cx| {
        let sub = cx.subscribe(&input, |form: &mut Form, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && !form.touched {
                form.touched = true;
                cx.notify();
            }
        });
        Form {
            _sub: Some(sub),
            ..Form::new()
        }
    });
    let base = tr!(
        "gitStash",
        "branch.base",
        reference = stash.reference.as_str(),
        message = display_summary(&stash)
    );
    let list = state.clone();
    // 不论成败都重载:`git stash branch` 可能已经建好并检出了分支,随后的应用才失败
    let settled: OnChanged = {
        let list = state.clone();
        Rc::new(move |cx: &mut App| {
            let on_changed = list.read(cx).on_changed.clone();
            on_changed(cx);
            reload(&list, cx);
        })
    };
    let focus_input = input.clone();

    open_guarded(
        kind::GIT_STASH_BRANCH,
        window,
        cx,
        move |dialog, _window, cx| {
            let name = input.read(cx).value().trim().to_string();
            let f = form.read(cx);
            let invalid =
                branch_name_notice(&name, f.touched, f.submitted).map(|key| t("gitBranch", key));
            let (busy, error) = (f.op.busy, f.op.error.clone());
            // 与分支弹窗的新建 / 重命名同一口径:名字不合法(含空名)确认键就禁用,红字按
            // 上面的时机出(一打开不红);回车照样走 `on_ok`,那时空名也报出来
            let can_submit = git::validate_branch_name(&name).is_ok();
            let (form_ok, input_ok, list, repo, stash, settled) = (
                form.clone(),
                input.clone(),
                list.clone(),
                repo.clone(),
                stash.clone(),
                settled.clone(),
            );
            dialog
                .title(t("gitStash", "branch.title"))
                .w(px(git_ui::DIALOG_W_MD))
                .overlay_closable(!busy)
                .footer(git_ui::dialog_footer(
                    if busy {
                        t("gitStash", "branch.working")
                    } else {
                        t("gitStash", "branch.ok")
                    },
                    Some(t("gitStash", "confirm.cancel").into()),
                    false,
                    busy || !can_submit,
                ))
                .child(
                    git_ui::form_body()
                        .child(git_ui::hint_line(base.clone()))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .child(git_ui::field_label(t("gitStash", "branch.nameLabel")))
                                .child(Input::new(&input))
                                // 与分支弹窗的新建 / 重命名同一套:不合法就地报错,否则给命名提示
                                .child(match invalid {
                                    Some(msg) => git_ui::notice_line(msg, ui::color_error()),
                                    None => git_ui::hint_line(t("gitBranch", "nameHint")),
                                }),
                        )
                        .when_some(error, |el, error| el.child(git_ui::error_text_box(&error))),
                )
                .on_ok(move |_: &ClickEvent, window, cx| {
                    let name = input_ok.read(cx).value().trim().to_string();
                    form_ok.update(cx, |f, cx| {
                        f.submitted = true;
                        cx.notify();
                    });
                    // 非法名(含空名)已经就地报着了
                    if git::validate_branch_name(&name).is_err() {
                        return false;
                    }
                    let (index, hash, reference) =
                        (stash.index, stash.hash.clone(), stash.reference.clone());
                    let op_name = name.clone();
                    let list = list.clone();
                    run_form_op(
                        &form_ok,
                        kind::GIT_STASH_BRANCH,
                        "result.failedTitle",
                        repo.clone(),
                        move |repo| git::stash_branch(repo, &op_name, index, Some(&hash)),
                        move |outcome, _form, window, cx| {
                            close_guarded(kind::GIT_STASH_BRANCH, window, cx);
                            let notice = match outcome {
                                StashApplyOutcome::Applied => Notice::Info(tr!(
                                    "gitStash",
                                    "result.branched",
                                    reference = reference,
                                    name = name
                                )),
                                StashApplyOutcome::Conflicts { files } => Notice::Attention {
                                    text: t("gitStash", "result.conflicts").to_string(),
                                    files,
                                },
                            };
                            list.update(cx, |s, cx| {
                                s.notice = Some(notice);
                                cx.notify();
                            });
                            refocus(&list, window, cx);
                        },
                        Some(settled.clone()),
                        window,
                        cx,
                    );
                    false
                })
        },
    );
    autofocus(&focus_input, window, cx);
}

// ─── 新建贮藏 ─────────────────────────────────────────────────

/// [`open_save`] 的本体:成功回调拿得到 `window`(从列表里打开时要把焦点还给列表)。
fn open_save_with(repo_path: String, on_saved: OnSaved, window: &mut Window, cx: &mut App) {
    if repo_path.is_empty() || is_open(kind::GIT_STASH_SAVE) {
        return;
    }
    let form = cx.new(|_| Form::new());
    let input =
        cx.new(|cx| InputState::new(window, cx).placeholder(t("gitStash", "save.placeholder")));
    let repo_name = Path::new(&repo_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| repo_path.clone());
    let focus_input = input.clone();

    open_guarded(
        kind::GIT_STASH_SAVE,
        window,
        cx,
        move |dialog, _window, cx| {
            let f = form.read(cx);
            let (busy, keep_index, include_untracked) =
                (f.op.busy, f.keep_index, f.include_untracked);
            let (error, hint) = (f.op.error.clone(), f.hint.clone());
            let (keep_form, untracked_form) = (form.clone(), form.clone());
            let (form_ok, input_ok, repo, on_saved) = (
                form.clone(),
                input.clone(),
                PathBuf::from(&repo_path),
                on_saved.clone(),
            );
            dialog
                .title(t("gitStash", "save.title"))
                .w(px(git_ui::DIALOG_W_MD))
                .overlay_closable(!busy)
                .footer(git_ui::dialog_footer(
                    if busy {
                        t("gitStash", "save.working")
                    } else {
                        t("gitStash", "save.ok")
                    },
                    Some(t("gitStash", "confirm.cancel").into()),
                    false,
                    busy,
                ))
                .child(
                    git_ui::form_body()
                        .child(git_ui::hint_line(tr!(
                            "gitStash",
                            "save.repo",
                            name = repo_name.as_str()
                        )))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .child(git_ui::field_label(t("gitStash", "save.messageLabel")))
                                .child(Input::new(&input)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    git_ui::check_row(
                                        "git-stash-save-keep-index",
                                        keep_index,
                                        t("gitStash", "save.keepIndex"),
                                        false,
                                    )
                                    .on_click(
                                        move |_: &ClickEvent, _window, cx| {
                                            keep_form.update(cx, |f, cx| {
                                                f.keep_index = !f.keep_index;
                                                cx.notify();
                                            })
                                        },
                                    ),
                                )
                                .child(
                                    git_ui::check_row(
                                        "git-stash-save-untracked",
                                        include_untracked,
                                        t("gitStash", "save.includeUntracked"),
                                        false,
                                    )
                                    .on_click(
                                        move |_: &ClickEvent, _window, cx| {
                                            untracked_form.update(cx, |f, cx| {
                                                f.include_untracked = !f.include_untracked;
                                                cx.notify();
                                            })
                                        },
                                    ),
                                ),
                        )
                        .when_some(hint, |el, hint| {
                            el.child(git_ui::notice_line(hint, ui::color_warning()))
                        })
                        .when_some(error, |el, error| el.child(git_ui::error_text_box(&error))),
                )
                .on_ok(move |_: &ClickEvent, window, cx| {
                    let options = {
                        let f = form_ok.read(cx);
                        StashSaveOptions {
                            message: Some(input_ok.read(cx).value().to_string()),
                            keep_index: f.keep_index,
                            include_untracked: f.include_untracked,
                        }
                    };
                    let on_saved = on_saved.clone();
                    run_form_op(
                        &form_ok,
                        kind::GIT_STASH_SAVE,
                        "save.failedTitle",
                        repo.clone(),
                        move |repo| git::stash_save(repo, &options),
                        move |outcome, form, window, cx| match outcome {
                            StashSaveOutcome::Saved { reference } => {
                                close_guarded(kind::GIT_STASH_SAVE, window, cx);
                                on_saved(reference, None, window, cx);
                            }
                            // 贮藏已经生成(按 refs/stash 判的),只是有文件清不掉:照样关框、
                            // 刷新,说明里带上 git 原文
                            StashSaveOutcome::SavedWithWarnings { reference, detail } => {
                                close_guarded(kind::GIT_STASH_SAVE, window, cx);
                                on_saved(reference, Some(detail), window, cx);
                            }
                            StashSaveOutcome::NothingToStash => form.update(cx, |f, cx| {
                                f.hint = Some(t("gitStash", "save.nothing").to_string());
                                cx.notify();
                            }),
                        },
                        // 成功的重载由 `on_saved` 做;`stash push` 失败时什么都没动
                        None,
                        window,
                        cx,
                    );
                    false
                })
        },
    );
    autofocus(&focus_input, window, cx);
}

// ─── 贮藏后检出 ───────────────────────────────────────────────

fn submit_smart_checkout(
    form: &Entity<Form>,
    store: Entity<AppStore>,
    repo_path: String,
    branch: String,
    on_changed: OnChanged,
    window: &mut Window,
    cx: &mut App,
) {
    let target = branch.clone();
    run_form_op(
        form,
        kind::GIT_STASH_CHECKOUT,
        "checkout.failedTitle",
        PathBuf::from(repo_path),
        move |repo| git::checkout_with_stash(repo, &target),
        move |outcome, form, window, cx| {
            let branch = branch.as_str();
            // 自动贮藏总在栈顶;恢复不顺时它就留在那儿
            let reference = "stash@{0}";
            match outcome {
                SmartCheckoutOutcome::CheckedOut | SmartCheckoutOutcome::Restored => {
                    close_guarded(kind::GIT_STASH_CHECKOUT, window, cx);
                    let key = if outcome == SmartCheckoutOutcome::Restored {
                        "checkout.restored"
                    } else {
                        "checkout.done"
                    };
                    git_ui::toast(
                        &store,
                        tr!("gitStash", key, branch = branch),
                        Tone::Info,
                        cx,
                    );
                }
                // 下面两种结局写进框里;等待期间框被 Esc 关掉的话退到 toast / 失败详情框
                SmartCheckoutOutcome::RestoreConflicts { files } => {
                    let text = tr!(
                        "gitStash",
                        "checkout.conflicts",
                        branch = branch,
                        reference = reference
                    );
                    if !is_open(kind::GIT_STASH_CHECKOUT) {
                        git_ui::toast(&store, text, Tone::Attention, cx);
                        return;
                    }
                    form.update(cx, |f, cx| {
                        f.outcome = Some((text, files, None));
                        cx.notify();
                    });
                }
                SmartCheckoutOutcome::RestoreFailed { detail } => {
                    let text = tr!(
                        "gitStash",
                        "checkout.restoreFailed",
                        branch = branch,
                        reference = reference
                    );
                    if !is_open(kind::GIT_STASH_CHECKOUT) {
                        git_ui::show_git_error(
                            tr!("gitStash", "checkout.title", branch = branch),
                            format!("{text}\n\n{detail}"),
                            window,
                            cx,
                        );
                        return;
                    }
                    let detail = git_ui::error_text_state(&detail, 8, window, cx);
                    form.update(cx, |f, cx| {
                        f.outcome = Some((text, Vec::new(), Some(detail)));
                        cx.notify();
                    });
                }
                // 贮藏生成了、工作区却没清干净(有文件被占用):没有检出,仍在原分支
                SmartCheckoutOutcome::CheckoutCancelled { detail } => {
                    let text = tr!("gitStash", "checkout.cancelled", reference = reference);
                    if !is_open(kind::GIT_STASH_CHECKOUT) {
                        git_ui::show_git_error(
                            tr!("gitStash", "checkout.title", branch = branch),
                            format!("{text}\n\n{detail}"),
                            window,
                            cx,
                        );
                        return;
                    }
                    let detail = git_ui::error_text_state(&detail, 8, window, cx);
                    form.update(cx, |f, cx| {
                        f.outcome = Some((text, Vec::new(), Some(detail)));
                        cx.notify();
                    });
                }
            }
        },
        // 检出失败时贮藏已恢复回去,但工作区与贮藏列表都被动过一遍:成败都让面板重载
        Some(on_changed),
        window,
        cx,
    );
}

// ─── 小弹窗的执行器 ───────────────────────────────────────────

/// 小弹窗的操作:防重复提交 → 占闸、后台执行([`git_ui::spawn_gated`])→ 成功交给
/// `on_ok`(关框 / 改表单由它定);失败把 git 原文写回表单(框还开着)或退到失败详情框
/// (框已被关掉)。`on_settled` 不论成败都在最后调一次(面板 / 列表重载:失败也可能动过
/// 工作区)。闸被别的区占着时不执行,原因就地写回表单。
#[allow(clippy::too_many_arguments)]
fn run_form_op<T: Send + 'static>(
    form: &Entity<Form>,
    dialog: &'static str,
    failed_title_key: &'static str,
    repo: PathBuf,
    op: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
    on_ok: impl FnOnce(T, &Entity<Form>, &mut Window, &mut App) + 'static,
    on_settled: Option<OnChanged>,
    window: &mut Window,
    cx: &mut App,
) {
    let began = form.update(cx, |f, cx| {
        let began = f.op.begin();
        if began {
            f.hint = None;
            cx.notify();
        }
        began
    });
    if !began {
        return;
    }
    let done_form = form.clone();
    let started = git_ui::spawn_gated(
        &repo.to_string_lossy(),
        op,
        move |result, window, cx| {
            let form = done_form;
            match result {
                Ok(value) => {
                    form.update(cx, |f, cx| {
                        f.op.finish();
                        cx.notify();
                    });
                    on_ok(value, &form, window, cx);
                }
                Err(err) => {
                    let detail = error_text(&err);
                    eprintln!("[git] {dialog} 失败: {detail}");
                    if is_open(dialog) {
                        let state = git_ui::error_text_state(&detail, 8, window, cx);
                        form.update(cx, |f, cx| {
                            f.op.fail(state);
                            cx.notify();
                        });
                    } else {
                        form.update(cx, |f, _| f.op.finish());
                        git_ui::show_git_error(t("gitStash", failed_title_key), detail, window, cx);
                    }
                }
            }
            if let Some(on_settled) = on_settled {
                on_settled(cx);
            }
        },
        window,
        cx,
    );
    if !started {
        let state = git_ui::error_text_state(git_ui::gate_busy_text(), 2, window, cx);
        form.update(cx, |f, cx| {
            f.op.fail(state);
            cx.notify();
        });
    }
}

// ─── 小件 ─────────────────────────────────────────────────────

/// 最多 `limit` 个文件 + 剩余个数。
fn file_preview(files: &[String], limit: usize) -> (Vec<String>, usize) {
    let shown: Vec<String> = files.iter().take(limit).cloned().collect();
    let rest = files.len() - shown.len();
    (shown, rest)
}

/// 文件清单(冲突 / 挡住检出的文件):浅框里一行一个(图标 + 文件名 + 弱化目录,与更改区
/// 的文件行同一种写法),超出 [`PREVIEW_FILES`] 的收成「…以及另外 N 个」。只读,不可点。
fn file_list(files: &[String]) -> Div {
    let (shown, rest) = file_preview(files, PREVIEW_FILES);
    div()
        .flex()
        .flex_col()
        .p(px(4.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(ui::border_subtle())
        .bg(ui::bg_surface())
        .children(shown.iter().map(|path| {
            div()
                .h(px(git_ui::ROW_H))
                .px(px(8.0))
                .flex()
                .items_center()
                .child(git_ui::file_label(path, ui::text_primary(), false, true))
        }))
        .when(rest > 0, |el| {
            el.child(
                git_ui::hint_line(tr!(
                    "gitStash",
                    "result.moreFiles",
                    count = rest.to_string()
                ))
                .px(px(8.0))
                .py(px(4.0)),
            )
        })
}

/// 危险操作的确认框(删除 / 全部清除):确认键是危险色,叠在列表弹窗之上。`message` 的
/// 第一行是问句(正文色),其余是补充说明(弱化)。
fn danger_confirm(
    title: SharedString,
    message: String,
    ok: SharedString,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let on_ok = Rc::new(on_ok);
    let (lead, rest) = match message.split_once('\n') {
        Some((lead, rest)) => (lead.to_string(), Some(rest.to_string())),
        None => (message, None),
    };
    open_guarded(
        kind::GIT_STASH_DANGER,
        window,
        cx,
        move |dialog, _window, _cx| {
            let on_ok = on_ok.clone();
            dialog
                .title(title.clone())
                .w(px(git_ui::DIALOG_W_SM))
                .overlay_closable(true)
                .footer(git_ui::dialog_footer(
                    ok.clone(),
                    Some(t("gitStash", "confirm.cancel").into()),
                    true,
                    false,
                ))
                .child(
                    git_ui::form_body()
                        .child(
                            div()
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::text_primary())
                                .child(lead.clone()),
                        )
                        .when_some(rest.clone(), |el, rest| el.child(git_ui::hint_line(rest))),
                )
                .on_ok(move |_: &ClickEvent, window, cx| {
                    on_ok(window, cx);
                    true
                })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stash(index: usize, hash: &str) -> StashInfo {
        StashInfo {
            index,
            reference: format!("stash@{{{index}}}"),
            hash: hash.to_string(),
            message: String::new(),
            summary: String::new(),
            is_wip: false,
            branch: None,
            timestamp: 0,
            has_untracked: false,
        }
    }

    /// 刷新后的选中:刚贮藏过选最新;原先那个还在就跟着它(下标漂移);
    /// 没了落在原位置附近;空列表为 None。
    #[test]
    fn 刷新后跟住原先选中的贮藏() {
        let list = vec![stash(0, "c"), stash(1, "b"), stash(2, "a")];
        assert_eq!(reselect(&list, Some("b"), Some(0), false), Some(1));
        assert_eq!(reselect(&list, Some("b"), Some(2), true), Some(0));
        assert_eq!(reselect(&list, Some("gone"), Some(2), false), Some(2));
        assert_eq!(reselect(&list, Some("gone"), Some(9), false), Some(2));
        assert_eq!(reselect(&list, None, None, false), Some(0));
        assert_eq!(reselect(&[], Some("b"), Some(1), false), None);
    }

    #[test]
    fn 方向键到头不绕圈() {
        assert_eq!(step(0, None, 1), None);
        assert_eq!(step(3, None, 1), Some(0));
        assert_eq!(step(3, Some(0), -1), Some(0));
        assert_eq!(step(3, Some(1), 1), Some(2));
        assert_eq!(step(3, Some(2), 1), Some(2));
    }

    #[test]
    fn 文件清单截断() {
        let files: Vec<String> = (0..9).map(|i| format!("f{i}")).collect();
        let (shown, rest) = file_preview(&files, 6);
        assert_eq!(shown.len(), 6);
        assert_eq!(rest, 3);
        let (shown, rest) = file_preview(&files[..2], 6);
        assert_eq!((shown.len(), rest), (2, 0));
    }

    /// 已跟踪文件与提交 diff 的文件行同一张表;未跟踪的按更改区口径标 `?`、未跟踪色
    /// (与认不出的状态同字母,颜色分得开)。
    #[test]
    fn 状态字母() {
        ui::set_palette(ui::Palette::dark());
        for status in ["modified", "added", "deleted", "renamed", "weird"] {
            assert_eq!(
                status_badge(status, false),
                git_ui::commit_file_status(status)
            );
        }
        assert_eq!(status_badge("modified", false).0, "M");
        assert_eq!(status_badge("added", true), ("?", untracked_color()));
        assert_ne!(status_badge("weird", false).1, untracked_color());
    }

    #[test]
    fn 短哈希() {
        assert_eq!(short_hash("0123456789abcdef"), "0123456");
        assert_eq!(short_hash("abc"), "abc");
    }

    /// 「从贮藏新建分支」的空名口径与分支弹窗的新建 / 重命名一致:一打开不红;动过输入框
    /// (打字后清空)即就地报「请输入分支名」,不必等回车(E2E r4 C9);点过「创建」也报。
    /// 不合法的名字不论动没动过都当场报。
    #[test]
    fn 清空分支名即就地报错() {
        assert_eq!(branch_name_notice("", false, false), None, "一打开不红");
        assert_eq!(branch_name_notice("", true, false), Some("nameEmpty"));
        assert_eq!(branch_name_notice("", false, true), Some("nameEmpty"));
        assert_eq!(
            branch_name_notice("  ".trim(), true, false),
            Some("nameEmpty")
        );
        assert_eq!(
            branch_name_notice("a b", false, false),
            Some("nameWhitespace")
        );
        assert_eq!(
            branch_name_notice("a..b", false, false),
            Some("nameBadFormat")
        );
        assert_eq!(branch_name_notice("feature/x", true, true), None);
    }

    /// 分支名校验的每种原因都对得上分支弹窗的文案 key(两种语言都在)。
    #[test]
    fn 分支名校验复用分支弹窗文案() {
        for issue in [
            BranchNameIssue::Empty,
            BranchNameIssue::Whitespace,
            BranchNameIssue::IllegalChar,
            BranchNameIssue::LeadingDash,
            BranchNameIssue::Reserved,
            BranchNameIssue::BadFormat,
        ] {
            let key = branch_issue_key(issue);
            for locale in crate::i18n::Locale::ALL {
                assert!(
                    mt_i18n::lookup(locale, "gitBranch", key).is_some(),
                    "gitBranch.{key} 缺条目({locale})"
                );
            }
        }
    }
}
