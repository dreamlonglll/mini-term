//! Hook HTTP 服务器模块
//!
//! 在后台线程监听 `127.0.0.1` 的 HTTP 请求，接收 Claude Code / Codex / Grok /
//! oh-my-pi 的 hook 事件上报，并通过注入的 [`StatusEmitter`] 通知上层。
//!
//! **端口 / 协议 / 路由一个字都不能改**:各家 CLI 里已注册在用户机器上的 hook
//! 命令（与 omp 的进程内扩展）按当前形态 POST 过来,改了等于让存量用户的 AI 感知
//! 集体失灵。

use crate::monitor::{SessionIdentity, StatusEmitter};
use crate::tracker::SessionTracker;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// hook HTTP 线程与 GPUI 主线程、500ms 轮询线程共享这几张表:std::sync::Mutex 一旦
// 有持锁者 panic 就整把锁中毒,主线程下一次 lock 跟着 panic 整个进程就没了。
// parking_lot 没有中毒概念(与 mt-app/mt-pty/mt-terminal 同款)。
use parking_lot::Mutex;

/// 默认监听端口
const DEFAULT_PORT: u16 = 23456;
/// 端口冲突时最多尝试的端口数
const MAX_PORT_ATTEMPTS: u16 = 5;
/// 每个 PTY 保留的已结束会话墓碑数量上限
const ENDED_SESSIONS_CAP: usize = 8;
/// 每个 PTY 跟踪的活跃会话数量上限（正常只有 1 个；嵌套非交互实例/事件乱序
/// 时短暂多个，上限只是防御事件丢失导致的累积）
const ACTIVE_SESSIONS_CAP: usize = 8;
/// 单个 hook 请求 body 的字节上限。真实 payload 只有几百字节（三家 CLI 发的都是
/// 一小段 JSON），1 MiB 已宽出三个数量级；超限直接 413，不进 JSON 解析。
const MAX_HOOK_BODY_BYTES: usize = 1024 * 1024;
/// Hook 事件的 JSON payload
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // 保留完整字段供未来 UI 细化使用
pub struct HookPayload {
    /// PTY ID（由 MINITERM_PTY_ID 环境变量传递）
    pub pty_id: Option<u32>,
    /// 事件名（如 UserPromptSubmit, PreToolUse 等）
    pub event: Option<String>,
    /// 来源 agent（claude-code / codex / grok / omp）
    pub agent: Option<String>,
    /// 会话 ID
    pub session_id: Option<String>,
    /// 工作目录
    pub cwd: Option<String>,
    /// 工具名称（PreToolUse/PostToolUse 时有值）
    pub tool_name: Option<String>,
    /// SessionEnd 的结束原因（clear / logout / prompt_input_exit / other），
    /// Claude Code 写在 stdin payload 里，sidecar 原样转发
    pub reason: Option<String>,
    /// Notification 事件的通知文案(Claude Code 自带,sidecar 原样转发),
    /// 用于区分「API 错误/重试中」与「需要授权/等待输入」
    pub message: Option<String>,
    /// Notification 事件的结构化类型(permission_prompt / idle_prompt /
    /// elicitation_dialog / …,与官方 hooks 文档的 Notification matcher 同集)。
    /// 比 `message` 文案匹配可靠得多,优先采信;缺失时(旧版 Claude Code)才回落文案。
    pub notification_type: Option<String>,
    /// StopFailure 的错误类别(rate_limit / overloaded / authentication_failed /
    /// max_output_tokens / …)。目前只入日志,不参与状态判定 —— 无论哪种错误,
    /// 回合都已经结束,状态一律回落 ai-idle。
    pub error_type: Option<String>,
    /// hook payload 自带的事件名(Claude Code 公共字段)。sidecar 从 argv 取事件名
    /// 注入 `event`;用户手改配置漏写参数时靠这个兜底,否则整条事件被丢弃。
    pub hook_event_name: Option<String>,
}

/// Hook 状态信息，供 UI 查询
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStatusInfo {
    pub port: u16,
    pub running: bool,
}

/// pane 内 AI 会话的精确身份（hook 上报）。对话镜像用它把 pane 绑到
/// 确切的会话记录文件，避免同项目多 pane 串台。
#[derive(Debug, Clone)]
pub struct HookSessionId {
    /// 来源 agent（claude-code / codex），缺省按 Claude 处理
    pub agent: Option<String>,
    pub session_id: String,
}

/// Hook 状态管理器，记录每个 PTY 的最后 hook 事件时间和状态
#[derive(Clone)]
pub struct HookState {
    last_hook_time: Arc<Mutex<HashMap<u32, Instant>>>,
    last_hook_status: Arc<Mutex<HashMap<u32, String>>>,
    /// pty → 当前会话身份；/clear 等换会话时随下一个 hook 事件自动刷新
    last_session: Arc<Mutex<HashMap<u32, HookSessionId>>>,
    /// 记录哪些 PTY 曾经收到过 hook 事件（一旦标记，永不降级回轮询）
    hook_enabled: Arc<Mutex<std::collections::HashSet<u32>>>,
    /// pty → 已结束会话 id 的环形墓碑。hook 脚本是独立进程，POST 到达
    /// 顺序无保证：SessionEnd 之后仍可能收到旧会话迟到的 Stop/Notification，
    /// 若放行会把已退出的 pane 重新推回 ai-idle。`remove()` 不清墓碑
    /// （SessionEnd 自身要先打墓碑再 remove），PTY 关闭时由 `purge()` 清理。
    ended_sessions: Arc<Mutex<HashMap<u32, VecDeque<String>>>>,
    /// pty → 当前活跃会话 id 集合（有序去重）。SessionEnd 只有在移除该会话后
    /// 集合为空时才执行销毁动作：嵌套非交互实例（Bash 工具里跑 `claude -p` /
    /// `codex exec`，继承 MINITERM_PTY_ID）与"退出后立刻重开"的乱序场景下，
    /// pane 上还有别的活跃会话，误销毁会把正在工作的外层会话打回 idle。
    active_sessions: Arc<Mutex<HashMap<u32, VecDeque<String>>>>,
    port: Arc<Mutex<u16>>,
    /// 保存 server 实例，供运行时停止（Arc 共享给监听线程）
    server: Arc<Mutex<Option<Arc<tiny_http::Server>>>>,
}

impl Default for HookState {
    fn default() -> Self {
        Self::new()
    }
}

impl HookState {
    pub fn new() -> Self {
        Self {
            last_hook_time: Arc::new(Mutex::new(HashMap::new())),
            last_hook_status: Arc::new(Mutex::new(HashMap::new())),
            last_session: Arc::new(Mutex::new(HashMap::new())),
            hook_enabled: Arc::new(Mutex::new(std::collections::HashSet::new())),
            ended_sessions: Arc::new(Mutex::new(HashMap::new())),
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            port: Arc::new(Mutex::new(0)),
            server: Arc::new(Mutex::new(None)),
        }
    }

    /// 检查指定 PTY 是否已启用 hook（曾经收到过 hook 事件）
    ///
    /// 一旦启用，完全信任 hook 状态，不再降级回进程轮询。
    pub fn is_hook_enabled(&self, pty_id: u32) -> bool {
        self.hook_enabled.lock().contains(&pty_id)
    }

    /// 获取指定 PTY 的 hook 状态
    pub fn get_status(&self, pty_id: u32) -> Option<String> {
        self.last_hook_status.lock().get(&pty_id).cloned()
    }

    /// 距上一次 hook 事件（或上一次状态落盘）的时长；从未收到过事件返回 None。
    /// 停摆兜底（`monitor::stall_settle_target`）用它确认「状态本身也
    /// 已经静置足够久」，而不只是 PTY 没输出。
    pub(crate) fn status_age(&self, pty_id: u32) -> Option<Duration> {
        self.last_hook_time
            .lock()
            .get(&pty_id)
            .map(|t| t.elapsed())
    }

    /// 当前会话身份;从未收到带 session_id 的事件时返回 None
    pub fn session_of(&self, pty_id: u32) -> Option<HookSessionId> {
        self.last_session.lock().get(&pty_id).cloned()
    }

    /// 记录 hook 上报的会话身份(每个事件都带,直接覆盖即可)。
    /// 返回身份是否发生变化(新 pane/换会话/agent 修正),变化时调用方通知上层。
    /// agent 也参与比较:codex 的 SessionStart 不带 turn_id 会被 hook 二进制
    /// 误推断为 claude-code,靠后续带 turn_id 的事件在这里纠正并重新通知。
    fn record_session(&self, pty_id: u32, agent: Option<String>, session_id: String) -> bool {
        let mut map = self.last_session.lock();
        let changed = map
            .get(&pty_id)
            .is_none_or(|prev| prev.session_id != session_id || prev.agent != agent);
        map.insert(pty_id, HookSessionId { agent, session_id });
        changed
    }

    /// 更新指定 PTY 的 hook 状态
    pub(crate) fn update(&self, pty_id: u32, status: String) {
        self.hook_enabled.lock().insert(pty_id);
        self.last_hook_time
            .lock()
            .insert(pty_id, Instant::now());
        self.last_hook_status.lock().insert(pty_id, status);
    }

    /// 移除指定 PTY 的 hook 状态。不清墓碑：SessionEnd 打完墓碑后调用
    /// 本方法，墓碑要继续挡住旧会话的迟到事件。
    pub fn remove(&self, pty_id: u32) {
        self.hook_enabled.lock().remove(&pty_id);
        self.last_hook_time.lock().remove(&pty_id);
        self.last_hook_status.lock().remove(&pty_id);
        self.last_session.lock().remove(&pty_id);
    }

    /// PTY 关闭时的彻底清理：hook 状态 + 墓碑 + 活跃会话集
    pub fn purge(&self, pty_id: u32) {
        self.remove(pty_id);
        self.ended_sessions.lock().remove(&pty_id);
        self.active_sessions.lock().remove(&pty_id);
    }

    /// 记录会话为活跃。任意非 SessionEnd 事件都调（不只 SessionStart：
    /// hook server 中途启用时首个事件可能是 Stop/PreToolUse）。
    /// 有序去重；超容量挤掉最老的——正常情况集合里只有 1 个。
    fn note_session_active(&self, pty_id: u32, session_id: &str) {
        let mut map = self.active_sessions.lock();
        let queue = map.entry(pty_id).or_default();
        if queue.iter().any(|s| s == session_id) {
            return;
        }
        if queue.len() >= ACTIVE_SESSIONS_CAP {
            queue.pop_front();
        }
        queue.push_back(session_id.to_string());
    }

    /// SessionEnd：把该会话移出活跃集，返回移除后活跃集是否已空。
    /// 为空 → 这是 pane 上最后一个会话，调用方执行销毁动作；
    /// 非空 → pane 上还有别的活跃会话（嵌套 `claude -p` / 退出后立刻重开的
    /// 乱序），只打墓碑不销毁。payload 无 session_id 时不移除，仅报告空否。
    fn end_session(&self, pty_id: u32, session_id: Option<&str>) -> bool {
        let mut map = self.active_sessions.lock();
        let Some(queue) = map.get_mut(&pty_id) else {
            return true;
        };
        if let Some(sid) = session_id {
            queue.retain(|s| s != sid);
        }
        let empty = queue.is_empty();
        if empty {
            map.remove(&pty_id);
        }
        empty
    }

    /// 给已结束的会话 id 打墓碑
    pub fn mark_session_ended(&self, pty_id: u32, session_id: String) {
        let mut map = self.ended_sessions.lock();
        let queue = map.entry(pty_id).or_default();
        if queue.iter().any(|s| s == &session_id) {
            return;
        }
        if queue.len() >= ENDED_SESSIONS_CAP {
            queue.pop_front();
        }
        queue.push_back(session_id);
    }

    /// 该会话是否已被打墓碑（已结束）
    pub fn is_session_ended(&self, pty_id: u32, session_id: &str) -> bool {
        self.ended_sessions
            .lock()
            .get(&pty_id)
            .is_some_and(|q| q.iter().any(|s| s == session_id))
    }

    /// 摘除墓碑:SessionStart 表明同 id 会话再次存活(退出后 claude -c / --resume
    /// 重开),不摘的话该会话的后续事件被永久忽略,身份也无法重新记录
    pub fn revive_session(&self, pty_id: u32, session_id: &str) {
        if let Some(queue) = self.ended_sessions.lock().get_mut(&pty_id) {
            queue.retain(|s| s != session_id);
        }
    }

    /// 获取当前服务器端口
    pub fn get_port(&self) -> u16 {
        *self.port.lock()
    }

    /// 设置服务器端口
    fn set_port(&self, port: u16) {
        *self.port.lock() = port;
    }

    /// 保存 server 实例
    fn set_server(&self, server: Option<Arc<tiny_http::Server>>) {
        *self.server.lock() = server;
    }

    /// 检查 server 是否正在运行
    pub fn is_server_running(&self) -> bool {
        self.server.lock().is_some()
    }

    /// 与 server 启停串行化地执行回调。
    ///
    /// **刻意不检查 server 是否在运行**:调用方(停摆兜底)的判据建立在 pane 自己
    /// 的 hook 记录上,与 server 活着与否无关。曾在这里 gate 过 `server.as_ref()?`,
    /// 后果是 AI 正跑着时去设置里关掉 hook 开关,该 pane 的 `is_hook_enabled` 仍为
    /// true、`resolve_status` 仍认 hook 状态权威,而唯一能把它从 ai-working 拉回来
    /// 的收敛路径被挡在门外——黄灯从此永久卡死。
    pub fn with_server_lock<T>(&self, callback: impl FnOnce() -> T) -> T {
        let _guard = self.server.lock();
        callback()
    }
}

/// Notification 的结构化分类。
///
/// Claude Code 的 Notification 事件承载多种语义,payload 里的 `notification_type`
/// 直接给出类别(取值与官方 hooks 文档的 Notification matcher 同集)。此前只能靠
/// `message` 文案关键词猜,既怕本地化文案变动,也分不清「权限请求」与「闲置提醒」。
/// 现在以类型为准,类型缺失(旧版 Claude Code / Codex)时才回落文案匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationKind {
    /// 需要用户确认/授权 —— 点托盘黄灯
    Confirmation,
    /// 纯知会,不需要用户做什么(闲置提醒、认证成功、表单已提交…)
    Passive,
    /// 无类型字段,交给文案匹配
    Unknown,
}

fn classify_notification(notification_type: Option<&str>) -> NotificationKind {
    match notification_type {
        Some("permission_prompt") | Some("elicitation_dialog") | Some("agent_needs_input") => {
            NotificationKind::Confirmation
        }
        // `task_complete` 是 grok 的类型（回合做完时的知会）。它必须归 Passive:
        // 判 Confirmation 会让每次任务完成都点亮「有事等你确认」的黄灯,
        // 而真正的完成播报另有 Stop 事件负责。
        Some("idle_prompt") | Some("auth_success") | Some("elicitation_complete")
        | Some("elicitation_response") | Some("agent_completed") | Some("task_complete") => {
            NotificationKind::Passive
        }
        _ => NotificationKind::Unknown,
    }
}

/// 该 `Stop` 是否只是会话收尾时补发的那一发（grok 特有）。
///
/// grok 在会话结束时会额外发一次 `Stop`（`reason` 为 `channel_closed` /
/// `shutdown`），官方文档明说它的决策输出会被忽略、只是个观察点。若照常映射成
/// `ai-idle` + cause=`Stop`，UI 的 `isAiCompletion` 会认成「任务完成」——用户每次
/// 退出 grok 都白挨一次完成提示音，紧接着 SessionEnd 才把 pane 收到 idle。
/// Claude/Codex 的 Stop 不带 reason，判据对它们恒为假。
fn is_session_teardown_stop(event: &str, reason: Option<&str>) -> bool {
    event == "Stop" && matches!(reason, Some("channel_closed") | Some("shutdown"))
}

/// Notification 文案是否为「API 错误/自动重试中」类:此时 AI 仍在自己重试,
/// 属于工作中而非等待用户;若映射 ai-idle 会制造假的 working→idle 完成沿,
/// 连接异常期间提示音/闪烁反复误报「完成」。按已知文案特征识别,
/// 未匹配的一律按「需要用户注意」处理(ai-idle),漏匹配只是多响一声,不丢提醒。
///
/// 仅在 `notification_type` 缺失时才走这里:官方类型集里没有「错误重试」一类,
/// 类型已知就说明该通知属于权限/闲置/认证等已分类语义,不该再按文案当成重试。
fn is_retry_notification(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("retrying")
        || m.contains("api error")
        || m.contains("connection error")
        || m.contains("network error")
        || m.contains("overloaded")
        || m.contains("rate limit")
}

/// Notification 文案是否为「需要用户确认/授权」类(`notification_type` 缺失时的兜底)。
///
/// Claude Code 的 Notification 事件承载两类语义:权限请求("Claude needs your
/// permission to use …")与闲置提醒("Claude is waiting for your input",空闲
/// 60 秒触发)。后者不是待办事项 —— 若也标 attention,pane 只要闲置就点亮
/// 托盘黄灯,黄灯从「有事要确认」退化成「AI 没在跑」,失去信号价值。
/// 按权限类关键词白名单判定,未匹配(含无文案)一律不标 attention:
/// 真正的授权请求另有 PermissionRequest/Elicitation 事件兜底,漏匹配不丢黄灯。
fn is_confirmation_notification(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("permission")
        || m.contains("approv")
        || m.contains("authoriz")
        || m.contains("confirm")
        || m.contains("授权")
        || m.contains("确认")
        || m.contains("允许")
}

/// Notification 是否「需要用户确认」:类型优先,类型缺失才看文案。
fn notification_needs_confirmation(
    notification_type: Option<&str>,
    message: Option<&str>,
) -> bool {
    match classify_notification(notification_type) {
        NotificationKind::Confirmation => true,
        NotificationKind::Passive => false,
        NotificationKind::Unknown => message.is_some_and(is_confirmation_notification),
    }
}

/// 事件 → UI cause（hook 事件名，v0.9.3 起透传；UI 的 isAiCompletion 只认
/// `Stop`，attention 黄灯认 `PermissionRequest`/`Elicitation`/`StopFailure`）。
/// Notification 需细分后归一化：权限/确认类与真正的权限请求同义，归一化为
/// "PermissionRequest"（否则 UI 拿不到 notification_type/message，无法区分闲置
/// 提醒——闲置提醒不是待办，不该点黄灯）；重试类已映射 ai-working，原样透传。
fn event_cause<'a>(event: &'a str, notification_type: Option<&str>, message: Option<&str>) -> &'a str {
    if event == "Notification" && notification_needs_confirmation(notification_type, message) {
        "PermissionRequest"
    } else {
        event
    }
}

/// 该 `Stop` 是否是用户打断后的回合收尾（omp 上报 `reason: "aborted"`）。
///
/// omp 在 Esc/Ctrl+C 打断后仍会发 `agent_end`，扩展据 `stopReason == "aborted"`
/// 标成这样一发。它要把状态收到 ai-idle（回合确实结束了），但 cause 不能是 `Stop`
/// ——UI 的 isAiCompletion 会认成「任务完成」，每次打断都白响一声。cause 改成
/// `Interrupt`，与输入检测那条打断路径（`note_user_interrupt`）同名：两条路先后
/// 到达时 `emit_if_changed` 按「状态与 cause 都没变」去重，只发一次。
/// Claude/Codex 的 Stop 不带 reason，grok 的 reason 集里也没有 aborted，恒为假。
fn is_user_abort_stop(event: &str, reason: Option<&str>) -> bool {
    event == "Stop" && reason == Some("aborted")
}

/// 该 cause 是否表示「有事等你处理」——托盘黄灯的依据，同时也是
/// `StatusEmitter::emit_if_changed` 的去重豁免名单：黄灯的清除发生在 UI 侧
/// （用户对该 pane 键入即视为已在处理），去重表感知不到，若按「状态与 cause
/// 都没变」去重，同一轮内第二次授权请求 / 第二次 API 失败就会被吞掉。
/// (原为 `pub(crate)`:老架构里「点不点黄灯」由前端 TS 复算,迁移后 UI 在同一
/// 进程内,直接用本函数,免得再养一份会漂移的副本。)
pub fn is_attention_cause(cause: &str) -> bool {
    matches!(
        cause,
        "PermissionRequest" | "Elicitation" | "StopFailure"
    )
}

/// 将 hook 事件名映射为 PTY 状态
///
/// - ai-working: 表示 AI 正在处理（思考/工具调用/工具失败后的续处理/子代理/压缩/API 重试）
/// - ai-idle: 表示 AI 等待用户输入（停止/回合因 API 错误结束/权限请求/通知等）
/// - SessionEnd 单独处理（清除 hook 状态），不在此映射
fn map_event_to_status(
    event: &str,
    agent: Option<&str>,
    notification_type: Option<&str>,
    message: Option<&str>,
    reason: Option<&str>,
) -> Option<&'static str> {
    // grok 会话收尾补发的 Stop：不是完成，交给紧随其后的 SessionEnd 收状态
    if is_session_teardown_stop(event, reason) {
        return None;
    }
    // Codex 的 PermissionRequest 在审批 UI 弹出前触发，批准后直接执行工具，
    // 直到 PostToolUse 之前不再有任何 hook 事件。若映射为 ai-idle，批准后
    // 整个命令执行期间状态都会卡在 ai-idle，且审批弹出时误报"任务完成"，
    // 因此对 Codex 保持 ai-working（仍处于任务中）。
    if event == "PermissionRequest" && agent == Some("codex") {
        return Some("ai-working");
    }
    // API 错误/重试类 Notification:AI 还在自动重试,保持工作中。
    // 有结构化类型时不走文案匹配(官方类型集里没有「错误重试」一类)。
    if event == "Notification"
        && classify_notification(notification_type) == NotificationKind::Unknown
        && message.is_some_and(is_retry_notification)
    {
        return Some("ai-working");
    }
    match event {
        // ai-working 状态：AI 正在积极工作
        //
        // PostToolUseFailure / PostToolBatch / PermissionDenied / ElicitationResult
        // 是 v0.10.3 补上的事件空洞：工具失败、并行工具批收尾、auto 模式拒绝、
        // MCP 表单回填之后 AI 都还在跑，此前没有任何事件覆盖这些时刻，
        // 状态只能靠下一个 PreToolUse 才恢复。后两者还兼任黄灯的熄灭路径
        // （attention 随状态转 ai-working 一并清除）。
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
        | "PostToolBatch" | "PermissionDenied" | "ElicitationResult" | "SubagentStart"
        | "SubagentStop" | "PreCompact" | "PostCompact" => Some("ai-working"),
        // ai-idle 状态：AI 等待用户输入
        //
        // StopFailure = 回合因 API 错误结束。官方文档明确：`Stop` 不会在这种情况下
        // 触发（"API errors fire StopFailure instead"），此前不注册该事件，pane 会
        // 确定性地卡在 ai-working 直到下一轮对话——这是「Stop 丢失」最主要的来源，
        // 不是丢包。cause 保持 StopFailure：不是完成，不播报，但点黄灯提示要回来看。
        "SessionStart" | "Stop" | "StopFailure" | "PermissionRequest" | "Notification"
        | "Elicitation" => Some("ai-idle"),
        _ => None,
    }
}

/// 打断是否应当改写状态（`note_user_interrupt` 的纯判定部分，抽出来是为了可测：
/// 发射一侧需要 sink，单测里构造不出来）。
///
/// 两道闸：hook 未启用的 pane 走的是轮询降级路径，那条路径本就按输出活跃度给出
/// ai-idle，插手只会互相打架；当前状态不是 ai-working 说明没什么可打断的
/// （已经在等用户，或压根不在 AI 会话里，Esc 只是 shell/TUI 的普通按键）。
fn interrupt_should_settle(hook_state: &HookState, pty_id: u32) -> bool {
    hook_state.is_hook_enabled(pty_id)
        && hook_state.get_status(pty_id).as_deref() == Some("ai-working")
}

/// 用户按 Esc / 单次 Ctrl+C 打断 AI 时的状态收敛。
///
/// 官方文档：`Stop` hooks "don't fire on user interrupts" —— 打断不产生任何 hook
/// 事件，pane 会一直停在 ai-working，直到下一轮任务跑完才恢复。hook 侧无解，
/// 只能由输入检测补这一刀（`AiPerception::observe_input` 识别裸 Esc / Ctrl+C
/// 后调用）。
///
/// 与 v0.9.3 删掉的「输出活跃度兜底」的本质区别：那条是**无记忆**的，每 500ms
/// 用 hook_status 重算一次，降级结果不落盘，于是空闲期的零星伪输出把状态反复
/// 抬起再落下，每个下降沿都被 UI 当成一次完成播报。这里是一次性事件驱动，
/// 结果**写进 last_hook_status**，monitor 后续每轮重算得到同一个 ai-idle，
/// 不存在摆动，也就没有可供误判的下降沿。
///
/// 其余三重保险：只作用于 hook 已启用且当前正是 ai-working 的 pane（无 hook 的
/// pane 走轮询降级路径，不干预）；cause 用 `Interrupt` 而非 `Stop`，UI 的
/// `isAiCompletion` 认不出它，不会播报完成；误判（AI 其实还在跑，比如 Esc 只是
/// 关了个补全弹层）由下一个 hook 事件立刻纠正回 ai-working。
pub fn note_user_interrupt(
    hook_state: &HookState,
    emitter: &StatusEmitter,
    pty_id: u32,
    agent: Option<String>,
) {
    if !interrupt_should_settle(hook_state, pty_id) {
        return;
    }
    hook_state.update(pty_id, "ai-idle".to_string());
    emitter.emit_if_changed(pty_id, "ai-idle", Some("Interrupt"), agent);
    eprintln!("[hook-server] pty_id={} 用户打断 -> ai-idle", pty_id);
}

/// 启动 hook HTTP 服务器
///
/// 在后台线程监听，接收 hook 事件后通过 `emitter` 通知上层。
/// 端口从 DEFAULT_PORT 开始尝试，冲突时自动递增。
/// 返回 `Err` 表示无法绑定端口，调用方应将错误提示给用户。
///
/// `data_dir` 是端口文件(`hook-server.json`)的落地目录 —— 原实现走
/// `app.path().app_data_dir()`,迁移后改由调用方显式传入(hook 二进制自己按
/// 这个文件找端口,路径口径必须与旧版一致)。
pub fn start_hook_server(
    hook_state: HookState,
    emitter: StatusEmitter,
    tracker: SessionTracker,
    data_dir: PathBuf,
) -> Result<(), String> {
    // 如果已经在运行，不重复启动
    if hook_state.is_server_running() {
        eprintln!("[hook-server] 服务器已在运行，跳过启动");
        return Ok(());
    }

    // 在当前线程绑定端口，以便同步获取 server 实例
    let bound = {
        let mut result = None;
        for offset in 0..MAX_PORT_ATTEMPTS {
            let port = DEFAULT_PORT + offset;
            let addr = format!("127.0.0.1:{}", port);
            match tiny_http::Server::http(&addr) {
                Ok(s) => {
                    eprintln!("[hook-server] 监听 {}", addr);
                    hook_state.set_port(port);
                    result = Some((s, port));
                    break;
                }
                Err(e) => {
                    eprintln!("[hook-server] 端口 {} 被占用: {}", port, e);
                }
            }
        }
        result
    };

    let (server, port) = match bound {
        Some(s) => s,
        None => {
            eprintln!("[hook-server] 无法绑定任何端口，hook 服务器未启动");
            return Err("无法绑定端口 (23456-23460)，hook 服务器启动失败".to_string());
        }
    };

    // 用 Arc 包装 server，共享给 HookState 和监听线程
    let server = Arc::new(server);
    hook_state.set_server(Some(server.clone()));

    // 写入端口文件
    write_port_file(&data_dir, port);

    std::thread::spawn(move || {
        // 处理请求
        for mut request in server.incoming_requests() {
            if request.method() != &tiny_http::Method::Post {
                let response =
                    tiny_http::Response::from_string("Method Not Allowed").with_status_code(405);
                let _ = request.respond(response);
                continue;
            }

            let url = request.url().to_string();
            if url != "/hook" {
                let response = tiny_http::Response::from_string("Not Found").with_status_code(404);
                let _ = request.respond(response);
                continue;
            }

            // 读取 body。端点监听在 127.0.0.1 且无鉴权(改鉴权要动 sidecar 协议),
            // 同机任意进程都能 POST 过来,不封顶等于把内存交给对方:先看
            // Content-Length 拦掉声明超限的,再用 take() 兜住谎报/分块传输的情形。
            if request
                .body_length()
                .is_some_and(|n| n > MAX_HOOK_BODY_BYTES)
            {
                let response =
                    tiny_http::Response::from_string("Payload Too Large").with_status_code(413);
                let _ = request.respond(response);
                continue;
            }
            let mut body = String::new();
            // 多读一个字节:读满 MAX+1 说明真身超限,而不是刚好卡在上限
            let read = request
                .as_reader()
                .take(MAX_HOOK_BODY_BYTES as u64 + 1)
                .read_to_string(&mut body);
            if read.is_err() {
                let response =
                    tiny_http::Response::from_string("Bad Request").with_status_code(400);
                let _ = request.respond(response);
                continue;
            }
            if body.len() > MAX_HOOK_BODY_BYTES {
                eprintln!(
                    "[hook-server] body 超过 {} 字节上限，已拒绝",
                    MAX_HOOK_BODY_BYTES
                );
                let response =
                    tiny_http::Response::from_string("Payload Too Large").with_status_code(413);
                let _ = request.respond(response);
                continue;
            }

            // 解析 JSON payload
            let payload: HookPayload = match serde_json::from_str(&body) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[hook-server] JSON 解析失败: {}", e);
                    let response =
                        tiny_http::Response::from_string("Bad Request").with_status_code(400);
                    let _ = request.respond(response);
                    continue;
                }
            };

            // 立即响应 200，不阻塞 hook 脚本
            let response = tiny_http::Response::from_string("OK").with_status_code(200);
            let _ = request.respond(response);

            // 处理事件。事件名优先取 sidecar 从 argv 注入的 `event`；缺失时回落
            // payload 自带的 `hook_event_name`（用户手改配置漏写命令行参数的兜底，
            // 否则整条事件会因无事件名被静默丢弃）。
            let resolved_event = payload
                .event
                .clone()
                .or_else(|| payload.hook_event_name.clone());
            if let (Some(pty_id), Some(event)) = (payload.pty_id, resolved_event.as_deref()) {
                if event == "SessionEnd" {
                    // 只用 payload 自带的 session_id 打墓碑。不要退回 session_of:
                    // 新会话的 SessionStart 若先到,session_of 已是新会话,兜底会
                    // 把新会话误打进墓碑,冻结其全部后续事件。
                    if let Some(sid) = payload.session_id.clone() {
                        hook_state.mark_session_ended(pty_id, sid);
                    }
                    let was_last =
                        hook_state.end_session(pty_id, payload.session_id.as_deref());
                    if payload.reason.as_deref() == Some("clear") {
                        // /clear 换会话不是退出：紧随其后的 SessionStart 会带新
                        // session id 刷新状态，这里只靠墓碑挡住旧会话的迟到事件
                        eprintln!(
                            "[hook-server] pty_id={} event=SessionEnd(clear) -> 仅记录墓碑",
                            pty_id
                        );
                    } else if !was_last {
                        // pane 上还有别的活跃会话:嵌套非交互实例(Bash 工具里跑
                        // `claude -p` / `codex exec`,继承 MINITERM_PTY_ID)结束,
                        // 或退出后立刻重开、新 SessionStart 先到的乱序。此时清
                        // hook 状态 / AI 会话标记会误杀仍在跑的会话,只留墓碑。
                        eprintln!(
                            "[hook-server] pty_id={} event=SessionEnd 非最后活跃会话,仅记录墓碑",
                            pty_id
                        );
                    } else {
                        // 最后一个活跃会话结束 → 权威退出信号：清 hook 状态回退
                        // 到轮询，同时清输入检测的 AI 会话标记——双击 Ctrl+C
                        // 间隔超窗漏检时靠这里自愈
                        hook_state.remove(pty_id);
                        tracker.clear_ai_session(pty_id);
                        emitter.emit_if_changed(pty_id, "idle", Some("SessionEnd"), None);
                        eprintln!(
                            "[hook-server] pty_id={} event=SessionEnd(reason={:?}) -> hook 已清除，回退到 idle",
                            pty_id, payload.reason
                        );
                    }
                } else {
                    // 已结束会话的迟到事件直接丢弃：hook 脚本是独立进程，
                    // POST 到达顺序无保证，放行会把退出后的 pane 推回 ai-idle。
                    // 例外:SessionStart 是会话再次存活的肯定证据(退出后 claude -c /
                    // --resume 同 id 重开),复活墓碑走正常记录,否则该会话被永久忽略
                    if let Some(sid) = payload.session_id.as_deref() {
                        if event == "SessionStart" {
                            hook_state.revive_session(pty_id, sid);
                        } else if hook_state.is_session_ended(pty_id, sid) {
                            eprintln!(
                                "[hook-server] pty_id={} event={} 来自已结束会话 {}，忽略",
                                pty_id, event, sid
                            );
                            continue;
                        }
                    }
                    // 会话身份先于状态映射记录:即使事件不映射状态(如未知事件),
                    // session_id 也是有效信息;/clear 换会话时靠这里自动刷新
                    if let Some(sid) = payload.session_id.clone() {
                        hook_state.note_session_active(pty_id, &sid);
                        if hook_state.record_session(pty_id, payload.agent.clone(), sid.clone()) {
                            // hook 端点无鉴权,cwd 是任何本地进程都能塞的字段,而上层会
                            // 把它持久化成「未来某次 PTY 的启动目录」。只放行确实存在的
                            // 目录:构造出来的假路径与已被删掉的 worktree 一并挡在这里,
                            // 续接时回落 pane 自己的 cwd,而不是让 create_pty 直接失败。
                            let cwd = payload
                                .cwd
                                .clone()
                                .filter(|p| std::path::Path::new(p).is_dir());
                            // 会话身份变化(新会话/换会话)时通知上层,供布局持久化
                            // 记录「退出时该 pane 正跑着哪个 AI 会话」以便重启续接
                            emitter.notify_session_identity(SessionIdentity {
                                pty_id,
                                agent: payload.agent.clone(),
                                session_id: sid,
                                cwd,
                            });
                        }
                    }
                    let mapped = map_event_to_status(
                        event,
                        payload.agent.as_deref(),
                        payload.notification_type.as_deref(),
                        payload.message.as_deref(),
                        payload.reason.as_deref(),
                    );
                    if let Some(status) = mapped {
                        // hook 事件是 AI 进程存活的直接证据:输入检测漏判启动
                        // (别名/包装脚本)或误判退出(任务中双击 Ctrl+C)时,
                        // 靠这里把 AI 会话标记扶正,保住后续 marker/移动端语义
                        tracker.mark_ai_session(
                            pty_id,
                            payload.agent.as_deref().unwrap_or("claude"),
                        );
                        hook_state.update(pty_id, status.to_string());

                        // 通知上层（与 monitor 共享同一份去重表）。cause 带
                        // 归一化后的事件名:Stop/PermissionRequest/Notification 都落
                        // ai-idle,但只有 Stop 是「任务做完了」,UI 据此决定播报与
                        // 托盘黄绿灯（见 utils/aiCompletion.ts 与 store 的 attention 判定）
                        let cause = if is_user_abort_stop(event, payload.reason.as_deref()) {
                            "Interrupt"
                        } else {
                            event_cause(
                                event,
                                payload.notification_type.as_deref(),
                                payload.message.as_deref(),
                            )
                        };
                        emitter.emit_if_changed(pty_id, status, Some(cause), payload.agent.clone());

                        eprintln!(
                            "[hook-server] pty_id={} event={} agent={:?} -> status={} cause={}{}",
                            pty_id,
                            event,
                            payload.agent,
                            status,
                            cause,
                            payload
                                .error_type
                                .as_deref()
                                .map(|e| format!(" error_type={}", e))
                                .unwrap_or_default()
                        );
                    } else {
                        // 未映射事件：新版 Claude Code / Codex 加了事件而这里还没跟上时
                        // 的可见信号（注册列表是白名单，正常不会有陌生事件抵达）
                        eprintln!(
                            "[hook-server] pty_id={} event={} 无状态映射，已忽略",
                            pty_id, event
                        );
                    }
                }
            }
        }
    });

    Ok(())
}

/// 停止 hook HTTP 服务器
///
/// 取出保存的 server 实例，调用 `unblock()` 中断阻塞循环，
/// 清理端口文件并重置端口。
pub fn stop_hook_server(hook_state: &HookState, data_dir: &Path) {
    let server = hook_state.server.lock().take();
    if let Some(s) = server {
        s.unblock();
        eprintln!("[hook-server] 服务器已停止");
    }
    hook_state.set_port(0);
    // 清理端口文件
    delete_port_file(data_dir);
}

/// 运行时切换 hook server 开关(原 `toggle_hook_server` Tauri command)
pub fn set_hook_server_enabled(
    hook_state: &HookState,
    emitter: &StatusEmitter,
    tracker: &SessionTracker,
    data_dir: &Path,
    enabled: bool,
) -> Result<(), String> {
    if enabled {
        if !hook_state.is_server_running() {
            start_hook_server(
                hook_state.clone(),
                emitter.clone(),
                tracker.clone(),
                data_dir.to_path_buf(),
            )?;
        }
    } else if hook_state.is_server_running() {
        stop_hook_server(hook_state, data_dir);
    }
    Ok(())
}

/// 当前 hook 服务状态(原 `get_hook_status` Tauri command)
pub fn hook_status(hook_state: &HookState) -> HookStatusInfo {
    HookStatusInfo {
        port: hook_state.get_port(),
        running: hook_state.is_server_running(),
    }
}

/// 将端口信息写入 `{data_dir}/hook-server.json`
fn write_port_file(dir: &Path, port: u16) {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("hook-server.json");
    let content = format!("{{\"port\":{}}}", port);
    if let Err(e) = crate::util::atomic_write(&path, content.as_bytes()) {
        eprintln!("[hook-server] 写入端口文件失败 {}: {}", path.display(), e);
    } else {
        eprintln!("[hook-server] 端口文件已写入 {}", path.display());
    }
}

/// 删除端口文件 `{data_dir}/hook-server.json`
fn delete_port_file(dir: &Path) {
    let path = dir.join("hook-server.json");
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!("[hook-server] 删除端口文件失败 {}: {}", path.display(), e);
        } else {
            eprintln!("[hook-server] 端口文件已删除 {}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// 回归测试:server 没运行时回调**照样执行**。
    ///
    /// 唯一的调用方是停摆兜底,它的判据是 pane 自己的 hook 记录。此前这里 gate 了
    /// `server.as_ref()?`,于是 AI 跑着时关掉 hook 开关 → `is_hook_enabled` 仍为
    /// true、`resolve_status` 仍认 hook 状态权威、收敛却被跳过 → 黄灯永久卡死。
    #[test]
    fn server_lock_runs_callback_even_when_server_stopped() {
        let state = HookState::new();
        assert!(!state.is_server_running());

        let mut ran = false;
        state.with_server_lock(|| ran = true);
        assert!(ran, "server 未运行时回调也必须执行");
    }

    #[test]
    fn server_lock_blocks_stop_until_callback_finishes() {
        let state = HookState::new();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        state.set_server(Some(Arc::new(server)));

        let guarded_state = state.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            guarded_state.with_server_lock(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
        });

        entered_rx.recv().unwrap();
        let stopping_state = state.clone();
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let stopper = thread::spawn(move || {
            stopping_state.set_server(None);
            stopped_tx.send(()).unwrap();
        });

        assert!(stopped_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release_tx.send(()).unwrap();
        stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
        stopper.join().unwrap();
        assert!(!state.is_server_running());
    }

    #[test]
    fn hook_state_records_and_clears_session_identity() {
        let state = HookState::new();
        assert!(state.session_of(1).is_none());

        state.record_session(1, Some("claude-code".into()), "sid-a".into());
        let s = state.session_of(1).unwrap();
        assert_eq!(s.session_id, "sid-a");
        assert_eq!(s.agent.as_deref(), Some("claude-code"));

        // /clear 换会话:同 pty 覆盖为新 id
        state.record_session(1, Some("claude-code".into()), "sid-b".into());
        assert_eq!(state.session_of(1).unwrap().session_id, "sid-b");

        // SessionEnd / PTY 关闭走 remove:会话身份一并清除
        state.remove(1);
        assert!(state.session_of(1).is_none());
    }

    #[test]
    fn end_session_last_active_triggers_teardown() {
        let state = HookState::new();
        // 从未见过任何会话:保守按"最后一个"处理(执行销毁,对齐旧行为)
        assert!(state.end_session(1, Some("sid-a")));

        // 正常生命周期:唯一活跃会话结束 -> 销毁
        state.note_session_active(1, "sid-a");
        assert!(state.end_session(1, Some("sid-a")));
    }

    #[test]
    fn nested_session_end_keeps_outer_alive() {
        let state = HookState::new();
        // 外层交互会话 A 活跃中,嵌套非交互实例 B(claude -p)启动又结束
        state.note_session_active(1, "sid-outer");
        state.note_session_active(1, "sid-nested");
        assert!(!state.end_session(1, Some("sid-nested"))); // 不销毁:A 还在
        assert!(state.end_session(1, Some("sid-outer"))); // A 退出才销毁
    }

    #[test]
    fn exit_restart_race_skips_teardown() {
        let state = HookState::new();
        // 退出后立刻重开:新会话 B 的 SessionStart 先到,旧会话 A 的 SessionEnd 迟到
        state.note_session_active(1, "sid-a");
        state.note_session_active(1, "sid-b");
        assert!(!state.end_session(1, Some("sid-a"))); // B 活跃,不销毁
    }

    #[test]
    fn end_session_unknown_sid_respects_remaining_active() {
        let state = HookState::new();
        state.note_session_active(1, "sid-a");
        // 未知会话结束(其 Start 早于 hook server 启用):A 仍活跃,不销毁
        assert!(!state.end_session(1, Some("sid-x")));
        // payload 无 session_id:按剩余活跃集判断
        assert!(!state.end_session(1, None));
        assert!(state.end_session(1, Some("sid-a")));
    }

    #[test]
    fn note_session_active_dedup_and_cap() {
        let state = HookState::new();
        // 重复 note 去重,不占额外容量
        state.note_session_active(1, "sid-0");
        state.note_session_active(1, "sid-0");
        // 再 note sid-1..sid-CAP,溢出一格 → 最老的 sid-0 被挤出
        for i in 1..ACTIVE_SESSIONS_CAP + 1 {
            state.note_session_active(1, &format!("sid-{}", i));
        }
        // 结束 sid-1..sid-(CAP-1):每次集合都还非空
        for i in 1..ACTIVE_SESSIONS_CAP {
            assert!(!state.end_session(1, Some(&format!("sid-{}", i))));
        }
        // 结束最后一个成员即空——证明 sid-0 确实已被挤出(否则此处非空)
        assert!(state.end_session(1, Some(&format!("sid-{}", ACTIVE_SESSIONS_CAP))));
    }

    #[test]
    fn purge_clears_active_sessions() {
        let state = HookState::new();
        state.note_session_active(1, "sid-a");
        state.purge(1);
        // purge 后无残留:未知 sid 结束按空集处理
        assert!(state.end_session(1, Some("sid-b")));
    }

    #[test]
    fn tombstone_blocks_ended_session() {
        let state = HookState::new();
        assert!(!state.is_session_ended(1, "sid-a"));
        state.mark_session_ended(1, "sid-a".into());
        assert!(state.is_session_ended(1, "sid-a"));
        // 其他会话 / 其他 pty 不受影响
        assert!(!state.is_session_ended(1, "sid-b"));
        assert!(!state.is_session_ended(2, "sid-a"));
    }

    #[test]
    fn tombstone_survives_remove_cleared_by_purge() {
        let state = HookState::new();
        state.update(1, "ai-idle".into());
        state.mark_session_ended(1, "sid-a".into());

        // SessionEnd 路径:先打墓碑再 remove,墓碑必须存活
        state.remove(1);
        assert!(!state.is_hook_enabled(1));
        assert!(state.is_session_ended(1, "sid-a"));

        // PTY 关闭走 purge:墓碑一并清理
        state.purge(1);
        assert!(!state.is_session_ended(1, "sid-a"));
    }

    #[test]
    fn tombstone_capped_and_deduped() {
        let state = HookState::new();
        // 重复打墓碑不占额外容量
        state.mark_session_ended(1, "sid-0".into());
        state.mark_session_ended(1, "sid-0".into());
        for i in 1..ENDED_SESSIONS_CAP + 2 {
            state.mark_session_ended(1, format!("sid-{}", i));
        }
        // 超容量后最老的被挤出,最新的保留
        assert!(!state.is_session_ended(1, "sid-0"));
        assert!(state.is_session_ended(1, &format!("sid-{}", ENDED_SESSIONS_CAP + 1)));
    }

    /// 测试便捷包装:多数用例不关心 notification_type / reason
    fn map_event(event: &str, agent: Option<&str>, message: Option<&str>) -> Option<&'static str> {
        map_event_to_status(event, agent, None, message, None)
    }

    #[test]
    fn codex_permission_request_maps_to_ai_working() {
        assert_eq!(
            map_event("PermissionRequest", Some("codex"), None),
            Some("ai-working")
        );
    }

    #[test]
    fn claude_permission_request_keeps_ai_idle() {
        assert_eq!(
            map_event("PermissionRequest", Some("claude-code"), None),
            Some("ai-idle")
        );
        // agent 字段缺失时保持原有行为
        assert_eq!(map_event("PermissionRequest", None, None), Some("ai-idle"));
    }

    #[test]
    fn other_events_unaffected_by_agent() {
        assert_eq!(map_event("Stop", Some("codex"), None), Some("ai-idle"));
        assert_eq!(
            map_event("PreToolUse", Some("codex"), None),
            Some("ai-working")
        );
        assert_eq!(map_event("Unknown", Some("codex"), None), None);
    }

    /// 回合因 API 错误结束:官方文档明确此时 `Stop` 不触发,只有 StopFailure。
    /// 不映射它,pane 会确定性地卡在 ai-working 直到下一轮对话。
    #[test]
    fn stop_failure_falls_back_to_ai_idle() {
        assert_eq!(map_event("StopFailure", Some("claude-code"), None), Some("ai-idle"));
        // 但它不是「完成」:cause 必须原样透传,UI 的 isAiCompletion 只认 Stop
        assert_eq!(event_cause("StopFailure", None, None), "StopFailure");
        // 需要用户回来重发 → 走 attention 黄灯
        assert!(is_attention_cause("StopFailure"));
    }

    /// 补上的事件空洞:工具失败 / 并行工具批收尾 / auto 模式拒绝 / MCP 表单回填
    /// 之后 AI 都还在跑,此前没有任何事件覆盖这些时刻。
    #[test]
    fn newly_covered_events_map_to_ai_working() {
        for event in [
            "PostToolUseFailure",
            "PostToolBatch",
            "PermissionDenied",
            "ElicitationResult",
        ] {
            assert_eq!(
                map_event(event, Some("claude-code"), None),
                Some("ai-working"),
                "{event} 应视为 AI 仍在工作"
            );
        }
    }

    /// PermissionDenied / ElicitationResult 是黄灯的熄灭路径:状态转 ai-working
    /// 时 UI 把 attention 清掉,它们自身不得再被算作 attention。
    #[test]
    fn permission_resolution_events_are_not_attention() {
        assert!(!is_attention_cause("PermissionDenied"));
        assert!(!is_attention_cause("ElicitationResult"));
        assert!(!is_attention_cause("Stop"));
        assert!(!is_attention_cause("Interrupt"));
        assert!(is_attention_cause("PermissionRequest"));
        assert!(is_attention_cause("Elicitation"));
    }

    /// notification_type 优先于文案:结构化类型在,就不再猜关键词
    #[test]
    fn notification_type_overrides_message_heuristics() {
        // 闲置提醒即便文案里带「确认」字样也不点黄灯
        assert_eq!(
            event_cause("Notification", Some("idle_prompt"), Some("请确认下一步")),
            "Notification"
        );
        // 权限请求即便文案是本地化的、关键词全不匹配,也照样归一化点黄灯
        assert_eq!(
            event_cause("Notification", Some("permission_prompt"), Some("Bash ツールの実行")),
            "PermissionRequest"
        );
        // MCP 表单打开 → 黄灯;表单已提交 → 不再是待办
        assert_eq!(
            event_cause("Notification", Some("elicitation_dialog"), None),
            "PermissionRequest"
        );
        assert_eq!(
            event_cause("Notification", Some("elicitation_complete"), None),
            "Notification"
        );
    }

    /// 用户打断:AI 正在跑时按 Esc/Ctrl+C,状态收敛到 ai-idle。
    #[test]
    fn interrupt_settles_running_ai() {
        let state = HookState::new();
        state.update(1, "ai-working".to_string());
        assert!(interrupt_should_settle(&state, 1));
    }

    /// 打断的结果必须**落盘**到 hook 状态——这正是它与 v0.9.3 删掉的
    /// 「输出活跃度兜底」的分水岭:那条不落盘,每 500ms 重算一次,于是状态在
    /// working/idle 之间反复摆动,每个下降沿都被 UI 当成一次完成播报。
    /// 这里改完 last_hook_status 后第二次打断不再满足条件,不会重复发射。
    #[test]
    fn interrupt_result_is_persisted_and_not_repeated() {
        let state = HookState::new();
        state.update(1, "ai-working".to_string());
        assert!(interrupt_should_settle(&state, 1));

        state.update(1, "ai-idle".to_string()); // note_user_interrupt 的落盘动作
        assert_eq!(state.get_status(1).as_deref(), Some("ai-idle"));
        assert!(
            !interrupt_should_settle(&state, 1),
            "已收敛的 pane 再次打断不应重复改写状态"
        );
    }

    /// 端到端走一遍注入版 `note_user_interrupt`:落盘 + 只发一条 Interrupt。
    #[test]
    fn note_user_interrupt_latches_and_emits_once() {
        use crate::monitor::{StatusChange, StatusEmitter};
        use std::sync::Mutex as StdMutex;

        let seen: Arc<StdMutex<Vec<(String, Option<String>)>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let emitter = StatusEmitter::new(Arc::new(move |c: StatusChange| {
            sink_seen.lock().unwrap().push((c.status, c.cause));
        }));
        let state = HookState::new();
        state.update(1, "ai-working".to_string());

        note_user_interrupt(&state, &emitter, 1, Some("claude".into()));
        note_user_interrupt(&state, &emitter, 1, Some("claude".into()));

        assert_eq!(state.get_status(1).as_deref(), Some("ai-idle"), "结论必须落盘");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("ai-idle".to_string(), Some("Interrupt".to_string()))],
            "只应发一条,且 cause 不是 Stop(不算完成)"
        );
    }

    /// 不越界的三种情形:hook 未启用(走轮询降级路径)、已在等用户、陌生 pty。
    #[test]
    fn interrupt_leaves_non_working_panes_alone() {
        let state = HookState::new();
        assert!(!interrupt_should_settle(&state, 1), "陌生 pty 不该被改写");

        state.update(2, "ai-idle".to_string());
        assert!(!interrupt_should_settle(&state, 2), "已在等用户,无事可打断");

        // hook 从未启用的 pane(WSL/SSH/hook 关闭):即便 Esc 也不插手
        let bare = HookState::new();
        assert!(!interrupt_should_settle(&bare, 3));
    }

    /// 类型已知时不再按文案判重试:官方类型集里没有「错误重试」一类,
    /// 权限文案里恰好出现 "rate limit" 之类的字样不该把状态推成 ai-working。
    #[test]
    fn known_notification_type_skips_retry_heuristics() {
        assert_eq!(
            map_event_to_status(
                "Notification",
                Some("claude-code"),
                Some("permission_prompt"),
                Some("Allow Bash to run `gh api --rate-limit`?"),
                None,
            ),
            Some("ai-idle")
        );
        // 类型缺失(旧版 Claude Code)时文案兜底照旧生效
        assert_eq!(
            map_event_to_status(
                "Notification",
                Some("claude-code"),
                None,
                Some("API Error: 529 Overloaded · Retrying…"),
                None,
            ),
            Some("ai-working")
        );
    }

    #[test]
    fn retry_notification_keeps_ai_working() {
        // API 错误/重试类文案:AI 仍在自动重试,不产生假完成沿
        for msg in [
            "API Error (Request timed out.) · Retrying in 1 seconds… (attempt 1/10)",
            "Connection error, retrying...",
            "API Error: 529 Overloaded",
            "Rate limit reached",
        ] {
            assert_eq!(
                map_event("Notification", Some("claude-code"), Some(msg)),
                Some("ai-working"),
                "误判为 idle: {msg}"
            );
        }
    }

    #[test]
    fn attention_notification_maps_to_ai_idle() {
        // 需要授权/等待输入类文案:保持提醒行为
        for msg in [
            "Claude needs your permission to use Bash",
            "Claude is waiting for your input",
        ] {
            assert_eq!(
                map_event("Notification", Some("claude-code"), Some(msg)),
                Some("ai-idle"),
                "误判为 working: {msg}"
            );
        }
        // 无 message 时保持原有行为
        assert_eq!(
            map_event("Notification", Some("claude-code"), None),
            Some("ai-idle")
        );
    }

    #[test]
    fn permission_notification_normalized_to_permission_request() {
        // 权限/确认类文案:与真正的权限请求同义,归一化后 UI 才点得了黄灯
        for msg in [
            "Claude needs your permission to use Bash",
            "Waiting for your approval to run the command",
        ] {
            assert_eq!(
                event_cause("Notification", None, Some(msg)),
                "PermissionRequest",
                "该亮黄灯没亮: {msg}"
            );
        }
    }

    #[test]
    fn idle_reminder_notification_keeps_event_name() {
        // 闲置提醒不是待办:保持 Notification 原名,UI 不点黄灯也不算完成
        assert_eq!(
            event_cause("Notification", None, Some("Claude is waiting for your input")),
            "Notification"
        );
        // 无文案无从判定,保守不归一化(真授权另有 PermissionRequest/Elicitation 兜底)
        assert_eq!(event_cause("Notification", None, None), "Notification");
    }

    #[test]
    fn non_notification_events_pass_through() {
        // 事件名原样透传:UI 的 isAiCompletion 只认 Stop,黄灯认 PermissionRequest/Elicitation
        assert_eq!(event_cause("PermissionRequest", None, None), "PermissionRequest");
        assert_eq!(event_cause("Elicitation", None, None), "Elicitation");
        assert_eq!(event_cause("Stop", None, None), "Stop");
        assert_eq!(event_cause("UserPromptSubmit", None, None), "UserPromptSubmit");
    }

    // ---- oh-my-pi(omp)特有语义 ----

    /// omp 打断后的回合收尾(`Stop` + `reason: aborted`):状态收到 ai-idle,但 cause
    /// 是 `Interrupt` 不是 `Stop` —— 否则每次 Esc 打断都播一次「完成」。
    /// 与输入检测那条打断路径同名,两条路先后到达时按去重只发一次。
    #[test]
    fn omp_aborted_stop_is_an_interrupt_not_a_completion() {
        assert!(is_user_abort_stop("Stop", Some("aborted")));
        assert_eq!(
            map_event_to_status("Stop", Some("omp"), None, None, Some("aborted")),
            Some("ai-idle"),
            "回合确实结束了,状态要收到 ai-idle"
        );
        assert!(!is_attention_cause("Interrupt"), "打断不点黄灯");
        // 不是别家的 Stop:无 reason / grok 的收尾 reason / SessionEnd 都不算
        assert!(!is_user_abort_stop("Stop", None));
        assert!(!is_user_abort_stop("Stop", Some("end_turn")));
        assert!(!is_user_abort_stop("SessionEnd", Some("aborted")));
        assert!(!is_user_abort_stop("StopFailure", Some("aborted")));
    }

    /// omp 扩展会上报的每个事件名都必须有状态映射(`SessionEnd` 走独立分支),
    /// 漏一个就是一段状态空洞。表由 hook_registry 维护,与扩展模板逐条对账。
    #[test]
    fn omp_reported_event_set_is_fully_mapped() {
        for event in crate::hook_registry::OMP_REPORTED_EVENTS {
            if *event == "SessionEnd" {
                continue;
            }
            assert!(
                map_event(event, Some("omp"), None).is_some(),
                "{event} 无状态映射"
            );
        }
        // 提问工具映射成 Elicitation:等作答期间点黄灯,作答后熄灭
        assert_eq!(map_event("Elicitation", Some("omp"), None), Some("ai-idle"));
        assert!(is_attention_cause(event_cause("Elicitation", None, None)));
        assert_eq!(
            map_event("ElicitationResult", Some("omp"), None),
            Some("ai-working")
        );
        // 自动重试的 Notification 靠文案里的 retrying 判为「仍在工作」
        assert_eq!(
            map_event(
                "Notification",
                Some("omp"),
                Some("API error, retrying (attempt 1/3): 529 overloaded")
            ),
            Some("ai-working")
        );
        // omp 的 PermissionRequest 走 Claude 语义(ai-idle + 黄灯),不是 Codex 那条特例
        assert_eq!(
            map_event("PermissionRequest", Some("omp"), None),
            Some("ai-idle")
        );
    }

    // ---- Grok Build 特有语义 ----

    /// grok 没有 PermissionRequest 事件,「等待授权」只从 Notification 的
    /// `permission_prompt` 类型认出来——这是它唯一的黄灯来源。
    #[test]
    fn grok_permission_prompt_lights_the_attention_lamp() {
        assert_eq!(
            event_cause("Notification", Some("permission_prompt"), None),
            "PermissionRequest"
        );
        assert!(is_attention_cause(event_cause(
            "Notification",
            Some("permission_prompt"),
            None
        )));
    }

    /// grok 的 `task_complete` 是「回合做完了」的知会,不是待办:归 Passive,
    /// 否则每完成一次任务就点一盏「有事等你确认」的黄灯。
    #[test]
    fn grok_task_complete_notification_is_passive() {
        assert_eq!(
            classify_notification(Some("task_complete")),
            NotificationKind::Passive
        );
        assert_eq!(
            event_cause("Notification", Some("task_complete"), None),
            "Notification"
        );
        assert!(!is_attention_cause("Notification"));
    }

    /// 会话收尾补发的 Stop 不得被当成任务完成(否则每次退出 grok 都白响一声)。
    /// 收状态交给紧随其后的 SessionEnd。
    #[test]
    fn grok_teardown_stop_is_not_a_completion() {
        for reason in ["channel_closed", "shutdown"] {
            assert!(is_session_teardown_stop("Stop", Some(reason)));
            assert_eq!(
                map_event_to_status("Stop", Some("grok"), None, None, Some(reason)),
                None,
                "reason={reason} 的 Stop 不该改写状态"
            );
        }
        // 正常回合结束的 Stop 照旧映射 ai-idle 并被 UI 认作完成
        assert_eq!(
            map_event_to_status("Stop", Some("grok"), None, None, Some("end_turn")),
            Some("ai-idle")
        );
        // Claude/Codex 的 Stop 不带 reason,判据对它们恒为假
        assert!(!is_session_teardown_stop("Stop", None));
        assert_eq!(
            map_event_to_status("Stop", Some("claude-code"), None, None, None),
            Some("ai-idle")
        );
        // SessionEnd 自带的 reason 不能被这条规则误伤(它走的是另一条分支)
        assert!(!is_session_teardown_stop("SessionEnd", Some("shutdown")));
    }

    /// grok 会用到的其余事件都必须有状态映射,漏一个就是一段状态空洞
    #[test]
    fn grok_event_set_is_fully_mapped() {
        for (event, expected) in [
            ("SessionStart", "ai-idle"),
            ("UserPromptSubmit", "ai-working"),
            ("PreToolUse", "ai-working"),
            ("PostToolUse", "ai-working"),
            ("PostToolUseFailure", "ai-working"),
            ("PermissionDenied", "ai-working"),
            ("SubagentStart", "ai-working"),
            ("SubagentStop", "ai-working"),
            ("PreCompact", "ai-working"),
            ("PostCompact", "ai-working"),
            ("Stop", "ai-idle"),
            ("StopFailure", "ai-idle"),
            ("Notification", "ai-idle"),
        ] {
            assert_eq!(
                map_event(event, Some("grok"), None),
                Some(expected),
                "{event} 无状态映射"
            );
        }
    }
}
