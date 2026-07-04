# 经 `\\wsl$` UNC 扫描 WSL 内 AI 会话的契约

> 来源任务：07-02-wsl-ai-sessions。Session 块合并显示 Windows 宿主与 WSL 发行版两个
> 会话来源（术语见根目录 `CONTEXT.md`）。凡从 Windows 侧读 WSL 发行版内文件（9P 协议），
> 本文契约都适用，不限于 AI 会话。

## Scenario: 从 Windows 读取 WSL 发行版内的 claude/codex 会话

### 1. Scope / Trigger

- 新增跨层 Tauri command（`list_wsl_distros` / `get_wsl_ai_sessions`）+ 变更
  `get_ai_session_content` 签名 → 必须 code-spec 深度。
- 任何"从 Windows 进程读 `\\wsl$\<distro>\...`"的后续需求（文件树、监听等）先读本文。

### 2. Signatures

```rust
// src-tauri/src/wsl_distros.rs
#[tauri::command]
pub fn list_wsl_distros() -> Vec<WslDistro>;   // 非 Windows 恒返回空
pub struct WslDistro { name: String, is_default: bool }  // serde camelCase → isDefault

// src-tauri/src/ai_sessions.rs
#[tauri::command(async)]   // 必须 async：WSL 冷启动 + 9P 读取是秒级阻塞
pub fn get_wsl_ai_sessions(project_path: String, distro: Option<String>, force: Option<bool>)
    -> Result<Vec<AiSession>, String>;
#[tauri::command(async)]
pub fn get_ai_session_content(session_type: String, session_id: String,
    project_path: String, wsl_distro: Option<String>) -> Result<Vec<AiSessionMessage>, String>;
```

### 3. Contracts

- `get_wsl_ai_sessions` 入参：
  - `project_path` 是 WSL UNC（`\\wsl$\` / `\\wsl.localhost\`，经 `mt_core::wsl_path::parse_unc`）
    → distro 与 unix cwd 从路径推导，**忽略入参 `distro`**（WSL 根项目）。
  - `project_path` 是 Windows 盘符路径 → 必须给 `distro`，按默认 `/mnt` 规则映射
    （`D:\Git\foo` → `/mnt/d/Git/foo`，盘符小写；不解析 wsl.conf 自定义 automount root）。
  - `force=true` 绕过缓存（供手动刷新）。
- 返回的 `AiSession.wslDistro`（`Option<String>`，`skip_serializing_if none`）标记来源；
  前端查看正文时原样回传给 `get_ai_session_content`。
- 发行版枚举**只读注册表** `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`
  （子键 `DistributionName`、根键 `DefaultDistribution` 指默认项 guid、`State==1` 过滤，
  State 缺省视为已安装）。**禁止 spawn `wsl.exe -l`**：进程开销 + stdout 是 UTF-16LE 坑。
  依赖 `windows-registry 0.2`（与 windows 0.58 共享依赖树）。
- WSL 内 home 定位：枚举 `\\wsl$\<distro>\home\*` + `\root`，只取含 `.claude`/`.codex`
  的目录；防串项目靠会话文件内 cwd 精确校验兜底，不靠目录名。

### 4. Validation & Error Matrix

| 条件 | 行为 |
|------|------|
| 非 Windows 平台 | `list_wsl_distros` 空数组；WSL 扫描空结果 |
| Windows 路径但未给 distro / 相对路径 / 非 WSL 的 UNC | `Ok(vec![])`（静默） |
| WSL 未安装 / 发行版被卸载 / UNC IO 任意失败 | `Ok(vec![])`（静默降级，绝不弹错） |
| 发行版 VM 未运行 | 读 UNC 会**自动拉起 VM**（数秒 + vmmem 常驻）——已接受的决策，不加运行状态预检 |

### 5. Good/Base/Bad Cases

- **Good**：UNC 根项目 `\\wsl$\Ubuntu\home\u\proj` → 推导 (Ubuntu, /home/u/proj)，零配置出会话。
- **Base**：`D:\Git\foo` + distro=Ubuntu → 映射 `/mnt/d/git/foo`（比较用 lowercase），
  合并 Windows + WSL 两来源。
- **Bad**：给 UNC 项目又传 distro 且不一致 → 以路径推导为准；`\\server\share` 普通 UNC → 空。

### 6. Tests Required

- `cargo test`（`src-tauri/src/ai_sessions.rs` tests 模块）：
  `windows_path_to_wsl_mnt`（盘符小写 / verbatim 剥离 / UNC 与相对路径拒绝）、
  `normalize_unix_path`（lowercase / 去尾 `/` / 保留 `/`）、`derive_wsl_target`
  （UNC 优先于入参 distro）、`is_encoded_variant`（大小写不敏感变体）。
- `src-tauri/src/wsl_distros.rs`：`build_distro_list` 纯函数测试（默认项标记、State 过滤）。

### 7. Wrong vs Correct

#### Wrong

```rust
// ① 用 Windows 版 normalize 比较 Linux cwd —— `/` 被换成 `\`，永不匹配
if normalize_path(unix_cwd) == normalize_path("/mnt/d/git/foo") { ... }
// ② 同步 command 里读 \\wsl$ —— Tauri 非 async command 在主线程跑，UI 冻结数秒
#[tauri::command] pub fn get_wsl_ai_sessions(...) { fs::read_dir(unc)... }
// ③ 持缓存锁做 9P 扫描 —— Windows 侧同锁请求被 WSL 冷启动拖死
let mut cache = session_cache().lock()?; let sessions = scan_wsl(...); cache.insert(...);
```

#### Correct

```rust
// ① Linux 路径用 unix 语义 normalize（保留 `/`、lowercase、去尾 `/`）
if normalize_unix_path(unix_cwd) == normalize_unix_path(target) { ... }
// ② 秒级 IO 一律 #[tauri::command(async)]
// ③ 查缓存后立刻释放锁，扫描完再短暂加锁写回
let cached = { session_cache().lock()?.get(&key).cloned() };  // 锁即取即放
let sessions = scan_wsl(...);                                  // 无锁慢 IO
session_cache().lock()?.insert(key, ...);                      // 短暂写回
```

## 其他决策记录

- WSL 侧扫描上限低于 Windows 侧（claude 100 vs 300、codex 200 vs 500）：9P 逐文件
  往返毫秒级，全量扫描不可接受。缓存 key 掺 distro（`wsl|<distro小写>|<unix cwd>`），
  TTL 10s（Windows 侧 2s 不变）。
- drvfs（`/mnt/*`）默认大小写不敏感且盘符挂载点小写，故 cwd 匹配统一 lowercase；
  claude 项目目录名编码 `encode_project_path` 对 unix 路径同样成立
  （`/mnt/d/git/foo` → `-mnt-d-git-foo`）。
