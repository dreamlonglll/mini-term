//! Seam 1:中转服务端心跳保活与空闲僵死连接回收。
//!
//! 进程内启动真实中转(缩短 ping 周期与空闲超时),从 WebSocket 边界验证:
//! 1. 正常响应 Ping 的桌面端与移动端即使无业务消息也会保活不断开;
//! 2. 停止读流/不回 Pong 的僵死桌面端在 idle_timeout 后被服务端主动断开,
//!    在线移动端随之收到 `Presence { desktop_online: false }`,且连接名额归还;
//! 3. 停止读流/不回 Pong 的僵死移动端在 idle_timeout 后被服务端主动断开,
//!    在线桌面端随之收到 `UnsubscribePane` 通知。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    DesktopToRelay, MobileToRelay, RelayToDesktop, RelayToMobile, PROTOCOL_VERSION,
};
use mt_relay_server::{app, RelayLimits, RelayState};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

const DESKTOP_KEY: &str = "test-desktop-key";

async fn spawn_relay(
    ping_interval: Duration,
    idle_timeout: Duration,
    max_connections: usize,
) -> SocketAddr {
    let state = RelayState::new()
        .with_desktop_key(Some(DESKTOP_KEY.into()))
        .with_heartbeat(ping_interval, idle_timeout)
        .with_limits(RelayLimits {
            max_connections,
            max_connections_per_ip: max_connections,
            ..RelayLimits::default()
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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

async fn recv_json<T: serde::de::DeserializeOwned>(ws: &mut WsClient) -> Option<T> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("timed out waiting for message")?;
        match frame {
            Ok(Message::Text(text)) => {
                return Some(serde_json::from_str(&text).expect("invalid message"));
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
}

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

/// 驱动一条客户端持续消费服务端 Ping 并回 Pong(tokio-tungstenite 在 poll_next 时
/// 自动排队 Pong,需要 flush / next 推进),在指定时长内不得收到 Close。
async fn pump_heartbeats_for(ws: &mut WsClient, duration: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + duration;
    let mut pings = 0usize;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return pings;
        }
        match tokio::time::timeout(deadline - now, ws.next()).await {
            Err(_) => return pings,
            Ok(Some(Ok(Message::Ping(payload)))) => {
                pings += 1;
                // 主动显式刷出 Pong,确保服务端立即收到
                ws.send(Message::Pong(payload)).await.unwrap();
            }
            Ok(Some(Ok(Message::Pong(_)))) => {}
            Ok(other) => panic!("expected heartbeat survival, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn responsive_clients_stay_alive_past_idle_timeout_via_server_pings() {
    let addr = spawn_relay(Duration::from_millis(60), Duration::from_millis(220), 8).await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;

    // 两条连接都保持读流回 Pong,持续超过 2 倍 idle_timeout 仍必须健康存活
    let (desktop_pings, mobile_pings) = tokio::join!(
        pump_heartbeats_for(&mut desktop, Duration::from_millis(500)),
        pump_heartbeats_for(&mut mobile, Duration::from_millis(500)),
    );
    assert!(
        desktop_pings >= 2 && mobile_pings >= 2,
        "服务端应持续向两端发送 Ping: desktop={desktop_pings}, mobile={mobile_pings}"
    );

    // 心跳期后业务消息仍正常互通
    send_json(
        &mut mobile,
        &MobileToRelay::SubscribePane {
            pane_id: "p1".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribePane {
            pane_id: "p1".into()
        })
    );
}

#[tokio::test]
async fn unresponsive_desktop_is_disconnected_and_frees_connection_slot() {
    // 全局仅 2 个名额(恰好被 desktop + mobile 占满),以此顺带验证超时后名额归还
    let addr = spawn_relay(Duration::from_millis(50), Duration::from_millis(180), 2).await;
    let (_stalled_desktop, mut mobile) = paired_pair(addr).await;

    // `_stalled_desktop` 不再读流、不回 Pong(模拟 NAT 半开 / 睡眠僵死);
    // 移动端持续响应 Ping 保持在线,应在超时后收到 desktop_online=false
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_offline = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(400), mobile.next()).await {
            Ok(Some(Ok(Message::Ping(p)))) => {
                mobile.send(Message::Pong(p)).await.unwrap();
            }
            Ok(Some(Ok(Message::Text(text)))) => {
                let msg: RelayToMobile = serde_json::from_str(&text).unwrap();
                if matches!(
                    msg,
                    RelayToMobile::Presence {
                        desktop_online: false,
                        ..
                    }
                ) {
                    saw_offline = true;
                    break;
                }
            }
            other => panic!("unexpected mobile frame while waiting for desktop timeout: {other:?}"),
        }
    }
    assert!(saw_offline, "僵死桌面端超时断开后移动端应收到离线通知");

    // 僵死桌面端的名额已释放:新桌面端可以立即重连(否则全局 2 名额会报 503)
    let _reconnected = connect(addr, "/ws/desktop").await;
}

#[tokio::test]
async fn unresponsive_mobile_is_disconnected_and_drops_subscriptions() {
    let addr = spawn_relay(Duration::from_millis(50), Duration::from_millis(180), 4).await;
    let (mut desktop, mut stalled_mobile) = paired_pair(addr).await;

    // 移动端先订阅 pane,然后彻底停止读流 / 不回 Pong
    send_json(
        &mut stalled_mobile,
        &MobileToRelay::SubscribePane {
            pane_id: "pane-hb".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribePane {
            pane_id: "pane-hb".into()
        })
    );

    // 桌面端持续响应 Ping,应在移动端空闲超时后收到 UnsubscribePane
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_unsub = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(400), desktop.next()).await {
            Ok(Some(Ok(Message::Ping(p)))) => {
                desktop.send(Message::Pong(p)).await.unwrap();
            }
            Ok(Some(Ok(Message::Text(text)))) => {
                let msg: RelayToDesktop = serde_json::from_str(&text).unwrap();
                if msg
                    == (RelayToDesktop::UnsubscribePane {
                        pane_id: "pane-hb".into(),
                    })
                {
                    saw_unsub = true;
                    break;
                }
            }
            other => panic!("unexpected desktop frame while waiting for mobile timeout: {other:?}"),
        }
    }
    assert!(saw_unsub, "僵死移动端超时断开后桌面端应收到退订通知");
    drop(stalled_mobile);
}
