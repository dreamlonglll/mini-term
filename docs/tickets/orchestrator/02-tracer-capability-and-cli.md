# 02 — 追踪弹：编排能力授予 + 控制面骨架 + CLI 首命令

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** 编排者的追踪弹，切通最薄一条端到端路径。用户在 AI 启动器编辑界面勾选「允许编排」；用该启动器起的 pane 经 PTY 生命周期层既有的应用内部环境变量通道注入编排令牌与自身 pane 身份（`MINITERM_` 保留前缀已保证用户/项目级 env 不可覆盖）；hook server 既有的本地 HTTP 服务新增一组控制端点，令牌鉴权 fail-closed；新 sidecar `mt-agent-cli`（sidecars 独立工作区，版本自成语义）提供首批两个命令：`list-launchers` 与 `list-projects`（返回本项目 + 同分组项目；未分组项目仅本项目）。桌面能力（项目与分组投影、启动器名单）经注入 trait 提供，控制服务不依赖应用层——照抄 `RelayHost`/Noop 假实现的既有模式。

演示口径：在编排者 pane 里跑 CLI 能列出启动器与可达项目；在普通 pane 里跑同一命令被明确拒绝。

**Blocked by:** None — can start immediately.

**Status:** done（6ad5a31/12995d2/ab3c07e，随合并 ab73962 入 main）

- [x] 启动器编辑 UI 有「允许编排」开关；配置存 config.db，**不碰 config.json 投影**（不涉 `ConfigSshView` 对账）
- [x] 勾选启动器起的 pane 内 CLI 可用；普通 pane / 无令牌 / 坏令牌一律被拒（fail-closed）——CLI↔真 hook server 的整条 HTTP 往返未真机走过，留工单 09 验收
- [x] `list-projects` 可达范围 = 本项目 + 同分组；未分组仅本项目；改分组即时生效
- [x] 主缝测试：`mt-ai::control` 17 例（真 tiny_http + 假宿主）
- [x] 辅缝脚手架：`orchestrator_wire.rs` 4 例（跨工作区引 `sidecars/agent-control`）
- [x] 新文案进 i18n 字典源头并重跑生成器
- [x] cargo test --workspace 全绿（合并后复验）

**合并期决议**（主会话解冲突时定）：`LaunchRequest` 增 `grant` 字段，spawn 链穿 `OrchestratorGrant`。移动端引用「允许编排」启动器暂**不授予**（spec 故事 22 的放行需协议带 flag，留后续工单裁决）。

**两轴评审后的整改**（同波落地）：① 工单 01 预留的 `extra_env` 通用注入缝判为 Speculative Generality 删除（令牌走不了它——需要 spawn 时才诞生的 pty_id），`merge_internal_env` 及其 3 例测试一并删除，`_with_env` 系列归名 `_with_grant`；② `ControlPlane` 的 grants/tokens 两把锁并成一把（grant/revoke 加锁顺序相反是 AB-BA 雷）；③ SSH 远程项目不授予令牌（原会在本地 ssh 客户端进程上登记一枚永远用不上的活令牌）；④ CLI 帮助文案 musicians → orchestrated sessions。

**评审留档（未整改）**：`mt-agent-cli` 与 `miniterm-hook` 的端口发现同形异构（宜共享进 mt-core，但动 hook 属行为变更，另行立项）；`upsert_launcher` 尾参裸 bool；「改分组即时生效」实为配置落盘刷镜像（`save_config_soon` 有 500ms 防抖，最坏滞后半秒，实用可接受）。
