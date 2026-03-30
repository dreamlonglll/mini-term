# Mini-Term

基于 Tauri v2 的桌面终端管理器，支持多项目、多标签、分屏布局，并能感知 AI 进程（Claude / Codex）状态。

## 解决痛点

1. 对于All In Ai 的用户来说，为了使用Vibe Coding，还需要打开各种idea等开发工具，大且占内存。使用终端，又少了些会话、多项目管理。
2. 多项目多claude/codex并发开发，某个项目的某个Agent执行完了后，无法直观看到。

## 功能特性

- **多项目管理** — 左侧边栏管理多个项目目录，一键切换工作区
- **多标签 + 分屏** — 每个项目独立标签页，支持横向/纵向递归分屏，Allotment 拖拽调整比例
- **终端渲染** — xterm.js v6 + WebGL 加速（自动降级为 Canvas），支持 Ctrl+Shift+C/V 复制粘贴
- **AI 状态感知** — 自动检测终端中运行的 Claude / Codex 会话，实时显示 idle / working 状态
- **会话管理** — 读取本地 Claude 和 Codex 历史会话记录，右键可复制恢复命令快速续接
- **文件树** — 集成目录浏览器，过滤 `.gitignore` 条目，支持文件监听实时刷新
- **布局持久化** — 分屏布局、标签页状态自动保存，重启后恢复
- **文件拖拽** — 拖拽文件到终端自动插入路径
- **Warm Carbon 主题** — 暖炭色调设计，自定义 CSS 变量体系

## 技术栈

| 层      | 技术                                              |
| ------- | ------------------------------------------------- |
| 前端    | React 19 + TypeScript + Tailwind CSS v4 + Vite 7  |
| 后端    | Rust（Tauri v2），`portable-pty` 管理 PTY          |
| 终端    | xterm.js v6（WebGL addon）                        |
| 状态    | Zustand                                            |
| 布局    | Allotment（主布局三栏 + 递归 SplitNode 分屏树）    |

## 快速开始

### 前置条件

- [Node.js](https://nodejs.org/) >= 18
- [Rust](https://www.rust-lang.org/tools/install) >= 1.70
- [Tauri v2 CLI](https://v2.tauri.app/start/prerequisites/)

### 安装与运行

```bash
# 安装依赖
npm install

# 启动完整 Tauri 开发环境（前端 + 后端）
npm run tauri dev

# 仅启动 Vite 前端（无后端，Tauri API 不可用）
npm run dev

# 构建发布包
npm run tauri build
```

## 项目结构

```
mini-term/
├── src/                        # 前端源码
│   ├── App.tsx                 # 三栏主布局入口
│   ├── store.ts                # Zustand 全局状态
│   ├── types.ts                # 类型定义
│   ├── styles.css              # 全局样式与设计变量
│   └── components/
│       ├── ProjectList.tsx     # 项目列表 + 会话面板
│       ├── SessionList.tsx     # AI 会话历史列表
│       ├── FileTree.tsx        # 文件目录树
│       ├── TerminalArea.tsx    # 标签管理 + 分屏逻辑
│       ├── SplitLayout.tsx     # 递归渲染分屏树
│       ├── TerminalInstance.tsx # xterm.js 终端实例
│       ├── TabBar.tsx          # 标签栏
│       ├── SettingsModal.tsx   # 设置弹窗
│       └── StatusDot.tsx       # 状态指示点
├── src-tauri/                  # Rust 后端
│   └── src/
│       ├── lib.rs              # Tauri 初始化与命令注册
│       ├── pty.rs              # PTY 生命周期管理 + AI 会话检测
│       ├── process_monitor.rs  # 进程状态轮询
│       ├── config.rs           # 配置持久化
│       ├── fs.rs               # 目录列表与文件监听
│       └── ai_sessions.rs     # Claude/Codex 历史会话读取
└── package.json
```

## 架构概览

### 数据流

```
用户键入 → xterm.onData → invoke('write_pty') → Rust PTY writer
Rust PTY reader → 16ms 批量缓冲 → emit('pty-output') → term.write()
进程监控 → 500ms 轮询 → emit('pty-status-change') → StatusDot 更新
```

### 状态优先级

终端面板状态从叶节点聚合到标签页和项目级别：

```
error > ai-working > ai-idle > idle
```

### AI 会话检测

通过追踪用户终端输入识别 AI 会话：
- 输入 `claude` 或 `codex` + 回车 → 进入 AI 会话
- Ctrl+C / Ctrl+D / `/exit` → 退出 AI 会话

### 会话历史读取

- **Claude** — 扫描 `~/.claude/projects/<编码路径>/` 下的 JSONL 文件
- **Codex** — 遍历 `~/.codex/sessions/` 目录，匹配项目路径

## 推荐开发环境

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
