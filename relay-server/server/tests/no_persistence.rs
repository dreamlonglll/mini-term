//! Seam 1:消息体不落盘。
//!
//! 以真实子进程启动中转,把工作目录 / HOME / TMP 全部指向全新临时目录,
//! 从两头驱动配对、结构快照、镜像与指令全套真实协议帧(均含"消息体"),
//! 结束后检查临时目录树里没有产生任何文件。

use futures_util::{SinkExt, StreamExt};
use mt_relay_protocol::{
    DesktopToRelay, MirrorMessage, MobileToRelay, RelayToDesktop, RelayToMobile, PROTOCOL_VERSION,
};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 递归收集目录下所有文件(不含目录本身)。
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
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

#[tokio::test]
async fn relay_process_leaves_no_files_behind() {
    // 全新隔离目录:进程 cwd 与所有常见落盘位置都指到这里
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("mt-relay-no-persist-{ts}"));
    let home = root.join("home");
    let tmp = root.join("tmp");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_mt-relay-server"))
        .current_dir(&root)
        .env("RELAY_BIND", "127.0.0.1")
        .env("RELAY_PORT", "0") // 临时端口,从 stderr 日志解析实际值
        .env("MT_RELAY_DESKTOP_KEY", "no-persist-key")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("TMP", &tmp)
        .env("TEMP", &tmp)
        .env("TMPDIR", &tmp)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("failed to spawn relay binary");

    // 从 stderr 拿实际端口;之后持续排空管道防止子进程 eprintln 阻塞
    let stderr = child.stderr.take().unwrap();
    let mut reader = std::io::BufReader::new(stderr);
    let port: u16 = {
        let mut port = None;
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if let Some(rest) = line.trim().strip_prefix("[relay] listening on 127.0.0.1:") {
                port = rest
                    .split_whitespace()
                    .next()
                    .and_then(|p| p.parse().ok());
                break;
            }
            line.clear();
        }
        port.expect("relay did not report listening port")
    };
    std::thread::spawn(move || {
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
    });

    // ── 驱动带消息体的全套协议流量 ──
    let connect = |path: &'static str| async move {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}{path}"))
            .await
            .expect("ws connect failed");
        ws
    };

    // 桌面端握手
    let mut desktop = connect("/ws/desktop").await;
    send_json(
        &mut desktop,
        &DesktopToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop_key: "no-persist-key".into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::HelloAck { .. })
    ));
    let _ = recv_json::<RelayToDesktop>(&mut desktop).await; // pairingUpdate

    // 配对
    send_json(&mut desktop, &DesktopToRelay::RequestPairingCode).await;
    let code = match recv_json::<RelayToDesktop>(&mut desktop).await {
        Some(RelayToDesktop::PairingCode { code }) => code,
        other => panic!("expected pairingCode, got {other:?}"),
    };
    let mut mobile = connect("/ws/mobile").await;
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
    let _ = recv_json::<RelayToMobile>(&mut mobile).await; // presence
    let _ = recv_json::<RelayToDesktop>(&mut desktop).await; // pairingUpdate(true)
    let _ = recv_json::<RelayToDesktop>(&mut desktop).await; // snapshot request

    // 镜像订阅 + 携带明文"对话内容"的镜像消息 + 移动端指令
    send_json(
        &mut mobile,
        &MobileToRelay::SubscribePane {
            pane_id: "pane-1".into(),
        },
    )
    .await;
    let _ = recv_json::<RelayToDesktop>(&mut desktop).await; // subscribe
    send_json(
        &mut desktop,
        &DesktopToRelay::MirrorAppend {
            pane_id: "pane-1".into(),
            messages: vec![MirrorMessage {
                seq: 0,
                source: "assistant".into(),
                content: "SECRET-CONVERSATION-BODY-must-not-touch-disk".into(),
                timestamp: String::new(),
            }],
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToMobile>(&mut mobile).await,
        Some(RelayToMobile::MirrorAppend { .. })
    ));
    send_json(
        &mut mobile,
        &MobileToRelay::MobileCommand {
            pane_id: "pane-1".into(),
            command_id: "cmd-1".into(),
            text: "SECRET-COMMAND-must-not-touch-disk".into(),
        },
    )
    .await;
    assert!(matches!(
        recv_json::<RelayToDesktop>(&mut desktop).await,
        Some(RelayToDesktop::MobileCommand { .. })
    ));

    // 收尾:断开连接,结束进程,检查无残留文件
    mobile.close(None).await.ok();
    desktop.close(None).await.ok();
    tokio::time::sleep(Duration::from_millis(200)).await;
    child.kill().ok();
    let _ = child.wait();

    let mut files = Vec::new();
    collect_files(&root, &mut files);
    assert!(
        files.is_empty(),
        "中转不得写任何文件,发现残留: {files:?}"
    );

    std::fs::remove_dir_all(&root).ok();
}
