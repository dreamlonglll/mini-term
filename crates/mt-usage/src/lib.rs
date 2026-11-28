//! 用量统计：会话轮次解析 / SQLite 账本 / 聚合 / 计价。
//!
//! 从 `src-tauri/src/usage_stats/`（mod + turns + ledger + aggregate + pricing）
//! 整块搬来，逻辑逐字保留，只去掉 Tauri：两个 `#[tauri::command]` 变普通
//! `pub fn`，`AppHandle` 换成显式的 `app_data_dir: &Path` 与事件回调
//! （`ledger::SyncEvent` / `ledger::SyncSink`，替原来的 `app.emit`）。
//!
//! ```text
//! ~/.claude|.codex|{grok_home} 的 JSONL ─┐
//! ~/.gemini/antigravity-cli(agy)索引库   ├─sync──▶ usage.db（sessions/turns/tool_events）
//!   + transcript_full.jsonl ──────────────┘
//!                                                       │
//!                                      query ─▶ Aggregator + PricingTable
//!                                                       ▼
//!                                                UsageStatsPayload
//! ```
//!
//! 查价表的来源(models.dev 价格表的归一 / 24h 磁盘缓存 / 降级链路)在
//! [`models_dev`];拉网那一跳由调用方注入(mt-app 的 `pricing.rs`)。
//!
//! 采集与展示分离：原始 JSONL 只在同步时按文件指纹增量读一次并落账本，展示层
//! 永远查账本（毫秒级），任何参数切换都是纯查询。
//!
//! 分桶按**记录自身时刻**求当地偏移（`chrono-tz` 带 IANA 数据），DST 地区的历史
//! 记录才不会错日——这条不许简化成固定偏移。


pub mod aggregate;
pub mod ledger;
pub mod models_dev;
pub mod pricing;
pub mod turns;

pub use aggregate::{
    Aggregator, CountStat, DailyStat, ModelStat, ProjectStat, ProviderStat, RequestLogStat,
    TopSessionStat, UsageStatsPayload,
};
pub use ledger::{
    ledger_db_path, spawn_usage_ledger_sync, usage_ledger_query, usage_ledger_sync, SyncEvent,
    SyncSink,
};
pub use pricing::{cost_of, ModelPrice, PricingTable};
pub use turns::{ParsedSession, ToolUse, Turn, UsageTotals};

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use mt_ai::AgentKind;
use mt_core::path_key::windows_eq_key;

/// 一个待解析的会话任务。
enum SessionJob {
    /// Claude 主转录 + 其子代理转录（subagents/ 下全部 .jsonl，独立计费必须纳入）
    Claude { main: PathBuf, subagents: Vec<PathBuf> },
    Codex { path: PathBuf },
    /// Grok 一个会话是一整个目录（summary.json + updates.jsonl）
    Grok { dir: PathBuf },
    /// agy 一个会话 = 索引库里的一行(元信息)+ 它的 `transcript_full.jsonl`(用量)
    Agy {
        conv: mt_ai::sessions::AgyConversation,
        transcript: PathBuf,
    },
    /// CodeBuddy 主转录 + 子代理转录（同 Claude，`<id>/subagents/` 下）
    CodeBuddy {
        main: PathBuf,
        subagents: Vec<PathBuf>,
    },
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
            SessionJob::Agy { transcript, .. } => turns::mtime_ms(transcript),
            SessionJob::CodeBuddy { main, subagents } => subagents
                .iter()
                .map(|p| turns::mtime_ms(p))
                .fold(turns::mtime_ms(main), i64::max),
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

/// 枚举 `~/.codebuddy/projects/` 下全部项目的会话（账本要全量历史）。
/// 与 Claude 一样把 `<id>/subagents/` 里的子代理转录并入主会话计费。
fn collect_codebuddy_jobs(home: &Path, jobs: &mut Vec<SessionJob>) {
    let projects_dir = home.join(".codebuddy").join("projects");
    let Ok(project_entries) = fs::read_dir(&projects_dir) else {
        return;
    };
    for project in project_entries.flatten() {
        let project_path = project.path();
        if !project_path.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&project_path) else {
            continue;
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
            subagents.sort();
            jobs.push(SessionJob::CodeBuddy {
                main: path,
                subagents,
            });
        }
    }
}

fn collect_codex_jobs(home: &Path, jobs: &mut Vec<SessionJob>) {
    let sessions_dir = home.join(".codex").join("sessions");
    if !sessions_dir.exists() {
        return;
    }
    let mut paths = Vec::new();
    mt_ai::sessions::collect_codex_session_paths(&sessions_dir, &mut paths);
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

/// 枚举 agy 索引库(`{app_data_dir}/conversation_summaries.db`)里的全部 CLI 会话,
/// **含子代理会话** —— 那也是实打实花掉的 token,项目归属用它自己的工作区。
///
/// 只收已经落了 transcript 的(开了 agy 没发消息的空会话、刚建还没写第一步的会话
/// 没有用量)。与别家一样全量入账本,项目 scope 交给查询层按 cwd 终判。
/// 没装 agy(索引库不在)是常态,什么都不收。
fn collect_agy_jobs(app_data_dir: &Path, jobs: &mut Vec<SessionJob>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    // 索引库读不懂(agy 改了表结构之类)每轮同步都会再撞一次,自动刷新 5s 一轮,
    // 只在本进程第一次撞上时留痕,别把日志刷满
    static WARNED: AtomicBool = AtomicBool::new(false);

    let db = mt_ai::sessions::agy_summaries_db_path_in(app_data_dir);
    if !db.is_file() {
        return;
    }
    let conversations = match mt_ai::sessions::read_agy_conversations_from(&db) {
        Ok(c) => c,
        Err(e) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!("[usage_stats] 读 agy 会话索引失败,本轮跳过 agy: {e}");
            }
            return;
        }
    };
    for conv in conversations {
        // id 已由索引读取层挡过穿越;拼不出路径的一律跳过
        let Some(transcript) = mt_ai::sessions::agy_transcript_path_in(app_data_dir, &conv.id)
        else {
            continue;
        };
        if transcript.is_file() {
            jobs.push(SessionJob::Agy { conv, transcript });
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

/// agy 的 API 归属。transcript / 索引都不记请求端点,agy 也不让用户改;本机 agy
/// 1.5.12 日志里模型列表、配额与 Code Assist 请求全走这个 host(实测 2026-11),
/// 统一归到这里,与 grok 恒归官方 host 同口径。
const AGY_HOST: &str = "daily-cloudcode-pa.googleapis.com";

/// 供应商归属解析（baseurl 维度排行）。
/// - Claude 转录不记录 baseurl：整体按当前 ~/.claude/settings.json 的
///   env.ANTHROPIC_BASE_URL 归桶（缺省 api.anthropic.com）——历史会话按当前配置近似。
/// - Codex 按 session_meta.model_provider 查 ~/.codex/config.toml 的
///   model_providers.<id>.base_url；查不到回退 id（内置 "openai" → api.openai.com）。
/// - Grok 恒为 `api.x.ai`（见 `GROK_HOST`）。
/// - agy 恒为 `daily-cloudcode-pa.googleapis.com`（见 `AGY_HOST`）。
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
        match AgentKind::parse(s.agent) {
            Some(AgentKind::Claude) => return self.claude_host.clone(),
            Some(AgentKind::Grok) => return GROK_HOST.to_string(),
            Some(AgentKind::Agy) => return AGY_HOST.to_string(),
            _ => {}
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
pub(crate) fn session_in_scope(cwd: Option<&str>, project: &str) -> bool {
    let proj = windows_eq_key(project);
    cwd.is_some_and(|c| {
        let c = windows_eq_key(c);
        // 键是正斜杠形态(见 `windows_eq_key`)
        c == proj || c.starts_with(&format!("{proj}/"))
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
    Agy,
    CodeBuddy,
}

/// 账本里存的 agent 字符串 → `ParsedSession` 要的 `&'static str`。
/// 只认已接入用量统计的四家(与 [`AgentFilter`] 同集);未知值一律按 claude
/// （账本只会写入这四种，兜底只为不 panic）。不借 agent 表的 `history` 位:
/// pi 接了历史面板却没接用量统计,两件事不再同进同出。
pub(crate) fn agent_from_db(agent: &str) -> &'static str {
    AgentKind::parse(agent)
        .filter(|k| {
            matches!(
                k,
                AgentKind::Claude
                    | AgentKind::Codex
                    | AgentKind::Grok
                    | AgentKind::Agy
                    | AgentKind::CodeBuddy
            )
        })
        .unwrap_or(AgentKind::Claude)
        .key()
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
        // Windows 形态:盘符大小写、正反斜杠混用、尾随分隔符
        assert!(session_in_scope(Some(r"d:\git\Proj\sub"), r"D:\Git\proj\"));
        assert!(session_in_scope(Some("D:/Git/proj"), r"D:\Git\proj"));
        assert!(!session_in_scope(Some(r"D:\Git\proj2"), r"D:\Git\proj"));
    }

    /// 账本 agent 串 → 解析器口径:只认接了用量统计的四家,其余按 claude(与改造前逐字相同)。
    #[test]
    fn agent_from_db_keeps_four_way_split() {
        assert_eq!(agent_from_db("claude"), "claude");
        assert_eq!(agent_from_db("codex"), "codex");
        assert_eq!(agent_from_db("grok"), "grok");
        assert_eq!(agent_from_db("agy"), "agy");
        assert_eq!(agent_from_db("codebuddy"), "codebuddy");
        for other in ["", "omp", "pi", "opencode", "什么鬼"] {
            assert_eq!(agent_from_db(other), "claude", "{other:?}");
        }
    }

    /// 筛选项的 serde 口径:面板传 `"agy"`,认得出;未知值仍被拒收。
    #[test]
    fn agent_filter_accepts_agy() {
        let f: AgentFilter = serde_json::from_str(r#""agy""#).unwrap();
        assert!(f == AgentFilter::Agy);
        let f: AgentFilter = serde_json::from_str(r#""codebuddy""#).unwrap();
        assert!(f == AgentFilter::CodeBuddy);
        assert!(serde_json::from_str::<AgentFilter>(r#""gemini""#).is_err());
    }

    /// 按 agy 1.5.12 的真实表结构建索引库:CLI 会话(含子代理)只要落了 transcript
    /// 就收;没 transcript 的空会话、桌面版 / IDE 的会话(`app_data_dir` 不是
    /// `antigravity-cli`)不收;库不在什么都不收。
    #[test]
    fn agy_jobs_collected_from_index_including_subagents() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-usage-agy-jobs-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut jobs = Vec::new();
        collect_agy_jobs(&root, &mut jobs);
        assert!(jobs.is_empty(), "没装 agy:索引库不在,什么都不收");

        let db = mt_ai::sessions::agy_summaries_db_path_in(&root);
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT \"\",`preview` text NOT NULL DEFAULT \"\",`step_count` integer NOT NULL DEFAULT 0,`last_modified_time` datetime NOT NULL,`workspace_uris` text NOT NULL,`parent_conversation_id` text NOT NULL DEFAULT \"\",`nesting_depth` integer NOT NULL DEFAULT 0,`last_user_input_time` datetime NOT NULL,`app_data_dir` text NOT NULL DEFAULT \"\",PRIMARY KEY (`conversation_id`));
                 INSERT INTO conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,parent_conversation_id,nesting_depth,last_user_input_time,app_data_dir) VALUES
                   ('main-1','','hi',6,'2026-11-19 13:48:11+00:00','[\"file:///d:/Git/proj\"]','',0,'2026-11-19 13:47:42+00:00','antigravity-cli'),
                   ('sub-1','','',6,'2026-11-19 13:48:08+00:00','[\"file:///D:/Git/proj/sub\"]','main-1',1,'0001-01-01 00:00:00+00:00','antigravity-cli'),
                   ('empty-1','','',0,'2026-11-19 13:00:00+00:00','[\"file:///d:/Git/proj\"]','',0,'0001-01-01 00:00:00+00:00','antigravity-cli'),
                   ('ide-1','','x',9,'2026-11-19 13:00:00+00:00','[\"file:///d:/Git/proj\"]','',0,'0001-01-01 00:00:00+00:00','antigravity');",
            )
            .unwrap();
        for id in ["main-1", "sub-1", "ide-1"] {
            let t = mt_ai::sessions::agy_transcript_path_in(&root, id).unwrap();
            std::fs::create_dir_all(t.parent().unwrap()).unwrap();
            std::fs::write(&t, "{}\n").unwrap();
        }

        collect_agy_jobs(&root, &mut jobs);
        let mut got: Vec<(String, bool)> = jobs
            .iter()
            .map(|j| match j {
                SessionJob::Agy { conv, transcript } => {
                    assert!(transcript.is_file());
                    (conv.id.clone(), conv.is_subagent())
                }
                _ => panic!("expected agy job"),
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            [("main-1".to_string(), false), ("sub-1".to_string(), true)],
            "子代理会话必须纳入;空会话与 IDE 会话不收"
        );
        std::fs::remove_dir_all(&root).ok();
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
