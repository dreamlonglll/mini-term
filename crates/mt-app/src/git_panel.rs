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
//! 大 monorepo 上是秒级);同时开关 [`git_watch`] 的输出旁路总闸与 `.git` 目录监听
//! (工作区监听收着时转去替文件树服务,见「自动刷新 / 抽屉与窗口」)。
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
//! 三路:原有的 pty 输出旁路([`git_watch`]),外加 `.git` 目录监听与工作区监听
//! (都在 [`crate::git_dir_watch`]):AI agent 在终端里提交、外部工具切分支、AI / 外部
//! 编辑器改了文件,面板都自己跟上。
//!
//! - `.git` 监听去抖之后先在后台读一份 [`RepoObservation`](签名 + 冲突数),**没变就不刷**
//!   (防别的程序只刷新索引 stat 缓存时白刷一遍);只变了索引(终端里 `git add`、解决了一个
//!   冲突)就只重取「更改」区,冲突数直接用读到的那份 —— 分支快照里其余各项(分支、领先
//!   落后、最近分支、贮藏数、推送计划…)只随引用 / 仓库配置变,配置不在监听里(改了它连
//!   事件都没有),所以不必重取;引用变了才连分支、计数、提交历史一起刷(**不**重新发现仓库,
//!   见下)。两种都叫文件树按签名核一遍 git 标记([`git_watch::request_check`]);
//! - pty 嗅探命中时,`.git` 监听挂在选中仓库上就**只**叫它比一次签名([`PtyRoute`]):终端
//!   里的 git 命令改仓库必然经过 `.git`(提交 / 检出 / 合并 / 拉取 / 贮藏都动引用或索引),
//!   与那一路各刷一遍是白刷;`git log --stat` / `git show` 这类误命中签名不变,不刷。唯一
//!   两路都看不到的是子模块里的提交(父仓库的「更改」会多出一行,`.git` 与工作区监听都
//!   跳过了子模块),所以签名没变时补一次**只**重取「更改」列表。监听没挂上(列目录失败、
//!   正在换目标)才回落原版的两个 500ms 去抖;
//! - 工作区监听去抖之后按忽略规则过一遍,**只**重取「更改」列表,并把这批路径交给文件树
//!   ([`git_watch::request_paths`],它只认自己没在监听的 —— 折起来的目录的汇总字母);
//!   这批变化之后已经有一趟取数开跑的就不再要。git 正写着索引(`index.lock` 在)时「更改」的
//!   重取先压着、隔一阵复查,锁老过 [`crate::git_dir_watch::INDEX_LOCK_PATIENCE`](开着编辑器
//!   的 `git commit`、崩溃残锁)就不等了照常取(判据见 [`IndexLockGate`]);交给文件树的那份
//!   不等锁。目录太多时的递归监听与 Linux 上的低频兜底见 `git_dir_watch` 模块注释「递归子树」。
//!
//! ## 签名的先后
//!
//! 面板手上的签名([`RepoMeta::signature`])有三个来源:分支快照、`.git` 检查、面板自己写完
//! 索引时读的那份([`GitChangesEvent::IndexWritten`])。每份都带「开始读的时刻」,**谁新听
//! 谁的**:比手上那份旧的检查结果直接丢;分支快照在途时到的检查先压着,等快照落地拿它的
//! 签名再比(否则「刚发出的那一轮还没回来」会被误判成变了、再整轮刷一遍);快照落地时手上
//! 那份若是它发出之后才读的,签名与冲突数沿用手上的(判据见 [`check_fate`] 等几个纯函数)。
//!
//! ## 什么时候重新发现仓库
//!
//! `discover_git_repos` 要扫盘(父目录型项目上是秒级),只在换项目、手动 ↻、Worktree 管理
//! 弹窗改过之后跑;监听 / 写操作触发的刷新不跑 —— 仓库栏上的分支名由分支快照回填。
//!
//! ## 抽屉与窗口
//!
//! 抽屉收着时 pty 旁路、`.git` 监听与节拍都停;同项目再打开时走便宜路:重取一次「更改」+
//! 比一次签名决定要不要连分支 / 历史一起刷,不重新发现仓库。
//!
//! 工作区监听收着时**不停**,改为替文件树服务:文件树只盯根与展开着的目录,折起来的目录里
//! 的纯内容改动(汇总字母)全靠这一路交路径。收着时不跑节拍,由监听的唤醒口叫醒
//! ([`GitPanel::sync_feeder`]),一批只交路径、不重取「更改」。盯哪个仓库:面板对当前项目
//! 选过的那个,没选过就在后台找项目根所属的仓库(只往上找,不扫盘 —— 「收着不跑
//! `discover_git_repos`」那道闸照旧);父目录型项目在抽屉打开过之前找不到,不替它挂。
//! 抽屉开合时同一个仓库的监听原样留着,不撤了重挂。
//!
//! 窗口最小化(不可见)时节拍暂停、替文件树取批次的任务也停,监听照记,恢复可见时补比一次
//! 签名、把攒下的那批交掉;只是失焦照旧跑。
//!
//! SSH projects use these same mounted sections and dialogs. A three-second visible-only
//! tick quietly refreshes worktree status and compares repository signatures; unchanged
//! observations never reset history or request a repaint. Only local contexts install
//! filesystem watchers. Repository caches are keyed by the captured backend identity.
//!
//! 仓库栏的刷新按钮只在手动 ↻(或还一条分支都没有)时转圈,自动刷新不转 —— 结果没变时
//! 整轮都不重画。
//!
//! # 两块的高度
//!
//! 「更改」与「提交历史」按拖拽比例分,但内容比分到的矮的那一块收到内容高度、把富余
//! 让给另一块(见 [`section_heights`]):冲突演练里「已暂存」不必再滚动去找、下面的历史
//! 却空着半截。两块都放不下时照旧按比例。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Task, Window, canvas, div, prelude::FluentBuilder as _, px,
};
use mt_project::git::{
    BranchInfo, BranchTracking, GitRepoInfo, PullConfig, PullMode, PullOutcome, PushPlan,
    RepoOpKind, RepoOpState, RepoSignature, WorktreeWatchDirs,
};
use mt_remote::GitContext;
use mt_ui::tooltip::TooltipExt as _;

use crate::git_changes::{GitChanges, GitChangesEvent};
use crate::git_dir_watch::{
    GitDirWatch, IndexLockGate, LockProbe, RepoObservation, WorktreeBatch, WorktreeWatch,
};
use crate::git_history::{CommitDetailPane, GitHistoryContent, GitHistoryEvent};
use crate::git_ui::{self, Tone, estimate_text_width};
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

    /// `src/utils/projectDataCache.ts` 的对应物:后端身份键 → (仓库列表, 选中仓库)。
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
/// 工作区监听深处没挂上(Linux 上目录太多)时,抽屉开着每隔这么久兜底重取一次「更改」
/// (见 `git_dir_watch` 模块注释「递归子树」)。大仓库上一次 status 是几百毫秒,10s 一次
/// 约占半个百分点的单核。
const PARTIAL_POLL: Duration = Duration::from_secs(10);
/// Remote workspaces have no local filesystem watcher; poll quietly while presented.
const REMOTE_POLL: Duration = Duration::from_secs(3);

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

/// 两份元数据画出来是不是一样:除签名(不上屏)外逐项比。快照落地时据此决定要不要重画。
/// ⚠️ [`RepoMeta`] 加了上屏的字段,这里要跟着加。
fn meta_view_eq(a: &RepoMeta, b: &RepoMeta) -> bool {
    a.head_label == b.head_label
        && a.head_tracking == b.head_tracking
        && a.tracking == b.tracking
        && a.recent == b.recent
        && a.remotes == b.remotes
        && a.op == b.op
        && a.pull_config == b.pull_config
        && a.push_plan == b.push_plan
        && a.stash_count == b.stash_count
}

/// 后台读一份分支快照。只读;除分支列表外,单项查询失败不拖垮整份。
///
/// 一次开库取齐([`mt_project::git::repo_panel_snapshot`]),签名在其余各项之前算 ——
/// 读快照期间仓库又变了的话,签名停在旧状态,`.git` 监听下一次比对就认得出「变了」。
fn load_branch_snapshot(repo: &GitContext) -> anyhow::Result<(Vec<BranchInfo>, RepoMeta)> {
    let snap = repo.repo_panel_snapshot(RECENT_LIMIT)?;
    let meta = RepoMeta {
        head_label: snap.head_label,
        head_tracking: snap.head_tracking,
        tracking: snap.tracking,
        recent: snap.recent,
        remotes: snap.remotes,
        op: snap.op,
        signature: snap.signature,
        pull_config: snap.pull_config,
        push_plan: snap.push_plan,
        stash_count: snap.stash_count,
    };
    Ok((snap.branches, meta))
}

/// 分支弹窗要的一份快照(弹窗每帧从面板现取,数据始终与仓库栏一致)。
///
/// 大头的分支数据放在共享的 [`BranchData`] 里:面板按 [`GitPanel::branch_gen`] 缓存一份,
/// 数据没换时每帧取快照只是加引用计数,不再把上千条分支整份深拷贝。
#[derive(Clone)]
pub(crate) struct BranchSnapshot {
    pub repo: String,
    pub context: GitContext,
    pub data: Rc<BranchData>,
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

/// 分支快照里随分支列表一起换的那几样(见 [`BranchSnapshot`])。
pub(crate) struct BranchData {
    /// 面板分支数据的代号([`GitPanel::branch_gen`]):同一代号必是同一份数据,分支弹窗
    /// 拿它当排好的行的缓存键。
    pub generation: u64,
    pub branches: Vec<BranchInfo>,
    pub tracking: Vec<BranchTracking>,
    pub recent: Vec<String>,
    pub remotes: Vec<String>,
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
    /// 「提交详情」栏:画在抽屉左侧,由 `main.rs` 的抽屉层经 [`Self::detail_pane`] 取去挂。
    detail_pane: Entity<CommitDetailPane>,
    repos: Vec<GitRepoInfo>,
    selected_repo: String,
    branches: Vec<BranchInfo>,
    /// 仓库栏刷新按钮转不转圈 / 分支弹窗的「加载中」占位:手动 ↻ 或还一条分支都没有时的
    /// 那趟快照才算(自动刷新不转圈,见模块注释)。在途与否看 `snapshot_in_flight`。
    branches_loading: bool,
    /// 有一趟分支快照在途(不管转不转圈)。
    snapshot_in_flight: bool,
    /// 正在查看(**未 checkout**)的分支;`None` = 跟随 HEAD。
    view_branch: Option<String>,
    meta: RepoMeta,
    /// 分支数据的单调代号:`branches` / `meta` 每换一份(落地、清空)+1。分支快照按它
    /// 缓存共享的那份 [`BranchData`],分支弹窗按它缓存排好的行。
    branch_gen: u64,
    /// [`Self::branch_snapshot`] 共享出去的分支数据,代号对不上就重建。
    branch_data: RefCell<Option<Rc<BranchData>>>,
    /// 正在跑的写操作(见模块注释「写操作」)。
    busy: Option<GitBusy>,
    feedback_context: Option<GitContext>,
    enable_prompted: std::collections::HashSet<String>,
    /// 抽屉里显示的是不是本面板。收着时不扫盘。
    visible: bool,
    /// 收着的时候项目切过 → 打开时补拉一次。
    stale: bool,
    /// Captured backend identity; identical paths on different hosts never share state.
    project_context: Option<GitContext>,
    project_path: Option<String>,
    project_id: Option<String>,
    load_error: Option<String>,
    /// 迟到响应丢弃(换仓库后旧的分支响应不许覆盖)。
    branch_request: u64,
    repo_request: u64,
    /// `.git` 目录监听(见模块注释「自动刷新」)。
    dir_watch: GitDirWatch,
    /// 换监听目标的请求令牌(后台列目录期间又换了仓库,旧结果丢弃)。
    watch_request: u64,
    /// 监听触发的签名检查正在后台跑。
    sig_checking: bool,
    sig_request: u64,
    /// 检查在跑期间又来了一批变化:等它回来再查一轮,别把这批漏掉。
    sig_recheck: bool,
    /// `meta.signature` / `meta.op` 是哪一刻读的(见模块注释「签名的先后」)。
    sig_observed_at: Option<Instant>,
    /// 最近一轮「分支 + 更改 + 历史」整轮重取发出的时刻:比它早开始的检查结果不必再看。
    round_issued: Option<Instant>,
    /// 分支快照在途时到的检查结果:等快照落地拿新签名再比。
    deferred_check: Option<RepoObservation>,
    /// 走签名那条路的 pty 命中时刻:比签名的检查回来后,还没有哪趟「更改」取数在它之后
    /// 开跑的话补一次(子模块,见模块注释「自动刷新」)。
    pty_hit_at: Option<Instant>,
    /// 同项目重新打开抽屉:监听挂上之后当场比一次签名(见模块注释「抽屉与窗口」)。
    catch_up: bool,
    /// 窗口在不在呈现(最小化 / 整个被挡住时为 `false`,节拍暂停)。
    window_visible: bool,
    /// 工作区监听(AI / 外部编辑器改了文件 → 只重取「更改」列表,见 `git_dir_watch`)。
    work_watch: WorktreeWatch,
    /// 一批工作区变化正在后台过忽略规则(或在复查 `index.lock` / 重列监听清单)。
    work_checking: bool,
    /// 「更改」的重取撞上 `index.lock` 时压着等它写完,最多等一阵(见 [`IndexLockGate`])。
    index_lock: IndexLockGate,
    /// 工作区监听深处没挂上时,上一次兜底重取的时刻(见 [`PARTIAL_POLL`])。
    partial_polled: Option<Instant>,
    /// 抽屉收着时替文件树挂工作区监听的那个仓库(见模块注释「抽屉与窗口」)。
    hidden_repo: Option<String>,
    /// 求 `hidden_repo` 的请求令牌(后台找仓库期间又换了项目 / 打开了抽屉,旧结果丢弃)。
    hidden_request: u64,
    /// 抽屉收着、窗口可见时等工作区监听唤醒口的任务(见 [`Self::sync_feeder`])。
    _feeder: Option<Task<()>>,
    /// 两块内容区加起来的高度,中缝拖拽换算比例要用。
    sections_height: f32,
    /// 上一帧按内容让位时用到的两块「想要的高度」(见 [`SectionHeights`])。量出来的
    /// 新值与它不同就再画一帧。
    natural_seen: (Option<f32>, Option<f32>),
    /// 上一帧「更改」区实际占的比例(按内容让位后未必等于 `ratio`)。拖拽从这里起算,
    /// 中缝才不会一按就跳。
    effective_ratio: f32,
    /// 仓库栏左段(仓库 + 分支 + 计数)的宽度,canvas 量的。计数胶囊收不收看它。
    bar_left_width: f32,
    /// 上一帧画的计数形态(量出来的宽度换了档就再画一帧)。
    sync_badge: SyncBadge,
    /// 上一帧左段的让位档(同上)。
    bar_squeeze: BarSqueeze,
    drag: Option<SectionDrag>,
    _tick: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

fn remote_operation_label(operation: mt_remote::RemoteGitOperationKind) -> &'static str {
    match operation {
        mt_remote::RemoteGitOperationKind::Stage => t("gitHistory", "operationStage"),
        mt_remote::RemoteGitOperationKind::Unstage => t("gitHistory", "operationUnstage"),
        mt_remote::RemoteGitOperationKind::StageAll => t("gitHistory", "operationStageAll"),
        mt_remote::RemoteGitOperationKind::UnstageAll => t("gitHistory", "operationUnstageAll"),
        mt_remote::RemoteGitOperationKind::Commit => t("gitHistory", "operationCommit"),
        mt_remote::RemoteGitOperationKind::Amend => t("gitHistory", "operationAmend"),
        mt_remote::RemoteGitOperationKind::Discard => t("gitHistory", "operationDiscard"),
        mt_remote::RemoteGitOperationKind::StageHunks => t("gitHistory", "operationStageHunks"),
        mt_remote::RemoteGitOperationKind::UnstageHunks => t("gitHistory", "operationUnstageHunks"),
        mt_remote::RemoteGitOperationKind::DiscardHunks => t("gitHistory", "operationDiscardHunks"),
        mt_remote::RemoteGitOperationKind::Checkout => t("gitHistory", "operationCheckout"),
        mt_remote::RemoteGitOperationKind::CreateBranch => t("gitHistory", "operationCreateBranch"),
        mt_remote::RemoteGitOperationKind::RenameBranch => t("gitHistory", "operationRenameBranch"),
        mt_remote::RemoteGitOperationKind::DeleteBranch => t("gitHistory", "operationDeleteBranch"),
        mt_remote::RemoteGitOperationKind::Fetch => t("gitHistory", "operationFetch"),
        mt_remote::RemoteGitOperationKind::Pull => t("gitHistory", "operationPull"),
        mt_remote::RemoteGitOperationKind::Push => t("gitHistory", "operationPush"),
        mt_remote::RemoteGitOperationKind::SaveStash => t("gitHistory", "operationSaveStash"),
        mt_remote::RemoteGitOperationKind::ApplyStash => t("gitHistory", "operationApplyStash"),
        mt_remote::RemoteGitOperationKind::PopStash => t("gitHistory", "operationPopStash"),
        mt_remote::RemoteGitOperationKind::DropStash => t("gitHistory", "operationDropStash"),
        mt_remote::RemoteGitOperationKind::ClearStashes => t("gitHistory", "operationClearStashes"),
        mt_remote::RemoteGitOperationKind::StashBranch => t("gitHistory", "operationStashBranch"),
        mt_remote::RemoteGitOperationKind::Continue => t("gitHistory", "operationContinue"),
        mt_remote::RemoteGitOperationKind::Abort => t("gitHistory", "operationAbort"),
        mt_remote::RemoteGitOperationKind::ResolveConflict => {
            t("gitHistory", "operationResolveConflict")
        }
        mt_remote::RemoteGitOperationKind::MarkResolved => t("gitHistory", "operationMarkResolved"),
        mt_remote::RemoteGitOperationKind::CherryPick => t("gitHistory", "operationCherryPick"),
        mt_remote::RemoteGitOperationKind::Revert => t("gitHistory", "operationRevert"),
        mt_remote::RemoteGitOperationKind::Reset => t("gitHistory", "operationReset"),
        mt_remote::RemoteGitOperationKind::UndoCommit => t("gitHistory", "operationUndoCommit"),
        mt_remote::RemoteGitOperationKind::RewordCommit => t("gitHistory", "operationRewordCommit"),
        mt_remote::RemoteGitOperationKind::CreateTag => t("gitHistory", "operationCreateTag"),
        mt_remote::RemoteGitOperationKind::DeleteTag => t("gitHistory", "operationDeleteTag"),
        mt_remote::RemoteGitOperationKind::PushTag => t("gitHistory", "operationPushTag"),
        mt_remote::RemoteGitOperationKind::DeleteRemoteBranch => {
            t("gitHistory", "operationDeleteRemoteBranch")
        }
        mt_remote::RemoteGitOperationKind::Merge => t("gitHistory", "operationMerge"),
        mt_remote::RemoteGitOperationKind::Rebase => t("gitHistory", "operationRebase"),
        mt_remote::RemoteGitOperationKind::AddWorktree => t("gitHistory", "operationAddWorktree"),
        mt_remote::RemoteGitOperationKind::RemoveWorktree => {
            t("gitHistory", "operationRemoveWorktree")
        }
        mt_remote::RemoteGitOperationKind::PruneWorktrees => {
            t("gitHistory", "operationPruneWorktrees")
        }
    }
}

fn recovery_status_label(status: Option<&mt_project::git::GitStatus>) -> &'static str {
    match status {
        Some(mt_project::git::GitStatus::Modified) => t("gitCommit", "statusModified"),
        Some(mt_project::git::GitStatus::Added) => t("gitCommit", "statusAdded"),
        Some(mt_project::git::GitStatus::Deleted) => t("gitCommit", "statusDeleted"),
        Some(mt_project::git::GitStatus::Renamed) => t("gitCommit", "statusRenamed"),
        Some(mt_project::git::GitStatus::Untracked) => t("gitCommit", "statusUntracked"),
        Some(mt_project::git::GitStatus::Conflicted) => t("gitCommit", "statusConflicted"),
        None => t("gitHistory", "recoveryUnchanged"),
    }
}

/// Captured selection for feedback; never discovers repositories or performs I/O.
pub(crate) fn current_repo_for_project(store: &AppStore, project_id: &str) -> Option<GitContext> {
    let project = crate::git_backend::project_context(store, project_id);
    REPO_CACHE.with(|cache| {
        cache
            .borrow()
            .get(&project.key())
            .and_then(|(_, selected)| {
                (!selected.is_empty()).then(|| project.with_path(selected.clone()))
            })
    })
}

#[cfg(test)]
pub(crate) fn set_test_selected_repo(store: &AppStore, project_id: &str, repo: &str) {
    let key = crate::git_backend::project_context(store, project_id).key();
    REPO_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .insert(key, (Vec::new(), repo.to_owned()));
    });
}

pub(crate) fn project_has_multiple_repos(store: &AppStore, project_id: Option<&str>) -> bool {
    let Some(id) = project_id else {
        return false;
    };
    let key = crate::git_backend::project_context(store, id).key();
    REPO_CACHE.with(|cache| {
        cache
            .borrow()
            .get(&key)
            .is_some_and(|(repos, _)| repos.len() > 1)
    })
}

impl GitPanel {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project_context = store
            .read(cx)
            .active_project()
            .map(|project| crate::git_backend::project_context(store.read(cx), &project.id));
        let project_path = project_context
            .as_ref()
            .map(|context| context.path().to_string());
        let project_id = store.read(cx).active_project_id.clone();
        let enable_prompted = project_id
            .iter()
            .filter(|id| crate::git_backend::remote_git_enabled(store.read(cx), id))
            .cloned()
            .collect();
        let changes = cx.new(|cx| GitChanges::new(store.clone(), window, cx));
        let history = cx.new(|cx| GitHistoryContent::new(store.clone(), window, cx));
        let detail_pane = cx.new(|cx| CommitDetailPane::new(history.clone(), cx));

        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(
            &changes,
            window,
            |this: &mut Self, _entity, event, window, cx| match event {
                // 提交成功:历史整体重来 + 重载分支(分支头已前移);「更改」区自己已重取,
                // 这三样凑成一整轮(紧接着那次 `.git` 检查据此短路)。「提交并推送」接着开
                // 推送对话框(预选当前分支),在那里看清待推送的提交再确认
                GitChangesEvent::Committed { open_push, context } => {
                    if this.repo_context().as_ref() != Some(context) {
                        return;
                    }
                    this.round_issued = Some(Instant::now());
                    this.history.update(cx, |h, cx| h.reload(cx));
                    this.load_branches(cx);
                    if *open_push {
                        this.open_push_dialog(None, window, cx);
                    }
                }
                GitChangesEvent::OpenPush(context) => {
                    crate::git_push::open(cx.entity(), context.clone(), None, window, cx);
                }
                // 更改区里贮藏了改动:工作区与贮藏数都变了,整面板刷新
                GitChangesEvent::Stashed => this.refresh_after_write(cx),
                // 暂存 / 丢弃 / 冲突处理写完:记作「已知」(见模块注释「签名的先后」)
                GitChangesEvent::IndexWritten(observed) => this.absorb_own_write(*observed, cx),
            },
        ));
        subs.push(cx.subscribe_in(
            &history,
            window,
            |this: &mut Self, _entity, event, window, cx| match event {
                // pty 嗅探回落那条路(`.git` 监听没挂上):只重载分支 —— 仓库栏的分支名由
                // 快照回填,不必为它重新发现仓库
                GitHistoryEvent::RefreshRepos => this.load_branches(cx),
                // 历史区的改写操作(检出 / 重置 / 拣选…)动了 HEAD、分支或工作区:
                // 分支徽章与「更改」列表都要重取(历史区自己已重载),凑成一整轮
                GitHistoryEvent::RepoChanged => {
                    this.round_issued = Some(Instant::now());
                    this.load_branches(cx);
                    this.changes.update(cx, |c, cx| c.load(cx));
                    git_watch::request_check(git_watch::Subscriber::FileTree);
                }
                GitHistoryEvent::CommitUndone { context, message } => {
                    this.changes.update(cx, |c, cx| {
                        c.restore_message_on(context, message, window, cx)
                    });
                }
            },
        ));
        // Original renderer and view-mode state remain shared by both backends.
        subs.push(cx.observe(&store, |_, _, cx| cx.notify()));
        // 项目路径的比对只在活动项目可能变了时做(`StoreEvent::touches_active_project`),
        // 不再被 OSC 标题 / AI 状态这些与仓库无关的变化叫醒
        subs.push(
            cx.subscribe(&store, |this: &mut Self, _, event: &StoreEvent, cx| {
                if matches!(
                    event,
                    StoreEvent::Config(crate::store::ConfigSection::Settings)
                ) {
                    let context = this.active_project_context(cx);
                    let project_id = this.store.read(cx).active_project_id.clone();
                    if context != this.project_context {
                        this.replace_project_context(context, project_id);
                        this.stale = true;
                        if this.visible {
                            this.on_project_changed(cx);
                        }
                        cx.notify();
                    }
                    return;
                }
                if !event.touches_active_project() && !event.touches_ssh_connections() {
                    return;
                }
                let context = this.active_project_context(cx);
                let project_id = this.store.read(cx).active_project_id.clone();
                if context != this.project_context || project_id != this.project_id {
                    this.replace_project_context(context, project_id);
                    if this.visible && (this.window_visible || this.local_project()) {
                        this.on_project_changed(cx);
                    } else {
                        // 收着的时候不扫盘 —— 原版收起时组件根本没挂载。替文件树挂的工作区
                        // 监听跟着换项目(见模块注释「抽屉与窗口」)
                        this.stale = true;
                        this.refresh_hidden_repo(cx);
                    }
                    cx.notify();
                }
            }),
        );
        // 最小化 / 整个被挡住:节拍暂停(监听照记),恢复呈现时补比一次签名
        subs.push(cx.observe_window_visibility(
            window,
            |this: &mut Self, visibility, _window, cx| {
                this.set_window_visible(visibility.is_visible(), cx)
            },
        ));
        // 抽屉一开始是收着的:替文件树挂上工作区监听(见模块注释「抽屉与窗口」)。等实体
        // 建好再做 —— 这里还没有 `self`
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this: &mut Self, cx| this.refresh_hidden_repo(cx));
        })
        .detach();

        Self {
            store,
            changes,
            history,
            detail_pane,
            repos: Vec::new(),
            selected_repo: String::new(),
            branches: Vec::new(),
            branches_loading: false,
            snapshot_in_flight: false,
            view_branch: None,
            meta: RepoMeta::default(),
            branch_gen: 0,
            branch_data: RefCell::new(None),
            busy: None,
            feedback_context: None,
            enable_prompted,
            visible: false,
            stale: true,
            project_path,
            project_context,
            project_id,
            load_error: None,
            branch_request: 0,
            repo_request: 0,
            dir_watch: GitDirWatch::new(),
            watch_request: 0,
            sig_checking: false,
            sig_request: 0,
            sig_recheck: false,
            sig_observed_at: None,
            round_issued: None,
            deferred_check: None,
            pty_hit_at: None,
            catch_up: false,
            window_visible: window.is_visible(),
            work_watch: WorktreeWatch::new(),
            work_checking: false,
            index_lock: IndexLockGate::default(),
            partial_polled: None,
            hidden_repo: None,
            hidden_request: 0,
            _feeder: None,
            sections_height: 0.0,
            natural_seen: (None, None),
            effective_ratio: 0.5,
            bar_left_width: 0.0,
            sync_badge: SyncBadge::Full,
            bar_squeeze: BarSqueeze::None,
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
        let context = self.active_project_context(cx);
        let project_id = self.store.read(cx).active_project_id.clone();
        if context.as_ref().is_some_and(GitContext::is_remote)
            && let Some(id) = &project_id
        {
            self.enable_prompted.insert(id.clone());
        }
        if context != self.project_context || project_id != self.project_id {
            self.replace_project_context(context, project_id);
            self.stale = true;
        }
        git_watch::set_enabled(visible && self.local_project());
        if visible {
            if self.stale && (self.window_visible || self.local_project()) {
                self.on_project_changed(cx);
            } else if !self.stale
                && !self.selected_repo.is_empty()
                && (self.window_visible || self.local_project())
            {
                // 同项目再打开(见模块注释「抽屉与窗口」):收着时 `.git` 监听与节拍都停、
                // 工作区那一路只把路径交给了文件树,「更改」只能重取一次补上;引用 / 索引
                // 变没变等 `.git` 监听挂上后比一次签名再定,不重新发现仓库
                self.changes.update(cx, |c, cx| c.load(cx));
                self.catch_up = true;
            }
            if self.window_visible {
                self.start_tick(cx);
            }
        } else {
            self._tick = None;
            self.catch_up = false;
            self.pty_hit_at = None;
            // 收着不重取「更改」,重新打开时整份重取
            self.index_lock.reset();
        }
        if visible {
            // 按当时选中的仓库挂两路监听
            self.retarget_watch(cx);
        } else {
            // `.git` 那一路撤掉,工作区那一路替文件树接着听
            self.refresh_hidden_repo(cx);
        }
        cx.notify();
    }

    /// 窗口呈现与否变了(见模块注释「抽屉与窗口」)。不可见时停节拍 —— 监听的 sink 照记,
    /// 节拍一恢复就交;恢复时另补比一次签名兜底。
    fn set_window_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.window_visible == visible {
            return;
        }
        self.window_visible = visible;
        if !self.visible {
            // 抽屉收着:替文件树等唤醒口的任务跟着停 / 起(恢复时先取一遍攒下的那批)
            self.sync_feeder(cx);
            return;
        }
        if visible {
            if self.stale {
                self.on_project_changed(cx);
            } else {
                self.start_tick(cx);
                if self
                    .project_context
                    .as_ref()
                    .is_some_and(GitContext::is_remote)
                {
                    self.changes.update(cx, |changes, cx| changes.load(cx));
                }
                self.check_git_dir_change(cx);
            }
        } else {
            self._tick = None;
        }
    }

    /// 抽屉左侧要挂的「提交详情」栏(见 `git_history::detail` 模块注释)。没选中提交、
    /// 详情被关上、「提交历史」区收着、面板没有项目时为 `None`。
    ///
    /// 不看 [`Self::visible`]:抽屉收起的退场动画期间面板已置为不可见,详情栏要跟着
    /// 抽屉一起滑出去,不该先没了。
    pub fn detail_pane(&self, cx: &App) -> Option<Entity<CommitDetailPane>> {
        let history_open = SECTION_UI.with(|s| s.borrow().history_open);
        let shown = history_open
            && self.store.read(cx).active_project().is_some()
            && self.history.read(cx).detail_shown();
        shown.then(|| self.detail_pane.clone())
    }

    /// pty-output 嗅探的节拍。100ms 一拍:抽干旁路 → 命中按 [`PtyRoute`] 分流(监听挂着就
    /// 只叫它比签名;否则通知两个子面板,**各自** 500ms 去抖,原版是两个独立定时器)。
    /// 同一拍顺带问一声 `.git` / 工作区监听有没有攒够一批安静下来的变化。
    fn start_tick(&mut self, cx: &mut Context<Self>) {
        let interval = if self
            .project_context
            .as_ref()
            .is_some_and(GitContext::is_remote)
        {
            REMOTE_POLL
        } else {
            Duration::from_millis(git_watch::POLL_MS)
        };
        self._tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                let alive = this
                    .update(cx, |this: &mut Self, cx| this.on_tick(cx))
                    .is_ok();
                if !alive {
                    return;
                }
            }
        }));
    }

    fn on_tick(&mut self, cx: &mut Context<Self>) {
        if !self.visible || !self.window_visible {
            return;
        }
        if self
            .project_context
            .as_ref()
            .is_some_and(GitContext::is_remote)
        {
            self.changes.update(cx, |c, cx| c.load(cx));
            self.check_git_dir_change(cx);
            return;
        }
        if git_watch::drain_hit() {
            let watching = self.dir_watch.repo() == Some(self.selected_repo.as_str());
            match pty_route(watching) {
                PtyRoute::Signature => {
                    self.pty_hit_at = Some(Instant::now());
                    self.dir_watch.poke();
                }
                PtyRoute::Debounce => {
                    self.changes.update(cx, |c, _| c.note_pty_hit());
                    self.history.update(cx, |h, _| h.note_pty_hit());
                }
            }
        }
        self.changes.update(cx, |c, cx| c.tick(cx));
        self.history.update(cx, |h, cx| h.tick(cx));
        if self.dir_watch.take_due() {
            self.check_git_dir_change(cx);
        }
        // 上一批还在后台查:这一拍不取 —— 取出来也只能原样退回,每拍空转一次
        if !self.work_checking {
            if let Some(batch) = self.work_watch.take_due() {
                self.check_worktree_change(Some(batch), cx);
            } else if self.index_lock.retry_due(Instant::now()) {
                // 「更改」的重取压着等 index.lock:到点复查一次锁(不是每拍都查)
                self.check_worktree_change(None, cx);
            }
        }
        self.poll_partial_worktree(cx);
    }

    /// 工作区监听深处没挂上(Linux 上目录太多,见 `git_dir_watch` 模块注释「递归子树」):
    /// 每 [`PARTIAL_POLL`] 兜底重取一次「更改」;这段时间里已经有一趟开跑的就不再要。
    fn poll_partial_worktree(&mut self, cx: &mut Context<Self>) {
        if !self.work_watch.is_partial()
            || self.work_watch.repo() != Some(self.selected_repo.as_str())
        {
            return;
        }
        let now = Instant::now();
        if self
            .partial_polled
            .is_some_and(|at| now.duration_since(at) < PARTIAL_POLL)
        {
            return;
        }
        self.partial_polled = Some(now);
        let since = now.checked_sub(PARTIAL_POLL).unwrap_or(now);
        if !self.changes.read(cx).load_covers(since) {
            self.changes.update(cx, |c, cx| c.load(cx));
        }
    }

    fn active_project_context(&self, cx: &App) -> Option<GitContext> {
        let store = self.store.read(cx);
        store
            .active_project()
            .map(|project| crate::git_backend::project_context(store, &project.id))
    }

    fn local_project(&self) -> bool {
        self.project_context
            .as_ref()
            .and_then(GitContext::local_path)
            .is_some()
    }

    fn remote_git_disabled(&self, cx: &App) -> bool {
        let store = self.store.read(cx);
        store.active_project().is_some_and(|project| {
            project.ssh_connection_id.is_some()
                && !crate::git_backend::remote_git_enabled(store, &project.id)
        })
    }

    fn replace_project_context(&mut self, context: Option<GitContext>, project_id: Option<String>) {
        if context.as_ref().is_some_and(GitContext::is_remote)
            && let Some(id) = &project_id
        {
            self.enable_prompted.insert(id.clone());
        }
        self.repo_request += 1;
        self.watch_request += 1;
        self.hidden_request += 1;
        self.reset_repo_state();
        self.repos.clear();
        self.selected_repo.clear();
        self.project_path = context.as_ref().map(|context| context.path().to_string());
        self.project_context = context;
        self.project_id = project_id;
    }

    /// 换仓库 / 换项目:分支、元数据、签名的先后记录全部作废;在途的分支快照也作废 ——
    /// 它属于上一个仓库,且不作废的话「在途」标记会一直挂着(签名检查就一直被压着)。
    fn reset_repo_state(&mut self) {
        self.busy = None;
        self.view_branch = None;
        self.branches.clear();
        self.meta = RepoMeta::default();
        // 分支数据换了:分支弹窗缓存的行作废
        self.branch_gen += 1;
        self.branch_request += 1;
        self.branches_loading = false;
        self.snapshot_in_flight = false;
        self.sig_observed_at = None;
        self.round_issued = None;
        self.deferred_check = None;
        self.pty_hit_at = None;
        self.sig_checking = false;
        self.sig_request += 1;
        self.sig_recheck = false;
        self.load_error = None;
    }

    /// 项目变了:先吃缓存(没有就清空)→ 重新发现仓库。
    fn on_project_changed(&mut self, cx: &mut Context<Self>) {
        self.stale = false;
        self.reset_repo_state();
        self.repo_request += 1;
        if self.remote_git_disabled(cx) {
            self._tick = None;
            self.repos.clear();
            self.selected_repo.clear();
            self.push_repo_down(cx);
            return;
        }
        git_watch::set_enabled(self.visible && self.local_project());
        if self.visible && self.window_visible {
            self.start_tick(cx);
        }
        let key = self
            .project_context
            .as_ref()
            .map(GitContext::key)
            .unwrap_or_default();
        let cached = REPO_CACHE.with(|c| c.borrow().get(&key).cloned());
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

    /// 发现仓库(**重**:首次扫盘深度 5)。两种后端共用原面板与仓库缓存。
    /// Worktree 管理弹窗改过之后跑(见模块注释「什么时候重新发现仓库」)。
    fn load_repos(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.project_context.clone() else {
            return;
        };
        let key = context.key();
        self.repo_request += 1;
        let req = self.repo_request;
        cx.spawn(async move |this, cx| {
            let executor = context.clone();
            let result =
                mt_remote::run_git_on(&executor, move || context.discover_git_repos()).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.repo_request != req
                    || this.project_context.as_ref().map(GitContext::key).as_ref() != Some(&key)
                {
                    return;
                }
                let before = this.selected_repo.clone();
                let repos_before = repo_list_key(&this.repos);
                let error_before = this.load_error.clone();
                match result {
                    Ok(repos) => {
                        this.load_error = None;
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
                                key.clone(),
                                (this.repos.clone(), this.selected_repo.clone()),
                            )
                        });
                    }
                    Err(err) => {
                        eprintln!("[git] 发现仓库失败: {err:#}");
                        this.load_error = Some(git_ui::error_text(&err));
                        this.repos.clear();
                        this.selected_repo.clear();
                    }
                }
                if before != this.selected_repo {
                    let error = this.load_error.take();
                    this.reset_repo_state();
                    this.load_error = error;
                }
                this.push_repo_down(cx);
                if branches_stale_after_discovery(
                    &before,
                    &this.selected_repo,
                    this.branches.is_empty(),
                    this.snapshot_in_flight,
                ) {
                    this.load_branches(cx);
                }
                this.retarget_watch(cx);
                // 扫出来的与眼下一样(手动 ↻ 的常态)就不重画:分支快照那边自己会画
                if this.load_error != error_before
                    || before != this.selected_repo
                    || repos_before != repo_list_key(&this.repos)
                {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 重载分支列表与随它一起的元数据(领先落后、最近分支、进行中操作、签名)。不转圈,
    /// 见 [`Self::load_branches_with`]。
    fn load_branches(&mut self, cx: &mut Context<Self>) {
        self.load_branches_with(false, cx);
    }

    /// `spin` = 仓库栏的刷新按钮转圈(手动 ↻);还一条分支都没有时也转(弹窗里是「加载中」
    /// 占位)。自动刷新不转圈,结果与眼下相同时整趟不重画(见模块注释)。
    fn load_branches_with(&mut self, spin: bool, cx: &mut Context<Self>) {
        let Some(context) = self.repo_context() else {
            self.branches.clear();
            self.meta = RepoMeta::default();
            self.branches_loading = false;
            self.snapshot_in_flight = false;
            self.branch_gen += 1;
            return;
        };
        let spin = spin || self.branches.is_empty();
        let spinner_changed = spin && !self.branches_loading;
        self.branches_loading |= spin;
        self.snapshot_in_flight = true;
        // 这趟的签名读得不早于此刻(快照先读签名)
        let issued = Instant::now();
        self.branch_request += 1;
        let req = self.branch_request;
        let repo = context.clone();
        cx.spawn(async move |this, cx| {
            let executor = context.clone();
            let result =
                mt_remote::run_git_on(&executor, move || load_branch_snapshot(&context)).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                // 迟到响应丢弃:换仓库后的旧响应不许覆盖(换仓库时令牌已前移)
                if this.branch_request != req {
                    return;
                }
                this.snapshot_in_flight = false;
                if this.repo_context().as_ref() != Some(&repo) {
                    // 令牌没换、仓库却换了(不该出现):在途标记照样收掉,别把检查一直压着
                    this.branches_loading = false;
                    return;
                }
                // 转着圈的那趟落地一定要画一下把圈收掉
                let mut changed = std::mem::take(&mut this.branches_loading);
                match result {
                    Ok((list, mut meta)) => {
                        changed |= this.load_error.take().is_some();
                        // 正在查看的分支没了(被删 / 改名):回到跟随 HEAD,
                        // 否则历史区拿着一个不存在的分支名去查
                        if this
                            .view_branch
                            .as_ref()
                            .is_some_and(|v| !list.iter().any(|b| &b.name == v))
                        {
                            this.view_branch = None;
                            changed = true;
                        }
                        let selected = this.selected_repo.clone();
                        if let Some(info) = this
                            .repos
                            .iter_mut()
                            .find(|r| r.path.to_string_lossy() == selected)
                            && info.current_branch != meta.head_label
                        {
                            info.current_branch = meta.head_label.clone();
                            changed = true;
                        }
                        // 签名与冲突数:手上那份若是这趟发出之后才读的(检查 / 面板自己写完索引),
                        // 它更新,沿用(见模块注释「签名的先后」)
                        if keep_newer_observation(this.sig_observed_at, issued) {
                            meta.signature = this.meta.signature;
                            meta.op = this.meta.op;
                        } else {
                            this.sig_observed_at = Some(issued);
                        }
                        changed |= this.branches != list || !meta_view_eq(&this.meta, &meta);
                        this.branches = list;
                        this.meta = meta;
                        this.branch_gen += 1;
                    }
                    Err(err) => {
                        eprintln!("[git] 取分支失败: {err:#}");
                        let error = git_ui::error_text(&err);
                        changed |= this.load_error.as_ref() != Some(&error);
                        this.load_error = Some(error);
                    }
                }
                this.push_repo_down(cx);
                if changed {
                    cx.notify();
                }
                // 在途期间压着的那次检查:拿刚落地的签名再比
                if let Some(observed) = this.deferred_check.take()
                    && (this.local_project() || (this.visible && this.window_visible))
                {
                    this.apply_observation(observed, cx);
                }
            });
        })
        .detach();
        if spinner_changed {
            cx.notify();
        }
    }

    /// 仓库内容变了之后的一整轮重取:分支快照 + 「更改」+ 提交历史,文件树按签名核一遍。
    /// **不**重新发现仓库(见模块注释)。`round` 决定转不转圈、历史区静不静默。
    fn reload_views(&mut self, round: Round, cx: &mut Context<Self>) {
        self.round_issued = Some(Instant::now());
        self.load_branches_with(round == Round::Manual, cx);
        // 「更改」列表要显式重取 —— push_repo_down 的 set_repo 对同仓库路径会短路
        self.changes.update(cx, |c, cx| c.load(cx));
        self.history.update(cx, |h, cx| match round {
            Round::Watch => h.reload_quiet(cx),
            Round::Manual | Round::Write => h.reload(cx),
        });
        // 文件树的 git 标记也跟上:合并 / 变基「继续」做完、终端 / AI pane 里提交之后它
        // 不一定听得到。手动 ↻ 无条件重取;其余让它比签名(它自己的 `.git` 事件多半已经取过)
        if round == Round::Manual {
            git_watch::request_refresh(git_watch::Subscriber::FileTree);
        } else {
            git_watch::request_check(git_watch::Subscriber::FileTree);
        }
    }

    /// 手动 ↻:重新发现仓库 + 一整轮重取 + 文件树无条件重取。
    pub(crate) fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.load_repos(cx);
        self.reload_views(Round::Manual, cx);
    }

    /// 写操作(Fetch / 拉取 / 推送 / 检出 / 分支增删改 / 贮藏 / 横幅「继续」「中止」…)做完:
    /// 一整轮重取,不重新发现仓库 —— 仓库集合没变,分支名由快照回填。
    pub(crate) fn refresh_after_write(&mut self, cx: &mut Context<Self>) {
        self.reload_views(Round::Write, cx);
    }

    fn recovery_controls(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let state = self.store.read(cx);
        let id = state.active_project_id.as_deref()?;
        let context = crate::git_backend::recovery_context(state, id)?;
        let mut controls = div().flex().flex_col().gap(px(8.0)).p(px(10.0));
        let mut any = false;
        if let Some(unknown) = context.unknown_write() {
            any = true;
            controls = controls
                .child(ui::desc_text(tr!(
                    "gitHistory",
                    "verifyRepository",
                    repo = unknown.repo
                )))
                .child(
                    crate::prompt::footer_button(
                        "verify-remote-write",
                        t("gitHistory", "verifyWrite"),
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.verify_remote_write(window, cx)),
                    ),
                )
                .child(
                    crate::prompt::footer_button(
                        "abandon-remote-write",
                        t("gitHistory", "abandonAction"),
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.abandon_remote_write(window, cx)),
                    ),
                );
        }
        let blockers = if self.selected_repo.is_empty() {
            context.recovery_blockers_in_scope()
        } else {
            context.with_path(&self.selected_repo).recovery_blockers()
        };
        for blocker in blockers
            .into_iter()
            .filter(|blocker| blocker.owner_project_id != id)
        {
            any = true;
            let owner = blocker.owner_project_id;
            let name = state
                .project(&owner)
                .map(|project| project.name.clone())
                .unwrap_or_else(|| owner.clone());
            let store = self.store.clone();
            controls = controls
                .child(ui::desc_text(tr!(
                    "gitHistory",
                    "blockedByProject",
                    project = name,
                    repo = blocker.write.repo
                )))
                .child(
                    crate::prompt::footer_button(
                        "recovery-owner-project",
                        tr!("gitHistory", "goToRecoveryProject", project = name),
                    )
                    .on_click(move |_, _, cx| {
                        store.update(cx, |store, cx| store.set_active_project(&owner, cx))
                    }),
                );
        }
        any.then(|| controls.into_any_element())
    }

    fn verify_remote_write(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.store.read(cx);
        let Some(id) = state.active_project_id.clone() else {
            return;
        };
        let Some(repo) = crate::git_backend::recovery_context(state, &id) else {
            return;
        };
        if !crate::git_backend::remote_git_enabled(state, &id) {
            let panel = cx.entity().downgrade();
            let consent = match repo.recovery_consent() {
                Ok(consent) => Rc::new(RefCell::new(Some(consent))),
                Err(error) => {
                    git_ui::show_git_error(
                        t("gitHistory", "verifyWrite"),
                        git_ui::error_text(&error),
                        window,
                        cx,
                    );
                    return;
                }
            };
            Confirm::new(
                t("gitHistory", "recoveryReadConsentTitle"),
                t("gitHistory", "recoveryReadConsentMessage"),
            )
            .detail(vec![git_ui::repo_label(&repo)])
            .ok_text(t("gitHistory", "recoveryReadConsentAction"))
            .open(
                move |window, cx| {
                    let Some(consent) = consent.borrow_mut().take() else {
                        return;
                    };
                    let _ = panel.update(cx, |panel, cx| {
                        panel.load_recovery_snapshot(repo.clone(), Some(consent), window, cx)
                    });
                },
                window,
                cx,
            );
        } else {
            self.load_recovery_snapshot(repo, None, window, cx);
        }
    }

    fn abandon_remote_write(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.store.read(cx);
        let Some(id) = state.active_project_id.as_deref() else {
            return;
        };
        let Some(repo) = crate::git_backend::recovery_context(state, id) else {
            return;
        };
        let confirmation = match repo.prepare_offline_abandon() {
            Ok(confirmation) => Rc::new(RefCell::new(Some(confirmation))),
            Err(error) => {
                git_ui::show_git_error(
                    t("gitHistory", "abandonTitle"),
                    git_ui::error_text(&error),
                    window,
                    cx,
                );
                return;
            }
        };
        let panel = cx.entity().downgrade();
        Confirm::new(
            t("gitHistory", "abandonTitle"),
            t("gitHistory", "abandonWarning"),
        )
        .detail(vec![git_ui::repo_label(&repo)])
        .danger()
        .ok_text(t("gitHistory", "abandonAction"))
        .open(
            move |window, cx| {
                let Some(confirmation) = confirmation.borrow_mut().take() else {
                    return;
                };
                match repo.abandon_recovery(confirmation) {
                    Ok(()) => {
                        let _ = panel.update(cx, |panel, cx| {
                            git_ui::toast_for_repo(
                                &panel.store,
                                &repo,
                                t("gitHistory", "abandoned").to_string(),
                                Tone::Attention,
                                cx,
                            );
                            cx.notify();
                        });
                    }
                    Err(error) => git_ui::show_git_error(
                        t("gitHistory", "abandonTitle"),
                        git_ui::error_text(&error),
                        window,
                        cx,
                    ),
                }
            },
            window,
            cx,
        );
    }

    fn load_recovery_snapshot(
        &mut self,
        repo: GitContext,
        consent: Option<mt_remote::RemoteGitRecoveryConsent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let captured = repo.clone();
        let store = self.store.clone();
        let panel = cx.entity().downgrade();
        let recovery_only = consent.is_some();
        window.spawn(cx, async move |cx| {
                let result = mt_remote::run_git_on(&captured, move || {
                    if let Some(consent) = consent { repo.recovery_snapshot_with_consent(consent) }
                    else { repo.recovery_snapshot() }
                }).await;
                let _ = cx.update(|window, cx| match result {
                    Ok(snapshot) => {
                        let format_time = |time: std::time::SystemTime| crate::git_history::format_absolute_time(time.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs().min(i64::MAX as u64) as i64);
                        let observed = format_time(snapshot.observed);
                        let occurred = format_time(snapshot.occurred);
                        let message = match (recovery_only, snapshot.completion_proven) {
                            (true, true) => t("gitHistory", "recoveryOnlyProven"),
                            (true, false) => t("gitHistory", "recoveryOnlyRisk"),
                            (false, true) => t("gitHistory", "recoveryProven"),
                            (false, false) => t("gitHistory", "recoveryRisk"),
                        };
                        let mut detail = vec![
                            git_ui::repo_label(&captured.with_path(snapshot.repo.clone())),
                            remote_operation_label(snapshot.operation).to_string(),
                            tr!("gitHistory", "recoveryOccurred", timestamp = occurred),
                            format!("HEAD: {}", snapshot.head.as_deref().unwrap_or("∅")),
                            tr!(
                                "gitHistory",
                                "recoverySignature",
                                refs = snapshot.signature.refs,
                                index = snapshot.signature.index
                            ),
                            tr!(
                                "gitHistory",
                                "recoveryOperation",
                                operation = snapshot.op.kind.map(|kind| match kind { RepoOpKind::Merge => t("gitBranch", "opMerge"), RepoOpKind::Rebase => t("gitBranch", "opRebase"), RepoOpKind::CherryPick => t("gitLog", "action.cherryPick"), RepoOpKind::Revert => t("gitLog", "action.revert") }).unwrap_or_else(|| t("gitHistory", "recoveryClean")),
                                conflicts = snapshot.op.conflicted
                            ),
                            tr!(
                                "gitHistory",
                                "recoveryFiles",
                                count = snapshot.files.len(),
                                timestamp = observed
                            ),
                        ];
                        detail.extend(snapshot.files.iter().take(20).map(|file| {
                            format!("{} · {} / {}", file.path,
                                recovery_status_label(file.staged_status.as_ref()),
                                recovery_status_label(file.unstaged_status.as_ref()))
                        }));
                        let context = captured.clone();
                        Confirm::new(t("gitHistory", "verifyWrite"), message)
                            .detail(detail)
                            .ok_text(t("gitHistory", if recovery_only { "acknowledgeReadOnlyRecovery" } else { "acknowledgeRecovery" }))
                            .open(
                                move |window, cx| {
                                    let context = context.clone();
                                    let executor = context.clone();
                                    let snapshot = snapshot.clone();
                                    let store = store.clone();
                                    let panel = panel.clone();
                                    window
                                        .spawn(cx, async move |cx| {
                                            let result =
                                                mt_remote::run_git_on(&executor, move || {
                                                    context.acknowledge_recovery(&snapshot)
                                                })
                                                .await;
                                            let _ = cx.update(|window, cx| match result {
                                                Ok(()) => {
                                                    git_ui::toast_for_repo(
                                                        &store,
                                                        &executor,
                                                        t("gitHistory", if recovery_only { "readOnlyVerified" } else { "writeVerified" })
                                                            .to_string(),
                                                        Tone::Info,
                                                        cx,
                                                    );
                                                    let _ = panel.update(cx, |panel, cx| {
                                                        if panel
                                                            .project_context
                                                            .as_ref()
                                                            .and_then(GitContext::project_id)
                                                            == executor.project_id()
                                                        {
                                                            if crate::git_backend::remote_git_enabled(panel.store.read(cx), executor.project_id().unwrap_or("")) { panel.refresh_after_write(cx); } else { cx.notify(); }
                                                        }
                                                    });
                                                }
                                                Err(err) => git_ui::show_git_error(
                                                    t("gitHistory", "verifyWrite"),
                                                    git_ui::error_text(&err),
                                                    window,
                                                    cx,
                                                ),
                                            });
                                        })
                                        .detach();
                                },
                                window,
                                cx,
                            );
                    }
                    Err(err) => git_ui::show_git_error(
                        t("gitHistory", "verifyWrite"),
                        git_ui::error_text(&err),
                        window,
                        cx,
                    ),
                });
            })
            .detach();
    }

    /// 把仓库 / 分支透给两个子面板。
    fn push_repo_down(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo_context();
        let branches = self.branches.clone();
        let head_branch = self.current_branch().map(str::to_string);
        let view_branch = self.view_branch.clone();
        self.changes
            .update(cx, |c, cx| c.set_repo(repo.clone(), cx));
        self.history.update(cx, |h, cx| {
            h.sync(
                repo,
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
        self.reset_repo_state();
        if let Some(project) = self.project_context.as_ref().map(GitContext::key) {
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

    /// 分支弹窗的数据快照。分支数据共享面板缓存的那一份(见 [`BranchData`]):分支弹窗
    /// 开着时每帧都要取,数据没换就不深拷贝。
    pub(crate) fn branch_snapshot(&self) -> BranchSnapshot {
        BranchSnapshot {
            repo: self.selected_repo.clone(),
            context: self
                .repo_context()
                .unwrap_or_else(|| GitContext::broken(t("gitHistory", "selectRepo").to_string())),
            data: self.branch_data(),
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

    /// 共享的分支数据:缓存着、且代号对得上就直接给,否则按眼下的 `branches` / `meta`
    /// 建一份(只在分支数据换了之后的第一次取快照时深拷贝)。
    fn branch_data(&self) -> Rc<BranchData> {
        let mut cache = self.branch_data.borrow_mut();
        if let Some(data) = cache.as_ref().filter(|d| d.generation == self.branch_gen) {
            return data.clone();
        }
        let data = Rc::new(BranchData {
            generation: self.branch_gen,
            branches: self.branches.clone(),
            tracking: self.meta.tracking.clone(),
            recent: self.meta.recent.clone(),
            remotes: self.meta.remotes.clone(),
        });
        *cache = Some(data.clone());
        data
    }

    /// 面板自己没在跑写操作,但仓库的写操作闸被别的区占着(见模块注释「写操作」)。
    fn gated(&self) -> bool {
        self.busy.is_none()
            && self
                .repo_context()
                .is_some_and(|repo| git_ui::gate::is_busy_key(&repo.repository_key()))
    }

    pub(crate) fn repo_context(&self) -> Option<GitContext> {
        if self.selected_repo.is_empty() {
            return None;
        }
        self.project_context
            .as_ref()
            .map(|context| context.with_path(self.selected_repo.clone()))
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
        if let Some(repo) = &self.feedback_context {
            git_ui::toast_for_repo(&self.store, repo, message, tone, cx);
        } else {
            git_ui::toast(&self.store, message, tone, cx);
        }
    }

    /// 跑一个 git 写操作(见模块注释「写操作」)。
    ///
    /// `op` 占着仓库的写操作闸在后台执行器上跑(阻塞 CLI,见 [`git_ui::spawn_gated`]);
    /// `done` 拿到成功结果后在主线程上推摘要 / 接着开下一个确认框。失败统一弹详情框,
    /// 标题是「{动作}失败」。无论成败都整面板刷新([`Self::refresh_after_write`])—— 失败也
    /// 可能已经改了一半(合并停在冲突上之类)。闸被别的区占着时不执行,推一条 toast 说明。
    pub(crate) fn run_write_op<T: Send + 'static>(
        &mut self,
        busy: GitBusy,
        op: impl FnOnce(&GitContext) -> anyhow::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo_context() else {
            return;
        };
        self.run_write_op_on(&repo, busy, op, done, window, cx);
    }

    pub(crate) fn run_write_op_on<T: Send + 'static>(
        &mut self,
        repo: &GitContext,
        busy: GitBusy,
        op: impl FnOnce(&GitContext) -> anyhow::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.repo_context().as_ref() == Some(repo);
        if active && self.busy.is_some() {
            return;
        }
        let completed_repo = repo.clone();
        let this = cx.entity().downgrade();
        let started = git_ui::spawn_gated(
            repo,
            op,
            move |result, window, cx| {
                let _ = this.update(cx, |this: &mut Self, cx| {
                    let active = this.repo_context().as_ref() == Some(&completed_repo);
                    if active {
                        this.busy = None;
                    }
                    match result {
                        Ok(value) => {
                            this.feedback_context = Some(completed_repo.clone());
                            done(this, value, window, cx);
                            this.feedback_context = None;
                        }
                        Err(err) => {
                            eprintln!("[git] {}失败: {err:#}", busy.label());
                            git_ui::show_git_error(
                                git_ui::repo_text(
                                    &this.store,
                                    &completed_repo,
                                    tr!("gitBranch", "opFailed", op = busy.label()),
                                    cx,
                                ),
                                git_ui::error_text_for_store(&err, &this.store, cx),
                                window,
                                cx,
                            );
                        }
                    }
                    if active {
                        this.refresh_after_write(cx);
                        cx.notify();
                    }
                    this.feedback_context = None;
                });
            },
            window,
            cx,
        );
        if !started {
            self.notify_result(git_ui::gate_busy_text().to_string(), Tone::Attention, cx);
            return;
        }
        if active {
            self.busy = Some(busy);
            cx.notify();
        }
    }

    /// `git fetch --all`。
    pub(crate) fn fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_write_op(
            GitBusy::Fetch,
            GitContext::git_fetch_all,
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
                Some(mode) => repo.git_pull_with(mode),
                None => repo.git_pull_by_config(),
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
        let Some(repo) = self.repo_context() else {
            return;
        };
        crate::git_push::open(cx.entity(), repo, branch, window, cx);
    }

    /// 打开贮藏列表(仓库栏贮藏按钮)。有写操作在跑时不开 —— 贮藏的改写同样要抢
    /// `index.lock`。
    pub(crate) fn open_stash_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_repo.is_empty() || self.busy.is_some() {
            return;
        }
        let Some(repo) = self.repo_context() else {
            return;
        };
        let on_changed = self.stash_on_changed(false, cx);
        crate::git_stash::open_list(self.store.clone(), repo, on_changed, window, cx);
    }

    /// 检出 `branch` 会被本地改动挡住(分支弹窗检出前的预检,见 `git_branches::checkout`):
    /// 开「贮藏后检出」确认框。
    pub(crate) fn open_smart_checkout_on(
        &mut self,
        repo: &GitContext,
        branch: String,
        conflicts: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repo = repo.clone();
        let on_changed = self.stash_on_changed_on(Some(repo.clone()), true, cx);
        crate::git_stash::open_smart_checkout(
            self.store.clone(),
            repo,
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
        self.stash_on_changed_on(self.repo_context(), follow_head, cx)
    }

    fn stash_on_changed_on(
        &self,
        repo: Option<GitContext>,
        follow_head: bool,
        cx: &mut Context<Self>,
    ) -> Rc<dyn Fn(&mut App)> {
        let panel = cx.entity().downgrade();
        Rc::new(move |cx: &mut App| {
            let _ = panel.update(cx, |p, cx| {
                if p.repo_context() != repo {
                    return;
                }
                if follow_head {
                    p.set_view_branch(None, cx);
                }
                p.refresh_after_write(cx);
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
            move |repo| repo.continue_operation(kind),
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
        let Some(repo) = self.repo_context() else {
            return;
        };
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
                    this.run_write_op_on(
                        &repo,
                        GitBusy::Abort,
                        move |repo| repo.abort_operation(kind),
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

    /// 让监听目标跟上眼下的样子:抽屉开着 = 选中的仓库上挂 `.git` 与工作区两路;收着 = 只留
    /// 工作区那一路替文件树听 [`Self::hidden_repo`](见模块注释「抽屉与窗口」)。两路各自比,
    /// 没变的那一路不动(抽屉开合时工作区监听不必撤了重挂)。目录在后台列(要读盘)。
    fn retarget_watch(&mut self, cx: &mut Context<Self>) {
        let local = self.local_project();
        let want_dir = self.repo_context().and_then(|context| {
            self.visible
                .then(|| {
                    context
                        .local_path()
                        .map(|path| path.to_string_lossy().into_owned())
                })
                .flatten()
        });
        let want_work = if self.visible {
            want_dir.clone()
        } else {
            self.hidden_repo.clone().filter(|_| local)
        };
        let dir_ok = want_dir.as_deref() == self.dir_watch.repo();
        let work_ok = want_work.as_deref() == self.work_watch.repo();
        if !(dir_ok && work_ok) {
            // 在途的那趟列目录作废:它可能正替刚撤掉的目标列
            self.watch_request += 1;
        }
        if !dir_ok {
            self.dir_watch.clear();
        }
        if !work_ok {
            self.work_watch.clear();
            self.index_lock.reset();
            self.partial_polled = None;
        }
        self.sync_feeder(cx);
        let dir_repo = want_dir.filter(|_| !dir_ok);
        let work_repo = want_work.filter(|_| !work_ok);
        if dir_repo.is_none() && work_repo.is_none() {
            // 同项目重新打开、两路都还挂着:当场比一次签名(见 `set_visible`)
            if std::mem::take(&mut self.catch_up) {
                self.check_git_dir_change(cx);
            }
            return;
        }
        let req = self.watch_request;
        // These targets were obtained only through local_path in the watcher gate.
        let dir_task = dir_repo.as_ref().map(PathBuf::from);
        let work_task = work_repo.as_ref().map(PathBuf::from);
        cx.spawn(async move |this, cx| {
            let (git_dirs, work_dirs) = cx
                .background_executor()
                .spawn(async move {
                    let git_dirs = dir_task.map(|repo| {
                        mt_project::git::git_dir_layout(&repo).map(|layout| {
                            let dirs = mt_project::git::git_watch_dirs(&layout);
                            (layout, dirs)
                        })
                    });
                    let work_dirs =
                        work_task.map(|repo| mt_project::git::worktree_watch_dirs(&repo));
                    (git_dirs, work_dirs)
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.watch_request != req {
                    return;
                }
                match (dir_repo, git_dirs) {
                    (Some(repo), Some(Ok((layout, dirs)))) => {
                        this.dir_watch.set_target(repo, layout, dirs);
                    }
                    // 监听挂不上不影响使用:手动 ↻ 与 pty 嗅探(回落去抖那条)照旧
                    (_, Some(Err(err))) => eprintln!("[git] 监听 .git 失败: {err:#}"),
                    _ => {}
                }
                match (work_repo, work_dirs) {
                    (Some(repo), Some(Ok(plan))) => {
                        log_worktree_plan(&plan);
                        this.work_watch.set_target(repo, plan);
                    }
                    (_, Some(Err(err))) => eprintln!("[git] 监听工作区失败: {err:#}"),
                    _ => {}
                }
                this.sync_feeder(cx);
                // 同项目重新打开:收着期间引用 / 索引变没变,当场比一次签名(见 `set_visible`)
                if std::mem::take(&mut this.catch_up) {
                    this.check_git_dir_change(cx);
                }
            });
        })
        .detach();
    }

    /// 抽屉收着时替文件树挂工作区监听的仓库(见模块注释「抽屉与窗口」):面板对当前项目选过
    /// 的那个(没换过项目就是选中的,换过就查 [`REPO_CACHE`]);都没有就在后台找项目根所属
    /// 的仓库 —— 只往上找,不像 `discover_git_repos` 那样扫盘。父目录型项目在抽屉打开过之前
    /// 找不到,不替它挂。抽屉开着时什么都不做。
    fn refresh_hidden_repo(&mut self, cx: &mut Context<Self>) {
        self.hidden_request += 1;
        if self.visible {
            return;
        }
        let project = self
            .project_context
            .clone()
            .filter(|context| context.local_path().is_some());
        let Some(project) = project else {
            self.hidden_repo = None;
            self.retarget_watch(cx);
            return;
        };
        let known = if !self.stale
            && self.project_context.as_ref() == Some(&project)
            && !self.selected_repo.is_empty()
        {
            Some(self.selected_repo.clone())
        } else {
            REPO_CACHE
                .with(|c| {
                    c.borrow()
                        .get(&project.key())
                        .map(|(_, selected)| selected.clone())
                })
                .filter(|selected| !selected.is_empty())
        };
        self.hidden_repo = known.clone();
        self.retarget_watch(cx);
        if known.is_some() {
            return;
        }
        let req = self.hidden_request;
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    project
                        .local_path()
                        .and_then(mt_project::git::enclosing_repo)
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.hidden_request != req || this.visible {
                    return;
                }
                this.hidden_repo = found.map(|path| path.to_string_lossy().to_string());
                this.retarget_watch(cx);
            });
        })
        .detach();
    }

    /// 抽屉收着、窗口可见、工作区监听挂着时,起一个等唤醒口的任务替文件树取批次(见
    /// [`Self::feed_file_tree`]);别的情形撤掉。窗口最小化时它停着,监听照记,恢复时先把攒下
    /// 的那批取掉 —— 那批开头的一声叫在了已经撤掉的口上。
    fn sync_feeder(&mut self, cx: &mut Context<Self>) {
        let want = !self.visible && self.window_visible && self.work_watch.repo().is_some();
        if !want {
            if self._feeder.take().is_some() {
                self.work_watch.set_waker(None);
            }
            return;
        }
        if self._feeder.is_some() {
            return;
        }
        let (tx, mut rx) = mpsc::unbounded();
        self.work_watch.set_waker(Some(tx));
        self._feeder = Some(cx.spawn(async move |this, cx| {
            loop {
                // 取到没有攒着的为止(没到点就睡到点);然后等下一批开头的那一声
                loop {
                    let Ok(wait) = this.update(cx, |this: &mut Self, cx| this.feed_file_tree(cx))
                    else {
                        return;
                    };
                    let Some(wait) = wait else {
                        break;
                    };
                    cx.background_executor().timer(wait).await;
                }
                if rx.next().await.is_none() {
                    return;
                }
            }
        }));
    }

    /// 抽屉收着时工作区监听攒下的一批:只把路径交给文件树(它自己按忽略规则过、只认没在
    /// 监听的那些 —— 折起来的目录的汇总字母靠它跟上),目录出现 / 消失顺带重列清单。不重取
    /// 「更改」,也就不看 `index.lock`。返回还要等多久再来取(`None` = 没有攒着的)。
    fn feed_file_tree(&mut self, cx: &mut Context<Self>) -> Option<Duration> {
        if self.visible {
            // 抽屉开了:节拍接管
            return None;
        }
        let Some(batch) = self.work_watch.take_due() else {
            return self
                .work_watch
                .due_in()
                .map(|wait| wait.max(Duration::from_millis(10)));
        };
        git_watch::request_paths(
            git_watch::Subscriber::FileTree,
            &batch.paths,
            batch.overflow,
        );
        if batch.dirs_changed {
            self.replan_worktree(batch.last, cx);
        }
        None
    }

    /// 后台重列工作区监听清单(抽屉收着时,有目录出现 / 消失)。上一趟还没回来就把「要重列」
    /// 退回去,下一批再来。
    fn replan_worktree(&mut self, last: Instant, cx: &mut Context<Self>) {
        let Some(repo) = self.work_watch.repo().map(str::to_string) else {
            return;
        };
        if self.work_checking {
            self.work_watch.requeue(WorktreeBatch {
                paths: Vec::new(),
                overflow: false,
                dirs_changed: true,
                last,
            });
            return;
        }
        let Some(path) = self
            .project_context
            .as_ref()
            .map(|context| context.with_path(repo.clone()))
            .and_then(|context| context.local_path().map(std::path::Path::to_path_buf))
        else {
            return;
        };
        self.work_checking = true;
        cx.spawn(async move |this, cx| {
            let plan = cx
                .background_executor()
                .spawn(async move { mt_project::git::worktree_watch_dirs(&path) })
                .await;
            let _ = this.update(cx, |this: &mut Self, _cx| {
                this.work_checking = false;
                if this.work_watch.repo() == Some(repo.as_str())
                    && let Ok(plan) = plan
                {
                    this.work_watch.update_dirs(plan);
                }
            });
        })
        .detach();
    }

    /// 工作区安静下来的一批变化(`batch`),或压着等 `index.lock` 的那次重取到点复查(`None`):
    /// 后台按忽略规则过一遍(全是被忽略的路径就不刷)、看一眼锁,有目录出现 / 消失就顺带重列
    /// 监听清单;要刷的话**只**重取「更改」列表 —— 历史与分支只随 `.git` 变化走,大仓库里改个
    /// 文件不至于把整个面板拖着重来。这批之后已经有一趟取数开跑的不再要;git 正写着索引时
    /// 「更改」的重取先压着、最多等一阵,交给文件树的路径不等(见模块注释「自动刷新」)。
    fn check_worktree_change(&mut self, batch: Option<WorktreeBatch>, cx: &mut Context<Self>) {
        let Some(repo) = self.work_watch.repo().map(str::to_string) else {
            return;
        };
        if self.work_checking {
            // 上一批还没查完(节拍那边已经不在这时取,这里只是兜底):退回去下一拍再交
            if let Some(batch) = batch {
                self.work_watch.requeue(batch);
            }
            return;
        }
        let Some(path) = self
            .project_context
            .as_ref()
            .map(|context| context.with_path(repo.clone()))
            .and_then(|context| context.local_path().map(std::path::Path::to_path_buf))
        else {
            return;
        };
        self.work_checking = true;
        let git_dir = self
            .dir_watch
            .layout()
            .filter(|_| self.dir_watch.repo() == Some(repo.as_str()))
            .map(|layout| layout.git_dir);
        cx.spawn(async move |this, cx| {
            let (at, probe, checked) = cx
                .background_executor()
                .spawn(async move {
                    let at = Instant::now();
                    // git 正写着索引(长一点的变基 / 检出 / 贮藏)时工作区是中间态;锁在不在、
                    // 有多老,交给 `IndexLockGate` 定等不等
                    let probe = git_dir.as_deref().map(LockProbe::read).unwrap_or_default();
                    let checked = batch.map(|batch| {
                        let relevant = batch.overflow
                            || mt_project::git::any_unignored_change(&path, &batch.paths)
                                .unwrap_or(true);
                        let dirs = batch
                            .dirs_changed
                            .then(|| mt_project::git::worktree_watch_dirs(&path).ok())
                            .flatten();
                        (batch, relevant, dirs)
                    });
                    (at, probe, checked)
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.work_checking = false;
                if this.work_watch.repo() != Some(repo.as_str()) || this.selected_repo != repo {
                    return;
                }
                let mut last = None;
                if let Some((batch, relevant, dirs)) = checked {
                    if let Some(plan) = dirs {
                        this.work_watch.update_dirs(plan);
                    }
                    if relevant {
                        // 文件树只认它自己没在监听的那些路径(折起来的目录,汇总字母要跟上)。
                        // 不等锁:它取状态与索引锁无关,等了反倒让文件树也跟着卡住
                        git_watch::request_paths(
                            git_watch::Subscriber::FileTree,
                            &batch.paths,
                            batch.overflow,
                        );
                        last = Some(batch.last);
                    }
                }
                // 锁新就压着等它写完(到点由节拍复查),锁没了 / 太老了就取;这批之后已经有
                // 一趟开跑(`.git` 那一路刚整轮刷过之类)就不再要
                if let Some(last) = this.index_lock.offer(last, probe, at)
                    && !this.changes.read(cx).load_covers(last)
                {
                    this.changes.update(cx, |c, cx| c.load(cx));
                }
            });
        })
        .detach();
    }

    /// `.git` 下攒够一批安静下来的变化(或 pty 命中 / 重新打开 / 窗口恢复呈现叫的一次):
    /// 后台读一份 [`RepoObservation`],按「签名的先后」决定刷什么(见模块注释)。顺带重列
    /// 监听目录(新出现的分支前缀目录要补挂)。监听没挂上时什么都不做。
    fn check_git_dir_change(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo_context() else {
            return;
        };
        let layout = if repo.local_path().is_some() {
            if self.dir_watch.repo() != Some(repo.path()) {
                return;
            }
            let Some(layout) = self.dir_watch.layout() else {
                return;
            };
            Some(layout)
        } else {
            None
        };
        if self.sig_checking {
            self.sig_recheck = true;
            return;
        }
        self.sig_checking = true;
        self.sig_request += 1;
        let req = self.sig_request;
        let task_repo = repo.clone();
        cx.spawn(async move |this, cx| {
            let executor = task_repo.clone();
            let (observed, dirs) = mt_remote::run_git_on(&executor, move || {
                let at = Instant::now();
                let observed = RepoObservation {
                    at,
                    signature: task_repo.repo_signature().ok(),
                    op: task_repo.repo_operation_state().ok(),
                };
                let dirs = layout.map(|layout| mt_project::git::git_watch_dirs(&layout));
                (observed, dirs)
            })
            .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.sig_request != req || this.repo_context().as_ref() != Some(&repo) {
                    return;
                }
                this.sig_checking = false;
                if repo.is_remote()
                    && (!this.visible || !this.window_visible || observed.signature.is_none())
                {
                    // Retain the last history snapshot during hidden or failed polls.
                    // Changes loading reports transport errors without resetting history.
                    this.sig_recheck = false;
                    return;
                }
                if std::mem::take(&mut this.sig_recheck) {
                    // 期间又有变化:这份签名可能已经旧了,下一拍重查(变化时刻按现在算)
                    this.dir_watch.poke();
                }
                if let Some(dirs) = dirs {
                    this.dir_watch.update_dirs(dirs);
                }
                let known = this.round_issued.max(this.sig_observed_at);
                match check_fate(observed.at, known, this.snapshot_in_flight) {
                    // 这份读得比手上的旧(或之后已整轮重取过):没什么可补的
                    CheckFate::Covered => this.settle_pty_hit(observed.at, cx),
                    // 分支快照还在路上:等它落地拿新签名再比
                    CheckFate::Defer => this.deferred_check = Some(observed),
                    CheckFate::Compare => this.apply_observation(observed, cx),
                }
            });
        })
        .detach();
    }

    /// 拿一份读到的状态与手上的签名比,决定刷什么(见模块注释「自动刷新」)。比手上的旧就丢。
    fn apply_observation(&mut self, observed: RepoObservation, cx: &mut Context<Self>) {
        if self.sig_observed_at.is_some_and(|at| observed.at < at) {
            self.settle_pty_hit(observed.at, cx);
            return;
        }
        match sig_verdict(self.meta.signature, observed.signature) {
            // 没变:别的程序只是刷新了索引的 stat 缓存之类
            SigVerdict::Unchanged => {}
            // 只动了索引(终端里 `git add`、解决了一个冲突):重取「更改」区,冲突数(横幅)
            // 直接用读到的;文件树按签名核一遍(解决冲突后的 C 要消掉)。分支快照不必重取,
            // 理由见模块注释
            SigVerdict::IndexOnly => {
                self.meta.signature = observed.signature;
                self.sig_observed_at = Some(observed.at);
                if let Some(op) = observed.op
                    && op != self.meta.op
                {
                    self.meta.op = op;
                    cx.notify();
                }
                self.changes.update(cx, |c, cx| c.load(cx));
                git_watch::request_check(git_watch::Subscriber::FileTree);
            }
            SigVerdict::Refs => {
                self.meta.signature = observed.signature;
                self.sig_observed_at = Some(observed.at);
                self.reload_views(Round::Watch, cx);
            }
        }
        self.settle_pty_hit(observed.at, cx);
    }

    /// 走签名那条路的 pty 命中,在 `checked_at` 开始的那次检查之后收尾:签名没变、也没有
    /// 哪趟「更改」取数在命中之后开跑,就补一次只重取「更改」(子模块里的提交,见模块注释)。
    /// 命中晚于这次检查的留给下一次(poke 过,一定还有一次)。
    fn settle_pty_hit(&mut self, checked_at: Instant, cx: &mut Context<Self>) {
        let Some(hit) = self.pty_hit_at.filter(|hit| *hit <= checked_at) else {
            return;
        };
        self.pty_hit_at = None;
        if !self.changes.read(cx).load_covers(hit) {
            self.changes.update(cx, |c, cx| c.load(cx));
        }
    }

    /// 「更改」区自己的暂存 / 丢弃 / 冲突处理写完了:写完当场读的那份签名与冲突数记作
    /// 「已知」,紧接着那次 `.git` 检查比出来没变就不再刷一遍(见模块注释「签名的先后」)。
    /// 引用那半与手上的对不上(期间别处动了引用)就不收,留给检查去判。
    fn absorb_own_write(&mut self, observed: RepoObservation, cx: &mut Context<Self>) {
        if !absorbs_own_write(self.meta.signature, self.sig_observed_at, &observed) {
            return;
        }
        self.meta.signature = observed.signature;
        self.sig_observed_at = Some(observed.at);
        if let Some(op) = observed.op
            && op != self.meta.op
        {
            // 解决了一个冲突:横幅上的冲突数跟着变
            self.meta.op = op;
            cx.notify();
        }
    }
}

/// 工作区监听清单不是逐个非递归挂全的时候记一行(见 `git_dir_watch` 模块注释「递归子树」)。
fn log_worktree_plan(plan: &WorktreeWatchDirs) {
    let limit = mt_project::git::WORKTREE_WATCH_DIR_LIMIT;
    if !plan.trees.is_empty() {
        eprintln!(
            "[git] 工作区目录多于 {limit} 个,改为递归监听 {} 棵子树",
            plan.trees.len()
        );
    } else if plan.truncated {
        eprintln!(
            "[git] 工作区目录多于 {limit} 个,只监听浅层的那些,抽屉开着时深处每 {}s 兜底重取一次",
            PARTIAL_POLL.as_secs()
        );
    }
}

/// 一轮整面板重取的来由(见 [`GitPanel::reload_views`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Round {
    /// 手动 ↻:按钮转圈、历史标「刷新中…」、文件树无条件重取
    Manual,
    /// 面板 / 弹窗里的写操作做完
    Write,
    /// `.git` 监听判出引用变了:历史静默重载(没变不重画)
    Watch,
}

/// pty 嗅探命中之后走哪条路(见模块注释「自动刷新」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtyRoute {
    /// `.git` 监听挂在选中的仓库上:只叫它比一次签名,没变不刷
    Signature,
    /// 监听没挂上(列目录失败 / 正在换目标):回落原版的两个 500ms 去抖
    Debounce,
}

fn pty_route(watching_selected: bool) -> PtyRoute {
    if watching_selected {
        PtyRoute::Signature
    } else {
        PtyRoute::Debounce
    }
}

/// 两份签名的差别该刷什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SigVerdict {
    /// 没变
    Unchanged,
    /// 只有索引那半变了:只重取「更改」与冲突数
    IndexOnly,
    /// 引用变了(或有一边读不到):整轮重取
    Refs,
}

fn sig_verdict(known: Option<RepoSignature>, new: Option<RepoSignature>) -> SigVerdict {
    match (known, new) {
        (Some(old), Some(new)) if old == new => SigVerdict::Unchanged,
        (Some(old), Some(new)) if old.refs == new.refs => SigVerdict::IndexOnly,
        _ => SigVerdict::Refs,
    }
}

/// 一次检查读到的状态怎么处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckFate {
    /// 读得不比手上的新(手上那份是之后读的,或之后已经发过整轮重取):丢
    Covered,
    /// 分支快照在途:它落地时签名会换,先压着,落地后再比
    Defer,
    /// 当场比
    Compare,
}

/// `observed` = 这次检查开始读的时刻;`known` = 手上签名的读取时刻与最近一轮整轮重取的发出
/// 时刻里晚的那个。
fn check_fate(observed: Instant, known: Option<Instant>, snapshot_in_flight: bool) -> CheckFate {
    if known.is_some_and(|known| observed <= known) {
        CheckFate::Covered
    } else if snapshot_in_flight {
        CheckFate::Defer
    } else {
        CheckFate::Compare
    }
}

/// 分支快照落地时,签名与冲突数要不要沿用手上那份:手上那份是这趟快照发出**之后**才读的。
fn keep_newer_observation(observed_at: Option<Instant>, snapshot_issued: Instant) -> bool {
    observed_at.is_some_and(|at| at > snapshot_issued)
}

/// 面板自己写完索引后读的那份能不能直接记作「已知」:两边都读得到、引用那半一致(只是
/// 索引变了),且不比手上的旧。
fn absorbs_own_write(
    known: Option<RepoSignature>,
    known_at: Option<Instant>,
    observed: &RepoObservation,
) -> bool {
    let (Some(known), Some(new)) = (known, observed.signature) else {
        return false;
    };
    known.refs == new.refs && known_at.is_none_or(|at| at <= observed.at)
}

/// 仓库列表画出来取决于的那几项(`GitRepoInfo` 没有 `PartialEq`)。
fn repo_list_key(repos: &[GitRepoInfo]) -> Vec<(String, PathBuf, Option<String>, bool)> {
    repos
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                r.path.clone(),
                r.current_branch.clone(),
                r.is_worktree,
            )
        })
        .collect()
}

/// 发现仓库回来之后要不要再取一次分支快照:选中的仓库换了,或者还一份都没取过(切项目
/// 吃了仓库缓存之后的第一轮)且没有在途的那一趟。
///
/// 选中仓库没变时不重取:「刷新」本来就同时发了一趟分支快照,这里再发会把它作废(令牌
/// 前移),横幅 / 计数要等扫盘(深 5 层)做完再从头取一遍 —— 外部 `git merge --abort`
/// 后冲突横幅拖到约 3.3s 才消失(E2E r4 A11)。
///
/// `loading` 是「有快照在途」(`snapshot_in_flight`),不是刷新按钮转不转圈 —— 自动刷新
/// 不转圈,但同样在途。
fn branches_stale_after_discovery(before: &str, after: &str, empty: bool, loading: bool) -> bool {
    before != after || (empty && !loading)
}

/// detached HEAD 的显示名是带括号的短 hash(`"(1a2b3c4)"`)。
fn is_detached_label(label: &str) -> bool {
    label.starts_with('(') && label.ends_with(')')
}

/// detached 显示名里的短 hash(剥掉两侧括号)。文案自己带括号(「分离 HEAD(…)」),
/// 直接套显示名会成双层括号;不是 detached 显示名的原样返回。
pub(crate) fn detached_hash(label: &str) -> &str {
    label
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(label)
}

/// 仓库栏写按钮(Fetch / 拉取 / 推送 / 贮藏)的提示该写哪一种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteTip {
    /// 正在跑的就是它:「Fetch 中…」
    Running,
    /// 有别的写操作在跑 —— 面板自己的(推送在后台跑着),或别的区占着写操作闸:
    /// 按钮置灰,提示写「另一项 Git 操作正在进行」,与提交按钮同一句
    Waiting,
    /// 闲着:常规说明
    Normal,
}

/// `op` = 这颗按钮对应的写操作(贮藏按钮没有自己的进行中态,给 `None`)。
fn write_tip(busy: Option<GitBusy>, gated: bool, op: Option<GitBusy>) -> WriteTip {
    if busy.is_some() && busy == op {
        WriteTip::Running
    } else if busy.is_some() || gated {
        WriteTip::Waiting
    } else {
        WriteTip::Normal
    }
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
        lines.push(tr!(
            "gitBranch",
            "tipDetached",
            commit = detached_hash(branch)
        ));
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

/// 紧凑形态的 ↑↓ 计数(`↑2↓1`,见 [`SyncBadge::Compact`]):不带胶囊底,箭头与数字
/// 同色(待推送 success、待拉取 info,与两枚胶囊同一张语义表)。两项都是 0 时不画。
fn compact_sync(ahead: usize, behind: usize) -> Option<gpui::Div> {
    if ahead == 0 && behind == 0 {
        return None;
    }
    let item = |shapes: &'static [mt_ui::icons::Shape], count: usize, color: gpui::Hsla| {
        div()
            .flex()
            .items_center()
            .child(git_ui::icon(shapes, git_ui::ICON_SIZE_XS, color))
            .child(
                git_ui::mono(div())
                    .text_color(color)
                    .child(count.to_string()),
            )
    };
    Some(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(3.0))
            .px(px(4.0))
            .text_size(ui::font_px(11.0))
            .when(ahead > 0, |el| {
                el.child(item(git_ui::ICON_ARROW_UP, ahead, ui::color_success()))
            })
            .when(behind > 0, |el| {
                el.child(item(git_ui::ICON_ARROW_DOWN, behind, ui::color_info()))
            }),
    )
}

/// 计数收进 tooltip 之后([`SyncBadge::Hidden`])留的提示:分支图标右上角一个小圆点
/// (待拉取 info 优先、只有待推送 success),绝对定位、不占宽。`counts` 为 `None` 或
/// 两项都是 0 时原样返回图标。
fn sync_dot_over(icon: impl IntoElement, counts: Option<(usize, usize)>) -> AnyElement {
    let color = match counts {
        Some((_, behind)) if behind > 0 => ui::color_info(),
        Some((ahead, _)) if ahead > 0 => ui::color_success(),
        _ => return icon.into_any_element(),
    };
    div()
        .relative()
        .flex_none()
        .child(icon)
        .child(
            div()
                .absolute()
                .top(px(-2.0))
                .right(px(-3.0))
                .size(px(6.0))
                .rounded_full()
                .bg(color),
        )
        .into_any_element()
}

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

/// 两块内容区怎么分(见模块注释「两块的高度」):返回「更改」区实际占的比例。
///
/// `total` 是两块加起来的高度,`ratio` 是拖拽比例,`changes` / `history` 是两块各自
/// 「全部摊开要多高」(说不准时 `None`,当作要多少给多少)。中缝的位置
/// `b = total × ratio`,再钳进 `[min(c, total − h), max(c, total − h)]`:
///
/// - 两块合起来放得下(`c + h ≤ total`):中缝只在「两块都不用滚」的范围里随比例走,
///   哪块都不会一边滚动、一边让另一块空着;
/// - 放不下:比分到的矮的那块收成内容高度、富余全给另一块,两块都不够时照比例。
///
/// 结果对比例单调,拖中缝时跟手(到了「再拖就有一块要滚 / 留白」的边界才停)。
fn section_split(total: f32, ratio: f32, changes: Option<f32>, history: Option<f32>) -> f32 {
    if total <= 1.0 {
        return ratio;
    }
    let c = changes.unwrap_or(f32::INFINITY);
    let rest = history.map_or(f32::NEG_INFINITY, |h| total - h);
    let (lo, hi) = if c <= rest { (c, rest) } else { (rest, c) };
    (total * ratio).clamp(lo, hi).clamp(0.0, total) / total
}

/// 「更改」区不矮于 `floor`(提交区整块 + 一行列表,见 `GitChanges::min_height`):
/// 比例钳位的 15% 下限按面板总高算,面板不高时还不够放提交区,中缝拖到下限就把提交
/// 按钮裁掉、滚也滚不出来(E2E r4 B11)。面板矮到连 `floor` 都放不下时整块给「更改」,
/// 提交按钮优先于历史区。仍对比例单调。
fn keep_changes_floor(split: f32, total: f32, floor: Option<f32>) -> f32 {
    match floor {
        Some(floor) if total > 1.0 => split.max((floor / total).min(1.0)),
        _ => split,
    }
}

/// 两份「想要的高度」算不算同一份(半像素以内当没变,免得量值抖动来回重画)。
fn same_heights(a: (Option<f32>, Option<f32>), b: (Option<f32>, Option<f32>)) -> bool {
    let same = |x: Option<f32>, y: Option<f32>| match (x, y) {
        (Some(x), Some(y)) => (x - y).abs() <= 0.5,
        (None, None) => true,
        _ => false,
    };
    same(a.0, b.0) && same(a.1, b.1)
}

/// 仓库栏 ↑↓ 计数的三种形态。左段挤不下时按「截仓库名 → 截分支名 → 计数收紧 →
/// 计数收进 tooltip」的次序让位(见 [`GitPanel::render_repo_bar`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncBadge {
    /// 两枚胶囊(↑n 绿、↓m 蓝)
    Full,
    /// 一段不带底的紧凑字(`↑2↓1`)
    Compact,
    /// 不画数字(分支徽章的 tooltip 里本来就逐行写着),分支图标右上角留一个小圆点
    /// 提示「有待推送 / 待拉取」(见 [`sync_dot_over`])
    Hidden,
}

/// 仓库名截断后至少留多宽(两个字 + 省略号);名字本来更短就按名字算。默认抽屉宽度下
/// 左段只有 170px 上下,仓库 + 分支两段的图标、留白就占去八十多,保底只能给这么多。
const REPO_NAME_FLOOR: f32 = 36.0;

/// 仓库栏左段在「仓库名、分支名都截到保底」时的宽度构成。
#[derive(Debug, Clone, Copy)]
struct BarFloor {
    /// 仓库名的保底宽(见 [`REPO_NAME_FLOOR`])
    repo_name: f32,
    /// 有没有出处前缀(收到 0 之后还剩一个间距)
    detail: bool,
    worktree: bool,
    /// 分支徽章旁在转圈
    busy: bool,
    ahead: usize,
    behind: usize,
    /// 界面字号缩放(`ui::font_px(1.0)`)
    scale: f32,
}

/// 仓库触发器收到最窄时的宽:左右 6、图标 14、间距 6、(出处收到 0 还剩的)间距 6、
/// 名字保底、(worktree 图标的)间距 6 与 12、间距 6、▾ 12 加起来。各段尺寸照
/// [`GitPanel::render_repo_bar`] 的布局抄,也是触发器的 `min_w`。
fn repo_floor_width(f: &BarFloor) -> f32 {
    let detail = if f.detail { 6.0 } else { 0.0 };
    let worktree = if f.worktree { 18.0 } else { 0.0 };
    12.0 + 14.0 + 6.0 + detail + f.repo_name + worktree + 18.0
}

/// 分支徽章(不含计数)收到最窄时的宽:左右 6 + 图标 14 + 间距 6 + 名字胶囊保底 40
/// +(转圈:间距 6 + 12)。
fn branch_floor_width(f: &BarFloor) -> f32 {
    12.0 + 14.0 + 6.0 + 40.0 + if f.busy { 18.0 } else { 0.0 }
}

/// ↑↓ 计数按 `badge` 形态占多宽(含与前一项的间距)。胶囊里的数字是 11px 等宽字,
/// 一位按 0.62em 估(偏宽一点)。
fn counters_width(f: &BarFloor, badge: SyncBadge) -> f32 {
    let digits = |n: usize| n.to_string().len() as f32 * 11.0 * f.scale * 0.62;
    let counts = [f.ahead, f.behind];
    let counts = counts.iter().filter(|n| **n > 0);
    match badge {
        // 每枚:与前一项的间距 6 + 左 5 + 箭头 10 + 间距 2 + 数字 + 右 7
        SyncBadge::Full => counts.map(|&n| 6.0 + 24.0 + digits(n)).sum(),
        // 一段:间距 6 + 左右 4 + 每项(箭头 10 + 数字)+ 两项之间 3
        SyncBadge::Compact => {
            let items: Vec<f32> = counts.map(|&n| 10.0 + digits(n)).collect();
            if items.is_empty() {
                0.0
            } else {
                14.0 + items.iter().sum::<f32>() + 3.0 * (items.len() - 1) as f32
            }
        }
        SyncBadge::Hidden => 0.0,
    }
}

/// 左段放下全部内容(名字截到保底、计数按 `badge` 形态)要多宽。两段之间的间距 4。
fn bar_min_width(f: &BarFloor, badge: SyncBadge) -> f32 {
    repo_floor_width(f) + 4.0 + branch_floor_width(f) + counters_width(f, badge)
}

/// 左段宽 `width` 时计数取哪种形态。`width` 还没量出来(0)时先按完整的画。
fn sync_badge_for(width: f32, floor: &BarFloor) -> SyncBadge {
    if width <= 0.0 || width >= bar_min_width(floor, SyncBadge::Full) {
        SyncBadge::Full
    } else if width >= bar_min_width(floor, SyncBadge::Compact) {
        SyncBadge::Compact
    } else {
        SyncBadge::Hidden
    }
}

/// 左段连「名字截到保底、计数收进 tooltip」都放不下(抽屉拖到最窄)时的让位,接在
/// [`SyncBadge`] 三档之后:先把仓库触发器收成只剩文件夹图标,再把分支徽章也收成只剩
/// 分支图标。两个入口始终露着、点得到,名字都在各自的 tooltip 里 —— 此前最窄 240 时
/// 仓库触发器按保底宽占满左段,分支徽章被整个裁掉,分支弹窗没了入口(E2E r4 A1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarSqueeze {
    /// 照常(名字截断到保底)
    None,
    /// 仓库触发器只留文件夹图标
    RepoIcon,
    /// 仓库触发器、分支徽章都只留图标(徽章转圈时圈顶替图标)
    Icons,
}

/// 收成图标的仓库触发器 / 分支徽章的宽:左右 6 + 图标 14。
const ICON_ONLY_WIDTH: f32 = 26.0;

/// 左段宽 `width` 时取哪一档让位。`width` 还没量出来(0)时先按照常画。
fn bar_squeeze_for(width: f32, floor: &BarFloor) -> BarSqueeze {
    if width <= 0.0 || width >= bar_min_width(floor, SyncBadge::Hidden) {
        BarSqueeze::None
    } else if width >= ICON_ONLY_WIDTH + 4.0 + branch_floor_width(floor) {
        BarSqueeze::RepoIcon
    } else {
        BarSqueeze::Icons
    }
}

/// 仓库路径的展示形态:本机原生分隔符、不带尾部分隔符。libgit2 的 workdir 是
/// `D:/x/proj/`,扫盘找到的子仓库是 `D:\x\proj\sub` —— 不归一的话同一处 tooltip
/// 两种写法。
fn native_path_display(path: &str) -> String {
    let trimmed = mt_core::path_key::trim_trailing_separators(path);
    let native = if cfg!(windows) {
        trimmed.replace('/', "\\")
    } else {
        trimmed.to_string()
    };
    // 盘符根(`D:`)要留住分隔符,否则是「D 盘的当前目录」
    if native.len() == 2 && native.ends_with(':') {
        format!("{native}{}", std::path::MAIN_SEPARATOR)
    } else {
        native
    }
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
        let broken_remote = self.store.read(cx).active_project().is_some_and(|project| {
            project.ssh_connection_id.is_some()
                && self
                    .store
                    .read(cx)
                    .remote_connection_of(&project.id)
                    .is_none()
        });
        if broken_remote {
            return div().size_full().child(centered_hint(
                git_ui::ICON_REPO,
                t("gitHistory", "connectionRemoved"),
            ));
        }
        if self.remote_git_disabled(cx) {
            let project_id = self
                .store
                .read(cx)
                .active_project_id
                .clone()
                .unwrap_or_default();
            let recovery = self.recovery_controls(cx);
            if recovery.is_none() && self.visible && self.enable_prompted.insert(project_id.clone())
            {
                let store = self.store.clone();
                _window.defer(cx, move |window, cx| {
                    crate::git_backend::request_enable(store, project_id, window, cx)
                });
            }
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(12.0))
                .child(ui::desc_text(t("gitHistory", "remoteDisabled")))
                .children(recovery)
                .child(
                    crate::prompt::footer_button(
                        "enable-remote-git",
                        t("gitHistory", "enableEntry"),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(id) = this.store.read(cx).active_project_id.clone() {
                            crate::git_backend::request_enable(this.store.clone(), id, window, cx);
                        }
                    })),
                );
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
        // 两块都展开时按内容让位(见模块注释「两块的高度」)。两块「想要的高度」都是
        // 上一帧量的;canvas 发现变了会再要一帧。「更改」区的列表高度读它的派生模型
        // (缓存的,不再每帧建树),一帧只问一次、下限跟着同一个值算
        let (changes_grow, history_grow) = if both_open {
            let changes = self.changes.read(cx);
            let changes_natural = changes.natural_height(cx);
            let changes_floor = changes.min_height(changes_natural);
            self.natural_seen = (changes_natural, self.history.read(cx).natural_height());
            let split = section_split(
                self.sections_height,
                section.ratio,
                self.natural_seen.0,
                self.natural_seen.1,
            );
            let split = keep_changes_floor(split, self.sections_height, changes_floor);
            self.effective_ratio = split;
            (split, 1.0 - split)
        } else {
            self.effective_ratio = section.ratio;
            (changes_grow, history_grow)
        };
        let section_box = |grow_by: f32| {
            grow(
                div().flex_basis(px(0.0)).min_h(px(0.0)).overflow_hidden(),
                grow_by,
            )
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
        if let Some(error) = &self.load_error {
            root = root.child(git_ui::error_block(error.clone()));
        }
        if let Some(recovery) = self.recovery_controls(cx) {
            root = root.child(recovery);
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
            .child(section_box(changes_grow).child(self.changes.clone()));

        let history_header = section_header(
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
        );
        // 中缝手柄与「提交历史」标题包在一起、画在标题**之后**(见 `render_section_handle`)
        let handle = both_open.then(|| self.render_section_handle(cx));
        root = root
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(history_header)
                    .children(handle),
            )
            .child(section_box(history_grow).child(self.history.clone()))
            // 量一次两块内容区加起来的高度:中缝拖拽要按它换算比例。顺带看两块「想要的
            // 高度」与这一帧用的是不是同一份 —— 内容变了(多了一行变更、历史翻了页)
            // 就再要一帧,按新值重新分
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, window, cx| {
                        let stale = this.update(cx, |panel: &mut GitPanel, cx| {
                            // 整块面板的高度减掉固定件(仓库栏 + 横幅 + 两个 header 30)
                            let fixed = if panel.repos.is_empty() {
                                60.0
                            } else if panel.meta.op.kind.is_some() {
                                60.0 + BAR_HEIGHT * 2.0
                            } else {
                                60.0 + BAR_HEIGHT
                            };
                            let total = (f32::from(bounds.size.height) - fixed).max(1.0);
                            let resized = (total - panel.sections_height).abs() > 0.5;
                            panel.sections_height = total;
                            let natural = (
                                panel.changes.read(cx).natural_height(cx),
                                panel.history.read(cx).natural_height(),
                            );
                            let both_open = SECTION_UI.with(|s| {
                                let s = s.borrow();
                                s.changes_open && s.history_open
                            });
                            both_open && (resized || !same_heights(natural, panel.natural_seen))
                        });
                        if stale {
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );

        // 拖拽期间鼠标可能划出手柄,移动/松手挂在面板根上
        root.when(self.drag.is_some(), |el| {
            el.on_mouse_move(
                cx.listener(|this: &mut Self, event: &MouseMoveEvent, _window, cx| {
                    let Some(drag) = this.drag else { return };
                    if drag.total <= 0.0 {
                        return;
                    }
                    let dy = f32::from(event.position.y - drag.start_y);
                    let next = clamp_ratio(drag.start_ratio + dy / drag.total);
                    // 比例真变了才重画:钳到上下限之后鼠标接着动,每一下都是一次白画的整窗帧
                    let moved = SECTION_UI.with(|s| {
                        let mut s = s.borrow_mut();
                        std::mem::replace(&mut s.ratio, next) != next
                    });
                    if moved {
                        cx.notify();
                    }
                }),
            )
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
    /// 中缝拖拽手柄(`GitHistory.tsx:516-523`):骑在「提交历史」标题上沿的 6px 绝对定位条
    /// (上下各 3px)。
    ///
    /// ⚠️ 它必须是标题容器里**排在标题之后**的子元素:下半截压在标题上,gpui 的冒泡
    /// 按绘制顺序倒着派,后画的先拿到。原先手柄先画、标题后画 —— 按下时标题先记下一次
    /// 待定点击(手柄的 `stop_propagation` 轮到时已经晚了),拖拽时标题跟着光标走,松手
    /// 仍在标题上,于是「提交历史」被折叠(E2E 截到过)。现在手柄先拿到按下、就地拦住。
    ///
    /// 不用 `occlude` / `block_mouse_except_scroll` 挡身后的命中:拖拽期间光标一直骑在
    /// 手柄上,面板根上那对移动 / 松手监听要「根被悬停」才触发,挡掉就拖不动、也松不开。
    fn render_section_handle(&self, cx: &mut Context<Self>) -> AnyElement {
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
                    // 从中缝**眼下**的位置起算:按内容让过位时它不在 `ratio` 那儿,
                    // 按 `ratio` 起算的话一按就跳
                    let ratio = clamp_ratio(this.effective_ratio);
                    SECTION_UI.with(|s| s.borrow_mut().ratio = ratio);
                    this.drag = Some(SectionDrag {
                        start_y: event.position.y,
                        start_ratio: ratio,
                        total: this.sections_height,
                    });
                }),
            )
            .into_any_element()
    }

    /// 左段的保底宽度构成(计数胶囊该取哪种形态的判据,见 [`sync_badge_for`])。
    fn bar_floor(&self) -> BarFloor {
        let info = self.selected_repo_info();
        let scale = f32::from(ui::font_px(1.0));
        let name = info.map(|r| r.name.as_str()).unwrap_or_default();
        let viewing =
            self.view_branch.is_some() && self.view_branch.as_deref() != self.current_branch();
        let tracking = if viewing {
            None
        } else {
            self.meta.head_tracking.as_ref()
        };
        BarFloor {
            // 估宽了只是计数早一点收紧:左段本身 `overflow_hidden`,怎么估都不会压到按钮组
            repo_name: REPO_NAME_FLOOR.min(estimate_text_width(name, 13.0 * scale)),
            detail: info.is_some_and(|r| {
                repo_detail(&r.path.to_string_lossy(), self.project_path.as_deref()).is_some()
            }),
            worktree: info.is_some_and(|r| r.is_worktree),
            busy: self.busy.is_some_and(GitBusy::touches_branch),
            ahead: tracking.map_or(0, |t| t.ahead),
            behind: tracking.map_or(0, |t| t.behind),
            scale,
        }
    }

    /// 仓库栏(`GitHistory.tsx:353-499`),重排成清晰的两段:左段仓库 + 分支,
    /// 右段图标按钮组。
    ///
    /// **按钮组永不让位、左段永不压到它上面**:左段 `flex_1 + min_w(0) + overflow_hidden`,
    /// 窄下去时按次序让位 ——
    ///
    /// 1. 仓库出处(父级路径)、仓库名先截(收缩权重远大于分支徽章;仓库名截到
    ///    [`REPO_NAME_FLOOR`] 为止);
    /// 2. 再截分支名(名字胶囊保底 40px,见 `git_ui::name_pill`);
    /// 3. 还放不下:↑↓ 计数从两枚胶囊收成一段紧凑字,再不行就不画(分支徽章的 tooltip
    ///    里逐行写着)。这一档靠 canvas 量左段宽度、按 [`bar_min_width`] 判,量值换档时
    ///    再画一帧;
    /// 4. 抽屉拖到最窄还放不下:仓库触发器收成只剩图标,再不行分支徽章也只剩图标
    ///    ([`BarSqueeze`]),两个入口都点得到。
    ///
    /// 两段的最小宽都**显式**给(名字截到保底之后的宽,见 [`repo_floor_width`] /
    /// [`branch_floor_width`]):自动最小宽按的是名字的完整宽度(截断只管溢出怎么画,
    /// 不改内容宽),仓库名会一个字都不让、全挤到分支名上;给 0 又会缩得比内容还窄。
    fn render_repo_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let floor = self.bar_floor();
        self.sync_badge = sync_badge_for(self.bar_left_width, &floor);
        self.bar_squeeze = bar_squeeze_for(self.bar_left_width, &floor);
        let repo_icon_only = self.bar_squeeze != BarSqueeze::None;
        let info = self.selected_repo_info();
        let repo_name = info.map(|r| r.name.clone()).unwrap_or_default();
        let is_worktree = info.is_some_and(|r| r.is_worktree);
        // 选中的那个也只画叶子名,回头再看仍然认不出是哪个 `api` —— 名字前面
        // 补一段弱化的父级路径(与下拉里第二行同一个判据),完整路径挂 tooltip
        let detail =
            info.and_then(|r| repo_detail(&r.path.to_string_lossy(), self.project_path.as_deref()));
        let repo_path_tip: SharedString = native_path_display(&self.selected_repo).into();

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
            // 先于分支徽章收缩(权重 × 基准宽分摊,权重悬殊 = 几乎只缩它),缩到保底为止
            .map(|el| shrink(el, 1000.0))
            .min_w(px(if repo_icon_only {
                ICON_ONLY_WIDTH
            } else {
                repo_floor_width(&floor)
            }))
            .hover(|el| el.bg(git_ui::button_hover_bg()))
            .child(git_ui::icon(
                git_ui::ICON_REPO,
                git_ui::ICON_SIZE,
                ui::color_folder(),
            ))
            // 只剩图标时名字、出处、▾ 都不画(完整路径在 tooltip 里)
            .when(!repo_icon_only, |el| {
                el
                    // 父级路径先让位:栏子窄下去时先截它,仓库名与分支徽章保住
                    .when_some(detail, |el, detail| {
                        el.child(
                            div()
                                .min_w(px(0.0))
                                .max_w(px(90.0))
                                .map(|el| shrink(el, 1000.0))
                                .text_size(ui::font_px(12.0))
                                .text_color(ui::text_muted())
                                .truncate()
                                .child(format!("{detail}/")),
                        )
                    })
                    .child(
                        div()
                            .min_w(px(floor.repo_name))
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
            })
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

        // 量左段的宽:它是 flex_1 + basis 0,宽度只随栏宽与按钮组变,与里面画什么无关,
        // 按它换计数形态不会来回抖
        let this = cx.entity();
        let measure = canvas(
            move |bounds: Bounds<Pixels>, window, cx| {
                let stale = this.update(cx, |panel: &mut GitPanel, _cx| {
                    panel.bar_left_width = f32::from(bounds.size.width);
                    let floor = panel.bar_floor();
                    sync_badge_for(panel.bar_left_width, &floor) != panel.sync_badge
                        || bar_squeeze_for(panel.bar_left_width, &floor) != panel.bar_squeeze
                });
                if stale {
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let left = div()
            .relative()
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .items_center()
            .gap(px(4.0))
            .child(repo_trigger)
            .children(self.render_branch_badge(cx))
            .child(measure);

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
    /// 宽度**随分支名自适应**:栏子挤不下时,仓库名先截到保底,再轮到分支名(胶囊自己
    /// 截断,保底 40px),最后计数收紧 / 收进 tooltip —— 次序见 [`Self::render_repo_bar`]。
    /// 截断时全名在 tooltip 里。
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
        let full = self.sync_badge == SyncBadge::Full;
        let floor = self.bar_floor();
        // 最窄一档只留图标(名字、计数都在 tooltip 里),转圈时圈顶替图标
        let icon_only = self.bar_squeeze == BarSqueeze::Icons;
        let min_width = if icon_only {
            ICON_ONLY_WIDTH
        } else {
            branch_floor_width(&floor) + counters_width(&floor, self.sync_badge)
        };
        let lead_icon = if icon_only && busy {
            ui::spinner(px(git_ui::ICON_SIZE), ui::accent()).into_any_element()
        } else {
            sync_dot_over(
                git_ui::icon(lead, git_ui::ICON_SIZE, lead_color),
                (self.sync_badge == SyncBadge::Hidden).then_some((ahead, behind)),
            )
        };

        Some(
            div()
                .id("git-branch-badge")
                .flex()
                .items_center()
                .gap(px(6.0))
                .h(px(26.0))
                .px(px(6.0))
                .rounded(px(4.0))
                // 仓库触发器缩到保底之后才轮到它;只缩分支名,计数按档整枚出现 / 收起
                .min_w(px(min_width))
                .cursor_pointer()
                .hover(|el| el.bg(git_ui::button_hover_bg()))
                .child(lead_icon)
                .when(!icon_only, |el| {
                    el.child(git_ui::name_pill(
                        "git-branch-badge-name",
                        branch,
                        pill_color,
                        None,
                    ))
                    .when(full && ahead > 0, |el| el.child(git_ui::ahead_pill(ahead)))
                    .when(full && behind > 0, |el| {
                        el.child(git_ui::behind_pill(behind))
                    })
                    .when(self.sync_badge == SyncBadge::Compact, |el| {
                        el.children(compact_sync(ahead, behind))
                    })
                    .when(busy, |el| {
                        el.child(ui::spinner(px(git_ui::ICON_SIZE_SM), ui::accent()))
                    })
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
        // 有写操作在跑(面板自己的,或别的区占着写操作闸):其余写按钮一律禁用,提示口径
        // 见 [`write_tip`](见模块注释「写操作」)
        let gated = self.gated();
        let idle = busy.is_none() && !gated;
        let is = |op: GitBusy| busy == Some(op);
        let blocked = |op: GitBusy| (busy.is_some() && busy != Some(op)) || gated;
        let head = self.head_branch().map(str::to_string);
        let detached = self.current_branch().is_some_and(is_detached_label);
        let upstream = self.meta.head_tracking.as_ref().map(|t| t.upstream.clone());
        let mode = self.pull_mode_label();
        let tip = |op: Option<GitBusy>, normal: &dyn Fn() -> SharedString| -> SharedString {
            match write_tip(busy, gated, op) {
                WriteTip::Running => {
                    tr!("gitBranch", "busyTip", op = op.map_or("", GitBusy::label)).into()
                }
                WriteTip::Waiting => git_ui::gate_busy_text().into(),
                WriteTip::Normal => normal(),
            }
        };

        let fetch_tip = tip(Some(GitBusy::Fetch), &|| t("gitBranch", "fetchTip").into());
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

        let pull_tip = tip(Some(GitBusy::Pull), &|| {
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
        });
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

        let push_tip = tip(Some(GitBusy::Push), &|| {
            if detached {
                return t("gitBranch", "pushTipDetached").into();
            }
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
        });
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
        let stash_tip = tip(None, &|| {
            if stash_count > 0 {
                tr!("gitStash", "countTip", count = stash_count).into()
            } else {
                t("gitStash", "title").into()
            }
        });
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

        // 横幅底已经染过一层 attention,冲突数胶囊的字色按染过的底算对比度
        let banner_bg = ui::with_alpha(color, 0.12);
        let status = if conflicted > 0 {
            git_ui::pill_on(
                tr!("gitBranch", "conflictCount", count = conflicted),
                color,
                git_ui::ground_with(banner_bg),
            )
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
                .bg(banner_bg)
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
        // 显示的是「点了会切到」的那种视图(提交详情 / 提交 diff 左栏用同一颗)
        git_ui::view_mode_toggle("git-changes-view-mode", tree_mode)
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
        let project = self.project_context.clone();
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
                let project = project.clone();
                item.on_click(move |_window, cx| {
                    let path = path.clone();
                    this.update(cx, |this, cx| {
                        if this.project_context == project {
                            this.select_repo(path, cx);
                        }
                    });
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
        let project_id = self.project_id.clone();
        let project_path = self.project_path.clone().unwrap_or_default();
        let repo_path = repo.path.to_string_lossy().to_string();
        // 项目根仓库不带 cwd 覆盖(默认就是项目根)
        let same_as_root = if self
            .project_context
            .as_ref()
            .is_some_and(GitContext::is_remote)
        {
            repo_path.trim_end_matches('/') == project_path.trim_end_matches('/')
        } else {
            is_project_root_repo(&repo_path, &project_path)
        };
        let title = if repo.is_worktree {
            format!("⎇ {}", repo.current_branch.clone().unwrap_or(repo.name.clone()))
        } else {
            repo.name.clone()
        };
        let this = cx.entity();
        let Some(repo_for_worktree) = self.repo_context() else {
            return Vec::new();
        };
        let worktree_project_id = project_id.clone();
        let worktree_project = self.project_context.clone();

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
                    let project = worktree_project.clone();
                    crate::git_worktree::open(
                        // 单仓库模式:discover_repos = false
                        repo_for_worktree.clone(),
                        false,
                        worktree_project_id.clone(),
                        move |cx| {
                            this.update(cx, |this, cx| {
                                if this.project_context == project {
                                    this.load_repos(cx);
                                }
                            });
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
///   会向上找到它)→ 仓库自己的绝对路径(本机原生分隔符,与仓库栏 tooltip 同一写法,
///   见 [`native_path_display`]),免得只剩一个没有出处的叶子名。
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
        None => Some(native_path_display(&repo)),
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
            Some(native_path_display("D:/Git/mini-term"))
        );
        // 前缀只是「字符串像」不算在项目内:mono2 不是 mono 的子目录
        assert_eq!(
            repo_detail(r"D:\Git\mono2\api", Some(r"D:\Git\mono")),
            Some(native_path_display("D:/Git/mono2/api"))
        );
        // 还没挂上项目路径时同理(空路径不能当前缀,否则谁都算项目内)
        assert_eq!(
            repo_detail(r"D:\Git\mono\api", None),
            Some(native_path_display("D:/Git/mono/api"))
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

    /// 游离 HEAD 的说明文案自己带括号:套进去的只能是剥掉括号的哈希,否则成
    /// 「（(1a2b3c4)）」(E2E r4 A6)。两种语言都只剩一层括号。
    #[test]
    fn 游离提示只有一层括号() {
        assert_eq!(detached_hash("(1a2b3c4)"), "1a2b3c4");
        assert_eq!(
            detached_hash("main"),
            "main",
            "不是 detached 显示名的原样返回"
        );
        assert_eq!(detached_hash("(wip"), "(wip");
        for locale in crate::i18n::Locale::ALL {
            for key in ["detachedNote", "tipDetached"] {
                let template = mt_i18n::lookup(locale, "gitBranch", key).expect("文案在");
                let text =
                    mt_i18n::interpolate(template, &[("commit", detached_hash("(1a2b3c4)"))]);
                assert!(text.contains("1a2b3c4"), "{text}");
                assert!(
                    !text.contains("((") && !text.contains("（(") && !text.contains(")）"),
                    "{locale}.{key}: {text}"
                );
            }
        }
        let tip = branch_tip_lines("(1a2b3c4)", None, true, false);
        assert!(!tip[0].contains('('), "{tip:?}");
    }

    /// 发现仓库回来:选中仓库没变、分支快照已有或在途时不再重取 —— 重取会作废「刷新」
    /// 同时发出的那一趟,横幅要等扫盘做完再从头取(E2E r4 A11 横幅 3.3s 才消失)。
    #[test]
    fn 发现仓库后不作废在途的分支快照() {
        // 刷新:选中没变,分支快照在途 / 已到
        assert!(!branches_stale_after_discovery("D:/r", "D:/r", false, true));
        assert!(!branches_stale_after_discovery("D:/r", "D:/r", true, true));
        assert!(!branches_stale_after_discovery(
            "D:/r", "D:/r", false, false
        ));
        // 切项目吃了缓存:选中没变,但一份都没取过
        assert!(branches_stale_after_discovery("D:/r", "D:/r", true, false));
        // 选中仓库换了(原先的不见了 / 第一次发现)
        assert!(branches_stale_after_discovery("", "D:/r", true, false));
        assert!(branches_stale_after_discovery("D:/a", "D:/b", false, true));
    }

    fn sig(refs: u64, index: u64) -> RepoSignature {
        RepoSignature { refs, index }
    }

    fn after(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// pty 命中:`.git` 监听挂在选中仓库上才走签名,否则回落两个去抖。
    #[test]
    fn pty_命中挂着监听时改走签名() {
        assert_eq!(pty_route(true), PtyRoute::Signature);
        assert_eq!(pty_route(false), PtyRoute::Debounce);
    }

    /// 签名的差别:全同不刷;只有索引变了走轻量那条(不重取分支快照);引用变了或读不到
    /// 一边整轮刷。
    #[test]
    fn 只动了索引走轻量路径() {
        let s = sig(1, 10);
        assert_eq!(sig_verdict(Some(s), Some(s)), SigVerdict::Unchanged);
        assert_eq!(
            sig_verdict(Some(s), Some(sig(1, 11))),
            SigVerdict::IndexOnly
        );
        assert_eq!(sig_verdict(Some(s), Some(sig(2, 10))), SigVerdict::Refs);
        assert_eq!(sig_verdict(Some(s), Some(sig(2, 11))), SigVerdict::Refs);
        assert_eq!(sig_verdict(None, Some(s)), SigVerdict::Refs, "还没有快照");
        assert_eq!(sig_verdict(Some(s), None), SigVerdict::Refs, "读不到当变了");
    }

    /// 检查结果的处置:不比手上的新就丢;快照在途就压着等它落地;否则当场比。
    #[test]
    fn 检查结果按先后处置() {
        let t0 = Instant::now();
        // 从没有过已知:在途就压着,不在途就比
        assert_eq!(check_fate(t0, None, false), CheckFate::Compare);
        assert_eq!(check_fate(t0, None, true), CheckFate::Defer);
        // 手上那份(或最近一轮整轮重取)晚于这次检查开读:丢,即便快照在途
        assert_eq!(
            check_fate(after(t0, 10), Some(after(t0, 20)), true),
            CheckFate::Covered
        );
        assert_eq!(
            check_fate(after(t0, 20), Some(after(t0, 20)), false),
            CheckFate::Covered,
            "同一刻:整轮重取读到的不早于它"
        );
        // 检查更新:写完操作后刷新已发出、快照还没回来 —— 压着,别当场判「变了」整轮再刷
        assert_eq!(check_fate(after(t0, 500), Some(t0), true), CheckFate::Defer);
        assert_eq!(
            check_fate(after(t0, 500), Some(t0), false),
            CheckFate::Compare
        );
    }

    /// 快照落地:手上那份签名是它发出之后读的才沿用(面板刚写完索引 / 检查刚比过)。
    #[test]
    fn 快照落地不拿旧签名盖新的() {
        let t0 = Instant::now();
        assert!(!keep_newer_observation(None, t0));
        assert!(
            !keep_newer_observation(Some(t0), after(t0, 1)),
            "手上的更旧"
        );
        assert!(!keep_newer_observation(Some(t0), t0), "同一刻按快照的");
        assert!(keep_newer_observation(Some(after(t0, 5)), t0));
    }

    /// 面板自己写完索引:引用那半一致、且不比手上的旧才收;读不到不收。
    #[test]
    fn 自己写完索引记作已知() {
        let t0 = Instant::now();
        let observed = |at: Instant, s: Option<RepoSignature>| RepoObservation {
            at,
            signature: s,
            op: None,
        };
        let known = Some(sig(1, 10));
        // 常态:暂存了一个文件,只有索引变了
        assert!(absorbs_own_write(
            known,
            Some(t0),
            &observed(after(t0, 5), Some(sig(1, 11)))
        ));
        assert!(absorbs_own_write(
            known,
            None,
            &observed(t0, Some(sig(1, 11)))
        ));
        // 期间别处动了引用:留给检查去判(要整轮刷)
        assert!(!absorbs_own_write(
            known,
            Some(t0),
            &observed(after(t0, 5), Some(sig(2, 11)))
        ));
        // 手上那份是之后读的:不拿旧的盖
        assert!(!absorbs_own_write(
            known,
            Some(after(t0, 10)),
            &observed(after(t0, 5), Some(sig(1, 11)))
        ));
        // 任一边读不到
        assert!(!absorbs_own_write(
            None,
            None,
            &observed(t0, Some(sig(1, 11)))
        ));
        assert!(!absorbs_own_write(
            known,
            Some(t0),
            &observed(after(t0, 5), None)
        ));
    }

    /// 端到端推一遍「面板里暂存一次」:写完读的签名被收下之后,紧接着那次 `.git` 检查
    /// 读到的同一份签名判成没变 —— 「更改」不再重取第二遍、分支快照一次都不取。
    #[test]
    fn 面板暂存后紧接着的检查短路() {
        let t0 = Instant::now();
        let before = sig(1, 10);
        let written = sig(1, 11);
        // 已知:上一份快照(t0 发出)
        let mut known = Some(before);
        let mut known_at = Some(t0);
        // 写完那一刻(t0+100)读的
        let own = RepoObservation {
            at: after(t0, 100),
            signature: Some(written),
            op: None,
        };
        assert!(absorbs_own_write(known, known_at, &own));
        known = own.signature;
        known_at = Some(own.at);
        // 500ms 去抖后的检查(t0+600):读到的还是这份
        let check_at = after(t0, 600);
        assert_eq!(check_fate(check_at, known_at, false), CheckFate::Compare);
        assert_eq!(sig_verdict(known, Some(written)), SigVerdict::Unchanged);
    }

    /// 仓库栏写按钮的提示:面板自己在推送时(没占别人的闸,`gated = false`),其余按钮
    /// 同样写「另一项 Git 操作正在进行」(E2E r4 A10 只有提交按钮写了);正在跑的那颗写
    /// 「…中」;别的区占着闸时一律等;闲着照常。
    #[test]
    fn 写按钮忙时都说明原因() {
        use GitBusy::*;
        let push = Some(Push);
        assert_eq!(write_tip(push, false, Some(Push)), WriteTip::Running);
        for op in [Some(Fetch), Some(Pull), None] {
            assert_eq!(write_tip(push, false, op), WriteTip::Waiting, "{op:?}");
            assert_eq!(write_tip(None, true, op), WriteTip::Waiting, "{op:?}");
            assert_eq!(write_tip(None, false, op), WriteTip::Normal, "{op:?}");
        }
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

    /// 「更改」与「提交历史」分高度:放得下时中缝只在两块都不用滚的范围里随比例走;
    /// 放不下时矮的那块收成内容高度、富余给另一块;说不准的一块当作要多少给多少。
    #[test]
    fn 两块高度按内容让位() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        // conflict-demo 那一例:更改要 600、历史只要 244,共 900,比例 0.5 ——
        // 历史让出富余,更改整块摊开不必再滚(中缝挪到 600)
        assert!(close(
            section_split(900.0, 0.5, Some(600.0), Some(244.0)),
            600.0 / 900.0
        ));
        // 反过来:更改只要 200、历史要 800 —— 更改让出富余(中缝挪到 200)
        assert!(close(
            section_split(900.0, 0.5, Some(200.0), Some(800.0)),
            200.0 / 900.0
        ));
        // 更改很少(空态 160)、历史还有下一页(说不准):更改收成内容,其余给历史
        assert!(close(
            section_split(900.0, 0.5, Some(160.0), None),
            160.0 / 900.0
        ));
        // 两块都放得下:比例落在 [c, total - h] 里就照比例
        assert!(close(
            section_split(900.0, 0.5, Some(300.0), Some(200.0)),
            0.5
        ));
        // 两块都放不下:照比例
        assert!(close(
            section_split(900.0, 0.4, Some(2000.0), Some(3000.0)),
            0.4
        ));
        assert!(close(section_split(900.0, 0.4, None, None), 0.4));
        // 还没量出总高:照比例
        assert!(close(section_split(0.0, 0.3, Some(10.0), Some(10.0)), 0.3));

        // 拖中缝时对比例单调(跟手,不会往回跳)
        let mut last = 0.0;
        for step in 0..=100 {
            let ratio = step as f32 / 100.0;
            let split = section_split(900.0, ratio, Some(600.0), Some(244.0));
            assert!(split + 1e-6 >= last, "ratio {ratio}: {split} < {last}");
            last = split;
        }
    }

    /// 中缝拖到 15% 下限时「更改」区仍不矮于提交区 + 一行列表(E2E r4 B11:总高 700、
    /// 提交区约 190,15% 只有 105,提交按钮被裁掉)。放得下时不改比例;面板矮到放不下
    /// 时整块给「更改」;还没量出来时照旧。
    #[test]
    fn 更改区不矮于提交区() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        let floor = Some(234.0);
        // 每个允许的比例下,「更改」区的高度都 ≥ floor,且对比例单调
        let mut last = 0.0;
        for step in 15..=85 {
            let ratio = clamp_ratio(step as f32 / 100.0);
            let split = keep_changes_floor(
                section_split(700.0, ratio, Some(900.0), Some(1200.0)),
                700.0,
                floor,
            );
            assert!(split * 700.0 + 1e-3 >= 234.0, "ratio {ratio}: {split}");
            assert!(split + 1e-6 >= last, "ratio {ratio}: {split} < {last}");
            last = split;
        }
        // 比例本来就够:原样
        assert!(close(keep_changes_floor(0.5, 700.0, floor), 0.5));
        // 面板矮到放不下 floor:整块给「更改」
        assert!(close(keep_changes_floor(0.15, 200.0, floor), 1.0));
        // 还没量出来
        assert!(close(keep_changes_floor(0.15, 700.0, None), 0.15));
        assert!(close(keep_changes_floor(0.3, 0.0, floor), 0.3));
    }

    /// 仓库栏计数让位:宽度够放两枚胶囊就画胶囊;不够先收成紧凑字,再不够不画。
    /// 门槛是「仓库名、分支名都截到保底之后」还要的宽。
    #[test]
    fn 计数胶囊按宽度逐档收() {
        let floor = BarFloor {
            repo_name: REPO_NAME_FLOOR,
            detail: false,
            worktree: false,
            busy: false,
            ahead: 2,
            behind: 1,
            scale: 1.0,
        };
        let full = bar_min_width(&floor, SyncBadge::Full);
        let compact = bar_min_width(&floor, SyncBadge::Compact);
        let hidden = bar_min_width(&floor, SyncBadge::Hidden);
        assert!(
            hidden < compact && compact < full,
            "{hidden} {compact} {full}"
        );
        assert_eq!(sync_badge_for(full + 20.0, &floor), SyncBadge::Full);
        assert_eq!(sync_badge_for(full - 1.0, &floor), SyncBadge::Compact);
        assert_eq!(sync_badge_for(compact - 1.0, &floor), SyncBadge::Hidden);
        // 还没量出来(0)先按完整的画
        assert_eq!(sync_badge_for(0.0, &floor), SyncBadge::Full);
        // 没有计数时三档一样宽
        let quiet = BarFloor {
            ahead: 0,
            behind: 0,
            ..floor
        };
        assert_eq!(
            bar_min_width(&quiet, SyncBadge::Full),
            bar_min_width(&quiet, SyncBadge::Hidden)
        );
        // 仓库名本来就短:保底按名字估,不硬撑到 48
        assert!(estimate_text_width("ui", 13.0) < REPO_NAME_FLOOR);
        assert!(estimate_text_width("中文仓库", 13.0) > estimate_text_width("abcd", 13.0));
    }

    /// 抽屉拖到最窄(240,左段只剩约 67px:按钮组 5 颗 + 贮藏数胶囊占去其余)时左段逐档
    /// 让位:先把仓库触发器收成图标,再把分支徽章也收成图标 —— 分支弹窗的入口始终在
    /// (E2E r4 A1:最窄时分支胶囊被整个裁掉)。每一档的内容都放得下左段。
    #[test]
    fn 最窄时分支入口仍在() {
        let floor = BarFloor {
            repo_name: REPO_NAME_FLOOR,
            detail: false,
            worktree: false,
            busy: false,
            ahead: 2,
            behind: 1,
            scale: 1.0,
        };
        let hidden = bar_min_width(&floor, SyncBadge::Hidden);
        assert_eq!(bar_squeeze_for(0.0, &floor), BarSqueeze::None, "还没量出来");
        assert_eq!(bar_squeeze_for(hidden, &floor), BarSqueeze::None);
        let repo_icon = ICON_ONLY_WIDTH + 4.0 + branch_floor_width(&floor);
        assert!(repo_icon < hidden);
        assert_eq!(bar_squeeze_for(hidden - 1.0, &floor), BarSqueeze::RepoIcon);
        assert_eq!(bar_squeeze_for(repo_icon, &floor), BarSqueeze::RepoIcon);
        assert_eq!(bar_squeeze_for(repo_icon - 1.0, &floor), BarSqueeze::Icons);
        // 最窄 240 的左段:两颗图标(26 + 间距 4 + 26)放得下
        let narrowest = 67.0;
        assert_eq!(bar_squeeze_for(narrowest, &floor), BarSqueeze::Icons);
        assert!(ICON_ONLY_WIDTH * 2.0 + 4.0 <= narrowest);
    }

    /// 仓库路径 tooltip:本机原生分隔符、不带尾部分隔符(libgit2 给的 `D:/x/proj/`
    /// 与扫盘给的 `D:\x\proj\sub` 写成同一种样子)。
    #[test]
    fn 仓库路径按本机写法显示() {
        if cfg!(windows) {
            assert_eq!(
                native_path_display("D:/Git/playground/"),
                r"D:\Git\playground"
            );
            assert_eq!(native_path_display(r"D:\Git\a\sub\"), r"D:\Git\a\sub");
            assert_eq!(native_path_display("D:/"), r"D:\");
        } else {
            assert_eq!(native_path_display("/home/u/proj/"), "/home/u/proj");
        }
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
