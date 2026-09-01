//! 移动端中转体系:桌面端 → 中转服务器的出站 WebSocket 长连(docs/adr/0001)。
//!
//! 连接由本 crate 持有:握手校验协议版本,断线后指数退避自动重连;
//! 状态变化经 [`RelayEvents::status_changed`] 交给上层(设置页「移动端」区域展示)。
//! 版本不匹配时停止重连(重试无意义),等待用户升级。

use parking_lot::Mutex;
use std::time::Duration;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    CommandFailReason, DesktopRejectReason, DesktopToRelay, MirrorMessage, MobileLauncher,
    MobilePane, MobileProject, RelayToDesktop, StartSessionFailReason, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::host::{AiLauncher, RelayEvents, RelayHost};
use crate::mirror::{self, history_slice, MirrorParser, MIRROR_PAGE_SIZE};
use crate::util::is_wsl_unc_path;

/// 握手 ack 等待超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 镜像会话文件轮询间隔。
const MIRROR_POLL_INTERVAL: Duration = Duration::from_secs(1);

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
    recent_mobile_cmds: Mutex<HashMap<String, Vec<String>>>,
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

    /// 登记一条已写入 PTY 的移动端指令原文(镜像回流改标来源用,每 pane 上限 20 条)。
    fn record_mobile_cmd(&self, pane_id: &str, text: &str) {
        let mut map = self.recent_mobile_cmds.lock();
        let list = map.entry(pane_id.to_string()).or_default();
        list.push(text.trim().to_string());
        if list.len() > 20 {
            list.remove(0);
        }
    }

    /// 镜像新消息回流时调用:与最近移动端指令逐字匹配的 user 消息改标 "mobile"。
    /// 不匹配保持 "desktop"(误差方向安全:最多把移动端指令标成桌面输入)。
    fn relabel_mobile_sources(&self, pane_id: &str, messages: &mut [MirrorMessage]) {
        let mut map = self.recent_mobile_cmds.lock();
        let Some(list) = map.get_mut(pane_id) else {
            return;
        };
        for msg in messages.iter_mut() {
            if msg.source != "desktop" {
                continue;
            }
            // 作答标记按结构化 labels 对账——多题标记的 content 是合并文本,
            // 与逐条登记的单个 label 对不上。全部选中项都出自本端登记才改标
            if msg.kind.as_deref() == Some("questionAnswered") {
                if !msg.labels.is_empty()
                    && msg.labels.iter().all(|l| list.iter().any(|c| c == l))
                {
                    msg.source = "mobile".into();
                    for label in &msg.labels {
                        if let Some(pos) = list.iter().position(|c| c == label) {
                            list.remove(pos);
                        }
                    }
                }
                continue;
            }
            if let Some(pos) = list.iter().position(|cmd| cmd == msg.content.trim()) {
                msg.source = "mobile".into();
                list.remove(pos);
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
                self.relabel_mobile_sources(pane_id, &mut new_msgs);
                messages.lock().extend(new_msgs.clone());
                let _ = self.send(DesktopToRelay::MirrorAppend {
                    pane_id: pane_id.into(),
                    messages: new_msgs,
                });
            }
            None => *slot = None,
        }
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
        self.spawn(async move {
            mirror_task(manager, pane_id, project_path, messages, runtime, cancel_rx).await;
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
        // 移动端指令:到达即写入目标 pane 的 PTY(写穿,不排队),随即回执
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

/// 写穿移动端指令:等价本人在桌面对该终端敲入同样内容并回车。
/// 回执仅表示"已写入 PTY",AI 真正接收以镜像回流为准。
fn handle_mobile_command(
    manager: &Arc<MobileRelayManager>,
    pane_id: String,
    command_id: String,
    text: String,
) {
    let pty_id = manager.pane_ptys.lock().get(&pane_id).copied();
    let result = match pty_id {
        None => Err(CommandFailReason::PaneNotFound),
        Some(pty_id) => {
            // 走宿主的写穿口(输入跟踪/AI marker/SSH autofill 解除全语义),
            // 文本 + \r 一次写入 = 敲入内容并回车;AI 工作中依赖 CLI 自身输入缓冲
            let data = format!("{text}\r");
            manager
                .host
                .write_pty(pty_id, data)
                .map_err(|_| CommandFailReason::WriteFailed)
        }
    };
    if result.is_ok() {
        manager.record_mobile_cmd(&pane_id, &text);
    }
    manager.send_command_receipt(pane_id, command_id, result);
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
async fn mirror_task(
    manager: Arc<MobileRelayManager>,
    pane_id: String,
    project_path: String,
    messages: Arc<Mutex<Vec<MirrorMessage>>>,
    runtime: Arc<Mutex<Option<MirrorRuntime>>>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let mut sent_initial = false;

    loop {
        // 绑定分两层,每轮重取(PTY 映射、hook 上报都可能后到):
        // 1. hook 上报过会话身份 → 只认该会话的文件,未落盘就等(空镜像),
        //    不退启发式——退了就会串到同项目其他 pane 的会话;
        // 2. 无会话身份(未启用 hook)→ 退回"项目最新文件 + AI 启动时刻下限"启发式。
        let pty_id = manager.pane_ptys.lock().get(&pane_id).copied();
        let resolved = match pty_id.and_then(|id| manager.host.hook_session(id)) {
            Some(s) => {
                mirror::resolve_session_file_by_id(&project_path, s.agent.as_deref(), &s.session_id)
            }
            None => {
                // 启发式的前提是"这个 agent 会往磁盘写我们认识的会话记录"。pi /
                // opencode 不写(或格式不认),此时退启发式就会绑到同项目里 Claude/
                // Codex 的最新文件,把别人的对话贴到这个 pane 上——比空镜像更糟。
                let agent = pty_id.and_then(|id| manager.host.ai_session_agent(id));
                if agent.is_some_and(|a| !mirror::agent_has_session_log(&a)) {
                    None
                } else {
                    let ai_started =
                        pty_id.and_then(|id| manager.host.ai_session_started_at(id));
                    mirror::resolve_session_file(&project_path, ai_started)
                }
            }
        };
        match resolved {
            None => {
                // 属于本轮会话的文件尚未出现(AI 刚启动还没落盘):先给空快照,出现后再重发
                if !sent_initial {
                    sent_initial = true;
                    let _ = manager.send(DesktopToRelay::MirrorSnapshot {
                        pane_id: pane_id.clone(),
                        messages: vec![],
                        has_more: false,
                    });
                }
            }
            Some((path, agent)) => {
                let rebind = runtime.lock().as_ref().is_none_or(|rt| rt.path != path);
                if rebind {
                    // 首次绑定或换绑到更新的会话文件:全量解析 + 重发快照。
                    // 全量读在锁外(文件可能不小);换入运行态与替换消息缓存在
                    // 同一次持锁内完成,防点选作答的泵在中间插进来喂错解析器
                    if let Some((bytes, offset)) = mirror::read_from_offset(&path, 0) {
                        let (page, has_more) = {
                            let mut slot = runtime.lock();
                            let rt = slot.insert(MirrorRuntime {
                                parser: MirrorParser::new(agent),
                                path,
                                offset,
                            });
                            let msgs = rt.parser.feed(&bytes);
                            let mut m = messages.lock();
                            *m = msgs;
                            history_slice(&m, None, MIRROR_PAGE_SIZE)
                        };
                        sent_initial = true;
                        let _ = manager.send(DesktopToRelay::MirrorSnapshot {
                            pane_id: pane_id.clone(),
                            messages: page,
                            has_more,
                        });
                    }
                } else {
                    // 增量与「文件被截断→清运行态待重绑」都在泵里
                    manager.pump_mirror(&pane_id, &messages, &runtime);
                }
            }
        }

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
        manager.relabel_mobile_sources("pane-1", &mut msgs);
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
        manager.relabel_mobile_sources("pane-1", &mut again);
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
        manager.relabel_mobile_sources("pane-1", &mut other);
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
        manager.relabel_mobile_sources("pane-1", &mut mixed);
        assert_eq!(mixed[0].source, "desktop");

        // 全部选中项都出自本端登记 → 改标并消费,再来一遍不再命中
        let mut mine = vec![marker(3, &["方案B"])];
        manager.relabel_mobile_sources("pane-1", &mut mine);
        assert_eq!(mine[0].source, "mobile");
        let mut again = vec![marker(4, &["方案B"])];
        manager.relabel_mobile_sources("pane-1", &mut again);
        assert_eq!(again[0].source, "desktop");

        // 打断标记(labels 为空)不参与对账
        let mut interrupted = vec![marker(5, &[])];
        manager.record_mobile_cmd("pane-1", "whatever");
        manager.relabel_mobile_sources("pane-1", &mut interrupted);
        assert_eq!(interrupted[0].source, "desktop");
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
}
