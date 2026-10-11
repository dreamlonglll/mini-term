//! 中转服务器核心:axum Router、桌面端/移动端 WebSocket 端点与配对状态机。
//!
//! 以 lib 形式暴露 `app()` / `RelayState`,让 Seam 1 测试进程内启动真实服务、
//! 用真实协议帧从边界驱动;`main.rs` 只负责读环境变量并绑定端口。
//!
//! 中转纪律:消息体仅内存转发不落盘;日志只记元数据(连接、鉴权结果),不记消息内容。
//! 配对状态(一次性配对码、移动端长期凭证)同样仅存内存——中转重启后需重新扫码配对。
//!
//! # 资源上限(公网服务,未鉴权的连接也要先占资源)
//!
//! - **单条消息**:按端点分开设([`DESKTOP_MAX_MESSAGE_BYTES`] / [`MOBILE_MAX_MESSAGE_BYTES`]),
//!   取代 axum 默认的 64 MiB。限额在升级时定死,握手前的那条 hello 也受它约束。
//! - **连接数**:全局 + 按客户端 IP 两道闸([`RelayLimits`]),在 WebSocket 升级**之前**判,
//!   超限直接回 HTTP 503 / 429,不进握手。
//! - **出站队列**:每条连接有界([`RelayLimits::outbound_queue`]),满了 = 对端消费太慢,
//!   断开该连接而不是丢帧,理由见 [`ConnSlot::send`]。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Extension, Router};
use mt_relay_protocol::{
    CommandFailReason, DesktopRejectReason, DesktopToRelay, MobileRejectReason, MobileToRelay,
    RelayToDesktop, RelayToMobile, StartSessionFailReason, PROTOCOL_VERSION,
};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, Notify};

/// 握手超时:连上后必须在此时限内送达 hello,否则直接断开。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 一次性配对码有效期。
const PAIRING_CODE_TTL: Duration = Duration::from_secs(10 * 60);

/// 桌面端单条入站消息上限:16 MiB。
///
/// 桌面端发来的消息里只有对话镜像(`mirrorSnapshot` / `mirrorHistory` 一页 50 条文本、
/// `mirrorAppend`)能长大:协议里没有图片或二进制载荷,镜像只抽会话记录里的
/// user / assistant **文本**,外加工具调用的一行摘要(工具名 + 截到 160 字的目标,
/// 单条几百字节,不计入那 50 条、单页总数另有封顶;不含工具输出)。
/// 常见一页几十 KB 到几百 KB;极端情况是用户往 AI 里贴过大段日志,一页 50 条里带几条 MB 级正文,也就几 MB。16 MiB 给这种
/// 极端情况留出三倍以上余量,同时比默认的 64 MiB 收紧四倍 —— 升级前的这一跳谁都能连,
/// 默认值等于允许任何人让中转为每条连接先攒 64 MiB。
pub const DESKTOP_MAX_MESSAGE_BYTES: usize = 16 << 20;

/// 移动端单条入站消息上限:1 MiB。
///
/// 移动端上行只有握手、订阅 / 退订、翻页请求、点选作答、改名(桌面端截 64 字)、
/// 发起会话与移动端指令 —— 唯一的自由文本是指令正文,手机上敲 / 粘贴的内容到 KB 级
/// 已经很长。1 MiB 是千倍量级的余量。
pub const MOBILE_MAX_MESSAGE_BYTES: usize = 1 << 20;

/// 全局并发 WebSocket 连接数上限的默认值(`RELAY_MAX_CONNECTIONS`)。
///
/// 中转是单租户 1×1 拓扑:正常只有一条桌面端 + 一条移动端,顶替 / 重连的瞬间多出
/// 一两条。64 远高于正常用量,又把「每条连接先攒一条最大消息」的最坏内存压在
/// 64 × 16 MiB = 1 GiB 以内。
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// 单个客户端 IP 并发连接数上限的默认值(`RELAY_MAX_CONNECTIONS_PER_IP`)。
///
/// ⚠️ 反代后面部署时必须配 [`RelayLimits::client_ip_header`],否则所有连接的对端地址
/// 都是反代自己,这道闸就退化成第二道全局闸(16 条仍够正常使用,只是挡不住单一来源
/// 占满全局名额)。
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 16;

/// 每条连接出站队列容量(帧数)的默认值。
///
/// 正常流量是零星的结构增量、1s 一轮的镜像增量与回执,手机切回前台时的突发
/// (ack + presence + 快照 + 若干镜像页)也就十来帧。积压到 256 帧 = 对端已经落后
/// 很多轮,断开重连比继续攒更快恢复。
pub const DEFAULT_OUTBOUND_QUEUE: usize = 256;

/// 连接相关的资源上限。默认值见各常量;`main.rs` 从环境变量覆盖。
#[derive(Debug, Clone)]
pub struct RelayLimits {
    /// 全局并发 WebSocket 连接数(含尚未握手的)
    pub max_connections: usize,
    /// 单个客户端 IP 的并发连接数
    pub max_connections_per_ip: usize,
    /// 按 IP 计数时从哪个请求头取客户端地址(`RELAY_CLIENT_IP_HEADER`,如
    /// `CF-Connecting-IP`)。`None` = 用 TCP 对端地址。头里有多个逗号分隔的值时取
    /// **最后一个**(离中转最近的那一跳写的);头缺失或解析不出 IP 时回落对端地址。
    /// 只在中转前面确有反代改写该头时才配 —— 否则客户端可以自报任意值绕开这道闸
    /// (全局闸不受影响)。
    pub client_ip_header: Option<HeaderName>,
    /// 每条连接的出站队列容量(帧)
    pub outbound_queue: usize,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_connections_per_ip: DEFAULT_MAX_CONNECTIONS_PER_IP,
            client_ip_header: None,
            outbound_queue: DEFAULT_OUTBOUND_QUEUE,
        }
    }
}

/// 当前在册的连接计数(全局 + 按 IP)。条目随 [`ConnPermit`] 析构递减,归零即删,
/// 所以 `per_ip` 的大小不会超过全局上限。
#[derive(Default)]
struct ConnCounts {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// 一条 WebSocket 连接占用的名额。升级前领取,随连接任务一起活到连接结束;
/// 升级没成(客户端半路走了)时随 `on_upgrade` 的闭包一起析构,名额同样归还。
struct ConnPermit {
    counts: Arc<Mutex<ConnCounts>>,
    ip: IpAddr,
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        // 析构里不能 panic(可能正处于展开中):锁中毒也照常归还名额
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts.total = counts.total.saturating_sub(1);
        if let Some(n) = counts.per_ip.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.per_ip.remove(&self.ip);
            }
        }
    }
}

/// 名额不够时的拒绝理由(映射成 HTTP 状态码)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmitError {
    /// 全局满 → 503
    Global,
    /// 该 IP 满 → 429
    PerIp,
}

impl IntoResponse for AdmitError {
    fn into_response(self) -> Response {
        match self {
            AdmitError::Global => (
                StatusCode::SERVICE_UNAVAILABLE,
                "relay connection limit reached",
            )
                .into_response(),
            AdmitError::PerIp => (
                StatusCode::TOO_MANY_REQUESTS,
                "too many connections from this client",
            )
                .into_response(),
        }
    }
}

/// 解析计数用的客户端地址(口径见 [`RelayLimits::client_ip_header`])。
/// 两样都拿不到(进程内测试没挂 ConnectInfo)时归到 0.0.0.0 这一个桶。
fn client_ip(limits: &RelayLimits, headers: &HeaderMap, peer: Option<SocketAddr>) -> IpAddr {
    let from_header = limits.client_ip_header.as_ref().and_then(|name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
    });
    from_header
        .or(peer.map(|a| a.ip()))
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

/// 推给某条连接的出站帧(经有界 mpsc 送入该连接自己的写循环)。
type OutboundTx = mpsc::Sender<Message>;

/// 一条已注册连接(桌面端或移动端槽位)。1×1 拓扑:每个槽同一时刻至多一条。
struct ConnSlot {
    generation: u64,
    tx: OutboundTx,
    /// 出站队列溢出信号:连接自己的循环收到即断开(见 [`ConnSlot::send`])
    overflow: Arc<Notify>,
}

impl ConnSlot {
    /// 往这条连接的出站队列放一帧。**不阻塞**:调用方都持着全局状态锁。
    ///
    /// 队列满 = 对端消费跟不上(弱网手机、卡住的桌面端)。策略是**断开**而不是丢帧:
    /// 镜像按 seq 连续、结构增量是差量、回执一问一答,丢掉任何一条对端都不会知道,
    /// 只会静默失真(漏一段对话、项目列表对不上、指令永远等不到回执)。断开则走的是
    /// 两端早就有的重连路径 —— 移动端重连后握手拿 presence、桌面端回发全量快照、
    /// 重新订阅拿镜像快照,状态整体自愈;桌面端重连同理。
    fn send(&self, frame: Message) {
        match self.tx.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.overflow.notify_one(),
            // 写循环已退出:注销就在路上,这一帧本来也送不到了
            Err(TrySendError::Closed(_)) => {}
        }
    }
}

/// 把一帧写给对端,同时盯着出站队列的溢出信号。
///
/// 写本身也要和溢出赛跑:对端彻底卡死(TCP 窗口为 0)时 `send` 永远不返回,
/// 只在外层 select 里等溢出信号就永远等不到断开的那一刻。返回 `false` = 该断开了。
async fn send_or_overflow(
    socket: &mut WebSocket,
    frame: Message,
    overflow: &Notify,
    side: &str,
    generation: u64,
) -> bool {
    tokio::select! {
        sent = socket.send(frame) => sent.is_ok(),
        _ = overflow.notified() => {
            log_overflow(side, generation);
            false
        }
    }
}

fn log_overflow(side: &str, generation: u64) {
    eprintln!(
        "[relay] {side} outbound queue full (gen {generation}): slow consumer, disconnecting"
    );
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
    /// 连接数 / 出站队列上限(见 [`RelayLimits`])
    limits: Arc<RelayLimits>,
    /// 在册连接计数,[`RelayState::admit`] 领名额、[`ConnPermit`] 析构归还
    conns: Arc<Mutex<ConnCounts>>,
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
            limits: Arc::new(RelayLimits::default()),
            conns: Arc::new(Mutex::new(ConnCounts::default())),
        }
    }

    /// 覆盖连接数 / 出站队列上限(默认值见 [`RelayLimits::default`])。
    /// 容量类字段为 0 时按 1 处理:0 等于拒绝一切连接 / 建不出队列,不会是有意的配置。
    pub fn with_limits(mut self, limits: RelayLimits) -> Self {
        self.limits = Arc::new(RelayLimits {
            max_connections: limits.max_connections.max(1),
            max_connections_per_ip: limits.max_connections_per_ip.max(1),
            outbound_queue: limits.outbound_queue.max(1),
            ..limits
        });
        self
    }

    /// 当前生效的上限(入口据此打印启动日志)。
    pub fn limits(&self) -> &RelayLimits {
        &self.limits
    }

    /// 为一条新连接领名额:全局与该 IP 都没满才放行。
    fn admit(&self, ip: IpAddr) -> Result<ConnPermit, AdmitError> {
        let mut counts = self.conns.lock().unwrap();
        if counts.total >= self.limits.max_connections {
            return Err(AdmitError::Global);
        }
        let per_ip = counts.per_ip.get(&ip).copied().unwrap_or(0);
        if per_ip >= self.limits.max_connections_per_ip {
            return Err(AdmitError::PerIp);
        }
        counts.total += 1;
        counts.per_ip.insert(ip, per_ip + 1);
        Ok(ConnPermit {
            counts: self.conns.clone(),
            ip,
        })
    }

    /// 升级前的名额闸:解析客户端地址 → 领名额;拒绝时记一行元数据(不记地址本身)。
    fn admit_request(
        &self,
        side: &str,
        headers: &HeaderMap,
        peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    ) -> Result<ConnPermit, AdmitError> {
        let peer = peer.map(|Extension(ConnectInfo(addr))| addr);
        let ip = client_ip(&self.limits, headers, peer);
        self.admit(ip).inspect_err(|reason| match reason {
            AdmitError::Global => eprintln!(
                "[relay] {side} connection rejected: global limit ({}) reached",
                self.limits.max_connections
            ),
            AdmitError::PerIp => eprintln!(
                "[relay] {side} connection rejected: per-client limit ({}) reached",
                self.limits.max_connections_per_ip
            ),
        })
    }

    /// 新建一条连接的出站队列(容量取 [`RelayLimits::outbound_queue`])。
    fn outbound_channel(&self) -> (OutboundTx, mpsc::Receiver<Message>) {
        mpsc::channel(self.limits.outbound_queue)
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

/// `peer` 只有经 `into_make_service_with_connect_info` 起服务时才有(`main.rs`);
/// 进程内测试直接 serve `app()`,拿不到时按头部 / 共用桶计数。
async fn desktop_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<RelayState>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
) -> Response {
    let permit = match state.admit_request("desktop", &headers, peer) {
        Ok(permit) => permit,
        Err(reason) => return reason.into_response(),
    };
    ws.max_message_size(DESKTOP_MAX_MESSAGE_BYTES)
        .max_frame_size(DESKTOP_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            // 名额随连接任务活到连接结束
            let _permit = permit;
            handle_desktop(socket, state).await;
        })
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
    let (tx, mut rx) = state.outbound_channel();
    let overflow = Arc::new(Notify::new());
    let paired = {
        let mut inner = state.inner.lock().unwrap();
        let replaced = inner.desktop.take();
        if let Some(old) = replaced.as_ref() {
            eprintln!("[relay] desktop connection replaced (gen {})", old.generation);
            old.send(Message::Close(None));
        }
        inner.desktop = Some(ConnSlot {
            generation,
            tx,
            overflow: overflow.clone(),
        });
        // presence:桌面端从离线转为在线时通知移动端(顶替不算状态变化)
        if replaced.is_none() {
            if let Some(mobile) = inner.mobile.as_ref() {
                mobile.send(to_text(&RelayToMobile::Presence {
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
            // 出站队列溢出(慢消费者):断开,注销后由桌面端重连自愈
            _ = overflow.notified() => {
                log_overflow("desktop", generation);
                break;
            }
            out = rx.recv() => match out {
                // 槽位持有者(顶替我们的新连接/未来的路由方)让我们发帧;Close 帧发完即退出
                Some(frame) => {
                    let is_close = matches!(frame, Message::Close(_));
                    let sent =
                        send_or_overflow(&mut socket, frame, &overflow, "desktop", generation)
                            .await;
                    if is_close {
                        eprintln!("[relay] desktop disconnected (gen {generation}, replaced)");
                        return; // 被顶替:槽已属于新连接,不注销
                    }
                    if !sent {
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
                mobile.send(to_text(&RelayToMobile::SessionsSnapshot {
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
                mobile.send(to_text(&RelayToMobile::SessionsDelta {
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
                    mobile.send(to_text(&RelayToMobile::MirrorSnapshot {
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
                    mobile.send(to_text(&RelayToMobile::MirrorAppend { pane_id, messages }));
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
                    mobile.send(to_text(&RelayToMobile::MirrorHistory {
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
                    mobile.send(to_text(&RelayToMobile::PaneClosed { pane_id }));
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
                mobile.send(to_text(&RelayToMobile::CommandReceipt {
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
                mobile.send(to_text(&RelayToMobile::StartSessionReceipt {
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
                desktop.send(to_text(&RelayToDesktop::PairingCode { code }));
            }
        }
        DesktopToRelay::ResetPairing => {
            let mut inner = state.inner.lock().unwrap();
            inner.pairing_code = None;
            inner.credential = None;
            if let Some(mobile) = inner.mobile.take() {
                mobile.send(to_text(&RelayToMobile::Revoked));
                mobile.send(Message::Close(None));
                drop_subscriptions(&mut inner);
            }
            eprintln!("[relay] pairing reset: credential revoked");
            if let Some(desktop) = inner.desktop.as_ref() {
                desktop.send(to_text(&RelayToDesktop::PairingUpdate { paired: false }));
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
            mobile.send(to_text(&RelayToMobile::Presence {
                desktop_online: false,
            }));
        }
    }
}

// ─── 移动端连接 ───

async fn mobile_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<RelayState>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
) -> Response {
    let permit = match state.admit_request("mobile", &headers, peer) {
        Ok(permit) => permit,
        Err(reason) => return reason.into_response(),
    };
    ws.max_message_size(MOBILE_MAX_MESSAGE_BYTES)
        .max_frame_size(MOBILE_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            handle_mobile(socket, state).await;
        })
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
            old.send(to_text(&RelayToMobile::Revoked));
            old.send(Message::Close(None));
            drop_subscriptions(&mut inner);
        }
        if let Some(desktop) = inner.desktop.as_ref() {
            desktop.send(to_text(&RelayToDesktop::PairingUpdate { paired: true }));
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
    let (tx, mut rx) = state.outbound_channel();
    let overflow = Arc::new(Notify::new());
    let desktop_online = {
        let mut inner = state.inner.lock().unwrap();
        if let Some(old) = inner.mobile.take() {
            eprintln!("[relay] mobile connection replaced (gen {})", old.generation);
            old.send(Message::Close(None));
            // 旧连接的订阅对新连接无意义,清掉并通知桌面端停止镜像推送
            drop_subscriptions(&mut inner);
        }
        inner.mobile = Some(ConnSlot {
            generation,
            tx,
            overflow: overflow.clone(),
        });
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
            desktop.send(to_text(&RelayToDesktop::SessionsSnapshotRequest));
        }
    }

    // ── 消息循环 ──
    loop {
        tokio::select! {
            // 出站队列溢出(弱网 / 后台挂起的手机):断开,重连后握手 + 快照自愈
            _ = overflow.notified() => {
                log_overflow("mobile", generation);
                break;
            }
            out = rx.recv() => match out {
                Some(frame) => {
                    let is_close = matches!(frame, Message::Close(_));
                    let sent =
                        send_or_overflow(&mut socket, frame, &overflow, "mobile", generation)
                            .await;
                    if is_close {
                        eprintln!("[relay] mobile disconnected (gen {generation}, kicked)");
                        return; // 被吊销/顶替:槽位已易主或已清空,不注销
                    }
                    if !sent {
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
        // 先 let 再 then_some:「解构模式 => 方法链」这一形状 rustfmt 排不了,
        // 会把整个 match 退成 `let forward =\n match` 的回退排版
        MobileToRelay::RequestMirrorHistory {
            pane_id,
            before_seq,
        } => {
            let subscribed = inner.subscriptions.contains(&pane_id);
            subscribed.then_some(RelayToDesktop::RequestMirrorHistory {
                pane_id,
                before_seq,
            })
        }
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
                reject_command_offline(&inner, "mobile command", pane_id, command_id);
                None
            }
        }
        // 点选作答 agent 提问:与移动端指令同款——桌面端离线即拒,回执同通道
        MobileToRelay::AnswerQuestion {
            pane_id,
            command_id,
            seq,
            question_id,
            question_index,
            option_index,
        } => {
            if inner.desktop.is_some() {
                Some(RelayToDesktop::AnswerQuestion {
                    pane_id,
                    command_id,
                    seq,
                    question_id,
                    question_index,
                    option_index,
                })
            } else {
                reject_command_offline(&inner, "answer question", pane_id, command_id);
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
                    mobile.send(to_text(&RelayToMobile::StartSessionReceipt {
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
        desktop.send(to_text(&msg));
    }
}

/// 桌面端离线时的路由层拒绝:直接给移动端回失败的指令回执(不做存储转发)。
/// 移动端指令与点选作答共用——两者的回执都是 CommandReceipt。
fn reject_command_offline(inner: &Inner, what: &str, pane_id: String, command_id: String) {
    eprintln!("[relay] {what} rejected: desktop offline");
    if let Some(mobile) = inner.mobile.as_ref() {
        mobile.send(to_text(&RelayToMobile::CommandReceipt {
            pane_id,
            command_id,
            ok: false,
            reason: Some(CommandFailReason::DesktopOffline),
        }));
    }
}

/// 清空移动端订阅并逐一通知桌面端退订(移动端断线/被顶替/被吊销时调用)。
fn drop_subscriptions(inner: &mut Inner) {
    let panes: Vec<String> = inner.subscriptions.drain().collect();
    if let Some(desktop) = inner.desktop.as_ref() {
        for pane_id in panes {
            desktop.send(to_text(&RelayToDesktop::UnsubscribePane { pane_id }));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// 名额:同一 IP 先撞按 IP 闸,换个 IP 再撞全局闸;名额随 permit 析构归还,
    /// 计数归零的 IP 条目随之删除(map 不会被一次性的来源撑大)。
    #[test]
    fn admit_enforces_global_and_per_ip_limits_and_releases_on_drop() {
        let state = RelayState::new().with_limits(RelayLimits {
            max_connections: 3,
            max_connections_per_ip: 2,
            ..RelayLimits::default()
        });
        let (a, b) = (ip("203.0.113.1"), ip("203.0.113.2"));

        let p1 = state.admit(a).unwrap();
        let p2 = state.admit(a).unwrap();
        assert_eq!(state.admit(a).err(), Some(AdmitError::PerIp));
        let p3 = state.admit(b).unwrap();
        assert_eq!(state.admit(b).err(), Some(AdmitError::Global));

        drop(p1);
        let p4 = state.admit(a).expect("析构后名额应归还");
        drop((p2, p3, p4));

        let counts = state.conns.lock().unwrap();
        assert_eq!(counts.total, 0);
        assert!(counts.per_ip.is_empty(), "归零的 IP 条目应删除");
    }

    /// 0 不是有意的配置(等于拒绝一切连接 / 建不出队列),按 1 处理。
    #[test]
    fn zero_limits_are_clamped_to_one() {
        let state = RelayState::new().with_limits(RelayLimits {
            max_connections: 0,
            max_connections_per_ip: 0,
            client_ip_header: None,
            outbound_queue: 0,
        });
        let limits = state.limits();
        assert_eq!(
            (
                limits.max_connections,
                limits.max_connections_per_ip,
                limits.outbound_queue
            ),
            (1, 1, 1)
        );
        let _only = state.admit(ip("198.51.100.1")).unwrap();
    }

    /// 计数用的客户端地址:没配头名时只认对端地址(头里写什么都不理);配了取头的
    /// 最后一段;头缺失 / 不是 IP 时回落对端;两样都没有归 0.0.0.0 桶。
    #[test]
    fn client_ip_uses_configured_header_last_hop_then_peer() {
        let peer: SocketAddr = "10.0.0.9:5555".parse().unwrap();
        let mut limits = RelayLimits::default();
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", "198.51.100.7".parse().unwrap());
        assert_eq!(client_ip(&limits, &headers, Some(peer)), peer.ip());

        limits.client_ip_header = Some(HeaderName::from_static("cf-connecting-ip"));
        assert_eq!(client_ip(&limits, &headers, Some(peer)), ip("198.51.100.7"));

        headers.insert("cf-connecting-ip", "1.1.1.1, 198.51.100.8".parse().unwrap());
        assert_eq!(client_ip(&limits, &headers, Some(peer)), ip("198.51.100.8"));

        headers.insert("cf-connecting-ip", "garbage".parse().unwrap());
        assert_eq!(client_ip(&limits, &headers, Some(peer)), peer.ip());
        assert_eq!(
            client_ip(&limits, &HeaderMap::new(), None),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    /// 出站队列有界:满了不再增长,而是发出溢出信号让连接自行断开。
    #[tokio::test]
    async fn full_outbound_queue_signals_overflow_instead_of_growing() {
        let (tx, rx) = mpsc::channel(2);
        let overflow = Arc::new(Notify::new());
        let slot = ConnSlot {
            generation: 1,
            tx,
            overflow: overflow.clone(),
        };
        slot.send(Message::Text("a".into()));
        slot.send(Message::Text("b".into()));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), overflow.notified())
                .await
                .is_err(),
            "未满之前不该报溢出"
        );

        slot.send(Message::Text("c".into()));
        tokio::time::timeout(Duration::from_secs(1), overflow.notified())
            .await
            .expect("队列满时应发出溢出信号");
        assert_eq!(rx.len(), 2, "满了之后的帧不进队列");
    }
}
