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

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};

use crate::config::AppConfig;

/// 配置库 schema 版本。
///
/// ⚠️ 与 `usage.db` 相反、与 `layout.db` 相同:**版本不匹配绝不删表重建**。
/// 账本是 JSONL 的派生缓存,重建只是多跑一次 backfill;配置是第一手数据,
/// 重建即用户资产蒸发。普通 settings 键仍按当前强类型 schema 清理；需要跨旧版
/// 保存周期保留的新字段必须单独放进 meta(见 `downloadDir`),避免旧程序整份保存时
/// 把自己不认识的键删掉。
const SCHEMA_VERSION: i64 = 1;

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
const META_DOWNLOAD_DIR: &str = "setting.downloadDir";

/// 摘出去单独入表的两个键(其余进 settings)。
const KEY_PROJECTS: &str = "projects";
const KEY_SSH_CONNECTIONS: &str = "sshConnections";
const KEY_DOWNLOAD_DIR: &str = "downloadDir";

/// `config.db` 的读写口。
pub(crate) struct ConfigDb {
    conn: Mutex<Connection>,
    path: PathBuf,
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
    /// 单个 settings 值解析不出来时**整体失败**而不是跳过:配置不像布局,
    /// 少一个键可能就是「shell 列表没了」,静默降级比报错危险。
    pub fn load(&self) -> Result<Option<AppConfig>> {
        if self.is_empty() {
            return Ok(None);
        }
        let conn = self.conn.lock().map_err(|_| anyhow!("配置库锁中毒"))?;

        let mut map = Map::new();
        {
            let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (key, raw) = row?;
                let value: Value = serde_json::from_str(&raw)
                    .with_context(|| format!("配置项 {key} 的值解析失败"))?;
                map.insert(key, value);
            }
        }
        map.insert(
            KEY_PROJECTS.to_string(),
            Value::Array(read_ordered(&conn, "projects")?),
        );
        map.insert(
            KEY_SSH_CONNECTIONS.to_string(),
            Value::Array(read_ordered(&conn, "ssh_connections")?),
        );
        if let Some(raw) = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![META_DOWNLOAD_DIR],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            let value =
                serde_json::from_str(&raw).context("配置项 downloadDir 的兼容值解析失败")?;
            map.insert(KEY_DOWNLOAD_DIR.to_string(), value);
        }

        let config: AppConfig =
            serde_json::from_value(Value::Object(map)).context("配置库内容不符合当前 schema")?;
        Ok(Some(config))
    }

    /// 整份写回。一个事务里做完:settings 逐键 upsert + 清理消失的键,
    /// projects / sshConnections 逐行 upsert + 删除消失的 id。
    ///
    /// 「整份 API、行级落盘」是刻意的:调用方(`ConfigStore::save`)的契约仍是
    /// 「拿一份完整配置写下去」,不必为每种改动各开一个入口;而磁盘上只有真正
    /// 变了的行被触碰。
    pub fn save(&self, config: &AppConfig) -> Result<()> {
        let (mut settings, projects, connections) = split_config(config)?;
        let download_dir = settings
            .iter()
            .position(|(key, _)| key == KEY_DOWNLOAD_DIR)
            .map(|index| settings.swap_remove(index).1);

        let mut conn = self.conn.lock().map_err(|_| anyhow!("配置库锁中毒"))?;
        let tx = conn.transaction()?;
        {
            let mut put = tx.prepare(
                "INSERT INTO settings(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value
                 WHERE value <> excluded.value",
            )?;
            let mut live: HashSet<String> = HashSet::new();
            for (key, value) in &settings {
                put.execute(params![key, value])?;
                live.insert(key.clone());
            }
            // 字段被删掉(或变成 skip_serializing)后,库里那条残留也要跟着走,
            // 否则下次 load 会把它塞回 AppConfig ——而它可能已经改了语义。
            let stale = stale_keys(&tx, "SELECT key FROM settings", &live)?;
            let mut del = tx.prepare("DELETE FROM settings WHERE key = ?1")?;
            for key in stale {
                del.execute(params![key])?;
            }
        }
        write_ordered(&tx, "projects", &projects)?;
        write_ordered(&tx, "ssh_connections", &connections)?;
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
        tx.execute(
            "INSERT INTO meta(key, value) VALUES(?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![META_INITIALIZED],
        )?;
        tx.commit()?;
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
    let projects = take_rows(&mut map, KEY_PROJECTS)?;
    let connections = take_rows(&mut map, KEY_SSH_CONNECTIONS)?;

    let mut settings = Vec::with_capacity(map.len());
    for (key, value) in map {
        settings.push((key, serde_json::to_string(&value)?));
    }
    Ok((settings, projects, connections))
}

/// 从 Map 里摘出一个数组字段,变成 (id, 整条 JSON) 的行。
///
/// 缺 `id` 的元素直接报错而不是跳过:那意味着 schema 出了问题,静默丢一条
/// 等于用户少一个项目。
fn take_rows(map: &mut Map<String, Value>, key: &str) -> Result<Rows> {
    let Some(Value::Array(items)) = map.remove(key) else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{key} 里有一条记录缺 id"))?
            .to_string();
        rows.push((id, serde_json::to_string(&item)?));
    }
    Ok(rows)
}

/// 按 `ord` 读回一张「一行一条」的表。
fn read_ordered(conn: &Connection, table: &str) -> Result<Vec<Value>> {
    let sql = format!("SELECT data FROM {table} ORDER BY ord");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let raw = row?;
        out.push(serde_json::from_str(&raw).with_context(|| format!("{table} 的一行解析失败"))?);
    }
    Ok(out)
}

/// 写一张「一行一条」的表:逐行 upsert(内容与顺序都没变的行不触碰)+ 删除消失的 id。
fn write_ordered(tx: &rusqlite::Transaction<'_>, table: &str, rows: &Rows) -> Result<()> {
    let sql = format!(
        "INSERT INTO {table}(id, ord, data) VALUES(?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET ord = excluded.ord, data = excluded.data
         WHERE ord <> excluded.ord OR data <> excluded.data"
    );
    let mut put = tx.prepare(&sql)?;
    let mut live: HashSet<String> = HashSet::new();
    for (index, (id, data)) in rows.iter().enumerate() {
        put.execute(params![id, index as i64, data])?;
        live.insert(id.clone());
    }
    let stale = stale_keys(tx, &format!("SELECT id FROM {table}"), &live)?;
    let mut del = tx.prepare(&format!("DELETE FROM {table} WHERE id = ?1"))?;
    for id in stale {
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
    use crate::config::{ProjectConfig, ShellConfig};
    use mt_core::SshConnection;

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
        ProjectConfig {
            id: id.into(),
            name: name.into(),
            path: format!("D:/{id}"),
            description: None,
            saved_layout: None,
            expanded_dirs: vec![],
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: vec![],
            wsl_sessions_distro: None,
            ssh_connection_id: None,
            parent_project_id: None,
            kind_override: None,
        }
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
        }
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
}
