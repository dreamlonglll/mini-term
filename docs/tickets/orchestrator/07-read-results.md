# 07 — 读结果：read-transcript + read-screen

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** 读结果两层（ADR 0003 的能力分层）。`read-transcript`：按增量读乐手的结构化会话记录，仅 Claude / Codex / Grok 可用；绑定以 hook 上报的会话身份为权威，opencode / pi 等无会话记录的 agent 明确报错，**禁止启发式绑定**（与对话镜像同一条铁律）；增量口径与镜像 seq 一致。`read-screen`：对所有乐手可用，进程内直读该 pane 终端画面（VT grid）尾部 N 行纯文本——无记录 agent 的兜底，也用于看清审批提示原文。

**Blocked by:** 03（先有乐手可读）

**Status:** ready-for-agent

- [ ] 三大家乐手的回答能以结构化增量读出（新增量只含上次之后的内容）
- [ ] 无记录 agent 调 read-transcript 得到明确错误；read-screen 对其可用
- [ ] 向非自启 pane 读取被拒（「不存在」语义）
- [ ] 主缝测试覆盖能力分层、增量语义与范围裁决
