//! 界面布局持久化(rusqlite,存 `{active_data_dir}/layout.db`)。
//!
//! # 为什么从 config.json 里搬出来
//!
//! 布局是**交互频次**的数据(拖分隔条 / 开关终端 / 分屏 / 拖窗口),配置是月级的
//! (SSH 连接、shell 列表、各种开关)。此前两者共用 `config.json` 一个信封:改一次
//! 布局要把整份配置 `to_string_pretty` 重写一遍,还要先 `copy` 一份同样大小的
//! `.bak` —— 实测本机 config.json 62.6 KB,即拖一次分隔条约 125 KB 落盘,而真正
//! 变的只有几个 f64。搬进 SQLite 后一次布局变更是**一行 upsert**。
//!
//! 顺带把损坏半径切开:config.json 写坏会连项目列表与 SSH 连接一起赔进去,
//! 布局库炸了只丢布局(下次启动回到默认分屏,项目一个不少)。
//!
//! # 为什么树仍存 JSON,不拆关系表
//!
//! `SavedProjectLayout` 那棵分屏树**永远整读整写**,没有任何按节点查询的需求。
//! 拆成 `nodes(id, parent_id, ...)` 递归表只会把一次 upsert 变成 N 行事务,外加
//! 自己维护递归完整性与孤儿清理。SQLite 在这里的价值是「更好的写入信封」,
//! 不是「换数据模型」—— 磁盘上那段 JSON 与旧 config.json 里的 `savedLayout`
//! **逐字段一致**(同一个 serde 定义),`mt-app` 的 `persist.rs` 一行没动。
//!
//! # 与 usage.db 的两处刻意不同
//!
//! - **不复用同一个库**:`usage.db` 二十多 MB,且 [`mt_usage`] 的策略是 schema
//!   版本不匹配即**删表重建**(账本可从 JSONL 再生,所以无所谓)。布局**不可再生**,
//!   混进去等于给它装了个自毁开关。
//! - **版本不匹配不重建**:见 [`SCHEMA_VERSION`] 的注释。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use mt_config::{AppConfig, SavedProjectLayout};

/// 布局库 schema 版本。
///
/// ⚠️ 与 `usage.db` 相反:**版本不匹配绝不删表重建**。账本是 JSONL 的派生缓存,
/// 重建只是多跑一次 backfill;布局是第一手数据,重建即用户资产蒸发。
/// 加字段一律走 `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ADD COLUMN` 的加法路线,
/// 读到**更高**的版本号(用户装过新版又降级回来)也照常读写 —— kv 表天生向前兼容,
/// 不认识的 key 原样留着,新版装回去还在。
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS app_layout (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS project_layout (
  project_id    TEXT PRIMARY KEY,
  layout_json   TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
";

/// `meta` 表里记「已从 config.json 灌过一次」的键。存在即不再迁移 ——
/// 否则用户清空布局后重启,又会被旧 config.json 里的残留复活。
const META_MIGRATED: &str = "config_migrated";
const META_SCHEMA_VERSION: &str = "schema_version";

// `app_layout` 的键名。与 config.json 里的 camelCase 键同名,便于对着旧文件排查。
const KEY_LAYOUT_SIZES: &str = "layoutSizes";
const KEY_MIDDLE_COLUMN_SIZES: &str = "middleColumnSizes";
const KEY_MIDDLE_COLUMN_VISIBLE: &str = "middleColumnVisible";
const KEY_RIGHT_DRAWER_WIDTH: &str = "rightDrawerWidth";
const KEY_WINDOW: &str = "window";
// GPUI 版新增的键(旧 config.json 里没有对应物),沿用 camelCase 命名口径。
const KEY_TERMINALS_PANEL_VISIBLE: &str = "terminalsPanelVisible";

/// 窗口的开合状态。gpui 的 `WindowBounds` 三个变体的镜像 —— 那个类型不实现
/// serde,而本 crate 刻意**不依赖 gpui**(布局存储不该把整个 GPU 栈拖进来,
/// 也才好脱离窗口跑单测),故在这里复刻一份最小形状,转换住在 `mt-app` 侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowMode {
    Windowed,
    Maximized,
    Fullscreen,
}

/// 窗口几何。`x/y/width/height` 恒为**还原尺寸**(最大化/全屏时也存还原后的框),
/// 与 gpui `WindowBounds` 各变体内附的 bounds 同一语义:退出时最大化,下次启动
/// 直接最大化打开,而用户按「还原」时拿到的仍是最后一次窗口态的大小。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowGeometry {
    pub mode: WindowMode,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl WindowGeometry {
    /// 明显不可用的几何(尺寸为 0/负数/NaN,或小得放不下任何内容)直接判废,
    /// 由调用方回落默认居中窗口。**不校验是否落在某块屏幕内** —— 那要问平台
    /// 拿显示器列表,是 `mt-app` 的活(见 `main.rs` 的 `restore_window_bounds`)。
    pub fn is_sane(&self) -> bool {
        const MIN_SIDE: f64 = 200.0;
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.width >= MIN_SIDE
            && self.height >= MIN_SIDE
    }
}

/// 全局(非项目级)布局项。每一项都是 `Option`:`None` = 库里没有这个键,
/// 由调用方沿用自己的默认值,**不是**「用户显式设成了默认值」。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalLayout {
    /// 三栏比例(左 / 中 / 右)。
    pub layout_sizes: Option<Vec<f64>>,
    /// 中栏内部(文件树 / 会话列表)的比例。
    pub middle_column_sizes: Option<Vec<f64>>,
    pub middle_column_visible: Option<bool>,
    pub right_drawer_width: Option<f64>,
    /// 终端区右缘「终端列表」竖条面板的显隐(GPUI 版新增,无旧 config 对应物)。
    pub terminals_panel_visible: Option<bool>,
    pub window: Option<WindowGeometry>,
}

impl GlobalLayout {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// `layout.db` 的读写口。
///
/// 持有一条常开连接(WAL + 5s busy_timeout)。写者只有 UI 线程一个,读也只在启动
/// 时发生一次,`Mutex<Connection>` 足够;没有 `mt-usage` 那套同步合并的必要。
pub struct LayoutStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl LayoutStore {
    /// `{dir}/layout.db`。目录不存在就建出来。
    ///
    /// 打不开(损坏 / 版本过旧的 SQLite 格式)时把旧文件挪成 `layout.db.corrupt`
    /// 留证再建一个空的:布局丢了只是回到默认分屏,而让持久化整个停摆意味着
    /// 用户此后每次退出的调整都白做。挪不动(权限 / 被占用)才向上抛。
    pub fn open_at(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("创建应用数据目录失败: {}", dir.display()))?;
        let path = dir.join("layout.db");
        match Self::try_open(&path) {
            Ok(store) => Ok(store),
            Err(first) => {
                let corrupt = path.with_extension("db.corrupt");
                let _ = fs::remove_file(&corrupt);
                if fs::rename(&path, &corrupt).is_err() {
                    return Err(first);
                }
                eprintln!(
                    "[layout] {} 打不开({first:#}),已挪至 {} 并重建空库",
                    path.display(),
                    corrupt.display()
                );
                Self::try_open(&path)
            }
        }
    }

    fn try_open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("打开布局库失败: {}", path.display()))?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        // journal_mode 是有返回行的语句,得走 query_row。转不过去(比如另一实例
        // 正握着这个库)不算失败:退回默认的 delete 模式照样能读写,只是少了
        // 读写不互阻的好处。
        let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0));
        // 默认阈值是 1000 页(约 4 MB)——对一个稳态只有几 KB 的库来说,意味着
        // WAL 能长到主库的上千倍才回收一次。实测首启迁移 44 个项目后 WAL 就有
        // 450 KB 而主库 4 KB;进程被强杀时这个 WAL 会一直躺在数据目录里。
        // 32 页(约 128 KB)对布局这种小步快写的负载足够摊薄 fsync。
        let _ = conn.execute_batch("PRAGMA wal_autocheckpoint=32");
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("建表失败: {}", path.display()))?;

        let store = Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        };
        store.check_schema_version();
        Ok(store)
    }

    /// 版本只记录、不裁决(见 [`SCHEMA_VERSION`])。读到更高的版本只打一行日志,
    /// 并且**不把它降回去** —— 用户切回新版时不该发现自己的库被降级标记过。
    fn check_schema_version(&self) {
        let found = self
            .meta_get(META_SCHEMA_VERSION)
            .and_then(|v| v.parse::<i64>().ok());
        match found {
            Some(v) if v > SCHEMA_VERSION => {
                eprintln!("[layout] 库版本 {v} 高于本程序的 {SCHEMA_VERSION},按兼容模式读写");
            }
            Some(v) if v == SCHEMA_VERSION => {}
            _ => {
                self.meta_set(META_SCHEMA_VERSION, &SCHEMA_VERSION.to_string());
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // ─── meta ────────────────────────────────────────────────────────────

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

    /// 是否还没从 config.json 灌过。首启(空库)返回 true。
    pub fn needs_config_migration(&self) -> bool {
        self.meta_get(META_MIGRATED).is_none()
    }

    // ─── 全局布局项 ──────────────────────────────────────────────────────

    /// 读全部全局项。库里没有 / 值解析不出来的键一律当 `None`
    /// ——手改坏一个键不该让整份布局读不出来。
    pub fn load_globals(&self) -> GlobalLayout {
        let Ok(conn) = self.conn.lock() else {
            return GlobalLayout::default();
        };
        let mut map: HashMap<String, String> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare("SELECT key, value FROM app_layout") {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                for (key, value) in rows.flatten() {
                    map.insert(key, value);
                }
            }
        }
        GlobalLayout {
            layout_sizes: from_kv(&map, KEY_LAYOUT_SIZES),
            middle_column_sizes: from_kv(&map, KEY_MIDDLE_COLUMN_SIZES),
            middle_column_visible: from_kv(&map, KEY_MIDDLE_COLUMN_VISIBLE),
            right_drawer_width: from_kv(&map, KEY_RIGHT_DRAWER_WIDTH),
            terminals_panel_visible: from_kv(&map, KEY_TERMINALS_PANEL_VISIBLE),
            window: from_kv(&map, KEY_WINDOW),
        }
    }

    /// 整体写回全局项。`None` 的字段**保持库里原样**(不删除)——
    /// 调用方通常只改了其中一项,不该因为没填其余字段就把它们抹掉。
    pub fn save_globals(&self, globals: &GlobalLayout) -> Result<()> {
        let mut conn = self.conn.lock().map_err(|_| anyhow::anyhow!("布局库锁中毒"))?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO app_layout(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )?;
            let mut put = |key: &str, value: Option<String>| -> Result<()> {
                if let Some(v) = value {
                    stmt.execute(params![key, v])?;
                }
                Ok(())
            };
            put(KEY_LAYOUT_SIZES, to_json(&globals.layout_sizes))?;
            put(KEY_MIDDLE_COLUMN_SIZES, to_json(&globals.middle_column_sizes))?;
            put(
                KEY_MIDDLE_COLUMN_VISIBLE,
                to_json(&globals.middle_column_visible),
            )?;
            put(KEY_RIGHT_DRAWER_WIDTH, to_json(&globals.right_drawer_width))?;
            put(
                KEY_TERMINALS_PANEL_VISIBLE,
                to_json(&globals.terminals_panel_visible),
            )?;
            put(KEY_WINDOW, to_json(&globals.window))?;
        }
        tx.commit()?;
        Ok(())
    }

    // ─── 项目级分屏树 ────────────────────────────────────────────────────

    /// 读全部项目布局。某一行的 JSON 解析失败只丢那一个项目,其余照常返回。
    pub fn load_project_layouts(&self) -> HashMap<String, SavedProjectLayout> {
        let mut out = HashMap::new();
        let Ok(conn) = self.conn.lock() else {
            return out;
        };
        let Ok(mut stmt) = conn.prepare("SELECT project_id, layout_json FROM project_layout")
        else {
            return out;
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) else {
            return out;
        };
        for (project_id, json) in rows.flatten() {
            match serde_json::from_str::<SavedProjectLayout>(&json) {
                Ok(mut layout) => {
                    mt_config::normalize_saved_layout(&mut layout);
                    out.insert(project_id, layout);
                }
                Err(e) => eprintln!("[layout] 项目 {project_id} 的布局解析失败,已跳过: {e}"),
            }
        }
        out
    }

    /// 写一个项目的布局。空布局(一个 pane 都没有)按**删行**处理 ——
    /// 项目关光终端后重启不该又冒出一个空壳。
    pub fn save_project_layout(
        &self,
        project_id: &str,
        layout: &SavedProjectLayout,
        now_ms: i64,
    ) -> Result<()> {
        if layout.tabs.is_empty() {
            return self.delete_project_layout(project_id);
        }
        let json = serde_json::to_string(layout)?;
        let conn = self.conn.lock().map_err(|_| anyhow::anyhow!("布局库锁中毒"))?;
        conn.execute(
            "INSERT INTO project_layout(project_id, layout_json, updated_at_ms) VALUES(?1, ?2, ?3)
             ON CONFLICT(project_id) DO UPDATE SET
               layout_json = excluded.layout_json,
               updated_at_ms = excluded.updated_at_ms",
            params![project_id, json, now_ms],
        )?;
        Ok(())
    }

    pub fn delete_project_layout(&self, project_id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow::anyhow!("布局库锁中毒"))?;
        conn.execute(
            "DELETE FROM project_layout WHERE project_id = ?1",
            params![project_id],
        )?;
        Ok(())
    }

    /// 清掉不在 `live` 里的项目行。项目删除走的是配置那条路径,布局库这边靠
    /// 启动时对一次账收口 —— 单个删除漏调也不会攒出无主行。
    pub fn retain_projects(&self, live: &HashSet<String>) -> Result<()> {
        let stale: Vec<String> = {
            let conn = self.conn.lock().map_err(|_| anyhow::anyhow!("布局库锁中毒"))?;
            let mut stmt = conn.prepare("SELECT project_id FROM project_layout")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.flatten().filter(|id| !live.contains(id)).collect()
        };
        for id in stale {
            self.delete_project_layout(&id)?;
        }
        Ok(())
    }

    // ─── 从 config.json 一次性迁移 ───────────────────────────────────────

    /// 把存量 `config.json` 里的布局灌进本库,并打上「已迁移」标记。
    ///
    /// 只在 [`needs_config_migration`](Self::needs_config_migration) 为真时调用。
    /// 幂等靠 meta 标记而不是「库里有没有数据」:用户把所有终端关光后重启,
    /// 库是空的但迁移确实做过,按后者判会把旧布局又灌回来。
    ///
    /// 返回灌进去的项目数。
    pub fn migrate_from_config(&self, config: &AppConfig) -> Result<usize> {
        let globals = GlobalLayout {
            layout_sizes: config.layout_sizes.clone(),
            middle_column_sizes: config.middle_column_sizes.clone(),
            // 这个字段在 config 里是裸 bool(默认 true),分不出「用户设过 true」
            // 与「从来没设过」。一律搬:值本身就是当前生效的那个,搬过来语义不变。
            middle_column_visible: Some(config.middle_column_visible),
            right_drawer_width: config.right_drawer_width,
            // 终端列表竖条与窗口几何都是 GPUI 版新加的能力,旧 config.json 里
            // 没有对应物(窗口几何在 Tauri 版存在另一个文件 `.window-state.json`,
            // 格式不兼容,不迁)
            terminals_panel_visible: None,
            window: None,
        };
        if !globals.is_empty() {
            self.save_globals(&globals)?;
        }

        let mut count = 0usize;
        for project in &config.projects {
            let Some(layout) = project.saved_layout.as_ref() else {
                continue;
            };
            if layout.tabs.is_empty() {
                continue;
            }
            self.save_project_layout(&project.id, layout, 0)?;
            count += 1;
        }
        self.meta_set(META_MIGRATED, "1");
        Ok(count)
    }
}

fn to_json<T: Serialize>(value: &Option<T>) -> Option<String> {
    value.as_ref().and_then(|v| serde_json::to_string(v).ok())
}

/// kv 表里取一个键并解析。缺键 / 解析失败一律 `None` —— 手改坏一个键不该让
/// 整份布局读不出来(闭包做不到泛型,所以是个自由函数)。
fn from_kv<T: for<'de> Deserialize<'de>>(map: &HashMap<String, String>, key: &str) -> Option<T> {
    map.get(key).and_then(|v| serde_json::from_str(v).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_config::{ProjectConfig, SavedPane, SavedSplitNode, SavedTab};

    fn temp_dir(label: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mt-layout-test-{label}-{ts}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn layout(shell: &str) -> SavedProjectLayout {
        SavedProjectLayout {
            tabs: vec![SavedTab {
                custom_title: None,
                split_layout: SavedSplitNode::Split {
                    direction: "vertical".into(),
                    sizes: vec![30.0, 70.0],
                    children: vec![
                        SavedSplitNode::Leaf {
                            pane: None,
                            panes: vec![SavedPane {
                                shell_name: shell.into(),
                                cwd: None,
                                ai_session: None,
                            }],
                        },
                        SavedSplitNode::Leaf {
                            pane: None,
                            panes: vec![SavedPane {
                                shell_name: shell.into(),
                                cwd: Some("D:/x".into()),
                                ai_session: None,
                            }],
                        },
                    ],
                },
            }],
            active_tab_index: 0,
        }
    }

    #[test]
    fn 分屏树往返() {
        let dir = temp_dir("roundtrip");
        let store = LayoutStore::open_at(&dir).unwrap();
        store.save_project_layout("p1", &layout("cmd"), 42).unwrap();

        let back = store.load_project_layouts();
        let got = back.get("p1").unwrap();
        assert_eq!(got.tabs.len(), 1);
        let SavedSplitNode::Split { sizes, children, .. } = &got.tabs[0].split_layout else {
            panic!("应还原成 split");
        };
        assert_eq!(sizes, &vec![30.0, 70.0]);
        assert_eq!(children.len(), 2);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 重开库仍读得到() {
        let dir = temp_dir("reopen");
        {
            let store = LayoutStore::open_at(&dir).unwrap();
            store.save_project_layout("p1", &layout("cmd"), 1).unwrap();
        }
        let store = LayoutStore::open_at(&dir).unwrap();
        assert!(store.load_project_layouts().contains_key("p1"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 全局项部分更新不抹掉其它键() {
        let dir = temp_dir("globals-partial");
        let store = LayoutStore::open_at(&dir).unwrap();
        store
            .save_globals(&GlobalLayout {
                layout_sizes: Some(vec![20.0, 60.0, 20.0]),
                right_drawer_width: Some(360.0),
                ..Default::default()
            })
            .unwrap();
        // 只改三栏比例,其余字段留 None
        store
            .save_globals(&GlobalLayout {
                layout_sizes: Some(vec![25.0, 55.0, 20.0]),
                ..Default::default()
            })
            .unwrap();

        let got = store.load_globals();
        assert_eq!(got.layout_sizes, Some(vec![25.0, 55.0, 20.0]));
        assert_eq!(got.right_drawer_width, Some(360.0), "没填的字段不该被抹掉");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 窗口几何往返() {
        let dir = temp_dir("window");
        let store = LayoutStore::open_at(&dir).unwrap();
        let geo = WindowGeometry {
            mode: WindowMode::Maximized,
            x: 100.0,
            y: 50.0,
            width: 1440.0,
            height: 900.0,
        };
        store
            .save_globals(&GlobalLayout {
                window: Some(geo),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(store.load_globals().window, Some(geo));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 离谱的窗口几何判废() {
        let ok = WindowGeometry {
            mode: WindowMode::Windowed,
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: 800.0,
        };
        assert!(ok.is_sane());
        assert!(!WindowGeometry { width: 0.0, ..ok }.is_sane());
        assert!(!WindowGeometry { height: -5.0, ..ok }.is_sane());
        assert!(!WindowGeometry { x: f64::NAN, ..ok }.is_sane());
        assert!(!WindowGeometry { width: 10.0, ..ok }.is_sane(), "小得放不下内容");
    }

    #[test]
    fn 空布局按删行处理() {
        let dir = temp_dir("empty");
        let store = LayoutStore::open_at(&dir).unwrap();
        store.save_project_layout("p1", &layout("cmd"), 1).unwrap();
        store
            .save_project_layout(
                "p1",
                &SavedProjectLayout {
                    tabs: vec![],
                    active_tab_index: 0,
                },
                2,
            )
            .unwrap();
        assert!(!store.load_project_layouts().contains_key("p1"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 清理无主项目行() {
        let dir = temp_dir("retain");
        let store = LayoutStore::open_at(&dir).unwrap();
        store.save_project_layout("p1", &layout("cmd"), 1).unwrap();
        store.save_project_layout("p2", &layout("cmd"), 1).unwrap();

        let live: HashSet<String> = ["p1".to_string()].into_iter().collect();
        store.retain_projects(&live).unwrap();

        let back = store.load_project_layouts();
        assert!(back.contains_key("p1"));
        assert!(!back.contains_key("p2"));
        fs::remove_dir_all(&dir).ok();
    }

    /// 迁移:config.json 的布局灌进库,且**只灌一次** —— 用户关光终端后重启,
    /// 不该被旧 config 里的残留复活。
    #[test]
    fn 从配置迁移一次且只迁一次() {
        let dir = temp_dir("migrate");
        let store = LayoutStore::open_at(&dir).unwrap();
        assert!(store.needs_config_migration(), "空库该要迁移");

        let mut config = AppConfig::default();
        config.layout_sizes = Some(vec![20.0, 60.0, 20.0]);
        config.right_drawer_width = Some(400.0);
        config.projects.push(ProjectConfig {
            id: "p1".into(),
            name: "proj".into(),
            path: "D:/proj".into(),
            saved_layout: Some(layout("cmd")),
            ..project_stub()
        });

        let n = store.migrate_from_config(&config).unwrap();
        assert_eq!(n, 1);
        assert!(!store.needs_config_migration(), "迁移后不该再迁");
        assert_eq!(store.load_globals().layout_sizes, Some(vec![20.0, 60.0, 20.0]));
        assert_eq!(store.load_globals().right_drawer_width, Some(400.0));
        assert!(store.load_project_layouts().contains_key("p1"));

        // 用户把终端关光 → 库里删了行,但标记还在,重启不该被 config 复活
        store.delete_project_layout("p1").unwrap();
        let store = LayoutStore::open_at(&dir).unwrap();
        assert!(!store.needs_config_migration());
        assert!(!store.load_project_layouts().contains_key("p1"));

        fs::remove_dir_all(&dir).ok();
    }

    /// 旧格式的 `pane`(单数)在迁移后仍读得出来 —— 迁移是逐字节搬 JSON,
    /// 归一化在读出来那一刻做(与 `migrate_config` 同一口径)。
    #[test]
    fn 旧格式单_pane_读出时归一化() {
        let dir = temp_dir("legacy-pane");
        let store = LayoutStore::open_at(&dir).unwrap();
        // 直接写一段旧格式 JSON:`pane`(单数)是 `skip_serializing` 的,走
        // `save_project_layout` 反而写不出这种形状 —— 这里模拟的是存量库/迁移
        // 时从 config.json 原样搬过来的那份数据。
        {
            let conn = store.conn.lock().unwrap();
            let json = r#"{"tabs":[{"splitLayout":{"type":"leaf","pane":{"shellName":"cmd","cwd":"D:/x"}}}],"activeTabIndex":0}"#;
            conn.execute(
                "INSERT INTO project_layout(project_id, layout_json, updated_at_ms) VALUES('p1', ?1, 0)",
                params![json],
            )
            .unwrap();
        }

        let back = store.load_project_layouts();
        let got = back.get("p1").unwrap();
        let SavedSplitNode::Leaf { pane, panes } = &got.tabs[0].split_layout else {
            panic!("应是 leaf");
        };
        assert!(pane.is_none(), "旧字段读完即清");
        assert_eq!(panes.len(), 1, "应归一化进 panes");
        assert_eq!(panes[0].shell_name, "cmd");

        fs::remove_dir_all(&dir).ok();
    }

    /// 损坏的库不该让持久化整个停摆:挪走留证 + 重建空库,程序照常起来。
    #[test]
    fn 损坏的库挪走并重建() {
        let dir = temp_dir("corrupt");
        fs::write(dir.join("layout.db"), b"this is definitely not a sqlite file").unwrap();
        let store = LayoutStore::open_at(&dir).unwrap();
        store.save_project_layout("p1", &layout("cmd"), 1).unwrap();
        assert!(store.load_project_layouts().contains_key("p1"));
        assert!(dir.join("layout.db.corrupt").exists(), "旧文件留证");
        fs::remove_dir_all(&dir).ok();
    }

    fn project_stub() -> ProjectConfig {
        ProjectConfig {
            id: String::new(),
            name: String::new(),
            path: String::new(),
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
}
