# Spec：SSH 工具从 MCP 迁移到 CLI + Skill v1

> 状态：draft · 2026-07-31（rev2：CLI 改为守护进程架构，保留连接池复用；rev3：PR1–PR3 已落地，实测结论与剩余手动矩阵见 §9/§11，PR4 后标 final）
> 前置研究结论：Codex CLI 自 2025-12 起原生支持 skills（项目级 `.codex/skills/`），SKILL.md 已是跨 agent 标准格式，单一方案可同时覆盖 Claude Code 与 Codex。

## Problem Statement

现有 SSH MCP（`mt-ssh-mcp` sidecar）能用，但体验有四处硬伤：

1. **WSL 内完全不可用**：`.mcp.json` 里写的是 Windows 绝对路径，WSL 里的 Claude Code 无法把它当 MCP server 拉起来——这是搁置已久的待办；
2. **注册面过宽**：启用一个项目要动 5 个文件（`.mcp.json`、项目 `.codex/config.toml`、`~/.codex/config.toml` 信任、`~/.claude/settings.json` 免审批、`.gitignore`），停用还只能清理一半；
3. **上下文与进程开销**：4 个 MCP 工具 schema 常驻 agent 上下文；每个 agent 会话挂一个常驻 sidecar 进程；
4. **输出契约对 agent 不自然**：远程 stdout/stderr/exit code 被封装进 JSON 信封、100KB 封顶截断，agent 无法管道组合、无法用退出码做 `&&` 链式判断。

## Solution 概览

新增 `mt-ssh-cli` sidecar 二进制（复用 `mt-ssh` crate 的连接与 SFTP 代码），agent 通过 Bash 直接调用；注册方式从「写 MCP 配置」改为「生成两份 SKILL.md」。

CLI 采用 **daemon 架构**：同一个二进制的 `--daemon` 模式作为全局唯一守护进程持有 `SshPool`（对标 MCP sidecar 的常驻池），CLI 调用经本机 IPC 转发给 daemon，避免每次命令重新 SSH 握手。daemon 由 CLI 按需自拉起、空闲自退，对 agent 完全透明。

| 维度 | 旧（MCP） | 新（CLI + Skill） |
|------|-----------|-------------------|
| agent 调用方式 | MCP 工具（JSON 信封） | Bash 调 CLI（原生 stdout/stderr/exit code） |
| 注册产物 | `.mcp.json` + 项目/全局 `.codex/config.toml` + `~/.claude/settings.json` | `.claude/skills/mini-term-ssh/SKILL.md` + `.codex/skills/mini-term-ssh/SKILL.md` |
| WSL | 不可用 | 可用（Windows exe 经 interop 执行，config.json 与 named pipe 都在 Windows 侧） |
| 常驻进程 | 每个 agent 会话一个 sidecar，各持独立池 | 全机一个 daemon，池全局共享，空闲自退 |
| 连接复用 | sidecar 进程内 SshPool | daemon 进程内 SshPool（同一套 acquire / evict+retry / cooldown 编排） |
| 审计 / 护栏 | `ssh-mcp-audit.log` + config.json 传输硬拒绝 | 原样继承（daemon 单点写日志，顺带消除多进程并发追加交错） |
| project 范围过滤 | `--project-id` 写在 `.mcp.json` args（进程级） | `--project-id` 写死在 SKILL.md 命令示例里（请求级） |

`sshMcpEnabled` 配置字段名保留（存量配置兼容），语义变为「启用 SSH 工具（CLI + Skill）」。

## 设计

### 1. `mt-ssh-cli` 二进制（agent 可见面）

位置：`src-tauri/mt-sidecars/src/bin/mt-ssh-cli.rs`，新 `[[bin]]`。参数解析用 clap v4 derive（sidecar 独立 crate，不影响主程序编译；clap 的用法错误信息对 agent 可读性好，`--help` 即文档）。

#### 子命令

```
mt-ssh-cli list [--project-id <ID>] [--json]
mt-ssh-cli exec [--project-id <ID>] [--cwd <DIR>] [--timeout <SECS>] <CONNECTION> [--] <COMMAND>...
mt-ssh-cli upload   [--project-id <ID>] [--timeout <SECS>] <CONNECTION> <LOCAL_PATH> <REMOTE_PATH>
mt-ssh-cli download [--project-id <ID>] [--timeout <SECS>] <CONNECTION> <REMOTE_PATH> <LOCAL_PATH>

# 运维子命令（排障用，不进 SKILL.md）：
mt-ssh-cli daemon           # 前台跑 daemon（正常由 CLI 自动 detached 拉起，无需手动）
mt-ssh-cli daemon-status    # 打印 daemon 版本 / pid / 池内 session 数
mt-ssh-cli daemon-stop      # 请求 daemon 优雅退出（drain 池后退出）
```

- `--project-id` 缺省时读环境变量 `MINITERM_PROJECT_ID`，再缺省 = 不限项目（与 MCP 现行语义一致）。v1 由 SKILL.md 写死 flag，env 注入留作后续增强；
- `<CONNECTION>` 按 name 优先、id 兜底匹配（复用 `find_connection`，歧义/未找到语义不变）；
- exec 的 `<COMMAND>...` 为 trailing args，按空格 join 成远程命令（clap `trailing_var_arg` + `allow_hyphen_values`）；`--cwd` 沿用 `cd <dir> && ` 前缀拼接；
- 超时默认：exec 60s、upload/download 300s（与 MCP 对齐），由 daemon 端强制执行（CLI 被杀不影响清理）；
- upload/download 的 `<LOCAL_PATH>` 在 **CLI 端绝对化**后再发给 daemon——daemon 的 cwd 与调用方无关，相对路径必须在调用现场解析。

#### 输出与退出码契约

| 场景 | stdout | stderr | 退出码 |
|------|--------|--------|--------|
| exec 正常 | 远程 stdout 原样字节流（**实时流式**） | 远程 stderr 原样 | 透传远程退出码 |
| exec 超时 | 已收到的部分输出 | `mt-ssh-cli: error: timed out after <N>s` | 124（对齐 GNU timeout） |
| 连接/认证/传输失败、用法错误、daemon 不可达 | — | `mt-ssh-cli: error: <原因>`（绝不含密码） | 2 |
| list | 表格文本；`--json` 输出与 MCP `SshConnectionView` 同构的 JSON | — | 0 |
| upload/download 成功 | 一行 `uploaded/downloaded <N> bytes: <local> ↔ <remote>` | — | 0 |

- **不做 100KB 封顶**：输出经 daemon 流式转发、CLI 实时打印，Claude/Codex 的 Bash 结果自身有截断；SKILL.md 教 agent 大输出重定向到文件；
- 远程命令自身返回 124 时与超时无法区分——接受该歧义（stderr 有无 `mt-ssh-cli: error:` 前缀可辅助判断）；
- CLI 自身错误统一 exit 2 + 前缀，agent 靠 stderr 前缀与远程命令的 exit 2 区分。

### 2. 守护进程架构（CLI ↔ daemon）

对标 MCP sidecar 的常驻池收益，但把「每个 agent 会话一个进程」收敛成「全机一个 daemon」。

#### 进程模型

- **全局单例**，不按 project 拆分：池按 `connection_id` 缓存 session，与 project 无关；project 范围过滤是**请求级**参数——daemon 处理每个请求时重新读 config.json 并按该请求的 `--project-id` 过滤（MCP 里这是进程启动参数，语义等价，且天然保证「主程序里改关联范围即时生效」的既有承诺）；
- daemon 就是 `mt-ssh-cli --daemon`（同一二进制），持有一个 `SshPool`，复用 MCP 侧全套编排：acquire → exec → transport 错 evict + 单次 retry → 失败 mark_unhealthy 30s cooldown → 审计。

#### IPC 传输与协议

- Windows：named pipe `\\.\pipe\mini-term.ssh-cli.<用户 SID（清洗后原文，非哈希——哈希在 Rust 版本间不保证稳定，会让升级前后的 CLI/daemon 算出不同 pipe 名、版本握手失效）>`，pipe security descriptor 限**仅当前用户**可连；
- macOS/Linux：Unix domain socket（`$XDG_RUNTIME_DIR` 优先，回退 config.json 同目录），权限 0600；
- WSL 关键路径：CLI 在 WSL 里经 interop 执行时**仍是 Windows 进程**，连的是 Windows 侧 named pipe——daemon、池、config.json 全在 Windows 侧，天然一致；
- 协议：newline-delimited JSON 帧（serde camelCase，`v` 字段版本号）。
  - 连接建立后 daemon 先发 `{type:"hello", version:"<CARGO_PKG_VERSION>", pid}`；
  - 请求（CLI→daemon，单行）：`{v:1, op:"list|exec|upload|download|status|shutdown", projectId?, connection?, command?, cwd?, timeoutSecs?, localPath?, remotePath?}`；
  - 响应流（daemon→CLI，多行直至终帧）：
    - `{type:"stdout", dataB64}` / `{type:"stderr", dataB64}`（exec 输出分片，base64 保二进制安全）；
    - 终帧 `{type:"result", exitCode?, timedOut?, bytes?, connections?}`（按 op 携带对应字段）；
    - 终帧 `{type:"error", message}`（message 绝不含密码，与 MCP 同一纪律）。

#### 生命周期

- **自拉起**：CLI 连 pipe 失败 → spawn 自身 `daemon` 子命令（detached：Windows `DETACHED_PROCESS`+`CREATE_NEW_PROCESS_GROUP`，Unix `process_group(0)` + stdio 全空——与双 fork/setsid 等效且免 unsafe）→ 带退避重试连接（总窗口 ~3s）；
- **并发竞态**：多个 CLI 同时拉 daemon → pipe/socket 绑定天然互斥，抢输的 daemon 实例静默退出，CLI 重试连接即收敛；
- **空闲自退**：无活跃请求且 10 分钟无新连接 → drain 池（逐 session disconnect ByApplication，复用 `pool.shutdown()`）→ 退出。孤儿 daemon 最多存活一个空闲周期；
- **版本握手**：CLI 比对 hello 帧 version 与自身不符（app 升级后旧 daemon 还在跑）→ 发 `shutdown` op → 等旧 daemon 退出 → 拉起新版。dev 迭代同理受益：`daemon-stop` 可手动踢掉占着二进制文件锁的旧 daemon（Windows 下运行中的 exe 无法被 stage-sidecars 覆盖，现有「跳过被占用文件」的容忍逻辑继续兜底）；
- **CLI 中途被杀**（agent Bash 超时 / Ctrl+C）：daemon 检测到连接断开即关闭对应 exec channel（session 保留在池里），请求级超时也由 daemon 独立强制，不依赖 CLI 存活。

#### 安全边界

- IPC 仅限当前用户可连；信任边界与现状**等价**——本机同用户进程本来就能直接 spawn `mt-ssh-mcp` 或读 config.json，daemon 不新增暴露面；
- `is_blocked_local_path`（config.json 传输硬拒绝）在 **daemon 端**执行，单点强制，CLI 无法绕过；
- 审计日志由 daemon 单点追加（沿用 `ssh-mcp-audit.log` 文件与行格式），消除并发写交错。

#### 降级路径

daemon 拉不起来或 IPC 异常（极端环境）→ CLI 自动 fallback 为进程内 one-shot 执行（`SshPool::new()` → acquire → 执行 → `shutdown()`），stderr 记一行降级提示。业务逻辑走同一 service 层（见 §3），fallback 近零成本，保证工具永远可用。

### 3. 共享 service 层（`mt-sidecars` 加 lib target）

daemon 化后抽取层次升级：不只抽纯函数，而是把**完整业务编排**抽成 service 层，MCP handler 与 daemon handler 都退化成薄传输层。

`mt-sidecars/src/lib.rs` + `src/ssh_service.rs`：

- `list_connections(project_id) -> Vec<SshConnectionView>`；
- `exec(pool, project_id, connection, command, cwd, timeout, on_output) -> ExecOutcome`——含 find_connection、acquire、evict+retry、cooldown、审计全套编排（从 `mt-ssh-mcp.rs` 的 `ssh_exec` 方法体整体迁移）；`on_output: impl FnMut(StreamKind, &[u8])` 流式回调：daemon 侧写 IPC 帧实时转发，MCP 侧收集进 Vec 再 cap_output 打包 JSON（两端行为各自保持不变）；
- `transfer(pool, direction, ...) -> u64`——含护栏 + retry + 审计（迁自 `run_transfer`）；
- 纯函数与既有单测随迁：`find_connection` / `build_remote_command` / `cap_output` / audit 系列 / `is_blocked_local_path*` / `run_exec_on_session`（改造为回调式）/ `TransferDirection`。

`mt-ssh-mcp.rs` 瘦身成 rmcp 适配层，**过渡期继续构建发布**（存量项目 `.mcp.json` 仍指向它，两者共享同一审计日志与行为）。

### 4. SKILL.md 生成

daemon 对 skill 完全透明——SKILL.md 只描述 CLI 用法，不提 daemon。两份产物，路径：

- `<project>/.claude/skills/mini-term-ssh/SKILL.md`（Claude Code）
- `<project>/.codex/skills/mini-term-ssh/SKILL.md`（Codex）

生成策略与 `.mcp.json` 不同：SKILL.md 是 mini-term **独占生成物**（不是与用户共享的合并文件），直接整文件覆盖写入，文件头带 generated 注释标记。内容含 CLI 绝对路径与 project-id（机器相关 → 进 `.gitignore`）。

模板（Claude 版；Codex 版去掉 `allowed-tools` 行，其余相同）：

```markdown
---
name: mini-term-ssh
description: Run commands or transfer files on remote servers over SSH using this project's saved connections (managed by mini-term). Use when the user asks to operate on a remote host — run commands, inspect logs, deploy services, upload or download files.
allowed-tools: 'Bash("<ABS_PATH>" *), PowerShell("<ABS_PATH>" *)'
---

<!-- generated by mini-term; do not edit (regenerated on enable) -->

# mini-term SSH CLI

This project has saved SSH connections managed by mini-term. Authentication is
handled internally — NEVER ask the user for passwords and NEVER use plain
`ssh` / `scp` / `sftp`.

CLI binary (quote the path, it may contain spaces):

    "<ABS_PATH>"

Inside WSL, call it via Windows interop:

    "$(wslpath '<ABS_PATH>')"

All examples below already carry this project's id.

## List available connections

    "<ABS_PATH>" list --project-id <PROJECT_ID> --json

## Run a remote command

    "<ABS_PATH>" exec --project-id <PROJECT_ID> [--cwd DIR] [--timeout SECS] <connection> -- <command...>

- Remote stdout/stderr stream through; exit code = remote exit code.
- Exit 124 = timeout (default 60s), exit 2 + `mt-ssh-cli: error:` on stderr = CLI/connection error.
- Large output: append `> file.log` and read the file afterwards.

## Upload / download files (SFTP)

    "<ABS_PATH>" upload   --project-id <PROJECT_ID> <connection> <local_path> <remote_path>
    "<ABS_PATH>" download --project-id <PROJECT_ID> <connection> <remote_path> <local_path>

Do NOT base64-echo file contents through exec — use upload/download.
```

要点：

- frontmatter 仅 `name` / `description`（+ Claude 版 `allowed-tools`）——两端公共子集；`allowed-tools` 让 skill 激活期间免审批（授权随下一条用户消息失效，符合预期）；
- 正文英文（与现有 MCP tool descriptions 一致，对两端 agent 触发与遵循最稳）；
- description 是触发关键：明确列出「远程执行 / 查日志 / 部署 / 传文件」场景。

### 5. `ssh_skill_registry.rs`（新模块，替代 `ssh_mcp_registry` 的注册职责）

对标 `ssh_mcp_registry.rs` 的风格（校验 → 幂等写入 → 可读中文错误）：

**`enable_ssh_tools(project_dir, project_id)`**：
1. 写 `.claude/skills/mini-term-ssh/SKILL.md`（含 `allowed-tools`）；
2. 写 `.codex/skills/mini-term-ssh/SKILL.md`；
3. `.gitignore` 幂等追加 `.claude/skills/mini-term-ssh/` 与 `.codex/skills/mini-term-ssh/`（存量 `.mcp.json` / `.codex/` 条目不清理，无害）;
4. `trust_project_in_codex` 保留（项目级 `.codex/` 内容大概率仍要求 trusted，写入无害且幂等）；
5. **迁移清理**：调 `remove_project_mcp_json` / `remove_project_codex_config` / `set_claude_mcp_approval(false)`（`ssh_mcp_registry` 相应函数改 `pub(crate)`），老项目切换时自动摘除 MCP 注册。

**`disable_ssh_tools(project_dir)`**：删两份 SKILL.md 及空目录（`skills/` 目录若空一并删除，不留空壳）；顺带执行同样的 MCP 迁移清理（兜底存量）。Codex 信任与 `.gitignore` 沿用现行策略不回收。

`lib.rs`：注册 `enable_ssh_tools` / `disable_ssh_tools`，移除 `enable_ssh_mcp` / `disable_ssh_mcp`。

### 6. 前端与 i18n

- `SshAssocModal.tsx`：`invoke('enable_ssh_mcp')` → `invoke('enable_ssh_tools')`，disable 同理；其余勾选/范围迁移逻辑不动（daemon 每请求重读 config.json，范围变更即时生效的承诺保持）；
- `src/i18n/locales/sshAssoc.ts`：文案里的「SSH MCP」改为「SSH 工具」（zh/en 双语同步），`enabledMessage` 保留「正在运行的会话需重启后生效」（skill 在会话启动时扫描，保守沿用该表述）；
- `types.ts` 的 `sshMcpEnabled` 字段名保留，注释更新语义。

### 7. 构建管线

- `scripts/stage-sidecars.mjs`：`SIDECARS` 数组加 `'mt-ssh-cli'`；
- `src-tauri/tauri.conf.json`：`bundle.externalBin` 加 `"binaries/mt-ssh-cli"`；
- `mt-sidecars/Cargo.toml`：加 `[lib]`（bins 引用 service 层）、新 `[[bin]]`、`clap = { version = "4", features = ["derive"] }`、tokio 增补 `net` feature（named pipe / unix socket）、`base64`。

### 8. 测试计划

**单元测试（`cargo test`，沿用 fs.rs 式风格）**：
- `ssh_service` 迁移后原测试全绿（find_connection / cap_output / audit / 护栏等 40+ 条），`run_exec_on_session` 回调化后 MCP 侧收集行为等价；
- CLI 新增：trailing args 拼装、`--project-id` flag/env 优先级、退出码映射纯函数、list 文本/JSON 投影不含密码、local_path 绝对化；
- daemon 协议：请求/响应帧序列化 round-trip、hello 版本比对逻辑、stdout/stderr 分片 base64 编解码（纯函数级）；
- daemon 生命周期（集成测试，进程内起 socket/pipe）：并发绑定互斥收敛、空闲计时触发 drain、shutdown op 幂等；
- registry 新增：SKILL.md 生成内容断言（含 project-id、Windows 路径原样嵌入、frontmatter 差异化）、覆盖写幂等、disable 清理含空目录、`.gitignore` 追加幂等、enable/disable round-trip 含 MCP 迁移清理（`.mcp.json` 里其它 server 毫发无损）。

**手动 e2e 清单**：
1. Windows + Claude Code：skill 触发 → list/exec/upload/download → 首调自拉 daemon → 二调复用池（观察延迟差）→ 审计日志落行；
2. Windows + Codex：同上，重点观察沙箱是否拦网络/拦 pipe（见开放问题）；
3. WSL + Claude Code：interop 路径执行、连通 Windows 侧 daemon、退出码透传；
4. 生命周期：并发多 pane 同时首调（竞态收敛）、app 升级后版本握手换代、`daemon-stop` 排障、CLI 中途 Ctrl+C 后 daemon 池不泄漏；
5. 边界：大输出重定向、超时 124、未关联连接不可见、`allowed-tools` 带空格路径实测。

### 9. 取舍与开放问题

| # | 问题 | v1 决策 | 落地结论（2026-07-31 实测） |
|---|------|---------|------------------------------|
| 1 | daemon 引入生命周期复杂度（自拉起竞态、孤儿、版本换代、dev 下二进制文件锁定） | 接受，换取池复用；空闲 10min 自退 + 版本握手 + `daemon-stop` 三道兜底 | 真机全部验证通过：4 路并发冷启动收敛到单 daemon；空闲窗口后自退；旧版 daemon 被新 CLI 自动踢旧拉新；`daemon-stop` 后二进制文件锁释放、stage 脚本可覆盖 |
| 2 | Codex workspace-write 沙箱默认禁网，CLI 需网络（连 pipe 是否被沙箱拦也未知）→ 可能每次审批 | v1 不写 `network_access = true`（Windows 主平台沙箱为实验状态、影响有限）；实测被拦再评估是否随 enable 写入项目 `.codex/config.toml` | **待手动实测**（Windows + Codex 交互式会话） |
| 3 | `allowed-tools: 'Bash("<带空格路径>" *)'` 语法未实测 | 实测失败则去掉该行，agent 首次调用手动批准一次 | 部分结论：该行不破坏 skill 加载与触发；Windows 版 Claude Code 的 shell 工具可能是 PowerShell，模板已补 `PowerShell("<ABS_PATH>" *)` 双条目（注意：带 `&` 调用符的写法会导致 skill 加载失败）。无头沙箱环境无法验证自动放行；交互式会话「首次调用批准一次」兜底可用，交互式自动放行**待手动实测** |
| 4 | Codex 对未知 frontmatter 字段（如误留 allowed-tools）容忍度未知 | 两份模板独立生成规避 | 按决策落地（Codex 版无 allowed-tools），无需再验 |
| 5 | skill 触发率不如 MCP 工具列表强曝光 | description 覆盖典型场景；实测触发不稳再在 CLAUDE.md 模板补一句指引 | Claude Code 无头会话 4/4 轮自然语言（"查远程服务器 uptime"）均正确触发 skill 并按模板拼出带 project-id 的命令；触发率暂无忧 |
| 6 | 远程命令自身 exit 124 与超时歧义 | 接受，stderr 前缀可辅助区分 | 按决策落地 |
| 7 | daemon 与残存 MCP sidecar 并存时各持独立池（同一连接可能两条 session） | 接受（服务器视角只是两个登录），PR3 下线 MCP 后消失 | 接受不变（注：MCP 下线是 PR4） |

### 10. 分阶段落地

- **PR1**：service 层抽取（`mt-sidecars` lib + `ssh_service` 回调化）+ MCP 适配层瘦身回归。纯重构，行为零变化，`cargo test` 全绿即可合；✅ 已落地
- **PR2**：`mt-ssh-cli` 二进制（CLI + daemon + IPC 协议 + 降级路径）+ 构建管线。纯新增，不动注册链路；✅ 已落地
- **PR3**：`ssh_skill_registry` + 前端切换 + i18n + MCP 迁移清理 + docs（features 双语；CLAUDE.md command 表原本未列 SSH 命令，无需改）。注：`ssh_mcp_registry` 的写入路径已在本阶段一并移除（切换后即无调用方，留着是死代码），PR4 仅剩 bin 下线；✅ 已落地
- **PR4**（后续版本）：确认存量迁移完成后，移除 `mt-ssh-mcp` bin 与 externalBin 条目、`ssh_mcp_registry` 仅留读侧清理，spec 标 final。

### 11. 落地验证记录（2026-07-31）

自动化 e2e 已跑通（Windows + 真实 SSH 连接 oracle-4c-24g，全部通过）：

- CLI 契约：`list` 表格/JSON 不含敏感字段；`exec` 退出码透传（0/17）、超时 124 + stderr 前缀、未找到连接 exit 2；SFTP 上传/下载 round-trip 字节一致；审计日志逐条落行（成功记字节数/退出码，失败记 error/timeout）；project 范围过滤越权连接「未找到」，env 兜底生效；
- daemon：冷启动自拉起（~1.8s 全链路）→ 二次调用池复用（~0.39s）；4 路并发 exec 不串流；4 路并发冷启动收敛单 daemon；CLI 中途被杀 daemon 存活且池不泄漏；空闲自退（短窗口计时验证）；版本握手换代（0.4.8 daemon 被 0.4.9 CLI 踢旧拉新）；`daemon-stop` 幂等、释放二进制文件锁；强制端点不可用时自动降级 one-shot 且业务成功；
- skill 端到端：临时项目放入生成的 SKILL.md，新起 Claude Code 会话用自然语言问「远程服务器 uptime」→ 触发 skill → 经 daemon 执行 CLI → 正确返回远端 uptime/内核版本（权限经显式白名单放行，等价 skill allowed-tools 语义）。

**剩余手动矩阵**（需交互式会话/真机环境，见 §9 表）：

1. Windows + Claude Code 交互式：UI「关联 SSH」启用 → skill 自动触发 → `allowed-tools` 是否免审批（开放问题 #3 交互式部分）；
2. Windows + Codex：沙箱是否拦网络/拦 pipe（开放问题 #2）；
3. WSL + Claude Code：interop 路径执行、连 Windows 侧 daemon、退出码透传；
4. 存量 MCP 项目在 UI 里重新保存关联 → 迁移清理生效（单测已覆盖文件级行为，差 UI 全流程一遍）；
5. IPC 端点跨用户连接被拒（Windows ACL / Unix 0600 —— 代码已实现，需第二个系统用户实测）。
