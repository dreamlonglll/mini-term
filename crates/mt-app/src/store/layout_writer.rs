//! `layout.db` 落盘的后台单写者线程。
//!
//! # 为什么要把写盘搬出 UI 线程
//!
//! 拖分隔条 / 改窗口大小 / 切分屏都会调 [`AppStore::flush_layout_now`](super::AppStore::flush_layout_now),
//! 底层落进 `mt_layout::LayoutStore` 的 SQLite 写事务。即便做了 300ms 防抖,真落盘那一刻
//! 若遇杀软扫盘、慢磁盘或并发 WAL checkpoint,仍会在 GPUI 主线程上制造可感知的掉帧。
//!
//! 参照 [`super::config_writer`] 的同一模式:
//!
//! - **主线程只采快照**:`flush_layout_now` 在 UI 线程上把脏掉的全局项和各项目分屏树
//!   采成不可变的 [`LayoutBatch`],入队即返回;
//! - **后台单写者**:单独一根 `mt-layout-writer` 线程顺序落进 `LayoutStore` ——
//!   消除主线程 I/O 阻塞,同时天然串行,不会出现新旧批次竞态乱序;
//! - **按槽位折叠**:写线程忙着写盘期间,主线程又送来多次布局变更时,
//!   - 全局项(`GlobalLayout`)是全量快照,后来的直接覆盖前一份;
//!   - 项目布局按 `project_id` 折叠,同一项目后来的 `Save`/`Delete` 覆盖前一份,
//!     不同项目的更新各自保留、互不覆盖;
//! - **退出必落盘**:`App::on_app_quit` 持有的 [`LayoutDrainHandle`] 在进程退出前同步
//!   等待队列与在途批次全部写完。写线程启动失败时优雅退回主线程同步直写。

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mt_config::SavedProjectLayout;
use mt_layout::{GlobalLayout, LayoutStore};

/// 排干的兜底上限。写线程若卡在一次不返回的 SQLite 写盘上(网络盘掉线),
/// 宁可丢这一次布局保存也不能把退出流程永久吊死(与 `config_writer::DRAIN_DEADLINE` 同口径)。
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// 单个项目的一条待落盘布局动作。
#[derive(Debug, Clone)]
pub(super) enum ProjectLayoutOp {
    Save {
        layout: SavedProjectLayout,
        now_ms: i64,
    },
    Delete,
}

/// 主线程一次 `flush_layout_now` 采集出来的不可变脏快照。
#[derive(Debug, Clone, Default)]
pub(super) struct LayoutBatch {
    pub globals: Option<GlobalLayout>,
    pub projects: Vec<(String, ProjectLayoutOp)>,
}

impl LayoutBatch {
    pub(super) fn is_empty(&self) -> bool {
        self.globals.is_none() && self.projects.is_empty()
    }
}

/// 队列状态机(纯数据,可不建线程独立单测)。
#[derive(Debug, Default)]
struct Queue {
    pending_globals: Option<GlobalLayout>,
    pending_projects: HashMap<String, ProjectLayoutOp>,
    /// 每次非空 `push` 递增,用于追踪已入队与已落盘的代数。
    queued: u64,
    /// 最近一次落盘完成时的代数。
    written: u64,
    /// 写线程正在写的那批快照的代数;`None` 表示写线程空闲。
    in_flight: Option<u64>,
    /// 实际执行过的写盘批次数(单测验证折叠用)。
    writes: u64,
    stop: bool,
}

impl Queue {
    /// 投递一批脏快照。空批次忽略;非空时按 `globals` 与 `project_id` 折叠合并。
    fn push(&mut self, batch: LayoutBatch) -> Option<u64> {
        if batch.is_empty() {
            return None;
        }
        if let Some(globals) = batch.globals {
            self.pending_globals = Some(globals);
        }
        for (project_id, op) in batch.projects {
            self.pending_projects.insert(project_id, op);
        }
        self.queued += 1;
        Some(self.queued)
    }

    /// 写线程取走当前积压的全部改动作为一个批次,并记为在途。
    fn take_next(&mut self) -> Option<(u64, LayoutBatch)> {
        if self.pending_globals.is_none() && self.pending_projects.is_empty() {
            return None;
        }
        let globals = self.pending_globals.take();
        let mut projects: Vec<(String, ProjectLayoutOp)> = self.pending_projects.drain().collect();
        // 按 project_id 排序,让写入顺序确定且便于日志排查。
        projects.sort_by(|a, b| a.0.cmp(&b.0));
        let generation = self.queued;
        self.in_flight = Some(generation);
        Some((generation, LayoutBatch { globals, projects }))
    }

    fn finish_in_flight(&mut self, generation: u64) {
        self.in_flight = None;
        self.written = self.written.max(generation);
        self.writes += 1;
    }

    /// 队列既无待写也无在途批次 = 全部已落盘。
    fn is_idle(&self) -> bool {
        self.pending_globals.is_none()
            && self.pending_projects.is_empty()
            && self.in_flight.is_none()
    }

    fn request_stop(&mut self) {
        self.stop = true;
    }
}

struct Shared {
    queue: Mutex<Queue>,
    cv: Condvar,
}

impl Shared {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            cv: Condvar::new(),
        }
    }

    fn drain(&self) {
        let deadline = Instant::now() + DRAIN_DEADLINE;
        let mut guard = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        while !guard.is_idle() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                tracing::warn!(
                    "[layout] 等布局写盘超过 {}s 仍未收摊,本次放弃排干(最后一次布局可能没落盘)",
                    DRAIN_DEADLINE.as_secs()
                );
                return;
            };
            let (next, _) = self
                .cv
                .wait_timeout(guard, left)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
        }
    }
}

/// 退出前等待全部待写布局落盘的轻量句柄,可跨 `App::on_app_quit` 闭包持有。
#[derive(Clone)]
pub(super) struct LayoutDrainHandle {
    shared: Arc<Shared>,
}

impl LayoutDrainHandle {
    pub(super) fn drain(&self) {
        self.shared.drain();
    }
}

/// `layout.db` 后台单写者。
pub(super) struct LayoutWriter {
    store: Arc<LayoutStore>,
    shared: Arc<Shared>,
    /// `None` = 写线程启动失败,退回主线程同步写盘。
    worker: Option<thread::JoinHandle<()>>,
}

impl LayoutWriter {
    pub(super) fn spawn(store: Arc<LayoutStore>) -> Self {
        let shared = Arc::new(Shared::new());
        let worker_store = store.clone();
        let worker_shared = shared.clone();
        let worker = match thread::Builder::new()
            .name("mt-layout-writer".into())
            .spawn(move || worker_loop(worker_store, worker_shared))
        {
            Ok(handle) => Some(handle),
            Err(err) => {
                tracing::warn!("[layout] 后台写线程启动失败({err}),退回主线程同步写盘");
                None
            }
        };
        Self {
            store,
            shared,
            worker,
        }
    }

    /// 投递一批布局变更。写线程不可用时同步直写。
    pub(super) fn enqueue(&self, batch: LayoutBatch) {
        if batch.is_empty() {
            return;
        }
        if self.worker.is_none() {
            write_batch(&self.store, &batch);
            return;
        }
        let mut guard = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if guard.push(batch).is_some() {
            self.shared.cv.notify_all();
        }
    }

    /// 供 `App::on_app_quit` 持有的同步排干句柄。
    pub(super) fn drain_handle(&self) -> LayoutDrainHandle {
        LayoutDrainHandle {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for LayoutWriter {
    fn drop(&mut self) {
        if let Some(handle) = self.worker.take() {
            {
                let mut guard = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                guard.request_stop();
                self.shared.cv.notify_all();
            }
            let _ = handle.join();
        }
    }
}

fn worker_loop(store: Arc<LayoutStore>, shared: Arc<Shared>) {
    loop {
        let next = {
            let mut guard = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(item) = guard.take_next() {
                    break Some(item);
                }
                if guard.stop {
                    break None;
                }
                guard = shared.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
        };

        let Some((generation, batch)) = next else {
            return;
        };

        write_batch(&store, &batch);

        let mut guard = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        guard.finish_in_flight(generation);
        shared.cv.notify_all();
    }
}

fn write_batch(store: &LayoutStore, batch: &LayoutBatch) {
    if let Some(globals) = &batch.globals
        && let Err(err) = store.save_globals(globals)
    {
        tracing::warn!("[layout] 全局布局写盘失败: {err:#}");
    }
    for (project_id, op) in &batch.projects {
        match op {
            ProjectLayoutOp::Save { layout, now_ms } => {
                if let Err(err) = store.save_project_layout(project_id, layout, *now_ms) {
                    tracing::warn!("[layout] 项目 {project_id} 的布局写盘失败: {err:#}");
                }
            }
            ProjectLayoutOp::Delete => {
                if let Err(err) = store.delete_project_layout(project_id) {
                    tracing::warn!("[layout] 删除项目 {project_id} 的布局失败: {err:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_config::{SavedPane, SavedSplitNode, SavedTab};

    fn sample_globals(width: f64) -> GlobalLayout {
        GlobalLayout {
            right_drawer_width: Some(width),
            ..GlobalLayout::default()
        }
    }

    fn sample_project_layout(panel_title: &str) -> SavedProjectLayout {
        SavedProjectLayout {
            tabs: vec![SavedTab {
                custom_title: Some(panel_title.to_string()),
                split_layout: SavedSplitNode::Leaf {
                    pane: None,
                    panes: vec![SavedPane {
                        shell_name: "pwsh".to_string(),
                        cwd: None,
                        ai_session: None,
                    }],
                },
            }],
            active_tab_index: 0,
        }
    }

    #[test]
    fn 空批次不入队也不递增代数() {
        let mut q = Queue::default();
        assert!(q.is_idle());
        assert_eq!(q.push(LayoutBatch::default()), None);
        assert_eq!(q.queued, 0);
        assert!(q.is_idle());
        assert!(q.take_next().is_none());
    }

    #[test]
    fn 连续多次_globals_变更折叠为最新快照() {
        let mut q = Queue::default();
        assert_eq!(
            q.push(LayoutBatch {
                globals: Some(sample_globals(320.0)),
                projects: Vec::new(),
            }),
            Some(1)
        );
        assert_eq!(
            q.push(LayoutBatch {
                globals: Some(sample_globals(420.0)),
                projects: Vec::new(),
            }),
            Some(2)
        );
        assert!(!q.is_idle());

        let (generation, batch) = q.take_next().expect("应取到折叠后的批次");
        assert_eq!(generation, 2);
        assert_eq!(
            batch.globals.as_ref().and_then(|g| g.right_drawer_width),
            Some(420.0)
        );
        assert!(batch.projects.is_empty());
        // 在途期间不算 idle
        assert!(!q.is_idle());

        q.finish_in_flight(generation);
        assert!(q.is_idle());
        assert_eq!(q.written, 2);
        assert_eq!(q.writes, 1);
    }

    #[test]
    fn 同项目覆盖而不同项目互不覆盖() {
        let mut q = Queue::default();
        q.push(LayoutBatch {
            globals: Some(sample_globals(300.0)),
            projects: vec![
                (
                    "p-a".into(),
                    ProjectLayoutOp::Save {
                        layout: sample_project_layout("panel-1"),
                        now_ms: 100,
                    },
                ),
                (
                    "p-b".into(),
                    ProjectLayoutOp::Save {
                        layout: sample_project_layout("panel-b1"),
                        now_ms: 100,
                    },
                ),
            ],
        });
        // p-a 改为 Delete,p-b 更新为 panel-b2,新增 p-c,globals 未变(None)不应清掉已有待写 globals
        q.push(LayoutBatch {
            globals: None,
            projects: vec![
                ("p-a".into(), ProjectLayoutOp::Delete),
                (
                    "p-b".into(),
                    ProjectLayoutOp::Save {
                        layout: sample_project_layout("panel-b2"),
                        now_ms: 200,
                    },
                ),
                (
                    "p-c".into(),
                    ProjectLayoutOp::Save {
                        layout: sample_project_layout("panel-c1"),
                        now_ms: 200,
                    },
                ),
            ],
        });

        let (generation, batch) = q.take_next().expect("应取到合并后的批次");
        assert_eq!(generation, 2);
        assert_eq!(
            batch.globals.as_ref().and_then(|g| g.right_drawer_width),
            Some(300.0),
            "后续批次 globals=None 不能抹掉先前的全局脏快照"
        );
        assert_eq!(batch.projects.len(), 3);
        assert_eq!(batch.projects[0].0, "p-a");
        assert!(matches!(batch.projects[0].1, ProjectLayoutOp::Delete));
        assert_eq!(batch.projects[1].0, "p-b");
        assert!(matches!(
            batch.projects[1].1,
            ProjectLayoutOp::Save { now_ms: 200, .. }
        ));
        assert_eq!(batch.projects[2].0, "p-c");
    }

    #[test]
    fn 在途写入期间新到变更留在下一批且不提前_idle() {
        let mut q = Queue::default();
        q.push(LayoutBatch {
            globals: Some(sample_globals(300.0)),
            projects: Vec::new(),
        });
        let (gen1, _) = q.take_next().unwrap();
        assert!(!q.is_idle());

        // 写盘期间主线程又推进了新布局
        q.push(LayoutBatch {
            globals: None,
            projects: vec![("p-1".into(), ProjectLayoutOp::Delete)],
        });
        q.finish_in_flight(gen1);
        assert!(!q.is_idle(), "待写队列还有项目变更,不能判定 idle");

        let (gen2, batch2) = q.take_next().unwrap();
        assert_eq!(gen2, 2);
        assert!(batch2.globals.is_none());
        assert_eq!(batch2.projects.len(), 1);
        q.finish_in_flight(gen2);
        assert!(q.is_idle());
        assert_eq!(q.writes, 2);
    }

    #[test]
    fn 后台线程真实落盘并在_drain_后立即完整可读() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mini-term-layout-writer-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let store = Arc::new(LayoutStore::open_at(&dir).unwrap());
        let writer = LayoutWriter::spawn(store.clone());
        let drain = writer.drain_handle();

        writer.enqueue(LayoutBatch {
            globals: Some(sample_globals(360.0)),
            projects: vec![
                (
                    "proj-keep".into(),
                    ProjectLayoutOp::Save {
                        layout: sample_project_layout("pan-1"),
                        now_ms: 1000,
                    },
                ),
                (
                    "proj-del".into(),
                    ProjectLayoutOp::Save {
                        layout: sample_project_layout("pan-x"),
                        now_ms: 1000,
                    },
                ),
            ],
        });
        writer.enqueue(LayoutBatch {
            globals: Some(sample_globals(480.0)),
            projects: vec![("proj-del".into(), ProjectLayoutOp::Delete)],
        });

        drain.drain();
        drop(writer);

        let globals = store.load_globals();
        assert_eq!(globals.right_drawer_width, Some(480.0));
        let projects = store.load_project_layouts();
        assert!(projects.contains_key("proj-keep"));
        assert!(!projects.contains_key("proj-del"));

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
