# 06 — 等待：wait 长轮询四类终态

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** `wait` 长轮询等待某乐手状态收敛，支持超时，返回四类终态：`ai-idle`（干完，含 cause）/ `attention`（停在等审批或向人提问，含原因，如 PermissionRequest）/ `idle`（agent 已退出）/ pane 不存在。状态判定完全复用 hook 权威状态机与既有兜底（停摆收敛 / 用户打断），不新增判定逻辑。attention 时编排者不代答（ADR 0003 铁律）：它拿到状态后在自己对话里播报请用户处理，零新增 UI——既有黄灯徽章兜着。

**Blocked by:** 03（先有乐手可等）

**Status:** done（80ff245）

- [x] 四类终态 + 超时语义在主缝测试全覆盖（经假宿主驱动状态迁移）
- [x] attention 返回携带原因原文
- [x] 向非自启 pane wait 被拒（「不存在」语义）
- [ ] 真机：乐手挂黄灯 → 编排者播报 → 人工在乐手 pane 处理 → 下一次 wait 拿到恢复后的终态，全流程走通 ← **待工单 09 真机验收**（本票不启 GPUI 实例：并行工单占着 `target/debug`）。进程内那一半已由 `wait_人工处理后下一次拿到恢复后的终态` 走通

## 设计决议

**`wait` 不走桌面主线程那条泵，但照样进「另起线程」那张表。** 泵的时限
`ACTION_TIMEOUT` 是 3 秒、为「建一个 pane」定的，而 `wait` 要等的是一个 AI 回合
——几分钟是常态，差两个数量级。于是它一次也不打扰 gpui 主线程：就在
`try_handle_control` 起的那条一次性线程上反复问 `OrchestratorActions::pane_liveness`
（那个方法的契约本来就是「很快、不跳主线程」，读的都是 `Arc<Mutex<..>>` 后面的
只读状态）。

那张表因此**改了名**：`blocks_on_desktop()` → `needs_own_thread()`。工单 03~05 里
进表的理由只有一种（要回主线程等回执），名字与判据恰好同义；`wait` 打破了这个巧合
——它不碰主线程，却要在那条线程上睡几分钟。而 `try_handle_control` 关心的从来只是
结论：**别在 HTTP 那条单线程循环里就地做**。hook 上报排在同一条队上，`wait` 就地跑
会把 AI 状态感知那条权威通道卡住几分钟（`start-session` 最多卡三秒）。工单 07 的
`read` 要回主线程读画面，同样进这一档。

**轮询期间一把锁都不持。** `resolve_target` 出锁之后才问死活，循环里手上只剩一个
`Arc<dyn OrchestratorActions>`（`actions()` 早就把那把锁放了）。持着 `registry` 睡
几分钟会把整个控制面挂住——连 `revoke_pane` 都进不来。

**判定完全从既有事实读出来，一条新逻辑都没加。** 三档终态由
`PaneLiveness::settled()` 算出，输入只有三样既有事实：`monitor::resolve_status` 的
状态（hook 权威 + 它落盘之后的两条兜底结论）、`AiSessionState` 三态、以及
`hook_server::is_attention_cause` 这个现成判据。`wait` 一个字都不写回状态机。

| 终态 | 判据 | 线上 `outcome` |
|------|------|----------------|
| 干完了 | `status == "ai-idle"` 且成因不属 attention | `ai-idle`（带 `cause`） |
| 等你处理 | **成因**属 attention（`PermissionRequest` / `Elicitation` / `StopFailure`） | `attention`（带 `cause`） |
| agent 已退出 | `status == "idle"` 且 `ai_session == Ended` | `idle` |
| pane 不存在 | `resolve_target` 三条语义 | 错误码 `paneNotFound` / `selfTarget` / `paneGone` |

**判定顺序：先看成因，再看状态。** attention 与状态**不是**一一对应的——Claude 的
`PermissionRequest` 映射成 `ai-idle`，Codex 的映射成 `ai-working`（批准后直接执行
工具，`hook_server::map_event_to_status` 对它有专门一条）。反过来先看状态的话，一个
正等着 Codex 审批的乐手会被 `ai-working` 那一档吞掉、一直等到超时——而 attention
恰恰是最该立刻告诉人的那一档。这条差异有一例专门的测试（同一个 cause 配两种状态）。

**成因原文照带，不归一。** `ai-idle` 底下躺着三件完全不同的事：`Stop` 是真做完，
`Interrupt` 是用户按了 Esc（兜底之一），`Stall` 是停摆兜底收敛的。把它们归一成
「完成」就是把半截活报成交付。CLI 的 `--help` 里写死了「only Stop means the work
really completed」。

**成因的来源是发射器的去重表，不是 hook 状态。** `HookState` 只存状态，不存成因；
成因住在 `StatusEmitter::prev`（`StatusChange::cause` 的来源）。于是新开一条只读的
`AiPerception::cause_of()`，`StatusEmitter::last_cause` 从 `pub(crate)` 放开成 `pub`。
**刻意用这一份而不是另养一份**：停摆兜底避让 attention pane 读的就是它
（`monitor::stall_settle_target`），托盘黄灯也是——三处同一份事实，不会漂。

**`Unknown` 那一档 fail-closed：不收敛，等到上界答 `pending` + `status: "idle"`。**
没 hook、输入检测也没认出来的自定义启动器（ADR 0003 明说任何启动器都能当乐手），
`resolve_status` 恒答 `idle`，桌面侧**说不上来**里头还有没有 agent 在跑。谎报成
`idle`（已退出）会让编排者去重起一个还在跑的活；谎报成 `ai-idle`（干完了）会让它把
没开始的活当成交付。两个都比多等一会儿坏得多。`pending` + `status: "idle"` 这对组合
是「这个乐手我看不透」的**唯一签名**（`pending` + `ai-working` 才是「真在跑」），
`--help` 里写清了怎么读它、以及那种乐手上 `wait` 永远不会收敛。

**超时不是错误，是一条正常回执。** 等到耐心用尽仍未收敛 → **200 + `outcome:
"pending"`**，不是 HTTP 错误码。理由：「它还没干完」是一条正常的观测结果，编排者据此
决定继续等还是先去干别的；做成错误码就得给它一个 CLI 退出码档位，而那三档说的都是
「你的请求不对 / 我们出了问题」，两样都不是。于是**四类结论全部退出码 0**，编排者必须
读 `outcome`——这一条在 `--help` 里被单独强调（`the exit code is 0 for all of them`），
并由一条测试钉住。

**上界 300s / 默认 60s / 节拍 250ms。**

| 常量 | 值 | 理由 |
|------|-----|------|
| `WAIT_MAX` | 300s | 下界是「一个 AI 回合正常要多久」（几分钟是常态，短于它就是逼编排者不停重投，每次重投都是一次进程启动）；上界是「出岔子最多白占一条线程多久」。超上界**钳而不拒**——上界是我们这侧的实现约束，编排者无从知道，为一个能安全钳回来的数字报错只是多一趟往返；`waitedMs` 让它看得出被钳过 |
| `WAIT_DEFAULT` | 60s | 照着**编排者自己那侧的工具调用超时**定：`wait` 是同步阻塞的 CLI 调用，跑它的 agent 通常给一次 shell 调用两分钟。默认值必须稳稳落在那之内，否则「不给 `--timeout` 直接用」这条默认路径就是「命令被自己的宿主 kill 掉」 |
| `WAIT_POLL_INTERVAL` | 250ms | 比 monitor 那条 500ms 轮询快一档，但**不是**为了抢在它前面——hook 事件是在 HTTP 线程上**同步**落进状态与去重表的，状态变化本来就不用等轮询那一拍。250ms 只是把「回合边界 → 编排者拿到回执」压到半秒以内，代价是每秒四次几把互斥锁的读 |

节拍**不做成可注入**：主缝测试靠请求里那个 `timeoutMs` 把整轮压到几百毫秒，用不着
为它另开一个只有测试会拨的旋钮。`timeoutMs: 0` 是合法值，语义是「只看一眼就回来」
——顺带给了编排者一条非阻塞查状态的路，省得为它另加一条命令。

**CLI 的读超时必须按命令放大。** `READ_TIMEOUT` 是 5 秒，而 `wait` 会在服务端睡到
几分钟——照 5 秒读的话长轮询每次都是 CLI 先断线，编排者拿到的会是「够不着」而不是
终态，这条命令整个不可用。于是 `Command::read_timeout()`：只有 `wait` 走
`mt_agent_control::wait_read_timeout(requested) = requested.min(WAIT_MAX) + READ_TIMEOUT`，
其余照旧。**写超时不跟着放大**——请求体早就发完了，长轮询等的是响应。
`WAIT_MAX` / `WAIT_DEFAULT` 因此两侧各有一份常量（sidecar 够不到 `mt-ai`），
由 `tests/orchestrator_wire.rs` 拿**两侧真值**钉住相等 + 读超时留富余，
与 `ACTION_TIMEOUT` ↔ `READ_TIMEOUT` 那条是同一种保险。

**回执刻意不复用 `PaneView`。** 那一份是「这个乐手是什么」（项目 / 启动器 / 死活），
这一份是「这一次等待的结论」。`status` 与 `outcome` 都留着，**两处场合非它不可**：
`attention` 时 `status` 说明停在 `ai-idle`（Claude）还是 `ai-working`（Codex）；
`pending` 时它是唯一能区分「真在跑」与「看不透」的东西。三个终态名与 `status`
**同一套词汇**（`ai-idle` / `idle`），编排者不必认第二种拼法。

**`wait` 期间 pane 被关掉 → `paneGone`（410），不是憋到上界给 `pending`。**
「你起的那个已经关了」是一个确定的结论，编排者该立刻知道；与 `resolve_target` 开头
那一档同码，`send` 也用它，编排者只需认识一种。

**新错误码：一个都没有。** 四类终态里的「pane 不存在」复用可见范围铁律那三条
（`paneNotFound` 404 / `selfTarget` 403 / `paneGone` 410），全部经
`ControlPlane::resolve_target`；请求缺 `targetPaneId` 是 `badRequest`。超时是成功
回执，不需要码。

**attention 时本命令一个字节都不写。** ADR 0003 的「不代答」在这里没有可绕的路——
`wait` 手上只有 `pane_liveness` 这条只读缝，`send_input` 压根不在它的调用图里。
主缝有一条测试在 attention → 人工处理 → 恢复的整条路径上断言 `FakeActions::sends`
为空；辅缝那个 attention 假宿主的 `send_input` 直接 `unreachable!()`。
**零新增 UI**：那个 pane 的黄灯徽章本来就亮着（`is_attention_cause` 是同一份判据）。

## 留档（未整改）

- **`send` 之后立刻 `wait` 会拿到上一回合的 `ai-idle`**（本票最需要真机复核的一条）。
  写穿之后那一瞬 agent 还没来得及发 `UserPromptSubmit`，状态仍是上一回合的
  `ai-idle` + `Stop`，`wait` 就地收敛，编排者拿到一个假的「干完了」。回执里
  `waitedMs` 接近 0 是它唯一的签名，`--help` 里写了这条与应对（再 wait 一次）。
  **刻意不修**：正确的修法要么是「先等它动起来」的启发式（那正是「不新增判定逻辑」
  要挡的东西，且回合极短时它自己也会误判），要么是让 `send` 回执带一个可比较的
  回合序号（那是 05 已定稿的形状，且 hook 那侧没有现成的回合计数）。留工单 09
  真机看它实际有多容易撞上，再决定要不要在 09 一并处理。
- **无 hook 的乐手上 `wait` 永远不收敛**（`pending` + `status: "idle"`）。这是工单 03
  留档那条「`Unknown` 按占名额算」的另一面：`resolve_status` 对它恒答 `idle`，而
  fail-closed 不许把「说不上来」谎报成终态。代价是那类乐手每次 `wait` 都白等满耐心
  （默认 60s 一条线程）。修它要动降级状态机本身（给无 hook 路径加退出判定），
  那是 v0.9.3 假完成重复播报踩出来的铁律区，与本票范围不成比例。`--help` 里告诉
  编排者认这个签名、改走 `read`（工单 07）或问用户。
- **降级路径上 `ai-idle` 可能只是「安静了 3 秒」**。无 hook 时 `resolve_status` 按
  输出活跃度判：AI_ACTIVE_TIMEOUT（3s）无输出即 `ai-idle`。于是一个被输入检测认出来
  的 opencode 乐手安静思考 3 秒，`wait` 就会报「干完了」（且没有 cause）。这是降级
  路径的既有性质（徽章也是这么显示的），不是 `wait` 新引入的；编排者能从
  `cause` 缺失认出「这是个没有 hook 的会话，结论没那么硬」。
- **`wait` 期间不复查授予**。编排者的令牌若在轮询中途被撤销（它自己的 pane 被关），
  这一趟仍会跑完并答复。无害——跑 CLI 的那个 pane 本身也已经没了，答给谁都无所谓；
  复查一次要重进 `registry` 锁，换不来什么。
- **同一个乐手上的并发 `wait` 各占一条线程**。没有去重/合并：编排者是串行调 CLI 的
  单进程，实际到不了这个场景；真撞上也只是多几条大部分时间在睡的线程。
- **`attention` 的成因可能滞后一小会儿**。黄灯的清除发生在 UI 侧（用户对该 pane 键入
  即视为已在处理），发射器的去重表**感知不到**——于是用户批准之后、下一个 hook 事件
  抵达之前的那个窗口里，`wait` 仍会答 `attention`。窗口是毫秒级（批准后 agent 立刻
  发 `PreToolUse` 之类），且多答一次 attention 的后果只是编排者多播报一句、下次
  `wait` 拿到正确的值。**刻意不去读 UI 侧那份 attention 标记**：那会把控制面绑到
  渲染层的状态上。
- **`paneId` 是全局 PTY 编号**（工单 03 留档同款，本票未动）。
- **CLI ↔ 真 hook server 的整条 HTTP 往返仍未真机走过**（与工单 02/03/05 同一条），
  主缝/辅缝都是进程内对账。留工单 09 验收——`wait` 是其中最值得真机看的一条：它是
  唯一一条读超时被放大、且连接要开着好几分钟的命令。
