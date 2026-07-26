//! 移动端中转体系:桌面端 → 中转服务器的出站 WebSocket 长连(docs/adr/0001)。
//!
//! 连接由 Rust 后端持有:握手校验协议版本,断线后指数退避自动重连;
//! 状态变化通过 `mobile-relay-status` 事件推给前端(设置页「移动端」区域展示)。
//! 版本不匹配时停止重连(重试无意义),等待用户升级。

use std::sync::Mutex as StdMutex;
use std::time::Duration;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    DesktopRejectReason, DesktopToRelay, MirrorMessage, MobileLauncher, MobilePane, MobileProject,
    RelayToDesktop, StartSessionFailReason, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::config::AiLauncher;
use crate::mobile_mirror::{self, history_slice, MirrorParser, MIRROR_PAGE_SIZE};

/// 握手 ack 等待超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 镜像会话文件轮询间隔。
const MIRROR_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 前端 store 喂入的同步载荷:比 wire 类型多项目路径(镜像绑定用,不发给移动端)。
///
/// v2 起前端上报 `config.projects` **全集**(不再只报有活跃 AI 会话的项目),
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
}

/// 一个被订阅 pane 的镜像状态:取消句柄 + 已解析消息(分页取数用)。
struct MirrorSub {
    cancel_tx: watch::Sender<bool>,
    messages: Arc<StdMutex<Vec<MirrorMessage>>>,
}

/// 连接状态(serde camelCase 与前端 MobileRelayStatusPayload 对齐)。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MobileRelayStatusPayload {
    /// "disconnected" | "connecting" | "connected" | "reconnecting" | "versionMismatch"
    /// | "authFailed"(密钥不匹配)| "keyNotConfigured"(中转未配置密钥)。
    /// 后三者都是配置问题:停止重连,等用户改配置。
    pub status: String,
    /// versionMismatch 时携带,供前端给出明确升级提示
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

/// 当前连接会话的取消句柄;整个 manager 由 Tauri 全局托管。
pub struct MobileRelayManager {
    cancel: StdMutex<Option<watch::Sender<bool>>>,
    status: StdMutex<MobileRelayStatusPayload>,
    /// 已连接会话的出站消息通道(请求配对码/重置配对经此送往中转)
    outbound: StdMutex<Option<mpsc::UnboundedSender<DesktopToRelay>>>,
    /// 最近一次 PairingUpdate 的配对状态(断线清空)
    paired: StdMutex<Option<bool>>,
    /// 项目结构的最新快照(前端 store 经 command 喂入,后端据此组装增量)
    sessions: StdMutex<Vec<MobileProject>>,
    /// pane → 项目路径(镜像订阅时解析会话文件用)
    pane_paths: StdMutex<HashMap<String, String>>,
    /// pane → PTY id(移动端指令写穿目标)
    pane_ptys: StdMutex<HashMap<String, u32>>,
    /// 已订阅镜像的 pane 集合
    mirror_subs: StdMutex<HashMap<String, MirrorSub>>,
    /// 每 pane 最近成功写入的移动端指令原文:会话记录回流时把匹配的
    /// user 消息来源改标为 "mobile"(见 relabel_mobile_sources)
    recent_mobile_cmds: StdMutex<HashMap<String, Vec<String>>>,
}

impl MobileRelayManager {
    pub fn new() -> Self {
        Self {
            cancel: StdMutex::new(None),
            status: StdMutex::new(MobileRelayStatusPayload::simple("disconnected")),
            outbound: StdMutex::new(None),
            paired: StdMutex::new(None),
            sessions: StdMutex::new(Vec::new()),
            pane_paths: StdMutex::new(HashMap::new()),
            pane_ptys: StdMutex::new(HashMap::new()),
            mirror_subs: StdMutex::new(HashMap::new()),
            recent_mobile_cmds: StdMutex::new(HashMap::new()),
        }
    }

    fn set_status(&self, app: &AppHandle, mut payload: MobileRelayStatusPayload) {
        // 断开/重连中时配对状态不可知,清空避免陈旧值误导 UI
        if payload.status != "connected" {
            *self.paired.lock().unwrap() = None;
        }
        payload.paired = *self.paired.lock().unwrap();
        *self.status.lock().unwrap() = payload.clone();
        let _ = app.emit("mobile-relay-status", payload);
    }

    /// 中转推送 PairingUpdate 时更新配对状态并重发 status 事件。
    fn set_paired(&self, app: &AppHandle, paired: bool) {
        *self.paired.lock().unwrap() = Some(paired);
        let mut payload = self.status.lock().unwrap().clone();
        payload.paired = Some(paired);
        *self.status.lock().unwrap() = payload.clone();
        let _ = app.emit("mobile-relay-status", payload);
    }

    pub fn current_status(&self) -> MobileRelayStatusPayload {
        self.status.lock().unwrap().clone()
    }

    /// 向中转发送消息(仅已连接时可用)。
    fn send(&self, msg: DesktopToRelay) -> Result<(), String> {
        let outbound = self.outbound.lock().unwrap();
        match outbound.as_ref() {
            Some(tx) => tx.send(msg).map_err(|_| "connection closing".into()),
            None => Err("not connected to relay".into()),
        }
    }

    /// 接收前端 store 喂入的项目全量状态:组装增量推给中转,存下新状态,
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
        *self.pane_ptys.lock().unwrap() = pane_ptys;

        // 订阅中的 pane 已不在活跃集合(pane 关闭/AI 会话结束)→ PaneClosed
        let gone: Vec<String> = {
            let subs = self.mirror_subs.lock().unwrap();
            subs.keys()
                .filter(|id| !pane_paths.contains_key(*id))
                .cloned()
                .collect()
        };
        for pane_id in gone {
            self.unsubscribe_pane(&pane_id);
            let _ = self.send(DesktopToRelay::PaneClosed { pane_id });
        }
        *self.pane_paths.lock().unwrap() = pane_paths;

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
                    })
                    .collect(),
            })
            .collect();

        let delta = {
            let mut sessions = self.sessions.lock().unwrap();
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

    /// 撤销单个镜像订阅(幂等)。
    fn unsubscribe_pane(&self, pane_id: &str) {
        if let Some(sub) = self.mirror_subs.lock().unwrap().remove(pane_id) {
            let _ = sub.cancel_tx.send(true);
        }
    }

    /// 撤销全部镜像订阅(与中转断线时调用;移动端重连后会重新订阅)。
    fn clear_mirror_subs(&self) {
        let subs: Vec<MirrorSub> = self.mirror_subs.lock().unwrap().drain().map(|(_, s)| s).collect();
        for sub in subs {
            let _ = sub.cancel_tx.send(true);
        }
    }

    /// 登记一条已写入 PTY 的移动端指令原文(镜像回流改标来源用,每 pane 上限 20 条)。
    fn record_mobile_cmd(&self, pane_id: &str, text: &str) {
        let mut map = self.recent_mobile_cmds.lock().unwrap();
        let list = map.entry(pane_id.to_string()).or_default();
        list.push(text.trim().to_string());
        if list.len() > 20 {
            list.remove(0);
        }
    }

    /// 镜像新消息回流时调用:与最近移动端指令逐字匹配的 user 消息改标 "mobile"。
    /// 不匹配保持 "desktop"(误差方向安全:最多把移动端指令标成桌面输入)。
    fn relabel_mobile_sources(&self, pane_id: &str, messages: &mut [MirrorMessage]) {
        let mut map = self.recent_mobile_cmds.lock().unwrap();
        let Some(list) = map.get_mut(pane_id) else {
            return;
        };
        for msg in messages.iter_mut() {
            if msg.source != "desktop" {
                continue;
            }
            if let Some(pos) = list.iter().position(|cmd| cmd == msg.content.trim()) {
                msg.source = "mobile".into();
                list.remove(pos);
            }
        }
    }

    /// 分页取数:从订阅的消息缓存里取 seq < before_seq 的最近一页并回发。
    fn send_mirror_history(&self, pane_id: &str, before_seq: u64) {
        let slice = {
            let subs = self.mirror_subs.lock().unwrap();
            let Some(sub) = subs.get(pane_id) else { return };
            let messages = sub.messages.lock().unwrap();
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
    /// 启动器名单从磁盘配置现取:它是低频数据,没必要在内存里再维护一份副本。
    fn send_snapshot(&self, app: &AppHandle) {
        let projects = self.sessions.lock().unwrap().clone();
        let launchers = launchers_of(app)
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
    pub fn apply(&self, app: &AppHandle, relay_url: &str, desktop_key: &str) {
        if let Some(tx) = self.cancel.lock().unwrap().take() {
            let _ = tx.send(true);
        }
        let url = match normalize_relay_url(relay_url) {
            Some(u) => u,
            None => {
                self.set_status(app, MobileRelayStatusPayload::simple("disconnected"));
                return;
            }
        };

        // WSS 需要 rustls CryptoProvider;显式装 ring 后端(依赖树只编译了 ring)。
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (cancel_tx, cancel_rx) = watch::channel(false);
        *self.cancel.lock().unwrap() = Some(cancel_tx);
        let app = app.clone();
        let desktop_key = desktop_key.to_string();
        tauri::async_runtime::spawn(async move {
            connection_loop(app, url, desktop_key, cancel_rx).await;
        });
    }
}

/// 能否在该项目远程发起 AI 会话。
///
/// SSH 远程项目与 WSL 根项目一律为否:它们的对话镜像目前一定是空的
/// (`mobile_mirror` 只认本机 Windows 宿主来源),在那儿开会话等于盲发指令。
/// WSL **关联**项目(根路径是普通 Windows 路径)不在此列——它的镜像可用与否取决于
/// 启动器把 AI 起在哪一侧,那是既有的 v1 镜像限制,不由本判定兜底。
pub fn can_start_session(path: &str, ssh_connection_id: Option<&str>) -> bool {
    ssh_connection_id.is_none() && mt_core::parse_wsl_unc(path).is_none()
}

/// 读取当前配置里的 AI 启动器名单(配置整块缺失时退回预置两条)。
fn launchers_of(app: &AppHandle) -> Vec<AiLauncher> {
    crate::config::read_config(app)
        .mobile_relay
        .unwrap_or_default()
        .launchers
}

impl Default for MobileRelayManager {
    fn default() -> Self {
        Self::new()
    }
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
    app: AppHandle,
    url: String,
    desktop_key: String,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let manager = app.state::<MobileRelayManager>();
    let mut attempt: u32 = 0;
    loop {
        let status = if attempt == 0 { "connecting" } else { "reconnecting" };
        manager.set_status(&app, MobileRelayStatusPayload::simple(status));

        match connect_once(&app, &url, &desktop_key, &mut cancel_rx).await {
            Attempt::Cancelled => return,
            Attempt::VersionMismatch { expected, actual } => {
                manager.set_status(
                    &app,
                    MobileRelayStatusPayload {
                        status: "versionMismatch".into(),
                        expected_version: Some(expected),
                        actual_version: Some(actual),
                        paired: None,
                    },
                );
                return;
            }
            // 配置问题不是网络问题:停在明确状态上,等用户改配置后重新「保存并连接」
            Attempt::Rejected(reason) => {
                manager.set_status(
                    &app,
                    MobileRelayStatusPayload::simple(reject_status(reason)),
                );
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

/// 握手拒绝原因 → 前端状态串(两种密钥问题的修法不同,文案也不同)。
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
    app: &AppHandle,
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

    let manager = app.state::<MobileRelayManager>();
    manager.set_status(app, MobileRelayStatusPayload::simple("connected"));

    // 注册出站通道(配对码请求/重置配对/结构快照经此发送)
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<DesktopToRelay>();
    *manager.outbound.lock().unwrap() = Some(outbound_tx);

    // 连上即推一份全量快照:覆盖"桌面端重连时移动端已在线"的场景
    manager.send_snapshot(app);

    // 已连接:读循环 + 出站转发,直到断线/取消
    let outcome = loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => handle_relay_message(app, &manager, &text),
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
    *manager.outbound.lock().unwrap() = None;
    // 断线后镜像推送无处可去,撤销全部订阅;移动端重连会重新订阅
    manager.clear_mirror_subs();
    outcome
}

/// 处理中转推来的消息(已握手连接上)。
fn handle_relay_message(app: &AppHandle, manager: &MobileRelayManager, text: &str) {
    match serde_json::from_str::<RelayToDesktop>(text) {
        Ok(RelayToDesktop::PairingCode { code }) => {
            let _ = app.emit("mobile-relay-pairing-code", PairingCodePayload { code });
        }
        Ok(RelayToDesktop::PairingUpdate { paired }) => {
            manager.set_paired(app, paired);
        }
        // 移动端上线,回发最新结构快照(中转不缓存)
        Ok(RelayToDesktop::SessionsSnapshotRequest) => manager.send_snapshot(app),
        // 对话镜像:订阅/退订/分页
        Ok(RelayToDesktop::SubscribePane { pane_id }) => subscribe_pane(app, manager, pane_id),
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
        }) => handle_mobile_command(app, manager, pane_id, command_id, text),
        // 移动端重命名会话:pane 标题归前端布局状态所有,后端只做长度收敛后转交
        Ok(RelayToDesktop::RenamePane { pane_id, title }) => {
            let _ = app.emit(
                "mobile-rename-pane",
                RenamePanePayload {
                    pane_id,
                    title: sanitize_pane_title(&title),
                },
            );
        }
        // 移动端发起新 AI 会话:后端校验后交给前端建 tab(PTY 与布局都归前端管)
        Ok(RelayToDesktop::StartAiSession {
            request_id,
            project_id,
            launcher_id,
        }) => handle_start_ai_session(app, manager, request_id, project_id, launcher_id),
        Ok(_) => {}
        Err(_) => eprintln!("[mobile-relay] unparseable relay message (ignored)"),
    }
}

/// pane 自定义标题的字符数上限。桌面端 tab 栏是一行横排,超长标题会把同组其它
/// tab 挤出可视区;截断而不是拒绝——用户改的名字过长是手滑,不该整条改名失败。
const MAX_PANE_TITLE_CHARS: usize = 64;

/// 收敛移动端传来的标题:去首尾空白、砍掉控制字符、限长。
/// 空串是合法输入(= 清除自定义名),原样返回给前端处理。
fn sanitize_pane_title(title: &str) -> String {
    title
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PANE_TITLE_CHARS)
        .collect()
}

/// `mobile-rename-pane` 事件载荷:交给前端改布局里那个 pane 的 customTitle。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamePanePayload {
    pub pane_id: String,
    /// 已收敛过的标题;空串 = 清除自定义名,回落 shell 名
    pub title: String,
}

/// `mobile-start-session` 事件载荷:后端校验通过后交给前端执行的启动指令。
/// 命令与 shell 只在桌面端进程内流转,不回传中转。
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

/// 校验移动端的发起请求,通过则 emit 给前端执行,不通过直接回失败回执。
///
/// 校验的是"目标存在且支持",**不**校验命令内容——命令来自桌面端配置,
/// 这是 ADR 0002 的防线本身,再做内容白名单也挡不住拼接。
fn handle_start_ai_session(
    app: &AppHandle,
    manager: &MobileRelayManager,
    request_id: String,
    project_id: String,
    launcher_id: String,
) {
    let config = crate::config::read_config(app);
    let Some(project) = config.projects.iter().find(|p| p.id == project_id) else {
        manager.send_start_receipt(request_id, None, Some(StartSessionFailReason::ProjectNotFound));
        return;
    };
    if !can_start_session(&project.path, project.ssh_connection_id.as_deref()) {
        manager.send_start_receipt(request_id, None, Some(StartSessionFailReason::NotSupported));
        return;
    }
    let launchers = config.mobile_relay.unwrap_or_default().launchers;
    let launcher = match resolve_launcher(&launchers, &launcher_id) {
        Ok(l) => l,
        Err(reason) => {
            manager.send_start_receipt(request_id, None, Some(reason));
            return;
        }
    };

    let _ = app.emit(
        "mobile-start-session",
        StartSessionPayload {
            request_id,
            project_id,
            launcher_id,
            launcher_name: launcher.name,
            shell_name: launcher.shell,
            command: launcher.command,
        },
    );
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
    app: &AppHandle,
    manager: &MobileRelayManager,
    pane_id: String,
    command_id: String,
    text: String,
) {
    use mt_relay_protocol::CommandFailReason;

    let pty_id = manager.pane_ptys.lock().unwrap().get(&pane_id).copied();
    let result = match pty_id {
        None => Err(CommandFailReason::PaneNotFound),
        Some(pty_id) => {
            // 复用 write_pty 全语义(输入跟踪/AI marker/SSH autofill 解除),
            // 文本 + \r 一次写入 = 敲入内容并回车;AI 工作中依赖 CLI 自身输入缓冲
            let data = format!("{text}\r");
            crate::pty::write_pty(app.clone(), app.state(), pty_id, data, None)
                .map_err(|_| CommandFailReason::WriteFailed)
        }
    };

    match result {
        Ok(()) => {
            manager.record_mobile_cmd(&pane_id, &text);
            let _ = manager.send(DesktopToRelay::CommandReceipt {
                pane_id,
                command_id,
                ok: true,
                reason: None,
            });
        }
        Err(reason) => {
            let _ = manager.send(DesktopToRelay::CommandReceipt {
                pane_id,
                command_id,
                ok: false,
                reason: Some(reason),
            });
        }
    }
}

/// 建立镜像订阅:绑定 pane 所属项目的最新会话文件,启动轮询任务。
/// 重复订阅先撤旧再建新(移动端重连后重订阅拿到新快照)。
fn subscribe_pane(app: &AppHandle, manager: &MobileRelayManager, pane_id: String) {
    manager.unsubscribe_pane(&pane_id);
    let Some(project_path) = manager.pane_paths.lock().unwrap().get(&pane_id).cloned() else {
        // pane 已不存在(或从未同步):直接告知已关闭
        let _ = manager.send(DesktopToRelay::PaneClosed { pane_id });
        return;
    };

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let messages = Arc::new(StdMutex::new(Vec::new()));
    manager.mirror_subs.lock().unwrap().insert(
        pane_id.clone(),
        MirrorSub {
            cancel_tx,
            messages: messages.clone(),
        },
    );
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        mirror_task(app, pane_id, project_path, messages, cancel_rx).await;
    });
}

/// 镜像轮询任务:解析 pane 应镜像的最新会话文件,增量读取新行推送;
/// 出现更新的会话文件时重新绑定并重发快照。
async fn mirror_task(
    app: AppHandle,
    pane_id: String,
    project_path: String,
    messages: Arc<StdMutex<Vec<MirrorMessage>>>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let manager = app.state::<MobileRelayManager>();
    let pty_manager = app.state::<crate::pty::PtyManager>();
    let hook_state = app.state::<crate::hook_server::HookState>();
    let mut bound: Option<(PathBuf, MirrorParser, u64)> = None;
    let mut sent_initial = false;

    loop {
        // 绑定分两层,每轮重取(PTY 映射、hook 上报都可能后到):
        // 1. hook 上报过会话身份 → 只认该会话的文件,未落盘就等(空镜像),
        //    不退启发式——退了就会串到同项目其他 pane 的会话;
        // 2. 无会话身份(未启用 hook)→ 退回"项目最新文件 + AI 启动时刻下限"启发式。
        let pty_id = manager.pane_ptys.lock().unwrap().get(&pane_id).copied();
        let resolved = match pty_id.and_then(|id| hook_state.session_of(id)) {
            Some(s) => mobile_mirror::resolve_session_file_by_id(
                &project_path,
                s.agent.as_deref(),
                &s.session_id,
            ),
            None => {
                let ai_started = pty_id.and_then(|id| pty_manager.ai_session_started_at(id));
                mobile_mirror::resolve_session_file(&project_path, ai_started)
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
                let rebind = bound.as_ref().is_none_or(|(p, _, _)| *p != path);
                if rebind {
                    // 首次绑定或换绑到更新的会话文件:全量解析 + 重发快照
                    let mut parser = MirrorParser::new(agent);
                    if let Some((bytes, offset)) = mobile_mirror::read_from_offset(&path, 0) {
                        let msgs = parser.feed(&bytes);
                        *messages.lock().unwrap() = msgs;
                        bound = Some((path, parser, offset));
                        sent_initial = true;
                        let (page, has_more) = {
                            let m = messages.lock().unwrap();
                            history_slice(&m, None, MIRROR_PAGE_SIZE)
                        };
                        let _ = manager.send(DesktopToRelay::MirrorSnapshot {
                            pane_id: pane_id.clone(),
                            messages: page,
                            has_more,
                        });
                    }
                } else if let Some((bpath, parser, offset)) = bound.as_mut() {
                    match mobile_mirror::read_from_offset(bpath, *offset) {
                        Some((bytes, new_offset)) => {
                            *offset = new_offset;
                            if !bytes.is_empty() {
                                let mut new_msgs = parser.feed(&bytes);
                                if !new_msgs.is_empty() {
                                    // 移动端指令回流:匹配的 user 消息改标 "mobile"
                                    manager.relabel_mobile_sources(&pane_id, &mut new_msgs);
                                    messages.lock().unwrap().extend(new_msgs.clone());
                                    let _ = manager.send(DesktopToRelay::MirrorAppend {
                                        pane_id: pane_id.clone(),
                                        messages: new_msgs,
                                    });
                                }
                            }
                        }
                        // 文件被截断/重写:下一轮重新绑定
                        None => bound = None,
                    }
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

/// mobile-relay-pairing-code 事件载荷。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingCodePayload {
    pub code: String,
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

/// 应用(或清除)中转地址与桌面端密钥。前端在保存设置时调用;
/// 地址为空字符串 = 断开并停用。
#[tauri::command]
pub fn mobile_relay_apply(
    app: AppHandle,
    manager: tauri::State<'_, MobileRelayManager>,
    relay_url: String,
    desktop_key: String,
) -> Result<(), String> {
    manager.apply(&app, &relay_url, &desktop_key);
    Ok(())
}

/// 查询当前连接状态(前端打开设置页时取初始值,后续靠事件增量更新)。
#[tauri::command]
pub fn mobile_relay_status(
    manager: tauri::State<'_, MobileRelayManager>,
) -> MobileRelayStatusPayload {
    manager.current_status()
}

/// 请求中转签发一次性配对码;结果经 mobile-relay-pairing-code 事件推回。
#[tauri::command]
pub fn mobile_relay_request_pairing_code(
    manager: tauri::State<'_, MobileRelayManager>,
) -> Result<(), String> {
    manager.send(DesktopToRelay::RequestPairingCode)
}

/// 重置配对:吊销移动端全部凭证;结果经 mobile-relay-status 的 paired 字段推回。
#[tauri::command]
pub fn mobile_relay_reset_pairing(
    manager: tauri::State<'_, MobileRelayManager>,
) -> Result<(), String> {
    manager.send(DesktopToRelay::ResetPairing)
}

/// 前端 store 喂入项目全量状态(项目全集;pane 侧仍按"仅 AI 会话 pane"裁剪)。
#[tauri::command]
pub fn mobile_relay_update_sessions(
    manager: tauri::State<'_, MobileRelayManager>,
    projects: Vec<SyncProject>,
) {
    manager.update_sessions(projects);
}

/// 启动器配置变化后重发一次全量快照(不为启动器单开增量消息)。
#[tauri::command]
pub fn mobile_relay_launchers_changed(
    app: AppHandle,
    manager: tauri::State<'_, MobileRelayManager>,
) {
    manager.send_snapshot(&app);
}

/// 前端执行完发起流程后回执:ok = pane 已建且启动命令已写入 PTY。
/// `reason` 仅失败时携带,取值对齐 `StartSessionFailReason` 的 camelCase 串。
#[tauri::command]
pub fn mobile_relay_start_session_result(
    manager: tauri::State<'_, MobileRelayManager>,
    request_id: String,
    ok: bool,
    pane_id: Option<String>,
    reason: Option<StartSessionFailReason>,
) {
    manager.send_start_receipt(
        request_id,
        if ok { pane_id } else { None },
        if ok { None } else { reason.or(Some(StartSessionFailReason::SpawnFailed)) },
    );
}

/// 校验一条启动命令能否被识别为 AI 会话(「移动端」面板保存启动器时的非阻塞提示)。
///
/// 这只是把失败从"手机上等 15 秒超时"前移到配置时,**不是安全防线**:
/// 防线是"命令只能来自桌面端配置"(见 ADR 0002)。
#[tauri::command]
pub fn mobile_relay_check_launcher_command(command: String) -> bool {
    crate::pty::is_interactive_ai_command(command.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(sanitize_pane_title(&long).chars().count(), MAX_PANE_TITLE_CHARS);
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
        // 前端按 camelCase 读该事件载荷
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

        // 未绑定 shell → None(前端据此用默认 shell)
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
        assert!(mobile_relay_check_launcher_command("claude".into()));
        assert!(mobile_relay_check_launcher_command("  codex  ".into()));
        assert!(mobile_relay_check_launcher_command(
            "claude --dangerously-skip-permissions".into()
        ));
        // 非 AI CLI / 非交互标志:提示会被识别不了
        assert!(!mobile_relay_check_launcher_command("npm test".into()));
        assert!(!mobile_relay_check_launcher_command("claude -p 'hi'".into()));
        assert!(!mobile_relay_check_launcher_command("codex --version".into()));
        assert!(!mobile_relay_check_launcher_command(String::new()));
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
        let manager = MobileRelayManager::new();
        manager.record_mobile_cmd("pane-1", "  npm test ");

        let mut msgs = vec![
            MirrorMessage {
                seq: 0,
                source: "desktop".into(),
                content: "unrelated input".into(),
                timestamp: String::new(),
            },
            MirrorMessage {
                seq: 1,
                source: "desktop".into(),
                content: "npm test".into(),
                timestamp: String::new(),
            },
            MirrorMessage {
                seq: 2,
                source: "assistant".into(),
                content: "npm test".into(),
                timestamp: String::new(),
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
        }];
        manager.relabel_mobile_sources("pane-1", &mut other);
        assert_eq!(other[0].source, "desktop");
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
