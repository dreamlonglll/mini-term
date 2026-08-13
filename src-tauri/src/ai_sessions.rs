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
pub(crate) const MAX_SESSIONS_PER_SOURCE: usize = 80;
pub(crate) const MAX_TOTAL_SESSIONS: usize = 120;
const SESSION_CACHE_TTL: Duration = Duration::from_secs(2);
// WSL 扫描代价高(9P + 可能触发 VM 冷启动),TTL 放宽;手动刷新走 force 绕过。
const WSL_SESSION_CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct CachedSessions {
    pub(crate) loaded_at: Instant,
    pub(crate) sessions: Vec<AiSession>,
}

static SESSION_CACHE: OnceLock<Mutex<HashMap<String, CachedSessions>>> = OnceLock::new();

/// 会话列表缓存(Windows / WSL / SSH 远程三来源共用同一 map,key 前缀区分)。
/// 锁契约:即取即放,**绝不跨慢 IO 持锁**(见 spec/backend/wsl-unc-session-scanning.md)。
pub(crate) fn session_cache() -> &'static Mutex<HashMap<String, CachedSessions>> {
    SESSION_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSession {
    pub id: String,
    pub session_type: String, // "claude" | "codex" | "grok"
    pub title: String,
    pub timestamp: String, // ISO 8601
    /// 会话来源:Some = 该 WSL 发行版内的会话,None = Windows 宿主会话。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wsl_distro: Option<String>,
    /// 会话来源:Some = 该 SSH 连接指向的远程机器上的会话(SSH 远程项目),
    /// None = 本机来源。与 `wsl_distro` 同为 CONTEXT.md「会话来源」标识,互斥。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_connection_id: Option<String>,
}

/// 获取用户 home 目录
fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// 从 Claude 会话 jsonl 文本中提取首个非空 `cwd` 字段。
/// 前部行可能是无 cwd 的 summary/meta,逐行找到即止;非 JSON 行忽略。
fn extract_session_cwd(jsonl: &str) -> Option<String> {
    for line in jsonl.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                if !cwd.is_empty() {
                    return Some(cwd.to_string());
                }
            }
        }
    }
    None
}

/// 按 session id 在 `~/.claude/projects` 各桶中反查 Claude 会话的启动 cwd。
///
/// `claude --resume` 只在当前目录对应的桶里找会话,起于子目录的会话必须回到
/// 原目录才能恢复;桶目录名是有损编码(所有非字母数字都变 `-`),不做反解码,
/// 直接按 `<id>.jsonl` 文件名精确命中后读记录内的真实 cwd。
/// 供续接链路对无 cwd 的存量 pane 记录做兜底反查,查到由前端写回持久化。
#[tauri::command]
pub fn lookup_ai_session_cwd(session_id: String) -> Option<String> {
    // 与前端 resume 命令的校验同口径,顺带防路径拼接注入
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let projects = home_dir()?.join(".claude").join("projects");
    let target = format!("{}.jsonl", session_id);
    for entry in std::fs::read_dir(&projects).ok()?.flatten() {
        let candidate = entry.path().join(&target);
        if !candidate.is_file() {
            continue;
        }
        // cwd 几乎必在首条正式记录;50 行封顶,病态大文件不整读
        if let Ok(f) = std::fs::File::open(&candidate) {
            use std::io::BufRead;
            let head: Vec<String> = std::io::BufReader::new(f)
                .lines()
                .map_while(Result::ok)
                .take(50)
                .collect();
            if let Some(cwd) = extract_session_cwd(&head.join("\n")) {
                // 目录可能已经不在了（worktree 被移除、项目搬家）。返回它只会让
                // 续接时的 create_pty 直接失败、pane 变 error —— 不如当作查不到，
                // 让前端回落 pane 自己的 cwd。
                if std::path::Path::new(&cwd).is_dir() {
                    return Some(cwd);
                }
            }
        }
    }
    None
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
/// pub(crate):SSH 远程项目的会话扫描(remote_ssh.rs)复用同一编码。
pub(crate) fn encode_project_path(project_path: &str) -> String {
    project_path
        .trim_end_matches(['/', '\\'])
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// 目录名是否为编码名的「变体」:大小写不同(drvfs 大小写不敏感,WSL 内
/// `cd /mnt/d/GIT/foo` 也能进同一目录)或仅多出尾部 `-`(带尾部斜杠的同一项目)。
/// 编码有损,变体命中后仍需读 jsonl 内真实 cwd 精确校验,防止吃进兄弟项目。
pub(crate) fn is_encoded_variant(dir_name: &str, encoded: &str) -> bool {
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
pub(crate) fn find_claude_project_dirs(project_path: &str) -> Vec<PathBuf> {
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
pub(crate) fn normalize_path(path: &str) -> String {
    path.replace('/', "\\")
        .to_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

/// Unix 语义路径统一化(小写 + 保留 `/`,去尾部 `/`),用于 WSL 内 / SSH 远程 cwd 比较
pub(crate) fn normalize_unix_path(path: &str) -> String {
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
            ssh_connection_id: None,
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
    let lines: Vec<String> = reader
        .lines()
        .take(50)
        .filter_map(Result::ok)
        .collect();
    claude_session_info_from_lines(lines.iter().map(String::as_str))
}

/// 从会话文件的前若干行提取 (title, timestamp)。行级纯函数,本地(BufReader)
/// 与远程(SFTP 读头部字节后按行切)两条路径共用。
pub(crate) fn claude_session_info_from_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
) -> (String, String) {
    for line in lines {
        let obj: serde_json::Value = match serde_json::from_str(line) {
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
/// pub(crate)：使用统计(usage_stats)全局扫描时复用同一标题映射。
pub(crate) fn load_codex_thread_names(codex_dir: &Path) -> HashMap<String, String> {
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

pub(crate) fn sort_newest_session_paths(paths: &mut Vec<PathBuf>, limit: usize) {
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
pub(crate) fn collect_codex_session_paths(dir: &Path, paths: &mut Vec<PathBuf>) {
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

/// Codex 会话文件头部 session_meta 行的关键字段。
pub(crate) struct CodexSessionMeta {
    pub(crate) id: String,
    pub(crate) timestamp: String,
    pub(crate) cwd: String,
}

/// 解析一行,若是 session_meta 则取出 id/timestamp/cwd。行级纯函数,
/// SSH 远程扫描(remote_ssh.rs)用它对远程 rollout 文件做 cwd 匹配。
pub(crate) fn codex_meta_from_line(line: &str) -> Option<CodexSessionMeta> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    Some(CodexSessionMeta {
        id: obj
            .pointer("/payload/id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        timestamp: obj
            .pointer("/payload/timestamp")
            .or_else(|| obj.get("timestamp"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        cwd: obj
            .pointer("/payload/cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// 从一行 response_item 里提取第一条真实用户输入作为标题候选
/// (跳过 `<...>` 系统注入与 `# AGENTS.md` 前缀)。行级纯函数,本地与远程共用。
pub(crate) fn codex_user_title_from_line(line: &str) -> Option<String> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return None;
    }
    if obj.pointer("/payload/role").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    let arr = obj.pointer("/payload/content").and_then(|v| v.as_array())?;
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
            return Some(trimmed.chars().take(100).collect());
        }
    }
    None
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
            if let Some(t) = codex_user_title_from_line(&line) {
                title = t;
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
        ssh_connection_id: None,
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

/// 解析 Claude JSONL 的一行为消息。非 user/assistant / 空内容 / 非 JSON → None。
/// 行级纯函数,本地与远程(SFTP)两条正文读取路径共用。
pub(crate) fn claude_message_from_line(line: &str) -> Option<AiSessionMessage> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;

    let role = match obj.get("type").and_then(|t| t.as_str()) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return None,
    };

    let content = extract_text_content(obj.pointer("/message/content"));
    if content.is_empty() {
        return None;
    }

    let timestamp = obj
        .get("timestamp")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    Some(AiSessionMessage {
        role: role.to_string(),
        content,
        timestamp,
    })
}

/// 从单个 Claude JSONL 会话文件读取全部消息
fn read_claude_messages_from_file(path: &Path) -> Result<Vec<AiSessionMessage>, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法打开文件: {}", e))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    // 显式循环而非 map_while(Result::ok):坏行(如非 UTF-8)只跳过该行,
    // 不中断迭代 —— map_while 会在首个 Err 处截断其后全部消息。
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if let Some(m) = claude_message_from_line(&line) {
            messages.push(m);
        }
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

/// 解析 Codex JSONL 的一行为消息。非 response_item / 非 user/assistant / 空内容 → None。
/// 行级纯函数,本地与远程(SFTP)两条正文读取路径共用。
pub(crate) fn codex_message_from_line(line: &str) -> Option<AiSessionMessage> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;

    if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return None;
    }

    let role = match obj.pointer("/payload/role").and_then(|v| v.as_str()) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return None,
    };

    let content = extract_text_content(obj.pointer("/payload/content"));
    if content.is_empty() {
        return None;
    }

    let timestamp = obj
        .pointer("/payload/timestamp")
        .or_else(|| obj.get("timestamp"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    Some(AiSessionMessage {
        role: role.to_string(),
        content,
        timestamp,
    })
}

/// 从单个 Codex JSONL 会话文件读取全部消息
fn read_codex_messages_from_file(path: &Path) -> Result<Vec<AiSessionMessage>, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法打开文件: {}", e))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    // 显式循环而非 map_while(Result::ok):坏行只跳过,不截断后续消息(同 claude 侧)。
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if let Some(m) = codex_message_from_line(&line) {
            messages.push(m);
        }
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
        "grok" => read_grok_session_content(&session_id, &project_path),
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
    sessions.extend(get_grok_sessions(&project_path));

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

// ─── Grok Build 会话 ────────────────────────────────────────────
//
// 磁盘布局与另外两家都不同:一个会话是一**整个目录**,不是一个文件。
//
//   {grok_home}/sessions/{编码后的 cwd}/{session-id}/
//       summary.json     标题/模型/时间戳/消息数,会话身份的索引
//       updates.jsonl    ACP 会话更新流(对话正文的权威来源)
//
// 组目录名是 cwd 的 URL 编码;编码超过 255 字节时退化成 `{slug}-{hash16}`,
// 并在目录内写一个 `.cwd` 记下原始路径。我们**解码**目录名去比项目路径,
// 而不是编码项目路径去比目录名 —— 后者要逐字复刻 grok 所用 urlencoding
// crate 的转义集(以及未来的任何调整),前者对两种编码形态一并成立。

/// grok 会话根目录:`{$GROK_HOME | ~/.grok}/sessions`
fn grok_sessions_dir() -> Option<PathBuf> {
    crate::hook_registry::grok_home().map(|h| h.join("sessions"))
}

/// 百分号解码(只处理 `%XX`;grok 的 urlencoding 不产出 `+` 代空格)。
/// 非法转义原样保留,交给调用方的形态校验兜底。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 还原组目录代表的 cwd。
///
/// 先试 URL 解码:解出来的绝对路径必然以 `/` 开头或第二字符是 `:`(盘符),
/// 而 `{slug}-{hash}` 形态两者都不满足,据此无歧义地区分两种编码;
/// 后者回落读目录内的 `.cwd`。
pub(crate) fn decode_grok_cwd_dir(dir: &Path) -> Option<String> {
    let name = dir.file_name()?.to_str()?;
    let decoded = percent_decode(name);
    let looks_absolute =
        decoded.starts_with('/') || decoded.chars().nth(1) == Some(':');
    if looks_absolute {
        return Some(decoded);
    }
    fs::read_to_string(dir.join(".cwd"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 项目对应的 grok 组目录(通常 0 或 1 个;大小写/分隔符差异会各成一个目录)
pub(crate) fn find_grok_cwd_dirs(project_path: &str) -> Vec<PathBuf> {
    let Some(sessions_dir) = grok_sessions_dir() else {
        return Vec::new();
    };
    let normalized = normalize_path(project_path);
    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            decode_grok_cwd_dir(p).is_some_and(|cwd| normalize_path(&cwd) == normalized)
        })
        .collect()
}

/// 会话目录里的 `updates.jsonl`;不存在(会话刚建、正文未落盘)返回 None
pub(crate) fn grok_updates_path(session_dir: &Path) -> Option<PathBuf> {
    let path = session_dir.join("updates.jsonl");
    path.is_file().then_some(path)
}

/// 按 session id 在项目的组目录里定位会话目录
pub(crate) fn find_grok_session_dir(project_path: &str, session_id: &str) -> Option<PathBuf> {
    find_grok_cwd_dirs(project_path)
        .into_iter()
        .map(|group| group.join(session_id))
        .find(|p| p.is_dir())
}

/// summary.json 的关键字段(grok 的 `Summary` 结构是 snake_case)
struct GrokSummary {
    id: String,
    title: String,
    timestamp: String,
}

fn read_grok_summary(session_dir: &Path) -> Option<GrokSummary> {
    let raw = fs::read_to_string(session_dir.join("summary.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    // id 以目录名为准:summary.json 里的 info.id 是同一个值,但目录名是我们
    // 后续拼路径的依据,两者万一不一致要以能定位到文件的那个为准。
    let id = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .or_else(|| {
            v.pointer("/info/id")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })?;
    let timestamp = v
        .get("updated_at")
        .or_else(|| v.get("created_at"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let title = v
        .get("session_summary")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("(无标题)")
        .to_string();
    Some(GrokSummary { id, title, timestamp })
}

/// Windows 宿主视角的 grok 会话扫描
fn get_grok_sessions(project_path: &str) -> Vec<AiSession> {
    let mut sessions = Vec::new();
    for group in find_grok_cwd_dirs(project_path) {
        let Ok(entries) = fs::read_dir(&group) else {
            continue;
        };
        for entry in entries.flatten() {
            if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
                return sessions;
            }
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if let Some(s) = read_grok_summary(&dir) {
                sessions.push(AiSession {
                    id: s.id,
                    session_type: "grok".to_string(),
                    title: s.title,
                    timestamp: s.timestamp,
                    wsl_distro: None,
                    ssh_connection_id: None,
                });
            }
        }
    }
    sessions
}

/// `updates.jsonl` 一行的语义分类
enum GrokUpdate {
    /// 用户消息分片(连续多行属于同一条消息)
    UserChunk { text: String, timestamp: u64 },
    /// AI 回复分片
    AgentChunk { text: String, timestamp: u64 },
    /// 其余更新:工具调用、计划、回合收尾…… 都是消息边界
    Boundary,
}

/// 解析一行 `updates.jsonl`。
///
/// 行的形态是 `{"timestamp":<unix秒>,"method":"…","params":{…}}`,
/// 旧版本没有信封、整行就是 params。`params.update.sessionUpdate` 是判别式。
/// xAI 扩展轨(`method` 为 `_x.ai/session/update`)与 ACP 轨共用同一个判别式键,
/// 但扩展轨里没有消息分片,一律当边界。
fn parse_grok_update(line: &str) -> GrokUpdate {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return GrokUpdate::Boundary;
    };
    let is_xai = v.get("method").and_then(|m| m.as_str()) == Some("_x.ai/session/update");
    let timestamp = v.get("timestamp").and_then(|t| t.as_u64()).unwrap_or(0);
    let params = v.get("params").unwrap_or(&v);
    let Some(update) = params.get("update") else {
        return GrokUpdate::Boundary;
    };
    if is_xai {
        return GrokUpdate::Boundary;
    }
    let tag = update.get("sessionUpdate").and_then(|t| t.as_str());
    let text = || {
        let content = update.get("content")?;
        if content.get("type").and_then(|t| t.as_str()) != Some("text") {
            return None;
        }
        content.get("text").and_then(|t| t.as_str()).map(str::to_string)
    };
    match tag {
        Some("user_message_chunk") => {
            // `content._meta.bashCommand` = `!bash` 直通命令的回显,
            // `_meta.hostTurn` = 宿主注入的回合(工具结果/系统提醒),
            // 两者都不是用户说的话 —— grok 自己的提示词抽取也这么排除。
            let injected = update.pointer("/content/_meta/bash_command").is_some()
                || update.pointer("/content/_meta/bashCommand").is_some()
                || update.pointer("/_meta/hostTurn").and_then(|v| v.as_bool()) == Some(true);
            match text() {
                Some(t) if !injected => GrokUpdate::UserChunk { text: t, timestamp },
                _ => GrokUpdate::Boundary,
            }
        }
        Some("agent_message_chunk") => match text() {
            Some(t) => GrokUpdate::AgentChunk { text: t, timestamp },
            None => GrokUpdate::Boundary,
        },
        _ => GrokUpdate::Boundary,
    }
}

/// unix 秒 → ISO 8601(前端与 MirrorMessage 都按 ISO 字符串排序/展示)
fn unix_to_iso(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// `updates.jsonl` 的增量解析器。
///
/// 与 Claude/Codex 的「一行一条消息」不同,grok 把一条消息拆成任意多个
/// `*_message_chunk` 行(流式落盘)。必须攒到边界才成一条消息,否则镜像里
/// 一句回答会碎成几十条。边界 = 任何非同角色分片的行(工具调用、回合收尾、
/// 对方开口),与 grok 自身的会话重放口径一致。
pub(crate) struct GrokUpdateParser {
    role: Option<&'static str>,
    buf: String,
    started_at: u64,
}

impl GrokUpdateParser {
    pub(crate) fn new() -> Self {
        Self {
            role: None,
            buf: String::new(),
            started_at: 0,
        }
    }

    /// 喂入一行,返回**被这一行收尾**的上一条消息(如果有)
    pub(crate) fn feed_line(&mut self, line: &str) -> Option<AiSessionMessage> {
        let (role, text, ts) = match parse_grok_update(line) {
            GrokUpdate::UserChunk { text, timestamp } => ("user", text, timestamp),
            GrokUpdate::AgentChunk { text, timestamp } => ("assistant", text, timestamp),
            GrokUpdate::Boundary => return self.flush(),
        };
        if self.role == Some(role) {
            self.buf.push_str(&text);
            return None;
        }
        let done = self.flush();
        self.role = Some(role);
        self.buf = text;
        self.started_at = ts;
        done
    }

    /// 冲出仍在缓冲区里的消息(读到文件尾时调)
    pub(crate) fn flush(&mut self) -> Option<AiSessionMessage> {
        let role = self.role.take()?;
        let content = std::mem::take(&mut self.buf);
        let started_at = std::mem::take(&mut self.started_at);
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(AiSessionMessage {
            role: role.to_string(),
            content: trimmed.to_string(),
            timestamp: unix_to_iso(started_at),
        })
    }
}

fn read_grok_messages_from_file(path: &Path) -> Result<Vec<AiSessionMessage>, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法打开文件: {}", e))?;
    let reader = BufReader::new(file);
    let mut parser = GrokUpdateParser::new();
    let mut messages = Vec::new();
    for line in reader.lines() {
        // 坏行只跳过该行,不中断迭代(与 Claude 路径同因)
        let Ok(line) = line else { continue };
        if let Some(m) = parser.feed_line(&line) {
            messages.push(m);
        }
    }
    if let Some(m) = parser.flush() {
        messages.push(m);
    }
    Ok(messages)
}

fn read_grok_session_content(
    session_id: &str,
    project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let dir = find_grok_session_dir(project_path, session_id)
        .ok_or_else(|| "未找到 Grok 会话目录".to_string())?;
    let path = grok_updates_path(&dir).ok_or_else(|| "会话正文尚未落盘".to_string())?;
    read_grok_messages_from_file(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Grok Build ----

    fn grok_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mt-grok-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 组目录名是 cwd 的 URL 编码;我们解码它去比项目路径,而不是反过来编码
    /// 项目路径(那要逐字复刻 grok 所用 crate 的转义集)。
    #[test]
    fn grok_cwd_dir_decodes_url_encoded_names() {
        let root = grok_temp_dir("decode");
        for (name, expect) in [
            ("D%3A%5CGit%5Cmini-term", r"D:\Git\mini-term"),
            ("%2Fhome%2Fu%2Fproj", "/home/u/proj"),
            // 非 ASCII 与空格
            ("D%3A%5CGit%5Cbhyt-%E4%B8%80%E4%BD%93%E6%9C%BA", r"D:\Git\bhyt-一体机"),
            ("%2Fhome%2Fu%2Fmy%20proj", "/home/u/my proj"),
        ] {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            assert_eq!(decode_grok_cwd_dir(&dir).as_deref(), Some(expect), "{name}");
        }
        fs::remove_dir_all(&root).ok();
    }

    /// 超长 cwd 退化成 `{slug}-{hash}`,原路径写在目录内的 `.cwd` 里
    #[test]
    fn grok_cwd_dir_falls_back_to_dot_cwd_file() {
        let root = grok_temp_dir("hashdir");
        let dir = root.join("mini-term-0123456789abcdef");
        fs::create_dir_all(&dir).unwrap();
        // 没有 .cwd 时无从还原:宁可不认,也不能瞎猜成项目路径
        assert!(decode_grok_cwd_dir(&dir).is_none());

        fs::write(dir.join(".cwd"), "D:\\Git\\very-long-path\n").unwrap();
        assert_eq!(decode_grok_cwd_dir(&dir).as_deref(), Some(r"D:\Git\very-long-path"));
        fs::remove_dir_all(&root).ok();
    }

    fn grok_chunk(tag: &str, text: &str, ts: u64) -> String {
        format!(
            r#"{{"timestamp":{ts},"method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"{tag}","content":{{"type":"text","text":"{text}"}}}}}}}}"#
        )
    }

    /// 分片合并 + 边界收尾 + 角色切换即边界
    #[test]
    fn grok_parser_merges_chunks_and_splits_on_boundaries() {
        let mut p = GrokUpdateParser::new();
        assert!(p.feed_line(&grok_chunk("user_message_chunk", "hello ", 100)).is_none());
        assert!(p.feed_line(&grok_chunk("user_message_chunk", "world", 101)).is_none());
        // 角色切换本身就是边界:上一条在此收尾
        let user = p
            .feed_line(&grok_chunk("agent_message_chunk", "hi", 102))
            .expect("用户消息应在角色切换处收尾");
        assert_eq!(user.role, "user");
        assert_eq!(user.content, "hello world");
        assert_eq!(user.timestamp, "1970-01-01T00:01:40Z", "应取首个分片的时刻");

        // 文件读完时冲出尾部消息
        let agent = p.flush().expect("尾部消息应被冲出");
        assert_eq!(agent.role, "assistant");
        assert_eq!(agent.content, "hi");
        assert!(p.flush().is_none(), "重复 flush 不该产出");
    }

    /// 旧格式(无 method/timestamp 信封,整行就是 params)必须照样解析
    #[test]
    fn grok_parser_accepts_legacy_envelope_free_lines() {
        let mut p = GrokUpdateParser::new();
        let legacy = r#"{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"legacy"}}}"#;
        assert!(p.feed_line(legacy).is_none());
        let msg = p.flush().unwrap();
        assert_eq!(msg.content, "legacy");
        assert_eq!(msg.timestamp, "", "无 timestamp 字段时留空,不能编一个");
    }

    /// 非文本内容(图片)、坏行、纯空白消息都不该产出条目
    #[test]
    fn grok_parser_skips_noise() {
        let mut p = GrokUpdateParser::new();
        for line in [
            "not json at all",
            r#"{"params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"image","data":"…"}}}}"#,
            r#"{"params":{"update":{"sessionUpdate":"plan"}}}"#,
            r#"{"params":{}}"#,
        ] {
            assert!(p.feed_line(line).is_none(), "{line} 不该产出消息");
        }
        assert!(p.flush().is_none());

        // 只有空白的消息也丢掉
        let mut q = GrokUpdateParser::new();
        q.feed_line(&grok_chunk("agent_message_chunk", "   ", 1));
        assert!(q.flush().is_none());
    }

    #[test]
    fn grok_summary_json_yields_session_entry() {
        let root = grok_temp_dir("summary");
        let dir = root.join("0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("summary.json"),
            r#"{"info":{"id":"0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e","cwd":"D:\\Git\\proj"},
                "session_summary":"修 PTY 背压","created_at":"2026-08-01T10:00:00Z",
                "updated_at":"2026-08-01T11:30:00Z","num_messages":12,"current_model_id":"grok-4"}"#,
        )
        .unwrap();

        let s = read_grok_summary(&dir).expect("应解析出会话");
        assert_eq!(s.id, "0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e");
        assert_eq!(s.title, "修 PTY 背压");
        assert_eq!(s.timestamp, "2026-08-01T11:30:00Z", "列表按更新时刻排序");

        // 无标题(标题还没生成完)不能让整条会话消失
        fs::write(dir.join("summary.json"), r#"{"created_at":"2026-08-01T10:00:00Z"}"#).unwrap();
        let s = read_grok_summary(&dir).expect("缺字段也应给出条目");
        assert_eq!(s.title, "(无标题)");
        assert_eq!(s.timestamp, "2026-08-01T10:00:00Z", "缺 updated_at 时回落 created_at");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn extract_session_cwd_skips_lines_without_cwd() {
        // 会话 jsonl 前部可能是无 cwd 的 summary/meta 行,取首个带 cwd 的记录
        let jsonl = "{\"type\":\"summary\",\"summary\":\"t\"}\n{\"cwd\":\"/Users/dida/proj/sub\",\"type\":\"user\"}\n{\"cwd\":\"/other\",\"type\":\"user\"}";
        assert_eq!(extract_session_cwd(jsonl).as_deref(), Some("/Users/dida/proj/sub"));
    }

    #[test]
    fn extract_session_cwd_none_when_absent_or_malformed() {
        assert_eq!(extract_session_cwd("{\"type\":\"summary\"}\nnot json"), None);
        assert_eq!(extract_session_cwd(""), None);
        // 空串 cwd 不算有效
        assert_eq!(extract_session_cwd("{\"cwd\":\"\"}"), None);
    }

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
    fn ai_session_serializes_ssh_connection_id_only_when_present() {
        let mut s = AiSession {
            id: "s1".into(),
            session_type: "claude".into(),
            title: "t".into(),
            timestamp: "2026-07-05T00:00:00Z".into(),
            wsl_distro: None,
            ssh_connection_id: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("sshConnectionId"), "None 不应序列化: {json}");
        assert!(!json.contains("wslDistro"));

        s.ssh_connection_id = Some("conn-1".into());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"sshConnectionId\":\"conn-1\""), "camelCase 对齐: {json}");
    }

    #[test]
    fn claude_session_info_from_lines_finds_first_user_message() {
        let lines = [
            r#"{"type":"summary","summary":"x"}"#,
            r#"{"type":"user","message":{"content":"<local-command-caveat>skip me"},"timestamp":"2026-01-01T00:00:00Z"}"#,
            r#"{"type":"user","message":{"content":"fix the bug"},"timestamp":"2026-01-02T00:00:00Z"}"#,
        ];
        let (title, ts) = claude_session_info_from_lines(lines);
        assert_eq!(title, "fix the bug");
        assert_eq!(ts, "2026-01-02T00:00:00Z");
    }

    #[test]
    fn claude_session_info_from_lines_empty_returns_untitled() {
        let (title, ts) = claude_session_info_from_lines([]);
        assert_eq!(title, "Untitled");
        assert!(ts.is_empty());
    }

    #[test]
    fn claude_message_from_line_parses_roles_and_skips_noise() {
        let user = r#"{"type":"user","message":{"content":"hello"},"timestamp":"t1"}"#;
        let m = claude_message_from_line(user).unwrap();
        assert_eq!(m.role, "user");
        assert_eq!(m.content, "hello");
        assert_eq!(m.timestamp, "t1");

        let assistant =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]},"timestamp":"t2"}"#;
        let m = claude_message_from_line(assistant).unwrap();
        assert_eq!(m.role, "assistant");
        assert_eq!(m.content, "hi");

        // 非消息行 / 空内容 / 非 JSON → None
        assert!(claude_message_from_line(r#"{"type":"system"}"#).is_none());
        assert!(claude_message_from_line(r#"{"type":"user","message":{"content":""}}"#).is_none());
        assert!(claude_message_from_line("not json").is_none());
    }

    #[test]
    fn codex_message_from_line_parses_response_items_only() {
        let user = r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"do it"}],"timestamp":"t1"}}"#;
        let m = codex_message_from_line(user).unwrap();
        assert_eq!(m.role, "user");
        assert_eq!(m.content, "do it");

        assert!(codex_message_from_line(r#"{"type":"session_meta","payload":{}}"#).is_none());
        assert!(codex_message_from_line("garbage").is_none());
    }

    #[test]
    fn codex_meta_from_line_extracts_fields() {
        let line = r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/home/u/proj","timestamp":"2026-01-01T00:00:00Z"}}"#;
        let meta = codex_meta_from_line(line).unwrap();
        assert_eq!(meta.id, "abc");
        assert_eq!(meta.cwd, "/home/u/proj");
        assert_eq!(meta.timestamp, "2026-01-01T00:00:00Z");

        assert!(codex_meta_from_line(r#"{"type":"response_item"}"#).is_none());
        assert!(codex_meta_from_line("").is_none());
    }

    #[test]
    fn codex_user_title_from_line_skips_injected_text() {
        let injected = r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"<user_instructions>x</user_instructions>"}]}}"#;
        assert!(codex_user_title_from_line(injected).is_none());

        let agents = r##"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}]}}"##;
        assert!(codex_user_title_from_line(agents).is_none());

        let real = r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"refactor the pool"}]}}"#;
        assert_eq!(codex_user_title_from_line(real).as_deref(), Some("refactor the pool"));
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
