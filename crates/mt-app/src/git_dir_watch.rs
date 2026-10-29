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
//!   objects / logs 噪声丢掉),命中就把「最近一次相关变化」的时刻记下;
//! - 主线程借 Git 面板现成的 100ms 节拍问 [`GitDirWatch::take_due`]:安静满去抖窗口了
//!   才报一次,由面板去后台算签名、对比、决定刷什么(见 `git_panel.rs`)。
//!
//! 只在面板可见时挂监听(收起即 [`GitDirWatch::clear`]),与 `set_visible` 那道扫盘闸
//! 同一口径。

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
}
