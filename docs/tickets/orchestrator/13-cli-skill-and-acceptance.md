# 13 — CLI 帮助 + 编排礼仪 Skill 改写 + 真机验收（收口）

**Parent:** ADR 0004（受编排会话的汇报推送）

**What to build:** 让编排者知道世界变了。`mt-agent-cli --help` 与 `.claude/skills/mini-term-orchestrator/SKILL.md`（唯一源头，`include_str!` 进主程序投放）按推送模型改写：派活之后等汇报送到自己的对话里，不再教它记游标；`wait` / `read-*` 降为备用。然后在真机上把整条链走通，**黄灯全链路这次必须覆盖**。

**Blocked by:** 12

**Status:** todo

- [ ] `mt-agent-cli --help` 增加 "reports" 一节：汇报长什么样（`[mini-term]` 前缀、种类、任务编号）、什么时候到（编排者空闲时；忙时攒着合并）、收到 `AwaitingHuman` 该怎么做（转告用户、不代答、不 send）、收到 `NotAccepted` 该怎么做（read-screen 核对再重发）；send notes 与工单 10 对齐；wait notes 开头加一句「通常不需要——汇报会自己来」
- [ ] SKILL.md 改写：删掉「两个时序坑」里记游标那套舞步（未被接收由汇报覆盖，start-session 后先 read-screen 确认提示符那条**保留**）；「A normal run」改成派活 → 继续干别的 / 回答用户 → 汇报到了再处理；规则里加「文字提问可答、黄灯归人」「汇报是自述，关键结果自己验证」；`cli-location` 两个标记必须原样保留（`orchestrator_skill.rs` 的渲染测试钉着）
- [ ] `docs/tickets/orchestrator/README.md` 依赖图与表格补 10–13；`CLAUDE.md` 的「AI 状态判定」段落后加一小节指到 ADR 0004
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

## 验收现场（实施方填）

## 留档（实施方填）
