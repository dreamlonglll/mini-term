//! 贮藏(git stash)弹窗,对标 IDEA 的 Stash / Unstash:
//!
//! - [`open_list`]:贮藏列表(左栏)+ 所选贮藏的改动文件与 diff(右栏);应用 / 弹出 /
//!   恢复暂存区状态 / 从此新建分支 / 删除 / 全部清除;没有贮藏时收成一个小空态框;
//! - [`open_save`]:新建贮藏(消息 / 保留暂存区 / 含未跟踪文件);
//! - [`open_smart_checkout`]:检出被本地改动挡住时的「贮藏后检出」确认框(IDEA 的
//!   Smart Checkout),后端是 [`git::checkout_with_stash`]。
//!
//! 入口由 Git 面板接线(仓库栏 / 更改区菜单 / 分支检出),本模块不碰面板;操作之后调
//! `on_changed` 让面板重载 —— 失败也调,冲突 / 半途失败同样动过工作区。
//!
//! # 结果就地显示
//!
//! toast 在弹窗遮罩下看不见,弹窗里的操作结果一律**就地**显示:成功一行说明;冲突黄字并
//! 写明「贮藏已保留」;失败是可选中复制的 git 原文。等待期间弹窗被 Esc 关掉的话,失败
//! 退到 [`git_ui::show_git_error`]。
//!
//! # 防重入与迟到
//!
//! 同一时刻只跑一个改写操作(`busy`;进行中按钮置灰、菜单项禁用);列表与文件列表的读取
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
    Render, ScrollStrategy, SharedString, Stateful, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::dialog::{Cancel, Confirm as DialogConfirm, Dialog, DialogFooter};
use gpui_component::input::{Input, InputState, TextareaState};
use mt_project::git::{
    self, BranchNameIssue, GitStatus, SmartCheckoutOutcome, StashApplyOutcome, StashFileInfo,
    StashInfo, StashSaveOptions, StashSaveOutcome,
};
use mt_ui::icons::{Geom, Ink, Shape};
use mt_ui::tooltip::TooltipExt as _;

use crate::git_diff::{self, CommitFileDiffPane};
use crate::git_history::{format_absolute_time, format_relative_time};
use crate::git_ui::{self, ICON_BUTTON, ICON_SIZE, Tone};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{autofocus, close_guarded, dialog_title, is_open, kind, open_guarded};
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
/// 贮藏成功之后的回调(参数是新贮藏的引用 `stash@{0}`)。
type OnSaved = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// 左栏宽度(与单文件历史弹窗同宽)。
const LIST_WIDTH: f32 = 320.0;
/// 贮藏行高(两行布局)。uniform_list 要求等高。
const ROW_HEIGHT: f32 = 52.0;
/// 改动文件行高。
const FILE_ROW_HEIGHT: f32 = 28.0;
/// 改动文件区最多露出几行,再多就滚动 —— 下面的 diff 才是主角。
const FILE_ROWS_VISIBLE: usize = 7;
/// 冲突 / 挡住检出的文件最多列几个,其余收成「…以及另外 N 个」。
const PREVIEW_FILES: usize = 6;

// ─── 图标 ─────────────────────────────────────────────────────
//
// 与 `git_ui` 同一口径(Lucide 原样 path、线宽 1.2/16);合并时收进共用模块。

const LUCIDE: (f32, f32, f32) = (0.0, 0.0, 24.0);
const STROKE: f32 = 1.2 / 16.0;

const fn path(d: &'static str) -> Shape {
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Path {
            d,
            view: LUCIDE,
            even_odd: false,
        },
    )
}

/// 贮藏(Lucide `archive`)。
pub const ICON_STASH: &[Shape] = &[
    path("M3 3h18a1 1 0 0 1 1 1v3a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z"),
    path("M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8"),
    path("M10 12h4"),
];
/// 删除 / 全部清除(Lucide `trash-2`)。
pub const ICON_TRASH: &[Shape] = &[
    path("M3 6h18"),
    path("M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6"),
    path("M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2"),
    path("M10 11v6"),
    path("M14 11v6"),
];

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
}

/// 小弹窗(新建贮藏 / 新建分支 / 贮藏后检出)的表单状态。
struct Form {
    /// 确认后、结果回来之前:确认键禁用,防重复提交。
    busy: bool,
    /// 上一次执行失败的 git 原文(可选中复制)。
    error: Option<Entity<TextareaState>>,
    /// 非错误的就地提示(「没有可贮藏的改动」)。
    hint: Option<String>,
    /// 点过确认(此后空的必填项才报错)。
    submitted: bool,
    keep_index: bool,
    include_untracked: bool,
    /// 贮藏后检出的终态(已检出、但恢复不顺):说明 + 文件 + 原文。
    outcome: Option<(String, Vec<String>, Option<Entity<TextareaState>>)>,
}

impl Form {
    fn new() -> Self {
        Self {
            busy: false,
            error: None,
            hint: None,
            submitted: false,
            keep_index: false,
            include_untracked: true,
            outcome: None,
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
#[allow(dead_code)] // 入口由主会话接线后移除
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
#[allow(dead_code)] // 入口由主会话接线后移除
pub fn open_save(
    store: Entity<AppStore>,
    repo_path: String,
    on_changed: Rc<dyn Fn(&mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    let toast_store = store.clone();
    let on_saved = Rc::new(
        move |reference: String, _window: &mut Window, cx: &mut App| {
            git_ui::toast(
                &toast_store,
                tr!("gitStash", "save.saved", reference = reference),
                Tone::Info,
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
#[allow(dead_code)] // 入口由主会话接线后移除
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
    let (preview, more) = file_preview(&conflicts, PREVIEW_FILES);
    open_guarded(
        kind::GIT_STASH_CHECKOUT,
        window,
        cx,
        move |dialog, _window, cx| {
            let title = tr!("gitStash", "checkout.title", branch = branch.as_str());
            let f = form.read(cx);
            if let Some((text, files, detail)) = f.outcome.clone() {
                // 终态:已经检出,但恢复不顺 —— 只剩「知道了」
                let (shown, rest) = file_preview(&files, PREVIEW_FILES);
                return dialog
                    .title(title)
                    .w(px(480.0))
                    .overlay_closable(true)
                    .footer(
                        DialogFooter::new().child(
                            Button::new("ok")
                                .label(t("gitStash", "checkout.gotIt"))
                                .primary()
                                .cursor_pointer()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(DialogConfirm { secondary: false }),
                                        cx,
                                    )
                                }),
                        ),
                    )
                    .child(
                        form_body()
                            .child(notice_line(ICON_ALERT_SHAPES, ui::color_warning(), text))
                            .child(file_lines(&shown, rest))
                            .when_some(detail, |el, detail| {
                                el.child(git_ui::error_text_box(&detail))
                            }),
                    )
                    .on_ok(|_, _, _| true);
            }
            let (busy, error) = (f.busy, f.error.clone());
            let lead = if preview.is_empty() {
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
            dialog
                .title(title)
                .w(px(480.0))
                .overlay_closable(true)
                .footer(form_footer(
                    t("gitStash", "checkout.ok"),
                    t("gitStash", "checkout.working"),
                    busy,
                ))
                .child(
                    form_body()
                        .child(
                            div()
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::text_primary())
                                .child(lead),
                        )
                        .when(!preview.is_empty(), |el| {
                            el.child(file_lines(&preview, more))
                        })
                        .child(muted_line(tr!(
                            "gitStash",
                            "checkout.explain",
                            branch = branch.as_str()
                        )))
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

/// 跑一个改写操作:防重入 → 后台执行 → 结果写成 [`Notice`] → 通知面板、重读列表。
fn run_op<T: Send + 'static>(
    state: &Entity<StashDialog>,
    busy: Busy,
    op: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
    on_ok: impl FnOnce(T) -> Notice + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(repo) = state.update(cx, |s, cx| {
        if s.busy.is_some() {
            return None;
        }
        s.busy = Some(busy);
        s.notice = None;
        cx.notify();
        Some(PathBuf::from(&s.repo_path))
    }) else {
        return;
    };
    refocus(state, window, cx);
    let state = state.clone();
    window
        .spawn(cx, async move |cx| {
            let result = cx
                .background_executor()
                .spawn(async move { op(&repo) })
                .await;
            let _ = cx.update(|window, cx| {
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
            });
        })
        .detach();
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
        move |reference: String, window: &mut Window, cx: &mut App| {
            let on_changed = list.update(cx, |s, cx| {
                s.select_newest = true;
                s.notice = Some(Notice::Info(tr!(
                    "gitStash",
                    "save.saved",
                    reference = reference
                )));
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
        if s.stashes.is_empty() && s.list_error.is_none() && !s.list_loading {
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

/// 没有贮藏:小框 + 空态 + 「贮藏当前改动…」。
fn render_empty_dialog(dialog: Dialog, state: &Entity<StashDialog>, cx: &mut App) -> Dialog {
    let focus = state.read(cx).focus.clone();
    let save_state = state.clone();
    dialog
        .w(px(420.0))
        .title(dialog_title(kind::GIT_STASH, t("gitStash", "title")))
        .overlay_closable(true)
        .child(
            div()
                .key_context(STASH_CONTEXT)
                .track_focus(&focus)
                .px(px(16.0))
                .pb(px(16.0))
                .child(git_ui::empty_state(
                    ICON_STASH,
                    t("gitStash", "empty"),
                    Some(
                        ui::primary_button("git-stash-empty-save", t("gitStash", "stashChanges"))
                            .h(px(28.0))
                            .on_click(move |_: &ClickEvent, window, cx| {
                                open_save_from_list(&save_state, window, cx)
                            }),
                    ),
                )),
        )
}

fn render_list_column(state: &Entity<StashDialog>, cx: &mut App) -> AnyElement {
    let mono = git_ui::mono_font(&state.read(cx).store, cx);
    let s = state.read(cx);
    let count = s.stashes.len();
    let busy = s.busy.is_some();

    let (new_state, refresh_state, clear_state) = (state.clone(), state.clone(), state.clone());
    let header = div()
        .flex_none()
        .h(px(48.0))
        .pl(px(14.0))
        .pr(px(8.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(git_ui::icon(ICON_STASH, ICON_SIZE, ui::accent()))
        .child(
            div()
                .text_size(ui::font_px(14.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui::text_primary())
                .child(t("gitStash", "title")),
        )
        .child(git_ui::pill(count.to_string(), ui::text_muted()).font_family(mono.clone()))
        .child(div().flex_1())
        .child(
            git_ui::icon_button("git-stash-new", git_ui::ICON_PLUS, false, busy)
                .tip(t("gitStash", "stashChanges"))
                .when(!busy, |el| {
                    el.on_click(move |_: &ClickEvent, window, cx| {
                        open_save_from_list(&new_state, window, cx)
                    })
                }),
        )
        .child(
            git_ui::icon_button(
                "git-stash-refresh",
                git_ui::ICON_REFRESH,
                s.list_loading,
                false,
            )
            .tip(t("gitStash", "refresh"))
            .on_click(move |_: &ClickEvent, _window, cx| reload(&refresh_state, cx)),
        )
        .child(
            danger_icon_button("git-stash-clear", ICON_TRASH, busy || count == 0)
                .tip(t("gitStash", "clearAll"))
                .when(!busy && count > 0, |el| {
                    el.on_click(move |_: &ClickEvent, window, cx| {
                        confirm_clear(&clear_state, window, cx)
                    })
                }),
        );

    let body: AnyElement = match s.list_error.clone() {
        Some(err) if count == 0 => div()
            .flex_1()
            .p(px(16.0))
            .text_size(ui::font_px(12.0))
            .text_color(ui::color_error())
            .child(tr!("gitStash", "loadFailed", detail = err))
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
                                .map(|i| render_row(&list_state, i, &mono, now, cx))
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
        .px(px(14.0))
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

fn render_row(
    state: &Entity<StashDialog>,
    index: usize,
    mono: &SharedString,
    now: i64,
    cx: &mut App,
) -> AnyElement {
    let s = state.read(cx);
    let Some(stash) = s.stashes.get(index) else {
        return div().into_any_element();
    };
    let selected = s.selected == Some(index);
    let busy = s.busy.is_some();
    let when = format_relative_time(stash.timestamp, now);
    let meta = match &stash.branch {
        Some(branch) => format!("{branch} · {when}"),
        None => when,
    };
    let tip = format!(
        "{}\n{} · {}",
        stash.message,
        format_absolute_time(stash.timestamp),
        &stash.hash[..7.min(stash.hash.len())]
    );
    let (click_state, menu_state) = (state.clone(), state.clone());

    let mut row = div()
        .id(SharedString::from(format!("git-stash-row-{index}")))
        .relative()
        .w_full()
        .h(px(ROW_HEIGHT))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(4.0))
        .px(px(10.0))
        .rounded(px(4.0))
        .cursor_pointer();
    row = if selected {
        row.bg(ui::accent_subtle()).child(git_ui::selection_bar())
    } else {
        row.hover(|el| el.bg(ui::bg_overlay()))
    };
    row.child(
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_none()
                    .font_family(mono.clone())
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(stash.reference.clone()),
            )
            .when(stash.is_wip, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(mono.clone())
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child("WIP"),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_primary())
                    .child(display_summary(stash)),
            )
            .when(stash.has_untracked, |el| {
                el.child(git_ui::pill(t("gitStash", "untracked"), ui::color_info()))
            }),
    )
    .child(
        div()
            .w_full()
            .min_w_0()
            .truncate()
            .font_family(mono.clone())
            .text_size(ui::font_px(11.0))
            .text_color(ui::text_muted())
            .child(meta),
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
    let mono = git_ui::mono_font(&state.read(cx).store, cx);
    let diff = state.read(cx).diff.clone();
    let toolbar = diff.toolbar("git-stash-diff", kind::GIT_STASH, cx);
    let s = state.read(cx);
    let stash = s.selected_stash().cloned();
    let busy = s.busy;
    let enabled = busy.is_none() && stash.is_some();

    // ── 标题行:所选贮藏 + 右上角 diff 工具组(含 ✕)
    let title = div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(16.0))
        .py(px(10.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(
            div()
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
                            .text_size(ui::font_px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui::text_primary())
                            .child(display_summary(stash)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(mono.clone())
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_muted())
                            .child(format!(
                                "{} · {}",
                                stash.reference,
                                &stash.hash[..7.min(stash.hash.len())]
                            )),
                    )
                }),
        )
        .child(toolbar);

    // ── 操作条
    let (apply_state, pop_state, index_state, branch_state, drop_state) = (
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
    );
    let reinstate = s.reinstate_index;
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
            text_button(
                "git-stash-apply",
                t("gitStash", "action.apply"),
                Tone3::Primary,
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
            text_button(
                "git-stash-pop",
                t("gitStash", "action.pop"),
                Tone3::Ghost,
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
            check_row(
                "git-stash-reinstate",
                reinstate,
                t("gitStash", "action.reinstateIndex"),
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
            text_button(
                "git-stash-branch",
                t("gitStash", "action.newBranch"),
                Tone3::Ghost,
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
            text_button(
                "git-stash-drop",
                t("gitStash", "action.drop"),
                Tone3::Danger,
                enabled,
            )
            .tip(t("gitStash", "action.dropTip"))
            .when(enabled, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| confirm_drop(&drop_state, window, cx))
            }),
        );

    // ── 结果行
    let notice = s.notice.as_ref().map(render_notice);

    // ── 改动文件
    let file_count = s.files.len();
    let files_body: AnyElement = if s.files_loading {
        div()
            .px(px(16.0))
            .py(px(8.0))
            .child(git_ui::busy_line(t("gitStash", "files.loading")))
            .into_any_element()
    } else if let Some(err) = s.files_error.clone() {
        div()
            .px(px(16.0))
            .py(px(8.0))
            .text_size(ui::font_px(12.0))
            .text_color(ui::color_error())
            .child(tr!("gitStash", "files.failed", detail = err))
            .into_any_element()
    } else if file_count == 0 {
        div()
            .px(px(16.0))
            .py(px(8.0))
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_muted())
            .child(if stash.is_some() {
                t("gitStash", "files.empty")
            } else {
                ""
            })
            .into_any_element()
    } else {
        let list_state = state.clone();
        let visible = file_count.min(FILE_ROWS_VISIBLE);
        div()
            .h(px(visible as f32 * FILE_ROW_HEIGHT + 8.0))
            .px(px(8.0))
            .pb(px(8.0))
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
            .into_any_element()
    };
    let files = div()
        .flex_none()
        .flex()
        .flex_col()
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(git_ui::group_header(tr!(
            "gitStash",
            "files.header",
            count = file_count.to_string()
        )))
        .child(files_body);

    // ── diff
    let has_file = s.selected_file.is_some();
    let diff_body = if has_file {
        diff.body(cx)
    } else {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(ui::font_px(13.0))
            .text_color(ui::text_muted())
            .child(if file_count > 0 {
                t("gitStash", "files.selectHint")
            } else {
                ""
            })
            .into_any_element()
    };

    div()
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .flex()
        .flex_col()
        .child(title)
        .child(actions)
        .children(notice)
        .child(files)
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .bg(ui::bg_base())
                .child(diff_body),
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
    let (name, dir) = match path.rsplit_once('/') {
        Some((dir, name)) => (name.to_string(), dir.to_string()),
        None => (path.to_string(), String::new()),
    };
    let (letter, color) = status_badge(&file.file.status, file.untracked);
    let deleted = file.file.status == "deleted";
    let tip = if file.untracked {
        format!("{path}\n{}", t("gitStash", "files.untrackedTip"))
    } else {
        path.to_string()
    };
    let click_state = state.clone();

    let mut row = div()
        .id(SharedString::from(format!("git-stash-file-{index}")))
        .relative()
        .w_full()
        .h(px(FILE_ROW_HEIGHT))
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(10.0))
        .rounded(px(4.0))
        .cursor_pointer();
    row = if selected {
        row.bg(ui::accent_subtle()).child(git_ui::selection_bar())
    } else {
        row.hover(|el| el.bg(ui::bg_overlay()))
    };
    row.child(mt_ui::icons::FileIcon::new(&name, false, false))
        .child(
            div()
                .flex_none()
                .max_w(px(260.0))
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(13.0))
                .text_color(if deleted {
                    ui::text_muted()
                } else {
                    ui::text_primary()
                })
                .when(deleted, |el| el.line_through())
                .child(name),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(dir),
        )
        .when(file.untracked, |el| {
            el.child(git_ui::pill(t("gitStash", "untracked"), ui::color_info()))
        })
        .child(
            div()
                .flex_none()
                .w(px(14.0))
                .text_right()
                .text_size(ui::font_px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(letter),
        )
        .tip(tip)
        .on_click(move |_: &ClickEvent, _window, cx| {
            if click_state.read(cx).selected_file != Some(index) {
                select_file(&click_state, index, cx);
            }
        })
        .into_any_element()
}

/// 文件状态 → 字母与颜色(颜色与更改区同源)。未跟踪文件在贮藏里是「新增」,字母标 `U`。
fn status_badge(status: &str, untracked: bool) -> (&'static str, Hsla) {
    let (letter, status) = if untracked {
        ("U", Some(GitStatus::Untracked))
    } else {
        match status {
            "added" => ("A", Some(GitStatus::Added)),
            "deleted" => ("D", Some(GitStatus::Deleted)),
            "renamed" => ("R", Some(GitStatus::Renamed)),
            "modified" => ("M", Some(GitStatus::Modified)),
            _ => ("?", None),
        }
    };
    (letter, crate::git_changes::status_color(status.as_ref()))
}

fn render_notice(notice: &Notice) -> AnyElement {
    let (shapes, color, body): (&'static [Shape], Hsla, Div) = match notice {
        Notice::Info(text) => (
            git_ui::ICON_CHECK,
            ui::color_success(),
            div().child(
                div()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_secondary())
                    .child(text.clone()),
            ),
        ),
        Notice::Attention { text, files } => {
            let (shown, rest) = file_preview(files, PREVIEW_FILES);
            (
                ICON_ALERT_SHAPES,
                ui::color_warning(),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::color_warning())
                            .child(text.clone()),
                    )
                    .child(file_lines(&shown, rest)),
            )
        }
        Notice::Error(detail) => (
            ICON_ALERT_SHAPES,
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
        .child(
            div()
                .flex_none()
                .pt(px(1.0))
                .child(git_ui::icon(shapes, ICON_SIZE, color)),
        )
        .child(body.flex_1().min_w_0())
        .into_any_element()
}

/// 警告三角(`git_ui` 那枚)。起个别名,读起来不像「又一个图标表」。
const ICON_ALERT_SHAPES: &[Shape] = git_ui::ICON_ALERT;

// ─── 从贮藏新建分支 ───────────────────────────────────────────

/// 分支名的即时校验(空名不报,等点「创建」再说)。复用分支弹窗的文案。
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
    let form = cx.new(|_| Form::new());
    let input =
        cx.new(|cx| InputState::new(window, cx).placeholder(t("gitBranch", "namePlaceholder")));
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
            // 边打边校验;空名不报,点过「创建」才报
            let invalid = match git::validate_branch_name(&name) {
                Err(issue) if !name.is_empty() => Some(t("gitBranch", branch_issue_key(issue))),
                Err(_) => f.submitted.then(|| t("gitBranch", "nameEmpty")),
                Ok(()) => None,
            };
            let (busy, error) = (f.busy, f.error.clone());
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
                .w(px(440.0))
                .overlay_closable(true)
                .footer(form_footer(
                    t("gitStash", "branch.ok"),
                    t("gitStash", "branch.working"),
                    busy,
                ))
                .child(
                    form_body()
                        .child(muted_line(base.clone()))
                        .child(field_label(t("gitStash", "branch.nameLabel")))
                        .child(Input::new(&input))
                        .when_some(invalid, |el, msg| el.child(error_line(msg)))
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
            let (busy, keep_index, include_untracked) = (f.busy, f.keep_index, f.include_untracked);
            let (error, hint) = (f.error.clone(), f.hint.clone());
            let (keep_form, untracked_form) = (form.clone(), form.clone());
            let (form_ok, input_ok, repo, on_saved) = (
                form.clone(),
                input.clone(),
                PathBuf::from(&repo_path),
                on_saved.clone(),
            );
            dialog
                .title(t("gitStash", "save.title"))
                .w(px(460.0))
                .overlay_closable(true)
                .footer(form_footer(
                    t("gitStash", "save.ok"),
                    t("gitStash", "save.working"),
                    busy,
                ))
                .child(
                    form_body()
                        .child(muted_line(tr!(
                            "gitStash",
                            "save.repo",
                            name = repo_name.as_str()
                        )))
                        .child(field_label(t("gitStash", "save.messageLabel")))
                        .child(Input::new(&input))
                        .child(
                            check_row(
                                "git-stash-save-keep-index",
                                keep_index,
                                t("gitStash", "save.keepIndex"),
                            )
                            .mt(px(4.0))
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
                            check_row(
                                "git-stash-save-untracked",
                                include_untracked,
                                t("gitStash", "save.includeUntracked"),
                            )
                            .on_click(
                                move |_: &ClickEvent, _window, cx| {
                                    untracked_form.update(cx, |f, cx| {
                                        f.include_untracked = !f.include_untracked;
                                        cx.notify();
                                    })
                                },
                            ),
                        )
                        .when_some(hint, |el, hint| {
                            el.child(notice_line(ICON_ALERT_SHAPES, ui::color_warning(), hint))
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
                                on_saved(reference, window, cx);
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
            }
        },
        // 检出失败时贮藏已恢复回去,但工作区与贮藏列表都被动过一遍:成败都让面板重载
        Some(on_changed),
        window,
        cx,
    );
}

// ─── 小弹窗的执行器 ───────────────────────────────────────────

/// 小弹窗的操作:防重复提交 → 后台执行 → 成功交给 `on_ok`(关框 / 改表单由它定);
/// 失败把 git 原文写回表单(框还开着)或退到失败详情框(框已被关掉)。`on_settled`
/// 不论成败都在最后调一次(面板 / 列表重载:失败也可能动过工作区)。
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
    let started = form.update(cx, |f, cx| {
        if f.busy {
            return false;
        }
        f.busy = true;
        f.error = None;
        f.hint = None;
        cx.notify();
        true
    });
    if !started {
        return;
    }
    let form = form.clone();
    window
        .spawn(cx, async move |cx| {
            let result = cx
                .background_executor()
                .spawn(async move { op(&repo) })
                .await;
            let _ = cx.update(|window, cx| {
                form.update(cx, |f, cx| {
                    f.busy = false;
                    cx.notify();
                });
                match result {
                    Ok(value) => on_ok(value, &form, window, cx),
                    Err(err) => {
                        let detail = error_text(&err);
                        eprintln!("[git] {dialog} 失败: {detail}");
                        if is_open(dialog) {
                            let state = git_ui::error_text_state(&detail, 8, window, cx);
                            form.update(cx, |f, cx| {
                                f.error = Some(state);
                                cx.notify();
                            });
                        } else {
                            git_ui::show_git_error(
                                t("gitStash", failed_title_key),
                                detail,
                                window,
                                cx,
                            );
                        }
                    }
                }
                if let Some(on_settled) = on_settled {
                    on_settled(cx);
                }
            });
        })
        .detach();
}

/// 失败的可展示原因:git 的 stderr 原文收掉首尾空白。
fn error_text(err: &anyhow::Error) -> String {
    format!("{err:#}").trim().to_string()
}

// ─── 小件 ─────────────────────────────────────────────────────

/// 最多 `limit` 个文件 + 剩余个数。
fn file_preview(files: &[String], limit: usize) -> (Vec<String>, usize) {
    let shown: Vec<String> = files.iter().take(limit).cloned().collect();
    let rest = files.len() - shown.len();
    (shown, rest)
}

/// 文件清单(等宽小字,一行一个)+「…以及另外 N 个」。
fn file_lines(files: &[String], rest: usize) -> Div {
    let mut el = div().flex().flex_col().gap(px(2.0));
    for file in files {
        el = el.child(
            div()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_secondary())
                .child(file.clone()),
        );
    }
    if rest > 0 {
        el = el.child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(tr!(
                    "gitStash",
                    "result.moreFiles",
                    count = rest.to_string()
                )),
        );
    }
    el
}

/// 文字按钮的三种语气。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone3 {
    Primary,
    Ghost,
    Danger,
}

/// 操作条上的文字按钮:高 26px;禁用时压淡、不挂悬停与手型(调用方也别挂点击)。
fn text_button(id: &'static str, label: &'static str, tone: Tone3, enabled: bool) -> Stateful<Div> {
    if !enabled {
        return div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(26.0))
            .px(px(12.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(ui::border_subtle())
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_muted())
            .opacity(0.6)
            .child(label);
    }
    let button = match tone {
        Tone3::Primary => ui::primary_button(id, label),
        Tone3::Ghost => ui::ghost_button(id, label),
        Tone3::Danger => ui::danger_button(id, label),
    };
    button.flex_none().h(px(26.0)).px(px(12.0))
}

/// 勾选行:复选框 + 文字,整行可点(点击区高 26px)。点击由调用方挂。
fn check_row(id: &'static str, checked: bool, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .h(px(26.0))
        .px(px(4.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|el| el.bg(ui::bg_overlay()))
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_primary())
        .child(git_ui::check_box(checked, ui::accent()))
        .child(label)
}

/// 危险色的图标按钮:悬停时图标转错误色、底色叠一层淡红。
fn danger_icon_button(id: &'static str, shapes: &'static [Shape], disabled: bool) -> Stateful<Div> {
    let base = div()
        .id(id)
        .group(id)
        .size(px(ICON_BUTTON))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(4.0));
    if disabled {
        return base
            .opacity(0.45)
            .child(git_ui::icon(shapes, ICON_SIZE, ui::text_muted()));
    }
    base.cursor_pointer()
        .hover(|el| el.bg(ui::with_alpha(ui::color_error(), 0.12)))
        .child(git_ui::hover_icon(
            id,
            shapes,
            ICON_SIZE,
            ui::text_muted(),
            ui::color_error(),
        ))
}

/// 图标 + 一句说明(小弹窗里的就地提示)。
fn notice_line(shapes: &'static [Shape], color: Hsla, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .pt(px(1.0))
                .child(git_ui::icon(shapes, ICON_SIZE, color)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(ui::font_px(12.0))
                .text_color(color)
                .child(text.into()),
        )
}

/// 「取消 / 确认」页脚。确认键在等待期间换成「…中」并禁用(防重复提交)。按钮派发
/// Dialog 自己的 Confirm / Cancel,与回车 / Esc 走同一条路(见 `prompt::confirm_footer`)。
fn form_footer(ok: &'static str, working: &'static str, busy: bool) -> DialogFooter {
    DialogFooter::new()
        .child(
            Button::new("cancel")
                .label(t("gitStash", "confirm.cancel"))
                .cursor_pointer()
                .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
        )
        .child(
            Button::new("ok")
                .label(if busy { working } else { ok })
                .primary()
                .cursor_pointer()
                .disabled(busy)
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(DialogConfirm { secondary: false }), cx)
                }),
        )
}

fn form_body() -> Div {
    div().px(px(20.0)).flex().flex_col().gap(px(8.0))
}

fn field_label(text: &'static str) -> Div {
    div()
        .mt(px(4.0))
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_secondary())
        .child(text)
}

fn muted_line(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_muted())
        .child(text.into())
}

fn error_line(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(ui::font_px(12.0))
        .text_color(ui::color_error())
        .child(text.into())
}

/// 危险操作的确认框(删除 / 全部清除):确认键是危险色,叠在列表弹窗之上。
fn danger_confirm(
    title: SharedString,
    message: String,
    ok: SharedString,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let on_ok = Rc::new(on_ok);
    open_guarded(
        kind::GIT_STASH_DANGER,
        window,
        cx,
        move |dialog, _window, _cx| {
            let on_ok = on_ok.clone();
            let mut body = form_body();
            for line in message.split('\n') {
                body = body.child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_primary())
                        // 空行也要占一行高
                        .child(if line.is_empty() {
                            SharedString::from(" ")
                        } else {
                            SharedString::from(line.to_string())
                        }),
                );
            }
            dialog
                .title(title.clone())
                .w(px(420.0))
                .overlay_closable(true)
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label(t("gitStash", "confirm.cancel"))
                                .cursor_pointer()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(Cancel), cx)
                                }),
                        )
                        .child(
                            Button::new("ok")
                                .label(ok.clone())
                                .danger()
                                .cursor_pointer()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(DialogConfirm { secondary: false }),
                                        cx,
                                    )
                                }),
                        ),
                )
                .child(body)
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

    /// 每枚图标:每一笔都解析得出点、所有点落在单位方框内(容差同 `git_ui` 那条)。
    #[test]
    fn 图标几何都在单位方框内() {
        for (i, shapes) in [ICON_STASH, ICON_TRASH].iter().enumerate() {
            for shape in shapes.iter() {
                let (pts, _) = shape.geom.points();
                assert!(pts.len() >= 2, "第 {i} 枚图标有一笔没解析出点");
                for (x, y) in pts {
                    assert!(
                        (-0.01..=1.01).contains(&x) && (-0.01..=1.01).contains(&y),
                        "第 {i} 枚图标越界点 ({x}, {y})"
                    );
                }
            }
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

    #[test]
    fn 状态字母() {
        assert_eq!(status_badge("modified", false).0, "M");
        assert_eq!(status_badge("added", false).0, "A");
        assert_eq!(status_badge("deleted", false).0, "D");
        assert_eq!(status_badge("renamed", false).0, "R");
        assert_eq!(status_badge("added", true).0, "U", "未跟踪的新增标 U");
        assert_eq!(status_badge("weird", false).0, "?");
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
