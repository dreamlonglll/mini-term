# portable-pty 在 Windows ConPTY 下的 cwd 静默 fallback 陷阱

> 用 `portable-pty` 启动子进程时，若传给 `CommandBuilder::cwd()` 的路径**不是一个 Windows 端合法的存在目录**，Windows 后端会在 `current_directory()` 里通过 `Path::is_dir()` 静默检测，**失败时无任何错误日志地退回 `$USERPROFILE`**。用户在终端里 `pwd` 会发现自己在 `C:\Users\<u>` 而不是预期目录，且没有任何提示告诉他发生了什么。

---

## What

凡是调用 `portable_pty::CommandBuilder::cwd(&path)` 启动子进程，**都必须保证 `path` 满足**：

1. 是一个有效的 Windows 端路径字符串（不是 UNC `\\wsl$\...`、不是 verbatim `\\?\...`、不是 Linux 形式 `/home/...`）
2. 该路径在调用 `cwd()` 之前**确实存在且是目录**（`Path::new(&path).is_dir() == true`）

若调用方拿到的 cwd 不满足这两条（例如本身就是 WSL UNC、或者用户在配置里写了不存在的目录），**必须在调用 `cwd()` 之前替换成兜底路径**：

```rust
fn fallback_windows_cwd() -> String {
    std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
}

let effective_cwd = if Path::new(&user_cwd).is_dir() {
    user_cwd
} else {
    fallback_windows_cwd()
};
cmd.cwd(&effective_cwd);
```

不要把"原 cwd 是否合法"的检查依赖给 portable-pty —— 它的 fallback 是设计行为，不会报错。

---

## Why

portable-pty 0.8.1 `src/cmdbuilder.rs` `current_directory()` 在 Windows 上的实现大致是：

```rust
fn current_directory(&self) -> Option<PathBuf> {
    self.cwd.as_ref()
        .and_then(|c| {
            let p = Path::new(c);
            if p.is_dir() { Some(p.to_path_buf()) } else { None }
        })
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}
```

`is_dir()` 失败（路径不存在 / 不是目录 / UNC 通过 9P 访问超时）直接返回 None，链式 `or_else` 退到 `$USERPROFILE`。**没有 log、没有 Result、没有 panic**。

对调用方而言，"我传了 `\\wsl$\Ubuntu\home\u\proj`，怎么 shell 里 `pwd` 是 `C:\Users\u`" 是个**靠灰盒推理才能定位**的诊断陷阱。

CreateProcessW API 本身允许 UNC `lpCurrentDirectory`（MSDN 明文，见 `createprocess-unc-cwd-limit.md` 调研），portable-pty 这一层主动加了 `is_dir()` 校验属于"防御性设计"。不能寄望它放过 —— 在调用方就把 cwd 处理掉是唯一可靠路径。

---

## How to apply

1. **任何拼 `cmd.cwd(...)` 之前先想清楚**：这个路径是不是用户输入直接来的？是不是 canonicalize 后的 verbatim 形式？是不是跨文件系统协议（UNC、网络驱动器）？任一为是 → 先校验或替换
2. **校验**：`std::path::Path::new(&cwd).is_dir()` 在 cwd() 调用前先跑一次
3. **替换**：失败时退到 `%USERPROFILE%`，再不行退到 `C:\`。**绝不能让 portable-pty 自己 fallback**，那是诊断黑洞
4. **如果改写了 cwd**：通过 event/log 通知前端或调用方，让用户知道"你给的 cwd 没用上"。mini-term 在 WSL 启动器路径上 emit `wsl-shell-override` 事件就是这条
5. **单测**：`fallback_windows_cwd_returns_existing_path` 一类测试只能断言"返回非空"，无法在通用 CI 上断言"目录存在"（CI 用户可能没 USERPROFILE）；真实校验靠手动跑

---

## How to apply（场景示例）

mini-term 在 `create_pty` 入口处理 WSL UNC cwd 时，**没有**直接把 `\\wsl$\Ubuntu\home\u\proj` 传给 portable-pty，而是：

```rust
// src-tauri/src/pty.rs
let (effective_shell, effective_args, effective_cwd) = match &wsl_override {
    Some((distro, unix_path)) => (
        "wsl.exe".to_string(),
        vec!["-d".into(), distro.clone(), "--cd".into(), unix_path.clone()],
        fallback_windows_cwd(),  // 关键：cwd 不传 UNC，传 %USERPROFILE%
    ),
    None => (shell, args, cwd),
};
cmd.cwd(&effective_cwd);
```

`wsl.exe` 进程自己通过 `--cd <unix-path>` 进入 Linux 目录，portable-pty 这一侧拿到的 cwd 永远是 Windows 合法目录。

---

## 真实出处

`05-25-support-wsl-project-root` 任务 PR3 实现 `create_pty` 入口 WSL 分支时，最早试过直接把 `\\wsl$\<distro>\<unix>` 传给 portable-pty，调研发现 `current_directory()` 的 `is_dir()` 在 9P 上行为不稳定 + 失败静默 fallback，最终采用"在调用方先替换 cwd 为 USERPROFILE，由 wsl.exe --cd 负责进 Linux 目录"。调研全文见 `.trellis/tasks/05-25-support-wsl-project-root/research/createprocess-unc-cwd-limit.md` 第二节（portable-pty 实现路径）。

实现位置：
- `src-tauri/src/pty.rs` — `decide_wsl_override` / `fallback_windows_cwd` / `create_pty` match arm
- portable-pty 0.8.1 源码本机路径：`C:\Users\12197\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\portable-pty-0.8.1\src\cmdbuilder.rs`（`current_directory` 函数）
