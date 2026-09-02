# 13 — CLI 帮助 + 编排礼仪 Skill 改写 + 真机验收（收口）

**Parent:** ADR 0004（受编排会话的汇报推送）

**What to build:** 让编排者知道世界变了。`mt-agent-cli --help` 与 `.claude/skills/mini-term-orchestrator/SKILL.md`（唯一源头，`include_str!` 进主程序投放）按推送模型改写：派活之后等汇报送到自己的对话里，不再教它记游标；`wait` / `read-*` 降为备用。然后在真机上把整条链走通，**黄灯全链路这次必须覆盖**。

**Blocked by:** 12

**Status:** in progress（**真机验收仍是欠账**；CLI 帮助与 SKILL.md 那两项已随工单 14 整段重写——推送改成落文件 + `wait` 取件，2026-09-02 真机验收刚起步就被用户叫停。新形态的验收清单见 `14-reports-to-files-and-wait.md` 留档第 8 条）

- [x] `mt-agent-cli --help` 增加 "reports" 一节：汇报长什么样（`[mini-term]` 前缀、种类、任务编号）、什么时候到（编排者空闲时；忙时攒着合并）、收到 `AwaitingHuman` 该怎么做（转告用户、不代答、不 send）、收到 `NotAccepted` 该怎么做（read-screen 核对再重发）；send notes 与工单 10 对齐；wait notes 开头加一句「通常不需要——汇报会自己来」
- [x] SKILL.md 改写：删掉「两个时序坑」里记游标那套舞步（未被接收由汇报覆盖，start-session 后先 read-screen 确认提示符那条**保留**）；「A normal run」改成派活 → 继续干别的 / 回答用户 → 汇报到了再处理；规则里加「文字提问可答、黄灯归人」「汇报是自述，关键结果自己验证」；`cli-location` 两个标记必须原样保留（`orchestrator_skill.rs` 的渲染测试钉着）
- [x] `docs/tickets/orchestrator/README.md` 依赖图与表格补 10–13；`CLAUDE.md` 的「AI 状态判定」段落后加一小节指到 ADR 0004
- [ ] 真机验收（隔离数据目录，配方见 `~/.claude/projects/D--Git-mini-term/memory/feedback_no_e2e_gpui.md` 与工单 09 现场记录）：
  - [ ] 单乐手：派活 → 编排者不做任何等待 → 汇报出现在编排者 pane，含回复原文与任务编号
  - [ ] 编排者忙时两条汇报攒着，回合结束后合并送达一次
  - [ ] 黄灯全链路：乐手挂黄灯（Claude 关 auto mode 触发 PermissionRequest）→ 编排者收到 AwaitingHuman 并播报 → 人去乐手 pane 处理 → 恢复后 TurnEnded 送达；编排者自己挂黄灯期间汇报不投、处理完才投
  - [ ] 文字提问往返：乐手用文字提问并结束回合 → 汇报到编排者 → 编排者 send 回答 → 乐手继续
  - [ ] 未被接收：start-session 后立刻 send（agent 还在启动）→ 15s 后 NotAccepted 送达
  - [ ] 黄灯拦截：乐手挂黄灯时 send 得到 `targetAwaitingHuman`
  - [ ] 用户接管乐手聊一句 → 那一回合也汇报
  - [ ] Codex 一家至少走一遍单乐手 + 黄灯（它的 PermissionRequest 停在 ai-working，是判据最容易错的一家）
- [ ] 验收现场记录写进本票（照工单 09 的格式），未覆盖项明确留档

## 设计要点

- Skill 的读者是 LLM：每条规则说清「看到什么 → 做什么」，别写机制解释。汇报的样例文本贴一段真的（从真机验收抄）。
- 真机验收时 dev 实例与装机版同名，杀进程必须按路径过滤（记忆里有记档）。

### 实施决议（文档/CLI/Skill 三件，真机验收未做）

落点四个文件：`sidecars/src/bin/mt-agent-cli.rs`（`EXIT_CODE_HELP` + 两条新测试）、
`.claude/skills/mini-term-orchestrator/SKILL.md`（整份改写）、
`crates/mt-app/src/orchestrator_skill.rs`（渲染测试换了一条断言）、
`CLAUDE.md` 与 `docs/tickets/orchestrator/README.md`。
**仓里没有 `.codex/skills/`**——那一份是投放时按同一源头现渲染的，没有副本要同步。

**`reports` 一节放在退出码之后、send / wait notes 之前。** 它现在是整条链路的主干，
send notes 的 `taskId` 那段与 wait notes 的开场都往回指它；而 `--help` 是每次都会被
塞进编排者上下文的东西，主干不在前面它可能读到一半就去动手了。

**五档按编排者**看得到的说法**列，不写 `AwaitingHuman` / `NotAccepted`。**
那两个是 `ReportKind` 的内部名，汇报文本里根本不出现；写进 `--help` 只会让编排者
去找一个不存在的字段。于是 help 里的小标题是 `turn ended` / `waiting for a human` /
`exited` / `closed` / `not picked up`——与渲染出来的抬头对得上。

**汇报的语言跟随桌面显示语言，两处都点了一句。** 跨语言不变量只有两条：`[mini-term]`
前缀（它不进字典，工单 12 的决议）与抬头字段的顺序。SKILL.md 里贴的两段样例用
**英文字典**渲染（Skill 正文是英文，读者要能逐字对上），与工单 12 留档里那两段中文
是同一场景、同一批键。

**「派活即等会拿到上一回合的 `ai-idle`」这条事实没删，只是降级。** 记游标那套舞步
（读一次 `nextCursor` → send → wait → 比对增量）整段删掉了——它的用途已被汇报覆盖；
但那条事实本身仍然为真，于是挪进 Skill 末尾的「When to fall back to pulling」里，
作为「别拿 `wait` 当回合完成判据」的理由。`--help` 的 wait notes 里那句原样留着。

**Skill 的规则从 6 条变 9 条，新增的三条全排在前四位。** 顺序按「最容易做错的在前」：
① 不代答（原第 1 条，改写成同时覆盖汇报里的黄灯与 `send` 被拒两条入口）；
② 文字提问可答——与①是同一条边界的两侧，判据是**哪一种汇报带来的问题**，不是问题
怎么措辞；③ `targetAwaitingHuman` 是规矩不是故障，下一步是转告 + 去干别的；
④ 尾部会被追加、别自己再写一遍。原来的递归/可见范围/上限/整块粘贴四条顺延。

**`read-screen` 在 `--help` 里补了两处而不是一处**（工单 10 留档第一条）：编排者撞上
黄灯只有两条路——自己 `wait` 撞到 `attention`，或者 `send` 被 `targetAwaitingHuman`
拒掉。只补一处就是漏一半。

**`orchestrator_skill.rs` 的渲染测试换了一条断言。** 原来钉的
`` `wait` alone cannot tell you a turn finished `` 那句随改写消失了，换成钉推送模型的
两条不变量：`Results are pushed to you` 与 `[mini-term]`。`Never answer for a human`
与 `` Do not `send` immediately after `start-session` `` 两条原样留着——它们是这份
Skill 存在的理由，改写不该动它们。`cli-location` 两个标记一个字没动。

## 验收现场（实施方填）

**未做**——本次交付只含 CLI 帮助、Skill 改写与文档收口，没有起过 GPUI 实例。
上面那两条验收项（八个子项 + 现场记录）留空，由编排会话另行安排。

## 留档（实施方填）

1. **真机验收整节未做**，是本票唯一的欠账。工单 12 留档第 8 条点名的三条仍是最该先看的：
   黄灯全链路（受编排会话挂黄灯 → 汇报 → 人处理 → 恢复后终态汇报，ADR 0004 的「后果」
   第三条点名要求）、汇报进入编排者对话后它会不会把 `[mini-term]` 当成用户发言去回复、
   以及一次派活到收到汇报的实际延迟。另外工单 12 留档第 2 条要求看一眼 Claude 的 JSONL
   相对 `Stop` hook 的落盘先后（增量为空补画面尾部那一档是否真的被触发）。
2. **SKILL.md 里那两段样例是按英文字典手工拼的**（与 `locales/orchestrator.ts` 的 `en`
   逐字一致），不是从真机抄的，也没有测试钉住。真机验收时应拿一条真实投递的文本对一次；
   字典日后被润色时样例会悄悄走散——判为可接受（样例的作用是让编排者认出形状，不是逐字
   匹配），但值得知道。
3. **`EXIT_CODE_HELP` 因这一节长了约 35 行**，而它每次 `--help` 都会整段进编排者的上下文。
   没有再压缩：这一节替掉的正是编排者原本要靠 `wait` / `read-*` 轮询摸索出来的东西。真机上
   若发现 help 太长挤掉别的，第一个该砍的是 `exited` / `closed` 那两行——编排者对这两件事
   做不了什么。
4. **Skill 的 frontmatter `description` 顺手改了一句**（"wait for them to settle, and read
   their results" → "have their results pushed back into this conversation"）。它决定这份
   Skill 何时被 agent 加载，改动很小但不是零风险，真机上顺带看一眼还能不能被正常触发。
5. **仓里没有 `.codex/skills/mini-term-orchestrator/SKILL.md`**：Codex 版是投放时按同一源头
   现渲染的（只差 `allowed-tools` 一行），没有第二份副本要同步。
6. **CLI 帮助与 SKILL.md 已被工单 14 整段重写**（2026-09-02）：推送模型换成「汇报落文件 +
   `wait` 取件」，`reports` 一节、wait notes、Skill 的样例与五个小标题（改成
   `turn-ended` / `awaiting-human` / … 这几个稳定 slug）全部随之改过，本票留档第 2、3、4
   条描述的都是旧文本。上面第 1 条（真机验收整节未做）**仍然是欠账**，且要按新形态重走
   ——清单见 `14-reports-to-files-and-wait.md` 的留档第 9 条。
