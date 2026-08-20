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

# 全工作区测试（26 个目标 1300+ 例）
cargo test --workspace

# 中转服务端协议边界测试 / sidecar 工作区
cd relay-server && cargo test
cargo build --manifest-path sidecars/Cargo.toml

# 移动端 PWA
cd mobile && npm run build

# 改文案后重新生成 i18n 字典
node crates/mt-i18n/tools/gen_from_ts.mjs
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
| `mt-config` | 配置持久化与主题包。不依赖 gpui |
| `mt-i18n` | 双语文案层。**字典源头是 `locales/*.ts`**（TS 对象字面量，随 Tauri 版下线迁入），`src/dict.rs` 由 `tools/gen_from_ts.mjs` 生成——**禁止手改 dict.rs**，改文案改 locales 后重跑生成器，`tests/consistency.rs` 的对账常量随之更新 |
| `mt-relay` | 移动端中转桌面侧：出站 WSS 长连、配对、项目快照/增量、对话镜像（`mirror.rs`）、移动端指令写穿 |
| `mt-ssh` | 共享 SSH 通信层（russh 持久会话池 + SFTP 原语），主程序与 sidecar 共用 |
| `mt-usage` | 用量统计：会话轮次解析 / SQLite 账本 / 聚合 / 计价 |
| `mt-core` | 叶子共享库（WSL UNC 解析 / SSH 提示扫描 / 原子写等）。⚠️ 依赖方向铁律：只依赖 serde/serde_json/dirs，绝不反向依赖上层 crate——它同时被三个 sidecar 与 mt-ssh 链接 |

### PTY 数据流（进程内，无 IPC）

reader 线程读 PTY 字节直接喂 `mt-terminal` 的 VT 状态机，UI 按帧取 grid 渲染。
原 Tauri 版的 16ms 批缓冲 / 有界 channel / 4MB-1MB 双水位背压 / 30s 超时兜底整套
是为 WebView IPC 边界造的，已随架构作废；孤儿 PTY 回收同理（单进程无失引用链路）。

### AI 状态判定（idle / ai-idle / ai-working）

hook 上报（`mt-ai::hook_server`）一旦启用即为权威，退出以 SessionEnd 为准；无 hook 时降级为输入检测（`mt-ai::detect` 识别键入的 `claude`/`codex`/`opencode`/`pi`/`grok` 命令，含 ↑ 历史/Tab 补全的行快照兜底与输出回扫）+ 输出活跃度轮询。非 hook 的例外有两条：

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
- 只有 Claude/Codex/Grok 有可解析的会话记录（`mt-relay::mirror` 的 `agent_has_session_log`）。opencode/pi 这类**只靠输入检测识别**的 agent 拿得到状态徽章与移动端指令，但没有对话镜像、AI 历史面板与用量统计——镜像必须据此跳过启发式绑定，否则会绑到同项目其它 agent 的最新会话文件，把别人的对话贴到该 pane 上
- Grok 的会话记录形态与另外两家不同：一个会话是**一整个目录**（`{grok_home}/sessions/{URL 编码的 cwd}/{session-id}/`，正文 `updates.jsonl` 是 ACP 更新流，一条消息拆成多个 chunk 行、攒到边界才成一条；元信息在 `summary.json`）。定位项目走**解码目录名**而非编码项目路径，详见 `mt-ai::sessions` 的 Grok 段注释
- GPUI 迁移期的逐批决策与「记档不修」清单在 `docs/gpui-migration-progress.md`——改到相关模块（拖拽/托盘/标题栏/关窗/toast 等）前先查该文档对应批次的记档，很多「看起来是 bug」的行为是评审定稿的取舍
- 领域术语表在 `CONTEXT.md`（会话/会话来源/项目等 ubiquitous language）
