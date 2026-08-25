//! 目录监听。原本是 `notify` 回调里 `emit("fs-change")` 给前端,
//! 现在改成**注入式回调**:构造 [`FsWatcher`] 时交一个 sink 进来,
//! 由上层决定怎么接(GPUI 侧典型做法是在 sink 里 `cx.update` 后 `cx.notify()`)。
//!
//! watch / unwatch 的生命周期语义与 Tauri 版完全一致:
//! 同一路径按引用计数复用一个 `RecommendedWatcher`,计数归零才真正摘除。
//! 这条不能简化 —— 文件树的压缩链让多个 UI 节点可能 watch 同一路径
//! (链中段与其真实节点),无计数时后注册者会顶掉前者的 watcher、
//! 先注销者会把仍在用的 watcher 一并摘除。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use anyhow::{Context as _, Result};
use notify::{Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;

/// 一次文件系统变更。`kind` 沿用原实现的 `format!("{:?}", event.kind)`
/// 调试字符串形态 —— 上层只用它区分「创建/删除/修改」做重扫决策。
#[derive(Debug, Clone)]
pub struct FsChange {
    /// 触发监听时登记的项目路径,用来把变更路由回对应项目的文件树。
    pub project_path: String,
    pub path: PathBuf,
    pub kind: String,
}

/// 变更 sink。跨线程调用(notify 自己的线程),故要求 `Send + Sync`。
pub type FsChangeSink = Arc<dyn Fn(FsChange) + Send + Sync>;

pub struct FsWatcher {
    // 值为 (watcher, 引用计数)
    watchers: Mutex<HashMap<PathBuf, (RecommendedWatcher, usize)>>,
    sink: FsChangeSink,
}

impl FsWatcher {
    pub fn new<F>(sink: F) -> Self
    where
        F: Fn(FsChange) + Send + Sync + 'static,
    {
        Self {
            watchers: Mutex::new(HashMap::new()),
            sink: Arc::new(sink),
        }
    }

    /// 用 mpsc 通道接变更(测试与「自己起消费线程」的调用方用)。
    /// 通道断开后 sink 变成空操作,不影响 watcher 本身。
    pub fn with_channel() -> (Self, Receiver<FsChange>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        (
            Self::new(move |change| {
                let _ = tx.lock().send(change);
            }),
            rx,
        )
    }

    /// 开始监听目录(非递归)。同一路径重复调用只递增引用计数,不重建 watcher
    /// —— notify 后端重复注册同一路径会浪费句柄。
    ///
    /// `path` 必须在 `project_path` 之内:与 `fs.rs` 各入口共用
    /// [`crate::fs::verify_under_project_root`] 这一把尺子(canonicalize 解 `..`
    /// 与符号链接)。现有调用方(文件树、文件查看器)传的本来就是根内路径,
    /// 属纵深防御——挡住将来有人拿拖放/输入框来的路径直接开监听。
    /// WSL UNC(`\\wsl.localhost\...`)项目走同一条路:文件树列目录本就每次
    /// 过这个校验,监听侧口径一致即可,不额外分叉。
    pub fn watch(&self, path: &Path, project_path: &str) -> Result<()> {
        // 只做校验,**不拿返回的规范化路径当 key** —— unwatch 收到的是调用方
        // 手上的原始路径,两边 key 必须同形,否则引用计数对不上、watcher 摘不掉
        crate::fs::verify_under_project_root(Path::new(project_path), path, true)?;

        let key = path.to_path_buf();
        // 已有同路径 watcher:仅计数 +1
        {
            let mut watchers = self.watchers.lock();
            if let Some(entry) = watchers.get_mut(&key) {
                entry.1 += 1;
                return Ok(());
            }
        }

        let sink = self.sink.clone();
        let project_path = project_path.to_string();
        let mut watcher = notify::recommended_watcher(move |res: Result<NotifyEvent, _>| {
            if let Ok(event) = res {
                for p in &event.paths {
                    sink(FsChange {
                        project_path: project_path.clone(),
                        path: p.clone(),
                        kind: format!("{:?}", event.kind),
                    });
                }
            }
        })
        .context("创建文件监听器失败")?;

        watcher
            .watch(&key, RecursiveMode::NonRecursive)
            .with_context(|| format!("监听目录失败: {}", key.display()))?;

        let mut watchers = self.watchers.lock();
        // 竞态兜底:抢先检查后、insert 前若有并发注册者已写入,则沿用其 watcher 只递增计数
        match watchers.get_mut(&key) {
            Some(entry) => entry.1 += 1,
            None => {
                watchers.insert(key, (watcher, 1));
            }
        }
        Ok(())
    }

    /// 引用计数 -1,归零时摘除 watcher。未被监听的路径是空操作。
    pub fn unwatch(&self, path: &Path) {
        let mut watchers = self.watchers.lock();
        if let Some(entry) = watchers.get_mut(path) {
            entry.1 -= 1;
            if entry.1 == 0 {
                watchers.remove(path);
            }
        }
    }

    /// 当前实际持有的 watcher 数量(不是引用计数之和)。
    pub fn watched_count(&self) -> usize {
        self.watchers.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn temp_dir(tag: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mini-term-watch-{tag}-{ts}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 测试里项目根就是被监听目录本身(watch 现在会校验路径在根内)。
    fn proj(dir: &Path) -> String {
        dir.to_string_lossy().to_string()
    }

    #[test]
    fn watch_refcounts_same_path() {
        let dir = temp_dir("refcount");
        let (w, _rx) = FsWatcher::with_channel();

        w.watch(&dir, &proj(&dir)).unwrap();
        w.watch(&dir, &proj(&dir)).unwrap();
        assert_eq!(w.watched_count(), 1, "同一路径只应有一个 watcher");

        // 第一次 unwatch 只把计数降到 1,watcher 必须还在
        w.unwatch(&dir);
        assert_eq!(w.watched_count(), 1);
        w.unwatch(&dir);
        assert_eq!(w.watched_count(), 0);
        // 多余的 unwatch 不应 panic
        w.unwatch(&dir);
        assert_eq!(w.watched_count(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sink_receives_change_for_watched_dir() {
        let dir = temp_dir("sink");
        let (w, rx) = FsWatcher::with_channel();
        let root = proj(&dir);
        w.watch(&dir, &root).unwrap();

        std::fs::write(dir.join("new.txt"), "hello").unwrap();

        // notify 在 Windows 上有几十毫秒级延迟,给足预算但不无限等
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got = None;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(change) => {
                    got = Some(change);
                    break;
                }
                Err(_) => continue,
            }
        }
        let change = got.expect("10s 内未收到任何 fs 变更事件");
        assert_eq!(change.project_path, root);
        assert!(!change.kind.is_empty());

        w.unwatch(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn custom_sink_is_invoked() {
        // 注入任意闭包(不只是 channel):验证回调形态本身
        let dir = temp_dir("closure");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let w = FsWatcher::new(move |_change| {
            hits_clone.fetch_add(1, Ordering::Relaxed);
        });
        w.watch(&dir, &proj(&dir)).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && hits.load(Ordering::Relaxed) == 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(hits.load(Ordering::Relaxed) > 0, "注入的闭包应被调用");

        w.unwatch(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn watch_nonexistent_path_errors() {
        let (w, _rx) = FsWatcher::with_channel();
        let root = temp_dir("missing-root");
        let missing = root.join("definitely-missing-xyz");
        assert!(w.watch(&missing, &proj(&root)).is_err());
        assert_eq!(w.watched_count(), 0, "失败的 watch 不应留下条目");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn watch_rejects_path_outside_project_root() {
        // root/sub 与 root 平级的 outside:拿 `..` 逃出去的路径不许开监听
        let root = temp_dir("escape-root");
        let outside = temp_dir("escape-outside");
        let (w, _rx) = FsWatcher::with_channel();

        let escaped = root.join("..").join(outside.file_name().unwrap());
        assert!(
            w.watch(&escaped, &proj(&root)).is_err(),
            "越出项目根的路径必须拒绝"
        );
        assert_eq!(w.watched_count(), 0, "被拒的 watch 不应留下条目");

        // 同一目录换成从根内进入则放行,证明拒绝的是「越界」而非「路径带 ..」
        let inside = root.join("sub");
        std::fs::create_dir_all(&inside).unwrap();
        w.watch(&root.join("sub").join("..").join("sub"), &proj(&root))
            .unwrap();
        assert_eq!(w.watched_count(), 1);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
