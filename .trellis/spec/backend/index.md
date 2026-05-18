# 后端开发规范（Rust / Tauri）

> `src-tauri/` 的后端编码规范。

---

## 规范索引

| 文档 | 说明 |
|------|------|
| [Agent 配置注入](./agent-config-injection.md) | 幂等读写 Claude Code / Codex 的外部配置文件（hooks、MCP server 注册） |

---

## 约定

### 约定：tauri-free 共享 crate `mt-core`

**What**：凡是「Tauri app 主体」与「独立 sidecar 二进制」都要用的逻辑（共享类型、纯函数、配置读取），放进 `src-tauri/mt-core/` 这个**不依赖 `tauri`** 的库 crate，两边以路径依赖共用。

**Why**：sidecar bin（如 `mt-ssh-mcp`，及未来其它）若 `use tauri_app_lib` 会链接整个 Tauri（webview 等），体积与编译时间不可接受。`mt-core` 不依赖 tauri，sidecar 依赖它即可拿到共享逻辑而不背 Tauri。

**已在 `mt-core` 的内容**：`SshConnection` 类型、`scan_ssh_prompt` / `strip_ansi_codes`、`prepare_ssh_key` 纯逻辑、`config.json` 读取（`read_ssh_connections` / `config_json_path`）。

**注意**：`mt-core` 没有 `AppHandle`，定位 `config.json` 之类的路径要用 `dirs` crate 自行按平台拼（镜像 `src-tauri/src/bin/miniterm-hook.rs` 的平台分支），不能用 Tauri 的 `app.path()`。

---

## Gotchas

> **`BatchMode=yes` 会连带禁用 SSH 密码认证。**
>
> 给 `ssh` 拼参数时，`-o BatchMode=yes`（让密钥 / agent 认证失败时立即返回、不挂起）会**同时禁掉密码认证**。需要 PTY autofill 灌密码的连接绝不能带 `BatchMode=yes`。按认证类型区分：密码型传 `batch=false`，密钥 / agent 型传 `batch=true`。见 `src-tauri/src/bin/mt-ssh-mcp.rs` 的 `build_ssh_args`。

> **stdio MCP sidecar 的 stdout 只能输出协议消息。**
>
> `mt-ssh-mcp` 这类 stdio MCP server，进程自身 stdout 仅允许 MCP 协议 JSON；任何日志 / 调试输出一律走 stderr（`eprintln!`）。子进程（如 `ssh`、`icacls`）的输出必须捕获进返回值或 `Stdio::null()`，绝不能透传到本进程 stdout，否则破坏协议。
