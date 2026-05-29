# Research: codex-hook.js 完整处理逻辑

- **Query**: Codex hook 脚本的 stdin 读取、payload 解析、状态映射、权限阻塞机制
- **Scope**: external (GitHub: rullerzhou-afk/clawd-on-desk)
- **Date**: 2026-05-07

## Findings

### 1. 主流程 (main 函数)

```javascript
// hooks/codex-hook.js - main()
function main() {
  const config = getPlatformConfig();
  const resolve = createPidResolver({
    agentNames: { win: new Set(["codex.exe"]), mac: new Set(["codex"]), linux: new Set(["codex"]) },
    platformConfig: config,
  });

  readStdinJson().then((payload) => {
    // 1. 先尝试构建权限请求体
    const permissionBody = buildPermissionBody(payload || {}, resolve);
    if (permissionBody) {
      // 权限请求: POST 到 Clawd 服务器，阻塞等待响应，将结果写回 stdout
      requestCodexPermission(permissionBody, (output) => {
        process.stdout.write(`${output}\n`);
        process.exit(0);
      });
      return;
    }

    // 2. 非权限事件: 构建状态体并 POST
    const body = buildStateBody(payload || {}, resolve);
    if (!body) process.exit(0);
    postStateToRunningServer(JSON.stringify(body), { timeoutMs: 100 }, () => process.exit(0));
  });
}
```

### 2. stdin 读取机制

`readStdinJson()` 定义在 `hooks/shared-process.js`：

```javascript
function readStdinJson() {
  return new Promise((resolve) => {
    const chunks = [];
    let done = false;
    let timer = null;

    const onData = (c) => chunks.push(c);
    function finish() {
      if (done) return;
      done = true;
      if (timer) clearTimeout(timer);
      // ...
      let payload = {};
      try {
        const raw = Buffer.concat(chunks).toString();
        if (raw.trim()) payload = JSON.parse(raw);
      } catch {}
      resolve(payload);
    }

    process.stdin.on("data", onData);
    process.stdin.on("end", finish);
    timer = setTimeout(finish, 400);  // 400ms 超时保护
  });
}
```

**关键点**：
- 从 stdin 读取完整 JSON（Codex CLI 通过管道传入 payload）
- 400ms 超时保护，防止无限等待
- 解析失败时返回空对象 `{}`

### 3. 事件→状态映射

```javascript
const EVENT_TO_STATE = {
  SessionStart: "idle",
  UserPromptSubmit: "thinking",
  PreToolUse: "working",
  PostToolUse: "working",
  Stop: "idle",  // 注释: server.js 会用 per-turn tool-use map 重新解析
};
```

### 4. 状态事件处理 (buildStateBody)

`buildStateBody()` 构建发往 Clawd 桌面宠物服务器的状态通知体：

```javascript
function buildStateBody(payload, resolve) {
  const event = payload.hook_event_name;
  const state = EVENT_TO_STATE[event];
  if (!state) return null;
  
  // Stop 事件在 stop_hook_active=true 时被跳过
  if (event === "Stop" && payload.stop_hook_active === true) return null;

  const body = {
    state,
    session_id: normalizeCodexSessionId(payload.session_id, payload.transcript_path),
    event,
    agent_id: "codex",
    hook_source: "codex-official",
  };

  // 可选字段：cwd, turn_id, permission_mode, transcript_path, model
  // ...

  // 读取 session_meta 以获取子代理角色和上游信息
  const sessionMeta = readFirstSessionMeta(payload.transcript_path);
  const threadName = readCodexThreadName(sessionId);
  if (threadName) body.session_title = threadName;
  
  const codexRole = resolveCodexSessionRole(payload, sessionMeta);
  if (codexRole !== ROLE_UNKNOWN) body.codex_session_role = codexRole;
  applyCodexUpstreamFields(body, payload, sessionMeta);

  // 工具指纹
  const toolInputFingerprint = buildToolInputFingerprint(toolInput);
  if (toolInputFingerprint) body.tool_input_fingerprint = toolInputFingerprint;

  // PID 解析
  if (process.env.CLAWD_REMOTE) {
    body.host = readHostPrefix();
  } else {
    const { stablePid, agentPid, detectedEditor, pidChain } = resolve();
    body.source_pid = stablePid;
    // ...
  }

  return body;
}
```

### 5. 权限请求处理 (buildPermissionBody + requestCodexPermission)

**PermissionRequest 是唯一的阻塞型 hook**，流程为：

1. Codex CLI 触发 PermissionRequest hook，通过 stdin 传入 JSON payload
2. `buildPermissionBody()` 构建权限请求体（只对 `hook_event_name === "PermissionRequest"` 有效）
3. `requestCodexPermission()` 将请求 POST 到 Clawd 桌面宠物的 HTTP 服务器
4. Clawd 服务器展示权限弹窗给用户（"allow" / "deny"）
5. 用户做出决策后，HTTP 响应返回
6. hook 脚本将决策结果写回 stdout，Codex CLI 解析并执行

```javascript
function buildPermissionBody(payload, resolve) {
  if (event !== "PermissionRequest") return null;

  const body = {
    agent_id: "codex",
    hook_source: "codex-official",
    session_id: normalizeCodexSessionId(payload.session_id, payload.transcript_path),
    tool_name: payload.tool_name || "Unknown",
    tool_input: normalizeToolMatchValue(rawToolInput) || {},
  };

  // 可选: tool_input_description, cwd, turn_id, permission_mode,
  //       transcript_path, model, tool_use_id, tool_input_fingerprint
  return body;
}

function requestCodexPermission(body, callback) {
  postPermissionToRunningServer(
    JSON.stringify(body),
    {
      timeoutMs: getCodexPermissionTimeoutMs(),  // 最大 590000ms (约10分钟)
      probeTimeoutMs: 100,
    },
    (ok, _port, responseBody) => {
      callback(ok ? sanitizeCodexPermissionOutput(responseBody) : buildCodexNoDecisionOutput());
    }
  );
}
```

### 6. stdout 输出格式

权限决策的 stdout 输出格式：

```json
// 允许
{
  "hookSpecificOutput": {
    "hookEventName": "PermissionRequest",
    "decision": { "behavior": "allow" }
  }
}

// 拒绝（含原因）
{
  "hookSpecificOutput": {
    "hookEventName": "PermissionRequest",
    "decision": { "behavior": "deny", "message": "User denied the request" }
  }
}

// 无决策（Clawd 未运行或超时）
{}
```

```javascript
function buildCodexPermissionOutput(decision) {
  const safeDecision = sanitizeCodexPermissionDecision(decision);
  if (!safeDecision) return buildCodexNoDecisionOutput();  // "{}"
  return JSON.stringify({
    hookSpecificOutput: {
      hookEventName: "PermissionRequest",
      decision: safeDecision,
    },
  });
}

function sanitizeCodexPermissionDecision(decision) {
  // 只允许 "allow" 或 "deny"
  const behavior = decision.behavior === "deny" ? "deny"
    : (decision.behavior === "allow" ? "allow" : null);
  if (!behavior) return null;

  const out = { behavior };
  if (behavior === "deny" && typeof decision.message === "string") {
    out.message = decision.message;
  }
  return out;
}
```

### 7. Session ID 规范化

```javascript
function normalizeCodexSessionId(value, transcriptPath = "") {
  // 优先从 transcript 文件名提取 UUID
  const transcriptSessionId = extractCodexSessionIdFromTranscriptPath(transcriptPath);
  // 格式: rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl -> uuid
  const raw = transcriptSessionId || value || "default";
  return raw.startsWith("codex:") ? raw : `codex:${raw}`;
}
```

### 8. transcript_path 解析与 session_meta 读取

hook 脚本会读取 JSONL transcript 文件的第一条 `session_meta` 记录来获取子代理角色信息：

```javascript
function readFirstSessionMeta(transcriptPath) {
  // 以 8KB 块读取，最多 256KB，查找第一个 type=session_meta 的行
  // 用于判断是否为 subagent session
}
```

### 9. 工具输入指纹

对 tool_input 做规范化后计算 SHA1 哈希，用于去重：

```javascript
function buildToolInputFingerprint(toolInput) {
  const normalized = normalizeToolMatchValue(toolInput);
  return crypto.createHash("sha1")
    .update(JSON.stringify(normalized))
    .digest("hex");
}
```

规范化限制：
- 字符串最长 240 字符
- 数组最多 16 项
- 对象最多 32 个 key
- 嵌套最深 6 层

## 关键常量

| 常量 | 值 | 含义 |
|------|-----|------|
| `CODEX_PERMISSION_TIMEOUT_MS` | 590000 | 权限请求最大等待时间 |
| `SESSION_META_READ_CHUNK_BYTES` | 8192 | session_meta 读取块大小 |
| `SESSION_META_READ_MAX_BYTES` | 256KB | session_meta 最大读取量 |
| `TOOL_MATCH_STRING_MAX` | 240 | 工具输入字符串截断长度 |
| `TOOL_MATCH_DEPTH_MAX` | 6 | 工具输入嵌套深度限制 |

## 关键文件

| 文件 | 描述 |
|------|------|
| `hooks/codex-hook.js` | Codex hook 主脚本 |
| `hooks/shared-process.js` | stdin 读取、PID 解析 |
| `hooks/server-config.js` | HTTP 通信（权限/状态 POST） |
| `hooks/codex-subagent-fields.js` | 子代理角色分类 |
| `hooks/codex-session-index.js` | session 标题读取 |
