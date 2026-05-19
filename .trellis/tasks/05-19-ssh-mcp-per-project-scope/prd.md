# SSH MCP 改为按项目关联授权

## Goal

把 SSH MCP 的连接暴露控制从「连接级 `agentAccessible` 开关」改为「按项目关联授权」：
所有 SSH 连接默认都能被 MCP 工具访问；具体哪个项目的 agent 能看到哪些连接，由项目右键菜单的「关联 SSH」按项目设定。

## What I already know（基于 23358b9）

- `SshConnection`（`mt-core/src/ssh_connection.rs` / `types.ts`）有 `agentAccessible: bool`。
- sidecar `mt-ssh-mcp.rs` 全局读 `config.json`，按 `agent_accessible` 过滤连接；server 无项目上下文。
- `enable_ssh_mcp`/`disable_ssh_mcp`（`ssh_mcp_registry.rs`）按项目写 `.mcp.json` / `.codex/config.toml`，注册项 `args` 为空。
- 右键菜单（`ProjectList.tsx`）有「启用/停用 SSH MCP」单项，`ProjectConfig.sshMcpEnabled` 持久化。
- sidecar 通过 `mt_core::read_ssh_connections()` 读全部连接。

## Decision（用户已拍板）

- 「关联 SSH」**合并为唯一入口**：右键菜单移除「启用/停用 SSH MCP」，新增「关联 SSH…」打开弹窗。
  弹窗勾选连接 = 启用 MCP 并限定可见范围；全部取消 = 停用 MCP。
- 默认范围 = 全部连接（`sshConnectionIds` 未设置即代表全部，含将来新增）。
- sidecar 通过 `--project-id <id>` CLI 参数获知所属项目，运行时按 `config.json` 里该项目的
  `sshConnectionIds` 过滤连接。范围改动即时生效（无需重启会话）；启用/停用仍需重启会话。

## Requirements

- 移除 `SshConnection.agentAccessible`（前后端类型 + SshModal 表单勾选框 + SshRow 标签）。
- `ProjectConfig` 新增 `sshConnectionIds?: string[]`：项目关联的连接 id 列表;`undefined` = 全部。
- `mt-core`：`config_reader` 增读 `projects[].sshConnectionIds`;新增按项目过滤的读取函数（纯函数可单测）。
- `mt-ssh-mcp`：解析 `--project-id`;`ssh_list_connections` / `ssh_exec` 按项目范围过滤;去掉 `agent_accessible` 相关逻辑。
- `enable_ssh_mcp` 接收 project id 并写进 `.mcp.json` / Codex 的 `args`（`--project-id <id>`）。
- 右键菜单：移除「启用/停用 SSH MCP」，新增「关联 SSH…」弹窗（连接清单 + 复选框，按 group 归类）。

## Acceptance Criteria

- [ ] SshModal 表单与连接行不再出现 agent 可见性相关 UI。
- [ ] 项目右键菜单「关联 SSH…」弹窗可勾选连接;勾选≥1 启用 MCP,全不勾停用。
- [ ] sidecar 收到 `--project-id` 后只暴露该项目关联的连接;未关联的连接 `ssh_exec` 不可达。
- [ ] 项目未设 `sshConnectionIds` 时，sidecar 暴露全部连接（默认全部）。
- [ ] 启用/停用幂等，不破坏既有 `.mcp.json` / `config.toml`（沿用 marker 模式）。
- [ ] Rust 单测覆盖新过滤纯函数;`cargo build` 与前端 typecheck 通过。

## Out of Scope

- 连接级以外的新权限维度（命令白名单等）。
- 已运行 agent 会话的 MCP 热加载。

## Technical Notes

- 遵循 `.trellis/spec/backend/agent-config-injection.md`：读-改-写 + marker 幂等。
- sidecar stdout 仅 MCP 协议;CLI 解析失败不 panic，退化为「无项目范围 = 全部可见」。
- `SshMcp` handler struct 需持有 `project_id: Option<String>`。
