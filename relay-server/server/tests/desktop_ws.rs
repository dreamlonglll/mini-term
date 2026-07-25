//! Seam 1:中转协议边界测试(桌面端一侧)。
//!
//! 进程内启动真实中转实例,用 tokio-tungstenite 模拟桌面端客户端,
//! 从 WebSocket 边界驱动真实协议帧;不触碰中转内部模块。
//! 覆盖 v2 的桌面端鉴权:密钥正确 / 错误 / 缺失 / 中转未配置四种握手结局。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{DesktopRejectReason, DesktopToRelay, RelayToDesktop, PROTOCOL_VERSION};
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

/// 进程内启动中转(已配置桌面端密钥),返回监听地址。
async fn spawn_relay() -> SocketAddr {
    spawn_relay_with_key(Some(DESKTOP_KEY.into())).await
}

/// 进程内启动中转,自定义中转侧的密钥配置(None = 未配置,fail-closed)。
async fn spawn_relay_with_key(key: Option<String>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        axum::serve(listener, app(RelayState::new().with_desktop_key(key))).into_future(),
    );
    addr
}

async fn connect_desktop(addr: SocketAddr) -> WsClient {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/desktop"))
        .await
        .expect("desktop ws connect failed");
    ws
}

async fn send_hello(ws: &mut WsClient, version: u32) {
    send_hello_with_key(ws, version, DESKTOP_KEY).await;
}

async fn send_hello_with_key(ws: &mut WsClient, version: u32, key: &str) {
    let hello = DesktopToRelay::Hello {
        protocol_version: version,
        desktop_key: key.into(),
    };
    ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();
}

/// 读下一条文本帧并解析为 RelayToDesktop;None = 连接被关闭。
async fn recv_msg(ws: &mut WsClient) -> Option<RelayToDesktop> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for relay message")?;
        match frame {
            Ok(Message::Text(text)) => {
                return Some(serde_json::from_str(&text).expect("invalid relay message"))
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue, // ping/pong 等控制帧
        }
    }
}

#[tokio::test]
async fn handshake_success_returns_ack_then_pairing_state() {
    let addr = spawn_relay().await;
    let mut ws = connect_desktop(addr).await;
    send_hello(&mut ws, PROTOCOL_VERSION).await;

    let ack = recv_msg(&mut ws).await.expect("expected helloAck");
    assert_eq!(
        ack,
        RelayToDesktop::HelloAck {
            protocol_version: PROTOCOL_VERSION
        }
    );
    // 握手成功后立即收到当前配对状态(未配对)
    let update = recv_msg(&mut ws).await.expect("expected pairingUpdate");
    assert_eq!(update, RelayToDesktop::PairingUpdate { paired: false });
}

#[tokio::test]
async fn version_mismatch_is_rejected_with_versions_then_closed() {
    let addr = spawn_relay().await;
    let mut ws = connect_desktop(addr).await;
    send_hello(&mut ws, 999).await;

    let reject = recv_msg(&mut ws).await.expect("expected helloReject");
    assert_eq!(
        reject,
        RelayToDesktop::HelloReject {
            reason: DesktopRejectReason::VersionMismatch,
            expected_version: Some(PROTOCOL_VERSION),
            actual_version: Some(999),
        }
    );
    // 拒绝后连接必须被关闭
    assert!(recv_msg(&mut ws).await.is_none(), "connection should close after reject");
}

#[tokio::test]
async fn correct_desktop_key_is_accepted() {
    let addr = spawn_relay().await;
    let mut ws = connect_desktop(addr).await;
    send_hello_with_key(&mut ws, PROTOCOL_VERSION, DESKTOP_KEY).await;

    assert!(matches!(
        recv_msg(&mut ws).await,
        Some(RelayToDesktop::HelloAck { .. })
    ));
}

#[tokio::test]
async fn wrong_desktop_key_is_rejected_then_closed() {
    let addr = spawn_relay().await;
    let mut ws = connect_desktop(addr).await;
    send_hello_with_key(&mut ws, PROTOCOL_VERSION, "not-the-key").await;

    assert_eq!(
        recv_msg(&mut ws).await.expect("expected helloReject"),
        RelayToDesktop::HelloReject {
            reason: DesktopRejectReason::InvalidKey,
            expected_version: None,
            actual_version: None,
        }
    );
    assert!(
        recv_msg(&mut ws).await.is_none(),
        "connection should close after key reject"
    );
}

#[tokio::test]
async fn missing_desktop_key_is_rejected() {
    let addr = spawn_relay().await;
    let mut ws = connect_desktop(addr).await;
    // 空密钥 = 未携带:与错误密钥同样按 invalidKey 拒绝
    send_hello_with_key(&mut ws, PROTOCOL_VERSION, "").await;

    assert!(matches!(
        recv_msg(&mut ws).await,
        Some(RelayToDesktop::HelloReject {
            reason: DesktopRejectReason::InvalidKey,
            ..
        })
    ));
    assert!(recv_msg(&mut ws).await.is_none());
}

#[tokio::test]
async fn relay_without_configured_key_rejects_every_desktop() {
    // fail-closed:中转没配 MT_RELAY_DESKTOP_KEY 时,任何密钥都连不上
    let addr = spawn_relay_with_key(None).await;
    for key in ["", "any-key", DESKTOP_KEY] {
        let mut ws = connect_desktop(addr).await;
        send_hello_with_key(&mut ws, PROTOCOL_VERSION, key).await;
        assert!(
            matches!(
                recv_msg(&mut ws).await,
                Some(RelayToDesktop::HelloReject {
                    reason: DesktopRejectReason::KeyNotConfigured,
                    ..
                })
            ),
            "未配置密钥的中转必须拒绝桌面端(尝试密钥: {key:?})"
        );
        assert!(recv_msg(&mut ws).await.is_none());
    }
}

#[tokio::test]
async fn blank_configured_key_is_treated_as_not_configured() {
    // MT_RELAY_DESKTOP_KEY="   " 不能被当成"密钥就是空白",否则等于没鉴权
    let addr = spawn_relay_with_key(Some("   ".into())).await;
    let mut ws = connect_desktop(addr).await;
    send_hello_with_key(&mut ws, PROTOCOL_VERSION, "   ").await;

    assert!(matches!(
        recv_msg(&mut ws).await,
        Some(RelayToDesktop::HelloReject {
            reason: DesktopRejectReason::KeyNotConfigured,
            ..
        })
    ));
}

#[tokio::test]
async fn first_message_not_hello_closes_without_ack() {
    let addr = spawn_relay().await;
    let mut ws = connect_desktop(addr).await;
    ws.send(Message::Text(r#"{"type":"garbage"}"#.into()))
        .await
        .unwrap();

    assert!(recv_msg(&mut ws).await.is_none(), "non-hello first message must be dropped");
}

/// 完成握手并消费 ack + pairingUpdate 两帧。
async fn handshake(ws: &mut WsClient) {
    send_hello(ws, PROTOCOL_VERSION).await;
    assert!(matches!(
        recv_msg(ws).await,
        Some(RelayToDesktop::HelloAck { .. })
    ));
    assert!(matches!(
        recv_msg(ws).await,
        Some(RelayToDesktop::PairingUpdate { .. })
    ));
}

#[tokio::test]
async fn desktop_can_reconnect_after_disconnect() {
    let addr = spawn_relay().await;

    // 第一次连接握手成功后主动断开
    let mut first = connect_desktop(addr).await;
    handshake(&mut first).await;
    first.close(None).await.unwrap();
    drop(first);

    // 断线后重连,中转必须再次接受
    let mut second = connect_desktop(addr).await;
    handshake(&mut second).await;
}

#[tokio::test]
async fn new_desktop_connection_replaces_old_one() {
    let addr = spawn_relay().await;

    let mut old = connect_desktop(addr).await;
    handshake(&mut old).await;

    let mut new = connect_desktop(addr).await;
    handshake(&mut new).await;

    // 旧连接被顶替:下一次读取应得到关闭
    assert!(
        recv_msg(&mut old).await.is_none(),
        "old desktop connection must be closed when a new one arrives"
    );
}
