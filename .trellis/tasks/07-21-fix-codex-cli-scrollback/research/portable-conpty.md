# portable-pty 0.8.1 侧载 ConPTY 调研

> 调研时间：2026-07-21
> 范围：mini-term 当前锁定的 `portable-pty 0.8.1`；Windows 侧载 `conpty.dll` / `OpenConsole.exe` 的 API、资源布局、加载时机与回退语义。
> 结论来源：本仓库 lockfile、对应 crate 的精确发布提交，以及 Microsoft Terminal 官方源码/发行资产。

## 结论先行

1. mini-term 的 `src-tauri/Cargo.lock` 锁定 `portable-pty 0.8.1`。该版本**没有公开的“指定 conpty.dll 路径”API**。调用现有的 `native_pty_system().openpty(...)` 即可；Windows 实现在第一次创建 PTY 时自动尝试 `LoadLibraryW("conpty.dll")`，成功便使用侧载实现，失败便使用 `kernel32.dll` 的系统 ConPTY。
2. 可直接使用 Microsoft 官方 Windows Terminal release 附带的 `Microsoft.Windows.Console.ConPTY.<version>.nupkg`。截至调研日，官方最新 release `v1.24.11911.0` 附带 `Microsoft.Windows.Console.ConPTY.1.24.260710001.nupkg`，内含 x86/x64/ARM64 三套 `conpty.dll` 和 `OpenConsole.exe`。
3. `conpty.dll` 的架构必须与 **mini-term 进程架构**一致；`OpenConsole.exe` 则应与 **原生 Windows 架构**一致。官方包因此规定：x86 进程带 x86/x64/ARM64 三种 OpenConsole，x64 进程带 x64/ARM64 两种，ARM64 进程只带 ARM64。
4. 不要把“修改子进程 PATH”当成加载方案。`conpty.dll` 在 `openpty` 阶段、shell/Codex spawn 之前由 **mini-term 当前进程**加载；`CommandBuilder::env("PATH", ...)` 只影响之后的 shell 子进程，来不及影响 DLL 选择。
5. PATH 不是 Windows DLL 搜索顺序中的最高优先级。最稳妥的部署是让受信任的 `conpty.dll` 位于应用 EXE 目录，或在首次 `openpty` 前以绝对路径预加载并持有 DLL。实现复审最终采用后者：直接调用 `LoadLibraryW`、检查三个兼容导出、保留 module 引用，不修改进程或子进程 PATH。

## 1. portable-pty 0.8.1 的精确加载 API

mini-term 现有调用不需要变化：

```rust
let pty_system = portable_pty::native_pty_system();
let pair = pty_system.openpty(size)?;
```

Windows 内部调用链是：

```text
native_pty_system().openpty(...)
  -> PsuedoCon::new(...)
  -> 首次解引用 lazy_static CONPTY
  -> load_conpty()
```

`portable-pty 0.8.1` 发布包记录的精确源码提交为 `4afedd626dadd15d9c2929bab0e2063b54f61393`。其 `load_conpty()` 行为是：

```rust
let kernel = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(...);
if let Ok(sideloaded) = ConPtyFuncs::open(Path::new("conpty.dll")) {
    sideloaded
} else {
    kernel
}
```

动态解析的兼容入口为 `CreatePseudoConsole`、`ResizePseudoConsole`、`ClosePseudoConsole`。Microsoft 当前官方 `conpty.dll` 的 `.def` 文件仍明确导出这三个兼容别名，因此可被 `portable-pty 0.8.1` 直接加载；官方同时说明新消费者应使用带 `Conpty*` 前缀的新入口，但这不影响旧兼容入口。

`CONPTY` 是进程级 `lazy_static`：一旦第一次创建 PTY 完成选择，之后不会因为复制文件或修改 PATH 而重新选择，必须重启 mini-term。

来源：

- [portable-pty 0.8.1 精确提交：`pty/src/win/psuedocon.rs`](https://github.com/wezterm/wezterm/blob/4afedd626dadd15d9c2929bab0e2063b54f61393/pty/src/win/psuedocon.rs#L27-L63)
- [portable-pty 0.8.1 Windows `openpty` 调用 `PsuedoCon::new`](https://github.com/wezterm/wezterm/blob/4afedd626dadd15d9c2929bab0e2063b54f61393/pty/src/win/conpty.rs#L10-L31)
- [Microsoft Terminal v1.24.11911.0 的 `winconpty.def` 兼容导出](https://github.com/microsoft/terminal/blob/v1.24.11911.0/src/winconpty/dll/winconpty.def)

## 2. 官方资源与正确目录树

官方 release 的 NuGet 包结构（已实际下载并核对）：

```text
Microsoft.Windows.Console.ConPTY.1.24.260710001.nupkg
├─ runtimes/win-x86/native/conpty.dll
├─ runtimes/win-x64/native/conpty.dll
├─ runtimes/win-arm64/native/conpty.dll
├─ build/native/runtimes/x86/OpenConsole.exe
├─ build/native/runtimes/x64/OpenConsole.exe
└─ build/native/runtimes/arm64/OpenConsole.exe
```

运行时应整理成下面的布局，其中 `<load-dir>` 是实际被 `LoadLibraryW("conpty.dll")` 找到的目录：

```text
<load-dir>/conpty.dll
<load-dir>/x86/OpenConsole.exe
<load-dir>/x64/OpenConsole.exe
<load-dir>/arm64/OpenConsole.exe
```

`conpty.dll` 自己按以下顺序找 host：

1. 先找与 DLL 同目录的 `<load-dir>/OpenConsole.exe`；
2. 不存在时，用 `IsWow64Process2` 判断原生机器架构，找 `<load-dir>/<x86|x64|arm64>/OpenConsole.exe`；
3. 仍不存在时，退回 `%SystemRoot%/System32/conhost.exe`。

因此有两种合法打包方式：

- **单一、确定的原生架构安装包**：`conpty.dll` 与正确架构的 `OpenConsole.exe` 同目录。
- **一个进程架构兼容多种原生 Windows 架构**：按官方 NuGet targets 使用架构子目录。官方约束为：
  - x86 mini-term：x86 `conpty.dll`，以及 `x86/`、`x64/`、`arm64/` 三个 host；
  - x64 mini-term：x64 `conpty.dll`，以及 `x64/`、`arm64/` 两个 host；
  - ARM64 mini-term：ARM64 `conpty.dll`，以及 `arm64/` host。

同目录存在一个错误架构的 `OpenConsole.exe` 时，DLL 会先选中它，并不会再尝试架构子目录；因此不要把任意架构 host 无条件平铺到根目录。`conpty.dll` 和所有 `OpenConsole.exe` 应取自同一个、固定版本的官方包；官方源码没有发现版本匹配校验，混用版本的行为**未确认且不应依赖**。

来源：

- [Microsoft Terminal v1.24.11911.0 官方 release（含 ConPTY nupkg）](https://github.com/microsoft/terminal/releases/tag/v1.24.11911.0)
- [官方 NuGet nuspec：三架构资源清单及 Win10 10.0.17763+ 说明](https://github.com/microsoft/terminal/blob/v1.24.11911.0/src/winconpty/package/winconpty.nuspec#L5-L33)
- [官方 props：不同进程架构必须携带哪些 OpenConsole host](https://github.com/microsoft/terminal/blob/v1.24.11911.0/src/winconpty/package/managed/Microsoft.Windows.Console.ConPTY.props#L3-L23)
- [官方 targets：`conpty.dll` 复制到输出根，host 复制到架构子目录](https://github.com/microsoft/terminal/blob/v1.24.11911.0/src/winconpty/package/native/Microsoft.Windows.Console.ConPTY.targets#L11-L22)
- [官方 `conpty.dll`：同目录、架构子目录、系统 conhost 的选择顺序](https://github.com/microsoft/terminal/blob/v1.24.11911.0/src/winconpty/winconpty.cpp#L35-L96)

## 3. PATH / 环境加载语义

`portable-pty 0.8.1` 的间接依赖 `shared_library 0.1.9` 最终对裸文件名调用 `LoadLibraryW`。Microsoft 文档规定：没有完整路径时使用标准 DLL 搜索策略。默认启用 Safe DLL Search Mode 的 unpackaged app 中，关键顺序是：已加载模块 / Known DLLs 等特殊项 → 应用 EXE 所在目录 → System32 → Windows 目录 → 当前目录 → PATH。也就是说：

- 把资源目录**前置到 PATH**只保证它排在其他 PATH 条目前面，不会超过应用目录、System32、已加载模块等更早位置；“PATH 优先加载”不是严格表述。
- 若 `conpty.dll` 与 `Mini-Term.exe` 同目录，不必改 PATH。
- 若 DLL 位于 Tauri 资源子目录，必须在**首次 `openpty` 前**让当前 mini-term 进程可搜索到该目录；仅给 shell/Codex 的 `CommandBuilder` 注入 PATH 无效。
- `OpenConsole.exe` 不通过 PATH 搜索，而是由已加载的 `conpty.dll` 根据自己的模块目录拼出绝对候选路径。
- DLL 搜索目录必须是随应用安装且普通用户不可任意替换的受信任目录；Microsoft 明确警告攻击者可通过可写搜索目录实施 DLL preloading。

来源：

- [`shared_library 0.1.9` Windows 实现最终调用 `LoadLibraryW`](https://docs.rs/crate/shared_library/0.1.9/source/src/dynamic_library.rs)
- [Microsoft `LoadLibraryW`：相对路径/裸模块名使用标准搜索策略](https://learn.microsoft.com/en-us/windows/win32/api/libloaderapi/nf-libloaderapi-loadlibraryw)
- [Microsoft DLL 搜索顺序与安全警告](https://learn.microsoft.com/en-us/windows/win32/dlls/dynamic-link-library-search-order#standard-search-order-for-unpackaged-apps)

## 4. 回退语义（实现时必须区分）

| 场景 | 实际行为 | 是否会再试系统实现 |
|---|---|---|
| 系统 `kernel32.dll` 不导出三个 ConPTY 入口 | `expect` 直接失败；侧载 DLL 甚至不会被尝试 | 否。侧载不能把 Win10 下限降到 1809 以下 |
| 找不到 `conpty.dll`、DLL 架构不匹配、加载失败、缺少兼容导出 | `ConPtyFuncs::open` 返回错误，portable-pty 静默选择 `kernel32.dll` | 是 |
| `conpty.dll` 已加载，但找不到任何 `OpenConsole.exe` 候选 | 侧载 DLL 内部改用 `%SystemRoot%/System32/conhost.exe` | 不是切回 kernel 函数，但 host 已退回系统版本，可能重现机器差异 |
| 找到 `OpenConsole.exe`，但其架构错误/损坏/无法启动 | 候选只按“文件存在”选中；`CreatePseudoConsole` 返回失败 HRESULT，`openpty` 失败 | 未发现再次尝试系统 conhost/kernel 的逻辑 |
| 首次 PTY 已选择系统或侧载 DLL，随后才补文件/改 PATH | `lazy_static` 已缓存选择 | 否，需重启进程 |

`portable-pty` 对侧载失败没有自带诊断日志，也没有返回“本次使用系统/侧载”的公开状态。因此实施时应在 mini-term 自己的启动日志中记录：选定资源目录、预期 DLL/host 是否存在、目标架构，以及是否在首次 PTY 前完成加载准备。若要百分之百确认运行中采用哪个 host，可检查实际启动的 `OpenConsole.exe`/`conhost.exe` 进程；如何在当前项目中自动化该检查，**本调研未验证**。

## 5. 给 mini-term 实现阶段的最小建议

1. 固定一个经过测试的 `Microsoft.Windows.Console.ConPTY` 版本，不要在用户机器启动时下载 latest。
2. 按 Tauri target 构建时从同一 nupkg 选择匹配进程架构的 `conpty.dll`，并按上表携带 host。
3. 优先让 `conpty.dll` 处于 `Mini-Term.exe` 目录；若 Tauri 打包规则只能放资源目录，则在应用初始化、任何 `openpty` 之前准备当前进程加载路径。不要改 shell 子进程 PATH 来解决。
4. 启动前检查完整资源树并记日志；缺任一必需 host 时宁可显式告警，避免“看似侧载成功、实际又回到系统 conhost”。
5. 保留 portable-pty 原生 kernel 回退，以便资源损坏时终端仍可用；但 side DLL 已载入而 host 损坏属于 `openpty` 错误，需由 mini-term 额外决定是否回退。portable-pty 本身没有第二次回退 API。

## 本调研未确认事项

- 当前 Tauri 2 Windows MSI/NSIS 对 `bundle.resources` 的最终落盘位置和写权限；需要实现代理结合本仓库打包产物验证。
- 显式以绝对路径预加载 `conpty.dll` 后，再由 portable-pty 裸名加载的代码路径已按 Windows loaded-module 搜索顺序实现并通过构建；原故障机器上的端到端滚动效果仍需人工验收。
- Windows Terminal `v1.24.11911.0` 是否正是解决截图所示 Codex ED2/ED3 scrollback 差异的最小版本。调研只确认了侧载机制与官方最新资产，没有建立具体修复提交与版本下限。
