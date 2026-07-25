//! Seam 1:移动端指令与发起会话的路由测试。
//!
//! 指令 / 发起会话的双向转发、回执往返、桌面端离线即拒(路由层生成失败回执)、
//! 目标不存在的错误回执转发。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    CommandFailReason, DesktopToRelay, MobileToRelay, RelayToDesktop, RelayToMobile,
    StartSessionFailReason, PROTOCOL_VERSION,
};
use mt_relay_server::{app, RelayState};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 中转与桌面端约定的共享密钥(v2 起桌面端握手必须携带)。
const DESKTOP_KEY: &str = "test-desktop-key";

async fn spawn_relay() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        axum::serve(
            listener,
            app(RelayState::new().with_desktop_key(Some(DESKTOP_KEY.into()))),
        )
        .into_future(),
    );
    addr
}

async fn connect(addr: SocketAddr, path: &str) -> WsClient {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}{path}"))
        .await
        .expect("ws connect failed");
    ws
}

async fn send_json<T: serde::Serialize>(ws: &mut WsClient, msg: &T) {
    ws.send(Message::Text(serde_json::to_string(msg).unwrap().into()))
        .await
        .unwrap();
}

async fn recv_json<T: serde::de::DeserializeOwned>(ws: &mut WsClient) -> Option<T> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for message")?;
        match frame {
            Ok(Message::Text(text)) => {
                return Some(serde_json::from_str(&text).expect("invalid message"))
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
}

async fn desktop_handshake(addr: SocketAddr) -> WsClient {
    let mut ws = connect(addr, "/ws/desktop").await;
    send_json(
        &mut ws,
        &DesktopToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop_key: DESKTOP_KEY.into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut ws).await,
        Some(RelayToDesktop::HelloAck { .. })
    ));
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut ws).await,
        Some(RelayToDesktop::PairingUpdate { .. })
    ));
    ws
}

/// 配对并建立移动端连接;桌面端消费掉配对/快照请求等副产帧。
async fn paired_mobile(addr: SocketAddr, desktop: &mut WsClient) -> WsClient {
    send_json(desktop, &DesktopToRelay::RequestPairingCode).await;
    let code = match recv_json::<RelayToDesktop>(desktop).await {
        Some(RelayToDesktop::PairingCode { code }) => code,
        other => panic!("expected pairingCode, got {other:?}"),
    };
    let mut mobile = connect(addr, "/ws/mobile").await;
    send_json(
        &mut mobile,
        &MobileToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: Some(code),
            credential: None,
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::HelloAck { .. })
    ));
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence { .. })
    ));
    assert!(matches!(
        recv_json::<RelayToDesktop>(desktop).await,
        Some(RelayToDesktop::PairingUpdate { paired: true })
    ));
    assert!(matches!(
        recv_json::<RelayToDesktop>(desktop).await,
        Some(RelayToDesktop::SessionsSnapshotRequest)
    ));
    mobile
}

#[tokio::test]
async fn command_routes_to_desktop_and_receipt_returns() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_handshake(addr).await;
    let mut mobile = paired_mobile(addr, &mut desktop).await;

    // 指令 → 桌面端
    send_json(
        &mut mobile,
        &MobileToRelay::MobileCommand {
            pane_id: "pane-1".into(),
            command_id: "cmd-1".into(),
            text: "npm test".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::MobileCommand {
            pane_id: "pane-1".into(),
            command_id: "cmd-1".into(),
            text: "npm test".into(),
        })
    );

    // "已写入"回执 → 移动端
    send_json(
        &mut desktop,
        &DesktopToRelay::CommandReceipt {
            pane_id: "pane-1".into(),
            command_id: "cmd-1".into(),
            ok: true,
            reason: None,
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::CommandReceipt {
            pane_id: "pane-1".into(),
            command_id: "cmd-1".into(),
            ok: true,
            reason: None,
        })
    );
}

#[tokio::test]
async fn desktop_offline_rejects_command_immediately() {
    let addr = spawn_relay().await;
    // 先配对(需要桌面端在线),然后桌面端下线
    let mut desktop = desktop_handshake(addr).await;
    let mut mobile = paired_mobile(addr, &mut desktop).await;
    desktop.close(None).await.unwrap();
    drop(desktop);
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: false
        })
    );

    // 离线即拒:中转直接回失败回执,不存储转发
    send_json(
        &mut mobile,
        &MobileToRelay::MobileCommand {
            pane_id: "pane-1".into(),
            command_id: "cmd-9".into(),
            text: "lost command".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::CommandReceipt {
            pane_id: "pane-1".into(),
            command_id: "cmd-9".into(),
            ok: false,
            reason: Some(CommandFailReason::DesktopOffline),
        })
    );
}

#[tokio::test]
async fn target_missing_failure_receipt_is_routed() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_handshake(addr).await;
    let mut mobile = paired_mobile(addr, &mut desktop).await;

    send_json(
        &mut mobile,
        &MobileToRelay::MobileCommand {
            pane_id: "gone-pane".into(),
            command_id: "cmd-2".into(),
            text: "hello?".into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::MobileCommand { .. })
    ));

    // 桌面端发现目标不存在 → 失败回执原样送达移动端
    send_json(
        &mut desktop,
        &DesktopToRelay::CommandReceipt {
            pane_id: "gone-pane".into(),
            command_id: "cmd-2".into(),
            ok: false,
            reason: Some(CommandFailReason::PaneNotFound),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::CommandReceipt {
            pane_id: "gone-pane".into(),
            command_id: "cmd-2".into(),
            ok: false,
            reason: Some(CommandFailReason::PaneNotFound),
        })
    );
}

#[tokio::test]
async fn start_ai_session_routes_to_desktop_and_receipt_returns() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_handshake(addr).await;
    let mut mobile = paired_mobile(addr, &mut desktop).await;

    // 发起请求 → 桌面端(原样转发,中转不解析 launcherId 的含义)
    send_json(
        &mut mobile,
        &MobileToRelay::StartAiSession {
            request_id: "req-1".into(),
            project_id: "p1".into(),
            launcher_id: "l1".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::StartAiSession {
            request_id: "req-1".into(),
            project_id: "p1".into(),
            launcher_id: "l1".into(),
        })
    );

    // "pane 已建、命令已写入"回执 → 移动端
    send_json(
        &mut desktop,
        &DesktopToRelay::StartSessionReceipt {
            request_id: "req-1".into(),
            ok: true,
            pane_id: Some("pane-7".into()),
            reason: None,
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::StartSessionReceipt {
            request_id: "req-1".into(),
            ok: true,
            pane_id: Some("pane-7".into()),
            reason: None,
        })
    );
}

#[tokio::test]
async fn start_ai_session_failure_receipt_is_routed() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_handshake(addr).await;
    let mut mobile = paired_mobile(addr, &mut desktop).await;

    send_json(
        &mut mobile,
        &MobileToRelay::StartAiSession {
            request_id: "req-2".into(),
            project_id: "p1".into(),
            launcher_id: "deleted-launcher".into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::StartAiSession { .. })
    ));

    send_json(
        &mut desktop,
        &DesktopToRelay::StartSessionReceipt {
            request_id: "req-2".into(),
            ok: false,
            pane_id: None,
            reason: Some(StartSessionFailReason::LauncherNotFound),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::StartSessionReceipt {
            request_id: "req-2".into(),
            ok: false,
            pane_id: None,
            reason: Some(StartSessionFailReason::LauncherNotFound),
        })
    );
}

#[tokio::test]
async fn desktop_offline_rejects_start_ai_session_at_router() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_handshake(addr).await;
    let mut mobile = paired_mobile(addr, &mut desktop).await;
    desktop.close(None).await.unwrap();
    drop(desktop);
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: false
        })
    );

    // 离线即拒:不做存储转发,手机立刻拿到明确原因而不是一直转圈
    send_json(
        &mut mobile,
        &MobileToRelay::StartAiSession {
            request_id: "req-9".into(),
            project_id: "p1".into(),
            launcher_id: "l1".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::StartSessionReceipt {
            request_id: "req-9".into(),
            ok: false,
            pane_id: None,
            reason: Some(StartSessionFailReason::DesktopOffline),
        })
    );
}
