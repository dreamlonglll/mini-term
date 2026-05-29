# Research: Codex JSONL 日志轮询机制深度分析

- **Query**: codex-log-monitor.js 完整实现细节
- **Scope**: external (GitHub: rullerzhou-afk/clawd-on-desk)
- **Date**: 2026-05-07

## Findings

### 1. 日志文件发现逻辑

#### 基础路径
```
~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
```
配置来自 `agents/codex.js`:
```javascript
logConfig: {
  sessionDir: "~/.codex/sessions",
  filePattern: "rollout-*.jsonl",
  pollIntervalMs: 1500,
}
```

#### _getSessionDirs() 完整逻辑

目录发现使用三层策略：

```javascript
_getSessionDirs() {
  const dirs = [];
  
  // 策略 1: 当天 + 前两天的日期目录
  for (let daysAgo = 0; daysAgo <= 2; daysAgo++) {
    // ~/.codex/sessions/2026/05/07
    // ~/.codex/sessions/2026/05/06
    // ~/.codex/sessions/2026/05/05
  }
  
  // 策略 2: 最近 7 个已存在的日期目录（缓存 1 小时）
  // 处理时钟/时区漂移和 codex resume 旧会话
  for (const dir of this._getCachedRecentExistingDayDirs(7)) addDir(dir);
  
  // 策略 3: 含活跃 rollout 文件的任何日期目录（缓存 5 秒）
  // 处理 Codex Desktop 长期会话写入旧目录的情况
  for (const dir of this._getActiveDayDirs()) addDir(dir);
  
  return dirs;
}
```

#### 活跃日期目录扫描 (_getActiveDayDirs)

```javascript
_getActiveDayDirs(withinMs = ACTIVE_SESSION_WINDOW_MS) {  // 5分钟窗口
  // 遍历 baseDir 下所有 YYYY/MM/DD 目录
  // 检查每个 rollout-*.jsonl 的 mtime
  // 如果 mtime 在 5 分钟内 -> 该目录是活跃的
  // 结果缓存 5 秒
}
```

### 2. 增量读取实现

#### _pollFile() 核心逻辑

```javascript
_pollFile(filePath, fileName) {
  let stat = fs.statSync(filePath);
  
  let tracked = this._tracked.get(filePath);
  if (!tracked) {
    // 新文件 — 提取 session ID
    const sessionId = this._extractSessionId(fileName);
    
    // 检查是否为已退休后恢复的文件
    const retired = this._retiredTracked.get(filePath);
    const resumeOffset = retired && stat.size >= retired.offset ? retired.offset : 0;
    
    tracked = {
      offset: resumeOffset,
      sessionId: "codex:" + sessionId,
      cwd: retired ? retired.cwd : "",
      lastState: retired ? retired.lastState : null,
      partial: "",                    // 未完成的行缓冲
      hadToolUse: false,              // 本轮是否使用了工具
      isSubagent: false,              // 是否为子代理
      agentPid: null,                 // codex 进程 PID
      pendingApprovalDetail: null,    // 待审批的命令详情
      // backfill 模式判断:
      // 文件 mtime 在 monitor 启动前 5 秒以上 -> 历史文件
      backfilling: !retired && stat.size > 0 &&
        stat.mtimeMs < this._startedAtMs - BACKFILL_GRACE_MS,
    };
    this._tracked.set(filePath, tracked);
  }
  
  // 无新数据
  if (stat.size <= tracked.offset) return;
  
  // 增量读取
  const fd = fs.openSync(filePath, "r");
  const readLen = stat.size - tracked.offset;
  const buf = Buffer.alloc(readLen);
  fs.readSync(fd, buf, 0, readLen, tracked.offset);
  fs.closeSync(fd);
  tracked.offset = stat.size;
  
  // 分行处理，保留不完整的最后一行
  const text = tracked.partial + buf.toString("utf8");
  const lines = text.split("\n");
  const remainder = lines.pop() || "";
  tracked.partial = remainder.length > MAX_PARTIAL_BYTES ? "" : remainder;
  // MAX_PARTIAL_BYTES = 65536 -> 超大行被丢弃
  
  for (const line of lines) {
    if (!line.trim()) continue;
    this._processLine(line, tracked);
  }
  
  // backfill 完成后发射快照
  if (tracked.backfilling) {
    this._emitBackfillSnapshot(tracked);
    tracked.backfilling = false;
  }
}
```

### 3. JSONL 记录类型解析与状态映射

#### logEventMap 完整映射表

```javascript
logEventMap: {
  "session_meta":                        "idle",
  "event_msg:task_started":              "thinking",
  "event_msg:user_message":              "thinking",
  "event_msg:agent_message":             null,        // 忽略，文本输出不改变状态
  "event_msg:guardian_assessment":        "working",   // Desktop 自动审查
  "event_msg:exec_command_end":          "working",
  "event_msg:patch_apply_end":           "working",
  "event_msg:custom_tool_call_output":   "working",
  "response_item:function_call":         "working",
  "response_item:custom_tool_call":      "working",
  "response_item:web_search_call":       "working",
  "event_msg:task_complete":             "codex-turn-end",  // 特殊处理
  "event_msg:context_compacted":         "sweeping",
  "event_msg:turn_aborted":             "idle",
}
```

#### _processLine() 完整处理流程

```javascript
_processLine(line, tracked) {
  let obj = JSON.parse(line);
  
  // === 重放保护层 1: 时间戳过滤 ===
  if (obj.timestamp) {
    const ts = Date.parse(obj.timestamp);
    if (ts < this._startedAtMs - 1500) return;  // 跳过旧事件
  }
  
  // 构建查找 key: "type:subtype" 或 "type"
  const type = obj.type;
  const subtype = payload?.type || "";
  const key = subtype ? type + ":" + subtype : type;
  
  // === session_meta 处理 ===
  if (type === "session_meta") {
    tracked.cwd = payload.cwd || "";
    const role = this._classifier.registerSession(tracked.sessionId, { sessionMeta: payload });
    // 标记为 subagent 或 root
  }
  
  // === session 标题提取 ===
  // 从 turn_context.summary 提取
  // 也从 codex session_index.jsonl 读取 thread_name
  
  // === Approval timer 清除条件 ===
  // exec_command_end / function_call_output -> 命令已完成
  // guardian_assessment (in_progress/approved) -> Desktop 自动审查
  if (key === "event_msg:exec_command_end" 
      || key === "response_item:function_call_output"
      || this._isGuardianApprovalActivity(payload)) {
    clearTimeout(tracked.approvalTimer);
    tracked.pendingApprovalDetail = null;
  }
  
  // 查找状态映射
  const state = this._config.logEventMap[key];
  if (state === undefined || state === null) return;
  
  // === 工具使用追踪 ===
  if (key === "event_msg:task_started") tracked.hadToolUse = false;
  if (key === "response_item:function_call") tracked.hadToolUse = true;
  
  // === codex-turn-end 特殊处理 ===
  if (state === "codex-turn-end") {
    clearTimeout(tracked.approvalTimer);
    const resolved = this._isTrackedSubagent(tracked)
      ? "idle"                                    // 子代理总是 idle
      : (tracked.hadToolUse ? "attention" : "idle");  // 用过工具 -> attention
    tracked.hadToolUse = false;
    this._emitStateChange(tracked, resolved, key);
    return;
  }
  
  // === Approval Heuristic (2s 超时判断) ===
  if (key === "response_item:function_call") {
    const cmd = this._extractShellCommand(payload);
    tracked.pendingApprovalDetail = cmd ? { command: cmd, rawPayload: payload } : null;
    
    if (cmd) {
      // 显式权限请求（sandbox_permissions / justification）直接标记
      if (this._isExplicitApprovalRequest(payload)) {
        this._emitStateChange(tracked, "codex-permission", key, {
          permissionDetail: tracked.pendingApprovalDetail,
        });
        return;
      }
      
      // 启动 2 秒定时器
      tracked.approvalTimer = setTimeout(() => {
        tracked.lastState = "codex-permission";
        this._emitStateChange(tracked, "codex-permission", key, {
          permissionDetail: tracked.pendingApprovalDetail,
        });
      }, APPROVAL_HEURISTIC_MS);  // 2000ms
    }
  }
  
  // === backfill 门控 ===
  if (tracked.backfilling) {
    tracked.lastState = state;
    return;  // 静默更新，不发射
  }
  
  // 避免重复 working 状态
  if (state === tracked.lastState && state === "working") return;
  tracked.lastState = state;
  this._emitStateChange(tracked, state, key);
}
```

### 4. Approval Heuristic 完整实现

**核心思路**: Codex CLI 在执行 shell 命令前会发出 `function_call` 记录。如果 2 秒内没有 `exec_command_end` 或 `function_call_output`，则推断 Codex 正在等待用户审批。

**触发条件**:
1. `response_item:function_call` 且 `name` 为 `shell_command` 或 `exec_command`
2. 提取命令内容: `arguments.command` 或 `arguments.cmd`

**快速路径（跳过 2s 等待）**:
- 显式权限请求: `sandbox_permissions === "require_escalated"` 或有 `justification` 字段

**清除条件**:
- `exec_command_end` — 命令执行完毕
- `function_call_output` — 工具调用有输出
- `guardian_assessment` (status: "in_progress" 或 "approved") — Desktop 自动审查

```javascript
_extractShellCommand(payload) {
  if (payload.name !== "shell_command" && payload.name !== "exec_command") return "";
  const args = typeof payload.arguments === "string" 
    ? JSON.parse(payload.arguments) : payload.arguments;
  return args?.command || args?.cmd || "";
}

_isExplicitApprovalRequest(payload) {
  const args = typeof payload.arguments === "string"
    ? JSON.parse(payload.arguments) : payload.arguments;
  if (args.sandbox_permissions === "require_escalated") return true;
  if (typeof args.justification === "string" && args.justification.trim()) return true;
  return false;
}
```

### 5. 重放保护两层机制

#### 层 1: 行级时间戳过滤

```javascript
// _processLine 中
if (obj.timestamp) {
  const ts = Date.parse(obj.timestamp);
  if (ts < this._startedAtMs - 1500) return;  // 1.5秒宽限
}
```
- 只对有 `timestamp` 字段的记录有效
- 容忍 1.5 秒时钟偏差

#### 层 2: 文件级 backfill 模式

```javascript
// _pollFile 中设置
backfilling: !retired && stat.size > 0 &&
  stat.mtimeMs < this._startedAtMs - BACKFILL_GRACE_MS  // 5秒
```

**backfill 行为**:
- 设置 `tracked.backfilling = true`
- `_processLine` 中只更新 `tracked.lastState`，不发射状态变更
- 不启动 approval timer
- 但 **会** 更新 cwd、sessionTitle、subagent 分类等元数据
- 第一轮读取结束后，调用 `_emitBackfillSnapshot()`

#### backfill 快照发射

```javascript
_emitBackfillSnapshot(tracked) {
  const snapshotState = tracked.lastState;
  // 只对 "持续状态" 发射快照
  if (!BACKFILL_SNAPSHOT_STATES.has(snapshotState)) return;
  // BACKFILL_SNAPSHOT_STATES = { "thinking", "working", "codex-permission" }
  
  this._emitStateChange(tracked, snapshotState, tracked.lastStateEvent, extra);
}
```

**设计意图**: 如果 Clawd 重启时 Codex 正在 thinking/working/waiting-for-permission，backfill 会发出一次当前状态让桌面宠物恢复正确显示。如果历史最终状态是 idle/attention，则不发射（静默恢复）。

### 6. 文件追踪管理

#### 容量限制
- `MAX_TRACKED_FILES = 50` — 同时追踪的最大文件数
- `MAX_RETIRED_TRACKED_FILES = 100` — 退休文件缓存
- `MAX_PARTIAL_BYTES = 65536` — 部分行最大缓冲

#### 淘汰策略 (_pruneTrackedFilesIfNeeded)

```javascript
_pruneTrackedFilesIfNeeded() {
  // 优先淘汰从未发射过状态的文件（neverEmitted）
  // 再淘汰已发射过状态的文件（emitted）
  // 两组内部都按 lastEventTime 升序（最旧先淘汰）
  // 淘汰 -> _retireTrackedFile -> 保存 offset 等元数据到 _retiredTracked
}
```

#### 退休文件恢复

当已退休的文件重新出现新写入时，可从 `_retiredTracked` 恢复：
- 恢复上次的 offset（避免重读）
- 恢复 cwd、sessionTitle、lastState、hadToolUse、isSubagent 等

### 7. Session ID 提取

```javascript
_extractSessionId(fileName) {
  // rollout-2026-03-25T15-10-51-019d23d4-f1a9-7633-b9c7-758327137228.jsonl
  // UUID v7 是文件名的最后 5 段（8-4-4-4-12 hex）
  const base = fileName.replace(".jsonl", "");
  const parts = base.split("-");
  if (parts.length < 10) return null;
  return parts.slice(-5).join("-");
}
```

### 8. PID 发现 (Linux only)

```javascript
_findCodexWriterPid(filePath) {
  // 仅 Linux: 通过 /proc 扫描
  // 遍历 /proc/<pid>/cmdline 查找包含 "codex" 的进程
  // 然后检查 /proc/<pid>/fd/* 是否指向目标 rollout 文件
}
```

## 关键常量

| 常量 | 值 | 含义 |
|------|-----|------|
| `APPROVAL_HEURISTIC_MS` | 2000 | 权限审批推断超时 |
| `MAX_TRACKED_FILES` | 50 | 最大追踪文件数 |
| `MAX_RETIRED_TRACKED_FILES` | 100 | 最大退休文件缓存 |
| `MAX_PARTIAL_BYTES` | 65536 | 部分行最大缓冲 |
| `RECENT_DAY_DIR_CACHE_MS` | 3600000 | 最近目录缓存 1 小时 |
| `ACTIVE_SESSION_WINDOW_MS` | 300000 | 活跃会话窗口 5 分钟 |
| `BACKFILL_GRACE_MS` | 5000 | backfill 宽限 5 秒 |

## 关键文件

| 文件 | 描述 |
|------|------|
| `agents/codex-log-monitor.js` | JSONL 轮询监控主类 |
| `agents/codex.js` | agent 配置（logEventMap 等） |
| `agents/codex-subagent-classifier.js` | 子代理分类器 |
| `hooks/codex-session-index.js` | session 标题索引读取 |
