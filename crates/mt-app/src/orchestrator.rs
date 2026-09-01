//! 编排控制面的桌面侧接线(ADR 0003 / 工单 02)。
//!
//! `mt-ai` 的控制面不认识配置层,项目表与启动器名单经
//! [`mt_ai::OrchestratorHost`] 注入 —— 与移动端中转的 `RelayHost` 同一个模式,
//! 连「主线程刷新、后台线程只读一份镜像」的处理都照抄
//! (控制端点跑在 hook 那条 HTTP 线程上,碰不得 gpui 实体)。
//!
//! ```text
//! AppStore(主线程) ──refresh_orchestrator_mirror()──→ OrchestratorMirror
//!                                                            │只读
//!                              hook/控制 HTTP 线程 ──HostImpl─┘
//! ```
//!
//! # 分组归属在这里算,不在 mt-ai 算
//!
//! 「可达范围 = 本项目 + 同分组」这条裁决住在 `mt_ai::control`,但**分组是什么
//! 形状**是配置层的事(`project_tree` 是棵可嵌套的树)。于是这里把它压平成一个
//! 可比较的 `group_id`,控制面只做「标签相等」的判断。
//!
//! # 动作那条缝:同步回执 + 超时(工单 03)
//!
//! 「起乐手」要建 pane —— 那是 gpui 主线程的活,而控制面在 HTTP 线程上,且 CLI
//! 正等着一个答复。于是走一条与 `RelayEvents` 形似、语义相反的路:
//!
//! ```text
//! 控制面(HTTP 线程) ──OrchestratorSignal + oneshot──→ 泵(主线程) ──建 pane
//!                  ←────────── recv_timeout ─────────────────────┘
//! ```
//!
//! - 中转那条是**异步回执**(`start_session` 丢进 channel 就返回,结果稍后经
//!   `start_session_result` 回给手机);这条是**同步等**:CLI 是个前台进程,
//!   它拿到的退出码就是回执本身。
//! - 用 `std::sync::mpsc::sync_channel(1)` 而不是 futures 的 oneshot:HTTP 线程
//!   是一条裸线程,没有执行器可以 `block_on`。
//! - **必须有超时**([`ACTION_TIMEOUT`]):泵没接线(纯单测 / 窗口已关)或主线程
//!   卡死时,不许把 HTTP 线程永久挂在那儿。超时答 `DesktopBusy` —— 注意那**不是**
//!   「没起成」,见下。
//!
//! # 超时之后:先记账,再谈回执
//!
//! 发起侧到点就走,泵这边却可能正把 pane 建到一半。两条防线各管一头:
//!
//! - **建之前先看时限**:信号里带一个 `deadline`(发起侧算好的绝对时刻),
//!   泵取到已经过期的信号就**直接丢弃、不建 pane** —— 队列积压时这是最省的止损,
//!   而且没人在等那个回执了。
//! - **建成之后先记账**:pane 真起来了就一定要落进控制面的记账
//!   (`StartSessionSpec::landed`,唯一的 [`StartedSession`] 构造路径),回执送不送
//!   得到与它无关。否则桌面上会长出一个不进 `list-panes`、不占名额、
//!   目标解析答「不存在」的幽灵乐手。
//!
//! 两条之间仍有一线窗口(判完时限、建 pane 期间发起侧刚好到点),那一档由「先记账」
//! 兜住:会话在,记账也在,编排者 `list-panes` 看得见。
//!
//! # 写穿走同一条泵(工单 05)
//!
//! `send` 也是主线程的活(写 PTY 要经 gpui 实体上的 [`AppStore::write_to_pane`]),
//! 于是它与起会话共用这一条泵、这一个时限、这一种超时结论。两者只有一处不同:
//! 起会话超时后**桌面上可能真多了一个会话**(所以有那套记账契约),写穿超时后
//! 什么实体都不会留下 —— 字节要么进了 PTY 要么没进。
//!
//! 装配(bracketed paste 包裹、换行归一)在**控制面那一侧**做好([`PaneInput`]),
//! 这里只挑一份:目标终端开着 bracketed paste 就写包裹版,没开就写裸版 ——
//! 与用户按 Ctrl+V 时 `mt_ui::terminal::input::paste_to_bytes` 读的是同一个模式位。

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedSender};
use gpui::{App, Entity, Task, Window};
use mt_ai::{
    AiSessionState, ControlLauncher, ControlProject, Delivered, OrchestratorActions,
    OrchestratorHost, PaneInput, PaneLiveness, SendFailure, StartFailure, StartSessionSpec,
    StartedSession, ACTION_TIMEOUT,
};
use mt_config::{AiLauncher, AppConfig, ProjectTreeItem};
use parking_lot::Mutex;

use crate::ai::AiBridge;
use crate::i18n::tr;
use crate::store::{AppStore, LaunchError, LaunchNotice, LaunchPlacement, LaunchRequest};

/// 起 PTY 时要不要给这个 pane 发一枚编排令牌。
///
/// 做成具名类型而不是裸 `bool`:起 PTY 的调用点有七八处,绝大多数是「不发」,
/// 一个 `false` 混进参数表里读不出是在说什么;也免得哪天顺手传成 `true`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrchestratorGrant {
    /// 普通 pane:什么都不注入。
    #[default]
    None,
    /// 编排者 pane:注入令牌 + 自身身份。
    Grant,
}

impl OrchestratorGrant {
    /// 授予与否**只看启动器上的那个开关** —— 这是唯一的信任根。
    pub fn from_launcher(launcher: &mt_config::AiLauncher) -> Self {
        if launcher.orchestration {
            Self::Grant
        } else {
            Self::None
        }
    }

    pub fn is_granted(self) -> bool {
        matches!(self, Self::Grant)
    }
}

/// 受编排会话拿到的授予:**恒为「不发」**(ADR 0003 的禁套娃)。
///
/// 具名成一个常量而不是在起会话那一行写 `OrchestratorGrant::None`,是为了让这条
/// 裁决有个能被测试指着说话的名字 —— 它与 [`OrchestratorGrant::from_launcher`]
/// 是**互斥**的两条路:授予只从「用户在桌面上勾了开关、并亲手/按配置起了那个
/// pane」来,编排者起的会话一律不走那条路,哪怕目标启动器自己勾了开关。
pub const ORCHESTRATED_GRANT: OrchestratorGrant = OrchestratorGrant::None;

// ─── 并发上限:配置里的那个数 → 控制面要的上限 ────────────────

/// 设置项能填到的最大值。
///
/// 上界不是防手滑,是**防把机器拖垮**:一个受编排会话 = 一条 PTY + 一个几百 MB
/// 的 agent 进程 + 它自己拉起的子进程,20 个已经远超一台开发机能同时供养的量。
/// 另有一条硬约束:它必须 ≤ [`mt_ai::MAX_SESSIONS_PER_ORCHESTRATOR`](记账表长度
/// 上限)—— 记账装不下全部**活着**的乐手时,「活着的一条都不能丢」那条不变式就
/// 被逼到墙角了。那条不等式由本模块的测试拿两侧真常量钉住。
pub const SESSION_CAP_MAX: u32 = 20;

/// 配置里那个 `Option<u32>` → 控制面要的 `usize`。
///
/// 三件事收在这一处(界面初值、提交后推给控制面、启动接线各调一次,口径必须同一份):
///
/// - `None` = **用户没设过**,兜底到 [`mt_ai::DEFAULT_SESSION_CAP`]。默认值只有
///   那一个常量,`mt-config` 不认识 `mt-ai`,所以兜底只能落在这一层;
/// - 越界的值钳回 `0..=SESSION_CAP_MAX`。config.db 是本程序自己写的,理论上填不进
///   越界值,但配置也可能是从旧 `config.json` 迁进来的 —— 钳一下比信它便宜;
/// - `0` 是**合法值**,语义是「暂停一切新的受编排会话」(见
///   [`mt_ai::ControlPlane::set_session_cap`]),不许被当成「没设过」吞掉。
pub fn resolve_session_cap(saved: Option<u32>) -> usize {
    saved.unwrap_or(mt_ai::DEFAULT_SESSION_CAP as u32).min(SESSION_CAP_MAX) as usize
}

/// 主线程按配置刷新、控制面 HTTP 线程只读的那一份桌面状态切面。
#[derive(Default)]
pub struct OrchestratorMirror {
    launchers: Vec<ControlLauncher>,
    projects: Vec<ControlProject>,
}

/// 镜像的共享句柄:[`AiBridge`] 持一份给刷新用,[`HostImpl`] 持一份给读。
///
/// [`AiBridge`]: crate::ai::AiBridge
pub type SharedMirror = Arc<Mutex<OrchestratorMirror>>;

impl OrchestratorMirror {
    /// 整体替换。**每次配置落盘都刷**(见 `store::layout::save_config_now`)——
    /// 「改分组即时生效」靠的就是这条,控制面自己每次请求现读镜像。
    pub fn replace(&mut self, config: &AppConfig) {
        self.launchers = launcher_facets(config);
        self.projects = project_facets(config);
    }
}

/// 注入给 `mt-ai` 的宿主实现。
pub struct HostImpl {
    mirror: SharedMirror,
}

impl HostImpl {
    pub fn new(mirror: SharedMirror) -> Self {
        Self { mirror }
    }
}

impl OrchestratorHost for HostImpl {
    fn launchers(&self) -> Vec<ControlLauncher> {
        self.mirror.lock().launchers.clone()
    }

    fn projects(&self) -> Vec<ControlProject> {
        self.mirror.lock().projects.clone()
    }
}

// ─── 动作:起乐手 / 查死活 ────────────────────────────────────

/// 发起侧不再等待的那个时刻。
///
/// 用**绝对时刻**而不是「还剩多久」:信号在队列里排的那段时间也要算进去。
/// gpui 主线程上照样可以读 [`Instant`]。
#[derive(Debug, Clone, Copy)]
struct Deadline(Instant);

impl Deadline {
    /// 从此刻起再等 `patience`。
    fn after(patience: Duration) -> Self {
        Self(Instant::now() + patience)
    }

    /// 已经没人等这个回执了吗。
    fn passed(self) -> bool {
        Instant::now() >= self.0
    }
}

/// 控制面递给主线程的活。**唯一**的跨线程口。
enum OrchestratorSignal {
    StartSession {
        spec: StartSessionSpec,
        /// 回执通道。容量 1:主线程放完就走,不等对面取。
        reply: std::sync::mpsc::SyncSender<Result<StartedSession, StartFailure>>,
        /// 发起侧到这个时刻就不等了(= 它发信号那一刻 + [`ACTION_TIMEOUT`])。
        /// 泵在**动手之前**看一眼,过期了就整条丢掉,连 pane 都不建。
        deadline: Deadline,
    },
    /// 把一段输入写穿进某个乐手(工单 05)。写 PTY 要经 gpui 实体上的
    /// [`AppStore::write_to_pane`],所以同样是主线程的活。
    SendInput {
        /// 乐手的 PTY 编号(控制面已按可见范围铁律裁决过)。
        pane_id: u32,
        /// 已经装配好的两份字节,挑哪份看目标终端的真实粘贴模式。
        input: PaneInput,
        reply: std::sync::mpsc::SyncSender<Result<Delivered, SendFailure>>,
        deadline: Deadline,
    },
}

impl OrchestratorSignal {
    /// 发起侧的耐心到点没有。
    ///
    /// **穷尽 match、没有 `_` 兜底**:加一种新信号时这里编译不过,免得它悄悄绕过
    /// 泵开头那道时限闸 —— 绕过去就是「没人等的活照做」,起会话那条还会长出
    /// 无人认领的 pane。
    fn deadline(&self) -> Deadline {
        match self {
            Self::StartSession { deadline, .. } | Self::SendInput { deadline, .. } => *deadline,
        }
    }
}

/// 注入给 `mt-ai` 的动作实现。
struct ActionsImpl {
    ai: AiBridge,
    tx: UnboundedSender<OrchestratorSignal>,
}

impl OrchestratorActions for ActionsImpl {
    /// 把活丢给主线程,**同步等**一个答复(见模块注释)。
    fn start_session(&self, spec: StartSessionSpec) -> Result<StartedSession, StartFailure> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        // 时限由**发起侧**算:等的人是它,泵那边只照着这个时刻判要不要动手。
        let deadline = Deadline::after(ACTION_TIMEOUT);
        if self
            .tx
            .unbounded_send(OrchestratorSignal::StartSession {
                spec,
                reply,
                deadline,
            })
            .is_err()
        {
            // 泵没了(窗口关了 / 还没接线):不许把「没人接」当成起成了
            return Err(StartFailure::DesktopBusy);
        }
        answer
            .recv_timeout(ACTION_TIMEOUT)
            .unwrap_or(Err(StartFailure::DesktopBusy))
    }

    /// 写穿同样要回主线程(`AppStore::write_to_pane` 挂在 gpui 实体上),
    /// 于是与起会话**同一条路**:同一个泵、同一个时限、同一种超时结论。
    fn send_input(&self, pane_id: u32, input: PaneInput) -> Result<Delivered, SendFailure> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        let deadline = Deadline::after(ACTION_TIMEOUT);
        if self
            .tx
            .unbounded_send(OrchestratorSignal::SendInput {
                pane_id,
                input,
                reply,
                deadline,
            })
            .is_err()
        {
            return Err(SendFailure::DesktopBusy);
        }
        answer
            .recv_timeout(ACTION_TIMEOUT)
            .unwrap_or(Err(SendFailure::DesktopBusy))
    }

    /// 死活**不跳主线程** —— 名额判定一次要问好几个 pane,而这几样东西后台线程
    /// 本来就读得到:
    ///
    /// - `alive` 走 [`AiBridge`] 的活 pane 名册(建 PTY 时登记、pane 关闭时注销,
    ///   与 500ms 轮询同一份);
    /// - `status` 走 `AiPerception`(hook 状态与旁路检测都在 `Arc<Mutex<..>>` 后面);
    /// - 「在不在 AI 会话里」见 [`ai_session_state`] —— 那一问有**答不上来**的时候。
    fn pane_liveness(&self, pane_id: u32) -> PaneLiveness {
        let status = self.ai.perception().status_of(pane_id);
        let ai_session = ai_session_state(&self.ai, pane_id, &status);
        PaneLiveness {
            alive: self.ai.is_pane_live(pane_id),
            status,
            ai_session,
        }
    }
}

/// 「这个 pane 还在 AI 会话里吗」的宿主侧判据,**fail-closed 三态**。
///
/// 判据只读现有的两样事实,一个字都不动 AI 状态机本身
/// (`monitor::resolve_status` 那条链是本仓的权威通道,不给编排让路):
///
/// - 状态不是裸 `idle` → [`AiSessionState::Active`]。hook 说 `ai-*` 也好,
///   输入检测认出来了也好,都是「在」的正面证据。
/// - 状态是 `idle`,但这个 pane 上 **hook 已启用** → [`AiSessionState::Ended`]。
///   hook 一旦启用即为权威,`idle` 就是 SessionEnd 落地了(或停摆兜底判了已退出)
///   —— 这一档才真正把名额还回来。
/// - 其余 → [`AiSessionState::Unknown`],**按占着名额算**。这一档长这样:
///   自定义启动器的命令不在 `mt_ai::AI_COMMANDS` 里(ADR 0003 明说任何启动器都能
///   当乐手)、又没有 hook,于是 `resolve_status` 恒答 `idle`。按「不占」算的话
///   硬上限就是摆设,可以无限起;按「占」算最坏是少起一个,编排者会收到明确的
///   `sessionLimitReached`,它自己排队 —— 两害相权取轻。
///
/// ⚠️ 代价记在工单 03 的留档里:无 hook 的 agent(opencode/pi 之流)自行退出后
/// 名额不会释放,得等用户把那个 pane 关掉。修它要动降级状态机,不在本轮范围。
fn ai_session_state(ai: &AiBridge, pane_id: u32, status: &str) -> AiSessionState {
    if status != "idle" {
        AiSessionState::Active
    } else if ai.perception().hooks().is_hook_enabled(pane_id) {
        AiSessionState::Ended
    } else {
        AiSessionState::Unknown
    }
}

/// 接上动作那条缝并起主线程泵。返回的 [`Task`] 要被宿主持有 —— 丢了句柄泵就没了,
/// 之后所有起会话请求都会走成 `DesktopBusy`。
///
/// 与 `mobile_relay::install` 的差别:那边有面板、观察者、去抖同步,得有个实体
/// 装着;这边只有一条泵,`window.spawn` 足够,不必为它造一个空壳实体。
pub fn install(store: Entity<AppStore>, window: &mut Window, cx: &mut App) -> Task<()> {
    let (tx, mut rx) = mpsc::unbounded::<OrchestratorSignal>();
    let ai = store.read(cx).ai();
    ai.perception()
        .control()
        .set_actions(Arc::new(ActionsImpl {
            ai: ai.clone(),
            tx,
        }));

    window.spawn(cx, async move |cx| {
        while let Some(signal) = rx.next().await {
            // **动手之前先看时限**:发起侧早走了就整条丢掉,连 pane 都不建 ——
            // 起出来也没人认领(见模块注释)。队列积压时这是最省的止损。
            // 判据对每一种信号都成立,所以在分发**之前**问(见
            // [`OrchestratorSignal::deadline`] 那条穷尽 match)。
            if signal.deadline().passed() {
                continue;
            }
            match signal {
                OrchestratorSignal::StartSession { spec, reply, .. } => {
                    // 窗口没了就别再答复:发起方会在 `ACTION_TIMEOUT` 上收敛成 DesktopBusy
                    let Ok(result) = cx.update(|window, cx| start_session(&store, spec, window, cx))
                    else {
                        return;
                    };
                    // 对面可能已经等超时走了(`SyncSender` 于是报错),不是问题 ——
                    // 记账在 `spec.landed()` 里已经落地,回执丢了也不会长出幽灵乐手。
                    let _ = reply.send(result);
                }
                OrchestratorSignal::SendInput {
                    pane_id,
                    input,
                    reply,
                    ..
                } => {
                    let Ok(result) = cx.update(|_window, cx| send_input(&store, pane_id, input, cx))
                    else {
                        return;
                    };
                    // 回执丢了这条**没有**起会话那种「桌面上多了个东西」的后果:
                    // 字节要么进了 PTY 要么没进,不留任何要记账的实体。
                    let _ = reply.send(result);
                }
            }
        }
    })
}

/// 主线程上真正把乐手起出来。
///
/// 落地动作整套走[共享入口](AppStore::launch_ai_session) —— 与桌面端新终端菜单、
/// 移动端发起是同一条。这里只负责三件编排者自己的事:按 id 把启动器取出来、
/// 挑 `Background` 落点(ADR 0002 的出生礼仪)、把结局折成控制面的闭集。
fn start_session(
    store: &Entity<AppStore>,
    spec: StartSessionSpec,
    window: &mut Window,
    cx: &mut App,
) -> Result<StartedSession, StartFailure> {
    // 按 id 取具名启动器。**命令文本只在这里到共享入口之间的几行里流转** ——
    // 它没进过控制面,更没到过编排者手上(ADR 0002 的唯一防线)。
    let launcher = store
        .read(cx)
        .mobile_relay()
        .launchers
        .iter()
        .find(|l| l.id == spec.launcher_id())
        .cloned();
    // 编排者自己那个 pane 的 tab 标题:记账要抄一份,好在编排者离场之后还说得出
    // 「是谁起的」(工单 04)。**必须在这儿查** —— 控制面不认识布局树,而离场之后
    // 那个 pane 就没了,现查只会查到空。查不到就传空串,展示侧兜「未知编排者」。
    let orchestrator_label = store
        .read(cx)
        .pane_label_by_pty(spec.orchestrator_pane_id())
        .unwrap_or_default();
    // 控制面刚查过名单,到这儿还能没了只有一种可能:用户正巧把它删了。
    let Some(launcher) = launcher else {
        return Err(StartFailure::SpawnFailed);
    };

    let message = tr!(
        "app",
        "orchestratorStartSession",
        launcher = launcher.name.clone()
    );
    let outcome = store
        .update(cx, |store, cx| {
            store.launch_ai_session(
                orchestrated_launch_request(spec.project_id(), &launcher, message),
                window,
                cx,
            )
        })
        .map_err(|err| match err {
            LaunchError::ProjectNotFound => StartFailure::ProjectGone,
            LaunchError::SpawnFailed => StartFailure::SpawnFailed,
        })?;

    // pane 建成了但启动命令没交到一根活着的 PTY 手上 —— 对编排者而言这就是失败
    // (pane 本身**保留不杀**,用户回头能看到它卡在哪)。与移动端回执同一判据。
    //
    // **这一档刻意不登记记账**:那儿只是一个裸终端,没有 AI 在跑;登记了它会以
    // 「AI 会话状态不可知」永久占着一个名额(见 `ai_session_state`),比不登记更坏。
    if !outcome.command_delivered() {
        return Err(StartFailure::SpawnFailed);
    }
    // 对外的 pane 身份一律是 **PTY 编号**:编排者自己的身份也是它
    // (`MINITERM_ORCHESTRATOR_PANE`),自指禁令因此是一次裸比较。
    //
    // 编号随 `LaunchOutcome` 一起回来,不回头再查一次布局树:`command_delivered()`
    // 为真已经蕴含它在场(两者出自同一次查找),那条「命令写进去了却查不到编号」
    // 的窄口子因此不存在 —— 它正是幽灵乐手的另一个入口。
    let Some(pane_id) = outcome.pty_id else {
        return Err(StartFailure::SpawnFailed);
    };
    // **先记账,再谈回执**:`landed` 把这条乐手写进控制面的范围记账,并且是
    // `StartedSession` 唯一的构造路径。发起侧就算已经超时走人,桌面上这个真实
    // 存在的受编排会话照样进 `list-panes`、照样占名额、照样能被点名。
    Ok(spec.landed(pane_id, &orchestrator_label))
}

/// 主线程上真正把那段输入写进乐手。
///
/// **整条走既有的写穿入口**[`AppStore::write_to_pane`] —— 与用户自己在那个终端
/// 上粘贴并回车是同一段代码，输入跟踪 / AI marker / attention 黄灯清除 /
/// SSH autofill 解除全挂在它下游（`TerminalPane::write` → `PtySession::write`），
/// 绕过去就全丢了（`store::launch` 第 5 步那条纪律的同一条红线）。移动端指令
/// 走的也是这一条 —— 工单要的「语义与移动端完全一致」就是**同一个函数**，
/// 不是另一个长得像的。
///
/// 只做两件本地判断：把 PTY 编号翻回「项目 + pane」（写穿入口按那个点名），
/// 以及问一句目标终端此刻开没开 bracketed paste。
fn send_input(
    store: &Entity<AppStore>,
    pane_id: u32,
    input: PaneInput,
    cx: &mut App,
) -> Result<Delivered, SendFailure> {
    // 一个共享借用里连问两句:`Entity::read` 的返回值会把借用持到语句结束,
    // 拿 `&mut App` 直接连着 read 两次借用检查过不去。
    let app: &App = cx;
    let Some((project_id, pane)) = store.read(app).pane_of_pty(pane_id) else {
        // 控制面裁决时它还活着,到这儿没了 = 用户刚好把它关掉。
        return Err(SendFailure::PaneGone);
    };
    let bracketed_paste = store.read(app).pane_bracketed_paste(pane_id, app);

    let written = store.update(cx, |store, cx| {
        store.write_to_pane(&project_id, &pane, input.bytes(bracketed_paste), cx)
    });
    if !written {
        // `write_to_pane` 只在「找不到那个终端实体」时答 false —— 与上面那一档
        // 同因异形(布局树里还有这个 pane,但它的 PTY 已经不在了)。
        return Err(SendFailure::WriteFailed);
    }
    Ok(Delivered { bracketed_paste })
}

/// 「受编排会话」那份落地请求的**唯一**构造处。
///
/// 抽成不碰 `window` / `cx` 的纯函数,是为了让[禁套娃的实际防线](ORCHESTRATED_GRANT)
/// 有个能被单测指着说话的调用点 —— 「类型上没有那个位」只保证了控制面到桌面这一段,
/// 真正决定发不发令牌的是下面 `grant:` 那一行。
fn orchestrated_launch_request<'a>(
    project_id: &'a str,
    launcher: &'a AiLauncher,
    message: String,
) -> LaunchRequest<'a> {
    LaunchRequest {
        project_id,
        launcher_name: &launcher.name,
        shell_name: launcher.shell.as_deref(),
        command: &launcher.command,
        // 挂进活动面板最左侧叶子:不激活、不抢焦点、不切项目
        placement: LaunchPlacement::Background,
        // **禁套娃**:受编排会话一律不授予编排能力,哪怕这个启动器自己勾了
        // 「允许编排」—— 那个开关是「谁能当编排者」的授予位,只对用户在桌面上
        // 亲手起的那条路生效(`OrchestratorGrant::from_launcher`)。
        grant: ORCHESTRATED_GRANT,
        // 诞生一次性提示:与移动端发起同一档 toast(info 图标 + 点击切项目),
        // 只是文案说明出身。凭证被盗时这是唯一的审计迹象,所以即便不切过去也要弹。
        notice: Some(LaunchNotice {
            kind: crate::notify::ToastKind::MobileSession,
            message,
        }),
    }
}

/// 启动器投影:**全量**(任何启动器都能当乐手,ADR 0003),只留 id 与展示名。
///
/// 「允许编排」那个开关**不投影** —— 它是「谁能当编排者」的授予位,不是
/// 「谁能被起」的过滤位;而受编排会话一律不发令牌(禁套娃,工单 03),
/// 编排者知道这个位也没有用处。
pub fn launcher_facets(config: &AppConfig) -> Vec<ControlLauncher> {
    config
        .mobile_relay
        .as_ref()
        .map(|r| r.launchers.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|l| ControlLauncher {
            id: l.id.clone(),
            name: l.name.clone(),
        })
        .collect()
}

/// 项目投影:每个项目配一个**已解析好的所属分组 id**。
///
/// 三条口径:
///
/// - 嵌套分组取**最内层**那个(最具体的那次「这是一件事」的表达);
/// - 不在项目树里的项目(顶层 / 异常配置)是未分组 —— 只能在本项目内编排;
/// - 子项目(worktree「设为项目」,不进项目树)**继承父项目的分组** ——
///   它与父项目本来就是同一件事,否则从 worktree 里起的编排者够不到主仓项目。
pub fn project_facets(config: &AppConfig) -> Vec<ControlProject> {
    let mut groups: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(tree) = config.project_tree.as_ref() {
        collect_groups(tree, None, &mut groups);
    }

    config
        .projects
        .iter()
        .map(|p| ControlProject {
            id: p.id.clone(),
            name: p.name.clone(),
            path: p.path.clone(),
            group_id: resolve_group(config, &groups, &p.id),
            // **照实投影**,不在这里先折成一个「能不能起乐手」的布尔:
            // 那是控制面的裁决(`ControlProject::is_remote`),配置层只管说事实。
            // 与 `mobile_relay::to_relay_project` 同一个字段、同一份事实。
            ssh_connection_id: p.ssh_connection_id.clone(),
        })
        .collect()
}

/// 深度优先压平项目树:每个项目 id → 最内层分组 id。
fn collect_groups(
    items: &[ProjectTreeItem],
    current: Option<&str>,
    out: &mut std::collections::HashMap<String, String>,
) {
    for item in items {
        match item {
            ProjectTreeItem::Group(group) => {
                collect_groups(&group.children, Some(&group.id), out);
            }
            ProjectTreeItem::ProjectId(id) => {
                if let Some(group) = current {
                    out.entry(id.clone()).or_insert_with(|| group.to_string());
                }
            }
        }
    }
}

/// 项目自己的分组;没有就沿 `parent_project_id` 往上找(子项目继承父项目)。
///
/// 上溯**有上限**:配置是用户可编辑的数据,环状 `parentProjectId` 不该把
/// 控制面转死在这里。
fn resolve_group(
    config: &AppConfig,
    groups: &std::collections::HashMap<String, String>,
    project_id: &str,
) -> Option<String> {
    const MAX_DEPTH: usize = 8;
    let mut id = project_id.to_string();
    for _ in 0..MAX_DEPTH {
        if let Some(group) = groups.get(&id) {
            return Some(group.clone());
        }
        let parent = config
            .projects
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.parent_project_id.clone())?;
        if parent == id {
            return None;
        }
        id = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_config::{MobileRelayConfig, ProjectGroup};

    fn project(id: &str) -> mt_config::ProjectConfig {
        mt_config::ProjectConfig {
            id: id.to_string(),
            name: format!("项目{id}"),
            path: format!("D:\\repos\\{id}"),
            description: None,
            saved_layout: None,
            expanded_dirs: Vec::new(),
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: Vec::new(),
            wsl_sessions_distro: None,
            ssh_connection_id: None,
            parent_project_id: None,
            kind_override: None,
        }
    }

    fn config(
        projects: Vec<mt_config::ProjectConfig>,
        tree: Option<Vec<ProjectTreeItem>>,
    ) -> AppConfig {
        AppConfig {
            projects,
            project_tree: tree,
            ..AppConfig::default()
        }
    }

    fn group(id: &str, children: Vec<ProjectTreeItem>) -> ProjectTreeItem {
        ProjectTreeItem::Group(ProjectGroup {
            id: id.to_string(),
            name: format!("组{id}"),
            collapsed: false,
            children,
        })
    }

    fn leaf(id: &str) -> ProjectTreeItem {
        ProjectTreeItem::ProjectId(id.to_string())
    }

    #[test]
    fn 顶层项目未分组() {
        let c = config(vec![project("a"), project("b")], Some(vec![leaf("a"), leaf("b")]));
        let facets = project_facets(&c);
        assert!(facets.iter().all(|p| p.group_id.is_none()));
        assert_eq!(facets[0].path, "D:\\repos\\a");
    }

    #[test]
    fn 同组项目拿到同一个分组_id() {
        let c = config(
            vec![project("a"), project("b"), project("c")],
            Some(vec![group("g1", vec![leaf("a"), leaf("b")]), leaf("c")]),
        );
        let facets = project_facets(&c);
        let by = |id: &str| {
            facets
                .iter()
                .find(|p| p.id == id)
                .unwrap()
                .group_id
                .clone()
        };
        assert_eq!(by("a"), Some("g1".into()));
        assert_eq!(by("a"), by("b"));
        assert_eq!(by("c"), None);
    }

    /// 嵌套分组取**最内层**:更具体的那次表达说了算。
    #[test]
    fn 嵌套分组取最内层() {
        let c = config(
            vec![project("a"), project("b")],
            Some(vec![group(
                "outer",
                vec![leaf("a"), group("inner", vec![leaf("b")])],
            )]),
        );
        let facets = project_facets(&c);
        assert_eq!(facets[0].group_id, Some("outer".into()));
        assert_eq!(facets[1].group_id, Some("inner".into()));
    }

    /// 不在树里的项目(异常配置兜底)是未分组,不该凭空并进某个组。
    #[test]
    fn 树外项目未分组() {
        let c = config(vec![project("a"), project("ghost")], Some(vec![group("g1", vec![leaf("a")])]));
        let facets = project_facets(&c);
        assert_eq!(facets[1].group_id, None);
    }

    /// 子项目(worktree「设为项目」)继承父项目的分组 —— 它不进项目树。
    #[test]
    fn 子项目继承父项目分组() {
        let mut child = project("child");
        child.parent_project_id = Some("a".into());
        let c = config(
            vec![project("a"), child],
            Some(vec![group("g1", vec![leaf("a")])]),
        );
        let facets = project_facets(&c);
        assert_eq!(facets[1].group_id, Some("g1".into()));
    }

    /// 环状 parentProjectId(用户手改配置)不许把控制面转死。
    #[test]
    fn 父子成环不死循环() {
        let mut a = project("a");
        a.parent_project_id = Some("b".into());
        let mut b = project("b");
        b.parent_project_id = Some("a".into());
        let c = config(vec![a, b], None);
        assert!(project_facets(&c).iter().all(|p| p.group_id.is_none()));
    }

    /// 启动器投影是全量的,且**只带 id 与名字**(命令不给编排者看)。
    #[test]
    fn 启动器投影全量且不带命令() {
        let c = AppConfig {
            mobile_relay: Some(MobileRelayConfig {
                relay_url: String::new(),
                desktop_key: String::new(),
                launchers: vec![
                    AiLauncher {
                        id: "l1".into(),
                        name: "Claude".into(),
                        shell: Some("wsl".into()),
                        command: "claude --dangerously".into(),
                        orchestration: true,
                    },
                    AiLauncher {
                        id: "l2".into(),
                        name: "Codex".into(),
                        shell: None,
                        command: "codex".into(),
                        orchestration: false,
                    },
                ],
            }),
            ..AppConfig::default()
        };
        let facets = launcher_facets(&c);
        assert_eq!(facets.len(), 2, "没勾「允许编排」的也能当乐手");
        assert_eq!(facets[0].id, "l1");
        assert_eq!(facets[0].name, "Claude");
        // 类型上就没有命令字段,这里钉的是「别哪天顺手加回去」
        let json = serde_json::to_string(&serde_json::json!({
            "id": facets[0].id,
            "name": facets[0].name,
        }))
        .unwrap();
        assert!(!json.contains("dangerously"));
    }

    /// 项目投影**照实带**上 SSH 连接 id,远程项目据此当不了乐手宿主。
    ///
    /// 这条判据在工单 02 之前是「全仓没有任何项目带它」的状态,与
    /// `mobile_relay` 那条 `远程项目镜像照实带连接id且不可发起会话` 同源 ——
    /// 投影不老实,控制面的裁决就是空转。
    #[test]
    fn 远程项目投影照实带连接id且不能当宿主() {
        let mut remote = project("r");
        remote.ssh_connection_id = Some("conn-1".into());
        let c = config(vec![project("a"), remote], None);
        let facets = project_facets(&c);

        assert_eq!(facets[0].ssh_connection_id, None);
        assert!(!facets[0].is_remote());
        assert_eq!(facets[1].ssh_connection_id.as_deref(), Some("conn-1"));
        assert!(facets[1].is_remote(), "SSH 远程项目当不了乐手宿主");
    }

    /// **禁套娃**:受编排会话拿到的授予恒为「不发」,与启动器开关那条路互斥。
    #[test]
    fn 受编排会话一律不授予编排能力() {
        // 哪怕目标启动器自己勾了「允许编排」:那个开关只对「用户在桌面上亲手
        // 起的那条路」生效(`from_launcher`),编排者起会话走的是那个常量。
        let orchestrating = orchestrating_launcher();
        assert!(OrchestratorGrant::from_launcher(&orchestrating).is_granted());
        assert!(
            !ORCHESTRATED_GRANT.is_granted(),
            "乐手不许继承编排能力,否则禁套娃只剩君子协定"
        );
    }

    fn orchestrating_launcher() -> AiLauncher {
        AiLauncher {
            id: "l".into(),
            name: "Claude".into(),
            shell: Some("wsl".into()),
            // 命令文本只在桌面端进程内流转,这里顺带钉一下它不外泄
            command: "claude --dangerously".into(),
            orchestration: true,
        }
    }

    /// **禁套娃的实际防线钉在调用点上**,而不是钉在「那个常量等于 None」这句
    /// 套套逻辑上:真正决定发不发令牌的是落地请求里 `grant:` 那一行,
    /// 把它改成 `from_launcher(&launcher)` 这条测试就红。
    #[test]
    fn 受编排会话的落地请求恒不授予编排能力() {
        let launcher = orchestrating_launcher();
        let req = orchestrated_launch_request("p-self", &launcher, "起了一个".into());

        assert_eq!(
            req.grant,
            OrchestratorGrant::None,
            "启动器勾了「允许编排」也不许把令牌传给乐手"
        );
        assert!(!req.grant.is_granted());
        // 出生礼仪(ADR 0002):不抢焦点、不切项目,且必弹一次诞生提示
        assert_eq!(req.placement, LaunchPlacement::Background);
        assert!(req.notice.is_some(), "凭证被盗时这是唯一的审计迹象");
        // 命令与 shell 照配置取,项目照裁决结果落
        assert_eq!(req.project_id, "p-self");
        assert_eq!(req.command, "claude --dangerously");
        assert_eq!(req.shell_name, Some("wsl"));
        assert_eq!(req.launcher_name, "Claude");
    }

    // ─── 写穿装配的跨 crate 对账(工单 05)────────────────────────

    /// **编排者的写穿与用户按 Ctrl+V 装配出同一串字节**。
    ///
    /// 装配有两份实现,隔着 crate 边界:粘贴那条在 `mt_ui::paste_to_bytes`
    /// (它吃 `TermMode`,而 `mt-ai` 不依赖 `mt-terminal`/`gpui`,搬不过去),
    /// 写穿那条在 `mt_ai::PaneInput`(装配在控制面才让主缝测试断言得到)。
    /// **`mt-app` 是唯一同时看得见两侧的地方**,于是这条对账住在这里 ——
    /// 少了它,哪天有人只改一侧的换行口径,编排者发的多行 prompt 会与用户
    /// 亲手粘的表现不一样,而两边的单测各自照绿。
    ///
    /// 唯一的差别是**末尾那个回车**:粘贴不按回车(用户自己按),写穿要按。
    #[test]
    fn 写穿装配与用户粘贴同口径() {
        use mt_terminal::alacritty_terminal::term::TermMode;
        use mt_ui::paste_to_bytes;

        for text in [
            "单行",
            "第一行\n第二行",
            "混着\r\n两种\n换行",
            "带代码块:\n```rust\nfn main() {}\n```",
        ] {
            let input = PaneInput::assemble(text).expect("非空正文");

            let pasted = paste_to_bytes(text, TermMode::BRACKETED_PASTE);
            assert_eq!(
                input.bracketed(),
                format!("{}\r", String::from_utf8(pasted).unwrap()),
                "text={text:?}: 包裹版与用户粘贴的字节不一致"
            );

            let pasted = paste_to_bytes(text, TermMode::empty());
            assert_eq!(
                input.plain(),
                format!("{}\r", String::from_utf8(pasted).unwrap()),
                "text={text:?}: 裸版与用户粘贴的字节不一致"
            );
        }
    }

    /// 发起侧已经不等了的信号,泵**动手之前**就该丢掉 —— 建出来的乐手没人认领。
    #[test]
    fn 时限过了就不该再动手() {
        assert!(
            Deadline(Instant::now() - Duration::from_millis(1)).passed(),
            "过了时限必须认得出来"
        );
        assert!(
            !Deadline::after(ACTION_TIMEOUT).passed(),
            "刚发出的信号不许被误丢"
        );
        // 时限是**绝对时刻**:在队列里排的时间也算,不是「拿到手才开始数」
        let d = Deadline::after(Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(60));
        assert!(d.passed());
    }

    /// 授予与否只看启动器上的开关,不看名字、命令或别的任何东西。
    #[test]
    fn 授予只认启动器开关() {
        let mut l = AiLauncher {
            id: "l".into(),
            name: "Claude".into(),
            shell: None,
            command: "claude".into(),
            orchestration: false,
        };
        assert_eq!(OrchestratorGrant::from_launcher(&l), OrchestratorGrant::None);
        assert!(!OrchestratorGrant::from_launcher(&l).is_granted());

        l.orchestration = true;
        assert!(OrchestratorGrant::from_launcher(&l).is_granted());
        // 缺省态必须是「不发」
        assert!(!OrchestratorGrant::default().is_granted());
    }

    #[test]
    fn 镜像整体替换() {
        let mut mirror = OrchestratorMirror::default();
        let c = config(vec![project("a")], None);
        mirror.replace(&c);
        assert_eq!(mirror.projects.len(), 1);

        mirror.replace(&AppConfig::default());
        assert!(mirror.projects.is_empty(), "整体替换,不许留旧项目");
    }

    // ─── 并发上限(工单 08) ───────────────────────────────────

    /// 没设过 → 默认值;设过 → 照用;0 是合法值不是「没设过」。
    #[test]
    fn 上限没设过时用默认值() {
        assert_eq!(resolve_session_cap(None), mt_ai::DEFAULT_SESSION_CAP);
        assert_eq!(resolve_session_cap(Some(3)), 3);
        assert_eq!(
            resolve_session_cap(Some(0)),
            0,
            "0 = 暂停一切新的受编排会话,不许被兜底成默认值"
        );
        // 存量配置的迁移路径:缺这个键就是 None
        assert_eq!(
            resolve_session_cap(AppConfig::default().orchestrator_session_cap),
            mt_ai::DEFAULT_SESSION_CAP
        );
    }

    /// 越界的值钳回上界(配置可能是从旧 config.json 迁进来的,别信它)。
    #[test]
    fn 上限越界被钳回上界() {
        assert_eq!(
            resolve_session_cap(Some(u32::MAX)),
            SESSION_CAP_MAX as usize
        );
        assert_eq!(
            resolve_session_cap(Some(SESSION_CAP_MAX)),
            SESSION_CAP_MAX as usize
        );
    }

    /// 上界必须留在记账表长度之内 —— 拿**两侧真常量**比，不是抄字面量。
    ///
    /// 反过来(上限 > 记账上限)时记账装不下全部活着的乐手,而修剪只丢已经关掉的、
    /// 「活着的一条都不能丢」,那条不变式会被逼到墙角。
    #[test]
    fn 上限的上界不超过记账表长度() {
        assert!(
            SESSION_CAP_MAX as usize <= mt_ai::MAX_SESSIONS_PER_ORCHESTRATOR,
            "并发上限的上界 {} 超过了每编排者记账上限 {}",
            SESSION_CAP_MAX,
            mt_ai::MAX_SESSIONS_PER_ORCHESTRATOR
        );
        assert!(
            mt_ai::DEFAULT_SESSION_CAP <= SESSION_CAP_MAX as usize,
            "默认值得落在设置项填得出来的范围里"
        );
    }

    /// 设置项说明里那个取值范围是**手写进文案**的,改了 [`SESSION_CAP_MAX`]
    /// 却忘了改文案时在这儿红一次(双语各查一遍)。
    #[test]
    fn 上限文案与取值范围对得上() {
        let max = SESSION_CAP_MAX.to_string();
        for locale in [mt_i18n::Locale::Zh, mt_i18n::Locale::En] {
            let desc = mt_i18n::t_in(locale, "settings", "aiHook.sessionCapDesc");
            assert!(
                desc.contains(&max),
                "{locale:?} 的说明里没提到上界 {max}: {desc}"
            );
            // 术语纪律:用户可见面一律「受编排会话 / orchestrated session」
            assert!(
                !desc.contains("乐手") && !desc.to_lowercase().contains("musician"),
                "{locale:?} 的用户可见文案不许出现口语别名: {desc}"
            );
        }
    }
}
