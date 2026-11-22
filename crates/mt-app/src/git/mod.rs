//! Git 面板与工作流子系统。
//!
//! 收拢原先平铺在 `mt-app/src/` 顶层的 15 个 `git_*` 模块：后端抽象、变更与提交、
//! 分支 / 推送 / 贮藏 / Worktree、Diff 视图、提交历史与图形、冲突解决、目录与 PTY
//! 刷新监听，以及共享 UI 原语。

pub mod backend;
pub mod branches;
pub mod changes;
pub mod conflict;
pub mod diff;
pub mod file_history;
pub mod graph;
pub mod history;
pub mod panel;
pub mod push;
pub mod stash;
pub mod ui;
pub mod worktree;

pub use mt_app_core::git::{dir_watch, watch};
