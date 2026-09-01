# 06 — 等待：wait 长轮询四类终态

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** `wait` 长轮询等待某乐手状态收敛，支持超时，返回四类终态：`ai-idle`（干完，含 cause）/ `attention`（停在等审批或向人提问，含原因，如 PermissionRequest）/ `idle`（agent 已退出）/ pane 不存在。状态判定完全复用 hook 权威状态机与既有兜底（停摆收敛 / 用户打断），不新增判定逻辑。attention 时编排者不代答（ADR 0003 铁律）：它拿到状态后在自己对话里播报请用户处理，零新增 UI——既有黄灯徽章兜着。

**Blocked by:** 03（先有乐手可等）

**Status:** ready-for-agent

- [ ] 四类终态 + 超时语义在主缝测试全覆盖（经假宿主驱动状态迁移）
- [ ] attention 返回携带原因原文
- [ ] 向非自启 pane wait 被拒（「不存在」语义）
- [ ] 真机：乐手挂黄灯 → 编排者播报 → 人工在乐手 pane 处理 → 下一次 wait 拿到恢复后的终态，全流程走通
