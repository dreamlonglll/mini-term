# 14 — 汇报改为落文件 + `wait` 取件（推翻 12 的终端投递）

**Parent:** ADR 0004（2026-09-02 修订版）

**What to build:** 用户真机一看就否了「整段汇报写穿进编排者终端」：像用户在输入，且上下文线性膨胀。改成：桌面端生成汇报时把正文写成编排者项目目录下的一个文件，**一个字都不写进编排者终端**；`wait` 改为等待**汇报**——有新汇报就返回「哪个会话、什么事、文件在哪」并取走它们，正文由编排者用自己的 Read 工具读。投递泵、投递闸、写穿编排者那一整支删掉。

**Blocked by:** 12（账本与渲染保留复用）、13 文档部分（Skill/help 要再改一遍）

**Status:** done

- [x] 汇报文件：每条汇报渲染成 Markdown 写到 `<编排者所在项目>/.mini-term/reports/<编排者 pane 编号>/<NNNN>-<kind>.md`（NNNN 每编排者 4 位递增；kind 用 `turn-ended` / `awaiting-human` / `exited` / `closed` / `not-accepted`）。项目路径取自编排者的授予（`Grant.project_id` → 宿主项目表），取不到时退到应用数据目录 `orchestrator-reports/<pane>/`。写文件走 `mt_core::atomic_write`，目录不存在就建。单文件上限 256 KiB，超了截断并在文末注明
- [x] 文件内容：抬头改成键值行（`session`、`launcher`、`project`、`kind`、`cause`、`turn`、`tasks`、`at`，缺的不出现），空一行后是正文（transcript 增量 / 画面尾部 / 未被接收说明 / 等人处理说明），文案继续走 mt-i18n `orchestrator` 命名空间；删掉批头那两条（`batchHeader` / `batchDropped`）与其它不再用的键，字典重新生成、对账常量更新（875 → 862）
- [x] 账本：`ReportLedger` 改成两道队（待落盘 `staged` → 待取走 `ready`），后者每条带文件路径；`take_batch` 语义不变（取一次即收敛），另加按 `session_pane_id` 过滤的 `take_batch_from`；`INBOX_CAP` 溢出照旧丢最旧并计数（`dropped` 随 `wait` 回执带出）
- [x] 生成即落盘：`observe_status` / `observe_pane_closed` / `tick` 产生汇报时进 staged 队并戳线程；常驻线程（12 那条，只换末端）做「渲染 + 写盘 + 入队 + tick」。删掉 `can_deliver` / `deliver_pending` / 写穿编排者的 `send_input` 调用与相关测试；`PaneLiveness::awaiting_human` 保留给 `send` 的黄灯闸
- [x] `wait` 重做：请求 `{targetPaneId?: u32, timeoutMs?}`（`targetPaneId` 变可选）；阻塞到有匹配的未取走汇报即返回并取走，形状 `{"waited": {"outcome": "reports" | "pending", "reports": [{paneId, kind, cause?, taskIds, file, at}], "dropped": n, "status"?, "waitedMs": n}}`；`pending` 时若给了 `targetPaneId` 附带它此刻的 `status`；`--timeout 0` 是合法的「只看一眼」。`WAIT_MAX` / `WAIT_DEFAULT` / 读超时那套不变；旧的 `ai-idle` / `attention` / `idle` 三档终态连同 `PaneLiveness::settled` / `WaitState` 一起删掉
- [x] 清理：编排者 pane 关闭（`revoke_pane`）或重新授予（`grant`）时删掉它的汇报目录（后台删，失败只打日志）；`crates/mt-app/src/orchestrator_skill.rs` 的 `.gitignore` 条目加一条 `.mini-term/reports/`（同一套幂等追加）
- [x] sidecars/agent-control：`WaitOutcome` 改成新形状（`reports: Vec<ReportNote>`，`is_settled` = 拿到了汇报、`needs_human` = 任一条 `kind == "awaiting-human"`），`ControlRequest::wait` 的 `target_pane_id` 变可选；mt-agent-cli 的 `wait` 子命令 `--pane` 变可选；`--help` 的 `reports` 节与 wait notes 重写；`crates/mt-ai/tests/orchestrator_wire.rs` 的 wait 对账改新形状（并真读一次落盘的文件）
- [x] SKILL.md 再改一遍（唯一源头，`cli-location` 标记原样）：「Results are pushed to you」整节换成「Results land as files; `wait` tells you when」；样例改成 `wait` 的 JSON 回执 + 一个文件的内容；`orchestrator_skill.rs` 的渲染测试断言随之改
- [x] 主缝测试（控制面）：回合结束 → 文件存在且内容含 transcript 增量与任务编号 → `wait` 立刻返回并带上该文件路径 → 再 `wait --timeout 0` 是 `pending`（取一次即收敛）；`wait` 阻塞中来了汇报能被唤醒；按 `--pane` 过滤；黄灯汇报文件含画面原文；未被接收端到端；`dropped` 带出；编排者关闭后目录被删；渲染出的文件不含裸控制字节
- [x] `cargo test --workspace` + `cargo test --manifest-path sidecars/Cargo.toml --workspace` 全绿；工单 12 与 13 的 Status 与「留档」加一行指到本票

## 设计要点（给实施方）

- **12 的东西能留就留**：`reports.rs` 账本、`transcript_binding`、渲染与 `sanitize_report`、常驻线程、`observe_*` 接线都复用，只是「写穿编排者」换成「写文件 + 入队」。别推倒重来。
- **谁写文件**：mt-ai 的控制面（它已经在读 transcript 文件、已经知道项目路径）。mt-app 只多一条 `.gitignore` 条目。
- **`wait` 的唤醒**：账本入队后戳一下等待中的 `wait` 线程（条件变量或 250ms 轮询皆可，现有 `wait` 就是轮询 `pane_liveness`，照它改成轮询「有没有未取走的汇报」最省）。
- **文件路径回执用绝对路径**（Windows 反斜杠原样），编排者的 Read 工具直接吃。
- **`.mini-term/reports/` 这个目录名**与远程粘贴的 `.mini-term/pasted` 同一个根，是刻意的。
- 术语纪律照旧：用户可见面（`--help`、Skill、汇报文件正文）一律「orchestrated session / 受编排会话」。

## 纪律

- 禁跑 `cargo fmt`。Edit 工具可能把整份文件写成 CRLF，改完用 `git ls-files --eol` 核对。
- 不做任何 git 提交 / stash / checkout——由编排会话统一提交。不起 GPUI 实例。
- 跑 `cargo test -p mt-app --bin mini-term` 前确认没有 dev 实例占着本 worktree 的 `target/debug/mini-term.exe`（装机版不占，别杀它）。

## 设计决议（实施方填）

落点：`crates/mt-ai/src/reports.rs`（账本两道队）、`crates/mt-ai/src/control.rs`（本票绝大部分）、
`crates/mt-app/src/ai.rs`（一行 `set_data_dir`）、`crates/mt-app/src/orchestrator_skill.rs`
（`.gitignore` 多一条）、`crates/mt-i18n/locales/orchestrator.ts` + 重新生成的 `dict.rs`
+ `tests/consistency.rs` 的对账常量（875 → 862）、`sidecars/agent-control/src/lib.rs`、
`sidecars/src/bin/mt-agent-cli.rs`、`.claude/skills/mini-term-orchestrator/SKILL.md`、
`CLAUDE.md` 与本目录 `README.md`、仓根 `.gitignore`。

### 账本改成两道队，而不是给 `Report` 加一个 `file` 字段

汇报的**生产**在桌面主线程上（`observe_*`），而渲染要读会话记录文件、写盘要碰磁盘
——两件事不能在同一条线程上做完。于是每个编排者名下两道队：`staged`（已产出、
还没落成文件）与 `ready`（已落盘、等 `wait` 取走），中间隔着那条常驻线程
（`take_staged` → 渲染 → 写文件 → `deposit`）。

给 `Report` 挂一个 `file: Option<String>` 也能表达，但那样「有没有落盘」就变成一个
**要靠调用方自觉维护的状态位**：忘了填的那一条会以 `file: null` 出现在 `wait` 的
回执里，编排者拿着一个空路径去 Read。两道队让它在类型上说不出来——`ReadyReport`
的 `file` 是 `String`，造不出一个没有文件的 ready 汇报。

`dropped` 是**两道队共用的一个计数**：编排者关心的只有「我永远看不到几条」，
它是攒在哪一段丢的无关紧要。写文件失败那一档也计进去（`note_dropped`）——
静默丢一条汇报，编排者就永远不知道自己看到的不是全部。

### 抬头是稳定的 ASCII 键值行，只有正文走 i18n

12 那一版抬头是「`受编排会话 #12 · 启动器 Claude · 回合结束 · 成因 Stop`」，
六个字段名与五种汇报各自那句话全在字典里。落成文件之后这套不成立了：编排者要
**按 kind 分支**（黄灯归人、回合结束才验收），而它读到的语言由用户的桌面设置定
——中文界面下它得去匹配「停下等人处理」，英文界面下匹配 "stopped, waiting for a
human"，Skill 里那份样例只能对上一种。于是抬头改成：

```text
session: 101
launcher: Claude
project: 项目p-self
kind: turn-ended
cause: Stop
turn: 0s
tasks: t1
at: 2026-09-02T15:08:20+08:00
```

键名与 `kind` 的五个取值都是 ASCII 常量（`ReportKind::slug()`，同时是文件名的
后半段与 `wait` 回执里的 `kind` —— 一处真源），**缺的字段整行不出现**。
正文照旧走 mt-i18n（那是给它读的话，不是给它 parse 的）：

```text
（承上，空一行之后）
会话记录增量（第 0 条起，共 2 条）：
[user] 把测试跑一遍
[assistant] 跑完了，3 个失败
```

黄灯那一档（`awaitingHumanNote` + 画面尾部原文）：

```text
session: 101
launcher: Claude
project: 项目p-self
kind: awaiting-human
cause: PermissionRequest
at: 2026-09-02T15:08:47+08:00

你不能替人回答。把下面的画面原文转述给用户，请人到那个终端处理完，再继续派活。
终端画面尾部原文：
> Allow running rm -rf /tmp/x ?
  1. Yes  2. No
```

（两段都是主缝测试 `回合结束的汇报落成文件并被_wait_取走` /
`黄灯的汇报文件带画面原文` 真渲染出来的原文，`项目p-self` 是测试夹具里的项目名。）

字典因此净减 13 条（875 → 862）：批头 2 条、抬头字段 6 条、五种汇报各自那句话 5 条。
`awaitingHumanNote` / `notAcceptedNote` / 正文那 5 条一条没动。

### `wait` 的回执长什么样

同一次取件的 JSON（主缝测试真产出，路径是测试的临时目录；字段顺序按结构体声明，
`cause` / `status` 无值时整个字段不出线）：

```json
{
  "waited": {
    "outcome": "reports",
    "reports": [
      {
        "paneId": 101,
        "kind": "turn-ended",
        "cause": "Stop",
        "taskIds": ["t1"],
        "file": "C:\\...\\.mini-term\\reports\\7\\0001-turn-ended.md",
        "at": "2026-09-02T15:08:20+08:00"
      }
    ],
    "dropped": 0,
    "waitedMs": 0
  }
}
```

`pending` 时 `reports` 是**空数组**而不是缺字段——编排者只需要认一种形状。

### `--pane` 可选，但「已经关了」那一档要先把汇报交出来

不点名 = 名下任一受编排会话的下一条汇报（派完几个活等谁先回来，是最常见的姿势）；
点名就只取那一个的，别人的留在队里。点名那一档照走 `resolve_target`（自指 /
不是你起的 / 已经关了）——**只有 `paneGone` 破例**：那种 pane 多半正躺着一条
`closed` 汇报没被取走，而那恰恰是编排者最需要的一条。于是先试一次取件，取不到
才答 `paneGone`（测试 `wait_点名已关的会话先把汇报交出来`）。

`pending` 且点了名时回执带上那个 pane 此刻的 `status`：`ai-working` = 真在跑，
`idle` = 这个会话我们看不透（没 hook、输入检测也没认出来的自定义启动器，它永远
不会有 `turn-ended` 汇报）。不点名时整个字段不出线——几个会话状态各不相同，
报哪一个都不对。

### 三档终态删干净，不留兼容

`PaneLiveness::settled` / `WaitState` / `wait_outcome` 的旧形状全删。这个功能没发过版，
留一个「两种 outcome 词汇都认」的兼容层只会让编排者在 `--help` 里读到两套说法。
`PaneLiveness::awaiting_human` 留着——它还有两个消费者（`send` 的黄灯闸、账本的
attention 判定）。

### 序号机在控制面，不在账本

`NNNN` 每编排者从 1 起单调递增，住在 `Inner::report_seq`（一把叶子锁）。
**不回头扫目录**：授予令牌那一刻汇报目录被整个删掉（`grant`），于是编号与目录里
的文件天然对得上。撤令牌时序号一并清掉。

账本不认识文件、不认识路径、不认识挂钟——`ReadyReport::at` 那个 RFC3339 字符串
是渲染侧现取的（`local_timestamp()`），相差一拍以内。要让它精确到事件时刻就得给
账本的每个入口都加一个 `DateTime` 参数，那是把一个纯状态机的接口翻倍去换一个
秒级精度，不值。

### 汇报目录的清扫必须**同步**判存在，异步删

`revoke_pane` / `grant` 里那次 `purge_dirs`：先在调用线程上 `dir.exists()` 过一遍，
只有真存在的才丢线程去 `remove_dir_all`。这不是省一次线程的优化，是一道**时序
护栏**——授予令牌那一刻的清扫要是无条件拖着一条线程出去，它完全可能跑到第一条
汇报落盘**之后**才动手，把刚写好的文件连目录一起端掉。实施中被它咬过：测试随机
红，每次换一个测试，报 `os error 3`（写文件时目录已不在）。

### 落盘线程只换了末端，别的一个字没动

`start_report_pump` 那条 while 循环、`sync_channel(1)` 的唤醒口、`Weak<Inner>`、
`cfg!(test)` 下不起线程——全是 12 的原样。变的只有它最后调的那个方法：
`deliver_pending`（查闸 → 取批 → 渲染 → 写 PTY）换成 `materialize_pending`
（取 staged → 逐条渲染 → 写文件 → 入队）。`observe_status` 里「编排者自己刚闲下来
就戳泵」那条分支删了——没有闸了，落盘不看收件人的状态。

游标的口径也简化了：12 要在「整批渲染完、写穿成功」之后才逐条回写，于是有一个
局部 `cursors` map；现在一条汇报一个文件，写成功就地 `set_reported_cursor`，
写失败就不动（那一段增量留给下一条）。测试 `汇报编号四位递增且增量不重复` 钉住
「同一段增量不许贴两遍」。

### `--help` 与 SKILL.md 的五个小标题改成 slug

13 那一版用的是渲染出来的说法（`turn ended` / `waiting for a human` / …），
因为那时汇报正文里就是那么写的。现在文件里写的是 `kind: turn-ended`，帮助里
也就得是 `turn-ended` —— 让编排者能逐字对上。CLI 那两条帮助测试随之改名重写
（`帮助文案讲清楚了汇报落文件由_wait_取件` / `帮助文案讲清楚了取件的几条边界`）。

## 留档（实施方填）

1. **`Closed` 那条汇报多半带不到会话记录增量**（12 留档第 1 条原样成立）。桌面侧关
   pane 的顺序是 `dispose_terminal` → `observe_pane_closed`（此刻 hook 身份还在）→
   `shutdown` → `AiPerception::pane_closed`（**先 purge hook 再撤令牌**），而落盘是
   异步的：线程真去渲染时 `host_pane_session` 已经答不出 session_id 了，那条汇报
   只剩抬头 + 任务编号清单。判为不值得修：那一回合的增量在它结束时已经作为
   `turn-ended` 落过一次盘，而「已关闭」这件事本身编排者也做不了什么。
2. **写汇报文件会顺带把项目目录建出来**。`write_report_file` 走 `create_dir_all`，
   于是项目已经被用户从磁盘上删掉、但还留在项目表里时，第一条汇报会把
   `<项目路径>/.mini-term/reports/<pane>/` 整条链建回来（一个空壳目录树）。
   代价很小、路径来自用户自己的项目表，没有做「父目录不存在就退到数据目录」的
   判断——那要多一次 stat 且语义更绕（一半汇报在项目里、一半在数据目录）。
3. **单文件 256 KiB 的上限之外，目录本身没有上限**。一个编排者跑一下午能攒出几百
   个文件；它们随 pane 关闭一起删，中途不做修剪。真机上若发现某个项目里堆了几千
   个汇报，该加的是「取走即删」而不是数量上限——但那会让编排者失去「回头再读一遍」
   的能力，所以没有先做。
4. **`dropped` 只在有汇报可交时才随批次出去**。`take_batch` 在 ready 队为空时答
   `None`（哪怕 `dropped > 0`），于是「最后几条全丢了、之后再没有新汇报」这种病态
   序列里那个计数交不出去。反过来做的话 `wait` 会返回一个 `outcome: "reports"` +
   空数组的回执，编排者更难读。判为可接受。
5. **落盘线程那条 while 循环仍然没有自动化覆盖**（`cfg!(test)` 下不起，12 留档第 3
   条原样成立）。被测的是它调的两个同步方法；循环本身只做「睡 1s / 被戳醒 /
   upgrade Weak / 调一次」四件事。
6. **汇报文件的时刻是落盘时刻，不是事件时刻**（见上「序号机在控制面」）。正常路径
   上相差一拍（`PUMP_INTERVAL` = 1s）以内；线程被磁盘拖住时会更大，而 `turn` 那个
   耗时字段用的是账本里的 `Instant`，不受影响。
7. **本仓自己的 `.gitignore` 手写了一条 `.mini-term/reports/`**。投放那条链路只在
   「两份 SKILL.md 都是自己写的」时才碰 `.gitignore`，而本仓那份是被 git 跟踪的源
   文件、投放整体跳过（6138f02 修的就是这个），于是在 mini-term 仓里开编排者时那条
   自动追加不会发生。别的项目照旧自动。
8. **撤令牌与落盘线程之间有一个窄口子**：线程已经 `take_staged` 出一批、正在渲染时
   编排者的 pane 关掉了，`revoke_pane` 先清两道队再删目录，而那条线程随后仍会把手上
   那几条写进去（`write_report_file` 会把目录建回来）——留下一个没有主人的
   `.mini-term/reports/<pane>/`，要等这个 pane 编号再次被授予时才会被清掉。汇报本身
   不会错发（收件箱已经没了，`wait` 也再没人调得动）。修法是给落盘线程加一个「写之前
   再确认一次收件人还在」的复查，代价是每条汇报多一次锁 + 一次记账查；判为不值——
   PTY 编号单调递增，同一个编号再被授予是很久以后的事，而那时会先删一次。
9. **没做真机验收**。工单 13 的验收清单（八个子项）连同 ADR 0004「后果」点名要求的
   黄灯全链路仍然空着，且现在要按新形态重走：起编排者 → 派活 → `wait --timeout 300`
   → 按回执里的 `file` 用 Read 读 → 处置。真机上最该先看的四条：① `wait` 的实际
   往返延迟与编排者会不会因为 `pending` 就放弃；② 汇报目录在真实项目里被 git 看见
   没有（`.gitignore` 那条追加有没有生效）；③ 编排者 pane 关掉之后目录真的被删；
   ④ Claude 的 JSONL 相对 `Stop` hook 的落盘先后（12 留档第 2 条要求的那次观察，
   「增量为空补画面尾部」那一档是否真的被触发）。
