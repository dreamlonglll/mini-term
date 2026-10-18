//! 建连传输层:DNS 解析 + 逐地址限时的 TCP 连接、可掐断的传输流、算法偏好,
//! 以及建连阶段记录(超时报错要指出卡在哪一步)。
//!
//! # 为什么传输流要「可掐断」
//!
//! russh 的 `client::connect_stream` 读到服务器标识串后就把会话任务 spawn 出去,
//! 自己只等「密钥交换完成」的信号。外层超时丢掉这个 future 时,那个任务**不会**跟着
//! 结束(russh 0.61 `client/mod.rs` 的 `run_inner`):
//!
//! - 密钥交换进行中(`kex.active()`),它根本不轮询 Handle 的消息通道,察觉不到
//!   Handle 已经没了,只是一直等 socket 上的下一个包;
//! - keepalive 只在 `Authenticated` 状态才发。认证前那个计时器到点后什么也不做、
//!   也不重新上弦,于是一直处于「已到点」:`select!` 每轮都立刻命中它而空转,同一轮里
//!   inactivity 计时器又因为「这轮没发 keepalive」被重置 —— 永远等不到到点。所以
//!   `Config::inactivity_timeout` 在认证前这段兜不住(本 crate 不设它的原因见
//!   `pool.rs` 的 `handshake`)。
//!
//! 结果是一条泄漏的任务加一个永不关闭的 socket(黑洞服务器永远不会主动断开)。这里给
//! 传输流套一层 [`GuardedStream`]:建连失败 / 超时 / 会话销毁时 [`StreamKill::kill`]
//! 让它的读写立即报错,russh 任务随之从 `run_inner` 返回、关闭 socket 退出。

use std::borrow::Cow;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::task::AtomicWaker;
use russh::keys::Algorithm;
use russh::{cipher, kex, mac};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

// ============================================================================
// 可掐断的传输流
// ============================================================================

/// [`StreamKill`] 与 [`GuardedStream`] 共享的状态。
struct KillState {
    killed: AtomicBool,
    /// 读写两个方向各记一个 waker:russh 把流 `split` 成两半,分别在读循环与
    /// flush 里被 poll,只记一个会漏掉另一侧的唤醒。
    read_waker: AtomicWaker,
    write_waker: AtomicWaker,
    /// 已从对端读到的字节数。超时报错据此区分「服务器一个字节都没发」与
    /// 「发了标识串但密钥交换没完成」。
    bytes_read: AtomicU64,
}

/// 传输流的掐断开关。可克隆,任一克隆调 [`Self::kill`] 都生效。
#[derive(Clone)]
pub(crate) struct StreamKill(Arc<KillState>);

impl StreamKill {
    pub(crate) fn new() -> Self {
        Self(Arc::new(KillState {
            killed: AtomicBool::new(false),
            read_waker: AtomicWaker::new(),
            write_waker: AtomicWaker::new(),
            bytes_read: AtomicU64::new(0),
        }))
    }

    /// 包一层可掐断的流。同一个开关只该包一条流。
    pub(crate) fn wrap<S>(&self, inner: S) -> GuardedStream<S> {
        GuardedStream {
            inner,
            state: Arc::clone(&self.0),
        }
    }

    /// 掐断:此后读写一律立即报错,并唤醒正挂在读 / 写上的任务。幂等。
    pub(crate) fn kill(&self) {
        self.0.killed.store(true, Ordering::SeqCst);
        self.0.read_waker.wake();
        self.0.write_waker.wake();
    }

    #[cfg(test)]
    fn is_killed(&self) -> bool {
        self.0.killed.load(Ordering::SeqCst)
    }

    pub(crate) fn bytes_read(&self) -> u64 {
        self.0.bytes_read.load(Ordering::Relaxed)
    }
}

/// drop 即掐断。建连途中(future 被超时丢弃时局部变量随之 drop)与
/// `CachedSession` 里都靠它保证 russh 会话任务不会比持有者活得久。
pub(crate) struct KillOnDrop(StreamKill);

impl KillOnDrop {
    pub(crate) fn new(kill: StreamKill) -> Self {
        Self(kill)
    }

    pub(crate) fn kill(&self) {
        self.0.kill();
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.kill();
    }
}

/// 套在 TCP 流 / 跳板 direct-tcpip 通道流外面的可掐断包装,见模块注释。
pub(crate) struct GuardedStream<S> {
    inner: S,
    state: Arc<KillState>,
}

fn aborted_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "ssh transport closed locally (connect deadline, failure or session teardown)",
    )
}

impl<S: AsyncRead + Unpin> AsyncRead for GuardedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // 先登记 waker 再查标志:kill 若发生在两步之间,也一定能把本任务唤醒。
        this.state.read_waker.register(cx.waker());
        if this.state.killed.load(Ordering::SeqCst) {
            return Poll::Ready(Err(aborted_error()));
        }
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            let read = buf.filled().len().saturating_sub(before);
            this.state
                .bytes_read
                .fetch_add(read as u64, Ordering::Relaxed);
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for GuardedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.state.write_waker.register(cx.waker());
        if this.state.killed.load(Ordering::SeqCst) {
            return Poll::Ready(Err(aborted_error()));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.state.write_waker.register(cx.waker());
        if this.state.killed.load(Ordering::SeqCst) {
            return Poll::Ready(Err(aborted_error()));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.state.killed.load(Ordering::SeqCst) {
            // 已掐断就不必等对端:russh 收尾时的 shutdown 直接放行,任务立刻退出。
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

// ============================================================================
// 建连阶段记录
// ============================================================================

/// 建连走到了哪一步。外层超时触发时据此写出「卡在哪个阶段」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectStage {
    /// DNS 解析。
    Resolve,
    /// TCP 连接(逐个地址尝试)。
    TcpConnect,
    /// 经上一跳开 direct-tcpip 隧道(ProxyJump)。
    Tunnel,
    /// SSH 标识串交换 + 密钥交换(含主机密钥校验)。
    Handshake,
    /// 认证。
    Auth,
}

#[derive(Default)]
struct TrackerState {
    /// 当前这一跳的可读描述(`example.com:22` / `jump host bastion:22`)。
    hop: String,
    stage: Option<ConnectStage>,
    /// 当前阶段的补充说明(已试过的地址、正在等的认证方法……)。
    detail: Option<String>,
    /// 握手阶段盯着的传输流,超时时看它读到过多少字节。
    stream: Option<StreamKill>,
}

/// 建连进度。建连 future 被外层超时丢弃后仍要读它,所以单独放在 `Arc` 里共享。
#[derive(Default)]
pub(crate) struct StageTracker {
    state: Mutex<TrackerState>,
}

impl StageTracker {
    fn with<R>(&self, f: impl FnOnce(&mut TrackerState) -> R) -> R {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    /// 进入新阶段,清掉上一阶段的补充说明与盯着的流。
    pub(crate) fn enter(&self, hop: &str, stage: ConnectStage) {
        self.with(|state| {
            state.hop = hop.to_string();
            state.stage = Some(stage);
            state.detail = None;
            state.stream = None;
        });
    }

    pub(crate) fn set_detail(&self, detail: impl Into<String>) {
        let detail = detail.into();
        self.with(|state| state.detail = Some(detail));
    }

    pub(crate) fn watch_stream(&self, kill: &StreamKill) {
        let kill = kill.clone();
        self.with(|state| state.stream = Some(kill));
    }

    /// 拼超时错误。英文开头(agent 用 GBK 控制台读也不会乱码),写明预算、阶段与那一跳。
    pub(crate) fn describe_timeout(&self, target: &str, budget: Duration) -> String {
        self.with(|state| {
            let hop = state.hop.as_str();
            let what = match state.stage {
                None => "before the connection attempt started".to_string(),
                Some(ConnectStage::Resolve) => format!("during DNS resolution of {hop}"),
                Some(ConnectStage::TcpConnect) => format!("during TCP connect to {hop}"),
                Some(ConnectStage::Tunnel) => format!("while opening a tunnel to {hop}"),
                Some(ConnectStage::Handshake) => format!("during SSH handshake with {hop}"),
                Some(ConnectStage::Auth) => format!("during authentication on {hop}"),
            };
            let detail = match (state.stage, &state.stream, &state.detail) {
                (Some(ConnectStage::Handshake), Some(stream), _) => Some(if stream.bytes_read() == 0 {
                    "connected, but the server sent no SSH identification string".to_string()
                } else {
                    "the server sent its SSH identification string, but the key exchange did not complete"
                        .to_string()
                }),
                (_, _, Some(detail)) => Some(detail.clone()),
                _ => None,
            };
            let budget = fmt_duration(budget);
            match detail {
                Some(detail) => {
                    format!("ssh connection to {target} timed out after {budget} {what} ({detail})")
                }
                None => format!("ssh connection to {target} timed out after {budget} {what}"),
            }
        })
    }
}

/// 把时长写成 `20s` / `1.5s` / `300ms`,给错误文本用。
pub(crate) fn fmt_duration(d: Duration) -> String {
    if d.subsec_millis() == 0 && d >= Duration::from_secs(1) {
        format!("{}s", d.as_secs())
    } else if d < Duration::from_secs(1) {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

// ============================================================================
// DNS + 逐地址 TCP 连接
// ============================================================================

/// 第 `addresses_left` 个待试地址能用的时长:还有别的地址可试时最多 `per_address`,
/// 免得一个黑洞地址(典型是不通的 IPv6)吃光整段预算;最后一个地址没有退路,用掉剩余
/// 的全部预算。
pub(crate) fn address_timeout(
    remaining: Duration,
    per_address: Duration,
    addresses_left: usize,
) -> Duration {
    if addresses_left <= 1 {
        remaining
    } else {
        remaining.min(per_address)
    }
}

/// 全部地址都没连上。
#[derive(Debug)]
pub(crate) struct TcpAttempts {
    /// 每个地址的失败原因(`addr: reason`),按尝试顺序。
    pub(crate) notes: Vec<String>,
    /// 是否因为总预算耗尽才停下(而非每个地址都明确失败)。
    pub(crate) deadline_exceeded: bool,
}

/// 按顺序逐个地址去连,每个地址按 [`address_timeout`] 限时;连上即返回。
///
/// `connect` 可注入,单测用它模拟「黑洞地址 + 正常地址」而不碰真实网络。
/// 每失败一个地址就把进度写进 `tracker`,外层超时时报错能列出已试过的地址。
pub(crate) async fn connect_any<S, F, Fut>(
    addrs: &[SocketAddr],
    deadline: Instant,
    per_address: Duration,
    tracker: &StageTracker,
    mut connect: F,
) -> Result<S, TcpAttempts>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<S>>,
{
    let mut notes: Vec<String> = Vec::new();
    for (index, addr) in addrs.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TcpAttempts {
                notes,
                deadline_exceeded: true,
            });
        }
        let limit = address_timeout(remaining, per_address, addrs.len() - index);
        match tokio::time::timeout(limit, connect(*addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => notes.push(format!("{addr}: {e}")),
            Err(_) => notes.push(format!(
                "{addr}: no response within {}",
                fmt_duration(limit)
            )),
        }
        tracker.set_detail(notes.join("; "));
    }
    let deadline_exceeded = Instant::now() >= deadline;
    Err(TcpAttempts {
        notes,
        deadline_exceeded,
    })
}

/// TCP 建连失败的分类。
#[derive(Debug)]
pub(crate) enum TcpConnectError {
    /// DNS 解析失败或没有地址。
    Resolve(String),
    /// 每个地址都明确失败(拒绝、不可达……),预算还没用完。
    Failed(String),
    /// 总预算耗尽;调用方用 [`StageTracker::describe_timeout`] 出超时文案。
    DeadlineExceeded,
}

/// 解析 `host` 并按 [`connect_any`] 逐个地址连。
///
/// `host` 是 IP 字面量时不走 DNS;整串被方括号包住的 IPv6(`[::1]`)先去掉括号。
pub(crate) async fn connect_tcp(
    host: &str,
    port: u16,
    hop_label: &str,
    deadline: Instant,
    per_address: Duration,
    tracker: &StageTracker,
) -> Result<tokio::net::TcpStream, TcpConnectError> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    tracker.enter(hop_label, ConnectStage::Resolve);
    let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((bare, port)).await {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            return Err(TcpConnectError::Resolve(format!(
                "DNS resolution of {bare} failed: {e}"
            )));
        }
    };
    if addrs.is_empty() {
        return Err(TcpConnectError::Resolve(format!(
            "DNS resolution of {bare} returned no addresses"
        )));
    }
    tracker.enter(hop_label, ConnectStage::TcpConnect);
    match connect_any(
        &addrs,
        deadline,
        per_address,
        tracker,
        tokio::net::TcpStream::connect,
    )
    .await
    {
        Ok(stream) => Ok(stream),
        Err(attempts) if attempts.deadline_exceeded => Err(TcpConnectError::DeadlineExceeded),
        Err(attempts) => Err(TcpConnectError::Failed(format!(
            "TCP connect failed ({})",
            attempts.notes.join("; ")
        ))),
    }
}

// ============================================================================
// 算法偏好
// ============================================================================

/// russh 默认表里的「伪算法」(扩展协商标记),不是真的密钥交换算法。
const KEX_EXTENSIONS: [kex::Name; 4] = [
    kex::EXTENSION_SUPPORT_AS_CLIENT,
    kex::EXTENSION_SUPPORT_AS_SERVER,
    kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
    kex::EXTENSION_OPENSSH_STRICT_KEX_AS_SERVER,
];

/// 追加在 russh 默认密钥交换算法之后的兼容算法,按 OpenSSH 客户端默认顺序。
///
/// 系统 OpenSSH 客户端默认带 ECDH-NIST 三条,russh 默认没有:只有 `ecdh-sha2-nistp*`
/// 加上 SHA-1 系 DH 的老服务器、部分网络设备,会出现「终端能连、池报 No common Kex
/// algorithm」。故意**不加** `diffie-hellman-group1-sha1` / `group14-sha1` /
/// `group-exchange-sha1`:OpenSSH 默认也早已不带它们。
const COMPAT_KEX: [kex::Name; 3] = [
    kex::ECDH_SHA2_NISTP256,
    kex::ECDH_SHA2_NISTP384,
    kex::ECDH_SHA2_NISTP521,
];

/// 追加在默认对称加密算法之后的兼容算法。OpenSSH 默认有 `aes128-gcm@openssh.com`,
/// russh 默认只有 256 位 GCM。故意不加任何 CBC 与 3DES。
const COMPAT_CIPHERS: [cipher::Name; 1] = [cipher::AES_128_GCM];

/// 追加在默认 MAC 之后的兼容算法:只有 SHA-1 MAC 的老 OpenSSH / 网络设备。
/// AEAD 加密(chacha20-poly1305 / *-gcm)协商成功时根本用不到 MAC,这两条只在
/// 退到 `aes*-ctr` 时才可能被选中。
const COMPAT_MACS: [mac::Name; 2] = [mac::HMAC_SHA1_ETM, mac::HMAC_SHA1];

/// `base` 后面追加 `extra` 里尚未出现的项,保持各自原有顺序。
fn append_missing<T: Copy + PartialEq>(base: &[T], extra: &[T]) -> Vec<T> {
    let mut out = base.to_vec();
    for item in extra {
        if !out.contains(item) {
            out.push(*item);
        }
    }
    out
}

/// 本 crate 用的算法偏好:russh 默认表(强算法)在前,OpenSSH 客户端默认带、russh 默认
/// 没有的兼容算法按 OpenSSH 的顺序追加在后 —— 双方都支持强算法时协商结果不变,只有老
/// 服务器才会落到追加的那几条。
///
/// `host_keys` 是按 known_hosts 调过序的主机密钥算法(见
/// `known_hosts::preferred_host_key_algorithms`),`None` 沿用默认顺序。
pub(crate) fn preferred_algorithms(host_keys: Option<Vec<Algorithm>>) -> russh::Preferred {
    let base = russh::Preferred::default();
    let (mut kex_order, extensions): (Vec<kex::Name>, Vec<kex::Name>) = base
        .kex
        .iter()
        .copied()
        .partition(|name| !KEX_EXTENSIONS.contains(name));
    kex_order = append_missing(&kex_order, &COMPAT_KEX);
    // 扩展标记仍放最后:它们不参与真正的算法选择,位置只是保持与默认表一致。
    kex_order.extend(extensions);
    russh::Preferred {
        kex: Cow::Owned(kex_order),
        key: match host_keys {
            Some(order) => Cow::Owned(order),
            None => base.key.clone(),
        },
        cipher: Cow::Owned(append_missing(&base.cipher, &COMPAT_CIPHERS)),
        mac: Cow::Owned(append_missing(&base.mac, &COMPAT_MACS)),
        compression: base.compression.clone(),
    }
}

// ============================================================================
// tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn kill_wakes_a_pending_reader_and_fails_io() {
        let (client, mut server) = tokio::io::duplex(64);
        let kill = StreamKill::new();
        let mut guarded = kill.wrap(client);

        server.write_all(b"SSH-2.0-test\r\n").await.unwrap();
        let mut buf = [0u8; 14];
        guarded.read_exact(&mut buf).await.unwrap();
        assert_eq!(kill.bytes_read(), 14);

        // 对端不再发任何东西:读会一直挂着,直到被掐断。
        let reader = tokio::spawn(async move {
            let mut one = [0u8; 1];
            let result = guarded.read(&mut one).await;
            (result, guarded)
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!reader.is_finished(), "没掐断前读应一直挂着");
        kill.kill();
        let (result, mut guarded) = tokio::time::timeout(Duration::from_secs(2), reader)
            .await
            .expect("掐断必须唤醒挂着的读")
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert!(kill.is_killed());
        // 写同样立即失败,shutdown 直接放行。
        assert!(guarded.write_all(b"x").await.is_err());
        assert!(guarded.shutdown().await.is_ok());
        drop(server);
    }

    #[test]
    fn kill_on_drop_kills() {
        let kill = StreamKill::new();
        {
            let _guard = KillOnDrop::new(kill.clone());
            assert!(!kill.is_killed());
        }
        assert!(kill.is_killed());
    }

    #[test]
    fn address_timeout_caps_all_but_the_last_address() {
        let per = Duration::from_secs(5);
        assert_eq!(
            address_timeout(Duration::from_secs(20), per, 3),
            Duration::from_secs(5)
        );
        assert_eq!(
            address_timeout(Duration::from_secs(3), per, 2),
            Duration::from_secs(3),
            "剩余预算比单地址上限还少时以剩余为准"
        );
        assert_eq!(
            address_timeout(Duration::from_secs(20), per, 1),
            Duration::from_secs(20),
            "最后一个地址用掉全部剩余预算"
        );
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[tokio::test]
    async fn connect_any_skips_a_blackhole_address_within_its_share() {
        let tracker = StageTracker::default();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(5);
        let result = connect_any(
            &[addr(1), addr(2)],
            deadline,
            Duration::from_millis(80),
            &tracker,
            |a: SocketAddr| async move {
                if a.port() == 1 {
                    // 黑洞:永不返回。
                    std::future::pending::<io::Result<u16>>().await
                } else {
                    Ok(a.port())
                }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 2);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "黑洞地址只该吃掉它那一份(80ms),实际 {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn connect_any_reports_each_failure_and_the_deadline() {
        let tracker = StageTracker::default();
        tracker.enter("h:22", ConnectStage::TcpConnect);
        let deadline = Instant::now() + Duration::from_millis(150);
        let err = connect_any(
            &[addr(1), addr(2)],
            deadline,
            Duration::from_millis(50),
            &tracker,
            |a: SocketAddr| async move {
                if a.port() == 1 {
                    Err::<(), _>(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"))
                } else {
                    std::future::pending().await
                }
            },
        )
        .await
        .unwrap_err();
        assert!(err.deadline_exceeded);
        assert_eq!(err.notes.len(), 2);
        assert!(err.notes[0].contains("refused"), "{:?}", err.notes);
        assert!(err.notes[1].contains("no response"), "{:?}", err.notes);
        let message = tracker.describe_timeout("u@h:22", Duration::from_secs(20));
        assert!(
            message.starts_with(
                "ssh connection to u@h:22 timed out after 20s during TCP connect to h:22"
            ),
            "{message}"
        );
        assert!(message.contains("127.0.0.1:1: refused"), "{message}");
    }

    #[tokio::test]
    async fn connect_tcp_refused_port_fails_fast_without_timeout() {
        // 先占一个端口再放掉:该端口上没人监听,连接被立即拒绝。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let tracker = StageTracker::default();
        let started = Instant::now();
        let err = connect_tcp(
            "127.0.0.1",
            port,
            "127.0.0.1:x",
            Instant::now() + Duration::from_secs(10),
            Duration::from_secs(5),
            &tracker,
        )
        .await
        .unwrap_err();
        match err {
            TcpConnectError::Failed(message) => {
                assert!(message.starts_with("TCP connect failed ("), "{message}");
                assert!(message.contains(&format!("127.0.0.1:{port}")), "{message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn describe_timeout_names_each_stage() {
        let tracker = StageTracker::default();
        let budget = Duration::from_secs(20);
        let text = tracker.describe_timeout("u@h:22", budget);
        assert!(
            text.contains("before the connection attempt started"),
            "{text}"
        );

        tracker.enter("h:22", ConnectStage::Resolve);
        assert_eq!(
            tracker.describe_timeout("u@h:22", budget),
            "ssh connection to u@h:22 timed out after 20s during DNS resolution of h:22"
        );

        tracker.enter("h:22", ConnectStage::Handshake);
        let kill = StreamKill::new();
        tracker.watch_stream(&kill);
        let text = tracker.describe_timeout("u@h:22", budget);
        let expected = "during SSH handshake with h:22 \
                        (connected, but the server sent no SSH identification string)";
        assert!(text.ends_with(expected), "{text}");
        kill.0.bytes_read.store(20, Ordering::Relaxed);
        let text = tracker.describe_timeout("u@h:22", budget);
        assert!(text.contains("the key exchange did not complete"), "{text}");

        tracker.enter("jump host j:22", ConnectStage::Auth);
        tracker.set_detail("waiting for: password");
        assert_eq!(
            tracker.describe_timeout("u@h:22", budget),
            "ssh connection to u@h:22 timed out after 20s during authentication on jump host j:22 \
             (waiting for: password)"
        );

        tracker.enter("h:22", ConnectStage::Tunnel);
        let text = tracker.describe_timeout("u@h:22", Duration::from_millis(1500));
        assert!(
            text.contains("timed out after 1.5s while opening a tunnel to h:22"),
            "{text}"
        );
    }

    #[test]
    fn fmt_duration_formats() {
        assert_eq!(fmt_duration(Duration::from_secs(20)), "20s");
        assert_eq!(fmt_duration(Duration::from_millis(300)), "300ms");
        assert_eq!(fmt_duration(Duration::from_millis(1500)), "1.5s");
    }

    fn names<T: AsRef<str>>(list: &[T]) -> Vec<String> {
        list.iter().map(|n| n.as_ref().to_string()).collect()
    }

    #[test]
    fn preferred_algorithms_appends_openssh_compat_after_strong_defaults() {
        let preferred = preferred_algorithms(None);
        assert_eq!(
            names(&preferred.kex),
            [
                "mlkem768x25519-sha256",
                "curve25519-sha256",
                "curve25519-sha256@libssh.org",
                "diffie-hellman-group-exchange-sha256",
                "diffie-hellman-group18-sha512",
                "diffie-hellman-group17-sha512",
                "diffie-hellman-group16-sha512",
                "diffie-hellman-group15-sha512",
                "diffie-hellman-group14-sha256",
                "ecdh-sha2-nistp256",
                "ecdh-sha2-nistp384",
                "ecdh-sha2-nistp521",
                "ext-info-c",
                "ext-info-s",
                "kex-strict-c-v00@openssh.com",
                "kex-strict-s-v00@openssh.com",
            ]
        );
        assert_eq!(
            names(&preferred.cipher),
            [
                "chacha20-poly1305@openssh.com",
                "aes256-gcm@openssh.com",
                "aes256-ctr",
                "aes192-ctr",
                "aes128-ctr",
                "aes128-gcm@openssh.com",
            ]
        );
        assert_eq!(
            names(&preferred.mac),
            [
                "hmac-sha2-512-etm@openssh.com",
                "hmac-sha2-256-etm@openssh.com",
                "hmac-sha2-512",
                "hmac-sha2-256",
                "hmac-sha1-etm@openssh.com",
                "hmac-sha1",
            ]
        );
        // 明令禁止的弱算法一条都不许出现。
        let all: Vec<String> = names(&preferred.kex)
            .into_iter()
            .chain(names(&preferred.cipher))
            .collect();
        for banned in [
            "diffie-hellman-group1-sha1",
            "diffie-hellman-group14-sha1",
            "diffie-hellman-group-exchange-sha1",
            "3des-cbc",
            "aes128-cbc",
            "aes192-cbc",
            "aes256-cbc",
        ] {
            assert!(
                !all.iter().any(|n| n == banned),
                "{banned} must not be offered"
            );
        }
        // 主机密钥算法未指定时沿用 russh 默认。
        assert_eq!(preferred.key, russh::Preferred::default().key);
    }

    #[test]
    fn preferred_algorithms_keeps_the_known_hosts_host_key_order() {
        let order = vec![Algorithm::Rsa { hash: None }, Algorithm::Ed25519];
        let preferred = preferred_algorithms(Some(order.clone()));
        assert_eq!(preferred.key.as_ref(), order.as_slice());
    }
}
