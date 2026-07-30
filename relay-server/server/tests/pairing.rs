//! Seam 1:配对全套协议边界测试。
//!
//! 进程内启动真实中转,模拟桌面端 + 移动端两头驱动真实协议帧:
//! 配对码兑换凭证、错误凭证拒绝、配对码一次性、新配对顶旧、重置吊销。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    DesktopToRelay, MobileRejectReason, MobileToRelay, RelayToDesktop, RelayToMobile,
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

/// 中转与桌面端约定的共享密钥(v2 起桌面端握手必须携带)。
const DESKTOP_KEY: &str = "test-desktop-key";

async fn spawn_relay() -> SocketAddr {
    spawn_relay_with_state(RelayState::new()).await
}

async fn spawn_relay_with_state(state: RelayState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = state.with_desktop_key(Some(DESKTOP_KEY.into()));
    tokio::spawn(axum::serve(listener, app(state)).into_future());
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

/// 读下一条文本帧并反序列化;None = 连接被关闭。
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

/// 建立已握手的桌面端连接(消费 ack + pairingUpdate)。
async fn paired_desktop(addr: SocketAddr) -> WsClient {
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

/// 读取下一条配对相关的桌面端消息,跳过移动端上线附带的结构快照请求。
async fn recv_desktop_pairing(ws: &mut WsClient) -> Option<RelayToDesktop> {
    loop {
        match recv_json::<RelayToDesktop>(ws).await {
            Some(RelayToDesktop::SessionsSnapshotRequest) => continue,
            other => return other,
        }
    }
}

/// 桌面端请求配对码并取回。
async fn request_pairing_code(desktop: &mut WsClient) -> String {
    send_json(desktop, &DesktopToRelay::RequestPairingCode).await;
    match recv_desktop_pairing(desktop).await {
        Some(RelayToDesktop::PairingCode { code }) => code,
        other => panic!("expected pairingCode, got {other:?}"),
    }
}

/// 移动端用配对码/凭证握手。
async fn mobile_hello(
    addr: SocketAddr,
    pairing_code: Option<String>,
    credential: Option<String>,
) -> (WsClient, Option<RelayToMobile>) {
    let mut ws = connect(addr, "/ws/mobile").await;
    send_json(
        &mut ws,
        &MobileToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            pairing_code,
            credential,
        },
    )
    .await;
    let reply = recv_json::<RelayToMobile>(&mut ws).await;
    (ws, reply)
}

#[tokio::test]
async fn pairing_code_exchanges_for_credential() {
    let addr = spawn_relay().await;
    let mut desktop = paired_desktop(addr).await;
    let code = request_pairing_code(&mut desktop).await;

    let (_mobile, reply) = mobile_hello(addr, Some(code), None).await;
    match reply {
        Some(RelayToMobile::HelloAck {
            credential: Some(cred),
            ..
        }) => assert!(!cred.is_empty()),
        other => panic!("expected helloAck with credential, got {other:?}"),
    }

    // 桌面端收到配对成功通知
    assert_eq!(
        recv_desktop_pairing(&mut desktop).await,
        Some(RelayToDesktop::PairingUpdate { paired: true })
    );
}

#[tokio::test]
async fn credential_survives_reconnect_without_rescan() {
    let addr = spawn_relay().await;
    let mut desktop = paired_desktop(addr).await;
    let code = request_pairing_code(&mut desktop).await;

    let (mut mobile, reply) = mobile_hello(addr, Some(code), None).await;
    let cred = match reply {
        Some(RelayToMobile::HelloAck {
            credential: Some(c),
            ..
        }) => c,
        other => panic!("expected credential, got {other:?}"),
    };
    mobile.close(None).await.unwrap();
    drop(mobile);

    // 关闭重开:凭长期凭证重连,无需再扫码;不再签发新凭证
    let (_mobile2, reply2) = mobile_hello(addr, None, Some(cred)).await;
    assert!(matches!(
        reply2,
        Some(RelayToMobile::HelloAck { credential: None, .. })
    ));
}

#[tokio::test]
async fn pairing_code_is_single_use() {
    let addr = spawn_relay().await;
    let mut desktop = paired_desktop(addr).await;
    let code = request_pairing_code(&mut desktop).await;

    let (_m1, reply1) = mobile_hello(addr, Some(code.clone()), None).await;
    assert!(matches!(reply1, Some(RelayToMobile::HelloAck { .. })));

    // 同一配对码第二次兑换必须被拒
    let (mut m2, reply2) = mobile_hello(addr, Some(code), None).await;
    assert_eq!(
        reply2,
        Some(RelayToMobile::HelloReject {
            reason: MobileRejectReason::InvalidPairingCode
        })
    );
    assert!(recv_json::<RelayToMobile>(&mut m2).await.is_none());
}

#[tokio::test]
async fn expired_pairing_code_is_rejected() {
    // TTL 为 0:签发即过期
    let addr = spawn_relay_with_state(RelayState::with_code_ttl(Duration::ZERO)).await;
    let mut desktop = paired_desktop(addr).await;
    let code = request_pairing_code(&mut desktop).await;

    tokio::time::sleep(Duration::from_millis(20)).await;
    let (_m, reply) = mobile_hello(addr, Some(code), None).await;
    assert_eq!(
        reply,
        Some(RelayToMobile::HelloReject {
            reason: MobileRejectReason::InvalidPairingCode
        })
    );
}

#[tokio::test]
async fn wrong_or_missing_auth_is_rejected() {
    let addr = spawn_relay().await;

    // 错误凭证
    let (_m1, reply1) = mobile_hello(addr, None, Some("bogus".into())).await;
    assert_eq!(
        reply1,
        Some(RelayToMobile::HelloReject {
            reason: MobileRejectReason::InvalidCredential
        })
    );

    // 既无配对码也无凭证
    let (_m2, reply2) = mobile_hello(addr, None, None).await;
    assert_eq!(
        reply2,
        Some(RelayToMobile::HelloReject {
            reason: MobileRejectReason::MissingAuth
        })
    );

    // 无效配对码
    let (_m3, reply3) = mobile_hello(addr, Some("nope".into()), None).await;
    assert_eq!(
        reply3,
        Some(RelayToMobile::HelloReject {
            reason: MobileRejectReason::InvalidPairingCode
        })
    );
}

#[tokio::test]
async fn mobile_version_mismatch_rejected() {
    let addr = spawn_relay().await;
    let mut ws = connect(addr, "/ws/mobile").await;
    send_json(
        &mut ws,
        &MobileToRelay::Hello {
            protocol_version: 999,
            pairing_code: None,
            credential: None,
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut ws).await,
        Some(RelayToMobile::HelloReject {
            reason: MobileRejectReason::VersionMismatch
        })
    );
}

#[tokio::test]
async fn new_pairing_revokes_old_device() {
    let addr = spawn_relay().await;
    let mut desktop = paired_desktop(addr).await;

    // 设备 A 配对并保持连接
    let code_a = request_pairing_code(&mut desktop).await;
    let (mut mobile_a, reply_a) = mobile_hello(addr, Some(code_a), None).await;
    let cred_a = match reply_a {
        Some(RelayToMobile::HelloAck {
            credential: Some(c),
            ..
        }) => c,
        other => panic!("expected credential, got {other:?}"),
    };
    // 消费握手后紧随的 presence 帧
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile_a).await,
        Some(RelayToMobile::Presence { .. })
    ));
    assert_eq!(
        recv_desktop_pairing(&mut desktop).await,
        Some(RelayToDesktop::PairingUpdate { paired: true })
    );

    // 设备 B 扫新码配对
    let code_b = request_pairing_code(&mut desktop).await;
    let (_mobile_b, reply_b) = mobile_hello(addr, Some(code_b), None).await;
    assert!(matches!(reply_b, Some(RelayToMobile::HelloAck { .. })));

    // 设备 A 立即收到 revoked 并被断开
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile_a).await,
        Some(RelayToMobile::Revoked)
    );
    assert!(recv_json::<RelayToMobile>(&mut mobile_a).await.is_none());

    // 设备 A 的旧凭证重连被拒
    let (_m, reply) = mobile_hello(addr, None, Some(cred_a)).await;
    assert_eq!(
        reply,
        Some(RelayToMobile::HelloReject {
            reason: MobileRejectReason::InvalidCredential
        })
    );
}

#[tokio::test]
async fn reset_pairing_revokes_credential_and_kicks_mobile() {
    let addr = spawn_relay().await;
    let mut desktop = paired_desktop(addr).await;
    let code = request_pairing_code(&mut desktop).await;

    let (mut mobile, reply) = mobile_hello(addr, Some(code), None).await;
    let cred = match reply {
        Some(RelayToMobile::HelloAck {
            credential: Some(c),
            ..
        }) => c,
        other => panic!("expected credential, got {other:?}"),
    };
    // 消费握手后紧随的 presence 帧
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence { .. })
    ));
    assert_eq!(
        recv_desktop_pairing(&mut desktop).await,
        Some(RelayToDesktop::PairingUpdate { paired: true })
    );

    // 桌面端一键重置配对
    send_json(&mut desktop, &DesktopToRelay::ResetPairing).await;
    assert_eq!(
        recv_desktop_pairing(&mut desktop).await,
        Some(RelayToDesktop::PairingUpdate { paired: false })
    );

    // 在线移动端被吊销并断开
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Revoked)
    );
    assert!(recv_json::<RelayToMobile>(&mut mobile).await.is_none());

    // 旧凭证全部失效
    let (_m, reply) = mobile_hello(addr, None, Some(cred)).await;
    assert_eq!(
        reply,
        Some(RelayToMobile::HelloReject {
            reason: MobileRejectReason::InvalidCredential
        })
    );
}
