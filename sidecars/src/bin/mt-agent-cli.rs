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
//! mt-agent-cli list-launchers     # 我能用哪些 AI 启动器起乐手
//! mt-agent-cli list-projects      # 我能在哪些项目里起(本项目 + 同分组)
//! ```
//!
//! 成功：JSON 到 stdout，退出码 0。失败：JSON 到 stderr，退出码见 [`exit_code`]。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use mt_agent_control::{
    ControlFailure, ControlRequest, IdentityError, identity_from_env, parse_launchers,
    parse_projects, CONTROL_PREFIX,
};

/// 连接与读取超时。控制端点是本机进程，慢到这个份上一定是出事了。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(
    name = "mt-agent-cli",
    about = "mini-term orchestrator control CLI (requires an orchestration-enabled pane)",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the AI launchers this orchestrator may start musicians with.
    ListLaunchers,
    /// List the projects reachable from here (own project + same group).
    ListProjects,
}

impl Command {
    /// 路由里那一段（与桌面侧的命令名一字不差）。
    fn endpoint(&self) -> &'static str {
        match self {
            Self::ListLaunchers => "list-launchers",
            Self::ListProjects => "list-projects",
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
    let body = serde_json::to_string(&ControlRequest::from(&identity))
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
    /// 2 = 你没有这个能力；3 = 桌面端够不着；4 = 请求被拒。
    fn exit_code(&self) -> i32 {
        match self {
            Self::Identity(_) => 2,
            Self::DesktopUnreachable(_) => 3,
            Self::Rejected(f) if f.is_denied() => 2,
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
        assert_eq!(CONTROL_PREFIX, "/control/");
    }
}
