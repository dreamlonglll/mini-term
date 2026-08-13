//! 使用统计账本（rusqlite，存 `{app_data_dir}/usage.db`）：采集与展示分离的中间层。
//!
//! 原始 JSONL 只在同步时读一次 → 落账本；展示层 `usage_ledger_query` 永远查账本，
//! 毫秒级返回，任何参数切换都是纯查询。同步按文件粒度增量：`sync_state` 记
//! (path, mtime, size) 指纹，未变跳过（一次 stat 的成本），变了整会话先删后插
//! ——重写/compact/回卷/收缩都严格收敛为当前文件的镜像。
//! 设计合同：docs/plans/2026-08-02-usage-stats-ledger-redesign.md。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::aggregate::{Aggregator, UsageStatsPayload};
use super::pricing::{ModelPrice, PricingTable};
use super::turns::{self, ParsedSession, Turn, UsageTotals};
use super::{
    collect_claude_jobs, collect_codex_jobs, collect_grok_jobs, AgentFilter, ProviderResolver,
    SessionJob,
};

/// 账本 schema 版本：结构变更时 +1。open 时版本不匹配即删表重建（账本可从
/// JSONL 再生，空 sync_state 自动触发 backfill，无需逐版本迁移脚本）。
/// v3：新增 tool_events 表（工具/Shell/MCP 排行，设计 §2.2）。
const SCHEMA_VERSION: i64 = 3;

/// 账本 schema（设计 §1）。成本不落库：定价会更新，查询时按前端传入的定价表现算。
/// turns 主键为 (session_id, request_id)：每会话各存自己的份，fork/subagent 复制的
/// 历史允许跨会话重复落库，跨文件 message_id 去重在聚合层做（与旧内存路径同规则，
/// 归属确定、不随同步顺序漂移）。
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
  session_id     TEXT NOT NULL,
  request_id     TEXT NOT NULL,
  message_id     TEXT,
  ts_ms          INTEGER,
  model          TEXT,
  input          INTEGER NOT NULL DEFAULT 0,
  output         INTEGER NOT NULL DEFAULT 0,
  reasoning      INTEGER NOT NULL DEFAULT 0,
  cache_read     INTEGER NOT NULL DEFAULT 0,
  cache_write    INTEGER NOT NULL DEFAULT 0,
  cache_write_1h INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (session_id, request_id)
);
CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns(ts_ms);
CREATE TABLE IF NOT EXISTS tool_events (
  session_id TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  kind       TEXT NOT NULL,
  name       TEXT NOT NULL,
  ts_ms      INTEGER,
  dedup_key  TEXT,
  PRIMARY KEY (session_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_tool_events_ts ON tool_events(ts_ms);
CREATE TABLE IF NOT EXISTS sync_state (
  file_path TEXT PRIMARY KEY,
  mtime_ms  INTEGER NOT NULL,
  size      INTEGER NOT NULL
);
";

/// 全局同步互斥：同一时刻只有一个同步在跑。运行期间的新触发经 SYNC_PENDING
/// 合并进现役轮收尾补跑（见 run_coalesced），不会被丢弃。
/// Connection 每次命令内打开（WAL 下读写连接互不阻塞，查询永远秒回不等同步）。
static SYNC_LOCK: Mutex<()> = Mutex::new(());
static SYNC_PENDING: AtomicBool = AtomicBool::new(false);

/// 同步触发的合并语义：任何触发先置 pending 再取锁。
/// - `blocking = false`（定时 / 打开面板的后台触发）：抢不到锁立即返回 false，
///   由现役持锁者收尾时消费 pending 补跑一轮——运行中途落盘的新增量最多滞后
///   一轮，而不是等下一次外部触发。
/// - `blocking = true`（刷新按钮）：排队等现役轮次收尾。返回时必然已有一轮
///   「起始晚于本次置位 pending」的同步跑完，故账本已含本次触发的增量——
///   前端据此做「先同步再查」，不会先闪一次同步前的旧值。
///
/// 返回是否由本次调用取得锁并执行（blocking 下现役者已代跑完 pending 时，
/// 本次可能一轮都不用跑，仍返回 true——语义是「同步已完成」而非「我跑了」）。
///
/// 注意：`blocking = true` 不可在 round 内重入调用（std Mutex 非重入，会死锁），
/// round 内的触发一律走 `blocking = false`。
///
/// （blocking=false 的残余竞窗：持锁者最后一次消费 pending 之后、放锁之前的触发
/// 会推迟到下一次外部触发；窗口极窄，且文件指纹保证届时必然补齐，不丢数据只延时。）
fn run_coalesced(
    lock: &Mutex<()>,
    pending: &AtomicBool,
    blocking: bool,
    mut round: impl FnMut(),
) -> bool {
    pending.store(true, Ordering::SeqCst);
    // 锁内数据是 ()，中毒不代表任何状态损坏（每轮从 db + 文件指纹重建），
    // 恢复 guard 继续；否则一次 sync panic 会让后续所有同步在应用整个
    // 生命周期内静默 no-op，账本从此停更。
    let guard = if blocking {
        match lock.lock() {
            Ok(g) => Some(g),
            Err(p) => Some(p.into_inner()),
        }
    } else {
        match lock.try_lock() {
            Ok(g) => Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    };
    let Some(_guard) = guard else { return false };
    while pending.swap(false, Ordering::SeqCst) {
        round();
    }
    true
}

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
    /// 打开账本；仅**真损坏**（NotADatabase/DatabaseCorrupt）才删除重建 + 由空
    /// sync_state 触发 backfill（数据源头是 JSONL，账本可再生）。其余错误
    /// （锁竞争/权限/磁盘满等环境性失败）原样上抛——误判成损坏会把健康账本
    /// 从活跃写入者脚下删掉。
    pub(super) fn open(db_path: &Path) -> Result<Self, String> {
        match Self::open_raw(db_path) {
            Ok(conn) => Ok(Self { conn }),
            Err(first_err) if is_corruption(&first_err) => {
                for suffix in ["", "-wal", "-shm"] {
                    let mut p = db_path.as_os_str().to_owned();
                    p.push(suffix);
                    let _ = fs::remove_file(PathBuf::from(p));
                }
                Self::open_raw(db_path)
                    .map(|conn| Self { conn })
                    .map_err(|e| format!("账本重建失败: {e}（原错误: {first_err}）"))
            }
            Err(e) => Err(format!("账本打开失败: {e}")),
        }
    }

    fn open_raw(db_path: &Path) -> rusqlite::Result<Connection> {
        let mut conn = Connection::open(db_path)?;
        // 与同步写入者的瞬时锁竞争按等待处理，不作为打开失败冒泡
        conn.busy_timeout(Duration::from_millis(5000))?;
        // journal_mode 语句有返回行，走 query_row；synchronous=NORMAL 在 WAL 下
        // 每事务免 fsync（只在 checkpoint），backfill 数千文件的落库才够快
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        // 版本不匹配（含旧库）→ 删表重建，空 sync_state 触发 backfill。
        // IMMEDIATE 事务先取写锁再复读版本：并发打开（查询命令 vs 后台同步）
        // 只有一个连接执行重建，后到者拿到锁后读到新版本直接跳过——否则会把
        // 对方刚建好、甚至已开始回填的新表再 DROP 一遍
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            tx.execute_batch(
                "DROP TABLE IF EXISTS turns; DROP TABLE IF EXISTS sessions;
                 DROP TABLE IF EXISTS tool_events; DROP TABLE IF EXISTS sync_state;",
            )?;
            tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        }
        tx.execute_batch(SCHEMA)?;
        tx.commit()?;
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
            SessionJob::Grok { dir } => turns::parse_grok_session(dir),
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
        // 按会话整删重插：文件收缩（compact/重写变短/子代理转录删除）的残留行
        // 才能收敛——UPSERT 只吸收重复，吸收不了缩短。fork 复制的历史各会话
        // 自己存一份，不会误删别的会话的行
        tx.execute("DELETE FROM turns WHERE session_id = ?1", params![s.session_id])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO turns(session_id, request_id, message_id, ts_ms, model,
                                   input, output, reasoning, cache_read, cache_write, cache_write_1h)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(session_id, request_id) DO UPDATE SET
                   message_id = excluded.message_id, ts_ms = excluded.ts_ms,
                   model = excluded.model, input = excluded.input, output = excluded.output,
                   reasoning = excluded.reasoning, cache_read = excluded.cache_read,
                   cache_write = excluded.cache_write, cache_write_1h = excluded.cache_write_1h",
            )?;
            for (i, t) in s.turns.iter().enumerate() {
                // turn 身份规则（设计 §1.1）：会话内 Claude 按 message id、无 id /
                // Codex 按顺序号（append-only 文件下稳定）；主/子转录复制同一条
                // 消息时由 ON CONFLICT 在会话内吸收
                let request_id = match (&t.message_id, s.agent) {
                    (Some(id), "claude") => format!("claude:{id}"),
                    // grok 的键是 `prompt_id#model`（一个回合可跨多个模型），
                    // 与 Claude 同理靠它在会话内吸收 fork 复制来的重复回合
                    (Some(id), "grok") => format!("grok:{id}"),
                    (_, "codex") => format!("codex:{i}"),
                    _ => format!("noid:{i}"),
                };
                stmt.execute(params![
                    s.session_id,
                    request_id,
                    t.message_id,
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
        // 工具事件与 turns 同规则:按会话整删重插,收缩严格收敛
        tx.execute("DELETE FROM tool_events WHERE session_id = ?1", params![s.session_id])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO tool_events(session_id, seq, kind, name, ts_ms, dedup_key)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (i, u) in s.tool_uses.iter().enumerate() {
                stmt.execute(params![
                    s.session_id,
                    i as i64,
                    u.kind,
                    u.name,
                    u.timestamp_ms,
                    u.dedup_key,
                ])?;
            }
        }
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
            AgentFilter::Grok => Some("grok"),
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT t.session_id, t.ts_ms, t.model,
                    t.input, t.output, t.reasoning, t.cache_read, t.cache_write, t.cache_write_1h,
                    s.agent, s.cwd, s.title, s.provider, s.mtime_ms, t.message_id
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
                    agent: super::agent_from_db(&agent),
                    session_id,
                    cwd: row.get(10)?,
                    title: row.get(11)?,
                    provider: row.get(12)?,
                    mtime_ms: row.get(13)?,
                    turns: Vec::new(),
                    tool_uses: Vec::new(),
                });
            }
            let cur = sessions.last_mut().expect("just pushed");
            cur.turns.push(Turn {
                // 还原 message_id：fork/subagent 复制的历史每会话各存一份，
                // 跨会话去重交回聚合层 seen_ids（首见者得；ORDER BY session_id
                // 使归属确定，不随同步顺序漂移）
                message_id: row.get(14)?,
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
        drop(rows);

        // 工具事件:同窗口/agent 口径查回并挂到对应会话;只有工具事件、无计费
        // turn 的会话也要出现在结果里(排行不丢)
        let mut idx: HashMap<String, usize> = sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (s.session_id.clone(), i))
            .collect();
        let mut stmt = self.conn.prepare_cached(
            "SELECT te.session_id, te.kind, te.name, te.ts_ms, te.dedup_key,
                    s.agent, s.cwd, s.title, s.provider, s.mtime_ms
             FROM tool_events te JOIN sessions s ON s.session_id = te.session_id
             WHERE COALESCE(te.ts_ms, s.mtime_ms) >= ?1
               AND COALESCE(te.ts_ms, s.mtime_ms) <= ?2
               AND (?3 IS NULL OR s.agent = ?3)
             ORDER BY te.session_id, te.seq",
        )?;
        let mut rows = stmt.query(params![since_ms, until_ms.unwrap_or(i64::MAX), agent_str])?;
        while let Some(row) = rows.next()? {
            let session_id: String = row.get(0)?;
            let i = match idx.get(&session_id) {
                Some(&i) => i,
                None => {
                    let agent: String = row.get(5)?;
                    sessions.push(ParsedSession {
                        agent: super::agent_from_db(&agent),
                        session_id: session_id.clone(),
                        cwd: row.get(6)?,
                        title: row.get(7)?,
                        provider: row.get(8)?,
                        mtime_ms: row.get(9)?,
                        turns: Vec::new(),
                        tool_uses: Vec::new(),
                    });
                    idx.insert(session_id, sessions.len() - 1);
                    sessions.len() - 1
                }
            };
            let kind: String = row.get(1)?;
            sessions[i].tool_uses.push(super::turns::ToolUse {
                // &'static str 映射（账本只会存这三种值）
                kind: match kind.as_str() {
                    "shell" => "shell",
                    "mcp" => "mcp",
                    _ => "tool",
                },
                name: row.get(2)?,
                timestamp_ms: row.get(3)?,
                dedup_key: row.get(4)?,
            });
        }
        Ok(sessions)
    }
}

/// 判定 rusqlite 错误是否为数据库文件本体损坏（可安全删除重建的唯一情形）。
fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
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
        // 指纹落在 updates.jsonl 上：目录本身的 mtime 不随正文追加推进
        SessionJob::Grok { dir } => {
            let updates = dir.join("updates.jsonl");
            let mtime = turns::mtime_ms(&updates);
            let size = size_of(&updates);
            (updates, mtime, size)
        }
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

/// 查询账本 → 聚合快照（async 命令：Tauri v2 的同步命令跑在主线程，而本命令
/// 打开连接可能等 busy_timeout——落主线程会把整个窗口冻住；async 移到 runtime
/// 工作线程，查询本身仍是毫秒级）。`pricing` 由前端拉 models.dev 后传入
/// （$/token）；窗口/时区/分桶参数语义与旧 start_usage_stats 一致；项目 scope
/// 沿用 normalize + 子路径规则按 cwd 终判。
#[tauri::command]
pub async fn usage_ledger_query(
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

/// 触发一次增量同步。
/// - `wait` 缺省 / false（定时刷新、打开面板）：立即返回，工作进后台线程
///   （现役同步在跑则直接放弃，由其收尾），前端不被阻塞。
/// - `wait = true`（刷新按钮）：等同步真正跑完再返回，前端据此做「先同步再查」。
///   旧实现一律 fire-and-forget，前端点刷新时先查到的必然是同步前的旧账本，
///   真值要等 synced 事件才补上——表现为每点一次刷新金额跳一次。
///
/// 完成后 emit `usage-ledger-synced {added}`（added = 重解析的文件数，0 表示
/// 无变化前端可跳过重查）；backfill（sync_state 为空）期间另按节流 emit
/// `usage-ledger-progress {processed, total}`。
#[tauri::command]
pub async fn usage_ledger_sync(app: AppHandle, wait: Option<bool>) -> Result<(), String> {
    let db_path = ledger_db_path(&app)?;
    let blocking = wait.unwrap_or(false);
    let run = move || {
        // catch_unwind 兜底：sync panic 只损失本轮增量，账本与查询不受影响
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_sync(&app, &db_path, blocking);
        }));
        if outcome.is_err() {
            eprintln!("[usage_stats] ledger sync panicked");
        }
    };
    if blocking {
        // 同步是重文件 I/O + sqlite 写：交给 blocking 线程池，不占 tokio worker
        return tauri::async_runtime::spawn_blocking(run)
            .await
            .map_err(|e| format!("同步任务失败: {e}"));
    }
    std::thread::spawn(run);
    Ok(())
}

fn run_sync(app: &AppHandle, db_path: &Path, blocking: bool) {
    run_coalesced(&SYNC_LOCK, &SYNC_PENDING, blocking, || sync_round(app, db_path));
}

fn sync_round(app: &AppHandle, db_path: &Path) {
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
    if let Some(grok_home) = crate::hook_registry::grok_home() {
        collect_grok_jobs(&grok_home.join("sessions"), &mut jobs);
    }
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
        let tool = r#"{"type":"response_item","timestamp":"2026-08-01T09:30:00.000Z","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"rg -n foo src\"}","call_id":"call_z1"}}"#;
        let t1 = r#"{"type":"event_msg","timestamp":"2026-08-01T10:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}"#;
        let t2 = r#"{"type":"event_msg","timestamp":"2026-08-01T11:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}}"#;
        format!("{meta}\n{ctx}\n{tool}\n{t1}\n{t2}\n")
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

    fn claude_bash_line(toolu: &str, ts: &str, cmd: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","cwd":"/p/alpha","message":{{"id":"mt-{toolu}","model":"claude-opus-4-8","usage":{{"input_tokens":0,"output_tokens":0}},"content":[{{"type":"tool_use","id":"{toolu}","name":"Bash","input":{{"command":"{cmd}"}}}}]}}}}"#
        )
    }

    #[test]
    fn tool_events_roundtrip_and_shrink_converge() {
        let root = temp_root("toolev");
        let claude = root.join("sess-a.jsonl");
        fs::write(
            &claude,
            format!(
                "{}\n{}\n",
                claude_line("m1", "2026-08-01T10:00:00Z", 50),
                claude_bash_line("toolu_A", "2026-08-01T10:00:01Z", "git status")
            ),
        )
        .unwrap();
        let names = HashMap::new();
        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        let job = SessionJob::Claude { main: claude.clone(), subagents: vec![] };
        ledger.sync_job(&job, &names).unwrap();

        let sessions = ledger.query_sessions(AgentFilter::All, 0, None).unwrap();
        assert_eq!(sessions.len(), 1);
        let got: Vec<(&str, &str, Option<&str>)> = sessions[0]
            .tool_uses
            .iter()
            .map(|u| (u.kind, u.name.as_str(), u.dedup_key.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![("tool", "Bash", Some("toolu_A")), ("shell", "git", Some("toolu_A#s"))],
            "工具事件必须完整落库并按原样查回"
        );
        assert_eq!(
            sessions[0].tool_uses[0].timestamp_ms,
            turns::parse_rfc3339_ms("2026-08-01T10:00:01Z")
        );

        // 收缩:工具行被移除 → 重扫后 tool_events 收敛
        fs::write(&claude, format!("{}\n", claude_line("m1", "2026-08-01T10:00:00Z", 50))).unwrap();
        ledger.sync_job(&job, &names).unwrap();
        let sessions = ledger.query_sessions(AgentFilter::All, 0, None).unwrap();
        assert!(sessions[0].tool_uses.is_empty(), "收缩残留的工具事件必须收敛");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tool_only_session_still_queryable_in_window() {
        // 只有工具事件、无计费 turn 的会话:窗口内也要能查回(排行不丢)
        let root = temp_root("toolonly");
        let claude = root.join("sess-t.jsonl");
        fs::write(
            &claude,
            format!("{}\n", claude_bash_line("toolu_X", "2026-08-01T10:00:00Z", "ls -la")),
        )
        .unwrap();
        let names = HashMap::new();
        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        ledger
            .sync_job(&SessionJob::Claude { main: claude, subagents: vec![] }, &names)
            .unwrap();
        let sessions = ledger.query_sessions(AgentFilter::All, 0, None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].turns.is_empty());
        assert_eq!(sessions[0].tool_uses.len(), 2, "tool=Bash + shell=ls");
        fs::remove_dir_all(&root).ok();
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
                SessionJob::Grok { dir } => turns::parse_grok_session(dir),
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
    fn claude_fork_attribution_deterministic_across_sync_orders() {
        // fork 复制历史：两个会话文件含同一条 message_id。归属不得随同步顺序漂移，
        // 共享 turn 聚合只计一次（跨文件去重回到聚合层，与旧内存路径同规则）
        let table = pricing();
        let mut snapshots = Vec::new();
        for (tag, order) in [("ab", [0usize, 1]), ("ba", [1, 0])] {
            let root = temp_root(&format!("fork-{tag}"));
            let a = root.join("sess-a.jsonl");
            let b = root.join("sess-b.jsonl");
            fs::write(&a, format!("{}\n", claude_line("m1", "2026-08-01T10:00:00Z", 50))).unwrap();
            fs::write(
                &b,
                format!("{}\n{}\n", claude_line("m1", "2026-08-01T10:00:00Z", 50), claude_line("m2", "2026-08-01T11:00:00Z", 7)),
            )
            .unwrap();
            let jobs = [
                SessionJob::Claude { main: a, subagents: vec![] },
                SessionJob::Claude { main: b, subagents: vec![] },
            ];
            let names = HashMap::new();
            let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
            for &i in &order {
                ledger.sync_job(&jobs[i], &names).unwrap();
            }
            // 每会话各存自己的份：m1 两行 + m2 一行
            assert_eq!(turn_count(&ledger), 3, "fork 复制的历史按会话各存一份");
            let sessions = ledger.query_sessions(AgentFilter::All, 0, None).unwrap();
            let mut agg = Aggregator::new(0, None, -480, Some("Asia/Shanghai"), false);
            for s in &sessions {
                agg.add_session(s, &table);
            }
            snapshots.push(serde_json::to_value(agg.snapshot()).unwrap());
            fs::remove_dir_all(&root).ok();
        }
        assert_eq!(snapshots[0], snapshots[1], "归属不得随同步顺序漂移");
        assert_eq!(snapshots[0]["totalCalls"], serde_json::json!(2), "共享 m1 只计一次");
        assert_eq!(snapshots[0]["sessionCount"], serde_json::json!(2));
    }

    #[test]
    fn claude_shrink_converges() {
        let root = temp_root("claude-shrink");
        let main = root.join("sess-a.jsonl");
        let sub_dir = root.join("subagents");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(&main, format!("{}\n", claude_line("m1", "2026-08-01T10:00:00Z", 50))).unwrap();
        let sub = sub_dir.join("agent-x.jsonl");
        fs::write(&sub, format!("{}\n", claude_line("m9", "2026-08-01T12:00:00Z", 33))).unwrap();
        let names = HashMap::new();
        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        ledger
            .sync_job(&SessionJob::Claude { main: main.clone(), subagents: vec![sub.clone()] }, &names)
            .unwrap();
        assert_eq!(turn_count(&ledger), 2);

        // 子代理转录被删除 → 指纹变化触发重扫 → 残留的 claude:m9 行必须收敛
        fs::remove_file(&sub).unwrap();
        ledger
            .sync_job(&SessionJob::Claude { main, subagents: vec![] }, &names)
            .unwrap();
        assert_eq!(turn_count(&ledger), 1, "Claude 收缩残留必须收敛");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn legacy_schema_is_rebuilt_on_version_mismatch() {
        let root = temp_root("migrate");
        let db = root.join("usage.db");
        {
            // 手工构造 v1 旧库（user_version=0，turns 以 request_id 单列主键、无 message_id 列）
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE turns (request_id TEXT PRIMARY KEY, session_id TEXT NOT NULL);
                 CREATE TABLE sessions (session_id TEXT PRIMARY KEY);
                 CREATE TABLE sync_state (file_path TEXT PRIMARY KEY, mtime_ms INTEGER NOT NULL, size INTEGER NOT NULL);
                 INSERT INTO sync_state VALUES('x', 1, 1);",
            )
            .unwrap();
        }
        let ledger = Ledger::open(&db).expect("版本不匹配必须重建而非报错");
        assert!(ledger.sync_state_empty().unwrap(), "重建后空 sync_state 触发 backfill");
        ledger
            .conn
            .execute("INSERT INTO turns(session_id, request_id, message_id) VALUES('s','r','m')", [])
            .expect("新 schema 必须含 message_id 列与复合主键");
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
    fn concurrent_opens_during_version_rebuild_are_safe() {
        // 首启 v3 迁移的真实时序:查询命令与后台同步几乎同时 open 一个旧版库。
        // 版本重建必须原子(IMMEDIATE 事务):否则 B 读到旧 version 后会把 A
        // 刚建好、甚至已开始回填的新表再 DROP 一遍,同步轮次静默丢失
        let root = temp_root("racemig");
        let db = root.join("usage.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE turns (request_id TEXT PRIMARY KEY, session_id TEXT NOT NULL);
                 CREATE TABLE sync_state (file_path TEXT PRIMARY KEY, mtime_ms INTEGER NOT NULL, size INTEGER NOT NULL);",
            )
            .unwrap();
        }
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let p = db.clone();
                std::thread::spawn(move || {
                    let ledger = Ledger::open(&p).expect("并发迁移打开必须成功");
                    // 打开即写:模拟同步线程立刻开始回填
                    ledger
                        .conn
                        .execute(
                            "INSERT OR IGNORE INTO sync_state VALUES('probe', 1, 1)",
                            [],
                        )
                        .expect("迁移后的表不得被并发重建再次删除");
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let ledger = Ledger::open(&db).unwrap();
        let v: i64 = ledger.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let probe: i64 = ledger
            .conn
            .query_row("SELECT COUNT(*) FROM sync_state WHERE file_path='probe'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(probe, 1, "任一线程迁移后写入的数据必须存活");
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

    #[test]
    fn sync_trigger_during_run_coalesces_into_rerun() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let lock = Mutex::new(());
        let pending = AtomicBool::new(false);
        let rounds = AtomicUsize::new(0);
        let ran = run_coalesced(&lock, &pending, false, || {
            if rounds.fetch_add(1, Ordering::SeqCst) == 0 {
                // 首轮进行中又来一次触发：拿不到锁不得直接执行，
                // 应置 pending 交由现役持锁者补跑
                let reentrant =
                    run_coalesced(&lock, &pending, false, || panic!("并发触发不得直接执行"));
                assert!(!reentrant, "锁被占用时触发方必须立即返回");
            }
        });
        assert!(ran);
        assert_eq!(rounds.load(Ordering::SeqCst), 2, "运行期间的触发必须补跑一轮，不得丢弃");
    }

    /// 刷新按钮的 blocking 语义：现役同步在跑时不得像后台触发那样直接放弃，
    /// 必须排队等它收尾——否则前端「先同步再查」在并发下会退化回查到同步前的
    /// 旧账本，金额照旧跳动。
    #[test]
    fn blocking_trigger_waits_for_inflight_round() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let lock = Arc::new(Mutex::new(()));
        let pending = Arc::new(AtomicBool::new(false));
        let rounds = Arc::new(AtomicUsize::new(0));
        let holder_started = Arc::new(AtomicBool::new(false));

        // 现役后台同步：持锁跑一轮 300ms
        let (l, p, r, s) = (lock.clone(), pending.clone(), rounds.clone(), holder_started.clone());
        let holder = std::thread::spawn(move || {
            run_coalesced(&l, &p, false, || {
                s.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(300));
                r.fetch_add(1, Ordering::SeqCst);
            });
        });
        while !holder_started.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }

        // 阻塞触发：必须等现役轮收尾才返回
        let ran = run_coalesced(&lock, &pending, true, || {
            rounds.fetch_add(1, Ordering::SeqCst);
        });
        assert!(ran, "blocking 触发必须取得锁");
        assert!(
            rounds.load(Ordering::SeqCst) >= 2,
            "返回时必须已有一轮起始晚于本次触发的同步跑完（现役轮 + 消费 pending 的补跑）"
        );
        holder.join().unwrap();
    }

    /// 回归测试：round panic 会让 guard 在展开中 drop、std Mutex 中毒。
    /// 中毒锁必须被恢复继续用——否则一次 panic 后所有同步触发都走
    /// try_lock Err 分支静默返回，账本在应用整个生命周期内停更。
    #[test]
    fn coalesced_sync_recovers_after_round_panic() {
        use std::sync::atomic::AtomicBool;
        let lock = Mutex::new(());
        let pending = AtomicBool::new(false);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_coalesced(&lock, &pending, false, || panic!("模拟 sync panic"));
        }));
        let mut ran_round = false;
        let ran = run_coalesced(&lock, &pending, false, || ran_round = true);
        assert!(ran, "panic 中毒后的锁必须可恢复，触发不得静默失效");
        assert!(ran_round, "恢复后本轮 round 必须实际执行");
    }

    #[test]
    fn non_corruption_open_error_does_not_delete_ledger() {
        let root = temp_root("cantopen");
        let db = root.join("usage.db");
        {
            let ledger = Ledger::open(&db).unwrap();
            ledger
                .conn
                .execute("INSERT INTO sync_state VALUES('marker', 1, 1)", [])
                .unwrap();
        }
        // -wal 路径被目录占位 → 打开报 CantOpen（环境性错误，非损坏）。
        // 不得把健康主库当损坏删除
        let wal_dir = root.join("usage.db-wal");
        fs::create_dir_all(&wal_dir).unwrap();
        assert!(Ledger::open(&db).is_err(), "非损坏错误必须上抛，不得静默重建");
        fs::remove_dir_all(&wal_dir).unwrap();
        let ledger = Ledger::open(&db).expect("障碍移除后原账本应完好");
        let marker: i64 = ledger
            .conn
            .query_row("SELECT COUNT(*) FROM sync_state WHERE file_path='marker'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(marker, 1, "非损坏错误不得删除账本数据");
        fs::remove_dir_all(&root).ok();
    }
}
