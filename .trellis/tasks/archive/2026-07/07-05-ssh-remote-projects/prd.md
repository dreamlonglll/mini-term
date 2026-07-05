# SSH 远程开发：远程项目（文件树/终端/Session）

## Goal

把「SSH 远程机器上的目录」提升为 mini-term 的一等项目（类 VS Code Remote-SSH）：
项目列表可新建「SSH 远程项目」，指向某个已保存的 SSH 连接 + 远程目录；文件树浏览远程
文件、新开终端自动 SSH 进入项目目录、Session 块查看远程机器上的 claude/codex 历史会话。

现状：SSH 只是「终端里的一条命令」（右键手动连接 + 密码自动填充 + SSH MCP 共享给 agent），
项目的文件树/Session/Git 全部只支持本地。本任务补齐「远程开发」的最小可用形态。

需求已通过 grilling 访谈逐分支收敛（2026-07-05），本 PRD 为最终共识。

## Requirements

### R1 通信层：抽出 russh 会话池为共享 crate

* 把 `src-tauri/mt-sidecars/src/pool.rs` 的 russh 持久会话池抽到新 crate `mt-ssh`
  （不塞进刻意轻量的 `mt-core`）。
* `mt-sidecars`（mt-ssh-mcp）与主程序各自持有一个池实例，行为不变。
* 主程序通过池获得 SFTP（readdir/stat/read/canonicalize）与 exec 能力，
  session 按 `connection.id` 全局复用。
* known_hosts 沿用池的 accept-new 策略（写 `~/.ssh/known_hosts` 明文条目，
  与系统 ssh 共用同一文件，语义一致）。

### R2 配置模型与创建入口

* `ProjectConfig` 新增 `sshConnectionId?: string`：有值即远程项目；
  `path` 存远程 POSIX 绝对路径。不内嵌连接快照，引用为单一来源。
* 连接被删除 → 项目进入「断链」错误态（列表可见、可删除项目，功能入口给出明确错误提示），
  与 `SshAssocModal` 过滤陈旧 id 的既有模式一致。
* 项目列表「添加项目」旁新增「添加远程项目」：弹窗选已有 SSH 连接 + 手输远程路径
  （默认 `~`），保存前用 SFTP stat 验证目录存在（`~` 用 SFTP canonicalize 展开）。
* 项目列表中远程项目带远程标识（类似 WSL 标识的既有样式）。

### R3 远程文件树

* FileTree 对远程项目走新 command（SFTP readdir），懒加载逐目录展开，与本地树同一组件渲染。
* 忽略过滤：项目根 `.gitignore`（SFTP 读一次、按项目缓存）+ `ALWAYS_IGNORE` 固定黑名单；
  不做逐级 `.gitignore` 收集。
* 刷新：每次展开目录重新拉取 + 树顶手动刷新按钮；不做轮询、不做 notify 监听。
* 操作范围（MVP 只读）：浏览、复制相对/绝对路径（POSIX 分隔符）、拖拽文件到终端插入远程路径；
  「资源管理器显示 / 默认应用打开 / 重命名 / 删除」对远程项目隐藏。

### R4 终端自动连接

* 远程项目下新开 tab/分屏 pane：PTY 子进程直接 spawn
  `ssh -t [-p port] [-i 临时私钥] user@host "cd <远程路径> && exec $SHELL -l"`，
  不经过本地 shell（对齐 WSL 根项目 spawn wsl.exe 的启动器重写模式）。
* 复用现有 `prepare_ssh_key`（私钥权限临时副本）与 `arm_ssh_autofill`（密码自动填充）。
* 每个 pane 是独立 ssh 进程；本机需有 OpenSSH 客户端（Win10+ 自带），缺失时报错提示。
* 断线 UX：ssh 进程退出后 pane 保留，叠加「连接已断开，点击重连」提示；点击在同一 pane
  重新 spawn ssh（重走密码/私钥链路）。不做自动重连。
* AI 状态感知：走现有降级路径（PTY 输入/输出扫描标记 AI 会话 + 近期输出判 working/idle），
  对远程天然可用；hook 精确状态不可用，接受降级，不另做远程探测。
* 布局持久化：远程项目恢复布局时所有 pane 统一重开 ssh。

### R5 远程 Session 块

* 新 command 扫描远程 `~/.claude/projects/<编码路径>` 与 `~/.codex/sessions`
  （走 SFTP，照搬 WSL 会话的扫描/混排/分段加载模式）。
* 远程 `$HOME` 用 SFTP `canonicalize(".")` 获取；claude 项目目录编码复用
  `ai_sessions.rs` 现有路径编码逻辑。
* 会话列表按时间混排、带远程标识；正文查看走 SFTP 读（含增量 offset 读取）。
* 「复制恢复命令」天然可用（纯字符串，用户粘贴到远程终端执行）。

### R6 本地消费者 gate（横切）

所有消费 `project.path` 的本地逻辑，对远程项目必须显式跳过或改走远程链路，逐一核对：

* git 状态轮询（`git.rs`）→ 跳过（远程 Git 二期）
* `notify` 文件监听（`fs.rs` watch）→ 跳过
* 本地 `get_ai_sessions` → 改走 R5 远程链路
* 项目环境变量注入 → 远程项目隐藏 envVars 入口（二期考虑注入远程 shell）
* 「关联 SSH MCP」→ 远程项目不提供（agent 本来就跑在远程机上，`.mcp.json` 也写不进远程目录）
* cc-connect 项目导入等其余 path 消费点 → 实现阶段全量 grep 核对

## Acceptance Criteria

* [ ] `mt-ssh` crate 抽出后，`mt-ssh-mcp` sidecar 全部现有功能回归通过（exec/upload/download、
      会话池 keepalive/LRU/懒重连、PKCS#1 私钥、known_hosts accept-new）
* [ ] 能通过「添加远程项目」创建指向 Linux 机器的项目；路径不存在时保存被拒并提示
* [ ] 远程文件树能懒加载展开多级目录；根 .gitignore 与 ALWAYS_IGNORE 生效；手动刷新可用
* [ ] 树节点复制路径为 POSIX 格式；拖拽文件到终端插入远程路径
* [ ] 远程项目新开 tab/分屏 pane 自动 SSH 并落在项目目录；密码连接自动填充、私钥连接直连
* [ ] 在远程终端跑 claude，pane 状态能进入 ai-working/ai-idle（降级路径）
* [ ] kill ssh 进程模拟断线，pane 出现重连提示，点击后恢复连接
* [ ] Session 块列出远程机器 claude/codex 会话、带远程标识、可查看正文
* [ ] 远程项目不出现 git 徽章、envVars 入口、关联 SSH 入口；本地项目一切行为不变
* [ ] 连接被删除后，远程项目显示断链错误态且不崩溃
* [ ] `cd src-tauri && cargo test` 全绿；新增逻辑（路径编码、gitignore 合并、断链态等）有单测

## Definition of Done

* Rust 单测覆盖新增纯逻辑；`cargo test` / 前端 build / lint 全绿
* README（中英）功能说明与 Commands 清单更新
* spec：新增「远程项目」契约文档并登记索引（对齐 `spec/backend/wsl-unc-session-scanning.md` 惯例）
* i18n 双语文案齐全（zh/en）

## Technical Approach

关键决策（均已与用户确认）：

1. **通信层**：抽 `pool.rs` → `mt-ssh` crate，主程序直连 russh 池（否决：spawn 系统 ssh
   做文件操作——Windows 无 ControlMaster，每次操作重新握手；sidecar IPC 代理——自建协议复杂脆弱）。
2. **PTY 形态**：直接 spawn ssh 作 PTY 子进程（否决：本地 shell 写 ssh 命令——语义模糊；
   russh 自建远程 PTY 通道——需重写整套 pty.rs 生命周期，风险最大）。
3. **忽略过滤**：根 .gitignore + 黑名单（否决：完整逐级收集——公网延迟下逐层 SFTP 往返太慢）。
4. **断线**：pane 内一键重连（否决：自动重连——无法区分用户主动 exit 与异常断线）。
5. **AI 感知**：接受 hook 缺失、用输出扫描降级——从代码验证该路径与本地/远程无关。

## Decision (ADR-lite)

**Context**: 主程序需要访问远程文件系统；sidecar 已有成熟 russh 会话池但主程序不依赖 russh。
**Decision**: 抽共享 crate `mt-ssh`，主程序与 sidecar 各持一池；同一机器可能同时存在两条连接，接受。
**Consequences**: 主程序二进制引入 russh/tokio 依赖；池代码单一来源便于后续远程 Git、树写操作复用；
抽包重构是全特性的前置工程，必须先行且独立回归。

## Out of Scope（二期清单）

* 远程 Git 状态（徽章/轮询，需远程 exec）
* 文件树写操作（重命名/删除/新建）与 SFTP 目录浏览选择器
* envVars 注入远程 shell、完整逐级 .gitignore
* Windows SSH server 远程（MVP 仅 POSIX：Linux/macOS）
* 自动重连、远程 hook 精确 AI 状态
* 现有「终端右键 SSH 连接」功能保持原样，不动

## Technical Notes

* 已勘察文件：`mt-sidecars/src/pool.rs`（russh 0.61 + ring，SFTP=russh-sftp 2.3，
  keepalive 30s×3 / idle 10min / LRU 8 / gatetime 30s）、`mt-core/src/ssh_connection.rs`、
  `pty.rs`（arm_ssh_autofill、output_contains_ai_command、decide_wsl_override 先例）、
  `process_monitor.rs`（hook 优先 + 输出扫描降级）、`fs.rs`（ignore crate 逐级收集、
  ALWAYS_IGNORE）、`ai_sessions.rs`（WSL 会话先例）、`SshModal/SshAssocModal/FileTree/SessionList`。
* gitignore 远程复用：SFTP 读根 `.gitignore` 内容 → `ignore::gitignore::GitignoreBuilder`
  逐行 `add_line`；匹配用相对路径避免 Windows `Path` 对 POSIX 绝对路径的歧义。
* 前端凡涉及远程路径的 relativePath 计算不得假设 `\` 分隔符。
* 参考 spec：`spec/backend/wsl-unc-session-scanning.md`（缓存锁不跨慢 IO、双 normalize、
  静默降级等惯例同样适用于 SFTP 慢 IO）。
