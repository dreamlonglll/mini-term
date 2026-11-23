//! Hook HTTP 服务器模块
//!
//! 在后台线程监听 `127.0.0.1` 的 HTTP 请求，接收 Claude Code / Codex / Grok /
//! oh-my-pi / pi / Antigravity CLI(agy)的 hook 事件上报，并通过注入的
//! [`StatusEmitter`] 通知上层。agy 的事件先过一层归一化（`route_agy_event`）。
//!
//! **端口 / 协议 / 路由一个字都不能改**:各家 CLI 里已注册在用户机器上的 hook
//! 命令（与 omp / pi 的进程内扩展）按当前形态 POST 过来,改了等于让存量用户的 AI 感知
//! 集体失灵。

use crate::agent::AgentKind;
use crate::monitor::{SessionIdentity, StatusEmitter};
use crate::tracker::SessionTracker;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

// hook HTTP 线程与 GPUI 主线程、500ms 轮询线程共享这几张表:std::sync::Mutex 一旦
// 有持锁者 panic 就整把锁中毒,主线程下一次 lock 跟着 panic 整个进程就没了。
// parking_lot 没有中毒概念(与 mt-app/mt-pty/mt-terminal 同款)。
use parking_lot::{Condvar, Mutex};

/// 默认监听端口
const DEFAULT_PORT: u16 = 23456;
/// 端口冲突时最多尝试的端口数
const MAX_PORT_ATTEMPTS: u16 = 5;
/// 每个 PTY 保留的已结束会话墓碑数量上限
const ENDED_SESSIONS_CAP: usize = 8;
/// 每个 PTY 跟踪的活跃会话数量上限（正常只有 1 个；嵌套非交互实例/事件乱序
/// 时短暂多个，上限只是防御事件丢失导致的累积）
const ACTIVE_SESSIONS_CAP: usize = 8;
/// 每个 pane 记多少个在跑的 agy 子代理会话(正常是个位数;上限只防灌假 id)
const AGY_SUBAGENTS_CAP: usize = 64;
/// 单个 hook 请求 body 的字节上限。真实 payload 只有几百字节（各家 CLI 发的都是
/// 一小段 JSON），1 MiB 已宽出三个数量级；超限直接 413，不进 JSON 解析。
const MAX_HOOK_BODY_BYTES: usize = 1024 * 1024;
/// 请求行 + 全部请求头的总字节上限，防御畸形/慢速无限请求头耗尽内存。
const MAX_HOOK_HEADER_BYTES: usize = 64 * 1024;
/// 单个 TCP 连接读取请求头与 body 的总超时（本机回环正常请求不足 1ms）。
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// 单个 TCP 连接写回响应的超时。
const CONNECTION_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// 处理 hook 请求的最大并发 worker 线程数：少量挂起的慢连接不会卡死正常上报，
/// 同时上限封住本地端口被并发灌爆时的线程开销。
const MAX_CONCURRENT_WORKERS: usize = 8;
/// worker 队列的最大待处理连接数，满载时由 accept 线程直接丢弃，避免阻塞 accept/stop。
const MAX_QUEUED_CONNECTIONS: usize = 32;
/// 空闲 worker 线程的存活时间：空闲时自然回收，不常驻多余线程。
const WORKER_IDLE_TTL: Duration = Duration::from_secs(30);
/// Hook 事件的 JSON payload
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // 保留完整字段供未来 UI 细化使用
pub struct HookPayload {
    /// PTY ID（由 MINITERM_PTY_ID 环境变量传递）
    pub pty_id: Option<u32>,
    /// 事件名（如 UserPromptSubmit, PreToolUse 等）
    pub event: Option<String>,
    /// 来源 agent（claude-code / codex / grok / omp / pi / agy）
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
    /// agy `Stop` 的结束原因(`NO_TOOL_CALL` / `USER_CANCELED` / `ERROR` / …),sidecar 由
    /// `terminationReason` 改名转发。只有 agy 发,见 `route_agy_event`。
    pub termination_reason: Option<String>,
    /// agy `Stop` 的 `fullyIdle`:`false` = 主会话在等子代理,这次 Stop 不是完成。
    pub fully_idle: Option<bool>,
    /// agy 事件的 `lastUserInput` 是否非空(正文不经 sidecar 转发,只留这一位)。
    /// `false` = 子代理会话的事件(实测子代理的 `lastUserInput` 恒为空串)。
    pub has_user_input: Option<bool>,
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

/// 终端内带内状态上报（OSC 7501 Program Status Protocol）的核心状态值。
///
/// 定义在 `mt-ai` 以保持本 crate 不依赖 `mt-terminal`；宿主在输出旁路做一层零开销映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscProgramState {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
    Clear,
}

/// OSC 7501 `state=blocked` 时的阻塞细分类型（`kind=`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscBlockedKind {
    Permission,
    Question,
    Auth,
    Other,
}

/// 一条从 PTY 输出流解析出的 OSC 7501 程序状态上报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscStatus {
    pub state: OscProgramState,
    pub kind: Option<OscBlockedKind>,
    pub app: Option<String>,
    pub id: Option<String>,
    pub msg: Option<String>,
}

/// 用户触发退出（`/exit` / 双击 Ctrl+D / 双击 Ctrl+C）后屏蔽 `state=idle` 的窗口：
/// agy 进程退出瞬间会在约 100ms 内再吐一次 `state=idle`（1.5.14 实测），若用它扶正
/// 会话或刷新 `last_hook_time`，10s 退出兜底就会被挡住。强活体状态（working/blocked/
/// done/error）不受本窗口限制，立刻扶正。
const EXIT_IDLE_IGNORE_WINDOW: Duration = Duration::from_secs(3);

/// 合并在单把锁下的全部按 PTY 索引的 hook 状态表。
///
/// 此前 8 张表各自持一把 `Arc<Mutex<...>>`，跨表读写（`update` / `remove` / `purge` /
/// `agy_subagent_settled` / `note_user_interrupt`）存在中间态窗口，且单次操作要反复加解锁。
/// 收拢进同一把 `parking_lot::Mutex` 后，跨表变更天然原子一致。
#[derive(Default)]
struct HookTables {
    last_hook_time: HashMap<u32, Instant>,
    last_hook_status: HashMap<u32, String>,
    /// pty → 当前会话身份；/clear 等换会话时随下一个 hook 事件自动刷新
    last_session: HashMap<u32, HookSessionId>,
    /// 记录哪些 PTY 曾经收到过 hook 事件（一旦标记，永不降级回轮询）
    hook_enabled: HashSet<u32>,
    /// pty → 已结束会话 id 的环形墓碑。hook 脚本是独立进程，POST 到达
    /// 顺序无保证：SessionEnd 之后仍可能收到旧会话迟到的 Stop/Notification，
    /// 若放行会把已退出的 pane 重新推回 ai-idle。`remove()` 不清墓碑
    /// （SessionEnd 自身要先打墓碑再 remove），PTY 关闭时由 `purge()` 清理。
    ended_sessions: HashMap<u32, VecDeque<String>>,
    /// pty → 当前活跃会话 id 集合（有序去重）。SessionEnd 只有在移除该会话后
    /// 集合为空时才执行销毁动作：嵌套非交互实例（Bash 工具里跑 `claude -p` /
    /// `codex exec`，继承 MINITERM_PTY_ID）与"退出后立刻重开"的乱序场景下，
    /// pane 上还有别的活跃会话，误销毁会把正在工作的外层会话打回 idle。
    active_sessions: HashMap<u32, VecDeque<String>>,
    /// agy:pane 上还在跑的子代理会话 id(子代理调模型时记入、它的收尾 Stop 时移出)。
    /// 只服务「主会话的完成被延后」的判定,见 [`EventRoute::DeferStop`]
    agy_subagents: HashMap<u32, HashSet<String>>,
    /// agy:主会话以 `Stop(fullyIdle=false)` 收尾、正等子代理收尾的 pane → 那次 Stop 的
    /// terminationReason。主会话再有任何事件即清除
    agy_deferred_stop: HashMap<u32, Option<String>>,
    /// 最近一次权威状态来源（HTTP hook 或 OSC 7501）报出的规范 agent 标识（如 `"agy"`）。
    ///
    /// 仅靠 OSC 7501（未开 hook / WSL / SSH）时没有 `session_id`、不进 `last_session`，
    /// 靠这张表让 `monitor::pane_agent_lacks_session_end` 认出它是 `agy`，退出后 10s 兜底
    /// 才能干净收成 `idle`。
    last_agent: HashMap<u32, String>,
    /// OSC 7501 最近一次已发射的 `blocked(kind, msg)` 指纹：单次弹窗内 TUI 重绘/按键导航
    /// 重发同一条 `blocked` 时去重，避免用户每按一次方向键就重复响提示音；一旦离开
    /// `blocked`（任何非 blocked 状态更新 / HTTP hook / 打断）或换了 `kind`/`msg` 立刻放行。
    last_osc_blocked: HashMap<u32, (OscBlockedKind, Option<String>)>,
}

impl HookTables {
    fn should_settle_interrupt(&self, pty_id: u32) -> bool {
        self.hook_enabled.contains(&pty_id)
            && self.last_hook_status.get(&pty_id).map(String::as_str) == Some("ai-working")
    }

    fn update(&mut self, pty_id: u32, status: String) {
        self.hook_enabled.insert(pty_id);
        self.last_hook_time.insert(pty_id, Instant::now());
        self.last_hook_status.insert(pty_id, status);
        self.last_osc_blocked.remove(&pty_id);
    }

    fn remove_pty(&mut self, pty_id: u32) {
        self.hook_enabled.remove(&pty_id);
        self.last_hook_time.remove(&pty_id);
        self.last_hook_status.remove(&pty_id);
        self.last_session.remove(&pty_id);
        self.agy_subagents.remove(&pty_id);
        self.agy_deferred_stop.remove(&pty_id);
        self.last_agent.remove(&pty_id);
        self.last_osc_blocked.remove(&pty_id);
    }
}

/// Hook 状态管理器，记录每个 PTY 的最后 hook 事件时间和状态
#[derive(Clone)]
pub struct HookState {
    tables: Arc<Mutex<HookTables>>,
    /// 串行化权威状态入口（HTTP hook / OSC 7501 / 用户打断），防止同一 pane 的状态表更新
    /// 与 `StatusEmitter` 发射顺序在并发下交错撕裂。
    dispatch_lock: Arc<Mutex<()>>,
    port: Arc<Mutex<u16>>,
    /// 保存运行中的 server 句柄，供运行时停止以及 `with_server_lock` 与启停串行化
    server: Arc<Mutex<Option<Arc<ServerHandle>>>>,
}

impl Default for HookState {
    fn default() -> Self {
        Self::new()
    }
}

impl HookState {
    pub fn new() -> Self {
        Self {
            tables: Arc::new(Mutex::new(HookTables::default())),
            dispatch_lock: Arc::new(Mutex::new(())),
            port: Arc::new(Mutex::new(0)),
            server: Arc::new(Mutex::new(None)),
        }
    }

    /// 检查指定 PTY 是否已启用 hook（曾经收到过 hook 事件）
    ///
    /// 一旦启用，完全信任 hook 状态，不再降级回进程轮询。
    pub fn is_hook_enabled(&self, pty_id: u32) -> bool {
        self.tables.lock().hook_enabled.contains(&pty_id)
    }

    /// 获取指定 PTY 的 hook 状态
    pub fn get_status(&self, pty_id: u32) -> Option<String> {
        self.tables.lock().last_hook_status.get(&pty_id).cloned()
    }

    /// 距上一次 hook 事件（或上一次状态落盘）的时长；从未收到过事件返回 None。
    /// 停摆兜底（`monitor::stall_settle_target`）用它确认「状态本身也
    /// 已经静置足够久」，而不只是 PTY 没输出。
    pub(crate) fn status_age(&self, pty_id: u32) -> Option<Duration> {
        self.tables
            .lock()
            .last_hook_time
            .get(&pty_id)
            .map(|t| t.elapsed())
    }

    /// 当前会话身份;从未收到带 session_id 的事件时返回 None
    pub fn session_of(&self, pty_id: u32) -> Option<HookSessionId> {
        self.tables.lock().last_session.get(&pty_id).cloned()
    }

    /// 最近一次权威状态入口（HTTP hook / OSC 7501）报出的 agent 名；未上报过返回 None。
    pub(crate) fn agent_of(&self, pty_id: u32) -> Option<String> {
        self.tables.lock().last_agent.get(&pty_id).cloned()
    }

    /// 记录 hook 上报的会话身份(每个事件都带,直接覆盖即可)。
    /// 返回身份是否发生变化(新 pane/换会话/agent 修正),变化时调用方通知上层。
    /// agent 也参与比较:codex 的 SessionStart 不带 turn_id 会被 hook 二进制
    /// 误推断为 claude-code,靠后续带 turn_id 的事件在这里纠正并重新通知。
    pub(crate) fn record_session(
        &self,
        pty_id: u32,
        agent: Option<String>,
        session_id: String,
    ) -> bool {
        let mut tables = self.tables.lock();
        let changed = tables
            .last_session
            .get(&pty_id)
            .is_none_or(|prev| prev.session_id != session_id || prev.agent != agent);
        tables
            .last_session
            .insert(pty_id, HookSessionId { agent, session_id });
        changed
    }

    /// 更新指定 PTY 的 hook 状态
    pub(crate) fn update(&self, pty_id: u32, status: String) {
        self.tables.lock().update(pty_id, status);
    }

    /// 移除指定 PTY 的 hook 状态。不清墓碑：SessionEnd 打完墓碑后调用
    /// 本方法，墓碑要继续挡住旧会话的迟到事件。
    pub fn remove(&self, pty_id: u32) {
        self.tables.lock().remove_pty(pty_id);
    }

    /// agy 子代理会话又有动静(调模型):记进该 pane 的在跑集合。容量封顶,防同机
    /// 进程往无鉴权端口灌假 id 撑内存。
    pub(crate) fn agy_subagent_active(&self, pty_id: u32, session_id: &str) {
        let mut tables = self.tables.lock();
        let set = tables.agy_subagents.entry(pty_id).or_default();
        if set.len() < AGY_SUBAGENTS_CAP || set.contains(session_id) {
            set.insert(session_id.to_string());
        }
    }

    /// agy 子代理收尾:移出在跑集合。集合空了、且主会话正以 `fullyIdle=false` 等着时,
    /// 取走主会话那次 Stop 的 terminationReason(`Some`)—— 主会话的这一回合此刻才算完。
    pub(crate) fn agy_subagent_settled(
        &self,
        pty_id: u32,
        session_id: &str,
    ) -> Option<Option<String>> {
        let mut tables = self.tables.lock();
        let all_settled = match tables.agy_subagents.get_mut(&pty_id) {
            Some(set) => {
                set.remove(session_id);
                set.is_empty()
            }
            None => true,
        };
        if all_settled {
            tables.agy_deferred_stop.remove(&pty_id)
        } else {
            None
        }
    }

    /// 主会话以 `Stop(fullyIdle=false)` 收尾:记下,等子代理收尾时再判完成。
    pub(crate) fn agy_defer_stop(&self, pty_id: u32, termination_reason: Option<String>) {
        self.tables
            .lock()
            .agy_deferred_stop
            .insert(pty_id, termination_reason);
    }

    /// 主会话又有动静(继续干活 / 自己发了 fullyIdle=true 的 Stop):延后的完成作废。
    pub(crate) fn agy_clear_deferred_stop(&self, pty_id: u32) {
        self.tables.lock().agy_deferred_stop.remove(&pty_id);
    }

    /// PTY 关闭时的彻底清理：hook 状态 + 墓碑 + 活跃会话集
    pub fn purge(&self, pty_id: u32) {
        let mut tables = self.tables.lock();
        tables.remove_pty(pty_id);
        tables.ended_sessions.remove(&pty_id);
        tables.active_sessions.remove(&pty_id);
    }

    /// 记录会话为活跃。任意非 SessionEnd 事件都调（不只 SessionStart：
    /// hook server 中途启用时首个事件可能是 Stop/PreToolUse）。
    /// 有序去重；超容量挤掉最老的——正常情况集合里只有 1 个。
    fn note_session_active(&self, pty_id: u32, session_id: &str) {
        let mut tables = self.tables.lock();
        let queue = tables.active_sessions.entry(pty_id).or_default();
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
        let mut tables = self.tables.lock();
        let Some(queue) = tables.active_sessions.get_mut(&pty_id) else {
            return true;
        };
        if let Some(sid) = session_id {
            queue.retain(|s| s != sid);
        }
        let empty = queue.is_empty();
        if empty {
            tables.active_sessions.remove(&pty_id);
        }
        empty
    }

    /// 给已结束的会话 id 打墓碑
    pub fn mark_session_ended(&self, pty_id: u32, session_id: String) {
        let mut tables = self.tables.lock();
        let queue = tables.ended_sessions.entry(pty_id).or_default();
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
        self.tables
            .lock()
            .ended_sessions
            .get(&pty_id)
            .is_some_and(|q| q.iter().any(|s| s == session_id))
    }

    /// 摘除墓碑:SessionStart 表明同 id 会话再次存活(退出后 claude -c / --resume
    /// 重开),不摘的话该会话的后续事件被永久忽略,身份也无法重新记录
    pub fn revive_session(&self, pty_id: u32, session_id: &str) {
        if let Some(queue) = self.tables.lock().ended_sessions.get_mut(&pty_id) {
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

    /// 保存 server 实例（测试用）
    #[cfg(test)]
    fn set_server(&self, server: Option<Arc<ServerHandle>>) {
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

/// 该 `Stop` 是否是用户打断后的回合收尾（omp / pi 上报 `reason: "aborted"`）。
///
/// omp 在 Esc/Ctrl+C 打断后仍会发 `agent_end`，扩展据 `stopReason == "aborted"`
/// 标成这样一发（pi 是 `agent_settled` 的 `aborted` 位，同一个上报形态）。它要把状态收到 ai-idle（回合确实结束了），但 cause 不能是 `Stop`
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
    // 因此对 Codex 保持 ai-working（仍处于任务中）。哪家是这个形态查
    // `AgentSpec::permission_request_keeps_working`。
    if event == "PermissionRequest"
        && agent
            .and_then(AgentKind::parse)
            .is_some_and(|k| k.spec().permission_request_keeps_working)
    {
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

/// 一条 hook 事件落到状态上的去向(纯函数 [`route_event`] 的结果)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventRoute<'a> {
    /// agy 子代理会话的事件:不记会话身份、不改状态,只用来追踪在跑的子代理
    /// (`settled` = 这是它的收尾 Stop),供 [`EventRoute::DeferStop`] 判主会话何时算完
    Subagent { settled: bool },
    /// agy 主会话以 `Stop(fullyIdle=false)` 收尾:照常记会话身份、不改状态,完成延后到
    /// 它拉起的子代理全部收尾(实测两种时序都有,见 [`route_agy_event`])
    DeferStop,
    /// 照常记会话身份,但不改状态。带一句日志用的说明
    KeepStatus(&'static str),
    /// 改成 `status`,成因 `cause`(UI 据此判完成 / 黄灯,见 [`event_cause`])
    Status {
        status: &'static str,
        cause: &'a str,
    },
}

/// 一条事件该怎么落:agy 先过 [`route_agy_event`],其余各家走既有映射
/// ([`map_event_to_status`] + 打断 / Notification 的成因归一化)。
fn route_event<'a>(event: &'a str, payload: &HookPayload) -> EventRoute<'a> {
    if payload.agent.as_deref().and_then(AgentKind::parse) == Some(AgentKind::Agy) {
        return route_agy_event(
            event,
            payload.termination_reason.as_deref(),
            payload.fully_idle,
            payload.has_user_input,
        );
    }
    route_mapped(
        event,
        payload.agent.as_deref(),
        payload.notification_type.as_deref(),
        payload.message.as_deref(),
        payload.reason.as_deref(),
    )
}

/// 既有映射(Claude / Codex / Grok / omp / pi 的口径,改 agy 时一个字没动)。
fn route_mapped<'a>(
    event: &'a str,
    agent: Option<&str>,
    notification_type: Option<&str>,
    message: Option<&str>,
    reason: Option<&str>,
) -> EventRoute<'a> {
    match map_event_to_status(event, agent, notification_type, message, reason) {
        Some(status) => EventRoute::Status {
            status,
            // cause 带归一化后的事件名:Stop/PermissionRequest/Notification 都落 ai-idle,
            // 但只有 Stop 是「任务做完了」,UI 据此决定播报与托盘黄绿灯
            cause: if is_user_abort_stop(event, reason) {
                "Interrupt"
            } else {
                event_cause(event, notification_type, message)
            },
        },
        // 未映射事件：新版 Claude Code / Codex 加了事件而这里还没跟上时
        // 的可见信号（注册列表是白名单，正常不会有陌生事件抵达）
        None => EventRoute::KeepStatus("无状态映射，已忽略"),
    }
}

/// agy(Antigravity CLI)事件的归一化。agy 的事件集与语义都和别家不同(实测 1.5.12,
/// 见 `hook_registry` 的 agy 段):
///
/// - **子代理**(`invoke_subagent`)同样触发全局 hook,带它自己的 conversationId,且
///   `lastUserInput` 恒为空串 → `has_user_input == Some(false)`:不记会话身份(否则 pane
///   的身份被换成子代理会话,续接 / 镜像全错)、不改状态([`EventRoute::Subagent`]),
///   只用来追踪「还有哪些子代理在跑」。
/// - `PreInvocation`(每次调模型前)是唯一注册的「工作中」事件;`PostInvocation` /
///   `PostToolUse` 没注册,用户手工加了也一样按工作中算。
/// - `Stop`:
///   - `fully_idle == Some(false)`:主会话的回合停了,但它拉起的后台任务(子代理)还在跑
///     → 先不算完成([`EventRoute::DeferStop`])。实测(1.5.12)两种时序都有:① 主会话
///     等子代理回报时先停一次,收到回报后接着 `PreInvocation` → … → `Stop(fullyIdle=true)`
///     —— 这时主会话的新事件把延后作废,以它自己的 Stop 为准;② 主会话收到回报、答完
///     就停了(`fullyIdle=false`,子代理还在做收尾),之后**再没有**主会话事件,只有子代理
///     的收尾 Stop —— 这时等子代理全部收尾,才把那次延后的 Stop 补记成完成;
///   - `USER_CANCELED` → 走 omp / pi 那条 `reason: "aborted"` 的路(ai-idle,cause=
///     `Interrupt`,不播报完成)。实测 Esc 打断**不发** Stop,这条是给别的取消路径留的,
///     Esc 由 `note_user_interrupt` 收;
///   - `ERROR` / `MAX_INVOCATIONS` / `MAX_FORCED_INVOCATIONS` /
///     `MAX_TOKEN_BUDGET_EXCEEDED`:回合是被迫停下的 → 按 `StopFailure`(ai-idle + 黄灯,
///     不播报完成);
///   - 其余(`NO_TOOL_CALL` 正常完成、`TERMINAL_*`、`INJECTED_RESPONSE`、未知、缺失)→
///     正常 `Stop`(完成)。
/// - 其余事件(`SessionStart` 等)走既有映射。
fn route_agy_event<'a>(
    event: &'a str,
    termination_reason: Option<&str>,
    fully_idle: Option<bool>,
    has_user_input: Option<bool>,
) -> EventRoute<'a> {
    let agy = Some(AgentKind::Agy.key());
    if has_user_input == Some(false) {
        return EventRoute::Subagent {
            settled: event == "Stop" && fully_idle != Some(false),
        };
    }
    match event {
        "PreInvocation" | "PostInvocation" | "PostToolUse" => EventRoute::Status {
            status: "ai-working",
            cause: event,
        },
        "Stop" if fully_idle == Some(false) => EventRoute::DeferStop,
        "Stop" => match termination_reason {
            Some("USER_CANCELED") => route_mapped("Stop", agy, None, None, Some("aborted")),
            Some(
                "ERROR"
                | "MAX_INVOCATIONS"
                | "MAX_FORCED_INVOCATIONS"
                | "MAX_TOKEN_BUDGET_EXCEEDED",
            ) => route_mapped("StopFailure", agy, None, None, None),
            _ => route_mapped("Stop", agy, None, None, None),
        },
        _ => route_mapped(event, agy, None, None, None),
    }
}

/// 子代理全部收尾时,要不要把主会话延后的完成补记上(纯判定,见 [`EventRoute::DeferStop`])。
///
/// 只在主会话「还算在这一回合里」时补记:状态仍是 ai-working,或者只是子代理收尾期间
/// 输出静默、被停摆兜底悄悄收成了 ai-idle(cause=`Stall`,不是完成,补记上才有完成播报)。
/// 其余一律作废:被 Esc 打断过(cause=`Interrupt`,打断的回合不算做完)、已退出 / 回落
/// idle、或别的 ai-idle 成因 —— 宁可少报一次完成,不报假完成。
fn deferred_stop_applies(status: Option<&str>, last_cause: Option<&str>) -> bool {
    match status {
        Some("ai-working") => true,
        Some("ai-idle") => last_cause == Some(crate::monitor::STALL_CAUSE),
        _ => false,
    }
}

/// 该 agent 退出时发不发 `SessionEnd`(agent 表的 `session_end_hook`)。缺省 / 空串按
/// Claude(会话身份的约定);认不出的按「发」算,保持改造前的行为。
pub(crate) fn agent_sends_session_end(agent: Option<&str>) -> bool {
    AgentKind::from_session_agent(agent).is_none_or(|kind| kind.spec().session_end_hook)
}

/// 打断是否应当改写状态（`note_user_interrupt` 的纯判定部分，抽出来是为了可测：
/// 发射一侧需要 sink，单测里构造不出来）。
///
/// 两道闸：hook 未启用的 pane 走的是轮询降级路径，那条路径本就按输出活跃度给出
/// ai-idle，插手只会互相打架；当前状态不是 ai-working 说明没什么可打断的
/// （已经在等用户，或压根不在 AI 会话里，Esc 只是 shell/TUI 的普通按键）。
#[cfg(test)]
fn interrupt_should_settle(hook_state: &HookState, pty_id: u32) -> bool {
    hook_state.tables.lock().should_settle_interrupt(pty_id)
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
    let _dispatch_guard = hook_state.dispatch_lock.lock();
    let settled = {
        let mut tables = hook_state.tables.lock();
        if tables.should_settle_interrupt(pty_id) {
            tables.update(pty_id, "ai-idle".to_string());
            true
        } else {
            false
        }
    };
    if !settled {
        return;
    }
    emitter.emit_if_changed(pty_id, "ai-idle", Some("Interrupt"), agent);
    tracing::info!("[hook-server] pty_id={} 用户打断 -> ai-idle", pty_id);
}

/// 消费一条从 PTY 字节流解析出来的 OSC 7501（Program Status Protocol）状态上报。
///
/// 当前仅处理 Antigravity CLI（`app=antigravity` / `agy`）：
/// - 补齐其 `hooks.json` 缺失的权限确认黄灯（`state=blocked:kind=permission` → `PermissionRequest`）
///   与提问等待黄灯（`state=blocked:kind=question` → `Elicitation`）；
/// - 未开 hook / WSL / SSH 远程场景下同样获得精确状态与完成播报，与已有 HTTP hook 共用同一
///   `HookState` + `StatusEmitter` 去重表，双向并发到达时绝不双播完成。
/// - 非 `agy` 的程序（普通 CLI 如 `brew` / `terraform`，或已有完善自有 hook 的其他 AI CLI）
///   在此拦截，不污染状态机。
pub fn observe_osc_status(
    hook_state: &HookState,
    emitter: &StatusEmitter,
    tracker: &SessionTracker,
    pty_id: u32,
    status: OscStatus,
) {
    let Some(kind) = status.app.as_deref().and_then(AgentKind::parse) else {
        return;
    };
    if kind != AgentKind::Agy {
        return;
    }

    // 层次化子任务（带非空 id）的 done / idle / working 不改主会话状态，防止子任务收尾
    // 误报主会话完成；但 blocked 确需人工处理，无论是否带 id 一律点灯。
    if status.id.is_some() && status.state != OscProgramState::Blocked {
        return;
    }

    let _dispatch_guard = hook_state.dispatch_lock.lock();
    let agy_key = AgentKind::Agy.key();

    match status.state {
        OscProgramState::Blocked => {
            let blocked_kind = status.kind.unwrap_or(OscBlockedKind::Other);
            let cause = match blocked_kind {
                OscBlockedKind::Question => "Elicitation",
                OscBlockedKind::Permission | OscBlockedKind::Auth | OscBlockedKind::Other => {
                    "PermissionRequest"
                }
            };
            let fingerprint = (blocked_kind, status.msg);
            {
                let mut tables = hook_state.tables.lock();
                if tables.last_osc_blocked.get(&pty_id) == Some(&fingerprint) {
                    return;
                }
                tables.update(pty_id, "ai-idle".to_string());
                tables.last_agent.insert(pty_id, agy_key.to_string());
                tables.agy_deferred_stop.remove(&pty_id);
                tables.last_osc_blocked.insert(pty_id, fingerprint);
            }
            tracker.mark_ai_session(pty_id, agy_key);
            emitter.emit_if_changed(pty_id, "ai-idle", Some(cause), Some(agy_key.to_string()));
            tracing::info!(
                "[osc7501] pty_id={} agent=agy state=blocked kind={:?} -> ai-idle cause={}",
                pty_id,
                blocked_kind,
                cause
            );
        }
        OscProgramState::Working => {
            {
                let mut tables = hook_state.tables.lock();
                tables.update(pty_id, "ai-working".to_string());
                tables.last_agent.insert(pty_id, agy_key.to_string());
                tables.agy_deferred_stop.remove(&pty_id);
            }
            tracker.mark_ai_session(pty_id, agy_key);
            emitter.emit_if_changed(
                pty_id,
                "ai-working",
                Some("OscWorking"),
                Some(agy_key.to_string()),
            );
            tracing::info!(
                "[osc7501] pty_id={} agent=agy state=working -> ai-working",
                pty_id
            );
        }
        OscProgramState::Done => {
            {
                let mut tables = hook_state.tables.lock();
                // HTTP hook `Stop(fullyIdle=false)` 已明确延后完成等子代理收尾时，
                // 不让伴随的 OSC `state=done` 抢跑清空延后标记。
                if tables.agy_deferred_stop.contains_key(&pty_id) {
                    return;
                }
                tables.update(pty_id, "ai-idle".to_string());
                tables.last_agent.insert(pty_id, agy_key.to_string());
            }
            tracker.mark_ai_session(pty_id, agy_key);
            emitter.emit_if_changed(pty_id, "ai-idle", Some("Stop"), Some(agy_key.to_string()));
            tracing::info!(
                "[osc7501] pty_id={} agent=agy state=done -> ai-idle cause=Stop",
                pty_id
            );
        }
        OscProgramState::Error => {
            {
                let mut tables = hook_state.tables.lock();
                tables.update(pty_id, "ai-idle".to_string());
                tables.last_agent.insert(pty_id, agy_key.to_string());
                tables.agy_deferred_stop.remove(&pty_id);
            }
            tracker.mark_ai_session(pty_id, agy_key);
            emitter.emit_if_changed(
                pty_id,
                "ai-idle",
                Some("StopFailure"),
                Some(agy_key.to_string()),
            );
            tracing::info!(
                "[osc7501] pty_id={} agent=agy state=error -> ai-idle cause=StopFailure",
                pty_id
            );
        }
        OscProgramState::Idle => {
            // agy 退出瞬间（/exit、双击 Ctrl+D）会再发一次 state=idle：窗口内忽略，
            // 既不扶正已被清掉的会话标记，也不重置静默计时，让 10s 退出兜底正常落回 idle。
            if tracker.recently_exited(pty_id, EXIT_IDLE_IGNORE_WINDOW) {
                tracing::debug!(
                    "[osc7501] pty_id={} agent=agy 退出窗口内忽略 state=idle",
                    pty_id
                );
                return;
            }
            {
                let mut tables = hook_state.tables.lock();
                if tables.agy_deferred_stop.contains_key(&pty_id) {
                    return;
                }
                tables.update(pty_id, "ai-idle".to_string());
                tables.last_agent.insert(pty_id, agy_key.to_string());
            }
            tracker.mark_ai_session(pty_id, agy_key);
            emitter.emit_if_changed(
                pty_id,
                "ai-idle",
                Some("OscIdle"),
                Some(agy_key.to_string()),
            );
        }
        OscProgramState::Clear => {
            // 进程明确清空状态且输入检测已判退出时，直接收尾回 idle，不必再等 10s 兜底。
            if !tracker.is_ai_session(pty_id) && hook_state.is_hook_enabled(pty_id) {
                hook_state.remove(pty_id);
                emitter.emit_if_changed(pty_id, "idle", Some("SessionEnd"), None);
                tracing::info!(
                    "[osc7501] pty_id={} agent=agy state=clear(已退出) -> idle",
                    pty_id
                );
            }
        }
    }
}

// ─── 基于标准库 TcpListener 的轻量 HTTP/1.x 服务端 ───────────────────────────

/// 运行中的 hook HTTP 服务器句柄。
///
/// 通过 `shutdown` 标记 + 唤醒自连接立刻中断阻塞中的 `TcpListener::accept()`，
/// 再回收 accept 线程，保证 `stop_hook_server` 返回时端口已经释放。
struct ServerHandle {
    addr: Option<SocketAddr>,
    shutdown: Arc<AtomicBool>,
    pool: Arc<WorkerPool>,
    accept_thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl ServerHandle {
    fn stop(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.pool.close();
        // 幂等守卫：`stop_hook_server` 显式调过 `stop()` 后，`Arc<ServerHandle>`
        // 紧接着落出作用域还会触发 `Drop::drop` 再次进入这里；此时 accept 线程已回收、
        // 端口已关，直接返回可避免向已关闭端口再发一次 200ms 超时的唤醒连接。
        let Some(handle) = self.accept_thread.lock().take() else {
            return;
        };
        let woke = match self.addr {
            Some(addr) => match TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
                Ok(_) => true,
                Err(e) => {
                    tracing::warn!("[hook-server] 唤醒停止连接失败 ({}): {}", addr, e);
                    false
                }
            },
            None => true,
        };
        if woke {
            let _ = handle.join();
        }
    }

    #[cfg(test)]
    fn for_test() -> Self {
        Self {
            addr: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            pool: Arc::new(WorkerPool::new(
                MAX_CONCURRENT_WORKERS,
                MAX_QUEUED_CONNECTIONS,
            )),
            accept_thread: Mutex::new(None),
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct ServerContext {
    hook_state: HookState,
    emitter: StatusEmitter,
    tracker: SessionTracker,
    read_timeout: Duration,
    write_timeout: Duration,
}

struct PoolState {
    queue: VecDeque<TcpStream>,
    total_workers: usize,
    idle_workers: usize,
    closed: bool,
}

/// 惰性伸缩的有限并发 worker 池：空闲时 0 线程，突发或遇到慢连接时按需扩到
/// `max_workers`，空闲超过 `WORKER_IDLE_TTL` 自动退出。
struct WorkerPool {
    state: Mutex<PoolState>,
    cv: Condvar,
    max_workers: usize,
    max_queued: usize,
}

struct WorkerGuard(Arc<WorkerPool>);

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.total_workers = state.total_workers.saturating_sub(1);
    }
}

impl WorkerPool {
    fn new(max_workers: usize, max_queued: usize) -> Self {
        Self {
            state: Mutex::new(PoolState {
                queue: VecDeque::new(),
                total_workers: 0,
                idle_workers: 0,
                closed: false,
            }),
            cv: Condvar::new(),
            max_workers,
            max_queued,
        }
    }

    fn submit(self: &Arc<Self>, stream: TcpStream, ctx: &Arc<ServerContext>) {
        let need_spawn = {
            let mut state = self.state.lock();
            if state.closed {
                return;
            }
            if state.queue.len() >= self.max_queued {
                tracing::warn!(
                    "[hook-server] 待处理连接数达上限 ({})，已丢弃新连接",
                    self.max_queued
                );
                return;
            }
            state.queue.push_back(stream);
            let spawn =
                state.queue.len() > state.idle_workers && state.total_workers < self.max_workers;
            if spawn {
                state.total_workers += 1;
            }
            self.cv.notify_one();
            spawn
        };

        if need_spawn {
            let pool = Arc::clone(self);
            let guard = WorkerGuard(Arc::clone(self));
            let ctx = Arc::clone(ctx);
            if let Err(e) = thread::Builder::new()
                .name("mt-hook-worker".into())
                .spawn(move || {
                    let _guard = guard;
                    pool.worker_loop(&ctx);
                })
            {
                tracing::warn!("[hook-server] 启动 worker 线程失败: {}", e);
            }
        }
    }

    fn worker_loop(&self, ctx: &ServerContext) {
        loop {
            let stream = {
                let mut state = self.state.lock();
                loop {
                    if let Some(stream) = state.queue.pop_front() {
                        break Some(stream);
                    }
                    if state.closed {
                        break None;
                    }
                    state.idle_workers += 1;
                    let timed_out = self.cv.wait_for(&mut state, WORKER_IDLE_TTL).timed_out();
                    state.idle_workers -= 1;
                    if timed_out && state.queue.is_empty() {
                        break None;
                    }
                }
            };
            let Some(stream) = stream else {
                break;
            };
            handle_connection(stream, ctx);
        }
    }

    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.queue.clear();
        self.cv.notify_all();
    }
}

/// 带总截止时间的 `TcpStream` 读包装：既每次 `read` 设置 socket `read_timeout`，
/// 又以绝对 `deadline` 封死总耗时（抵御逐字节慢速发送的 slowloris 连接）。
struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let now = Instant::now();
            if now >= self.deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "hook request read timed out",
                ));
            }
            let remaining = self.deadline.saturating_duration_since(now);
            let timeout = remaining.max(Duration::from_millis(1));
            self.stream.set_read_timeout(Some(timeout))?;
            match self.stream.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }
}

enum LineRead {
    Line,
    EofEmpty,
    EofPartial,
}

enum RequestError {
    EmptyConnection,
    TimedOut,
    MethodNotAllowed,
    NotFound,
    PayloadTooLarge,
    BadRequest,
}

fn is_timeout_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max_total: usize,
    total_read: &mut usize,
) -> Result<LineRead, RequestError> {
    buf.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(slice) => slice,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_timeout_error(&e) => return Err(RequestError::TimedOut),
            Err(_) => return Err(RequestError::BadRequest),
        };
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                LineRead::EofEmpty
            } else {
                LineRead::EofPartial
            });
        }
        let (take_len, done) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (available.len(), false),
        };
        if total_read.saturating_add(take_len) > max_total {
            return Err(RequestError::BadRequest);
        }
        buf.extend_from_slice(&available[..take_len]);
        *total_read += take_len;
        reader.consume(take_len);
        if done {
            return Ok(LineRead::Line);
        }
    }
}

fn strip_line_ending(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn read_exact_into(
    reader: &mut impl Read,
    out: &mut Vec<u8>,
    mut remaining: usize,
) -> Result<(), RequestError> {
    let mut buf = [0u8; 8192];
    while remaining > 0 {
        let to_read = remaining.min(buf.len());
        match reader.read(&mut buf[..to_read]) {
            Ok(0) => return Err(RequestError::BadRequest),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                remaining -= n;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_timeout_error(&e) => return Err(RequestError::TimedOut),
            Err(_) => return Err(RequestError::BadRequest),
        }
    }
    Ok(())
}

fn read_chunked_body(reader: &mut impl BufRead, max_bytes: usize) -> Result<Vec<u8>, RequestError> {
    let mut body = Vec::new();
    let mut line = Vec::new();
    let mut meta_bytes = 0usize;

    loop {
        match read_bounded_line(reader, &mut line, MAX_HOOK_HEADER_BYTES, &mut meta_bytes)? {
            LineRead::Line => {}
            LineRead::EofEmpty | LineRead::EofPartial => return Err(RequestError::BadRequest),
        }
        let raw =
            std::str::from_utf8(strip_line_ending(&line)).map_err(|_| RequestError::BadRequest)?;
        let size_str = raw.split(';').next().unwrap_or("").trim();
        if size_str.is_empty() || !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RequestError::BadRequest);
        }
        let chunk_size =
            usize::from_str_radix(size_str, 16).map_err(|_| RequestError::BadRequest)?;

        if chunk_size == 0 {
            // 读完可选 trailer 头直到空行
            loop {
                match read_bounded_line(reader, &mut line, MAX_HOOK_HEADER_BYTES, &mut meta_bytes)?
                {
                    LineRead::Line => {
                        if strip_line_ending(&line).is_empty() {
                            return Ok(body);
                        }
                    }
                    LineRead::EofEmpty | LineRead::EofPartial => {
                        return Err(RequestError::BadRequest);
                    }
                }
            }
        }

        if body.len().saturating_add(chunk_size) > max_bytes {
            return Err(RequestError::PayloadTooLarge);
        }

        read_exact_into(reader, &mut body, chunk_size)?;

        // 每个 chunk 数据后必须跟 CRLF
        let mut crlf = [0u8; 2];
        let mut read = 0usize;
        while read < 2 {
            match reader.read(&mut crlf[read..]) {
                Ok(0) => return Err(RequestError::BadRequest),
                Ok(n) => read += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if is_timeout_error(&e) => return Err(RequestError::TimedOut),
                Err(_) => return Err(RequestError::BadRequest),
            }
        }
        if crlf != *b"\r\n" {
            return Err(RequestError::BadRequest);
        }
    }
}

fn read_hook_request_body(reader: &mut BufReader<DeadlineStream>) -> Result<String, RequestError> {
    let mut line = Vec::new();
    let mut header_bytes = 0usize;

    // 1. 解析请求行: METHOD TARGET HTTP/1.x
    match read_bounded_line(reader, &mut line, MAX_HOOK_HEADER_BYTES, &mut header_bytes)? {
        LineRead::EofEmpty => return Err(RequestError::EmptyConnection),
        LineRead::EofPartial => return Err(RequestError::BadRequest),
        LineRead::Line => {}
    }
    let req_line =
        std::str::from_utf8(strip_line_ending(&line)).map_err(|_| RequestError::BadRequest)?;
    let mut parts = req_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(RequestError::BadRequest);
    };
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/1.") {
        return Err(RequestError::BadRequest);
    }
    let method = method.to_string();
    let target = target.to_string();
    let is_http11 = version == "HTTP/1.1";

    // 2. 解析请求头（大小写不敏感）
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    let mut expect_continue = false;

    loop {
        match read_bounded_line(reader, &mut line, MAX_HOOK_HEADER_BYTES, &mut header_bytes)? {
            LineRead::Line => {}
            LineRead::EofEmpty | LineRead::EofPartial => return Err(RequestError::BadRequest),
        }
        let trimmed = strip_line_ending(&line);
        if trimmed.is_empty() {
            break;
        }
        let header_str = std::str::from_utf8(trimmed).map_err(|_| RequestError::BadRequest)?;
        let Some((name, value)) = header_str.split_once(':') else {
            return Err(RequestError::BadRequest);
        };
        if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(RequestError::BadRequest);
        }
        let value = value.trim();

        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(RequestError::BadRequest);
            }
            let parsed = value
                .parse::<usize>()
                .map_err(|_| RequestError::BadRequest)?;
            if let Some(prev) = content_length
                && prev != parsed
            {
                return Err(RequestError::BadRequest);
            }
            content_length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            let codings: Vec<&str> = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if codings.len() == 1 && codings[0].eq_ignore_ascii_case("chunked") {
                is_chunked = true;
            } else if codings.len() == 1 && codings[0].eq_ignore_ascii_case("identity") {
                // 非分块
            } else {
                return Err(RequestError::BadRequest);
            }
        } else if name.eq_ignore_ascii_case("expect") && value.eq_ignore_ascii_case("100-continue")
        {
            expect_continue = true;
        }
    }

    // RFC 9112 §6.3: 同时携带 Transfer-Encoding 与 Content-Length 按非法请求拒绝
    if is_chunked && content_length.is_some() {
        return Err(RequestError::BadRequest);
    }

    // 3. 方法与路由校验（先于 100-continue 与读 body）
    if method != "POST" {
        return Err(RequestError::MethodNotAllowed);
    }
    if target != "/hook" {
        return Err(RequestError::NotFound);
    }

    // 4. 端点监听在 127.0.0.1 且无鉴权(改鉴权要动 sidecar 协议),
    // 同机任意进程都能 POST 过来,不封顶等于把内存交给对方:先看
    // Content-Length 拦掉声明超限的,分块传输同样在读每块前校验上限。
    if let Some(len) = content_length
        && len > MAX_HOOK_BODY_BYTES
    {
        return Err(RequestError::PayloadTooLarge);
    }

    // 5. 若客户端发了 Expect: 100-continue，路由与长度都合法后再放行继续发 body
    if expect_continue && is_http11 {
        let stream = &mut reader.get_mut().stream;
        if stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() || stream.flush().is_err() {
            return Err(RequestError::TimedOut);
        }
    }

    // 6. 读取 body
    let body_bytes = if is_chunked {
        read_chunked_body(reader, MAX_HOOK_BODY_BYTES)?
    } else if let Some(len) = content_length {
        let mut buf = Vec::with_capacity(len.min(8192));
        read_exact_into(reader, &mut buf, len)?;
        buf
    } else {
        Vec::new()
    };

    String::from_utf8(body_bytes).map_err(|_| RequestError::BadRequest)
}

fn send_http_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

fn handle_connection(stream: TcpStream, ctx: &ServerContext) {
    let _ = stream.set_write_timeout(Some(ctx.write_timeout));
    let _ = stream.set_nodelay(true);
    let deadline = Instant::now() + ctx.read_timeout;
    let mut reader = BufReader::with_capacity(8192, DeadlineStream { stream, deadline });

    let body = match read_hook_request_body(&mut reader) {
        Ok(body) => body,
        Err(RequestError::EmptyConnection) => return,
        Err(RequestError::TimedOut) => {
            tracing::debug!("[hook-server] 连接读取超时，已主动断开");
            return;
        }
        Err(RequestError::MethodNotAllowed) => {
            send_http_response(
                &mut reader.get_mut().stream,
                405,
                "Method Not Allowed",
                "Method Not Allowed",
            );
            return;
        }
        Err(RequestError::NotFound) => {
            send_http_response(&mut reader.get_mut().stream, 404, "Not Found", "Not Found");
            return;
        }
        Err(RequestError::PayloadTooLarge) => {
            tracing::warn!(
                "[hook-server] body 超过 {} 字节上限，已拒绝",
                MAX_HOOK_BODY_BYTES
            );
            send_http_response(
                &mut reader.get_mut().stream,
                413,
                "Payload Too Large",
                "Payload Too Large",
            );
            return;
        }
        Err(RequestError::BadRequest) => {
            send_http_response(
                &mut reader.get_mut().stream,
                400,
                "Bad Request",
                "Bad Request",
            );
            return;
        }
    };

    // 解析 JSON payload
    let payload: HookPayload = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("[hook-server] JSON 解析失败: {}", e);
            send_http_response(
                &mut reader.get_mut().stream,
                400,
                "Bad Request",
                "Bad Request",
            );
            return;
        }
    };

    // 立即响应 200 并释放连接，不阻塞 hook 脚本
    send_http_response(&mut reader.get_mut().stream, 200, "OK", "OK");
    drop(reader);

    // 串行处理事件，确保与 OSC 7501 / 打断之间的跨表状态与去重发射顺序一致
    let _dispatch_guard = ctx.hook_state.dispatch_lock.lock();
    handle_hook_payload(payload, &ctx.hook_state, &ctx.emitter, &ctx.tracker);
}

fn handle_hook_payload(
    payload: HookPayload,
    hook_state: &HookState,
    emitter: &StatusEmitter,
    tracker: &SessionTracker,
) {
    // 处理事件。事件名优先取 sidecar 从 argv 注入的 `event`；缺失时回落
    // payload 自带的 `hook_event_name`（用户手改配置漏写命令行参数的兜底，
    // 否则整条事件会因无事件名被静默丢弃）。
    let resolved_event = payload
        .event
        .clone()
        .or_else(|| payload.hook_event_name.clone());
    let (Some(pty_id), Some(event)) = (payload.pty_id, resolved_event.as_deref()) else {
        return;
    };

    let route = route_event(event, &payload);
    if let EventRoute::Subagent { settled } = route {
        // agy 子代理会话的事件:它带自己的 conversationId,记下来会把 pane 的
        // 会话身份换成子代理的,状态也会被子代理的 Stop 提前收成「完成」。
        // 只拿它追踪在跑的子代理:主会话的完成可能正等着它们收尾
        let deferred = match payload.session_id.as_deref() {
            Some(sid) if settled => hook_state.agy_subagent_settled(pty_id, sid),
            Some(sid) => {
                hook_state.agy_subagent_active(pty_id, sid);
                None
            }
            None => None,
        };
        tracing::info!(
            "[hook-server] pty_id={} event={} 来自 agy 子代理会话 {:?}(settled={})，不改状态",
            pty_id,
            event,
            payload.session_id,
            settled
        );
        let Some(reason) = deferred else { return };
        if !deferred_stop_applies(
            hook_state.get_status(pty_id).as_deref(),
            emitter.last_cause(pty_id).as_deref(),
        ) {
            tracing::info!(
                "[hook-server] pty_id={} 子代理已全部收尾,但主会话已不在工作中(打断/退出),延后的完成作废",
                pty_id
            );
            return;
        }
        if let EventRoute::Status { status, cause } =
            route_agy_event("Stop", reason.as_deref(), None, None)
        {
            tracker.mark_ai_session(pty_id, AgentKind::Agy.key());
            if let Some(agent) = payload.agent.as_deref().filter(|a| !a.is_empty()) {
                hook_state
                    .tables
                    .lock()
                    .last_agent
                    .insert(pty_id, agent.to_string());
            }
            hook_state.update(pty_id, status.to_string());
            emitter.emit_if_changed(pty_id, status, Some(cause), payload.agent.clone());
            tracing::info!(
                "[hook-server] pty_id={} 子代理已全部收尾,补记主会话延后的完成 -> status={} cause={}",
                pty_id,
                status,
                cause
            );
        }
        return;
    }

    if event == "SessionEnd" {
        // 只用 payload 自带的 session_id 打墓碑。不要退回 session_of:
        // 新会话的 SessionStart 若先到,session_of 已是新会话,兜底会
        // 把新会话误打进墓碑,冻结其全部后续事件。
        if let Some(sid) = payload.session_id.clone() {
            hook_state.mark_session_ended(pty_id, sid);
        }
        let was_last = hook_state.end_session(pty_id, payload.session_id.as_deref());
        if payload.reason.as_deref() == Some("clear") {
            // /clear 换会话不是退出：紧随其后的 SessionStart 会带新
            // session id 刷新状态，这里只靠墓碑挡住旧会话的迟到事件
            tracing::info!(
                "[hook-server] pty_id={} event=SessionEnd(clear) -> 仅记录墓碑",
                pty_id
            );
        } else if !was_last {
            // pane 上还有别的活跃会话:嵌套非交互实例(Bash 工具里跑
            // `claude -p` / `codex exec`,继承 MINITERM_PTY_ID)结束,
            // 或退出后立刻重开、新 SessionStart 先到的乱序。此时清
            // hook 状态 / AI 会话标记会误杀仍在跑的会话,只留墓碑。
            tracing::info!(
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
            tracing::info!(
                "[hook-server] pty_id={} event=SessionEnd(reason={:?}) -> hook 已清除，回退到 idle",
                pty_id,
                payload.reason
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
                tracing::info!(
                    "[hook-server] pty_id={} event={} 来自已结束会话 {}，忽略",
                    pty_id,
                    event,
                    sid
                );
                return;
            }
        }
        // 会话身份先于状态映射记录:即使事件不映射状态(如未知事件),
        // session_id 也是有效信息;/clear 换会话时靠这里自动刷新
        if let Some(sid) = payload.session_id.clone() {
            // 活跃集只服务 SessionEnd 的「是不是最后一个」判定。不发 SessionEnd
            // 的 agent(agy)的会话进去了就永远出不来:同一 pane 之后再跑 claude,
            // claude 退出时的 SessionEnd 会被这些残留挡成「非最后活跃会话」,
            // 状态收不回 idle。所以它们不进
            if agent_sends_session_end(payload.agent.as_deref()) {
                hook_state.note_session_active(pty_id, &sid);
            }
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
        // agy 主会话的事件:`Stop(fullyIdle=false)` 记下延后的完成,其余任何事件
        // (继续干活 / 自己的 fullyIdle=true 收尾)都让它作废
        if payload.agent.as_deref().and_then(AgentKind::parse) == Some(AgentKind::Agy) {
            if route == EventRoute::DeferStop {
                hook_state.agy_defer_stop(pty_id, payload.termination_reason.clone());
            } else {
                hook_state.agy_clear_deferred_stop(pty_id);
            }
        }
        if let EventRoute::Status { status, cause } = route {
            // hook 事件是 AI 进程存活的直接证据:输入检测漏判启动
            // (别名/包装脚本)或误判退出(任务中双击 Ctrl+C)时,
            // 靠这里把 AI 会话标记扶正,保住后续 marker/移动端语义
            tracker.mark_ai_session(
                pty_id,
                payload.agent.as_deref().unwrap_or(AgentKind::Claude.key()),
            );
            if let Some(agent) = payload.agent.as_deref().filter(|a| !a.is_empty()) {
                hook_state
                    .tables
                    .lock()
                    .last_agent
                    .insert(pty_id, agent.to_string());
            }
            hook_state.update(pty_id, status.to_string());

            // 通知上层（与 monitor 共享同一份去重表）。cause 带
            // 归一化后的事件名(见 route_mapped):Stop/PermissionRequest/
            // Notification 都落 ai-idle,但只有 Stop 是「任务做完了」,UI 据此
            // 决定播报与托盘黄绿灯（见 notify.rs 与 store 的 attention 判定）
            emitter.emit_if_changed(pty_id, status, Some(cause), payload.agent.clone());

            tracing::info!(
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
        } else if let EventRoute::KeepStatus(why) = route {
            tracing::info!("[hook-server] pty_id={} event={} {}", pty_id, event, why);
        } else if route == EventRoute::DeferStop {
            tracing::info!(
                "[hook-server] pty_id={} event={} fullyIdle=false:后台子代理还在跑,完成延后到它们收尾",
                pty_id,
                event
            );
        }
    }
}

/// 在已绑定的 `TcpListener` 上启动 hook HTTP 监听循环。
/// 返回 `Ok(Some(port))` 表示本次安装成功，`Ok(None)` 表示并发竞态下已有实例在跑。
fn start_hook_server_on_listener(
    listener: TcpListener,
    hook_state: HookState,
    emitter: StatusEmitter,
    tracker: SessionTracker,
    read_timeout: Duration,
    write_timeout: Duration,
) -> Result<Option<u16>, String> {
    let addr = listener
        .local_addr()
        .map_err(|e| format!("获取 hook 服务器监听地址失败: {}", e))?;
    let port = addr.port();

    let shutdown = Arc::new(AtomicBool::new(false));
    let pool = Arc::new(WorkerPool::new(
        MAX_CONCURRENT_WORKERS,
        MAX_QUEUED_CONNECTIONS,
    ));
    let ctx = Arc::new(ServerContext {
        hook_state: hook_state.clone(),
        emitter,
        tracker,
        read_timeout,
        write_timeout,
    });

    let accept_shutdown = Arc::clone(&shutdown);
    let accept_pool = Arc::clone(&pool);
    let accept_thread = thread::Builder::new()
        .name("mt-hook-accept".into())
        .spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if accept_shutdown.load(Ordering::Acquire) {
                            drop(stream);
                            break;
                        }
                        accept_pool.submit(stream, &ctx);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        if accept_shutdown.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    Err(e) => {
                        if accept_shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        tracing::warn!("[hook-server] accept 失败: {}", e);
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        })
        .map_err(|e| format!("启动 hook 监听线程失败: {}", e))?;

    let handle = Arc::new(ServerHandle {
        addr: Some(addr),
        shutdown,
        pool,
        accept_thread: Mutex::new(Some(accept_thread)),
    });

    {
        let mut slot = hook_state.server.lock();
        if slot.is_some() {
            drop(slot);
            handle.stop();
            return Ok(None);
        }
        *slot = Some(handle);
        hook_state.set_port(port);
    }

    Ok(Some(port))
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
        tracing::info!("[hook-server] 服务器已在运行，跳过启动");
        return Ok(());
    }

    // 在当前线程绑定端口，以便同步得知结果
    let bound = {
        let mut result = None;
        for offset in 0..MAX_PORT_ATTEMPTS {
            let port = DEFAULT_PORT + offset;
            let addr = format!("127.0.0.1:{}", port);
            match TcpListener::bind(&addr) {
                Ok(listener) => {
                    tracing::info!("[hook-server] 监听 {}", addr);
                    result = Some(listener);
                    break;
                }
                Err(e) => {
                    tracing::warn!("[hook-server] 端口 {} 被占用: {}", port, e);
                }
            }
        }
        result
    };

    let listener = match bound {
        Some(l) => l,
        None => {
            tracing::error!("[hook-server] 无法绑定任何端口，hook 服务器未启动");
            return Err("无法绑定端口 (23456-23460)，hook 服务器启动失败".to_string());
        }
    };

    if let Some(port) = start_hook_server_on_listener(
        listener,
        hook_state,
        emitter,
        tracker,
        CONNECTION_READ_TIMEOUT,
        CONNECTION_WRITE_TIMEOUT,
    )? {
        // 写入端口文件
        write_port_file(&data_dir, port);
    }

    Ok(())
}

/// 停止 hook HTTP 服务器
///
/// 取出保存的 server 句柄，立刻中断阻塞的监听循环，
/// 清理端口文件并重置端口。
pub fn stop_hook_server(hook_state: &HookState, data_dir: &Path) {
    let server = {
        let mut slot = hook_state.server.lock();
        hook_state.set_port(0);
        slot.take()
    };
    if let Some(s) = server {
        s.stop();
        tracing::info!("[hook-server] 服务器已停止");
    }
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
        tracing::warn!("[hook-server] 写入端口文件失败 {}: {}", path.display(), e);
    } else {
        tracing::info!("[hook-server] 端口文件已写入 {}", path.display());
    }
}

/// 删除端口文件 `{data_dir}/hook-server.json`
fn delete_port_file(dir: &Path) {
    let path = dir.join("hook-server.json");
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!("[hook-server] 删除端口文件失败 {}: {}", path.display(), e);
        } else {
            tracing::info!("[hook-server] 端口文件已删除 {}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        state.set_server(Some(Arc::new(ServerHandle::for_test())));

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

    // ---- pi 特有语义 ----

    /// pi 扩展的上报表同样必须全有状态映射;它沿用 omp 的口径(打断 = Stop+aborted,
    /// 扩展对话框 = Elicitation 黄灯 / ElicitationResult 熄灯),hook server 不为它开特例。
    #[test]
    fn pi_reported_event_set_is_fully_mapped() {
        for event in crate::hook_registry::PI_REPORTED_EVENTS {
            if *event == "SessionEnd" {
                continue;
            }
            assert!(
                map_event(event, Some("pi"), None).is_some(),
                "{event} 无状态映射"
            );
        }
        assert_eq!(
            map_event_to_status("Stop", Some("pi"), None, None, Some("aborted")),
            Some("ai-idle")
        );
        assert_eq!(map_event("Elicitation", Some("pi"), None), Some("ai-idle"));
        assert_eq!(
            map_event("ElicitationResult", Some("pi"), None),
            Some("ai-working")
        );
        assert!(is_attention_cause(event_cause("StopFailure", None, None)));
    }

    // ---- Antigravity CLI(agy)特有语义 ----

    fn payload(value: serde_json::Value) -> HookPayload {
        serde_json::from_value(value).expect("测试 payload 应能反序列化")
    }

    fn status(status: &'static str, cause: &'static str) -> EventRoute<'static> {
        EventRoute::Status { status, cause }
    }

    /// 子代理会话的事件(`lastUserInput` 为空串 → sidecar 转成 `has_user_input: false`)
    /// 一律不改状态、不记身份 —— 子代理的 Stop 不是主会话的完成,它的 id 也不能顶掉
    /// pane 的会话身份;只有它的收尾 Stop(fullyIdle 不是 false)标成 settled。
    #[test]
    fn agy_subagent_events_never_touch_status() {
        for event in ["SessionStart", "PreInvocation", "PostToolUse"] {
            assert_eq!(
                route_agy_event(event, Some("NO_TOOL_CALL"), Some(true), Some(false)),
                EventRoute::Subagent { settled: false },
                "{event}"
            );
        }
        assert_eq!(
            route_agy_event("Stop", Some("NO_TOOL_CALL"), Some(true), Some(false)),
            EventRoute::Subagent { settled: true }
        );
        assert_eq!(
            route_agy_event("Stop", Some("NO_TOOL_CALL"), None, Some(false)),
            EventRoute::Subagent { settled: true }
        );
        // 子代理自己也在等孙代理:还没收尾
        assert_eq!(
            route_agy_event("Stop", Some("NO_TOOL_CALL"), Some(false), Some(false)),
            EventRoute::Subagent { settled: false }
        );
        // 缺这一位(老 sidecar / 手工 POST)不当子代理
        assert_eq!(
            route_agy_event("PreInvocation", None, None, None),
            status("ai-working", "PreInvocation")
        );
    }

    /// 每次调模型前的 PreInvocation 是 agy 唯一注册的「工作中」事件;没注册的
    /// PostInvocation / PostToolUse 被用户手工加上也一样按工作中算。
    #[test]
    fn agy_invocation_events_map_to_working() {
        for event in ["PreInvocation", "PostInvocation", "PostToolUse"] {
            assert_eq!(
                route_agy_event(event, None, None, Some(true)),
                status("ai-working", event)
            );
        }
        // 其余事件走既有映射(SessionStart → ai-idle)
        assert_eq!(
            route_agy_event("SessionStart", None, None, Some(true)),
            status("ai-idle", "SessionStart")
        );
        assert_eq!(
            route_agy_event("SomethingNew", None, None, Some(true)),
            EventRoute::KeepStatus("无状态映射，已忽略")
        );
    }

    /// Stop 的四条分支:等子代理 → 不改;用户取消 → 打断;被迫停下 → StopFailure 黄灯;
    /// 其余 → 正常完成。
    #[test]
    fn agy_stop_routes_by_termination_reason() {
        // 后台子代理还在跑:完成延后到它们收尾(或被主会话的新事件作废)
        assert_eq!(
            route_agy_event("Stop", Some("NO_TOOL_CALL"), Some(false), Some(true)),
            EventRoute::DeferStop
        );
        // 用户取消:回合结束但不算完成(与 omp / pi 的 aborted 同一条路)
        assert_eq!(
            route_agy_event("Stop", Some("USER_CANCELED"), Some(true), Some(true)),
            status("ai-idle", "Interrupt")
        );
        // 被迫停下:不播报完成,点黄灯提示回来看
        for reason in [
            "ERROR",
            "MAX_INVOCATIONS",
            "MAX_FORCED_INVOCATIONS",
            "MAX_TOKEN_BUDGET_EXCEEDED",
        ] {
            let route = route_agy_event("Stop", Some(reason), Some(true), Some(true));
            assert_eq!(route, status("ai-idle", "StopFailure"), "{reason}");
            assert!(is_attention_cause("StopFailure"));
        }
        // 正常完成及其余一切(含未知 / 缺失 / fullyIdle 缺失)
        for reason in [
            Some("NO_TOOL_CALL"),
            Some("TERMINAL_STEP_TYPE"),
            Some("TERMINAL_CUSTOM_HOOK"),
            Some("INJECTED_RESPONSE"),
            Some("SOMETHING_NEW"),
            None,
        ] {
            assert_eq!(
                route_agy_event("Stop", reason, Some(true), Some(true)),
                status("ai-idle", "Stop"),
                "{reason:?}"
            );
        }
        assert_eq!(
            route_agy_event("Stop", Some("NO_TOOL_CALL"), None, None),
            status("ai-idle", "Stop")
        );
    }

    /// 只有 agent=agy 才走 agy 的归一化:别家 payload 里出现同名字段也不受影响。
    #[test]
    fn agy_routing_is_keyed_on_agent_field() {
        let agy = payload(serde_json::json!({
            "agent": "agy", "has_user_input": false
        }));
        assert_eq!(
            route_event("Stop", &agy),
            EventRoute::Subagent { settled: true }
        );

        let claude = payload(serde_json::json!({
            "agent": "claude-code", "has_user_input": false, "fully_idle": false,
            "termination_reason": "ERROR"
        }));
        assert_eq!(route_event("Stop", &claude), status("ai-idle", "Stop"));
        assert_eq!(
            route_event("PreInvocation", &claude),
            EventRoute::KeepStatus("无状态映射，已忽略"),
            "PreInvocation 不是别家的事件"
        );
    }

    /// 主会话 `Stop(fullyIdle=false)` 之后的两种实测时序(agy 1.5.12):
    /// ① 主会话收到子代理回报后接着干活 —— 它的新事件作废延后,子代理收尾时不补记;
    /// ② 主会话答完就停、之后只剩子代理收尾 —— 子代理全部收尾时才补记那次 Stop。
    /// 多个子代理并行时,要等最后一个收尾。
    #[test]
    fn agy_deferred_stop_waits_for_all_subagents() {
        let hs = HookState::new();
        // 时序 ②:主会话等着,单个子代理收尾 → 补记(带主会话那次的 reason)
        hs.agy_subagent_active(1, "sub-a");
        hs.agy_defer_stop(1, Some("NO_TOOL_CALL".into()));
        assert_eq!(
            hs.agy_subagent_settled(1, "sub-a"),
            Some(Some("NO_TOOL_CALL".to_string()))
        );
        assert_eq!(hs.agy_subagent_settled(1, "sub-a"), None, "只补记一次");

        // 时序 ①:主会话延后后又有动静 → 作废,子代理收尾不补记
        hs.agy_subagent_active(2, "sub-b");
        hs.agy_defer_stop(2, Some("NO_TOOL_CALL".into()));
        hs.agy_clear_deferred_stop(2);
        assert_eq!(hs.agy_subagent_settled(2, "sub-b"), None);

        // 并行两个子代理:第一个收尾时还有一个在跑,不补记;第二个收尾才补记
        hs.agy_subagent_active(3, "x");
        hs.agy_subagent_active(3, "y");
        hs.agy_defer_stop(3, None);
        assert_eq!(hs.agy_subagent_settled(3, "x"), None);
        assert_eq!(hs.agy_subagent_settled(3, "y"), Some(None));

        // 没有延后的完成时,子代理收尾什么都不补
        hs.agy_subagent_active(4, "z");
        assert_eq!(hs.agy_subagent_settled(4, "z"), None);

        // pane 的 hook 状态被清掉(退出 / SessionEnd 兜底)时,两张表一并清
        hs.agy_subagent_active(5, "w");
        hs.agy_defer_stop(5, None);
        hs.remove(5);
        assert_eq!(hs.agy_subagent_settled(5, "w"), None);
    }

    /// 补记只在主会话还算「这一回合里」时生效:工作中,或只是被停摆兜底悄悄收了;
    /// 打断过、已退出、其余 ai-idle 一律作废(宁可少报一次完成,不报假完成)。
    #[test]
    fn deferred_stop_applies_only_while_turn_is_live() {
        assert!(deferred_stop_applies(
            Some("ai-working"),
            Some("PreInvocation")
        ));
        assert!(deferred_stop_applies(Some("ai-working"), None));
        assert!(deferred_stop_applies(Some("ai-idle"), Some("Stall")));
        assert!(!deferred_stop_applies(Some("ai-idle"), Some("Interrupt")));
        assert!(!deferred_stop_applies(Some("ai-idle"), Some("Stop")));
        assert!(!deferred_stop_applies(Some("idle"), Some("StallExit")));
        assert!(!deferred_stop_applies(None, None));
    }

    /// 改造成 `route_event` 后别家的口径逐条不变(打断 / 黄灯归一化 / 收尾 Stop / 未知事件)。
    #[test]
    fn route_event_keeps_other_agents_unchanged() {
        let omp = payload(serde_json::json!({ "agent": "omp", "reason": "aborted" }));
        assert_eq!(route_event("Stop", &omp), status("ai-idle", "Interrupt"));

        let grok = payload(serde_json::json!({
            "agent": "grok", "notification_type": "permission_prompt"
        }));
        assert_eq!(
            route_event("Notification", &grok),
            status("ai-idle", "PermissionRequest")
        );
        let grok_teardown = payload(serde_json::json!({ "agent": "grok", "reason": "shutdown" }));
        assert!(matches!(
            route_event("Stop", &grok_teardown),
            EventRoute::KeepStatus(_)
        ));

        let codex = payload(serde_json::json!({ "agent": "codex" }));
        assert_eq!(
            route_event("PermissionRequest", &codex),
            status("ai-working", "PermissionRequest")
        );
        let bare = payload(serde_json::json!({}));
        assert_eq!(
            route_event("UserPromptSubmit", &bare),
            status("ai-working", "UserPromptSubmit")
        );
    }

    /// 发不发 SessionEnd 查 agent 表;缺省按 Claude,认不出的按「发」(保持原行为)。
    #[test]
    fn session_end_capability_follows_agent_table() {
        assert!(agent_sends_session_end(None));
        assert!(agent_sends_session_end(Some("")));
        assert!(agent_sends_session_end(Some("claude-code")));
        assert!(agent_sends_session_end(Some("pi")));
        assert!(agent_sends_session_end(Some("some-wrapper-x")));
        assert!(!agent_sends_session_end(Some("agy")));
        assert!(!agent_sends_session_end(Some("opencode")));
    }

    /// agy 的 pane 同样走得到 Esc 打断收敛(agy 打断不发任何 hook,实测)。
    #[test]
    fn agy_interrupt_is_settled_by_input_detection() {
        let state = HookState::new();
        state.update(1, "ai-working".to_string());
        assert!(interrupt_should_settle(&state, 1));
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

    // ---- HTTP 服务端健壮性与并发超时测试 ----

    struct TestHookServer {
        state: HookState,
        port: u16,
        events: Arc<Mutex<Vec<crate::monitor::StatusChange>>>,
    }

    impl TestHookServer {
        fn start(read_timeout: Duration) -> Self {
            let events: Arc<Mutex<Vec<crate::monitor::StatusChange>>> =
                Arc::new(Mutex::new(Vec::new()));
            let sink_events = Arc::clone(&events);
            let emitter = StatusEmitter::new(Arc::new(move |c: crate::monitor::StatusChange| {
                sink_events.lock().push(c);
            }));
            let state = HookState::new();
            let tracker = SessionTracker::new();
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            let port = start_hook_server_on_listener(
                listener,
                state.clone(),
                emitter,
                tracker,
                read_timeout,
                Duration::from_secs(2),
            )
            .expect("start test hook server")
            .expect("server installed");
            Self {
                state,
                port,
                events,
            }
        }

        fn send_raw(&self, raw: &[u8]) -> String {
            let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");
            stream.write_all(raw).expect("write request");
            stream.flush().expect("flush");
            let mut resp = String::new();
            let _ = stream.read_to_string(&mut resp);
            resp
        }

        fn wait_for_status(&self, pty_id: u32, expected: &str) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if self.state.get_status(pty_id).as_deref() == Some(expected) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!(
                "pty_id={pty_id} 未在时限内变为 {expected:?}，当前={:?}",
                self.state.get_status(pty_id)
            );
        }
    }

    impl Drop for TestHookServer {
        fn drop(&mut self) {
            let server = self.state.server.lock().take();
            if let Some(s) = server {
                s.stop();
            }
            self.state.set_port(0);
        }
    }

    /// 慢速 / 挂起的 TCP 连接（只发请求头不发 body）不得阻塞后续正常的 `/hook` 上报，
    /// 且服务端会在 read_timeout 到期后主动断开该挂起连接。
    #[test]
    fn slow_stalled_connection_does_not_block_subsequent_hooks_and_times_out() {
        let srv = TestHookServer::start(Duration::from_millis(250));

        // 1. 建立一个只发请求头、声明 64 字节 body 却永不发送的慢连接
        let mut stalled = TcpStream::connect(("127.0.0.1", srv.port)).expect("connect stalled");
        stalled
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set timeout");
        stalled
            .write_all(b"POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 64\r\n\r\n")
            .expect("write partial headers");
        stalled.flush().expect("flush");

        // 2. 趁慢连接挂着，立刻发一条正常的 /hook 上报，必须在远短于超时时间内完成
        let body =
            r#"{"pty_id":42,"event":"UserPromptSubmit","agent":"claude-code","session_id":"s1"}"#;
        let req = format!(
            "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\ncOnTeNt-LeNgTh: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let started = Instant::now();
        let resp = srv.send_raw(req.as_bytes());
        assert!(
            resp.starts_with("HTTP/1.1 200 OK\r\n"),
            "正常请求被阻或返回异常: {resp:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "正常请求耗时 {:?}，疑似被慢连接阻塞",
            started.elapsed()
        );
        srv.wait_for_status(42, "ai-working");
        assert!(
            srv.events
                .lock()
                .iter()
                .any(|e| e.pty_id == 42 && e.status == "ai-working")
        );

        // 3. 验证慢连接在超时后被服务端自行关闭（对端读到 EOF 或连接重置）
        let mut buf = [0u8; 64];
        let closed = match stalled.read(&mut buf) {
            Ok(0) => true,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                true
            }
            other => panic!("慢连接应在超时后由服务端断开，实测: {other:?}"),
        };
        assert!(closed);
    }

    /// Transfer-Encoding: chunked（含分片、chunk extension 与 trailer）与 1 MiB 上限校验。
    #[test]
    fn http_server_supports_chunked_encoding_and_size_limit() {
        let srv = TestHookServer::start(Duration::from_secs(2));

        let part1 = r#"{"pty_id":7,"event":"#;
        let part2 = r#""Stop","agent":"codex"}"#;
        let req = format!(
            "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\ntRaNsFeR-eNcOdInG: chunked\r\n\r\n\
             {:x};ext=1\r\n{}\r\n{:x}\r\n{}\r\n0\r\nX-Trailer: done\r\n\r\n",
            part1.len(),
            part1,
            part2.len(),
            part2
        );
        let resp = srv.send_raw(req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp:?}");
        srv.wait_for_status(7, "ai-idle");

        // 超过 1 MiB 上限的分块立即返回 413，不尝试分配巨额缓冲
        let huge_chunk = format!(
            "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            MAX_HOOK_BODY_BYTES + 1
        );
        let resp = srv.send_raw(huge_chunk.as_bytes());
        assert!(
            resp.starts_with("HTTP/1.1 413 Payload Too Large\r\n"),
            "{resp:?}"
        );
    }

    /// Expect: 100-continue 握手，以及超限 Content-Length 直接 413 而不回 100 Continue。
    #[test]
    fn http_server_handles_expect_100_continue() {
        let srv = TestHookServer::start(Duration::from_secs(2));

        let body = r#"{"pty_id":9,"event":"PreToolUse","agent":"claude-code"}"#;
        let mut stream = TcpStream::connect(("127.0.0.1", srv.port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set timeout");
        let headers = format!(
            "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).expect("write headers");
        stream.flush().expect("flush");

        let mut interim = [0u8; 64];
        let n = stream.read(&mut interim).expect("read 100 continue");
        let interim_str = std::str::from_utf8(&interim[..n]).expect("utf8");
        assert!(
            interim_str.starts_with("HTTP/1.1 100 Continue\r\n\r\n"),
            "未收到 100 Continue: {interim_str:?}"
        );

        stream.write_all(body.as_bytes()).expect("write body");
        stream.flush().expect("flush");
        let mut final_resp = String::new();
        let _ = stream.read_to_string(&mut final_resp);
        assert!(
            final_resp.starts_with("HTTP/1.1 200 OK\r\n"),
            "{final_resp:?}"
        );
        srv.wait_for_status(9, "ai-working");

        // 超大 Content-Length + Expect: 100-continue 必须直接 413
        let oversized = format!(
            "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n",
            MAX_HOOK_BODY_BYTES + 1
        );
        let resp = srv.send_raw(oversized.as_bytes());
        assert!(
            resp.starts_with("HTTP/1.1 413 Payload Too Large\r\n"),
            "{resp:?}"
        );
    }

    /// 路由、方法、错误 JSON、超限 Content-Length 状态码与 stop 立即释放端口。
    #[test]
    fn http_server_status_codes_and_immediate_stop() {
        let srv = TestHookServer::start(Duration::from_secs(2));
        let port = srv.port;

        let r405 = srv.send_raw(b"GET /hook HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(
            r405.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
            "{r405:?}"
        );

        let r404 =
            srv.send_raw(b"POST /other HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\n\r\n{}");
        assert!(r404.starts_with("HTTP/1.1 404 Not Found\r\n"), "{r404:?}");

        let r413 = srv.send_raw(
            format!(
                "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
                MAX_HOOK_BODY_BYTES + 1
            )
            .as_bytes(),
        );
        assert!(
            r413.starts_with("HTTP/1.1 413 Payload Too Large\r\n"),
            "{r413:?}"
        );

        let r400 = srv
            .send_raw(b"POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4\r\n\r\n{bad");
        assert!(r400.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{r400:?}");

        // stop 后立刻释放端口且 is_server_running 变为 false，且 Drop 不再触发第二次 200ms 唤醒超时
        let state = srv.state.clone();
        let stop_started = Instant::now();
        drop(srv);
        assert!(
            stop_started.elapsed() < Duration::from_millis(150),
            "stop_hook_server + Drop 不应重复触发 200ms 唤醒超时，实际耗时: {:?}",
            stop_started.elapsed()
        );
        assert!(!state.is_server_running());
        assert_eq!(state.get_port(), 0);
        let rebound = TcpListener::bind(("127.0.0.1", port));
        assert!(rebound.is_ok(), "stop 后端口应立即释放，实际: {rebound:?}");
    }

    // ---- OSC 7501（Program Status Protocol，agy 权限/提问黄灯） ----

    fn osc(
        state: OscProgramState,
        kind: Option<OscBlockedKind>,
        app: Option<&str>,
        id: Option<&str>,
        msg: Option<&str>,
    ) -> OscStatus {
        OscStatus {
            state,
            kind,
            app: app.map(str::to_string),
            id: id.map(str::to_string),
            msg: msg.map(str::to_string),
        }
    }

    /// agy 权限弹窗与提问弹窗分别映射为 PermissionRequest / Elicitation 黄灯，
    /// 恢复工作（working）时熄灭黄灯，完成（done）报 Stop，出错（error）报 StopFailure。
    #[test]
    fn agy_osc7501_maps_permission_question_working_done_and_error() {
        let state = HookState::new();
        let tracker = SessionTracker::new();
        let seen = Arc::new(Mutex::new(Vec::<(String, Option<String>, Option<String>)>::new()));
        let seen_cb = Arc::clone(&seen);
        let emitter = StatusEmitter::new(Arc::new(move |c: crate::monitor::StatusChange| {
            seen_cb.lock().push((c.status, c.cause, c.agent));
        }));

        // 1. 权限确认框 -> PermissionRequest 黄灯
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            7,
            osc(
                OscProgramState::Blocked,
                Some(OscBlockedKind::Permission),
                Some("antigravity"),
                None,
                Some("VHJ1c3Q="),
            ),
        );
        // 2. 同条弹窗重绘/导航再发一遍相同 blocked -> 去重不重复响铃
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            7,
            osc(
                OscProgramState::Blocked,
                Some(OscBlockedKind::Permission),
                Some("antigravity"),
                None,
                Some("VHJ1c3Q="),
            ),
        );
        // 3. 用户批准后恢复 working -> 熄灭黄灯
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            7,
            osc(OscProgramState::Working, None, Some("antigravity"), None, None),
        );
        // 4. 提问框 -> Elicitation 黄灯
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            7,
            osc(
                OscProgramState::Blocked,
                Some(OscBlockedKind::Question),
                Some("antigravity"),
                None,
                Some("UHJvY2VlZD8="),
            ),
        );
        // 5. 多题提问切到下一题（msg 变化）-> 再次触发 Elicitation
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            7,
            osc(
                OscProgramState::Blocked,
                Some(OscBlockedKind::Question),
                Some("antigravity"),
                None,
                Some("TmV4dD8="),
            ),
        );
        // 6. 作答后继续干活再完成 -> Stop
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            7,
            osc(OscProgramState::Working, None, Some("antigravity"), None, None),
        );
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            7,
            osc(OscProgramState::Done, None, Some("antigravity"), None, None),
        );
        // 7. 紧接着 HTTP hook 的 Stop 到达 -> 与 OSC done 同为 ("ai-idle", Some("Stop"))，绝不双播完成
        handle_hook_payload(
            payload(serde_json::json!({
                "pty_id": 7,
                "event": "Stop",
                "agent": "agy",
                "session_id": "conv-1",
                "termination_reason": "NO_TOOL_CALL",
                "fully_idle": true,
                "has_user_input": true
            })),
            &state,
            &emitter,
            &tracker,
        );
        // 8. 回合出错 -> StopFailure 黄灯
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            7,
            osc(OscProgramState::Error, None, Some("antigravity"), None, None),
        );

        let events = seen.lock().clone();
        assert_eq!(
            events,
            vec![
                ("ai-idle".into(), Some("PermissionRequest".into()), Some("agy".into())),
                ("ai-working".into(), Some("OscWorking".into()), Some("agy".into())),
                ("ai-idle".into(), Some("Elicitation".into()), Some("agy".into())),
                ("ai-idle".into(), Some("Elicitation".into()), Some("agy".into())),
                ("ai-working".into(), Some("OscWorking".into()), Some("agy".into())),
                ("ai-idle".into(), Some("Stop".into()), Some("agy".into())),
                ("ai-idle".into(), Some("StopFailure".into()), Some("agy".into())),
            ]
        );
        assert!(tracker.is_ai_session(7));
        assert_eq!(state.agent_of(7).as_deref(), Some("agy"));
    }

    /// 非 agy 程序（普通 CLI 或别家 agent）以及子任务非 blocked 状态一律忽略；
    /// 子任务 blocked（确需人工批准）仍点灯。
    #[test]
    fn osc7501_ignores_non_agy_apps_and_subtask_non_blocked() {
        let state = HookState::new();
        let tracker = SessionTracker::new();
        let seen = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let seen_cb = Arc::clone(&seen);
        let emitter = StatusEmitter::new(Arc::new(move |c: crate::monitor::StatusChange| {
            seen_cb.lock().push((c.status, c.cause));
        }));

        for app in [None, Some("brew"), Some("terraform"), Some("claude"), Some("codex")] {
            observe_osc_status(
                &state,
                &emitter,
                &tracker,
                3,
                osc(OscProgramState::Blocked, Some(OscBlockedKind::Permission), app, None, None),
            );
        }
        // 子任务 done 不得误报主会话完成
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            3,
            osc(OscProgramState::Done, None, Some("antigravity"), Some("sub/1"), None),
        );
        assert!(seen.lock().is_empty());
        assert!(!state.is_hook_enabled(3));

        // 子任务 blocked 仍点灯
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            3,
            osc(
                OscProgramState::Blocked,
                Some(OscBlockedKind::Permission),
                Some("antigravity"),
                Some("sub/1"),
                Some("cA=="),
            ),
        );
        assert_eq!(
            seen.lock().clone(),
            vec![("ai-idle".into(), Some("PermissionRequest".into()))]
        );
    }

    /// `/exit` 退出瞬间 agy 再吐出的 `state=idle` 不得复活已被清掉的 AI 会话标记；
    /// 随后发 `state=clear` 直接回落 idle；而正常启动时的 `state=idle` 正常扶正会话。
    #[test]
    fn agy_osc7501_exit_ignores_trailing_idle_and_honors_clear() {
        let state = HookState::new();
        let tracker = SessionTracker::new();
        let seen = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let seen_cb = Arc::clone(&seen);
        let emitter = StatusEmitter::new(Arc::new(move |c: crate::monitor::StatusChange| {
            seen_cb.lock().push((c.status, c.cause));
        }));

        // 别名启动（输入检测未认出）发 state=idle -> 扶正会话
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            9,
            osc(OscProgramState::Idle, None, Some("antigravity"), None, None),
        );
        assert!(tracker.is_ai_session(9));
        assert_eq!(state.get_status(9).as_deref(), Some("ai-idle"));

        // 用户敲 /exit -> 输入检测清会话并记退出时刻
        tracker.track_input(9, "/exit\r");
        assert!(!tracker.is_ai_session(9));

        // agy 临死前 100ms 内再发 state=idle -> 忽略，不复活会话
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            9,
            osc(OscProgramState::Idle, None, Some("antigravity"), None, None),
        );
        assert!(!tracker.is_ai_session(9), "退出窗口内的 state=idle 不得复活会话");

        // 若进程发了 state=clear -> 立即清状态回落 idle
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            9,
            osc(OscProgramState::Clear, None, Some("antigravity"), None, None),
        );
        assert!(!state.is_hook_enabled(9));
        assert_eq!(
            seen.lock().last().cloned(),
            Some(("idle".into(), Some("SessionEnd".into())))
        );
    }

    /// HTTP hook `Stop(fullyIdle=false)` 正在等子代理收尾时，伴随的 OSC `done`/`idle`
    /// 不得抢跑清掉延后标记或提前报完成；一旦 `blocked` 或子代理收尾再按正常流程推进。
    #[test]
    fn agy_osc7501_done_does_not_preempt_deferred_stop() {
        let state = HookState::new();
        let tracker = SessionTracker::new();
        let seen = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let seen_cb = Arc::clone(&seen);
        let emitter = StatusEmitter::new(Arc::new(move |c: crate::monitor::StatusChange| {
            seen_cb.lock().push((c.status, c.cause));
        }));

        // 主会话开工 + 登记后台子代理 + 主会话 Stop(fullyIdle=false) 挂起延后完成
        state.update(11, "ai-working".to_string());
        state.agy_subagent_active(11, "sub-1");
        state.agy_defer_stop(11, Some("END_TURN".to_string()));
        emitter.emit_if_changed(11, "ai-working", Some("PreInvocation"), Some("agy".into()));

        // OSC 7501 伴随 state=done / state=idle 到达 -> 不得抢跑报 Stop
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            11,
            osc(OscProgramState::Done, None, Some("antigravity"), None, None),
        );
        observe_osc_status(
            &state,
            &emitter,
            &tracker,
            11,
            osc(OscProgramState::Idle, None, Some("antigravity"), None, None),
        );
        assert_eq!(state.get_status(11).as_deref(), Some("ai-working"));
        assert_eq!(
            seen.lock().clone(),
            vec![("ai-working".into(), Some("PreInvocation".into()))]
        );

        // 子代理收尾时仍能取出此前延后的完成原因
        assert_eq!(
            state.agy_subagent_settled(11, "sub-1"),
            Some(Some("END_TURN".to_string()))
        );
    }
}
