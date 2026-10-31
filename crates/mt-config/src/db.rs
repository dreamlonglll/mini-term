//! `config.db`:`AppConfig` 的持久化归属方(rusqlite)。
//!
//! # config.json 现在是什么
//!
//! **一份只给 sidecar 读的最小投影**,不再是配置的家。三个 sidecar 二进制
//! (`miniterm-hook` / `mt-ssh-mcp` / `mt-ssh-cli`)通过 `mt_core::config_reader`
//! 自己解析 `{app_data_dir}/config.json`,取 `sshConnections` 与 `projects[]` 的
//! 四个 SSH 字段做能力令牌鉴权,而且**每次请求重读**(主程序里改「关联 SSH」范围
//! 要即时生效)。那条链路必须原地不动:
//!
//! - `mt-core` 的依赖铁律是只依赖 serde/serde_json/dirs —— 给它加 rusqlite 就是
//!   给每个 sidecar 静态塞进一份 SQLite,而 hook 是每次事件冷启动的小程序;
//! - `ssh_service.rs` 那道「拒绝传输 mini-term 自己的 config.json」的安全护栏
//!   (它是明文凭据库)按的就是这个路径;
//! - 审计日志与 IPC socket 目录也拿 config.json 的所在目录当锚点。
//!
//! 于是取舍是:**config.json 保留、但瘦身成投影**,sidecar 侧一行代码不用改,
//! 其余全部内容搬进本库。副作用是明文密码的暴露面从「整份配置」缩到了
//! 「一个只有 SSH 的小文件」。
//!
//! # 为什么值得搬
//!
//! 与 `mt-layout` 同一笔账,只是量级更大:改一个字号、切一次主题、展开一个目录,
//! 此前都要把整份 config.json `to_string_pretty` 重写 + 复制一份等大的 `.bak`
//! (实测本机 64 KB)。现在是一个事务里几十行 upsert,且**内容没变的行不落盘**。
//!
//! # 存储形状:settings 走 kv,projects/sshConnections 一行一条
//!
//! 落库走的是 `serde_json::to_value(&AppConfig)` 拆出来的那张 Map ——
//! **不逐字段手写映射**。这条决定很关键:`AppConfig` 有四十多个字段且还在长,
//! 手写映射意味着每加一个设置项都要同步改存储代码,漏一处就是「设置存不下来」
//! 这类最难查的 bug。走 serde 的话,字段增删自动跟随,camelCase 与
//! `skip_serializing_if` 的语义也与旧 config.json **逐字节一致**。
//!
//! `projects` 与 `sshConnections` 从那张 Map 里摘出去单独入表(id 主键 + `ord`
//! 保序):它们是**逐条编辑**的实体,一行一条才能做到改一个项目只写一行。
//! 其余键(含嵌套的 `projectTree` / `mobileRelay` / `sessionLineage`)整个存进
//! settings 的一个 value —— 与 `mt-layout` 里不拆分屏树同一个论证:永远整读整写
//! 的东西,拆成关系表只换来维护递归完整性的负担。
//!
//! # 与 layout.db 分家
//!
//! 两个库而不是两张表:配置写频次低、布局写频次高(拖分隔条),WAL checkpoint
//! 的节奏本就不同;更要紧的是损坏半径 —— 布局丢了回到默认分屏,配置丢了是
//! 项目列表与 SSH 连接全没。后者因此有真备份(见 [`ConfigDb::backup_to`]),
//! 前者没有。
//!
//! # 前向兼容:旧版本不许抹掉新版本写的东西
//!
//! 用户会在预览版与正式版之间来回装,同一个库先后被新旧两个版本读写。规矩是
//! **旧版本只动自己认识的东西**:
//!
//! - **settings 里不认识的键原样留在库里**:不读进内存、不改写、不删。保存时只删
//!   两类键 —— 本版本认识、但这次序列化省略了的(用户把某项清回了 `None`,或是
//!   `skip_serializing` 的只读字段),以及 [`RETIRED_KEYS`] 里显式列出的下线键。
//!   「认识」的判据是 `AppConfig` 的字段表(见 [`known_setting_keys`]);
//! - **项目 / 连接行里不认识的字段**经 `ProjectConfig::extra` / `SshConnection::extra`
//!   (`#[serde(flatten)]`)原样带着往返;下线字段见 [`RETIRED_CONNECTION_KEYS`];
//! - **单个值 / 单行读不懂时降级,不整库失败**:坏掉的 settings 键本次按默认值顶上,
//!   坏行本次跳过;两者在库里的原文都**原样保留** —— 之后的保存只要用户没改过
//!   那一项,就不碰它(见 [`Degraded`])。多半是新版本改了某个字段的形状,换回新
//!   版本就又读得懂了。只有降级之后整份仍凑不成一个 `AppConfig` 才报错(进只读模式)。
//!
//! sidecar 读的 `config.json` 投影不受影响:投影只写本版本已知的字段(见
//! `config.rs` 的 `ssh_projection`),未知字段只活在库里。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::config::{AppConfig, ProjectConfig, SshConnection};

/// 配置库 schema 版本。
///
/// ⚠️ 与 `usage.db` 相反、与 `layout.db` 相同:**版本不匹配绝不删表重建**。
/// 账本是 JSONL 的派生缓存,重建只是多跑一次 backfill;配置是第一手数据,
/// 重建即用户资产蒸发。新旧版本共用一个库的规矩见模块注释「前向兼容」段。
const SCHEMA_VERSION: i64 = 1;

/// 历史上确实下线过的 settings 键:保存时从库里删掉。**只有这里列出的**不认识的键
/// 会被删,其余不认识的键一律原样保留(理由见模块注释「前向兼容」段)。
///
/// ⚠️ **下线的键名永不复用** —— 带着这张表的旧版本会把同名新键当垃圾删掉。
///
/// 盘点口径(git log 逐条核过 `AppConfig` 被删的字段):
/// - `skin`:内置皮肤栏移除(c871442,2026-08-25),晚于配置入库,库里真有;
/// - `aiAttentionNotify`:「待确认时也提醒」开关移除(2026-10-31),待确认改为
///   固定随完成通知的三个通道提醒,库里真有;
/// - 其余下线早于配置入库(3d142eb,2026-08-20)。入库只经 `AppConfig` 序列化,
///   库里本不该有,列出来是为了这张表就是完整的历史清单:四个面板显隐开关
///   (Activity Bar 改版)、`aiPanelSize`(AI 历史面板移除)、`ccConnect`
///   (cc-connect 集成整体移除)。
const RETIRED_KEYS: &[&str] = &[
    "aiAttentionNotify",
    "aiPanelSize",
    "ccConnect",
    "filesVisible",
    "gitVisible",
    "projectsVisible",
    "sessionsVisible",
    "skin",
];

/// SSH 连接行里下线过的字段:`proxyJump`(跳板机功能移除)、`agentAccessible`
/// (改为按项目关联)。两者都早于配置入库,只可能经存量 `config.json` 的一次性
/// 导入落进 `SshConnection::extra`,入库前剥掉。
///
/// 项目行没有对应的表:`ProjectConfig` 从来没删过字段(同样按 git log 核过)。
const RETIRED_CONNECTION_KEYS: &[&str] = &["agentAccessible", "proxyJump"];

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS projects (
  id   TEXT PRIMARY KEY,
  ord  INTEGER NOT NULL,
  data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS ssh_connections (
  id   TEXT PRIMARY KEY,
  ord  INTEGER NOT NULL,
  data TEXT NOT NULL
);
";

/// 「库里已经有一份配置」的标记。**不靠「表里有没有行」判** —— 用户把项目全删光
/// 后 projects 表就是空的,按后者判会认为库是空的、转头又从 config.json 灌一遍,
/// 把删掉的项目全复活。
const META_INITIALIZED: &str = "initialized";
const META_SCHEMA_VERSION: &str = "schema_version";
/// `downloadDir` 特意存进旧版本不会清理的 meta，而不是 settings。
/// 这样用户短暂降级并保存其它设置后，再升级回来仍能恢复下载目录覆盖值。
///
/// 「前向兼容」段那套通用机制**盖不住它**,所以这条特判留着:通用机制只管得住
/// 本版本之后的版本,而 v1.1.4 ~ v1.13.7 这些已发版本只认 meta 里这一份 —— 挪回
/// settings 的话,降级到它们时下载目录读不到,它们保存时还会把 settings 里那条
/// 当残留删掉。
const META_DOWNLOAD_DIR: &str = "setting.downloadDir";

/// 摘出去单独入表的两个键(其余进 settings)。
const KEY_PROJECTS: &str = "projects";
const KEY_SSH_CONNECTIONS: &str = "sshConnections";
const KEY_DOWNLOAD_DIR: &str = "downloadDir";

/// `config.db` 的读写口。
pub(crate) struct ConfigDb {
    conn: Mutex<Connection>,
    path: PathBuf,
    /// 最近一次 [`load`](Self::load) 降级掉的东西。**锁序:先 `conn` 后它**,且只在
    /// 持有 `conn` 时读写 —— load / save 因此天然串行,不会互相拿到半截状态。
    degraded: Mutex<Degraded>,
}

/// 一次加载里「读不懂、先降级」的键与行,保存时据此**不覆盖库里的原文**。
///
/// 多半是新版本改了某个字段的形状(比如把字符串改成了对象):本版本读不懂,但换回
/// 新版本还读得懂,所以库里那份原文不能被本版本的默认值顶掉 —— **除非用户在本版本
/// 里改了这一项**,那就是用户的新意图,照写。
#[derive(Debug, Default, Clone)]
struct Degraded {
    /// 坏掉的 settings 键 → 加载那一刻它在内存里(已过 `migrate_config`)序列化出来的
    /// 值,`None` = 序列化时省略。保存时值还是这个 = 用户没动过 → 库里原文不碰。
    settings: HashMap<String, Option<String>>,
    /// 解析失败、本次跳过的项目行 id:保存时不当残留删掉。
    projects: HashSet<String>,
    /// 同上,SSH 连接行。
    connections: HashSet<String>,
}

impl ConfigDb {
    /// `{dir}/config.db`。
    ///
    /// 打不开时先尝试上一代备份 `config.db.bak` 自愈(与 config.json 时代的
    /// `.bak` 同语义),备份也不行才向上抛 —— **绝不静默重建空库**:那等于
    /// 一次读盘故障就把用户的项目列表和 SSH 连接全清了。
    pub fn open_at(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("创建应用数据目录失败: {}", dir.display()))?;
        let path = dir.join("config.db");
        match Self::try_open(&path) {
            Ok(db) => Ok(db),
            Err(first) => {
                let bak = path.with_extension("db.bak");
                if !bak.exists() {
                    return Err(first);
                }
                // 坏掉的主库先挪走留证,再把备份顶上
                let corrupt = path.with_extension("db.corrupt");
                let _ = fs::remove_file(&corrupt);
                let _ = fs::rename(&path, &corrupt);
                fs::copy(&bak, &path)
                    .with_context(|| format!("从备份恢复配置库失败: {}", bak.display()))?;
                let db = Self::try_open(&path).map_err(|second| {
                    anyhow!("配置库损坏且备份不可用: {first:#} / 备份: {second:#}")
                })?;
                eprintln!(
                    "[config] config.db 打不开({first:#}),已用备份 {} 恢复(坏库留在 {})",
                    bak.display(),
                    corrupt.display()
                );
                Ok(db)
            }
        }
    }

    fn try_open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("打开配置库失败: {}", path.display()))?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        // journal_mode 是有返回行的语句,得走 query_row。转不过去不算失败:
        // 退回默认的 delete 模式照样能读写。
        let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0));
        // 稳态只有几十 KB 的库,默认 1000 页(约 4MB)阈值意味着 WAL 能长到主库的
        // 上百倍才回收一次(理由同 mt-layout)。
        let _ = conn.execute_batch("PRAGMA wal_autocheckpoint=32");
        // 外键没用上,但 `synchronous=FULL` 值得:配置不可再生,断电丢最后一次
        // 保存比多花一次 fsync 贵得多(布局库那边刻意没开这条)。
        let _ = conn.execute_batch("PRAGMA synchronous=FULL");
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("建表失败: {}", path.display()))?;

        let db = Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
            degraded: Mutex::new(Degraded::default()),
        };
        db.check_schema_version();
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn check_schema_version(&self) {
        match self
            .meta_get(META_SCHEMA_VERSION)
            .and_then(|v| v.parse::<i64>().ok())
        {
            Some(v) if v > SCHEMA_VERSION => {
                eprintln!("[config] 库版本 {v} 高于本程序的 {SCHEMA_VERSION},按兼容模式读写");
            }
            Some(v) if v == SCHEMA_VERSION => {}
            _ => self.meta_set(META_SCHEMA_VERSION, &SCHEMA_VERSION.to_string()),
        }
    }

    fn meta_get(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
    }

    fn meta_set(&self, key: &str, value: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            );
        }
    }

    /// 库里还没有过一份配置(首启 / 尚未从 config.json 迁移)。
    pub fn is_empty(&self) -> bool {
        self.meta_get(META_INITIALIZED).is_none()
    }

    /// 读出整份配置。库为空时返回 `Ok(None)`(调用方按「首次启动」处理)。
    ///
    /// **单个值 / 单行读不懂时降级,不整体失败**:坏掉的 settings 键按默认值顶上、
    /// 坏行跳过,各打一行日志,库里的原文留着不动(保存时也不覆盖,见 [`Degraded`])。
    /// 此前是整体失败 → 进只读模式,于是新版本改一个字段的形状,就能让装回旧版本的
    /// 用户整份配置不可写。只有降级之后整份仍凑不成 `AppConfig` 才报错。
    pub fn load(&self) -> Result<Option<AppConfig>> {
        if self.is_empty() {
            return Ok(None);
        }
        let conn = self.conn.lock().map_err(|_| anyhow!("配置库锁中毒"))?;

        let mut raw: Vec<(String, String)> = {
            let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        // downloadDir 以 meta 那份为准(住那儿的理由见 `META_DOWNLOAD_DIR`)
        if let Some(value) = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![META_DOWNLOAD_DIR],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            raw.retain(|(key, _)| key != KEY_DOWNLOAD_DIR);
            raw.push((KEY_DOWNLOAD_DIR.to_string(), value));
        }
        let (projects, bad_projects) = read_rows::<ProjectConfig>(&conn, "projects")?;
        let (connections, bad_connections) = read_rows::<SshConnection>(&conn, "ssh_connections")?;

        let (mut config, bad_keys) = settings_from_raw(raw)?;
        config.projects = projects;
        config.ssh_connections = connections;

        let degraded = Degraded {
            settings: pin_settings(&config, &bad_keys)?,
            projects: bad_projects,
            connections: bad_connections,
        };
        // 还持着 conn 锁时换进去(锁序见 `degraded` 字段)
        *self
            .degraded
            .lock()
            .map_err(|_| anyhow!("配置库降级表锁中毒"))? = degraded;
        Ok(Some(config))
    }

    /// 整份写回。一个事务里做完:settings 逐键 upsert + 清理消失的键,
    /// projects / sshConnections 逐行 upsert + 删除消失的 id。
    ///
    /// 「整份 API、行级落盘」是刻意的:调用方(`ConfigStore::save`)的契约仍是
    /// 「拿一份完整配置写下去」,不必为每种改动各开一个入口;而磁盘上只有真正
    /// 变了的行被触碰。
    ///
    /// 「消失」只对本版本认识的东西成立:不认识的 settings 键、加载时读不懂的键与行
    /// 一律原样留着(见模块注释「前向兼容」段)。
    pub fn save(&self, config: &AppConfig) -> Result<()> {
        let (mut settings, projects, connections) = split_config(config)?;
        let download_dir = settings
            .iter()
            .position(|(key, _)| key == KEY_DOWNLOAD_DIR)
            .map(|index| settings.swap_remove(index).1);

        let mut conn = self.conn.lock().map_err(|_| anyhow!("配置库锁中毒"))?;
        // 在副本上记账,事务提交成功才换回去 —— 失败的保存不该让降级键「转正」
        let mut degraded = self
            .degraded
            .lock()
            .map_err(|_| anyhow!("配置库降级表锁中毒"))?
            .clone();
        let tx = conn.transaction()?;
        {
            let mut put = tx.prepare(
                "INSERT INTO settings(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value
                 WHERE value <> excluded.value",
            )?;
            let mut live: HashSet<String> = HashSet::new();
            for (key, value) in &settings {
                live.insert(key.clone());
                if degraded.keeps_setting(key, Some(value)) {
                    continue;
                }
                put.execute(params![key, value])?;
            }
            // 库里有、这次没写的键,只删两类:下线表里的,和本版本认识但这次省略了的
            // (用户清回了缺省 / 只读不写的字段)—— 后者留着的话,下次 load 会把旧值
            // 塞回 AppConfig。不认识的键多半是新版本写的,原样留着。
            let known = known_setting_keys();
            let stale = stale_keys(&tx, "SELECT key FROM settings", &live)?;
            let mut del = tx.prepare("DELETE FROM settings WHERE key = ?1")?;
            for key in stale {
                let retired = RETIRED_KEYS.contains(&key.as_str());
                if !retired && !known.contains(key.as_str()) {
                    continue;
                }
                // downloadDir 的原文在 meta,settings 里同名的只可能是残留,不参与降级
                if !retired && key != KEY_DOWNLOAD_DIR && degraded.keeps_setting(&key, None) {
                    continue;
                }
                del.execute(params![key])?;
            }
        }
        write_ordered(&tx, "projects", &projects, &mut degraded.projects)?;
        write_ordered(
            &tx,
            "ssh_connections",
            &connections,
            &mut degraded.connections,
        )?;
        if !degraded.keeps_setting(KEY_DOWNLOAD_DIR, download_dir.as_deref()) {
            match download_dir {
                Some(value) => {
                    tx.execute(
                        "INSERT INTO meta(key, value) VALUES(?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        params![META_DOWNLOAD_DIR, value],
                    )?;
                }
                None => {
                    tx.execute(
                        "DELETE FROM meta WHERE key = ?1",
                        params![META_DOWNLOAD_DIR],
                    )?;
                }
            }
        }
        tx.execute(
            "INSERT INTO meta(key, value) VALUES(?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![META_INITIALIZED],
        )?;
        tx.commit()?;
        if let Ok(mut slot) = self.degraded.lock() {
            *slot = degraded;
        }
        Ok(())
    }

    /// 备份到 `config.db.bak`。走 SQLite 的 backup API 而不是文件拷贝 ——
    /// WAL 模式下直接 copy 主库文件会漏掉还在 WAL 里没 checkpoint 的那部分。
    ///
    /// 在每次成功 [`load`](Self::load) 之后调一次(每启动一代备份),
    /// 与 config.json 时代「覆写前留一代 .bak」是同一份保险。
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("配置库锁中毒"))?;
        conn.backup(rusqlite::MAIN_DB, path, None)
            .with_context(|| format!("备份配置库到 {} 失败", path.display()))?;
        Ok(())
    }

    /// 密码封存迁移后的清扫。SQLite 更新一行**不会抹掉页内旧 cell 的字节**,明文
    /// 密码会作为页内碎片残留;WAL 旧帧里同样躺着明文页。`VACUUM` 按逻辑内容把库
    /// 整个重写,`wal_checkpoint(TRUNCATE)` 把 WAL 截成零。只在真的封存过东西时调
    /// (稳态几十 KB 的库,跑一次是毫秒级),平时不跑。
    pub fn scrub_after_secret_rewrite(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("配置库锁中毒"))?;
        conn.execute_batch("VACUUM").context("VACUUM 配置库失败")?;
        // checkpoint 是有返回行的语句(busy / log / checkpointed),走 query_row。
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .context("截断配置库 WAL 失败")?;
        Ok(())
    }
}

impl Degraded {
    /// 这个 settings 键这次保存要不要**留着库里的原文不碰**:它是加载时降级掉的键,
    /// 且值还是加载那一刻的样子(`current` = 这次序列化出来的值,`None` = 省略)。
    ///
    /// 值变了 = 用户在本版本里改过这一项,那是用户的新意图:就此转正(移出降级表)、
    /// 照常写。
    fn keeps_setting(&mut self, key: &str, current: Option<&str>) -> bool {
        match self.settings.get(key) {
            Some(pinned) if pinned.as_deref() == current => true,
            Some(_) => {
                self.settings.remove(key);
                false
            }
            None => false,
        }
    }
}

/// `AppConfig` 认得的全部顶层键(serde 改名后的 camelCase;含 `skip_serializing` 的
/// 只读字段,它们库里不该有,有就当残留删)。不在这里的键就是「不认识的」。
fn known_setting_keys() -> &'static HashSet<&'static str> {
    static KEYS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    KEYS.get_or_init(|| struct_fields::<AppConfig>().iter().copied().collect())
}

/// 取一个结构体的 serde 字段表(`#[derive(Deserialize)]` 生成的那张 `FIELDS`)。
///
/// 做法是递给它一个只认 `deserialize_struct` 的探针反序列化器:derive 出来的代码
/// 会把字段表原样交进来,拿到就报错收工。⚠️ **前提是结构体本身没有
/// `#[serde(flatten)]`** —— 带 flatten 的结构体改走 `deserialize_map`,交不出字段表,
/// 这里会拿到空表(单测 `已知键覆盖全部序列化字段` 钉着 `AppConfig` 这一条)。
fn struct_fields<T: DeserializeOwned>() -> &'static [&'static str] {
    use serde::de::{Error as _, Visitor};

    struct Probe(Option<&'static [&'static str]>);

    impl<'de> serde::Deserializer<'de> for &mut Probe {
        type Error = serde::de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(
            self,
            _visitor: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            Err(Self::Error::custom("字段表探针"))
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _visitor: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            self.0 = Some(fields);
            Err(Self::Error::custom("字段表探针"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }

    let mut probe = Probe(None);
    let _ = T::deserialize(&mut probe);
    probe.0.unwrap_or(&[])
}

/// settings 原文 → `AppConfig`(projects / sshConnections 留空,由调用方填)。
/// 第二项是读不懂、本次按默认值顶上的键。
///
/// 不认识的键连 JSON 都不解:它们不进内存(serde 本来也会忽略),坏成什么样都
/// 与本版本无关,库里原样留着。
fn settings_from_raw(raw: Vec<(String, String)>) -> Result<(AppConfig, Vec<String>)> {
    let known = known_setting_keys();
    let mut map = Map::new();
    let mut bad = Vec::new();
    for (key, text) in raw {
        // projects / sshConnections 住自己的表,settings 里要是有同名残留也不认
        if !known.contains(key.as_str()) || key == KEY_PROJECTS || key == KEY_SSH_CONNECTIONS {
            continue;
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => {
                map.insert(key, value);
            }
            Err(err) => {
                eprintln!(
                    "[config] 配置项 {key} 的值不是合法 JSON({err}),本次按默认值,库里原值保留"
                );
                bad.push(key);
            }
        }
    }

    // 快路径:一个坏键都没有,整份一次过(稳态走这里,只解析一遍)
    let first = parse_settings(map.clone());
    if bad.is_empty()
        && let Ok(config) = first
    {
        return Ok((config, bad));
    }

    let defaults = default_settings()?;
    if first.is_err() {
        // 慢路径(只在整份凑不起来时走):逐键放进一份默认配置里试,揪出是哪几个
        // 键拖垮了整份。serde 的字段彼此独立,单键试得出来就是它的问题。
        let keys: Vec<String> = map.keys().cloned().collect();
        for key in keys {
            let Some(value) = map.get(&key).cloned() else {
                continue;
            };
            let mut probe = defaults.clone();
            probe.insert(key.clone(), value);
            if let Err(err) = parse_settings(probe) {
                eprintln!(
                    "[config] 配置项 {key} 的值不符合当前 schema({err}),本次按默认值,库里原值保留"
                );
                map.remove(&key);
                bad.push(key);
            }
        }
    }
    // 坏键用默认值顶上:`defaultShell` / `availableShells` 这类没有 serde 缺省的必填键
    // 缺了整份就起不来,拿 `AppConfig::default()` 那份补;其余省略即走 serde 缺省。
    for key in &bad {
        if let Some(value) = defaults.get(key) {
            map.insert(key.clone(), value.clone());
        }
    }
    let config = parse_settings(map).context("配置库内容不符合当前 schema")?;
    Ok((config, bad))
}

/// settings 的 Map(不含 projects / sshConnections)→ `AppConfig`,两张表先按空的填。
fn parse_settings(mut map: Map<String, Value>) -> serde_json::Result<AppConfig> {
    map.insert(KEY_PROJECTS.to_string(), Value::Array(Vec::new()));
    map.insert(KEY_SSH_CONNECTIONS.to_string(), Value::Array(Vec::new()));
    serde_json::from_value(Value::Object(map))
}

/// `AppConfig::default()` 序列化出来的 settings(不含 projects / sshConnections)。
fn default_settings() -> Result<Map<String, Value>> {
    match serde_json::to_value(AppConfig::default()).context("默认配置序列化失败")? {
        Value::Object(mut map) => {
            map.remove(KEY_PROJECTS);
            map.remove(KEY_SSH_CONNECTIONS);
            Ok(map)
        }
        _ => Err(anyhow!("默认配置序列化结果不是对象")),
    }
}

/// 给降级键记下「用户没动过」时它的样子:过一遍 `migrate_config`(调用方拿到库里
/// 读出来的配置后都会过这一遍,手上那份就是这个样子)再序列化。
fn pin_settings(
    config: &AppConfig,
    bad_keys: &[String],
) -> Result<HashMap<String, Option<String>>> {
    if bad_keys.is_empty() {
        return Ok(HashMap::new());
    }
    let (settings, _, _) = split_config(&crate::config::migrate_config(config.clone()))?;
    Ok(bad_keys
        .iter()
        .map(|key| {
            let value = settings
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone());
            (key.clone(), value)
        })
        .collect())
}

/// 把一份 `AppConfig` 拆成 (settings 键值对, projects 行, sshConnections 行)。
///
/// 走 serde 的 Value 而不是逐字段手写 —— 理由见模块注释。
type Rows = Vec<(String, String)>;
fn split_config(config: &AppConfig) -> Result<(Rows, Rows, Rows)> {
    let value = serde_json::to_value(config).context("配置序列化失败")?;
    let Value::Object(mut map) = value else {
        return Err(anyhow!("配置序列化结果不是对象"));
    };
    let projects = take_rows(&mut map, KEY_PROJECTS, &[])?;
    let connections = take_rows(&mut map, KEY_SSH_CONNECTIONS, RETIRED_CONNECTION_KEYS)?;

    let mut settings = Vec::with_capacity(map.len());
    for (key, value) in map {
        settings.push((key, serde_json::to_string(&value)?));
    }
    Ok((settings, projects, connections))
}

/// 从 Map 里摘出一个数组字段,变成 (id, 整条 JSON) 的行;`retired` 里的下线字段
/// (只可能躺在 `extra` 里)顺手剥掉。
///
/// 缺 `id` 的元素直接报错而不是跳过:那意味着 schema 出了问题,静默丢一条
/// 等于用户少一个项目。
fn take_rows(map: &mut Map<String, Value>, key: &str, retired: &[&str]) -> Result<Rows> {
    let Some(Value::Array(items)) = map.remove(key) else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::with_capacity(items.len());
    for mut item in items {
        if let Value::Object(fields) = &mut item {
            // retain 而不是 remove:开了 preserve_order 时 remove 会打乱其余键的顺序
            fields.retain(|name, _| !retired.contains(&name.as_str()));
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{key} 里有一条记录缺 id"))?
            .to_string();
        rows.push((id, serde_json::to_string(&item)?));
    }
    Ok(rows)
}

/// 按 `ord` 读回一张「一行一条」的表,逐行反序列化。
///
/// 单行读不懂(不是 JSON / 字段类型对不上 —— 多半是新版本改过形状)**只跳过这一行**
/// 并打日志,整张表照读;第二项是这些行的 id,保存时据此不把它们当残留删掉。
fn read_rows<T: DeserializeOwned>(
    conn: &Connection,
    table: &str,
) -> Result<(Vec<T>, HashSet<String>)> {
    let sql = format!("SELECT id, data FROM {table} ORDER BY ord");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    let mut bad = HashSet::new();
    for row in rows {
        let (id, raw) = row?;
        match serde_json::from_str::<T>(&raw) {
            Ok(item) => out.push(item),
            Err(err) => {
                eprintln!(
                    "[config] {table} 里 id={id} 的一行解析失败({err}),本次跳过,库里原样保留"
                );
                bad.insert(id);
            }
        }
    }
    Ok((out, bad))
}

/// 写一张「一行一条」的表:逐行 upsert(内容与顺序都没变的行不触碰)+ 删除消失的 id。
///
/// `preserved` 是加载时读不懂、跳过了的行:它们不在 `rows` 里,但**不算消失**,留着
/// 不删(保留原 `ord`,可能与本次的序号撞上 —— 换回读得懂的版本时位置也许挪一格,
/// 但不丢)。`rows` 里出现同 id 的行 = 用户在本版本里重建了它,就此转正、照常覆盖。
fn write_ordered(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    rows: &Rows,
    preserved: &mut HashSet<String>,
) -> Result<()> {
    let sql = format!(
        "INSERT INTO {table}(id, ord, data) VALUES(?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET ord = excluded.ord, data = excluded.data
         WHERE ord <> excluded.ord OR data <> excluded.data"
    );
    let mut put = tx.prepare(&sql)?;
    let mut live: HashSet<String> = HashSet::new();
    for (index, (id, data)) in rows.iter().enumerate() {
        put.execute(params![id, index as i64, data])?;
        preserved.remove(id);
        live.insert(id.clone());
    }
    let stale = stale_keys(tx, &format!("SELECT id FROM {table}"), &live)?;
    let mut del = tx.prepare(&format!("DELETE FROM {table} WHERE id = ?1"))?;
    for id in stale {
        if preserved.contains(&id) {
            continue;
        }
        del.execute(params![id])?;
    }
    Ok(())
}

/// 库里有、但本次不再出现的主键。
fn stale_keys(
    tx: &rusqlite::Transaction<'_>,
    select: &str,
    live: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut stmt = tx.prepare(select)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut stale = Vec::new();
    for row in rows {
        let key = row?;
        if !live.contains(&key) {
            stale.push(key);
        }
    }
    Ok(stale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ShellConfig;

    fn temp_dir(label: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mt-config-db-{label}-{ts}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn project(id: &str, name: &str) -> ProjectConfig {
        ProjectConfig::new(id, name, format!("D:/{id}"))
    }

    fn conn(id: &str) -> SshConnection {
        SshConnection {
            id: id.into(),
            name: format!("conn-{id}"),
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            password: Some("secret".into()),
            identity_file: None,
            group: None,
            extra: Default::default(),
        }
    }

    /// 绕过 `save` 直接往库里塞一行原文 —— 模拟「更新的版本写过这个库」。
    fn put_raw(db: &ConfigDb, sql: &str, params: &[&str]) {
        let conn = db.conn.lock().unwrap();
        conn.execute(sql, rusqlite::params_from_iter(params.iter()))
            .unwrap();
    }

    fn setting_raw(db: &ConfigDb, key: &str) -> Option<String> {
        db.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap()
    }

    fn row_raw(db: &ConfigDb, table: &str, id: &str) -> Option<String> {
        db.conn
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT data FROM {table} WHERE id = ?1"),
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap()
    }

    #[test]
    fn 空库返回_none() {
        let dir = temp_dir("empty");
        let db = ConfigDb::open_at(&dir).unwrap();
        assert!(db.is_empty());
        assert!(db.load().unwrap().is_none());
        fs::remove_dir_all(&dir).ok();
    }

    /// 整份往返:标量设置、项目、SSH 连接、嵌套对象都要一字不差回来。
    #[test]
    fn 整份配置往返() {
        let dir = temp_dir("roundtrip");
        let db = ConfigDb::open_at(&dir).unwrap();

        let mut config = AppConfig {
            ui_font_size: 15.0,
            theme: "dark".into(),
            terminal_font_family: Some("Cascadia Mono".into()),
            download_dir: Some("/tmp/mini-term-downloads".into()),
            hook_enabled: true,
            projects: vec![project("p1", "甲"), project("p2", "乙")],
            ssh_connections: vec![conn("c1")],
            default_shell: "PowerShell".into(),
            available_shells: vec![ShellConfig {
                name: "PowerShell".into(),
                command: "powershell.exe".into(),
                args: None,
            }],
            ..Default::default()
        };

        db.save(&config).unwrap();
        let back = db.load().unwrap().unwrap();

        assert_eq!(back.ui_font_size, 15.0);
        assert_eq!(back.theme, "dark");
        assert_eq!(back.terminal_font_family.as_deref(), Some("Cascadia Mono"));
        assert_eq!(
            back.download_dir.as_deref(),
            Some("/tmp/mini-term-downloads")
        );
        assert!(back.hook_enabled);
        assert_eq!(back.default_shell, "PowerShell");
        assert_eq!(back.available_shells.len(), 1);
        assert_eq!(back.projects.len(), 2);
        assert_eq!(back.projects[0].id, "p1");
        assert_eq!(back.projects[0].name, "甲");
        assert_eq!(back.ssh_connections.len(), 1);
        assert_eq!(back.ssh_connections[0].password.as_deref(), Some("secret"));
        assert!(!db.is_empty());

        // downloadDir 放在旧版本不会清理的 meta；settings 中不留同名键。
        {
            let conn = db.conn.lock().unwrap();
            let settings_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM settings WHERE key = ?1",
                    params![KEY_DOWNLOAD_DIR],
                    |row| row.get(0),
                )
                .unwrap();
            let compatible_value: String = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    params![META_DOWNLOAD_DIR],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(settings_count, 0);
            assert_eq!(compatible_value, "\"/tmp/mini-term-downloads\"");
        }

        // 恢复系统默认会移除 meta 中的兼容值。
        config.download_dir = None;
        db.save(&config).unwrap();
        assert!(db.load().unwrap().unwrap().download_dir.is_none());
        let compatible_count: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM meta WHERE key = ?1",
                params![META_DOWNLOAD_DIR],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(compatible_count, 0);

        fs::remove_dir_all(&dir).ok();
    }

    /// 项目顺序是用户拖出来的,必须原样回来(靠 `ord` 而不是 id 字典序)。
    #[test]
    fn 项目顺序按_ord_还原() {
        let dir = temp_dir("order");
        let db = ConfigDb::open_at(&dir).unwrap();
        let mut config = AppConfig {
            projects: vec![project("zzz", "第一"), project("aaa", "第二")],
            ..Default::default()
        };
        db.save(&config).unwrap();

        let back = db.load().unwrap().unwrap();
        let ids: Vec<&str> = back.projects.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["zzz", "aaa"], "顺序不该被主键的字典序打乱");

        // 调换顺序后再存一次
        config.projects.swap(0, 1);
        db.save(&config).unwrap();
        let back = db.load().unwrap().unwrap();
        let ids: Vec<&str> = back.projects.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["aaa", "zzz"]);

        fs::remove_dir_all(&dir).ok();
    }

    /// 删掉的项目/连接必须从库里消失 —— 整份 save 的语义是「这就是全部」。
    #[test]
    fn 删除的项目与连接不残留() {
        let dir = temp_dir("delete");
        let db = ConfigDb::open_at(&dir).unwrap();
        let mut config = AppConfig {
            projects: vec![project("p1", "甲"), project("p2", "乙")],
            ssh_connections: vec![conn("c1"), conn("c2")],
            ..Default::default()
        };
        db.save(&config).unwrap();

        config.projects.retain(|p| p.id == "p1");
        config.ssh_connections.retain(|c| c.id == "c2");
        db.save(&config).unwrap();

        let back = db.load().unwrap().unwrap();
        assert_eq!(back.projects.len(), 1);
        assert_eq!(back.projects[0].id, "p1");
        assert_eq!(back.ssh_connections.len(), 1);
        assert_eq!(back.ssh_connections[0].id, "c2");

        fs::remove_dir_all(&dir).ok();
    }

    /// 用户把项目全删光 → 表是空的,但库**不是**「空库」——
    /// 否则下次启动会被判成没迁移过,转头从 config.json 把删掉的项目全复活。
    #[test]
    fn 项目删光后仍不是空库() {
        let dir = temp_dir("all-deleted");
        let db = ConfigDb::open_at(&dir).unwrap();
        let mut config = AppConfig {
            projects: vec![project("p1", "甲")],
            ..Default::default()
        };
        db.save(&config).unwrap();

        config.projects.clear();
        db.save(&config).unwrap();

        assert!(!db.is_empty(), "标记在 meta 上,不看表里有没有行");
        assert!(db.load().unwrap().unwrap().projects.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 重开库仍读得到() {
        let dir = temp_dir("reopen");
        {
            let db = ConfigDb::open_at(&dir).unwrap();
            let config = AppConfig {
                projects: vec![project("p1", "甲")],
                ui_font_size: 17.0,
                ..Default::default()
            };
            db.save(&config).unwrap();
        }
        let db = ConfigDb::open_at(&dir).unwrap();
        let back = db.load().unwrap().unwrap();
        assert_eq!(back.projects[0].name, "甲");
        assert_eq!(back.ui_font_size, 17.0);
        fs::remove_dir_all(&dir).ok();
    }

    /// 布局字段是 `skip_serializing` 的,不该出现在配置库里(它们住 layout.db)。
    #[test]
    fn 布局字段不进配置库() {
        let dir = temp_dir("no-layout");
        let db = ConfigDb::open_at(&dir).unwrap();
        let config = AppConfig {
            layout_sizes: Some(vec![20.0, 80.0]),
            right_drawer_width: Some(400.0),
            ..Default::default()
        };
        db.save(&config).unwrap();

        let keys: Vec<String> = {
            let conn = db.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT key FROM settings").unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.flatten().collect()
        };
        for banned in ["layoutSizes", "rightDrawerWidth", "middleColumnVisible"] {
            assert!(!keys.contains(&banned.to_string()), "{banned} 不该进配置库");
        }
        fs::remove_dir_all(&dir).ok();
    }

    /// 备份能顶上:主库删掉后从 .bak 恢复,内容一致。
    #[test]
    fn 备份可用于恢复() {
        let dir = temp_dir("backup");
        let bak = dir.join("config.db.bak");
        {
            let db = ConfigDb::open_at(&dir).unwrap();
            let config = AppConfig {
                projects: vec![project("p1", "甲")],
                ..Default::default()
            };
            db.save(&config).unwrap();
            db.backup_to(&bak).unwrap();
        }
        assert!(bak.exists());

        // 主库写坏 → open 时自动从备份恢复
        fs::write(dir.join("config.db"), b"not a database at all").unwrap();
        let _ = fs::remove_file(dir.join("config.db-wal"));
        let _ = fs::remove_file(dir.join("config.db-shm"));
        let db = ConfigDb::open_at(&dir).unwrap();
        let back = db.load().unwrap().unwrap();
        assert_eq!(back.projects[0].name, "甲");
        assert!(dir.join("config.db.corrupt").exists(), "坏库留证");

        drop(db);
        fs::remove_dir_all(&dir).ok();
    }

    /// 没有备份可用时**必须报错**,绝不静默重建空库 ——
    /// 那等于一次读盘故障就把用户的项目列表和 SSH 连接全清了。
    #[test]
    fn 无备份时损坏必须报错() {
        let dir = temp_dir("no-backup");
        fs::write(dir.join("config.db"), b"not a database at all").unwrap();
        assert!(ConfigDb::open_at(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    // ─── 前向兼容 ───────────────────────────────────────────────

    /// 字段表探针拿到的是 `AppConfig` 的完整字段表:序列化出来的每个键都在,
    /// 只读不写的布局字段也在;下线键一个都不在。`AppConfig` 哪天加了
    /// `#[serde(flatten)]`,探针会拿到空表,这条第一个红。
    #[test]
    fn 已知键覆盖全部序列化字段() {
        let known = known_setting_keys();
        let config = AppConfig {
            ui_font_family: Some("x".into()),
            download_dir: Some("/d".into()),
            tab_title_follows_shell: Some(true),
            mobile_relay: Some(Default::default()),
            ssh_groups: vec!["g".into()],
            ..Default::default()
        };
        let Value::Object(map) = serde_json::to_value(&config).unwrap() else {
            panic!("AppConfig 序列化结果不是对象");
        };
        for key in map.keys() {
            assert!(known.contains(key.as_str()), "{key} 不在已知键里");
        }
        for key in [
            "layoutSizes",
            "middleColumnVisible",
            "rightDrawerWidth",
            "vscodePath",
        ] {
            assert!(known.contains(key), "只读字段 {key} 也得算已知");
        }
        for key in RETIRED_KEYS {
            assert!(!known.contains(key), "下线键 {key} 不该还是已知字段");
        }
    }

    /// 新版本写的 settings 键:旧版本读得进来(忽略)、存得回去(原文一字不动)。
    /// 已知键被清回缺省时照样删 —— 那是用户的「重置」,留着下次会被读回来。
    #[test]
    fn 未知_settings_键原样保留() {
        let dir = temp_dir("unknown-setting");
        let db = ConfigDb::open_at(&dir).unwrap();
        let mut config = AppConfig {
            terminal_font_family: Some("Cascadia Mono".into()),
            ..Default::default()
        };
        db.save(&config).unwrap();
        let future = r#"{"mode":"fancy","level":3}"#;
        put_raw(
            &db,
            "INSERT INTO settings(key, value) VALUES(?1, ?2)",
            &["futureFeature", future],
        );

        let mut back = db.load().unwrap().unwrap();
        back.ui_font_size = 18.0;
        back.terminal_font_family = None;
        db.save(&back).unwrap();

        assert_eq!(setting_raw(&db, "futureFeature").as_deref(), Some(future));
        assert_eq!(setting_raw(&db, "uiFontSize").as_deref(), Some("18.0"));
        assert!(
            setting_raw(&db, "terminalFontFamily").is_none(),
            "已知键清回 None 后必须删掉"
        );

        // 再存一轮也不动它
        config.ui_font_size = 19.0;
        db.save(&config).unwrap();
        assert_eq!(setting_raw(&db, "futureFeature").as_deref(), Some(future));
        fs::remove_dir_all(&dir).ok();
    }

    /// 下线表里的键保存时删掉(`skin` 是配置入库后才下线的,老库里真有)。
    #[test]
    fn 下线键保存时删除() {
        let dir = temp_dir("retired");
        let db = ConfigDb::open_at(&dir).unwrap();
        db.save(&AppConfig::default()).unwrap();
        put_raw(
            &db,
            "INSERT INTO settings(key, value) VALUES(?1, ?2)",
            &["skin", "\"blueprint\""],
        );

        let back = db.load().unwrap().unwrap();
        db.save(&back).unwrap();
        assert!(setting_raw(&db, "skin").is_none(), "下线键必须删掉");
        fs::remove_dir_all(&dir).ok();
    }

    /// 项目 / 连接行里新版本加的字段原样往返,编辑别的字段也不丢;
    /// 连接的下线字段(`proxyJump` 等,只可能来自存量 config.json)入库前剥掉。
    #[test]
    fn 项目与连接的未知字段往返() {
        let dir = temp_dir("unknown-fields");
        let db = ConfigDb::open_at(&dir).unwrap();
        db.save(&AppConfig::default()).unwrap();
        put_raw(
            &db,
            "INSERT INTO projects(id, ord, data) VALUES(?1, 0, ?2)",
            &[
                "p1",
                r#"{"id":"p1","name":"甲","path":"D:/p1","pinned":true,"layoutHints":{"x":1}}"#,
            ],
        );
        put_raw(
            &db,
            "INSERT INTO ssh_connections(id, ord, data) VALUES(?1, 0, ?2)",
            &[
                "c1",
                r#"{"id":"c1","name":"prod","host":"h","port":22,"user":"u","password":"enc:v1:abc","jumpHost":"bastion","proxyJump":"old@b"}"#,
            ],
        );

        let mut back = db.load().unwrap().unwrap();
        assert_eq!(
            back.projects[0].extra.get("pinned"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            back.ssh_connections[0].password.as_deref(),
            Some("enc:v1:abc")
        );
        assert!(
            !back.ssh_connections[0].extra.contains_key("password"),
            "密码只进自己的字段"
        );
        back.projects[0].name = "甲改".into();
        back.ssh_connections[0].port = 2222;
        db.save(&back).unwrap();

        let project: Value =
            serde_json::from_str(&row_raw(&db, "projects", "p1").unwrap()).unwrap();
        assert_eq!(project["name"], "甲改");
        assert_eq!(project["pinned"], true);
        assert_eq!(project["layoutHints"]["x"], 1);

        let raw_conn = row_raw(&db, "ssh_connections", "c1").unwrap();
        assert_eq!(
            raw_conn.matches("\"password\"").count(),
            1,
            "密码键不许重影: {raw_conn}"
        );
        let connection: Value = serde_json::from_str(&raw_conn).unwrap();
        assert_eq!(connection["port"], 2222);
        assert_eq!(connection["jumpHost"], "bastion");
        assert!(
            connection.get("proxyJump").is_none(),
            "下线字段要剥掉: {raw_conn}"
        );

        let again = db.load().unwrap().unwrap();
        assert_eq!(again.projects[0].extra.len(), 2);
        assert_eq!(again.ssh_connections[0].extra.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// 单个键读不懂 → 按默认值顶上、整份照常加载;没改过它的保存**不覆盖**库里的
    /// 原文,用户改了才写新值。必填键(`availableShells`)坏了也起得来。
    #[test]
    fn 单键坏值降级且保存不丢原值() {
        let dir = temp_dir("bad-value");
        let db = ConfigDb::open_at(&dir).unwrap();
        db.save(&AppConfig {
            ui_font_size: 15.0,
            ..Default::default()
        })
        .unwrap();
        let upsert = "INSERT INTO settings(key, value) VALUES(?1, ?2)
                      ON CONFLICT(key) DO UPDATE SET value = excluded.value";
        put_raw(&db, upsert, &["uiFontSize", r#"{"base":15,"scale":1.2}"#]);
        put_raw(&db, upsert, &["availableShells", r#""pwsh-only""#]);
        put_raw(&db, upsert, &["theme", "not json {"]);
        put_raw(&db, upsert, &["mobileRelay", "[1,2]"]);

        let back = db.load().unwrap().expect("单键坏值不许拖垮整库");
        let defaults = AppConfig::default();
        assert_eq!(back.ui_font_size, defaults.ui_font_size);
        assert_eq!(back.available_shells.len(), defaults.available_shells.len());
        assert_eq!(back.theme, defaults.theme);
        assert!(back.mobile_relay.is_none());

        // 调用方都会先过 migrate_config(mobileRelay 缺失会补一份缺省)再存
        let mut migrated = crate::config::migrate_config(back);
        migrated.hook_enabled = true;
        db.save(&migrated).unwrap();
        assert_eq!(
            setting_raw(&db, "uiFontSize").as_deref(),
            Some(r#"{"base":15,"scale":1.2}"#)
        );
        assert_eq!(
            setting_raw(&db, "availableShells").as_deref(),
            Some(r#""pwsh-only""#)
        );
        assert_eq!(setting_raw(&db, "theme").as_deref(), Some("not json {"));
        assert_eq!(setting_raw(&db, "mobileRelay").as_deref(), Some("[1,2]"));
        assert_eq!(setting_raw(&db, "hookEnabled").as_deref(), Some("true"));

        // 用户在本版本里改了其中一项 → 这一项照常写,其余坏值仍留着
        migrated.ui_font_size = 16.0;
        db.save(&migrated).unwrap();
        assert_eq!(setting_raw(&db, "uiFontSize").as_deref(), Some("16.0"));
        assert_eq!(setting_raw(&db, "theme").as_deref(), Some("not json {"));
        // 转正之后再改回默认值,也照写(不再当降级键)
        migrated.ui_font_size = defaults.ui_font_size;
        db.save(&migrated).unwrap();
        assert_eq!(setting_raw(&db, "uiFontSize").as_deref(), Some("13.0"));
        fs::remove_dir_all(&dir).ok();
    }

    /// meta 里 downloadDir 的兼容值坏了同样降级,且保存不抹掉它。
    #[test]
    fn 下载目录兼容值坏了也降级() {
        let dir = temp_dir("bad-download-dir");
        let db = ConfigDb::open_at(&dir).unwrap();
        db.save(&AppConfig::default()).unwrap();
        put_raw(
            &db,
            "INSERT INTO meta(key, value) VALUES(?1, ?2)",
            &[META_DOWNLOAD_DIR, "{broken"],
        );

        let back = db.load().unwrap().unwrap();
        assert!(back.download_dir.is_none());
        db.save(&back).unwrap();
        assert_eq!(db.meta_get(META_DOWNLOAD_DIR).as_deref(), Some("{broken"));

        // 用户选了新目录 → 照写
        let mut next = back;
        next.download_dir = Some("/new".into());
        db.save(&next).unwrap();
        assert_eq!(db.meta_get(META_DOWNLOAD_DIR).as_deref(), Some("\"/new\""));
        fs::remove_dir_all(&dir).ok();
    }

    /// 坏行只跳过它自己,其余行照读;保存时坏行原样留着,好行照常增删。
    #[test]
    fn 坏行不拖垮整库() {
        let dir = temp_dir("bad-row");
        let db = ConfigDb::open_at(&dir).unwrap();
        db.save(&AppConfig {
            projects: vec![project("p1", "甲"), project("p4", "丁")],
            ssh_connections: vec![conn("c1")],
            ..Default::default()
        })
        .unwrap();
        let bad_shape = r#"{"id":"p2","name":{"zh":"乙"},"path":"D:/p2"}"#;
        let bad_json = "{not json";
        let bad_conn = r#"{"id":"c2","name":"x","host":"h","port":"twenty-two","user":"u"}"#;
        put_raw(
            &db,
            "INSERT INTO projects(id, ord, data) VALUES(?1, 1, ?2)",
            &["p2", bad_shape],
        );
        put_raw(
            &db,
            "INSERT INTO projects(id, ord, data) VALUES(?1, 2, ?2)",
            &["p3", bad_json],
        );
        put_raw(
            &db,
            "INSERT INTO ssh_connections(id, ord, data) VALUES(?1, 1, ?2)",
            &["c2", bad_conn],
        );

        let mut back = db.load().unwrap().expect("坏行不许拖垮整库");
        let ids: Vec<&str> = back.projects.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["p1", "p4"]);
        assert_eq!(back.ssh_connections.len(), 1);

        // 删掉一个好项目、加一个新项目再存:好行照常增删,坏行原样留着
        back.projects.retain(|p| p.id != "p4");
        back.projects.push(project("p5", "戊"));
        db.save(&back).unwrap();
        assert!(row_raw(&db, "projects", "p4").is_none(), "好行照常删");
        assert!(row_raw(&db, "projects", "p5").is_some());
        assert_eq!(row_raw(&db, "projects", "p2").as_deref(), Some(bad_shape));
        assert_eq!(row_raw(&db, "projects", "p3").as_deref(), Some(bad_json));
        assert_eq!(
            row_raw(&db, "ssh_connections", "c2").as_deref(),
            Some(bad_conn)
        );

        // 重开库再读一遍,仍然只跳过坏行
        drop(db);
        let db = ConfigDb::open_at(&dir).unwrap();
        let again = db.load().unwrap().unwrap();
        let ids: Vec<&str> = again.projects.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["p1", "p5"]);
        fs::remove_dir_all(&dir).ok();
    }
}
