//! 建连 + 握手 + 认证的共用路径:会话池(含「测试连接」[`crate::SshPool::probe`])与终端
//! shell([`crate::shell::open_shell`])都走这里。自 `pool.rs` 抽出。
//!
//! 两种调用方的差别全在 [`ConnectOptions`] 里:
//!
//! - 会话池([`ConnectOptions::pool`]):没有交互提示器,建连整段受一个固定的 `connect_timeout`
//!   期限约束,行为与抽出之前一字不变 —— accept-new 静默追加 known_hosts、认证顺序、
//!   「重连只试密码」那一轮、报错文案;
//! - 终端 shell:带 [`Interaction`],首见主机 / 私钥口令 / 密码在终端里问用户;网络预算按
//!   [`crate::interact::PauseClock`] 计,等用户输入的时间不算;设 TCP_NODELAY(敲键的延迟);
//!   不开 russh 自带的 keepalive(认证前它会空转,见 `shell.rs` 模块注释),认证后由 shell 自己做。

use std::sync::Arc;
use std::time::Duration;

use mt_core::SshConnection;
use russh::client::{self, Handle};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::auth::{self, AgentSession, AuthError, AuthPlan, AuthRound, ClientNotes, PasswordInput};
use crate::interact::{Interaction, PauseClock};
use crate::known_hosts::{self, MtClient};
use crate::pool::{AcquireError, AcquireErrorKind, PoolConfig, SshClientEnv};
use crate::target::ResolvedHost;
use crate::transport::{self, ConnectStage, GuardedStream, KillOnDrop, StageTracker, StreamKill};

/// 一次建连的参数。
pub(crate) struct ConnectOptions<'a> {
    /// 建连依赖的本机环境(known_hosts / home / ssh-agent)。
    pub(crate) env: &'a SshClientEnv,
    /// DNS + TCP + 握手 + 认证整段的网络时间预算(终端 shell 不含等用户的时间)。
    pub(crate) connect_timeout: Duration,
    /// 多个地址时除最后一个外每个地址的 TCP 上限(见 `PoolConfig::connect_timeout_per_address`)。
    pub(crate) connect_timeout_per_address: Duration,
    /// russh 自带的 keepalive(只在认证后发)。`None` 不开。
    pub(crate) keepalive_interval: Option<Duration>,
    pub(crate) keepalive_max: usize,
    /// 设 TCP_NODELAY。交互式终端要(每个按键都是小包);池不设,与此前 `client::connect`
    /// (russh 默认 `nodelay = false`)的行为保持一致。
    pub(crate) nodelay: bool,
    /// 终端 shell 的交互提示器;池为 `None`。
    pub(crate) interaction: Option<&'a Arc<Interaction>>,
}

impl<'a> ConnectOptions<'a> {
    /// 会话池(含「测试连接」)的参数:没有交互提示器。
    pub(crate) fn pool(config: &PoolConfig, env: &'a SshClientEnv) -> Self {
        Self {
            env,
            connect_timeout: config.connect_timeout,
            connect_timeout_per_address: config.connect_timeout_per_address,
            keepalive_interval: Some(config.keepalive_interval),
            keepalive_max: config.keepalive_max,
            nodelay: false,
            interaction: None,
        }
    }
}

/// 建好的连接:会话 handle + 它底层传输的掐断开关 + 认证用上的方法 + 旁路信息。
pub(crate) struct Link {
    pub(crate) handle: Handle<MtClient>,
    pub(crate) transport: KillOnDrop,
    /// 认证成功用的哪一种(认证完才填)。只有 [`crate::SshPool::probe`] 读它,池里的会话不留。
    pub(crate) method: Option<auth::AuthMethodUsed>,
    /// 这条连接的 [`MtClient`] 共享出来的旁路信息。终端 shell 读它来说清连接是怎么断的。
    pub(crate) notes: Arc<ClientNotes>,
}

/// 建连期限:池是一个固定时刻;终端 shell 的期限随等用户输入的时间顺延。
enum Deadline<'a> {
    Fixed(tokio::time::Instant),
    Pausable {
        start: tokio::time::Instant,
        budget: Duration,
        clock: &'a PauseClock,
    },
}

impl Deadline<'_> {
    /// 此刻的期限(TCP 逐地址限时用)。
    fn at(&self) -> tokio::time::Instant {
        match self {
            Deadline::Fixed(deadline) => *deadline,
            Deadline::Pausable {
                start,
                budget,
                clock,
            } => clock.deadline(*start, *budget),
        }
    }
}

/// 在 `connect_timeout` 预算内建连 + 认证,超时报 `Timeout` 并写明卡在哪个阶段。
///
/// 超时丢弃建连 future 时,[`KillOnDrop`] 随之 drop、掐断传输,russh 已 spawn 的会话任务随即
/// 退出,不泄漏任务与 socket。
pub(crate) async fn connect_link(
    conn: &SshConnection,
    host: &ResolvedHost,
    opts: &ConnectOptions<'_>,
) -> Result<Link, AcquireError> {
    let target = format!("{}@{}", host.user, host.endpoint());
    let budget = opts.connect_timeout;
    let tracker = StageTracker::default();
    let Some(interaction) = opts.interaction else {
        let deadline = tokio::time::Instant::now() + budget;
        let fixed = Deadline::Fixed(deadline);
        let connecting = connect_target(conn, host, &fixed, &tracker, &target, opts);
        return match tokio::time::timeout_at(deadline, connecting).await {
            Ok(result) => result,
            Err(_) => Err(AcquireError::new(
                AcquireErrorKind::Timeout,
                tracker.describe_timeout(&target, budget),
            )),
        };
    };
    let start = tokio::time::Instant::now();
    let clock = interaction.clock();
    let deadline = Deadline::Pausable {
        start,
        budget,
        clock,
    };
    let connecting = connect_target(conn, host, &deadline, &tracker, &target, opts);
    match clock.timeout(start, budget, connecting).await {
        Ok(result) => result,
        Err(_) => Err(AcquireError::new(
            AcquireErrorKind::Timeout,
            tracker.describe_timeout(&target, budget),
        )),
    }
}

/// 建连并认证,返回建好的 [`Link`]。任何一步失败,已建好的连接随返回值 drop 而掐断。
///
/// 认证在密码轮到之前就被服务器断开(加固过的 sshd `MaxAuthTries` 只有 3、4,
/// agent 里 key 一多就会这样)时,重连一次只试密码与 keyboard-interactive,见
/// [`AuthRound::WithoutKeys`]。重连同样受 `connect_timeout` 约束,逐方法报错里写明;
/// 一次 acquire 不管试了几轮,认证失败只记一次冷却。
async fn connect_target(
    conn: &SshConnection,
    host: &ResolvedHost,
    deadline: &Deadline<'_>,
    tracker: &StageTracker,
    target: &str,
    opts: &ConnectOptions<'_>,
) -> Result<Link, AcquireError> {
    // 一次建连(含重连那一轮)共用一个 agent 连接(懒连接,用不到就不连)。
    let mut agent = AgentSession::new(opts.env.agent.clone());
    let label = host.endpoint();
    let auth_label = format!("{}@{}", host.user, host.endpoint());
    let password = reveal_password(conn.password.as_deref());
    let interaction = opts.interaction.map(|interaction| &**interaction);
    let mut link = open_target(host, &label, deadline, tracker, target, opts).await?;
    tracker.enter(&label, ConnectStage::Auth);
    let plan = AuthPlan {
        host,
        label: auth_label.clone(),
        password: &password,
        round: AuthRound::First,
        interaction,
    };
    let notes = Arc::clone(&link.notes);
    let first =
        auth::authenticate_host(&mut link.handle, &plan, &mut agent, &notes, tracker).await;
    let earlier = match first {
        Ok(()) => {
            link.method = notes.take_authenticated();
            return Ok(link);
        }
        Err(AuthError::RetryWithoutKeys { notes: earlier, .. }) => earlier,
        Err(other) => return Err(auth_error(other)),
    };

    eprintln!(
        "[mt-ssh] {auth_label}: the server closed the connection during public key \
         authentication before the password's turn (its MaxAuthTries is probably below the \
         default 6); reconnecting once to try the password without offering keys"
    );
    // 先掐断第一条连接,再连第二条。
    drop(link);
    let label = format!("{label} (reconnected for password authentication)");
    let mut link = open_target(host, &label, deadline, tracker, target, opts).await?;
    tracker.enter(&label, ConnectStage::Auth);
    let plan = AuthPlan {
        host,
        label: auth_label,
        password: &password,
        round: AuthRound::WithoutKeys { earlier },
        interaction,
    };
    let notes = Arc::clone(&link.notes);
    auth::authenticate_host(&mut link.handle, &plan, &mut agent, &notes, tracker)
        .await
        .map_err(auth_error)?;
    link.method = notes.take_authenticated();
    Ok(link)
}

/// 连上目标的 TCP 并握手。返回的 [`Link`] drop 即掐断传输;建到一半失败或超时丢弃时
/// 同样掐断。
async fn open_target(
    host: &ResolvedHost,
    label: &str,
    deadline: &Deadline<'_>,
    tracker: &StageTracker,
    target: &str,
    opts: &ConnectOptions<'_>,
) -> Result<Link, AcquireError> {
    let kill = StreamKill::new();
    // 先装好「drop 即掐断」的守卫再把流交给 russh:之后任何失败或超时丢弃本
    // future,它都会随之 drop。
    let transport = KillOnDrop::new(kill.clone());
    let notes = Arc::new(ClientNotes::default());
    let tcp = transport::connect_tcp(
        &host.host_name,
        host.port,
        label,
        deadline.at(),
        opts.connect_timeout_per_address,
        tracker,
    )
    .await
    .map_err(|e| match e {
        transport::TcpConnectError::Resolve(message)
        | transport::TcpConnectError::Failed(message) => AcquireError::new(
            AcquireErrorKind::Network,
            format!("ssh connect to {label} failed: {message}"),
        ),
        transport::TcpConnectError::DeadlineExceeded => AcquireError::new(
            AcquireErrorKind::Timeout,
            tracker.describe_timeout(target, opts.connect_timeout),
        ),
    })?;
    // 池不设 TCP_NODELAY:与此前 `client::connect`(russh 默认 `nodelay = false`)的行为保持
    // 一致。终端 shell 设上(见 `ConnectOptions::nodelay`),失败不要紧。
    if opts.nodelay {
        let _ = tcp.set_nodelay(true);
    }
    // 对端地址只给终端 shell 的首见主机提示用。
    let peer = opts
        .interaction
        .is_some()
        .then(|| tcp.peer_addr().ok())
        .flatten();
    tracker.enter(label, ConnectStage::Handshake);
    tracker.watch_stream(&kill);
    let handle = handshake(kill.wrap(tcp), host, label, &notes, opts, peer).await?;
    Ok(Link {
        handle,
        transport,
        method: None,
        notes,
    })
}

/// 在已建立的传输上做 SSH 握手(标识串 + 密钥交换 + 主机密钥校验)。
async fn handshake<S>(
    stream: GuardedStream<S>,
    host: &ResolvedHost,
    label: &str,
    notes: &Arc<ClientNotes>,
    opts: &ConnectOptions<'_>,
    peer: Option<std::net::SocketAddr>,
) -> Result<Handle<MtClient>, AcquireError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // 首选 host key 算法按 known_hosts 调序(见 `preferred_host_key_algorithms`)。
    // 这里读失败就沿用默认顺序 —— `check_server_key` 会再读一次,失败即拒绝并说明原因。
    let host_keys = known_hosts::read_known_hosts(&opts.env.known_hosts_path)
        .ok()
        .and_then(|raw| {
            known_hosts::preferred_host_key_algorithms(&known_hosts::known_host_key_types(
                &raw,
                &host.known_hosts_name,
            ))
        });
    // 不设 `inactivity_timeout`(russh 默认 None):
    // - 认证前它兜不住 —— keepalive 计时器在认证前到点后空转、每轮都把 inactivity
    //   计时器重置(见 transport 模块注释)。建连期的收尾改由 connect_timeout +
    //   传输掐断保证,有测试证明服务器一侧会立刻看到连接关闭;
    // - 认证后,服务器不回 keepalive 时 `keepalive_max` 次后 russh 已会断开;服务器
    //   正常回 keepalive 时,每个回包都会重置 inactivity 计时,设了也不会误杀长时间
    //   没有输出的会话 —— 但也没有任何增益。
    // 用 `..Default::default()` 一次性赋值,避开 clippy::field_reassign_with_default。
    let cfg = Arc::new(client::Config {
        keepalive_interval: opts.keepalive_interval,
        keepalive_max: opts.keepalive_max,
        preferred: transport::preferred_algorithms(host_keys),
        ..Default::default()
    });
    let mut handler = MtClient::new(
        host.known_hosts_name.clone(),
        opts.env.known_hosts_path.clone(),
        Arc::clone(notes),
    );
    if let Some(interaction) = opts.interaction {
        handler = handler.interactive(Arc::clone(interaction), peer);
    }
    client::connect_stream(cfg, stream, handler)
        .await
        .map_err(|e| match notes.take_rejection() {
            Some(reason) => AcquireError::new(
                AcquireErrorKind::HostKey,
                known_hosts::connect_error(label, &e, Some(reason)),
            ),
            None => AcquireError::new(
                AcquireErrorKind::Network,
                known_hosts::connect_error(label, &e, None),
            ),
        })
}

/// 解开目标连接存的密码(认证细节见 `auth.rs`)。
///
/// 配置里的 password 是 `mt-secret` 的信封(或升级窗口期的遗留明文),**在这里**
/// 解开 —— 主程序与三个 sidecar 共用这个池,解封点唯一就不会有谁忘了。
/// 解不开(密钥换了 / 密文被改)只记进报错(`password: stored password unavailable`),
/// 照样去试别的方法,绝不把密文当密码送出去;错误文本不含密码。
fn reveal_password(stored: Option<&str>) -> PasswordInput {
    match stored.filter(|p| !p.is_empty()) {
        Some(stored) => match mt_secret::reveal_global(stored) {
            Ok(password) => PasswordInput::Available(password),
            Err(e) => PasswordInput::Unavailable(e.to_string()),
        },
        None => PasswordInput::NotConfigured,
    }
}

/// 认证失败 → 池的错误分类。没有重连的「重连只试密码」按认证失败算。用户取消只在终端 shell
/// 出现(由它按取消处理),这里归到认证一类。
fn auth_error(error: AuthError) -> AcquireError {
    match error {
        AuthError::Rejected(message)
        | AuthError::RetryWithoutKeys { message, .. }
        | AuthError::Cancelled(message) => AcquireError::new(AcquireErrorKind::Auth, message),
        AuthError::Transport(message) => AcquireError::new(AcquireErrorKind::Network, message),
    }
}
