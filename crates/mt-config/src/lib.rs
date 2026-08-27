//! 配置持久化与主题包。**不依赖 gpui,也不依赖 tauri**。
//!
//! 从 `src-tauri/src/config.rs`(1298 行)与 `src-tauri/src/theme_packs.rs`(429 行)
//! 移入。既有字段与 camelCase 名保持兼容；新增偏好使用可选字段，旧版配置仍可
//! 原样读入。
//!
//! # 三个入口
//!
//! | 类型 | 职责 |
//! |---|---|
//! | [`ConfigStore`] | `config.json` 的读/写 + 写盘令牌(乐观并发) |
//! | [`ThemePacks`](theme_packs::ThemePacks) | `{app_data_dir}/themes` 的列举 / 导入 / 删除 / 资源读取 |
//! | [`paths`] | app data 目录定位与历史 identifier 迁移 |
//!
//! ```no_run
//! let store = mt_config::ConfigStore::open()?;   // 顺带跑 identifier 迁移
//! let loaded = store.load()?;                     // 拿到配置 + 本次令牌
//! store.save(loaded.token, &loaded.config)?;      // 令牌过期则拒写,调用方须重读
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! # 去 Tauri 化改了什么
//!
//! - 路径不再走 `AppHandle::path().app_data_dir()`,改由 [`paths`] 用 `dirs` 自己拼。
//!   Tauri v2 的实现就是 `dirs::data_dir()?.join(identifier)`,磁盘位置一致,
//!   论证与实测见 [`paths`] 模块文档。
//! - `#[tauri::command] load_config / save_config` 去掉宏,成为 [`ConfigStore`] 的方法。
//! - Tauri managed state `ConfigToken(AtomicU64)` 变成 [`ConfigStore`] 自己的字段,
//!   语义逐字不变:两处同时改配置时,后写者的令牌已过期,必须重读再写。
//! - 主题包的 `read_theme_asset` 原本返回 base64(WebView asset 协议的兜底通道),
//!   现在直接给 `Vec<u8>` —— 单进程里没有跨边界传输。
//!
//! 这两个源文件里没有 `emit` 事件出口,所以"事件改回调"这条对本 crate 无适用项;
//! 唯一的外发信号是写盘令牌,已经是返回值。

pub mod paths;
pub mod theme_packs;

mod config;
mod db;

pub use config::{
    AiLauncher, AppConfig, ConfigStore, EditorConfig, LoadedConfig, MobileRelayConfig,
    OldProjectGroup, ProjectConfig, ProjectEnvVar, ProjectGroup, ProjectTreeItem, SaveError,
    SavedAiSession, SavedLineageEdge, SavedPane, SavedProjectLayout, SavedSplitNode, SavedTab,
    ShellConfig, SshConnection, default_remote_paste_dir, migrate_config,
    normalize_saved_layout, read_config_from,
};
pub use paths::{
    APP_IDENTIFIER, DATA_DIR_ENV, LEGACY_IDENTIFIER, active_data_dir, app_data_dir, config_path,
    migrate_legacy_app_data, themes_dir,
};
pub use theme_packs::{ThemePackData, ThemePackEntry, ThemePacks};
