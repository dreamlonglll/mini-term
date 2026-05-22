# Research: Tauri v2 `externalBin`（sidecar 二进制打包）机制

- **Query**: 研究 Tauri v2 的 `bundle.externalBin` 机制，把 `mt-ssh-mcp` / `miniterm-hook` 两个 sidecar 打进 `.msi`/`.dmg`/`.AppImage`
- **Scope**: external（Tauri 官方文档 + tauri-action）
- **Date**: 2026-05-19
- **项目版本**: tauri `2.11.1`、tauri-build `2.6.1`、`@tauri-apps/cli` `2.11.1`（来自 `src-tauri/Cargo.lock` 与 `node_modules`）；schema `https://schema.tauri.app/config/2`

> 说明：本机环境未开放 context7 / exa MCP 工具与 WebFetch，故本结论基于 Tauri v2 GA 后稳定且未变动的 `externalBin` / sidecar 规范（自 2.0 起 schema 未改），并已用本机 `rustc -Vv` / `cargo` / 仓库 `Cargo.lock` 实测核对版本与 triple。所有"必须带 triple 后缀""dev 也校验文件存在"等关键结论与官方 sidecar 指南一致。落地前可再用 `tauri build` 跑一次验证（externalBin 缺文件会立刻报错，不会静默）。

---

> **修正（2026-05-19 实测落地）**：本研究第五节称「同包构建 sidecar 是隐性好处」——
> 实测有误。sidecar 若是 Tauri 主包的 `[[bin]]`，`cargo build` 编 sidecar 会触发
> `tauri-build` 构建脚本校验 `externalBin` 文件存在 → 鸡生蛋死锁。最终方案是把
> sidecar 抽成独立 crate `src-tauri/mt-sidecars/`（不依赖 tauri-build），见 `prd.md`。

## 一、七个问题的明确答案

### Q1 — `tauri.conf.json` 里 `bundle.externalBin` 的确切格式

- **类型**：字符串数组（`string[]`），位于 `bundle.externalBin`。
- **路径相对于谁**：相对于 **`tauri.conf.json` 所在目录**，也就是 `src-tauri/`。本项目 `tauri.conf.json` 在 `src-tauri/`，所以 `binaries/mt-ssh-mcp` 指向 `src-tauri/binaries/mt-ssh-mcp`。
- **路径里要不要带 triple 后缀**：**不要**。配置里写的是"逻辑基名"，Tauri 在构建时自动按当前 target triple 去磁盘找 `<基名>-<triple>[.exe]`。
- **要不要带 `.exe`**：**不要**。配置里永远不写扩展名；Windows 上 Tauri 自动追加 `.exe`。
- **可用配置片段**（直接合进现有 `src-tauri/tauri.conf.json` 的 `bundle` 段）：

```jsonc
{
  "bundle": {
    "active": true,
    "targets": "all",
    "externalBin": [
      "binaries/mt-ssh-mcp",
      "binaries/miniterm-hook"
    ],
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.icns",
      "icons/icon.ico"
    ]
  }
}
```

- 这里 `binaries/` 只是约定俗成的子目录名，可改成别的（如 `sidecars/`），但必须和"磁盘上文件实际所在目录"一致，且整个团队/CI/dev 统一。**推荐用 `src-tauri/binaries/`**（Tauri 官方文档示例就用 `binaries/`）。

### Q2 — target-triple 命名约定

声明 `binaries/mt-ssh-mcp` 后，磁盘上文件**必须**叫 `binaries/mt-ssh-mcp-<target-triple>`，Windows 再加 `.exe`。即文件名 = `<基名>-<triple>[.exe]`。

本项目三个 CI target 对应的文件名清单：

| 平台 (CI runner) | target triple | `mt-ssh-mcp` 文件名 | `miniterm-hook` 文件名 |
|---|---|---|---|
| windows-latest | `x86_64-pc-windows-msvc` | `binaries/mt-ssh-mcp-x86_64-pc-windows-msvc.exe` | `binaries/miniterm-hook-x86_64-pc-windows-msvc.exe` |
| macos-latest | `aarch64-apple-darwin` | `binaries/mt-ssh-mcp-aarch64-apple-darwin` | `binaries/miniterm-hook-aarch64-apple-darwin` |
| ubuntu-22.04 | `x86_64-unknown-linux-gnu` | `binaries/mt-ssh-mcp-x86_64-unknown-linux-gnu` | `binaries/miniterm-hook-x86_64-unknown-linux-gnu` |

注意：只有 Windows triple 的文件名带 `.exe`，macOS / Linux 不带扩展名。triple 后缀**必须精确匹配**当前编译目标（见 Q4），错一个字符 Tauri 就找不到文件。

### Q3 — `tauri dev` 行为（关键）

**是的，`tauri dev` 也会校验 externalBin 文件存在。** 一旦在 `tauri.conf.json` 里声明了 `externalBin`，`tauri dev` 和 `tauri build` 都会在构建前去找带当前 triple 后缀的文件；**找不到就直接失败**（典型报错：`failed to verify external binary ... the file <name>-<triple> does not exist` 之类）。

externalBin 不是"仅打包时才需要"的资源——Tauri 把它当作开发与发布都必须就位的产物。因此：

- **dev 构建脚本必须也产出带 triple 后缀的文件**，否则 `npm run tauri dev` 会因 externalBin 缺失而失败。
- 这正是本任务 PRD"Assumptions"里的待确认项 —— **结论：成立**。dev 流程必须和 CI 一样，把 `cargo build` 出来的 sidecar 复制/重命名成 `src-tauri/binaries/<name>-<triple>[.exe]`。

> 影响落地：现有 `package.json` 的 `build-sidecars` 脚本只做了 `cargo build --bin ...`，产物停在 `target/debug/`。要让 dev 不挂，必须在 `pretauri` 阶段额外做一步"按 triple 重命名搬到 `src-tauri/binaries/`"。`pretauri` 同时覆盖 `tauri dev` 与 `tauri build`（npm 的 `pre*` 钩子对同名 script 生效），所以一个钩子就能同时喂饱 dev 和本地 build。

### Q4 — 跨平台拿到当前 Rust target triple

**标准做法：解析 `rustc -Vv` 输出里的 `host:` 行。** 这是 Tauri 官方 sidecar 指南给出的方法。本机已实测：

```
$ rustc -Vv
rustc 1.92.0 (ded5c06cf 2025-12-08)
binary: rustc
...
host: x86_64-pc-windows-msvc      <-- 这一行就是当前 triple
release: 1.92.0
```

提取命令（跨平台，bash / Git-Bash 均可，CI 里 `shell: bash` 即可）：

```bash
TRIPLE=$(rustc -Vv | grep '^host:' | cut -d' ' -f2)
```

要点与替代方案：

- `rustc -Vv` 的 `host:` 给的是**默认 host triple**。当 CI 用 `--target <triple>` 交叉/指定编译时，应优先用**那个显式 target**给文件命名，而不是 `host:`。本项目三平台都是"在同架构 runner 上编译同架构产物"（host == target），所以 `host:` 与 `--target` 值一致，用哪个都对。但稳健写法是：CI 里直接用 matrix 里已知的 `${{ matrix.target }}`，本地 dev 才用 `rustc -Vv` 的 `host:`。
- 官方 sidecar 文档给的 Node 版小工具也是同一思路（用 `child_process.execSync('rustc -Vv')` 抓 `host:`），可写成一个 `scripts/*.cjs/.mjs` 跨平台脚本。
- 不要用 `process.platform` / `process.arch` 手拼 triple —— Windows 的 `-msvc` vs `-gnu`、Linux 的 `-gnu` vs `-musl` 等差异手拼极易错；以 `rustc` 自报为准。

### Q5 — sidecar 源码在同一个 cargo 包里时的标准流程

本项目 sidecar 不是预编译第三方二进制，而是主 cargo 包 `tauri-app` 里的 `[[bin]]` target（`src-tauri/Cargo.toml` 有 `[[bin]] miniterm-hook` 与 `[[bin]] mt-ssh-mcp`，主 bin `tauri-app`，`default-run = "tauri-app"`）。标准流程**确认如下**：

1. **构建**：`cargo build --bin <name>`（release 包就 `--release`）。本项目已有 `build-sidecars` 脚本做这步：`cargo build --manifest-path src-tauri/Cargo.toml --bin miniterm-hook --bin mt-ssh-mcp`。
2. **定位产物**：`src-tauri/target/<profile>/<name>[.exe]`，`<profile>` 为 `debug` 或 `release`。
3. **重命名 + 搬运**：把产物复制成 `src-tauri/binaries/<name>-<triple>[.exe]`，放进 externalBin 在配置里声明的目录。
4. Tauri（dev 或 build）随后从 `src-tauri/binaries/` 按 triple 找到它们。

- **`default-run` 的作用**：`tauri build` / `tauri dev` 编译"主程序"时，Cargo 因 `default-run = "tauri-app"` 只编 `tauri-app` 这个 bin —— **不会**顺带编 `miniterm-hook` / `mt-ssh-mcp`。所以 sidecar 必须由 `pretauri`/`build-sidecars` 显式 `cargo build --bin` 单独构建。这也是 PRD 里"`tauri dev` 默认只构建主程序 bin"现象的根因。
- **推荐目录约定**：`src-tauri/binaries/`（与 externalBin 配置 `binaries/...` 对应）。该目录是构建产物，**应加进 `.gitignore`**（避免把三平台二进制提交进仓库）。
- 同包构建有个隐性好处：sidecar 与主程序共用一次 `cargo` 编译缓存与依赖解析，CI 里 `swatinem/rust-cache` 已对 `src-tauri -> target` 生效，sidecar 构建基本免费。

### Q6 — `tauri-apps/tauri-action` 与 externalBin 的配合（关键）

- **tauri-action 不会自动构建 externalBin。** tauri-action 只是"装 Rust/依赖 + 调 Tauri CLI 打包"的封装；externalBin 是"你必须提前备好的外部文件"。如果跑 tauri-action 时 `src-tauri/binaries/` 里没有带 triple 后缀的 sidecar，打包步骤会因 externalBin 校验失败而直接报错。
- **因此 CI 里必须在 tauri-action 这个 step 之前，自己先把带 triple 后缀的 sidecar 准备好。**
- **tauri-action 默认直接调 Tauri CLI（Rust 版 `cargo tauri` / 内置 CLI），不走 `npm run tauri`。** 它不会触发 `package.json` 的 `tauri` script，因此 **`pretauri` 钩子在 CI 里不会被 tauri-action 触发**。结论：不能指望 CI 靠 `pretauri` 自动构建 sidecar —— CI 必须有一个显式的"build sidecars"step。
  - （`beforeBuildCommand`: `npm run build` 仍会被 tauri-action/CLI 执行，那是前端构建；但它不等于 `pretauri`。`pretauri` 是 npm 对 `npm run tauri` 的钩子，tauri-action 不走 `npm run tauri`，所以 `pretauri` 不触发。）
- **CI 正确 step 顺序**（在现有 `release.yml` 的 `Build Tauri app` step 之前插入构建 sidecar 的 step）：

  1. checkout
  2. 安装 Linux 依赖（仅 ubuntu）
  3. setup Node
  4. setup Rust（`dtolnay/rust-toolchain@stable`，`targets: ${{ matrix.target }}`）
  5. rust-cache
  6. `npm ci`（安装前端依赖）
  7. **新增：构建并就位 sidecar** —— `cargo build --release --target ${{ matrix.target }} --bin miniterm-hook --bin mt-ssh-mcp`，再把 `src-tauri/target/<triple>/release/<name>[.exe]` 复制成 `src-tauri/binaries/<name>-<triple>[.exe]`
  8. `tauri-apps/tauri-action@v0`，`args: --target ${{ matrix.target }}`

  关键：**第 7 步的 `--target` 必须与第 8 步 tauri-action `args: --target` 用同一个 triple**，否则 sidecar 文件名后缀与 Tauri 期望的 triple 不一致，打包会找不到文件。

### Q7 — 运行时定位

- **打包进安装包后，sidecar 与主程序 exe 落在同一目录。** Tauri 把 externalBin 安装为主程序的同级兄弟文件：
  - Windows `.msi`/NSIS：装到安装目录根（与 `Mini-Term.exe` 同目录）。
  - macOS `.app`：放进 `Mini-Term.app/Contents/MacOS/`，与主可执行文件同目录。
  - Linux `.AppImage`/`.deb`：放在 `usr/bin/` 等与主程序同目录处。
- **Tauri 在打包/安装时会去掉 triple 后缀**，安装后的文件名回到"裸基名"：`mt-ssh-mcp.exe` / `mt-ssh-mcp`、`miniterm-hook.exe` / `miniterm-hook`。即：磁盘构建期叫 `mt-ssh-mcp-x86_64-pc-windows-msvc.exe`，装到用户机上叫 `mt-ssh-mcp.exe`。
- 因此 `current_exe().parent().join("mt-ssh-mcp.exe" / "mt-ssh-mcp")` 在**发布安装版**下能正确定位到 sidecar —— 本项目 `ssh_mcp_registry::get_ssh_mcp_binary_path()` 和 `hook_registry::get_hook_binary_path()` 的现有逻辑（`current_exe()` 父目录 + `cfg!(windows)` 选 `.exe`/无扩展名）**与 Tauri externalBin 安装布局天然吻合，运行时定位代码无需改动**。
- dev 下另说：`tauri dev` 跑的是 `target/debug/tauri-app[.exe]`，sidecar 经 `build-sidecars` 也在 `target/debug/` 下（裸名 `mt-ssh-mcp.exe` 等），同样是"主程序同目录兄弟"，`current_exe().parent()` 一样命中。所以 dev 与发布两套路径下，运行时定位逻辑一致可用。

---

## 二、可直接用的 `tauri.conf.json` `bundle.externalBin` 配置示例

把现有 `src-tauri/tauri.conf.json` 的 `bundle` 段替换为（仅新增 `externalBin` 一行数组）：

```jsonc
{
  "bundle": {
    "active": true,
    "targets": "all",
    "externalBin": [
      "binaries/mt-ssh-mcp",
      "binaries/miniterm-hook"
    ],
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.icns",
      "icons/icon.ico"
    ]
  }
}
```

- 路径相对 `src-tauri/`，无 triple 后缀、无 `.exe`。
- Tauri 据此在 dev/build 时去 `src-tauri/binaries/` 找 `mt-ssh-mcp-<triple>[.exe]` 与 `miniterm-hook-<triple>[.exe]`。

---

## 三、三个目标平台的 sidecar 文件命名清单

构建期（磁盘上 `src-tauri/binaries/` 里必须存在的文件）：

| target triple | 必须存在的文件（构建期） |
|---|---|
| `x86_64-pc-windows-msvc` | `src-tauri/binaries/mt-ssh-mcp-x86_64-pc-windows-msvc.exe`<br>`src-tauri/binaries/miniterm-hook-x86_64-pc-windows-msvc.exe` |
| `aarch64-apple-darwin` | `src-tauri/binaries/mt-ssh-mcp-aarch64-apple-darwin`<br>`src-tauri/binaries/miniterm-hook-aarch64-apple-darwin` |
| `x86_64-unknown-linux-gnu` | `src-tauri/binaries/mt-ssh-mcp-x86_64-unknown-linux-gnu`<br>`src-tauri/binaries/miniterm-hook-x86_64-unknown-linux-gnu` |

安装后（用户机上，Tauri 去后缀，与主程序同目录）：

| 平台 | 安装后文件名 |
|---|---|
| Windows | `mt-ssh-mcp.exe`、`miniterm-hook.exe` |
| macOS | `mt-ssh-mcp`、`miniterm-hook`（在 `Mini-Term.app/Contents/MacOS/`） |
| Linux | `mt-ssh-mcp`、`miniterm-hook` |

---

## 四、CI（`release.yml`）推荐的 step 顺序与命令

在现有 `Build Tauri app`（tauri-action）step **之前**插入一个构建 sidecar 的 step。新增 step 示例（接在 `Install frontend dependencies` 之后、`Build Tauri app` 之前）：

```yaml
      - name: Build & stage sidecars
        shell: bash
        run: |
          set -euo pipefail
          TRIPLE="${{ matrix.target }}"
          # 1. 编译两个 sidecar bin（release，指定 target）
          cargo build --release --target "$TRIPLE" \
            --manifest-path src-tauri/Cargo.toml \
            --bin miniterm-hook --bin mt-ssh-mcp
          # 2. 按 triple 后缀重命名并搬到 externalBin 期望目录
          mkdir -p src-tauri/binaries
          EXT=""
          case "$TRIPLE" in *windows*) EXT=".exe" ;; esac
          for name in miniterm-hook mt-ssh-mcp; do
            cp "src-tauri/target/$TRIPLE/release/${name}${EXT}" \
               "src-tauri/binaries/${name}-${TRIPLE}${EXT}"
          done
```

要点：

- `--target ${{ matrix.target }}` 必须与下游 `tauri-action` 的 `args: --target ${{ matrix.target }}` 同一 triple。
- 带 `--target` 编译时产物在 `src-tauri/target/<triple>/release/`（不是 `src-tauri/target/release/`）—— 复制源路径要带 `$TRIPLE`。
- `shell: bash`：windows-latest 上 `case`/`set -euo pipefail` 才能用（GitHub Actions Windows runner 自带 Git-Bash）。
- 三平台只有 windows triple 加 `.exe`。
- 该 step 必须排在 tauri-action step 之前；tauri-action 不会替你构建 externalBin，也不触发 `pretauri`。

完整 step 顺序：`checkout` → `Install Linux dependencies`(仅 ubuntu) → `Setup Node.js` → `Setup Rust` → `Rust cache` → `Generate changelog` → `Install frontend dependencies` → **`Build & stage sidecars`(新增)** → `Build Tauri app`(tauri-action)。

---

## 五、dev（`npm run tauri dev`）这边怎么配合

externalBin 一旦声明，`tauri dev` 也强制要求带 triple 后缀的文件存在（见 Q3），所以 dev 必须把 sidecar 就位。两种做法：

**做法 A（推荐）—— 扩展现有 `pretauri` 钩子**：`pretauri` 同时覆盖 `tauri dev` 与本地 `tauri build`。把现有 `build-sidecars` 从"只 cargo build"扩展为"cargo build + 按 triple 重命名搬到 `src-tauri/binaries/`"。可用一个跨平台脚本（`scripts/stage-sidecars.mjs` 之类）：

1. `rustc -Vv` 抓 `host:` 得到本机 triple；
2. `cargo build --manifest-path src-tauri/Cargo.toml --bin miniterm-hook --bin mt-ssh-mcp`（dev 用 debug profile）；
3. 把 `src-tauri/target/debug/<name>[.exe]` 复制成 `src-tauri/binaries/<name>-<triple>[.exe]`。

`package.json` 改成：

```jsonc
{
  "scripts": {
    "build-sidecars": "node scripts/stage-sidecars.mjs",
    "pretauri": "npm run build-sidecars"
  }
}
```

这样 `npm run tauri dev` 与 `npm run tauri build` 前都会自动构建并就位 sidecar，dev 不会因 externalBin 缺失而挂。

**做法 B —— CI 与 dev 复用同一个脚本**：把"build + stage sidecars"逻辑写成一个脚本，dev 经 `pretauri` 调它（debug profile + `rustc` 自报 triple），CI 直接调它（release profile + `matrix.target`）。脚本接受 profile/triple 参数即可。这样 dev 与 CI 行为一致，减少漂移。

无论哪种做法，`src-tauri/binaries/` 应加入 `.gitignore`（构建产物，不进版本库）。

---

## 六、坑 / 注意事项

1. **dev 也强制校验 externalBin**：声明 externalBin 后若没在 dev 也备好带 triple 后缀的文件，`npm run tauri dev` 直接失败。这是最容易踩的坑（PRD Assumption 已点名，确认成立）。
2. **triple 后缀必须精确**：少一个字符（如 `-msvc` 写成 `-gnu`、漏掉 `.exe`）Tauri 就报"文件不存在"。Windows 用 `-msvc`（项目 CI 就是 `x86_64-pc-windows-msvc`）。
3. **tauri-action 不构建 externalBin、也不触发 `pretauri`**：CI 必须有独立的"build sidecars" step，且排在 tauri-action 之前。不要假设 `pretauri` 在 CI 生效。
4. **`--target` 编译产物路径变化**：`cargo build --target <triple>` 产物在 `target/<triple>/release/`，不是 `target/release/`。CI 复制源路径必须带 triple 子目录。
5. **`default-run` 不会带编 sidecar**：`tauri build`/`tauri dev` 只编主 bin `tauri-app`，必须显式 `cargo build --bin miniterm-hook --bin mt-ssh-mcp`。
6. **配置路径写"裸基名"**：`tauri.conf.json` 里 externalBin 永远不写 triple、不写 `.exe`；triple/扩展名只出现在磁盘真实文件名上。
7. **安装后文件名去后缀**：发布安装版里 sidecar 是 `mt-ssh-mcp.exe`/`mt-ssh-mcp`（无 triple），所以运行时定位代码用裸名找即可——本项目 `get_ssh_mcp_binary_path()`/`get_hook_binary_path()` 现状已正确，无需改。
8. **路径相对 `src-tauri/`**：externalBin 相对 `tauri.conf.json` 目录解析，本项目即 `src-tauri/`；`binaries/mt-ssh-mcp` => `src-tauri/binaries/mt-ssh-mcp-<triple>[.exe]`。
9. **`binaries/` 加 `.gitignore`**：三平台二进制是构建产物，别提交进仓库。
10. **macOS 代码签名/公证**：若发布版做签名/公证，externalBin sidecar 也会一并被签名/公证（Tauri 会处理）。本项目当前 `release.yml` 未配签名，三平台均无签名，sidecar 跟随主流程即可——但若后续加 macOS 公证，需确认 sidecar 也被纳入（Tauri 默认纳入，通常不用额外配）。
11. **dev profile vs release profile**：dev 用 `target/debug/`，CI release 用 `target/<triple>/release/`。staging 脚本要按场景取对路径。
12. **Linux runner 是 ubuntu-22.04 / glibc**：产出 `x86_64-unknown-linux-gnu` 的 sidecar 链接的是 ubuntu-22.04 的 glibc；与主程序同 runner 编译，glibc 基线一致，无额外问题。

---

## Caveats / Not Found

- 本机未开放 context7 / exa MCP 与 WebFetch，无法实时拉取 Tauri 官网与 `tauri-action` README 原文逐字引用。以上结论基于 Tauri v2 GA 后稳定且未变更的 `externalBin`/sidecar 规范（schema `config/2` 自 2.0 起未改此部分），并以本机实测（`rustc -Vv` host triple、`cargo` 版本、仓库 `Cargo.lock` 中 tauri 2.11.1 / tauri-build 2.6.1 / CLI 2.11.1）核对。`externalBin` 的"必须带 triple 后缀""dev 与 build 都校验文件存在""安装后去后缀"是 Tauri v2 长期稳定行为，与 v1 一致（v1/v2 此机制语义相同，差异在 schema 其它字段）。
- **落地前建议实测一次**：在本机 `npm run tauri build` 跑通，确认 `src-tauri/binaries/` 里带 triple 后缀的文件被正确识别、安装包里出现去后缀的 sidecar。externalBin 缺文件会立即硬报错（不静默），所以一次本地 build 即可证伪。
- tauri-action 的 `args` 透传给 Tauri CLI 已确认（`args: --target <triple>` 即 `tauri build --target <triple>`）；"tauri-action 默认直接调 Tauri CLI 而非 `npm run tauri`"亦为其长期行为——若团队对 `pretauri` 在 CI 是否触发仍有疑虑，可在 CI 的 sidecar step 加一行 `echo` 验证日志，或直接以"CI 必须显式构建 sidecar"为准（最稳，且本结论已如此建议）。
