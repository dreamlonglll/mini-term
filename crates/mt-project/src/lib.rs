//! 项目侧的本地能力:文件树、搜索、Git、外部编辑器、WSL 发行版枚举。
//!
//! # 待移入
//!
//! | 来源 | 行数 | 说明 |
//! |---|---|---|
//! | `src-tauri/src/fs.rs` | 868 | 目录列举(`.gitignore` 过滤)+ `notify` 监听 + 文件增删改 |
//! | `src-tauri/src/git.rs` | 1402 | git2 状态/diff/log/stage/commit/worktree 全套 |
//! | `src-tauri/src/search.rs` | 461 | 全文搜索(可取消) |
//! | `src-tauri/src/editor.rs` | 59 | 用外部编辑器 / 默认程序打开路径 |
//! | `src-tauri/src/wsl_distros.rs` | 128 | 读 `HKCU\...\Lxss` 注册表枚举发行版 |
//!
//! # 移植时要改的
//!
//! - `fs-change` 事件原本 `emit` 给前端,改成把变更推给 GPUI 的 model
//!   (`cx.notify()` 触发重绘),文件树因此不再需要前端侧的防抖去重。
//! - `git2` 仍用 `vendored-openssl` feature —— 换 GPUI 不改变这条,
//!   Windows MSVC 上的坑与依据见 `spec/backend/rust-crypto-on-windows-msvc.md`。
//! - `search` 的取消原本靠 Tauri managed `SearchManager` + 前端发 cancel 命令,
//!   GPUI 下同一个进程内直接用 `AbortHandle` 即可。
//!
//! # 未决
//!
//! **远程 SSH 项目**(`remote_ssh.rs` 1281 行)依赖 `src-tauri/mt-ssh`,
//! 而 `mt-ssh` / `mt-core` 目前仍留在 `src-tauri/` 下(还被 `mt-sidecars` 引用)。
//! 等到要移植远程项目时再一并把这两个 crate 挪进 `crates/`,同时改
//! `mt-sidecars` 的 path 依赖与 `scripts/stage-sidecars.mjs`。**不要提前挪。**
