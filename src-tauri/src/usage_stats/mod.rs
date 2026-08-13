mod aggregate;
pub mod ledger;
mod pricing;
mod turns;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 一个待解析的会话任务。
enum SessionJob {
    /// Claude 主转录 + 其子代理转录（subagents/ 下全部 .jsonl，独立计费必须纳入）
    Claude { main: PathBuf, subagents: Vec<PathBuf> },
    Codex { path: PathBuf },
    /// Grok 一个会话是一整个目录（summary.json + updates.jsonl）
    Grok { dir: PathBuf },
}

impl SessionJob {
    fn mtime_ms(&self) -> i64 {
        match self {
            // 子代理文件可能比主转录更新（后台代理晚于主会话收尾），粗筛取两者最大
            SessionJob::Claude { main, subagents } => subagents
                .iter()
                .map(|p| turns::mtime_ms(p))
                .fold(turns::mtime_ms(main), i64::max),
            SessionJob::Codex { path } => turns::mtime_ms(path),
            // 取 updates.jsonl 而非目录：目录 mtime 在多数文件系统上只反映
            // 条目增删，正文追加不会推进它，增量同步会因此漏掉新回合
            SessionJob::Grok { dir } => turns::mtime_ms(&dir.join("updates.jsonl")),
        }
    }
}

/// 枚举 ~/.claude/projects/ 下**全部**项目的会话（账本要全量历史；
/// 项目 scope 过滤移到查询层按 cwd 终判，与 ai_sessions.rs 的目录直达是两条入口）。
fn collect_claude_jobs(home: &Path, jobs: &mut Vec<SessionJob>) {
    let projects_dir = home.join(".claude").join("projects");
    let Ok(project_entries) = fs::read_dir(&projects_dir) else {
        return;
    };
    for project in project_entries.flatten() {
        let project_path = project.path();
        if project_path.is_dir() {
            collect_claude_jobs_in_dir(&project_path, jobs);
        }
    }
}

/// 递归收集目录下全部 .jsonl（子代理转录可嵌套：subagents/workflows/wf_*/*.jsonl，
/// 只扫一层会系统性漏算 Workflow 子代理的用量）。
fn collect_jsonl_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_recursive(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// 收集单个 Claude 项目目录下的全部会话(主转录 + subagents 子转录)。
fn collect_claude_jobs_in_dir(project_path: &Path, jobs: &mut Vec<SessionJob>) {
    let Ok(entries) = fs::read_dir(project_path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mut subagents = Vec::new();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            collect_jsonl_recursive(&project_path.join(stem).join("subagents"), &mut subagents);
        }
        // read_dir 顺序平台相关：排序让 noid 顺序号与 parity 结果可复现
        subagents.sort();
        jobs.push(SessionJob::Claude { main: path, subagents });
    }
}

fn collect_codex_jobs(home: &Path, jobs: &mut Vec<SessionJob>) {
    let sessions_dir = home.join(".codex").join("sessions");
    if !sessions_dir.exists() {
        return;
    }
    let mut paths = Vec::new();
    crate::ai_sessions::collect_codex_session_paths(&sessions_dir, &mut paths);
    jobs.extend(paths.into_iter().map(|path| SessionJob::Codex { path }));
}

/// 枚举 `{grok_home}/sessions/<编码 cwd>/<session-id>/` 下的全部会话。
///
/// 与另外两家一样全量入账本，项目 scope 交给查询层按 cwd 终判——这里刻意
/// **不**解码组目录名去筛：cwd 以 `summary.json` 里的 `info.cwd` 为准，
/// 会话迁移（`/fork --worktree`、目录改名）后目录名会与之不一致。
fn collect_grok_jobs(sessions_root: &Path, jobs: &mut Vec<SessionJob>) {
    let Ok(groups) = fs::read_dir(sessions_root) else {
        return;
    };
    for group in groups.flatten() {
        let group_path = group.path();
        if !group_path.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&group_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.is_dir() && dir.join("updates.jsonl").is_file() {
                jobs.push(SessionJob::Grok { dir });
            }
        }
    }
}

/// baseurl → 展示 host（保留端口，中转站常以端口区分）。
fn url_host(url: &str) -> Option<String> {
    let s = url.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let host = s.split('/').next()?.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// grok 的 API 归属。会话记录不带 baseurl（与 Claude 同处境），而 grok 的自定义
/// 模型 baseUrl 是按模型配的、不是按会话记的，无从对历史会话精确归因——统一归到
/// 官方 host，与 Claude 缺省回落 api.anthropic.com 同口径。
const GROK_HOST: &str = "api.x.ai";

/// 供应商归属解析（baseurl 维度排行）。
/// - Claude 转录不记录 baseurl：整体按当前 ~/.claude/settings.json 的
///   env.ANTHROPIC_BASE_URL 归桶（缺省 api.anthropic.com）——历史会话按当前配置近似。
/// - Codex 按 session_meta.model_provider 查 ~/.codex/config.toml 的
///   model_providers.<id>.base_url；查不到回退 id（内置 "openai" → api.openai.com）。
/// - Grok 恒为 `api.x.ai`（见 `GROK_HOST`）。
struct ProviderResolver {
    claude_host: String,
    codex_hosts: HashMap<String, String>,
}

impl ProviderResolver {
    fn new(home: &Path) -> Self {
        let claude_host = fs::read_to_string(home.join(".claude").join("settings.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.pointer("/env/ANTHROPIC_BASE_URL")
                    .and_then(|u| u.as_str())
                    .and_then(url_host)
            })
            .unwrap_or_else(|| "api.anthropic.com".into());

        let mut codex_hosts = HashMap::new();
        if let Ok(s) = fs::read_to_string(home.join(".codex").join("config.toml")) {
            if let Ok(doc) = s.parse::<toml_edit::DocumentMut>() {
                if let Some(tbl) = doc.get("model_providers").and_then(|v| v.as_table()) {
                    for (id, item) in tbl.iter() {
                        if let Some(h) = item.get("base_url").and_then(|v| v.as_str()).and_then(url_host) {
                            codex_hosts.insert(id.to_string(), h);
                        }
                    }
                }
            }
        }
        Self { claude_host, codex_hosts }
    }

    fn resolve(&self, s: &turns::ParsedSession) -> String {
        if s.agent == "claude" {
            return self.claude_host.clone();
        }
        if s.agent == "grok" {
            return GROK_HOST.to_string();
        }
        match s.provider.as_deref() {
            Some(id) => self.codex_hosts.get(id).cloned().unwrap_or_else(|| {
                if id == "openai" { "api.openai.com".into() } else { id.to_string() }
            }),
            None => "api.openai.com".into(),
        }
    }
}

/// 单项目 scope 的 cwd 终判：用与选目录一致的 normalize(大小写/分隔符容错)
/// 比较，并放行子目录启动的会话(cwd 为项目路径的子路径)。
/// 会话枚举永远全量入账本、scope 只在查询层按本函数过滤——从项目子目录
/// 启动(目录名编码与项目根不同)的 Claude 会话也能被计入。
pub(super) fn session_in_scope(cwd: Option<&str>, project: &str) -> bool {
    let proj = crate::ai_sessions::normalize_path(project);
    cwd.is_some_and(|c| {
        let c = crate::ai_sessions::normalize_path(c);
        c == proj || c.starts_with(&format!("{proj}\\"))
    })
}

/// agent 过滤：serde 层拒收未知值(原为 String,未知值会静默退化为全扫)。
#[derive(Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentFilter {
    All,
    Claude,
    Codex,
    Grok,
}

/// 账本里存的 agent 字符串 → `ParsedSession` 要的 `&'static str`。
/// 未知值一律按 claude（账本只会写入这三种，兜底只为不 panic）。
pub(super) fn agent_from_db(agent: &str) -> &'static str {
    match agent {
        "codex" => "codex",
        "grok" => "grok",
        _ => "claude",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_matches_root_and_subdirectory_sessions() {
        // 项目根精确命中 + 子目录启动的会话放行(分隔符/大小写容错);
        // 同名前缀的兄弟目录不得误入
        assert!(session_in_scope(Some("/Users/u/proj"), "/Users/u/proj"));
        assert!(session_in_scope(Some("/Users/u/proj/packages/web"), "/Users/u/proj"));
        assert!(session_in_scope(Some("/Users/U/Proj"), "/users/u/proj"));
        assert!(!session_in_scope(Some("/Users/u/proj-other"), "/Users/u/proj"));
        assert!(!session_in_scope(None, "/Users/u/proj"));
    }

    #[test]
    fn claude_subagents_collected_recursively() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-usage-mod-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("proj");
        // 主转录 sess-1.jsonl + 一层 subagents + workflows/wf_x 深层
        let deep = project.join("sess-1").join("subagents").join("workflows").join("wf_x");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(project.join("sess-1.jsonl"), "{}\n").unwrap();
        let flat = project.join("sess-1").join("subagents").join("agent-a.jsonl");
        std::fs::write(&flat, "{}\n").unwrap();
        std::fs::write(deep.join("agent-b.jsonl"), "{}\n").unwrap();
        // 非 .jsonl 忽略
        std::fs::write(deep.join("note.txt"), "x").unwrap();

        let mut jobs = Vec::new();
        collect_claude_jobs_in_dir(&project, &mut jobs);
        assert_eq!(jobs.len(), 1);
        match &jobs[0] {
            SessionJob::Claude { subagents, .. } => {
                assert_eq!(subagents.len(), 2, "深层 workflows/wf_*/ 子代理必须纳入");
            }
            _ => panic!("expected claude job"),
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
