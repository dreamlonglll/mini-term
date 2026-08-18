//! AI 感知:hook 上报、状态判定、会话记录读取。
//!
//! **这是 mini-term 真正的差异化所在,也是迁移中最不该动逻辑的一块。**
//! 整块与 Tauri 的耦合只有两处(读配置目录、emit 状态事件),其余是纯 Rust,
//! 应当**逐字搬运**,不要顺手重构。
//!
//! # 待移入
//!
//! | 来源 | 行数 | 说明 |
//! |---|---|---|
//! | `src-tauri/src/hook_server.rs` | 1187 | hook 上报接收端(权威状态源) |
//! | `src-tauri/src/hook_registry.rs` | 1111 | Claude / Codex / Grok 三家的 hook 注册 |
//! | `src-tauri/src/process_monitor.rs` | 571 | 500ms 轮询 + 降级判定 + 停摆兜底 |
//! | `src-tauri/src/ai_sessions.rs` | 2123 | 三家会话记录的读取与谱系扫描 |
//!
//! # 搬运时的红线
//!
//! - **降级结论必须落盘**:用户打断(`note_user_interrupt`)与停摆兜底
//!   (`stall_settle_target`,10s 双静默)得出的结论要写回 hook 状态,触发一次
//!   即收敛。v0.9.3 那版无记忆兜底会让假完成每 20~50s 重复播报 —— 这条铁律
//!   不能在搬运中丢失。
//! - **Grok 的两处结构性差异**照抄 `hook_registry::register_grok_hooks` 的注释:
//!   ① Claude 兼容层导致同一事件来两趟,靠 `GROK_SESSION_ID` + 有无 argv 丢弃;
//!   ② 注册进 `~/.grok/hooks/` 的必须是**不含空格的裸文件名**。
//! - **只有 Claude / Codex / Grok 有可解析的会话记录**。opencode / pi 这类只靠
//!   输入检测识别的 agent 必须在镜像绑定时跳过,否则会绑到同项目其它 agent 的
//!   最新会话文件。
//!
//! # 移植时要改的
//!
//! - `pty-status-change` 事件不再 `emit`,改成更新 GPUI model;
//!   `StatusEmitter` 的**去重表要保留** —— 它防的是迟到 hook 事件推错状态后
//!   monitor 的纠正被吞掉,与传输层无关。
//! - 输入检测那一路(识别键入的 `claude`/`codex`/`opencode`/`pi`/`grok`,含 ↑ 历史
//!   与 Tab 补全的行快照兜底)原本长在 `pty.rs` 里,现在改由 `mt-pty` 的写入路径
//!   旁路一份字节过来。`mt-pty` 不该知道 AI 的存在。
