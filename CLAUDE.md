# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**mini-term** — GPUI 原生桌面终端管理器，支持多项目、多标签、分屏布局，并能感知 AI 进程（Claude/Codex/Grok 等）状态；带移动端中转镜像与 SSH 远程项目能力。

- **UI/渲染**: [gpui](https://crates.io/crates/gpui) 0.2.x（Zed 官方，crates.io 版）+ gpui-component（Resizable/Modal/Input/Tree 等）
- **终端**: alacritty_terminal（VT 状态机，进程内直喂，无 IPC）+ portable-pty
- **发布形态**: Windows x64 NSIS 安装包（`scripts/windows-installer.nsi`，包内平铺 exe + 三个 sidecar + portable-conpty，全部「与 exe 同目录」）+ macOS dmg + Linux deb/tar.gz
- **历史**: 项目最初是 Tauri v2 + React 实现，v1.0.0-beta 后整体删除切换到 GPUI 原生版；找旧实现看 git 历史（合并点 `236d5c1`）

## 开发命令

```bash
# 首次/换版本后：构建三个 sidecar 并连同便携 ConPTY 就位到 target/debug/
node scripts/stage-sidecars.mjs

# 启动开发实例（⚠️ 与装机版并跑时必须隔离数据目录）
MT_APP_DATA_DIR="$LOCALAPPDATA/mini-term-gpui-dev" cargo run -p mt-app

# 全工作区测试（27 个目标 1500+ 例）
cargo test --workspace

# 中转服务端协议边界测试 / sidecar 工作区
cd relay-server && cargo test
cargo build --manifest-path sidecars/Cargo.toml

# 移动端 PWA
cd mobile && npm run build

# 改文案后重新生成 i18n 字典
node crates/mt-i18n/tools/gen_from_ts.mjs

# 改 oh-my-pi 扩展模板后离线验证（Bun 运行时，omp 本身不必安装）
bun run tools/omp-ext-check.ts
```

- ⚠️ **禁跑 `cargo fmt`**：本仓 HEAD 非 rustfmt-clean，全仓 fmt 会重排几十个文件淹没 diff。
- ⚠️ GPUI dev 实例运行中时 `cargo test -p mt-app` 会卡在「无法替换 target/debug/mini-term.exe」——先关实例，或 `cargo test --no-run --message-format=json` 取出测试二进制直接执行。

## 架构说明

### 工作区布局

| 目录 | 说明 |
|------|------|
| `crates/` | 主工作区（根 Cargo.toml，members = crates/*） |
| `sidecars/` | sidecar 二进制独立工作区（miniterm-hook / mt-ssh-mcp / mt-ssh-cli）。版本号自成语义——daemon 换代靠它判断，**不跟随主程序发版**，故不并入根 workspace。产物由 `scripts/stage-sidecars.mjs` 就位到主程序 exe 同目录 |
| `relay-server/` | 移动端中转服务（axum），其 `protocol` crate 被 `mt-relay` 跨工作区 path 依赖 |
| `mobile/` | 移动端 PWA（React + TS + Vite），产物由中转托管 |

### crates/ 各 crate 职责

| crate | 职责 |
|-------|------|
| `mt-app` | GPUI 应用壳：Workspace 组件树、AppStore 全局状态、SplitNode 布局树、各面板/弹窗/托盘/标题栏。组件树图见 `main.rs` 模块注释 |
| `mt-ui` | GPUI 渲染层：终端 view/element、主题桥。不含业务逻辑 |
| `mt-terminal` | VT 状态机 + grid 模型（alacritty_terminal 封装）。不依赖 gpui |
| `mt-pty` | PTY 生命周期（spawn/read/write/resize/kill）+ 便携 ConPTY 预载（`conpty.rs`，从 exe 旁 `portable-conpty/` LoadLibrary 预载） |
| `mt-ai` | AI 感知：hook server（权威）、hook 注册（`hook_registry.rs`）、输入检测降级（`detect.rs`）、状态判定（`monitor.rs`/`perception.rs`）、会话记录读取（`sessions.rs`） |
| `mt-project` | 文件树、目录监听、搜索、Git（git2，vendored-openssl 必须保留）、外部编辑器、WSL 发行版枚举 |
| `mt-config` | 配置持久化(`config.db`,rusqlite)与主题包。不依赖 gpui。`config.json` 已退化成给 sidecar 读的 SSH 投影(见下节);界面布局另见 `mt-layout` |
| `mt-layout` | 界面布局持久化(`layout.db`,rusqlite):三栏比例 / 每项目分屏树 / 窗口几何。分屏树整棵存 JSON 不拆关系表,理由见模块注释 |
| `mt-i18n` | 双语文案层。**字典源头是 `locales/*.ts`**（TS 对象字面量，随 Tauri 版下线迁入），`src/dict.rs` 由 `tools/gen_from_ts.mjs` 生成——**禁止手改 dict.rs**，改文案改 locales 后重跑生成器，`tests/consistency.rs` 的对账常量随之更新 |
| `mt-relay` | 移动端中转桌面侧：出站 WSS 长连、配对、项目快照/增量、对话镜像（`mirror.rs`）、移动端指令写穿 |
| `mt-ssh` | 共享 SSH 通信层（russh 持久会话池 + SFTP 原语），主程序与 sidecar 共用 |
| `mt-usage` | 用量统计：会话轮次解析 / SQLite 账本 / 聚合 / 计价 |
| `mt-core` | 叶子共享库（WSL UNC 解析 / SSH 提示扫描 / 原子写等）。⚠️ 依赖方向铁律：只依赖 serde/serde_json/dirs，绝不反向依赖上层 crate——它同时被三个 sidecar 与 mt-ssh 链接 |

### PTY 数据流（进程内，无 IPC）

reader 线程读 PTY 字节直接喂 `mt-terminal` 的 VT 状态机，UI 按帧取 grid 渲染。
原 Tauri 版的 16ms 批缓冲 / 有界 channel / 4MB-1MB 双水位背压 / 30s 超时兜底整套
是为 WebView IPC 边界造的，已随架构作废；孤儿 PTY 回收同理（单进程无失引用链路）。

### 持久化布局：数据目录里有什么

`{active_data_dir}`（`%APPDATA%\com.mini-term.app`，dev 实例由 `MT_APP_DATA_DIR` 覆盖）：

| 文件 | 内容 | 谁读写 |
|------|------|--------|
| `config.db` | **配置本体**（项目、SSH 连接、全部设置） | 只有主程序（`mt-config::db`） |
| `config.json` | **给 sidecar 读的 SSH 投影**，派生物 | 主程序写，三个 sidecar 二进制读 |
| `config.json.pre-sqlite` | 存量用户迁移前的完整旧配置存档，不删不改 | 只在回退/排查时用 |
| `config.db.bak` | 每次成功加载后留的一代库备份 | 库损坏时自动顶上 |
| `layout.db` | 界面布局（见下节） | 只有主程序（`mt-layout`） |
| `usage.db` | 用量账本（可从 JSONL 再生） | `mt-usage` |
| `hook-server.json` | hook 端口文件 | 主程序写，sidecar 读 |

### config.json 为什么还在（且必须还在）

它不再是配置的家，只剩 `sshConnections` + `projects[]` 的四个 SSH 字段。**那条 sidecar 链路不能动**：

- 三个 sidecar（`miniterm-hook` / `mt-ssh-mcp` / `mt-ssh-cli`）经 `mt_core::config_reader` 自己解析这个文件做 SSH 能力令牌的 fail-closed 鉴权，且**每次请求重读**（主程序里改「关联 SSH」范围要即时生效）
- `mt-core` 的依赖铁律是只依赖 serde/serde_json/dirs——给它加 rusqlite 就是给每次事件都冷启动的 hook 小程序静态塞进一份 SQLite
- `sidecars/src/ssh_service.rs` 那道「拒绝传输 mini-term 自己的 config.json」的安全护栏按的就是这个路径；审计日志与 IPC socket 目录也拿它的所在目录当锚点

⚠️ **改投影形状时必须同步 `mt_core::config_reader::ConfigSshView`**——两边隔着 crate 边界、没有共享类型，只靠字段名对齐。护栏是 `mt-config` 里的 `投影能被_sidecar_的解析器读懂`，它直接调 sidecar 那份解析器。

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

hook 上报（`mt-ai::hook_server`）一旦启用即为权威，退出以 SessionEnd 为准；无 hook 时降级为输入检测（`mt-ai::detect` 识别键入的 `claude`/`codex`/`opencode`/`pi`/`grok`/`omp` 命令，含 ↑ 历史/Tab 补全的行快照兜底与输出回扫）+ 输出活跃度轮询。非 hook 的例外有两条：

1. **用户打断**：Claude 在 Esc/Ctrl+C 中断时不发任何事件（官方文档明示 `Stop` 不触发），由写入侧识别裸 Esc/Ctrl+C 后调 `note_user_interrupt` 把 hook 状态收敛为 ai-idle，cause=`Interrupt` 不算完成。
2. **停摆兜底**（`stall_settle_target`）：hook 停在 ai-working 且状态与 PTY 输出双双静默 10s 时收敛——此前触发过退出（Ctrl+D/双击 Ctrl+C/`/exit` 且之后无 hook 事件扶正）判为已退出 → `idle`/cause=`StallExit`，否则 → `ai-idle`/cause=`Stall`；正等用户批准的 pane（上次 cause 属 attention 类，如 Codex 的 `PermissionRequest`）豁免，否则黄灯会被抹掉。

**铁律**：两条兜底都把结论**落盘**进 hook 状态，触发一次即收敛、不再摆动——无记忆兜底（假完成每 20~50s 重复播报）是踩过的坑，别回去。

### 移动端中转体系（`relay-server/` + `mobile/` + `mt-relay`）

- `relay-server/protocol`：桌面端与中转共享的协议消息 crate（JSON over WebSocket，serde camelCase，版本号握手校验，当前 v2）；PWA 侧 TypeScript 类型在 `mobile/src/protocol.ts` 手写镜像，两侧字段必须同步维护
- `relay-server/server`：axum 中转，只做转发不落盘；桌面端接入需携带 `MT_RELAY_DESKTOP_KEY`（未配置即拒绝一切桌面连接，fail-closed）
- **AI 启动器**：桌面端配置的具名 `{名称, shell?, 命令}`，移动端只按 id 引用、看得到名字，命令文本从不经过移动端或中转（ADR 0002 的边界）
- 部署见 `docs/deploy-relay.zh-CN.md`（英文版 `docs/deploy-relay.md`）

## 注意事项

- Grok 的 hook 接入与另外两家有两处结构性差异，改动前先看 `mt-ai::hook_registry::register_grok_hooks` 的注释：① grok 默认还会扫描 `~/.claude/settings.json` 的 hooks（Claude 兼容层），同一事件会来两趟，sidecar 靠 `GROK_SESSION_ID` + 是否带 argv 丢弃兼容层那趟（只注册了 Claude 的用户必须放行——那是唯一来源，判据落在原生 hook 文件是否在场）；② 注册进 `~/.grok/hooks/` 的命令必须是**不含空格的裸文件名**（hook 二进制随注册复制进该目录），带空格会被 grok 丢给 shell，而 Windows 上具体是 git-bash/pwsh/powershell/cmd 由环境决定、四家引号语义互斥；事件名改由 grok 注入的 `GROK_HOOK_EVENT` 传递
- oh-my-pi（omp）的 hook **不走 sidecar**：它的扩展点是 Bun 进程内加载的 TS 模块，`mt-ai::hook_registry` 把自带的 `crates/mt-ai/assets/miniterm-omp.ts` 整份写进 `~/.omp/agent/extensions/miniterm.ts`，扩展在 omp 进程内 `fetch` 本地 hook 服务器、事件名翻译成与 Claude 同名的 PascalCase。两条硬约束：① **只有 `ctx.mode === "tui"` 的主会话上报**——omp 的子代理是同进程内的独立会话，会重新绑定所有扩展工厂，照常上报会把父会话误报成完成；② 打断后的 `agent_end` 以 `Stop` + `reason: aborted` 上报，hook server 落成 cause=`Interrupt`。模板里 `pi.on(...)` 的事件集与 `OMP_HOOK_EVENTS`、上报的事件名与 `OMP_REPORTED_EVENTS` 都有单测逐条对账，改模板先改常量
- 只有 Claude/Codex/Grok 有可解析的会话记录（`mt-relay::mirror` 的 `agent_has_session_log`）。opencode/pi 这类**只靠输入检测识别**的 agent（以及有 hook 但记录格式未接入的 omp）拿得到状态徽章与移动端指令，但没有对话镜像、AI 历史面板与用量统计——镜像必须据此跳过启发式绑定，否则会绑到同项目其它 agent 的最新会话文件，把别人的对话贴到该 pane 上
- Grok 的会话记录形态与另外两家不同：一个会话是**一整个目录**（`{grok_home}/sessions/{URL 编码的 cwd}/{session-id}/`，正文 `updates.jsonl` 是 ACP 更新流，一条消息拆成多个 chunk 行、攒到边界才成一条；元信息在 `summary.json`）。定位项目走**解码目录名**而非编码项目路径，详见 `mt-ai::sessions` 的 Grok 段注释
- GPUI 迁移期的逐批决策与「记档不修」清单在 `docs/gpui-migration-progress.md`——改到相关模块（拖拽/托盘/标题栏/关窗/toast 等）前先查该文档对应批次的记档，很多「看起来是 bug」的行为是评审定稿的取舍
- 领域术语表在 `CONTEXT.md`（会话/会话来源/项目等 ubiquitous language）
