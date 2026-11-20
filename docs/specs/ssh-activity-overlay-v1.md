# Spec：SSH 交互浮层 v1（可视化 mini-term-ssh）

> 状态：draft · 2026-11-20 · 分支 `feat/ssh-activity-overlay`
> 前置：[`ssh-cli-skill-v1.md`](ssh-cli-skill-v1.md)（`mt-ssh-cli` + daemon + SKILL.md 的现状）

## Problem Statement

AI 通过 mini-term-ssh skill（`mt-ssh-cli`）在服务器上执行命令时，用户只能在 AI 的 TUI 里看到被折叠、截断过的工具调用，看不清 AI 到底向服务器发了什么、服务器回了什么。

## 用户口径（已拍板）

1. **形态**：每个终端 pane 内容区**右上角**一颗悬浮按钮（与 Ctrl+F 查找条同一个角）。该 pane 里第一次用到 mini-term-ssh 时按钮出现；点击后在按钮下方弹出悬浮框。**不放进右侧抽屉**。
2. **悬浮框底色**：鼠标不在悬浮框上时是**淡底**（约 30% 透明度的终端底色，常量可调），鼠标移入变**实底**，移出恢复。
3. **内容**：只显示 **AI 与服务器的交互**，全部原文：
   - `exec`：AI 发出的原始命令（不脱敏）、服务器的原样输出（stdout + stderr）、退出码 / 超时 / 错误；
   - `upload` / `download`：本地路径、远端路径、传输字节数 / 错误。
   - **不显示** `list`（只读本地配置、不碰服务器），也不显示 CLI 自己的降级提示、用法错误等本机消息。
4. **不做 WSL 支持**。
5. 其余默认（用户未提异议）：悬浮框区域始终拦截鼠标（能滚动、能点按钮）；点悬浮框外部**不关闭**，只有再点按钮或点 ✕ 才收起；悬浮框不抢键盘焦点。

## 非目标（v1 不做）

WSL pane、远程 SSH 项目的 pane（AI 跑在远端）、`list`、旧的 `mt-ssh-mcp` 路径、持久化（重启即清空）、移动端镜像、设置页开关、upload/download 进度。

**不改**：daemon、CLI↔daemon IPC 协议、SKILL.md、hook server（`/hook` 路由与端口一个字不动）。

## 架构

```
AI 的 Bash 工具 → mt-ssh-cli（继承 pane 环境：MINITERM_SSH_FEED=127.0.0.1:<port>/<nonce>）
   ├─ 照旧：daemon / one-shot → 远端服务器（stdout/stderr/退出码契约不变）
   └─ 新增：后台线程把 start / out / end 写成 NDJSON → 127.0.0.1:<port>（一次调用一条 TCP 连接）
mt-app ssh_activity 监听线程 → 按 nonce 认 pane → mpsc → GPUI 任务 → SshActivity 模型
   → TerminalPane 右上角按钮 + 悬浮框
```

为什么在 CLI 侧抄送（而不是 daemon / hook server / 审计日志）：

- CLI 的 `run_plan`（`sidecars/src/bin/mt-ssh-cli.rs`）是所有业务调用的唯一入口，daemon 路径（`run_via_daemon` 收帧循环）与降级 one-shot 路径（`ssh_service::exec` 回调）的输出都经过它，抄送点覆盖 100%；daemon 看不到 one-shot 路径，且是全机单例，不知道该报给 dev 实例还是装机版。
- CLI 继承 pane 的环境变量，天然知道自己属于哪个 pane、哪个 mini-term 实例。
- hook server 缺省关闭（`hook_enabled=false` 时不起服务器、不注入端口），单线程顺序处理请求，流式输出会挡住 AI 状态事件；`/hook` 路由按约定不能改。

## 通道与线协议

### 环境变量

- 名字：`MINITERM_SSH_FEED`，值：`127.0.0.1:<port>/<nonce>`。
  - `nonce`：每个 PTY 一个，128 bit 随机数的小写 hex（32 字符）。用于**归属**（nonce → pty_id）与**防伪造**（同机其它进程拿不到 nonce）。
  - 变量名刻意避开 `KEY` / `SECRET` / `TOKEN` 字样：Codex 的 shell 环境策略可能按名字过滤这类变量。
  - `MINITERM_` 是保留前缀，用户 / 项目环境变量覆盖不了。
- 只注入**本地 pane**（`store/panes.rs` 的 `start_pty_with`，与 `MINITERM_PTY_ID` 同处）；远程 SSH pane 不注入。WSL pane 照常注入给 `wsl.exe`，但不进 `WSLENV`，到不了发行版内——即不支持，符合口径，`mt-pty/src/launch.rs` 不改。
- 监听没起来时不注入。

### 连接与帧

- 一次 CLI 业务调用（`exec` / `upload` / `download`）= 一条 TCP 连接，内容是 newline-delimited JSON（serde camelCase），UTF-8，每行一帧。
- **第一帧必须是 `start`**，且必须在连接建立后 2s 内到达；nonce 不认识、首帧不是 `start`、解析失败 → 主程序直接断开，不产生任何记录。
- 帧定义（版本字段 `v` 只出现在 `start`，当前为 1；主程序遇到不认识的 `v` 断开）：

```jsonc
// 调用开始（CLI 在把请求交给 daemon / one-shot 之前发）
{"type":"start","v":1,"nonce":"<32 hex>","op":"exec|upload|download",
 "connection":"<AI 传的连接名或 id>",
 "command":"<exec：join 之后的原始远程命令>",   // 仅 exec
 "cwd":"<--cwd 原值>",                         // 仅 exec，可缺省
 "timeoutSecs":60,                             // AI 显式传了才有
 "localPath":"<绝对化之后的本地路径>",          // 仅 upload/download
 "remotePath":"<远端路径原值>"}                 // 仅 upload/download

// 服务器输出分片（仅 exec；原始字节 base64，单帧原始数据 ≤ 32 KiB）
{"type":"out","stream":"stdout|stderr","dataB64":"..."}

// 调用结束（恰好一帧，随后 CLI 关闭连接）
{"type":"end",
 "exitCode":0,            // exec 拿到的远程退出码（信号终止时为 128+n）；缺省 = 没有
 "timedOut":false,        // exec 运行中超时
 "notStarted":false,      // exec 连接阶段超时，命令未启动
 "bytes":1234,            // upload/download 成功传输的字节数
 "error":"<错误原文>",     // 连接 / 认证 / 令牌 / 服务端拒绝 / daemon 中途断开等；绝不含密码（沿用 service 层纪律）
 "outputBytes":56789,     // exec 本次调用服务器输出的总字节数（含未镜像部分）
 "truncated":false}       // 有输出没镜像出去（超上限或队列满丢弃）
```

- 连接在 `end` 之前断开（CLI 被杀、Bash 工具超时）→ 主程序把这条记成「已中断」。
- daemon 在信号终止时补发的说明行（`mt-ssh-cli: remote command terminated by signal ...`）本来就是 stderr 帧，照常镜像。CLI 自己打印的降级提示、`finish_exec` 的收尾说明行**不镜像**——状态由 `end` 帧表达，界面自己渲染文案。

### CLI 侧约束（`sidecars/`）

- 新模块 `sidecars/src/activity_feed.rs`（lib 内，便于单测），只被 `mt-ssh-cli` 使用；`mt-ssh-mcp` 不接。
- 只在 `MINITERM_SSH_FEED` 存在且解析成功时启用；解析失败静默不启用。**不回落去读任何端口文件**。
- 只对 `exec` / `upload` / `download` 上报；`list`、`daemon*` 运维子命令、`run_plan` 之前就失败的用法错误（clap、MSYS 改写检测、本地路径绝对化失败）都不报。
- `start` 在 `run_plan` 入口发一次（提权直走 one-shot、daemon 不可用降级 one-shot 都只发一次，不重复）。
- **绝不影响 CLI 本身**：stdout / stderr 输出、退出码、时序都与现在一致；feed 自己的任何失败都不打印、不改退出码。
  - 网络 I/O 全在独立后台线程；业务路径只做非阻塞入队（有界队列，满了丢弃并记 `truncated`）。
  - 连接超时 ≤ 200ms，写超时 ≤ 500ms；连不上或写失败 → 本次调用整体放弃上报。
  - 镜像上限：单次调用 1 MiB 原始输出，超出部分不再发 `out`，只计数，`end.truncated=true`。
  - `main` 以 `std::process::exit` 结束：退出前必须把 `end` 交给后台线程并等它写完，**总等待 ≤ 500ms**，到点放弃。
- sidecar 版本号升到 `0.4.13`（`sidecars/Cargo.toml` + `Cargo.lock`）；IPC 协议版本不变。

### 主程序侧约束（`crates/mt-app`）

- **监听**：启动时 `127.0.0.1:0` 绑随机端口，不依赖 hook 开关；accept 循环一条线程、每个连接一条读线程（或等价方案），不阻塞 UI 线程。绑定失败只记日志、不注入变量，功能整体关闭。
  - 单行上限 256 KiB，超限断开该连接；同时在跑的连接数设上限（如 64），超出直接关。
  - 解码后的事件经 `futures::channel::mpsc` 交给 GPUI 任务（参照 `main.rs` 里 `ai_events` 的泵），在 UI 线程更新模型。
- **nonce 表**：`nonce → pty_id`，起 PTY 时登记、PTY 关闭（含重启换 PTY）时注销；监听线程只读这张表做认证。
- **模型**（只在内存，不落盘）：每个 pty 一个按时间排序的记录队列。
  - 记录：op、连接名、命令 / cwd / 超时、本地 / 远端路径、开始时间、结束结局（运行中 / 退出码 / 运行超时 / 连接阶段超时 / 错误原文 / 已中断 / 传输字节数）、耗时、输出段（stdout / stderr 交错保序）、输出总字节、是否截断。
  - 上限（主程序侧再兜一次，不信任客户端）：单条记录镜像输出 1 MiB；每个 pane 合计 4 MiB 或 500 条，超了丢最早的整条。
  - 输出解码：按流（stdout / stderr 各自）做 UTF-8 增量解码，跨分片的多字节字符要拼接，非法字节按 lossy 替换；剥掉 ANSI 转义（CSI / OSC 等）；`\r\n` → `\n`，残留的裸 `\r` 按换行处理。
  - PTY 关闭 → 该 pty 的记录与 nonce 一起清掉。
  - 刷新节流：同一 pane 的输出更新合并通知，≤ 约 10Hz；只让相关 pane 重画，不能每个输出分片都把所有 pane 重画一遍。
- **日志纪律**：`eprintln!` 只记连接数 / 错误类别，**绝不写命令原文或输出**。

## 界面

### 悬浮按钮（`TerminalPane`，`crates/mt-app/src/pane.rs`）

- 显示条件：该 pane 至少有一条记录。清空后消失。
- 位置：pane 根容器内绝对定位，右上角 `top 6px / right 14px`（与查找条同一锚点；右侧 14px 天然避开终端滚动条）。**查找条打开时按钮下移到查找条下方**（间距 6px），两者不重叠。
- 外观：实底（`bg_elevated` + `border_default`，与右下角「↓」回到底部浮钮同一家族），高度 ≥ 28px，字号 13px 起；内容是 SSH / 服务器类图标（优先用现有图标集，没有就用文字 `SSH`）+ 记录条数。
  - 有调用在跑：右上角一颗 accent 小圆点。要做呼吸效果只能走 `mt_ui::motion::pulse_phase` 低频泵，**禁止 `with_animation(..).repeat()`**。
  - 没有在跑、且最近一条结局是失败（退出码≠0 / 超时 / 错误 / 已中断）：红色小圆点。
- `.occlude()`：点按钮不能穿透到终端（不起选区、不发鼠标上报）。
- tooltip：「AI 的 SSH 交互」一类文案。
- 点击：切换悬浮框开 / 关；点完把键盘焦点还给终端。

### 悬浮框

- 锚定在按钮正下方、右对齐；宽 `clamp(360px, pane 宽 × 50%, 720px)`，高 ≤ pane 高 × 60%（内容少时按内容收缩，最小约 160px）；pane 太小时随之收缩，不超出 pane。
- **底色**：
  - 未悬停：终端底色 × `IDLE_BG_ALPHA`（常量，初值 0.30）+ 细边框，无阴影；
  - 悬停（鼠标在悬浮框内）：实底 `bg_elevated` + 阴影。
  - 用 group hover 实现，文字颜色两态一致。gpui 没有实时 backdrop blur（见 `frost.rs` 模块注释），不做毛玻璃。
- 始终 `.occlude()`：滚轮在框内滚的是框，点击不穿透到终端。
- 不抢键盘焦点；点框外不关闭；只有再点按钮或点框内 ✕ 才关。开关状态是 pane 级运行时态，不持久化。
- **标题栏**：标题（如「SSH 交互」）+ 条数；右侧「复制全部」「清空」「✕」。清空 = 清掉该 pane 全部记录（按钮随之消失、框关闭）。
- **列表**：按时间顺序，最新在底部；停在底部时新输出自动跟随，用户往上翻时不打扰。必须虚拟化（gpui `list` / `uniform_list` 或等价方案），单条 1 MiB 输出、500 条记录时滚动依然流畅。
- **每条记录**：
  - 头部：连接名（胶囊）+ `$ <原始命令>`（等宽，长命令换行显示全文）；有 `--cwd` 时下一行弱化显示 `cwd: <原值>`；AI 显式传了 `--timeout` 时弱化显示。upload 显示 `↑ 上传 <本地> → <远端>`，download 显示 `↓ 下载 <远端> → <本地>`。头部右侧「复制命令」「复制输出」两个小按钮（可悬停才显）。
  - 输出：等宽 13px，保留空白，长行折行；stdout 用主文字色，stderr 用错误 / 警告色。
  - 结尾状态行：运行中（accent，不跑实时计时器）/ `退出码 0 · 1.2s`（0 为常规色，非 0 为错误色）/ `超时（Ns）` / `连接阶段超时，命令未执行` / `错误：<原文>` / `已中断` / `完成 · N 字节`。截断时加一行 `输出过长，只保留了前 1 MiB（共 N 字节）`。
- 动画：v1 可以不做开合动画；若做，必须是方向性位移（自按钮向下展开）+ 零尺寸变化，不做透明度淡入。
- 文案全部走 i18n：新增 `crates/mt-i18n/locales/sshActivity.ts`（zh / en），在 `index.ts` 注册，`node crates/mt-i18n/tools/gen_from_ts.mjs` 重新生成，`tests/consistency.rs` 的对账常量随之更新。**禁止手改 `dict.rs`。**

## 安全与隐私

- 威胁模型与 hook server 同档：不防同一账户下的本机进程。监听只绑 `127.0.0.1`；nonce 128 bit 挡伪造；CLI 只发往环境变量里那个端口（PTY 是主程序的子进程，主程序活着端口就在它手里），不读端口文件。
- 命令与输出原文只在内存里，不落盘、不进日志、不经中转。

## 兼容性

- 新主程序 + 旧 CLI：没人上报，按钮不出现。
- 旧主程序 + 新 CLI：没有 `MINITERM_SSH_FEED`，不上报。
- SKILL.md 指向装机目录的 CLI，dev 实例不重写 SKILL.md：dev 里 AI 调用的是装机版 CLI。dev 联调直接在 dev pane 里手跑新编出来的 `mt-ssh-cli.exe`（浮层只认 pane 环境，不认调用方是不是 AI）。

## 测试

- **sidecars**（`cargo test --manifest-path sidecars/Cargo.toml`）：环境变量解析（合法 / 缺省 / 畸形）；帧编码；1 MiB 镜像上限与 `truncated`；本地 `TcpListener` 收到完整 start / out / end 序列；端口没人监听时快速放弃、业务返回值不变；对端不读（写阻塞）时退出等待 ≤ 500ms；`list` 不上报。
- **mt-app**（`cargo test -p mt-app`，**不许加 `tests/` 目录**，单测放模块内）：帧解码；未知 nonce / 首帧非 start / 首帧超时 / 超长行被断开；UTF-8 跨分片拼接；ANSI 剥离；`\r` 处理；单条与每 pane 上限；未收到 `end` 断开 → 已中断；PTY 关闭清记录与 nonce；进程内真 TCP 监听 + 假客户端的端到端。
- **i18n**：`cargo test -p mt-i18n`。
- **手工 E2E**（主会话做）：dev 实例 + WSL SSH 靶机，在 dev pane 里手跑新 CLI 的 exec（正常 / 非 0 / 超时 / 大输出 / 中途 Ctrl+C）、upload、download，验证按钮出现、悬浮框内容、淡底 / 实底切换、查找条避让、清空、分屏下各 pane 互不串。

## 验收标准

1. 本地 pane 里 AI（或手动）调用 `mt-ssh-cli exec/upload/download` 后，该 pane 右上角出现按钮，点开能看到命令原文、服务器原样输出与结局；`list` 不出现。
2. CLI 的 stdout / stderr / 退出码与改动前逐字节一致；主程序没开、端口不通、主程序卡住时 CLI 行为不变，额外延迟 ≤ 500ms。
3. 悬浮框未悬停时为淡底、悬停为实底；滚动、复制、清空、关闭正常；不抢键盘焦点；点框外不关。
4. Ctrl+F 查找条打开时按钮与查找条不重叠。
5. 大输出（1 MiB+）、多条记录下界面不卡顿，内存受上限约束。
6. 全工作区测试、sidecars 测试、变更行 rustfmt / clippy 检查通过。
