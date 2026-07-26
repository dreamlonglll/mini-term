//! 中转服务器核心:axum Router、桌面端/移动端 WebSocket 端点与配对状态机。
//!
//! 以 lib 形式暴露 `app()` / `RelayState`,让 Seam 1 测试进程内启动真实服务、
//! 用真实协议帧从边界驱动;`main.rs` 只负责读环境变量并绑定端口。
//!
//! 中转纪律:消息体仅内存转发不落盘;日志只记元数据(连接、鉴权结果),不记消息内容。
//! 配对状态(一次性配对码、移动端长期凭证)同样仅存内存——中转重启后需重新扫码配对。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::Router;
use mt_relay_protocol::{
    CommandFailReason, DesktopRejectReason, DesktopToRelay, MobileRejectReason, MobileToRelay,
    RelayToDesktop, RelayToMobile, StartSessionFailReason, PROTOCOL_VERSION,
};
use tokio::sync::mpsc;

/// 握手超时:连上后必须在此时限内送达 hello,否则直接断开。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 一次性配对码有效期。
const PAIRING_CODE_TTL: Duration = Duration::from_secs(10 * 60);

/// 推给某条连接的出站帧(经 mpsc 送入该连接自己的写循环)。
type OutboundTx = mpsc::UnboundedSender<Message>;

/// 一条已注册连接(桌面端或移动端槽位)。1×1 拓扑:每个槽同一时刻至多一条。
struct ConnSlot {
    generation: u64,
    tx: OutboundTx,
}

struct PairingCode {
    code: String,
    issued_at: Instant,
}

#[derive(Default)]
struct Inner {
    desktop: Option<ConnSlot>,
    mobile: Option<ConnSlot>,
    /// 待兑换的一次性配对码(签发新码/兑换成功/重置配对时作废)
    pairing_code: Option<PairingCode>,
    /// 当前有效的移动端长期凭证(1×1:新配对生效即顶替)
    credential: Option<String>,
    /// 移动端当前订阅的镜像 pane 集合;未订阅 pane 的镜像消息在路由层丢弃。
    /// 只存 pane id(元数据),不缓存镜像内容。
    subscriptions: std::collections::HashSet<String>,
}

#[derive(Clone)]
pub struct RelayState {
    inner: Arc<Mutex<Inner>>,
    generation_counter: Arc<AtomicU64>,
    code_ttl: Duration,
    /// 桌面端共享密钥(部署方经 `MT_RELAY_DESKTOP_KEY` 配置)。
    /// `None` = 未配置 → fail-closed,拒绝一切桌面连接。
    desktop_key: Option<Arc<String>>,
}

impl RelayState {
    /// 未配置桌面端密钥的实例:任何桌面连接都会被拒。
    /// 生产入口必须用 [`RelayState::with_desktop_key`] 传入实际密钥。
    pub fn new() -> Self {
        Self::with_code_ttl(PAIRING_CODE_TTL)
    }

    /// 测试用:自定义配对码有效期。
    pub fn with_code_ttl(code_ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            generation_counter: Arc::new(AtomicU64::new(0)),
            code_ttl,
            desktop_key: None,
        }
    }

    /// 配置桌面端共享密钥。空白字符串按"未配置"处理(避免 `MT_RELAY_DESKTOP_KEY=`
    /// 这种写法被当成"密钥就是空串"而放行任意空密钥的桌面端)。
    pub fn with_desktop_key(mut self, key: Option<String>) -> Self {
        self.desktop_key = key
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .map(Arc::new);
        self
    }

    /// 是否已配置桌面端密钥(入口据此打印启动日志)。
    pub fn desktop_key_configured(&self) -> bool {
        self.desktop_key.is_some()
    }

    /// 桌面端握手鉴权:先看中转有没有配密钥(未配 = 拒绝一切),再比对。
    fn authenticate_desktop(&self, presented: &str) -> Result<(), DesktopRejectReason> {
        match self.desktop_key.as_deref() {
            None => Err(DesktopRejectReason::KeyNotConfigured),
            Some(expected) if secret_eq(expected, presented) => Ok(()),
            Some(_) => Err(DesktopRejectReason::InvalidKey),
        }
    }

    fn next_generation(&self) -> u64 {
        self.generation_counter.fetch_add(1, Ordering::Relaxed) + 1
    }
}

impl Default for RelayState {
    fn default() -> Self {
        Self::new()
    }
}

/// 密钥比对:等长时不因首个差异字节提前返回。长度本身仍会泄漏(不敏感)。
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// WebSocket 端点路由(不含 PWA 静态资源,测试直接用这个)。
pub fn app(state: RelayState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws/desktop", any(desktop_ws_handler))
        .route("/ws/mobile", any(mobile_ws_handler))
        .with_state(state)
}

/// 端点路由 + PWA 静态托管:非 API 路径回退到 `pwa_dir`,未命中文件时兜底
/// index.html(SPA 路由)。移动端扫码打开的页面即由此提供。
pub fn app_with_pwa(state: RelayState, pwa_dir: &str) -> Router {
    let index = std::path::Path::new(pwa_dir).join("index.html");
    let serve = tower_http::services::ServeDir::new(pwa_dir)
        .fallback(tower_http::services::ServeFile::new(index));
    app(state).fallback_service(serve)
}

fn to_text<T: serde::Serialize>(msg: &T) -> Message {
    Message::Text(serde_json::to_string(msg).unwrap().into())
}

/// 生成不可猜测的随机 id(配对码/凭证)。uuid v4 simple 格式,32 位十六进制。
fn random_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

// ─── 桌面端连接 ───

async fn desktop_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<RelayState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_desktop(socket, state))
}

/// 桌面端连接生命周期:握手(版本 → 密钥)→ 注册(顶替旧连接)→ 消息循环 → 注销。
async fn handle_desktop(mut socket: WebSocket, state: RelayState) {
    // ── 握手:第一条消息必须是 hello,且版本匹配、密钥正确 ──
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.recv()).await;
    let (actual_version, desktop_key) = match first {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<DesktopToRelay>(&text) {
            Ok(DesktopToRelay::Hello {
                protocol_version,
                desktop_key,
            }) => (protocol_version, desktop_key),
            _ => {
                eprintln!("[relay] desktop handshake failed: first message not hello");
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        },
        _ => {
            eprintln!("[relay] desktop handshake failed: timeout or non-text frame");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    // 校验顺序:版本 → 密钥。版本对不上时密钥字段的语义本就不可信。
    if actual_version != PROTOCOL_VERSION {
        eprintln!(
            "[relay] desktop rejected: protocol version {actual_version} != {PROTOCOL_VERSION}"
        );
        let reject = RelayToDesktop::HelloReject {
            reason: DesktopRejectReason::VersionMismatch,
            expected_version: Some(PROTOCOL_VERSION),
            actual_version: Some(actual_version),
        };
        let _ = socket.send(to_text(&reject)).await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    // 鉴权失败只记原因,绝不记密钥本身(中转日志纪律)
    if let Err(reason) = state.authenticate_desktop(&desktop_key) {
        eprintln!("[relay] desktop rejected: {reason:?}");
        let reject = RelayToDesktop::HelloReject {
            reason,
            expected_version: None,
            actual_version: None,
        };
        let _ = socket.send(to_text(&reject)).await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    // ── 注册:顶替旧桌面连接(两台桌面端互踢属配置错误,v1 不做仲裁) ──
    let generation = state.next_generation();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let paired = {
        let mut inner = state.inner.lock().unwrap();
        let replaced = inner.desktop.take();
        if let Some(old) = replaced.as_ref() {
            eprintln!("[relay] desktop connection replaced (gen {})", old.generation);
            let _ = old.tx.send(Message::Close(None));
        }
        inner.desktop = Some(ConnSlot { generation, tx });
        // presence:桌面端从离线转为在线时通知移动端(顶替不算状态变化)
        if replaced.is_none() {
            if let Some(mobile) = inner.mobile.as_ref() {
                let _ = mobile.tx.send(to_text(&RelayToMobile::Presence {
                    desktop_online: true,
                }));
            }
        }
        inner.credential.is_some()
    };
    eprintln!("[relay] desktop connected (gen {generation})");

    // 握手成功:ack + 当前配对状态
    let ack = RelayToDesktop::HelloAck {
        protocol_version: PROTOCOL_VERSION,
    };
    if socket.send(to_text(&ack)).await.is_err()
        || socket
            .send(to_text(&RelayToDesktop::PairingUpdate { paired }))
            .await
            .is_err()
    {
        deregister_desktop(&state, generation);
        return;
    }

    // ── 消息循环 ──
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                // 槽位持有者(顶替我们的新连接/未来的路由方)让我们发帧;Close 帧发完即退出
                Some(frame) => {
                    let is_close = matches!(frame, Message::Close(_));
                    if socket.send(frame).await.is_err() || is_close {
                        if is_close {
                            eprintln!("[relay] desktop disconnected (gen {generation}, replaced)");
                            return; // 被顶替:槽已属于新连接,不注销
                        }
                        break;
                    }
                }
                None => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<DesktopToRelay>(&text) {
                        Ok(msg) => handle_desktop_message(&state, msg),
                        Err(_) => eprintln!("[relay] desktop sent unparseable message (ignored)"),
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }

    deregister_desktop(&state, generation);
    eprintln!("[relay] desktop disconnected (gen {generation})");
}

/// 处理握手后的桌面端业务消息。
fn handle_desktop_message(state: &RelayState, msg: DesktopToRelay) {
    match msg {
        DesktopToRelay::Hello { .. } => {} // 重复 hello 忽略
        // 结构快照/增量:只转发给在线移动端,中转不缓存不解析内容
        DesktopToRelay::SessionsSnapshot {
            projects,
            launchers,
        } => {
            let inner = state.inner.lock().unwrap();
            if let Some(mobile) = inner.mobile.as_ref() {
                let _ = mobile.tx.send(to_text(&RelayToMobile::SessionsSnapshot {
                    projects,
                    launchers,
                }));
            }
        }
        DesktopToRelay::SessionsDelta {
            upserts,
            removed_project_ids,
        } => {
            let inner = state.inner.lock().unwrap();
            if let Some(mobile) = inner.mobile.as_ref() {
                let _ = mobile.tx.send(to_text(&RelayToMobile::SessionsDelta {
                    upserts,
                    removed_project_ids,
                }));
            }
        }
        // 镜像消息:仅路由给已订阅该 pane 的移动端,未订阅一律丢弃
        DesktopToRelay::MirrorSnapshot {
            pane_id,
            messages,
            has_more,
        } => {
            let inner = state.inner.lock().unwrap();
            if inner.subscriptions.contains(&pane_id) {
                if let Some(mobile) = inner.mobile.as_ref() {
                    let _ = mobile.tx.send(to_text(&RelayToMobile::MirrorSnapshot {
                        pane_id,
                        messages,
                        has_more,
                    }));
                }
            }
        }
        DesktopToRelay::MirrorAppend { pane_id, messages } => {
            let inner = state.inner.lock().unwrap();
            if inner.subscriptions.contains(&pane_id) {
                if let Some(mobile) = inner.mobile.as_ref() {
                    let _ = mobile
                        .tx
                        .send(to_text(&RelayToMobile::MirrorAppend { pane_id, messages }));
                }
            }
        }
        DesktopToRelay::MirrorHistory {
            pane_id,
            messages,
            has_more,
        } => {
            let inner = state.inner.lock().unwrap();
            if inner.subscriptions.contains(&pane_id) {
                if let Some(mobile) = inner.mobile.as_ref() {
                    let _ = mobile.tx.send(to_text(&RelayToMobile::MirrorHistory {
                        pane_id,
                        messages,
                        has_more,
                    }));
                }
            }
        }
        // pane 关闭:转发并清掉订阅(后续同 pane 消息不再路由)
        DesktopToRelay::PaneClosed { pane_id } => {
            let mut inner = state.inner.lock().unwrap();
            if inner.subscriptions.remove(&pane_id) {
                if let Some(mobile) = inner.mobile.as_ref() {
                    let _ = mobile
                        .tx
                        .send(to_text(&RelayToMobile::PaneClosed { pane_id }));
                }
            }
        }
        // 指令回执:原样转发(以 command_id 关联,不依赖订阅状态)
        DesktopToRelay::CommandReceipt {
            pane_id,
            command_id,
            ok,
            reason,
        } => {
            let inner = state.inner.lock().unwrap();
            if let Some(mobile) = inner.mobile.as_ref() {
                let _ = mobile.tx.send(to_text(&RelayToMobile::CommandReceipt {
                    pane_id,
                    command_id,
                    ok,
                    reason,
                }));
            }
        }
        // 发起会话回执:原样转发(以 request_id 关联)
        DesktopToRelay::StartSessionReceipt {
            request_id,
            ok,
            pane_id,
            reason,
        } => {
            let inner = state.inner.lock().unwrap();
            if let Some(mobile) = inner.mobile.as_ref() {
                let _ = mobile.tx.send(to_text(&RelayToMobile::StartSessionReceipt {
                    request_id,
                    ok,
                    pane_id,
                    reason,
                }));
            }
        }
        DesktopToRelay::RequestPairingCode => {
            let code = random_id();
            let mut inner = state.inner.lock().unwrap();
            inner.pairing_code = Some(PairingCode {
                code: code.clone(),
                issued_at: Instant::now(),
            });
            eprintln!("[relay] pairing code issued");
            if let Some(desktop) = inner.desktop.as_ref() {
                let _ = desktop.tx.send(to_text(&RelayToDesktop::PairingCode { code }));
            }
        }
        DesktopToRelay::ResetPairing => {
            let mut inner = state.inner.lock().unwrap();
            inner.pairing_code = None;
            inner.credential = None;
            if let Some(mobile) = inner.mobile.take() {
                let _ = mobile.tx.send(to_text(&RelayToMobile::Revoked));
                let _ = mobile.tx.send(Message::Close(None));
                drop_subscriptions(&mut inner);
            }
            eprintln!("[relay] pairing reset: credential revoked");
            if let Some(desktop) = inner.desktop.as_ref() {
                let _ = desktop
                    .tx
                    .send(to_text(&RelayToDesktop::PairingUpdate { paired: false }));
            }
        }
    }
}

fn deregister_desktop(state: &RelayState, generation: u64) {
    let mut inner = state.inner.lock().unwrap();
    if inner
        .desktop
        .as_ref()
        .is_some_and(|s| s.generation == generation)
    {
        inner.desktop = None;
        // presence:桌面端离线,立即推给在线移动端(离线横幅)
        if let Some(mobile) = inner.mobile.as_ref() {
            let _ = mobile.tx.send(to_text(&RelayToMobile::Presence {
                desktop_online: false,
            }));
        }
    }
}

// ─── 移动端连接 ───

async fn mobile_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<RelayState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_mobile(socket, state))
}

/// 移动端握手的鉴权结果。
enum MobileAuth {
    /// 配对码兑换成功,携带新签发凭证
    NewlyPaired(String),
    /// 凭证重连成功
    Resumed,
    Rejected(MobileRejectReason),
}

/// 校验移动端握手并落配对状态(锁内完成,不做 IO)。
fn authenticate_mobile(
    state: &RelayState,
    pairing_code: Option<String>,
    credential: Option<String>,
) -> MobileAuth {
    let mut inner = state.inner.lock().unwrap();
    if let Some(code) = pairing_code {
        let valid = inner.pairing_code.as_ref().is_some_and(|active| {
            active.code == code && active.issued_at.elapsed() <= state.code_ttl
        });
        if !valid {
            return MobileAuth::Rejected(MobileRejectReason::InvalidPairingCode);
        }
        // 兑换成功:配对码一次性作废;新凭证顶替旧凭证(1×1),踢掉旧移动端连接
        inner.pairing_code = None;
        let new_credential = random_id();
        inner.credential = Some(new_credential.clone());
        if let Some(old) = inner.mobile.take() {
            let _ = old.tx.send(to_text(&RelayToMobile::Revoked));
            let _ = old.tx.send(Message::Close(None));
            drop_subscriptions(&mut inner);
        }
        if let Some(desktop) = inner.desktop.as_ref() {
            let _ = desktop
                .tx
                .send(to_text(&RelayToDesktop::PairingUpdate { paired: true }));
        }
        MobileAuth::NewlyPaired(new_credential)
    } else if let Some(cred) = credential {
        if inner.credential.as_deref() == Some(cred.as_str()) {
            MobileAuth::Resumed
        } else {
            MobileAuth::Rejected(MobileRejectReason::InvalidCredential)
        }
    } else {
        MobileAuth::Rejected(MobileRejectReason::MissingAuth)
    }
}

/// 移动端连接生命周期:握手(配对码兑换/凭证校验)→ 注册 → 消息循环 → 注销。
async fn handle_mobile(mut socket: WebSocket, state: RelayState) {
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.recv()).await;
    let (version, pairing_code, credential) = match first {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<MobileToRelay>(&text) {
            Ok(MobileToRelay::Hello {
                protocol_version,
                pairing_code,
                credential,
            }) => (protocol_version, pairing_code, credential),
            _ => {
                eprintln!("[relay] mobile handshake failed: first message not hello");
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        },
        _ => {
            eprintln!("[relay] mobile handshake failed: timeout or non-text frame");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    if version != PROTOCOL_VERSION {
        eprintln!("[relay] mobile rejected: protocol version {version} != {PROTOCOL_VERSION}");
        let _ = socket
            .send(to_text(&RelayToMobile::HelloReject {
                reason: MobileRejectReason::VersionMismatch,
            }))
            .await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    let issued_credential = match authenticate_mobile(&state, pairing_code, credential) {
        MobileAuth::Rejected(reason) => {
            eprintln!("[relay] mobile rejected: {reason:?}");
            let _ = socket
                .send(to_text(&RelayToMobile::HelloReject { reason }))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
        MobileAuth::NewlyPaired(cred) => Some(cred),
        MobileAuth::Resumed => None,
    };

    // ── 注册:同凭证重连顶替旧连接 ──
    let generation = state.next_generation();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let desktop_online = {
        let mut inner = state.inner.lock().unwrap();
        if let Some(old) = inner.mobile.take() {
            eprintln!("[relay] mobile connection replaced (gen {})", old.generation);
            let _ = old.tx.send(Message::Close(None));
            // 旧连接的订阅对新连接无意义,清掉并通知桌面端停止镜像推送
            drop_subscriptions(&mut inner);
        }
        inner.mobile = Some(ConnSlot { generation, tx });
        inner.desktop.is_some()
    };
    eprintln!("[relay] mobile connected (gen {generation})");

    // 握手成功:ack + 当前桌面端 presence;桌面端在线则请它回发最新结构快照
    let ack = RelayToMobile::HelloAck {
        protocol_version: PROTOCOL_VERSION,
        credential: issued_credential,
    };
    if socket.send(to_text(&ack)).await.is_err()
        || socket
            .send(to_text(&RelayToMobile::Presence { desktop_online }))
            .await
            .is_err()
    {
        deregister_mobile(&state, generation);
        return;
    }
    if desktop_online {
        let inner = state.inner.lock().unwrap();
        if let Some(desktop) = inner.desktop.as_ref() {
            let _ = desktop
                .tx
                .send(to_text(&RelayToDesktop::SessionsSnapshotRequest));
        }
    }

    // ── 消息循环 ──
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(frame) => {
                    let is_close = matches!(frame, Message::Close(_));
                    if socket.send(frame).await.is_err() || is_close {
                        if is_close {
                            eprintln!("[relay] mobile disconnected (gen {generation}, kicked)");
                            return; // 被吊销/顶替:槽位已易主或已清空,不注销
                        }
                        break;
                    }
                }
                None => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<MobileToRelay>(&text) {
                        Ok(msg) => handle_mobile_message(&state, msg),
                        Err(_) => eprintln!("[relay] mobile sent unparseable message (ignored)"),
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }

    deregister_mobile(&state, generation);
    eprintln!("[relay] mobile disconnected (gen {generation})");
}

/// 处理握手后的移动端业务消息:订阅登记 + 转发给桌面端。
fn handle_mobile_message(state: &RelayState, msg: MobileToRelay) {
    let mut inner = state.inner.lock().unwrap();
    let forward = match msg {
        MobileToRelay::Hello { .. } => None, // 重复 hello 忽略
        MobileToRelay::SubscribePane { pane_id } => {
            inner.subscriptions.insert(pane_id.clone());
            Some(RelayToDesktop::SubscribePane { pane_id })
        }
        MobileToRelay::UnsubscribePane { pane_id } => {
            inner.subscriptions.remove(&pane_id);
            Some(RelayToDesktop::UnsubscribePane { pane_id })
        }
        MobileToRelay::RequestMirrorHistory {
            pane_id,
            before_seq,
        } => inner
            .subscriptions
            .contains(&pane_id)
            .then_some(RelayToDesktop::RequestMirrorHistory {
                pane_id,
                before_seq,
            }),
        // 移动端指令:桌面端离线即拒(路由层生成失败回执,不做存储转发)
        MobileToRelay::MobileCommand {
            pane_id,
            command_id,
            text,
        } => {
            if inner.desktop.is_some() {
                Some(RelayToDesktop::MobileCommand {
                    pane_id,
                    command_id,
                    text,
                })
            } else {
                eprintln!("[relay] mobile command rejected: desktop offline");
                if let Some(mobile) = inner.mobile.as_ref() {
                    let _ = mobile.tx.send(to_text(&RelayToMobile::CommandReceipt {
                        pane_id,
                        command_id,
                        ok: false,
                        reason: Some(CommandFailReason::DesktopOffline),
                    }));
                }
                None
            }
        }
        // 重命名会话:桌面端离线就丢弃。无回执通道——改没改成看结构增量回不回新
        // title,离线时手机侧本来就看得到「桌面端离线」横幅
        MobileToRelay::RenamePane { pane_id, title } => inner
            .desktop
            .is_some()
            .then_some(RelayToDesktop::RenamePane { pane_id, title }),
        // 发起新 AI 会话:同样离线即拒(桌面离线意味着起不来,补送没有意义)
        MobileToRelay::StartAiSession {
            request_id,
            project_id,
            launcher_id,
        } => {
            if inner.desktop.is_some() {
                Some(RelayToDesktop::StartAiSession {
                    request_id,
                    project_id,
                    launcher_id,
                })
            } else {
                eprintln!("[relay] start ai session rejected: desktop offline");
                if let Some(mobile) = inner.mobile.as_ref() {
                    let _ = mobile.tx.send(to_text(&RelayToMobile::StartSessionReceipt {
                        request_id,
                        ok: false,
                        pane_id: None,
                        reason: Some(StartSessionFailReason::DesktopOffline),
                    }));
                }
                None
            }
        }
    };
    if let (Some(msg), Some(desktop)) = (forward, inner.desktop.as_ref()) {
        let _ = desktop.tx.send(to_text(&msg));
    }
}

/// 清空移动端订阅并逐一通知桌面端退订(移动端断线/被顶替/被吊销时调用)。
fn drop_subscriptions(inner: &mut Inner) {
    let panes: Vec<String> = inner.subscriptions.drain().collect();
    if let Some(desktop) = inner.desktop.as_ref() {
        for pane_id in panes {
            let _ = desktop
                .tx
                .send(to_text(&RelayToDesktop::UnsubscribePane { pane_id }));
        }
    }
}

fn deregister_mobile(state: &RelayState, generation: u64) {
    let mut inner = state.inner.lock().unwrap();
    if inner
        .mobile
        .as_ref()
        .is_some_and(|s| s.generation == generation)
    {
        inner.mobile = None;
        drop_subscriptions(&mut inner);
    }
}
