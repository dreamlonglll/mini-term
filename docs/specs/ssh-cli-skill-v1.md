# Spec：SSH 工具从 MCP 迁移到 CLI + Skill v1

> 状态：draft · 2026-07-31（rev2：CLI 改为守护进程架构；rev3：PR1–PR3 已落地；rev4：code review 安全加固——项目能力令牌、显式 channel 取消、IPC fail-closed；rev5 · 2026-10 · sidecar 0.4.11：传输层修复——拉起 daemon 不再泄漏调用方标准句柄与工作目录、daemon 日志落盘、版本握手只换代更旧的 daemon、shutdown 优雅退出、IPC 客户端校验端点属主、WSL 下 POSIX 本地路径；同属 0.4.11 的 service 层与注册器修复——`--timeout` 覆盖整个请求（含建连）、exec 只在开 channel 时持会话锁、开 channel 短超时后淘汰重试、exec 立即 EOF、服务端拒绝与远端信号的退出码、`--cwd` 引号、审计脱敏扩展与只记可能已执行的命令、传输护栏识别本机别名、能力令牌报错分类、SKILL.md 只预批 list/exec、dev 实例不碰 SKILL.md、启动校正 SKILL.md；同批的会话池与 SFTP 修复——建连读 `~/.ssh/config` 并依次试 agent / 默认私钥 / 密码、建连整段限时、认证失败冷却、兼容老算法、known_hosts 通配与吊销、SFTP 不再全程占会话锁、下载流水线、上传保留属性与软链、密钥重载，见 §3；同属 0.4.11 的评审修复（CLI / daemon / 注册器一侧）——以管理员身份（提权）运行时不用 daemon、`daemon` 子命令提权时拒绝启动，Git Bash（MSYS）改写实参的检测与 SKILL.md 的单引号 / `//` 写法，管道 owner 核验，端点拒绝访问时不拉起直接降级，日志轮转加锁，死 session 持锁标记退役、排队请求不再各耗一次开 channel 超时，SKILL.md 模板版本号与启动校正只朝较新的一方收敛）
> 前置研究结论：Codex CLI 自 2025-12 起原生支持 skills（项目级 `.codex/skills/`），SKILL.md 已是跨 agent 标准格式，单一方案可同时覆盖 Claude Code 与 Codex。

## Problem Statement

现有 SSH MCP（`mt-ssh-mcp` sidecar）能用，但体验有四处硬伤：

1. **WSL 内完全不可用**：`.mcp.json` 里写的是 Windows 绝对路径，WSL 里的 Claude Code 无法把它当 MCP server 拉起来——这是搁置已久的待办；
2. **注册面过宽**：启用一个项目要动 5 个文件（`.mcp.json`、项目 `.codex/config.toml`、`~/.codex/config.toml` 信任、`~/.claude/settings.json` 免审批、`.gitignore`），停用还只能清理一半；
3. **上下文与进程开销**：4 个 MCP 工具 schema 常驻 agent 上下文；每个 agent 会话挂一个常驻 sidecar 进程；
4. **输出契约对 agent 不自然**：远程 stdout/stderr/exit code 被封装进 JSON 信封、100KB 封顶截断，agent 无法管道组合、无法用退出码做 `&&` 链式判断。

## Solution 概览

新增 `mt-ssh-cli` sidecar 二进制（复用 `mt-ssh` crate 的连接与 SFTP 代码），agent 通过 Bash 直接调用；注册方式从「写 MCP 配置」改为「生成两份 SKILL.md」。

CLI 采用 **daemon 架构**：同一个二进制的 `--daemon` 模式作为全局唯一守护进程持有 `SshPool`（对标 MCP sidecar 的常驻池），CLI 调用经本机 IPC 转发给 daemon，避免每次命令重新 SSH 握手。daemon 由 CLI 按需自拉起、空闲自退，对 agent 完全透明。

| 维度 | 旧（MCP） | 新（CLI + Skill） |
|------|-----------|-------------------|
| agent 调用方式 | MCP 工具（JSON 信封） | Bash 调 CLI（原生 stdout/stderr/exit code） |
| 注册产物 | `.mcp.json` + 项目/全局 `.codex/config.toml` + `~/.claude/settings.json` | `.claude/skills/mini-term-ssh/SKILL.md` + `.codex/skills/mini-term-ssh/SKILL.md` |
| WSL | 不可用 | 可用（Windows exe 经 interop 执行，config.json 与 named pipe 都在 Windows 侧） |
| 常驻进程 | 每个 agent 会话一个 sidecar，各持独立池 | 全机一个 daemon，池全局共享，空闲自退 |
| 连接复用 | sidecar 进程内 SshPool | daemon 进程内 SshPool（同一套 acquire / evict+retry / cooldown 编排） |
| 审计 / 护栏 | `ssh-mcp-audit.log` + config.json 传输硬拒绝 | 原样继承（daemon 单点写日志，顺带消除多进程并发追加交错） |
| project 范围过滤 | `--project-id` 写在 `.mcp.json` args（存量 MCP 进程级） | 随机 `--project-token` 写死在 SKILL.md（CLI 请求级；未知/重复令牌 fail closed） |

`sshMcpEnabled` 配置字段名保留（存量配置兼容），语义变为「启用 SSH 工具（CLI + Skill）」；项目新增可选 `sshCliToken`，旧项目在下次保存「关联 SSH」时生成并持久化。

## 设计

### 1. `mt-ssh-cli` 二进制（agent 可见面）

位置：`src-tauri/mt-sidecars/src/bin/mt-ssh-cli.rs`，新 `[[bin]]`。参数解析用 clap v4 derive（sidecar 独立 crate，不影响主程序编译；clap 的用法错误信息对 agent 可读性好，`--help` 即文档）。

#### 子命令

```
mt-ssh-cli list --project-token <TOKEN> [--json]
mt-ssh-cli exec --project-token <TOKEN> [--cwd <DIR>] [--timeout <SECS>] <CONNECTION> [--] <COMMAND>...
mt-ssh-cli upload   --project-token <TOKEN> [--timeout <SECS>] <CONNECTION> <LOCAL_PATH> <REMOTE_PATH>
mt-ssh-cli download --project-token <TOKEN> [--timeout <SECS>] <CONNECTION> <REMOTE_PATH> <LOCAL_PATH>

# 运维子命令（排障用，不进 SKILL.md）：
mt-ssh-cli daemon           # 前台跑 daemon（正常由 CLI 自动 detached 拉起，无需手动）
mt-ssh-cli daemon-status    # 打印 daemon 版本 / 协议 / pid / 池内 session 数 / 在途请求数，以及日志路径
mt-ssh-cli daemon-stop      # 请求 daemon 优雅退出（停止接新连接 → 等在途请求，上限 60s → drain 池 → 退出）
```

`daemon-status` 退出码：0 = 在跑、1 = 不在跑（或端点忙——daemon 正在优雅退出）；`daemon-status` / `daemon-stop` 遇到被非当前用户的进程占着的端点一律拒绝通信、exit 2，遇到拒绝本进程连接的端点（`ERROR_ACCESS_DENIED`，0.4.11 起单列）同样 exit 2 并说明（见 §2 安全边界）。

以管理员身份（提权）运行时（0.4.11 起，判据是本进程令牌的 `TokenElevation`，判定不了按提权处理）：四个业务子命令不连也不拉起 daemon，直接进程内 one-shot，stderr 一行说明；`daemon` 子命令拒绝启动（exit 2 并说明，日志里记一行）；`daemon-status` / `daemon-stop` 照常工作——它们不带能力令牌、也不让 daemon 读写文件——`daemon-status` 另补一行说明这个提权 CLI 不用 daemon。原因见 §2 安全边界。Unix 不涉及。

- 四个业务子命令都强制要求 `--project-token`。令牌由 mini-term 后端生成 UUID v4、写入项目配置与 SKILL.md；CLI 不接受 project-id，也没有「缺省 = 全部连接」路径；
- `<CONNECTION>` 按 name 优先、id 兜底匹配（复用 `find_connection`，歧义/未找到语义不变；未找到时 CLI 提示用 `mt-ssh-cli list`，MCP 提示 `ssh_list_connections`）；
- exec 的 `<COMMAND>...` 为 trailing args，按空格 join 成远程命令（clap `trailing_var_arg` + `allow_hyphen_values`）；`--cwd` 沿用 `cd <dir> && ` 前缀拼接，目录按 POSIX shell 规则**按需**加引号（0.4.11 起）：简单路径原样（Windows OpenSSH 的 cmd.exe 远端照样认），带空格 / 特殊字符的优先双引号（POSIX 与 cmd 都认），含 `$`、反引号、`"`、`!`、换行或会被双引号转义的 `\` 时退回 POSIX 单引号；开头的 `~` / `~user` 连同第一个 `/` 留在引号外以保住 home 展开（`~"/x"` 在 POSIX 里不展开）；`-` 开头的相对路径补成 `./-x`。`--cwd` 默认远端是 POSIX shell，Windows 主机建议把切目录写进命令本身；
- exec 不转发 stdin：exec 请求发出后立即给 channel 发 EOF（0.4.11 起），读 stdin 的远端命令马上读到 EOF 而不是挂到超时；没有 TTY，交互式提示会失败；
- 超时默认：exec 60s、upload/download 300s（与 MCP 对齐），由 daemon 端强制执行（CLI 被杀不影响清理）。0.4.11 起时限覆盖**整个请求**：从请求进门算起，建连 / 握手 / 认证（池 acquire，含复用前等会话锁）、开 channel、跑命令（或传输）都在里面——此前 acquire 不受时限约束，假服务器只收 TCP 不说话就会无限挂住；
- upload/download 的 `<LOCAL_PATH>` 在 **CLI 端绝对化**后再发给 daemon——daemon 的 cwd 固定为 exe 所在目录、与调用方无关，相对路径必须在调用现场解析；
- **Git Bash（MSYS）改写实参**（0.4.11 起检测）：Git Bash 拉起原生程序前会把像 POSIX 路径的实参改写成 Windows 路径。实测（Git for Windows，`MSYSTEM=MINGW64`）：`/var/log/syslog` → `D:/Program Files/Git/var/log/syslog`、`/tmp/x.log` → `C:/Users/<用户>/AppData/Local/Temp/x.log`、`--cwd=/etc/nginx` → `--cwd=D:/Program Files/Git/etc/nginx`、`/` → `D:/Program Files/Git/`、`-I/x` → `-ID:/…`、`@/x` / `+/x` 同理、`/a:/b` → `D:\…;D:\…`、未加引号的 `~/x` 先被 bash 展开再改写；**单引号括成一个实参的整条命令**（不以 `/` 开头）、以 `//` 开头且后面还有 `/` 的路径原样到达，单独的 `//x` 变成 `/x`；单引号括起来但以 `/` 开头的整条命令照样被改写。Claude Code 在 Windows 上的 Bash 工具就是 Git Bash，此前 SKILL.md 教的 `-- <command...>`、`--cwd`、upload / download 的远端绝对路径都会被改坏。现在：
  - SKILL.md 教远端命令整条单引号放在 `--` 之后（命令本身以绝对路径开头时把开头写成 `//`），`--cwd` 与 upload / download 的远端绝对路径写成 `//var/log/x`（远端 POSIX 下等同 `/var/log/x`），`~` 路径单引号；本地路径照常——MSYS 改出来的正是它在这台 Windows 上对应的本地路径；
  - CLI 在 `MSYSTEM` 非空、且转换没被关掉（`MSYS_NO_PATHCONV` 不在环境里——Git for Windows 只看在不在，实测空值、`0` 都生效——且 `MSYS2_ARG_CONV_EXCL` 不是 `*`）时检查三类实参：`--cwd` 的值、upload / download 的远端路径、exec `--` 之后的每个实参。看起来被改写过——以 `X:/` 或 `X:\` 开头，或 `-<字母>` / `@` / `+` 之后、任一 `=` 之后紧跟 `X:/`——就按用法错误 exit 2，说明该怎么写，什么都不执行（改写过的命令已无从还原）。本地路径不查；
  - 误报的出路：给 Windows 远端传盘符路径时，exec 把整条命令单引号括成一个实参（它不以盘符开头，不拦），upload / download 给这次调用加 `MSYS_NO_PATHCONV=1`。WSL 与 PowerShell 不改写实参、也不设 `MSYSTEM`，不受影响；
- Windows 上以单个 `/` 开头的本地路径（POSIX 绝对路径，`//server/share` 不算）单独处理：CLI 经 WSL interop 被调用时仍是 Windows 进程，直接绝对化会把当前目录在 `/mnt/d/…` 时的 `/tmp/a.log` 解析成 `D:\tmp\a.log`。当前目录是 WSL 发行版自己的文件系统（interop 给的 cwd 形如 `\\wsl.localhost\<发行版>\…` / `\\wsl$\<发行版>\…`）时映射进该发行版根目录（`\\wsl.localhost\<发行版>\tmp\a.log`）；否则按用法错误 exit 2，提示在 WSL 里用 `"$(wslpath -w '<path>')"` 传 Windows 路径。Git Bash / MSYS2 会在拉起原生程序前把 `/c/…`、`/tmp/…` 实参换成 Windows 路径，不受影响（关掉转换的 `MSYS_NO_PATHCONV=1` 下才会走到报错分支）。

#### 输出与退出码契约

| 场景 | stdout | stderr | 退出码 |
|------|--------|--------|--------|
| exec 正常 | 远程 stdout 原样字节流（**实时流式**） | 远程 stderr 原样 | 透传远程退出码 |
| exec 远端以信号终止（0.4.11 起） | 已收到的输出 | 远程 stderr + 一行 `mt-ssh-cli: remote command terminated by signal SIGKILL[ (core dumped)][: <服务端说明>]; exit code 137` | `128 + 信号号`（HUP 1、INT 2、QUIT 3、ILL 4、ABRT 6、FPE 8、KILL 9、USR1 10、SEGV 11、USR2 12、PIPE 13、ALRM 14、TERM 15；认不出的信号 255） |
| exec 远端既没报退出码也没报信号 | 已收到的输出 | `mt-ssh-cli: error: remote exit status unavailable` | 2 |
| exec 超时（命令已发出） | 已收到的部分输出 | `mt-ssh-cli: error: timed out after <N>s` | 124（对齐 GNU timeout） |
| exec 连接阶段超时（0.4.11 起：建连 / 认证 / 开 channel 时就到点，命令没有启动） | — | `mt-ssh-cli: error: timed out after <N>s while connecting to '<连接>'; the remote command was not started` | 124 |
| 服务端拒绝 exec 请求（channel Failure，0.4.11 起立即返回） | — | `mt-ssh-cli: error: the SSH server rejected the exec request; ...` | 2 |
| 连接/认证/传输失败、用法错误、daemon 不可达、令牌 / 配置错误 | — | `mt-ssh-cli: error: <原因>`（绝不含密码） | 2 |
| list | 表格文本；`--json` 输出与 MCP `SshConnectionView` 同构的 JSON | — | 0 |
| upload/download 成功 | 一行 `uploaded/downloaded <N> bytes: <local> ↔ <remote>` | — | 0 |

- **不做 100KB 封顶**：输出经 daemon 流式转发、CLI 实时打印，Claude/Codex 的 Bash 结果自身有截断；SKILL.md 教 agent 大输出重定向到文件；
- 远程命令自身返回 124 时与超时无法区分——接受该歧义（stderr 有无 `mt-ssh-cli: error:` 前缀可辅助判断）；同理远程命令自己 `exit 137` 与被 KILL 无法从退出码区分，信号说明行不带 `error:`（那是远端的结局，不是 CLI 错误）；
- 信号映射在 service 层完成（exit-status 与 exit-signal 都报时以 exit-status 为准），说明行由各 transport 打：daemon 在终帧前补一个 stderr 帧（走现有帧，**不改协议**），one-shot 由 CLI 打，MCP 的 `exitCode` 同样是 128+n、`stderr` 末尾补 `[mt-ssh-mcp] ...` 一行；MCP 遇连接阶段超时按 `internal_error` 报（命令没跑，不伪装成「跑了但超时」）；
- CLI 自身错误统一 exit 2 + 前缀，包含 clap 用法错误（先输出 `mt-ssh-cli: error:` 再输出 clap 详情）；`--help` / `--version` 保持原样 exit 0。

### 2. 守护进程架构（CLI ↔ daemon）

对标 MCP sidecar 的常驻池收益，但把「每个 agent 会话一个进程」收敛成「全机一个 daemon」。

#### 进程模型

- **全局单例**，不按 project 拆分：池按 `connection_id` 缓存 session，与 project 无关；CLI 的 project 范围是**请求级能力令牌**——daemon 每次重读 config.json，把 `projectToken` 精确解析到唯一且已启用的项目后再按 `sshConnectionIds` 过滤。令牌缺失、为空或纯空白、未知、在任意项目间重复、命中未启用项目、解析出的范围为空，或配置不可读时都返回错误，绝不回退到全部连接；存量 MCP 继续走原 project-id 语义（读不到就是空，语义不变）；
- 令牌路径的报错分类（0.4.11 起，`mt_core::TokenScopeError`，此前一律 `invalid project token`）：缺令牌（先于读盘判）；数据目录定位不了；`config.json` 不存在（给出路径，提示 mini-term 没在这个账户下运行过，或本进程的 APPDATA 与 mini-term 不同）；读不了（io 错误类别 + 系统文案）；解析不了（serde 错误类别 + 行列号——serde 的 Display 会带出错的值原文，可能是一段密文，所以不用它）；没有项目持有令牌（SKILL.md 过期，请在 mini-term 里重新保存「关联 SSH」；或 SKILL.md 来自 dev 实例——sidecar 只读装机版数据目录）；令牌被多个项目持有；项目已停用；范围里没有仍存在的连接。**报错里绝不回显配置内容，也不回显完整令牌**（只露前 8 位）；
- daemon 就是 `mt-ssh-cli --daemon`（同一二进制），持有一个 `SshPool`，复用 MCP 侧全套编排：acquire → exec → transport 错精确淘汰（`evict_if_same`，0.4.11 起，不误杀并发请求刚建好的新 session）+ 单次 retry → 失败 mark_unhealthy 30s cooldown → 审计。0.4.11 起的 exec 编排细节：
  - **会话锁只在开 channel 时持有**：拿锁 → `channel_open_session` → 立即放锁（与 mt-ssh `run_bounded_exec_on_session` 同口径）。daemon 是全机单例，此前锁一直持到命令结束，同一连接上所有 agent 的 exec 全局串行，排队时间还不算超时；
  - **开 channel 短超时** `min(10s, 剩余时限)`（只计 `channel_open` 本身，不计等锁——锁被占说明 session 在用，不说明它死了）：到点说明这条 session 多半已死（网络切换后 TCP 半开等）→ `evict_if_same` → 在剩余时限内重新 acquire → 重试一次；新连接上仍开不出 channel → 报错（exit 2）。与「命令运行超时」区分：后者照旧第一次不淘汰——单个 channel 超时不代表 session 死了；
  - **判死的 session 不再让排队的请求各耗一次超时**（0.4.11 评审修复）：短超时到点时**持着会话锁**先 `mark_unhealthy`、再 `evict_if_same`，然后才放锁。排在这把锁上的请求拿到锁先查 `is_unhealthy_now()`，已标记就不开 channel、返回 `SessionRetired`，编排层重新 acquire，**不计入**那一次重试；排在池 acquire 存活检查（同一把锁）上的请求放锁后发现它已不在池里，直接换新的——池不会再把判死的 session 交出去。此前 8 个并发 exec 落在同一条死 session 上，2 个约 10 秒完成、6 个约 20 秒（评审实测），现在都在约一次开 channel 超时后完成。防死循环：acquire 交出的带标记 session 只会是冷却标记（下一条，留在池里挡新请求），重新 acquire 拿回的若仍是同一条（`Arc::ptr_eq`）且带标记，在 acquire 之后的冷却判定处按冷却报错；
  - 建连这一步封装成 `ssh_service::acquire_within`，失败分类 `AcquireFailure::{DeadlineExpired, Cancelled, Failed}`：exec 把 `DeadlineExpired` 映射成连接阶段超时（124），`Failed` 原文透传（2）；transfer 三类都是错误，沿用 transfer 既有的超时口径。池内建连另有自己的整段预算（`PoolConfig::connect_timeout`，默认 20s，覆盖 DNS / TCP / 握手 / 认证，见 mt-ssh `transport`），到点由池返回连接错误、报错写明卡在哪个阶段，按 `Failed` 原文透传（exit 2）——这种超时调大 `--timeout` 也没用，所以刻意不并入 124；只有 `--timeout` 比它短、在建连途中先到点才是 124；
  - 冷却文案：冷却由「同一请求在新 session 上重试仍 transport 失败」触发，与认证无关（认证失败由池自己冷却），提示调用方约 30 秒后重试。

#### IPC 传输与协议

- Windows：named pipe `\\.\pipe\mini-term.ssh-cli.<用户 SID（清洗后原文，非哈希——哈希在 Rust 版本间不保证稳定，会让升级前后的 CLI/daemon 算出不同 pipe 名、版本握手失效）>`，pipe security descriptor 限**仅当前用户**可连：SDDL `O:<SID>D:P(A;;GA;;;<SID>)`——owner 自 0.4.11 起显式写成当前用户（客户端要核验管道 owner，不能随建管道进程令牌的默认 owner 漂移：提权令牌的默认 owner 通常是 Administrators 组）；
- macOS/Linux：Unix domain socket（`$XDG_RUNTIME_DIR` 优先，回退 config.json 同目录），权限 0600；stale socket 恢复由 advisory lock 串行化，且删除前两次核对 `dev+ino`。chmod 失败会关闭 listener、删除 socket 并返回 Runtime；
- WSL 关键路径：CLI 在 WSL 里经 interop 执行时**仍是 Windows 进程**，连的是 Windows 侧 named pipe——daemon、池、config.json 全在 Windows 侧，天然一致；
- **客户端核验服务端身份**（0.4.11 起）：上面的 ACL / 0600 只挡得住「别的用户连我的 daemon」，挡不住「别的用户抢先占住端点冒充 daemon」——管道名全局可见、用户 SID 也不是秘密，冒充者能看到 agent 的命令与能力令牌、伪造输出和退出码（tokio 客户端默认 `SECURITY_IDENTIFICATION`，令牌冒充提权不成立，但读请求、伪造响应成立）。所以 CLI 连上后、收发任何数据之前先核验：Windows 走 `GetNamedPipeServerProcessId` → `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` → `OpenProcessToken(TOKEN_QUERY)` → `GetTokenInformation(TokenUser)` → 与本进程用户 SID `EqualSid`，**并且**（0.4.11 评审修复）在客户端句柄上 `GetSecurityInfo(SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION)` 取管道对象的 owner，也必须 `EqualSid` 当前用户——前一项以「创建管道实例的 PID」为锚，建管道的进程退出、实例句柄还活在别处时 PID 可能被回收给当前用户的某个进程而误判可信，owner 是管道对象自己的属性，别的账户建的管道 owner 改不成我们的 SID（那要 `SeRestorePrivilege`）；Unix 取 `UnixStream::peer_cred()` 的 uid 与 `getuid()` 比。任何一步失败或不相等都判「不可信」（Windows 核验失败先断开重连一次，排除 daemon 恰在退出的竞态）。不可信与「连不上」是两类错误：前者 CLI 不再自拉起 daemon（端点被占，拉起也绑不上），直接降级 one-shot 并在 stderr 提示端点被非当前用户的进程占用。第三类（0.4.11 评审修复）：打开端点就被拒（Windows `ERROR_ACCESS_DENIED`、Unix `EACCES`——别的账户建的管道 / socket，或安全描述符不放行本进程的权限级别、沙箱令牌）同样是「端点被占、无法与之通信」，不拉起、立即降级并写明原因（此前归为「连不上」，每次都白拉起一个注定绑不上的 daemon，再等约 3 秒重连窗口才降级）；daemon 绑定失败后的探测遇到它也直接报 Runtime。会被判为不可信的已知情形：旧版留下的、以管理员身份（提权）跑着的 daemon——未提权进程打不开它的 token，它建的管道 owner 通常也是 Administrators 组（提权令牌的默认 owner；本机未在提权环境实测）；0.4.11 起 daemon 不再提权运行（见安全边界），遇到旧的，代价只是降级（`daemon-stop` 同样拒绝与它通信，它空闲 10 分钟自退，或在任务管理器里结束）；
- 协议：newline-delimited JSON 帧（serde camelCase，`v` 字段为 IPC 协议版本，当前为 2）。
  - 连接建立后 daemon 先发 `{type:"hello", version:"<CARGO_PKG_VERSION>", protocolVersion:2, pid}`；`protocolVersion` 对反序列化保持可选，缺失时按 legacy v1 处理；
  - 请求（CLI→daemon，单行）：`{v:2, op:"list|exec|upload|download|status|shutdown", projectToken?, connection?, command?, cwd?, timeoutSecs?, localPath?, remotePath?}`；
  - 响应流（daemon→CLI，多行直至终帧）：
    - `{type:"stdout", dataB64}` / `{type:"stderr", dataB64}`（exec 输出分片，base64 保二进制安全）；
    - 终帧 `{type:"result", exitCode?, timedOut?, notStarted?, bytes?, connections?, sessions?, activeRequests?}`（按 op 携带对应字段；`activeRequests` 是 0.4.11 起 status 附带的「除这条 status 外仍在处理的连接数」；`notStarted` 是 0.4.11 起 exec 连接阶段超时时才出现的 `true`——新 CLI 据此改报「连接阶段超时、命令没启动」，普通结果与运行中超时的线上形状不变。两者都是加字段不升协议版本——旧 CLI 解码时忽略不认识的字段）；
    - 远端信号的说明行作为终帧前的一个普通 stderr 帧送达（不新增帧类型）；
    - 终帧 `{type:"error", message}`（message 绝不含密码，与 MCP 同一纪律）。

#### 生命周期

- **自拉起**：CLI 连 pipe 失败 → spawn 自身 `daemon` 子命令（detached：Windows `DETACHED_PROCESS`+`CREATE_NEW_PROCESS_GROUP`，Unix `process_group(0)`——与双 fork/setsid 等效且免 unsafe）→ 带退避重试连接（总窗口 ~3s）。端点在但所有管道实例都忙（`ERROR_PIPE_BUSY`：高并发瞬间，或 daemon 正在优雅退出）时不拉起、只退避重连，拉起的新 daemon 反正绑不上；端点不可信或拒绝本进程连接（`ERROR_ACCESS_DENIED`）时也不拉起、直接降级；以管理员身份运行的 CLI 根本不连、不拉起（见 §1 末段与安全边界）。拉起的 daemon 与调用方彻底脱钩（0.4.11 起，实现在 `daemon_spawn`）：
  - **不继承调用方的标准句柄**：std 的 `Command::spawn` 在 Windows 上以 `bInheritHandles=TRUE` 调 `CreateProcessW` 且不带句柄白名单，子进程继承本进程所有可继承句柄——调用方交给 CLI 的 stdout/stderr 管道写端正是可继承的，被常驻 daemon 拿走后，CLI 早已退出，`$(cli …)` / `cli … | tail` 却要等 daemon 空闲自退（10 分钟）才见 EOF，WSL interop 下调用本身不返回（实测）。`CommandExt::inherit_handles(false)` / `raw_attribute` 仍是 unstable，所以拉起前用 `SetHandleInformation` 把 CLI 自己的三个标准句柄设成不可继承（`GetStdHandle` 返回 NULL / `INVALID_HANDLE_VALUE` 时跳过）。Unix 不需要：std 在子进程里把 NUL / 日志文件 dup2 到 0/1/2，Rust 自己开的 fd 一律 `O_CLOEXEC`。两个平台共同的残留边界：调用方额外塞进来的非标准可继承句柄 / 没设 CLOEXEC 的 fd 3+ 仍会跟进 daemon；
  - **工作目录 = exe 所在目录**（取不到用系统临时目录）：否则 daemon 继承调用方 cwd（通常是项目目录），Windows 上整段生命周期锁着它，改名报 EBUSY（实测）；
  - **stdout/stderr 接日志文件** `mt-ssh-cli-daemon.log`（见下「daemon 日志」），定位不到数据目录就接 NUL；
- **并发竞态**：多个 CLI 同时拉 daemon → Windows 首 pipe 实例或 Unix endpoint recovery lock 收敛；只有探测到**当前用户**的活 daemon（身份核验通过且发了 hello）才归类 `AlreadyRunning` 并静默让位；非法路径、权限、其它 bind 错误，以及端点被别的用户的进程占着或拒绝本进程连接（后者不再耗满 1 秒探测窗口），一律 Runtime；
- **空闲自退**：无活跃请求且 10 分钟无新连接 → drain 池（逐 session disconnect ByApplication，复用 `pool.shutdown()`）→ 退出。孤儿 daemon 最多存活一个空闲周期；
- **版本握手**（0.4.11 起）：CLI 读 hello 帧的包版本 `version`（semver 数值比较）与 IPC 版本 `protocolVersion`，按下表处置；缺失 `protocolVersion` 的 legacy v1 hello 按 v1 认，版本串解析不了按「更旧」处理。只换代**更旧**的 daemon，是因为 dev 实例与装机版的 sidecar 版本不同、共用同一个端点，旧规则「版本不等就踢」会让两边来回互踢、互相切断在途请求。

  | daemon 相对 CLI | 协议一致 | 协议不同 |
  |---|---|---|
  | 更旧 | 按对端协议发 `shutdown` 换代 → 等端点释放 → 拉起新版（最多两轮） | 同左 |
  | 相同或更新 | 直接使用 | 不踢，本次降级 one-shot，stderr 说明原因 |

  dev 迭代：`daemon-stop` 可手动踢掉占着二进制文件锁的 daemon（Windows 下运行中的 exe 无法被 stage-sidecars 覆盖，现有「跳过被占用文件」的容忍逻辑继续兜底）；同版本而协议不同（只会是 dev 构建）也只能这样手动换；
- **优雅退出**（0.4.11 起）：daemon 收到 `shutdown`（`daemon-stop` 或版本换代）→ 先回 ack → **立刻停止接受新连接**（Windows 丢掉监听实例，新客户端拿到 `ERROR_PIPE_BUSY` / 找不到管道；Unix 关 listener、删 socket 并放掉 recovery lock，新 daemon 可立即在同一路径起来）→ 等在途请求做完，上限 60s（= exec 默认超时：默认参数的 exec 不会被换代硬切；upload/download 默认 300s、显式加大 `--timeout` 的 exec 到点照样被切断，那条 CLI 得到 "connection to daemon lost mid-request"——换代不能被一个长传输拖住几分钟）→ drain 池 → 退出。等待期间新来的 CLI 连不上，退避 ~3s 后降级 one-shot。⚠️ **0.4.10 及更早的 daemon 收到 shutdown 仍是立即退出、硬切在途请求**——已发出去的旧二进制改不了，新 CLI 给它们换代那一刻的在途请求照旧会断；反过来旧 CLI（0.4.10）遇到新 daemon 仍会按旧规则发 shutdown，新 daemon 按上面的流程优雅退出；
- **CLI 中途被杀**（agent Bash 超时 / Ctrl+C）：daemon 用状态型取消令牌通知 service，并等待显式 `channel.close().await` 完成；请求超时也在同一 channel 状态机内先 close 再返回 124。不能依赖 russh `Channel` Drop（Drop 不发送 Close）。0.4.11 起取消令牌在编排的每一步都被响应——建连 / 认证（acquire 的 future 直接丢弃）、等会话锁、开 channel、收流；这次等待仍以 10s 为限作为纵深兜底：到点放弃这次编排（丢弃 future）并记日志，保证连接计数归零、daemon 能空闲自退；
- **daemon 日志**（0.4.11 起）：CLI 拉起 daemon 时把它的 stdout/stderr 追加写到数据目录（`mt_core::config_json_path()` 的父目录，与审计日志同一锚点，不存在就创建）下的 `mt-ssh-cli-daemon.log`；拉起前超过 1 MiB 就轮转成 `.log.1`（只留一代）。「看大小 → 改名 → 打开」持 `mt-ssh-cli-daemon.log.lock` 上的系统文件锁（Windows `LockFileEx`、Unix `flock`，std `File::try_lock`；锁文件常驻，进程崩溃时系统自动放锁，没有陈旧锁）串行执行，最多等 2 秒，等不到就这次不轮转、只追加——此前并发冷启动的几个 CLI 各自轮转，后一个会把前一个刚建好、前一个 daemon 已开着的新日志再改名成 `.1`，冲掉真正的上一代、那个 daemon 也跟着写进 `.1`（评审修复）。daemon 自己的生命周期事件（启动 / 版本 / 端点、空闲退出、shutdown、客户端断开、取消兜底超时）带 UTC 毫秒时间戳，每行一次写出（并发冷启动的几个 daemon 共写一份日志，行不交错）；会话池等下层的 `eprintln!`（如 `[mt-ssh] host key rejected: …`、重试日志）不带时间戳，夹在事件行之间。日志句柄只交给 daemon，不经过调用方的标准句柄。`daemon-status` 打印日志路径（daemon 不在时也打印）。

#### 安全边界

- IPC 仅限当前用户可连；信任边界与现状**等价**——本机同用户进程本来就能直接 spawn `mt-ssh-mcp` 或读 config.json，daemon 不新增暴露面；
- **daemon 不以管理员身份（提权）运行**（0.4.11 评审修复，存量问题）：管道 ACL 只认用户 SID、不带完整性标签，提权令牌与同一用户普通权限令牌的 `TokenUser` 是同一个 SID。此前用户以管理员身份跑终端时 CLI 会拉起提权的 daemon，之后同用户任何普通权限进程都能直连管道，带上 SKILL.md 里的令牌（或改写用户自己可写的 config.json 造一个），让它以管理员身份往 `C:\Program Files` 写文件、读只有管理员能读的文件——等于绕过 UAC，上一条的「等价」不再成立。现在提权的 CLI 不连也不拉起 daemon、直接进程内 one-shot，`daemon` 子命令提权时拒绝启动（见 §1），daemon 因而总是普通权限，「同用户进程能用它」重新与「同用户进程本来就能直接跑 CLI」等价。判据取 `TokenElevation`（`TokenIsElevated`），判定不了按提权处理：关了 UAC 的管理员账户、内置 Administrator 也落在「提权」里，这些环境下 CLI 一律 one-shot——只损失池复用，换来不必推敲各种 UAC 配置下还有没有更低权限的同用户进程。旧版留下的提权 daemon 见上「客户端核验服务端身份」（判不可信、空闲自退）；
- 反方向，CLI 只和**当前用户**的 daemon 说话（见上「客户端核验服务端身份」）：别的用户抢注同名管道 / 在 /tmp 放同名 socket 时，CLI 不发请求、不自拉起、降级 one-shot；`daemon-status` / `daemon-stop` 拒绝通信并 exit 2；daemon 绑定失败后的活体探测同样核验，端点被别的用户占着时报 Runtime，不静默让位；
- CLI 的项目授权由随机能力令牌决定，agent 可见自己的 token，但不能通过省略参数或伪造 project-id 扩大范围；
- `is_blocked_local_path`（数据目录传输硬拒绝，0.4.10 起整棵目录树）在 **daemon 端**执行，单点强制，CLI 无法绕过。0.4.11 起比较前先把指向本机磁盘的 Windows 别名改写回盘符路径：`\\?\C:\…`、`\\.\C:\…`、本机管理共享 `\\localhost\C$\…` / `\\127.0.0.1\c$\…` / `\\<本机名>\C$\…`（含 `\\?\UNC\…` 写法）、WSL 的 `\\wsl.localhost\<发行版>\mnt\c\…` / `\\wsl$\…\mnt\c\…`（canonicalize 之后仍是 9P 的 UNC 路径，此前拦不住）；纵深再加一道：路径任一分量（规范化前后都查，大小写不敏感，忽略 NTFS 流后缀与结尾的点 / 空格）就是装机版数据目录名 `com.mini-term.app` 一律拦——改写规则覆盖不到的别名（非管理共享、改过 automount 根的 WSL）也跑不掉。dev 数据目录（`MT_APP_DATA_DIR`）名字可能很普通，只按前缀拦；
- **dev 数据目录的护栏取决于「谁拉起了全局 daemon」**（评审记录，按威胁模型接受，不改代码）：daemon 是全机单例，护栏读的 `MT_APP_DATA_DIR` 是 **daemon 自己**的环境（拉起它的那次 CLI 调用传下来的），不是发请求的 CLI 的。dev 实例终端里的 agent 借装机版令牌经 daemon 上传 / 下载时，dev 目录能不能被拦住（只按前缀拦），取决于这个 daemon 是不是从带着同一个 `MT_APP_DATA_DIR` 的终端里拉起的——装机版终端拉起的 daemon 不知道 dev 目录在哪。接受：不防同用户的本机进程（它们本来就能直接读写 dev 目录）；装机版数据目录不受影响（按 `config_json_path()` 与目录名拦，与环境无关）；
- 审计日志由 daemon 单点追加（沿用 `ssh-mcp-audit.log` 文件与行格式），消除并发写交错。0.4.11 起 exec 审计行只记**远程命令可能已经开始执行**的请求，`exit=` 取 `<退出码>`（信号终止时是 128+n）/ `timeout` / `cancelled`（跑到一半调用方断开，此前不记）/ `unknown`（远端没报退出状态，此前误记成 `timeout`）；建连阶段失败、超时或取消、冷却中、服务端拒绝、传输层失败（含重试仍失败，此前误记成 `exit=timeout`）都不记——命令没离开本机或没被远端接受，记一行只会误导。transfer 的审计口径不变，只是连接阶段超时与 acquire 失败一样不记；
- 审计脱敏（`redact_command`，0.4.11 扩展）：`--password` / `--passwd` / `--pass` / `--passphrase` / `--token` / `--api-key` / `--secret` / `--client-secret` 等（以 `-password` / `-secret` / `-token` / `-api-key` 结尾的长选项同样算，`--opt 值` 与 `--opt=值` 两种写法）、keytool 的 `-storepass` 一类、openssl 的 `pass:<口令>`、`sshpass -p`（只认 sshpass 自己的选项区，`ssh -p 22` 不受影响）、变量赋值 / `key=value`（键含 PASSWORD / PASSWD / SECRET / TOKEN / API_KEY / APIKEY / PRIVATE_KEY，或 `*_PWD`、`SSHPASS`、`REDISCLI_AUTH`）、HTTP 头 `Authorization` / `Proxy-Authorization`（`Bearer` / `Basic` / `token` 等方案名之后的凭据）与 `X-Api-Key` 一类。命令按空白切段、不解析引号，带引号的值跨多段时一并打码到收尾引号。「宁漏勿误伤」不变：`ps -p 123`、`ssh -p 22`、`grep token file`、`--password-stdin`、`--no-password` 等原样。

#### 降级路径

daemon 拉不起来或 IPC 异常（极端环境）→ CLI 自动 fallback 为进程内 one-shot 执行（`SshPool::new()` → acquire → 执行 → `shutdown()`），stderr 记一行降级提示。业务逻辑走同一 service 层（见 §3），fallback 近零成本，保证工具永远可用。0.4.11 起另有五种降级，提示各自说明原因：端点被非当前用户的进程占着；端点拒绝本进程连接（`ERROR_ACCESS_DENIED`，不拉起、立即降级）；正在跑的 daemon 比 CLI 新且协议不同（不踢它）；端点一直忙（daemon 正在优雅退出）；本进程以管理员身份运行（根本不碰 daemon）。请求一旦被 daemon 受理就不再降级——中途断连按 CLI 错误收尾（exec 可能有副作用，不能盲目重跑）。

### 3. 共享 service 层（`mt-sidecars` 加 lib target）

daemon 化后抽取层次升级：不只抽纯函数，而是把**完整业务编排**抽成 service 层，MCP handler 与 daemon handler 都退化成薄传输层。

`mt-sidecars/src/lib.rs` + `src/ssh_service.rs`：

- `ConnectionScope::{LegacyProject, Capability}` 明确区分存量 MCP 与 CLI 授权入口；`list_connections(scope) -> Result<Vec<SshConnectionView>, ServiceError>`；
- `exec(pool, ExecRequest, cancellation, on_output) -> ExecOutcome`——含范围解析、find_connection、acquire、evict+retry、cooldown、审计与 channel 内超时/取消；`on_output: impl FnMut(StreamKind, &[u8])` 流式回调：daemon 侧写 IPC 帧实时转发，MCP 侧收集进 Vec 再 cap_output 打包 JSON；
- `transfer(pool, direction, ...) -> u64`——含护栏 + retry + 审计（迁自 `run_transfer`）；
- 纯函数与既有单测随迁：`find_connection` / `build_remote_command` / `cap_output` / audit 系列 / `is_blocked_local_path*` / `run_exec_on_session`（改造为回调式）/ `TransferDirection`。

#### 会话池与 SFTP 原语（`mt-ssh` / `mt-secret`，0.4.11 同批；主程序的远程项目同样受益）

- **建连对齐终端里的系统 ssh**：读 `~/.ssh/config`（`HostName` / `Port` / `User` / `IdentityFile` / `IdentitiesOnly` / `HostKeyAlias` / `Include`，以及含多跳的 `ProxyJump`）；连接自身的端口（非 22 时）、用户、私钥优先，与终端拼的 `ssh [-p port] [-i key] user@host` 等价。`ProxyCommand`（以及跳板自己又配了代理）不支持：回落为不经代理的直连（等同旧行为），连接失败的报错与日志里附一句「已忽略 ~/.ssh/config 里的 ProxyCommand」；`Match` 块整块跳过。配置文件读不了（IO 错误，不是不存在）时沿用该连接上次成功解析的路线；写坏时只拿上次的路线复用健康会话，新建连接报 Config 错误。Unix 上组或其他用户可写的配置文件按 OpenSSH 口径忽略（只查权限位）；
- **认证顺序**：连接私钥 → ssh-agent 里其余的 key（Windows：`SSH_AUTH_SOCK` 为命名管道时用它，再试 `\\.\pipe\openssh-ssh-agent`，最后 Pageant）→ 默认私钥 `id_ed25519` / `id_ecdsa` / `id_rsa`（没显式指定私钥时）→ 密码 → keyboard-interactive。私钥读不了（不存在 / 格式不支持 / 带口令）只记原因、继续试别的；带口令的私钥只能经 agent 使用；公钥尝试有上限（配了密码 4 把、没配 5 把），保证密码在 sshd 默认 `MaxAuthTries 6` 之内轮得到；加固服务器（MaxAuthTries 更小）在密码轮到之前就断开时，同一建连预算内重连一次、只试密码与 keyboard-interactive。`IdentityFile` 指向 `.pub` 时到 agent 里找同一把 key 签名（1Password 等的写法）。跳板只用 agent / 默认私钥 / 它自己的 `IdentityFile`（密码属于目标连接），跳板认证失败的报错以 `authentication failed for jump host …` 开头。失败时逐方法列出结果（英文前缀、不含密码）；认证途中断网按网络错误报（`connection lost during authentication …`），不计冷却；
- **建连限时**：DNS / TCP / 握手 / 认证整段 20s（`PoolConfig::connect_timeout`，逐地址 5s），超时写明卡在哪个阶段；建连途中被丢弃（请求时限、取消）时掐断传输，不留 russh 任务与 socket。退出码口径见 §2「进程模型」：池内超时 = 连接错误 exit 2；
- **认证失败冷却**：按连接身份冷却 30s，期间 acquire 直接返回 `authentication cooldown: …`、不碰网络；改了连接配置即换了身份，可立即重试。网络错误、超时、主机密钥问题不计入。同一连接的建连单飞（按 `conn.id`）：并发请求排队等同一次建连，拿到闸后重查池与冷却，排队期间同一身份建连失败的直接共享那次的错误——冷却不会被一波并发绕过；
- **兼容算法**：在 russh 默认表之后追加 `ecdh-sha2-nistp256/384/521`、`aes128-gcm@openssh.com`、`hmac-sha1-etm@openssh.com` / `hmac-sha1`（强算法仍优先）；
- **known_hosts**：支持 `*` / `?` 通配、`!` 否定、`@revoked`（命中即拒、不追加）与哈希条目；拒绝原因带英文前缀（`host key verification failed: …`、`known_hosts unreadable …`）；追加新条目前若文件末尾没有换行先补一个（与 OpenSSH 相同，免得粘坏上一行、让那台主机退回首次信任）；
- **SFTP 传输**：会话锁只在开 channel 时持有，传输期间持 SFTP lease，同一连接上的 exec、其它传输与 acquire 不排队；「等锁 + 开 channel + 请求子系统」整段 10s 上限（`PoolConfig::sftp_open_timeout`），超时或会话已被标记不健康时返回 transport 错误，由调用方淘汰重连；
  - download：约 1 MiB 窗口多请求并行（块大小按 `limits@openssh.com`，封顶 64 KiB；未通告时 32 KiB），短读从实际位置补读；先写本地 `<local>.<pid>-<seq>.mt-sftp-partial` 再 rename 替换，失败或超时删除临时文件、原有本地文件不动；远端是目录时报错；
  - upload：逐跳跟随软链，软链保持软链（悬空软链在它指向处新建；成环、指向目录或特殊文件时传输前报错）。跟随前按内核 `fs.protected_symlinks` 的规则把关：软链所在目录 sticky 且全局可写（如 `/tmp`）时，只跟随目录属主或登录用户自己的软链，否则报错——防止共享目录里预埋的软链把上传引去覆盖别人的文件；在最终文件所在目录写隐藏临时文件 `.<名>.mt-partial-…`，创建时权限位即与目标一致、属主尽量对齐；有 `posix-rename@openssh.com` 时原子替换，否则「目标 → 备份、临时 → 目标、删备份」带回滚；原文件不在原位时绝不删临时文件，留下的临时 / 备份路径写进错误信息。属主或权限位复刻不了、或目录里建不了临时文件时，像 scp 一样原地覆盖写（中途失败会留下不完整的文件，错误信息写明）；目标不可写时报错、文件不动。超时取消后在后台清理远端临时文件。替换与回滚一旦开始就在独立任务里跑完，外层超时或取消不会让目标停在被挪走的状态。sidecar 与 mt-remote 粘贴上传走带整体时限的入口 `run_sftp_upload_until`：池在时限前收尾并按阶段写明结局（没动过 / 原文件未动、临时文件后台删除 / 原地覆盖到一半——远端文件可能不完整），调用方外层只留 10s 兜底（`SftpTransferError::TimedOut`）；双斜杠开头的远端路径（Git Bash 下的 `//var/log/x`）全程原样保留；
- **密钥重载**：sidecar 懒加载的 `credential.key` 解不开信封时，若密钥文件已被主程序重建则重读并重试一次（主程序自己登记的密钥不重载），常驻 daemon 不再一直报 `password unavailable`。

`mt-ssh-mcp.rs` 瘦身成 rmcp 适配层，**过渡期继续构建发布**（存量项目 `.mcp.json` 仍指向它，两者共享同一审计日志与行为）。

### 4. SKILL.md 生成

daemon 对 skill 完全透明——SKILL.md 只描述 CLI 用法，不提 daemon。两份产物，路径：

- `<project>/.claude/skills/mini-term-ssh/SKILL.md`（Claude Code）
- `<project>/.codex/skills/mini-term-ssh/SKILL.md`（Codex）

生成策略与 `.mcp.json` 不同：SKILL.md 是 mini-term **独占生成物**（不是与用户共享的合并文件），直接整文件覆盖写入，文件头带 generated 注释标记。内容含 CLI 绝对路径与项目 capability token（机器相关 → 进 `.gitignore`）。

模板（Claude 版；Codex 版去掉 `allowed-tools` 整段，其余相同）：

```markdown
---
name: mini-term-ssh
description: Run commands or transfer files on remote servers over SSH using this project's saved connections (managed by mini-term). Use when the user asks to operate on a remote host — run commands, inspect logs, deploy services, upload or download files.
allowed-tools:
  - 'Bash(''<ABS_PATH>'' list --project-token ''<PROJECT_TOKEN>'' *)'
  - 'Bash(''<ABS_PATH>'' exec --project-token ''<PROJECT_TOKEN>'' *)'
  - 'Bash("$(wslpath ''<ABS_PATH>'')" list --project-token ''<PROJECT_TOKEN>'' *)'
  - 'Bash("$(wslpath ''<ABS_PATH>'')" exec --project-token ''<PROJECT_TOKEN>'' *)'
  - 'Bash(wslpath ''<ABS_PATH>'')'
  - 'PowerShell(& ''<ABS_PATH>'' list --project-token ''<PROJECT_TOKEN>'' *)'
  - 'PowerShell(& ''<ABS_PATH>'' exec --project-token ''<PROJECT_TOKEN>'' *)'
---

<!-- generated by mini-term (skill template v2); do not edit (regenerated on enable) -->

# mini-term SSH CLI

This project has saved SSH connections managed by mini-term. Authentication is
handled internally — NEVER ask the user for passwords and NEVER use plain
`ssh` / `scp` / `sftp`.

CLI binary (quote the path, it may contain spaces):

    "<ABS_PATH>"

All examples carry this project's capability token. Never replace or omit it.

## Bash / Git Bash

    '<ABS_PATH>' list --project-token '<PROJECT_TOKEN>' --json
    '<ABS_PATH>' exec --project-token '<PROJECT_TOKEN>' [--cwd //DIR] [--timeout SECS] <connection> -- '<command>'
    '<ABS_PATH>' exec --project-token '<PROJECT_TOKEN>' <connection> -- 'tail -n 50 /var/log/syslog'
    '<ABS_PATH>' upload   --project-token '<PROJECT_TOKEN>' <connection> <local_path> <remote_path>
    '<ABS_PATH>' download --project-token '<PROJECT_TOKEN>' <connection> <remote_path> <local_path>
    '<ABS_PATH>' download --project-token '<PROJECT_TOKEN>' <connection> //var/log/app.log ./app.log

（0.4.11 起：远端命令整条单引号成一个实参放在 `--` 之后；说明 Git Bash（MSYS）会把像 POSIX 路径的实参改写成 Windows 路径，所以命令本身以绝对路径开头时把开头写成 `//`，`--cwd` 与 upload / download 的远端绝对路径以 `//` 开头，`~` 路径单引号，本地路径照常；CLI 遇到疑似被改写的实参 exit 2。见 §1「Git Bash（MSYS）改写实参」。）

## WSL (Windows interop)

    "$(wslpath '<ABS_PATH>')" list --project-token '<PROJECT_TOKEN>' --json
    # exec/upload/download 使用同一前缀

`<ABS_PATH>` 内若含 `'`，POSIX 单引号按 `'"'"'` 转义后再传给 `wslpath`。

## PowerShell

PowerShell 的带引号可执行路径前必须使用调用运算符 `&`：

    & '<ABS_PATH>' list --project-token '<PROJECT_TOKEN>' --json
    & '<ABS_PATH>' exec --project-token '<PROJECT_TOKEN>' [--cwd DIR] [--timeout SECS] <connection> -- '<command>'
    & '<ABS_PATH>' upload   --project-token '<PROJECT_TOKEN>' <connection> <local_path> <remote_path>
    & '<ABS_PATH>' download --project-token '<PROJECT_TOKEN>' <connection> <remote_path> <local_path>

PowerShell 路径中的 `'` 按 `''` 转义。

（0.4.11 起正文另有一节 Behaviour 与退出码清单：远端命令单引号括起来，本地 shell 就不展开 `$VAR` / `~` / 通配、不改写路径（内部的 `'` 在 Bash 写 `'\''`、PowerShell 写 `''`）；没有 TTY / 没有 stdin（远端立即 EOF，交互式提示会失败，用 `sudo -n`、`-y`、`DEBIAN_FRONTEND=noninteractive` 等非交互参数）；`--cwd` 默认远端是 POSIX shell，Windows 主机把切目录写进命令本身；`--timeout` 覆盖整个调用含建连，建连本身另有 20s 上限（到点按连接错误 exit 2，调大 `--timeout` 无济于事）；退出码 = 远程退出码 / 124 超时（"while connecting" 表示命令没启动）/ 128+n 信号 / 2 CLI 或连接错误。WSL 一节说明 upload/download 的本地路径用相对路径或 `"$(wslpath -w '/abs/path')"`，当前目录在 `/mnt/*` 下时传 `/tmp/x` 这类 POSIX 绝对路径会被拒（exit 2）。upload/download 一节注明它们不预批、会弹审批。）

Do NOT base64-echo file contents through exec — use upload/download.
```

要点：

- frontmatter 仅 `name` / `description`（+ Claude 版 `allowed-tools`）——两端公共子集；`allowed-tools` 让 skill 激活期间免审批（授权随下一条用户消息失效，符合预期）；
- `allowed-tools` **只预批 `list` 与 `exec`**（0.4.11 起；此前 `Bash("<ABS_PATH>" *)` 放行整个 CLI，upload/download 能免审批读写任意本地文件，而且双引号写法与正文教的单引号写法根本对不上）。Claude Code 的 Bash / PowerShell 规则按命令原文匹配（引号算在内，`*` 匹配任意文本，末尾 ` *` 也匹配不带参数的形式），所以每条规则都与正文的调用形式逐字一致：Bash 的 `'<ABS_PATH>'`、WSL 的 `"$(wslpath '<ABS_PATH>')"`（外加命令替换里那条 `wslpath '<ABS_PATH>'`——Claude Code 会单独检查命令替换内部的命令）、PowerShell 的 `& '<ABS_PATH>'`，后面紧跟子命令与本项目令牌。写成 YAML 列表、每项单引号标量（内部 `'` 双写），避免「空格 / 逗号分隔的一行」被路径里的空格拆坏；路径或令牌含 `*`（会变成通配）或换行时整段不写，宁可每次弹审批。示例改成 `-- '<command>'`、远端路径 `//…` 之后规则不变——它们只管到 `exec --project-token '<令牌>'` 为止，后面由 ` *` 兜住；
- dev 实例（本实例写 `config.json` 投影的目录不是 sidecar 读的那个，通常是设了 `MT_APP_DATA_DIR`）：它生成的令牌永远验证不过，写 SKILL.md 只会覆盖装机版那份、停用时删的也是装机版的——启用 / 停用 / 启动校正都跳过注册器的一切文件改动（SKILL.md、`.gitignore`、`~/.codex/config.toml` 信任、MCP 迁移清理），关联照常存进本实例配置，界面提示补一句说明（`sshAssoc.devInstanceNote`）；
- 启动校正（0.4.11 起）：主程序启动后在后台把现存的、mini-term 生成的 SKILL.md（Claude / Codex 两份）对齐；缺失的不重建（可能是用户故意删的），用户自己写的同名文件不碰；只看开启了关联且带令牌的本地项目，远程 SSH 项目跳过；dev 实例整个跳过；每次重写打一行日志，任何失败都不影响启动。要不要重写按生成标记里的**模板版本号**分三档（评审修复：此前只比「内容是否与本机模板输出一致」，模板不同的两份安装会来回改写，模板相同时又一直钉在旧 CLI 上）：
  - 生成标记写成 `<!-- generated by mini-term (skill template vN); do not edit (regenerated on enable) -->`，`N` 是 mt-ai 里的常量 `SKILL_TEMPLATE_VERSION`（本批为 2），模板一改就加一；`(skill template vN)` 这段格式是跨版本契约，以后只许改数字。此前不带版本号的生成标记照样认得出（视同 v1）；
  - 文件里的版本**比本机新**：不动——由更新的 mini-term 管（它的模板本机渲染不出来），连引用的 CLI 都不看；
  - **比本机旧或没有版本号**：重写；
  - **相同**：引用的 CLI 不存在、或不是本机盘上的绝对路径（UNC 之类不去 stat——别人仓库里写一个 `\\主机\share\…`，一 stat 就会带着当前用户的凭据去连那台主机）、或令牌与配置不符 → 重写；引用的就是本机这份 → 内容与模板输出不一致才重写；引用同机另一份仍存在的 CLI → 只有本机这份的修改时间**严格更新**才改指本机，否则不动（任何一边读不到修改时间也不动）。两份安装各跑各的校正，只会朝较新的一方收敛；
  - 文件里引用的 CLI **只 stat、绝不执行**（不跑它的 `--version` 之类）：SKILL.md 可能随仓库来自他人，执行它等于任意代码执行；
- 正文英文（与现有 MCP tool descriptions 一致，对两端 agent 触发与遵循最稳）；
- description 是触发关键：明确列出「远程执行 / 查日志 / 部署 / 传文件」场景。

### 5. `ssh_skill_registry.rs`（新模块，替代 `ssh_mcp_registry` 的注册职责）

对标 `ssh_mcp_registry.rs` 的风格（校验 → 幂等写入 → 可读中文错误）：

**`enable_ssh_tools(project_dir, project_token?) -> { message, projectToken }`**：
1. 写 `.claude/skills/mini-term-ssh/SKILL.md`（含 `allowed-tools`）；
2. 写 `.codex/skills/mini-term-ssh/SKILL.md`；
3. `.gitignore` 幂等追加 `.claude/skills/mini-term-ssh/` 与 `.codex/skills/mini-term-ssh/`（存量 `.mcp.json` / `.codex/` 条目不清理，无害）;
4. `trust_project_in_codex` 保留（项目级 `.codex/` 内容大概率仍要求 trusted，写入无害且幂等）；
5. **迁移清理**：调 `remove_project_mcp_json` / `remove_project_codex_config` / `set_claude_mcp_approval(false)`（`ssh_mcp_registry` 相应函数改 `pub(crate)`），老项目切换时自动摘除 MCP 注册。

已有非空 token 稳定复用；旧项目没有 token 时后端生成 UUID v4 并返回，前端随后持久化到对应 `ProjectConfig.sshCliToken`。

**`disable_ssh_tools(project_dir)`**：删两份 SKILL.md 及空目录（`skills/` 目录若空一并删除，不留空壳）；顺带执行同样的 MCP 迁移清理（兜底存量）。Codex 信任与 `.gitignore` 沿用现行策略不回收。

（GPUI 版落在 `mt_ai::ssh_registry`。0.4.11 起 `enable` / `disable` 多一个参数 `projection_dir`——本实例写 `config.json` 投影的目录（mt-app 取 `ConfigStore::path()` 的父目录），与 sidecar 读的 `mt_core::config_json_path()` 父目录规范化后比较，不同即 dev 实例：只解析令牌 / 什么都不删，结果带 `registry_skipped`。另有 `reconcile_skill_files_on_startup`（启动校正，见 §4），mt-app 在窗口打开后丢 background executor 调一次。）

`lib.rs`：注册 `enable_ssh_tools` / `disable_ssh_tools`，移除 `enable_ssh_mcp` / `disable_ssh_mcp`。

### 6. 前端与 i18n

- `SshAssocModal.tsx`：已启用项目每次点击保存也幂等调用 `enable_ssh_tools`，用于 reconcile SKILL.md、清理存量 MCP 并补齐 token；disable 时同时清除 `sshCliToken`。daemon 每请求重读 config.json，范围变更即时生效；
- `src/i18n/locales/sshAssoc.ts`：文案里的「SSH MCP」改为「SSH 工具」（zh/en 双语同步），`enabledMessage` 保留「正在运行的会话需重启后生效」（skill 在会话启动时扫描，保守沿用该表述）；
- `types.ts` 的 `sshMcpEnabled` 字段名保留，新增可选 `sshCliToken`。

### 7. 构建管线

- `scripts/stage-sidecars.mjs`：`SIDECARS` 数组加 `'mt-ssh-cli'`；
- `src-tauri/tauri.conf.json`：`bundle.externalBin` 加 `"binaries/mt-ssh-cli"`；
- `mt-sidecars/Cargo.toml`：加 `[lib]`（bins 引用 service 层）、新 `[[bin]]`、`clap = { version = "4", features = ["derive"] }`、tokio 增补 `net` feature（named pipe / unix socket）、`base64`。

### 8. 测试计划

**单元测试（`cargo test`，沿用 fs.rs 式风格）**：
- `ssh_service` 迁移后原测试全绿（find_connection / cap_output / audit / 护栏等 40+ 条），`run_exec_on_session` 回调化后 MCP 侧收集行为等价；
- CLI 新增：trailing args 拼装、强制 `--project-token`、clap 错误前缀、退出码映射、list 文本/JSON 投影不含密码、local_path 绝对化；
- daemon 协议：请求/响应帧序列化 round-trip、hello 版本比对逻辑、stdout/stderr 分片 base64 编解码（纯函数级）；
- daemon 生命周期（集成测试，进程内起 socket/pipe）：并发绑定互斥收敛、空闲计时触发 drain、shutdown op 幂等；shutdown 后立刻连不上、在途请求照常做完、等待有上限；status 的在途数；同进程起的管道服务端核验通过、换成别的 SID 必判不可信；
- 冷启动端到端（`sidecars/tests/cli_daemon_spawn.rs`，仅 Windows，跑真实 `mt-ssh-cli.exe`，独立端点 + 临时 APPDATA，收尾必 `daemon-stop`）：用管道接住 CLI 的 stdout/stderr，CLI 退出后数秒内必须 EOF；CLI 的 cwd 退出后能改名；`daemon-status` 报日志路径、日志里有带时间戳的启动事件。对 0.4.10 二进制跑同一用例会在 5s 处失败（不挂死）；
- registry 新增：SKILL.md 生成内容断言（含 project-token、PowerShell `&`、POSIX/PowerShell 撇号转义、frontmatter 差异化）、覆盖写幂等、disable 清理含空目录、`.gitignore` 追加幂等、enable/disable round-trip 含 MCP 迁移清理。0.4.11 补：`allowed-tools` 只含 list / exec、每条规则（按 YAML 单引号标量解码后）都是正文某条示例的开头、含 `*` / 换行时整段不写；正文的 stdin / `--cwd` / WSL / 退出码说明；dev 实例启用只解析令牌、停用不删装机版文件；启动校正的各分支（最新不动、旧模板重写、令牌漂移重写、CLI 不存在改指向、另一处安装不动、外来文件与缺失文件不碰）；
- service 层（0.4.11）：收流状态机喂消息序列（Success / Data / stderr / ExitStatus / ExitSignal / Failure 的组合）、信号号映射与说明行、`--cwd` 引号的正反例（实测过 POSIX shell 的展开与字面）、脱敏正反例、护栏别名改写与按名字拦截、能力令牌报错分类（不回显配置内容与完整令牌）。另有进程内 russh 假服务器的端到端用例（`ssh_service/fake_server_tests.rs`，127.0.0.1 临时端口，池用 `SshPool::with_paths` + 临时 known_hosts，编排核心不读配置不写审计）：输出与退出码、立即 EOF、KILL / SEGV / 自定义名 USR2 信号、服务端拒绝、同一 session 上两个 exec 真并发（服务端会合点，串行就超时）、开 channel 卡住后淘汰重试成功 / 新连接也卡住报错、只收 TCP 不说话时连接阶段超时、取消打断建连、运行中超时记 timeout 且 session 留池、运行中取消记 cancelled。并发与 EOF 两条已做过变异验证（退回旧实现必红）。
- 0.4.11 评审修复：提权判定（判定不了按提权）与提示 / 拒绝文案；MSYS 改写检测按实测改写结果逐条正反例、只在 `MSYSTEM` 在且没关转换时查、三类实参各自的报错写法；管道 owner 是当前用户（daemon 的 SDDL 显式 owner，跑测试的 shell 提不提权都一样）、「进程 / owner 两项都要过」的判定、拒绝本进程的管道判 `Denied` 而非「连不上」、daemon 绑定遇到它直接 Runtime；日志轮转：8 个线程同时为 daemon 打开日志 20 轮，上一代完整留在 `.1`、每个 daemon 都写进新日志，锁被占时不轮转只追加；假服务器上排在死 session 锁上的请求在第一个判死后立刻 `SessionRetired`、8 个并发 exec 都在约一次开 channel 超时后完成、带标记的 session 不开 channel、冷却标记直接报冷却；启动校正按模板版本三档、两份安装朝修改时间较新的一方收敛、UNC 路径不 stat；冷启动集成测试补两条（MSYS 改写过的实参 exit 2 且不拉起 daemon；端点拒绝本进程时 2 秒内降级、`daemon-status` exit 2），跑测试的 shell 提权时冷启动与拒绝访问两条用例跳过（提权的 CLI 不碰 daemon）。日志轮转、死 session 退役、拒绝访问三条做过变异验证（退回旧行为必红）。

**手动 e2e 清单**：
1. Windows + Claude Code：skill 触发 → list/exec/upload/download → 首调自拉 daemon → 二调复用池（观察延迟差）→ 审计日志落行；
2. Windows + Codex：同上，重点观察沙箱是否拦网络/拦 pipe（见开放问题）；
3. WSL + Claude Code：interop 路径执行、连通 Windows 侧 daemon、退出码透传；
4. 生命周期：并发多 pane 同时首调（竞态收敛）、app 升级后版本握手换代、`daemon-stop` 排障、CLI 中途 Ctrl+C 后 daemon 池不泄漏；
5. 边界：大输出重定向、超时 124、未关联连接不可见、`allowed-tools` 带空格路径实测。

### 9. 取舍与开放问题

| # | 问题 | v1 决策 | 落地结论（2026-07-31 实测） |
|---|------|---------|------------------------------|
| 1 | daemon 引入生命周期复杂度（自拉起竞态、孤儿、版本换代、dev 下二进制文件锁定） | 接受，换取池复用；空闲 10min 自退 + 版本握手 + `daemon-stop` 三道兜底 | 真机全部验证通过：4 路并发冷启动收敛到单 daemon；空闲窗口后自退；旧版 daemon 被新 CLI 自动踢旧拉新；`daemon-stop` 后二进制文件锁释放、stage 脚本可覆盖 |
| 2 | Codex workspace-write 沙箱默认禁网，CLI 需网络（连 pipe 是否被沙箱拦也未知）→ 可能每次审批 | v1 不写 `network_access = true`（Windows 主平台沙箱为实验状态、影响有限）；实测被拦再评估是否随 enable 写入项目 `.codex/config.toml` | **待手动实测**（Windows + Codex 交互式会话） |
| 3 | `allowed-tools: 'Bash("<带空格路径>" *)'` 语法未实测 | 实测失败则去掉该行，agent 首次调用手动批准一次 | 0.4.11 起改为 YAML 列表、只预批 list / exec，每条规则与正文调用形式逐字一致（Bash 单引号路径、WSL `"$(wslpath …)"`、PowerShell `& '<ABS_PATH>'`）——依据 Claude Code 文档「Bash / PowerShell 规则按命令原文匹配、`*` 匹配任意文本、末尾 ` *` 也匹配裸命令、命令替换内部单独检查」。PowerShell 规则对 `&` 调用形式的匹配（文档说按 AST 解析、别名会被规范化，没说调用运算符如何处理）与 WSL 命令替换的放行**待交互式手动实测**；匹配不上的后果只是照常弹审批 |
| 4 | Codex 对未知 frontmatter 字段（如误留 allowed-tools）容忍度未知 | 两份模板独立生成规避 | 按决策落地（Codex 版无 allowed-tools），无需再验 |
| 5 | skill 触发率不如 MCP 工具列表强曝光 | description 覆盖典型场景；实测触发不稳再在 CLAUDE.md 模板补一句指引 | Claude Code 无头会话 4/4 轮自然语言（"查远程服务器 uptime"）均正确触发 skill；rev4 模板改为 capability token 后需随最终手动矩阵复核 |
| 6 | 远程命令自身 exit 124 与超时歧义 | 接受，stderr 前缀可辅助区分 | 按决策落地 |
| 7 | daemon 与残存 MCP sidecar 并存时各持独立池（同一连接可能两条 session） | 接受（服务器视角只是两个登录），PR3 下线 MCP 后消失 | 接受不变（注：MCP 下线是 PR4） |

### 10. 分阶段落地

- **PR1**：service 层抽取（`mt-sidecars` lib + `ssh_service` 回调化）+ MCP 适配层瘦身回归。纯重构，行为零变化，`cargo test` 全绿即可合；✅ 已落地
- **PR2**：`mt-ssh-cli` 二进制（CLI + daemon + IPC 协议 + 降级路径）+ 构建管线。纯新增，不动注册链路；✅ 已落地
- **PR3**：`ssh_skill_registry` + 前端切换 + i18n + MCP 迁移清理 + docs（features 双语；CLAUDE.md command 表原本未列 SSH 命令，无需改）。注：`ssh_mcp_registry` 的写入路径已在本阶段一并移除（切换后即无调用方，留着是死代码），PR4 仅剩 bin 下线；✅ 已落地
- **PR4**（后续版本）：确认存量迁移完成后，移除 `mt-ssh-mcp` bin 与 externalBin 条目、`ssh_mcp_registry` 仅留读侧清理，spec 标 final。

### 11. 落地验证记录（2026-07-31）

rev3 自动化 e2e 已跑通（Windows + 真实 SSH 连接 oracle-4c-24g）；rev4 安全加固已完成自动化单元/集成回归，真实 SSH 手动复核仍列在下方：

- CLI 契约（rev3）：`list` 表格/JSON 不含敏感字段；`exec` 退出码透传（0/17）、超时 124 + stderr 前缀、未找到连接 exit 2；SFTP 上传/下载 round-trip 字节一致；审计日志逐条落行。rev4 已移除 project-id/env 兜底，改为必填 capability token；
- daemon：冷启动自拉起（~1.8s 全链路）→ 二次调用池复用（~0.39s）；4 路并发 exec 不串流；4 路并发冷启动收敛单 daemon；CLI 中途被杀 daemon 存活且池不泄漏；空闲自退（短窗口计时验证）；版本握手换代（0.4.8 daemon 被 0.4.9 CLI 踢旧拉新）；`daemon-stop` 幂等、释放二进制文件锁；强制端点不可用时自动降级 one-shot 且业务成功；
- skill 端到端：临时项目放入生成的 SKILL.md，新起 Claude Code 会话用自然语言问「远程服务器 uptime」→ 触发 skill → 经 daemon 执行 CLI → 正确返回远端 uptime/内核版本（权限经显式白名单放行，等价 skill allowed-tools 语义）。

**剩余手动矩阵**（需交互式会话/真机环境，见 §9 表）：

1. Windows + Claude Code 交互式：UI「关联 SSH」启用 → skill 自动触发 → `allowed-tools` 是否免审批（开放问题 #3 交互式部分）；
2. Windows + Codex：沙箱是否拦网络/拦 pipe（开放问题 #2）；
3. WSL + Claude Code：interop 路径执行、连 Windows 侧 daemon、退出码透传；
4. 存量 MCP 项目在 UI 里重新保存关联 → 生成并持久化 capability token、重写 Skill、迁移清理生效（单测已覆盖文件级行为，差 UI 全流程一遍）；
5. IPC 端点跨用户连接被拒（Windows ACL / Unix 0600 —— 代码已实现，需第二个系统用户实测）；反方向「别的用户抢注端点 → CLI 判不可信、降级 one-shot，`daemon-status` / `daemon-stop` exit 2」同样需第二个用户实测（单用户下只验证了核验链本身与 SID 不同时的判定）。
