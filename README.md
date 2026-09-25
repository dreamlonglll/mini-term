<p align="center">
  <img src="docs/icon.png" width="128" height="128" alt="Mini-Term Logo">
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
  <img src="https://img.shields.io/badge/version-1.13.10-blue" alt="version">
  <img src="https://img.shields.io/badge/platform-Windows-0078D4" alt="platform">
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux-experimental-lightgrey" alt="platform-experimental">
  <img src="https://img.shields.io/badge/GPUI-native-8A2BE2" alt="gpui">
  <img src="https://img.shields.io/badge/Rust-1.95%2B-dea584" alt="rust">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="license">
</p>

<p align="center">
  <a href="https://github.com/dreamlonglll/mini-term/releases">下载安装包</a> ·
  <a href="docs/features.zh-CN.md">完整功能清单</a> ·
  <a href="docs/deploy-relay.zh-CN.md">中转部署</a>
</p>


**GPUI 原生实现**：Rust 原生渲染、单进程，界面不依赖 WebView2（只有 HTML 预览按需调用系统 WebView，缺失时回落简版渲染）。

> 早期的 Tauri + React 实现已于 v1.0.0-beta 后从仓库移除并停止发布（历史版本安装包仍可在旧 Release 下载，源码看 git 历史）。

---

## 适用人群

- **多项目拥有者**
- **VsCode 风格爱好者**
- **终端爱好者**

![主界面](docs/screenshots/main.png)

---



## 一堆为「跟 AI 一起工作」调过的细节

| 功能                 | 描述                                                         |
|:--|---|
| **AI事件通知** | 直接接入 **Claude Code / Codex / Grok Build / oh-my-pi 官方 Hook API**（oh-my-pi 走它的进程内扩展机制），使用轮询作为兜底。<br />注意：windows端的 GrokBuild 需要将本程序安装到`非空格`目录，才能保证hook功能正常<br />Windows 下卸载 Mini-Term 时会自动摘掉它写进各家 AI 工具配置里的 hook 条目（升级安装不动） |
| **外置主题包** | 兼容 Dream Skin 格式的皮肤：文件夹或 zip 导入、manifest 的 sha256 校验、改文件即热重载；皮肤可自带背景图，终端随之透明化压在氛围层上。外链一律走同一道闸（禁 `@import`，指向包外的引用全拒）。点「更多皮肤」直达仓库 [`theme/`](theme/) 皮肤库（现有深色 Blue Hour 蓝调时分、浅色 Morning Mist 晨雾两款），挑一份下载后导入即用；想自己做一份，字段说明在 [`docs/theme-pack-example/`](docs/theme-pack-example/) |
| **手机端支持** | **前提**：中转要跑在**你自己的**服务器上（1C1G 足够，Docker 一条命令起，另需一个解析到它的域名做 TLS）。见[部署文档](docs/deploy-relay.zh-CN.md)。 |
| **费用统计** | 顶栏「统计」打开使用统计面板：Claude Code / Codex / Grok 的**成本、调用、会话数**多维聚合，按日 / 按小时趋势图，模型、项目排行与 Top 会话，范围和口径随手切。<br />数据计算方式参考 ccusage 项目 [ccusage/ccusage: npx ccusage](https://github.com/ccusage/ccusage) |
| **SSH支持** | **SSH 远程项目** — 服务器上的目录直接添加成项目：文件树经 SFTP 懒加载，终端 `ssh -t` 直连并自动落到项目目录，断线后覆盖层一键重连，远程机器上的 Claude / Codex 历史会话也能读出正文。远程缓存键掺入连接 id，两台服务器上的同名路径不会串数据；连接管理弹窗按分组归类，连接可拖拽排序 / 换组，密码加密保存 <br /><br />**WSL 支持** — `\\wsl$\<distro>\<path>` 直接当项目根，自动改用 `wsl.exe --cd` 启动，`pwd` 真的落在 WSL 里而不是 `C:\Windows`；Windows 下还能直接读 WSL 发行版内的 Claude / Codex 会话历史<br /><br />**供Agent调用** 通过内置Skill，允许AI通过SSH远程执行服务器命令。 项目右键「关联 SSH」勾选连接即按项目启用 |
| **Markdown 预览** | 文件树点开 `.md` 即按块虚拟化渲染（长文档滚动不重排整篇）：```` ```mermaid ```` 围栏纯 Rust 渲染成图表（不依赖浏览器 / Node，跟随亮暗主题，出错退回代码块；点击图表整窗放大，滚轮缩放、拖动平移）、本地与网络图片、GFM 表格、链接按四类处置（外链先确认、锚点滚到标题、本地文件开新页签），行内代码按主题强调色显示（橙字深底） |
| **HTML 预览** | 本地 `.html` 用系统 WebView（Windows 为 WebView2）真渲染，CSS 与脚本照跑、效果与浏览器一致，相对路径的样式 / 脚本 / 图片按项目目录加载，改完源码未保存也能切预览看效果；外链先确认再交浏览器，链到本地文件作为新页签打开，页面脚本读不到项目里的其它文件。WebView 不可用或 Linux 上回落简版渲染 |
| **终端页签** | 页签最左按 shell 显示图标（pwsh / Windows PowerShell / cmd / bash / zsh / fish / nu / WSL 各一枚），**标题跟随 shell 报的窗口标题**：oh-my-posh 的当前目录直接缀在 shell 名后，几个同名 pwsh 一眼分清；shell 自己的默认标题不显示、AI 会话跑着时只留品牌图标，可在「终端」设置里关闭。工作台页签与终端页签两层分明：只有页级保留强调色顶线，终端页签是圆角胶囊，关闭按钮悬停才现身 |
| **Git集成** | VS Code 风格的 **Changes 面板**（Staged / Changes / Untracked 分组，单文件或全量 stage / discard，`Ctrl+Enter` 提交），并且支持 **Worktree 管理**（对项目右键-> Worktree管理）；提交历史增量分页，十万级提交的大仓库首页也即点即出 |
| **长文本粘贴** | 剪贴板 ≥10 行或 ≥2000 字符时自动转存临时 `.txt`，粘贴带引号的路径——AI 工具不必硬吞超长内容 |
| **图片粘贴** | 剪贴板里有截图自动检测，存成临时 PNG 并粘路径，兼容 PinPix 等非标准格式； |
| **远程自动落地** | 上面两种粘贴在 SSH 远程项目里会经 SFTP 传到远端再粘**远端**路径；WSL 项目自动把 `C:\...` 换算成 `/mnt/c/...` |
| **文件拖拽** | 从文件树或资源管理器拖文件到终端，插入带引号的绝对路径，精准落到目标分屏 |
| **命令库** | 终端控制条「命令」钮（或 `Ctrl+Shift+K`）弹出全局命令库：常用命令按分组保存，点一下写进当前终端并回车，`Ctrl+↵` / `Ctrl+点击` 只粘贴不回车方便先改参数；浮层内直接搜索、新增、编辑、删除、建组，与项目 / SSH 连接无关，哪台机器上都是同一份 |
| **全局搜索** | `Ctrl+Shift+F` 唤起，文件名 / 内容双模式（文件名含 `/` 即按路径匹配），子串或正则，后端流式推送随时可取消；内容搜索并行读文件，跳过 4MB 以上与二进制文件，命中收满 1000 条即停（状态条标「1000+」） |
| **项目级环境变量** | 按项目注入 PTY 子进程，严格 POSIX 校验，Rust 端二次防御，WSL 下经 WSLENV 透传 |
| **智能 Ctrl+C/V** | 默认开启：有选区时复制、无选区时中断程序，`Ctrl+V` 直接粘贴；Windows 大段粘贴自动分块防 ConPTY 丢行 |
| **拖选停留自动复制** | 拖选后按住鼠标静止超过设定时长自动复制选区并弹「已复制」气泡，时长可调（0 = 关闭） |
| **Alt+单击定位光标** | 按住 Alt（macOS ⌥）单击命令行任意位置，光标直接挪过去——同一行内按列差合成方向键；跨行一律不动，免得触发行编辑器的历史召回。shell 提示符下逐格准确，Claude CLI 这类 Ink TUI 不保证 |
| **启动零网络请求** | 原生渲染无 Web 资源，启动不发任何网络请求（价格表按天拉取，拉不到用缓存）；启动时配置只读一次、备份挪到后台，首帧出得更快 |
| **刷屏不卡界面** | PTY 字节在后台线程直喂 VT 状态机、UI 按帧取格子渲染——单进程零 IPC，没有中间缓冲可堆积，`cat` 大文件也拖不垮界面；终端刷屏只重绘终端自己，闲置的分屏终端直接复用上一帧，窗口最小化时渲染整个停掉、一帧不画；终端重绘帧率可在「系统 → 性能」里调（前台 10~240、默认 30，失焦 1~60、默认 5），高刷屏想更顺就调高，笔记本想省电就调低，改完即时生效 |
| **新版 ConPTY** | Windows 下随包附带 Windows Terminal 1.24 的 `conpty.dll` + `OpenConsole.exe` 并在启动时预载，绕开老版本系统 conhost 的宽字符列宽、换行回绕、resize 丢行等已知缺陷；怀疑显示问题出在它身上时，设环境变量 `MT_DISABLE_PORTABLE_CONPTY=1` 即回落系统 ConPTY |
| **添加项目即打开** | 弹窗、分组右键、拖目录进列表、SSH 远程、Worktree 设为项目——任何一条入口添加完直接切过去并开好第一个终端，不必再面对空态页点一次「新建终端」 |
| **项目行悬停预览** | 悬停 250ms 弹出该项目正在运行的 AI Session 终端区 |
| **设置面板分组** | 侧栏两级菜单：终端、外观、AI、系统，每页只剩一屏，不用滚半页找开关 |

---

## 技术栈

整套应用为 **Rust 原生实现**：

| 层 | 实现 |
|---|---|
| 壳 / 渲染 | GPUI（gpui-pre 0.3，Zed 2026-09 快照；GPU 原生渲染，单进程；仅 HTML 预览按需嵌入系统 WebView） |
| UI | 纯 Rust：gpui-component + 自绘组件 |
| 终端 | alacritty_terminal（进程内 VT 解析，零 IPC、零序列化）· portable-pty · Windows 随包 ConPTY（Windows Terminal 1.24） |
| 状态 / 布局 | 单一 Store · 递归 SplitNode 分屏树 |
| 配置 / 布局持久化 | rusqlite（`config.db` 配置本体 · `layout.db` 界面布局） |
| Git / 文件 | git2（libgit2）· notify + ignore |
| 用量统计 | rusqlite 本地账本 · 自绘趋势图 |
| 移动端中转 | axum + tokio WebSocket（`relay-server/`）· React + Vite PWA（`mobile/`） |
| 测试 | **2129 个 Rust 测试**（33 个测试目标） |

---

## 快速开始

### 下载安装

前往 [Releases](https://github.com/dreamlonglll/mini-term/releases) 下载，三平台产物：

- **Windows x64（主要支持平台）** — `Mini-Term_*_x64-setup.exe` 安装包（NSIS，用户级安装免管理员；装过旧版的默认原目录升级，且**先卸载旧版再装**而不是文件覆盖写；桌面快捷方式在组件页自行勾选，升级时沿用上一次的选择）
- **macOS arm64（Apple Silicon）** — `Mini-Term_*_aarch64.dmg`
- **macOS x64（Intel）** — `Mini-Term_*_x64.dmg`
- **Linux x64** — `Mini-Term_*_amd64.deb` 或 `Mini-Term_*_amd64.tar.gz`

> **平台支持**
> - **Windows** — 主要支持平台，保证可用性，日常开发与测试都在 Windows 上
> - **macOS / Linux** — 代码层面已支持，但**可用性欠佳**、未经充分打磨，欢迎提 Issue

macOS 首次打开若提示 "is damaged and can't be opened"，是因为 Release 产物没有 Apple Developer ID 签名被 Gatekeeper 拦下，不是文件真的坏了。拖进 `/Applications` 后执行一次即可：

```bash
xattr -cr /Applications/Mini-Term.app
```

### 从源码构建

需要 Rust >= 1.95（仓库根 `rust-toolchain.toml` 钉在 1.98.1，装了 rustup 会自动切过去）；构建 sidecar 就位脚本需要 Node.js >= 20（仅标准库，无 npm 依赖）。

```bash
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term

node scripts/stage-sidecars.mjs      # 构建三个 sidecar 并连同便携 ConPTY 就位到 target/debug/
cargo run -p mt-app                  # 开发
cargo build --release -p mt-app      # 产物 target/release/mini-term(.exe)
```

> hook 上报与便携 ConPTY 按「与 exe 同目录」定位 sidecar 与资源，发布包已带齐；源码运行要完整体验，先跑一次 `stage-sidecars.mjs`（release 构建对应 `--release`，就位到 `target/release/`）。

---

## 更多

- 📖 **[完整功能清单](docs/features.zh-CN.md)** — 每一项功能的详细说明、架构概览与边界条件
- 📱 **[中转服务部署文档](docs/deploy-relay.zh-CN.md)** — 手机远程功能所需的自托管中转
- 🐛 **[提 Issue / PR](https://github.com/dreamlonglll/mini-term/issues)** — 外部贡献会经过功能验证和安全审查后合并

## 许可证

本项目基于 [MIT 协议](LICENSE) 开源。

学 AI，上 L 站 — [LinuxDO](https://linux.do/)
