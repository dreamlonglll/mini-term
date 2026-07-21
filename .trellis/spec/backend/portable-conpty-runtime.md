# Windows 便携 ConPTY 运行时契约

> 当前 Windows 发布目标为 x64。mini-term 固定并侧载 Microsoft 官方 ConPTY，
> 用同一版本的 `conpty.dll` / `OpenConsole.exe` 消除不同 Windows build 的 TUI 行为差异。

## 1. Scope / Trigger

修改 Windows PTY 创建、Tauri resources、release target、sidecar staging 或
`portable-pty` 版本时都必须检查本契约。资源链路是：

```text
Microsoft release nupkg → stage-conpty.mjs → Tauri Windows resources
→ app.path().resource_dir() → setup 首项预检/预载 → 第一次 openpty()
```

当前只支持 `x86_64-pc-windows-msvc`。新增 x86/ARM64 应用目标前，必须显式扩充
release matrix、staging 映射、PE 校验和运行时架构策略；不能让未知架构选到 x64 DLL。

## 2. Signatures

构建侧入口：

```js
stagePortableConpty({ target, cacheDir?, outputDir? })
stagePortableConptyFromDirectory({
  target, packageRoot, outputDir, verifyOfficialHashes?
})
validatePortableConptyLayout(outputDir, { verifyOfficialHashes? })
```

运行时纯决策与副作用边界：

```rust
fn choose_conpty_bootstrap<F>(
    resource_dir: &Path,
    target_arch: &str,
    probe: F,
) -> ConptyBootstrapDecision;

#[cfg(windows)]
fn initialize(app: &tauri::AppHandle) -> ConptyBootstrapDecision;
```

`initialize` 必须是 `lib.rs::.setup(...)` 的第一项，早于配置迁移、hook server、
process monitor，以及任何 `native_pty_system().openpty(...)`。整个初始化由进程级
`OnceLock` 保护；重复或并发调用只返回首次决策，不能重复加载或改变运行时状态。

## 3. Contracts

固定来源为 Microsoft Terminal release `v1.24.11911.0` 中的
`Microsoft.Windows.Console.ConPTY.1.24.260710001.nupkg`，包 SHA-256：

```text
9382ad7becb7e4d84e300578d8e4f4df28f43d979d9055d978c42913c47e0e9d
```

x64 应用的 resource tree 必须完整且来自同一包：

```text
portable-conpty/conpty.dll                 PE 0x8664 (x64)
portable-conpty/x64/OpenConsole.exe        PE 0x8664 (x64)
portable-conpty/arm64/OpenConsole.exe      PE 0xaa64 (ARM64)
```

ARM64 host 不是 ARM64 应用发布物；它是 x64 进程运行于 ARM64 原生 Windows 时，
官方 `conpty.dll` 按 `IsWow64Process2` 选择的 host。不要在资源根目录平铺
`OpenConsole.exe`，否则 DLL 会先选根文件并绕过架构子目录。

运行时使用 `app.path().resource_dir()`，不得依赖 cwd。资源完整后以绝对路径
`LoadLibraryW` 预载 DLL，并检查 `CreatePseudoConsole`、`ResizePseudoConsole`、
`ClosePseudoConsole` 三个兼容导出；成功的 module 引用保留到进程退出。Windows
`LoadLibraryW` 对裸模块名会先检查已加载模块，因此 portable-pty 随后的裸名加载会
命中已预载 DLL，不需要修改进程 PATH。local shell、WSL、SSH 以及其他子进程均继承
原有环境；项目级用户 PATH 仍按既有“后入覆盖”规则生效。

`portable-pty 0.8.1` 的回退边界：仅在侧载 DLL **尚未被选中**时可回退系统
`kernel32.dll`。DLL 已加载后若 `CreatePseudoConsole` 失败，进程级缓存无法切换函数表；
本项目不声称也不伪造二次回退。

## 4. Validation & Error Matrix

| 条件 | 行为 |
|---|---|
| 非 Windows target | sidecar 流程跳过 ConPTY staging |
| Windows target 不是 x64 | staging 明确失败，禁止错架构发布 |
| nupkg/package file SHA 不匹配 | staging 失败，禁止进入 Tauri bundle |
| DLL/任一 host 缺失、为空、PE header/架构错误 | 构建失败；运行时不预载并选择系统 ConPTY |
| `LoadLibraryW` 或兼容导出预检失败 | 释放 module、不改变环境、选择系统 ConPTY |
| 资源完整且预检成功 | 保留预载 module、不改变 PATH、选择便携实现 |
| 预载后 `CreatePseudoConsole` 失败 | 保持现有 `openpty` 错误；不声称二次回退 |

所有运行时分支必须输出 `[conpty-bootstrap]` 诊断，至少包含 backend、arch、资源目录
或系统回退原因。

## 5. Good / Base / Bad Cases

- **Good**：Windows x64 安装包带完整三文件资源树；启动日志为
  `backend=portable ... dll=preloaded hosts=x64,arm64`，Codex CLI 使用固定运行时。
- **Base**：开发目录尚未 staging 或安装资源损坏；日志明确 `backend=system reason=...`，
  终端仍由 portable-pty 的系统 ConPTY 路径打开。
- **Bad**：只带 x64 `OpenConsole.exe`、把 host 平铺根目录、从 cwd 找资源、修改进程或
  shell 子进程 PATH、重复执行 bootstrap，或捕获一次 `openpty` 错误后再调用同一 API
  冒充二次回退。

## 6. Tests Required

反馈环为 `npm run test:conpty`，断言点：

1. `tauri.windows.conf.json` 将 staging 目录映射到 `portable-conpty`；
2. 固定版本、官方 URL 与 SHA-256 不漂移；
3. x64 target 产出 x64 DLL、x64 host、ARM64 host，PE machine 精确匹配；
4. 非 x64 Windows target 明确拒绝；
5. Rust 完整资源 + probe 成功选择 Portable，且决策过程不修改 PATH；
6. 缺 DLL、缺 host、PE 错架构、probe 失败、未知进程架构均选择 System；
7. Windows 初始化使用 `OnceLock` 保证绝对路径预载只发生一次，module 引用保留到退出。

发布前还应实际执行一次 staging 并用 `verifyOfficialHashes: true` 校验最终目录。
特定 Windows build 上的 Codex 长输出滚动仍属于人工验收，纯契约测试不能替代。

## 7. Wrong vs Correct

### Wrong：在 `create_pty` 或 shell env 才准备 DLL

```rust
let mut cmd = CommandBuilder::new(shell);
cmd.env("PATH", portable_dir); // 太晚：openpty 已选择并缓存 ConPTY
let pair = native_pty_system().openpty(size)?;
```

### Correct：Tauri setup 首项完成一次性初始化

```rust
.setup(|app| {
    #[cfg(windows)]
    conpty_bootstrap::initialize(app.handle());
    // 其余初始化；之后用户操作才可能进入 openpty
    Ok(())
})
```

### Wrong：DLL 已选中后重试同一 API冒充系统回退

```rust
native_pty_system().openpty(size)
    .or_else(|_| native_pty_system().openpty(size)) // 同一进程级函数表
```

### Correct：只承诺预检前回退

资源树或 DLL 导出预检失败时完全不启用侧载目录，让 portable-pty 首次选择系统函数表；
预载成功后的 `CreatePseudoConsole` 错误按真实能力返回。
