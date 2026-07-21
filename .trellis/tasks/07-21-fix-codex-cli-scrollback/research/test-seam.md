# 便携 ConPTY：最小、确定的回归测试切入点

## 结论

建议把本需求拆成两个测试面，并用一条命令串起来：

1. `tests/conptyBundle.test.cjs`：验证 **要打包的资源是完整且架构正确的**；
2. `src-tauri/src/conpty_bootstrap.rs` 内的 `#[cfg(test)] mod tests`：验证 **启动计划优先选择便携目录，资源不完整或探测失败时走系统 ConPTY，且全过程不修改 PATH**。

反馈环命令：

```powershell
node --test tests/conptyBundle.test.cjs && cargo test --manifest-path src-tauri/Cargo.toml --lib conpty_bootstrap
```

测试文件尚未创建时，这条命令立即为红；先落下面列出的测试，再实现资源 staging、Tauri resources 和启动初始化，同一命令应转绿。完成后可在 `package.json` 增加同内容的 `test:conpty` 脚本，日常只跑 `npm run test:conpty`。

## 已确认的仓库事实

- 当前 `package.json` 只有前端 build 和 sidecar staging，没有 test 脚本（[package.json](../../../../package.json#L6-L12)）。现有根级测试均采用 `tests/*.test.cjs` + Node test runner，因此 bundle 契约测试放 `tests/conptyBundle.test.cjs` 与仓库习惯一致。
- 当前 Tauri bundle 只声明两个 `externalBin`，尚未声明 `bundle.resources`（[tauri.conf.json](../../../../src-tauri/tauri.conf.json#L30-L43)）。Tauri 2 schema 明确：`bundle.resources` 可声明文件/目录或 source→target map，资源按目标目录复制；官方入口为 [Tauri Resources](https://v2.tauri.app/develop/resources/)。
- Windows 下 Tauri `resource_dir()` 是主程序 exe 所在目录；因此运行时可以从 `app.path().resource_dir()` 解析便携资源，不要依赖当前工作目录。该行为来自本仓锁定的 Tauri 2.11.1 源码/API。
- 发布流水线先执行 `stage-sidecars.mjs --release --target <triple>`，再执行 Tauri build（[release.yml](../../../../.github/workflows/release.yml#L80-L96)）。ConPTY staging 应复用这个确定的 target 输入，而不是在测试或运行时猜测机器架构。
- 当前首个 ConPTY 实际使用点是 `create_pty` 中的 `native_pty_system().openpty(...)`（[pty.rs](../../../../src-tauri/src/pty.rs#L783-L812)）；Tauri 启动初始化入口是 `lib.rs` 的 `.setup(...)`（[lib.rs](../../../../src-tauri/src/lib.rs#L31-L68)）。便携路径准备必须发生在任何 `openpty` 之前。
- 本仓锁定 `portable-pty 0.8.1`（[Cargo.lock](../../../../src-tauri/Cargo.lock#L4097-L4106)）。其 Windows 实现先加载 `kernel32.dll`，随后用裸名尝试 `conpty.dll`；裸 DLL 加载成功则使用侧载版本，加载失败则返回 kernel32 的系统实现（[portable-pty 0.8.1 源码](https://github.com/wezterm/wezterm/blob/portable-pty-0.8.1/pty/src/win/psuedocon.rs#L44-L63)）。这就是“便携优先、加载失败回退系统”的上游边界。

## 最小测试清单

### A. `tests/conptyBundle.test.cjs`：资源/打包契约

只测源资源和配置，不构建 MSI/NSIS，保持在秒级。

1. `tauri.conf.json`（或 Windows 专用 Tauri config）必须把 staging 目录映射到约定的 resource 子目录，例如 `conpty/`。
2. 对声明支持的每个 Windows target，staging 后必须同时存在：
   - `conpty.dll`
   - `OpenConsole.exe`
3. 两个文件必须非空；再读取 PE header 的 `Machine` 字段，防止“文件齐了但架构放错”：
   - `i686-pc-windows-msvc` → `0x014c`
   - `x86_64-pc-windows-msvc` → `0x8664`
   - `aarch64-pc-windows-msvc` → `0xaa64`
4. staging 对不支持的 target 必须显式跳过或报可识别错误，不能静默复制错误架构。

这组测试的预期红绿非常明确：缺任一文件、文件为 0、PE 架构不匹配、资源未进 Tauri config 都红；四项同时满足才绿。

> 若实现选择“仓库只保存原始 nupkg，构建时解包”，测试应先对一个临时 staging 目录调用可导出的 staging 函数，再检查产物；不要依赖开发机上已经存在的 `src-tauri/target` 或 `src-tauri/binaries`。

### B. `src-tauri/src/conpty_bootstrap.rs`：运行时选择契约

建议新建小模块，不把更多启动细节塞进已经很大的 `pty.rs`。把副作用拆成“纯计划 + 启动时应用”：

- `resolve_conpty_dir(resource_dir, target_arch) -> Option<PathBuf>`：只有 DLL/EXE 成对存在且架构目录匹配才返回；
- `choose_conpty_bootstrap(..., probe) -> Portable(path) | System(reason)`：`probe` 用闭包/trait 注入，单测不真正加载 DLL，也不修改进程全局 PATH。

至少写以下 6 个单测：

1. 完整资源 + probe 成功 → `Portable`；
2. 选择 `Portable` 前后进程 PATH 完全不变；
3. 缺 `conpty.dll` → `System`；
4. 缺 `OpenConsole.exe` → `System`；
5. probe 返回加载失败 → `System`；
6. x86/x64/arm64 target→目录映射正确，未知架构 → `System`。

不要在生产代码或并行 Rust 单测中 `set_var("PATH", ...)`：PATH 是进程全局可变状态，会改变无关子进程并造成竞态。生产代码在 Tauri `.setup(...)` 最前面以绝对路径预载 DLL，并永久持有 module 引用；后续裸名加载按 Windows 已加载模块规则命中它。

### C. 启动顺序的最低成本验收

在 `lib.rs::.setup` 的第一项 Windows 初始化调用 bootstrap，并让 bootstrap 返回可记录的 `Portable(path)` / `System(reason)`。代码审查需要确认它位于配置迁移、hook server、process monitor 之前，更关键的是位于任何 `create_pty/openpty` 之前。

自动化层面，“选择过程不改变 PATH”由 B-2 给出确定结果；“资源确实被 bundle”由 A-1/A-2 给出确定结果。无需为了验证调用顺序拉起 WebView/Tauri GUI。

## 回退边界（必须写进 PRD/验收）

`portable-pty 0.8.1` 的自动回退仅发生在 **裸名 `conpty.dll` 无法加载** 时；如果 DLL 已成功加载，但随后 `CreatePseudoConsole` 返回失败，源码会直接返回 `openpty` 错误，并不会第二次改用 kernel32。

因此，本任务可以确定承诺并测试的是：

- 资源缺失/不完整 → 不注入便携目录 → 系统 ConPTY；
- DLL 预探测失败（错误架构、依赖缺失、导出不全）→ 不注入便携目录 → 系统 ConPTY；
- 注入后 `portable-pty` 自身裸 DLL 加载失败 → 上游回退 kernel32。

“便携 DLL 已加载但 `CreatePseudoConsole` 运行失败后仍自动回退”目前 **未被 portable-pty 0.8.1 支持**。若 PRD 要求覆盖这一类失败，需要换/patch PTY 后端或做进程隔离，不属于最小改动，也不能用上述纯函数测试假装已经保证。

## 尚未确认

- 便携 ConPTY 的最终来源包、固定版本、SHA-256、签名/许可证，以及每个 nupkg 内的准确文件路径尚未确认；bundle 测试应在来源确定后把版本和哈希加入契约。
- 当前 release matrix 的 Windows 只构建 `x86_64-pc-windows-msvc`；截图提到 x86/x64/arm64，但仓库是否本次就要发布三种 Windows target 尚未确认。若仍只发 x64，最小发布测试只需 x64；staging 的三架构映射单测仍可保留。
- 未实际解包最终 MSI/NSIS 做安装后文件检查。最小方案通过 staging + Tauri resources config 建立确定反馈；若发布门禁要求证明安装包内容，再在 Windows release job 增加一次安装包解包/安装后 smoke test，作为第二层而非本地红绿环。
