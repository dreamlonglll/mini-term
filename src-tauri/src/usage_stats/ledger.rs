//! 使用统计账本（rusqlite，存 `{app_data_dir}/usage.db`）：采集与展示分离的中间层。
//!
//! 原始 JSONL 只在同步时读一次 → 落账本；展示层 `usage_ledger_query` 永远查账本，
//! 毫秒级返回，任何参数切换都是纯查询。同步按文件粒度增量：`sync_state` 记
//! (path, mtime, size) 指纹，未变跳过（一次 stat 的成本），变了整文件重解析后
//! UPSERT——重写/compact/回卷被主键幂等吸收。
//! 设计合同：docs/plans/2026-08-02-usage-stats-ledger-redesign.md。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::aggregate::{Aggregator, UsageStatsPayload};
use super::pricing::{ModelPrice, PricingTable};
use super::turns::{self, ParsedSession, Turn, UsageTotals};
use super::{collect_claude_jobs, collect_codex_jobs, AgentFilter, ProviderResolver, SessionJob};

/// 账本 schema（设计 §1）。成本不落库：定价会更新，查询时按前端传入的定价表现算。
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
  session_id  TEXT PRIMARY KEY,
  agent       TEXT NOT NULL,
  cwd         TEXT,
  title       TEXT,
  provider    TEXT,
  file_path   TEXT NOT NULL,
  mtime_ms    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS turns (
  request_id     TEXT PRIMARY KEY,
  session_id     TEXT NOT NULL,
  ts_ms          INTEGER,
  model          TEXT,
  input          INTEGER NOT NULL DEFAULT 0,
  output         INTEGER NOT NULL DEFAULT 0,
  reasoning      INTEGER NOT NULL DEFAULT 0,
  cache_read     INTEGER NOT NULL DEFAULT 0,
  cache_write    INTEGER NOT NULL DEFAULT 0,
  cache_write_1h INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns(ts_ms);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);
CREATE TABLE IF NOT EXISTS sync_state (
  file_path TEXT PRIMARY KEY,
  mtime_ms  INTEGER NOT NULL,
  size      INTEGER NOT NULL
);
";

/// 全局同步互斥：同一时刻只有一个同步在跑（try_lock 失败即有现役同步，直接放弃）。
/// Connection 每次命令内打开（WAL 下读写连接互不阻塞，查询永远秒回不等同步）。
static SYNC_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LedgerProgressPayload {
    processed: usize,
    total: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LedgerSyncedPayload {
    added: usize,
}

pub(super) struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// 打开账本；打开/建表失败视为损坏 → 删除重建 + 由空 sync_state 触发 backfill
    /// （数据源头是 JSONL，账本可再生，无需备份机制）。
    pub(super) fn open(db_path: &Path) -> Result<Self, String> {
        match Self::open_raw(db_path) {
            Ok(conn) => Ok(Self { conn }),
            Err(first_err) => {
                for suffix in ["", "-wal", "-shm"] {
                    let mut p = db_path.as_os_str().to_owned();
                    p.push(suffix);
                    let _ = fs::remove_file(PathBuf::from(p));
                }
                Self::open_raw(db_path)
                    .map(|conn| Self { conn })
                    .map_err(|e| format!("账本重建失败: {e}（原错误: {first_err}）"))
            }
        }
    }

    fn open_raw(db_path: &Path) -> rusqlite::Result<Connection> {
        let conn = Connection::open(db_path)?;
        // journal_mode 语句有返回行，走 query_row；synchronous=NORMAL 在 WAL 下
        // 每事务免 fsync（只在 checkpoint），backfill 数千文件的落库才够快
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(conn)
    }

    /// sync_state 为空 = 账本新建（或损坏重建）→ 本轮同步是 backfill，要发进度。
    fn sync_state_empty(&self) -> rusqlite::Result<bool> {
        let row: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM sync_state LIMIT 1", [], |r| r.get(0))
            .optional()?;
        Ok(row.is_none())
    }

    /// 同步一个会话任务。指纹未变返回 Ok(false)（跳过）；变了/新文件整组重解析，
    /// 全部 turn UPSERT 后更新 sync_state，返回 Ok(true)。
    fn sync_job(
        &mut self,
        job: &SessionJob,
        thread_names: &HashMap<String, String>,
    ) -> rusqlite::Result<bool> {
        let (key_path, mtime, size) = job_fingerprint(job);
        let key = key_path.to_string_lossy().into_owned();
        let unchanged = self
            .conn
            .query_row(
                "SELECT 1 FROM sync_state WHERE file_path = ?1 AND mtime_ms = ?2 AND size = ?3",
                params![key, mtime, size],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some();
        if unchanged {
            return Ok(false);
        }

        let parsed = match job {
            SessionJob::Claude { main, subagents } => turns::parse_claude_session(main, subagents),
            SessionJob::Codex { path } => turns::parse_codex_session(path, thread_names),
        };
        // 解析失败（文件消失/无读权限）：不记指纹，下轮枚举到再试
        let Some(s) = parsed else { return Ok(false) };

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions(session_id, agent, cwd, title, provider, file_path, mtime_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id) DO UPDATE SET
               agent = excluded.agent, cwd = excluded.cwd, title = excluded.title,
               provider = excluded.provider, file_path = excluded.file_path,
               mtime_ms = excluded.mtime_ms",
            params![s.session_id, s.agent, s.cwd, s.title, s.provider, key, s.mtime_ms],
        )?;
        // 顺序号身份（noid:/codex:）先清后插：文件 compact/重写变短时残留的
        // 高下标 turn 才能收敛（UPSERT 只吸收重复，吸收不了缩短）。
        // claude:{message_id} 全局唯一不受影响，fork 复制历史由主键天然去重
        let seq_prefix = if s.agent == "codex" {
            format!("codex:{}:%", s.session_id)
        } else {
            format!("noid:{}:%", s.session_id)
        };
        tx.execute("DELETE FROM turns WHERE request_id LIKE ?1", params![seq_prefix])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO turns(request_id, session_id, ts_ms, model,
                                   input, output, reasoning, cache_read, cache_write, cache_write_1h)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(request_id) DO UPDATE SET
                   session_id = excluded.session_id, ts_ms = excluded.ts_ms,
                   model = excluded.model, input = excluded.input, output = excluded.output,
                   reasoning = excluded.reasoning, cache_read = excluded.cache_read,
                   cache_write = excluded.cache_write, cache_write_1h = excluded.cache_write_1h",
            )?;
            for (i, t) in s.turns.iter().enumerate() {
                // turn 身份规则（设计 §1.1）：Claude 有 id 全局唯一；无 id / Codex
                // 按会话内顺序号（append-only 文件下稳定；重排场景由上面的先清后插收敛）
                let request_id = match (&t.message_id, s.agent) {
                    (Some(id), "claude") => format!("claude:{id}"),
                    (_, "codex") => format!("codex:{}:{i}", s.session_id),
                    _ => format!("noid:{}:{i}", s.session_id),
                };
                stmt.execute(params![
                    request_id,
                    s.session_id,
                    t.timestamp_ms,
                    t.model,
                    t.usage.input as i64,
                    t.usage.output as i64,
                    t.usage.reasoning as i64,
                    t.usage.cache_read as i64,
                    t.usage.cache_write as i64,
                    t.usage.cache_write_1h as i64,
                ])?;
            }
        }
        tx.execute(
            "INSERT INTO sync_state(file_path, mtime_ms, size) VALUES(?1, ?2, ?3)
             ON CONFLICT(file_path) DO UPDATE SET
               mtime_ms = excluded.mtime_ms, size = excluded.size",
            params![key, mtime, size],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// 按窗口/agent 查 turns+sessions，组回 ParsedSession（喂现有 Aggregator，
    /// UsageStatsPayload 形状不变）。窗口判定与聚合层同口径：turn 缺时间戳回退
    /// session.mtime_ms。项目 scope 过滤在命令层（需要 normalize，SQL 不好做）。
    fn query_sessions(
        &self,
        agents: AgentFilter,
        since_ms: i64,
        until_ms: Option<i64>,
    ) -> rusqlite::Result<Vec<ParsedSession>> {
        let agent_str = match agents {
            AgentFilter::All => None,
            AgentFilter::Claude => Some("claude"),
            AgentFilter::Codex => Some("codex"),
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT t.session_id, t.ts_ms, t.model,
                    t.input, t.output, t.reasoning, t.cache_read, t.cache_write, t.cache_write_1h,
                    s.agent, s.cwd, s.title, s.provider, s.mtime_ms
             FROM turns t JOIN sessions s ON s.session_id = t.session_id
             WHERE COALESCE(t.ts_ms, s.mtime_ms) >= ?1
               AND COALESCE(t.ts_ms, s.mtime_ms) <= ?2
               AND (?3 IS NULL OR s.agent = ?3)
             ORDER BY t.session_id, t.rowid",
        )?;
        let mut sessions: Vec<ParsedSession> = Vec::new();
        let mut rows = stmt.query(params![since_ms, until_ms.unwrap_or(i64::MAX), agent_str])?;
        while let Some(row) = rows.next()? {
            let session_id: String = row.get(0)?;
            if sessions.last().map(|s| s.session_id.as_str()) != Some(session_id.as_str()) {
                let agent: String = row.get(9)?;
                sessions.push(ParsedSession {
                    // &'static str 映射（账本只会存这两种值）
                    agent: if agent == "codex" { "codex" } else { "claude" },
                    session_id,
                    cwd: row.get(10)?,
                    title: row.get(11)?,
                    provider: row.get(12)?,
                    mtime_ms: row.get(13)?,
                    turns: Vec::new(),
                });
            }
            let cur = sessions.last_mut().expect("just pushed");
            cur.turns.push(Turn {
                // 主键已把同 message_id 收敛为一行，聚合层无需再跨文件去重
                message_id: None,
                model: row.get(2)?,
                timestamp_ms: row.get(1)?,
                usage: UsageTotals {
                    input: row.get::<_, i64>(3)? as u64,
                    output: row.get::<_, i64>(4)? as u64,
                    reasoning: row.get::<_, i64>(5)? as u64,
                    cache_read: row.get::<_, i64>(6)? as u64,
                    cache_write: row.get::<_, i64>(7)? as u64,
                    cache_write_1h: row.get::<_, i64>(8)? as u64,
                },
            });
        }
        Ok(sessions)
    }
}

/// 同步指纹：Claude job 的 mtime 取主转录与全部子代理转录最大值、size 取总和
/// ——任一子文件更新都触发整组重解析（mtime 秒级精度的文件系统靠 size 兜底）。
fn job_fingerprint(job: &SessionJob) -> (PathBuf, i64, i64) {
    fn size_of(p: &Path) -> i64 {
        fs::metadata(p).map(|m| m.len() as i64).unwrap_or(0)
    }
    match job {
        SessionJob::Claude { main, subagents } => {
            let mtime = job.mtime_ms();
            let size = subagents.iter().map(|p| size_of(p)).sum::<i64>() + size_of(main);
            (main.clone(), mtime, size)
        }
        SessionJob::Codex { path } => (path.clone(), turns::mtime_ms(path), size_of(path)),
    }
}

fn ledger_db_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法获取应用数据目录: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("创建应用数据目录失败: {e}"))?;
    Ok(dir.join("usage.db"))
}

/// 查询账本 → 聚合快照（同步命令，毫秒级）。`pricing` 由前端拉 models.dev 后传入
/// （$/token）；窗口/时区/分桶参数语义与旧 start_usage_stats 一致；项目 scope
/// 沿用 normalize + 子路径规则按 cwd 终判。
#[tauri::command]
pub fn usage_ledger_query(
    app: AppHandle,
    agents: AgentFilter,
    since_ms: i64,
    until_ms: Option<i64>,
    project_path: Option<String>,
    tz_offset_minutes: i32,
    tz_name: Option<String>,
    hourly: bool,
    pricing: HashMap<String, ModelPrice>,
) -> Result<UsageStatsPayload, String> {
    let ledger = Ledger::open(&ledger_db_path(&app)?)?;
    let sessions = ledger
        .query_sessions(agents, since_ms, until_ms)
        .map_err(|e| format!("账本查询失败: {e}"))?;

    let home = dirs::home_dir().ok_or("无法获取 home 目录")?;
    let resolver = ProviderResolver::new(&home);
    let table = PricingTable::new(pricing);
    let mut agg = Aggregator::new(since_ms, until_ms, tz_offset_minutes, tz_name.as_deref(), hourly);
    for mut s in sessions {
        // 单项目 scope 的 cwd 终判(session_in_scope:normalize + 子路径放行)
        let in_scope = match project_path.as_deref() {
            Some(proj) => super::session_in_scope(s.cwd.as_deref(), proj),
            None => true,
        };
        if in_scope {
            s.provider = Some(resolver.resolve(&s));
            agg.add_session(&s, &table);
        }
    }
    Ok(agg.snapshot())
}

/// 触发一次增量同步：立即返回，工作进后台线程（现役同步在跑则直接放弃，
/// 由其收尾）。完成后 emit `usage-ledger-synced {added}`（added = 重解析的
/// 文件数，0 表示无变化前端可跳过重查）；backfill（sync_state 为空）期间
/// 另按节流 emit `usage-ledger-progress {processed, total}`。
#[tauri::command]
pub fn usage_ledger_sync(app: AppHandle) -> Result<(), String> {
    let db_path = ledger_db_path(&app)?;
    std::thread::spawn(move || {
        // catch_unwind 兜底：sync panic 只损失本轮增量，账本与查询不受影响
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_sync(&app, &db_path);
        }));
        if outcome.is_err() {
            eprintln!("[usage_stats] ledger sync panicked");
        }
    });
    Ok(())
}

fn run_sync(app: &AppHandle, db_path: &Path) {
    let Ok(_guard) = SYNC_LOCK.try_lock() else {
        return; // 已有同步在跑，本次触发合并进现役轮
    };
    let mut ledger = match Ledger::open(db_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[usage_stats] 账本打开失败: {e}");
            return;
        }
    };
    let Some(home) = dirs::home_dir() else { return };

    let mut jobs: Vec<SessionJob> = Vec::new();
    collect_claude_jobs(&home, &mut jobs);
    collect_codex_jobs(&home, &mut jobs);
    let thread_names = crate::ai_sessions::load_codex_thread_names(&home.join(".codex"));

    let backfill = ledger.sync_state_empty().unwrap_or(false);
    let total = jobs.len();
    let mut added = 0usize;
    let mut last_emit = Instant::now();
    for (i, job) in jobs.iter().enumerate() {
        match ledger.sync_job(job, &thread_names) {
            Ok(true) => added += 1,
            Ok(false) => {}
            // 单文件失败不拖垮全量（下轮指纹仍不匹配，会重试）
            Err(e) => eprintln!("[usage_stats] 同步文件失败: {e}"),
        }
        if backfill && last_emit.elapsed() >= Duration::from_millis(250) {
            let _ = app.emit(
                "usage-ledger-progress",
                LedgerProgressPayload { processed: i + 1, total },
            );
            last_emit = Instant::now();
        }
    }
    let _ = app.emit("usage-ledger-synced", LedgerSyncedPayload { added });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_stats::pricing::ModelPrice;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "mini-term-ledger-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn claude_line(id: &str, ts: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","cwd":"/p/alpha","message":{{"id":"{id}","model":"claude-opus-4-8","usage":{{"input_tokens":10,"output_tokens":{output},"cache_read_input_tokens":5}}}}}}"#
        )
    }

    fn codex_lines(session_id: &str) -> String {
        let meta = format!(
            r#"{{"type":"session_meta","timestamp":"2026-08-01T09:00:00.000Z","payload":{{"id":"{session_id}","cwd":"/p/beta","model_provider":"openai"}}}}"#
        );
        let ctx = r#"{"type":"turn_context","timestamp":"2026-08-01T09:00:01.000Z","payload":{"model":"gpt-5.3-codex","cwd":"/p/beta"}}"#;
        let t1 = r#"{"type":"event_msg","timestamp":"2026-08-01T10:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}"#;
        let t2 = r#"{"type":"event_msg","timestamp":"2026-08-01T11:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}}"#;
        format!("{meta}\n{ctx}\n{t1}\n{t2}\n")
    }

    fn turn_count(ledger: &Ledger) -> i64 {
        ledger
            .conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap()
    }

    fn pricing() -> PricingTable {
        let mut m = HashMap::new();
        m.insert(
            "claude-opus-4-8".to_string(),
            ModelPrice { input: 1e-6, output: 5e-6, cache_read: 1e-7, cache_write: 1.25e-6 },
        );
        PricingTable::new(m)
    }

    #[test]
    fn sync_is_idempotent_and_incremental() {
        let root = temp_root("idem");
        let claude = root.join("sess-a.jsonl");
        fs::write(
            &claude,
            format!("{}\n{}\n", claude_line("m1", "2026-08-01T10:00:00Z", 50), claude_line("m2", "2026-08-01T10:05:00Z", 70)),
        )
        .unwrap();
        let codex = root.join("rollout-b.jsonl");
        fs::write(&codex, codex_lines("sess-b")).unwrap();
        let jobs = vec![
            SessionJob::Claude { main: claude.clone(), subagents: vec![] },
            SessionJob::Codex { path: codex.clone() },
        ];
        let names = HashMap::new();

        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        for j in &jobs {
            assert!(ledger.sync_job(j, &names).unwrap(), "新文件必须重解析");
        }
        assert_eq!(turn_count(&ledger), 4);

        // 指纹未变 → 跳过
        for j in &jobs {
            assert!(!ledger.sync_job(j, &names).unwrap(), "未变文件必须跳过");
        }
        // 强制整文件重解析（清指纹模拟 mtime 变化）→ UPSERT 幂等，数量不变
        ledger.conn.execute("DELETE FROM sync_state", []).unwrap();
        for j in &jobs {
            assert!(ledger.sync_job(j, &names).unwrap());
        }
        assert_eq!(turn_count(&ledger), 4, "幂等重跑数量不得变化");

        // 追加一个 turn → 增量吸收
        let mut content = fs::read_to_string(&claude).unwrap();
        content.push_str(&claude_line("m3", "2026-08-01T10:10:00Z", 9));
        content.push('\n');
        fs::write(&claude, content).unwrap();
        assert!(ledger.sync_job(&jobs[0], &names).unwrap());
        assert_eq!(turn_count(&ledger), 5);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ledger_query_matches_in_memory_aggregation() {
        let root = temp_root("parity");
        let claude = root.join("sess-a.jsonl");
        let sub_dir = root.join("sess-a").join("subagents");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(
            &claude,
            format!("{}\n{}\n", claude_line("m1", "2026-08-01T10:00:00Z", 50), claude_line("m2", "2026-08-02T10:05:00Z", 70)),
        )
        .unwrap();
        let sub = sub_dir.join("agent-a.jsonl");
        fs::write(&sub, format!("{}\n", claude_line("m9", "2026-08-01T12:00:00Z", 33))).unwrap();
        let codex = root.join("rollout-b.jsonl");
        fs::write(&codex, codex_lines("sess-b")).unwrap();
        let jobs = vec![
            SessionJob::Claude { main: claude.clone(), subagents: vec![sub.clone()] },
            SessionJob::Codex { path: codex.clone() },
        ];
        let names = HashMap::new();
        let table = pricing();

        // 旧路径等价核心：parse → 内存聚合（provider 保持解析原值，不跑 resolver
        // ——两侧同口径，resolver 读真实 home 配置会让测试环境相关）
        let mut mem: Vec<ParsedSession> = jobs
            .iter()
            .filter_map(|j| match j {
                SessionJob::Claude { main, subagents } => turns::parse_claude_session(main, subagents),
                SessionJob::Codex { path } => turns::parse_codex_session(path, &names),
            })
            .collect();
        mem.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let mut agg_mem = Aggregator::new(0, None, -480, Some("Asia/Shanghai"), false);
        for s in &mem {
            agg_mem.add_session(s, &table);
        }

        // 新路径：落库再查 → 同一 Aggregator
        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        for j in &jobs {
            ledger.sync_job(j, &names).unwrap();
        }
        let db_sessions = ledger.query_sessions(AgentFilter::All, 0, None).unwrap();
        let mut agg_db = Aggregator::new(0, None, -480, Some("Asia/Shanghai"), false);
        for s in &db_sessions {
            agg_db.add_session(s, &table);
        }

        let a = serde_json::to_value(agg_mem.snapshot()).unwrap();
        let b = serde_json::to_value(agg_db.snapshot()).unwrap();
        assert_eq!(a, b, "落库再查必须与内存聚合逐字段一致");

        // 窗口/agent 过滤在查询层生效
        let day2 = turns::parse_rfc3339_ms("2026-08-02T00:00:00Z").unwrap();
        let only_late = ledger.query_sessions(AgentFilter::All, day2, None).unwrap();
        assert_eq!(only_late.len(), 1);
        assert_eq!(only_late[0].turns.len(), 1);
        let only_codex = ledger.query_sessions(AgentFilter::Codex, 0, None).unwrap();
        assert_eq!(only_codex.len(), 1);
        assert_eq!(only_codex[0].agent, "codex");
        assert_eq!(only_codex[0].provider.as_deref(), Some("openai"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claude_fork_same_message_id_stored_once() {
        let root = temp_root("fork");
        let a = root.join("sess-a.jsonl");
        let b = root.join("sess-b.jsonl");
        // fork 复制历史：两个会话文件含同一条 message_id
        fs::write(&a, format!("{}\n", claude_line("m1", "2026-08-01T10:00:00Z", 50))).unwrap();
        fs::write(
            &b,
            format!("{}\n{}\n", claude_line("m1", "2026-08-01T10:00:00Z", 50), claude_line("m2", "2026-08-01T11:00:00Z", 7)),
        )
        .unwrap();
        let names = HashMap::new();
        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        ledger
            .sync_job(&SessionJob::Claude { main: a, subagents: vec![] }, &names)
            .unwrap();
        ledger
            .sync_job(&SessionJob::Claude { main: b, subagents: vec![] }, &names)
            .unwrap();
        // 主键天然去重：m1 只有一行，归属最后解析的会话
        assert_eq!(turn_count(&ledger), 2);
        let owner: String = ledger
            .conn
            .query_row("SELECT session_id FROM turns WHERE request_id = 'claude:m1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(owner, "sess-b");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_rewrite_shrink_converges() {
        let root = temp_root("shrink");
        let codex = root.join("rollout-b.jsonl");
        fs::write(&codex, codex_lines("sess-b")).unwrap();
        let names = HashMap::new();
        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        let job = SessionJob::Codex { path: codex.clone() };
        ledger.sync_job(&job, &names).unwrap();
        assert_eq!(turn_count(&ledger), 2);

        // compact/重写变短：只剩 1 个 turn → 顺序号先清后插，残留收敛
        let meta = r#"{"type":"session_meta","timestamp":"2026-08-01T09:00:00.000Z","payload":{"id":"sess-b","cwd":"/p/beta","model_provider":"openai"}}"#;
        let t1 = r#"{"type":"event_msg","timestamp":"2026-08-01T10:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}"#;
        fs::write(&codex, format!("{meta}\n{t1}\n")).unwrap();
        ledger.sync_job(&job, &names).unwrap();
        assert_eq!(turn_count(&ledger), 1, "缩短残留必须收敛");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn corrupted_db_is_rebuilt() {
        let root = temp_root("corrupt");
        let db = root.join("usage.db");
        fs::write(&db, "definitely not a sqlite file").unwrap();
        let ledger = Ledger::open(&db).expect("损坏账本必须删除重建");
        assert!(ledger.sync_state_empty().unwrap());
        fs::remove_dir_all(&root).ok();
    }
}
