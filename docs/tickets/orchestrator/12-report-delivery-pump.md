# 12 — 汇报投递泵：投递闸 + 渲染 + 写穿编排者 + 桌面接线

**Parent:** ADR 0004（受编排会话的汇报推送）

**What to build:** 把 11 的账本装进 `ControlPlane`，加一条后台投递线程：编排者可投递（`ai-idle` 且非 attention 且 pane 活着）时把它收件箱里的汇报取空、渲染成一段文字、经既有的 `OrchestratorActions::send_input` 写进编排者的终端。桌面侧只做两处接线：状态变化与 pane 关闭喂给控制面。做完这张票，编排者派活之后**什么都不用做**，结果会自己回到它的对话里。

**Blocked by:** 10（任务账本）、11（汇报账本）

**Status:** done → **投递方式被工单 14 推翻**（2026-09-02 用户真机否了「写穿编排者终端」：像用户在输入且上下文膨胀）。账本、渲染、接线保留，投递泵/投递闸/写穿编排者删除，改为落文件 + `wait` 取件，见 14。

- [x] `ControlPlane` 新增 `pub fn observe_status(pty_id, status, cause)` 与 `pub fn observe_pane_closed(pty_id)`：按记账表把「这个 pty 是谁的乐手」翻出来喂给 11 的账本；pty 是编排者自己时只用来**唤醒投递**（它可能刚变空闲）；两者都不是就忽略。记账增删（`register_landed` / `revoke_pane` / 离场）同步到账本的 `register_hand` / `forget_hand`；编排者撤销令牌时 `drop_inbox`
- [x] 投递闸 `can_deliver(&PaneLiveness) -> bool`（纯函数、有单测）：`alive && status == "ai-idle" && !is_attention_cause(cause)`。`ai-working` 暂存、黄灯暂存、`idle`（裸 shell）暂存、pane 没了丢弃
- [x] 投递线程：`ControlPlane` 内一条常驻 `std::thread`（首次 `set_actions` 时起，与 hook 服务同寿），吃一条 `mpsc` 唤醒通道 + 每 1s 一次的自唤醒（跑 `tick` 与重试被暂存的收件箱）。唤醒时对每个有积压的编排者：查闸 → `take_batch` → 渲染 → `send_input(orchestrator, PaneInput::assemble(text))`（**不追加**格式尾部）。`send_input` 答 `PaneGone` 就丢收件箱；答 `DesktopBusy` / `WriteFailed` 把这批放回收件箱头部下一拍再试
- [x] 渲染：每条汇报一段，整批一个头（有 `dropped` 时注明丢了几条）。`TurnEnded` / `Exited` / `Closed` 带 transcript 增量——从 `reported_cursor` 起到末尾，走 `TranscriptSource::read`（session id 只认 hook，与 `read_transcript` 同一套裁决），角色标 `[user]` / `[assistant]`，字节上界复用 `read_transcript` 的那一档并标 `truncated`，渲染完 `set_reported_cursor`；`transcriptUnsupported` / `sessionUnidentified` 那两档改用 `read_screen` 的尾部若干行。`AwaitingHuman` 带画面尾部原文（编排者要转告用户）。`NotAccepted` 只带任务编号与「写入后 15 秒未见开始处理，请 read-screen 核对后重发」
- [x] 文案全部走 mt-i18n `orchestrator` 命名空间（10 已建），中英各一份；每段以固定前缀 `[mini-term]` 开头好让编排者认出来；**用户可见面不许出现「乐手」/「musician」**
- [x] 桌面接线：`crates/mt-app/src/store/ai.rs` 的 `AiEvent::Status` 处理里调 `observe_status`；`dispose_terminal` 里在撤销令牌之前调 `observe_pane_closed`。就这两处
- [x] 主缝测试（`control.rs` 的 tests，用 `FakeActions` 把写进编排者 pane 的字节抄下来、`pane_liveness` 可控、`FakeTranscripts` 可控）：乐手 Stop → 编排者空闲 → 文字写进编排者 pane 且含 transcript 增量与任务编号；编排者 `ai-working` 时暂存、转 `ai-idle` 后一次投递且多条合并；编排者黄灯暂存；编排者 `idle` 暂存；编排者 pane 没了收件箱丢弃；乐手黄灯 → 汇报含画面原文；无记录 agent → 画面尾部；`NotAccepted` 端到端；`DesktopBusy` 重试不丢；投递用的 `PaneInput` 不带格式尾部
- [x] 写进编排者的那段文字**不能被 `mt-ai::detect` 误认成 AI 命令、不能被当成打断键**（整块 bracketed paste，tracker 记成一次提交；写一条测试钉住渲染结果不含裸 `\x1b` / `\x03` 单字节）
- [x] `cargo test --workspace` 全绿（跑 mt-app 单测前关掉 dev 实例）

## 设计要点（给实施方）

- **一切复用**：写编排者走 `send_input`（与 `send` 同一条泵、同一个 `ACTION_TIMEOUT`），读 transcript 走 `transcripts()`，读画面走 `read_screen`，闸的输入走 `pane_liveness`。四条缝都已注入，这一票**不新增 trait 方法**。
- **投递线程一把锁都不许持着做 I/O**：与 `wait` 同款——取事实、放锁、再读文件 / 跳主线程。
- **闸查完到写入之间有窗口**（用户恰好开始打字）：接受，ADR 0004 的「后果」写了。但**不许**在 `ai-working` 时写——那是靠 agent 排队而不是靠闸。
- **编排者的 `MINITERM_*` 身份是 pane**：它里头的 agent 退出再起（同一 pane）令牌仍在，收件箱照留；`can_deliver` 会在它再次 `ai-idle` 时放行。
- 汇报头部信息：乐手 pane 编号、启动器展示名、项目名（记账表里都有）、成因原文、回合耗时（有的话）、任务编号清单。**不回显 prompt 正文**。
- 渲染出的文字体量要有上界（整批不超过 `read_transcript` 单次回执的上限量级）；超了截断并说明——编排者随时能 `read-transcript` 补读。
- 主缝测试里的投递线程节拍要可控：把「1s 自唤醒」做成常量，测试里用显式唤醒（例如 `observe_status` 触发）而不是睡等。

## 纪律

- 禁跑 `cargo fmt`。Edit 工具可能把整份文件写成 CRLF，改完用 `git ls-files --eol` 核对。
- 不做任何 git 提交 / stash / checkout——由编排会话统一提交。
- 跑 `cargo test -p mt-app --bin mini-term` 前确认没有 dev 实例占着 `target/debug/mini-term.exe`（装机版不占，别杀它）。

## 设计决议（实施方填）

落点：`crates/mt-ai/src/control.rs`（本票的绝大部分）、`crates/mt-ai/src/reports.rs`
（删掉模块头那行 `#![allow(dead_code)]` 与它的注释）、`crates/mt-app/src/store/ai.rs`
（两处接线）、`crates/mt-i18n/locales/orchestrator.ts` + 重新生成的 `src/dict.rs`
+ `tests/consistency.rs` 的对账常量（855 → 875）。

### 「尚未汇报的任务编号」收成一份真源，工单 10 那份删掉

工单 10 在 `Task` 上做了一个 `reported` 位，外加 `unreported_task_ids` /
`take_unreported_task_ids` 两个查询——它与 11 的账本自己那份「尚未汇报」清单是
**同一件事的两份事实**（10 的留档也预见了这一点）。本票按编排会话的裁决删掉了
10 那份（连同两条测试），`Task` 与 `tasks_of` 退回纯事实查询：派了几条、派给谁、
写入那一刻对面什么状态。

双写收在**一处**：`ControlPlane::note_task_written` 先在 `registry` 记一条事实、
出锁后把 `(target, task_id, target_status, now)` 喂给账本。`send` 那一行一个字没改
——它调的还是同一个方法，「写穿成功之后才登记」那条口径原样保留。

### 「跑一遍投递」是同步方法，线程只是节拍器

`ControlPlane::deliver_pending()`（扫 `pending_orchestrators` → 查闸 → `take_batch`
→ 渲染 → `send_input`）与 `ControlPlane::pump_reports(now)`（= `ledger.tick(now)`
+ `deliver_pending()`）都是 `pub` 的同步方法，后台线程只是每 `PUMP_INTERVAL`（1s）
调一次 `pump_reports`。**主缝测试全部直接调这两个方法，一次 sleep 都没有**，
15 秒的未接收超时靠给 `pump_reports` 传一个未来的 `Instant` 拨过去。

线程本身：`sync_channel(1)` 的唤醒口住在 `Inner` 里（多次唤醒自动并成一次），
线程只拿 `Weak<Inner>`——控制面一落地它下一拍就自己走掉，测试里不留常驻线程。
唤醒点两处：`observe_status` 产生了汇报，或者**编排者自己**的状态变了且它收件箱
有积压（后者就是「它刚闲下来」那一档）。

**`cfg!(test)` 时不起这条线程**：主缝测试按拍显式调 `pump_reports`，后台再有一个
投递方就是两个人抢同一个收件箱——谁先 `take_batch` 谁就把对方的断言变成掷骰子
（实施中真撞上了一次，`编排者忙时暂存闲下来一次投` 稳定红）。被测的是
`pump_reports` / `deliver_pending` 本身，不是那条八行的 while 循环。

### 游标先在本批的局部账上推进，写穿成功才回写

`render_batch` 返回 `RenderedBatch { text, cursors }`，`cursors` 是**渲染期间**用掉的
游标，只有 `send_input` 答 `Ok` 才逐条 `set_reported_cursor`。两个理由缺一不可：

- **失败要能重来**：`DesktopBusy` 那一批放回队首，下一拍得把同一段增量重渲一遍；
  提前落游标就等于「没送到但算汇报过了」。
- **同一批里同一个 pane 可能有两条终结性汇报**（编排者忙了两个回合），第二条得从
  第一条的末尾接着数。所以渲染期间的游标走一份**局部 map**，从账本读一次做初值、
  逐条推进——光读账本的话同一段增量会被贴两遍。有测试钉住（`编排者忙时暂存闲下来一次投`
  里那句 `第一轮完` 只许出现一次）。

### 家族裁决 / 绑定 / 项目路径抽成 `transcript_binding`，两处共用

`read-transcript` 与汇报渲染现在调同一个 `ControlPlane::transcript_binding()`，
它答三档 `TranscriptBinding::{Ready, Unsupported, Unidentified}`——命令那一侧把后两档
翻成错误码，渲染那一侧把后两档换成画面尾部。摊开写两遍的走散方式很具体：
忘了「session_id 只认 hook、绝不启发式」。有一条测试钉住没有会话身份时
**一次记录都不去读**（`没有会话身份的汇报换画面尾部`）。

### 渲染：抬头按字段拼，缺的不出现

抬头是几个字段用 ` · ` 串起来：`受编排会话 #{pane}` · `启动器 X` · `项目 Y` ·
那句话（回合结束 / 停下等人处理 / 已退出 / 已关闭 / 任务 tN 可能没被接收）·
`成因 Stop` · `本回合用时 2m13s` · `涉及任务 t3, t4`。**没有的字段整段不出现**，
于是不必为「无成因的降级路径」再造一套「回合结束（无成因）」文案。

- **前缀 `[mini-term]` 不进字典**：它是标识不是文案，翻译它只会让编排者在两种语言里
  认两套标记。它同时是一道安全护栏——整段以它打头，
  `detect::interactive_ai_command_name`（只看行首第一个词）就绝不会把正文里出现的
  `claude` / `codex` 当成「用户敲了一条 AI 命令」。
- **回合耗时不进字典**：`2m13s` 两种语言都读得懂，为它造「分/秒」两套文案只是多两个键。
- **`Closed` 那一档不读画面**：pane 都没了，`read_screen` 注定答 `PaneGone`，
  不做这次白跑的主线程往返（有测试钉住 `screen_calls` 里没有它）。
- **整批共用一份 `MAX_READ_BYTES`（32 KiB）预算**：超了就地截断 + 一句
  `bodyTruncated`。与 `read-transcript` 的取舍不同——那条有游标、少给一条下次取得回来；
  这条是推过去的，宁可给半段也别整段不给。

### `sanitize_report`：先剥转义序列，再筛裸控制字节

会话记录与终端画面里什么都可能有。**只剥序列**的话 `\x03` 这种单字节留得下来，
**只筛字符**的话 `ESC[31m` 会留下一串 `[31m` 的字面噪音。于是两步都做：
`detect::strip_ansi_codes` + 逐字符筛掉 C0 与 DEL（`\t` 压成空格，抬头那一行连换行
也压成空格）。这不是美化——裸 `ESC[201~` 能把 bracketed paste 提前截断（后半截变成
真键入），裸 `ESC` / `\x03` 是打断键。有一条测试把最刁的载荷（颜色序列 + `\x03` +
一个粘贴结束标记 + 一句 `claude --resume`）灌进会话记录与画面，断言写出去的字节里
一个裸控制字符都没有、`interactive_ai_command_name` 认不出、`is_interrupt_key` 为假。

### 文案：`orchestrator` ns 加 20 条（855 → 875）

批头 2 条、抬头字段 6 条、五种汇报各自那句话与两条提示 7 条、正文 5 条。术语纪律
（不许出现「乐手 / musician」）与「不许自带 ESC / 换行」由 `汇报文案双语齐备且不带别名`
**遍历整个 ns** 兜住，不是逐条登记——这一批文案的调用点全在 mt-ai，
`mt-app::i18n::USED_KEYS`（那是 mt-app 自己用到的 key 清单）不必登记。

### 渲染出来的样子（中文，工单 13 改 Skill 时引用）

下面这段是主缝测试 `回合结束的汇报写进编排者的终端` 真实渲染出来的文字（装配前，
`\r` 已还原成换行；`项目p-self` 是测试夹具里的项目名）：

```text
[mini-term] 以下是 mini-term 桌面端自动送达的受编排会话汇报，共 1 条。用户没有说话——是你派出去的受编排会话有了新进展。请据此决定下一步；需要人处理的事直接告诉用户。

[mini-term] 受编排会话 #101 · 启动器 Claude · 项目 项目p-self · 回合结束 · 成因 Stop · 本回合用时 0s · 涉及任务 t1
会话记录增量（第 0 条起，共 2 条）：
[user] 把测试跑一遍
[assistant] 跑完了，3 个失败
```

黄灯那一档长这样（`awaitingHumanNote` + 画面尾部原文）：

```text
[mini-term] 受编排会话 #101 · 启动器 Claude · 项目 项目p-self · 停下等人处理 · 成因 PermissionRequest
你不能替人回答。把下面的画面原文转述给用户，请人到那个终端处理完，再继续派活。
终端画面尾部原文：
> Allow running rm -rf /tmp/x ?
  1. Yes  2. No
```

整段以 `[mini-term]` 打头、每条汇报也各自以它打头——Skill 里可以直接教编排者
「看到 `[mini-term]` 开头的消息就是桌面端送来的结果，不是用户在说话」。

## 留档（实施方填）

1. **`Closed` 那条汇报多半带不到会话记录增量**。桌面侧关 pane 的顺序是
   `dispose_terminal` → `observe_pane_closed`（此刻 hook 身份还在）→ `shutdown`
   → `AiBridge::remove_pane` → `AiPerception::pane_closed`（**先 purge hook 再撤令牌**），
   而投递是异步的：泵真去渲染时 `host_pane_session` 已经答不出 session_id 了，
   于是那条汇报只剩抬头 + 任务编号清单。修法是让 `observe_pane_closed` 当场把会话身份
   快照下来，代价是账本或控制面里多一张必须同步清理的表——判为不值：那一回合的增量
   在它结束时就已经作为 `TurnEnded` 投过一次了，而「已关闭」这件事本身编排者也做不了
   什么。
2. ~~**transcript 增量为空时只给一句「还没有新的会话记录」，不退回画面尾部**~~ →
   **已补**（编排会话验收时顺手改的）：增量为空时那句照说，再补一份画面尾部，游标不倒退，
   下一条汇报从同一处接着数；测试 `增量为空的汇报补画面尾部` 钉住「后落盘的记录一条不丢」。
   工单 13 真机验收仍要看一眼 Claude 的 JSONL 相对 `Stop` hook 的落盘先后。
3. **泵那条 while 循环没有自动化覆盖**（`cfg!(test)` 下不起）。被测的是它调的两个同步
   方法；循环本身只做「睡 1s / 被戳醒 / upgrade Weak / 调一次」四件事。真机行为留给
   工单 13 的端到端验收。
4. **闸查完到写入之间那个窗口没做处置**：用户恰好在编排者 pane 上打字的一瞬撞上投递，
   粘贴会插进去。ADR 0004 的「后果」明写接受，本票没有加「空闲满 N 秒才投」的设置。
5. **`deliver_pending` 每拍对每个有积压的编排者现查一次 `pane_liveness`**，没有缓存。
   编排者数量是个位数、那个方法的契约本来就是「很快、不跳主线程」，判为不必优化。
6. **汇报会在编排者的 pane 上留下输入痕迹**：它整条走 `AppStore::write_to_pane`
   （与移动端指令同一条），于是输入跟踪会把这一批记成一次用户提交、AI marker 列表里
   会多一条。这与 ADR 0004 的「等价于用户在编排者的 pane 里对它说了一句话」一致，
   没有做特判；真机上若发现 marker 列表被汇报刷屏，再谈要不要给写穿加一个「不记标记」
   的旁路。
7. **`dropped`（收件箱溢出丢了几条）会进批头，但只有 `reports.rs` 的单测覆盖**，
   没有端到端的主缝测试——凑满 50 条汇报要 25 个来回，性价比太低。批头那一行的渲染
   与 `batchDropped` 文案本身由 `汇报文案双语齐备且不带别名` 与 `render_batch` 的
   代码路径保证。
8. **没做真机验收**：起 GPUI dev 实例会占本 worktree 的 `target/debug` exe，而工单 13
   本来就要走一遍端到端。真机上最该先看的三条：黄灯全链路（受编排会话挂黄灯 → 汇报 →
   人处理 → 恢复后终态汇报，ADR 0004 的「后果」第三条点名要求）、汇报进入编排者对话
   之后它会不会把 `[mini-term]` 当成用户发言去回复、以及一次派活到收到汇报的实际延迟。
