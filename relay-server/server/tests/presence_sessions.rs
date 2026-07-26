//! Seam 1:presence 推送与结构快照/增量事件的路由测试。
//!
//! 进程内启动真实中转,模拟桌面端 + 已配对移动端从两头驱动:
//! 移动端上线获知桌面 presence、桌面上线/离线推送、快照请求转发、增量转发。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    DesktopToRelay, MobileLauncher, MobilePane, MobileProject, MobileToRelay, RelayToDesktop,
    RelayToMobile, PROTOCOL_VERSION,
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

/// 桌面端握手(消费 ack + pairingUpdate)。
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

/// 通过配对流程建立移动端凭证,返回(已断开原连接的)凭证。
async fn obtain_credential(addr: SocketAddr, desktop: &mut WsClient) -> String {
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
    let cred = match recv_json::<RelayToMobile>(&mut mobile).await {
        Some(RelayToMobile::HelloAck {
            credential: Some(c),
            ..
        }) => c,
        other => panic!("expected credential, got {other:?}"),
    };
    // 消费配对成功通知,保持桌面端消息流干净
    assert!(matches!(
        recv_json::<RelayToDesktop>(desktop).await,
        Some(RelayToDesktop::PairingUpdate { paired: true })
    ));
    mobile.close(None).await.ok();
    cred
}

/// 用凭证连上移动端并消费 helloAck,返回连接(presence 帧留给调用方断言)。
async fn mobile_connect(addr: SocketAddr, credential: &str) -> WsClient {
    let mut ws = connect(addr, "/ws/mobile").await;
    send_json(
        &mut ws,
        &MobileToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: None,
            credential: Some(credential.into()),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut ws).await,
        Some(RelayToMobile::HelloAck { .. })
    ));
    ws
}

/// 一个有活跃 pane 的项目 + 一个没有活跃 pane 的项目(v2 起后者也进快照)。
fn sample_projects() -> Vec<MobileProject> {
    vec![
        MobileProject {
            project_id: "p1".into(),
            name: "demo".into(),
            panes: vec![MobilePane {
                pane_id: "pane-1".into(),
                title: "claude".into(),
                status: "ai-working".into(),
            }],
            can_start_session: true,
            // 带分组的项目:中转是「反序列化→再序列化」转发的,这里同时守着 groupPath 不被吃掉
            group_path: vec!["工作".into(), "后端".into()],
        },
        MobileProject {
            project_id: "p2".into(),
            name: "remote".into(),
            panes: vec![],
            can_start_session: false,
            group_path: vec![],
        },
    ]
}

fn sample_launchers() -> Vec<MobileLauncher> {
    vec![MobileLauncher {
        id: "l1".into(),
        name: "Claude".into(),
    }]
}

#[tokio::test]
async fn mobile_hello_receives_current_presence() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_handshake(addr).await;
    let cred = obtain_credential(addr, &mut desktop).await;

    // 桌面端在线:presence = true,且桌面端收到快照请求
    let mut mobile = mobile_connect(addr, &cred).await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: true
        })
    );
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SessionsSnapshotRequest)
    );
    mobile.close(None).await.ok();

    // 桌面端离线后重连移动端:presence = false,且不会有快照请求
    desktop.close(None).await.unwrap();
    drop(desktop);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut mobile2 = mobile_connect(addr, &cred).await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile2).await,
        Some(RelayToMobile::Presence {
            desktop_online: false
        })
    );
}

#[tokio::test]
async fn desktop_going_offline_and_online_pushes_presence() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_handshake(addr).await;
    let cred = obtain_credential(addr, &mut desktop).await;

    let mut mobile = mobile_connect(addr, &cred).await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: true
        })
    );
    // 消费快照请求
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SessionsSnapshotRequest)
    );

    // 杀掉桌面端 → 移动端数秒内收到离线 presence
    desktop.close(None).await.unwrap();
    drop(desktop);
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: false
        })
    );

    // 桌面端重新上线 → 移动端收到在线 presence
    let _desktop2 = desktop_handshake(addr).await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: true
        })
    );
}

#[tokio::test]
async fn snapshot_and_delta_are_routed_to_mobile() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_handshake(addr).await;
    let cred = obtain_credential(addr, &mut desktop).await;

    let mut mobile = mobile_connect(addr, &cred).await;
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence { .. })
    ));
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SessionsSnapshotRequest)
    );

    // 桌面端响应快照 → 路由到移动端(含空 pane 项目与启动器名单)
    send_json(
        &mut desktop,
        &DesktopToRelay::SessionsSnapshot {
            projects: sample_projects(),
            launchers: sample_launchers(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::SessionsSnapshot {
            projects: sample_projects(),
            launchers: sample_launchers(),
        })
    );

    // 桌面端推增量 → 路由到移动端
    send_json(
        &mut desktop,
        &DesktopToRelay::SessionsDelta {
            upserts: vec![],
            removed_project_ids: vec!["p1".into()],
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::SessionsDelta {
            upserts: vec![],
            removed_project_ids: vec!["p1".into()]
        })
    );
}

#[tokio::test]
async fn sessions_messages_without_mobile_are_dropped() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_handshake(addr).await;

    // 无移动端在线:快照/增量被丢弃,桌面端连接不受影响
    send_json(
        &mut desktop,
        &DesktopToRelay::SessionsSnapshot {
            projects: sample_projects(),
            launchers: sample_launchers(),
        },
    )
    .await;
    send_json(&mut desktop, &DesktopToRelay::RequestPairingCode).await;
    // 仍能收到配对码响应,证明连接与消息处理正常
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::PairingCode { .. })
    ));
}
