//! 三家 AI CLI 的会话记录读取与谱系扫描(原 `src-tauri/src/ai_sessions.rs`)。
//!
//! **只有 Claude / Codex / Grok 有可解析的会话记录**([`agent_has_session_log`])。
//! opencode / pi 这类只靠输入检测识别的 agent 拿得到状态徽章与移动端指令,但没有
//! 对话镜像、AI 历史面板与用量统计 —— 镜像必须据此跳过启发式绑定,否则会绑到同
//! 项目其它 agent 的最新会话文件,把别人的对话贴到该 pane 上。
//!
//! 原本的 `#[tauri::command]` 全部去掉,函数签名不变;跨模块共享的
//! `pub(crate)` 项一律放宽成 `pub` —— 原来的消费者(remote_ssh / usage_stats /
//! mobile_mirror)迁移后各自成 crate,只能从这里 `pub` 出去。

mod codex;
mod grok;
mod lineage;

pub use codex::{
    CodexSessionMeta, codex_message_from_line, codex_meta_from_line, codex_user_title_from_line,
    collect_codex_session_paths, load_codex_thread_names,
};
pub use grok::{
    GrokUpdateParser, decode_grok_cwd_dir, find_grok_cwd_dirs, find_grok_session_dir,
    grok_updates_path,
};
pub use lineage::{
    BookkeptLineageEdge, LineageEdge, branch_title_from_texts, claude_branch_title_from_lines,
    claude_fork_edge_from_lines, codex_fork_edge_from_meta_line, latest_model_from_lines,
    scan_session_lineage,
};

use codex::{
    get_codex_sessions, get_codex_sessions_in, read_codex_session_content,
    read_wsl_codex_session_content,
};
use grok::{get_grok_sessions, read_grok_session_content};
use lineage::latest_model_from_file_tail;

use serde::Serialize;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

const MAX_CLAUDE_SESSION_FILES_TO_SCAN: usize = 300;
const MAX_CODEX_SESSION_FILES_TO_SCAN: usize = 500;
// WSL 侧经 \\wsl$ 走 9P 协议,逐文件读慢(毫秒级往返),上限下调。
const MAX_WSL_CLAUDE_SESSION_FILES_TO_SCAN: usize = 100;
const MAX_WSL_CODEX_SESSION_FILES_TO_SCAN: usize = 200;
pub const MAX_SESSIONS_PER_SOURCE: usize = 80;
pub const MAX_TOTAL_SESSIONS: usize = 120;
const SESSION_CACHE_TTL: Duration = Duration::from_secs(2);
// WSL 扫描代价高(9P + 可能触发 VM 冷启动),TTL 放宽;手动刷新走 force 绕过。
const WSL_SESSION_CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct CachedSessions {
    pub loaded_at: Instant,
    pub sessions: Vec<AiSession>,
}

static SESSION_CACHE: OnceLock<Mutex<HashMap<String, CachedSessions>>> = OnceLock::new();

/// 会话列表缓存(Windows / WSL / SSH 远程三来源共用同一 map,key 前缀区分)。
/// 锁契约:即取即放,**绝不跨慢 IO 持锁**(见 spec/backend/wsl-unc-session-scanning.md)。
pub fn session_cache() -> &'static Mutex<HashMap<String, CachedSessions>> {
    SESSION_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSession {
    pub id: String,
    pub session_type: String, // "claude" | "codex" | "grok"
    pub title: String,
    pub timestamp: String, // ISO 8601
    /// 会话最新使用的模型（Claude: 尾窗反扫 assistant 行 message.model;
    /// Codex: turn_context 的 payload.model; Grok: summary.json 的
    /// current_model_id）。前端按模型名推厂商图标——CLI ≠ 模型厂商
    /// （claude CLI 挂 GLM/DeepSeek 中转是常见用法）。None = 识别不出,回落 CLI 图标。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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

/// 该 agent 是否有本 crate 能解析的会话记录(Claude / Codex / Grok 三家)。
///
/// 输入检测能认出的 agent 比这宽(pi / opencode 也在 `detect::AI_COMMANDS` 里),
/// 它们**没有**可解析的记录文件。调用方(对话镜像)必须据此跳过启发式绑定:
/// 「按项目找最新的 claude/codex/grok 记录」对一个 pi pane 调用,会把同项目里别家
/// 的对话贴到这个 pane 上(串台)。宁可空镜像。
///
/// 用 `contains` 而非全等:hook 上报的 agent 是 `claude-code`,输入检测是 `claude`。
///
/// (原本长在 `mobile_mirror.rs` 里;记录形态的判定属于本 crate,镜像迁到
/// mt-relay 后从这里引。)
pub fn agent_has_session_log(agent: &str) -> bool {
    let agent = agent.to_ascii_lowercase();
    agent.contains("claude") || agent.contains("codex") || agent.contains("grok")
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
/// session id 白名单:字母数字与 `-` `_`(Claude UUID / Codex rollout id /
/// Grok UUIDv7 均落在其中)。id 来自前端 invoke 参数或持久化数据,不可信,
/// 拼进文件路径前必须过这道,挡 `../` 一类穿越。
pub fn session_id_path_safe(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn lookup_ai_session_cwd(session_id: String) -> Option<String> {
    // 与前端 resume 命令的校验同口径,顺带防路径拼接注入
    if !session_id_path_safe(&session_id) {
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
/// pub:SSH 远程项目的会话扫描(remote_ssh.rs → 将来的 mt-project)复用同一编码。
pub fn encode_project_path(project_path: &str) -> String {
    project_path
        .trim_end_matches(['/', '\\'])
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// 目录名是否为编码名的「变体」:大小写不同(drvfs 大小写不敏感,WSL 内
/// `cd /mnt/d/GIT/foo` 也能进同一目录)或仅多出尾部 `-`(带尾部斜杠的同一项目)。
/// 编码有损,变体命中后仍需读 jsonl 内真实 cwd 精确校验,防止吃进兄弟项目。
pub fn is_encoded_variant(dir_name: &str, encoded: &str) -> bool {
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
pub fn find_claude_project_dirs(project_path: &str) -> Vec<PathBuf> {
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
pub fn normalize_path(path: &str) -> String {
    path.replace('/', "\\")
        .to_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

/// Unix 语义路径统一化(小写 + 保留 `/`,去尾部 `/`),用于 WSL 内 / SSH 远程 cwd 比较
pub fn normalize_unix_path(path: &str) -> String {
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
    if let Some(wsl) = crate::util::parse_wsl_unc(project_path) {
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
            model: latest_model_from_file_tail(&path),
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
    let lines: Vec<String> = reader.lines().take(50).filter_map(Result::ok).collect();
    claude_session_info_from_lines(lines.iter().map(String::as_str))
}

/// 从会话文件的前若干行提取 (title, timestamp)。行级纯函数,本地(BufReader)
/// 与远程(SFTP 读头部字节后按行切)两条路径共用。
pub fn claude_session_info_from_lines<'a>(
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

/// 按 mtime 降序排列会话文件路径并截到 limit。
/// decorate-sort-undecorate:每个 path 只 stat 一次,而不是在比较器里 stat
/// (比较器每次 2 次 syscall × O(n log n) 次,数百个 Codex rollout 文件时是可观的 IO)。
/// 取不到 mtime(扫描后刚被删/无权限)的一律沉到末尾,彼此之间按路径降序 ——
/// Codex/Claude 的文件名带时间戳前缀,路径序近似时间序。旧比较器对这类路径
/// 给的是不满足传递性的序,顺带一并修正。
pub fn sort_newest_session_paths(paths: &mut Vec<PathBuf>, limit: usize) {
    let mut decorated: Vec<(Option<SystemTime>, PathBuf)> = std::mem::take(paths)
        .into_iter()
        .map(|p| (p.metadata().and_then(|m| m.modified()).ok(), p))
        .collect();

    decorated.sort_by(|(ta, pa), (tb, pb)| match (ta, tb) {
        (Some(ta), Some(tb)) => tb.cmp(ta),
        // 有 mtime 的一律排在无 mtime 的前面,两边都没有则退回路径降序
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => pb.cmp(pa),
    });

    decorated.truncate(limit);
    paths.extend(decorated.into_iter().map(|(_, p)| p));
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
pub fn claude_message_from_line(line: &str) -> Option<AiSessionMessage> {
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

/// 读取会话正文。`wsl_distro` 有值时从对应发行版的 UNC 位置读取(WSL 会话)。
/// 标注 async:WSL 冷启动 + 9P 读取可能秒级,不能阻塞主线程。
pub fn get_ai_session_content(
    session_type: String,
    session_id: String,
    project_path: String,
    wsl_distro: Option<String>,
) -> Result<Vec<AiSessionMessage>, String> {
    // id 会拼进各家会话文件路径(本机/WSL 的 `<id>.jsonl`),统一在分发口挡穿越
    if !session_id_path_safe(&session_id) {
        return Err("非法会话 id".to_string());
    }
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

pub fn get_ai_sessions(project_path: String) -> Result<Vec<AiSession>, String> {
    let cache_key = normalize_path(&project_path);

    {
        let cache = session_cache()
            .lock()
            .map_err(|_| "session cache lock poisoned".to_string())?;
        if let Some(cached) = cache.get(&cache_key) {
            if cached.loaded_at.elapsed() < SESSION_CACHE_TTL {
                return Ok(cached.sessions.clone());
            }
        }
        // 扫描期间不持锁:三家会话目录全量扫盘可能秒级,别把 WSL 侧
        // get_wsl_ai_sessions 与其它项目的查询一起卡住(与下方 WSL 侧同一口径)
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

    // 重新取锁写回:两次锁窗口之间可能有并发扫描先写入,与 WSL 侧一致地后来者覆盖
    // (两份结果扫的是同一批文件,取更新的那份没有正确性差别)
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

/// 获取项目在 WSL 发行版内的 claude/codex 会话。
/// - WSL 根项目(UNC 路径):distro 从路径推导,忽略入参;
/// - WSL 关联项目(Windows 路径):按入参 distro + /mnt 映射;
/// - 无法推导来源 / 一切 IO 失败:静默返回空列表。
/// `force` 绕过缓存,供手动刷新使用。
/// 标注 async:9P 扫描 + 可能的 VM 冷启动是秒级操作,不能阻塞主线程。
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

    // ---- 可解析会话记录的白名单 ----

    /// 只有 Claude/Codex/Grok 有可解析的记录;pi/opencode 必须落在白名单外,
    /// 否则镜像会退启发式绑到同项目别家的会话文件(串台)。
    #[test]
    fn only_claude_codex_and_grok_have_session_logs() {
        for agent in ["claude", "claude-code", "codex", "Codex", "grok", "Grok"] {
            assert!(agent_has_session_log(agent), "{agent} 应有会话记录");
        }
        for agent in ["pi", "opencode", "", "gemini"] {
            assert!(
                !agent_has_session_log(agent),
                "{agent} 不应被认为有会话记录"
            );
        }
    }

    // ---- 会话文件排序 ----

    /// decorate-sort 后的结果必须与 mtime 降序一致(每个 path 只 stat 一次)。
    #[test]
    fn sort_newest_session_paths_orders_by_mtime_desc() {
        let dir = std::env::temp_dir().join(format!(
            "mini-term-sort-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        // 文件名故意与时间序相反(z 最老、a 最新),证明排的是 mtime 不是路径
        let names = ["z.jsonl", "m.jsonl", "a.jsonl"];
        let mut created = Vec::new();
        for name in names {
            let p = dir.join(name);
            fs::write(&p, b"{}\n").unwrap();
            // 文件系统 mtime 分辨率有限,逐个拉开间隔
            std::thread::sleep(Duration::from_millis(20));
            created.push(p);
        }

        let mut paths = created.clone();
        sort_newest_session_paths(&mut paths, 10);
        let expect: Vec<PathBuf> = created.iter().rev().cloned().collect();
        assert_eq!(paths, expect, "应按 mtime 降序(最新在前)");

        // limit 生效:只留最新的两个
        let mut paths = created.clone();
        sort_newest_session_paths(&mut paths, 2);
        assert_eq!(paths, expect[..2].to_vec());

        // 取不到 mtime 的排在有 mtime 的后面,且不丢元素
        let ghost = dir.join("nonexistent.jsonl");
        let mut paths = vec![ghost.clone(), created[0].clone()];
        sort_newest_session_paths(&mut paths, 10);
        assert_eq!(paths, vec![created[0].clone(), ghost]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sort_newest_session_paths_handles_empty_and_zero_limit() {
        let mut empty: Vec<PathBuf> = Vec::new();
        sort_newest_session_paths(&mut empty, 5);
        assert!(empty.is_empty());

        let mut one = vec![PathBuf::from("/nonexistent/x.jsonl")];
        sort_newest_session_paths(&mut one, 0);
        assert!(one.is_empty(), "limit=0 应清空");
    }

    // ---- session id 白名单 ----

    #[test]
    fn session_id_path_safe_rejects_traversal_and_metachars() {
        assert!(session_id_path_safe("0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e"));
        assert!(session_id_path_safe("abc_123"));
        assert!(!session_id_path_safe(""));
        assert!(!session_id_path_safe("../../etc/passwd"));
        assert!(!session_id_path_safe("a/b"));
        assert!(!session_id_path_safe("a b"));
    }

    #[test]
    fn get_ai_session_content_rejects_bad_id_before_fs() {
        // 分发口即拒绝,不落到任何文件系统访问
        let r = get_ai_session_content(
            "claude".into(),
            "../../etc/passwd".into(),
            "/tmp".into(),
            None,
        );
        assert!(r.is_err());
    }

    #[test]
    fn extract_session_cwd_skips_lines_without_cwd() {
        // 会话 jsonl 前部可能是无 cwd 的 summary/meta 行,取首个带 cwd 的记录
        let jsonl = "{\"type\":\"summary\",\"summary\":\"t\"}\n{\"cwd\":\"/Users/dida/proj/sub\",\"type\":\"user\"}\n{\"cwd\":\"/other\",\"type\":\"user\"}";
        assert_eq!(
            extract_session_cwd(jsonl).as_deref(),
            Some("/Users/dida/proj/sub")
        );
    }

    #[test]
    fn extract_session_cwd_none_when_absent_or_malformed() {
        assert_eq!(
            extract_session_cwd("{\"type\":\"summary\"}\nnot json"),
            None
        );
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
            model: None,
            wsl_distro: None,
            ssh_connection_id: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("sshConnectionId"), "None 不应序列化: {json}");
        assert!(!json.contains("wslDistro"));

        s.ssh_connection_id = Some("conn-1".into());
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains("\"sshConnectionId\":\"conn-1\""),
            "camelCase 对齐: {json}"
        );
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

        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]},"timestamp":"t2"}"#;
        let m = claude_message_from_line(assistant).unwrap();
        assert_eq!(m.role, "assistant");
        assert_eq!(m.content, "hi");

        // 非消息行 / 空内容 / 非 JSON → None
        assert!(claude_message_from_line(r#"{"type":"system"}"#).is_none());
        assert!(claude_message_from_line(r#"{"type":"user","message":{"content":""}}"#).is_none());
        assert!(claude_message_from_line("not json").is_none());
    }

    #[test]
    fn path_style_normalize_uses_matching_semantics() {
        assert_eq!(PathStyle::Windows.normalize("D:/Git/Foo/"), r"d:\git\foo");
        assert_eq!(
            PathStyle::Unix.normalize("/mnt/d/Git/Foo/"),
            "/mnt/d/git/foo"
        );
    }
}
