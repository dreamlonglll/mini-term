//! 全局 `config.json` 的 tauri-free 读取器。
//!
//! mini-term 主程序通过 Tauri 的 app data dir 持久化 `config.json`,但 sidecar
//! 二进制(如 SSH MCP server)没有 `AppHandle`,无法用 Tauri API 拿到该路径。
//! 这里镜像 `miniterm-hook` 里 `get_port_file_path` 的平台分支逻辑,自行定位
//! `{app_data_dir}/com.mini-term.app/config.json`。
//!
//! 本模块读 `sshConnections` 与 `projects[]` 的 `sshConnectionIds`,供 SSH MCP
//! sidecar 按项目过滤可见连接。

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

/// `config.json` 的最小投影,只取本模块关心的字段。
///
/// serde 默认忽略未知字段,因此无需复刻完整的 `AppConfig`;字段缺失时
/// `#[serde(default)]` 给空 Vec。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigSshView {
    #[serde(default)]
    ssh_connections: Vec<SshConnection>,
    #[serde(default)]
    projects: Vec<ProjectScopeView>,
}

/// `config.json` 里 `projects[]` 的最小投影,只取项目 id 与其 SSH 关联范围。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectScopeView {
    #[serde(default)]
    id: String,
    /// 该项目关联的 SSH 连接 id 列表。
    /// `None`(字段缺失)= 未设置关联 → 默认全部连接可见。
    #[serde(default)]
    ssh_connection_ids: Option<Vec<String>>,
}

/// 读取对指定项目可见的 SSH 连接。
///
/// 范围规则:
/// - `project_id` 为 `None`(sidecar 未带 `--project-id`)→ 返回全部连接。
/// - `project_id` 命中的项目设置了 `sshConnectionIds` → 仅返回 id 在该列表里的连接。
/// - 项目未找到 / 未设置 `sshConnectionIds` → 返回全部连接(默认全部可见)。
///
/// 文件不存在 / 路径无法定位 / JSON 解析失败时一律返回空 Vec,绝不 panic
/// ——sidecar 在 stdio 协议下不能因配置问题崩溃。
pub fn read_ssh_connections_for_project(project_id: Option<&str>) -> Vec<SshConnection> {
    let view = parse_config_from(config_json_path());
    scope_connections(view.ssh_connections, &view.projects, project_id)
}

/// 从给定路径读取并解析 `config.json` 的最小投影。抽出便于单元测试注入临时文件。
///
/// 文件不存在 / 读取失败 / JSON 解析失败时返回空投影(空连接 + 空项目)。
fn parse_config_from(path: Option<PathBuf>) -> ConfigSshView {
    let Some(path) = path else {
        return ConfigSshView::default();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return ConfigSshView::default();
    };
    serde_json::from_str::<ConfigSshView>(&content).unwrap_or_default()
}

/// 按项目关联范围过滤连接。纯函数,便于单测。
///
/// 范围规则见 [`read_ssh_connections_for_project`]。
fn scope_connections(
    connections: Vec<SshConnection>,
    projects: &[ProjectScopeView],
    project_id: Option<&str>,
) -> Vec<SshConnection> {
    let Some(pid) = project_id else {
        return connections;
    };
    let scope = projects
        .iter()
        .find(|p| p.id == pid)
        .and_then(|p| p.ssh_connection_ids.as_ref());
    match scope {
        // 项目未找到 / 未设置 → 默认全部可见
        None => connections,
        // 已设置 → 仅保留 id 在列表里的连接(空列表 → 全部不可见)
        Some(ids) => connections
            .into_iter()
            .filter(|c| ids.iter().any(|id| id == &c.id))
            .collect(),
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

    fn conn(id: &str) -> SshConnection {
        SshConnection {
            id: id.into(),
            name: format!("conn-{id}"),
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            password: None,
            identity_file: None,
            group: None,
        }
    }

    // --- parse_config_from ---

    #[test]
    fn missing_file_yields_empty_view() {
        let view = parse_config_from(Some(PathBuf::from("/definitely/not/a/real/config.json")));
        assert!(view.ssh_connections.is_empty());
        assert!(view.projects.is_empty());
    }

    #[test]
    fn none_path_yields_empty_view() {
        let view = parse_config_from(None);
        assert!(view.ssh_connections.is_empty());
        assert!(view.projects.is_empty());
    }

    #[test]
    fn invalid_json_yields_empty_view() {
        let path = temp_file("invalid", "{ not valid json");
        let view = parse_config_from(Some(path.clone()));
        assert!(view.ssh_connections.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn config_without_ssh_connections_yields_empty_view() {
        // 真实 config.json 含大量其它字段,缺 sshConnections 时应给空 Vec
        let path = temp_file("nossh", r#"{"theme":"dark","hookEnabled":true}"#);
        let view = parse_config_from(Some(path.clone()));
        assert!(view.ssh_connections.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_ssh_connections_ignoring_other_fields() {
        let json = r#"{
            "theme": "dark",
            "smartCopyPaste": false,
            "sshConnections": [
                {"id":"1","name":"prod","host":"10.0.0.5","port":22,"user":"root","password":"secret"},
                {"id":"2","name":"dev","host":"dev.example.com","port":2222,"user":"deploy"}
            ]
        }"#;
        let path = temp_file("withssh", json);
        let view = parse_config_from(Some(path.clone()));
        assert_eq!(view.ssh_connections.len(), 2);
        assert_eq!(view.ssh_connections[0].name, "prod");
        assert_eq!(view.ssh_connections[0].password.as_deref(), Some("secret"));
        assert!(view.ssh_connections[1].password.is_none());
        assert_eq!(view.ssh_connections[1].port, 2222);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_project_ssh_scope() {
        let json = r#"{
            "sshConnections": [{"id":"1","name":"prod","host":"h","port":22,"user":"u"}],
            "projects": [
                {"id":"p1","name":"A","path":"/a","sshConnectionIds":["1"]},
                {"id":"p2","name":"B","path":"/b"}
            ]
        }"#;
        let path = temp_file("withproj", json);
        let view = parse_config_from(Some(path.clone()));
        assert_eq!(view.projects.len(), 2);
        assert_eq!(view.projects[0].id, "p1");
        assert_eq!(view.projects[0].ssh_connection_ids.as_deref(), Some(&["1".to_string()][..]));
        // 未设置 sshConnectionIds 的项目为 None
        assert!(view.projects[1].ssh_connection_ids.is_none());
        let _ = std::fs::remove_file(&path);
    }

    // --- scope_connections ---

    #[test]
    fn scope_none_project_returns_all() {
        // sidecar 未带 --project-id → 不限定,全部可见
        let conns = vec![conn("1"), conn("2")];
        let scoped = scope_connections(conns, &[], None);
        assert_eq!(scoped.len(), 2);
    }

    #[test]
    fn scope_project_not_found_returns_all() {
        let conns = vec![conn("1"), conn("2")];
        let projects = vec![ProjectScopeView {
            id: "other".into(),
            ssh_connection_ids: Some(vec!["1".into()]),
        }];
        // 给定的 project_id 在 projects 里找不到 → 默认全部可见
        let scoped = scope_connections(conns, &projects, Some("missing"));
        assert_eq!(scoped.len(), 2);
    }

    #[test]
    fn scope_project_without_ids_returns_all() {
        let conns = vec![conn("1"), conn("2")];
        let projects = vec![ProjectScopeView {
            id: "p1".into(),
            ssh_connection_ids: None,
        }];
        // 项目存在但未设置 sshConnectionIds → 默认全部可见
        let scoped = scope_connections(conns, &projects, Some("p1"));
        assert_eq!(scoped.len(), 2);
    }

    #[test]
    fn scope_filters_to_listed_ids() {
        let conns = vec![conn("1"), conn("2"), conn("3")];
        let projects = vec![ProjectScopeView {
            id: "p1".into(),
            ssh_connection_ids: Some(vec!["1".into(), "3".into()]),
        }];
        let scoped = scope_connections(conns, &projects, Some("p1"));
        let ids: Vec<&str> = scoped.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["1", "3"]);
    }

    #[test]
    fn scope_empty_ids_returns_none() {
        // 显式空列表 → 该项目不关联任何连接,全部不可见
        let conns = vec![conn("1"), conn("2")];
        let projects = vec![ProjectScopeView {
            id: "p1".into(),
            ssh_connection_ids: Some(vec![]),
        }];
        let scoped = scope_connections(conns, &projects, Some("p1"));
        assert!(scoped.is_empty());
    }

    #[test]
    fn scope_ignores_unknown_ids_in_list() {
        // sshConnectionIds 含已删除连接的陈旧 id,只过滤出仍存在的连接
        let conns = vec![conn("1")];
        let projects = vec![ProjectScopeView {
            id: "p1".into(),
            ssh_connection_ids: Some(vec!["1".into(), "stale".into()]),
        }];
        let scoped = scope_connections(conns, &projects, Some("p1"));
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, "1");
    }
}
