//! 终端内置 SSH 的冒烟测试:进程内假 sshd ↔ `mt_remote::open_terminal_shell_with`,宿主一侧照
//! mt-app 的 pane 胶水那样把提示交给 `mt_app_core::shell_link::ShellLink`(行编辑 / 预输入 /
//! 取消),按键也经它喂进去 —— 「pane 胶水 + 真 shell 通道」在不依赖 GPUI 的这一层跑通。
//!
//! 只连 127.0.0.1 上本测试自己起的服务器;known_hosts 一律是测试自己的临时文件
//! (`SshClientEnv::isolated`),绝不碰真实的 `~/.ssh` 与 ssh-agent。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mt_app_core::shell_link::{FeedAction, LinkPhase, PromptEvent, ShellLink, ShellPort};
use mt_config::SshConnection;
use mt_remote::{
    OpeningShell, PromptReply, ShellExit, ShellOpenErrorKind, ShellSession, TerminalPrompt,
    TerminalPrompts,
};
use mt_ssh::russh;
use mt_ssh::{ShellConfig, SshClientEnv};
use russh::keys::PrivateKey;
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet, Pty};
use tokio::net::TcpListener;

static UNIQUE: AtomicUsize = AtomicUsize::new(0);
const WAIT: Duration = Duration::from_secs(10);
const PASSWORD: &str = "s3cret-pw";

fn server_key() -> PrivateKey {
    PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
        &[201; 32],
    ))
}

// ─── 假 sshd:只认密码,开 pty + shell / exec,逐行回显,`exit N` 退出 ─────────────

#[derive(Default)]
struct Record {
    passwords: Vec<String>,
    lines: Vec<String>,
    pty: Option<(String, u32, u32)>,
    env: Vec<(String, String)>,
    exec: Option<String>,
    window_changes: Vec<(u32, u32)>,
}

struct Fake {
    record: Arc<Mutex<Record>>,
    line: Vec<u8>,
}

impl server::Handler for Fake {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::reject())
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.record
            .lock()
            .unwrap()
            .passwords
            .push(password.to_string());
        if password == PASSWORD {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
                partial_success: false,
            })
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.record.lock().unwrap().pty = Some((term.to_string(), col_width, row_height));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.record
            .lock()
            .unwrap()
            .env
            .push((variable_name.to_string(), variable_value.to_string()));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, b"motd\r\n".to_vec())?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.record.lock().unwrap().exec = Some(String::from_utf8_lossy(data).into_owned());
        session.channel_success(channel)?;
        session.data(channel, b"motd\r\n".to_vec())?;
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
        self.record
            .lock()
            .unwrap()
            .window_changes
            .push((col_width, row_height));
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        for &byte in data {
            if byte != b'\r' {
                self.line.push(byte);
                continue;
            }
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
            self.record.lock().unwrap().lines.push(line.clone());
            if let Some(code) = line.strip_prefix("exit ") {
                session.exit_status_request(channel, code.parse().unwrap_or(0))?;
                session.eof(channel)?;
                session.close(channel)?;
            } else {
                session.data(channel, format!("echo:{line}\r\n").into_bytes())?;
            }
        }
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
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = Arc::new(server::Config {
            keys: vec![server_key()],
            auth_rejection_time: Duration::from_millis(5),
            auth_rejection_time_initial: Some(Duration::ZERO),
            methods: MethodSet::from(&[MethodKind::Password][..]),
            inactivity_timeout: None,
            ..Default::default()
        });
        let record = Arc::new(Mutex::new(Record::default()));
        let ended = Arc::new(AtomicUsize::new(0));
        let task = {
            let (record, ended) = (record.clone(), ended.clone());
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let handler = Fake {
                        record: record.clone(),
                        line: Vec::new(),
                    };
                    let (config, ended) = (config.clone(), ended.clone());
                    tokio::spawn(async move {
                        if let Ok(session) = server::run_stream(config, socket, handler).await {
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

    fn record<T>(&self, read: impl FnOnce(&Record) -> T) -> T {
        read(&self.record.lock().unwrap())
    }

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

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mt-remote-terminal-{}-{}-{name}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn known_hosts(&self) -> PathBuf {
        self.0.join("known_hosts")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn connection(port: u16) -> SshConnection {
    SshConnection {
        id: "t".into(),
        name: "t".into(),
        host: "127.0.0.1".into(),
        port,
        user: "u".into(),
        password: None,
        identity_file: None,
        group: None,
        use_system_ssh: false,
        extra: Default::default(),
    }
}

fn config() -> ShellConfig {
    ShellConfig {
        connect_timeout: Duration::from_secs(10),
        keepalive_interval: None,
        ..ShellConfig::default()
    }
}

// ─── 宿主一侧:照 pane 胶水的口径驱动 ShellLink ─────────────────────────

/// 生产那边包着 `ShellSession` 的同一个形状(mt-app 的 `RemoteShell`)。
struct Port(ShellSession);

impl ShellPort for Port {
    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        self.0.write(bytes)
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<bool, String> {
        self.0.resize(cols, rows)
    }
}

/// 一个 pane:终端屏幕(按序记下画进去的字节)+ 链路 + 远端退出。
struct Host {
    screen: Arc<Mutex<Vec<u8>>>,
    exit: Arc<Mutex<Option<ShellExit>>>,
    link: ShellLink<Port, PromptReply>,
    prompts: TerminalPrompts,
    opening: Option<OpeningShell>,
}

impl Host {
    fn start(dir: &TestDir, conn: SshConnection, request: mt_remote::ShellRequest) -> Self {
        let screen = Arc::new(Mutex::new(Vec::new()));
        let exit = Arc::new(Mutex::new(None));
        let mut link = ShellLink::new(request.rows, request.cols);
        link.request_size();
        let (prompts, opening) = {
            let (screen, exit) = (screen.clone(), exit.clone());
            mt_remote::open_terminal_shell_with(
                SshClientEnv::isolated(dir.known_hosts()),
                config(),
                conn,
                request,
                move |bytes: &[u8]| screen.lock().unwrap().extend_from_slice(bytes),
                move |end| *exit.lock().unwrap() = Some(end),
            )
        };
        Self {
            screen,
            exit,
            link,
            prompts,
            opening: Some(opening),
        }
    }

    fn show(&self, bytes: &[u8]) {
        self.screen.lock().unwrap().extend_from_slice(bytes);
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned()
    }

    /// 用户敲一次键(或一次粘贴):照 pane 胶水 —— 回显画进屏幕、读完的一行应答给 SSH 层、
    /// 在跑时直接写进会话。
    fn keys(&mut self, bytes: &[u8]) -> FeedAction {
        let feed = self.link.feed(bytes);
        self.show(&feed.show);
        if let Some(answer) = feed.answer {
            let line = answer.line.map(|mut line| std::mem::take(&mut *line));
            answer.reply.answer(line);
        }
        if feed.action == FeedAction::Forward {
            self.link.write(bytes).unwrap();
        }
        feed.action
    }

    /// 收下一条提示画进屏幕;提示流结束(建连有了结论)返回 `false`。
    async fn next_prompt(&mut self) -> bool {
        let prompt = tokio::time::timeout(WAIT, self.prompts.recv())
            .await
            .expect("等提示超时");
        let Some(prompt) = prompt else {
            return false;
        };
        let event = match prompt {
            TerminalPrompt::Notice(text) => PromptEvent::Notice(text),
            TerminalPrompt::ReadLine {
                prompt,
                echo,
                reply,
            } => PromptEvent::ReadLine {
                prompt,
                echo,
                reply,
            },
        };
        let show = self.link.prompt(event);
        self.show(&show);
        true
    }

    /// 收提示直到出现一条读行。
    async fn until_prompting(&mut self) {
        while !self.link.is_prompting() {
            assert!(
                self.next_prompt().await,
                "提示流提前结束了:{}",
                self.screen()
            );
        }
    }

    /// 提示流收完、等建连结果,成功就照胶水的口径落成在跑(补 resize、冲刷预输入)。
    async fn finish_open(&mut self) -> Result<(), ShellOpenErrorKind> {
        while self.next_prompt().await {}
        let opening = self.opening.take().expect("只等一次");
        let result = tokio::time::timeout(WAIT, opening)
            .await
            .expect("等建连结果超时");
        match result {
            Ok(session) => {
                let Ok(opened) = self.link.opened(Port(session)) else {
                    panic!("连接中应当接住会话");
                };
                if !opened.typeahead.is_empty() {
                    self.link.write(&opened.typeahead).unwrap();
                }
                Ok(())
            }
            Err(error) => {
                self.link.end();
                Err(error.kind)
            }
        }
    }

    async fn wait_screen(&self, needle: &str) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.screen().contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("屏幕上等不到 {needle:?}:\n{}", self.screen());
    }

    async fn wait_exit(&self) -> ShellExit {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let Some(exit) = self.exit.lock().unwrap().clone() {
                return exit;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("等不到远端退出");
    }
}

/// 远程项目 pane 的完整一程:首见主机答 yes(回显)、密码在终端里输(不回显)、exec 远端登录命令、
/// 输入输出往返、远端退出码透传。
#[tokio::test]
async fn 远程项目_首见主机与密码在终端里答_之后照常收发() {
    let server = FakeServer::start().await;
    let dir = TestDir::new("remote");
    let request = mt_remote::remote_shell_request("/srv/app", 30, 100);
    let mut host = Host::start(&dir, connection(server.addr.port()), request);

    host.until_prompting().await;
    assert!(
        host.screen().contains("can't be established"),
        "首见主机先给两行说明:\n{}",
        host.screen()
    );
    for key in ["y", "e", "s"] {
        assert_eq!(host.keys(key.as_bytes()), FeedAction::Consumed);
    }
    assert_eq!(host.keys(b"\r"), FeedAction::Consumed);
    assert!(host.screen().ends_with("yes\r\n"), "主机密钥的回答照常回显");

    host.until_prompting().await;
    assert!(
        host.screen().ends_with("u@127.0.0.1's password: "),
        "{}",
        host.screen()
    );
    for key in PASSWORD.bytes() {
        host.keys(&[key]);
    }
    host.keys(b"\r");
    host.finish_open().await.expect("建连应当成功");
    assert!(
        !host.screen().contains(PASSWORD),
        "密码一个字都不能回显:\n{}",
        host.screen()
    );
    assert!(
        std::fs::read_to_string(dir.known_hosts())
            .unwrap_or_default()
            .contains("ssh-ed25519"),
        "答了 yes 就记进 known_hosts"
    );

    host.wait_screen("motd\r\n").await;
    assert_eq!(host.link.phase(), LinkPhase::Running);
    assert_eq!(host.keys(b"ls -la\r"), FeedAction::Forward);
    host.wait_screen("echo:ls -la\r\n").await;

    server.record(|record| {
        assert_eq!(record.passwords, vec![PASSWORD.to_string()]);
        assert_eq!(
            record.pty,
            Some(("xterm-256color".to_string(), 100, 30)),
            "TERM 与开 pty 时的尺寸"
        );
        assert_eq!(
            record.exec.as_deref(),
            Some("cd '/srv/app' 2>/dev/null; exec $SHELL -l")
        );
        assert!(
            record
                .env
                .contains(&("COLORTERM".to_string(), "truecolor".to_string())),
            "{:?}",
            record.env
        );
    });

    host.keys(b"exit 3\r");
    let exit = host.wait_exit().await;
    assert_eq!(exit.code, Some(3));
}

/// 预输入:读口令提示出现之前敲的字清掉(不会被当成口令、也不进远端),答完口令之后、shell
/// 就绪之前敲的就绪后冲刷;连接中改过尺寸,就绪时补一次 window-change。
#[tokio::test]
async fn 预输入在读口令时清掉_答完之后的就绪后冲刷() {
    let server = FakeServer::start().await;
    let dir = TestDir::new("typeahead");
    let mut host = Host::start(
        &dir,
        connection(server.addr.port()),
        mt_remote::login_shell_request(24, 80),
    );

    // 建连途中抢先敲的一行
    assert_eq!(host.keys(b"rm -rf /tmp/x\r"), FeedAction::Consumed);
    host.until_prompting().await;
    host.keys(b"yes\r");
    host.until_prompting().await;
    host.keys(format!("{PASSWORD}\r").as_bytes());
    // 口令已答、shell 还没开:这之后敲的才算预输入;顺手改一次尺寸
    host.keys(b"whoami\r");
    host.link.resize(40, 132).unwrap();
    host.finish_open().await.expect("建连应当成功");

    host.wait_screen("echo:whoami\r\n").await;
    server.record(|record| {
        assert_eq!(
            record.passwords,
            vec![PASSWORD.to_string()],
            "抢先敲的那一行不能被当成口令"
        );
        assert_eq!(record.lines, vec!["whoami".to_string()]);
        assert!(record.exec.is_none(), "接管开的是登录 shell");
        assert_eq!(record.pty.as_ref().map(|p| (p.1, p.2)), Some((80, 24)));
    });
    let deadline = Instant::now() + WAIT;
    while server.record(|r| r.window_changes.is_empty()) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(server.record(|r| r.window_changes.clone()), vec![(132, 40)]);
}

/// 读口令时按 Ctrl+C:这一行取消,SSH 层报「已取消」,连接随之断开。
#[tokio::test]
async fn 读口令时_ctrl_c_取消建连() {
    let server = FakeServer::start().await;
    let dir = TestDir::new("cancel");
    let mut host = Host::start(
        &dir,
        connection(server.addr.port()),
        mt_remote::login_shell_request(24, 80),
    );
    host.until_prompting().await;
    host.keys(b"yes\r");
    host.until_prompting().await;
    host.keys(b"hal");
    assert_eq!(host.keys(b"\x03"), FeedAction::Consumed);
    assert!(host.screen().ends_with("^C\r\n"));
    assert_eq!(host.finish_open().await, Err(ShellOpenErrorKind::Cancelled));
    assert!(server.wait_ended(1).await, "取消之后连接要断开");
    assert!(
        server.record(|r| r.passwords.is_empty()),
        "取消的那一行不交出去"
    );
}

/// 没在读行时按 Ctrl+C:宿主丢掉建连句柄(= 中止),提示流随之结束,服务器那头的连接断开。
#[tokio::test]
async fn 丢掉建连句柄即中止() {
    let server = FakeServer::start().await;
    let dir = TestDir::new("abort");
    let mut host = Host::start(
        &dir,
        connection(server.addr.port()),
        mt_remote::login_shell_request(24, 80),
    );
    host.until_prompting().await;
    host.keys(b"yes\r");
    // 主机密钥答完、密码提示之前:这时的 Ctrl+C 是「取消建连」
    assert_eq!(host.keys(b"\x03"), FeedAction::Cancel);
    assert_eq!(host.link.phase(), LinkPhase::Ended);
    drop(host.opening.take());
    let ended =
        tokio::time::timeout(WAIT, async { while host.prompts.recv().await.is_some() {} }).await;
    assert!(ended.is_ok(), "中止之后提示流要结束");
    assert!(server.wait_ended(1).await, "中止之后连接要断开");
}
