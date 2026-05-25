# 支持打开 WSL 目录作为 mini-term 的项目根

## Goal

让 Windows 用户能把 `\\wsl$\<distro>\<unix-path>` 或 `\\wsl.localhost\<distro>\<unix-path>` 形式的 WSL 目录作为 mini-term 的项目根：文件树能浏览、开终端时自动用 `wsl.exe -d <distro> --cd <unix-path>` 启动，cwd 真正落在 WSL 里。

## Requirements

1. `\\wsl$\<distro>\<unix-path>` 与 `\\wsl.localhost\<distro>\<unix-path>` 都被识别为 WSL 路径（含 `\\?\UNC\` verbatim 前缀变体）
2. 前端展示的所有路径（项目根、文件树节点、面包屑、拖拽插入）都不带 `\\?\UNC\` 前缀
3. 当 cwd 是 WSL UNC 路径时，`create_pty` 忽略用户配置的 shell（cmd/pwsh），强制改用 `wsl.exe -d <distro> --cd <unix-path>` 启动（不显式指定 Linux shell，由 wsl.exe 取 distro 默认登录 shell）；其余 cwd 走原逻辑
4. UI 在 WSL 项目里开终端时给出明显提示："Detected WSL project, using wsl.exe to start shell"
5. distro 从 cwd 路径直接 parse（不调 `wsl -l -v` 探测）
6. Linux/macOS 平台：WSL 检测函数返回 None，所有现有行为保持不变

## Acceptance Criteria

- [ ] 在 Windows 上把 `\\wsl$\Ubuntu\home\<user>\proj` 加为项目，文件树能正常展开、文件预览正常
- [ ] 前端任何位置展示路径都不含 `\\?\UNC\` 前缀
- [ ] 在该项目里开终端，shell 内 `pwd` 显示对应 unix 路径（如 `/home/<user>/proj`），不是 `C:\Windows` 或 `$USERPROFILE`
- [ ] 用户即使把 mini-term shell 设置成 cmd.exe，WSL 项目仍用 wsl.exe 启动
- [ ] `\\wsl.localhost\Ubuntu\...` 与 `\\wsl$\Ubuntu\...` 等价识别
- [ ] mt-core 单测覆盖：`wsl_path::parse_unc` 对 8 种 corner case（`wsl$`/`wsl.localhost`/`\\?\UNC\` 前缀变体/大小写/混合分隔符/空 path/非 WSL UNC/Linux 平台 None）
- [ ] src-tauri 单测覆盖：`strip_verbatim_prefix` 对 `\\?\UNC\<host>\<rest>` 的新行为、`verify_under_project_root` 在 WSL UNC 下的等价性
- [ ] Linux/macOS 上 `cargo test` 通过，所有现有测试不退化
- [ ] Windows 上 `cargo test` 通过 + 手动验证：用真实 WSL 路径加项目 → 开终端 → `pwd` 正确

## Definition of Done

- 上述 AC 全部勾选
- `cargo test`（mt-core / src-tauri）+ `npm run build` 通过
- README 在 Shell 章节后补一段 WSL 支持说明（自动识别 + wsl.exe 启动 + AI 状态识别不可用的 caveat）
- 不破坏 Windows 本地路径、Linux/macOS 路径的现有行为（已有测试不退化）

## Technical Approach

**核心思路**：在「路径表示」与「进程启动」两层各加一道 WSL 分支，业务主链不动。

### 1. 路径层（mt-core + src-tauri/fs.rs）

- mt-core 新增 `wsl_path` 模块：
  - `parse_unc(path) -> Option<WslPath { distro, unix_path }>` — 支持 `\\(wsl$|wsl.localhost)\<distro>\<rest>` 及 `\\?\UNC\<host>\<distro>\<rest>` verbatim 变体
  - Linux/macOS cfg 下函数体直接 return None
- src-tauri/src/fs.rs `strip_verbatim_prefix` 扩展规则：`\\?\UNC\<host>\<rest>` → `\\<host>\<rest>`（保留盘符变体的原行为）
- `verify_under_project_root` 不动 — root 与 target 都 canonicalize 后 starts_with 比较仍正确（已验证）

### 2. 进程启动层（src-tauri/src/pty.rs）

- `create_pty` 入口加分支：先调 `wsl_path::parse_unc(&cwd)`
  - Some → 重写 `shell = "wsl.exe"`、`args = ["-d", &distro, "--cd", &unix_path]`，cwd 设为 `%USERPROFILE%`（任何 Windows 端合法路径，避免 portable-pty `is_dir` 静默 fallback）
  - None → 走原逻辑
- 重写发生时通过 Tauri event 推一个 `wsl-shell-override` payload 给前端，前端显示一次性 toast（不打扰长期视觉）

### 3. 前端（src/）

- ProjectList 不改入口（用户继续用现有"添加项目"对话框粘 UNC 路径，Tauri dialog 已经返回不带 verbatim 的原始 UNC）
- 新增一个 toast 处理 `wsl-shell-override` 事件，提示 "Detected WSL project, using wsl.exe to start shell"

## Decision (ADR-lite)

**Context**：WSL UNC 路径在 mini-term 全链路（fs、pty、UI）均无特殊处理，开终端会被 cmd.exe 静默退回 C:\Windows，体验不可用。

**Decision**：
- 启动器选 wsl.exe（业界一致做法：Windows Terminal `MangleStartingDirectoryForWSL`、VS Code `getWslProfiles`、wezterm `WslDomain`、vibeyard PR #113）
- Shell 强制覆盖（无视用户 shell 配置）— 与 Windows Terminal 一致，避免用户在每个 WSL 项目单独配 shell
- WSL 内 shell 不显式指定 — 用 wsl.exe 默认登录 shell，与 `wsl ~` 体验一致
- distro 从路径直接 parse，不调 `wsl -l -v` 探测 — 路径里已含 distro 名，多调一次 wsl 是冗余开销

**Consequences**：
- ✅ 用户体验闭环：「添加 WSL 文件夹 → 开终端 → 立即在 unix 路径里」
- ✅ 跨 Win10 1903+/Win11 兼容
- ❌ AI 状态识别（process_monitor）在 wsl.exe 启动后失效 — 列入 Out of Scope，单独 spike
- ❌ notify watcher 在 WSL 9P 上事件大概率丢失 — 列入 Out of Scope，本次接受能力降级（用户文件树手动刷新）

## Out of Scope (explicit)

- WSL1 兼容性（仅验 WSL2）
- 通过 SSH 连远端 WSL（已有 SSH 链路另说）
- 自动安装/启用 WSL 发行版
- 「添加 WSL 项目」专用按钮（含 `wsl -l -v` distro 选择器）
- notify 在 UNC 路径下切 PollWatcher（接受文件树事件降级）
- WSL 徽章 / 短路径显示（`Ubuntu: /home/u/proj` 形式）
- AI 进程识别（process_monitor 看不到 WSL VM 内 claude/codex）— 单独 spike
- Windows 端编辑 WSL 文件的 EOL/权限语义问题

## Implementation Plan (3 PRs)

**PR1 — mt-core: 新增 wsl_path 模块**（约 80 行 + 单测）
- 新建 `crates/mt-core/src/wsl_path.rs`，提供 `parse_unc` + `WslPath` 结构
- 8 种 corner case 单测
- 不接通业务，仅自包含模块

**PR2 — src-tauri/fs.rs: 扩展 strip_verbatim_prefix**（约 20 行 + 单测）
- 新增 `\\?\UNC\<host>\<rest>` 分支
- 单测覆盖盘符变体与 UNC 变体的剥前缀行为
- 验证 `verify_under_project_root` 在 WSL UNC 下仍正确（新增一条集成单测）

**PR3 — src-tauri/pty.rs + 前端 toast: 接通启动器**（约 60 行 + 单测 + 前端 toast）
- `create_pty` 入口调 `wsl_path::parse_unc`，匹配则重写启动参数 + emit `wsl-shell-override` event
- 单测覆盖：WSL UNC 触发重写、普通路径不触发、Linux/macOS 直传不变
- 前端 `src/components/`（具体位置实现时定）订阅事件显示一次性 toast
- README 补 WSL 支持说明

## Research References

- [`research/wsl-path-behavior-on-windows.md`](research/wsl-path-behavior-on-windows.md) — UNC 路径在 Rust std / Tauri dialog / notify 下行为
- [`research/createprocess-unc-cwd-limit.md`](research/createprocess-unc-cwd-limit.md) — CreateProcess 限制真相 + 四方案对比 + portable-pty 源码定位
- [`research/peer-tools-wsl-ux.md`](research/peer-tools-wsl-ux.md) — 9 个工具的 WSL UX + 8 条设计建议

## Technical Notes

- portable-pty 0.8.1 在 Windows 走 ConPTY，`current_directory()` 内 `is_dir()` 失败会静默退回 `$USERPROFILE` — PR3 cwd 必须显式设为存在的目录
- `wsl.exe --cd` 接受 `/` 开头的 Linux 绝对路径，不接受 `\\wsl$\...`（必须先 parse 出 unix-path）
- `\\wsl$\` 与 `\\wsl.localhost\` 同走 `p9np.dll`，但 Win32 字符串层不归一 — parse_unc 需识别两种 host
- `dunce::simplified` 不剥 `\\?\UNC\`（只处理 `VerbatimDisk`）— PR2 自己实现
- `wsl --list -v` 输出编码在不同 Win 版本不一致 — 本次不用（不调 wsl -l -v）
- 关键代码位置：`src-tauri/src/pty.rs:574`、`src-tauri/src/fs.rs:113-122` 与 `:139`
- portable-pty 源码本机路径：`C:\Users\12197\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\portable-pty-0.8.1\src\cmdbuilder.rs` 与 `psuedocon.rs`
