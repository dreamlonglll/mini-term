//! 移动端中转体系:桌面端 → 中转服务器的出站 WebSocket 长连(docs/adr/0001)。
//!
//! 连接由本 crate 持有:握手校验协议版本,断线后指数退避自动重连;
//! 状态变化经 [`RelayEvents::status_changed`] 交给上层(设置页「移动端」区域展示)。
//! 版本不匹配时停止重连(重试无意义),等待用户升级。

use parking_lot::Mutex;
use std::time::{Duration, Instant, SystemTime};

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    CommandFailReason, DesktopRejectReason, DesktopToRelay, MirrorMessage, MobileLauncher,
    MobilePane, MobileProject, RelayToDesktop, StartSessionFailReason, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::host::{AiLauncher, RelayEvents, RelayHost};
use crate::mirror::{self, MIRROR_PAGE_SIZE, MirrorAgent, MirrorParser, history_slice};
use crate::util::is_wsl_unc_path;

/// 握手 ack 等待超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 镜像会话文件轮询间隔。
const MIRROR_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 移动端指令:正文写进 PTY 之后、单独补写提交回车之前的间隔。
///
/// 正文与回车**分两次写、中间隔一段**,模仿真人「粘贴 → 按回车」。此前一次写入
/// `正文\r`,长指令的回车被当成正文里的换行、没有提交(E2E:约 30 字正常,121 字
/// 中文、约 230 字的 curl 只进了输入框)。根因在 Claude Code(2.1.292)的 stdin 分词:
/// 一次读到的数据 ≥ 64 个 UTF-16 码元时,控制字符(退格除外)不再切成单独的按键,
/// `\r` 留在文本块里被输入框当换行插入 —— 30 字 + `\r` 不到 64,长指令超了。
/// 所以回车必须落在 agent 的**另一次读**里:
/// - bracketed paste 开着(Claude Code 会开;实测便携与系统 ConPTY 都把 `?2004h`
///   透给终端、把 `ESC[200~` / `ESC[201~` 原样交给 agent):正文包成
///   `ESC[200~ … ESC[201~`,结束标记之后的 `\r` 必定切成独立的回车键,与读的批次
///   无关(Claude Code 自己往 PTY 里给会话回话也是「包裹正文 → 10ms 后单独写 `\r`」);
/// - 没开:只能靠间隔让 agent 先把正文读走 —— 事件循环被渲染占住期间到的输入,
///   下一次读会并成一块。实测(ConPTY + Bun 回显探针,事件循环每 100ms 忙 80ms):
///   间隔 30ms 时 10 次里并块 5 次,150ms 时 0 次;空闲时 1ms 就分得开。取 150ms,
///   比真人粘贴后按回车还快,只多压回执一拍。
const MOBILE_SUBMIT_DELAY: Duration = Duration::from_millis(150);

/// 启发式绑定(无 hook 会话身份)**已绑上**时多久重新定位一次。
///
/// 启发式定位要把四家的会话目录各扫一遍 —— Codex 那条递归整个
/// `~/.codex/sessions`、逐个 stat 再打开最新 30 个读头,实测一次 30ms 量级
/// (本机 378 个 rollout);已绑上时每秒扫一遍纯属浪费。增量泵照旧每轮跑,
/// 只把「项目里有没有更新的会话文件」这个问题降到 10s 问一次。代价如实记:
/// 无 hook 时切会话(`/clear` 等)的换绑最多晚 10s。
const HEURISTIC_RELOCATE_INTERVAL: Duration = Duration::from_secs(10);

/// 启发式绑定**还没绑上**(本轮会话的文件尚未落盘)时多久重新定位一次。
/// 比已绑上时勤一些:这段时间移动端看到的是空镜像,等的就是第一个文件出现。
const HEURISTIC_PENDING_INTERVAL: Duration = Duration::from_secs(3);

/// 桌面侧 store 喂入的同步载荷:比 wire 类型多项目路径(镜像绑定用,不发给移动端)。
///
/// v2 起上报 `config.projects` **全集**(不再只报有活跃 AI 会话的项目),
/// "仅 AI 会话 pane 可见"的裁剪只作用于 `panes`。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProject {
    pub project_id: String,
    pub name: String,
    pub path: String,
    /// SSH 远程项目引用的连接 id;本地项目为 None(判定能否远程发起会话用)
    #[serde(default)]
    pub ssh_connection_id: Option<String>,
    pub panes: Vec<SyncPane>,
    /// 桌面端项目树里的祖先分组名链(根→父),顶层项目为空。原样透传给移动端。
    #[serde(default)]
    pub group_path: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPane {
    pub pane_id: String,
    pub title: String,
    pub status: String,
    /// 该 pane 当前的 PTY id(移动端指令写穿目标);终端未创建时缺省
    #[serde(default)]
    pub pty_id: Option<u32>,
    /// 桌面端黄灯(agent 提问待答/等待授权批准),原样透传给移动端
    #[serde(default)]
    pub needs_attention: bool,
}

/// 一个镜像绑定的运行态:解析器 + 绑定文件与读取偏移。
/// 三者必须同锁——增量泵的「读文件→喂解析→挪偏移」要原子,拆开就会把同一段
/// 字节喂两次(消息重复、seq 漂移)。
struct MirrorRuntime {
    parser: MirrorParser,
    path: PathBuf,
    offset: u64,
}

/// 决定「这个 pane 该镜像哪个文件」的全部输入。与上一次定位时不同 = 当轮必须
/// 重新定位(hook 上报了新身份、AI 换了一轮、PTY 换了),时机与原来每轮都定位一致。
#[derive(Debug, Clone, PartialEq, Eq)]
enum LocateKey {
    /// hook 上报过会话身份:按身份精确定位
    Hook {
        pty_id: u32,
        agent: Option<String>,
        session_id: String,
    },
    /// 输入检测认出的 agent 没有可解析的记录(pi / opencode):恒为空镜像,不做 IO
    NoLog { pty_id: Option<u32>, agent: String },
    /// 无会话身份:项目最新文件 + AI 启动时刻下限的启发式
    Heuristic {
        pty_id: Option<u32>,
        agent: Option<String>,
        started: Option<SystemTime>,
    },
}

/// 一个镜像订阅的定位状态:上一次用什么输入、在什么时刻、定位到了哪个文件。
///
/// 只在轮询任务里流转(每轮随阻塞任务带进带出),不与点选作答的泵共享 ——
/// 那条只碰 [`MirrorRuntime`]。
#[derive(Default)]
struct MirrorLocator {
    key: Option<LocateKey>,
    /// 上一次定位的结果。`None` = 本轮会话的文件尚未落盘 / 该 agent 没有记录:
    /// 此时**不泵**旧绑定(与原来「定位落空的那一轮不泵」同口径)
    resolved: Option<(PathBuf, MirrorAgent)>,
    at: Option<Instant>,
    /// 已给过首个(可能为空的)快照
    sent_initial: bool,
}

impl MirrorLocator {
    /// 这一轮要不要重新定位。`bound` = 运行态当前绑着的文件(`None` = 从没绑过,
    /// 或增量泵发现文件被截断/删掉而清了运行态)。
    fn needs_locate(&self, key: &LocateKey, bound: Option<&Path>, now: Instant) -> bool {
        if self.key.as_ref() != Some(key) {
            return true;
        }
        let resolved = self.resolved.as_ref().map(|(path, _)| path.as_path());
        match key {
            // 精确绑定:身份没变且已绑在定位结果上 → 只做增量泵。文件还没落盘、
            // 或被截断清掉了运行态,才每轮再找(与原来同一节奏)
            LocateKey::Hook { .. } => resolved.is_none() || resolved != bound,
            // 结果恒为 None,输入没变就不必再算
            LocateKey::NoLog { .. } => false,
            LocateKey::Heuristic { .. } => {
                let elapsed = self
                    .at
                    .map_or(Duration::MAX, |at| now.saturating_duration_since(at));
                match bound {
                    // 绑过但运行态被泵清掉了(文件截断/重写):立刻重绑,
                    // 与原来「下一轮重新绑定」同时机
                    None if resolved.is_some() => true,
                    None => elapsed >= HEURISTIC_PENDING_INTERVAL,
                    Some(_) => elapsed >= HEURISTIC_RELOCATE_INTERVAL,
                }
            }
        }
    }
}

/// 一条镜像订阅在轮询任务手上的那几样。每轮连同 `Arc` 一起带进阻塞线程池。
struct MirrorJob {
    pane_id: String,
    project_path: String,
    messages: Arc<Mutex<Vec<MirrorMessage>>>,
    runtime: Arc<Mutex<Option<MirrorRuntime>>>,
}

/// 按定位输入找会话文件(阻塞 IO,只在阻塞线程池上调)。
fn locate_session_file(project_path: &str, key: &LocateKey) -> Option<(PathBuf, MirrorAgent)> {
    match key {
        LocateKey::Hook {
            agent, session_id, ..
        } => mirror::resolve_session_file_by_id(project_path, agent.as_deref(), session_id),
        LocateKey::NoLog { .. } => None,
        LocateKey::Heuristic { started, .. } => {
            mirror::resolve_session_file(project_path, *started)
        }
    }
}

/// 一个被订阅 pane 的镜像状态:取消句柄 + 已解析消息(分页取数用)
/// + 绑定运行态(共享持有:1s 轮询之外,移动端点选作答也要先泵一次再校验)。
struct MirrorSub {
    cancel_tx: watch::Sender<bool>,
    messages: Arc<Mutex<Vec<MirrorMessage>>>,
    runtime: Arc<Mutex<Option<MirrorRuntime>>>,
}

/// 异步任务(连接循环 + 镜像轮询)的落脚处。
///
/// Tauri 时代这些任务跑在 `tauri::async_runtime::spawn`(Tauri 自带的全局 tokio
/// 运行时)上;GPUI 壳里没有这个东西,故默认自持一个小的多线程运行时。
/// 宿主若已有 tokio 运行时,用 [`MobileRelayManager::with_runtime`] 注入它的
/// Handle,避免进程里多出一个线程池。
enum Spawner {
    Owned(tokio::runtime::Runtime),
    External(tokio::runtime::Handle),
}

impl Spawner {
    fn handle(&self) -> &tokio::runtime::Handle {
        match self {
            Spawner::Owned(rt) => rt.handle(),
            Spawner::External(h) => h,
        }
    }
}

/// 连接状态(serde camelCase 与移动端面板的展示模型对齐)。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MobileRelayStatusPayload {
    /// "disconnected" | "connecting" | "connected" | "reconnecting" | "versionMismatch"
    /// | "authFailed"(密钥不匹配)| "keyNotConfigured"(中转未配置密钥)。
    /// 后三者都是配置问题:停止重连,等用户改配置。
    pub status: String,
    /// versionMismatch 时携带,供 UI 给出明确升级提示
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_version: Option<u32>,
    /// 移动端配对状态(中转 PairingUpdate 推送);None = 尚未知悉(未连上中转)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired: Option<bool>,
}

impl MobileRelayStatusPayload {
    fn simple(status: &str) -> Self {
        Self {
            status: status.into(),
            expected_version: None,
            actual_version: None,
            paired: None,
        }
    }
}

/// 每 pane 留存的移动端指令登记上限(含已认领的)。已认领的要留着:移动端退出
/// 镜像再进 = 重订阅 = 整份重解析,得靠它们把早先的移动端指令再认回来,
/// 一屏快照([`MIRROR_PAGE_SIZE`] 条)里的都该认得回。
const MOBILE_CMD_KEEP: usize = MIRROR_PAGE_SIZE;

/// 回流消息时间戳相对登记时刻的容差:登记在 PTY 投递之后,grok 的时间戳又只到秒,
/// 真回显的时间戳可能略早于登记时刻。
const MOBILE_ECHO_SLACK: Duration = Duration::from_secs(5);

/// 一条已写入 PTY 的移动端指令原文(或点选作答的选中项),等会话记录回流时改标来源。
struct MobileCmd {
    text: String,
    /// 登记时刻:回流消息的时间戳比它早(超出容差)就不可能是这条指令的回显
    recorded_at: SystemTime,
    /// 认领了这条登记的回流消息的时间戳;None = 回显还没到
    claimed_ts: Option<String>,
}

/// 改标口径:增量新行与整份历史分开对待。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelabelScope {
    /// 增量泵读到的新行:只认未认领的登记。时间戳解析不了照样放行 —— 新行本就是
    /// 刚落盘的,这是改标一直以来的口径
    Appended,
    /// 换绑 / 重订阅整份解析出的历史:已认领的登记按 (原文, 时间戳) 认回原来那条;
    /// 未认领的只认时间戳不早于登记时刻的消息,解析不了的一律不认 —— 历史里
    /// 同文的旧桌面输入不能被近期的移动端指令认走
    History,
}

impl MobileCmd {
    fn accepts(&self, text: &str, ts: &str, at: Option<SystemTime>, scope: RelabelScope) -> bool {
        if self.text != text {
            return false;
        }
        match &self.claimed_ts {
            // 已认领:只在整份重解析时认回同一条消息
            Some(claimed) => scope == RelabelScope::History && !ts.is_empty() && claimed == ts,
            None => match at {
                Some(at) => at + MOBILE_ECHO_SLACK >= self.recorded_at,
                None => scope == RelabelScope::Appended,
            },
        }
    }
}

/// 镜像消息时间戳(RFC 3339,四家解析器都产出这个形态)→ 时刻;空串 / 认不得 → None。
fn parse_mirror_timestamp(ts: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(ts.trim())
        .ok()
        .map(SystemTime::from)
}

/// 为回流消息里的一段原文找一条登记(跳过这一轮改标里已被占用的 `used`):
/// 先找已认领的(重解析时认回同一条),再找未认领的。
fn find_mobile_cmd(
    list: &[MobileCmd],
    used: &[usize],
    text: &str,
    ts: &str,
    at: Option<SystemTime>,
    scope: RelabelScope,
) -> Option<usize> {
    let ok = |i: &usize| !used.contains(i) && list[*i].accepts(text, ts, at, scope);
    (0..list.len())
        .filter(|i| list[*i].claimed_ts.is_some())
        .find(ok)
        .or_else(|| {
            (0..list.len())
                .filter(|i| list[*i].claimed_ts.is_none())
                .find(ok)
        })
}

/// 当前连接会话的取消句柄;整个 manager 由上层用 `Arc` 长期持有。
pub struct MobileRelayManager {
    cancel: Mutex<Option<watch::Sender<bool>>>,
    status: Mutex<MobileRelayStatusPayload>,
    /// 已连接会话的出站消息通道(请求配对码/重置配对经此送往中转)
    outbound: Mutex<Option<mpsc::UnboundedSender<DesktopToRelay>>>,
    /// 最近一次 PairingUpdate 的配对状态(断线清空)
    paired: Mutex<Option<bool>>,
    /// 项目结构的最新快照(上层 store 经 update_sessions 喂入,后端据此组装增量)
    sessions: Mutex<Vec<MobileProject>>,
    /// pane → 项目路径(镜像订阅时解析会话文件用)
    pane_paths: Mutex<HashMap<String, String>>,
    /// pane → PTY id(移动端指令写穿目标)
    pane_ptys: Mutex<HashMap<String, u32>>,
    /// 已订阅镜像的 pane 集合
    mirror_subs: Mutex<HashMap<String, MirrorSub>>,
    /// 每 pane 最近成功写入的移动端指令原文:会话记录回流时把匹配的
    /// user 消息来源改标为 "mobile"(见 relabel_mobile_sources)
    recent_mobile_cmds: Mutex<HashMap<String, Vec<MobileCmd>>>,
    /// 上一条移动端指令写完(含提交回车)的信号:指令按到达顺序串行写入,
    /// 见 [`handle_mobile_command`]
    command_tail: Mutex<Option<oneshot::Receiver<()>>>,
    /// 指令正文与提交回车之间的间隔:生产恒为 [`MOBILE_SUBMIT_DELAY`],单测改短
    submit_delay: Duration,
    /// 桌面侧状态查询(项目表 / 启动器 / PTY 写穿 / AI 会话身份)
    host: Arc<dyn RelayHost>,
    /// 状态与动作出口(原来的四个 Tauri 事件)
    events: Arc<dyn RelayEvents>,
    /// 异步任务的落脚运行时,首次 `apply` 时惰性创建
    spawner: OnceLock<Option<Spawner>>,
}

impl MobileRelayManager {
    /// 自持一个后台 tokio 运行时(2 个工作线程,首次 [`apply`] 时才创建)。
    ///
    /// [`apply`]: MobileRelayManager::apply
    pub fn new(host: Arc<dyn RelayHost>, events: Arc<dyn RelayEvents>) -> Self {
        Self {
            cancel: Mutex::new(None),
            status: Mutex::new(MobileRelayStatusPayload::simple("disconnected")),
            outbound: Mutex::new(None),
            paired: Mutex::new(None),
            sessions: Mutex::new(Vec::new()),
            pane_paths: Mutex::new(HashMap::new()),
            pane_ptys: Mutex::new(HashMap::new()),
            mirror_subs: Mutex::new(HashMap::new()),
            recent_mobile_cmds: Mutex::new(HashMap::new()),
            command_tail: Mutex::new(None),
            submit_delay: MOBILE_SUBMIT_DELAY,
            host,
            events,
            spawner: OnceLock::new(),
        }
    }

    /// 复用调用方已有的 tokio 运行时(需要 time + net 两个驱动)。
    pub fn with_runtime(
        host: Arc<dyn RelayHost>,
        events: Arc<dyn RelayEvents>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        let manager = Self::new(host, events);
        let _ = manager.spawner.set(Some(Spawner::External(handle)));
        manager
    }

    /// 把一个后台任务丢上运行时;运行时建不起来时返回 false(仅在系统资源耗尽时发生)。
    fn spawn<F>(&self, fut: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let spawner = self.spawner.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("mt-relay")
                .enable_all()
                .build()
                .map(Spawner::Owned)
                .map_err(|e| eprintln!("[mobile-relay] tokio runtime unavailable: {e}"))
                .ok()
        });
        match spawner {
            Some(s) => {
                s.handle().spawn(fut);
                true
            }
            None => false,
        }
    }

    fn set_status(&self, mut payload: MobileRelayStatusPayload) {
        // 断开/重连中时配对状态不可知,清空避免陈旧值误导 UI
        if payload.status != "connected" {
            *self.paired.lock() = None;
        }
        payload.paired = *self.paired.lock();
        *self.status.lock() = payload.clone();
        self.events.status_changed(payload);
    }

    /// 中转推送 PairingUpdate 时更新配对状态并重发 status。
    fn set_paired(&self, paired: bool) {
        *self.paired.lock() = Some(paired);
        let mut payload = self.status.lock().clone();
        payload.paired = Some(paired);
        *self.status.lock() = payload.clone();
        self.events.status_changed(payload);
    }

    /// 查询当前连接状态(打开设置页时取初始值,后续靠 [`RelayEvents`] 增量更新)。
    pub fn current_status(&self) -> MobileRelayStatusPayload {
        self.status.lock().clone()
    }

    /// 向中转发送消息(仅已连接时可用)。
    fn send(&self, msg: DesktopToRelay) -> Result<(), String> {
        let outbound = self.outbound.lock();
        match outbound.as_ref() {
            Some(tx) => tx.send(msg).map_err(|_| "connection closing".into()),
            None => Err("not connected to relay".into()),
        }
    }

    /// 请求中转签发一次性配对码;结果经 [`RelayEvents::pairing_code`] 推回。
    pub fn request_pairing_code(&self) -> Result<(), String> {
        self.send(DesktopToRelay::RequestPairingCode)
    }

    /// 重置配对:吊销移动端全部凭证;结果经状态回调的 `paired` 字段推回。
    pub fn reset_pairing(&self) -> Result<(), String> {
        self.send(DesktopToRelay::ResetPairing)
    }

    /// 启动器配置变化后重发一次全量快照(不为启动器单开增量消息)。
    pub fn launchers_changed(&self) {
        self.send_snapshot();
    }

    /// 接收上层 store 喂入的项目全量状态:组装增量推给中转,存下新状态,
    /// 更新 pane→路径映射;被订阅镜像的 pane 消失时通知移动端并撤销订阅。
    pub fn update_sessions(&self, projects: Vec<SyncProject>) {
        let mut pane_paths: HashMap<String, String> = HashMap::new();
        let mut pane_ptys: HashMap<String, u32> = HashMap::new();
        for p in &projects {
            for pane in &p.panes {
                pane_paths.insert(pane.pane_id.clone(), p.path.clone());
                if let Some(pty_id) = pane.pty_id {
                    pane_ptys.insert(pane.pane_id.clone(), pty_id);
                }
            }
        }
        *self.pane_ptys.lock() = pane_ptys;

        // 订阅中的 pane 已不在活跃集合(pane 关闭/AI 会话结束)→ PaneClosed
        let gone: Vec<String> = {
            let subs = self.mirror_subs.lock();
            subs.keys()
                .filter(|id| !pane_paths.contains_key(*id))
                .cloned()
                .collect()
        };
        for pane_id in gone {
            self.unsubscribe_pane(&pane_id);
            let _ = self.send(DesktopToRelay::PaneClosed { pane_id });
        }
        *self.pane_paths.lock() = pane_paths;

        let next: Vec<MobileProject> = projects
            .into_iter()
            .map(|p| MobileProject {
                can_start_session: can_start_session(&p.path, p.ssh_connection_id.as_deref()),
                project_id: p.project_id,
                name: p.name,
                group_path: p.group_path,
                panes: p
                    .panes
                    .into_iter()
                    .map(|x| MobilePane {
                        pane_id: x.pane_id,
                        title: x.title,
                        status: x.status,
                        needs_attention: x.needs_attention,
                    })
                    .collect(),
            })
            .collect();

        let delta = {
            let mut sessions = self.sessions.lock();
            let delta = diff_sessions(&sessions, &next);
            *sessions = next;
            delta
        };
        if let Some((upserts, removed_project_ids)) = delta {
            // 未连接/无移动端时发送失败无妨:移动端上线会拿到全量快照
            let _ = self.send(DesktopToRelay::SessionsDelta {
                upserts,
                removed_project_ids,
            });
        }
    }

    /// 回发一条发起会话回执(成功带 pane_id,失败带 reason)。
    fn send_start_receipt(
        &self,
        request_id: String,
        pane_id: Option<String>,
        reason: Option<StartSessionFailReason>,
    ) {
        let _ = self.send(DesktopToRelay::StartSessionReceipt {
            request_id,
            ok: reason.is_none(),
            pane_id,
            reason,
        });
    }

    /// 桌面侧执行完发起流程后回执:ok = pane 已建且启动命令已写入 PTY。
    /// `reason` 仅失败时携带。
    pub fn start_session_result(
        &self,
        request_id: String,
        ok: bool,
        pane_id: Option<String>,
        reason: Option<StartSessionFailReason>,
    ) {
        self.send_start_receipt(
            request_id,
            if ok { pane_id } else { None },
            if ok {
                None
            } else {
                reason.or(Some(StartSessionFailReason::SpawnFailed))
            },
        );
    }

    /// 撤销单个镜像订阅(幂等)。
    fn unsubscribe_pane(&self, pane_id: &str) {
        if let Some(sub) = self.mirror_subs.lock().remove(pane_id) {
            let _ = sub.cancel_tx.send(true);
        }
    }

    /// 撤销全部镜像订阅(与中转断线时调用;移动端重连后会重新订阅)。
    fn clear_mirror_subs(&self) {
        let subs: Vec<MirrorSub> = self
            .mirror_subs
            .lock()
            .drain()
            .map(|(_, s)| s)
            .collect();
        for sub in subs {
            let _ = sub.cancel_tx.send(true);
        }
    }

    /// 登记一条已写入 PTY 的移动端指令原文(镜像回流改标来源用,每 pane 上限
    /// [`MOBILE_CMD_KEEP`] 条)。
    fn record_mobile_cmd(&self, pane_id: &str, text: &str) {
        self.record_mobile_cmd_at(pane_id, text, SystemTime::now());
    }

    fn record_mobile_cmd_at(&self, pane_id: &str, text: &str, recorded_at: SystemTime) {
        let mut map = self.recent_mobile_cmds.lock();
        let list = map.entry(pane_id.to_string()).or_default();
        list.push(MobileCmd {
            text: text.trim().to_string(),
            recorded_at,
            claimed_ts: None,
        });
        if list.len() > MOBILE_CMD_KEEP {
            list.remove(0);
        }
    }

    /// 镜像消息回流时调用:与移动端指令登记逐字匹配的 user 消息改标 "mobile"。
    /// 不匹配保持 "desktop"(误差方向安全:最多把移动端指令标成桌面输入)。
    ///
    /// 一条登记只认领一条消息(认领后记下该消息的时间戳,不再匹配新消息);
    /// 整份历史([`RelabelScope::History`])另有两条约束,见该枚举。
    /// 只持 `recent_mobile_cmds` 一把锁且期间不碰别的锁(叶子锁):增量泵持着
    /// runtime 锁调、换绑在不持锁时调,两种顺序都不会与点选作答的泵死锁。
    fn relabel_mobile_sources(
        &self,
        pane_id: &str,
        messages: &mut [MirrorMessage],
        scope: RelabelScope,
    ) {
        let mut map = self.recent_mobile_cmds.lock();
        let Some(list) = map.get_mut(pane_id) else {
            return;
        };
        // 这一轮已被认领 / 认回的登记:同一轮里一条登记只配一条消息
        // (重解析时时间戳与原文都相同的两条消息,也只认回一条)
        let mut used: Vec<usize> = Vec::new();
        for msg in messages.iter_mut() {
            if msg.source != "desktop" {
                continue;
            }
            // 作答标记按结构化 labels 对账——多题标记的 content 是合并文本,
            // 与逐条登记的单个 label 对不上。全部选中项都出自本端登记才改标;
            // 打断标记(labels 为空)不参与
            let texts: Vec<&str> = if msg.kind.as_deref() == Some("questionAnswered") {
                msg.labels.iter().map(String::as_str).collect()
            } else {
                vec![msg.content.trim()]
            };
            if texts.is_empty() {
                continue;
            }
            let at = parse_mirror_timestamp(&msg.timestamp);
            let start = used.len();
            for text in &texts {
                match find_mobile_cmd(list, &used, text, &msg.timestamp, at, scope) {
                    Some(i) => used.push(i),
                    None => break,
                }
            }
            if used.len() - start == texts.len() {
                msg.source = "mobile".into();
                for &i in &used[start..] {
                    list[i].claimed_ts = Some(msg.timestamp.clone());
                }
            } else {
                used.truncate(start);
            }
        }
    }

    /// 回发一条指令/作答回执(成功时 reason 为 None)。
    fn send_command_receipt(
        &self,
        pane_id: String,
        command_id: String,
        result: Result<(), CommandFailReason>,
    ) {
        let _ = self.send(DesktopToRelay::CommandReceipt {
            pane_id,
            command_id,
            ok: result.is_ok(),
            reason: result.err(),
        });
    }

    /// 把一条移动端指令写穿到 pane:正文 → 等 [`Self::submit_delay`] → 单独一个 `\r`
    /// (为什么分两次写见 [`MOBILE_SUBMIT_DELAY`])。两次写同一个 PTY;回车写不进去
    /// 同样算失败 —— 正文停在输入框里没提交,回执不能报成功。
    ///
    /// 等待是 `tokio::time::sleep`,不占运行时线程,也不持任何锁。
    async fn write_mobile_command(
        &self,
        pane_id: &str,
        text: &str,
    ) -> Result<(), CommandFailReason> {
        let pty_id = self.pane_ptys.lock().get(pane_id).copied();
        let pty_id = pty_id.ok_or(CommandFailReason::PaneNotFound)?;
        if !text.is_empty() {
            let body = mobile_command_body(text, self.host.bracketed_paste(pty_id));
            self.host
                .write_pty(pty_id, body)
                .map_err(|_| CommandFailReason::WriteFailed)?;
            tokio::time::sleep(self.submit_delay).await;
        }
        self.host
            .write_pty(pty_id, "\r".into())
            .map_err(|_| CommandFailReason::WriteFailed)
    }

    /// 增量泵一次镜像:读绑定文件的新字节喂解析器,新消息改标来源、并入缓存并
    /// 推送 MirrorAppend。全程持 runtime 锁——泵有两个调用方(1s 轮询与点选
    /// 作答),「读文件→喂解析→挪偏移」必须原子,否则同一段字节会被喂两次。
    /// 文件被截断/重写时清空 runtime,下一轮轮询重新绑定。
    fn pump_mirror(
        &self,
        pane_id: &str,
        messages: &Mutex<Vec<MirrorMessage>>,
        runtime: &Mutex<Option<MirrorRuntime>>,
    ) {
        let mut slot = runtime.lock();
        let Some(rt) = slot.as_mut() else { return };
        match mirror::read_from_offset(&rt.path, rt.offset) {
            Some((bytes, new_offset)) => {
                rt.offset = new_offset;
                if bytes.is_empty() {
                    return;
                }
                let mut new_msgs = rt.parser.feed(&bytes);
                if new_msgs.is_empty() {
                    return;
                }
                // 移动端指令/作答回流:匹配的消息改标 "mobile"
                self.relabel_mobile_sources(pane_id, &mut new_msgs, RelabelScope::Appended);
                messages.lock().extend(new_msgs.clone());
                let _ = self.send(DesktopToRelay::MirrorAppend {
                    pane_id: pane_id.into(),
                    messages: new_msgs,
                });
            }
            None => *slot = None,
        }
    }

    /// 这一轮的定位输入。绑定分两层,每轮重取(PTY 映射、hook 上报都可能后到):
    /// 1. hook 上报过会话身份 → 只认该会话的文件,未落盘就等(空镜像),
    ///    不退启发式——退了就会串到同项目其他 pane 的会话;
    /// 2. 无会话身份(未启用 hook)→ 退回"项目最新文件 + AI 启动时刻下限"启发式。
    ///
    /// 这几问都是查内存表,每轮问一遍没有代价;贵的是拿着答案去扫磁盘,
    /// 那一步由 [`MirrorLocator::needs_locate`] 把关。
    fn mirror_locate_key(&self, pane_id: &str) -> LocateKey {
        let pty_id = self.pane_ptys.lock().get(pane_id).copied();
        if let Some(pty) = pty_id
            && let Some(s) = self.host.hook_session(pty)
        {
            return LocateKey::Hook {
                pty_id: pty,
                agent: s.agent,
                session_id: s.session_id,
            };
        }
        // 启发式的前提是"这个 agent 会往磁盘写我们认识的会话记录"。pi /
        // opencode 不写(或格式不认),此时退启发式就会绑到同项目里 Claude/
        // Codex 的最新文件,把别人的对话贴到这个 pane 上——比空镜像更糟。
        let agent = pty_id.and_then(|id| self.host.ai_session_agent(id));
        if let Some(agent) = agent.as_deref()
            && !mirror::agent_has_session_log(agent)
        {
            return LocateKey::NoLog {
                pty_id,
                agent: agent.to_string(),
            };
        }
        LocateKey::Heuristic {
            pty_id,
            agent,
            started: pty_id.and_then(|id| self.host.ai_session_started_at(id)),
        }
    }

    /// 镜像轮询的一轮:必要时重新定位,换绑或增量泵。**全是阻塞文件 IO**,
    /// 只在阻塞线程池上跑(见 [`mirror_task`])。
    ///
    /// `locate` 是定位函数本身(生产传 [`locate_session_file`]),单测注入计数替身。
    fn mirror_round(
        &self,
        job: &MirrorJob,
        locator: &mut MirrorLocator,
        now: Instant,
        locate: impl Fn(&str, &LocateKey) -> Option<(PathBuf, MirrorAgent)>,
    ) {
        let key = self.mirror_locate_key(&job.pane_id);
        let bound = job.runtime.lock().as_ref().map(|rt| rt.path.clone());
        if locator.needs_locate(&key, bound.as_deref(), now) {
            locator.resolved = locate(&job.project_path, &key);
            locator.key = Some(key);
            locator.at = Some(now);
        }
        match locator.resolved.clone() {
            None => {
                // 属于本轮会话的文件尚未出现(AI 刚启动还没落盘):先给空快照,出现后再重发
                if !locator.sent_initial {
                    locator.sent_initial = true;
                    let _ = self.send(DesktopToRelay::MirrorSnapshot {
                        pane_id: job.pane_id.clone(),
                        messages: vec![],
                        has_more: false,
                    });
                }
            }
            Some((path, agent)) if bound.as_deref() != Some(path.as_path()) => {
                // 首次绑定或换绑到更新的会话文件:全量解析 + 重发快照
                if self.rebind_mirror(job, path, agent) {
                    locator.sent_initial = true;
                }
            }
            // 增量与「文件被截断→清运行态待重绑」都在泵里
            Some(_) => self.pump_mirror(&job.pane_id, &job.messages, &job.runtime),
        }
    }

    /// 换绑:整份解析新文件并重发快照。返回是否绑上(文件读不了 = 没绑上,下一轮再试)。
    ///
    /// 解析用一只新解析器、在锁外做完(文件可能不小,解析可能几百毫秒,不该让点选
    /// 作答的泵干等);换入运行态与替换消息缓存在同一次持锁内完成,防点选作答的泵
    /// 在中间插进来喂错解析器。
    ///
    /// 全量同样要改标来源:移动端发起的新会话里第一条指令、重订阅时早先发过的
    /// 移动端指令都只在这份全量里出现,不改标就一律显示成桌面输入。改标也在锁外
    /// (只取叶子锁 `recent_mobile_cmds`)。
    fn rebind_mirror(&self, job: &MirrorJob, path: PathBuf, agent: MirrorAgent) -> bool {
        let mut parser = MirrorParser::new(agent);
        let Some((mut msgs, offset)) = mirror::parse_whole_file(&path, &mut parser) else {
            return false;
        };
        self.relabel_mobile_sources(&job.pane_id, &mut msgs, RelabelScope::History);
        let (page, has_more) = {
            let mut slot = job.runtime.lock();
            *slot = Some(MirrorRuntime {
                parser,
                path,
                offset,
            });
            let mut m = job.messages.lock();
            *m = msgs;
            history_slice(&m, None, MIRROR_PAGE_SIZE)
        };
        let _ = self.send(DesktopToRelay::MirrorSnapshot {
            pane_id: job.pane_id.clone(),
            messages: page,
            has_more,
        });
        true
    }

    /// 分页取数:从订阅的消息缓存里取 seq < before_seq 的最近一页并回发。
    fn send_mirror_history(&self, pane_id: &str, before_seq: u64) {
        let slice = {
            let subs = self.mirror_subs.lock();
            let Some(sub) = subs.get(pane_id) else { return };
            let messages = sub.messages.lock();
            history_slice(&messages, Some(before_seq), MIRROR_PAGE_SIZE)
        };
        let (messages, has_more) = slice;
        let _ = self.send(DesktopToRelay::MirrorHistory {
            pane_id: pane_id.into(),
            messages,
            has_more,
        });
    }

    /// 发送当前全量快照(握手成功后 / 收到中转的快照请求时 / 启动器配置变化时)。
    /// 启动器名单从宿主现取:它是低频数据,没必要在内存里再维护一份副本。
    fn send_snapshot(&self) {
        let projects = self.sessions.lock().clone();
        let launchers = self
            .host
            .launchers()
            .into_iter()
            .map(|l| MobileLauncher {
                id: l.id,
                name: l.name,
            })
            .collect();
        let _ = self.send(DesktopToRelay::SessionsSnapshot {
            projects,
            launchers,
        });
    }

    /// 应用新的中转地址与桌面端密钥:先停旧连接;地址非空则启动新的重连循环。
    /// 地址为空字符串 = 断开并停用。
    pub fn apply(self: &Arc<Self>, relay_url: &str, desktop_key: &str) {
        if let Some(tx) = self.cancel.lock().take() {
            let _ = tx.send(true);
        }
        let url = match normalize_relay_url(relay_url) {
            Some(u) => u,
            None => {
                self.set_status(MobileRelayStatusPayload::simple("disconnected"));
                return;
            }
        };

        // WSS 需要 rustls CryptoProvider;显式装 ring 后端(依赖树只编译了 ring)。
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (cancel_tx, cancel_rx) = watch::channel(false);
        *self.cancel.lock() = Some(cancel_tx);
        let desktop_key = desktop_key.to_string();
        let manager = Arc::clone(self);
        let spawned = self.spawn(async move {
            connection_loop(manager, url, desktop_key, cancel_rx).await;
        });
        if !spawned {
            self.set_status(MobileRelayStatusPayload::simple("disconnected"));
        }
    }

    /// 建立镜像订阅:绑定 pane 所属项目的最新会话文件,启动轮询任务。
    /// 重复订阅先撤旧再建新(移动端重连后重订阅拿到新快照)。
    fn subscribe_pane(self: &Arc<Self>, pane_id: String) {
        self.unsubscribe_pane(&pane_id);
        let Some(project_path) = self.pane_paths.lock().get(&pane_id).cloned() else {
            // pane 已不存在(或从未同步):直接告知已关闭
            let _ = self.send(DesktopToRelay::PaneClosed { pane_id });
            return;
        };

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let messages = Arc::new(Mutex::new(Vec::new()));
        let runtime = Arc::new(Mutex::new(None));
        self.mirror_subs.lock().insert(
            pane_id.clone(),
            MirrorSub {
                cancel_tx,
                messages: messages.clone(),
                runtime: runtime.clone(),
            },
        );
        let manager = Arc::clone(self);
        let job = Arc::new(MirrorJob {
            pane_id,
            project_path,
            messages,
            runtime,
        });
        self.spawn(async move {
            mirror_task(manager, job, cancel_rx).await;
        });
    }
}

/// 能否在该项目远程发起 AI 会话。
///
/// SSH 远程项目与 WSL 根项目一律为否:它们的对话镜像目前一定是空的
/// (`mirror` 只认本机 Windows 宿主来源),在那儿开会话等于盲发指令。
/// WSL **关联**项目(根路径是普通 Windows 路径)不在此列——它的镜像可用与否取决于
/// 启动器把 AI 起在哪一侧,那是既有的 v1 镜像限制,不由本判定兜底。
pub fn can_start_session(path: &str, ssh_connection_id: Option<&str>) -> bool {
    ssh_connection_id.is_none() && !is_wsl_unc_path(path)
}

/// 一次连接尝试的结局。
enum Attempt {
    /// 握手成功且后来断线(网络抖动/中转重启) → 立即从头重连
    ConnectedThenLost,
    /// 没连上/握手失败 → 退避后重试
    Failed,
    /// 版本不匹配 → 停止循环
    VersionMismatch { expected: u32, actual: u32 },
    /// 密钥被拒(填错 / 中转未配置) → 停止循环,重试无意义
    Rejected(DesktopRejectReason),
    /// 用户取消(改地址/清空地址) → 停止循环,状态由调用方设置
    Cancelled,
}

async fn connection_loop(
    manager: Arc<MobileRelayManager>,
    url: String,
    desktop_key: String,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let mut attempt: u32 = 0;
    loop {
        let status = if attempt == 0 {
            "connecting"
        } else {
            "reconnecting"
        };
        manager.set_status(MobileRelayStatusPayload::simple(status));

        match connect_once(&manager, &url, &desktop_key, &mut cancel_rx).await {
            Attempt::Cancelled => return,
            Attempt::VersionMismatch { expected, actual } => {
                manager.set_status(MobileRelayStatusPayload {
                    status: "versionMismatch".into(),
                    expected_version: Some(expected),
                    actual_version: Some(actual),
                    paired: None,
                });
                return;
            }
            // 配置问题不是网络问题:停在明确状态上,等用户改配置后重新「保存并连接」
            Attempt::Rejected(reason) => {
                manager.set_status(MobileRelayStatusPayload::simple(reject_status(reason)));
                return;
            }
            Attempt::ConnectedThenLost => attempt = 1,
            Attempt::Failed => attempt = attempt.saturating_add(1),
        }

        let delay = backoff_delay(attempt);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel_rx.changed() => return,
        }
    }
}

/// 握手拒绝原因 → 上层状态串(两种密钥问题的修法不同,文案也不同)。
fn reject_status(reason: DesktopRejectReason) -> &'static str {
    match reason {
        DesktopRejectReason::InvalidKey => "authFailed",
        DesktopRejectReason::KeyNotConfigured => "keyNotConfigured",
        // 版本不匹配走 Attempt::VersionMismatch 分支,不会到这里
        DesktopRejectReason::VersionMismatch => "versionMismatch",
    }
}

/// 单次连接:建连 → hello(带密钥)→ 等 ack → 已连接后挂住直到断线/取消。
async fn connect_once(
    manager: &Arc<MobileRelayManager>,
    url: &str,
    desktop_key: &str,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Attempt {
    let connect = tokio_tungstenite::connect_async(url);
    let mut ws = tokio::select! {
        r = connect => match r {
            Ok((ws, _)) => ws,
            Err(e) => {
                eprintln!("[mobile-relay] connect failed: {e}");
                return Attempt::Failed;
            }
        },
        _ = cancel_rx.changed() => return Attempt::Cancelled,
    };

    let hello = DesktopToRelay::Hello {
        protocol_version: PROTOCOL_VERSION,
        desktop_key: desktop_key.to_string(),
    };
    if ws
        .send(Message::Text(
            serde_json::to_string(&hello).unwrap().into(),
        ))
        .await
        .is_err()
    {
        return Attempt::Failed;
    }

    // 等待握手响应
    let ack = tokio::select! {
        r = tokio::time::timeout(HANDSHAKE_TIMEOUT, ws.next()) => r,
        _ = cancel_rx.changed() => return Attempt::Cancelled,
    };
    match ack {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<RelayToDesktop>(&text) {
            Ok(RelayToDesktop::HelloAck { .. }) => {}
            Ok(RelayToDesktop::HelloReject {
                reason,
                expected_version,
                actual_version,
            }) => {
                return match reason {
                    DesktopRejectReason::VersionMismatch => Attempt::VersionMismatch {
                        expected: expected_version.unwrap_or(PROTOCOL_VERSION),
                        actual: actual_version.unwrap_or(PROTOCOL_VERSION),
                    },
                    other => Attempt::Rejected(other),
                }
            }
            // 握手期不该出现其他消息;当协议错乱处理
            Ok(_) | Err(_) => return Attempt::Failed,
        },
        _ => return Attempt::Failed,
    }

    manager.set_status(MobileRelayStatusPayload::simple("connected"));

    // 注册出站通道(配对码请求/重置配对/结构快照经此发送)
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<DesktopToRelay>();
    *manager.outbound.lock() = Some(outbound_tx);

    // 连上即推一份全量快照:覆盖"桌面端重连时移动端已在线"的场景
    manager.send_snapshot();

    // 已连接:读循环 + 出站转发,直到断线/取消
    let outcome = loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => handle_relay_message(manager, &text),
                Some(Ok(Message::Close(_))) | None => break Attempt::ConnectedThenLost,
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    eprintln!("[mobile-relay] connection lost: {e}");
                    break Attempt::ConnectedThenLost;
                }
            },
            out = outbound_rx.recv() => {
                if let Some(msg) = out {
                    let text = serde_json::to_string(&msg).unwrap();
                    if ws.send(Message::Text(text.into())).await.is_err() {
                        break Attempt::ConnectedThenLost;
                    }
                }
            },
            _ = cancel_rx.changed() => {
                let _ = ws.close(None).await;
                break Attempt::Cancelled;
            }
        }
    };
    *manager.outbound.lock() = None;
    // 断线后镜像推送无处可去,撤销全部订阅;移动端重连会重新订阅
    manager.clear_mirror_subs();
    outcome
}

/// 处理中转推来的消息(已握手连接上)。
fn handle_relay_message(manager: &Arc<MobileRelayManager>, text: &str) {
    match serde_json::from_str::<RelayToDesktop>(text) {
        Ok(RelayToDesktop::PairingCode { code }) => manager.events.pairing_code(code),
        Ok(RelayToDesktop::PairingUpdate { paired }) => manager.set_paired(paired),
        // 移动端上线,回发最新结构快照(中转不缓存)
        Ok(RelayToDesktop::SessionsSnapshotRequest) => manager.send_snapshot(),
        // 对话镜像:订阅/退订/分页
        Ok(RelayToDesktop::SubscribePane { pane_id }) => manager.subscribe_pane(pane_id),
        Ok(RelayToDesktop::UnsubscribePane { pane_id }) => manager.unsubscribe_pane(&pane_id),
        Ok(RelayToDesktop::RequestMirrorHistory {
            pane_id,
            before_seq,
        }) => manager.send_mirror_history(&pane_id, before_seq),
        // 移动端指令:按到达顺序写穿目标 pane 的 PTY(正文与回车分两次写),写完回执
        Ok(RelayToDesktop::MobileCommand {
            pane_id,
            command_id,
            text,
        }) => handle_mobile_command(manager, pane_id, command_id, text),
        // 移动端点选作答 agent 提问:校验挂起后注入按键,回执复用 CommandReceipt
        Ok(RelayToDesktop::AnswerQuestion {
            pane_id,
            command_id,
            seq,
            question_id,
            question_index,
            option_index,
        }) => handle_answer_question(
            manager,
            pane_id,
            command_id,
            seq,
            question_id,
            question_index,
            option_index,
        ),
        // 移动端重命名会话:pane 标题归上层布局状态所有,本 crate 只做长度收敛后转交
        Ok(RelayToDesktop::RenamePane { pane_id, title }) => {
            manager.events.rename_pane(RenamePanePayload {
                pane_id,
                title: sanitize_pane_title(&title),
            });
        }
        // 移动端发起新 AI 会话:校验后交给上层建 pane(PTY 与布局都归上层管)
        Ok(RelayToDesktop::StartAiSession {
            request_id,
            project_id,
            launcher_id,
        }) => handle_start_ai_session(manager, request_id, project_id, launcher_id),
        Ok(_) => {}
        Err(_) => eprintln!("[mobile-relay] unparseable relay message (ignored)"),
    }
}

/// pane 自定义标题的字符数上限。桌面端 tab 栏是一行横排,超长标题会把同组其它
/// tab 挤出可视区;截断而不是拒绝——用户改的名字过长是手滑,不该整条改名失败。
const MAX_PANE_TITLE_CHARS: usize = 64;

/// 收敛移动端传来的标题:去首尾空白、砍掉控制字符、限长。
/// 空串是合法输入(= 清除自定义名),原样交给上层处理。
fn sanitize_pane_title(title: &str) -> String {
    title
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PANE_TITLE_CHARS)
        .collect()
}

/// [`RelayEvents::rename_pane`] 的载荷:交给上层改布局里那个 pane 的自定义标题。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamePanePayload {
    pub pane_id: String,
    /// 已收敛过的标题;空串 = 清除自定义名,回落 shell 名
    pub title: String,
}

/// [`RelayEvents::start_session`] 的载荷:校验通过后交给上层执行的启动指令。
/// 命令与 shell 只在桌面端进程内流转,不回传中转(ADR 0002)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartSessionPayload {
    pub request_id: String,
    pub project_id: String,
    pub launcher_id: String,
    /// 启动器展示名(桌面端通知文案用)
    pub launcher_name: String,
    /// 绑定的 shell 名;None = 用默认 shell
    pub shell_name: Option<String>,
    /// 要写入 PTY 的启动命令
    pub command: String,
}

/// 校验移动端的发起请求,通过则交给上层执行,不通过直接回失败回执。
///
/// 校验的是"目标存在且支持",**不**校验命令内容——命令来自桌面端配置,
/// 这是 ADR 0002 的防线本身,再做内容白名单也挡不住拼接。
fn handle_start_ai_session(
    manager: &Arc<MobileRelayManager>,
    request_id: String,
    project_id: String,
    launcher_id: String,
) {
    let Some(project) = manager.host.project(&project_id) else {
        manager.send_start_receipt(request_id, None, Some(StartSessionFailReason::ProjectNotFound));
        return;
    };
    if !can_start_session(&project.path, project.ssh_connection_id.as_deref()) {
        manager.send_start_receipt(request_id, None, Some(StartSessionFailReason::NotSupported));
        return;
    }
    let launchers = manager.host.launchers();
    let launcher = match resolve_launcher(&launchers, &launcher_id) {
        Ok(l) => l,
        Err(reason) => {
            manager.send_start_receipt(request_id, None, Some(reason));
            return;
        }
    };

    manager.events.start_session(StartSessionPayload {
        request_id,
        project_id,
        launcher_id,
        launcher_name: launcher.name,
        shell_name: launcher.shell,
        command: launcher.command,
    });
}

/// launcher id → 启动这次会话需要的东西。空白 shell 名等同未绑定(不能拿去
/// `available_shells` 里找一个空名条目)。
fn resolve_launcher(
    launchers: &[AiLauncher],
    launcher_id: &str,
) -> Result<AiLauncher, StartSessionFailReason> {
    launchers
        .iter()
        .find(|l| l.id == launcher_id)
        .map(|l| AiLauncher {
            shell: l.shell.clone().filter(|s| !s.trim().is_empty()),
            ..l.clone()
        })
        .ok_or(StartSessionFailReason::LauncherNotFound)
}

/// 写穿移动端指令:等价本人在桌面对该终端粘贴同样内容、再按回车。走宿主的
/// 写穿口(输入跟踪 / AI marker / SSH autofill 解除全语义);AI 工作中依赖 CLI
/// 自身的输入缓冲。回执仅表示"已写入 PTY"(正文与回车都写进去了),AI 真正接收
/// 以镜像回流为准。
///
/// 正文与回车之间要隔一段([`MOBILE_SUBMIT_DELAY`]),不能在连接循环里干等:整条
/// 丢进运行时异步写。多条指令**按到达顺序串行**——每条先等上一条连同回车写完
/// 再动手,否则紧挨着到的两条会交错成「正文A 正文B \r \r」。接龙(换上自己的完成
/// 信号、拿走上一条的)在这里同步做,连接循环又是逐条处理消息,队序即到达序。
fn handle_mobile_command(
    manager: &Arc<MobileRelayManager>,
    pane_id: String,
    command_id: String,
    text: String,
) {
    let (done_tx, done_rx) = oneshot::channel::<()>();
    let prev = manager.command_tail.lock().replace(done_rx);
    let task_manager = Arc::clone(manager);
    // 连接循环本就跑在这个运行时上,不会落到「运行时建不起来」那一支
    manager.spawn(async move {
        // 上一条的任务没了(panic / 运行时收摊)时 Err,同样放行
        if let Some(prev) = prev {
            let _ = prev.await;
        }
        let result = task_manager.write_mobile_command(&pane_id, &text).await;
        if result.is_ok() {
            task_manager.record_mobile_cmd(&pane_id, &text);
        }
        task_manager.send_command_receipt(pane_id, command_id, result);
        let _ = done_tx.send(());
    });
}

/// 移动端指令正文 → 写进 PTY 的那一段(不含提交回车)。
///
/// - bracketed paste 开着:与桌面端粘贴(mt-ui `paste_to_bytes`)同一编码 —— 换行
///   归一成 `\r`、剔掉正文里的结束标记(防正文提前「结束粘贴」、后半段被当按键)、
///   包上 `ESC[200~ … ESC[201~`。多行指令因此整块是粘贴内容,中途不会提交;
/// - 没开:裸写,换行一律归一成 `\n`(LF = Ctrl+J,Claude Code 输入框里是「插入
///   换行」),只让随后单独写的那个 `\r` 承担提交。
fn mobile_command_body(text: &str, bracketed: bool) -> String {
    if bracketed {
        let body = text
            .replace("\r\n", "\r")
            .replace('\n', "\r")
            .replace("\x1b[201~", "");
        format!("\x1b[200~{body}\x1b[201~")
    } else {
        text.replace("\r\n", "\n").replace('\r', "\n")
    }
}

/// 移动端点选作答:回执复用 CommandReceipt,成功时登记选中项供回流改标来源。
fn handle_answer_question(
    manager: &Arc<MobileRelayManager>,
    pane_id: String,
    command_id: String,
    seq: u64,
    question_id: String,
    question_index: u32,
    option_index: u32,
) {
    let result = try_answer_question(
        manager,
        &pane_id,
        seq,
        &question_id,
        question_index,
        option_index,
    );
    if let Ok(label) = &result {
        // 登记选中项:作答回流的 questionAnswered 标记按 labels 对账改标 "mobile"
        manager.record_mobile_cmd(&pane_id, label);
    }
    manager.send_command_receipt(pane_id, command_id, result.map(|_| ()));
}

/// 点选作答的校验与注入,Ok 携带选中项 label。
///
/// 先增量泵一次镜像——桌面刚作答/打断而轮询还没读到时,靠泵把这 1s 窗口压到
/// 毫秒级,否则挂起校验会放行一次盲注(按键落进普通输入框)。校验→写入→推进
/// 进度在同一次 runtime 持锁内完成,防两次点按撞进度;write_pty 只是投递
/// channel(乐观 Ok),持锁跨它没有阻塞与重入风险。
/// 仍防不住的残余:桌面只把高亮移走没作答、多题在桌面先答了一题(都不落
/// 会话记录),这类错位注入无法从记录侧感知。
fn try_answer_question(
    manager: &Arc<MobileRelayManager>,
    pane_id: &str,
    seq: u64,
    question_id: &str,
    question_index: u32,
    option_index: u32,
) -> Result<String, CommandFailReason> {
    // 克隆出共享句柄立即放开订阅表锁:泵要做文件 IO,不该顶着 subs 锁做
    let handles = manager
        .mirror_subs
        .lock()
        .get(pane_id)
        .map(|sub| (sub.messages.clone(), sub.runtime.clone()));
    let Some((messages, runtime)) = handles else {
        return Err(CommandFailReason::QuestionNotPending);
    };
    manager.pump_mirror(pane_id, &messages, &runtime);

    let mut slot = runtime.lock();
    let Some(rt) = slot.as_mut() else {
        return Err(CommandFailReason::QuestionNotPending);
    };
    let Some(answer) = rt
        .parser
        .answer_keys(seq, question_id, question_index, option_index)
    else {
        return Err(CommandFailReason::QuestionNotPending);
    };
    let Some(pty_id) = manager.pane_ptys.lock().get(pane_id).copied() else {
        return Err(CommandFailReason::PaneNotFound);
    };
    manager
        .host
        .write_pty(pty_id, answer.keys)
        .map_err(|_| CommandFailReason::WriteFailed)?;
    rt.parser.mark_answered(seq);
    Ok(answer.label)
}

/// 镜像轮询任务:解析 pane 应镜像的最新会话文件,增量读取新行推送;
/// 出现更新的会话文件时重新绑定并重发快照。
///
/// 每一轮([`MobileRelayManager::mirror_round`])整个丢进 tokio 的**阻塞线程池**:
/// 定位要扫会话目录、换绑要把整份记录解析一遍、增量泵也是文件读,而中转运行时
/// 只有两个工作线程,WebSocket 收发也在上面 —— 让它们去等磁盘,移动端的指令
/// 回执与镜像推送就一起卡住。
async fn mirror_task(
    manager: Arc<MobileRelayManager>,
    job: Arc<MirrorJob>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let mut locator = MirrorLocator::default();

    loop {
        let round = {
            let manager = manager.clone();
            let job = job.clone();
            let mut locator = std::mem::take(&mut locator);
            tokio::task::spawn_blocking(move || {
                manager.mirror_round(&job, &mut locator, Instant::now(), locate_session_file);
                locator
            })
        };
        // 阻塞任务 panic / 运行时收摊:这条订阅就此结束(与原来任务体 panic 同结局)
        locator = match round.await {
            Ok(locator) => locator,
            Err(_) => return,
        };

        tokio::select! {
            _ = tokio::time::sleep(MIRROR_POLL_INTERVAL) => {}
            _ = cancel_rx.changed() => return,
        }
    }
}

/// 组装结构增量:整项目 upsert(新增或内容变化)+ 项目移除。无变化返回 None。
fn diff_sessions(
    prev: &[MobileProject],
    next: &[MobileProject],
) -> Option<(Vec<MobileProject>, Vec<String>)> {
    let prev_map: HashMap<&str, &MobileProject> =
        prev.iter().map(|p| (p.project_id.as_str(), p)).collect();
    let mut upserts: Vec<MobileProject> = Vec::new();
    for p in next {
        match prev_map.get(p.project_id.as_str()) {
            Some(old) if **old == *p => {}
            _ => upserts.push(p.clone()),
        }
    }

    let next_ids: HashSet<&str> = next.iter().map(|p| p.project_id.as_str()).collect();
    let removed: Vec<String> = prev
        .iter()
        .filter(|p| !next_ids.contains(p.project_id.as_str()))
        .map(|p| p.project_id.clone())
        .collect();

    if upserts.is_empty() && removed.is_empty() {
        None
    } else {
        Some((upserts, removed))
    }
}

/// 指数退避:1s → 2s → 4s → … 封顶 60s。attempt 从 1 计。
fn backoff_delay(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.saturating_sub(1).min(6); // 1,2,4,8,16,32,64
    Duration::from_secs(secs.min(60))
}

/// 用户输入的中转地址 → 桌面端 WebSocket 端点 URL。
///
/// 接受 wss/ws/https/http 前缀或无前缀(默认 wss);去尾部斜杠后拼 `/ws/desktop`。
/// 空白输入返回 None(= 未配置,不建连)。
fn normalize_relay_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let with_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        trimmed.to_string()
    } else {
        format!("wss://{trimmed}")
    };
    Some(format!("{}/ws/desktop", with_scheme.trim_end_matches('/')))
}

/// 中转设置界面要不要亮「明文传输」警告:地址会走 `ws://`(非 TLS),且主机
/// 不是回环 / 私网地址。
///
/// 前缀判定与 [`normalize_relay_url`] 逐条对齐:`ws://` 与被映射过去的 `http://`
/// 才算明文;`wss://` / `https://` / 无前缀(默认 wss)都不算。走明文时桌面端密钥
/// 在握手里、移动端指令在帧里,链路上谁都看得见 —— 拿到密钥就能向中转申请配对码、
/// 顶掉用户的手机并往 PTY 写命令。
///
/// **只警告不阻断**:本机 / 局域网自测、前面有反代终结 TLS 都是正当用法,所以
/// 回环(`localhost` / 127.0.0.0/8 / `::1`)与私网(10/8、172.16/12、192.168/16、
/// 169.254/16、fc00::/7、fe80::/10,含 IPv4 映射的 IPv6)地址不提示。认不出的主机名
/// 一律提示 —— 宁可多提醒一句。
pub fn relay_url_needs_tls_warning(input: &str) -> bool {
    let trimmed = input.trim();
    let Some(rest) = trimmed
        .strip_prefix("ws://")
        .or_else(|| trimmed.strip_prefix("http://"))
    else {
        return false;
    };
    // authority 到第一个 `/` `?` `#` 为止;userinfo(`user:pass@`)不是主机
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = relay_url_host(host_port);
    // 还没敲出主机名(输入框里只有 `ws://`)时先不吵
    !host.is_empty() && !is_local_relay_host(&host)
}

/// authority 里去掉端口后的主机名(小写、去掉结尾的 `.`)。
/// `[v6]:port` 取括号内(连同 `%zone` 一起去掉);不带括号却有多个 `:` 的
/// 当作裸 IPv6 整体取(这种写法连不上,但判定上别把它截成半截)。
fn relay_url_host(host_port: &str) -> String {
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else if host_port.matches(':').count() > 1 {
        host_port
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    let host = host.split('%').next().unwrap_or_default();
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// 回环或私网主机(见 [`relay_url_needs_tls_warning`] 的清单)。
fn is_local_relay_host(host: &str) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn local_v4(ip: Ipv4Addr) -> bool {
        ip.is_loopback() || ip.is_private() || ip.is_link_local()
    }

    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return local_v4(ip);
    }
    if let Ok(ip) = host.parse::<Ipv6Addr>() {
        return ip.is_loopback()
            || ip.is_unique_local()
            || ip.is_unicast_link_local()
            || ip.to_ipv4_mapped().is_some_and(local_v4);
    }
    false
}

/// 校验一条启动命令能否被识别为 AI 会话(「移动端」面板保存启动器时的非阻塞提示)。
///
/// 这只是把失败从"手机上等 15 秒超时"前移到配置时,**不是安全防线**:
/// 防线是"命令只能来自桌面端配置"(见 ADR 0002)。
pub fn check_launcher_command(command: &str) -> bool {
    mt_ai::is_interactive_ai_command(command.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::NoopRelayHost;

    /// 不接线的 manager:测的都是纯逻辑(退避 / URL / diff / 改标),
    /// 不碰宿主也不建连接。
    fn test_manager() -> MobileRelayManager {
        MobileRelayManager::new(Arc::new(NoopRelayHost), Arc::new(NoopRelayHost))
    }

    #[test]
    fn backoff_is_exponential_with_cap() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(6), Duration::from_secs(32));
        // 封顶 60s,不随 attempt 溢出
        assert_eq!(backoff_delay(7), Duration::from_secs(60));
        assert_eq!(backoff_delay(100), Duration::from_secs(60));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn normalize_relay_url_schemes() {
        assert_eq!(
            normalize_relay_url("wss://relay.example.com").as_deref(),
            Some("wss://relay.example.com/ws/desktop")
        );
        assert_eq!(
            normalize_relay_url("ws://192.168.1.5:8080").as_deref(),
            Some("ws://192.168.1.5:8080/ws/desktop")
        );
        // http(s) 自动映射到 ws(s)
        assert_eq!(
            normalize_relay_url("https://relay.example.com/").as_deref(),
            Some("wss://relay.example.com/ws/desktop")
        );
        assert_eq!(
            normalize_relay_url("http://localhost:8080").as_deref(),
            Some("ws://localhost:8080/ws/desktop")
        );
        // 无前缀默认 wss(公网默认加密)
        assert_eq!(
            normalize_relay_url("relay.example.com").as_deref(),
            Some("wss://relay.example.com/ws/desktop")
        );
        // 空白 = 未配置
        assert_eq!(normalize_relay_url("   "), None);
        assert_eq!(normalize_relay_url(""), None);
    }

    /// 明文警告只看两件事:最终会不会走 `ws://`、主机是不是回环 / 私网。
    #[test]
    fn tls_warning_only_for_plaintext_ws_to_public_hosts() {
        // 公网主机走明文:要警告(含 http:// 被映射成 ws:// 的那条)
        for url in [
            "ws://relay.example.com",
            "ws://relay.example.com:8080/",
            "http://relay.example.com",
            "  ws://Relay.Example.COM  ",
            "ws://8.8.8.8:8080",
            "ws://172.32.0.1",
            "ws://[2001:db8::1]:8080",
            "ws://user:pass@relay.example.com",
            "ws://relay.example.com.",
            // 不是 `.localhost` 结尾,只是长得像
            "ws://localhost.example.com",
        ] {
            assert!(relay_url_needs_tls_warning(url), "{url} 应当警告");
        }

        // TLS(显式或默认)一律不警告
        for url in [
            "wss://relay.example.com",
            "https://relay.example.com",
            "relay.example.com",
            "",
            "   ",
            // 还没敲出主机名
            "ws://",
        ] {
            assert!(!relay_url_needs_tls_warning(url), "{url:?} 不该警告");
        }

        // 回环 / 私网走明文:不警告
        for url in [
            "ws://localhost:8080",
            "ws://LOCALHOST",
            "ws://relay.localhost",
            "http://127.0.0.1:8080",
            "ws://127.8.9.10",
            "ws://10.0.0.5",
            "ws://172.16.3.4:9000",
            "ws://172.31.255.255",
            "ws://192.168.1.5:8080/ws",
            "ws://169.254.10.20",
            "ws://[::1]:8080",
            "ws://[fd12:3456::1]",
            "ws://[fe80::1%25eth0]:8080",
            "ws://[::ffff:192.168.1.5]",
            "ws://admin@192.168.1.5",
        ] {
            assert!(!relay_url_needs_tls_warning(url), "{url} 不该警告");
        }
    }

    #[test]
    fn relay_url_host_strips_port_brackets_and_zone() {
        assert_eq!(
            relay_url_host("relay.example.com:8080"),
            "relay.example.com"
        );
        assert_eq!(relay_url_host("[::1]:8080"), "::1");
        assert_eq!(relay_url_host("[fe80::1%eth0]"), "fe80::1");
        assert_eq!(relay_url_host("::1"), "::1", "裸 IPv6 不能被截成半截");
        assert_eq!(relay_url_host("Example.COM."), "example.com");
        assert_eq!(relay_url_host(""), "");
    }

    fn project(id: &str, name: &str, panes: &[(&str, &str)]) -> MobileProject {
        MobileProject {
            project_id: id.into(),
            name: name.into(),
            panes: panes
                .iter()
                .map(|(pane_id, status)| mt_relay_protocol::MobilePane {
                    pane_id: (*pane_id).into(),
                    title: "claude".into(),
                    status: (*status).into(),
                    needs_attention: false,
                })
                .collect(),
            can_start_session: true,
            group_path: vec![],
        }
    }

    #[test]
    fn sanitize_pane_title_trims_strips_controls_and_limits_length() {
        assert_eq!(sanitize_pane_title("  重构登录  "), "重构登录");
        // 换行/ESC 之类的控制字符会破坏 tab 栏的单行排版
        assert_eq!(sanitize_pane_title("a\nb\x1b[31mc"), "ab[31mc");
        // 全空白 = 清除自定义名
        assert_eq!(sanitize_pane_title("   "), "");
        // 按字符数限长,不是字节数——中文不该被砍成半个字
        let long = "长".repeat(100);
        assert_eq!(
            sanitize_pane_title(&long).chars().count(),
            MAX_PANE_TITLE_CHARS
        );
    }

    #[test]
    fn can_start_session_rejects_remote_and_wsl_root_projects() {
        // 普通 Windows 本地项目:可发起
        assert!(can_start_session(r"D:\Git\mini-term", None));
        assert!(can_start_session("/home/u/proj", None));

        // SSH 远程项目:镜像一定是空的,置灰
        assert!(!can_start_session("/home/u/proj", Some("conn-1")));

        // WSL 根项目(UNC 路径,含 verbatim 与大小写变体):同样置灰
        assert!(!can_start_session(r"\\wsl$\Ubuntu\home\u\proj", None));
        assert!(!can_start_session(r"\\wsl.localhost\Debian\srv", None));
        assert!(!can_start_session(r"\\?\UNC\wsl$\Ubuntu\home\u", None));
        assert!(!can_start_session(r"\\WSL.LocalHost\Ubuntu\home\u", None));
    }

    #[test]
    fn can_start_session_allows_wsl_associated_project() {
        // WSL「关联」项目的根路径是普通 Windows 路径 —— 它不置灰:
        // 镜像可用与否取决于启动器把 AI 起在哪一侧,不由本判定兜底
        assert!(can_start_session(r"D:\Git\some-wsl-linked-project", None));
    }

    #[test]
    fn diff_detects_can_start_session_change_as_upsert() {
        // 项目从本地改成 SSH 远程(或反之)必须推到移动端,否则弹层置灰状态会陈旧
        let prev = vec![project("p1", "demo", &[])];
        let mut next = prev.clone();
        next[0].can_start_session = false;
        let (upserts, removed) = diff_sessions(&prev, &next).unwrap();
        assert_eq!(upserts.len(), 1);
        assert!(!upserts[0].can_start_session);
        assert!(removed.is_empty());
    }

    #[test]
    fn reject_status_maps_each_reason_to_its_own_state() {
        // 三种拒绝的修法不同(升级 / 改密钥 / 去中转配密钥),状态串不能合并
        assert_eq!(reject_status(DesktopRejectReason::InvalidKey), "authFailed");
        assert_eq!(
            reject_status(DesktopRejectReason::KeyNotConfigured),
            "keyNotConfigured"
        );
        assert_eq!(
            reject_status(DesktopRejectReason::VersionMismatch),
            "versionMismatch"
        );
    }

    #[test]
    fn start_session_payload_serializes_camel_case() {
        // 载荷形状钉住:移动端面板与旧前端按 camelCase 读同一份字段名
        let json = serde_json::to_string(&StartSessionPayload {
            request_id: "req-1".into(),
            project_id: "p1".into(),
            launcher_id: "l1".into(),
            launcher_name: "Claude".into(),
            shell_name: Some("wsl-bash".into()),
            command: "claude".into(),
        })
        .unwrap();
        assert!(
            json.contains(r#""requestId":"req-1""#)
                && json.contains(r#""launcherName":"Claude""#)
                && json.contains(r#""shellName":"wsl-bash""#),
            "{json}"
        );
    }

    #[test]
    fn launcher_id_resolves_to_command_and_shell() {
        let launchers = vec![
            AiLauncher {
                id: "l1".into(),
                name: "Claude (WSL)".into(),
                shell: Some("wsl-bash".into()),
                command: "claude".into(),
            },
            AiLauncher {
                id: "l2".into(),
                name: "Codex".into(),
                shell: None,
                command: "codex".into(),
            },
            AiLauncher {
                id: "l3".into(),
                name: "Blank shell".into(),
                shell: Some("  ".into()),
                command: "claude".into(),
            },
        ];

        let l1 = resolve_launcher(&launchers, "l1").unwrap();
        assert_eq!(l1.command, "claude");
        assert_eq!(l1.shell.as_deref(), Some("wsl-bash"));

        // 未绑定 shell → None(上层据此用默认 shell)
        let l2 = resolve_launcher(&launchers, "l2").unwrap();
        assert_eq!(l2.command, "codex");
        assert!(l2.shell.is_none());

        // 空白 shell 名等同未绑定,不能拿去 availableShells 里找一个空名条目
        assert!(resolve_launcher(&launchers, "l3").unwrap().shell.is_none());

        // 已被删除的启动器 → launcherNotFound
        assert_eq!(
            resolve_launcher(&launchers, "gone").unwrap_err(),
            StartSessionFailReason::LauncherNotFound
        );
    }

    #[test]
    fn launcher_command_check_matches_pty_ai_detection() {
        // 面板保存时的提示口径 = PTY 输入检测口径(两处漂移就会出现
        // "面板说没问题、手机上却永远等不到 AI 会话")
        assert!(check_launcher_command("claude"));
        assert!(check_launcher_command("  codex  "));
        assert!(check_launcher_command("grok"));
        assert!(check_launcher_command("claude --dangerously-skip-permissions"));
        assert!(check_launcher_command("grok --resume"));
        // 非 AI CLI / 非交互标志:提示会被识别不了
        assert!(!check_launcher_command("npm test"));
        assert!(!check_launcher_command("claude -p 'hi'"));
        assert!(!check_launcher_command("codex --version"));
        assert!(!check_launcher_command("grok -p 'hi'"));
        assert!(!check_launcher_command(""));
    }

    #[test]
    fn diff_detects_added_project() {
        let prev = vec![];
        let next = vec![project("p1", "demo", &[("a", "ai-working")])];
        let (upserts, removed) = diff_sessions(&prev, &next).unwrap();
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].project_id, "p1");
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_detects_pane_status_change_as_project_upsert() {
        let prev = vec![project("p1", "demo", &[("a", "ai-working")])];
        let next = vec![project("p1", "demo", &[("a", "ai-idle")])];
        let (upserts, removed) = diff_sessions(&prev, &next).unwrap();
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].panes[0].status, "ai-idle");
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_detects_removed_project() {
        let prev = vec![
            project("p1", "demo", &[("a", "ai-idle")]),
            project("p2", "other", &[("b", "ai-working")]),
        ];
        let next = vec![project("p2", "other", &[("b", "ai-working")])];
        let (upserts, removed) = diff_sessions(&prev, &next).unwrap();
        assert!(upserts.is_empty());
        assert_eq!(removed, vec!["p1".to_string()]);
    }

    #[test]
    fn diff_no_change_returns_none() {
        let state = vec![project("p1", "demo", &[("a", "ai-working")])];
        assert!(diff_sessions(&state, &state.clone()).is_none());
    }

    #[test]
    fn diff_mixed_upsert_and_removal() {
        let prev = vec![
            project("p1", "demo", &[("a", "ai-idle")]),
            project("p2", "other", &[("b", "ai-working")]),
        ];
        let next = vec![
            project("p2", "other", &[("b", "error")]),
            project("p3", "new", &[("c", "ai-working")]),
        ];
        let (upserts, removed) = diff_sessions(&prev, &next).unwrap();
        let upsert_ids: Vec<&str> = upserts.iter().map(|p| p.project_id.as_str()).collect();
        assert_eq!(upsert_ids, vec!["p2", "p3"]);
        assert_eq!(removed, vec!["p1".to_string()]);
    }

    #[test]
    fn relabel_marks_matching_user_message_as_mobile_once() {
        let manager = test_manager();
        manager.record_mobile_cmd("pane-1", "  npm test ");

        let mut msgs = vec![
            MirrorMessage {
                seq: 0,
                source: "desktop".into(),
                content: "unrelated input".into(),
                timestamp: String::new(),
                ..Default::default()
            },
            MirrorMessage {
                seq: 1,
                source: "desktop".into(),
                content: "npm test".into(),
                timestamp: String::new(),
                ..Default::default()
            },
            MirrorMessage {
                seq: 2,
                source: "assistant".into(),
                content: "npm test".into(),
                timestamp: String::new(),
                ..Default::default()
            },
        ];
        manager.relabel_mobile_sources("pane-1", &mut msgs, RelabelScope::Appended);
        assert_eq!(msgs[0].source, "desktop");
        assert_eq!(msgs[1].source, "mobile");
        // assistant 消息不受影响
        assert_eq!(msgs[2].source, "assistant");

        // 记录一次性消费:同文本再次回流按桌面输入处理
        let mut again = vec![MirrorMessage {
            seq: 3,
            source: "desktop".into(),
            content: "npm test".into(),
            timestamp: String::new(),
            ..Default::default()
        }];
        manager.relabel_mobile_sources("pane-1", &mut again, RelabelScope::Appended);
        assert_eq!(again[0].source, "desktop");

        // 其他 pane 的记录互不影响
        manager.record_mobile_cmd("pane-2", "ls");
        let mut other = vec![MirrorMessage {
            seq: 0,
            source: "desktop".into(),
            content: "ls".into(),
            timestamp: String::new(),
            ..Default::default()
        }];
        manager.relabel_mobile_sources("pane-1", &mut other, RelabelScope::Appended);
        assert_eq!(other[0].source, "desktop");
    }

    /// 点选作答的回流标记按结构化 labels 对账改标——content 是多题合并文本,
    /// 与逐条登记的单个 label 对不上,不能走普通指令那条逐字匹配。
    #[test]
    fn relabel_marks_answer_markers_by_labels() {
        let manager = test_manager();
        manager.record_mobile_cmd("pane-1", "方案B");

        let marker = |seq: u64, labels: &[&str]| MirrorMessage {
            seq,
            source: "desktop".into(),
            content: labels.join(", "),
            timestamp: String::new(),
            kind: Some("questionAnswered".into()),
            ref_seq: Some(0),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };

        // 有未登记的 label(桌面替答了一题)→ 不改标,登记条目也不消费
        let mut mixed = vec![marker(2, &["方案B", "桌面答的"])];
        manager.relabel_mobile_sources("pane-1", &mut mixed, RelabelScope::Appended);
        assert_eq!(mixed[0].source, "desktop");

        // 全部选中项都出自本端登记 → 改标并消费,再来一遍不再命中
        let mut mine = vec![marker(3, &["方案B"])];
        manager.relabel_mobile_sources("pane-1", &mut mine, RelabelScope::Appended);
        assert_eq!(mine[0].source, "mobile");
        let mut again = vec![marker(4, &["方案B"])];
        manager.relabel_mobile_sources("pane-1", &mut again, RelabelScope::Appended);
        assert_eq!(again[0].source, "desktop");

        // 打断标记(labels 为空)不参与对账
        let mut interrupted = vec![marker(5, &[])];
        manager.record_mobile_cmd("pane-1", "whatever");
        manager.relabel_mobile_sources("pane-1", &mut interrupted, RelabelScope::Appended);
        assert_eq!(interrupted[0].source, "desktop");
    }

    /// RFC 3339 时间戳 → SystemTime(测试里按会话记录的时间戳算登记时刻)。
    fn at(ts: &str) -> SystemTime {
        parse_mirror_timestamp(ts).unwrap()
    }

    fn desktop_msg(seq: u64, content: &str, ts: &str) -> MirrorMessage {
        MirrorMessage {
            seq,
            source: "desktop".into(),
            content: content.into(),
            timestamp: ts.into(),
            ..Default::default()
        }
    }

    fn sources(msgs: &[MirrorMessage]) -> Vec<&str> {
        msgs.iter().map(|m| m.source.as_str()).collect()
    }

    /// 整份历史改标:早于登记时刻的同文旧桌面输入不被认走,时间戳认不得的不认;
    /// 认领后同一份历史再解析(重订阅)仍认回那一条,但增量新行不再命中。
    #[test]
    fn relabel_history_respects_record_time_and_reclaims_on_reparse() {
        let manager = test_manager();
        manager.record_mobile_cmd_at("pane-1", "继续", at("2026-01-01T00:10:00Z"));
        let history = || {
            vec![
                // 移动端发送前一天桌面敲过同样的话
                desktop_msg(0, "继续", "2025-12-31T09:00:00Z"),
                // 发送前几分钟(超出容差)
                desktop_msg(1, "继续", "2026-01-01T00:05:00Z"),
                // 真回显:落盘略早于登记也在容差内(登记在 PTY 投递之后)
                desktop_msg(2, "继续", "2026-01-01T00:09:58.120Z"),
                // 时间戳认不得:历史里一律不认
                desktop_msg(3, "继续", ""),
            ]
        };
        let mut first = history();
        manager.relabel_mobile_sources("pane-1", &mut first, RelabelScope::History);
        assert_eq!(sources(&first), ["desktop", "desktop", "mobile", "desktop"]);

        // 重订阅 = 同一份记录整份重解析:认回同一条,不多不少
        let mut again = history();
        manager.relabel_mobile_sources("pane-1", &mut again, RelabelScope::History);
        assert_eq!(sources(&again), ["desktop", "desktop", "mobile", "desktop"]);

        // 已认领的登记不再认增量新行(哪怕时间戳相同)
        let mut appended = vec![desktop_msg(4, "继续", "2026-01-01T00:09:58.120Z")];
        manager.relabel_mobile_sources("pane-1", &mut appended, RelabelScope::Appended);
        assert_eq!(appended[0].source, "desktop");
    }

    /// 同文的两条移动端指令各认各的回显;重解析时已认领的优先认回原消息,
    /// 不会被历史里更早的那条抢走而把后一条挤成桌面输入。
    #[test]
    fn relabel_history_pairs_repeated_commands_one_to_one() {
        let manager = test_manager();
        manager.record_mobile_cmd_at("pane-1", "继续", at("2026-01-01T00:00:00Z"));
        manager.record_mobile_cmd_at("pane-1", "继续", at("2026-01-01T01:00:00Z"));
        let history = || {
            vec![
                desktop_msg(0, "继续", "2026-01-01T00:00:01Z"),
                desktop_msg(1, "继续", "2026-01-01T01:00:01Z"),
                desktop_msg(2, "继续", "2026-01-01T02:00:00Z"),
            ]
        };
        let mut first = history();
        manager.relabel_mobile_sources("pane-1", &mut first, RelabelScope::History);
        assert_eq!(sources(&first), ["mobile", "mobile", "desktop"]);
        let mut again = history();
        manager.relabel_mobile_sources("pane-1", &mut again, RelabelScope::History);
        assert_eq!(sources(&again), ["mobile", "mobile", "desktop"]);
    }

    /// 作答标记在整份历史里同样按 labels 对账,重解析认回。
    #[test]
    fn relabel_history_marks_answer_markers() {
        let manager = test_manager();
        manager.record_mobile_cmd_at("pane-1", "方案B", at("2026-01-01T00:00:00Z"));
        let marker = || MirrorMessage {
            seq: 2,
            source: "desktop".into(),
            content: "方案B".into(),
            timestamp: "2026-01-01T00:00:03Z".into(),
            kind: Some("questionAnswered".into()),
            ref_seq: Some(1),
            labels: vec!["方案B".into()],
            ..Default::default()
        };
        for _ in 0..2 {
            let mut msgs = vec![marker()];
            manager.relabel_mobile_sources("pane-1", &mut msgs, RelabelScope::History);
            assert_eq!(msgs[0].source, "mobile");
        }
    }

    /// 登记上限按条数滚动,已认领的也占名额(最老的先出)。
    #[test]
    fn mobile_cmd_registry_is_bounded() {
        let manager = test_manager();
        for i in 0..MOBILE_CMD_KEEP + 5 {
            manager.record_mobile_cmd("pane-1", &format!("cmd {i}"));
        }
        let map = manager.recent_mobile_cmds.lock();
        let list = &map["pane-1"];
        assert_eq!(list.len(), MOBILE_CMD_KEEP);
        assert_eq!(list[0].text, "cmd 5");
    }

    // ── 移动端指令写穿:正文与提交回车分两次写 ──

    /// 记下每一次 PTY 写入的宿主。`bracketed` 模拟终端里的 agent 开没开
    /// bracketed paste,`reject` 让写入一律失败(PTY 已没了)。
    #[derive(Default)]
    struct RecordingHost {
        writes: Mutex<Vec<(u32, String)>>,
        bracketed: bool,
        reject: bool,
    }

    impl RelayHost for RecordingHost {
        fn launchers(&self) -> Vec<AiLauncher> {
            Vec::new()
        }
        fn project(&self, _project_id: &str) -> Option<RelayProject> {
            None
        }
        fn write_pty(&self, pty_id: u32, data: String) -> Result<(), String> {
            if self.reject {
                return Err("pty gone".into());
            }
            self.writes.lock().push((pty_id, data));
            Ok(())
        }
        fn bracketed_paste(&self, _pty_id: u32) -> bool {
            self.bracketed
        }
        fn hook_session(&self, _pty_id: u32) -> Option<HookSessionId> {
            None
        }
        fn ai_session_agent(&self, _pty_id: u32) -> Option<String> {
            None
        }
        fn ai_session_started_at(&self, _pty_id: u32) -> Option<SystemTime> {
            None
        }
    }

    /// 接上真运行时的 manager:pane-1 → PTY 7,出站通道接到测试手里收回执。
    struct CommandFixture {
        rt: tokio::runtime::Runtime,
        manager: Arc<MobileRelayManager>,
        host: Arc<RecordingHost>,
        outbound: mpsc::UnboundedReceiver<DesktopToRelay>,
    }

    type Receipt = (String, bool, Option<CommandFailReason>);

    impl CommandFixture {
        fn new(host: RecordingHost, submit_delay: Duration) -> Self {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_time()
                .build()
                .unwrap();
            let host = Arc::new(host);
            let mut manager = MobileRelayManager::with_runtime(
                host.clone(),
                Arc::new(NoopRelayHost),
                rt.handle().clone(),
            );
            manager.submit_delay = submit_delay;
            manager.pane_ptys.lock().insert("pane-1".into(), 7);
            let (tx, outbound) = mpsc::unbounded_channel();
            *manager.outbound.lock() = Some(tx);
            Self {
                rt,
                manager: Arc::new(manager),
                host,
                outbound,
            }
        }

        fn send(&self, pane_id: &str, command_id: &str, text: &str) {
            handle_mobile_command(
                &self.manager,
                pane_id.into(),
                command_id.into(),
                text.into(),
            );
        }

        /// 按到达顺序等齐 n 条指令回执(超时即失败)。
        fn receipts(&mut self, n: usize) -> Vec<Receipt> {
            let outbound = &mut self.outbound;
            self.rt.block_on(async move {
                let mut out = Vec::new();
                while out.len() < n {
                    let msg = tokio::time::timeout(Duration::from_secs(5), outbound.recv())
                        .await
                        .expect("等回执超时")
                        .expect("出站通道已关");
                    if let DesktopToRelay::CommandReceipt {
                        command_id,
                        ok,
                        reason,
                        ..
                    } = msg
                    {
                        out.push((command_id, ok, reason));
                    }
                }
                out
            })
        }

        fn writes(&self) -> Vec<String> {
            self.host
                .writes
                .lock()
                .iter()
                .map(|(_, data)| data.clone())
                .collect()
        }
    }

    fn ok_receipt(command_id: &str) -> Receipt {
        (command_id.into(), true, None)
    }

    /// 正文与回车分两次写、先正文后回车、同一个 PTY;两次都写完才回执成功,
    /// 并登记原文供回流改标。
    #[test]
    fn mobile_command_writes_text_then_enter_separately() {
        let mut fx = CommandFixture::new(RecordingHost::default(), Duration::from_millis(1));
        let text = "解释一下 crates/mt-relay/src/relay.rs 里镜像轮询为什么要丢进阻塞线程池";
        fx.send("pane-1", "c1", text);
        assert_eq!(fx.receipts(1), [ok_receipt("c1")]);
        assert_eq!(fx.writes(), [text, "\r"]);
        assert!(fx.host.writes.lock().iter().all(|(pty, _)| *pty == 7));
        let registry = fx.manager.recent_mobile_cmds.lock();
        assert_eq!(registry["pane-1"][0].text, text);
    }

    /// 终端开着 bracketed paste:正文按桌面粘贴同一编码包裹(多行的换行归一成
    /// `\r`,只是粘贴内容),提交回车照旧单独写在结束标记之后。
    #[test]
    fn mobile_command_wraps_body_when_bracketed_paste_is_on() {
        let host = RecordingHost {
            bracketed: true,
            ..Default::default()
        };
        let mut fx = CommandFixture::new(host, Duration::from_millis(1));
        fx.send("pane-1", "c1", "第一行\n第二行");
        assert_eq!(fx.receipts(1), [ok_receipt("c1")]);
        assert_eq!(fx.writes(), ["\x1b[200~第一行\r第二行\x1b[201~", "\r"]);
    }

    /// 紧挨着到的几条指令不交错:每条等上一条连同回车写完才动手,回执也按到达序。
    #[test]
    fn mobile_commands_are_written_one_by_one_in_arrival_order() {
        let mut fx = CommandFixture::new(RecordingHost::default(), Duration::from_millis(30));
        fx.send("pane-1", "c1", "A");
        fx.send("pane-1", "c2", "B");
        fx.send("pane-1", "c3", "C");
        assert_eq!(
            fx.receipts(3),
            [ok_receipt("c1"), ok_receipt("c2"), ok_receipt("c3")]
        );
        assert_eq!(fx.writes(), ["A", "\r", "B", "\r", "C", "\r"]);
    }

    /// 写不进去:回执报 WriteFailed,不补写回车、不登记;pane 不在映射里报
    /// PaneNotFound。失败的那条不卡住后面的队伍。
    #[test]
    fn mobile_command_failures_report_and_skip_enter() {
        let host = RecordingHost {
            reject: true,
            ..Default::default()
        };
        let mut fx = CommandFixture::new(host, Duration::from_millis(1));
        fx.send("pane-1", "c1", "hi");
        fx.send("ghost", "c2", "hi");
        assert_eq!(
            fx.receipts(2),
            [
                ("c1".into(), false, Some(CommandFailReason::WriteFailed)),
                ("c2".into(), false, Some(CommandFailReason::PaneNotFound)),
            ]
        );
        assert!(fx.writes().is_empty());
        assert!(fx.manager.recent_mobile_cmds.lock().is_empty());
    }

    /// 正文编码:裸写时换行一律归一成 LF(只让最后单独的 `\r` 提交);包裹时与
    /// 桌面粘贴同口径 —— 换行归一成 CR、剔掉内嵌的结束标记。
    #[test]
    fn mobile_command_body_encoding() {
        assert_eq!(mobile_command_body("a\r\nb\rc\nd", false), "a\nb\nc\nd");
        assert_eq!(
            mobile_command_body("a\r\nb\nc", true),
            "\x1b[200~a\rb\rc\x1b[201~"
        );
        assert_eq!(
            mobile_command_body("x\x1b[201~y", true),
            "\x1b[200~xy\x1b[201~"
        );
    }

    /// 新版 Claude 的立即执行本地命令(`/usage`)命令行记在 system 行:镜像里显示成
    /// 键入形态 `/usage`,移动端发的同文指令照样改标「移动端」;输出行不进镜像。
    #[test]
    fn local_command_system_line_is_mirrored_and_relabeled() {
        let fx = MirrorFixture::new("local-command");
        let a = fx.dir.join("a.jsonl");
        let command = r#"{"type":"system","subtype":"local_command","content":"<command-name>/usage</command-name>\n            <command-message>usage</command-message>\n            <command-args></command-args>","level":"info","timestamp":"2026-01-01T00:00:00.000Z","uuid":"u1","isMeta":false}"#;
        let output = r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout>Total cost: $0.00</local-command-stdout>","level":"info","timestamp":"2026-01-01T00:00:00.000Z","uuid":"u2","isMeta":false,"commandRun":{"command":"usage","args":""}}"#;
        std::fs::write(&a, format!("{command}\n{output}\n")).unwrap();
        fx.manager
            .record_mobile_cmd_at("pane-1", "/usage", at("2025-12-31T23:59:59Z"));
        *fx.host.hook.lock() = hook("sid-a");
        let locate = |_: &str, _: &LocateKey| Some((a.clone(), MirrorAgent::Claude));
        fx.round(&mut MirrorLocator::default(), Instant::now(), locate);
        assert_eq!(fx.texts(), ["/usage"]);
        assert_eq!(sources(&fx.job.messages.lock()), ["mobile"]);
    }

    #[test]
    fn status_payload_serializes_camel_case() {
        let payload = MobileRelayStatusPayload {
            status: "versionMismatch".into(),
            expected_version: Some(1),
            actual_version: Some(2),
            paired: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(
            json.contains(r#""expectedVersion":1"#) && json.contains(r#""actualVersion":2"#),
            "{json}"
        );
        // 简单状态不携带版本字段
        let simple = serde_json::to_string(&MobileRelayStatusPayload::simple("connected")).unwrap();
        assert_eq!(simple, r#"{"status":"connected"}"#);
    }

    // ── 镜像轮询:定位节流 ──

    use crate::host::{HookSessionId, RelayProject};
    use std::cell::{Cell, RefCell};

    /// 可控的宿主:hook 身份 / 输入检测的 agent / AI 启动时刻都由测试随手改。
    #[derive(Default)]
    struct FakeHost {
        hook: Mutex<Option<HookSessionId>>,
        agent: Mutex<Option<String>>,
        started: Mutex<Option<SystemTime>>,
    }

    impl RelayHost for FakeHost {
        fn launchers(&self) -> Vec<AiLauncher> {
            Vec::new()
        }
        fn project(&self, _project_id: &str) -> Option<RelayProject> {
            None
        }
        fn write_pty(&self, _pty_id: u32, _data: String) -> Result<(), String> {
            Ok(())
        }
        fn hook_session(&self, _pty_id: u32) -> Option<HookSessionId> {
            self.hook.lock().clone()
        }
        fn ai_session_agent(&self, _pty_id: u32) -> Option<String> {
            self.agent.lock().clone()
        }
        fn ai_session_started_at(&self, _pty_id: u32) -> Option<SystemTime> {
            *self.started.lock()
        }
    }

    struct MirrorFixture {
        manager: MobileRelayManager,
        host: Arc<FakeHost>,
        job: MirrorJob,
        dir: PathBuf,
    }

    impl MirrorFixture {
        fn new(tag: &str) -> Self {
            let host = Arc::new(FakeHost::default());
            let manager = MobileRelayManager::new(host.clone(), Arc::new(NoopRelayHost));
            manager.pane_ptys.lock().insert("pane-1".into(), 7);
            let dir = std::env::temp_dir().join(format!(
                "mt-relay-locate-{tag}-{}",
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let job = MirrorJob {
                pane_id: "pane-1".into(),
                project_path: r"D:\proj".into(),
                messages: Arc::new(Mutex::new(Vec::new())),
                runtime: Arc::new(Mutex::new(None)),
            };
            Self {
                manager,
                host,
                job,
                dir,
            }
        }

        fn round(
            &self,
            locator: &mut MirrorLocator,
            now: Instant,
            locate: impl Fn(&str, &LocateKey) -> Option<(PathBuf, MirrorAgent)>,
        ) {
            self.manager.mirror_round(&self.job, locator, now, locate);
        }

        fn texts(&self) -> Vec<String> {
            self.job
                .messages
                .lock()
                .iter()
                .map(|m| m.content.clone())
                .collect()
        }

        fn bound(&self) -> Option<PathBuf> {
            self.job.runtime.lock().as_ref().map(|rt| rt.path.clone())
        }
    }

    impl Drop for MirrorFixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn user_line(text: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}},"timestamp":"2026-01-01T00:00:00Z"}}"#
        ) + "\n"
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(user_line(text).as_bytes()).unwrap();
    }

    fn hook(session_id: &str) -> Option<HookSessionId> {
        Some(HookSessionId {
            agent: Some("claude-code".into()),
            session_id: session_id.into(),
        })
    }

    /// hook 身份没变且已绑上:此后只做增量泵,不再扫目录(时间过多久都一样);
    /// 身份一变,**当轮**就重新定位并换绑 —— 与原来每轮都定位的换绑时机一致。
    #[test]
    fn hook_bound_pumps_only_until_identity_changes() {
        let fx = MirrorFixture::new("hook");
        let a = fx.dir.join("a.jsonl");
        let b = fx.dir.join("b.jsonl");
        std::fs::write(&a, user_line("a1")).unwrap();
        std::fs::write(&b, user_line("b1") + &user_line("b2")).unwrap();
        let calls = Cell::new(0);
        let locate = |_: &str, key: &LocateKey| {
            calls.set(calls.get() + 1);
            match key {
                LocateKey::Hook { session_id, .. } if session_id == "sid-a" => {
                    Some((a.clone(), MirrorAgent::Claude))
                }
                LocateKey::Hook { session_id, .. } if session_id == "sid-b" => {
                    Some((b.clone(), MirrorAgent::Claude))
                }
                _ => None,
            }
        };
        let t0 = Instant::now();
        let mut loc = MirrorLocator::default();
        *fx.host.hook.lock() = hook("sid-a");

        fx.round(&mut loc, t0, locate);
        assert_eq!(calls.get(), 1);
        assert_eq!(fx.texts(), ["a1"]);

        append(&a, "a2");
        fx.round(&mut loc, t0 + Duration::from_secs(1), locate);
        fx.round(&mut loc, t0 + Duration::from_secs(600), locate);
        assert_eq!(calls.get(), 1, "身份没变:只泵不定位");
        assert_eq!(fx.texts(), ["a1", "a2"], "增量照常进来");

        *fx.host.hook.lock() = hook("sid-b");
        fx.round(&mut loc, t0 + Duration::from_secs(601), locate);
        assert_eq!(calls.get(), 2, "身份一变当轮重新定位");
        assert_eq!(fx.bound(), Some(b.clone()));
        assert_eq!(fx.texts(), ["b1", "b2"], "换绑后是新会话的全量");

        // 文件被截断重写:泵清掉运行态,下一轮立刻重绑(与原来同节奏)
        std::fs::write(&b, user_line("x")).unwrap();
        fx.round(&mut loc, t0 + Duration::from_secs(602), locate);
        assert_eq!(fx.bound(), None, "截断由泵发现并清运行态");
        fx.round(&mut loc, t0 + Duration::from_secs(603), locate);
        assert_eq!(calls.get(), 3);
        assert_eq!(fx.texts(), ["x"]);
    }

    /// hook 已上报身份、会话文件还没落盘:每轮都再找(移动端在等第一条消息),
    /// 文件一出现当轮绑上。
    #[test]
    fn hook_identity_without_file_keeps_locating_every_round() {
        let fx = MirrorFixture::new("pending");
        let a = fx.dir.join("a.jsonl");
        let calls = Cell::new(0);
        let locate = |_: &str, _: &LocateKey| {
            calls.set(calls.get() + 1);
            a.exists().then(|| (a.clone(), MirrorAgent::Claude))
        };
        let t0 = Instant::now();
        let mut loc = MirrorLocator::default();
        *fx.host.hook.lock() = hook("sid-a");

        fx.round(&mut loc, t0, locate);
        fx.round(&mut loc, t0 + Duration::from_secs(1), locate);
        assert_eq!(calls.get(), 2);
        assert_eq!(fx.bound(), None);

        std::fs::write(&a, user_line("hi")).unwrap();
        fx.round(&mut loc, t0 + Duration::from_secs(2), locate);
        assert_eq!(calls.get(), 3);
        assert_eq!(fx.texts(), ["hi"]);
        fx.round(&mut loc, t0 + Duration::from_secs(3), locate);
        assert_eq!(calls.get(), 3, "绑上之后只泵");
    }

    /// 移动端发起的新会话:第一条移动端指令只出现在首次绑定的全量里,同样标成
    /// 「移动端」;之后增量进来的同文桌面输入照旧是「桌面」;移动端退出镜像再进
    /// (重订阅 = 新运行态从头整份解析)两条都认回原样。
    #[test]
    fn rebind_relabels_mobile_commands_in_full_history() {
        let fx = MirrorFixture::new("rebind-relabel");
        let a = fx.dir.join("a.jsonl");
        let locate = |_: &str, _: &LocateKey| a.exists().then(|| (a.clone(), MirrorAgent::Claude));
        // user_line 的时间戳是 2026-01-01T00:00:00Z:指令在它前一秒写入 PTY
        fx.manager
            .record_mobile_cmd_at("pane-1", "fix the bug", at("2025-12-31T23:59:59Z"));
        *fx.host.hook.lock() = hook("sid-a");
        let t0 = Instant::now();
        let mut loc = MirrorLocator::default();
        fx.round(&mut loc, t0, locate);
        assert_eq!(fx.bound(), None, "会话文件还没落盘");

        std::fs::write(&a, user_line("fix the bug")).unwrap();
        fx.round(&mut loc, t0 + Duration::from_secs(1), locate);
        assert_eq!(sources(&fx.job.messages.lock()), ["mobile"]);

        // 桌面又敲了一遍同样的话(测试行时间戳相同,真实记录精确到毫秒)
        append(&a, "fix the bug");
        fx.round(&mut loc, t0 + Duration::from_secs(2), locate);
        assert_eq!(sources(&fx.job.messages.lock()), ["mobile", "desktop"]);

        let job = MirrorJob {
            pane_id: "pane-1".into(),
            project_path: r"D:\proj".into(),
            messages: Arc::new(Mutex::new(Vec::new())),
            runtime: Arc::new(Mutex::new(None)),
        };
        fx.manager.mirror_round(
            &job,
            &mut MirrorLocator::default(),
            t0 + Duration::from_secs(3),
            locate,
        );
        assert_eq!(sources(&job.messages.lock()), ["mobile", "desktop"]);
    }

    /// 无 hook 的启发式:已绑上时 10s 才重新定位一次,期间照常泵;AI 换了一轮
    /// (启动时刻变了)不等节流,当轮重新定位。
    #[test]
    fn heuristic_relocates_on_interval_or_when_inputs_change() {
        let fx = MirrorFixture::new("heuristic");
        let a = fx.dir.join("a.jsonl");
        let b = fx.dir.join("b.jsonl");
        std::fs::write(&a, user_line("a1")).unwrap();
        std::fs::write(&b, user_line("b1")).unwrap();
        let newest = RefCell::new(a.clone());
        let calls = Cell::new(0);
        let locate = |_: &str, key: &LocateKey| {
            calls.set(calls.get() + 1);
            assert!(matches!(key, LocateKey::Heuristic { .. }));
            Some((newest.borrow().clone(), MirrorAgent::Claude))
        };
        *fx.host.agent.lock() = Some("claude".into());
        *fx.host.started.lock() = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(100));
        let t0 = Instant::now();
        let mut loc = MirrorLocator::default();

        fx.round(&mut loc, t0, locate);
        assert_eq!((calls.get(), fx.bound()), (1, Some(a.clone())));

        // 出现了更新的会话文件:节流期内先不换,旧绑定照泵
        *newest.borrow_mut() = b.clone();
        append(&a, "a2");
        fx.round(&mut loc, t0 + Duration::from_secs(5), locate);
        assert_eq!(calls.get(), 1);
        assert_eq!(fx.texts(), ["a1", "a2"]);

        fx.round(&mut loc, t0 + HEURISTIC_RELOCATE_INTERVAL, locate);
        assert_eq!(calls.get(), 2, "到点重新定位");
        assert_eq!(fx.bound(), Some(b.clone()));

        // 同 pane 起了新一轮 AI:输入变了,不等节流
        *fx.host.started.lock() = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(200));
        fx.round(
            &mut loc,
            t0 + HEURISTIC_RELOCATE_INTERVAL + Duration::from_secs(1),
            locate,
        );
        assert_eq!(calls.get(), 3);
    }

    /// 启发式还没绑上(本轮会话的文件没落盘):按较短的间隔再找。
    #[test]
    fn heuristic_unbound_retries_on_pending_interval() {
        let fx = MirrorFixture::new("heuristic-pending");
        let calls = Cell::new(0);
        let locate = |_: &str, _: &LocateKey| {
            calls.set(calls.get() + 1);
            None
        };
        let t0 = Instant::now();
        let mut loc = MirrorLocator::default();
        fx.round(&mut loc, t0, locate);
        fx.round(&mut loc, t0 + Duration::from_secs(1), locate);
        assert_eq!(calls.get(), 1);
        fx.round(&mut loc, t0 + HEURISTIC_PENDING_INTERVAL, locate);
        assert_eq!(calls.get(), 2);
        assert!(loc.sent_initial, "定位落空时给过空快照");
    }

    /// 没有会话记录的 agent(pi / opencode):定位输入是 NoLog,生产定位函数
    /// 对它不做任何 IO;输入不变就连这一问都省掉,运行态始终是空的。
    #[test]
    fn nolog_agent_never_binds() {
        let fx = MirrorFixture::new("nolog");
        *fx.host.agent.lock() = Some("pi".into());
        assert!(matches!(
            fx.manager.mirror_locate_key("pane-1"),
            LocateKey::NoLog { .. }
        ));
        assert_eq!(
            locate_session_file(r"D:\proj", &fx.manager.mirror_locate_key("pane-1")),
            None
        );
        let calls = Cell::new(0);
        let locate = |_: &str, key: &LocateKey| {
            calls.set(calls.get() + 1);
            locate_session_file(r"D:\proj", key)
        };
        let t0 = Instant::now();
        let mut loc = MirrorLocator::default();
        for s in 0..20 {
            fx.round(&mut loc, t0 + Duration::from_secs(s), locate);
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(fx.bound(), None);
    }
}
