//! 全局 `config.json` 的 tauri-free 读取器。
//!
//! mini-term 主程序通过 Tauri 的 app data dir 持久化 `config.json`,但 sidecar
//! 二进制(如 SSH MCP server)没有 `AppHandle`,无法用 Tauri API 拿到该路径。
//! 这里镜像 `miniterm-hook` 里 `get_port_file_path` 的平台分支逻辑,自行定位
//! `{app_data_dir}/com.mini-term.app/config.json`。
//!
//! 本模块只读 `sshConnections` 字段,供 SSH MCP sidecar 使用。

use crate::ssh_connection::SshConnection;
use serde::Deserialize;
use std::path::PathBuf;

/// mini-term 的 Tauri app 标识,决定 app data 子目录名。
const APP_ID: &str = "com.mini-term.app";

/// 定位全局 `config.json` 的平台特定路径。
///
/// 与 `miniterm-hook.rs` 的 `get_port_file_path` 同套平台分支,仅文件名换成
/// `config.json`:
/// - Windows: `%APPDATA%/com.mini-term.app/config.json`
/// - macOS: `~/Library/Application Support/com.mini-term.app/config.json`
/// - Linux: `$XDG_DATA_HOME/com.mini-term.app/config.json`
///   或 `~/.local/share/com.mini-term.app/config.json`
pub fn config_json_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|appdata| PathBuf::from(appdata).join(APP_ID).join("config.json"))
    }

    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| {
            h.join("Library")
                .join("Application Support")
                .join(APP_ID)
                .join("config.json")
        })
    }

    #[cfg(target_os = "linux")]
    {
        let data_dir = std::env::var("XDG_DATA_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")));
        data_dir.map(|d| d.join(APP_ID).join("config.json"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// `config.json` 的最小投影,只取本模块关心的 `sshConnections` 字段。
///
/// serde 默认忽略未知字段,因此无需复刻完整的 `AppConfig`;字段缺失时
/// `#[serde(default)]` 给空 Vec。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigSshView {
    #[serde(default)]
    ssh_connections: Vec<SshConnection>,
}

/// 读取全局 `config.json` 里的 SSH 连接列表。
///
/// 文件不存在 / 路径无法定位 / JSON 解析失败时一律返回空 Vec,绝不 panic
/// ——sidecar 在 stdio 协议下不能因配置问题崩溃。
pub fn read_ssh_connections() -> Vec<SshConnection> {
    parse_ssh_connections_from(config_json_path())
}

/// 从给定路径读取并解析 SSH 连接。抽出便于单元测试注入临时文件。
fn parse_ssh_connections_from(path: Option<PathBuf>) -> Vec<SshConnection> {
    let Some(path) = path else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<ConfigSshView>(&content) {
        Ok(view) => view.ssh_connections,
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mt-core-cfg-{name}-{ts}.json"));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn missing_file_yields_empty_vec() {
        let conns = parse_ssh_connections_from(Some(PathBuf::from(
            "/definitely/not/a/real/config.json",
        )));
        assert!(conns.is_empty());
    }

    #[test]
    fn none_path_yields_empty_vec() {
        assert!(parse_ssh_connections_from(None).is_empty());
    }

    #[test]
    fn invalid_json_yields_empty_vec() {
        let path = temp_file("invalid", "{ not valid json");
        let conns = parse_ssh_connections_from(Some(path.clone()));
        assert!(conns.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn config_without_ssh_connections_yields_empty_vec() {
        // 真实 config.json 含大量其它字段,缺 sshConnections 时应给空 Vec
        let path = temp_file("nossh", r#"{"theme":"dark","hookEnabled":true}"#);
        let conns = parse_ssh_connections_from(Some(path.clone()));
        assert!(conns.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_ssh_connections_ignoring_other_fields() {
        let json = r#"{
            "theme": "dark",
            "smartCopyPaste": false,
            "sshConnections": [
                {"id":"1","name":"prod","host":"10.0.0.5","port":22,"user":"root","password":"secret","agentAccessible":true},
                {"id":"2","name":"dev","host":"dev.example.com","port":2222,"user":"deploy"}
            ]
        }"#;
        let path = temp_file("withssh", json);
        let conns = parse_ssh_connections_from(Some(path.clone()));
        assert_eq!(conns.len(), 2);
        assert_eq!(conns[0].name, "prod");
        assert!(conns[0].agent_accessible);
        assert_eq!(conns[0].password.as_deref(), Some("secret"));
        // 第二条没有 agentAccessible / password,应取默认值
        assert!(!conns[1].agent_accessible);
        assert!(conns[1].password.is_none());
        assert_eq!(conns[1].port, 2222);
        let _ = std::fs::remove_file(&path);
    }
}
