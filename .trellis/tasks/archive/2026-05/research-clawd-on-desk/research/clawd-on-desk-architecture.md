# Research: clawd-on-desk 项目架构与 AI 状态检测机制

- **Query**: 分析 clawd-on-desk 如何获取 Claude Code 和 Codex 的事件/状态
- **Scope**: external (GitHub 远程仓库分析)
- **Date**: 2026-05-07

## 一、项目整体架构

### 技术栈

| 维度 | 技术 |
|------|------|
| 语言 | JavaScript (Node.js, CommonJS) |
| 框架 | Electron 41 |
| 打包 | electron-builder |
| 依赖 | electron-updater, htmlparser2, koffi (Win32 FFI) |
| 类型 | 桌面宠物 (desktop pet) 应用 |

### 核心架构特征

clawd-on-desk 是一个 **纯 Electron** 应用（无 Tauri、无 Rust），通过在桌面上放置一个像素风螃蟹宠物来可视化 AI 编码工具的实时状态。

**关键文件结构：**

| 目录/文件 | 职责 |
|-----------|------|
| `agents/` | 每个 AI 工具的配置模块 (event map, process names, capabilities) |
| `agents/registry.js` | agent 注册表，提供按 ID/进程名查找 |
| `hooks/` | 各 AI 工具的 hook 脚本 + 安装器 |
| `hooks/clawd-hook.js` | Claude Code 的 command hook 脚本 |
| `hooks/codex-hook.js` | Codex CLI 的 command hook 脚本 |
| `hooks/install.js` | 自动将 hook 注册到 `~/.claude/settings.json` |
| `hooks/server-config.js` | 本地 HTTP 服务器配置 (端口发现、状态推送) |
| `hooks/shared-process.js` | 进程树遍历、stdin 读取、平台配置 |
| `src/server.js` | HTTP 服务器 (`127.0.0.1:23333`)，路由 `/state` 和 `/permission` |
| `src/server-route-state.js` | `/state` POST 处理器 |
| `src/state.js` | 状态机 + 多会话管理 + 睡眠序列 |
| `src/main.js` | Electron 主进程入口 |
| `src/renderer.js` | SVG 渲染 + 眼球追踪 |
| `src/permission.js` | 权限气泡窗口管理 |
| `agents/codex-log-monitor.js` | Codex JSONL 日志增量轮询 |
| `src/claude-settings-watcher.js` | 监控 `~/.claude/settings.json` 变化 |

## 二、核心机制：Claude Code 状态获取

### 2.1 Claude Code Command Hooks (主要机制)

clawd-on-desk 使用 Claude Code 的 **官方 hook 系统** 来获取状态。Claude Code 支持在 `~/.claude/settings.json` 中注册 hook，当特定事件发生时执行对应命令。

**注册过程 (`hooks/install.js`):**

启动时自动将 hook 命令写入 `~/.claude/settings.json`：

```json
{
  "hooks": {
    "SessionStart": [{ "type": "command", "command": "node /path/to/clawd-hook.js SessionStart" }],
    "SessionEnd": [{ "type": "command", "command": "node /path/to/clawd-hook.js SessionEnd" }],
    "UserPromptSubmit": [{ "type": "command", "command": "node /path/to/clawd-hook.js UserPromptSubmit" }],
    "PreToolUse": [{ "type": "command", "command": "node /path/to/clawd-hook.js PreToolUse" }],
    "PostToolUse": [{ "type": "command", "command": "node /path/to/clawd-hook.js PostToolUse" }],
    "PostToolUseFailure": [{ "type": "command", "command": "node /path/to/clawd-hook.js PostToolUseFailure" }],
    "Stop": [{ "type": "command", "command": "node /path/to/clawd-hook.js Stop" }],
    "SubagentStart": [{ "type": "command", "command": "node /path/to/clawd-hook.js SubagentStart" }],
    "SubagentStop": [{ "type": "command", "command": "node /path/to/clawd-hook.js SubagentStop" }],
    "Notification": [{ "type": "command", "command": "node /path/to/clawd-hook.js Notification" }],
    "Elicitation": [{ "type": "command", "command": "node /path/to/clawd-hook.js Elicitation" }],
    "PreCompact": [{ "type": "command", "command": "node /path/to/clawd-hook.js PreCompact" }],
    "PostCompact": [{ "type": "command", "command": "node /path/to/clawd-hook.js PostCompact" }],
    "StopFailure": [{ "type": "command", "command": "node /path/to/clawd-hook.js StopFailure" }],
    "PermissionRequest": [{ "type": "http", "url": "http://127.0.0.1:23333/permission" }]
  }
}
```

**hook 脚本执行流程 (`hooks/clawd-hook.js`):**

```
Claude Code 触发事件
  -> 执行 `node clawd-hook.js <EventName>`
  -> 通过 stdin 读取 JSON payload (session_id, cwd, tool_name 等)
  -> 遍历进程树获取 terminal PID / agent PID / editor 信息
  -> 构建 state body: { state, session_id, event, agent_id, source_pid, cwd, ... }
  -> HTTP POST 到 127.0.0.1:23333/state
  -> Clawd 主进程接收并更新状态机
```

**事件到状态映射 (`agents/claude-code.js`):**

```javascript
eventMap: {
  SessionStart: "idle",
  SessionEnd: "sleeping",
  UserPromptSubmit: "thinking",
  PreToolUse: "working",
  PostToolUse: "working",
  PostToolUseFailure: "error",
  Stop: "attention",
  StopFailure: "error",
  SubagentStart: "juggling",
  SubagentStop: "working",
  PreCompact: "sweeping",
  PostCompact: "attention",
  Notification: "notification",
  Elicitation: "notification",
  WorktreeCreate: "carrying",
}
```

### 2.2 HTTP Permission Hook (阻塞式)

PermissionRequest 使用 HTTP hook（而非 command hook），因为它是**阻塞式**的：

```
Claude Code 请求权限
  -> HTTP POST 127.0.0.1:23333/permission { tool_name, tool_input, session_id, permission_suggestions }
  -> Clawd 创建 BrowserWindow 弹出权限气泡卡片
  -> 用户点击 Allow / Deny
  -> HTTP 响应 { behavior: "allow" | "deny" }
  -> Claude Code 按用户决策继续/终止
```

### 2.3 进程树遍历 (`hooks/shared-process.js`)

每次 hook 触发时，脚本会遍历进程树来识别上下文信息：

```javascript
// createPidResolver: 从 process.ppid 开始向上遍历最多 8 层
// Windows: wmic process where ProcessId=... get Name,ParentProcessId/CommandLine
// macOS/Linux: ps -o ppid= -p PID / ps -o comm= -p PID

// 识别的信息:
// - terminalPid: 终端应用的 PID (Windows Terminal, VS Code 等)
// - agentPid: AI 工具的 PID (claude.exe, codex.exe 等)
// - detectedEditor: 编辑器类型 ("code" | "cursor")
// - pidChain: 完整进程链
```

### 2.4 Settings 文件监控 (`src/claude-settings-watcher.js`)

Clawd 还会监控 `~/.claude/settings.json` 的变化：

- 使用 `fs.watch` 监控 `~/.claude/` 目录
- 当 settings.json 被修改时，检查 hook 是否还存在
- 如果 hook 被其他工具（如 CC-Switch）覆盖，自动重新注册
- 防抖 1s，限流 5s

## 三、核心机制：Codex CLI 状态获取

Codex 使用**双路策略**：official hooks (主要) + JSONL log polling (降级)。

### 3.1 Codex Official Hooks

类似 Claude Code，但注册在 `~/.codex/hooks.json`：

```javascript
// hooks/codex-hook.js
EVENT_TO_STATE = {
  SessionStart: "idle",
  UserPromptSubmit: "thinking",
  PreToolUse: "working",
  PostToolUse: "working",
  Stop: "idle", // server.js 会根据 turn 是否有 tool use 决定 attention/idle
}
```

Codex 的 PermissionRequest 也是 command hook，但 hook 脚本会挂起等待，将 `/permission` 的响应转为 stdout JSON decision。

### 3.2 Codex JSONL Log Polling (降级机制，`agents/codex-log-monitor.js`)

当 official hooks 不可用时（旧版本 Codex、hooks 禁用等），使用文件轮询：

```
轮询目录: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
轮询间隔: 1500ms
增量读取: 记录每个文件的 offset，只读新增内容
```

**JSONL 事件映射:**

```javascript
logEventMap: {
  "session_meta": "idle",
  "event_msg:task_started": "thinking",
  "event_msg:user_message": "thinking",
  "response_item:function_call": "working",
  "event_msg:exec_command_end": "working",
  "event_msg:task_complete": "codex-turn-end",
  "event_msg:context_compacted": "sweeping",
  "event_msg:turn_aborted": "idle",
}
```

**Approval Heuristic (权限推断):**

Codex JSONL 没有显式的权限等待事件，所以使用了启发式推断：
1. 当出现 `response_item:function_call` 且包含 shell 命令时
2. 启动 2000ms 定时器
3. 如果 2s 内没有 `exec_command_end`，推断为等待用户批准
4. 发出 `codex-permission` 状态

**重放保护 (两层):**
1. 行级别：跳过 timestamp 早于 monitor 启动的条目
2. 文件级别：首次读取旧文件标记为 backfilling，静默回放后仅合成一次当前状态

## 四、状态管理架构

### 4.1 本地 HTTP 服务器 (`src/server.js`)

```
监听: 127.0.0.1:23333 (备选 23334-23337)
路由:
  GET  /state      -> 健康检查
  POST /state      -> 接收 hook 状态上报
  POST /permission -> 接收权限请求
运行时配置: ~/.clawd/runtime.json (记录当前端口)
```

### 4.2 状态机 (`src/state.js`)

- 多会话追踪：每个 session_id 独立维护状态
- 优先级解析：error > notification > working > thinking > idle > sleeping
- 最小显示时长：每个状态有最小持续时间，避免闪烁
- 睡眠序列：60s 无活动 -> yawn -> doze -> collapse -> sleep
- DND 模式：静默所有 hook 事件
- 进程存活检测：检测已崩溃/退出的 agent 进程并清理孤立 session

### 4.3 多 Agent 支持

支持的 AI 工具：

| Agent | 检测机制 | 进程名 |
|-------|----------|--------|
| Claude Code | command hooks + HTTP permission hook | claude.exe / claude |
| Codex CLI | official hooks + JSONL polling | codex.exe / codex |
| Copilot CLI | command hooks | copilot-cli |
| Gemini CLI | command hooks | gemini |
| Cursor Agent | command hooks | cursor |
| CodeBuddy | command hooks + HTTP permission | codebuddy |
| Kiro CLI | per-agent hooks | kiro-cli |
| Kimi CLI | hook via config.toml | kimi |
| opencode | in-process plugin | opencode |

## 五、与 mini-term 的 process_monitor.rs 对比

### mini-term 的方式

mini-term 使用**进程轮询 (process polling)** 方式：

```rust
// process_monitor.rs: 500ms 定时轮询
// 1. 遍历所有 PTY 实例
// 2. pty_manager.is_ai_session(pty_id) — 检查子进程名是否包含 "claude" / "codex"
// 3. pty_manager.has_recent_output(pty_id, 3s) — 检查最近 3s 是否有输出
// 4. 状态: ai-working (有输出) / ai-idle (无输出) / idle (非AI)
```

### 对比表

| 维度 | mini-term | clawd-on-desk |
|------|-----------|---------------|
| **检测方式** | 进程名轮询 + PTY 输出监控 | 官方 hook 系统 (事件驱动) |
| **延迟** | 500ms 轮询间隔 + 3s 活跃超时 | 接近零延迟 (hook 即时触发) |
| **状态粒度** | 3 种 (idle, ai-working, ai-idle) | 12+ 种 (idle, thinking, working, building, juggling, conducting, error, happy, notification, sweeping, carrying, sleeping) |
| **事件来源** | PTY 子进程名匹配 | Claude Code 官方 hook API |
| **权限检测** | 无 | HTTP hook 弹出审批气泡 |
| **Subagent 感知** | 无 | SubagentStart/SubagentStop 事件 |
| **工具使用识别** | 无 (只能看到有/无输出) | PreToolUse/PostToolUse 精确知道工具名称 |
| **上下文信息** | 仅 PTY ID | session_id, cwd, tool_name, editor, PID chain |
| **多 Agent** | 不区分 (只看进程名包含 claude/codex) | 独立追踪每个 agent 的每个 session |
| **实现复杂度** | 低 (~50 行 Rust) | 高 (数千行 JS，HTTP 服务器 + hook 安装器 + 文件监控) |
| **对 AI 工具的依赖** | 无 (被动观察) | 强依赖 Claude Code 的 hook API |
| **工作前提** | AI 工具必须在 mini-term 的 PTY 里运行 | AI 工具可以在任何终端里运行 |

### 核心差异总结

1. **被动 vs 主动**：mini-term 是被动观察者（轮询进程 + 看输出），clawd-on-desk 是通过官方 API 获得主动通知
2. **耦合度**：mini-term 与 AI 工具零耦合，clawd-on-desk 深度依赖各工具的 hook 系统
3. **信息量**：mini-term 只能判断"AI 在不在运行/有没有输出"，clawd-on-desk 能获得精确的生命周期事件
4. **适用场景**：mini-term 是终端管理器（AI 在其内运行），clawd-on-desk 是独立的桌面宠物（全局监控任意终端）

## Caveats / 注意事项

- clawd-on-desk 的 hook 机制依赖 Claude Code v2.1+ 的 settings.json hook 功能
- hook 注册会修改用户的 `~/.claude/settings.json`，可能与其他使用 hook 的工具冲突
- Codex JSONL 轮询的 approval heuristic 是启发式的（2s 超时），可能有误判
- clawd-on-desk 的方案不适合直接移植到 mini-term，因为 mini-term 需要 PTY 级别的集成而非全局监控
- Claude Code 的 hook API 不是公开稳定的 API，可能在未来版本中变化
