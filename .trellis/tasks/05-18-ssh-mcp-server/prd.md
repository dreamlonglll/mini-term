# SSH MCP Server: 按项目隔离的 stdio sidecar

## Goal

把 mini-term 现有的 SSH 连接管理（`config.json` 里的 `sshConnections`）做成一个独立的 stdio MCP Server（sidecar 二进制），让运行在 mini-term 终端里的 AI agent（Claude Code / Codex）能以 MCP 工具的形式调用已保存的 SSH 连接，在远程主机上程序化执行命令。SSH MCP 按项目隔离启用：只有在某个项目上启用后，才为该项目写入对应的 MCP 注册配置。

## What I already know

### 现状（已勘察代码）

- SSH 连接存在 `config.json` 的 `sshConnections`（`config.rs:172` `SshConnection` = id/name/host/port/user/password?/identityFile?/proxyJump?/group?）。密码明文存储。
- 现在连接是 UI 触发：右键菜单 → `connectSsh()`（`TerminalInstance.tsx:34`）→ `arm_ssh_autofill`（`pty.rs:990`）注册密码自动填充 → `prepare_ssh_key`（`ssh.rs:60`）收紧私钥权限 → 往 PTY 写 `ssh` 命令。
- `arm_ssh_autofill` 绑定 mini-term 的 `pty_id`，sidecar 的 ssh 子进程没有 pty_id，无法直接复用；但纯函数 `scan_ssh_prompt` / `strip_ansi_codes`（`pty.rs:267`）可复用。
- `prepare_ssh_key`（`ssh.rs:60`）可原样复用。
- 项目模型 `ProjectConfig`（`config.rs` / `types.ts:47`）= id/name/path/...，每个项目有 `path`（目录）。SSH 连接目前是全局的，不与项目绑定。

### 现有 hook 系统是「mini-term 写 agent 配置」的现成先例（`hook_registry.rs`）

- `miniterm-hook`（`bin/miniterm-hook.rs`）是极简 bin（仅 serde_json + std），证明项目能出独立瘦 bin。
- Claude hook 写 `~/.claude/settings.json`（全局）；Codex hook 写 `~/.codex/hooks.json` + `~/.codex/config.toml` `[features]`。
- `hook_registry.rs` 已有：JSON 合并写回 + marker 幂等更新（`HOOK_MARKER`）、`toml_edit` 改 `config.toml`、`get_hook_binary_path()` 解析同目录兄弟 bin。
- 现有 hook 注册是**全局**的；本任务要求 SSH MCP **按项目**——这是关键差异点。

### 已定决策（用户拍板）

- 拓扑 A1：独立 stdio sidecar bin（新增 `[[bin]]`，对标 `miniterm-hook`）。
- 认证：复用 `portable-pty` + `scan_ssh_prompt` 做 PTY autofill 喂密码。
- 接受 agent 的 SSH 操作 headless（不在 mini-term UI 可见）。
- 目标 agent：Claude Code + Codex 都要。
- SSH MCP 按项目隔离启用，启用时才写入对应 MCP 注册配置。

## Assumptions (temporary)

- sidecar 用 Rust + 官方 `rmcp` crate 实现 stdio MCP。
- Claude Code 用项目根的 `.mcp.json` 实现天然的按项目隔离。
- Codex（v0.130.0+）支持项目级 `<project>/.codex/config.toml` 的 `[mcp_servers]`，与 Claude `.mcp.json` 对称，两端按项目隔离都干净；须用 `toml_edit` 自己写（`codex mcp add` 只写全局）。
- v1 工具面：`ssh_list_connections` + `ssh_exec`；不含 upload/download、不含连接管理写操作。
- sidecar 直接读全局 `config.json` 的 `sshConnections`。

## Open Questions

全部已解决：
- Codex 项目级 MCP → 支持 `<project>/.codex/config.toml`。
- 配置写入策略 → 全自动（写配置 + Codex `trust_level=trusted` + Claude `enableAllProjectMcpServers`）。
- 启用入口 → 项目列表右键菜单。
- 暴露范围 → 逐连接 `agentAccessible` 标记，默认关。
- `.mcp.json` 自动加 `.gitignore` → 是（marker 幂等合并，不覆盖既有内容）。

## Requirements

- 新增 stdio MCP sidecar bin `mt-ssh-mcp`（`rmcp` 1.7.0），暴露 SSH 工具给 agent。
- 工具：`ssh_list_connections`（仅返回 `agentAccessible=true` 的连接，且不含密码字段）、`ssh_exec`（入参 connection + command + 可选 timeout/cwd，返回 stdout/stderr/exitCode，输出封顶 + 超时强制 kill）。
- 认证：密码（PTY autofill，复用 `scan_ssh_prompt`）、私钥（复用 `prepare_ssh_key` 临时副本）、proxy_jump（`ssh -J`）；host key 用 `-o StrictHostKeyChecking=accept-new`。
- `SshConnection` 加 `agentAccessible: bool`（默认 false），SshModal 表单加勾选框；只有勾选的连接对 agent 可见。
- 启用入口：项目列表右键菜单加「启用 / 停用 SSH MCP」开关（按项目持久化，`ProjectConfig` 加字段）。
- 启用时全自动为该项目写入 Claude `.mcp.json` + Codex `<project>/.codex/config.toml`，并自动写 Codex `trust_level=trusted`、设 Claude `enableAllProjectMcpServers`，并把 `.mcp.json` 加入项目 `.gitignore`；停用时移除。写入用 marker 幂等合并（复用 `hook_registry` 模式），不覆盖既有内容。
- 启用 / 停用后在 UI 提示「需重启该项目的 agent 会话才生效」。
- `ssh_exec` 写审计日志（每次执行记一行：时间 / 连接 / 命令 / 退出码）。

## Acceptance Criteria

- [ ] `ssh_list_connections` 只返回 `agentAccessible=true` 的连接，且不含 password 字段。
- [ ] agent 能通过 `ssh_exec` 在远程主机跑命令并拿到 stdout/exitCode；输出过大被封顶并标记。
- [ ] 密码型连接经 PTY autofill 成功认证；私钥型连接复用临时副本成功认证；proxy_jump 连接可用。
- [ ] 未启用 SSH MCP 的项目不写入任何 `.mcp.json` / `.codex/config.toml`。
- [ ] 启用/停用幂等，不破坏用户既有的 `.mcp.json` / `config.toml` 内容；停用后干净移除本功能写入的条目。
- [ ] `ssh_list` 及任何错误信息均不泄漏明文密码。
- [ ] 每次 `ssh_exec` 有审计日志记录。

## Definition of Done (team quality bar)

- Rust 单元测试覆盖关键纯函数（命令拼接、配置合并、输出封顶）。
- `cargo build` / 前端 typecheck 通过。
- 行为变化在回答中说明（按全局 CLAUDE.md，不写额外文档）。
- 风险点（密码处理、配置文件写入）有回滚考虑。

## Out of Scope (explicit)

- 文件传输（upload/download）—— 留 v2。
- agent 侧管理连接（增删改）—— 连接仍在 SshModal 管。
- 有状态 / 持久远程 shell 会话、连接池 —— 留 v2。
- agent SSH 操作在 mini-term UI 可见 —— 已决定接受 headless。
- in-app HTTP MCP（A2）/ 混合（A3）拓扑。

## Technical Notes

- 复用：`prepare_ssh_key`（`ssh.rs:60`）、`scan_ssh_prompt` / `strip_ansi_codes`（`pty.rs:267`）、`hook_registry.rs` 的配置合并 / marker 模式、`get_hook_binary_path()`、`toml_edit`、`portable-pty`、`dirs`。
- 建议抽 `tauri`-free 的 core crate，避免 sidecar 链接整个 Tauri。
- sidecar 无 `AppHandle`，需自行用 `dirs` 拼 `config.json` 路径（对标 `miniterm-hook.rs:114` `get_port_file_path`）。
- stdio 铁律：sidecar stdout 只输出 MCP 协议 JSON，日志走 stderr。
- 安全：明文密码不出 sidecar；建议 `SshConnection` 加 `agentAccessible` 逐连接开关；`ssh_exec` 等于把 LLM 接到所有服务器的 RCE，考虑审计日志。

## Research References

- [`research/codex-mcp-config.md`](research/codex-mcp-config.md) — Codex v0.130.0 支持项目级 `<project>/.codex/config.toml` 的 `[mcp_servers]`；须 `toml_edit` 自己写；未 trust 的项目其 `.codex/config.toml` 被静默忽略。
- [`research/claude-mcp-json.md`](research/claude-mcp-json.md) — Claude Code 用项目根 `.mcp.json`（`mcpServers` 对象）按项目隔离；首次用须一次性审批；运行中会话不热加载，须重启。
- [`research/rust-mcp-sdk.md`](research/rust-mcp-sdk.md) — 用官方 `rmcp` 1.7.0 实现 stdio MCP server，`#[tool]` 宏低样板；依赖 tokio + edition 2024（Rust ≥ 1.85），sidecar ~2-3MB。

## Research Notes — 研究后新增的约束与决策点

- **幂等合并**：写 `.mcp.json` / `.codex/config.toml` 须 marker 幂等合并（复用 `hook_registry.rs` 模式），不能整文件覆盖——项目可能已有团队共享配置。
- **重启生效**：启用 SSH MCP 后，已运行的 agent 会话须重启才加载新 server，UI 需提示。
- **Codex trust 门控**：Codex 项目级配置要求该项目在 `~/.codex/config.toml` 标 `trust_level = "trusted"`，否则静默失效。
- **Claude 首次审批**：Claude 首次用项目 `.mcp.json` 会弹审批，可用用户级 `enableAllProjectMcpServers` 免弹窗。
- **工具链**：rmcp 自身是 edition 2024，要求 Rust 工具链 ≥ 1.85；我方代码可继续用 edition 2021（依赖的 edition 不影响本 crate），只需确保本机 rustc 够新。

## Technical Approach

- **crate 结构**：抽一个 `tauri`-free 的 `mt-core` 库 crate（路径依赖即可，不强制 workspace），放共享逻辑：`SshConnection` 类型、`config.json` 路径解析与读取、ssh 命令拼接、`scan_ssh_prompt` / `strip_ansi`、key prep。`tauri_app_lib` 与新 bin 都依赖它。
- **sidecar**：新增 `[[bin]] mt-ssh-mcp`（对标 `miniterm-hook`），用 `rmcp` 跑 stdio MCP server，依赖 `mt-core` + `portable-pty` + `tokio`，不链接 `tauri`。stdout 仅协议、日志走 stderr。
- **认证**：`ssh_exec` 自己用 `portable-pty` 起 ssh 子进程，复用 `scan_ssh_prompt` 做 autofill 喂密码（不依赖 mini-term 的 pty_id）。
- **项目启用**：新增 Tauri 命令 `enable_ssh_mcp(project)` / `disable_ssh_mcp(project)`，逻辑对标 `hook_registry.rs`（读-改-写 + marker 幂等）。
- **暴露控制**：sidecar 读全局 `config.json`，按 `agentAccessible` 过滤；项目「启用」只决定是否注册 MCP，连接可见性完全由 `agentAccessible` 决定。

## Decision (ADR-lite)

**Context**：要让 mini-term 终端里的 AI agent 用上已存 SSH 连接；agent 是独立进程，够不到 Tauri 命令。
**Decision**：A1 拓扑（独立 stdio sidecar bin + rmcp）；PTY autofill 认证；逐连接 `agentAccessible` 控暴露；按项目右键菜单启用，启用时全自动写双 agent 配置并自动提权。
**Consequences**：agent 的 SSH 操作 headless 不可见；启用后 agent 会话需重启；mini-term 替用户跳过了 Codex trust / Claude 审批确认（用户已知情同意）；明文密码留在 `config.json` 但不出 sidecar。v2 再做文件传输、连接池、UI 可见性。

## Implementation Plan（小步 PR）

- **PR1**：抽 `mt-core` crate；`tauri_app_lib` 改依赖它且行为不变；`SshConnection` 加 `agentAccessible` + SshModal 勾选框 + 前后端类型同步。单测：命令拼接、配置读取。
- **PR2**：`mt-ssh-mcp` bin —— rmcp stdio server，`ssh_list_connections` + `ssh_exec`（PTY autofill / 私钥 / proxy_jump / accept-new / 超时 / 输出封顶）+ 审计日志。单测：输出封顶、连接过滤。
- **PR3**：项目右键菜单启用 / 停用；Tauri 命令写 / 删 `.mcp.json` + `.codex/config.toml` + trust + enableAllProjectMcpServers + `.gitignore`（marker 幂等）；`ProjectConfig` 加启用状态字段；重启提示。
