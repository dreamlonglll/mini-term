# 07 — 读结果：read-transcript + read-screen

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** 读结果两层（ADR 0003 的能力分层）。`read-transcript`：按增量读乐手的结构化会话记录，仅 Claude / Codex / Grok 可用；绑定以 hook 上报的会话身份为权威，opencode / pi 等无会话记录的 agent 明确报错，**禁止启发式绑定**（与对话镜像同一条铁律）；增量口径与镜像 seq 一致。`read-screen`：对所有乐手可用，进程内直读该 pane 终端画面（VT grid）尾部 N 行纯文本——无记录 agent 的兜底，也用于看清审批提示原文。

**Blocked by:** 03（先有乐手可读）

**Status:** done（待填）

- [x] 三大家乐手的回答能以结构化增量读出（新增量只含上次之后的内容）
- [x] 无记录 agent 调 read-transcript 得到明确错误；read-screen 对其可用
- [x] 向非自启 pane 读取被拒（「不存在」语义）
- [x] 主缝测试覆盖能力分层、增量语义与范围裁决

## 设计决议

**增量游标就是 seq，而且服务端一个字节都不存。** `seq` 是消息在这条会话记录里的
序号（0 起、连续、只增），与 `mt_relay::mirror::MirrorParser` 给镜像消息编的那个
seq 是同一套语义 —— 工单明说不许发明第二种游标。编排者把上次回执里的
`nextCursor` 原样传回来，服务端只回它之后的部分。

- **为什么不存游标**：seq 是「从头解析这条记录」的确定性产物（记录文件只追加不
  改写），每次现算即可。存的话就得为每个编排者 × 每个乐手各维护一份解析器状态，
  那是一堆会走散的东西；镜像那边存是因为它是个长跑的轮询任务，本来就有运行态。
- **代价**：每次请求把整条记录读一遍解析一遍。跑了几小时的会话那个 JSONL 可以是
  几十 MB —— 于是这条命令登记进了下面那张「另起线程」表。
- **游标只在一个 `sessionId` 内有意义**，回执因此带着 `sessionId`：乐手 `/clear`
  或退出重开之后是另一条会话、seq 从 0 重新数。编排者按这个字段认换绑，
  `--help` 里写着「它一变就把游标归零」。越界的游标一律**钳回总数**、不报错。
- **与镜像 seq 的一个诚实差别**：镜像除文字消息外还会为 Claude 的提问卡片、
  已作答标记各占一个 seq（那是移动端点选作答要的），本命令只出文字消息。
  于是同一条会话在两侧的**编号可能对不上**——两者是同一套*语义*（0 起、连续、
  只增、游标 = 下一条的序号），不是同一串*数字*。编排者从不与移动端交换游标，
  这不构成问题；真要合并两侧时得重新对齐。

**绑定只认 hook 上报的会话身份，一次都不猜。** 与对话镜像同一条铁律（CLAUDE.md
与 `mt_relay::mirror` 的模块注释各写了一遍）。没有身份就答 `sessionUnidentified`，
**绝不**回退到「这个项目里最新的那个会话文件」——那条退路会把同项目里另一个
agent 的对话贴到这个 pane 上。主缝有一条测试专门钉它：项目里明明躺着一条别人的
记录，被拒的那次请求**一次都没去翻过磁盘**（`FakeTranscripts::calls` 必须为空）。

**能力分层落在两个码上，且各自写清下一步。** 编排者是 LLM，错误消息就是它的下一
步指令：

| code | status | 含义与下一步 |
|------|--------|------|
| `transcriptUnsupported` | 409 | 这个 agent 没有可解析的会话记录（opencode/pi）。**永远不会好转** → 换 `read-screen` |
| `sessionUnidentified` | 409 | hook 还没上报会话身份，无从绑定。**可能待会就有** → 等，或先 `read-screen` |

分成两个码而不是一个，是因为编排者该做的事不同。两条消息里都点名了 `read-screen`，
辅缝有断言钉住。判据顺序也有讲究：**先看 agent 家族、再看有没有身份** ——
opencode 的 pane 上两样都不满足，先答「这家没有记录」比答「还没上报」有用得多。

**`agent` 名的收敛与 hook/检测的分工。** `OrchestratorHost::pane_session` 新增一个
方法，照实报两样事实：`session_id` **只**来自 hook（`HookState::session_of`），
`agent` hook 优先、退回输入检测（`SessionTracker::ai_session_agent`）。退这一步是
必须的——opencode / pi 压根不发 hook，没有它就答不出上面那句「换 read-screen」。
收敛成家族名（`claude`/`codex`/`grok`）的 `transcript_family` 与
`mirror::resolve_session_file_by_id` **一字不差**（`contains` 而非全等；codex/grok
先判；其余落 claude），一条测试拿 `agent_has_session_log` 与它双向对账。
agent 名缺席时按 claude 兜底也是照抄镜像那条 `else` —— 那不是启发式：我们仍然只
认那个确切的 session_id，猜错家族只会找不到文件、答一份空记录，**绝不会绑到别人
的会话上**。

**记录还没落盘 → 空一段，不是错误。** 乐手刚起来、第一条消息还没写时，
`total: 0` 比一个错误码有用（编排者接着等即可）；对话镜像在同一处给的也是一份空
镜像。代价是「文件定位失败」与「会话确实还没有消息」在回执上不可区分，见留档。

**`read-screen` 走主线程动作泵。** 终端本体（`Arc<Mutex<Term>>`）后台线程读得动，
但 `pty_id → 终端` 那张表（`AppStore::terminals`）装的是 gpui 实体。为它在旁边另建
一份镜像表就是给 pane 生命周期再加一个必须同步注销的地方 —— 漏一次注销就永久攥着
一份一万行的回看缓冲（工单 03 留档里 `AiPerception::pane_closed` 那条耦合已经提醒
过一次）。读一屏是几十微秒的主线程占用，与建 pane 不是一个量级，走泵便宜得多。
于是它与 `start-session` / `send` 共用同一条泵、同一个 `ACTION_TIMEOUT`、同一种
`desktopBusy` 结论；`OrchestratorSignal::deadline()` 的穷尽 `match` 照旧兜住新信号。

**`blocks_on_desktop()` 更名 `needs_own_thread()`。** 那张表的**真实**用途是
「这条命令能不能就地在 HTTP 线程上答完」，而占着不放的理由有两种，旧名字只说得出
第一种：

1. 要回桌面主线程（`start-session` / `send` / `read-screen`）；
2. 要做慢 IO（`read-transcript` 读整条会话记录）。

`read-transcript` 不碰主线程，但一样不能占着那条队 —— hook 那个 HTTP 服务是单线程
循环，排在一次几十 MB 的全文件解析后面就是让 AI 状态感知给编排让路。改名而不是加
第二张表：两张表就是「新命令漏登记一处」的机会，而那正是这张表当初收成一份的理由。
四处穷尽 `match` 与测试一并改名。

**`read-screen` 的量：行数钳不拒，字节从头上砍。** 默认 50 行（一屏 TUI 通常
30~50 行，够看清一次审批提示的全文与上下文），上界 500 行（回看缓冲默认 10000 行，
不设上界一条命令就能把整条滚屏搬出来）。`0` 与超大值都**钳**回区间而不是报错——
那是编排者随手写出来的数，为它们各造一个错误码只多一次往返。

**尾部的定义：从最后一个非空行往回数。** 光标下方那一片空屏是纯噪音，不该占掉
配额；但**当中与开头的空行留着**——空行是画面的一部分（TUI 靠它分块），抹掉就不是
「那一屏的样子」了。取行/裁行尾空格/剥颜色属性/跳宽字符 spacer 全在新增的
`mt_terminal::TerminalEmulator::tail_lines` 里**一次持锁**做完：逐行各锁一次的话，
reader 线程会在中间推进状态机，取回来的几行就不是同一帧的画面了。行内口径与既有的
`line_text` 共用同一个 `row_text`（那条是「按绝对行号点名取一行」，AI 任务标记校验
锚点在用），两者有一条逐字相同的对账测试。备用屏（alt screen，agent 的 TUI 常驻
那一档）下 `history_size()` 恒 0，于是自然只给备用屏那一屏 —— 正是此刻用户看见的
东西，不必另加判断。

**响应体量上界 32 KiB（`MAX_READ_BYTES`），两条命令的处置相反。**
请求那道 64 KiB 的闸管不住这一头：读命令的产出由乐手写了多少决定。32 KiB ≈ 一万个
汉字，够装下一次回合的完整回答，又只占编排者上下文的一小块。

- **transcript 有游标**，砍掉的下次取得回来 → **少给几条 + `hasMore: true`**，
  绝不把一条消息劈成两半（半条回答没有价值，而 seq 的语义是「第几条」，
  劈开就没法表达「我读到某条的一半」）。
- **例外是第一条**：它自己就顶穿预算时**必须**给出去（截断 + `truncated: true`），
  否则 `nextCursor` 永远等于 `cursor`，编排者卡在这条上无限重试，后面的内容永久
  读不到。截断按**字节**并落在字符边界上（预算是字节预算，一个汉字三字节）。
- **screen 没有游标**（画面是此刻的样子，没有「上一页」）→ **从头上砍、保住尾部**，
  编排者要看的永远是最新那几行。回执里的 `truncated` 如实说明砍过。

**顺手修掉一个会让 CLI 整个读不到东西的坑：控制回执一律 Content-Length，不许
chunked。** tiny_http 默认在 body ≥ 32 KiB 时自动切 `Transfer-Encoding: chunked`
（`chunked_threshold` 的默认值），而 `mt-agent-cli` 是**手写的 HTTP 客户端**：它按
`\r\n\r\n` 切一次头体、剩下的整块当 JSON 解析（`mt_agent_control` 不带任何 HTTP
客户端依赖，那是它的设计取舍）。一旦分块，body 里就混进十六进制的块长度行，CLI 只
会答 `malformedResponse`。**工单 07 的两条读命令正是第一批产得出 32 KiB 以上回执的
命令**（`list-*` / `send` 的回执都只有几百字节），所以这个坑此前没人踩到。
修法是在 `respond()` 上加一句 `with_chunked_threshold(usize::MAX)`（我们的回执自带
上界，不需要流式分块省内存），并留一条 `大回执不许走分块传输` 钉住它 —— 那条测试
用汉字正文，因为块边界会把三字节序列劈开，连 `read_to_string` 都过不去（本票就是
这么发现它的）。

**两条命令都过 `resolve_target`。** 可见范围铁律只有那一处实现，`send` 是它的第一个
消费者，这两条照用。一条测试对两个端点各跑一遍三种语义（自指 / 别人的（含不存在）
/ 已关），并额外断言**被拒的读一次都不惊动桌面端、也不去翻磁盘**。另有一条钉住
「agent 退出但 pane 还在」时两条命令照常可用 —— 那正是「回头看它留下了什么」的时刻
（`resolve_target` 只判 pane 死活，不判 AI 会话状态，工单 05 的留档同源）。

**会话记录那条缝（`TranscriptSource`）是纯粹的测试缝，并且说明白了。**
唯一实现 `SessionLogTranscripts` 同时是**默认值** —— 生产路径一行都不必接线，
它整个转交既有的 `mt_ai::sessions::get_ai_session_content`（AI 历史面板与对话镜像
读的同一份代码，路径穿越校验也在它里头）。抽出这条缝只因为三家的定位逻辑各自扎在
用户 home 底下（`~/.claude` / `~/.codex` / `~/.grok`），而本票是会话记录解析的
**消费者、不是改造者**：既不许给它加一个「根目录可覆写」的参数，也绝不能让测试去读
用户真实的会话记录。与另外两条注入缝的区别写在类型注释里：那两条未注入时是
`Noop`（fail-closed，因为它们要动桌面），这一条未注入时是真的去磁盘上找。

**`HostImpl` 拿两张表而不是整个 `AiBridge`。** 后者会绕成
`ControlPlane → HostImpl → AiBridge → AiPerception → ControlPlane` 一圈放不掉的
`Arc`。`HookState` / `SessionTracker` 内部都是 `Arc<Mutex<..>>`、克隆即共享同一份，
且都不认识 `ControlPlane`。合并那一半拆成纯函数 `merge_pane_session`：hook 那张表
没有公开的写入口（`record_session` 是私有的，只有真 hook 事件进得去），不拆开的话
「hook 优先」这条一条测试都写不出来。

**新错误码清单**（闭集，CLI 按 code 分档；两条都落在退出码 4「改你的请求」）：

| code | status | 含义 |
|------|--------|------|
| `transcriptUnsupported` | 409 | 这个 agent 没有可解析的会话记录 —— 换 `read-screen` |
| `sessionUnidentified` | 409 | hook 还没上报会话身份，无从绑定（**不猜**） |

复用既有码：`paneNotFound`(404) / `paneGone`(410) / `selfTarget`(403) 全部经
`resolve_target`；`desktopBusy`(503) 只有 `read-screen` 会给（transcript 不走桌面）；
`badRequest`(400) 是缺 `targetPaneId`。**`read-screen` 没有新增失败码** ——
`ScreenFailure` 的两档（终端没了 / 主线程没答）正好映到 `paneGone` 与 `desktopBusy`。

**CLI 形状**：`read-transcript --pane <ID> [--cursor <SEQ>]` /
`read-screen --pane <ID> [--lines <N>]`。可选参数不给就**整个字段不出线** ——
默认行数只有桌面侧那一个常量，在 CLI 侧复制一份就是两处口径。`--help` 的
`after_help` 加了一段 `read notes:`，逐句被测试钉住：能力分层要给出改用哪条命令、
游标只在一个会话内有效、`hasMore` 怎么处理、绑定**不猜**（别让它以为是 bug 去绕
过），以及审批提示**不许代答**（ADR 0003 的铁律，`read-screen` 正是它最可能被误用
的地方）。

## 留档（未整改）

- **`read-transcript` 每次都把整条会话记录读一遍**。跑了几小时的会话那个 JSONL
  可以是几十 MB，每次请求都是一次全文件读 + 解析 + 一个 `Vec<AiSessionMessage>` 的
  分配。当前的止损是「另起线程，别卡住 hook 那条队」；真要治得让
  `TranscriptSource` 支持「从字节偏移续读」，而那需要把 seq ↔ 字节偏移的映射存在
  服务端（镜像那边的 `MirrorRuntime` 就是这么干的），也就是把本票刻意不建的那份
  每编排者 × 每乐手的解析器状态建起来。MVP 判为不值。
- **「记录文件定位失败」与「会话确实还没有消息」在回执上不可区分**，两者都是
  `total: 0` 的空一段。对编排者而言下一步相同（等，或 `read-screen`），但一条
  **永远**定位不到的会话（比如乐手跑在 WSL 里）会让它一直空转下去。
  `wait`（工单 06）应当是那个循环的闸；`--help` 里也写了 `read-screen` 是兜底。
- **WSL 里的乐手读不到 transcript**：`SessionLogTranscripts` 给
  `get_ai_session_content` 传的 `wsl_distro` 恒为 `None`。受编排会话一律起在本机
  项目里（SSH 远程项目在 `start-session` 那步就被 `remoteProjectUnsupported` 挡掉），
  但**启动器的 shell 可以是 `wsl`** —— 那时会话记录落在发行版内，本机路径下找不到。
  正确的修法是把项目的 `wsl_sessions_distro` 一并投影进 `ControlProject`；
  本票没做，因为那要动线上形状且没有真机可验。此档目前表现为一份空 transcript
  （不是错误），`read-screen` 照常可用。
- **定位用的是项目路径，不是 pane 自己的 cwd**。与对话镜像同一个口径（`mirror_task`
  也是拿 pane 所属项目的路径去找会话桶）。hook 推送的 `SessionIdentity` 里其实带着
  cwd，但那是**推**的事件载荷，`HookState` 上没有按 pty 查 cwd 的口，后台线程读不到。
  影响：启动器配了自定义 cwd（起在项目子目录里）时 Claude 的会话桶对不上，
  表现为空 transcript。修它要么给 `HookState` 加一张 cwd 表（碰 hook 那条链路，
  本票的纪律是不碰），要么等镜像那侧一起改。
- **`read-screen` 分不清「你要的行数比屏幕上的内容少」与「屏幕上就这么多」**：
  两者都只回实际拿到的行数，`truncated` 只标字节上界砍过。编排者拿到的行数正好等于
  它要的数时自己加大 `--lines` 再取一次即可，判为够用；加一个 `hasMoreLines` 要让
  桌面侧多算一次总行数，收益不抵。
- **transcript 的 seq 与镜像 seq 是同一套语义、不是同一串数字**（见上方设计决议
  的第一段）。两侧真要对齐得让本命令也产出提问卡片那几条，而那些类型住在
  `mt_relay_protocol`（跨工作区），`mt-ai` 不该依赖它。
- **`truncated: true` 那条消息的后半截再也取不回来**：游标是「第几条」，跳过它就是
  跳过整条。一条超过 32 KiB 的单条消息在实践中只可能是 agent 贴了整个文件，
  编排者要全文可以让乐手把它写到文件里、或者自己去读那个文件。
- **CLI ↔ 真 hook server 的整条 HTTP 往返仍未真机走过**（与工单 02/03/05 同一条），
  主缝/辅缝都是进程内对账。留工单 09 验收。**本票尤其需要真机看一眼**：
  上面那条 chunked 的坑就是「进程内 handler 对账绿、真 HTTP 往返红」的典型形状，
  已经补了一条真起 tiny_http 的测试，但走的是测试自己那个 HTTP 客户端而非 CLI 的。
- **三家真实会话记录的读出效果未验**（假记录只保证了增量与分层的语义）。Grok 的
  `updates.jsonl` 是 ACP 更新流、一条消息拆成多个 chunk 攒到边界才成一条，
  `get_ai_session_content` 已经处理，但真机上「一次回合读出来是几条」值得看一眼。
  留工单 09。
- **`blocks_on_desktop` → `needs_own_thread` 的更名与工单 06 并行**：06 的 `wait`
  也要在那张表上加一行。合并时把 `Wait` 那一支放进 `needs_own_thread` 的 `true`
  分支即可（`wait` 是「等时间」，第三种占着不放的理由，值得在那条注释里补一句）。
