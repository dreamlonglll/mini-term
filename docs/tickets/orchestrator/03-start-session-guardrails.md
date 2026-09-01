# 03 — 启动乐手：start-session + 范围/上限/套娃裁决

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** 编排者能真的起乐手。`start-session` 按启动器 id 在可达项目里启动受编排会话（复用 01 的共享入口；出生礼仪沿用 ADR 0002——不抢焦点、不切项目、一次性提示）；`list-panes` 列出自己乐手及其状态。服务端裁决四条边界（ADR 0003）：分组外项目明确报错；并发上限默认 5——只计存活的 AI 会话中乐手、退出即释放名额、超限返回明确错误（不静默排队）；受编排会话不注入编排令牌（禁套娃）；目标是编排者自己即拒绝（自指禁令）。可见范围铁律：对任何非自启 pane 的读写一律以「不存在」语义拒绝。范围记账（谁 spawn 了谁）由控制服务持有；编排者 pane 重启即新身份，MVP 无收养。

**Blocked by:** 01（共享入口）、02（控制面骨架与令牌）

**Status:** done（b0ad0a6；两轴评审整改见下方「设计决议」，本轮整改提交在 `refactor: 工单03 两轴评审整改`）

- [x] 编排者可在本项目与同分组项目启动乐手并拿到回执；乐手 pane 正常进入 AI 会话
- [x] 出生不抢焦点不切项目，一次性提示照旧（`LaunchPlacement::Background` + 新文案 `orchestratorStartSession`）
- [x] 第 6 个存活乐手的启动请求得到明确「已达上限」错误；乐手退出后名额释放（两条释放路径各一例）
- [x] 乐手 pane 内跑 CLI 被拒（无令牌）；编排者以自己为目标被拒；读写他人 pane 得「不存在」语义
- [x] 启动器不存在 / 项目不可达 / pane 已关的错误语义各自明确
- [x] 主缝测试覆盖以上全部裁决；`cargo test --workspace` 全绿

## 设计决议

**四条裁决各自落在哪**（全部在 `mt_ai::control`，桌面侧只负责「照做」）：

| 裁决 | 落点 |
|------|------|
| 分组外项目 | `ControlPlane::start_session` 的 `reachable_projects` 查找 → `projectUnreachable`。**组外与不存在同码**：区分开来就是一台项目扫描器 |
| 并发上限 | 同函数的 `live_session_count() >= session_cap()` → `sessionLimitReached`（429，不静默排队） |
| 禁套娃 | **类型层面**：`StartSessionSpec` 里没有「要不要授予」这个位；桌面侧唯一的落地请求构造处 `orchestrator::orchestrated_launch_request` 传具名常量 `ORCHESTRATED_GRANT`（恒 `None`），与 `from_launcher` 那条授予路互斥。**那个调用点由测试直接钉住**（把 `grant:` 那一行改成 `from_launcher(&launcher)` 即红）——「类型上没有那个位」只保证了控制面→桌面这一段，真正决定发不发令牌的是那一行 |
| 自指禁令 | `ControlPlane::resolve_target` 的第一行 → `selfTarget` |

**记账契约：乐手一落地就记账，再谈回执送不送得到。** 起乐手是一次跨线程往返，发起侧兜着超时；「先答复、答复到了才记账」会在超时那一瞬长出**幽灵乐手**——桌面上真有一个受编排会话在跑，却不进 `list-panes`、不占名额、`resolve_target` 答「不存在」，一次违反 ADR 0003 三条（可见范围 / 上限只计活着的 AI 会话 / 不静默排队）。于是：

- `StartSessionSpec` 本身就是一张**落地登记凭据**，字段私有、只能读；动作实现只能靠 `spec.landed(pty_id)` 造出 `StartedSession`（那个类型没有别的构造路径），而 `landed` 做的第一件事就是把记账写进 `registry.sessions`。「拿到回执 = 记账已落地」因此是**类型层面**成立的。展示用的项目名/启动器名随 spec 一起带下去（控制面查宿主时顺手拿到），桌面侧原样回填，记账落地那一刻就是完整的。
- 控制面 HTTP 侧**不再插第二次**——`registry.sessions` 只有 `register_landed` 一个写入点。
- 泵在**动手之前**先看时限（信号带一个 `Deadline`，由发起侧算好的绝对时刻）：过期就整条丢掉、连 pane 都不建。队列积压时这是最省的止损，而且没人在等那个回执了。
- 「命令写进去了却查不到 PTY 编号」那条同类窄口子直接消掉：PTY 编号随 `LaunchOutcome::pty_id` 一起回来（`pty_alive` 本来就是从同一次查找算出来的），`command_delivered()` 为真即蕴含它在场。反过来，命令**没**送达时刻意**不登记**——那儿只是一个裸终端，登记了它会以「AI 会话状态不可知」永久占住一个名额。

**存活判据：fail-closed 三态。** `PaneLiveness` 的「在不在 AI 会话里」是 `AiSessionState`（`Active` / `Ended` / `Unknown`），不是拿状态字符串与 `"idle"` 裸比。占名额的判据是 `alive && ai_session != Ended`——**只有明确结束才释放名额**。宿主侧的判据（`orchestrator::ai_session_state`，只读现有事实，不动 AI 状态机）：

| 情形 | 结论 |
|------|------|
| `status != "idle"` | `Active`（hook 说 `ai-*`，或输入检测认出来了） |
| `status == "idle"` 且该 pane **hook 已启用** | `Ended`（hook 一旦启用即权威，`idle` 就是 SessionEnd 落地/停摆兜底判了已退出） |
| 其余 | `Unknown` → **按占名额算** |

`Unknown` 那一档就是评审揪出的洞：命令不在 `AI_COMMANDS` 里的自定义启动器（ADR 0003 明说任何启动器都能当乐手）又没 hook 时，`resolve_status` 恒答 `idle`；按「不占」判，硬上限形同虚设、可无限起。按「占」判最坏是少起一个，编排者收到明确的 `sessionLimitReached` 自己排队——两害相权取轻。代价见留档第一条。

`alive` 仍走 `AiBridge::is_pane_live`（500ms 轮询那份活 pane 名册），`status` 仍走 `AiPerception::status_of`。**每次请求现查**，不靠事件驱动记账；三样都在后台线程读得到，`pane_liveness` 因此不跳主线程。

**上限的注入位**：`DEFAULT_SESSION_CAP: usize = 5` + `ControlPlane::set_session_cap()`，内部是一个 `AtomicUsize`（**不加第二把锁**——多一把就多一组与 `registry` 交叉持有的可能）。工单 08 只需在装配处调一次 setter，裁决那一行一个字不动。

**超时 3 秒**（`mt_ai::control::ACTION_TIMEOUT`——常量住在 `mt-ai` 而不是桌面侧，因为它是「控制面等 / 桌面兜 / CLI 读超时留富余」的三方约束）：下界是「建一个 pane 正常几十毫秒」的一个数量级以上；上界是 CLI 的 `READ_TIMEOUT`（`mt_agent_control::READ_TIMEOUT`，5 秒）必须留富余。**那条跨工作区不等式由 `crates/mt-ai/tests/orchestrator_wire.rs` 拿两侧真常量钉住**（此前它拿字面量 `Duration::from_secs(5)` 断言，改了 CLI 那侧不会红，是假保险）。回执走 `std::sync::mpsc::sync_channel(1)` 而不是 futures oneshot——HTTP 线程是裸线程，没有执行器可 `block_on`。

**`desktopBusy` 的语义**：它**不是**「没起成」，而是「桌面没在时限内答复，**那个会话可能已经起来了**」——记账在乐手落地那一刻就已落地，没答上来的只是这一趟回执。于是：

- 错误消息改成 `the desktop did not answer in time; the session may have started anyway - run list-panes before retrying`；
- CLI 仍归退出码 3（那一档的含义是「改请求也没用」，不是「重试就好」），但 `--help` 的 `after_help` 里写清了这条，`ControlFailure::is_desktop_unavailable` 的注释同步。让重试不再是无脑推荐动作。

**阻塞命令另起线程**：hook 那个 HTTP 服务是**单线程 for 循环**，`start-session` 就地阻塞会把排在后面的 hook 上报一起卡住（AI 状态感知是本仓的权威通道，不给编排让路）。于是 `try_handle_control` 对 `blocks_on_desktop()` 的命令：先在 HTTP 线程做掉鉴权（挡住「任意进程都能让我们起线程」），再把已鉴权的活丢一条一次性线程。一条测试用 600ms 的慢动作 + 并发 `/hook` 请求钉住这条。

**命令名表收成一处**：`control::Command` 枚举（`ALL` 定长数组 + `name()` / `blocks_on_desktop()` 两处**穷尽 match、无 `_` 兜底**）。此前命令名与阻塞属性各写一处，工单 06 的 `wait` 必然阻塞、漏登记就静默卡住 hook 队——正是这条设计要防的事。现在加一个变体，`ALL`、`name`、`blocks_on_desktop`、`handle` 的分发**四处一起编译不过**。

**记账保留策略与修剪上限**：`revoke_pane` 只作废「该 pane 作为**编排者**」名下的记账（不杀乐手）；乐手自己关闭时那次 `revoke_pane` 刻意**不抹掉**它的记账条目——抹掉了「已关」就退化成「不存在」，而「你起的那个已经关了」是编排者应该知道、且不泄露任何东西的信息。只增不减的真代价不止「列表变长」：`live_session_count` 每次数名额要对**历史全部**乐手各问一次死活。两笔整改：

- 数名额走只取 `pane_id` 的 `session_ids_of`，不再为拿一个编号克隆整条记账的六个 `String`；
- 每个编排者的记账条数上限 `MAX_SESSIONS_PER_ORCHESTRATOR = 50`，超出时**只丢已经关掉的、从最旧的丢起**（`revoke_pane` 走到乐手自己那次会在 `registry.closed` 里记一笔，仅供修剪用；对外的 `alive` 一律现查）。**活着的一条都不能丢**——丢掉一条活着的记账就是造一个幽灵乐手，丢不动就让它超着。50 是「远大于并发上限、又不至于让数名额变贵」的一个数。

**新错误码清单**（闭集，CLI 按 code 分档）：

| code | status | 含义 |
|------|--------|------|
| `launcherNotFound` | 404 | 启动器 id 不在名单里 |
| `projectUnreachable` | 403 | 目标项目不可达（组外 / 不存在，**刻意同码**） |
| `remoteProjectUnsupported` | 409 | SSH 远程项目当不了乐手宿主 |
| `sessionLimitReached` | 429 | 已达并发上限 |
| `startFailed` | 500 | 终端没建成 / 命令没交到活着的 PTY（**这一档确实没起成**，不留记账） |
| `desktopBusy` | 503 | 主线程没在时限内答复——**会话可能已经起来了**，先 `list-panes` 查一眼（CLI 归退出码 3） |
| `selfTarget` | 403 | 自指禁令 |
| `paneNotFound` | 404 | 不是自己起的乐手（含压根不存在）——统一「不存在」语义 |
| `paneGone` | 410 | 是自己起的，但那个 pane 已关 |

**线上形状新增**：请求体加 `launcherId` / `projectId` / `targetPaneId`（都 `skip_serializing_if`，`list-*` 的请求体与工单 02 一字不差）；响应加 `PaneView`（`start-session` 回执与 `list-panes` 每条同形）；`ProjectView` 加 `canStartSessions`（远程项目为 `false`，省得编排者白试一次）。`ControlProject` 加 `ssh_connection_id`（**照实投影**，与 `mobile_relay::to_relay_project` 同一份事实；折成 `remote: bool` 会把裁决提前到配置层）。

**`targetPaneId` 提前就位**：05/06/07 的 send/wait/read 要用，两侧同时定稿省得 05 再动一次 CLI 的线上形状；桌面侧暂 `#[allow(dead_code)]`。共享裁决函数 `resolve_target` 本票落地并测试（三条语义各一例），命令留给后续工单。

## 留档（未整改）

- **无 hook 的 agent 自行退出后名额不释放**（两轴评审第 2 条的另一半，本轮**刻意不修**）。opencode/pi 这类只靠输入检测识别的 agent 退出时，`SessionTracker` 的会话标记不一定被清（输入检测只认得 `exit` / Ctrl+D / `/exit` 那几种形态），而停摆兜底的第一道闸要求「hook 已启用」——于是 `resolve_status` 停在 `ai-idle`，宿主判为 `Active`，名额一直占着。**最坏情况**：编排者起的 opencode 乐手跑完退了，那个名额要等用户亲手关掉那个 pane 才还回来；上限默认 5，编排者会提前吃到 `sessionLimitReached`。
  为什么不在这儿修：修它得动降级状态机本身（给无 hook 路径加一条退出判定），而「降级结论必须落盘、触发一次即收敛」是 v0.9.3 那版假完成重复播报踩出来的铁律区，改动风险与本票范围不成比例。真要修，正确的位置是 `mt-ai::monitor` 的降级路径，不是编排控制面。
- **诞生提示复用 `ToastKind::MobileSession`**：渲染与交互口径完全相同（info 图标 + 点击切项目 + 用 message），只有文案不同。更诚实的名字是 `RemoteSession`（移动端与编排者都是「远程发起」），但那是纯重命名、要碰 `mobile_relay.rs` 与 `toast.rs` 的既有代码，合并冲突面大于命名收益。等 04~08 都稳定后一并改。
- **`orchestrator::start_session` 与 `mobile_relay::try_start_session` 的收尾三件套近乎逐行同形**：`LaunchPlacement::Background` 落点 + `ToastKind::MobileSession` 的诞生提示 + `command_delivered()` 的成功判据。工单 01 已经把四步落地动作抽进共享入口 `AppStore::launch_ai_session`，剩下的这三行再抽一层的收益不抵耦合——两条路径的回执类型、错误集与出身文案都不同，抽出来就得再参数化三样东西。
- **`set_host` 与 `set_actions` 两个装配处分离**：宿主在 `ai.rs` 一开机就注入（不依赖窗口），动作泵在 `orchestrator::install` 里注入（要 `window` 才建得了 pane）。时机不同是真实约束，判为可接受；代价是「控制面接没接线」要看两处。
- **`paneId` 是全局 PTY 编号**：编排者从自己两次 `start-session` 回执的编号跳变，能推断出这中间桌面上另有 pane 被创建（用户自己开了几个终端）。与可见范围铁律的精神相悖，但泄露量级极小（只有一个计数器的增量，看不出是什么 pane、在哪个项目）。换成每编排者独立编号要多一张映射表与一层翻译，不值。
- **`start-session` 走两遍 parse + authorize**：`try_handle_control` 先在 HTTP 线程 `authorize_body` 一遍，把活丢进线程后 `handle` 再解析鉴权一遍。这是「起线程之前先挡住无令牌请求」的必要代价——鉴权是一次哈希查找，比让任意进程靠一串无令牌请求让我们不停起线程便宜得多。
- **并发上限有个 TOCTOU 窗口**：同一个编排者并发发两条 `start-session` 时，两条都可能在各自数完名额之后才落地，于是短暂超出上限一个。唯一的收紧办法是持 `registry` 锁跨过 `actions.start_session()`（一条会阻塞好几秒的外部调用），代价远大于收益；而编排者是串行调 CLI 的单进程，实际到不了这个窗口。
- **`list-panes` 会列出已关的乐手**：记账在编排者 pane 生命周期内只增不减（`alive: false`），这是有意的（「已关」比「不存在」有用）。本轮已给它加了 50 条的修剪上限（只丢已关的、从最旧丢起），但「列表变长」在到达上限之前仍然成立。工单 04 做「编排者已离场」标识时可以顺带定一条展示侧的折叠口径。
- **`alive` 判据依赖 `AiPerception::pane_closed` 这条路径**：pane 关闭时若哪天不再调它（比如整项目关闭走了别的路），乐手会永远显示为活着并占着名额，且 `registry.closed` 也标不上、修剪跟着丢不动。当前 `AiBridge::remove_pane` 是唯一注销点，与令牌撤销是同一处，自洽；但这条耦合值得在改动 pane 生命周期时复查。
- **CLI ↔ 真 hook server 的整条 HTTP 往返仍未真机走过**（与工单 02 同一条），主缝/辅缝都是进程内对账。留工单 09 验收。
- **`mt-agent-cli` 与 `miniterm-hook` 的端口发现同形异构**（工单 02 已留档，本票未动）。
- **「乐手」这个词在实现里有一百来处**（注释与测试名），与 `CONTEXT.md` 的术语「受编排会话」分叉。判为**不改**：改了反而难读。已在 `CONTEXT.md` 的术语表里把它登记成口语别名，并写死「任何用户可见文案一律用受编排会话 / orchestrated session」——用户可见面由测试兜（CLI 的 `--help` 与控制面的错误消息各有一条 `!contains("musician")` 断言）。
