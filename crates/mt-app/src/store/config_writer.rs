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
//!
//! # 启动那一代库备份也在这条线程上
//!
//! `config.db.bak` 是「每启动留一代」的保险:留住的必须是**本次运行改动之前**的
//! 那一代,否则一次把配置写坏的保存会连备份一起写坏。它原本在
//! `ConfigStore::load` 里同步做(首帧前主线程上多一次 SQLite backup + fsync),
//! 现在是写线程起来后的**第一件活**([`ConfigWriter::spawn`] 的 `backup_first`):
//!
//! - 启动后 config.db 的写入只有一个入口 —— 本线程的 `ConfigStore::save`
//!   (`load` 期间的迁移 / 密码封存回写发生在加载那一刻、本就在备份之前,与原来
//!   同序);单线程顺序执行,排在备份后面的保存不可能先落盘;
//! - 备份期间入队的保存只是在队列里等着;挂着一个伪代数的 `in_flight`,退出排干
//!   同样会等备份做完;
//! - 写线程起不来时退回主线程当场同步备份,仍在任何一次保存之前。
//!
//! # 写盘失败要让用户看见
//!
//! 搬到后台之后,写失败(盘满 / 权限 / 杀软锁库)原先只在 stderr 留一行 —— 用户
//! 照常改设置,重启后全没了,事前毫无征兆。现在每次失败都经 channel
//! ([`ConfigWriter::spawn`] 返回的那一头)交回主线程,由 `AppStore` 推 toast;
//! **写线程里绝不碰 GPUI**。这类故障往往让之后每一次保存都失败,主线程那头用
//! [`FailureThrottle`] 去重:同一类 [`FAILURE_NOTICE_INTERVAL`] 内只提示一次。

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use mt_config::{AppConfig, ConfigStore, SaveError};

/// 排干的兜底上限。写线程若卡在一次不返回的 fsync 上(网络盘掉线),
/// 宁可丢这一次保存也不能把退出流程永久吊死。
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// 同一类写盘失败多久只提示一次。
pub(super) const FAILURE_NOTICE_INTERVAL: Duration = Duration::from_secs(60);

/// 写盘失败的类别 —— 去重的粒度。令牌过期不在其列:那是设计内的跳过
/// (见 [`write_once`]),不是故障。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum FailureKind {
    Serialize,
    Io,
    Db,
}

/// 一次写盘失败,由写线程交回主线程。
#[derive(Debug)]
pub(super) struct SaveFailure {
    pub(super) kind: FailureKind,
    /// 可直接展示的原因(已去掉 `SaveError` 的「配置写盘失败:」前缀,
    /// toast 正文自己会说)。
    pub(super) detail: String,
}

impl SaveFailure {
    /// 该不该报给用户、报什么。`StaleToken` 返回 `None`。
    fn from_error(err: &SaveError) -> Option<Self> {
        let (kind, detail) = match err {
            SaveError::StaleToken { .. } => return None,
            SaveError::Serialize(e) => (FailureKind::Serialize, e.to_string()),
            SaveError::Io(e) => (FailureKind::Io, e.to_string()),
            SaveError::Db(e) => (FailureKind::Db, format!("{e:#}")),
        };
        Some(Self { kind, detail })
    }
}

/// 失败提示的去重闸(主线程持有)。**纯状态**,单测直接驱动。
#[derive(Default)]
pub(super) struct FailureThrottle {
    last_shown: HashMap<FailureKind, Instant>,
}

impl FailureThrottle {
    /// 这一类失败现在该不该提示。放行即记下时间;窗口内同类再来一律压掉。
    /// 不同类互不影响 —— 库写失败压不住随后冒出来的另一种故障。
    pub(super) fn admit(&mut self, kind: FailureKind, now: Instant) -> bool {
        if let Some(last) = self.last_shown.get(&kind)
            && now.saturating_duration_since(*last) < FAILURE_NOTICE_INTERVAL
        {
            return false;
        }
        self.last_shown.insert(kind, now);
        true
    }
}

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
    /// 写盘失败的回报口(写线程 → 主线程)。
    failures: UnboundedSender<SaveFailure>,
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
                tracing::warn!(
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
    ///
    /// 返回的接收端是写盘失败的回报口(见模块注释「写盘失败要让用户看见」),
    /// 由主线程泵着推 toast。
    ///
    /// `backup_first`:先给 config.db 留这一代备份,再开始处理保存(见模块注释
    /// 「启动那一代库备份也在这条线程上」)。配置没加载成功时传 `false`。
    pub(super) fn spawn(
        store: Arc<ConfigStore>,
        backup_first: bool,
    ) -> (Self, UnboundedReceiver<SaveFailure>) {
        let (failures, failures_rx) = mpsc::unbounded();
        let mut queue = Queue::new();
        if backup_first {
            // 备份占着一个伪代数的「在写」:排干据此等它做完
            queue.in_flight = Some(BACKUP_GENERATION);
        }
        let shared = Arc::new(Shared {
            queue: Mutex::new(queue),
            idle_or_work: Condvar::new(),
            failures,
        });
        let worker = shared.clone();
        let worker_store = store.clone();
        let handle = std::thread::Builder::new()
            .name("mt-config-writer".into())
            .spawn(move || run(&worker, &worker_store, backup_first))
            .map_err(|err| {
                // 起不了线程(句柄耗尽)极罕见。此时 `enqueue` 退化成主线程同步写,
                // 卡顿回到改造前的样子 —— 但绝不能因此不落盘。
                tracing::error!("[config] 配置写线程起不来({err}),本次运行退回主线程同步写盘");
                err
            })
            .ok();
        if handle.is_none() && backup_first {
            // 没有写线程:当场同步备份,仍排在任何一次保存之前
            store.backup_db();
            shared.lock().finish(BACKUP_GENERATION);
        }
        (Self { shared, handle }, failures_rx)
    }

    /// 入队一份快照。**不阻塞**(一次加锁 + 一次唤醒)。
    ///
    /// 写线程起不来时退化成当场同步写 —— 语义不变,只是卡顿回到改造前。
    pub(super) fn enqueue(&self, store: &ConfigStore, token: u64, config: AppConfig) {
        let snapshot = Snapshot { token, config };
        if self.handle.is_none() {
            write_once(store, &snapshot, &self.shared.failures);
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

/// 启动备份占用的伪代数。真实代数从 1 起(`Queue::push` 先加一),0 不会撞上;
/// `finish(0)` 也不会把 `written` 往回拉。
const BACKUP_GENERATION: u64 = 0;

/// 写线程主循环。`backup_first` 时先留这一代库备份,做完才开始取保存。
fn run(shared: &Arc<Shared>, store: &ConfigStore, backup_first: bool) {
    if backup_first {
        store.backup_db();
        {
            let mut queue = shared.lock();
            queue.finish(BACKUP_GENERATION);
        }
        // 唤醒可能正在排干的退出钩子
        shared.idle_or_work.notify_all();
    }
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
        write_once(store, &snapshot, &shared.failures);
        {
            let mut queue = shared.lock();
            queue.finish(generation);
        }
        // 唤醒可能正在排干的退出钩子
        shared.idle_or_work.notify_all();
    }
}

/// 真正碰磁盘的那一下。失败在 stderr 留痕,并经 `failures` 交回主线程推 toast
/// (令牌过期除外,理由见下)。发送失败(主线程那头已经没了,退出排干时)
/// 无所谓 —— stderr 已经留过痕。
fn write_once(store: &ConfigStore, snapshot: &Snapshot, failures: &UnboundedSender<SaveFailure>) {
    match store.save(snapshot.token, &snapshot.config) {
        Ok(()) => {}
        Err(SaveError::StaleToken { provided, current }) => {
            // 入队时令牌是对的,写的时候不对了 = 这中间发生过一次重新 load,
            // 手上这份快照是基于旧配置改的。**跳过是唯一正确的处置**:重试会
            // 拿陈旧内容盖掉刚读进来的那份。单进程壳里走不到这条(load 只在
            // 启动与令牌过期重试时发生,两处都在主线程)。
            tracing::warn!("[config] 配置快照已过期(token {provided} != {current}),本次跳过不覆盖");
        }
        Err(err) => {
            tracing::error!("[store] 配置保存失败: {err}");
            if let Some(failure) = SaveFailure::from_error(&err) {
                let _ = failures.unbounded_send(failure);
            }
        }
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

        let (writer, mut failures) = ConfigWriter::spawn(store.clone(), false);
        for size in [14.0_f64, 15.0, 16.0, 17.0] {
            let mut config = AppConfig::default();
            config.ui_font_size = size;
            writer.enqueue(&store, token, config);
        }
        writer.drain_handle().drain();

        let back = store.read();
        assert_eq!(back.ui_font_size, 17.0, "落盘的是最后入队的那一份");
        assert!(
            failures.try_recv().is_err(),
            "写成功不该往主线程回报任何失败"
        );

        drop(writer);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 启动备份挪到写线程之后,「备份留的是本次运行改动之前那一代」这条语义不能丢:
    /// 写线程一起来就入队的保存,也必须排在备份后面落盘;排干要等备份做完。
    #[test]
    fn 启动备份先于任何一次保存() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mt-config-writer-backup-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(ConfigStore::at(dir.join("config.json")));
        // 第一次启动:建库,落一份字号 13 的配置
        let token = store.load().expect("首次 load 建空库").token;
        let initial = AppConfig {
            ui_font_size: 13.0,
            ..Default::default()
        };
        write_once(
            &store,
            &Snapshot {
                token,
                config: initial,
            },
            &mpsc::unbounded().0,
        );
        std::fs::remove_file(dir.join("config.db.bak")).ok();

        // 第二次启动:加载不备份,写线程一起来就入队一次保存
        let store = Arc::new(ConfigStore::at(dir.join("config.json")));
        let token = store.load_without_backup().expect("第二次 load").token;
        let (writer, _failures) = ConfigWriter::spawn(store.clone(), true);
        let changed = AppConfig {
            ui_font_size: 19.0,
            ..Default::default()
        };
        writer.enqueue(&store, token, changed);
        writer.drain_handle().drain();

        assert_eq!(store.read().ui_font_size, 19.0, "保存照常落盘");
        // 备份里必须是改动之前的那一代:拷到另一个目录当主库读回来
        let bak_dir = dir.join("bak-check");
        std::fs::create_dir_all(&bak_dir).unwrap();
        std::fs::copy(dir.join("config.db.bak"), bak_dir.join("config.db"))
            .expect("排干之后备份必然已经在了");
        let from_bak = ConfigStore::at(bak_dir.join("config.json")).read();
        assert_eq!(from_bak.ui_font_size, 13.0, "备份早于本次运行的第一次写入");

        drop(writer);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 备份在写的时候排干不能放行(退出钩子等它做完)。
    #[test]
    fn 排干等启动备份做完() {
        let mut queue = Queue::new();
        queue.in_flight = Some(BACKUP_GENERATION);
        assert!(!queue.is_idle(), "备份还在做");
        queue.push(snapshot(1));
        assert!(
            queue.take().is_some(),
            "队列本身照常取(写线程保证先做完备份再取)"
        );
        queue.finish(1);
        queue.finish(BACKUP_GENERATION);
        assert_eq!(queue.written, 1, "伪代数不把水位往回拉");
    }

    /// 去重:同一类失败在窗口内只放行第一次,窗口一过再放行一次。
    #[test]
    fn 同类失败窗口内只提示一次() {
        let mut throttle = FailureThrottle::default();
        let t0 = Instant::now();
        assert!(throttle.admit(FailureKind::Db, t0), "第一次必须提示");
        assert!(!throttle.admit(FailureKind::Db, t0 + Duration::from_secs(1)));
        assert!(!throttle.admit(
            FailureKind::Db,
            t0 + FAILURE_NOTICE_INTERVAL - Duration::from_millis(1)
        ));
        assert!(
            throttle.admit(FailureKind::Db, t0 + FAILURE_NOTICE_INTERVAL),
            "窗口过了要再提示一次 —— 故障还在,用户得知道"
        );
        // 窗口从**上次放行**起算,不是从第一次
        assert!(!throttle.admit(
            FailureKind::Db,
            t0 + FAILURE_NOTICE_INTERVAL + Duration::from_secs(30)
        ));
    }

    /// 不同类互不压制:库写失败之后冒出来的写盘故障照样要提示。
    #[test]
    fn 不同类失败互不压制() {
        let mut throttle = FailureThrottle::default();
        let t0 = Instant::now();
        assert!(throttle.admit(FailureKind::Db, t0));
        assert!(throttle.admit(FailureKind::Io, t0));
        assert!(throttle.admit(FailureKind::Serialize, t0));
        assert!(!throttle.admit(FailureKind::Io, t0 + Duration::from_secs(5)));
    }

    /// 令牌过期是设计内的跳过,不是故障 —— 不报给用户;其余三类照实归类,
    /// 正文去掉 `SaveError` 自带的前缀(toast 正文自己会说「没能写入磁盘」)。
    #[test]
    fn 令牌过期不回报且其余失败照实归类() {
        let stale = SaveError::StaleToken {
            provided: 1,
            current: 2,
        };
        assert!(SaveFailure::from_error(&stale).is_none());

        let io = SaveFailure::from_error(&SaveError::Io(std::io::Error::other("磁盘已满")))
            .expect("写盘失败要回报");
        assert_eq!(io.kind, FailureKind::Io);
        assert_eq!(io.detail, "磁盘已满");

        let db = SaveFailure::from_error(&SaveError::Db(
            anyhow::anyhow!("database is locked").context("写 settings 失败"),
        ))
        .expect("库写失败要回报");
        assert_eq!(db.kind, FailureKind::Db);
        assert!(
            db.detail.contains("写 settings 失败") && db.detail.contains("database is locked"),
            "带上整条原因链: {}",
            db.detail
        );
    }

    /// 端到端:令牌对不上的快照被跳过,channel 里什么都没有。
    #[test]
    fn 过期快照被跳过且不回报() {
        let dir = std::env::temp_dir().join(format!(
            "mt-config-writer-stale-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = ConfigStore::at(dir.join("config.json"));
        let token = store.load().expect("首次 load 建空库").token;
        let (tx, mut rx) = mpsc::unbounded();
        write_once(&store, &snapshot(token + 1), &tx);
        assert!(rx.try_recv().is_err(), "令牌过期不是故障,不该弹给用户");
        std::fs::remove_dir_all(&dir).ok();
    }
}
