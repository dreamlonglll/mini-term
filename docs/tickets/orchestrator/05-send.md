# 05 — 派活：send 写穿（bracketed paste 多行）

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** `send` 让编排者向自己的乐手写一段 prompt，语义与移动端指令完全一致——立即写穿不排队，等价本人在桌面敲入同样内容并回车（输入跟踪 / AI marker / SSH autofill 解除一个都不能少，直接走既有写穿入口）。多行文本以 bracketed paste 包裹装配，避免中途换行提前触发发送。

**Blocked by:** 03（先有乐手可派活）

**Status:** ready-for-agent

- [ ] 单行 prompt 正确送达乐手内 agent 并触发执行
- [ ] 多行 prompt（含代码块）作为整体粘贴送达，不提前发送
- [ ] 向非自启 pane send 被拒（「不存在」语义）
- [ ] 主缝测试：写穿内容与 bracketed paste 装配经假宿主断言
- [ ] 真机验证：Claude / Codex / Grok 三家实际收到多行粘贴的表现符合预期
