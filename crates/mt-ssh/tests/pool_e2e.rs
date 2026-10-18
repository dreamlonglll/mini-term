//! 会话池端到端测试:进程内 russh 服务器(必要时外加进程内 ssh-agent)对本 crate 的池。
//!
//! 只连 127.0.0.1 上本测试自己起的服务器;home / ssh config / known_hosts / ssh-agent
//! 一律指向测试自己的临时目录与端点,**绝不碰**真实的 `~/.ssh` 与系统 ssh-agent。
//! 密码一律用明文(`mt_secret::reveal_global` 对非信封值原样放行,不碰凭据密钥文件)。

use std::borrow::Cow;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use mt_core::SshConnection;
use mt_ssh::russh;
use mt_ssh::{AcquireErrorKind, AgentSource, CachedSession, PoolConfig, SshClientEnv, SshPool};
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

/// `ssh-keygen -t ed25519 -N mt-ssh-test` 一次性生成的**测试专用废弃密钥**(口令
/// `mt-ssh-test`),只用于验证「带口令的私钥」分支,不对应任何真实主机 / 账号。
const ENCRYPTED_ED25519: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABA75HILGL
APMrA1XbjP6MESAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIBesQO+amJx6a3fp
PLZOFM3wPp281cA8v6Hp64YXCl5BAAAAoCWh+qD6Zu3kDVRYifuttM1K9ePMit4hediMNY
2HzoAjXBsTe/o6klBk7BpoKIWnd/xP43UUIIA8xum0M0SF/7rAun23qLvjDJ7N3tUTjb+b
mS+z48nBMju37Iflv39vxijIdth07d001dxNOtaTVjnVYoHOwHX0tTkXeyuD0CGMS7fCHp
v9aDxRcrMAaNVKMzZbmEjucR5L+VMM5njNnS4=
-----END OPENSSH PRIVATE KEY-----
";
const ENCRYPTED_ED25519_PASSPHRASE: &str = "mt-ssh-test";

// ============================================================================
// 夹具
// ============================================================================

/// 由固定种子构造的 ed25519 私钥(不经 rng)。
fn ed25519(seed: u8) -> PrivateKey {
    PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
        &[seed; 32],
    ))
}

fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

fn write_key(path: &Path, key: &PrivateKey) {
    let text = key
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .unwrap();
    std::fs::write(path, text.as_bytes()).unwrap();
}

/// Unix 上把测试写的 ssh config(或冒充它的目录)收紧权限:组可写的配置文件会被当成
/// 「Bad owner or permissions」整个忽略,umask 偏松的机器上测试就不再测它想测的东西。
fn set_private(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// 一个刚刚还在、现在没人监听的本机端口:连上去立即被拒绝。
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// 测试专用 home:`<root>/.ssh/{config,known_hosts,...}`,drop 时删除。
struct TestHome {
    root: PathBuf,
}

impl TestHome {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mt-ssh-e2e-{}-{}-{name}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".ssh")).unwrap();
        Self { root }
    }

    fn ssh(&self, name: &str) -> PathBuf {
        self.root.join(".ssh").join(name)
    }

    fn config(&self, text: &str) {
        std::fs::write(self.ssh("config"), text).unwrap();
        set_private(&self.ssh("config"), 0o600);
    }

    fn env(&self) -> SshClientEnv {
        SshClientEnv {
            home_dir: Some(self.root.clone()),
            ssh_config_path: Some(self.ssh("config")),
            known_hosts_path: self.ssh("known_hosts"),
            agent: AgentSource::Disabled,
            local_user: Some("tester".into()),
        }
    }

    fn known_hosts(&self) -> String {
        std::fs::read_to_string(self.ssh("known_hosts")).unwrap_or_default()
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn connection(id: &str, host: &str, port: u16, user: &str) -> SshConnection {
    SshConnection {
        id: id.into(),
        name: id.into(),
        host: host.into(),
        port,
        user: user.into(),
        password: None,
        identity_file: None,
        group: None,
        extra: Default::default(),
    }
}

fn pool_config(connect_timeout: Duration) -> PoolConfig {
    PoolConfig {
        connect_timeout,
        ..PoolConfig::default()
    }
}

async fn exec(session: &CachedSession, command: &str) -> String {
    let output =
        mt_ssh::run_bounded_exec_on_session(session, command, Duration::from_secs(5), 4096)
            .await
            .unwrap();
    String::from_utf8(output.stdout).unwrap()
}

// ============================================================================
// 进程内 SSH 服务器
// ============================================================================

#[derive(Clone)]
struct Behavior {
    /// exec 回显的前缀,用来分辨连到了哪台服务器。
    name: String,
    methods: Vec<MethodKind>,
    password: Option<String>,
    kbd_password: Option<String>,
    keys: Vec<PublicKey>,
    /// 失败计到这个数就直接关掉 TCP 连接、不发 DISCONNECT(dropbear 的做法;0 = 不限)。
    max_auth_tries: usize,
    /// 模拟 OpenSSH sshd `MaxAuthTries`:失败计到这个数就发 DISCONNECT「Too many
    /// authentication failures」再断开(0 = 不限)。
    disconnect_after_failures: usize,
    /// 收到密码认证请求时直接关掉 TCP 连接(模拟认证途中断网)。
    drop_on_password: bool,
    /// 是否放行 direct-tcpip(当跳板用)。
    allow_tunnel: bool,
    preferred: Option<russh::Preferred>,
}

impl Behavior {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            methods: vec![
                MethodKind::PublicKey,
                MethodKind::Password,
                MethodKind::KeyboardInteractive,
            ],
            password: None,
            kbd_password: None,
            keys: Vec::new(),
            max_auth_tries: 0,
            disconnect_after_failures: 0,
            drop_on_password: false,
            allow_tunnel: false,
            preferred: None,
        }
    }
}

/// 服务器一侧 TCP 流的「静音」开关。模拟 sshd 因失败次数过多而断开时,russh 服务端的
/// 认证回调里只能把 DISCONNECT 排进队列;静音让服务器先不读客户端发来的下一个请求,
/// 排着的 DISCONNECT 一定先发出去(与 sshd 一样,后面的请求不再被处理)。服务器关写端
/// 时解除。
#[derive(Default)]
struct Mute {
    muted: AtomicBool,
    waker: futures::task::AtomicWaker,
}

struct MutableStream {
    inner: TcpStream,
    mute: Arc<Mute>,
}

impl AsyncRead for MutableStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.mute.muted.load(Ordering::SeqCst) {
            this.mute.waker.register(cx.waker());
            if this.mute.muted.load(Ordering::SeqCst) {
                return Poll::Pending;
            }
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for MutableStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.mute.muted.store(false, Ordering::SeqCst);
        this.mute.waker.wake();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

struct TestHandler {
    behavior: Behavior,
    log: Arc<Mutex<Vec<String>>>,
    failures: usize,
    /// 本连接的服务端句柄(`run_stream` 返回后填上),发 DISCONNECT 用。
    handle: Arc<Mutex<Option<server::Handle>>>,
    mute: Arc<Mute>,
}

impl TestHandler {
    fn log(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }

    async fn reject(&mut self) -> Result<Auth, russh::Error> {
        self.failures += 1;
        if self.behavior.max_auth_tries != 0 && self.failures >= self.behavior.max_auth_tries {
            self.log("too many authentication failures".into());
            return Err(russh::Error::Disconnect);
        }
        let limit = self.behavior.disconnect_after_failures;
        if limit != 0 && self.failures >= limit {
            self.log("too many authentication failures, disconnecting".into());
            self.mute.muted.store(true, Ordering::SeqCst);
            let handle = self.handle.lock().unwrap().clone();
            match handle {
                Some(handle) => {
                    let _ = handle
                        .disconnect(
                            russh::Disconnect::ProtocolError,
                            "Too many authentication failures".into(),
                            String::new(),
                        )
                        .await;
                }
                None => return Err(russh::Error::Disconnect),
            }
        }
        Ok(Auth::reject())
    }

    fn accepts(&self, key: &PublicKey) -> bool {
        self.behavior
            .keys
            .iter()
            .any(|k| k.key_data() == key.key_data())
    }
}

impl server::Handler for TestHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::reject())
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.log(format!("password {user}"));
        if self.behavior.drop_on_password {
            // 不发 DISCONNECT、直接关掉 TCP:对客户端来说就是认证途中断了网。
            return Err(russh::Error::Disconnect);
        }
        if self.behavior.password.as_deref() == Some(password) {
            Ok(Auth::Accept)
        } else {
            self.reject().await
        }
    }

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.log(format!("offered {user} {}", fingerprint(key)));
        if self.accepts(key) {
            Ok(Auth::Accept)
        } else {
            // 与 sshd 一样:被拒的公钥算一次失败。
            self.reject().await
        }
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.log(format!("publickey {user} {}", fingerprint(key)));
        if self.accepts(key) {
            Ok(Auth::Accept)
        } else {
            self.reject().await
        }
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let Some(mut response) = response else {
            return Ok(Auth::Partial {
                name: Cow::Borrowed(""),
                instructions: Cow::Borrowed(""),
                prompts: Cow::Owned(vec![(Cow::Borrowed("Password: "), false)]),
            });
        };
        let answer = response
            .next()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        self.log(format!("keyboard-interactive {user}"));
        if answer.is_some() && answer == self.behavior.kbd_password {
            Ok(Auth::Accept)
        } else {
            self.reject().await
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        let reply = format!("{}:{}", self.behavior.name, String::from_utf8_lossy(data));
        session.data(channel, reply.into_bytes())?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.log(format!("direct-tcpip {host_to_connect}:{port_to_connect}"));
        if !self.behavior.allow_tunnel {
            return Ok(false);
        }
        let target = format!("{host_to_connect}:{port_to_connect}");
        tokio::spawn(async move {
            if let Ok(mut tcp) = tokio::net::TcpStream::connect(target).await {
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
            }
        });
        Ok(true)
    }
}

struct TestServer {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<String>>>,
    accepted: Arc<AtomicUsize>,
    ended: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = server::Config {
            keys: vec![ed25519(200)],
            auth_rejection_time: Duration::from_millis(5),
            auth_rejection_time_initial: Some(Duration::ZERO),
            methods: MethodSet::from(behavior.methods.as_slice()),
            inactivity_timeout: None,
            ..Default::default()
        };
        if let Some(preferred) = behavior.preferred.clone() {
            config.preferred = preferred;
        }
        let config = Arc::new(config);
        let log = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let ended = Arc::new(AtomicUsize::new(0));
        let task = {
            let (log, accepted, ended) = (log.clone(), accepted.clone(), ended.clone());
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    accepted.fetch_add(1, Ordering::SeqCst);
                    let mute = Arc::new(Mute::default());
                    let slot = Arc::new(Mutex::new(None));
                    let handler = TestHandler {
                        behavior: behavior.clone(),
                        log: log.clone(),
                        failures: 0,
                        handle: slot.clone(),
                        mute: mute.clone(),
                    };
                    let socket = MutableStream {
                        inner: socket,
                        mute,
                    };
                    let (config, ended) = (config.clone(), ended.clone());
                    tokio::spawn(async move {
                        if let Ok(session) = server::run_stream(config, socket, handler).await {
                            *slot.lock().unwrap() = Some(session.handle());
                            let _ = session.await;
                        }
                        ended.fetch_add(1, Ordering::SeqCst);
                    });
                }
            })
        };
        Self {
            addr,
            log,
            accepted,
            ended,
            task,
        }
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn count(&self, prefix: &str) -> usize {
        self.log().iter().filter(|l| l.starts_with(prefix)).count()
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// 等到至少 `n` 条会话结束(服务器一侧看到连接关闭)。
    async fn wait_ended(&self, n: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.ended.load(Ordering::SeqCst) >= n {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ============================================================================
// 进程内 ssh-agent(russh 自带的 agent 服务端)
// ============================================================================

/// 拒绝一切签名请求的 agent(模拟用户拒绝确认 / 硬件 key 没被触摸)。
#[derive(Clone)]
struct DenySigning;

impl russh::keys::agent::server::Agent for DenySigning {
    #[allow(clippy::manual_async_fn)]
    fn confirm_request(
        &self,
        msg: russh::keys::agent::server::MessageType,
    ) -> impl std::future::Future<Output = bool> + Send {
        let allow = !matches!(msg, russh::keys::agent::server::MessageType::Sign);
        async move { allow }
    }
}

#[cfg(windows)]
async fn spawn_agent<A>(agent: A, keys: &[PrivateKey]) -> PathBuf
where
    A: russh::keys::agent::server::Agent + Send + Sync + 'static,
{
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = format!(
        r"\\.\pipe\mt-ssh-e2e-agent-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::SeqCst)
    );
    let first = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .unwrap();
    let pipe = name.clone();
    let listener = futures::stream::unfold(Some(first), move |server| {
        let pipe = pipe.clone();
        async move {
            let server = server?;
            server.connect().await.ok()?;
            let next = ServerOptions::new().create(&pipe).ok();
            Some((Ok(server), next))
        }
    });
    tokio::spawn(russh::keys::agent::server::serve(Box::pin(listener), agent));
    let mut client = AgentClient::connect_named_pipe(&name).await.unwrap();
    for key in keys {
        client.add_identity(key, &[]).await.unwrap();
    }
    PathBuf::from(name)
}

#[cfg(unix)]
async fn spawn_agent<A>(agent: A, keys: &[PrivateKey]) -> PathBuf
where
    A: russh::keys::agent::server::Agent + Send + Sync + 'static,
{
    let path = std::env::temp_dir().join(format!(
        "mt-ssh-e2e-agent-{}-{}.sock",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let incoming = futures::stream::unfold(listener, |listener| async move {
        let accepted = listener.accept().await.map(|(stream, _)| stream);
        Some((accepted, listener))
    });
    tokio::spawn(russh::keys::agent::server::serve(Box::pin(incoming), agent));
    let mut client = AgentClient::connect_uds(&path).await.unwrap();
    for key in keys {
        client.add_identity(key, &[]).await.unwrap();
    }
    path
}

// ============================================================================
// B1-1 建连超时
// ============================================================================

/// 只接受 TCP、不说一个字节的服务器:`acquire` 在预算内返回,报错写明卡在握手、对方
/// 没发标识串;服务器一侧随即看到连接关闭(没有泄漏的 socket)。
#[tokio::test]
async fn silent_server_times_out_in_the_handshake_and_releases_the_socket() {
    let home = TestHome::new("silent");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        while let Ok(n) = socket.read(&mut buf).await {
            if n == 0 {
                break;
            }
        }
        Instant::now()
    });

    let pool = SshPool::with_env(pool_config(Duration::from_millis(800)), home.env());
    let mut conn = connection("silent", "127.0.0.1", port, "u");
    conn.password = Some("pw".into());
    let started = Instant::now();
    let err = pool.try_acquire(&conn).await.err().expect("must time out");
    let returned = Instant::now();

    assert!(
        returned - started < Duration::from_secs(3),
        "took {:?}",
        returned - started
    );
    assert_eq!(err.kind, AcquireErrorKind::Timeout);
    assert!(
        err.message.starts_with(&format!(
            "ssh connection to u@127.0.0.1:{port} timed out after 800ms during SSH handshake \
             with 127.0.0.1:{port}"
        )),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("no SSH identification string"),
        "{}",
        err.message
    );
    let closed_at = tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("server should see the connection closed")
        .unwrap();
    assert!(closed_at.saturating_duration_since(returned) < Duration::from_secs(2));
    pool.shutdown().await;
}

/// 发了标识串就不再说话的服务器:此时 russh 已把会话任务 spawn 出去、在等密钥交换,
/// 它根本不看 Handle 是否还在。超时后传输被掐断,任务随即退出 —— 服务器一侧很快读到
/// EOF。没有掐断的话,这条连接会一直挂着(russh 认证前不发 keepalive,也没有
/// inactivity 超时)。
#[tokio::test]
async fn banner_only_server_times_out_in_key_exchange_and_the_russh_task_exits() {
    let home = TestHome::new("banner-only");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket
            .write_all(b"SSH-2.0-OpenSSH_9.6 silent-after-banner\r\n")
            .await
            .unwrap();
        let mut received = 0usize;
        let mut buf = [0u8; 4096];
        while let Ok(n) = socket.read(&mut buf).await {
            if n == 0 {
                break;
            }
            received += n;
        }
        (Instant::now(), received)
    });

    let pool = SshPool::with_env(pool_config(Duration::from_millis(800)), home.env());
    let conn = connection("banner", "127.0.0.1", port, "u");
    let err = pool.try_acquire(&conn).await.err().expect("must time out");
    let returned = Instant::now();

    assert_eq!(err.kind, AcquireErrorKind::Timeout);
    assert!(
        err.message.contains("during SSH handshake"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("key exchange did not complete"),
        "{}",
        err.message
    );
    let (closed_at, received) = tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("the spawned russh session task must exit and close the socket")
        .unwrap();
    assert!(received > 0, "the client sent its banner and KEXINIT");
    assert!(closed_at.saturating_duration_since(returned) < Duration::from_secs(2));
    pool.shutdown().await;
}

#[tokio::test]
async fn refused_port_is_a_network_error_not_a_timeout() {
    let home = TestHome::new("refused");
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), home.env());
    let err = pool
        .try_acquire(&connection("refused", "127.0.0.1", port, "u"))
        .await
        .err()
        .expect("nothing listens there");
    assert_eq!(err.kind, AcquireErrorKind::Network);
    assert!(
        err.message.starts_with(&format!(
            "ssh connect to 127.0.0.1:{port} failed: TCP connect failed ("
        )),
        "{}",
        err.message
    );
    pool.shutdown().await;
}

/// 认证完成后长时间没有输出、但服务器照常回 keepalive 的会话不会被断开
/// (本 crate 不设 inactivity 超时,keepalive 回包又会清零失败计数)。
#[tokio::test]
async fn authenticated_idle_session_survives_many_keepalive_rounds() {
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        ..Behavior::new("idle")
    })
    .await;
    let home = TestHome::new("idle");
    let pool = SshPool::with_env(
        PoolConfig {
            keepalive_interval: Duration::from_millis(100),
            keepalive_max: 2,
            ..pool_config(Duration::from_secs(5))
        },
        home.env(),
    );
    let mut conn = connection("idle", "127.0.0.1", server.port(), "u");
    conn.password = Some("pw".into());
    let session = pool.try_acquire(&conn).await.expect("connects");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!session.lock().await.is_closed(), "idle session was killed");
    assert_eq!(exec(&session, "still-there").await, "idle:still-there");
    pool.shutdown().await;
}

// ============================================================================
// B1-2 认证
// ============================================================================

/// 私钥带口令(池拿不到口令)→ 记下原因,回落到密码。密码错时报错逐条列出每种方法,
/// 不含密码本身;随后进入冷却(B1-3);改了密码(身份变了)立即可重试并成功。
#[tokio::test]
async fn key_load_failure_falls_back_to_password_with_per_method_report_and_cooldown() {
    let server = TestServer::start(Behavior {
        password: Some("right-pw".into()),
        ..Behavior::new("fallback")
    })
    .await;
    let home = TestHome::new("fallback");
    std::fs::write(home.ssh("encrypted_key"), ENCRYPTED_ED25519).unwrap();
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let mut conn = connection("fallback", "127.0.0.1", server.port(), "deploy");
    conn.identity_file = Some("~/.ssh/encrypted_key".into());
    conn.password = Some("wrong-pw".into());

    let err = pool.try_acquire(&conn).await.err().expect("wrong password");
    assert_eq!(err.kind, AcquireErrorKind::Auth);
    let message = &err.message;
    assert!(
        message.starts_with(&format!(
            "authentication failed for deploy@127.0.0.1:{} (server allows: \
             publickey,password,keyboard-interactive): ",
            server.port()
        )),
        "{message}"
    );
    assert!(
        message.contains(
            "identity file ~/.ssh/encrypted_key (connection setting): encrypted with a passphrase"
        ),
        "{message}"
    );
    assert!(
        message.contains("password: rejected by server"),
        "{message}"
    );
    assert!(
        message.contains("keyboard-interactive: rejected by server"),
        "{message}"
    );
    assert!(!message.contains("wrong-pw"), "password leaked: {message}");

    // 冷却期内同一身份直接返错,不再去连服务器。
    let accepted = server.accepted();
    let err = pool.try_acquire(&conn).await.err().expect("cooldown");
    assert_eq!(err.kind, AcquireErrorKind::Cooldown);
    assert!(
        err.message.starts_with("authentication cooldown: "),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("to avoid triggering server-side bans"),
        "{}",
        err.message
    );
    assert_eq!(
        server.accepted(),
        accepted,
        "no new TCP connection in cooldown"
    );

    // 改了密码 = 新身份:立即重试,私钥加载失败不妨碍密码登录。
    conn.password = Some("right-pw".into());
    let session = pool.try_acquire(&conn).await.expect("password fallback");
    assert_eq!(exec(&session, "whoami").await, "fallback:whoami");
    pool.shutdown().await;
}

/// 什么都没配:按 OpenSSH 习惯试默认私钥 `~/.ssh/id_ed25519`。
#[tokio::test]
async fn default_key_is_tried_when_nothing_is_configured() {
    let key = ed25519(11);
    let server = TestServer::start(Behavior {
        keys: vec![key.public_key().clone()],
        ..Behavior::new("default-key")
    })
    .await;
    let home = TestHome::new("default-key");
    write_key(&home.ssh("id_ed25519"), &key);
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let session = pool
        .try_acquire(&connection("default", "127.0.0.1", server.port(), "u"))
        .await
        .expect("default key");
    assert_eq!(exec(&session, "x").await, "default-key:x");
    assert_eq!(server.count("password"), 0);
    pool.shutdown().await;
}

/// 连接配置里写 `~/.ssh/...`:展开到 home;显式配了私钥时不再试默认私钥。
#[tokio::test]
async fn tilde_identity_file_is_expanded_and_suppresses_default_keys() {
    let wanted = ed25519(12);
    let default_key = ed25519(13);
    let server = TestServer::start(Behavior {
        keys: vec![wanted.public_key().clone()],
        ..Behavior::new("tilde")
    })
    .await;
    let home = TestHome::new("tilde");
    write_key(&home.ssh("work_key"), &wanted);
    write_key(&home.ssh("id_ed25519"), &default_key);
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let mut conn = connection("tilde", "127.0.0.1", server.port(), "u");
    conn.identity_file = Some("~/.ssh/work_key".into());
    let session = pool.try_acquire(&conn).await.expect("tilde key");
    assert_eq!(exec(&session, "y").await, "tilde:y");
    assert!(
        !server
            .log()
            .iter()
            .any(|l| l.contains(&fingerprint(default_key.public_key()))),
        "default key must not be offered: {:?}",
        server.log()
    );
    pool.shutdown().await;
}

/// ssh-agent 里的 key;以及带口令、但已加载进 agent 的私钥经 agent 签名使用。
#[tokio::test]
async fn agent_keys_are_used_including_for_an_encrypted_identity_file() {
    let encrypted = PrivateKey::from_openssh(ENCRYPTED_ED25519)
        .unwrap()
        .decrypt(ENCRYPTED_ED25519_PASSPHRASE)
        .unwrap();
    let other = ed25519(21);
    let server = TestServer::start(Behavior {
        keys: vec![encrypted.public_key().clone()],
        ..Behavior::new("agent")
    })
    .await;
    let agent_path = spawn_agent((), &[other.clone(), encrypted.clone()]).await;
    let home = TestHome::new("agent");
    std::fs::write(home.ssh("encrypted_key"), ENCRYPTED_ED25519).unwrap();
    // IdentitiesOnly:只用显式私钥 —— agent 里另一把 key 不得出现在服务器日志里。
    home.config(&format!(
        "Host agent-host\n  HostName 127.0.0.1\n  Port {}\n  IdentitiesOnly yes\n  \
         IdentityFile ~/.ssh/encrypted_key\n",
        server.port()
    ));
    let env = SshClientEnv {
        agent: AgentSource::Endpoint(agent_path),
        ..home.env()
    };
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), env);
    let session = pool
        .try_acquire(&connection("agent", "agent-host", 22, "u"))
        .await
        .expect("agent-backed encrypted key");
    assert_eq!(exec(&session, "z").await, "agent:z");
    let log = server.log();
    assert!(
        !log.iter()
            .any(|l| l.contains(&fingerprint(other.public_key()))),
        "IdentitiesOnly must hide other agent keys: {log:?}"
    );

    // 没有 IdentitiesOnly、没有显式私钥:agent 里的 key 照常被试到。
    let plain = TestServer::start(Behavior {
        keys: vec![other.public_key().clone()],
        ..Behavior::new("agent-plain")
    })
    .await;
    let session = pool
        .try_acquire(&connection("agent-plain", "127.0.0.1", plain.port(), "u"))
        .await
        .expect("agent key");
    assert_eq!(exec(&session, "w").await, "agent-plain:w");
    pool.shutdown().await;
}

/// agent 拒绝签名:不卡死会话、不吞掉后续方法,照常回落到密码。
#[tokio::test]
async fn agent_signing_failure_still_lets_the_password_through() {
    let key = ed25519(31);
    let server = TestServer::start(Behavior {
        keys: vec![key.public_key().clone()],
        password: Some("pw".into()),
        ..Behavior::new("deny")
    })
    .await;
    let agent_path = spawn_agent(DenySigning, std::slice::from_ref(&key)).await;
    let home = TestHome::new("deny");
    let env = SshClientEnv {
        agent: AgentSource::Endpoint(agent_path),
        ..home.env()
    };
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), env);
    let mut conn = connection("deny", "127.0.0.1", server.port(), "u");
    conn.password = Some("pw".into());
    let session = pool
        .try_acquire(&conn)
        .await
        .expect("password after agent failure");
    assert_eq!(exec(&session, "v").await, "deny:v");
    assert_eq!(server.count("password u"), 1, "{:?}", server.log());
    pool.shutdown().await;
}

/// agent 里一堆服务器不认的 key + 模拟 sshd `MaxAuthTries 6`:公钥只试到上限,
/// 密码一定有机会;不设上限的话第 6 把 key 被拒时连接就被服务器断开了。
#[tokio::test]
async fn publickey_attempts_are_capped_so_the_password_gets_its_turn() {
    let keys: Vec<PrivateKey> = (40..47).map(ed25519).collect();
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        max_auth_tries: 6,
        ..Behavior::new("cap")
    })
    .await;
    let agent_path = spawn_agent((), &keys).await;
    let home = TestHome::new("cap");
    let env = SshClientEnv {
        agent: AgentSource::Endpoint(agent_path),
        ..home.env()
    };
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), env);
    let mut conn = connection("cap", "127.0.0.1", server.port(), "u");
    conn.password = Some("pw".into());
    let session = pool
        .try_acquire(&conn)
        .await
        .expect("password within MaxAuthTries");
    assert_eq!(exec(&session, "q").await, "cap:q");
    let log = server.log();
    assert_eq!(server.count("offered"), 4, "{log:?}");
    assert_eq!(server.count("password u"), 1, "{log:?}");
    pool.shutdown().await;
}

/// 服务器在认证途中断开(这里模拟 MaxAuthTries 2):已经被拒过,按认证失败算
/// (进入冷却),报错写明连接被断开。
#[tokio::test]
async fn server_dropping_the_connection_mid_auth_is_an_auth_failure() {
    let keys: Vec<PrivateKey> = (80..83).map(ed25519).collect();
    let server = TestServer::start(Behavior {
        max_auth_tries: 2,
        ..Behavior::new("strict")
    })
    .await;
    let agent_path = spawn_agent((), &keys).await;
    let home = TestHome::new("strict");
    let env = SshClientEnv {
        agent: AgentSource::Endpoint(agent_path),
        ..home.env()
    };
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), env);
    let conn = connection("strict", "127.0.0.1", server.port(), "u");
    let err = pool.try_acquire(&conn).await.err().expect("dropped");
    assert_eq!(err.kind, AcquireErrorKind::Auth, "{}", err.message);
    assert!(
        err.message.contains("rejected by server"),
        "{}",
        err.message
    );
    assert!(err.message.contains("connection"), "{}", err.message);
    let err = pool.try_acquire(&conn).await.err().expect("cooldown");
    assert_eq!(err.kind, AcquireErrorKind::Cooldown);
    pool.shutdown().await;
}

/// 只开 keyboard-interactive 的服务器:用配置的密码作答。
#[tokio::test]
async fn keyboard_interactive_only_server_accepts_the_password() {
    let server = TestServer::start(Behavior {
        methods: vec![MethodKind::KeyboardInteractive],
        kbd_password: Some("pw".into()),
        ..Behavior::new("kbd")
    })
    .await;
    let home = TestHome::new("kbd");
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let mut conn = connection("kbd", "127.0.0.1", server.port(), "u");
    conn.password = Some("pw".into());
    let session = pool.try_acquire(&conn).await.expect("kbd-int");
    assert_eq!(exec(&session, "k").await, "kbd:k");
    assert_eq!(server.count("password"), 0, "password method not offered");
    pool.shutdown().await;
}

/// ssh config 的 HostName / Port / User / IdentityFile 生效;改了 ssh config 后
/// 不会复用到旧目标上的 session。
#[tokio::test]
async fn ssh_config_alias_resolves_and_config_changes_are_not_reused() {
    let key = ed25519(51);
    let a = TestServer::start(Behavior {
        keys: vec![key.public_key().clone()],
        ..Behavior::new("server-a")
    })
    .await;
    let b = TestServer::start(Behavior {
        keys: vec![key.public_key().clone()],
        ..Behavior::new("server-b")
    })
    .await;
    let home = TestHome::new("alias");
    write_key(&home.ssh("svc_key"), &key);
    let config_for = |port: u16| {
        format!(
            "Host svc\n  HostName 127.0.0.1\n  Port {port}\n  User cfg-user\n  \
             IdentityFile ~/.ssh/svc_key\n"
        )
    };
    home.config(&config_for(a.port()));
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let conn = connection("svc", "svc", 22, "");

    let first = pool.try_acquire(&conn).await.expect("alias -> a");
    assert_eq!(exec(&first, "1").await, "server-a:1");
    assert!(a.log().iter().any(|l| l.starts_with("publickey cfg-user")));
    drop(first);

    home.config(&config_for(b.port()));
    let second = pool.try_acquire(&conn).await.expect("alias -> b");
    assert_eq!(exec(&second, "2").await, "server-b:2");
    pool.shutdown().await;
}

/// 单跳 ProxyJump:跳板按自己的 ssh config 认证、做主机密钥校验,在跳板上开
/// direct-tcpip 到目标;shutdown 时目标与跳板都被断开。
#[tokio::test]
async fn proxy_jump_tunnels_to_the_target_and_shutdown_closes_both_hops() {
    let jump_key = ed25519(61);
    let jump = TestServer::start(Behavior {
        keys: vec![jump_key.public_key().clone()],
        allow_tunnel: true,
        ..Behavior::new("jump")
    })
    .await;
    let target = TestServer::start(Behavior {
        password: Some("tpw".into()),
        ..Behavior::new("target")
    })
    .await;
    let home = TestHome::new("jump");
    write_key(&home.ssh("jump_key"), &jump_key);
    home.config(&format!(
        "Host target-alias\n  HostName 127.0.0.1\n  Port {}\n  ProxyJump jumper@jump-alias\n\
         Host jump-alias\n  HostName 127.0.0.1\n  Port {}\n  IdentityFile ~/.ssh/jump_key\n",
        target.port(),
        jump.port()
    ));
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), home.env());
    let mut conn = connection("jumped", "target-alias", 22, "deploy");
    conn.password = Some("tpw".into());

    let session = pool.try_acquire(&conn).await.expect("via jump host");
    assert_eq!(exec(&session, "hostname").await, "target:hostname");
    let tunnel = format!("direct-tcpip 127.0.0.1:{}", target.port());
    assert_eq!(jump.count(&tunnel), 1, "{:?}", jump.log());
    assert_eq!(jump.count("publickey jumper "), 1, "{:?}", jump.log());
    assert_eq!(
        jump.count("password"),
        0,
        "the target's password is not sent to the jump host"
    );
    assert_eq!(target.count("password deploy"), 1);
    let known_hosts = home.known_hosts();
    assert!(known_hosts.contains(&format!("[127.0.0.1]:{} ", jump.port())));
    assert!(known_hosts.contains(&format!("[127.0.0.1]:{} ", target.port())));

    drop(session);
    pool.shutdown().await;
    assert!(
        target.wait_ended(1, Duration::from_secs(3)).await,
        "target closed"
    );
    assert!(
        jump.wait_ended(1, Duration::from_secs(3)).await,
        "jump host closed"
    );
}

/// 多跳 ProxyJump(逗号分隔):逐跳经上一跳的隧道连下一跳。
#[tokio::test]
async fn multi_hop_proxy_jump_chains_tunnels() {
    let key = ed25519(71);
    let first = TestServer::start(Behavior {
        keys: vec![key.public_key().clone()],
        allow_tunnel: true,
        ..Behavior::new("hop1")
    })
    .await;
    let second = TestServer::start(Behavior {
        keys: vec![key.public_key().clone()],
        allow_tunnel: true,
        ..Behavior::new("hop2")
    })
    .await;
    let target = TestServer::start(Behavior {
        keys: vec![key.public_key().clone()],
        ..Behavior::new("final")
    })
    .await;
    let home = TestHome::new("multi-hop");
    write_key(&home.ssh("id_ed25519"), &key);
    home.config(&format!(
        "Host final\n  HostName 127.0.0.1\n  Port {}\n  ProxyJump 127.0.0.1:{},127.0.0.1:{}\n",
        target.port(),
        first.port(),
        second.port()
    ));
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), home.env());
    let session = pool
        .try_acquire(&connection("multi", "final", 22, "u"))
        .await
        .expect("two hops");
    assert_eq!(exec(&session, "m").await, "final:m");
    let to_second = format!("direct-tcpip 127.0.0.1:{}", second.port());
    assert_eq!(first.count(&to_second), 1, "{:?}", first.log());
    let to_target = format!("direct-tcpip 127.0.0.1:{}", target.port());
    assert_eq!(second.count(&to_target), 1, "{:?}", second.log());
    pool.shutdown().await;
}

/// `Host *` 配了一条 ProxyCommand(公司代理、cloudflared 之类):本来能直连的主机照常
/// 直连(读 ssh config 之前的行为);连不上时报错末尾写明忽略了哪条 ProxyCommand。
#[tokio::test]
async fn proxy_command_is_ignored_and_the_host_is_reached_directly() {
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        ..Behavior::new("direct")
    })
    .await;
    let home = TestHome::new("proxy-command");
    home.config("Host *\n  ProxyCommand cloudflared access ssh --hostname %h\n");
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let mut conn = connection("direct", "127.0.0.1", server.port(), "u");
    conn.password = Some("pw".into());
    let session = pool.try_acquire(&conn).await.expect("reached directly");
    assert_eq!(exec(&session, "d").await, "direct:d");

    let port = closed_port();
    let err = pool
        .try_acquire(&connection("gated", "127.0.0.1", port, "u"))
        .await
        .err()
        .expect("nothing listens there");
    assert_eq!(err.kind, AcquireErrorKind::Network);
    assert!(
        err.message
            .starts_with(&format!("ssh connect to 127.0.0.1:{port} failed: ")),
        "{}",
        err.message
    );
    assert!(
        err.message.contains(
            "note: ignored \"ProxyCommand cloudflared access ssh --hostname %h\" from ssh config"
        ),
        "{}",
        err.message
    );
    assert!(
        err.message
            .contains("已忽略 ~/.ssh/config 里的 ProxyCommand"),
        "{}",
        err.message
    );
    pool.shutdown().await;
}

/// ssh config 一时读不了(这里换成了同名目录,模拟编辑器保存时的独占锁):沿用上次的路线,
/// 健康的会话照常复用;要新建时也按上次的路线连,而不是把别名当主机名。
#[tokio::test]
async fn unreadable_ssh_config_keeps_the_last_route() {
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        ..Behavior::new("svc")
    })
    .await;
    let home = TestHome::new("unreadable-config");
    home.config(&format!(
        "Host svc-alias\n  HostName 127.0.0.1\n  Port {}\n",
        server.port()
    ));
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let mut conn = connection("svc", "svc-alias", 22, "u");
    conn.password = Some("pw".into());
    let first = pool.try_acquire(&conn).await.expect("via ssh config");

    std::fs::remove_file(home.ssh("config")).unwrap();
    std::fs::create_dir(home.ssh("config")).unwrap();
    set_private(&home.ssh("config"), 0o700);
    let again = pool.try_acquire(&conn).await.expect("reused");
    assert!(Arc::ptr_eq(&first, &again), "the healthy session is reused");
    assert_eq!(server.accepted(), 1);

    // 会话没了也照上次的路线新建。
    drop((first, again));
    pool.evict(&conn.id).await;
    let rebuilt = pool.try_acquire(&conn).await.expect("last route");
    assert_eq!(exec(&rebuilt, "r").await, "svc:r");
    assert_eq!(server.accepted(), 2);
    pool.shutdown().await;
}

/// ssh config 写坏了:健康的会话照常复用;要新建连接时才报 `Config`(终端里的 ssh 同样
/// 会拒绝)。
#[tokio::test]
async fn broken_ssh_config_does_not_block_reusing_a_healthy_session() {
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        ..Behavior::new("svc")
    })
    .await;
    let home = TestHome::new("broken-config");
    let config = |port: &str| format!("Host svc-alias\n  HostName 127.0.0.1\n  Port {port}\n");
    home.config(&config(&server.port().to_string()));
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let mut conn = connection("svc", "svc-alias", 22, "u");
    conn.password = Some("pw".into());
    let first = pool.try_acquire(&conn).await.expect("via ssh config");

    home.config(&config("abc"));
    let again = pool.try_acquire(&conn).await.expect("reused");
    assert!(Arc::ptr_eq(&first, &again));

    drop((first, again));
    pool.evict(&conn.id).await;
    let err = pool.try_acquire(&conn).await.err().expect("config error");
    assert_eq!(err.kind, AcquireErrorKind::Config);
    assert!(
        err.message.contains("invalid Port value 'abc'"),
        "{}",
        err.message
    );
    assert_eq!(server.accepted(), 1);
    pool.shutdown().await;
}

/// 跳板只开密码类认证:跳板一律不用密码(有意如此),报错写明是跳板的认证失败、跳板支持
/// 哪些方法、为什么不用密码。
#[tokio::test]
async fn password_only_jump_host_fails_with_a_jump_specific_message() {
    let jump = TestServer::start(Behavior {
        methods: vec![MethodKind::Password, MethodKind::KeyboardInteractive],
        password: Some("jpw".into()),
        allow_tunnel: true,
        ..Behavior::new("jump")
    })
    .await;
    let target = TestServer::start(Behavior {
        password: Some("tpw".into()),
        ..Behavior::new("target")
    })
    .await;
    let home = TestHome::new("jump-password-only");
    home.config(&format!(
        "Host target-alias\n  HostName 127.0.0.1\n  Port {}\n  ProxyJump jumper@jump-alias\n\
         Host jump-alias\n  HostName 127.0.0.1\n  Port {}\n",
        target.port(),
        jump.port()
    ));
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), home.env());
    let mut conn = connection("jumped", "target-alias", 22, "deploy");
    conn.password = Some("tpw".into());
    let err = pool.try_acquire(&conn).await.err().expect("jump auth");
    assert_eq!(err.kind, AcquireErrorKind::Auth);
    assert!(
        err.message.starts_with(&format!(
            "authentication failed for jump host jumper@127.0.0.1:{} (server allows: \
             password,keyboard-interactive): ",
            jump.port()
        )),
        "{}",
        err.message
    );
    assert!(
        err.message.contains(
            "password: not used for jump hosts (mini-term signs in to a jump host with keys only"
        ),
        "{}",
        err.message
    );
    assert_eq!(jump.count("password"), 0, "{:?}", jump.log());
    assert_eq!(target.accepted(), 0);
    pool.shutdown().await;
}

// ============================================================================
// B1-4 老服务器算法 / B1-5、B1-6 主机密钥
// ============================================================================

/// 只有 ECDH-NIST 密钥交换 + aes128-ctr + hmac-sha1 的「老服务器」,以及只有
/// aes128-gcm 的服务器:都能协商成功(russh 默认表里没有这几条)。
#[tokio::test]
async fn legacy_server_algorithms_negotiate() {
    let home = TestHome::new("legacy");
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let legacy_sets = [
        (
            vec![russh::kex::ECDH_SHA2_NISTP256],
            vec![russh::cipher::AES_128_CTR],
            vec![russh::mac::HMAC_SHA1],
        ),
        (
            vec![russh::kex::ECDH_SHA2_NISTP384],
            vec![russh::cipher::AES_128_GCM],
            vec![russh::mac::HMAC_SHA1_ETM],
        ),
    ];
    for (index, (kex, cipher, mac)) in legacy_sets.into_iter().enumerate() {
        let server = TestServer::start(Behavior {
            password: Some("pw".into()),
            preferred: Some(russh::Preferred {
                kex: Cow::Owned(kex),
                cipher: Cow::Owned(cipher),
                mac: Cow::Owned(mac),
                ..russh::Preferred::default()
            }),
            ..Behavior::new("legacy")
        })
        .await;
        let mut conn = connection(&format!("legacy-{index}"), "127.0.0.1", server.port(), "u");
        conn.password = Some("pw".into());
        let session = pool.try_acquire(&conn).await.expect("legacy algorithms");
        assert_eq!(exec(&session, "l").await, "legacy:l");
    }
    pool.shutdown().await;
}

#[tokio::test]
async fn changed_or_revoked_host_keys_are_rejected_with_english_prefixes() {
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        ..Behavior::new("hostkey")
    })
    .await;
    let home = TestHome::new("hostkey");
    let pool = SshPool::with_env(pool_config(Duration::from_secs(5)), home.env());
    let mut conn = connection("hostkey", "127.0.0.1", server.port(), "u");
    conn.password = Some("pw".into());
    let host = format!("[127.0.0.1]:{}", server.port());

    // 登记的是另一把 key:按变更拒绝。
    let stale = ed25519(99).public_key().to_openssh().unwrap();
    std::fs::write(home.ssh("known_hosts"), format!("{host} {stale}\n")).unwrap();
    let err = pool.try_acquire(&conn).await.err().expect("mismatch");
    assert_eq!(err.kind, AcquireErrorKind::HostKey);
    assert!(
        err.message.starts_with(&format!(
            "ssh connect to 127.0.0.1:{} failed: host key verification failed: ",
            server.port()
        )),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("主机密钥与 known_hosts 不符"),
        "{}",
        err.message
    );

    // 服务器的 key 被 `@revoked *` 吊销:哪怕另有一行登记了它也拒绝。
    let real = ed25519(200).public_key().to_openssh().unwrap();
    std::fs::write(
        home.ssh("known_hosts"),
        format!("{host} {real}\n@revoked * {real}\n"),
    )
    .unwrap();
    let err = pool.try_acquire(&conn).await.err().expect("revoked");
    assert_eq!(err.kind, AcquireErrorKind::HostKey);
    assert!(err.message.contains("@revoked"), "{}", err.message);

    // 只靠通配行登记且 key 一致:放行。
    std::fs::write(home.ssh("known_hosts"), format!("[127.0.0.*]:* {real}\n")).unwrap();
    let session = pool.try_acquire(&conn).await.expect("wildcard match");
    assert_eq!(exec(&session, "h").await, "hostkey:h");
    pool.shutdown().await;
}

// ============================================================================
// 评审修复:单飞建连 / 加固服务器 / 认证途中断网 / 公钥 IdentityFile
// ============================================================================

/// 同一连接的并发 acquire 单飞:密码错时服务器只看到一轮认证(一条 TCP、一次密码、一次
/// keyboard-interactive),所有请求都拿到认证失败(或冷却)。
#[tokio::test]
async fn concurrent_acquires_with_a_wrong_password_run_one_authentication() {
    let server = TestServer::start(Behavior {
        password: Some("right".into()),
        ..Behavior::new("single-flight")
    })
    .await;
    let home = TestHome::new("single-flight-wrong");
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), home.env());
    let mut conn = connection("flight", "127.0.0.1", server.port(), "u");
    conn.password = Some("wrong".into());

    let attempts = futures::future::join_all((0..6).map(|_| pool.try_acquire(&conn))).await;
    for attempt in &attempts {
        let err = attempt.as_ref().err().expect("wrong password");
        assert!(
            matches!(
                err.kind,
                AcquireErrorKind::Auth | AcquireErrorKind::Cooldown
            ),
            "{err:?}"
        );
    }
    assert_eq!(server.accepted(), 1, "{:?}", server.log());
    assert_eq!(server.count("password u"), 1, "{:?}", server.log());
    assert_eq!(
        server.count("keyboard-interactive u"),
        1,
        "{:?}",
        server.log()
    );
    pool.shutdown().await;
}

/// 成功时同样只建一条会话,所有并发请求拿到同一个。
#[tokio::test]
async fn concurrent_acquires_share_one_new_session() {
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        ..Behavior::new("shared")
    })
    .await;
    let home = TestHome::new("single-flight-ok");
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), home.env());
    let mut conn = connection("shared", "127.0.0.1", server.port(), "u");
    conn.password = Some("pw".into());

    let sessions: Vec<Arc<CachedSession>> =
        futures::future::join_all((0..6).map(|_| pool.try_acquire(&conn)))
            .await
            .into_iter()
            .map(|result| result.expect("connects"))
            .collect();
    for session in &sessions[1..] {
        assert!(Arc::ptr_eq(session, &sessions[0]));
    }
    assert_eq!(server.accepted(), 1);
    assert_eq!(server.count("password u"), 1);
    assert_eq!(pool.len().await, 1);
    pool.shutdown().await;
}

/// 单飞的持闸者在建连途中被丢弃(调用方超时 / 取消):闸随之放开,下一个请求自己去建,
/// 不会被一个已经不存在的建连挡住。
#[tokio::test]
async fn a_dropped_leader_releases_the_single_flight_gate() {
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        ..Behavior::new("after-drop")
    })
    .await;
    // 第一条连接一声不吭地挂着,之后的连接转给真服务器。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let upstream = server.addr;
    let front = tokio::spawn(async move {
        let mut parked = Vec::new();
        while let Ok((mut socket, _)) = listener.accept().await {
            if parked.is_empty() {
                parked.push(socket);
                continue;
            }
            tokio::spawn(async move {
                if let Ok(mut upstream) = TcpStream::connect(upstream).await {
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
                }
            });
        }
    });
    let home = TestHome::new("dropped-leader");
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), home.env());
    let mut conn = connection("dropped", "127.0.0.1", port, "u");
    conn.password = Some("pw".into());

    let gave_up = tokio::time::timeout(Duration::from_millis(300), pool.try_acquire(&conn)).await;
    assert!(gave_up.is_err(), "the first connection is parked");
    let session = tokio::time::timeout(Duration::from_secs(5), pool.try_acquire(&conn))
        .await
        .expect("the gate was released with the dropped leader")
        .expect("connects through the second connection");
    assert_eq!(exec(&session, "x").await, "after-drop:x");
    pool.shutdown().await;
    front.abort();
}

/// 加固过的 sshd(`MaxAuthTries 3`)+ agent 里一堆它不认的 key:第三把被拒后服务器发
/// 「Too many authentication failures」断开,密码还没轮到。重连一次只试密码,登录成功。
#[tokio::test]
async fn hardened_server_disconnecting_before_the_password_gets_a_password_only_retry() {
    let keys: Vec<PrivateKey> = (110..115).map(ed25519).collect();
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        disconnect_after_failures: 3,
        ..Behavior::new("hardened")
    })
    .await;
    let agent_path = spawn_agent((), &keys).await;
    let home = TestHome::new("hardened");
    let env = SshClientEnv {
        agent: AgentSource::Endpoint(agent_path),
        ..home.env()
    };
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), env);
    let mut conn = connection("hardened", "127.0.0.1", server.port(), "u");
    conn.password = Some("pw".into());

    let session = pool.try_acquire(&conn).await.expect("password on retry");
    assert_eq!(exec(&session, "p").await, "hardened:p");
    let log = server.log();
    assert_eq!(server.accepted(), 2, "{log:?}");
    assert_eq!(server.count("password u"), 1, "{log:?}");
    assert!(server.count("offered") <= 4, "{log:?}");
    pool.shutdown().await;
}

/// 同样的服务器、密码却是错的:重连那一轮也失败。逐方法报错写明两轮,冷却只记一次 ——
/// 下一次 acquire 直接冷却,不再碰服务器。
#[tokio::test]
async fn password_only_retry_failure_is_one_auth_failure() {
    let keys: Vec<PrivateKey> = (120..125).map(ed25519).collect();
    let server = TestServer::start(Behavior {
        password: Some("right".into()),
        disconnect_after_failures: 3,
        ..Behavior::new("hardened-wrong")
    })
    .await;
    let agent_path = spawn_agent((), &keys).await;
    let home = TestHome::new("hardened-wrong");
    let env = SshClientEnv {
        agent: AgentSource::Endpoint(agent_path),
        ..home.env()
    };
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), env);
    let mut conn = connection("hardened-wrong", "127.0.0.1", server.port(), "u");
    conn.password = Some("s3cret-but-wrong".into());

    let err = pool.try_acquire(&conn).await.err().expect("wrong password");
    assert_eq!(err.kind, AcquireErrorKind::Auth, "{}", err.message);
    let message = &err.message;
    assert!(
        message.contains("the server closed the connection: Too many authentication failures"),
        "{message}"
    );
    assert!(
        message.contains("reconnected to try the password without offering public keys"),
        "{message}"
    );
    assert!(
        message.contains("password: rejected by server"),
        "{message}"
    );
    assert!(!message.contains("s3cret"), "password leaked: {message}");
    assert_eq!(server.accepted(), 2);

    let err = pool.try_acquire(&conn).await.err().expect("cooldown");
    assert_eq!(err.kind, AcquireErrorKind::Cooldown);
    assert_eq!(server.accepted(), 2, "no reconnect in cooldown");
    pool.shutdown().await;
}

/// 认证途中断了网(服务器直接关 TCP、不发 DISCONNECT,之前也没拒过任何方法):是网络错误,
/// 不冷却,报错不谎称「被服务器拒绝」。
#[tokio::test]
async fn connection_lost_during_authentication_is_a_network_error_without_cooldown() {
    let server = TestServer::start(Behavior {
        password: Some("pw".into()),
        drop_on_password: true,
        ..Behavior::new("flaky")
    })
    .await;
    let home = TestHome::new("lost-mid-auth");
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), home.env());
    let mut conn = connection("flaky", "127.0.0.1", server.port(), "u");
    conn.password = Some("pw".into());

    let err = pool.try_acquire(&conn).await.err().expect("dropped");
    assert_eq!(err.kind, AcquireErrorKind::Network, "{}", err.message);
    assert!(
        err.message.starts_with(&format!(
            "connection lost during authentication on u@127.0.0.1:{}: ",
            server.port()
        )),
        "{}",
        err.message
    );
    assert!(!err.message.contains("rejected"), "{}", err.message);

    let err = pool.try_acquire(&conn).await.err().expect("dropped again");
    assert_eq!(err.kind, AcquireErrorKind::Network, "not a cooldown");
    assert_eq!(server.accepted(), 2);
    pool.shutdown().await;
}

/// `IdentityFile` 指向公钥(1Password 推荐的写法)+ `IdentitiesOnly yes`:到 agent 里找
/// 同一把 key 签名;agent 里的其它 key 一把都不出示。
#[tokio::test]
async fn public_key_identity_file_signs_through_the_agent() {
    let wanted = ed25519(101);
    let other = ed25519(102);
    let server = TestServer::start(Behavior {
        keys: vec![wanted.public_key().clone()],
        ..Behavior::new("pubfile")
    })
    .await;
    let agent_path = spawn_agent((), &[other.clone(), wanted.clone()]).await;
    let home = TestHome::new("pubfile");
    std::fs::write(
        home.ssh("work.pub"),
        format!("{}\n", wanted.public_key().to_openssh().unwrap()),
    )
    .unwrap();
    home.config(&format!(
        "Host pubfile-host\n  HostName 127.0.0.1\n  Port {}\n  IdentitiesOnly yes\n  \
         IdentityFile ~/.ssh/work.pub\n",
        server.port()
    ));
    let env = SshClientEnv {
        agent: AgentSource::Endpoint(agent_path),
        ..home.env()
    };
    let pool = SshPool::with_env(pool_config(Duration::from_secs(10)), env);
    let session = pool
        .try_acquire(&connection("pubfile", "pubfile-host", 22, "u"))
        .await
        .expect("signed by the agent");
    assert_eq!(exec(&session, "k").await, "pubfile:k");
    let log = server.log();
    assert!(
        !log.iter()
            .any(|l| l.contains(&fingerprint(other.public_key()))),
        "IdentitiesOnly must hide other agent keys: {log:?}"
    );
    pool.shutdown().await;
}
