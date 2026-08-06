# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**mini-term** — 一个基于 Tauri v2 的桌面终端管理器，支持多项目、多标签、分屏布局，并能感知 AI 进程（Claude/Codex）状态。

- **前端**: React 19 + TypeScript + Tailwind CSS v4 + Vite
- **后端**: Rust (Tauri v2)，使用 `portable-pty` 管理 PTY
- **终端渲染**: xterm.js v6（WebGL addon，自动降级为 Canvas）
- **状态管理**: Zustand（全局单一 store）
- **布局分割**: Allotment（三栏主布局）+ 递归 SplitNode 树（分屏终端）

## 开发命令

```bash
# 启动完整 Tauri 开发环境（前端 + 后端一起）
npm run tauri dev

# 仅启动 Vite 前端（无后端，Tauri API 不可用）
npm run dev

# 构建发布包
npm run tauri build

# 仅构建前端
npm run build

# Rust 单元测试（在 src-tauri/ 目录下运行）
cd src-tauri && cargo test
```

## 架构说明

### Rust 后端 (`src-tauri/src/`)

| 文件 | 职责 |
|------|------|
| `lib.rs` | Tauri app 初始化，注册所有 command 和 plugin |
| `pty.rs` | PTY 生命周期管理（create/write/resize/kill）；16ms 批量缓冲后通过 `pty-output` 事件推送数据；reader→flush 走**有界** channel，配合前端水位实现全链路背压（见「PTY 数据流」） |
| `process_monitor.rs` | 后台线程每 500ms 判定各 pane 状态（idle/ai-idle/ai-working）：hook 上报（`hook_server.rs`）一旦启用即为权威，退出以 SessionEnd 为准；无 hook 时降级为输入检测（`pty.rs` 的 AI 命令识别）+ 输出活跃度轮询，通过 `pty-status-change` 事件通知前端。非 hook 的例外有两条。① **用户打断**：Claude 在 Esc/Ctrl+C 中断时不发任何事件（官方文档明示 `Stop` 不触发），由 `write_pty` 识别裸 Esc/Ctrl+C 后调 `hook_server::note_user_interrupt` 把 hook 状态收敛为 ai-idle，cause=`Interrupt` 不算完成。② **停摆兜底**（`stall_settle_target`）：hook 停在 ai-working 且状态与 PTY 输出双双静默 10s 时收敛——此前触发过退出（Ctrl+D/双击 Ctrl+C/`/exit` 且之后无 hook 事件扶正）判为已退出 → `idle`/cause=`StallExit`，否则 → `ai-idle`/cause=`Stall`；正等用户批准的 pane（上次 cause 属 attention 类，如 Codex 的 `PermissionRequest`）豁免，否则黄灯会被抹掉。两条兜底都把结论**落盘**进 hook 状态，触发一次即收敛、不再摆动——这是与 v0.9.3 删掉的无记忆兜底（假完成每 20~50s 重复播报）的分水岭 |
| `config.rs` | `AppConfig` 持久化到 `{app_data_dir}/config.json`；提供跨平台预置 shell 列表 |
| `fs.rs` | 目录列表（过滤 `.gitignore`）+ `notify` 文件监听，通过 `fs-change` 事件通知前端 |
| `ai_sessions.rs` | 读取 Claude/Codex 历史会话记录 |
| `mobile_relay.rs` | 移动端中转体系：对中转服务器的出站 WSS 长连（带桌面端密钥握手、指数退避重连）、配对码/重置配对、项目快照与项目级增量、镜像订阅管理、移动端指令写穿 PTY、移动端发起会话的校验与派发、移动端改会话名的标题收敛 |
| `mobile_mirror.rs` | 对话镜像：pane → 项目最新会话 JSONL 的增量解析（半行拼接）、分页取数 |

**Tauri Commands**: `load_config`, `save_config`, `create_pty`, `write_pty`, `resize_pty`, `kill_pty`, `kill_all_ptys`, `set_pty_flow_paused`, `list_directory`, `watch_directory`, `unwatch_directory`, `get_ai_sessions`, `mobile_relay_apply`, `mobile_relay_status`, `mobile_relay_request_pairing_code`, `mobile_relay_reset_pairing`, `mobile_relay_update_sessions`, `mobile_relay_launchers_changed`, `mobile_relay_start_session_result`, `mobile_relay_check_launcher_command`

**Tauri Events（后端→前端）**: `pty-output`, `pty-exit`, `pty-status-change`, `fs-change`, `mobile-relay-status`, `mobile-relay-pairing-code`, `mobile-start-session`, `mobile-rename-pane`

### 移动端中转体系（`relay-server/` + `mobile/`）

- `relay-server/protocol`：桌面端与中转共享的协议消息 crate（JSON over WebSocket，serde camelCase，带版本号握手校验，当前 v2）；PWA 侧 TypeScript 类型在 `mobile/src/protocol.ts` 手写镜像，两侧字段必须同步维护
- `relay-server/server`：axum 中转服务，只做转发不落盘；桌面端接入需携带 `MT_RELAY_DESKTOP_KEY`（未配置即拒绝一切桌面连接，fail-closed）；`cd relay-server && cargo test` 跑 Seam 1 协议边界测试
- `mobile/`：React + TS + Vite PWA（扫码配对、会话列表、对话镜像、移动端指令、发起新 AI 会话、会话重命名）；`cd mobile && npm run build` 构建，产物由中转托管；部署见 `docs/deploy-relay.zh-CN.md`（英文版 `docs/deploy-relay.md`）
- **AI 启动器**：桌面端配置的具名 `{名称, shell?, 命令}`，移动端只按 id 引用、看得到名字，命令文本从不经过移动端或中转（ADR 0002 的边界）

### 前端 (`src/`)

**数据流**：
- `store.ts` 是唯一全局状态，用 `Map<projectId, ProjectState>` 存储每个项目的 tabs
- 每个 Tab 的终端区域是一棵 `SplitNode` 树（leaf = 单个 pane，split = 横/纵分屏）
- `PaneStatus` 优先级：`error > ai-working > ai-idle > idle`，从叶节点聚合到 Tab 级别

**关键组件**：

| 组件 | 职责 |
|------|------|
| `App.tsx` | 三栏 Allotment 主布局（ProjectList \| FileTree \| TerminalArea + AIHistoryPanel） |
| `TerminalArea.tsx` | Tab 管理 + 分屏逻辑（`insertSplit`/`removePane` 操作 SplitNode 树） |
| `SplitLayout.tsx` | 递归渲染 SplitNode 树，使用 Allotment 实现可拖拽分屏 |
| `TerminalInstance.tsx` | xterm.js 终端实例，WebGL 渲染，ResizeObserver 自适应，文件拖拽插入路径 |
| `TerminalConfigModal.tsx` | 终端配置 modal（shell 列表管理） |
| `MobileRelayModal.tsx` | 「移动端」面板：中转地址 + 桌面端密钥、连接状态、配对二维码、AI 启动器 |
| `AiLauncherSection.tsx` | AI 启动器增删改（名称 / shell / 命令 + 命令识别警告），嵌在「移动端」面板 |

**类型系统** (`src/types.ts`): 前端所有类型定义，与后端 Rust 结构通过 `serde(rename_all = "camelCase")` 对齐。

### PTY 数据流

```
用户键入 → xterm.onData → invoke('write_pty') → Rust writer
Rust reader → 有界 channel → 16ms 批量缓冲 → emit('pty-output') → term.write()
                  ↑                                                    │
                  └── 背压：flush 暂停 → channel 满 → reader 停读 ←────┘
                      invoke('set_pty_flow_paused')  ← 前端积压过高水位
进程退出 → emit('pty-exit') → store.updatePaneStatusByPty('error')
进程监控 → emit('pty-status-change') → store.updatePaneStatusByPty(status)
页面加载 → invoke('kill_all_ptys') → 回收上一轮遗留的 PTY（早于任何 create_pty）
```

**背压**：`term.write()` 的完成回调统计「已收到未解析」的字节数，越过 4MB 让后端暂停投递、
回落到 1MB 恢复。后端暂停时 flush 不取数据，有界 channel 迅速填满，reader 随之停止从 ConPTY 读，
背压直达刷屏进程本身——和真实终端一样，慢终端拖慢 `cat`，而不是把数据全缓存到内存里。
后端有 30s 超时兜底，前端崩了也不会把 shell 永久卡在写阻塞上。

**孤儿 PTY**：`PtyManager` 活在主进程里，WebView2 renderer 被 OOM 杀掉后页面重载，
恢复出的 pane 是全新 id 并各自新建 PTY，旧 PTY 就此无人引用却继续运行（崩一次漏一整套，
内存压力递增形成正反馈）。前端在 `load_config` 之前先 `kill_all_ptys` 掐断这条链路。

## 注意事项

- 文件拖拽到终端会将文件路径作为文本写入 PTY（不是上传文件）
- `WebkitAppRegion: 'drag'` 用于自定义标题栏拖拽，菜单项需设置 `no-drag` 区域
- 分屏关闭最后一个 pane 时会关闭整个 tab（`removePane` 返回 `null` 时触发）
- AI 会话识别有两层：Claude/Codex hook 上报（`hook_server.rs`，权威）+ 输入检测（`pty.rs` 识别键入的 `claude`/`codex`/`opencode`/`pi` 命令，含 ↑ 历史/Tab 补全的行快照兜底与输出回扫）；不做子进程名轮询
- 只有 Claude/Codex 有可解析的会话记录文件（`mobile_mirror::agent_has_session_log`）。opencode/pi 这类**只靠输入检测识别**的 agent 拿得到状态徽章与移动端指令，但没有对话镜像、AI 历史面板与用量统计——镜像必须据此跳过启发式绑定，否则会绑到同项目 Claude/Codex 的最新会话文件，把别人的对话贴到该 pane 上

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **mini-term-hubs** (5347 symbols, 12740 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> Index stale? Run `node .gitnexus/run.cjs analyze` from the project root — it auto-selects an available runner. No `.gitnexus/run.cjs` yet? `npx gitnexus analyze` (npm 11 crash → `npm i -g gitnexus`; #1939).

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows. For regression review, compare against the default branch: `detect_changes({scope: "compare", base_ref: "main"})`.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `rename` which understands the call graph.
- NEVER commit changes without running `detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/mini-term-hubs/context` | Codebase overview, check index freshness |
| `gitnexus://repo/mini-term-hubs/clusters` | All functional areas |
| `gitnexus://repo/mini-term-hubs/processes` | All execution flows |
| `gitnexus://repo/mini-term-hubs/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->
