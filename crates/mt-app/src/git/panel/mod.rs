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
//! (都在 [`crate::git::dir_watch`]):AI agent 在终端里提交、外部工具切分支、AI / 外部
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
//!   重取先压着、隔一阵复查,锁老过 [`crate::git::dir_watch::INDEX_LOCK_PATIENCE`](开着编辑器
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
//!
//! # 折叠动画
//!
//! 两块的内容区本来就是 `flex-basis: 0` + `overflow: hidden` 的竖向裁剪窗,折叠 / 展开只是
//! 把 flex-grow 在 0 与分到的份额之间切。点标题那一刻记下屏上的两块 grow,之后
//! [`SECTION_TOGGLE`](mt_ui::motion::SECTION_TOGGLE) 时长里从它补到目标([`blend_grows`]);
//! 目标照旧每帧按上面的规则现算(内容让位、拖拽比例都不受影响),动画期间只是不直接用它。
//! canvas 量的是整块面板的高度,与两块怎么分无关,不会跟补间互相拉扯。标题上的箭头同步
//! 转过去(`fold_motion`)。
//!
//! 进行中操作横幅(①')出现 / 消失时高度裁剪展开 / 收起(`height_reveal`),两块可分配的
//! 高度按它这一帧的插值高度扣。

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

use crate::fold_motion::{FlipTweens, OPEN_TURNS};
use crate::git::changes::{GitChanges, GitChangesEvent};
use crate::git::dir_watch::{
    GitDirWatch, IndexLockGate, LockProbe, RepoObservation, WorktreeBatch, WorktreeWatch,
};
use crate::git::history::{CommitDetailPane, GitHistoryContent, GitHistoryEvent};
use crate::git::ui::{self as git_ui, Tone, estimate_text_width};
use crate::git::watch as git_watch;
use crate::height_reveal::HeightReveal;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuItem};
use crate::prompt::Confirm;
use crate::store::{AppStore, StoreEvent};
use crate::ui;

mod layout;
mod remote_recovery;
mod toolbar;
mod watch;
mod write_ops;

pub use layout::clamp_ratio;
use layout::*;
use toolbar::*;
use watch::*;
pub(crate) use watch::{detached_hash, op_noun};

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
    /// 抽屉收起期间收到过仅影响渲染的 store 事件 → 打开时补刷。
    dirty_while_hidden: bool,
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
    /// 两块折叠区标题上的箭头:只让刚点的那一块转(见 `fold_motion`)。
    chevrons: FlipTweens,
    /// 区块折叠 / 展开的 flex-grow 补间(见模块注释「折叠动画」):点标题时重播。
    grow_anim: mt_ui::motion::Transition,
    /// 点标题那一刻屏上的两块 grow(补间起点)。
    grow_from: (f32, f32),
    /// 上一帧实际画的两块 grow。
    grow_shown: (f32, f32),
    /// 进行中操作横幅的高度裁剪展开 / 收起。
    banner: HeightReveal,
    /// 横幅最近一次画的操作 —— 收起动画里 `meta.op` 已经清了,照它画旧的那条。
    banner_op: RepoOpState,
    /// 上一帧看横幅时的「仓库 + 数据到没到」:换了仓库或数据刚到就直接到位、不播。
    banner_seen: Option<(String, bool)>,
    /// 这一帧横幅占多高(canvas 换算两块可分配高度用,与画出来的是同一个数)。
    banner_h: f32,
    _tick: Option<Task<()>>,
    _subs: Vec<Subscription>,
}
pub(crate) fn current_repo_for_project(store: &AppStore, project_id: &str) -> Option<GitContext> {
    let project = crate::git::backend::project_context(store, project_id);
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
    let key = crate::git::backend::project_context(store, project_id).key();
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
    let key = crate::git::backend::project_context(store, id).key();
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
            .map(|project| crate::git::backend::project_context(store.read(cx), &project.id));
        let project_path = project_context
            .as_ref()
            .map(|context| context.path().to_string());
        let project_id = store.read(cx).active_project_id.clone();
        let enable_prompted = project_id
            .iter()
            .filter(|id| crate::git::backend::remote_git_enabled(store.read(cx), id))
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
                    crate::git::push::open(cx.entity(), context.clone(), None, window, cx);
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
        // 仅订阅本面板渲染依赖的 store 事件;抽屉关闭时不触发空重绘,等展开再补刷。
        subs.push(
            cx.subscribe(&store, |this: &mut Self, _, event: &StoreEvent, cx| {
                if !event.touches_git_panel() {
                    return;
                }
                if this.visible {
                    cx.notify();
                } else {
                    this.dirty_while_hidden = true;
                }
            }),
        );
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
                            cx.notify();
                        } else {
                            this.dirty_while_hidden = true;
                        }
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
                    if this.visible {
                        cx.notify();
                    } else {
                        this.dirty_while_hidden = true;
                    }
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
            dirty_while_hidden: false,
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
            chevrons: FlipTweens::new(mt_ui::motion::CHEVRON_ROTATE),
            grow_anim: mt_ui::motion::Transition::settled(mt_ui::motion::SECTION_TOGGLE),
            grow_from: (0.5, 0.5),
            grow_shown: (0.5, 0.5),
            banner: HeightReveal::new(false),
            banner_op: RepoOpState::default(),
            banner_seen: None,
            banner_h: 0.0,
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
        if visible {
            self.dirty_while_hidden = false;
        }
        // 抽屉重新打开:横幅按眼下的状态直接到位,不在抽屉滑进来的同时再长一遍
        self.banner_seen = None;
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
            .map(|project| crate::git::backend::project_context(store, &project.id))
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
                && !crate::git::backend::remote_git_enabled(store, &project.id)
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
                    Err(err) => this.repo_discovery_failed(&err, cx),
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

    fn repo_discovery_failed(&mut self, err: &anyhow::Error, cx: &mut Context<Self>) {
        tracing::warn!("[git] 发现仓库失败: {err:#}");
        if !self.local_project() {
            self.load_error = Some(git_ui::error_text_for_store(err, &self.store, cx));
        }
        self.repos.clear();
        self.selected_repo.clear();
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
                        tracing::warn!("[git] 取分支失败: {err:#}");
                        if !this.local_project() {
                            let error = git_ui::error_text_for_store(&err, &this.store, cx);
                            changed |= this.load_error.as_ref() != Some(&error);
                            this.load_error = Some(error);
                        }
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
}

impl Render for GitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                window.defer(cx, move |window, cx| {
                    crate::git::backend::request_enable(store, project_id, window, cx)
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
                            crate::git::backend::request_enable(this.store.clone(), id, window, cx);
                        }
                    })),
                );
        }

        let section = SECTION_UI.with(|s| *s.borrow());
        let both_open = section.changes_open && section.history_open;
        let changes_grow = if section.changes_open {
            if section.history_open {
                section.ratio
            } else {
                1.0
            }
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
        // 折叠 / 展开的补间(见模块注释「折叠动画」):动画期间从点击那一刻的屏上值补到目标
        let folding = self.grow_anim.running();
        let fold_p = self.grow_anim.drive(window);
        let (changes_grow, history_grow) = if folding {
            blend_grows(self.grow_from, (changes_grow, history_grow), fold_p)
        } else {
            (changes_grow, history_grow)
        };
        self.grow_shown = (changes_grow, history_grow);
        self.chevrons.drive(window);
        let changes_turns =
            self.chevrons
                .value_open("git-section-changes", section.changes_open, OPEN_TURNS);
        let history_turns =
            self.chevrons
                .value_open("git-section-history", section.history_open, OPEN_TURNS);
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
            self.sync_banner();
            if self.banner.is_mounted()
                && let Some(banner) = self.render_op_banner(self.banner_op, cx)
            {
                root = root.child(self.banner.render(banner, window));
            }
            self.banner_h = self.banner.shown_height(BAR_HEIGHT);
        } else {
            self.banner_h = 0.0;
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
                changes_turns,
                false,
                Some(self.render_view_mode_toggle(cx)),
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    let open = SECTION_UI.with(|s| {
                        let mut s = s.borrow_mut();
                        s.changes_open = !s.changes_open;
                        s.changes_open
                    });
                    this.start_section_toggle("git-section-changes", open);
                    cx.notify();
                }),
            ))
            .child(section_box(changes_grow).child(self.changes.clone()));

        let history_header = section_header(
            "git-section-history",
            t("panels", "history"),
            section.history_open,
            history_turns,
            true,
            None,
            cx.listener(|this, _: &ClickEvent, _window, cx| {
                let open = SECTION_UI.with(|s| {
                    let mut s = s.borrow_mut();
                    s.history_open = !s.history_open;
                    s.history_open
                });
                this.start_section_toggle("git-section-history", open);
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
                            // 整块面板的高度减掉固定件(仓库栏 + 横幅 + 两个 header 30)。
                            // 横幅开合时按它这一帧的插值高度扣
                            let fixed = if panel.repos.is_empty() {
                                60.0
                            } else {
                                60.0 + BAR_HEIGHT + panel.banner_h
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

#[cfg(test)]
mod tests;
