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

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Extension, Router};
use mt_relay_protocol::{
    AssistFailReason, CommandFailReason, DesktopRejectReason, DesktopToRelay, HistoryFailReason,
    MobileRejectReason, MobileToRelay, RelayToDesktop, RelayToMobile, StartSessionFailReason,
    PROTOCOL_VERSION,
};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, Notify};

/// 握手超时:连上后必须在此时限内送达 hello,否则直接断开。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 服务端心跳发包周期:保活穿过反代 / NAT,并主动探测已静默断连的对端
/// (尤其是浏览器 PWA 无法自行发送 RFC 6455 Ping 控制帧,需由服务端发起 Ping)。
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(30);

/// 服务端读空闲超时:超过此时长未收到对端任何有效帧(含 Pong)即断开,释放连接名额与房间槽位。
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

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

/// 缺省桌面端槽位 id:兼容未携带 `desktopId` 的旧版桌面端。
pub const DEFAULT_DESKTOP_ID: &str = "default";

/// 桌面端标识最大长度(ASCII 字母/数字/`-`/`_`)。
const MAX_DESKTOP_ID_LEN: usize = 64;

/// 桌面端展示名字符数上限。
const MAX_DESKTOP_NAME_CHARS: usize = 64;

/// 全局并发 WebSocket 连接数上限的默认值(`RELAY_MAX_CONNECTIONS`)。
///
/// 单个中转支持接入多台桌面端(每台桌面端各自最多绑定一部手机):64 足以支撑
/// 二三十台桌面端同时在线与重连瞬时冗余,又把「每条连接先攒一条最大消息」的
/// 最坏内存压在 64 × 16 MiB = 1 GiB 以内。
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// 单个客户端 IP 并发连接数上限的默认值(`RELAY_MAX_CONNECTIONS_PER_IP`)。
///
/// ⚠️ 反代后面部署时必须配 [`RelayLimits::client_ip_header`],否则所有连接的对端地址
/// 都是反代自己,这道闸就退化成第二道全局闸(16 条仍够多台电脑日常使用,只是挡不住
/// 单一来源占满全局名额)。
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 16;

/// 中转同时维护的桌面端房间数默认上限(`RELAY_MAX_DESKTOPS`)。
/// 只有通过 `MT_RELAY_DESKTOP_KEY` 鉴权的桌面端才能建房,未配对且离线的空房在
/// 断开时立即回收;64 远高于个人多机用量,同时防止异常客户端无限建房撑爆内存。
pub const DEFAULT_MAX_DESKTOPS: usize = 64;

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
    /// 同时维护的桌面端房间数上限(活跃在线或已配对/有未过期配对码的桌面端)
    pub max_desktops: usize,
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
            max_desktops: DEFAULT_MAX_DESKTOPS,
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

    fn send_json<T: serde::Serialize>(&self, msg: &T) {
        if let Some(frame) = to_text(msg) {
            self.send(frame);
        }
    }
}

/// 获取互斥锁:单次任务异常导致锁中毒时照常恢复内部状态,防止单点 panic
/// 演化为整台中转服务永久不可用(与 [`ConnPermit::drop`] 同一口径)。
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        tracing::error!("[relay] mutex poisoned, recovering inner state");
        poisoned.into_inner()
    })
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
    tracing::warn!(
        "[relay] {side} outbound queue full (gen {generation}): slow consumer, disconnecting"
    );
}

struct PairingCode {
    code: String,
    issued_at: Instant,
}

/// 一台桌面端的独立房间:同一时刻至多一条桌面连接 + 至多一条移动端连接 + 1 份有效凭证。
/// 多台桌面端共享同一个中转实例时,各房间完全隔离(issue #106)。
#[derive(Default)]
struct DesktopRoom {
    desktop: Option<ConnSlot>,
    /// 桌面端最近一次握手携带的展示名(如主机名或自定义名称)
    desktop_name: Option<String>,
    mobile: Option<ConnSlot>,
    /// 待兑换的一次性配对码(签发新码/兑换成功/重置配对时作废)
    pairing_code: Option<PairingCode>,
    /// 当前有效的移动端长期凭证(每台桌面端最多 1 份:新配对生效即顶替旧手机)
    credential: Option<String>,
    /// 移动端当前订阅的镜像 pane 集合;未订阅 pane 的镜像消息在路由层丢弃。
    /// 只存 pane id(元数据),不缓存镜像内容。
    subscriptions: HashSet<String>,
    /// 移动端当前订阅的终端屏幕 pane 集合;仅切入终端视图时按需订阅,未订阅一律丢弃。
    terminal_subscriptions: HashSet<String>,
}

#[derive(Default)]
struct Inner {
    /// `desktop_id` → 该桌面端的独立房间
    rooms: HashMap<String, DesktopRoom>,
    /// 活跃一次性配对码 → 所属 `desktop_id`
    pairing_codes: HashMap<String, String>,
    /// 活跃移动端长期凭证 → 所属 `desktop_id`
    credentials: HashMap<String, String>,
}

impl Inner {
    fn clear_pairing_code(&mut self, desktop_id: &str) {
        if let Some(room) = self.rooms.get_mut(desktop_id) {
            if let Some(old) = room.pairing_code.take() {
                self.pairing_codes.remove(&old.code);
            }
        }
    }

    fn set_pairing_code(&mut self, desktop_id: &str, code: String) {
        self.clear_pairing_code(desktop_id);
        let room = self.rooms.entry(desktop_id.to_string()).or_default();
        room.pairing_code = Some(PairingCode {
            code: code.clone(),
            issued_at: Instant::now(),
        });
        self.pairing_codes.insert(code, desktop_id.to_string());
    }

    fn clear_credential(&mut self, desktop_id: &str) {
        if let Some(room) = self.rooms.get_mut(desktop_id) {
            if let Some(old) = room.credential.take() {
                self.credentials.remove(&old);
            }
        }
    }

    fn set_credential(&mut self, desktop_id: &str, cred: String) {
        self.clear_credential(desktop_id);
        let room = self.rooms.entry(desktop_id.to_string()).or_default();
        room.credential = Some(cred.clone());
        self.credentials.insert(cred, desktop_id.to_string());
    }

    /// 清理指定房间的过期配对码;若房间彻底空置(无桌面、无手机、无凭证、无有效配对码)
    /// 则移除房间本身,防止离线未配对的历史 desktop_id 无限累积。
    fn gc_room_if_empty(&mut self, desktop_id: &str, code_ttl: Duration) {
        let expired_code = self
            .rooms
            .get(desktop_id)
            .and_then(|room| room.pairing_code.as_ref())
            .filter(|code| code.issued_at.elapsed() > code_ttl)
            .map(|code| code.code.clone());
        if let Some(code) = expired_code {
            self.pairing_codes.remove(&code);
            if let Some(room) = self.rooms.get_mut(desktop_id) {
                if room
                    .pairing_code
                    .as_ref()
                    .is_some_and(|active| active.code == code)
                {
                    room.pairing_code = None;
                }
            }
        }
        let is_empty = self.rooms.get(desktop_id).is_some_and(|room| {
            room.desktop.is_none()
                && room.mobile.is_none()
                && room.credential.is_none()
                && room.pairing_code.is_none()
        });
        if is_empty {
            self.rooms.remove(desktop_id);
        }
    }

    /// 扫描并回收所有已过期的空房间(建新房判上限前调用)。
    fn gc_empty_rooms(&mut self, code_ttl: Duration) {
        let ids: Vec<String> = self.rooms.keys().cloned().collect();
        for id in ids {
            self.gc_room_if_empty(&id, code_ttl);
        }
    }
}

/// 规范化桌面端标识:缺省或空白归一为 [`DEFAULT_DESKTOP_ID`] 兼容旧版桌面端;
/// 非空时校验长度与字符集(`[A-Za-z0-9_-]`,≤ 64 字节)。
fn normalize_desktop_id(raw: Option<&str>) -> Result<String, DesktopRejectReason> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_DESKTOP_ID.to_string());
    };
    if raw.len() > MAX_DESKTOP_ID_LEN
        || !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(DesktopRejectReason::InvalidDesktopId);
    }
    Ok(raw.to_string())
}

/// 收敛桌面端展示名:去首尾空白、过滤控制字符、限长;空串归一为 `None`。
fn sanitize_desktop_name(raw: Option<&str>) -> Option<String> {
    let cleaned: String = raw?
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DESKTOP_NAME_CHARS)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

#[derive(Clone)]
pub struct RelayState {
    inner: Arc<Mutex<Inner>>,
    generation_counter: Arc<AtomicU64>,
    code_ttl: Duration,
    /// 服务端心跳周期(定时向已握手连接发 Ping)
    ping_interval: Duration,
    /// 服务端读空闲超时(超过此时长未收到任何帧即断开)
    idle_timeout: Duration,
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
            ping_interval: DEFAULT_PING_INTERVAL,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            desktop_key: None,
            limits: Arc::new(RelayLimits::default()),
            conns: Arc::new(Mutex::new(ConnCounts::default())),
        }
    }

    /// 覆盖服务端心跳周期与读空闲超时(测试用;零时长按 1ms 钳位)。
    pub fn with_heartbeat(mut self, ping_interval: Duration, idle_timeout: Duration) -> Self {
        self.ping_interval = ping_interval.max(Duration::from_millis(1));
        self.idle_timeout = idle_timeout.max(Duration::from_millis(1));
        self
    }

    /// 覆盖连接数 / 出站队列上限(默认值见 [`RelayLimits::default`])。
    /// 容量类字段为 0 时按 1 处理:0 等于拒绝一切连接 / 建不出队列,不会是有意的配置。
    pub fn with_limits(mut self, limits: RelayLimits) -> Self {
        self.limits = Arc::new(RelayLimits {
            max_connections: limits.max_connections.max(1),
            max_connections_per_ip: limits.max_connections_per_ip.max(1),
            max_desktops: limits.max_desktops.max(1),
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
        let mut counts = lock_unpoisoned(&self.conns);
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
            AdmitError::Global => tracing::warn!(
                "[relay] {side} connection rejected: global limit ({}) reached",
                self.limits.max_connections
            ),
            AdmitError::PerIp => tracing::warn!(
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

fn to_text<T: serde::Serialize>(msg: &T) -> Option<Message> {
    match serde_json::to_string(msg) {
        Ok(json) => Some(Message::Text(json.into())),
        Err(err) => {
            tracing::error!("[relay] failed to serialize outbound message: {err}");
            None
        }
    }
}

async fn send_socket_json<T: serde::Serialize>(socket: &mut WebSocket, msg: &T) -> bool {
    let Some(frame) = to_text(msg) else {
        return false;
    };
    socket.send(frame).await.is_ok()
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

/// 桌面端连接生命周期:握手(版本 → 密钥 → 桌面端标识)→ 按房间注册(同 id 顶替旧连接)→ 消息循环 → 注销。
async fn handle_desktop(mut socket: WebSocket, state: RelayState) {
    // ── 握手:第一条消息必须是 hello,且版本匹配、密钥正确、desktop_id 合法 ──
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.recv()).await;
    let (actual_version, desktop_key, raw_desktop_id, raw_desktop_name) = match first {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<DesktopToRelay>(&text) {
            Ok(DesktopToRelay::Hello {
                protocol_version,
                desktop_key,
                desktop_id,
                desktop_name,
            }) => (protocol_version, desktop_key, desktop_id, desktop_name),
            _ => {
                tracing::warn!("[relay] desktop handshake failed: first message not hello");
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        },
        _ => {
            tracing::warn!("[relay] desktop handshake failed: timeout or non-text frame");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    // 校验顺序:版本 → 密钥 → 桌面端标识。版本对不上时后续字段语义本就不可信。
    if actual_version != PROTOCOL_VERSION {
        tracing::warn!(
            "[relay] desktop rejected: protocol version {actual_version} != {PROTOCOL_VERSION}"
        );
        let reject = RelayToDesktop::HelloReject {
            reason: DesktopRejectReason::VersionMismatch,
            expected_version: Some(PROTOCOL_VERSION),
            actual_version: Some(actual_version),
        };
        let _ = send_socket_json(&mut socket, &reject).await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    // 鉴权失败只记原因,绝不记密钥本身(中转日志纪律)
    if let Err(reason) = state.authenticate_desktop(&desktop_key) {
        tracing::warn!("[relay] desktop rejected: {reason:?}");
        let reject = RelayToDesktop::HelloReject {
            reason,
            expected_version: None,
            actual_version: None,
        };
        let _ = send_socket_json(&mut socket, &reject).await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    let desktop_id = match normalize_desktop_id(raw_desktop_id.as_deref()) {
        Ok(id) => id,
        Err(reason) => {
            tracing::warn!("[relay] desktop rejected: {reason:?}");
            let reject = RelayToDesktop::HelloReject {
                reason,
                expected_version: None,
                actual_version: None,
            };
            let _ = send_socket_json(&mut socket, &reject).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let desktop_name = sanitize_desktop_name(raw_desktop_name.as_deref());

    // ── 按房间注册:同 desktop_id 顶替自己的旧连接,不同 desktop_id 互不干扰 ──
    let generation = state.next_generation();
    let (tx, mut rx) = state.outbound_channel();
    let overflow = Arc::new(Notify::new());
    let registered: Result<bool, DesktopRejectReason> = {
        let mut inner = lock_unpoisoned(&state.inner);
        inner.gc_empty_rooms(state.code_ttl);
        if !inner.rooms.contains_key(&desktop_id) && inner.rooms.len() >= state.limits.max_desktops
        {
            Err(DesktopRejectReason::RoomLimitReached)
        } else {
            let room = inner.rooms.entry(desktop_id.clone()).or_default();
            let replaced = room.desktop.take();
            if let Some(old) = replaced.as_ref() {
                tracing::info!(
                    "[relay] desktop connection replaced (desktop {desktop_id}, gen {})",
                    old.generation
                );
                old.send(Message::Close(None));
            }
            let name_changed = room.desktop_name != desktop_name;
            room.desktop_name = desktop_name;
            room.desktop = Some(ConnSlot {
                generation,
                tx,
                overflow: overflow.clone(),
            });
            // presence:桌面端从离线转为在线,或桌面端改名重连时通知该房间的在线移动端
            if replaced.is_none() || name_changed {
                if let Some(mobile) = room.mobile.as_ref() {
                    mobile.send_json(&RelayToMobile::Presence {
                        desktop_online: true,
                        desktop_name: room.desktop_name.clone(),
                    });
                }
            }
            Ok(room.credential.is_some())
        }
    };
    let paired = match registered {
        Ok(paired) => paired,
        Err(reason) => {
            tracing::warn!(
                "[relay] desktop rejected: room limit ({}) reached",
                state.limits.max_desktops
            );
            let reject = RelayToDesktop::HelloReject {
                reason,
                expected_version: None,
                actual_version: None,
            };
            let _ = send_socket_json(&mut socket, &reject).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    tracing::info!("[relay] desktop connected (desktop {desktop_id}, gen {generation})");

    // 握手成功:ack + 当前配对状态
    let ack = RelayToDesktop::HelloAck {
        protocol_version: PROTOCOL_VERSION,
        supports_multi_desktop: true,
    };
    if !send_socket_json(&mut socket, &ack).await
        || !send_socket_json(&mut socket, &RelayToDesktop::PairingUpdate { paired }).await
    {
        deregister_desktop(&state, &desktop_id, generation);
        return;
    }

    // ── 消息循环(含服务端心跳与空闲超时探测) ──
    let mut ping_ticker = tokio::time::interval(state.ping_interval);
    ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 消费首次立即触发的 tick,等满一个周期再发第一帧 Ping
    ping_ticker.tick().await;
    let mut last_seen = tokio::time::Instant::now();

    loop {
        let idle_deadline = last_seen + state.idle_timeout;
        tokio::select! {
            // 出站队列溢出(慢消费者):断开,注销后由桌面端重连自愈
            _ = overflow.notified() => {
                log_overflow("desktop", generation);
                break;
            }
            // 读空闲超时:对端静默断连(NAT 半开等),主动断开释放名额与房间槽位
            _ = tokio::time::sleep_until(idle_deadline) => {
                tracing::warn!(
                    "[relay] desktop idle timeout ({:?}, desktop {desktop_id}, gen {generation}), disconnecting",
                    state.idle_timeout
                );
                break;
            }
            // 定时发 Ping 保活并探测对端存活;发送本身亦受 idle_deadline 约束防写缓冲卡死
            _ = ping_ticker.tick() => {
                match tokio::time::timeout_at(
                    idle_deadline,
                    send_or_overflow(
                        &mut socket,
                        Message::Ping(Default::default()),
                        &overflow,
                        "desktop",
                        generation,
                    ),
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(_) => {
                        tracing::warn!(
                            "[relay] desktop ping send timed out (desktop {desktop_id}, gen {generation}), disconnecting"
                        );
                        break;
                    }
                }
            }
            out = rx.recv() => match out {
                // 槽位持有者(顶替我们的新连接/未来的路由方)让我们发帧;Close 帧发完即退出
                Some(frame) => {
                    let is_close = matches!(frame, Message::Close(_));
                    let sent =
                        send_or_overflow(&mut socket, frame, &overflow, "desktop", generation)
                            .await;
                    if is_close {
                        tracing::info!(
                            "[relay] desktop disconnected (desktop {desktop_id}, gen {generation}, replaced)"
                        );
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
                    last_seen = tokio::time::Instant::now();
                    match serde_json::from_str::<DesktopToRelay>(&text) {
                        Ok(msg) => handle_desktop_message(&state, &desktop_id, generation, msg),
                        Err(_) => tracing::warn!("[relay] desktop sent unparseable message (ignored)"),
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {
                    // Pong / Ping / Binary 均证明对端仍存活
                    last_seen = tokio::time::Instant::now();
                }
                Some(Err(_)) => break,
            },
        }
    }

    deregister_desktop(&state, &desktop_id, generation);
    tracing::info!("[relay] desktop disconnected (desktop {desktop_id}, gen {generation})");
}

/// 处理握手后的桌面端业务消息(严格限定在所属 `desktop_id` 房间内)。
fn handle_desktop_message(
    state: &RelayState,
    desktop_id: &str,
    generation: u64,
    msg: DesktopToRelay,
) {
    let mut inner = lock_unpoisoned(&state.inner);
    let is_owner = inner
        .rooms
        .get(desktop_id)
        .and_then(|room| room.desktop.as_ref())
        .is_some_and(|slot| slot.generation == generation);
    if !is_owner {
        return;
    }

    match msg {
        DesktopToRelay::Hello { .. } => {} // 重复 hello 忽略
        // 结构快照/增量:只转发给该房间的在线移动端,中转不缓存不解析内容
        DesktopToRelay::SessionsSnapshot {
            projects,
            launchers,
            supports_history,
            supports_terminal_view,
            supports_input_assist,
        } => {
            if let Some(mobile) = inner
                .rooms
                .get(desktop_id)
                .and_then(|room| room.mobile.as_ref())
            {
                mobile.send_json(&RelayToMobile::SessionsSnapshot {
                    projects,
                    launchers,
                    supports_history,
                    supports_terminal_view,
                    supports_input_assist,
                });
            }
        }
        DesktopToRelay::SessionsDelta {
            upserts,
            removed_project_ids,
        } => {
            if let Some(mobile) = inner
                .rooms
                .get(desktop_id)
                .and_then(|room| room.mobile.as_ref())
            {
                mobile.send_json(&RelayToMobile::SessionsDelta {
                    upserts,
                    removed_project_ids,
                });
            }
        }
        // 镜像消息:仅路由给该房间已订阅该 pane 的移动端,未订阅一律丢弃
        DesktopToRelay::MirrorSnapshot {
            pane_id,
            messages,
            has_more,
        } => {
            if let Some(room) = inner.rooms.get(desktop_id) {
                if room.subscriptions.contains(&pane_id) {
                    if let Some(mobile) = room.mobile.as_ref() {
                        mobile.send_json(&RelayToMobile::MirrorSnapshot {
                            pane_id,
                            messages,
                            has_more,
                        });
                    }
                }
            }
        }
        DesktopToRelay::MirrorAppend { pane_id, messages } => {
            if let Some(room) = inner.rooms.get(desktop_id) {
                if room.subscriptions.contains(&pane_id) {
                    if let Some(mobile) = room.mobile.as_ref() {
                        mobile.send_json(&RelayToMobile::MirrorAppend { pane_id, messages });
                    }
                }
            }
        }
        DesktopToRelay::MirrorHistory {
            pane_id,
            messages,
            has_more,
        } => {
            if let Some(room) = inner.rooms.get(desktop_id) {
                if room.subscriptions.contains(&pane_id) {
                    if let Some(mobile) = room.mobile.as_ref() {
                        mobile.send_json(&RelayToMobile::MirrorHistory {
                            pane_id,
                            messages,
                            has_more,
                        });
                    }
                }
            }
        }
        // pane 关闭:转发并清掉镜像与终端两类订阅(后续同 pane 消息不再路由)
        DesktopToRelay::PaneClosed { pane_id } => {
            if let Some(room) = inner.rooms.get_mut(desktop_id) {
                let had_mirror = room.subscriptions.remove(&pane_id);
                let had_term = room.terminal_subscriptions.remove(&pane_id);
                if had_mirror || had_term {
                    if let Some(mobile) = room.mobile.as_ref() {
                        mobile.send_json(&RelayToMobile::PaneClosed { pane_id });
                    }
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
            if let Some(mobile) = inner
                .rooms
                .get(desktop_id)
                .and_then(|room| room.mobile.as_ref())
            {
                mobile.send_json(&RelayToMobile::CommandReceipt {
                    pane_id,
                    command_id,
                    ok,
                    reason,
                });
            }
        }
        // 发起/恢复会话回执:原样转发(以 request_id 关联)
        DesktopToRelay::StartSessionReceipt {
            request_id,
            ok,
            pane_id,
            reason,
        } => {
            if let Some(mobile) = inner
                .rooms
                .get(desktop_id)
                .and_then(|room| room.mobile.as_ref())
            {
                mobile.send_json(&RelayToMobile::StartSessionReceipt {
                    request_id,
                    ok,
                    pane_id,
                    reason,
                });
            }
        }
        // 历史会话列表与正文响应:原样转发给在线移动端,中转不落盘不缓存
        DesktopToRelay::SessionHistory {
            request_id,
            project_id,
            ok,
            sessions,
            reason,
        } => {
            if let Some(mobile) = inner
                .rooms
                .get(desktop_id)
                .and_then(|room| room.mobile.as_ref())
            {
                mobile.send_json(&RelayToMobile::SessionHistory {
                    request_id,
                    project_id,
                    ok,
                    sessions,
                    reason,
                });
            }
        }
        DesktopToRelay::SessionContent {
            request_id,
            project_id,
            session_id,
            ok,
            messages,
            has_more,
            reason,
        } => {
            if let Some(mobile) = inner
                .rooms
                .get(desktop_id)
                .and_then(|room| room.mobile.as_ref())
            {
                mobile.send_json(&RelayToMobile::SessionContent {
                    request_id,
                    project_id,
                    session_id,
                    ok,
                    messages,
                    has_more,
                    reason,
                });
            }
        }
        // 终端屏幕快照:仅路由给已订阅该 pane 终端视图的移动端,未订阅一律丢弃,中转不缓存
        DesktopToRelay::TerminalScreen {
            pane_id,
            cols,
            rows,
            cursor_row,
            cursor_col,
            cursor_visible,
            alt_screen,
            lines,
        } => {
            if let Some(room) = inner.rooms.get(desktop_id) {
                if room.terminal_subscriptions.contains(&pane_id) {
                    if let Some(mobile) = room.mobile.as_ref() {
                        mobile.send_json(&RelayToMobile::TerminalScreen {
                            pane_id,
                            cols,
                            rows,
                            cursor_row,
                            cursor_col,
                            cursor_visible,
                            alt_screen,
                            lines,
                        });
                    }
                }
            }
        }
        // 输入辅助(`@` 文件列表 / `/` 技能列表):原样转发给在线移动端,不落盘不缓存
        DesktopToRelay::ProjectFiles {
            request_id,
            pane_id,
            ok,
            files,
            truncated,
            reason,
        } => {
            if let Some(mobile) = inner
                .rooms
                .get(desktop_id)
                .and_then(|room| room.mobile.as_ref())
            {
                mobile.send_json(&RelayToMobile::ProjectFiles {
                    request_id,
                    pane_id,
                    ok,
                    files,
                    truncated,
                    reason,
                });
            }
        }
        DesktopToRelay::ProjectSkills {
            request_id,
            pane_id,
            ok,
            prefix,
            skills,
            reason,
        } => {
            if let Some(mobile) = inner
                .rooms
                .get(desktop_id)
                .and_then(|room| room.mobile.as_ref())
            {
                mobile.send_json(&RelayToMobile::ProjectSkills {
                    request_id,
                    pane_id,
                    ok,
                    prefix,
                    skills,
                    reason,
                });
            }
        }
        DesktopToRelay::RequestPairingCode => {
            let code = random_id();
            inner.set_pairing_code(desktop_id, code.clone());
            tracing::info!("[relay] pairing code issued (desktop {desktop_id})");
            if let Some(desktop) = inner
                .rooms
                .get(desktop_id)
                .and_then(|room| room.desktop.as_ref())
            {
                desktop.send_json(&RelayToDesktop::PairingCode { code });
            }
        }
        DesktopToRelay::ResetPairing => {
            inner.clear_pairing_code(desktop_id);
            inner.clear_credential(desktop_id);
            if let Some(room) = inner.rooms.get_mut(desktop_id) {
                if let Some(mobile) = room.mobile.take() {
                    mobile.send_json(&RelayToMobile::Revoked);
                    mobile.send(Message::Close(None));
                    drop_subscriptions(room);
                }
                tracing::info!("[relay] pairing reset: credential revoked (desktop {desktop_id})");
                if let Some(desktop) = room.desktop.as_ref() {
                    desktop.send_json(&RelayToDesktop::PairingUpdate { paired: false });
                }
            }
        }
    }
}

fn deregister_desktop(state: &RelayState, desktop_id: &str, generation: u64) {
    let mut inner = lock_unpoisoned(&state.inner);
    if let Some(room) = inner.rooms.get_mut(desktop_id) {
        if room
            .desktop
            .as_ref()
            .is_some_and(|s| s.generation == generation)
        {
            room.desktop = None;
            // presence:桌面端离线,立即推给该房间的在线移动端(离线横幅)
            if let Some(mobile) = room.mobile.as_ref() {
                mobile.send_json(&RelayToMobile::Presence {
                    desktop_online: false,
                    desktop_name: room.desktop_name.clone(),
                });
            }
        }
    }
    inner.gc_room_if_empty(desktop_id, state.code_ttl);
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

/// 移动端握手鉴权并原子注册到目标桌面端房间的结果。
enum MobileRegistration {
    Accepted {
        desktop_id: String,
        desktop_name: Option<String>,
        desktop_online: bool,
        issued_credential: Option<String>,
    },
    Rejected(MobileRejectReason),
}

/// 校验移动端握手并原子落盘配对状态与连接槽(单次持锁完成,不做 IO,无竞态窗口)。
fn authenticate_and_register_mobile(
    state: &RelayState,
    pairing_code: Option<String>,
    credential: Option<String>,
    slot: ConnSlot,
) -> MobileRegistration {
    let mut inner = lock_unpoisoned(&state.inner);
    if let Some(code) = pairing_code {
        let Some(desktop_id) = inner.pairing_codes.get(&code).cloned() else {
            return MobileRegistration::Rejected(MobileRejectReason::InvalidPairingCode);
        };
        let valid = inner
            .rooms
            .get(&desktop_id)
            .and_then(|room| room.pairing_code.as_ref())
            .is_some_and(|active| {
                active.code == code && active.issued_at.elapsed() <= state.code_ttl
            });
        if !valid {
            inner.clear_pairing_code(&desktop_id);
            inner.gc_room_if_empty(&desktop_id, state.code_ttl);
            return MobileRegistration::Rejected(MobileRejectReason::InvalidPairingCode);
        }
        // 兑换成功:配对码一次性作废;新凭证只顶替该桌面端的旧凭证(每台桌面端最多 1 部手机),
        // 踢掉该房间的旧移动端连接,不影响其他桌面端房间
        inner.clear_pairing_code(&desktop_id);
        let new_credential = random_id();
        inner.set_credential(&desktop_id, new_credential.clone());
        let room = inner.rooms.entry(desktop_id.clone()).or_default();
        if let Some(old) = room.mobile.take() {
            old.send_json(&RelayToMobile::Revoked);
            old.send(Message::Close(None));
            drop_subscriptions(room);
        }
        room.mobile = Some(slot);
        if let Some(desktop) = room.desktop.as_ref() {
            desktop.send_json(&RelayToDesktop::PairingUpdate { paired: true });
        }
        MobileRegistration::Accepted {
            desktop_name: room.desktop_name.clone(),
            desktop_online: room.desktop.is_some(),
            desktop_id,
            issued_credential: Some(new_credential),
        }
    } else if let Some(cred) = credential {
        let Some(desktop_id) = inner.credentials.get(&cred).cloned() else {
            return MobileRegistration::Rejected(MobileRejectReason::InvalidCredential);
        };
        let Some(room) = inner.rooms.get_mut(&desktop_id) else {
            inner.credentials.remove(&cred);
            return MobileRegistration::Rejected(MobileRejectReason::InvalidCredential);
        };
        let matches_cred = room
            .credential
            .as_deref()
            .is_some_and(|expected| secret_eq(expected, &cred));
        if !matches_cred {
            return MobileRegistration::Rejected(MobileRejectReason::InvalidCredential);
        }
        if let Some(old) = room.mobile.take() {
            tracing::info!(
                "[relay] mobile connection replaced (desktop {desktop_id}, gen {})",
                old.generation
            );
            old.send(Message::Close(None));
            // 旧连接的订阅对新连接无意义,清掉并通知桌面端停止镜像推送
            drop_subscriptions(room);
        }
        room.mobile = Some(slot);
        MobileRegistration::Accepted {
            desktop_name: room.desktop_name.clone(),
            desktop_online: room.desktop.is_some(),
            desktop_id,
            issued_credential: None,
        }
    } else {
        MobileRegistration::Rejected(MobileRejectReason::MissingAuth)
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
                tracing::warn!("[relay] mobile handshake failed: first message not hello");
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        },
        _ => {
            tracing::warn!("[relay] mobile handshake failed: timeout or non-text frame");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    if version != PROTOCOL_VERSION {
        tracing::warn!("[relay] mobile rejected: protocol version {version} != {PROTOCOL_VERSION}");
        let _ = send_socket_json(
            &mut socket,
            &RelayToMobile::HelloReject {
                reason: MobileRejectReason::VersionMismatch,
            },
        )
        .await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    let generation = state.next_generation();
    let (tx, mut rx) = state.outbound_channel();
    let overflow = Arc::new(Notify::new());
    let slot = ConnSlot {
        generation,
        tx,
        overflow: overflow.clone(),
    };
    let (desktop_id, desktop_name, desktop_online, issued_credential) =
        match authenticate_and_register_mobile(&state, pairing_code, credential, slot) {
            MobileRegistration::Rejected(reason) => {
                tracing::warn!("[relay] mobile rejected: {reason:?}");
                let _ = send_socket_json(&mut socket, &RelayToMobile::HelloReject { reason }).await;
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            MobileRegistration::Accepted {
                desktop_id,
                desktop_name,
                desktop_online,
                issued_credential,
            } => (desktop_id, desktop_name, desktop_online, issued_credential),
        };
    tracing::info!("[relay] mobile connected (desktop {desktop_id}, gen {generation})");

    // 握手成功:ack + 当前桌面端 presence;桌面端在线则请它回发最新结构快照
    let ack = RelayToMobile::HelloAck {
        protocol_version: PROTOCOL_VERSION,
        credential: issued_credential,
        desktop_id: Some(desktop_id.clone()),
        desktop_name: desktop_name.clone(),
    };
    if !send_socket_json(&mut socket, &ack).await
        || !send_socket_json(
            &mut socket,
            &RelayToMobile::Presence {
                desktop_online,
                desktop_name,
            },
        )
        .await
    {
        deregister_mobile(&state, &desktop_id, generation);
        return;
    }
    if desktop_online {
        let inner = lock_unpoisoned(&state.inner);
        if let Some(desktop) = inner
            .rooms
            .get(&desktop_id)
            .and_then(|room| room.desktop.as_ref())
        {
            desktop.send_json(&RelayToDesktop::SessionsSnapshotRequest);
        }
    }

    // ── 消息循环(含服务端心跳与空闲超时探测) ──
    let mut ping_ticker = tokio::time::interval(state.ping_interval);
    ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping_ticker.tick().await;
    let mut last_seen = tokio::time::Instant::now();

    loop {
        let idle_deadline = last_seen + state.idle_timeout;
        tokio::select! {
            // 出站队列溢出(弱网 / 后台挂起的手机):断开,重连后握手 + 快照自愈
            _ = overflow.notified() => {
                log_overflow("mobile", generation);
                break;
            }
            // 读空闲超时:手机切后台挂死 / 断网无 FIN 时主动断开,释放订阅与名额
            _ = tokio::time::sleep_until(idle_deadline) => {
                tracing::warn!(
                    "[relay] mobile idle timeout ({:?}, desktop {desktop_id}, gen {generation}), disconnecting",
                    state.idle_timeout
                );
                break;
            }
            // 定时发 Ping 保活并探测存活(浏览器自动回 Pong)
            _ = ping_ticker.tick() => {
                match tokio::time::timeout_at(
                    idle_deadline,
                    send_or_overflow(
                        &mut socket,
                        Message::Ping(Default::default()),
                        &overflow,
                        "mobile",
                        generation,
                    ),
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(_) => {
                        tracing::warn!(
                            "[relay] mobile ping send timed out (desktop {desktop_id}, gen {generation}), disconnecting"
                        );
                        break;
                    }
                }
            }
            out = rx.recv() => match out {
                Some(frame) => {
                    let is_close = matches!(frame, Message::Close(_));
                    let sent =
                        send_or_overflow(&mut socket, frame, &overflow, "mobile", generation)
                            .await;
                    if is_close {
                        tracing::info!(
                            "[relay] mobile disconnected (desktop {desktop_id}, gen {generation}, kicked)"
                        );
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
                    last_seen = tokio::time::Instant::now();
                    match serde_json::from_str::<MobileToRelay>(&text) {
                        Ok(msg) => handle_mobile_message(&state, &desktop_id, generation, msg),
                        Err(_) => tracing::warn!("[relay] mobile sent unparseable message (ignored)"),
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {
                    // Pong / Ping / Binary 均证明对端仍存活
                    last_seen = tokio::time::Instant::now();
                }
                Some(Err(_)) => break,
            },
        }
    }

    deregister_mobile(&state, &desktop_id, generation);
    tracing::info!("[relay] mobile disconnected (desktop {desktop_id}, gen {generation})");
}

/// 处理握手后的移动端业务消息:订阅登记 + 转发给所属房间的桌面端。
fn handle_mobile_message(
    state: &RelayState,
    desktop_id: &str,
    generation: u64,
    msg: MobileToRelay,
) {
    let mut inner = lock_unpoisoned(&state.inner);
    let Some(room) = inner.rooms.get_mut(desktop_id) else {
        return;
    };
    if !room
        .mobile
        .as_ref()
        .is_some_and(|slot| slot.generation == generation)
    {
        return;
    }

    let forward = match msg {
        MobileToRelay::Hello { .. } => None, // 重复 hello 忽略
        MobileToRelay::SubscribePane { pane_id } => {
            room.subscriptions.insert(pane_id.clone());
            Some(RelayToDesktop::SubscribePane { pane_id })
        }
        MobileToRelay::UnsubscribePane { pane_id } => {
            room.subscriptions.remove(&pane_id);
            Some(RelayToDesktop::UnsubscribePane { pane_id })
        }
        // 先 let 再 then_some:「解构模式 => 方法链」这一形状 rustfmt 排不了,
        // 会把整个 match 退成 `let forward =\n match` 的回退排版
        MobileToRelay::RequestMirrorHistory {
            pane_id,
            before_seq,
        } => {
            let subscribed = room.subscriptions.contains(&pane_id);
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
            if room.desktop.is_some() {
                Some(RelayToDesktop::MobileCommand {
                    pane_id,
                    command_id,
                    text,
                })
            } else {
                reject_command_offline(room, "mobile command", pane_id, command_id);
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
            if room.desktop.is_some() {
                Some(RelayToDesktop::AnswerQuestion {
                    pane_id,
                    command_id,
                    seq,
                    question_id,
                    question_index,
                    option_index,
                })
            } else {
                reject_command_offline(room, "answer question", pane_id, command_id);
                None
            }
        }
        // 重命名会话:桌面端离线就丢弃。无回执通道——改没改成看结构增量回不回新
        // title,离线时手机侧本来就看得到「桌面端离线」横幅
        MobileToRelay::RenamePane { pane_id, title } => room
            .desktop
            .is_some()
            .then_some(RelayToDesktop::RenamePane { pane_id, title }),
        // 发起新 AI 会话:同样离线即拒(桌面离线意味着起不来,补送没有意义)
        MobileToRelay::StartAiSession {
            request_id,
            project_id,
            launcher_id,
        } => {
            if room.desktop.is_some() {
                Some(RelayToDesktop::StartAiSession {
                    request_id,
                    project_id,
                    launcher_id,
                })
            } else {
                reject_start_offline(room, "start ai session", request_id);
                None
            }
        }
        // 拉取历史会话列表:离线即拒,不让移动端空等超时
        MobileToRelay::RequestSessionHistory {
            request_id,
            project_id,
        } => {
            if room.desktop.is_some() {
                Some(RelayToDesktop::RequestSessionHistory {
                    request_id,
                    project_id,
                })
            } else {
                tracing::warn!("[relay] request session history rejected: desktop offline");
                if let Some(mobile) = room.mobile.as_ref() {
                    mobile.send_json(&RelayToMobile::SessionHistory {
                        request_id,
                        project_id,
                        ok: false,
                        sessions: Vec::new(),
                        reason: Some(HistoryFailReason::DesktopOffline),
                    });
                }
                None
            }
        }
        // 拉取单条历史会话正文:离线即拒
        MobileToRelay::RequestSessionContent {
            request_id,
            project_id,
            agent,
            session_id,
            before_seq,
        } => {
            if room.desktop.is_some() {
                Some(RelayToDesktop::RequestSessionContent {
                    request_id,
                    project_id,
                    agent,
                    session_id,
                    before_seq,
                })
            } else {
                tracing::warn!("[relay] request session content rejected: desktop offline");
                if let Some(mobile) = room.mobile.as_ref() {
                    mobile.send_json(&RelayToMobile::SessionContent {
                        request_id,
                        project_id,
                        session_id,
                        ok: false,
                        messages: Vec::new(),
                        has_more: false,
                        reason: Some(HistoryFailReason::DesktopOffline),
                    });
                }
                None
            }
        }
        // 基于历史会话继续对话:与发起新会话同一回执通道,离线即拒
        MobileToRelay::ResumeAiSession {
            request_id,
            project_id,
            agent,
            session_id,
        } => {
            if room.desktop.is_some() {
                Some(RelayToDesktop::ResumeAiSession {
                    request_id,
                    project_id,
                    agent,
                    session_id,
                })
            } else {
                reject_start_offline(room, "resume ai session", request_id);
                None
            }
        }
        // 终端视图按需订阅 / 退订
        MobileToRelay::SubscribeTerminal { pane_id } => {
            room.terminal_subscriptions.insert(pane_id.clone());
            Some(RelayToDesktop::SubscribeTerminal { pane_id })
        }
        MobileToRelay::UnsubscribeTerminal { pane_id } => {
            room.terminal_subscriptions.remove(&pane_id);
            Some(RelayToDesktop::UnsubscribeTerminal { pane_id })
        }
        // 应急特殊按键:桌面离线即拒,回执复用 CommandReceipt
        MobileToRelay::SendTerminalKey {
            pane_id,
            command_id,
            key,
        } => {
            if room.desktop.is_some() {
                Some(RelayToDesktop::SendTerminalKey {
                    pane_id,
                    command_id,
                    key,
                })
            } else {
                reject_command_offline(room, "terminal key", pane_id, command_id);
                None
            }
        }
        // 输入辅助(`@` 文件 / `/` 技能):桌面离线直接路由层返回失败,不让移动端空等
        MobileToRelay::RequestProjectFiles {
            request_id,
            pane_id,
            query,
        } => {
            if room.desktop.is_some() {
                Some(RelayToDesktop::RequestProjectFiles {
                    request_id,
                    pane_id,
                    query,
                })
            } else {
                tracing::warn!("[relay] request project files rejected: desktop offline");
                if let Some(mobile) = room.mobile.as_ref() {
                    mobile.send_json(&RelayToMobile::ProjectFiles {
                        request_id,
                        pane_id,
                        ok: false,
                        files: Vec::new(),
                        truncated: false,
                        reason: Some(AssistFailReason::DesktopOffline),
                    });
                }
                None
            }
        }
        MobileToRelay::RequestProjectSkills {
            request_id,
            pane_id,
        } => {
            if room.desktop.is_some() {
                Some(RelayToDesktop::RequestProjectSkills {
                    request_id,
                    pane_id,
                })
            } else {
                tracing::warn!("[relay] request project skills rejected: desktop offline");
                if let Some(mobile) = room.mobile.as_ref() {
                    mobile.send_json(&RelayToMobile::ProjectSkills {
                        request_id,
                        pane_id,
                        ok: false,
                        prefix: String::new(),
                        skills: Vec::new(),
                        reason: Some(AssistFailReason::DesktopOffline),
                    });
                }
                None
            }
        }
    };
    if let (Some(msg), Some(desktop)) = (forward, room.desktop.as_ref()) {
        desktop.send_json(&msg);
    }
}

/// 发起/恢复会话在桌面端离线时的路由层拒绝:复用 StartSessionReceipt 通道。
fn reject_start_offline(room: &DesktopRoom, what: &str, request_id: String) {
    tracing::warn!("[relay] {what} rejected: desktop offline");
    if let Some(mobile) = room.mobile.as_ref() {
        mobile.send_json(&RelayToMobile::StartSessionReceipt {
            request_id,
            ok: false,
            pane_id: None,
            reason: Some(StartSessionFailReason::DesktopOffline),
        });
    }
}

/// 桌面端离线时的路由层拒绝:直接给移动端回失败的指令回执(不做存储转发)。
/// 移动端指令与点选作答共用——两者的回执都是 CommandReceipt。
fn reject_command_offline(room: &DesktopRoom, what: &str, pane_id: String, command_id: String) {
    tracing::warn!("[relay] {what} rejected: desktop offline");
    if let Some(mobile) = room.mobile.as_ref() {
        mobile.send_json(&RelayToMobile::CommandReceipt {
            pane_id,
            command_id,
            ok: false,
            reason: Some(CommandFailReason::DesktopOffline),
        });
    }
}

/// 清空移动端订阅并逐一通知所属桌面端退订(移动端断线/被顶替/被吊销时调用)。
fn drop_subscriptions(room: &mut DesktopRoom) {
    let panes: Vec<String> = room.subscriptions.drain().collect();
    let term_panes: Vec<String> = room.terminal_subscriptions.drain().collect();
    if let Some(desktop) = room.desktop.as_ref() {
        for pane_id in panes {
            desktop.send_json(&RelayToDesktop::UnsubscribePane { pane_id });
        }
        for pane_id in term_panes {
            desktop.send_json(&RelayToDesktop::UnsubscribeTerminal { pane_id });
        }
    }
}

fn deregister_mobile(state: &RelayState, desktop_id: &str, generation: u64) {
    let mut inner = lock_unpoisoned(&state.inner);
    if let Some(room) = inner.rooms.get_mut(desktop_id) {
        if room
            .mobile
            .as_ref()
            .is_some_and(|s| s.generation == generation)
        {
            room.mobile = None;
            drop_subscriptions(room);
        }
    }
    inner.gc_room_if_empty(desktop_id, state.code_ttl);
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

    /// 任一持锁路径 panic 造成锁中毒后,中转的接纳计数与内部状态仍可恢复,不连锁瘫痪。
    #[test]
    fn poisoned_mutex_recovers_without_panicking() {
        let state = RelayState::new();
        let conns = state.conns.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = conns.lock().unwrap();
            panic!("simulate panic while holding conns lock");
        }));
        assert!(state.conns.is_poisoned());
        let permit = state
            .admit(ip("203.0.113.9"))
            .expect("锁中毒后仍应正常接纳连接");
        drop(permit);

        let inner = state.inner.clone();
        {
            let (tx, _rx) = state.outbound_channel();
            inner.lock().unwrap().rooms.insert(
                DEFAULT_DESKTOP_ID.to_string(),
                DesktopRoom {
                    desktop: Some(ConnSlot {
                        generation: 1,
                        tx,
                        overflow: Arc::new(Notify::new()),
                    }),
                    ..DesktopRoom::default()
                },
            );
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = inner.lock().unwrap();
            panic!("simulate panic while holding inner lock");
        }));
        assert!(state.inner.is_poisoned());
        handle_desktop_message(
            &state,
            DEFAULT_DESKTOP_ID,
            1,
            DesktopToRelay::RequestPairingCode,
        );
        assert!(lock_unpoisoned(&state.inner)
            .rooms
            .get(DEFAULT_DESKTOP_ID)
            .and_then(|r| r.pairing_code.as_ref())
            .is_some());
    }

    /// 0 不是有意的配置(等于拒绝一切连接 / 建不出队列),按 1 处理。
    #[test]
    fn zero_limits_are_clamped_to_one() {
        let state = RelayState::new().with_limits(RelayLimits {
            max_connections: 0,
            max_connections_per_ip: 0,
            max_desktops: 0,
            client_ip_header: None,
            outbound_queue: 0,
        });
        let limits = state.limits();
        assert_eq!(
            (
                limits.max_connections,
                limits.max_connections_per_ip,
                limits.max_desktops,
                limits.outbound_queue
            ),
            (1, 1, 1, 1)
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
