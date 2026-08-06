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
  <img src="https://img.shields.io/badge/version-0.10.5-blue" alt="version">
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

## 八个最值得一试的地方

### 🔔 AI 跑完了，你第一时间知道

不是靠猜进程名——Mini-Term 直接接入了 **Claude Code / Codex 官方 Hook API**，SessionStart / ToolUse 等事件实时上报，比轮询更准更快（进程轮询作为降级兜底保留）。设置里一键注册 / 卸载 Hook，**合并而不是覆盖**你已有的 hook 配置。

状态从「面板 → 标签页 → 项目」逐层聚合（`error > ai-working > ai-idle > idle`），任务从 working 转 idle 的瞬间触发四件事，每一项都能单独开关：

- 右下角 Toast 通知（只对非活跃项目弹，同项目自动去重）
- 项目列表 **DONE** 徽章
- 任务栏闪烁（Windows）/ Dock 跳动（macOS），仅窗口失焦时触发
- 提示音（内置合成音，也可以换成你自己的音频文件）

窗口切走之后还有**状态栏图标**接力（Windows 托盘 / macOS 菜单栏）：黄=待确认、蓝=处理中、绿=完成未读、灰=安静。左键点一下**直接落到该处理的那个会话**——切项目 + 激活到具体分屏并聚焦终端，优先级是「待确认/异常 > 最先完成 > 处理中」，与标题栏状态灯同一套口径；右键菜单里点某个项目则定位到该项目内最该处理的分屏。不想让它改变当前视图的话，设置里可以关掉。

Claude / Codex 之外，终端里跑 **opencode / pi** 也会被认出来——不靠 hook，而是识别你敲下的命令，状态灯、完成播报与手机端发指令照常可用；只是这两家没有可解析的本地会话记录，所以对话镜像、AI 历史面板和用量统计对它们是空的，也不会去蹭同项目里 Claude / Codex 的会话。

Hook 一旦接入，就是该面板的状态来源：完成信号只认 `Stop` 事件，权限审批框弹出同样是「在等你」，不会被当作任务完成。剩下的麻烦是**徽章卡住**——`Stop` 在若干情形下根本不触发（回合因 API 错误结束、你自己按 Esc 打断），这些已按官方事件逐个补齐；再兜一层「停摆判定」：状态与终端输出双双静默 10 秒就把徽章摘下来，此前若已触发过退出（Ctrl+D、连按两次 Ctrl+C、`/exit`）则直接判为已退出。兜底结论一次写定、不会来回摆动，所以不会重演早期版本那种「同一个任务每隔二三十秒播报一次完成」。

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

### 📊 这个月 AI 花了多少钱，一眼看到

顶栏「统计」打开使用统计面板：Claude Code / Codex 的**成本、调用、会话数**多维聚合，按日 / 按小时趋势图，模型、项目排行与 Top 会话，范围和口径随手切。

数据从本地会话记录解析进 **rusqlite 账本**——面板毫秒级出数，后台增量同步补新账；fork 复制出来的历史**不会重复计费**，缓存读写按官方价差精确计价。价格表每天从 models.dev 拉一次（只读公开价目，**不上传任何用量数据**），拉不到就用缓存，绝不拿假数据糊你。

### 🔁 重启不断线：AI 会话自动续接

关掉 Mini-Term 再打开，上次每个分屏里跑着的 Claude / Codex 会**自动 `--resume` 续回原会话**——会话身份来自 hook 上报、随布局一起持久化，跨一次重启还在。写回终端前有白名单校验兜底：识别不了的一律不写，远程 pane 不参与，宁可不续也不敲错命令。不想让它自己敲命令的，「设置 → 系统 → 常规」一个开关关掉——终端照常恢复，只是不自动跑续接。

### 🧰 把你的 SSH 连接，变成 AI 能调用的工具

保存好的 SSH 连接可以交给终端里跑着的 Claude Code / Codex 直接操作。项目右键「关联 SSH」勾选连接即按项目启用，**可见范围就限定在你勾的那几个**。

**v0.9.0 起这套工具从 MCP 换成了 CLI + Skill**：启用时生成 Claude / Codex 两份 `SKILL.md`（内嵌 CLI 绝对路径与该项目的随机能力令牌，幂等追加 `.gitignore`，并自动摘除存量项目里的旧 MCP 注册）。好处是 agent 按需加载 skill，不再有一份工具 schema 常驻上下文；调用的是普通命令行，可以和 `grep`、管道、重定向自由组合。

内置 `mt-ssh-cli` sidecar 提供 `list` / `exec` / `upload` / `download` 四个子命令：远程 stdout / stderr 与退出码**原样流式透传**（`124` 超时、`2` CLI 错误），上传下载走 **SFTP 分块流式传输**（内存恒定、能传大文件），认证凭据始终留在本机，每次调用写审计日志。**每条命令都必须带项目令牌**——缺失、未知、重复或属于已停用项目的一律拒绝执行，绝不回退成「能看见全部连接」。

CLI 背后是**全机单例 daemon** 持有的持久连接池：首次调用自动把它拉起来并完成握手 + 认证（秒级），之后每条命令只花一个 RTT；空闲 10 分钟 drain 自退，版本升级自动换代；`Ctrl+C`、客户端断开或超时会显式关掉对应 SSH channel，健康 session 继续留在池里。IPC 端点只有当前用户能连，建不起来就 fail closed；daemon 不可用时自动降级为进程内直连，输出与退出码契约完全一致。还有一条硬护栏——拒绝传输 mini-term 自己的 `config.json`（里面是全部 SSH 明文凭据）。

> 过渡期 `mt-ssh-mcp` MCP sidecar 仍随安装包发布，计划下个周期下线。

### 🌐 远程目录当本地项目用，WSL 也一样

**SSH 远程项目**——把服务器上的目录直接添加成项目：文件树经 SFTP 懒加载展开，终端 `ssh -t` 直连并自动落到项目目录，断线后覆盖层一键重连，远程机器上的 Claude / Codex 历史会话也能按时间混排读出来看正文。远程缓存键掺入连接 id，两台服务器上的同名路径不会串数据。

**WSL 支持**——`\\wsl$\<distro>\<path>` 直接当项目根。检测到 cwd 是 WSL 路径时自动改用 `wsl.exe --cd` 启动，`pwd` 真的落在 WSL 里而不是 `C:\Windows`。Windows 下还能直接读 WSL 发行版内的 Claude / Codex 会话历史（走 UNC + 注册表枚举，不 spawn `wsl.exe`）。

### 🪟 多项目 · 递归分屏 · 会话历史

- **左侧项目列表**管理多个工作区，支持**最多 3 级嵌套分组**、拖拽排序、从资源管理器拖文件夹直接添加
- **横竖任意嵌套的递归分屏**，拖拽调比例；标签 / 分屏 / 窗口大小位置全部持久化，重启原样恢复
- **终端缓存**——切项目、切标签、切分屏都不重建 xterm 实例，内容不丢；启动按需懒加载，只给当前可见的 pane 建 PTY，历史项目再多也不拖慢启动
- **滚动缓冲行数可调**（默认 1 万行，设置里改小当场生效并释放内存），正确处理 CSI 3J，Codex 的流式内容折叠与 `/clear` 都能如实反映；Windows 版内置固定版本官方 ConPTY 运行时，跨 Windows 版本行为一致
- **AI 会话历史**——读本地 Claude / Codex 记录，右键复制恢复命令快速续接，也能直接看完整对话正文（Markdown 渲染 + `Ctrl+F` 搜索）
- **AI 任务标记**——会话里每次按 Enter 自动打点，`Ctrl+Shift+↑/↓` 在历史提交之间跳转

### 🌿 Git 集成 + Worktree 批量管理

VS Code 风格的 **Changes 面板**（Staged / Changes / Untracked 分组，单文件或全量 stage / discard，`Ctrl+Enter` 提交），并排 / 内联双视图 Diff，游标分页的提交历史，以及**手绘 SVG 分支拓扑图**（按 lane 布局与上色，合并提交实心点套外环，后端 revwalk 加 TOPOLOGICAL 排序避免 rebase 后连线断裂）。

**Worktree 管理**对多 Agent 并行开发特别有用：项目根目录本身不是仓库时会**向下扫描子仓库**并按主工作区归并，组头可勾选多选 / 全选，**一次为每个勾选的仓库各建一个 worktree**（分支下拉取各仓库分支的交集）。建好的 worktree 可以一键「设为项目」挂到主项目下面成为子项目，或者直接开个终端进去。**AI agent 在终端里把 worktree 删掉之后**，回到窗口时列表会自动把目录已消失的子项目连同终端资源一起收掉，不留失效条目（只在父项目还在时清理，盘符掉线不会误删）。

![Git 集成](docs/screenshots/git.png)

---

## 还有一堆为「跟 AI 一起工作」调过的细节

| | |
|---|---|
| **长文本粘贴** | 剪贴板 ≥10 行或 ≥2000 字符时自动转存临时 `.txt`，粘贴带引号的路径——AI 工具不必硬吞超长内容 |
| **图片粘贴** | 剪贴板里有截图自动检测，存成临时 PNG 并粘路径，兼容 PinPix 等非标准格式 |
| **远程自动落地** | 上面两种粘贴在 SSH 远程项目里会经 SFTP 传到远端再粘**远端**路径；WSL 项目自动把 `C:\...` 换算成 `/mnt/c/...` |
| **文件拖拽** | 从文件树或资源管理器拖文件到终端，插入带引号的绝对路径，精准落到目标分屏；拖到一半改主意按 Esc 就地取消，路径不写入、也不会退化成一次点击打开文件 |
| **内置文件编辑器** | 文件树点开即改：CodeMirror 6 内核，140+ 语言语法高亮按需加载，查找替换、代码折叠、多光标，`Ctrl+S` 原子落盘，外部改动自动感知，Markdown 实时预览未保存草稿 |
| **全局搜索** | `Ctrl+Shift+F` 唤起，文件名 / 内容双模式，子串或正则，后端流式推送随时可取消 |
| **项目级环境变量** | 按项目注入 PTY 子进程，严格 POSIX 校验，Rust 端二次防御，WSL 下经 WSLENV 透传 |
| **智能 Ctrl+C/V** | 可选开启：有选区时复制、无选区时中断程序；Windows 大段粘贴自动分块防 ConPTY 丢行 |
| **满屏图标** | 文件树 Material 主题文件图标、项目行 AI 品牌图标与技术栈图标——全量图标数据独立 chunk 按需懒加载，主包零增量 |
| **拖选停留自动复制** | 拖选后按住鼠标静止超过设定时长自动复制选区并弹「已复制」气泡，时长可调（0 = 关闭） |
| **项目描述** | 右键给项目补一行灰色小字备注，一排 worktree 子项目各自在干什么一眼分清 |
| **启动零网络请求** | 字体本地打包（移除 Google Fonts 外链），重型弹窗全部懒加载，主包 gzip 从 631KB 降到 378KB |
| **全链路背压** | `cat` 大文件、AI 刷屏时，前端积压过高会一路顶回刷屏进程本身——慢终端拖慢进程，而不是把数据全堆进内存；万一渲染进程被系统杀掉重载，上一轮遗留的 PTY 也会先回收，不会崩一次漏一整套 |
| **三种主题 + 蓝图皮肤** | Auto / Light / Dark（暖炭色调），另有科幻风蓝图皮肤；标题栏与主题同色，启动无浅色闪烁 |
| **自定义标题栏** | 无边框窗口 + 自绘标题栏，配色跟着主题走不再是系统那条灰白；按平台适配习惯——Windows / Linux 右侧三键并保留 Win11 贴靠布局（悬停最大化按钮弹分屏菜单），macOS 保留原生交通灯。右侧状态灯汇总所有项目的 AI 状态，点一下直接跳到下一个该处理的会话（最先完成的排最前） |
| **中英双语** | 一键切换全界面实时重渲染，首次启动按系统语言探测，自研轻量 i18n 无额外运行时依赖 |
| **连体字** | `==` `=>` `!=` `->` 合成 ligature glyph（需 Fira Code / JetBrains Mono 等含 calt 表的字体） |
| **设置面板分组** | 侧栏两级菜单：终端（Shell / 复制粘贴）、外观（主题与语言 / 字体）、AI（完成通知 / Hook 事件）、系统（常规 / 外部编辑器），每页只剩一屏，不用滚半页找开关 |

---

## 技术栈

| 层 | 技术 |
|---|---|
| 框架 | Tauri v2（Rust 后端 + 系统 WebView，安装包小、常驻内存低） |
| 前端 | React 19 + TypeScript 5.8 + Tailwind CSS v4 + Vite 7 |
| 终端 | xterm.js v6（WebGL 加速，自动降级 Canvas） |
| 状态 / 布局 | Zustand 单一 Store · Allotment + 递归 SplitNode 分屏树 |
| PTY / Git | portable-pty · git2 · notify + ignore |
| 用量统计 | rusqlite 本地账本 · recharts 趋势图 |
| 移动端中转 | axum + tokio WebSocket（`relay-server/`）· React + Vite PWA（`mobile/`） |
| 测试 | **609 个 Rust 测试**（桌面端 556 + 中转 53）+ 77 个 Node 测试 |

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

需要 Node.js >= 20.19（或 >= 22.12）、Rust >= 1.95、[Tauri v2 CLI](https://v2.tauri.app/start/prerequisites/)。

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
