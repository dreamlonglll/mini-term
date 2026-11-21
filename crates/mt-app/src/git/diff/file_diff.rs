use super::*;

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
pub(super) enum HunkOp {
    Stage,
    Unstage,
    Discard,
}

/// 进行中的块操作是从哪颗按钮点的(「处理中…」画在哪)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Busy {
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
pub(super) enum OpScope {
    Hunk(usize),
    Change(usize),
    Picked,
}

/// 一次块操作交给后端的全部东西,在**点按钮那一刻**取好(见 [`DiffState::op_target`]):
/// 丢弃要先确认,确认框开着时屏幕上的 diff 与勾选不会变,落下去的就是用户看到的那些;
/// 后端再重读两侧防陈旧。
#[derive(Clone)]
pub(super) struct OpTarget {
    pub(super) diff: GitDiffResult,
    /// 要动的块(块号递增)与各块生效的行(`None` = 整块)
    pub(super) picks: Vec<HunkPick>,
    pub(super) busy: Busy,
    /// 操作后原地重取时停在第几块(原位置上的那一块 / 下一块);`None` = 不跳,停在原地
    pub(super) land: Option<usize>,
    /// 按行操作时一共几行(确认框文案用);整块是 `None`
    pub(super) lines: Option<usize>,
    /// 并排视图里整处改动(没勾行)的操作:确认框说「这一处改动」而不是「勾选的行」
    pub(super) change: bool,
}

/// 单文件 diff 页签的控制面:文件序列、当前下标、进行中的块操作与就地错误。
///
/// 与 [`DiffState`] 分开:后者是各种 diff 共用的「diff 内容」,这些是单文件 diff 独有的。
/// 页签观察它:换了文件(`current` / `files` 变了)就跟着改身份与标题(见 [`tab`])。
pub(super) struct FileDiff {
    pub(super) context: GitContext,
    pub(super) files: Vec<DiffFile>,
    pub(super) current: usize,
    pub(super) on_changed: Option<OnChanged>,
    pub(super) on_switch: Option<OnSwitch>,
    pub(super) reload_files: Option<ReloadFiles>,
    /// 进行中的块操作。`Some` 时全部块按钮(连同工具栏的「所选行」)禁用,防重复点击
    pub(super) busy: Option<Busy>,
    /// 块操作失败的原文。就地显示在工具栏下方(多行原文,toast 装不下)
    pub(super) op_error: Option<String>,
    /// 失败是因为 diff 过期:错误条上给「刷新」
    pub(super) stale: bool,
}

impl FileDiff {
    pub(super) fn file(&self) -> &DiffFile {
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
pub(super) struct HunkUi {
    pub(super) state: Entity<DiffState>,
    pub(super) ctl: Entity<FileDiff>,
    pub(super) side: DiffSide,
    /// 进行中的块操作
    pub(super) busy: Option<Busy>,
    /// 文件不是 UTF-8 时按钮一律禁用,悬停给这句原因(块操作按 UTF-8 拼接写回,
    /// 会把 GBK / UTF-16 文件写坏;整文件暂存不受影响)
    pub(super) blocked: Option<SharedString>,
}

/// 「处理中…」(块头 / 工具栏上正在跑的那颗按钮换成它)。
pub(super) fn working_label() -> gpui::Div {
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
pub(super) fn op_label(op: HunkOp, picked: usize) -> SharedString {
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
pub(super) fn side_ops(side: DiffSide) -> &'static [HunkOp] {
    match side {
        DiffSide::Unstaged => &[HunkOp::Stage, HunkOp::Discard],
        DiffSide::Staged => &[HunkOp::Unstage],
        DiffSide::Working => &[],
    }
}

pub(super) fn op_key(op: HunkOp) -> &'static str {
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
pub(super) fn hunk_actions(
    hunk_ui: &HunkUi,
    hunk: usize,
    head: SharedString,
    picked: usize,
) -> AnyElement {
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
pub(super) fn picks_toolbar(
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
pub(super) fn non_utf8_hunk_reason(encoding: TextEncoding) -> SharedString {
    let label = match encoding.label() {
        "" => t("gitCommit", "hunkUnknownEncoding"),
        label => label,
    };
    tr!("gitCommit", "hunkNonUtf8", encoding = label).into()
}

/// 按钮点下去时,它记的作用对象还在不在原处(`check` 拿眼下的行模型核对,见 [`ChangeKey`])。
/// 不在就重画一帧换上新按钮、返回 `false`,这次点击作罢。
pub(super) fn still_there(
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
pub(super) fn request_hunk_op(
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
    let source = {
        let c = ctl.read(cx);
        (c.context.key(), c.file().path.clone(), c.file().side)
    };
    let (state, ctl) = (state.clone(), ctl.clone());
    Confirm::new(title, message)
        .ok_text(t("gitChanges", "discardOk"))
        .danger()
        .open(
            move |_window, cx| {
                let same_source = {
                    let c = ctl.read(cx);
                    c.context.key() == source.0
                        && c.file().path == source.1
                        && c.file().side == source.2
                };
                if same_source {
                    run_hunk_op(&state, &ctl, op, target.clone(), cx);
                }
            },
            window,
            cx,
        );
}

/// 真正执行块操作:拿点按钮时那份 diff(两侧全文 + 块)与要动的块 / 行交给多块后端,后端
/// 重读两侧比对,对不上就拒绝(见 `mt_project::git::hunk` 模块注释)。几块一趟拼完、只落
/// 一次盘。成功后通知更改列表并原地重取 diff;失败就地红字。
pub(super) fn run_hunk_op(
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
        (c.context.clone(), c.file().path.clone())
    };
    // 按块操作同样写暂存区 / 工作区:与面板上的其它写操作共用一道闸,被占时就地说明
    let Some(guard) = git_ui::gate::try_acquire_key(&repo.repository_key()) else {
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
        let executor = repo.clone();
        let result = mt_remote::run_git_on(&executor, move || match op {
            HunkOp::Stage => repo.stage_hunks(&path, &diff, &picks),
            HunkOp::Unstage => repo.unstage_hunks(&path, &diff, &picks),
            HunkOp::Discard => repo.discard_hunks(&path, &diff, &picks),
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
                tracing::warn!("[git] 按块操作失败: {err:#}");
                c.busy = None;
                c.stale = err.is::<StaleDiff>();
                c.op_error = Some(if c.stale {
                    t("gitCommit", "hunkStale").to_string()
                } else {
                    let detail = if matches!(
                        err.downcast_ref::<mt_remote::GitFailure>(),
                        Some(mt_remote::GitFailure::RepositoryBlocked { .. })
                    ) {
                        git_ui::error_text_for_store(&err, &AppStore::global(cx), cx)
                    } else {
                        git_ui::error_text(&err)
                    };
                    tr!("gitCommit", "hunkFailed", detail = detail)
                });
                cx.notify();
            }),
        });
    })
    .detach();
}

/// 取一份 diff 的三种时机(决定落地时动不动旧内容、滚动位置与勾选)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fetch {
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
pub(super) fn load_file(
    state: &Entity<DiffState>,
    ctl: &Entity<FileDiff>,
    fetch: Fetch,
    cx: &mut App,
) {
    let (repo, file) = {
        let c = ctl.read(cx);
        (c.context.clone(), c.file().clone())
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
        let executor = repo.clone();
        let result = mt_remote::run_git_on(&executor, move || {
            repo.get_git_diff_from(&file.path, file.old_path.as_deref(), file.side)
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
pub(super) fn resync_index(old: &[DiffFile], current: usize, new: &[DiffFile]) -> Option<usize> {
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
pub(super) fn resync_files(
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
pub(super) fn switch_file(
    state: &Entity<DiffState>,
    ctl: &Entity<FileDiff>,
    delta: isize,
    cx: &mut App,
) {
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
pub(super) fn empty_text(file: &DiffFile) -> &'static str {
    if file.old_path.as_deref().is_some_and(|old| old != file.path) {
        t("gitCommit", "renameOnly")
    } else {
        t("gitCommit", "noMoreChanges")
    }
}

/// 当前侧的说明(「暂存区 → 工作区」这类)。
pub(super) fn side_label(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Staged => t("gitCommit", "sideStaged"),
        DiffSide::Unstaged => t("gitCommit", "sideUnstaged"),
        DiffSide::Working => t("gitCommit", "sideWorking"),
    }
}

/// 提交 diff 左栏的宽度(放得下「文件名 + 弱化目录 + 状态字母」一行)。
pub(super) const COMMIT_LIST_W: f32 = 280.0;

/// diff 工具栏左半的文件抬头:文件图标 + 文件名(14px 半粗)+ 弱化的目录 + 状态胶囊。
/// 各种 diff 页签共用(单文件 diff 在后面再接「+12 −3」与当前侧)。
pub(super) fn file_heading(path: &str, status: Option<(&str, gpui::Hsla)>) -> gpui::Div {
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
pub(super) fn title_block(file: &DiffFile, state: &Entity<DiffState>, cx: &App) -> AnyElement {
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
pub(super) fn file_nav(
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
pub(super) fn error_bar(
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
