# Research: Claude Code 如何加载项目级 MCP Server（`.mcp.json`）

- **Query**: Claude Code 项目级 MCP server 加载机制：`.mcp.json` schema、信任/审批流程、作用域优先级、热加载时序、写入 repo 的坑
- **Scope**: external（Claude Code 官方文档）
- **Date**: 2026-05-18
- **背景**: mini-term 要把 SSH 连接管理做成 stdio MCP sidecar，启用某项目「SSH MCP」时自动往该项目目录写 MCP 注册配置。本调研只覆盖 Claude Code 一侧；Codex 一侧（`~/.codex/config.toml` `[mcp_servers]`）需另开调研。

---

## TL;DR 给实现者的结论

1. **用项目根的 `.mcp.json`**（不是 `.claude/settings.json`，`settings.json` 根本不读 `mcpServers` key）。这是 Claude Code 唯一的「按项目隔离」且能精确控制的方式。
2. **schema 极简**：顶层 `mcpServers` 对象，每个 server 一个 key；stdio 类型用 `type` / `command` / `args` / `env` 四个字段。
3. **会弹审批**：`.mcp.json` 里的 project-scoped server 默认需要用户**一次性审批**（首次进入项目时弹窗，或在 `/mcp` 面板里批准）。
4. **`enableAllProjectMcpServers: true`** 可让 Claude Code 自动批准 `.mcp.json` 里所有 server（跳过弹窗）——但这要写进**用户级** `~/.claude/settings.json` 才安全可控。
5. **不能热加载新 server**：运行中的会话写入/新增 `.mcp.json` 条目，需要**重启 Claude Code 会话**才生效（`list_changed` 只刷新「已连接 server」的工具列表，不会拉起新 server）。
6. **写 repo 的坑**：`.mcp.json` 设计上就是给团队共享 / 提交版本控制的；mini-term 自动写入会被 git 跟踪。若 `command` 用绝对路径（指向 mini-term sidecar bin），提交后对队友机器无效 → **建议 `.gitignore` 排除，或用环境变量展开**。

---

## Findings

### 1. `.mcp.json` 的 JSON Schema（stdio server 完整示例）

`.mcp.json` 放在**项目仓库根目录**（不是 `.claude/` 子目录里，放错位置会「never load」）。
文件结构由顶层 `mcpServers` 对象构成，每个 key 是一个 server 名。

#### stdio server 的完整字段

| 字段 | 说明 |
|---|---|
| `type` | `"stdio"`。也接受不写（stdio 是本地进程默认）。HTTP 类型用 `"http"`，`"streamable-http"` 是 `"http"` 的别名。 |
| `command` | server 可执行文件路径或 PATH 上的命令名。**本地脚本必须用绝对路径**——相对路径会相对「Claude Code 启动目录」解析而非 `.mcp.json` 所在目录，是最常见的启动失败原因。 |
| `args` | 命令行参数数组。 |
| `env` | 传给 server 子进程的环境变量对象。注意：`settings.json` 里的 `env` **不会**传播给 MCP 子进程，要传环境变量必须写在这里的 per-server `env`。 |

#### 官方标准格式（来自 `claude mcp add --scope project` 生成结果）

```json
{
  "mcpServers": {
    "shared-server": {
      "command": "/path/to/server",
      "args": [],
      "env": {}
    }
  }
}
```

#### stdio server 完整示例（带 type / args / env，适配 mini-term SSH sidecar）

```json
{
  "mcpServers": {
    "miniterm-ssh": {
      "type": "stdio",
      "command": "C:\\Users\\you\\AppData\\Local\\mini-term\\miniterm-ssh-mcp.exe",
      "args": ["--project-id", "proj-123"],
      "env": {
        "MINITERM_SSH_MCP_LOG": "stderr"
      }
    }
  }
}
```

> 官方 `claude mcp add-json` 文档给的 stdio 范例 JSON：
> `{"type":"stdio","command":"/path/to/weather-cli","args":["--api-key","abc123"],"env":{"CACHE_DIR":"/tmp"}}`

#### 环境变量展开（关键，能解决绝对路径硬编码问题）

`.mcp.json` 支持环境变量展开，可用于 `command` / `args` / `env` / `url` / `headers`：

- `${VAR}` — 展开为环境变量 `VAR` 的值
- `${VAR:-default}` — `VAR` 有值用其值，否则用 `default`

若某个必需变量未设置且无默认值，Claude Code **会解析配置失败**（整个 `.mcp.json` 报错）。

特殊变量：
- `CLAUDE_PROJECT_DIR` — 项目根目录。Claude Code 会把它注入到 **server 子进程的环境**（不是 Claude Code 自身环境）。因此在 project/user 作用域的 `.mcp.json` 里通过 `${CLAUDE_PROJECT_DIR}` 引用时**必须带默认值**，例如 `${CLAUDE_PROJECT_DIR:-.}`。（只有 plugin 提供的 MCP 配置才能裸用 `${CLAUDE_PROJECT_DIR}`。）
- server 进程内也能直接读 `process.env.CLAUDE_PROJECT_DIR` / `os.environ["CLAUDE_PROJECT_DIR"]`，或调 MCP `roots/list` 请求拿到 Claude Code 的启动目录。

可选高级字段（stdio 通常用不上，HTTP 才需要）：`oauth`、`headers`、`headersHelper`、`alwaysLoad`（设 `true` 让该 server 工具不走 tool-search 延迟加载，但会阻塞启动直到连上，上限 5s 连接超时；需 Claude Code v2.1.121+）。

**保留名**：server 名 `workspace` 是内部保留的，用了会被跳过并告警。给 mini-term 的 server 起名别用 `workspace`（建议 `miniterm-ssh` 之类）。

---

### 2. 信任 / 审批流程：项目目录有 `.mcp.json` 时会发生什么

**会弹窗（或在 `/mcp` 面板里需要批准）。** 官方明确：

> "For security reasons, Claude Code prompts for approval before using project-scoped servers from `.mcp.json` files."

具体行为：

- `.mcp.json` 里的 project-scoped server 需要**一次性审批（one-time approval）**。
- 首次在该项目里启动 Claude Code 时会出现审批提示；用户批准后选择被记住。
- 如果审批弹窗被忽略 / dismiss 掉，**该 server 会一直处于 disabled 状态**，直到用户在 `/mcp` 面板里手动批准。
- 调试现象（来自 debug-your-config 文档的「常见原因」表）：
  - 症状「Project MCP server added but doesn't appear」→ 原因「一次性审批弹窗被 dismiss 了」→ 解决「跑 `/mcp` 查看状态并批准」。
- 想重置审批选择：`claude mcp reset-project-choices`。

#### `enableAllProjectMcpServers` 设置

来自 settings.json `Available settings` 表：

| Key | 说明 | 示例值 |
|---|---|---|
| `enableAllProjectMcpServers` | **自动批准项目 `.mcp.json` 文件里定义的所有 MCP server** | `true` |
| `enabledMcpjsonServers` | 按名字白名单：只批准 `.mcp.json` 里指定的某些 server | `["memory", "github"]` |
| `disabledMcpjsonServers` | 按名字黑名单：拒绝 `.mcp.json` 里指定的某些 server | `["filesystem"]` |

含义：

- `enableAllProjectMcpServers: true` → Claude Code 不再为 `.mcp.json` 弹审批窗，全部自动信任。
- `enabledMcpjsonServers: ["miniterm-ssh"]` → 只自动批准名为 `miniterm-ssh` 的 server，其它仍需手动批准。这是比「全开」更精细的做法。
- 这些 key 写在 `settings.json` 里。**安全要点**：若把 `enableAllProjectMcpServers` 写进项目自身的 `.claude/settings.json` 并提交，等于让任何 clone 该 repo 的人自动信任 repo 里的 `.mcp.json`——绕过了信任机制。要让用户自己掌控，应写进**用户级** `~/.claude/settings.json`（或本地 `.claude/settings.local.json`，不提交）。

#### 用户怎么批准（三种途径）

1. 首次进入项目时的审批弹窗里选「批准」。
2. 会话内运行 `/mcp` 命令，在面板里查看 server 状态并批准 / Reconnect。
3. 预先在 `settings.json` 写 `enableAllProjectMcpServers: true` 或 `enabledMcpjsonServers: ["<server名>"]`，跳过交互。

> 安全相关补充：MCP 文档反复提示「连接前确认信任该 server」，会拉外部内容的 server 有 prompt injection 风险。`headersHelper`（执行任意 shell 命令）在 project/local 作用域下「只有用户接受 workspace trust 对话框后才会运行」。stdio server 的 `command` 本质也是在用户机器上执行任意进程，所以审批门槛是设计使然。

---

### 3. 作用域与优先级：`.mcp.json` vs `~/.claude.json` vs `.claude/settings.json`

#### 三种 MCP 作用域（这是关键，三者是不同文件、不同语义）

| Scope | 加载范围 | 是否随团队共享 | 存储位置 |
|---|---|---|---|
| **Local**（默认） | 仅当前项目 | 否（仅自己） | `~/.claude.json`（按项目路径分桶） |
| **Project** | 仅当前项目 | 是（通过版本控制） | **项目根的 `.mcp.json`** |
| **User** | 你的所有项目 | 否（仅自己） | `~/.claude.json` |

要点：

- **Local scope**（`claude mcp add` 不带 `--scope` 的默认）：写进 `~/.claude.json`，按「当前项目路径」分桶存储。结构：
  ```json
  {
    "projects": {
      "/path/to/your/project": {
        "mcpServers": {
          "stripe": { "type": "http", "url": "https://mcp.stripe.com" }
        }
      }
    }
  }
  ```
  注意：MCP 的「local scope」与 settings 的「local settings」是两码事——MCP local server 存在 `~/.claude.json`（家目录），而 settings 的 local 是项目里的 `.claude/settings.local.json`。
- **Project scope**：`claude mcp add --scope project` 写进项目根 `.mcp.json`，设计上就是要 check 进版本控制，让全队共享。
- **User scope**：`claude mcp add --scope user` 写进 `~/.claude.json`，跨所有项目可用，仅自己可见。

> `claude mcp add` 的 `--scope` 旧名对照：`local` 旧称 `project`；`user` 旧称 `global`。

#### `.claude/settings.json` 不参与 MCP 配置

**重要澄清**：`.claude/settings.json` / `~/.claude/settings.json` **不读 `mcpServers` key**。

- debug 文档的常见错误表：「MCP servers added under `mcpServers` in `settings.json` never appear」→ 原因「`settings.json` does not read an `mcpServers` key」→ 解决「项目 server 定义在仓库根的 `.mcp.json`，或用 `claude mcp add --scope user`」。
- `settings.json`（各级）只负责 MCP 的**审批策略**（`enableAllProjectMcpServers` / `enabledMcpjsonServers` / `disabledMcpjsonServers`）和 managed 限制（`allowedMcpServers` / `deniedMcpServers`），**不存 server 定义本身**。
- `~/.claude.json` 也别拿来写 permissions/hooks/env——那些归 `~/.claude/settings.json`；`~/.claude.json` 存的是 app 状态、OAuth session、user+local 作用域的 MCP 配置、per-project 状态（含 trust 设置）和各种缓存。

文档官方对照表（settings 文档「Settings files」）：

| 类型 | User | Project | Local |
|---|---|---|---|
| MCP servers | `~/.claude.json` | `.mcp.json` | `~/.claude.json`（per-project 分桶） |

#### 作用域优先级（同名 server 定义在多处时）

同名 server 在多处定义时，Claude Code 只连一次，取**最高优先级**来源：

1. Local scope
2. Project scope
3. User scope
4. Plugin 提供的 server
5. claude.ai connector

三种 scope 按 **server 名字**匹配重复项；plugin 和 connector 按 endpoint（URL/command）匹配。

#### managed 层（企业管控，mini-term 通常不涉及）

管理员可部署 `managed-mcp.json` 到系统目录取得**独占控制**（用户无法增改任何 MCP server）；或用 `allowedMcpServers` / `deniedMcpServers` 做白/黑名单策略。denylist 优先级最高。如果用户机器上有企业 `managed-mcp.json`，mini-term 写的 `.mcp.json` 可能被整体忽略——属于边缘情况，但值得在错误提示里考虑。

#### 「按项目隔离」最干净的选择 → 结论

**项目根 `.mcp.json`（Project scope）是唯一真正干净的「按项目隔离」方案**：

- 配置物理上就在项目目录里，启用/停用只动这一个文件，天然按项目隔离。
- 不污染用户全局 `~/.claude.json`。
- 停用时直接从 `.mcp.json` 删掉对应 key（或删整个文件）即可，幂等好做（对标 mini-term 现有 `hook_registry.rs` 的 marker 幂等模式）。
- 对比：Local scope 也是「仅当前项目」，但存在 `~/.claude.json` 的 `projects[<path>].mcpServers`——配置散落在全局文件里、按绝对路径分桶，mini-term 要去改一个大 JSON 的深层嵌套，且项目移动/改名后失配，明显更脏。
- 唯一代价：`.mcp.json` 在项目目录里会被 git 跟踪（见第 5 节的坑）。

---

### 4. 时序问题：会话已启动后才写/改 `.mcp.json`，能否热加载

**结论：不能热加载新增的 MCP server。运行中的会话写入或新增 `.mcp.json` 条目后，必须重启 Claude Code 会话才生效。**

依据：

- 官方文档对 `settings.json` 明确写了热加载：「Edits to `settings.json` take effect in the running session after a brief file-stability delay. You don't need to restart.」——但**对 `.mcp.json` 完全没有任何等价表述**。
- `.mcp.json` 的 project-scoped server 走的是「会话启动时读取 + 一次性审批」路径。审批弹窗是「首次进入项目时」出现的，属于 session 启动行为。
- MCP 文档里唯一的「动态更新」是 `list_changed` 通知：「allowing MCP servers to dynamically update their available tools, prompts, and resources without requiring you to disconnect and reconnect」——这只刷新**已经连上的 server** 的工具/prompt/resource 列表，**不会拉起一个 `.mcp.json` 里新增的 server**。
- stdio server 是本地进程，文档明确「Stdio servers are local processes and are not reconnected automatically」（连断线自动重连都没有）。
- 社区排障普遍做法：新增/改 MCP 配置后「重启 Claude Code / 重启 VS Code」才能看到新 server（多个第三方排障文与 GitHub issue 一致）。
- `alwaysLoad` / `MCP_TIMEOUT` / `MCP_CONNECTION_NONBLOCKING` 等相关行为描述全部围绕「session startup」「first prompt is built」展开，进一步佐证 server 列表在会话启动时定型。

**对 mini-term 的影响（重要）**：

- mini-term 在用户启用「SSH MCP」时往项目目录写 `.mcp.json`，如果该项目的终端里**已经有一个 Claude Code 会话在跑**，那个会话**不会**自动获得 SSH 工具。
- 需要的话由 mini-term UI 提示用户：「SSH MCP 已启用，请重启该项目里的 Claude Code 会话以加载」。
- 反过来：在启动 Claude Code **之前**就把 `.mcp.json` 写好，则新会话启动即加载（仍需过一次审批，除非用户已设 `enableAllProjectMcpServers`）。
- 已存在 server 的「工具集变化」可热刷新（靠 sidecar 发 `list_changed`），但「server 从无到有」不行。
- 注：以上是基于官方文档明确措辞（`settings.json` 有热加载声明、`.mcp.json` 无）的强推断，文档没有一句正面写「`.mcp.json` 改动需重启」。实现时建议默认按「需重启」设计并在 UI 提示，最稳妥。

---

### 5. 自动往用户 repo 写 `.mcp.json` 的坑

#### 坑 1：会被 git 跟踪 / 提交

`.mcp.json` 的官方定位就是「designed to be checked into version control」——它躺在项目根目录，`git status` 会把它当新文件，用户 `git add .` 很容易顺手提交。mini-term 自动写入会制造一个用户没主动创建、却会进 commit 的文件。

#### 坑 2：绝对路径硬编码 → 提交后对队友无效

stdio server 的 `command` 推荐用**绝对路径**（相对路径会失败）。mini-term sidecar bin 的绝对路径是**机器特定**的（不同 OS、不同安装位置都不同）。一旦把带绝对路径的 `.mcp.json` 提交：

- 队友 clone 后，`command` 指向一个他们机器上不存在的路径 → server 启动失败，`/mcp` 显示 failed。
- 跨平台更糟：Windows 的 `C:\...\miniterm-ssh-mcp.exe` 在 macOS/Linux 完全无意义。
- 而且 `.mcp.json` 还会触发队友的 MCP 审批弹窗——一个他们根本没装 mini-term 的人被问要不要信任一个 SSH RCE server。

#### 坑 3：泄漏 / 安全面

`.mcp.json` 本身只存 server 注册信息（命令/参数/env），mini-term 的 SSH 明文密码存在 `config.json` 不在这里，所以 `.mcp.json` 本身不直接泄密。但它把「这台机器上有个能 SSH 进各服务器的 MCP server」这一事实写进了可能被提交的文件，等于在 repo 历史里留下基础设施线索。

#### 缓解建议（给实现者）

1. **写入时同步把 `.mcp.json` 加进项目的 `.gitignore`**（幂等追加一行 `.mcp.json`）。这是最直接的做法——mini-term 写的是「本机专用」配置，不应进版本控制。PRD 的 Open Question「`.mcp.json` 写进项目目录后是否要自动 `.gitignore`」→ 本调研结论：**应该**。
   - 注意边缘情况：若该项目本来就有团队共享的 `.mcp.json`（别的 server），mini-term 既不能覆盖也不能贸然 gitignore 整个文件。需要先检测文件是否已存在/已被 git 跟踪，再决定策略（marker 幂等合并而非整文件覆盖，对标 `hook_registry.rs`）。
2. **绝对路径不可避免**（sidecar bin 路径就是机器特定的），但可用环境变量展开缓解可移植性：把 sidecar 路径放进一个环境变量，`.mcp.json` 里写 `${MINITERM_SSH_MCP_BIN}`。不过这要求该变量在 Claude Code 进程环境里可见——mini-term 是 Claude Code 的父进程（终端宿主），可以在 spawn 终端 shell 时注入该环境变量，使 `.mcp.json` 在 mini-term 启动的终端里可移植、在别处则优雅失败。
3. **停用即清理**：用户关掉某项目的「SSH MCP」时，从 `.mcp.json` 移除对应 server key（marker 幂等，文件里没有别的 server 就删整个文件），别留垃圾配置。
4. **审批是用户的事**：mini-term 写完 `.mcp.json` 后，用户仍需在 Claude Code 里过一次审批。mini-term 可在 UI 里提示这一步，或（若想免审批）提示用户在 `~/.claude/settings.json` 加 `enabledMcpjsonServers: ["miniterm-ssh"]`——但**不要由 mini-term 擅自改用户的 `settings.json` 来绕过信任**，那是用户的安全决定。
5. 注：Windows 上「stdio server 经 `npx` 启动需要 `cmd /c` 包一层」是社区公认的坑，但**对 mini-term 不适用**——mini-term sidecar 是原生 `.exe`，`command` 直接指向 exe 即可，不经 npx，无需 `cmd /c` 包装。

---

## Caveats / Not Found

- **热加载时序**：官方文档没有一句正面陈述「修改 `.mcp.json` 需重启会话」。第 4 节结论是基于「`settings.json` 明确写了热加载、`.mcp.json` 完全没写」这一文档对比 + 社区一致实践的强推断。若要 100% 确认，建议在 mini-term 实测：起一个 Claude Code 会话 → 往项目根写 `.mcp.json` → 看 `/mcp` 是否出现新 server。
- **审批弹窗的确切 UI 形态**（是阻塞式 modal 还是 `/mcp` 里的待批列表）文档未逐字描述，只确认「prompts for approval」「one-time approval」「dismiss 后需 `/mcp` 批准」。
- 本调研只覆盖 **Claude Code**。Codex 一侧的项目级 MCP（`~/.codex/config.toml` `[mcp_servers]` 是否支持 per-project）是 PRD 的 Open Question，未在本文件覆盖，需另开调研。
- 文档版本随 Claude Code CLI 持续更新，部分字段标注了最低版本（如 `alwaysLoad` 需 v2.1.121+、`authServerMetadataUrl` 需 v2.1.64+）。本文核心机制（`.mcp.json` schema、审批、作用域优先级）是稳定面，但具体字段以用户实际 CLI 版本为准。

---

## 信息来源（官方文档，2026-05-18 抓取）

- Claude Code — Connect Claude Code to tools via MCP: <https://code.claude.com/docs/en/mcp>
  （`.mcp.json` schema、stdio server 示例、安装作用域、Scope hierarchy and precedence、环境变量展开、project-scoped 审批与 `claude mcp reset-project-choices`、managed MCP）
- Claude Code — Settings: <https://code.claude.com/docs/en/settings>
  （`enableAllProjectMcpServers` / `enabledMcpjsonServers` / `disabledMcpjsonServers` / `allowedMcpServers` / `deniedMcpServers` 设置项；Settings files 对照表中 MCP servers 各作用域存储位置；`~/.claude.json` vs `settings.json` 区别）
- Claude Code — Debug your configuration: <https://code.claude.com/docs/en/debug-your-config>
  （`/mcp` 检查、project-scoped server 一次性审批、`.mcp.json` 必须在仓库根而非 `.claude/` 下、`settings.json` 不读 `mcpServers` key、相对路径失败、`settings.json` env 不传播给 MCP 子进程、`settings.json` 热加载声明）
- Claude Code — Troubleshooting: <https://code.claude.com/docs/en/troubleshooting>
  （「Settings not applying, hooks not firing, MCP servers not loading」指向 debug-your-config）
- 社区排障（佐证「新增 MCP 配置需重启 Claude Code / VS Code」、Windows `npx` 需 `cmd /c` 包装）：
  - "Claude Code MCP server setup on Windows" — automatelab.tech: <https://automatelab.tech/claude-code-mcp-windows-setup/>
  - GitHub issue「[BUG] External MCP Servers Not Loading in Claude Code」、「project-scoped MCP Server not working in Claude Code」（anthropics/claude-code issue tracker，经 DuckDuckGo 检索命中）
