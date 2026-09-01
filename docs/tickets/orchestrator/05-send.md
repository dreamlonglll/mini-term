# 05 — 派活：send 写穿（bracketed paste 多行）

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** `send` 让编排者向自己的乐手写一段 prompt，语义与移动端指令完全一致——立即写穿不排队，等价本人在桌面敲入同样内容并回车（输入跟踪 / AI marker / SSH autofill 解除一个都不能少，直接走既有写穿入口）。多行文本以 bracketed paste 包裹装配，避免中途换行提前触发发送。

**Blocked by:** 03（先有乐手可派活）

**Status:** done（9952cc3）

- [x] 单行 prompt 正确送达乐手内 agent 并触发执行（装配成一整块粘贴 + 包裹之外的一个回车，主缝逐字断言）
- [x] 多行 prompt（含代码块）作为整体粘贴送达，不提前发送（换行归一成 `\r`，回车在 `ESC[201~` 之后）
- [x] 向非自启 pane send 被拒（「不存在」语义，且一个字节都不落到桌面端）
- [x] 主缝测试：写穿内容与 bracketed paste 装配经假宿主断言（`FakeActions::send_input` 把两份装配结果都抄下来）
- [ ] 真机验证：Claude / Codex / Grok 三家实际收到多行粘贴的表现符合预期 ← **待工单 09 真机验收**（本票不启 GPUI 实例：并行工单占着 `target/debug`）

## 设计决议

**写穿走的是既有入口，不是「另一个长得像的函数」。** 桌面侧落点是
`AppStore::write_to_pane`（`crates/mt-app/src/store/panes.rs:587`）——移动端指令
（`mobile_relay.rs` 的 `RelaySignal::WritePty`）、启动器写命令（`store/launch.rs`
第 5 步）走的都是它。四样语义全挂在它的下游，绕过去就全丢：

| 语义 | 落点 |
|------|------|
| 输入跟踪（AI 会话身份） | `TerminalPane::write` → `observe_input_with_line_snapshot` |
| AI marker（任务锚点） | 同上，`take_submits()` → `arm_marks` |
| attention 黄灯清除 | 同上，`cx.emit(PaneEvent::UserInput)` → `clear_pane_attention_by_pty` |
| SSH autofill 解除 | `PtySession::write` → `disarm_ssh_autofill_on_user_input` |

**装配在控制面、挑选在桌面侧。** [`PaneInput`] 是控制面装配好的**两份**完整字节
序列（包裹版 / 裸版），桌面侧只做一次布尔判断：目标终端此刻开没开
`TermMode::BRACKETED_PASTE`（`AppStore::pane_bracketed_paste`，读的是 VT 状态机
那一位，与用户按 Ctrl+V 时 `mt_ui::paste_to_bytes` 读的**是同一位**，不另存镜像）。

- **为什么不无条件包裹**（工单原本的倾向）：乐手的 agent 退出之后 pane 会退回裸
  shell，`cmd.exe` / PowerShell 都不认 bracketed paste，硬包一层就是往用户的终端里
  灌一串肉眼可见的乱码。而 `resolve_target` 只保证「是你起的、pane 还活着」，
  **不**保证里头还有 agent 在跑——那一档是真实且常见的（工单 06 的 `wait` 返回
  `idle` 就是它）。
- **为什么两份都在控制面备好**：装配是纯字符串运算，摆在控制面才让主缝的假宿主
  把**真正写进 PTY 的那串字节**逐字断言（验收项第 4 条），不必起 gpui。桌面侧
  剩下的那一次挑选本身由 `mt-app` 的单测钉住。
- **口径与 `mt_ui::paste_to_bytes` 对账**：换行一律归一成 `\r`（PTY 那头把 `\n` 当
  「换行但不回车」，多行会出阶梯）、正文里的 `ESC[201~` 剔掉（否则 prompt 自己就能
  把粘贴块提前截断，后半截变成真键入）。两份实现隔着 crate 边界（`paste_to_bytes`
  吃 `TermMode`，而 `mt-ai` 不依赖 `mt-terminal`/`gpui`，搬不过去），
  **`mt-app` 是唯一同时看得见两侧的地方**，对账测试 `写穿装配与用户粘贴同口径`
  就住在那儿——唯一的差别是末尾那个回车（粘贴不按，写穿要按）。

**单行也包 bracketed paste**（工单把这条留给实施方定）。三条理由：① 一种形状一条
路，「只有多行才包」会长出第二条只在少数情形走到的分支；② 粘贴块里的正文对 TUI
而言是纯文本，单行里出现的 `\t`、`\x1b` 同样被当字面量而不是热键；③
`mt_ai::tracker` 认得这对标记（`tracker.rs:141`），整块 + 一个回车恰好被记成**一次**
提交，AI 会话身份与 marker 因此与用户亲手粘贴完全同形。

**回车在包裹之外，末尾的换行先删掉。** 包在里头只是往编辑框插一个换行，送不出去。
而 LLM 拿 heredoc / 三引号拼 prompt 时结尾几乎总带一个换行，原样留着就是替它多按
一次回车——在等确认的 TUI 里那一下会被当成「确认」。

**空正文被拒是一条裁决，不是入参校验。** 裸回车就是替用户按确认，而「attention
不代答」是 ADR 0003 的铁律；那是最顺手的代答姿势，在装配处就堵掉。给它专门的
`emptyInput` 码（而不是并进 `badRequest`），编排者才读得懂为什么被拒——错误消息与
CLI `--help` 都写清了是哪条规矩。

**回执刻意不复用 `PaneView`。** 写穿之后那一瞬的 `status` 一定还是写之前的样子
（agent 还没来得及反应），摆在回执里会诱导编排者把「刚发完还是 ai-idle」读成
「它干完了」。于是只回 `{paneId, bracketedPaste}`：后者**如实**说明是不是当成一整块
送进去的——为假时那段多行是逐行进去的，很可能已被中途的换行提前发出，这是编排者
需要知道的事实，不是可以粉饰的实现细节。要看状态走工单 06 的 `wait`。

**`send` 必须登记 `blocks_on_desktop()`。** 写 PTY 要回 gpui 主线程，就地阻塞会把
hook 上报那条单线程 HTTP 队一起卡住。它与 `start-session` 共用同一条泵、同一个
`ACTION_TIMEOUT`、同一种超时结论；两者只有一处不同——起会话超时后桌面上**可能真多了
一个会话**（所以有那套记账契约），写穿超时后什么实体都不会留下。泵开头那道时限闸
提到了分发之前，判据由 `OrchestratorSignal::deadline()` 的穷尽 `match` 兜住：加新
信号时编译不过，绕不过去。

**body 上限 64 KiB 判为够用，不动。** `send` 是唯一可能顶到它的命令。64 KiB 已是
一万多个英文词的一段指令，远超「派一次活」的合理体量；编排者要塞的大块上下文本来
就该以文件路径的形式交过去（乐手自己能读文件），不该逐字灌进 PTY。顶到上限时得到的是
明确的 `payloadTooLarge`，**不是截断**——两头都有测试（32 KiB 整段送达 / 超限一个
字节都不落到桌面端）。hook 端点那道闸一个字没动。

**新错误码**（闭集，CLI 按 code 分档；两条都落在退出码 4「改你的请求」）：

| code | status | 含义 |
|------|--------|------|
| `emptyInput` | 400 | 正文为空/全空白——裸回车即代答，ADR 0003 禁 |
| `sendFailed` | 500 | 找得到那个乐手，但正文没交到它的 PTY 手上 |

复用既有码：`paneNotFound`(404) / `paneGone`(410) / `selfTarget`(403) 全部经
`ControlPlane::resolve_target`（本票是它的**第一个真消费者**，`#[allow(dead_code)]`
与 `target_pane_id` 上那条一并摘掉）；`desktopBusy`(503) 语义与起会话一致。

**正文的保密面**：编排者写的 prompt 是用户项目里的内容，与启动器命令文本同一档待遇
——不进日志、不进错误消息、不进回执。`PaneInput` 因此**手写 `Debug` 只打长度**
（它会经 panic 消息落到 stderr），`CliError` 的三个变体一个都不装正文，
主缝有一条测试拿一段带路径的正文钉住回执不回显。

**CLI 形状**：`send --pane <ID> (--text <TEXT> | --stdin)`，两者由 clap 的
`ArgGroup` 保证**恰好给一个**。不做「没给就读 stdin」——这个二进制常在没有管道的
pane 里被跑到，那样会挂住等一个永远不来的 EOF，编排者看到的会是「命令卡死」。

## 留档（未整改）

- **本票顺手修了一条既有的随机红**：`坏令牌与伪造令牌一律被拒` 造「伪造令牌」时把
  末位固定换成 `'0'`，而令牌是十六进制串——本来就以 `'0'` 结尾时（1/16）那枚
  「伪造」令牌与真令牌一模一样，测试随机变红。本次跑到了，改成按末位在 `'0'/'1'`
  之间翻。与本票无关，但在同一个文件里，顺手带上。
- **`orchestrator_wire.rs` 里那条「未知命令」用的名字曾是 `send`**：本票把它换成
  `no-such-command`。工单 06/07 请注意别再拿将来要实现的命令名当占位。
- **写穿与「乐手此刻在不在 AI 会话里」解耦**：`resolve_target` 只判「是你起的 + pane
  还活着」，不判 `AiSessionState`。于是向一个 agent 已退出的乐手 `send`，会把 prompt
  写进那个裸 shell（`bracketedPaste: false` 是唯一的提示）。**刻意如此**：判了就得
  回答「Unknown 那一档算不算在跑」，而那一档在无 hook 的自定义启动器上恒为真
  （工单 03 留档），一判就把正常的 opencode/pi 乐手全挡在外面。编排者拿回执里那一位
  自己判断，比这边猜好。
- **不做「写穿去重 / 幂等」**：编排者重试一次 `send` 就是真的再写一次。控制面没有
  command_id 那样的幂等键（移动端指令有，因为它跨网络可能重投；CLI 是本机同步调用，
  拿到退出码就是确定的结论）。`desktopBusy` 那一档是唯一的模糊窗口——它与起会话
  同款，`--help` 里已经写清「先查一眼再决定」。
- **`AppStore::pane_bracketed_paste` 找不到终端时答 `false`**：包一层只在对面认得它
  时才有意义，认不出时少包一层最多是逐行送进去，多包一层是往用户屏幕上灌乱码。
  但这也意味着「PTY 已经没了」与「终端没开粘贴模式」在这个布尔上不可区分——
  真的没了会在紧接着的 `write_to_pane` 上得到 `sendFailed`，所以没有实际歧义。
- **正文里的 ESC 序列除结束标记外不过滤**（与 `paste_to_bytes` 同）。包裹版里它们
  是字面量，无害；裸版里会直接进应用，与用户粘贴同样的行为。编排者本来就能写任意
  文本，这不是新增的面。
- **CLI ↔ 真 hook server 的整条 HTTP 往返仍未真机走过**（与工单 02/03 同一条），
  主缝/辅缝都是进程内对账。留工单 09 验收。
- **三家 agent 的多行粘贴真机表现未验**（本票验收项第 5 条），留工单 09。Grok 有
  「出卡认首行」的既有已知差异，多行粘贴在它那儿最值得先看。
