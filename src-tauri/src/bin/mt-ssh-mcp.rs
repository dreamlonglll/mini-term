//! mt-ssh-mcp —— mini-term 的 SSH MCP server sidecar(stdio 传输)。
//!
//! 这是一个独立的瘦二进制(对标 `miniterm-hook`),用官方 `rmcp` crate 跑
//! 一个 stdio MCP server,把 mini-term 已保存的 SSH 连接暴露成 MCP 工具,
//! 供运行在 mini-term 终端里的 AI agent(Claude Code / Codex)调用。
//!
//! 本文件覆盖 PR2:server 骨架 + `ssh_list_connections` + `ssh_exec`。
//!
//! stdio 铁律:进程的 **stdout 只能输出 MCP 协议 JSON-RPC 消息**;任何日志 /
//! 调试输出一律走 stderr,否则会破坏 JSON-RPC 帧、导致客户端判定 server 挂掉。
//! `ssh_exec` 起的 ssh 子进程的输出是**工具结果数据**,必须收集进返回值,
//! 绝不能透传到本进程 stdout。

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Serialize;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

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
    /// 是否对 agent 可见。能出现在本列表里的恒为 true。
    agent_accessible: bool,
}

/// 把原始连接列表过滤 + 投影成对 agent 可见的视图。
///
/// 安全核心:只保留 `agent_accessible == true` 的连接,并映射到不含
/// `password` / `identityFile` 的 `SshConnectionView`。抽成纯函数便于单测。
fn visible_connections(conns: Vec<mt_core::SshConnection>) -> Vec<SshConnectionView> {
    conns
        .into_iter()
        .filter(|c| c.agent_accessible)
        .map(|c| SshConnectionView {
            id: c.id,
            name: c.name,
            host: c.host,
            port: c.port,
            user: c.user,
            group: c.group,
            agent_accessible: c.agent_accessible,
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
    /// 可选:超时秒数,超时强制 kill ssh 进程。缺省 60。
    #[schemars(
        description = "Optional timeout in seconds; the ssh process is killed if it exceeds this. Defaults to 60."
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
    /// 远程命令的标准错误(可能被封顶截断;密码型 PTY 路径下恒为空)。
    stderr: String,
    /// 退出码。超时被 kill 时为 None。
    exit_code: Option<i32>,
    /// stdout 或 stderr 是否因超出封顶被截断。
    truncated: bool,
    /// 是否因超时被强制终止。
    timed_out: bool,
}

// ---------------------------------------------------------------------------
// ssh_exec —— 纯逻辑(可单测)
// ---------------------------------------------------------------------------

/// 在连接列表里按 name 或 id 查找连接,并校验 `agent_accessible`。
///
/// 匹配规则(先 name 后 id,均大小写敏感,与 SshModal 行为一致):
/// - name 精确命中多条 → 歧义错误;
/// - name 无命中再按 id 精确命中;
/// - 命中的连接 `agent_accessible == false` → 未授权错误。
///
/// 所有错误信息**不含密码**(只回显用户给的标识符)。
fn find_connection(
    conns: &[mt_core::SshConnection],
    selector: &str,
) -> Result<mt_core::SshConnection, String> {
    let by_name: Vec<&mt_core::SshConnection> =
        conns.iter().filter(|c| c.name == selector).collect();
    let matched = match by_name.len() {
        1 => by_name[0],
        n if n > 1 => {
            return Err(format!(
                "SSH connection name '{selector}' is ambiguous: {n} connections share this name. \
                Use the connection id instead."
            ));
        }
        _ => {
            // name 无命中 → 退而按 id 精确匹配
            match conns.iter().find(|c| c.id == selector) {
                Some(c) => c,
                None => {
                    return Err(format!(
                        "No SSH connection found matching '{selector}'. \
                        Call ssh_list_connections to see available connections."
                    ));
                }
            }
        }
    };

    if !matched.agent_accessible {
        return Err(format!(
            "SSH connection '{selector}' is not marked accessible to agents. \
            Enable it in mini-term's SSH manager first."
        ));
    }
    Ok(matched.clone())
}

/// 拼远程要执行的命令:`cwd` 非空时前缀 `cd <cwd> && `。
fn build_remote_command(command: &str, cwd: Option<&str>) -> String {
    match cwd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(dir) => format!("cd {dir} && {command}"),
        None => command.to_string(),
    }
}

/// 拼 ssh 命令行参数(不含 "ssh" 本身,作为 program 单独传给 spawn)。
///
/// 形如:`[-p <port>] [-i <key>] [-J <jump>] -o StrictHostKeyChecking=accept-new
/// [-o BatchMode=yes] <user>@<host> <remote_command>`。
///
/// `identity_path` 为已解析的私钥路径(可能是收紧权限的临时副本);路径里的
/// 反斜杠转正斜杠 —— Windows OpenSSH 接受正斜杠,且作为独立 argv 元素传递
/// 不经 shell,不会有转义问题(转正斜杠仅为与主程序 connectSsh 行为一致)。
///
/// `batch_mode`:`true` 时加 `-o BatchMode=yes` 禁掉一切交互提示(密钥 /
/// agent 路径用,认证失败时不会挂起等 stdin);**密码型连接必须传 `false`**
/// —— `BatchMode=yes` 会连带禁掉密码认证,PTY autofill 就再也喂不进密码。
fn build_ssh_args(
    conn: &mt_core::SshConnection,
    identity_path: Option<&str>,
    remote_command: &str,
    batch_mode: bool,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if conn.port != 0 && conn.port != 22 {
        args.push("-p".into());
        args.push(conn.port.to_string());
    }
    if let Some(key) = identity_path.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("-i".into());
        args.push(key.replace('\\', "/"));
    }
    if let Some(jump) = conn.proxy_jump.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        args.push("-J".into());
        args.push(jump.to_string());
    }
    // host key:首见自动接受并记入 known_hosts,已变更则仍拒绝。
    // 非交互场景必需,否则 ssh 会卡在 host-key 确认提示。
    args.push("-o".into());
    args.push("StrictHostKeyChecking=accept-new".into());
    if batch_mode {
        // 密钥 / agent 路径:禁掉交互密码提示,认证失败立即返回而非挂起。
        args.push("-o".into());
        args.push("BatchMode=yes".into());
    }
    args.push(format!("{}@{}", conn.user, conn.host));
    args.push(remote_command.to_string());
    args
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
// ssh_exec —— 执行(阻塞 I/O,在 spawn_blocking 里跑)
// ---------------------------------------------------------------------------

/// 非密码路径:用 `std::process::Command` 起 ssh,管道分离 stdout/stderr。
///
/// 在独立线程里等待进程结束以实现超时:超时则 kill。返回 `SshExecResult`。
fn run_ssh_piped(args: &[String], timeout: Duration) -> Result<SshExecResult, String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new("ssh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn ssh: {e}"))?;

    // stdout/stderr 各开一个线程读到底,避免管道写满导致子进程阻塞。
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stderr_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    // 轮询 try_wait 实现超时。
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let exit_code: Option<i32> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("error while waiting for ssh: {e}")),
        }
    };

    // 进程已结束(或被 kill),管道走到 EOF,读线程随即返回。
    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_bytes = stderr_handle.join().unwrap_or_default();
    let (stdout, out_trunc) =
        cap_output(&String::from_utf8_lossy(&stdout_bytes), OUTPUT_CAP_BYTES);
    let (stderr, err_trunc) =
        cap_output(&String::from_utf8_lossy(&stderr_bytes), OUTPUT_CAP_BYTES);

    Ok(SshExecResult {
        stdout,
        stderr,
        exit_code,
        truncated: out_trunc || err_trunc,
        timed_out,
    })
}

/// 密码路径:用 `portable-pty` 起 ssh,扫描 PTY 输出做密码 autofill。
///
/// PTY 下 stdout/stderr 合并为一路,全部计入 `stdout`,`stderr` 留空。
/// 每会话只灌一次密码;命中 `AuthFailed`(密码错误)后停止灌密码,
/// 避免连灌错误密码。
///
/// 实现要点:扫描 + 回写都在**读线程**里做 —— 读线程独占 `reader`,把
/// `writer` 一并 move 进去就能在命中密码提示时立即回写,不需要跨线程共享
/// PTY 句柄。主线程只负责轮询子进程与超时 kill。
fn run_ssh_pty(args: &[String], password: &str, timeout: Duration) -> Result<SshExecResult, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("failed to open pty: {e}"))?;

    let mut cmd = CommandBuilder::new("ssh");
    for a in args {
        cmd.arg(a);
    }
    // UTF-8 环境,与主程序 create_pty 一致,避免远程输出乱码。
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_CTYPE", "C.UTF-8");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("failed to spawn ssh in pty: {e}"))?;
    // slave 句柄留着会让 master 读不到 EOF,spawn 后立即丢弃。
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("failed to clone pty reader: {e}"))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("failed to take pty writer: {e}"))?;

    // 读线程:收集 PTY 全部输出,并在命中密码提示时回写密码。
    // 进程退出后 master 走到 EOF,read 返回 0,线程结束。
    let password_owned = password.to_string();
    let read_handle = std::thread::spawn(move || {
        let mut collected: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        // 每会话只灌一次密码:命中并写入后置 true,后续不再写。
        let mut password_sent = false;
        // 命中 AuthFailed 后彻底停止灌密码。
        let mut auth_failed = false;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    collected.extend_from_slice(&buf[..n]);
                    if password_sent || auth_failed {
                        continue;
                    }
                    // 扫描尾部输出判定密码提示。对累积 buffer 取尾段足够:
                    // 提示串很短,strip ANSI 后判尾即可。
                    let tail = strip_recent_text(&collected);
                    match mt_core::scan_ssh_prompt(&tail) {
                        mt_core::SshPromptScan::Password => {
                            let mut line = password_owned.clone();
                            line.push('\r');
                            if writer.write_all(line.as_bytes()).is_ok() {
                                let _ = writer.flush();
                            }
                            password_sent = true;
                        }
                        mt_core::SshPromptScan::AuthFailed => {
                            auth_failed = true;
                        }
                        mt_core::SshPromptScan::None => {}
                    }
                }
                Err(_) => break,
            }
        }
        collected
    });

    // 主线程:轮询子进程,超时强制 kill。
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(format!("error while waiting for ssh in pty: {e}"));
            }
        }
    }

    let exit_code = child.wait().ok().map(|s| s.exit_code() as i32);
    // 进程已结束,master 走到 EOF,读线程随即返回。
    let collected = read_handle.join().unwrap_or_default();
    let (stdout, truncated) =
        cap_output(&String::from_utf8_lossy(&collected), OUTPUT_CAP_BYTES);

    Ok(SshExecResult {
        stdout,
        stderr: String::new(),
        exit_code,
        truncated,
        timed_out,
    })
}

/// 从累积的 PTY 字节里取尾段并 strip ANSI,用于密码提示扫描。
///
/// 密码 / 认证失败提示串都很短,只需看输出末尾;取尾段也避免对越来越大的
/// buffer 反复全量 strip。在 UTF-8 字符边界处切割以免产生非法 UTF-8。
fn strip_recent_text(collected: &[u8]) -> String {
    /// 尾段窗口大小:足以覆盖最长的提示行。
    const TAIL_WINDOW: usize = 512;
    let start = collected.len().saturating_sub(TAIL_WINDOW);
    let tail = &collected[start..];
    mt_core::strip_ansi_codes(&String::from_utf8_lossy(tail))
}

#[derive(Clone)]
struct SshMcp;

#[tool_router]
impl SshMcp {
    /// 列出对 agent 可见的、已保存的 SSH 连接。
    ///
    /// 只返回 `agentAccessible == true` 的连接,且**不含任何密码字段**。
    #[tool(
        description = "List the saved SSH connections that are marked accessible to AI agents. \
        Returns connection metadata (id, name, host, port, user, group) with NO passwords. \
        Use a connection's name (or id) with ssh_exec to run commands on that host."
    )]
    async fn ssh_list_connections(
        &self,
        Parameters(ListConnectionsArgs {}): Parameters<ListConnectionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        // 读全局 config.json 的 sshConnections;文件缺失/解析失败时为空 Vec。
        let views = visible_connections(mt_core::read_ssh_connections());

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
        Only connections marked accessible to agents can be used."
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

        // 1. 查连接 + 校验授权。错误不含密码。
        let conn = find_connection(&mt_core::read_ssh_connections(), &connection)
            .map_err(|e| McpError::invalid_params(e, None))?;

        // 2. 拼远程命令(可选 cwd 前缀)。
        let remote_command = build_remote_command(&command, cwd.as_deref());
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));

        // 3. 私钥:先准备权限收紧的临时副本。准备失败回退原始路径让 ssh 自报错。
        let identity_path: Option<String> = conn
            .identity_file
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|orig| match mt_core::prepare_ssh_key(orig) {
                Ok(tmp) => tmp,
                Err(e) => {
                    eprintln!("[mt-ssh-mcp] prepare_ssh_key failed, using original path: {e}");
                    orig.to_string()
                }
            });

        // 密码型连接走 PTY autofill,**不能**加 BatchMode(否则禁掉密码认证);
        // 密钥 / agent 路径加 BatchMode,认证失败立即返回不挂起。
        let password = conn.password.clone();
        let use_password = password.as_deref().map(|p| !p.is_empty()).unwrap_or(false);
        let ssh_args = build_ssh_args(
            &conn,
            identity_path.as_deref(),
            &remote_command,
            /* batch_mode = */ !use_password,
        );
        let conn_name_for_audit = conn.name.clone();

        // 4. portable-pty / std::process::Command 都是阻塞 API —— 丢进
        //    spawn_blocking,不阻塞 tokio runtime。
        let exec_result = tokio::task::spawn_blocking(move || match password {
            Some(pw) if !pw.is_empty() => run_ssh_pty(&ssh_args, &pw, timeout),
            _ => run_ssh_piped(&ssh_args, timeout),
        })
        .await
        .map_err(|e| McpError::internal_error(format!("ssh exec task panicked: {e}"), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

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
    // 日志走 stderr —— stdout 留给 MCP 协议 JSON。失败也只往 stderr 写。
    eprintln!("[mt-ssh-mcp] starting stdio MCP server");

    // 握手并注册工具;.serve() 绑定进程的 stdin/stdout 作为 stdio 传输。
    let service = SshMcp.serve(stdio()).await.inspect_err(|e| {
        eprintln!("[mt-ssh-mcp] failed to start server: {e}");
    })?;

    // 阻塞直到 stdin 关闭 / 客户端断开 —— 这是 sidecar 正常退出的信号。
    service.waiting().await?;

    eprintln!("[mt-ssh-mcp] client disconnected, exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str, accessible: bool, password: Option<&str>) -> mt_core::SshConnection {
        mt_core::SshConnection {
            id: id.into(),
            name: format!("conn-{id}"),
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            password: password.map(|s| s.into()),
            identity_file: Some("/home/u/.ssh/id_rsa".into()),
            proxy_jump: None,
            group: Some("内网".into()),
            agent_accessible: accessible,
        }
    }

    #[test]
    fn visible_connections_keeps_only_agent_accessible() {
        let conns = vec![
            conn("1", true, Some("secret1")),
            conn("2", false, Some("secret2")),
            conn("3", true, None),
        ];
        let views = visible_connections(conns);
        assert_eq!(views.len(), 2);
        let ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["1", "3"]);
        assert!(views.iter().all(|v| v.agent_accessible));
    }

    #[test]
    fn visible_connections_empty_when_none_accessible() {
        let conns = vec![conn("1", false, None), conn("2", false, None)];
        assert!(visible_connections(conns).is_empty());
    }

    #[test]
    fn serialized_view_never_leaks_password_or_identity_file() {
        // 安全验收:即便源连接有明文密码与私钥路径,序列化结果也绝不能含它们。
        let conns = vec![conn("1", true, Some("super-secret-password"))];
        let views = visible_connections(conns);
        let json = serde_json::to_string(&views).unwrap();
        assert!(!json.contains("super-secret-password"));
        assert!(!json.to_lowercase().contains("password"));
        assert!(!json.contains("identityFile"));
        assert!(!json.contains("id_rsa"));
        // 但应保留展示字段
        assert!(json.contains("\"host\":\"10.0.0.5\""));
        assert!(json.contains("\"agentAccessible\":true"));
    }

    // --- find_connection ---

    #[test]
    fn find_connection_matches_by_name() {
        let conns = vec![conn("1", true, None)];
        let found = find_connection(&conns, "conn-1").unwrap();
        assert_eq!(found.id, "1");
    }

    #[test]
    fn find_connection_matches_by_id_when_name_misses() {
        let conns = vec![conn("abc", true, None)];
        let found = find_connection(&conns, "abc").unwrap();
        assert_eq!(found.id, "abc");
    }

    #[test]
    fn find_connection_errors_when_not_found() {
        let conns = vec![conn("1", true, None)];
        let err = find_connection(&conns, "does-not-exist").unwrap_err();
        assert!(err.contains("No SSH connection found"));
    }

    #[test]
    fn find_connection_errors_when_not_accessible() {
        let conns = vec![conn("1", false, None)];
        let err = find_connection(&conns, "conn-1").unwrap_err();
        assert!(err.contains("not marked accessible"));
    }

    #[test]
    fn find_connection_errors_on_ambiguous_name() {
        let mut a = conn("1", true, None);
        let mut b = conn("2", true, None);
        a.name = "dup".into();
        b.name = "dup".into();
        let err = find_connection(&[a, b], "dup").unwrap_err();
        assert!(err.contains("ambiguous"));
    }

    #[test]
    fn find_connection_error_never_contains_password() {
        // 安全:连接未授权时,错误信息不能泄漏该连接的明文密码。
        let conns = vec![conn("1", false, Some("topsecretpw"))];
        let err = find_connection(&conns, "conn-1").unwrap_err();
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

    // --- build_ssh_args ---

    #[test]
    fn build_ssh_args_minimal() {
        let c = conn("1", true, None);
        let args = build_ssh_args(&c, None, "uptime", true);
        // 默认端口 22 不出现 -p;无私钥不出现 -i;无跳板不出现 -J
        assert!(!args.contains(&"-p".to_string()));
        assert!(!args.contains(&"-i".to_string()));
        assert!(!args.contains(&"-J".to_string()));
        assert!(args.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        assert!(args.contains(&"root@10.0.0.5".to_string()));
        assert_eq!(args.last().unwrap(), "uptime");
    }

    #[test]
    fn build_ssh_args_includes_non_default_port() {
        let mut c = conn("1", true, None);
        c.port = 2222;
        let args = build_ssh_args(&c, None, "uptime", true);
        let pos = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[pos + 1], "2222");
    }

    #[test]
    fn build_ssh_args_includes_identity_file_with_forward_slashes() {
        let c = conn("1", true, None);
        let args = build_ssh_args(&c, Some(r"C:\tmp\keys\abc.key"), "uptime", true);
        let pos = args.iter().position(|a| a == "-i").unwrap();
        // Windows 路径反斜杠转正斜杠
        assert_eq!(args[pos + 1], "C:/tmp/keys/abc.key");
    }

    #[test]
    fn build_ssh_args_includes_proxy_jump() {
        let mut c = conn("1", true, None);
        c.proxy_jump = Some("user@bastion".into());
        let args = build_ssh_args(&c, None, "uptime", true);
        let pos = args.iter().position(|a| a == "-J").unwrap();
        assert_eq!(args[pos + 1], "user@bastion");
    }

    #[test]
    fn build_ssh_args_target_precedes_remote_command() {
        let c = conn("1", true, None);
        let args = build_ssh_args(&c, None, "cd /tmp && ls", true);
        let target_pos = args.iter().position(|a| a == "root@10.0.0.5").unwrap();
        // user@host 必须是倒数第二个,remote command 是最后一个
        assert_eq!(target_pos, args.len() - 2);
        assert_eq!(args.last().unwrap(), "cd /tmp && ls");
    }

    #[test]
    fn build_ssh_args_batch_mode_toggles_batchmode_option() {
        let c = conn("1", true, None);
        // batch_mode = true(密钥/agent 路径):带 BatchMode=yes
        let with_batch = build_ssh_args(&c, None, "uptime", true);
        assert!(with_batch.contains(&"BatchMode=yes".to_string()));
        // batch_mode = false(密码型,PTY autofill):绝不能带 BatchMode,
        // 否则 ssh 会禁掉密码认证,autofill 喂不进密码。
        let without_batch = build_ssh_args(&c, None, "uptime", false);
        assert!(!without_batch.contains(&"BatchMode=yes".to_string()));
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
        assert!(year >= 2025 && year < 2100);
    }
}
