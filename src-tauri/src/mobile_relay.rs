//! 移动端中转体系:桌面端 → 中转服务器的出站 WebSocket 长连(docs/adr/0001)。
//!
//! 连接由 Rust 后端持有:握手校验协议版本,断线后指数退避自动重连;
//! 状态变化通过 `mobile-relay-status` 事件推给前端(设置页「移动端」区域展示)。
//! 版本不匹配时停止重连(重试无意义),等待用户升级。

use std::sync::Mutex as StdMutex;
use std::time::Duration;

use std::collections::{HashMap, HashSet};

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{DesktopToRelay, MobileProject, RelayToDesktop, PROTOCOL_VERSION};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

/// 握手 ack 等待超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 连接状态(serde camelCase 与前端 MobileRelayStatusPayload 对齐)。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MobileRelayStatusPayload {
    /// "disconnected" | "connecting" | "connected" | "reconnecting" | "versionMismatch"
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
    /// 活跃 AI 会话结构的最新快照(前端 store 经 command 喂入,后端据此组装增量)
    sessions: StdMutex<Vec<MobileProject>>,
}

impl MobileRelayManager {
    pub fn new() -> Self {
        Self {
            cancel: StdMutex::new(None),
            status: StdMutex::new(MobileRelayStatusPayload::simple("disconnected")),
            outbound: StdMutex::new(None),
            paired: StdMutex::new(None),
            sessions: StdMutex::new(Vec::new()),
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

    /// 接收前端 store 喂入的活跃 AI 会话全量状态:组装增量推给中转,存下新状态。
    pub fn update_sessions(&self, next: Vec<MobileProject>) {
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

    /// 发送当前全量快照(握手成功后 / 收到中转的快照请求时)。
    fn send_snapshot(&self) {
        let projects = self.sessions.lock().unwrap().clone();
        let _ = self.send(DesktopToRelay::SessionsSnapshot { projects });
    }

    /// 应用新的中转地址:先停旧连接;地址非空则启动新的重连循环。
    pub fn apply(&self, app: &AppHandle, relay_url: &str) {
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
        tauri::async_runtime::spawn(async move {
            connection_loop(app, url, cancel_rx).await;
        });
    }
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
    /// 用户取消(改地址/清空地址) → 停止循环,状态由调用方设置
    Cancelled,
}

async fn connection_loop(app: AppHandle, url: String, mut cancel_rx: watch::Receiver<bool>) {
    let manager = app.state::<MobileRelayManager>();
    let mut attempt: u32 = 0;
    loop {
        let status = if attempt == 0 { "connecting" } else { "reconnecting" };
        manager.set_status(&app, MobileRelayStatusPayload::simple(status));

        match connect_once(&app, &url, &mut cancel_rx).await {
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

/// 单次连接:建连 → hello → 等 ack → 已连接后挂住直到断线/取消。
async fn connect_once(
    app: &AppHandle,
    url: &str,
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
                expected_version,
                actual_version,
            }) => {
                return Attempt::VersionMismatch {
                    expected: expected_version,
                    actual: actual_version,
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
    manager.send_snapshot();

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
        Ok(RelayToDesktop::SessionsSnapshotRequest) => manager.send_snapshot(),
        Ok(_) => {}
        Err(_) => eprintln!("[mobile-relay] unparseable relay message (ignored)"),
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

/// 应用(或清除)中转地址。前端在保存设置时调用;空字符串 = 断开并停用。
#[tauri::command]
pub fn mobile_relay_apply(
    app: AppHandle,
    manager: tauri::State<'_, MobileRelayManager>,
    relay_url: String,
) -> Result<(), String> {
    manager.apply(&app, &relay_url);
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

/// 前端 store 喂入活跃 AI 会话全量状态(可见性规则由前端裁剪:仅 AI 会话 pane)。
#[tauri::command]
pub fn mobile_relay_update_sessions(
    manager: tauri::State<'_, MobileRelayManager>,
    projects: Vec<MobileProject>,
) {
    manager.update_sessions(projects);
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
        }
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
