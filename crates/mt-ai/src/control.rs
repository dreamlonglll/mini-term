//! 编排控制面：本地 HTTP 上给「编排者」用的一组控制端点。
//!
//! 骨架与鉴权设计见 `docs/adr/0003-ai-session-orchestration.md`。
//!
//! # 它长在哪
//!
//! 与 hook 上报**共用同一个本地 HTTP 服务**（[`crate::hook_server`] 那个 tiny_http
//! 监听），只是换一组路由前缀 `/control/`。共用的理由有两条：端口发现那套
//! （`hook-server.json` + `MINITERM_HOOK_PORT`）sidecar 侧现成，不必再造一份；
//! 主程序也不必为编排再开一个监听端口。
//!
//! **`/hook` 那条路由一个字都不能动**（三家 CLI 已注册在用户机器上的 hook 命令
//! 按当前形态 POST 过来），控制端点只是在它旁边加分支。
//!
//! # 鉴权：fail-closed
//!
//! `/hook` 无鉴权（同机任意进程都能 POST），控制端点不行 —— 它能列出用户的项目、
//! 后续（工单 03）还能起进程。于是：
//!
//! - 令牌**随机生成、每 pane 一枚**，由主程序在 spawn 勾了「允许编排」的启动器
//!   那一刻登记进 [`ControlPlane`]，经 `MINITERM_ORCHESTRATOR_TOKEN` 注入子进程；
//! - 请求必须同时带令牌与**自身 pane 身份**（`MINITERM_ORCHESTRATOR_PANE`），
//!   两者对不上即拒 —— 身份随环境钉死，工单 03 的自指禁令不必靠猜；
//! - 无令牌 / 认不出的令牌 / 身份对不上 → 401，不做任何降级放行。
//!
//! # 桌面能力经注入 trait 提供
//!
//! 本 crate 不依赖 `mt-config` / `gpui`，分成**查**与**做**两条注入缝：
//!
//! - [`OrchestratorHost`] —— 项目表与启动器名单（与 `mt_relay::host::RelayHost`
//!   同一个模式，[`NoopOrchestratorHost`] 是给测试与「尚未接线」用的空实现）；
//! - [`OrchestratorActions`] —— 起乐手、查乐手死活（照 `RelayEvents` 那条「出向
//!   动作」的路数，但**回执是同步的**：CLI 在等一个答复，见下）。
//!
//! **每次请求现查**：分组关系改了要即时生效，所以 handler 每次都问一遍宿主，
//! 不在授予令牌那一刻把可达项目算死。
//!
//! # 范围记账：谁 spawn 了谁（工单 03）
//!
//! 「可见与可驱动范围仅限编排者自己启动的会话」（ADR 0003）这条铁律的事实底座
//! 是记账表 `TokenRegistry::sessions` —— 不在表里的 pane，对编排者而言就是不
//! 存在。它与令牌**同锁**：编排者 pane 一撤销令牌，名下记账一并**降级**（乐手
//! 照常活着 —— 不陪葬；编排者 pane 重开是新身份，MVP 不做收养）。
//!
//! 名额与死活一律**现查**（[`OrchestratorActions::pane_liveness`]），不靠事件
//! 驱动记账：漏收一次「乐手退出」的事件不该永久占住一个名额。
//!
//! # 「编排者已离场」：出身留着，可达性收走（工单 04）
//!
//! 记账同时是**桌面 tab 上那枚出身标识**的事实来源（[`ControlPlane::origins`]）。
//! 于是编排者退场时不能把名下记账整体删掉 —— 删了「编排者已离场」就无从显示。
//! 改成留下记账、置一个 [`OrchestratedSession::orchestrator_departed`] 位：
//!
//! - **可达性立刻收走**：`belongs_to` 一律答否，`list-panes` / `resolve_target` /
//!   名额三处同时失效。原编排者的令牌已经撤了，本来就没人认证得成它；那个位是
//!   为「同一编号上又拿到一次授予」准备的 —— 前世的乐手不许被今生认领。
//! - **展示保留**：`origins` 照出这一条，桌面把标识降级成「编排者已离场」。
//! - **回收有主**：已离场记账只可能对着**还活着的**乐手 pane（已关的那些在退场
//!   那一刻就回收了），等乐手自己关掉时那次 `revoke_pane` 一并清掉。
//!   于是它不是一条只涨不消的表。
//!
//! # 记账契约：乐手一落地就记账，**再谈回执送不送得到**
//!
//! 起乐手是一次跨线程往返（HTTP 线程 → 桌面主线程 → 回来），发起侧兜着
//! [`ACTION_TIMEOUT`]。要是「先答复、答复到了才记账」，超时那一瞬就会长出一个
//! **幽灵乐手**：桌面上真有一个受编排会话在跑，却不进 `list-panes`、不占名额、
//! 被 [`ControlPlane::resolve_target`] 答成「不存在」—— 一次违反 ADR 0003 三条。
//!
//! 于是契约反过来：[`StartSessionSpec`] 本身就是一张**落地登记凭据**，动作实现
//! 只能靠 [`StartSessionSpec::landed`] 造出 [`StartedSession`]（那个类型没有别的
//! 构造路径），而 `landed` 做的第一件事就是把记账写进 `registry.sessions`。
//! 「拿到回执 = 记账已落地」因此是**类型层面**成立的，不靠调用方自觉。
//!
//! # 为什么有些命令要另起线程
//!
//! `start-session` 得回桌面主线程去建 pane，而 hook 那个 HTTP 服务是**单线程
//! 循环**（`hook_server.rs` 的 `for request in server.incoming_requests()`）——
//! 就地阻塞等回执会把排在后面的 hook 上报一起卡住，而 AI 状态感知是本仓的权威
//! 通道，不能为编排让路。于是 [`try_handle_control`] 对这类命令：先在 HTTP 线程
//! 上把鉴权做掉（挡住「任意进程都能让我们起线程」），再把已鉴权的活丢给一条
//! 独立线程跑完并响应。判据是 [`Command::needs_own_thread`]。
//!
//! # 长轮询：`wait` 一次也不打扰主线程（工单 06）
//!
//! [`ControlPlane::wait`] 要等的是**一个 AI 回合**（几分钟是常态），而上面那条
//! 泵的时限 [`ACTION_TIMEOUT`] 是 3 秒、为「建一个 pane」定的 —— 两者差两个
//! 数量级，硬套过去只会得到一串 `desktopBusy`。于是 `wait` 走另一条路：就在
//! 那条一次性线程上反复问 [`OrchestratorActions::pane_liveness`]（那个方法的
//! 契约本来就是「很快、不跳主线程」，读的都是后台线程够得到的只读状态），
//! gpui 主线程一次都不惊动。
//!
//! 它照样进 `needs_own_thread` 那张表 —— 那张表说的是「别在 HTTP 那条循环里
//! 就地做」，而 `wait` 要占的正是几分钟。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 控制端点的路由前缀。
pub const CONTROL_PREFIX: &str = "/control/";

/// [`OrchestratorActions::start_session`] 必须在多久之内答复。
///
/// 住在**这里**而不是桌面侧，是因为它是一条三方约束：本模块（等回执）、桌面端
/// （兜超时）、以及 sidecar CLI 的读超时（`mt_agent_control::READ_TIMEOUT`）。
/// 定 3 秒的理由两头夹出来：下界是「建一个 pane 正常要多久」——本机 spawn 一根
/// PTY 通常几十毫秒，3 秒是它的一个数量级以上；上界是 CLI 的读超时必须留出富余，
/// 否则起会话稍慢一点就变成 CLI 先断线，编排者拿到的会是「够不着」而不是这边
/// 给的明确答复。**跨工作区的那条不等式由 `tests/orchestrator_wire.rs` 拿两侧
/// 真常量钉住**。
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(3);

/// [`ControlPlane::wait`] 长轮询的**服务端上界**（工单 06）。
///
/// 编排者给的超时只是个愿望：一条 `wait` 请求占着一条一次性线程，让它按调用方
/// 报的数字无限期挂下去，就是把「hook 服务旁边偶尔多起一条线程」变成一条慢性
/// 泄漏。5 分钟这个数两头夹出来：
///
/// - 下界是**一个 AI 回合正常要多久** —— Claude / Codex 跑一轮改代码几分钟是
///   常态，上界短于它就等于逼编排者不停重投，每次重投都是一次进程启动；
/// - 上界是**出了岔子最多白占一条线程多久** —— 5 分钟之后总要给它一个回执，
///   让它自己决定继续等还是先去干别的。
///
/// 超过上界的请求**不报错**，按上界算（`wait` 的超时是正常回执，不是错误码）。
/// CLI 侧那份同名常量由 `tests/orchestrator_wire.rs` 拿两侧真常量钉住 ——
/// 它得按这个数放大自己的读超时，否则长轮询会变成 CLI 先断线。
pub const WAIT_MAX: Duration = Duration::from_secs(300);

/// 编排者没说等多久时的默认耐心。
///
/// 60 秒是照着**编排者自己那一侧的工具调用超时**定的：`wait` 是一次同步阻塞的
/// CLI 调用，而跑它的那个 agent 通常给一次 shell 调用两分钟。默认值必须稳稳落
/// 在那之内，否则「不给 `--timeout` 直接用」这条默认路径就是「命令被自己的宿主
/// kill 掉」。要等更久的编排者显式给 `--timeout`，并自己把宿主那侧的超时一起放大。
pub const WAIT_DEFAULT: Duration = Duration::from_secs(60);

/// 长轮询的节拍。
///
/// 比 monitor 那条 500ms 轮询快一档，但**不是**为了抢在它前面：hook 事件是在
/// HTTP 线程上**同步**落进 `last_hook_status` 与去重表的（见 [`crate::hook_server`]），
/// 状态变化本来就不用等轮询那一拍。250ms 只是把「回合边界 → 编排者拿到回执」
/// 的延迟压到半秒以内，代价是每秒四次几把互斥锁的读 —— 对这条低频命令可忽略。
///
/// **不做成可注入**：主缝测试靠请求里那个 `timeoutMs` 把整轮压到几百毫秒，
/// 用不着为它另开一个只有测试会拨的旋钮。
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// 编排令牌的环境变量名（应用内部协议，`MINITERM_` 保留前缀保证用户/项目级
/// 环境变量覆盖不掉）。
pub const TOKEN_ENV: &str = "MINITERM_ORCHESTRATOR_TOKEN";
/// 编排者自身 pane 身份的环境变量名。
pub const PANE_ENV: &str = "MINITERM_ORCHESTRATOR_PANE";

/// 单个控制请求 body 的字节上限。
///
/// 除 `send` 之外的命令 body 只有一百来字节；`send` 那条会带上编排者写的整段
/// prompt，是唯一一条可能顶到上限的命令。**64 KiB 判为够用**：那已经是一万多个
/// 英文词的一段指令，远超「派一次活」的合理体量，而编排者要塞的大块上下文
/// 本来就该以文件路径的形式交过去（乐手自己能读文件），不该逐字灌进 PTY。
/// 顶到上限时得到的是明确的 `payloadTooLarge`，不是截断。
const MAX_CONTROL_BODY_BYTES: usize = 64 * 1024;

// ─── 注入接口 ─────────────────────────────────────────────────

/// 启动器在控制面里的切面。
///
/// **只有 `id` + `name`**：ADR 0002 的边界在编排这条链路上照旧 —— 编排者只能按
/// id 引用具名启动器，命令文本不给它看，更不给它自拟。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlLauncher {
    pub id: String,
    pub name: String,
}

/// 项目在控制面里的切面。
///
/// `group_id` 是**已经解析好的所属分组 id**（宿主负责走项目树算出来，
/// 未分组为 `None`）。放在宿主那侧是因为分组树的形状属于配置层，
/// 而这里只需要一个可比较的归属标签。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlProject {
    pub id: String,
    pub name: String,
    pub path: String,
    pub group_id: Option<String>,
    /// 引用的 SSH 连接 id；本地项目为 `None`。
    ///
    /// **照实投影**（与 `mt_relay::host::RelayProject` 同一个字段、同一份配置
    /// 事实），不在宿主那侧先折成一个 `remote: bool` —— 两处投影同一件事，
    /// 形状一致才好对账，而「远程项目算不算可用宿主」是本模块的裁决，
    /// 不该提前在配置层判掉。
    ///
    /// 裁决本身在 [`Self::is_remote`]：SSH 远程项目**不能当乐手宿主** ——
    /// 编排令牌只会注进本地 ssh 客户端进程，远端 agent 根本拿不到（工单 02 的
    /// 评审结论，`store::panes::start_pty` 那侧也照此不发令牌）。共享入口
    /// `AppStore::launch_ai_session` 刻意**不判**这一条（那是发起侧策略），
    /// 所以它必须落在这里。
    pub ssh_connection_id: Option<String>,
}

impl ControlProject {
    /// 是 SSH 远程项目吗（= 不能当乐手宿主）。
    pub fn is_remote(&self) -> bool {
        self.ssh_connection_id.is_some()
    }
}

/// 控制面向桌面端要东西的入口（`RelayHost` 的同款注入 trait）。
///
/// **两个方法都在 HTTP 线程上被调用**，实现方自己负责跨线程取值
/// （`mt-app` 那边是主线程刷新、HTTP 线程只读的一份镜像）。
pub trait OrchestratorHost: Send + Sync + 'static {
    /// 当前配置里的 AI 启动器名单（全量：任何启动器都能当乐手，见 ADR 0003）。
    fn launchers(&self) -> Vec<ControlLauncher>;

    /// 当前项目表（含分组归属）。**每次请求现查** —— 改分组要即时生效。
    fn projects(&self) -> Vec<ControlProject>;
}

/// 什么都不做的宿主实现。
///
/// 只给测试和「尚未接线」的占位场景用：名单恒空，编排者会看到「项目不可用」。
/// 生产路径必须注入真正的实现。
pub struct NoopOrchestratorHost;

impl OrchestratorHost for NoopOrchestratorHost {
    fn launchers(&self) -> Vec<ControlLauncher> {
        Vec::new()
    }
    fn projects(&self) -> Vec<ControlProject> {
        Vec::new()
    }
}

// ─── 动作注入（起乐手 / 查死活）─────────────────────────────────

/// 一次「起乐手」的落地请求，**同时是一张落地登记凭据**。
///
/// **没有命令文本，也没有「要不要授予编排能力」这个位** —— 类型上就没有第二种
/// 可能：
///
/// - 命令由桌面侧按 [`Self::launcher_id`] 从配置里取（ADR 0002 的唯一防线是
///   「命令只能来自桌面端配置」，编排者连看都看不到）；
/// - 受编排会话**一律不授予**编排能力（ADR 0003 的禁套娃），哪怕目标启动器自己
///   勾了「允许编排」。
///
/// 字段全私有、只读不改：这几样东西同时是**记账那一份事实**（见
/// [`Self::landed`]），拆成「请求一份、记账一份」就是两份走散的机会。
pub struct StartSessionSpec {
    /// 记账草稿：除 pane 身份之外都已经知道了（控制面查宿主时顺手拿到的）。
    draft: SessionDraft,
    /// 登记的去处。带在请求里而不是让动作实现自己去找控制面 —— 记账是**控制面
    /// 的**事实，动作实现只负责说一句「它落地了，编号是这个」。
    plane: ControlPlane,
}

/// 记账草稿：一条 [`OrchestratedSession`] 差 pane 身份的那一部分。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionDraft {
    orchestrator_pane_id: u32,
    project_id: String,
    project_name: String,
    launcher_id: String,
    launcher_name: String,
}

impl SessionDraft {
    fn landed(self, pane_id: u32, orchestrator_label: &str) -> OrchestratedSession {
        OrchestratedSession {
            pane_id,
            orchestrator_pane_id: self.orchestrator_pane_id,
            orchestrator_label: orchestrator_label.to_string(),
            // 刚落地的乐手，编排者当然还在（它正等着这一趟回执）。
            orchestrator_departed: false,
            project_id: self.project_id,
            project_name: self.project_name,
            launcher_id: self.launcher_id,
            launcher_name: self.launcher_name,
        }
    }
}

impl std::fmt::Debug for StartSessionSpec {
    /// 不打 [`ControlPlane`]（里头是几张表和一把锁，打出来只有噪音）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartSessionSpec")
            .field("draft", &self.draft)
            .finish_non_exhaustive()
    }
}

impl StartSessionSpec {
    /// 具名启动器的 id（控制面已确认它在名单里）。
    pub fn launcher_id(&self) -> &str {
        &self.draft.launcher_id
    }

    /// 落地项目（控制面已确认它可达、且不是 SSH 远程项目）。
    pub fn project_id(&self) -> &str {
        &self.draft.project_id
    }

    /// 谁起的。桌面侧的诞生提示要说明出身（ADR 0002 的一次性提示）。
    pub fn orchestrator_pane_id(&self) -> u32 {
        self.draft.orchestrator_pane_id
    }

    /// **乐手落地了**：先把记账写进控制面，再把回执交回去。
    ///
    /// 这是造出 [`StartedSession`] 的**唯一**路径（那个类型没有公开构造式），
    /// 所以「桌面上多了一个受编排会话」与「控制面记着它」在类型层面就是同一件事
    /// —— 回执后来送没送到（发起侧可能已经在 [`ACTION_TIMEOUT`] 上放弃了）
    /// 与记账无关。幽灵乐手那条竞态因此不存在。
    ///
    /// `pane_id` 是乐手的 **PTY 编号**：编排者自己的身份也是它
    /// （`MINITERM_ORCHESTRATOR_PANE`），自指禁令因此是一次裸比较，不必翻译。
    ///
    /// ⚠️ 命令**没有**交到活着的 PTY 手上时别调它：那儿只是一个裸终端，不是受
    /// 编排会话；登记了反而会让它以「AI 会话状态不可知」永久占着一个名额。
    ///
    /// `orchestrator_label` 是编排者 pane 此刻的 tab 标题，由桌面侧查出来交进来
    /// —— 控制面自己认不出（它不认识布局树），而这一份得**抄下来**：编排者离场
    /// 之后那个 pane 就没了，「编排者已离场（某某）」这句话只能靠落地时的快照。
    /// 查不到就传空串。
    pub fn landed(self, pane_id: u32, orchestrator_label: &str) -> StartedSession {
        let session = self.draft.landed(pane_id, orchestrator_label);
        self.plane.register_landed(session.clone());
        StartedSession { session }
    }
}

/// 乐手起成之后桌面侧回来的东西。
///
/// **只能由 [`StartSessionSpec::landed`] 造出来** —— 拿得到它就说明记账已经落地。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedSession {
    session: OrchestratedSession,
}

impl StartedSession {
    /// 乐手的 pane 身份（= PTY 编号，与鉴权用的 `paneId` 同一个命名空间）。
    pub fn pane_id(&self) -> u32 {
        self.session.pane_id
    }
}

/// 起乐手失败的原因。**闭集** —— 桌面侧只负责判定，映射成对外错误码是本模块的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFailure {
    /// 从裁决到落地之间项目没了（用户刚好把它移除）。
    ProjectGone,
    /// 终端没建成，或启动命令没交到一根活着的 PTY 手上。
    SpawnFailed,
    /// 桌面主线程没在时限内答复（泵没接线 / 主线程忙死）。
    DesktopBusy,
}

/// 「这个 pane 还在 AI 会话里吗」—— **三态**。
///
/// 两态（拿状态字符串与 `"idle"` 裸比）在无 hook 的降级路径上两头都不成立：
///
/// - 命令不在 `crate::detect::AI_COMMANDS` 里的自定义启动器（ADR 0003：**任何**
///   启动器都能当乐手）没有 hook 时恒答 `"idle"`，判成「不占名额」就是硬上限
///   形同虚设 —— 可以无限起；
/// - 反过来，输入检测认得的 agent 自行退出时也未必收得到信号。
///
/// 于是把「答不上来」显式做成一档，并且**按占名额算**（fail-closed：宁可少起
/// 一个让编排者收到明确的 `sessionLimitReached`，也不让上限变成摆设）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSessionState {
    /// 明确在 AI 会话里：hook 权威说 `ai-*`，或输入检测认出了它。
    Active,
    /// 明确不在了：hook 已在这个 pane 上启用，而它说 `idle`（SessionEnd 是权威
    /// 的退出信号）；或者 pane 压根已经关了。**这一档才释放名额。**
    Ended,
    /// **答不上来**：这个 pane 没有 hook，输入检测也没把它认成 AI 会话。
    /// 「从没跑起来」与「跑过又退了」在这条路上不可区分，只能保守占着名额。
    Unknown,
}

/// 一个 pane 此刻的样子。名额判定与 `list-panes` 的状态列都读它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneLiveness {
    /// pane 还在桌面上吗（PTY 层面；用户关掉那个 tab 即 `false`）。
    pub alive: bool,
    /// AI 状态：`idle` / `ai-idle` / `ai-working`（与桌面徽章同一口径）。
    /// **展示 + `wait` 的终态判定**（`list-panes` 的状态列读它），
    /// 名额判定读的是下面那个。
    pub status: String,
    /// 名额判定的那一问，见 [`AiSessionState`]。宿主侧据实回答，
    /// 别为了让它「好看」去改 AI 状态机本身。
    pub ai_session: AiSessionState,
    /// 上一次发给 UI 的**状态成因**：hook 事件名原文（`Stop` / `PermissionRequest`
    /// / `Interrupt` / `Stall` …，见 [`crate::monitor::StatusChange::cause`]）。
    ///
    /// 工单 06 的 `wait` 有两档终态全靠它，一个字的新判定都不必加：
    ///
    /// - **「停在等审批 / 向人提问」的判据是成因，不是状态字符串**：Claude 的
    ///   `PermissionRequest` 落在 `ai-idle`，而 Codex 的落在 `ai-working`
    ///   （`hook_server::map_event_to_status` 对它有专门一条：批准后直接执行工具，
    ///   状态得留在工作中）。只看状态的话，一个正等着 Codex 审批的乐手会被当成
    ///   「还在跑」一直等到超时 —— 而 attention 恰恰是最该立刻告诉人的那一档。
    ///   判据本身用现成的 [`crate::hook_server::is_attention_cause`]。
    /// - **`ai-idle` 那一档要把成因原样交给编排者**：`Stop` 是真干完了，
    ///   `Interrupt` 是用户按了 Esc，`Stall` 是停摆兜底收敛的 —— 这三件事编排者
    ///   得分得开，否则它会把一次被打断的活当成做完了。
    ///
    /// `None` = 无 hook 的降级路径（那条路上 monitor 一律以无成因发射），或这个
    /// pane 还没发过任何状态。
    pub cause: Option<String>,
}

impl PaneLiveness {
    /// 已经不在了（pane 关了）的那一档。pane 都没了，AI 会话自然是明确结束。
    pub fn gone() -> Self {
        Self {
            alive: false,
            status: "idle".to_string(),
            ai_session: AiSessionState::Ended,
            cause: None,
        }
    }

    /// 占不占一个名额：**活着，且不是明确已经结束**（ADR 0003：上限只计活着的
    /// AI 会话；「不可知」按占用算，见 [`AiSessionState`]）。
    pub fn occupies_slot(&self) -> bool {
        self.alive && self.ai_session != AiSessionState::Ended
    }

    /// 收敛成 [`WaitState`] 了吗。`None` = 还在跑 / 说不上来，`wait` 接着等。
    ///
    /// **判定顺序有讲究：先看成因，再看状态。** attention 与状态不是一一对应的
    /// （见 [`Self::cause`] 的第一条）；反过来先看状态，Codex 的审批等待会被
    /// `ai-working` 那一档吞掉。
    ///
    /// 三档之外一律不收敛，尤其是 [`AiSessionState::Unknown`]（无 hook、输入检测
    /// 也没认出来的自定义启动器）：那一档**说不上来**里头还有没有 agent 在跑，
    /// 谎报成 `idle`（已退出）或 `ai-idle`（干完了）都是编排者据以做决定的假事实。
    /// 它会一路等到上界，拿一个 `pending` + `status: "idle"` 的诚实回执 ——
    /// 那两样合起来就是「这个乐手我看不透」的唯一签名。
    fn settled(&self) -> Option<WaitState> {
        if !self.alive {
            // pane 都没了：那是 `paneGone`，不是终态（由调用方答）
            return None;
        }
        if self
            .cause
            .as_deref()
            .is_some_and(crate::hook_server::is_attention_cause)
        {
            return Some(WaitState::Attention);
        }
        match self.status.as_str() {
            "ai-idle" => Some(WaitState::AiIdle),
            // 「已退出」只认**明确**结束的那一档（见 [`AiSessionState`]）
            "idle" if self.ai_session == AiSessionState::Ended => Some(WaitState::Idle),
            _ => None,
        }
    }
}

/// `wait` 认得的三类终态（第四类「pane 不存在」是错误码，见
/// [`ControlPlane::resolve_target`]）。
///
/// **一条判定逻辑都不新增**：三档全从既有事实读出来 —— hook 权威状态机
/// （`monitor::resolve_status`）、它落盘之后的两条兜底结论（`note_user_interrupt`
/// 的用户打断、`stall_settle_target` 的 10s 双静默收敛），以及 attention 的现成
/// 判据 [`crate::hook_server::is_attention_cause`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitState {
    /// 干完了。**成因照带** —— 只有 `Stop` 是真做完。
    AiIdle,
    /// 停在等审批或向人提问。**编排者不代答**（ADR 0003 的铁律）：它拿着成因去
    /// 自己的对话里播报，由人去那个乐手 pane 处理。零新增 UI —— 既有黄灯徽章
    /// 本来就亮着。
    Attention,
    /// agent 已经退出，pane 退回裸 shell（`alive` 仍为真）。
    Idle,
}

impl WaitState {
    /// 线上那个字符串。**与 `status` 同一套词汇**（`ai-idle` / `idle`）——
    /// 编排者不必再认第二种拼法，看到什么就是徽章上的那个意思；
    /// `attention` 是这套词汇里多出来的一档。
    fn name(self) -> &'static str {
        match self {
            Self::AiIdle => "ai-idle",
            Self::Attention => "attention",
            Self::Idle => "idle",
        }
    }
}

/// 到上界还没收敛时那个 `outcome`。
///
/// **不是错误**：见 [`ControlPlane::wait`] 的「超时不是错误」。
const WAIT_PENDING: &str = "pending";

// ─── 写穿：一段要交给乐手的输入（工单 05）────────────────────────

/// 一段**装配好的**、要当成「本人在那个 pane 上粘贴并回车」写进去的输入。
///
/// # 为什么装配在控制面这一侧
///
/// 装配是纯字符串运算，桌面侧只剩「把这串字节交给既有写穿入口」这一件事 ——
/// 于是[主缝测试](self)拿假宿主就能把**真正写进 PTY 的那串字节**逐字断言，
/// 不必起 gpui。桌面侧唯一还要做的判断是下面那个 `bracketed_paste`。
///
/// # 为什么带两份而不是一份
///
/// 包不包 `ESC[200~ … ESC[201~` 取决于**目标终端此刻开没开 DECSET 2004**
/// （`TermMode::BRACKETED_PASTE`），而那是 VT 状态机的事实，只有桌面侧读得到；
/// 本 crate 不依赖 `mt-terminal`/`gpui`，也不该为了猜它去建一条状态镜像。
///
/// 于是两份都在这里备好，桌面侧照着终端的真实模式挑一份 —— 与用户按 Ctrl+V
/// 时 `mt_ui::terminal::input::paste_to_bytes` 做的判断**是同一个判断**。
/// 不无条件包裹的理由：乐手的 agent 退出之后那个 pane 会退回裸 shell
/// （`cmd.exe` / PowerShell 都不认 bracketed paste），那时候硬包一层
/// 就是往用户的终端里灌一串肉眼可见的乱码。
///
/// # 装配口径（与 `paste_to_bytes` 对齐，由 `mt-app` 的对账测试钉住）
///
/// - **换行一律归一成 `\r`**：PTY 那头把 `\n` 当作「换行但不回车」，多行会出阶梯；
/// - **正文里的结束标记 `ESC[201~` 剔掉**：否则 prompt 自己就能把粘贴块提前
///   截断，后半截变成真键入 —— 那正是本命令要防的事；
/// - **末尾的换行删掉、另补一个 `\r`**，且那个 `\r` 在**包裹之外**：包在里头
///   只是往编辑框里插一个换行，送不出去。
#[derive(Clone, PartialEq, Eq)]
pub struct PaneInput {
    /// 目标开着 bracketed paste 时写这份。
    bracketed: String,
    /// 没开时写这份（= 归一后的正文 + 回车，与裸粘贴同形）。
    plain: String,
}

impl PaneInput {
    /// 装配一段输入。正文空（或只有空白）时返回 `None`。
    ///
    /// 空正文被拒**不只是入参校验**：那等于替用户按一下回车，而「停在等审批 /
    /// 向人提问时编排者不代答」是 ADR 0003 的铁律。裸回车是最顺手的代答姿势，
    /// 在这里就堵掉。
    ///
    /// 对外可见只为一件事：`mt-app` 那条[跨 crate 对账测试](自身模块注释的
    /// 「装配口径」)要拿它与 `mt_ui::paste_to_bytes` 比一次 —— 那是同时看得见
    /// 两侧的唯一地方。**装配本身仍然只在 [`ControlPlane::send`] 里被调用**。
    pub fn assemble(text: &str) -> Option<Self> {
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let body = normalized.replace("\x1b[201~", "");
        // 结尾的换行删掉：LLM 拿 heredoc 拼 prompt 时末尾几乎总带一个换行，
        // 原样留着就是替它多按一次回车 —— 在等确认的 TUI 里那一下会被当成「确认」。
        let body = body.trim_end_matches('\r');
        if body.trim().is_empty() {
            return None;
        }
        Some(Self {
            bracketed: format!("\x1b[200~{body}\x1b[201~\r"),
            plain: format!("{body}\r"),
        })
    }

    /// 目标终端开着 bracketed paste 时该写的那串。
    pub fn bracketed(&self) -> &str {
        &self.bracketed
    }

    /// 没开时该写的那串。
    pub fn plain(&self) -> &str {
        &self.plain
    }

    /// 按目标终端的真实模式挑一份。桌面侧唯一要做的判断。
    pub fn bytes(&self, bracketed_paste: bool) -> &str {
        if bracketed_paste {
            self.bracketed()
        } else {
            self.plain()
        }
    }
}

impl std::fmt::Debug for PaneInput {
    /// **正文一个字都不打**（ADR 0002 的防线延伸到编排者发的 prompt：那是用户
    /// 项目里的内容，不许经由日志 / panic 消息 / 错误上报漏出去）。只给长度。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneInput")
            .field("bytes", &self.plain.len())
            .finish_non_exhaustive()
    }
}

/// 一次写穿落地之后桌面侧回来的东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivered {
    /// 写的是哪一份（= 目标终端此刻开没开 bracketed paste）。
    ///
    /// **如实回答**，并且原样透给编排者：为假时它那段多行 prompt 是被逐行送进去
    /// 的，很可能已经被中途的换行提前发出去了 —— 这是它需要知道的事实，
    /// 不是可以粉饰的实现细节。
    pub bracketed_paste: bool,
}

/// 写穿失败的原因。**闭集** —— 桌面侧只负责判定，映射成对外错误码是本模块的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendFailure {
    /// 从裁决到落地之间那个 pane 没了（用户刚好把它关掉）。
    PaneGone,
    /// 找得到那个终端，但字节没交出去。
    WriteFailed,
    /// 桌面主线程没在时限内答复（泵没接线 / 主线程忙死）。
    DesktopBusy,
}

/// 控制面要桌面端**做事**的入口。
///
/// 与 [`OrchestratorHost`] 的分工：那条是查（纯读镜像，随手可答），这条是做
/// （要回主线程建 pane / 写 PTY）。刻意分成两个 trait 而不是并成一个，是因为
/// 两者的线程契约完全不同 —— 下面这条注意事项对 `Host` 不成立。
///
/// ⚠️ **三个方法都在 HTTP 线程上被调用，且调用方（本模块）不持任何锁**。
/// [`Self::start_session`] 与 [`Self::send_input`] 允许阻塞（实现方回主线程 +
/// 同步等回执 + 自己兜超时，超时就答 `DesktopBusy`，别永久挂住）；
/// [`Self::pane_liveness`] 必须**很快**（名额判定一次要问好几个 pane），
/// 实现方应当从后台线程读得到的状态里现答，别为它跳主线程。
pub trait OrchestratorActions: Send + Sync + 'static {
    /// 起一个受编排会话。
    ///
    /// 成功的唯一造法是 [`StartSessionSpec::landed`]（记账在它里头先落地，
    /// 见模块注释的「记账契约」）。实现方还应当自己兜住 [`ACTION_TIMEOUT`]：
    /// 到点没答复就返回 [`StartFailure::DesktopBusy`]，别把 HTTP 线程永久挂着。
    fn start_session(&self, spec: StartSessionSpec) -> Result<StartedSession, StartFailure>;

    /// 把一段输入写穿进某个乐手，**立即写、不排队**（ADR 0003 / 工单 05）。
    ///
    /// 语义必须与移动端指令**完全一致**：等价本人在桌面上对那个终端粘贴这段
    /// 内容并回车 —— 输入跟踪 / AI marker / SSH autofill 解除一个都不能少。
    /// 实现方**必须走既有的写穿入口**（`AppStore::write_to_pane`），别另开一条
    /// 裸 PTY 写：那几样语义全挂在那条链路上。
    ///
    /// 目标已经由 [`ControlPlane::resolve_target`] 裁决过（是调用者自己起的乐手、
    /// 且 pane 还活着），实现方不必再判可见范围；但从裁决到落地之间 pane 可能
    /// 刚好被关掉，那一档答 [`SendFailure::PaneGone`]。
    ///
    /// 写哪一份由实现方按目标终端的真实 bracketed paste 模式挑（[`PaneInput::bytes`]），
    /// 并在回执里[如实说明挑了哪份](Delivered::bracketed_paste)。
    fn send_input(&self, pane_id: u32, input: PaneInput) -> Result<Delivered, SendFailure>;

    /// 这个 pane 现在什么样（活着吗、在不在 AI 会话里）。
    fn pane_liveness(&self, pane_id: u32) -> PaneLiveness;
}

/// 什么都做不了的动作实现。
///
/// 只给测试和「尚未接线」用：起会话与写穿恒答 `DesktopBusy`
/// （fail-closed —— 没接线绝不能被当成「随便起」的理由），死活恒答「已经不在了」。
pub struct NoopOrchestratorActions;

impl OrchestratorActions for NoopOrchestratorActions {
    fn start_session(&self, _spec: StartSessionSpec) -> Result<StartedSession, StartFailure> {
        Err(StartFailure::DesktopBusy)
    }
    fn send_input(&self, _pane_id: u32, _input: PaneInput) -> Result<Delivered, SendFailure> {
        Err(SendFailure::DesktopBusy)
    }
    fn pane_liveness(&self, _pane_id: u32) -> PaneLiveness {
        PaneLiveness::gone()
    }
}

/// 一条范围记账：一个乐手的出身。
///
/// 「谁 spawn 了谁」由控制服务持有（ADR 0003）—— 桌面侧的 pane 上**不打**这个
/// 标记，因为「受编排会话出身不构成状态」：编排者退场了乐手照常活着，只是没人
/// 再够得到它。桌面 tab 上那枚出身标识也读这张表（[`ControlPlane::origins`]），
/// 不在 pane 上另存一份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratedSession {
    /// 乐手的 pane 身份（= PTY 编号）。
    pub pane_id: u32,
    /// 起它的编排者 pane。可见范围的判据就是这一条**加上**下面那个已离场位。
    pub orchestrator_pane_id: u32,
    /// 编排者的展示名（桌面侧在乐手落地那一刻抄下来的 tab 标题）。
    ///
    /// **抄一份而不是回头现查**：编排者离场之后那个 pane 就没了，现查只会查到
    /// 空 —— 而「编排者已离场」这句话恰恰要在那时候说出编排者是谁。空串 = 落地
    /// 时就没认出来（正常路径下不该发生），展示侧自己兜一个「未知编排者」。
    pub orchestrator_label: String,
    /// 编排者已经离场了吗（pane 关闭 / 令牌撤销 / 在同一编号上重新授予）。
    ///
    /// 出身**保留、标识降级**（工单 04）：乐手照常活着，tab 上那枚标识从
    /// 「由某某启动」变成「编排者已离场」。一旦置位就再也不会翻回来 ——
    /// MVP 不做收养，编排者 pane 重开是新身份，够不到前世起的会话。
    pub orchestrator_departed: bool,
    pub project_id: String,
    pub project_name: String,
    pub launcher_id: String,
    /// 启动器展示名。**不是命令文本** —— 那东西一个字都不出控制面。
    pub launcher_name: String,
}

impl OrchestratedSession {
    /// 这条记账归不归**此刻这个**编排者。
    ///
    /// 两问缺一不可：pane 编号对得上，**且编排者没离场过**。只比编号的话，
    /// 一个离场之后又在同一编号上拿到授予的编排者会把前世的乐手认领回去
    /// —— ADR 0003 明说 MVP 不做收养。PTY 编号单调递增，正常路径上碰不到这一档，
    /// 但「同一 pane 重复授予」（`ControlPlane::grant`）那条路做得到。
    fn belongs_to(&self, orchestrator_pane_id: u32) -> bool {
        self.orchestrator_pane_id == orchestrator_pane_id && !self.orchestrator_departed
    }
}

/// 一个 pane 的「受编排」出身，给桌面 tab 上那枚标识用。
///
/// **不是 [`OrchestratedSession`] 的克隆**：tab 每帧都画，展示只需要两样东西
/// （谁起的、那个人还在不在），项目/启动器那几样是给编排者看的，不进渲染路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOrigin {
    /// 起它的编排者的展示名；空串 = 落地时没认出来。
    pub orchestrator_label: String,
    /// `true` → 标识降级成「编排者已离场」。
    pub orchestrator_departed: bool,
}

// ─── 令牌与授予 ───────────────────────────────────────────────

/// 一枚已登记的编排能力。
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    /// 编排者自己的 pane（= `MINITERM_PTY_ID`）。
    pub pane_id: u32,
    /// 编排者所在项目 —— 可达范围的原点。
    pub project_id: String,
}

/// 同时存活的受编排会话上限（ADR 0003 的默认值）。
///
/// **不写死在判断处**：工单 08 把它变成了设置项，装配处（桌面侧的
/// `AiBridge::refresh_orchestrator_mirror`）按配置调一次
/// [`ControlPlane::set_session_cap`]，裁决那一行一个字都没动。用户没设过时
/// 配置里是 `None`，界面层拿这个常量兜底 —— 默认值只有这一处。
pub const DEFAULT_SESSION_CAP: usize = 5;

/// 一个编排者名下最多留多少条记账。
///
/// 记账只增不减是有意的（「你起的那个已经关了」比「不存在」有用），但一个长命
/// 编排者起关几百个乐手时那张表就只涨不消 —— 除了列表变长，真代价是
/// [`ControlPlane::live_session_count`] 每次数名额都要对**历史全部**乐手各问一次
/// 死活。于是给它一个上限，超出时**只丢已经关掉的、从最旧的丢起**：
/// 活着的一条都不能丢（丢了就成幽灵乐手），丢不动就让它超着。
///
/// 50 是「远大于并发上限（默认 5）、又不至于让数名额变贵」的一个数：编排者能
/// 回看的历史足够长，而现查死活最多问 50 次。
///
/// **公开出去是给设置项的上界当锚**：并发上限一旦让用户填得比这张表还长，
/// 记账就装不下全部**活着**的乐手（修剪只丢已关的、丢不动就超着），
/// 「活着的一条都不能丢」这条不变式会被逼到墙角。工单 08 的
/// `mt_app::orchestrator::SESSION_CAP_MAX` 由一条测试拿这个真常量钉住，
/// 不是抄一个字面量。
pub const MAX_SESSIONS_PER_ORCHESTRATOR: usize = 50;

/// 控制命令的闭集。
///
/// **命令名与「会不会阻塞主线程」这两件事只有这一份表**。此前它们各写一处
/// （`handle` 的 `match` 与一个 `matches!`），新命令漏登记阻塞属性就会静默卡住
/// hook 那条队 —— 而 hook 是 AI 状态感知的权威通道，正是这条设计要防的事。
///
/// 现在漏一处编译不过：加一个变体，[`Self::ALL`]（定长数组）、[`Self::name`]、
/// [`Self::needs_own_thread`]（两处穷尽 `match`，都没有 `_` 兜底）与
/// [`ControlPlane::handle`] 的分发一起报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    ListLaunchers,
    ListProjects,
    StartSession,
    ListPanes,
    Send,
    Wait,
}

impl Command {
    const ALL: [Self; 6] = [
        Self::ListLaunchers,
        Self::ListProjects,
        Self::StartSession,
        Self::ListPanes,
        Self::Send,
        Self::Wait,
    ];

    /// 路由里那一段（与 sidecar CLI 的 `Command::endpoint` 一字不差）。
    fn name(self) -> &'static str {
        match self {
            Self::ListLaunchers => "list-launchers",
            Self::ListProjects => "list-projects",
            Self::StartSession => "start-session",
            Self::ListPanes => "list-panes",
            Self::Send => "send",
            Self::Wait => "wait",
        }
    }

    /// 命令名里带查询串 / 多层路径的一律认不出来，别去猜。
    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// 这条命令**能不能就地在 HTTP 那条循环上跑完**。
    ///
    /// 工单 03~05 里它叫 `blocks_on_desktop`，工单 06 改成了现在这个名字：那时候
    /// 进表的理由只有一种（要回 gpui 主线程等回执），名字与判据恰好同义；`wait`
    /// 打破了这个巧合 —— 它**一次也不碰主线程**，却要在这条线程上睡几分钟。
    /// [`try_handle_control`] 关心的从来只是结论，于是名字改成说结论的那一个。
    ///
    /// 两种进表的理由：
    ///
    /// - **要回桌面主线程等回执**（`start-session` / `send`）：那一等按
    ///   [`ACTION_TIMEOUT`] 算，最长 3 秒；
    /// - **要长轮询**（`wait`）：主线程一次不惊动，但会在这条线程上睡到
    ///   [`WAIT_MAX`]（默认 [`WAIT_DEFAULT`]）—— 比前一种久两个数量级。
    ///
    /// 两种都绝不能就地做：hook 那个 HTTP 服务是**单线程循环**
    /// （`hook_server.rs` 的 `for request in server.incoming_requests()`），占着它
    /// 就是把 AI 状态感知那条权威通道一起卡住 —— 而 `wait` 卡的还不是三秒，
    /// 是几分钟。工单 07 的 `read` 要回主线程读终端画面，同样进这一档。
    fn needs_own_thread(self) -> bool {
        match self {
            Self::ListLaunchers | Self::ListProjects | Self::ListPanes => false,
            Self::StartSession | Self::Send | Self::Wait => true,
        }
    }
}

/// 控制面本体。内部全是 `Arc`，`Clone` 即同一份。
#[derive(Clone, Default)]
pub struct ControlPlane {
    inner: Arc<Inner>,
}

struct Inner {
    host: Mutex<Option<Arc<dyn OrchestratorHost>>>,
    actions: Mutex<Option<Arc<dyn OrchestratorActions>>>,
    /// 令牌登记 + 范围记账。三张表是同一份事实的几个索引,必须在**同一把锁**
    /// 下变更 —— 拆成两把锁时 `grant`(先 grants 后 tokens)与 `revoke_pane`
    /// (先 tokens 后 grants)的加锁顺序相反,是典型的 AB-BA 死锁雷。
    ///
    /// ⚠️ 加新状态时守住两条:① 与令牌相关的一切都进这把锁,别开第二把;
    /// ② **持这把锁时绝不调注入进来的 trait**([`OrchestratorHost`] /
    /// [`OrchestratorActions`] 的实现是上层代码,自带别的锁)—— 先在锁内把
    /// 需要的东西拷出来,出锁再问。
    registry: Mutex<TokenRegistry>,
    /// 并发上限。用原子量而不是再加一把锁:多一把锁就多一组与 `registry`
    /// 交叉持有的可能。
    session_cap: AtomicUsize,
    /// 记账版本号，见 [`ControlPlane::origins_version`]。同样是原子量、同样的
    /// 理由 —— 渲染路径每帧都读它，绝不能让它多长一把锁出来。
    origins_version: AtomicU64,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            host: Mutex::new(None),
            actions: Mutex::new(None),
            registry: Mutex::new(TokenRegistry::default()),
            session_cap: AtomicUsize::new(DEFAULT_SESSION_CAP),
            origins_version: AtomicU64::new(0),
        }
    }
}

#[derive(Default)]
struct TokenRegistry {
    /// token → 授予。
    grants: HashMap<String, Grant>,
    /// pane → token（pane 关闭时按 pane 撤销，重复授予时顶掉旧的）。
    tokens: HashMap<u32, String>,
    /// 乐手 pane → 它的出身。**可见范围铁律的事实底座**：不在这张表里的 pane，
    /// 对编排者而言就是不存在。
    sessions: HashMap<u32, OrchestratedSession>,
    /// 已经关掉的乐手 pane（`revoke_pane` 走到乐手自己那一次时记下）。
    ///
    /// **只给修剪用**（见 [`MAX_SESSIONS_PER_ORCHESTRATOR`]）：对外那个 `alive`
    /// 一律现查 [`OrchestratorActions::pane_liveness`]，一个字都不读这里 ——
    /// 事件驱动的死活记账漏一次就是永久占住名额，那条路本模块不走。
    closed: HashSet<u32>,
}

impl TokenRegistry {
    /// 编排者退场：名下**活着的**乐手记账标记成「已离场」，已经关掉的就地回收。
    ///
    /// 为什么留着活的：tab 上那枚出身标识要降级成「编排者已离场」（工单 04），
    /// 记账一抹这句话就无从说起 —— 而 ADR 0003 明说编排者退出不连坐乐手。
    ///
    /// 为什么已关的要丢：那条记账既没有 tab 可标（pane 都没了），也没有编排者
    /// 读得到（下面那个 [`OrchestratedSession::belongs_to`] 一律答否），留着就是
    /// 一条**永不回收**的泄漏 —— 修剪只在 [`Self::trim`] 也就是登记新乐手那一刻
    /// 跑，而已离场的编排者再也不会登记新乐手了。
    ///
    /// 剩下的那些「已离场 + 乐手还活着」的记账由谁回收：乐手自己关掉时那次
    /// `revoke_pane`（见 [`ControlPlane::revoke_pane`]）。于是已离场记账的条数
    /// 恒不超过桌面上还活着的受编排 pane 数，不是一条只涨不消的表。
    fn depart(&mut self, orchestrator_pane_id: u32) {
        let closed = &mut self.closed;
        self.sessions.retain(|_, s| {
            if !s.belongs_to(orchestrator_pane_id) {
                return true;
            }
            if closed.remove(&s.pane_id) {
                return false;
            }
            s.orchestrator_departed = true;
            true
        });
    }

    /// 记账超出上限时，**只丢已经关掉的、从最旧的丢起**。
    ///
    /// PTY 编号单调递增，所以按 pane_id 升序就是起的先后序。活着的一条都不丢：
    /// 丢掉一条活着的记账 = 桌面上多一个谁也够不到的幽灵乐手。
    ///
    /// 只数**当前身份**名下的（`belongs_to`）：已离场编排者留下的那些记账既不
    /// 占名额也进不了 `list-panes`，凭什么把新身份的额度吃掉。
    fn trim(&mut self, orchestrator_pane_id: u32) {
        let mut mine: Vec<u32> = self
            .sessions
            .values()
            .filter(|s| s.belongs_to(orchestrator_pane_id))
            .map(|s| s.pane_id)
            .collect();
        let Some(mut over) = mine.len().checked_sub(MAX_SESSIONS_PER_ORCHESTRATOR) else {
            return;
        };
        if over == 0 {
            return;
        }
        mine.sort_unstable();
        for id in mine {
            if over == 0 {
                break;
            }
            if self.closed.remove(&id) {
                self.sessions.remove(&id);
                over -= 1;
            }
        }
    }
}

impl ControlPlane {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入桌面能力。未注入时等价于 [`NoopOrchestratorHost`]。
    pub fn set_host(&self, host: Arc<dyn OrchestratorHost>) {
        *self.inner.host.lock() = Some(host);
    }

    /// 注入桌面动作。未注入时等价于 [`NoopOrchestratorActions`]（起会话恒失败）。
    pub fn set_actions(&self, actions: Arc<dyn OrchestratorActions>) {
        *self.inner.actions.lock() = Some(actions);
    }

    /// 改并发上限（工单 08 的设置项落点）。`0` = 不许起任何乐手。
    pub fn set_session_cap(&self, cap: usize) {
        self.inner.session_cap.store(cap, Ordering::Relaxed);
    }

    pub fn session_cap(&self) -> usize {
        self.inner.session_cap.load(Ordering::Relaxed)
    }

    /// 授予一枚编排令牌：随机生成、每 pane 一枚，同一 pane 重复授予顶掉旧的。
    ///
    /// **重复授予 = 新身份**：同一个 pane 编号上再来一次（PTY 重开 / SSH 重连）
    /// 时，前一次授予名下的乐手一律转「已离场」—— 与 [`Self::revoke_pane`] 同一句
    /// 话，MVP 不做收养。少了这一步，前世的乐手会被今生认领回去。
    pub fn grant(&self, pane_id: u32, project_id: &str) -> String {
        let token = new_token();
        {
            let mut guard = self.inner.registry.lock();
            let registry = &mut *guard;
            if let Some(old) = registry.tokens.insert(pane_id, token.clone()) {
                registry.grants.remove(&old);
            }
            registry.depart(pane_id);
            registry.grants.insert(
                token.clone(),
                Grant {
                    pane_id,
                    project_id: project_id.to_string(),
                },
            );
        }
        self.bump_origins();
        token
    }

    /// pane 关闭 / 重开 PTY：撤销它手上的令牌，并把它名下的范围记账**降级**。
    ///
    /// **不杀乐手**（ADR 0003：乐手 pane 里可能躺着改到一半的代码，大脑崩了不该
    /// 烧现场）；只是从此没人够得到它们了 —— 编排者 pane 重开是新身份，
    /// MVP 不做收养。
    ///
    /// 两路各走各的：
    ///
    /// - **编排者那一路**（[`TokenRegistry::depart`]）：名下活着的乐手记账**留着**，
    ///   只标一个「已离场」位 —— 桌面 tab 上那枚出身标识据此从「由某某启动」
    ///   降级成「编排者已离场」（工单 04）。已经关掉的那些就地回收。
    ///   工单 03 那一版是整体删除，出身信息随之消失，标识就无从显示了。
    /// - **乐手那一路**：记账同样留着，只在 `closed` 里记一笔，好让 `list-panes`
    ///   与目标解析答得出「你起的那个已经关了」，而不是一句无从下手的「不存在」；
    ///   那一笔同时是[记账修剪](MAX_SESSIONS_PER_ORCHESTRATOR)挑「可以丢的那些」
    ///   的依据。**例外**：它的编排者已经离场了 —— 那条记账从此既没有 tab 可标
    ///   也没有编排者读得到，就地回收，免得成一条永不过期的泄漏。
    ///
    /// 名额一律不受影响 —— 它是现查存活的。
    pub fn revoke_pane(&self, pane_id: u32) {
        {
            let mut guard = self.inner.registry.lock();
            // 先 deref 出来:`MutexGuard` 上的字段访问会整体借走 guard,
            // `sessions` 与 `closed` 那两笔改动就没法同时成立。
            let registry = &mut *guard;
            if let Some(token) = registry.tokens.remove(&pane_id) {
                registry.grants.remove(&token);
            }
            // 乐手那一路
            match registry.sessions.get(&pane_id) {
                Some(s) if s.orchestrator_departed => {
                    registry.sessions.remove(&pane_id);
                    registry.closed.remove(&pane_id);
                }
                Some(_) => {
                    registry.closed.insert(pane_id);
                }
                None => {}
            }
            // 编排者那一路
            registry.depart(pane_id);
        }
        self.bump_origins();
    }

    /// 记下「这个乐手落地了」。**唯一的写入点**，由 [`StartSessionSpec::landed`]
    /// 在动作实现拿到 pane 身份的那一刻调 —— 早于任何回执（见模块注释）。
    fn register_landed(&self, session: OrchestratedSession) {
        {
            let mut registry = self.inner.registry.lock();
            let orchestrator = session.orchestrator_pane_id;
            // PTY 编号单调递增，编号复用理论上不会发生；真撞上也别让新乐手继承
            // 前一条的「已关」标记。
            registry.closed.remove(&session.pane_id);
            registry.sessions.insert(session.pane_id, session);
            registry.trim(orchestrator);
        }
        self.bump_origins();
    }

    // ─── 出身快照（桌面 tab 上那枚标识）───────────────────────

    /// 记账版本号：`sessions` 每变一次就 +1。
    ///
    /// **给渲染路径用**：tab 每帧都画，而记账住在 `registry` 那把锁后面
    /// （hook / 控制 HTTP 线程也在用它）。渲染侧照 `layout_snapshot` 的老规矩
    /// 缓存一份 [`Self::origins`]，号没变就只花一次原子读。
    ///
    /// ⚠️ 用法有顺序要求：**先读号、再取快照**。反过来（先取快照再读号）会把
    /// 一份旧快照配上新号存住，之后再也刷不掉。按正序则最坏是多刷一次。
    pub fn origins_version(&self) -> u64 {
        self.inner.origins_version.load(Ordering::Acquire)
    }

    /// 全部受编排会话的出身：pane 编号 → [`SessionOrigin`]。
    ///
    /// **含已离场编排者名下的**（那正是要显示「编排者已离场」的那些），
    /// 也含已经关掉的乐手（tab 都没了，查不到就是不画，无害）。
    /// 按 [`Self::origins_version`] 缓存着用，别每帧调。
    pub fn origins(&self) -> HashMap<u32, SessionOrigin> {
        self.inner
            .registry
            .lock()
            .sessions
            .values()
            .map(|s| {
                (
                    s.pane_id,
                    SessionOrigin {
                        orchestrator_label: s.orchestrator_label.clone(),
                        orchestrator_departed: s.orchestrator_departed,
                    },
                )
            })
            .collect()
    }

    /// 记账变了。**必须在放掉 `registry` 锁之后调** —— 见 [`Self::origins_version`]
    /// 的顺序要求。
    fn bump_origins(&self) {
        self.inner.origins_version.fetch_add(1, Ordering::Release);
    }

    /// 该 pane 当前是否持有编排能力（UI 标识与工单 03 的裁决用）。
    pub fn is_orchestrator(&self, pane_id: u32) -> bool {
        self.inner.registry.lock().tokens.contains_key(&pane_id)
    }

    /// 只做「解析 + 鉴权」的那一半。
    ///
    /// 给 [`try_handle_control`] 在**把活丢进新线程之前**先过一道用的：不然同机
    /// 任意进程都能靠一串无令牌请求让我们不停起线程。鉴权很便宜（一次哈希查找），
    /// 在 HTTP 线程上做掉不影响 hook。
    fn authorize_body(&self, body: &str) -> Result<Grant, ControlError> {
        let request: ControlRequest =
            serde_json::from_str(body).map_err(|_| ControlError::BadRequest)?;
        self.authorize(&request.token, request.pane_id)
    }

    /// 校验令牌 + 自称身份。任何一环对不上都是 401，不降级。
    fn authorize(&self, token: &str, pane_id: u32) -> Result<Grant, ControlError> {
        if token.is_empty() {
            return Err(ControlError::MissingToken);
        }
        let registry = self.inner.registry.lock();
        let Some(grant) = registry.grants.get(token) else {
            return Err(ControlError::InvalidToken);
        };
        if grant.pane_id != pane_id {
            // 令牌与自称的 pane 对不上：要么被抄去了别处，要么调用方在撒谎。
            return Err(ControlError::InvalidToken);
        }
        Ok(grant.clone())
    }

    fn host_launchers(&self) -> Vec<ControlLauncher> {
        match self.inner.host.lock().as_ref() {
            Some(h) => h.launchers(),
            None => Vec::new(),
        }
    }

    fn host_projects(&self) -> Vec<ControlProject> {
        match self.inner.host.lock().as_ref() {
            Some(h) => h.projects(),
            None => Vec::new(),
        }
    }

    /// 取动作实现。**取出来就放锁** —— 后面那些调用可能阻塞好几秒
    /// （`start-session` 要等主线程），持着锁等于把整个控制面挂住。
    fn actions(&self) -> Arc<dyn OrchestratorActions> {
        match self.inner.actions.lock().as_ref() {
            Some(a) => a.clone(),
            None => Arc::new(NoopOrchestratorActions),
        }
    }

    // ─── 范围记账 ─────────────────────────────────────────────

    /// 某个编排者名下的乐手，**按 pane 编号升序**（= 起的先后序，PTY 编号单调
    /// 递增）。顺序稳定是给编排者看的：反复 `list-panes` 顺序跳来跳去很难读。
    ///
    /// 已离场那一批不在其中（[`OrchestratedSession::belongs_to`]）：那是前世的
    /// 身份留下的记账，只用来在 tab 上画「编排者已离场」。
    fn sessions_of(&self, orchestrator_pane_id: u32) -> Vec<OrchestratedSession> {
        let registry = self.inner.registry.lock();
        let mut list: Vec<OrchestratedSession> = registry
            .sessions
            .values()
            .filter(|s| s.belongs_to(orchestrator_pane_id))
            .cloned()
            .collect();
        list.sort_by_key(|s| s.pane_id);
        list
    }

    /// 某个编排者名下乐手的 **pane 编号**，升序。
    ///
    /// 数名额只要 id，`sessions_of` 那条会把每条记账的六个 `String` 都克隆一遍
    /// （历史全部乐手 × 每次 `start-session`），白花。
    ///
    /// 同样跳过已离场那一批：**已离场编排者留下的记账不占任何人的名额**
    /// （它们既进不了 `list-panes`，也不该让新身份少起一个）。
    fn session_ids_of(&self, orchestrator_pane_id: u32) -> Vec<u32> {
        let registry = self.inner.registry.lock();
        let mut ids: Vec<u32> = registry
            .sessions
            .values()
            .filter(|s| s.belongs_to(orchestrator_pane_id))
            .map(|s| s.pane_id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// 当前占着名额的乐手数：**每次请求现查存活**，不靠事件驱动记账。
    ///
    /// ⚠️ 先在 [`Self::session_ids_of`] 里把 id 拷出来（那把锁到此已经放掉），
    /// 再逐个问 [`OrchestratorActions::pane_liveness`] —— 持 `registry` 锁去调
    /// 注入进来的外部代码，就是把上层的锁序绑进本模块的锁序。
    fn live_session_count(&self, orchestrator_pane_id: u32) -> usize {
        let ids = self.session_ids_of(orchestrator_pane_id);
        let actions = self.actions();
        ids.into_iter()
            .filter(|id| actions.pane_liveness(*id).occupies_slot())
            .count()
    }

    /// 解析「编排者点名的那个乐手」。**05/06/07 的 send / wait / read 共用这一条**
    /// —— 可见范围铁律只该有一处实现，三条命令各写一遍就是三个走散的机会。
    ///
    /// 三种结论各自明确，且**一条都不泄露**：
    ///
    /// - 目标是编排者自己 → [`ControlError::SelfTarget`]（自指禁令，ADR 0003）。
    ///   自己的身份本来就钉在它的环境变量里，给专门的码是让它能自我纠正。
    /// - 目标不在自己的记账里（别人的乐手 / 用户亲手开的会话 / 编造的编号 /
    ///   **前世那个身份起的乐手**）→ 一律 [`ControlError::PaneNotFound`]。
    ///   **不区分**「不存在」与「存在但不归你」—— 区分开来就是一台探测桌面上有
    ///   哪些 pane 的扫描器。已离场那一档由 [`OrchestratedSession::belongs_to`]
    ///   挡掉：那个编排者的令牌早撤了，谁也认证不成它；万一同一编号又拿到授予，
    ///   已离场位保证它还是够不着（MVP 不做收养）。
    /// - 是自己的乐手、但 pane 已经关了 → [`ControlError::PaneGone`]。这条**可以**
    ///   说：那是它自己起的东西，告诉它「关了」比「不存在」有用得多。
    // 收成 `pub(crate)` 是因为它没有跨 crate 消费者，也不该有 ——
    // 可见范围铁律是本模块的裁决。工单 05 的 `send` 是它的第一个真消费者，
    // 06/07 的 wait / read 照用同一条。
    pub(crate) fn resolve_target(
        &self,
        grant: &Grant,
        target_pane_id: u32,
    ) -> Result<OrchestratedSession, ControlError> {
        if target_pane_id == grant.pane_id {
            return Err(ControlError::SelfTarget);
        }
        let session = {
            let registry = self.inner.registry.lock();
            registry
                .sessions
                .get(&target_pane_id)
                .filter(|s| s.belongs_to(grant.pane_id))
                .cloned()
        };
        let session = session.ok_or(ControlError::PaneNotFound)?;
        // 锁已经放掉了才问死活（见 `live_session_count` 的同款注意事项）
        if !self.actions().pane_liveness(target_pane_id).alive {
            return Err(ControlError::PaneGone);
        }
        Ok(session)
    }

    /// 处理一条控制请求。`command` 是 `/control/` 之后那一段。
    pub fn handle(&self, command: &str, body: &str) -> ControlOutcome {
        let request: ControlRequest = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(_) => return ControlError::BadRequest.into_outcome(),
        };
        let grant = match self.authorize(&request.token, request.pane_id) {
            Ok(g) => g,
            Err(e) => return e.into_outcome(),
        };
        let Some(command) = Command::parse(command) else {
            return ControlError::UnknownCommand.into_outcome();
        };
        match command {
            Command::ListLaunchers => {
                let launchers = self
                    .host_launchers()
                    .into_iter()
                    .map(|l| LauncherView {
                        id: l.id,
                        name: l.name,
                    })
                    .collect();
                ok_outcome(&ControlData::Launchers { launchers })
            }
            Command::ListProjects => {
                let all = self.host_projects();
                let reachable = reachable_projects(&all, &grant.project_id);
                if reachable.is_empty() {
                    // 编排者所在的项目已经不在项目表里（被删了 / 配置没接线）。
                    // 与「分组里只有自己」区分得开：后者至少有自己那一条。
                    return ControlError::ProjectUnavailable.into_outcome();
                }
                let projects = reachable
                    .into_iter()
                    .map(|p| ProjectView {
                        current: p.id == grant.project_id,
                        // 远程项目照列（编排者在那儿有别的活可干，工单 05 之后
                        // 还能读它的画面），但**先告诉它起不了乐手** —— 与其让它
                        // 试一次再吃 `remoteProjectUnsupported`，不如列表里就写清。
                        can_start_sessions: !p.is_remote(),
                        id: p.id,
                        name: p.name,
                        path: p.path,
                    })
                    .collect();
                ok_outcome(&ControlData::Projects { projects })
            }
            Command::StartSession => self.start_session(&grant, &request),
            Command::Send => self.send(&grant, &request),
            Command::Wait => self.wait(&grant, &request),
            Command::ListPanes => {
                let actions = self.actions();
                let panes = self
                    .sessions_of(grant.pane_id)
                    .into_iter()
                    .map(|s| {
                        let liveness = actions.pane_liveness(s.pane_id);
                        pane_view(&s, &liveness)
                    })
                    .collect();
                ok_outcome(&ControlData::Panes { panes })
            }
        }
    }

    /// `start-session`：四条裁决按「越便宜越靠前」排，全过了才去动桌面。
    fn start_session(&self, grant: &Grant, request: &ControlRequest) -> ControlOutcome {
        let Some(launcher_id) = nonempty(request.launcher_id.as_deref()) else {
            return ControlError::BadRequest.into_outcome();
        };
        // 启动器**现查**：与可达项目同一个口径，用户刚加的那条要立刻能用。
        // 全量名单（任何启动器都能当乐手，ADR 0003）—— 目标启动器自己勾没勾
        // 「允许编排」在这里**不看**：那是「谁能当编排者」的授予位，
        // 而乐手一律不授予（禁套娃）。
        let Some(launcher) = self
            .host_launchers()
            .into_iter()
            .find(|l| l.id == launcher_id)
        else {
            return ControlError::LauncherNotFound.into_outcome();
        };

        // 裁决一：范围。缺省落在编排者自己那个项目。
        let all = self.host_projects();
        let reachable = reachable_projects(&all, &grant.project_id);
        if reachable.is_empty() {
            return ControlError::ProjectUnavailable.into_outcome();
        }
        let target_id = nonempty(request.project_id.as_deref()).unwrap_or(&grant.project_id);
        let Some(project) = reachable.iter().find(|p| p.id == target_id) else {
            // 组外项目与压根不存在的项目**同一个错误**：「那个项目存在但不在你
            // 组里」本身就是不该泄露的信息（可见范围铁律的项目版）。
            return ControlError::ProjectUnreachable.into_outcome();
        };
        // 裁决二：SSH 远程项目当不了乐手宿主（令牌注不到远端去，见 `ControlProject`）。
        if project.is_remote() {
            return ControlError::RemoteProjectUnsupported.into_outcome();
        }

        // 裁决三：名额。**不静默排队** —— 明确报错，让编排者自己调度
        // （ADR 0003）。判据是现查存活，退出的乐手立刻把名额还回来。
        //
        // 已知窗口：同一个编排者并发发两条 start-session 时，两条都可能在各自
        // 数完之后才落地，于是短暂超出上限一个。不为它加锁 —— 唯一的收紧办法是
        // 持锁跨过 `actions.start_session()`（一条会阻塞好几秒的外部调用），
        // 代价远大于收益；而编排者是串行调 CLI 的单进程，实际到不了这个窗口。
        if self.live_session_count(grant.pane_id) >= self.session_cap() {
            return ControlError::SessionLimitReached.into_outcome();
        }

        // 裁决四（禁套娃）在类型上：[`StartSessionSpec`] 里没有「要不要授予」
        // 这个位，桌面侧那条路只有一种可能。
        //
        // 这份 spec 同时是**落地登记凭据**：展示用的项目名/启动器名在这里就一并
        // 带上（宿主刚查过，手上就有），桌面侧原样回填即可 —— 记账因此在乐手
        // 落地那一刻就完整，不必等回执回到这条线程（见模块注释的「记账契约」）。
        let spec = StartSessionSpec {
            draft: SessionDraft {
                orchestrator_pane_id: grant.pane_id,
                project_id: project.id.clone(),
                project_name: project.name.clone(),
                launcher_id: launcher.id,
                launcher_name: launcher.name,
            },
            plane: self.clone(),
        };
        let started = match self.actions().start_session(spec) {
            Ok(s) => s,
            Err(StartFailure::ProjectGone) => return ControlError::ProjectUnreachable.into_outcome(),
            Err(StartFailure::SpawnFailed) => return ControlError::StartFailed.into_outcome(),
            Err(StartFailure::DesktopBusy) => return ControlError::DesktopBusy.into_outcome(),
        };

        // 记账已经在 `landed` 里落地了，这里不再插一次（只有一个写入点）。
        // 回执与 `list-panes` 里那一条**同形**：编排者只需要认识一种 pane 视图。
        let liveness = self.actions().pane_liveness(started.session.pane_id);
        ok_outcome(&ControlData::Pane {
            pane: pane_view(&started.session, &liveness),
        })
    }

    /// `send`：把一段 prompt 写穿进自己的某个乐手。
    ///
    /// **立即写、不排队**（ADR 0003）：写不进去就明确报错，不缓存也不重试 ——
    /// 缓存重试会让「我发了」与「它收到了」这两件事分家，而编排者接下来要靠
    /// `wait` / `read` 判断乐手在干什么。
    ///
    /// 裁决顺序照旧「越便宜越靠前」：先把请求体本身看完（不惊动记账与桌面），
    /// 再过可见范围铁律，最后才动 PTY。
    fn send(&self, grant: &Grant, request: &ControlRequest) -> ControlOutcome {
        let Some(target_pane_id) = request.target_pane_id else {
            return ControlError::BadRequest.into_outcome();
        };
        // 空正文即拒（见 [`PaneInput::assemble`]：裸回车是最顺手的代答姿势）。
        let Some(input) = PaneInput::assemble(request.text.as_deref().unwrap_or_default()) else {
            return ControlError::EmptyInput.into_outcome();
        };
        // 可见范围铁律走**共用**的那一条，别在这儿另写一遍。
        let session = match self.resolve_target(grant, target_pane_id) {
            Ok(s) => s,
            Err(e) => return e.into_outcome(),
        };
        match self.actions().send_input(session.pane_id, input) {
            Ok(delivered) => ok_outcome(&ControlData::Sent {
                sent: SentView {
                    pane_id: session.pane_id,
                    bracketed_paste: delivered.bracketed_paste,
                },
            }),
            Err(SendFailure::PaneGone) => ControlError::PaneGone.into_outcome(),
            Err(SendFailure::WriteFailed) => ControlError::SendFailed.into_outcome(),
            Err(SendFailure::DesktopBusy) => ControlError::DesktopBusy.into_outcome(),
        }
    }

    /// `wait`：长轮询等一个乐手收敛成终态（工单 06）。
    ///
    /// # 它不走桌面主线程那条泵
    ///
    /// 那条泵的时限是 [`ACTION_TIMEOUT`]（3 秒，为「建一个 pane」定的），而这条
    /// 命令要等的是**一个 AI 回合** —— 几分钟是常态，差两个数量级。于是 `wait`
    /// 一次也不打扰 gpui 主线程：它就在 [`try_handle_control`] 起的那条一次性
    /// 线程上反复问 [`OrchestratorActions::pane_liveness`]（那个方法的契约本来
    /// 就是「很快、不跳主线程」，读的都是 `Arc<Mutex<..>>` 后面的只读状态）。
    ///
    /// **轮询期间一把锁都不持**：[`Self::resolve_target`] 出锁之后才问死活，
    /// 循环里手上只剩一个 `Arc<dyn OrchestratorActions>`（[`Self::actions`] 早就
    /// 把 `actions` 那把锁放了）。持着 `registry` 睡几分钟会把整个控制面挂住 ——
    /// 连 `revoke_pane` 都进不来。
    ///
    /// # 判定完全复用既有状态机
    ///
    /// 三档终态由 [`PaneLiveness::settled`] 从既有事实读出来，本函数一个判定都不
    /// 新增：hook 权威状态机、它落盘之后的两条兜底（用户打断 / 停摆收敛）、
    /// 以及 attention 的现成判据。成因**原文照带**给编排者。
    ///
    /// # 超时不是错误
    ///
    /// 等到耐心用尽还没收敛，答的是 **200 + `outcome: "pending"`**，不是 HTTP
    /// 错误码 ——「它还没干完」是一条正常的观测结果，编排者据此决定继续等还是先
    /// 去干别的。做成错误码就得给它一个 CLI 退出码档位，而那三档说的都是
    /// 「你的请求不对 / 我们出了问题」，两样都不是。
    ///
    /// # attention 到此为止
    ///
    /// 收到 attention 就立刻返回，**本函数不做任何代答动作**（ADR 0003 的铁律）：
    /// 它把成因交给编排者，由编排者在自己的对话里请用户去那个 pane 处理。
    /// 零新增 UI —— 那个 pane 的黄灯徽章本来就亮着。
    ///
    /// ⚠️ **一条已知的窄口子**：`send` 之后**立刻** `wait`，那一瞬的状态可能还是
    /// 上一回合的 `ai-idle` + `Stop`（agent 还没来得及发 `UserPromptSubmit`），
    /// 于是拿到一个假的「干完了」。回执里 `waitedMs` 接近 0 是它唯一的签名。
    /// 刻意**不**在这里加一条「先等它动起来」的启发式：那正是「不新增判定逻辑」
    /// 要挡的东西，而且回合极短时那条启发式自己也会误判。留档见工单 06。
    fn wait(&self, grant: &Grant, request: &ControlRequest) -> ControlOutcome {
        let Some(target_pane_id) = request.target_pane_id else {
            return ControlError::BadRequest.into_outcome();
        };
        let patience = wait_patience(request.timeout_ms);
        // 可见范围铁律走**共用**的那一条（自指 / 不是你起的 / 已经关了）。
        // 只在开头判一次：轮询期间编排者的令牌若被撤销，跑 CLI 的那个 pane 本身
        // 也已经没了，这一趟回执答给谁都无所谓。
        let session = match self.resolve_target(grant, target_pane_id) {
            Ok(s) => s,
            Err(e) => return e.into_outcome(),
        };
        let actions = self.actions();
        let started = Instant::now();
        loop {
            let liveness = actions.pane_liveness(session.pane_id);
            if !liveness.alive {
                // 等着等着被用户关掉了。与 `resolve_target` 开头那一档同一个码：
                // 「你起的那个已经关了」比一个憋到上界的 `pending` 有用得多。
                return ControlError::PaneGone.into_outcome();
            }
            let elapsed = started.elapsed();
            if let Some(state) = liveness.settled() {
                return wait_outcome(session.pane_id, state.name(), &liveness, elapsed);
            }
            let Some(remaining) = patience.checked_sub(elapsed).filter(|r| !r.is_zero()) else {
                return wait_outcome(session.pane_id, WAIT_PENDING, &liveness, elapsed);
            };
            // 剩得比一拍还少就只睡那么多：编排者给的超时是个承诺，别超出去。
            // （`timeoutMs: 0` 因此是合法的「只看一眼就回来」。）
            std::thread::sleep(WAIT_POLL_INTERVAL.min(remaining));
        }
    }
}

/// 编排者要的耐心 → 实际等多久。不给就是 [`WAIT_DEFAULT`]，超上界按 [`WAIT_MAX`] 算。
///
/// **钳而不拒**：上界是我们这侧的实现约束，编排者无从知道；为一个能安全钳回来的
/// 数字报错只是多一趟往返。回执里的 `waitedMs` 会如实说明实际等了多久，
/// 它自己看得出来被钳过。
fn wait_patience(timeout_ms: Option<u64>) -> Duration {
    match timeout_ms {
        Some(ms) => Duration::from_millis(ms).min(WAIT_MAX),
        None => WAIT_DEFAULT,
    }
}

/// 装一条 `wait` 回执。三档终态与 `pending` 共用一种形状 —— 编排者读 `outcome`
/// 分支，别的字段照旧在那儿。
fn wait_outcome(
    pane_id: u32,
    outcome: &'static str,
    liveness: &PaneLiveness,
    waited: Duration,
) -> ControlOutcome {
    ok_outcome(&ControlData::Waited {
        waited: WaitView {
            pane_id,
            outcome,
            status: liveness.status.clone(),
            cause: liveness.cause.clone(),
            waited_ms: waited.as_millis() as u64,
        },
    })
}

/// trim 之后还剩东西的那一档；空串与全空白一律当没给。
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// 随机令牌：两个 v4 UUID 拼成 256 bit 的十六进制串。
///
/// 用 `uuid` 而不是自己搓熵：它的 v4 走 `getrandom`（操作系统 CSPRNG），
/// 而本 crate 原本没有随机数依赖，`uuid` 已经在工作区里。
fn new_token() -> String {
    format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

/// 可达项目 = 本项目 + 同分组项目；未分组只有本项目（ADR 0003）。
///
/// 找不到本项目时返回空 —— 调用方据此报「项目不可用」。顺序照抄宿主给的顺序
/// （桌面侧栏序），本项目**不**特意提到最前面。
pub fn reachable_projects(all: &[ControlProject], own_project_id: &str) -> Vec<ControlProject> {
    let Some(own) = all.iter().find(|p| p.id == own_project_id) else {
        return Vec::new();
    };
    match own.group_id.as_deref() {
        None => vec![own.clone()],
        Some(group) => all
            .iter()
            .filter(|p| p.group_id.as_deref() == Some(group))
            .cloned()
            .collect(),
    }
}

// ─── 线上形状 ─────────────────────────────────────────────────

/// 控制请求的 body。
///
/// 命令各取所需：`list-*` 只用前两个字段，`start-session` 另看
/// `launcherId` / `projectId`，工单 05~07 的 send / wait / read 看 `targetPaneId`。
/// 一个结构体装全部而不是每条命令一个类型，是为了让鉴权能在**分发之前**做掉
/// （未知命令也不该成为免鉴权的口子）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ControlRequest {
    #[serde(default)]
    token: String,
    /// 调用方自称的 pane 身份（来自 `MINITERM_ORCHESTRATOR_PANE`）。
    pane_id: u32,
    /// `start-session`：用哪个具名启动器。
    #[serde(default)]
    launcher_id: Option<String>,
    /// `start-session`：落在哪个项目；不给就是编排者自己那个。
    #[serde(default)]
    project_id: Option<String>,
    /// 以某个乐手为目标的命令（`send` / `wait`，工单 07 的 read 同款）用它。
    #[serde(default)]
    target_pane_id: Option<u32>,
    /// `wait`：最多等多久（毫秒）。
    ///
    /// 不给就是 [`WAIT_DEFAULT`]；超过 [`WAIT_MAX`] 按上界算（**钳而不拒**，
    /// 见 [`wait_patience`]）。`0` 是合法值，语义是「只看一眼就回来」。
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// `send`：要写穿进去的正文。
    ///
    /// ⚠️ **这是用户项目里的内容**（编排者拿自己的上下文拼出来的 prompt）——
    /// 与启动器的命令文本同一档待遇：不许进日志、不许进错误消息、不许出现在
    /// 任何回执里（ADR 0002 的防线延伸）。
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ControlData {
    Launchers { launchers: Vec<LauncherView> },
    Projects { projects: Vec<ProjectView> },
    /// `start-session` 的回执：刚起成的那一个。
    Pane { pane: PaneView },
    /// `list-panes`：自己名下的全部乐手（含已经关掉的）。
    Panes { panes: Vec<PaneView> },
    /// `send` 的回执。
    Sent { sent: SentView },
    /// `wait` 的回执。
    Waited { waited: WaitView },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LauncherView {
    id: String,
    name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectView {
    id: String,
    name: String,
    path: String,
    /// 编排者自己所在的那条。
    current: bool,
    /// 能不能在这里起乐手。SSH 远程项目是 `false`（令牌注不到远端去）。
    can_start_sessions: bool,
}

/// 一个乐手在编排者眼里的样子。`start-session` 的回执与 `list-panes` 的每一条
/// 都是它 —— 编排者只需要认识一种 pane 视图。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PaneView {
    /// 乐手的 pane 身份（= PTY 编号）。后续命令按它点名。
    pane_id: u32,
    project_id: String,
    project_name: String,
    launcher_id: String,
    launcher_name: String,
    /// AI 状态：`idle` / `ai-idle` / `ai-working`（与桌面徽章同一口径）。
    status: String,
    /// pane 还在桌面上吗。编排者退场不杀乐手，反过来乐手被用户关掉时这里就是
    /// `false` —— 记账留着，好让编排者看得见「我起的那个已经没了」。
    alive: bool,
}

/// `send` 的回执。
///
/// **刻意不复用 [`PaneView`]**：写穿之后那一瞬的 `status` 一定还是写之前的样子
/// （agent 还没来得及反应），把它摆在回执里会诱导编排者把「刚发完还是 ai-idle」
/// 读成「它干完了」。要看状态请走工单 06 的 `wait`。
///
/// ⚠️ 正文一个字都不回显 —— 那是用户项目里的内容（见 `ControlRequest::text`）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SentView {
    /// 写进了哪个乐手。
    pane_id: u32,
    /// 是不是当成一整块粘贴送进去的。
    ///
    /// 为 `false` 时目标终端此刻没开 bracketed paste（多半是那个 agent 已经退了、
    /// pane 退回裸 shell），多行正文是逐行进去的 —— 中途的换行很可能已经把它
    /// 提前发出去了。**如实告诉编排者**，让它自己决定要不要重来。
    bracketed_paste: bool,
}

/// `wait` 的回执。
///
/// **刻意不复用 [`PaneView`]**：那一份是「这个乐手是什么」（项目 / 启动器 /
/// 死活），而这一份是「这一次等待的结论」。把项目名启动器名再抄一遍只会让编排者
/// 在两种形状之间反复对齐；要那些走 `list-panes`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WaitView {
    /// 等的是哪个乐手。
    pane_id: u32,
    /// 结论：`ai-idle`（干完了）/ `attention`（等你处理）/ `idle`（agent 已退出），
    /// 或 `pending`（到耐心用尽还没收敛 —— **正常回执，不是错误**）。
    ///
    /// 前三个与 `status` **同一套词汇**，编排者不必认第二种拼法。
    outcome: &'static str,
    /// 收工那一刻的 AI 状态原文，与 `list-panes` 的状态列同一口径。
    ///
    /// 与 `outcome` **不重复**，两处场合非它不可：
    ///
    /// - `attention` 时它说明那个 pane 停在 `ai-idle`（Claude 的审批等待）还是
    ///   `ai-working`（Codex 的 —— 批准后直接执行工具，状态留在工作中）；
    /// - `pending` 时它是唯一能区分「真在跑」（`ai-working`）与「这个乐手看不透」
    ///   （`idle`：没 hook、输入检测也没认出来的自定义启动器）的东西。
    status: String,
    /// 成因原文（hook 事件名）。**照带不翻译** —— `Stop` 是真做完，`Interrupt`
    /// 是用户按了 Esc，`Stall` 是停摆兜底收敛的，`PermissionRequest` /
    /// `Elicitation` / `StopFailure` 是三种「等你处理」。
    ///
    /// 没有就整个字段不出线：无 hook 的降级路径上一律没有成因（那条路上只有
    /// 输出活跃度，没有事件）。
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<String>,
    /// 实际等了多久（毫秒）。
    ///
    /// 编排者据此认出两件事：自己给的超时被[钳到上界](wait_patience)了，
    /// 以及「几乎没等就答了 `ai-idle`」那一档 —— 那多半是上一回合的残留
    /// （见 [`ControlPlane::wait`] 结尾那条已知窄口子）。
    waited_ms: u64,
}

fn pane_view(session: &OrchestratedSession, liveness: &PaneLiveness) -> PaneView {
    PaneView {
        pane_id: session.pane_id,
        project_id: session.project_id.clone(),
        project_name: session.project_name.clone(),
        launcher_id: session.launcher_id.clone(),
        launcher_name: session.launcher_name.clone(),
        status: liveness.status.clone(),
        alive: liveness.alive,
    }
}

/// 一条控制请求的结论：HTTP 状态码 + JSON body。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlOutcome {
    pub status: u16,
    pub body: String,
}

/// 错误是**闭集**：CLI 按 code 分支，文案只给人看。
///
/// ⚠️ 消息一律是**给编排者读的英文短句**，且**一个字都不许带出启动器的命令文本
/// 或项目里的什么内容**（ADR 0002 的防线）。另外 [`Self::into_outcome`] 是手工
/// 拼的 JSON 字面量 —— 消息里别写引号和反斜杠。
///
/// 对外只出 [`Self::code`] 那个字符串（sidecar 侧按它分档），类型本身不出 crate。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlError {
    MissingToken,
    InvalidToken,
    BadRequest,
    UnknownCommand,
    ProjectUnavailable,
    PayloadTooLarge,
    // ── 工单 03 ──
    /// 启动器 id 不在名单里。
    LauncherNotFound,
    /// 目标项目不在可达范围内（组外 / 不存在，**刻意不区分**）。
    ProjectUnreachable,
    /// SSH 远程项目当不了乐手宿主。
    RemoteProjectUnsupported,
    /// 已达并发上限。编排者自己排队，不静默等待。
    SessionLimitReached,
    /// 桌面端起不来那个会话（终端没建成 / 命令没交到活着的 PTY 手上）。
    StartFailed,
    /// 桌面主线程没在时限内答复。**不等于「没起成」** —— 见 [`Self::message`]。
    DesktopBusy,
    /// 自指禁令：编排者不能驱动自己那个 pane。
    SelfTarget,
    /// 目标不是自己起的乐手（含压根不存在）—— 统一的「不存在」语义。
    PaneNotFound,
    /// 是自己起的乐手，但那个 pane 已经被关掉了。
    PaneGone,
    // ── 工单 05 ──
    /// `send` 给了空正文（或只有空白）。
    ///
    /// 单独一个码而不是并进 [`Self::BadRequest`]：它不是「请求拼错了」，
    /// 而是一条**裁决** —— 裸回车就是替用户按确认，ADR 0003 的「attention 不代答」
    /// 在这里堵住最顺手的那条代答姿势。给它自己的码，编排者才读得懂为什么被拒。
    EmptyInput,
    /// 找得到那个乐手，但正文没交到它的 PTY 手上。
    SendFailed,
}

impl ControlError {
    fn code(self) -> &'static str {
        match self {
            Self::MissingToken => "missingToken",
            Self::InvalidToken => "invalidToken",
            Self::BadRequest => "badRequest",
            Self::UnknownCommand => "unknownCommand",
            Self::ProjectUnavailable => "projectUnavailable",
            Self::PayloadTooLarge => "payloadTooLarge",
            Self::LauncherNotFound => "launcherNotFound",
            Self::ProjectUnreachable => "projectUnreachable",
            Self::RemoteProjectUnsupported => "remoteProjectUnsupported",
            Self::SessionLimitReached => "sessionLimitReached",
            Self::StartFailed => "startFailed",
            Self::DesktopBusy => "desktopBusy",
            Self::SelfTarget => "selfTarget",
            Self::PaneNotFound => "paneNotFound",
            Self::PaneGone => "paneGone",
            Self::EmptyInput => "emptyInput",
            Self::SendFailed => "sendFailed",
        }
    }

    fn status(self) -> u16 {
        match self {
            Self::MissingToken | Self::InvalidToken => 401,
            Self::BadRequest | Self::EmptyInput => 400,
            Self::UnknownCommand | Self::LauncherNotFound | Self::PaneNotFound => 404,
            Self::ProjectUnavailable | Self::RemoteProjectUnsupported => 409,
            Self::PayloadTooLarge => 413,
            Self::ProjectUnreachable | Self::SelfTarget => 403,
            Self::SessionLimitReached => 429,
            Self::StartFailed | Self::SendFailed => 500,
            Self::DesktopBusy => 503,
            Self::PaneGone => 410,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::MissingToken => "no orchestrator token in this pane",
            Self::InvalidToken => "orchestrator token rejected",
            Self::BadRequest => "malformed control request",
            Self::UnknownCommand => "unknown control command",
            Self::ProjectUnavailable => "orchestrator project is no longer available",
            Self::PayloadTooLarge => "control request body too large",
            Self::LauncherNotFound => "no such AI launcher",
            Self::ProjectUnreachable => {
                "project is not reachable from here (own project and same group only)"
            }
            Self::RemoteProjectUnsupported => {
                "SSH remote projects cannot host orchestrated sessions"
            }
            Self::SessionLimitReached => {
                "orchestrated session limit reached; wait for one of yours to finish"
            }
            Self::StartFailed => "the desktop could not start that session",
            // ⚠️ 这条**不是**「没起成」：记账在乐手落地那一刻就已经写进控制面
            // （见模块注释的「记账契约」），没答上来的只是这一趟回执。于是正确的
            // 下一步是先 list-panes 看一眼，而不是无脑重试再起一个。
            Self::DesktopBusy => {
                "the desktop did not answer in time; the session may have started anyway - \
                 run list-panes before retrying"
            }
            Self::SelfTarget => "an orchestrator cannot drive its own pane",
            Self::PaneNotFound => "no such orchestrated session",
            Self::PaneGone => "that orchestrated session's pane has been closed",
            // 「不代答」那条铁律的用户可见面：说清为什么，别让它以为是拼错了。
            Self::EmptyInput => {
                "send needs a non-empty prompt; a bare Enter would be answering on the user's \
                 behalf, which an orchestrator must not do"
            }
            Self::SendFailed => "the desktop could not deliver that input to the session",
        }
    }

    pub(crate) fn into_outcome(self) -> ControlOutcome {
        ControlOutcome {
            status: self.status(),
            body: format!(
                r#"{{"ok":false,"error":{{"code":"{}","message":"{}"}}}}"#,
                self.code(),
                self.message()
            ),
        }
    }
}

fn ok_outcome(data: &ControlData) -> ControlOutcome {
    // 手工拼壳:`{"ok":true,"data":<data>}`。data 侧走 serde,壳只有两个字面量键。
    let payload = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    ControlOutcome {
        status: 200,
        body: format!(r#"{{"ok":true,"data":{payload}}}"#),
    }
}

// ─── HTTP 落点 ────────────────────────────────────────────────

/// 控制路由：这是一条控制请求就地处理完（返回 `None`），否则把请求**原样交还**
/// 给调用方去走 hook 那条路（返回 `Some`）。
///
/// 交还而不是借用，是因为 `tiny_http::Request::respond` 吃 `self`。
///
/// 与 `/hook` 的另一处差别：那条为了不阻塞 hook 脚本先回 200 再处理，这条**必须**
/// 先处理再回响应 —— 调用方等的就是数据。
pub(crate) fn try_handle_control(
    mut request: tiny_http::Request,
    plane: &ControlPlane,
) -> Option<tiny_http::Request> {
    let url = request.url().to_string();
    let Some(command) = url.strip_prefix(CONTROL_PREFIX) else {
        return Some(request);
    };
    if request.method() != &tiny_http::Method::Post {
        respond(request, ControlError::BadRequest.into_outcome());
        return None;
    }
    // 与 hook 端点同款两道闸：先看声明的长度，再用 take() 兜住谎报/分块传输。
    if request
        .body_length()
        .is_some_and(|n| n > MAX_CONTROL_BODY_BYTES)
    {
        respond(request, ControlError::PayloadTooLarge.into_outcome());
        return None;
    }
    let mut body = String::new();
    let read = {
        use std::io::Read;
        request
            .as_reader()
            .take(MAX_CONTROL_BODY_BYTES as u64 + 1)
            .read_to_string(&mut body)
    };
    if read.is_err() {
        respond(request, ControlError::BadRequest.into_outcome());
        return None;
    }
    if body.len() > MAX_CONTROL_BODY_BYTES {
        respond(request, ControlError::PayloadTooLarge.into_outcome());
        return None;
    }
    // 会阻塞在桌面主线程上的命令：**不能占着这条 HTTP 线程等** —— 它同时也是
    // hook 上报的那条队（见模块注释）。鉴权先在本线程做掉挡住无令牌请求，
    // 已鉴权的活丢给一条一次性线程跑完再响应。
    //
    // 每条请求一个线程而不是线程池：控制命令是编排者手动调 CLI 触发的低频动作，
    // 一个池子的复杂度换不来什么；而带令牌的请求方本来就是我们自己起的进程。
    //
    // 认不出的命令名（带查询串 / 多层路径的也在内）当然不阻塞，落到下面那条路
    // 由 `handle` 统一答「未知命令」。
    if Command::parse(command).is_some_and(Command::needs_own_thread) {
        if let Err(err) = plane.authorize_body(&body) {
            respond(request, err.into_outcome());
            return None;
        }
        let plane = plane.clone();
        let command = command.to_string();
        std::thread::spawn(move || {
            let outcome = plane.handle(&command, &body);
            respond(request, outcome);
        });
        return None;
    }
    let outcome = plane.handle(command, &body);
    respond(request, outcome);
    None
}

fn respond(request: tiny_http::Request, outcome: ControlOutcome) {
    let response = tiny_http::Response::from_string(outcome.body)
        .with_status_code(outcome.status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        );
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::time::Duration;

    // ─── 假宿主 ───────────────────────────────────────────────

    /// 注入式假宿主。项目表放在 `Mutex` 里是为了**中途改分组**：
    /// 「改分组即时生效」这条只有在同一个 plane 上前后各请求一次才证得出来。
    #[derive(Default)]
    struct FakeHost {
        launchers: Mutex<Vec<ControlLauncher>>,
        projects: Mutex<Vec<ControlProject>>,
    }

    impl OrchestratorHost for Arc<FakeHost> {
        fn launchers(&self) -> Vec<ControlLauncher> {
            self.launchers.lock().clone()
        }
        fn projects(&self) -> Vec<ControlProject> {
            self.projects.lock().clone()
        }
    }

    fn launcher(id: &str, name: &str) -> ControlLauncher {
        ControlLauncher {
            id: id.into(),
            name: name.into(),
        }
    }

    fn project(id: &str, group: Option<&str>) -> ControlProject {
        ControlProject {
            id: id.into(),
            name: format!("项目{id}"),
            path: format!("D:\\repos\\{id}"),
            group_id: group.map(str::to_string),
            ssh_connection_id: None,
        }
    }

    /// 同一条项目，但是 SSH 远程的。
    fn remote_project(id: &str, group: Option<&str>) -> ControlProject {
        ControlProject {
            ssh_connection_id: Some("conn-1".into()),
            ..project(id, group)
        }
    }

    // ─── 假动作实现 ───────────────────────────────────────────

    /// 一次 `start_session` 收到的东西（spec 本身是一次性的落地凭据、不可克隆，
    /// 所以只把要断言的那几样抄出来）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SpecFacts {
        launcher_id: String,
        project_id: String,
        orchestrator_pane_id: u32,
    }

    /// 一次 `send_input` 收到的东西：写给谁、装配成了什么样。
    ///
    /// **两份都抄下来**：装配是控制面的产出，真桌面挑哪一份只是一次布尔判断 ——
    /// 把两份都钉住，等于把「写进 PTY 的那串字节」整个钉住。
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SendCall {
        pane_id: u32,
        bracketed: String,
        plain: String,
    }

    /// 桌面动作的假实现：**记下收到的 spec**（裁决对不对，看它收到了什么），
    /// 按需要伪造失败，并把「起出来的乐手」登记成一个可以随手改死活的现场。
    #[derive(Default)]
    struct FakeActions {
        /// 下一个乐手的 pane 编号（真桌面上是 PTY 编号，同样单调递增）。
        next_pane: Mutex<u32>,
        /// 非 `None` 时 `start_session` 一律这么失败。
        fail: Mutex<Option<StartFailure>>,
        /// 每次 `start_session` 收到的 spec 摘要，按顺序。
        calls: Mutex<Vec<SpecFacts>>,
        /// pane → 死活。没登记过的一律「已经不在了」。
        liveness: Mutex<HashMap<u32, PaneLiveness>>,
        /// 起会话时先睡这么久（测「阻塞命令不占 HTTP 线程」用）。
        delay: Mutex<Option<std::time::Duration>>,
        /// 每次 `send_input` 收到的东西，按顺序。
        sends: Mutex<Vec<SendCall>>,
        /// 非 `None` 时 `send_input` 一律这么失败。
        send_fail: Mutex<Option<SendFailure>>,
        /// 目标终端开着 bracketed paste 吗。
        /// `None` = 常态（乐手都是 AI TUI，一律开着）。
        bracketed_paste: Mutex<Option<bool>>,
    }

    impl FakeActions {
        fn set(&self, pane_id: u32, alive: bool, status: &str, ai_session: AiSessionState) {
            self.set_with_cause(pane_id, alive, status, ai_session, None);
        }
        /// 带成因的那一档（`wait` 的 attention / ai-idle 两档全靠它）。
        fn set_with_cause(
            &self,
            pane_id: u32,
            alive: bool,
            status: &str,
            ai_session: AiSessionState,
            cause: Option<&str>,
        ) {
            self.liveness.lock().insert(
                pane_id,
                PaneLiveness {
                    alive,
                    status: status.into(),
                    ai_session,
                    cause: cause.map(str::to_string),
                },
            );
        }
        fn live(&self, pane_id: u32, status: &str) {
            self.set(pane_id, true, status, AiSessionState::Active);
        }
        /// 一个回合干完了：hook 的 `Stop` 落地（**这才是真做完**）。
        fn finished(&self, pane_id: u32, cause: &str) {
            self.set_with_cause(pane_id, true, "ai-idle", AiSessionState::Active, Some(cause));
        }
        /// 停在等审批 / 向人提问：状态由调用方给 —— Claude 落 `ai-idle`，
        /// Codex 的 `PermissionRequest` 落 `ai-working`（真实差异，见
        /// `hook_server::map_event_to_status`）。
        fn attention(&self, pane_id: u32, status: &str, cause: &str) {
            self.set_with_cause(pane_id, true, status, AiSessionState::Active, Some(cause));
        }
        /// 乐手 pane 被用户关掉。
        fn close(&self, pane_id: u32) {
            self.liveness.lock().insert(pane_id, PaneLiveness::gone());
        }
        /// agent 自己退出了，但 pane 还在（还能回去看它留下的东西）。
        /// **hook 已启用**的那条路：`idle` 是权威的退出信号。
        fn agent_exited(&self, pane_id: u32) {
            self.set(pane_id, true, "idle", AiSessionState::Ended);
        }
        /// 降级路径：这个 pane 没 hook、输入检测也没认出它是 AI 会话 ——
        /// 桌面侧**答不上来**「它还在不在跑」。
        fn unknown(&self, pane_id: u32) {
            self.set(pane_id, true, "idle", AiSessionState::Unknown);
        }
    }

    impl OrchestratorActions for Arc<FakeActions> {
        fn start_session(&self, spec: StartSessionSpec) -> Result<StartedSession, StartFailure> {
            if let Some(d) = *self.delay.lock() {
                std::thread::sleep(d);
            }
            self.calls.lock().push(SpecFacts {
                launcher_id: spec.launcher_id().to_string(),
                project_id: spec.project_id().to_string(),
                orchestrator_pane_id: spec.orchestrator_pane_id(),
            });
            if let Some(err) = *self.fail.lock() {
                return Err(err);
            }
            let pane_id = {
                let mut next = self.next_pane.lock();
                *next += 1;
                100 + *next
            };
            // 刚起的乐手：命令已经敲进去了，输入检测那一刻就把它认成 AI 会话
            self.live(pane_id, "ai-idle");
            // 与真桌面同一条契约：先记账（`landed`），再谈回执。编排者的 tab
            // 标题由桌面侧查出来带进来，这里给一个固定的假名。
            Ok(spec.landed(pane_id, "大脑"))
        }

        /// 与真桌面同形：记下收到的装配结果，按目标终端的真实模式挑一份，
        /// 并**如实**回报挑了哪份。
        fn send_input(&self, pane_id: u32, input: PaneInput) -> Result<Delivered, SendFailure> {
            if let Some(d) = *self.delay.lock() {
                std::thread::sleep(d);
            }
            self.sends.lock().push(SendCall {
                pane_id,
                bracketed: input.bracketed().to_string(),
                plain: input.plain().to_string(),
            });
            if let Some(err) = *self.send_fail.lock() {
                return Err(err);
            }
            Ok(Delivered {
                bracketed_paste: self.bracketed_paste.lock().unwrap_or(true),
            })
        }

        fn pane_liveness(&self, pane_id: u32) -> PaneLiveness {
            self.liveness
                .lock()
                .get(&pane_id)
                .cloned()
                .unwrap_or_else(PaneLiveness::gone)
        }
    }

    // ─── HTTP 级脚手架 ────────────────────────────────────────

    /// 起一个**真的** tiny_http 服务，路由分发与生产路径是同一段代码
    /// （[`try_handle_control`]）。端口取 0 让内核挑，绝不去碰 23456 那几个
    /// —— 用户机器上很可能正跑着装机版。
    fn serve(plane: ControlPlane) -> u16 {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                if let Some(request) = try_handle_control(request, &plane) {
                    // 非控制路由：模拟 hook 循环的「其余一律 404」
                    let _ = request.respond(
                        tiny_http::Response::from_string("Not Found").with_status_code(404),
                    );
                }
            }
        });
        port
    }

    /// 裸 HTTP POST，返回 (状态码, body)。
    fn post(port: u16, path: &str, body: &str) -> (u16, String) {
        request_raw(port, "POST", path, Some(body))
    }

    fn request_raw(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let payload = body.unwrap_or("");
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap();
        (status, body.to_string())
    }

    fn json(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("响应必须是 JSON")
    }

    fn error_code(body: &str) -> String {
        json(body)["error"]["code"].as_str().unwrap().to_string()
    }

    /// 一套「已授予编排能力的编排者 pane」的现场。编排者是 pane 7，在 `p-self`。
    fn granted() -> (ControlPlane, Arc<FakeHost>, Arc<FakeActions>, u16, String) {
        let host = Arc::new(FakeHost::default());
        *host.launchers.lock() = vec![launcher("claude", "Claude"), launcher("codex", "Codex")];
        *host.projects.lock() = vec![project("p-self", None)];
        let actions = Arc::new(FakeActions::default());
        let plane = ControlPlane::new();
        plane.set_host(Arc::new(host.clone()));
        plane.set_actions(Arc::new(actions.clone()));
        let token = plane.grant(7, "p-self");
        let port = serve(plane.clone());
        (plane, host, actions, port, token)
    }

    /// 编排者 pane 7 的请求体。`extra` 是这条命令自己那几个字段的 JSON 片段
    /// （空串就是只带鉴权那两项）。
    fn payload_of(token: &str, extra: &str) -> String {
        if extra.is_empty() {
            format!(r#"{{"token":"{token}","paneId":7}}"#)
        } else {
            format!(r#"{{"token":"{token}","paneId":7,{extra}}}"#)
        }
    }

    /// 起一个乐手，返回它的 pane 编号。
    fn start(port: u16, token: &str, extra: &str) -> (u16, serde_json::Value) {
        let (status, body) = post(port, "/control/start-session", &payload_of(token, extra));
        (status, json(&body))
    }

    /// `send` 一段正文。正文经 serde 转义 —— prompt 里带引号 / 反斜杠 / 换行
    /// 是常态，手拼 JSON 字面量在这条命令上必错。
    fn send(port: u16, token: &str, target: u32, text: &str) -> (u16, serde_json::Value) {
        let text = serde_json::to_string(text).unwrap();
        let payload = payload_of(token, &format!(r#""targetPaneId":{target},"text":{text}"#));
        let (status, body) = post(port, "/control/send", &payload);
        (status, json(&body))
    }

    /// 起一个乐手并把编号交出来（`send` 那一组测试的开场白）。
    fn start_musician(port: u16, token: &str) -> u32 {
        let (status, v) = start(port, token, r#""launcherId":"claude""#);
        assert_eq!(status, 200, "{v}");
        v["data"]["pane"]["paneId"].as_u64().unwrap() as u32
    }

    // ─── 鉴权 fail-closed ─────────────────────────────────────

    /// 普通 pane 里跑 CLI：没有令牌 → 明确被拒（演示口径的另一半）。
    #[test]
    fn 无令牌一律被拒() {
        let (_plane, _host, _actions, port, _token) = granted();
        for cmd in ["list-launchers", "list-projects"] {
            let (status, body) = post(port, &format!("/control/{cmd}"), r#"{"paneId":7}"#);
            assert_eq!(status, 401, "{cmd}");
            assert_eq!(error_code(&body), "missingToken", "{cmd}");
            assert_eq!(json(&body)["ok"], false);
        }
    }

    /// 伪造 / 猜的令牌一律被拒，且**不泄露**任何数据。
    #[test]
    fn 坏令牌与伪造令牌一律被拒() {
        let (_plane, _host, _actions, port, token) = granted();
        // 改最后一位。**必须真的改掉** —— 原来固定换成 '0',令牌本来就以 '0'
        // 结尾时(十六进制,1/16 的概率)「伪造」的那枚与真令牌一模一样,
        // 这条测试就会随机变红。
        let last = token.chars().last().unwrap();
        let forged = format!("{}{}", &token[..token.len() - 1], if last == '0' { '1' } else { '0' });
        for bad in ["", "not-a-token", forged.as_str()] {
            let payload = format!(r#"{{"token":"{bad}","paneId":7}}"#);
            let (status, body) = post(port, "/control/list-launchers", &payload);
            assert_eq!(status, 401, "token={bad}");
            assert!(!body.contains("Claude"), "被拒的请求不许带出数据: {body}");
        }
    }

    /// 令牌被抄到别的 pane 去用：自称身份与令牌登记的 pane 对不上 → 拒。
    #[test]
    fn 身份与令牌对不上被拒() {
        let (_plane, _host, _actions, port, token) = granted();
        let payload = format!(r#"{{"token":"{token}","paneId":8}}"#);
        let (status, body) = post(port, "/control/list-launchers", &payload);
        assert_eq!(status, 401);
        assert_eq!(error_code(&body), "invalidToken");
    }

    /// pane 关掉之后令牌立刻作废（重开的 pane 是新身份，够不到前世的能力）。
    #[test]
    fn 撤销后令牌立即失效() {
        let (plane, _host, _actions, port, token) = granted();
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        assert_eq!(post(port, "/control/list-launchers", &payload).0, 200);

        plane.revoke_pane(7);
        let (status, body) = post(port, "/control/list-launchers", &payload);
        assert_eq!(status, 401);
        assert_eq!(error_code(&body), "invalidToken");
    }

    /// 同一 pane 再次授予会顶掉旧令牌（PTY 重开、SSH 重连）。
    #[test]
    fn 重复授予顶掉旧令牌() {
        let (plane, _host, _actions, port, old) = granted();
        let new = plane.grant(7, "p-self");
        assert_ne!(old, new);

        let old_payload = format!(r#"{{"token":"{old}","paneId":7}}"#);
        assert_eq!(post(port, "/control/list-launchers", &old_payload).0, 401);
        let new_payload = format!(r#"{{"token":"{new}","paneId":7}}"#);
        assert_eq!(post(port, "/control/list-launchers", &new_payload).0, 200);
    }

    /// 令牌不可预测：每次授予都是新的一枚，且长度够。
    #[test]
    fn 令牌每次都不同() {
        let plane = ControlPlane::new();
        let a = plane.grant(1, "p");
        let b = plane.grant(2, "p");
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "两个 v4 UUID 的十六进制 = 64 字符");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ─── list-launchers ───────────────────────────────────────

    /// 勾了「允许编排」的启动器起的 pane：令牌可用，拿得到启动器名单。
    #[test]
    fn 编排者_pane_能列出启动器() {
        let (_plane, _host, _actions, port, token) = granted();
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        let (status, body) = post(port, "/control/list-launchers", &payload);
        assert_eq!(status, 200);
        let v = json(&body);
        assert_eq!(v["ok"], true);
        let list = v["data"]["launchers"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["id"], "claude");
        assert_eq!(list[0]["name"], "Claude");
        // ADR 0002 的边界：命令文本一个字都不给编排者看
        assert!(!body.contains("command"), "启动器命令不得出现在响应里: {body}");
        assert!(!body.contains("shell"), "启动器 shell 不得出现在响应里: {body}");
    }

    // ─── list-projects 的可达范围 ─────────────────────────────

    /// 未分组项目：只有本项目。
    #[test]
    fn 未分组项目只能看到自己() {
        let (_plane, host, _actions, port, token) = granted();
        *host.projects.lock() = vec![
            project("p-self", None),
            project("p-other", None),
            project("p-grouped", Some("g1")),
        ];
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        let (status, body) = post(port, "/control/list-projects", &payload);
        assert_eq!(status, 200);
        let list = json(&body)["data"]["projects"].clone();
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], "p-self");
        assert_eq!(list[0]["current"], true);
    }

    /// 同分组项目可达，组外项目一概不可见。
    #[test]
    fn 同分组项目可达而组外不可见() {
        let (_plane, host, _actions, port, token) = granted();
        *host.projects.lock() = vec![
            project("p-self", Some("g1")),
            project("p-sibling", Some("g1")),
            project("p-outsider", Some("g2")),
            project("p-loose", None),
        ];
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        let (_status, body) = post(port, "/control/list-projects", &payload);
        let ids: Vec<String> = json(&body)["data"]["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["p-self", "p-sibling"]);
        assert!(!body.contains("p-outsider"), "组外项目泄露: {body}");
        assert!(!body.contains("p-loose"), "未分组项目泄露: {body}");
    }

    /// **改分组即时生效**：同一个 plane、同一枚令牌，前后两次请求结论不同 ——
    /// 可达范围是每次请求现查的，不是授予那一刻算死的。
    #[test]
    fn 改分组即时生效() {
        let (_plane, host, _actions, port, token) = granted();
        *host.projects.lock() = vec![project("p-self", None), project("p-friend", Some("g1"))];
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);

        let (_s, before) = post(port, "/control/list-projects", &payload);
        assert_eq!(
            json(&before)["data"]["projects"].as_array().unwrap().len(),
            1
        );

        // 用户把两个项目拖进同一分组
        *host.projects.lock() = vec![
            project("p-self", Some("g1")),
            project("p-friend", Some("g1")),
        ];
        let (_s, after) = post(port, "/control/list-projects", &payload);
        let ids: Vec<String> = json(&after)["data"]["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["p-self", "p-friend"], "改分组必须即时生效");
    }

    /// 编排者所在的项目被删掉：给明确错误，而不是一个空列表。
    #[test]
    fn 项目没了给明确错误() {
        let (_plane, host, _actions, port, token) = granted();
        *host.projects.lock() = vec![project("p-other", None)];
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        let (status, body) = post(port, "/control/list-projects", &payload);
        assert_eq!(status, 409);
        assert_eq!(error_code(&body), "projectUnavailable");
    }

    /// 宿主没接线（Noop）时不许把「反正没配置」当成放行的理由。
    #[test]
    fn 未接线宿主不放行也不崩() {
        let plane = ControlPlane::new();
        let token = plane.grant(1, "p-self");
        let port = serve(plane.clone());
        let payload = format!(r#"{{"token":"{token}","paneId":1}}"#);

        let (status, body) = post(port, "/control/list-launchers", &payload);
        assert_eq!(status, 200);
        assert!(
            json(&body)["data"]["launchers"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let (status, _body) = post(port, "/control/list-projects", &payload);
        assert_eq!(status, 409, "项目表空 = 本项目不可达");
    }

    // ─── 协议边界 ─────────────────────────────────────────────

    #[test]
    fn 未知命令与坏_json_有各自的语义() {
        let (_plane, _host, _actions, port, token) = granted();
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);

        let (status, body) = post(port, "/control/list-everything", &payload);
        assert_eq!(status, 404);
        assert_eq!(error_code(&body), "unknownCommand");

        let (status, body) = post(port, "/control/list-launchers", "not json");
        assert_eq!(status, 400);
        assert_eq!(error_code(&body), "badRequest");

        // 鉴权在命令分发之前：未知命令也不该成为免鉴权的口子
        let (status, body) = post(port, "/control/list-everything", r#"{"paneId":7}"#);
        assert_eq!(status, 401);
        assert_eq!(error_code(&body), "missingToken");
    }

    #[test]
    fn 非_post_与超大_body_被拒() {
        let (_plane, _host, _actions, port, token) = granted();
        let (status, _body) = request_raw(port, "GET", "/control/list-launchers", None);
        assert_eq!(status, 400);

        let huge = format!(
            r#"{{"token":"{token}","paneId":7,"pad":"{}"}}"#,
            "x".repeat(MAX_CONTROL_BODY_BYTES)
        );
        let (status, body) = post(port, "/control/list-launchers", &huge);
        assert_eq!(status, 413);
        assert_eq!(error_code(&body), "payloadTooLarge");
    }

    /// 控制路由不许吃掉 hook 那条路（`/hook` 一个字都不能动）。
    #[test]
    fn 非控制路由原样交还() {
        let (_plane, _host, _actions, port, _token) = granted();
        let (status, _body) = post(port, "/hook", "{}");
        assert_eq!(status, 404, "本测试服务只接控制路由,交还的请求应走到 404");
    }

    // ─── 纯裁决 ───────────────────────────────────────────────

    #[test]
    fn 可达范围的纯函数口径() {
        let all = vec![
            project("a", Some("g1")),
            project("b", Some("g1")),
            project("c", None),
        ];
        assert_eq!(
            reachable_projects(&all, "a")
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(
            reachable_projects(&all, "c")
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c"]
        );
        assert!(reachable_projects(&all, "missing").is_empty());
    }

    #[test]
    fn 是不是编排者可查() {
        let plane = ControlPlane::new();
        assert!(!plane.is_orchestrator(3));
        plane.grant(3, "p");
        assert!(plane.is_orchestrator(3));
        plane.revoke_pane(3);
        assert!(!plane.is_orchestrator(3));
    }

    // ─── start-session：起乐手 ────────────────────────────────

    /// 本项目起乐手：拿到回执，桌面侧收到的是「按 id 引用的启动器 + 落地项目」。
    #[test]
    fn 能在本项目起乐手并拿到回执() {
        let (_plane, _host, actions, port, token) = granted();
        let (status, v) = start(port, &token, r#""launcherId":"codex""#);
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["ok"], true);
        let pane = &v["data"]["pane"];
        assert_eq!(pane["paneId"], 101);
        assert_eq!(pane["projectId"], "p-self", "不给 projectId 就落在本项目");
        assert_eq!(pane["launcherId"], "codex");
        assert_eq!(pane["launcherName"], "Codex");
        assert_eq!(pane["status"], "ai-idle");
        assert_eq!(pane["alive"], true);

        let calls = actions.calls.lock().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].launcher_id, "codex");
        assert_eq!(calls[0].project_id, "p-self");
        assert_eq!(calls[0].orchestrator_pane_id, 7, "出身要带上，诞生提示要用");
    }

    /// 同分组的另一个项目也可以起（ADR 0003 的跨项目范围）。
    #[test]
    fn 能在同分组项目起乐手() {
        let (_plane, host, actions, port, token) = granted();
        *host.projects.lock() = vec![project("p-self", Some("g1")), project("p-api", Some("g1"))];

        let (status, v) = start(port, &token, r#""launcherId":"claude","projectId":"p-api""#);
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["data"]["pane"]["projectId"], "p-api");
        assert_eq!(actions.calls.lock()[0].project_id, "p-api");
    }

    /// 组外项目与压根不存在的项目**同一个错误** —— 区分开来就是一台项目扫描器。
    #[test]
    fn 组外项目与不存在的项目同一个错误() {
        let (_plane, host, actions, port, token) = granted();
        *host.projects.lock() = vec![
            project("p-self", Some("g1")),
            project("p-outsider", Some("g2")),
        ];

        for target in ["p-outsider", "p-never-existed"] {
            let extra = format!(r#""launcherId":"claude","projectId":"{target}""#);
            let (status, v) = start(port, &token, &extra);
            assert_eq!(status, 403, "{target}: {v}");
            assert_eq!(v["error"]["code"], "projectUnreachable", "{target}");
            let body = v.to_string();
            assert!(!body.contains("项目p-outsider"), "组外项目名泄露: {body}");
        }
        assert!(actions.calls.lock().is_empty(), "被拒的请求不许惊动桌面端");
    }

    /// 启动器不存在有自己的错误码（与「项目不可达」分得开）。
    #[test]
    fn 启动器不存在给专门的错误() {
        let (_plane, _host, actions, port, token) = granted();
        let (status, v) = start(port, &token, r#""launcherId":"grok""#);
        assert_eq!(status, 404, "{v}");
        assert_eq!(v["error"]["code"], "launcherNotFound");
        assert!(actions.calls.lock().is_empty());

        // 连 launcherId 都没给：这是请求本身不对，不是「找不到」
        let (status, v) = start(port, &token, "");
        assert_eq!(status, 400, "{v}");
        assert_eq!(v["error"]["code"], "badRequest");
    }

    /// SSH 远程项目当不了乐手宿主：令牌只会注进本地 ssh 客户端进程。
    #[test]
    fn 远程项目不能当乐手宿主() {
        let (_plane, host, actions, port, token) = granted();
        *host.projects.lock() = vec![
            project("p-self", Some("g1")),
            remote_project("p-remote", Some("g1")),
        ];

        let (status, v) = start(port, &token, r#""launcherId":"claude","projectId":"p-remote""#);
        assert_eq!(status, 409, "{v}");
        assert_eq!(v["error"]["code"], "remoteProjectUnsupported");
        assert!(actions.calls.lock().is_empty(), "别让桌面端白起一个 pane");

        // 可达列表里照列，但先写明起不了 —— 省得编排者试一次才知道
        let (_s, body) = post(port, "/control/list-projects", &payload_of(&token, ""));
        let list = json(&body)["data"]["projects"].clone();
        let list = list.as_array().unwrap();
        assert_eq!(list[0]["id"], "p-self");
        assert_eq!(list[0]["canStartSessions"], true);
        assert_eq!(list[1]["id"], "p-remote");
        assert_eq!(list[1]["canStartSessions"], false);
    }

    /// 起会话的响应与 spec 里一个字都不带命令文本（ADR 0002 的防线）。
    #[test]
    fn 回执与落地请求都不带命令文本() {
        let (_plane, host, actions, port, token) = granted();
        *host.launchers.lock() = vec![launcher("l1", "Claude")];
        let (_status, v) = start(port, &token, r#""launcherId":"l1""#);
        let body = v.to_string();
        assert!(!body.contains("command"), "命令字段不该出现: {body}");
        assert!(!body.contains("shell"), "shell 字段不该出现: {body}");
        // 落地请求的类型上就只有 id —— 命令由桌面侧自己按 id 去配置里取
        let spec = actions.calls.lock()[0].clone();
        assert_eq!(spec.launcher_id, "l1");
    }

    /// **禁套娃**：起出来的乐手不持有编排令牌，它自己跑 CLI 一律被拒。
    #[test]
    fn 乐手不持有编排令牌() {
        let (plane, _host, _actions, port, token) = granted();
        let (_status, v) = start(port, &token, r#""launcherId":"claude""#);
        let musician = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        assert!(
            !plane.is_orchestrator(musician),
            "受编排会话一律不授予编排能力"
        );
        // 乐手 pane 里跑 CLI：它连令牌都没有，第一道闸就挡住了
        let payload = format!(r#"{{"token":"{token}","paneId":{musician}}}"#);
        let (status, body) = post(port, "/control/list-launchers", &payload);
        assert_eq!(status, 401, "抄编排者的令牌也不行");
        assert_eq!(error_code(&body), "invalidToken");
    }

    // ─── 并发上限 ─────────────────────────────────────────────

    /// 第 6 个存活乐手被明确拒绝（**不静默排队**）；退出一个就还回一个名额。
    #[test]
    fn 第六个乐手被拒而退出即释放名额() {
        let (plane, _host, actions, port, token) = granted();
        assert_eq!(plane.session_cap(), DEFAULT_SESSION_CAP);

        let mut panes = Vec::new();
        for i in 0..DEFAULT_SESSION_CAP {
            let (status, v) = start(port, &token, r#""launcherId":"claude""#);
            assert_eq!(status, 200, "第 {} 个应当起得来: {v}", i + 1);
            panes.push(v["data"]["pane"]["paneId"].as_u64().unwrap() as u32);
        }

        let (status, v) = start(port, &token, r#""launcherId":"claude""#);
        assert_eq!(status, 429, "{v}");
        assert_eq!(v["error"]["code"], "sessionLimitReached");

        // ① agent 自己退出（pane 还在，能回去看它留下的东西）→ 名额还回来
        actions.agent_exited(panes[0]);
        let (status, v) = start(port, &token, r#""launcherId":"claude""#);
        assert_eq!(status, 200, "AI 会话退出即释放名额: {v}");
        let extra = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        // 又满了
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 429);

        // ② 乐手 pane 被用户关掉 → 同样还回来
        actions.close(panes[1]);
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 200);
        assert!(extra > 100);
    }

    /// **「不可知」按占名额算**（fail-closed）。
    ///
    /// 无 hook + 命令不在 `AI_COMMANDS` 里的自定义启动器（ADR 0003：任何启动器
    /// 都能当乐手）恒答 `idle`，若按「不在 AI 会话里 = 不占名额」判，硬上限就是
    /// 摆设 —— 可以无限起。宁可少起一个。
    #[test]
    fn 说不上来的乐手照样占着名额() {
        let (plane, _host, actions, port, token) = granted();
        plane.set_session_cap(2);

        let first = start(port, &token, r#""launcherId":"claude""#).1["data"]["pane"]["paneId"]
            .as_u64()
            .unwrap() as u32;
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 200);
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 429, "满了");

        // 这个 pane 没有 hook、输入检测也没认出它 —— 桌面侧答不上来
        actions.unknown(first);
        let (status, v) = start(port, &token, r#""launcherId":"claude""#);
        assert_eq!(status, 429, "「说不上来」不许释放名额,否则上限形同虚设: {v}");
        assert_eq!(v["error"]["code"], "sessionLimitReached");

        // 只有**明确结束**才还名额：hook 权威说 idle（SessionEnd 落地了）
        actions.agent_exited(first);
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 200);
    }

    /// 上限是**可注入**的（工单 08 的设置项落点），判断处不必改一个字。
    #[test]
    fn 上限可注入() {
        let (plane, _host, _actions, port, token) = granted();
        plane.set_session_cap(1);
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 200);
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 429);

        // 0 = 不许起任何乐手（设置项拉到底的语义）
        plane.set_session_cap(0);
        let plane2 = ControlPlane::new();
        plane2.set_session_cap(0);
        assert_eq!(plane2.session_cap(), 0);
    }

    /// **调低上限不许杀已存活的乐手**（工单 08 的验收项，也是一根防退化的钉子）。
    ///
    /// 上限只在 `start-session` 那一行被读一次，所以「调低不杀」是当前实现的
    /// 天然结论 —— 正因为是天然的，才更容易被后来人一条「超限就回收」的
    /// 顺手逻辑破掉。乐手 pane 里可能躺着改到一半的代码（ADR 0003 否决
    /// 「编排者退出时自动关闭其受编排会话」用的是同一条理由）。
    #[test]
    fn 调低上限不动已存活的乐手() {
        let (plane, _host, actions, port, token) = granted();
        let mut panes = Vec::new();
        for _ in 0..3 {
            let (status, v) = start(port, &token, r#""launcherId":"claude""#);
            assert_eq!(status, 200);
            panes.push(v["data"]["pane"]["paneId"].as_u64().unwrap() as u32);
        }

        // 用户把上限从 5 拉到 1 —— 已经在跑的三个一个都不许动
        plane.set_session_cap(1);

        let (status, body) = post(port, "/control/list-panes", &payload_of(&token, ""));
        assert_eq!(status, 200);
        let listed = json(&body)["data"]["panes"].clone();
        let listed = listed.as_array().unwrap();
        assert_eq!(listed.len(), 3, "调低上限不该让谁从名册上消失: {body}");
        for pane in listed {
            assert_eq!(pane["alive"], true, "已存活的乐手不许被回收: {pane}");
        }
        // 桌面侧的死活现场也一个字没改（控制面压根没有「关 pane」这个能力，
        // 真要加「超限就回收」得先给 `OrchestratorActions` 开一道口子）
        for pane in &panes {
            assert!(
                actions.liveness.lock()[pane].alive,
                "pane {pane} 被谁关掉了"
            );
        }

        // 只影响**后续**裁决：现在超着限，新的一律拒
        let (status, v) = start(port, &token, r#""launcherId":"claude""#);
        assert_eq!(status, 429, "{v}");
        assert_eq!(v["error"]["code"], "sessionLimitReached");

        // 名额得等真的降到新上限**之下**才长出来：关掉两个还剩一个，仍然满
        actions.close(panes[0]);
        actions.close(panes[1]);
        assert_eq!(
            start(port, &token, r#""launcherId":"claude""#).0,
            429,
            "还剩一个活着,上限 1 就是满的"
        );
        actions.close(panes[2]);
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 200);
    }

    /// 名额只计**自己**的乐手：别人起满了不该拖累我。
    #[test]
    fn 名额按编排者各算各的() {
        let (plane, _host, _actions, port, token) = granted();
        plane.set_session_cap(1);
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 200);
        assert_eq!(start(port, &token, r#""launcherId":"claude""#).0, 429);

        // 另一个编排者（pane 9）名额是空的
        let other = plane.grant(9, "p-self");
        let payload = format!(r#"{{"token":"{other}","paneId":9,"launcherId":"claude"}}"#);
        let (status, body) = post(port, "/control/start-session", &payload);
        assert_eq!(status, 200, "{body}");
    }

    // ─── list-panes：可见范围 ─────────────────────────────────

    /// 只看得见自己起的，且顺序稳定（按 pane 编号 = 起的先后）。
    #[test]
    fn list_panes_只列自己的乐手() {
        let (plane, _host, actions, port, token) = granted();
        let mine: Vec<u32> = (0..2)
            .map(|_| {
                start(port, &token, r#""launcherId":"codex""#).1["data"]["pane"]["paneId"]
                    .as_u64()
                    .unwrap() as u32
            })
            .collect();

        // 另一个编排者也起了一个
        let other = plane.grant(9, "p-self");
        let payload = format!(r#"{{"token":"{other}","paneId":9,"launcherId":"claude"}}"#);
        let (_s, body) = post(port, "/control/start-session", &payload);
        let theirs = json(&body)["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        let (status, body) = post(port, "/control/list-panes", &payload_of(&token, ""));
        assert_eq!(status, 200);
        let ids: Vec<u32> = json(&body)["data"]["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["paneId"].as_u64().unwrap() as u32)
            .collect();
        assert_eq!(ids, mine, "别人的乐手一条都不该露面");
        assert!(!ids.contains(&theirs));

        // 状态是现查的：agent 干起活来，列表立刻跟上
        actions.live(mine[0], "ai-working");
        let (_s, body) = post(port, "/control/list-panes", &payload_of(&token, ""));
        let panes = json(&body)["data"]["panes"].clone();
        assert_eq!(panes[0]["status"], "ai-working");
        assert_eq!(panes[1]["status"], "ai-idle");
    }

    /// 乐手被关掉之后**照列**，只是 `alive` 变假 —— 编排者要看得见「我起的那个
    /// 已经没了」，而不是它凭空消失。
    #[test]
    fn 关掉的乐手照列但标为不在() {
        let (_plane, _host, actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let pane = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        actions.close(pane);
        let (_s, body) = post(port, "/control/list-panes", &payload_of(&token, ""));
        let panes = json(&body)["data"]["panes"].clone();
        assert_eq!(panes.as_array().unwrap().len(), 1);
        assert_eq!(panes[0]["alive"], false);
        assert_eq!(panes[0]["status"], "idle");
    }

    /// 起会话失败**不留记账**：没起成的东西不该占着名额也不该出现在列表里。
    #[test]
    fn 起失败不留记账() {
        let (_plane, _host, actions, port, token) = granted();
        for (failure, status, code) in [
            (StartFailure::SpawnFailed, 500, "startFailed"),
            (StartFailure::DesktopBusy, 503, "desktopBusy"),
            (StartFailure::ProjectGone, 403, "projectUnreachable"),
        ] {
            *actions.fail.lock() = Some(failure);
            let (got, v) = start(port, &token, r#""launcherId":"claude""#);
            assert_eq!(got, status, "{failure:?}: {v}");
            assert_eq!(v["error"]["code"], code, "{failure:?}");
        }
        *actions.fail.lock() = None;

        let (_s, body) = post(port, "/control/list-panes", &payload_of(&token, ""));
        assert!(
            json(&body)["data"]["panes"].as_array().unwrap().is_empty(),
            "失败的尝试不该留下记账: {body}"
        );
    }

    /// **幽灵乐手那条竞态的钉子**：记账在 [`StartSessionSpec::landed`] 里落地，
    /// 回执之后再也不写第二次 —— 于是发起侧超时走人、回执丢在半路，也不会在桌面上
    /// 留下一个「不进 `list-panes`、不占名额、点名答不存在」的真实会话。
    #[test]
    fn 回执丢了记账照样在() {
        /// 起成之后把回执丢掉、只答「桌面忙」的动作实现 —— 与真现场
        /// 「主线程建成了 pane，但 HTTP 线程已经在 ACTION_TIMEOUT 上放弃」同构。
        struct DropsReply;

        impl OrchestratorActions for DropsReply {
            fn start_session(&self, spec: StartSessionSpec) -> Result<StartedSession, StartFailure> {
                let started = spec.landed(202, "大脑");
                drop(started); // 回执没送到
                Err(StartFailure::DesktopBusy)
            }
            fn send_input(&self, _pane_id: u32, _input: PaneInput) -> Result<Delivered, SendFailure> {
                Ok(Delivered {
                    bracketed_paste: true,
                })
            }
            fn pane_liveness(&self, pane_id: u32) -> PaneLiveness {
                PaneLiveness {
                    alive: pane_id == 202,
                    status: "ai-idle".into(),
                    ai_session: AiSessionState::Active,
                    cause: None,
                }
            }
        }

        let host = Arc::new(FakeHost::default());
        *host.launchers.lock() = vec![launcher("claude", "Claude")];
        *host.projects.lock() = vec![project("p-self", None)];
        let plane = ControlPlane::new();
        plane.set_host(Arc::new(host));
        plane.set_actions(Arc::new(DropsReply));
        let token = plane.grant(7, "p-self");
        let port = serve(plane.clone());

        // 编排者这一趟拿到的是「桌面没答上来」
        let (status, v) = start(port, &token, r#""launcherId":"claude""#);
        assert_eq!(status, 503, "{v}");
        assert_eq!(v["error"]["code"], "desktopBusy");
        // 而那条消息要指向 list-panes，不许诱导它无脑重试
        let message = v["error"]["message"].as_str().unwrap();
        assert!(message.contains("list-panes"), "文案得指路: {message}");

        // …但桌面上那个真实存在的乐手照样看得见
        let (_s, body) = post(port, "/control/list-panes", &payload_of(&token, ""));
        let panes = json(&body)["data"]["panes"].clone();
        assert_eq!(panes.as_array().unwrap().len(), 1, "别把它变成幽灵: {body}");
        assert_eq!(panes[0]["paneId"], 202);
        assert_eq!(panes[0]["alive"], true);

        // …照样占着名额（ADR 0003：上限只计活着的 AI 会话，它就是一个）
        plane.set_session_cap(1);
        let (status, v) = start(port, &token, r#""launcherId":"claude""#);
        assert_eq!(status, 429, "幽灵乐手不占名额就是在鼓励重试再起一个: {v}");

        // …照样点得动（可见范围铁律：它是自己起的，不该答「不存在」）
        let me = Grant {
            pane_id: 7,
            project_id: "p-self".into(),
        };
        assert_eq!(plane.resolve_target(&me, 202).unwrap().pane_id, 202);
    }

    /// 动作没接线时**一律拒绝**起会话（fail-closed，别把「没配置」当放行理由）。
    #[test]
    fn 未接线的动作实现拒绝起会话() {
        let host = Arc::new(FakeHost::default());
        *host.launchers.lock() = vec![launcher("claude", "Claude")];
        *host.projects.lock() = vec![project("p-self", None)];
        let plane = ControlPlane::new();
        plane.set_host(Arc::new(host));
        let token = plane.grant(7, "p-self");
        let port = serve(plane);

        let (status, v) = start(port, &token, r#""launcherId":"claude""#);
        assert_eq!(status, 503, "{v}");
        assert_eq!(v["error"]["code"], "desktopBusy");
    }

    // ─── 目标解析：可见范围铁律 ───────────────────────────────

    /// 三条语义各自明确：自指 / 他人（含不存在）/ 已关。
    ///
    /// 这个函数是 **05/06/07 的 send / wait / read 共用的那一条**，所以直接钉它，
    /// 不等命令落地。
    #[test]
    fn 目标解析的三条语义() {
        let (plane, _host, actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let mine = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        // 另一个编排者的乐手
        let other_token = plane.grant(9, "p-self");
        let payload = format!(r#"{{"token":"{other_token}","paneId":9,"launcherId":"claude"}}"#);
        let (_s, body) = post(port, "/control/start-session", &payload);
        let theirs = json(&body)["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        let me = Grant {
            pane_id: 7,
            project_id: "p-self".into(),
        };

        // 自己的乐手：解析得到，带着出身信息
        let hit = plane.resolve_target(&me, mine).expect("自己起的该找得到");
        assert_eq!(hit.pane_id, mine);
        assert_eq!(hit.orchestrator_pane_id, 7);
        assert_eq!(hit.launcher_name, "Claude");

        // 自指禁令
        assert_eq!(plane.resolve_target(&me, 7), Err(ControlError::SelfTarget));

        // 别人的乐手、用户亲手开的会话、编造的编号 —— **同一个**「不存在」
        for target in [theirs, 9, 4242] {
            assert_eq!(
                plane.resolve_target(&me, target),
                Err(ControlError::PaneNotFound),
                "target={target} 必须与「不存在」不可区分"
            );
        }

        // 自己的乐手但 pane 已经关了：这条可以说
        actions.close(mine);
        assert_eq!(plane.resolve_target(&me, mine), Err(ControlError::PaneGone));

        // agent 退出但 pane 还在 ≠ 已关：还能回去读它留下的东西（工单 07）
        actions.agent_exited(mine);
        assert!(plane.resolve_target(&me, mine).is_ok());
    }

    /// 编排者 pane 一撤销令牌，名下记账就**够不到**了（**不杀乐手**）——
    /// pane 重开是新身份，够不到前世起的会话。
    #[test]
    fn 编排者撤销后记账够不到但乐手照活() {
        let (plane, _host, actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let musician = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        plane.revoke_pane(7);
        assert!(
            actions.pane_liveness(musician).alive,
            "编排者退场不许连坐乐手"
        );

        // 同一个 pane 重开 PTY = 新身份，列表是空的
        let reborn = plane.grant(7, "p-self");
        let (status, body) = post(port, "/control/list-panes", &payload_of(&reborn, ""));
        assert_eq!(status, 200);
        assert!(
            json(&body)["data"]["panes"].as_array().unwrap().is_empty(),
            "MVP 不做收养: {body}"
        );

        let me = Grant {
            pane_id: 7,
            project_id: "p-self".into(),
        };
        assert_eq!(
            plane.resolve_target(&me, musician),
            Err(ControlError::PaneNotFound),
            "前世的乐手对新身份而言就是不存在"
        );
    }

    /// 乐手自己的 pane 关闭时也会走 `revoke_pane`（那是每个 pane 都调的路径），
    /// 那一次**不许**把它从记账里抹掉 —— 抹掉了「已关」就退化成「不存在」。
    #[test]
    fn 乐手关闭不抹掉自己的记账() {
        let (plane, _host, actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let musician = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        actions.close(musician);
        plane.revoke_pane(musician);

        let me = Grant {
            pane_id: 7,
            project_id: "p-self".into(),
        };
        assert_eq!(plane.resolve_target(&me, musician), Err(ControlError::PaneGone));
    }

    // ─── 「编排者已离场」（工单 04）─────────────────────────────

    /// 出身**留着、标识降级**：编排者退场后 `origins` 里那一条还在，
    /// 只是 `orchestrator_departed` 置了位 —— tab 上那枚标识据此换文案。
    ///
    /// 这是工单 04 的核心：工单 03 那一版在这里把记账整体删了，出身信息随之
    /// 消失，「编排者已离场」就无从显示。
    #[test]
    fn 编排者离场后出身留着并标记已离场() {
        let (plane, _host, actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let musician = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        let before = plane.origins();
        let origin = before.get(&musician).expect("刚起的乐手就该有出身");
        assert_eq!(origin.orchestrator_label, "大脑", "编排者是谁得记下来");
        assert!(!origin.orchestrator_departed);

        plane.revoke_pane(7);

        assert!(
            actions.pane_liveness(musician).alive,
            "编排者退场不许连坐乐手"
        );
        let after = plane.origins();
        let origin = after.get(&musician).expect("出身不许随编排者一起消失");
        assert!(origin.orchestrator_departed, "标识该降级成「编排者已离场」");
        assert_eq!(
            origin.orchestrator_label, "大脑",
            "离场之后更要说得出是谁 —— 那个 pane 已经没了，现查查不到"
        );
    }

    /// **已离场编排者的前世乐手不许被任何人驱动**。
    ///
    /// 三条路各堵一次：原编排者的令牌已撤（认证不成）、同一编号上重新授予的
    /// 新身份（MVP 不做收养）、别的编排者（可见范围铁律）。
    #[test]
    fn 已离场编排者的乐手谁也够不到() {
        let (plane, _host, _actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let musician = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        plane.revoke_pane(7);

        // ① 原令牌当场失效 —— 没人能再以 pane 7 的身份说话
        let (status, _body) = post(port, "/control/list-panes", &payload_of(&token, ""));
        assert_eq!(status, 401, "撤销后原令牌必须立刻不认");

        // ② 同一编号上重新授予 = 新身份，前世的乐手对它就是「不存在」
        let reborn = plane.grant(7, "p-self");
        let (status, body) = post(port, "/control/list-panes", &payload_of(&reborn, ""));
        assert_eq!(status, 200);
        assert!(
            json(&body)["data"]["panes"].as_array().unwrap().is_empty(),
            "MVP 不做收养: {body}"
        );
        let me = Grant {
            pane_id: 7,
            project_id: "p-self".into(),
        };
        assert_eq!(
            plane.resolve_target(&me, musician),
            Err(ControlError::PaneNotFound)
        );

        // ③ 别的编排者一样够不到
        let other = Grant {
            pane_id: 9,
            project_id: "p-self".into(),
        };
        assert_eq!(
            plane.resolve_target(&other, musician),
            Err(ControlError::PaneNotFound)
        );

        // 而它本人照常活着 —— 用户随时可以亲手接管
        assert!(plane.origins().contains_key(&musician));
    }

    /// **已离场的记账不占任何人的名额**：重开的编排者拿到的是满额，
    /// 别的编排者也不受牵连。
    #[test]
    fn 已离场的记账不占名额() {
        let (plane, _host, _actions, port, token) = granted();
        for i in 0..DEFAULT_SESSION_CAP {
            let (status, v) = start(port, &token, r#""launcherId":"claude""#);
            assert_eq!(status, 200, "第 {i} 个该起得来: {v}");
        }
        let (status, _v) = start(port, &token, r#""launcherId":"claude""#);
        assert_eq!(status, 429, "第 6 个撞上限");

        plane.revoke_pane(7);
        let reborn = plane.grant(7, "p-self");
        assert_eq!(
            plane.live_session_count(7),
            0,
            "前世的乐手不该吃掉新身份的额度"
        );
        for i in 0..DEFAULT_SESSION_CAP {
            let (status, v) = start(port, &reborn, r#""launcherId":"claude""#);
            assert_eq!(status, 200, "新身份的第 {i} 个: {v}");
        }
    }

    /// 已离场记账的**回收口径**：等它那个乐手 pane 自己关掉。
    ///
    /// 那一刻它既没有 tab 可标（pane 没了）也没有编排者读得到，留着就是一条
    /// 永不过期的泄漏 —— 修剪只在登记新乐手时跑，而已离场的编排者不会再登记。
    #[test]
    fn 已离场编排者的乐手关掉后记账就地回收() {
        let (plane, _host, actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let musician = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        plane.revoke_pane(7);
        assert!(plane.origins().contains_key(&musician), "乐手还活着，标识还要画");

        actions.close(musician);
        plane.revoke_pane(musician); // 乐手 pane 关闭走的就是这条
        assert!(
            !plane.origins().contains_key(&musician),
            "编排者已离场 + 乐手也关了 = 这条记账再没人用得着"
        );
    }

    /// 反过来的顺序：乐手先关、编排者后走 —— 那条已关记账在退场那一刻回收。
    #[test]
    fn 编排者离场时已关的乐手记账当场回收() {
        let (plane, _host, actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let closed_one = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let live_one = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        actions.close(closed_one);
        plane.revoke_pane(closed_one);
        plane.revoke_pane(7);

        let origins = plane.origins();
        assert!(!origins.contains_key(&closed_one), "已关的那条就地回收");
        assert!(
            origins[&live_one].orchestrator_departed,
            "还活着的那条留下来标「编排者已离场」"
        );
    }

    /// 同一 pane 编号上**重复授予**也是新身份（PTY 重开 / SSH 重连那条路）：
    /// 前世的乐手一律转已离场，不许被认领回去。
    #[test]
    fn 重复授予即新身份前世乐手转已离场() {
        let (plane, _host, _actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let musician = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        // 没走 revoke，直接再授予一次
        let reborn = plane.grant(7, "p-self");
        assert!(
            plane.origins()[&musician].orchestrator_departed,
            "重复授予 = 新身份，前世的乐手该转已离场"
        );
        let (status, body) = post(port, "/control/list-panes", &payload_of(&reborn, ""));
        assert_eq!(status, 200);
        assert!(
            json(&body)["data"]["panes"].as_array().unwrap().is_empty(),
            "{body}"
        );
    }

    /// 出身快照的版本号：记账每变一次就该动一下 —— 渲染侧靠它决定要不要重取。
    #[test]
    fn 出身版本号跟着记账变() {
        let (plane, _host, actions, port, token) = granted();
        let v0 = plane.origins_version();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let musician = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;
        let v1 = plane.origins_version();
        assert!(v1 > v0, "登记乐手要动号");

        plane.revoke_pane(7);
        let v2 = plane.origins_version();
        assert!(v2 > v1, "编排者离场要动号 —— 标识得跟着降级");

        actions.close(musician);
        plane.revoke_pane(musician);
        assert!(plane.origins_version() > v2, "回收也要动号");

        // 纯读不动号:否则渲染侧每帧都要重取一次
        let before = plane.origins_version();
        let _ = plane.origins();
        assert_eq!(plane.origins_version(), before);
    }

    /// 出身快照**只带展示要用的两样**，编排者那边的东西一个字不带过来。
    #[test]
    fn 出身快照不带项目与启动器细节() {
        let (plane, _host, _actions, port, token) = granted();
        let (_s, v) = start(port, &token, r#""launcherId":"claude""#);
        let musician = v["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        let origin = plane.origins().remove(&musician).unwrap();
        assert_eq!(
            origin,
            SessionOrigin {
                orchestrator_label: "大脑".into(),
                orchestrator_departed: false,
            }
        );
    }

    // ─── send：写穿与 bracketed paste 装配（工单 05）────────────

    /// 单行 prompt：装配成一整块粘贴 + **包裹之外**的一个回车。
    ///
    /// **单行也包 bracketed paste**，理由有三：① 一种形状一条路，「只有多行才包」
    /// 会长出第二条只在少数情形下走到的分支；② 粘贴块里的正文对 TUI 而言是纯
    /// 文本，单行 prompt 里出现的 `\t`、`\x1b` 之类同样被当字面量而不是热键；
    /// ③ `mt_ai::tracker` 认得这对标记，整块 + 一个回车恰好被记成**一次**提交，
    /// AI 会话身份与 marker 因此与用户亲手粘贴完全同形。
    #[test]
    fn 单行_prompt_装配成一块粘贴并跟一个回车() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);

        let (status, v) = send(port, &token, musician, "跑一下测试");
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["data"]["sent"]["paneId"], musician);
        assert_eq!(v["data"]["sent"]["bracketedPaste"], true);

        let calls = actions.sends.lock().clone();
        assert_eq!(calls.len(), 1, "立即写穿一次，不排队也不重发");
        assert_eq!(calls[0].pane_id, musician);
        assert_eq!(calls[0].bracketed, "\x1b[200~跑一下测试\x1b[201~\r");
        assert_eq!(calls[0].plain, "跑一下测试\r");
    }

    /// 多行 prompt（含代码块）：整体一块送达，中途的换行**一个都不许**变成提交。
    ///
    /// 换行一律归一成 `\r`（PTY 那头把 `\n` 当「换行但不回车」，多行会出阶梯）——
    /// 与 `mt_ui::terminal::input::paste_to_bytes` 同一口径。
    #[test]
    fn 多行_prompt_整块送达且回车在包裹之外() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);

        // 混着 LF 与 CRLF —— 编排者拼 prompt 时两种都可能出现
        let prompt = "修一下这个：\n```rust\r\nfn main() {}\n```";
        let (status, v) = send(port, &token, musician, prompt);
        assert_eq!(status, 200, "{v}");

        let calls = actions.sends.lock().clone();
        assert_eq!(
            calls[0].bracketed,
            "\x1b[200~修一下这个：\r```rust\rfn main() {}\r```\x1b[201~\r"
        );
        assert_eq!(calls[0].plain, "修一下这个：\r```rust\rfn main() {}\r```\r");
        // 裸 `\n` 一个都不许进 PTY
        assert!(!calls[0].bracketed.contains('\n'), "换行没归一成 \\r");
        // 回车在**结束标记之后**：包在里头只是往编辑框插一个换行，送不出去
        assert!(calls[0].bracketed.ends_with("\x1b[201~\r"));
    }

    /// 正文末尾的换行删掉再补一个回车 —— 否则就是替编排者多按一次。
    ///
    /// LLM 拿 heredoc / 三引号拼 prompt 时结尾几乎总带一个换行，原样留着那一下
    /// 在等确认的 TUI 里会被当成「确认」。
    #[test]
    fn 正文末尾的换行不会变成多按一次回车() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);

        assert_eq!(send(port, &token, musician, "干活\n\n").0, 200);
        let calls = actions.sends.lock().clone();
        assert_eq!(calls[0].bracketed, "\x1b[200~干活\x1b[201~\r");
        assert_eq!(calls[0].plain, "干活\r");
    }

    /// 正文里的结束标记要剔掉：留着的话 prompt 自己就能把粘贴块提前截断，
    /// 后半截变成真键入 —— 那正是本命令要防的事（`paste_to_bytes` 同款防线）。
    #[test]
    fn 正文里的结束标记被剔掉() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);

        assert_eq!(send(port, &token, musician, "前半\x1b[201~后半").0, 200);
        let calls = actions.sends.lock().clone();
        assert_eq!(calls[0].bracketed, "\x1b[200~前半后半\x1b[201~\r");
        assert_eq!(
            calls[0].bracketed.matches("\x1b[201~").count(),
            1,
            "结束标记只许有装配加上的那一个"
        );
    }

    /// 目标终端没开 bracketed paste（agent 退了、pane 退回裸 shell）时**如实说**：
    /// 那段多行是逐行进去的，编排者需要知道。
    #[test]
    fn 目标没开粘贴模式时如实告知() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        *actions.bracketed_paste.lock() = Some(false);

        let (status, v) = send(port, &token, musician, "第一行\n第二行");
        assert_eq!(status, 200, "{v}");
        assert_eq!(
            v["data"]["sent"]["bracketedPaste"], false,
            "不许粉饰成整块送达"
        );
        // 装配照旧两份都备着，桌面侧挑的是裸的那一份
        let calls = actions.sends.lock().clone();
        assert_eq!(calls[0].plain, "第一行\r第二行\r");
    }

    /// [`PaneInput::bytes`] 就是桌面侧那一次布尔判断。
    #[test]
    fn 按目标模式挑一份() {
        let input = PaneInput::assemble("一行").unwrap();
        assert_eq!(input.bytes(true), input.bracketed());
        assert_eq!(input.bytes(false), input.plain());
        assert_eq!(input.bytes(true), "\x1b[200~一行\x1b[201~\r");
        assert_eq!(input.bytes(false), "一行\r");
        // 正文一个字都不许出现在 Debug 里（它会进日志 / panic 消息）
        assert!(!format!("{input:?}").contains("一行"));
    }

    /// **可见范围铁律**：向非自启 pane 写穿一律「不存在」语义，且不惊动桌面端。
    #[test]
    fn 向非自启_pane_写穿被拒() {
        let (plane, _host, actions, port, token) = granted();
        let mine = start_musician(port, &token);

        // 另一个编排者的乐手
        let other = plane.grant(9, "p-self");
        let payload = format!(r#"{{"token":"{other}","paneId":9,"launcherId":"claude"}}"#);
        let (_s, body) = post(port, "/control/start-session", &payload);
        let theirs = json(&body)["data"]["pane"]["paneId"].as_u64().unwrap() as u32;

        actions.sends.lock().clear();
        // 别人的乐手 / 别人的编排者 pane / 编造的编号 —— 同一个「不存在」
        for target in [theirs, 9, 4242] {
            let (status, v) = send(port, &token, target, "干活");
            assert_eq!(status, 404, "target={target}: {v}");
            assert_eq!(v["error"]["code"], "paneNotFound", "target={target}");
        }
        // 自指禁令有自己的码（自己的身份本来就钉在环境里，能自我纠正）
        let (status, v) = send(port, &token, 7, "干活");
        assert_eq!(status, 403, "{v}");
        assert_eq!(v["error"]["code"], "selfTarget");

        assert!(
            actions.sends.lock().is_empty(),
            "被拒的写穿一个字节都不许落到桌面端"
        );
        // 自己的乐手照旧写得进去（上面几条不是把整条路封死了）
        assert_eq!(send(port, &token, mine, "干活").0, 200);
    }

    /// 自己的乐手但 pane 已经关了：这条**可以**说清楚。
    #[test]
    fn 写穿已关的乐手得到_pane_gone() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.close(musician);

        let (status, v) = send(port, &token, musician, "干活");
        assert_eq!(status, 410, "{v}");
        assert_eq!(v["error"]["code"], "paneGone");
        assert!(actions.sends.lock().is_empty());
    }

    /// 空正文被拒 —— 裸回车就是替用户按确认，而「attention 不代答」是 ADR 0003
    /// 的铁律。给它专门的码，编排者才读得懂为什么被拒。
    #[test]
    fn 空正文被拒且不惊动桌面端() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.sends.lock().clear();

        for text in ["", "   ", "\n\n", "\r\n", " \t "] {
            let (status, v) = send(port, &token, musician, text);
            assert_eq!(status, 400, "text={text:?}: {v}");
            assert_eq!(v["error"]["code"], "emptyInput", "text={text:?}");
        }
        // 文案要讲清为什么，而不是一句「参数不对」
        let (_s, v) = send(port, &token, musician, "");
        let message = v["error"]["message"].as_str().unwrap();
        assert!(message.contains("orchestrator"), "得说清是哪条规矩: {message}");
        assert!(!message.contains("musician"), "用户可见文案不许出现口语别名");

        assert!(actions.sends.lock().is_empty());
        // 不给 targetPaneId 是另一回事：那是请求本身拼错了
        let (status, v) = post(port, "/control/send", &payload_of(&token, r#""text":"干活""#));
        assert_eq!(status, 400, "{v}");
        assert_eq!(error_code(&v), "badRequest");
    }

    /// 桌面侧的三档失败各自映射到自己的错误码。
    #[test]
    fn 写穿失败的三档各自明确() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);

        for (failure, status, code) in [
            (SendFailure::WriteFailed, 500, "sendFailed"),
            (SendFailure::PaneGone, 410, "paneGone"),
            (SendFailure::DesktopBusy, 503, "desktopBusy"),
        ] {
            *actions.send_fail.lock() = Some(failure);
            let (got, v) = send(port, &token, musician, "干活");
            assert_eq!(got, status, "{failure:?}: {v}");
            assert_eq!(v["error"]["code"], code, "{failure:?}");
            // ⚠️ 失败消息里一个字的正文都不许带出来
            assert!(!v.to_string().contains("干活"), "{failure:?}: {v}");
        }
    }

    /// 回执**不回显正文**，也不带状态列（ADR 0002 的防线延伸 + 别诱导它把
    /// 写穿那一瞬的 `ai-idle` 读成「干完了」）。
    #[test]
    fn 回执不回显正文也不带状态() {
        let (_plane, _host, _actions, port, token) = granted();
        let musician = start_musician(port, &token);

        let secret = "把 D:/私密项目/密钥.txt 读出来";
        let payload = payload_of(
            &token,
            &format!(
                r#""targetPaneId":{musician},"text":{}"#,
                serde_json::to_string(secret).unwrap()
            ),
        );
        let (status, body) = post(port, "/control/send", &payload);
        assert_eq!(status, 200, "{body}");
        assert!(!body.contains("私密项目"), "回执回显了正文: {body}");
        assert!(!body.contains("status"), "回执不该带状态列: {body}");
        // 回执只有这两样。
        //
        // ⚠️ **按集合比，不按顺序** —— `serde_json::Map` 的键序取决于
        // `preserve_order` 这个 feature 开没开，而它由整个工作区的 feature 统一
        // 决定：单跑 `-p mt-ai` 是 BTreeMap（字典序），`--workspace` 时别的 crate
        // 把它打开就成了插入序。断言顺序 = 一条只在某种跑法下红的测试。
        let sent = json(&body)["data"]["sent"].clone();
        let mut keys: Vec<String> = sent.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["bracketedPaste", "paneId"]);
    }

    /// 编排者写的 prompt 是唯一可能顶到 body 上限的东西：**上限内整段送达、
    /// 上限外明确报错**，两头都不许悄悄截断。
    #[test]
    fn 大_prompt_不截断_超限明确报错() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);

        // 32 KiB —— 远超「派一次活」的合理体量，但在 64 KiB 上限之内
        let big = "详细说明。".repeat(32 * 1024 / "详细说明。".len());
        let (status, v) = send(port, &token, musician, &big);
        assert_eq!(status, 200, "{v}");
        let calls = actions.sends.lock().clone();
        assert_eq!(
            calls[0].plain.len(),
            big.len() + 1,
            "正文被截断了（末尾那 1 字节是补的回车）"
        );

        // 顶破上限：拒得明确，且一个字节都没落到桌面端
        actions.sends.lock().clear();
        let huge = "x".repeat(MAX_CONTROL_BODY_BYTES + 1);
        let (status, v) = send(port, &token, musician, &huge);
        assert_eq!(status, 413, "{v}");
        assert_eq!(v["error"]["code"], "payloadTooLarge");
        assert!(actions.sends.lock().is_empty(), "超限的正文不许写出去半截");
    }

    // ─── wait：四类终态与超时（工单 06）────────────────────────

    /// `wait` 一次。`timeout_ms` 直接给毫秒 —— 主缝测试靠它把长轮询压到几百毫秒，
    /// 不必为节拍另开一个只有测试会拨的旋钮。
    fn wait(port: u16, token: &str, target: u32, timeout_ms: u64) -> (u16, serde_json::Value) {
        let payload = payload_of(
            token,
            &format!(r#""targetPaneId":{target},"timeoutMs":{timeout_ms}"#),
        );
        let (status, body) = post(port, "/control/wait", &payload);
        (status, json(&body))
    }

    /// 终态一：**干完了**。成因原文照带 —— 只有 `Stop` 是真做完。
    #[test]
    fn wait_干完了返回_ai_idle_并带成因() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.finished(musician, "Stop");

        let (status, v) = wait(port, &token, musician, 2_000);
        assert_eq!(status, 200, "{v}");
        let w = &v["data"]["waited"];
        assert_eq!(w["outcome"], "ai-idle");
        assert_eq!(w["status"], "ai-idle", "状态原文与徽章同一口径");
        assert_eq!(w["cause"], "Stop");
        assert_eq!(w["paneId"], musician);
        // 已经收敛了就立刻答，不必等满耐心
        assert!(w["waitedMs"].as_u64().unwrap() < 1_000, "{w}");
    }

    /// `ai-idle` 的三种成因**必须分得开**：`Stop` 是干完了，`Interrupt` 是用户
    /// 按了 Esc（两条兜底之一），`Stall` 是停摆兜底收敛的。编排者拿它们做的决定
    /// 完全不同 —— 把「被打断」当成「做完了」就是把半截活报成交付。
    #[test]
    fn wait_把打断与停摆的成因原样交出去() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);

        for cause in ["Stop", "Interrupt", "Stall"] {
            actions.finished(musician, cause);
            let (status, v) = wait(port, &token, musician, 2_000);
            assert_eq!(status, 200, "{v}");
            assert_eq!(v["data"]["waited"]["outcome"], "ai-idle", "cause={cause}");
            assert_eq!(
                v["data"]["waited"]["cause"], cause,
                "成因必须原文照带，不许归一成「完成」"
            );
        }
    }

    /// 终态二：**停在等审批 / 向人提问**，原因原文照带。
    ///
    /// ⚠️ 两家的状态不一样，而判据是**成因**不是状态：Claude 的
    /// `PermissionRequest` 落在 `ai-idle`，Codex 的落在 `ai-working`
    /// （批准后直接执行工具，`hook_server::map_event_to_status` 对它有专门一条）。
    /// 只看状态的话，正等着 Codex 审批的乐手会被当成「还在跑」一直等到超时 ——
    /// 而 attention 恰恰是最该立刻告诉人的那一档。
    #[test]
    fn wait_停在_attention_立刻返回并带原因() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);

        for (status_text, cause) in [
            ("ai-idle", "PermissionRequest"),   // Claude
            ("ai-working", "PermissionRequest"), // Codex：批准后直接执行工具
            ("ai-idle", "Elicitation"),
            ("ai-idle", "StopFailure"), // 回合因 API 错误结束，要人回来看
        ] {
            actions.attention(musician, status_text, cause);
            let (http, v) = wait(port, &token, musician, 2_000);
            assert_eq!(http, 200, "{v}");
            let w = &v["data"]["waited"];
            assert_eq!(
                w["outcome"], "attention",
                "status={status_text} cause={cause}: attention 的判据是成因不是状态"
            );
            assert_eq!(w["cause"], cause, "原因原文要交到编排者手上");
            assert_eq!(w["status"], status_text, "状态照实说，两家形态不同");
            assert!(w["waitedMs"].as_u64().unwrap() < 1_000, "别让人干等着: {w}");
        }
    }

    /// 终态三：**agent 已退出**（pane 还在，退回裸 shell）。
    ///
    /// 与 `ai-idle` 是两回事：前者 pane 还在 AI 会话里，后者已经没有 agent 了 ——
    /// 编排者要据此决定是接着派活还是重起一个。判据借
    /// [`AiSessionState::Ended`]，不是拿状态字符串裸比。
    #[test]
    fn wait_agent_退出返回_idle() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.agent_exited(musician);

        let (status, v) = wait(port, &token, musician, 2_000);
        assert_eq!(status, 200, "{v}");
        let w = &v["data"]["waited"];
        assert_eq!(w["outcome"], "idle");
        assert_eq!(w["status"], "idle");
        assert!(w["cause"].is_null(), "退出这一档没有成因: {w}");
    }

    /// 终态四：**pane 不存在**（可见范围铁律）。三条语义各一例，一条都不泄露
    /// 桌面上还有什么别的 pane。
    #[test]
    fn wait_向非自启_pane_被拒() {
        let (_plane, _host, _actions, port, token) = granted();
        let musician = start_musician(port, &token);
        let _ = musician;

        // 别人的乐手 / 用户亲手开的会话 / 编造的编号 —— 统一「不存在」
        let (status, v) = wait(port, &token, 4242, 2_000);
        assert_eq!(status, 404);
        assert_eq!(v["error"]["code"], "paneNotFound");
        // 自指禁令
        let (status, v) = wait(port, &token, 7, 2_000);
        assert_eq!(status, 403);
        assert_eq!(v["error"]["code"], "selfTarget");
    }

    /// 是自己起的、但那个 pane 已经关了 → `paneGone`（**可以**说，那是它自己
    /// 起的东西）。开头那一次裁决就挡下来，不进轮询。
    #[test]
    fn wait_乐手_pane_已关时明确报已关() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.close(musician);

        let (status, v) = wait(port, &token, musician, 2_000);
        assert_eq!(status, 410);
        assert_eq!(v["error"]["code"], "paneGone");
    }

    /// 等着等着被用户关掉：同样答 `paneGone`，而不是憋到上界给一个 `pending`。
    /// 「你起的那个已经关了」是一个确定的结论，编排者该立刻知道。
    #[test]
    fn wait_期间被关掉答已关而不是超时() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.live(musician, "ai-working"); // 还在跑，wait 会进轮询

        let closer = {
            let actions = actions.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                actions.close(musician);
            })
        };
        let (status, v) = wait(port, &token, musician, 5_000);
        closer.join().unwrap();
        assert_eq!(status, 410, "{v}");
        assert_eq!(v["error"]["code"], "paneGone");
    }

    /// **超时不是错误**：等到耐心用尽仍在跑 → 200 + `pending`，让编排者自己决定
    /// 继续等还是先去干别的。做成错误码就得给它一个「你或我们出了问题」的退出码档位。
    #[test]
    fn wait_超时是正常回执而不是错误() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.live(musician, "ai-working");

        let (status, v) = wait(port, &token, musician, 300);
        assert_eq!(status, 200, "超时不许变成 HTTP 错误: {v}");
        let w = &v["data"]["waited"];
        assert_eq!(w["outcome"], "pending");
        assert_eq!(w["status"], "ai-working", "还在跑这件事要说清");
        assert_eq!(v["ok"], true);
        assert!(
            w["waitedMs"].as_u64().unwrap() >= 300,
            "该等满的耐心不许提前收工: {w}"
        );
    }

    /// **状态迁移经假宿主驱动**：轮询期间乐手从 ai-working 干完 → wait 拿到终态。
    /// 这条证的是长轮询真的在轮询，而不是只看了开头那一眼。
    #[test]
    fn wait_轮询到状态迁移后收敛() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.live(musician, "ai-working");

        let flip = {
            let actions = actions.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                actions.finished(musician, "Stop");
            })
        };
        let (status, v) = wait(port, &token, musician, 5_000);
        flip.join().unwrap();
        assert_eq!(status, 200, "{v}");
        let w = &v["data"]["waited"];
        assert_eq!(w["outcome"], "ai-idle");
        assert_eq!(w["cause"], "Stop");
        assert!(
            w["waitedMs"].as_u64().unwrap() >= 60,
            "它得是等出来的，不是开头那一眼: {w}"
        );
    }

    /// 人工处理之后的下一次 `wait` 拿到恢复后的终态（工单验收项第 4 条的进程内
    /// 那一半；真机走查留工单 09）。attention 期间编排者**什么都不做** ——
    /// 这里也就没有任何代答动作可测：`send` 的调用记录必须是空的。
    #[test]
    fn wait_人工处理后下一次拿到恢复后的终态() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);

        actions.attention(musician, "ai-idle", "PermissionRequest");
        let (_s, v) = wait(port, &token, musician, 2_000);
        assert_eq!(v["data"]["waited"]["outcome"], "attention");

        // 用户去那个 pane 批准了 → 下一个 hook 事件把状态推回工作中
        actions.live(musician, "ai-working");
        let (_s, v) = wait(port, &token, musician, 300);
        assert_eq!(v["data"]["waited"]["outcome"], "pending", "批完还在跑");

        // 干完
        actions.finished(musician, "Stop");
        let (_s, v) = wait(port, &token, musician, 2_000);
        assert_eq!(v["data"]["waited"]["outcome"], "ai-idle");
        assert_eq!(v["data"]["waited"]["cause"], "Stop");

        assert!(
            actions.sends.lock().is_empty(),
            "attention 时编排者不代答（ADR 0003）：wait 一个字节都不许替它写"
        );
    }

    /// **fail-closed**：说不上来的那一档（无 hook、输入检测也没认出来的自定义
    /// 启动器）绝不谎报成终态。它等到上界，答 `pending` + `status: "idle"` ——
    /// 那两样合起来是「这个乐手我看不透」的唯一签名。
    ///
    /// 谎报成 `idle`（已退出）会让编排者去重起一个还在跑的活；谎报成 `ai-idle`
    /// （干完了）会让它把没开始的活当成交付。两个都比多等一会儿坏得多。
    #[test]
    fn wait_说不上来时不谎报终态() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.unknown(musician);

        let (status, v) = wait(port, &token, musician, 300);
        assert_eq!(status, 200, "{v}");
        let w = &v["data"]["waited"];
        assert_eq!(
            w["outcome"], "pending",
            "「答不上来」不许被当成干完了或已退出: {w}"
        );
        assert_eq!(w["status"], "idle", "pending + idle = 这个乐手看不透");
    }

    /// 超时上界：编排者报一个天文数字也只等到 [`WAIT_MAX`]（**钳而不拒**）。
    /// 这条不真等 5 分钟 —— 只钉纯函数那一层的钳位口径。
    #[test]
    fn wait_耐心有服务端上界() {
        assert_eq!(wait_patience(None), WAIT_DEFAULT, "不给就是默认值");
        assert_eq!(
            wait_patience(Some(u64::MAX)),
            WAIT_MAX,
            "报多大都只等到上界，且不报错"
        );
        assert_eq!(wait_patience(Some(1_500)), Duration::from_millis(1_500));
        assert_eq!(
            wait_patience(Some(0)),
            Duration::ZERO,
            "0 是合法值：只看一眼就回来"
        );
        assert!(WAIT_DEFAULT < WAIT_MAX, "默认值得落在上界之内");
        assert!(
            WAIT_POLL_INTERVAL < WAIT_DEFAULT,
            "一拍都比默认耐心长的话就成了「只看一眼」"
        );
    }

    /// `timeoutMs: 0` = 只看一眼就回来（不睡）。给编排者一条「非阻塞查一下状态」
    /// 的路，省得为它另加一条命令。
    #[test]
    fn wait_零超时只看一眼() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.live(musician, "ai-working");

        let started = std::time::Instant::now();
        let (status, v) = wait(port, &token, musician, 0);
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["data"]["waited"]["outcome"], "pending");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "0 超时不许睡一拍"
        );
    }

    /// 没给目标编号 → `badRequest`（与 `send` 同一条）。
    #[test]
    fn wait_缺目标编号被拒() {
        let (_plane, _host, _actions, port, token) = granted();
        let payload = payload_of(&token, r#""timeoutMs":100"#);
        let (status, body) = post(port, "/control/wait", &payload);
        assert_eq!(status, 400);
        assert_eq!(error_code(&body), "badRequest");
    }

    /// 长轮询期间 hook 那条路必须照常通 —— `wait` 一等就是几分钟，
    /// 就地跑会把 AI 状态感知那条**权威通道**卡到天荒地老。
    ///
    /// （`start-session` 那条同款测试用的是 600ms 的慢动作；这条更狠：
    /// wait 是真的在睡，睡的正是我们让它睡的那 1.5 秒。）
    #[test]
    fn 长轮询期间_hook_那条路不被卡住() {
        let (_plane, _host, actions, port, token) = granted();
        let musician = start_musician(port, &token);
        actions.live(musician, "ai-working"); // 永远不收敛，一定等满

        let waiter = {
            let token = token.clone();
            std::thread::spawn(move || wait(port, &token, musician, 1_500))
        };
        // 让 wait 先把请求发出去
        std::thread::sleep(Duration::from_millis(120));
        let started = std::time::Instant::now();
        let (status, _body) = post(port, "/hook", r#"{"ptyId":1,"event":"Stop"}"#);
        let hook_took = started.elapsed();
        // serve() 里非控制路由一律 404（真 hook 循环在别处），要的是「立刻答」
        assert_eq!(status, 404);
        assert!(
            hook_took < Duration::from_millis(500),
            "长轮询把 hook 那条队卡住了: {hook_took:?}"
        );

        let (status, v) = waiter.join().unwrap();
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["data"]["waited"]["outcome"], "pending");
    }

    /// 纯判定那一层：三档终态 + 两档不收敛，不起 HTTP。
    #[test]
    fn 终态判定只认既有事实() {
        let liveness = |status: &str, ai: AiSessionState, cause: Option<&str>| PaneLiveness {
            alive: true,
            status: status.into(),
            ai_session: ai,
            cause: cause.map(str::to_string),
        };

        // 干完了（无 hook 的降级路径上没有成因，照样是 ai-idle）
        assert_eq!(
            liveness("ai-idle", AiSessionState::Active, Some("Stop")).settled(),
            Some(WaitState::AiIdle)
        );
        assert_eq!(
            liveness("ai-idle", AiSessionState::Unknown, None).settled(),
            Some(WaitState::AiIdle),
            "无 hook 的乐手也能报干完了，只是没有成因"
        );
        // attention 先于状态：Codex 那一档状态是 ai-working
        assert_eq!(
            liveness("ai-working", AiSessionState::Active, Some("PermissionRequest")).settled(),
            Some(WaitState::Attention)
        );
        // 已退出只认明确结束的那一档
        assert_eq!(
            liveness("idle", AiSessionState::Ended, None).settled(),
            Some(WaitState::Idle)
        );
        assert_eq!(
            liveness("idle", AiSessionState::Unknown, None).settled(),
            None,
            "说不上来的那一档不许被当成已退出"
        );
        // 还在跑
        assert_eq!(
            liveness("ai-working", AiSessionState::Active, Some("PreToolUse")).settled(),
            None
        );
        // pane 都没了不是终态（由调用方答 paneGone）
        assert_eq!(PaneLiveness::gone().settled(), None);
        // 线上那三个名字与 status 同一套词汇
        assert_eq!(WaitState::AiIdle.name(), "ai-idle");
        assert_eq!(WaitState::Idle.name(), "idle");
        assert_eq!(WaitState::Attention.name(), "attention");
    }

    // ─── 记账修剪 ─────────────────────────────────────────────

    /// 直接往记账表里塞一条（绕开 HTTP 与并发上限，专测修剪口径）。
    fn record(plane: &ControlPlane, orchestrator: u32, pane_id: u32) {
        plane.register_landed(OrchestratedSession {
            pane_id,
            orchestrator_pane_id: orchestrator,
            orchestrator_label: "大脑".into(),
            orchestrator_departed: false,
            project_id: "p-self".into(),
            project_name: "项目".into(),
            launcher_id: "claude".into(),
            launcher_name: "Claude".into(),
        });
    }

    /// 超出上限时**只丢已经关掉的、从最旧的丢起**。
    #[test]
    fn 记账超上限时先丢最旧的已关条目() {
        let plane = ControlPlane::new();
        // 先塞满上限：前 10 条随后被用户关掉
        for i in 0..MAX_SESSIONS_PER_ORCHESTRATOR as u32 {
            record(&plane, 7, 100 + i);
        }
        for i in 0..10u32 {
            plane.revoke_pane(100 + i); // 乐手 pane 关闭走的就是这条
        }
        assert_eq!(
            plane.session_ids_of(7).len(),
            MAX_SESSIONS_PER_ORCHESTRATOR,
            "还没超，一条都不该丢"
        );

        // 再起三个：把最旧的三条已关记账挤掉
        for i in 0..3u32 {
            record(&plane, 7, 200 + i);
        }
        let ids = plane.session_ids_of(7);
        assert_eq!(ids.len(), MAX_SESSIONS_PER_ORCHESTRATOR);
        assert!(!ids.contains(&100), "最旧的已关条目该先走");
        assert!(!ids.contains(&102));
        assert!(ids.contains(&103), "只丢超出的那几条，别多丢");
        assert!(ids.contains(&200) && ids.contains(&202));
    }

    /// **活着的一条都不能丢** —— 丢掉一条活着的记账就是造一个幽灵乐手，
    /// 比表长一点坏得多。丢不动就让它超着。
    #[test]
    fn 修剪不许丢活着的记账() {
        let plane = ControlPlane::new();
        let over = MAX_SESSIONS_PER_ORCHESTRATOR as u32 + 20;
        for i in 0..over {
            record(&plane, 7, 100 + i);
        }
        assert_eq!(
            plane.session_ids_of(7).len(),
            over as usize,
            "一条都没关，超着也不许丢"
        );

        // 关掉 5 条，下一次登记才丢得动 5 条
        for i in 0..5u32 {
            plane.revoke_pane(100 + i);
        }
        record(&plane, 7, 900);
        assert_eq!(plane.session_ids_of(7).len(), over as usize + 1 - 5);
    }

    /// 修剪按编排者各算各的：别人的记账不该被我起会话挤掉。
    #[test]
    fn 修剪只动自己名下的记账() {
        let plane = ControlPlane::new();
        record(&plane, 9, 50);
        plane.revoke_pane(50);
        for i in 0..(MAX_SESSIONS_PER_ORCHESTRATOR as u32 + 5) {
            record(&plane, 7, 100 + i);
        }
        assert_eq!(plane.session_ids_of(9), vec![50], "别人的记账一条不动");
    }

    /// 修剪只数**当前身份**名下的：前世留下的那些已离场记账既不占名额、
    /// 也不该把新身份的 50 条额度吃掉。
    #[test]
    fn 修剪不数已离场的记账() {
        let plane = ControlPlane::new();
        for i in 0..MAX_SESSIONS_PER_ORCHESTRATOR as u32 {
            record(&plane, 7, 100 + i);
        }
        // 编排者离场:这 50 条全转已离场(乐手都还活着,一条都不该丢)
        plane.revoke_pane(7);
        assert_eq!(plane.origins().len(), MAX_SESSIONS_PER_ORCHESTRATOR);
        assert!(plane.session_ids_of(7).is_empty(), "新身份名下一条都没有");

        // 新身份从零起算,50 条额度是它自己的
        for i in 0..MAX_SESSIONS_PER_ORCHESTRATOR as u32 {
            record(&plane, 7, 300 + i);
        }
        assert_eq!(
            plane.session_ids_of(7).len(),
            MAX_SESSIONS_PER_ORCHESTRATOR,
            "前世那 50 条不许挤掉今生的"
        );
    }

    // ─── 命令表 ───────────────────────────────────────────────

    /// 命令名与「能不能就地跑完」只有一份表。加一个变体时 `ALL` / `name` /
    /// `needs_own_thread` / `handle` 的分发会一起编译不过 —— 这条测试补的是
    /// 「表里那几条自己对得上」。
    #[test]
    fn 每条命令都解析得回自己() {
        for cmd in Command::ALL {
            assert_eq!(Command::parse(cmd.name()), Some(cmd), "{cmd:?}");
        }
        // 名字与 sidecar CLI 的 `Command::endpoint` 一字不差
        let names: Vec<&str> = Command::ALL.iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            vec![
                "list-launchers",
                "list-projects",
                "start-session",
                "list-panes",
                "send",
                "wait"
            ]
        );
        // 认不出的一律 None（带查询串 / 多层路径的别去猜）。
        // ⚠️ 占位名必须是**永远不会被实现**的那种：工单 05 的留档记着这条教训
        // （它当年拿 `send` 当占位，05 一落地那条测试就成了「send 居然是未知命令」）。
        // 工单 07 的 `read` 同理，别写进来。
        for bad in ["", "send?x=1", "send/extra", "wait/", "no-such-command"] {
            assert_eq!(Command::parse(bad), None, "{bad}");
        }
    }

    /// 「别在 HTTP 那条循环里就地做」的那些命令都登记齐了，其余就地答完。
    ///
    /// 两种进表的理由（见 [`Command::needs_own_thread`]）：`start-session` / `send`
    /// 要回 gpui 主线程等回执；`wait` 主线程一次不碰，但要在这条线程上睡到几分钟。
    /// 漏登记任意一条都会把 hook 上报那条队一起卡住 —— 正是这张表要防的事，
    /// 而 `wait` 卡的不是三秒，是几分钟。
    #[test]
    fn 不能就地跑完的命令都登记在表里() {
        let threaded: Vec<&str> = Command::ALL
            .iter()
            .filter(|c| c.needs_own_thread())
            .map(|c| c.name())
            .collect();
        assert_eq!(threaded, vec!["start-session", "send", "wait"]);
    }

    // ─── 阻塞命令不占 HTTP 线程 ───────────────────────────────

    /// `start-session` 要等桌面主线程，而 hook 上报与它排在**同一条 HTTP 队**上。
    /// 起会话期间 hook 那条路必须照常通 —— AI 状态感知不给编排让路。
    #[test]
    fn 起会话期间_hook_那条路不被卡住() {
        let (_plane, _host, actions, port, token) = granted();
        *actions.delay.lock() = Some(Duration::from_millis(600));

        let start_token = token.clone();
        let slow = std::thread::spawn(move || start(port, &start_token, r#""launcherId":"claude""#));
        // 让慢请求先进到 handler 里
        std::thread::sleep(Duration::from_millis(120));

        let began = std::time::Instant::now();
        let (status, _body) = post(port, "/hook", "{}");
        let waited = began.elapsed();
        assert_eq!(status, 404, "本测试服务只接控制路由，hook 请求走到 404");
        assert!(
            waited < Duration::from_millis(400),
            "hook 上报被起会话卡了 {waited:?} —— 阻塞命令必须另起线程"
        );

        let (status, v) = slow.join().unwrap();
        assert_eq!(status, 200, "{v}");
    }

    /// 另起线程的那条路**不能**变成免鉴权的口子：无令牌请求在 HTTP 线程上就被挡。
    #[test]
    fn 阻塞命令的鉴权一样_fail_closed() {
        let (_plane, _host, actions, port, _token) = granted();
        for payload in [
            r#"{"paneId":7,"launcherId":"claude"}"#.to_string(),
            r#"{"token":"forged","paneId":7,"launcherId":"claude"}"#.to_string(),
            "not json".to_string(),
        ] {
            let (status, body) = post(port, "/control/start-session", &payload);
            assert!(status == 401 || status == 400, "payload={payload}: {body}");
        }
        assert!(actions.calls.lock().is_empty(), "被拒的请求不许惊动桌面端");
    }
}
