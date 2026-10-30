//! 目录监听:**进程级单例** + 订阅者句柄。
//!
//! 原本是 `notify` 回调里 `emit("fs-change")` 给前端,GPUI 版改成**注入式回调**:
//! 构造 [`FsWatcher`] 时交一个 sink 进来,由上层决定怎么接(GPUI 侧典型做法是
//! sink 里往 channel 丢,主线程任务醒来再失效缓存、`cx.notify()`)。
//!
//! # 一个进程一个 notify watcher
//!
//! 此前每个被监听目录各建一个 `RecommendedWatcher`:展开 30 个目录、开 10 个页签
//! 就是约 40 个实例。Windows 后端每个实例一条线程、每 100ms 醒一次
//! (`WaitForSingleObjectEx(…, 100, …)`,合计约 400 次/秒空唤醒);Linux 每个实例
//! 占一个 inotify 实例(默认每用户上限 128)。现在全进程只有 [`Hub`] 手里那一个
//! notify watcher,[`FsWatcher`] 退化成**订阅者句柄**:watch / unwatch 记在它名下,
//! **句柄 drop 时名下全部监听一并撤销**,调用方漏了 unwatch 也不会泄漏。
//!
//! # 两层引用计数
//!
//! 1. 订阅者内按字面路径计数,归零才摘。这条不能简化 —— 文件树的压缩链让多个
//!    UI 节点可能 watch 同一路径(链中段与其真实节点),无计数时后注册者会顶掉
//!    前者、先注销者会把仍在用的一并摘除。
//! 2. 跨订阅者按目录的**物理身份**(完整 `canonicalize` 的结果)计数:后端对同一
//!    目录只注册一次,最后一个订阅撤走才真正 unwatch。不拿调用方的字面路径当后端
//!    key,是因为同一目录的两种写法(Windows 大小写不同、Linux/macOS 经符号链接)
//!    在单实例后端里会互相踩:inotify 对同一 inode 返回同一个 wd、notify 只记后
//!    注册的那个路径;FSEvents 后端按 canonicalize 结果记账,摘掉一个另一个跟着失效。
//!
//! # 分发规则
//!
//! notify 回报的是**后端形态**的路径:Windows / inotify 是「注册时交给它的目录 +
//! 子项名」(`dir.join(name)`;inotify 报目录自身被删/被移走时就是 `dir`),FSEvents
//! 报真实路径(与 canonicalize 同形)。后端一律用 canonical 形态注册,回调里对事件的
//! **每个路径** `p`:
//!
//! - `p` 的父目录是被监听目录 → 交给该目录的订阅者,路径改写成
//!   `订阅者的字面 key.join(文件名)`;
//! - `p` 本身是被监听目录(被删、被改名、被重建;Windows 上这类事件来自它父目录的
//!   监听)→ 交给该目录的订阅者,路径改写成字面 key。
//!
//! 改写后订阅者拿到的路径与旧实现逐字一致(旧实现拿字面路径注册,notify 在它上面
//! join 子项名)。`project_path` 用该订阅者注册时给的项目根 —— 同一目录可以被两个
//! 订阅者用不同项目根注册(嵌套项目、文件树与查看器同时监听),各收各的。rename
//! 带两个路径时逐个路径分发,不会把对面目录的路径串过去(旧实现是整条事件交给收到
//! 它的实例,inotify 跨目录 rename 的 Both 事件会把源路径一起带走)。同一事件里同一
//! 订阅者的同一路径只交一次。没有路径的事件(inotify 队列溢出等)与旧实现一样丢弃。
//!
//! # 注册在后台
//!
//! [`FsWatcher::watch`] / [`FsWatcher::unwatch`] / drop 只改内存里的**期望表**,不碰
//! 磁盘也不碰 notify,GPUI 主线程上调用不会卡。校验路径在项目根内
//! ([`crate::fs::verify_under_project_root`],要 canonicalize —— WSL/网络盘上每次都是
//! 往返)、求物理身份、调 notify 注册/摘除,全由单例自己的注册线程(`mt-fs-watch`)
//! 对账完成:每一轮先摘该摘的、再挂该挂的、最后解析**一个**待决订阅,一个慢盘上的
//! 注册不会把摘除压在后面太久。
//!
//! 竞态:注册还在排队或进行中时调用方已经 unwatch(折叠目录、关页签、删目录前脱挂、
//! 句柄 drop),订阅记录当场删除;注册线程回来按「订阅者 + 路径 + 代次号」认领,
//! 认不到就丢弃结果,已经挂上的后端条目在下一轮因无人引用被摘掉。最终状态只由
//! 期望表决定:没有残留,计数不会出负数。
//!
//! 注册失败(越界、目录不存在、notify 报错)由注册线程打一行
//! `[标签] 监听 X 失败: …`(标签见 [`FsWatcher::with_label`]),订阅停在「失败」态但
//! 计数照记 —— 调用方的 unwatch 照常配对;对同一路径再 watch 一次会重新排队重试。
//! 被监听目录自己被删、被改名、被重建时(见 [`reshapes_dir`]),后端那一条摘掉重挂:
//! 句柄/wd 还指着旧目录对象,不重挂的话共享这一条的所有订阅者都跟着失聪;目录已经
//! 不在就落到「失败」态(不打日志:目录被删是常事,订阅方重列清单时自会撤掉它)。
//!
//! # 注册晚到的窗口
//!
//! [`FsWatcher::watch`] 返回 [`WatchReady`],后端真正挂上(或失败、被撤)时落定。
//! 调用方在**后台线程**里先 [`WatchReady::wait`] 一小段(预算 [`WATCH_READY_BUDGET`])
//! 再列目录 / 读盘,次序就与同步时代「先挂监听、再列目录」一致,列完之后的变化一定
//! 有事件,也不用多列一遍。超出预算(注册线程被慢盘拖住)时照常往下走,那一小段
//! 窗口里的变化要等该目录下一次事件或手动刷新才看得到。
//! 没走「注册完成后补发一次让调用方重列」:列目录与注册并发时,调用方不知道自己那次
//! 读目录在注册之前还是之后,补发就得每次展开都列两遍(WSL 上是双倍的慢 IO)。
//!
//! # 死锁红线
//!
//! notify 的 `watch()` 在 Windows / inotify 上同步等后端线程回执,而后端线程正是跑
//! 事件回调的那条;FSEvents 的 watch/unwatch 要先等 runloop 回到空闲再 join 它。
//! 本模块只有一把会被回调拿的锁(`Shared::state`),规则是:
//!
//! - 注册线程只在取快照、写结果时拿它,**调 notify 的 watch/unwatch 时一定不持有**;
//!   notify watcher 本身归注册线程独占,没有第二把锁;
//! - 事件回调只在查路由那一小段拿它,**调 sink 之前放掉** —— sink 里再调本模块
//!   (watch / unwatch / drop)也不会自锁;
//! - 主线程的 watch / unwatch / drop 只拿它改内存表,既不等注册线程也不等后端线程。
//!
//! # 平台
//!
//! Linux inotify、macOS FSEvents 走同一个单例。macOS 有一笔代价:notify 7 的 FSEvents
//! 后端**每次 watch/unwatch 都停掉整条流**(`CFRunLoopStop` + join runloop 线程)再按
//! 全部路径重建(`since = Now`),重建那几毫秒里所有被监听目录的事件都可能丢,每次
//! 注册的开销也随路径数线性增长;旧实现一目录一条流,互不打扰。换来的是 N 条 runloop
//! 线程降成 1 条。丢事件窗口与上面「注册晚到」同一量级,不另做处理。

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use notify::event::ModifyKind;
use notify::{Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::{Condvar, Mutex};

/// 一次文件系统变更。`kind` 沿用原实现的 `format!("{:?}", event.kind)`
/// 调试字符串形态 —— 上层只用它区分「创建/删除/修改」做重扫决策。
#[derive(Debug, Clone)]
pub struct FsChange {
    /// 触发监听时登记的项目路径,用来把变更路由回对应项目的文件树。
    pub project_path: String,
    /// 变更路径,形态是订阅者注册时的字面目录 + 子项名(见模块注释「分发规则」)。
    pub path: PathBuf,
    pub kind: String,
    /// `path` 这一项**本身**出现或消失了:建、删、改名(走了 / 来了)。只是内容或
    /// 元数据变了为 `false`。判据与后端重挂同一个([`reshapes_dir`]),订阅方拿它
    /// 认「我缓存着的那个目录是不是已经换了一个」,不必再去解析 `kind` 的调试串。
    pub structural: bool,
}

/// 变更 sink。在 notify 后端线程上调用,故要求 `Send + Sync`;调用时不持有本模块的锁。
pub type FsChangeSink = Arc<dyn Fn(FsChange) + Send + Sync>;

/// 后台列目录 / 读盘前等监听挂上的预算(见模块注释「注册晚到的窗口」)。
///
/// 本地盘一次注册是亚毫秒到毫秒级,这个数只在注册线程被慢盘(WSL/网络盘上每次
/// canonicalize 都是往返)拖住、排着队时才会等满;等满就照常往下走,不无限等。
pub const WATCH_READY_BUDGET: Duration = Duration::from_millis(500);

/// 一次 watch 的落定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchOutcome {
    /// 后端已挂上,此后该目录的变化一定有事件。
    Active,
    /// 注册失败(原因原文,注册线程已打过日志)。
    Failed(String),
    /// 落定前就被 unwatch / 句柄 drop 了。
    Cancelled,
}

/// [`FsWatcher::watch`] 的回执:后端真正挂上、失败或被撤时落定。只落定一次。
#[derive(Clone)]
pub struct WatchReady(Arc<ReadySignal>);

struct ReadySignal {
    outcome: Mutex<Option<WatchOutcome>>,
    cv: Condvar,
}

impl ReadySignal {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    /// 只认第一次落定。调用方可能正持着状态锁 —— 锁序固定是「状态锁 → 回执锁」,
    /// 等待方([`WatchReady::wait`])只拿回执锁,不会反过来。
    fn settle(&self, outcome: WatchOutcome) {
        let mut slot = self.outcome.lock();
        if slot.is_none() {
            *slot = Some(outcome);
            self.cv.notify_all();
        }
    }
}

impl WatchReady {
    fn settled(outcome: WatchOutcome) -> Self {
        let signal = ReadySignal::new();
        signal.settle(outcome);
        Self(signal)
    }

    /// 不等,看一眼现状(`None` = 还在排队或注册中)。
    pub fn outcome(&self) -> Option<WatchOutcome> {
        self.0.outcome.lock().clone()
    }

    /// 阻塞等落定,最多 `timeout`;超时返回 `None`。**别在 UI 线程上调**。
    pub fn wait(&self, timeout: Duration) -> Option<WatchOutcome> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.0.outcome.lock();
        while slot.is_none() {
            if self.0.cv.wait_until(&mut slot, deadline).timed_out() {
                break;
            }
        }
        slot.clone()
    }
}

/// 订阅者句柄(见模块注释)。drop 时撤销名下全部监听。
pub struct FsWatcher {
    hub: Arc<Hub>,
    id: SubId,
}

impl FsWatcher {
    /// 向进程级单例登记一个订阅者。
    pub fn new<F>(sink: F) -> Self
    where
        F: Fn(FsChange) + Send + Sync + 'static,
    {
        Self::with_label("watch", sink)
    }

    /// 同 [`Self::new`],注册失败的日志行以 `[label]` 开头(文件树 `files`、
    /// 文件查看器 `file-viewer`),与各自模块的其它日志同一个前缀。
    pub fn with_label<F>(label: &'static str, sink: F) -> Self
    where
        F: Fn(FsChange) + Send + Sync + 'static,
    {
        Hub::global().subscribe(label, Arc::new(sink))
    }

    /// 用 mpsc 通道接变更(测试与「自己起消费线程」的调用方用)。
    /// 通道断开后 sink 变成空操作,不影响监听本身。
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

    /// 开始监听目录(非递归)。**只登记期望、立即返回**,校验与注册在注册线程里做
    /// (见模块注释「注册在后台」);要保证「此后的变化一定有事件」就在后台线程里
    /// 先等返回的 [`WatchReady`]。
    ///
    /// 同一路径重复调用只递增引用计数。`path` 必须在 `project_path` 之内:与 `fs.rs`
    /// 各入口共用 [`crate::fs::verify_under_project_root`] 这一把尺子(canonicalize 解
    /// `..` 与符号链接),属纵深防御 —— 挡住将来有人拿拖放/输入框来的路径直接开监听。
    /// WSL UNC(`\\wsl.localhost\...`)项目走同一条路。校验不过、目录不存在或 notify
    /// 报错时落「失败」态并打日志,计数照记;对失败的路径再 watch 一次会重试。
    pub fn watch(&self, path: &Path, project_path: &str) -> WatchReady {
        self.hub.watch(self.id, path, project_path)
    }

    /// 引用计数 -1,归零时撤销(后端那一条在无人引用后由注册线程摘除)。
    /// 未被监听的路径是空操作。`path` 要与 watch 时的字面路径同形。
    pub fn unwatch(&self, path: &Path) {
        self.hub.unwatch(self.id, path);
    }

    /// 名下持有且未失败的路径数(排队中的也算;不是引用计数之和)。
    pub fn watched_count(&self) -> usize {
        self.hub.watched_count(self.id)
    }

    /// 这条路径的最近一次注册是否失败了(未监听的路径返回 `false`)。
    pub fn is_failed(&self, path: &Path) -> bool {
        self.hub.is_failed(self.id, path)
    }
}

impl Drop for FsWatcher {
    fn drop(&mut self) {
        self.hub.unsubscribe(self.id);
    }
}

// ─── 单例 ─────────────────────────────────────────────────

type SubId = u64;

/// 一个订阅对一个后端目录的路由:事件改写回 `key` 的形态、带上 `project_path`。
struct Route {
    sub: SubId,
    key: PathBuf,
    project_path: String,
}

enum Phase {
    /// 等注册线程校验 + 求物理身份。
    Pending(Arc<ReadySignal>),
    /// 身份已知、路由已挂,等后端注册 `canonical`。
    Resolved {
        canonical: PathBuf,
        ready: Arc<ReadySignal>,
    },
    /// 后端已注册。
    Active { canonical: PathBuf },
    /// 注册失败;计数照记,再 watch 一次重试。
    Failed,
}

struct WatchEntry {
    count: usize,
    project_path: String,
    /// 每次进入 Pending 换一个新号;注册线程回来按号认领,认不到就丢弃结果。
    generation: u64,
    phase: Phase,
}

struct Subscriber {
    label: &'static str,
    sink: FsChangeSink,
    /// 字面路径 → 订阅。
    watches: HashMap<PathBuf, WatchEntry>,
}

#[derive(Default)]
struct State {
    subs: HashMap<SubId, Subscriber>,
    next_sub: SubId,
    next_generation: u64,
    /// canonical 目录 → 路由。**只有 Resolved / Active 的订阅有路由**,所以
    /// 键集合就是「后端应当注册着的目录」。
    routes: HashMap<PathBuf, Vec<Route>>,
    /// 后端实际注册着的目录。只有注册线程改它(调完 notify 回来记账)。
    registered: HashSet<PathBuf>,
    /// 被监听目录自己被删/改名/重建,要摘掉重挂的(事件回调记,注册线程消费)。
    stale: HashSet<PathBuf>,
    /// 期望表变了、注册线程该醒。
    dirty: bool,
    /// 注册线程手上没活、在睡(测试等它收敛用)。
    idle: bool,
    shutdown: bool,
    /// 注册线程没起来时的原因;此后 watch 一律直接失败。
    dead: Option<String>,
    /// 创建过几个 notify watcher(单例口径的断言用)。
    backends_created: usize,
}

impl State {
    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.idle = false;
    }

    /// 撤掉一条订阅:落定回执、摘路由。后端那一条等注册线程对账时摘。
    fn release(&mut self, sub: SubId, key: &Path, entry: WatchEntry) {
        match entry.phase {
            Phase::Pending(ready) => ready.settle(WatchOutcome::Cancelled),
            Phase::Resolved { canonical, ready } => {
                self.drop_route(&canonical, sub, key);
                ready.settle(WatchOutcome::Cancelled);
            }
            Phase::Active { canonical } => self.drop_route(&canonical, sub, key),
            Phase::Failed => {}
        }
    }

    fn drop_route(&mut self, canonical: &Path, sub: SubId, key: &Path) {
        if let Some(routes) = self.routes.get_mut(canonical) {
            routes.retain(|route| !(route.sub == sub && route.key == key));
            if routes.is_empty() {
                self.routes.remove(canonical);
            }
        }
    }

    /// 最早排队的待决订阅(按代次号,先来先办)。
    fn next_pending(&self) -> Option<Job> {
        self.subs
            .iter()
            .flat_map(|(&sub, subscriber)| {
                subscriber
                    .watches
                    .iter()
                    .filter(|(_, entry)| matches!(entry.phase, Phase::Pending(_)))
                    .map(move |(key, entry)| (entry.generation, sub, key, &entry.project_path))
            })
            .min_by_key(|(generation, ..)| *generation)
            .map(|(generation, sub, key, project_path)| Job {
                sub,
                key: key.clone(),
                project_path: project_path.clone(),
                generation,
            })
    }

    /// 注册线程解析完一个待决订阅,回来记账。返回要打的日志行。
    fn apply_resolution(&mut self, job: Job, result: Result<PathBuf>) -> Vec<String> {
        let Some(subscriber) = self.subs.get_mut(&job.sub) else {
            return Vec::new();
        };
        let label = subscriber.label;
        let Some(entry) = subscriber.watches.get_mut(&job.key) else {
            return Vec::new();
        };
        // 解析期间被撤掉又重挂过:新的一轮还在 Pending,留给下一轮办
        if entry.generation != job.generation {
            return Vec::new();
        }
        let Phase::Pending(ready) = &entry.phase else {
            return Vec::new();
        };
        let ready = ready.clone();
        match result {
            Err(err) => {
                let msg = format!("{err:#}");
                entry.phase = Phase::Failed;
                ready.settle(WatchOutcome::Failed(msg.clone()));
                vec![format!("[{label}] 监听 {} 失败: {msg}", job.key.display())]
            }
            Ok(canonical) => {
                // 别的订阅已经把这个目录挂上了:直接就位,不必再碰后端
                let active = self.registered.contains(&canonical);
                entry.phase = if active {
                    Phase::Active {
                        canonical: canonical.clone(),
                    }
                } else {
                    Phase::Resolved {
                        canonical: canonical.clone(),
                        ready: ready.clone(),
                    }
                };
                let project_path = entry.project_path.clone();
                self.routes.entry(canonical).or_default().push(Route {
                    sub: job.sub,
                    key: job.key,
                    project_path,
                });
                if active {
                    ready.settle(WatchOutcome::Active);
                }
                Vec::new()
            }
        }
    }

    /// 注册线程调完一次后端 watch,回来记账。返回要打的日志行。
    fn apply_registration(&mut self, dir: &Path, result: Result<()>) -> Vec<String> {
        let targets: Vec<(SubId, PathBuf)> = self
            .routes
            .get(dir)
            .map(|routes| routes.iter().map(|r| (r.sub, r.key.clone())).collect())
            .unwrap_or_default();
        match result {
            Ok(()) => {
                self.registered.insert(dir.to_path_buf());
                for (sub, key) in targets {
                    let Some(entry) = self
                        .subs
                        .get_mut(&sub)
                        .and_then(|subscriber| subscriber.watches.get_mut(&key))
                    else {
                        continue;
                    };
                    if !matches!(entry.phase, Phase::Resolved { .. }) {
                        continue;
                    }
                    if let Phase::Resolved { canonical, ready } =
                        std::mem::replace(&mut entry.phase, Phase::Failed)
                    {
                        entry.phase = Phase::Active { canonical };
                        ready.settle(WatchOutcome::Active);
                    }
                }
                Vec::new()
            }
            Err(err) => {
                // 挂不上:引用这个目录的订阅全部落「失败」(重挂失败时也包括原先 Active 的)
                let msg = format!("{err:#}");
                self.routes.remove(dir);
                let mut logs = Vec::new();
                for (sub, key) in targets {
                    let Some(subscriber) = self.subs.get_mut(&sub) else {
                        continue;
                    };
                    let Some(entry) = subscriber.watches.get_mut(&key) else {
                        continue;
                    };
                    if let Phase::Resolved { ready, .. } =
                        std::mem::replace(&mut entry.phase, Phase::Failed)
                    {
                        ready.settle(WatchOutcome::Failed(msg.clone()));
                    }
                    logs.push(format!(
                        "[{}] 监听 {} 失败: {msg}",
                        subscriber.label,
                        key.display()
                    ));
                }
                logs
            }
        }
    }
}

/// 注册线程的一件解析活。
struct Job {
    sub: SubId,
    key: PathBuf,
    project_path: String,
    generation: u64,
}

struct Shared {
    state: Mutex<State>,
    /// 叫醒注册线程。
    work_cv: Condvar,
    /// 注册线程睡下时广播(测试等收敛用)。
    idle_cv: Condvar,
}

impl Shared {
    /// notify 的事件回调(后端线程上)。查路由时持锁,调 sink 前放锁。
    fn dispatch(&self, res: notify::Result<NotifyEvent>) {
        let Ok(event) = res else {
            return;
        };
        if event.paths.is_empty() {
            return;
        }
        let kind = format!("{:?}", event.kind);
        let reshapes = reshapes_dir(&event.kind);
        let mut out: Vec<(SubId, FsChangeSink, FsChange)> = Vec::new();
        let mut wake = false;
        {
            let mut guard = self.state.lock();
            let st = &mut *guard;
            for p in &event.paths {
                // 去重只在同一个路径的命中之间做:同一订阅者经「父目录」与「自身」
                // 两条路由命中同一路径时只交一次
                let start = out.len();
                if let Some(routes) = st.routes.get(p) {
                    if reshapes && st.registered.contains(p) {
                        st.stale.insert(p.clone());
                        wake = true;
                    }
                    for route in routes {
                        push_change(
                            &st.subs,
                            &mut out,
                            start,
                            route,
                            route.key.clone(),
                            &kind,
                            reshapes,
                        );
                    }
                }
                if let (Some(parent), Some(name)) = (p.parent(), p.file_name())
                    && let Some(routes) = st.routes.get(parent)
                {
                    for route in routes {
                        push_change(
                            &st.subs,
                            &mut out,
                            start,
                            route,
                            route.key.join(name),
                            &kind,
                            reshapes,
                        );
                    }
                }
            }
            if wake {
                st.mark_dirty();
            }
        }
        if wake {
            self.work_cv.notify_one();
        }
        for (_, sink, change) in out {
            sink(change);
        }
    }
}

fn push_change(
    subs: &HashMap<SubId, Subscriber>,
    out: &mut Vec<(SubId, FsChangeSink, FsChange)>,
    start: usize,
    route: &Route,
    path: PathBuf,
    kind: &str,
    structural: bool,
) {
    if out[start..]
        .iter()
        .any(|(sub, _, change)| *sub == route.sub && change.path == path)
    {
        return;
    }
    let Some(subscriber) = subs.get(&route.sub) else {
        return;
    };
    out.push((
        route.sub,
        subscriber.sink.clone(),
        FsChange {
            project_path: route.project_path.clone(),
            path,
            kind: kind.to_string(),
            structural,
        },
    ));
}

/// 事件落在被监听目录**自身**、且是这几类时,后端那一条要摘掉重挂:目录被删、
/// 被改名(走了/来了)、被重建。Windows 的目录句柄、inotify 的 wd 都绑在旧目录对象上,
/// 不重挂的话共享这一条的订阅者全体失聪。普通的「目录被修改」(Windows 父目录监听
/// 在子目录内容变化时也会报)不算,否则每次子项变化都要重挂一遍。
fn reshapes_dir(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// 进程级单例的外壳。持有者全部放手后通知注册线程收摊(全局那一份永不放手)。
struct Hub {
    shared: Arc<Shared>,
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.shared.state.lock().shutdown = true;
        self.shared.work_cv.notify_all();
    }
}

impl Hub {
    fn global() -> Arc<Hub> {
        static GLOBAL: OnceLock<Arc<Hub>> = OnceLock::new();
        GLOBAL
            .get_or_init(|| Hub::spawn(Box::new(notify_backend)))
            .clone()
    }

    fn spawn(factory: BackendFactory) -> Arc<Hub> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                idle: true,
                ..State::default()
            }),
            work_cv: Condvar::new(),
            idle_cv: Condvar::new(),
        });
        let worker = shared.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("mt-fs-watch".into())
            .spawn(move || worker_loop(worker, factory))
        {
            // 起不了线程(句柄耗尽)极罕见。此后 watch 一律直接失败:界面照常用,
            // 只是目录变化不再自动刷新
            eprintln!("[watch] 起注册线程失败,目录监听停用: {err}");
            shared.state.lock().dead = Some(format!("起注册线程失败: {err}"));
        }
        Arc::new(Hub { shared })
    }

    fn subscribe(self: &Arc<Self>, label: &'static str, sink: FsChangeSink) -> FsWatcher {
        let mut st = self.shared.state.lock();
        st.next_sub += 1;
        let id = st.next_sub;
        st.subs.insert(
            id,
            Subscriber {
                label,
                sink,
                watches: HashMap::new(),
            },
        );
        FsWatcher {
            hub: self.clone(),
            id,
        }
    }

    fn watch(&self, sub: SubId, path: &Path, project_path: &str) -> WatchReady {
        let mut guard = self.shared.state.lock();
        let st = &mut *guard;
        st.next_generation += 1;
        let generation = st.next_generation;
        let Some(subscriber) = st.subs.get_mut(&sub) else {
            return WatchReady::settled(WatchOutcome::Cancelled);
        };
        let label = subscriber.label;
        let entry = match subscriber.watches.entry(path.to_path_buf()) {
            Entry::Occupied(occupied) => {
                let entry = occupied.into_mut();
                entry.count += 1;
                match &entry.phase {
                    Phase::Pending(ready) | Phase::Resolved { ready, .. } => {
                        return WatchReady(ready.clone());
                    }
                    Phase::Active { .. } => return WatchReady::settled(WatchOutcome::Active),
                    // 上次失败:重新排队重试(失败态没有路由,换项目根也安全)
                    Phase::Failed => entry.project_path = project_path.to_string(),
                }
                entry
            }
            // 新订阅:相位下面立刻置成 Pending
            Entry::Vacant(vacant) => vacant.insert(WatchEntry {
                count: 1,
                project_path: project_path.to_string(),
                generation,
                phase: Phase::Failed,
            }),
        };
        if let Some(err) = &st.dead {
            entry.phase = Phase::Failed;
            eprintln!("[{label}] 监听 {} 失败: {err}", path.display());
            return WatchReady::settled(WatchOutcome::Failed(err.clone()));
        }
        let ready = ReadySignal::new();
        entry.phase = Phase::Pending(ready.clone());
        entry.generation = generation;
        st.mark_dirty();
        drop(guard);
        self.shared.work_cv.notify_one();
        WatchReady(ready)
    }

    fn unwatch(&self, sub: SubId, path: &Path) {
        let mut guard = self.shared.state.lock();
        let st = &mut *guard;
        let Some(subscriber) = st.subs.get_mut(&sub) else {
            return;
        };
        let Some(entry) = subscriber.watches.get_mut(path) else {
            return;
        };
        // 计数归零的条目当场删除,所以这里 count ≥ 1,不会减出负数
        entry.count -= 1;
        if entry.count > 0 {
            return;
        }
        let Some(entry) = subscriber.watches.remove(path) else {
            return;
        };
        st.release(sub, path, entry);
        st.mark_dirty();
        drop(guard);
        self.shared.work_cv.notify_one();
    }

    fn unsubscribe(&self, sub: SubId) {
        let mut guard = self.shared.state.lock();
        let st = &mut *guard;
        let Some(mut subscriber) = st.subs.remove(&sub) else {
            return;
        };
        for (key, entry) in subscriber.watches.drain() {
            st.release(sub, &key, entry);
        }
        st.mark_dirty();
        drop(guard);
        self.shared.work_cv.notify_one();
        // sink 在锁外释放:闭包的 Drop 里做什么都与本模块无关
        drop(subscriber);
    }

    fn watched_count(&self, sub: SubId) -> usize {
        let st = self.shared.state.lock();
        st.subs.get(&sub).map_or(0, |subscriber| {
            subscriber
                .watches
                .values()
                .filter(|entry| !matches!(entry.phase, Phase::Failed))
                .count()
        })
    }

    fn is_failed(&self, sub: SubId, path: &Path) -> bool {
        let st = self.shared.state.lock();
        st.subs
            .get(&sub)
            .and_then(|subscriber| subscriber.watches.get(path))
            .is_some_and(|entry| matches!(entry.phase, Phase::Failed))
    }
}

// ─── 注册线程 ─────────────────────────────────────────────

/// 注册线程取活:要摘的、要挂的、一个待决订阅。没活就睡到有人叫;收摊返回 `None`。
fn next_work(shared: &Shared) -> Option<(Vec<PathBuf>, Vec<PathBuf>, Option<Job>)> {
    let mut guard = shared.state.lock();
    loop {
        if guard.shutdown {
            return None;
        }
        let st = &mut *guard;
        st.dirty = false;
        let stale: HashSet<PathBuf> = std::mem::take(&mut st.stale)
            .into_iter()
            .filter(|dir| st.registered.contains(dir))
            .collect();
        let removals: Vec<PathBuf> = st
            .registered
            .iter()
            .filter(|dir| !st.routes.contains_key(*dir) || stale.contains(*dir))
            .cloned()
            .collect();
        let additions: Vec<PathBuf> = st
            .routes
            .keys()
            .filter(|dir| !st.registered.contains(*dir) || stale.contains(*dir))
            .cloned()
            .collect();
        let job = st.next_pending();
        if removals.is_empty() && additions.is_empty() && job.is_none() {
            st.idle = true;
            shared.idle_cv.notify_all();
            while !guard.dirty && !guard.shutdown {
                shared.work_cv.wait(&mut guard);
            }
            continue;
        }
        st.idle = false;
        return Some((removals, additions, job));
    }
}

fn worker_loop(shared: Arc<Shared>, mut factory: BackendFactory) {
    let dispatcher = Dispatcher(Arc::downgrade(&shared));
    // notify watcher 只归这条线程,调它时手上没有任何锁(见模块注释「死锁红线」)
    let mut backend: Option<Box<dyn Backend>> = None;
    while let Some((removals, additions, job)) = next_work(&shared) {
        // ① 先摘:删目录前的脱挂要尽快把句柄放掉
        if let Some(backend) = backend.as_mut() {
            for dir in &removals {
                // 目录已被删时 inotify / FSEvents 会报「没在监听」,不必理会
                let _ = backend.unwatch(dir);
            }
        }
        if !removals.is_empty() {
            let mut st = shared.state.lock();
            for dir in &removals {
                st.registered.remove(dir);
            }
        }
        // ② 再挂。目录已经不在了就不碰后端:多半是被监听目录自己被删之后的重挂(删掉
        // 某个前缀下最后一个分支时 git 连空目录 `refs/heads/bugfix` 一起删),或解析完才
        // 被删。订阅照样落「失败」,但这不是故障 —— 订阅方下一次重列清单时自会撤掉它,
        // 不打日志
        for dir in additions {
            if !dir.is_dir() {
                let vanished = Err(anyhow::anyhow!("目录已不存在: {}", dir.display()));
                let _ = shared.state.lock().apply_registration(&dir, vanished);
                continue;
            }
            let result = match backend.as_mut() {
                Some(backend) => backend.watch(&dir),
                None => match factory(dispatcher.clone()) {
                    Ok(created) => {
                        shared.state.lock().backends_created += 1;
                        backend.insert(created).watch(&dir)
                    }
                    Err(err) => Err(err),
                },
            };
            let logs = shared.state.lock().apply_registration(&dir, result);
            print_logs(logs);
        }
        // ③ 解析一个待决订阅(IO 在锁外)
        if let Some(job) = job {
            let result = resolve(&job.key, &job.project_path);
            let logs = shared.state.lock().apply_resolution(job, result);
            print_logs(logs);
        }
    }
    // 收摊:notify watcher 在锁外析构
    drop(backend);
}

fn print_logs(logs: Vec<String>) {
    for line in logs {
        eprintln!("{line}");
    }
}

/// 校验在项目根内 + 求物理身份。
///
/// 校验只做纵深防御(见 [`FsWatcher::watch`]);物理身份要**完整** canonicalize(连叶子
/// 符号链接一起解开)—— 后端监听本来就跟随链接,身份得是它真正盯着的那个目录。
fn resolve(key: &Path, project_path: &str) -> Result<PathBuf> {
    crate::fs::verify_under_project_root(Path::new(project_path), key, true)?;
    std::fs::canonicalize(key).with_context(|| format!("监听目录失败: {}", key.display()))
}

/// notify watcher 的最小接口。测试里换成假后端,数调用、卡时机。
trait Backend {
    fn watch(&mut self, dir: &Path) -> Result<()>;
    fn unwatch(&mut self, dir: &Path) -> Result<()>;
}

type BackendFactory = Box<dyn FnMut(Dispatcher) -> Result<Box<dyn Backend>> + Send>;

/// 事件回调手里的单例引用。弱引用:单例收摊后迟到的事件直接丢弃。
#[derive(Clone)]
struct Dispatcher(Weak<Shared>);

impl Dispatcher {
    fn dispatch(&self, res: notify::Result<NotifyEvent>) {
        if let Some(shared) = self.0.upgrade() {
            shared.dispatch(res);
        }
    }
}

struct NotifyBackend(RecommendedWatcher);

impl Backend for NotifyBackend {
    fn watch(&mut self, dir: &Path) -> Result<()> {
        self.0
            .watch(dir, RecursiveMode::NonRecursive)
            .with_context(|| {
                format!(
                    "监听目录失败: {}",
                    crate::fs::strip_verbatim_prefix(dir.to_path_buf()).display()
                )
            })
    }

    fn unwatch(&mut self, dir: &Path) -> Result<()> {
        self.0.unwatch(dir)?;
        Ok(())
    }
}

fn notify_backend(dispatcher: Dispatcher) -> Result<Box<dyn Backend>> {
    let watcher = notify::recommended_watcher(move |res: notify::Result<NotifyEvent>| {
        dispatcher.dispatch(res)
    })
    .context("创建文件监听器失败")?;
    Ok(Box::new(NotifyBackend(watcher)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, RemoveKind, RenameMode};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{RecvTimeoutError, TryRecvError};

    /// notify 在 Windows 上有几十毫秒级延迟,给足预算但不无限等。
    const BUDGET: Duration = Duration::from_secs(10);

    fn temp_dir(tag: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mini-term-watch-{tag}-{ts}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 测试里项目根就是被监听目录本身(watch 会校验路径在根内)。
    fn proj(dir: &Path) -> String {
        dir.to_string_lossy().to_string()
    }

    fn canon(dir: &Path) -> PathBuf {
        std::fs::canonicalize(dir).unwrap()
    }

    fn channel_sink() -> (FsChangeSink, Receiver<FsChange>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        (
            Arc::new(move |change: FsChange| {
                let _ = tx.lock().send(change);
            }),
            rx,
        )
    }

    /// 什么都不做的 sink。
    fn noop_sink() -> FsChangeSink {
        Arc::new(|_: FsChange| {})
    }

    /// 预算内收一条满足条件的变更(真 notify 后端用)。
    fn recv_matching(
        rx: &Receiver<FsChange>,
        pred: impl Fn(&FsChange) -> bool,
    ) -> Option<FsChange> {
        let deadline = Instant::now() + BUDGET;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left.min(Duration::from_millis(500))) {
                Ok(change) if pred(&change) => return Some(change),
                Ok(_) | Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
        None
    }

    /// 已到的变更路径(假后端的分发是同步的,emit 返回时就到齐了)。
    fn drain_paths(rx: &Receiver<FsChange>) -> Vec<PathBuf> {
        rx.try_iter().map(|change| change.path).collect()
    }

    impl Hub {
        fn real() -> Arc<Hub> {
            Hub::spawn(Box::new(notify_backend))
        }

        /// 等注册线程把期望表对账完、睡下。
        fn wait_idle(&self, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut st = self.shared.state.lock();
            while !st.idle {
                if self
                    .shared
                    .idle_cv
                    .wait_until(&mut st, deadline)
                    .timed_out()
                {
                    return st.idle;
                }
            }
            true
        }

        fn registered(&self) -> HashSet<PathBuf> {
            self.shared.state.lock().registered.clone()
        }

        fn backends_created(&self) -> usize {
            self.shared.state.lock().backends_created
        }
    }

    // ── 假后端:数调用、可把某个目录的 watch 卡住、可手工发事件 ──

    #[derive(Default)]
    struct FakeLog {
        live: HashSet<PathBuf>,
        watch_calls: Vec<PathBuf>,
        unwatch_calls: Vec<PathBuf>,
        dispatcher: Option<Dispatcher>,
    }

    #[derive(Default)]
    struct GateState {
        block: Option<PathBuf>,
        entered: bool,
        open: bool,
    }

    /// 把对某个目录的后端 watch 卡住,直到放行 —— 模拟「注册还在进行中」。
    #[derive(Default)]
    struct Gate {
        state: Mutex<GateState>,
        cv: Condvar,
    }

    impl Gate {
        fn block(&self, dir: PathBuf) {
            *self.state.lock() = GateState {
                block: Some(dir),
                entered: false,
                open: false,
            };
        }

        fn pass(&self, dir: &Path) {
            let mut st = self.state.lock();
            if st.block.as_deref() != Some(dir) {
                return;
            }
            st.entered = true;
            self.cv.notify_all();
            while !st.open {
                self.cv.wait(&mut st);
            }
        }

        fn wait_entered(&self, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut st = self.state.lock();
            while !st.entered {
                if self.cv.wait_until(&mut st, deadline).timed_out() {
                    return st.entered;
                }
            }
            true
        }

        fn open(&self) {
            self.state.lock().open = true;
            self.cv.notify_all();
        }
    }

    struct FakeBackend {
        log: Arc<Mutex<FakeLog>>,
        gate: Arc<Gate>,
    }

    impl Backend for FakeBackend {
        fn watch(&mut self, dir: &Path) -> Result<()> {
            self.gate.pass(dir);
            if !dir.is_dir() {
                anyhow::bail!("目录不存在: {}", dir.display());
            }
            let mut log = self.log.lock();
            log.watch_calls.push(dir.to_path_buf());
            log.live.insert(dir.to_path_buf());
            Ok(())
        }

        fn unwatch(&mut self, dir: &Path) -> Result<()> {
            let mut log = self.log.lock();
            log.unwatch_calls.push(dir.to_path_buf());
            log.live.remove(dir);
            Ok(())
        }
    }

    struct Fake {
        hub: Arc<Hub>,
        log: Arc<Mutex<FakeLog>>,
        gate: Arc<Gate>,
    }

    impl Fake {
        fn new() -> Self {
            let log = Arc::new(Mutex::new(FakeLog::default()));
            let gate = Arc::new(Gate::default());
            let (factory_log, factory_gate) = (log.clone(), gate.clone());
            let hub = Hub::spawn(Box::new(
                move |dispatcher: Dispatcher| -> Result<Box<dyn Backend>> {
                    factory_log.lock().dispatcher = Some(dispatcher);
                    Ok(Box::new(FakeBackend {
                        log: factory_log.clone(),
                        gate: factory_gate.clone(),
                    }) as Box<dyn Backend>)
                },
            ));
            Self { hub, log, gate }
        }

        /// 以后端形态(canonical 路径)发一条事件,与 notify 回调同一入口。
        fn emit(&self, kind: EventKind, paths: &[PathBuf]) {
            let dispatcher = self.log.lock().dispatcher.clone().expect("后端还没建");
            let event = paths
                .iter()
                .fold(NotifyEvent::new(kind), |event, p| event.add_path(p.clone()));
            dispatcher.dispatch(Ok(event));
        }

        fn live(&self) -> HashSet<PathBuf> {
            self.log.lock().live.clone()
        }

        fn idle(&self) {
            assert!(self.hub.wait_idle(BUDGET), "注册线程 10s 内没收敛");
        }
    }

    // ── 订阅者内语义(全局单例;断言都按本订阅者/本目录过滤,不怕并行测试) ──

    #[test]
    fn watch_refcounts_same_path() {
        let dir = temp_dir("refcount");
        let (w, _rx) = FsWatcher::with_channel();

        w.watch(&dir, &proj(&dir));
        w.watch(&dir, &proj(&dir));
        assert_eq!(w.watched_count(), 1, "同一路径只应占一条");

        // 第一次 unwatch 只把计数降到 1,监听必须还在
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
        assert_eq!(
            w.watch(&dir, &root).wait(BUDGET),
            Some(WatchOutcome::Active)
        );

        std::fs::write(dir.join("new.txt"), "hello").unwrap();

        let change = recv_matching(&rx, |_| true).expect("10s 内未收到任何 fs 变更事件");
        assert_eq!(change.project_path, root);
        assert!(!change.kind.is_empty());
        // 交出去的是订阅者注册时的字面形态,不是后端的 canonical(`\\?\`)形态
        assert!(
            change.path.starts_with(&dir),
            "路径应改写回字面形态: {}",
            change.path.display()
        );

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
        assert_eq!(
            w.watch(&dir, &proj(&dir)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        std::fs::write(dir.join("a.txt"), "x").unwrap();

        let deadline = Instant::now() + BUDGET;
        while Instant::now() < deadline && hits.load(Ordering::Relaxed) == 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(hits.load(Ordering::Relaxed) > 0, "注入的闭包应被调用");

        w.unwatch(&dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn watch_nonexistent_path_fails_and_can_retry() {
        let (w, _rx) = FsWatcher::with_channel();
        let root = temp_dir("missing-root");
        let missing = root.join("definitely-missing-xyz");
        assert!(matches!(
            w.watch(&missing, &proj(&root)).wait(BUDGET),
            Some(WatchOutcome::Failed(_))
        ));
        assert_eq!(w.watched_count(), 0, "失败的 watch 不计入持有数");
        assert!(w.is_failed(&missing));

        // 目录补上后再 watch 一次就重试(失败的那次计数照记,两次 unwatch 才配平)
        std::fs::create_dir_all(&missing).unwrap();
        assert_eq!(
            w.watch(&missing, &proj(&root)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        assert!(!w.is_failed(&missing));
        w.unwatch(&missing);
        assert_eq!(w.watched_count(), 1);
        w.unwatch(&missing);
        assert_eq!(w.watched_count(), 0);
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
            matches!(
                w.watch(&escaped, &proj(&root)).wait(BUDGET),
                Some(WatchOutcome::Failed(_))
            ),
            "越出项目根的路径必须拒绝"
        );
        assert_eq!(w.watched_count(), 0, "被拒的 watch 不计入持有数");

        // 同一目录换成从根内进入则放行,证明拒绝的是「越界」而非「路径带 ..」
        let inside = root.join("sub");
        std::fs::create_dir_all(&inside).unwrap();
        assert_eq!(
            w.watch(&root.join("sub").join("..").join("sub"), &proj(&root))
                .wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        assert_eq!(w.watched_count(), 1);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    // ── 单例口径 ──

    #[test]
    fn global_hub_is_shared_and_creates_one_notify_watcher() {
        let (a, _ra) = FsWatcher::with_channel();
        let (b, _rb) = FsWatcher::with_channel();
        assert!(Arc::ptr_eq(&a.hub, &b.hub), "所有句柄都挂在同一个单例上");

        let d1 = temp_dir("global-1");
        let d2 = temp_dir("global-2");
        assert_eq!(
            a.watch(&d1, &proj(&d1)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        assert_eq!(
            b.watch(&d2, &proj(&d2)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        // 全局单例被本进程所有测试共用:无论谁先挂上,notify watcher 只建一次
        assert_eq!(
            a.hub.backends_created(),
            1,
            "全进程只应创建一个 notify watcher"
        );

        drop((a, b));
        std::fs::remove_dir_all(&d1).ok();
        std::fs::remove_dir_all(&d2).ok();
    }

    #[test]
    fn one_notify_watcher_per_hub_across_subscribers_and_dirs() {
        let hub = Hub::real();
        let root = temp_dir("one-backend");
        let dirs: Vec<PathBuf> = (0..3)
            .map(|i| {
                let dir = root.join(format!("d{i}"));
                std::fs::create_dir_all(&dir).unwrap();
                dir
            })
            .collect();
        let subs: Vec<FsWatcher> = (0..3).map(|_| hub.subscribe("t", noop_sink())).collect();
        for sub in &subs {
            for dir in &dirs {
                assert_eq!(
                    sub.watch(dir, &proj(&root)).wait(BUDGET),
                    Some(WatchOutcome::Active)
                );
            }
        }
        assert_eq!(hub.backends_created(), 1);
        assert_eq!(
            hub.registered().len(),
            3,
            "3 个目录各注册一次,与订阅者数无关"
        );

        drop(subs);
        assert!(hub.wait_idle(BUDGET));
        assert!(hub.registered().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    // ── 跨订阅者引用计数 / 分发 ──

    #[test]
    fn refcount_spans_subscribers() {
        let fake = Fake::new();
        let dir = temp_dir("x-sub");
        let a = fake.hub.subscribe("a", channel_sink().0);
        let b = fake.hub.subscribe("b", channel_sink().0);

        assert_eq!(
            a.watch(&dir, &proj(&dir)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        assert_eq!(
            b.watch(&dir, &proj(&dir)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        fake.idle();
        assert_eq!(
            fake.log.lock().watch_calls,
            vec![canon(&dir)],
            "两个订阅者同一目录,后端只注册一次"
        );

        a.unwatch(&dir);
        fake.idle();
        assert_eq!(fake.live(), HashSet::from([canon(&dir)]), "b 还在用,不许摘");

        b.unwatch(&dir);
        fake.idle();
        assert!(fake.live().is_empty(), "最后一个订阅撤走才真正摘");
        assert_eq!(fake.log.lock().unwatch_calls, vec![canon(&dir)]);
        assert!(fake.hub.registered().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn case_variants_share_one_backend_entry_but_keep_their_own_form() {
        let fake = Fake::new();
        let dir = temp_dir("case");
        let upper = dir
            .parent()
            .unwrap()
            .join(dir.file_name().unwrap().to_string_lossy().to_uppercase());
        let (s1, rx1) = channel_sink();
        let (s2, rx2) = channel_sink();
        let lower_sub = fake.hub.subscribe("lower", s1);
        let upper_sub = fake.hub.subscribe("upper", s2);
        assert_eq!(
            lower_sub.watch(&dir, &proj(&dir)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        assert_eq!(
            upper_sub.watch(&upper, &proj(&upper)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        fake.idle();
        assert_eq!(
            fake.log.lock().watch_calls.len(),
            1,
            "大小写不同的同一目录只注册一次"
        );

        fake.emit(
            EventKind::Create(CreateKind::File),
            &[canon(&dir).join("x.txt")],
        );
        assert_eq!(drain_paths(&rx1), vec![dir.join("x.txt")]);
        assert_eq!(drain_paths(&rx2), vec![upper.join("x.txt")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn same_dir_different_projects_each_get_their_own() {
        // 嵌套项目:外层项目与内层项目同时监听内层目录,project_path 各收各的
        let hub = Hub::real();
        let root = temp_dir("nested");
        let pkg = root.join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let (s1, rx1) = channel_sink();
        let (s2, rx2) = channel_sink();
        let outer = hub.subscribe("outer", s1);
        let inner = hub.subscribe("inner", s2);
        assert_eq!(
            outer.watch(&pkg, &proj(&root)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        assert_eq!(
            inner.watch(&pkg, &proj(&pkg)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        assert_eq!(hub.registered().len(), 1);

        let file = pkg.join("hello.txt");
        std::fs::write(&file, "hi").unwrap();
        let c1 = recv_matching(&rx1, |c| c.path == file).expect("外层订阅者没收到");
        let c2 = recv_matching(&rx2, |c| c.path == file).expect("内层订阅者没收到");
        assert_eq!(c1.project_path, proj(&root));
        assert_eq!(c2.project_path, proj(&pkg));

        drop((outer, inner));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dispatch_routes_each_path_to_its_own_dir() {
        let fake = Fake::new();
        let root = temp_dir("route");
        let (a, b) = (root.join("a"), root.join("b"));
        let inner = a.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (s1, rx1) = channel_sink();
        let (s2, rx2) = channel_sink();
        let w1 = fake.hub.subscribe("w1", s1);
        let w2 = fake.hub.subscribe("w2", s2);
        for ready in [
            w1.watch(&a, &proj(&root)),
            w1.watch(&inner, &proj(&root)),
            w2.watch(&b, &proj(&root)),
        ] {
            assert_eq!(ready.wait(BUDGET), Some(WatchOutcome::Active));
        }

        // 跨目录 rename:一条事件两个路径,各回各家,不串
        fake.emit(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[canon(&a).join("old.txt"), canon(&b).join("new.txt")],
        );
        assert_eq!(drain_paths(&rx1), vec![a.join("old.txt")]);
        assert_eq!(drain_paths(&rx2), vec![b.join("new.txt")]);

        // 不在任何被监听目录下:谁都不给
        fake.emit(
            EventKind::Create(CreateKind::File),
            &[canon(&root).join("stray.txt")],
        );
        assert!(drain_paths(&rx1).is_empty());
        assert!(drain_paths(&rx2).is_empty());

        // 孙辈只归直接父目录的订阅:a 的订阅拿不到 a/inner/deep.txt 以外的串门
        fake.emit(
            EventKind::Create(CreateKind::File),
            &[canon(&inner).join("deep.txt")],
        );
        assert_eq!(drain_paths(&rx1), vec![inner.join("deep.txt")]);
        assert!(drain_paths(&rx2).is_empty());

        // 同一订阅者经「父目录 a」与「自身 inner」两条路由命中同一路径:只交一次
        fake.emit(EventKind::Modify(ModifyKind::Any), &[canon(&inner)]);
        assert_eq!(drain_paths(&rx1), vec![inner.clone()]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dir_removed_or_recreated_gets_reregistered() {
        let fake = Fake::new();
        let root = temp_dir("stale");
        let dir = root.join("build");
        std::fs::create_dir_all(&dir).unwrap();
        let (sink, rx) = channel_sink();
        let w = fake.hub.subscribe("w", sink);
        assert_eq!(
            w.watch(&dir, &proj(&root)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        fake.idle();
        let c = canon(&dir);

        // 目录被删后又建回来(rm -rf build && mkdir build):后端那一条摘掉重挂
        fake.emit(
            EventKind::Create(CreateKind::Folder),
            std::slice::from_ref(&c),
        );
        assert_eq!(
            drain_paths(&rx),
            vec![dir.clone()],
            "目录自身的事件交给它的订阅者"
        );
        fake.idle();
        assert_eq!(fake.log.lock().watch_calls, vec![c.clone(), c.clone()]);
        assert_eq!(fake.log.lock().unwatch_calls, vec![c.clone()]);
        assert_eq!(fake.live(), HashSet::from([c.clone()]));
        assert!(!w.is_failed(&dir));

        // 普通的「目录被修改」不重挂
        fake.emit(EventKind::Modify(ModifyKind::Any), std::slice::from_ref(&c));
        fake.idle();
        assert_eq!(fake.log.lock().watch_calls.len(), 2);

        // 真删了:重挂时目录已不在 → 不碰后端、落「失败」态(不打日志),后端不残留
        std::fs::remove_dir_all(&dir).unwrap();
        fake.emit(
            EventKind::Remove(RemoveKind::Folder),
            std::slice::from_ref(&c),
        );
        fake.idle();
        assert!(w.is_failed(&dir));
        assert!(fake.live().is_empty());
        assert!(fake.hub.registered().is_empty());
        // 失败的订阅照样能配对撤销
        w.unwatch(&dir);
        assert!(!w.is_failed(&dir));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn structural_flag_marks_create_remove_and_rename_only() {
        let fake = Fake::new();
        let root = temp_dir("structural");
        let dir = root.join("sub");
        std::fs::create_dir_all(&dir).unwrap();
        let (sink, rx) = channel_sink();
        let w = fake.hub.subscribe("w", sink);
        for ready in [w.watch(&root, &proj(&root)), w.watch(&dir, &proj(&root))] {
            assert_eq!(ready.wait(BUDGET), Some(WatchOutcome::Active));
        }
        let flags = |rx: &Receiver<FsChange>| -> Vec<(PathBuf, bool)> {
            rx.try_iter().map(|c| (c.path, c.structural)).collect()
        };

        // 建 / 删 / 改名:不论经父目录路由还是目录自身路由,都标结构变化
        for kind in [
            EventKind::Create(CreateKind::Folder),
            EventKind::Remove(RemoveKind::Folder),
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
        ] {
            fake.emit(kind, &[canon(&dir)]);
            assert_eq!(flags(&rx), vec![(dir.clone(), true)], "{kind:?}");
            fake.idle();
        }
        // 子项的建也是结构变化(落在子项自己身上)
        fake.emit(
            EventKind::Create(CreateKind::File),
            &[canon(&dir).join("a.txt")],
        );
        assert_eq!(flags(&rx), vec![(dir.join("a.txt"), true)]);
        // 只是内容 / 元数据变了:不标
        fake.emit(EventKind::Modify(ModifyKind::Any), &[canon(&dir)]);
        assert_eq!(flags(&rx), vec![(dir.clone(), false)]);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 文件树的恢复路径(第四轮 E2E C12):展开着的目录被外部删掉、又建回同名目录。
    /// 它自己那条订阅在后端重挂时落「失败」(目录已不在),之后**不会**自愈 —— 订阅方
    /// 要凭父目录交来的结构变化把它撤掉、重新 watch。这里用真 notify 后端把这条路走一遍:
    /// 父目录订阅收得到删与建两条结构变化,撤掉重挂之后新目录里的变化照常有事件。
    #[test]
    fn recreated_dir_resumes_after_rewatch() {
        let hub = Hub::real();
        let root = temp_dir("recreate");
        let dir = root.join("e2e-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let (sink, rx) = channel_sink();
        let w = hub.subscribe("files", sink);
        let project = proj(&root);
        for ready in [w.watch(&root, &project), w.watch(&dir, &project)] {
            assert_eq!(ready.wait(BUDGET), Some(WatchOutcome::Active));
        }

        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            recv_matching(&rx, |c| c.path == dir && c.structural).is_some(),
            "父目录订阅应收到目录自身被删的结构变化"
        );
        assert!(hub.wait_idle(BUDGET));
        assert!(w.is_failed(&dir), "目录已不在:原订阅重挂失败");

        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            recv_matching(&rx, |c| c.path == dir && c.structural).is_some(),
            "同名目录建回来也是结构变化"
        );
        // 订阅方的处置:撤掉旧订阅、重新 watch —— 新目录挂上后变化照常上报
        w.unwatch(&dir);
        assert_eq!(
            w.watch(&dir, &project).wait(BUDGET),
            Some(WatchOutcome::Active)
        );
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        assert!(
            recv_matching(&rx, |c| c.path == dir.join("b.txt")).is_some(),
            "重新挂上之后新目录里的变化要有事件"
        );

        drop(w);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dropping_subscriber_revokes_only_its_watches() {
        let fake = Fake::new();
        let root = temp_dir("drop");
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (s1, rx1) = channel_sink();
        let (s2, rx2) = channel_sink();
        let w1 = fake.hub.subscribe("w1", s1);
        let w2 = fake.hub.subscribe("w2", s2);
        for ready in [
            w1.watch(&a, &proj(&root)),
            w1.watch(&b, &proj(&root)),
            w2.watch(&b, &proj(&root)),
        ] {
            assert_eq!(ready.wait(BUDGET), Some(WatchOutcome::Active));
        }
        fake.idle();
        assert_eq!(fake.live(), HashSet::from([canon(&a), canon(&b)]));

        // 不逐个 unwatch,直接丢句柄
        drop(w1);
        fake.idle();
        assert_eq!(
            fake.live(),
            HashSet::from([canon(&b)]),
            "w1 名下的 a 要摘,b 还有 w2"
        );
        assert_eq!(fake.hub.registered(), HashSet::from([canon(&b)]));
        assert!(
            matches!(rx1.try_recv(), Err(TryRecvError::Disconnected)),
            "句柄 drop 后 sink 随之释放"
        );

        fake.emit(
            EventKind::Create(CreateKind::File),
            &[canon(&b).join("x.txt")],
        );
        assert_eq!(
            drain_paths(&rx2),
            vec![b.join("x.txt")],
            "另一个订阅者不受影响"
        );
        assert_eq!(w2.watched_count(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    // ── 后台注册的竞态 ──

    #[test]
    fn unwatch_before_registration_completes_leaves_nothing() {
        let fake = Fake::new();
        let root = temp_dir("race");
        let [steady, slow, queued] = ["steady", "slow", "queued"].map(|name| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        });
        let (sink, rx) = channel_sink();
        let w = fake.hub.subscribe("w", sink);
        assert_eq!(
            w.watch(&steady, &proj(&root)).wait(BUDGET),
            Some(WatchOutcome::Active)
        );

        // 让注册线程卡在 slow 的后端 watch 里
        fake.gate.block(canon(&slow));
        let slow_ready = w.watch(&slow, &proj(&root));
        assert!(
            fake.gate.wait_entered(BUDGET),
            "注册线程应卡在后端 watch 里"
        );
        // 注册线程卡着,排在后面的只能待在队里
        let queued_ready = w.watch(&queued, &proj(&root));
        assert_eq!(queued_ready.outcome(), None);

        // 主线程侧:撤销不等注册线程(它正卡着)
        let started = Instant::now();
        w.unwatch(&slow);
        w.unwatch(&queued);
        w.unwatch(&queued); // 多余的一次:计数不出负数
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(slow_ready.outcome(), Some(WatchOutcome::Cancelled));
        assert_eq!(queued_ready.outcome(), Some(WatchOutcome::Cancelled));
        assert_eq!(w.watched_count(), 1);

        // 事件回调也不等注册线程
        fake.emit(
            EventKind::Create(CreateKind::File),
            &[canon(&steady).join("a.txt")],
        );
        assert_eq!(drain_paths(&rx), vec![steady.join("a.txt")]);

        fake.gate.open();
        fake.idle();
        assert_eq!(
            fake.live(),
            HashSet::from([canon(&steady)]),
            "卡住的那次注册完成后应随即被摘掉"
        );
        assert_eq!(fake.hub.registered(), HashSet::from([canon(&steady)]));
        assert!(
            !fake.log.lock().watch_calls.contains(&canon(&queued)),
            "排队中就被撤的不应碰后端"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn churn_with_live_writes_does_not_deadlock() {
        let (done_tx, done_rx) = channel();
        let body = std::thread::spawn(move || {
            let hub = Hub::real();
            let root = temp_dir("churn");
            let project = proj(&root);
            let dirs: Vec<PathBuf> = (0..4)
                .map(|i| {
                    let dir = root.join(format!("d{i}"));
                    std::fs::create_dir_all(&dir).unwrap();
                    dir
                })
                .collect();

            // sink 里回调本模块(拿状态锁、登记/撤销):验证调 sink 之前已经放锁
            let probe = Arc::new(hub.subscribe("probe", noop_sink()));
            let hits = Arc::new(AtomicUsize::new(0));
            let echo = {
                let (probe, hits) = (probe.clone(), hits.clone());
                let (dir0, project) = (dirs[0].clone(), project.clone());
                hub.subscribe(
                    "echo",
                    Arc::new(move |_: FsChange| {
                        hits.fetch_add(1, Ordering::Relaxed);
                        let _ = probe.watched_count();
                        probe.watch(&dir0, &project);
                        probe.unwatch(&dir0);
                    }),
                )
            };
            for dir in &dirs {
                assert_eq!(
                    echo.watch(dir, &project).wait(BUDGET),
                    Some(WatchOutcome::Active)
                );
            }

            let stop = Arc::new(AtomicBool::new(false));
            let writer = {
                let (dirs, stop) = (dirs.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut n = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        for dir in &dirs {
                            let _ =
                                std::fs::write(dir.join(format!("f{}.txt", n % 8)), n.to_string());
                        }
                        n += 1;
                        std::thread::sleep(Duration::from_millis(2));
                    }
                })
            };
            let churners: Vec<_> = (0..2)
                .map(|t| {
                    let (hub, dirs, project) = (hub.clone(), dirs.clone(), project.clone());
                    std::thread::spawn(move || {
                        for i in 0..150usize {
                            let w = hub.subscribe("churn", noop_sink());
                            for dir in &dirs {
                                w.watch(dir, &project);
                            }
                            if (i + t) % 3 == 0 {
                                // 让一部分注册真正走到后端,与撤销交错
                                w.watch(&dirs[0], &project).wait(Duration::from_millis(50));
                            }
                            if i % 2 == 0 {
                                for dir in &dirs {
                                    w.unwatch(dir);
                                }
                            }
                            // 剩下的交给句柄 drop 撤销
                            drop(w);
                        }
                    })
                })
                .collect();
            for churner in churners {
                churner.join().unwrap();
            }
            // 写入线程继续写,直到 echo 收到过事件(macOS 上高频注册会反复重建
            // FSEvents 流,churn 期间可能一条都没赶上)
            let deadline = Instant::now() + BUDGET;
            while hits.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
            assert!(hits.load(Ordering::Relaxed) > 0, "写文件期间应收到事件");

            drop(echo);
            drop(probe);
            assert!(hub.wait_idle(BUDGET));
            assert!(hub.registered().is_empty(), "全部撤销后后端不应残留");
            assert_eq!(hub.backends_created(), 1);
            std::fs::remove_dir_all(&root).ok();
            let _ = done_tx.send(());
        });
        match done_rx.recv_timeout(Duration::from_secs(60)) {
            Ok(()) => body.join().unwrap(),
            Err(RecvTimeoutError::Disconnected) => {
                if let Err(panic) = body.join() {
                    std::panic::resume_unwind(panic);
                }
            }
            Err(RecvTimeoutError::Timeout) => panic!("60s 内没跑完:疑似死锁"),
        }
    }
}
