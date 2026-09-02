//! 汇报账本（ADR 0004）：把受编排会话的状态事实折成「汇报」，按编排者攒收件箱。
//!
//! # 为什么它是一块**纯**状态机
//!
//! 投递那一侧（工单 12）做的事全是有副作用的：查编排者的投递闸、读 transcript、
//! 跳主线程写字节、按秒级节拍重试。要是把「什么时候该出一条汇报」也塞进去，这
//! 段判定就只能在一条后台线程 + 真实时钟上验证——而它恰恰是最该逐条钉死的一块：
//! 一次误报等于编排者拿着假事实继续派活，一次漏报等于它永远等不到回音。
//!
//! 于是这里 **不认识 [`crate::control::ControlPlane`]、不起线程、不做 I/O、一次
//! `Instant::now()` 都不调**——时间一律由调用方传进来，15 秒的未接收超时在测试里
//! 拨一行就过去了。整张票交付时没有任何调用方，全部价值在本文件末尾的单测里。
//!
//! # 三种事实进，五种汇报出
//!
//! 进来的事实只有三样：受编排会话的**状态变化**（`status` + `cause`，与
//! [`crate::monitor::StatusChange`] 同一口径）、**pane 关闭**、**派活写入**。
//! 出去的是 [`ReportKind`] 那五档，按编排者攒进各自的收件箱等 12 来取。
//!
//! # 判定口径全部沿用既有状态机，一条新启发式都不加
//!
//! - **回合** = 进入 `ai-working` → 转到**非 attention 成因**的 `ai-idle`。成因原文
//!   照带（`Stop` 是真干完，`Interrupt` 是用户按了 Esc，`Stall` 是停摆兜底收敛的
//!   ——编排者得分得开，见 [`crate::control::PaneLiveness::cause`] 的论证）。
//! - **attention 看成因、不看状态**：Claude 的 `PermissionRequest` 落在 `ai-idle`，
//!   Codex 的落在 `ai-working`（`hook_server::map_event_to_status` 有专门一条）。
//!   只看状态的话，一个正等着 Codex 审批的乐手会被当成「还在跑」，而那恰恰是最
//!   该立刻告诉人的一档。判据直接用现成的 [`is_attention_cause`]。
//! - **退出**是 `ai-*` → `idle`，成因原文可能是 `SessionEnd`（权威退出信号）也可能
//!   是 `StallExit`（停摆兜底判定已退出，见 `monitor::stall_settle_target`）。
//!
//! # 与停摆兜底同一条铁律：**结论落盘、触发一次即收敛**
//!
//! v0.9.3 那版无记忆兜底让假完成每 20~50s 重复播报一次，代价记在
//! `crates/mt-ai/src/lib.rs` 的红线里。这里的每一条状态都是一次性的：回合结束后
//! `turn` 置空，下一次 `ai-idle` 不会再报；未被接收的等待一旦超时就摘掉，不会每
//! 拍重发；`Closed` 之后该 pane 的一切事实一律忽略。**没有任何一条判定会在同一
//! 份事实上反复得出同一个结论。**

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::hook_server::is_attention_cause;

/// 派活写入之后，等多久没见受编排会话开始处理就判「未被接收」。
///
/// ADR 0004：桌面端**不**承诺一个任务对应一个回合，只判定一件事——写入之后对方
/// 有没有开始处理新输入。15 秒这个数两头夹出来：下界是「一段 prompt 从写进 PTY
/// 到 agent 抬手」正常要多久（bracketed paste 落地 + agent 读到 + 发出
/// `UserPromptSubmit`，本机是百毫秒量级，15 秒是它的两个数量级以上，不会把「慢
/// 了一点」误报成「没收到」）；上界是「编排者干等多久还不算离谱」——它此刻正
/// 阻塞在自己那一轮的末尾等汇报，超过半分钟就该给它一个明确的说法。
const ACK_TIMEOUT: Duration = Duration::from_secs(15);

/// 单个编排者收件箱里最多攒几条汇报。
///
/// 溢出丢**最旧**的并累加 `dropped`，取批次时把丢弃条数一并交出去（12 在渲染的
/// 批头里注明「丢了几条」）。丢新的不行：最新那几条才是编排者最需要的现状。
///
/// 50 条的量级：编排者忙着跑自己那一轮（`ai-working`）时汇报只攒不投，而一个
/// 编排者名下的受编排会话数本就有硬上限（`MAX_SESSIONS_PER_ORCHESTRATOR`），
/// 每个乐手一轮最多贡献几条，50 条足够覆盖「编排者忙了一整轮」这段窗口。
const INBOX_CAP: usize = 50;

/// 单个受编排会话「尚未随汇报带走」的任务编号上限。
///
/// 正常路径上这个清单在每个回合结束时清空，长不到哪去；这条上限防的是病态情形
/// ——受编排会话卡在 `ai-working` 再也没结束过回合，而编排者还在一条条派活。
/// 与工单 10 任务账本「每编排者保留最近 200 条」同一个量级，溢出丢最旧。
const PENDING_TASKS_CAP: usize = 200;

/// AI 状态字符串（与桌面徽章、`PaneLiveness::status` 同一口径）。
const AI_WORKING: &str = "ai-working";
const AI_IDLE: &str = "ai-idle";
const IDLE: &str = "idle";

/// 这个状态字符串表示「pane 里还有 AI 会话在跑」吗。
///
/// 退出判定（`ai-*` → `idle`）靠它；写成白名单而不是 `starts_with("ai-")`，
/// 是为了让将来新增状态时**必须**回到这里做一次决定。
fn is_ai_status(status: &str) -> bool {
    status == AI_WORKING || status == AI_IDLE
}

/// 一条汇报说的是哪件事。
///
/// 五档穷尽 CONTEXT.md「汇报」词条列的种类：回合已结束 / 停下等人处理 / 已退出 /
/// 已被关闭 / 任务未被接收。**这里不渲染一个字**——正文（transcript 增量、画面
/// 尾部、文案）全是工单 12 的事，本模块只负责「什么时候、就哪件事」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportKind {
    /// 一个回合结束了。`cause` 是成因原文（`Stop` / `Interrupt` / `Stall` …，
    /// 无 hook 的降级路径上可能是 `None`）——**只有 `Stop` 才算这一回合有交付**，
    /// 该由编排者自己分辨，账本不替它折成布尔。
    TurnEnded {
        cause: Option<String>,
        started_at: Instant,
        ended_at: Instant,
    },
    /// 停下来等人处理（审批 / 交互式提问）。编排者不代答，只负责转告用户
    /// （ADR 0003 的铁律，ADR 0004 一字不动）。
    AwaitingHuman { cause: String },
    /// agent 退出了，pane 退回裸 shell。`cause` 是成因原文（`SessionEnd` /
    /// `StallExit` 都可能）。
    Exited { cause: Option<String> },
    /// pane 被关掉了。没有成因可言——它连终端都不在了。
    Closed,
    /// 派出去的任务迟迟没见对方开始处理（写入后 [`ACK_TIMEOUT`] 未转入
    /// `ai-working`）。**不代表任务失败**，只代表「看不出它收到了」。
    NotAccepted { task_id: String },
}

/// 一条汇报。
///
/// `task_ids` 是「此前写入且尚未汇报过」的任务编号清单——只有终结性的三档
/// （[`ReportKind::TurnEnded`] / [`ReportKind::Exited`] / [`ReportKind::Closed`]）
/// 带走它并清空；[`ReportKind::AwaitingHuman`] / [`ReportKind::NotAccepted`]
/// 一律为空：回合还没完，那些编号还得留给结束时那一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// 收件人：起这个受编排会话的编排者 pane。
    pub orchestrator_pane_id: u32,
    /// 主角：受编排会话的 pane 身份（= PTY 编号，与控制面对外的 pane 身份一致）。
    pub session_pane_id: u32,
    pub kind: ReportKind,
    pub task_ids: Vec<String>,
    /// 生成时刻（调用方传进来的那个 `now`）。
    pub at: Instant,
}

/// 一次取空的结果。
///
/// `dropped` 是**上一次取空之后**因收件箱溢出丢掉的条数，取走即归零——12 在批头
/// 里说明「另有 N 条汇报因积压被丢弃」，编排者据此知道自己看到的不是全部。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportBatch {
    pub reports: Vec<Report>,
    pub dropped: usize,
}

/// 一次派活在等的那声回应。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingAck {
    task_id: String,
    written_at: Instant,
}

/// 一个受编排会话的追踪状态。
#[derive(Debug)]
struct SessionTrack {
    orchestrator_pane_id: u32,
    /// pane 已经关了。置位之后该 pane 的一切事实一律忽略（触发一次即收敛）。
    closed: bool,
    /// 上一次看到的状态字符串。退出判定（`ai-*` → `idle`）唯一的依据，
    /// 也挡住 `idle` → `idle` 的重复上报。
    last_status: Option<String>,
    /// 进行中那个回合的起点；`None` = 此刻不在回合里。
    /// **只有它是 `Some` 时才可能出 [`ReportKind::TurnEnded`]**。
    turn_started_at: Option<Instant>,
    /// 尚未随汇报带走的任务编号。
    pending_task_ids: VecDeque<String>,
    /// 写入之后还没见对方开始处理的派活。
    awaiting_ack: Vec<PendingAck>,
    /// 已汇报到 transcript 的第几条消息（12 渲染完增量后回写）。
    reported_cursor: usize,
}

impl SessionTrack {
    fn new(orchestrator_pane_id: u32) -> Self {
        Self {
            orchestrator_pane_id,
            closed: false,
            last_status: None,
            turn_started_at: None,
            pending_task_ids: VecDeque::new(),
            awaiting_ack: Vec::new(),
            reported_cursor: 0,
        }
    }

    /// 取走并清空「尚未汇报」的任务编号（终结性的三档才调）。
    fn take_task_ids(&mut self) -> Vec<String> {
        self.pending_task_ids.drain(..).collect()
    }
}

/// 一个编排者的收件箱。
#[derive(Debug, Default)]
struct Inbox {
    reports: VecDeque<Report>,
    dropped: usize,
}

/// 汇报账本本体。
///
/// 12 把它锁在 `ControlPlane` 里，喂三种事实、按节拍取批次。所有方法都是 `&mut
/// self` 的同步调用，**没有内部可变性、没有后台线程**——并发编排全在调用方。
#[derive(Debug, Default)]
pub struct ReportLedger {
    /// 受编排会话 pane → 追踪状态。**不在这张表里的 pane 一律忽略**：编排者
    /// 自己的状态变化也会打进 `observe_status`（12 那边一条事件流喂过来），
    /// 必须在这里被过滤掉，否则编排者自己的回合会给自己发汇报。
    sessions: HashMap<u32, SessionTrack>,
    /// 编排者 pane → 收件箱。惰性建立，取空即销毁。
    inboxes: HashMap<u32, Inbox>,
}

impl ReportLedger {
    pub fn new() -> Self {
        Self::default()
    }

    // ─── 记账表同步（12 从 `TokenRegistry::sessions` 镜像过来）───────────

    /// 登记一个受编排会话属于哪个编排者。
    ///
    /// **幂等**：已在表里的 pane 原样保留（PTY 编号单调递增、不复用，重复登记只
    /// 会是 12 那边的同步抖动；清掉进行中的回合与待汇报任务反而是丢事实）。
    pub fn register_session(&mut self, session_pane_id: u32, orchestrator_pane_id: u32) {
        self.sessions
            .entry(session_pane_id)
            .or_insert_with(|| SessionTrack::new(orchestrator_pane_id));
    }

    /// 忘掉一个受编排会话（记账被回收时）。**不产生汇报**——「pane 关了」那条
    /// 是 [`Self::observe_pane_closed`] 的事，12 的调用顺序也是先关后忘。
    pub fn forget_session(&mut self, session_pane_id: u32) {
        self.sessions.remove(&session_pane_id);
    }

    // ─── 事实进 ────────────────────────────────────────────────────

    /// 吃一次受编排会话的状态变化。返回**本次是否产生了新汇报**（12 据此决定
    /// 要不要唤醒投递泵，省一次回头查收件箱的锁）。
    ///
    /// `status` / `cause` 与 [`crate::monitor::StatusChange`] 同字段同口径。
    pub fn observe_status(
        &mut self,
        session_pane_id: u32,
        status: &str,
        cause: Option<&str>,
        now: Instant,
    ) -> bool {
        // 未注册的 pane（编排者自己、用户亲手开的会话、裸 shell）一律忽略。
        let Some(track) = self.sessions.get_mut(&session_pane_id) else {
            return false;
        };
        if track.closed {
            return false;
        }
        let orchestrator = track.orchestrator_pane_id;
        let mut produced: Vec<(ReportKind, Vec<String>)> = Vec::new();

        // ① attention：**只看成因**。Codex 的 `PermissionRequest` 停在
        //    `ai-working`，看状态就会漏掉它。回合不结束、不重置，任务清单不带走
        //    ——同一成因再来一次就再出一条（第二次审批是第二件事，与
        //    `StatusEmitter::emit_if_changed` 对 attention 的去重豁免同一个道理）。
        let attention = cause.is_some_and(is_attention_cause);
        if let Some(c) = cause.filter(|_| attention) {
            produced.push((
                ReportKind::AwaitingHuman {
                    cause: c.to_string(),
                },
                Vec::new(),
            ));
        }

        // ② 派活回执：一见 `ai-working` 就说明对方开始处理新输入了，全部等待
        //    就地清掉且**不出汇报**（ADR 0004：只判定「有没有开始处理」）。
        if status == AI_WORKING {
            track.awaiting_ack.clear();
        }

        // ③ 回合与退出。
        match status {
            AI_WORKING => {
                // 「进入」才起点：回合中途的 PreToolUse/PostToolUse 也是 ai-working，
                // 每次都重置起点的话回合耗时会缩成最后一段工具调用。
                if track.turn_started_at.is_none() {
                    track.turn_started_at = Some(now);
                }
            }
            AI_IDLE => {
                // attention 成因的 ai-idle（Claude 的 PermissionRequest）不结束回合
                // ——人处理完之后这一轮还会接着跑。
                if !attention {
                    if let Some(started_at) = track.turn_started_at.take() {
                        let task_ids = track.take_task_ids();
                        produced.push((
                            ReportKind::TurnEnded {
                                cause: cause.map(|c| c.to_string()),
                                started_at,
                                ended_at: now,
                            },
                            task_ids,
                        ));
                    }
                    // 回合不在进行中（`ai-idle` → `ai-idle`，例如 Stop 之后闲置
                    // 提醒把成因换成 `Notification`）：**什么都不出**。
                }
            }
            IDLE => {
                // 退出只认「从 AI 会话里出来」这一次沿：`idle` → `idle` 不重复报，
                // 从没进过 AI 会话的 pane 也不报。
                if track.last_status.as_deref().is_some_and(is_ai_status) {
                    // 进行中的回合随之作废，**不另出 TurnEnded**：退出那条汇报把
                    // 回合期间的东西一并带走（12 用 `reported_cursor` 取增量）。
                    track.turn_started_at = None;
                    track.awaiting_ack.clear();
                    let task_ids = track.take_task_ids();
                    produced.push((
                        ReportKind::Exited {
                            cause: cause.map(|c| c.to_string()),
                        },
                        task_ids,
                    ));
                }
            }
            _ => {}
        }
        track.last_status = Some(status.to_string());

        let produced_any = !produced.is_empty();
        for (kind, task_ids) in produced {
            self.push(orchestrator, session_pane_id, kind, task_ids, now);
        }
        produced_any
    }

    /// 吃一次「受编排会话的 pane 被关掉了」。返回是否产生了汇报。
    ///
    /// 出一条 [`ReportKind::Closed`]（带走尚未汇报的任务编号），此后该 pane 的
    /// 一切事实一律忽略——关掉之后迟到的 hook 事件不该把它重新推活。
    pub fn observe_pane_closed(&mut self, session_pane_id: u32, now: Instant) -> bool {
        let Some(track) = self.sessions.get_mut(&session_pane_id) else {
            return false;
        };
        if track.closed {
            return false;
        }
        track.closed = true;
        track.turn_started_at = None;
        track.awaiting_ack.clear();
        let orchestrator = track.orchestrator_pane_id;
        let task_ids = track.take_task_ids();
        self.push(orchestrator, session_pane_id, ReportKind::Closed, task_ids, now);
        true
    }

    /// 吃一次派活写入（工单 10 的 `send` 成功之后调）。
    ///
    /// 两件事：把任务编号挂进「尚未汇报」清单（随下一条终结性汇报走）；以及在
    /// **写入那一刻目标不是 `ai-working`** 时登记一次未接收等待。
    ///
    /// 写入时目标已经在 `ai-working` 的**刻意不登记**：prompt 进了 agent 自己的
    /// 输入缓冲，从外面看不出它什么时候轮到——那一档看不出来，就不猜。
    pub fn note_task_written(
        &mut self,
        session_pane_id: u32,
        task_id: &str,
        target_status_at_write: &str,
        now: Instant,
    ) {
        let Some(track) = self.sessions.get_mut(&session_pane_id) else {
            return;
        };
        if track.closed {
            return;
        }
        if track.pending_task_ids.len() >= PENDING_TASKS_CAP {
            track.pending_task_ids.pop_front();
        }
        track.pending_task_ids.push_back(task_id.to_string());
        if target_status_at_write != AI_WORKING {
            track.awaiting_ack.push(PendingAck {
                task_id: task_id.to_string(),
                written_at: now,
            });
        }
    }

    /// 时间往前走一格：把超过 [`ACK_TIMEOUT`] 还没见对方开始处理的派活报出去。
    /// 返回是否产生了汇报。
    ///
    /// 报完就摘（触发一次即收敛，不会每拍重发）。任务编号**不**随这条汇报带走
    /// ——回合还没完，它得留给结束时那一条。
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut timed_out: Vec<(u32, u32, String)> = Vec::new();
        for (&session_pane_id, track) in self.sessions.iter_mut() {
            if track.closed {
                continue;
            }
            // 先取出来：闭包里再读 `track.orchestrator_pane_id` 会与 `retain`
            // 对 `track.awaiting_ack` 的可变借用打架。
            let orchestrator = track.orchestrator_pane_id;
            track.awaiting_ack.retain(|ack| {
                if now.duration_since(ack.written_at) >= ACK_TIMEOUT {
                    timed_out.push((orchestrator, session_pane_id, ack.task_id.clone()));
                    false
                } else {
                    true
                }
            });
        }
        let produced_any = !timed_out.is_empty();
        for (orchestrator, session_pane_id, task_id) in timed_out {
            self.push(
                orchestrator,
                session_pane_id,
                ReportKind::NotAccepted { task_id },
                Vec::new(),
                now,
            );
        }
        produced_any
    }

    // ─── 收件箱出 ──────────────────────────────────────────────────

    /// 这个编排者有积压吗。
    pub fn has_pending(&self, orchestrator_pane_id: u32) -> bool {
        self.inboxes
            .get(&orchestrator_pane_id)
            .is_some_and(|inbox| !inbox.reports.is_empty())
    }

    /// 此刻有积压的全部编排者（顺序不保证）。投递泵每拍照着它扫一遍。
    pub fn pending_orchestrators(&self) -> Vec<u32> {
        self.inboxes
            .iter()
            .filter(|(_, inbox)| !inbox.reports.is_empty())
            .map(|(&id, _)| id)
            .collect()
    }

    /// 一次取空。没有积压时答 `None`。
    pub fn take_batch(&mut self, orchestrator_pane_id: u32) -> Option<ReportBatch> {
        let inbox = self.inboxes.get_mut(&orchestrator_pane_id)?;
        if inbox.reports.is_empty() {
            return None;
        }
        let reports: Vec<Report> = inbox.reports.drain(..).collect();
        let dropped = std::mem::take(&mut inbox.dropped);
        self.inboxes.remove(&orchestrator_pane_id);
        Some(ReportBatch { reports, dropped })
    }

    /// 把取走的一批**放回队首**（12 的投递遇上 `desktopBusy` / 写失败时下一拍再试）。
    ///
    /// 放回的这批比期间新到的更旧，所以进队首；连带 `dropped` 一起还回去，
    /// 免得「投递失败」把「有几条被丢过」这件事洗掉。放回后仍受 [`INBOX_CAP`]
    /// 约束——溢出照旧丢最旧的，也就是放回的这批里最靠前的那几条。
    pub fn requeue_batch(&mut self, orchestrator_pane_id: u32, batch: ReportBatch) {
        if batch.reports.is_empty() && batch.dropped == 0 {
            return;
        }
        let inbox = self.inboxes.entry(orchestrator_pane_id).or_default();
        inbox.dropped += batch.dropped;
        for report in batch.reports.into_iter().rev() {
            inbox.reports.push_front(report);
        }
        while inbox.reports.len() > INBOX_CAP {
            inbox.reports.pop_front();
            inbox.dropped += 1;
        }
    }

    /// 丢掉一个编排者的收件箱（它 pane 关了 / 令牌撤销 / 离场）。
    ///
    /// **连同它名下受编排会话的追踪一起忘掉**——ADR 0004 的「编排者关闭后暂存的
    /// 汇报随之作废」不只是清一次队列：那些乐手还活着（不陪葬，ADR 0003），它们
    /// 后续的每一次 Stop 都还会打进 `observe_status`，收件人却已经没了。留着追踪
    /// 就是留一条只涨不消的队列。忘掉之后它们变回「未注册的 pane」，一切事实一律
    /// 忽略；万一编排者在同一编号上重新拿到授予，12 会照记账表重新 `register`。
    pub fn drop_inbox(&mut self, orchestrator_pane_id: u32) {
        self.inboxes.remove(&orchestrator_pane_id);
        self.sessions
            .retain(|_, track| track.orchestrator_pane_id != orchestrator_pane_id);
    }

    // ─── transcript 汇报游标（12 渲染完增量后回写）─────────────────────

    /// 已经汇报到 transcript 的第几条消息。未登记的 pane 答 `0`
    /// （「什么都还没报过」，与新登记的乐手同一个起点）。
    pub fn reported_cursor(&self, session_pane_id: u32) -> usize {
        self.sessions
            .get(&session_pane_id)
            .map_or(0, |track| track.reported_cursor)
    }

    /// 回写汇报游标。未登记的 pane 静默忽略。
    pub fn set_reported_cursor(&mut self, session_pane_id: u32, cursor: usize) {
        if let Some(track) = self.sessions.get_mut(&session_pane_id) {
            track.reported_cursor = cursor;
        }
    }

    // ─── 内部 ──────────────────────────────────────────────────────

    fn push(
        &mut self,
        orchestrator_pane_id: u32,
        session_pane_id: u32,
        kind: ReportKind,
        task_ids: Vec<String>,
        at: Instant,
    ) {
        let inbox = self.inboxes.entry(orchestrator_pane_id).or_default();
        if inbox.reports.len() >= INBOX_CAP {
            inbox.reports.pop_front();
            inbox.dropped += 1;
        }
        inbox.reports.push_back(Report {
            orchestrator_pane_id,
            session_pane_id,
            kind,
            task_ids,
            at,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORCH: u32 = 10;
    const HAND: u32 = 20;

    /// 一个已登记好一名乐手的账本 + 一个时间原点。
    fn ledger() -> (ReportLedger, Instant) {
        let mut l = ReportLedger::new();
        l.register_session(HAND, ORCH);
        (l, Instant::now())
    }

    /// 取空并断言只有一条，返回那一条。
    fn only(l: &mut ReportLedger, orchestrator: u32) -> Report {
        let batch = l
            .take_batch(orchestrator)
            .expect("收件箱里应当有汇报");
        assert_eq!(batch.reports.len(), 1, "期望恰好一条: {:?}", batch.reports);
        batch.reports.into_iter().next().unwrap()
    }

    fn secs(base: Instant, n: u64) -> Instant {
        base + Duration::from_secs(n)
    }

    // ─── 回合规则 ──────────────────────────────────────────────────

    /// 回合 = 进入 ai-working → 转到非 attention 成因的 ai-idle。
    #[test]
    fn 进入工作再回空闲算一个回合() {
        let (mut l, t0) = ledger();
        assert!(!l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0));
        assert!(l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 30)));

        let report = only(&mut l, ORCH);
        assert_eq!(report.orchestrator_pane_id, ORCH);
        assert_eq!(report.session_pane_id, HAND);
        assert_eq!(report.at, secs(t0, 30));
        assert_eq!(
            report.kind,
            ReportKind::TurnEnded {
                cause: Some("Stop".into()),
                started_at: t0,
                ended_at: secs(t0, 30),
            }
        );
    }

    /// 只有正在进行中的回合才会结束：ai-idle → ai-idle（Stop 之后闲置提醒把成因
    /// 换成 Notification）**不算**新回合结束。这正是「触发一次即收敛」那条铁律
    /// 在本模块的落点——回合结束后 `turn` 置空，下一次 ai-idle 不会再报。
    #[test]
    fn 空闲到空闲不算新回合结束() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0);
        assert!(l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 5)));
        // 闲置提醒：还是 ai-idle，成因换成 Notification
        assert!(!l.observe_status(HAND, AI_IDLE, Some("Notification"), secs(t0, 70)));
        // 再来一次 SessionStart 之类的 ai-idle 也一样
        assert!(!l.observe_status(HAND, AI_IDLE, Some("SessionStart"), secs(t0, 80)));

        let batch = l.take_batch(ORCH).unwrap();
        assert_eq!(batch.reports.len(), 1, "只该有那一条回合结束");
    }

    /// 回合中途的 ai-working 事件不重置起点（否则回合耗时会缩成最后一段工具调用）。
    #[test]
    fn 回合中途不重置起点() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0);
        l.observe_status(HAND, AI_WORKING, Some("PreToolUse"), secs(t0, 3));
        l.observe_status(HAND, AI_WORKING, Some("PostToolUse"), secs(t0, 9));
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 12));

        match only(&mut l, ORCH).kind {
            ReportKind::TurnEnded { started_at, .. } => assert_eq!(started_at, t0),
            other => panic!("期望回合结束: {:?}", other),
        }
    }

    /// 一上来就是 ai-idle（SessionStart）时没有回合可结束。
    #[test]
    fn 没开始过的回合不会结束() {
        let (mut l, t0) = ledger();
        assert!(!l.observe_status(HAND, AI_IDLE, Some("SessionStart"), t0));
        assert!(l.take_batch(ORCH).is_none());
    }

    // ─── attention 规则 ────────────────────────────────────────────

    /// attention **只看成因、不看状态**：Codex 的 PermissionRequest 停在 ai-working。
    #[test]
    fn 等待处理只看成因不看状态() {
        let (mut l, t0) = ledger();
        assert!(l.observe_status(HAND, AI_WORKING, Some("PermissionRequest"), t0));
        assert_eq!(
            only(&mut l, ORCH).kind,
            ReportKind::AwaitingHuman {
                cause: "PermissionRequest".into()
            }
        );

        // Claude 那一档落在 ai-idle，同样出一条
        let (mut l2, t2) = ledger();
        assert!(l2.observe_status(HAND, AI_IDLE, Some("PermissionRequest"), t2));
        assert_eq!(
            only(&mut l2, ORCH).kind,
            ReportKind::AwaitingHuman {
                cause: "PermissionRequest".into()
            }
        );
    }

    /// attention 不结束回合、不重置回合起点。
    #[test]
    fn 等待处理不结束也不重置回合() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0);
        // Claude 的权限请求落 ai-idle：不得被当成回合结束
        l.observe_status(HAND, AI_IDLE, Some("PermissionRequest"), secs(t0, 4));
        l.observe_status(HAND, AI_WORKING, Some("PostToolUse"), secs(t0, 8));
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 20));

        let batch = l.take_batch(ORCH).unwrap();
        assert_eq!(batch.reports.len(), 2);
        assert_eq!(
            batch.reports[0].kind,
            ReportKind::AwaitingHuman {
                cause: "PermissionRequest".into()
            }
        );
        assert_eq!(
            batch.reports[1].kind,
            ReportKind::TurnEnded {
                cause: Some("Stop".into()),
                started_at: t0, // 起点没被 attention 冲掉
                ended_at: secs(t0, 20),
            }
        );
    }

    /// 同一成因再来一次再出一条：第二次审批是第二件事
    /// （与 `StatusEmitter::emit_if_changed` 对 attention 的去重豁免同一个道理）。
    #[test]
    fn 同一等待成因再来一次再出一条() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_IDLE, Some("PermissionRequest"), t0);
        l.observe_status(HAND, AI_WORKING, Some("PermissionDenied"), secs(t0, 2));
        l.observe_status(HAND, AI_IDLE, Some("PermissionRequest"), secs(t0, 5));

        let batch = l.take_batch(ORCH).unwrap();
        assert_eq!(batch.reports.len(), 2);
        assert!(batch
            .reports
            .iter()
            .all(|r| matches!(r.kind, ReportKind::AwaitingHuman { .. })));
    }

    /// 三种 attention 成因（`hook_server::is_attention_cause` 的全集）都认。
    #[test]
    fn 三种等待成因都认() {
        for cause in ["PermissionRequest", "Elicitation", "StopFailure"] {
            let (mut l, t0) = ledger();
            assert!(
                l.observe_status(HAND, AI_IDLE, Some(cause), t0),
                "{cause} 应出一条等待处理"
            );
        }
    }

    // ─── 退出规则 ──────────────────────────────────────────────────

    /// ai-* → idle 出 Exited，成因原文照带；进行中的回合随之作废、不另出 TurnEnded。
    #[test]
    fn 退出作废进行中的回合且只出一条() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0);
        assert!(l.observe_status(HAND, IDLE, Some("SessionEnd"), secs(t0, 6)));

        let report = only(&mut l, ORCH);
        assert_eq!(
            report.kind,
            ReportKind::Exited {
                cause: Some("SessionEnd".into())
            }
        );
    }

    /// 停摆兜底判定已退出那一档：成因原文是 `StallExit`
    /// （见 `monitor::stall_settle_target`）。
    #[test]
    fn 停摆退出的成因是原文() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0);
        l.observe_status(HAND, IDLE, Some("StallExit"), secs(t0, 10));
        assert_eq!(
            only(&mut l, ORCH).kind,
            ReportKind::Exited {
                cause: Some("StallExit".into())
            }
        );
    }

    /// idle → idle 不重复报；从没进过 AI 会话的 pane 也不报。
    #[test]
    fn 退出只报一次且裸_shell_不报() {
        let (mut l, t0) = ledger();
        // 从没进过 ai-* 就直接 idle：不是「退出」
        assert!(!l.observe_status(HAND, IDLE, Some("SessionEnd"), t0));
        assert!(l.take_batch(ORCH).is_none());

        l.observe_status(HAND, AI_IDLE, Some("SessionStart"), secs(t0, 1));
        assert!(l.observe_status(HAND, IDLE, Some("SessionEnd"), secs(t0, 2)));
        assert!(!l.observe_status(HAND, IDLE, None, secs(t0, 3)), "重复 idle 不再报");
        let batch = l.take_batch(ORCH).unwrap();
        assert_eq!(batch.reports.len(), 1);
    }

    /// 退出之后同一 pane 里重开 agent（`claude -c`）照常追踪下一个回合。
    #[test]
    fn 退出后同一_pane_重开照常追踪() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0);
        l.observe_status(HAND, IDLE, Some("SessionEnd"), secs(t0, 5));
        l.take_batch(ORCH);

        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), secs(t0, 60));
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 90));
        assert!(matches!(
            only(&mut l, ORCH).kind,
            ReportKind::TurnEnded { .. }
        ));
    }

    // ─── 关闭规则 ──────────────────────────────────────────────────

    #[test]
    fn 关闭出一条汇报且此后一切事实被忽略() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0);
        assert!(l.observe_pane_closed(HAND, secs(t0, 3)));
        assert_eq!(only(&mut l, ORCH).kind, ReportKind::Closed);

        // 关闭之后：迟到的 hook 事件、重复关闭、再派活，一律无声
        assert!(!l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 4)));
        assert!(!l.observe_status(HAND, AI_WORKING, Some("PreToolUse"), secs(t0, 5)));
        assert!(!l.observe_pane_closed(HAND, secs(t0, 6)));
        l.note_task_written(HAND, "t9", IDLE, secs(t0, 7));
        assert!(!l.tick(secs(t0, 999)));
        assert!(l.take_batch(ORCH).is_none(), "关闭之后不该再有任何汇报");
    }

    // ─── 未被接收规则 ──────────────────────────────────────────────

    /// 写入时目标不是 ai-working → 登记等待；超过 15s 未开始处理就报，报完即摘。
    #[test]
    fn 派活超时未开始处理出未被接收() {
        let (mut l, t0) = ledger();
        l.note_task_written(HAND, "t1", AI_IDLE, t0);
        assert!(!l.tick(secs(t0, 14)), "还没到点");
        assert!(l.tick(secs(t0, 15)), "15s 到点即报");

        let report = only(&mut l, ORCH);
        assert_eq!(
            report.kind,
            ReportKind::NotAccepted {
                task_id: "t1".into()
            }
        );
        // 触发一次即收敛：不会每拍重发
        assert!(!l.tick(secs(t0, 60)));
        assert!(l.take_batch(ORCH).is_none());
    }

    /// 之后第一次转到 ai-working 即清掉等待，**不出汇报**。
    #[test]
    fn 转入工作即清掉未接收等待() {
        let (mut l, t0) = ledger();
        l.note_task_written(HAND, "t1", AI_IDLE, t0);
        assert!(!l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), secs(t0, 2)));
        assert!(!l.tick(secs(t0, 60)), "等待已被清掉，不该再报");
        assert!(l.take_batch(ORCH).is_none());
    }

    /// 目标写入时已是 ai-working 的**不登记**——那一档看不出来，不猜。
    #[test]
    fn 写入时已在工作中的派活不登记等待() {
        let (mut l, t0) = ledger();
        l.note_task_written(HAND, "t1", AI_WORKING, t0);
        assert!(!l.tick(secs(t0, 600)));
        assert!(l.take_batch(ORCH).is_none());
    }

    /// 写进裸 shell（idle）的派活同样要盯——它多半根本没进 agent。
    #[test]
    fn 写入裸_shell_的派活照样登记等待() {
        let (mut l, t0) = ledger();
        l.note_task_written(HAND, "t1", IDLE, t0);
        assert!(l.tick(secs(t0, 20)));
    }

    /// 多条等待各自按自己的写入时刻到点。
    #[test]
    fn 多条未接收等待各自到点() {
        let (mut l, t0) = ledger();
        l.note_task_written(HAND, "t1", AI_IDLE, t0);
        l.note_task_written(HAND, "t2", AI_IDLE, secs(t0, 10));
        assert!(l.tick(secs(t0, 16)));
        assert_eq!(
            only(&mut l, ORCH).kind,
            ReportKind::NotAccepted {
                task_id: "t1".into()
            }
        );
        assert!(l.tick(secs(t0, 26)));
        assert_eq!(
            only(&mut l, ORCH).kind,
            ReportKind::NotAccepted {
                task_id: "t2".into()
            }
        );
    }

    // ─── 任务编号随汇报走 ──────────────────────────────────────────

    #[test]
    fn 回合结束带走任务清单并清空() {
        let (mut l, t0) = ledger();
        l.note_task_written(HAND, "t1", AI_IDLE, t0);
        l.note_task_written(HAND, "t2", AI_WORKING, secs(t0, 1));
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), secs(t0, 2));
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 20));

        let report = only(&mut l, ORCH);
        assert_eq!(report.task_ids, vec!["t1".to_string(), "t2".to_string()]);

        // 清空了：下一个回合不再重复带
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), secs(t0, 30));
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 40));
        assert!(only(&mut l, ORCH).task_ids.is_empty());
    }

    #[test]
    fn 退出与关闭同样带走任务清单() {
        let (mut l, t0) = ledger();
        l.note_task_written(HAND, "t1", AI_IDLE, t0);
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), secs(t0, 1));
        l.observe_status(HAND, IDLE, Some("SessionEnd"), secs(t0, 5));
        assert_eq!(only(&mut l, ORCH).task_ids, vec!["t1".to_string()]);

        let (mut l2, t2) = ledger();
        l2.note_task_written(HAND, "t7", AI_IDLE, t2);
        l2.observe_pane_closed(HAND, secs(t2, 1));
        assert_eq!(only(&mut l2, ORCH).task_ids, vec!["t7".to_string()]);
    }

    /// AwaitingHuman / NotAccepted **不**带走清单——回合还没完。
    #[test]
    fn 等待处理与未被接收不带走任务清单() {
        let (mut l, t0) = ledger();
        l.note_task_written(HAND, "t1", AI_IDLE, t0);
        l.observe_status(HAND, AI_IDLE, Some("PermissionRequest"), secs(t0, 1));
        assert!(only(&mut l, ORCH).task_ids.is_empty());

        l.tick(secs(t0, 20));
        assert!(only(&mut l, ORCH).task_ids.is_empty());

        // 编号还在清单上，留给结束时那一条
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), secs(t0, 30));
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 40));
        assert_eq!(only(&mut l, ORCH).task_ids, vec!["t1".to_string()]);
    }

    /// 病态情形的护栏：回合永远不结束时「尚未汇报」的清单不会无限长。
    #[test]
    fn 尚未汇报的任务清单有上限() {
        let (mut l, t0) = ledger();
        for i in 0..PENDING_TASKS_CAP + 5 {
            l.note_task_written(HAND, &format!("t{i}"), AI_WORKING, t0);
        }
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), secs(t0, 1));
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 2));
        let ids = only(&mut l, ORCH).task_ids;
        assert_eq!(ids.len(), PENDING_TASKS_CAP);
        assert_eq!(ids[0], "t5", "溢出丢的是最旧的");
    }

    // ─── 收件箱 ────────────────────────────────────────────────────

    #[test]
    fn 收件箱溢出丢最旧并累加丢弃计数() {
        let (mut l, t0) = ledger();
        // 每一轮「进入工作 → Stop」出一条回合结束
        for i in 0..(INBOX_CAP + 3) as u64 {
            l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), secs(t0, i * 10));
            l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, i * 10 + 5));
        }
        let batch = l.take_batch(ORCH).unwrap();
        assert_eq!(batch.reports.len(), INBOX_CAP);
        assert_eq!(batch.dropped, 3);
        // 留下的是最新的那批：第一条的结束时刻对应第 3 轮
        match batch.reports[0].kind {
            ReportKind::TurnEnded { ended_at, .. } => assert_eq!(ended_at, secs(t0, 35)),
            ref other => panic!("期望回合结束: {other:?}"),
        }
    }

    #[test]
    fn 取批次一次取空并把丢弃计数归零() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, None, t0);
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 1));
        assert!(l.has_pending(ORCH));
        assert_eq!(l.pending_orchestrators(), vec![ORCH]);

        let batch = l.take_batch(ORCH).unwrap();
        assert_eq!(batch.dropped, 0);
        assert!(!l.has_pending(ORCH));
        assert!(l.pending_orchestrators().is_empty());
        assert!(l.take_batch(ORCH).is_none(), "空收件箱答 None");
    }

    #[test]
    fn 放回批次回到队首并恢复丢弃计数() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, None, t0);
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 1));
        let batch = l.take_batch(ORCH).unwrap();

        // 投递失败 → 期间又来了一条新汇报 → 把旧的放回队首
        l.observe_pane_closed(HAND, secs(t0, 2));
        l.requeue_batch(
            ORCH,
            ReportBatch {
                reports: batch.reports.clone(),
                dropped: 2,
            },
        );
        let again = l.take_batch(ORCH).unwrap();
        assert_eq!(again.dropped, 2, "丢弃计数不能被投递失败洗掉");
        assert_eq!(again.reports.len(), 2);
        assert_eq!(again.reports[0], batch.reports[0], "放回的那批仍在最前");
        assert_eq!(again.reports[1].kind, ReportKind::Closed);
    }

    // ─── 隔离：未注册 / 已关闭 / 收件箱已丢弃 ───────────────────────

    /// 未注册的 pane 一律忽略——**编排者自己的状态变化也会打进来**
    /// （12 那边一条事件流喂过来），不过滤掉它就会给自己发汇报。
    #[test]
    fn 未注册的_pane_的事实一律忽略() {
        let mut l = ReportLedger::new();
        let t0 = Instant::now();
        // ORCH 是编排者自己：它跑完一轮也不该产生任何汇报
        assert!(!l.observe_status(ORCH, AI_WORKING, Some("UserPromptSubmit"), t0));
        assert!(!l.observe_status(ORCH, AI_IDLE, Some("Stop"), secs(t0, 1)));
        assert!(!l.observe_pane_closed(ORCH, secs(t0, 2)));
        l.note_task_written(999, "t1", AI_IDLE, t0);
        assert!(!l.tick(secs(t0, 600)));
        assert!(l.take_batch(ORCH).is_none());
        assert!(l.pending_orchestrators().is_empty());
    }

    /// 编排者收件箱被丢弃后不再累积（连同它名下乐手的追踪一起忘掉）。
    #[test]
    fn 收件箱被丢弃后不再累积() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, None, t0);
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 1));
        assert!(l.has_pending(ORCH));

        l.drop_inbox(ORCH);
        assert!(!l.has_pending(ORCH));
        assert!(l.take_batch(ORCH).is_none());

        // 乐手还活着，但它后续的一切事实都没有收件人了
        assert!(!l.observe_status(HAND, AI_WORKING, None, secs(t0, 10)));
        assert!(!l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 20)));
        assert!(!l.observe_pane_closed(HAND, secs(t0, 30)));
        assert!(l.take_batch(ORCH).is_none());
    }

    /// 记账被回收（`forget_session`）之后同样不再累积，且不产生汇报。
    #[test]
    fn 忘掉的乐手不再产生汇报() {
        let (mut l, t0) = ledger();
        l.forget_session(HAND);
        assert!(!l.observe_status(HAND, AI_WORKING, None, t0));
        assert!(!l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 1)));
        assert!(l.take_batch(ORCH).is_none());
    }

    /// 重复登记是幂等的：不清掉进行中的回合与待汇报任务。
    #[test]
    fn 重复登记不冲掉进行中的状态() {
        let (mut l, t0) = ledger();
        l.note_task_written(HAND, "t1", AI_WORKING, t0);
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), secs(t0, 1));
        l.register_session(HAND, ORCH); // 12 那边的同步抖动
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 9));

        let report = only(&mut l, ORCH);
        assert_eq!(report.task_ids, vec!["t1".to_string()]);
        match report.kind {
            ReportKind::TurnEnded { started_at, .. } => assert_eq!(started_at, secs(t0, 1)),
            other => panic!("期望回合结束: {other:?}"),
        }
    }

    // ─── 独立性 ────────────────────────────────────────────────────

    #[test]
    fn 两个乐手各自独立() {
        let mut l = ReportLedger::new();
        let t0 = Instant::now();
        l.register_session(20, ORCH);
        l.register_session(21, ORCH);

        l.observe_status(20, AI_WORKING, Some("UserPromptSubmit"), t0);
        l.observe_status(21, AI_WORKING, Some("UserPromptSubmit"), secs(t0, 2));
        l.note_task_written(20, "t1", AI_WORKING, t0);
        l.note_task_written(21, "t2", AI_WORKING, secs(t0, 2));
        // 20 号结束回合，21 号还在跑
        l.observe_status(20, AI_IDLE, Some("Stop"), secs(t0, 5));

        let report = only(&mut l, ORCH);
        assert_eq!(report.session_pane_id, 20);
        assert_eq!(report.task_ids, vec!["t1".to_string()]);

        // 关掉 20 号不影响 21 号
        l.observe_pane_closed(20, secs(t0, 6));
        l.take_batch(ORCH);
        l.observe_status(21, AI_IDLE, Some("Stop"), secs(t0, 9));
        let report = only(&mut l, ORCH);
        assert_eq!(report.session_pane_id, 21);
        assert_eq!(report.task_ids, vec!["t2".to_string()]);
    }

    #[test]
    fn 两个编排者各自独立() {
        let mut l = ReportLedger::new();
        let t0 = Instant::now();
        l.register_session(20, 10);
        l.register_session(30, 11);

        l.observe_status(20, AI_WORKING, None, t0);
        l.observe_status(20, AI_IDLE, Some("Stop"), secs(t0, 1));
        l.observe_status(30, AI_WORKING, None, t0);
        l.observe_status(30, AI_IDLE, Some("Stop"), secs(t0, 2));

        let mut pending = l.pending_orchestrators();
        pending.sort_unstable();
        assert_eq!(pending, vec![10, 11]);
        assert_eq!(only(&mut l, 10).session_pane_id, 20);
        assert!(l.has_pending(11), "取空 10 号不该动 11 号");

        // 丢掉 10 号的收件箱也不牵连 11 号
        l.drop_inbox(10);
        assert_eq!(only(&mut l, 11).session_pane_id, 30);
    }

    // ─── 典型序列（三家各一条）───────────────────────────────────

    /// Claude 典型序列：UserPromptSubmit → PreToolUse → PostToolUse → Stop，
    /// 只出**一条**回合结束。
    #[test]
    fn claude_典型序列只出一条回合结束() {
        let (mut l, t0) = ledger();
        for (i, (status, cause)) in [
            (AI_IDLE, "SessionStart"),
            (AI_WORKING, "UserPromptSubmit"),
            (AI_WORKING, "PreToolUse"),
            (AI_WORKING, "PostToolUse"),
            (AI_WORKING, "PreToolUse"),
            (AI_WORKING, "PostToolUse"),
            (AI_IDLE, "Stop"),
        ]
        .into_iter()
        .enumerate()
        {
            l.observe_status(HAND, status, Some(cause), secs(t0, i as u64));
        }
        let report = only(&mut l, ORCH);
        assert_eq!(
            report.kind,
            ReportKind::TurnEnded {
                cause: Some("Stop".into()),
                started_at: secs(t0, 1),
                ended_at: secs(t0, 6),
            }
        );
    }

    /// Codex 审批序列：PermissionRequest 停在 ai-working（`map_event_to_status`
    /// 对它有专门一条），出一条等待处理 + 一条回合结束。
    #[test]
    fn codex_审批序列出一条等待处理与一条回合结束() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("PermissionRequest"), t0);
        l.observe_status(HAND, AI_WORKING, Some("PostToolUse"), secs(t0, 30));
        l.observe_status(HAND, AI_IDLE, Some("Stop"), secs(t0, 45));

        let batch = l.take_batch(ORCH).unwrap();
        assert_eq!(batch.reports.len(), 2);
        assert_eq!(
            batch.reports[0].kind,
            ReportKind::AwaitingHuman {
                cause: "PermissionRequest".into()
            }
        );
        assert_eq!(
            batch.reports[1].kind,
            ReportKind::TurnEnded {
                cause: Some("Stop".into()),
                started_at: t0,
                ended_at: secs(t0, 45),
            }
        );
    }

    /// 用户亲手打断（`note_user_interrupt` 落盘的那一档）：出回合结束，
    /// 成因原文是 `Interrupt` —— 编排者据此知道这一轮**没有交付**。
    #[test]
    fn 用户打断出回合结束且成因是原文() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0);
        l.observe_status(HAND, AI_IDLE, Some("Interrupt"), secs(t0, 8));
        assert_eq!(
            only(&mut l, ORCH).kind,
            ReportKind::TurnEnded {
                cause: Some("Interrupt".into()),
                started_at: t0,
                ended_at: secs(t0, 8),
            }
        );
    }

    /// 停摆兜底（`stall_settle_target` 的 10s 双静默）：同理，成因原文是 `Stall`。
    #[test]
    fn 停摆兜底出回合结束且成因是原文() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, Some("UserPromptSubmit"), t0);
        l.observe_status(HAND, AI_IDLE, Some("Stall"), secs(t0, 30));
        assert_eq!(
            only(&mut l, ORCH).kind,
            ReportKind::TurnEnded {
                cause: Some("Stall".into()),
                started_at: t0,
                ended_at: secs(t0, 30),
            }
        );
    }

    /// 无 hook 的降级路径上 monitor 一律以**无成因**发射：照样能追回合。
    #[test]
    fn 无成因的状态变化照样追得出回合() {
        let (mut l, t0) = ledger();
        l.observe_status(HAND, AI_WORKING, None, t0);
        l.observe_status(HAND, AI_IDLE, None, secs(t0, 4));
        assert_eq!(
            only(&mut l, ORCH).kind,
            ReportKind::TurnEnded {
                cause: None,
                started_at: t0,
                ended_at: secs(t0, 4),
            }
        );
    }

    // ─── transcript 汇报游标 ───────────────────────────────────────

    #[test]
    fn 汇报游标可读写且陌生_pane_答零() {
        let (mut l, _t0) = ledger();
        assert_eq!(l.reported_cursor(HAND), 0);
        l.set_reported_cursor(HAND, 7);
        assert_eq!(l.reported_cursor(HAND), 7);

        // 陌生 pane：读答 0，写静默忽略（不会凭空建出一条追踪）
        assert_eq!(l.reported_cursor(999), 0);
        l.set_reported_cursor(999, 42);
        assert_eq!(l.reported_cursor(999), 0);

        // 忘掉之后回到零
        l.forget_session(HAND);
        assert_eq!(l.reported_cursor(HAND), 0);
    }
}
