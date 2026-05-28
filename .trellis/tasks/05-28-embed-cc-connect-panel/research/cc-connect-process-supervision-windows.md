# Research: cc-connect 进程监管 (Windows / Tauri v2)

- **Query**: 查清 cc-connect daemon/windows.go 实际做什么 + mini-term 拉起 cc-connect 进程的最佳方案
- **Scope**: mixed (外部:cc-connect Go 源码、Tauri v2 文档、Rust crates;内部:mini-term src-tauri)
- **Date**: 2026-05-28

---

## cc-connect Windows daemon 现状(代码层)

### 1. `daemon/windows.go` —— **不是 stub**,完整 Task Scheduler 实现

代码 `//go:build windows`,大小 7719 字节。实现 `Manager` 接口的 `schtasksManager`,核心动作是用 PowerShell 调 Windows Task Scheduler:

- **Install** (`schtasksManager.Install`):
  1. 创建 `~/.cc-connect/cc-connect-daemon.ps1`,内容是 `while ($true) { & cc-connect.exe; if ($LASTEXITCODE -eq 0) { exit 0 } else { Start-Sleep -Seconds 10 } }` —— 自带 **崩溃 10s 后重启** 循环
  2. PowerShell `Register-ScheduledTask`:`TaskName = cc-connect`,`Trigger = AtLogOn -User $env:USERNAME`,`Principal = LogonType Interactive, RunLevel Limited`,`Action = powershell.exe -WindowStyle Hidden -NoProfile -NonInteractive -ExecutionPolicy Bypass -File <ps1>`
  3. 调用 `Start-ScheduledTask`
- **Status**: PowerShell `Get-ScheduledTask -TaskName cc-connect`,读 `$task.State` 判断 `Running`
- **Stop/Restart/Uninstall**: `Stop-ScheduledTask` / `Unregister-ScheduledTask`
- **要求**: PATH 里有 `powershell.exe`(`exec.LookPath` 检查)。出错信息为 `"powershell.exe not found: Windows Task Scheduler management requires PowerShell"`

注意:Task Scheduler 不是 Windows Service,**不需要管理员权限**,跑在当前用户上下文。

### 2. `daemon/unsupported.go` —— 用户当前看到的报错来源

```go
//go:build !linux && !darwin && !windows

func newPlatformManager() (Manager, error) {
    return nil, fmt.Errorf("daemon management is not supported on %s; use a process manager (e.g. nssm, pm2) instead", runtime.GOOS)
}
```

报错原文 `"daemon management is not supported on windows..."` **物理上不该来自 unsupported.go**(它的 build tag 已经排除 windows)。可能性:

1. **最有可能**:用户装的是 v1.3.2 stable (2026-04-21,`npm install -g cc-connect`)。Windows daemon 支持是 PR #817 在 **2026-05-05** 合并 (commit `d8efe17a`),v1.3.2 之前没有 windows.go,Windows 平台会落到 unsupported.go 兜底。最新 `v1.3.3-beta.4` (2026-05-27) 才完整可用。
2. 次要可能:CHANGELOG 中 v1.3.3-beta.3 (2026-05-24) 还在 fix Windows cross-compile (`add missing CheckLinger stub`),说明 daemon/windows.go 整段路径直到这个 beta 才稳定可分发。

### 3. `daemon/manager.go` —— 平台无关接口

```go
type Manager interface {
    Install(cfg Config) error
    Uninstall() error
    Start() error
    Stop() error
    Restart() error
    Status() (*Status, error)
    Platform() string
}

type Config struct {
    BinaryPath string; WorkDir string
    LogFile string; LogMaxSize int64
    EnvPATH string; EnvExtra map[string]string
}

type Status struct {
    Installed bool; Running bool; PID int
    Platform string // "systemd", "launchd", "schtasks"
}
```

`NewManager() → newPlatformManager()` 走 build-tag 分流,Status.PID 在 launchd / systemd 路径都填,**但 schtasks 路径不填 PID**(Task Scheduler 仅暴露 State,不直接给 child PID)。

### 4. `daemon/launchd.go` / `daemon/systemd.go` —— 对照参考

| 维度 | launchd (macOS) | systemd (Linux) | schtasks (Windows) |
|---|---|---|---|
| 服务定义 | `~/Library/LaunchAgents/com.cc-connect.service.plist` | `~/.config/systemd/user/cc-connect.service` 或 `/etc/systemd/system/...` (root) | Task Scheduler `cc-connect` + `~/.cc-connect/cc-connect-daemon.ps1` |
| 自动重启 | plist `KeepAlive.SuccessfulExit=false` (launchd 内建) | unit `Restart=on-failure RestartSec=10` (systemd 内建) | ps1 while loop + `Start-Sleep -Seconds 10` (脚本层) |
| 开机启动 | `RunAtLoad=true` + `LimitLoadToSessionType=[Aqua,Background]` | `WantedBy=default.target` (user) / `multi-user.target` (system) | `New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME` |
| PID 可拿 | launchctl print 输出 `pid = ` | systemctl show MainPID= | **拿不到**(只有 State=Running) |
| 状态查询 | `launchctl print user/$uid/com.cc-connect.service` | `systemctl --user show ...` | `Get-ScheduledTask -TaskName cc-connect` |
| 卸载 | `launchctl bootout` + rm plist | `systemctl disable --now` + rm unit + daemon-reload | `Stop-ScheduledTask` + `Unregister-ScheduledTask` + rm ps1 |
| Linger 警告 | 总是 true (launchd 持续) | `loginctl show-user $USER -p Linger`,非 root 且没 linger 会警告 SSH 断线 stop | 总是 false (Windows 无概念,`CheckLinger()` no-op) |

**Windows 缺什么**(对照 launchd/systemd):
- 拿不到子进程 PID(用户调 `cc-connect.exe`,但 mini-term 这边只能看 Task Scheduler `cc-connect` 任务,实际进程 PID 要另查)
- 没有 systemd 的 Restart 退避策略(systemd 默认指数退避抑制 crash loop;ps1 是固定 10s,可能频繁重启)
- Task Scheduler 任务跟用户绑死,跨用户 / Service 账户使用受限

### 5. cc-connect 自带 HTTP Management API —— **这是 mini-term 探活/集成的关键**

文件:`core/management.go` + `docs/management-api.md`(v1.1-draft)。

- **默认端口**:`9820`(`cmd/cc-connect/web.go` 默认 fallback)
- **启用方式**:config.toml 中
  ```toml
  [management]
  enabled = true
  port = 9820
  token = "mgmt-secret"
  ```
  或者执行 `cc-connect web`,会 **自动写入 config 并生成 token**(`config.EnableWebAdmin`)
- **认证**:`Authorization: Bearer <token>` 或 query `?token=<token>`
- **核心端点**:
  - `GET /api/v1/status` → `{ok: true, data: {version, uptime_seconds, connected_platforms, projects_count, bridge_adapters[]}}`(**直接当 health probe**)
  - `POST /api/v1/restart` → 软重启
  - 其余 project/session/cron 资源端点完整
- **Web 控制台**:`http://localhost:9820/login?token=<token>` 自动重定向到 SPA(`web/` 子目录的 React/Vite 应用,`web/embed.go` 用 `go:embed web/dist` 嵌进二进制)
- **实例锁**:`cmd/cc-connect/instance_lock.go` 用 flock(Unix)/ no-op (Windows) 防止同一 config 起两个进程。Windows `instance_lock_windows.go` 是 stub,仅写 PID 文件不锁

---

## 三种 supervision 策略对比

### A. **attach-only**(零监管,只发现已有实例)

| 维度 | 详情 |
|---|---|
| 工作方式 | 用户在 mini-term **某个终端 pane 内** 自己跑 `cc-connect` 或装到 Task Scheduler;mini-term 仅扫 `127.0.0.1:9820` 做 health check 决定 UI 显示哪种状态 |
| 探活方法 | `reqwest::get("http://127.0.0.1:9820/api/v1/status?token=...")` 走 mini-term Rust 端,2-3s 超时;读 `~/.cc-connect/config.toml` 解 `[management] port/token` |
| 启动/停止 | mini-term **不负责**。UI 提供 "在新终端打开运行" 按钮 → 用 `pty::create_pty` 起个 pane 跑 `cc-connect` 命令(或 `cc-connect web` 触发自动启动) |
| 崩溃恢复 | 不管。用户自己看 PTY pane 输出或装 Task Scheduler 走 cc-connect 自带 ps1 重启 |
| 退出 mini-term | cc-connect 进程跟 mini-term 完全解耦,该跑跑 |
| 复杂度 | **最低**。一个 HTTP probe + 一个 webview iframe 加载 `http://localhost:9820/login?token=...` 完事 |
| 风险 | 用户体验:第一次得手动起 cc-connect。但可用 **快捷创建终端 + 预填命令** 抹平 |

### B. **spawn + monitor**(mini-term 当 supervisor)

| 维度 | 详情 |
|---|---|
| 工作方式 | mini-term Rust 端用 `std::process::Command` 或 `portable-pty::CommandBuilder` 启动 `cc-connect` 子进程,Arc<Mutex<Child>>;后台 thread `child.wait()` + 自动重启 |
| spawn 方式 | 三选一:1) **portable-pty 复用**:走 `pair.slave.spawn_command(cmd)` —— 但 cc-connect 是后台 service 不需要 TTY,会多分配 ConPTY 浪费;2) **`std::process::Command::new("cc-connect").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()` + thread 读 stdout/stderr** —— 跨平台标准;3) **`shared_child = "1"`**(Tauri shell 自己用的)—— 允许多线程 wait/kill;4) **`command-group`** —— 进程组管理,kill 整组,Windows 走 Job Object |
| 探活 | 启 child 后等 500ms-3s 做 HTTP probe;失败重试 N 次再判失败 |
| 崩溃恢复 | wait thread 看到 exit → 指数退避(1s/2s/4s/最高 30s)重启 |
| 退出 mini-term | tauri `on_window_event(WindowEvent::Destroyed)` → 调 `child.kill()`。需要解决 **Windows 进程树 kill**:`std::process::Child::kill` 只杀直接子,如果 cc-connect 又 fork shell 子进程会孤儿,**用 `command-group` 的 `GroupChild::kill` 走 Job Object 杀整组** |
| 端口 | mini-term 控制 config.toml,可强制写一个固定 token + 端口(避开端口被占的情况要重试) |
| 复杂度 | 高。要处理:重启风暴抑制、stdout/stderr 落盘、Windows 进程树杀法、PATH 解析(cc-connect 装在 npm 全局还是 $PATH 还是绝对路径) |
| 风险 | 1) 跟 cc-connect 自己的 daemon Install 互斥 —— 用户如果同时跑 Task Scheduler 的 cc-connect 又被 mini-term 拉起一个,会撞 9820 端口或撞实例锁 2) 用户关 mini-term 就 cc-connect 也走 —— 飞书/Telegram 消息断推 |

### C. **sidecar**(Tauri 官方"分发小工具进程"模式)

| 维度 | 详情 |
|---|---|
| 工作方式 | `tauri.conf.json` 的 `bundle.externalBin` 放 `cc-connect.exe`(每平台 `-${TARGET_TRIPLE}.exe`),`npm run tauri build` 时一起打进 installer;Rust `app.shell().sidecar("cc-connect").spawn()` 启动 |
| Plugin | 必须装 `tauri-plugin-shell`(mini-term 当前 Cargo.toml **没装**,只有 tauri-plugin-opener / dialog / clipboard-manager / window-state);capabilities `shell:allow-execute` + `sidecar: true` |
| 优势 | mini-term 安装包自带 cc-connect,用户零额外步骤;tauri 自动处理 child kill 和异步 stdout 流 |
| 劣势 | 1) **跟 cc-connect 升级周期绑死** —— cc-connect 一周一个 beta,mini-term 跟进慢就过期 2) cc-connect 是 npm/brew 分发的独立产品,sidecar bundle 重复打包(npm + sidecar 双份)反人类 3) `externalBin` 必须 host triple 匹配,mini-term build 时要从 cc-connect releases 下载对应平台二进制塞进 binaries/ 4) **跟用户已有的全局 `cc-connect`(npm 全局装)冲突** —— mini-term sidecar 启动的版本可能跟用户 `cc-connect web` 看到的版本不一致,token / config / sessions 路径冲突灾难 |
| 复杂度 | 中(代码层)但 **集成层最高**(每次发版要拉 cc-connect 各平台二进制) |
| 适用场景 | 完全不期望用户单独管理 cc-connect 的产品,mini-term 不是 |

---

## mini-term 现有 spawn 模式

文件:`D:\Git\mini-term\src-tauri\src\pty.rs` + `Cargo.toml`

### 当前依赖(`src-tauri/Cargo.toml`)
- `portable-pty = "0.8"` —— 唯一的 spawn 子进程依赖,**专为 TTY 设计**
- **没有** `tauri-plugin-shell`、`shared_child`、`command-group`、`tokio`(同步线程模型)
- 已有的 tauri plugin:opener、dialog、clipboard-manager、window-state(都不是 spawn 类)

### `pty::create_pty` spawn 流程(`src-tauri/src/pty.rs:604-908`)
1. `native_pty_system().openpty(PtySize{rows:24,cols:80,...})` → `PtyPair{master, slave}`
2. `CommandBuilder::new(&shell).arg(...).cwd(...).env(...)` 拼命令
3. `pair.slave.spawn_command(cmd)` → `Box<dyn Child + Send + Sync>`
4. 单独 thread:`reader.read(buf)` → mpsc::channel<Vec<u8>>
5. 另一个 thread:从 channel 收数据,16ms 批量缓冲,`app.emit("pty-output", ...)`,exit 时 `app.emit("pty-exit", ...)`
6. `PtyInstance { writer, master, child }` 存进 `Arc<Mutex<HashMap<u32, PtyInstance>>>`
7. **kill 路径**(`pty.rs:1036-1045`):后台 thread 里先 `child.kill()` 再 drop,因为 Windows `ClosePseudoConsole()` 同步阻塞等子进程退出,直接 drop 会冻结整个 app

### 能否复用 portable-pty spawn cc-connect

**技术上可以**,但有副作用:
- cc-connect 不需要 TTY(它是 HTTP server),分配 ConPTY (Windows) 是浪费
- ConPTY 会占一对管道句柄,16ms reader 线程持续 read,即使 cc-connect 几乎没 stdout 输出也会跑空轮询
- 现有 reader/flush thread 是为 xterm.js 渲染设计,把后台 service 塞进去前端 UI 要单独跳过这个 pty 不当终端显示 —— 不如直接 `std::process::Command`

### 唯一可直接复用的:**spawn 错误传播 + thread 模型**

代码组织(reader thread + mpsc + flush thread + Arc<Mutex<HashMap>>)对 spawn cc-connect **借鉴 OK**,但不该走同一个 `PtyManager::instances` HashMap —— 应该新建 `CcConnectSupervisor` 单例。

---

## 推荐方案 + 理由

### 推荐:**A (attach-only) 为主 + 微量补充**

**核心逻辑**:mini-term **不当 supervisor**,只做"控制面板 + 探活 + 一键启动"。

理由(强排序):

1. **架构边界清晰**。cc-connect 自带完整 Management API(`/api/v1/status` health probe)+ Web 控制台 SPA(`http://localhost:9820/login?token=...`)。mini-term 复用这套即可,**不需要重新发明 supervision**。
2. **不重复造轮子**。cc-connect 自己的 `cc-connect daemon install` 在 v1.3.3+ Windows 已通过 Task Scheduler 提供 production-grade 的"开机启动 + 崩溃重启 + 永驻"。mini-term 抢这个责任,要么走 sidecar 跟用户全局 cc-connect 撞 config 撞 token,要么走 B 但崩溃恢复 / 端口冲突 / 进程树 kill 这些都得自己写一遍。
3. **避免生命周期冲突**。如果 mini-term 是 supervisor,关 mini-term 就杀 cc-connect → 飞书/Telegram 推送断 → 用户必须开着 mini-term 才能收消息。这违反 cc-connect 的设计意图(后台常驻)。
4. **跟用户既有用法零冲突**。已经 `npm install -g cc-connect && cc-connect daemon install` 的用户,mini-term 上来就能 attach。未装的用户,mini-term 弹"复制此命令到新终端运行:`cc-connect`"按钮,一键创建 pane(`pty::create_pty` 已经能干这事),用户回车,1s 后探活成功即接管。
5. **跟 Windows v1.3.2 兼容**。即使用户装的是 v1.3.2(daemon install 报 unsupported),只要他们能在前台跑 `cc-connect`(npm 装的 binary 直接能跑),9820 一开 mini-term 就能用。daemon install 等 v1.3.3 stable 出来再让用户升级。

### 实现要点

| 模块 | 做法 |
|---|---|
| 发现 cc-connect 实例 | a) 读 `~/.cc-connect/config.toml`(`dirs::home_dir().join(".cc-connect/config.toml")`,mini-term 已依赖 `dirs = "6"` 可直接用)拿到 `[management] port/token` b) probe `http://127.0.0.1:<port>/api/v1/status` 超时 2s c) probe 成功 → 状态 Running;config 存在但 probe 失败 → Installed not running;config 不存在 → 未配置 |
| 启动 cc-connect | UI "Start" 按钮 → `pty::create_pty(shell=cc-connect, cwd=home)` 起个 pane;或调 Tauri command `spawn_cc_connect()` 后台 spawn(`std::process::Command::new("cc-connect").spawn()`,不抓 stdout)。**让用户看到** 启动过程在终端 pane 里,出错时第一时间看到 stderr,比 mini-term 自己吞日志强。 |
| 嵌入 web 控制台 | tauri webview 加载 `http://localhost:9820/login?token=<token>`,9820 SPA 是自适应 layout(5 种语言、暗色主题),原生看就行。**前提**:tauri.conf.json `app.security.csp` 允许 `connect-src http://localhost:9820` 和 `frame-src` |
| 探活 worker | mini-term Rust 端起一个 `thread::spawn` 每 5s probe 一次 status,emit `cc-connect-status` 事件,前端按状态切 UI(stop/start 按钮、链接到 web、错误提示) |
| 平台差异提示 | Windows v1.3.2 用户点 "Install as daemon" 时报错 → mini-term 弹 toast 引导升级到 v1.3.3+;Linux WSL 走 nohup 替代;macOS / Linux 走 daemon install 标准路径 |
| **不做** | sidecar bundle(打包重复)、子进程监管(职责重叠)、修改 cc-connect 的 config(让用户自管,只在用户 explicit 同意时写 token) |

### 弱补充:Optional spawn-with-restart 选项

给少数用户提供 "由 mini-term 拉起并监管" 的开关(默认 off):
- 走 `std::process::Command::new("cc-connect").stdout(Stdio::null()).stderr(Stdio::null()).spawn()`
- Arc<Mutex<Child>> + `thread::spawn(move || child.wait())` 监控
- exit 后指数退避重启(1s → 30s)
- `on_window_event(Destroyed)` 调 `child.kill()`
- **不引入额外 crate**:`std::process::Child::kill` 在 Windows 上 `TerminateProcess` 直接子进程,因为 cc-connect 是 single-process server(`go.mod` 里没 fork shell 调用),够用。需要 kill 整个 process tree 时再引 `command-group`

---

## 风险点

| # | 风险 | 缓解 |
|---|---|---|
| 1 | **用户 v1.3.2 daemon install 报 unsupported** | UI 检测 cc-connect --version < 1.3.3 时灰掉 "Install as daemon" 按钮,提示升级 |
| 2 | **9820 端口被占用** | probe 失败时读 config.toml 看实际配置;如果 `cc-connect web` 自动配置时端口被其他进程占,cc-connect 启动会失败,mini-term 在 pane 里能看到错误 |
| 3 | **config.toml token 读权限** | `~/.cc-connect/config.toml` 默认权限是 0644(launchd.go / systemd.go 见到 MkdirAll 0755),mini-term 进程能读;但跨用户场景(用 mini-term 看其他用户 cc-connect)不支持 —— 不该是常见用法 |
| 4 | **iframe / webview CSP 拦截** | tauri.conf.json 显式 allow `http://localhost:9820`(query 带 token,不要走 cookie);避免 mixed-content(都是 http://) |
| 5 | **多实例 cc-connect**(同时 daemon 和前台手起两个) | cc-connect Windows instance_lock_windows.go 是 no-op(仅写 PID 文件),不会阻止;9820 端口竞争由 OS 处理,后来者失败。UI 只 probe 9820,看到谁就是谁 |
| 6 | **cc-connect API 改版** | management-api.md 标 `v1.1-draft`,后续可能 breaking。mini-term 端集中封装一层 client,版本协商靠 `/api/v1/status.version` |
| 7 | **mini-term spawn 后 cc-connect 卡 stdout buffer** | 如果走可选 spawn 路径,务必 `stdout/stderr Stdio::null()` 或开 reader thread 排空,否则 OS pipe buffer 满后 cc-connect 写阻塞 |
| 8 | **Windows 进程树 kill** | 当前不引 `command-group`;mini-term 关闭只 kill cc-connect.exe 主进程,cc-connect 自己起的 agent 子进程(claude / codex)留给 cc-connect 自己 cleanup;若以后发现孤儿进程,加 `command-group` |
| 9 | **WSL 用户路径** | mini-term WSL pane 启 cc-connect 跑在 WSL VM 内,端口要 forward 到 Windows host 才能 probe。建议直接在 Windows host 跑 cc-connect,避开 WSL bridge |

---

## 引用

### cc-connect 源码(commit `d6654023`,2026-05-28)
- `daemon/windows.go` —— https://raw.githubusercontent.com/chenhg5/cc-connect/main/daemon/windows.go (schtasks Task Scheduler 实现)
- `daemon/unsupported.go` —— https://raw.githubusercontent.com/chenhg5/cc-connect/main/daemon/unsupported.go (build tag `!linux !darwin !windows`)
- `daemon/manager.go` —— https://raw.githubusercontent.com/chenhg5/cc-connect/main/daemon/manager.go (Manager 接口 + Config/Status)
- `daemon/launchd.go` —— https://raw.githubusercontent.com/chenhg5/cc-connect/main/daemon/launchd.go (macOS KeepAlive)
- `daemon/systemd.go` —— https://raw.githubusercontent.com/chenhg5/cc-connect/main/daemon/systemd.go (Linux Restart=on-failure)
- `core/management.go` —— https://raw.githubusercontent.com/chenhg5/cc-connect/main/core/management.go (HTTP Management Server)
- `docs/management-api.md` —— https://raw.githubusercontent.com/chenhg5/cc-connect/main/docs/management-api.md (REST API spec v1.1-draft)
- `cmd/cc-connect/web.go` —— https://raw.githubusercontent.com/chenhg5/cc-connect/main/cmd/cc-connect/web.go (默认 port 9820,EnableWebAdmin 自动生成 token)
- `cmd/cc-connect/daemon.go` —— https://raw.githubusercontent.com/chenhg5/cc-connect/main/cmd/cc-connect/daemon.go (daemon install/uninstall/start/stop CLI)
- `cmd/cc-connect/instance_lock_windows.go` —— Windows lock 是 no-op,只写 PID 文件
- `CHANGELOG.md` —— v1.3.3-beta.3 (2026-05-24) "Windows cross-compile: add missing CheckLinger stub";PR #817 (2026-05-05) "Windows supports daemon install"
- 最新 release:v1.3.3-beta.4 (2026-05-27);最新 stable:v1.3.2 (2026-04-21,**无 Windows daemon 支持**)

### Tauri v2 文档
- Sidecar / externalBin:https://raw.githubusercontent.com/tauri-apps/tauri-docs/v2/src/content/docs/develop/sidecar.mdx
- Shell plugin:https://raw.githubusercontent.com/tauri-apps/tauri-docs/v2/src/content/docs/plugin/shell.mdx
- tauri-plugin-shell 内部依赖:`shared_child = "1"`、`os_pipe = "1"`、`open = "5"`、`regex = "1"`、`encoding_rs = "0.8"`(参见 plugins-workspace/plugins/shell/Cargo.toml)

### Rust spawn 子进程相关 crate
- `shared_child` v1.1.1(2025-07-04,37.5M 下载)—— Tauri shell 内部用,多线程 Child wait/kill — https://github.com/oconnor663/shared_child.rs
- `command-group` v5.0.1(2023-11-18,6.1M 下载)—— watchexec 出品,Process Group/Job Object 管理,kill 整组 — https://github.com/watchexec/command-group
- `portable-pty` 0.9.0 (mini-term 用 0.8) — Wez Furlong 的 wezterm 子项目,跨平台 PTY 抽象,Windows 走 ConPTY — https://docs.rs/portable-pty
- `std::process::Command` —— stdlib,Windows kill 走 TerminateProcess(直接子进程)

### mini-term 当前实现
- `D:\Git\mini-term\src-tauri\Cargo.toml` —— `portable-pty = "0.8"`,无 tauri-plugin-shell,有 tiny_http(自管 hook server)、dirs、windows 0.58
- `D:\Git\mini-term\src-tauri\src\pty.rs` —— `create_pty` (line 604) 走 portable-pty native_pty_system,reader thread + 16ms flush,kill 路径 (line 1036) Windows ClosePseudoConsole 必须后台 drop 避免 freeze
- `D:\Git\mini-term\src-tauri\src\lib.rs` —— Tauri Builder 注册 plugin,无 shell plugin
- `D:\Git\mini-term\CLAUDE.md` —— "前端 React 19 + Tauri v2";"PTY 数据流"段已沉淀 16ms 批量缓冲约定
