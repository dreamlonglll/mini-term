//! 配置持久化与主题包。
//!
//! # 待移入
//!
//! | 来源 | 说明 |
//! |---|---|
//! | `src-tauri/src/config.rs` (1298 行) | `AppConfig` 与 `{app_data_dir}/config.json` 读写、跨平台预置 shell 列表、`migrate_legacy_app_data` |
//! | `src-tauri/src/theme_packs.rs` (429 行) | 主题包的列举/导入/删除/资源读取 |
//!
//! # 移植时要改的
//!
//! - `app_data_dir` 原本从 `tauri::AppHandle` 取,改成 `dirs::data_dir()` 自己拼
//!   (identifier 曾从 `com.tauri-app.tauri-app` 迁到 `com.mini-term.app`,
//!   `migrate_legacy_app_data` 的兼容分支要原样保留)。
//! - `#[tauri::command] load_config / save_config` 去掉宏,变普通函数。
//! - **`ConfigToken`** 这个 Tauri managed state 的乐观并发计数在 GPUI 下改成
//!   放进全局 App state,语义不变(前端两处同时改配置时后写者必须重读)。
//!
//! # 与 gpui-component 主题层的关系
//!
//! `theme_packs` 里「配色」那一半可以直接映射到 `gpui_component::theme` 的
//! JSON schema + `registry` 运行时切换,不必自己再造一套 token 系统;
//! 「背景图 / 字体 / 终端配色」那一半是 mini-term 特有的,留在本 crate。
//! 决定映射边界前先读 `gpui-component/crates/ui/src/theme/schema.rs`。
