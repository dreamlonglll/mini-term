//! miniterm-hook CLI 小工具
//!
//! 极简二进制，被 Claude Code / Codex / Grok Build 的 hook 系统调用。
//! 功能：读 stdin JSON payload -> 读环境变量 -> POST 到 miniterm hook 服务器。
//!
//! 三家的调用约定不同，在这里抹平（见 `resolve_event_name` /
//! `alias_camel_case_fields` / `detect_agent`）：
//! - Claude/Codex：事件名走 argv，payload 是 snake_case；
//! - Grok：事件名走 `GROK_HOOK_EVENT`（snake_case，如 `pre_tool_use`），
//!   payload 是 camelCase（`sessionId` / `toolName` / …）。
//!
//! 依赖最小化：仅使用 serde_json + 标准库，不引入额外 HTTP 客户端。

use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

/// 从 stdin 读取的超时时间（毫秒）
const STDIN_TIMEOUT_MS: u64 = 400;

fn main() {
    // 1. 事件名：argv（Claude/Codex）优先，Grok 原生注册不带 argv
    let argv_event = std::env::args().nth(1).unwrap_or_default();
    let grok_session = std::env::var("GROK_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let is_grok = grok_session.is_some();

    // Grok 默认还会扫描 ~/.claude/settings.json 里的 hooks（Claude 兼容层），
    // 于是同一个 grok 事件可能来两趟：一趟走我们写进 ~/.grok/hooks/ 的原生条目
    // （无 argv），一趟走 Claude 条目（有 argv）。原生条目在场时丢掉兼容层那一趟，
    // 否则每个事件都要多跑一个进程，attention 类事件还会绕过去重双发。
    // 反之（用户只注册了 Claude、没注册 Grok）必须放行——那是唯一的来源。
    if is_grok && !argv_event.is_empty() && grok_native_hook_installed() {
        return;
    }

    let event_name = resolve_event_name(&argv_event);

    // 2. 从 stdin 读取 JSON payload（带超时）
    let stdin_payload = read_stdin_with_timeout();

    // 3. 读取环境变量
    let pty_id = std::env::var("MINITERM_PTY_ID").ok();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    // 4. 获取服务器端口
    let port = match get_server_port() {
        Some(p) => p,
        None => {
            // 无法获取端口，静默退出
            return;
        }
    };

    // 5. 构造 POST body
    let mut body = if let Some(ref payload) = stdin_payload {
        serde_json::from_str::<serde_json::Value>(payload).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Grok 的 payload 是 camelCase，hook server 只认 snake_case：不抹平的话
    // session_id / tool_name / notification_type 全读不到，pane 绑不到会话、
    // 权限黄灯也点不亮。Claude/Codex 的 payload 里没有这些 camel 键，是空操作。
    alias_camel_case_fields(&mut body);

    // 注入字段
    if let Some(ref pty_id_str) = pty_id {
        if let Ok(id) = pty_id_str.parse::<u32>() {
            body["pty_id"] = serde_json::json!(id);
        }
    }
    if !event_name.is_empty() {
        body["event"] = serde_json::json!(event_name);
    }
    if let Some(ref cwd_str) = cwd {
        // 仅在 payload 中没有 cwd 时注入
        if body.get("cwd").is_none() {
            body["cwd"] = serde_json::json!(cwd_str);
        }
    }
    // Grok 会话 id 的环境变量兜底：payload 缺字段时仍能精确绑定镜像
    if body.get("session_id").is_none() {
        if let Some(ref sid) = grok_session {
            body["session_id"] = serde_json::json!(sid);
        }
    }

    // 推断 agent 类型
    if body.get("agent").is_none() {
        body["agent"] = serde_json::json!(detect_agent(&body, is_grok));
    }

    let body_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());

    // 6. 发送 HTTP POST
    send_http_post(port, &body_str);
}

/// 来源 agent。
///
/// `GROK_SESSION_ID` 由 grok 的 hook runner 注入，且在其实现里**优先级最高**
/// （runner 注入的环境变量总是覆盖用户/插件的 extra_env），是可靠判据，必须先判：
/// grok 会读 Claude 的 settings.json，payload 形状也与 Claude 相近，靠形状推断
/// 会把 grok 事件标成 `claude-code`，镜像随后按 claude 去找会话文件，绑到同项目
/// Claude 的最新对话上（串台，与 CLAUDE.md 对 opencode/pi 的警告同源）。
fn detect_agent(body: &serde_json::Value, is_grok: bool) -> &'static str {
    if is_grok {
        return "grok";
    }
    // Codex 的 hook payload 固定携带 turn_id，Claude Code 没有该字段；
    // transcript_path 两者都有，不能用来区分。
    if body.get("turn_id").is_some() {
        "codex"
    } else {
        "claude-code"
    }
}

/// 事件名：argv 优先，其次 `GROK_HOOK_EVENT`（snake_case → PascalCase）。
///
/// Grok 的原生注册条目刻意不带 argv：Windows 上带空格的命令行会被 grok 丢给
/// shell 执行（git-bash / pwsh / powershell / cmd 由环境决定），四家 shell 的引号
/// 语义互斥，写不出一份通用命令文本；不含空格的可执行文件路径则直接 spawn，
/// 完全绕开 shell（详见 hook_registry::register_grok_hooks）。
fn resolve_event_name(argv_event: &str) -> String {
    if !argv_event.is_empty() {
        return argv_event.to_string();
    }
    match std::env::var("GROK_HOOK_EVENT") {
        Ok(e) if !e.is_empty() => pascal_case(&e),
        _ => String::new(),
    }
}

/// `pre_tool_use` → `PreToolUse`。grok 的事件名恰是我们注册用的 PascalCase 的
/// snake_case 形式，逐段首字母大写即可还原，不必硬编码映射表（新事件自动跟随）。
fn pascal_case(snake: &str) -> String {
    snake
        .split('_')
        .filter(|seg| !seg.is_empty())
        .map(|seg| {
            let mut chars = seg.chars();
            match chars.next() {
                Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// camelCase → snake_case 键别名（仅在目标键缺失时补），覆盖 hook server 实际
/// 读取的字段。`error` → `error_type` 额外要求值是字符串：grok 的 StopFailure
/// 用 `error` 承载分类字符串，而别家可能把结构化错误对象塞在同名键下。
fn alias_camel_case_fields(body: &mut serde_json::Value) {
    const ALIASES: &[(&str, &str)] = &[
        ("sessionId", "session_id"),
        ("toolName", "tool_name"),
        ("hookEventName", "hook_event_name"),
        ("notificationType", "notification_type"),
        ("errorType", "error_type"),
    ];
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for (camel, snake) in ALIASES {
        if obj.contains_key(*snake) {
            continue;
        }
        if let Some(v) = obj.get(*camel).cloned() {
            obj.insert(snake.to_string(), v);
        }
    }
    if !obj.contains_key("error_type") {
        if let Some(v @ serde_json::Value::String(_)) = obj.get("error").cloned() {
            obj.insert("error_type".to_string(), v);
        }
    }
}

/// mini-term 是否已把原生 hook 装进 grok 的 hooks 目录（`$GROK_HOME` 优先）
fn grok_native_hook_installed() -> bool {
    let home = match std::env::var("GROK_HOME") {
        Ok(h) if !h.is_empty() => std::path::PathBuf::from(h),
        _ => match dirs::home_dir() {
            Some(h) => h.join(".grok"),
            None => return false,
        },
    };
    home.join("hooks").join("miniterm.json").is_file()
}

/// 从 stdin 读取 JSON，带超时保护
fn read_stdin_with_timeout() -> Option<String> {
    // 使用线程实现超时读取
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        let _ = tx.send(input);
    });

    match rx.recv_timeout(Duration::from_millis(STDIN_TIMEOUT_MS)) {
        Ok(input) if !input.trim().is_empty() => Some(input),
        _ => None,
    }
}

/// 获取 hook 服务器端口
///
/// 优先从环境变量 MINITERM_HOOK_PORT 读取，然后从标准路径查找 hook-server.json
fn get_server_port() -> Option<u16> {
    // 优先从环境变量获取
    if let Ok(port_str) = std::env::var("MINITERM_HOOK_PORT") {
        if let Ok(port) = port_str.parse::<u16>() {
            return Some(port);
        }
    }

    // 从 hook-server.json 文件获取
    let port_file = get_port_file_path()?;
    let content = std::fs::read_to_string(port_file).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("port")?.as_u64().map(|p| p as u16)
}

/// 获取 hook-server.json 的平台特定路径
fn get_port_file_path() -> Option<std::path::PathBuf> {
    let app_id = "com.mini-term.app";

    #[cfg(target_os = "windows")]
    {
        // Windows: %APPDATA%/com.mini-term.app/hook-server.json
        std::env::var("APPDATA").ok().map(|appdata| {
            std::path::PathBuf::from(appdata)
                .join(app_id)
                .join("hook-server.json")
        })
    }

    #[cfg(target_os = "macos")]
    {
        // macOS: ~/Library/Application Support/com.mini-term.app/hook-server.json
        dirs::home_dir().map(|h| {
            h.join("Library")
                .join("Application Support")
                .join(app_id)
                .join("hook-server.json")
        })
    }

    #[cfg(target_os = "linux")]
    {
        // Linux: $XDG_DATA_HOME/com.mini-term.app/hook-server.json
        // 或 ~/.local/share/com.mini-term.app/hook-server.json
        let data_dir = std::env::var("XDG_DATA_HOME")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")));
        data_dir.map(|d| d.join(app_id).join("hook-server.json"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// 使用原始 HTTP 发送 POST 请求到本地 hook 服务器
///
/// 不等待响应，尽快退出以不阻塞 AI 工具
fn send_http_post(port: u16, body: &str) {
    let addr = format!("127.0.0.1:{}", port);

    // 连接超时 100ms
    let sock_addr = match addr.parse() {
        Ok(a) => a,
        Err(_) => return,
    };
    let stream = match TcpStream::connect_timeout(&sock_addr, Duration::from_millis(100)) {
        Ok(s) => s,
        Err(_) => return, // 连接失败静默退出
    };

    // 设置写超时
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));

    let request = format!(
        "POST /hook HTTP/1.1\r\n\
         Host: 127.0.0.1:{}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        port,
        body.len(),
        body
    );

    let mut stream = stream;
    let _ = stream.write_all(request.as_bytes());
    let _ = stream.flush();
    // 不读取响应，立即退出
}

#[cfg(test)]
mod tests {
    use super::*;

    /// grok 的全部事件名都必须能还原成注册用的 PascalCase，否则
    /// hook server 的 `map_event_to_status` 认不出来，状态徽章整条失效。
    #[test]
    fn grok_event_names_round_trip_to_pascal_case() {
        for (snake, pascal) in [
            ("session_start", "SessionStart"),
            ("session_end", "SessionEnd"),
            ("user_prompt_submit", "UserPromptSubmit"),
            ("pre_tool_use", "PreToolUse"),
            ("post_tool_use", "PostToolUse"),
            ("post_tool_use_failure", "PostToolUseFailure"),
            ("permission_denied", "PermissionDenied"),
            ("stop", "Stop"),
            ("stop_failure", "StopFailure"),
            ("notification", "Notification"),
            ("subagent_start", "SubagentStart"),
            ("subagent_stop", "SubagentStop"),
            ("pre_compact", "PreCompact"),
            ("post_compact", "PostCompact"),
        ] {
            assert_eq!(pascal_case(snake), pascal, "{snake} 还原错误");
        }
    }

    /// argv 有值时不看环境变量（Claude/Codex 路径保持原样）
    #[test]
    fn argv_event_takes_precedence() {
        assert_eq!(resolve_event_name("PreToolUse"), "PreToolUse");
    }

    /// 来源判定：grok 优先于形状推断。误判成 claude-code 会让镜像
    /// 绑到同项目 Claude 的最新会话文件上（串台）。
    #[test]
    fn grok_env_wins_over_payload_shape() {
        let claude_shaped = serde_json::json!({ "session_id": "s" });
        assert_eq!(detect_agent(&claude_shaped, true), "grok");
        assert_eq!(detect_agent(&claude_shaped, false), "claude-code");

        let codex_shaped = serde_json::json!({ "turn_id": "t" });
        assert_eq!(detect_agent(&codex_shaped, false), "codex");
        // turn_id 是 codex 的形状特征，但 grok 环境变量在场时不参与判定
        assert_eq!(detect_agent(&codex_shaped, true), "grok");
    }

    #[test]
    fn camel_case_fields_are_aliased_without_clobbering() {
        let mut body = serde_json::json!({
            "sessionId": "sid-1",
            "toolName": "run_terminal_command",
            "hookEventName": "pre_tool_use",
            "notificationType": "permission_prompt",
            "error": "rate_limit",
        });
        alias_camel_case_fields(&mut body);
        assert_eq!(body["session_id"], "sid-1");
        assert_eq!(body["tool_name"], "run_terminal_command");
        assert_eq!(body["hook_event_name"], "pre_tool_use");
        assert_eq!(body["notification_type"], "permission_prompt");
        assert_eq!(body["error_type"], "rate_limit");

        // 已有 snake_case 值不被 camel 版本覆盖（Claude/Codex 的 payload 优先）
        let mut both = serde_json::json!({ "session_id": "snake", "sessionId": "camel" });
        alias_camel_case_fields(&mut both);
        assert_eq!(both["session_id"], "snake");

        // 结构化 error 对象不得被当成分类字符串
        let mut structured = serde_json::json!({ "error": { "code": 500 } });
        alias_camel_case_fields(&mut structured);
        assert!(structured.get("error_type").is_none());
    }
}
