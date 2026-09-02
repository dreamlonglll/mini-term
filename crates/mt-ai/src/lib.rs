//! AI 感知:hook 上报、状态判定、会话记录读取。
//!
//! **这是 mini-term 真正的差异化所在,也是迁移中最不该动逻辑的一块。**
//! 整块与 Tauri 的耦合只有两处(读配置目录、emit 状态事件),其余是纯 Rust,
//! 已按**逐字搬运**的原则移入,没有顺手重构。
//!
//! # 已移入
//!
//! | 来源 | 行数 | 落位 |
//! |---|---|---|
//! | `src-tauri/src/hook_server.rs` | 1187 | [`hook_server`] |
//! | `src-tauri/src/hook_registry.rs` | 1111 | [`hook_registry`] |
//! | `src-tauri/src/process_monitor.rs` | 571 | [`monitor`] |
//! | `src-tauri/src/ai_sessions.rs` | 2348 | [`sessions`] |
//! | `src-tauri/src/pty.rs` 的 AI 命令识别 / 打断识别 | — | [`detect`] + [`tracker`] |
//! | `src-tauri/src/mobile_mirror.rs::agent_has_session_log` | — | [`sessions`] |
//!
//! # 搬运时的红线(仍然生效)
//!
//! - **降级结论必须落盘**:用户打断([`hook_server::note_user_interrupt`])与停摆
//!   兜底(`monitor::stall_settle_target`,10s 双静默)得出的结论要写回 hook 状态,
//!   触发一次即收敛。v0.9.3 那版无记忆兜底会让假完成每 20~50s 重复播报 —— 这条
//!   铁律不能丢失。
//! - **正等用户批准的 pane 豁免停摆兜底**(上次 cause 属 attention 类,如 Codex 的
//!   `PermissionRequest`),否则黄灯会被抹掉。
//! - **Grok 的两处结构性差异**见 [`hook_registry`] 的模块注释:
//!   ① Claude 兼容层导致同一事件来两趟,靠 `GROK_SESSION_ID` + 有无 argv 丢弃
//!   (只注册了 Claude 的用户必须放行);② 注册进 `~/.grok/hooks/` 的必须是
//!   **不含空格的裸文件名**。
//! - **只有 Claude / Codex / Grok 有可解析的会话记录**
//!   ([`sessions::agent_has_session_log`])。opencode / pi / omp 这类没有记录解析的
//!   agent 必须在镜像绑定时跳过,否则会绑到同项目其它 agent 的最新会话文件。
//! - **oh-my-pi(omp)的 hook 不走 sidecar**:它的扩展点是进程内加载的 TS 模块,
//!   [`hook_registry`] 把自带的扩展整份写进 `~/.omp/agent/extensions/`,扩展在 omp
//!   进程内 `fetch` 本地 hook 服务器,事件名翻译成与 Claude 同名的 PascalCase。
//! - **hook 接收端原样保留**:端口(23456 起,冲突递增 5 次)、路由(`POST /hook`)、
//!   payload 形状一个字都没改 —— 各家已注册在用户机器上的 hook 命令还得打得进来。
//!
//! # 与原实现的接口差异
//!
//! - `pty-status-change` 不再 `emit`,改成注入式 [`monitor::StatusSink`];
//!   `pty-ai-session` 走同一 trait 的 `session_identified`(默认 no-op)。
//!   **[`monitor::StatusEmitter`] 的去重表原样保留** —— 它防的是迟到 hook 事件推错
//!   状态后 monitor 的纠正被吞掉,与传输层无关。
//! - 输入检测那一路原本长在 `pty.rs` 里,现在由上层把 PTY 两个方向的字节旁路给
//!   [`AiPerception::observe_input`] / [`AiPerception::observe_output`]。
//!   本 crate 不依赖 mt-pty,也不依赖 gpui。
//! - 原先经 Tauri 解析的路径(`app_data_dir` 下的端口文件)改为显式参数传入。

pub mod detect;
pub mod hook_registry;
pub mod hook_server;
pub mod monitor;
pub mod perception;
pub mod sessions;
pub mod tracker;
mod util;

pub use detect::{interactive_ai_command_name, is_interactive_ai_command, AI_COMMANDS};
pub use hook_server::{is_attention_cause, HookState, HookStatusInfo};
pub use monitor::{SessionIdentity, StatusChange, StatusEmitter, StatusSink};
pub use perception::AiPerception;
pub use sessions::{agent_has_session_log, AiSession, AiSessionMessage, LineageEdge};
pub use tracker::{SessionTracker, UserSubmit};
