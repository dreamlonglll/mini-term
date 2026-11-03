//! Git 面板的 `.git` 目录监听:AI agent 在终端里提交、外部工具改分支之后面板自己刷新。
//!
//! # 为什么还要这一路
//!
//! [`crate::git_watch`] 只嗅 PTY 输出(且排除 AI pane),而 AI 在终端里提交、IDE 的
//! git 插件切分支、命令行里 `git rebase` 停在冲突上 —— 这些都不经过它。这里直接盯
//! 仓库的 git 目录:HEAD / index / refs / packed-refs / FETCH_HEAD / ORIG_HEAD /
//! MERGE_HEAD / rebase-merge 等的出现、消失与改写。
//!
//! # 复用进程级单例
//!
//! 监听走 [`mt_project::watch::FsWatcher`] —— 全进程一个 notify watcher,这里只是一个
//! 订阅者句柄(drop 即撤销名下全部监听)。它是**非递归**的,`refs/` 下逐层的目录由
//! [`mt_project::git::git_watch_dirs`] 列出来;新出现的分支前缀目录(`feature/`)在下一
//! 轮检查时补挂。
//!
//! # 分工
//!
//! - notify 线程上的 sink 只做一件事:按 [`is_relevant_git_change`] 过滤(`*.lock`、
//!   objects / logs 噪声丢掉,只留贮藏的 reflog —— 删 `stash@{n>0}` 时只有它变),
//!   命中就把「最近一次相关变化」的时刻记下;
//! - 主线程借 Git 面板现成的 100ms 节拍问 [`GitDirWatch::take_due`]:安静满去抖窗口了
//!   才报一次,由面板去后台算签名、对比、决定刷什么(见 `git_panel.rs`)。
//!
//! 只在面板可见时挂监听(收起即 [`GitDirWatch::clear`]),与 `set_visible` 那道扫盘闸
//! 同一口径。
//!
//! # 工作区监听([`WorktreeWatch`])
//!
//! `.git` 只在提交 / 暂存 / 切分支时变;AI agent、外部编辑器、文件查看器保存改的是
//! **工作区文件**,索引纹丝不动,上面那一路听不到(终端嗅探还排除了 AI pane)——
//! 「更改」列表就停在旧样子,文件树却已标上 M。于是另挂一路盯工作区:
//!
//! - 目录清单由 [`mt_project::git::worktree_watch_dirs`] 在后台列:工作区根 + 没被忽略的
//!   子目录,广度优先,跳过 `.git`、嵌套仓库、`node_modules` / `target`;非递归监听一个
//!   目录一条,最多 [`mt_project::git::WORKTREE_WATCH_DIR_LIMIT`] 条(Windows 上每条约
//!   16 KB 缓冲,封顶约 4 MB)。目录更多时见下面「递归子树」;
//! - 同一目录文件树也在听时共用后端那一条,不重复注册(递归的那些另走一个后端,与文件树
//!   的非递归监听互不干扰,见 `mt_project::watch` 模块注释「递归监听」);
//! - sink 只记路径与时刻(`.git` 下的直接丢),并在「目录出现 / 消失」时标记要重列清单;
//! - sink 只做常数开销的事:先只按路径过滤,「是不是目录」这一次 stat 在**锁外**做
//!   (`\\wsl.localhost` 上一次 stat 是毫秒级的 9P 往返,主线程每 100ms 的 `take_due` 要拿
//!   同一把锁),且只对建 / 删 / 改名这几种事件做;
//! - 主线程借面板节拍问 [`WorktreeWatch::take_due`]:安静满 [`DEBOUNCE`](或一直有变化、
//!   攒满 [`WORKTREE_MAX_WAIT`])才交出一批,面板在后台按忽略规则过一遍
//!   ([`mt_project::git::any_unignored_change`]),有一条没被忽略才**只重取「更改」列表**
//!   (历史 / 分支不动,大仓库不跟着抖),并把这批路径交给文件树。上一批还在后台查时面板
//!   不来取(取出来也只能原样退回);git 正写着索引(`index.lock` 在)时「更改」的重取先压着,
//!   等它写完再取,但最多等 [`INDEX_LOCK_PATIENCE`](见 [`IndexLockGate`]);交给文件树的
//!   那份不等锁;
//! - Git 抽屉收着时面板不跑节拍,改由 [`WorktreeWatch::set_waker`] 登记的唤醒口叫醒:一批
//!   开头叫一声,面板按 [`WorktreeWatch::due_in`] 睡到点再取,只把路径交给文件树
//!   (见 `git_panel.rs` 模块注释「抽屉与窗口」)。
//!
//! ## 递归子树
//!
//! bigwt 那种上千个目录的工作区一目录一条挂不下,只挂浅层的话深处的改动永远不自动出现。
//! 后端原生支持子树监听的平台(Windows / macOS,[`mt_project::git::NATIVE_TREE_WATCH`])
//! 改成:工作区根非递归、每个顶层目录递归一条(顶层目录太多就整根递归一条)—— 顶层的
//! `target/` / `node_modules/` 不在任何一条递归里,构建、装依赖的洪水进不来。子树里仍会有
//! 被忽略的目录、子包的依赖目录,清单里列着它们(`WorktreeWatchDirs::pruned`),sink 把落在
//! 它们**里面**的事件直接丢掉(名字是 `node_modules` / `target` 的按名字认)。
//!
//! notify 7 的 Windows 后端在 ReadDirectoryChangesW 缓冲溢出时什么都不报(返回 0 字节就
//! 当没事),丢掉的变化里可能夹着真改动。递归子树更容易被洪水灌满,所以 sink 数着丢掉的
//! 事件,一批里多到 [`FLOOD_DROPPED`] 条就按溢出交 —— 但等洪水安静满去抖才交,装依赖期间
//! 不每 2s 整批刷一次。
//!
//! Linux 的 inotify 没有子树监听,只能逐目录挂到上限(那边放宽到 1024 条);再多的话
//! [`WorktreeWatch::is_partial`] 为真,面板开着时低频兜底重取。
//!
//! 防自激:刷新链路全是只读(git2 的 status 不回写索引,读文件不产生写事件),面板自己
//! 不会把自己叫醒;面板里的丢弃 / 按块丢弃会改工作区,顺带再取一次列表,无害。
//!
//! # 签名检查读什么([`RepoObservation`])
//!
//! `.git` 那一路安静下来之后,面板在后台读一份 [`RepoObservation`]:签名 + 进行中操作 /
//! 冲突数,连同**开始读的时刻**。时刻是用来排先后的:面板手上的签名可能来自一份还没落地
//! 的分支快照、面板自己刚写完索引时读的那份、或者更早的一次检查,谁新听谁的(规则在
//! `git_panel.rs`)。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures::channel::mpsc;
use mt_project::git::{
    GitDirLayout, RepoOpState, RepoSignature, WORKTREE_SKIP_DIRS, WorktreeWatchDirs,
    is_relevant_git_change,
};
use mt_project::watch::{FsWatcher, WatchReady};
use parking_lot::Mutex;

/// 去抖窗口:最后一次相关变化之后安静这么久才刷。一次 `git commit` 会在几十毫秒里
/// 连写 index / 对象 / 引用 / reflog,一次 rebase 更多 —— 等它写完再看。
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// 某一刻读到的仓库「面板可见状态」(见模块注释「签名检查读什么」)。
#[derive(Debug, Clone, Copy)]
pub struct RepoObservation {
    /// 开始读的时刻:读到的状态不早于它。
    pub at: Instant,
    /// 读不到(仓库被删 / 正被别的进程改写)为 `None`。
    pub signature: Option<RepoSignature>,
    /// 进行中的合并 / 变基… 与冲突数(横幅);读不到为 `None`。
    pub op: Option<RepoOpState>,
}

impl RepoObservation {
    /// **阻塞**(两次 git2 只读,不写 `.git`),调用方放后台执行器。
    pub fn read(repo: &Path) -> Self {
        let at = Instant::now();
        Self {
            at,
            signature: mt_project::git::repo_signature(repo).ok(),
            op: mt_project::git::repo_operation_state(repo).ok(),
        }
    }
}

/// `old` → `new` 的差额:要撤掉的、要新挂的(各自保持 `new` / `old` 里的先后)。
/// 两边都有上限(`WORKTREE_WATCH_DIR_LIMIT`,Linux 上 1024),主线程上不必 N² 地逐个 `contains`。
fn dir_diff(old: &[PathBuf], new: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let old_set: HashSet<&PathBuf> = old.iter().collect();
    let new_set: HashSet<&PathBuf> = new.iter().collect();
    let removed = old
        .iter()
        .filter(|d| !new_set.contains(d))
        .cloned()
        .collect();
    let added = new
        .iter()
        .filter(|d| !old_set.contains(d))
        .cloned()
        .collect();
    (removed, added)
}

/// sink 与主线程共享的那一小块。
#[derive(Default)]
struct Shared {
    /// 当前监听目标的布局(过滤要用)。`None` = 没在监听。
    layout: Option<GitDirLayout>,
    /// 最近一次相关变化的时刻;被 [`GitDirWatch::take_due`] 取走后清空。
    last_change: Option<Instant>,
}

/// 当前挂着的目标。
struct Target {
    repo: String,
    layout: GitDirLayout,
    dirs: Vec<PathBuf>,
}

pub struct GitDirWatch {
    watcher: FsWatcher,
    shared: Arc<Mutex<Shared>>,
    target: Option<Target>,
}

impl GitDirWatch {
    pub fn new() -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let sink = shared.clone();
        let watcher = FsWatcher::with_label("git-panel", move |change| {
            // notify 线程:只过滤 + 记时刻,不碰 GPUI
            let mut shared = sink.lock();
            let relevant = shared
                .layout
                .as_ref()
                .is_some_and(|layout| is_relevant_git_change(layout, &change.path));
            if relevant {
                shared.last_change = Some(Instant::now());
            }
        });
        Self {
            watcher,
            shared,
            target: None,
        }
    }

    /// 正在监听的仓库路径。
    pub fn repo(&self) -> Option<&str> {
        self.target.as_ref().map(|t| t.repo.as_str())
    }

    /// 当前目标的布局(检查时重列监听目录要用)。
    pub fn layout(&self) -> Option<GitDirLayout> {
        self.target.as_ref().map(|t| t.layout.clone())
    }

    /// 换监听目标(切仓库 / 切项目 / 面板重新可见)。旧目标的目录全部撤掉。
    ///
    /// 返回各目录的注册回执(监听在后台挂上,单测要等它落定再动文件)。
    pub fn set_target(
        &mut self,
        repo: String,
        layout: GitDirLayout,
        dirs: Vec<PathBuf>,
    ) -> Vec<WatchReady> {
        self.clear();
        let ready = dirs
            .iter()
            .map(|dir| self.watch_dir(&layout, dir))
            .collect();
        let mut shared = self.shared.lock();
        shared.layout = Some(layout.clone());
        shared.last_change = None;
        drop(shared);
        self.target = Some(Target { repo, layout, dirs });
        ready
    }

    /// 同一目标下监听目录集合变了(新出现 / 被删的分支前缀目录):只增删差额。
    pub fn update_dirs(&mut self, dirs: Vec<PathBuf>) {
        let Some(target) = self.target.as_mut() else {
            return;
        };
        if target.dirs == dirs {
            return;
        }
        let (removed, added) = dir_diff(&target.dirs, &dirs);
        for old in &removed {
            self.watcher.unwatch(old);
        }
        let layout = target.layout.clone();
        target.dirs = dirs;
        for dir in &added {
            self.watch_dir(&layout, dir);
        }
    }

    fn watch_dir(&self, layout: &GitDirLayout, dir: &Path) -> WatchReady {
        // 监听器要求路径在「项目根」内(纵深防御),这里的根就是 common 目录:
        // git_dir(含 linked worktree 的 `.git/worktrees/<名>`)与 refs 都在它下面
        let root = layout.common_dir.to_string_lossy().to_string();
        self.watcher.watch(dir, &root)
    }

    /// 撤掉全部监听(面板收起 / 远程项目 / 没有仓库)。
    pub fn clear(&mut self) {
        if let Some(target) = self.target.take() {
            for dir in &target.dirs {
                self.watcher.unwatch(dir);
            }
        }
        let mut shared = self.shared.lock();
        shared.layout = None;
        shared.last_change = None;
    }

    /// 手动记一笔「刚有变化」(签名检查在跑期间又来了一批时,让它回来后再查一轮)。
    pub fn poke(&mut self) {
        if self.target.is_some() {
            self.shared.lock().last_change = Some(Instant::now());
        }
    }

    /// 有没有一批「已经安静满 [`DEBOUNCE`]」的相关变化。取走即清,一批只报一次。
    pub fn take_due(&mut self) -> bool {
        let mut shared = self.shared.lock();
        match shared.last_change {
            Some(at) if at.elapsed() >= DEBOUNCE => {
                shared.last_change = None;
                true
            }
            _ => false,
        }
    }
}

// ─── 工作区监听 ───────────────────────────────────────────────

/// 一直有变化(AI 连着写文件)时,最多攒这么久就报一批 —— 只按「安静满去抖」报的话,
/// 列表会在整段生成期间一动不动。
pub const WORKTREE_MAX_WAIT: Duration = Duration::from_secs(2);

/// 一批最多记多少条路径;再多只记「溢出」(溢出按相关算,不再逐条过忽略规则)。
const PENDING_CAP: usize = 256;

/// 一批之内因为落在被跳过的目录里而丢掉的事件到这么多条,就当监听缓冲可能溢出过(见模块
/// 注释「递归子树」):这批按溢出交,但要等这股洪水安静满 [`DEBOUNCE`] 才交 —— 装依赖、
/// 构建期间不至于每 [`WORKTREE_MAX_WAIT`] 整批刷一次。一块 16 KB 的缓冲装一两百条,
/// 上千条已是装依赖 / 构建那个量级。
const FLOOD_DROPPED: usize = 1000;

/// sink 与主线程共享的那一小块。
#[derive(Default)]
struct WorkShared {
    /// 工作区根(过滤 `.git` 要用)。`None` = 没在监听
    root: Option<PathBuf>,
    /// 换一次目标 +1:sink 分两段拿锁(stat 在锁外),回来对不上号的那条丢掉
    generation: u64,
    /// 挂着监听的目录(非递归的与递归子树根):它们自己被删 / 改名时要重列清单
    dirs: HashSet<PathBuf>,
    /// 递归子树里被跳过的目录(`WorktreeWatchDirs::pruned`):落在它们里面的事件直接丢
    pruned: HashSet<PathBuf>,
    pending: HashSet<PathBuf>,
    overflow: bool,
    dirs_dirty: bool,
    /// 这一批第一条 / 最近一条记下的变化的时刻
    first: Option<Instant>,
    last: Option<Instant>,
    /// 这一批里丢掉的事件数与最后一条的时刻(见 [`FLOOD_DROPPED`])
    dropped: usize,
    dropped_last: Option<Instant>,
    /// 一批开了头就叫一声(见 [`WorktreeWatch::set_waker`]);叫过之后这批交出去才复位
    waker: Option<mpsc::UnboundedSender<()>>,
    wake_sent: bool,
}

impl WorkShared {
    fn reset_batch(&mut self) {
        self.pending.clear();
        self.overflow = false;
        self.dirs_dirty = false;
        self.first = None;
        self.last = None;
        self.dropped = 0;
        self.dropped_last = None;
        self.wake_sent = false;
    }

    fn flooded(&self) -> bool {
        self.dropped >= FLOOD_DROPPED
    }

    /// 这一批还要等多久才交(见 [`batch_wait`])。
    fn wait(&self, now: Instant) -> Option<Duration> {
        let flood = self.dropped_last.filter(|_| self.flooded());
        batch_wait(self.first, self.last, flood, now)
    }

    /// 这一批开了头:叫醒消费方,一批只叫一次。
    fn wake(&mut self) {
        if !self.wake_sent
            && let Some(waker) = &self.waker
        {
            self.wake_sent = true;
            let _ = waker.unbounded_send(());
        }
    }

    fn record(&mut self, path: PathBuf, now: Instant) {
        if self.pending.len() < PENDING_CAP {
            self.pending.insert(path);
        } else {
            self.overflow = true;
        }
        self.first.get_or_insert(now);
        self.last = Some(now);
        self.wake();
    }
}

/// 一批什么时候该交:记下了路径的,安静满 [`DEBOUNCE`] 或攒满 [`WORKTREE_MAX_WAIT`];只有
/// 「丢得太多」(`flood` = 那股洪水最后一条的时刻)的,等洪水安静满去抖。返回还要等多久
/// (`Some(0)` = 现在就交),什么都没攒为 `None`。
fn batch_wait(
    first: Option<Instant>,
    last: Option<Instant>,
    flood: Option<Instant>,
    now: Instant,
) -> Option<Duration> {
    match (first, last) {
        (Some(first), Some(last)) => {
            let quiet = DEBOUNCE.saturating_sub(now.saturating_duration_since(last));
            let cap = WORKTREE_MAX_WAIT.saturating_sub(now.saturating_duration_since(first));
            Some(quiet.min(cap))
        }
        _ => flood.map(|at| DEBOUNCE.saturating_sub(now.saturating_duration_since(at))),
    }
}

/// 一批安静下来的工作区变化。
#[derive(Debug)]
pub struct WorktreeBatch {
    pub paths: Vec<PathBuf>,
    /// 路径太多没记全,或丢的事件多到缓冲可能溢出过:当相关处理
    pub overflow: bool,
    /// 有目录出现 / 消失:监听目录清单要重列
    pub dirs_changed: bool,
    /// 这一批最后一条变化记下的时刻(sink 收到事件时;文件真正被写不晚于它)。在它之后才
    /// 开跑的取数一定反映得出这一批。
    pub last: Instant,
}

/// 一条变化是不是 git 自己的目录里的(`<root>/.git/…`,linked worktree 的 `.git` 文件同理)。
fn is_git_internal(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(|rel| rel.components().next())
        .is_some_and(|first| first.as_os_str() == ".git")
}

/// 这条变化是不是落在被跳过的目录**里面**(递归子树里被忽略的、嵌套仓库、依赖与构建产物,
/// 见 `WorktreeWatchDirs::pruned`)。目录自己那一条不算 —— 它出现 / 消失照常要看。名字在
/// `WORKTREE_SKIP_DIRS` 里的按名字认:新冒出来的(子包里刚装的 `node_modules`)还没进清单。
/// 只看字符串,不碰盘。
fn inside_pruned(root: &Path, pruned: &HashSet<PathBuf>, path: &Path) -> bool {
    // 根自己、根外的都不归它管(根的祖先叫 `target` 也不算)
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    if rel.as_os_str().is_empty() {
        return false;
    }
    path.ancestors()
        .skip(1)
        .take_while(|dir| *dir != root)
        .any(|dir| {
            pruned.contains(dir)
                || dir
                    .file_name()
                    .is_some_and(|name| WORKTREE_SKIP_DIRS.iter().any(|skip| name == *skip))
        })
}

/// 这条事件是不是「某一项出现 / 消失 / 改名」(notify 的 kind 是 `EventKind` 的调试串,
/// `Create(..)` / `Remove(..)` / `Modify(Name(..))`)。只看字符串,不碰盘。
fn is_structural(kind: &str) -> bool {
    kind.starts_with("Create") || kind.starts_with("Remove") || kind.contains("Name")
}

/// 这条变化会不会改变「该挂哪些目录」:目录被建 / 删 / 改名。`is_dir_now` 是调用方在
/// **锁外** stat 的结果;被删 / 改走的目录已经 stat 不到了,只能靠「它在不在清单里」认。
fn reshapes_dirs(structural: bool, is_dir_now: bool, watched: bool) -> bool {
    structural && (watched || is_dir_now)
}

/// 当前挂着的目标。
struct WorkTarget {
    repo: String,
    /// 非递归的(工作区根在第一个)与递归的
    dirs: Vec<PathBuf>,
    trees: Vec<PathBuf>,
    /// 深处没挂上(见 `WorktreeWatchDirs::truncated`)
    partial: bool,
}

/// 工作区监听(见模块注释「工作区监听」)。
pub struct WorktreeWatch {
    watcher: FsWatcher,
    shared: Arc<Mutex<WorkShared>>,
    target: Option<WorkTarget>,
}

impl WorktreeWatch {
    pub fn new() -> Self {
        let shared = Arc::new(Mutex::new(WorkShared::default()));
        let sink = shared.clone();
        let watcher = FsWatcher::with_label("git-worktree", move |change| {
            // notify 线程:只过滤 + 记账,不碰 GPUI。先按路径过滤(递归子树里被跳过的目录、
            // `.git`),再在锁外 stat「是不是目录」—— 主线程每拍的 `take_due` 等的是同一把锁,
            // 网络盘上一次 stat 就是一次往返;装依赖的洪水在第一段就丢掉,不碰盘
            let structural = is_structural(&change.kind);
            let generation = {
                let mut shared = sink.lock();
                let Some(root) = shared.root.as_deref() else {
                    return;
                };
                if is_git_internal(root, &change.path) {
                    return;
                }
                // 后端叫「这棵子树自己重扫一遍」(macOS FSEvents 合并 / 丢了事件时的
                // MustScanSubDirs,notify 报成 `Other`):整批按溢出算
                if change.kind == "Other" {
                    shared.overflow = true;
                    shared.record(change.path, Instant::now());
                    return;
                }
                if inside_pruned(root, &shared.pruned, &change.path) {
                    shared.dropped += 1;
                    shared.dropped_last = Some(Instant::now());
                    if shared.flooded() {
                        shared.wake();
                    }
                    return;
                }
                // 挂着监听的目录自己「被修改」(Windows 上它的子项变了,父目录那条会报):它
                // 里面的变化由它自己那条监听报,这条只是噪声
                if !structural && shared.dirs.contains(&change.path) {
                    return;
                }
                shared.generation
            };
            let is_dir_now = structural && change.path.is_dir();
            let mut shared = sink.lock();
            if shared.generation != generation {
                return;
            }
            if reshapes_dirs(structural, is_dir_now, shared.dirs.contains(&change.path)) {
                shared.dirs_dirty = true;
            }
            shared.record(change.path, Instant::now());
        });
        Self {
            watcher,
            shared,
            target: None,
        }
    }

    /// 正在监听的仓库路径。
    pub fn repo(&self) -> Option<&str> {
        self.target.as_ref().map(|t| t.repo.as_str())
    }

    /// 深处没挂上(目录太多又没法递归,见 `WorktreeWatchDirs::truncated`):面板要低频兜底。
    pub fn is_partial(&self) -> bool {
        self.target.as_ref().is_some_and(|t| t.partial)
    }

    /// 换监听目标。`plan.dirs` 第一项是工作区根(见 `worktree_watch_dirs`)。返回各目录的
    /// 注册回执。
    pub fn set_target(&mut self, repo: String, plan: WorktreeWatchDirs) -> Vec<WatchReady> {
        self.clear();
        let Some(root) = plan.dirs.first().cloned() else {
            return Vec::new();
        };
        let project = root.to_string_lossy().to_string();
        let mut ready: Vec<WatchReady> = plan
            .dirs
            .iter()
            .map(|dir| self.watcher.watch(dir, &project))
            .collect();
        ready.extend(
            plan.trees
                .iter()
                .map(|dir| self.watcher.watch_tree(dir, &project)),
        );
        let mut shared = self.shared.lock();
        shared.root = Some(root);
        shared.generation += 1;
        shared.dirs = plan.dirs.iter().chain(&plan.trees).cloned().collect();
        shared.pruned = plan.pruned.into_iter().collect();
        shared.reset_batch();
        drop(shared);
        self.target = Some(WorkTarget {
            repo,
            dirs: plan.dirs,
            trees: plan.trees,
            partial: plan.truncated,
        });
        ready
    }

    /// 同一目标下目录清单变了:两种方式各自只增删差额。
    pub fn update_dirs(&mut self, plan: WorktreeWatchDirs) {
        let Some(target) = self.target.as_mut() else {
            return;
        };
        let Some(root) = target.dirs.first().cloned() else {
            return;
        };
        let project = root.to_string_lossy().to_string();
        if target.dirs != plan.dirs {
            let (removed, added) = dir_diff(&target.dirs, &plan.dirs);
            for old in &removed {
                self.watcher.unwatch(old);
            }
            for dir in &added {
                self.watcher.watch(dir, &project);
            }
        }
        if target.trees != plan.trees {
            let (removed, added) = dir_diff(&target.trees, &plan.trees);
            for old in &removed {
                self.watcher.unwatch_tree(old);
            }
            for dir in &added {
                self.watcher.watch_tree(dir, &project);
            }
        }
        let mut shared = self.shared.lock();
        shared.dirs = plan.dirs.iter().chain(&plan.trees).cloned().collect();
        shared.pruned = plan.pruned.into_iter().collect();
        drop(shared);
        target.dirs = plan.dirs;
        target.trees = plan.trees;
        target.partial = plan.truncated;
    }

    /// 撤掉全部监听(面板收起 / 远程项目 / 没有仓库)。
    pub fn clear(&mut self) {
        if let Some(target) = self.target.take() {
            for dir in &target.dirs {
                self.watcher.unwatch(dir);
            }
            for dir in &target.trees {
                self.watcher.unwatch_tree(dir);
            }
        }
        let mut shared = self.shared.lock();
        shared.root = None;
        shared.generation += 1;
        shared.dirs.clear();
        shared.pruned.clear();
        shared.reset_batch();
    }

    /// 登记 / 撤掉唤醒口:此后每一批开头往口里丢一声(一批一声,交出去才复位)。Git 抽屉
    /// 收着、面板不跑节拍时替文件树服务用(见 `git_panel.rs` 模块注释「抽屉与窗口」)。
    /// 换口时这批还没叫过。
    pub fn set_waker(&mut self, waker: Option<mpsc::UnboundedSender<()>>) {
        let mut shared = self.shared.lock();
        shared.waker = waker;
        shared.wake_sent = false;
    }

    /// 攒着的这一批还要等多久才交(`Some(0)` = 现在就交);什么都没攒为 `None`。
    pub fn due_in(&self) -> Option<Duration> {
        self.shared.lock().wait(Instant::now())
    }

    /// 有没有一批该交出去的变化:记下了路径的,安静满 [`DEBOUNCE`] 或这一批已经攒了
    /// [`WORKTREE_MAX_WAIT`];只丢不记的洪水等它安静下来(见 [`FLOOD_DROPPED`])。取走即清。
    pub fn take_due(&mut self) -> Option<WorktreeBatch> {
        let now = Instant::now();
        let mut shared = self.shared.lock();
        if !shared.wait(now)?.is_zero() {
            return None;
        }
        let batch = WorktreeBatch {
            paths: shared.pending.drain().collect(),
            overflow: shared.overflow || shared.flooded(),
            dirs_changed: shared.dirs_dirty,
            last: shared.last.or(shared.dropped_last).unwrap_or(now),
        };
        shared.reset_batch();
        Some(batch)
    }

    /// 把一批退回去,安静满去抖之后再交(变化时刻按现在算):上一批还没查完时的兜底。
    pub fn requeue(&mut self, batch: WorktreeBatch) {
        if self.target.is_none() {
            return;
        }
        let now = Instant::now();
        let mut shared = self.shared.lock();
        for path in batch.paths {
            shared.record(path, now);
        }
        shared.overflow |= batch.overflow;
        shared.dirs_dirty |= batch.dirs_changed;
        shared.first.get_or_insert(now);
        shared.last = Some(now);
        shared.wake();
    }
}

// ─── 「更改」重取撞上 index.lock ─────────────────────────────

/// 等 `index.lock` 最多等多久(见 [`IndexLockGate`])。正常的 git 写索引(add / commit /
/// checkout / stash)持锁是毫秒到一两秒;再久就是编辑器开着的 `git commit`(提交说明写完
/// 之前锁一直在)或 git 崩溃留下的残锁,一直等下去「更改」列表就一直停在旧样子。取 2s:
/// 与 [`WORKTREE_MAX_WAIT`] 同一量级,持续改动时列表本来就是这个节奏;放行后读到的若是
/// git 写到一半的中间态,它写完时 `.git` 那一路还会再叫一次重取。
pub const INDEX_LOCK_PATIENCE: Duration = Duration::from_secs(2);

/// 等锁期间隔多久再看一眼(锁通常很快就没了,不必等满)。
const INDEX_LOCK_RETRY: Duration = DEBOUNCE;

/// 一次 `index.lock` 探测(后台读)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LockProbe {
    pub present: bool,
    /// 按锁文件的修改时间算有多老;读不到、或修改时间在将来(时钟不同步的网络盘)为 `None`
    pub age: Option<Duration>,
}

impl LockProbe {
    /// **阻塞**(一次 stat),调用方放后台执行器。
    pub fn read(git_dir: &Path) -> Self {
        match std::fs::metadata(git_dir.join("index.lock")) {
            Err(_) => Self::default(),
            Ok(meta) => Self {
                present: true,
                age: meta
                    .modified()
                    .ok()
                    .and_then(|at| SystemTime::now().duration_since(at).ok()),
            },
        }
    }
}

/// 「更改」列表的重取撞上 `index.lock` 时怎么办(面板持有的小状态机):锁还新(git 正写着
/// 索引,工作区可能是中间态)就把这次重取压着,隔 [`INDEX_LOCK_RETRY`] 再看;锁老过
/// [`INDEX_LOCK_PATIENCE`] 就不等了,照常取。
///
/// 锁的年龄取「按修改时间算的」与「从第一次看到它算起的」两者中大的:前者让崩溃残锁、开了
/// 很久的提交编辑器第一次撞上就放行;后者兜住修改时间读不到 / 在将来、或有谁一直碰它的锁
/// —— 只看前者的话这两种又会无限期等下去。锁没了才清「第一次看到」。
#[derive(Debug, Default)]
pub struct IndexLockGate {
    /// 压着的那次重取:要反映的最后一条变化的时刻、下一次看锁的时刻
    held: Option<(Instant, Instant)>,
    /// 这把锁第一次被看到的时刻
    first_seen: Option<Instant>,
}

impl IndexLockGate {
    /// 交一次看锁的结果(`at` = 开始看的时刻),连同一批要重取的变化(`last` = 这批最后一条
    /// 的时刻;只是到点复查时为 `None`)。返回 `Some(last)` = 现在就取,要反映到 `last` 为止
    /// 的变化(压着的那次并进来,取晚的);`None` = 没有要取的,或者接着压着。
    pub fn offer(
        &mut self,
        last: Option<Instant>,
        probe: LockProbe,
        at: Instant,
    ) -> Option<Instant> {
        let need = match (self.held.map(|(held, _)| held), last) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if !probe.present {
            self.first_seen = None;
            self.held = None;
            return need;
        }
        let first = *self.first_seen.get_or_insert(at);
        let seen = at.saturating_duration_since(first);
        let age = probe.age.map_or(seen, |age| age.max(seen));
        let need = need?;
        if age >= INDEX_LOCK_PATIENCE {
            self.held = None;
            return Some(need);
        }
        let retry = (INDEX_LOCK_PATIENCE - age).min(INDEX_LOCK_RETRY);
        self.held = Some((need, at + retry));
        None
    }

    /// 压着的那次到点该复查了。
    pub fn retry_due(&self, now: Instant) -> bool {
        self.held.is_some_and(|(_, at)| now >= at)
    }

    /// 换目标 / 收起:压着的作废。
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// notify 在 Windows 上有几十毫秒级延迟,给足预算但不无限等。
    const BUDGET: Duration = Duration::from_secs(10);

    /// 临时仓库:目录名带 pid + 纳秒 + 进程内序号;根 canonicalize 后剥掉 `\\?\`(与监听回报的
    /// 字面形态、libgit2 的 realpath 都对得上)。
    fn temp_repo(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mini-term-gitwatch-{tag}-{}-{ts}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = mt_project::fs::strip_verbatim_prefix(std::fs::canonicalize(&dir).unwrap());
        let out = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .expect("git 起不来");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        dir
    }

    /// 预算内等 `take_due` 报一次。
    fn wait_due(watch: &mut GitDirWatch) -> bool {
        let deadline = Instant::now() + BUDGET;
        while Instant::now() < deadline {
            if watch.take_due() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// 端到端:refs 下的变化攒够去抖报一次、取走即清;`*.lock` 噪声不报;
    /// 撤掉监听之后再动也不报。
    #[test]
    fn 相关变化去抖后报一次_噪声与撤销后不报() {
        let root = temp_repo("due");
        let layout = mt_project::git::git_dir_layout(&root).unwrap();
        let dirs = mt_project::git::git_watch_dirs(&layout);
        let mut watch = GitDirWatch::new();
        for ready in watch.set_target(root.to_string_lossy().to_string(), layout.clone(), dirs) {
            assert_eq!(
                ready.wait(BUDGET),
                Some(mt_project::watch::WatchOutcome::Active)
            );
        }

        // 噪声:只有锁文件
        std::fs::write(layout.git_dir.join("index.lock"), "x").unwrap();
        std::thread::sleep(DEBOUNCE + Duration::from_millis(400));
        assert!(!watch.take_due(), "锁文件不该触发刷新");
        std::fs::remove_file(layout.git_dir.join("index.lock")).unwrap();
        std::thread::sleep(DEBOUNCE + Duration::from_millis(200));
        let _ = watch.take_due();

        // 相关:新建一个分支引用
        let refs = layout.common_dir.join("refs").join("heads");
        std::fs::write(
            refs.join("feature"),
            "0000000000000000000000000000000000000000\n",
        )
        .unwrap();
        assert!(wait_due(&mut watch), "refs 下的变化应在去抖后报出来");
        assert!(!watch.take_due(), "一批只报一次");

        // 撤掉之后
        watch.clear();
        std::fs::write(
            refs.join("other"),
            "0000000000000000000000000000000000000000\n",
        )
        .unwrap();
        std::thread::sleep(DEBOUNCE + Duration::from_millis(400));
        assert!(!watch.take_due());
        assert_eq!(watch.repo(), None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 预算内等工作区监听交出一批。
    fn wait_batch(watch: &mut WorktreeWatch) -> Option<WorktreeBatch> {
        let deadline = Instant::now() + BUDGET;
        while Instant::now() < deadline {
            if let Some(batch) = watch.take_due() {
                return Some(batch);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// 端到端:子目录里改文件 → 去抖后交出一批(带路径);`.git` 里的变化不算;
    /// 新建目录要求重列清单;撤掉之后再动不报。
    #[test]
    fn 工作区变化去抖后交出_git_目录不算() {
        let root = temp_repo("worktree");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("a.ts"), "a\n").unwrap();
        let plan = mt_project::git::worktree_watch_dirs(&root).unwrap();
        assert!(plan.dirs.iter().any(|d| d.ends_with("src")), "{plan:?}");
        assert!(plan.trees.is_empty(), "目录少,逐个非递归挂");
        let mut watch = WorktreeWatch::new();
        for ready in watch.set_target(root.to_string_lossy().to_string(), plan) {
            assert_eq!(
                ready.wait(BUDGET),
                Some(mt_project::watch::WatchOutcome::Active)
            );
        }

        // git 目录里的变化(根目录的监听会报一条「.git 被修改」)不算
        std::fs::write(root.join(".git").join("scratch"), "x").unwrap();
        std::thread::sleep(DEBOUNCE + Duration::from_millis(400));
        assert!(watch.take_due().is_none(), ".git 下的变化不该交出来");

        std::fs::write(root.join("src").join("a.ts"), "changed\n").unwrap();
        let batch = wait_batch(&mut watch).expect("工作区文件改动应在去抖后交出来");
        assert!(
            batch.paths.iter().any(|p| p.ends_with("a.ts")),
            "{:?}",
            batch.paths
        );
        assert!(watch.take_due().is_none(), "一批只交一次");

        std::fs::create_dir_all(root.join("lib")).unwrap();
        let batch = wait_batch(&mut watch).expect("新建目录也是一批");
        assert!(batch.dirs_changed, "新目录要求重列监听清单");

        watch.clear();
        std::fs::write(root.join("src").join("a.ts"), "again\n").unwrap();
        std::thread::sleep(DEBOUNCE + Duration::from_millis(400));
        assert!(watch.take_due().is_none());
        assert_eq!(watch.repo(), None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 差额:撤掉的按旧清单的先后、新挂的按新清单的先后;没变的两边都不出现。
    #[test]
    fn 监听目录只增删差额() {
        let p = |s: &str| PathBuf::from(s);
        let old = [p("/r"), p("/r/a"), p("/r/b"), p("/r/c")];
        let new = [p("/r"), p("/r/c"), p("/r/d"), p("/r/a/x")];
        let (removed, added) = dir_diff(&old, &new);
        assert_eq!(removed, vec![p("/r/a"), p("/r/b")]);
        assert_eq!(added, vec![p("/r/d"), p("/r/a/x")]);
        let (removed, added) = dir_diff(&old, &old);
        assert!(removed.is_empty() && added.is_empty());
    }

    /// 只有建 / 删 / 改名才可能改变监听清单;被删的目录 stat 不到,靠清单认。
    #[test]
    fn 结构变化才重列清单() {
        assert!(is_structural("Create(Folder)"));
        assert!(is_structural("Remove(Any)"));
        assert!(is_structural("Modify(Name(Both))"));
        assert!(!is_structural("Modify(Data(Content))"));
        assert!(!is_structural("Modify(Any)"));
        assert!(reshapes_dirs(true, true, false), "新建的目录");
        assert!(reshapes_dirs(true, false, true), "被删的监听目录");
        assert!(!reshapes_dirs(true, false, false), "建 / 删的是文件");
        assert!(!reshapes_dirs(false, true, true), "只是内容变了");
    }

    #[test]
    fn git_目录内部的路径认得出() {
        let root = Path::new("/w/repo");
        assert!(is_git_internal(root, Path::new("/w/repo/.git")));
        assert!(is_git_internal(root, Path::new("/w/repo/.git/index")));
        assert!(!is_git_internal(root, Path::new("/w/repo/src/.github")));
        assert!(!is_git_internal(root, Path::new("/w/repo/.gitignore")));
        assert!(!is_git_internal(root, Path::new("/elsewhere/.git")));
    }

    /// 端到端(递归模式,D4):根非递归 + 一个顶层目录递归。子树深处的改动交得出来;落在
    /// 被跳过目录里面的丢掉(目录自己那条照常交);唤醒口一批只叫一声、交出去才复位。
    #[test]
    fn 递归子树深处的改动交得出_被跳过的目录里不交() {
        let root = temp_repo("worktree-tree");
        let src = root.join("src");
        let deep = src.join("d005").join("s00");
        let gen_dir = src.join("gen");
        for dir in [&deep, &gen_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(deep.join("f5000.txt"), "a\n").unwrap();
        let plan = WorktreeWatchDirs {
            dirs: vec![root.clone()],
            trees: vec![src.clone()],
            pruned: vec![gen_dir.clone()],
            truncated: false,
        };
        let mut watch = WorktreeWatch::new();
        let (tx, mut rx) = mpsc::unbounded();
        watch.set_waker(Some(tx));
        for ready in watch.set_target(root.to_string_lossy().to_string(), plan) {
            assert_eq!(
                ready.wait(BUDGET),
                Some(mt_project::watch::WatchOutcome::Active)
            );
        }
        assert!(!watch.is_partial());
        let wakes = |rx: &mut mpsc::UnboundedReceiver<()>| {
            let mut n = 0;
            while rx.try_recv().is_ok() {
                n += 1;
            }
            n
        };

        // 被跳过目录里面的写不交;它自己那条「被修改」(Windows 上子项变了会报)照常交,由面板
        // 按忽略规则滤掉;挂着监听的 src 自己「被修改」是噪声,不交
        std::fs::write(gen_dir.join("out.js"), "x").unwrap();
        std::thread::sleep(DEBOUNCE + Duration::from_millis(400));
        if let Some(batch) = watch.take_due() {
            assert!(
                batch.paths.iter().all(|p| *p == gen_dir),
                "被跳过目录里的变化不该交出来: {:?}",
                batch.paths
            );
        }
        let _ = wakes(&mut rx);

        std::fs::write(deep.join("f5000.txt"), "changed\n").unwrap();
        std::fs::write(deep.join("f5001.txt"), "new\n").unwrap();
        let batch = wait_batch(&mut watch).expect("子树深处的改动应在去抖后交出来");
        assert!(
            batch.paths.iter().any(|p| *p == deep.join("f5000.txt")),
            "{:?}",
            batch.paths
        );
        assert!(!batch.overflow);
        assert_eq!(wakes(&mut rx), 1, "一批只叫一声");

        // 交出去之后复位:下一批再叫
        std::fs::write(deep.join("f5000.txt"), "again\n").unwrap();
        assert!(wait_batch(&mut watch).is_some());
        assert_eq!(wakes(&mut rx), 1);

        watch.clear();
        std::fs::write(deep.join("f5000.txt"), "after\n").unwrap();
        std::thread::sleep(DEBOUNCE + Duration::from_millis(400));
        assert!(watch.take_due().is_none());
        assert_eq!(wakes(&mut rx), 0);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 落在被跳过目录**里面**才丢:目录自己那条不丢;名字是 node_modules / target 的按名字认
    /// (新冒出来的还不在清单里);工作区之外的不归它管。
    #[test]
    fn 被跳过目录里面的变化认得出() {
        let root = Path::new("/w/repo");
        let pruned: HashSet<PathBuf> = [PathBuf::from("/w/repo/src/gen")].into();
        let inside = |p: &str| inside_pruned(root, &pruned, Path::new(p));
        assert!(inside("/w/repo/src/gen/a.js"));
        assert!(inside("/w/repo/src/gen/deep/b.js"));
        assert!(!inside("/w/repo/src/gen"), "目录自己出现 / 消失照常要看");
        assert!(!inside("/w/repo/src/generated/a.js"), "前缀相同不算");
        assert!(inside("/w/repo/pkg/node_modules/x/index.js"));
        assert!(!inside("/w/repo/pkg/node_modules"));
        assert!(inside("/w/repo/crates/a/target/debug/x"));
        assert!(!inside("/w/repo/src/a.rs"));
        assert!(!inside("/elsewhere/node_modules/x"));
        // 根自己:往上的祖先叫 target 也不算
        let nested_root = Path::new("/home/u/target/repo");
        assert!(!inside_pruned(nested_root, &pruned, nested_root));
        assert!(!inside_pruned(
            nested_root,
            &pruned,
            Path::new("/home/u/target/repo/a.rs")
        ));
    }

    /// 丢掉的事件多到缓冲可能溢出过:没记下路径也按溢出交一批,但要等洪水安静满去抖;
    /// 没多到那个量就什么都不交。
    #[test]
    fn 丢得太多按溢出交() {
        let mut watch = WorktreeWatch::new();
        let now = Instant::now();
        {
            let mut shared = watch.shared.lock();
            shared.root = Some(PathBuf::from("/w/repo"));
            shared.dropped = FLOOD_DROPPED - 1;
            shared.dropped_last = now.checked_sub(DEBOUNCE);
        }
        assert!(watch.take_due().is_none(), "没多到那个量");
        assert_eq!(watch.due_in(), None);
        {
            let mut shared = watch.shared.lock();
            shared.dropped = FLOOD_DROPPED;
            shared.dropped_last = Some(now);
        }
        assert!(watch.take_due().is_none(), "洪水还没安静下来");
        assert!(watch.due_in().is_some_and(|wait| !wait.is_zero()));
        watch.shared.lock().dropped_last = now.checked_sub(DEBOUNCE);
        let batch = watch.take_due().expect("安静下来之后交一批");
        assert!(batch.overflow && batch.paths.is_empty());
        assert!(watch.take_due().is_none(), "交过就清");
    }

    /// 一批什么时候交:记下了路径的安静满去抖或攒满上限;只丢不记的洪水等它安静下来,
    /// 洪水还在流就一直不交(装依赖期间不每 2s 刷一次)。
    #[test]
    fn 批次到点的判定() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(batch_wait(None, None, None, t0), None);
        // 记下了路径:离安静满去抖还差多少
        assert_eq!(
            batch_wait(Some(t0), Some(t0), None, t0 + ms(100)),
            Some(DEBOUNCE - ms(100))
        );
        assert_eq!(
            batch_wait(Some(t0), Some(t0), None, t0 + DEBOUNCE),
            Some(Duration::ZERO)
        );
        // 一直有变化:攒满上限就交
        let last = t0 + WORKTREE_MAX_WAIT;
        assert_eq!(
            batch_wait(Some(t0), Some(last), None, last),
            Some(Duration::ZERO)
        );
        // 只有洪水:洪水最后一条之后安静满去抖才交
        let flood = t0 + ms(5000);
        assert_eq!(
            batch_wait(None, None, Some(flood), flood + ms(100)),
            Some(DEBOUNCE - ms(100))
        );
        assert_eq!(
            batch_wait(None, None, Some(flood), flood + DEBOUNCE),
            Some(Duration::ZERO)
        );
    }

    /// index.lock 的耐心(L1):短锁压着等它写完、隔一阵复查;锁没了当场取;老锁不等;锁一直
    /// 在最多等 [`INDEX_LOCK_PATIENCE`];读不到修改时间按第一次看到起算;锁没了再出现重新起算。
    #[test]
    fn 索引锁短锁等写完_长锁放行() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let held = |age: Option<u64>| LockProbe {
            present: true,
            age: age.map(ms),
        };
        let gone = LockProbe::default();

        // 没有锁:照常取;没有要取的就什么都不取
        let mut gate = IndexLockGate::default();
        assert_eq!(gate.offer(Some(t0), gone, t0), Some(t0));
        assert_eq!(gate.offer(None, gone, t0), None);

        // 新锁(git 正写着索引):压着,隔一阵复查
        assert_eq!(gate.offer(Some(t0), held(Some(100)), t0), None);
        assert!(!gate.retry_due(t0 + ms(100)));
        assert!(gate.retry_due(t0 + INDEX_LOCK_RETRY));
        // 期间又来一批:并进压着的那次,要反映的时刻取晚的
        let t1 = t0 + ms(200);
        assert_eq!(gate.offer(Some(t1), held(Some(300)), t1), None);
        // 锁没了:当场取
        assert_eq!(gate.offer(None, gone, t0 + ms(700)), Some(t1));
        assert!(!gate.retry_due(t0 + ms(60_000)), "取过了就不再压着");

        // 老锁(开着编辑器的 git commit、崩溃残锁):第一次撞上就放行
        let mut gate = IndexLockGate::default();
        assert_eq!(gate.offer(Some(t0), held(Some(60_000)), t0), Some(t0));

        // 锁一直在:最多等满耐心,复查不越过放行的那一刻
        let mut gate = IndexLockGate::default();
        assert_eq!(gate.offer(Some(t0), held(Some(1800)), t0), None);
        assert!(
            gate.retry_due(t0 + ms(200)),
            "离放行只剩 200ms,不必等满复查间隔"
        );
        assert_eq!(gate.offer(None, held(Some(2000)), t0 + ms(200)), Some(t0));

        // 读不到修改时间(或在将来):按第一次看到它起算
        let mut gate = IndexLockGate::default();
        assert_eq!(gate.offer(Some(t0), held(None), t0), None);
        assert_eq!(gate.offer(None, held(None), t0 + ms(1000)), None);
        assert_eq!(
            gate.offer(None, held(None), t0 + INDEX_LOCK_PATIENCE),
            Some(t0)
        );
        // 锁还在:之后的批次不再等
        let t2 = t0 + ms(5000);
        assert_eq!(gate.offer(Some(t2), held(None), t2), Some(t2));
        // 修改时间一直很新、但已经看了它很久(有谁一直碰它):同样放行,不无限等
        assert_eq!(gate.offer(Some(t2), held(Some(10)), t2), Some(t2));
        // 锁没了又出现:重新起算
        assert_eq!(gate.offer(None, gone, t2), None);
        let t3 = t2 + ms(100);
        assert_eq!(gate.offer(Some(t3), held(None), t3), None);
        gate.reset();
        assert!(!gate.retry_due(t3 + ms(60_000)), "reset 后压着的作废");
    }

    /// 真文件探测:没有锁 / 刚建的锁(年龄很小)。
    #[test]
    fn 索引锁探测() {
        let root = temp_repo("lockprobe");
        let git_dir = root.join(".git");
        assert_eq!(LockProbe::read(&git_dir), LockProbe::default());
        std::fs::write(git_dir.join("index.lock"), "x").unwrap();
        let probe = LockProbe::read(&git_dir);
        assert!(probe.present);
        assert!(probe.age.is_none_or(|age| age < Duration::from_secs(60)));
        std::fs::remove_dir_all(&root).ok();
    }
}
