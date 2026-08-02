mod aggregate;
mod pricing;
mod turns;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use aggregate::{Aggregator, UsageStatsPayload};
use pricing::{ModelPrice, PricingTable};

/// 代际取消：新请求 / cancel 都令计数 +1，worker 每处理一个文件比对代际，
/// 不等即静默退出（对齐 search.rs 的 start/cancel 模式，前端同样按 requestId 双保险）。
static GENERATION: AtomicU64 = AtomicU64::new(0);

const FLUSH_EVERY_FILES: usize = 16;
const FLUSH_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressPayload<'a> {
    request_id: &'a str,
    processed: usize,
    total: usize,
    partial: &'a UsageStatsPayload,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DonePayload<'a> {
    request_id: &'a str,
    stats: UsageStatsPayload,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorPayload<'a> {
    request_id: &'a str,
    error: String,
}

/// 一个待解析的会话任务。
enum SessionJob {
    /// Claude 主转录 + 其子代理转录（subagents/*.jsonl，独立计费必须纳入）
    Claude { main: PathBuf, subagents: Vec<PathBuf> },
    Codex { path: PathBuf },
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
        }
    }
}

/// 枚举 ~/.claude/projects/ 下**全部**项目的会话（统计需要全局枚举，
/// 与 ai_sessions.rs 的按项目 cwd 过滤是两条入口）。
fn collect_claude_jobs(home: &Path, project: Option<&str>, jobs: &mut Vec<SessionJob>) {
    // 单项目 scope:目录名即 cwd 编码,复用 ai_sessions 的匹配原语直达,不全盘枚举
    if let Some(project) = project {
        for dir in crate::ai_sessions::find_claude_project_dirs(project) {
            collect_claude_jobs_in_dir(&dir, jobs);
        }
        return;
    }
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
            let sub_dir = project_path.join(stem).join("subagents");
            if let Ok(subs) = fs::read_dir(&sub_dir) {
                for sub in subs.flatten() {
                    let sp = sub.path();
                    if sp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        subagents.push(sp);
                    }
                }
            }
        }
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

/// 供应商归属解析（baseurl 维度排行）。
/// - Claude 转录不记录 baseurl：整体按当前 ~/.claude/settings.json 的
///   env.ANTHROPIC_BASE_URL 归桶（缺省 api.anthropic.com）——历史会话按当前配置近似。
/// - Codex 按 session_meta.model_provider 查 ~/.codex/config.toml 的
///   model_providers.<id>.base_url；查不到回退 id（内置 "openai" → api.openai.com）。
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
        match s.provider.as_deref() {
            Some(id) => self.codex_hosts.get(id).cloned().unwrap_or_else(|| {
                if id == "openai" { "api.openai.com".into() } else { id.to_string() }
            }),
            None => "api.openai.com".into(),
        }
    }
}

/// agent 过滤：serde 层拒收未知值(原为 String,未知值会静默退化为全扫)。
#[derive(Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentFilter {
    All,
    Claude,
    Codex,
}

/// 一次扫描的请求参数（打包随 worker 线程移动）。
struct ScanParams {
    request_id: String,
    agents: AgentFilter,
    since_ms: i64,
    /// 窗口上界(custom range 的「止」,含当日);None = 开区间到现在
    until_ms: Option<i64>,
    /// 单项目 scope:登记项目的绝对路径;None = 整机全部
    project_path: Option<String>,
    tz_offset_minutes: i32,
    tz_name: Option<String>,
    hourly: bool,
}

fn run_scan(app: &AppHandle, my_gen: u64, params: &ScanParams, pricing: PricingTable) {
    let request_id = params.request_id.as_str();
    let agents = params.agents;
    let since_ms = params.since_ms;
    let Some(home) = dirs::home_dir() else {
        let _ = app.emit(
            "usage-stats-error",
            ErrorPayload { request_id, error: "无法获取 home 目录".into() },
        );
        return;
    };

    let mut jobs: Vec<SessionJob> = Vec::new();
    if agents != AgentFilter::Codex {
        collect_claude_jobs(&home, params.project_path.as_deref(), &mut jobs);
    }
    if agents != AgentFilter::Claude {
        collect_codex_jobs(&home, &mut jobs);
    }
    // mtime 粗筛：最后写入早于窗口起点的文件不可能有窗内 turn（仅省解析，
    // 窗口判定仍由聚合层逐 turn 终判）
    jobs.retain(|j| j.mtime_ms() >= since_ms);
    let total = jobs.len();

    let thread_names = if agents != AgentFilter::Claude {
        crate::ai_sessions::load_codex_thread_names(&home.join(".codex"))
    } else {
        HashMap::new()
    };

    let mut agg = Aggregator::new(
        since_ms,
        params.until_ms,
        params.tz_offset_minutes,
        params.tz_name.as_deref(),
        params.hourly,
    );
    let resolver = ProviderResolver::new(&home);
    let mut last_flush = Instant::now();
    let mut since_flush = 0usize;

    for (processed, job) in jobs.iter().enumerate() {
        if GENERATION.load(Ordering::SeqCst) != my_gen {
            return; // 已被新请求/取消淘汰，静默退出
        }
        let parsed = match job {
            SessionJob::Claude { main, subagents } => turns::parse_claude_session(main, subagents),
            SessionJob::Codex { path } => turns::parse_codex_session(path, &thread_names),
        };
        if let Some(mut s) = parsed {
            // 单项目 scope 的 cwd 终判:Claude 已按目录直达(此处双保险),
            // Codex rollout 无目录索引,只能解析后按 cwd 过滤(mtime 粗筛仍生效)
            let in_scope = match params.project_path.as_deref() {
                Some(proj) => s
                    .cwd
                    .as_deref()
                    .is_some_and(|c| c.trim_end_matches(['/', '\\']) == proj.trim_end_matches(['/', '\\'])),
                None => true,
            };
            if in_scope {
                s.provider = Some(resolver.resolve(&s));
                agg.add_session(&s, &pricing);
            }
        }

        since_flush += 1;
        if since_flush >= FLUSH_EVERY_FILES || last_flush.elapsed() >= FLUSH_EVERY {
            let partial = agg.snapshot();
            let _ = app.emit(
                "usage-stats-progress",
                ProgressPayload { request_id, processed: processed + 1, total, partial: &partial },
            );
            since_flush = 0;
            last_flush = Instant::now();
        }
    }

    if GENERATION.load(Ordering::SeqCst) != my_gen {
        return;
    }
    let _ = app.emit(
        "usage-stats-done",
        DonePayload { request_id, stats: agg.snapshot() },
    );
}

/// 启动一次统计扫描：立即返回，工作进后台线程，结果经
/// usage-stats-progress / usage-stats-done / usage-stats-error 事件流回。
/// `pricing` 由前端拉 models.dev 后传入（$/token）；`agents` 为 all|claude|codex；
/// `since_ms` 为窗口起点（前端按本地日历日算好）；`tz_offset_minutes` 为
/// JS getTimezoneOffset() 原值，供分桶按本地时区；`hourly` = 「今天」视图按小时分桶。
#[tauri::command]
pub fn start_usage_stats(
    app: AppHandle,
    request_id: String,
    agents: AgentFilter,
    since_ms: i64,
    until_ms: Option<i64>,
    project_path: Option<String>,
    tz_offset_minutes: i32,
    tz_name: Option<String>,
    hourly: bool,
    pricing: HashMap<String, ModelPrice>,
) -> Result<(), String> {
    let my_gen = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let table = PricingTable::new(pricing);
    let params = ScanParams {
        request_id,
        agents,
        since_ms,
        until_ms,
        project_path,
        tz_offset_minutes,
        tz_name,
        hourly,
    };

    std::thread::spawn(move || {
        // catch_unwind 兜底：panic 也要给前端一个 error 事件，否则骨架屏永远转圈
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_scan(&app, my_gen, &params, table);
        }));
        if outcome.is_err() {
            eprintln!("[usage_stats] worker panicked during scan {}", params.request_id);
            let _ = app.emit(
                "usage-stats-error",
                ErrorPayload { request_id: &params.request_id, error: "统计扫描异常终止".into() },
            );
        }
    });

    Ok(())
}

/// 取消当前扫描（关 Modal 即停）：代际 +1，现役 worker 在下一个文件边界退出。
#[tauri::command]
pub fn cancel_usage_stats() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}
