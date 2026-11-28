//! miniterm-hook CLI 小工具
//!
//! 极简二进制，被 Claude Code / Codex / Grok Build / Antigravity CLI 的 hook 系统调用。
//! 功能：读 stdin JSON payload -> 读环境变量 -> POST 到 miniterm hook 服务器。
//!
//! 几家的调用约定不同，在这里抹平（见 `classify_source` / `resolve_event_name` /
//! `alias_camel_case_fields` / `normalize_agy_fields` / `detect_agent`）：
//! - Claude/Codex：事件名走 argv，payload 是 snake_case；
//! - Grok：事件名走 `GROK_HOOK_EVENT`（snake_case，如 `pre_tool_use`），
//!   payload 是 camelCase（`sessionId` / `toolName` / …）；
//! - Antigravity CLI（agy）：事件名走 argv（payload 里另有 `hookEventName`），payload 是
//!   camelCase（`conversationId` / `terminationReason` / `fullyIdle` / …），且**要求 hook
//!   往 stdout 输出一个 JSON 对象** —— 我们对它的每个事件都输出 `{}`（见 `ack_agy`）。
//!
//! 依赖最小化：仅使用 serde_json + 标准库，不引入额外 HTTP 客户端。

use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

/// 从 stdin 读取的超时时间（毫秒）
const STDIN_TIMEOUT_MS: u64 = 400;

fn main() {
    // 1. 事件名：argv（Claude/Codex/agy）优先，Grok 原生注册不带 argv
    let argv_event = std::env::args().nth(1).unwrap_or_default();
    let grok_session = std::env::var("GROK_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let agy_conversation = std::env::var(AGY_CONVERSATION_ENV)
        .ok()
        .filter(|s| !s.is_empty());

    // agy 的判定要看 payload（见 `classify_source`），所以带它的环境变量时先把 stdin 读了并解析；
    // 不带的照旧走原来的顺序（先判 grok 兼容层要不要丢，再读 stdin），别家行为一点不变
    let early_stdin_read = agy_conversation.is_some();
    let early_payload = if early_stdin_read {
        read_stdin_with_timeout()
            .as_deref()
            .and_then(parse_payload_object)
    } else {
        None
    };

    // agy 要求 hook 往 stdout 输出 JSON 对象：**先于一切早退**（拿不到端口、连不上服务器、
    // stdin 超时）交上 `{}`。只打这一个：`{}` 在 PreInvocation / Stop 上 = 不注入、放行
    // （PreToolUse 上则是拒绝工具，所以那个事件绝不注册，见 hook_registry 的 agy 段）
    if agy_should_ack(agy_conversation.as_deref(), early_payload.as_ref()) {
        ack_agy();
    }
    // agy 的环境变量在场、payload 却没读到(stdin 超时 / 读不懂):多半就是 agy 本身,只是
    // 判不出它是主会话还是子代理(`lastUserInput`)、Stop 是不是 fullyIdle —— 冒着把子代理
    // 收尾报成「完成」的风险上报,不如这一条不报;交给后续事件与停摆兜底。别家的 hook
    // 一定带 payload,不会落进这里
    if agy_drops_unreadable(agy_conversation.as_deref(), early_payload.as_ref()) {
        return;
    }

    let source = classify_source(
        agy_conversation.as_deref(),
        grok_session.as_deref(),
        early_payload.as_ref(),
    );
    let is_agy = source == Source::Agy;
    let is_grok = source == Source::Grok;

    // Grok 默认还会扫描 ~/.claude/settings.json 里的 hooks（Claude 兼容层），
    // 于是同一个 grok 事件可能来两趟：一趟走我们写进 ~/.grok/hooks/ 的原生条目
    // （无 argv），一趟走 Claude 条目（有 argv）。原生条目在场时丢掉兼容层那一趟，
    // 否则每个事件都要多跑一个进程，attention 类事件还会绕过去重双发。
    // 反之（用户只注册了 Claude、没注册 Grok）必须放行——那是唯一的来源。
    if is_grok && !argv_event.is_empty() && grok_native_hook_installed() {
        return;
    }

    let event_name = if is_agy {
        agy_event_name(&argv_event, early_payload.as_ref())
    } else {
        resolve_event_name(&argv_event)
    };

    // 2. 从 stdin 读取并解析 JSON payload（带超时）；agy 环境变量在场时上面已读并解析过，直接复用
    let parsed_payload = match early_payload {
        Some(payload) => Some(payload),
        None if early_stdin_read => None,
        None => read_stdin_with_timeout()
            .as_deref()
            .and_then(parse_payload_object),
    };

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

    // 5. 构造 POST body（顶层不是对象的按空对象算：下面要往里插字段）
    let mut body = parsed_payload.unwrap_or_else(|| serde_json::json!({}));

    // agy：会话 id / Stop 字段改名，`lastUserInput` 正文换成一个布尔（隐私 + body 上限）
    if is_agy {
        normalize_agy_fields(&mut body, agy_conversation.as_deref());
    }

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
    if is_grok && body.get("session_id").is_none() {
        if let Some(ref sid) = grok_session {
            body["session_id"] = serde_json::json!(sid);
        }
    }

    // 推断 agent 类型（agy 以判定为准，不看 payload）
    if is_agy || body.get("agent").is_none() {
        body["agent"] = serde_json::json!(detect_agent(&body, source));
    }

    let body_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());

    // 6. 发送 HTTP POST
    send_http_post(port, &body_str);
}

/// agy 给 hook 进程注入的环境变量（= 本次事件的 conversationId）
const AGY_CONVERSATION_ENV: &str = "ANTIGRAVITY_CONVERSATION_ID";

/// 调起本进程的是谁（靠环境变量 + payload 判，先于形状推断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Agy,
    Grok,
    /// Claude / Codex，交给 [`detect_agent`] 按 payload 形状分
    Other,
}

/// 来源判定。环境变量会被子进程**继承**，所以单看环境变量不够：
///
/// - agy 要求 `ANTIGRAVITY_CONVERSATION_ID` 非空**且** payload 带 `conversationId` 键。
///   grok / claude / codex 跑在 agy 的终端工具里时会继承 agy 的环境变量，但它们的
///   payload 没有 `conversationId`（grok 是 `sessionId`，另两家是 `session_id`）；
/// - agy 先于 grok 判：agy 跑在 grok 的终端工具里时，agy 的 hook 进程会继承
///   `GROK_SESSION_ID`，按 grok 判就会被「grok 兼容层去重」整条丢掉。
fn classify_source(
    agy_env: Option<&str>,
    grok_env: Option<&str>,
    payload: Option<&serde_json::Value>,
) -> Source {
    let agy_payload = payload.is_some_and(|p| p.get("conversationId").is_some());
    if agy_env.is_some_and(|s| !s.is_empty()) && agy_payload {
        Source::Agy
    } else if grok_env.is_some_and(|s| !s.is_empty()) {
        Source::Grok
    } else {
        Source::Other
    }
}

/// 要不要往 stdout 交 `{}`：agy 的环境变量在场，且 payload 不是别家的（带
/// `conversationId`，或者压根没读到 / 读不懂 —— stdin 超时也得交，agy 正等着）。
/// 别家（payload 在、但没有 `conversationId`）照旧不往 stdout 写任何东西。
fn agy_should_ack(agy_env: Option<&str>, payload: Option<&serde_json::Value>) -> bool {
    agy_env.is_some_and(|s| !s.is_empty())
        && payload.is_none_or(|p| p.get("conversationId").is_some())
}

/// agy 环境变量在场但 payload 读不到 → 只回 `{}`、不上报(见 `main` 里的说明)。
fn agy_drops_unreadable(agy_env: Option<&str>, payload: Option<&serde_json::Value>) -> bool {
    agy_env.is_some_and(|s| !s.is_empty()) && payload.is_none()
}

/// 往 stdout 交一个空 JSON 对象。写失败（stdout 被关了）也不 panic —— `println!` 会。
fn ack_agy() {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b"{}\n");
    let _ = out.flush();
}

/// stdin 内容 → JSON 对象；读不懂或顶层不是对象返回 `None`。
fn parse_payload_object(raw: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .filter(|v| v.is_object())
}

/// agy 的事件名：argv 优先（注册的命令里带着），缺了回落 payload 的 `hookEventName`。
/// 不看 `GROK_HOOK_EVENT`：agy 跑在 grok 里时那是继承来的、别人的事件名。
fn agy_event_name(argv_event: &str, payload: Option<&serde_json::Value>) -> String {
    if !argv_event.is_empty() {
        return argv_event.to_string();
    }
    payload
        .and_then(|p| p.get("hookEventName"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// agy 的错误原文转发的长度上限（字符）。只进 hook server 的日志。
const AGY_ERROR_MAX_CHARS: usize = 200;

/// agy payload 的归一化（hook server 只认 snake_case，且只要它用得上的那几项）：
///
/// - `session_id` ← `conversationId`，缺了回落 `ANTIGRAVITY_CONVERSATION_ID`；
/// - `terminationReason` → `termination_reason`、`fullyIdle` → `fully_idle`（Stop 用）；
/// - `lastUserInput` 是**用户最近一条输入的全文** —— 正文不转发（隐私；hook server 的
///   body 上限 1 MiB，粘一大段代码就能顶穿），只留 `has_user_input`（非空即 true）：
///   子代理会话的事件里它恒为空串，hook server 据此整条丢弃；
/// - `toolCall`（工具参数，可能是整段命令 / 文件内容）用不到，不转发；
/// - `error`（Stop / PostToolUse 的错误原文）非空时截断成 `error_type`，只进日志。
fn normalize_agy_fields(body: &mut serde_json::Value, env_conversation: Option<&str>) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if !obj.contains_key("session_id") {
        let sid = obj
            .get("conversationId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or(env_conversation.filter(|s| !s.is_empty()))
            .map(str::to_string);
        if let Some(sid) = sid {
            obj.insert("session_id".to_string(), serde_json::Value::String(sid));
        }
    }
    if let Some(v @ serde_json::Value::String(_)) = obj.get("terminationReason").cloned() {
        obj.insert("termination_reason".to_string(), v);
    }
    if let Some(v @ serde_json::Value::Bool(_)) = obj.get("fullyIdle").cloned() {
        obj.insert("fully_idle".to_string(), v);
    }
    // 不是字符串的（不该出现）不下结论：缺这一位 hook server 按主会话处理
    if let Some(serde_json::Value::String(text)) = obj.remove("lastUserInput") {
        obj.insert(
            "has_user_input".to_string(),
            serde_json::Value::Bool(!text.is_empty()),
        );
    }
    obj.remove("toolCall");
    if let Some(serde_json::Value::String(error)) = obj.remove("error") {
        if !error.is_empty() && !obj.contains_key("error_type") {
            let short: String = error.chars().take(AGY_ERROR_MAX_CHARS).collect();
            obj.insert("error_type".to_string(), serde_json::Value::String(short));
        }
    }
}

/// 来源 agent。
///
/// `GROK_SESSION_ID` 由 grok 的 hook runner 注入，且在其实现里**优先级最高**
/// （runner 注入的环境变量总是覆盖用户/插件的 extra_env），是可靠判据，必须先判：
/// grok 会读 Claude 的 settings.json，payload 形状也与 Claude 相近，靠形状推断
/// 会把 grok 事件标成 `claude-code`，镜像随后按 claude 去找会话文件，绑到同项目
/// Claude 的最新对话上（串台，与 AGENTS.md 对 opencode/pi 的警告同源）。
/// agy 同理（见 [`classify_source`]）。
fn detect_agent(body: &serde_json::Value, source: Source) -> &'static str {
    match source {
        Source::Agy => return "agy",
        Source::Grok => return "grok",
        Source::Other => {}
    }
    // Codex 的 hook payload 固定携带 turn_id，Claude Code 没有该字段；
    // transcript_path 两者都有，不能用来区分。
    if body.get("turn_id").is_some() {
        return "codex";
    }
    // CodeBuddy 的 hook 与 Claude 同构，但 transcript 落在 ~/.codebuddy/projects
    // 下；不区分就会被标成 claude-code，镜像绑到同项目 Claude 的最新会话（串台）。
    let transcript = body
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if transcript.contains(".codebuddy") {
        "codebuddy"
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

/// 获取 hook-server.json 的平台特定路径。
///
/// 与 `mt_core::config_json_path()` 平台分支口径完全一致（均按装机版数据目录定位，
/// 不认 `MT_APP_DATA_DIR`），直接复用并替换文件名。
fn get_port_file_path() -> Option<std::path::PathBuf> {
    mt_core::config_json_path().map(|path| path.with_file_name("hook-server.json"))
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
        assert_eq!(detect_agent(&claude_shaped, Source::Grok), "grok");
        assert_eq!(detect_agent(&claude_shaped, Source::Other), "claude-code");

        let codex_shaped = serde_json::json!({ "turn_id": "t" });
        assert_eq!(detect_agent(&codex_shaped, Source::Other), "codex");
        // turn_id 是 codex 的形状特征，但 grok 环境变量在场时不参与判定
        assert_eq!(detect_agent(&codex_shaped, Source::Grok), "grok");
        assert_eq!(detect_agent(&codex_shaped, Source::Agy), "agy");
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

    // ---- Antigravity CLI(agy) ----

    /// agy 实测的 Stop payload(本机 1.5.12 探针抓的形状,正文换成占位)。
    fn agy_stop_payload() -> serde_json::Value {
        serde_json::json!({
            "artifactDirectoryPath": "C:\\Users\\u\\.gemini\\antigravity-cli\\brain\\conv-1",
            "continuationActive": false,
            "conversationId": "conv-1",
            "cwd": "D:\\Git\\proj",
            "error": "",
            "executionNum": 0,
            "fullyIdle": true,
            "hookEventName": "Stop",
            "lastUserInput": "帮我把这段很长很长的代码重构一下……",
            "modelName": "gemini-3.8-flash-high",
            "terminationReason": "NO_TOOL_CALL",
            "transcriptPath": "C:\\Users\\u\\.gemini\\antigravity-cli\\brain\\conv-1\\.system_generated\\logs\\transcript_full.jsonl",
            "workspacePaths": ["D:\\Git\\proj"]
        })
    }

    /// agy 的判定要环境变量与 payload 的 `conversationId` 同时成立。
    #[test]
    fn agy_needs_both_env_and_conversation_id() {
        let agy = agy_stop_payload();
        assert_eq!(
            classify_source(Some("conv-1"), None, Some(&agy)),
            Source::Agy
        );
        // 只有 payload 没有环境变量(手工 POST / 老版本):不认
        assert_eq!(classify_source(None, None, Some(&agy)), Source::Other);
        // 环境变量是空串:不认
        assert_eq!(classify_source(Some(""), None, Some(&agy)), Source::Other);
        // 只有环境变量、payload 读不到:不认 agy(但仍要交 `{}`,见下一条)
        assert_eq!(classify_source(Some("conv-1"), None, None), Source::Other);
    }

    /// 嵌套方向一:agy 跑在 grok 的终端工具里 —— agy 的 hook 进程继承了 `GROK_SESSION_ID`,
    /// 必须仍判成 agy(按 grok 判会被「兼容层去重」整条丢掉,agent 也会标错)。
    #[test]
    fn agy_inside_grok_is_still_agy() {
        let agy = agy_stop_payload();
        assert_eq!(
            classify_source(Some("conv-1"), Some("grok-sid"), Some(&agy)),
            Source::Agy
        );
        assert_eq!(detect_agent(&agy, Source::Agy), "agy");
        // 事件名不看继承来的 GROK_HOOK_EVENT:argv 优先,缺了回落 hookEventName
        assert_eq!(agy_event_name("Stop", Some(&agy)), "Stop");
        assert_eq!(agy_event_name("", Some(&agy)), "Stop");
        assert_eq!(agy_event_name("", None), "");
    }

    /// 嵌套方向二:grok / claude / codex 跑在 agy 的终端工具里 —— 它们的 hook 进程继承了
    /// `ANTIGRAVITY_CONVERSATION_ID`,但 payload 没有 `conversationId`:照旧按本家判,
    /// 也不往 stdout 写 `{}`(别家的 hook 输出协议不归我们改)。
    #[test]
    fn others_inside_agy_keep_their_own_identity() {
        let grok = serde_json::json!({ "sessionId": "grok-sid", "hookEventName": "stop" });
        assert_eq!(
            classify_source(Some("conv-1"), Some("grok-sid"), Some(&grok)),
            Source::Grok
        );
        assert!(!agy_should_ack(Some("conv-1"), Some(&grok)));

        let claude = serde_json::json!({ "session_id": "s", "transcript_path": "x" });
        assert_eq!(
            classify_source(Some("conv-1"), None, Some(&claude)),
            Source::Other
        );
        assert_eq!(detect_agent(&claude, Source::Other), "claude-code");
        assert!(!agy_should_ack(Some("conv-1"), Some(&claude)));

        let codex = serde_json::json!({ "session_id": "s", "turn_id": "t" });
        assert_eq!(detect_agent(&codex, Source::Other), "codex");
    }

    /// `{}` 要先于一切早退交出去:agy 的事件、以及 agy 环境变量在场但 stdin 超时 / 读不懂
    /// 的情形(那时 agy 正等着输出);没有 agy 环境变量的进程一律不写 stdout。
    #[test]
    fn agy_ack_covers_timeouts_and_skips_everyone_else() {
        let agy = agy_stop_payload();
        assert!(agy_should_ack(Some("conv-1"), Some(&agy)));
        assert!(agy_should_ack(Some("conv-1"), None), "stdin 超时也得交");
        assert!(!agy_should_ack(None, Some(&agy)));
        assert!(!agy_should_ack(Some(""), None));
        assert!(!agy_should_ack(None, None));
    }

    /// agy 环境变量在场、payload 没读到:交 `{}` 后整条不报 —— 判不出主会话 / 子代理、
    /// Stop 是否 fullyIdle,按 claude-code 报上去会把子代理收尾误播成「完成」。
    /// 别家(有 payload)与不在 agy 里的进程照常上报。
    #[test]
    fn agy_unreadable_payload_is_acked_but_not_reported() {
        assert!(agy_drops_unreadable(Some("conv-1"), None));
        let claude_inside_agy = serde_json::json!({ "session_id": "s" });
        assert!(!agy_drops_unreadable(
            Some("conv-1"),
            Some(&claude_inside_agy)
        ));
        assert!(!agy_drops_unreadable(
            Some("conv-1"),
            Some(&agy_stop_payload())
        ));
        assert!(
            !agy_drops_unreadable(None, None),
            "不在 agy 里:照旧(claude 空 stdin 兜底)"
        );
        assert!(!agy_drops_unreadable(Some(""), None));
    }

    #[test]
    fn payload_must_be_a_json_object() {
        assert!(parse_payload_object("{\"a\":1}").is_some());
        for raw in ["", "{broken", "[1]", "\"x\"", "null"] {
            assert!(parse_payload_object(raw).is_none(), "{raw:?}");
        }
    }

    /// 归一化:会话 id / Stop 字段改名;`lastUserInput` 只留「非空」一位、正文不转发;
    /// `toolCall` 不转发;空的 `error` 不变成 error_type。
    #[test]
    fn agy_fields_are_normalized_without_leaking_user_input() {
        let mut body = agy_stop_payload();
        normalize_agy_fields(&mut body, Some("env-conv"));
        alias_camel_case_fields(&mut body);
        assert_eq!(
            body["session_id"], "conv-1",
            "payload 的 conversationId 优先"
        );
        assert_eq!(body["termination_reason"], "NO_TOOL_CALL");
        assert_eq!(body["fully_idle"], true);
        assert_eq!(body["has_user_input"], true);
        assert_eq!(body["hook_event_name"], "Stop");
        assert!(body.get("lastUserInput").is_none());
        assert!(body.get("error").is_none());
        assert!(body.get("error_type").is_none(), "空 error 不是错误");
        let text = serde_json::to_string(&body).unwrap();
        assert!(!text.contains("重构"), "用户输入的正文不能转发: {text}");

        // 子代理:lastUserInput 是空串 → has_user_input=false;toolCall 不转发
        let mut sub = serde_json::json!({
            "conversationId": "sub-1",
            "lastUserInput": "",
            "toolCall": { "id": "t", "name": "run_command", "args": { "CommandLine": "rm -rf x" } },
            "error": "exit status 1"
        });
        normalize_agy_fields(&mut sub, None);
        assert_eq!(sub["has_user_input"], false);
        assert!(sub.get("toolCall").is_none());
        assert_eq!(sub["error_type"], "exit status 1");

        // 缺 conversationId 回落环境变量;lastUserInput 缺失 / 不是字符串时不下结论
        let mut bare = serde_json::json!({ "lastUserInput": null });
        normalize_agy_fields(&mut bare, Some("env-conv"));
        assert_eq!(bare["session_id"], "env-conv");
        assert!(bare.get("has_user_input").is_none());

        // 超长错误原文截断
        let mut long = serde_json::json!({ "error": "错".repeat(AGY_ERROR_MAX_CHARS + 50) });
        normalize_agy_fields(&mut long, None);
        assert_eq!(
            long["error_type"].as_str().unwrap().chars().count(),
            AGY_ERROR_MAX_CHARS
        );
    }

    #[test]
    fn port_file_lives_beside_config_json() {
        let port_path = get_port_file_path();
        let config_path = mt_core::config_json_path();
        assert_eq!(
            port_path.as_ref().and_then(|p| p.parent()),
            config_path.as_ref().and_then(|p| p.parent())
        );
        if let Some(path) = port_path {
            assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some("hook-server.json")
            );
        }
    }
}
