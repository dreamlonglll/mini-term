# 08 — 乐手并发上限设置项

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** 设置页新增「乐手并发上限」调节项（默认 5），即时生效：调整后的上限立刻作用于后续 `start-session` 裁决。

**Blocked by:** 03（上限裁决先存在）

**Status:** ready-for-agent

- [ ] 设置可调、重启后保持
- [ ] 调低后不杀已存活乐手，只影响后续启动裁决
- [ ] 新文案进 i18n 字典源头并重跑生成器
