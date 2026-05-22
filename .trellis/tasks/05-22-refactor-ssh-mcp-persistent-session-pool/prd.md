# 重构 mt-ssh-mcp 为持久 SSH 会话池

## Goal

把 `mt-ssh-mcp` sidecar 内部 `ssh_exec` 工具的"每次 spawn 一个 `ssh user@host <cmd>` 子进程"模型，重构为"基于 Rust SSH 协议库（russh / ssh2）在 sidecar 进程内维护一个 `connection_id → SSH Session` 缓存池"模型。

**目的**：
1. **性能**：消除每次工具调用 1–3 秒的 TCP+SSH 握手 + 密码认证开销。Agent 连续多次 `ssh_exec` 时第二次开始走纯 RTT。
2. **稳定性**：彻底绕开当前密码 PTY autofill 路径（不存在密码提示扫描，所以 OSC strip 漏命中 → ssh 等输入 → 60s 超时 + join 死锁 这条故障链整体消失）。
3. **跨平台一致**：当前 PTY autofill 与 Windows ConPTY 强绑定，换库后无平台差异分支。

## What I already know

### 当前实现（已盘点）

- **入口**：`src-tauri/mt-sidecars/src/bin/mt-ssh-mcp.rs` `ssh_exec` 工具（line 654 起）。
- **执行路径**：`tokio::task::spawn_blocking` 内分两路：
  - 密码型 → `run_ssh_pty(args, password, timeout)`：`portable-pty` 起 `ssh user@host <cmd>` + PTY autofill 喂密码。
  - 非密码型（密钥 / agent）→ `run_ssh_piped(args, timeout)`：`std::process::Command` 起 ssh + 管道收集 stdout/stderr。
- **状态**：`SshMcp` 结构体只持有 `project_id`，**无任何会话缓存**。
- **每次调用都拼新的 `ssh_args`**：`build_ssh_args(conn, identity_path, remote_command, batch_mode)`（line 189）—— 包含 `-p`、`-i`、`-J`、`-o StrictHostKeyChecking=accept-new`、`-o BatchMode=yes`（仅非密码型）。
- **入参**：`SshExecArgs { connection, command, timeout_secs, cwd }`。`cwd` 通过 `cd <dir> && cmd` 拼到远程命令里（line 170）。

### `SshConnection` 数据形状（`mt-core/src/ssh_connection.rs`）

```rust
pub struct SshConnection {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,                            // 22 默认
    pub user: String,
    pub password: Option<String>,
    pub identity_file: Option<String>,        // 私钥路径
    pub proxy_jump: Option<String>,           // "user@bastion" 字符串
    pub group: Option<String>,
}
```

**没有的字段**（值得注意）：
- 私钥 passphrase（当前实现假定密钥无 passphrase，或依赖 ssh-agent）
- ssh-agent 显式开关（当前由 ssh 客户端自己处理）
- keyboard-interactive 偏好

### 当前 crate 依赖（`mt-sidecars/Cargo.toml`）

```toml
[dependencies]
mt-core = { path = "../mt-core" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
dirs = "6"
portable-pty = "0.8"        # 重构后 mt-ssh-mcp 不再需要,但 miniterm-hook 仍依赖,保留
rmcp = { version = "1", features = ["server", "macros", "transport-io"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "io-std"] }
```

### 现有非功能特性（必须保留）

- 超时强制 kill（默认 60s，可入参覆盖）
- 输出封顶（stdout/stderr 各 100 KB）
- 审计日志（`{config_dir}/ssh-mcp-audit.log`）
- 按项目过滤连接可见性（`--project-id` + `read_ssh_connections_for_project`）
- 错误信息不泄漏密码
- `ssh_list_connections` 返回视图不含 `password` / `identityFile`

## Requirements (evolving)

### 功能

- [ ] sidecar 进程内维护 `HashMap<connection_id, CachedSession>`，第一次 `ssh_exec(<connection>, ...)` 建立 SSH session，后续复用。
- [ ] `ssh_exec` 工具入参 / 出参 schema 100% 兼容现状（`connection`、`command`、`timeout_secs`、`cwd` → `stdout`、`stderr`、`exitCode`、`truncated`、`timedOut`）。
- [ ] `ssh_list_connections` 不变。
- [ ] 支持认证方式（**与当前能力对齐**，不扩展）：
  - 密码（`SshConnection.password`）—— 同时尝试 `password` 与 `keyboard-interactive` 认证 method（部分服务器只接后者）。
  - 私钥（`SshConnection.identity_file`，**仅支持无 passphrase 的密钥**；如果密钥需要 passphrase，报清晰错误指引用户用无 passphrase 密钥）。
  - 同一连接可同时配置 password 与 identity_file，按"先 publickey 后 password"顺序尝试。
  - **不实现** ssh-agent（保持库默认行为，不主动读 SSH_AUTH_SOCK）。
- [ ] **跳板机功能整体移除**：
  - sidecar 收到 `proxy_jump != None` 的连接时，`ssh_exec` 立即返回明确错误（"jump host is no longer supported by ssh_exec; remove proxy_jump on this connection"）。
  - 配套清理：主程序 SshModal 的 jump 输入框、`SshConnection.proxy_jump` 字段、`build_ssh_args` 的 `-J` 逻辑（这些不在本任务范围，**列入 follow-up**）。
- [ ] host-key 策略：首次 accept-new、变更拒绝（语义与当前 `-o StrictHostKeyChecking=accept-new` 一致），写入 `~/.ssh/known_hosts`。

### 非功能

- [ ] 单次 `ssh_exec` 超时强制中止 channel（不中止整个 session）。
- [ ] 输出封顶 100 KB / 流不变。
- [ ] 审计日志格式不变。
- [ ] 错误信息不泄漏明文密码。
- [ ] Windows / macOS / Linux 三平台 sidecar 都能编译并工作。

### 会话池策略（已确定）

- [ ] **空闲淘汰**：10 分钟没有 `ssh_exec` 即关闭 + 移出 map（HikariCP `idleTimeout=600_000` 风格）。
- [ ] **最长生命周期**：2 小时硬回收（防 NAT 静默丢链）。
- [ ] **Keepalive**：russh `Config { keepalive_interval: Some(30s), keepalive_max: 3 }`，90s 探不到判死。
- [ ] **重连**：lazy 检测 + 单次 retry（覆盖 acquire 与 use 之间的死链 race）+ 失败后 `unhealthy_until = now + 30s` cooldown。
- [ ] **并发**：单 session `tokio::sync::Mutex` 串行 channel 操作；跨 session 自由并行。
- [ ] **池上限**：8 个 session，LRU 淘汰（用 `last_used` 计算）。
- [ ] **后台 reaper**：每 60s tick 一遍扫描 idle + max-lifetime。
- [ ] **shutdown 钩子**：`service.waiting().await?` 返回后调 `pool.shutdown()` → 对每个 session 跑 `disconnect(Disconnect::ByApplication, "", "en")` + 2s 单 session 超时兜底。`Drop for SshPool` 作为 backstop（只 abort reaper，async disconnect 在 Drop 里没法做）。

### 配置一致性

- [ ] **连接信息冻结**：sidecar 启动时读一次 `config.json`，之后用户在主程序里改了密码/主机/私钥**不实时生效**。变更后用户需要重启 agent 终端（与 `.mcp.json` 改了要重启 agent 一样的语义）。`session_pool` 内部用启动时快照的连接信息建立 session，不重读。
- [ ] **host-key 变更**：保持 ssh 默认行为，known_hosts mismatch → 拒绝连接、返回明确错误。agent 拿到错误后可指引用户去清 `~/.ssh/known_hosts` 对应行。

## Open Questions

✅ 全部 closed（见下方 Decision）。

## Acceptance Criteria (evolving)

- [ ] 同一 `connection` 连续 5 次 `ssh_exec("echo hi")`：第一次 < 3s（首连接），第 2–5 次 < 200ms（纯 RTT + exec channel）。
- [ ] 密码型连接连续多次调用不再卡死（当前 bug 自然消失）。
- [ ] `proxy_jump != None` 的连接被拒，错误信息明确指出 jump 功能已移除。
- [ ] 私钥（无 passphrase）连接能成功执行命令。passphrase 密钥报清晰错误。
- [ ] sidecar 进程 stdin EOF 时所有会话 graceful disconnect，不留 dangling fd / 远程 zombie session。
- [ ] session 因网络原因断开后，下一次 `ssh_exec` 自动重连成功（不报永久错误）。
- [ ] Windows / macOS / Linux 三平台 release 构建通过、sidecar 二进制都能跑。
- [ ] 既有 `mt-ssh-mcp` 单测全部通过（schema、find_connection、build_remote_command、cap_output、format_audit_line 等纯函数测试）。
- [ ] 新增 session pool 的单测：缓存命中 / miss、过期淘汰、断连重连、shutdown 释放。

## Definition of Done

- 三平台 release 构建通过（GitHub Actions 现有 workflow）。
- 单测全绿，关键路径补集成测试（最好对 localhost sshd 做端到端，可选）。
- README 中"SSH MCP"章节更新（如果有行为差异需要说明）。
- 配置文件 schema 若扩展（如新增 passphrase 字段），主程序 SshModal 同步更新。
- 旧的 `run_ssh_pty` / `run_ssh_piped` / `strip_recent_text` / `mt_core::scan_ssh_prompt` 等"PTY autofill"相关代码评估是否删除（主程序的 PTY autofill 仍在使用，不能删 mt-core 里的部分）。

## Out of Scope (explicit, evolving)

- 主程序内嵌终端的 SSH 体验改造（那条路径走 PTY，是用户交互式终端，与本 sidecar 无关）。
- SSH tunneling / port forwarding 工具暴露给 agent（仅保留 `ssh_exec`）。
- 文件传输（sftp、scp）工具——可后续单独任务。
- 改 `ssh_exec` 的 MCP schema（保持入参 / 出参完全兼容）。
- **密钥 passphrase 支持**（明确不做，遇到 passphrase 密钥报错指引用户用无 passphrase 密钥或转 ssh-agent —— 但 ssh-agent 也不在本任务范围）。
- **ssh-agent 集成**（保持库默认行为，不主动读 SSH_AUTH_SOCK）。
- **跳板机 proxy_jump 功能**（彻底移除，sidecar 直接拒绝）。
- 主程序 SshModal 移除 jump 输入框、`SshConnection.proxy_jump` 字段、`build_ssh_args` 的 `-J` 逻辑——这些算 **follow-up 任务**，不在本任务 PR 里做（避免改动面过大）。

## Technical Approach

### 库与依赖

- 新增依赖：`russh = "0.61"`、`russh-keys`（OpenSSH 格式密钥读取）。`mt-sidecars/Cargo.toml` 加，不动 `mt-core`。
- 删除：`portable-pty` 在 `mt-ssh-mcp.rs` 路径不再使用（**保留在 Cargo.toml**，因为 `miniterm-hook` 还在用它）。

### 文件改动 / 新增

| 文件 | 操作 |
|---|---|
| `src-tauri/mt-sidecars/src/bin/mt-ssh-mcp.rs` | 主改：`ssh_exec` 工具改走 `SshPool::acquire`；删除 `run_ssh_pty` / `run_ssh_piped` / `strip_recent_text`；保留 `build_remote_command` / `cap_output` / `format_audit_line` / `find_connection` / `connection_views` / `parse_project_id` 等纯函数 |
| `src-tauri/mt-sidecars/src/bin/mt_ssh_mcp/pool.rs`（**新增**） | `SshPool` / `CachedSession` / `MtClient` (`Handler` 实现) / `PoolConfig`。本任务核心 |
| `src-tauri/mt-sidecars/Cargo.toml` | 加 russh 依赖 |
| `src-tauri/mt-core/src/ssh_prompt.rs` | **不动**。仍被主程序的 PTY autofill 使用 |
| `src-tauri/mt-core/src/ssh_key.rs` | **评估**：`sanitize_key_permissions` 是否仅供 sidecar 用？若是则删；若主程序也用则保留 |

### `ssh_exec` 工具新执行链

```
ssh_exec(connection, command, timeout_secs, cwd)
  → find_connection(read_ssh_connections_for_project, ...)           // lookup 仍每次重读 config.json
  → proxy_jump != None? → 返回明确错误 "jump host removed"
  → pool.acquire(snapshot)                                            // lazy 建/复用
       └ check is_closed → rebuild;否则直接 reuse
  → session.lock().await.channel_open_session().await
  → channel.exec(true, "cd <cwd> && <command>" or "<command>").await
  → 收集 stdout / stderr / exit_code,各自 cap_output 100KB
  → audit_log
  → 序列化 SshExecResult
```

### 主机密钥（known_hosts）

- 在 `MtClient::check_server_key` 回调内同步比对：
  - 首见 → 写入 `~/.ssh/known_hosts` 并返 `Ok(true)`（accept-new 等价）。
  - 已存且匹配 → `Ok(true)`。
  - 已存但 mismatch → 返 `Ok(false)` 拒绝；russh 会把握手 fail 反给 caller。错误信息透传给 agent。
- `known_hosts` 路径：`dirs::home_dir() / ".ssh" / "known_hosts"`。

### 认证 fallback 顺序（单连接）

1. 若 `identity_file` 非空：尝试 `authenticate_publickey`。密钥需 passphrase 时报清晰错误退出（不进 password 兜底，避免误以为是网络问题）。
2. 若 publickey 失败 / 未配且 `password` 非空：尝试 `authenticate_password`，若服务器只接 `keyboard-interactive` method，回退 `authenticate_keyboard_interactive_*` 直接喂密码。
3. 两者皆败 → 返回 auth 失败错误，session 不入池。

### 超时（per-call）

- `timeout_secs` 仍是入参，默认 60s。
- 实现：`tokio::time::timeout(duration, channel_exec_loop)`。超时 → 关闭 channel（不断 session），返回 `timedOut: true` + 当前已收集的 stdout/stderr。

### 配置一致性

- `read_ssh_connections_for_project(...)` 仍在每次 `ssh_exec` 入口调用（保持现有"新增连接立即可见"行为）。
- 但一旦某 `connection_id` 第一次建池 session，**该 session 用建立时的 `SshConnection` 快照绑死**——后续此 session 不重读 password / identity_file。语义：用户改密码后，旧 session 沿用旧密码继续工作直到自然淘汰或 agent 重启。

## Decision (ADR-lite)

**Context**: `mt-ssh-mcp` sidecar 当前每次 `ssh_exec` 都 spawn 新 ssh 子进程，每次 1-3s TCP+SSH 握手 + 密码 PTY autofill。密码 autofill 路径在 Windows 上有 OSC strip 漏命中 → 60s 超时 → join 死锁的故障链（独立任务 `fix-ssh-mcp-pty-join-deadlock` 在修）。

**Decision**: 引入 `russh 0.61`，在 sidecar 进程内维护按 `connection_id` 缓存的 SSH session 池。第一次调用某连接时建 session，后续 `ssh_exec` 复用同一 session 开 exec channel。MVP 范围：密码 + 无 passphrase 私钥（与当前能力对齐）；跳板机移除；连接信息 sidecar 启动后冻结；host-key 保持 accept-new 语义。

池参数：idle 10min / lifetime 2h / keepalive 30s×3 / cap 8 LRU / lazy 重连 + 单次 retry + 30s cooldown / 单 session `tokio::sync::Mutex`。

**Consequences**:
- ✅ 连续 `ssh_exec` 第 2 次起从「秒级」降到「纯 RTT」。
- ✅ 彻底绕开 PTY autofill 路径——`fix-ssh-mcp-pty-join-deadlock` 任务里加的两层修复（OSC strip + join timeout）依然要合并（主程序终端仍用 PTY autofill），但 sidecar 不再受影响。
- ✅ 删除约 200 行 `run_ssh_pty` / `run_ssh_piped` / `strip_recent_text` 代码 + `BatchMode=yes` 那条 gotcha 整段失效。
- ⚠️ 引入 russh 依赖：sidecar 体积 + 几 MB（pure Rust 静态链接），三平台都过编译。
- ⚠️ 主机密钥校验从「ssh 客户端委托」改为「自己实现 known_hosts 解析」——可能与主程序的 PTY 路径有行为微差（主程序仍走 ssh 客户端）。
- ⚠️ `SshConnection.proxy_jump` 字段在 sidecar 入口被拒绝；UI 层的清理算 **follow-up 任务**（不在本任务 PR 内）。
- ⚠️ 密钥 passphrase 不支持，遇到加密密钥需用户用无 passphrase 版或转 ssh-agent（也不在 MVP）。

## Implementation Plan (small PRs)

按"骨架先立、行为再补、清理收尾"切，每 PR 都可独立合并、可独立 review。

### PR1: 加 russh 依赖 + 池骨架 + 主机密钥校验

- `Cargo.toml` 加 `russh`、`russh-keys`。
- 新建 `src-tauri/mt-sidecars/src/bin/mt_ssh_mcp/pool.rs`：`SshPool` 结构体 + `PoolConfig`（默认值硬编码） + `MtClient` Handler。
- 实现 `acquire(snapshot) -> Result<Arc<Mutex<CachedSession>>>`：lazy 建 session，认证 password / publickey 双路径，known_hosts accept-new 语义。
- 单测：`PoolConfig` 默认值、known_hosts 解析 + 比对、LRU 淘汰算法。
- **不动 `ssh_exec` 工具入口**，旧 `run_ssh_pty/piped` 继续工作。

### PR2: 切换 `ssh_exec` 走池 + 单次 retry + 审计

- 改 `ssh_exec` 内部：找连接 → 校验 `proxy_jump == None` → `pool.acquire` → 开 channel → exec → 收集输出 → cap_output → 审计 → 序列化。
- 超时实现：`tokio::time::timeout` 包 channel exec loop。
- 单次 auto-retry：第一次 channel_open 或 exec 出错 → 移除池 entry → 重建 session → 再试一次 → 仍失败 → set unhealthy_until → 返错。
- 删除 `run_ssh_pty` / `run_ssh_piped` / `strip_recent_text`（在本 crate 内部、未导出）。
- 单测：`ssh_exec` 工具结果序列化、proxy_jump 拒绝错误信息、cap_output 不变。
- **手测**：连续 5 次 `ssh echo hi` 时延、密码连接、私钥连接、kill server 后重连。

### PR3: 池后台 reaper + shutdown 钩子 + 清理 + spec 更新

- 在 `SshPool::new` 内 `tokio::spawn` reaper（60s tick）。
- `main()` 中 `service.waiting().await?` 之后 `pool.shutdown().await`。
- `Drop for SshPool` 作为 reaper abort 的 backstop。
- `Cargo.toml` 检查：若 `portable-pty` 仅 `miniterm-hook` 用，加注释说明。
- `.trellis/spec/backend/index.md` 更新：移除"`BatchMode=yes` 禁掉密码认证"那条 gotcha，标注仅对**主程序 PTY autofill 路径**还有效。
- README 在 SSH MCP 章节加一句"sidecar 维护进程内 session 池，首次调用 ~秒级，后续纯 RTT"。
- 评估并删除 `mt_core::ssh_key::sanitize_key_permissions`（若确认仅 sidecar 用）。

### 不在本任务的 follow-up（独立 PR）

- 主程序 SshModal 移除 jump host 输入框、`SshConnection.proxy_jump` 字段、`build_ssh_args` 的 `-J` 逻辑。
- `ssh_disconnect` MCP 工具（用户主动让 agent 断 + 重连，用于改密码后立即生效）。
- `passphrase` 字段扩展支持加密密钥。

## Research References

- [`research/russh-vs-ssh2.md`](research/russh-vs-ssh2.md) — russh 胜出（async 原生 + ProxyJump 免费 + 维护活跃），ssh2 Windows 不需要 OpenSSL（之前判断错误）
- [`research/session-pool-patterns.md`](research/session-pool-patterns.md) — HikariCP / paramiko / autossh / OpenSSH 综合，给出 10min/2h/30s×3/cap=8/lazy+1retry+30s cooldown 的推荐 profile + 8 项失效模式矩阵

## Technical Notes

- 重构主要影响 `src-tauri/mt-sidecars/src/bin/mt-ssh-mcp.rs`；可能新增一个 `session_pool.rs` 子模块。
- `mt-core` 不应被改成依赖 SSH 库（保持 tauri-free + minimal-dep 的约定，sidecar 是 ssh 实现的唯一持有者）。
- `mt_core::prepare_ssh_key`（收紧权限的临时私钥副本）在新方案下可能不再需要——库直接读密钥 bytes，不经过 ssh 客户端的权限校验。是否可删待定。
- `mt_core::scan_ssh_prompt` / `strip_ansi_codes` 仍被主程序 PTY autofill 使用，**不能删**。
- 必须明确：构建产物体积变化。pure-Rust russh 静态链接进 sidecar 大概率涨几 MB。
