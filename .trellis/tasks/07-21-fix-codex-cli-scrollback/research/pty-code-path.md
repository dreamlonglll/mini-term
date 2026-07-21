# Windows PTY / portable-pty / Codex CLI 现有链路研究

> 研究范围：只读检查本仓库与本机已解析依赖源码；未修改业务代码。结论对应当前工作区与 `src-tauri/Cargo.lock` 锁定的依赖版本。

## 结论摘要

mini-term 的 PTY 创建入口集中在 `src-tauri/src/pty.rs::create_pty`。所有前端本地、WSL 和 SSH 项目最终都调用同一个 Tauri command；该命令在解析 WSL/SSH 启动分支之前就执行 `portable_pty::native_pty_system().openpty(...)`。Windows 下，锁定的 `portable-pty 0.8.1` 将 native backend 固定为 `ConPtySystem`，并在第一次 `openpty()` 时优先按文件名加载 `conpty.dll`，加载失败才使用 `kernel32.dll` 中的系统 ConPTY。

因此最小接入边界是：

1. 打包侧：在 `src-tauri/tauri.conf.json` 的 `bundle.resources` 增加同架构 `conpty.dll` + `OpenConsole.exe`；当前配置没有 `resources`。
2. 运行时：在 `src-tauri/src/pty.rs::create_pty` 第 804 行 `native_pty_system()` / 第 805 行 `openpty()` 之前，从 `app.path().resource_dir()` 解析并校验这一对文件，再使该目录进入 `LoadLibraryW("conpty.dll")` 和 `OpenConsole.exe` 的查找路径。
3. 完整回退：资源缺失或 `conpty.dll` 加载失败时，上游已有 kernel32 回退；但 side-loaded DLL 已加载而 `CreatePseudoConsole` 初始化失败时，上游全局惰性实例无法切回 kernel32，需要额外封装或修补依赖，不能仅靠再次调用 `native_pty_system()` 实现。

## 现有端到端调用链

### 1. 前端创建 PTY

- `src/components/PaneGroup.tsx:57-117`：pane 尚无 `ptyId` 时调用 `createProjectPty(project, shell)`；新 tab 也在 `PaneGroup.tsx:119-147` 调同一函数。
- `src/components/TerminalArea.tsx:68,121`：其他创建入口也复用 `createProjectPty`。
- `src/utils/remoteProject.ts:40-64`：
  - 普通本地项目调用 `invoke('create_pty', { shell, args, cwd, envs })`；
  - SSH 远程项目仍调用同一个 `create_pty`，只是传 `sshRemote`，后端再把实际子进程改为 `ssh`。

前端没有 Codex 专用的 PTY 创建路径，也没有后端选择参数。

### 2. Rust 创建 PTY

`src-tauri/src/pty.rs:787-1137` 是唯一的 `create_pty` 实现；`src-tauri/src/lib.rs` 将其注册到 Tauri invoke handler。

关键顺序如下：

1. `pty.rs:797-802`：若有 `ssh_remote`，先准备 SSH 启动参数。
2. `pty.rs:804-812`：无条件调用 `native_pty_system().openpty(...)`，初始尺寸为 80x24。
3. `pty.rs:814-852`：openpty 之后才判定 WSL override / SSH / 普通本地 shell，并得到 `effective_shell/effective_args/effective_cwd`。
4. `pty.rs:854-950`：创建 `CommandBuilder`，设置 shell 参数、cwd、TERM/LANG、hook 环境与项目环境变量。
5. `pty.rs:951`：`pair.slave.spawn_command(cmd)` 将最终 shell、`wsl.exe` 或 `ssh.exe` 放入已创建的 ConPTY。
6. `pty.rs:953-1137`：取得 reader/writer，后台线程按 16ms 合并输出，经 Tauri `pty-output` 事件发送给前端；实例保存 master/child/writer。

输入、resize、销毁仍走同一 master：

- `pty.rs:1174-1213`：`write_pty` 写入 writer。
- `pty.rs:1215-1241`：`resize_pty` 调 `MasterPty::resize`。
- `pty.rs:1243-1287`：`kill_pty` 先 kill child，再在线程中 drop master；注释明确 Windows drop 会调用同步的 `ClosePseudoConsole()`。

### 3. 输出进入 xterm.js

- `src/utils/terminalCache.ts:226-242`：全局监听 `pty-output`，用 `entry.term.write(data)` 写入对应 xterm。
- `terminalCache.ts:263-291`：创建 xterm，当前 `scrollback: 100000`。
- `terminalCache.ts:301-320`：当前代码已经拦截 ED3（CSI 3J），并阻止 DECSET/DECRST 47、1047、1049 进入备用缓冲区。这不是待接入便携 ConPTY 的位置；它位于 ConPTY 输出之后。PRD 已将继续改写 ANSI 序列列为 Out of Scope。
- `terminalCache.ts:358-375`：xterm `onData` 回写 `write_pty`，resize 回写 `resize_pty`。

## 当前 Windows backend 的确切选择逻辑

### 仓库依赖

- `src-tauri/Cargo.toml` 声明 `portable-pty = "0.8"`。
- `src-tauri/Cargo.lock:4097-4116` 锁定 `portable-pty 0.8.1`。
- 仓库业务源码只有 `pty.rs:804` 一处调用 `native_pty_system()`。

### portable-pty 0.8.1 一手源码

本机 Cargo registry 源码：

`C:/Users/12197/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/portable-pty-0.8.1/`

- `src/lib.rs:389-396`：`native_pty_system()` 返回 `NativePtySystem::default()`；Windows 的 `NativePtySystem` 是 `win::conpty::ConPtySystem`。没有运行时 WinPTY/其他后端选择。
- `src/win/conpty.rs:12-43`：`ConPtySystem::openpty` 建输入/输出 pipe，然后调用 `PsuedoCon::new`。
- `src/win/psuedocon.rs:44-59`：先确认 `kernel32.dll` 导出 ConPTY 函数；随后优先 `ConPtyFuncs::open(Path::new("conpty.dll"))`，失败才返回 kernel32 实现。注释明确 side-loaded `conpty.dll` 与 `openconsole.exe` 应随应用部署。
- `src/win/psuedocon.rs:61-63`：选中的函数表保存在进程级 `lazy_static CONPTY`，初始化后不会重新选择。
- `src/win/psuedocon.rs:78-95`：`PsuedoCon::new` 通过选中的函数表调用 `CreatePseudoConsole`。
- `src/win/psuedocon.rs:110-171`：最终用 `CreateProcessW` 把 shell 子进程附着到该 pseudo console。

`portable-pty 0.8.1` 没有公开“传入 conpty.dll/OpenConsole 绝对路径”或“强制 system ConPTY”的 API。

其依赖 `shared_library 0.1.9` 的 Windows 实现（`src/dynamic_library.rs:323-345`）把传入文件名直接交给 `LoadLibraryW`；因此 `Path::new("conpty.dll")` 依赖 Windows DLL 搜索路径，而不是仓库 cwd 或 Tauri resource API。

## 打包与运行时资源现状

### Tauri 配置

`src-tauri/tauri.conf.json` 当前：

- `bundle.externalBin` 仅包含 `binaries/mt-ssh-mcp` 与 `binaries/miniterm-hook`；
- 没有 `bundle.resources`；
- 没有 ConPTY/OpenConsole 目录或资源声明。

当前 sidecar staging 在 `scripts/stage-sidecars.mjs`：按目标 triple 构建两个 sidecar，发布时复制到 `src-tauri/binaries/<name>-<triple>.exe`，dev 时额外复制裸名到 `src-tauri/target/debug/`。该脚本目前完全不处理 ConPTY/OpenConsole，但它已经接收 `--target`，是按架构 staging 资源时可复用的构建接缝。

Tauri 2.11.1 / tauri-utils 2.9.1 一手源码显示：

- `tauri-utils/src/config.rs:1585-1629`：`bundle.resources` 支持文件、目录、glob 和 source→target map；资源复制到 `$RESOURCE`，map 可精确控制目标路径。
- `tauri-utils/src/platform.rs:249-301`：Windows 的 resource directory 是主程序 exe 所在目录；不要把这一当前实现当成业务硬编码，应仍通过 Tauri path resolver 获取。
- `tauri/src/path/desktop.rs:219-232`：运行时公开 `app.path().resource_dir()`。

`src-tauri/src/pty.rs` 已收到 `AppHandle`，但当前只导入 `tauri::{AppHandle, Emitter}`；若使用 `app.path()`，实现阶段需按 Tauri trait 要求引入 `tauri::Manager`（是否为当前版本的必要编译导入应由实现/编译确认）。

### 发布架构

`.github/workflows/release.yml:13-19` 当前 Windows release matrix 只有 `x86_64-pc-windows-msvc`；macOS 为 arm64、Linux 为 x64。也就是说当前实际发布支持的 Windows CPU 架构只有 x64。若未来把“支持架构”扩到 x86/arm64，CI matrix、资源 staging 与匹配校验必须一起增加；仓库目前没有这三架构的既有支持证据。

## 最小接入点与边界

### A. 打包接入点

首选修改面：

- `src-tauri/tauri.conf.json`：增加 `bundle.resources`，稳定产出同目录的 `conpty.dll` 与 `OpenConsole.exe`，或产出稳定的 `portable-conpty/<arch>/` 子目录。
- `scripts/stage-sidecars.mjs` 或独立 staging 脚本：按 `--target` 选择匹配架构，并在 Tauri build 前校验两文件齐全。现有 CI 在 `release.yml:83-87` 先 staging sidecars、后 build Tauri，可直接扩展这一流水线接缝。

若资源直接映射到 Windows `$RESOURCE` 根目录，当前 Tauri 上它与主 exe 同目录，更符合 portable-pty 上游“alongside application”的默认 LoadLibrary 设计；若使用架构子目录，则运行时必须显式改变 DLL/OpenConsole 搜索路径。

### B. 运行时接入点

唯一且最小的位置是 `src-tauri/src/pty.rs:803-812`，即 `native_pty_system/openpty` 之前。建议把路径解析、配对校验、诊断与 backend 尝试封装成一个 Windows-only helper，`create_pty` API 和前端 payload 无需变化。

必须在 `openpty()` 前准备搜索路径，因为 `CONPTY` 的惰性初始化发生在 `PsuedoCon::new` 内；在 `spawn_command` 前做已经太晚。

`CommandBuilder::new` 位于 `pty.rs:854`，晚于 `openpty`。上游 `cmdbuilder.rs:72-84,197-212` 显示它在构造时快照进程环境。因此如果实现采用临时 PATH 策略，并能在构造 `CommandBuilder` 前恢复原 PATH，shell 子进程可继续快照原始 PATH；但进程环境修改是全局状态，多个并发 PTY 创建及其他并发 spawn 的竞态仍需专门处理，不能只做无锁 `set_var`/恢复。

### C. WSL / SSH 边界

当前 openpty 发生在 WSL/SSH/本地分支选择之前。因此：

- 只在第 804 行前全局准备便携 ConPTY，会让普通 shell、`wsl.exe` 和本地 `ssh.exe` 三者都由同一个便携 Windows PTY host 承载；
- shell、cwd、参数、env 注入逻辑仍可保持不变，但不能准确宣称“仅普通本地 shell 使用便携 backend”；
- 若 PRD 的“仅本地 Windows PTY”意指排除 WSL/SSH，必须把分支判定提前，或提供可按实例选择的 backend。当前源码没有这种 seam。

此处产品语义需实现阶段按 PRD 解释确认；研究未替用户扩大范围。

## 回退能力与已确认限制

### 已由上游覆盖

- 找不到 `conpty.dll`、DLL 无法加载或缺少需要的导出时，`ConPtyFuncs::open` 返回 Err，`load_conpty()` 使用先前加载的 kernel32 函数表。
- 因此“资源不存在/明显不完整时不要把目录加入搜索路径”加上上游 load fallback，可覆盖最基础的缺失回退。

### 不能仅靠当前 API 覆盖

如果 `conpty.dll` 成功加载，但 `OpenConsole.exe` 缺失、版本/架构不匹配导致 `CreatePseudoConsole` 返回失败，`openpty()` 会返回 Err；由于 `CONPTY` 已被 `lazy_static` 固定为 side-loaded 函数表，再调用 `native_pty_system().openpty()` 仍使用同一 DLL，不会改走 kernel32。

要满足 PRD 的“初始化失败也自动回退”，实现需要以下一种额外机制，具体方案尚未由本研究确认：

- fork/patch portable-pty，使 Windows backend 可显式选择绝对路径 side-loaded 实现与 system 实现；或
- 在触发上游全局初始化前完成足够强的资源/架构/版本/可启动性验证，并把剩余初始化失败视为不可恢复（这仍不完全满足 PRD）；或
- 采用其他能够隔离 side-loaded 初始化的 backend/进程方案。

不要把“捕获第一次 openpty Err 后再调用 native_pty_system”写成回退，它在 0.8.1 的全局函数表设计下无效。

## 实现阶段应保持不变的路径

- Tauri command 名称与前端参数：`create_pty(shell,args,cwd,envs,sshRemote)`。
- `effective_shell/effective_args/effective_cwd` 的 WSL、SSH 与普通本地分支。
- TERM/LANG/MINITERM/项目 env 注入顺序。
- reader/writer、16ms output batching、`pty-output`/`pty-exit` 事件。
- xterm `scrollback: 100000` 及现有前端 parser handler（本任务 PRD 明确不改 ANSI 序列）。

## 未确认项

- 未检查或选择具体 Microsoft Terminal / NuGet 资源版本、许可证与下载 URL；由 `research/portable-conpty.md` 负责。
- 未在真实 dev/build/install 产物中验证 Tauri resources 的最终文件布局；这里只依据当前锁定 Tauri 源码确定 resolver 与 bundler 语义。
- 未确认 `OpenConsole.exe` 内部查找是否只依赖 PATH、DLL sibling directory，或还有版本特定规则；因此资源对必须同目录，并由专门的 portable-conpty 研究给出最终装载策略。
- 未执行任何真实 portable ConPTY 初始化或 Codex CLI 人工复现。

## 关键文件清单

| 文件 | 作用 | 与本任务的关系 |
|---|---|---|
| `src/utils/remoteProject.ts` | 统一前端 `create_pty` payload | 证明本地/SSH 共用入口 |
| `src/components/PaneGroup.tsx`、`TerminalArea.tsx` | pane/tab 创建 | 上游调用者 |
| `src-tauri/src/pty.rs` | PTY 创建、I/O、resize、kill | 运行时最小接入点 |
| `src-tauri/src/lib.rs` | Tauri state/setup/command 注册 | 可选的一次性初始化位置，但当前 `AppHandle` 已传给 `create_pty` |
| `src/utils/terminalCache.ts` | xterm 配置、PTY 事件分发 | 证明前端已有 100000 scrollback 与 ANSI handler |
| `src-tauri/Cargo.toml` / `Cargo.lock` | portable-pty 依赖与精确版本 | 锁定 0.8.1 行为 |
| `src-tauri/tauri.conf.json` | bundle 配置 | 当前缺 resources；打包接入点 |
| `scripts/stage-sidecars.mjs` | target-aware staging | 可复用架构选择/校验接缝 |
| `.github/workflows/release.yml` | 发布矩阵 | 当前 Windows 仅 x86_64 |
