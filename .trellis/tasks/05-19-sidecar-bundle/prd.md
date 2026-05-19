# sidecar 打进发布安装包

## Goal

让 mini-term 的两个 sidecar 二进制 —— `mt-ssh-mcp`（SSH MCP server）与 `miniterm-hook`（AI hook）—— 通过 Tauri `externalBin` 机制被正式打进发布安装包（`.msi`/`.dmg`/`.AppImage`），使发布版安装后 SSH MCP 与 AI hook 功能真正可用，而不只在 `npm run tauri dev` 下能用。

## Requirements

* sidecar 抽到独立 crate `src-tauri/mt-sidecars/`（不依赖 tauri/tauri-build），含 `miniterm-hook` 与 `mt-ssh-mcp` 两个 `[[bin]]`。
* `tauri.conf.json` 的 `bundle.externalBin` 声明两个 sidecar（`binaries/mt-ssh-mcp`、`binaries/miniterm-hook`）。
* `src-tauri/Cargo.toml` 移除两个 sidecar `[[bin]]` 与仅 sidecar 用的 `rmcp`/`tokio` 依赖。
* 跨平台 staging 脚本 `scripts/stage-sidecars.mjs`：`cargo build` mt-sidecars 两个 bin，复制成 `src-tauri/binaries/<name>-<triple>[.exe]`；dev 模式再复制裸名到 `src-tauri/target/debug/`。
* `package.json` `build-sidecars` → 调脚本；`pretauri` 钩子覆盖 `tauri dev`/`tauri build`。
* `release.yml` 在 `tauri-action` 前新增一步调脚本（`--release --target ${{ matrix.target }}`）。
* `src-tauri/binaries/`、`src-tauri/mt-sidecars/target/` 加进 `.gitignore`。

## Acceptance Criteria

* [x] sidecar 独立成 `mt-sidecars` crate，两个 `.rs` 移入。
* [x] `tauri.conf.json` 声明 `bundle.externalBin` 含两个 sidecar。
* [x] `scripts/stage-sidecars.mjs` 能构建 mt-sidecars 并就位 sidecar（实测 dev 模式通过）。
* [x] `release.yml` 三平台都在打包前调 staging 脚本。
* [x] `cargo check` 主程序通过 —— 去掉 rmcp/tokio 后仍编译，且 externalBin 校验过关。
* [x] 真实 `npm run tauri build` 通过 —— 产出 MSI + NSIS，`target/release/` 含 `mt-ssh-mcp.exe`/`miniterm-hook.exe`（Tauri 剥 triple 后缀放主程序旁）。

## Technical Approach

把 sidecar 抽成独立 crate，是因为 `tauri-build` 的构建脚本会在**任何** `cargo build` 主包时校验 `externalBin` 文件存在 —— 若 sidecar 还是主包的 `[[bin]]`，构建 sidecar 本身就会触发该校验，而文件正是这次构建要产出的（鸡生蛋死锁）。独立 crate 不依赖 `tauri-build`，单独构建不触发校验。

* `src-tauri/mt-sidecars/`：新 crate，deps = mt-core、serde、serde_json、dirs、portable-pty、rmcp、tokio。
* `tauri.conf.json` `bundle.externalBin: ["binaries/mt-ssh-mcp","binaries/miniterm-hook"]`（路径相对 `src-tauri/`，写裸基名）。
* staging 脚本：`cargo build --manifest-path src-tauri/mt-sidecars/Cargo.toml`，产物复制成 `binaries/<name>-<triple>[.exe]`（externalBin 校验 + 发布打包）；dev 模式额外复制裸名到 `src-tauri/target/debug/`（运行时 `current_exe().parent()` 定位），该副本被运行中 MCP server 占用时 best-effort 跳过。
* CI：`release.yml` 新增 step 在 `tauri-action` 前调脚本（`--release --target`）—— tauri-action 不触发 `pretauri`。

## Decision (ADR-lite)

**Context**：sidecar 原是主 cargo 包 `tauri-app` 的 `[[bin]]`。base config 声明 externalBin 后，`cargo build` 编 sidecar 会跑 `tauri-build` 构建脚本校验 externalBin 文件 → 文件不存在 → 死锁。
**Decision**：选「sidecar 拆独立 crate」（用户在两方案中选定）。`mt-sidecars` 不依赖 `tauri-build`，根除死锁；externalBin 常驻 base config，dev/release 同一套。
**Consequences**：sidecar 与主程序分属两个 crate、两个 `target/`。dev 运行时定位用 `current_exe().parent()`，故 staging 脚本须把 debug sidecar 额外复制到 `src-tauri/target/debug/`。本地 `npm run tauri build` 经 `pretauri` 跑的是 debug staging → 本地打包会含 debug sidecar（功能正常，体积偏大）；正式发布走 CI（`--release`），无此问题。运行时定位 Rust 代码无需改动。

## Out of Scope

* 不改 SSH MCP / hook 的运行时逻辑。
* macOS 代码签名/公证。
* 本地 `npm run tauri build` 用 release sidecar（npm pre 钩子无法区分 dev/build；正式发布走 CI 已正确）。

## Research References

* [`research/tauri-external-bin.md`](research/tauri-external-bin.md) — Tauri v2 externalBin 机制。**注**：该研究漏判「同包 sidecar 构建会触发 tauri-build 校验」这一死锁，实测后改用独立 crate，见上方 Decision。

## Technical Notes

* 改动文件：新增 `src-tauri/mt-sidecars/`（Cargo.toml + src/bin/×2）、`scripts/stage-sidecars.mjs`；改 `src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json`、`src-tauri/.gitignore`、`package.json`、`.github/workflows/release.yml`。
* 验证：`node scripts/stage-sidecars.mjs` 实测构建 + 就位通过；`cargo check --manifest-path src-tauri/Cargo.toml` 实测通过（externalBin 校验过关）。
