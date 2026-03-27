# Mini-Term 桌面终端工具设计文档

## 概述

Mini-Term 是一个基于 Tauri 2 + React + TypeScript 的桌面终端管理工具。它提供多项目管理、文件浏览、多终端分屏以及 AI（Claude/Codex）会话历史查看功能，旨在为开发者提供一个集成化的终端工作环境。

## 技术栈

| 层级 | 技术选型 |
|------|----------|
| 桌面框架 | Tauri 2 |
| 前端框架 | React 18 + TypeScript |
| 终端渲染 | xterm.js |
| 伪终端 | portable-pty（Rust crate，Tauri 侧） |
| 文件监听 | notify（Rust crate） |
| 分屏布局 | allotment |
| 文件树 | 自定义 TreeNode 组件 |
| 构建工具 | Vite |
| 样式方案 | Tailwind CSS |

## 整体布局

```
┌──────────────────────────────────────────────────────────────────┐
│  Mini-Term  │  文件  │  终端  │  设置                    — □ ✕  │
├────────┬──────────┬──────────────────────────────┬──────────────┤
│        │          │  [●pwsh ✕] [●bash ✕] [+]     │              │
│ 项目   │  文件树   ├──────────────┬───────────────┤  AI 会话历史  │
│ 列表   │          │              │               │              │
│ (20%)  │  (30%    │   终端面板    │   终端面板     │  (可收起)     │
│        │  可调)   │              ├───────────────┤              │
│        │          │              │               │              │
│        │          │              │   终端面板     │              │
├────────┴──────────┴──────────────┴───────────────┴──────────────┤
```

### 左栏：项目列表（~20%）

- 展示用户添加的项目（文件夹）列表
- 支持添加 / 删除项目
- 选中高亮，点击切换当前项目
- 每个项目右侧显示 **AI 活动状态指示**：
  - 🟣 紫色闪烁 — 该项目有 AI（claude/codex）正在工作
  - 🟢 绿色 — 该项目有 AI 等待用户输入
  - 无指示 — 该项目无 AI 活动
- **项目隔离**：切换项目时，右侧终端 tab 组、中栏文件树、会话历史列表全部切换为对应项目的内容

### 中栏：文件树（~30%，宽度可调）

- 采用**懒加载**策略：初始只加载根目录，展开文件夹时按需加载子目录内容
- 支持展开 / 折叠文件夹
- 默认遵循 `.gitignore` 规则过滤文件（如 `node_modules`、`target` 等）
- Rust 后端通过 notify crate 监听**已展开目录**的文件系统变化，实时更新
- **拖拽支持**：可将文件拖拽到右侧终端面板中，插入文件的**绝对路径**
- 左栏与中栏、中栏与右栏之间的分隔线可拖拽调整宽度

### 右栏：终端区域（填充剩余空间）

#### Tab 与分屏模型

采用类似 iTerm2 的模型：**每个 Tab 包含一个分屏布局**。

- 顶部 Tab 栏，每个 tab 包含一个独立的分屏布局树
- Tab 标题包含终端类型名称和**活动状态圆点**：
  - 🟢 绿色 — 空闲，等待用户输入
  - 🟠 橙色闪烁 — 进程运行中（如 `npm run dev`）
  - 🟣 紫色闪烁 — AI 工作中（检测到 claude/codex 命令）
  - 🔴 红色 — 进程异常退出
- 如果 tab 内有多个分屏面板，tab 状态取优先级最高的状态（error > ai-working > running > idle）
- 支持新建 tab（"+" 按钮）和关闭 tab（"✕" 按钮）
- **每个项目独立维护自己的 tab 组**，切换项目时整体切换

#### 分屏操作

- 在当前 tab 内，支持任意方向递归分割（类似 tmux / iTerm2）
- 通过快捷键或右键菜单在当前面板中新建分屏：左/右/上/下
- 分屏之间的分隔线可拖拽调整大小
- 支持 2x2、1+2 等复杂布局
- 关闭某个分屏面板时终止对应 PTY 进程，如果是 tab 内最后一个面板则关闭整个 tab

#### 终端集成

- 前端使用 xterm.js 渲染终端
- Rust 后端使用 portable-pty 创建伪终端进程
- **可配置终端类型**：支持配置多种终端，如：
  - cmd
  - PowerShell (pwsh 7)
  - Windows PowerShell (powershell 5.1)
  - Git Bash
  - 其他自定义 shell
- 新建 tab 或新建分屏时可选择终端类型

### 最右侧：AI 会话历史（可收起侧栏）

- 展示**当前项目**的 Claude / Codex 历史 session 列表
- 每个 session 展示：
  - AI 类型 + 编号（如 "Claude #12"）
  - 时间
  - 对话轮数
- 切换左栏项目时，列表自动更新
- 面板可收起以获得更大终端空间

#### AI 会话数据源

- **Claude**: 读取 `~/.claude/projects/` 目录。Claude Code 按项目路径存储会话，每个项目目录下有 JSONL 格式的会话文件，包含会话 ID、时间戳、消息内容等
- **Codex**: 读取 `~/.codex/` 目录（如存在），格式待确认
- Rust 后端封装 `AISessionReader` trait，提供统一的会话读取接口：
  ```rust
  trait AISessionReader {
      fn list_sessions(&self, project_path: &Path) -> Vec<AISession>;
  }
  ```
- 为 Claude 和 Codex 分别实现该 trait，便于未来适配格式变化或新增 AI 工具
- 若 `~/.claude/` 目录不存在，会话历史面板显示为空，不报错

## 数据架构

### 项目配置持久化

配置文件使用 JSON 格式，存储在 Tauri 的 `app_data_dir()`（Windows 上为 `%APPDATA%/mini-term/`）。

```typescript
interface AppConfig {
  projects: ProjectConfig[];
  defaultShell: string;
  availableShells: ShellConfig[];
}

interface ProjectConfig {
  id: string;
  name: string;
  path: string;
}

interface ShellConfig {
  name: string;
  command: string;
  args?: string[];
}
```

### 项目运行时状态

```typescript
interface ProjectState {
  id: string;
  tabs: TerminalTab[];
  activeTabId: string;
}

interface TerminalTab {
  id: string;
  splitLayout: SplitNode;
  status: TabStatus;
}
// Tab 标题自动生成：单 pane 时取 shellName，多 pane 时取活跃 pane 的 shellName
// 用户可双击 tab 标题手动重命名

// 递归分屏布局
type SplitNode =
  | { type: 'leaf'; pane: PaneState }
  | { type: 'split'; direction: 'horizontal' | 'vertical'; children: SplitNode[]; sizes: number[] };

interface PaneState {
  id: string;
  shellName: string;
  status: 'idle' | 'running' | 'ai-working' | 'error';
  ptyId: number;
}

type TabStatus = 'idle' | 'running' | 'ai-working' | 'error';
```

### AI 会话历史

```typescript
interface AISession {
  id: string;
  type: 'claude' | 'codex';
  projectPath: string;
  startTime: string;
  messageCount: number;
}
```

## PTY 数据通道

PTY 的输入输出是高频数据流，不适合使用 Tauri invoke（请求-响应模式）。采用以下方案：

- **PTY 输出（Rust → 前端）**：使用 Tauri event system 推送 `pty-output` 事件
  - 数据格式：UTF-8 字符串（xterm.js 的 `write()` 方法接受 string）
  - 缓冲策略：Rust 侧每个 PTY 维护一个输出缓冲区，每 **16ms**（约 60fps）批量发送一次，避免逐字节推送造成性能问题
  - 事件 payload：`{ ptyId: number, data: string }`
- **PTY 输入（前端 → Rust）**：使用 Tauri invoke 调用 `write_pty` 命令（用户输入频率低，invoke 模式足够）
- **PTY 调整大小**：使用 Tauri invoke 调用 `resize_pty` 命令

## 前后端通信

Tauri 命令（前端 → Rust）：

| 命令 | 描述 |
|------|------|
| `create_pty(shell, cwd)` | 创建 PTY 进程，返回 pty_id |
| `write_pty(pty_id, data)` | 向 PTY 写入数据 |
| `resize_pty(pty_id, cols, rows)` | 调整 PTY 大小 |
| `kill_pty(pty_id)` | 终止 PTY 进程 |
| `list_directory(project_root, path)` | 列出目录内容（用于文件树懒加载），根据 project_root 定位 `.gitignore` 并过滤 |
| `watch_directory(path)` | 注册文件系统监听（文件树展开目录时调用） |
| `unwatch_directory(path)` | 注销文件系统监听（文件树折叠目录时调用） |
| `get_ai_sessions(project_path)` | 获取指定项目的 AI 会话历史 |
| `load_config()` | 加载应用配置 |
| `save_config(config)` | 保存应用配置 |

Tauri 事件（Rust → 前端推送）：

| 事件 | payload | 描述 |
|------|---------|------|
| `pty-output` | `{ ptyId, data }` | PTY 批量输出数据（每 16ms） |
| `pty-exit` | `{ ptyId, exitCode }` | PTY 进程退出 |
| `pty-status-change` | `{ ptyId, status }` | PTY 状态变化（idle/running/ai-working/error） |
| `fs-change` | `{ projectPath, path, kind }` | 文件系统变化通知（含所属项目路径） |

## 终端状态检测

### Windows 平台方案

由于本工具主要面向 Windows 平台，终端状态检测采用 Windows 特定方案：

- **子进程检测**：通过 Windows API（`CreateToolhelp32Snapshot` + `Process32Next`）遍历进程树，找到 ConPTY shell 进程的子进程
- **空闲（idle）**：shell 进程无子进程
- **运行中（running）**：shell 进程有子进程在运行
- **AI 工作中（ai-working）**：子进程名匹配 `claude.exe`、`codex.exe` 等关键字
- **错误（error）**：PTY 进程非零退出（通过 `pty-exit` 事件获知）

### 检测频率

- Rust 侧启动一个后台线程，每 **500ms** 检查一次所有活跃 PTY 的进程状态
- 状态变化时通过 `pty-status-change` 事件通知前端
- 项目列表的 AI 状态 = 该项目所有 pane 中 AI 状态的聚合

## 项目切换与终端生命周期

- 切换项目时，非活跃项目的 PTY 进程**继续运行**，不中断
- 非活跃项目的 xterm.js 实例采用**隐藏 DOM**策略（`display: none`），保留完整的 scrollback buffer 和渲染状态
- 切换回时直接显示，无需重建
- 状态检测线程持续监控所有项目的所有 PTY，确保非活跃项目的状态指示也能实时更新

## 文件拖拽

1. 中栏文件树节点设置 `draggable`
2. `dragstart` 事件携带文件绝对路径
3. 终端面板监听 `drop` 事件
4. 收到 drop 后，通过 `write_pty` 将绝对路径文本写入对应 PTY

## 关键依赖

### Rust (Cargo)

- `tauri` — 桌面框架
- `portable-pty` — 跨平台伪终端
- `notify` — 文件系统监听
- `serde` / `serde_json` — 序列化
- `dirs` — 获取用户目录（定位 `~/.claude/`）
- `windows` — Windows API 调用（进程树遍历）

### 前端 (npm)

- `react` + `react-dom` — UI 框架
- `typescript` — 类型安全
- `@xterm/xterm` + `@xterm/addon-fit` + `@xterm/addon-webgl` — 终端渲染
- `allotment` — 分屏面板
- `@tauri-apps/api` — Tauri 前端 API
- `vite` + `@vitejs/plugin-react` — 构建工具
- `tailwindcss` — 样式
