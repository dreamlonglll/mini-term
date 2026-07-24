//! 移动端中转体系:桌面端 → 中转服务器的出站 WebSocket 长连(docs/adr/0001)。
//!
//! 连接由 Rust 后端持有:握手校验协议版本,断线后指数退避自动重连;
//! 状态变化通过 `mobile-relay-status` 事件推给前端(设置页「移动端」区域展示)。
//! 版本不匹配时停止重连(重试无意义),等待用户升级。

use std::sync::Mutex as StdMutex;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{DesktopToRelay, RelayToDesktop, PROTOCOL_VERSION};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::watch;
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
}

impl MobileRelayStatusPayload {
    fn simple(status: &str) -> Self {
        Self {
            status: status.into(),
            expected_version: None,
            actual_version: None,
        }
    }
}

/// 当前连接会话的取消句柄;整个 manager 由 Tauri 全局托管。
pub struct MobileRelayManager {
    cancel: StdMutex<Option<watch::Sender<bool>>>,
    status: StdMutex<MobileRelayStatusPayload>,
}

impl MobileRelayManager {
    pub fn new() -> Self {
        Self {
            cancel: StdMutex::new(None),
            status: StdMutex::new(MobileRelayStatusPayload::simple("disconnected")),
        }
    }

    fn set_status(&self, app: &AppHandle, payload: MobileRelayStatusPayload) {
        *self.status.lock().unwrap() = payload.clone();
        let _ = app.emit("mobile-relay-status", payload);
    }

    pub fn current_status(&self) -> MobileRelayStatusPayload {
        self.status.lock().unwrap().clone()
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
            Err(_) => return Attempt::Failed,
        },
        _ => return Attempt::Failed,
    }

    let manager = app.state::<MobileRelayManager>();
    manager.set_status(app, MobileRelayStatusPayload::simple("connected"));

    // 已连接:挂住读循环直到断线/取消(业务消息由后续 ticket 在此路由)
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Close(_))) | None => return Attempt::ConnectedThenLost,
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    eprintln!("[mobile-relay] connection lost: {e}");
                    return Attempt::ConnectedThenLost;
                }
            },
            _ = cancel_rx.changed() => {
                let _ = ws.close(None).await;
                return Attempt::Cancelled;
            }
        }
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

    #[test]
    fn status_payload_serializes_camel_case() {
        let payload = MobileRelayStatusPayload {
            status: "versionMismatch".into(),
            expected_version: Some(1),
            actual_version: Some(2),
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
