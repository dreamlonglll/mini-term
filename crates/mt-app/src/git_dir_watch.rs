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
//!   目录一条,最多 [`mt_project::git::WORKTREE_WATCH_DIR_LIMIT`] 条(大仓库只挂浅层,
//!   深处的变化由另两路与手动刷新兜底;Windows 上每条约 16 KB 缓冲,封顶约 4 MB)。
//!   同一目录文件树也在听时共用后端那一条,不重复注册;
//! - sink 只记路径与时刻(`.git` 下的直接丢),并在「目录出现 / 消失」时标记要重列清单;
//! - 主线程借面板节拍问 [`WorktreeWatch::take_due`]:安静满 [`DEBOUNCE`](或一直有变化、
//!   攒满 [`WORKTREE_MAX_WAIT`])才交出一批,面板在后台按忽略规则过一遍
//!   ([`mt_project::git::any_unignored_change`]),有一条没被忽略才**只重取「更改」列表**
//!   (历史 / 分支不动,大仓库不跟着抖)。
//!
//! 防自激:刷新链路全是只读(git2 的 status 不回写索引,读文件不产生写事件),面板自己
//! 不会把自己叫醒;面板里的丢弃 / 按块丢弃会改工作区,顺带再取一次列表,无害。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mt_project::git::{GitDirLayout, is_relevant_git_change};
use mt_project::watch::{FsWatcher, WatchReady};
use parking_lot::Mutex;

/// 去抖窗口:最后一次相关变化之后安静这么久才刷。一次 `git commit` 会在几十毫秒里
/// 连写 index / 对象 / 引用 / reflog,一次 rebase 更多 —— 等它写完再看。
pub const DEBOUNCE: Duration = Duration::from_millis(500);

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
        for old in target.dirs.iter().filter(|d| !dirs.contains(d)) {
            self.watcher.unwatch(old);
        }
        let added: Vec<PathBuf> = dirs
            .iter()
            .filter(|d| !target.dirs.contains(d))
            .cloned()
            .collect();
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

/// sink 与主线程共享的那一小块。
#[derive(Default)]
struct WorkShared {
    /// 工作区根(过滤 `.git` 要用)。`None` = 没在监听
    root: Option<PathBuf>,
    /// 挂着监听的目录:它们自己被删 / 改名时要重列清单
    dirs: HashSet<PathBuf>,
    pending: HashSet<PathBuf>,
    overflow: bool,
    dirs_dirty: bool,
    /// 这一批第一条 / 最近一条变化的时刻
    first: Option<Instant>,
    last: Option<Instant>,
}

impl WorkShared {
    fn reset_batch(&mut self) {
        self.pending.clear();
        self.overflow = false;
        self.dirs_dirty = false;
        self.first = None;
        self.last = None;
    }
}

/// 一批安静下来的工作区变化。
#[derive(Debug, Default)]
pub struct WorktreeBatch {
    pub paths: Vec<PathBuf>,
    /// 路径太多没记全:当相关处理
    pub overflow: bool,
    /// 有目录出现 / 消失:监听目录清单要重列
    pub dirs_changed: bool,
}

/// 一条变化是不是 git 自己的目录里的(`<root>/.git/…`,linked worktree 的 `.git` 文件同理)。
fn is_git_internal(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(|rel| rel.components().next())
        .is_some_and(|first| first.as_os_str() == ".git")
}

/// 这条变化会不会改变「该挂哪些目录」:目录被建 / 删 / 改名(notify 的 kind 是
/// `EventKind` 的调试串,`Create(..)` / `Remove(..)` / `Modify(Name(..))`)。
fn reshapes_dirs(kind: &str, path: &Path, watched: &HashSet<PathBuf>) -> bool {
    let structural =
        kind.starts_with("Create") || kind.starts_with("Remove") || kind.contains("Name");
    // 被删 / 改走的目录已经 stat 不到了,只能靠「它在不在清单里」认
    structural && (watched.contains(path) || path.is_dir())
}

/// 工作区监听(见模块注释「工作区监听」)。
pub struct WorktreeWatch {
    watcher: FsWatcher,
    shared: Arc<Mutex<WorkShared>>,
    /// 当前目标:(仓库路径, 挂着的目录)
    target: Option<(String, Vec<PathBuf>)>,
}

impl WorktreeWatch {
    pub fn new() -> Self {
        let shared = Arc::new(Mutex::new(WorkShared::default()));
        let sink = shared.clone();
        let watcher = FsWatcher::with_label("git-worktree", move |change| {
            // notify 线程:只过滤 + 记账,不碰 GPUI
            let mut shared = sink.lock();
            let Some(root) = shared.root.clone() else {
                return;
            };
            if is_git_internal(&root, &change.path) {
                return;
            }
            if reshapes_dirs(&change.kind, &change.path, &shared.dirs) {
                shared.dirs_dirty = true;
            }
            if shared.pending.len() < PENDING_CAP {
                shared.pending.insert(change.path);
            } else {
                shared.overflow = true;
            }
            let now = Instant::now();
            shared.first.get_or_insert(now);
            shared.last = Some(now);
        });
        Self {
            watcher,
            shared,
            target: None,
        }
    }

    /// 正在监听的仓库路径。
    pub fn repo(&self) -> Option<&str> {
        self.target.as_ref().map(|(repo, _)| repo.as_str())
    }

    /// 换监听目标。`dirs` 第一项是工作区根(见 `worktree_watch_dirs`)。返回各目录的注册回执。
    pub fn set_target(&mut self, repo: String, dirs: Vec<PathBuf>) -> Vec<WatchReady> {
        self.clear();
        let Some(root) = dirs.first().cloned() else {
            return Vec::new();
        };
        let project = root.to_string_lossy().to_string();
        let ready = dirs
            .iter()
            .map(|dir| self.watcher.watch(dir, &project))
            .collect();
        let mut shared = self.shared.lock();
        shared.root = Some(root);
        shared.dirs = dirs.iter().cloned().collect();
        shared.reset_batch();
        drop(shared);
        self.target = Some((repo, dirs));
        ready
    }

    /// 同一目标下目录清单变了:只增删差额。
    pub fn update_dirs(&mut self, dirs: Vec<PathBuf>) {
        let Some((_, current)) = self.target.as_mut() else {
            return;
        };
        if *current == dirs {
            return;
        }
        let Some(root) = current.first().cloned() else {
            return;
        };
        let project = root.to_string_lossy().to_string();
        for old in current.iter().filter(|d| !dirs.contains(d)) {
            self.watcher.unwatch(old);
        }
        for dir in dirs.iter().filter(|d| !current.contains(d)) {
            self.watcher.watch(dir, &project);
        }
        self.shared.lock().dirs = dirs.iter().cloned().collect();
        *current = dirs;
    }

    /// 撤掉全部监听(面板收起 / 远程项目 / 没有仓库)。
    pub fn clear(&mut self) {
        if let Some((_, dirs)) = self.target.take() {
            for dir in &dirs {
                self.watcher.unwatch(dir);
            }
        }
        let mut shared = self.shared.lock();
        shared.root = None;
        shared.dirs.clear();
        shared.reset_batch();
    }

    /// 有没有一批该交出去的变化:安静满 [`DEBOUNCE`],或这一批已经攒了
    /// [`WORKTREE_MAX_WAIT`]。取走即清。
    pub fn take_due(&mut self) -> Option<WorktreeBatch> {
        let mut shared = self.shared.lock();
        let (first, last) = (shared.first?, shared.last?);
        if last.elapsed() < DEBOUNCE && first.elapsed() < WORKTREE_MAX_WAIT {
            return None;
        }
        let batch = WorktreeBatch {
            paths: shared.pending.drain().collect(),
            overflow: shared.overflow,
            dirs_changed: shared.dirs_dirty,
        };
        shared.reset_batch();
        Some(batch)
    }

    /// 上一批还在后台检查时又到点了:把这一批退回去,等下一拍再交(变化时刻按现在算)。
    pub fn requeue(&mut self, batch: WorktreeBatch) {
        if self.target.is_none() {
            return;
        }
        let mut shared = self.shared.lock();
        for path in batch.paths {
            if shared.pending.len() < PENDING_CAP {
                shared.pending.insert(path);
            } else {
                shared.overflow = true;
            }
        }
        shared.overflow |= batch.overflow;
        shared.dirs_dirty |= batch.dirs_changed;
        let now = Instant::now();
        shared.first.get_or_insert(now);
        shared.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// notify 在 Windows 上有几十毫秒级延迟,给足预算但不无限等。
    const BUDGET: Duration = Duration::from_secs(10);

    fn temp_repo(tag: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mini-term-gitwatch-{tag}-{ts}"));
        std::fs::create_dir_all(&dir).unwrap();
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
        let dirs = mt_project::git::worktree_watch_dirs(&root).unwrap().dirs;
        assert!(dirs.iter().any(|d| d.ends_with("src")), "{dirs:?}");
        let mut watch = WorktreeWatch::new();
        for ready in watch.set_target(root.to_string_lossy().to_string(), dirs) {
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

    #[test]
    fn git_目录内部的路径认得出() {
        let root = Path::new("/w/repo");
        assert!(is_git_internal(root, Path::new("/w/repo/.git")));
        assert!(is_git_internal(root, Path::new("/w/repo/.git/index")));
        assert!(!is_git_internal(root, Path::new("/w/repo/src/.github")));
        assert!(!is_git_internal(root, Path::new("/w/repo/.gitignore")));
        assert!(!is_git_internal(root, Path::new("/elsewhere/.git")));
    }
}
