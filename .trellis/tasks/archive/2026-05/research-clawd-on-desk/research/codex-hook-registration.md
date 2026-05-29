# Research: Codex Official Hooks 注册流程

- **Query**: Codex hooks 完整注册流程，hooks.json 格式，安装器实现
- **Scope**: external (GitHub: rullerzhou-afk/clawd-on-desk)
- **Date**: 2026-05-07

## Findings

### 1. hooks.json 配置文件格式

Codex 的 hook 配置存储在 `~/.codex/hooks.json`，格式为：

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "\"node\" \"path/to/codex-hook.js\"",
            "timeout": 30
          }
        ]
      }
    ],
    "PermissionRequest": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "\"node\" \"path/to/codex-hook.js\"",
            "timeout": 600
          }
        ]
      }
    ]
  }
}
```

**关键特征**：
- 每个事件名下是一个数组，每个元素包含 `hooks` 数组（嵌套结构）
- 每个 hook 项有 `type: "command"`、`command` 字符串、`timeout` 秒数
- **PermissionRequest 的 timeout 为 600 秒**（约 10 分钟），其他事件为 30 秒
- Windows 平台需要用 PowerShell call operator 包装命令

来源: `hooks/codex-install-utils.js`

```javascript
// hooks/codex-install-utils.js
const CODEX_HOOK_EVENTS = [
  "SessionStart",
  "UserPromptSubmit",
  "PreToolUse",
  "PermissionRequest",
  "PostToolUse",
  "Stop",
];

function timeoutForCodexEvent(event) {
  return event === "PermissionRequest" ? 600 : 30;
}
```

### 2. 注册的 6 个 Hook 事件

| 事件 | 超时 | 用途 |
|------|------|------|
| `SessionStart` | 30s | 会话开始，映射到 idle 状态 |
| `UserPromptSubmit` | 30s | 用户提交提示，映射到 thinking |
| `PreToolUse` | 30s | 工具调用前，映射到 working |
| `PermissionRequest` | 600s | 权限请求，阻塞等待用户决策 |
| `PostToolUse` | 30s | 工具调用后，映射到 working |
| `Stop` | 30s | 回合结束，映射到 idle |

### 3. 安装器入口 (codex-install.js)

`hooks/codex-install.js` 是 Codex hook 安装的入口文件，它委托给 `codex-install-utils.js`：

```javascript
// hooks/codex-install.js
const {
  DEFAULT_PARENT_DIR,
  DEFAULT_CONFIG_PATH,
  DEFAULT_FEATURES_CONFIG,
  CODEX_HOOK_EVENTS,
  buildCodexHookCommand,
  registerCodexCommandHooks,
  unregisterCodexCommandHooks,
} = require("./codex-install-utils");

const MARKER = "codex-hook.js";

function registerCodexHooks(options = {}) {
  return registerCodexCommandHooks({
    ...options,
    marker: MARKER,
    scriptName: MARKER,
    events: CODEX_OFFICIAL_HOOK_EVENTS,
    label: "Codex official hooks",
  });
}
```

### 4. 注册核心逻辑 (codex-install-utils.js)

`registerCodexCommandHooks()` 的完整流程：

1. **检查 `~/.codex/` 是否存在**，不存在则跳过
2. **启用 config.toml 的 `[features].codex_hooks = true`** — 调用 `ensureCodexHooksFeature()`
3. **解析 hooks.json**（不存在则初始化空对象）
4. **解析 node 路径** — 优先用 `resolveNodeBin()` 检测，回退到已注册 hook 中提取
5. **构建 hook 命令**：
   - POSIX: `"node" "path/to/codex-hook.js"`
   - Windows: `& "node" "path/to/codex-hook.js"` (PowerShell 调用)
   - 远程: `CLAWD_REMOTE=1 "node" "path/to/codex-hook.js"`
6. **遍历 6 个事件**，对每个事件：
   - 查找已存在的 marker（`codex-hook.js`）
   - 如果存在且命令相同 -> skip
   - 如果存在但命令不同 -> update
   - 如果不存在 -> 添加新的 hook 条目
7. **原子写入 hooks.json**

```javascript
// codex-install-utils.js 关键片段
function buildCodexHookCommand(nodeBin, hookScript, platform) {
  return formatNodeHookCommand(nodeBin, hookScript, {
    platform,
    windowsWrapper: "powershell",
  });
}

function withCommandEnv(command, env, platform) {
  // POSIX: KEY='value' command
  // Win32: $env:KEY='value'; command
}
```

### 5. config.toml feature flag

除了 hooks.json 之外，安装器还会确保 `~/.codex/config.toml` 中的 feature flag 被启用：

```toml
[features]
codex_hooks = true
```

`ensureCodexHooksFeature()` 会解析 TOML（手写解析器），找到 `[features]` 段落并添加或修改 `codex_hooks = true`。

### 6. 与 Claude Code hooks 的区别

| 特性 | Claude Code (install.js) | Codex (codex-install.js) |
|------|--------------------------|--------------------------|
| 配置文件 | `~/.claude/settings.json` | `~/.codex/hooks.json` |
| 事件数量 | 11+ core + versioned | 6 个固定事件 |
| PermissionRequest | HTTP hook (`http://127.0.0.1:23333/permission`) | command hook (阻塞 stdin→stdout) |
| Feature flag | 无 | `config.toml [features].codex_hooks = true` |
| Hook 格式 | `{ type, command, shell? }` | `{ type, command, timeout }` |

## 关键文件

| 文件 | 描述 |
|------|------|
| `hooks/codex-install.js` | Codex hook 安装入口 |
| `hooks/codex-install-utils.js` | 注册/卸载核心逻辑 |
| `hooks/install.js` | Claude Code hook 安装器（对比参考） |
| `hooks/server-config.js` | Node 路径解析、端口管理 |
| `hooks/json-utils.js` | 原子 JSON 写入工具 |
