//! 终端交互式 shell 通道:用 russh 直接开 pty + shell / exec,远端字节直接进终端 VT,
//! 不经系统 `ssh.exe`、不经 ConPTY。
//!
//! # 宿主怎么用
//!
//! [`open_shell`] 建连 + 认证 + 申请 pty + 开 shell / exec,成功时返回 [`ShellSession`]。建连期间
//! 要显示的文本与要用户回答的问题从 `prompts` 通道送给宿主([`ShellPrompt`]),宿主须**并发**地
//! 处理它(回答 `ReadLine` 之前 `open_shell` 一直在等);`open_shell` 返回时(成败都算)这条通道
//! 的发送端随之丢弃,宿主那头的接收端就此结束。之后的输出经 `on_output`、结束经 `on_exit` 交付。
//!
//! 要问用户的只有这几种(全部沿用 OpenSSH 的英文原文):首见主机的 yes / no / 指纹
//! (`known_hosts.rs`;不符 / 吊销 / 读写失败直接拒绝,不问),带口令私钥的口令(`key_unlock.rs`),
//! 密码与 keyboard-interactive 的问询(`auth.rs`)。服务器的认证 banner 作为 [`ShellPrompt::Notice`]
//! 显示,控制字符先转义(`interact::terminal_safe`)。
//!
//! # 执行体
//!
//! 每个 shell 独占一条 SSH 连接(不复用会话池:sshd 的 `MaxSessions`、池的淘汰都不该影响终端)。
//! 建好之后有三个执行体:
//!
//! - **输出泵**(tokio 任务):`ChannelReadHalf::wait()` 读 Data / ExtendedData(stderr)/ 退出状态 /
//!   EOF / Close,数据推进一个有界队列;
//! - **输出线程**(独立 std 线程 `mt-ssh-shell-output`):从队列取数据调 `on_output`,最后调一次
//!   `on_exit`。emulator 推进可能很慢,所以不占 tokio worker;
//! - **驱动**(tokio 任务):按入队顺序执行写入 / 改尺寸,做 keepalive,负责断开。
//!
//! 读写分在两个任务里是必须的:写可能因服务器的 channel 窗口耗尽而挂起,若读也在同一个任务里就
//! 跟着停下 —— russh 的会话任务随之卡在投递上,连服务器调整窗口的消息都处理不了,两边互等。
//! [`ShellSession`] 的方法只往驱动的无界队列里放命令,绝不阻塞调用线程(GPUI 主线程)。
//!
//! # 背压(russh 0.61 源码里的结论)
//!
//! - russh 收到 `CHANNEL_DATA` 时**先**按收到的字节数补窗口(`Encrypted::adjust_window_size`:
//!   剩余窗口低于一半就补回 `client::Config::window_size`,默认 2 MiB),**再**把数据
//!   `send().await` 进这个 channel 的有界队列(`Config::channel_buffer_size`,默认 100 条;每条不超过
//!   我们通告的最大包 32 KiB)。所以 SSH 窗口只反映「russh 收到了」,不反映「应用读走了」,起不到
//!   应用层背压的作用;
//! - 真正的背压在那个 `send().await`:队列满时 russh 的会话任务整个停在那里、不再读 socket,
//!   服务器的发送被 TCP 窗口挡住,远端程序随之阻塞在写 pty 上。每个 shell 一条连接,停的只有它
//!   自己;
//! - 本模块在输出泵与输出线程之间再放一个有界队列(`OUTPUT_QUEUE` 块):输出线程慢 → 泵停在
//!   `send().await` → russh 的 channel 队列满 → 会话任务停读 socket,一路反压到远端。在途数据最多
//!   约是服务器侧 2 MiB 窗口 + russh 队列 + 本模块队列,与 OpenSSH(同为 2 MiB 窗口)同一量级;
//! - 会话任务停读期间,keepalive 的回包也读不进来:驱动把「输出正在积压」同样当作服务器还活着,
//!   免得把一个只是读得慢的会话误判成死链。
//!
//! # 超时与 keepalive
//!
//! - 建连(DNS / TCP / 握手 / 各次认证请求)共用 [`ShellConfig::connect_timeout`] 的网络时间预算,
//!   等用户输入(以及本地解私钥)的时间不计入(`interact::PauseClock`);认证之后开 channel + pty +
//!   shell 另受 [`ShellConfig::channel_timeout`] 约束;
//! - 用户思考期间服务器因 LoginGraceTime 断开时,报错写明「等用户期间被服务器断开」与等了多久
//!   (`auth.rs` 的 `lost_while_waiting` / `interactive_loss`),不是泛泛的超时;
//! - **不开 russh 自带的 keepalive**:它的计时器在认证前到点后不重新上弦,`select!` 每轮都立刻命中
//!   它而空转(见 `transport.rs` 模块注释)—— 会话池靠「建连 20s 内结束」躲开,终端里用户在密码
//!   提示前想一分钟就会把一个 CPU 核跑满。认证之后由驱动自己每隔一个间隔发一次
//!   `keepalive@openssh.com`(口径同 russh / OpenSSH `ServerAliveCountMax`):连续
//!   [`ShellConfig::keepalive_max`] 次没有应答、期间也没收到别的任何东西(且输出没在积压)即判定
//!   连接已死,掐断传输,[`ShellEnd::ConnectionLost`] 写 OpenSSH 的原文
//!   `Timeout, server … not responding.`。
//!
//! # 关闭口径
//!
//! [`ShellSession::close`](与 Drop 同效):EOF + 关 channel + DISCONNECT,限时
//! [`ShellConfig::close_timeout`] 后掐断传输;之后 `on_exit` 不再回调(与 `mt_pty::PtySession`
//! 的 Drop 口径一致:上层自己关的不算退出),`on_output` 也不再交付。远端退出(Close)或连接断开时
//! `on_exit` 恰好回调一次,在这之前的输出都已交付完。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use mt_core::SshConnection;
use russh::client::{Handle, Msg};
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf, Pty, Sig};
use tokio::sync::{mpsc, oneshot, watch};

use crate::auth::ClientNotes;
use crate::connect::{self, ConnectOptions, Link};
use crate::interact::Interaction;
use crate::known_hosts::MtClient;
use crate::pool::{AcquireError, AcquireErrorKind, SshClientEnv};
use crate::target::{self, ResolvedHost};
use crate::transport::{fmt_duration, KillOnDrop};

/// 输出泵与输出线程之间的队列长度(块数,每块不超过一个 SSH 包,32 KiB 以内)。
const OUTPUT_QUEUE: usize = 64;

/// `SSH_EXTENDED_DATA_STDERR`(RFC 4254 唯一定义的扩展数据类型)。
const SSH_EXTENDED_DATA_STDERR: u32 = 1;

/// 发一个 keepalive 请求最多等多久入队(会话任务正因输出积压停着时别把驱动也卡住)。
const KEEPALIVE_SEND_LIMIT: Duration = Duration::from_secs(5);

// ============================================================================
// 对外类型
// ============================================================================

/// 要开的 shell。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRequest {
    /// pty 的 TERM,例 `"xterm-256color"`。
    pub term: String,
    pub cols: u16,
    pub rows: u16,
    /// `None` → `shell` 请求(登录 shell);`Some(cmd)` → `exec(cmd)`
    /// (远程项目用 `cd '<path>' 2>/dev/null; exec $SHELL -l`,拼法见
    /// `mt_pty::ssh::build_remote_login_command`)。
    pub command: Option<String>,
    /// 逐条发 `env` 请求(`LANG` / `LC_*` / `COLORTERM` 等),尽力而为:服务器(sshd 的
    /// `AcceptEnv`)不收不算错。
    pub env: Vec<(String, String)>,
}

/// 建连期间要宿主在终端里显示 / 向用户要的东西。
#[derive(Debug)]
pub enum ShellPrompt {
    /// 原样显示到终端的文本(已拼好 `\r\n`;沿用 OpenSSH 的英文原文措辞)。
    Notice(String),
    /// 读一行:`prompt` 原样显示,`echo = false` 时不回显。回 `None`(或丢掉 `reply`)= 用户取消
    /// (Ctrl+C / 关 pane)。
    ReadLine {
        prompt: String,
        echo: bool,
        reply: oneshot::Sender<Option<String>>,
    },
}

/// [`open_shell`] 的可调参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellConfig {
    /// 建连的网络时间预算:DNS + TCP + 握手 + 各次认证请求,**不含**等用户输入的时间。默认 20s
    /// (与会话池一致)。
    pub connect_timeout: Duration,
    /// 一个主机名解析出多个地址时,除最后一个外每个地址的 TCP 上限。默认 5s。
    pub connect_timeout_per_address: Duration,
    /// 认证之后开 channel + pty + shell / exec 整段的上限。默认 10s。
    pub channel_timeout: Duration,
    /// keepalive(`keepalive@openssh.com`)间隔;`None` 不发、也不判死。默认 30s(与会话池一致)。
    pub keepalive_interval: Option<Duration>,
    /// 连续多少次 keepalive 没有应答(期间也没收到别的任何东西)就判定连接已死;0 = 只发不判。
    /// 默认 3(与会话池一致)。
    pub keepalive_max: usize,
    /// 主动关闭时 EOF / 关 channel / DISCONNECT 的上限,到点掐断传输。默认 2s。
    pub close_timeout: Duration,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(20),
            connect_timeout_per_address: Duration::from_secs(5),
            channel_timeout: Duration::from_secs(10),
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 3,
            close_timeout: Duration::from_secs(2),
        }
    }
}

/// [`open_shell`] 失败的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellOpenErrorKind {
    /// 用户在终端里取消了某个提问(或宿主不再处理提示)。
    Cancelled,
    /// 连接配置本身的问题(主机为空、没有用户名……),重试无用。
    Config,
    /// 主机密钥不符 / 已吊销 / known_hosts 读写失败,或用户对首见主机答了 no。
    HostKey,
    /// 服务器拒绝了全部认证方法(或根本没有可用的凭据)。
    Auth,
    /// DNS、TCP、握手失败,或建连途中连接断了。
    Network,
    /// 网络时间预算内没建好连接,或认证后开 shell 超时;消息写明卡在哪个阶段。
    Timeout,
    /// 认证之后开 channel / shell / exec 被服务器拒绝。
    Channel,
}

/// [`open_shell`] 的错误。`message` 英文开头,绝不含密码或私钥内容(主机密钥拒绝原因后面附
/// 中文细节,与会话池同一口径)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOpenError {
    pub kind: ShellOpenErrorKind,
    pub message: String,
}

impl ShellOpenError {
    fn new(kind: ShellOpenErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ShellOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ShellOpenError {}

/// shell 结束(远端退出或连接断开)时交给 `on_exit` 的信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellExit {
    /// 远端程序的退出码(服务器发了 `exit-status` 才有)。
    pub code: Option<u32>,
    /// 远端程序被信号终止时的信号名(`TERM` / `KILL` / `HUP` ……,服务器发了 `exit-signal` 才有)。
    pub signal: Option<String>,
    pub reason: ShellEnd,
}

/// shell 是怎么结束的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEnd {
    /// 远端程序退出了(收到了 `exit-status` 或 `exit-signal`)。
    Exited,
    /// 服务器关了 channel(EOF / Close),但没给退出状态。
    Closed,
    /// channel 还没关连接就没了:服务器断开、网络断了或 keepalive 判死。附可读原因(OpenSSH 风格)。
    ConnectionLost(String),
}

/// 一个开着的终端 shell。所有方法都只往驱动任务的队列里放命令,不阻塞调用线程。
pub struct ShellSession {
    commands: mpsc::UnboundedSender<Command>,
    shared: Arc<Shared>,
    /// 上次下发的尺寸 `(cols, rows)`,[`Self::resize`] 用它去重。
    size: Mutex<(u16, u16)>,
    target_label: String,
}

impl ShellSession {
    /// 写入远端(用户键入、粘贴)。入队即返回,绝不阻塞调用线程;会话已结束 / 已关闭返回 `Err`。
    /// 多次写保序。
    pub fn write(&self, bytes: &[u8]) -> Result<(), String> {
        self.ensure_open()?;
        if bytes.is_empty() {
            return Ok(());
        }
        self.send(Command::Data(bytes.to_vec()))
    }

    /// 改 pty 尺寸。与上次下发的尺寸相同就不发,返回是否真的下发了 `window-change`。
    /// SSH 的 window-change 一步到位,不必像 ConPTY 那样逐格走。
    pub fn resize(&self, cols: u16, rows: u16) -> Result<bool, String> {
        self.ensure_open()?;
        let mut size = lock(&self.size);
        if *size == (cols, rows) {
            return Ok(false);
        }
        self.send(Command::Resize { cols, rows })?;
        *size = (cols, rows);
        Ok(true)
    }

    /// 主动关闭:EOF + 关 channel + DISCONNECT,限时后掐断传输。之后 `on_exit` 不再回调
    /// (上层自己关的不算退出),`on_output` 也不再交付。幂等;Drop 同效。不阻塞调用线程。
    ///
    /// 若远端恰好同时退出、`on_exit` 已经先一步开始回调,它会照常跑完 —— 两者在
    /// 「退出 / 关闭」只认先到的那一个。
    pub fn close(&self) {
        self.shared.close();
        let _ = self.commands.send(Command::Close);
    }

    /// `user@host:port`,显示用。
    pub fn target_label(&self) -> &str {
        &self.target_label
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.shared.is_ended() {
            Err(format!("the SSH session to {} has ended", self.target_label))
        } else {
            Ok(())
        }
    }

    fn send(&self, command: Command) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| format!("the SSH session to {} has ended", self.target_label))
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        self.close();
    }
}

impl std::fmt::Debug for ShellSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellSession")
            .field("target", &self.target_label)
            .field("ended", &self.shared.is_ended())
            .finish()
    }
}

// ============================================================================
// 建连
// ============================================================================

/// 建连 + 认证(按需经 `prompts` 问用户)+ 申请 pty + 开 shell / exec。
///
/// 返回 `Ok` 时 shell 已就绪,输出泵已经在跑:`on_output` 在**独立 std 线程**上调用(不占 tokio
/// worker),远端退出或连接断开时 `on_exit` 在同一线程上恰好调用一次。失败时两者直接丢弃、不调用。
///
/// 必须在 tokio 运行时上下文里 await(会 spawn 任务);调用方放到 mt-remote 的共享运行时上即可。
/// `prompts` 要由宿主并发地处理(见模块注释),本函数返回时它的发送端随之丢弃。
pub async fn open_shell<F, E>(
    conn: &SshConnection,
    env: &SshClientEnv,
    config: &ShellConfig,
    request: ShellRequest,
    prompts: mpsc::UnboundedSender<ShellPrompt>,
    on_output: F,
    on_exit: E,
) -> Result<ShellSession, ShellOpenError>
where
    F: FnMut(&[u8]) + Send + 'static,
    E: FnOnce(ShellExit) + Send + 'static,
{
    let interaction = Arc::new(Interaction::new(prompts));
    // 无论成败(含调用方丢弃本 future),返回前摘掉提示通道:宿主那头的接收端随即结束。
    let _finish = FinishPrompts(&interaction);
    let host = target::resolve_target(conn, env.home_dir.as_deref(), env.local_user.as_deref())
        .map_err(|why| ShellOpenError::new(ShellOpenErrorKind::Config, why))?;
    let target_label = format!("{}@{}", host.user, host.endpoint());
    let opts = ConnectOptions {
        env,
        connect_timeout: config.connect_timeout,
        connect_timeout_per_address: config.connect_timeout_per_address,
        keepalive_interval: None,
        keepalive_max: 0,
        nodelay: true,
        interaction: Some(&interaction),
    };
    let link = connect::connect_link(conn, &host, &opts)
        .await
        .map_err(|error| open_error(error, &interaction, &target_label))?;
    let Link {
        handle,
        transport,
        notes,
        ..
    } = link;

    let opening = open_channel(&handle, &request, &interaction, &notes, &host);
    let opened = match tokio::time::timeout(config.channel_timeout, opening).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(ShellOpenError::new(
                ShellOpenErrorKind::Timeout,
                format!(
                    "opening the shell on {target_label} timed out after {} (no reply to the \
                     session channel / pty / shell requests)",
                    fmt_duration(config.channel_timeout)
                ),
            ));
        }
    };

    let (stop, stopping) = watch::channel(false);
    let shared = Arc::new(Shared::new(stop));
    let (output, output_rx) = mpsc::channel(OUTPUT_QUEUE);
    if !opened.early.is_empty() {
        // 队列是空的,放得下。
        let _ = output.try_send(Delivery::Output(opened.early));
    }
    let delivering = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("mt-ssh-shell-output".into())
        .spawn(move || deliver(output_rx, &delivering, on_output, on_exit))
        .map_err(|e| {
            ShellOpenError::new(
                ShellOpenErrorKind::Channel,
                format!("cannot start the shell output thread: {e}"),
            )
        })?;
    tokio::spawn(pump(
        opened.read,
        output,
        Arc::clone(&shared),
        stopping.clone(),
        notes,
        (host.host_name.clone(), host.port),
    ));
    let (commands, command_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive(Driver {
        handle,
        transport,
        write: opened.write,
        commands: command_rx,
        shared: Arc::clone(&shared),
        stopping,
        keepalive_interval: config.keepalive_interval,
        keepalive_max: config.keepalive_max,
        close_timeout: config.close_timeout,
        host_name: host.host_name.clone(),
    }));
    Ok(ShellSession {
        commands,
        shared,
        size: Mutex::new((request.cols, request.rows)),
        target_label,
    })
}

/// drop 即摘掉提示通道。
struct FinishPrompts<'a>(&'a Interaction);

impl Drop for FinishPrompts<'_> {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// 建连错误 → shell 的错误分类。用户取消、对首见主机答 no 优先于建连本身报的分类。
fn open_error(error: AcquireError, interaction: &Interaction, target: &str) -> ShellOpenError {
    if interaction.is_cancelled() {
        return ShellOpenError::new(
            ShellOpenErrorKind::Cancelled,
            format!("ssh connection to {target} was cancelled"),
        );
    }
    if interaction.host_key_declined() {
        return ShellOpenError::new(
            ShellOpenErrorKind::HostKey,
            "Host key verification failed.",
        );
    }
    let kind = match error.kind {
        AcquireErrorKind::Config => ShellOpenErrorKind::Config,
        AcquireErrorKind::Network => ShellOpenErrorKind::Network,
        AcquireErrorKind::Timeout => ShellOpenErrorKind::Timeout,
        AcquireErrorKind::HostKey => ShellOpenErrorKind::HostKey,
        AcquireErrorKind::Auth | AcquireErrorKind::Cooldown => ShellOpenErrorKind::Auth,
    };
    ShellOpenError::new(kind, error.message)
}

/// 开好的 channel:读写两半 + shell 确认之前就到了的输出。
struct OpenedChannel {
    read: ChannelReadHalf,
    write: ChannelWriteHalf<Msg>,
    early: Vec<u8>,
}

/// 开 session channel,发 `pty-req` / `env` / `shell` 或 `exec`,按顺序等两个回复(`env` 不要回复)。
/// pty 被拒时照 OpenSSH 提示一行后继续;shell / exec 被拒算 [`ShellOpenErrorKind::Channel`]。
async fn open_channel(
    handle: &Handle<MtClient>,
    request: &ShellRequest,
    interaction: &Interaction,
    notes: &ClientNotes,
    host: &ResolvedHost,
) -> Result<OpenedChannel, ShellOpenError> {
    let lost = |what: &str| {
        ShellOpenError::new(
            ShellOpenErrorKind::Network,
            format!(
                "ssh connection to {} lost while {what}: {}",
                host.endpoint(),
                lost_reason(notes, &host.host_name, host.port)
            ),
        )
    };
    let channel = match handle.channel_open_session().await {
        Ok(channel) => channel,
        Err(russh::Error::ChannelOpenFailure(reason)) => {
            return Err(ShellOpenError::new(
                ShellOpenErrorKind::Channel,
                format!("the server refused to open a session channel ({reason:?})"),
            ));
        }
        Err(_) if handle.is_closed() => return Err(lost("opening the session channel")),
        Err(e) => {
            return Err(ShellOpenError::new(
                ShellOpenErrorKind::Channel,
                format!("opening a session channel failed: {e}"),
            ));
        }
    };
    let (mut read, write) = channel.split();
    let sent = async {
        write
            .request_pty(
                true,
                &request.term,
                u32::from(request.cols),
                u32::from(request.rows),
                0,
                0,
                &terminal_modes(),
            )
            .await?;
        for (name, value) in &request.env {
            write.set_env(false, name.as_str(), value.as_str()).await?;
        }
        match &request.command {
            None => write.request_shell(true).await,
            Some(command) => write.exec(true, command.as_bytes()).await,
        }
    };
    if sent.await.is_err() {
        return Err(lost("starting the shell"));
    }
    let what = if request.command.is_some() {
        "command"
    } else {
        "shell"
    };
    let mut early = Vec::new();
    let pty = next_reply(&mut read, &mut early).await;
    let started = match pty {
        Ok(granted) => {
            if !granted {
                interaction.notice("PTY allocation request failed on channel 0\r\n");
            }
            next_reply(&mut read, &mut early).await
        }
        Err(end) => Err(end),
    };
    match started {
        Ok(true) => Ok(OpenedChannel { read, write, early }),
        Ok(false) => Err(ShellOpenError::new(
            ShellOpenErrorKind::Channel,
            format!("the server refused to start the {what}"),
        )),
        Err(ChannelEnded) if handle.is_closed() => Err(lost("starting the shell")),
        Err(ChannelEnded) => Err(ShellOpenError::new(
            ShellOpenErrorKind::Channel,
            format!("the server closed the channel before the {what} started"),
        )),
    }
}

/// 开 shell 期间 channel 就没了。
struct ChannelEnded;

/// 等下一个 channel 请求的回复(`want_reply = true` 的请求按发出顺序回复)。期间到的输出攒进
/// `early`,稍后第一个交付。
async fn next_reply(read: &mut ChannelReadHalf, early: &mut Vec<u8>) -> Result<bool, ChannelEnded> {
    loop {
        match read.wait().await {
            Some(ChannelMsg::Success) => return Ok(true),
            Some(ChannelMsg::Failure) => return Ok(false),
            Some(ChannelMsg::Data { data }) => early.extend_from_slice(&data),
            Some(ChannelMsg::ExtendedData {
                data,
                ext: SSH_EXTENDED_DATA_STDERR,
            }) => early.extend_from_slice(&data),
            Some(ChannelMsg::Close) | None => return Err(ChannelEnded),
            Some(_) => {}
        }
    }
}

/// pty 的终端模式:至少 `IUTF8 = 1`(远端行编辑按 UTF-8 退格)、`VERASE = 127`(与终端退格键发的
/// DEL 一致),其余取 Linux 上 OpenSSH 从本地 xterm 转发过去的常见值。不含 `TTY_OP_END`
/// (russh 自己补,放进来会把长度算错)。
fn terminal_modes() -> Vec<(Pty, u32)> {
    vec![
        (Pty::VINTR, 3),
        (Pty::VQUIT, 28),
        (Pty::VERASE, 127),
        (Pty::VKILL, 21),
        (Pty::VEOF, 4),
        (Pty::VEOL, 255),
        (Pty::VEOL2, 255),
        (Pty::VSTART, 17),
        (Pty::VSTOP, 19),
        (Pty::VSUSP, 26),
        (Pty::VREPRINT, 18),
        (Pty::VWERASE, 23),
        (Pty::VLNEXT, 22),
        (Pty::VDISCARD, 15),
        (Pty::IGNPAR, 0),
        (Pty::PARMRK, 0),
        (Pty::INPCK, 0),
        (Pty::ISTRIP, 0),
        (Pty::INLCR, 0),
        (Pty::IGNCR, 0),
        (Pty::ICRNL, 1),
        (Pty::IUCLC, 0),
        (Pty::IXON, 1),
        (Pty::IXANY, 0),
        (Pty::IXOFF, 0),
        (Pty::IMAXBEL, 1),
        (Pty::IUTF8, 1),
        (Pty::ISIG, 1),
        (Pty::ICANON, 1),
        (Pty::XCASE, 0),
        (Pty::ECHO, 1),
        (Pty::ECHOE, 1),
        (Pty::ECHOK, 1),
        (Pty::ECHONL, 0),
        (Pty::NOFLSH, 0),
        (Pty::TOSTOP, 0),
        (Pty::IEXTEN, 1),
        (Pty::ECHOCTL, 1),
        (Pty::ECHOKE, 1),
        (Pty::PENDIN, 0),
        (Pty::OPOST, 1),
        (Pty::OLCUC, 0),
        (Pty::ONLCR, 1),
        (Pty::OCRNL, 0),
        (Pty::ONOCR, 0),
        (Pty::ONLRET, 0),
        (Pty::CS8, 1),
        (Pty::PARENB, 0),
        (Pty::PARODD, 0),
        (Pty::TTY_OP_ISPEED, 38400),
        (Pty::TTY_OP_OSPEED, 38400),
    ]
}

/// 连接是怎么断的(OpenSSH 风格的原文)。
fn lost_reason(notes: &ClientNotes, host: &str, port: u16) -> String {
    match (notes.ended_io(), notes.ended()) {
        (Some(std::io::ErrorKind::UnexpectedEof), _) => {
            format!("Connection to {host} closed by remote host.")
        }
        (Some(std::io::ErrorKind::ConnectionReset), _) => {
            format!("Connection reset by {host} port {port}")
        }
        (_, Some(end)) => end.to_string(),
        (_, None) => format!("the connection to {host} was closed"),
    }
}

// ============================================================================
// 会话期间:共享状态、输出泵、输出线程、驱动
// ============================================================================

const STATE_RUNNING: u8 = 0;
/// 上层关掉的:不再交付输出,不回调 `on_exit`。
const STATE_CLOSED: u8 = 1;
/// `on_exit` 已回调。
const STATE_EXITED: u8 = 2;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 三个执行体与 [`ShellSession`] 共享的状态。
struct Shared {
    /// [`STATE_RUNNING`] / [`STATE_CLOSED`] / [`STATE_EXITED`]:「退出」与「关闭」只认先到的那个。
    state: AtomicU8,
    /// 会话已结束(远端退出、连接断开或上层关闭):写入与改尺寸一律报错。
    ended: AtomicBool,
    /// 置 `true` = 输出泵与驱动停下(上层关闭,或远端已退出、该断开连接了)。
    stop: watch::Sender<bool>,
    /// 驱动判定连接已死的原因(keepalive),输出泵报 `ConnectionLost` 时优先用它。
    lost: Mutex<Option<String>>,
    /// 输出泵从 channel 收到的消息数,keepalive 用来判断会话还在动。
    progress: AtomicU64,
    /// 输出泵正等着输出线程腾地方(输出积压中)。
    backlogged: AtomicBool,
}

impl Shared {
    fn new(stop: watch::Sender<bool>) -> Self {
        Self {
            state: AtomicU8::new(STATE_RUNNING),
            ended: AtomicBool::new(false),
            stop,
            lost: Mutex::new(None),
            progress: AtomicU64::new(0),
            backlogged: AtomicBool::new(false),
        }
    }

    fn is_running(&self) -> bool {
        self.state.load(Ordering::SeqCst) == STATE_RUNNING
    }

    fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst) || !self.is_running()
    }

    /// 上层关闭:之后不再交付输出、不回调 `on_exit`(退出若已先到,照常)。
    fn close(&self) {
        let _ = self.state.compare_exchange(
            STATE_RUNNING,
            STATE_CLOSED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        self.ended.store(true, Ordering::SeqCst);
        self.request_stop();
    }

    /// 认定退出:返回 `true` 才可以回调 `on_exit`(上层没先一步关掉)。
    fn finish(&self) -> bool {
        self.state
            .compare_exchange(
                STATE_RUNNING,
                STATE_EXITED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    fn request_stop(&self) {
        self.stop.send_replace(true);
    }

    fn set_lost(&self, reason: String) {
        lock(&self.lost).get_or_insert(reason);
    }

    fn lost(&self) -> Option<String> {
        lock(&self.lost).clone()
    }
}

/// 等到「该停了」(发送端没了也算)。
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

/// 输出泵交给输出线程的东西。
enum Delivery {
    Output(Vec<u8>),
    Exit(ShellExit),
}

/// 输出线程:按序交付输出,最后回调一次 `on_exit`。上层关闭后不再交付。
fn deliver<F, E>(mut queue: mpsc::Receiver<Delivery>, shared: &Shared, mut on_output: F, on_exit: E)
where
    F: FnMut(&[u8]),
    E: FnOnce(ShellExit),
{
    let mut on_exit = Some(on_exit);
    while let Some(delivery) = queue.blocking_recv() {
        match delivery {
            Delivery::Output(bytes) => {
                if shared.is_running() {
                    on_output(&bytes);
                }
            }
            Delivery::Exit(exit) => {
                if shared.finish() {
                    if let Some(on_exit) = on_exit.take() {
                        on_exit(exit);
                    }
                }
                break;
            }
        }
    }
}

/// 输出泵收到的退出相关消息。
#[derive(Default)]
struct ExitTrack {
    code: Option<u32>,
    signal: Option<String>,
    eof: bool,
    closed: bool,
}

/// 输出泵:读 channel,数据推给输出线程(队列满时停下,背压一路传到远端,见模块注释);channel
/// 关闭或连接断开时把结论交给输出线程,并让驱动断开连接。上层关闭时直接停下、不报退出。
async fn pump(
    mut read: ChannelReadHalf,
    output: mpsc::Sender<Delivery>,
    shared: Arc<Shared>,
    mut stopping: watch::Receiver<bool>,
    notes: Arc<ClientNotes>,
    (host, port): (String, u16),
) {
    let mut exit = ExitTrack::default();
    loop {
        let message = tokio::select! {
            biased;
            () = stopped(&mut stopping) => return,
            message = read.wait() => message,
        };
        shared.progress.fetch_add(1, Ordering::Relaxed);
        match message {
            Some(ChannelMsg::Data { data })
            | Some(ChannelMsg::ExtendedData {
                data,
                ext: SSH_EXTENDED_DATA_STDERR,
            }) => {
                shared.backlogged.store(true, Ordering::Relaxed);
                let sent = tokio::select! {
                    biased;
                    () = stopped(&mut stopping) => return,
                    sent = output.send(Delivery::Output(data.to_vec())) => sent,
                };
                shared.backlogged.store(false, Ordering::Relaxed);
                if sent.is_err() {
                    // 输出线程没了(`on_output` panic 之类):没人收了,断开连接收场。
                    shared.ended.store(true, Ordering::SeqCst);
                    shared.request_stop();
                    return;
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => exit.code = Some(exit_status),
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                exit.signal = Some(signal_text(&signal_name));
            }
            Some(ChannelMsg::Eof) => exit.eof = true,
            Some(ChannelMsg::Close) => {
                exit.closed = true;
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    shared.ended.store(true, Ordering::SeqCst);
    let reason = if exit.code.is_some() || exit.signal.is_some() {
        ShellEnd::Exited
    } else if exit.eof || exit.closed {
        ShellEnd::Closed
    } else {
        ShellEnd::ConnectionLost(
            shared
                .lost()
                .unwrap_or_else(|| lost_reason(&notes, &host, port)),
        )
    };
    let finished = ShellExit {
        code: exit.code,
        signal: exit.signal,
        reason,
    };
    let _ = output.send(Delivery::Exit(finished)).await;
    // 远端已退出 / 连接已断:让驱动断开(连接还在的话)并收尾。
    shared.request_stop();
}

/// 信号名(`exit-signal` 里去掉 `SIG` 前缀的那种写法)。
fn signal_text(signal: &Sig) -> String {
    let name = match signal {
        Sig::ABRT => "ABRT",
        Sig::ALRM => "ALRM",
        Sig::FPE => "FPE",
        Sig::HUP => "HUP",
        Sig::ILL => "ILL",
        Sig::INT => "INT",
        Sig::KILL => "KILL",
        Sig::PIPE => "PIPE",
        Sig::QUIT => "QUIT",
        Sig::SEGV => "SEGV",
        Sig::TERM => "TERM",
        Sig::USR1 => "USR1",
        Sig::Custom(name) => name.as_str(),
    };
    name.to_string()
}

/// 驱动任务收到的命令(`ShellSession` 的方法入队,按序执行)。
enum Command {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// 驱动任务持有的东西:连接本身(handle + 传输开关)与 channel 的写半边。
struct Driver {
    handle: Handle<MtClient>,
    transport: KillOnDrop,
    write: ChannelWriteHalf<Msg>,
    commands: mpsc::UnboundedReceiver<Command>,
    shared: Arc<Shared>,
    stopping: watch::Receiver<bool>,
    keepalive_interval: Option<Duration>,
    keepalive_max: usize,
    close_timeout: Duration,
    host_name: String,
}

/// 驱动:按序写入 / 改尺寸、keepalive;停下时(上层关闭、远端退出)限时优雅断开,最后掐断传输。
/// keepalive 判死或会话任务已经没了时不再优雅断开,直接掐断 —— 输出泵随即看到连接没了,报
/// `ConnectionLost`。
async fn drive(mut driver: Driver) {
    let mut keepalive = driver.keepalive_interval.map(|every| {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker
    });
    let mut seen = activity(&driver);
    let mut silent = 0usize;
    let graceful = 'drive: loop {
        tokio::select! {
            biased;
            () = stopped(&mut driver.stopping) => break 'drive true,
            command = driver.commands.recv() => match command {
                Some(Command::Data(bytes)) => {
                    // 服务器的窗口耗尽时这里会挂起:同时盯着「该停了」。
                    let sent = tokio::select! {
                        biased;
                        () = stopped(&mut driver.stopping) => break 'drive true,
                        sent = driver.write.data_bytes(bytes) => sent,
                    };
                    if sent.is_err() {
                        break 'drive false;
                    }
                }
                Some(Command::Resize { cols, rows }) => {
                    let sent = tokio::select! {
                        biased;
                        () = stopped(&mut driver.stopping) => break 'drive true,
                        sent = driver.write.window_change(
                            u32::from(cols),
                            u32::from(rows),
                            0,
                            0,
                        ) => sent,
                    };
                    if sent.is_err() {
                        break 'drive false;
                    }
                }
                Some(Command::Close) | None => break 'drive true,
            },
            () = tick(&mut keepalive) => {
                let now = activity(&driver);
                if now != seen || driver.shared.backlogged.load(Ordering::Relaxed) {
                    silent = 0;
                } else {
                    silent += 1;
                }
                seen = now;
                if driver.keepalive_max != 0 && silent > driver.keepalive_max {
                    driver
                        .shared
                        .set_lost(format!("Timeout, server {} not responding.", driver.host_name));
                    break 'drive false;
                }
                // 发出去即可:回包(REQUEST_SUCCESS / FAILURE)会让下一轮看到「收到了东西」。
                let _ = tokio::time::timeout(
                    KEEPALIVE_SEND_LIMIT,
                    driver.handle.send_keepalive(true),
                )
                .await;
            }
        }
    };
    driver.shared.ended.store(true, Ordering::SeqCst);
    if graceful && !driver.handle.is_closed() {
        let (handle, write) = (&driver.handle, &driver.write);
        let closing = async {
            let _ = write.eof().await;
            let _ = write.close().await;
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await;
            while !handle.is_closed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let _ = tokio::time::timeout(driver.close_timeout, closing).await;
    }
    driver.transport.kill();
}

/// 会话还在动的迹象:从服务器读到的字节数、输出泵收到的消息数。
fn activity(driver: &Driver) -> (u64, u64) {
    (
        driver.transport.bytes_read(),
        driver.shared.progress.load(Ordering::Relaxed),
    )
}

async fn tick(ticker: &mut Option<tokio::time::Interval>) {
    match ticker {
        Some(ticker) => {
            ticker.tick().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_config_defaults_match_the_pool_where_they_overlap() {
        let config = ShellConfig::default();
        let pool = crate::PoolConfig::default();
        assert_eq!(config.connect_timeout, pool.connect_timeout);
        assert_eq!(
            config.connect_timeout_per_address,
            pool.connect_timeout_per_address
        );
        assert_eq!(config.keepalive_interval, Some(pool.keepalive_interval));
        assert_eq!(config.keepalive_max, pool.keepalive_max);
    }

    #[test]
    fn terminal_modes_carry_utf8_and_erase_without_the_end_marker() {
        let modes = terminal_modes();
        assert!(modes.contains(&(Pty::IUTF8, 1)));
        assert!(modes.contains(&(Pty::VERASE, 127)));
        assert!(!modes.iter().any(|(mode, _)| *mode == Pty::TTY_OP_END));
    }

    #[test]
    fn exit_and_close_race_is_decided_by_whichever_comes_first() {
        let (stop, _stopping) = watch::channel(false);
        let shared = Shared::new(stop);
        assert!(shared.finish(), "没关过:退出照常回调");
        shared.close();
        assert!(shared.is_ended());

        let (stop, mut stopping) = watch::channel(false);
        let shared = Shared::new(stop);
        shared.close();
        assert!(!shared.finish(), "关过之后不再回调退出");
        assert!(*stopping.borrow_and_update(), "关闭让泵与驱动停下");
    }
}
