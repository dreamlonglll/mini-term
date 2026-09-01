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
//! ```
//!
//! 成功：JSON 到 stdout，退出码 0。失败：JSON 到 stderr，退出码见 [`CliError::exit_code`]。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use mt_agent_control::{
    ControlFailure, ControlRequest, IdentityError, identity_from_env, parse_launchers, parse_panes,
    parse_projects, parse_started_pane, CONNECT_TIMEOUT, CONTROL_PREFIX, READ_TIMEOUT,
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
     change the request or wait for one of your sessions to finish";

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
}

impl Command {
    /// 路由里那一段（与桌面侧的命令名一字不差）。
    fn endpoint(&self) -> &'static str {
        match self {
            Self::ListLaunchers => "list-launchers",
            Self::ListProjects => "list-projects",
            Self::StartSession { .. } => "start-session",
            Self::ListPanes => "list-panes",
        }
    }

    /// 这条命令的请求体。**只带 id** —— 启动器的命令文本从不经过 CLI
    /// （ADR 0002：命令只能来自桌面端配置）。
    fn request(&self, identity: &mt_agent_control::Identity) -> ControlRequest {
        match self {
            Self::StartSession { launcher, project } => {
                ControlRequest::start_session(identity, launcher, project.as_deref())
            }
            _ => ControlRequest::from(identity),
        }
    }
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
    let body = serde_json::to_string(&command.request(&identity))
        .map_err(|e| CliError::DesktopUnreachable(format!("cannot encode request: {e}")))?;
    let (status, response) = post(port, command.endpoint(), &body)?;

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
fn post(port: u16, endpoint: &str, body: &str) -> Result<(u16, String), CliError> {
    let addr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| CliError::DesktopUnreachable(format!("bad address: {e}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| {
        CliError::DesktopUnreachable(format!("cannot reach mini-term on 127.0.0.1:{port}: {e}"))
    })?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
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
        assert_eq!(CONTROL_PREFIX, "/control/");
    }

    /// 每条命令带上自己那几个字段，不带别人的。
    #[test]
    fn 请求体按命令各取所需() {
        let id = mt_agent_control::Identity {
            token: "t".into(),
            pane_id: 5,
        };
        let json = |c: &Command| serde_json::to_string(&c.request(&id)).unwrap();

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

    /// CLI 的读超时必须留出富余给桌面侧等主线程的那 3 秒。真常量对真常量的
    /// 那条断言在主仓 `crates/mt-ai/tests/orchestrator_wire.rs`（跨工作区，
    /// 那边够得到 `mt_ai::control::ACTION_TIMEOUT`）。
    #[test]
    fn 读超时大于连接超时() {
        assert!(READ_TIMEOUT > CONNECT_TIMEOUT);
    }
}
