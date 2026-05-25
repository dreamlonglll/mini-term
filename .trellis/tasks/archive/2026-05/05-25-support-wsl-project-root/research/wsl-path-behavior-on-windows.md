# Research: WSL UNC 路径在 Windows / Rust std / Tauri 上的行为

- **Query**: 调研 WSL 文件系统路径（`\\wsl$\<distro>\...` / `\\wsl.localhost\<distro>\...`）在 Windows 上对 Rust std::fs、`Path::canonicalize`、`Path::starts_with`、`dunce`、`notify` 以及 Tauri v2 `plugin-dialog` 的具体行为
- **Scope**: 外部（官方文档 + 同类项目实测）
- **Date**: 2026-05-25

---

## TL;DR — 对 mini-term 实现的最小可行结论

`\\wsl$\<distro>\...` 与 `\\wsl.localhost\<distro>\...` 在 Windows 上指向同一个 Plan 9 redirector（`p9np.dll` + `P9rdr.sys`），微软自 Win10 build 21354（2021-04）起把 `\\wsl.localhost\` 设为推荐写法，旧的 `\\wsl$\` 保留兼容。但**两种写法在 Windows API 层是两个不同的字符串**，`Path::starts_with` 不会等价、`canonicalize` 也不会归一 —— canonicalize 会把它们分别变成 `\\?\UNC\wsl$\<distro>\...` 与 `\\?\UNC\wsl.localhost\<distro>\...`（已被多方项目实测确认，见 openai/codex#24101 显示的 `\\?\UNC\wsl.localhost\Ubuntu\...`）。

`std::fs::read_dir` / `fs::metadata` / `Path::is_dir` 对两种形式都能 work（`\\?\UNC\` 的 verbatim 形式更可靠，跳过 MAX_PATH 限制），但 `dunce::simplified` **不会**剥掉 `\\?\UNC\` 前缀（仅对 `VerbatimDisk` 生效），需要自己实现一个 strip 规则：`\\?\UNC\<server>\<rest>` → `\\<server>\<rest>`。

`notify` 自己文档明确：网络文件系统（含 `\\wsl$`）`ReadDirectoryChangesW` 「may not emit any events」，需 fallback 到 `PollWatcher`。Zed、Codex、Claude Code 都被同一问题反复咬过。

Tauri v2 `plugin-dialog` 用 `rfd 0.16` → `IFileDialog::GetDisplayName(SIGDN_FILESYSPATH)`，能选 UNC 但返回的是不带 `\\?\` 的 `\\wsl$\...` 或 `\\wsl.localhost\...` 原始形式。Windows IFileDialog 显示 WSL 时默认 label 是 "Linux"，地址栏归一不会改 prefix。

---

## Findings

### 1. `\\wsl$\` vs `\\wsl.localhost\` 的关系

#### 历史与微软推荐

- 2019-03（build 18342）：`\\wsl$\<distro>` 首次出现，作为从 Windows 端访问 Linux 文件的入口（[Microsoft WSL 18342 release notes](https://learn.microsoft.com/en-us/windows/wsl/release-notes#build-18342)）
- 2020-06（build 20150）：内部前缀切到 `\\wsl\`，老 `\\wsl$\` 兼容保留 —— "Change \\wsl$ prefix to \\wsl to support URI parsing. The old \\wsl$ path is still supported."
- 2021-04（build 21354）：再切到 `\\wsl.localhost\`，理由是网络上若有名为 `wsl` 的机器会冲突 —— **"Switch the \wsl prefix to \wsl.localhost to avoid issues when there is a machine on the network named 'wsl'. \wsl$ will continue to work."**（[Microsoft WSL release notes build 21354](https://learn.microsoft.com/en-us/windows/wsl/release-notes#build-21354)）

#### 实现层是否相同？

- 文件系统驱动是同一套：`p9np.dll`（Plan 9 Network Provider）+ `P9rdr.sys`（[microsoft/WSL#12934 — Open sourcing P9rdr.sys and p9np.dll, which runs the "\\wsl.localhost" filesystem redirection](https://github.com/microsoft/WSL/issues/12934)）
- 两个 host 名都被 `p9np.dll` 接管 → 实际访问的是同一个 9P 服务（由 `init.lxs` / `LxssManager` 在 distro VM 内提供）
- **但 Windows 把 host 名当字符串处理**：`GetFullPathNameW(\\wsl$\Ubuntu\...)` 和 `GetFullPathNameW(\\wsl.localhost\Ubuntu\...)` 返回不同 string。`Path::starts_with` 不会等价

#### 兼容性矩阵（基于 release notes 推断）

| Windows 版本 | `\\wsl$\` | `\\wsl.localhost\` |
|---|---|---|
| Win10 < build 18342（2019-03 前） | 不支持 | 不支持 |
| Win10 build 18342–21354 | 支持 | 不支持 |
| Win10 build 21354+ / Win11 全部 | 支持（保留兼容） | **推荐** |

实操结论：现代 Win10/Win11 默认两种都能用，但显示在 File Explorer 地址栏的是 `\\wsl.localhost\`。

---

### 2. Rust std::fs / std::path 对 UNC 的行为

#### 2.1 `Path::canonicalize` 在 UNC 上的输出

Rust std `canonicalize` 在 Windows 上调用 `GetFinalPathNameByHandleW`，默认行为：
- `Path::new(r"\\wsl$\Ubuntu\home").canonicalize()` → `\\?\UNC\wsl$\Ubuntu\home`
- `Path::new(r"\\wsl.localhost\Ubuntu\home").canonicalize()` → `\\?\UNC\wsl.localhost\Ubuntu\home`

**两种写法 canonicalize 后不会归一**（host 名字段照搬）。

证据：
- Rust 标准库 `std::path::Prefix` 文档明确给出 `\\?\UNC\server\share` parse 出 `VerbatimUNC(server, share)`（[Path::Prefix 文档](https://doc.rust-lang.org/std/path/enum.Prefix.html)）
- [rust-lang/rust#42869 — std::fs::canonicalize returns UNC paths on Windows](https://github.com/rust-lang/rust/issues/42869) 长期 open issue，明确指出 canonicalize 返回 `\\?\C:\foo\bar...` 这种 verbatim 形式，「sometimes the `?` can be a hostname」
- [openai/codex#24101](https://github.com/openai/codex/issues/24101) 显示 Codex 沙箱 setup log 实测得到 `\\?\UNC\wsl.localhost\Ubuntu\home\<user>\src\github.com\<org>\webapp` —— 直接的真机证据

#### 2.2 `read_dir` / `metadata` / `is_dir` 行为

- **`\\?\UNC\wsl$\...`（verbatim UNC 形式）**：Windows 跳过路径规范化（`GetFullPathName` 不再处理），直接传给驱动 → `p9np.dll` 接管。`fs::read_dir` 可用。注意 verbatim 路径不做 `.` / `..` 解析。
- **`\\wsl$\...`（裸 UNC 形式）**：Windows 走标准化路径，仍由 `p9np.dll` 接管，行为等价。

两种形式都能正常 `read_dir`、`metadata`、`is_dir`，证据：Rust 1.65 起 `std::fs` 在长路径场景会**自动**把路径转 verbatim 用于 IO（[rust-lang/rust#89174 — Automatically convert paths to verbatim for filesystem operations that support it](https://github.com/rust-lang/rust/pull/89174)，已 merge），这条 path 不会跨 9P 边界出问题。

#### 2.3 `Path::starts_with` 的语义

源码（`library/std/src/path.rs` `iter_after` + `PartialEq for PrefixComponent`）：
- 按 `Component` 逐项比较，使用 `PartialEq`
- `Prefix::VerbatimUNC(server, share)` 的 PartialEq 直接比较 `OsStr` 字节
- **没有 case-insensitive 处理** —— 即使 Windows 文件系统 case-insensitive，Rust std 的 `Path::starts_with` 在 Windows 上仍是 byte-equal

实测含义：
```rust
let a = Path::new(r"\\?\UNC\WSL$\Ubuntu\home");
let b = Path::new(r"\\?\UNC\wsl$\Ubuntu\home");
a.starts_with(b) // false — host 大小写不同
```

对 mini-term 的影响：**只要 root 与 target 都从同一个用户输入路径出发，分别 canonicalize 后用 starts_with 比较是安全的**（因为 GetFinalPathNameByHandleW 对同一物理路径返回 deterministic 字符串）。但如果 root 来自配置而 target 来自前端拼接，需要先各自 canonicalize 再比 —— 现有 `verify_under_project_root` 已经这样做了。

参考：`PartialEq for PrefixComponent` impl（[rust path.rs source](https://github.com/rust-lang/rust/blob/master/library/std/src/path.rs)）：

```rust
impl<'a> PartialEq for PrefixComponent<'a> {
    fn eq(&self, other: &PrefixComponent<'a>) -> bool {
        self.parsed == other.parsed  // 字节级别比较
    }
}
```

---

### 3. 安全剥离 `\\?\UNC\` verbatim 前缀

#### 3.1 标准库

`std::path` **没有提供**剥 verbatim 前缀的官方 API。`Components` 迭代到 `Component::Prefix(PrefixComponent)`，`kind()` 返回 `Prefix::VerbatimUNC(server, share)`，你能拿到 server/share，但要自己拼回 `\\server\share\...` 形式。

#### 3.2 `dunce` crate（mini-term 没有引入）

**`dunce::simplified` 故意不剥 `\\?\UNC\`**。源码（`dunce 1.0.5/src/lib.rs:152-160`）：

```rust
#[cfg(windows)]
fn is_safe_to_strip_unc(path: &Path) -> bool {
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::VerbatimDisk(..) => {},            // 只剥 \\?\C:\ 这种
            _ => return false,                          // 其它 verbatim 前缀（包括 VerbatimUNC）一律保留
        },
        _ => return false,
    }
    ...
}
```

文档原话（[dunce::simplified 文档](https://docs.rs/dunce/latest/dunce/fn.simplified.html)）：「`\\?\C:\Windows` will be converted to `C:\Windows`, but `\\?\C:\COM` will be left as-is」 —— 只处理 disk 路径，UNC 不在范围内。

#### 3.3 安全的剥前缀做法（推荐）

把 `\\?\UNC\<server>\<share>\<rest>` 还原成 `\\<server>\<share>\<rest>` 的标准做法：

```rust
fn strip_unc_verbatim(p: &Path) -> Option<PathBuf> {
    // 利用 std 的 prefix parser，比纯字符串切片安全
    match p.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimUNC(server, share) => {
                let rest = p.strip_prefix(prefix.as_os_str()).ok()?;
                let mut s = PathBuf::from(r"\\");
                s.push(server);
                s.push(share);
                s.push(rest);
                Some(s)
            }
            _ => None,
        },
        _ => None,
    }
}
```

或更直接的字符串切片（mini-term 现有风格）：

```rust
if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
    return PathBuf::from(format!(r"\\{}", rest));
}
```

注意：剥前缀后路径仍然是 UNC 形式 (`\\wsl$\Ubuntu\...`)，**不是**驱动盘路径 —— 这是必然的，WSL 没有 drive letter。

#### 3.4 其它备选 crate

- **`normpath`（1.5.1）**：提供 `BasePath` / `normalize` / `localize_name`，对 verbatim 路径态度是「不要乱解析 `..`」，没有专门的 UNC simplify API。比 dunce 重，不必引入
- **`path-clean`**：纯字符串清洗，不感知 Windows verbatim 语义，不合用

结论：**mini-term 直接在 `strip_verbatim_prefix` 里加一条 UNC 分支即可**，不需要新增依赖。

---

### 4. `notify` crate 监听 WSL 目录的可靠性

#### 4.1 官方明确警告

`notify 8.x` 文档「Known Problems → Network filesystems」（[notify docs](https://docs.rs/notify/latest/notify/)）：

> **Network mounted filesystems like NFS may not emit any events for notify to listen to.**
> This applies especially to WSL programs watching windows paths ([issue #254](https://github.com/notify-rs/notify/issues/254)).
> A workaround is the `PollWatcher` backend.

WSL 的 9P（无论是 Windows 端走 `\\wsl$\` 看 Linux，还是 Linux 端通过 `/mnt/c` 看 Windows）属于 notify 文档点名的 network filesystem。

#### 4.2 业界共识与同类项目修复

- **Zed**：[zed-industries/zed#51340](https://github.com/zed-industries/zed/issues/51340) — closed，明确加了 PollWatcher fallback。报告引用 [microsoft/WSL#4739](https://github.com/microsoft/WSL/issues/4739) "open since 2019, 179+ comments, no fix from Microsoft in 6+ years"
- **stably/orca**：[stablyai/orca#602](https://github.com/stablyai/orca/pull/602) — 用 Node `fs.readdir` + 2s 轮询给 WSL repos 做 file watching，标题 "feat: file watching for WSL repos via polling"，正文实测 `@parcel/watcher`（也是 `ReadDirectoryChangesW` 实现）在 WSL UNC 上 "Failed to read changes" 一直报错
- **capypara20/rust-folder-watcher**：[#40](https://github.com/capypara20/rust-folder-watcher/pull/40) — `ReadDirectoryChangesExW`（class=2 RDNEI）专为 NTFS 本地路径，在 UNC/SMB 上即时返回 `ERROR_INVALID_FUNCTION`，必须 fallback 到 `ReadDirectoryChangesW`
- **Claude Code**：[anthropics/claude-code#58253](https://github.com/anthropics/claude-code/issues/58253) — Windows 11 上 `claude agents` 在 SMB-mapped UNC cwd 上 deadlock，根因「ReadDirectoryChangesW over UNC has documented edge cases — notifications can be lost or never delivered」

#### 4.3 ReadDirectoryChangesW + WSL 9P 的具体表现

把上面综合起来：
- **注册成功**：`ReadDirectoryChangesW` 在 `\\wsl$\Ubuntu\home\user` 上能 `CreateFileW` 拿到 dir handle，能 register watch
- **事件丢失或不到达**：从 Linux 端（`vim`、`echo > file`）发起的修改，Windows 端的 `ReadDirectoryChangesW` 大概率收不到事件 —— 9P 协议不像 NTFS 那样有内核级 change notification 钩子
- **从 Windows 端**：在 File Explorer 里手动改文件，事件可能能上来，但不稳定

#### 4.4 对 mini-term `watch_directory` 的实操结论

现状代码（`src-tauri/src/fs.rs:241-269`）用 `notify::recommended_watcher` → Windows 上是 `ReadDirectoryChangesWatcher`。WSL 项目下注册不会报错，但事件十之八九不到。

可行选项（不超出本研究范围）：
1. 检测项目根是否是 WSL UNC，是的话用 `notify::PollWatcher::with_initial_scan(interval=2s, ..)`
2. 完全不监听（依赖前端手动刷新），WSL 项目下文件树不自动更新
3. 用一个最小轮询线程（2s 重新 `read_dir` 顶层）

业界主流做法是 1。

---

### 5. Tauri v2 `plugin-dialog` 的 `open({directory: true})`

#### 5.1 调用链

- Tauri v2 `@tauri-apps/plugin-dialog` → `tauri-plugin-dialog`（Rust）→ `rfd 0.16`（`Cargo.toml` 实测：`rfd = { version = "0.16", default-features = false, features = ["common-controls-v6"] }`）
- `rfd` Windows backend（`src/backend/win_cid/file_dialog/dialog_ffi.rs`）：用 COM 调 `IFileDialog`（`IFileOpenDialog`），通过 `IShellItem::GetDisplayName(SIGDN_FILESYSPATH, ...)` 取最终路径

#### 5.2 `IFileDialog` 对 UNC / WSL 的支持

- `IFileDialog` 原生支持 UNC，包括 `\\wsl$\` 和 `\\wsl.localhost\`
- 在 Windows 11 File Dialog 的左侧导航里，"Linux" 节点直接挂载 `\\wsl.localhost\`，用户点进 Ubuntu 选目录是完全 supported 的官方场景
- `SIGDN_FILESYSPATH` 文档：「Returns the item's file system path, if it has one. Only items that report SFGAO_FILESYSTEM have a file system path.」（[SIGDN 文档](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/ne-shobjidl_core-sigdn)）

WSL 文件 / 目录都报 `SFGAO_FILESYSTEM`（因为它们经 p9np 暴露为 NT 文件系统对象），所以 `SIGDN_FILESYSPATH` 不会失败。

#### 5.3 返回的路径形式

`SIGDN_FILESYSPATH` 返回的是「filesystem path」，对 WSL 来说是 `\\wsl.localhost\<distro>\<unix-path>` 形式（File Dialog 内部识别什么 prefix 就返回什么 prefix），**不会**主动加 `\\?\UNC\` verbatim 前缀。

证据：
- Tauri 自己 [`plugins-workspace#3335`](https://github.com/tauri-apps/plugins-workspace/pull/3335)「fix(opener): strip UNC prefix before passing path to ILCreateFromPathW on Windows」描述里写 「`dunce::canonicalize` converts UNC paths like `\\server\share\file.mkv` to `\\?\UNC\server\share\file.mkv`」—— 证明 dialog 出来的是 `\\server\share\...` 形式，是 Tauri 内部 `canonicalize` 才让它变成 `\\?\UNC\...`
- 同类 issues 提到 dialog 返回的 WSL 路径就是 `\\wsl.localhost\Ubuntu\home\<user>\<proj>`，例如 codex#18506 / claude-code#61003 用户提供的 path 都是该形式

#### 5.4 已知 Tauri 在 WSL UNC 上的问题

- **Tauri opener plugin**：UNC 路径传给 `ILCreateFromPathW` 时如果带 `\\?\UNC\` 前缀会失败，PR #3335 修复（已闭，未 merge —— 当前 main 可能仍有问题）
- **Tauri dialog 本身**：**没有**已知专门针对 WSL/UNC 的 bug；选目录正常工作

#### 5.5 对 mini-term 实操结论

`@tauri-apps/plugin-dialog` `open({directory: true})` 选 WSL 目录会返回 `\\wsl.localhost\Ubuntu\home\user\proj` 这种形式（不带 verbatim 前缀），直接传到后端 `filter_directories` 是 OK 的，问题只出在我们自己 `canonicalize` 之后形成 `\\?\UNC\` 形式传回前端。

---

## Related Specs

- `D:\Git\mini-term\.trellis\tasks\05-25-support-wsl-project-root\prd.md` — 本任务 PRD，列出已知代码位置
- `D:\Git\mini-term\src-tauri\src\fs.rs:109-122` — 现有 `strip_verbatim_prefix`，目前只剥 `\\?\<drive>:\` 不剥 `\\?\UNC\`
- `D:\Git\mini-term\src-tauri\src\fs.rs:139-177` — `verify_under_project_root`，用 canonicalize + starts_with
- `D:\Git\mini-term\src-tauri\src\fs.rs:241-269` — `watch_directory`，用 `notify::recommended_watcher`

## External References

| URL | 用途 |
|---|---|
| [Microsoft Learn — WSL release notes](https://learn.microsoft.com/en-us/windows/wsl/release-notes) | `\\wsl.localhost\` 引入历史（build 21354） |
| [Microsoft Learn — Working across file systems](https://learn.microsoft.com/en-us/windows/wsl/filesystems) | WSL 文件系统互访官方文档 |
| [Microsoft Learn — File path formats on Windows](https://learn.microsoft.com/en-us/dotnet/standard/io/file-path-formats) | UNC、`\\?\UNC\`、Skip normalization 语义 |
| [Microsoft Learn — SIGDN enum](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/ne-shobjidl_core-sigdn) | `SIGDN_FILESYSPATH` 返回值定义 |
| [microsoft/WSL#12934](https://github.com/microsoft/WSL/issues/12934) | 确认 `\\wsl.localhost\` 由 `P9rdr.sys` + `p9np.dll` 实现 |
| [Rust Path::Prefix 文档](https://doc.rust-lang.org/std/path/enum.Prefix.html) | `VerbatimUNC(server, share)` parse 行为 |
| [rust-lang/rust#42869](https://github.com/rust-lang/rust/issues/42869) | canonicalize 返回 UNC 的长期 issue |
| [rust-lang/rust#89174](https://github.com/rust-lang/rust/pull/89174) | std::fs 自动转 verbatim 用于 IO |
| [dunce 1.0.5 文档](https://docs.rs/dunce/) + [源码](https://docs.rs/dunce/latest/src/dunce/lib.rs.html) | `simplified` / `is_safe_to_strip_unc` 只针对 VerbatimDisk |
| [notify 8.x 文档 — Network filesystems](https://docs.rs/notify/latest/notify/#network-filesystems) | 明确建议 WSL/网络 FS 用 PollWatcher |
| [notify-rs/notify#254](https://github.com/notify-rs/notify/issues/254) | WSL 上事件丢失的官方追踪 issue |
| [microsoft/WSL#4739](https://github.com/microsoft/WSL/issues/4739) | inotify 在 9P 上 silently fail，6 年未修 |
| [zed-industries/zed#51340](https://github.com/zed-industries/zed/issues/51340) | Zed 加 PollWatcher fallback 的完整 case |
| [stablyai/orca#602](https://github.com/stablyai/orca/pull/602) | 用 2s 轮询给 WSL repos 做 file watching |
| [capypara20/rust-folder-watcher#40](https://github.com/capypara20/rust-folder-watcher/pull/40) | ReadDirectoryChangesExW vs W 在 UNC 上的差异 |
| [anthropics/claude-code#58253](https://github.com/anthropics/claude-code/issues/58253) | UNC cwd deadlock 的 51 threads at 0 CPU 案例 |
| [openai/codex#24101](https://github.com/openai/codex/issues/24101) | 实测 `\\?\UNC\wsl.localhost\Ubuntu\...` canonicalize 输出 |
| [anthropics/claude-code#61003](https://github.com/anthropics/claude-code/issues/61003) | Cowork 在 WSL UNC 上的 ACL 失败 |
| [tauri-apps/plugins-workspace#3335](https://github.com/tauri-apps/plugins-workspace/pull/3335) | Tauri opener UNC prefix strip 修复 |
| [PolyMeilex/rfd 源码 dialog_ffi.rs](https://github.com/PolyMeilex/rfd/blob/master/src/backend/win_cid/file_dialog/dialog_ffi.rs) | rfd Windows backend 用 IFileDialog + SIGDN_FILESYSPATH |

---

## 实测建议（给后续单元测试）

`src-tauri/src/fs.rs` 增加 `#[cfg(windows)]` 测试用例：

```rust
#[cfg(windows)]
#[test]
fn strip_verbatim_prefix_handles_unc_wsl() {
    // 没有真机的纯字符串测试，验证转换逻辑而非真实 IO
    let p = PathBuf::from(r"\\?\UNC\wsl$\Ubuntu\home\user");
    let stripped = strip_verbatim_prefix(p);
    assert_eq!(stripped.to_string_lossy(), r"\\wsl$\Ubuntu\home\user");

    let p2 = PathBuf::from(r"\\?\UNC\wsl.localhost\Ubuntu\home\user");
    let stripped2 = strip_verbatim_prefix(p2);
    assert_eq!(stripped2.to_string_lossy(), r"\\wsl.localhost\Ubuntu\home\user");
}

#[cfg(windows)]
#[test]
fn unc_paths_starts_with_is_byte_equal() {
    use std::path::Path;
    let a = Path::new(r"\\?\UNC\wsl$\Ubuntu\home\user");
    let b = Path::new(r"\\?\UNC\wsl$\Ubuntu\home");
    assert!(a.starts_with(b)); // 同 prefix，按 Component 匹配

    let c = Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home");
    assert!(!a.starts_with(c)); // host 字段不同，starts_with 不归一
}
```

需要真机集成测试的（建议放在 CI 之外的手动验证步骤）：
- 在已安装 WSL2 的 Win11 上，`fs::read_dir(r"\\wsl$\Ubuntu\home")` 能拿到 Linux home 内容
- `Path::new(r"\\wsl$\Ubuntu\home").canonicalize()` 真实返回 `\\?\UNC\wsl$\Ubuntu\home`
- `notify::recommended_watcher().watch(r"\\wsl$\Ubuntu\home", NonRecursive)` 不报错，但 WSL 内 `touch file` 后 60s 内可能收不到事件

## Caveats / Not Found

- **没找到**官方明确的「`\\wsl$\` 与 `\\wsl.localhost\` 在 Win32 API 层 100% 等价/不等价」的权威断言；当前结论是从 release notes + 同类项目实测推断的「两个 hostname 都被 `p9np.dll` 接管，但在字符串/Path 层不归一」
- **没有真机实测**这份调研里所有结论 —— 所有 "应该返回什么" 的描述来自社区/官方文档的间接证据，需后续单元 + 集成测试验证
- WSL1 与 WSL2 的 9P 实现细节略有差异（WSL1 走 `LXSS` lift，WSL2 走 utility VM 内的 plan9 server），但 `\\wsl$\` 入口对外接口一致，本调研不区分
- `IFileDialog` 在 IPv4 hostname 字面量（"wsl"）做名称解析的具体策略不清楚，理论上某些极端环境（局域网真有名为 `wsl` 的 SMB 服务器）下 `\\wsl$\` 行为可能不一致 —— 实操中遇到再处理
