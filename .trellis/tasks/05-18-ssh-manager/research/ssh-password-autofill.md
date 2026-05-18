# Research: SSH 密码自动填充机制（SSH_ASKPASS vs PTY 输出扫描）

- **Query**: 在 mini-term 中如何可靠地把已保存的 SSH 密码自动喂给系统 OpenSSH `ssh` 客户端
- **Scope**: external（OpenSSH 文档 / 源码 / Win32-OpenSSH issue tracker）
- **Date**: 2026-05-18

## 背景约束（来自代码与 PRD）

- mini-term 后端用 `portable-pty` 在真实 PTY 里 spawn 进程；`create_pty` 已支持 `cmd.env(...)` 注入环境变量（见 `src-tauri/src/pty.rs:545-572`，已注入 `TERM` / `LANG` / `MINITERM_PTY_ID` 等）。
- PTY master 的 reader 线程已在做"输出扫描"——检测 AI 命令 echo（`output_contains_ai_command`，`pty.rs:692-708`），且持有 `writer`，已有 `write_pty_chunked` 写入助手。
- SSH 连接通过 `create_pty`（spawn `ssh`）或 `write_pty`（往现有 shell 写 `ssh ...\r`）拉起。PRD 决策是"在当前终端写入 `ssh` 命令"。

## 核心问题

OpenSSH `ssh` **故意**只从控制 TTY（`/dev/tty`）读取密码，不读 stdin / argv / 环境变量。源码 `read_passphrase()`（`readpass.c`）默认走 `RPP_REQUIRE_TTY` 分支 `open(_PATH_TTY, ...)`。因此不能简单地把密码写进进程 stdin。需要真正的机制。

---

## 机制 1：SSH_ASKPASS + SSH_ASKPASS_REQUIRE

### 工作原理（OpenSSH 源码 `readpass.c` 的 `read_passphrase()`）

`read_passphrase()` 的 askpass 判定逻辑（master 分支，`readpass.c`）：

```c
if (((s=getenv("DISPLAY")) && *s) || ((s=getenv("WAYLAND_DISPLAY")) && *s))
    allow_askpass = 1;
if ((s = getenv(SSH_ASKPASS_REQUIRE_ENV)) != NULL) {
    if (strcasecmp(s, "force") == 0) { use_askpass = 1; allow_askpass = 1; }
    else if (strcasecmp(s, "prefer") == 0) use_askpass = allow_askpass;
    else if (strcasecmp(s, "never") == 0) allow_askpass = 0;
}
...
if (use_askpass && allow_askpass) {
    askpass = getenv("SSH_ASKPASS") ? getenv("SSH_ASKPASS") : _PATH_SSH_ASKPASS_DEFAULT;
    return ssh_askpass(askpass, prompt, askpass_hint);
}
```

`ssh_askpass()` 的契约（关键）：

- **prompt 作为 `argv[1]`** 传给 helper：`execlp(askpass, askpass, msg, NULL)`（Windows 上是 `posix_spawn` → `CreateProcessW`）。
- helper 把密码**打印到 stdout**，ssh 读取第一行，按 `\r\n` 截断（`buf[strcspn(buf,"\r\n")]='\0'`）。
- helper 必须**退出码 0**，否则 ssh 丢弃结果。
- ssh 额外 `setenv("SSH_ASKPASS_PROMPT", hint, 1)`：值为 `"confirm"`（host-key 确认场景）或 `"none"`（FIDO 通知）。普通密码场景该变量不设置。

`SSH_ASKPASS_REQUIRE=force` 的效果：`use_askpass=1` 且 `allow_askpass=1` 被**无条件**置位，**不再需要 `DISPLAY`**。该判定在 `read_passphrase()` 入口，对所有调用方生效。

### 关键澄清：man page 说"passphrase"，但实际覆盖密码认证

`ssh.1` man page（man.openbsd.org/ssh）的措辞只提 "passphrase"，容易误读为"只对密钥 passphrase 生效"。但源码证实：**SSH 服务器密码认证**也走同一个 `read_passphrase()`。`sshconnect2.c` 的 `userauth_passwd()`：

```c
xasprintf(&prompt, "%s@%s's password: ", authctxt->server_user, host);
password = read_passphrase(prompt, 0);
```

所以 `SSH_ASKPASS_REQUIRE=force` 下，**密码认证、密钥 passphrase、过期密码修改、host-key 确认**全部会调用 askpass helper。这正是本功能需要的（用户保存的是登录密码）。

### Windows 内置 OpenSSH 是否支持？—— 支持，但有版本门槛

| Windows 版本 | 内置 OpenSSH | `SSH_ASKPASS_REQUIRE` | 结论 |
|---|---|---|---|
| Windows 10（如 21H2 及多数版本） | `OpenSSH_for_Windows_8.1p1` | **不支持**（8.4 前无此变量） | force 无效；8.1 的 askpass 逻辑也不同 |
| Windows 11（22H2 / 23H2 / 24H2） | `OpenSSH_for_Windows_8.6p1`+ | **支持** | 设 `SSH_ASKPASS_REQUIRE=force` 即生效 |

证据：

- Win32-OpenSSH issue **#1726**（"SSH_ASKPASS_REQUIRE not respected"，2021）：维护者 bagajjal 明确回复 "`SSH_ASKPASS_REQUIRE` is introduced in OpenSSH V8.4. Current Windows latest version is V8.1." → Windows 10 的 8.1 不支持。
  <https://github.com/PowerShell/Win32-OpenSSH/issues/1726>
- Win32-OpenSSH issue **#2115**（"SSH_ASKPASS is ignored by ssh.exe on Windows 11"，2023）：报告者在 Windows 11 + `OpenSSH_for_Windows_8.6p1` 上发现单设 `SSH_ASKPASS` 无效；评论给出解法——**同时设 `SSH_ASKPASS_REQUIRE=force`**，报告者确认 "it works!"。
  <https://github.com/PowerShell/Win32-OpenSSH/issues/2115>

OpenSSH 8.4 发布于 2020-09-27（`SSH_ASKPASS_REQUIRE` 来自 bz#69，见 <https://www.openssh.com/txt/release-8.4>）。Windows 11 自带 8.6+（<https://www.openssh.com/txt/release-8.6>）。

### Windows 上的 helper 程序设计要点

- **不需要 GUI / 窗口 / `DISPLAY`**：`force` 模式下源码完全不检查 `DISPLAY`/`WAYLAND_DISPLAY`。helper 可以是纯 console 程序，只要把密码写 stdout 并 `exit(0)`。
- **子系统无所谓**：helper 是 ssh 用 `CreateProcessW` 起的子进程，其 stdout 被重定向到管道。console subsystem 的小程序最简单（无窗口闪烁风险，且 stdout 行为可控）。如担心一闪而过的控制台窗口，可用 GUI subsystem 程序、或加 `CREATE_NO_WINDOW`——但实测 console helper 在已重定向 stdout 时通常不弹窗。
- **helper 必须能被找到**：`SSH_ASKPASS` 应是 helper 的**绝对路径**。`.exe` / `.cmd` / `.bat` 均可（`.cmd`/`.bat` 经 `cmd /c`），但 8.1 时代有 `CreateProcessW failed error:2` 报告（issue #1726），绝对路径 + `.exe` 最稳妥。

### 密码如何安全传给 helper

helper 收到的 `argv[1]` 只是 prompt 文本（如 `user@host's password:`），**不含密码**。密码要由 mini-term 自己传给 helper。三种途径：

1. **环境变量（推荐）**：mini-term spawn `ssh` 时用 `cmd.env("MINITERM_SSH_PASSWORD", pwd)` 注入；helper 子进程继承该环境变量，读出后打印 stdout。`portable-pty` 的 `CommandBuilder` 已支持（`pty.rs:545` 已这么做）。环境变量对其他用户不可见（仅该进程树可见），生命周期短。
2. **临时文件**：写权限 600 的临时文件，路径经环境变量传 helper，helper 读完即删。比 env 复杂，无明显收益。
3. **argv**：把密码作为 helper 的命令行参数——**不推荐**，本机其他进程可通过进程列表看到 argv。

建议用途径 1。注意：通过环境变量传递密码意味着同机器的进程枚举工具能看到 `ssh` 进程的环境块（需要相应权限）。考虑到 PRD 已明确"明文密码落盘"是用户接受的取舍，环境变量传递的风险等级与之相当或更低（不落盘、进程退出即消失）。

### helper 程序形态选择

- **方案 A：独立打包一个 mini-term-askpass.exe**。最干净，但要多构建一个产物 + 处理路径解析。
- **方案 B：复用主程序自身**。`ssh` 起的是 `mini-term.exe --ssh-askpass`，主程序检测到该 flag 时只做"读 `MINITERM_SSH_PASSWORD` → 打印 stdout → exit(0)"再退出，不进入 Tauri 主流程。Tauri/Rust 程序可在 `main()` 最前面判断 `std::env::args()`。**推荐方案 B**：零额外产物，路径就是 `std::env::current_exe()`。需注意 mini-term 是 GUI subsystem，作为 console helper 运行时 stdout 可能要显式 attach；用 console subsystem 单独小 exe（方案 A）在这点上反而省心。二者权衡后由实现阶段定。

### macOS / Linux 行为

- 同一份 `read_passphrase()` 源码，`SSH_ASKPASS_REQUIRE=force` 在 OpenSSH ≥ 8.4 一致生效。
- **macOS**：系统自带 OpenSSH。macOS 12 (Monterey) 起为 8.6+，13/14/15 更高 → 支持 `force`。老 macOS 11 是 8.1（不支持）。
- **Linux**：发行版自带 OpenSSH，2021 年后的主流发行版（Ubuntu 21.04+/22.04、Debian 11+ 等）均 ≥ 8.4 → 支持。`force` 模式下不需要 X11 askpass（如 `ssh-askpass` / `ksshaskpass`），因为是 mini-term 自己的 helper。
- 跨平台 helper：Unix 上 helper 是普通可执行文件，stdout 行为天然正常，比 Windows 更省心。

### 机制 1 小结

- 优点：官方机制；一次注入、ssh 主动调用；不依赖脆弱的字符串匹配；密码不进 PTY 字节流（不会被 echo / 记录）；自动覆盖密码认证 + passphrase + host-key 确认。
- 缺点：**Windows 10 不支持**（8.1）；需要一个 helper 程序（或主程序自带 askpass 分支）；host-key 首次确认也会被 askpass 接管（见下"陷阱"）。

### 机制 1 的陷阱：host-key 首次确认

`sshconnect.c` 的 `confirm()`（用于 `Are you sure you want to continue connecting (yes/no/[fingerprint])?`）**也调用 `read_passphrase()`**。`force` 模式下，这个确认提示同样会调 askpass helper，并设 `SSH_ASKPASS_PROMPT=confirm`。

后果：若 helper 无脑返回密码，host-key 提示会收到"密码"当作答案 → 非 `yes`/空 → ssh 判定为拒绝 → 连接失败。

helper 必须区分：

```text
若 环境变量 SSH_ASKPASS_PROMPT == "confirm"  → 打印 "yes"（自动接受 host key）
否则                                          → 打印保存的密码
```

是否自动 `yes` 接受 host key 是一个安全取舍；保守做法是 `confirm` 场景返回空串或不返回（exit≠0），让连接失败并提示用户改用"普通终端手动确认一次"。建议实现阶段决策；MVP 可先自动 `yes`（与"复用系统 known_hosts"的 PRD 取向一致）。

---

## 机制 2：PTY 输出扫描 + 回写

mini-term 持有 PTY master，可在 reader 线程扫描 `ssh` 输出，匹配到密码提示就往 `writer` 写 `password + "\n"`。

### 现代 OpenSSH 的确切提示串（源码取证）

| 场景 | 源文件 | 格式串 | 实际样例 |
|---|---|---|---|
| 密码认证 | `sshconnect2.c` `userauth_passwd` | `"%s@%s's password: "`（`server_user`@`host`） | `root@10.0.0.5's password: ` |
| 键盘交互 | `sshconnect2.c` `userauth_kbdint` | 服务器下发的 prompt，常见即 `Password: ` | `Password: ` |
| 密钥 passphrase | `ssh-add`/`ssh` | `Enter passphrase for key '...': ` | `Enter passphrase for key '/home/u/.ssh/id_rsa': ` |
| 过期密码-旧 | `sshconnect2.c` | `"Enter %.30s@%.128s's old password: "` | `Enter root@host's old password: ` |
| 过期密码-新 | `sshconnect2.c` | `"Enter %.30s@%.128s's new password: "` | — |
| host-key 首次确认 | `sshconnect.c` `check_host_key` | `Are you sure you want to continue connecting (yes/no/[fingerprint])? ` | 同左 |
| host-key 复述 | `sshconnect.c` `confirm` | `Please type 'yes', 'no' or the fingerprint: ` | 同左 |

注意提示串里的 `'s password:` 中那个是 ASCII 单引号 `'`（U+0027），不是花引号。

### 推荐的匹配正则

对密码提示，宽松但安全的匹配（不区分大小写、容忍行尾空格）：

```text
(?i)password:\s*$
```

它能命中 `<user>@<host>'s password:` 和键盘交互的 `Password:`。若想更精确区分"密码认证 vs passphrase"：

- 密码认证 / 键盘交互（要回密码）：`(?i)(?:'s password:|^\s*password:)\s*$`
- 密钥 passphrase（要回 passphrase，不是登录密码）：`(?i)enter passphrase for `
  注意 PRD 里 `identityFile` 是私钥路径；若私钥带 passphrase，本功能保存的"password"语义上不一定等于 passphrase。MVP 可只处理服务器密码认证，passphrase 让用户手动输。

host-key 提示用单独正则识别，**不要**当成密码提示：

```text
(?i)are you sure you want to continue connecting|please type 'yes'
```

### 排序与时序

正常顺序：①（可选）host-key 确认 → ②密码提示。host-key 提示一定在密码提示之前。扫描器应：

- 命中 host-key 提示 → 视实现决策回写 `yes\n` 自动接受，或不回写交给用户。
- 命中密码提示 → 回写密码。

### 去重与防死循环（关键）

OpenSSH 默认 `NumberOfPasswordPrompts=3`：**密码错误会重试最多 3 次**，每次重新打印 `<user>@<host>'s password:`，且前面会先打印 `Permission denied, please try again.`（见 `userauth_passwd`：`if (attempt_passwd != 1) error("Permission denied, please try again.")`）。

若扫描器对每个 `password:` 都回写保存的密码，密码错误时会自动连灌 3 次错密码、3 次失败，纯属噪音。必须加护栏：

1. **每个 ssh 进程只自动填一次密码**：在 `PtyManager` 加 per-pty 状态（如 `ssh_autofill: HashMap<u32, AutofillState>`），首次匹配密码提示并回写后置 `done`，后续 `password:` 不再回写（让用户手动改）。
2. **看到 `Permission denied, please try again.` 立即禁用该 pty 的自动填充**——明确信号"保存的密码是错的"。
3. **跨缓冲区匹配**：reader 是 16ms 批量 + 4096 字节分块（`pty.rs:587-598`），提示串可能跨块。应在 per-pty 维护一个小的"尾部残留 buffer"（如末 256 字节）拼接后再匹配，匹配后清空，避免重复命中同一行。
4. **ANSI 处理**：`ssh` 的密码提示是裸文本无颜色，但稳妥起见可复用现成的 `strip_ansi_codes()`（`pty.rs:224`）后再匹配。
5. **回写时机**：必须在 `pty-output` emit 之后或同时把密码写进 `writer`；密码会被终端 echo 关闭，用户不可见，但仍进入了 PTY 字节流。

### 机制 2 小结

- 优点：纯后端，无 helper 程序，无额外产物；**不依赖 OpenSSH 版本**，Windows 10 的 8.1 一样可用；和现有"输出扫描检测 AI 命令"基础设施同构。
- 缺点：脆弱——依赖提示串文本（locale 理论上可变，但 OpenSSH 客户端这些字符串是硬编码英文、无 gettext，实践中稳定）；密码进入 PTY 字节流；时序竞态需谨慎处理；错密码重试护栏必须做对，否则灌错密码 / 死循环；与 host-key 提示的交错需要正确分流。

---

## 机制 3：plink -pw / sshpass（简评）

- **PuTTY `plink -pw <password>`**：可直接命令行传密码。但 `plink` **不是 Windows 自带**，需用户另装 PuTTY，且不读 `~/.ssh/config`、known_hosts 与 OpenSSH 不共享。与 PRD"复用系统 `ssh` 客户端、复用 known_hosts/ssh-agent"的决策冲突。新版 plink 还要求先用 `-hostkey` 或交互接受过 host key。不推荐作为主方案。
- **`sshpass`**：Linux 上常见（`sshpass -p <pwd> ssh ...`，靠分配伪 TTY 喂密码）。但 **Windows 不自带**，且官方 Win32-OpenSSH 也未集成（仅有 open 状态的 feature request issue #1943，<https://github.com/PowerShell/Win32-OpenSSH/issues/1943>）。macOS 需 `brew install sshpass`（Homebrew 甚至一度因安全考虑下架）。跨平台一致性差，不推荐。

结论：plink / sshpass 都需要用户额外安装、且会偏离"统一用系统 OpenSSH"的架构决策，不值得采用。可作为"用户自行配置的自定义 ssh 命令"间接支持，但不应是 mini-term 的内建自动填充机制。

---

## 推荐方案（Windows 优先）

### 主方案：SSH_ASKPASS + SSH_ASKPASS_REQUIRE=force

mini-term spawn `ssh` 时（走 `create_pty` 是最干净的，能精确控制环境变量；若走 `write_pty` 往现有 shell 写命令则无法注入进程级 env，见下"注意"），设置：

```text
SSH_ASKPASS         = <askpass helper 的绝对路径>
SSH_ASKPASS_REQUIRE = force
MINITERM_SSH_PASSWORD = <保存的明文密码>     # helper 通过它拿密码
DISPLAY             = 不需要设置（force 模式下源码不检查）
```

helper 程序逻辑（极简）：

```text
读环境变量 SSH_ASKPASS_PROMPT:
  == "confirm" → 打印 "yes"（自动接受首次 host key；安全取舍，可改为返回空让用户手动确认）
  其它/未设置  → 打印 环境变量 MINITERM_SSH_PASSWORD 的值
打印后换行，exit(0)
```

helper 形态推荐"主程序自带 `--ssh-askpass` 分支"或"独立 console 小 exe"，由实现阶段按构建复杂度定（见上文"helper 程序形态选择"）。

为什么是主方案：官方机制、不靠字符串匹配、密码不进 PTY 字节流、自动覆盖密码 + passphrase + host-key 确认、Windows 11 实测可用（issue #2115 证实）。

### 重要注意：env 注入要求用 `create_pty` 起 ssh

`SSH_ASKPASS` 等是**进程级环境变量**，必须在 spawn `ssh` 进程时通过 `CommandBuilder::env` 注入。若按当前 PRD"在现有 shell 里写 `ssh ...\r`"，则这些 env 不会附加到 `ssh` 进程上（除非先写 `set`/`$env:` 再写 `ssh`，很丑）。

→ 实现建议：**SSH 连接走 `create_pty` 直接 spawn `ssh`（带 env），而不是往现有 shell 写命令字符串**。这会与 PRD 的"在当前终端写入命令"决策有出入，需要 implement 阶段与用户确认。若必须保留"当前终端写命令"形态，则主方案退化为机制 2。

### 回退方案：PTY 输出扫描（机制 2）

用于两种情况：(a) 检测到 OpenSSH < 8.4（如 Windows 10 的 8.1）；(b) SSH 连接采用"往现有 shell 写命令"形态、无法注入 env。

回退实现要点（前面"机制 2"已详述）：

- 密码提示匹配：`(?i)(?:'s password:|^\s*password:)\s*$`
- host-key 提示匹配（单独分流）：`(?i)are you sure you want to continue connecting|please type 'yes'`
- 每个 ssh pty **只自动填一次密码**；命中 `Permission denied, please try again.` 立即永久禁用该 pty 自动填充。
- per-pty 维护尾部残留 buffer（≈256B）解决跨 16ms/4096B 分块匹配；匹配后清空防重复。
- 复用 `strip_ansi_codes()` 后匹配。

可检测 ssh 版本来二选一：实现时可先跑 `ssh -V`（输出形如 `OpenSSH_for_Windows_8.6p1, LibreSSL ...`）解析主次版本号，≥ 8.4 用机制 1，否则用机制 2。

---

## 跨平台注意事项汇总

| 平台 | 自带 OpenSSH | 主方案可用性 | 备注 |
|---|---|---|---|
| Windows 11 | 8.6p1+ | 可用（需 `SSH_ASKPASS_REQUIRE=force`） | issue #2115 证实 |
| Windows 10 | 8.1p1 | **不可用** → 回退机制 2 | 8.1 < 8.4，无 `SSH_ASKPASS_REQUIRE` |
| macOS 12+ | 8.6+ | 可用 | macOS 11 是 8.1，需回退 |
| Linux（2021+ 发行版） | ≥ 8.4 | 可用 | 老发行版需回退 |

通用：

- `ssh.exe` 默认路径 `C:\Windows\System32\OpenSSH\ssh.exe`；也可能用户装了 Git for Windows / Scoop 的 ssh，版本不同——靠 `ssh -V` 实测而非假设。
- host-key 首次确认：主方案下被 askpass 接管（`SSH_ASKPASS_PROMPT=confirm`）；回退方案下要单独识别提示串。两种方案都要决定"自动 yes vs 交给用户"。
- 密码提示字符串是 OpenSSH 客户端**硬编码英文**，无 i18n / gettext，locale 不影响匹配（这点对回退方案是利好）。

---

## Caveats / Not Found

- 未实机验证 helper 在 mini-term 的 GUI-subsystem 主程序内作为 `--ssh-askpass` 运行时 stdout 是否需要 `AttachConsole`/显式句柄处理——实现阶段需在 `npm run tauri dev` 下实测。建议优先试"独立 console 小 exe"，行为最确定。
- 未确认 Windows 各 patch 级（如 Win10 22H2 后期更新）是否回升过 OpenSSH 版本——以 `ssh -V` 运行时探测为准。
- `_PATH_SSH_ASKPASS_DEFAULT` 在 Win32-OpenSSH 的具体取值未取证；本方案显式设 `SSH_ASKPASS` 绝对路径，不依赖默认值。
- 回退方案的"键盘交互（kbdint）"提示由**服务器**下发，绝大多数 PAM 配置就是 `Password: `，但理论上服务器可自定义为任意文本；极端自定义服务器下回退方案可能匹配不到。

## 来源（URLs）

- OpenSSH `ssh(1)` man page（`SSH_ASKPASS` / `SSH_ASKPASS_REQUIRE` 定义）：<https://man.openbsd.org/ssh>
- OpenSSH 源码 `readpass.c`（`read_passphrase` / `ssh_askpass` 实现）：<https://github.com/openssh/openssh-portable/blob/master/readpass.c>
- OpenSSH 源码 `sshconnect2.c`（`userauth_passwd` 密码提示串）：<https://github.com/openssh/openssh-portable/blob/master/sshconnect2.c>
- OpenSSH 源码 `sshconnect.c`（host-key `confirm()` 提示串）：<https://github.com/openssh/openssh-portable/blob/master/sshconnect.c>
- OpenSSH 8.4 release notes（`SSH_ASKPASS_REQUIRE` 引入，bz#69）：<https://www.openssh.com/txt/release-8.4>
- OpenSSH 8.6 release notes（Windows 11 自带版本）：<https://www.openssh.com/txt/release-8.6>
- Win32-OpenSSH issue #2115（Windows 11 需 `SSH_ASKPASS_REQUIRE=force`）：<https://github.com/PowerShell/Win32-OpenSSH/issues/2115>
- Win32-OpenSSH issue #1726（`SSH_ASKPASS_REQUIRE` 8.4 才引入，Win10 的 8.1 不支持）：<https://github.com/PowerShell/Win32-OpenSSH/issues/1726>
- Win32-OpenSSH issue #1943（`sshpass` 未集成进 Windows，仍是 open 的 feature request）：<https://github.com/PowerShell/Win32-OpenSSH/issues/1943>
- Unix StackExchange（`SSH_ASKPASS_REQUIRE=force` 用法讨论）：<https://unix.stackexchange.com/questions/272506/can-i-get-ssh-to-use-an-askpass-program-even-if-it-was-run-from-a-terminal>
