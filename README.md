<p align="center">
  <img src="src-tauri/icons/icon.png" width="128" height="128" alt="Mini-Term Logo">
</p>

<h1 align="center">Mini-Term</h1>

<p align="center">
  <strong>为 AI 时代打造的桌面终端管理器</strong><br>
  多项目 · 多标签 · 递归分屏 · AI 状态感知 · SSH 远程 · Git Worktree · 手机远程看 AI
</p>

<p align="center">
  <strong>简体中文</strong> · <a href="README.en.md">English</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-0.8.5-blue" alt="version">
  <img src="https://img.shields.io/badge/platform-Windows-0078D4" alt="platform">
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux-experimental-lightgrey" alt="platform-experimental">
  <img src="https://img.shields.io/badge/Tauri-v2-orange" alt="tauri">
  <img src="https://img.shields.io/badge/React-19-61dafb" alt="react">
  <img src="https://img.shields.io/badge/Rust-2021-dea584" alt="rust">
</p>

<p align="center">
  <a href="https://github.com/dreamlonglll/mini-term/releases">下载安装包</a> ·
  <a href="docs/features.zh-CN.md">完整功能清单</a> ·
  <a href="docs/deploy-relay.zh-CN.md">中转部署</a>
</p>

---

## 一个场景

你同时开着 4 个 Claude Code 会话，分散在 3 个项目里。

**哪个跑完了？哪个卡在等你确认？** 系统终端不会告诉你，你只能一个个点开看。为了这点事去开 VS Code / IDEA，又是几百兆内存换一个终端窗口。

Mini-Term 就是为这件事做的：项目列表上的状态灯实时跳动，AI 一跑完立刻弹提醒、任务栏闪烁、响一声；出门在外掏出手机，看到的是同一份现场，还能直接给它发下一条指令。

![主界面](docs/screenshots/main.png)

---

## 六个最值得一试的地方

### 🔔 AI 跑完了，你第一时间知道

不是靠猜进程名——Mini-Term 直接接入了 **Claude Code / Codex 官方 Hook API**，SessionStart / ToolUse 等事件实时上报，比轮询更准更快（进程轮询作为降级兜底保留）。设置里一键注册 / 卸载 Hook，**合并而不是覆盖**你已有的 hook 配置。

状态从「面板 → 标签页 → 项目」逐层聚合（`error > ai-working > ai-idle > idle`），任务从 working 转 idle 的瞬间触发四件事，每一项都能单独开关：

- 右下角 Toast 通知（只对非活跃项目弹，同项目自动去重）
- 项目列表 **DONE** 徽章
- 任务栏闪烁（Windows）/ Dock 跳动（macOS），仅窗口失焦时触发
- 提示音（内置合成音，也可以换成你自己的音频文件）

### 📱 出门在外，用手机看桌面上跑着的 AI

这大概是 Mini-Term 最不一样的地方。

顶栏「移动端」面板里填好中转地址 → 保存并连接 → 生成配对二维码，**手机相机一扫就进 PWA 自动配对**。之后你在外面能：

- 看**按项目分组的活跃 AI 会话列表**，状态灯与桌面端实时同步
- 点进任一会话**实时看对话镜像**，AI 回复 Markdown 渲染，往上滚自动分页加载更早的消息
- 在底部输入框**直接发指令**，等价于你本人在桌面键盘上敲下并回车，带即时回执
- **从手机发起一个全新会话**：选项目 → 选 AI 启动器，桌面端后台开标签把 agent 拉起来，起来后手机自动进入它的对话镜像
- 给会话**改个看得懂的名字**，同步显示到桌面端的标签上

安全边界是认真设计过的：配对码一次性有效（10 分钟），新设备配对自动顶替旧设备，「重置配对」立即吊销全部凭证；**中转服务器只转发不落盘**，日志仅留元数据（有子进程级自动化测试断言全流程零文件残留）；AI 启动器的**命令文本从不经过手机或中转**，手机只按 id 引用、只看得到名字。

> **前提**：中转要跑在**你自己的**服务器上（1C1G 足够，Docker 一条命令起，另需一个解析到它的域名做 TLS）。这是刻意的设计——没有任何第三方服务掺在中间。见[部署文档](docs/deploy-relay.zh-CN.md)。

### 🧰 把你的 SSH 连接，变成 AI 能调用的工具

保存好的 SSH 连接可以作为 **MCP 工具**暴露给终端里跑着的 Claude Code / Codex。项目右键「关联 SSH」勾选连接即按项目启用，**可见范围就限定在你勾的那几个**。

内置 `mt-ssh-mcp` sidecar（基于官方 rmcp 的 stdio MCP server）提供四个工具：`ssh_list_connections`、`ssh_exec`、`ssh_upload`、`ssh_download`。`ssh_exec` 复用你存好的密码 / 私钥认证，带超时、输出封顶与审计日志；上传下载走 **SFTP 分块流式传输**，内存恒定、能传大文件，不必再用 `ssh_exec` + base64 echo 那套受输出封顶限制的土办法。

sidecar 内部维护**进程内 SSH 会话池**（russh + tokio）：首次调用某连接做一次握手 + 认证（秒级），之后每条命令只花一个 RTT。还有一条硬护栏——拒绝传输 mini-term 自己的 `config.json`（里面是全部 SSH 明文凭据）。

启用 / 停用会按命名 marker **幂等写入** Claude 的 `.mcp.json` 与 Codex 的 `.codex/config.toml`，不会弄乱你手写的配置。

### 🌐 远程目录当本地项目用，WSL 也一样

**SSH 远程项目**——把服务器上的目录直接添加成项目：文件树经 SFTP 懒加载展开，终端 `ssh -t` 直连并自动落到项目目录，断线后覆盖层一键重连，远程机器上的 Claude / Codex 历史会话也能按时间混排读出来看正文。远程缓存键掺入连接 id，两台服务器上的同名路径不会串数据。

**WSL 支持**——`\\wsl$\<distro>\<path>` 直接当项目根。检测到 cwd 是 WSL 路径时自动改用 `wsl.exe --cd` 启动，`pwd` 真的落在 WSL 里而不是 `C:\Windows`。Windows 下还能直接读 WSL 发行版内的 Claude / Codex 会话历史（走 UNC + 注册表枚举，不 spawn `wsl.exe`）。

### 🪟 多项目 · 递归分屏 · 会话历史

- **左侧项目列表**管理多个工作区，支持**最多 3 级嵌套分组**、拖拽排序、从资源管理器拖文件夹直接添加
- **横竖任意嵌套的递归分屏**，拖拽调比例；标签 / 分屏 / 窗口大小位置全部持久化，重启原样恢复
- **终端缓存**——切项目、切标签、切分屏都不重建 xterm 实例，内容不丢；启动按需懒加载，只给当前可见的 pane 建 PTY，历史项目再多也不拖慢启动
- **10 万行滚动缓冲**，正确处理 CSI 3J，Codex 的流式内容折叠与 `/clear` 都能如实反映；Windows 版内置固定版本官方 ConPTY 运行时，跨 Windows 版本行为一致
- **AI 会话历史**——读本地 Claude / Codex 记录，右键复制恢复命令快速续接，也能直接看完整对话正文（Markdown 渲染 + `Ctrl+F` 搜索）
- **AI 任务标记**——会话里每次按 Enter 自动打点，`Ctrl+Shift+↑/↓` 在历史提交之间跳转

### 🌿 Git 集成 + Worktree 批量管理

VS Code 风格的 **Changes 面板**（Staged / Changes / Untracked 分组，单文件或全量 stage / discard，`Ctrl+Enter` 提交），并排 / 内联双视图 Diff，游标分页的提交历史，以及**手绘 SVG 分支拓扑图**（按 lane 布局与上色，合并提交实心点套外环，后端 revwalk 加 TOPOLOGICAL 排序避免 rebase 后连线断裂）。

**Worktree 管理**对多 Agent 并行开发特别有用：项目根目录本身不是仓库时会**向下扫描子仓库**并按主工作区归并，组头可勾选多选 / 全选，**一次为每个勾选的仓库各建一个 worktree**（分支下拉取各仓库分支的交集）。建好的 worktree 可以一键「设为项目」挂到主项目下面成为子项目，或者直接开个终端进去。

![Git 集成](docs/screenshots/git.png)

---

## 还有一堆为「跟 AI 一起工作」调过的细节

| | |
|---|---|
| **长文本粘贴** | 剪贴板 ≥10 行或 ≥2000 字符时自动转存临时 `.txt`，粘贴带引号的路径——AI 工具不必硬吞超长内容 |
| **图片粘贴** | 剪贴板里有截图自动检测，存成临时 PNG 并粘路径，兼容 PinPix 等非标准格式 |
| **远程自动落地** | 上面两种粘贴在 SSH 远程项目里会经 SFTP 传到远端再粘**远端**路径；WSL 项目自动把 `C:\...` 换算成 `/mnt/c/...` |
| **文件拖拽** | 从文件树或资源管理器拖文件到终端，插入带引号的绝对路径，精准落到目标分屏 |
| **内置文件编辑器** | 文件树点开即改：CodeMirror 6 内核，140+ 语言语法高亮按需加载，查找替换、代码折叠、多光标，`Ctrl+S` 原子落盘，外部改动自动感知，Markdown 实时预览未保存草稿 |
| **全局搜索** | `Ctrl+Shift+F` 唤起，文件名 / 内容双模式，子串或正则，后端流式推送随时可取消 |
| **项目级环境变量** | 按项目注入 PTY 子进程，严格 POSIX 校验，Rust 端二次防御，WSL 下经 WSLENV 透传 |
| **智能 Ctrl+C/V** | 可选开启：有选区时复制、无选区时中断程序；Windows 大段粘贴自动分块防 ConPTY 丢行 |
| **三种主题 + 蓝图皮肤** | Auto / Light / Dark（暖炭色调），另有科幻风蓝图皮肤；Windows 原生标题栏跟随，启动无浅色闪烁 |
| **中英双语** | 一键切换全界面实时重渲染，首次启动按系统语言探测，自研轻量 i18n 无额外运行时依赖 |
| **连体字** | `==` `=>` `!=` `->` 合成 ligature glyph（需 Fira Code / JetBrains Mono 等含 calt 表的字体） |

---

## 技术栈

| 层 | 技术 |
|---|---|
| 框架 | Tauri v2（Rust 后端 + 系统 WebView，安装包小、常驻内存低） |
| 前端 | React 19 + TypeScript 5.8 + Tailwind CSS v4 + Vite 7 |
| 终端 | xterm.js v6（WebGL 加速，自动降级 Canvas） |
| 状态 / 布局 | Zustand 单一 Store · Allotment + 递归 SplitNode 分屏树 |
| PTY / Git | portable-pty · git2 · notify + ignore |
| 移动端中转 | axum + tokio WebSocket（`relay-server/`）· React + Vite PWA（`mobile/`） |
| 测试 | **491 个 Rust 测试**（桌面端 438 + 中转 53）+ 19 个 Node 测试 |

---

## 快速开始

### 下载安装

前往 [Releases](https://github.com/dreamlonglll/mini-term/releases) 下载最新安装包。

> **平台支持**
> - **Windows** — 主要支持平台，保证可用性，日常开发与测试都在 Windows 上
> - **macOS / Linux** — 代码层面已支持，但**可用性欠佳**、未经充分打磨，欢迎提 Issue

macOS 首次打开若提示 "is damaged and can't be opened"，是因为 Release 产物没有 Apple Developer ID 签名被 Gatekeeper 拦下，不是文件真的坏了。拖进 `/Applications` 后执行一次即可：

```bash
xattr -cr /Applications/Mini-Term.app
```

### 从源码构建

需要 Node.js >= 20.19（或 >= 22.12）、Rust >= 1.85、[Tauri v2 CLI](https://v2.tauri.app/start/prerequisites/)。

```bash
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term
npm install
npm run tauri dev      # 开发（前端 + 后端）
npm run tauri build    # 构建发布包
```

---

## 更多

- 📖 **[完整功能清单](docs/features.zh-CN.md)** — 每一项功能的详细说明、架构概览与边界条件
- 📱 **[中转服务部署文档](docs/deploy-relay.zh-CN.md)** — 手机远程功能所需的自托管中转
- 🐛 **[提 Issue / PR](https://github.com/dreamlonglll/mini-term/issues)** — 外部贡献会经过功能验证和安全审查后合并

学 AI，上 L 站 — [LinuxDO](https://linux.do/)
