//! mini-term 应用域非 GPUI 核心层。
//!
//! 承接原 `mt-app` 中不依赖 GPUI 的纯数据结构、状态机、后台单写者、文件与 Git
//! 监听及 diff 算法：单测无需链接 GPUI / WebView / Direct3D，并自动归入
//! `.github/scripts/non_gpui_packages.mjs` 的跨平台快速测试集。

pub mod ai;
pub mod cli;
pub mod config_writer;
pub mod file_ops;
pub mod file_tree_git_refresh;
pub mod focus_nav;
pub mod git;
pub mod kitty_graphics;
pub mod layout_writer;
pub mod line_input;
pub mod markers;
pub mod persist;
pub mod project_tree;
pub mod pty_slot;
pub mod quick_ssh;
pub mod session_branch;
pub mod shell_ops;
pub mod ssh_activity;
pub mod ssh_conn;
pub mod startup_trace;
pub mod tree;
pub mod workbench;
