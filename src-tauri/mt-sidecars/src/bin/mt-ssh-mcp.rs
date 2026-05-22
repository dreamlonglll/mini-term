//! mt-ssh-mcp —— mini-term 的 SSH MCP server sidecar(stdio 传输)。
//!
//! 这是一个独立的瘦二进制(对标 `miniterm-hook`),用官方 `rmcp` crate 跑
//! 一个 stdio MCP server,把 mini-term 已保存的 SSH 连接暴露成 MCP 工具,
//! 供运行在 mini-term 终端里的 AI agent(Claude Code / Codex)调用。
//!
//! 实现层:`ssh_exec` 通过 `mt_sidecars::pool::SshPool` 在 sidecar 进程内
//! 维护一个 `connection_id → SSH session` 缓存池(russh 0.61 持久连接),
//! 第一次调用建 session,后续复用,彻底绕开旧的「每次 spawn ssh 子进程 +
//! PTY autofill 喂密码」路径。
//!
//! stdio 铁律:进程的 **stdout 只能输出 MCP 协议 JSON-RPC 消息**;任何日志 /
//! 调试输出一律走 stderr,否则会破坏 JSON-RPC 帧、导致客户端判定 server 挂掉。
//! `ssh_exec` 收集到的远程输出是**工具结果数据**,必须放进返回值序列化,
//! 绝不能透传到本进程 stdout。

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use russh::ChannelMsg;
use serde::Serialize;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use mt_sidecars::pool::SshPool;

/// 输出封顶:stdout / stderr 各自最多保留约 100 KB,超出截断并标记。
const OUTPUT_CAP_BYTES: usize = 100 * 1024;

/// `ssh_exec` 的默认超时秒数。
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// 审计日志文件名(与 `config.json` 同目录)。
const AUDIT_LOG_FILE: &str = "ssh-mcp-audit.log";

// ---------------------------------------------------------------------------
// ssh_list_connections
// ---------------------------------------------------------------------------

/// `ssh_list_connections` 工具的入参 —— 无参数。
///
/// rmcp 的 `#[tool]` 仍要求入参结构体派生 `Deserialize + JsonSchema`,
/// 这里用一个空结构体表示「无入参」。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListConnectionsArgs {}

/// 暴露给 agent 的 SSH 连接视图。
///
/// 安全要点:**绝不包含 `password` / `identityFile` 等敏感字段**——
/// `mt_core::SshConnection` 含明文密码,绝不能直接序列化给 agent。
/// 这里只挑选展示用的非敏感字段。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SshConnectionView {
    /// 连接稳定 id,后续 `ssh_exec` 用它指定连接。
    id: String,
    /// 连接展示名。
    name: String,
    /// 远程主机地址。
    host: String,
    /// 远程 SSH 端口。
    port: u16,
    /// 登录用户名。
    user: String,
    /// 连接所属分组(可选)。
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<String>,
}

/// 把连接列表投影成对 agent 可见的视图。
///
/// 安全核心:映射到不含 `password` / `identityFile` 的 `SshConnectionView`,
/// 绝不把明文密码序列化给 agent。传入的连接列表已由
/// `read_ssh_connections_for_project` 按项目关联范围过滤。抽成纯函数便于单测。
fn connection_views(conns: Vec<mt_core::SshConnection>) -> Vec<SshConnectionView> {
    conns
        .into_iter()
        .map(|c| SshConnectionView {
            id: c.id,
            name: c.name,
            host: c.host,
            port: c.port,
            user: c.user,
            group: c.group,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ssh_exec —— 入参 / 出参
// ---------------------------------------------------------------------------

/// `ssh_exec` 工具的入参。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SshExecArgs {
    /// 目标连接:已保存 SSH 连接的 name(也接受 id)。
    #[schemars(description = "Name of a saved SSH connection (its id is also accepted).")]
    connection: String,
    /// 在远程主机上执行的命令。
    #[schemars(description = "The command to run on the remote host.")]
    command: String,
    /// 可选:超时秒数,超时关闭 channel(不影响池里 session)。缺省 60。
    #[schemars(
        description = "Optional timeout in seconds; the exec channel is closed if it exceeds this. Defaults to 60."
    )]
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// 可选:远程工作目录,非空时命令前缀 `cd <cwd> && `。
    #[schemars(
        description = "Optional remote working directory; the command is prefixed with `cd <cwd> && ` when set."
    )]
    #[serde(default)]
    cwd: Option<String>,
}

/// `ssh_exec` 的执行结果,序列化为工具返回的 JSON。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SshExecResult {
    /// 远程命令的标准输出(可能被封顶截断)。
    stdout: String,
    /// 远程命令的标准错误(可能被封顶截断)。
    stderr: String,
    /// 退出码。超时被强制关闭 channel 时为 None。
    exit_code: Option<i32>,
    /// stdout 或 stderr 是否因超出封顶被截断。
    truncated: bool,
    /// 是否因超时被强制终止。
    timed_out: bool,
}

// ---------------------------------------------------------------------------
// ssh_exec —— 纯逻辑(可单测)
// ---------------------------------------------------------------------------

/// 在连接列表里按 name 或 id 查找连接。
///
/// 匹配规则(先 name 后 id,均大小写敏感,与 SshModal 行为一致):
/// - name 精确命中多条 → 歧义错误;
/// - name 无命中再按 id 精确命中;
/// - 均无命中 → 未找到错误。
///
/// 传入列表已由 `read_ssh_connections_for_project` 按项目关联范围过滤,
/// 因此本项目无权访问的连接天然「未找到」,无需再单独校验授权。
/// 所有错误信息**不含密码**(只回显用户给的标识符)。
fn find_connection(
    conns: &[mt_core::SshConnection],
    selector: &str,
) -> Result<mt_core::SshConnection, String> {
    let by_name: Vec<&mt_core::SshConnection> =
        conns.iter().filter(|c| c.name == selector).collect();
    match by_name.len() {
        1 => Ok(by_name[0].clone()),
        n if n > 1 => Err(format!(
            "SSH connection name '{selector}' is ambiguous: {n} connections share this name. \
            Use the connection id instead."
        )),
        // name 无命中 → 退而按 id 精确匹配
        _ => match conns.iter().find(|c| c.id == selector) {
            Some(c) => Ok(c.clone()),
            None => Err(format!(
                "No SSH connection found matching '{selector}'. \
                Call ssh_list_connections to see available connections."
            )),
        },
    }
}

/// 拼远程要执行的命令:`cwd` 非空时前缀 `cd <cwd> && `。
fn build_remote_command(command: &str, cwd: Option<&str>) -> String {
    match cwd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(dir) => format!("cd {dir} && {command}"),
        None => command.to_string(),
    }
}

/// 把一段输出按字节封顶。返回 (截断后的文本, 是否发生截断)。
///
/// 按字节而非字符封顶以严格控制返回体积;在 UTF-8 字符边界处切割,
/// 避免产生非法 UTF-8。
fn cap_output(s: &str, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s.to_string(), false);
    }
    // 从 cap 处向前回退到一个 UTF-8 字符边界
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[output truncated]");
    (out, true)
}

/// 格式化一行审计日志。抽出便于单测。
///
/// 形如:`2026-05-18T12:34:56Z\tconn=prod\texit=0\tcmd=ls -la`
/// 命令里的换行替换成空格,保证一次执行就是一行。
fn format_audit_line(timestamp: &str, conn_name: &str, command: &str, exit: Option<i32>) -> String {
    let exit_str = match exit {
        Some(code) => code.to_string(),
        None => "timeout".to_string(),
    };
    let one_line_cmd = command.replace(['\n', '\r'], " ");
    format!("{timestamp}\tconn={conn_name}\texit={exit_str}\tcmd={one_line_cmd}\n")
}

/// 极简 UTC 时间戳(无需引入 chrono):基于 UNIX 秒数。
///
/// 仅用于审计日志,精确到秒、UTC。失败回退 "unknown"。
fn utc_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => return "unknown".to_string(),
    };
    // 把 UNIX 秒数拆成 YYYY-MM-DDTHH:MM:SSZ(标准公历换算)。
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // 从 1970-01-01 起按年累加,处理闰年。
    let mut year = 1970i64;
    let mut day_of_era = days as i64;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let year_len = if leap { 366 } else { 365 };
        if day_of_era < year_len {
            break;
        }
        day_of_era -= year_len;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_lens = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    while month < 12 && day_of_era >= month_lens[month] {
        day_of_era -= month_lens[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02}T{hh:02}:{mm:02}:{ss:02}Z",
        month + 1,
        day_of_era + 1,
    )
}

/// 把一行审计日志追加到 `{config.json 所在目录}/ssh-mcp-audit.log`。
///
/// 写日志失败绝不影响工具结果 —— 只往 stderr 记一笔。
fn append_audit_log(conn_name: &str, command: &str, exit: Option<i32>) {
    let Some(cfg_path) = mt_core::config_json_path() else {
        eprintln!("[mt-ssh-mcp] audit: cannot locate config.json dir, skipping audit log");
        return;
    };
    let Some(dir) = cfg_path.parent() else {
        eprintln!("[mt-ssh-mcp] audit: config.json has no parent dir, skipping audit log");
        return;
    };
    let log_path = dir.join(AUDIT_LOG_FILE);
    let line = format_audit_line(&utc_timestamp(), conn_name, command, exit);
    let write_result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = write_result {
        eprintln!("[mt-ssh-mcp] audit: failed to write {AUDIT_LOG_FILE}: {e}");
    }
}

// ---------------------------------------------------------------------------
// ssh_exec —— 远程执行(走 SshPool)
// ---------------------------------------------------------------------------

/// 单次 channel exec 的累积输出。
struct ChannelOutcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
}

/// 在已 acquire 到的 session 上开 channel、跑 exec、收集流。
///
/// 不做超时——超时由外层 `tokio::time::timeout` 兜底。返回 `Result`:
/// `Err(String)` 代表 transport-level 失败(channel 开不了 / exec 发不出去),
/// caller 可用此信号触发"evict + 重连"。
async fn run_exec_on_session(
    session: &mt_sidecars::pool::CachedSession,
    remote_command: &str,
) -> Result<ChannelOutcome, String> {
    let handle_guard = session.lock().await;
    let mut channel = handle_guard
        .channel_open_session()
        .await
        .map_err(|e| format!("channel_open_session failed: {e}"))?;
    channel
        .exec(true, remote_command)
        .await
        .map_err(|e| format!("channel exec failed: {e}"))?;

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut exit_code: Option<i32> = None;

    // RFC 4254 §5.2: ExtendedData.ext == 1 表示 stderr。
    const SSH_EXTENDED_DATA_STDERR: u32 = 1;

    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, ext } if ext == SSH_EXTENDED_DATA_STDERR => {
                stderr.extend_from_slice(&data);
            }
            ChannelMsg::ExitStatus { exit_status } => {
                exit_code = Some(exit_status as i32);
                // 不能立刻 break:服务器可能在 ExitStatus 之后还会发 Close/Eof,
                // 也可能还有最后一批 Data 待收。让循环走到 wait() 返回 None。
            }
            // 收到 Eof / Close 后,wait() 很快会返回 None 退出循环;不需要 break。
            _ => {}
        }
    }
    // 主动关闭 channel(server-side 可能已经关了,这里幂等 best-effort)。
    let _ = channel.close().await;
    drop(handle_guard);

    Ok(ChannelOutcome {
        stdout,
        stderr,
        exit_code,
    })
}

/// 把一次 channel 执行的累积结果做 cap_output + 包装成工具返回结构。
fn finalize_outcome(outcome: ChannelOutcome, timed_out: bool) -> SshExecResult {
    let (stdout, out_trunc) =
        cap_output(&String::from_utf8_lossy(&outcome.stdout), OUTPUT_CAP_BYTES);
    let (stderr, err_trunc) =
        cap_output(&String::from_utf8_lossy(&outcome.stderr), OUTPUT_CAP_BYTES);
    SshExecResult {
        stdout,
        stderr,
        exit_code: if timed_out { None } else { outcome.exit_code },
        truncated: out_trunc || err_trunc,
        timed_out,
    }
}

/// 从进程参数里解析 `--project-id <id>`。
///
/// 支持 `--project-id <value>` 与 `--project-id=<value>` 两种写法。
/// 未提供 / 值为空白 → `None`(不限定项目,暴露全部连接)。
/// 抽成纯函数(入参为参数序列)便于单测;解析绝不 panic。
fn parse_project_id<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix("--project-id=") {
            let v = rest.trim();
            return (!v.is_empty()).then(|| v.to_string());
        }
        if arg == "--project-id" {
            let v = iter.next()?.trim().to_string();
            return (!v.is_empty()).then_some(v);
        }
    }
    None
}

#[derive(Clone)]
struct SshMcp {
    /// 本 sidecar 所属项目的 id(来自启动参数 `--project-id`)。
    /// `None` = 未指定项目 → 暴露全部已保存连接。
    project_id: Option<String>,
    /// 进程内 SSH 持久会话池。`Arc` 让 `SshMcp` 的 derive(Clone)
    /// (rmcp 框架要求)保持廉价 —— Clone 只复制 Arc 指针。
    pool: Arc<SshPool>,
}

#[tool_router]
impl SshMcp {
    /// 列出本项目可访问的、已保存的 SSH 连接。
    ///
    /// 范围由 mini-term 里该项目的「关联 SSH」设定决定,且**不含任何密码字段**。
    #[tool(
        description = "List the saved SSH connections this project's agent may access. \
        Returns connection metadata (id, name, host, port, user, group) with NO passwords. \
        Use a connection's name (or id) with ssh_exec to run commands on that host."
    )]
    async fn ssh_list_connections(
        &self,
        Parameters(ListConnectionsArgs {}): Parameters<ListConnectionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        // 读全局 config.json 的 sshConnections,并按本项目的关联范围过滤;
        // 文件缺失/解析失败时为空 Vec。
        let views = connection_views(mt_core::read_ssh_connections_for_project(
            self.project_id.as_deref(),
        ));

        // 序列化失败属于不可恢复的内部错误,回结构化 MCP 错误而非 panic。
        let json = serde_json::to_string(&views).map_err(|e| {
            McpError::internal_error(format!("failed to serialize SSH connections: {e}"), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// 通过已保存的 SSH 连接在远程主机上执行一条命令。
    #[tool(
        description = "Run a command on a remote host via a saved SSH connection. \
        Provide the connection's name (or id) and the command. \
        Optionally set timeout_secs (default 60) and cwd (remote working directory). \
        Returns stdout, stderr, exitCode and a truncated flag. \
        Only connections this project is associated with can be used."
    )]
    async fn ssh_exec(
        &self,
        Parameters(args): Parameters<SshExecArgs>,
    ) -> Result<CallToolResult, McpError> {
        let SshExecArgs {
            connection,
            command,
            timeout_secs,
            cwd,
        } = args;

        // 1. 查连接(列表已按本项目关联范围过滤,越权连接天然「未找到」)。错误不含密码。
        //    `read_ssh_connections_for_project` 仍在每次 ssh_exec 入口调用 ——
        //    保证用户在主程序里新加的连接立即可见。一旦池里建好同 id 的 session,
        //    后续连接信息「变更」不会动那条 session(见 PRD「配置一致性」)。
        let conn = find_connection(
            &mt_core::read_ssh_connections_for_project(self.project_id.as_deref()),
            &connection,
        )
        .map_err(|e| McpError::invalid_params(e, None))?;

        // 2. 拼远程命令(可选 cwd 前缀)。
        let remote_command = build_remote_command(&command, cwd.as_deref());
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
        let conn_name_for_audit = conn.name.clone();
        let conn_id = conn.id.clone();

        // 3. 走池:lazy 建/复用 session。
        //    pool.acquire 失败:transport / auth 层错;直接返给 agent(不进 retry,
        //    auth 错重试只会徒增暴力)。
        let session = self
            .pool
            .acquire(&conn)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
        if session.is_unhealthy_now() {
            return Err(McpError::internal_error(
                "session is in cooldown after a previous auth failure; retry shortly",
                None,
            ));
        }

        // 4. 在 session 上跑 exec + 收集输出,整段套 tokio::time::timeout 超时。
        //    第一次失败 → evict + 重新 acquire → 再试一次 → 仍失败 → 标 unhealthy + 返错。
        let outcome_result =
            tokio::time::timeout(timeout, run_exec_on_session(&session, &remote_command)).await;

        let exec_result = match outcome_result {
            Ok(Ok(outcome)) => {
                session.touch();
                finalize_outcome(outcome, false)
            }
            Ok(Err(first_err)) => {
                // transport-level 错(channel 开不了 / exec 发不出),可能是死链 race。
                // 移除并重建,再试一次。
                eprintln!("[mt-ssh-mcp] exec on cached session failed, retrying: {first_err}");
                self.pool.evict(&conn_id).await;
                let session2 = self.pool.acquire(&conn).await.map_err(|e| {
                    McpError::internal_error(format!("reconnect failed: {e}"), None)
                })?;
                if session2.is_unhealthy_now() {
                    return Err(McpError::internal_error(
                        "session is in cooldown after a previous auth failure; retry shortly",
                        None,
                    ));
                }
                match tokio::time::timeout(
                    timeout,
                    run_exec_on_session(&session2, &remote_command),
                )
                .await
                {
                    Ok(Ok(outcome)) => {
                        session2.touch();
                        finalize_outcome(outcome, false)
                    }
                    Ok(Err(second_err)) => {
                        // 两次都失败 —— 进 30s gatetime cooldown,避免连发把服务器打死。
                        session2.mark_unhealthy(Duration::from_secs(30));
                        append_audit_log(&conn_name_for_audit, &command, None);
                        return Err(McpError::internal_error(
                            format!("ssh exec failed after retry: {second_err}"),
                            None,
                        ));
                    }
                    Err(_) => {
                        // 第二次:超时。已有输出仍然丢失(channel 收集器没拿到),
                        // 但与原行为一致 —— 强制中止 + timedOut=true。
                        eprintln!(
                            "[mt-ssh-mcp] exec timed out on retry after {}s",
                            timeout.as_secs()
                        );
                        SshExecResult {
                            stdout: String::new(),
                            stderr: String::new(),
                            exit_code: None,
                            truncated: false,
                            timed_out: true,
                        }
                    }
                }
            }
            Err(_) => {
                // 第一次超时:已无法拿到部分输出(future 被取消),只能上报 timeout。
                // 不 evict、不 disconnect session —— 单 channel 超时不代表整个 session 死了。
                eprintln!(
                    "[mt-ssh-mcp] exec timed out after {}s on connection '{}'",
                    timeout.as_secs(),
                    conn_name_for_audit
                );
                SshExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                    truncated: false,
                    timed_out: true,
                }
            }
        };

        // 5. 审计日志:每次执行记一行(失败不影响结果)。
        append_audit_log(&conn_name_for_audit, &command, exec_result.exit_code);

        let json = serde_json::to_string(&exec_result).map_err(|e| {
            McpError::internal_error(format!("failed to serialize ssh_exec result: {e}"), None)
        })?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for SshMcp {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo 是 #[non_exhaustive],不能用结构体字面量构造;
        // 从 Default 起手再逐字段赋值。
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(
            "mini-term SSH tools. Use ssh_list_connections to discover SSH connections \
            that mini-term has shared with agents, then ssh_exec to run commands on them."
                .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 启动参数 `--project-id <id>` 决定本 sidecar 所属项目;缺省则暴露全部连接。
    let project_id = parse_project_id(std::env::args().skip(1));

    // 日志走 stderr —— stdout 留给 MCP 协议 JSON。失败也只往 stderr 写。
    eprintln!(
        "[mt-ssh-mcp] starting stdio MCP server (project: {})",
        project_id.as_deref().unwrap_or("<all>")
    );

    // 初始化进程内 SSH 会话池(默认 profile,无需 env 调参)。
    let pool = Arc::new(SshPool::new());

    // 握手并注册工具;.serve() 绑定进程的 stdin/stdout 作为 stdio 传输。
    // `SshMcp` 派生 Clone(rmcp 框架要求);Clone 内部走 `Arc::clone`,共享同一池。
    let service = SshMcp {
        project_id,
        pool: pool.clone(),
    }
    .serve(stdio())
    .await
    .inspect_err(|e| {
        eprintln!("[mt-ssh-mcp] failed to start server: {e}");
    })?;

    // 阻塞直到 stdin 关闭 / 客户端断开 —— 这是 sidecar 正常退出的信号。
    service.waiting().await?;

    // shutdown 钩子:对每条 session 跑 disconnect(ByApplication)+ 2s 单 session 超时,
    // 并 abort 池内后台 reaper。**必须在 eprintln 退出语句前调用**——否则 sidecar 进程
    // 退出后远端只能感知 TCP RST,留下 dangling channel/session 直到服务器自身回收。
    eprintln!("[mt-ssh-mcp] draining session pool");
    pool.shutdown().await;

    eprintln!("[mt-ssh-mcp] client disconnected, exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str, password: Option<&str>) -> mt_core::SshConnection {
        mt_core::SshConnection {
            id: id.into(),
            name: format!("conn-{id}"),
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            password: password.map(|s| s.into()),
            identity_file: Some("/home/u/.ssh/id_rsa".into()),
            group: Some("内网".into()),
        }
    }

    // --- parse_project_id ---

    #[test]
    fn parse_project_id_space_form() {
        let args = vec!["--project-id".to_string(), "p1".to_string()];
        assert_eq!(parse_project_id(args), Some("p1".to_string()));
    }

    #[test]
    fn parse_project_id_equals_form() {
        let args = vec!["--project-id=p2".to_string()];
        assert_eq!(parse_project_id(args), Some("p2".to_string()));
    }

    #[test]
    fn parse_project_id_absent_yields_none() {
        let args = vec!["--other".to_string(), "x".to_string()];
        assert_eq!(parse_project_id(args), None);
    }

    #[test]
    fn parse_project_id_blank_value_yields_none() {
        assert_eq!(
            parse_project_id(vec!["--project-id".to_string(), "  ".to_string()]),
            None
        );
        assert_eq!(parse_project_id(vec!["--project-id=".to_string()]), None);
    }

    #[test]
    fn parse_project_id_missing_value_yields_none() {
        assert_eq!(parse_project_id(vec!["--project-id".to_string()]), None);
    }

    // --- connection_views ---

    #[test]
    fn connection_views_projects_all_connections() {
        // connection_views 本身不再过滤,原样投影传入的连接列表
        let conns = vec![conn("1", Some("secret1")), conn("2", None)];
        let views = connection_views(conns);
        let ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["1", "2"]);
    }

    #[test]
    fn connection_views_empty_input_yields_empty() {
        assert!(connection_views(vec![]).is_empty());
    }

    #[test]
    fn serialized_view_never_leaks_password_or_identity_file() {
        // 安全验收:即便源连接有明文密码与私钥路径,序列化结果也绝不能含它们。
        let conns = vec![conn("1", Some("super-secret-password"))];
        let views = connection_views(conns);
        let json = serde_json::to_string(&views).unwrap();
        assert!(!json.contains("super-secret-password"));
        assert!(!json.to_lowercase().contains("password"));
        assert!(!json.contains("identityFile"));
        assert!(!json.contains("id_rsa"));
        // 但应保留展示字段
        assert!(json.contains("\"host\":\"10.0.0.5\""));
    }

    // --- find_connection ---

    #[test]
    fn find_connection_matches_by_name() {
        let conns = vec![conn("1", None)];
        let found = find_connection(&conns, "conn-1").unwrap();
        assert_eq!(found.id, "1");
    }

    #[test]
    fn find_connection_matches_by_id_when_name_misses() {
        let conns = vec![conn("abc", None)];
        let found = find_connection(&conns, "abc").unwrap();
        assert_eq!(found.id, "abc");
    }

    #[test]
    fn find_connection_errors_when_not_found() {
        let conns = vec![conn("1", None)];
        let err = find_connection(&conns, "does-not-exist").unwrap_err();
        assert!(err.contains("No SSH connection found"));
    }

    #[test]
    fn find_connection_errors_on_ambiguous_name() {
        let mut a = conn("1", None);
        let mut b = conn("2", None);
        a.name = "dup".into();
        b.name = "dup".into();
        let err = find_connection(&[a, b], "dup").unwrap_err();
        assert!(err.contains("ambiguous"));
    }

    #[test]
    fn find_connection_not_found_error_never_contains_password() {
        // 安全:未找到时错误信息只回显 selector,绝不泄漏任何连接的明文密码。
        let conns = vec![conn("1", Some("topsecretpw"))];
        let err = find_connection(&conns, "does-not-exist").unwrap_err();
        assert!(!err.contains("topsecretpw"));
    }

    // --- build_remote_command ---

    #[test]
    fn build_remote_command_without_cwd() {
        assert_eq!(build_remote_command("ls -la", None), "ls -la");
    }

    #[test]
    fn build_remote_command_with_cwd_prefixes_cd() {
        assert_eq!(
            build_remote_command("ls -la", Some("/var/log")),
            "cd /var/log && ls -la"
        );
    }

    #[test]
    fn build_remote_command_ignores_blank_cwd() {
        assert_eq!(build_remote_command("pwd", Some("   ")), "pwd");
        assert_eq!(build_remote_command("pwd", Some("")), "pwd");
    }

    // --- cap_output ---

    #[test]
    fn cap_output_short_string_unchanged() {
        let (out, trunc) = cap_output("hello", 100);
        assert_eq!(out, "hello");
        assert!(!trunc);
    }

    #[test]
    fn cap_output_truncates_long_string() {
        let big = "x".repeat(500);
        let (out, trunc) = cap_output(&big, 100);
        assert!(trunc);
        assert!(out.starts_with(&"x".repeat(100)));
        assert!(out.contains("output truncated"));
    }

    #[test]
    fn cap_output_exact_cap_not_truncated() {
        let s = "y".repeat(100);
        let (out, trunc) = cap_output(&s, 100);
        assert!(!trunc);
        assert_eq!(out, s);
    }

    #[test]
    fn cap_output_respects_utf8_boundary() {
        // 多字节字符:cap 落在字符中间时,回退到边界,结果仍是合法 UTF-8。
        let s = "中".repeat(100); // 每个 '中' 占 3 字节
        let (out, trunc) = cap_output(&s, 100); // 100 不是 3 的倍数
        assert!(trunc);
        // 结果可被正常当作 &str 使用即证明是合法 UTF-8
        assert!(out.chars().take_while(|&c| c == '中').count() <= 34);
    }

    // --- format_audit_line ---

    #[test]
    fn format_audit_line_basic() {
        let line = format_audit_line("2026-05-18T12:00:00Z", "prod", "ls -la", Some(0));
        assert!(line.starts_with("2026-05-18T12:00:00Z\t"));
        assert!(line.contains("conn=prod"));
        assert!(line.contains("exit=0"));
        assert!(line.contains("cmd=ls -la"));
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn format_audit_line_timeout_has_no_exit_code() {
        let line = format_audit_line("2026-05-18T12:00:00Z", "prod", "sleep 999", None);
        assert!(line.contains("exit=timeout"));
    }

    #[test]
    fn format_audit_line_collapses_multiline_command() {
        let line = format_audit_line("t", "c", "echo a\necho b\r\necho c", Some(0));
        // 命令里的换行被替成空格 —— 一次执行只占一行
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }

    // --- utc_timestamp ---

    #[test]
    fn utc_timestamp_has_expected_shape() {
        let ts = utc_timestamp();
        // 形如 YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "got: {ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        // 年份在合理范围
        let year: i64 = ts[..4].parse().unwrap();
        assert!((2025..2100).contains(&year));
    }

    // --- finalize_outcome ---

    #[test]
    fn finalize_outcome_passes_through_normal_exec() {
        let outcome = ChannelOutcome {
            stdout: b"hi\n".to_vec(),
            stderr: b"warn\n".to_vec(),
            exit_code: Some(0),
        };
        let r = finalize_outcome(outcome, false);
        assert_eq!(r.stdout, "hi\n");
        assert_eq!(r.stderr, "warn\n");
        assert_eq!(r.exit_code, Some(0));
        assert!(!r.truncated);
        assert!(!r.timed_out);
    }

    #[test]
    fn finalize_outcome_marks_truncated_when_stdout_overflows() {
        let outcome = ChannelOutcome {
            stdout: vec![b'a'; OUTPUT_CAP_BYTES + 10],
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        let r = finalize_outcome(outcome, false);
        assert!(r.truncated);
        // 截断后内容长度受 cap + truncation marker 约束
        assert!(r.stdout.contains("output truncated"));
    }

    #[test]
    fn finalize_outcome_timed_out_drops_exit_code() {
        // 超时路径:即便 outcome 携带 exit_code,最终结果也必须 None。
        let outcome = ChannelOutcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        let r = finalize_outcome(outcome, true);
        assert!(r.timed_out);
        assert_eq!(r.exit_code, None);
    }
}
