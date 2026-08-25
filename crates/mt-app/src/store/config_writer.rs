//! 配置落盘的**单写者后台线程**。
//!
//! # 为什么需要它
//!
//! `AppStore::save_config_now` 原本是一条同步链,整条都跑在 GPUI 主线程上:
//!
//! ```text
//! flush_layout_now(layout.db 一行 upsert)
//!   → ConfigStore::save
//!       → ConfigDb::save    整个 SQLite 事务(settings 逐键 upsert + stale 全表扫
//!                           + projects/sshConnections 逐行 upsert),且库开了
//!                           `synchronous=FULL`
//!       → write_ssh_projection  读整份 config.json 做内容比对 + 原子写 + fsync
//! ```
//!
//! 它有十来个调用点直接挂在 UI 事件处理器上(SSH 连接 CRUD、启动器名单、项目
//! 环境变量、worktree 建项目……)。慢盘 / 网络盘 / 杀软扫描时那次 fsync 是几百
//! 毫秒,主线程就卡几百毫秒 —— 终端不刷、键入不回显。
//!
//! # 语义必须保住的那一条:入队即快照
//!
//! 这些调用点当初刻意选的是 `save_config_now` 而不是 500ms 防抖的
//! `save_config_soon`,理由写在各自的注释里(「密码/私钥路径这类东西不该在防抖
//! 窗口里被一次崩溃吃掉」)。所以搬到后台**不能**顺手变成又一个防抖:
//!
//! - **入队即快照**:调用方在主线程上把 `AppConfig` 克隆一份交出去,此后它再改
//!   自己那份也不影响已入队的内容;
//! - **落盘窗口是毫秒级**:写线程一直阻塞在条件变量上,入队即被唤醒,不等任何
//!   计时器 —— 风险窗口从「500ms 的防抖窗」缩回「一次线程唤醒 + 一次事务」。
//!
//! # 折叠(coalescing)为什么合法
//!
//! 队列里只留**最新的一份**快照:后来者直接盖掉还没轮到的前一份。这一步合法
//! 的前提是 `ConfigStore::save` 的契约是「拿一份完整配置写下去」(整份 API、
//! 行级落盘,见 `mt_config::db` 的模块注释)—— 全量快照下,写 A 再写 B 与只写 B
//! 的磁盘终态逐字节相同。
//!
//! ⚠️ **哪天 `save` 变成增量 patch,这个折叠就立刻不成立**。改那边之前先回来看
//! 这一段。
//!
//! # 顺序保证
//!
//! 单线程 + 一次只取一份 + 取走到写完期间 `in_flight` 挂着,于是:
//!
//! - 绝不会有两次写交错(SQLite 那边虽有 `Mutex<Connection>` 兜底,但投影文件
//!   那次 `read + atomic_write` 没有锁,交错会写出撕裂的 config.json);
//! - 绝不会乱序 —— 代数单调递增,晚入队的必然后落盘。
//!
//! # 退出排干
//!
//! [`ConfigWriter::drain`] 阻塞到「队列空且没有在写」。挂点见
//! `AppStore::new` 里注册的 `on_app_quit`:gpui 的 `App::shutdown` 是**先把所有
//! 退出观察者的函数体跑完、收齐它们返回的 future,再统一 await**,所以哪怕本
//! 观察者比 `main.rs` 那个先注册,轮到我们的 future 被 poll 时,`main.rs` 在函数
//! 体里补的最后一次 `save_config_now()` 也已经入队了。
//!
//! (gpui 给 await 阶段设了 100ms 上限,但 `block_with_timeout` 是在**当前线程**
//! 上 poll 的:我们的 future 首次 poll 里就把排干做完再返回 `Ready`,超时无从
//! 插手 —— 与改造前「退出时同步写完」的行为一致。)

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mt_config::{AppConfig, ConfigStore, SaveError};

/// 排干的兜底上限。写线程若卡在一次不返回的 fsync 上(网络盘掉线),
/// 宁可丢这一次保存也不能把退出流程永久吊死。
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// 一份待落盘的完整配置快照。
struct Snapshot {
    token: u64,
    config: AppConfig,
}

/// 队列状态机。**与线程无关的纯状态**,单测直接驱动它(见文件末尾)。
struct Queue {
    /// 还没轮到的那一份;后来者盖前者 —— 折叠合法性见模块注释。
    pending: Option<Snapshot>,
    /// 入队代数,单调递增。
    queued: u64,
    /// 已落盘(或已放弃)的最高代数。
    written: u64,
    /// 正在写的那一代;`None` = 写线程空闲。
    in_flight: Option<u64>,
    /// 真正碰过磁盘的次数 —— 只为单测看得见「折叠掉了几代」。
    writes: u64,
    /// 收摊信号(`Drop` 里置位)。
    stop: bool,
}

impl Queue {
    fn new() -> Self {
        Self {
            pending: None,
            queued: 0,
            written: 0,
            in_flight: None,
            writes: 0,
            stop: false,
        }
    }

    /// 入队一份快照,返回它的代数。已有待写快照时**直接盖掉**(折叠)。
    fn push(&mut self, snapshot: Snapshot) -> u64 {
        self.queued += 1;
        self.pending = Some(snapshot);
        self.queued
    }

    /// 取走待写的那一份。取走即进入 `in_flight`,排干据此判「还没写完」。
    fn take(&mut self) -> Option<(u64, Snapshot)> {
        let snapshot = self.pending.take()?;
        let generation = self.queued;
        self.in_flight = Some(generation);
        self.writes += 1;
        Some((generation, snapshot))
    }

    /// 一代写完(成功或失败都算落停 —— 失败只留痕,不重试)。
    fn finish(&mut self, generation: u64) {
        self.written = self.written.max(generation);
        self.in_flight = None;
    }

    /// 队列空且没有在写。
    fn is_idle(&self) -> bool {
        self.pending.is_none() && self.in_flight.is_none()
    }
}

/// 主线程与写线程共享的那一份。
struct Shared {
    queue: Mutex<Queue>,
    /// 两个方向都用它:入队唤醒写线程、写完唤醒排干的人。
    idle_or_work: Condvar,
}

impl Shared {
    /// 锁中毒(写线程在持锁期间 panic)时照旧拿数据继续用 ——
    /// 队列本身没有会被半途改坏的不变量,而「存不下就不让用」是本仓的红线。
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 阻塞到队列空且没有在写。带兜底上限,超时只留痕不吊死。
    fn drain(&self) {
        let deadline = Instant::now() + DRAIN_DEADLINE;
        let mut queue = self.lock();
        while !queue.is_idle() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                eprintln!(
                    "[config] 等配置写盘超过 {}s 仍未收摊,本次放弃排干(最后一次改动可能没落盘)",
                    DRAIN_DEADLINE.as_secs()
                );
                return;
            };
            let (next, _) = self
                .idle_or_work
                .wait_timeout(queue, left)
                .unwrap_or_else(|e| e.into_inner());
            queue = next;
        }
    }
}

/// 配置落盘的入口:主线程只管入队,写盘在自己的线程上。
///
/// 一个 `AppStore` 一份,随 store 一起活到进程结束。
pub(super) struct ConfigWriter {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl ConfigWriter {
    /// 起写线程。`store` 走 `Arc` 与主线程共用同一个实例 ——
    /// 令牌(`AtomicU64`)与库句柄(`Mutex<Option<Arc<ConfigDb>>>`)都在它身上,
    /// 两侧看到的必须是同一份。
    pub(super) fn spawn(store: Arc<ConfigStore>) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::new()),
            idle_or_work: Condvar::new(),
        });
        let worker = shared.clone();
        let handle = std::thread::Builder::new()
            .name("mt-config-writer".into())
            .spawn(move || run(&worker, &store))
            .map_err(|err| {
                // 起不了线程(句柄耗尽)极罕见。此时 `enqueue` 退化成主线程同步写,
                // 卡顿回到改造前的样子 —— 但绝不能因此不落盘。
                eprintln!("[config] 配置写线程起不来({err}),本次运行退回主线程同步写盘");
                err
            })
            .ok();
        Self { shared, handle }
    }

    /// 入队一份快照。**不阻塞**(一次加锁 + 一次唤醒)。
    ///
    /// 写线程起不来时退化成当场同步写 —— 语义不变,只是卡顿回到改造前。
    pub(super) fn enqueue(&self, store: &ConfigStore, token: u64, config: AppConfig) {
        let snapshot = Snapshot { token, config };
        if self.handle.is_none() {
            write_once(store, &snapshot);
            return;
        }
        {
            let mut queue = self.shared.lock();
            queue.push(snapshot);
        }
        self.shared.idle_or_work.notify_all();
    }

    /// 给退出钩子用的排干句柄(`on_app_quit` 的闭包要 `'static`,不能借 store)。
    pub(super) fn drain_handle(&self) -> DrainHandle {
        DrainHandle(self.shared.clone())
    }
}

impl Drop for ConfigWriter {
    /// 兜底一道:先排干(`stop` 只在队列空之后才生效,待写的那份不会被丢),
    /// 再收线程。正常退出走的是 `on_app_quit` 那条,这里管的是「store 被正常
    /// drop」的场合(测试、以及将来若有多实例)。
    fn drop(&mut self) {
        self.shared.drain();
        {
            let mut queue = self.shared.lock();
            queue.stop = true;
        }
        self.shared.idle_or_work.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 退出钩子手上的那一份。克隆自 [`ConfigWriter::drain_handle`]。
#[derive(Clone)]
pub(super) struct DrainHandle(Arc<Shared>);

impl DrainHandle {
    /// 阻塞到 pending 快照全部落盘。
    pub(super) fn drain(&self) {
        self.0.drain();
    }
}

/// 写线程主循环。
fn run(shared: &Arc<Shared>, store: &ConfigStore) {
    loop {
        let job = {
            let mut queue = shared.lock();
            loop {
                if let Some(job) = queue.take() {
                    break Some(job);
                }
                // `stop` 排在 `take` 之后:收摊时待写的那一份仍要落盘
                if queue.stop {
                    break None;
                }
                queue = shared
                    .idle_or_work
                    .wait(queue)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some((generation, snapshot)) = job else {
            return;
        };
        write_once(store, &snapshot);
        {
            let mut queue = shared.lock();
            queue.finish(generation);
        }
        // 唤醒可能正在排干的退出钩子
        shared.idle_or_work.notify_all();
    }
}

/// 真正碰磁盘的那一下。失败只在 stderr 留痕 —— 与本壳其它每一处配置写入同一
/// 口径(见 `env_vars.rs` 文件头那条取舍)。
fn write_once(store: &ConfigStore, snapshot: &Snapshot) {
    match store.save(snapshot.token, &snapshot.config) {
        Ok(()) => {}
        Err(SaveError::StaleToken { provided, current }) => {
            // 入队时令牌是对的,写的时候不对了 = 这中间发生过一次重新 load,
            // 手上这份快照是基于旧配置改的。**跳过是唯一正确的处置**:重试会
            // 拿陈旧内容盖掉刚读进来的那份。单进程壳里走不到这条(load 只在
            // 启动与令牌过期重试时发生,两处都在主线程)。
            eprintln!(
                "[config] 配置快照已过期(token {provided} != {current}),本次跳过不覆盖"
            );
        }
        Err(err) => eprintln!("[store] 配置保存失败: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(token: u64) -> Snapshot {
        Snapshot {
            token,
            config: AppConfig::default(),
        }
    }

    /// 折叠:连续入队三份,写线程只会取到**最后**那一份,前两代直接被盖掉。
    ///
    /// 这条是整个改造的核心取舍 —— 合法性依赖「`save` 是全量快照」,
    /// 见模块注释里那条 ⚠️。
    #[test]
    fn 连续入队折叠成最新一份() {
        let mut queue = Queue::new();
        queue.push(snapshot(11));
        queue.push(snapshot(22));
        queue.push(snapshot(33));
        assert_eq!(queue.queued, 3, "三次入队都记了代数");

        let (generation, taken) = queue.take().expect("有待写快照");
        assert_eq!(generation, 3);
        assert_eq!(taken.token, 33, "落盘的是最后入队的那一份");
        assert_eq!(queue.writes, 1, "前两代被折叠掉,只碰一次磁盘");
        assert!(queue.take().is_none(), "取走之后队列就空了");
    }

    /// 代数单调递增,`finish` 只往前推不回退 —— 「晚入队的必然后落盘」靠它。
    #[test]
    fn 代数单调递增且不回退() {
        let mut queue = Queue::new();
        queue.push(snapshot(1));
        let (first, _) = queue.take().unwrap();
        queue.finish(first);
        assert_eq!(queue.written, 1);

        queue.push(snapshot(2));
        queue.push(snapshot(3));
        let (second, _) = queue.take().unwrap();
        assert_eq!(second, 3, "代数按入队次数走,不按落盘次数");
        queue.finish(second);
        assert_eq!(queue.written, 3);

        // 迟到的旧代数不该把水位拉回去
        queue.finish(1);
        assert_eq!(queue.written, 3, "written 只进不退");
    }

    /// 排干的判据:**取走还没写完**也算「没排干」——
    /// 只看 `pending` 是空的会让退出钩子在事务跑一半时放行。
    #[test]
    fn 排干判据覆盖在写的那一份() {
        let mut queue = Queue::new();
        assert!(queue.is_idle(), "刚建出来就是空闲");

        queue.push(snapshot(1));
        assert!(!queue.is_idle(), "有待写快照");

        let (generation, _) = queue.take().unwrap();
        assert!(
            !queue.is_idle(),
            "已取走但还没写完 —— 这一刻放行退出就会丢这次保存"
        );

        queue.finish(generation);
        assert!(queue.is_idle(), "写完才算排干");
    }

    /// 收摊信号不吃掉待写的那一份:`run` 的循环先 `take` 后看 `stop`。
    #[test]
    fn 收摊时待写快照仍会落盘() {
        let mut queue = Queue::new();
        queue.push(snapshot(7));
        queue.stop = true;
        let (_, taken) = queue.take().expect("stop 不该吃掉待写的快照");
        assert_eq!(taken.token, 7);
    }

    /// 端到端:真起写线程,连续入队后排干,磁盘上必须是**最后**那一份。
    ///
    /// 顺带钉住「排干返回时写线程确实收摊了」——`drain` 之后立刻读盘就该拿到
    /// 终态,不需要 sleep。
    #[test]
    fn 写线程按最后一份落盘且排干后可读() {
        let dir = std::env::temp_dir().join(format!(
            "mt-config-writer-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(ConfigStore::at(dir.join("config.json")));
        // 令牌只有 load 过才发放 —— 没有它 `save` 一律拒绝(这条红线不能绕)
        let token = store.load().expect("首次 load 建空库").token;

        let writer = ConfigWriter::spawn(store.clone());
        for size in [14.0_f64, 15.0, 16.0, 17.0] {
            let mut config = AppConfig::default();
            config.ui_font_size = size;
            writer.enqueue(&store, token, config);
        }
        writer.drain_handle().drain();

        let back = store.read();
        assert_eq!(back.ui_font_size, 17.0, "落盘的是最后入队的那一份");

        drop(writer);
        std::fs::remove_dir_all(&dir).ok();
    }
}
