//! ─── Antigravity CLI(`agy`)会话:存储层 ─────────────────────────────
//!
//! 只管「东西在哪、长什么样」:定位目录、读会话索引、逐行解析逐步记录。历史面板 /
//! 对话镜像 / 用量统计各自在这之上组装,不要在那几处再写一遍路径与字段名。
//!
//! # 磁盘布局(对照本机 agy 1.5.12 实测,2026-11)
//!
//! ```text
//! ~/.gemini/
//!   config/hooks.json                         全局 hook 配置(hook_registry 写这里)
//!   antigravity-cli/                          CLI 的 appDataDir
//!     conversation_summaries.db               SQLite(WAL)会话索引,见 [`AgyConversation`]
//!     conversations/<id>.db                   轨迹本体(protobuf blob,不解析)
//!     brain/<id>/.system_generated/logs/
//!       transcript_full.jsonl                 逐步记录:正文与用量的权威来源
//!       transcript.jsonl                      压缩版(参数被转成字符串、长字段截断),不用
//!       chunks/…                              内部分块镜像,不用
//! ```
//!
//! - **不认改目录的环境变量**:`ANTIGRAVITY_GEMINI_DIR` 实测不改全局配置与数据目录
//!   的位置,这里一律按 `~/.gemini`。
//! - **transcript 会被整份重写**(上下文压缩时),增量读取方必须处理「文件变短 / 内容
//!   被换掉」→ 从头再读。
//! - **会话 → 项目**只能查索引库:transcript 里没有 cwd。索引里 `workspace_uris`
//!   是 `file:///D:/Git/x` 形态的 JSON 数组。
//! - **子代理会话**(`invoke_subagent`)在索引里 `parent_conversation_id` 非空、
//!   `nesting_depth > 0`;它的 transcript 首步是 `SYSTEM_MESSAGE`(父会话发来的任务),
//!   没有 `USER_INPUT`。历史列表 / 镜像都该跳过它,用量统计则应计入(那也是花掉的 token)。
//! - 同一个索引库里还有 Antigravity 桌面版 / IDE 的会话(`app_data_dir` 为
//!   `antigravity` / `antigravity-ide`),它们的 transcript 不在本目录,一律滤掉。
//!
//! # transcript_full.jsonl 的行形态
//!
//! 一行一步,字段 snake_case:`step_index` / `source`(`USER_EXPLICIT` / `MODEL` /
//! `SYSTEM`)/ `type` / `status`(`DONE` / `ERROR` …)/ `created_at`(RFC 3339 UTC)。
//!
//! | `type` | 含义 | 关键字段 |
//! |---|---|---|
//! | `USER_INPUT` | 用户发言 | `content`:`<USER_REQUEST>…</USER_REQUEST>` 外裹若干元信息块 |
//! | `PLANNER_RESPONSE` | 一次模型调用 | `content`(正文,可缺)/ `thinking` / `tool_calls[{name,args}]` / `model` / 用量 |
//! | `GENERIC` | 一次工具执行的结果 | `content`(前几行是 `Created At:` / `Completed At:`)/ `error` |
//! | `SYSTEM_MESSAGE` | 系统注入(子代理收到的任务等) | `content` |
//! | `ERROR_MESSAGE` | 系统报错(Esc 打断后是 `The stream was interrupted…`) | `content` / `error` |
//!
//! `GENERIC` 不带工具名,与上一条 `PLANNER_RESPONSE` 的 `tool_calls` **按顺序**一一对应。
//!
//! 用量口径(与 `agy -p --output-format stream-json` 的 `total_tokens` 对过账):
//! `input_tokens` **不含**缓存命中部分(`cache_read_tokens` 另计),`output_tokens`
//! **已含**思考 token。

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, SecondsFormat, Utc};
use mt_core::path_key::{posix_ci_eq_key, windows_eq_key};
use serde_json::Value;

use super::grok::percent_decode;

/// 索引库里 CLI 会话的 `app_data_dir` 取值
const CLI_APP_DATA_DIR: &str = "antigravity-cli";

/// 快照读取(WSL 9P / SSH SFTP)单个索引文件(`.db` 或 `-wal`)的字节上限。
/// 护栏而非功能上限:正常数十到数千会话摘要为几百 KB 到数 MB,32 MB 已留足余量,
/// 避免病态大库把桌面端内存或网络打满。
pub const AGY_INDEX_SNAPSHOT_MAX_BYTES: usize = 32 * 1024 * 1024;

static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(0);

/// `~/.gemini`
fn gemini_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini"))
}

/// agy 的全局定制目录 `~/.gemini/config`(`hooks.json` 所在)。
pub fn agy_config_dir() -> Option<PathBuf> {
    gemini_dir().map(|g| g.join("config"))
}

/// agy CLI 的数据目录 `~/.gemini/antigravity-cli`。
pub fn agy_app_data_dir() -> Option<PathBuf> {
    gemini_dir().map(|g| g.join(CLI_APP_DATA_DIR))
}

/// 会话索引库路径(不保证存在)。
pub fn agy_summaries_db_path_in(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("conversation_summaries.db")
}

/// 某会话的 transcript 路径(不检查存在)。id 不过 [`super::session_id_path_safe`]
/// 返回 `None` —— id 来自 hook 端口 / 持久化布局,拼路径前必须挡穿越。
pub fn agy_transcript_path_in(app_data_dir: &Path, conversation_id: &str) -> Option<PathBuf> {
    super::session_id_path_safe(conversation_id).then(|| {
        app_data_dir
            .join("brain")
            .join(conversation_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript_full.jsonl")
    })
}

/// 本机某会话的 transcript;文件不存在(会话刚建、还没落第一步)返回 `None`。
pub fn find_agy_transcript(conversation_id: &str) -> Option<PathBuf> {
    let dir = agy_app_data_dir()?;
    agy_transcript_path_in(&dir, conversation_id).filter(|p| p.is_file())
}

// ─── 会话索引(conversation_summaries.db) ───────────────────────────

/// 索引库里的一条 CLI 会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyConversation {
    pub id: String,
    /// agy 生成的标题(TUI 会话一两轮后才有;`-p` 跑出来的常年为空)
    pub title: String,
    /// 首条用户输入的截断预览(TUI 会话里与 `title` 相同)
    pub preview: String,
    /// 工作区本地路径(已从 `file:///` URI 解码,正斜杠,大小写原样)
    pub workspaces: Vec<String>,
    /// 最后修改时间,RFC 3339 毫秒 UTC(`2026-11-19T13:44:59.312Z`);零值 / 解析不了为 `None`
    pub updated_at: Option<String>,
    /// 最后一次用户输入的时间,口径同上
    pub last_user_input_at: Option<String>,
    pub step_count: i64,
    /// 父会话 id;`Some` = 子代理会话
    pub parent_id: Option<String>,
    pub nesting_depth: i64,
}

impl AgyConversation {
    /// 子代理会话(`invoke_subagent` 拉起的那种):历史列表与镜像跳过,用量计入。
    pub fn is_subagent(&self) -> bool {
        self.parent_id.is_some() || self.nesting_depth > 0
    }

    /// 列表展示用的标题:`title` 优先,空则 `preview`。都空返回空串,由调用方兜底。
    pub fn display_title(&self) -> &str {
        if self.title.trim().is_empty() {
            self.preview.trim()
        } else {
            self.title.trim()
        }
    }

    /// 该会话是否属于 `project_path`:任一工作区与项目路径相同,或落在项目目录之内
    /// (在项目子目录里起的 agy)。比较走 [`windows_eq_key`](分隔符统一、去尾 `/`、
    /// 小写)—— 索引里的盘符大小写并不稳定(`file:///d:/…` 与 `file:///D:/…` 都见过)。
    pub fn belongs_to(&self, project_path: &str) -> bool {
        let project = windows_eq_key(project_path);
        if project.is_empty() {
            return false;
        }
        self.workspaces.iter().any(|ws| {
            let ws = windows_eq_key(ws);
            ws == project
                || ws
                    .strip_prefix(&project)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    /// POSIX 语义下的项目归属判定(供 WSL 与 SSH 远程项目用):比较走 [`posix_ci_eq_key`]
    /// (不把 `\` 当分隔符、去尾 `/`、折叠大小写以兼容 WSL `/mnt/*` drvfs)。
    /// 任一工作区与项目路径相同或位于其子目录下即匹配;项目根为 `/` 时匹配所有 POSIX 绝对路径。
    pub fn belongs_to_posix(&self, project_path: &str) -> bool {
        let trimmed = project_path.trim();
        if trimmed.is_empty() {
            return false;
        }
        if trimmed.starts_with('/') && trimmed.bytes().all(|b| b == b'/') {
            return self.workspaces.iter().any(|ws| ws.starts_with('/'));
        }
        let project = posix_ci_eq_key(trimmed);
        if project.is_empty() {
            return false;
        }
        self.workspaces.iter().any(|ws| {
            let ws = posix_ci_eq_key(ws);
            ws == project
                || ws
                    .strip_prefix(&project)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }
}

/// 读本机索引库里的全部 CLI 会话(含子代理会话,调用方按 [`AgyConversation::is_subagent`]
/// 自己筛)。库不在 / 打不开 / 表结构认不得一律返回空 —— 没装 agy 是常态,不是错误。
pub fn read_agy_conversations() -> Vec<AgyConversation> {
    let Some(dir) = agy_app_data_dir() else {
        return Vec::new();
    };
    let db = agy_summaries_db_path_in(&dir);
    if !db.is_file() {
        return Vec::new();
    }
    read_agy_conversations_from(&db).unwrap_or_else(|e| {
        tracing::warn!("[agy-sessions] 读会话索引失败: {e}");
        Vec::new()
    })
}

/// [`read_agy_conversations`] 的可测部分。
///
/// **只读**打开:库是 agy 进程开着的 WAL 库,读者不阻塞写者;`busy_timeout` 兜住
/// checkpoint 期间的短暂锁。查询只点名确实在用的列,agy 加列不影响;删列 / 改名
/// 时查询失败,返回 `Err`。
pub fn read_agy_conversations_from(db: &Path) -> Result<Vec<AgyConversation>, String> {
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("打开 {} 失败: {e}", db.display()))?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(500));
    query_agy_conversations(&conn)
}

fn query_agy_conversations(conn: &rusqlite::Connection) -> Result<Vec<AgyConversation>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT conversation_id, title, preview, step_count, last_modified_time, \
             workspace_uris, parent_conversation_id, nesting_depth, last_user_input_time \
             FROM conversation_summaries WHERE app_data_dir = ?1",
        )
        .map_err(|e| format!("查询会话索引失败: {e}"))?;
    let rows = stmt
        .query_map([CLI_APP_DATA_DIR], |row| {
            Ok(AgyConversation {
                id: row.get::<_, String>(0)?,
                title: text_or_empty(row.get_ref(1)?),
                preview: text_or_empty(row.get_ref(2)?),
                step_count: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                updated_at: normalize_db_time(&text_or_empty(row.get_ref(4)?)),
                workspaces: parse_workspace_uris(&text_or_empty(row.get_ref(5)?)),
                parent_id: Some(text_or_empty(row.get_ref(6)?))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                nesting_depth: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                last_user_input_at: normalize_db_time(&text_or_empty(row.get_ref(8)?)),
            })
        })
        .map_err(|e| format!("查询会话索引失败: {e}"))?;
    Ok(rows
        .flatten()
        .filter(|c| super::session_id_path_safe(&c.id))
        .collect())
}

struct TempSnapshotDir(PathBuf);

impl TempSnapshotDir {
    fn create() -> Result<Self, String> {
        let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("mt-agy-snap-{}-{ts}-{seq}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| format!("创建临时快照目录失败: {e}"))?;
        Ok(Self(dir))
    }
}

impl Drop for TempSnapshotDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 从内存字节快照(`conversation_summaries.db` + 可选 `-wal`)解析 agy 会话索引。
///
/// 供 WSL(经 9P 拷贝后读)与 SSH 远程(经 SFTP 下载后读)共用:
/// - **不拷贝 `-shm`**:那是原宿主内核私有的 WAL index,SQLite 打开我们的本地临时副本时
///   会自动根据 `-wal` 重建本地 `-shm` 并回放已提交帧;
/// - 带 `-wal` 时先以读写模式打开**临时副本**(绝不碰源库)让 SQLite 回放 `-wal`;
///   若跨网络拷贝碰上 WAL reset 导致带 WAL 打开/查询失败,自动剥掉 `-wal` 降级仅读主库。
pub fn read_agy_conversations_from_snapshot(
    db_bytes: &[u8],
    wal_bytes: Option<&[u8]>,
) -> Result<Vec<AgyConversation>, String> {
    use rusqlite::{Connection, OpenFlags};

    if db_bytes.is_empty() {
        return Err("会话索引快照为空".to_string());
    }
    let tmp = TempSnapshotDir::create()?;
    let db_path = tmp.0.join("conversation_summaries.db");
    let wal_path = tmp.0.join("conversation_summaries.db-wal");
    let shm_path = tmp.0.join("conversation_summaries.db-shm");

    fs::write(&db_path, db_bytes).map_err(|e| format!("写入临时索引库失败: {e}"))?;

    if let Some(wal) = wal_bytes.filter(|w| !w.is_empty()) {
        fs::write(&wal_path, wal).map_err(|e| format!("写入临时 WAL 失败: {e}"))?;
        let with_wal = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("打开临时索引库(含 WAL)失败: {e}"))
        .and_then(|conn| query_agy_conversations(&conn));
        if with_wal.is_ok() {
            return with_wal;
        }
        // 带 WAL 读取失败(例如拷贝瞬间撞上 WAL reset):移除临时 WAL/SHM 后回退仅读主库
        let _ = fs::remove_file(&wal_path);
        let _ = fs::remove_file(&shm_path);
    }

    // 仅主库(不含 WAL):先只读打开;若主库头部记录了 WAL journal mode 且禁止建 shm,
    // 再用读写打开本私有副本(仅在临时目录下创建空 shm,不修改源库)。
    let ro = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("打开临时索引库失败: {e}"))
    .and_then(|conn| query_agy_conversations(&conn));
    if ro.is_ok() {
        return ro;
    }
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("打开临时索引库失败: {e}"))?;
    query_agy_conversations(&conn)
}

/// 带字节上限地读一个本地/UNC 文件;超限返回 `Err`。
fn read_file_capped(path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
    let mut out = Vec::new();
    let limit = (max_bytes as u64).saturating_add(1);
    file.take(limit)
        .read_to_end(&mut out)
        .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    if out.len() > max_bytes {
        return Err(format!(
            "{} 超过快照大小上限({max_bytes} 字节)",
            path.display()
        ));
    }
    Ok(out)
}

/// 经本地临时快照读磁盘上的 agy 索引库(供 WSL 9P `\\wsl$` 用)。
///
/// 跨 9P 直接 `rusqlite::open` 正在被 Linux 侧写入的 WAL 库容易因 `-shm` 共享锁
/// 不兼容而报 `SQLITE_PROTOCOL` / `SQLITE_IOERR_SHMOPEN`;按「先主库、后 `-wal`」
/// 只读拷贝到本地临时目录后再打开,完全不与 Linux 侧争锁,也不漏未 checkpoint 的新会话。
pub fn read_agy_conversations_via_snapshot(db: &Path) -> Result<Vec<AgyConversation>, String> {
    let db_bytes = read_file_capped(db, AGY_INDEX_SNAPSHOT_MAX_BYTES)?;
    let mut wal_os = db.as_os_str().to_os_string();
    wal_os.push("-wal");
    let wal_path = PathBuf::from(wal_os);
    let wal_bytes = if wal_path.is_file() {
        read_file_capped(&wal_path, AGY_INDEX_SNAPSHOT_MAX_BYTES).ok()
    } else {
        None
    };
    read_agy_conversations_from_snapshot(&db_bytes, wal_bytes.as_deref())
}

/// TEXT 列取字符串;NULL / 非文本按空串(Go 的 sqlite 驱动把 datetime 存成 TEXT,
/// 但别家工具改过的库里可能是别的类型,不为一列失败整行)。
fn text_or_empty(v: rusqlite::types::ValueRef<'_>) -> String {
    match v {
        rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        _ => String::new(),
    }
}

/// 索引库的时间串 → RFC 3339 毫秒 UTC。
///
/// 库里的形态是 Go `time.Time` 经 sqlite 驱动落成的 `2026-11-19 13:44:59.3126675+00:00`
/// (小数位数不定,也可能没有);零值是 `0001-01-01 00:00:00+00:00`,表示「没有」。
pub fn normalize_db_time(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with("0001-01-01") {
        return None;
    }
    let iso = raw.replacen(' ', "T", 1);
    DateTime::parse_from_rfc3339(&iso).ok().map(|t| {
        t.with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    })
}

/// `workspace_uris` 列(JSON 字符串数组,可能是空串)→ 本地路径列表。
pub fn parse_workspace_uris(raw: &str) -> Vec<String> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw.trim()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .filter_map(file_uri_to_path)
        .collect()
}

/// `file:///D:/Git/x` → `D:/Git/x`;`file:///home/u/x` → `/home/u/x`;百分号转义解码。
/// 不是 `file://` 本机 URI(远程工作区等)返回 `None`。
pub fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///…`:去掉 authority 为空时多出的那个 `/` 之前先判盘符形态
    let rest = if rest.starts_with('/') {
        rest
    } else {
        // `file://host/share` 这类带主机名的形态不是本机路径
        return None;
    };
    let decoded = percent_decode(rest);
    let bytes = decoded.as_bytes();
    // `/D:/x` → `D:/x`
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        Some(decoded[1..].to_string())
    } else {
        Some(decoded)
    }
}

// ─── transcript_full.jsonl ───────────────────────────────────────

/// 步骤类型。认不出的原样留在 `Other` 里,调用方一般忽略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgyStepKind {
    UserInput,
    PlannerResponse,
    Generic,
    SystemMessage,
    ErrorMessage,
    Other(String),
}

impl AgyStepKind {
    fn parse(raw: &str) -> Self {
        match raw {
            "USER_INPUT" => Self::UserInput,
            "PLANNER_RESPONSE" => Self::PlannerResponse,
            "GENERIC" => Self::Generic,
            "SYSTEM_MESSAGE" => Self::SystemMessage,
            "ERROR_MESSAGE" => Self::ErrorMessage,
            other => Self::Other(other.to_string()),
        }
    }
}

/// 模型发起的一次工具调用(`PLANNER_RESPONSE.tool_calls[]`)。
#[derive(Debug, Clone, PartialEq)]
pub struct AgyToolCall {
    /// agy 内置工具名(`run_command` / `read_file` / `ask_question` …)
    pub name: String,
    /// 参数对象原样(键是 PascalCase,如 `CommandLine` / `Path`)
    pub args: Value,
}

/// transcript 里的一步。
#[derive(Debug, Clone, PartialEq)]
pub struct AgyStep {
    pub step_index: i64,
    pub kind: AgyStepKind,
    /// `USER_EXPLICIT` / `MODEL` / `SYSTEM`
    pub source: String,
    /// `DONE` / `ERROR` / …
    pub status: String,
    /// RFC 3339 UTC(秒级,`2026-11-19T13:44:32Z`)
    pub created_at: Option<String>,
    pub content: Option<String>,
    pub thinking: Option<String>,
    pub error: Option<String>,
    pub model: Option<String>,
    /// 不含缓存命中部分
    pub input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    /// 已含思考 token
    pub output_tokens: Option<u64>,
    pub tool_calls: Vec<AgyToolCall>,
}

impl AgyStep {
    /// 用户发言里真正的请求文本(见 [`agy_user_request_text`])。非 `USER_INPUT` 返回 `None`。
    pub fn user_text(&self) -> Option<String> {
        (self.kind == AgyStepKind::UserInput)
            .then(|| agy_user_request_text(self.content.as_deref().unwrap_or("")))
            .filter(|t| !t.is_empty())
    }

    /// 工具结果正文(见 [`agy_tool_result_text`])。非 `GENERIC` 返回 `None`。
    pub fn tool_result_text(&self) -> Option<String> {
        (self.kind == AgyStepKind::Generic)
            .then(|| agy_tool_result_text(self.content.as_deref().unwrap_or("")))
    }

    /// 这一步是否带用量(只有 `PLANNER_RESPONSE` 带)。
    pub fn has_usage(&self) -> bool {
        self.input_tokens.is_some() || self.output_tokens.is_some()
    }
}

/// 解析 transcript 的一行;非 JSON / 不是对象 / 缺 `type` 返回 `None`。
pub fn parse_agy_step(line: &str) -> Option<AgyStep> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let obj = v.as_object()?;
    let kind = AgyStepKind::parse(obj.get("type")?.as_str()?);
    let s = |key: &str| obj.get(key).and_then(Value::as_str).map(str::to_string);
    let n = |key: &str| obj.get(key).and_then(Value::as_u64);
    let tool_calls = obj
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| {
                    Some(AgyToolCall {
                        name: c.get("name")?.as_str()?.to_string(),
                        args: c.get("args").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(AgyStep {
        step_index: obj.get("step_index").and_then(Value::as_i64).unwrap_or(-1),
        kind,
        source: s("source").unwrap_or_default(),
        status: s("status").unwrap_or_default(),
        created_at: s("created_at"),
        content: s("content"),
        thinking: s("thinking"),
        error: s("error"),
        model: s("model"),
        input_tokens: n("input_tokens"),
        cache_read_tokens: n("cache_read_tokens"),
        output_tokens: n("output_tokens"),
        tool_calls,
    })
}

/// 整份读出 transcript(坏行跳过)。读不到返回空。
pub fn read_agy_transcript(path: &Path) -> Vec<AgyStep> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| parse_agy_step(&l))
        .collect()
}

/// `USER_INPUT.content` → 用户真正发的那句。
///
/// 形态:`<USER_REQUEST>\n{原文}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>…</ADDITIONAL_METADATA>`,
/// 首轮或改设置后还会跟 `<USER_SETTINGS_CHANGE>…` 之类的块。有 `USER_REQUEST` 块就只取
/// 块内;没有(老版本 / 别的入口)就去掉所有已知的元信息块后取剩余部分。
pub fn agy_user_request_text(content: &str) -> String {
    if let Some(inner) = between(content, "<USER_REQUEST>", "</USER_REQUEST>") {
        return inner.trim().to_string();
    }
    let mut text = content.to_string();
    for tag in ["ADDITIONAL_METADATA", "USER_SETTINGS_CHANGE"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        while let Some(start) = text.find(&open) {
            let Some(end_rel) = text[start..].find(&close) else {
                break;
            };
            text.replace_range(start..start + end_rel + close.len(), "");
        }
    }
    text.trim().to_string()
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(&s[start..end])
}

/// `GENERIC.content` → 工具输出正文:去掉开头成对出现的 `Created At:` / `Completed At:`
/// 时间戳行(实测会重复两遍)。错误步(`Encountered error in step execution: …`)原样保留。
pub fn agy_tool_result_text(content: &str) -> String {
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.peek() {
        if line.starts_with("Created At:") || line.starts_with("Completed At:") {
            lines.next();
        } else {
            break;
        }
    }
    lines.collect::<Vec<_>>().join("\n").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-19T13:44:32Z","content":"<USER_REQUEST>\nRead hello.txt\n第二行\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-19T21:44:32+08:00.\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection`.\n</USER_SETTINGS_CHANGE>"}"#;
    const PLAN: &str = r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-19T13:44:34Z","model":"gemini-3.8-flash-high","input_tokens":8913,"cache_read_tokens":0,"output_tokens":408,"thinking":"hmm","tool_calls":[{"name":"read_file","args":{"Path":"C:\\ws\\hello.txt"}},{"name":"run_command","args":{"CommandLine":"echo probe-ok","WaitMsBeforeAsync":5000}}]}"#;
    const RESULT: &str = r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-19T13:46:14Z","content":"Created At: 2026-11-19T21:46:14+08:00\nCompleted At: 2026-11-19T21:46:14+08:00\nCreated At: 2026-11-19T21:46:14+08:00\nCompleted At: 2026-11-19T21:46:14+08:00\nShowing lines 1-2 of 2\n1: probe file line 1\n2: line 2\r"}"#;
    const ABORT: &str = r#"{"step_index":4,"source":"SYSTEM","type":"ERROR_MESSAGE","status":"DONE","error":"The stream was interrupted. Please continue the task you were working on.","created_at":"2026-11-19T13:54:20Z","content":"Error: The stream was interrupted. Please continue the task you were working on."}"#;

    #[test]
    fn 解析用户发言只取请求块() {
        let step = parse_agy_step(USER).unwrap();
        assert_eq!(step.kind, AgyStepKind::UserInput);
        assert_eq!(step.source, "USER_EXPLICIT");
        assert_eq!(step.created_at.as_deref(), Some("2026-11-19T13:44:32Z"));
        assert_eq!(step.user_text().as_deref(), Some("Read hello.txt\n第二行"));
        assert!(!step.has_usage());
    }

    #[test]
    fn 没有请求块时剥掉元信息块() {
        let raw = "hi there\n<ADDITIONAL_METADATA>\nx\n</ADDITIONAL_METADATA>";
        assert_eq!(agy_user_request_text(raw), "hi there");
        assert_eq!(agy_user_request_text("  plain  "), "plain");
    }

    #[test]
    fn 解析模型回合的工具调用与用量() {
        let step = parse_agy_step(PLAN).unwrap();
        assert_eq!(step.kind, AgyStepKind::PlannerResponse);
        assert_eq!(step.model.as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(step.input_tokens, Some(8913));
        assert_eq!(step.cache_read_tokens, Some(0));
        assert_eq!(step.output_tokens, Some(408));
        assert_eq!(step.thinking.as_deref(), Some("hmm"));
        assert!(step.content.is_none());
        let names: Vec<_> = step.tool_calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["read_file", "run_command"]);
        assert_eq!(step.tool_calls[1].args["CommandLine"], "echo probe-ok");
        assert!(step.has_usage());
        assert!(step.user_text().is_none());
    }

    #[test]
    fn 工具结果去掉时间戳行() {
        let step = parse_agy_step(RESULT).unwrap();
        assert_eq!(step.kind, AgyStepKind::Generic);
        assert_eq!(
            step.tool_result_text().as_deref(),
            Some("Showing lines 1-2 of 2\n1: probe file line 1\n2: line 2")
        );
    }

    #[test]
    fn 打断与未知类型() {
        let step = parse_agy_step(ABORT).unwrap();
        assert_eq!(step.kind, AgyStepKind::ErrorMessage);
        assert_eq!(step.source, "SYSTEM");
        assert!(step.error.unwrap().contains("interrupted"));
        let other = parse_agy_step(r#"{"type":"CHECKPOINT","step_index":9}"#).unwrap();
        assert_eq!(other.kind, AgyStepKind::Other("CHECKPOINT".into()));
        assert!(parse_agy_step("not json").is_none());
        assert!(parse_agy_step(r#"{"step_index":1}"#).is_none(), "缺 type");
        assert!(parse_agy_step("[1,2]").is_none());
    }

    #[test]
    fn 时间串归一() {
        assert_eq!(
            normalize_db_time("2026-11-19 13:44:59.3126675+00:00").as_deref(),
            Some("2026-11-19T13:44:59.312Z")
        );
        assert_eq!(
            normalize_db_time("2026-11-19 21:44:59+08:00").as_deref(),
            Some("2026-11-19T13:44:59.000Z")
        );
        assert_eq!(normalize_db_time("0001-01-01 00:00:00+00:00"), None);
        assert_eq!(normalize_db_time(""), None);
        assert_eq!(normalize_db_time("garbage"), None);
    }

    #[test]
    fn 工作区_uri_解码() {
        assert_eq!(
            parse_workspace_uris(r#"["file:///D:/Git/mini-term","file:///d:/Projects/A%20B"]"#),
            ["D:/Git/mini-term", "d:/Projects/A B"]
        );
        assert_eq!(
            file_uri_to_path("file:///home/u/%E4%B8%AD%E6%96%87").as_deref(),
            Some("/home/u/中文")
        );
        assert_eq!(file_uri_to_path("file://server/share/x"), None);
        assert_eq!(file_uri_to_path("https://x"), None);
        assert!(parse_workspace_uris("").is_empty());
        assert!(parse_workspace_uris("null").is_empty());
        // 坏转义原样保留,不 panic
        assert_eq!(
            file_uri_to_path("file:///D:/a%2").as_deref(),
            Some("D:/a%2")
        );
        assert_eq!(
            file_uri_to_path("file:///D:/a%zz").as_deref(),
            Some("D:/a%zz")
        );
    }

    fn conv(workspaces: &[&str]) -> AgyConversation {
        AgyConversation {
            id: "id".into(),
            title: String::new(),
            preview: "预览".into(),
            workspaces: workspaces.iter().map(|s| s.to_string()).collect(),
            updated_at: None,
            last_user_input_at: None,
            step_count: 1,
            parent_id: None,
            nesting_depth: 0,
        }
    }

    #[test]
    fn 项目归属_同目录或子目录_大小写与分隔符不敏感() {
        let c = conv(&["d:/Git/mini-term"]);
        assert!(c.belongs_to(r"D:\Git\mini-term"));
        assert!(c.belongs_to("D:/Git/mini-term/"));
        assert!(!c.belongs_to(r"D:\Git\mini"), "前缀不是目录边界");
        assert!(
            !c.belongs_to(r"D:\Git\mini-term\crates"),
            "项目是会话的子目录不算"
        );
        assert!(conv(&["D:/Git/mini-term/crates/mt-ai"]).belongs_to(r"D:\Git\mini-term"));
        assert!(!conv(&[]).belongs_to(r"D:\Git\mini-term"));
        assert!(!c.belongs_to(""));
    }

    #[test]
    fn 标题回落与子代理判定() {
        let mut c = conv(&[]);
        assert_eq!(c.display_title(), "预览");
        c.title = "One Word Reply".into();
        assert_eq!(c.display_title(), "One Word Reply");
        assert!(!c.is_subagent());
        c.parent_id = Some("p".into());
        assert!(c.is_subagent());
    }

    #[test]
    fn transcript_路径挡穿越() {
        let root = Path::new("/data");
        assert!(agy_transcript_path_in(root, "../x").is_none());
        assert!(agy_transcript_path_in(root, "").is_none());
        let p = agy_transcript_path_in(root, "35ad1fcb-aaf4").unwrap();
        assert!(p.ends_with("brain/35ad1fcb-aaf4/.system_generated/logs/transcript_full.jsonl"));
    }

    /// 按 agy 1.5.12 的真实表结构建一个库,验读取口径:只要 CLI 会话、空串父 id 归 None、
    /// 零值时间归 None、非法 id 丢弃。
    #[test]
    fn 从索引库读会话() {
        let dir = std::env::temp_dir().join(format!(
            "mt-agy-summ-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("conversation_summaries.db");
        {
            let c = rusqlite::Connection::open(&db).unwrap();
            c.execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT \"\",`preview` text NOT NULL DEFAULT \"\",`step_count` integer NOT NULL DEFAULT 0,`last_modified_time` datetime NOT NULL,`workspace_uris` text NOT NULL,`status` text NOT NULL DEFAULT \"\",`source` text NOT NULL DEFAULT \"\",`project_id` text NOT NULL DEFAULT \"\",`agent_name` text NOT NULL DEFAULT \"\",`parent_conversation_id` text NOT NULL DEFAULT \"\",`nesting_depth` integer NOT NULL DEFAULT 0,`battle_id` text NOT NULL DEFAULT \"\",`winning_conversation_id` text NOT NULL DEFAULT \"\",`not_fully_idle` numeric NOT NULL DEFAULT false,`killed` numeric NOT NULL DEFAULT false,`last_user_input_time` datetime NOT NULL,`last_user_input_step_index` integer NOT NULL DEFAULT -1,`app_data_dir` text NOT NULL DEFAULT \"\",`raw_summary` blob,`group_id` text NOT NULL DEFAULT \"\",PRIMARY KEY (`conversation_id`));
                 INSERT INTO conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,parent_conversation_id,nesting_depth,last_user_input_time,app_data_dir) VALUES
                   ('7e54f704-1278','One Word Hello Reply','One Word Hello Reply',5,'2026-11-19 13:54:20.6942645+00:00','[\"file:///C:/ws\"]','',0,'2026-11-19 13:54:17.4253292+00:00','antigravity-cli'),
                   ('a75384f0-7b16','','',6,'2026-11-19 13:48:08.1612944+00:00','[\"file:///C:/ws\"]','35f9ab48-2d2b',1,'0001-01-01 00:00:00+00:00','antigravity-cli'),
                   ('53a95188-06b4','','DataTable',41,'2025-12-27 10:40:16.8277112+00:00','[\"file:///d:/x\"]','',0,'0001-01-01 00:00:00+00:00','antigravity'),
                   ('../evil','','',1,'2026-11-19 13:00:00+00:00','','',0,'0001-01-01 00:00:00+00:00','antigravity-cli');",
            )
            .unwrap();
        }
        let mut rows = read_agy_conversations_from(&db).unwrap();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(rows.len(), 2, "{rows:?}");
        let main = &rows[0];
        assert_eq!(main.id, "7e54f704-1278");
        assert_eq!(main.display_title(), "One Word Hello Reply");
        assert_eq!(main.updated_at.as_deref(), Some("2026-11-19T13:54:20.694Z"));
        assert_eq!(
            main.last_user_input_at.as_deref(),
            Some("2026-11-19T13:54:17.425Z")
        );
        assert_eq!(main.workspaces, ["C:/ws"]);
        assert!(!main.is_subagent());
        let sub = &rows[1];
        assert_eq!(sub.parent_id.as_deref(), Some("35f9ab48-2d2b"));
        assert!(sub.is_subagent());
        assert_eq!(sub.last_user_input_at, None);

        // 表结构认不得 → Err,不 panic
        let bad = dir.join("bad.db");
        rusqlite::Connection::open(&bad)
            .unwrap()
            .execute_batch("CREATE TABLE conversation_summaries (conversation_id text);")
            .unwrap();
        assert!(read_agy_conversations_from(&bad).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn posix_项目归属_保留反斜杠_支持子目录与根目录() {
        let c = conv(&["/home/User/proj"]);
        assert!(c.belongs_to_posix("/home/user/proj"));
        assert!(c.belongs_to_posix("/home/User/proj/"));
        assert!(!c.belongs_to_posix("/home/user/pro"), "前缀不是目录边界");
        assert!(
            !c.belongs_to_posix("/home/user/proj/sub"),
            "项目是会话的子目录不算"
        );
        assert!(conv(&["/home/user/proj/crates/mt-ai"]).belongs_to_posix("/home/user/proj"));
        assert!(conv(&["/mnt/d/Git/Mini-Term"]).belongs_to_posix("/mnt/d/git/mini-term"));
        // 反斜杠在 POSIX 里是普通文件名字符,不等价于 `/`
        assert!(!conv(&[r"/home/u/a\b"]).belongs_to_posix("/home/u/a/b"));
        // 根目录 `/` 匹配任何绝对 POSIX 工作区,不匹配空串或相对路径
        assert!(c.belongs_to_posix("/"));
        assert!(!conv(&["relative/dir"]).belongs_to_posix("/"));
        assert!(!c.belongs_to_posix(""));
    }

    /// 模拟运行中的 agy(WAL 模式、写连接仍打开、尚未 checkpoint):
    /// 只拷贝 `.db` 与 `.db-wal`(绝不拷 `-shm`)到临时快照,新写入的未 checkpoint 会话必须可读;
    /// WAL 损坏时优雅回退仅读主库而不报错。
    #[test]
    fn 快照读取覆盖未_checkpoint_的_wal_并容忍损坏_wal() {
        let dir = std::env::temp_dir().join(format!(
            "mt-agy-wal-snap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("conversation_summaries.db");
        let schema = "CREATE TABLE `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT \"\",`preview` text NOT NULL DEFAULT \"\",`step_count` integer NOT NULL DEFAULT 0,`last_modified_time` datetime NOT NULL,`workspace_uris` text NOT NULL,`parent_conversation_id` text NOT NULL DEFAULT \"\",`nesting_depth` integer NOT NULL DEFAULT 0,`last_user_input_time` datetime NOT NULL,`app_data_dir` text NOT NULL DEFAULT \"\",PRIMARY KEY (`conversation_id`));";

        // 1) 建库并 checkpoint 一条基线记录进入主库
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer
            .execute_batch(&format!(
                "PRAGMA journal_mode=WAL;
                 PRAGMA wal_autocheckpoint=0;
                 {schema}
                 INSERT INTO conversation_summaries VALUES
                   ('base-1','Base','Base',3,'2026-11-19 10:00:00+00:00','[\"file:///home/u/proj\"]','',0,'2026-11-19 10:00:00+00:00','antigravity-cli');
                 PRAGMA wal_checkpoint(TRUNCATE);"
            ))
            .unwrap();

        // 2) 写一条新会话,保持写连接打开且禁止 autocheckpoint —— 它只存在于 `-wal` 中
        writer
            .execute(
                "INSERT INTO conversation_summaries VALUES
                   ('wal-2','Fresh In WAL','Fresh',2,'2026-11-19 12:00:00+00:00','[\"file:///home/u/proj/sub\"]','',0,'2026-11-19 12:00:00+00:00','antigravity-cli')",
                [],
            )
            .unwrap();

        let wal = dir.join("conversation_summaries.db-wal");
        assert!(wal.is_file() && fs::metadata(&wal).unwrap().len() > 0);

        // 通过经快照读(模拟 WSL 9P):两条都可读
        let mut via = read_agy_conversations_via_snapshot(&db).unwrap();
        via.sort_by(|a, b| a.id.cmp(&b.id));
        let via_ids: Vec<&str> = via.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(via_ids, ["base-1", "wal-2"]);

        // 通过内存字节快照读(模拟 SSH SFTP):带 WAL 读出两条,不带 WAL 只读出主库基线
        let db_bytes = fs::read(&db).unwrap();
        let wal_bytes = fs::read(&wal).unwrap();
        let mut from_snap =
            read_agy_conversations_from_snapshot(&db_bytes, Some(&wal_bytes)).unwrap();
        from_snap.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            from_snap.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["base-1", "wal-2"]
        );

        let main_only = read_agy_conversations_from_snapshot(&db_bytes, None).unwrap();
        assert_eq!(
            main_only.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["base-1"]
        );

        // WAL 损坏时回退仅读主库,不报错
        let fallback =
            read_agy_conversations_from_snapshot(&db_bytes, Some(b"corrupted-wal-bytes")).unwrap();
        assert_eq!(
            fallback.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["base-1"]
        );

        // 空字节快照报错
        assert!(read_agy_conversations_from_snapshot(&[], None).is_err());

        // 大小上限守卫
        assert!(read_file_capped(&db, 8).is_err());
        assert!(read_file_capped(&db, db_bytes.len()).is_ok());

        drop(writer);
        let _ = fs::remove_dir_all(&dir);
    }
}
