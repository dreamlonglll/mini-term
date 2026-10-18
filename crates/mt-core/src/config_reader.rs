//! 全局 `config.json` 的 tauri-free 读取器。
//!
//! mini-term 主程序通过 Tauri 的 app data dir 持久化 `config.json`,但 sidecar
//! 二进制(如 SSH MCP server)没有 `AppHandle`,无法用 Tauri API 拿到该路径。
//! 这里镜像 `miniterm-hook` 里 `get_port_file_path` 的平台分支逻辑,自行定位
//! `{app_data_dir}/com.mini-term.app/config.json`。
//!
//! 本模块读 `sshConnections` 与 `projects[]` 的 `sshConnectionIds`,供 SSH MCP
//! sidecar 按项目过滤可见连接。
//!
//! 两条入口的失败口径不同:
//! - MCP 存量的 project-id 路径([`read_ssh_connections_for_project`])读不到就当空配置
//!   (stdio 协议下不能因配置问题崩);
//! - CLI / daemon 的能力令牌路径([`read_ssh_connections_for_token`])**fail closed** 且
//!   报错分类([`TokenScopeError`]):配置不存在 / 读不了 / 解析不了 / 令牌对不上 / 范围
//!   为空各给一句可操作的提示,任何一类都返回 `Err`,绝不退回「全部连接」。

use crate::ssh_connection::SshConnection;
use serde::Deserialize;
use std::path::{Path, PathBuf};

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
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigSshView {
    #[serde(default)]
    ssh_connections: Vec<SshConnection>,
    #[serde(default)]
    projects: Vec<ProjectScopeView>,
}

/// `config.json` 里 `projects[]` 的最小投影,只取项目 id 与其 SSH 关联范围。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectScopeView {
    #[serde(default)]
    id: String,
    /// 只有项目启用 SSH MCP/CLI 能力时，其能力令牌才有效。
    #[serde(default)]
    ssh_mcp_enabled: bool,
    /// CLI/daemon 用的项目能力令牌。令牌命中后才能解析该项目的连接范围。
    #[serde(default)]
    ssh_cli_token: Option<String>,
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
    read_ssh_connections_for_project_at(config_json_path(), project_id)
}

/// [`read_ssh_connections_for_project`] 的按路径版本。
///
/// 生产路径永远走上面那个(路径由本模块自己定位)。这一支是给**写方**验证用的:
/// `config.json` 现在是 `mt-config` 写出来的 SSH 投影,而解析它的是这里 ——
/// 两边隔着一个 crate 边界、没有共享类型,只靠字段名对齐。mt-config 的测试
/// 直接拿本函数验投影,「投影漂了但没人发现」这条缝就堵上了。
pub fn read_ssh_connections_for_project_at(
    path: Option<PathBuf>,
    project_id: Option<&str>,
) -> Vec<SshConnection> {
    let view = parse_config_from(path);
    scope_connections(view.ssh_connections, &view.projects, project_id)
}

/// 读取能力令牌所属项目可见的 SSH 连接。
///
/// CLI/daemon 只能走此入口。任何失败都返回分类后的 [`TokenScopeError`],绝不回退到
/// 全部连接;命中后沿用项目 `sshConnectionIds` 的现有范围语义。
pub fn read_ssh_connections_for_token(
    project_token: &str,
) -> Result<Vec<SshConnection>, TokenScopeError> {
    read_ssh_connections_for_token_at(config_json_path(), project_token)
}

/// [`read_ssh_connections_for_token`] 的按路径版本。用途同
/// [`read_ssh_connections_for_project_at`] —— 给写方验证投影。
///
/// 令牌为空白时**先于读盘**就返回 —— 那是调用方漏传参数,与配置状态无关。
pub fn read_ssh_connections_for_token_at(
    path: Option<PathBuf>,
    project_token: &str,
) -> Result<Vec<SshConnection>, TokenScopeError> {
    if project_token.trim().is_empty() {
        return Err(TokenScopeError::MissingToken);
    }
    let (path, view) = load_config_view(path)?;
    scope_connections_for_token(view.ssh_connections, &view.projects, project_token, &path)
}

/// 能力令牌解析失败的分类。`Display` 是面向 agent 的英文提示(CLI 前缀 `mt-ssh-cli:
/// error:` 之后原样打印),每类都说清「怎么办」。
///
/// 两条纪律:
/// - **绝不回显配置内容**:读失败只带 io 错误类别与系统文案,解析失败只带 serde 的
///   错误类别与行列号(serde 的 Display 会把出错的值原文带出来,可能就是一段密文);
/// - **绝不回显完整令牌**:只给前几位(见 [`token_hint`]),够人对照是哪份 SKILL.md。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenScopeError {
    /// 令牌缺失或纯空白。
    MissingToken,
    /// 定位不了 mini-term 的数据目录(环境变量缺失)。
    ConfigPathUnknown,
    /// `config.json` 不存在。
    ConfigMissing { path: PathBuf },
    /// `config.json` 读不了(权限、被占用、不是 UTF-8……)。
    ConfigUnreadable {
        path: PathBuf,
        kind: std::io::ErrorKind,
        detail: String,
    },
    /// `config.json` 不是合法 JSON,或结构对不上。
    ConfigInvalid {
        path: PathBuf,
        category: &'static str,
        line: usize,
        column: usize,
    },
    /// 没有项目持有这个令牌。
    UnknownToken { token_hint: String, path: PathBuf },
    /// 不止一个项目持有这个令牌(配置被手改或合并坏了)。
    AmbiguousToken { token_hint: String, path: PathBuf },
    /// 持有令牌的项目已停用 SSH 工具。
    ProjectDisabled { token_hint: String },
    /// 项目找到了,但「关联 SSH」范围里一个仍存在的连接都没有。
    EmptyScope { token_hint: String },
}

/// 数据目录靠哪个环境变量定位 —— 报错时告诉用户该查哪个。
#[cfg(target_os = "windows")]
const DATA_DIR_ENV_HINT: &str = "APPDATA";
#[cfg(target_os = "macos")]
const DATA_DIR_ENV_HINT: &str = "HOME";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const DATA_DIR_ENV_HINT: &str = "XDG_DATA_HOME / HOME";

impl std::fmt::Display for TokenScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenScopeError::MissingToken => write!(
                f,
                "missing project token: pass --project-token exactly as written in this \
                 project's SKILL.md"
            ),
            TokenScopeError::ConfigPathUnknown => write!(
                f,
                "cannot locate mini-term's data directory ({DATA_DIR_ENV_HINT} is not set), so \
                 the SSH connections saved in mini-term cannot be read"
            ),
            TokenScopeError::ConfigMissing { path } => write!(
                f,
                "mini-term's SSH configuration {} does not exist: mini-term has not been run \
                 under this user account yet, or this process resolves a different data \
                 directory than mini-term does (it is located via {DATA_DIR_ENV_HINT})",
                path.display()
            ),
            TokenScopeError::ConfigUnreadable { path, kind, detail } => write!(
                f,
                "cannot read mini-term's SSH configuration {} ({kind:?}: {detail})",
                path.display()
            ),
            TokenScopeError::ConfigInvalid {
                path,
                category,
                line,
                column,
            } => write!(
                f,
                "mini-term's SSH configuration {} could not be parsed ({category} error at line \
                 {line}, column {column}); restart mini-term so it rewrites the file",
                path.display()
            ),
            TokenScopeError::UnknownToken { token_hint, path } => write!(
                f,
                "unknown project token {token_hint}: no project in {} holds it. The SKILL.md may \
                 be stale — re-save \"Link SSH\" for this project in mini-term to regenerate it — \
                 or it was generated by a mini-term dev instance (MT_APP_DATA_DIR), whose tokens \
                 only exist in that instance's own data directory while mt-ssh-cli always reads \
                 the installed app's configuration",
                path.display()
            ),
            TokenScopeError::AmbiguousToken { token_hint, path } => write!(
                f,
                "project token {token_hint} is assigned to more than one project in {}; re-save \
                 \"Link SSH\" in mini-term for the affected projects so each gets its own token",
                path.display()
            ),
            TokenScopeError::ProjectDisabled { token_hint } => write!(
                f,
                "project token {token_hint} belongs to a project whose SSH tools are disabled in \
                 mini-term; enable them again via \"Link SSH\""
            ),
            TokenScopeError::EmptyScope { token_hint } => write!(
                f,
                "the project for token {token_hint} has no SSH connections left in its \"Link \
                 SSH\" scope (the linked connections were deleted, or the linked group is \
                 empty); link connections to the project again in mini-term"
            ),
        }
    }
}

impl std::error::Error for TokenScopeError {}

/// 令牌的可展示前缀:最多 8 个字符、且不超过令牌长度的一半,后接 `…`。
///
/// 正常令牌是 36 字符的 UUID,只露 8 位(32 bit)足够对照「是哪份 SKILL.md」,又不至于
/// 让一条错误信息把整个能力令牌带进日志 / 会话记录。
fn token_hint(token: &str) -> String {
    let token = token.trim();
    let shown = (token.chars().count() / 2).min(8);
    let prefix: String = token.chars().take(shown).collect();
    format!("{prefix}…")
}

/// serde_json 错误类别 → 报错用的词。
fn json_error_category(category: serde_json::error::Category) -> &'static str {
    match category {
        serde_json::error::Category::Io => "I/O",
        serde_json::error::Category::Syntax => "JSON syntax",
        serde_json::error::Category::Data => "unexpected structure",
        serde_json::error::Category::Eof => "truncated JSON",
    }
}

/// 读取并解析 `config.json` 的最小投影,失败按类别报错(令牌路径用)。
fn load_config_view(path: Option<PathBuf>) -> Result<(PathBuf, ConfigSshView), TokenScopeError> {
    let path = path.ok_or(TokenScopeError::ConfigPathUnknown)?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(TokenScopeError::ConfigMissing { path });
        }
        Err(e) => {
            return Err(TokenScopeError::ConfigUnreadable {
                path,
                kind: e.kind(),
                detail: e.to_string(),
            });
        }
    };
    match serde_json::from_str::<ConfigSshView>(&content) {
        Ok(view) => Ok((path, view)),
        Err(e) => Err(TokenScopeError::ConfigInvalid {
            path,
            category: json_error_category(e.classify()),
            line: e.line(),
            column: e.column(),
        }),
    }
}

/// 从给定路径读取并解析 `config.json` 的最小投影。抽出便于单元测试注入临时文件。
///
/// 文件不存在 / 读取失败 / JSON 解析失败时返回空投影(空连接 + 空项目)—— 这是 MCP
/// project-id 路径的既有语义;令牌路径走 [`load_config_view`],失败按类别报错。
fn parse_config_from(path: Option<PathBuf>) -> ConfigSshView {
    load_config_view(path)
        .map(|(_, view)| view)
        .unwrap_or_default()
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

/// 按项目能力令牌过滤连接。任何对不上的情形都 fail closed,绝不回退到全部连接。
///
/// `config_path` 只用于拼报错(告诉用户 sidecar 读的是哪份配置)。
fn scope_connections_for_token(
    connections: Vec<SshConnection>,
    projects: &[ProjectScopeView],
    project_token: &str,
    config_path: &Path,
) -> Result<Vec<SshConnection>, TokenScopeError> {
    if project_token.trim().is_empty() {
        return Err(TokenScopeError::MissingToken);
    }
    let token_hint = token_hint(project_token);
    let mut matches = projects
        .iter()
        .filter(|project| project.ssh_cli_token.as_deref() == Some(project_token));
    let Some(project) = matches.next() else {
        return Err(TokenScopeError::UnknownToken {
            token_hint,
            path: config_path.to_path_buf(),
        });
    };
    // 重复映射先于「是否启用」判:哪怕其中一个项目已停用,也说明配置被改坏了。
    if matches.next().is_some() {
        return Err(TokenScopeError::AmbiguousToken {
            token_hint,
            path: config_path.to_path_buf(),
        });
    }
    if !project.ssh_mcp_enabled {
        return Err(TokenScopeError::ProjectDisabled { token_hint });
    }

    let scoped: Vec<SshConnection> = match project.ssh_connection_ids.as_ref() {
        None => connections,
        Some(ids) => connections
            .into_iter()
            .filter(|connection| ids.iter().any(|id| id == &connection.id))
            .collect(),
    };
    if scoped.is_empty() {
        return Err(TokenScopeError::EmptyScope { token_hint });
    }
    Ok(scoped)
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
            extra: Default::default(),
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
        assert_eq!(
            view.projects[0].ssh_connection_ids.as_deref(),
            Some(&["1".to_string()][..])
        );
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
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
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
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
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
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
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
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
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
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: Some(vec!["1".into(), "stale".into()]),
        }];
        let scoped = scope_connections(conns, &projects, Some("p1"));
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, "1");
    }

    /// 令牌路径的测试夹具:把 JSON 写进临时文件,返回 (路径, 解析出的投影)。
    fn token_fixture(name: &str, json: &str) -> (PathBuf, ConfigSshView) {
        let path = temp_file(name, json);
        let view = parse_config_from(Some(path.clone()));
        (path, view)
    }

    fn scope_token(
        path: &Path,
        view: ConfigSshView,
        token: &str,
    ) -> Result<Vec<SshConnection>, TokenScopeError> {
        scope_connections_for_token(view.ssh_connections, &view.projects, token, path)
    }

    #[test]
    fn capability_scope_rejects_unknown_token() {
        let json = r#"{
            "sshConnections": [
                {"id":"1","name":"prod","host":"h","port":22,"user":"u"}
            ],
            "projects": [
                {"id":"p1","sshMcpEnabled":true,"sshCliToken":"known-token","sshConnectionIds":["1"]}
            ]
        }"#;
        let (path, view) = token_fixture("unknown-token", json);

        let err =
            scope_token(&path, view, "unknown-token").expect_err("未知能力令牌必须 fail closed");

        assert!(
            matches!(err, TokenScopeError::UnknownToken { .. }),
            "{err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn capability_scope_rejects_blank_token() {
        let json = r#"{
            "sshConnections": [
                {"id":"1","name":"prod","host":"h","port":22,"user":"u"}
            ],
            "projects": [
                {"id":"p1","sshMcpEnabled":true,"sshCliToken":"   ","sshConnectionIds":["1"]}
            ]
        }"#;
        let (path, view) = token_fixture("blank-token", json);

        let err = scope_token(&path, view, "   ").expect_err("空白能力令牌必须 fail closed");

        assert_eq!(err, TokenScopeError::MissingToken);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn capability_scope_maps_token_to_exact_project_scope() {
        let json = r#"{
            "sshConnections": [
                {"id":"1","name":"prod","host":"prod","port":22,"user":"u"},
                {"id":"2","name":"dev","host":"dev","port":22,"user":"u"},
                {"id":"3","name":"ops","host":"ops","port":22,"user":"u"}
            ],
            "projects": [
                {"id":"p1","sshMcpEnabled":true,"sshCliToken":"token-a","sshConnectionIds":["1","3"]},
                {"id":"p2","sshMcpEnabled":true,"sshCliToken":"token-b","sshConnectionIds":["2"]}
            ]
        }"#;
        let (path, view) = token_fixture("valid-token", json);

        let scoped = scope_token(&path, view, "token-a").unwrap();
        let ids: Vec<&str> = scoped.iter().map(|c| c.id.as_str()).collect();

        assert_eq!(ids, ["1", "3"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn capability_scope_rejects_duplicate_token_mapping() {
        let json = r#"{
            "sshConnections": [
                {"id":"1","name":"prod","host":"prod","port":22,"user":"u"},
                {"id":"2","name":"dev","host":"dev","port":22,"user":"u"}
            ],
            "projects": [
                {"id":"p1","sshMcpEnabled":true,"sshCliToken":"duplicate","sshConnectionIds":["1"]},
                {"id":"p2","sshMcpEnabled":true,"sshCliToken":"duplicate","sshConnectionIds":["2"]}
            ]
        }"#;
        let (path, view) = token_fixture("duplicate-token", json);

        let err =
            scope_token(&path, view, "duplicate").expect_err("重复能力令牌映射必须 fail closed");

        assert!(
            matches!(err, TokenScopeError::AmbiguousToken { .. }),
            "{err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn capability_scope_rejects_token_for_disabled_project() {
        let json = r#"{
            "sshConnections": [
                {"id":"1","name":"prod","host":"prod","port":22,"user":"u"}
            ],
            "projects": [
                {
                    "id":"p1",
                    "sshMcpEnabled":false,
                    "sshCliToken":"disabled-token",
                    "sshConnectionIds":["1"]
                }
            ]
        }"#;
        let (path, view) = token_fixture("disabled-token", json);

        let err =
            scope_token(&path, view, "disabled-token").expect_err("已停用项目的残留令牌必须失效");

        assert!(
            matches!(err, TokenScopeError::ProjectDisabled { .. }),
            "{err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn capability_scope_rejects_duplicate_token_when_one_project_is_disabled() {
        let json = r#"{
            "sshConnections": [
                {"id":"1","name":"prod","host":"prod","port":22,"user":"u"},
                {"id":"2","name":"dev","host":"dev","port":22,"user":"u"}
            ],
            "projects": [
                {"id":"p1","sshMcpEnabled":true,"sshCliToken":"duplicate","sshConnectionIds":["1"]},
                {"id":"p2","sshMcpEnabled":false,"sshCliToken":"duplicate","sshConnectionIds":["2"]}
            ]
        }"#;
        let (path, view) = token_fixture("duplicate-disabled-token", json);

        let scoped = scope_token(&path, view, "duplicate");

        assert!(
            matches!(scoped, Err(TokenScopeError::AmbiguousToken { .. })),
            "禁用项目里的残留映射也必须让重复令牌 fail closed: {scoped:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // --- 令牌路径的报错分类(A2-9):每类都 Err,绝不退回全部连接 ---

    /// 范围解析成空(关联的连接全删了 / 整组关联的组空了)也是失败,不是「空列表成功」。
    #[test]
    fn capability_scope_with_no_remaining_connections_is_an_error() {
        let json = r#"{
            "sshConnections": [
                {"id":"1","name":"prod","host":"prod","port":22,"user":"u"}
            ],
            "projects": [
                {"id":"p1","sshMcpEnabled":true,"sshCliToken":"tok-empty","sshConnectionIds":["gone"]},
                {"id":"p2","sshMcpEnabled":true,"sshCliToken":"tok-none","sshConnectionIds":[]}
            ]
        }"#;
        let (path, view) = token_fixture("empty-scope", json);
        for token in ["tok-empty", "tok-none"] {
            let err = scope_token(&path, view.clone(), token).expect_err(token);
            assert!(matches!(err, TokenScopeError::EmptyScope { .. }), "{err:?}");
            assert!(err.to_string().contains("Link SSH"), "{err}");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn token_path_reports_missing_config_with_its_path() {
        let path = std::env::temp_dir()
            .join(format!("mt-core-cfg-missing-{}", std::process::id()))
            .join("config.json");
        let err = read_ssh_connections_for_token_at(Some(path.clone()), "tok").unwrap_err();
        assert_eq!(err, TokenScopeError::ConfigMissing { path: path.clone() });
        let text = err.to_string();
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains("does not exist"), "{text}");
        assert!(text.contains(DATA_DIR_ENV_HINT), "{text}");
    }

    #[test]
    fn token_path_reports_unlocatable_data_dir() {
        let err = read_ssh_connections_for_token_at(None, "tok").unwrap_err();
        assert_eq!(err, TokenScopeError::ConfigPathUnknown);
        assert!(err.to_string().contains(DATA_DIR_ENV_HINT));
    }

    /// 令牌缺失先于读盘判:配置在不在都报同一句「没带令牌」。
    #[test]
    fn blank_token_is_reported_before_touching_the_config() {
        for token in ["", "   ", "\t"] {
            assert_eq!(
                read_ssh_connections_for_token_at(None, token).unwrap_err(),
                TokenScopeError::MissingToken
            );
        }
        let text = TokenScopeError::MissingToken.to_string();
        assert!(text.contains("--project-token"), "{text}");
    }

    #[test]
    fn token_path_reports_unreadable_config_without_content() {
        // 把 config.json 造成一个目录:读文件必然失败(Windows 是 PermissionDenied,
        // Unix 是 IsADirectory 一类),要报「读不了」而不是「不存在」或「令牌不对」。
        let dir = std::env::temp_dir().join(format!(
            "mt-core-cfg-unreadable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("config.json");
        std::fs::create_dir_all(&path).unwrap();
        let err = read_ssh_connections_for_token_at(Some(path.clone()), "tok").unwrap_err();
        assert!(
            matches!(err, TokenScopeError::ConfigUnreadable { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("cannot read"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_path_reports_parse_errors_by_category_without_echoing_content() {
        // 语法错
        let path = temp_file("syntax", "{ not valid json");
        let err = read_ssh_connections_for_token_at(Some(path.clone()), "tok").unwrap_err();
        assert!(
            matches!(
                err,
                TokenScopeError::ConfigInvalid {
                    category: "JSON syntax",
                    ..
                }
            ),
            "{err:?}"
        );
        let _ = std::fs::remove_file(&path);

        // 结构错:serde 的 Display 会把出错的值原文带出来("invalid type: string
        // \"…\"")—— 那可能就是一段密文,报错里绝不能出现。
        let path = temp_file(
            "data",
            r#"{"sshConnections":[{"id":"1","name":"n","host":"h","port":"hunter2-secret","user":"u"}]}"#,
        );
        let err = read_ssh_connections_for_token_at(Some(path.clone()), "tok").unwrap_err();
        assert!(
            matches!(
                err,
                TokenScopeError::ConfigInvalid {
                    category: "unexpected structure",
                    line: 1,
                    ..
                }
            ),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(
            !text.contains("hunter2-secret"),
            "配置内容泄进了报错: {text}"
        );
        assert!(text.contains("line 1"), "{text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn token_errors_never_echo_the_full_token() {
        let token = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";
        let path = temp_file(
            "hint",
            r#"{"sshConnections":[{"id":"1","name":"n","host":"h","port":22,"user":"u"}],"projects":[]}"#,
        );
        let err = read_ssh_connections_for_token_at(Some(path.clone()), token).unwrap_err();
        let text = err.to_string();
        assert!(!text.contains(token), "完整令牌不该出现在报错里: {text}");
        assert!(text.contains("0f1e2d3c…"), "要留前几位方便对照: {text}");
        // 两条最常见的成因都要点到:过期的 SKILL.md、dev 实例生成的 SKILL.md
        assert!(text.contains("Link SSH"), "{text}");
        assert!(text.contains("MT_APP_DATA_DIR"), "{text}");
        assert!(text.contains(&path.display().to_string()), "{text}");
        let _ = std::fs::remove_file(&path);

        // 短令牌最多露一半
        assert_eq!(token_hint("abcd"), "ab…");
        assert_eq!(token_hint("x"), "…");
    }

    /// 令牌路径失败、MCP project-id 路径照旧:读不到就是空(语义不变)。
    #[test]
    fn legacy_project_path_still_degrades_to_empty() {
        let path = temp_file("legacy-invalid", "{ not valid json");
        assert!(read_ssh_connections_for_project_at(Some(path.clone()), Some("p1")).is_empty());
        assert!(read_ssh_connections_for_project_at(None, None).is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
