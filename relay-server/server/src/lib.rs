//! 中转服务器核心:axum Router 与桌面端 WebSocket 端点。
//!
//! 以 lib 形式暴露 `app()` / `RelayState`,让 Seam 1 测试进程内启动真实服务、
//! 用真实协议帧从边界驱动;`main.rs` 只负责读环境变量并绑定端口。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::Router;
use mt_relay_protocol::{DesktopToRelay, RelayToDesktop, PROTOCOL_VERSION};
use tokio::sync::{watch, Mutex};

/// 握手超时:连上后必须在此时限内送达 hello,否则直接断开。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 桌面端连接槽。1×1 拓扑:同一时刻只保留一条桌面连接,新连接顶替旧连接
/// (与配对"新顶旧"语义一致;两台桌面端互踢属配置错误,v1 不做仲裁)。
struct DesktopSlot {
    /// 当前连接的代号,用于断开时判断槽位是否仍属于自己
    generation: u64,
    /// 通知旧连接退出的信号(true = 被顶替)
    replaced_tx: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct RelayState {
    desktop: Arc<Mutex<Option<DesktopSlot>>>,
    generation_counter: Arc<AtomicU64>,
}

impl RelayState {
    pub fn new() -> Self {
        Self {
            desktop: Arc::new(Mutex::new(None)),
            generation_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 桌面端当前是否在线(presence 的单一事实来源,后续 ticket 推送给移动端)。
    pub async fn desktop_online(&self) -> bool {
        self.desktop.lock().await.is_some()
    }
}

impl Default for RelayState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn app(state: RelayState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws/desktop", any(desktop_ws_handler))
        .with_state(state)
}

async fn desktop_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<RelayState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_desktop(socket, state))
}

/// 桌面端连接生命周期:握手(版本校验)→ 注册(顶替旧连接)→ 消息循环 → 注销。
async fn handle_desktop(mut socket: WebSocket, state: RelayState) {
    // ── 握手:第一条消息必须是 hello,且版本匹配 ──
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.recv()).await;
    let actual_version = match hello {
        Ok(Some(Ok(Message::Text(text)))) => {
            match serde_json::from_str::<DesktopToRelay>(&text) {
                Ok(DesktopToRelay::Hello { protocol_version }) => protocol_version,
                _ => {
                    // 首条消息不是 hello:不 ack,直接断开(日志只记元数据)
                    eprintln!("[relay] desktop handshake failed: first message not hello");
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
            }
        }
        _ => {
            eprintln!("[relay] desktop handshake failed: timeout or non-text frame");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    if actual_version != PROTOCOL_VERSION {
        eprintln!(
            "[relay] desktop rejected: protocol version {actual_version} != {PROTOCOL_VERSION}"
        );
        let reject = RelayToDesktop::HelloReject {
            expected_version: PROTOCOL_VERSION,
            actual_version,
        };
        let _ = socket
            .send(Message::Text(serde_json::to_string(&reject).unwrap().into()))
            .await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    // ── 注册:顶替旧桌面连接 ──
    let generation = state.generation_counter.fetch_add(1, Ordering::Relaxed) + 1;
    let (replaced_tx, mut replaced_rx) = watch::channel(false);
    {
        let mut slot = state.desktop.lock().await;
        if let Some(old) = slot.take() {
            eprintln!("[relay] desktop connection replaced (gen {})", old.generation);
            let _ = old.replaced_tx.send(true);
        }
        *slot = Some(DesktopSlot {
            generation,
            replaced_tx,
        });
    }
    eprintln!("[relay] desktop connected (gen {generation})");

    let ack = RelayToDesktop::HelloAck {
        protocol_version: PROTOCOL_VERSION,
    };
    if socket
        .send(Message::Text(serde_json::to_string(&ack).unwrap().into()))
        .await
        .is_err()
    {
        deregister_desktop(&state, generation).await;
        return;
    }

    // ── 消息循环:目前桌面端不发业务消息,读到关闭/出错即退出 ──
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => { /* 后续 ticket 在此路由业务消息 */ }
                Some(Err(_)) => break,
            },
            _ = replaced_rx.changed() => {
                // 被新连接顶替:通知对端后退出,不清槽(槽已属于新连接)
                let _ = socket.send(Message::Close(None)).await;
                eprintln!("[relay] desktop disconnected (gen {generation}, replaced)");
                return;
            }
        }
    }

    deregister_desktop(&state, generation).await;
    eprintln!("[relay] desktop disconnected (gen {generation})");
}

/// 连接断开时清理槽位;仅当槽位仍属于本连接时才清(避免误清已顶替我们的新连接)。
async fn deregister_desktop(state: &RelayState, generation: u64) {
    let mut slot = state.desktop.lock().await;
    if slot.as_ref().is_some_and(|s| s.generation == generation) {
        *slot = None;
    }
}
