# Research: Codex CLI 如何配置 MCP Server（按项目隔离）

- **Query**: Codex CLI 配置 MCP server 的机制、文件位置、确切 TOML 格式；是否支持项目级 / 工作目录级 MCP 配置（项目隔离）；若只能全局有哪些变通方案；stdio MCP 启动机制与 Windows 坑。
- **Scope**: external（Codex 官方行为）+ 本地实证验证
- **Date**: 2026-05-18
- **验证方式**: 本机已安装 `codex-cli 0.130.0`（npm `@openai/codex@0.130.0`，Rust 原生二进制 `codex.exe` 224MB）。本研究**不是**纯文档推断——直接用真实 `codex` 命令做了端到端实验，并从 `codex.exe` 二进制里提取了 schema 字符串。结论可信度高。

---

## 核心结论（先看这个）

**Codex CLI v0.130.0 原生支持项目级 / 工作目录级 MCP 配置。** 项目隔离**不需要任何变通方案**，机制和 Claude Code 的 `.mcp.json` 几乎对等：

- 项目根放一个 **`<project-dir>/.codex/config.toml`**，里面写 `[mcp_servers.xxx]`。
- 只有「当前工作目录在该项目目录树内」的 Codex 会话才能看到这个 MCP server。其它项目看不到。
- 这是 Codex 的 **config layer（配置分层）** 机制：`User`（`~/.codex/config.toml`）→ `Project`（`<project>/.codex/config.toml`）逐层叠加，项目层覆盖用户层。
- **前提：项目目录必须被 Codex「信任」（trusted）**。未信任的目录，其 `.codex/config.toml` 会被完全忽略（hooks / exec policy 同理）。

→ 对 mini-term 的 SSH MCP 功能：**与 Claude Code 写 `.mcp.json` 完全对称**——在项目目录写 `.codex/config.toml`，停用时删掉即可。唯一额外要求是确保该项目在 Codex 里是 trusted（见下文「坑 5」）。

---

## Findings

### 1. Codex MCP server 配置机制与文件位置

#### 配置文件层级（config layers）

Codex 把配置分成多层（从二进制提取的 `ConfigLayerSource` 枚举确认）：

| Layer | 文件位置 | 优先级 |
|---|---|---|
| `Mdm` / `LegacyManagedConfigTomlFromFile` | 企业 MDM 管控配置（`managed_config.toml`） | 最高 |
| **`Project`** | **`<工作目录或其祖先目录>/.codex/config.toml`** | 中（覆盖 User） |
| `User` | `~/.codex/config.toml`（受 `CODEX_HOME` 重定向） | 低 |
| `System` | 系统级 `config.toml` | 最低 |
| 命令行 `-c key=value` | 运行时临时覆盖 | 叠加在最上面，不落盘 |

二进制内的实证字符串：
- `struct variant ConfigLayerSource::Project with 1 element`（与 `User` / `System` / `Mdm` 并列）
- `Optional working directory to resolve project config layers. If specified, return the effective config as seen from that directory (i.e., including any project layers between cwd and the project/repo root).`
- `project layers are not ordered from root to cwd`（项目层支持多级，从仓库根向下到 cwd 逐级）
- `Ignored unsupported project-local config keys in <path>`（项目层只接受一部分 key——但 `mcp_servers` 是被接受的，已实测）
- 信任弹窗原文：`Trusting the directory allows project-local config, hooks, and exec policies to load.`
- `Project-local config, hooks, and exec policies are disabled in the following folders until the project is trusted, but skills still load.`

#### MCP server 配置写在 `[mcp_servers]` 表

确认：MCP server 用 `[mcp_servers.<名字>]` 子表配置，可写在用户层 `~/.codex/config.toml`，也可写在项目层 `<project>/.codex/config.toml`。

二进制内的 schema 字符串：`struct RawMcpServerConfig with 24 elements`，含字段：
`command` / `args` / `env` / `env_vars` / `cwd` / `startup_timeout_sec` / `tool_timeout_sec` / `enabled` / `enabled_tools` / `disabled_tools` / `bearer_token_env_var` / `experimental_environment` / `oauth_resource` /（已弃用：`startup_timeout_ms`、`bearer_token`）。

`codex mcp get` 实测返回的 stdio server JSON 结构（确切字段名）：

```json
{
  "name": "miniterm-ssh",
  "enabled": true,
  "disabled_reason": null,
  "transport": {
    "type": "stdio",
    "command": "C:\\Program Files\\mini-term\\miniterm-ssh-mcp.exe",
    "args": ["--project-id", "proj2-uuid"],
    "env": { "MINITERM_PROJECT_ID": "proj2-uuid" },
    "env_vars": [],
    "cwd": "C:\\Users\\...\\proj2"
  },
  "enabled_tools": null,
  "disabled_tools": null,
  "startup_timeout_sec": 30.0,
  "tool_timeout_sec": 120.0
}
```

### 2. 一个 stdio MCP server 条目的确切 TOML 格式（已实测 round-trip）

写进 **`<project-dir>/.codex/config.toml`**（项目隔离）或 `~/.codex/config.toml`（全局）：

```toml
[mcp_servers.miniterm-ssh]
# stdio server 必填：可执行文件路径（或 PATH 中的命令名）
command = "C:\\Program Files\\mini-term\\miniterm-ssh-mcp.exe"
# 可选：命令行参数数组
args = ["--project-id", "proj2-uuid"]
# 可选：MCP server 进程的工作目录（绝对路径）
cwd = "D:\\Git\\some-project"
# 可选：握手/启动超时秒数（默认值见「坑 3」）
startup_timeout_sec = 30
# 可选：单个工具调用超时秒数
tool_timeout_sec = 120

# 可选：环境变量——必须是独立子表 [mcp_servers.<名字>.env]
[mcp_servers.miniterm-ssh.env]
MINITERM_PROJECT_ID = "proj2-uuid"
```

字段说明（全部已实测确认字段名）：

| TOML 字段 | 类型 | 说明 |
|---|---|---|
| `command` | string | **必填**。stdio MCP server 可执行文件路径或 PATH 命令名 |
| `args` | string 数组 | 可选。命令行参数 |
| `env` | 子表 `[mcp_servers.<名>.env]` | 可选。注入给 server 进程的环境变量。**注意是 TOML 子表，不是 inline `command` 同级的 key** |
| `cwd` | string | 可选。server 进程的工作目录（用绝对路径） |
| `startup_timeout_sec` | number | 可选。启动/握手超时秒数 |
| `tool_timeout_sec` | number | 可选。单次工具调用超时秒数 |
| `enabled` | bool | 可选。`false` 可禁用而不删除条目（实测 `enabled = false` 生效） |
| `enabled_tools` / `disabled_tools` | 数组 | 可选。工具白/黑名单 |

> 注意：早期 Codex 文档里出现过的 `startup_timeout_ms` 已被 `startup_timeout_sec` 取代（二进制里两者都在，`_ms` 标注为遗留兼容）。新代码用 `_sec`。

#### 用 `codex mcp` 子命令管理（CLI 角度）

Codex 0.130.0 有完整的 `codex mcp` 子命令组，可作为「Codex 官方写法」的参考，但**对本项目隔离需求不直接可用**（见下条）：

```
codex mcp list [--json]            # 列出当前 cwd 视角下生效的所有 MCP server
codex mcp get <NAME> [--json]      # 查看单个 server 配置
codex mcp add <NAME> --env K=V -- <command> [args...]   # 新增 stdio server
codex mcp add <NAME> --url <URL>   # 新增 streamable HTTP server
codex mcp remove <NAME>            # 删除
```

**关键限制（实测）：`codex mcp add` 永远写入全局用户 `~/.codex/config.toml`。** 即使在某个项目目录里运行，它也提示 `Added global MCP server 'xxx'.`，并把条目写进用户层 config，不碰项目层 `.codex/config.toml`。
→ 所以 mini-term **不能靠 `codex mcp add` 实现项目隔离**，必须像写 Claude `.mcp.json` 那样，自己用 `toml_edit` 直接读写 `<project>/.codex/config.toml`。

> `codex mcp add` 用 `--` 后跟命令的形式；它不接受 `-c mcp_servers.x.cwd=...` 这种内联覆盖来设 cwd（实测会报 `invalid transport`）。所以自写 TOML 反而更直接。

### 3. 关键问题：Codex 支持项目级 / 工作目录级 MCP 配置吗？——**支持**

#### 实测验证（codex-cli 0.130.0，Windows）

做了 5 组对照实验，全部通过：

| 实验 | 设置 | 结果 |
|---|---|---|
| A | trusted git 项目 `proj2`，放 `proj2/.codex/config.toml` 含 `mcp_servers.projlocal` | 在 `proj2` 内 `codex mcp list` 同时看到 `projlocal`（项目层）+ `userglobal`（用户层）✅ |
| B | 切到**另一个**项目 `proj3`（无项目 config） | `codex mcp list` **只看到 `userglobal`，看不到 `projlocal`** → 项目隔离成立 ✅ |
| C | **未信任**的项目 `proj4`，放了 `.codex/config.toml` 含 `untrustedlocal` | `untrustedlocal` **不加载** → 项目层受 trust 门控 ✅ |
| D | 用户层和项目层同名 server `userglobal` | 解析结果取**项目层的值** → 项目层覆盖用户层 ✅ |
| E | **非 git** 的普通目录 `proj5`（trusted） | `proj5/.codex/config.toml` 的 `nongitlocal` 照样加载 → 项目层只看目录，不依赖 git ✅ |

结论：

1. **项目配置文件 = `<工作目录或其祖先>/.codex/config.toml`**。Codex 从 cwd 向上找 `.codex/config.toml`（git 仓库根会作为边界，但非 git 目录也支持）。
2. **天然项目隔离**：项目 A 的 `.codex/config.toml` 里的 MCP server，项目 B 的 Codex 会话看不到。
3. **同名时项目层覆盖用户层**。
4. **前提：目录必须被 Codex 信任**（`~/.codex/config.toml` 里 `[projects."<绝对路径>"] trust_level = "trusted"`，或首次进入时交互信任）。

#### 其它隔离手段（也支持，但项目级 `.codex/config.toml` 已是最优解，仅作备选）

- **`CODEX_HOME` 环境变量**：可重定向整个配置目录（实测 `CODEX_HOME=/path codex ...` 生效，连 `~/.codex/config.toml` 一起换）。理论上可给每个项目一个独立 `CODEX_HOME`，但太重（要复制 auth/skills/history 等全部），**不推荐**。
- **`-c key=value` / `--config`**：运行时临时注入，可注入完整 MCP server（实测 `-c 'mcp_servers.x.command="echo"' -c 'mcp_servers.x.args=["hi"]'` 会让 `x` 出现在 `mcp list` 里且**不落盘**）。但 mini-term 不直接控制 Codex 启动命令行（Codex 是用户在终端里手敲的），所以**用不上**。
- **profiles（`[profiles.xxx]`）**：`config.toml` 的 profile 机制，靠 `codex -p <profile>` 选择。实测 `codex mcp` 子命令**不接受 `-p`**；且 profile 是用户手动选的，不是按目录自动切。`mcp_servers` 也不是 per-profile 覆盖的典型字段。**不适合做项目隔离**。

### 4. 「按项目隔离」对 Codex 的实现建议

既然 Codex 原生支持，方案与 Claude Code 对称、且无需变通：

**推荐方案（与 Claude `.mcp.json` 完全对称）**

- 用户在项目 X 上「启用 SSH MCP」时：
  - Claude Code：写 `X/.mcp.json` 的 `mcpServers.miniterm-ssh`（已是 PRD 计划）。
  - Codex：写 `X/.codex/config.toml` 的 `[mcp_servers.miniterm-ssh]`（**自己用 `toml_edit` 读写**，不要用 `codex mcp add`）。
- 停用时：从这两个文件移除对应条目（marker 幂等，对标 `hook_registry.rs`）。
- 额外一步：**确保项目 X 在 Codex 里 trusted**。可在写 `.codex/config.toml` 的同时，在 `~/.codex/config.toml` 写 `[projects."<X 绝对路径>"] trust_level = "trusted"`（用户现有 `~/.codex/config.toml` 已有大量这种条目，见「坑 5」），否则未信任项目里 Codex 会忽略整个项目层。是否替用户自动写信任，建议作为一个 Open Question 让用户拍板（自动写信任 = 替用户降低一道安全确认）。

这样就不需要「全局注册 + server 按 cwd 自判项目」之类的脏变通。仅当未来发现 Codex 项目层不可靠时，才回退到下面的备选：

**备选变通（仅当项目层方案失效时）**

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| B1 全局注册 + cwd 自判 | 在 `~/.codex/config.toml` 注册一个全局 `miniterm-ssh`，server 启动时读自己的 `cwd`，反查 mini-term `config.json` 里哪个项目 `path` 匹配，未启用则不暴露任何工具 | 一处注册；server 自带项目门控 | 全局可见（所有项目的 Codex 都加载这个 server 进程，只是没工具）；逻辑在 server 侧 |
| B2 全局注册 + 项目层 `enabled` 覆盖 | 全局注册 `miniterm-ssh` 后默认 `enabled = false`；启用的项目在 `<project>/.codex/config.toml` 写 `[mcp_servers.miniterm-ssh] enabled = true` 覆盖 | 利用「项目层覆盖用户层」+ `enabled` 字段；少写完整条目 | 仍要写项目层文件，不比推荐方案省事 |
| B3 per-project `CODEX_HOME` | 每个项目独立 `CODEX_HOME` 目录 | 完全隔离 | 极重，要同步 auth/history/skills，基本不可行 |

→ 直接用「推荐方案」，B1/B2/B3 仅备查。

### 5. Codex 如何发现并启动 stdio MCP server，有哪些坑

1. **发现机制**：Codex 启动时加载 config layers，把所有 `[mcp_servers.*]` 收集成 server 列表。stdio server 在会话开始时被 spawn，做 MCP 握手。
2. **命令解析**：`command` 既可以是绝对路径，也可以是 PATH 中的命令名（如用户 config 里的 `command = "npx"`）。**Windows 上建议给 mini-term 的 sidecar 用绝对路径 + `.exe` 后缀**（如 `C:\Program Files\mini-term\miniterm-ssh-mcp.exe`），避免 PATH 解析问题。
3. **启动超时坑**：握手有超时，超时报错原文 `... seconds. Add or adjust startup_timeout_sec in your config.toml`。sidecar 必须**启动后立即进入 MCP stdio 协议**，别在启动时做耗时初始化（如建 SSH 连接），否则会被判超时。SSH 连接应延迟到工具调用时再建。必要时在 TOML 里调大 `startup_timeout_sec`。
4. **stdio 铁律**：sidecar 的 **stdout 只能输出 MCP 协议 JSON-RPC**，任何日志/调试信息必须走 stderr，否则破坏协议握手。Codex 会单独读 server stderr（二进制里有 `Failed to read MCP server stderr`）。这条与 PRD 的「stdio 铁律」一致。
5. **TOML 中 Windows 路径转义坑**：basic string（双引号）里反斜杠要转义成 `\\`，例：`command = "C:\\Program Files\\mini-term\\miniterm-ssh-mcp.exe"`。**实测**：写成单反斜杠会让 Codex 直接报 `TOML parse error ... missing escaped value`，整个 config 加载失败。替代法：用 TOML literal string（单引号）`command = 'C:\Program Files\mini-term\miniterm-ssh-mcp.exe'` 不需要转义。用 `toml_edit` 的 `toml_edit::value(path_string)` 生成时它会自动正确转义，推荐走 `toml_edit`。
6. **trust 门控坑（最关键）**：`<project>/.codex/config.toml` 只有在该项目被 Codex「信任」后才生效。未信任 → 项目层（含 `mcp_servers`、hooks、exec policy）**全部被静默忽略**，只有 skills 仍加载。信任状态存在 `~/.codex/config.toml` 的 `[projects."<绝对路径>"]` 表里（`trust_level = "trusted"`）。本机用户的 `~/.codex/config.toml` 里已经有十几个 `[projects."..."]` 条目，包括 `[projects."\\?\D:\Git\mini-term"]`——注意 Codex 可能用 `\\?\` 前缀的扩展长路径形式存路径，写信任时要小心路径规范化匹配。
7. **`codex mcp add` 不可用于项目隔离**：见 §2 末，它只写全局。mini-term 要自己写项目层 `.codex/config.toml`。
8. **目录边界**：项目层是「从 cwd 向上找 `.codex/config.toml`」。把文件放在项目根目录的 `.codex/` 子目录即可；子目录里运行 Codex 也能继承（向上查找）。

### Files Found（本仓库相关代码）

| File Path | Description |
|---|---|
| `src-tauri/src/hook_registry.rs` | 现有「mini-term 写 agent 配置」的先例。`codex_config_path()`（`hook_registry.rs:69`）已返回 `~/.codex/config.toml`；`ensure_codex_hooks_feature()`（`hook_registry.rs:263-294`）已用 `toml_edit::DocumentMut` 解析+改 `[features]` 段并写回。**SSH MCP 写 `.codex/config.toml` 的 `[mcp_servers]` 可直接复用同一套 `toml_edit` 写法**，只是目标路径从 `~/.codex/config.toml` 换成 `<project>/.codex/config.toml` |
| `src-tauri/src/hook_registry.rs:42-56` | `get_hook_binary_path()` —— 解析与当前 exe 同目录的兄弟 bin 路径。SSH MCP sidecar 的 `command` 路径可仿此实现 |
| `.trellis/tasks/05-18-ssh-mcp-server/prd.md` | 本任务 PRD。其中 assumption「Codex 的 MCP 配置可能是全局的——按项目隔离对 Codex 需要额外方案」**可据本研究更正为：Codex 原生支持项目级 `.codex/config.toml`，无需额外方案** |

### Code Patterns（现有 `toml_edit` 写法，可复用）

`hook_registry.rs:281-291` 已经演示了 mini-term 改 Codex `config.toml` 的标准写法：

```rust
let mut doc: toml_edit::DocumentMut = content.parse::<toml_edit::DocumentMut>()?;
if doc.get("features").is_none() {
    doc["features"] = toml_edit::Item::Table(toml_edit::Table::new());
}
doc["features"]["codex_hooks"] = toml_edit::value(true);
std::fs::write(&config_path, doc.to_string())?;
```

写 `[mcp_servers.miniterm-ssh]` 同理：操作 `doc["mcp_servers"]["miniterm-ssh"]`，子字段 `command` / `args` / `env` 用 `toml_edit::value(...)` 与 `toml_edit::Array` / 子 `Table` 构造；`env` 要建成子表 `doc["mcp_servers"]["miniterm-ssh"]["env"]`。幂等更新/移除对标 `hook_registry.rs` 里的 marker 思路。

### External References

- Codex CLI 配置文档（官方）：`https://github.com/openai/codex/blob/main/docs/config.md` — 含 `[mcp_servers]` 说明（注：本研究因当前环境无 web 工具，未能在线打开该页；结论改用本机 `codex-cli 0.130.0` 二进制实测 + 二进制字符串提取得到，比文档更贴近实际行为）。
- Codex feature flags 文档（二进制内引用）：`https://developers.openai.com/codex/config-basic#feature-flags`
- 本机二进制证据来源：`@openai/codex@0.130.0` 原生二进制
  `.../@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/codex/codex.exe`
  （提取出 `ConfigLayerSource::Project`、`RawMcpServerConfig with 24 elements`、trust 门控文案等）。
- 本机用户实际配置 `~/.codex/config.toml` 已有可用样例：
  ```toml
  [mcp_servers]
  [mcp_servers.context7]
  command = "npx"
  args = ["@upstash/context7-mcp"]
  ```

### Related Specs

- `.trellis/spec/frontend/`、`.trellis/spec/guides/` — 目录存在但未含与 Codex/MCP 配置相关的 spec。

## Caveats / Not Found

- 本研究基于本机 **codex-cli 0.130.0**。Codex 迭代很快，未来版本 `.codex/config.toml` 项目层行为或 `RawMcpServerConfig` 字段可能变化；建议实现时在目标机器跑一次 `codex mcp list --json`（在某个 trusted 项目目录内、且该目录有 `.codex/config.toml`）做冒烟验证。
- 当前对话环境无 `WebSearch` / `WebFetch` / Exa / context7 工具可用，未能在线核对 Codex 官方 `docs/config.md` 原文。已用「本机真实二进制端到端实验 + 二进制 schema 字符串提取」替代，可信度高于文档推断，但官方文档对个别边角（如项目层接受的完整 key 白名单——二进制提示 `Ignored unsupported project-local config keys`，已确认 `mcp_servers` 在白名单内）描述可能更全。
- 项目层「从 cwd 向上查找」的**确切上界**（git 根 vs. 文件系统根 vs. 首个含 `.codex` 的目录）未逐字节确认；实测 git 项目与非 git 目录都能加载位于该目录的 `.codex/config.toml`。mini-term 把文件放在项目根的 `.codex/` 下即可，无需关心更深层细节。
- 未验证 streamable HTTP 类型 MCP server 的 TOML（`url` / `bearer_token_env_var`）——本任务 sidecar 是 stdio 拓扑（PRD 拓扑 A1），HTTP 不在范围内。
