# 02 — 追踪弹：编排能力授予 + 控制面骨架 + CLI 首命令

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** 编排者的追踪弹，切通最薄一条端到端路径。用户在 AI 启动器编辑界面勾选「允许编排」；用该启动器起的 pane 经 PTY 生命周期层既有的应用内部环境变量通道注入编排令牌与自身 pane 身份（`MINITERM_` 保留前缀已保证用户/项目级 env 不可覆盖）；hook server 既有的本地 HTTP 服务新增一组控制端点，令牌鉴权 fail-closed；新 sidecar `mt-agent-cli`（sidecars 独立工作区，版本自成语义）提供首批两个命令：`list-launchers` 与 `list-projects`（返回本项目 + 同分组项目；未分组项目仅本项目）。桌面能力（项目与分组投影、启动器名单）经注入 trait 提供，控制服务不依赖应用层——照抄 `RelayHost`/Noop 假实现的既有模式。

演示口径：在编排者 pane 里跑 CLI 能列出启动器与可达项目；在普通 pane 里跑同一命令被明确拒绝。

**Blocked by:** None — can start immediately.

**Status:** ready-for-agent

- [ ] 启动器编辑 UI 有「允许编排」开关；配置存 config.db，**不碰 config.json 投影**（不涉 `ConfigSshView` 对账）
- [ ] 勾选启动器起的 pane 内 CLI 可用；普通 pane / 无令牌 / 坏令牌一律被拒（fail-closed）
- [ ] `list-projects` 可达范围 = 本项目 + 同分组；未分组仅本项目；改分组即时生效
- [ ] 主缝测试：控制端点经注入假宿主覆盖鉴权与范围裁决（先例：hook server 既有 HTTP 级测试 + `NoopRelayHost` 模式）
- [ ] 辅缝脚手架：主仓测试直调 sidecar CLI 的解析器读真 handler 产出（先例：「投影能被 sidecar 的解析器读懂」跨工作区对账）
- [ ] 新文案进 i18n 字典源头并重跑生成器
- [ ] cargo test --workspace 全绿
