# Spec：移动端发起 AI 会话 v1（含协议 v2 与桌面端鉴权）

> 状态：ready-for-agent · 定稿 2026-07-25
> 决策背景：`docs/adr/0002-mobile-started-ai-sessions.md` · 术语：根目录 `CONTEXT.md`
> 前置体系：`docs/specs/mobile-relay-v1.md`（移动端 + 中转 v1）

## Problem Statement

移动端 v1 只覆盖了"续"，没覆盖"起"：我人在外面，能看见桌面上正在跑的 agent、能给它们发指令，但只要新任务需要一个**新**的 AI 会话，我就必须回到电脑前把它拉起来。通勤路上想到一件该让 agent 去做的事，只能记在备忘录里等回家。

同时体系里留着一个洞：中转的 `/ws/desktop` 端点没有鉴权，握手只校验协议版本，且新连接顶替旧连接。任何知道中转地址的人都能冒充桌面端——顶掉真桌面、收走移动端发来的指令、索取配对码把手机凭证顶掉。中转地址不是秘密：PWA 就托管在它上面。

## Solution

移动端首页右下角一个 **+ 悬浮按钮**，点开弹层：选项目 → 选 **AI 启动器** → 桌面端在该项目新建一个 tab、按启动器把 AI CLI 拉起来。启动器是桌面端配置的具名条目 `{名称, shell（可选）, 命令}`，移动端传的是启动器 id，命令文本从不经过移动端或中转。会话起来后自动进入对话镜像，此后与手动开的会话完全一致。

同一次协议 v2 变更里，桌面端接入中转改为携带部署方配置的共享密钥，中转未配置密钥时拒绝一切桌面连接。

裸 shell 依然不可见、不可驱动；移动端依然不能自拟命令。

## User Stories

### 发起会话

1. 作为移动端用户，我想在首页看到一个"新建会话"入口，以便不必先有活跃会话也能开工。
2. 作为移动端用户，我想在弹层里看到桌面端的全部项目，以便挑一个开始新任务。
3. 作为移动端用户，我想为这次会话选一个 AI 启动器（如 Claude / Codex），以便决定用哪个 agent 干活。
4. 作为移动端用户，我想在只配了一个启动器时跳过选择直接开，以便少点一步。
5. 作为移动端用户，我想发起后立刻看到"启动中"，以便知道请求已经被受理。
6. 作为移动端用户，我想 AI 真正起来后自动进入该会话的对话镜像，以便直接开始交代任务。
7. 作为移动端用户，我想启动迟迟不成功时看到明确的失败提示而不是一直转圈，以便知道要回桌面看一眼。
8. 作为移动端用户，我想"启动中"期间新建按钮被禁用，以便手抖连点不会开出一串会话。

### 启动器配置

9. 作为 mini-term 用户，我想在「移动端」面板里维护 AI 启动器列表（增删改），以便决定手机能起哪些 agent。
10. 作为 mini-term 用户，我想首次使用时就有 Claude / Codex 两条预置启动器，以便零配置直接用。
11. 作为 mini-term 用户，我想给启动器可选地绑定一个 shell，以便表达"在 WSL bash 里跑 claude"这类需求；不绑定则用默认 shell。
12. 作为 mini-term 用户，我想在保存一条不会被识别为 AI 会话的启动命令时收到警告，以便在配置时就发现问题，而不是每次在手机上等到超时。

### 桌面端表现

13. 作为 mini-term 用户，我想移动端发起的会话在桌面端就是一个普通的新 tab，以便回到电脑前无缝接管。
14. 作为 mini-term 用户，我想桌面端**不因为**远程发起而切换当前项目或 tab，以便我桌面上正在看的现场不被改动。
15. 作为 mini-term 用户，我想桌面端在移动端发起会话时弹一条提示（项目名 + 启动器名），以便回来后知道这个 tab 从哪来，也以便凭证被盗时留下痕迹。
16. 作为 mini-term 用户，我想启动失败的 pane 被保留而不是自动关掉，以便我能看到它卡在哪一步。

### 不可用场景

17. 作为移动端用户，我想 SSH 远程项目与 WSL 根项目的新建入口置灰并说明"对话镜像不可用"，以便不去开一个看不见回复的会话。
18. 作为移动端用户，我想桌面端离线时新建入口不可用，以便不产生"已经开了"的错觉。
19. 作为移动端用户，我想启动器被删光时得到"请先在桌面端配置启动器"的提示，以便知道该去哪补。

### 桌面端鉴权

20. 作为自托管运维者，我想中转要求桌面端出示共享密钥，以便陌生人无法冒充我的桌面端。
21. 作为自托管运维者，我想中转未配置密钥时拒绝一切桌面连接并在启动日志里说清楚，以便不会以为"能跑就是配好了"。
22. 作为 mini-term 用户，我想密钥不匹配时桌面端给出明确提示且不再无脑重连，以便一眼看出是配置问题而不是网络问题。

## Implementation Decisions

### 协议（v2）

`PROTOCOL_VERSION` 提到 `2`。两端仍是严格相等校验——中转与桌面端必须同版本升级，不做版本矩阵兼容（1×1 自托管拓扑，升级成本可控）。PWA 由中转托管，自动跟随。

新增/变更消息（`relay-server/protocol`，TS 侧 `mobile/src/protocol.ts` 手写镜像同步）：

- `DesktopToRelay::Hello` 增字段 `desktopKey: String`。
- `MobileToRelay::StartAiSession { requestId, projectId, launcherId }`
- `RelayToDesktop::StartAiSession { requestId, projectId, launcherId }`（原样转发）
- `DesktopToRelay::StartSessionReceipt { requestId, ok, paneId?, reason? }`
- `RelayToMobile::StartSessionReceipt { requestId, ok, paneId?, reason? }`
- `StartSessionFailReason`：`desktopOffline` | `projectNotFound` | `launcherNotFound` | `notSupported`（远程/WSL 根项目）| `spawnFailed`
- `MobileProject` 增 `canStartSession: bool`；**无活跃 pane 的项目也进快照**（`panes` 为空数组）。
- `SessionsSnapshot` 增 `launchers: Vec<MobileLauncher { id, name }>`——只有 id 与展示名，命令与 shell 不下发。启动器配置变化时桌面端重发一次全量快照（不为它单开增量消息）。

`ok: true` 的语义是**"pane 已创建、启动命令已写入 PTY"**，不承诺 AI 已经起来——与既有 `CommandReceipt` 的语义纪律一致。

### 桌面端

- `AppConfig.mobileRelay` 增 `launchers: Vec<AiLauncher>` 与 `desktopKey: String`。`AiLauncher = { id, name, shell: Option<String>, command: String }`，`shell` 引用 `availableShells` 里的条目名，缺省用 `defaultShell`。
- 配置缺省预置两条：`Claude → claude`、`Codex → codex`。旧 config 无 `launchers` 字段时按缺省填充。
- 启动器编辑 UI 落在「移动端」面板（`MobileRelayModal.tsx`），与中转地址、配对二维码同处。桌面端**不**暴露启动器入口（新建 tab 菜单不变）。
- 保存启动器时做**非阻塞**校验提示：命令首词不在 `AI_COMMANDS`（`claude` / `codex` / `opencode`，见 `pty.rs`）或带 `-p` / `--print` 等非交互标志时，提示"这条命令不会被识别为 AI 会话"。校验只是体验前移，**不是安全防线**——防线是"命令只能来自桌面端配置"。
- 后端收到 `StartAiSession`：校验 launcher 存在、项目存在、项目非 SSH 远程且非 WSL 根项目；任一不过直接回失败回执。通过则 emit Tauri 事件 `mobile-start-session { requestId, projectId, launcherId, launcherName, shellName, command }` 给前端。
- 前端处理该事件：`createProjectPty(project, shell)` → `addTab(projectId, tab)`（**不** `setActiveProject`、**不**切 tab）→ `saveLayoutToConfig` → `write_pty(ptyId, command + "\r")` → 推一条通知（`AiCompletionNotification` 新增 `kind: 'mobile-session'`，文案含项目名与启动器名）→ 调用新 command `mobile_relay_start_session_result { requestId, ok, paneId?, reason? }`。
- 写入沿用 `write_pty` 全语义（输入跟踪 / AI marker），与 `MobileCommand` 同一条路径——AI 会话身份靠输入检测建立，这是唯一能让 pane 进入 `ai-idle` 的途径，不能改成把 AI CLI 当 PTY 根程序 spawn。
- `create_pty` 后紧接着写命令有既有先例（右键「SSH 连接」路径）。
- 项目可见性：`mobileSessionSync.ts` 改为上报 `config.projects` 全集，`canStartSession = !sshConnectionId && !isWslRootPath(path)`。仅含 AI pane 的裁剪规则**只作用于 `panes` 数组**，不再决定项目是否进快照。
- 桌面端 Hello 携带 `desktopKey`；中转回 `HelloReject` 时区分版本不匹配与密钥不匹配，后者同样停止重连并在「移动端」面板给出明确文案。

### 中转

- 环境变量 `MT_RELAY_DESKTOP_KEY`。**未设置 = 拒绝一切桌面连接**（fail-closed），启动日志打印一行明确说明。
- 桌面握手校验顺序：版本 → 密钥。密钥不匹配记日志（不记密钥本身）并关连接。
- `StartAiSession` / `StartSessionReceipt` 按既有模式纯转发；桌面端离线时中转在路由层直接回 `ok: false, reason: desktopOffline`，与移动端指令的离线即拒行为一致。

### 移动端

- 首页布局不变（仍只渲染有活跃 pane 的项目），右下角加 + 悬浮按钮。
- 弹层两步：选项目（快照全集，`canStartSession=false` 的置灰并标注原因）→ 选启动器；启动器只有一条时跳过第二步。
- 发起后进入"启动中"：禁用 + 按钮，记下 `requestId`。收到 `ok: true` 后开始等 `paneId` 出现在快照 → 出现即自动 `openMirror`。**15 秒**未出现则提示"未能进入 AI 会话，请到桌面查看"并复位。
- `ok: false` 直接按 reason 出对应文案。
- 桌面端离线 / 启动器列表为空时，+ 按钮置灰并给出原因。

## Testing Decisions

沿用 `docs/specs/mobile-relay-v1.md` 的两条缝，不新开。

**Seam 1 — 中转协议边界**（`relay-server/server/tests/`）：
- 桌面端密钥正确 → 握手成功；密钥错误 / 缺失 → 拒绝并关连接；中转未配置密钥 → 拒绝一切桌面连接。
- `StartAiSession` 移动端 → 桌面端双向转发，`StartSessionReceipt` 回程转发。
- 桌面端离线时 `StartAiSession` 在路由层即回 `desktopOffline`。

**Seam 2 — 桌面端 cargo 单测**：
- `AiLauncher` / `desktopKey` 的 config serde round-trip；旧 config（无 `launchers`）加载后得到预置两条。
- `launcherId` → 启动命令与 shell 的解析；未知 id 返回 `launcherNotFound`。
- `canStartSession` 判定：SSH 远程项目与 WSL 根项目为 false，普通 Windows 路径项目为 true。
- 协议 v2 消息的 camelCase round-trip 与版本号常量。

**不开自动化缝**：PWA 与桌面前端界面（仓库无前端测试基建，维持现状，手动验证）。

## Out of Scope

- 移动端关闭 / kill pane。
- 移动端自拟启动命令，或任何形式的远程任意命令执行。
- 裸 shell pane 的可见与驱动（ADR 0001 与 0002 共同的红线）。
- WSL / SSH 远程项目的对话镜像（v1 遗留限制，独立立项）。
- 后台 pane 的终端输出缓冲回放（早期输出丢失问题早于本需求存在）。
- 启动失败 pane 的自动清理。
- 中转对桌面端的多实例仲裁（仍是后到顶替）。
- 协议版本矩阵兼容 / 能力协商。
- 项目级启动器、桌面端复用启动器（新建 tab 菜单）。
- Web Push 通知新会话事件。

## Further Notes

- **WSL 关联项目**（项目根是普通 Windows 路径、另行声明了发行版）**不**置灰：它的镜像可用与否取决于启动器把 AI 起在哪一侧。若用户配了个在 WSL 里跑 claude 的启动器，镜像会是空的——这是既有的 v1 镜像限制在新入口下的又一次暴露，不在本次范围内解决。
- 新建的 tab 会随 `saveLayoutToConfig` 持久化，下次桌面启动时按既有布局恢复逻辑重建（无 PTY，等挂载时懒创建），无需特殊处理。
- 该 pane 在桌面终端里的早期输出会丢（`terminalCache.ts` 的全局 `pty-output` 监听对未创建 xterm 的 ptyId 直接丢弃）。切过去时 TUI 会重绘当前屏，但 scrollback 历史没有；对话内容可从对话镜像或 Sessions 面板回看。
- 现有部署（`relay.dreaminglong.com`）升级时必须同时：重新构建并部署中转、配置 `MT_RELAY_DESKTOP_KEY`、在桌面端「移动端」面板填入同一值。三者缺一，桌面端就连不上——这是 fail-closed 的有意代价，部署文档（`docs/deploy-relay.md` 与 `.zh-CN.md`）需同步写明。
