//! Seam 1:对话镜像订阅/退订路由测试。
//!
//! 进程内启动真实中转,模拟桌面端 + 已配对移动端:
//! 订阅转发、镜像消息按订阅路由、未订阅 pane 消息丢弃、退订与断线自动退订。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    DesktopToRelay, MirrorMessage, MobileToRelay, RelayToDesktop, RelayToMobile, PROTOCOL_VERSION,
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

/// 建立桌面端 + 已配对移动端,消费握手期全部帧(桌面: ack/pairingUpdate×2/快照请求;
/// 移动端: ack/presence),返回干净的两条连接。
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

    // 消费桌面端的 pairingUpdate(paired:true) 与移动端上线触发的快照请求
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

fn msg(seq: u64, content: &str) -> MirrorMessage {
    MirrorMessage {
        seq,
        source: "assistant".into(),
        content: content.into(),
        timestamp: String::new(),
        ..Default::default()
    }
}

#[tokio::test]
async fn subscribe_routes_mirror_flow_end_to_end() {
    let addr = spawn_relay().await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;

    // 订阅转发到桌面端
    send_json(
        &mut mobile,
        &MobileToRelay::SubscribePane {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribePane {
            pane_id: "pane-1".into()
        })
    );

    // 桌面端回镜像快照 → 移动端收到
    send_json(
        &mut desktop,
        &DesktopToRelay::MirrorSnapshot {
            pane_id: "pane-1".into(),
            messages: vec![msg(0, "hi")],
            has_more: false,
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::MirrorSnapshot {
            pane_id: "pane-1".into(),
            messages: vec![msg(0, "hi")],
            has_more: false,
        })
    );

    // 增量与分页历史同样可达
    send_json(
        &mut desktop,
        &DesktopToRelay::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![msg(1, "more")],
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![msg(1, "more")],
        })
    );

    send_json(
        &mut mobile,
        &MobileToRelay::RequestMirrorHistory {
            pane_id: "pane-1".into(),
            before_seq: 1,
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::RequestMirrorHistory {
            pane_id: "pane-1".into(),
            before_seq: 1,
        })
    );
}

/// 中转按类型反序列化再重新序列化转发,不认识的字段会被吃掉:工具活动的
/// toolName / toolTarget 必须是协议里的具名字段,才能原样到达移动端。
#[tokio::test]
async fn tool_activity_fields_survive_forwarding() {
    let addr = spawn_relay().await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;

    send_json(
        &mut mobile,
        &MobileToRelay::SubscribePane {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribePane { .. })
    ));

    let tool = MirrorMessage {
        kind: Some("tool".into()),
        tool_name: Some("Bash".into()),
        tool_target: Some("cargo test".into()),
        ..msg(3, "Bash cargo test")
    };
    send_json(
        &mut desktop,
        &DesktopToRelay::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![tool.clone()],
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![tool],
        })
    );
}

#[tokio::test]
async fn unsubscribed_pane_messages_are_dropped() {
    let addr = spawn_relay().await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;

    // 只订阅 pane-1
    send_json(
        &mut mobile,
        &MobileToRelay::SubscribePane {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribePane { .. })
    ));

    // 桌面端发未订阅 pane-2 的镜像消息 → 丢弃;随后 pane-1 的消息正常到达,
    // 且移动端收到的第一条就是 pane-1(证明 pane-2 没被转发)
    send_json(
        &mut desktop,
        &DesktopToRelay::MirrorAppend {
            pane_id: "pane-2".into(),
            messages: vec![msg(0, "should be dropped")],
        },
    )
    .await;
    send_json(
        &mut desktop,
        &DesktopToRelay::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![msg(0, "should arrive")],
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![msg(0, "should arrive")],
        })
    );
}

#[tokio::test]
async fn unsubscribe_stops_mirror_flow() {
    let addr = spawn_relay().await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;

    send_json(
        &mut mobile,
        &MobileToRelay::SubscribePane {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribePane { .. })
    ));

    send_json(
        &mut mobile,
        &MobileToRelay::UnsubscribePane {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::UnsubscribePane {
            pane_id: "pane-1".into()
        })
    );

    // 退订后的镜像消息被丢弃;用 presence 事件证明连接仍活且没收到镜像帧
    send_json(
        &mut desktop,
        &DesktopToRelay::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![msg(9, "after unsubscribe")],
        },
    )
    .await;
    desktop.close(None).await.unwrap();
    drop(desktop);
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: false,
            desktop_name: None,
        })
    );
}

#[tokio::test]
async fn pane_closed_routed_and_clears_subscription() {
    let addr = spawn_relay().await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;

    send_json(
        &mut mobile,
        &MobileToRelay::SubscribePane {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribePane { .. })
    ));

    send_json(
        &mut desktop,
        &DesktopToRelay::PaneClosed {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::PaneClosed {
            pane_id: "pane-1".into()
        })
    );

    // 订阅已被清:pane 关闭后的迟到镜像消息不再转发
    send_json(
        &mut desktop,
        &DesktopToRelay::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![msg(10, "late")],
        },
    )
    .await;
    desktop.close(None).await.unwrap();
    drop(desktop);
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: false,
            desktop_name: None,
        })
    );
}

#[tokio::test]
async fn mobile_disconnect_auto_unsubscribes_on_desktop() {
    let addr = spawn_relay().await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;

    send_json(
        &mut mobile,
        &MobileToRelay::SubscribePane {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribePane { .. })
    ));

    // 移动端断线 → 中转代发退订,桌面端停止镜像推送
    mobile.close(None).await.unwrap();
    drop(mobile);
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::UnsubscribePane {
            pane_id: "pane-1".into()
        })
    );
}

#[tokio::test]
async fn terminal_view_and_input_assist_route_and_clean_up_on_disconnect() {
    use mt_relay_protocol::{
        AssistFailReason, CommandFailReason, MobileProjectFile, MobileSkillItem, TerminalKey,
        TerminalLine, TerminalSpan,
    };

    let addr = spawn_relay().await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;

    // 订阅 pane-1 的终端视图 → 转发桌面端
    send_json(
        &mut mobile,
        &MobileToRelay::SubscribeTerminal {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribeTerminal {
            pane_id: "pane-1".into(),
        })
    );

    // 同连接顺序先推未订阅的 pane-other（路由层丢弃），再推已订阅的 pane-1（完整送达移动端）
    send_json(
        &mut desktop,
        &DesktopToRelay::TerminalScreen {
            pane_id: "pane-other".into(),
            cols: 80,
            rows: 24,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            alt_screen: false,
            lines: vec![],
        },
    )
    .await;
    let screen = DesktopToRelay::TerminalScreen {
        pane_id: "pane-1".into(),
        cols: 80,
        rows: 24,
        cursor_row: 1,
        cursor_col: 5,
        cursor_visible: true,
        alt_screen: false,
        lines: vec![TerminalLine {
            spans: vec![TerminalSpan {
                text: "Allow? [y/n]".into(),
                ..Default::default()
            }],
            wrapped: false,
        }],
    };
    send_json(&mut desktop, &screen).await;
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::TerminalScreen { pane_id, cols: 80, rows: 24, lines, .. })
            if pane_id == "pane-1" && lines.len() == 1 && lines[0].spans[0].text == "Allow? [y/n]"
    ));

    // 发送应急特殊按键 → 转发给桌面端
    send_json(
        &mut mobile,
        &MobileToRelay::SendTerminalKey {
            pane_id: "pane-1".into(),
            command_id: "key-1".into(),
            key: TerminalKey::Escape,
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SendTerminalKey {
            pane_id: "pane-1".into(),
            command_id: "key-1".into(),
            key: TerminalKey::Escape,
        })
    );

    // @ 文件请求 / / 技能请求端到端转发
    send_json(
        &mut mobile,
        &MobileToRelay::RequestProjectFiles {
            request_id: "req-f".into(),
            pane_id: "pane-1".into(),
            query: "main".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::RequestProjectFiles {
            request_id: "req-f".into(),
            pane_id: "pane-1".into(),
            query: "main".into(),
        })
    );
    send_json(
        &mut desktop,
        &DesktopToRelay::ProjectFiles {
            request_id: "req-f".into(),
            pane_id: "pane-1".into(),
            ok: true,
            files: vec![MobileProjectFile {
                path: "src/main.rs".into(),
            }],
            truncated: false,
            reason: None,
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::ProjectFiles { request_id, ok: true, files, .. })
            if request_id == "req-f" && files.len() == 1 && files[0].path == "src/main.rs"
    ));

    send_json(
        &mut mobile,
        &MobileToRelay::RequestProjectSkills {
            request_id: "req-s".into(),
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::RequestProjectSkills {
            request_id: "req-s".into(),
            pane_id: "pane-1".into(),
        })
    );
    send_json(
        &mut desktop,
        &DesktopToRelay::ProjectSkills {
            request_id: "req-s".into(),
            pane_id: "pane-1".into(),
            ok: true,
            prefix: "/".into(),
            skills: vec![MobileSkillItem {
                name: "commit".into(),
                description: "Create a commit".into(),
                source: "project".into(),
            }],
            reason: None,
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::ProjectSkills { request_id, ok: true, prefix, skills, .. })
            if request_id == "req-s" && prefix == "/" && skills.len() == 1
    ));

    // 桌面端掉线后，按键与输入辅助均在路由层立即返回 DesktopOffline
    desktop.close(None).await.unwrap();
    drop(desktop);
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::Presence {
            desktop_online: false,
            ..
        })
    ));

    send_json(
        &mut mobile,
        &MobileToRelay::SendTerminalKey {
            pane_id: "pane-1".into(),
            command_id: "key-2".into(),
            key: TerminalKey::CtrlC,
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::CommandReceipt {
            pane_id: "pane-1".into(),
            command_id: "key-2".into(),
            ok: false,
            reason: Some(CommandFailReason::DesktopOffline),
        })
    );

    send_json(
        &mut mobile,
        &MobileToRelay::RequestProjectFiles {
            request_id: "req-f2".into(),
            pane_id: "pane-1".into(),
            query: String::new(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::ProjectFiles {
            request_id: "req-f2".into(),
            pane_id: "pane-1".into(),
            ok: false,
            files: vec![],
            truncated: false,
            reason: Some(AssistFailReason::DesktopOffline),
        })
    );

    send_json(
        &mut mobile,
        &MobileToRelay::RequestProjectSkills {
            request_id: "req-s2".into(),
            pane_id: "pane-1".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::ProjectSkills {
            request_id: "req-s2".into(),
            pane_id: "pane-1".into(),
            ok: false,
            prefix: String::new(),
            skills: vec![],
            reason: Some(AssistFailReason::DesktopOffline),
        })
    );
}

#[tokio::test]
async fn mobile_disconnect_auto_unsubscribes_terminal_on_desktop() {
    let addr = spawn_relay().await;
    let (mut desktop, mut mobile) = paired_pair(addr).await;

    send_json(
        &mut mobile,
        &MobileToRelay::SubscribeTerminal {
            pane_id: "pane-9".into(),
        },
    )
    .await;
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::SubscribeTerminal {
            pane_id: "pane-9".into(),
        })
    );

    mobile.close(None).await.unwrap();
    drop(mobile);
    assert_eq!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::UnsubscribeTerminal {
            pane_id: "pane-9".into(),
        })
    );
}
