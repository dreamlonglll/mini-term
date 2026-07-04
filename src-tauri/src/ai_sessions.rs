use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_CLAUDE_SESSION_FILES_TO_SCAN: usize = 300;
const MAX_CODEX_SESSION_FILES_TO_SCAN: usize = 500;
// WSL 侧经 \\wsl$ 走 9P 协议,逐文件读慢(毫秒级往返),上限下调。
const MAX_WSL_CLAUDE_SESSION_FILES_TO_SCAN: usize = 100;
const MAX_WSL_CODEX_SESSION_FILES_TO_SCAN: usize = 200;
const MAX_SESSIONS_PER_SOURCE: usize = 80;
const MAX_TOTAL_SESSIONS: usize = 120;
const SESSION_CACHE_TTL: Duration = Duration::from_secs(2);
// WSL 扫描代价高(9P + 可能触发 VM 冷启动),TTL 放宽;手动刷新走 force 绕过。
const WSL_SESSION_CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct CachedSessions {
    loaded_at: Instant,
    sessions: Vec<AiSession>,
}

static SESSION_CACHE: OnceLock<Mutex<HashMap<String, CachedSessions>>> = OnceLock::new();

fn session_cache() -> &'static Mutex<HashMap<String, CachedSessions>> {
    SESSION_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSession {
    pub id: String,
    pub session_type: String, // "claude" | "codex"
    pub title: String,
    pub timestamp: String, // ISO 8601
    /// 会话来源:Some = 该 WSL 发行版内的会话,None = Windows 宿主会话。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wsl_distro: Option<String>,
}

/// 获取用户 home 目录
fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// cwd 比较用的路径语义。Claude/Codex 的会话文件里记录的是运行时 cwd,
/// Windows 宿主与 WSL 发行版内的 cwd 语义不同,匹配时必须用对应的 normalize。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStyle {
    /// Windows 语义:`/`→`\` + lowercase + 去尾部 `\`
    Windows,
    /// Unix 语义:保留 `/` + lowercase + 去尾部 `/`。
    /// 不能复用 Windows 版(它把 `/` 换成 `\`,WSL cwd 会永不匹配)。
    /// lowercase 是因为 drvfs(/mnt/*)默认大小写不敏感,同一目录可能以不同大小写出现。
    Unix,
}

impl PathStyle {
    fn normalize(self, path: &str) -> String {
        match self {
            PathStyle::Windows => normalize_path(path),
            PathStyle::Unix => normalize_unix_path(path),
        }
    }
}

/// 将项目路径编码为 Claude 项目目录名。
/// Claude Code 会把 cwd 中**所有非字母数字字符**(含 `:` `\` `/` `.` 空格及中文等)
/// 统一替换为 `-`,而非仅替换路径分隔符。
/// 例如 `D:\Git\bhyt-一体机` → `D--Git-bhyt----`;
/// 对 unix cwd 同样成立:`/mnt/d/git/foo` → `-mnt-d-git-foo`。
fn encode_project_path(project_path: &str) -> String {
    project_path
        .trim_end_matches(['/', '\\'])
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// 目录名是否为编码名的「变体」:大小写不同(drvfs 大小写不敏感,WSL 内
/// `cd /mnt/d/GIT/foo` 也能进同一目录)或仅多出尾部 `-`(带尾部斜杠的同一项目)。
/// 编码有损,变体命中后仍需读 jsonl 内真实 cwd 精确校验,防止吃进兄弟项目。
fn is_encoded_variant(dir_name: &str, encoded: &str) -> bool {
    // encoded 只含 ASCII 字母数字与 `-`,lowercase 后 byte 长度不变,切片安全
    let dn = dir_name.to_lowercase();
    let en = encoded.to_lowercase();
    dn.starts_with(&en) && dn[en.len()..].chars().all(|c| c == '-')
}

/// 在指定 `.claude/projects` 目录下查找项目对应的所有 Claude 项目目录
/// (含尾部斜杠 / 大小写差异导致的变体)
fn find_claude_project_dirs_in(
    projects_dir: &Path,
    project_path: &str,
    style: PathStyle,
) -> Vec<PathBuf> {
    let encoded = encode_project_path(project_path);
    let normalized_project = style.normalize(project_path);

    let entries = match fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if dir_name == encoded {
            // 名称完全一致:直接采用
            dirs.push(path);
        } else if is_encoded_variant(dir_name, &encoded) {
            // 变体:可能是「同一项目的尾部斜杠/大小写变体」,也可能是「前缀相同的不同项目」
            // (如 `D:\Git\bhyt` 会前缀匹配到 `D:\Git\bhyt-一体机` 的目录 `D--Git-bhyt----`),
            // 读取会话文件内的真实 cwd 做精确校验,避免把兄弟项目的会话也吃进来。
            if dir_matches_project(&path, &normalized_project, style) {
                dirs.push(path);
            }
        }
    }

    dirs
}

/// Windows 宿主视角:查找项目路径对应的所有 Claude 项目目录
fn find_claude_project_dirs(project_path: &str) -> Vec<PathBuf> {
    let home = match home_dir() {
        Some(h) => h,
        None => return vec![],
    };
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.exists() {
        return vec![];
    }
    find_claude_project_dirs_in(&projects_dir, project_path, PathStyle::Windows)
}

/// 读取 Claude 项目目录下任一 jsonl 的 `cwd` 字段,确认其是否就是目标项目。
/// 用于消除目录名编码有损导致的前缀误匹配。
fn dir_matches_project(dir: &Path, normalized_project: &str, style: PathStyle) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        for line in reader.lines().take(5) {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
                    return style.normalize(cwd) == normalized_project;
                }
            }
        }
    }
    false
}

/// 路径统一化(小写 + 反斜杠,去尾部斜杠),用于 Windows 路径比较
fn normalize_path(path: &str) -> String {
    path.replace('/', "\\")
        .to_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

/// Unix 语义路径统一化(小写 + 保留 `/`,去尾部 `/`),用于 WSL 内 cwd 比较
fn normalize_unix_path(path: &str) -> String {
    path.to_lowercase().trim_end_matches('/').to_string()
}

// ─── WSL 路径推导 ──────────────────────────────────────────────

/// Windows 盘符路径 → WSL 默认 automount 挂载路径(`D:\Git\foo` → `/mnt/d/Git/foo`)。
/// 只支持默认 `/mnt` 挂载根,不解析 /etc/wsl.conf 自定义 root。
/// 盘符转小写(WSL 挂载点为小写),其余路径段保留原大小写
/// (drvfs 大小写不敏感,匹配阶段统一 lowercase 比较)。
/// 非盘符路径(UNC / 相对路径)返回 None。
fn windows_path_to_wsl_mnt(path: &str) -> Option<String> {
    // 剥盘符 verbatim 前缀 `\\?\C:\...`;`\\?\UNC\...` 剥后首字节非盘符,自然落 None
    let s = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let rest = s[2..].replace('\\', "/");
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        Some(format!("/mnt/{}", drive))
    } else {
        Some(format!("/mnt/{}/{}", drive, rest))
    }
}

/// 推导 WSL 会话来源:(distro, unix cwd)。
/// - 项目根是 WSL UNC(WSL 根项目):从路径解析,忽略入参 distro;
/// - 项目根是 Windows 盘符路径(WSL 关联项目):必须给 distro,按 /mnt 规则映射;
/// - 其他情况(无 distro / 非盘符路径)返回 None。
fn derive_wsl_target(project_path: &str, distro: Option<String>) -> Option<(String, String)> {
    if let Some(wsl) = mt_core::parse_wsl_unc(project_path) {
        return Some((wsl.distro, wsl.unix_path));
    }
    let distro = distro.filter(|d| !d.is_empty())?;
    let unix_cwd = windows_path_to_wsl_mnt(project_path)?;
    Some((distro, unix_cwd))
}

/// 枚举发行版内可能装有 claude/codex 的 home:`\home\*` + `\root`,
/// 凡含 `.claude` 或 `.codex` 目录的都纳入(多用户 distro 场景;防串项目由
/// cwd 精确校验兜底)。发行版未安装 / VM 启动失败等一切 IO 失败静默返回空。
/// 注意:读 `\\wsl$\<distro>\` 时若 VM 未运行,Windows 会自动启动它(可能数秒)。
fn wsl_candidate_homes(distro: &str) -> Vec<PathBuf> {
    let root = PathBuf::from(format!(r"\\wsl$\{}", distro));
    let mut homes: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = fs::read_dir(root.join("home")) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                homes.push(entry.path());
            }
        }
    }
    homes.push(root.join("root"));

    homes.retain(|h| h.join(".claude").is_dir() || h.join(".codex").is_dir());
    homes
}

// ─── Claude Sessions ───────────────────────────────────────────

/// 扫描指定 home 下的 Claude 会话。`wsl_distro` 为来源标识,一并写进结果。
fn get_claude_sessions_in(
    home: &Path,
    project_path: &str,
    style: PathStyle,
    max_files: usize,
    wsl_distro: Option<&str>,
) -> Vec<AiSession> {
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.exists() {
        return vec![];
    }
    let project_dirs = find_claude_project_dirs_in(&projects_dir, project_path, style);
    if project_dirs.is_empty() {
        return vec![];
    }

    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for dir in &project_dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if seen_ids.insert(id) {
                    paths.push(path);
                }
            }
        }
    }

    sort_newest_session_paths(&mut paths, max_files);

    let mut sessions = Vec::new();
    for path in paths {
        if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }

        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let (title, timestamp) = read_claude_session_info(&path);

        sessions.push(AiSession {
            id,
            session_type: "claude".to_string(),
            title,
            timestamp,
            wsl_distro: wsl_distro.map(String::from),
        });
    }

    sessions
}

/// Windows 宿主视角的 Claude 会话扫描
fn get_claude_sessions(project_path: &str) -> Vec<AiSession> {
    let home = match home_dir() {
        Some(h) => h,
        None => return vec![],
    };
    get_claude_sessions_in(
        &home,
        project_path,
        PathStyle::Windows,
        MAX_CLAUDE_SESSION_FILES_TO_SCAN,
        None,
    )
}

/// 读取 Claude JSONL,提取第一条 user message 的内容和时间戳
fn read_claude_session_info(path: &Path) -> (String, String) {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return ("Untitled".into(), String::new()),
    };

    let reader = BufReader::new(file);

    for line in reader.lines().take(50) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if obj.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }

        let content_val = obj.pointer("/message/content");

        let content = if let Some(s) = content_val.and_then(|c| c.as_str()) {
            s.to_string()
        } else if let Some(arr) = content_val.and_then(|c| c.as_array()) {
            // 多模态消息:取第一个 text block
            arr.iter()
                .filter_map(|item| {
                    if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                        item.get("text").and_then(|t| t.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .next()
                .unwrap_or_else(|| "Untitled".into())
        } else {
            "Untitled".into()
        };

        // 跳过系统注入消息(如 /clear 等本地命令产生的 <local-command-caveat> 等)
        let trimmed = content.trim_start();
        if trimmed.starts_with('<') {
            continue;
        }

        // 截断到 100 字符
        let title: String = content.chars().take(100).collect();

        let timestamp = obj
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        return (title, timestamp);
    }

    ("Untitled".into(), String::new())
}

// ─── Codex Sessions ────────────────────────────────────────────

/// 扫描指定 home 下的 Codex 会话。`wsl_distro` 为来源标识,一并写进结果。
fn get_codex_sessions_in(
    home: &Path,
    project_path: &str,
    style: PathStyle,
    max_files: usize,
    wsl_distro: Option<&str>,
) -> Vec<AiSession> {
    let codex_dir = home.join(".codex");
    let sessions_dir = codex_dir.join("sessions");

    if !sessions_dir.exists() {
        return vec![];
    }

    // 加载 session_index.jsonl 中的 thread_name 映射
    let thread_names = load_codex_thread_names(&codex_dir);

    let mut sessions = Vec::new();
    let normalized_project = style.normalize(project_path);

    let mut session_paths = Vec::new();
    collect_codex_session_paths(&sessions_dir, &mut session_paths);
    sort_newest_session_paths(&mut session_paths, max_files);

    for path in session_paths {
        if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }
        if let Some(session) =
            try_read_codex_session(&path, &normalized_project, style, &thread_names, wsl_distro)
        {
            sessions.push(session);
        }
    }

    sessions
}

/// Windows 宿主视角的 Codex 会话扫描
fn get_codex_sessions(project_path: &str) -> Vec<AiSession> {
    let home = match home_dir() {
        Some(h) => h,
        None => return vec![],
    };
    get_codex_sessions_in(
        &home,
        project_path,
        PathStyle::Windows,
        MAX_CODEX_SESSION_FILES_TO_SCAN,
        None,
    )
}

/// 加载 Codex session_index.jsonl → { id: thread_name }
fn load_codex_thread_names(codex_dir: &Path) -> HashMap<String, String> {
    let index_path = codex_dir.join("session_index.jsonl");
    let mut map = HashMap::new();

    let file = match fs::File::open(&index_path) {
        Ok(f) => f,
        Err(_) => return map,
    };

    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) {
            if let (Some(id), Some(name)) = (
                obj.get("id").and_then(|v| v.as_str()),
                obj.get("thread_name").and_then(|v| v.as_str()),
            ) {
                map.insert(id.to_string(), name.to_string());
            }
        }
    }

    map
}

fn sort_newest_session_paths(paths: &mut Vec<PathBuf>, limit: usize) {
    paths.sort_by(|a, b| {
        let mt = |p: &PathBuf| p.metadata().and_then(|m| m.modified()).ok();
        match (mt(a), mt(b)) {
            (Some(ta), Some(tb)) => tb.cmp(&ta),
            _ => b.cmp(a),
        }
    });
    if paths.len() > limit {
        paths.truncate(limit);
    }
}

/// 递归遍历 sessions/<year>/<month>/<day>/ 目录,仅收集文件路径。
/// 真正读取 JSONL 前先按路径日期排序和限量,避免历史记录增长后每次刷新都读全量内容。
fn collect_codex_session_paths(dir: &Path, paths: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_codex_session_paths(&path, paths);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
}

/// 读取 Codex session 文件,匹配 cwd 后返回 AiSession
fn try_read_codex_session(
    path: &Path,
    normalized_project: &str,
    style: PathStyle,
    thread_names: &HashMap<String, String>,
    wsl_distro: Option<&str>,
) -> Option<AiSession> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut matched_id = None;
    let mut matched_timestamp = String::new();

    let mut lines_iter = reader.lines();

    // 第一遍:前 5 行找 session_meta,匹配 cwd
    for line in (&mut lines_iter).take(5) {
        let line = line.ok()?;
        let obj: serde_json::Value = serde_json::from_str(&line).ok()?;

        if obj.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            continue;
        }

        let cwd = obj
            .pointer("/payload/cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if style.normalize(cwd) != normalized_project {
            return None;
        }

        matched_id = Some(
            obj.pointer("/payload/id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );

        matched_timestamp = obj
            .pointer("/payload/timestamp")
            .or_else(|| obj.get("timestamp"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        break;
    }

    let id = matched_id?;

    // 先查 session_index 中的 thread_name
    let mut title = thread_names.get(&id).cloned().unwrap_or_default();

    // 如果 thread_name 为空,从后续行中找第一条真实用户消息
    if title.is_empty() {
        for line in lines_iter.take(30) {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let obj: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
                continue;
            }
            if obj.pointer("/payload/role").and_then(|v| v.as_str()) != Some("user") {
                continue;
            }

            // 遍历 content blocks,找第一个非系统注入的 text
            if let Some(arr) = obj.pointer("/payload/content").and_then(|v| v.as_array()) {
                for item in arr {
                    if item.get("type").and_then(|t| t.as_str()) != Some("input_text") {
                        continue;
                    }
                    let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    let trimmed = text.trim_start();
                    if !trimmed.is_empty()
                        && !trimmed.starts_with('<')
                        && !trimmed.starts_with("# AGENTS.md")
                    {
                        title = trimmed.chars().take(100).collect();
                        break;
                    }
                }
            }
            if !title.is_empty() {
                break;
            }
        }

        if title.is_empty() {
            title = "Untitled".into();
        }
    }

    let timestamp = matched_timestamp;

    Some(AiSession {
        id,
        session_type: "codex".to_string(),
        title,
        timestamp,
        wsl_distro: wsl_distro.map(String::from),
    })
}

// ─── Session Content ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSessionMessage {
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

fn extract_text_content(content_val: Option<&serde_json::Value>) -> String {
    match content_val {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let texts: Vec<String> = arr
                .iter()
                .filter_map(|item| {
                    let t = item.get("type").and_then(|t| t.as_str())?;
                    match t {
                        "text" | "output_text" | "input_text" => {
                            item.get("text").and_then(|t| t.as_str()).map(String::from)
                        }
                        _ => None,
                    }
                })
                .collect();
            texts.join("\n\n")
        }
        _ => String::new(),
    }
}

/// 从单个 Claude JSONL 会话文件读取全部消息
fn read_claude_messages_from_file(path: &Path) -> Result<Vec<AiSessionMessage>, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法打开文件: {}", e))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let role = match obj.get("type").and_then(|t| t.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };

        let content = extract_text_content(obj.pointer("/message/content"));
        if content.is_empty() {
            continue;
        }

        let timestamp = obj
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        messages.push(AiSessionMessage {
            role: role.to_string(),
            content,
            timestamp,
        });
    }

    Ok(messages)
}

fn read_claude_session_content(
    session_id: &str,
    project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let project_dirs = find_claude_project_dirs(project_path);
    let filename = format!("{}.jsonl", session_id);

    let path = project_dirs
        .iter()
        .map(|dir| dir.join(&filename))
        .find(|p| p.exists())
        .ok_or_else(|| "会话文件不存在".to_string())?;

    read_claude_messages_from_file(&path)
}

fn is_codex_session_match(path: &Path, session_id: &str) -> bool {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let reader = BufReader::new(file);
    for line in reader.lines().take(5) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if obj.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
            if let Some(id) = obj.pointer("/payload/id").and_then(|v| v.as_str()) {
                return id == session_id;
            }
        }
    }
    false
}

/// 在 sessions 目录下按 session_meta.payload.id 定位会话文件
fn find_codex_session_file(sessions_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if !sessions_dir.exists() {
        return None;
    }
    let mut paths = Vec::new();
    collect_codex_session_paths(sessions_dir, &mut paths);
    paths
        .into_iter()
        .find(|p| is_codex_session_match(p, session_id))
}

/// 从单个 Codex JSONL 会话文件读取全部消息
fn read_codex_messages_from_file(path: &Path) -> Result<Vec<AiSessionMessage>, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法打开文件: {}", e))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }

        let role = match obj.pointer("/payload/role").and_then(|v| v.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };

        let content = extract_text_content(obj.pointer("/payload/content"));
        if content.is_empty() {
            continue;
        }

        let timestamp = obj
            .pointer("/payload/timestamp")
            .or_else(|| obj.get("timestamp"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        messages.push(AiSessionMessage {
            role: role.to_string(),
            content,
            timestamp,
        });
    }

    Ok(messages)
}

fn read_codex_session_content(
    session_id: &str,
    _project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let home = home_dir().ok_or_else(|| "无法获取 home 目录".to_string())?;
    let sessions_dir = home.join(".codex").join("sessions");

    if !sessions_dir.exists() {
        return Err("Codex sessions 目录不存在".to_string());
    }

    let session_file = find_codex_session_file(&sessions_dir, session_id)
        .ok_or_else(|| "未找到 Codex 会话文件".to_string())?;

    read_codex_messages_from_file(&session_file)
}

/// 读取 WSL 发行版内的 Claude 会话正文:逐 candidate home 定位项目目录下的 `<id>.jsonl`
fn read_wsl_claude_session_content(
    distro: &str,
    unix_cwd: &str,
    session_id: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let filename = format!("{}.jsonl", session_id);
    for home in wsl_candidate_homes(distro) {
        let projects_dir = home.join(".claude").join("projects");
        for dir in find_claude_project_dirs_in(&projects_dir, unix_cwd, PathStyle::Unix) {
            let path = dir.join(&filename);
            if path.exists() {
                return read_claude_messages_from_file(&path);
            }
        }
    }
    Err("会话文件不存在".to_string())
}

/// 读取 WSL 发行版内的 Codex 会话正文:逐 candidate home 按 id 定位
fn read_wsl_codex_session_content(
    distro: &str,
    session_id: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    for home in wsl_candidate_homes(distro) {
        let sessions_dir = home.join(".codex").join("sessions");
        if let Some(path) = find_codex_session_file(&sessions_dir, session_id) {
            return read_codex_messages_from_file(&path);
        }
    }
    Err("未找到 Codex 会话文件".to_string())
}

/// 读取会话正文。`wsl_distro` 有值时从对应发行版的 UNC 位置读取(WSL 会话)。
/// 标注 async:WSL 冷启动 + 9P 读取可能秒级,不能阻塞主线程。
#[tauri::command(async)]
pub fn get_ai_session_content(
    session_type: String,
    session_id: String,
    project_path: String,
    wsl_distro: Option<String>,
) -> Result<Vec<AiSessionMessage>, String> {
    if let Some(distro) = wsl_distro.filter(|d| !d.is_empty()) {
        // WSL 根项目的 distro 以路径解析为准(与 get_wsl_ai_sessions 口径一致)
        let (distro, unix_cwd) = derive_wsl_target(&project_path, Some(distro))
            .ok_or_else(|| "无法推导 WSL 项目路径".to_string())?;
        return match session_type.as_str() {
            "claude" => read_wsl_claude_session_content(&distro, &unix_cwd, &session_id),
            "codex" => read_wsl_codex_session_content(&distro, &session_id),
            _ => Err(format!("不支持的会话类型: {}", session_type)),
        };
    }

    match session_type.as_str() {
        "claude" => read_claude_session_content(&session_id, &project_path),
        "codex" => read_codex_session_content(&session_id, &project_path),
        _ => Err(format!("不支持的会话类型: {}", session_type)),
    }
}

// ─── Tauri Commands ────────────────────────────────────────────

#[tauri::command]
pub fn get_ai_sessions(project_path: String) -> Result<Vec<AiSession>, String> {
    let cache_key = normalize_path(&project_path);
    let mut cache = session_cache()
        .lock()
        .map_err(|_| "session cache lock poisoned".to_string())?;

    if let Some(cached) = cache.get(&cache_key) {
        if cached.loaded_at.elapsed() < SESSION_CACHE_TTL {
            return Ok(cached.sessions.clone());
        }
    }

    let mut sessions = Vec::new();

    sessions.extend(get_claude_sessions(&project_path));
    sessions.extend(get_codex_sessions(&project_path));

    // 按时间戳降序(最新在前)
    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    if sessions.len() > MAX_TOTAL_SESSIONS {
        sessions.truncate(MAX_TOTAL_SESSIONS);
    }

    cache.insert(
        cache_key,
        CachedSessions {
            loaded_at: Instant::now(),
            sessions: sessions.clone(),
        },
    );

    Ok(sessions)
}

/// 获取项目在 WSL 发行版内的 claude/codex 会话。
/// - WSL 根项目(UNC 路径):distro 从路径推导,忽略入参;
/// - WSL 关联项目(Windows 路径):按入参 distro + /mnt 映射;
/// - 无法推导来源 / 一切 IO 失败:静默返回空列表。
/// `force` 绕过缓存,供手动刷新使用。
/// 标注 async:9P 扫描 + 可能的 VM 冷启动是秒级操作,不能阻塞主线程。
#[tauri::command(async)]
pub fn get_wsl_ai_sessions(
    project_path: String,
    distro: Option<String>,
    force: Option<bool>,
) -> Result<Vec<AiSession>, String> {
    let (distro, unix_cwd) = match derive_wsl_target(&project_path, distro) {
        Some(t) => t,
        None => return Ok(vec![]),
    };

    let cache_key = format!(
        "wsl|{}|{}",
        distro.to_lowercase(),
        normalize_unix_path(&unix_cwd)
    );

    if !force.unwrap_or(false) {
        let cache = session_cache()
            .lock()
            .map_err(|_| "session cache lock poisoned".to_string())?;
        if let Some(cached) = cache.get(&cache_key) {
            if cached.loaded_at.elapsed() < WSL_SESSION_CACHE_TTL {
                return Ok(cached.sessions.clone());
            }
        }
        // 扫描期间不持锁:9P IO 可能秒级,别把 Windows 侧 get_ai_sessions 一起卡住
    }

    let mut sessions = Vec::new();
    for home in wsl_candidate_homes(&distro) {
        sessions.extend(get_claude_sessions_in(
            &home,
            &unix_cwd,
            PathStyle::Unix,
            MAX_WSL_CLAUDE_SESSION_FILES_TO_SCAN,
            Some(&distro),
        ));
        sessions.extend(get_codex_sessions_in(
            &home,
            &unix_cwd,
            PathStyle::Unix,
            MAX_WSL_CODEX_SESSION_FILES_TO_SCAN,
            Some(&distro),
        ));
    }

    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    if sessions.len() > MAX_TOTAL_SESSIONS {
        sessions.truncate(MAX_TOTAL_SESSIONS);
    }

    let mut cache = session_cache()
        .lock()
        .map_err(|_| "session cache lock poisoned".to_string())?;
    cache.insert(
        cache_key,
        CachedSessions {
            loaded_at: Instant::now(),
            sessions: sessions.clone(),
        },
    );

    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_newest_session_paths_keeps_recent_files_first() {
        let mut paths = vec![
            PathBuf::from(
                r"C:\Users\test\.codex\sessions\2025\10\28\rollout-2025-10-28T10-47-08-old.jsonl",
            ),
            PathBuf::from(
                r"C:\Users\test\.codex\sessions\2026\04\24\rollout-2026-04-24T19-00-00-newest.jsonl",
            ),
            PathBuf::from(
                r"C:\Users\test\.codex\sessions\2026\01\02\rollout-2026-01-02T09-00-00-middle.jsonl",
            ),
        ];

        sort_newest_session_paths(&mut paths, 2);

        assert_eq!(paths.len(), 2);
        assert!(paths[0].to_string_lossy().contains("newest"));
        assert!(paths[1].to_string_lossy().contains("middle"));
    }

    #[test]
    fn windows_path_to_wsl_mnt_maps_drive_and_separators() {
        assert_eq!(
            windows_path_to_wsl_mnt(r"D:\Git\foo").as_deref(),
            Some("/mnt/d/Git/foo")
        );
        // 盘符转小写,其余路径段保留原大小写
        assert_eq!(
            windows_path_to_wsl_mnt(r"C:\Users\Dev\My Proj").as_deref(),
            Some("/mnt/c/Users/Dev/My Proj")
        );
        // 尾部斜杠去掉
        assert_eq!(
            windows_path_to_wsl_mnt(r"D:\Git\foo\").as_deref(),
            Some("/mnt/d/Git/foo")
        );
        // 正斜杠形式也能处理
        assert_eq!(
            windows_path_to_wsl_mnt("d:/git/foo").as_deref(),
            Some("/mnt/d/git/foo")
        );
        // 盘符根
        assert_eq!(windows_path_to_wsl_mnt(r"C:\").as_deref(), Some("/mnt/c"));
        // verbatim 盘符前缀
        assert_eq!(
            windows_path_to_wsl_mnt(r"\\?\D:\Git\foo").as_deref(),
            Some("/mnt/d/Git/foo")
        );
    }

    #[test]
    fn windows_path_to_wsl_mnt_rejects_non_drive_paths() {
        assert!(windows_path_to_wsl_mnt(r"\\server\share").is_none());
        assert!(windows_path_to_wsl_mnt(r"\\wsl$\Ubuntu\home").is_none());
        assert!(windows_path_to_wsl_mnt(r"\\?\UNC\wsl$\Ubuntu\home").is_none());
        assert!(windows_path_to_wsl_mnt("/home/user/proj").is_none());
        assert!(windows_path_to_wsl_mnt("relative\\path").is_none());
        assert!(windows_path_to_wsl_mnt("").is_none());
    }

    #[test]
    fn normalize_unix_path_lowercases_and_trims_trailing_slash() {
        assert_eq!(normalize_unix_path("/mnt/d/Git/Foo/"), "/mnt/d/git/foo");
        assert_eq!(normalize_unix_path("/home/User/proj"), "/home/user/proj");
        // 保留 `/`,不换成 `\`(Windows 版 normalize_path 不可复用的原因)
        assert!(normalize_unix_path("/mnt/d/git").contains('/'));
        assert!(!normalize_unix_path("/mnt/d/git").contains('\\'));
    }

    #[test]
    fn derive_wsl_target_prefers_unc_and_ignores_distro_param() {
        // WSL 根项目:distro 从路径推导,入参被忽略
        let (distro, cwd) =
            derive_wsl_target(r"\\wsl$\Ubuntu-22.04\home\u\proj", Some("Debian".into())).unwrap();
        assert_eq!(distro, "Ubuntu-22.04");
        assert_eq!(cwd, "/home/u/proj");

        // wsl.localhost 形式同样支持
        let (distro, cwd) = derive_wsl_target(r"\\wsl.localhost\Ubuntu\home\u", None).unwrap();
        assert_eq!(distro, "Ubuntu");
        assert_eq!(cwd, "/home/u");
    }

    #[test]
    fn derive_wsl_target_maps_windows_path_with_distro() {
        let (distro, cwd) = derive_wsl_target(r"D:\Git\foo", Some("Ubuntu".into())).unwrap();
        assert_eq!(distro, "Ubuntu");
        assert_eq!(cwd, "/mnt/d/Git/foo");
    }

    #[test]
    fn derive_wsl_target_none_without_distro_or_unmappable_path() {
        // Windows 路径但没给 distro
        assert!(derive_wsl_target(r"D:\Git\foo", None).is_none());
        // 空 distro 等同未给
        assert!(derive_wsl_target(r"D:\Git\foo", Some("".into())).is_none());
        // 非 WSL 的 UNC 路径映射不了 /mnt
        assert!(derive_wsl_target(r"\\server\share\proj", Some("Ubuntu".into())).is_none());
    }

    #[test]
    fn encode_project_path_works_for_unix_cwd() {
        assert_eq!(encode_project_path("/mnt/d/git/foo"), "-mnt-d-git-foo");
        assert_eq!(encode_project_path("/home/u/proj"), "-home-u-proj");
        // 尾部斜杠先去掉再编码
        assert_eq!(encode_project_path("/home/u/proj/"), "-home-u-proj");
    }

    #[test]
    fn is_encoded_variant_matches_case_and_trailing_dashes() {
        // 大小写变体(drvfs 大小写不敏感)
        assert!(is_encoded_variant("-mnt-d-Git-foo", "-mnt-d-git-foo"));
        // 尾部斜杠变体(多出的尾部 `-`)
        assert!(is_encoded_variant("-home-u-proj-", "-home-u-proj"));
        // 前缀相同的不同项目也会命中变体判定 → 由 dir_matches_project 的 cwd 校验兜底排除
        assert!(is_encoded_variant("D--Git-bhyt----", "D--Git-bhyt"));
        // 非变体:后缀含非 `-` 字符
        assert!(!is_encoded_variant("-home-u-proj2", "-home-u-proj"));
        assert!(!is_encoded_variant("-home-u-pro", "-home-u-proj"));
    }

    #[test]
    fn path_style_normalize_uses_matching_semantics() {
        assert_eq!(
            PathStyle::Windows.normalize("D:/Git/Foo/"),
            r"d:\git\foo"
        );
        assert_eq!(PathStyle::Unix.normalize("/mnt/d/Git/Foo/"), "/mnt/d/git/foo");
    }
}
