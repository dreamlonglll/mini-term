//! 终端里的内置 SSH(mt-ssh 的 [`mt_ssh::open_shell`])给 GPUI 壳用的那一层。
//!
//! mt-app 不直接依赖 tokio / mt-ssh(见 mt-app 的 Cargo.toml),它要的东西都从这里取:
//!
//! - **建连放到进程级共享运行时上**([`open_terminal_shell`]):调用当场 spawn,返回的
//!   [`OpeningShell`] 只是那条任务的句柄 —— GPUI 任务可以先去收提示、再回头等结果;
//!   drop 它 = 中止建连(用户 Ctrl+C、右键「断开 SSH」、关 pane)。
//! - **提示通道换成不含 tokio 类型的样子**([`TerminalPrompts`] / [`TerminalPrompt`] /
//!   [`PromptReply`]):tokio 的 mpsc / oneshot 与执行器无关,GPUI 任务里直接 await。
//!   `open_shell` 返回(成败都算,含被中止)时 mt-ssh 摘掉发送端,[`TerminalPrompts::recv`]
//!   随即返回 `None` —— 宿主据此知道「提示都送完了」,再去等建连结果。
//! - **shell 请求怎么拼**:远程项目 pane 用 [`remote_shell_request`](远端命令是
//!   `mt_pty::ssh::build_remote_login_command`,与系统 OpenSSH 那条路同一句),右键「SSH 连接」
//!   接管用 [`login_shell_request`](登录 shell)。TERM 是 `xterm-256color`,环境变量见
//!   [`terminal_env`]。
//!
//! 与系统 OpenSSH 那条路([`crate::prepare_remote_launch`])相比,这里**不找 ssh 客户端、不复制
//! 私钥临时副本、不解密码交给 autofill**:私钥由 mt-ssh 直接读,已存密码在认证那一刻解开,
//! 要问用户的(首见主机、密码、OTP、私钥口令)在终端里问。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use mt_config::SshConnection;
use mt_ssh::{ShellConfig, ShellPrompt, SshClientEnv};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use zeroize::Zeroize;

pub use mt_ssh::{
    ShellEnd, ShellExit, ShellOpenError, ShellOpenErrorKind, ShellRequest, ShellSession,
};

/// 终端 shell 的 TERM(与本地 PTY 注入的一致,见 `mt_pty::launch`)。
pub const TERMINAL_TERM: &str = "xterm-256color";

/// 建连期间的一条提示,见模块注释。
pub enum TerminalPrompt {
    /// 原样画进终端的文本(已拼好 `\r\n`、已净化)。
    Notice(String),
    /// 读一行:`prompt` 原样显示,`echo = false` 时不回显;读完经 `reply` 应答。
    ReadLine {
        prompt: String,
        echo: bool,
        reply: PromptReply,
    },
}

impl std::fmt::Debug for TerminalPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Notice(text) => f.debug_tuple("Notice").field(text).finish(),
            Self::ReadLine { prompt, echo, .. } => f
                .debug_struct("ReadLine")
                .field("prompt", prompt)
                .field("echo", echo)
                .finish_non_exhaustive(),
        }
    }
}

/// 读行提示的应答句柄。丢掉不答 = 取消(mt-ssh 按用户取消处理)。
pub struct PromptReply(oneshot::Sender<Option<String>>);

impl PromptReply {
    /// 应答:`Some(行)` = 用户提交了这一行,`None` = 取消。明文整块交出去(不留副本);
    /// SSH 层已经不收了的话,退回来的那份当场清零。
    pub fn answer(self, line: Option<String>) {
        if let Err(Some(mut line)) = self.0.send(line) {
            line.zeroize();
        }
    }
}

impl std::fmt::Debug for PromptReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PromptReply")
    }
}

/// 建连期间的提示流。
pub struct TerminalPrompts(mpsc::UnboundedReceiver<ShellPrompt>);

impl TerminalPrompts {
    /// 下一条提示;`None` = 建连已经有了结论(成败都算),不会再有提示了。
    pub async fn recv(&mut self) -> Option<TerminalPrompt> {
        self.0.recv().await.map(|prompt| match prompt {
            ShellPrompt::Notice(text) => TerminalPrompt::Notice(text),
            ShellPrompt::ReadLine {
                prompt,
                echo,
                reply,
            } => TerminalPrompt::ReadLine {
                prompt,
                echo,
                reply: PromptReply(reply),
            },
        })
    }
}

/// 在共享运行时上跑着的那次建连。await 拿结果;**drop 即中止**(建连途中的连接随之断开,
/// 已经建好但还没取走的会话随任务输出一起丢弃 = 关闭)。
pub struct OpeningShell {
    task: Result<JoinHandle<Result<ShellSession, ShellOpenError>>, Option<String>>,
}

impl Future for OpeningShell {
    type Output = Result<ShellSession, ShellOpenError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.task {
            Ok(handle) => Pin::new(handle).poll(cx).map(|joined| match joined {
                Ok(result) => result,
                Err(err) => Err(ShellOpenError {
                    kind: if err.is_cancelled() {
                        ShellOpenErrorKind::Cancelled
                    } else {
                        ShellOpenErrorKind::Channel
                    },
                    message: format!("the SSH terminal task failed: {err}"),
                }),
            }),
            // 共享运行时都建不起来(极端环境)
            Err(message) => Poll::Ready(Err(ShellOpenError {
                kind: ShellOpenErrorKind::Network,
                message: message.take().unwrap_or_default(),
            })),
        }
    }
}

impl Drop for OpeningShell {
    fn drop(&mut self) {
        if let Ok(handle) = &self.task {
            handle.abort();
        }
    }
}

/// 起一条终端 shell:**当场**在进程级共享运行时上开始建连,返回提示流与建连句柄。
///
/// `on_output` / `on_exit` 的口径见 [`mt_ssh::open_shell`]:建连成功后在 mt-ssh 自己的输出线程上
/// 按序调用,`on_exit` 恰好一次(宿主主动关的不算);建连失败时两者直接丢弃。
///
/// 用真实环境:`~/.ssh/known_hosts`、系统 ssh-agent(与会话池同一份,见
/// [`mt_ssh::SshClientEnv::system`]),超时等参数取 [`ShellConfig::default`]。
pub fn open_terminal_shell<F, E>(
    conn: SshConnection,
    request: ShellRequest,
    on_output: F,
    on_exit: E,
) -> (TerminalPrompts, OpeningShell)
where
    F: FnMut(&[u8]) + Send + 'static,
    E: FnOnce(ShellExit) + Send + 'static,
{
    open_terminal_shell_with(
        SshClientEnv::system(),
        ShellConfig::default(),
        conn,
        request,
        on_output,
        on_exit,
    )
}

/// [`open_terminal_shell`] 的可注入版本(测试用隔离的 known_hosts 与更短的超时)。
#[doc(hidden)]
pub fn open_terminal_shell_with<F, E>(
    env: SshClientEnv,
    config: ShellConfig,
    conn: SshConnection,
    request: ShellRequest,
    on_output: F,
    on_exit: E,
) -> (TerminalPrompts, OpeningShell)
where
    F: FnMut(&[u8]) + Send + 'static,
    E: FnOnce(ShellExit) + Send + 'static,
{
    let (prompts, receiver) = mpsc::unbounded_channel();
    // 运行时建不起来时闭包没跑,`prompts` 随之丢弃,提示流当场结束
    let task = crate::shared_runtime_handle()
        .map(|handle| {
            handle.spawn(async move {
                mt_ssh::open_shell(&conn, &env, &config, request, prompts, on_output, on_exit).await
            })
        })
        .map_err(Some);
    (TerminalPrompts(receiver), OpeningShell { task })
}

/// 远程项目 pane 的 shell 请求:`exec` 远端命令 `cd '<path>' 2>/dev/null; exec $SHELL -l`
/// (与系统 OpenSSH 那条路的远端命令同一句),尺寸 `(rows, cols)`。
pub fn remote_shell_request(remote_path: &str, rows: u16, cols: u16) -> ShellRequest {
    ShellRequest {
        term: TERMINAL_TERM.to_string(),
        cols,
        rows,
        command: Some(mt_pty::ssh::build_remote_login_command(remote_path)),
        env: terminal_env(),
    }
}

/// 右键「SSH 连接」接管用的 shell 请求:登录 shell(`shell` 请求,与 `ssh user@host` 同),
/// 尺寸 `(rows, cols)`。
pub fn login_shell_request(rows: u16, cols: u16) -> ShellRequest {
    ShellRequest {
        term: TERMINAL_TERM.to_string(),
        cols,
        rows,
        command: None,
        env: terminal_env(),
    }
}

/// 随 shell 请求发给服务器的环境变量(`env` 请求,服务器不收不算错):
///
/// - `COLORTERM=truecolor`:本终端支持 24 位色;
/// - `LANG`:**本机进程有才发**(macOS / Linux 桌面上一般有),与 OpenSSH 默认
///   `SendEnv LANG` 的口径一致。Windows 上进程里通常没有 `LANG` —— 那就不发,远端沿用它自己的
///   locale,与此前系统 OpenSSH(Windows 版默认不 `SendEnv`)的行为相同;不硬塞本地 PTY 注入的
///   `C.UTF-8`:远端 sshd 多半 `AcceptEnv LANG`,塞过去会盖掉服务器上配好的中文 locale,
///   老系统(CentOS 7)上还没有 `C.UTF-8`,登录就报 `setlocale` 警告。
pub fn terminal_env() -> Vec<(String, String)> {
    terminal_env_from(std::env::var("LANG").ok())
}

fn terminal_env_from(lang: Option<String>) -> Vec<(String, String)> {
    let mut env = vec![("COLORTERM".to_string(), "truecolor".to_string())];
    if let Some(lang) = lang.map(|l| l.trim().to_string()).filter(|l| !l.is_empty()) {
        env.push(("LANG".to_string(), lang));
    }
    env
}

/// 显示用的连接目标:`user@host:port`(IPv6 字面量加方括号,端口 0 按 22)。
pub fn terminal_target(conn: &SshConnection) -> String {
    let host = conn.host.trim();
    let port = if conn.port == 0 { 22 } else { conn.port };
    let endpoint = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let user = conn.user.trim();
    if user.is_empty() {
        endpoint
    } else {
        format!("{user}@{endpoint}")
    }
}

/// OpenSSH 收场那句 `Connection to <host> closed.` 里的主机名(配置里写的原样)。
pub fn terminal_host(conn: &SshConnection) -> String {
    conn.host.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(host: &str, port: u16, user: &str) -> SshConnection {
        SshConnection {
            id: "c".into(),
            name: "n".into(),
            host: host.into(),
            port,
            user: user.into(),
            password: None,
            identity_file: None,
            group: None,
            use_system_ssh: false,
            extra: Default::default(),
        }
    }

    #[test]
    fn 远程项目的请求_exec_远端登录命令() {
        let request = remote_shell_request("/srv/my app", 40, 120);
        assert_eq!(request.term, "xterm-256color");
        assert_eq!((request.rows, request.cols), (40, 120));
        assert_eq!(
            request.command.as_deref(),
            Some("cd '/srv/my app' 2>/dev/null; exec $SHELL -l"),
            "与系统 OpenSSH 那条路的远端命令一字不差"
        );
        let login = login_shell_request(24, 80);
        assert_eq!(login.command, None, "接管开的是登录 shell");
        assert_eq!((login.rows, login.cols), (24, 80));
    }

    #[test]
    fn 环境变量_本机有_lang_才发() {
        assert_eq!(
            terminal_env_from(None),
            vec![("COLORTERM".to_string(), "truecolor".to_string())]
        );
        assert_eq!(
            terminal_env_from(Some("  ".into())).len(),
            1,
            "空值等于没有"
        );
        assert_eq!(
            terminal_env_from(Some("zh_CN.UTF-8".into())),
            vec![
                ("COLORTERM".to_string(), "truecolor".to_string()),
                ("LANG".to_string(), "zh_CN.UTF-8".to_string()),
            ]
        );
    }

    #[test]
    fn 显示用的连接目标() {
        assert_eq!(
            terminal_target(&conn("10.0.0.5", 22, "root")),
            "root@10.0.0.5:22"
        );
        assert_eq!(
            terminal_target(&conn(" h ", 0, "u")),
            "u@h:22",
            "端口 0 按 22"
        );
        assert_eq!(terminal_target(&conn("::1", 2222, "u")), "u@[::1]:2222");
        assert_eq!(terminal_target(&conn("[::1]", 22, "u")), "u@[::1]:22");
        assert_eq!(terminal_target(&conn("h", 22, "")), "h:22");
        assert_eq!(terminal_host(&conn(" h.example ", 22, "u")), "h.example");
    }

    /// 读行应答:SSH 层还在收就把明文整块交过去;不收了也不 panic(退回来的那份清零)。
    #[tokio::test]
    async fn 应答交出明文_对端不收也不出错() {
        let (tx, rx) = oneshot::channel();
        PromptReply(tx).answer(Some("pw".into()));
        assert_eq!(rx.await.unwrap(), Some("pw".to_string()));

        let (tx, rx) = oneshot::channel::<Option<String>>();
        drop(rx);
        PromptReply(tx).answer(Some("pw".into()));
    }
}
