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
use mt_ai::AgentKind;
use mt_core::path_key::windows_eq_key;
use mt_relay_protocol::{
    CommandFailReason, DesktopRejectReason, DesktopToRelay, HistoryFailReason, MirrorMessage,
    MobileHistorySession, MobileLauncher, MobilePane, MobileProject, PROTOCOL_VERSION,
    RelayToDesktop, StartSessionFailReason,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::host::{AiLauncher, RelayEvents, RelayHost};
use crate::mirror::{
    self, CodexPeers, HeuristicScope, MIRROR_PAGE_SIZE, MirrorAgent, MirrorParser, history_slice,
};
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

/// 桌面端 ↔ 中转长连的保活与探活参数(生产恒为 [`KEEPALIVE`],单测改短)。
#[derive(Debug, Clone, Copy)]
struct Keepalive {
    /// 已连接时多久发一次 WebSocket ping
    interval: Duration,
    /// 这么久没从中转收到任何帧(pong 也算)就判连接已死、断开重连
    timeout: Duration,
}

/// 保活:中间网络会悄悄回收空闲长连 —— CF 橙云约 100s,运营商 NAT 几分钟。连接一断,
/// 桌面端就撤掉全部镜像订阅,手机那边正在看的会话收不到内容;定时 ping 让连接不空闲。
///
/// 探活:TCP 半开(笔记本睡眠唤醒、切网络、对端被掐但没收到 FIN)时读不到 EOF,
/// 不主动判就一直以为在线,手机发来的订阅与指令全部落空。中转读到 ping 会自动回 pong,
/// 超过 `timeout`(约三个 ping 周期)一个帧都没收到就当连接已死。
const KEEPALIVE: Keepalive = Keepalive {
    interval: Duration::from_secs(25),
    timeout: Duration::from_secs(70),
};

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

/// 一个镜像绑定的运行态:解析器 + 绑定文件与读取游标(偏移 + 续读锚)。
/// 三者必须同锁——增量泵的「读文件→喂解析→挪偏移」要原子,拆开就会把同一段
/// 字节喂两次(消息重复、seq 漂移)。
struct MirrorRuntime {
    parser: MirrorParser,
    path: PathBuf,
    cursor: mirror::TailCursor,
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
    /// 输入检测认出的 agent 没有可解析的记录(opencode):恒为空镜像,不做 IO
    NoLog { pty_id: Option<u32>, agent: String },
    /// 无会话身份:项目最新文件 + AI 启动时刻下限的启发式
    Heuristic {
        pty_id: Option<u32>,
        agent: Option<String>,
        started: Option<SystemTime>,
        /// 同项目里其他 Codex pane 划的范围:别的 pane 起停、拿到 hook 身份都会变,
        /// 变了当轮重新定位
        codex_peers: CodexPeers,
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
        LocateKey::Heuristic {
            agent,
            started,
            codex_peers,
            ..
        } => mirror::resolve_session_file(
            project_path,
            &HeuristicScope {
                agent: agent.as_deref(),
                started: *started,
                codex_peers,
            },
        ),
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

/// 最近一次历史预览的全量消息缓存(单槽):上拉翻页时直接从内存按 `before_seq` 切片,
/// 不必每次翻页都重新扫盘解析整个 JSONL;换看别的会话或断线即覆盖/清空。
struct HistoryPreviewCache {
    project_id: String,
    agent: String,
    session_id: String,
    messages: Vec<MirrorMessage>,
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
    /// 历史会话只读预览的单槽分页缓存(断线清空)
    history_preview_cache: Mutex<Option<HistoryPreviewCache>>,
    /// 每 pane 最近成功写入的移动端指令原文:会话记录回流时把匹配的
    /// user 消息来源改标为 "mobile"(见 relabel_mobile_sources)
    recent_mobile_cmds: Mutex<HashMap<String, Vec<MobileCmd>>>,
    /// 上一条移动端指令写完(含提交回车)的信号:指令按到达顺序串行写入,
    /// 见 [`handle_mobile_command`]
    command_tail: Mutex<Option<oneshot::Receiver<()>>>,
    /// 指令正文与提交回车之间的间隔:生产恒为 [`MOBILE_SUBMIT_DELAY`],单测改短
    submit_delay: Duration,
    /// 长连保活 / 探活参数:生产恒为 [`KEEPALIVE`],单测改短
    keepalive: Keepalive,
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
            history_preview_cache: Mutex::new(None),
            recent_mobile_cmds: Mutex::new(HashMap::new()),
            command_tail: Mutex::new(None),
            submit_delay: MOBILE_SUBMIT_DELAY,
            keepalive: KEEPALIVE,
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
                .map_err(|e| tracing::error!("[mobile-relay] tokio runtime unavailable: {e}"))
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

    /// 撤销全部镜像订阅并清空历史预览缓存(与中转断线时调用;移动端重连后会重新订阅)。
    fn clear_mirror_subs(&self) {
        *self.history_preview_cache.lock() = None;
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
    /// 文件被截断/整份重写(偏移之前的原文对不上,见 [`mirror::TailCursor`])时清空
    /// runtime,下一轮轮询重新绑定。
    fn pump_mirror(
        &self,
        pane_id: &str,
        messages: &Mutex<Vec<MirrorMessage>>,
        runtime: &Mutex<Option<MirrorRuntime>>,
    ) {
        let mut slot = runtime.lock();
        let Some(rt) = slot.as_mut() else { return };
        match mirror::read_appended(&rt.path, &mut rt.cursor) {
            Some(bytes) => {
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

    /// 某 PTY 已知的精确会话身份:hook 上报优先(会随 `/clear` / `/new` 实时切换),
    /// 缺席时回落桌面端 pane 上记录的会话身份(如未开 hook 或 Codex resume 不重报 SessionStart)。
    fn known_session(&self, pty_id: u32) -> Option<crate::host::HookSessionId> {
        self.host
            .hook_session(pty_id)
            .or_else(|| self.host.pane_session(pty_id))
    }

    /// 这一轮的定位输入。绑定分两层,每轮重取(PTY 映射、hook 上报都可能后到):
    /// 1. 已有精确会话身份(hook 上报或 resume 写回的 pane 身份)→ 只认该会话的文件,
    ///    未落盘就等(空镜像),不退启发式——退了就会串到同项目其他 pane 的会话;
    /// 2. 无会话身份(未启用 hook)→ 退回"项目最新文件 + AI 启动时刻下限"启发式,
    ///    只找本 pane 那家 agent 的记录;同项目多个 Codex pane 按 [`CodexPeers`] 分。
    ///
    /// 这几问都是查内存表,每轮问一遍没有代价;贵的是拿着答案去扫磁盘,
    /// 那一步由 [`MirrorLocator::needs_locate`] 把关。
    fn mirror_locate_key(&self, pane_id: &str) -> LocateKey {
        let pty_id = self.pane_ptys.lock().get(pane_id).copied();
        if let Some(pty) = pty_id
            && let Some(s) = self.known_session(pty)
        {
            return LocateKey::Hook {
                pty_id: pty,
                agent: s.agent,
                session_id: s.session_id,
            };
        }
        // 启发式的前提是"这个 agent 会往磁盘写我们认识的会话记录"。
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
        let started = pty_id.and_then(|id| self.host.ai_session_started_at(id));
        let codex_peers = self.codex_peers(pane_id, agent.as_deref(), started);
        LocateKey::Heuristic {
            pty_id,
            agent,
            started,
            codex_peers,
        }
    }

    /// 同项目里其他 Codex pane 划给本 pane 的范围(口径见 [`CodexPeers`])。
    ///
    /// 只在本 pane 跑的是 Codex、且知道启动时刻时才算:别家的记录不带建会话时刻,
    /// 分不出来,照旧镜像项目最新的那个。同伴分两种 ——
    /// - 有 hook 身份的:它的会话是哪个已经确定,记进 `claimed` 不许别人绑;它的启动
    ///   时刻**不**当区间上界(hook 扶正记的启动时刻可能晚于它建会话,也没必要)。
    /// - 没有 hook 身份的:按启动时刻排队,晚于本 pane 的最早一个就是上界。
    ///   同一时刻启动的按 pane id 排,保证两个 pane 不会认领同一段。
    fn codex_peers(
        &self,
        pane_id: &str,
        agent: Option<&str>,
        started: Option<SystemTime>,
    ) -> CodexPeers {
        let is_codex =
            |agent: Option<&str>| agent.and_then(AgentKind::parse) == Some(AgentKind::Codex);
        let mut peers = CodexPeers::default();
        let Some(started) = started.filter(|_| is_codex(agent)) else {
            return peers;
        };
        let peer_panes: Vec<String> = {
            let paths = self.pane_paths.lock();
            let Some(own) = paths.get(pane_id).map(|p| windows_eq_key(p)) else {
                return peers;
            };
            paths
                .iter()
                .filter(|(id, path)| id.as_str() != pane_id && windows_eq_key(path) == own)
                .map(|(id, _)| id.clone())
                .collect()
        };
        let peer_ptys: Vec<(String, u32)> = {
            let ptys = self.pane_ptys.lock();
            peer_panes
                .into_iter()
                .filter_map(|id| ptys.get(&id).map(|pty| (id, *pty)))
                .collect()
        };
        for (peer_id, pty) in peer_ptys {
            let peer_agent = self.host.ai_session_agent(pty);
            if let Some(hook) = self.known_session(pty) {
                // codex 的 SessionStart 会被误推断成 claude-code,输入检测那边认的是 codex 也算
                if is_codex(hook.agent.as_deref()) || is_codex(peer_agent.as_deref()) {
                    peers.claimed.push(hook.session_id);
                }
                continue;
            }
            if !is_codex(peer_agent.as_deref()) {
                continue;
            }
            peers.contested = true;
            if let Some(peer_started) = self.host.ai_session_started_at(pty)
                && (peer_started, peer_id.as_str()) > (started, pane_id)
            {
                peers.next_start = Some(
                    peers
                        .next_start
                        .map_or(peer_started, |cur| cur.min(peer_started)),
                );
            }
        }
        peers.claimed.sort();
        peers.claimed.dedup();
        peers
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
        let mut parser = MirrorParser::new(agent, &job.project_path);
        let Some((mut msgs, cursor)) = mirror::parse_whole_file(&path, &mut parser) else {
            return false;
        };
        self.relabel_mobile_sources(&job.pane_id, &mut msgs, RelabelScope::History);
        let (page, has_more) = {
            let mut slot = job.runtime.lock();
            *slot = Some(MirrorRuntime {
                parser,
                path,
                cursor,
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
            supports_history: true,
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
                tracing::warn!("[mobile-relay] connect failed: {e}");
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

    // 保活 / 探活:定时 ping,中转(axum)读到即自动回 pong;收到任何帧都算活着
    let keepalive = manager.keepalive;
    let mut last_rx = Instant::now();
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + keepalive.interval,
        keepalive.interval,
    );
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // 已连接:读循环 + 出站转发,直到断线/取消
    let outcome = loop {
        tokio::select! {
            msg = ws.next() => {
                if matches!(msg, Some(Ok(_))) {
                    last_rx = Instant::now();
                }
                match msg {
                    Some(Ok(Message::Text(text))) => handle_relay_message(manager, &text),
                    Some(Ok(Message::Close(_))) | None => break Attempt::ConnectedThenLost,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::warn!("[mobile-relay] connection lost: {e}");
                        break Attempt::ConnectedThenLost;
                    }
                }
            },
            _ = ping.tick() => {
                let silent = last_rx.elapsed();
                if silent >= keepalive.timeout {
                    tracing::warn!(
                        "[mobile-relay] no frame from relay for {}s, reconnecting",
                        silent.as_secs()
                    );
                    break Attempt::ConnectedThenLost;
                }
                if ws.send(Message::Ping(Default::default())).await.is_err() {
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
        // 移动端请求项目的历史会话列表
        Ok(RelayToDesktop::RequestSessionHistory {
            request_id,
            project_id,
        }) => handle_request_session_history(manager, request_id, project_id),
        // 移动端请求单条历史会话的只读正文(分页)
        Ok(RelayToDesktop::RequestSessionContent {
            request_id,
            project_id,
            agent,
            session_id,
            before_seq,
        }) => handle_request_session_content(
            manager, request_id, project_id, agent, session_id, before_seq,
        ),
        // 移动端基于历史会话继续对话:校验归属与白名单、反查 cwd 后交给上层建 pane
        Ok(RelayToDesktop::ResumeAiSession {
            request_id,
            project_id,
            agent,
            session_id,
        }) => handle_resume_ai_session(manager, request_id, project_id, agent, session_id),
        Ok(_) => {}
        Err(_) => tracing::warn!("[mobile-relay] unparseable relay message (ignored)"),
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

/// [`RelayEvents::resume_session`] 的载荷:历史会话校验与 cwd 反查完成后交给上层执行。
/// 命令由桌面端按 `mt_ai::agent::resume_command` 本地模板生成,从不经过移动端或中转。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionPayload {
    pub request_id: String,
    pub project_id: String,
    /// 规范标识("claude" / "codex" / …)
    pub agent: String,
    /// 展示名("Claude Code" / "Codex" / …),用于桌面端审计通知
    pub agent_label: String,
    pub session_id: String,
    /// 收敛后的展示标题,用作新 pane 的自定义名
    pub title: String,
    /// 要写入 PTY 的续接命令
    pub command: String,
    /// 反查所得的会话原始启动目录(Claude 子目录会话续接用)
    pub cwd: Option<String>,
}

/// 历史会话标题 → 适合放到桌面端 tab 栏的 pane 标题:取第一个非空行、走
/// [`sanitize_pane_title`] 收敛,仍为空则回落 agent 展示名。
fn resume_pane_title(raw_title: &str, agent_label: &str) -> String {
    let first_line = raw_title
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let cleaned = sanitize_pane_title(first_line);
    if cleaned.is_empty() {
        sanitize_pane_title(agent_label)
    } else {
        cleaned
    }
}

/// 把 `mt_ai::sessions::AiSession` 映射成移动端历史条目。
fn to_mobile_history_session(
    session: mt_ai::sessions::AiSession,
    live_pane_id: Option<String>,
) -> MobileHistorySession {
    let kind = AgentKind::parse(&session.session_type);
    let agent = kind
        .map(|k| k.key().to_string())
        .unwrap_or_else(|| session.session_type.clone());
    let agent_label = kind
        .map(|k| k.spec().label.to_string())
        .unwrap_or_else(|| session.session_type.clone());
    let can_resume = mt_ai::agent::resume_command(Some(&agent), &session.id).is_some();
    MobileHistorySession {
        session_id: session.id,
        agent,
        agent_label,
        title: session.title,
        timestamp: session.timestamp,
        can_resume,
        live_pane_id,
    }
}

/// 处理移动端拉取某项目的历史会话列表。
///
/// 范围与 `StartAiSession` 一致:只覆盖本机宿主来源(SSH 远程项目与 WSL 根项目拒绝),
/// 磁盘扫描放到 tokio 阻塞线程池,不阻塞 WebSocket 读写与心跳。
fn handle_request_session_history(
    manager: &Arc<MobileRelayManager>,
    request_id: String,
    project_id: String,
) {
    let Some(project) = manager.host.project(&project_id) else {
        let _ = manager.send(DesktopToRelay::SessionHistory {
            request_id,
            project_id,
            ok: false,
            sessions: Vec::new(),
            reason: Some(HistoryFailReason::ProjectNotFound),
        });
        return;
    };
    if !can_start_session(&project.path, project.ssh_connection_id.as_deref()) {
        let _ = manager.send(DesktopToRelay::SessionHistory {
            request_id,
            project_id,
            ok: false,
            sessions: Vec::new(),
            reason: Some(HistoryFailReason::NotSupported),
        });
        return;
    }

    let mgr = Arc::clone(manager);
    let path = project.path;
    manager.spawn(async move {
        let result = tokio::task::spawn_blocking(move || mt_ai::sessions::get_ai_sessions(path))
            .await
            .map_err(|_| "history task cancelled".to_string())
            .and_then(|r| r);
        match result {
            Ok(raw) => {
                let sessions = raw
                    .into_iter()
                    .map(|s| {
                        let live = mgr.host.live_session_pane(&s.id);
                        to_mobile_history_session(s, live)
                    })
                    .collect();
                let _ = mgr.send(DesktopToRelay::SessionHistory {
                    request_id,
                    project_id,
                    ok: true,
                    sessions,
                    reason: None,
                });
            }
            Err(_) => {
                let _ = mgr.send(DesktopToRelay::SessionHistory {
                    request_id,
                    project_id,
                    ok: false,
                    sessions: Vec::new(),
                    reason: Some(HistoryFailReason::ReadFailed),
                });
            }
        }
    });
}

/// 处理移动端拉取某条历史会话的只读正文(按 `before_seq` 分页)。
///
/// 首次打开(`before_seq = None`)或缓存未命中时,先核对该会话确实归属目标项目
/// (防跨项目按 id 读全局目录里的 Codex / agy 记录),再整份解析并写入单槽缓存;
/// 上拉翻页(`before_seq = Some(_)`)命中缓存直接内存切片,零磁盘 IO。
fn handle_request_session_content(
    manager: &Arc<MobileRelayManager>,
    request_id: String,
    project_id: String,
    agent: String,
    session_id: String,
    before_seq: Option<u64>,
) {
    let send_fail = |mgr: &MobileRelayManager,
                     request_id: String,
                     project_id: String,
                     session_id: String,
                     reason: HistoryFailReason| {
        let _ = mgr.send(DesktopToRelay::SessionContent {
            request_id,
            project_id,
            session_id,
            ok: false,
            messages: Vec::new(),
            has_more: false,
            reason: Some(reason),
        });
    };

    let Some(project) = manager.host.project(&project_id) else {
        send_fail(
            manager,
            request_id,
            project_id,
            session_id,
            HistoryFailReason::ProjectNotFound,
        );
        return;
    };
    if !can_start_session(&project.path, project.ssh_connection_id.as_deref()) {
        send_fail(
            manager,
            request_id,
            project_id,
            session_id,
            HistoryFailReason::NotSupported,
        );
        return;
    }
    let Some(kind) = AgentKind::parse(&agent) else {
        send_fail(
            manager,
            request_id,
            project_id,
            session_id,
            HistoryFailReason::UnsupportedAgent,
        );
        return;
    };
    if !kind.spec().history || !mt_ai::agent::session_id_ok(&session_id) {
        send_fail(
            manager,
            request_id,
            project_id,
            session_id,
            HistoryFailReason::UnsupportedAgent,
        );
        return;
    }

    let canonical_agent = kind.key().to_string();
    // 翻页命中同会话单槽缓存:直接内存切片返回
    if let Some(before) = before_seq {
        let cached = manager.history_preview_cache.lock();
        if let Some(entry) = cached.as_ref()
            && entry.project_id == project_id
            && entry.agent == canonical_agent
            && entry.session_id == session_id
        {
            let (messages, has_more) =
                history_slice(&entry.messages, Some(before), MIRROR_PAGE_SIZE);
            drop(cached);
            let _ = manager.send(DesktopToRelay::SessionContent {
                request_id,
                project_id,
                session_id,
                ok: true,
                messages,
                has_more,
                reason: None,
            });
            return;
        }
    }

    let mgr = Arc::clone(manager);
    let path = project.path;
    manager.spawn(async move {
        let pid = project_id.clone();
        let sid = session_id.clone();
        let agent_key = canonical_agent.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            let sessions = mt_ai::sessions::get_ai_sessions(path.clone())
                .map_err(|_| HistoryFailReason::ReadFailed)?;
            let belongs = sessions
                .iter()
                .any(|s| s.id == sid && AgentKind::parse(&s.session_type) == Some(kind));
            if !belongs {
                return Err(HistoryFailReason::SessionNotFound);
            }
            mirror::parse_session_history_messages(&path, &agent_key, &sid)
                .map_err(|_| HistoryFailReason::ReadFailed)
        })
        .await
        .map_err(|_| HistoryFailReason::ReadFailed)
        .and_then(|r| r);

        match loaded {
            Ok(all_messages) => {
                let (messages, has_more) =
                    history_slice(&all_messages, before_seq, MIRROR_PAGE_SIZE);
                *mgr.history_preview_cache.lock() = Some(HistoryPreviewCache {
                    project_id: pid,
                    agent: canonical_agent,
                    session_id: session_id.clone(),
                    messages: all_messages,
                });
                let _ = mgr.send(DesktopToRelay::SessionContent {
                    request_id,
                    project_id,
                    session_id,
                    ok: true,
                    messages,
                    has_more,
                    reason: None,
                });
            }
            Err(reason) => {
                let _ = mgr.send(DesktopToRelay::SessionContent {
                    request_id,
                    project_id,
                    session_id,
                    ok: false,
                    messages: Vec::new(),
                    has_more: false,
                    reason: Some(reason),
                });
            }
        }
    });
}

/// 处理移动端基于历史会话继续对话:校验项目、白名单、项目归属并反查 cwd,
/// 再把本地模板生成的 resume 命令交由上层在后台新建 pane(或直接复用已在运行的 pane)。
fn handle_resume_ai_session(
    manager: &Arc<MobileRelayManager>,
    request_id: String,
    project_id: String,
    agent: String,
    session_id: String,
) {
    let Some(project) = manager.host.project(&project_id) else {
        manager.send_start_receipt(
            request_id,
            None,
            Some(StartSessionFailReason::ProjectNotFound),
        );
        return;
    };
    if !can_start_session(&project.path, project.ssh_connection_id.as_deref()) {
        manager.send_start_receipt(request_id, None, Some(StartSessionFailReason::NotSupported));
        return;
    }
    let Some(kind) = AgentKind::parse(&agent) else {
        manager.send_start_receipt(
            request_id,
            None,
            Some(StartSessionFailReason::ResumeUnsupported),
        );
        return;
    };
    if kind.resume_command(&session_id).is_none() {
        manager.send_start_receipt(
            request_id,
            None,
            Some(StartSessionFailReason::ResumeUnsupported),
        );
        return;
    }

    let mgr = Arc::clone(manager);
    let path = project.path;
    manager.spawn(async move {
        let sid = session_id.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            let sessions = mt_ai::sessions::get_ai_sessions(path)
                .map_err(|_| StartSessionFailReason::SessionNotFound)?;
            let matched = sessions
                .into_iter()
                .find(|s| s.id == sid && AgentKind::parse(&s.session_type) == Some(kind))
                .ok_or(StartSessionFailReason::SessionNotFound)?;
            let command = kind
                .resume_command(&matched.id)
                .ok_or(StartSessionFailReason::ResumeUnsupported)?;
            let cwd = if kind.looks_up_session_cwd() {
                mt_ai::sessions::lookup_ai_session_cwd(matched.id.clone())
            } else {
                None
            };
            Ok((matched, command, cwd))
        })
        .await
        .map_err(|_| StartSessionFailReason::SpawnFailed)
        .and_then(|r| r);

        match resolved {
            Ok((matched, command, cwd)) => {
                let agent_label = kind.spec().label.to_string();
                let title = resume_pane_title(&matched.title, &agent_label);
                mgr.events.resume_session(ResumeSessionPayload {
                    request_id,
                    project_id,
                    agent: kind.key().to_string(),
                    agent_label,
                    session_id: matched.id,
                    title,
                    command,
                    cwd,
                });
            }
            Err(reason) => {
                mgr.send_start_receipt(request_id, None, Some(reason));
            }
        }
    });
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
        manager.send_start_receipt(
            request_id,
            None,
            Some(StartSessionFailReason::ProjectNotFound),
        );
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
/// - bracketed paste 开着:与桌面端粘贴(mt-ui `paste_to_bytes`)共用
///   [`mt_core::paste::bracketed_paste`] —— 换行归一成 `\r`、剔掉正文里的开始 /
///   结束标记(含剔掉之后重新拼出来的;防正文提前「结束粘贴」、后半段被当按键)、
///   包上 `ESC[200~ … ESC[201~`。多行指令因此整块是粘贴内容,中途不会提交;
/// - 没开:裸写,换行一律归一成 `\n`(LF = Ctrl+J,Claude Code 输入框里是「插入
///   换行」),只让随后单独写的那个 `\r` 承担提交。
fn mobile_command_body(text: &str, bracketed: bool) -> String {
    if bracketed {
        mt_core::paste::bracketed_paste(text)
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
    /// 桌面粘贴同口径 —— 换行归一成 CR、剔掉内嵌的开始 / 结束标记。
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
        // 删掉一个标记后拼出来的结束标记同样剔掉,后半段不会溢出成按键
        assert_eq!(
            mobile_command_body("x\x1b[201\x1b[201~~\nrm -rf ~", true),
            "\x1b[200~x\rrm -rf ~\x1b[201~"
        );
        assert_eq!(
            mobile_command_body("x\x1b[200~y", true),
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
    /// `ptys` 里登记过的 pty 按登记的答,其余一律答上面三个字段(单 pane 用例的老口径)。
    #[derive(Default)]
    struct FakeHost {
        hook: Mutex<Option<HookSessionId>>,
        agent: Mutex<Option<String>>,
        started: Mutex<Option<SystemTime>>,
        ptys: Mutex<HashMap<u32, FakePty>>,
    }

    #[derive(Default, Clone)]
    struct FakePty {
        hook: Option<HookSessionId>,
        agent: Option<String>,
        started: Option<SystemTime>,
    }

    impl FakeHost {
        fn pty(&self, pty_id: u32) -> Option<FakePty> {
            self.ptys.lock().get(&pty_id).cloned()
        }
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
        fn hook_session(&self, pty_id: u32) -> Option<HookSessionId> {
            match self.pty(pty_id) {
                Some(p) => p.hook,
                None => self.hook.lock().clone(),
            }
        }
        fn ai_session_agent(&self, pty_id: u32) -> Option<String> {
            match self.pty(pty_id) {
                Some(p) => p.agent,
                None => self.agent.lock().clone(),
            }
        }
        fn ai_session_started_at(&self, pty_id: u32) -> Option<SystemTime> {
            match self.pty(pty_id) {
                Some(p) => p.started,
                None => *self.started.lock(),
            }
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

    /// 整份重写成**更长**的内容(agy 压缩上下文就是这样):长度判不出,续读锚判出来,
    /// 泵清运行态、下一轮从头换绑,镜像里是重写后的全量,不是旧消息 + 半截乱行。
    #[test]
    fn hook_bound_rebinds_after_longer_rewrite() {
        let fx = MirrorFixture::new("rewrite");
        let a = fx.dir.join("a.jsonl");
        std::fs::write(&a, user_line("a1") + &user_line("a2")).unwrap();
        let calls = Cell::new(0);
        let locate = |_: &str, _: &LocateKey| {
            calls.set(calls.get() + 1);
            Some((a.clone(), MirrorAgent::Claude))
        };
        let t0 = Instant::now();
        let mut loc = MirrorLocator::default();
        *fx.host.hook.lock() = hook("sid-a");
        fx.round(&mut loc, t0, locate);
        assert_eq!(fx.texts(), ["a1", "a2"]);

        let rewritten = user_line("summary") + &user_line("r1") + &user_line("r2");
        assert!(rewritten.len() > user_line("a1").len() * 2);
        std::fs::write(&a, rewritten).unwrap();
        fx.round(&mut loc, t0 + Duration::from_secs(1), locate);
        assert_eq!(fx.bound(), None, "重写由泵发现并清运行态");
        assert_eq!(
            fx.texts(),
            ["a1", "a2"],
            "没有把重写后的半截内容当增量喂进去"
        );
        fx.round(&mut loc, t0 + Duration::from_secs(2), locate);
        assert_eq!(calls.get(), 2);
        assert_eq!(fx.texts(), ["summary", "r1", "r2"]);

        // 换绑之后照常增量
        append(&a, "r3");
        fx.round(&mut loc, t0 + Duration::from_secs(3), locate);
        assert_eq!(fx.texts(), ["summary", "r1", "r2", "r3"]);
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

    /// agy 写我们认得的 transcript:没有 hook 身份时照别家走启发式(项目最新的主会话),
    /// 不落进 NoLog;hook 身份到了就按身份精确定位。
    #[test]
    fn agy_without_hook_uses_heuristic_and_with_hook_uses_identity() {
        let fx = MirrorFixture::new("agy-key");
        *fx.host.agent.lock() = Some("agy".into());
        assert!(matches!(
            fx.manager.mirror_locate_key("pane-1"),
            LocateKey::Heuristic { agent: Some(ref a), .. } if a == "agy"
        ));
        *fx.host.hook.lock() = Some(HookSessionId {
            agent: Some("agy".into()),
            session_id: "7e54f704-1278-4123-8ad4-11e8f6922c82".into(),
        });
        assert!(matches!(
            fx.manager.mirror_locate_key("pane-1"),
            LocateKey::Hook { agent: Some(ref a), .. } if a == "agy"
        ));
    }

    /// 没有会话记录的 agent(opencode):定位输入是 NoLog,生产定位函数
    /// 对它不做任何 IO;输入不变就连这一问都省掉,运行态始终是空的。
    #[test]
    fn nolog_agent_never_binds() {
        let fx = MirrorFixture::new("nolog");
        *fx.host.agent.lock() = Some("opencode".into());
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

    // ── 长连保活 / 探活 ──

    /// 本机起一个中转替身:收下桌面端 hello、回 HelloAck,之后交给 `after` 处置连接。
    async fn fake_relay<F, Fut>(after: F) -> String
    where
        F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut
            + Send
            + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _hello = ws.next().await;
            let ack = RelayToDesktop::HelloAck {
                protocol_version: PROTOCOL_VERSION,
            };
            ws.send(Message::Text(serde_json::to_string(&ack).unwrap().into()))
                .await
                .unwrap();
            after(ws).await;
        });
        format!("ws://{addr}/ws/desktop")
    }

    fn keepalive_manager() -> Arc<MobileRelayManager> {
        let mut manager = MobileRelayManager::new(Arc::new(NoopRelayHost), Arc::new(NoopRelayHost));
        manager.keepalive = Keepalive {
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(300),
        };
        Arc::new(manager)
    }

    fn keepalive_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// 连接半开:中转一个帧都不回(不读也就不回 pong),桌面端超时后自己判死重连,
    /// 不会一直挂着「已连接」让手机的订阅落空。
    #[test]
    fn keepalive_gives_up_on_silent_relay() {
        keepalive_runtime().block_on(async {
            let url = fake_relay(|ws| async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(ws);
            })
            .await;
            let manager = keepalive_manager();
            let (_cancel_tx, mut cancel_rx) = watch::channel(false);
            let started = Instant::now();
            let outcome = tokio::time::timeout(
                Duration::from_secs(5),
                connect_once(&manager, &url, "key", &mut cancel_rx),
            )
            .await
            .expect("应在探活超时后自行断开");
            assert!(matches!(outcome, Attempt::ConnectedThenLost));
            assert!(started.elapsed() >= Duration::from_millis(300));
            assert!(manager.outbound.lock().is_none(), "断开后出站通道已撤");
        });
    }

    /// 中转正常读(自动回 pong)时,超过探活时限也不断:ping 的回音让连接一直算活着。
    #[test]
    fn keepalive_keeps_responsive_relay() {
        keepalive_runtime().block_on(async {
            let url = fake_relay(|mut ws| async move {
                let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
                loop {
                    tokio::select! {
                        msg = ws.next() => if msg.is_none() { return },
                        _ = tokio::time::sleep_until(deadline) => break,
                    }
                }
                let _ = ws.close(None).await;
            })
            .await;
            let manager = keepalive_manager();
            let (_cancel_tx, mut cancel_rx) = watch::channel(false);
            let started = Instant::now();
            let outcome = tokio::time::timeout(
                Duration::from_secs(5),
                connect_once(&manager, &url, "key", &mut cancel_rx),
            )
            .await
            .expect("中转关闭后应返回");
            assert!(matches!(outcome, Attempt::ConnectedThenLost));
            assert!(
                started.elapsed() >= Duration::from_millis(900),
                "一直有 pong 回来,不该在 {:?} 就判死",
                started.elapsed()
            );
        });
    }

    // ── issue #103:同项目多个 Codex pane 分会话 ──

    fn codex_pty(started: SystemTime) -> FakePty {
        FakePty {
            hook: None,
            agent: Some("codex".into()),
            started: Some(started),
        }
    }

    fn peers_of(fx: &MirrorFixture, pane_id: &str) -> CodexPeers {
        match fx.manager.mirror_locate_key(pane_id) {
            LocateKey::Heuristic { codex_peers, .. } => codex_peers,
            other => panic!("应走启发式: {other:?}"),
        }
    }

    fn contested(next_start: Option<SystemTime>) -> CodexPeers {
        CodexPeers {
            contested: true,
            next_start,
            claimed: vec![],
        }
    }

    /// issue #103:同项目的无 hook Codex pane 按启动时刻排开,各自的区间上界是下一个
    /// 启动的同伴;别的项目、别家 agent 不算同伴;同伴拿到 hook 身份后改记成「已认领」,
    /// 不再当上界;同一时刻启动的按 pane id 排,不会两个 pane 认领同一段。
    #[test]
    fn codex_peers_partition_same_project_panes_by_start() {
        let fx = MirrorFixture::new("codex-peers");
        let t = |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        {
            let mut paths = fx.manager.pane_paths.lock();
            paths.insert("pane-1".into(), r"D:\proj".into());
            // 同一个项目,写法不同(大小写 / 分隔符 / 尾随分隔符)
            paths.insert("pane-2".into(), "d:/proj/".into());
            paths.insert("pane-3".into(), r"D:\other".into());
            paths.insert("pane-4".into(), r"D:\proj".into());
        }
        {
            let mut ptys = fx.manager.pane_ptys.lock();
            ptys.insert("pane-2".into(), 8);
            ptys.insert("pane-3".into(), 9);
            ptys.insert("pane-4".into(), 10);
        }
        {
            let mut ptys = fx.host.ptys.lock();
            ptys.insert(7, codex_pty(t(100)));
            ptys.insert(8, codex_pty(t(200)));
            // 别的项目里的 Codex
            ptys.insert(9, codex_pty(t(150)));
            // 同项目里的 Claude
            ptys.insert(
                10,
                FakePty {
                    hook: None,
                    agent: Some("claude".into()),
                    started: Some(t(120)),
                },
            );
        }
        let before = fx.manager.mirror_locate_key("pane-1");
        assert_eq!(peers_of(&fx, "pane-1"), contested(Some(t(200))));
        assert_eq!(peers_of(&fx, "pane-2"), contested(None));
        assert_eq!(
            peers_of(&fx, "pane-4"),
            CodexPeers::default(),
            "别家 agent 的 pane 不划区间"
        );

        // pane-2 拿到 hook 身份(codex 的 SessionStart 会被误推断成 claude-code,
        // 输入检测认的是 codex 也算):改记成已认领
        fx.host.ptys.lock().get_mut(&8).unwrap().hook = Some(HookSessionId {
            agent: Some("claude-code".into()),
            session_id: "sid-b".into(),
        });
        assert_eq!(
            peers_of(&fx, "pane-1"),
            CodexPeers {
                contested: false,
                next_start: None,
                claimed: vec!["sid-b".into()],
            }
        );
        assert_ne!(
            fx.manager.mirror_locate_key("pane-1"),
            before,
            "同伴的变化进定位输入,当轮重新定位"
        );

        // 同一时刻启动:pane id 小的上界就是对方的启动时刻(区间为空),大的拿到全部
        {
            let mut ptys = fx.host.ptys.lock();
            ptys.insert(8, codex_pty(t(100)));
        }
        assert_eq!(peers_of(&fx, "pane-1"), contested(Some(t(100))));
        assert_eq!(peers_of(&fx, "pane-2"), contested(None));
    }

    /// 本 pane 不是 Codex,或不知道启动时刻:不划区间(照旧镜像项目最新)。
    #[test]
    fn codex_peers_empty_without_codex_or_start() {
        let fx = MirrorFixture::new("codex-peers-none");
        let t = |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        {
            let mut paths = fx.manager.pane_paths.lock();
            paths.insert("pane-1".into(), r"D:\proj".into());
            paths.insert("pane-2".into(), r"D:\proj".into());
        }
        fx.manager.pane_ptys.lock().insert("pane-2".into(), 8);
        fx.host.ptys.lock().insert(8, codex_pty(t(200)));

        fx.host.ptys.lock().insert(
            7,
            FakePty {
                hook: None,
                agent: Some("codex".into()),
                started: None,
            },
        );
        assert_eq!(peers_of(&fx, "pane-1"), CodexPeers::default());

        fx.host.ptys.lock().insert(
            7,
            FakePty {
                hook: None,
                agent: Some("grok".into()),
                started: Some(t(100)),
            },
        );
        assert_eq!(peers_of(&fx, "pane-1"), CodexPeers::default());
    }

    // ── issue #104:历史会话列表、预览缓存与续接校验 ──

    #[derive(Default)]
    struct HistoryTestHost {
        projects: Mutex<HashMap<String, RelayProject>>,
        pane_sessions: Mutex<HashMap<u32, HookSessionId>>,
    }

    impl RelayHost for HistoryTestHost {
        fn launchers(&self) -> Vec<AiLauncher> {
            Vec::new()
        }
        fn project(&self, project_id: &str) -> Option<RelayProject> {
            self.projects.lock().get(project_id).cloned()
        }
        fn write_pty(&self, _pty_id: u32, _data: String) -> Result<(), String> {
            Ok(())
        }
        fn hook_session(&self, _pty_id: u32) -> Option<HookSessionId> {
            None
        }
        fn pane_session(&self, pty_id: u32) -> Option<HookSessionId> {
            self.pane_sessions.lock().get(&pty_id).cloned()
        }
        fn ai_session_agent(&self, _pty_id: u32) -> Option<String> {
            Some("codex".into())
        }
        fn ai_session_started_at(&self, _pty_id: u32) -> Option<SystemTime> {
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(100))
        }
    }

    #[test]
    fn resume_pane_title_takes_first_non_empty_line_and_falls_back() {
        assert_eq!(
            resume_pane_title("\n  \n  修复登录\t问题\n第二行", "Claude Code"),
            "修复登录问题"
        );
        assert_eq!(resume_pane_title("   \n\r\n  ", "Codex"), "Codex");
    }

    #[test]
    fn to_mobile_history_session_maps_agent_label_resume_and_live_pane() {
        let claude = to_mobile_history_session(
            mt_ai::sessions::AiSession {
                id: "0199a1b2-c3d4-7e8f-9012-3456789abcde".into(),
                session_type: "claude".into(),
                title: "排查性能".into(),
                timestamp: "2026-11-21T08:00:00Z".into(),
                model: None,
                wsl_distro: None,
                ssh_connection_id: None,
            },
            Some("pane-9".into()),
        );
        assert_eq!(claude.agent, "claude");
        assert_eq!(claude.agent_label, "Claude Code");
        assert!(claude.can_resume);
        assert_eq!(claude.live_pane_id.as_deref(), Some("pane-9"));

        let bad = to_mobile_history_session(
            mt_ai::sessions::AiSession {
                id: "--dangerously-skip-permissions".into(),
                session_type: "claude".into(),
                title: "可疑".into(),
                timestamp: String::new(),
                model: None,
                wsl_distro: None,
                ssh_connection_id: None,
            },
            None,
        );
        assert!(!bad.can_resume, "参数注入形态的 id 不允许续接");
    }

    #[test]
    fn history_and_resume_reject_missing_remote_and_invalid_inputs_synchronously() {
        let host = Arc::new(HistoryTestHost::default());
        {
            let mut projects = host.projects.lock();
            projects.insert(
                "local".into(),
                RelayProject {
                    path: r"D:\proj".into(),
                    ssh_connection_id: None,
                },
            );
            projects.insert(
                "remote".into(),
                RelayProject {
                    path: "/srv/app".into(),
                    ssh_connection_id: Some("conn-1".into()),
                },
            );
            projects.insert(
                "wsl".into(),
                RelayProject {
                    path: r"\\wsl$\Ubuntu\home\u\app".into(),
                    ssh_connection_id: None,
                },
            );
        }
        let manager = Arc::new(MobileRelayManager::new(host, Arc::new(NoopRelayHost)));
        let (tx, mut rx) = mpsc::unbounded_channel();
        *manager.outbound.lock() = Some(tx);

        // 历史列表:不存在 / 远程 / WSL 根项目
        handle_request_session_history(&manager, "h1".into(), "gone".into());
        assert_eq!(
            rx.try_recv().unwrap(),
            DesktopToRelay::SessionHistory {
                request_id: "h1".into(),
                project_id: "gone".into(),
                ok: false,
                sessions: vec![],
                reason: Some(HistoryFailReason::ProjectNotFound),
            }
        );
        for pid in ["remote", "wsl"] {
            handle_request_session_history(&manager, "h2".into(), pid.into());
            assert_eq!(
                rx.try_recv().unwrap(),
                DesktopToRelay::SessionHistory {
                    request_id: "h2".into(),
                    project_id: pid.into(),
                    ok: false,
                    sessions: vec![],
                    reason: Some(HistoryFailReason::NotSupported),
                }
            );
        }

        // 正文请求:非法 id / 无历史记录的 opencode 同步拒绝
        handle_request_session_content(
            &manager,
            "c1".into(),
            "local".into(),
            "claude".into(),
            "../etc/passwd".into(),
            None,
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            DesktopToRelay::SessionContent {
                request_id: "c1".into(),
                project_id: "local".into(),
                session_id: "../etc/passwd".into(),
                ok: false,
                messages: vec![],
                has_more: false,
                reason: Some(HistoryFailReason::UnsupportedAgent),
            }
        );
        handle_request_session_content(
            &manager,
            "c2".into(),
            "local".into(),
            "opencode".into(),
            "abc123".into(),
            None,
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            DesktopToRelay::SessionContent {
                request_id: "c2".into(),
                project_id: "local".into(),
                session_id: "abc123".into(),
                ok: false,
                messages: vec![],
                has_more: false,
                reason: Some(HistoryFailReason::UnsupportedAgent),
            }
        );

        // 续接:非法 id(命令注入 / 参数注入)与远程项目同步拒绝
        for bad_id in ["abc;rm -rf /", "--dangerously-skip-permissions"] {
            handle_resume_ai_session(
                &manager,
                "r1".into(),
                "local".into(),
                "claude".into(),
                bad_id.into(),
            );
            assert_eq!(
                rx.try_recv().unwrap(),
                DesktopToRelay::StartSessionReceipt {
                    request_id: "r1".into(),
                    ok: false,
                    pane_id: None,
                    reason: Some(StartSessionFailReason::ResumeUnsupported),
                }
            );
        }
        handle_resume_ai_session(
            &manager,
            "r2".into(),
            "remote".into(),
            "claude".into(),
            "abc123".into(),
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            DesktopToRelay::StartSessionReceipt {
                request_id: "r2".into(),
                ok: false,
                pane_id: None,
                reason: Some(StartSessionFailReason::NotSupported),
            }
        );
    }

    #[test]
    fn history_content_pagination_hits_single_slot_cache() {
        let host = Arc::new(HistoryTestHost::default());
        host.projects.lock().insert(
            "local".into(),
            RelayProject {
                path: r"D:\proj".into(),
                ssh_connection_id: None,
            },
        );
        let manager = Arc::new(MobileRelayManager::new(host, Arc::new(NoopRelayHost)));
        let (tx, mut rx) = mpsc::unbounded_channel();
        *manager.outbound.lock() = Some(tx);

        let msgs: Vec<MirrorMessage> = (0..60u64)
            .map(|seq| MirrorMessage {
                seq,
                source: "assistant".into(),
                content: format!("m-{seq}"),
                timestamp: String::new(),
                ..Default::default()
            })
            .collect();
        *manager.history_preview_cache.lock() = Some(HistoryPreviewCache {
            project_id: "local".into(),
            agent: "claude".into(),
            session_id: "sid-1".into(),
            messages: msgs,
        });

        handle_request_session_content(
            &manager,
            "page-2".into(),
            "local".into(),
            "claude".into(),
            "sid-1".into(),
            Some(10),
        );
        let reply = rx.try_recv().unwrap();
        match reply {
            DesktopToRelay::SessionContent {
                request_id,
                ok,
                messages,
                has_more,
                ..
            } => {
                assert_eq!(request_id, "page-2");
                assert!(ok);
                assert!(!has_more);
                assert_eq!(messages.len(), 10);
                assert_eq!(messages[0].seq, 0);
                assert_eq!(messages[9].seq, 9);
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    /// 无 hook 但有 `pane_session`(例如 codex resume 写回的身份)时:
    /// 本 pane 精确走 LocateKey::Hook,且同项目其他无 hook Codex pane 把它记进 claimed。
    #[test]
    fn pane_session_fallback_binds_precisely_and_claims_codex_peer() {
        let host = Arc::new(HistoryTestHost::default());
        host.pane_sessions.lock().insert(
            7,
            HookSessionId {
                agent: Some("codex".into()),
                session_id: "resumed-sid".into(),
            },
        );
        let manager = MobileRelayManager::new(host, Arc::new(NoopRelayHost));
        manager
            .pane_paths
            .lock()
            .insert("pane-1".into(), r"D:\proj".into());
        manager
            .pane_paths
            .lock()
            .insert("pane-2".into(), r"D:\proj".into());
        manager.pane_ptys.lock().insert("pane-1".into(), 7);
        manager.pane_ptys.lock().insert("pane-2".into(), 8);

        assert_eq!(
            manager.mirror_locate_key("pane-1"),
            LocateKey::Hook {
                pty_id: 7,
                agent: Some("codex".into()),
                session_id: "resumed-sid".into(),
            }
        );
        match manager.mirror_locate_key("pane-2") {
            LocateKey::Heuristic { codex_peers, .. } => {
                assert_eq!(codex_peers.claimed, vec!["resumed-sid".to_string()]);
                assert!(!codex_peers.contested);
            }
            other => panic!("pane-2 应走启发式并排除已认领会话: {other:?}"),
        }
    }
}
