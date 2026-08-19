//! mt-ssh —— mini-term 的共享 SSH 通信层(russh 持久会话池 + SFTP 传输原语)。
//!
//! 自 `mt-sidecars/src/pool.rs` 抽出(task 07-05-ssh-remote-projects PR1):
//! 主程序(src-tauri)与 sidecar(mt-ssh-mcp)都需要按 `mt_core::SshConnection`
//! 建立/复用 SSH session、exec、开 SFTP,两边以路径依赖共用本 crate、各持一个
//! 池实例。本 crate **不依赖 tauri / rmcp**;MCP 胶水、工具 schema、审计日志等
//! sidecar 专属逻辑仍留在 `mt-sidecars`。
//!
//! 依赖版本注意:russh 用 ring 后端(Windows MSVC 无 NASM,见
//! spec/backend/rust-crypto-on-windows-msvc.md);ssh-key / rsa 精确锁定(见
//! Cargo.toml 注释与 spec/backend/russh-rsa-key-loading.md),不要动。

pub mod pool;
pub mod sftp;

/// 再导出 russh:消费方(如 mt-ssh-mcp 对 `ChannelMsg` 的匹配)必须与池用同一个
/// russh crate 版本,否则 `Handle` / `Channel` 类型跨 crate 版本不可互换。
/// 统一从 `mt_ssh::russh` 取,避免各消费方自行声明 russh 依赖后版本漂移。
pub use russh;

pub use pool::{
    run_sftp_download_on_session, run_sftp_upload_on_session, CachedSession, MtClient,
    PoolConfig, SftpTransferError, SshPool,
};
pub use sftp::{SftpDirEntry, SftpHandle};
