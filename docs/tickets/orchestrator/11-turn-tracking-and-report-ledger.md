# 11 — 回合追踪与汇报账本（纯状态机，新模块）

**Parent:** ADR 0004（受编排会话的汇报推送）

**What to build:** 一个**不碰线程、不做 I/O、不认识 `ControlPlane`** 的纯状态机模块 `crates/mt-ai/src/reports.rs`：吃受编排会话的状态变化（`status` + `cause`）、pane 关闭、派活写入这三种事实，按 ADR 0004 的规则产出「汇报」并按编排者攒进各自的收件箱。它是 12 的引擎，本票交付时没有任何调用方——全部价值在它的单元测试里。

**Blocked by:** 无（与 10 并行；12 依赖本票）

**Status:** todo

- [ ] 模块 `crates/mt-ai/src/reports.rs`（`pub(crate)` 即可，12 在同 crate 内接线），`lib.rs` 加一行 `mod reports;`——本票只动这两个文件
- [ ] 类型：`ReportKind`（`TurnEnded { cause: Option<String>, started_at, ended_at }` / `AwaitingHuman { cause }` / `Exited { cause }` / `Closed` / `NotAccepted { task_id }`）、`Report { orchestrator_pane_id, hand_pane_id, kind, task_ids: Vec<String>, at }`、`ReportBatch { reports, dropped: usize }`
- [ ] 账本 API（命名实施方定，语义如下）：`observe_status(hand, status, cause, now)`、`observe_pane_closed(hand, now)`、`note_task_written(hand, task_id, target_status_at_write, now)`、`tick(now)`（未被接收超时）、`take_batch(orchestrator) -> Option<ReportBatch>`、`has_pending(orchestrator)`、`drop_inbox(orchestrator)`、`register_hand(hand, orchestrator)` / `forget_hand(hand)`（12 从记账表同步过来）、`reported_cursor(hand)` / `set_reported_cursor(hand, seq)`（12 渲染完 transcript 后回写）
- [ ] 回合规则：**回合 = 进入 `ai-working` → 转到非 attention 成因的 `ai-idle`**。只有正在进行中的回合才会结束；`ai-idle` → `ai-idle`（例如 Stop 之后闲置提醒把成因换成 `Notification`）**不算**新回合结束
- [ ] attention 规则：任何一次状态变化只要 `cause` 属 attention（`hook_server::is_attention_cause`），就出一条 `AwaitingHuman`，**不看状态**（Codex 的 `PermissionRequest` 停在 `ai-working`）；回合不结束、不重置；同一成因再来一次再出一条（第二次审批是第二件事）
- [ ] 退出规则：从 `ai-*` 转到 `idle` 出 `Exited`（成因原文，`SessionEnd` / `StallExit` 都可能）；进行中的回合随之作废（不另出 `TurnEnded`——退出那条汇报把回合期间的东西一并带走，由 12 用 `reported_cursor` 取增量）
- [ ] 关闭规则：`observe_pane_closed` 出 `Closed`，之后该乐手的一切事实忽略
- [ ] 未被接收规则：`note_task_written` 时 `target_status_at_write != "ai-working"` 才登记等待；之后第一次转到 `ai-working` 即清掉等待（不出汇报）；`tick` 发现等待超过 `ACK_TIMEOUT`（15s）就出 `NotAccepted { task_id }` 并清掉。目标写入时已是 `ai-working` 的**不登记**——那一档看不出来，不猜
- [ ] 任务编号随汇报走：`note_task_written` 把编号挂到该乐手的「尚未汇报」清单；`TurnEnded` / `Exited` / `Closed` 带走清单并清空；`AwaitingHuman` / `NotAccepted` **不**带走（回合还没完）
- [ ] 收件箱：每编排者一个，上限 `INBOX_CAP`（50），溢出丢最旧并累加 `dropped`，`take_batch` 一次取空并把 `dropped` 带出去归零
- [ ] 乐手关闭或编排者收件箱被丢弃后不再累积；未注册的 pane 的事实一律忽略（编排者自己的状态变化也会打进来，必须被过滤掉）
- [ ] 单元测试覆盖上面每一条规则，另加：Claude 典型序列（UserPromptSubmit→PreToolUse→…→Stop）只出一条 `TurnEnded`；Codex 审批序列（ai-working/PermissionRequest → ai-working/PostToolUse → ai-idle/Stop）出一条 `AwaitingHuman` + 一条 `TurnEnded`；用户打断（ai-idle/Interrupt）出 `TurnEnded` 且 cause 原文是 `Interrupt`；停摆（ai-idle/Stall）同理；两个乐手各自独立；两个编排者各自独立
- [ ] `cargo test -p mt-ai` 全绿

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

## 留档（实施方填）
