//! 使用统计账本（rusqlite，存 `{app_data_dir}/usage.db`）：采集与展示分离的中间层。
//!
//! 原始 JSONL 只在同步时读一次 → 落账本；展示层 `usage_ledger_query` 永远查账本，
//! 毫秒级返回，任何参数切换都是纯查询。同步按文件粒度增量：`sync_state` 记
//! (path, mtime, size) 指纹，未变跳过（一次 stat 的成本），变了整会话先删后插
//! ——重写/compact/回卷/收缩都严格收敛为当前文件的镜像。唯一例外是 agy:它的
//! transcript 压缩时整份重写、被压掉的调用从文件里消失,账本对它只增不删
//! (按会话内身份 UPSERT,见 `sync_job`)。
//! 设计合同：docs/plans/2026-08-02-usage-stats-ledger-redesign.md。
//!
//! 去 Tauri 化后的两处外部接口（逻辑本身未动）：
//! - 数据库路径不再由 `AppHandle::path().app_data_dir()` 推导，改由调用方传
//!   `app_data_dir: &Path`（`ledger_db_path` 负责建目录并拼 `usage.db`）；
//! - 进度/完成不再走 `app.emit`，改回调 `SyncSink`（事件形状见 `SyncEvent`）。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};

use super::aggregate::{Aggregator, UsageStatsPayload};
use super::pricing::{ModelPrice, PricingTable};
use super::turns::{self, ParsedSession, Turn, UsageTotals};
use super::{
    AgentFilter, ProviderResolver, SessionJob, collect_agy_jobs, collect_claude_jobs,
    collect_codebuddy_jobs, collect_codex_jobs, collect_grok_jobs,
};

/// 账本 schema 版本：结构变更时 +1。open 时版本不匹配即删表重建（账本可从
/// JSONL 再生，空 sync_state 自动触发 backfill，无需逐版本迁移脚本）。
/// v3：新增 tool_events 表（工具/Shell/MCP 排行，设计 §2.2）。
/// v4：turns / tool_events 新增 `eff_ts_ms` 并建索引，时间窗过滤改走索引（见 SCHEMA
/// 注释）。v3 → v4 是就地迁移（加列 + 一次回填，见 `migrate_v3_to_v4`）。
/// v5：turns 新增 `duration_ms`（日志时间戳估算耗时，供「请求日志」速度列使用）。
/// v4 → v5 走就地加列 + 清空 `sync_state`（见 `migrate_v4_to_v5`）：不 DROP `turns`，
/// 从而保住 agy 已被上下文压缩重写的旧调用记录，同时后台自动重扫为存量文件补填 `duration_ms`。
const SCHEMA_VERSION: i64 = 5;

/// 账本 schema（设计 §1）。成本不落库：定价会更新，查询时按前端传入的定价表现算。
/// turns 主键为 (session_id, request_id)：每会话各存自己的份，fork/subagent 复制的
/// 历史允许跨会话重复落库，跨文件 message_id 去重在聚合层做（与旧内存路径同规则，
/// 归属确定、不随同步顺序漂移）。
///
/// `eff_ts_ms` 是窗口判定用的「有效时刻」= COALESCE(ts_ms, 所属会话 mtime_ms)，写入时
/// 算好。以前查询现算 `COALESCE(t.ts_ms, s.mtime_ms)`，表达式跨两张表，`ts_ms` 上的
/// 索引用不上，每次切时间范围都是整表 JOIN 再排序。写入时算是一致的：会话行与它的
/// 全部 turns / 工具事件只在 `sync_job` 里、同一事务内整删重插，会话 mtime 一变，
/// 这两张表里它的行必然跟着重写。
///
/// 索引是 `(session_id, eff_ts_ms)` 而不是单列 `eff_ts_ms`：查询要按 session_id,
/// rowid 出行（组装 ParsedSession、fork 归属都靠这个顺序）。单列索引下窗口一宽就是
/// 逐行回表 + 整体排序，实测 10 万行「全部」比改前还慢一倍；复合索引配合外层按会话
/// 顺序走 sessions（见 `TURNS_IN_WINDOW_SQL`），内层每个会话只 seek 窗口内那一段，
/// 窄窗口快一个数量级，宽窗口也不比改前慢。
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
  eff_ts_ms      INTEGER,
  duration_ms    INTEGER,
  PRIMARY KEY (session_id, request_id)
);
CREATE INDEX IF NOT EXISTS idx_turns_sid_eff ON turns(session_id, eff_ts_ms);
CREATE TABLE IF NOT EXISTS tool_events (
  session_id TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  kind       TEXT NOT NULL,
  name       TEXT NOT NULL,
  ts_ms      INTEGER,
  dedup_key  TEXT,
  eff_ts_ms  INTEGER,
  PRIMARY KEY (session_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_tool_events_sid_eff ON tool_events(session_id, eff_ts_ms);
CREATE TABLE IF NOT EXISTS sync_state (
  file_path TEXT PRIMARY KEY,
  mtime_ms  INTEGER NOT NULL,
  size      INTEGER NOT NULL
);
";

/// 窗口内的计费 turn（?1 = since，?2 = until，闭区间；?3 = agent 或 NULL）。
///
/// 计划（`query_plans_use_eff_ts_indexes` 钉住）：外层按 session_id 顺序扫 sessions，
/// agent 过滤在外层整会话剪掉；内层走 `idx_turns_sid_eff` 按 (session_id=?, eff_ts_ms
/// 区间) seek，只读窗口内的行；排序只剩会话内按 rowid 的小排序。`CROSS JOIN` 是为了
/// 钉死连接顺序（SQLite 里它的左表恒为外层）：没有 ANALYZE 统计，规划器自己会挑
/// 「外层扫 turns」。`ORDER BY s.session_id` 与 `t.session_id` 等价（JOIN 条件相等），
/// 写成外层列才能让外层的扫描顺序直接满足它。
const TURNS_IN_WINDOW_SQL: &str = "
SELECT t.session_id, t.ts_ms, t.model,
       t.input, t.output, t.reasoning, t.cache_read, t.cache_write, t.cache_write_1h,
       s.agent, s.cwd, s.title, s.provider, s.mtime_ms, t.message_id, t.duration_ms
FROM sessions s CROSS JOIN turns t ON t.session_id = s.session_id
WHERE t.eff_ts_ms >= ?1 AND t.eff_ts_ms <= ?2
  AND (?3 IS NULL OR s.agent = ?3)
ORDER BY s.session_id, t.rowid";

/// 窗口内的工具事件，参数与计划同 [`TURNS_IN_WINDOW_SQL`]。
const TOOL_EVENTS_IN_WINDOW_SQL: &str = "
SELECT te.session_id, te.kind, te.name, te.ts_ms, te.dedup_key,
       s.agent, s.cwd, s.title, s.provider, s.mtime_ms
FROM sessions s CROSS JOIN tool_events te ON te.session_id = s.session_id
WHERE te.eff_ts_ms >= ?1 AND te.eff_ts_ms <= ?2
  AND (?3 IS NULL OR s.agent = ?3)
ORDER BY s.session_id, te.seq";

/// 全局同步互斥：同一时刻只有一个同步在跑。运行期间的新触发经 SYNC_PENDING
/// 合并进现役轮收尾补跑（见 run_coalesced），不会被丢弃。
/// 同步每轮自己开一条写连接；查询走 QUERY_CONN 复用的读连接（WAL 下读写连接互不
/// 阻塞，查询永远秒回不等同步）。
static SYNC_LOCK: Mutex<()> = Mutex::new(());
static SYNC_PENDING: AtomicBool = AtomicBool::new(false);

/// 查询连接复用槽。
///
/// 查询跑在 GPUI 的后台线程池上：落在哪条线程不固定，面板切时间范围时还可能两次查询
/// 同时在跑；rusqlite 的 `Connection` 是 Send 不是 Sync。所以不用 thread_local（每条
/// 池线程各攒一条，别的线程也关不掉它），而是一个「取走 / 还回」的单槽：
/// - 取：锁内 `take()` 出来立即放锁，查询全程不持锁；槽空（首次，或另一次查询正拿着）
///   就现开一条——并发查询不排队，最坏退化成改前的每次开库；
/// - 还：查询成功、且期间账本没被删库重建过（`LEDGER_EPOCH` 未变）才放回，槽里已有
///   别的就丢掉这条。
///
/// 锁只护着一次 `Option` 的换手，不存在锁竞争。
static QUERY_CONN: Mutex<Option<CachedLedger>> = Mutex::new(None);

/// 账本被删库重建的代次。重建前 +1，取出 / 还回时代次对不上的连接一律丢弃：Unix 上
/// 旧连接会一直读已 unlink 的旧文件；Windows 上它握着的句柄会让删文件失败。
static LEDGER_EPOCH: AtomicU64 = AtomicU64::new(0);

struct CachedLedger {
    db_path: PathBuf,
    epoch: u64,
    ledger: Ledger,
}

fn query_slot() -> std::sync::MutexGuard<'static, Option<CachedLedger>> {
    // 槽内只有一次换手，中毒不代表状态损坏
    QUERY_CONN.lock().unwrap_or_else(|p| p.into_inner())
}

/// 取一条查询连接：槽里有同一账本、同一代次的就复用，否则现开。
fn checkout_query_ledger(db_path: &Path) -> Result<CachedLedger, String> {
    // 代次要在开库**之前**读：开库期间若被别处重建，这条新连接还回时会被丢弃，
    // 而不是把可能指向旧文件的连接留在槽里
    let epoch = LEDGER_EPOCH.load(Ordering::SeqCst);
    let cached = query_slot().take();
    if let Some(c) = cached.filter(|c| c.db_path == db_path && c.epoch == epoch) {
        return Ok(c);
    }
    Ok(CachedLedger {
        db_path: db_path.to_path_buf(),
        epoch,
        ledger: Ledger::open(db_path)?,
    })
}

/// 查询成功后还回连接（代次对不上、或槽已被别的连接占了，就直接关掉这条）。
fn checkin_query_ledger(conn: CachedLedger) {
    if conn.epoch != LEDGER_EPOCH.load(Ordering::SeqCst) {
        return;
    }
    let mut slot = query_slot();
    if slot.is_none() {
        *slot = Some(conn);
    }
}

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

/// 同步过程中对外播报的事件（原 Tauri 事件 `usage-ledger-progress` /
/// `usage-ledger-synced` 的等价物，字段一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncEvent {
    /// backfill（`sync_state` 为空）期间按 250ms 节流上报
    Progress { processed: usize, total: usize },
    /// 一轮同步收尾。added = 本轮重解析的文件数，0 表示无变化、调用方可跳过重查
    Synced { added: usize },
}

/// 事件汇：原实现是 `app.emit`，现在由调用方决定怎么送到 UI。
/// `Send + Sync` 是因为同步可能跑在后台线程（见 `spawn_usage_ledger_sync`）。
pub type SyncSink = dyn Fn(SyncEvent) + Send + Sync;

pub(crate) struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// 打开账本；仅**真损坏**（NotADatabase/DatabaseCorrupt）才删除重建 + 由空
    /// sync_state 触发 backfill（数据源头是 JSONL，账本可再生）。其余错误
    /// （锁竞争/权限/磁盘满等环境性失败）原样上抛——误判成损坏会把健康账本
    /// 从活跃写入者脚下删掉。
    pub(crate) fn open(db_path: &Path) -> Result<Self, String> {
        match Self::open_raw(db_path) {
            Ok(conn) => Ok(Self { conn }),
            Err(first_err) if is_corruption(&first_err) => {
                // 先作废查询连接缓存：闲在槽里的那条握着文件句柄（Windows 上会让下面的
                // 删除失败），正被别的查询拿着的那条还回时按代次对不上丢弃
                LEDGER_EPOCH.fetch_add(1, Ordering::SeqCst);
                drop(query_slot().take());
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
        //
        // SQLite 对 journal-mode 转换不调用 busy handler（要拿 EXCLUSIVE 锁却绕过
        // busy_timeout）：首次启动（或撞版重建）时查询与后台同步同刻打开一个还是
        // delete 模式的库，只有一个连接能完成转换，输者 0ms 即得 BUSY 而非等满 5s。
        // 按 busy_timeout 同等预算自行重试，只认 BUSY；其余错误照常上抛，
        // 不碰外层 is_corruption 的删库重建路径。
        let mut waited = Duration::ZERO;
        loop {
            match conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0)) {
                Ok(_mode) => break,
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy
                        && waited < Duration::from_millis(5000) =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                    waited += Duration::from_millis(20);
                }
                Err(e) => return Err(e),
            }
        }
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        // 版本已是当前版本（绝大多数打开）：直接用，不开写事务、不跑 DDL。以前每次
        // 打开都 BEGIN IMMEDIATE 再跑一遍 CREATE IF NOT EXISTS——同步正在写库时，
        // 连只读的查询也得排队等写锁（最长 busy_timeout 5s）
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            Self::migrate(&mut conn)?;
        }
        Ok(conn)
    }

    /// 版本不匹配（新库 / 旧库 / 别的版本写过的库）时持写锁迁移：
    /// - v3：先就地跑 `migrate_v3_to_v4`，再跑 `migrate_v4_to_v5`；
    /// - v4：就地加 `duration_ms` 列并清 `sync_state` 触发增量补算（不 DROP `turns`，
    ///   从而保住 agy 已被上下文压缩重写的历史调用记录）；
    /// - 万一就地迁移失败（表结构不是预期形态）退回删表重建；
    /// - 其余：删表重建，空 sync_state 触发 backfill。
    ///
    /// IMMEDIATE 事务先取写锁再复读版本：并发打开（查询命令 vs 后台同步）
    /// 只有一个连接执行迁移，后到者拿到锁后读到新版本直接跳过——否则会把
    /// 对方刚建好、甚至已开始回填的新表再 DROP 一遍
    fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
        let mut tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        let migrated_in_place = matches!(version, 3 | 4)
            && {
                // 放在 savepoint 里：失败只回滚这一段，外层事务照常走删表重建
                let sp = tx.savepoint()?;
                let step = (|| -> rusqlite::Result<()> {
                    if version == 3 {
                        migrate_v3_to_v4(&sp)?;
                    }
                    migrate_v4_to_v5(&sp)?;
                    Ok(())
                })();
                match step {
                    Ok(()) => {
                        sp.commit()?;
                        true
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[usage_stats] 账本 v{version}→v{SCHEMA_VERSION} 就地迁移失败，改为重建: {e}"
                        );
                        false
                    }
                }
            };
        if !migrated_in_place {
            tx.execute_batch(
                "DROP TABLE IF EXISTS turns; DROP TABLE IF EXISTS sessions;
                 DROP TABLE IF EXISTS tool_events; DROP TABLE IF EXISTS sync_state;",
            )?;
        }
        tx.execute_batch(SCHEMA)?;
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        tx.commit()
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
            SessionJob::Agy { conv, transcript } => turns::parse_agy_session(conv, transcript),
            SessionJob::CodeBuddy { main, subagents } => {
                turns::parse_codebuddy_session(main, subagents)
            }
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
        //
        // agy 例外——只增不删:它的 transcript 在上下文压缩时被整份重写,被压掉的
        // 步骤从文件里消失了,但那些调用是真花过钱的。账本按「会话内身份」
        // (`agy:{step_index}@{created_at}`,见 `turns::agy_message_id`)UPSERT:
        // 还在文件里的步骤原地覆盖、不会记两次;已从文件消失的旧行原样留着
        if s.agent != "agy" {
            tx.execute(
                "DELETE FROM turns WHERE session_id = ?1",
                params![s.session_id],
            )?;
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO turns(session_id, request_id, message_id, ts_ms, model,
                                   input, output, reasoning, cache_read, cache_write, cache_write_1h,
                                   eff_ts_ms, duration_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(session_id, request_id) DO UPDATE SET
                   message_id = excluded.message_id, ts_ms = excluded.ts_ms,
                   model = excluded.model, input = excluded.input, output = excluded.output,
                   reasoning = excluded.reasoning, cache_read = excluded.cache_read,
                   cache_write = excluded.cache_write, cache_write_1h = excluded.cache_write_1h,
                   eff_ts_ms = excluded.eff_ts_ms, duration_ms = excluded.duration_ms",
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
                    // agy 的 message_id 是跨会话去重键(带模型与用量),主键只取其中
                    // 的会话内身份——同一步被原地改写时覆盖,而不是多出一行
                    (Some(id), "agy") => turns::agy_request_id(id).to_string(),
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
                    // 有效时刻：缺时间戳回退会话 mtime（会话行在本事务开头刚写入）
                    t.timestamp_ms.unwrap_or(s.mtime_ms),
                    t.duration_ms.map(|d| d as i64),
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
                "INSERT INTO tool_events(session_id, seq, kind, name, ts_ms, dedup_key, eff_ts_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (i, u) in s.tool_uses.iter().enumerate() {
                stmt.execute(params![
                    s.session_id,
                    i as i64,
                    u.kind,
                    u.name,
                    u.timestamp_ms,
                    u.dedup_key,
                    u.timestamp_ms.unwrap_or(s.mtime_ms),
                ])?;
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// 按窗口/agent 查 turns+sessions，组回 ParsedSession（喂现有 Aggregator，
    /// UsageStatsPayload 形状不变）。窗口判定与聚合层同口径：turn 缺时间戳回退
    /// session.mtime_ms——写入时已算进 `eff_ts_ms`，窗口过滤走 (session_id, eff_ts_ms)
    /// 索引，只读窗口内的行（计划见 [`TURNS_IN_WINDOW_SQL`]）。
    /// 项目 scope 过滤在命令层（需要 normalize，SQL 不好做）。
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
            AgentFilter::Agy => Some("agy"),
            AgentFilter::CodeBuddy => Some("codebuddy"),
        };
        let mut stmt = self.conn.prepare_cached(TURNS_IN_WINDOW_SQL)?;
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
                duration_ms: row
                    .get::<_, Option<i64>>(15)?
                    .and_then(|d| u64::try_from(d).ok()),
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
        let mut stmt = self.conn.prepare_cached(TOOL_EVENTS_IN_WINDOW_SQL)?;
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

/// v3 → v4 就地迁移：turns / tool_events 各加一列 `eff_ts_ms` 并一次回填存量行（索引
/// 随后由 SCHEMA 的 CREATE INDEX IF NOT EXISTS 建出）。回填口径与写入路径同一个：
/// COALESCE(ts_ms, 所属会话 mtime_ms)；没有会话行的孤儿行回填成什么都无所谓——查询
/// 仍 JOIN sessions，与旧查询一样把它们挡在外面。旧的 ts_ms 索引只服务于旧查询，一并删掉。
fn migrate_v3_to_v4(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "ALTER TABLE turns ADD COLUMN eff_ts_ms INTEGER;
         UPDATE turns SET eff_ts_ms = COALESCE(ts_ms,
           (SELECT s.mtime_ms FROM sessions s WHERE s.session_id = turns.session_id));
         ALTER TABLE tool_events ADD COLUMN eff_ts_ms INTEGER;
         UPDATE tool_events SET eff_ts_ms = COALESCE(ts_ms,
           (SELECT s.mtime_ms FROM sessions s WHERE s.session_id = tool_events.session_id));
         DROP INDEX IF EXISTS idx_turns_ts;
         DROP INDEX IF EXISTS idx_tool_events_ts;",
    )
}

/// v4 → v5 就地迁移：turns 加 `duration_ms` 列（存日志时间戳估算耗时），并清空
/// `sync_state` 触发后台重扫，为仍在磁盘上的会话补齐估算耗时。
///
/// 刻意**不 DROP `turns`**：agy 的 transcript 上下文压缩时会整份重写、旧步骤从磁盘
/// 消失，就地加列能把已入账的旧 agy 行原样保住，重扫时对仍存在的行按主键 UPSERT
/// 补填 `duration_ms`，已压缩消失的旧行保持 `duration_ms = NULL`。
fn migrate_v4_to_v5(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "ALTER TABLE turns ADD COLUMN duration_ms INTEGER;
         DELETE FROM sync_state;",
    )
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
        // 压缩重写可能让文件变短:size 变小同样是指纹变化,照常整份重读
        SessionJob::Agy { transcript, .. } => (
            transcript.clone(),
            turns::mtime_ms(transcript),
            size_of(transcript),
        ),
        SessionJob::CodeBuddy { main, subagents } => {
            let mtime = job.mtime_ms();
            let size = subagents.iter().map(|p| size_of(p)).sum::<i64>() + size_of(main);
            (main.clone(), mtime, size)
        }
    }
}

/// 账本文件路径：`{app_data_dir}/usage.db`（顺手把目录建出来）。
/// 原来由 `AppHandle::path().app_data_dir()` 推导，现在由调用方给。
pub fn ledger_db_path(app_data_dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(app_data_dir).map_err(|e| format!("创建应用数据目录失败: {e}"))?;
    Ok(app_data_dir.join("usage.db"))
}

/// 查询账本 → 聚合快照。`pricing` 由调用方拉 models.dev 后传入（$/token）；
/// 窗口/时区/分桶参数语义与旧 start_usage_stats 一致；项目 scope 沿用
/// normalize + 子路径规则按 cwd 终判。
///
/// 原本是 `async` 命令（Tauri v2 的同步命令跑在主线程，而打开连接可能等
/// busy_timeout，落主线程会把窗口冻住）。单进程下没有 IPC 命令这一层，函数
/// 本身是毫秒级的纯查询，改回同步；**调用方仍不应在 UI 线程上直接调它**——
/// 连接平时复用（`QUERY_CONN`），但首次开库、撞版迁移仍可能等 busy_timeout 最长 5s。
pub fn usage_ledger_query(
    app_data_dir: &Path,
    agents: AgentFilter,
    since_ms: i64,
    until_ms: Option<i64>,
    project_path: Option<String>,
    tz_offset_minutes: i32,
    tz_name: Option<String>,
    hourly: bool,
    pricing: HashMap<String, ModelPrice>,
) -> Result<UsageStatsPayload, String> {
    let cached = checkout_query_ledger(&ledger_db_path(app_data_dir)?)?;
    let sessions = match cached.ledger.query_sessions(agents, since_ms, until_ms) {
        Ok(sessions) => {
            checkin_query_ledger(cached);
            sessions
        }
        // 出错的连接不还回：下次现开，损坏之类由 Ledger::open 的重建路径接手
        Err(e) => return Err(format!("账本查询失败: {e}")),
    };

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

/// 跑一次增量同步（本函数**就地阻塞**，重文件 I/O + sqlite 写，别放 UI 线程）。
/// - `blocking = false`（定时刷新、打开面板的后台触发）：现役同步在跑就直接放弃，
///   由其收尾时消费 pending 补跑，不排队。
/// - `blocking = true`（刷新按钮）：排队等现役轮次收尾再返回，调用方据此做
///   「先同步再查」。旧实现一律 fire-and-forget，点刷新先查到的必然是同步前的
///   旧账本，真值要等 synced 事件才补上——表现为每点一次刷新金额跳一次。
///
/// 收尾送 `SyncEvent::Synced { added }`（added = 重解析的文件数，0 表示无变化，
/// 调用方可跳过重查）；backfill（sync_state 为空）期间另按节流送
/// `SyncEvent::Progress { processed, total }`。
///
/// 原命令的两条并发路径（`tauri::async_runtime::spawn_blocking` / `thread::spawn`）
/// 都只是「把这个函数挪出主线程」，不绑定运行时地保留成 `spawn_usage_ledger_sync`
/// 与本函数两支，调用方按需选。
pub fn usage_ledger_sync(db_path: &Path, blocking: bool, sink: &SyncSink) {
    // catch_unwind 兜底：sync panic 只损失本轮增量，账本与查询不受影响
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_sync(db_path, blocking, sink);
    }));
    if outcome.is_err() {
        tracing::error!("[usage_stats] ledger sync panicked");
    }
}

/// 后台触发一次同步（原命令 `wait` 缺省时的 `std::thread::spawn` 分支）：
/// 立即返回，不阻塞调用方。
pub fn spawn_usage_ledger_sync(
    db_path: PathBuf,
    sink: Arc<SyncSink>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || usage_ledger_sync(&db_path, false, sink.as_ref()))
}

fn run_sync(db_path: &Path, blocking: bool, sink: &SyncSink) {
    run_coalesced(&SYNC_LOCK, &SYNC_PENDING, blocking, || {
        sync_round(db_path, sink)
    });
}

fn sync_round(db_path: &Path, sink: &SyncSink) {
    let mut ledger = match Ledger::open(db_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("[usage_stats] 账本打开失败: {e}");
            return;
        }
    };
    let Some(home) = dirs::home_dir() else { return };

    let mut jobs: Vec<SessionJob> = Vec::new();
        collect_claude_jobs(&home, &mut jobs);
        collect_codex_jobs(&home, &mut jobs);
        collect_codebuddy_jobs(&home, &mut jobs);
    if let Some(grok_home) = mt_ai::hook_registry::grok_home() {
        collect_grok_jobs(&grok_home.join("sessions"), &mut jobs);
    }
    if let Some(agy_dir) = mt_ai::sessions::agy_app_data_dir() {
        collect_agy_jobs(&agy_dir, &mut jobs);
    }
    let thread_names = mt_ai::sessions::load_codex_thread_names(&home.join(".codex"));

    let backfill = ledger.sync_state_empty().unwrap_or(false);
    let total = jobs.len();
    let mut added = 0usize;
    let mut last_emit = Instant::now();
    for (i, job) in jobs.iter().enumerate() {
        match ledger.sync_job(job, &thread_names) {
            Ok(true) => added += 1,
            Ok(false) => {}
            // 单文件失败不拖垮全量（下轮指纹仍不匹配，会重试）
            Err(e) => tracing::warn!("[usage_stats] 同步文件失败: {e}"),
        }
        if backfill && last_emit.elapsed() >= Duration::from_millis(250) {
            sink(SyncEvent::Progress { processed: i + 1, total });
            last_emit = Instant::now();
        }
    }
    sink(SyncEvent::Synced { added });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::ModelPrice;

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
                SessionJob::Claude { main, subagents } => {
                    turns::parse_claude_session(main, subagents)
                }
                SessionJob::Codex { path } => turns::parse_codex_session(path, &names),
                SessionJob::Grok { dir } => turns::parse_grok_session(dir),
                SessionJob::Agy { conv, transcript } => turns::parse_agy_session(conv, transcript),
                SessionJob::CodeBuddy { main, subagents } => {
                    turns::parse_codebuddy_session(main, subagents)
                }
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
        // 重建会作废全局的查询连接槽,与复用测试串行
        let _serial = CACHE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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

    // ── 时间窗过滤对照:查询条件改走索引前后,结果必须逐条一致 ──

    /// 毫秒时刻 → RFC3339(带毫秒),造边界数据用。
    fn rfc3339(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
    }

    /// 缺 timestamp 字段的 assistant 行:窗口判定回退会话 mtime。
    fn claude_line_no_ts(id: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","cwd":"/p/alpha","message":{{"id":"{id}","model":"claude-opus-4-8","usage":{{"input_tokens":10,"output_tokens":{output}}}}}}}"#
        )
    }

    fn claude_bash_line_no_ts(toolu: &str, cmd: &str) -> String {
        format!(
            r#"{{"type":"assistant","cwd":"/p/alpha","message":{{"id":"mt-{toolu}","model":"claude-opus-4-8","usage":{{"input_tokens":0,"output_tokens":0}},"content":[{{"type":"tool_use","id":"{toolu}","name":"Bash","input":{{"command":"{cmd}"}}}}]}}}}"#
        )
    }

    /// 写文件后把 mtime 钉到给定毫秒(会话 mtime 即回退时刻)。
    fn write_with_mtime(path: &Path, lines: &[String], mtime_ms: i64) {
        fs::write(path, lines.join("\n") + "\n").unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_millis(mtime_ms as u64))
            .unwrap();
    }

    /// 查询结果的可比较签名:会话顺序 + 每会话 turn / 工具事件的字段与顺序。
    type TurnSig = (Option<i64>, Option<String>, u64, u64, Option<String>);
    type ToolSig = (&'static str, String, Option<i64>, Option<String>);
    type SessionSig = (String, &'static str, i64, Vec<TurnSig>, Vec<ToolSig>);

    fn signature(sessions: &[ParsedSession]) -> Vec<SessionSig> {
        sessions
            .iter()
            .map(|s| {
                (
                    s.session_id.clone(),
                    s.agent,
                    s.mtime_ms,
                    s.turns
                        .iter()
                        .map(|t| {
                            (
                                t.timestamp_ms,
                                t.model.clone(),
                                t.usage.input,
                                t.usage.output,
                                t.message_id.clone(),
                            )
                        })
                        .collect(),
                    s.tool_uses
                        .iter()
                        .map(|u| (u.kind, u.name.clone(), u.timestamp_ms, u.dedup_key.clone()))
                        .collect(),
                )
            })
            .collect()
    }

    /// 参照实现:直接读三张表的原始行,在 Rust 里按「缺时间戳回退会话 mtime」过滤,
    /// 再按 query_sessions 的组装规则拼回(先 turns 按 session_id,rowid;再工具事件
    /// 按 session_id,seq,只有工具事件的会话追加在末尾)。不经过任何 WHERE 子句,
    /// 用来把查询语义钉死——改查询条件前后都必须与它逐条一致。
    fn reference_query(
        ledger: &Ledger,
        agent: Option<&str>,
        since: i64,
        until: Option<i64>,
    ) -> Vec<SessionSig> {
        let until = until.unwrap_or(i64::MAX);
        let conn = &ledger.conn;
        let mut meta: HashMap<String, (String, i64)> = HashMap::new();
        let mut stmt = conn
            .prepare("SELECT session_id, agent, mtime_ms FROM sessions")
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            meta.insert(r.get(0).unwrap(), (r.get(1).unwrap(), r.get(2).unwrap()));
        }
        drop(rows);
        let keep = |sid: &str, ts: Option<i64>| -> Option<(&'static str, i64)> {
            let (ag, mtime) = meta.get(sid)?; // JOIN 语义:孤儿行不出现
            if agent.is_some_and(|a| a != ag) {
                return None;
            }
            let eff = ts.unwrap_or(*mtime);
            (since..=until)
                .contains(&eff)
                .then(|| (crate::agent_from_db(ag), *mtime))
        };

        let mut out: Vec<SessionSig> = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT session_id, ts_ms, model, input, output, message_id FROM turns
                 ORDER BY session_id, rowid",
            )
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let sid: String = r.get(0).unwrap();
            let ts: Option<i64> = r.get(1).unwrap();
            let Some((ag, mtime)) = keep(&sid, ts) else {
                continue;
            };
            if out.last().map(|s| s.0.as_str()) != Some(sid.as_str()) {
                out.push((sid.clone(), ag, mtime, Vec::new(), Vec::new()));
            }
            out.last_mut().unwrap().3.push((
                ts,
                r.get(2).unwrap(),
                r.get::<_, i64>(3).unwrap() as u64,
                r.get::<_, i64>(4).unwrap() as u64,
                r.get(5).unwrap(),
            ));
        }
        drop(rows);
        let mut stmt = conn
            .prepare(
                "SELECT session_id, kind, name, ts_ms, dedup_key FROM tool_events
                 ORDER BY session_id, seq",
            )
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let sid: String = r.get(0).unwrap();
            let ts: Option<i64> = r.get(3).unwrap();
            let Some((ag, mtime)) = keep(&sid, ts) else {
                continue;
            };
            let i = match out.iter().position(|s| s.0 == sid) {
                Some(i) => i,
                None => {
                    out.push((sid.clone(), ag, mtime, Vec::new(), Vec::new()));
                    out.len() - 1
                }
            };
            let kind: String = r.get(1).unwrap();
            let kind = match kind.as_str() {
                "shell" => "shell",
                "mcp" => "mcp",
                _ => "tool",
            };
            out[i]
                .4
                .push((kind, r.get(2).unwrap(), ts, r.get(4).unwrap()));
        }
        out
    }

    /// 混合数据:有时间戳的 / 缺时间戳靠会话 mtime 的 / 恰落在窗口两侧边界上的 /
    /// 边界外 1ms 的 / 只有工具事件的会话 / 另一个 agent 的会话。
    fn mixed_window_fixture(tag: &str) -> (PathBuf, Ledger, i64, i64) {
        let root = temp_root(tag);
        // 窗口 [t0, t1];codex_lines 的两条 token_count 恰好落在 t0 与 t1 上
        let t0 = turns::parse_rfc3339_ms("2026-08-01T10:00:00Z").unwrap();
        let t1 = t0 + 3_600_000;
        let a = root.join("s-a.jsonl");
        write_with_mtime(
            &a,
            &[
                claude_line("a1", &rfc3339(t0), 11),
                claude_line("a2", &rfc3339(t0 - 1), 12),
                claude_line("a3", &rfc3339(t1), 13),
                claude_line("a4", &rfc3339(t1 + 1), 14),
                claude_line_no_ts("a5", 15),
                claude_bash_line("toolu_a1", &rfc3339(t0 - 1), "git status"),
                claude_bash_line_no_ts("toolu_a2", "ls"),
            ],
            t0 + 1_800_000,
        );
        // 全部缺时间戳,会话 mtime 恰在左边界
        let b = root.join("s-b.jsonl");
        write_with_mtime(
            &b,
            &[claude_line_no_ts("b1", 21), claude_line_no_ts("b2", 22)],
            t0,
        );
        // 会话 mtime 在右边界外 1ms,但有一条带时间戳的在窗口内
        let c = root.join("s-c.jsonl");
        write_with_mtime(
            &c,
            &[
                claude_line_no_ts("c1", 31),
                claude_line("c2", &rfc3339(t0 + 600_000), 32),
            ],
            t1 + 1,
        );
        // 只有工具事件(无计费 turn)、缺时间戳,mtime 恰在右边界
        let d = root.join("s-d.jsonl");
        write_with_mtime(&d, &[claude_bash_line_no_ts("toolu_d1", "cargo test")], t1);
        let codex = root.join("rollout-e.jsonl");
        fs::write(&codex, codex_lines("sess-e")).unwrap();

        let names = HashMap::new();
        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        for main in [a, b, c, d] {
            assert!(
                ledger
                    .sync_job(
                        &SessionJob::Claude {
                            main,
                            subagents: vec![]
                        },
                        &names
                    )
                    .unwrap()
            );
        }
        assert!(
            ledger
                .sync_job(&SessionJob::Codex { path: codex }, &names)
                .unwrap()
        );
        (root, ledger, t0, t1)
    }

    fn fixture_windows(t0: i64, t1: i64) -> Vec<(i64, Option<i64>)> {
        vec![
            (0, None),
            (t0, Some(t1)),
            (t0 + 1, Some(t1 - 1)),
            (t0 - 1, Some(t0)),
            (t1, Some(t1)),
            (t1 + 1, None),
            (t0, None),
            (0, Some(t0 - 1)),
        ]
    }

    #[test]
    fn window_filter_matches_reference_semantics() {
        let (root, ledger, t0, t1) = mixed_window_fixture("window");
        let agents = [
            (AgentFilter::All, None),
            (AgentFilter::Claude, Some("claude")),
            (AgentFilter::Codex, Some("codex")),
        ];
        for (since, until) in fixture_windows(t0, t1) {
            for (filter, agent) in agents {
                let got = signature(&ledger.query_sessions(filter, since, until).unwrap());
                let want = reference_query(&ledger, agent, since, until);
                assert_eq!(got, want, "窗口 [{since}, {until:?}] agent={agent:?}");
            }
        }

        // 参照实现不能是空转:闭区间 [t0, t1] 下逐项核对边界取舍
        let got = signature(
            &ledger
                .query_sessions(AgentFilter::All, t0, Some(t1))
                .unwrap(),
        );
        let ids = |sid: &str| -> Vec<String> {
            got.iter()
                .find(|s| s.0 == sid)
                .map(|s| s.3.iter().filter_map(|t| t.4.clone()).collect())
                .unwrap_or_default()
        };
        assert_eq!(
            ids("s-a"),
            ["a1", "a3", "a5"],
            "左右边界含、边界外 1ms 不含、缺时间戳回退 mtime"
        );
        assert_eq!(ids("s-b"), ["b1", "b2"], "会话 mtime 恰在左边界");
        assert_eq!(ids("s-c"), ["c2"], "会话 mtime 在窗外时只剩带时间戳的那条");
        let d = got
            .iter()
            .find(|s| s.0 == "s-d")
            .expect("只有工具事件的会话也要查回");
        assert!(d.3.is_empty() && d.4.len() == 2);
        let e = got
            .iter()
            .find(|s| s.0 == "sess-e")
            .expect("codex 两条 token_count 恰在两侧边界");
        assert_eq!(e.3.len(), 2);
        let a = got.iter().find(|s| s.0 == "s-a").unwrap();
        assert_eq!(
            a.4.iter().map(|u| u.3.clone().unwrap()).collect::<Vec<_>>(),
            ["toolu_a2", "toolu_a2#s"],
            "边界外 1ms 的工具事件不含,缺时间戳的回退 mtime 入窗"
        );
        drop(ledger);
        fs::remove_dir_all(&root).ok();
    }

    /// 改前的两条窗口查询原文(条件现算 COALESCE,用不上索引),对照用。
    const LEGACY_TURNS_SQL: &str = "
        SELECT t.session_id, t.ts_ms, t.model,
               t.input, t.output, t.reasoning, t.cache_read, t.cache_write, t.cache_write_1h,
               s.agent, s.cwd, s.title, s.provider, s.mtime_ms, t.message_id, t.duration_ms
        FROM turns t JOIN sessions s ON s.session_id = t.session_id
        WHERE COALESCE(t.ts_ms, s.mtime_ms) >= ?1
          AND COALESCE(t.ts_ms, s.mtime_ms) <= ?2
          AND (?3 IS NULL OR s.agent = ?3)
        ORDER BY t.session_id, t.rowid";
    /// v3 库尚无 `duration_ms` 列，迁移前查询对照用。
    const LEGACY_V3_TURNS_SQL: &str = "
        SELECT t.session_id, t.ts_ms, t.model,
               t.input, t.output, t.reasoning, t.cache_read, t.cache_write, t.cache_write_1h,
               s.agent, s.cwd, s.title, s.provider, s.mtime_ms, t.message_id
        FROM turns t JOIN sessions s ON s.session_id = t.session_id
        WHERE COALESCE(t.ts_ms, s.mtime_ms) >= ?1
          AND COALESCE(t.ts_ms, s.mtime_ms) <= ?2
          AND (?3 IS NULL OR s.agent = ?3)
        ORDER BY t.session_id, t.rowid";
    const LEGACY_TOOL_EVENTS_SQL: &str = "
        SELECT te.session_id, te.kind, te.name, te.ts_ms, te.dedup_key,
               s.agent, s.cwd, s.title, s.provider, s.mtime_ms
        FROM tool_events te JOIN sessions s ON s.session_id = te.session_id
        WHERE COALESCE(te.ts_ms, s.mtime_ms) >= ?1
          AND COALESCE(te.ts_ms, s.mtime_ms) <= ?2
          AND (?3 IS NULL OR s.agent = ?3)
        ORDER BY te.session_id, te.seq";

    /// v3 账本的原样 schema(改前的 SCHEMA),造存量库用。
    const V3_SCHEMA: &str = "
        CREATE TABLE sessions (
          session_id TEXT PRIMARY KEY, agent TEXT NOT NULL, cwd TEXT, title TEXT,
          provider TEXT, file_path TEXT NOT NULL, mtime_ms INTEGER NOT NULL);
        CREATE TABLE turns (
          session_id TEXT NOT NULL, request_id TEXT NOT NULL, message_id TEXT, ts_ms INTEGER,
          model TEXT, input INTEGER NOT NULL DEFAULT 0, output INTEGER NOT NULL DEFAULT 0,
          reasoning INTEGER NOT NULL DEFAULT 0, cache_read INTEGER NOT NULL DEFAULT 0,
          cache_write INTEGER NOT NULL DEFAULT 0, cache_write_1h INTEGER NOT NULL DEFAULT 0,
          PRIMARY KEY (session_id, request_id));
        CREATE INDEX idx_turns_ts ON turns(ts_ms);
        CREATE TABLE tool_events (
          session_id TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, name TEXT NOT NULL,
          ts_ms INTEGER, dedup_key TEXT, PRIMARY KEY (session_id, seq));
        CREATE INDEX idx_tool_events_ts ON tool_events(ts_ms);
        CREATE TABLE sync_state (
          file_path TEXT PRIMARY KEY, mtime_ms INTEGER NOT NULL, size INTEGER NOT NULL);
        PRAGMA user_version = 3;";

    type RawRows = Vec<Vec<rusqlite::types::Value>>;

    fn raw_rows(
        conn: &Connection,
        sql: &str,
        since: i64,
        until: Option<i64>,
        agent: Option<&str>,
    ) -> RawRows {
        let mut stmt = conn.prepare(sql).unwrap();
        let n = stmt.column_count();
        let mut rows = stmt
            .query(params![since, until.unwrap_or(i64::MAX), agent])
            .unwrap();
        let mut out = Vec::new();
        while let Some(r) = rows.next().unwrap() {
            out.push(
                (0..n)
                    .map(|i| r.get::<_, rusqlite::types::Value>(i).unwrap())
                    .collect(),
            );
        }
        out
    }

    /// 全部窗口 × agent 组合下,两条查询各自的原始行。
    fn all_window_rows(
        conn: &Connection,
        turns_sql: &str,
        tools_sql: &str,
        t0: i64,
        t1: i64,
    ) -> Vec<(RawRows, RawRows)> {
        let mut out = Vec::new();
        for (since, until) in fixture_windows(t0, t1) {
            for agent in [None, Some("claude"), Some("codex")] {
                out.push((
                    raw_rows(conn, turns_sql, since, until, agent),
                    raw_rows(conn, tools_sql, since, until, agent),
                ));
            }
        }
        out
    }

    #[test]
    fn window_filter_matches_legacy_sql_row_by_row() {
        let (root, ledger, t0, t1) = mixed_window_fixture("legacy");
        let legacy = all_window_rows(
            &ledger.conn,
            LEGACY_TURNS_SQL,
            LEGACY_TOOL_EVENTS_SQL,
            t0,
            t1,
        );
        let current = all_window_rows(
            &ledger.conn,
            TURNS_IN_WINDOW_SQL,
            TOOL_EVENTS_IN_WINDOW_SQL,
            t0,
            t1,
        );
        assert!(
            legacy.iter().any(|(t, e)| !t.is_empty() && !e.is_empty()),
            "对照数据不能是空集"
        );
        assert_eq!(
            current, legacy,
            "改走 eff_ts_ms 后,每个窗口的每一行都必须与改前一致"
        );
        drop(ledger);
        fs::remove_dir_all(&root).ok();
    }

    /// 存量 v3 账本就地迁移:加列 + 一次回填,sync_state 保留(不触发全量重解析),
    /// 迁移后的新查询与迁移前旧库上的旧查询逐行一致。
    #[test]
    fn v3_ledger_migrates_in_place_with_backfill() {
        let (src_root, src, t0, t1) = mixed_window_fixture("v3src");
        drop(src);
        let root = temp_root("v3");
        let db = root.join("usage.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(V3_SCHEMA).unwrap();
            conn.execute(
                "ATTACH DATABASE ?1 AS src",
                [src_root.join("usage.db").to_string_lossy()],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO sessions SELECT * FROM src.sessions;
                 INSERT INTO turns SELECT session_id, request_id, message_id, ts_ms, model, input,
                   output, reasoning, cache_read, cache_write, cache_write_1h
                   FROM src.turns ORDER BY rowid;
                 INSERT INTO tool_events SELECT session_id, seq, kind, name, ts_ms, dedup_key
                   FROM src.tool_events ORDER BY rowid;
                 INSERT INTO sync_state SELECT * FROM src.sync_state;
                 DETACH DATABASE src;
                 -- 孤儿行(会话行缺失):旧查询 JOIN 掉,迁移后也不得冒出来
                 INSERT INTO turns(session_id, request_id, ts_ms, input) VALUES
                   ('ghost', 'g1', NULL, 5), ('ghost', 'g2', 0, 5);",
            )
            .unwrap();
        }
        // 改前:旧查询跑在旧库上
        let before = {
            let conn = Connection::open(&db).unwrap();
            all_window_rows(&conn, LEGACY_V3_TURNS_SQL, LEGACY_TOOL_EVENTS_SQL, t0, t1)
        };

        let ledger = Ledger::open(&db).unwrap();
        let version: i64 = ledger
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(
            ledger.sync_state_empty().unwrap(),
            "升到 v5 清空 sync_state 触发补算 duration_ms，但存量 turns 行原样保留"
        );
        for table in ["turns", "tool_events"] {
            let off: i64 = ledger
                .conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} x JOIN sessions s USING(session_id)
                         WHERE x.eff_ts_ms IS NOT COALESCE(x.ts_ms, s.mtime_ms)"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                off, 0,
                "{table}.eff_ts_ms 回填口径必须是 COALESCE(ts_ms, 会话 mtime)"
            );
        }
        let indexes: Vec<String> = ledger
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            indexes,
            ["idx_tool_events_sid_eff", "idx_turns_sid_eff"],
            "旧 ts_ms 索引删掉、新索引建上"
        );

        let after = all_window_rows(
            &ledger.conn,
            TURNS_IN_WINDOW_SQL,
            TOOL_EVENTS_IN_WINDOW_SQL,
            t0,
            t1,
        );
        // v5 多出的第 16 列 duration_ms 对旧行均为 NULL，前 15 列与工具事件须逐行一致
        let after_without_dur: Vec<(RawRows, RawRows)> = after
            .into_iter()
            .map(|(turns, tools)| {
                let trimmed = turns
                    .into_iter()
                    .map(|mut r| {
                        assert_eq!(r.pop(), Some(rusqlite::types::Value::Null));
                        r
                    })
                    .collect();
                (trimmed, tools)
            })
            .collect();
        assert_eq!(
            after_without_dur, before,
            "迁移后的查询结果必须与迁移前逐行一致"
        );
        drop(ledger);
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&src_root).ok();
    }

    /// v4 → v5 就地迁移：不 DROP turns（保住 agy 压缩掉的历史行），只加 duration_ms 列并
    /// 清空 sync_state 触发后台补算。
    #[test]
    fn v4_ledger_migrates_in_place_preserving_existing_turns() {
        let root = temp_root("v4tov5");
        let db = root.join("usage.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                   session_id TEXT PRIMARY KEY, agent TEXT NOT NULL, cwd TEXT, title TEXT,
                   provider TEXT, file_path TEXT NOT NULL, mtime_ms INTEGER NOT NULL);
                 CREATE TABLE turns (
                   session_id TEXT NOT NULL, request_id TEXT NOT NULL, message_id TEXT,
                   ts_ms INTEGER, model TEXT, input INTEGER NOT NULL DEFAULT 0,
                   output INTEGER NOT NULL DEFAULT 0, reasoning INTEGER NOT NULL DEFAULT 0,
                   cache_read INTEGER NOT NULL DEFAULT 0, cache_write INTEGER NOT NULL DEFAULT 0,
                   cache_write_1h INTEGER NOT NULL DEFAULT 0, eff_ts_ms INTEGER,
                   PRIMARY KEY (session_id, request_id));
                 CREATE INDEX idx_turns_sid_eff ON turns(session_id, eff_ts_ms);
                 CREATE TABLE tool_events (
                   session_id TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL,
                   name TEXT NOT NULL, ts_ms INTEGER, dedup_key TEXT, eff_ts_ms INTEGER,
                   PRIMARY KEY (session_id, seq));
                 CREATE INDEX idx_tool_events_sid_eff ON tool_events(session_id, eff_ts_ms);
                 CREATE TABLE sync_state (
                   file_path TEXT PRIMARY KEY, mtime_ms INTEGER NOT NULL, size INTEGER NOT NULL);
                 INSERT INTO sessions VALUES('agy-old', 'agy', 'D:\\p', '旧会话', NULL, 'p', 1000);
                 INSERT INTO turns VALUES('agy-old', 'agy:1@t', 'agy:1@t#m#10/0/20', 1000, 'gemini-4-neon-xhigh', 10, 20, 0, 0, 0, 0, 1000);
                 INSERT INTO sync_state VALUES('p', 1000, 100);
                 PRAGMA user_version = 4;",
            )
            .unwrap();
        }
        let ledger = Ledger::open(&db).expect("v4 账本就地迁移必须成功");
        let version: i64 = ledger
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(
            ledger.sync_state_empty().unwrap(),
            "清 sync_state 触发重扫补算耗时"
        );
        let sessions = ledger.query_sessions(AgentFilter::Agy, 0, None).unwrap();
        assert_eq!(sessions.len(), 1, "已入账的 agy 旧行必须完整保留");
        assert_eq!(sessions[0].turns.len(), 1);
        assert_eq!(sessions[0].turns[0].duration_ms, None);
        drop(ledger);
        fs::remove_dir_all(&root).ok();
    }

    /// 标着 v3 但表结构不对(就地迁移会失败):退回删表重建,不能把账本卡死在打不开。
    #[test]
    fn broken_v3_ledger_falls_back_to_rebuild() {
        let root = temp_root("badv3");
        let db = root.join("usage.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sync_state (file_path TEXT PRIMARY KEY, mtime_ms INTEGER NOT NULL, size INTEGER NOT NULL);
                 INSERT INTO sync_state VALUES('x', 1, 1);
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let ledger = Ledger::open(&db).expect("就地迁移失败必须退回重建");
        assert!(
            ledger.sync_state_empty().unwrap(),
            "重建后空 sync_state 触发 backfill"
        );
        let version: i64 = ledger
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(ledger);
        fs::remove_dir_all(&root).ok();
    }

    /// 版本已匹配时开库不开写事务:同步正拿着写锁时,查询照样秒开秒查,不等 busy_timeout。
    #[test]
    fn open_at_current_version_does_not_take_write_lock() {
        let root = temp_root("nolock");
        let db = root.join("usage.db");
        drop(Ledger::open(&db).unwrap());
        // 另一条连接(模拟正在写库的同步)拿着写锁不放
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO sync_state VALUES('w', 1, 1);")
            .unwrap();
        let started = Instant::now();
        let ledger = Ledger::open(&db).expect("版本匹配时开库不该去抢写锁");
        assert!(
            ledger
                .query_sessions(AgentFilter::All, 0, None)
                .unwrap()
                .is_empty()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "不该排队等写锁: {:?}",
            started.elapsed()
        );
        writer.execute_batch("ROLLBACK;").unwrap();
        drop(ledger);
        drop(writer);
        fs::remove_dir_all(&root).ok();
    }

    fn query_plan(conn: &Connection, sql: &str) -> Vec<String> {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(params![0i64, i64::MAX, Option::<String>::None], |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// 窗口查询必须按 (session_id, eff_ts_ms) 索引 seek 时间区间,且不做整体排序
    /// (没有 ANALYZE 统计,计划与表的大小无关,小库上钉住即可)。
    #[test]
    fn query_plans_use_eff_ts_indexes() {
        let (root, ledger, _, _) = mixed_window_fixture("plan");
        let turns = query_plan(&ledger.conn, TURNS_IN_WINDOW_SQL);
        let tools = query_plan(&ledger.conn, TOOL_EVENTS_IN_WINDOW_SQL);
        let legacy = query_plan(&ledger.conn, LEGACY_TURNS_SQL);
        eprintln!("[plan] turns       : {turns:?}");
        eprintln!("[plan] tool_events : {tools:?}");
        eprintln!("[plan] 改前 turns  : {legacy:?}");
        let seek = |plan: &[String], index: &str| {
            plan.iter().any(|d| {
                d.contains(&format!(
                    "USING INDEX {index} (session_id=? AND eff_ts_ms>? AND eff_ts_ms<?)"
                ))
            })
        };
        assert!(seek(&turns, "idx_turns_sid_eff"), "{turns:?}");
        assert!(seek(&tools, "idx_tool_events_sid_eff"), "{tools:?}");
        for plan in [&turns, &tools] {
            assert!(
                plan[0].starts_with("SCAN s"),
                "外层按会话顺序扫 sessions: {plan:?}"
            );
            assert!(
                !plan.iter().any(|d| d == "USE TEMP B-TREE FOR ORDER BY"),
                "不得整体排序: {plan:?}"
            );
        }
        assert!(
            !legacy.iter().any(|d| d.contains("eff_ts_ms>?")),
            "对照:旧条件用不上时间区间 seek"
        );
        drop(ledger);
        fs::remove_dir_all(&root).ok();
    }

    /// 查询连接槽与删库重建的代次是进程级全局状态:碰它们的测试串行跑。
    static CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn query_connection_is_reused_until_ledger_rebuilt() {
        let _serial = CACHE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let root = temp_root("reuse");
        let db = ledger_db_path(&root).unwrap();
        let query = || {
            usage_ledger_query(
                &root,
                AgentFilter::All,
                0,
                None,
                None,
                0,
                None,
                false,
                HashMap::new(),
            )
            .unwrap();
        };
        // 临时表只活在建它的那条连接上:有它 = 还是同一条连接
        let mark = || {
            let slot = query_slot();
            let cached = slot.as_ref().expect("查询成功后连接应还回槽里");
            cached
                .ledger
                .conn
                .execute_batch("CREATE TEMP TABLE reuse_marker(x)")
                .unwrap();
        };
        let marked = || -> Option<bool> {
            let slot = query_slot();
            let cached = slot.as_ref().filter(|c| c.db_path == db)?;
            let n: i64 = cached
                .ledger
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_temp_master WHERE name = 'reuse_marker'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            Some(n == 1)
        };

        query();
        mark();
        query();
        assert_eq!(marked(), Some(true), "第二次查询复用同一条连接");

        // 并发查询:槽里的连接正被拿着时现开一条,不排队;还回时槽已有就丢掉
        let taken = checkout_query_ledger(&db).unwrap();
        assert!(query_slot().is_none());
        query();
        assert_eq!(marked(), Some(false), "槽空时现开一条");
        checkin_query_ledger(taken);
        assert_eq!(marked(), Some(false), "槽已被占,还回的那条直接关掉");

        // 删库重建(代次 +1)之后,槽里的旧连接不再复用
        mark();
        LEDGER_EPOCH.fetch_add(1, Ordering::SeqCst);
        query();
        assert_eq!(marked(), Some(false), "代次变了必须换新连接");

        drop(query_slot().take());
        fs::remove_dir_all(&root).ok();
    }

    /// 度量:10 万条 turns 下开库与按时间窗查询的耗时(改前改后对比用)。
    /// `cargo test -p mt-usage --lib -- --ignored ledger_query_bench --nocapture`
    #[test]
    #[ignore]
    fn ledger_query_bench() {
        let root = temp_root("bench");
        let db = root.join("usage.db");
        // 2000 个会话 × 50 条 turn,时间铺满 100 天(每天 20 个会话);
        // 每个会话第一条缺时间戳,走会话 mtime 回退
        let day = 86_400_000i64;
        let base = turns::parse_rfc3339_ms("2026-05-01T00:00:00Z").unwrap();
        let names = HashMap::new();
        let mut ledger = Ledger::open(&db).unwrap();
        let started = Instant::now();
        for s in 0..2000i64 {
            let start = base + (s / 20) * day + (s % 20) * 3_600_000;
            let mut lines = vec![claude_line_no_ts(&format!("s{s}-m0"), 7)];
            for k in 1..50i64 {
                lines.push(claude_line(
                    &format!("s{s}-m{k}"),
                    &rfc3339(start + k * 60_000),
                    5 + k as u64,
                ));
            }
            let path = root.join(format!("sess-{s:04}.jsonl"));
            write_with_mtime(&path, &lines, start + 50 * 60_000);
            ledger
                .sync_job(
                    &SessionJob::Claude {
                        main: path,
                        subagents: vec![],
                    },
                    &names,
                )
                .unwrap();
        }
        assert_eq!(turn_count(&ledger), 100_000);
        eprintln!("[bench] 造数据 {:?}", started.elapsed());
        drop(ledger);

        fn time(label: &str, n: u32, f: &mut dyn FnMut()) {
            f(); // 预热
            let t = Instant::now();
            for _ in 0..n {
                f();
            }
            eprintln!("[bench] {label}: {:?}/次", t.elapsed() / n);
        }
        let last = base + 100 * day;
        time("Ledger::open", 50, &mut || drop(Ledger::open(&db).unwrap()));
        // 同一轮里的改前对照(机器上别的构建在跑,跨轮数字噪声大):改前的开库每次都
        // BEGIN IMMEDIATE + 整份 DDL;改前的窗口 SQL 条件现算 COALESCE
        time("  └ 对照:改前开库路径", 50, &mut || {
            let mut conn = Connection::open(&db).unwrap();
            conn.busy_timeout(Duration::from_millis(5000)).unwrap();
            conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))
                .unwrap();
            conn.execute_batch("PRAGMA synchronous=NORMAL;").unwrap();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            tx.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap();
            tx.execute_batch(SCHEMA).unwrap();
            tx.commit().unwrap();
        });
        let ledger = Ledger::open(&db).unwrap();
        for (label, since) in [
            ("近 1 天", last - day),
            ("近 7 天", last - 7 * day),
            ("近 30 天", last - 30 * day),
            ("全部", 0),
        ] {
            time(&format!("窗口 SQL {label}"), 20, &mut || {
                raw_rows(&ledger.conn, TURNS_IN_WINDOW_SQL, since, None, None);
            });
            time(&format!("  └ 对照:改前 SQL {label}"), 20, &mut || {
                raw_rows(&ledger.conn, LEGACY_TURNS_SQL, since, None, None);
            });
        }
        for (label, since) in [
            ("近 1 天", last - day),
            ("近 7 天", last - 7 * day),
            ("近 30 天", last - 30 * day),
            ("全部", 0),
        ] {
            let mut n_turns = 0;
            time(&format!("query_sessions {label}"), 20, &mut || {
                n_turns = ledger
                    .query_sessions(AgentFilter::All, since, None)
                    .unwrap()
                    .iter()
                    .map(|s| s.turns.len())
                    .sum::<usize>();
            });
            eprintln!("[bench]   └ 命中 {n_turns} 条 turn");
        }
        for (label, since) in [("近 7 天", last - 7 * day), ("全部", 0)] {
            time(&format!("usage_ledger_query {label}"), 20, &mut || {
                usage_ledger_query(
                    &root,
                    AgentFilter::All,
                    since,
                    None,
                    None,
                    0,
                    None,
                    false,
                    HashMap::new(),
                )
                .unwrap();
            });
        }
        drop(ledger);
        fs::remove_dir_all(&root).ok();
    }

    // ── agy:transcript 压缩时整份重写,账本只增不删 ──

    fn agy_plan_line(idx: i64, ts: &str, input: u64, cache: u64, output: u64) -> String {
        format!(
            r#"{{"step_index":{idx},"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"{ts}","model":"gemini-3.8-flash-high","input_tokens":{input},"cache_read_tokens":{cache},"output_tokens":{output},"thinking":"思考过程会被压缩掉"}}"#
        )
    }

    fn agy_other_line(idx: i64, kind: &str, ts: &str) -> String {
        format!(
            r#"{{"step_index":{idx},"source":"USER_EXPLICIT","type":"{kind}","status":"DONE","created_at":"{ts}","content":"<USER_REQUEST>\n做点事\n</USER_REQUEST>"}}"#
        )
    }

    fn agy_job(root: &Path, id: &str, parent: Option<&str>) -> SessionJob {
        let transcript = root
            .join(id)
            .join(".system_generated")
            .join("logs")
            .join("transcript_full.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        SessionJob::Agy {
            conv: mt_ai::sessions::AgyConversation {
                id: id.into(),
                title: "压缩测试".into(),
                preview: String::new(),
                workspaces: vec!["d:/Git/proj".into()],
                updated_at: None,
                last_user_input_at: None,
                step_count: 0,
                parent_id: parent.map(str::to_string),
                nesting_depth: i64::from(parent.is_some()),
            },
            transcript,
        }
    }

    fn agy_transcript(job: &SessionJob) -> &Path {
        match job {
            SessionJob::Agy { transcript, .. } => transcript,
            _ => unreachable!(),
        }
    }

    /// 某会话在账本里的 (行数, input 合计, cache_read 合计, output 合计)。
    fn agy_totals(ledger: &Ledger, sid: &str) -> (i64, i64, i64, i64) {
        ledger
            .conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(input),0), COALESCE(SUM(cache_read),0),
                        COALESCE(SUM(output),0)
                 FROM turns WHERE session_id = ?1",
                params![sid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    /// 追加 / 整份重写后变短 / 重写后再追加,外加两种重写变体(同一步原地改写用量、
    /// 回退后序号被复用):已计入的调用一条不丢,还在文件里的调用一条不重。
    #[test]
    fn agy_rewrite_never_loses_or_double_counts() {
        let root = temp_root("agy-rewrite");
        let names = HashMap::new();
        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        let job = agy_job(&root, "conv-a", None);
        let path = agy_transcript(&job).to_path_buf();
        // 每次写都把 mtime 往前推,模拟真实写盘(指纹 = mtime + size)
        let mut mtime = turns::parse_rfc3339_ms("2026-11-19T14:00:00Z").unwrap();
        let mut write = |lines: &[String]| {
            mtime += 1000;
            write_with_mtime(&path, lines, mtime);
        };

        // ① 首次:两次模型调用
        let user0 = agy_other_line(0, "USER_INPUT", "2026-11-19T13:46:08Z");
        let p1 = agy_plan_line(1, "2026-11-19T13:46:08Z", 100, 0, 10);
        let g2 = agy_other_line(2, "GENERIC", "2026-11-19T13:46:14Z");
        let p3 = agy_plan_line(3, "2026-11-19T13:46:14Z", 50, 200, 5);
        write(&[user0.clone(), p1.clone(), g2.clone(), p3.clone()]);
        assert!(ledger.sync_job(&job, &names).unwrap());
        assert_eq!(agy_totals(&ledger, "conv-a"), (2, 150, 200, 15));

        // ② 追加一次调用
        let p5 = agy_plan_line(5, "2026-11-19T13:46:30Z", 30, 400, 7);
        write(&[
            user0.clone(),
            p1.clone(),
            g2.clone(),
            p3.clone(),
            p5.clone(),
        ]);
        let before = fs::metadata(&path).unwrap().len();
        assert!(ledger.sync_job(&job, &names).unwrap());
        assert_eq!(
            agy_totals(&ledger, "conv-a"),
            (3, 180, 600, 22),
            "追加只多一行"
        );

        // ③ 上下文压缩:整份重写,前面的步骤被压成一条摘要,只留最后一次调用
        // (序号不变)。文件变短,但已计入的两次调用不能丢,留下的那次不能重记
        let summary = agy_other_line(4, "CHECKPOINT", "2026-11-19T13:46:29Z");
        write(&[summary.clone(), p5.clone()]);
        assert!(
            fs::metadata(&path).unwrap().len() < before,
            "重写后文件确实变短了"
        );
        assert!(ledger.sync_job(&job, &names).unwrap());
        assert_eq!(
            agy_totals(&ledger, "conv-a"),
            (3, 180, 600, 22),
            "重写变短:历史不丢、留下的那条不重复"
        );

        // ④ 重写之后接着追加
        let p7 = agy_plan_line(7, "2026-11-19T13:47:00Z", 20, 500, 3);
        write(&[summary.clone(), p5.clone(), p7.clone()]);
        assert!(ledger.sync_job(&job, &names).unwrap());
        assert_eq!(
            agy_totals(&ledger, "conv-a"),
            (4, 200, 1100, 25),
            "重写后再追加"
        );

        // 幂等:强制整份重读,数量与合计不变
        ledger.conn.execute("DELETE FROM sync_state", []).unwrap();
        assert!(ledger.sync_job(&job, &names).unwrap());
        assert_eq!(
            agy_totals(&ledger, "conv-a"),
            (4, 200, 1100, 25),
            "重读幂等"
        );

        // ⑤ 同一步被原地改写(序号、时刻不变,用量变了):覆盖,不多出一行
        let p7_final = agy_plan_line(7, "2026-11-19T13:47:00Z", 20, 500, 9);
        write(&[summary.clone(), p5.clone(), p7_final]);
        assert!(ledger.sync_job(&job, &names).unwrap());
        assert_eq!(
            agy_totals(&ledger, "conv-a"),
            (4, 200, 1100, 31),
            "原地改写只覆盖"
        );

        // ⑥ 回退后序号被复用:新的第 7 步时刻不同,是另一次真实调用,旧的第 7 步
        // 也照样留着(它花过钱)
        let p7_after_revert = agy_plan_line(7, "2026-11-19T13:50:00Z", 11, 0, 1);
        write(&[summary, p5, p7_after_revert]);
        assert!(ledger.sync_job(&job, &names).unwrap());
        assert_eq!(
            agy_totals(&ledger, "conv-a"),
            (5, 211, 1100, 32),
            "序号复用不覆盖旧调用"
        );

        // 查询口径:agy 筛选只出 agy,会话元信息来自索引
        let sessions = ledger.query_sessions(AgentFilter::Agy, 0, None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent, "agy");
        assert_eq!(sessions[0].cwd.as_deref(), Some(r"D:\Git\proj"));
        assert_eq!(sessions[0].title.as_deref(), Some("压缩测试"));
        assert_eq!(sessions[0].turns.len(), 5);
        assert!(
            ledger
                .query_sessions(AgentFilter::Claude, 0, None)
                .unwrap()
                .is_empty()
        );
        drop(ledger);
        fs::remove_dir_all(&root).ok();
    }

    /// 跨会话去重只吞真正的复制:fork 原样带过来的步骤只计一次;主会话与子代理
    /// 同一秒各走到第 5 步(实测见过)是两次调用,都要计入。
    #[test]
    fn agy_cross_session_dedup_only_swallows_true_copies() {
        let root = temp_root("agy-dedup");
        let names = HashMap::new();
        let mut ledger = Ledger::open(&root.join("usage.db")).unwrap();
        let p1 = agy_plan_line(1, "2026-11-19T13:47:42Z", 10025, 0, 828);
        let p3 = agy_plan_line(3, "2026-11-19T13:47:51Z", 3188, 7989, 374);
        let p5 = agy_plan_line(5, "2026-11-19T13:48:05Z", 2541, 9172, 544);
        let sub5 = agy_plan_line(5, "2026-11-19T13:48:05Z", 2384, 1788, 32);
        let fork7 = agy_plan_line(7, "2026-11-19T14:00:00Z", 900, 100, 9);
        let jobs = [
            (
                agy_job(&root, "parent", None),
                vec![p1.clone(), p3.clone(), p5],
            ),
            (agy_job(&root, "sub", Some("parent")), vec![sub5]),
            (agy_job(&root, "fork", None), vec![p1, p3, fork7]),
        ];
        for (job, lines) in &jobs {
            write_with_mtime(agy_transcript(job), lines, 1_795_000_000_000);
            assert!(ledger.sync_job(job, &names).unwrap());
        }
        let sessions = ledger.query_sessions(AgentFilter::All, 0, None).unwrap();
        let mut agg = Aggregator::new(0, None, -480, Some("Asia/Shanghai"), false);
        let table = PricingTable::new(HashMap::new());
        for s in &sessions {
            agg.add_session(s, &table);
        }
        let snap = serde_json::to_value(agg.snapshot()).unwrap();
        assert_eq!(
            snap["totalCalls"],
            serde_json::json!(5),
            "父 3 + 子代理 1 + fork 新增 1:fork 带过来的两步只计一次"
        );
        assert_eq!(snap["sessionCount"], serde_json::json!(3));
        fs::remove_dir_all(&root).ok();
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
