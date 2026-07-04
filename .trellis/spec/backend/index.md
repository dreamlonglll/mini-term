# 后端开发规范（Rust / Tauri）

> `src-tauri/` 的后端编码规范。

---

## 规范索引

| 文档 | 说明 |
|------|------|
| [Agent 配置注入](./agent-config-injection.md) | 幂等读写 Claude Code / Codex 的外部配置文件（hooks、MCP server 注册） |
| [Rust 加密 crate 在 Windows MSVC 上的 NASM 陷阱](./rust-crypto-on-windows-msvc.md) | 拉 russh / reqwest / rustls 一类加密 crate 时优先选 `ring` 后端，避开 aws-lc-sys 对 NASM 的工具链依赖 |
| [`rand_core` 多版本对齐](./rand-core-version-alignment.md) | 给加密 crate 写测试 fixture 时避开 rng-based API，直接用底层类型构造，绕开 rand_core 多版本 `CryptoRng` trait 路径冲突 |
| [russh 加载 RSA 私钥的两个坑](./russh-rsa-key-loading.md) | ① ssh-key 不认传统 PKCS#1/SEC1 PEM，需 rsa crate 自剥 PEM→DER fallback；② `PrivateKeyWithHashAlg::new(rsa_key, None)` 落到 SHA-1 被现代 OpenSSH 拒，须用 `best_supported_rsa_hash` 选 rsa-sha2-512/256 |
| [Tokio 常驻资源池骨架](./tokio-session-pool-pattern.md) | 带后台 reaper + graceful shutdown 的常驻资源池可复用骨架（Weak reaper、std Mutex 持 JoinHandle、shutdown 三步、fast-path 不查 unhealthy、纯函数抽决策） |
| [russh-sftp 文件传输](./russh-sftp-file-transfer.md) | 在 russh 0.61 持久 session 上用 `russh-sftp 2.3.0` 做 SFTP 上传/下载：接入模式（request_subsystem+into_stream）、大文件流式分块（禁整文件进内存）、协议层 `set_timeout` 默认 10s/逐请求须放宽、`config.json` 明文密码外泄硬护栏 |
| [portable-pty ConPTY cwd 静默 fallback](./portable-pty-conpty-cwd-fallback.md) | Windows 上 `CommandBuilder::cwd()` 拿到非合法目录时静默退回 `$USERPROFILE`，调用方必须先校验或替换 cwd，避免诊断黑洞 |
| [Windows `\\?\UNC\` verbatim 前缀剥离](./windows-unc-verbatim-prefix-strip.md) | `canonicalize` 返回的 UNC verbatim 前缀 `dunce::simplified` 不剥（只剥盘符），自己写一条 `\\?\UNC\<host>\<rest>` → `\\<host>\<rest>` 规则 |
| [`wsl.exe --cd` 路径语义](./wsl-exe-cd-path-semantics.md) | `wsl.exe -d <distro> --cd <path>` 的 path 不接受 `\\wsl$\` UNC，必须先 parse 出 distro 与 Linux 路径再传；distro 名从路径取，不调 `wsl -l -v` |
| [PTY 子进程环境变量注入契约](./pty-env-vars-injection.md) | `create_pty` 注入项目级 env 的完整契约：注入顺序（内部 env 先 / 用户 envs 后）、`MINITERM_*` 前后端双重保护、WSL 分支跳过注入、前后端 WSL 检测口径必须对齐 |
| [toml_edit 处理 array-of-tables](./toml-edit-array-of-tables.md) | 编辑 TOML `[[xxx]]` 数组表的标准模式，保留用户注释和顺序；包含类型推断歧义 / `ArrayOfTables` 无 `Index<usize>` 等坑的 Wrong vs Correct |
| [mini-term × cc-connect 集成约定](./cc-connect-integration.md) | Management API :9820 接入面、token 从 `~/.cc-connect/config.toml` 读、创建新项目必经 toml_edit + `POST /api/v1/restart`（`/reload` 不生效）、dashboard iframe URL + race-safe broken 标记 |
| [Tauri command nested struct 参数 invoke 约定](./tauri-command-nested-args.md) | 后端 command 含 struct 参数时前端 invoke 必须 wrap `{ req: {...} }` 而非散开；含 `#[serde(rename_all = "camelCase")]` 后跨边界字段名严格 1:1 对齐 |
| [经 `\\wsl$` UNC 扫描 WSL 内 AI 会话](./wsl-unc-session-scanning.md) | 从 Windows 读 WSL 发行版内文件的完整契约：发行版枚举只读注册表 Lxss（禁 spawn wsl.exe）、秒级 9P IO 必须 `#[tauri::command(async)]`、缓存锁不得跨慢 IO、unix/windows 两套 normalize 不可混用、一切失败静默降级 |

---

## 约定

### 约定：tauri-free 共享 crate `mt-core`

**What**：凡是「Tauri app 主体」与「独立 sidecar 二进制」都要用的逻辑（共享类型、纯函数、配置读取），放进 `src-tauri/mt-core/` 这个**不依赖 `tauri`** 的库 crate，两边以路径依赖共用。

**Why**：sidecar bin（如 `mt-ssh-mcp`，及未来其它）若 `use tauri_app_lib` 会链接整个 Tauri（webview 等），体积与编译时间不可接受。`mt-core` 不依赖 tauri，sidecar 依赖它即可拿到共享逻辑而不背 Tauri。

**已在 `mt-core` 的内容**：`SshConnection` 类型、`scan_ssh_prompt` / `strip_ansi_codes`、`prepare_ssh_key` 纯逻辑、`config.json` 读取（`read_ssh_connections_for_project` 按项目关联范围过滤连接 / `config_json_path`）。

**注意**：`mt-core` 没有 `AppHandle`，定位 `config.json` 之类的路径要用 `dirs` crate 自行按平台拼（镜像 `src-tauri/src/bin/miniterm-hook.rs` 的平台分支），不能用 Tauri 的 `app.path()`。

---

## Gotchas

> **`BatchMode=yes` 会连带禁用 SSH 密码认证。**
>
> 给 `ssh` 拼参数时，`-o BatchMode=yes`（让密钥 / agent 认证失败时立即返回、不挂起）会**同时禁掉密码认证**。需要 PTY autofill 灌密码的连接绝不能带 `BatchMode=yes`。**当前仅 mini-term 主程序内置终端的 SSH 启动路径仍依赖 ssh CLI**（见 `src-tauri/src/ssh.rs` 与 `src-tauri/src/pty.rs` 的 `arm_ssh_autofill` / PTY 扫描逻辑）；`mt-ssh-mcp` sidecar 自 v0.4.10 起已切换到 russh 进程内会话池（`src-tauri/mt-sidecars/src/pool.rs`），不再走 ssh CLI 与 BatchMode，无此 gotcha。

> **stdio MCP sidecar 的 stdout 只能输出协议消息。**
>
> `mt-ssh-mcp` 这类 stdio MCP server，进程自身 stdout 仅允许 MCP 协议 JSON；任何日志 / 调试输出一律走 stderr（`eprintln!`）。子进程（如 `ssh`、`icacls`）的输出必须捕获进返回值或 `Stdio::null()`，绝不能透传到本进程 stdout，否则破坏协议。
