//! 移动端中转体系的桌面端一侧。
//!
//! # 已移入
//!
//! | 来源 | 行数 | 落位 |
//! |---|---|---|
//! | `src-tauri/src/mobile_relay.rs` | 1233 | [`relay`] |
//! | `src-tauri/src/mobile_mirror.rs` | 598 | [`mirror`] |
//!
//! 出站 WSS 长连、桌面端密钥握手、指数退避重连、配对码/重置配对、项目快照与
//! 项目级增量、镜像订阅管理、移动端指令写穿 PTY、移动端发起会话的校验派发、
//! 改会话名的标题收敛 —— 全部逐字搬运,只换了 Tauri 那几个出入口。
//!
//! # 边界不变
//!
//! - 协议由 `relay-server/protocol` 定义,当前 **v2**;PWA 侧的 TS 类型在
//!   `mobile/src/protocol.ts` 手写镜像,两侧字段必须同步维护。**本次改造完全
//!   不触碰协议与 `mobile/`。**
//! - AI 启动器的命令文本从不经过移动端或中转(ADR 0002):
//!   [`AiLauncher`] 的 `command` / `shell` 只在桌面端进程内流转,
//!   发出去的 [`MobileLauncher`] 只有 `id` + `name`。
//! - 现网中转 `wss://relay.dreaminglong.com` 跑的就是 v2 + 桌面端密钥,
//!   桌面端换 UI 框架对它是透明的。
//! - **镜像绑定红线**:hook 上报过会话身份就只认那一个会话文件;没有身份时,
//!   只有"确实会写我们认识的会话记录"的 agent(claude / codex / grok)才退启发式。
//!   opencode / pi 这类必须给空镜像 —— 退启发式会把同项目里别家的对话贴到这个
//!   pane 上(串台),见 [`mirror::agent_has_session_log`]。
//!
//! # 与原实现的接口差异
//!
//! - 四个 Tauri 事件全部改注入回调,收在 [`RelayEvents`] 上:
//!   `mobile-relay-status` → [`RelayEvents::status_changed`]、
//!   `mobile-relay-pairing-code` → [`RelayEvents::pairing_code`]、
//!   `mobile-rename-pane` → [`RelayEvents::rename_pane`]、
//!   `mobile-start-session` → [`RelayEvents::start_session`]。
//!   后两个原本 emit 给前端由 TS 执行,现在是桌面侧的直接动作(建 pane / 改标题),
//!   这条链路反而变短了。
//! - 对桌面侧状态的依赖(项目表、启动器名单、PTY 写穿、AI 会话身份)收在
//!   [`RelayHost`] 上,本 crate 因此不依赖 mt-pty / mt-config / mt-app / gpui。
//! - 八个 `#[tauri::command]` 去壳成普通方法:
//!   `mobile_relay_apply` → [`MobileRelayManager::apply`]、
//!   `mobile_relay_status` → [`MobileRelayManager::current_status`]、
//!   `mobile_relay_request_pairing_code` → [`MobileRelayManager::request_pairing_code`]、
//!   `mobile_relay_reset_pairing` → [`MobileRelayManager::reset_pairing`]、
//!   `mobile_relay_update_sessions` → [`MobileRelayManager::update_sessions`]、
//!   `mobile_relay_launchers_changed` → [`MobileRelayManager::launchers_changed`]、
//!   `mobile_relay_start_session_result` → [`MobileRelayManager::start_session_result`]、
//!   `mobile_relay_check_launcher_command` → [`check_launcher_command`]。
//! - 中转地址 / 桌面端密钥不再由本 crate 读配置,改由 [`MobileRelayManager::apply`]
//!   显式传入;启动器名单走 [`RelayHost::launchers`] 现取。
//! - 连接循环与镜像轮询原先跑在 `tauri::async_runtime` 上,现在跑在本 crate
//!   自持的小 tokio 运行时(或宿主用 [`MobileRelayManager::with_runtime`] 注入的
//!   运行时)上。
//!
//! # 接线概览(Wave 2 的 mt-app)
//!
//! ```ignore
//! let manager = Arc::new(MobileRelayManager::new(host.clone(), events.clone()));
//! manager.apply(&cfg.relay_url, &cfg.desktop_key);   // 启动时 / 保存设置时
//! manager.update_sessions(projects);                  // store 变化时喂入全量
//! ```
//!
//! [`MobileLauncher`]: mt_relay_protocol::MobileLauncher

pub mod host;
pub mod mirror;
pub mod relay;
mod util;

pub use host::{AiLauncher, HookSessionId, NoopRelayHost, RelayEvents, RelayHost, RelayProject};
pub use mirror::{
    agent_has_session_log, history_slice, MirrorAgent, MirrorParser, MIRROR_PAGE_SIZE,
};
pub use relay::{
    can_start_session, check_launcher_command, MobileRelayManager, MobileRelayStatusPayload,
    RenamePanePayload, StartSessionPayload, SyncPane, SyncProject,
};
