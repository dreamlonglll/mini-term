# Windows `\\?\UNC\` verbatim 前缀必须自己剥（`dunce::simplified` 盲区）

> Rust `Path::canonicalize()` 在 Windows 上对 UNC 路径返回 `\\?\UNC\<host>\<rest>` verbatim 形式，对盘符路径返回 `\\?\<drive>:\<rest>`。把这些路径直接回传前端 / 拖进 shell / 写进配置都不友好。常用社区方案 `dunce::simplified()` 故意**只剥盘符**（`VerbatimDisk`）形式，**不剥 UNC** verbatim 前缀 —— 把 WSL UNC 或网络共享路径回传 UI 时必须自己写一条规则。

---

## What

凡是把 `Path::canonicalize()` 返回的路径**回传给前端、写进配置文件、拼进 shell 命令**，都必须做以下剥前缀处理：

| 输入形式 | 输出形式 |
|---|---|
| `\\?\C:\foo\bar`（盘符 verbatim） | `C:\foo\bar` |
| `\\?\UNC\<host>\<rest>`（UNC verbatim） | `\\<host>\<rest>` |
| `\\?\Volume{<guid>}\...`（Volume GUID verbatim） | **保留原样**（特殊情况，剥了会破语义） |
| 非 verbatim 路径 | **保留原样**（noop） |

正确实现（mini-term `src-tauri/src/fs.rs` 的版本）：

```rust
fn try_strip_windows_verbatim(s: &str) -> Option<String> {
    let rest = s.strip_prefix(r"\\?\")?;
    // UNC verbatim：必须先于盘符判断，否则被吞前两个反斜杠
    if let Some(unc_rest) = rest.strip_prefix(r"UNC\") {
        return Some(format!(r"\\{}", unc_rest));
    }
    // 盘符 verbatim
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return Some(rest.to_string());
    }
    None  // Volume GUID 等其他形式保留
}
```

**注意 strip 顺序**：`\\?\UNC\` 必须先匹配，因为 `\\?\` 是它的前缀；先匹配 `\\?\` 再判断盘符会让 UNC 路径走到盘符分支拿到错误结果。

**注意大小写**：strip 不归一化大小写（`\\?\UNC\WSL$\...` 剥成 `\\WSL$\...`），归一化交给上层调用方（如 `wsl_path::parse_unc`）。

---

## Why

- `Path::canonicalize()` 在 Windows 上调 `GetFinalPathNameByHandleW`，默认返回 verbatim 形式（绕过 MAX_PATH 限制）。`fs::read_dir`/`metadata`/`is_dir` 对 verbatim 都能正确处理，但 verbatim 前缀塞回 UI 用户看着难受、塞回 shell 大多数 shell 不识别、写进配置文件下次读取也得再剥
- `dunce` crate `simplified()` 的源码（`dunce-1.0.5/src/lib.rs` 中 `is_safe_to_strip_unc`）**只对 `Prefix::VerbatimDisk` 返回 true**，UNC verbatim 直接 fallthrough 保留原样。作者 [stated rationale](https://github.com/kornelski/dunce/issues/3)：UNC 路径剥前缀后行为可能与原 verbatim 形式不一致（绕开 MAX_PATH 的能力丢失），保守做法是不剥
- 但 mini-term 的场景是"展示用 / shell 用"，**不是再次传给 Windows API**，所以剥前缀是正确的；调用方负责保证剥完的路径只用于展示与字符串拼接，不再传回 `fs::read_dir` 这类（如要传回去，重新用原路径或重新 canonicalize）

---

## How to apply

1. **永远不要把 `canonicalize()` 返回值直接回传前端**：先过一遍 strip 函数
2. **strip 函数实现位置**：放在最贴近 fs.rs 的模块（mini-term 选择放 `src-tauri/src/fs.rs`），不要散落到各业务模块
3. **抽出纯字符串版本**：包装版 `strip_verbatim_prefix(PathBuf) -> PathBuf` 走 `#[cfg(windows)]`，但**内部纯字符串函数 `try_strip_windows_verbatim(&str) -> Option<String>` 跨平台可测**，单测在 Linux/macOS CI 也能跑（输入是字符串，行为可预测）
4. **测试矩阵**：
   - 盘符 verbatim 剥（`\\?\C:\foo` → `C:\foo`）
   - WSL UNC verbatim 剥（`\\?\UNC\wsl$\Ubuntu\home` → `\\wsl$\Ubuntu\home`）
   - 通用服务器 UNC verbatim 剥（`\\?\UNC\server\share` → `\\server\share`）
   - host 大小写保留（strip 不归一）
   - 仅 host 无 rest（`\\?\UNC\wsl$` → `\\wsl$`）
   - Volume GUID 保留原样
   - 已剥前缀的普通路径 noop（不重复剥）
5. **不要依赖 `dunce`**：项目已有 strip 函数就不要再引 `dunce`，避免两套规则共存。如果未来切到 `dunce`，要先确认它支持 UNC 剥（截至 1.0.5 不支持）

---

## 真实出处

`05-25-support-wsl-project-root` 任务 PR2 落地 `try_strip_windows_verbatim`，PR1 写 `wsl_path::parse_unc` 时把"识别 verbatim UNC 输入"作为必备 case 之一（否则 canonicalize 后的 WSL 路径无法被识别为 WSL）。调研全文见 `.trellis/tasks/05-25-support-wsl-project-root/research/wsl-path-behavior-on-windows.md` 第 3 节（dunce 盲区证据 + Rust issue rust-lang/rust#42869 长期 open 状态 + openai/codex#24101 实测）。

实现位置：
- `src-tauri/src/fs.rs` — `try_strip_windows_verbatim` / `strip_verbatim_prefix`（cfg(windows) 包装）
- `src-tauri/mt-core/src/wsl_path.rs` — `parse_unc` 支持 `\\?\UNC\` 前缀变体作为入口
