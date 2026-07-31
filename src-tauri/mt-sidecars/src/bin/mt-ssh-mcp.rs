//! mt-ssh-mcp —— mini-term 的 SSH MCP server sidecar(stdio 传输)。
//!
//! 这是一个独立的瘦二进制(对标 `miniterm-hook`),用官方 `rmcp` crate 跑
//! 一个 stdio MCP server,把 mini-term 已保存的 SSH 连接暴露成 MCP 工具,
//! 供运行在 mini-term 终端里的 AI agent(Claude Code / Codex)调用。
//!
//! 自 ssh-cli-skill 迁移(spec §3)起,本文件只是 **rmcp 薄适配层**:工具注册、
//! 入参 schema、返回 JSON 打包在这里;连接查找、会话池编排(evict+retry+cooldown)、
//! 审计日志、传输护栏等业务全部在 `mt_sidecars::ssh_service` —— 与 `mt-ssh-cli`
//! (CLI/daemon)共享同一套编排与审计,对外行为与迁移前逐项一致。
//!
//! stdio 铁律:进程的 **stdout 只能输出 MCP 协议 JSON-RPC 消息**;任何日志 /
//! 调试输出一律走 stderr,否则会破坏 JSON-RPC 帧、导致客户端判定 server 挂掉。
//! exec 收集到的远程输出是**工具结果数据**,必须放进返回值序列化,
//! 绝不能透传到本进程 stdout。

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Serialize;
use std::sync::Arc;

use mt_sidecars::ssh_service::{self, ServiceError, StreamKind, TransferDirection};
use mt_ssh::pool::SshPool;

/// 把 service 层错误映射为 MCP 错误；取消态只供 daemon 使用，MCP 若意外
/// 收到则按内部错误处理。
fn to_mcp_error(e: ServiceError) -> McpError {
    match e {
        ServiceError::InvalidParams(m) => McpError::invalid_params(m, None),
        ServiceError::Internal(m) => McpError::internal_error(m, None),
        ServiceError::Cancelled => McpError::internal_error("ssh exec cancelled", None),
    }
}

// ---------------------------------------------------------------------------
// ssh_list_connections
// ---------------------------------------------------------------------------

/// `ssh_list_connections` 工具的入参 —— 无参数。
///
/// rmcp 的 `#[tool]` 仍要求入参结构体派生 `Deserialize + JsonSchema`,
/// 这里用一个空结构体表示「无入参」。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListConnectionsArgs {}

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

/// 把回调收集到的输出缓冲 + service 层结果打包成工具返回结构。
///
/// 与迁移前 `finalize_outcome` 逐项等价:
/// - 超时路径:旧实现的 timeout 会取消收集 future、拿不到部分输出,返回空
///   stdout/stderr —— 这里对应地**丢弃**已收集缓冲,保持返回体一致;
/// - 正常路径:lossy UTF-8 + 100KB cap + truncated 标记。
fn finalize_collected(
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    outcome: &ssh_service::ExecOutcome,
) -> SshExecResult {
    let (stdout_buf, stderr_buf) = if outcome.timed_out {
        (Vec::new(), Vec::new())
    } else {
        (stdout_buf, stderr_buf)
    };
    let (stdout, out_trunc) = ssh_service::cap_output(
        &String::from_utf8_lossy(&stdout_buf),
        ssh_service::OUTPUT_CAP_BYTES,
    );
    let (stderr, err_trunc) = ssh_service::cap_output(
        &String::from_utf8_lossy(&stderr_buf),
        ssh_service::OUTPUT_CAP_BYTES,
    );
    SshExecResult {
        stdout,
        stderr,
        exit_code: outcome.exit_code,
        truncated: out_trunc || err_trunc,
        timed_out: outcome.timed_out,
    }
}

// ---------------------------------------------------------------------------
// ssh_upload / ssh_download —— 入参 / 出参
// ---------------------------------------------------------------------------

/// `ssh_upload` 工具的入参。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SshUploadArgs {
    /// 目标连接:已保存 SSH 连接的 name(也接受 id)。
    #[schemars(description = "Name of a saved SSH connection (its id is also accepted).")]
    connection: String,
    /// 本地源文件路径(在运行 mini-term 的这台机器上)。
    #[schemars(
        description = "Path to the LOCAL source file (on the machine running mini-term) to upload."
    )]
    local_path: String,
    /// 远程目标文件路径(在 SSH 服务器上)。
    #[schemars(
        description = "Destination file path on the REMOTE host where the file will be written."
    )]
    remote_path: String,
    /// 可选:超时秒数。缺省 300。
    #[schemars(description = "Optional timeout in seconds for the transfer. Defaults to 300.")]
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// `ssh_download` 工具的入参。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SshDownloadArgs {
    /// 目标连接:已保存 SSH 连接的 name(也接受 id)。
    #[schemars(description = "Name of a saved SSH connection (its id is also accepted).")]
    connection: String,
    /// 远程源文件路径(在 SSH 服务器上)。
    #[schemars(description = "Path to the REMOTE source file (on the SSH host) to download.")]
    remote_path: String,
    /// 本地目标文件路径(在运行 mini-term 的这台机器上,文件落盘到此处)。
    #[schemars(
        description = "Destination file path on the LOCAL machine (running mini-term) where the file is saved to disk."
    )]
    local_path: String,
    /// 可选:超时秒数。缺省 300。
    #[schemars(description = "Optional timeout in seconds for the transfer. Defaults to 300.")]
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// `ssh_upload` / `ssh_download` 的执行结果,序列化为工具返回的 JSON。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SshTransferResult {
    /// 传输方向:`upload` 或 `download`。
    direction: String,
    /// 实际传输的字节数。
    bytes: u64,
    /// 远程文件路径。
    remote_path: String,
    /// 本地文件路径。
    local_path: String,
    /// 是否成功(始终 true —— 失败走 McpError 而非此结构)。
    success: bool,
}

// ---------------------------------------------------------------------------
// 进程参数
// ---------------------------------------------------------------------------

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
        // 文件缺失/解析失败时为空 Vec。投影不含敏感字段。
        let views = ssh_service::list_connections(ssh_service::ConnectionScope::LegacyProject(
            self.project_id.as_deref(),
        ))
        .map_err(to_mcp_error)?;

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

        // 业务编排(连接查找/池/retry/cooldown/审计)全在 service 层;
        // MCP 侧用回调把流式输出收集进缓冲,最后 cap_output 打包 JSON。
        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut stderr_buf: Vec<u8> = Vec::new();
        let cancellation = ssh_service::ExecCancellation::new();
        let outcome = ssh_service::exec(
            &self.pool,
            ssh_service::ExecRequest {
                scope: ssh_service::ConnectionScope::LegacyProject(self.project_id.as_deref()),
                connection: &connection,
                command: &command,
                cwd: cwd.as_deref(),
                timeout_secs,
            },
            &cancellation,
            |kind, data| match kind {
                StreamKind::Stdout => stdout_buf.extend_from_slice(data),
                StreamKind::Stderr => stderr_buf.extend_from_slice(data),
            },
        )
        .await
        .map_err(to_mcp_error)?;

        let exec_result = finalize_collected(stdout_buf, stderr_buf, &outcome);

        let json = serde_json::to_string(&exec_result).map_err(|e| {
            McpError::internal_error(format!("failed to serialize ssh_exec result: {e}"), None)
        })?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// 通过已保存的 SSH 连接,把**本地**文件上传到**远程**主机(SFTP)。
    #[tool(
        description = "Upload a LOCAL file to a REMOTE host over SFTP, using a saved SSH connection. \
        Provide the connection's name (or id), local_path (source file on the machine running mini-term) \
        and remote_path (destination on the SSH host). Streams the file in chunks (constant memory, \
        handles large files). Optionally set timeout_secs (default 300). Returns the byte count. \
        Use this instead of base64-echoing files through ssh_exec. \
        Only connections this project is associated with can be used."
    )]
    async fn ssh_upload(
        &self,
        Parameters(args): Parameters<SshUploadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let SshUploadArgs {
            connection,
            local_path,
            remote_path,
            timeout_secs,
        } = args;
        self.run_transfer(
            TransferDirection::Upload,
            &connection,
            &local_path,
            &remote_path,
            timeout_secs,
        )
        .await
    }

    /// 通过已保存的 SSH 连接,把**远程**文件下载并落盘到**本地**路径(SFTP)。
    #[tool(
        description = "Download a REMOTE file to the LOCAL machine over SFTP, using a saved SSH connection. \
        Provide the connection's name (or id), remote_path (source file on the SSH host) and \
        local_path (destination on the machine running mini-term; the file is written to disk there). \
        The content is NOT returned in the response — read the saved local file afterwards if you need it. \
        Streams the file in chunks (constant memory, handles large files). Optionally set timeout_secs \
        (default 300). Returns the byte count. \
        Only connections this project is associated with can be used."
    )]
    async fn ssh_download(
        &self,
        Parameters(args): Parameters<SshDownloadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let SshDownloadArgs {
            connection,
            remote_path,
            local_path,
            timeout_secs,
        } = args;
        self.run_transfer(
            TransferDirection::Download,
            &connection,
            &local_path,
            &remote_path,
            timeout_secs,
        )
        .await
    }

    /// `ssh_upload` / `ssh_download` 的共享打包:业务编排(护栏 → 查连接 →
    /// acquire → 超时 + evict+retry → 审计)在 service 层,这里只做错误映射
    /// 与结构化返回。
    async fn run_transfer(
        &self,
        direction: TransferDirection,
        connection: &str,
        local_path: &str,
        remote_path: &str,
        timeout_secs: Option<u64>,
    ) -> Result<CallToolResult, McpError> {
        let bytes = ssh_service::transfer(
            &self.pool,
            direction,
            ssh_service::ConnectionScope::LegacyProject(self.project_id.as_deref()),
            connection,
            local_path,
            remote_path,
            timeout_secs,
        )
        .await
        .map_err(to_mcp_error)?;

        let result = SshTransferResult {
            direction: direction.as_str().to_string(),
            bytes,
            remote_path: remote_path.to_string(),
            local_path: local_path.to_string(),
            success: true,
        };
        let json = serde_json::to_string(&result).map_err(|e| {
            McpError::internal_error(format!("failed to serialize transfer result: {e}"), None)
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
    use mt_sidecars::ssh_service::ExecOutcome;

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

    // --- finalize_collected:回调收集 → MCP 返回结构(等价旧 finalize_outcome) ---

    #[test]
    fn finalize_collected_passes_through_normal_exec() {
        let outcome = ExecOutcome {
            exit_code: Some(0),
            timed_out: false,
        };
        let r = finalize_collected(b"hi\n".to_vec(), b"warn\n".to_vec(), &outcome);
        assert_eq!(r.stdout, "hi\n");
        assert_eq!(r.stderr, "warn\n");
        assert_eq!(r.exit_code, Some(0));
        assert!(!r.truncated);
        assert!(!r.timed_out);
    }

    #[test]
    fn finalize_collected_marks_truncated_when_stdout_overflows() {
        let outcome = ExecOutcome {
            exit_code: Some(0),
            timed_out: false,
        };
        let r = finalize_collected(
            vec![b'a'; ssh_service::OUTPUT_CAP_BYTES + 10],
            Vec::new(),
            &outcome,
        );
        assert!(r.truncated);
        // 截断后内容长度受 cap + truncation marker 约束
        assert!(r.stdout.contains("output truncated"));
    }

    #[test]
    fn finalize_collected_timed_out_drops_output_and_exit_code() {
        // 超时路径:旧实现拿不到部分输出(future 被取消),返回空 stdout/stderr;
        // 回调化后已收集的缓冲也要对应丢弃,保持返回体逐字节一致。
        let outcome = ExecOutcome {
            exit_code: None,
            timed_out: true,
        };
        let r = finalize_collected(b"partial".to_vec(), b"errs".to_vec(), &outcome);
        assert!(r.timed_out);
        assert_eq!(r.exit_code, None);
        assert_eq!(r.stdout, "");
        assert_eq!(r.stderr, "");
        assert!(!r.truncated);
    }

    // --- SshTransferResult 序列化(camelCase) -----------------------------

    #[test]
    fn ssh_transfer_result_serializes_camel_case() {
        let r = SshTransferResult {
            direction: "upload".into(),
            bytes: 42,
            remote_path: "/r".into(),
            local_path: "/l".into(),
            success: true,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"direction\":\"upload\""));
        assert!(json.contains("\"bytes\":42"));
        assert!(json.contains("\"remotePath\":\"/r\""));
        assert!(json.contains("\"localPath\":\"/l\""));
        assert!(json.contains("\"success\":true"));
    }

    // --- 入参解析(serde Deserialize + camelCase via #[serde] default) -----

    #[test]
    fn ssh_upload_args_parse_with_and_without_timeout() {
        // rmcp 用 JsonSchema 的字段名(snake_case 字段),JSON 入参用同名键。
        let with: SshUploadArgs = serde_json::from_str(
            r#"{"connection":"prod","local_path":"/a","remote_path":"/b","timeout_secs":120}"#,
        )
        .unwrap();
        assert_eq!(with.connection, "prod");
        assert_eq!(with.local_path, "/a");
        assert_eq!(with.remote_path, "/b");
        assert_eq!(with.timeout_secs, Some(120));

        let without: SshUploadArgs =
            serde_json::from_str(r#"{"connection":"p","local_path":"/a","remote_path":"/b"}"#)
                .unwrap();
        assert_eq!(without.timeout_secs, None);
    }

    #[test]
    fn ssh_download_args_parse() {
        let a: SshDownloadArgs =
            serde_json::from_str(r#"{"connection":"prod","remote_path":"/r","local_path":"/l"}"#)
                .unwrap();
        assert_eq!(a.remote_path, "/r");
        assert_eq!(a.local_path, "/l");
        assert_eq!(a.timeout_secs, None);
    }
}
