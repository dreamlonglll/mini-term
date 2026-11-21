//! Seam 1:中转的资源上限(单条消息 / 连接数 / 出站队列)。
//!
//! 进程内启动真实中转,从 WebSocket 边界驱动:连接数超限在升级前被 503 / 429 挡下、
//! 名额随断开归还;按 IP 计数认配置的请求头与对端地址;超限消息断开发送方,
//! 合法的大镜像照常转发;慢消费者的出站队列满了即断开、订阅随之清掉。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    DesktopToRelay, MirrorMessage, MobileToRelay, RelayToDesktop, RelayToMobile, PROTOCOL_VERSION,
};
use mt_relay_server::{
    app, RelayLimits, RelayState, DESKTOP_MAX_MESSAGE_BYTES, MOBILE_MAX_MESSAGE_BYTES,
};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 中转与桌面端约定的共享密钥(v2 起桌面端握手必须携带)。
const DESKTOP_KEY: &str = "test-desktop-key";

/// 测试里按 IP 计数用的请求头(模拟反代写入的客户端地址)。
const CLIENT_IP_HEADER: &str = "x-client-ip";

fn limits(max_connections: usize, per_ip: usize, header: Option<&str>) -> RelayLimits {
    RelayLimits {
        max_connections,
        max_connections_per_ip: per_ip,
        client_ip_header: header.map(|h| h.parse().unwrap()),
        ..RelayLimits::default()
    }
}

fn relay_state(limits: RelayLimits) -> RelayState {
    RelayState::new()
        .with_desktop_key(Some(DESKTOP_KEY.into()))
        .with_limits(limits)
}

/// 进程内启动中转(与其它测试同款:直接 serve `app()`,不带对端地址)。
async fn spawn_relay(limits: RelayLimits) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app(relay_state(limits))).into_future());
    addr
}

/// 与 `main.rs` 同款:带上对端地址(`into_make_service_with_connect_info`)。
async fn spawn_relay_with_peer_addr(limits: RelayLimits) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        axum::serve(
            listener,
            app(relay_state(limits)).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future(),
    );
    addr
}

/// 发起 WebSocket 升级;被名额闸挡在升级前时返回 HTTP 状态码。
async fn try_connect(
    addr: SocketAddr,
    path: &str,
    client_ip: Option<&str>,
) -> Result<WsClient, u16> {
    let mut request = format!("ws://{addr}{path}").into_client_request().unwrap();
    if let Some(ip) = client_ip {
        request
            .headers_mut()
            .insert(CLIENT_IP_HEADER, ip.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(e) => panic!("unexpected connect error: {e}"),
    }
}

async fn connect(addr: SocketAddr, path: &str) -> WsClient {
    try_connect(addr, path, None)
        .await
        .expect("ws connect failed")
}

/// 名额归还是异步的(服务端要先察觉对端断开):轮询到能连上为止。
async fn wait_until_connectable(addr: SocketAddr, path: &str, client_ip: Option<&str>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match try_connect(addr, path, client_ip).await {
            Ok(_) => return,
            Err(status) if tokio::time::Instant::now() < deadline => {
                assert!(status == 503 || status == 429, "unexpected status {status}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(status) => panic!("slot never freed (still {status})"),
        }
    }
}

async fn send_json<T: serde::Serialize>(ws: &mut WsClient, msg: &T) {
    ws.send(Message::Text(serde_json::to_string(msg).unwrap().into()))
        .await
        .unwrap();
}

/// 超限消息:服务端读到帧头就会断开,客户端这边写到一半被重置属预期,不 unwrap。
async fn send_json_lossy<T: serde::Serialize>(ws: &mut WsClient, msg: &T) {
    let _ = ws
        .send(Message::Text(serde_json::to_string(msg).unwrap().into()))
        .await;
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

/// 建立桌面端 + 已配对移动端,消费握手期全部帧,返回干净的两条连接。
async fn paired_pair(addr: SocketAddr) -> (WsClient, WsClient) {
    let mut desktop = connect(addr, "/ws/desktop").await;
    send_json(
        &mut desktop,
        &DesktopToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop_key: DESKTOP_KEY.into(),
            desktop_id: None,
            desktop_name: None,
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::HelloAck { .. })
    ));
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::PairingUpdate { .. })
    ));

    send_json(&mut desktop, &DesktopToRelay::RequestPairingCode).await;
    let code = match recv_json::<RelayToDesktop>(&mut desktop).await {
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
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::PairingUpdate { paired: true })
    ));
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SessionsSnapshotRequest)
    ));

    (desktop, mobile)
}

/// 移动端订阅 pane,并消费桌面端收到的订阅转发。
async fn subscribe(desktop: &mut WsClient, mobile: &mut WsClient, pane_id: &str) {
    send_json(
        mobile,
        &MobileToRelay::SubscribePane {
            pane_id: pane_id.into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(desktop).await,
        Some(RelayToDesktop::SubscribePane {
            pane_id: pane_id.into()
        })
    );
}

fn msg(seq: u64, content: String) -> MirrorMessage {
    MirrorMessage {
        seq,
        source: "assistant".into(),
        content,
        timestamp: String::new(),
        ..Default::default()
    }
}

#[tokio::test]
async fn global_limit_rejects_upgrade_with_503_until_a_slot_frees() {
    let addr = spawn_relay(limits(2, 10, None)).await;

    // 两个端点共用全局名额;尚未握手的连接同样占名额
    let first = connect(addr, "/ws/desktop").await;
    let _second = connect(addr, "/ws/mobile").await;
    assert_eq!(try_connect(addr, "/ws/mobile", None).await.err(), Some(503));
    assert_eq!(
        try_connect(addr, "/ws/desktop", None).await.err(),
        Some(503)
    );

    drop(first);
    wait_until_connectable(addr, "/ws/desktop", None).await;
}

#[tokio::test]
async fn per_client_limit_keys_on_configured_header() {
    let addr = spawn_relay(limits(10, 1, Some(CLIENT_IP_HEADER))).await;

    let first = try_connect(addr, "/ws/mobile", Some("203.0.113.1"))
        .await
        .expect("first connection from a client must pass");
    assert_eq!(
        try_connect(addr, "/ws/mobile", Some("203.0.113.1"))
            .await
            .err(),
        Some(429)
    );
    // 另一个客户端不受影响
    let _other = try_connect(addr, "/ws/desktop", Some("203.0.113.2"))
        .await
        .expect("another client must not be affected");

    drop(first);
    wait_until_connectable(addr, "/ws/mobile", Some("203.0.113.1")).await;
}

#[tokio::test]
async fn per_client_limit_falls_back_to_peer_address() {
    // 不配请求头:按 TCP 对端地址计数(测试里都是 127.0.0.1)
    let addr = spawn_relay_with_peer_addr(limits(10, 1, None)).await;

    let _first = connect(addr, "/ws/desktop").await;
    assert_eq!(try_connect(addr, "/ws/mobile", None).await.err(), Some(429));
}

#[tokio::test]
async fn oversized_mobile_message_disconnects_mobile() {
    let addr = spawn_relay(RelayLimits::default()).await;
    let (_desktop, mut mobile) = paired_pair(addr).await;

    send_json_lossy(
        &mut mobile,
        &MobileToRelay::MobileCommand {
            pane_id: "pane-1".into(),
            command_id: "cmd-1".into(),
            text: "x".repeat(MOBILE_MAX_MESSAGE_BYTES + 1),
        },
    )
    .await;
    assert_eq!(recv_json::<RelayToMobile>(&mut mobile).await, None);
}

#[tokio::test]
async fn oversized_desktop_message_disconnects_desktop() {
    let addr = spawn_relay(RelayLimits::default()).await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;
    subscribe(&mut desktop, &mut mobile, "pane-1").await;

    send_json_lossy(
        &mut desktop,
        &DesktopToRelay::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![msg(0, "x".repeat(DESKTOP_MAX_MESSAGE_BYTES + 1))],
        },
    )
    .await;
    assert_eq!(recv_json::<RelayToDesktop>(&mut desktop).await, None);
    // 超限帧没有被转发,移动端看到的是桌面端离线
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: false,
            desktop_name: None,
        })
    );
}

#[tokio::test]
async fn large_but_legal_mirror_snapshot_is_forwarded_intact() {
    let addr = spawn_relay(RelayLimits::default()).await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;
    subscribe(&mut desktop, &mut mobile, "pane-1").await;

    // 一页 50 条、合计约 4 MiB:比移动端上行上限大得多,但远在桌面端上限之内
    let messages: Vec<MirrorMessage> = (0..50)
        .map(|seq| msg(seq, "镜".repeat(28 * 1024)))
        .collect();
    let snapshot = DesktopToRelay::MirrorSnapshot {
        pane_id: "pane-1".into(),
        messages: messages.clone(),
        has_more: true,
    };
    assert!(serde_json::to_string(&snapshot).unwrap().len() > 4 * MOBILE_MAX_MESSAGE_BYTES);
    send_json(&mut desktop, &snapshot).await;

    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::MirrorSnapshot {
            pane_id: "pane-1".into(),
            messages,
            has_more: true,
        })
    );
}

#[tokio::test]
async fn slow_mobile_consumer_is_disconnected_when_outbound_queue_fills() {
    let addr = spawn_relay(RelayLimits {
        outbound_queue: 4,
        ..RelayLimits::default()
    })
    .await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;
    subscribe(&mut desktop, &mut mobile, "pane-1").await;

    // 移动端从此不读:内核收发缓冲填满后,中转写不出去,出站队列随之积满。
    // 桌面端持续推镜像增量,直到中转断开移动端 —— 注销时清掉订阅并通知桌面端退订
    let chunk = "x".repeat(512 * 1024);
    let mut unsubscribed = false;
    for seq in 0..256 {
        send_json(
            &mut desktop,
            &DesktopToRelay::MirrorAppend {
                pane_id: "pane-1".into(),
                messages: vec![msg(seq, chunk.clone())],
            },
        )
        .await;
        if let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::ZERO, desktop.next()).await
        {
            assert_eq!(
                serde_json::from_str::<RelayToDesktop>(&text).unwrap(),
                RelayToDesktop::UnsubscribePane {
                    pane_id: "pane-1".into()
                }
            );
            unsubscribed = true;
            break;
        }
    }
    if !unsubscribed {
        assert_eq!(
            recv_json::<RelayToDesktop>(&mut desktop).await,
            Some(RelayToDesktop::UnsubscribePane {
                pane_id: "pane-1".into()
            }),
            "slow mobile consumer was never disconnected"
        );
    }

    // 移动端这头:把缓冲里已送达的帧读完之后就是断开
    let mut drained = 0;
    while recv_json::<RelayToMobile>(&mut mobile).await.is_some() {
        drained += 1;
        assert!(drained <= 256, "mobile connection should have been closed");
    }
}
