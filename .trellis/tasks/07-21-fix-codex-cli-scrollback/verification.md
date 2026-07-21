# 实现验证记录

## 已通过

| 命令 | 结果 |
|---|---|
| `npm run test:conpty` | Node 4/4；Rust bootstrap 7/7 |
| `npm run test:tui-scrollback` | Codex ED2+ED3 replay 与不变量 2/2 |
| `node --test tests/*.test.cjs` | 根级 Node 测试 11/11 |
| `node scripts/stage-conpty.mjs --target x86_64-pc-windows-msvc` | 官方包下载、包/文件 SHA-256、x64 DLL + x64/ARM64 host PE 校验通过 |
| `cargo check --manifest-path src-tauri/Cargo.toml --all-targets` | 通过 |
| `cargo test --manifest-path src-tauri/Cargo.toml --lib` | 复审后 223/223 |
| `npx tsc --noEmit` | 通过，无诊断 |
| `npm run build` | 通过；仅既有大 chunk 警告 |
| `npx tauri build --debug --no-bundle` | 通过，生成 `target/debug/tauri-app.exe` |
| 对 `target/debug/portable-conpty` 执行 `verifyOfficialHashes: true` | 三个文件官方哈希与 PE machine 全部通过 |

## 已知门禁/人工项

`cargo clippy --lib -- -D warnings` 被仓库既有 12 个告警拦截，位置为
`clipboard.rs`、`git.rs`、`pty.rs` 既有代码和 `search.rs`；本次新增
`conpty_bootstrap.rs` 未产生 Clippy 诊断。为避免扩大修复范围未修改这些基线告警。

原故障依赖特定 Windows build。仍需在至少一台原本会复现的机器上人工确认：
Codex CLI 长输出可滚动、历史命令可回看，并观察启动 stderr 中
`[conpty-bootstrap] backend=portable`。当前自动化只证明资源、打包、选择、预检与
回退契约，不能冒充该人工验收。

`portable-pty 0.8.1` 仅保证 DLL 尚未选中前的系统回退；预载成功后若
`CreatePseudoConsole` 失败，不支持同进程二次回退。

复审进一步移除了进程级 PATH 前置及 `create_pty` 的 PATH 恢复分支。当前实现只以
绝对路径预载并持有 DLL，Windows 后续裸名加载从已加载模块命中；因此 local shell、
WSL、SSH 与其他子进程环境均不受影响。整个初始化由 `OnceLock` 保护，重复或并发调用
只执行一次。复审后 `npm run test:conpty` 仍为 Node 4/4、Rust 7/7，`cargo check
--all-targets` 通过，完整 Rust lib 测试为 223/223。

平台配置生效有三层证据：Tauri 2 schema 明确将 `tauri.windows.conf.json` 自动合并到
主配置；本仓锁定的 `tauri-utils 2.9.1` 在 `read_platform` 后执行 `merge`；实际 debug
构建依赖清单包含该配置与三项源资源，且 `target/debug/portable-conpty` 三个文件的
官方 SHA-256 与 PE machine 均已复验通过。release 工作流则在 tauri-action 之前显式
运行已串入 ConPTY staging 的 `stage-sidecars.mjs --release --target <triple>`。
