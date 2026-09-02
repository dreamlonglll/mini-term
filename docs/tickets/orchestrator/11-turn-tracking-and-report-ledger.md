# 11 — 回合追踪与汇报账本（纯状态机，新模块）

**Parent:** ADR 0004（受编排会话的汇报推送）

**What to build:** 一个**不碰线程、不做 I/O、不认识 `ControlPlane`** 的纯状态机模块 `crates/mt-ai/src/reports.rs`：吃受编排会话的状态变化（`status` + `cause`）、pane 关闭、派活写入这三种事实，按 ADR 0004 的规则产出「汇报」并按编排者攒进各自的收件箱。它是 12 的引擎，本票交付时没有任何调用方——全部价值在它的单元测试里。

**Blocked by:** 无（与 10 并行；12 依赖本票）

**Status:** done

- [x] 模块 `crates/mt-ai/src/reports.rs`（`pub(crate)` 即可，12 在同 crate 内接线），`lib.rs` 加一行 `mod reports;`——本票只动这两个文件
- [x] 类型：`ReportKind`（`TurnEnded { cause: Option<String>, started_at, ended_at }` / `AwaitingHuman { cause }` / `Exited { cause }` / `Closed` / `NotAccepted { task_id }`）、`Report { orchestrator_pane_id, hand_pane_id, kind, task_ids: Vec<String>, at }`、`ReportBatch { reports, dropped: usize }`
- [x] 账本 API（命名实施方定，语义如下）：`observe_status(hand, status, cause, now)`、`observe_pane_closed(hand, now)`、`note_task_written(hand, task_id, target_status_at_write, now)`、`tick(now)`（未被接收超时）、`take_batch(orchestrator) -> Option<ReportBatch>`、`has_pending(orchestrator)`、`drop_inbox(orchestrator)`、`register_hand(hand, orchestrator)` / `forget_hand(hand)`（12 从记账表同步过来）、`reported_cursor(hand)` / `set_reported_cursor(hand, seq)`（12 渲染完 transcript 后回写）
- [x] 回合规则：**回合 = 进入 `ai-working` → 转到非 attention 成因的 `ai-idle`**。只有正在进行中的回合才会结束；`ai-idle` → `ai-idle`（例如 Stop 之后闲置提醒把成因换成 `Notification`）**不算**新回合结束
- [x] attention 规则：任何一次状态变化只要 `cause` 属 attention（`hook_server::is_attention_cause`），就出一条 `AwaitingHuman`，**不看状态**（Codex 的 `PermissionRequest` 停在 `ai-working`）；回合不结束、不重置；同一成因再来一次再出一条（第二次审批是第二件事）
- [x] 退出规则：从 `ai-*` 转到 `idle` 出 `Exited`（成因原文，`SessionEnd` / `StallExit` 都可能）；进行中的回合随之作废（不另出 `TurnEnded`——退出那条汇报把回合期间的东西一并带走，由 12 用 `reported_cursor` 取增量）
- [x] 关闭规则：`observe_pane_closed` 出 `Closed`，之后该乐手的一切事实忽略
- [x] 未被接收规则：`note_task_written` 时 `target_status_at_write != "ai-working"` 才登记等待；之后第一次转到 `ai-working` 即清掉等待（不出汇报）；`tick` 发现等待超过 `ACK_TIMEOUT`（15s）就出 `NotAccepted { task_id }` 并清掉。目标写入时已是 `ai-working` 的**不登记**——那一档看不出来，不猜
- [x] 任务编号随汇报走：`note_task_written` 把编号挂到该乐手的「尚未汇报」清单；`TurnEnded` / `Exited` / `Closed` 带走清单并清空；`AwaitingHuman` / `NotAccepted` **不**带走（回合还没完）
- [x] 收件箱：每编排者一个，上限 `INBOX_CAP`（50），溢出丢最旧并累加 `dropped`，`take_batch` 一次取空并把 `dropped` 带出去归零
- [x] 乐手关闭或编排者收件箱被丢弃后不再累积；未注册的 pane 的事实一律忽略（编排者自己的状态变化也会打进来，必须被过滤掉）
- [x] 单元测试覆盖上面每一条规则，另加：Claude 典型序列（UserPromptSubmit→PreToolUse→…→Stop）只出一条 `TurnEnded`；Codex 审批序列（ai-working/PermissionRequest → ai-working/PostToolUse → ai-idle/Stop）出一条 `AwaitingHuman` + 一条 `TurnEnded`；用户打断（ai-idle/Interrupt）出 `TurnEnded` 且 cause 原文是 `Interrupt`；停摆（ai-idle/Stall）同理；两个乐手各自独立；两个编排者各自独立
- [x] `cargo test -p mt-ai` 全绿

## 设计要点（给实施方）

- 这个模块的一切时间都用调用方传进来的 `Instant`，**不许自己 `Instant::now()`**——测试要能把 15 秒拨过去。
- 与 `monitor::stall_settle_target` 同一条铁律：**结论落盘、触发一次即收敛**。未被接收的等待清掉就不再重发；回合结束后 `turn` 置空，下一次 `ai-idle` 不会再报。
- `hand_pane_id` 是 PTY 编号（与控制面对外的 pane 身份一致）。
- 不要在这里渲染文字、不要读 transcript、不要碰 `OrchestratorActions`——那些都是 12 的事。这一票的模块要能在没有任何桌面端的情况下被完整测试。
- 术语纪律：注释与测试名里可以叫「乐手」「大脑」，类型名与将来会进用户可见面的字符串一律 orchestrated session / orchestrator。

## 纪律

- 禁跑 `cargo fmt`。Edit 工具可能把整份文件写成 CRLF，新建文件用 LF，改完用 `git ls-files --eol` 核对。
- 不做任何 git 提交 / stash / checkout——由编排会话统一提交。
- 与工单 10 并行在同一个 worktree 里：10 在改 `control.rs`、mt-config、mt-app、sidecars，本票**只动 `reports.rs` 与 `lib.rs` 的一行**；cargo 构建锁互相等是正常的，链接期 LNK1104 是杀软扫 exe 的随机撞，重跑一次即可。

## 设计决议（实施方填）

落点：`crates/mt-ai/src/reports.rs`（新增），`crates/mt-ai/src/lib.rs` 加一行 `mod reports;`（私有模块，
`pub` 项即 crate 内可见；模块头挂了 `#![allow(dead_code)]` + 注释，**12 接线后连注释一起删**）。

### 命名：`hand_*` 一律改成 `session_*`

清单里写的 `hand_pane_id` / `register_hand` / `forget_hand` 全部落地为
`session_pane_id` / `register_session` / `forget_session`。理由是本票「设计要点」的最后一条
（类型名一律 orchestrated session 语义）；「乐手」只留在注释与测试名里。语义一字不变。

### 公开 API（给工单 12 的确切形状）

全部挂在 `pub struct ReportLedger`（`Default` + `new()`，纯 `&mut self` 同步方法，
**无内部可变性、无线程、无 I/O、全程不调 `Instant::now()`**；12 自己把它锁进 `ControlPlane`）。

**类型**

```rust
pub enum ReportKind {
    TurnEnded { cause: Option<String>, started_at: Instant, ended_at: Instant },
    AwaitingHuman { cause: String },   // attention 一定有成因，故非 Option
    Exited { cause: Option<String> },  // SessionEnd / StallExit，降级路径可能无成因
    Closed,
    NotAccepted { task_id: String },
}
pub struct Report {
    pub orchestrator_pane_id: u32,  // 收件人
    pub session_pane_id: u32,       // 主角（受编排会话的 PTY 编号）
    pub kind: ReportKind,
    pub task_ids: Vec<String>,
    pub at: Instant,                // 调用方传进来的那个 now
}
pub struct ReportBatch { pub reports: Vec<Report>, pub dropped: usize }
```

三者都是 `Debug + Clone + PartialEq + Eq`。

**方法**

| 签名 | 语义 |
|---|---|
| `register_session(&mut self, session_pane_id: u32, orchestrator_pane_id: u32)` | 登记归属。**幂等**：已在表里就原样保留（不清回合、不清待汇报任务），12 那边的同步抖动不会丢事实 |
| `forget_session(&mut self, session_pane_id: u32)` | 忘掉一名受编排会话。**不产生汇报**（「关了」那条是 `observe_pane_closed` 的事） |
| `observe_status(&mut self, session_pane_id: u32, status: &str, cause: Option<&str>, now: Instant) -> bool` | 吃一次状态变化（字段与 `monitor::StatusChange` 同口径）。返回**本次是否产生了新汇报**，12 据此决定要不要唤醒投递泵，省一次回头查收件箱 |
| `observe_pane_closed(&mut self, session_pane_id: u32, now: Instant) -> bool` | 出一条 `Closed`（带走任务清单）并置「已关闭」位；重复调答 `false`。返回是否产生了汇报 |
| `note_task_written(&mut self, session_pane_id: u32, task_id: &str, target_status_at_write: &str, now: Instant)` | 派活写入。编号进「尚未汇报」清单；写入时目标不是 `ai-working` 才登记未接收等待。无返回值（不会立刻产生汇报） |
| `tick(&mut self, now: Instant) -> bool` | 把超过 15s 的未接收等待报出去（报完即摘）。返回是否产生了汇报 |
| `has_pending(&self, orchestrator_pane_id: u32) -> bool` | 这个编排者有没有积压 |
| `pending_orchestrators(&self) -> Vec<u32>` | 此刻有积压的全部编排者（顺序不保证）。**清单外新增**：投递泵每拍要照着它扫，否则 12 得在外面另养一份编排者名单 |
| `take_batch(&mut self, orchestrator_pane_id: u32) -> Option<ReportBatch>` | 一次取空并把 `dropped` 带出去归零；空答 `None`（取空后连收件箱条目一起销毁） |
| `requeue_batch(&mut self, orchestrator_pane_id: u32, batch: ReportBatch)` | **清单外新增**：把取走的一批放回**队首**（12 遇 `desktopBusy` / 写失败时下一拍再试），`dropped` 一并还回；放回后仍受 `INBOX_CAP` 约束 |
| `drop_inbox(&mut self, orchestrator_pane_id: u32)` | 丢掉收件箱，**并把它名下受编排会话的追踪一起忘掉**（见下） |
| `reported_cursor(&self, session_pane_id: u32) -> usize` | 已汇报到 transcript 的第几条消息；陌生 pane 答 `0` |
| `set_reported_cursor(&mut self, session_pane_id: u32, cursor: usize)` | 回写游标；陌生 pane 静默忽略（不会凭空建出一条追踪） |

常量：`ACK_TIMEOUT = 15s`、`INBOX_CAP = 50`、`PENDING_TASKS_CAP = 200`（后者是清单外的护栏，见留档）。

### 判定上的三个取舍（12 接线前值得知道）

1. **游标是「消息条数」不是字节偏移**：`TranscriptSource::read` 交回的是
   `Vec<AiSessionMessage>`，12 渲染增量就是 `&msgs[cursor..]`，渲染完回写 `msgs.len()`。
   账本自己不解释这个数，换成别的口径也不会崩——但两边得同一个口径。
2. **attention 事件也可能开启一个回合**：`AwaitingHuman` 之后照常走状态转移，
   于是 Codex 那条「ai-working/PermissionRequest 打头」的序列里，回合起点落在
   `PermissionRequest` 那一刻而不是其后的 `PostToolUse`。清单要求的
   「不结束、不重置」全部满足（`turn_started_at` 是 `Option`，只在 `None` 时才写入），
   而回合耗时因此更接近真实。
3. **`drop_inbox` 顺手忘掉名下的受编排会话**：清单第 11 条要的是「编排者收件箱被丢弃后
   **不再累积**」。只清一次队列做不到——乐手不陪葬（ADR 0003），它后续每一次 `Stop`
   都还会打进 `observe_status`，而收件人已经没了；留着追踪就是留一条只涨不消的队列。
   忘掉之后它们变回「未注册的 pane」，一切事实一律忽略；编排者若在同一编号上重新拿到
   授予，12 会照记账表重新 `register_session`。**12 的调用顺序因此有讲究**：
   `observe_pane_closed`（要拿到 `Closed` 那条）必须早于 `drop_inbox` / `forget_session`。

### 规则 → 测试对照

`cargo test -p mt-ai reports`：37 例，全绿。

| 规则 | 测试 |
|---|---|
| 回合 = 进入 ai-working → 非 attention 的 ai-idle | `进入工作再回空闲算一个回合` |
| 只有进行中的回合才结束；ai-idle → ai-idle 不算 | `空闲到空闲不算新回合结束`、`没开始过的回合不会结束` |
| 回合起点不被中途的 ai-working 重置 | `回合中途不重置起点` |
| attention 只看成因不看状态 | `等待处理只看成因不看状态`、`三种等待成因都认` |
| attention 不结束、不重置回合 | `等待处理不结束也不重置回合` |
| 同一成因再来一次再出一条 | `同一等待成因再来一次再出一条` |
| ai-* → idle 出 Exited、成因原文、回合作废不另出 TurnEnded | `退出作废进行中的回合且只出一条`、`停摆退出的成因是原文` |
| 退出只报一次 / 裸 shell 不报 / 退出后重开照常追踪 | `退出只报一次且裸_shell_不报`、`退出后同一_pane_重开照常追踪` |
| 关闭出 Closed，之后一切事实忽略 | `关闭出一条汇报且此后一切事实被忽略` |
| 未被接收：15s 超时报一次、报完即收敛 | `派活超时未开始处理出未被接收`、`多条未接收等待各自到点` |
| 转到 ai-working 即清等待、不出汇报 | `转入工作即清掉未接收等待` |
| 写入时已是 ai-working 的不登记 | `写入时已在工作中的派活不登记等待`（反面：`写入裸_shell_的派活照样登记等待`） |
| 任务编号随终结性汇报走并清空 | `回合结束带走任务清单并清空`、`退出与关闭同样带走任务清单` |
| AwaitingHuman / NotAccepted 不带走清单 | `等待处理与未被接收不带走任务清单` |
| 收件箱上限 50、溢出丢最旧并累加 dropped | `收件箱溢出丢最旧并累加丢弃计数` |
| take_batch 一次取空并把 dropped 归零 | `取批次一次取空并把丢弃计数归零` |
| 放回队首（12 的重试路径） | `放回批次回到队首并恢复丢弃计数` |
| 未注册的 pane 一律忽略（含编排者自己） | `未注册的_pane_的事实一律忽略` |
| 关闭 / 收件箱被丢弃 / 记账被回收后不再累积 | 见上「关闭」一行、`收件箱被丢弃后不再累积`、`忘掉的乐手不再产生汇报` |
| 重复登记幂等 | `重复登记不冲掉进行中的状态` |
| Claude 典型序列只出一条 TurnEnded | `claude_典型序列只出一条回合结束` |
| Codex 审批序列出一条 AwaitingHuman + 一条 TurnEnded | `codex_审批序列出一条等待处理与一条回合结束` |
| 用户打断 / 停摆兜底的成因原文 | `用户打断出回合结束且成因是原文`、`停摆兜底出回合结束且成因是原文` |
| 无成因（降级路径）照样追回合 | `无成因的状态变化照样追得出回合` |
| 两个乐手 / 两个编排者各自独立 | `两个乐手各自独立`、`两个编排者各自独立` |
| 汇报游标读写 | `汇报游标可读写且陌生_pane_答零` |
| 待汇报任务清单上限（清单外护栏） | `尚未汇报的任务清单有上限` |

## 留档（实施方填）

1. **清单外多了三样东西**，都写在上面的 API 表里：`pending_orchestrators()`、
   `requeue_batch()`（12 明写要「把这批放回收件箱头部下一拍再试」，没有它 12 得逐条重推、
   还会把 `dropped` 洗掉）、`PENDING_TASKS_CAP = 200`（病态情形的护栏：受编排会话卡在
   `ai-working` 再也没结束过回合，而编排者还在一条条派活，「尚未汇报」清单会无限长；
   与工单 10 任务账本「每编排者最近 200 条」同量级，溢出丢最旧）。
2. **`observe_status` / `observe_pane_closed` / `tick` 返回 `bool`** 而不是清单里的 `()`：
   「本次是否产生了新汇报」是 12 唤醒投递泵的判据，返回它能省掉一次「放锁再回头查
   `has_pending`」的往返。不需要的调用方 `let _ =` 即可。
3. **`Exited` 之后不销毁追踪**：同一 pane 里 `claude -c` 重开是常见操作，销毁会让下一个
   回合彻底失踪。有测试钉住（`退出后同一_pane_重开照常追踪`）。
4. **`requeue_batch` 在 `drop_inbox` 之后会把收件箱重新建出来**（`entry().or_default()`）。
   正常路径碰不到——12 收到 `PaneGone` 是丢收件箱而不是放回。真放回了也只是多留一批
   死信在内存里（名下受编排会话已被忘掉，不会再涨），下一拍投递发现 pane 没了自会丢掉。
   没为这一档加判定：加了就得在账本里引入「这个编排者已经死了」的墓碑，那是一张只涨不消的表。
5. **本票交付时零调用方**，模块头挂着 `#![allow(dead_code)]`。**工单 12 接线后必须把它连同
   注释一起删掉**，否则这块以后新增的死代码会被静默放过。
6. **`cargo build -p mt-ai` 有 3 条 warning 全部来自并行的工单 10**（`control.rs` 的
   `AtomicBool` 未用、`TokenRegistry::tasks` / `OrchestratorTasks::{next_seq, tasks}` 未读），
   `reports.rs` 自身零 warning。
