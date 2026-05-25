//! mt-core —— mini-term 的共享核心库。
//!
//! 这里放**不依赖 tauri** 的纯逻辑,供 mini-term 主程序(`tauri_app_lib`)
//! 与独立的 sidecar 二进制(如 SSH MCP server)共用。
//!
//! 关键约束:本 crate 绝不能依赖 `tauri`,以便 sidecar 不必链接整个 Tauri。

mod config_reader;
mod ssh_connection;
mod ssh_key;
mod ssh_prompt;
mod wsl_path;

pub use config_reader::{config_json_path, read_ssh_connections_for_project};
pub use ssh_connection::SshConnection;
pub use ssh_key::{cleanup_ssh_temp_keys, prepare_ssh_key, restrict_permissions, temp_keys_dir};
pub use ssh_prompt::{scan_ssh_prompt, strip_ansi_codes, SshPromptScan};
pub use wsl_path::{parse_unc as parse_wsl_unc, WslPath};
