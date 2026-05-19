//! miniterm-hook CLI 小工具
//!
//! 极简二进制，被 Claude Code / Codex 的 hook 系统调用。
//! 功能：读 stdin JSON payload -> 读环境变量 -> POST 到 miniterm hook 服务器。
//!
//! 依赖最小化：仅使用 serde_json + 标准库，不引入额外 HTTP 客户端。

use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

/// 从 stdin 读取的超时时间（毫秒）
const STDIN_TIMEOUT_MS: u64 = 400;

fn main() {
    // 1. 获取事件名（从命令行参数）
    let event_name = std::env::args().nth(1).unwrap_or_default();

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

    // 推断 agent 类型
    if body.get("agent").is_none() {
        // 尝试从 stdin payload 的字段推断
        let agent = if body.get("transcript_path").is_some() {
            "codex"
        } else {
            "claude-code"
        };
        body["agent"] = serde_json::json!(agent);
    }

    let body_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());

    // 6. 发送 HTTP POST
    send_http_post(port, &body_str);
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
