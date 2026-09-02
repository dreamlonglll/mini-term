//! 编排控制面的**线上形状 + 响应解析器**。
//!
//! 这一份被两边用：
//!
//! - `mt-agent-cli`（同工作区的 sidecar 二进制）：发请求、解析响应、出 JSON；
//! - 主仓 `mt-ai` 的对账测试（跨工作区 path 依赖，dev-dependency）：拿桌面侧
//!   **真 handler 的产出**喂进这里的解析器，证明两边对得上。
//!
//! 与 `mt_core::config_reader::ConfigSshView` 是同一种对账关系：桌面侧
//! （`mt_ai::control`）与 sidecar 侧隔着 crate 边界、各有一份类型，靠字段名对齐，
//! 由一条测试兜住。区别只在这一份是**给测试直接调**的，不必等真跑起来才发现漂移。
//!
//! 依赖表只有 serde 两件套 —— 主仓把它引进测试图，不该顺带背一棵大依赖树。

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 连接超时。控制端点是本机进程，慢到这个份上一定是出事了。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// 读响应的超时。
///
/// ⚠️ 必须**大于**桌面侧等主线程的那个时限（`mt_ai::control::ACTION_TIMEOUT`
/// 3 秒），否则起会话稍慢一点就变成 CLI 先断线 —— 编排者拿到的会是「够不着」，
/// 而不是桌面端给的那个明确答复。
///
/// 住在这个 crate 而不是 `mt-agent-cli` 的 bin 里，是因为那条不等式跨着**工作区
/// 边界**：主仓 `mt-ai` 的对账测试（`tests/orchestrator_wire.rs`）要拿两侧的真
/// 常量比一次，而它够不到 bin 里的私有常量。放在这里，那条断言才不是假保险。
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

// ─── 长轮询（`wait`，工单 06）──────────────────────────────────

/// `wait` 长轮询的服务端上界（与 `mt_ai::control::WAIT_MAX` 必须一致）。
///
/// CLI 这一侧要它只为一件事：**按它放大自己的读超时**。上面那个 5 秒是为
/// 「一趟请求答一趟」定的，而 `wait` 会在服务端睡到几分钟 —— 照 5 秒读的话
/// 长轮询每次都变成 CLI 先断线，编排者拿到的会是「够不着」而不是终态。
///
/// 两侧各有一份常量、隔着工作区边界，由主仓 `tests/orchestrator_wire.rs`
/// 拿真常量钉住（与 `READ_TIMEOUT` ↔ `ACTION_TIMEOUT` 那条同一种保险）。
pub const WAIT_MAX: Duration = Duration::from_secs(300);

/// 不给 `--timeout` 时服务端的默认耐心（与 `mt_ai::control::WAIT_DEFAULT` 一致）。
///
/// CLI 得知道它，否则默认用法下读超时会按 5 秒算 —— 而服务端要等 60 秒。
pub const WAIT_DEFAULT: Duration = Duration::from_secs(60);

/// `wait` 这一趟该把读超时设成多少。
///
/// = 服务端最多会占用的那段时间 + 一份常规富余（[`READ_TIMEOUT`]，留给 HTTP 往返
/// 与最后那一次取样）。**必须严格大于服务端那一侧**，否则长轮询的正常回执
/// （包括 `pending`）永远拿不到。
///
/// `requested` 先自己钳一次上界：服务端也会钳（那是权威的一道），
/// 两边钳出同一个数，CLI 的读超时才与它真的等到的时间对得上。
pub fn wait_read_timeout(requested: Duration) -> Duration {
    requested.min(WAIT_MAX) + READ_TIMEOUT
}

/// 编排令牌的环境变量名（与 `mt_ai::control::TOKEN_ENV` 必须一致）。
pub const TOKEN_ENV: &str = "MINITERM_ORCHESTRATOR_TOKEN";
/// 编排者自身 pane 身份的环境变量名（与 `mt_ai::control::PANE_ENV` 必须一致）。
pub const PANE_ENV: &str = "MINITERM_ORCHESTRATOR_PANE";
/// 控制端点的路由前缀（与 `mt_ai::control::CONTROL_PREFIX` 必须一致）。
pub const CONTROL_PREFIX: &str = "/control/";

// ─── 自身身份（fail-closed 的第一道闸）─────────────────────────

/// 编排者的身份：一枚令牌 + 自己是哪个 pane。两者都由主程序在 spawn 时注入。
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub token: String,
    pub pane_id: u32,
}

/// 拿不到身份的三种情形。**都不是「猜一个默认值继续」的理由** —— 这个二进制
/// 在没有编排能力的 pane 里被跑到是常态（用户手敲、乐手 pane 里的 agent 乱试），
/// 明确拒绝就是正确行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    /// 没有令牌 = 这个 pane 没被授予编排能力。
    NotAnOrchestrator,
    /// 有令牌却没有 pane 身份 / 身份不是数字：注入链路坏了，不许放行。
    BrokenIdentity,
}

impl IdentityError {
    pub fn message(self) -> &'static str {
        match self {
            Self::NotAnOrchestrator => {
                "this pane has no orchestrator capability (no MINITERM_ORCHESTRATOR_TOKEN); \
                 enable 「允许编排」 on the AI launcher and start the session from it"
            }
            Self::BrokenIdentity => {
                "orchestrator token present but MINITERM_ORCHESTRATOR_PANE is missing or not a number"
            }
        }
    }
}

/// 从环境里取身份。取值函数注入，便于测试（`identity_from_env` 是它的实参版）。
pub fn identity_from(get: impl Fn(&str) -> Option<String>) -> Result<Identity, IdentityError> {
    let token = get(TOKEN_ENV)
        .filter(|t| !t.trim().is_empty())
        .ok_or(IdentityError::NotAnOrchestrator)?;
    let pane_id = get(PANE_ENV)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .ok_or(IdentityError::BrokenIdentity)?;
    Ok(Identity { token, pane_id })
}

/// 进程环境版。
pub fn identity_from_env() -> Result<Identity, IdentityError> {
    identity_from(|k| std::env::var(k).ok())
}

/// 控制请求的 body（与桌面侧 `ControlRequest` 对齐）。
///
/// 命令各取所需，用不上的字段**整个不出线**（`skip_serializing_if`）——
/// `list-*` 那两条命令的请求体因此与工单 02 时一字不差。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRequest {
    pub token: String,
    pub pane_id: u32,
    /// `start-session`：用哪个具名启动器（**只有 id** —— 命令文本从不经过这里）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launcher_id: Option<String>,
    /// `start-session`：落在哪个项目；不给就是编排者自己那个。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// 以某个乐手为目标的命令（`send` / `wait`，工单 07 的 read 同款）用它。
    /// 以某个乐手为目标的命令（`send` / `read-transcript` / `read-screen`，
    /// 工单 06 的 wait 同款）用它。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<u32>,
    /// `wait`：最多等多久（毫秒）。不给就是服务端的 [`WAIT_DEFAULT`]；
    /// 超过 [`WAIT_MAX`] 由服务端钳回上界（钳而不拒）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// `send`：要写穿进去的正文（编排者写的 prompt）。
    ///
    /// **只在这条命令的请求体里出现一次，别的地方一个字都不留** —— 它是用户
    /// 项目里的内容，与启动器的命令文本同一档待遇：不进日志、不进错误消息、
    /// 不进回执（回执只回 pane 编号与「有没有当成一块粘贴」）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `read-transcript`：从第几条读起（= 上次回执里的 `nextCursor`）。
    /// 不给就是从头。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    /// `read-screen`：要画面尾部多少行。不给由桌面侧用默认值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
}

impl From<&Identity> for ControlRequest {
    /// ⚠️ 这里**刻意逐个字段写 `None`**，不用 `..Default::default()`：
    /// 加一个字段忘了在这里补一行就编译不过，那正是我们要的护栏。
    fn from(id: &Identity) -> Self {
        Self {
            token: id.token.clone(),
            pane_id: id.pane_id,
            launcher_id: None,
            project_id: None,
            target_pane_id: None,
            text: None,
            timeout_ms: None,
            cursor: None,
            lines: None,
        }
    }
}

impl ControlRequest {
    /// `start-session` 的请求体。
    pub fn start_session(id: &Identity, launcher_id: &str, project_id: Option<&str>) -> Self {
        Self {
            launcher_id: Some(launcher_id.to_string()),
            project_id: project_id.map(str::to_string),
            ..Self::from(id)
        }
    }

    /// `send` 的请求体：往哪个乐手写、写什么。
    pub fn send(id: &Identity, target_pane_id: u32, text: &str) -> Self {
        Self {
            target_pane_id: Some(target_pane_id),
            text: Some(text.to_string()),
            ..Self::from(id)
        }
    }

    /// `wait` 的请求体：等哪个乐手、最多等多久。
    ///
    /// `timeout` 不给就整个字段不出线 —— 服务端据此落在它自己的
    /// [`WAIT_DEFAULT`]，默认值因此只有一处（在桌面侧那个常量上）。
    pub fn wait(id: &Identity, target_pane_id: u32, timeout: Option<Duration>) -> Self {
        Self {
            target_pane_id: Some(target_pane_id),
            timeout_ms: timeout.map(|d| d.min(WAIT_MAX).as_millis() as u64),
            ..Self::from(id)
        }
    }

    /// `read-transcript` 的请求体：读哪个乐手、从第几条起。
    pub fn read_transcript(id: &Identity, target_pane_id: u32, cursor: Option<u64>) -> Self {
        Self {
            target_pane_id: Some(target_pane_id),
            cursor,
            ..Self::from(id)
        }
    }

    /// `read-screen` 的请求体：读哪个乐手的画面、要尾部多少行。
    pub fn read_screen(id: &Identity, target_pane_id: u32, lines: Option<u32>) -> Self {
        Self {
            target_pane_id: Some(target_pane_id),
            lines,
            ..Self::from(id)
        }
    }
}

// ─── 响应 ─────────────────────────────────────────────────────

/// 一条启动器。**只有 id 与展示名** —— 命令文本从不下发（ADR 0002/0003）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Launcher {
    pub id: String,
    pub name: String,
}

/// 一条可达项目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    /// 编排者自己所在的那条。
    #[serde(default)]
    pub current: bool,
    /// 能不能在这里起乐手（SSH 远程项目起不了）。
    ///
    /// **缺省 `false`**：认不出这个字段的旧桌面端在场时，宁可让编排者以为
    /// 都起不了、去试一次吃个明确错误，也别反过来（fail-closed 的取向）。
    #[serde(default)]
    pub can_start_sessions: bool,
}

/// 一个受编排会话（乐手）在编排者眼里的样子。
///
/// `start-session` 的回执与 `list-panes` 的每一条都是它 —— 两条命令共用一个类型，
/// 编排者只需要认识一种 pane 视图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratedPane {
    /// 乐手的 pane 身份（= PTY 编号）。后续命令按它点名。
    pub pane_id: u32,
    pub project_id: String,
    pub project_name: String,
    pub launcher_id: String,
    /// 启动器展示名。**不是命令文本** —— 那东西一个字都不出桌面端。
    pub launcher_name: String,
    /// AI 状态：`idle` / `ai-idle` / `ai-working`。
    pub status: String,
    /// pane 还在桌面上吗。编排者退场不杀乐手；反过来乐手被用户关掉时这里是
    /// `false`，记账仍在（好让编排者看得见「我起的那个已经没了」）。
    #[serde(default)]
    pub alive: bool,
}

/// `send` 的回执。
///
/// **不带任何正文回显**：那是用户项目里的内容，与启动器的命令文本同一档待遇。
/// 也不带「这次派活的结果」—— 写穿之后那一瞬什么都还没发生，要看结果走 `wait`
/// （工单 06）或等汇报（ADR 0004）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendReceipt {
    /// 写进了哪个受编排会话。
    pub pane_id: u32,
    /// 这次派活的任务编号（`t1` / `t2`…，工单 10）。
    ///
    /// 汇报会带着它回来，编排者据此把「桌面端送来的这条结果」与「我派出去的
    /// 那件活」对上。每个编排者各自从 `t1` 数起。
    ///
    /// **缺省空串**：认不出这个字段的旧桌面端在场时给的就是空串 —— 编排者拿它
    /// 去核对会一眼看出对不上，**别猜**（补一个编号出来只会让它把别人的汇报
    /// 认成自己的）。
    #[serde(default)]
    pub task_id: String,
    /// 是不是当成一整块粘贴送进去的（bracketed paste）。
    ///
    /// **缺省 `false`**：认不出这个字段的旧桌面端在场时，宁可让编排者以为
    /// 多行没走成整块、去核对一眼，也别反过来（与 `Project::can_start_sessions`
    /// 同一个 fail-closed 取向）。
    #[serde(default)]
    pub bracketed_paste: bool,
    /// 写入那一刻目标的 AI 状态原文（工单 10）。**是事实，不是结果**：
    ///
    /// - `ai-working` —— 对面正忙，这段 prompt 进了它自己的输入缓冲，要等它
    ///   手上这一轮结束（Claude / Codex 会排队，Grok 未验）；
    /// - `ai-idle` —— 对面闲着，应当立刻开跑；
    /// - `idle` —— 里头的 agent 已经退了，这段字进的是**裸 shell**
    ///   （配合 `bracketed_paste: false` 一起读）。
    ///
    /// **缺省空串**（旧桌面端在场时），同样别猜。
    #[serde(default)]
    pub target_status: String,
}

/// `wait` 的结论（工单 06）。
///
/// 四类终态里的三类走这个结构（第四类「pane 不存在」是错误码 `paneNotFound`），
/// 外加一个 `pending`：**到耐心用尽还没收敛不是错误**，是一条正常的观测结果，
/// 编排者据此决定继续等还是先去干别的。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitOutcome {
    /// 等的是哪个受编排会话。
    pub pane_id: u32,
    /// 结论，四选一：
    ///
    /// - `ai-idle` —— 干完了。看 `cause` 才知道是**怎么**完的（只有 `Stop` 是
    ///   真做完；`Interrupt` 是用户按了 Esc，`Stall` 是停摆兜底收敛的）。
    /// - `attention` —— 停在等审批或向人提问，`cause` 是原因原文。
    ///   **编排者不代答**：在自己的对话里请用户去那个会话处理。
    /// - `idle` —— 里头的 agent 已经退出，会话退回裸 shell。
    /// - `pending` —— 到时限还没收敛。看 `status`：`ai-working` 是真在跑，
    ///   `idle` 是「这个会话看不透」（没有 hook、也没被识别成已知 AI 命令）。
    ///
    /// **缺省空串**：认不出这个字段的旧桌面端在场时，宁可让编排者拿到一个显然
    /// 不合法的结论去查，也别默认成某一档终态（fail-closed 的取向，与
    /// `Project::can_start_sessions` 同源）。
    #[serde(default)]
    pub outcome: String,
    /// 收工那一刻的 AI 状态：`idle` / `ai-idle` / `ai-working`，与
    /// `list-panes` 的状态列同一口径。
    #[serde(default)]
    pub status: String,
    /// 成因原文（hook 事件名）。无 hook 的会话没有成因。
    #[serde(default)]
    pub cause: Option<String>,
    /// 实际等了多久（毫秒）。给的超时被钳到上界时看得出来。
    #[serde(default)]
    pub waited_ms: u64,
}

impl WaitOutcome {
    /// 停在等审批 / 向人提问了吗 —— 编排者该**停手播报**的那一档。
    pub fn needs_human(&self) -> bool {
        self.outcome == "attention"
    }

    /// 收敛成终态了吗（`false` = `pending`，还得接着等或去干别的）。
    ///
    /// 认的是那三个具体的名字而不是「不等于 pending」：认不出的 `outcome`
    /// （旧/新桌面端、字段缺失）一律**不算**收敛 —— 少判一次终态只是多等一轮，
    /// 误判成终态是把没做完的活报成交付。
    pub fn is_settled(&self) -> bool {
        matches!(self.outcome.as_str(), "ai-idle" | "attention" | "idle")
    }
}

/// `read-transcript` 的回执：一段结构化会话记录增量（工单 07）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transcript {
    pub pane_id: u32,
    /// 记录家族：`claude` / `codex` / `grok`。
    #[serde(default)]
    pub agent: String,
    /// 这一段属于哪条会话。
    ///
    /// **游标只在同一个 `session_id` 内有意义** —— 乐手 `/clear` 或退出重开
    /// 之后 seq 从 0 重新数。它一变就把游标归零重取。
    #[serde(default)]
    pub session_id: String,
    /// 本次实际从第几条起（请求的游标越界时会被钳回来）。
    #[serde(default)]
    pub cursor: u64,
    /// 下次该传的游标。没有新内容时它等于 `cursor`。
    #[serde(default)]
    pub next_cursor: u64,
    /// 这条会话此刻一共有多少条消息。
    #[serde(default)]
    pub total: u64,
    /// 还有没读完的（被回执的字节上界挡住）—— 按 `next_cursor` 接着取。
    ///
    /// **缺省 `false`**：认不出这个字段的旧桌面端在场时，宁可让编排者以为读完了
    /// 也别让它无限循环取下去（与 `SendReceipt::bracketed_paste` 同一个取向）。
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub messages: Vec<TranscriptMessage>,
}

/// transcript 里的一条消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptMessage {
    /// 这条会话里的序号（0 起、连续、只增）。
    pub seq: u64,
    /// `user` / `assistant`。
    pub role: String,
    pub content: String,
    /// 记录里的时间戳，各家格式不同，原样透传。
    #[serde(default)]
    pub timestamp: String,
    /// 这条的正文被字节上界截断了 —— 后半截**再也取不回来**（只在一条消息
    /// 自己就超过上界时发生；不给出去的话游标永远走不过它）。
    #[serde(default)]
    pub truncated: bool,
}

/// `read-screen` 的回执：终端画面尾部若干行纯文本（工单 07）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Screen {
    pub pane_id: u32,
    /// 画面尾部若干行，**旧的在前**。颜色与属性已剥掉，行尾空格已裁。
    #[serde(default)]
    pub lines: Vec<String>,
    /// 字节上界砍掉了开头几行。
    #[serde(default)]
    pub truncated: bool,
}

/// 被拒 / 出错的响应。`code` 是闭集（桌面侧 `ControlError::code`），
/// CLI 按它决定退出码，`message` 只给人看。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlFailure {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl ControlFailure {
    /// 「这个 pane 没有编排能力」这一类（鉴权失败），CLI 据此给专门的退出码。
    pub fn is_denied(&self) -> bool {
        matches!(self.code.as_str(), "missingToken" | "invalidToken")
    }

    /// 桌面端在，但没答上来（主线程忙死 / 动作泵没接线）。
    ///
    /// 与「连都连不上」归进同一档退出码：两种处境下编排者都**改自己的请求也没用**。
    ///
    /// ⚠️ 但 `desktopBusy` **不等于「没起成」**：桌面端一旦把乐手起出来就先落记账、
    /// 再谈回执（`mt_ai::control` 的记账契约），所以没答上来的只是这一趟往返。
    /// 正确的下一步是先 `list-panes` 看一眼那个会话在不在，**别无脑重试** ——
    /// 重试很可能是在起第二个。
    pub fn is_desktop_unavailable(&self) -> bool {
        self.code == "desktopBusy"
    }

    fn malformed(status: u16, why: &str) -> Self {
        Self {
            status,
            code: "malformedResponse".to_string(),
            message: format!("cannot parse control response: {why}"),
        }
    }
}

/// 拆信封：`{"ok":true,"data":{…}}` → data；`{"ok":false,"error":{…}}` → 失败。
///
/// 状态码只作为兜底信号：**以 body 为准**，body 认不出来才拿状态码报错 ——
/// 中间挡了个代理之类的意外情形下，错误至少还能被读懂。
fn envelope(status: u16, body: &str) -> Result<serde_json::Value, ControlFailure> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err(ControlFailure::malformed(status, "not JSON"));
    };
    if value.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return value
            .get("data")
            .cloned()
            .ok_or_else(|| ControlFailure::malformed(status, "ok response without data"));
    }
    let error = value.get("error");
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("unknownError");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("control request failed");
    Err(ControlFailure {
        status,
        code: code.to_string(),
        message: message.to_string(),
    })
}

/// 解析 `list-launchers` 的响应。
pub fn parse_launchers(status: u16, body: &str) -> Result<Vec<Launcher>, ControlFailure> {
    let data = envelope(status, body)?;
    let list = data
        .get("launchers")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `launchers`"))?;
    serde_json::from_value(list)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `launchers`: {e}")))
}

/// 解析 `list-projects` 的响应。
pub fn parse_projects(status: u16, body: &str) -> Result<Vec<Project>, ControlFailure> {
    let data = envelope(status, body)?;
    let list = data
        .get("projects")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `projects`"))?;
    serde_json::from_value(list)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `projects`: {e}")))
}

/// 解析 `start-session` 的回执（单个乐手）。
pub fn parse_started_pane(status: u16, body: &str) -> Result<OrchestratedPane, ControlFailure> {
    let data = envelope(status, body)?;
    let pane = data
        .get("pane")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `pane`"))?;
    serde_json::from_value(pane)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `pane`: {e}")))
}

/// 解析 `list-panes` 的响应（自己名下的全部乐手）。
pub fn parse_panes(status: u16, body: &str) -> Result<Vec<OrchestratedPane>, ControlFailure> {
    let data = envelope(status, body)?;
    let list = data
        .get("panes")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `panes`"))?;
    serde_json::from_value(list)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `panes`: {e}")))
}

/// 解析 `send` 的回执。
pub fn parse_send_receipt(status: u16, body: &str) -> Result<SendReceipt, ControlFailure> {
    let data = envelope(status, body)?;
    let sent = data
        .get("sent")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `sent`"))?;
    serde_json::from_value(sent)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `sent`: {e}")))
}

/// 解析 `wait` 的结论。
pub fn parse_wait_outcome(status: u16, body: &str) -> Result<WaitOutcome, ControlFailure> {
    let data = envelope(status, body)?;
    let waited = data
        .get("waited")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `waited`"))?;
    serde_json::from_value(waited)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `waited`: {e}")))
}

/// 解析 `read-transcript` 的回执。
pub fn parse_transcript(status: u16, body: &str) -> Result<Transcript, ControlFailure> {
    let data = envelope(status, body)?;
    let transcript = data
        .get("transcript")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `transcript`"))?;
    serde_json::from_value(transcript)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `transcript`: {e}")))
}

/// 解析 `read-screen` 的回执。
pub fn parse_screen(status: u16, body: &str) -> Result<Screen, ControlFailure> {
    let data = envelope(status, body)?;
    let screen = data
        .get("screen")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `screen`"))?;
    serde_json::from_value(screen)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `screen`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── 身份 ─────────────────────────────────────────────────

    #[test]
    fn 令牌与身份都在时解析出编排者身份() {
        let id = identity_from(|k| match k {
            TOKEN_ENV => Some("tok".into()),
            PANE_ENV => Some("7".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(id.token, "tok");
        assert_eq!(id.pane_id, 7);
    }

    /// 普通 pane 里跑（没有注入）→ 明确拒绝，不去猜端口也不发请求。
    #[test]
    fn 没有令牌就是普通_pane() {
        assert_eq!(
            identity_from(|_| None).unwrap_err(),
            IdentityError::NotAnOrchestrator
        );
        // 空串与空白同样不算
        assert_eq!(
            identity_from(|k| (k == TOKEN_ENV).then(|| "   ".to_string())).unwrap_err(),
            IdentityError::NotAnOrchestrator
        );
    }

    /// 有令牌没身份 / 身份不是数字:注入链路坏了,一样拒绝(不许默认成 0)。
    #[test]
    fn 身份缺失或非法一律拒绝() {
        for pane in [None, Some("".to_string()), Some("abc".to_string()), Some("-1".to_string())] {
            let err = identity_from(|k| match k {
                TOKEN_ENV => Some("tok".into()),
                PANE_ENV => pane.clone(),
                _ => None,
            })
            .unwrap_err();
            assert_eq!(err, IdentityError::BrokenIdentity, "pane={pane:?}");
        }
    }

    #[test]
    fn 请求体按_camel_case_出线() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let json = serde_json::to_string(&ControlRequest::from(&id)).unwrap();
        assert_eq!(json, r#"{"token":"t","paneId":3}"#);
    }

    // ─── 响应 ─────────────────────────────────────────────────

    #[test]
    fn 解析启动器名单() {
        let body = r#"{"ok":true,"data":{"launchers":[{"id":"claude","name":"Claude"}]}}"#;
        let list = parse_launchers(200, body).unwrap();
        assert_eq!(
            list,
            vec![Launcher {
                id: "claude".into(),
                name: "Claude".into()
            }]
        );
    }

    #[test]
    fn 解析可达项目() {
        let body = r#"{"ok":true,"data":{"projects":[
            {"id":"p1","name":"前端","path":"D:\\a","current":true},
            {"id":"p2","name":"后端","path":"D:\\b","current":false}]}}"#;
        let list = parse_projects(200, body).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].current);
        assert_eq!(list[1].path, "D:\\b");
    }

    /// 被拒时错误码要原样透出来 —— CLI 靠它区分「不是编排者」与「请求本身不对」。
    #[test]
    fn 被拒响应解析成闭集错误码() {
        let body = r#"{"ok":false,"error":{"code":"invalidToken","message":"nope"}}"#;
        let err = parse_launchers(401, body).unwrap_err();
        assert_eq!(err.code, "invalidToken");
        assert_eq!(err.status, 401);
        assert!(err.is_denied());

        let body = r#"{"ok":false,"error":{"code":"projectUnavailable","message":"gone"}}"#;
        let err = parse_projects(409, body).unwrap_err();
        assert_eq!(err.code, "projectUnavailable");
        assert!(!err.is_denied(), "项目没了不是鉴权失败");
    }

    /// 认不出的响应不许当成空名单成功返回。
    #[test]
    fn 认不出的响应是失败而不是空名单() {
        for (status, body) in [
            (200, "not json"),
            (200, r#"{"ok":true}"#),
            (200, r#"{"ok":true,"data":{}}"#),
            (500, "<html>proxy error</html>"),
        ] {
            let err = parse_launchers(status, body).unwrap_err();
            assert_eq!(err.code, "malformedResponse", "body={body}");
        }
    }

    /// 服务端加了字段（工单 03 会往里塞东西）不许让解析器崩：未知字段忽略。
    #[test]
    fn 未知字段向前兼容() {
        let body = r#"{"ok":true,"data":{"projects":[
            {"id":"p1","name":"n","path":"p","current":true,"futureField":42}],"extra":1}}"#;
        assert_eq!(parse_projects(200, body).unwrap().len(), 1);
    }

    // ─── 工单 03 的两条命令 ───────────────────────────────────

    /// `start-session` 的请求体：**只带 id**，没给的字段整个不出线。
    #[test]
    fn 起会话请求体只带_id() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let json = serde_json::to_string(&ControlRequest::start_session(&id, "codex", None)).unwrap();
        assert_eq!(json, r#"{"token":"t","paneId":3,"launcherId":"codex"}"#);

        let json =
            serde_json::to_string(&ControlRequest::start_session(&id, "codex", Some("p-api")))
                .unwrap();
        assert_eq!(
            json,
            r#"{"token":"t","paneId":3,"launcherId":"codex","projectId":"p-api"}"#
        );
        // 命令文本没有出线的通道 —— 类型上就没有那个字段
        assert!(!json.contains("command"));
    }

    /// `list-*` 的请求体不受工单 03 的字段扩展影响（工单 02 那版一字不差）。
    #[test]
    fn 列表命令的请求体未被新字段污染() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let json = serde_json::to_string(&ControlRequest::from(&id)).unwrap();
        assert_eq!(json, r#"{"token":"t","paneId":3}"#);
    }

    #[test]
    fn 解析起会话回执() {
        let body = r#"{"ok":true,"data":{"pane":{"paneId":101,"projectId":"p1",
            "projectName":"前端","launcherId":"codex","launcherName":"Codex",
            "status":"ai-idle","alive":true}}}"#;
        let pane = parse_started_pane(200, body).unwrap();
        assert_eq!(pane.pane_id, 101);
        assert_eq!(pane.launcher_name, "Codex");
        assert_eq!(pane.status, "ai-idle");
        assert!(pane.alive);
    }

    #[test]
    fn 解析乐手名单() {
        let body = r#"{"ok":true,"data":{"panes":[
            {"paneId":101,"projectId":"p1","projectName":"前端","launcherId":"codex",
             "launcherName":"Codex","status":"ai-working","alive":true},
            {"paneId":102,"projectId":"p1","projectName":"前端","launcherId":"claude",
             "launcherName":"Claude","status":"idle","alive":false}]}}"#;
        let panes = parse_panes(200, body).unwrap();
        assert_eq!(panes.len(), 2);
        assert!(panes[0].alive);
        assert!(!panes[1].alive, "关掉的乐手照列，只是 alive 为假");
    }

    /// 空名单是**合法**的成功响应（还没起过乐手），不许被当成解析失败。
    #[test]
    fn 空乐手名单是成功() {
        let body = r#"{"ok":true,"data":{"panes":[]}}"#;
        assert!(parse_panes(200, body).unwrap().is_empty());
    }

    /// 认不出的响应照旧算失败，两条新解析器一视同仁。
    #[test]
    fn 新命令的坏响应也算失败() {
        for (status, body) in [(200, "not json"), (200, r#"{"ok":true,"data":{}}"#)] {
            assert_eq!(
                parse_panes(status, body).unwrap_err().code,
                "malformedResponse"
            );
            assert_eq!(
                parse_started_pane(status, body).unwrap_err().code,
                "malformedResponse"
            );
        }
    }

    // ─── 工单 05：send ────────────────────────────────────────

    /// `send` 的请求体：只带目标编号与正文，别人的字段一个不出线。
    #[test]
    fn 写穿请求体只带目标与正文() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let json = serde_json::to_string(&ControlRequest::send(&id, 101, "干活")).unwrap();
        assert_eq!(
            json,
            r#"{"token":"t","paneId":3,"targetPaneId":101,"text":"干活"}"#
        );
        // 启动器那两个字段不该跟着出线
        assert!(!json.contains("launcherId"));
        assert!(!json.contains("projectId"));
    }

    /// 多行正文经 JSON 转义出线 —— 换行归一是**桌面侧**的事，
    /// CLI 这一侧原样送过去，不许在这儿先改一遍（改两遍就是两种口径）。
    #[test]
    fn 多行正文原样出线由桌面侧归一() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let req = ControlRequest::send(&id, 101, "第一行\n第二行");
        assert_eq!(req.text.as_deref(), Some("第一行\n第二行"));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""text":"第一行\n第二行""#), "{json}");
    }

    #[test]
    fn 解析写穿回执() {
        let body = r#"{"ok":true,"data":{"sent":{"paneId":101,"taskId":"t7","bracketedPaste":true,"targetStatus":"ai-working"}}}"#;
        let sent = parse_send_receipt(200, body).unwrap();
        assert_eq!(sent.pane_id, 101);
        assert_eq!(sent.task_id, "t7");
        assert!(sent.bracketed_paste);
        assert_eq!(sent.target_status, "ai-working");
    }

    /// 旧桌面端不认得工单 10 那两个字段时：`bracketedPaste` 缺省 `false`
    /// （宁可让编排者以为多行没走成整块、去核对一眼，也别反过来），
    /// `taskId` / `targetStatus` 缺省**空串** —— 编排者拿空串去核对会一眼看出
    /// 对不上，比补一个编号出来强（补出来它会把别人的汇报认成自己的）。
    #[test]
    fn 回执缺字段时保守当成没整块粘贴() {
        let body = r#"{"ok":true,"data":{"sent":{"paneId":101}}}"#;
        let sent = parse_send_receipt(200, body).unwrap();
        assert!(!sent.bracketed_paste);
        assert!(sent.task_id.is_empty(), "别猜任务编号");
        assert!(sent.target_status.is_empty(), "别猜目标状态");
    }

    /// 认不出的响应照旧算失败，不许退化成「反正发出去了」。
    #[test]
    fn 写穿的坏响应也算失败() {
        for (status, body) in [(200, "not json"), (200, r#"{"ok":true,"data":{}}"#)] {
            assert_eq!(
                parse_send_receipt(status, body).unwrap_err().code,
                "malformedResponse",
                "body={body}"
            );
        }
    }

    /// 工单 05 的两个错误码落在「改你的请求」那一档（既不是没能力，也不是够不着）。
    #[test]
    fn 写穿错误码的分档() {
        let f = |code: &str| ControlFailure {
            status: 400,
            code: code.into(),
            message: String::new(),
        };
        // `targetAwaitingHuman`（工单 10 的黄灯拦截）同档:它是一条**裁决**,
        // 编排者该做的是转告用户、等那个会话被处理完再重发 —— 不是重试,
        // 也不是「桌面端出问题了」。
        for code in ["emptyInput", "sendFailed", "targetAwaitingHuman"] {
            assert!(!f(code).is_denied(), "{code} 不是鉴权失败");
            assert!(!f(code).is_desktop_unavailable(), "{code} 不是够不着");
        }
    }

    /// 工单 03 新增的错误码要落在正确的退出码档位上。
    #[test]
    fn 新错误码的分档() {
        let f = |code: &str| ControlFailure {
            status: 400,
            code: code.into(),
            message: String::new(),
        };
        // 桌面端没答上来 = 「够不着」那一档（过会儿再试，别改请求）
        assert!(f("desktopBusy").is_desktop_unavailable());
        assert!(!f("desktopBusy").is_denied());
        // 其余都是「请求被拒」：编排者该改自己的请求或等名额
        for code in [
            "launcherNotFound",
            "projectUnreachable",
            "remoteProjectUnsupported",
            "sessionLimitReached",
            "startFailed",
            "selfTarget",
            "paneNotFound",
            "paneGone",
        ] {
            assert!(!f(code).is_denied(), "{code} 不是鉴权失败");
            assert!(!f(code).is_desktop_unavailable(), "{code} 不是够不着");
        }
    }

    // ─── 工单 06：wait ────────────────────────────────────────

    /// `wait` 的请求体：只带目标编号与耐心，别人的字段一个不出线；
    /// 不给超时就整个字段不出线（默认值只住在桌面侧那个常量上）。
    #[test]
    fn 等待请求体只带目标与耐心() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let json = serde_json::to_string(&ControlRequest::wait(&id, 101, None)).unwrap();
        assert_eq!(json, r#"{"token":"t","paneId":3,"targetPaneId":101}"#);

        let json = serde_json::to_string(&ControlRequest::wait(
            &id,
            101,
            Some(Duration::from_secs(30)),
        ))
        .unwrap();
        assert_eq!(
            json,
            r#"{"token":"t","paneId":3,"targetPaneId":101,"timeoutMs":30000}"#
        );
        // 正文那个字段一个字都不该跟着 wait 出线
        assert!(!json.contains("text"));
    }

    /// 超上界的耐心两侧钳成同一个数：CLI 得按它算读超时，算错就会先断线。
    #[test]
    fn 超上界的耐心被钳回上界() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let req = ControlRequest::wait(&id, 101, Some(Duration::from_secs(9_999)));
        assert_eq!(req.timeout_ms, Some(WAIT_MAX.as_millis() as u64));
    }

    /// `wait` 的读超时必须**大于**服务端可能占用的那段时间，否则长轮询的正常
    /// 回执永远拿不到 —— 编排者看到的会是「够不着」而不是终态。
    #[test]
    fn 等待的读超时留出富余() {
        assert!(wait_read_timeout(WAIT_MAX) > WAIT_MAX);
        assert!(wait_read_timeout(WAIT_DEFAULT) > WAIT_DEFAULT);
        // 报多大都按上界算（服务端也只等到上界）
        assert_eq!(
            wait_read_timeout(Duration::from_secs(9_999)),
            wait_read_timeout(WAIT_MAX)
        );
        // 短耐心不许因此把常规读超时缩掉
        assert!(wait_read_timeout(Duration::ZERO) >= READ_TIMEOUT);
        assert!(WAIT_DEFAULT < WAIT_MAX);
    }

    #[test]
    fn 解析等待结论() {
        let body = r#"{"ok":true,"data":{"waited":{"paneId":101,"outcome":"ai-idle",
            "status":"ai-idle","cause":"Stop","waitedMs":4210}}}"#;
        let w = parse_wait_outcome(200, body).unwrap();
        assert_eq!(w.pane_id, 101);
        assert_eq!(w.outcome, "ai-idle");
        assert_eq!(w.cause.as_deref(), Some("Stop"));
        assert_eq!(w.waited_ms, 4210);
        assert!(w.is_settled());
        assert!(!w.needs_human());
    }

    /// attention 那一档：原因原文要到得了编排者手上，且 `needs_human` 认得出。
    /// Codex 的审批等待状态是 `ai-working` —— 判据是成因不是状态。
    #[test]
    fn 解析等待结论的_attention_档() {
        let body = r#"{"ok":true,"data":{"waited":{"paneId":101,"outcome":"attention",
            "status":"ai-working","cause":"PermissionRequest","waitedMs":900}}}"#;
        let w = parse_wait_outcome(200, body).unwrap();
        assert!(w.needs_human(), "编排者据此停手播报，不代答");
        assert!(w.is_settled());
        assert_eq!(w.cause.as_deref(), Some("PermissionRequest"));
        assert_eq!(w.status, "ai-working", "Codex 的审批等待停在工作中");
    }

    /// **超时是成功响应**：`pending` 不是错误，只是还没收敛。
    /// 没有成因时 `cause` 整个字段不出线，解析成 `None`。
    #[test]
    fn 超时的等待结论也是成功响应() {
        let body = r#"{"ok":true,"data":{"waited":{"paneId":101,"outcome":"pending",
            "status":"ai-working","waitedMs":60000}}}"#;
        let w = parse_wait_outcome(200, body).unwrap();
        assert_eq!(w.outcome, "pending");
        assert!(!w.is_settled(), "pending 不算收敛");
        assert!(!w.needs_human());
        assert_eq!(w.cause, None, "无 hook 的会话没有成因");
    }

    /// 认不出的 `outcome`（桌面端比 CLI 新 / 字段缺失）**不算收敛** ——
    /// 少判一次只是多等一轮，误判成终态是把没做完的活报成交付。
    #[test]
    fn 认不出的结论不算收敛() {
        let body = r#"{"ok":true,"data":{"waited":{"paneId":101,"status":"idle"}}}"#;
        let w = parse_wait_outcome(200, body).unwrap();
        assert_eq!(w.outcome, "");
        assert!(!w.is_settled());
        assert!(!w.needs_human());

        let body = r#"{"ok":true,"data":{"waited":{"paneId":101,"outcome":"somethingNew",
            "status":"ai-idle"}}}"#;
        assert!(!parse_wait_outcome(200, body).unwrap().is_settled());
    }

    /// 认不出的响应照旧算失败，不许退化成「反正没等到」。
    #[test]
    fn 等待的坏响应也算失败() {
        for (status, body) in [(200, "not json"), (200, r#"{"ok":true,"data":{}}"#)] {
            assert_eq!(
                parse_wait_outcome(status, body).unwrap_err().code,
                "malformedResponse",
                "body={body}"
            );
        }
    }

    /// `wait` 复用既有错误码，**没有自己的新码**：四类终态里那一类
    /// 「pane 不存在」就是 `paneNotFound`（与 `send` 同一条可见范围铁律）。
    #[test]
    fn 等待复用既有错误码() {
        for (status, code) in [
            (404, "paneNotFound"),
            (403, "selfTarget"),
            (410, "paneGone"),
        ] {
            let body = format!(
                r#"{{"ok":false,"error":{{"code":"{code}","message":"m"}}}}"#
            );
            let err = parse_wait_outcome(status, &body).unwrap_err();
            assert_eq!(err.code, code);
            assert!(!err.is_denied(), "{code} 不是鉴权失败");
            assert!(!err.is_desktop_unavailable(), "{code} 不是够不着");
        }
    }

    /// 远程项目的 `canStartSessions` 要透到 CLI 侧（编排者据此不去白试一次）。
    #[test]
    fn 可达项目带上能否起会话() {
        let body = r#"{"ok":true,"data":{"projects":[
            {"id":"p1","name":"本地","path":"D:\\a","current":true,"canStartSessions":true},
            {"id":"p2","name":"远程","path":"/srv/api","current":false,"canStartSessions":false}]}}"#;
        let list = parse_projects(200, body).unwrap();
        assert!(list[0].can_start_sessions);
        assert!(!list[1].can_start_sessions, "远程项目起不了乐手");
    }

    // ─── 工单 07：read-transcript / read-screen ────────────────

    fn id7() -> Identity {
        Identity {
            token: "t".into(),
            pane_id: 3,
        }
    }

    /// 读命令的请求体只带自己那几个字段，别人的一个不出线。
    #[test]
    fn 读请求体各取所需() {
        // 不带游标 = 从头读
        let json = serde_json::to_string(&ControlRequest::read_transcript(&id7(), 101, None)).unwrap();
        assert_eq!(json, r#"{"token":"t","paneId":3,"targetPaneId":101}"#);

        let json =
            serde_json::to_string(&ControlRequest::read_transcript(&id7(), 101, Some(12))).unwrap();
        assert_eq!(
            json,
            r#"{"token":"t","paneId":3,"targetPaneId":101,"cursor":12}"#
        );

        // 不给行数 = 让桌面侧用默认值（CLI 这一侧不复制那个默认值，
        // 复制了就成两处口径）
        let json = serde_json::to_string(&ControlRequest::read_screen(&id7(), 101, None)).unwrap();
        assert_eq!(json, r#"{"token":"t","paneId":3,"targetPaneId":101}"#);

        let json =
            serde_json::to_string(&ControlRequest::read_screen(&id7(), 101, Some(20))).unwrap();
        assert_eq!(
            json,
            r#"{"token":"t","paneId":3,"targetPaneId":101,"lines":20}"#
        );

        // 读命令一个字的正文都不带（`text` 是 send 专属）
        for req in [
            ControlRequest::read_transcript(&id7(), 101, Some(1)),
            ControlRequest::read_screen(&id7(), 101, Some(1)),
        ] {
            let json = serde_json::to_string(&req).unwrap();
            assert!(!json.contains("text"), "{json}");
            assert!(!json.contains("launcherId"), "{json}");
        }
    }

    #[test]
    fn 解析_transcript_回执() {
        let body = r#"{"ok":true,"data":{"transcript":{
            "paneId":101,"agent":"claude","sessionId":"s-1","cursor":2,"nextCursor":4,
            "total":9,"hasMore":true,"messages":[
              {"seq":2,"role":"user","content":"修一下","timestamp":"t1","truncated":false},
              {"seq":3,"role":"assistant","content":"修好了","timestamp":"t2","truncated":false}]}}}"#;
        let t = parse_transcript(200, body).unwrap();
        assert_eq!(t.pane_id, 101);
        assert_eq!(t.agent, "claude");
        assert_eq!(t.session_id, "s-1");
        assert_eq!((t.cursor, t.next_cursor, t.total), (2, 4, 9));
        assert!(t.has_more);
        assert_eq!(t.messages.len(), 2);
        assert_eq!(t.messages[0].seq, 2);
        assert_eq!(t.messages[1].content, "修好了");
        assert!(!t.messages[1].truncated);
    }

    /// 空一段（没有新内容）是合法成功，不是失败。
    #[test]
    fn 空的_transcript_增量也是合法成功() {
        let body = r#"{"ok":true,"data":{"transcript":{
            "paneId":101,"agent":"codex","sessionId":"s","cursor":4,"nextCursor":4,
            "total":4,"hasMore":false,"messages":[]}}}"#;
        let t = parse_transcript(200, body).unwrap();
        assert!(t.messages.is_empty());
        assert_eq!(t.cursor, t.next_cursor, "游标原地不动");
        assert!(!t.has_more);
    }

    /// 旧桌面端缺字段时保守缺省：`hasMore` 当 false（宁可以为读完了，
    /// 也别让编排者照着一个不存在的「还有更多」无限循环取）。
    #[test]
    fn transcript_缺字段时保守缺省() {
        let body = r#"{"ok":true,"data":{"transcript":{"paneId":101}}}"#;
        let t = parse_transcript(200, body).unwrap();
        assert!(!t.has_more);
        assert!(t.messages.is_empty());
        assert_eq!(t.total, 0);
        assert!(t.session_id.is_empty());
    }

    #[test]
    fn 解析_screen_回执() {
        let body = r#"{"ok":true,"data":{"screen":{
            "paneId":101,"lines":["$ codex","> approve? (y/n)"],"truncated":false}}}"#;
        let s = parse_screen(200, body).unwrap();
        assert_eq!(s.pane_id, 101);
        assert_eq!(s.lines, vec!["$ codex", "> approve? (y/n)"]);
        assert!(!s.truncated);

        // 空屏也是合法答案（那个乐手此刻什么都没显示）
        let body = r#"{"ok":true,"data":{"screen":{"paneId":101}}}"#;
        let s = parse_screen(200, body).unwrap();
        assert!(s.lines.is_empty());
        assert!(!s.truncated);
    }

    /// 认不出的响应照旧算失败，不许退化成「反正读到了」。
    #[test]
    fn 读命令的坏响应也算失败() {
        for (status, body) in [(200, "not json"), (200, r#"{"ok":true,"data":{}}"#)] {
            assert_eq!(
                parse_transcript(status, body).unwrap_err().code,
                "malformedResponse",
                "transcript body={body}"
            );
            assert_eq!(
                parse_screen(status, body).unwrap_err().code,
                "malformedResponse",
                "screen body={body}"
            );
        }
    }

    /// 工单 07 的两个错误码落在「改你的请求」那一档 —— **既不是**没能力，
    /// **也不是**够不着（够不着那一档的含义是「重试可能就好了」，而
    /// `transcriptUnsupported` 永远不会好转，得换 `read-screen`）。
    #[test]
    fn 读命令错误码的分档() {
        let f = |code: &str| ControlFailure {
            status: 409,
            code: code.into(),
            message: String::new(),
        };
        for code in ["transcriptUnsupported", "sessionUnidentified"] {
            assert!(!f(code).is_denied(), "{code} 不是鉴权失败");
            assert!(!f(code).is_desktop_unavailable(), "{code} 不是够不着");
        }
    }
}
