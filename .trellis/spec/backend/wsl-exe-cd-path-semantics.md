# `wsl.exe --cd` 路径语义：必须传 Linux 路径，不接受 `\\wsl$\` UNC

> 用 `wsl.exe -d <distro> --cd <path>` 启动 WSL 内 shell 时，`<path>` 的语义有**严格规则**：
>
> - `/` 开头 → 解释为 Linux 绝对路径（如 `/home/u/proj`）
> - 其他 → 解释为 Windows 绝对路径（如 `C:\Users\u\proj`）
> - **`\\wsl$\<distro>\...` 形式 UNC 不被接受** —— 必须先 parse 出 distro + linux-path，把 linux-path 传 `--cd`

---

## What

凡是要在 Windows 上启动 WSL 子进程并进入指定目录，**都必须遵循以下三步**：

1. **检测 cwd 是否为 WSL UNC 形式**：`\\wsl$\<distro>\<rest>` 或 `\\wsl.localhost\<distro>\<rest>` 或它们的 `\\?\UNC\` verbatim 变体
2. **parse 出 `distro` 与 `unix_path`**：从 UNC 路径里提取 distro 名（如 `Ubuntu`），把剩余部分转成 `/` 开头的 Linux 路径（反斜杠转正斜杠）
3. **拼启动命令**：`wsl.exe -d <distro> --cd <unix_path>`，**不能**直接把 UNC 传 `--cd`

正确实现（mini-term 的版本）：

```rust
// mt-core::wsl_path::parse_unc 返回 Option<WslPath { distro, unix_path }>
if let Some(WslPath { distro, unix_path }) = parse_wsl_unc(&cwd) {
    let shell = "wsl.exe";
    let args = vec![
        "-d".to_string(),
        distro,
        "--cd".to_string(),
        unix_path,  // 形如 "/home/u/proj"，绝不是 "\\wsl$\Ubuntu\home\u\proj"
    ];
    // portable-pty 的 cwd 设为 Windows 合法目录（见 portable-pty-conpty-cwd-fallback.md）
    let pty_cwd = fallback_windows_cwd();
}
```

`--cd` 反例：

```rust
// ❌ wsl.exe 不接受 UNC --cd，会失败或退回默认 home
let args = vec!["-d", "Ubuntu", "--cd", r"\\wsl$\Ubuntu\home\u\proj"];
```

---

## Why

- `wsl.exe --cd` 的语义文档**只在本机 `wsl --help` 输出里**（中英文均有），微软 learn.microsoft.com 上的 `WSL/basic-commands.md` 根本没列 `--cd`，是文档盲区
- 本机实测（Win11 Home China 10.0.26200，`wsl --help` 中文输出 2026-05-25）：
  ```
  --cd <Directory>
      将指定的目录设置为当前工作目录。
      如果使用 ~ 则作用 Linux 用户的主目录。
      如果以 / 字符开始,值将被解释为绝对 Linux 路径。
      否则,该值必须是绝对 Windows 路径。
  ```
- 业界已知 issue：[microsoft/terminal#11994](https://github.com/microsoft/terminal/issues/11994) — Windows Terminal 早期把 `//wsl$/...`（正斜杠形式）直接传 `--cd`，wsl.exe 拒绝；最终 Windows Terminal `Utils::MangleStartingDirectoryForWSL`（`microsoft/terminal:src/types/utils.cpp:1018-1107`）的做法就是先 parse 出 distro + linux-path，再传 linux-path
- 与 mini-term 路径处理协议对齐：项目内**存储**的 WSL 项目根仍是 UNC 形式（`\\wsl$\<distro>\<unix>`，便于在 File Explorer 复制粘贴），**启动 wsl.exe 时**才转译

---

## How to apply

1. **UNC parsing 集中在一处**：mini-term 选择放 `mt-core::wsl_path::parse_unc`（跨平台纯字符串函数），不要在各业务模块各写一份正则
2. **支持的输入形式**至少 4 种（mt-core 单测已覆盖）：
   - `\\wsl$\<distro>\<rest>`（旧形式，Win10 build 18342+）
   - `\\wsl.localhost\<distro>\<rest>`（新形式，Win10 build 21354+ 推荐）
   - `\\?\UNC\wsl$\<distro>\<rest>`（canonicalize 输出）
   - `\\?\UNC\wsl.localhost\<distro>\<rest>`（canonicalize 输出）
3. **host 大小写不敏感**（`WSL$` / `Wsl.LocalHost` 也要识别），**distro 名保留原大小写**（`Ubuntu-22.04` 不能改成 `ubuntu-22.04`）
4. **空 rest 归一为 `/`**：用户传 `\\wsl$\Ubuntu`（无子路径）时，`--cd /` 进 distro 根目录
5. **不要调 `wsl -l -v` 探测 distro**：distro 名已经在路径里，再调 wsl 是冗余开销 + 跨 Win 版本输出编码不一致（UTF-16 LE vs UTF-8 BOM）会引入新坑
6. **不显式指定 Linux shell**：`wsl.exe -d <distro> --cd <unix>` 后**不要**追加 `-- bash` / `-- zsh`，让 wsl.exe 取 distro 默认登录 shell（与 `wsl ~` 体验一致；用户在 WSL 内自己配 zsh/fish 不会被覆盖）

---

## 真实出处

`05-25-support-wsl-project-root` 任务 PR3 落地 `create_pty` 的 WSL 启动器分支，决策详见 PRD `## Decision (ADR-lite)` 段。`--cd` 路径语义、`wsl$`/`wsl.localhost` 双 host 兼容性、各 Win 版本兼容性的研究全文见：
- `.trellis/tasks/05-25-support-wsl-project-root/research/createprocess-unc-cwd-limit.md` 第四节（wsl.exe 启动器细节）+ 第五节（Windows Terminal / VS Code / wezterm 业界证据）
- `.trellis/tasks/05-25-support-wsl-project-root/research/wsl-path-behavior-on-windows.md` 第 1 节（`wsl$` vs `wsl.localhost` 历史与兼容矩阵）

实现位置：
- `src-tauri/mt-core/src/wsl_path.rs` — `parse_unc` / `WslPath` struct
- `src-tauri/src/pty.rs` — `decide_wsl_override` / `create_pty` match arm 拼装 `["-d", distro, "--cd", unix_path]`

业界参考实现：
- Windows Terminal `Utils::MangleStartingDirectoryForWSL`：`microsoft/terminal:src/types/utils.cpp:1018-1107`
- VS Code `getWslProfiles`：`microsoft/vscode:src/vs/platform/terminal/node/terminalProfiles.ts:280-310`
- vibeyard PR #113 `isWslPath` / `parseWslPath`：<https://github.com/elirantutia/vibeyard/pull/113>
