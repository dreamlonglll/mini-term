# AGENTS.md

This file provides guidance to AI coding agents (Claude Code, Codex, etc.) when working with code in this repository.

## Project Overview

**mini-term** — GPUI 原生桌面终端管理器，支持多项目、多标签、分屏布局，并能感知 AI 进程（Claude/Codex/Grok 等）状态；带移动端中转镜像与 SSH 远程项目能力。

- **UI/渲染**: [gpui-fast](https://crates.io/crates/gpui-fast) 0.1.17（Longbridge 维护的 GPUI 分支，crates.io 版；根 Cargo.toml 里以 `gpui` 为别名引用，窗口/文字后端在 `gpui-fast-platform`）+ gpui-component 0.10.0（Resizable/Modal/Input/Tree 等）
- **终端**: alacritty_terminal（VT 状态机，进程内直喂，无 IPC）+ portable-pty
- **发布形态**: Windows x64 NSIS 安装包（`scripts/windows-installer.nsi`，包内平铺 exe + `miniterm-hook` + portable-conpty + 扩展清单 `extensions.json`，全部「与 exe 同目录」；`mt-ssh-cli` / `mt-ssh-mcp` 与大语法动态库 `mt-syntax-pack` 不再随包，改为按需扩展 `ssh-tools` / `syntax-pack`；调试符号 `mini_term.pdb` 另作单独的 zip 资产，不进安装包；卸载时摘掉六家 AI 工具里的 hook 注册、删掉按需扩展目录，升级都不动）+ macOS dmg + Linux deb/tar.gz（同样带 `extensions.json`）。远端 Git helper **不再随包**，改为按需扩展（见下文「按需扩展」）：四平台 `linux-x86_64` / `linux-aarch64`（musl 静态构建）、`macos-x86_64` / `macos-aarch64` 各是一个发布资产 `mt-ext_git-helper-<platform>_<哈希>.zip`，另有每个桌面平台一个离线扩展包 `Mini-Term_<版本>_<平台键>-extensions.zip`；按 SSH 服务器平台选择，不按桌面平台选择，本机缺失时先下载、下载不成明确报错且不回落本机 Git。目标与打包范围不表示已完成所有平台或服务器环境验证
- **历史**: 项目最初是 Tauri v2 + React 实现，v1.0.0-beta 后整体删除切换到 GPUI 原生版；找旧实现看 git 历史（合并点 `236d5c1`）

## 开发命令

```bash
# 首次/换版本后：构建三个 sidecar、语法包动态库并连同便携 ConPTY 就位到 target/debug/
node scripts/stage-sidecars.mjs

# 启动开发实例（⚠️ 与装机版并跑时必须隔离数据目录）
MT_APP_DATA_DIR="$LOCALAPPDATA/mini-term-gpui-dev" cargo run -p mt-app

# 全工作区测试（2026-09 实数：33 个目标 / 2076 例 —— 目标数 = Running + Doc-tests 行数，
# 例数 = 各 `test result:` 行 passed 之和；数字只是快照，随代码增长）
cargo test --workspace

# 量化每帧 CPU / 帧时间 p50·p99 / 一帧合并了几次 invalidation（dev-only 度量工具，
# 默认不编译；档位与读数解释见 crates/mt-app/src/frame_profiler.rs 的模块注释）
cargo build -p mt-app --features frame-profiler
MT_FRAME_OVERLAY=1 MT_APP_DATA_DIR="$LOCALAPPDATA/mini-term-gpui-dev" ./target/debug/mini-term

# 中转服务端协议边界测试 / sidecar 工作区
cd relay-server && cargo test
cargo build --manifest-path sidecars/Cargo.toml

# 移动端 PWA
cd mobile && npm run build

# 改文案后重新生成 i18n 字典
node crates/mt-i18n/tools/gen_from_ts.mjs

# 改 oh-my-pi / pi 扩展模板后离线验证（Bun 运行时，omp / pi 本身不必安装）
bun run tools/omp-ext-check.ts
bun run tools/pi-ext-check.ts
# pi 模板再在本机装的真 pi 运行时里跑一遍（SDK + faux 假模型，不联网不花 token）
node tools/pi-ext-realrun.mjs

# 重做 README 顶部宣传片（Linux + Xvfb + lavapipe 实拍真实应用 → 逐帧合成，产物写到 docs/promo/；
# 依赖与参数见 tools/promo/README.md）
tools/promo/build.sh
```

- **便携 ConPTY**（Windows）：`main()` 在建 gpui 平台层之前调 `mt_pty::conpty::initialize_default()`，从 exe 同目录的 `portable-conpty\` 预载 Windows Terminal 1.24 的 conpty.dll，PTY 宿主于是是 `portable-conpty\x64\OpenConsole.exe` 而不是系统 `conhost.exe`。dev 实例的这个目录由上面的 `stage-sidecars.mjs` 就位到 `target/debug/`；没就位就回落系统 ConPTY——看日志里的 `[conpty-bootstrap] backend=portable|system` 一行。⚠️ 该脚本对 sidecar 与便携 ConPTY 不认 `CARGO_TARGET_DIR`：给构建另设了 target 目录时，要把 `target/debug/portable-conpty/`（连同三个 sidecar）复制到那个目录的 `debug/` 下，否则 dev 实例跑的是系统 conhost（脚本顺带编的 `mt-syntax-pack` 动态库走根 workspace 的 `cargo build`，认 `CARGO_TARGET_DIR`，直接落在该目录的 `debug/` 下）。`MT_DISABLE_PORTABLE_CONPTY=1` 跳过预载，用于开 / 关对比与排障
- 工具链由根目录 `rust-toolchain.toml` 钉死（channel + clippy/rustfmt），CI 按同一文件安装，本地 clippy 与 CI 同版本。新机器 / 新 checkout 先跑一次无参 `rustup toolchain install`（新版 rustup 已弃用「缺工具链时自动安装」）；升级工具链只改该文件的 `channel` 一处。
- ⚠️ **禁跑 `cargo fmt`**：本仓 HEAD 非 rustfmt-clean，全仓 fmt 会重排几十个文件淹没 diff。
- ⚠️ GPUI dev 实例运行中时，`cargo build -p mt-app` / `cargo run -p mt-app` 会卡在「无法替换 target/debug/mini-term.exe」——先关实例，或给这次构建另设 `CARGO_TARGET_DIR`。`cargo test`（含 `-p mt-app` 与 `--workspace`）不受影响：mt-app 没有集成测试，只编单测 harness（`target/debug/deps/mini_term-<hash>.exe`），不产出也不替换 `mini-term.exe`。
- **远端 Git helper 的 dev 就位**：dev / 源码构建没有扩展清单，不提供下载。要在 dev 实例里用托管 Git，按 `docs/features.zh-CN.md`「远端 Git helper：源码构建」把 helper 就位到 `target/debug/remote-git-helpers/<平台>/mt-git-helper`（exe 旁旧布局，与 exe 同信任等级）或设 `MT_REMOTE_GIT_HELPERS_DIR`；要测下载 / 离线导入链路，用 `scripts/pack-extensions.mjs` 打一份产物，`MT_EXTENSIONS_MANIFEST=<extensions.json>` 指给 dev 实例、设置页把下载源指向本地 HTTP 服务。同样不认 `CARGO_TARGET_DIR`（见上一条）
- ⚠️ **别给 mt-app 加 `tests/` 目录**：包里只要有集成测试，`cargo test` 就会先把该包的 bin 编出来（给测试提供 `CARGO_BIN_EXE_*`），上面那条文件锁就又回来了。不需要 GPUI 的端到端测试放进对应的下层 crate（PTY → VT → grid 冒烟在 `crates/mt-terminal/tests/terminal_smoke.rs`）。

## 架构说明

### 工作区布局

| 目录 | 说明 |
|------|------|
| `crates/` | 主工作区（根 Cargo.toml，members = crates/*） |
| `sidecars/` | sidecar 二进制独立工作区（miniterm-hook / mt-ssh-mcp / mt-ssh-cli）。版本号自成语义——daemon 换代靠它判断，**不跟随主程序发版**，故不并入根 workspace。dev 下产物由 `scripts/stage-sidecars.mjs` 就位到主程序 exe 同目录（没有扩展清单时 exe 旁旧布局可信）；发布包里只有 `miniterm-hook` 随包，`mt-ssh-cli` / `mt-ssh-mcp` 是按需扩展 `ssh-tools`（见「按需扩展」） |
| `relay-server/` | 移动端中转服务（axum），其 `protocol` crate 被 `mt-relay` 跨工作区 path 依赖 |
| `mobile/` | 移动端 PWA（React + TS + Vite），产物由中转托管 |

### crates/ 各 crate 职责

| crate | 职责 |
|-------|------|
| `mt-app` | GPUI 应用壳：Workspace 组件树、AppStore 全局状态、各面板/弹窗/托盘/标题栏。组件树图见 `main.rs` 模块注释 |
| `mt-app-core` | `mt-app` 下沉的非 GPUI 应用域逻辑与服务层：SplitNode 布局树与序列化、任务标记、项目树与 SSH 分组纯逻辑、会话分支森林、PTY 槽位、config/layout 后台单写者、SSH 活动抄送协议、文件树/Git 监听与 diff 算法；终端内置 SSH 的纯逻辑：输出闸（`output_gate`）、一条 SSH 链路的连接中状态机（`shell_link`：提示队列 / 行编辑 / 预输入 / Ctrl+C 取消）、终端内行编辑器（`line_input`）、右键接管的路由表 / 准入 / 等首屏（`ssh_takeover`）。不依赖 gpui / mt-ui / mt-remote（`unsafe_code = "forbid"`），自动归入 Windows 快速单测集 |
| `mt-ui` | GPUI 渲染层：终端 view/element、主题桥。不含业务逻辑。**不依赖 mt-config / mt-i18n / mt-project**（前两者改得勤，一改就连带重编这两万多行；后者带 git2）：终端查找条文案由宿主注入（`TerminalSearchBar::new` 的 labels 参数），技术栈徽标按落盘字符串查表（`TechIcon::new(kind.as_str())`）。内嵌 Nerd Font 图标字体（`assets/fonts/`，`terminal/symbols.rs`）：Windows 上 gpui 的回退表只认系统已装字体，内嵌字体进不去，所以是渲染层逐格把主字体缺的私用区图标换成该字体的 run，不是往 `font_fallbacks` 里加名字 |
| `mt-terminal` | VT 状态机 + grid 模型（alacritty_terminal 封装）。不依赖 gpui |
| `mt-pty` | PTY 生命周期（spawn/read/write/resize/kill）+ 便携 ConPTY 预载（`conpty.rs`，从 exe 旁 `portable-conpty/` LoadLibrary 预载；mt-app `main()` 启动时调一次，必须早于任何 spawn） |
| `mt-ai` | AI 感知：hook server（权威）、hook 注册（`hook_registry.rs`）、输入检测降级（`detect.rs`）、状态判定（`monitor.rs`/`perception.rs`）、会话记录读取（`sessions.rs`）、SSH 工具 skill 按项目启停（`ssh_registry.rs`）。两个 registry 共动 `~/.claude/settings.json`，读改写统一走 `claude_settings.rs`（原子写 + 进程内串行） |
| `mt-project` | 文件树、目录监听、搜索、Git（git2，vendored-openssl 必须保留）、外部编辑器、WSL 发行版枚举、技术栈探测与 `ProjectKind` 枚举（`project_kind`；枚举与 mt-ui 的徽标形状表同由 `crates/mt-ui/tools/gen_tech_icons.mjs` 生成，禁止手改） |
| `mt-git-helper` | 无 GPUI 的远端 Git helper：经 SSH stdio 处理版本化 Git 协议，复用 `mt-project::git` 核心，在服务器执行仓库读写操作；不提供监听端口或操作系统沙箱 |
| `mt-config` | 配置持久化(`config.db`,rusqlite)。主题包文件层再导出自 `mt-theme-packs`(`mt_config::ThemePacks` 原路径不变,目录口径 `themes_dir` 留在这里)。不依赖 gpui。`config.json` 已退化成给 sidecar 读的 SSH 投影(见下节);界面布局另见 `mt-layout` |
| `mt-extensions` | 按需扩展的本机侧（见下文「按需扩展」）：清单 `extensions.json` 解析（schema 1）、扩展根目录、`locate`（exe 旁旧布局〔仅无有效清单时〕→ helper 开发根目录 → 已装扩展）、校验安装（zip 与逐文件 sha256 + 字节数、内容寻址目录原子落位、unix 0755）、删除、启动 GC、离线包导入。**不联网**（下载那一跳在 mt-app 的 `extensions.rs`，口径同 mt-usage 的 `models_dev`），不依赖 gpui / mt-config，依赖表只许 serde/serde_json/sha2/zip/dirs（`tests/layers.test.cjs` 守着）。进程级视图 `mt_extensions::global()` 由 mt-app（扩展下载与 `syntax_pack`）、mt-remote（helper 部署）与 mt-ai（`ssh_registry` 找 mt-ssh-cli）共用 |
| `mt-syntax-pack-abi` | 语法高亮扩展包与宿主共用的零依赖 `#![no_std]` C ABI 与 16 语言白名单（`mt_syntax_pack_entry` / `PackTable` / `LanguageEntry` / `flags::ZED_REWRITE`）；`tests/layers.test.cjs` 守门零依赖 |
| `mt-syntax-pack` | 语法高亮扩展包（`cdylib` + 测试用 `rlib`）：把 16 种大体积 tree-sitter 语法（约 31.5 MB DLL / 2.86 MB zip）拆出主程序，经 `mt_syntax_pack_entry` 导出静态表给宿主加载；`mt-app` 只许在 `[dev-dependencies]` 链它做单测，生产依赖（normal / build）由 `tests/layers.test.cjs` 禁止 |
| `mt-theme-packs` | 外置主题包的文件层：`themes/` 目录的列举 / 导入(zip + manifest sha256 校验) / 删除 / 资源读取。自 mt-config 拆出，好让 mt-ui 不经 mt-config 连带依赖 rusqlite；依赖表只许 anyhow/serde/serde_json/sha2/zip |
| `mt-layout` | 界面布局持久化(`layout.db`,rusqlite):三栏比例 / 每项目分屏树 / 窗口几何。分屏树整棵存 JSON 不拆关系表,理由见模块注释 |
| `mt-i18n` | 双语文案层。**字典源头是 `locales/*.ts`**（TS 对象字面量，随 Tauri 版下线迁入），`src/dict.rs` 由 `tools/gen_from_ts.mjs` 生成——**禁止手改 dict.rs**，改文案改 locales 后重跑生成器，`tests/consistency.rs` 的对账常量随之更新 |
| `mt-relay` | 移动端中转桌面侧：出站 WSS 长连、配对、项目快照/增量、对话镜像（`mirror.rs`）、移动端指令写穿 |
| `mt-ssh` | 共享 SSH 通信层（russh 持久会话池 + SFTP 原语），主程序与 sidecar 共用；建连只看连接配置本身（`target.rs`）。⚠️ `~/.ssh` 里**只碰 `known_hosts`**：不读 `~/.ssh/config`（不认别名 / ProxyJump）、不试默认私钥 `~/.ssh/id_*`（用户明确要求，曾加过又撤回，别加回来）。`auth.rs` 依次试连接私钥 / ssh-agent / 密码、`known_hosts.rs` 认通配与 `@revoked`、`transport.rs` 建连整段限时且超时掐断传输；密码信封在 `pool::authenticate` 解开。终端里的交互式 shell 在 `shell.rs`（`open_shell` → `ShellSession`）：每个终端独占一条连接、开 pty + `shell` / `exec`，首见主机的 yes/no/指纹、密码、keyboard-interactive、私钥口令经 `ShellPrompt` 通道交给宿主在终端里问（`interact.rs` / `key_unlock.rs`，等用户的时间不计入建连超时）；建连 + 认证与会话池共用 `connect.rs`，池 / 「测试连接」/ sidecar 不带提示器，行为不变 |
| `mt-remote` | SSH 远程项目的服务层：SFTP 文件树 / 读写 / 上传下载 / 删除、远程 AI 会话扫描、远程 pane 启动预检。managed Git 负责平台匹配的 helper 部署（本机 helper 经 `mt_extensions::locate` 找；找不到是类型化的 `REMOTE_GIT_HELPER_MISSING`，在部署 deadline 之外调 mt-app 注入的 `set_helper_provisioner` 下载后自动重试一次，`missing_helper()` 解出缺的平台）、项目级授权校验（默认拒绝，绑定 SSH 端点与远端根目录，变化即重新授权）、托管 SSH Git 会话与身份失效；写结果未知时阻止后续写入，支持原通道 barrier/proof 核对及通道丢失后的只读快照，两条路径均须用户明确确认且重核快照有效性后恢复，不自动重放写操作，不将丢失原通道后的确认视为旧进程已退出。自持小 tokio 运行时，保留同步阻塞核心入口；托管 Git 的阻塞调用经异步 `run_git` 入口交给自有执行器，调用方只 await。终端内置 SSH 的宿主层在 `terminal_shell.rs`：`open_terminal_shell` 当场把 `mt_ssh::open_shell` spawn 到共享运行时（返回的 `OpeningShell` drop 即中止建连），提示通道包成不含 tokio 类型的 `TerminalPrompts` / `PromptReply`（mt-app 不直接依赖 tokio / mt-ssh），远程 pane 与右键接管的 shell 请求（`remote_shell_request` / `login_shell_request`，TERM=xterm-256color，env 只带 `COLORTERM` 与本机有的 `LANG`）；系统 OpenSSH 回退仍是 `prepare_remote_launch`。不依赖 gpui |
| `mt-secret` | SSH 密码封存：AES-256-GCM 信封 + `credential.key` 主密钥（Windows DPAPI / Unix 0600）。在 mt-core 之上，经 mt-ssh 进入 sidecar，依赖表只许 mt-core/ring/base64/serde/serde_json/zeroize（Windows 另加 DPAPI 用的 windows-sys） |
| `mt-usage` | 用量统计：会话轮次解析 / SQLite 账本 / 聚合 / 计价；models.dev 价格表的归一 / 24h 磁盘缓存 / 降级链路（`models_dev.rs`，拉网那一跳由 mt-app 的 `pricing.rs` 注入） |
| `mt-core` | 叶子共享库（WSL UNC 解析 / SSH 提示扫描 / 原子写等）。⚠️ 依赖方向铁律：只依赖 serde/serde_json/dirs，绝不反向依赖上层 crate——它同时被三个 sidecar 与 mt-ssh 链接 |

### PTY 数据流（进程内，无 IPC）

reader 线程读 PTY 字节直接喂 `mt-terminal` 的 VT 状态机，UI 按帧取 grid 渲染。
原 Tauri 版的 16ms 批缓冲 / 有界 channel / 4MB-1MB 双水位背压 / 30s 超时兜底整套
是为 WebView IPC 边界造的，已随架构作废；孤儿 PTY 回收同理（单进程无失引用链路）。

Windows 本地 pane 多一层输入协议：ConPTY 启动时发 `CSI ? 9001h` 请求
win32-input-mode，`mt-terminal` 跟踪该模式（`TerminalEmulator::win32_input_mode`）。
两类按键由 `mt-ui::terminal::input::win32_input_bytes` 编码成
`CSI Vk;Sc;Uc;Kd;Cs;Rc _`，其余按键仍走 legacy 字节：① 传统编码表达不了的
`Shift+Enter` / `Ctrl+J`，其中 `Ctrl+J` 的 `Uc=10`（Node raw / `_getwch` 依赖非零
Uc，OMP 需上游放宽解码）；② conhost 会挂起的传统前缀。⚠️ conhost 收到过一条记录后，
就不再把写入末尾的不完整序列当按键冲掉：裸 `ESC` 与 `ESC` + `[ O DEL` 挂到下一键，
`ESC` + `] P X ^ _` 吞掉之后所有按键直到下一条 Esc 记录，所以 `Esc`、`Alt+Backspace`、
`Alt+]` 等按传统字节查表换成等价记录（`held_prefix_record`，名单是把 `ESC` + 每个
ASCII 字节喂给便携与系统 conhost 扫出来的）。前台程序对 Shift+Enter / Ctrl+J
的解释不一：`Shift+Enter` 在 Codex（crossterm 直读记录）与 OMP（自己解码 WSI）里
是换行，在 Claude Code / pi 里被 conhost 翻成 `\r`，是提交。`mt-ai` 的输入旁路
据此判定（`tracker.rs::resolve_wsi_record`）：不在 AI 会话时两条都算回车；
AI 会话里 `Ctrl+J` 算换行，`Shift+Enter` 只有 codex / omp 算换行；Esc 记录换算成
裸 Esc，清行、摘回合闩、打断都与传统 Esc 一致。非 Windows 不跟踪；
`MT_DISABLE_WIN32_INPUT_MODE=1` 可关闭。

### 终端里的 SSH（内置 russh，系统 OpenSSH 回退）

连接的「终端使用系统 OpenSSH 客户端」（`SshConnection::use_system_ssh`）没勾时，终端里的 SSH 全走
mt-ssh 的 `ShellSession`，远端字节由 mt-ssh 自己的输出线程直接喂 emulator，不经 ssh.exe、不经 ConPTY；
勾了就原样走旧实现（远程 pane 起 ssh.exe、右键连接敲 `ssh …\r` + `SshAutofill` + `quick_ssh`），一字不变。
pane 的后端分两层（`mt-app::pane` 模块注释「后端」）：

- **基底**：本地 PTY（含 ssh.exe 回退的远程 pane），或内置 SSH 的远程项目 pane（`exec` 远端命令
  `cd '<path>' 2>/dev/null; exec $SHELL -l`，与回退那条同一句）；
- **接管层**：右键「SSH 连接」= 在基底上叠一条内置 SSH 会话。基底**不杀**、不给它发任何字节，它的输出
  关闸丢弃（不进 emulator / AI 感知 / git 观察，`mt-app-core::output_gate`）；先关闸再
  `TerminalEmulator::begin_takeover` 拍本地屏幕快照，会话收场后 `end_takeover` 写回、远端内容留在回看里，
  再开闸。新开的终端要等本地 PTY 回填、首屏提示符画完（光标停在字后面、静默 300ms，最多 5s）才接。
  写入 / 终端应答 / resize / 基底退出按接管阶段路由，路由表在 `mt-app-core::ssh_takeover`；
  接管中右键菜单多一项「断开 SSH」，已在接管中的终端再点连接只提示、不嵌套。

每条内置 SSH 链路一条前台任务，**严格按序**回主线程：建连提示 → 建连结果 → 远端退出；远端输出在
链路闸后暂扣，主线程画完提示、处理完建连结果才放行（提示一定先于 motd 上屏）。首见主机 / 密码 / OTP /
私钥口令在终端里读一行（`mt-app-core::line_input`）：这期间按键**只**喂行编辑器，不进 AI 输入旁路、
不发 `UserInput`、不进移动端；没在读行时 Ctrl+C 取消建连，其余按键进预输入（读行提示一出现即清空，
shell 就绪时才过旁路冲刷）。⚠️ `begin_takeover` 会关掉 win32-input-mode（键盘编码回到传统字节，
行编辑器认的就是这个），`end_takeover` 按快照还原；内置 SSH 的远程 pane 没有 ConPTY，本来就不开
（远端程序自己发 `CSI ? 9001h` 时照常跟踪）。失败 / 取消：醒目的一行进终端 —— 远程 pane 落成「退出」
（重连遮罩照常），接管则还原屏幕后再补一条 toast；收场留一行 `Connection to <host> closed.`
（断线是原因）。「SSH 已连接」灯：接管按会话生命周期报，`quick_ssh` 进程树轮询只留给回退那条。

### 持久化布局：数据目录里有什么

`{active_data_dir}`（`%APPDATA%\com.mini-term.app`，dev 实例由 `MT_APP_DATA_DIR` 覆盖）：

| 文件 | 内容 | 谁读写 |
|------|------|--------|
| `config.db` | **配置本体**（项目、SSH 连接、全部设置） | 只有主程序（`mt-config::db`） |
| `config.json` | **给 sidecar 读的 SSH 投影**，派生物 | 主程序写，三个 sidecar 二进制读 |
| `config.json.pre-sqlite` | 存量用户迁移前的完整旧配置存档，除密码与中转密钥字段封存外不删不改 | 只在回退/排查时用 |
| `config.db.bak` | 每次成功加载后留的一代库备份 | 库损坏时自动顶上 |
| `credential.key` | SSH 密码信封的主密钥（Windows 内容经 DPAPI 包裹；Unix 0600） | 主程序生成，sidecar 只读（见下节） |
| `layout.db` | 界面布局（见下节） | 只有主程序（`mt-layout`） |
| `usage.db` | 用量账本（可从 JSONL 再生） | `mt-usage` |
| `hook-server.json` | hook 端口文件 | 主程序写，sidecar 读 |
| `mini-term.log` | 装机版的 stderr/stdout（全部 `eprintln!`、panic 连同 backtrace 与模块基址、启动埋点）。只在进程没有控制台时接管，启动时超 2 MB 轮转成 `.log.1`；`MT_LOG_FILE=1` 可在控制台下强制落文件。backtrace 要解析成函数名 + 行号，把 release 资产里同版本的 `Mini-Term_<版本>_x64-pdb.zip` 解压到安装目录（`mini_term.pdb` 与 exe 同目录） | 主程序（`mt-app::logfile`） |
| `extensions/` | **仅 dev 隔离实例**（设了 `MT_APP_DATA_DIR`）的按需扩展目录。装机版的扩展目录**不在这里**：在 `dirs::data_local_dir()/com.mini-term.app/extensions`（Windows 是 `%LOCALAPPDATA%`，Local 不是 Roaming —— 几十 MB 二进制不该进漫游配置），见下文「按需扩展」 | 主程序（`mt-extensions`） |

⚠️ **config.db 前向兼容**：预览版与正式版会来回装、共用同一个库，所以旧版本**只删自己认识的键**，不认识的 settings 键、项目/连接行里不认识的字段（`extra`）、读不懂的键与行都原样留着（口径见 `mt-config/src/db.rs` 模块注释）。由此两条硬规矩：**删 `AppConfig` 字段时把键名加进 `db.rs` 的 `RETIRED_KEYS`**（否则库里旧值永远留着）；**下线的键名永不复用**。

### config.json 为什么还在（且必须还在）

它不再是配置的家，只剩 `sshConnections` + `projects[]` 的四个 SSH 字段。**那条 sidecar 链路不能动**：

- 三个 sidecar（`miniterm-hook` / `mt-ssh-mcp` / `mt-ssh-cli`）经 `mt_core::config_reader` 自己解析这个文件做 SSH 能力令牌的 fail-closed 鉴权，且**每次请求重读**（主程序里改「关联 SSH」范围要即时生效）
- `mt-core` 的依赖铁律是只依赖 serde/serde_json/dirs——给它加 rusqlite 就是给每次事件都冷启动的 hook 小程序静态塞进一份 SQLite
- `sidecars/src/ssh_service.rs` 那道「拒绝传输 mini-term 自己的 config.json」的安全护栏按的就是这个路径；审计日志与 IPC socket 目录也拿它的所在目录当锚点

⚠️ **改投影形状时必须同步 `mt_core::config_reader::ConfigSshView`**——两边隔着 crate 边界、没有共享类型，只靠字段名对齐。护栏是 `mt-config` 里的 `投影能被_sidecar_的解析器读懂`，它直接调 sidecar 那份解析器。

能不改形状就别改：sidecar 不随主程序发版，装机目录里的旧 sidecar 认不得新字段。「关联 SSH」的**整组关联**（`ProjectConfig::ssh_connection_groups`，组内后续新增的连接也算）就是这么接的——库里存组名，写投影时由 `ProjectConfig::effective_ssh_connection_ids` 展开成连接 id 并进 `sshConnectionIds`，每次保存重写投影，组成员变化即时生效，sidecar 一行没改。

### SSH 密码封存（`mt-secret`）

`SshConnection.password` 在库、投影、`.bak`、`.pre-sqlite` 存档四处**一律是信封串** `enc:v1:<base64(nonce‖密文‖tag)>`（AES-256-GCM），明文只活在「表单 → `AppStore::upsert_ssh_connection`」那一小段与认证那一刻。主密钥 32 字节随机，存 `{active_data_dir}/credential.key`：Windows 内容经 DPAPI（当前用户范围、禁弹窗）包裹，macOS/Linux 靠 0600。

- **封存点唯一**：`AppStore::upsert_ssh_connection`（`mt-app::secrets::stored_password`，密码没改就沿用旧信封——信封每次 nonce 不同，换了会让 `ssh_session_identity_changed` 误判身份变了、白白作废池里的 session）。`ConfigStore::save` 另有兜底封存挡「谁忘了封」
- **解封点三处**：编辑表单回填、终端自动填充（`pane::connect_ssh` / `mt_remote::prepare_remote_launch`）、`mt-ssh::pool::authenticate`——最后一处是主程序与三个 sidecar 共用的，所以 sidecar 不需要任何自己的解封代码
- **迁移**：`ConfigStore::load` 把存量明文一次性封存并回写库，随后 `VACUUM` + `wal_checkpoint(TRUNCATE)`（SQLite 更新一行不会抹掉页内旧 cell 字节，WAL 旧帧里也躺着明文页），再做这一代 `.bak`；`.pre-sqlite` 存档只改密码字段
- **密钥只由主程序生成**（`Vault::open_or_create`），sidecar 走 `mt_secret::global()` 懒加载：在 `mt_core::config_json_path()` 同目录**只读**打开，**刻意不认 `MT_APP_DATA_DIR`**——sidecar 读的投影本来就不认它，密钥跟着走就会拿 dev 实例的钥匙开装机版的信封。主程序在 `ConfigStore::load` 里 `mt_secret::install` 自己那把（先到先得），dev 隔离目录因此各有各的钥匙。常驻的 sidecar daemon 懒加载的钥匙解不开（`Undecryptable`）时会重读一次密钥文件，文件确实换过（主程序重建了 `credential.key`）才换上新钥匙再解一次；`install` 登记的钥匙从不重载
- **降级口径**：`reveal` 对不带 `enc:` 前缀的值原样放行（升级窗口期 sidecar 先读到旧明文投影也能连）；解不开返回 `Undecryptable`，UI 提示「请重新填写密码」，会话池报 `password unavailable`，**绝不把密文当密码送去认证**。凭据库开不起来时加载不失败，密码保持原样并在日志里喊
- **中转桌面密钥同一套**：`mobileRelay.desktopKey` 与 SSH 密码同一把钥匙、同一迁移时机（`ConfigStore::load`）与兜底封存；封存点 `AppStore::set_mobile_relay_endpoint`，解封点 `mobile_relay::install`（建连）与 `mobile_panel::open`（回填），交给 mt-relay 的是明文。空串 = 未填不封存；解不开按未填写处理（中转回「密钥不正确」后停住）并在面板就地标红
- **远程通知凭据同一套**（issue #87）：`remoteNotify` 的 Bark 设备 key / Telegram bot token / Webhook URL 与中转密钥同口径（同一迁移时机与兜底封存，空串不封存）；封存点 `AppStore::set_remote_notify_secret`，解封点 `remote_notify::build_request`（发送那一刻）与设置页回填（`settings::pages_remote_notify`）。发送走 mt-app 的 `remote_notify.rs`（判据复用 `notify::DoneTracker`，`AlertPlan.remote`），reqwest 错误一律 `without_url()`——token 就在 URL 里
- **威胁模型（诚实版）**：防的是配置文件被拷走/同步/被别的账户读到；**不防**同一账户下的本机进程（主程序自己就能无提示解密），与浏览器存密码同一档
- `mt-secret` 的依赖表只许有 mt-core / ring / base64 / serde / serde_json / zeroize，Windows 另加 DPAPI 用的 windows-sys（都是 sidecar 依赖树里已有的），它经 `mt-ssh` 进入三个 sidecar；`mt-core` 的叶子铁律不动，`SshConnection` 序列化形状一字未变

### 按需扩展（`mt-extensions` + `mt-app::extensions`）

主包瘦身：远程 Git helper（阶段 A）、SSH 工具 sidecar `mt-ssh-cli` + `mt-ssh-mcp`（阶段 B，扩展 id `ssh-tools`）、语法高亮扩展包（阶段 C，扩展 id `syntax-pack`）由官方 CI 构建、与主程序**同版本**绑定，按需下载到本机。要点（细节见 `crates/mt-extensions/src/lib.rs`、`crates/mt-app/src/extensions.rs` 与 `crates/mt-app/src/syntax_pack.rs` 的模块注释）：

- **信任模型（定稿，别改）**：扩展清单 `extensions.json` 与 exe 同目录（macOS `Contents/MacOS/`、deb `/usr/lib/mini-term/`），同一安装器写入，信任等级**等同 exe**。任何来源（GitHub / 用户配的镜像 / 离线包）一律先按清单校验 zip 的 sha256 + 字节数，再解包、再逐文件校验 sha256 + 字节数，镜像不可信也无妨。不签名、不在编译期嵌哈希（CI 里桌面与 helper 并行编译，编译期拿不到 helper 哈希）。没有清单（dev / 源码构建）= 不提供下载与离线导入；`MT_EXTENSIONS_MANIFEST=<路径>` 给 dev / 测试指定一份。
- **查找顺序**（`Extensions::locate`）：① exe 同目录旧布局（`remote-git-helpers/<target>/mt-git-helper`、`mt-ssh-cli[.exe]` / `mt-ssh-mcp[.exe]`、语法包动态库 `mt_syntax_pack.dll` / `libmt_syntax_pack.{dylib,so}` 平铺 exe 旁）—— 视同可信、不校验清单，是 dev（stage 脚本）与源码构建的路径，**只在本机没有有效清单时认**（清单缺失 / 过新 / 读不懂）；有效清单在场（正式安装包，或 dev 用 `MT_EXTENSIONS_MANIFEST` 模拟）时一律不认，tar.gz 解压覆盖旧版目录留下的旧 helper / sidecar / 语法包压不住下载的那份 → ②（仅 helper）`MT_REMOTE_GIT_HELPERS_DIR`（显式设的环境变量，照认）→ ③ 本机已装扩展。在场却坏掉的那一层**不会**落到下一层去用别的二进制。NSIS 安装时顺手删掉安装目录里旧版本留下的 `remote-git-helpers/`、`mt-ssh-cli.exe`、`mt-ssh-mcp.exe`（不留过期副本）。
- **存放**：`{root}/<id>/<zip sha256 前 16 位>/<文件>`，`{root}` = `MT_APP_DATA_DIR` 非空时 `{它}/extensions`，否则 `dirs::data_local_dir()/com.mini-term.app/extensions`。下载暂存 `{root}/.downloads/`、解包暂存 `{root}/<id>/.tmp-<随机>/`，全部校验通过后 rename（目录在 = 装好了；解包写出的文件 mtime = 安装时刻，SKILL.md 校正靠它，见下）。内容寻址是为了 Windows 上不覆盖运行中的 exe（mt-ssh-cli 的 daemon 工作目录就是它的版本目录）。启动后台 GC：每个**已知** id 留当前清单那份 + **最近装的一份其它版本**（按版本目录里文件的最新 mtime；当前清单里没这个 id 就只留最近一份），其余版本目录与未知 id 删掉，删不掉（被占用）留到下次；删除一律先 rename 成同级 `.trash-<随机>` 再删（`store.rs::discard`），Windows 上目录里有运行中的 daemon 或已加载的 DLL 时 rename 失败 → 整个原样留着，不会删成只剩半截；**没有清单时 GC 什么都不删**。多留一份是为了预览版 / 正式版共用数据目录来回切换时不必每次重下。NSIS 真卸载删 `%LOCALAPPDATA%\com.mini-term.app\extensions`（删前纳入清场），升级不删、不清场扩展目录。
- **资产**：每个扩展一个 zip，资产名内容寻址 `mt-ext_<id>_<zip sha256 前 12 位>.zip`；每个桌面平台一个离线扩展包 `Mini-Term_<版本>_<平台键>-extensions.zip`（平台键 `windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`）。打包脚本 `scripts/pack-extensions.mjs`（零 npm 依赖、产物逐字节可复现）；它与 mt-extensions 安装代码的口径由 `crates/mt-extensions/tests/fixtures/` 夹具两头对账（`tests/packExtensions.test.cjs` + `crates/mt-extensions/tests/fixtures.rs`）。CI 里 helper 扩展 zip 只在 remote-git-helpers 作业打一次、只由 upload-helper-extensions 作业传一次；`ssh-tools` 与 `syntax-pack` 是桌面平台相关的：build-desktop 把 `mt-syntax-pack` 编到 `staging/desktop/syntax-pack/`（不进安装包与 Unix tar），package-release 每行把本平台的两个 sidecar 与语法包动态库分别打成扩展并上传（内容寻址，四行不撞名），再生成清单与离线包（见 `release.yml` 文件头）。deb 的 Depends 仍按 mt-ssh-cli / mt-ssh-mcp 算（下载后在本机跑）。
- **下载**（`mt-app::extensions`）：只在用户的显式动作里 —— 设置 →「扩展」点下载 / 全部下载，发起托管 Git 部署（缺 helper 时 mt-remote 在部署 deadline 之外调注入的补齐器，下载有 toast 进度，成功后自动重试一次部署；部署触发的失败 10 分钟内不再自动重下），「关联 SSH」保存时本机没有 mt-ssh-cli（`Origin::EnableSshTools`，toast 进度、无冷却；首次启用下载不成整步失败，更新范围下载不成则范围照存、SKILL.md 不动），或在文件预览里打开语法包里的语言时点顶部提示条的「下载」（`Origin::SyntaxBanner`，toast 进度、无冷却，直接 await oneshot）。**唯一的非显式下载**是 SSH 工具的升级自愈（`Origin::SelfHeal`）：启动时有本地项目启用了 SSH 工具而本机没有 mt-ssh-cli → 后台下载，失败只记日志 + 设置页标原因、不推 toast。共享 HTTP 客户端流式写盘，累计超过清单 size 立刻断，连接 15s、60s 无数据判失败，同一扩展并发 `ensure` 去重（结果经 oneshot 交回，GPUI 任务里直接 await）。地址模板是配置键 `extensionsDownloadUrl`（空 = 默认 GitHub release，代码里不内置任何第三方代理）。
- **SSH 工具（`ssh-tools`）**：`mt_ai::ssh_registry` 经 `locate` 找 mt-ssh-cli，找不到就**绝不写** SKILL.md（`enable` 报错、启动校正跳过）。CLI 换版本 = 换内容寻址目录，SKILL.md 里的绝对路径靠启动校正收敛：旧目录被 GC 多留一份时「引用的 CLI 不在」不触发，收敛只靠「本机 CLI 的 mtime 严格更新」—— 解包文件的 mtime 是安装时刻，新装的那份更新，所以会改指；两个实例来回切时只朝较晚装的一方收敛，不来回改写。`ssh-tools` 装好（下载或离线导入，不论谁发起）后 mt-app 再跑一次校正。daemon 换代与路径无关：IPC 端点（Windows 命名管道按用户 SID、Unix socket 在 `$XDG_RUNTIME_DIR` 或 config.json 同目录）不随 exe 位置变，版本握手只换代更旧的 daemon；旧版本 daemon 在旧目录里跑时 GC 删不掉它（留到下次）。旧版 `.mcp.json` 里指向安装目录 `mt-ssh-mcp` 的历史注册不额外处理（启用 / 停用时本来就会摘除本 server 的条目）。
- **语法高亮扩展包（`syntax-pack`）**：主程序保留 26 种高频 / 小体积语法（gpui-component 显式开启的 22 种 feature + 常驻 `json` + `syntax_languages.rs` 的 `xml` / `ini` / `batch`），16 种大体积语法挪进 `mt-syntax-pack`（原 gpui-component 的 `cpp` / `elixir` / `php` / `ruby` / `scala` / `sql` / `zig` 7 种 + 原 `syntax_languages.rs` 的 `csharp` / `swift` / `cmake` / `proto` / `graphql` / `kotlin` / `powershell` / `dart` / `groovy` 9 种，后 9 种带 `flags::ZED_REWRITE` 交宿主重映射 query）。16 个语法 crate 均未定义 `TREE_SITTER_REUSE_ALLOCATOR`，外部扫描器直接走 C 运行时 `malloc`/`free`，tree-sitter 运行时只在宿主保留一份。宿主 `mt-app::syntax_pack` 启动时在后台 `locate`：来自扩展目录的动态库加载前经 `ring` 重算整文件 sha256 + 字节数，`libloading` 加载（Windows `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`，Unix `RTLD_NOW | RTLD_LOCAL`），调 `mt_syntax_pack_entry` 后按 ABI 版本 / 结构体大小 / UTF-8 / `mt_syntax_pack_abi::LANGUAGES` 白名单 / tree-sitter ABI 区间逐项核验，通过后 `mem::forget` 驻留进程（不卸载，避免 tree-sitter 静态指针悬空；Windows 上已加载的旧版 DLL 删不掉会留到下次启动 GC），再在主线程批量注册主名与别名并刷新 `watch_editor` 登记的已开编辑器与窗口。下载或离线导入（或删除把 `Failed` 复位回 `Missing`）后由 `extensions::dispatch` 调 `syntax_pack::on_extensions_changed` 热加载；缺包打开这 16 种语言按纯文本显示，有有效清单时在文件预览顶部出提示条（`render_syntax_pack_banner`），「关闭」只对本次会话生效。

### 布局持久化（`layout.db`，非 `config.json`）

「启动时还原上次退出的样子」这一整块——三栏比例、中栏比例与显隐、右侧抽屉宽度、
每个项目的分屏树（含 pane 的 shell / cwd / AI 会话身份）、窗口大小位置与最大化态
——住在 `{active_data_dir}/layout.db`，由 `mt-layout` 读写。

- **为什么搬出来**：布局是交互频次的数据，config.json 是月级的。此前拖一次分隔条
  要把整份配置 `to_string_pretty` 重写 + 复制一份等大的 `.bak`（实测 64 KB 配置 →
  一次拖拽约 128 KB 落盘）；现在是一行 upsert（实测同数据 layout.db 仅 4 KB）
- **AppConfig 里那五个字段仍在，但 `skip_serializing`**：只读不写，留作一次性迁移
  入口，观察一版后连字段删除。运行期它们是 `AppStore` 手上的**内存缓存**
  （启动时被 layout.db 的值覆盖），各处 getter 照旧读它
- ⚠️ **布局迁移有个顺序陷阱**：配置迁移一完成，`config.json` 就被覆盖成 SSH 投影，
  `savedLayout` 此后只活在内存那一份 `AppConfig` 上。要是布局迁移偏偏在那一次失败，
  重试时 config 已从 `config.db` 读、`savedLayout` 全是 `None`，旧布局就永久没了。
  兜底在 `store.rs::layout_migration_fallback`——回头读 `config.json.pre-sqlite`
- **迁移幂等靠 meta 标记**，不靠「库里有没有数据」：用户把终端关光后重启，库是空的
  但迁移确实做过，按后者判会把旧布局从 config.json 里复活
- **分屏树整棵存 JSON**（`project_layout.layout_json`），与旧 `savedLayout` 逐字段一致，
  `mt-app::persist` 一行没动。不拆关系表的论证见 `mt-layout` 模块注释
- 库损坏时挪成 `layout.db.corrupt` 并重建空库；开不起来则**本次布局只在内存里活着**，
  界面照常用（与配置加载失败时的「只读模式」同一条红线）

### AI 状态判定（idle / ai-idle / ai-working）

hook 上报（`mt-ai::hook_server`）一旦启用即为权威，退出以 SessionEnd 为准；无 hook 时降级为输入检测（`mt-ai::detect` 识别键入的 `claude`/`codex`/`opencode`/`pi`/`grok`/`omp`/`agy` 命令，含 ↑ 历史/Tab 补全的行快照兜底与输出回扫）+ 输出活跃度轮询。非 hook 的例外有三条：

1. **用户打断**：Claude 在 Esc/Ctrl+C 中断时不发任何事件（官方文档明示 `Stop` 不触发），由写入侧识别裸 Esc/Ctrl+C 后调 `note_user_interrupt` 把 hook 状态收敛为 ai-idle，cause=`Interrupt` 不算完成。
2. **停摆兜底**（`stall_settle_target`）：hook 停在 ai-working 且状态与 PTY 输出双双静默 10s 时收敛——此前触发过退出（Ctrl+D/双击 Ctrl+C/`/exit` 且之后无 hook 事件扶正）判为已退出 → `idle`/cause=`StallExit`，否则 → `ai-idle`/cause=`Stall`；正等用户批准的 pane（上次 cause 属 attention 类，如 Codex 的 `PermissionRequest`）豁免，否则黄灯会被抹掉。
3. **退出兜底**（`monitor::exit_settle_due`，只对没有 SessionEnd 的 agent，即 `AgentSpec::session_end_hook=false` 的 agy）：hook 状态停在 ai-idle、输入检测已判退出（agy 是 Ctrl+D 连按两次 / `/exit` / `/quit`）、状态与输出双双静默 10s → 视同 SessionEnd 清掉 hook 状态，回落 idle/cause=`StallExit`。

**铁律**：三条兜底都把结论**落盘**进 hook 状态，触发一次即收敛、不再摆动——无记忆兜底（假完成每 20~50s 重复播报）是踩过的坑，别回去。

降级路径（无 hook，`hook_enabled` 缺省就是关的）的完成播报同一条铁律：用户在 AI 会话里提交一行才给 pane 上**回合闩**（`SessionTracker::take_turn`），这一回合第一个 ai-working→ai-idle 下降沿不带成因（UI 认作完成）并摘闩；没人提交过的下降沿（启动 banner、TUI 空闲期零星重绘）与同一回合之后的下降沿一律带 cause=`Quiet`，不播报。裸 Esc / Ctrl+C / 退出摘闩——被打断的回合不算做完。

**PTY 死了（退出 / 没起来）error 是终态**：store 当场撤出 mt-ai 的 500ms 轮询（`AiBridge::remove_pane`）、清掉它的完成记录，`apply_ai_event` 对 error pane 的迟到事件一律不收；只有重连换一条新 PTY 才离开。

**界面上的灯是五档**（`mt-app::tree::StatusLight`）：`PaneStatus` 四态（后端与移动端协议口径，不加值）再叠 `attention` 位，多出第五档「等你处理」（实心圆 + 叹号，`ui::color_attention`），优先级 error > attention > ai-working > ai-idle > idle。页签、折叠标题条、悬停预览、项目行、切换器、终端列表竖条、边条全局徽标、会话列表都按它画。

### 移动端中转体系（`relay-server/` + `mobile/` + `mt-relay`）

- `relay-server/protocol`：桌面端与中转共享的协议消息 crate（JSON over WebSocket，serde camelCase，版本号握手校验，当前 v2）；PWA 侧 TypeScript 类型在 `mobile/src/protocol.ts` 手写镜像，两侧字段必须同步维护
- `relay-server/server`：axum 中转，只做转发不落盘；桌面端接入需携带 `MT_RELAY_DESKTOP_KEY`（未配置即拒绝一切桌面连接，fail-closed）
- **AI 启动器**：桌面端配置的具名 `{名称, shell?, 命令}`，移动端只按 id 引用、看得到名字，命令文本从不经过移动端或中转（ADR 0002 的边界）
- 部署见 `docs/deploy-relay.zh-CN.md`（英文版 `docs/deploy-relay.md`）

## 注意事项

- Grok 的 hook 接入与另外两家有两处结构性差异，改动前先看 `mt-ai::hook_registry::register_grok_hooks` 的注释：① grok 默认还会扫描 `~/.claude/settings.json` 的 hooks（Claude 兼容层），同一事件会来两趟，sidecar 靠 `GROK_SESSION_ID` + 是否带 argv 丢弃兼容层那趟（只注册了 Claude 的用户必须放行——那是唯一来源，判据落在原生 hook 文件是否在场）；② 注册进 `~/.grok/hooks/` 的命令必须是**不含空格的裸文件名**（hook 二进制随注册复制进该目录），带空格会被 grok 丢给 shell，而 Windows 上具体是 git-bash/pwsh/powershell/cmd 由环境决定、四家引号语义互斥；事件名改由 grok 注入的 `GROK_HOOK_EVENT` 传递
- oh-my-pi（omp）的 hook **不走 sidecar**：它的扩展点是 Bun 进程内加载的 TS 模块，`mt-ai::hook_registry` 把自带的 `crates/mt-ai/assets/miniterm-omp.ts` 整份写进 `~/.omp/agent/extensions/miniterm.ts`，扩展在 omp 进程内 `fetch` 本地 hook 服务器、事件名翻译成与 Claude 同名的 PascalCase。两条硬约束：① **只有 `ctx.mode === "tui"` 的主会话上报**——omp 的子代理是同进程内的独立会话，会重新绑定所有扩展工厂，照常上报会把父会话误报成完成；② 打断后的 `agent_end` 以 `Stop` + `reason: aborted` 上报，hook server 落成 cause=`Interrupt`。模板里 `pi.on(...)` 的事件集与 `OMP_HOOK_EVENTS`、上报的事件名与 `OMP_REPORTED_EVENTS` 都有单测逐条对账，改模板先改常量
- pi（earendil-works/pi，omp 的上游）同样走进程内 TS 扩展（`TsExtension`，写进 `~/.pi/agent/extensions/miniterm.ts`），但模板是另一份 `crates/mt-ai/assets/miniterm-pi.ts`，**两份不能互换**（对照实测：omp 模板装进 pi 会在 API 重试中途误报 StopFailure、漏掉权限对话框）：① 回合结束只认 `agent_settled`（pi ≥ 0.80.4）——`agent_end` 在自动重试 / 压缩重试 / 排队消息时一回合会来多次；② 没有 `session_switch`，换会话与 `/reload` 都是 `session_shutdown`(reason) 拆旧运行时 + 新运行时 `session_start`，只有 `quit` 是退出、`new/resume/fork` 打 `clear` 墓碑、`reload` 不报；③ 没有审批事件，扩展对话框走 `ui_prompt_start/end`（pi ≥ 0.84.4），只在回合进行中映射成 Elicitation 黄灯。对账常量是 `PI_HOOK_EVENTS` / `PI_REPORTED_EVENTS`。omp 也认 `PI_CODING_AGENT_DIR`，设了它两家共用同一个 `miniterm.ts`，归属按模板里的 `const AGENT = "…"` 认（`TsExtension::owns`）——别改掉模板里这一行，否则启动自愈会互相改写对方的注册。验证除了 `tools/pi-ext-check.ts`（伪事件总线），还有 `tools/pi-ext-realrun.mjs`（本机装的 pi 的 SDK + pi-ai 的 `fauxProvider`，`session.bindExtensions({ mode: "tui" })` 在真运行时里离线跑）
- Antigravity CLI（`agy`，Google）走 sidecar 的 shell 命令 hook，写进全局 `~/.gemini/config/hooks.json` 里 mini-term 专属的 `"mini-term"` 组（顶层键是组名，别家的组与键序原样保留；`ANTIGRAVITY_GEMINI_DIR` 实测不改这个位置）。硬约束全来自 agy 1.5.12 实测，改动前先读 `hook_registry` 的 agy 段与 `sessions/agy.rs` 模块注释：① **只注册 `PreInvocation` 与 `Stop`**，**绝不能注册 `PreToolUse`**——agy 要求 hook 往 stdout 输出 JSON，sidecar 对 agy 一律输出 `{}`，而 `PreToolUse` 的 `{}` = 拒绝执行工具；② sidecar 认 agy 要「`ANTIGRAVITY_CONVERSATION_ID` 非空 **且** payload 带 `conversationId`」两个判据（防与 grok 等互相嵌套时继承环境变量误判），`lastUserInput` 正文不转发、只留 `has_user_input` 布尔位；③ 子代理（`invoke_subagent`）也触发全局 hook、带自己的会话 id，`has_user_input=false` 的事件整条丢弃；主会话以 `Stop(fullyIdle=false)` 收尾时（后台子代理还在跑）完成**延后**：主会话再有事件就作废、以它自己的 Stop 为准，否则等该 pane 记下的在跑子代理全部收尾（子代理的 Stop）时补记那次 Stop——实测两种时序都有，只认前一种会让「主会话答完、子代理还在收尾」的回合永远不播报完成；④ Esc 打断、退出、权限框、提问框都**不发 hook**——打断走 `note_user_interrupt`，退出走上面的第三条兜底，Ctrl+D 要连按两次才退（`AgentSpec::double_ctrl_d_exit`）；**权限确认框与提问框改由 OSC 7501（Program Status Protocol）直通补齐**（`mt-terminal::osc7501` 只读流式扫描 + 原样回发 `ESC ] 7501 ; ? ST` 能力探测，`mt-ai::observe_osc_status` 仅接 `app=antigravity/agy`）：`state=blocked:kind=permission` → `PermissionRequest`、`kind=question` → `Elicitation`（同弹窗指纹去重，离开或换内容立刻放行），`working`/`done`/`error`/`idle`/`clear` 共用同一 `HookState` + `StatusEmitter` 去重（退出窗口 3s 内屏蔽 agy 临退时再吐的 `state=idle`；未开 hook / WSL / SSH 远程也一并受益）。便携 ConPTY 与 Unix/WSL/SSH 靠能力探测自动握手，仅 Windows 回落系统 `conhost.exe` 时 ConPTY 吞掉输入应答，由 `mt-app-core::osc7501::agy_env_fallback` 仅在本地非 WSL 且系统 ConPTY 下注入 `AGY_CLI_OSC7501=on` 兜底；`MT_DISABLE_OSC7501=1` 可整体关闭。会话索引是 SQLite（`~/.gemini/antigravity-cli/conversation_summaries.db`，只读打开 WAL 库），正文 `brain/<id>/.system_generated/logs/transcript_full.jsonl` 在上下文压缩时**整份重写**：镜像的续读游标带「偏移前 64 字节锚」识别重写，用量账本对 agy 只增不删（按 `agy:{step_index}@{created_at}` 去重）。agy 会话不支持从面板删除，不出提问卡片；历史列表与正文覆盖本机、WSL（经本地临时快照读 9P 上的 WAL 索引库）与 SSH 远程（经 SFTP 拉取索引快照），用量统计与镜像只覆盖本机来源
- Claude/Codex/pi/Grok/OMP/agy 有移动镜像可解析的本机会话记录（`mt-ai::sessions::agent_has_session_log`）。OMP 18.x 的记录位于 `{omp_agent_dir}/sessions/{编码 cwd}/{timestamp}_{session-id}.jsonl`，镜像按 hook session id 精确定位，解析 `message` 的 user/assistant/toolResult 与 assistant `toolCall(name=ask)`；pi 镜像不出提问卡片。pi 与 OMP 的本地定位与行解析共用 `sessions/omp.rs`（`PiFlavor`）；pi 只有 `--{绝对路径}--` 编码，OMP 另有 temp/home 相对桶。两者的桌面历史共用 `sessions/pi_history.rs`，与 hook 无关：OMP 读物理首行的 `type=title` 槽（旧文件读 header 标题），pi 读 `session_info`；正文沿 `parentId` 展示当前对话分支，标题槽不是树条目。OMP 不支持从面板删除记录；两者均未接用量统计与会话谱系。opencode 这类**只靠输入检测识别**的 agent 拿得到状态徽章与移动端指令，但没有对话镜像——镜像必须跳过启发式绑定，否则会把同项目其它 agent 的最新会话贴到该 pane 上
- pi 系会话的目录发现和头部 `cwd` 校验共用物理项目身份；OMP 新旧桶的同 ID 副本按修改时间、桶优先级、路径固定排序，列表与正文按同一顺序选择。两家共享 `sessions` 根时用标题槽 / 模型记录等格式证据分辨归属，无法识别的绝对路径桶旧记录不进入列表、镜像或 pi 删除目标。OMP 正文按 `activeLeafId` 重放持久化选枝，`null` 表示无活动叶子，不把后台用量追加误作活动分支。
- Grok 的会话记录形态与另外两家不同：一个会话是**一整个目录**（`{grok_home}/sessions/{URL 编码的 cwd}/{session-id}/`，正文 `updates.jsonl` 是 ACP 更新流，一条消息拆成多个 chunk 行、攒到边界才成一条；元信息在 `summary.json`）。定位项目走**解码目录名**而非编码项目路径，详见 `mt-ai::sessions` 的 Grok 段注释
- GPUI 迁移期的逐批决策与「记档不修」清单在 `docs/gpui-migration-progress.md`——改到相关模块（拖拽/托盘/标题栏/关窗/toast 等）前先查该文档对应批次的记档，很多「看起来是 bug」的行为是评审定稿的取舍
- 领域术语表在 `CONTEXT.md`（会话/会话来源/项目等 ubiquitous language）
