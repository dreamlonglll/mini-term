//! 桌面侧注入接口。
//!
//! 中转体系要读写桌面端的状态(项目表、AI 启动器名单、PTY、AI 会话身份),
//! 但本 crate **不依赖 mt-pty / mt-app / gpui** —— 这些依赖全部收敛到这里的两个
//! trait 上,由上层(Wave 2 的 mt-app)实现并在构造 [`MobileRelayManager`] 时注入。
//!
//! - [`RelayHost`]:入向查询(桌面端状态 → 中转)。原来靠 `tauri::State` 现取
//!   `PtyManager` / `HookState` / `read_config(app)` 的那些地方,现在走它。
//! - [`RelayEvents`]:出向动作(中转 → 桌面端)。对应原来四个 Tauri 事件:
//!   `mobile-relay-status` / `mobile-relay-pairing-code` /
//!   `mobile-rename-pane` / `mobile-start-session`。前两个是状态通知,后两个
//!   原本 emit 给前端由 TS 执行,现在直接是桌面侧的动作回调(建 pane / 改标题)。
//!
//! **两个 trait 的方法都可能在 tokio 工作线程上被调用**(连接循环与镜像轮询都在
//! 后台运行时里),实现方自己负责跳回 UI 线程。
//!
//! [`MobileRelayManager`]: crate::MobileRelayManager

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

pub use mt_ai::hook_server::HookSessionId;

use crate::relay::{MobileRelayStatusPayload, RenamePanePayload, StartSessionPayload};

/// 一条具名的"怎么起一个 AI 会话"。
///
/// 形状与 `mt_config::AiLauncher` 一致(serde 也对齐),但本 crate **不依赖
/// mt-config**:启动器名单由宿主注入,配置的读写归配置层。
/// ADR 0002 的边界在此:`command` / `shell` 只在桌面端进程内流转,
/// 发给移动端的只有 `id` + `name`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiLauncher {
    pub id: String,
    /// 展示名(移动端弹层里看到的就是它)
    pub name: String,
    /// 引用 `available_shells` 里的条目名;None / 空 = 用 `default_shell`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    pub command: String,
}

/// 移动端发起会话时要校验的项目投影(桌面端项目表的最小切面)。
#[derive(Debug, Clone, PartialEq)]
pub struct RelayProject {
    pub path: String,
    /// SSH 远程项目引用的连接 id;本地项目为 None
    pub ssh_connection_id: Option<String>,
}

/// 中转体系向桌面端要东西的入口。
pub trait RelayHost: Send + Sync + 'static {
    /// 当前配置里的 AI 启动器名单(配置整块缺失时由实现方回落预置条目)。
    /// 每次需要时现取:它是低频数据,没必要在中转层再维护一份副本。
    fn launchers(&self) -> Vec<AiLauncher>;

    /// 按 id 取项目(移动端发起会话时校验目标存在且支持)。
    fn project(&self, project_id: &str) -> Option<RelayProject>;

    /// 把一段文本写穿到指定 PTY,语义必须等价"本人在桌面对该终端敲了这些字节"
    /// (输入跟踪 / AI marker / SSH autofill 解除一个都不能少)。
    fn write_pty(&self, pty_id: u32, data: String) -> Result<(), String>;

    /// hook 上报过的会话身份(pty → agent + session_id);None = 未启用 hook 或尚未上报。
    /// 镜像绑定的第一层就靠它精确定位会话文件。
    fn hook_session(&self, pty_id: u32) -> Option<HookSessionId>;

    /// 输入检测识别到的 agent 名(无 hook 时判断"这个 agent 有没有会话记录")。
    fn ai_session_agent(&self, pty_id: u32) -> Option<String>;

    /// 本轮 AI 会话的启动时刻(启发式绑定的 mtime 下限);不在 AI 会话中返回 None。
    fn ai_session_started_at(&self, pty_id: u32) -> Option<SystemTime>;
}

/// 中转体系推给桌面端的状态与动作。
pub trait RelayEvents: Send + Sync + 'static {
    /// 连接状态变化(原 `mobile-relay-status` 事件),设置页「移动端」区域展示。
    fn status_changed(&self, status: MobileRelayStatusPayload);

    /// 中转签发的一次性配对码(原 `mobile-relay-pairing-code` 事件),用于出二维码。
    fn pairing_code(&self, code: String);

    /// 移动端改会话名(原 `mobile-rename-pane` 事件)。标题已收敛过,
    /// 空串 = 清除自定义名、回落 shell 名。
    fn rename_pane(&self, payload: RenamePanePayload);

    /// 移动端发起新 AI 会话(原 `mobile-start-session` 事件)。校验已通过,
    /// 桌面侧负责建 pane 并写入启动命令,完成后调
    /// [`MobileRelayManager::start_session_result`] 回执。
    ///
    /// [`MobileRelayManager::start_session_result`]: crate::MobileRelayManager::start_session_result
    fn start_session(&self, payload: StartSessionPayload);
}

/// 什么都不做的宿主实现。
///
/// **只给测试和"尚未接线"的占位场景用**:它让 `launchers()` / `project()` 恒空,
/// 移动端会看到"项目不存在 / 启动器不存在",镜像绑定永远退不出空快照。
/// 生产路径必须注入真正的实现。
pub struct NoopRelayHost;

impl RelayHost for NoopRelayHost {
    fn launchers(&self) -> Vec<AiLauncher> {
        Vec::new()
    }
    fn project(&self, _project_id: &str) -> Option<RelayProject> {
        None
    }
    fn write_pty(&self, _pty_id: u32, _data: String) -> Result<(), String> {
        Err("relay host not wired".into())
    }
    fn hook_session(&self, _pty_id: u32) -> Option<HookSessionId> {
        None
    }
    fn ai_session_agent(&self, _pty_id: u32) -> Option<String> {
        None
    }
    fn ai_session_started_at(&self, _pty_id: u32) -> Option<SystemTime> {
        None
    }
}

impl RelayEvents for NoopRelayHost {
    fn status_changed(&self, _status: MobileRelayStatusPayload) {}
    fn pairing_code(&self, _code: String) {}
    fn rename_pane(&self, _payload: RenamePanePayload) {}
    fn start_session(&self, _payload: StartSessionPayload) {}
}
