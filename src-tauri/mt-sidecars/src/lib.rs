//! mt-sidecars 共享库 —— sidecar 二进制之间复用的业务层。
//!
//! `ssh_service` 是 SSH 工具的完整业务编排（连接查找、池 acquire、evict+retry、
//! cooldown、审计、传输护栏），`mt-ssh-mcp`（rmcp 适配层）与 `mt-ssh-cli`
//! （CLI/daemon 传输层）都只做协议编解码、业务全部转调这里。
//!
//! 与 `mt-core` 的分工：mt-core 放跨主程序/sidecar 的纯逻辑（config 读取、
//! SshConnection 类型），这里放仅 sidecar 需要的业务编排（依赖 mt-ssh 会话池）。

pub mod daemon;
pub mod ipc;
pub mod ssh_service;
