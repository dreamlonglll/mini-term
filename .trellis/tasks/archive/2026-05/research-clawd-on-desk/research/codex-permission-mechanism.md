# Research: Codex 权限处理机制

- **Query**: Codex PermissionRequest 的阻塞等待机制、command hook 与 HTTP hook 的区别
- **Scope**: external (GitHub: rullerzhou-afk/clawd-on-desk)
- **Date**: 2026-05-07

## Findings

### 1. 核心区别: Claude Code vs Codex 的权限处理

| 特性 | Claude Code | Codex |
|------|-------------|-------|
| Hook 类型 | HTTP hook (bidirectional) | Command hook (stdin→stdout) |
| 配置位置 | `~/.claude/settings.json` | `~/.codex/hooks.json` |
| 通信方式 | Claude Code 直接 POST 到 Clawd HTTP 服务器 | Codex 启动 hook 子进程，通过管道传 JSON |
| 阻塞机制 | Claude Code 等待 HTTP 响应 | Codex 等待 hook 进程 stdout 输出并退出 |
| 超时 | HTTP timeout 600s | hook timeout 600s |
| 决策格式 | HTTP response body | 进程 stdout JSON |

### 2. Codex PermissionRequest 阻塞等待的完整流程

```
Codex CLI 需要执行受限操作（如 shell 命令）
    |
    v
Codex CLI 调用 command hook:
  stdin <- { hook_event_name: "PermissionRequest", tool_name, tool_input, ... }
    |
    v
codex-hook.js 启动，读取 stdin (400ms超时)
    |
    v
buildPermissionBody() 识别为 PermissionRequest
    |
    v
requestCodexPermission():
  1. 发现 Clawd 服务器端口 (probe)
  2. POST /permission 到 Clawd 服务器
  3. 等待最长 590 秒
    |
    v
Clawd 桌面宠物显示权限弹窗
  - 显示: tool_name, command, cwd 等信息
  - 用户点击 "Allow" 或 "Deny"
    |
    v
Clawd 服务器返回 HTTP 响应
    |
    v
codex-hook.js 解析响应，输出到 stdout:
  process.stdout.write(JSON.stringify({
    hookSpecificOutput: {
      hookEventName: "PermissionRequest",
      decision: { behavior: "allow" }  // 或 "deny"
    }
  }) + "\n");
  process.exit(0);
    |
    v
Codex CLI 读取 hook 进程的 stdout，解析决策
  - allow -> 执行命令
  - deny -> 拒绝执行
  - {} (空) -> Codex 使用默认行为（可能是自己的交互式提示）
```

### 3. 超时处理

```javascript
const CODEX_PERMISSION_TIMEOUT_MS = 590000;  // ~10分钟

function getCodexPermissionTimeoutMs() {
  // 可通过 CLAWD_CODEX_PERMISSION_TIMEOUT_MS 环境变量自定义
  const raw = Number(process.env.CLAWD_CODEX_PERMISSION_TIMEOUT_MS);
  if (Number.isFinite(raw) && raw > 0) return Math.min(raw, CODEX_PERMISSION_TIMEOUT_MS);
  return CODEX_PERMISSION_TIMEOUT_MS;
}
```

超时链:
- **Codex CLI** 对 hook 进程设置 600 秒超时
- **codex-hook.js** 对 HTTP 请求设置 590 秒超时（留 10 秒缓冲）
- 超时时 hook 进程输出 `{}` (无决策)，Codex CLI 自行处理

### 4. 端口发现与 HTTP 通信

```javascript
// server-config.js
function postPermissionToRunningServer(body, options, callback) {
  // 1. 发现 Clawd 服务器端口
  discover({ timeoutMs: 100 }, (port) => {
    if (!port) {
      callback(false, null, "", 0);  // Clawd 未运行
      return;
    }
    // 2. POST 到 /permission
    postPermissionToPort(port, payload, timeoutMs, (ok, port, responseBody, statusCode) => {
      callback(ok, port, responseBody, statusCode);
    });
  });
}
```

端口候选列表: `23333, 23334, 23335, 23336, 23337`

发现流程:
1. 优先尝试 `~/.clawd/runtime.json` 中记录的端口
2. 回退到默认端口列表
3. 通过 GET /state 验证是否为 Clawd 服务器（检查 `x-clawd-server` header）

### 5. 权限请求体详情

```javascript
{
  agent_id: "codex",
  hook_source: "codex-official",
  session_id: "codex:019d23d4-f1a9-7633-b9c7-758327137228",
  tool_name: "shell_command",      // 或 "exec_command"
  tool_input: {
    command: "npm install ...",
    workdir: "/path/to/project"
  },
  tool_input_description: "Install npm dependencies",  // 截取前 500 字符
  cwd: "/path/to/project",
  turn_id: "...",
  permission_mode: "...",
  transcript_path: "~/.codex/sessions/2026/05/07/rollout-....jsonl",
  model: "gpt-4o",
  tool_use_id: "...",
  tool_input_fingerprint: "sha1hash...",
  // PID 信息
  source_pid: 12345,
  editor: "code",          // 如果是从 VS Code 启动
  agent_pid: 67890,        // codex.exe 的 PID
  pid_chain: [12345, 67890, ...]
}
```

### 6. JSONL 轮询中的 Approval Heuristic

除了 official hook 处理外，codex-log-monitor.js 还有一套基于日志的权限审批推断:

```
response_item:function_call (shell_command/exec_command)
    |
    +-- 有 sandbox_permissions="require_escalated" 或 justification?
    |   YES -> 立即发射 "codex-permission"
    |
    +-- 启动 2 秒定时器
    |
    +-- 2 秒内收到 exec_command_end / function_call_output / guardian_assessment?
    |   YES -> 清除定时器，命令已执行
    |   NO  -> 定时器触发，发射 "codex-permission"
```

这套机制是 official hook 的**补充/回退**，用于：
- Codex 版本不支持 hooks 的情况
- Hook 注册失败的情况
- 远程监控场景

### 7. 子代理角色分类与权限

`codex-subagent-fields.js` 定义了角色分类：

```javascript
const ROLE_ROOT = "root";       // 主会话
const ROLE_SUBAGENT = "subagent"; // 子代理会话
const ROLE_UNKNOWN = "unknown";

// 分类信号源（优先级从高到低）:
// 1. codex_session_role 字段
// 2. source 字段 (subagent=false -> root)
// 3. agent_role 字段
// 4. agent_type 字段
// 5. parent_session_id 存在 -> subagent
// 6. parent_thread_id 存在 -> subagent
```

子代理在 turn-end 时总是映射到 `idle`（不显示 "attention"），因为子代理的工具使用是预期行为。

### 8. 安全约束

权限决策经过严格的清洗:

```javascript
function sanitizeCodexPermissionDecision(decision) {
  // 只允许 "allow" 或 "deny"，其他值被拒绝
  const behavior = decision.behavior === "deny" ? "deny"
    : (decision.behavior === "allow" ? "allow" : null);
  if (!behavior) return null;
  
  const out = { behavior };
  // 只有 deny 时允许附带 message
  if (behavior === "deny" && typeof decision.message === "string") {
    out.message = decision.message;
  }
  return out;
}
```

注释中特别说明: "Keep its output path constrained to behavior/message only; Codex currently fail-closes on several future decision fields."

## 关键文件

| 文件 | 描述 |
|------|------|
| `hooks/codex-hook.js` | 权限请求的入口（stdin→HTTP→stdout） |
| `hooks/server-config.js` | HTTP 通信层（postPermissionToRunningServer） |
| `agents/codex-log-monitor.js` | JSONL 日志中的 approval heuristic |
| `hooks/codex-subagent-fields.js` | 子代理角色分类 |
