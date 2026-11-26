//! Seam 1:单中转接入多台桌面端的全套协议边界测试(issue #106)。
//!
//! 覆盖:
//! - 多台不同 desktopId 的桌面端同时在线,互不顶替;
//! - 每台桌面端独立配对,最多绑定 1 部手机(新手机扫码只吊销该桌面端旧手机,不影响其他桌面);
//! - 重置配对只作废该桌面端凭证,不影响其他桌面;
//! - 会话快照 / 镜像 / 指令按房间严格隔离,绝不跨桌面串流;
//! - 桌面端改名重连推送最新 desktopName 给该房间在线移动端;
//! - 旧版桌面端(不带 desktopId)归入 default 槽位,与新版桌面端和平共处;
//! - 非法 desktopId 拒绝(invalidDesktopId)与房间数上限(roomLimitReached + 空房回收)。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    DesktopRejectReason, DesktopToRelay, MobilePane, MobileProject, MobileRejectReason,
    MobileToRelay, RelayToDesktop, RelayToMobile, PROTOCOL_VERSION,
};
use mt_relay_server::{app, RelayLimits, RelayState, DEFAULT_DESKTOP_ID};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

const DESKTOP_KEY: &str = "multi-desktop-key";

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

async fn desktop_connect(
    addr: SocketAddr,
    desktop_id: Option<&str>,
    desktop_name: Option<&str>,
) -> WsClient {
    let mut ws = connect(addr, "/ws/desktop").await;
    send_json(
        &mut ws,
        &DesktopToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop_key: DESKTOP_KEY.into(),
            desktop_id: desktop_id.map(str::to_string),
            desktop_name: desktop_name.map(str::to_string),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut ws).await,
        Some(RelayToDesktop::HelloAck {
            protocol_version: PROTOCOL_VERSION,
            supports_multi_desktop: true,
        })
    );
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut ws).await,
        Some(RelayToDesktop::PairingUpdate { .. })
    ));
    ws
}

async fn recv_desktop_skip_snapshot_req(ws: &mut WsClient) -> Option<RelayToDesktop> {
    loop {
        match recv_json::<RelayToDesktop>(ws).await {
            Some(RelayToDesktop::SessionsSnapshotRequest) => continue,
            other => return other,
        }
    }
}

/// 为指定桌面端配对一部手机,返回 `(mobile_ws, credential, desktop_id, desktop_name)`。
async fn pair_mobile(
    addr: SocketAddr,
    desktop: &mut WsClient,
) -> (WsClient, String, Option<String>, Option<String>) {
    send_json(desktop, &DesktopToRelay::RequestPairingCode).await;
    let code = match recv_desktop_skip_snapshot_req(desktop).await {
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
    let (cred, desktop_id, desktop_name) = match recv_json::<RelayToMobile>(&mut mobile).await {
        Some(RelayToMobile::HelloAck {
            credential: Some(c),
            desktop_id,
            desktop_name,
            ..
        }) => (c, desktop_id, desktop_name),
        other => panic!("expected helloAck with credential, got {other:?}"),
    };
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: true,
            ..
        })
    ));
    assert_eq!(
        recv_desktop_skip_snapshot_req(desktop).await,
        Some(RelayToDesktop::PairingUpdate { paired: true })
    );
    // 消费上线触发的快照请求(若还没被上面跳过)
    assert_eq!(
        recv_json::<RelayToDesktop>(desktop).await,
        Some(RelayToDesktop::SessionsSnapshotRequest)
    );
    (mobile, cred, desktop_id, desktop_name)
}

#[tokio::test]
async fn multiple_desktops_coexist_and_route_traffic_independently() {
    let addr = spawn_relay().await;
    let mut desktop_a = desktop_connect(addr, Some("desk-a"), Some("Home PC")).await;
    let mut desktop_b = desktop_connect(addr, Some("desk-b"), Some("Office Mac")).await;

    let (mut mobile_a, cred_a, id_a, name_a) = pair_mobile(addr, &mut desktop_a).await;
    let (mut mobile_b, cred_b, id_b, name_b) = pair_mobile(addr, &mut desktop_b).await;

    assert_eq!(id_a.as_deref(), Some("desk-a"));
    assert_eq!(name_a.as_deref(), Some("Home PC"));
    assert_eq!(id_b.as_deref(), Some("desk-b"));
    assert_eq!(name_b.as_deref(), Some("Office Mac"));
    assert_ne!(cred_a, cred_b);

    // 桌面 A 发快照 → 只有手机 A 收到
    send_json(
        &mut desktop_a,
        &DesktopToRelay::SessionsSnapshot {
            projects: vec![MobileProject {
                project_id: "proj-a".into(),
                name: "Alpha".into(),
                panes: vec![MobilePane {
                    pane_id: "pane-a".into(),
                    title: "claude".into(),
                    status: "ai-idle".into(),
                    needs_attention: false,
                }],
                can_start_session: true,
                group_path: vec![],
            }],
            launchers: vec![],
            supports_history: true,
            supports_terminal_view: true,
            supports_input_assist: true,
        },
    )
    .await;

    // 桌面 B 发快照 → 只有手机 B 收到
    send_json(
        &mut desktop_b,
        &DesktopToRelay::SessionsSnapshot {
            projects: vec![MobileProject {
                project_id: "proj-b".into(),
                name: "Beta".into(),
                panes: vec![],
                can_start_session: true,
                group_path: vec![],
            }],
            launchers: vec![],
            supports_history: true,
            supports_terminal_view: true,
            supports_input_assist: true,
        },
    )
    .await;

    match recv_json::<RelayToMobile>(&mut mobile_a).await {
        Some(RelayToMobile::SessionsSnapshot { projects, .. }) => {
            assert_eq!(projects.len(), 1);
            assert_eq!(projects[0].project_id, "proj-a");
        }
        other => panic!("mobile_a expected proj-a snapshot, got {other:?}"),
    }
    match recv_json::<RelayToMobile>(&mut mobile_b).await {
        Some(RelayToMobile::SessionsSnapshot { projects, .. }) => {
            assert_eq!(projects.len(), 1);
            assert_eq!(projects[0].project_id, "proj-b");
        }
        other => panic!("mobile_b expected proj-b snapshot, got {other:?}"),
    }

    // 手机 A 发指令 → 只有桌面 A 收到;手机 B 发指令 → 只有桌面 B 收到
    send_json(
        &mut mobile_a,
        &MobileToRelay::MobileCommand {
            pane_id: "pane-a".into(),
            command_id: "cmd-a".into(),
            text: "hi A".into(),
        },
    )
    .await;
    send_json(
        &mut mobile_b,
        &MobileToRelay::MobileCommand {
            pane_id: "pane-b".into(),
            command_id: "cmd-b".into(),
            text: "hi B".into(),
        },
    )
    .await;

    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop_a).await,
        Some(RelayToDesktop::MobileCommand {
            pane_id: "pane-a".into(),
            command_id: "cmd-a".into(),
            text: "hi A".into(),
        })
    );
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop_b).await,
        Some(RelayToDesktop::MobileCommand {
            pane_id: "pane-b".into(),
            command_id: "cmd-b".into(),
            text: "hi B".into(),
        })
    );
}

#[tokio::test]
async fn new_mobile_for_desktop_a_revokes_only_desktop_a_mobile() {
    let addr = spawn_relay().await;
    let mut desktop_a = desktop_connect(addr, Some("desk-a"), Some("PC-A")).await;
    let mut desktop_b = desktop_connect(addr, Some("desk-b"), Some("PC-B")).await;

    let (mut mobile_a1, cred_a1, _, _) = pair_mobile(addr, &mut desktop_a).await;
    let (mut mobile_b, _cred_b, _, _) = pair_mobile(addr, &mut desktop_b).await;

    // 桌面 A 新配对手机 A2 → 手机 A1 被吊销并断开,旧凭证失效
    let (_mobile_a2, _cred_a2, _, _) = pair_mobile(addr, &mut desktop_a).await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile_a1).await,
        Some(RelayToMobile::Revoked)
    );
    assert_eq!(recv_json::<RelayToMobile>(&mut mobile_a1).await, None);

    // 旧凭证 A1 重连被拒
    let mut retry = connect(addr, "/ws/mobile").await;
    send_json(
        &mut retry,
        &MobileToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: None,
            credential: Some(cred_a1),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut retry).await,
        Some(RelayToMobile::HelloReject {
            reason: MobileRejectReason::InvalidCredential,
        })
    );

    // 桌面 B 和手机 B 依然正常通信,毫发无损
    send_json(
        &mut mobile_b,
        &MobileToRelay::RenamePane {
            pane_id: "p-b".into(),
            title: "still-alive".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop_b).await,
        Some(RelayToDesktop::RenamePane {
            pane_id: "p-b".into(),
            title: "still-alive".into(),
        })
    );
}

#[tokio::test]
async fn reset_pairing_on_one_desktop_leaves_other_desktops_intact() {
    let addr = spawn_relay().await;
    let mut desktop_a = desktop_connect(addr, Some("desk-a"), None).await;
    let mut desktop_b = desktop_connect(addr, Some("desk-b"), None).await;

    let (mut mobile_a, _, _, _) = pair_mobile(addr, &mut desktop_a).await;
    let (mut mobile_b, _, _, _) = pair_mobile(addr, &mut desktop_b).await;

    send_json(&mut desktop_a, &DesktopToRelay::ResetPairing).await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop_a).await,
        Some(RelayToDesktop::PairingUpdate { paired: false })
    );
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile_a).await,
        Some(RelayToMobile::Revoked)
    );
    assert_eq!(recv_json::<RelayToMobile>(&mut mobile_a).await, None);

    // 桌面 B 仍可收发消息
    send_json(
        &mut desktop_b,
        &DesktopToRelay::SessionsDelta {
            upserts: vec![],
            removed_project_ids: vec!["gone".into()],
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile_b).await,
        Some(RelayToMobile::SessionsDelta {
            upserts: vec![],
            removed_project_ids: vec!["gone".into()],
        })
    );
}

#[tokio::test]
async fn desktop_rename_on_reconnect_pushes_updated_presence() {
    let addr = spawn_relay().await;
    let mut desktop = desktop_connect(addr, Some("desk-x"), Some("Old Name")).await;
    let (mut mobile, _cred, _, _) = pair_mobile(addr, &mut desktop).await;

    // 同一 desktop_id 改名重连:顶替旧连接并向在线手机推送新名称
    let _desktop2 = desktop_connect(addr, Some("desk-x"), Some("New Name")).await;
    assert_eq!(recv_json::<RelayToDesktop>(&mut desktop).await, None);
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: true,
            desktop_name: Some("New Name".into()),
        })
    );
}

#[tokio::test]
async fn legacy_desktop_without_id_maps_to_default_and_coexists_with_named() {
    let addr = spawn_relay().await;
    let mut legacy = desktop_connect(addr, None, None).await;
    let mut modern = desktop_connect(addr, Some("desk-modern"), Some("Work")).await;

    let (_m_legacy, _, id_legacy, name_legacy) = pair_mobile(addr, &mut legacy).await;
    let (_m_modern, _, id_modern, name_modern) = pair_mobile(addr, &mut modern).await;

    assert_eq!(id_legacy.as_deref(), Some(DEFAULT_DESKTOP_ID));
    assert_eq!(name_legacy, None);
    assert_eq!(id_modern.as_deref(), Some("desk-modern"));
    assert_eq!(name_modern.as_deref(), Some("Work"));
}

#[tokio::test]
async fn invalid_desktop_id_is_rejected() {
    let addr = spawn_relay().await;
    for bad in ["bad id", "../etc", "a".repeat(65).as_str()] {
        let mut ws = connect(addr, "/ws/desktop").await;
        send_json(
            &mut ws,
            &DesktopToRelay::Hello {
                protocol_version: PROTOCOL_VERSION,
                desktop_key: DESKTOP_KEY.into(),
                desktop_id: Some(bad.to_string()),
                desktop_name: None,
            },
        )
        .await;
        assert_eq!(
            recv_json::<RelayToDesktop>(&mut ws).await,
            Some(RelayToDesktop::HelloReject {
                reason: DesktopRejectReason::InvalidDesktopId,
                expected_version: None,
                actual_version: None,
            })
        );
        assert_eq!(recv_json::<RelayToDesktop>(&mut ws).await, None);
    }
}

#[tokio::test]
async fn room_limit_rejects_extra_desktops_and_empty_rooms_are_reclaimed() {
    let state = RelayState::new().with_limits(RelayLimits {
        max_desktops: 2,
        ..RelayLimits::default()
    });
    let addr = spawn_relay_with_state(state).await;

    let mut d1 = desktop_connect(addr, Some("d1"), None).await;
    let _d2 = desktop_connect(addr, Some("d2"), None).await;

    // 第三台不同 desktopId 撞上限
    let mut d3 = connect(addr, "/ws/desktop").await;
    send_json(
        &mut d3,
        &DesktopToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop_key: DESKTOP_KEY.into(),
            desktop_id: Some("d3".into()),
            desktop_name: None,
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut d3).await,
        Some(RelayToDesktop::HelloReject {
            reason: DesktopRejectReason::RoomLimitReached,
            expected_version: None,
            actual_version: None,
        })
    );
    assert_eq!(recv_json::<RelayToDesktop>(&mut d3).await, None);

    // d1(未配对)断开后空房间应自动回收,d3 随后可以接入
    d1.close(None).await.unwrap();
    drop(d1);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _d3_ok = desktop_connect(addr, Some("d3"), None).await;
}
