//! mt-ssh —— mini-term 的共享 SSH 通信层(russh 持久会话池 + SFTP 传输原语)。
//!
//! 自 `mt-sidecars/src/pool.rs` 抽出(task 07-05-ssh-remote-projects PR1):
//! 主程序(src-tauri)与 sidecar(mt-ssh-mcp)都需要按 `mt_core::SshConnection`
//! 建立/复用 SSH session、exec、开 SFTP,两边以路径依赖共用本 crate、各持一个
//! 池实例。本 crate **不依赖 tauri / rmcp**;MCP 胶水、工具 schema、审计日志等
//! sidecar 专属逻辑仍留在 `mt-sidecars`。
//!
//! 建连只看连接配置本身(`target`),`~/.ssh` 里只碰 known_hosts(支持通配与
//! `@revoked`,`known_hosts`):不读 `~/.ssh/config`、不试默认私钥。认证依次试连接私钥 /
//! ssh-agent / 密码(`auth`),建连整段限时且超时不泄漏 russh 任务(`transport`)。
//! 建连 + 握手 + 认证这一段会话池与终端 shell 共用(`connect`)。
//!
//! 终端里的 SSH(`shell`):每个终端 pane 独占一条连接,直接开 pty + shell / exec,远端字节
//! 直接进终端 VT;首见主机的 yes / no、私钥口令、密码与 keyboard-interactive 在终端里问用户
//! (`interact`、`key_unlock`)。会话池 / 「测试连接」/ sidecar 不带提示器,行为不受影响。
//!
//! 依赖版本注意:russh 用 ring 后端(Windows MSVC 无 NASM,见
//! spec/backend/rust-crypto-on-windows-msvc.md);ssh-key / rsa 精确锁定(见
//! Cargo.toml 注释与 spec/backend/russh-rsa-key-loading.md),不要动。

mod auth;
mod connect;
mod interact;
mod key_unlock;
mod known_hosts;
pub mod pool;
pub mod sftp;
mod sftp_transfer;
pub mod shell;
mod target;
mod transport;

/// 再导出 russh:消费方(如 mt-ssh-mcp 对 `ChannelMsg` 的匹配)必须与池用同一个
/// russh crate 版本,否则 `Handle` / `Channel` 类型跨 crate 版本不可互换。
/// 统一从 `mt_ssh::russh` 取,避免各消费方自行声明 russh 依赖后版本漂移。
pub use russh;

pub use auth::{AuthFailureReport, AuthMethodUsed};
pub use pool::{
    AcquireError, AcquireErrorKind, AgentSource, BoundedExecOutput, BoundedExecState,
    CachedSession, MtClient, PoolConfig, ProbeSuccess, SftpTransferError, SshClientEnv, SshPool,
};
pub use sftp::{SftpDirEntry, SftpHandle, SftpNodeKind};
pub use shell::{
    ShellConfig, ShellEnd, ShellExit, ShellOpenError, ShellOpenErrorKind, ShellPrompt,
    ShellRequest, ShellSession,
};

// 函数单列一组:与类型混在同一个 `{}` 里时,rustfmt 2021 / 2024 两种口径的排序相反。
pub use auth::parse_auth_failure;
pub use pool::{
    run_bounded_exec_on_session, run_sftp_download_on_session, run_sftp_upload_on_session,
    run_sftp_upload_until,
};
pub use shell::open_shell;
