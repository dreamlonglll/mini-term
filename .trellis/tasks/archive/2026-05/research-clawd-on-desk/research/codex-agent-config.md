# Research: Codex Agent 配置

- **Query**: agents/codex.js 完整配置及支持类
- **Scope**: external (GitHub: rullerzhou-afk/clawd-on-desk)
- **Date**: 2026-05-07

## Findings

### 1. agents/codex.js 完整配置

```javascript
module.exports = {
  id: "codex",
  name: "Codex CLI",
  
  // 进程名识别
  processNames: { win: ["codex.exe"], mac: ["codex"], linux: ["codex"] },
  
  // 事件源: 双通道
  eventSource: "hook+log-poll",
  
  // Official hook 事件映射（hook_event_name -> 状态）
  eventMap: {
    SessionStart: "idle",
    UserPromptSubmit: "thinking",
    PreToolUse: "working",
    PermissionRequest: "notification",
    PostToolUse: "working",
    Stop: "codex-turn-end",         // 特殊值，由 server.js 解析
  },
  
  // JSONL 日志事件映射（type:subtype -> 状态）
  logEventMap: {
    "session_meta":                        "idle",
    "event_msg:task_started":              "thinking",
    "event_msg:user_message":              "thinking",
    "event_msg:agent_message":             null,        // 忽略
    "event_msg:guardian_assessment":        "working",
    "event_msg:exec_command_end":          "working",
    "event_msg:patch_apply_end":           "working",
    "event_msg:custom_tool_call_output":   "working",
    "response_item:function_call":         "working",
    "response_item:custom_tool_call":      "working",
    "response_item:web_search_call":       "working",
    "event_msg:task_complete":             "codex-turn-end",
    "event_msg:context_compacted":         "sweeping",
    "event_msg:turn_aborted":             "idle",
  },
  
  // 能力声明
  capabilities: {
    httpHook: false,                // Codex 不使用 HTTP hook
    permissionApproval: true,       // 支持权限审批
    interactiveBubble: true,        // JSONL fallback 保留交互气泡
    sessionEnd: false,              // 无 SessionEnd 事件
    subagent: false,                // 不支持子代理追踪（通过 JSONL 补偿）
  },
  
  // 日志配置
  logConfig: {
    sessionDir: "~/.codex/sessions",
    filePattern: "rollout-*.jsonl",
    pollIntervalMs: 1500,           // 1.5秒轮询
  },
  
  // Hook 配置格式标识
  hookConfig: {
    configFormat: "codex-hooks-json",
  },
  
  stdinFormat: "codexHookJson",     // hook 进程的 stdin 格式
  pidField: "codex_pid",            // 进程 PID 字段名
};
```

### 2. eventMap vs logEventMap 对比

| 事件 | eventMap (official hook) | logEventMap (JSONL) |
|------|------------------------|---------------------|
| 会话开始 | SessionStart -> idle | session_meta -> idle |
| 用户输入 | UserPromptSubmit -> thinking | event_msg:task_started -> thinking |
| | | event_msg:user_message -> thinking |
| 工具调用前 | PreToolUse -> working | response_item:function_call -> working |
| 权限请求 | PermissionRequest -> notification | (approval heuristic) |
| 工具调用后 | PostToolUse -> working | event_msg:exec_command_end -> working |
| 回合结束 | Stop -> codex-turn-end | event_msg:task_complete -> codex-turn-end |
| 上下文压缩 | (无) | event_msg:context_compacted -> sweeping |
| 回合中止 | (无) | event_msg:turn_aborted -> idle |
| 自动审查 | (无) | event_msg:guardian_assessment -> working |
| 文本输出 | (无) | event_msg:agent_message -> null (忽略) |

### 3. 状态值含义

| 状态 | 含义 | 宠物表现 |
|------|------|---------|
| `idle` | 空闲等待用户输入 | 待命姿态 |
| `thinking` | 正在思考/处理用户提示 | 思考动画 |
| `working` | 正在执行工具/命令 | 工作动画 |
| `codex-permission` | 等待用户审批权限 | 提醒/弹窗 |
| `codex-turn-end` | 回合结束（特殊值，被解析） | -> attention 或 idle |
| `attention` | 需要用户关注（工具使用后） | 高亮提醒 |
| `sweeping` | 上下文压缩中 | 清理动画 |
| `notification` | 通知型事件 | 通知弹窗 |

### 4. 双通道事件源设计

```
eventSource: "hook+log-poll"
```

Codex 使用**双通道**设计的原因：

1. **Official hooks（主通道）**: 提供实时、精确的状态变更
   - 覆盖关键生命周期: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop
   - 唯一的权限决策通道: PermissionRequest
   
2. **JSONL log polling（补充通道）**: 覆盖 hooks 无法感知的事件
   - `context_compacted` (sweeping) — hooks 中没有对应事件
   - `turn_aborted` — hooks 中没有对应事件  
   - `guardian_assessment` — Desktop 自动审查
   - `web_search_call` — 网页搜索
   - Approval heuristic — 对不支持 hooks 的旧 Codex 版本的回退

### 5. agents/codex-subagent-classifier.js

```javascript
class CodexSubagentClassifier {
  constructor(options = {}) {
    this._capacity = 100;  // 最多追踪 100 个 session
    this._entries = new Map();  // sessionId -> { role }
  }

  registerSession(sessionId, input = {}) {
    // 从 hookPayload / sessionMeta 分类角色
    // 合并规则: subagent 一旦设定就不会被 root 覆盖
    // root + subagent -> subagent (sticky)
  }

  classify(sessionId) {
    // 返回 "root" | "subagent" | "unknown"
    // LRU: 每次访问会刷新到 Map 末尾
  }
}
```

**合并规则**：
- unknown + root -> root
- unknown + subagent -> subagent
- root + subagent -> **subagent** (subagent 是 sticky 的)
- subagent + root -> **subagent** (不回退)

### 6. hooks/codex-session-index.js

读取 `~/.codex/session_index.jsonl` 获取会话标题：

```javascript
function readCodexThreadName(sessionId, options = {}) {
  // 读取文件尾部 512KB
  // 查找 { id: "<bare-session-id>", thread_name: "..." }
  // 返回最后一个匹配的 thread_name
}

function bareCodexSessionId(sessionId) {
  // "codex:uuid" -> "uuid"
}
```

### 7. 与其他 Agent 配置对比

| 配置项 | Codex | Claude Code |
|--------|-------|-------------|
| processNames | codex / codex.exe | (不通过进程名) |
| eventSource | hook+log-poll | hook-only |
| PermissionRequest | command hook + JSONL fallback | HTTP hook |
| capabilities.httpHook | false | true (implied) |
| capabilities.sessionEnd | false | true |
| capabilities.subagent | false | true |
| logConfig | ~/.codex/sessions | (无) |
| hookConfig.configFormat | codex-hooks-json | claude-settings-json |

## 关键文件

| 文件 | 描述 |
|------|------|
| `agents/codex.js` | Codex agent 主配置 |
| `agents/codex-log-monitor.js` | JSONL 轮询监控器 |
| `agents/codex-subagent-classifier.js` | 子代理角色分类器 |
| `hooks/codex-session-index.js` | session 标题索引 |
| `hooks/codex-subagent-fields.js` | 子代理字段分类工具 |
| `agents/registry.js` | agent 注册表 |
