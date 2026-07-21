# 红色反馈环记录

## 命令

```powershell
node --test tests/conptyBundle.test.cjs; $nodeStatus = $LASTEXITCODE; cargo test --manifest-path src-tauri/Cargo.toml --lib conpty_bootstrap; $cargoStatus = $LASTEXITCODE; Write-Output "RED_LOOP_STATUS node=$nodeStatus cargo=$cargoStatus"; if ($nodeStatus -ne 0 -or $cargoStatus -ne 0) { exit 1 }
```

首次运行时间：2026-07-21。

## 首次失败

```text
✖ Windows bundle 将 staging 目录映射到 portable-conpty 资源目录
  Error: ENOENT: no such file or directory, open
  'D:\Git\mini-term\src-tauri\tauri.windows.conf.json'

✖ 官方 ConPTY 包版本、来源与 SHA-256 固定且可诊断
✖ x64 发布目标 staging 同包 x64 DLL 与 x64/ARM64 host
✖ 不支持的发布目标明确拒绝，不能静默复制错误架构
  Error [ERR_MODULE_NOT_FOUND]: Cannot find module
  'D:\Git\mini-term\scripts\stage-conpty.mjs'

tests 4
pass 0
fail 4

error[E0412]: cannot find type `ConptyBootstrapDecision` in this scope
error[E0425]: cannot find function `choose_conpty_bootstrap` in this scope
error[E0425]: cannot find function `build_conpty_path` in this scope

RED_LOOP_STATUS node=1 cargo=101
```

该反馈环同时守住两条最小契约：发布物资源 staging/bundle 完整性，以及第一次
`openpty()` 前的便携优先、预检失败走系统实现且不修改 PATH 的选择策略。原始滚动故障依赖特定
Windows build，当前机器无法替代受影响机器上的最终人工验收。

## 排名假设

1. 仓库没有把固定版本 ConPTY 资源贯通到 Tauri bundle 与运行时，因此受影响机器始终使用系统组件。
2. 即使仅携带 x64 DLL/host，ARM64 原生 Windows 上的 x64 进程仍会缺官方要求的 ARM64 host，导致退回系统 `conhost.exe`。
3. 若初始化发生在首次 `openpty()` 之后，portable-pty 0.8.1 的进程级缓存已经固定系统函数表。
4. 若为加载资源修改当前进程 PATH，会影响无关子进程并引入并发竞态；绝对路径预载并保留 module 引用即可让后续裸名加载命中目标 DLL。

## 修复后结果

```text
> npm run test:conpty

Node resource/bundle tests: 4 passed, 0 failed
Rust conpty_bootstrap tests: 7 passed, 0 failed
```

真实官方包 staging 后再对 Tauri debug build 输出目录执行官方文件哈希与 PE machine
校验，得到：

```text
files: [conpty.dll, x64/OpenConsole.exe, arm64/OpenConsole.exe]
machines: [0x8664, 0x8664, 0xaa64]
```

## ED3 自动折叠红色反馈环

命令：

```powershell
node --test tests/tuiScrollback.test.cjs
```

移除 ED3 override 前的确定失败：

```text
✖ Codex ED2+ED3 hard-reset 删除 saved lines 后只重放 canonical transcript
AssertionError: ED3 应删除 saved lines，实际 buffer=["expanded-1","folded transcript","",""]
actual baseY=1, expected baseY=0
tests 2, pass 1, fail 1
```

该失败证明 ED2 已清可见区，但 mini-term 的 ED3 handler 阻止 xterm 删除 saved line；
最终 transcript 已重放，旧 `expanded-1` 仍错误残留。

移除唯一的 CSI J override 后，同一命令转绿：

```text
✔ Codex ED2+ED3 hard-reset 删除 saved lines 后只重放 canonical transcript
✔ alternate-screen 拦截和 100000 行容量仍保留
tests 2, pass 2, fail 0
```
