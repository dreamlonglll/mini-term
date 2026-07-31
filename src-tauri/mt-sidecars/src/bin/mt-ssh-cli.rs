//! mt-ssh-cli —— mini-term 的 SSH 命令行 sidecar(agent 经 Bash 直接调用)。
//!
//! 对标 mt-ssh-mcp 的四个工具,但走原生 CLI 契约:远程 stdout/stderr 原样流出、
//! 退出码透传、超时 124、CLI 自身错误 2 + stderr `mt-ssh-cli: error:` 前缀。
//! 业务(连接查找/池编排/审计/护栏)全部在 `mt_sidecars::ssh_service`。
//!
//! 执行路径(spec §2):**优先连 daemon**(全机单例持全局 SshPool,同一连接
//! 二次调用免 SSH 握手)——daemon 不在则自动 detached 拉起、带退避重连;版本
//! 不符(app 升级后旧 daemon 残留)自动踢旧拉新;彻底连不上则降级为进程内
//! one-shot(建池 → 执行 → drain),stderr 记一行降级提示。对 agent 无感知差异。
//!
//! 退出码契约(spec §1):
//! - exec 正常:透传远程退出码;远端未上报 exit-status → 2 + stderr 前缀;
//! - exec 超时:124(对齐 GNU timeout)+ stderr `mt-ssh-cli: error: timed out after <N>s`;
//! - 连接/认证/传输失败、用法错误(clap 默认即 2)、daemon 不可达:2 + stderr
//!   前缀,绝不含密码;
//! - list / upload / download 成功:0。

use clap::{Parser, Subcommand};
use std::io::Write;
use std::time::Duration;

use mt_sidecars::daemon::{self, ServeOutcome};
use mt_sidecars::ipc::{self, Op, Request, ServerFrame};
use mt_sidecars::ssh_service::{self, StreamKind, TransferDirection};
use mt_ssh::pool::SshPool;

/// `--project-id` 缺省时读取的环境变量。
const PROJECT_ID_ENV: &str = "MINITERM_PROJECT_ID";

/// IPC 端点覆盖(测试/排障用,不进 SKILL.md)。
const ENDPOINT_ENV: &str = "MINITERM_SSH_CLI_ENDPOINT";

/// 超时退出码,对齐 GNU coreutils `timeout`。
const EXIT_TIMEOUT: i32 = 124;

/// CLI 自身错误(连接/认证/传输失败、用法错误)的退出码。
const EXIT_CLI_ERROR: i32 = 2;

/// 等 daemon hello 帧的上限。
const HELLO_TIMEOUT: Duration = Duration::from_secs(3);

/// 自拉起后带退避重连的间隔序列(总窗口 ~3s,spec §2 生命周期)。
const SPAWN_RETRY_DELAYS_MS: &[u64] = &[50, 100, 200, 400, 800, 1600];

#[derive(Parser)]
#[command(
    name = "mt-ssh-cli",
    version,
    about = "mini-term SSH CLI: run commands / transfer files on saved SSH connections",
    long_about = "mini-term SSH CLI. Uses SSH connections saved in mini-term (authentication is \
    handled internally; no passwords on the command line). Remote stdout/stderr stream through \
    and the remote exit code is passed through. Exit 124 = timeout, exit 2 = CLI/connection error \
    (with a `mt-ssh-cli: error:` prefix on stderr)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the saved SSH connections visible to this project
    List {
        /// Limit to connections associated with this mini-term project id
        /// (falls back to $MINITERM_PROJECT_ID, then all connections)
        #[arg(long)]
        project_id: Option<String>,
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Run a command on a remote host via a saved SSH connection
    Exec {
        /// Limit to connections associated with this mini-term project id
        #[arg(long)]
        project_id: Option<String>,
        /// Remote working directory (the command is prefixed with `cd <dir> && `)
        #[arg(long)]
        cwd: Option<String>,
        /// Timeout in seconds (default 60)
        #[arg(long)]
        timeout: Option<u64>,
        /// Name (or id) of a saved SSH connection
        connection: String,
        /// The remote command (everything after the connection; use `--` before
        /// commands that start with a dash)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Upload a local file to the remote host (SFTP)
    Upload {
        /// Limit to connections associated with this mini-term project id
        #[arg(long)]
        project_id: Option<String>,
        /// Timeout in seconds (default 300)
        #[arg(long)]
        timeout: Option<u64>,
        /// Name (or id) of a saved SSH connection
        connection: String,
        /// Local source file path
        local_path: String,
        /// Remote destination file path
        remote_path: String,
    },
    /// Download a remote file to the local machine (SFTP)
    Download {
        /// Limit to connections associated with this mini-term project id
        #[arg(long)]
        project_id: Option<String>,
        /// Timeout in seconds (default 300)
        #[arg(long)]
        timeout: Option<u64>,
        /// Name (or id) of a saved SSH connection
        connection: String,
        /// Remote source file path
        remote_path: String,
        /// Local destination file path
        local_path: String,
    },
    /// Run the connection-pool daemon in the foreground (normally auto-started; for troubleshooting)
    Daemon {
        /// Idle window in seconds before the daemon drains and exits (default 600)
        #[arg(long, hide = true)]
        idle_secs: Option<u64>,
    },
    /// Print the running daemon's version / pid / pooled session count
    DaemonStatus,
    /// Ask the running daemon to drain its pool and exit
    DaemonStop,
}

// ---------------------------------------------------------------------------
// 纯函数(可单测)
// ---------------------------------------------------------------------------

/// 解析生效的 project id:`--project-id` flag 优先,env 兜底,再缺省不限项目。
///
/// 空白值视为未提供(与 MCP 的 parse_project_id 语义一致)。
fn resolve_project_id(flag: Option<String>, env: Option<String>) -> Option<String> {
    let pick = |v: Option<String>| {
        v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    pick(flag).or_else(|| pick(env))
}

/// 把 trailing args 按空格 join 成远程命令(spec §1:与 ssh 客户端行为一致,
/// 引号语义交给远端 shell)。
fn join_command(parts: &[String]) -> String {
    parts.join(" ")
}

/// 把本地路径绝对化:daemon 的 cwd 与调用方无关,相对路径必须在调用现场解析。
/// one-shot 模式虽在本进程执行,也统一走这里,保证两条路径行为一致。
fn absolutize_local_path(path: &str) -> String {
    match std::path::absolute(path) {
        Ok(p) => p.to_string_lossy().to_string(),
        // 绝对化失败(如空路径)→ 原样透传,让底层文件操作报可读错误。
        Err(_) => path.to_string(),
    }
}

/// exec 结果 → 进程退出码的映射。
///
/// - 超时 → 124;
/// - 远程退出码 → 原样透传;
/// - 远端未上报 exit-status(如被信号杀死且服务器不转发)→ 2(CLI 错误域,
///   stderr 另有前缀行说明)。
fn exec_exit_code(outcome: &ssh_service::ExecOutcome) -> i32 {
    if outcome.timed_out {
        EXIT_TIMEOUT
    } else {
        outcome.exit_code.unwrap_or(EXIT_CLI_ERROR)
    }
}

/// exec 结束时的 stderr 收尾行 + 退出码(daemon 路径与 one-shot 共用,
/// 保证两条路径对 agent 完全一致)。
fn finish_exec(outcome: &ssh_service::ExecOutcome, timeout_secs: Option<u64>) -> i32 {
    if outcome.timed_out {
        let secs = timeout_secs
            .unwrap_or(ssh_service::DEFAULT_TIMEOUT_SECS)
            .max(1);
        eprintln!("mt-ssh-cli: error: timed out after {secs}s");
    } else if outcome.exit_code.is_none() {
        eprintln!("mt-ssh-cli: error: remote exit status unavailable");
    }
    exec_exit_code(outcome)
}

/// list 的表格文本渲染。列宽按内容自适应;不含任何敏感字段(输入视图本身
/// 就不含 password/identityFile)。
fn render_connections_table(views: &[ssh_service::SshConnectionView]) -> String {
    if views.is_empty() {
        return "no SSH connections visible to this project\n".to_string();
    }
    let headers = ["NAME", "HOST", "PORT", "USER", "GROUP", "ID"];
    let rows: Vec<[String; 6]> = views
        .iter()
        .map(|v| {
            [
                v.name.clone(),
                v.host.clone(),
                v.port.to_string(),
                v.user.clone(),
                v.group.clone().unwrap_or_default(),
                v.id.clone(),
            ]
        })
        .collect();
    let mut widths: [usize; 6] = headers.map(str::len);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(cell.len());
        }
    }
    let mut out = String::new();
    let fmt_row = |cells: [&str; 6], widths: &[usize; 6]| -> String {
        let mut line = String::new();
        for (i, (cell, w)) in cells.iter().zip(widths.iter()).enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            // 最后一列不补尾随空格
            if i < 5 {
                for _ in cell.len()..*w {
                    line.push(' ');
                }
            }
        }
        line.push('\n');
        line
    };
    out.push_str(&fmt_row(headers, &widths));
    for row in &rows {
        let cells: [&str; 6] = [&row[0], &row[1], &row[2], &row[3], &row[4], &row[5]];
        out.push_str(&fmt_row(cells, &widths));
    }
    out
}

/// list 结果的统一输出(daemon 与 one-shot 共用)。
fn print_connections(views: &[ssh_service::SshConnectionView], json: bool) -> i32 {
    if json {
        match serde_json::to_string_pretty(views) {
            Ok(s) => println!("{s}"),
            Err(e) => return fail(&format!("failed to serialize connections: {e}")),
        }
    } else {
        print!("{}", render_connections_table(views));
    }
    0
}

/// transfer 成功的统一摘要行(daemon 与 one-shot 共用)。
fn print_transfer_summary(
    direction: TransferDirection,
    bytes: u64,
    local_path: &str,
    remote_path: &str,
) {
    match direction {
        TransferDirection::Upload => {
            println!("uploaded {bytes} bytes: {local_path} ↔ {remote_path}")
        }
        TransferDirection::Download => {
            println!("downloaded {bytes} bytes: {local_path} ↔ {remote_path}")
        }
    }
}

/// 统一的 CLI 错误出口:stderr 一行前缀信息(绝不含密码),返回 exit 2。
fn fail(message: &str) -> i32 {
    eprintln!("mt-ssh-cli: error: {message}");
    EXIT_CLI_ERROR
}

/// 生效的 IPC 端点(env 覆盖供测试/排障)。
fn endpoint() -> String {
    std::env::var(ENDPOINT_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(ipc::default_endpoint)
}

// ---------------------------------------------------------------------------
// 业务请求的统一「计划」:一次构建,daemon 路径与 one-shot 降级共用
// ---------------------------------------------------------------------------

/// 业务子命令归一化后的执行计划。
struct Plan {
    req: Request,
    /// list 的 `--json` 开关(仅 list 用)。
    json: bool,
}

impl Plan {
    fn new(op: Op) -> Self {
        Self {
            req: Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(op),
                ..Default::default()
            },
            json: false,
        }
    }
}

// ---------------------------------------------------------------------------
// daemon 客户端:自拉起 + 版本握手 + 帧流处理
// ---------------------------------------------------------------------------

type DaemonReader = tokio::io::BufReader<tokio::io::ReadHalf<Box<dyn ipc::IpcStream>>>;
type DaemonWriter = tokio::io::WriteHalf<Box<dyn ipc::IpcStream>>;

/// 读一帧(单行);EOF / 解码失败返回 Err(String)。
async fn read_server_frame(reader: &mut DaemonReader) -> Result<ServerFrame, String> {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("daemon connection read failed: {e}"))?;
    if n == 0 {
        return Err("daemon closed the connection".into());
    }
    ipc::decode_frame(&line)
}

/// 写请求帧。
async fn write_request(writer: &mut DaemonWriter, req: &Request) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let line = ipc::encode_frame(req)?;
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("daemon connection write failed: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("daemon connection flush failed: {e}"))
}

/// 连接 daemon;连不上则 detached 拉起自身 `daemon` 并带退避重连。
async fn connect_or_spawn(ep: &str) -> Result<Box<dyn ipc::IpcStream>, String> {
    if let Ok(s) = ipc::connect(ep).await {
        return Ok(s);
    }
    spawn_daemon_detached()?;
    for delay in SPAWN_RETRY_DELAYS_MS {
        tokio::time::sleep(Duration::from_millis(*delay)).await;
        // 多个 CLI 并发首调:端点绑定互斥保证恰好一个 daemon 存活,
        // 抢输的实例静默退出,这里重试连接即收敛。
        if let Ok(s) = ipc::connect(ep).await {
            return Ok(s);
        }
    }
    Err("daemon did not become reachable after spawning".into())
}

/// detached 拉起 `mt-ssh-cli daemon`:无窗口、脱离本进程组、stdio 全空。
fn spawn_daemon_detached() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own binary: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // 端点被 env 覆盖时(测试/排障)透传给 daemon,保持两端一致。
    if let Ok(ep) = std::env::var(ENDPOINT_ENV) {
        cmd.env(ENDPOINT_ENV, ep);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        // DETACHED_PROCESS:不继承控制台(agent 的 Bash 结束不连坐 daemon);
        // CREATE_NEW_PROCESS_GROUP:Ctrl+C 信号不波及。
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 新进程组:调用方所在会话/作业结束不连坐 daemon(setsid 的轻量替代,
        // stdio 已全空,无控制终端依赖)。
        cmd.process_group(0);
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to spawn daemon: {e}"))
}

/// 版本握手判定:hello 报告的 daemon 版本与自身不一致即需要换代。
/// 精确相等比较 —— 同一发布内两个二进制必然同版本,任何差异都视为「旧 daemon 残留」。
fn daemon_version_mismatch(hello_version: &str, own_version: &str) -> bool {
    hello_version != own_version
}

/// 建立一条通过版本握手的 daemon 会话(hello 已消费)。
///
/// 版本不符(app 升级后旧 daemon 残留)→ 发 shutdown 踢掉旧 daemon → 等端点
/// 释放 → 重新自拉起新版。最多两轮,仍不符则放弃(caller 走降级)。
async fn open_daemon_session(ep: &str) -> Result<(DaemonReader, DaemonWriter), String> {
    let own_version = env!("CARGO_PKG_VERSION");
    let mut last_err = String::new();
    for _cycle in 0..2 {
        let stream = connect_or_spawn(ep).await?;
        let (r, w) = tokio::io::split(stream);
        let mut reader = tokio::io::BufReader::new(r);
        let mut writer = w;

        let hello = tokio::time::timeout(HELLO_TIMEOUT, read_server_frame(&mut reader))
            .await
            .map_err(|_| "daemon did not send hello in time".to_string())??;
        let version = match hello {
            ServerFrame::Hello { version, .. } => version,
            other => return Err(format!("unexpected first frame from daemon: {other:?}")),
        };
        if !daemon_version_mismatch(&version, own_version) {
            return Ok((reader, writer));
        }

        // 旧 daemon 残留:请求优雅退出(drain 池),等它放掉端点。
        eprintln!(
            "[mt-ssh-cli] daemon version {version} != {own_version}, restarting daemon"
        );
        let shutdown = Request {
            v: ipc::PROTOCOL_VERSION,
            op: Some(Op::Shutdown),
            ..Default::default()
        };
        if write_request(&mut writer, &shutdown).await.is_ok() {
            // 等 ack / 连接关闭,任何结果都继续。
            let _ = tokio::time::timeout(Duration::from_secs(3), read_server_frame(&mut reader))
                .await;
        }
        drop(reader);
        drop(writer);
        // 等端点真正释放(旧 daemon drain 池需要一点时间)。
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if ipc::connect(ep).await.is_err() {
                break;
            }
        }
        last_err = format!("old daemon v{version} still holding the endpoint");
    }
    Err(if last_err.is_empty() {
        "daemon version handshake failed".into()
    } else {
        last_err
    })
}

/// 经 daemon 执行一个业务计划。`Err` = daemon 路径不可用(caller 降级 one-shot);
/// `Ok(code)` = 请求已被 daemon 受理并出了终帧(不再降级 —— 业务错误原样透传)。
async fn run_via_daemon(plan: &Plan) -> Result<i32, String> {
    let ep = endpoint();
    let (mut reader, mut writer) = open_daemon_session(&ep).await?;
    write_request(&mut writer, &plan.req).await?;

    loop {
        // 请求已受理后帧读取不再设短超时:请求级超时由 daemon 强制执行,
        // 终帧一定会来(daemon 崩了则读到 EOF 报错)。
        let frame = match read_server_frame(&mut reader).await {
            Ok(f) => f,
            // 已受理的请求中途断连:不能盲目重跑(exec 可能有副作用),
            // 按 CLI 错误收尾而非降级。
            Err(e) => return Ok(fail(&format!("connection to daemon lost mid-request: {e}"))),
        };
        match frame {
            ServerFrame::Stdout { data_b64 } => match ipc::b64_decode(&data_b64) {
                Ok(data) => {
                    let _ = std::io::stdout()
                        .write_all(&data)
                        .and_then(|_| std::io::stdout().flush());
                }
                Err(e) => return Ok(fail(&e)),
            },
            ServerFrame::Stderr { data_b64 } => match ipc::b64_decode(&data_b64) {
                Ok(data) => {
                    let _ = std::io::stderr()
                        .write_all(&data)
                        .and_then(|_| std::io::stderr().flush());
                }
                Err(e) => return Ok(fail(&e)),
            },
            ServerFrame::Error { message } => return Ok(fail(&message)),
            ServerFrame::Result {
                exit_code,
                timed_out,
                bytes,
                connections,
                ..
            } => {
                let code = match plan.req.op {
                    Some(Op::List) => {
                        let views = connections.unwrap_or_default();
                        print_connections(&views, plan.json)
                    }
                    Some(Op::Exec) => {
                        let outcome = ssh_service::ExecOutcome {
                            exit_code,
                            timed_out: timed_out.unwrap_or(false),
                        };
                        finish_exec(&outcome, plan.req.timeout_secs)
                    }
                    Some(op @ (Op::Upload | Op::Download)) => {
                        let direction = op.transfer_direction().expect("transfer op");
                        match bytes {
                            Some(n) => {
                                print_transfer_summary(
                                    direction,
                                    n,
                                    plan.req.local_path.as_deref().unwrap_or(""),
                                    plan.req.remote_path.as_deref().unwrap_or(""),
                                );
                                0
                            }
                            None => fail("daemon result missing byte count"),
                        }
                    }
                    _ => fail("unexpected result frame for this operation"),
                };
                return Ok(code);
            }
            // 迟到的 hello(不应出现)忽略。
            ServerFrame::Hello { .. } => {}
        }
    }
}

// ---------------------------------------------------------------------------
// one-shot 执行(进程内直连 —— daemon 不可用时的降级路径)
// ---------------------------------------------------------------------------

/// one-shot 兜底执行一个业务计划(list 直接读 config,其余建临时池)。
async fn run_one_shot(plan: &Plan) -> i32 {
    let project_id = plan.req.project_id.clone();
    match plan.req.op {
        Some(Op::List) => {
            let views = ssh_service::list_connections(project_id.as_deref());
            print_connections(&views, plan.json)
        }
        Some(Op::Exec) => {
            let (Some(connection), Some(command)) =
                (plan.req.connection.clone(), plan.req.command.clone())
            else {
                return fail("exec requires a connection and a command");
            };
            let cwd = plan.req.cwd.clone();
            let timeout = plan.req.timeout_secs;
            run_with_pool(|pool| async move {
                let result = ssh_service::exec(
                    &pool,
                    project_id.as_deref(),
                    &connection,
                    &command,
                    cwd.as_deref(),
                    timeout,
                    |kind, data| {
                        // 远程输出原样字节流透传;写失败(下游管道关闭)不中断执行
                        // —— 远端命令继续跑完,与 ssh 客户端行为一致。
                        let _ = match kind {
                            StreamKind::Stdout => std::io::stdout()
                                .write_all(data)
                                .and_then(|_| std::io::stdout().flush()),
                            StreamKind::Stderr => std::io::stderr()
                                .write_all(data)
                                .and_then(|_| std::io::stderr().flush()),
                        };
                    },
                )
                .await;
                match result {
                    Ok(outcome) => finish_exec(&outcome, timeout),
                    Err(e) => fail(e.message()),
                }
            })
            .await
        }
        Some(op @ (Op::Upload | Op::Download)) => {
            let direction = op.transfer_direction().expect("transfer op");
            let (Some(connection), Some(local), Some(remote)) = (
                plan.req.connection.clone(),
                plan.req.local_path.clone(),
                plan.req.remote_path.clone(),
            ) else {
                return fail("transfer requires a connection and both paths");
            };
            let timeout = plan.req.timeout_secs;
            run_with_pool(|pool| async move {
                match ssh_service::transfer(
                    &pool,
                    direction,
                    project_id.as_deref(),
                    &connection,
                    &local,
                    &remote,
                    timeout,
                )
                .await
                {
                    Ok(bytes) => {
                        print_transfer_summary(direction, bytes, &local, &remote);
                        0
                    }
                    Err(e) => fail(e.message()),
                }
            })
            .await
        }
        _ => fail("unsupported operation"),
    }
}

/// 需要 SSH 会话的 one-shot 统一入口:建池 → 执行 → drain(远端收到
/// ByApplication disconnect,不留 dangling session)。
async fn run_with_pool<F, Fut>(f: F) -> i32
where
    F: FnOnce(std::sync::Arc<SshPool>) -> Fut,
    Fut: std::future::Future<Output = i32>,
{
    let pool = std::sync::Arc::new(SshPool::new());
    let code = f(pool.clone()).await;
    pool.shutdown().await;
    code
}

/// 业务计划入口:daemon 优先,不可用降级 one-shot(stderr 记一行)。
async fn run_plan(plan: Plan) -> i32 {
    match run_via_daemon(&plan).await {
        Ok(code) => code,
        Err(reason) => {
            eprintln!("mt-ssh-cli: daemon unavailable ({reason}); falling back to in-process SSH");
            run_one_shot(&plan).await
        }
    }
}

// ---------------------------------------------------------------------------
// 运维子命令(排障用,不进 SKILL.md)
// ---------------------------------------------------------------------------

/// `daemon`:前台跑守护进程(正常由 CLI 自动 detached 拉起)。
async fn run_daemon_foreground(idle_secs: Option<u64>) -> i32 {
    let idle = idle_secs
        .map(Duration::from_secs)
        .unwrap_or(daemon::DEFAULT_IDLE_EXIT);
    match daemon::serve(&endpoint(), idle).await {
        Ok(ServeOutcome::Idle) => {
            eprintln!("[mt-ssh-cli daemon] idle for {}s, exiting", idle.as_secs());
            0
        }
        Ok(ServeOutcome::Shutdown) => {
            eprintln!("[mt-ssh-cli daemon] shutdown requested, exiting");
            0
        }
        Err(daemon::ServeError::AlreadyRunning(e)) => {
            // 端点被占 = 另一个 daemon 赢了 —— 静默让位(并发竞态收敛)。
            eprintln!("[mt-ssh-cli daemon] not starting: {e}");
            0
        }
        // 绑定成功后的运行期故障:必须按错误暴露,不能伪装成「让位」。
        Err(daemon::ServeError::Runtime(e)) => fail(&format!("daemon failed: {e}")),
    }
}

/// `daemon-status`:打印版本 / pid / 池内 session 数;daemon 不在时友好提示。
///
/// 退出码语义对齐 `systemctl is-active` 一类状态探测命令:0 = 在跑、1 = 不在跑,
/// 便于脚本判断;「exit 2 = CLI 错误」的契约只约束业务子命令(本命令不进 SKILL.md)。
async fn run_daemon_status() -> i32 {
    let ep = endpoint();
    let Ok(stream) = ipc::connect(&ep).await else {
        println!("mt-ssh-cli daemon: not running");
        return 1;
    };
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(r);
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_server_frame(&mut reader)).await {
        Ok(Ok(ServerFrame::Hello { version, pid })) => (version, pid),
        _ => return fail("daemon reachable but did not send hello"),
    };
    let req = Request {
        v: ipc::PROTOCOL_VERSION,
        op: Some(Op::Status),
        ..Default::default()
    };
    if let Err(e) = write_request(&mut w, &req).await {
        return fail(&e);
    }
    match read_server_frame(&mut reader).await {
        Ok(ServerFrame::Result { sessions, .. }) => {
            println!(
                "mt-ssh-cli daemon: version={} pid={} sessions={}",
                hello.0,
                hello.1,
                sessions.unwrap_or(0)
            );
            0
        }
        Ok(ServerFrame::Error { message }) => fail(&message),
        Ok(other) => fail(&format!("unexpected status reply: {other:?}")),
        Err(e) => fail(&e),
    }
}

/// `daemon-stop`:请求优雅退出。幂等 —— daemon 不在时提示后仍返回 0。
async fn run_daemon_stop() -> i32 {
    let ep = endpoint();
    let Ok(stream) = ipc::connect(&ep).await else {
        println!("mt-ssh-cli daemon: not running (nothing to stop)");
        return 0;
    };
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(r);
    // 消费 hello(不校验版本 —— stop 就是给旧版本收尸用的)。
    let _ = tokio::time::timeout(HELLO_TIMEOUT, read_server_frame(&mut reader)).await;
    let req = Request {
        v: ipc::PROTOCOL_VERSION,
        op: Some(Op::Shutdown),
        ..Default::default()
    };
    if let Err(e) = write_request(&mut w, &req).await {
        return fail(&e);
    }
    match tokio::time::timeout(Duration::from_secs(5), read_server_frame(&mut reader)).await {
        Ok(Ok(ServerFrame::Result { .. })) => {
            println!("mt-ssh-cli daemon: stop requested (pool draining)");
            0
        }
        // 旧 daemon 可能不认识 shutdown → error 帧;或直接断连 —— 都算处理过。
        _ => {
            println!("mt-ssh-cli daemon: stop sent");
            0
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let env_project = std::env::var(PROJECT_ID_ENV).ok();

    let code = match cli.command {
        Command::List { project_id, json } => {
            let mut plan = Plan::new(Op::List);
            plan.req.project_id = resolve_project_id(project_id, env_project);
            plan.json = json;
            run_plan(plan).await
        }
        Command::Exec {
            project_id,
            cwd,
            timeout,
            connection,
            command,
        } => {
            let mut plan = Plan::new(Op::Exec);
            plan.req.project_id = resolve_project_id(project_id, env_project);
            plan.req.connection = Some(connection);
            plan.req.command = Some(join_command(&command));
            plan.req.cwd = cwd;
            plan.req.timeout_secs = timeout;
            run_plan(plan).await
        }
        Command::Upload {
            project_id,
            timeout,
            connection,
            local_path,
            remote_path,
        } => {
            let mut plan = Plan::new(Op::Upload);
            plan.req.project_id = resolve_project_id(project_id, env_project);
            plan.req.connection = Some(connection);
            plan.req.local_path = Some(absolutize_local_path(&local_path));
            plan.req.remote_path = Some(remote_path);
            plan.req.timeout_secs = timeout;
            run_plan(plan).await
        }
        Command::Download {
            project_id,
            timeout,
            connection,
            remote_path,
            local_path,
        } => {
            let mut plan = Plan::new(Op::Download);
            plan.req.project_id = resolve_project_id(project_id, env_project);
            plan.req.connection = Some(connection);
            plan.req.local_path = Some(absolutize_local_path(&local_path));
            plan.req.remote_path = Some(remote_path);
            plan.req.timeout_secs = timeout;
            run_plan(plan).await
        }
        Command::Daemon { idle_secs } => run_daemon_foreground(idle_secs).await,
        Command::DaemonStatus => run_daemon_status().await,
        Command::DaemonStop => run_daemon_stop().await,
    };

    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_service::ExecOutcome;

    // --- resolve_project_id: flag/env 优先级 ---

    #[test]
    fn resolve_project_id_flag_wins_over_env() {
        assert_eq!(
            resolve_project_id(Some("flag-id".into()), Some("env-id".into())),
            Some("flag-id".into())
        );
    }

    #[test]
    fn resolve_project_id_env_fallback() {
        assert_eq!(
            resolve_project_id(None, Some("env-id".into())),
            Some("env-id".into())
        );
    }

    #[test]
    fn resolve_project_id_none_when_both_absent() {
        assert_eq!(resolve_project_id(None, None), None);
    }

    #[test]
    fn resolve_project_id_blank_flag_falls_through_to_env() {
        // 空白 flag 视为未提供 → 用 env
        assert_eq!(
            resolve_project_id(Some("  ".into()), Some("env-id".into())),
            Some("env-id".into())
        );
        // 两边都空白 → None
        assert_eq!(resolve_project_id(Some("".into()), Some(" ".into())), None);
    }

    // --- join_command: trailing args 拼装 ---

    #[test]
    fn join_command_joins_with_spaces() {
        let parts = vec![
            "tail".to_string(),
            "-f".to_string(),
            "/var/log/app.log".to_string(),
        ];
        assert_eq!(join_command(&parts), "tail -f /var/log/app.log");
    }

    #[test]
    fn join_command_single_part_passthrough() {
        assert_eq!(join_command(&["uptime".to_string()]), "uptime");
    }

    // --- clap 解析:trailing args / -- 分隔 / 连字符参数 ---

    #[test]
    fn clap_exec_captures_trailing_args_with_hyphens() {
        let cli = Cli::try_parse_from([
            "mt-ssh-cli", "exec", "--project-id", "p1", "prod", "tail", "-f", "/var/log/x",
        ])
        .unwrap();
        match cli.command {
            Command::Exec {
                project_id,
                connection,
                command,
                ..
            } => {
                assert_eq!(project_id.as_deref(), Some("p1"));
                assert_eq!(connection, "prod");
                assert_eq!(command, ["tail", "-f", "/var/log/x"]);
            }
            _ => panic!("expected exec"),
        }
    }

    #[test]
    fn clap_exec_double_dash_separates_command() {
        let cli =
            Cli::try_parse_from(["mt-ssh-cli", "exec", "prod", "--", "ls", "-la"]).unwrap();
        match cli.command {
            Command::Exec { command, .. } => assert_eq!(command, ["ls", "-la"]),
            _ => panic!("expected exec"),
        }
    }

    #[test]
    fn clap_exec_requires_command() {
        // 只给 connection 不给命令 → 用法错误(clap 默认 exit 2,契约一致)
        assert!(Cli::try_parse_from(["mt-ssh-cli", "exec", "prod"]).is_err());
    }

    #[test]
    fn clap_parses_ops_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["mt-ssh-cli", "daemon"]).unwrap().command,
            Command::Daemon { idle_secs: None }
        ));
        assert!(matches!(
            Cli::try_parse_from(["mt-ssh-cli", "daemon-status"]).unwrap().command,
            Command::DaemonStatus
        ));
        assert!(matches!(
            Cli::try_parse_from(["mt-ssh-cli", "daemon-stop"]).unwrap().command,
            Command::DaemonStop
        ));
    }

    // --- 版本握手比对 ---

    #[test]
    fn daemon_version_mismatch_is_exact_equality() {
        assert!(!daemon_version_mismatch("0.4.8", "0.4.8"));
        assert!(daemon_version_mismatch("0.4.8", "0.4.9"));
        // 任何差异都算旧 daemon(不做语义化版本比较)
        assert!(daemon_version_mismatch("0.4.8-rc1", "0.4.8"));
    }

    // --- exec_exit_code: 退出码矩阵 ---

    #[test]
    fn exec_exit_code_passes_through_remote_code() {
        let ok = ExecOutcome {
            exit_code: Some(0),
            timed_out: false,
        };
        let fail17 = ExecOutcome {
            exit_code: Some(17),
            timed_out: false,
        };
        assert_eq!(exec_exit_code(&ok), 0);
        assert_eq!(exec_exit_code(&fail17), 17);
    }

    #[test]
    fn exec_exit_code_timeout_is_124() {
        let t = ExecOutcome {
            exit_code: None,
            timed_out: true,
        };
        assert_eq!(exec_exit_code(&t), 124);
    }

    #[test]
    fn exec_exit_code_missing_status_maps_to_cli_error() {
        let none = ExecOutcome {
            exit_code: None,
            timed_out: false,
        };
        assert_eq!(exec_exit_code(&none), 2);
    }

    // --- absolutize_local_path ---

    #[test]
    fn absolutize_local_path_resolves_relative() {
        let abs = absolutize_local_path("some-file.txt");
        assert!(std::path::Path::new(&abs).is_absolute(), "got: {abs}");
        assert!(abs.ends_with("some-file.txt"));
    }

    #[test]
    fn absolutize_local_path_keeps_absolute_unchanged() {
        let input = if cfg!(windows) {
            r"C:\tmp\x.bin"
        } else {
            "/tmp/x.bin"
        };
        assert_eq!(absolutize_local_path(input), input);
    }

    // --- list 表格渲染 ---

    fn view(id: &str, name: &str, group: Option<&str>) -> ssh_service::SshConnectionView {
        ssh_service::SshConnectionView {
            id: id.into(),
            name: name.into(),
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            group: group.map(|s| s.into()),
        }
    }

    #[test]
    fn render_table_contains_all_fields_and_header() {
        let out = render_connections_table(&[view("id-1", "prod", Some("内网"))]);
        assert!(out.starts_with("NAME"));
        assert!(out.contains("prod"));
        assert!(out.contains("10.0.0.5"));
        assert!(out.contains("22"));
        assert!(out.contains("root"));
        assert!(out.contains("内网"));
        assert!(out.contains("id-1"));
    }

    #[test]
    fn render_table_empty_has_friendly_message() {
        let out = render_connections_table(&[]);
        assert!(out.contains("no SSH connections"));
    }
}
