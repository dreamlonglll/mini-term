//! Git 面板的「更改」区。对应 `src/components/GitChanges.tsx`(442 行),
//! 之后按 IDEA 的 Commit 工具窗补齐了多选、键盘、冲突处理与提交区增强。
//!
//! # 四个分组的口径
//!
//! ```text
//! conflicts = 任一侧是 conflicted(置顶,有冲突才出现)
//! staged    = 有 stagedStatus,且不是冲突
//! unstaged  = 有 unstagedStatus 且不是 untracked,且不是冲突
//! untracked = unstagedStatus === 'untracked'
//! ```
//!
//! ⚠️ **同一个文件可以同时出现在 staged 与 unstaged 两组**(部分暂存),这是正确
//! 行为 —— 所以行的 `ElementId` 必须带区名前缀(原版 key 是 `${area}-${path}`,
//! 规格 §11 第 29 条),否则 gpui 会撞 id。冲突文件在 git2 的状态里两侧都是
//! conflicted,只进冲突组,不再在已暂存 / 未暂存里各出现一次。
//!
//! # 选择与键盘
//!
//! 单击选中(不再直接开 diff —— 否则没法 Shift 连选),**双击 / Enter 开 diff**;
//! Ctrl/Cmd+单击切换、Shift+单击连选,都只在同一分组内。选中多项时右键菜单与行内
//! 按钮对整组生效。列表获得焦点后 ↑/↓ 移动、Space 暂存 / 取消暂存、Delete 丢弃
//! (键位绑在 `hotkeys.rs` 的表外段,上下文 [`LIST_CONTEXT`],终端拿不到焦点时才生效)。
//!
//! # 丢弃 = 只丢未暂存的部分
//!
//! 原先的「丢弃修改」先把文件从暂存区撤回再从 HEAD 检出,部分暂存的文件连已暂存的
//! 那部分一起没了。现在与 `git restore` / IDEA 的回滚一致:从**暂存区**检出
//! (`mt_project::git::discard_unstaged`),已暂存的部分保留;未跟踪文件照旧删除。
//!
//! # 冲突:整文件与逐段
//!
//! 冲突行的右键菜单有整文件的「采用一侧 / 标记为已解决」,也有「逐段解决…」(行内按钮
//! 同一个入口):后者开 [`crate::git::conflict`] 的弹窗,一段一段选采用哪边再一次写回。
//! 能不能逐段解决随列表一起取([`SegmentStatus`],有冲突时才读那几个文件),删除 / 修改
//! 冲突、没有成对标记、二进制、编码认不出、过大时菜单项禁用并写明原因。
//!
//! # 失败要让用户看得见
//!
//! 原版每个 `invoke` 的 catch 都只 `console.error`(不弹 toast、不显红)。这里
//! 不照抄 —— 只打日志的结果是「点了没反应」,而提交被 hook 拒掉恰恰是用户最需要
//! 知道原因的时候。口径按失败出在哪儿分三种(日志照打):
//!
//! - **提交失败**(hook 拒绝 / GPG 出错 / 60s 超时):错误原文**就地**挂在提交按钮下方,
//!   错误色、可滚动、带复制按钮 —— 提交的错误常是 hook 的多行输出,tooltip 装不下。
//!   下一次提交、提交成功或换仓库时清掉(hook 输出得留给人读);
//! - **暂存 / 取消暂存 / 丢弃 / 冲突处理失败**:弹可选中复制原文的详情框
//!   (`git_ui::show_git_error`,与面板上的 Fetch / 检出同一口径)—— toast 5s 就收,
//!   装不下「哪些文件被 hook / 过滤器挡住」这类多行原文;
//! - **取变更失败**:列表区的空态换成错误原文 —— 否则读失败会被误认成
//!   「工作区是干净的」。它随 pty 输出每 500ms 去抖重取,弹框会刷屏,所以就地。
//!
//! # 提交并推送
//!
//! 与 IDEA 的 Commit and Push 一致:提交成功后**打开推送对话框**(预选当前分支),在那里
//! 看清待推送的提交与推送目标再确认;提交失败就不打开。这里不直接推。
//!
//! # 防重入
//!
//! 暂存 / 丢弃 / 提交都写暂存区,经仓库的写操作闸(`git_ui::gate`)执行:面板在拉取、
//! 历史区在拣选时,行内按钮、组操作与提交按钮一并禁用并写明原因。
//!
//! # 阻塞调用
//!
//! 全部 git 调用(git2 的同步 IO 与带超时的 git CLI)经 `mt_remote::run_git`
//! 执行;异步回包用请求令牌防迟到。
//!
//! # 取数调度
//!
//! 列表取数要的人多(工作区监听每 2s 一批、`.git` 监听、pty 嗅探、自己的写操作、diff 页签
//! 按块操作),大仓库一次 status 又要两三秒。[`GitChanges::load`] 因此是**单飞**的
//! ([`git_ui::flight`]):同一时刻只跑一份,期间再要只记脏位,跑完当场补一趟;在途那份照样
//! 落地,但「操作后落回的位置」与在等的 diff 页签留给补跑那份(在途那份可能还不含那次操作)。
//! 每次落地还顺手告诉 diff 页签「这个仓库变了」(`git_diff::notify_repo_changed`):挤在
//! Git 抽屉旁边、眼前那一页 diff 当场静默重取,不必切走再切回来。
//!
//! 勾着「修改上次提交」时的「已推送」判定另走一趟([`GitChanges::refresh_amend`]),不挡
//! 列表:它要逐个远程分支做祖先判定,大仓库可达十几秒。按引用签名缓存 —— HEAD 与远程跟踪
//! 分支都没动就不重取。
//! 本地勾选 amend 立即允许提交,仅勾选时读取元数据失败自动取消;周期刷新失败保留原状态。
//! 远程首次勾选 / 恢复草稿时阻挡提交并在失败时保留意图。之后的周期核验不让按钮闪灰。撤销消息、普通草稿历史、最近提交消息分组共存,恢复消息
//! 保存被替换草稿而不把它重新放回待恢复队列,菜单以稳定身份消费原仓库的条目。
//! 异步提交失败同时核对 store 活动项目与已选仓库,隐藏抽屉保留的旧 context 不能吞掉错误。
//!
//! 暂存 / 丢弃 / 冲突处理写完,在同一趟后台里**先**读一份签名([`RepoObservation`])再重取
//! 列表,经 [`GitChangesEvent::IndexWritten`] 交给面板记作「已知」:紧接着那次 `.git` 检查
//! 比出来没变,不再把列表、文件树重取一遍。
//!
//! # 列表只建看得见的行
//!
//! 根视图每帧都 render,没套 view 缓存的子视图跟着整个重跑 render + layout + paint ——
//! 任一终端输出的一拍、spinner 的一拍、一次悬停都算。此前整张列表每帧连同目录树一起
//! 重建,几千个改动时每一帧都是 0.1s 级。现在:
//!
//! - 列表走 `v_virtual_list`(变高:分组标题连外边距 32、文件 / 目录行 28),只给视口里
//!   那几行建元素;加载中 / 失败 / 空态这类整块占位仍是普通滚动容器;
//! - 分组 + 树形拍平后的行([`ListRow`])、每行高度、摊开总高、四组的显示顺序收在派生
//!   模型 [`ListModel`] 里,**只在数据落地、折叠切换、切视图模式之后**算一次;render、
//!   面板分高度、键盘导航、Shift 连选、diff 页签的文件序列都读这一份;
//! - 取数落地([`GitChanges::apply_snapshot`])先比一次,与眼下的数据一模一样就既不
//!   重算也不重画 —— 监听与 pty 嗅探触发的重取,绝大多数回包都没变。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, ClipboardItem, Context, Entity,
    EventEmitter, FocusHandle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, Render, SharedString, Size, StatefulInteractiveElement, Styled,
    Subscription, Window, canvas, div, point, prelude::FluentBuilder as _, px, size,
};
use gpui_component::input::{InputEvent, Position, Textarea, TextareaState};
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use mt_project::git::{
    AmendInfo, ChangeFileStatus, CommitMessageEntry, CommitTarget, ConflictInfo, ConflictSide,
    GitStatus, RepoOpKind, SegmentStatus,
};
use mt_remote::GitContext;
use mt_ui::tooltip::{Tooltip, TooltipExt as _};

use crate::git::dir_watch::RepoObservation;
use crate::git::ui::flight::{Flight, Landing};
use crate::git::ui::{
    self as git_ui, ButtonKind, ChangeTreeRow, ICON_ALERT, ICON_CHEVRON_DOWN, ICON_CLEAN,
    ICON_COPY, ICON_HISTORY, ICON_MERGE, ICON_MINUS, ICON_PLUS, ICON_UNDO, SectionHeader, Tone,
    danger_icon_button, icon_button, mono, status_color, status_label_for,
};
use crate::git::{conflict as git_conflict, diff as git_diff, watch as git_watch};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuItem};
use crate::prompt::Confirm;
use crate::store::AppStore;
use crate::ui;

gpui::actions!(
    mini_term,
    [
        /// 提交(Ctrl+Enter / Cmd+Enter,`GitChanges.tsx:411-415`)
        GitCommitMessage,
        /// 提交并推送(Ctrl+Alt+Enter / Cmd+Alt+Enter)
        GitCommitAndPush,
        /// 更改列表:选中上一行(↑)
        GitChangesSelectPrev,
        /// 更改列表:选中下一行(↓)
        GitChangesSelectNext,
        /// 更改列表:暂存 / 取消暂存所选(Space)
        GitChangesToggleStage,
        /// 更改列表:查看所选的 diff(Enter)
        GitChangesOpenDiff,
        /// 更改列表:丢弃所选的未暂存修改(Delete)
        GitChangesDiscard,
    ]
);

/// 「更改」区整体的 key context(提交框里的 Ctrl/Cmd+Enter 等,键位见 `hotkeys.rs`)。
pub(crate) const CHANGES_CONTEXT: &str = "GitChanges";

/// 更改列表的 key context(列表获得焦点时生效,键位见 `hotkeys.rs` 表外段)。
pub const LIST_CONTEXT: &str = "GitChangesList";

/// 提交标题的软上限(超出只提示,不拦截)。
const TITLE_SOFT_LIMIT: usize = 72;
/// 标题字数到这里开始显示计数。
const TITLE_COUNT_FROM: usize = 60;
/// 提交消息历史取多少条。
const HISTORY_LIMIT: usize = 20;

/// 「更改」区往上冒的事件。原版是 `onCommitSuccess` 这个 prop。
pub enum GitChangesEvent {
    /// 提交成功 —— 容器要刷新历史与分支(分支头已前移)。`open_push` = 这是「提交并推送」,
    /// 接着打开推送对话框(预选当前分支,见模块注释「提交并推送」)。
    Committed {
        open_push: bool,
        context: GitContext,
    },
    /// Open the push preview for the backend captured by a completed commit.
    OpenPush(GitContext),
    /// ▾ 菜单「贮藏当前改动…」贮藏成功 —— 工作区与贮藏数都变了,容器整面板刷新。
    Stashed,
    /// 暂存 / 取消暂存 / 丢弃 / 冲突处理写完了(成败都算):写完当场、重取列表**之前**读的
    /// 那份签名与冲突数。容器拿它当「已知」,紧接着那次 `.git` 检查就不再重刷一遍
    /// (见模块注释「取数调度」)。
    IndexWritten(RepoObservation),
}

mod commit;
mod context_menu;
mod model;
mod ops;
mod render;

use commit::*;
use model::*;
pub use model::{Area, in_area, message_hints, status_in_area};
use ops::*;
use render::*;

// ─── 组件 ─────────────────────────────────────────────────────

/// 一次后台取数的结果(变更 + 冲突细节 + 提交区要的 HEAD 信息)。
struct Snapshot {
    changes: Vec<ChangeFileStatus>,
    conflicts: Vec<ConflictInfo>,
    /// 每个冲突文件能不能逐段解决,与 `conflicts` 一一对应(读一遍文件判成对标记 / 编码,
    /// 冲突行的「逐段解决…」按它启用或写明原因)
    segments: Vec<SegmentStatus>,
    /// 进行中的合并 / 变基…(冲突菜单「采用 ours / theirs」的文案随它翻转)
    op_kind: Option<RepoOpKind>,
    target: Option<CommitTarget>,
    has_head: bool,
    metadata_warning: Option<String>,
}

/// 取数落地会改到的那几样,落地前拿来比「这份快照收下之后界面变不变」
/// (见 [`GitChanges::apply_snapshot`])。只借不拷。
struct Landed<'a> {
    changes: &'a [ChangeFileStatus],
    conflicts: &'a [ConflictInfo],
    segments: &'a [SegmentStatus],
    op_kind: Option<RepoOpKind>,
    target: Option<&'a CommitTarget>,
    has_head: bool,
    load_error: Option<&'a str>,
    metadata_warning: Option<&'a str>,
}

impl Landed<'_> {
    /// 收下 `snap` 之后有没有东西变(落地的赋值口径见 `apply_snapshot`):冲突细节与
    /// 进行中的操作只进菜单,也照样算 —— 宁可多画一帧,不能让菜单拿着旧数据。
    /// 「已推送」判定不在快照里(另走一趟,见 [`GitChanges::refresh_amend`],变了自己重画)。
    fn differs(&self, snap: &Snapshot) -> bool {
        // Amend intent survives a missing HEAD; metadata independently blocks it.
        !same_changes(self.changes, &snap.changes)
            || self.conflicts != snap.conflicts.as_slice()
            || self.segments != snap.segments.as_slice()
            || self.op_kind != snap.op_kind
            || self.target != snap.target.as_ref()
            || self.has_head != snap.has_head
            || self.metadata_warning != snap.metadata_warning.as_deref()
            || self.load_error.is_some()
    }

    /// 取数失败(列表清空、原因顶替空态)之后有没有东西变:同一条错误反复落地不算。
    fn differs_on_error(&self, error: &str) -> bool {
        !self.changes.is_empty()
            || !self.conflicts.is_empty()
            || !self.segments.is_empty()
            || self.op_kind.is_some()
            || self.load_error != Some(error)
    }
}
pub struct GitChanges {
    store: Entity<AppStore>,
    /// 捕获的仓库后端;`None` 时停在空态,不发起任何 IO。
    context: Option<GitContext>,
    changes: Vec<ChangeFileStatus>,
    /// 冲突文件的三方条目在不在(删除 / 修改冲突时有一侧缺席),随列表一起取
    conflicts: Vec<ConflictInfo>,
    /// 每个冲突文件能不能逐段解决,与 `conflicts` 一一对应,随列表一起取
    segments: Vec<SegmentStatus>,
    /// 进行中的合并 / 变基 / 拣选 / 还原,随列表一起取。变基时 git 的 ours / theirs
    /// 与直觉相反,冲突菜单据此换成写清方向的文案
    op_kind: Option<RepoOpKind>,
    loading: bool,
    loaded_once: bool,
    /// 上一次取变更失败的原文;`Some` 时列表区显示它而不是空态。
    load_error: Option<String>,
    metadata_warning: Option<String>,
    /// 组件态,**不落盘**(抽屉一关就没,规格 §11 第 25 条)。
    collapsed_dirs: HashSet<String>,
    collapsed_groups: HashSet<Area>,
    /// 分组标题 / 目录行的折叠箭头:只让刚点的那一行转(见 `fold_motion`)。
    chevrons: crate::fold_motion::FlipTweens,
    /// 列表取数的单飞调度(见模块注释「取数调度」);换仓库时作废在途的那份。
    flight: Flight,
    /// 派生模型的缓存(见 [`Self::model`])。改了 `changes` / 两种折叠态的地方都要
    /// [`Self::invalidate_model`];视图模式不归这里管,`model` 自己比
    list_model: RefCell<Option<Rc<ListModel>>>,
    /// diff 页签按块操作后在等的回执:最新一次重取落地后,按那时的列表把该分组的新文件
    /// 序列交回去(见 `git_diff::ReloadFiles`)。换仓库时以 `None` 交回。
    diff_waiters: Vec<(Area, git_diff::FilesDone)>,
    /// pty-output 嗅探的 500ms 去抖终点。
    debounce_until: Option<Instant>,
    /// 进行中的整文件操作数(暂存 / 丢弃 / 冲突处理)。`> 0` 时行内与组操作按钮禁用
    busy_ops: usize,

    // ── 选择 ──
    /// 选中项所在的分组。多选只在同一分组内
    sel_area: Option<Area>,
    selected: HashSet<String>,
    /// Shift 连选的锚点
    anchor: Option<String>,
    /// 键盘游标(↑/↓ 从这里走)
    cursor: Option<(Area, String)>,
    /// 操作完重取列表后要落回的位置:同一分组的第 n 行(Space 暂存后停在下一行)
    reselect: Option<(Area, usize)>,
    list_focus: FocusHandle,
    /// 列表的滚动位置。虚拟列表与加载中 / 空态那几种普通滚动容器共用(后者走它底下那个
    /// `ScrollHandle`);必须存在视图上,每帧新建等于每帧把滚动清零
    list_scroll: VirtualListScrollHandle,
    /// 下一帧把键盘游标所在的行滚进视野(键盘走动、操作后落回、diff 页签里换了文件)。
    /// 到 render 时才按当帧的行序找行,prepaint 时按当帧的视口算偏移
    reveal_cursor: bool,

    // ── 提交区 ──
    commit_input: Entity<TextareaState>,
    drafts: HashMap<String, CommitDraft>,
    blank_draft: Option<CommitDraft>,
    input_subscription: Option<Subscription>,
    amend_drafts: HashMap<String, (bool, Option<String>)>,
    pending_messages: HashMap<String, Vec<SavedMessage>>,
    saved_messages: HashMap<String, Vec<SavedMessage>>,
    next_message_id: u64,
    input_key: Option<String>,
    input_revision: Rc<Cell<u64>>,
    /// 占位符里写的分支(随列表一起取)
    target: Option<CommitTarget>,
    /// 当前占位符的文本 —— 只在变化时改,render 里改别的实体要克制
    placeholder: Option<String>,
    has_head: bool,
    amend: bool,
    amend_info: Option<AmendInfo>,
    /// amend 勾上时预填进去的那段消息(取消勾选时消息没动过就清掉)
    amend_prefill: Option<String>,
    amend_request: u64,
    /// 勾着 amend 时随列表重取「已推送」判定的那一趟(单飞,见 [`Self::refresh_amend`])
    amend_flight: Flight,
    amend_verifying: bool,
    /// 眼下 `amend_info` 是在哪份引用签名下取的:没变就不重取
    amend_key: Option<u64>,
    committing: bool,
    history_loading: bool,
    /// 上一次提交失败的说明(已格式化好的整句),挂在提交按钮下方。
    commit_error: Option<String>,
    /// 提交区整块的高度(canvas 量的):面板按内容分高度时要([`Self::natural_height`])。
    commit_area_h: Rc<Cell<f32>>,
    /// 提交按钮那一行(主按钮 + ▾)的窗口坐标:▾ 菜单贴着它的下沿 / 上沿弹,不压按钮。
    commit_buttons: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl EventEmitter<GitChangesEvent> for GitChanges {}

impl GitChanges {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let commit_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder(t("panels", "commitPlaceholder"))
        });
        let input_revision = Rc::new(Cell::new(0u64));
        let revision = input_revision.clone();
        let subscription = cx.subscribe(&commit_input, move |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                revision.set(revision.get().wrapping_add(1));
                cx.notify();
            }
        });
        Self {
            store,
            context: None,
            changes: Vec::new(),
            conflicts: Vec::new(),
            segments: Vec::new(),
            op_kind: None,
            loading: false,
            loaded_once: false,
            load_error: None,
            metadata_warning: None,
            collapsed_dirs: HashSet::new(),
            collapsed_groups: HashSet::new(),
            chevrons: crate::fold_motion::FlipTweens::new(mt_ui::motion::CHEVRON_ROTATE),
            flight: Flight::default(),
            list_model: RefCell::new(None),
            diff_waiters: Vec::new(),
            debounce_until: None,
            busy_ops: 0,
            sel_area: None,
            selected: HashSet::new(),
            anchor: None,
            cursor: None,
            reselect: None,
            list_focus: cx.focus_handle(),
            list_scroll: VirtualListScrollHandle::new(),
            reveal_cursor: false,
            commit_input,
            drafts: HashMap::new(),
            blank_draft: None,
            input_subscription: Some(subscription),
            amend_drafts: HashMap::new(),
            pending_messages: HashMap::new(),
            saved_messages: HashMap::new(),
            next_message_id: 0,
            input_key: None,
            input_revision,
            target: None,
            placeholder: None,
            has_head: false,
            amend: false,
            amend_info: None,
            amend_prefill: None,
            amend_request: 0,
            amend_flight: Flight::default(),
            amend_verifying: false,
            amend_key: None,
            committing: false,
            history_loading: false,
            commit_error: None,
            commit_area_h: Rc::new(Cell::new(0.0)),
            commit_buttons: Rc::new(Cell::new(None)),
        }
    }

    /// 「更改」区整块摊开(列表不滚动)要多高:列表内容 + 上下留白 + 提交区。面板分配
    /// 「更改 / 提交历史」两块高度时用(`git_panel::section_split`)。提交区是上一帧量的;
    /// 列表有分组时读派生模型的摊开总高([`ListModel::height`]),否则也取上一帧量的。
    /// 还没画过时为 `None`。
    ///
    /// 列表高度按**眼下的数据**、不按上一帧量:外部改动多出一组(未暂存)的那一帧,
    /// 「更改」区还是按旧内容分的高度,排在最后的分组(未跟踪)被挤出可见区、下一帧才
    /// 回来(E2E r4 A12 的过渡帧)。模型是缓存的,面板一帧问几次都只是读一个数。
    pub fn natural_height(&self, cx: &App) -> Option<f32> {
        let commit = self.commit_area_h.get();
        if commit <= 0.0 {
            return None;
        }
        let list = match self.model(cx).height {
            Some(list) => list,
            None => {
                let last = self.list_scroll.children_count().checked_sub(1)?;
                let first = self.list_scroll.bounds_for_item(0)?;
                let last = self.list_scroll.bounds_for_item(last)?;
                // 列表的上下内边距(4 + 8)+ 首个分组标题的上外边距 4
                f32::from(last.bottom() - first.top()) + LIST_PAD_Y + GROUP_HEADER_GAP
            }
        };
        Some(list + commit)
    }

    /// 「更改」区最矮要多高:提交区整块 + 列表至少露出一行(列表本身更矮时按列表)。
    /// 中缝拖到下限时面板按它兜底,提交按钮整颗露着、点得到(E2E r4 B11)。`natural` 是
    /// 同一帧的 [`Self::natural_height`](面板已经问过一次,不再算第二遍)。还没画过时
    /// 为 `None`。
    pub fn min_height(&self, natural: Option<f32>) -> Option<f32> {
        let commit = self.commit_area_h.get();
        if commit <= 0.0 {
            return None;
        }
        let floor = commit + LIST_PAD_Y + GROUP_HEADER_GAP + ROW_HEIGHT;
        Some(natural.map_or(floor, |natural| natural.min(floor)))
    }

    /// 换仓库(容器调)。`None` = 无仓库;后端身份随上下文一起比较。
    pub fn set_repo(&mut self, repo: Option<GitContext>, cx: &mut Context<Self>) {
        if self.context == repo {
            return;
        }
        if let Some(key) = self.input_key.as_ref()
            && self.context.as_ref().map(GitContext::key).as_ref() == Some(key)
        {
            self.amend_drafts
                .insert(key.clone(), (self.amend, self.amend_prefill.clone()));
        }
        self.context = repo;
        // Inputs are retained by backend key. Until render attaches the selected
        // draft, no keyboard action may submit the previous repository's input.
        self.placeholder = None;
        self.loading = false;
        self.reselect = None;
        self.debounce_until = None;
        self.loaded_once = false;
        self.busy_ops = 0;
        self.committing = false;
        self.history_loading = false;
        self.amend_request += 1;
        self.changes.clear();
        self.conflicts.clear();
        self.segments.clear();
        self.op_kind = None;
        self.collapsed_dirs.clear();
        self.invalidate_model();
        self.clear_selection();
        self.target = None;
        self.has_head = false;
        // amend 属于上一个仓库的 HEAD
        self.amend = false;
        self.amend_info = None;
        self.amend_prefill = None;
        self.amend_key = None;
        self.amend_flight.reset();
        self.amend_verifying = false;
        if let Some(key) = self.context.as_ref().map(GitContext::key)
            && let Some((amend, prefill)) = self.amend_drafts.get(&key)
        {
            self.amend = *amend;
            self.amend_prefill = prefill.clone();
        }
        // 错误都属于上一个仓库
        self.load_error = None;
        self.commit_error = None;
        self.metadata_warning = None;
        // 在等序列的 diff 页签属于上一个仓库:沿用它们的旧序列
        self.flush_diff_waiters(false, cx);
        // 上一个仓库在途的那趟作废:回来一律丢,新仓库这趟不必等它
        self.flight.reset();
        self.load(cx);
        cx.notify();
    }

    /// 容器的 pty-output 嗅探命中了 —— 起(或推后)自己那个 500ms 去抖窗口。
    ///
    /// 原版是两个面板**各有一个**定时器(规格 §11 第 27 条),所以这里的窗口
    /// 与提交历史区各算各的。
    pub fn note_pty_hit(&mut self) {
        self.debounce_until = Some(Instant::now() + Duration::from_millis(git_watch::DEBOUNCE_MS));
    }

    /// 容器的节拍:去抖窗口到点了就重取。
    pub fn tick(&mut self, cx: &mut Context<Self>) {
        if self.debounce_until.is_some_and(|at| Instant::now() >= at) {
            self.debounce_until = None;
            self.load(cx);
        }
    }

    /// 重取列表(单飞,见模块注释「取数调度」)。勾着 amend 时顺带另起一趟刷新「已推送」判定。
    pub fn load(&mut self, cx: &mut Context<Self>) {
        if self.context.is_none() {
            self.flush_diff_waiters(false, cx);
            return;
        }
        self.refresh_amend(cx);
        self.start_load(cx);
    }

    /// 在 `at` 那一刻之前的工作区 / 索引变化,列表这边是不是已经有一趟取数会反映出来
    /// (在那之后才开跑的,或者排着补跑)。面板拿它判断监听交来的一批要不要再要一次。
    pub fn load_covers(&self, at: Instant) -> bool {
        self.flight.covers(at)
    }

    fn start_load(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.context.clone() else {
            return;
        };
        // 在途:只记脏位,那趟落地后当场补跑
        let Some(token) = self.flight.begin(Instant::now()) else {
            return;
        };
        self.loading = !self.loaded_once;
        cx.spawn(async move |this, cx| {
            let result = mt_remote::run_git_on(&repo.clone(), move || load_snapshot(&repo)).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                let rerun = match this.flight.finish(token) {
                    Landing::Stale => return,
                    Landing::Done { rerun } => rerun,
                };
                this.apply_snapshot(result, rerun, cx);
                if rerun {
                    this.start_load(cx);
                }
            });
        })
        .detach();
        // 只有首次取数显示加载态;空的稳定快照也不能在轮询时反复闪烁。
        if self.loading {
            cx.notify();
        }
    }

    /// 勾着「修改上次提交」时重取「已推送」判定(见模块注释「取数调度」):单独一趟、不挡
    /// 列表;引用签名没变就只算一次签名、不重取。没勾 / 无仓库时什么都不做。
    fn refresh_amend(&mut self, cx: &mut Context<Self>) {
        if !self.amend || self.context.is_none() {
            return;
        }
        let Some(token) = self.amend_flight.begin(Instant::now()) else {
            return;
        };
        self.amend_verifying =
            self.context.as_ref().is_some_and(GitContext::is_remote) && self.amend_info.is_none();
        let req = self.amend_request;
        let Some(repo) = self.context.clone() else {
            return;
        };
        let known = self.amend_key;
        cx.spawn(async move |this, cx| {
            let fetched =
                mt_remote::run_git_on(&repo.clone(), move || fetch_amend(&repo, known)).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                let rerun = match this.amend_flight.finish(token) {
                    Landing::Stale => return,
                    Landing::Done { rerun } => rerun,
                };
                this.amend_verifying = false;
                cx.notify();
                // 期间取消过勾选(令牌前移)就作废;换仓库时 amend_flight 已作废,走不到这里
                if this.amend
                    && this.amend_request == req
                    && let Some((key, info)) = fetched
                {
                    this.amend_key = info.as_ref().and(key);
                    if info.is_none() && this.context.as_ref().is_some_and(GitContext::is_remote) {
                        this.commit_error =
                            Some(t("gitChanges", "metadataUnavailable").to_string());
                        cx.notify();
                    }
                    if this.amend_info != info {
                        this.amend_info = info;
                        cx.notify();
                    }
                }
                if rerun {
                    this.refresh_amend(cx);
                }
            });
        })
        .detach();
    }

    /// 一次取数落地(迟到的回包单飞调度已经丢掉了)。收下之后界面有东西变(数据、选择、
    /// 加载态)才重算派生模型、才重画;与眼下一模一样的回包什么都不碰 —— 此前每次落地
    /// 都无条件 notify,监听 / pty 嗅探触发的每一趟都是一次整窗帧。
    ///
    /// `rerun` = 在途期间又有人要过、落地后当场补跑:这份照样落(比画着的新),但它可能
    /// 还不含触发补跑的那次操作 ——「操作后落回的位置」与在等的 diff 页签留给补跑那份。
    /// 不补跑时 diff 页签的回执照交(它要的是「这一次重取落地了」,不管变没变)。取到了
    /// 还告诉 diff 页签这个仓库变了(见模块注释「取数调度」),不管列表这边变没变 ——
    /// 已经改过的文件接着改,状态字母不变,内容却变了。
    fn apply_snapshot(
        &mut self,
        result: anyhow::Result<Snapshot>,
        rerun: bool,
        cx: &mut Context<Self>,
    ) {
        let loading_shown = self.shows_loading();
        self.loading = false;
        self.loaded_once = true;
        let reselect = if rerun { self.reselect.take() } else { None };
        let loaded = result.is_ok();
        let changed = match result {
            Ok(snap) => {
                let changed = self.landed().differs(&snap);
                self.changes = snap.changes;
                self.conflicts = snap.conflicts;
                self.segments = snap.segments;
                self.op_kind = snap.op_kind;
                self.target = snap.target;
                self.has_head = snap.has_head;
                // Keep amend intent if HEAD disappears; never silently submit normally.
                self.load_error = None;
                self.metadata_warning = snap.metadata_warning;
                changed
            }
            Err(err) => {
                // 原版 `.catch(() => setChanges([]))` —— 失败即清空;
                // 这里另把原因留给列表区显示,别让人当成「没有变更」
                tracing::warn!("[git] 取变更失败: {err:#}");
                let error = format!("{err:#}");
                let changed = self.landed().differs_on_error(&error);
                self.changes.clear();
                self.conflicts.clear();
                self.segments.clear();
                self.op_kind = None;
                self.load_error = Some(error);
                self.metadata_warning = None;
                changed
            }
        };
        if changed {
            self.invalidate_model();
        }
        let reselected = self.after_reload(cx);
        if rerun {
            self.reselect = reselect;
        } else {
            self.flush_diff_waiters(loaded, cx);
        }
        if loaded && let Some(repo) = self.context.clone() {
            // Diff callbacks may re-enter this list, so notify after this update.
            cx.defer(move |cx| git_diff::notify_repo_changed(&repo, cx));
        }
        if changed || reselected || loading_shown != self.shows_loading() {
            cx.notify();
        }
    }

    /// 落地前的比对视图(见 [`Landed`])。
    fn landed(&self) -> Landed<'_> {
        Landed {
            changes: &self.changes,
            conflicts: &self.conflicts,
            segments: &self.segments,
            op_kind: self.op_kind,
            target: self.target.as_ref(),
            has_head: self.has_head,
            load_error: self.load_error.as_deref(),
            metadata_warning: self.metadata_warning.as_deref(),
        }
    }

    /// 列表区此刻画的是不是「加载中」(只有没数据、也没报错时才画它)。
    fn shows_loading(&self) -> bool {
        self.loading && self.changes.is_empty() && self.load_error.is_none()
    }

    /// 把在等序列的 diff 页签回执交出去:`loaded` 时按眼下的列表给该分组的新序列,否则
    /// `None`(页签沿用旧序列)。回执改的是页签的实体、还可能回调本列表(换了文件时让
    /// 选中项跟过去),延到这一轮更新之后再调,免得本实体正被借着时重入。
    fn flush_diff_waiters(&mut self, loaded: bool, cx: &mut Context<Self>) {
        if self.diff_waiters.is_empty() {
            return;
        }
        let model = self.model(cx);
        for (area, done) in std::mem::take(&mut self.diff_waiters) {
            let files = loaded.then(|| self.diff_files(model.order(area), area));
            cx.defer(move |cx| done(files, cx));
        }
    }

    fn tree_mode(&self, cx: &App) -> bool {
        self.store.read(cx).git_changes_view_mode() == "tree"
    }

    /// 派生模型(见 [`ListModel`])。缓存着、且是按眼下的视图模式算的就直接给,否则按
    /// 眼下的数据算一份存起来 —— 视图模式住在 store 里(面板标题栏的切换钮、设置同步都
    /// 能改它),这里每次比一下,不靠谁记得来作废。
    fn model(&self, cx: &App) -> Rc<ListModel> {
        let tree_mode = self.tree_mode(cx);
        if let Some(model) = self
            .list_model
            .borrow()
            .as_ref()
            .filter(|model| model.tree_mode == tree_mode)
        {
            return model.clone();
        }
        let model = Rc::new(ListModel::build(
            &self.changes,
            &self.collapsed_groups,
            &self.collapsed_dirs,
            tree_mode,
        ));
        *self.list_model.borrow_mut() = Some(model.clone());
        model
    }

    /// 派生模型作废(`changes` / 分组折叠 / 目录折叠变了),下次用到时重算。
    fn invalidate_model(&self) {
        self.list_model.borrow_mut().take();
    }

    // ── 选择 ──

    fn clear_selection(&mut self) {
        self.sel_area = None;
        self.selected.clear();
        self.anchor = None;
        self.cursor = None;
    }

    fn is_selected(&self, area: Area, path: &str) -> bool {
        self.sel_area == Some(area) && self.selected.contains(path)
    }

    fn select_single(&mut self, area: Area, index: usize) {
        let Some(path) = self.changes.get(index).map(|c| c.path.clone()) else {
            return;
        };
        self.sel_area = Some(area);
        self.selected.clear();
        self.selected.insert(path.clone());
        self.anchor = Some(path.clone());
        self.cursor = Some((area, path));
    }

    /// 单击行:修饰键决定是单选、切换还是连选;双击开 diff。
    fn click_row(
        &mut self,
        area: Area,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.list_focus, cx);
        let Some(path) = self.changes.get(index).map(|c| c.path.clone()) else {
            return;
        };
        let modifiers = event.modifiers();
        let same_area = self.sel_area == Some(area);
        if modifiers.shift && same_area && self.anchor.is_some() {
            let model = self.model(cx);
            let order = model.order(area);
            let anchor = self.anchor.clone().unwrap_or_default();
            let pos = |p: &str| order.iter().position(|&i| self.changes[i].path == p);
            if let (Some(a), Some(b)) = (pos(&anchor), pos(&path)) {
                let (lo, hi) = (a.min(b), a.max(b));
                self.selected = order[lo..=hi]
                    .iter()
                    .map(|&i| self.changes[i].path.clone())
                    .collect();
                self.cursor = Some((area, path));
            } else {
                self.select_single(area, index);
            }
        } else if modifiers.secondary() {
            if !same_area {
                self.select_single(area, index);
            } else {
                if !self.selected.remove(&path) {
                    self.selected.insert(path.clone());
                }
                self.anchor = Some(path.clone());
                self.cursor = Some((area, path));
                if self.selected.is_empty() {
                    self.sel_area = None;
                }
            }
        } else {
            self.select_single(area, index);
            if event.click_count() >= 2 {
                self.view_diff(index, area, window, cx);
            }
        }
        cx.notify();
    }

    /// 某一行上的动作要作用于哪些文件:行在选中集里 → 整个选中集(按显示顺序),
    /// 否则只有它自己。
    fn targets(&self, area: Area, index: usize, cx: &App) -> Vec<String> {
        let Some(path) = self.changes.get(index).map(|c| c.path.as_str()) else {
            return Vec::new();
        };
        if !self.is_selected(area, path) || self.selected.len() <= 1 {
            return vec![path.to_string()];
        }
        self.model(cx)
            .order(area)
            .iter()
            .map(|&i| self.changes[i].path.clone())
            .filter(|p| self.selected.contains(p))
            .collect()
    }

    /// 当前选中集(键盘动作用),按显示顺序。
    fn selection_targets(&self, cx: &App) -> Option<(Area, Vec<String>)> {
        let area = self.sel_area?;
        let paths: Vec<String> = self
            .model(cx)
            .order(area)
            .iter()
            .map(|&i| self.changes[i].path.clone())
            .filter(|p| self.selected.contains(p))
            .collect();
        (!paths.is_empty()).then_some((area, paths))
    }

    /// 选中集里第一项在本组显示顺序里的位置(操作完落回这里 = 停在「下一行」)。
    fn selection_anchor_pos(&self, area: Area, cx: &App) -> Option<usize> {
        self.model(cx)
            .order(area)
            .iter()
            .position(|&i| self.selected.contains(&self.changes[i].path))
    }

    /// 重取列表之后:清掉已不在原分组里的选中项;有待落回的位置就落回去。选择因此
    /// 变了就返回 `true`(数据没变时落地也得为它重画)。
    fn after_reload(&mut self, cx: &mut Context<Self>) -> bool {
        let model = self.model(cx);
        if let Some((area, pos)) = self.reselect.take() {
            let order = model.order(area);
            if let Some(&index) = order.get(pos).or(order.last()) {
                self.select_single(area, index);
                self.reveal_cursor = true;
                return true;
            }
            self.clear_selection();
            return true;
        }
        let Some(area) = self.sel_area else {
            return false;
        };
        let present: HashSet<&str> = model
            .members(area)
            .iter()
            .map(|&i| self.changes[i].path.as_str())
            .collect();
        let before = self.selected.len();
        self.selected.retain(|p| present.contains(p.as_str()));
        if self.selected.is_empty() {
            self.clear_selection();
            true
        } else if self
            .cursor
            .as_ref()
            .is_some_and(|(_, p)| !present.contains(p.as_str()))
        {
            self.cursor = None;
            true
        } else {
            self.selected.len() != before
        }
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows: Vec<(Area, usize)> = self.model(cx).nav().collect();
        if rows.is_empty() {
            return;
        }
        let current = self.cursor.as_ref().and_then(|(area, path)| {
            rows.iter()
                .position(|&(a, i)| a == *area && self.changes[i].path == *path)
        });
        let next = match current {
            None if delta > 0 => 0,
            None => rows.len() - 1,
            Some(pos) => (pos as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
        };
        let (area, index) = rows[next];
        self.select_single(area, index);
        self.reveal_cursor = true;
        cx.notify();
    }

    fn on_select_prev(&mut self, _: &GitChangesSelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(-1, cx);
    }

    fn on_select_next(&mut self, _: &GitChangesSelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(1, cx);
    }

    /// Space:未暂存 / 未跟踪 → 暂存,已暂存 → 取消暂存。完成后停在原位置的下一行。
    fn on_toggle_stage(
        &mut self,
        _: &GitChangesToggleStage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 上一次操作还在跑:连按 Space 不叠第二次(与按钮在 busy 时禁用同一口径)
        if self.busy_ops > 0 {
            return;
        }
        let Some((area, paths)) = self.selection_targets(cx) else {
            return;
        };
        let pos = self.selection_anchor_pos(area, cx).unwrap_or(0);
        match area {
            Area::Staged => self.unstage(paths, Some((area, pos)), window, cx),
            Area::Unstaged | Area::Untracked => self.stage(paths, Some((area, pos)), window, cx),
            Area::Conflicts => {}
        }
    }

    fn on_open_diff(
        &mut self,
        _: &GitChangesOpenDiff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((area, path)) = self.cursor.clone() else {
            return;
        };
        if let Some(index) = self.changes.iter().position(|c| c.path == path) {
            self.view_diff(index, area, window, cx);
        }
    }

    fn on_discard_key(
        &mut self,
        _: &GitChangesDiscard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy_ops > 0 {
            return;
        }
        if let Some((area @ (Area::Unstaged | Area::Untracked), paths)) = self.selection_targets(cx)
        {
            self.discard(area, paths, window, cx);
        }
    }
}

/// 一次取数:变更列表 + 冲突细节 + 提交区要的 HEAD 信息。全部是只读的 git2 查询。
/// amend 的「已推送」判定不在这里(见 [`GitChanges::refresh_amend`])。
fn load_snapshot(repo: &GitContext) -> anyhow::Result<Snapshot> {
    let changes = repo
        .get_changes_status()
        .map_err(|err| err.context(git_ui::repo_label(repo)))?;
    snapshot_with_metadata(
        changes,
        || repo.list_conflicts(),
        |infos| repo.conflict_segment_statuses(infos),
        || repo.repo_operation_state().map(|state| state.kind),
        || repo.commit_target(),
        || repo.head_exists(),
    )
}

/// Auxiliary failures retain groups, but never enable unsafe conflict actions.
fn snapshot_with_metadata(
    changes: Vec<ChangeFileStatus>,
    conflicts: impl FnOnce() -> anyhow::Result<Vec<ConflictInfo>>,
    segments: impl FnOnce(&[ConflictInfo]) -> anyhow::Result<Vec<SegmentStatus>>,
    operation: impl FnOnce() -> anyhow::Result<Option<RepoOpKind>>,
    target: impl FnOnce() -> anyhow::Result<CommitTarget>,
    head: impl FnOnce() -> bool,
) -> anyhow::Result<Snapshot> {
    let mut warnings = Vec::new();
    let conflicts = if changes.iter().any(is_conflicted) {
        match conflicts() {
            Ok(conflicts) => conflicts,
            Err(err) => {
                warnings.push(git_ui::error_text(&err));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let segments = if conflicts.is_empty() {
        Vec::new()
    } else {
        match segments(&conflicts) {
            Ok(segments) => segments,
            Err(err) => {
                warnings.push(git_ui::error_text(&err));
                vec![
                    SegmentStatus::Blocked(mt_project::git::SegmentBlocker::Unreadable);
                    conflicts.len()
                ]
            }
        }
    };
    let op_kind = match operation() {
        Ok(kind) => kind,
        Err(err) => {
            warnings.push(git_ui::error_text(&err));
            None
        }
    };
    let target = match target() {
        Ok(target) => Some(target),
        Err(err) => {
            warnings.push(git_ui::error_text(&err));
            None
        }
    };
    Ok(Snapshot {
        changes,
        conflicts,
        segments,
        op_kind,
        target,
        has_head: head(),
        metadata_warning: (!warnings.is_empty()).then(|| warnings.join("\n\n")),
    })
}

#[cfg(test)]
mod tests;
