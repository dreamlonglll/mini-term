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
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

const DEFAULT_PORT: u16 = 9820;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_AGENT_TYPE: &str = "claudecode";

/// 占位平台:cc-connect 的 config.validate() 强制每个 [[projects]] 至少有一个 [[projects.platforms]],
/// 否则冷启动直接 os.Exit(1)("config: projects[N] needs at least one [[projects.platforms]]")。
/// 导入时拿不到真实 IM 凭据 → 注入一个「冷启动安全」的占位平台,让写出的 config.toml 永远能冷启动,
/// 用户后续在 cc-connect Dashboard 把它替换为真实平台。
///
/// 选 telegram 的依据(经源码 + 隔离实测确认,cc-connect v1.3.2):工厂 New() 仅校验 token 非空,
/// Start() 把拨号丢进 goroutine 后 return nil,假 token 只会后台退避重连、绝不返回 error 让进程崩。
/// ⚠ 绝不能用 discord 占位:其 Start() 同步 session.Open() 并返回 error,作为单平台时会拖垮 engine→os.Exit。
/// ⚠ token 必须非空:空串会让 telegram 工厂报错 → main.go CreatePlatform 失败 → os.Exit(1)。
const PLACEHOLDER_PLATFORM_TYPE: &str = "telegram";
const PLACEHOLDER_PLATFORM_TOKEN: &str = "0:MINITERM_PLACEHOLDER_REPLACE_IN_DASHBOARD";

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

/// 导入项目结果:toml_written 必为 true(否则直接返 Err);restart_ok 可为 false,
/// 让前端按 tomlWritten / restartOk 分别决策写 projectLinks 与 toast 文案,
/// 避免"toml 已写但 restart 失败 → 前端不写 projectLinks → 半同步态"。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectResult {
    pub name: String,
    pub toml_written: bool,
    pub restart_ok: bool,
    pub restart_error: Option<String>,
}

/// 解除关联结果:类似 ImportProjectResult,deleted_ok 必为 true(否则返 Err);
/// restart_ok 可为 false,前端仍删本地 projectLinks 摆脱 broken 状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlinkProjectResult {
    pub name: String,
    pub deleted_ok: bool,
    pub restart_ok: bool,
    pub restart_error: Option<String>,
}

/// 批量导入结果:一次写盘 + 仅重启一次。imported/skipped 按 req 顺序分类,
/// 前端用自己持有的 projectId→name 映射写 projectLinks(不依赖此返回顺序)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchImportResult {
    pub imported: Vec<String>,
    pub skipped: Vec<String>,
    pub toml_written: bool,
    pub restart_ok: bool,
    pub restart_error: Option<String>,
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

/// 构造一个 [[projects]] 表(name + agent.type + agent.options.work_dir + 占位 platform),单/批量导入共用。
///
/// 必带一个占位 [[projects.platforms]](见 PLACEHOLDER_PLATFORM_*):cc-connect 校验阶段强制每个项目
/// 至少一个平台,否则冷启动 os.Exit(1)。不带占位平台的导入正是上一版被删的根因。
fn make_project_table(name: &str, work_dir: &str, agent_type: &str) -> Table {
    let mut new_proj = Table::new();
    new_proj["name"] = value(name);
    let mut agent = Table::new();
    agent["type"] = value(agent_type);
    let mut options = Table::new();
    options["work_dir"] = value(work_dir);
    agent["options"] = Item::Table(options);
    new_proj["agent"] = Item::Table(agent);

    // 占位平台:type = telegram,options.token 为非空假值。保证冷启动安全(详见常量注释)。
    let mut platform = Table::new();
    platform["type"] = value(PLACEHOLDER_PLATFORM_TYPE);
    let mut platform_options = Table::new();
    platform_options["token"] = value(PLACEHOLDER_PLATFORM_TOKEN);
    platform["options"] = Item::Table(platform_options);
    let mut platforms = ArrayOfTables::new();
    platforms.push(platform);
    new_proj["platforms"] = Item::ArrayOfTables(platforms);

    new_proj
}

/// Windows: 像终端一样按 PATH × PATHEXT 解析可执行文件,弥补 `Command::new` 解析裸名
/// 只补 `.exe`、不读 `PATHEXT` 的缺陷(cc-connect 常以 npm 脚本壳安装:cc-connect.cmd / .ps1,
/// 无原生 .exe,会触发 program not found)。返回 (program, prefix_args):
/// - .exe/.com/.cmd/.bat → (解析到的绝对路径, [])  (.cmd/.bat 由 std 自行经 cmd.exe 拉起)
/// - .ps1 脚本壳         → ("powershell", ["-NoProfile","-ExecutionPolicy","Bypass","-File", 路径])
///
/// 找不到则返 Err,提示用户到「连接」弹窗填绝对路径。
#[cfg(windows)]
fn resolve_windows_program(exe: &str) -> Result<(String, Vec<String>), String> {
    use std::path::Path;

    // PATHEXT 兜底默认值;额外补 .PS1(PATHEXT 通常不含,但我们能经 powershell 跑)
    let mut exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if !exts.iter().any(|e| e.eq_ignore_ascii_case(".PS1")) {
        exts.push(".PS1".to_string());
    }

    // 把一个具体文件包装成 (program, prefix_args)
    let wrap = |p: PathBuf| -> (String, Vec<String>) {
        let is_ps1 = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ps1"))
            .unwrap_or(false);
        let s = p.to_string_lossy().to_string();
        if is_ps1 {
            (
                "powershell".to_string(),
                vec![
                    "-NoProfile".to_string(),
                    "-ExecutionPolicy".to_string(),
                    "Bypass".to_string(),
                    "-File".to_string(),
                    s,
                ],
            )
        } else {
            (s, vec![])
        }
    };

    // 1) 用户给了带路径分隔符的目标(绝对/相对路径):直接用,漏扩展名则补 PATHEXT
    if exe.contains('\\') || exe.contains('/') {
        let p = PathBuf::from(exe);
        if p.is_file() {
            return Ok(wrap(p));
        }
        if Path::new(exe).extension().is_none() {
            for ext in &exts {
                let cand = PathBuf::from(format!("{}{}", exe, ext));
                if cand.is_file() {
                    return Ok(wrap(cand));
                }
            }
        }
        // 仍找不到:原样交给 Command,保留其原生错误语义
        return Ok((exe.to_string(), vec![]));
    }

    // 2) 裸名(可能自带扩展名)→ 扫 PATH 各目录
    let has_ext = Path::new(exe).extension().is_some();
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if has_ext {
                let cand = dir.join(exe);
                if cand.is_file() {
                    return Ok(wrap(cand));
                }
            } else {
                for ext in &exts {
                    let cand = dir.join(format!("{}{}", exe, ext));
                    if cand.is_file() {
                        return Ok(wrap(cand));
                    }
                }
            }
        }
    }

    Err(format!(
        "在 PATH 中找不到 \"{}\"(已按 PATHEXT 尝试 .exe/.cmd/.bat/.ps1 等);请在「连接」弹窗的「可执行文件」填写绝对路径",
        exe
    ))
}

/// 杀掉 mini-term 启动的 cc-connect。Windows 下 child 可能是脚本壳(cmd/powershell),
/// 真正的 node/cc-connect 是其子孙进程,`child.kill()` 杀不到 → 用 `taskkill /T /F` 杀整棵树;
/// 末尾 `child.kill()` + `wait()` 兜底确保壳进程结束并回收句柄。原生 .exe 时 taskkill 无子进程亦无害。
fn kill_child_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ====================== Tauri Commands ======================

/// async:probe 每 5s 轮询且内部是阻塞式 ureq HTTP(最长 5s 超时)。Tauri 把同步 command 放
/// 主线程执行,阻塞 HTTP 会冻结 UI;声明为 async 让其在异步运行时工作线程上跑,主线程不被阻塞。
#[tauri::command]
pub async fn cc_connect_probe(
    state: tauri::State<'_, CcConnectManager>,
    config_path: Option<String>,
) -> Result<CcConnectStatus, String> {
    let path = match resolve_config_path(config_path.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            return Ok(CcConnectStatus {
                running: false,
                port: DEFAULT_PORT,
                version: None,
                own_pid: state.own_pid(),
                diagnostic: Some(e),
            });
        }
    };
    let (token, port) = match read_token_port(&path) {
        Ok(v) => v,
        Err(e) => {
            return Ok(CcConnectStatus {
                running: false,
                port: DEFAULT_PORT,
                version: None,
                own_pid: state.own_pid(),
                diagnostic: Some(e),
            });
        }
    };
    let url = build_api_url(port, "/api/v1/status");
    Ok(match http_get_json(&url, &token) {
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
    })
}

#[tauri::command]
pub fn cc_connect_read_token(config_path: Option<String>) -> Result<String, String> {
    let path = resolve_config_path(config_path.as_deref())?;
    let (token, _port) = read_token_port(&path)?;
    Ok(token)
}

/// 解析 config.toml 的绝对路径(未传 config_path 时落到默认 ~/.cc-connect/config.toml)。
/// 供前端"编辑配置文件"在用户未填写路径时打开默认配置。
#[tauri::command]
pub fn cc_connect_config_path(config_path: Option<String>) -> Result<String, String> {
    let path = resolve_config_path(config_path.as_deref())?;
    Ok(path.to_string_lossy().to_string())
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
    // Windows 下 exe_path 可能是 npm 脚本壳(cc-connect.cmd / .ps1)或裸名,Command::new 解析裸名
    // 只补 .exe 会 program not found;这里按 PATH × PATHEXT 像终端一样解析,.ps1 包一层 powershell。
    #[cfg(windows)]
    let (program, prefix_args) = resolve_windows_program(&exe_path)
        .map_err(|e| format!("启动 cc-connect 失败 ({}): {}", exe_path, e))?;
    #[cfg(not(windows))]
    let (program, prefix_args): (String, Vec<String>) = (exe_path.clone(), Vec::new());

    let mut cmd = Command::new(&program);
    for a in &prefix_args {
        cmd.arg(a);
    }
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
        kill_child_tree(&mut child);
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
    let api_err = api_result.unwrap_err();
    // 2. fallback: 必须先校验 exe_path 再杀 child,
    //    否则 exe_path = None 时 child 已杀但没法 spawn,state 已清空 → 用户陷入"不是 mini-term 启动"半同步态。
    let exe = exe_path.ok_or_else(|| format!(
        "HTTP restart 失败 ({}),且未提供 exe_path 用于 fallback 重启;请在设置里填写 cc-connect 路径后手动重启",
        api_err,
    ))?;
    {
        let mut guard = state.child.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = guard.take() {
            kill_child_tree(&mut child);
        } else {
            return Err(format!(
                "HTTP restart 失败且 cc-connect 不是由 mini-term 启动 (原因: {})",
                api_err,
            ));
        }
    }
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
) -> Result<ImportProjectResult, String> {
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
    projects.push(make_project_table(&req.name, &req.work_dir, &agent_type));

    crate::fs::atomic_write(&path, doc.to_string().as_bytes())
        .map_err(|e| format!("写回 {} 失败: {}", path.display(), e))?;

    // toml 已写盘 → 无论 restart 成败都返 Ok(ImportProjectResult { toml_written: true, ... }),
    // 让前端按 restartOk 分支决策:成功 → 写 projectLinks + 成功 toast;
    // 失败 → 仍写 projectLinks(避免半同步态)+ 警告 toast 提示用户重启 cc-connect 生效。
    let url = build_api_url(port, "/api/v1/restart");
    let (restart_ok, restart_error) = match http_post_json(&url, &token, &serde_json::json!({})) {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e)),
    };
    Ok(ImportProjectResult {
        name: req.name,
        toml_written: true,
        restart_ok,
        restart_error,
    })
}

/// 批量导入多个项目:一次性把所有新 [[projects]] 写入 config.toml,然后只 POST 一次 /restart。
/// 相比逐个调 cc_connect_import_project,避免 N 次 restart 多次断开 IM active sessions。
/// 名称唯一性由前端在调用前 resolve(与现有项目 + 批次内部去重);后端对已存在同名做防御性跳过。
#[tauri::command]
pub fn cc_connect_import_projects(
    reqs: Vec<ImportProjectRequest>,
    config_path: Option<String>,
) -> Result<BatchImportResult, String> {
    let path = resolve_config_path(config_path.as_deref())?;
    let (token, port) = read_token_port(&path)?;
    let mut doc = read_doc(&path)?;

    let projects_item = doc
        .entry("projects")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    let projects = projects_item
        .as_array_of_tables_mut()
        .ok_or_else(|| "config.toml 的 projects 不是 array of tables".to_string())?;

    let mut existing: HashSet<String> = projects
        .iter()
        .filter_map(|t| t.get("name").and_then(|i| i.as_str()).map(String::from))
        .collect();

    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    for req in reqs {
        if existing.contains(&req.name) {
            skipped.push(req.name);
            continue;
        }
        let agent_type = req
            .agent_type
            .clone()
            .unwrap_or_else(|| DEFAULT_AGENT_TYPE.to_string());
        projects.push(make_project_table(&req.name, &req.work_dir, &agent_type));
        existing.insert(req.name.clone());
        imported.push(req.name);
    }

    // 没有任何新项目(全部已存在)→ 不写盘不重启,避免无谓断开 IM
    if imported.is_empty() {
        return Ok(BatchImportResult {
            imported,
            skipped,
            toml_written: false,
            restart_ok: true,
            restart_error: None,
        });
    }

    crate::fs::atomic_write(&path, doc.to_string().as_bytes())
        .map_err(|e| format!("写回 {} 失败: {}", path.display(), e))?;

    let url = build_api_url(port, "/api/v1/restart");
    let (restart_ok, restart_error) = match http_post_json(&url, &token, &serde_json::json!({})) {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e)),
    };
    Ok(BatchImportResult {
        imported,
        skipped,
        toml_written: true,
        restart_ok,
        restart_error,
    })
}

#[tauri::command]
pub fn cc_connect_unlink_project(
    name: String,
    config_path: Option<String>,
) -> Result<UnlinkProjectResult, String> {
    let path = resolve_config_path(config_path.as_deref())?;
    let (token, port) = read_token_port(&path)?;
    let del_url = build_api_url(port, &format!("/api/v1/projects/{}", urlencode(&name)));
    http_delete(&del_url, &token)?;
    // DELETE 已成功 → 同 import 一样,restart 即使失败也返 Ok,
    // 让前端仍删本地 projectLinks 摆脱 broken 红 icon,只 toast 提示重启 cc-connect。
    let restart_url = build_api_url(port, "/api/v1/restart");
    let (restart_ok, restart_error) = match http_post_json(&restart_url, &token, &serde_json::json!({})) {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e)),
    };
    Ok(UnlinkProjectResult {
        name,
        deleted_ok: true,
        restart_ok,
        restart_error,
    })
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

    #[test]
    fn make_project_table_injects_placeholder_platform() {
        // 守住冷启动回归线:make_project_table 产物必须带至少一个非空 token 的 [[projects.platforms]],
        // 否则 cc-connect config.validate() 会让进程 os.Exit(1)(上一版被删的根因)。
        let t = make_project_table("proj", "D:\\Git\\x", "claudecode");
        let platforms = t["platforms"]
            .as_array_of_tables()
            .expect("应有 platforms array of tables");
        assert_eq!(platforms.len(), 1);
        let p0 = platforms.get(0).unwrap();
        assert_eq!(p0["type"].as_str().unwrap(), "telegram");
        let token = p0["options"]["token"].as_str().unwrap();
        assert!(!token.is_empty(), "占位 token 必须非空(空串会让 telegram 工厂 os.Exit)");

        // round-trip:序列化后重新解析仍能读到平台 type
        let mut doc = DocumentMut::new();
        let mut arr = ArrayOfTables::new();
        arr.push(t);
        doc["projects"] = Item::ArrayOfTables(arr);
        let reparsed: DocumentMut = doc.to_string().parse().unwrap();
        let ptype = reparsed["projects"].as_array_of_tables().unwrap()
            .get(0).unwrap()["platforms"]
            .as_array_of_tables().unwrap()
            .get(0).unwrap()["type"]
            .as_str().unwrap();
        assert_eq!(ptype, "telegram");
    }
}
