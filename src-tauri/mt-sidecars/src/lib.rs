//! mt-sidecars 共享库:供 sidecar 二进制(mt-ssh-mcp / miniterm-hook)调用的内部模块。
//!
//! 目前仅暴露 `pool` —— mt-ssh-mcp 的 SSH 会话池实现。把它放在 lib 层而非
//! bin 内联,主要是为了:
//! 1. 让 `cargo test -p mt-sidecars` 能直接跑池的单测;
//! 2. 文件分文件后,bin 入口保持轻,只做工具注册与编排。

pub mod pool;
