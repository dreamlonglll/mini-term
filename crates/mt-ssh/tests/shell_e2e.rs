//! 终端 shell 端到端测试:进程内 russh 服务器对 `mt_ssh::open_shell`。
//!
//! 只连 127.0.0.1 上本测试自己起的服务器;known_hosts 一律是测试自己的临时文件
//! (`SshClientEnv::isolated`),**绝不碰**真实的 `~/.ssh` 与 ssh-agent。密码用明文
//! (`mt_secret::reveal_global` 对非信封值原样放行,不碰凭据密钥文件)。

use std::borrow::Cow;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::task::AtomicWaker;
use mt_core::SshConnection;
use mt_ssh::russh;
use mt_ssh::{
    PoolConfig, ShellConfig, ShellEnd, ShellExit, ShellOpenError, ShellOpenErrorKind, ShellPrompt,
    ShellRequest, ShellSession, SshClientEnv, SshPool,
};
use russh::keys::{HashAlg, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet, Pty, Sig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

/// 服务器主机密钥的种子(与 pool_e2e 同一把)。
const SERVER_KEY_SEED: u8 = 200;

/// `ssh-keygen -t ed25519 -N mt-ssh-test` 一次性生成的**测试专用废弃密钥**(口令
/// `mt-ssh-test`),只用于验证「在终端里问私钥口令」,不对应任何真实主机 / 账号。
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

const HOST_KEY_QUESTION: &str =
    "Are you sure you want to continue connecting (yes/no/[fingerprint])? ";

const WAIT: Duration = Duration::from_secs(10);

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

fn server_fingerprint() -> String {
    fingerprint(ed25519(SERVER_KEY_SEED).public_key())
}

/// 测试专用目录(known_hosts、私钥文件),drop 时删除。
struct TestDir {
    root: PathBuf,
}

impl TestDir {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mt-ssh-shell-{}-{}-{name}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn known_hosts_path(&self) -> PathBuf {
        self.root.join("known_hosts")
    }

    fn env(&self) -> SshClientEnv {
        SshClientEnv::isolated(self.known_hosts_path())
    }

    fn known_hosts(&self) -> String {
        std::fs::read_to_string(self.known_hosts_path()).unwrap_or_default()
    }

    /// 预先登记服务器的主机密钥(免得每个用例都先答一遍 yes)。
    fn trust(&self, port: u16) {
        let key = ed25519(SERVER_KEY_SEED).public_key().to_openssh().unwrap();
        std::fs::write(
            self.known_hosts_path(),
            format!("[127.0.0.1]:{port} {key}\n"),
        )
        .unwrap();
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn connection(port: u16) -> SshConnection {
    SshConnection {
        id: "shell".into(),
        name: "shell".into(),
        host: "127.0.0.1".into(),
        port,
        user: "u".into(),
        password: None,
        identity_file: None,
        group: None,
        extra: Default::default(),
    }
}

fn with_password(port: u16, password: &str) -> SshConnection {
    SshConnection {
        password: Some(password.into()),
        ..connection(port)
    }
}

fn request() -> ShellRequest {
    ShellRequest {
        term: "xterm-256color".into(),
        cols: 120,
        rows: 40,
        command: None,
        env: vec![
            ("LANG".into(), "en_US.UTF-8".into()),
            ("COLORTERM".into(), "truecolor".into()),
        ],
    }
}

// ============================================================================
// 进程内 SSH 服务器
// ============================================================================

#[derive(Clone)]
struct Kbd {
    name: &'static str,
    instructions: &'static str,
    prompts: Vec<(&'static str, bool)>,
    answers: Vec<&'static str>,
}

#[derive(Clone)]
struct Spec {
    methods: Vec<MethodKind>,
    password: Option<&'static str>,
    keys: Vec<PublicKey>,
    kbd: Option<Kbd>,
    banner: Option<&'static str>,
    refuse_pty: bool,
    refuse_shell: bool,
    /// 没认证完就在这个时长后掐断 TCP(模拟 sshd 的 LoginGraceTime)。
    grace: Option<Duration>,
}

impl Spec {
    fn password(password: &'static str) -> Self {
        Self {
            methods: vec![MethodKind::Password],
            password: Some(password),
            keys: Vec::new(),
            kbd: None,
            banner: None,
            refuse_pty: false,
            refuse_shell: false,
            grace: None,
        }
    }
}

/// 服务器一侧 TCP 流的「掐断」开关:掐断后读到 EOF、写报错,russh 服务端随即收场关 socket。
#[derive(Default)]
struct Sever {
    cut: AtomicBool,
    waker: AtomicWaker,
}

impl Sever {
    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        self.waker.wake();
    }
}

struct SeverableStream {
    inner: TcpStream,
    sever: Arc<Sever>,
}

impl AsyncRead for SeverableStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.sever.waker.register(cx.waker());
        if this.sever.cut.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for SeverableStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.sever.cut.load(Ordering::SeqCst) {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// 服务器看到的 pty 请求。
#[derive(Debug, Clone)]
struct PtyRecord {
    term: String,
    cols: u32,
    rows: u32,
    modes: Vec<(Pty, u32)>,
}

#[derive(Default)]
struct Record {
    events: Vec<String>,
    pty: Option<PtyRecord>,
}

struct Fake {
    spec: Spec,
    record: Arc<Mutex<Record>>,
    authenticated: Arc<AtomicBool>,
    sever: Arc<Sever>,
    line: Vec<u8>,
    size: (u32, u32),
}

impl Fake {
    fn log(&self, event: String) {
        self.record.lock().unwrap().events.push(event);
    }

    fn accepts(&self, key: &PublicKey) -> bool {
        self.spec.keys.iter().any(|k| k.key_data() == key.key_data())
    }

    /// 像 sshd 那样拒绝:剩余方法照旧是全部(russh 服务端默认会把刚失败的 password 方法摘掉,
    /// 真 sshd 不会 —— 不然就没法在终端里重输密码)。
    fn reject(&self) -> Auth {
        Auth::Reject {
            proceed_with_methods: Some(MethodSet::from(self.spec.methods.as_slice())),
            partial_success: false,
        }
    }

    /// 一行「命令」(回车结尾):模拟 shell 的几种行为。
    fn command(
        &mut self,
        channel: ChannelId,
        line: &str,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "exit" => {
                session.exit_status_request(channel, rest.parse().unwrap_or(0))?;
                session.eof(channel)?;
                session.close(channel)?;
            }
            "signal" => {
                session.exit_signal_request(channel, Sig::TERM, false, "terminated", "")?;
                session.eof(channel)?;
                session.close(channel)?;
            }
            "close" => {
                session.eof(channel)?;
                session.close(channel)?;
            }
            "stderr" => session.extended_data(channel, 1, rest.as_bytes().to_vec())?,
            "size" => session.data(
                channel,
                format!("size={}x{}\r\n", self.size.0, self.size.1).into_bytes(),
            )?,
            "drop" => self.sever.cut(),
            "flood" => {
                let lines: usize = rest.parse().unwrap_or(0);
                for index in 0..lines {
                    session.data(channel, format!("line {index:06}\r\n").into_bytes())?;
                }
                session.data(channel, b"flood-done\r\n".to_vec())?;
            }
            _ => session.data(channel, format!("echo:{line}\r\n").into_bytes())?,
        }
        Ok(())
    }
}

impl server::Handler for Fake {
    type Error = russh::Error;

    async fn authentication_banner(&mut self) -> Result<Option<String>, Self::Error> {
        Ok(self.spec.banner.map(str::to_string))
    }

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::reject())
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.log(format!("password {user}:{password}"));
        if self.spec.password == Some(password) {
            Ok(Auth::Accept)
        } else {
            Ok(self.reject())
        }
    }

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        if self.accepts(key) {
            Ok(Auth::Accept)
        } else {
            Ok(self.reject())
        }
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.log(format!("publickey {user} {}", fingerprint(key)));
        if self.accepts(key) {
            Ok(Auth::Accept)
        } else {
            Ok(self.reject())
        }
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let Some(kbd) = self.spec.kbd.clone() else {
            return Ok(self.reject());
        };
        let Some(response) = response else {
            let prompts: Vec<(Cow<'static, str>, bool)> = kbd
                .prompts
                .iter()
                .map(|(prompt, echo)| (Cow::Borrowed(*prompt), *echo))
                .collect();
            return Ok(Auth::Partial {
                name: Cow::Borrowed(kbd.name),
                instructions: Cow::Borrowed(kbd.instructions),
                prompts: Cow::Owned(prompts),
            });
        };
        let answers: Vec<String> = response
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect();
        self.log(format!("kbd {user}:{}", answers.join("|")));
        if answers == kbd.answers {
            Ok(Auth::Accept)
        } else {
            Ok(self.reject())
        }
    }

    async fn auth_succeeded(&mut self, _session: &mut Session) -> Result<(), Self::Error> {
        self.authenticated.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.record.lock().unwrap().pty = Some(PtyRecord {
            term: term.to_string(),
            cols: col_width,
            rows: row_height,
            modes: modes.to_vec(),
        });
        self.size = (col_width, row_height);
        if self.spec.refuse_pty {
            session.channel_failure(channel)?;
        } else {
            session.channel_success(channel)?;
        }
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.log(format!("env {variable_name}={variable_value}"));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.log("shell".into());
        if self.spec.refuse_shell {
            session.channel_failure(channel)?;
        } else {
            session.channel_success(channel)?;
            session.data(channel, b"ready\r\n".to_vec())?;
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.log(format!("exec {}", String::from_utf8_lossy(data)));
        session.channel_success(channel)?;
        session.data(channel, b"ready\r\n".to_vec())?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.size = (col_width, row_height);
        self.log(format!("window-change {col_width}x{row_height}"));
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        for &byte in data {
            if byte == b'\r' {
                let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
                self.command(channel, &line, session)?;
            } else {
                self.line.push(byte);
            }
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.log("eof".into());
        Ok(())
    }
}

struct FakeServer {
    addr: SocketAddr,
    record: Arc<Mutex<Record>>,
    ended: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    async fn start(spec: Spec) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = Arc::new(server::Config {
            keys: vec![ed25519(SERVER_KEY_SEED)],
            auth_rejection_time: Duration::from_millis(5),
            auth_rejection_time_initial: Some(Duration::ZERO),
            methods: MethodSet::from(spec.methods.as_slice()),
            inactivity_timeout: None,
            ..Default::default()
        });
        let record = Arc::new(Mutex::new(Record::default()));
        let ended = Arc::new(AtomicUsize::new(0));
        let task = {
            let (record, ended) = (record.clone(), ended.clone());
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let sever = Arc::new(Sever::default());
                    let authenticated = Arc::new(AtomicBool::new(false));
                    if let Some(grace) = spec.grace {
                        let (sever, authenticated) = (sever.clone(), authenticated.clone());
                        tokio::spawn(async move {
                            tokio::time::sleep(grace).await;
                            if !authenticated.load(Ordering::SeqCst) {
                                sever.cut();
                            }
                        });
                    }
                    let handler = Fake {
                        spec: spec.clone(),
                        record: record.clone(),
                        authenticated,
                        sever: sever.clone(),
                        line: Vec::new(),
                        size: (0, 0),
                    };
                    let stream = SeverableStream {
                        inner: socket,
                        sever,
                    };
                    let (config, ended) = (config.clone(), ended.clone());
                    tokio::spawn(async move {
                        if let Ok(session) = server::run_stream(config, stream, handler).await {
                            let _ = session.await;
                        }
                        ended.fetch_add(1, Ordering::SeqCst);
                    });
                }
            })
        };
        Self {
            addr,
            record,
            ended,
            task,
        }
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    fn events(&self) -> Vec<String> {
        self.record.lock().unwrap().events.clone()
    }

    fn count(&self, prefix: &str) -> usize {
        self.events()
            .iter()
            .filter(|event| event.starts_with(prefix))
            .count()
    }

    fn pty(&self) -> Option<PtyRecord> {
        self.record.lock().unwrap().pty.clone()
    }

    /// 等到至少 `n` 条连接在服务器一侧结束。
    async fn wait_ended(&self, n: usize) -> bool {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.ended.load(Ordering::SeqCst) >= n {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 夹在客户端与服务器之间的 TCP 转发。`freeze()` 之后两个方向都不再转发、连接也不断 ——
/// 模拟半开的死链(只有 keepalive 能发现)。
struct FreezingProxy {
    port: u16,
    frozen: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl FreezingProxy {
    async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let frozen = Arc::new(AtomicBool::new(false));
        let task = {
            let frozen = frozen.clone();
            tokio::spawn(async move {
                while let Ok((client, _)) = listener.accept().await {
                    let Ok(server) = TcpStream::connect(upstream).await else {
                        continue;
                    };
                    let (client_read, client_write) = client.into_split();
                    let (server_read, server_write) = server.into_split();
                    tokio::spawn(pipe(client_read, server_write, frozen.clone()));
                    tokio::spawn(pipe(server_read, client_write, frozen.clone()));
                }
            })
        };
        Self { port, frozen, task }
    }

    fn freeze(&self) {
        self.frozen.store(true, Ordering::SeqCst);
    }
}

impl Drop for FreezingProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn pipe(
    mut from: impl AsyncRead + Unpin,
    mut to: impl AsyncWrite + Unpin,
    frozen: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let read = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        while frozen.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if to.write_all(&buf[..read]).await.is_err() {
            break;
        }
    }
}

// ============================================================================
// 宿主一侧:回答提示、收输出
// ============================================================================

/// 宿主看到的提示。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Notice(String),
    Ask { prompt: String, echo: bool },
}

fn ask(prompt: &str, echo: bool) -> Seen {
    Seen::Ask {
        prompt: prompt.to_string(),
        echo,
    }
}

fn notice(text: &str) -> Seen {
    Seen::Notice(text.to_string())
}

/// 宿主对 `ReadLine` 的回答,按顺序取用;用完了就当取消。
enum Answer {
    Line(String),
    /// 想一会儿再答。
    Later(Duration, String),
    Cancel,
}

fn line(text: &str) -> Answer {
    Answer::Line(text.to_string())
}

/// 输出与退出的收集器(回调在输出线程上跑)。
#[derive(Clone, Default)]
struct Capture {
    output: Arc<Mutex<Vec<u8>>>,
    exits: Arc<Mutex<Vec<ShellExit>>>,
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    async fn wait_for(&self, needle: &str) -> bool {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.text().contains(needle) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn wait_exit(&self) -> Option<ShellExit> {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let Some(exit) = self.exits.lock().unwrap().first().cloned() {
                return Some(exit);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }

    fn exit_count(&self) -> usize {
        self.exits.lock().unwrap().len()
    }
}

struct Opened {
    result: Result<ShellSession, ShellOpenError>,
    seen: Vec<Seen>,
    capture: Capture,
}

impl Opened {
    fn session(&self) -> &ShellSession {
        match &self.result {
            Ok(session) => session,
            Err(error) => panic!("open_shell failed: {error:?}; prompts: {:?}", self.seen),
        }
    }

    fn error(&self) -> &ShellOpenError {
        match &self.result {
            Ok(_) => panic!("open_shell should have failed; prompts: {:?}", self.seen),
            Err(error) => error,
        }
    }
}

/// 照生产的用法开一个 shell:`open_shell` 放进 tokio 任务里跑(顺带证明它的 future 是 `Send`),
/// 宿主在另一个任务里并发地回答提示。
async fn open_with(
    dir: &TestDir,
    conn: SshConnection,
    config: ShellConfig,
    request: ShellRequest,
    answers: Vec<Answer>,
) -> Opened {
    let (prompts, mut incoming) = mpsc::unbounded_channel::<ShellPrompt>();
    let host = tokio::spawn(async move {
        let mut answers: VecDeque<Answer> = answers.into();
        let mut seen = Vec::new();
        while let Some(prompt) = incoming.recv().await {
            match prompt {
                ShellPrompt::Notice(text) => seen.push(Seen::Notice(text)),
                ShellPrompt::ReadLine {
                    prompt,
                    echo,
                    reply,
                } => {
                    seen.push(Seen::Ask { prompt, echo });
                    let answer = match answers.pop_front() {
                        Some(Answer::Line(text)) => Some(text),
                        Some(Answer::Later(delay, text)) => {
                            tokio::time::sleep(delay).await;
                            Some(text)
                        }
                        Some(Answer::Cancel) | None => None,
                    };
                    let _ = reply.send(answer);
                }
            }
        }
        seen
    });
    let capture = Capture::default();
    let env = dir.env();
    let (output, exits) = (capture.output.clone(), capture.exits.clone());
    let opening = tokio::spawn(async move {
        mt_ssh::open_shell(
            &conn,
            &env,
            &config,
            request,
            prompts,
            move |bytes: &[u8]| output.lock().unwrap().extend_from_slice(bytes),
            move |exit: ShellExit| exits.lock().unwrap().push(exit),
        )
        .await
    });
    let result = opening.await.unwrap();
    // open_shell 返回时提示通道的发送端随之丢弃,宿主那头随即结束。
    let seen = tokio::time::timeout(WAIT, host)
        .await
        .expect("the prompt channel must close when open_shell returns")
        .unwrap();
    Opened {
        result,
        seen,
        capture,
    }
}

async fn open(dir: &TestDir, conn: SshConnection, answers: Vec<Answer>) -> Opened {
    open_with(dir, conn, ShellConfig::default(), request(), answers).await
}

fn host_notice(port: u16) -> Seen {
    notice(&format!(
        "The authenticity of host '[127.0.0.1]:{port} ([127.0.0.1]:{port})' can't be \
         established.\r\nED25519 key fingerprint is {}.\r\n",
        server_fingerprint()
    ))
}

fn added_notice(port: u16) -> Seen {
    notice(&format!(
        "Warning: Permanently added '[127.0.0.1]:{port}' (ED25519) to the list of known \
         hosts.\r\n"
    ))
}

// ============================================================================
// 主机密钥
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 首见主机答_yes_后写入_known_hosts_第二次不再问() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("hostkey-yes");
    let port = server.port();
    let opened = open(&dir, with_password(port, "pw"), vec![line("yes")]).await;
    let _session = opened.session();
    assert_eq!(
        opened.seen,
        [
            host_notice(port),
            ask(HOST_KEY_QUESTION, true),
            added_notice(port)
        ]
    );
    let recorded = dir.known_hosts();
    assert!(
        recorded.starts_with(&format!("[127.0.0.1]:{port} ssh-ed25519 ")),
        "{recorded}"
    );
    assert!(opened.capture.wait_for("ready").await);

    let again = open(&dir, with_password(port, "pw"), Vec::new()).await;
    let _session = again.session();
    assert!(again.seen.is_empty(), "{:?}", again.seen);
    assert_eq!(dir.known_hosts(), recorded, "没有重复登记");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 首见主机照抄指纹也算同意() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("hostkey-fingerprint");
    let opened = open(
        &dir,
        with_password(server.port(), "pw"),
        vec![Answer::Line(format!("  {}  ", server_fingerprint()))],
    )
    .await;
    let _session = opened.session();
    assert!(dir.known_hosts().contains("ssh-ed25519 "));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 首见主机乱答会重问_yes_大小写不敏感() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("hostkey-again");
    let port = server.port();
    let opened = open(
        &dir,
        with_password(port, "pw"),
        vec![line("maybe"), line(""), line("YES")],
    )
    .await;
    let _session = opened.session();
    let again = "Please type 'yes', 'no' or the fingerprint: ";
    assert_eq!(
        opened.seen,
        [
            host_notice(port),
            ask(HOST_KEY_QUESTION, true),
            ask(again, true),
            ask(again, true),
            added_notice(port)
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 首见主机答_no_拒绝且不写_known_hosts() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("hostkey-no");
    let opened = open(&dir, with_password(server.port(), "pw"), vec![line("no")]).await;
    let error = opened.error();
    assert_eq!(error.kind, ShellOpenErrorKind::HostKey);
    assert_eq!(error.message, "Host key verification failed.");
    assert_eq!(dir.known_hosts(), "");
    assert_eq!(server.count("password"), 0, "没认证就断开");
    assert_eq!(opened.capture.exit_count(), 0, "失败时 on_exit 不调用");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 首见主机提问时取消() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("hostkey-cancel");
    let opened = open(
        &dir,
        with_password(server.port(), "pw"),
        vec![Answer::Cancel],
    )
    .await;
    assert_eq!(opened.error().kind, ShellOpenErrorKind::Cancelled);
    assert_eq!(dir.known_hosts(), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 主机密钥不符直接拒绝_不问() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("hostkey-mismatch");
    let stale = ed25519(99).public_key().to_openssh().unwrap();
    let recorded = format!("[127.0.0.1]:{} {stale}\n", server.port());
    std::fs::write(dir.known_hosts_path(), &recorded).unwrap();
    let opened = open(&dir, with_password(server.port(), "pw"), vec![line("yes")]).await;
    let error = opened.error();
    assert_eq!(error.kind, ShellOpenErrorKind::HostKey);
    assert!(
        error.message.contains("host key verification failed"),
        "{}",
        error.message
    );
    assert!(opened.seen.is_empty(), "不符时不问: {:?}", opened.seen);
    assert_eq!(dir.known_hosts(), recorded);
}

/// 没有提示器的路径(会话池 / 「测试连接」/ sidecar)照旧:首见主机静默 accept-new。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 无提示器的池路径首见主机仍静默_accept_new() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("pool-accept-new");
    let pool = SshPool::with_env(PoolConfig::default(), dir.env());
    let probe = pool.probe(&with_password(server.port(), "pw")).await;
    assert!(probe.is_ok(), "{probe:?}");
    assert!(
        dir.known_hosts()
            .starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", server.port())),
        "{}",
        dir.known_hosts()
    );
    pool.shutdown().await;
}

// ============================================================================
// 认证
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 存了密码就不问() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("stored-password");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let _session = opened.session();
    assert!(opened.seen.is_empty(), "{:?}", opened.seen);
    assert_eq!(server.count("password u:pw"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 密码错了提示重问_三次后失败() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("password-three");
    dir.trust(server.port());
    let opened = open(
        &dir,
        connection(server.port()),
        vec![line("wrong-1"), line("wrong-2"), line("wrong-3")],
    )
    .await;
    let error = opened.error();
    assert_eq!(error.kind, ShellOpenErrorKind::Auth, "{}", error.message);
    let prompt = "u@127.0.0.1's password: ";
    let denied = "Permission denied, please try again.\r\n";
    assert_eq!(
        opened.seen,
        [
            ask(prompt, false),
            notice(denied),
            ask(prompt, false),
            notice(denied),
            ask(prompt, false)
        ]
    );
    assert_eq!(server.count("password"), 3);
    assert!(
        error
            .message
            .contains("password: rejected by server (3 entered in the terminal)"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("wrong-"), "{}", error.message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 存的密码错了再问_答对即可() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("stored-then-ask");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "old"), vec![line("pw")]).await;
    let _session = opened.session();
    assert_eq!(opened.seen, [ask("u@127.0.0.1's password: ", false)]);
    assert_eq!(
        server.events()[..2],
        ["password u:old".to_string(), "password u:pw".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 密码提示时取消() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("password-cancel");
    dir.trust(server.port());
    let opened = open(&dir, connection(server.port()), vec![Answer::Cancel]).await;
    assert_eq!(opened.error().kind, ShellOpenErrorKind::Cancelled);
    assert_eq!(server.count("password"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyboard_interactive_多个问询照服务器的回显标志问() {
    let server = FakeServer::start(Spec {
        methods: vec![MethodKind::KeyboardInteractive],
        kbd: Some(Kbd {
            name: "Two-factor login",
            instructions: "Enter your password and code",
            prompts: vec![("Password: ", false), ("Verification code: ", true)],
            answers: vec!["pw", "123456"],
        }),
        ..Spec::password("pw")
    })
    .await;
    let dir = TestDir::new("kbd-multi");
    dir.trust(server.port());
    let opened = open(
        &dir,
        connection(server.port()),
        vec![line("pw"), line("123456")],
    )
    .await;
    let _session = opened.session();
    assert_eq!(
        opened.seen,
        [
            notice("Two-factor login\r\n"),
            notice("Enter your password and code\r\n"),
            ask("(u@127.0.0.1) Password: ", false),
            ask("(u@127.0.0.1) Verification code: ", true)
        ]
    );
    assert_eq!(server.count("kbd u:pw|123456"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyboard_interactive_单个不回显问询先静默用存的密码() {
    let server = FakeServer::start(Spec {
        methods: vec![MethodKind::KeyboardInteractive],
        kbd: Some(Kbd {
            name: "",
            instructions: "",
            prompts: vec![("Password: ", false)],
            answers: vec!["pw"],
        }),
        ..Spec::password("pw")
    })
    .await;
    let dir = TestDir::new("kbd-stored");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let _session = opened.session();
    assert!(opened.seen.is_empty(), "{:?}", opened.seen);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 认证_banner_净化控制字符后显示() {
    let server = FakeServer::start(Spec {
        banner: Some("Authorized use only\n\u{1b}]0;pwned\u{7}"),
        ..Spec::password("pw")
    })
    .await;
    let dir = TestDir::new("banner");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let _session = opened.session();
    assert_eq!(
        opened.seen,
        [notice("Authorized use only\r\n\\033]0;pwned\\007\r\n")]
    );
}

/// 带口令的连接私钥:口令错了直接重问(OpenSSH 同样不另外提示),对了就用它登录。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 带口令私钥在终端里问口令() {
    let locked = PrivateKey::from_openssh(ENCRYPTED_ED25519).unwrap();
    let server = FakeServer::start(Spec {
        methods: vec![MethodKind::PublicKey],
        keys: vec![locked.public_key().clone()],
        ..Spec::password("pw")
    })
    .await;
    let dir = TestDir::new("passphrase");
    dir.trust(server.port());
    let key_path = dir.root.join("id_encrypted");
    std::fs::write(&key_path, ENCRYPTED_ED25519).unwrap();
    let conn = SshConnection {
        identity_file: Some(key_path.display().to_string()),
        ..connection(server.port())
    };
    let opened = open(
        &dir,
        conn,
        vec![line("wrong"), line(ENCRYPTED_ED25519_PASSPHRASE)],
    )
    .await;
    let _session = opened.session();
    let prompt = format!("Enter passphrase for key '{}': ", key_path.display());
    assert_eq!(opened.seen, [ask(&prompt, false), ask(&prompt, false)]);
    assert_eq!(
        server.count(&format!("publickey u {}", fingerprint(locked.public_key()))),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 私钥口令留空就跳过这把_key_改问密码() {
    let locked = PrivateKey::from_openssh(ENCRYPTED_ED25519).unwrap();
    let server = FakeServer::start(Spec {
        methods: vec![MethodKind::PublicKey, MethodKind::Password],
        keys: vec![locked.public_key().clone()],
        ..Spec::password("pw")
    })
    .await;
    let dir = TestDir::new("passphrase-skip");
    dir.trust(server.port());
    let key_path = dir.root.join("id_encrypted");
    std::fs::write(&key_path, ENCRYPTED_ED25519).unwrap();
    let conn = SshConnection {
        identity_file: Some(key_path.display().to_string()),
        ..connection(server.port())
    };
    let opened = open(&dir, conn, vec![line(""), line("pw")]).await;
    let _session = opened.session();
    assert_eq!(
        opened.seen,
        [
            ask(
                &format!("Enter passphrase for key '{}': ", key_path.display()),
                false
            ),
            ask("u@127.0.0.1's password: ", false)
        ]
    );
    assert_eq!(server.count("publickey"), 0);
}

// ============================================================================
// 超时
// ============================================================================

/// 网络预算只有 800ms,用户在主机密钥与密码两个提问上各想了 1.2s:等用户的时间不计入,照常登录。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 等用户输入的时间不计入建连超时() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("timeout-excludes-user");
    let config = ShellConfig {
        connect_timeout: Duration::from_millis(800),
        ..ShellConfig::default()
    };
    let think = Duration::from_millis(1200);
    let opened = open_with(
        &dir,
        connection(server.port()),
        config,
        request(),
        vec![
            Answer::Later(think, "yes".into()),
            Answer::Later(think, "pw".into()),
        ],
    )
    .await;
    let _session = opened.session();
    assert!(opened.capture.wait_for("ready").await);
}

/// 用户在密码提示前想太久,服务器按 LoginGraceTime 掐断了连接:报网络错误并写明原委,不是泛泛的
/// 超时,也不算认证失败。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 等用户期间服务器因_login_grace_time_断开给出清楚的报错() {
    let server = FakeServer::start(Spec {
        grace: Some(Duration::from_millis(300)),
        ..Spec::password("pw")
    })
    .await;
    let dir = TestDir::new("login-grace");
    dir.trust(server.port());
    let opened = open(
        &dir,
        connection(server.port()),
        vec![Answer::Later(Duration::from_millis(1500), "pw".into())],
    )
    .await;
    let error = opened.error();
    assert_eq!(error.kind, ShellOpenErrorKind::Network, "{}", error.message);
    assert!(
        error.message.starts_with(&format!(
            "connection lost during authentication on u@127.0.0.1:{}",
            server.port()
        )),
        "{}",
        error.message
    );
    assert!(error.message.contains("LoginGraceTime"), "{}", error.message);
    assert_eq!(server.count("password"), 0, "密码没有送出去");
}

// ============================================================================
// shell 本身
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pty_请求带上_term_尺寸与终端模式() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("pty-request");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let session = opened.session();
    assert_eq!(
        session.target_label(),
        format!("u@127.0.0.1:{}", server.port())
    );
    let pty = server.pty().expect("pty-req");
    assert_eq!(pty.term, "xterm-256color");
    assert_eq!((pty.cols, pty.rows), (120, 40));
    assert!(pty.modes.contains(&(Pty::IUTF8, 1)), "{:?}", pty.modes);
    assert!(pty.modes.contains(&(Pty::VERASE, 127)), "{:?}", pty.modes);
    let events = server.events();
    assert!(events.contains(&"env LANG=en_US.UTF-8".to_string()), "{events:?}");
    assert!(events.contains(&"env COLORTERM=truecolor".to_string()), "{events:?}");
    assert!(events.contains(&"shell".to_string()), "{events:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_请求原样带上命令() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("exec");
    dir.trust(server.port());
    let command = "cd '/srv/my app' 2>/dev/null; exec $SHELL -l";
    let opened = open_with(
        &dir,
        with_password(server.port(), "pw"),
        ShellConfig::default(),
        ShellRequest {
            command: Some(command.into()),
            ..request()
        },
        Vec::new(),
    )
    .await;
    let _session = opened.session();
    assert!(opened.capture.wait_for("ready").await);
    assert_eq!(server.count(&format!("exec {command}")), 1);
    assert_eq!(server.count("shell"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 输入输出往返_stderr_也进终端() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("round-trip");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let session = opened.session();
    session.write(b"hel").unwrap();
    session.write(b"lo\r").unwrap();
    assert!(opened.capture.wait_for("echo:hello\r\n").await);
    session.write("stderr 中文 oops\r".as_bytes()).unwrap();
    assert!(opened.capture.wait_for("中文 oops").await);
    assert!(
        opened.capture.text().starts_with("ready\r\n"),
        "{}",
        opened.capture.text()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 改尺寸下发_window_change_同尺寸去重() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("resize");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let session = opened.session();
    assert_eq!(session.resize(120, 40), Ok(false), "与开 pty 时同尺寸");
    assert_eq!(session.resize(100, 30), Ok(true));
    assert_eq!(session.resize(100, 30), Ok(false));
    session.write(b"size\r").unwrap();
    assert!(opened.capture.wait_for("size=100x30").await);
    assert_eq!(server.count("window-change"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 退出码透传_之后写入报错() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("exit-status");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let session = opened.session();
    session.write(b"exit 7\r").unwrap();
    let exit = opened.capture.wait_exit().await.expect("on_exit");
    assert_eq!(
        exit,
        ShellExit {
            code: Some(7),
            signal: None,
            reason: ShellEnd::Exited
        }
    );
    assert!(server.wait_ended(1).await, "远端退出后连接随之断开");
    assert!(session.write(b"x").is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(opened.capture.exit_count(), 1, "恰好一次");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 信号退出透传() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("exit-signal");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    opened.session().write(b"signal\r").unwrap();
    let exit = opened.capture.wait_exit().await.expect("on_exit");
    assert_eq!(exit.signal.as_deref(), Some("TERM"));
    assert_eq!(exit.code, None);
    assert_eq!(exit.reason, ShellEnd::Exited);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 服务器只关_channel_不给退出码算_closed() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("channel-closed");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    opened.session().write(b"close\r").unwrap();
    let exit = opened.capture.wait_exit().await.expect("on_exit");
    assert_eq!(exit.reason, ShellEnd::Closed);
    assert_eq!((exit.code, exit.signal), (None, None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 连接被对端断开报_connection_lost() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("connection-lost");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    opened.session().write(b"drop\r").unwrap();
    let exit = opened.capture.wait_exit().await.expect("on_exit");
    match &exit.reason {
        ShellEnd::ConnectionLost(reason) => assert!(!reason.is_empty()),
        other => panic!("expected ConnectionLost, got {other:?}"),
    }
}

/// 半开的死链(两边都不再转发、连接也不断):keepalive 判死,报 OpenSSH 的原文。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keepalive_判死报_connection_lost() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let proxy = FreezingProxy::start(server.addr).await;
    let dir = TestDir::new("keepalive");
    dir.trust(proxy.port);
    let config = ShellConfig {
        keepalive_interval: Some(Duration::from_millis(200)),
        keepalive_max: 2,
        ..ShellConfig::default()
    };
    let opened = open_with(
        &dir,
        with_password(proxy.port, "pw"),
        config,
        request(),
        Vec::new(),
    )
    .await;
    let _session = opened.session();
    assert!(opened.capture.wait_for("ready").await);
    // 冻结前连接一直有 keepalive 回包,不会被误判。
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(opened.capture.exit_count(), 0, "活着的连接不判死");
    proxy.freeze();
    let exit = opened.capture.wait_exit().await.expect("on_exit");
    assert_eq!(
        exit.reason,
        ShellEnd::ConnectionLost("Timeout, server 127.0.0.1 not responding.".into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_之后不再回调_on_exit() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("close");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let session = opened.session();
    assert!(opened.capture.wait_for("ready").await);
    session.close();
    session.close();
    assert!(server.wait_ended(1).await, "关闭会断开连接");
    assert!(session.write(b"x").is_err());
    assert!(session.resize(10, 10).is_err());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(opened.capture.exit_count(), 0);
    assert!(server.events().contains(&"eof".to_string()), "先发了 EOF");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_与_close_同效() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("drop");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    assert!(opened.capture.wait_for("ready").await);
    let Opened {
        result, capture, ..
    } = opened;
    drop(result);
    assert!(server.wait_ended(1).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(capture.exit_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pty_被拒提示一行后照常开_shell() {
    let server = FakeServer::start(Spec {
        refuse_pty: true,
        ..Spec::password("pw")
    })
    .await;
    let dir = TestDir::new("pty-refused");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let _session = opened.session();
    assert_eq!(
        opened.seen,
        [notice("PTY allocation request failed on channel 0\r\n")]
    );
    assert!(opened.capture.wait_for("ready").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_被拒算_channel_错误() {
    let server = FakeServer::start(Spec {
        refuse_shell: true,
        ..Spec::password("pw")
    })
    .await;
    let dir = TestDir::new("shell-refused");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let error = opened.error();
    assert_eq!(error.kind, ShellOpenErrorKind::Channel, "{}", error.message);
    assert!(server.wait_ended(1).await, "失败时连接随之断开");
}

/// 大量输出(超过 2 MiB 窗口的一半,会触发窗口补充与背压)按序、不丢地交付;交付慢一点也一样。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 大量输出按序完整交付() {
    let server = FakeServer::start(Spec::password("pw")).await;
    let dir = TestDir::new("flood");
    dir.trust(server.port());
    let opened = open(&dir, with_password(server.port(), "pw"), Vec::new()).await;
    let session = opened.session();
    assert!(opened.capture.wait_for("ready").await);
    let lines = 150_000;
    session.write(format!("flood {lines}\r").as_bytes()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !opened.capture.text().contains("flood-done") {
        assert!(Instant::now() < deadline, "flood did not finish");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = opened.capture.text();
    let body = text
        .split_once("ready\r\n")
        .unwrap()
        .1
        .split_once("flood-done")
        .unwrap()
        .0;
    let expected: String = (0..lines).map(|i| format!("line {i:06}\r\n")).collect();
    assert!(body == expected, "output was reordered or lost");
}
