//! Git 面板容器。对应 `src/components/GitHistory.tsx`(548 行)。
//!
//! ```text
//! ① 仓库栏(repos 非空时才画)h36
//!    ├ 左段:仓库图标 + 名称 ▾          右键 = 「在终端中打开」/「Worktree 管理」
//!    │       分支图标 + 分支胶囊 + ↑n / ↓m 计数胶囊   点开 = 分支弹窗(git_branches)
//!    └ 右段:Fetch / 拉取(右键选方式)/ 推送(开推送对话框)/ 贮藏(开贮藏列表,
//!            有贮藏时旁边挂数量胶囊)/ 刷新
//! ①' 进行中操作横幅(合并 / 变基 / 拣选 / 还原时才画)h36
//! ② SectionHeader「更改」  h30   右侧挂 视图切换(树 / 列表)
//! ③ GitChanges           .git-section-body
//! ④ 中缝拖拽手柄(两块都展开时)
//! ⑤ SectionHeader「提交历史」h30(带上边框)
//! ⑥ GitHistoryContent    .git-section-body
//! ```
//!
//! # 两块折叠区**常驻挂载**
//!
//! 折叠只把高度收到 0(`flex-grow: 0` + `flex-basis: 0`),**不摘子实体** ——
//! 已加载的 commits、提交草稿都不丢(`GitHistory.tsx:111-112` 原注释)。
//!
//! # 视图状态放哪
//!
//! `sectionUi`(两块的展开态与比例)是**模块级、不落盘**的临时视图状态
//! (`GitHistory.tsx:13-15` 原注释)。GPUI 侧对应 [`SECTION_UI`] 这个 `thread_local`,
//! 与 `overlay.rs` / `ui.rs` 的先例同一形态。拉取方式([`PULL_MODE`])同理。
//!
//! # 可见性闸
//!
//! [`GitPanel::set_visible`] 收起时**不跑 `discover_git_repos`**(它要扫盘,
//! 大 monorepo 上是秒级);同时开关 [`git_watch`] 的输出旁路总闸与 `.git` 目录监听。
//! 范式照 `SessionPanel::set_visible`。
//!
//! # 写操作(分支 / 同步)
//!
//! Fetch / 拉取 / 推送 / 检出 / 合并 / 变基 / 删除…统一走 [`GitPanel::run_write_op`]:
//! **同一时刻只跑一个**([`GitBusy`],git 自己也靠 `index.lock` 互斥),进行中的那颗
//! 按钮转圈、其余禁用;成功推一条**带结果的摘要** toast(「已检出 feature/x」「拉取了
//! 5 个提交」),失败弹可选中复制原文的详情框([`git_ui::show_git_error`] —— toast
//! 5s 就收,装不下 git 的多行报错);完了一律整面板刷新。
//!
//! 跨区的防重入靠仓库的写操作闸(`git_ui::gate`,经 [`git_ui::spawn_gated`] 占 / 放):
//! 更改区在提交、历史区在拣选、推送 / 贮藏弹窗在执行时,仓库栏的写按钮与分支弹窗的动作
//! 一并禁用,tooltip 写明「另一项 Git 操作正在进行」;只读刷新不受影响。
//!
//! # 自动刷新
//!
//! 两路:原有的 pty 输出旁路([`git_watch`]),外加 `.git` 目录监听
//! ([`crate::git_dir_watch`]):AI agent 在终端里提交、外部工具切分支后面板自己跟上。
//! 目录监听去抖之后先在后台算 [`RepoSignature`],**没变就不刷**(防别的程序只刷新
//! 索引 stat 缓存时白刷一遍);只变了索引就只重取「更改」区与冲突数,引用变了才连
//! 分支、计数、提交历史一起刷。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Task, Window, canvas, div, prelude::FluentBuilder as _, px,
};
use mt_project::git::{
    BranchInfo, BranchTracking, GitRepoInfo, PullConfig, PullMode, PullOutcome, PushPlan,
    RepoOpKind, RepoOpState, RepoSignature,
};
use mt_ui::tooltip::TooltipExt as _;

use crate::git_changes::{GitChanges, GitChangesEvent};
use crate::git_dir_watch::GitDirWatch;
use crate::git_history::{GitHistoryContent, GitHistoryEvent};
use crate::git_ui::{self, Tone};
use crate::git_watch;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuItem};
use crate::prompt::Confirm;
use crate::store::{AppStore, StoreEvent};
use crate::ui;

/// 两块折叠区的会话级视图状态。**有意不落盘**。
#[derive(Clone, Copy)]
struct SectionUi {
    changes_open: bool,
    history_open: bool,
    ratio: f32,
}

thread_local! {
    static SECTION_UI: RefCell<SectionUi> = const {
        RefCell::new(SectionUi {
            changes_open: true,
            history_open: true,
            ratio: 0.5,
        })
    };

    /// `src/utils/projectDataCache.ts` 的对应物:项目路径 → (仓库列表, 选中仓库)。
    /// 换项目回来时先吃缓存再重新发现,避免每次都白屏一下。
    static REPO_CACHE: RefCell<HashMap<String, (Vec<GitRepoInfo>, String)>> =
        RefCell::new(HashMap::new());

    /// 拉取方式(仓库栏「拉取」右键菜单里选)。会话级、**不落盘**,左键按它拉;
    /// `None` = 本次会话还没手选(或选回了「跟随 git 配置」):不带方式参数拉,完全交给
    /// 用户的 git 配置([`mt_project::git::git_pull_by_config`]),tooltip 照
    /// [`RepoMeta::pull_config`] 显示「跟随配置:…」。
    static PULL_MODE: Cell<Option<PullMode>> = const { Cell::new(None) };
}

/// 区块比例的钳位(`GitHistory.tsx:17`)。
pub fn clamp_ratio(r: f32) -> f32 {
    r.clamp(0.15, 0.85)
}

/// 仓库栏 / 横幅的高度。两块内容区的可分配高度要扣掉它们。
const BAR_HEIGHT: f32 = 36.0;
/// 「最近」分组最多几条。
const RECENT_LIMIT: usize = 5;

/// 正在跑的写操作。同一时刻只跑一个 —— git 自己也靠 `index.lock` 互斥,并发点两下
/// 只会让第二个报「另一个 git 进程正在运行」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitBusy {
    Fetch,
    Pull,
    Push,
    Checkout,
    Create,
    Rename,
    Delete,
    Merge,
    Rebase,
    Continue,
    Abort,
}

impl GitBusy {
    /// 动作名(失败弹窗标题「{动作}失败」、按钮提示「{动作}中…」)。
    pub(crate) fn label(self) -> &'static str {
        t(
            "gitBranch",
            match self {
                GitBusy::Fetch => "opFetch",
                GitBusy::Pull => "opPull",
                GitBusy::Push => "opPush",
                GitBusy::Checkout => "opCheckout",
                GitBusy::Create => "opCreate",
                GitBusy::Rename => "opRename",
                GitBusy::Delete => "opDelete",
                GitBusy::Merge => "opMerge",
                GitBusy::Rebase => "opRebase",
                GitBusy::Continue => "opContinue",
                GitBusy::Abort => "opAbort",
            },
        )
    }

    /// 改分支 / HEAD 的那几种:进行中时分支徽章旁转圈。
    fn touches_branch(self) -> bool {
        matches!(
            self,
            GitBusy::Checkout
                | GitBusy::Create
                | GitBusy::Rename
                | GitBusy::Delete
                | GitBusy::Merge
                | GitBusy::Rebase
        )
    }
}

/// 随分支列表一起在后台读的仓库元数据。
#[derive(Clone, Default)]
struct RepoMeta {
    /// HEAD 的显示名(分支名;detached 是 `"(1a2b3c4)"`)。比仓库列表里那份新 ——
    /// 那份要等下一次重新发现仓库才更新。
    head_label: Option<String>,
    /// HEAD 分支相对上游的领先 / 落后。
    head_tracking: Option<BranchTracking>,
    /// 全部本地分支的领先 / 落后(分支弹窗逐行用)。
    tracking: Vec<BranchTracking>,
    recent: Vec<String>,
    remotes: Vec<String>,
    /// 进行中的合并 / 变基…(横幅)。
    op: RepoOpState,
    /// 读这份快照时的仓库签名 —— `.git` 监听的「没变就不刷」拿它比。
    signature: Option<RepoSignature>,
    /// 仓库 git 配置里的拉取方式(`branch.<b>.rebase` / `pull.rebase` / `pull.ff`),
    /// 只用于显示:没手选时拉取不带方式参数,由 git 自己读配置。
    pull_config: PullConfig,
    /// HEAD 分支的推送计划(推送按钮的 tooltip 写真实目标:目标可以不是上游)。
    push_plan: Option<PushPlan>,
    /// 贮藏条数(仓库栏贮藏按钮旁的数量胶囊)。
    stash_count: usize,
}

/// 后台读一份分支快照。只读;除分支列表外,单项查询失败不拖垮整份。
fn load_branch_snapshot(repo: &Path) -> anyhow::Result<(Vec<BranchInfo>, RepoMeta)> {
    use mt_project::git as g;
    let branches = g::get_repo_branches(repo)?;
    let meta = RepoMeta {
        head_label: g::current_head_label(repo).ok().flatten(),
        head_tracking: g::ahead_behind(repo).ok().flatten(),
        tracking: g::local_branch_tracking(repo).unwrap_or_default(),
        recent: g::recent_branches(repo, RECENT_LIMIT).unwrap_or_default(),
        remotes: g::list_remotes(repo).unwrap_or_default(),
        op: g::repo_operation_state(repo).unwrap_or_default(),
        signature: g::repo_signature(repo).ok(),
        pull_config: g::pull_config(repo),
        push_plan: g::push_plan(repo, None).ok(),
        stash_count: g::stash_count(repo).unwrap_or_default(),
    };
    Ok((branches, meta))
}

/// 分支弹窗要的一份快照(弹窗每帧从面板现取,数据始终与仓库栏一致)。
#[derive(Clone)]
pub(crate) struct BranchSnapshot {
    pub repo: String,
    pub branches: Vec<BranchInfo>,
    pub tracking: Vec<BranchTracking>,
    pub recent: Vec<String>,
    pub remotes: Vec<String>,
    /// HEAD 所在的本地分支;detached / 空仓库为 `None`。
    pub head: Option<String>,
    /// detached 时的显示名(`"(1a2b3c4)"`)。
    pub detached: Option<String>,
    pub loading: bool,
    pub busy: Option<GitBusy>,
    /// 面板自己没在跑,但仓库的写操作闸被别的区占着(更改区在提交、历史区在拣选…):
    /// 写类动作同样禁用。
    pub gated: bool,
}

impl BranchSnapshot {
    /// 写类动作现在能不能点。
    pub fn writable(&self) -> bool {
        self.busy.is_none() && !self.gated
    }
}

/// 中缝拖拽的一次会话。
#[derive(Clone, Copy)]
struct SectionDrag {
    start_y: Pixels,
    start_ratio: f32,
    /// 两块内容加起来的高度(靠 canvas 量出来)。
    total: f32,
}

pub struct GitPanel {
    store: Entity<AppStore>,
    changes: Entity<GitChanges>,
    history: Entity<GitHistoryContent>,
    repos: Vec<GitRepoInfo>,
    selected_repo: String,
    branches: Vec<BranchInfo>,
    branches_loading: bool,
    /// 正在查看(**未 checkout**)的分支;`None` = 跟随 HEAD。
    view_branch: Option<String>,
    meta: RepoMeta,
    /// 正在跑的写操作(见模块注释「写操作」)。
    busy: Option<GitBusy>,
    /// 抽屉里显示的是不是本面板。收着时不扫盘。
    visible: bool,
    /// 收着的时候项目切过 → 打开时补拉一次。
    stale: bool,
    /// 当前挂着的项目路径(判断项目有没有换)。
    project_path: Option<String>,
    /// 迟到响应丢弃(换仓库后旧的分支响应不许覆盖)。
    branch_request: u64,
    repo_request: u64,
    /// `.git` 目录监听(见模块注释「自动刷新」)。
    dir_watch: GitDirWatch,
    /// 换监听目标的请求令牌(后台列目录期间又换了仓库,旧结果丢弃)。
    watch_request: u64,
    /// 监听触发的签名检查正在后台跑。
    sig_checking: bool,
    /// 检查在跑期间又来了一批变化:等它回来再查一轮,别把这批漏掉。
    sig_recheck: bool,
    /// 两块内容区加起来的高度,中缝拖拽换算比例要用。
    sections_height: f32,
    drag: Option<SectionDrag>,
    _tick: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl GitPanel {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let changes = cx.new(|cx| GitChanges::new(store.clone(), window, cx));
        let history = cx.new(|cx| GitHistoryContent::new(store.clone(), window, cx));

        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(
            &changes,
            window,
            |this: &mut Self, _entity, event, window, cx| match event {
                // 提交成功:历史整体重来 + 重载分支(分支头已前移)。「提交并推送」接着开
                // 推送对话框(预选当前分支),在那里看清待推送的提交再确认
                GitChangesEvent::Committed { open_push } => {
                    this.history.update(cx, |h, cx| h.reload(cx));
                    this.load_branches(cx);
                    if *open_push {
                        this.open_push_dialog(None, window, cx);
                    }
                }
                // 更改区里贮藏了改动:工作区与贮藏数都变了,整面板刷新
                GitChangesEvent::Stashed => this.refresh_all(cx),
            },
        ));
        subs.push(cx.subscribe_in(
            &history,
            window,
            |this: &mut Self, _entity, event, window, cx| match event {
                GitHistoryEvent::RefreshRepos => this.refresh_repo_meta(cx),
                // 历史区的改写操作(检出 / 重置 / 拣选…)动了 HEAD、分支或工作区:
                // 分支徽章与「更改」列表都要重取(历史区自己已重载)
                GitHistoryEvent::RepoChanged => {
                    this.refresh_repo_meta(cx);
                    this.changes.update(cx, |c, cx| c.load(cx));
                }
                GitHistoryEvent::CommitUndone { message } => {
                    this.changes
                        .update(cx, |c, cx| c.restore_message(message, window, cx));
                }
            },
        ));
        // render 读活动项目(空态 / 远程)与更改视图模式,任何变化都照旧重画
        subs.push(cx.observe(&store, |_, _, cx| cx.notify()));
        // 项目路径的比对只在活动项目可能变了时做(`StoreEvent::touches_active_project`),
        // 不再被 OSC 标题 / AI 状态这些与仓库无关的变化叫醒
        subs.push(
            cx.subscribe(&store, |this: &mut Self, _, event: &StoreEvent, cx| {
                if !event.touches_active_project() {
                    return;
                }
                let path = this.store.read(cx).active_project().map(|p| p.path.clone());
                if path != this.project_path {
                    this.project_path = path;
                    if this.visible {
                        this.on_project_changed(cx);
                    } else {
                        // 收着的时候不扫盘 —— 原版收起时组件根本没挂载
                        this.stale = true;
                    }
                    cx.notify();
                }
            }),
        );

        Self {
            store,
            changes,
            history,
            repos: Vec::new(),
            selected_repo: String::new(),
            branches: Vec::new(),
            branches_loading: false,
            view_branch: None,
            meta: RepoMeta::default(),
            busy: None,
            visible: false,
            stale: true,
            project_path: None,
            branch_request: 0,
            repo_request: 0,
            dir_watch: GitDirWatch::new(),
            watch_request: 0,
            sig_checking: false,
            sig_recheck: false,
            sections_height: 0.0,
            drag: None,
            _tick: None,
            _subs: subs,
        }
    }

    /// 抽屉开合 / 换面板。收着时不扫盘、不嗅探 pty 输出、不盯 `.git`。
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        git_watch::set_enabled(visible);
        if visible {
            self.project_path = self.store.read(cx).active_project().map(|p| p.path.clone());
            if self.stale {
                self.on_project_changed(cx);
            }
            self.start_tick(cx);
        } else {
            self._tick = None;
        }
        // 可见 → 按当时选中的仓库挂监听;收起 → 撤掉
        self.retarget_watch(cx);
        cx.notify();
    }

    /// pty-output 嗅探的节拍。100ms 一拍:抽干旁路 → 命中就通知两个子面板
    /// (**各自** 500ms 去抖,原版是两个独立定时器)→ 到点的那个自己重取。
    /// 同一拍顺带问一声 `.git` 监听有没有攒够一批安静下来的变化。
    fn start_tick(&mut self, cx: &mut Context<Self>) {
        self._tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(git_watch::POLL_MS))
                    .await;
                let alive = this
                    .update(cx, |this: &mut Self, cx| {
                        if git_watch::drain_hit() {
                            this.changes.update(cx, |c, _| c.note_pty_hit());
                            this.history.update(cx, |h, _| h.note_pty_hit());
                        }
                        this.changes.update(cx, |c, cx| c.tick(cx));
                        this.history.update(cx, |h, cx| h.tick(cx));
                        if this.dir_watch.take_due() {
                            this.check_git_dir_change(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    return;
                }
            }
        }));
    }

    fn is_remote_project(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .active_project()
            .is_some_and(|p| p.ssh_connection_id.is_some())
    }

    /// 项目变了:先吃缓存(没有就清空)→ 重新发现仓库。
    fn on_project_changed(&mut self, cx: &mut Context<Self>) {
        self.stale = false;
        self.view_branch = None;
        self.branches.clear();
        self.meta = RepoMeta::default();
        let path = self.project_path.clone().unwrap_or_default();
        let cached = REPO_CACHE.with(|c| c.borrow().get(&path).cloned());
        match cached {
            Some((repos, selected)) => {
                self.repos = repos;
                self.selected_repo = selected;
            }
            None => {
                self.repos.clear();
                self.selected_repo.clear();
            }
        }
        self.push_repo_down(cx);
        self.load_repos(cx);
        self.retarget_watch(cx);
    }

    /// 发现仓库(**重**:首次扫盘深度 5)。远程项目直接 return。
    fn load_repos(&mut self, cx: &mut Context<Self>) {
        if self.is_remote_project(cx) {
            return;
        }
        let Some(path) = self.project_path.clone() else {
            return;
        };
        self.repo_request += 1;
        let req = self.repo_request;
        let task_path = std::path::PathBuf::from(&path);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::discover_git_repos(&task_path) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.repo_request != req {
                    return;
                }
                match result {
                    Ok(repos) => {
                        // 选中仓库保持原值(若仍在列表里),否则取第一个
                        let keep = repos
                            .iter()
                            .any(|r| r.path.to_string_lossy() == this.selected_repo);
                        if !keep {
                            this.selected_repo = repos
                                .first()
                                .map(|r| r.path.to_string_lossy().to_string())
                                .unwrap_or_default();
                        }
                        this.repos = repos;
                        REPO_CACHE.with(|c| {
                            c.borrow_mut().insert(
                                path.clone(),
                                (this.repos.clone(), this.selected_repo.clone()),
                            )
                        });
                    }
                    Err(err) => {
                        eprintln!("[git] 发现仓库失败: {err:#}");
                        this.repos.clear();
                    }
                }
                this.push_repo_down(cx);
                this.load_branches(cx);
                this.retarget_watch(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 重载分支列表与随它一起的元数据(领先落后、最近分支、进行中操作、签名)。
    fn load_branches(&mut self, cx: &mut Context<Self>) {
        let repo = self.selected_repo.clone();
        if repo.is_empty() {
            self.branches.clear();
            self.meta = RepoMeta::default();
            return;
        }
        self.branches_loading = true;
        self.branch_request += 1;
        let req = self.branch_request;
        let task_repo = std::path::PathBuf::from(&repo);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_branch_snapshot(&task_repo) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                // 迟到响应丢弃:换仓库后的旧响应不许覆盖
                if this.branch_request != req || this.selected_repo != repo {
                    return;
                }
                this.branches_loading = false;
                match result {
                    Ok((list, meta)) => {
                        // 正在查看的分支没了(被删 / 改名):回到跟随 HEAD,
                        // 否则历史区拿着一个不存在的分支名去查
                        if this
                            .view_branch
                            .as_ref()
                            .is_some_and(|v| !list.iter().any(|b| &b.name == v))
                        {
                            this.view_branch = None;
                        }
                        let selected = this.selected_repo.clone();
                        if let Some(info) = this
                            .repos
                            .iter_mut()
                            .find(|r| r.path.to_string_lossy() == selected)
                        {
                            info.current_branch = meta.head_label.clone();
                        }
                        this.branches = list;
                        this.meta = meta;
                    }
                    Err(err) => eprintln!("[git] 取分支失败: {err:#}"),
                }
                this.push_repo_down(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 仓库栏的手动刷新与 pty 嗅探回调都走这里(原版 `refreshRepoMeta`)。
    fn refresh_repo_meta(&mut self, cx: &mut Context<Self>) {
        self.load_repos(cx);
        self.load_branches(cx);
    }

    /// 整面板刷新:仓库元数据 + 分支 + 「更改」+ 提交历史。手动 ↻、写操作完成、
    /// `.git` 引用变化都走这里。
    pub(crate) fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.refresh_repo_meta(cx);
        // 「更改」列表也要跟着重取 —— push_repo_down 的 set_repo
        // 对同仓库路径会短路,不显式 load 它就纹丝不动
        self.changes.update(cx, |c, cx| c.load(cx));
        self.history.update(cx, |h, cx| h.reload(cx));
    }

    /// 把仓库 / 分支透给两个子面板。
    fn push_repo_down(&mut self, cx: &mut Context<Self>) {
        let repo = self.selected_repo.clone();
        let branches = self.branches.clone();
        let head_branch = self.current_branch().map(str::to_string);
        let view_branch = self.view_branch.clone();
        self.changes.update(cx, |c, cx| c.set_repo(&repo, cx));
        self.history.update(cx, |h, cx| {
            h.sync(
                &repo,
                &branches,
                head_branch.as_deref(),
                view_branch.as_deref(),
                cx,
            )
        });
    }

    fn select_repo(&mut self, path: String, cx: &mut Context<Self>) {
        if self.selected_repo == path {
            return;
        }
        self.selected_repo = path;
        // 换仓库:分支清空、viewBranch 复位、元数据清掉
        self.branches.clear();
        self.view_branch = None;
        self.meta = RepoMeta::default();
        if let Some(project) = self.project_path.clone() {
            REPO_CACHE.with(|c| {
                c.borrow_mut()
                    .insert(project, (self.repos.clone(), self.selected_repo.clone()))
            });
        }
        self.push_repo_down(cx);
        self.load_branches(cx);
        self.retarget_watch(cx);
        cx.notify();
    }

    fn selected_repo_info(&self) -> Option<&GitRepoInfo> {
        self.repos
            .iter()
            .find(|r| r.path.to_string_lossy() == self.selected_repo)
    }

    /// 当前 HEAD 分支名。**detached HEAD 时是 `"(1a2b3c4)"` 带括号的短 hash**
    /// (`git.rs:484-489`)—— 显示照旧,但绝不能当分支名传给 `get_git_log`。
    fn current_branch(&self) -> Option<&str> {
        self.selected_repo_info()
            .and_then(|r| r.current_branch.as_deref())
    }

    /// HEAD 所在的本地分支名;detached / 空仓库为 `None`。
    fn head_branch(&self) -> Option<&str> {
        self.current_branch().filter(|b| !is_detached_label(b))
    }

    // ── 给分支弹窗 / 推送对话框的接口 ──────────────────────

    /// 分支弹窗的数据快照。
    pub(crate) fn branch_snapshot(&self) -> BranchSnapshot {
        BranchSnapshot {
            repo: self.selected_repo.clone(),
            branches: self.branches.clone(),
            tracking: self.meta.tracking.clone(),
            recent: self.meta.recent.clone(),
            remotes: self.meta.remotes.clone(),
            head: self.head_branch().map(str::to_string),
            detached: self
                .current_branch()
                .filter(|b| is_detached_label(b))
                .map(str::to_string),
            loading: self.branches_loading,
            busy: self.busy,
            gated: self.gated(),
        }
    }

    /// 面板自己没在跑写操作,但仓库的写操作闸被别的区占着(见模块注释「写操作」)。
    fn gated(&self) -> bool {
        self.busy.is_none() && git_ui::gate::is_busy(&self.selected_repo)
    }

    pub(crate) fn repo_path(&self) -> &str {
        &self.selected_repo
    }

    pub(crate) fn store(&self) -> &Entity<AppStore> {
        &self.store
    }

    /// 切换「查看哪个分支的历史」(**不 checkout**)。`None` = 跟随 HEAD。
    pub(crate) fn set_view_branch(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        self.view_branch = name;
        self.push_repo_down(cx);
        cx.notify();
    }

    /// 推一条结果 toast(标题是当前项目名)。
    pub(crate) fn notify_result(&self, message: String, tone: Tone, cx: &mut App) {
        git_ui::toast(&self.store, message, tone, cx);
    }

    /// 跑一个 git 写操作(见模块注释「写操作」)。
    ///
    /// `op` 占着仓库的写操作闸在后台执行器上跑(阻塞 CLI,见 [`git_ui::spawn_gated`]);
    /// `done` 拿到成功结果后在主线程上推摘要 / 接着开下一个确认框。失败统一弹详情框,
    /// 标题是「{动作}失败」。无论成败都整面板刷新 —— 失败也可能已经改了一半(合并停在
    /// 冲突上之类)。闸被别的区占着时不执行,推一条 toast 说明。
    pub(crate) fn run_write_op<T: Send + 'static>(
        &mut self,
        busy: GitBusy,
        op: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_repo.is_empty() || self.busy.is_some() {
            return;
        }
        let this = cx.entity().downgrade();
        let started = git_ui::spawn_gated(
            &self.selected_repo,
            op,
            move |result, window, cx| {
                let _ = this.update(cx, |this: &mut Self, cx| {
                    this.busy = None;
                    match result {
                        Ok(value) => done(this, value, window, cx),
                        Err(err) => {
                            eprintln!("[git] {}失败: {err:#}", busy.label());
                            git_ui::show_git_error(
                                tr!("gitBranch", "opFailed", op = busy.label()),
                                git_ui::error_text(&err),
                                window,
                                cx,
                            );
                        }
                    }
                    this.refresh_all(cx);
                    cx.notify();
                });
            },
            window,
            cx,
        );
        if !started {
            self.notify_result(git_ui::gate_busy_text().to_string(), Tone::Attention, cx);
            return;
        }
        self.busy = Some(busy);
        cx.notify();
    }

    /// `git fetch --all`。
    pub(crate) fn fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_write_op(
            GitBusy::Fetch,
            mt_project::git::git_fetch_all,
            |this, outcome, _window, cx| {
                let message = if outcome.updated_refs == 0 {
                    t("gitBranch", "fetchUpToDate").to_string()
                } else {
                    tr!("gitBranch", "fetched", count = outcome.updated_refs)
                };
                this.notify_result(message, Tone::Info, cx);
            },
            window,
            cx,
        );
    }

    /// tooltip 里的拉取方式:手选过的照名字写;没手选时写「跟随配置:<配置里的方式>」。
    fn pull_mode_label(&self) -> String {
        match PULL_MODE.with(Cell::get) {
            Some(mode) => pull_mode_name(mode).to_string(),
            None => tr!(
                "gitBranch",
                "modeFollowConfig",
                mode = config_pull_name(&self.meta.pull_config)
            ),
        }
    }

    /// 拉取。手选过方式就带对应参数;没手选时**不带方式参数**,完全交给用户的 git 配置
    /// (`branch.<b>.rebase`、`rebase=merges` / `interactive` 都由 git 自己读)——
    /// 折算成三种方式再带参数会把 `merges` 降成 `--rebase`、拍平合并提交。
    fn pull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = PULL_MODE.with(Cell::get);
        self.run_write_op(
            GitBusy::Pull,
            move |repo| match mode {
                Some(mode) => mt_project::git::git_pull_with(repo, mode),
                None => mt_project::git::git_pull_by_config(repo),
            },
            |this, outcome, _window, cx| {
                let (message, tone) = match outcome {
                    PullOutcome::UpToDate => {
                        (t("gitBranch", "pullUpToDate").to_string(), Tone::Info)
                    }
                    PullOutcome::Updated { commits } => {
                        (tr!("gitBranch", "pulled", count = commits), Tone::Info)
                    }
                    PullOutcome::Conflicts { files } => (
                        tr!("gitBranch", "pullConflicts", count = files),
                        Tone::Attention,
                    ),
                };
                this.notify_result(message, tone, cx);
            },
            window,
            cx,
        );
    }

    /// 打开「推送」对话框,预选 `branch`(`None` = 当前分支)。仓库栏推送按钮、分支弹窗的
    /// 「推送」、「提交并推送」提交成功之后都走这里。
    pub(crate) fn open_push_dialog(
        &mut self,
        branch: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_repo.is_empty() || self.busy.is_some() {
            return;
        }
        crate::git_push::open(cx.entity(), self.selected_repo.clone(), branch, window, cx);
    }

    /// 打开贮藏列表(仓库栏贮藏按钮)。有写操作在跑时不开 —— 贮藏的改写同样要抢
    /// `index.lock`。
    pub(crate) fn open_stash_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_repo.is_empty() || self.busy.is_some() {
            return;
        }
        let on_changed = self.stash_on_changed(false, cx);
        crate::git_stash::open_list(
            self.store.clone(),
            self.selected_repo.clone(),
            on_changed,
            window,
            cx,
        );
    }

    /// 检出 `branch` 会被本地改动挡住(分支弹窗检出前的预检,见 `git_branches::checkout`):
    /// 开「贮藏后检出」确认框。
    pub(crate) fn open_smart_checkout(
        &mut self,
        branch: String,
        conflicts: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_repo.is_empty() {
            return;
        }
        let on_changed = self.stash_on_changed(true, cx);
        crate::git_stash::open_smart_checkout(
            self.store.clone(),
            self.selected_repo.clone(),
            branch,
            conflicts,
            on_changed,
            window,
            cx,
        );
    }

    /// 贮藏弹窗操作之后的回调:整面板刷新。`follow_head` = 顺带回到跟随 HEAD(检出类,
    /// 同普通检出)。弱引用:回调跟着弹窗走,面板没了就什么都不做。
    fn stash_on_changed(&self, follow_head: bool, cx: &mut Context<Self>) -> Rc<dyn Fn(&mut App)> {
        let panel = cx.entity().downgrade();
        Rc::new(move |cx: &mut App| {
            let _ = panel.update(cx, |p, cx| {
                if follow_head {
                    p.set_view_branch(None, cx);
                }
                p.refresh_all(cx);
            });
        })
    }

    /// 推送对话框里推送进行中 / 结束时同步一下按钮状态。
    pub(crate) fn set_push_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy.then_some(GitBusy::Push);
        cx.notify();
    }

    /// 横幅「继续」。
    fn continue_op(&mut self, kind: RepoOpKind, window: &mut Window, cx: &mut Context<Self>) {
        self.run_write_op(
            GitBusy::Continue,
            move |repo| mt_project::git::continue_operation(repo, kind),
            move |this, state, _window, cx| {
                let noun = op_noun(kind);
                let (message, tone) = match state.kind {
                    None => (tr!("gitBranch", "opFinished", op = noun), Tone::Info),
                    Some(_) if state.conflicted > 0 => (
                        tr!(
                            "gitBranch",
                            "opStillConflict",
                            op = noun,
                            count = state.conflicted
                        ),
                        Tone::Attention,
                    ),
                    Some(_) => (tr!("gitBranch", "opStillRunning", op = noun), Tone::Info),
                };
                this.notify_result(message, tone, cx);
            },
            window,
            cx,
        );
    }

    /// 横幅「中止」:先确认(已做的冲突解决会丢)。
    fn confirm_abort(&mut self, kind: RepoOpKind, window: &mut Window, cx: &mut Context<Self>) {
        let noun = op_noun(kind);
        let this = cx.entity();
        Confirm::new(
            tr!("gitBranch", "abortTitle", op = noun),
            tr!("gitBranch", "abortMessage", op = noun),
        )
        .ok_text(t("gitBranch", "abort"))
        .danger()
        .open(
            move |window, cx| {
                this.update(cx, |this, cx| {
                    this.run_write_op(
                        GitBusy::Abort,
                        move |repo| mt_project::git::abort_operation(repo, kind),
                        move |this, (), _window, cx| {
                            this.notify_result(
                                tr!("gitBranch", "opAborted", op = op_noun(kind)),
                                Tone::Info,
                                cx,
                            );
                        },
                        window,
                        cx,
                    );
                });
            },
            window,
            cx,
        );
    }

    // ── `.git` 目录监听 ───────────────────────────────────

    /// 让监听目标跟上「可见 + 本地项目 + 选中的仓库」。目录在后台列(要读盘)。
    fn retarget_watch(&mut self, cx: &mut Context<Self>) {
        let want = (self.visible && !self.selected_repo.is_empty() && !self.is_remote_project(cx))
            .then(|| self.selected_repo.clone());
        if want.as_deref() == self.dir_watch.repo() {
            return;
        }
        self.watch_request += 1;
        self.dir_watch.clear();
        let Some(repo) = want else {
            return;
        };
        let req = self.watch_request;
        let path = PathBuf::from(&repo);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let layout = mt_project::git::git_dir_layout(&path)?;
                    let dirs = mt_project::git::git_watch_dirs(&layout);
                    anyhow::Ok((layout, dirs))
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, _cx| {
                if this.watch_request != req {
                    return;
                }
                match result {
                    Ok((layout, dirs)) => {
                        this.dir_watch.set_target(repo, layout, dirs);
                    }
                    // 监听挂不上不影响使用:手动 ↻ 与 pty 嗅探照旧
                    Err(err) => eprintln!("[git] 监听 .git 失败: {err:#}"),
                }
            });
        })
        .detach();
    }

    /// `.git` 下攒够一批安静下来的变化:后台算签名,对比上一份快照决定刷什么
    /// (见模块注释「自动刷新」)。顺带重列监听目录(新出现的分支前缀目录要补挂)。
    fn check_git_dir_change(&mut self, cx: &mut Context<Self>) {
        let (Some(repo), Some(layout)) = (
            self.dir_watch.repo().map(str::to_string),
            self.dir_watch.layout(),
        ) else {
            return;
        };
        if self.sig_checking {
            self.sig_recheck = true;
            return;
        }
        self.sig_checking = true;
        let path = PathBuf::from(&repo);
        cx.spawn(async move |this, cx| {
            let (signature, dirs) = cx
                .background_executor()
                .spawn(async move {
                    (
                        mt_project::git::repo_signature(&path).ok(),
                        mt_project::git::git_watch_dirs(&layout),
                    )
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.sig_checking = false;
                if this.dir_watch.repo() != Some(repo.as_str()) || this.selected_repo != repo {
                    this.sig_recheck = false;
                    return;
                }
                if std::mem::take(&mut this.sig_recheck) {
                    // 期间又有变化:这份签名可能已经旧了,下一拍重查(变化时刻按现在算)
                    this.dir_watch.poke();
                }
                this.dir_watch.update_dirs(dirs);
                match (this.meta.signature, signature) {
                    // 没变:别的程序只是刷新了索引的 stat 缓存之类
                    (Some(old), Some(new)) if old == new => {}
                    // 只动了索引(终端里 `git add`、解决了一个冲突):「更改」区与冲突数
                    (Some(old), Some(new)) if old.refs == new.refs => {
                        this.meta.signature = Some(new);
                        this.changes.update(cx, |c, cx| c.load(cx));
                        this.load_branches(cx);
                    }
                    _ => {
                        this.meta.signature = signature;
                        this.refresh_all(cx);
                    }
                }
            });
        })
        .detach();
    }
}

/// detached HEAD 的显示名是带括号的短 hash(`"(1a2b3c4)"`)。
fn is_detached_label(label: &str) -> bool {
    label.starts_with('(') && label.ends_with(')')
}

/// 进行中操作的名词(「合并」「变基」…),拼进横幅与提示。
pub(crate) fn op_noun(kind: RepoOpKind) -> &'static str {
    t(
        "gitBranch",
        match kind {
            RepoOpKind::Merge => "kindMerge",
            RepoOpKind::Rebase => "kindRebase",
            RepoOpKind::CherryPick => "kindCherryPick",
            RepoOpKind::Revert => "kindRevert",
        },
    )
}

/// 拉取方式的名字(tooltip 与菜单共用)。
fn pull_mode_name(mode: PullMode) -> &'static str {
    t(
        "gitBranch",
        match mode {
            PullMode::Merge => "modeMerge",
            PullMode::Rebase => "modeRebase",
            PullMode::FfOnly => "modeFfOnly",
        },
    )
}

/// 用户 git 配置里那种拉取方式的名字:`rebase=merges` 单独写成「变基(保留合并)」。
fn config_pull_name(config: &PullConfig) -> &'static str {
    if config.mode == PullMode::Rebase && config.rebase_merges {
        t("gitBranch", "modeRebaseMerges")
    } else {
        pull_mode_name(config.mode)
    }
}

/// 分支徽章的 tooltip:分支名 + 上游 + ↑↓ 的含义 + 操作提示,一行一条。
///
/// `viewing` = 正在查看的另一个分支(未检出)—— 此时计数不显示,第一行说明这一点。
fn branch_tip_lines(
    branch: &str,
    tracking: Option<&BranchTracking>,
    detached: bool,
    viewing: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    if viewing {
        lines.push(tr!("gitBranch", "tipViewing", branch = branch));
    } else if detached {
        lines.push(tr!("gitBranch", "tipDetached", commit = branch));
    } else {
        lines.push(branch.to_string());
        match tracking {
            Some(tracking) => {
                lines.push(tr!(
                    "gitBranch",
                    "tipUpstream",
                    upstream = tracking.upstream.as_str()
                ));
                if tracking.ahead == 0 && tracking.behind == 0 {
                    lines.push(t("gitBranch", "tipInSync").to_string());
                }
                if tracking.ahead > 0 {
                    lines.push(tr!("gitBranch", "tipAhead", count = tracking.ahead));
                }
                if tracking.behind > 0 {
                    lines.push(tr!("gitBranch", "tipBehind", count = tracking.behind));
                }
            }
            None => lines.push(t("gitBranch", "tipNoUpstream").to_string()),
        }
    }
    lines.push(t("gitBranch", "tipOpenBranches").to_string());
    lines
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 整面板的空态(没选项目 / 远程项目):与列表空态同一套「大图标 + 说明」。
fn centered_hint(shapes: &'static [mt_ui::icons::Shape], text: &'static str) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(ui::bg_surface())
        .child(git_ui::empty_state(shapes, text, None))
        .into_any_element()
}

/// 任意 flex-grow(gpui 的 `flex_grow()` 只会设成 1)。
fn grow<E: Styled>(mut el: E, value: f32) -> E {
    el.style().flex_grow = Some(value);
    el
}

/// 任意 flex-shrink(gpui 只有 `flex_shrink()` = 1 与 `flex_shrink_0()`)。
fn shrink<E: Styled>(mut el: E, value: f32) -> E {
    el.style().flex_shrink = Some(value);
    el
}

impl Render for GitPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 空态按顺序短路(`GitHistory.tsx:331-345`)
        if self.store.read(cx).active_project().is_none() {
            return div().size_full().child(centered_hint(
                git_ui::ICON_REPO,
                t("gitHistory", "selectProject"),
            ));
        }
        if self.is_remote_project(cx) {
            // git 命令跑在本地,对远程路径无意义(远程 Git 二期)
            return div()
                .size_full()
                .border_t_1()
                .border_color(ui::border_subtle())
                .child(centered_hint(
                    git_ui::ICON_REMOTE,
                    t("gitHistory", "remoteNotSupported"),
                ));
        }

        let section = SECTION_UI.with(|s| *s.borrow());
        let both_open = section.changes_open && section.history_open;
        let changes_grow = if section.changes_open {
            if section.history_open { section.ratio } else { 1.0 }
        } else {
            0.0
        };
        let history_grow = if section.history_open {
            if section.changes_open {
                1.0 - section.ratio
            } else {
                1.0
            }
        } else {
            0.0
        };

        let this = cx.entity();
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .border_t_1()
            .border_color(ui::border_subtle());

        if !self.repos.is_empty() {
            root = root.child(self.render_repo_bar(cx));
            if let Some(banner) = self.render_op_banner(cx) {
                root = root.child(banner);
            }
        }

        root = root
            .child(section_header(
                "git-section-changes",
                t("panels", "changes"),
                section.changes_open,
                false,
                Some(self.render_view_mode_toggle(cx)),
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    SECTION_UI.with(|s| {
                        let mut s = s.borrow_mut();
                        s.changes_open = !s.changes_open;
                    });
                    let _ = this;
                    cx.notify();
                }),
            ))
            .child(
                grow(
                    div()
                        .flex_basis(px(0.0))
                        .min_h(px(0.0))
                        .overflow_hidden()
                        .child(self.changes.clone()),
                    changes_grow,
                ),
            );

        if both_open {
            root = root.child(self.render_section_handle(cx));
        }

        root = root
            .child(section_header(
                "git-section-history",
                t("panels", "history"),
                section.history_open,
                true,
                None,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    SECTION_UI.with(|s| {
                        let mut s = s.borrow_mut();
                        s.history_open = !s.history_open;
                    });
                    let _ = this;
                    cx.notify();
                }),
            ))
            .child(
                grow(
                    div()
                        .flex_basis(px(0.0))
                        .min_h(px(0.0))
                        .overflow_hidden()
                        .child(self.history.clone()),
                    history_grow,
                ),
            )
            // 量一次两块内容区加起来的高度:中缝拖拽要按它换算比例
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, cx| {
                        this.update(cx, |panel: &mut GitPanel, _cx| {
                            // 整块面板的高度减掉固定件(仓库栏 + 横幅 + 两个 header 30)
                            let fixed = if panel.repos.is_empty() {
                                60.0
                            } else if panel.meta.op.kind.is_some() {
                                60.0 + BAR_HEIGHT * 2.0
                            } else {
                                60.0 + BAR_HEIGHT
                            };
                            panel.sections_height =
                                (f32::from(bounds.size.height) - fixed).max(1.0);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );

        // 拖拽期间鼠标可能划出手柄,移动/松手挂在面板根上
        root.when(self.drag.is_some(), |el| {
            el.on_mouse_move(cx.listener(|this: &mut Self, event: &MouseMoveEvent, _window, cx| {
                let Some(drag) = this.drag else { return };
                if drag.total <= 0.0 {
                    return;
                }
                let dy = f32::from(event.position.y - drag.start_y);
                let next = clamp_ratio(drag.start_ratio + dy / drag.total);
                SECTION_UI.with(|s| s.borrow_mut().ratio = next);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _: &MouseUpEvent, _window, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
        })
    }
}

/// SectionHeader(`GitHistory.tsx:69-100`),走共用的 [`git_ui::SectionHeader`]。
/// `bordered` 只有下方「提交历史」用。`trailing` 是右侧的动作位(「更改」的视图切换
/// 按钮住这);共用图标按钮自己拦住冒泡,不会连带触发 header 的折叠。
fn section_header(
    id: &'static str,
    label: &'static str,
    open: bool,
    bordered: bool,
    trailing: Option<AnyElement>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut header = git_ui::SectionHeader::new(id, label).collapsible(open);
    if let Some(trailing) = trailing {
        header = header.trailing(trailing);
    }
    header
        .build()
        .when(bordered, |el| {
            el.border_t_1().border_color(ui::border_subtle())
        })
        .on_click(on_click)
        .into_any_element()
}

impl GitPanel {
    /// 中缝拖拽手柄(`GitHistory.tsx:516-523`):零高包裹 + 内部 6px 绝对定位条。
    fn render_section_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .relative()
            .h(px(0.0))
            .flex_none()
            .child(
                div()
                    .id("git-section-handle")
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(-3.0))
                    .h(px(6.0))
                    .cursor_row_resize()
                    .hover(|el| el.bg(ui::with_alpha(ui::accent(), 0.4)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, event: &MouseDownEvent, _window, cx| {
                            cx.stop_propagation();
                            let ratio = SECTION_UI.with(|s| s.borrow().ratio);
                            this.drag = Some(SectionDrag {
                                start_y: event.position.y,
                                start_ratio: ratio,
                                total: this.sections_height,
                            });
                        }),
                    ),
            )
            .into_any_element()
    }

    /// 仓库栏(`GitHistory.tsx:353-499`),重排成清晰的两段:左段仓库 + 分支,
    /// 右段图标按钮组。窄下去时左段先截断,按钮组永不让位。
    fn render_repo_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let info = self.selected_repo_info();
        let repo_name = info.map(|r| r.name.clone()).unwrap_or_default();
        let is_worktree = info.is_some_and(|r| r.is_worktree);
        // 选中的那个也只画叶子名,回头再看仍然认不出是哪个 `api` —— 名字前面
        // 补一段弱化的父级路径(与下拉里第二行同一个判据),完整路径挂 tooltip
        let detail =
            info.and_then(|r| repo_detail(&r.path.to_string_lossy(), self.project_path.as_deref()));
        let repo_path_tip: SharedString = self.selected_repo.clone().into();

        // ── 左段 ① 仓库 ──
        let repo_trigger = div()
            .id("git-repo-trigger")
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(26.0))
            .px(px(6.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .min_w(px(0.0))
            .hover(|el| el.bg(git_ui::button_hover_bg()))
            .child(git_ui::icon(
                git_ui::ICON_REPO,
                git_ui::ICON_SIZE,
                ui::color_folder(),
            ))
            // 父级路径先让位:栏子窄下去时先截它,仓库名与分支徽章保住
            .when_some(detail, |el, detail| {
                el.child(
                    div()
                        .min_w(px(0.0))
                        .max_w(px(90.0))
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .truncate()
                        .child(format!("{detail}/")),
                )
            })
            .child(
                div()
                    .min_w(px(0.0))
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_primary())
                    .truncate()
                    .child(repo_name),
            )
            .when(is_worktree, |el| {
                el.child(git_ui::icon(
                    git_ui::ICON_WORKTREE,
                    git_ui::ICON_SIZE_SM,
                    ui::text_muted(),
                ))
            })
            .child(git_ui::icon(
                git_ui::ICON_CHEVRON_DOWN,
                git_ui::ICON_SIZE_SM,
                ui::text_muted(),
            ))
            .tip(repo_path_tip)
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                let entries = this.repo_menu(cx);
                menu::show(event.position(), entries, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entries = this.repo_context_menu(cx);
                    menu::show(event.position, entries, window, cx);
                }),
            );

        let left = div()
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .items_center()
            .gap(px(4.0))
            .child(repo_trigger)
            .children(self.render_branch_badge(cx));

        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .h(px(BAR_HEIGHT))
            .flex_none()
            .pl(px(4.0))
            .pr(px(8.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(left)
            .child(self.render_bar_buttons(cx))
            .into_any_element()
    }

    /// 左段 ② 分支:分支图标 + 引用胶囊 + ↑n(待推送)/ ↓m(待拉取)计数胶囊。
    /// 点开分支弹窗。
    ///
    /// 胶囊配色与提交历史同一张语义表([`git_ui::RefKind`]):HEAD = accent;正在
    /// 查看另一个分支时按它是本地(success)还是远程(info)上色,前面的图标跟着换色
    /// 提示「这不是 HEAD」;detached HEAD 是语义表之外的状态,用警告色 + 提交图标。
    ///
    /// 宽度**随分支名自适应**:栏子挤不下时才与仓库名一起收缩(两边都 `min_w(0)` +
    /// 省略截断)。徽章的收缩权重是仓库名的 3 倍:flex 按「权重 × 基准宽」分摊,
    /// 分支名通常比仓库名长得多,等权会把仓库名先挤成「re…」。截断时全名在 tooltip 里。
    fn render_branch_badge(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let head = self.current_branch().map(str::to_string);
        let branch = self.view_branch.clone().or_else(|| head.clone())?;
        let viewing = self.view_branch.is_some() && self.view_branch != head;
        let detached = !viewing && head.as_deref().is_some_and(is_detached_label);
        let tracking = if viewing {
            None
        } else {
            self.meta.head_tracking.as_ref()
        };
        let viewing_remote = viewing
            && self
                .branches
                .iter()
                .any(|b| b.is_remote && b.name == branch);
        let (lead, lead_color, pill_color) = if detached {
            (git_ui::ICON_COMMIT, ui::text_muted(), ui::color_warning())
        } else if viewing {
            let kind = if viewing_remote {
                git_ui::RefKind::Remote
            } else {
                git_ui::RefKind::Local
            };
            let shapes = if viewing_remote {
                git_ui::ICON_REMOTE
            } else {
                git_ui::ICON_BRANCH
            };
            (shapes, kind.color(), kind.color())
        } else {
            (
                git_ui::ICON_BRANCH,
                ui::text_muted(),
                git_ui::RefKind::Head.color(),
            )
        };
        let tip: SharedString = branch_tip_lines(&branch, tracking, detached, viewing)
            .join("\n")
            .into();
        let (ahead, behind) = tracking.map_or((0, 0), |t| (t.ahead, t.behind));
        let busy = self.busy.is_some_and(GitBusy::touches_branch);

        Some(
            div()
                .id("git-branch-badge")
                .flex()
                .items_center()
                .gap(px(6.0))
                .h(px(26.0))
                .px(px(6.0))
                .rounded(px(4.0))
                .min_w(px(0.0))
                .map(|el| shrink(el, 3.0))
                .cursor_pointer()
                .hover(|el| el.bg(git_ui::button_hover_bg()))
                .child(git_ui::icon(lead, git_ui::ICON_SIZE, lead_color))
                .child(git_ui::name_pill(
                    "git-branch-badge-name",
                    branch,
                    pill_color,
                    None,
                ))
                .when(ahead > 0, |el| el.child(git_ui::ahead_pill(ahead)))
                .when(behind > 0, |el| el.child(git_ui::behind_pill(behind)))
                .when(busy, |el| {
                    el.child(ui::spinner(px(git_ui::ICON_SIZE_SM), ui::accent()))
                })
                .tip(tip)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    // 分支列表为空时懒加载一次(`GitHistory.tsx:422`)
                    if this.branches.is_empty() {
                        this.load_branches(cx);
                    }
                    crate::git_branches::open(cx.entity(), window, cx);
                }))
                .into_any_element(),
        )
    }

    /// 右段图标按钮组:Fetch / 拉取 / 推送 / 贮藏 / 刷新。
    fn render_bar_buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.busy;
        // 别的区占着写操作闸:写按钮一律禁用,tooltip 换成说明(见模块注释「写操作」)
        let gated = self.gated();
        let idle = busy.is_none() && !gated;
        let is = |op: GitBusy| busy == Some(op);
        let blocked = |op: GitBusy| (busy.is_some() && busy != Some(op)) || gated;
        let head = self.head_branch().map(str::to_string);
        let detached = self.current_branch().is_some_and(is_detached_label);
        let upstream = self.meta.head_tracking.as_ref().map(|t| t.upstream.clone());
        let mode = self.pull_mode_label();
        let busy_tip =
            |op: GitBusy| -> SharedString { tr!("gitBranch", "busyTip", op = op.label()).into() };
        let gate_tip = || -> SharedString { git_ui::gate_busy_text().into() };

        let fetch_tip: SharedString = if is(GitBusy::Fetch) {
            busy_tip(GitBusy::Fetch)
        } else if gated {
            gate_tip()
        } else {
            t("gitBranch", "fetchTip").into()
        };
        let fetch = git_ui::busy_icon_button(
            "git-fetch",
            git_ui::ICON_FETCH,
            fetch_tip,
            is(GitBusy::Fetch),
            blocked(GitBusy::Fetch),
        )
        .when(idle, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.fetch(window, cx)))
        });

        let pull_tip: SharedString = if is(GitBusy::Pull) {
            busy_tip(GitBusy::Pull)
        } else if gated {
            gate_tip()
        } else {
            match &upstream {
                Some(upstream) => tr!(
                    "gitBranch",
                    "pullTip",
                    target = upstream.as_str(),
                    mode = mode.as_str()
                ),
                None => tr!("gitBranch", "pullTipNoUpstream", mode = mode.as_str()),
            }
            .into()
        };
        let pull = git_ui::busy_icon_button(
            "git-pull",
            git_ui::ICON_PULL,
            pull_tip,
            is(GitBusy::Pull),
            blocked(GitBusy::Pull) || detached,
        )
        .when(idle && !detached, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.pull(window, cx)))
        })
        // 右键选拉取方式(会话级,不落盘)
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let entries = this.pull_mode_menu(cx);
                menu::show(event.position, entries, window, cx);
            }),
        );

        let push_tip: SharedString = if is(GitBusy::Push) {
            busy_tip(GitBusy::Push)
        } else if gated {
            gate_tip()
        } else if detached {
            t("gitBranch", "pushTipDetached").into()
        } else {
            // 写真实的推送目标(与推送对话框同一份计划):目标可以不是上游 —— 从
            // origin/main 拉出来的 feature 推的是 origin/feature
            let plan = match &self.meta.push_plan {
                Some(PushPlan::Ready(target)) => Some(target),
                _ => None,
            };
            match (plan, &upstream, &head) {
                (Some(target), _, _) if !target.sets_upstream() => {
                    tr!("gitBranch", "pushTip", target = target.label())
                }
                (Some(_), _, Some(head)) | (None, None, Some(head)) => {
                    tr!("gitBranch", "pushTipNoUpstream", branch = head.as_str())
                }
                // 计划没读到:退回按上游写
                (None, Some(upstream), _) => {
                    tr!("gitBranch", "pushTip", target = upstream.as_str())
                }
                _ => t("gitBranch", "pushTipDetached").to_string(),
            }
            .into()
        };
        let push = git_ui::busy_icon_button(
            "git-push",
            git_ui::ICON_PUSH,
            push_tip,
            is(GitBusy::Push),
            blocked(GitBusy::Push) || detached,
        )
        .when(idle && !detached, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_push_dialog(None, window, cx)
            }))
        });

        // 贮藏:开贮藏列表;有贮藏时旁边挂数量胶囊(数量随分支快照一起读),tooltip 带数
        let stash_count = self.meta.stash_count;
        let stash_tip: SharedString = if gated {
            gate_tip()
        } else if stash_count > 0 {
            tr!("gitStash", "countTip", count = stash_count).into()
        } else {
            t("gitStash", "title").into()
        };
        let stash_button = git_ui::icon_button("git-stash", git_ui::ICON_STASH, stash_tip, idle)
            .when(idle, |el| {
                el.on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_stash_list(window, cx)
                    }),
                )
            });
        let stash = div()
            .flex()
            .flex_none()
            .items_center()
            .child(stash_button)
            .when(stash_count > 0, |el| {
                el.child(git_ui::count_pill(stash_count, ui::text_muted()))
            });

        // 刷新:后台重载期间转小圈(列表保留旧内容,不清空)
        let refreshing = self.branches_loading;
        let refresh = git_ui::busy_icon_button(
            "git-refresh",
            git_ui::ICON_REFRESH,
            t("gitBranch", "refreshTip"),
            refreshing,
            false,
        )
        .when(!refreshing, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.refresh_all(cx)))
        });

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.0))
            .child(fetch)
            .child(pull)
            .child(push)
            .child(stash)
            .child(refresh)
            .into_any_element()
    }

    /// 拉取方式菜单。当前选中的打勾;选了只改方式,不立刻拉。第一项「跟随 git 配置」
    /// (默认)= 不带方式参数拉,括号里写配置里的方式;下面三项是本次会话手选。
    fn pull_mode_menu(&self, _cx: &mut Context<Self>) -> Vec<menu::MenuEntry> {
        let current = PULL_MODE.with(Cell::get);
        let mark = |on: bool| if on { "✓ " } else { "　" };
        let follow = tr!(
            "gitBranch",
            "modeFollowConfigItem",
            mode = config_pull_name(&self.meta.pull_config)
        );
        let mut entries = vec![
            menu::MenuEntry::Header(t("gitBranch", "pullModeHeader").into()),
            MenuItem::new(format!("{}{follow}", mark(current.is_none())))
                .on_click(|_window, _cx| PULL_MODE.with(|m| m.set(None)))
                .into(),
        ];
        for (mode, key) in [
            (PullMode::Merge, "modeMergeItem"),
            (PullMode::Rebase, "modeRebaseItem"),
            (PullMode::FfOnly, "modeFfOnlyItem"),
        ] {
            entries.push(
                MenuItem::new(format!(
                    "{}{}",
                    mark(current == Some(mode)),
                    t("gitBranch", key)
                ))
                .on_click(move |_window, _cx| PULL_MODE.with(|m| m.set(Some(mode))))
                .into(),
            );
        }
        entries
    }

    /// 进行中操作横幅:attention 语义 —— 左侧 3px 竖条 + 淡底 + 图标 + 加粗动词 +
    /// 冲突数胶囊,右对齐「继续」(主)「中止」(次)。仍有冲突时「继续」禁用。
    fn render_op_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let kind = self.meta.op.kind?;
        let conflicted = self.meta.op.conflicted;
        let color = ui::color_attention();
        let verb = t(
            "gitBranch",
            match kind {
                RepoOpKind::Merge => "bannerMerge",
                RepoOpKind::Rebase => "bannerRebase",
                RepoOpKind::CherryPick => "bannerCherryPick",
                RepoOpKind::Revert => "bannerRevert",
            },
        );
        let busy = self.busy;
        let idle = busy.is_none() && !self.gated();
        let continuing = busy == Some(GitBusy::Continue);
        let aborting = busy == Some(GitBusy::Abort);
        let can_continue = conflicted == 0 && idle;

        let status = if conflicted > 0 {
            git_ui::pill(tr!("gitBranch", "conflictCount", count = conflicted), color)
                .into_any_element()
        } else {
            div()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(t("gitBranch", "conflictsResolved"))
                .into_any_element()
        };

        let continue_label = if continuing {
            tr!("gitBranch", "busyTip", op = GitBusy::Continue.label())
        } else {
            t("gitBranch", "continue").to_string()
        };
        let continue_button = git_ui::action_button(
            "git-op-continue",
            continue_label,
            git_ui::ButtonKind::Primary,
            can_continue,
        )
        .when(conflicted > 0, |el| {
            el.tip(tr!("gitBranch", "continueBlocked", count = conflicted))
        })
        .when(can_continue, |el| {
            el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.continue_op(kind, window, cx)
            }))
        });
        let abort_label = if aborting {
            tr!("gitBranch", "busyTip", op = GitBusy::Abort.label())
        } else {
            t("gitBranch", "abort").to_string()
        };
        let abort_button =
            git_ui::action_button("git-op-abort", abort_label, git_ui::ButtonKind::Ghost, idle)
                .when(idle, |el| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.confirm_abort(kind, window, cx)
                    }))
                });

        Some(
            div()
                .relative()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.0))
                .h(px(BAR_HEIGHT))
                .pl(px(12.0))
                .pr(px(8.0))
                .bg(ui::with_alpha(color, 0.12))
                .border_b_1()
                .border_color(ui::border_subtle())
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(3.0))
                        .bg(color),
                )
                .child(git_ui::icon(
                    if conflicted > 0 {
                        git_ui::ICON_ALERT
                    } else {
                        git_ui::ICON_MERGE
                    },
                    git_ui::ICON_SIZE,
                    color,
                ))
                .child(
                    div()
                        .flex_none()
                        .text_size(ui::font_px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(ui::text_primary())
                        .child(verb),
                )
                .child(status)
                .child(div().flex_1())
                .child(continue_button)
                .child(abort_button)
                .into_any_element(),
        )
    }

    /// 「更改」标题栏右侧的视图切换(树/列表)。原先住在 GitChanges 的工具栏里,
    /// 工具栏撤掉后上移到这。
    fn render_view_mode_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let tree_mode = self.store.read(cx).git_changes_view_mode() == "tree";
        // 显示的是「点了会切到」的那种视图
        git_ui::icon_button(
            "git-changes-view-mode",
            if tree_mode {
                git_ui::ICON_LIST
            } else {
                git_ui::ICON_LIST_TREE
            },
            if tree_mode {
                t("gitChanges", "switchToList")
            } else {
                t("gitChanges", "switchToTree")
            },
            true,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
            // 按钮在 header 里:冒泡由共用图标按钮拦住,不会连带把整块折叠
            this.store.update(cx, |store, cx| {
                let next = if store.git_changes_view_mode() == "tree" {
                    "list"
                } else {
                    "tree"
                };
                store.set_git_changes_view_mode(next, cx);
            });
            // GitChanges 不 observe store,列表要重排得显式踢它一脚
            this.changes.update(cx, |_, cx| cx.notify());
            cx.notify();
        }))
        .into_any_element()
    }

    /// 仓库下拉。
    ///
    /// **走 [`menu`] 而不是自绘 absolute 面板**:点外关 / Esc / 贴边收拢 / 焦点
    /// 还原四件事那边已经做全了。代价是行内没法画「选中行 accent 底色」与右侧
    /// 分支胶囊的独立配色 —— 选中用 `✓ ` 前缀(菜单基件本来的勾选方案),
    /// 分支放右侧的弱化标签位。
    ///
    /// 重名仓库(monorepo 里的一堆 `api`)靠 [`repo_detail`] 补的第二行父级路径
    /// 区分,缩进与标题对齐 —— `✓ ` / 全角空格前缀本身就占一格。
    fn repo_menu(&self, cx: &mut Context<Self>) -> Vec<menu::MenuEntry> {
        let this = cx.entity();
        self.repos
            .iter()
            .map(|repo| {
                let path = repo.path.to_string_lossy().to_string();
                let selected = path == self.selected_repo;
                let label = if selected {
                    format!("✓ {}", repo.name)
                } else {
                    format!("　{}", repo.name)
                };
                let mut item = MenuItem::new(label);
                if let Some(detail) = repo_detail(&path, self.project_path.as_deref()) {
                    item = item.detail(format!("　{detail}"));
                }
                if let Some(branch) = &repo.current_branch {
                    item = item.shortcut(if repo.is_worktree {
                        format!("⎇ {branch}")
                    } else {
                        branch.clone()
                    });
                }
                let this = this.clone();
                item.on_click(move |_window, cx| {
                    let path = path.clone();
                    this.update(cx, |this, cx| this.select_repo(path, cx));
                })
                .into()
            })
            .collect()
    }

    /// 仓库栏右键菜单(`GitHistory.tsx:300-329`)。
    fn repo_context_menu(&self, cx: &mut Context<Self>) -> Vec<menu::MenuEntry> {
        let Some(repo) = self.selected_repo_info().cloned() else {
            return Vec::new();
        };
        let store = self.store.clone();
        let project_id = self.store.read(cx).active_project_id.clone();
        let project_path = self.project_path.clone().unwrap_or_default();
        let repo_path = repo.path.to_string_lossy().to_string();
        // 项目根仓库不带 cwd 覆盖(默认就是项目根)
        let same_as_root = is_project_root_repo(&repo_path, &project_path);
        let title = if repo.is_worktree {
            format!("⎇ {}", repo.current_branch.clone().unwrap_or(repo.name.clone()))
        } else {
            repo.name.clone()
        };
        let this = cx.entity();
        let repo_for_worktree = repo_path.clone();

        vec![
            menu::item(
                t("gitHistoryContent", "openInTerminal"),
                move |window, cx| {
                    let Some(project_id) = project_id.clone() else {
                        return;
                    };
                    let (cwd, title) = if same_as_root {
                        (None, None)
                    } else {
                        (Some(repo_path.clone()), Some(title.clone()))
                    };
                    let opened = store.update(cx, |store, cx| {
                        let pane =
                            store.new_terminal_with_cwd(&project_id, None, None, cwd, window, cx);
                        if let (Some(pane), Some(title)) = (pane.as_ref(), title) {
                            store.rename_pane(&project_id, pane, &title, cx);
                        }
                        pane.is_some()
                    });
                    if opened {
                        crate::workbench_area::activate_terminal_page(window, cx);
                    }
                },
            ),
            menu::separator(),
            menu::item(
                t("gitHistoryContent", "manageWorktrees"),
                move |window, cx| {
                    let this = this.clone();
                    crate::git_worktree::open(
                        // 单仓库模式:discover_repos = false
                        repo_for_worktree.clone(),
                        false,
                        None,
                        move |cx| {
                            this.update(cx, |this, cx| this.load_repos(cx));
                        },
                        window,
                        cx,
                    );
                },
            ),
        ]
    }
}

/// 这个仓库是不是项目根自己。
///
/// 两边的来源不同,只去尾部分隔符没法比:项目路径是用户填的(Windows 上是 `\`),
/// 而项目根仓库那条走 `Repository::workdir()`,libgit2 内部一律用 `/`
/// (`D:/Git/mini-term/`)。两边都转成 [`slash_form`](mt_core::path_key::slash_form)
/// (分隔符统一 `/`、去尾部分隔符,大小写原样)再比 —— 与 [`repo_detail`] 同一把尺。
///
/// (原版 `replace(/[\\/]+$/,'')` 只去尾巴,Windows 上项目根仓库永远判成「不是
/// 根」,「在终端中打开」会带上 cwd 覆盖并把页签改名成仓库名。)
fn is_project_root_repo(repo_path: &str, project_path: &str) -> bool {
    use mt_core::path_key::slash_form;
    slash_form(repo_path) == slash_form(project_path)
}

/// 仓库在下拉/仓库栏里的**出处**:它的父目录相对项目根的那一段。
///
/// 仓库名取的是目录叶子名([`mt_project::git::discover_git_repos`]),于是
/// `services/api`、`legacy/api` 这类 monorepo 布局会在下拉里排出好几个一模
/// 一样的「api」,光看名字分不出是哪一个。叶子名撞车的前提就是父目录不同,
/// 所以补上这一段父级路径**一定**能区分。
///
/// 三档:
/// - 仓库就是项目根 / 项目的直接子目录 → `None`。深度 1 的名字在同一个父目录
///   下天然唯一,给每行都挂一条空信息的副标题只是噪音。
/// - 仓库在项目里更深处 → `services`、`packages/tools` 这样的相对段。
/// - 仓库在项目**外**(项目落在某个仓库的子目录里,`discover_repo_limited`
///   会向上找到它)→ 仓库自己的绝对路径,免得只剩一个没有出处的叶子名。
fn repo_detail(repo_path: &str, project_path: Option<&str>) -> Option<String> {
    use mt_core::path_key::slash_form;
    let repo = slash_form(repo_path);
    if repo.is_empty() {
        return None;
    }
    let project = project_path.map(slash_form).unwrap_or_default();
    // 项目根自己就是仓库:没有可补的出处
    if repo == project {
        return None;
    }
    let rel = if project.is_empty() {
        None
    } else {
        repo.strip_prefix(&format!("{project}/"))
    };
    match rel {
        // 项目内:掐掉叶子名,剩下的父级段就是出处(直接子目录 → 没有父级段 → None)
        Some(rel) => rel.rsplit_once('/').map(|(parent, _)| parent.to_string()),
        // 项目外(或压根没有项目路径):给绝对路径,总比只有一个叶子名强
        None => Some(repo),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 比例钳在 0.15~0.85。
    #[test]
    fn 区块比例钳位() {
        assert_eq!(clamp_ratio(0.5), 0.5);
        assert_eq!(clamp_ratio(0.0), 0.15);
        assert_eq!(clamp_ratio(-3.0), 0.15);
        assert_eq!(clamp_ratio(1.0), 0.85);
        assert_eq!(clamp_ratio(0.15), 0.15);
        assert_eq!(clamp_ratio(0.85), 0.85);
    }

    /// 「在终端中打开」:项目根仓库**不带** cwd 覆盖,子仓库/worktree 才带。
    /// 判据是分隔符统一 + 去尾部分隔符后的字符串比较。
    #[test]
    fn 项目根仓库不带_cwd_覆盖() {
        let project = r"D:\Git\mini-term";
        // 完全相同
        assert!(is_project_root_repo(r"D:\Git\mini-term", project));
        // 只差一个尾部反斜杠 —— 仍然算同一个,不该带覆盖
        assert!(is_project_root_repo(r"D:\Git\mini-term\", project));
        // 多个尾部分隔符也要吃掉
        assert!(is_project_root_repo(r"D:\Git\mini-term\\", project));
        assert!(is_project_root_repo("/home/u/proj//", "/home/u/proj"));
        // libgit2 workdir 的形态(正斜杠 + 尾杠)—— 旧口径只去尾巴,这条判成
        // 「不是根」,Windows 上项目根仓库的「在终端中打开」会带覆盖并改页签名
        assert!(is_project_root_repo("D:/Git/mini-term/", project));
        // 子仓库:必须带覆盖
        assert!(!is_project_root_repo(r"D:\Git\mini-term\sub", project));
        assert!(!is_project_root_repo("D:/Git/mini-term/sub/", project));
    }

    /// 仓库出处只在「有信息量」时才给:项目根自己、项目的直接子目录都没有。
    #[test]
    fn 浅层仓库不补出处() {
        let project = r"D:\Git\washStatsion";
        // 项目根自己就是仓库
        assert_eq!(repo_detail(project, Some(project)), None);
        // libgit2 的 workdir 是正斜杠 + 尾部分隔符,归一化后仍要认出是同一个
        assert_eq!(repo_detail("D:/Git/washStatsion/", Some(project)), None);
        // 直接子目录:同一个父目录下叶子名天然唯一,补出来是空信息
        assert_eq!(repo_detail(r"D:\Git\washStatsion\web", Some(project)), None);
        assert_eq!(repo_detail("/home/u/proj/web", Some("/home/u/proj")), None);
    }

    /// monorepo 里的同名仓库靠父级路径区分 —— 这正是「一堆 api」那个场景。
    #[test]
    fn 同名仓库靠父级路径区分() {
        let project = r"D:\Git\mono";
        assert_eq!(
            repo_detail(r"D:\Git\mono\services\api", Some(project)),
            Some("services".to_string())
        );
        assert_eq!(
            repo_detail(r"D:\Git\mono\legacy\api", Some(project)),
            Some("legacy".to_string())
        );
        // 更深的层级把整条父级路径都给出来,两条出处必然不同
        assert_ne!(
            repo_detail(r"D:\Git\mono\a\x\api", Some(project)),
            repo_detail(r"D:\Git\mono\b\x\api", Some(project))
        );
        assert_eq!(
            repo_detail(r"D:\Git\mono\packages\tools\api", Some(project)),
            Some("packages/tools".to_string())
        );
    }

    /// 项目落在某个仓库的子目录里时(`discover_repo_limited` 向上找到的那种),
    /// 仓库不在项目内 —— 给绝对路径,别只剩一个没有出处的叶子名。
    #[test]
    fn 项目外的仓库给绝对路径() {
        assert_eq!(
            repo_detail(r"D:\Git\mini-term", Some(r"D:\Git\mini-term\crates")),
            Some("D:/Git/mini-term".to_string())
        );
        // 前缀只是「字符串像」不算在项目内:mono2 不是 mono 的子目录
        assert_eq!(
            repo_detail(r"D:\Git\mono2\api", Some(r"D:\Git\mono")),
            Some("D:/Git/mono2/api".to_string())
        );
        // 还没挂上项目路径时同理(空路径不能当前缀,否则谁都算项目内)
        assert_eq!(
            repo_detail(r"D:\Git\mono\api", None),
            Some("D:/Git/mono/api".to_string())
        );
        // 空仓库路径没什么可说的
        assert_eq!(repo_detail("", Some(r"D:\Git\mono")), None);
    }

    /// detached HEAD 的 `current_branch` 是 `"(1a2b3c4)"` —— 它**不在**
    /// `get_repo_branches` 的结果里,所以 `viewBranch` 永远取不到它,
    /// 也就永远不会被当分支名传给 `get_git_log`(那会 `bail!`)。
    #[test]
    fn detached_head_不当分支名查询() {
        let branches = [
            BranchInfo {
                name: "main".into(),
                is_head: false,
                is_remote: false,
                commit_hash: "abc".into(),
                upstream: Some("origin/main".into()),
            },
            BranchInfo {
                name: "origin/main".into(),
                is_head: false,
                is_remote: true,
                commit_hash: "abc".into(),
                upstream: None,
            },
        ];
        let detached = "(1a2b3c4)";
        assert!(
            !branches.iter().any(|b| b.name == detached),
            "括号短 hash 绝不会出现在分支列表里"
        );
        // viewBranch 只从分支列表里取 → 括号短 hash 进不去
        let picked: Option<&str> = branches
            .iter()
            .find(|b| b.name == detached)
            .map(|b| b.name.as_str());
        assert!(picked.is_none());
        assert!(is_detached_label(detached));
        assert!(!is_detached_label("main"));
        assert!(!is_detached_label("(wip"), "只有成对括号才算");
    }

    /// 分支徽章 tooltip:有上游时写出上游与 ↑↓ 的含义(为 0 的那项不写),同步时
    /// 说「与上游同步」;没上游 / detached / 查看别的分支各有说法;末行都是操作提示。
    #[test]
    fn 分支徽章提示逐行() {
        let tracking = BranchTracking {
            branch: "main".into(),
            upstream: "origin/main".into(),
            ahead: 2,
            behind: 0,
        };
        let lines = branch_tip_lines("main", Some(&tracking), false, false);
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[0], "main");
        assert!(lines[1].contains("origin/main"));
        assert!(lines[2].contains('2'));
        assert_eq!(lines[3], t("gitBranch", "tipOpenBranches"));

        let synced = BranchTracking {
            ahead: 0,
            ..tracking.clone()
        };
        let lines = branch_tip_lines("main", Some(&synced), false, false);
        assert_eq!(lines[2], t("gitBranch", "tipInSync"));

        let lines = branch_tip_lines("dev", None, false, false);
        assert_eq!(lines[1], t("gitBranch", "tipNoUpstream"));

        let lines = branch_tip_lines("(1a2b3c4)", None, true, false);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("1a2b3c4"));

        let lines = branch_tip_lines("origin/x", Some(&tracking), false, true);
        assert_eq!(lines.len(), 2, "查看别的分支时不列计数");
        assert!(lines[0].contains("origin/x"));
    }

    /// 改分支 / HEAD 的操作才让分支徽章转圈;同步类操作在自己的按钮上转。
    #[test]
    fn 忙碌态归属() {
        assert!(GitBusy::Checkout.touches_branch());
        assert!(GitBusy::Merge.touches_branch());
        assert!(!GitBusy::Fetch.touches_branch());
        assert!(!GitBusy::Push.touches_branch());
        assert!(!GitBusy::Continue.touches_branch());
    }
}
