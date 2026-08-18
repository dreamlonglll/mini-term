//! 项目侧的本地能力:文件树、目录监听、搜索、Git、外部编辑器、WSL 发行版枚举。
//!
//! **不依赖 Tauri,也不依赖 GPUI。** 全是同步阻塞的普通函数 + 少量长生命周期对象
//! ([`watch::FsWatcher`] / [`search::SearchManager`]),线程调度由调用方决定。
//!
//! # 已移入
//!
//! | 来源 | 去向 | 说明 |
//! |---|---|---|
//! | `src-tauri/src/fs.rs` | [`fs`] + [`watch`] | 目录列举(`.gitignore` 过滤)+ 文件增删改 / `notify` 监听 |
//! | `src-tauri/src/git.rs` | [`git`] | git2 状态/diff/log/stage/commit/worktree 全套 |
//! | `src-tauri/src/search.rs` | [`search`] | 全文搜索(可取消) |
//! | `src-tauri/src/editor.rs` | [`editor`] | 用外部编辑器 / 默认程序打开路径 |
//! | `src-tauri/src/wsl_distros.rs` | [`wsl_distros`] | 读 `HKCU\...\Lxss` 注册表枚举发行版 |
//!
//! # 移植时改掉的
//!
//! - **`fs-change` 不再 `emit`**:[`watch::FsWatcher`] 构造时注入一个 sink
//!   (`Fn(FsChange) + Send + Sync`),由上层决定怎么接 —— GPUI 侧典型做法是在
//!   sink 里更新 model 后 `cx.notify()` 触发重绘,因此也不再需要前端侧的防抖去重。
//!   watch/unwatch 的引用计数生命周期语义原样保留。
//! - **搜索取消不再走 IPC**:[`search::start_search`] 直接返回
//!   [`search::SearchHandle`],谁拿着谁能取消。`search_id` 随之消失
//!   (它只是为了跨 IPC 对齐事件);[`search::SearchManager`] 退化成
//!   「同一项目同时只留一个搜索」的簿记,键换成项目根路径。
//! - **错误类型 `Result<T, String>` → `anyhow::Result<T>`**,面向用户的中文文案
//!   一字未改(UI 仍可直接把 `to_string()` 弹出来)。
//! - **路径参数 `String` → `&Path`**,`AppHandle` / `State` 换成显式参数。
//!   [`editor::open_in_editor`] 因此不再自己读配置,「选哪个编辑器」拆成纯函数
//!   [`editor::select_editor`],本 crate 不依赖 `mt-config`。
//! - `git2` 仍用 `vendored-openssl` feature —— 换 GPUI 不改变这条,
//!   Windows MSVC 上的坑与依据见 `spec/backend/rust-crypto-on-windows-msvc.md`。
//!
//! # 调用方必须知道的
//!
//! 网络与大 IO 类函数(`git::git_pull` / `git_push` / `add_worktree` /
//! `remove_worktree` / `prune_worktrees` / `git_commit`,以及
//! [`search::run_search`])**会阻塞调用线程**。原实现靠 `#[tauri::command(async)]`
//! 把它们挪出主线程,这一层不做线程调度,调用方要自己丢到后台执行器上跑,
//! 否则 30s/120s 的超时会把 UI 线程按死。
//!
//! # 未决
//!
//! **远程 SSH 项目**(`remote_ssh.rs` 1281 行)依赖 `src-tauri/mt-ssh`,
//! 而 `mt-ssh` / `mt-core` 目前仍留在 `src-tauri/` 下(还被 `mt-sidecars` 引用)。
//! 等到要移植远程项目时再一并把这两个 crate 挪进 `crates/`,同时改
//! `mt-sidecars` 的 path 依赖与 `scripts/stage-sidecars.mjs`。**不要提前挪。**
//! 届时远程文件树复用 [`fs::natural_cmp`],与本地树保持同一排序观感。

pub mod editor;
pub mod fs;
pub mod git;
pub mod search;
pub mod watch;
pub mod wsl_distros;
