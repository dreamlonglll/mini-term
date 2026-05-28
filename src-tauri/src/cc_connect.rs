//! cc-connect 集成模块
//!
//! 为 mini-term 桥接到 cc-connect (chenhg5/cc-connect) 提供 8 个 Tauri command:
//! - probe / read_token: 健康检查 + 从 config.toml 读 [management].token
//! - start / stop / restart: 进程生命周期管理(mini-term 自己 spawn 时持有 Child)
//! - list_projects / import_project / unlink_project: 项目同步与关联
//!
//! 关键决策(详见 .trellis/tasks/05-28-embed-cc-connect-panel/prd.md):
//! - cc-connect 进程不需要 PTY,用 std::process::Command 即可(stdout/stderr null)
//! - 写回 config.toml 用 toml_edit 保留注释和顺序(沿用 hook_registry / ssh_mcp_registry 既有模式)
//! - 创建新 [[projects]] 后必须 POST /api/v1/restart 才生效(/reload 对全新项目无效)
//! - mini-term 关闭不联动 kill cc-connect(IM 持续可用) → 不在 Drop 里 kill

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

const DEFAULT_PORT: u16 = 9820;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_AGENT_TYPE: &str = "claudecode";

/// Tauri managed state:仅追踪 mini-term 自己 spawn 的 cc-connect Child 句柄。
/// 不缓存 probe 结果(每次走 HTTP 实时);不接管"用户手动启动"的进程。
#[derive(Default, Clone)]
pub struct CcConnectManager {
    child: Arc<Mutex<Option<Child>>>,
}

impl CcConnectManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn own_pid(&self) -> Option<u32> {
        self.child
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|child| child.id()))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CcConnectStatus {
    pub running: bool,
    pub port: u16,
    pub version: Option<String>,
    pub own_pid: Option<u32>,
    /// 探活失败时的友好诊断(token 缺失 / 端口不通 / 配置文件不存在等)
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CcProject {
    pub name: String,
    pub work_dir: Option<String>,
    pub agent_type: Option<String>,
    pub has_platform: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectRequest {
    pub name: String,
    pub work_dir: String,
    pub agent_type: Option<String>,
}

fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cc-connect").join("config.toml"))
}

fn resolve_config_path(override_path: Option<&str>) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    default_config_path()
        .ok_or_else(|| "无法定位 cc-connect 配置目录 (~/.cc-connect/config.toml)".to_string())
}

fn read_doc(config_path: &PathBuf) -> Result<DocumentMut, String> {
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| format!("读取 {} 失败: {}", config_path.display(), e))?;
    content
        .parse::<DocumentMut>()
        .map_err(|e| format!("解析 config.toml 失败: {}", e))
}

fn read_token_port(config_path: &PathBuf) -> Result<(String, u16), String> {
    let doc = read_doc(config_path)?;
    let mgmt = doc
        .get("management")
        .and_then(|i| i.as_table())
        .ok_or_else(|| "config.toml 缺少 [management] 段".to_string())?;
    let token = mgmt
        .get("token")
        .and_then(|i| i.as_str())
        .ok_or_else(|| "[management].token 未配置 (执行 cc-connect web 自动生成)".to_string())?
        .to_string();
    if token.is_empty() {
        return Err("[management].token 为空 (执行 cc-connect web 自动生成)".to_string());
    }
    let port = mgmt
        .get("port")
        .and_then(|i| i.as_integer())
        .map(|n| n as u16)
        .unwrap_or(DEFAULT_PORT);
    Ok((token, port))
}

fn build_api_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{}{}", port, path)
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build()
}

fn http_get_json(url: &str, token: &str) -> Result<serde_json::Value, String> {
    let resp = http_agent()
        .get(url)
        .set("Authorization", &format!("Bearer {}", token))
        .call()
        .map_err(|e| format!("GET {} 失败: {}", url, e))?;
    resp.into_json::<serde_json::Value>()
        .map_err(|e| format!("解析响应 JSON 失败: {}", e))
}

fn http_post_json(
    url: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let resp = http_agent()
        .post(url)
        .set("Authorization", &format!("Bearer {}", token))
        .send_json(body.clone())
        .map_err(|e| format!("POST {} 失败: {}", url, e))?;
    resp.into_json::<serde_json::Value>()
        .map_err(|e| format!("解析响应 JSON 失败: {}", e))
}

fn http_delete(url: &str, token: &str) -> Result<(), String> {
    http_agent()
        .delete(url)
        .set("Authorization", &format!("Bearer {}", token))
        .call()
        .map_err(|e| format!("DELETE {} 失败: {}", url, e))?;
    Ok(())
}

/// 项目名 URL path 编码。保守做法,只保留 unreserved 字符,其余按 RFC3986 百分号编码,
/// 避免拉一个 url crate。项目名通常仅含 [A-Za-z0-9_-],极少触发编码分支。
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "%20".to_string(),
            _ => {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf)
                    .bytes()
                    .map(|b| format!("%{:02X}", b))
                    .collect()
            }
        })
        .collect()
}

// ====================== Tauri Commands ======================

#[tauri::command]
pub fn cc_connect_probe(
    state: tauri::State<'_, CcConnectManager>,
    config_path: Option<String>,
) -> CcConnectStatus {
    let path = match resolve_config_path(config_path.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            return CcConnectStatus {
                running: false,
                port: DEFAULT_PORT,
                version: None,
                own_pid: state.own_pid(),
                diagnostic: Some(e),
            };
        }
    };
    let (token, port) = match read_token_port(&path) {
        Ok(v) => v,
        Err(e) => {
            return CcConnectStatus {
                running: false,
                port: DEFAULT_PORT,
                version: None,
                own_pid: state.own_pid(),
                diagnostic: Some(e),
            };
        }
    };
    let url = build_api_url(port, "/api/v1/status");
    match http_get_json(&url, &token) {
        Ok(json) => CcConnectStatus {
            running: true,
            port,
            version: json
                .pointer("/data/version")
                .and_then(|v| v.as_str())
                .map(String::from),
            own_pid: state.own_pid(),
            diagnostic: None,
        },
        Err(e) => CcConnectStatus {
            running: false,
            port,
            version: None,
            own_pid: state.own_pid(),
            diagnostic: Some(e),
        },
    }
}

#[tauri::command]
pub fn cc_connect_read_token(config_path: Option<String>) -> Result<String, String> {
    let path = resolve_config_path(config_path.as_deref())?;
    let (token, _port) = read_token_port(&path)?;
    Ok(token)
}

#[tauri::command]
pub fn cc_connect_start(
    state: tauri::State<'_, CcConnectManager>,
    exe_path: String,
    config_path: Option<String>,
    extra_args: Option<Vec<String>>,
) -> Result<u32, String> {
    let mut guard = state.child.lock().map_err(|e| e.to_string())?;
    if let Some(child) = guard.as_mut() {
        if let Ok(None) = child.try_wait() {
            return Err(format!(
                "cc-connect 已由 mini-term 启动 (pid={})",
                child.id()
            ));
        }
        *guard = None;
    }
    let mut cmd = Command::new(&exe_path);
    if let Some(cfg) = config_path.as_deref() {
        if !cfg.is_empty() {
            cmd.args(["--config", cfg]);
        }
    }
    if let Some(args) = extra_args {
        for a in args {
            if !a.is_empty() {
                cmd.arg(a);
            }
        }
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW = 0x08000000,避免弹出黑色控制台窗口
        cmd.creation_flags(0x08000000);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("启动 cc-connect 失败 ({}): {}", exe_path, e))?;
    let pid = child.id();
    *guard = Some(child);
    Ok(pid)
}

#[tauri::command]
pub fn cc_connect_stop(state: tauri::State<'_, CcConnectManager>) -> Result<(), String> {
    let mut guard = state.child.lock().map_err(|e| e.to_string())?;
    if let Some(mut child) = guard.take() {
        let _ = child.kill();
        let _ = child.wait();
        Ok(())
    } else {
        Err("cc-connect 不是由 mini-term 启动的,无法停止 (请到对应进程处自行关闭)".to_string())
    }
}

#[tauri::command]
pub fn cc_connect_restart(
    state: tauri::State<'_, CcConnectManager>,
    exe_path: Option<String>,
    config_path: Option<String>,
    extra_args: Option<Vec<String>>,
) -> Result<(), String> {
    // 1. 优先 HTTP /api/v1/restart
    let api_result = (|| -> Result<(), String> {
        let path = resolve_config_path(config_path.as_deref())?;
        let (token, port) = read_token_port(&path)?;
        let url = build_api_url(port, "/api/v1/restart");
        http_post_json(&url, &token, &serde_json::json!({}))?;
        Ok(())
    })();
    if api_result.is_ok() {
        return Ok(());
    }
    // 2. fallback: 仅对自己 spawn 的做 kill+respawn
    {
        let mut guard = state.child.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        } else {
            return Err(format!(
                "HTTP restart 失败且 cc-connect 不是由 mini-term 启动 (原因: {})",
                api_result.unwrap_err()
            ));
        }
    }
    let exe = exe_path.ok_or_else(|| "fallback restart 需要 exe_path".to_string())?;
    let _ = cc_connect_start(state, exe, config_path, extra_args)?;
    Ok(())
}

#[tauri::command]
pub fn cc_connect_list_projects(config_path: Option<String>) -> Result<Vec<CcProject>, String> {
    let path = resolve_config_path(config_path.as_deref())?;
    let (token, port) = read_token_port(&path)?;
    let url = build_api_url(port, "/api/v1/projects");
    let json = http_get_json(&url, &token)?;
    let arr = json
        .pointer("/data/projects")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "响应缺少 data.projects 数组".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for p in arr {
        let name = p
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let work_dir = p
            .get("work_dir")
            .and_then(|v| v.as_str())
            .map(String::from);
        let agent_type = p
            .get("agent_type")
            .and_then(|v| v.as_str())
            .map(String::from);
        let has_platform = p
            .get("platforms")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        out.push(CcProject {
            name,
            work_dir,
            agent_type,
            has_platform,
        });
    }
    Ok(out)
}

#[tauri::command]
pub fn cc_connect_import_project(
    req: ImportProjectRequest,
    config_path: Option<String>,
) -> Result<(), String> {
    let path = resolve_config_path(config_path.as_deref())?;
    let (token, port) = read_token_port(&path)?;
    let mut doc = read_doc(&path)?;

    let projects_item = doc
        .entry("projects")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    let projects = projects_item
        .as_array_of_tables_mut()
        .ok_or_else(|| "config.toml 的 projects 不是 array of tables".to_string())?;

    if projects.iter().any(|t| {
        t.get("name")
            .and_then(|i| i.as_str())
            .map(|n| n == req.name)
            .unwrap_or(false)
    }) {
        return Err(format!("cc-connect 已存在同名项目 \"{}\"", req.name));
    }

    let agent_type = req.agent_type.unwrap_or_else(|| DEFAULT_AGENT_TYPE.to_string());

    let mut new_proj = Table::new();
    new_proj["name"] = value(req.name.clone());

    let mut agent = Table::new();
    agent["type"] = value(agent_type);

    let mut options = Table::new();
    options["work_dir"] = value(req.work_dir.clone());
    agent["options"] = Item::Table(options);

    new_proj["agent"] = Item::Table(agent);

    projects.push(new_proj);

    std::fs::write(&path, doc.to_string())
        .map_err(|e| format!("写回 {} 失败: {}", path.display(), e))?;

    let url = build_api_url(port, "/api/v1/restart");
    http_post_json(&url, &token, &serde_json::json!({}))
        .map_err(|e| format!("写入成功但 restart cc-connect 失败,请手动重启 ({})", e))?;
    Ok(())
}

#[tauri::command]
pub fn cc_connect_unlink_project(
    name: String,
    config_path: Option<String>,
) -> Result<(), String> {
    let path = resolve_config_path(config_path.as_deref())?;
    let (token, port) = read_token_port(&path)?;
    let del_url = build_api_url(port, &format!("/api/v1/projects/{}", urlencode(&name)));
    http_delete(&del_url, &token)?;
    let restart_url = build_api_url(port, "/api/v1/restart");
    http_post_json(&restart_url, &token, &serde_json::json!({}))
        .map_err(|e| format!("DELETE 成功但 restart cc-connect 失败,请手动重启 ({})", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_safe_chars() {
        assert_eq!(urlencode("foo-bar_1"), "foo-bar_1");
        assert_eq!(urlencode("a.b~c"), "a.b~c");
    }

    #[test]
    fn url_encode_special() {
        assert_eq!(urlencode("foo bar"), "foo%20bar");
        assert_eq!(urlencode("a/b"), "a%2Fb");
    }

    #[test]
    fn import_appends_to_array_of_tables_preserving_comments() {
        let original = r#"# user-level comment
[[projects]]
name = "existing"

[projects.agent]
type = "claudecode"
"#;
        let mut doc: DocumentMut = original.parse().unwrap();
        let projects_item = doc
            .entry("projects")
            .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
        let projects = projects_item.as_array_of_tables_mut().unwrap();

        let mut new_proj = Table::new();
        new_proj["name"] = value("imported");
        let mut agent = Table::new();
        agent["type"] = value("claudecode");
        let mut options = Table::new();
        options["work_dir"] = value("D:\\Git\\mini-term");
        agent["options"] = Item::Table(options);
        new_proj["agent"] = Item::Table(agent);
        projects.push(new_proj);

        let serialized = doc.to_string();
        assert!(serialized.contains("# user-level comment"));
        assert!(serialized.contains("name = \"existing\""));
        assert!(serialized.contains("name = \"imported\""));

        // round-trip:重新解析,读 work_dir 字段必须等于原始值,不关心字符串引号风格
        let reparsed: DocumentMut = serialized.parse().unwrap();
        let projects = reparsed["projects"].as_array_of_tables().unwrap();
        assert_eq!(projects.len(), 2);
        let imported = projects.get(1).unwrap();
        let work_dir = imported["agent"]["options"]["work_dir"]
            .as_str()
            .unwrap();
        assert_eq!(work_dir, "D:\\Git\\mini-term");
    }

    #[test]
    fn import_creates_array_when_missing() {
        let original = r#"[management]
enabled = true
"#;
        let mut doc: DocumentMut = original.parse().unwrap();
        let projects_item = doc
            .entry("projects")
            .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
        let projects = projects_item.as_array_of_tables_mut().unwrap();
        let mut t = Table::new();
        t["name"] = value("first");
        projects.push(t);
        let s = doc.to_string();
        assert!(s.contains("[[projects]]"));
        assert!(s.contains("name = \"first\""));
        assert!(s.contains("enabled = true"));
    }

    #[test]
    fn duplicate_name_detected() {
        let original = r#"
[[projects]]
name = "dup"
"#;
        let doc: DocumentMut = original.parse().unwrap();
        let projects = doc
            .get("projects")
            .and_then(|i| i.as_array_of_tables())
            .unwrap();
        let has = projects.iter().any(|t| {
            t.get("name")
                .and_then(|i| i.as_str())
                .map(|n| n == "dup")
                .unwrap_or(false)
        });
        assert!(has);
    }
}
