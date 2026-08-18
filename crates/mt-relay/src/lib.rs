//! 移动端中转体系的桌面端一侧。
//!
//! # 待移入
//!
//! | 来源 | 行数 | 说明 |
//! |---|---|---|
//! | `src-tauri/src/mobile_relay.rs` | 1233 | 出站 WSS 长连、密钥握手、指数退避重连、配对码、项目快照与增量、移动端指令写穿 PTY |
//! | `src-tauri/src/mobile_mirror.rs` | 598 | pane → 项目最新会话 JSONL 的增量解析(半行拼接)与分页 |
//!
//! # 边界不变
//!
//! - 协议由 `relay-server/protocol` 定义,当前 **v2**;PWA 侧的 TS 类型在
//!   `mobile/src/protocol.ts` 手写镜像,两侧字段必须同步维护。**本次改造完全
//!   不触碰协议与 `mobile/`。**
//! - AI 启动器的命令文本从不经过移动端或中转(ADR 0002),移植时保持该边界。
//! - 现网中转 `wss://relay.dreaminglong.com` 跑的就是 v2 + 桌面端密钥,
//!   桌面端换 UI 框架对它是透明的。
//!
//! # 移植时要改的
//!
//! `mobile-start-session` / `mobile-rename-pane` 两个事件原本 `emit` 到前端,
//! 改成直接调用 GPUI 侧的对应动作(新建 pane / 改标题),这条链路反而变短了。
