//! mt-agent-cli —— 编排者的控制 CLI。
//!
//! 「编排者」是被显式授予编排能力的 AI 会话（ADR 0003）：用户在 AI 启动器上勾了
//! 「允许编排」，用它起的 pane 在 spawn 时被注入一枚编排令牌与自身 pane 身份，
//! pane 里的 agent 就靠这个二进制去驱动别的 AI 会话。
//!
//! # fail-closed 的三道闸
//!
//! 1. **没有令牌就不发请求**：`MINITERM_ORCHESTRATOR_TOKEN` /
//!    `MINITERM_ORCHESTRATOR_PANE` 任缺其一即报错退出（退出码 2）。普通 pane 里
//!    跑到这个命令是常态，明确拒绝就是正确行为。
//! 2. **服务端复核**：令牌与自称的 pane 身份对不上一样被拒 —— 身份钉死在环境里，
//!    不靠 CLI 自觉。
//! 3. **认不出的响应算失败**，不退化成「空名单」。
//!
//! # 端口发现
//!
//! 与 `miniterm-hook` 同一套：`MINITERM_HOOK_PORT` 优先（主程序 spawn 时注入，
//! dev 实例也走这条），否则读数据目录里的 `hook-server.json`。控制端点与 hook
//! 上报**共用**那个本地 HTTP 服务，只是路由前缀不同。
//!
//! # 用法
//!
//! ```text
//! mt-agent-cli list-launchers                          # 我能用哪些 AI 启动器起乐手
//! mt-agent-cli list-projects                           # 我能在哪些项目里起(本项目 + 同分组)
//! mt-agent-cli start-session --launcher <ID> [--project <ID>]   # 起一个受编排会话
//! mt-agent-cli list-panes                              # 我起过的受编排会话及其状态
//! mt-agent-cli send --pane <ID> --text <TEXT>          # 给某个受编排会话派活
//! mt-agent-cli send --pane <ID> --stdin                # 同上,正文从 stdin 读(多行)
//! mt-agent-cli wait --pane <ID> [--timeout <SECONDS>]  # 等它干完/等人/退出(长轮询)
//! ```
//!
//! 成功：JSON 到 stdout，退出码 0。失败：JSON 到 stderr，退出码见 [`CliError::exit_code`]。
//!
//! ⚠️ `wait` 是**唯一会阻塞好几分钟**的命令，它的读超时按请求的耐心放大
//! （[`mt_agent_control::wait_read_timeout`]）；其余命令一律用常规的 5 秒。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use mt_agent_control::{
    ControlFailure, ControlRequest, IdentityError, identity_from_env, parse_launchers, parse_panes,
    parse_projects, parse_send_receipt, parse_started_pane, parse_wait_outcome, wait_read_timeout,
    CONNECT_TIMEOUT, CONTROL_PREFIX, READ_TIMEOUT, WAIT_DEFAULT,
};

/// 退出码说明。**给编排者（LLM）读的**，所以 `--help` 里就得写清 `desktopBusy`
/// 那一条不是「重试就好」—— 那正是它最容易做错的判断。
const EXIT_CODE_HELP: &str = "\
Exit codes:
  0  success (JSON on stdout)
  2  this pane has no orchestrator capability, or the request was rejected as unauthorized
  3  the desktop could not be reached, or did not answer in time (desktopBusy).
     NOTE: desktopBusy does NOT mean the session failed to start - the desktop records
     an orchestrated session the moment it lands, before it answers. Run `list-panes`
     to check whether it is already there instead of retrying blindly.
  4  the desktop rejected the request (bad launcher/project, session limit, ...);
     change the request or wait for one of your sessions to finish

send notes:
  A prompt is delivered immediately, never queued, and is equivalent to typing it in
  that session and pressing Enter once. Multi-line prompts go in as a single
  bracketed-paste block so newlines inside them do not submit early - but the receipt
  reports bracketedPaste: false when the target terminal was not in paste mode (its
  agent has probably exited), which means the lines went in one by one. An empty
  prompt is refused: a bare Enter would answer a pending prompt on the user's behalf,
  and an orchestrator must never do that - ask the user to handle it instead.

wait notes:
  wait blocks until the session settles, then prints one of four outcomes - ALWAYS
  read `outcome`, the exit code is 0 for all of them:
    ai-idle    the turn finished. Read `cause` to learn HOW: only Stop means the work
               really completed; Interrupt means the user pressed Esc, and Stall means
               the session went silent and was settled by a fallback. Neither of those
               two is a delivered result.
    attention  it is waiting for approval or asking a question; `cause` says which
               (PermissionRequest / Elicitation / StopFailure).
               DO NOT answer for the user and DO NOT send that session anything.
               Tell the user in your own conversation and let them handle it there;
               its status badge is already yellow. Then wait again.
    idle       the agent inside that session has exited; the pane is back to a shell.
    pending    it did not settle before the timeout. This is NOT an error. status
               ai-working means it is genuinely busy - wait again. status idle means
               that session is opaque to us (no hooks, and its command is not a
               recognized AI command), so wait will never settle on it: use read or
               ask the user.
  --timeout defaults to 60s and is capped at 300s server-side; the command blocks for
  that long, so keep it under your own tool-call timeout. Calling wait immediately
  after send can return the PREVIOUS turn's ai-idle (the agent has not reacted yet) -
  a waitedMs near 0 is the tell; wait again to confirm.";

#[derive(Parser)]
#[command(
    name = "mt-agent-cli",
    about = "mini-term orchestrator control CLI (requires an orchestration-enabled pane)",
    after_help = EXIT_CODE_HELP,
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the AI launchers this orchestrator may start orchestrated sessions with.
    ListLaunchers,
    /// List the projects reachable from here (own project + same group).
    ListProjects,
    /// Start an orchestrated session with the given launcher.
    StartSession {
        /// Launcher id, as reported by `list-launchers`.
        #[arg(long, value_name = "ID")]
        launcher: String,
        /// Project id, as reported by `list-projects`; defaults to this pane's own project.
        #[arg(long, value_name = "ID")]
        project: Option<String>,
    },
    /// List the orchestrated sessions this orchestrator started, with their status.
    ListPanes,
    /// Send a prompt to one of your orchestrated sessions, as if typed and entered there.
    ///
    /// Multi-line text is delivered as one bracketed-paste block, so newlines inside it
    /// do not submit early; a single Enter is appended at the end.
    #[command(group = clap::ArgGroup::new("body").required(true).args(["text", "stdin"]))]
    Send {
        /// Pane id of the orchestrated session, as reported by `start-session` / `list-panes`.
        #[arg(long, value_name = "ID")]
        pane: u32,
        /// The prompt to send. Use --stdin instead for anything with newlines or quotes.
        #[arg(long, value_name = "TEXT")]
        text: Option<String>,
        /// Read the prompt from stdin (the whole stream). Preferred for multi-line prompts.
        #[arg(long)]
        stdin: bool,
    },
    /// Wait until one of your orchestrated sessions settles: finished, waiting for a
    /// human, or exited.
    ///
    /// Blocks (long poll). Always read `outcome` from the JSON - every outcome, the
    /// timeout included, exits 0. On `attention` the session is waiting for the USER:
    /// report it and let them handle it; never answer on their behalf.
    Wait {
        /// Pane id of the orchestrated session, as reported by `start-session` / `list-panes`.
        #[arg(long, value_name = "ID")]
        pane: u32,
        /// How long to wait, in seconds (default 60, capped at 300 server-side).
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
    },
}

impl Command {
    /// 路由里那一段（与桌面侧的命令名一字不差）。
    fn endpoint(&self) -> &'static str {
        match self {
            Self::ListLaunchers => "list-launchers",
            Self::ListProjects => "list-projects",
            Self::StartSession { .. } => "start-session",
            Self::ListPanes => "list-panes",
            Self::Send { .. } => "send",
            Self::Wait { .. } => "wait",
        }
    }

    /// 这条命令要给读响应留多久。
    ///
    /// 除 `wait` 之外都是一趟请求答一趟，[`READ_TIMEOUT`]（5 秒）足够；
    /// **`wait` 会在服务端睡到几分钟**，照 5 秒读的话长轮询每次都变成 CLI 先
    /// 断线 —— 编排者拿到的会是「够不着」，而不是它等的那个终态。
    ///
    /// 放大的口径住在 `mt_agent_control`（[`wait_read_timeout`]），因为它得与
    /// 服务端那个上界对得上，而那条不等式跨着工作区边界、由主仓的对账测试钉住。
    fn read_timeout(&self) -> Duration {
        match self {
            Self::Wait { timeout, .. } => {
                wait_read_timeout(timeout.map_or(WAIT_DEFAULT, Duration::from_secs))
            }
            _ => READ_TIMEOUT,
        }
    }

    /// 这条命令的请求体。**只带 id** —— 启动器的命令文本从不经过 CLI
    /// （ADR 0002：命令只能来自桌面端配置）。
    ///
    /// `send` 的正文是唯一的例外：那是编排者自己写的 prompt，本来就得经这里
    /// 传过去。它**只出现在请求体里** —— 不打印、不进错误消息。
    fn request(&self, identity: &mt_agent_control::Identity) -> Result<ControlRequest, CliError> {
        Ok(match self {
            Self::StartSession { launcher, project } => {
                ControlRequest::start_session(identity, launcher, project.as_deref())
            }
            Self::Send { pane, text, stdin } => {
                let body = read_body(text.as_deref(), *stdin)?;
                ControlRequest::send(identity, *pane, &body)
            }
            // 不给 `--timeout` 就整个字段不出线：默认耐心只住在桌面侧那个常量上
            Self::Wait { pane, timeout } => {
                ControlRequest::wait(identity, *pane, timeout.map(Duration::from_secs))
            }
            _ => ControlRequest::from(identity),
        })
    }
}

/// `send` 的正文：`--text` 直给，或 `--stdin` 整条读进来。
///
/// clap 的 `ArgGroup` 已经保证两者恰好给一个，所以这里没有「都没给」的分支 ——
/// 无参数时**绝不**去读 stdin：这个二进制常在没有管道的 pane 里被跑到，
/// 那样会挂住等一个永远不来的 EOF。
fn read_body(text: Option<&str>, stdin: bool) -> Result<String, CliError> {
    if let Some(text) = text {
        return Ok(text.to_string());
    }
    debug_assert!(stdin, "clap 的 ArgGroup 保证两者恰好给一个");
    let mut body = String::new();
    std::io::stdin()
        .read_to_string(&mut body)
        // ⚠️ 报错里只说读失败，**不带出已经读到的那部分正文**
        .map_err(|e| CliError::DesktopUnreachable(format!("cannot read prompt from stdin: {e}")))?;
    Ok(body)
}

fn main() {
    let cli = Cli::parse();
    match run(&cli.command) {
        Ok(json) => {
            println!("{json}");
        }
        Err(err) => {
            eprintln!("{}", err.to_json());
            std::process::exit(err.exit_code());
        }
    }
}

fn run(command: &Command) -> Result<String, CliError> {
    let identity = identity_from_env().map_err(CliError::Identity)?;
    let port = discover_port().ok_or(CliError::DesktopUnreachable(
        "cannot locate the mini-term hook server port (is mini-term running?)".to_string(),
    ))?;
    let body = serde_json::to_string(&command.request(&identity)?)
        .map_err(|e| CliError::DesktopUnreachable(format!("cannot encode request: {e}")))?;
    let (status, response) = post(port, command.endpoint(), &body, command.read_timeout())?;

    match command {
        Command::ListLaunchers => {
            let launchers = parse_launchers(status, &response).map_err(CliError::Rejected)?;
            to_json(&serde_json::json!({ "launchers": launchers }))
        }
        Command::ListProjects => {
            let projects = parse_projects(status, &response).map_err(CliError::Rejected)?;
            to_json(&serde_json::json!({ "projects": projects }))
        }
        Command::StartSession { .. } => {
            let pane = parse_started_pane(status, &response).map_err(CliError::Rejected)?;
            to_json(&serde_json::json!({ "pane": pane }))
        }
        Command::ListPanes => {
            let panes = parse_panes(status, &response).map_err(CliError::Rejected)?;
            to_json(&serde_json::json!({ "panes": panes }))
        }
        Command::Send { .. } => {
            let sent = parse_send_receipt(status, &response).map_err(CliError::Rejected)?;
            to_json(&serde_json::json!({ "sent": sent }))
        }
        // **超时也走成功这一路**：`pending` 是一条正常的观测结果，不是错误
        // （做成错误就得给它一个「你或我们出了问题」的退出码档位，两样都不是）。
        Command::Wait { .. } => {
            let waited = parse_wait_outcome(status, &response).map_err(CliError::Rejected)?;
            to_json(&serde_json::json!({ "waited": waited }))
        }
    }
}

fn to_json(value: &serde_json::Value) -> Result<String, CliError> {
    serde_json::to_string_pretty(value)
        .map_err(|e| CliError::DesktopUnreachable(format!("cannot encode output: {e}")))
}

// ─── 传输 ─────────────────────────────────────────────────────

/// hook 服务端口：环境变量优先，其次数据目录里的端口文件。
///
/// `MT_APP_DATA_DIR` 也认一下 —— 开发实例用它隔离数据目录，
/// 而端口文件那条平台默认路径（`mt_core::config_json_path` 的同目录）
/// 指的是装机版那一份。
fn discover_port() -> Option<u16> {
    if let Ok(v) = std::env::var("MINITERM_HOOK_PORT") {
        if let Ok(port) = v.trim().parse::<u16>() {
            return Some(port);
        }
    }
    let path = port_file_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("port")?.as_u64().map(|p| p as u16)
}

fn port_file_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("MT_APP_DATA_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir).join("hook-server.json"));
        }
    }
    // 数据目录的锚点走 mt-core 那份定位器(三个 sidecar 已经在用同一份)，
    // 只把文件名换掉。
    Some(mt_core::config_json_path()?.with_file_name("hook-server.json"))
}

/// 裸 HTTP POST，返回 (状态码, body)。不引 HTTP 客户端依赖（与 miniterm-hook 同款），
/// 区别是这条**要读响应**。
///
/// `read_timeout` 按命令来（[`Command::read_timeout`]）：`wait` 要等的是一个 AI
/// 回合，别的都是一趟请求答一趟。**写超时不跟着放大** —— 请求体早就发完了，
/// 长轮询等的是响应；写这一侧慢到 5 秒本来就是出事了。
fn post(
    port: u16,
    endpoint: &str,
    body: &str,
    read_timeout: Duration,
) -> Result<(u16, String), CliError> {
    let addr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| CliError::DesktopUnreachable(format!("bad address: {e}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| {
        CliError::DesktopUnreachable(format!("cannot reach mini-term on 127.0.0.1:{port}: {e}"))
    })?;
    stream
        .set_read_timeout(Some(read_timeout))
        .and_then(|()| stream.set_write_timeout(Some(READ_TIMEOUT)))
        .map_err(|e| CliError::DesktopUnreachable(format!("socket setup failed: {e}")))?;

    let request = format!(
        "POST {CONTROL_PREFIX}{endpoint} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| CliError::DesktopUnreachable(format!("send failed: {e}")))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| CliError::DesktopUnreachable(format!("read failed: {e}")))?;
    let raw = String::from_utf8_lossy(&raw).to_string();
    split_response(&raw)
        .ok_or_else(|| CliError::DesktopUnreachable("malformed HTTP response".to_string()))
}

/// 拆 HTTP 响应：状态行的状态码 + 空行之后的 body。
fn split_response(raw: &str) -> Option<(u16, String)> {
    let (head, body) = raw.split_once("\r\n\r\n")?;
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse::<u16>()
        .ok()?;
    Some((status, body.to_string()))
}

// ─── 错误语义 ─────────────────────────────────────────────────

/// ⚠️ `Debug` 是给测试用的。**三个变体一个都不许装 `send` 的正文** ——
/// 那是用户项目里的内容，`Debug` 会经 panic 消息 / `unwrap` 落到 stderr。
/// 读 stdin 失败那一档只带 io 错误本身，不带已经读到的半截。
#[derive(Debug)]
enum CliError {
    /// 这个 pane 压根没有编排能力（或注入链路坏了）。
    Identity(IdentityError),
    /// 桌面端够不着（没跑 / 端口文件没了 / 响应认不出）。
    DesktopUnreachable(String),
    /// 桌面端明确拒绝了这次请求。
    Rejected(ControlFailure),
}

impl CliError {
    /// 退出码是给编排者（LLM）看的第一手信号，语义分三档：
    /// 2 = 你没有这个能力；3 = 桌面端够不着 / 没答上来；4 = 请求被拒。
    ///
    /// 「够不着」那一档刻意把 `desktopBusy`（桌面端在，但主线程没在时限内答复）
    /// 也算进去：这两种处境下**改请求都没用**，而 4 那一档是「改你的请求或等名额」。
    ///
    /// ⚠️ 但 3 不等于「重试就好」：`desktopBusy` 时那个受编排会话很可能已经起来了
    /// （桌面端先记账再答复），该做的是 `list-panes` 查一眼。这一条写进了
    /// [`EXIT_CODE_HELP`]，`--help` 里编排者看得到。
    fn exit_code(&self) -> i32 {
        match self {
            Self::Identity(_) => 2,
            Self::DesktopUnreachable(_) => 3,
            Self::Rejected(f) if f.is_denied() => 2,
            Self::Rejected(f) if f.is_desktop_unavailable() => 3,
            Self::Rejected(_) => 4,
        }
    }

    fn to_json(&self) -> String {
        let (code, message) = match self {
            Self::Identity(IdentityError::NotAnOrchestrator) => (
                "notAnOrchestrator",
                IdentityError::NotAnOrchestrator.message().to_string(),
            ),
            Self::Identity(IdentityError::BrokenIdentity) => (
                "brokenIdentity",
                IdentityError::BrokenIdentity.message().to_string(),
            ),
            Self::DesktopUnreachable(why) => ("desktopUnreachable", why.clone()),
            Self::Rejected(f) => (f.code.as_str(), f.message.clone()),
        };
        serde_json::json!({ "error": { "code": code, "message": message } }).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拆响应取状态码与_body() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(split_response(raw), Some((200, "{}".to_string())));
        assert_eq!(split_response("garbage"), None);
    }

    /// 退出码是编排者的第一手信号，三档不能混。
    #[test]
    fn 退出码按语义分档() {
        assert_eq!(
            CliError::Identity(IdentityError::NotAnOrchestrator).exit_code(),
            2
        );
        assert_eq!(CliError::DesktopUnreachable("x".into()).exit_code(), 3);
        assert_eq!(
            CliError::Rejected(ControlFailure {
                status: 401,
                code: "invalidToken".into(),
                message: "m".into()
            })
            .exit_code(),
            2,
            "鉴权被拒与「没有能力」同档"
        );
        assert_eq!(
            CliError::Rejected(ControlFailure {
                status: 409,
                code: "projectUnavailable".into(),
                message: "m".into()
            })
            .exit_code(),
            4
        );
    }

    #[test]
    fn 错误输出是可解析的_json() {
        let err = CliError::Rejected(ControlFailure {
            status: 404,
            code: "unknownCommand".into(),
            message: "nope".into(),
        });
        let v: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();
        assert_eq!(v["error"]["code"], "unknownCommand");
        assert_eq!(v["error"]["message"], "nope");
    }

    /// 端点名与桌面侧路由必须一字不差。
    #[test]
    fn 端点名固定() {
        assert_eq!(Command::ListLaunchers.endpoint(), "list-launchers");
        assert_eq!(Command::ListProjects.endpoint(), "list-projects");
        assert_eq!(
            Command::StartSession {
                launcher: "x".into(),
                project: None
            }
            .endpoint(),
            "start-session"
        );
        assert_eq!(Command::ListPanes.endpoint(), "list-panes");
        assert_eq!(
            Command::Send {
                pane: 1,
                text: Some("x".into()),
                stdin: false
            }
            .endpoint(),
            "send"
        );
        assert_eq!(
            Command::Wait {
                pane: 1,
                timeout: None
            }
            .endpoint(),
            "wait"
        );
        assert_eq!(CONTROL_PREFIX, "/control/");
    }

    /// **只有 `wait` 放大读超时**，其余命令一律常规 5 秒。
    ///
    /// 反过来（`wait` 也用 5 秒）时长轮询每次都是 CLI 先断线，编排者拿到的会是
    /// 「够不着」而不是它等的那个终态 —— 这条命令因此整个不可用。
    #[test]
    fn 只有等待放大读超时() {
        assert_eq!(Command::ListPanes.read_timeout(), READ_TIMEOUT);
        assert_eq!(
            Command::Send {
                pane: 1,
                text: Some("x".into()),
                stdin: false
            }
            .read_timeout(),
            READ_TIMEOUT
        );

        // 不给 --timeout：按服务端的默认耐心放大
        let default = Command::Wait {
            pane: 1,
            timeout: None,
        }
        .read_timeout();
        assert!(default > WAIT_DEFAULT, "默认那一档也得留富余: {default:?}");

        // 给了就按给的放大，且**必须大于**服务端会占用的那段时间
        let asked = Duration::from_secs(120);
        let t = Command::Wait {
            pane: 1,
            timeout: Some(120),
        }
        .read_timeout();
        assert!(t > asked, "{t:?}");

        // 报一个天文数字：两侧都钳到上界，读超时也跟着落在上界那一档
        let max = mt_agent_control::WAIT_MAX;
        let huge = Command::Wait {
            pane: 1,
            timeout: Some(u64::MAX),
        }
        .read_timeout();
        assert_eq!(huge, wait_read_timeout(max));
        assert!(huge > max);
    }

    /// `wait` 的请求体只带目标与耐心；不给 `--timeout` 就整个字段不出线
    /// （默认耐心只住在桌面侧那个常量上，CLI 不抄一份）。
    #[test]
    fn 等待请求体带目标与耐心() {
        let id = mt_agent_control::Identity {
            token: "t".into(),
            pane_id: 5,
        };
        let json = |c: &Command| serde_json::to_string(&c.request(&id).unwrap()).unwrap();

        assert_eq!(
            json(&Command::Wait {
                pane: 101,
                timeout: None
            }),
            r#"{"token":"t","paneId":5,"targetPaneId":101}"#
        );
        assert_eq!(
            json(&Command::Wait {
                pane: 101,
                timeout: Some(30)
            }),
            r#"{"token":"t","paneId":5,"targetPaneId":101,"timeoutMs":30000}"#
        );
    }

    /// `--pane` 是必给的；`--timeout` 可选且必须是数字。
    #[test]
    fn 等待的参数形状() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|_| ());
        assert!(parse(&["mt-agent-cli", "wait", "--pane", "1"]).is_ok());
        assert!(parse(&["mt-agent-cli", "wait", "--pane", "1", "--timeout", "30"]).is_ok());
        assert!(parse(&["mt-agent-cli", "wait"]).is_err(), "--pane 必给");
        assert!(parse(&["mt-agent-cli", "wait", "--pane", "1", "--timeout", "x"]).is_err());
    }

    /// `wait` 的四类结论、以及**每一类都退出码 0** 这件事，必须写在 `--help` 里
    /// —— 编排者看不到就会拿退出码当结论，而那样它永远读不出 attention。
    ///
    /// attention 那一条还得写清「不代答」（ADR 0003 的铁律）：这是整条编排链路
    /// 上最容易被 LLM 自作主张跨过去的一道闸。
    #[test]
    fn 帮助文案讲清楚了_wait_的四类结论() {
        for outcome in ["ai-idle", "attention", "idle", "pending"] {
            assert!(EXIT_CODE_HELP.contains(outcome), "没写 {outcome} 那一档");
        }
        assert!(
            EXIT_CODE_HELP.contains("DO NOT answer for the user"),
            "attention 不代答是 ADR 0003 的铁律，必须写死在帮助里"
        );
        assert!(
            EXIT_CODE_HELP.contains("exit code is 0 for all of them"),
            "得告诉它读 outcome 而不是读退出码"
        );
        assert!(
            EXIT_CODE_HELP.contains("only Stop"),
            "ai-idle 的三种成因得分得开，别把被打断当成做完了"
        );
        assert!(
            EXIT_CODE_HELP.contains("is NOT an error"),
            "超时不是错误，得说清"
        );
        assert!(
            !EXIT_CODE_HELP.contains("musician"),
            "用户可见文案一律用 orchestrated session（术语表）"
        );
    }

    /// 每条命令带上自己那几个字段，不带别人的。
    #[test]
    fn 请求体按命令各取所需() {
        let id = mt_agent_control::Identity {
            token: "t".into(),
            pane_id: 5,
        };
        let json = |c: &Command| serde_json::to_string(&c.request(&id).unwrap()).unwrap();

        assert_eq!(json(&Command::ListPanes), r#"{"token":"t","paneId":5}"#);
        assert_eq!(
            json(&Command::StartSession {
                launcher: "codex".into(),
                project: Some("p-api".into())
            }),
            r#"{"token":"t","paneId":5,"launcherId":"codex","projectId":"p-api"}"#
        );
        // 不给 --project 就整个字段不出线（桌面侧据此落在编排者自己的项目）
        assert_eq!(
            json(&Command::StartSession {
                launcher: "codex".into(),
                project: None
            }),
            r#"{"token":"t","paneId":5,"launcherId":"codex"}"#
        );
    }

    /// 桌面端没答上来 = 「过会儿再试」那一档，与「连不上」同码。
    #[test]
    fn 桌面端忙不过来与够不着同档() {
        assert_eq!(
            CliError::Rejected(ControlFailure {
                status: 503,
                code: "desktopBusy".into(),
                message: "m".into()
            })
            .exit_code(),
            3
        );
        // 名额满了是「改你的请求 / 等一等」，不是够不着
        assert_eq!(
            CliError::Rejected(ControlFailure {
                status: 429,
                code: "sessionLimitReached".into(),
                message: "m".into()
            })
            .exit_code(),
            4
        );
    }

    /// `--help` 里必须写清 `desktopBusy` 不等于「没起成」—— 编排者看不到这句就会
    /// 无脑重试，而重试很可能是在起第二个受编排会话。
    #[test]
    fn 帮助文案讲清楚了_desktop_busy_该怎么办() {
        assert!(EXIT_CODE_HELP.contains("desktopBusy"));
        assert!(EXIT_CODE_HELP.contains("list-panes"), "得告诉它先查一眼");
        assert!(
            !EXIT_CODE_HELP.contains("musician"),
            "用户可见文案一律用 orchestrated session（术语表）"
        );
    }

    /// `send` 那三条只有编排者会踩的坑都得写在 `--help` 里：不排队、多行整块、
    /// 以及**空正文为什么被拒**（那是 ADR 0003 的「不代答」，不是参数校验）。
    #[test]
    fn 帮助文案讲清楚了_send_的语义() {
        assert!(EXIT_CODE_HELP.contains("bracketed-paste"), "多行怎么送的");
        assert!(EXIT_CODE_HELP.contains("never queued"), "立即写穿不排队");
        assert!(
            EXIT_CODE_HELP.contains("bracketedPaste: false"),
            "回执里那一位为假意味着什么，得写清"
        );
        assert!(
            EXIT_CODE_HELP.contains("on the user's behalf"),
            "空正文被拒的理由是「不代答」，不是参数不对"
        );
    }

    /// `--text` 与 `--stdin` **恰好给一个**：都不给会去读一个不存在的管道，
    /// 都给了则不知道听谁的。这条由 clap 的 `ArgGroup` 挡，这里钉住那个声明
    /// （`debug_assert` 顺带验证整棵命令树的定义没写坏）。
    #[test]
    fn 正文来源必须恰好给一个() {
        use clap::CommandFactory;
        Cli::command().debug_assert();

        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|_| ());
        assert!(parse(&["mt-agent-cli", "send", "--pane", "1", "--text", "x"]).is_ok());
        assert!(parse(&["mt-agent-cli", "send", "--pane", "1", "--stdin"]).is_ok());
        assert!(
            parse(&["mt-agent-cli", "send", "--pane", "1"]).is_err(),
            "两个都不给必须报错，不许悄悄去读 stdin"
        );
        assert!(
            parse(&["mt-agent-cli", "send", "--pane", "1", "--stdin", "--text", "x"]).is_err(),
            "两个都给了不知道听谁的"
        );
        assert!(
            parse(&["mt-agent-cli", "send", "--text", "x"]).is_err(),
            "--pane 是必给的"
        );
    }

    /// `--text` 直给时不碰 stdin。
    ///
    /// 这个二进制常在**没有管道**的 pane 里被跑到，那时候去读 stdin 会挂住等
    /// 一个永远不来的 EOF —— 编排者看到的会是「命令卡死」。
    #[test]
    fn 有_text_就不读_stdin() {
        assert_eq!(read_body(Some("干活"), false).unwrap(), "干活");
        // 多行原样带过去（换行归一是桌面侧的事）
        assert_eq!(read_body(Some("a\nb"), false).unwrap(), "a\nb");
    }

    /// `send` 的请求体带目标编号与正文，别的命令一如既往。
    #[test]
    fn 写穿请求体带目标与正文() {
        let id = mt_agent_control::Identity {
            token: "t".into(),
            pane_id: 5,
        };
        let cmd = Command::Send {
            pane: 101,
            text: Some("跑一下测试".into()),
            stdin: false,
        };
        let json = serde_json::to_string(&cmd.request(&id).unwrap()).unwrap();
        assert_eq!(
            json,
            r#"{"token":"t","paneId":5,"targetPaneId":101,"text":"跑一下测试"}"#
        );
    }

    /// CLI 的读超时必须留出富余给桌面侧等主线程的那 3 秒。真常量对真常量的
    /// 那条断言在主仓 `crates/mt-ai/tests/orchestrator_wire.rs`（跨工作区，
    /// 那边够得到 `mt_ai::control::ACTION_TIMEOUT`）。
    #[test]
    fn 读超时大于连接超时() {
        assert!(READ_TIMEOUT > CONNECT_TIMEOUT);
    }
}
