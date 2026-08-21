//! mt-core —— mini-term 的共享核心库。
//!
//! 这里放**不依赖 tauri** 的纯逻辑,供 mini-term 主程序(`tauri_app_lib`)
//! 与独立的 sidecar 二进制(如 SSH MCP server)共用。
//!
//! 关键约束:本 crate 绝不能依赖 `tauri`,以便 sidecar 不必链接整个 Tauri。
//!
//! # 收尾-1 批(GPUI 迁移)之后的定位
//!
//! 本 crate 已从 `src-tauri/mt-core` 物理移入 `crates/mt-core`,同时是:
//! - GPUI 工作区(仓库根 `Cargo.toml`)的成员 —— `mt-pty` / `mt-ai` / `mt-relay` /
//!   `mt-config` / `mt-project` 走 `mt-core.workspace = true` 引用;
//! - 旧 Tauri 侧 `src-tauri` 与 `src-tauri/mt-sidecars` 两个工作区的跨工作区
//!   path 依赖(与 `relay-server/protocol` 同一种用法)。
//!
//! **依赖方向铁律**:本 crate 是依赖图的**叶子**,依赖表只许有 serde / serde_json /
//! dirs 这一级的东西。它被 miniterm-hook / mt-ssh-mcp / mt-ssh-cli 三个独立小二进制
//! 直接链接,任何上层 crate(mt-config、mt-project……)都**不能**出现在它的依赖里。
//! 这条铁律决定了 `SshConnection` 的归属方向:定义留在本 crate,由 `mt-config`
//! 反向 `pub use` 过去(config.json 仍是它的持久化归属方,回归测试也留在那边)。

mod atomic_file;
mod config_reader;
mod ssh_connection;
mod ssh_key;
mod ssh_prompt;
mod tui_line;
mod wsl_path;

pub use atomic_file::atomic_write;
pub use config_reader::{
    config_json_path, read_ssh_connections_for_project, read_ssh_connections_for_project_at,
    read_ssh_connections_for_token, read_ssh_connections_for_token_at,
};
pub use ssh_connection::SshConnection;
pub use ssh_key::{cleanup_ssh_temp_keys, prepare_ssh_key, restrict_permissions, temp_keys_dir};
pub use ssh_prompt::{scan_ssh_prompt, strip_ansi_codes, SshPromptScan};
pub use tui_line::strip_tui_decoration;
pub use wsl_path::{parse_unc as parse_wsl_unc, WslPath};
