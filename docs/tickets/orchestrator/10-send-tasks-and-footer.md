# 10 — 派活改造：黄灯拦截 + 任务身份 + 汇报格式尾部（含设置项）

**Parent:** ADR 0004（受编排会话的汇报推送）

**What to build:** `send` 三处改造。① 目标受编排会话停在等人处理（attention）时**拒绝写入**（写入等于代答）；② 每次 `send` 得到一个任务编号，桌面端记下派活时刻与目标当时的状态，回执带回 `taskId` 与 `targetStatus`；③ 桌面端在 prompt 尾部追加一段固定的汇报格式要求（设置可关，默认开）。这张票只动派活这一侧，**不生成任何汇报**——回合追踪在 11，投递在 12。

**Blocked by:** 无（与 11 并行；12 依赖本票的任务账本）

**Status:** done

- [x] 目标 `cause` 属 attention（`hook_server::is_attention_cause`）且 pane 活着时 `send` 被拒，错误码 `targetAwaitingHuman`（409），**一个字节都不落到桌面端**（主缝用 FakeActions 断言 `send_input` 未被调用）
- [x] `send` 成功回执形状 `{sent: {paneId, taskId, bracketedPaste, targetStatus}}`；`taskId` 每编排者单调递增（`t1`、`t2`…，编排者重新授予后从 `t1` 重数）；`targetStatus` 是写入那一刻 `pane_liveness` 的 `status` 原文
- [x] 任务账本：`ControlPlane` 记 `Task { id, orchestrator_pane_id, target_pane_id, written_at: Instant, target_status_at_write }`，每编排者保留最近 200 条；对外暴露 `pub fn note_task_written`/查询接口给 11、12 用（形状由实施方定，但 12 要能按乐手取「尚未汇报的任务编号」）
- [x] 格式尾部：`PaneInput` 装配时在正文后追加一个空行 + 尾部文案，**包在 bracketed paste 之内、回车之外**；尾部文案来自 mt-i18n 新命名空间 `orchestrator`（键 `reportFooter`），中英各一份；`ControlPlane::set_report_footer(bool)` 关掉后一个字都不追加
- [x] 设置项：`AppConfig::orchestrator_report_footer: Option<bool>`（`None` = 开），round-trip 测试照 `orchestrator_session_cap` 那条；设置页「AI」区块并发上限旁边加开关，文案进 `settings.ts`，配置加载/保存时推给控制面（找 `set_session_cap` 的调用点照抄）
- [x] sidecars/agent-control：`SendReceipt` 增 `task_id: String`、`target_status: String`（都 `#[serde(default)]`，缺省空串——旧桌面端在场时编排者拿到空串去核对，别猜）；`targetAwaitingHuman` 落在退出码 4 那档；mt-agent-cli `--help` 的 send notes 写清三件事（黄灯被拒是规矩不是错误、taskId 的含义、尾部会被追加）
- [x] `crates/mt-ai/tests/orchestrator_wire.rs`：回执新字段能被 sidecar 解析器读懂；新错误码两侧一致
- [x] `cargo test -p mt-ai -p mt-config -p mt-app`（mt-app 无 lib 目标，跑单测用 `--bin mini-term`）+ `cargo test --manifest-path sidecars/Cargo.toml --workspace` 全绿；i18n 字典由 `node crates/mt-i18n/tools/gen_from_ts.mjs` 重新生成，`tests/consistency.rs` 的对账常量随之更新

## 设计要点（给实施方）

- **黄灯判据是成因不是状态**：Codex 的 `PermissionRequest` 停在 `ai-working`（`hook_server::map_event_to_status` 的专门一条），只看状态会放过它。`PaneLiveness::cause` 已经带着成因，`send` 里读它即可；判定放在 `resolve_target` 之后、`send_input` 之前。
- **拒绝不是排队**：ADR 0004 定的是「派活即写入，桌面端不排队」，唯一的例外就是黄灯。被拒的正文一个字不缓存，编排者收到 `targetAwaitingHuman` 后自己决定何时重发。错误消息里直接写「tell the user and wait for that session to settle」，与既有 `emptyInput` 那条同一个口气——它是一条裁决，不是入参校验。
- **尾部在控制面装配**：`PaneInput::assemble` 现在只吃正文；加一个带尾部的构造（或给 `assemble` 加参数），两份字节（包裹版 / 裸版）都带尾部。mt-ai 加 `mt-i18n` 依赖（零第三方依赖的叶子 crate，`Cargo.toml` 注释里写明理由），用 `mt_i18n::t("orchestrator", "reportFooter")` 取全局 locale 的文案——mt-app 启动与切语言时已经 `set_locale`，不必再传。
- **尾部文案内容**（中文版意思，英文照译；实施方润色）：
  > 【来自编排者的固定要求】做完后请用以下格式收尾：结果 / 改动的文件 / 做过的验证 / 未完成或存疑的事。遇到不确定的问题，用文字提出并结束本回合等待答复，不要使用交互式提问工具。
- **`targetStatus` 只是事实**，不折成「是不是在排队」的布尔：`ai-working` 意味着 prompt 进了 agent 自己的输入缓冲（Claude / Codex 会排队，Grok 未验），`ai-idle` / `idle` 意味着立刻开跑或写进了裸 shell。`--help` 里把这三档的含义写给编排者。
- **任务账本住在 `TokenRegistry` 旁边、同一把锁**：编排者撤销令牌时它名下的任务一并清掉（与 `sessions` 的 `depart` 同款处置）；乐手关掉时该乐手的任务照留（12 要拿它们做「已关闭」那条汇报的任务清单），等编排者离场再清。
- **回执仍不回显正文**（工单 05 的保密面），`Task` 结构里也**不存正文**。
- **别动 `wait` / `read-*`**，别动 Skill（13 收口）。

## 纪律

- 禁跑 `cargo fmt`。Edit 工具可能把整份文件写成 CRLF，改完用 `git ls-files --eol` 核对，LF 文件保持 LF。
- 不做任何 git 提交 / stash / checkout——由编排会话统一提交。
- 与工单 11 并行在同一个 worktree 里：11 只新增 `crates/mt-ai/src/reports.rs` 与 `lib.rs` 里一行 `mod`，本票**别碰那两处**；cargo 构建锁互相等是正常的，链接期 LNK1104 是杀软扫 exe 的随机撞，重跑一次即可。

## 设计决议

**黄灯闸只问一次死活，回执里的 `targetStatus` 用的是同一份快照。** `send` 在
`resolve_target` 之后调**一次** `pane_liveness`，三处都读它：`alive`（裁决到落地
之间被关掉 → `paneGone`）、`cause`（黄灯 → `targetAwaitingHuman`）、`status`
（回执与任务账本里的 `targetStatus`）。问两次的话闸看到的与回执报的可能不是同一
瞬间的事实——而这两件事恰恰会被编排者放在一起读（「它没在等人，所以我写进去了，
写的时候它是 ai-working」）。`resolve_target` 里那次 `alive` 检查因此被重复了一遍，
留着是有意的：它顺手把那道 TOCTOU 缩到了一次快照之内。

**判据收成 `PaneLiveness::awaiting_human()` 一个方法。** 「停在等人处理」此前在
`settled()` 里写了一遍（`wait` 的 attention 终态），本票要第二遍（send 闸），
工单 12 还要第三遍（投递闸）。摊开写三次就是三个走散的机会，而走散的方式是固定
的那一种：忘了 Codex 的 `PermissionRequest` 停在 `ai-working`，于是写成
`status == "ai-idle" && ...`。收成一个 `pub fn` 之后 12 直接调它。

**`targetStatus` 推翻了工单 05 的「回执不带状态」——但推翻的是字段，不是那条理由。**
05 拒绝的是**裸 `status`**：写穿之后那一瞬的状态一定还是写之前的样子，摆在回执里
会被读成「它干完了」。本票加的字段回答的是另一个问题——「这段 prompt 是被立刻处理，
还是排进了对面的输入缓冲」，而那是编排者接下来该等多久的唯一依据。两条防线保住 05
的本意：① 名字是 `targetStatus` 不是 `status`（前缀就是那句提醒），主缝有一条断言
钉住「不许叫 status」；② 文档与 `--help` 都把三档的含义写死成「写入那一刻的事实」。

**任务编号在写穿成功之后才发，失败不消耗号。** 号里出现看不见的洞会让编排者以为
自己漏收了一条汇报（`t1` `t3` 之间那个 `t2` 去哪了？）。代价是「写出去了但回执丢了」
那一档（`desktopBusy`）会留下一条编排者不知道编号的任务——与起会话的
`desktopBusy` 同款处境，处置也同款：`--help` 里那句「先 list-panes 查一眼」。

**任务账本住在 `TokenRegistry` 里，与令牌同一把锁、同一条命。** 编排者撤销令牌
（`revoke_pane`）或重新授予（`grant`，前世→今生）时 `depart` 把整本账丢掉，号码机
一并归零，于是新身份从 `t1` 重数——让它看见 `t37` 只会让它以为自己派过 36 次活。
**乐手那一路相反**：乐手 pane 被关掉时它身上的任务照留，因为工单 12 的「已关闭」
那条汇报要引它们；等编排者自己离场再一并清。这两条与 `sessions` 记账的处置一字不差
（活着的留、已关的收），刻意保持同构。

**`Task` 里不存正文，`reported` 是账本自己的位。** 保密面照抄 05（prompt 是用户
项目里的内容，不进日志/错误/回执，也不进账本，主缝有一条 `Debug` 断言钉住）。
「尚未汇报」做成 `Task::reported` 而不是另建一张表：查询接口给了两条——
`unreported_task_ids`（只看）与 `take_unreported_task_ids`（取一次即收敛），
后者是 12 渲染汇报时该调的那条。**取一次即收敛**与 `monitor` 那条「降级结论必须落盘、
不许每轮重算」同一条铁律：重复带出去会让编排者以为同一批活派了两遍。

**账本上限 200，超出时从最旧的丢起，号码不回收。** 与 `MAX_SESSIONS_PER_ORCHESTRATOR`
那条修剪分量不同——那边丢一条活着的记账就是造一个幽灵会话，这边丢掉的只是「回头
还能不能查到这条任务」。号码不回收是硬要求：回收会让同一次授予里出现两条同号任务，
而编号是汇报与派活对上的唯一凭据。

**尾部装配加的是 `assemble_with_footer`，`assemble` 原样留着。** 后者现在是
「不带尾部」那一档的别名，`mt-app` 那条跨 crate 对账测试（写穿与用户按 Ctrl+V
装配出同一串字节）因此一行没动——它比的是**正文口径**，尾部不该混进去。三条口径：

- **空正文判定只看正文**：先判空、再谈尾部。反过来的话「空 prompt + 尾部」就不空了，
  `emptyInput`（裸回车即代答）会被我们自己加的字绕过去——编排者发一个空 `send`，
  对面收到的是一段格式要求外加一个回车。主缝有一条 `尾部不许把空正文救活`。
- **正文与尾部之间空一行**（`\r\r`）：挨着写会被读成正文最后一句的续写，而它是
  一条与任务无关的元指令。
- **尾部照样过一遍归一**（换行 → `\r`、剔掉 `ESC[201~`）：眼下字典里是一行，
  但没有理由让「粘贴块不会被提前截断」这条不变量取决于文案怎么写。

**尾部文案钉「在不在、在哪儿」，不钉字面。** 测试拿 `mt_i18n::t("orchestrator",
"reportFooter")` 的返回值拼期望串——改一个字的润色不该红一条测试；另有一条双语
体检（非空、无口语别名、不含 ESC）在 `mt-app` 那侧（那是唯一同时看得见字典与
装配口径的地方）。

**新命名空间 `orchestrator` 而不是往 `settings` 里塞。** 这个 ns 里的字符串
**不进界面**，而是被写穿进另一个 AI 会话的终端，读者是另一个 LLM。措辞按「指令」
写不按「界面提示」写，与 `settings` 混在一起早晚会有人按界面文案的口径去改它。
工单 12 的全部汇报文案也落这个 ns（本票只放 `reportFooter` 一条）。

**mt-ai 引 mt-i18n 是必要的，且不破坏它的依赖体量。** 硬编码那段尾部就等于只有
一种语言，而它是要对面那个 LLM 照着执行的指令。mt-i18n 是零第三方依赖的叶子
（字典是编译期静态表，唯一且可选的依赖 serde 本 crate 早就在用），并且明令绝不引
gpui；`t()` 的读路径是一次原子读 + 二分查找，摆在 HTTP 线程上没有代价。语言由
mt-app 在启动与切语言时 `set_locale` 定，mt-ai 只读那个全局，不自己传 locale。
⚠️ 与 `mt-core` 那条依赖铁律无关——mt-core 才是被三个 sidecar 链接的那个，mt-ai 不是。

**设置项照抄上限那一套，只在一处不同。** `AppConfig::orchestrator_report_footer:
Option<bool>`（`None` = 开）、`skip_serializing_if`、专用 setter 当场推控制面 +
`save_config_soon` 落盘、`refresh_orchestrator_mirror` 顺带推一次（启动接线与每次
落盘都经过它）。唯一不同是兜底：上限的默认值是 `mt_ai::DEFAULT_SESSION_CAP` 那个
常量，而这个开关的默认值两侧都是 `true`——`mt_app::orchestrator::resolve_report_footer`
是**用户配置的兜底口径**，`ControlPlane` 那个 `AtomicBool` 的初值是**没接线时的行为**，
一条测试拿两侧真值比一次，防它们走散。

**`targetAwaitingHuman` 落 409 + 退出码 4，不新造档位。** 它与
`remoteProjectUnsupported` / `transcriptUnsupported` 同属「当前这个东西的状态不允许
你这么干」（409），CLI 侧按既有分档自动落进 4（不是鉴权、不是够不着）。错误消息
与 `emptyInput` 同一个口气：说清是哪条规矩 + 把下一步写出来（tell the user and wait
for that session to settle）——它是一条裁决，不是入参校验。

## 留档（未整改）

- **`--help` 里没写「黄灯时该怎么看到那个提示」**。编排者收到 `targetAwaitingHuman`
  之后最该做的其实是 `read-screen` 把审批提示原文读出来转告用户，而 help 里只说了
  "Tell the user"。没加是因为 read notes 那一段结尾已经写着「read-screen 是逐字读
  待批提示的办法」，再写一遍会让 send notes 变得更长（那段文字是每次 `--help` 都
  塞进编排者上下文的东西）。工单 13 收口 Skill 时把这条串起来更合适。
- **黄灯闸看的是「桌面端此刻知道的成因」，无 hook 的乐手一律穿过去**。降级路径上
  `cause` 恒为 `None`（monitor 那条路只有输出活跃度、没有事件），于是 opencode / pi
  这类只靠输入检测识别的 agent 停在一个交互式提问上时，`send` 照样写得进去——那正是
  ADR 0003 能力分层的既有边界（工单 07 的 `transcriptUnsupported` 是同一条线），
  本票没有扩大也没有收窄它。
- **`taskId` 与回合不是一一对应，桌面端也不打算让它成为一一对应**（ADR 0004 明写）。
  本票只保证「每次成功写入有一个编号 + 写入那一刻对面什么状态」；把编号绑到回合上是
  工单 11/12 的账本按「尚未汇报」清单近似的，近似的口径在 11 的验收里。
- **任务账本的两条查询与工单 11 的收件箱有语义重叠**：11 的账本自己也维护一份
  「该乐手尚未汇报的任务编号」。两者哪一份是 12 的真源由 12 定——本票把
  `take_unreported_task_ids` 备好并写清「取一次即收敛」，12 若改用 11 那份，
  这两个方法就只剩排查用途（不影响正确性，但会是一处冗余）。
- **`Task` / `MAX_TASKS_PER_ORCHESTRATOR` 没进 `lib.rs` 的 `pub use` 清单**，只能走
  `mt_ai::control::Task`（`control` 本来就是 `pub mod`，够得着）。没加是因为本票与
  工单 11 并行在同一个 worktree 里，而 11 要动的正是 `lib.rs`——两边同时整文件重写
  有互相盖掉的风险。12 若需要在 `mt-app` 里直接引这两个名字，顺手补进那张清单即可。
- **`unreported_of_hand` 是全表扫描**（全部编排者 × 最多 200 条）。按乐手建二级索引
  更快，但那是第二份必须与 `tasks` 同步的事实，而实际规模是「1~2 个编排者 × 几十条」
  ——一次扫描是微秒级，不值得为它多一处可能不同步的地方。
- **回执里的 `targetStatus` 与 `bracketedPaste` 有一档冗余**：`idle` + `false` 几乎
  总是同时出现（agent 退了 → pane 退回裸 shell → 没开粘贴模式）。留着两个是因为它们
  的来源不同（一个是 AI 状态机，一个是 VT 状态机那一位），真出现「`idle` 但
  `bracketedPaste: true`」时那本身就是编排者该知道的异常。
- **设置页那句 `orchestrationFooter` 里「『移动端』面板里的『允许编排』」已经过时**
  （AI 启动器的管理在 a4c98df 迁到了「设置 → Shell」）。与本票无关，没顺手改：
  改它会动到与本票无关的文案条目，留给下一个动这一页的人。
- **没做真机验证**（并行还有工单 11 在同一个 worktree 里跑，起 GPUI dev 实例会占
  `target/debug` 的 exe）。设置页那个新开关的渲染、黄灯拦截在真 Claude/Codex 上的
  表现、以及尾部被三家 agent 实际读到的效果，都留给工单 13 的真机验收一并走。
  尾部这一条尤其值得看：**它会进受编排会话的对话历史，每一次派活都多一段**，
  真机上要确认它不会把 agent 的注意力从任务本身带偏。
