//! Seam 1:移动端指令路由测试。
//!
//! 指令转发、回执往返、桌面端离线即拒(路由层生成失败回执)、
//! 目标不存在的错误回执转发。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    CommandFailReason, DesktopToRelay, MobileToRelay, RelayToDesktop, RelayToMobile,
    PROTOCOL_VERSION,
};
use mt_relay_server::{app, RelayState};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn spawn_relay() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app(RelayState::new())).into_future());
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
