# 代码勘察：SSH 远程项目落点与既有资产

> 来源：2026-07-05 grilling 访谈期间的主会话代码勘察。实现前请以最新代码为准复核行号。

## 1. 可复用资产

### russh 会话池（抽包对象）
- 位置：`src-tauri/mt-sidecars/src/pool.rs`，被 `src/bin/mt-ssh-mcp.rs` 使用。
- 内容：russh 0.61（`default-features=false, features=["ring"]`）持久会话池：
  `PoolConfig`（idle 10min / lifetime 2h / keepalive 30s×3 / LRU cap 8 / gatetime 30s /
  reaper 60s）、`CachedSession`（`Mutex<Handle<MtClient>>` 串行化 channel 操作）、
  known_hosts accept-new（明文条目写 `~/.ssh/known_hosts`，hashed 条目按首见处理）、
  认证顺序 identity_file → password（password + keyboard-interactive 双 method）、
  PKCS#1 RSA fallback（ssh-key `=0.7.0-rc.10` + rsa `=0.10.0-rc.18` 精确锁，勿动版本）。
- SFTP：`russh-sftp = "2.3.0"`，经 `channel.into_stream()` 接入，不依赖 russh 本体。
- 抽包目标：新 crate `src-tauri/mt-ssh`（不塞 mt-core——它刻意保持轻量无 tokio）。
  `mt-sidecars` 与主程序 `src-tauri` 都依赖它。MCP/rmcp 胶水留在 sidecar。
- 主程序已有 tokio（Tauri v2 runtime），async command 可直接用。

### mt-core 既有共享物
- `ssh_connection.rs`：`SshConnection { id, name, host, port, user, password?, identityFile?, group? }`，
  serde camelCase，主程序/前端/sidecar 三方共用。
- `ssh_key.rs`：`prepare_ssh_key`（私钥复制为权限收紧的临时副本，文件名按源路径稳定哈希）、
  `cleanup_ssh_temp_keys`（启动清理）。Tauri 薄包装在 `src-tauri/src/ssh.rs`。
- `config_reader.rs`：sidecar 每次工具调用重读 `config.json`（无缓存），范围由项目
  `sshConnectionIds` 决定。

### PTY 层（`src-tauri/src/pty.rs`）
- `arm_ssh_autofill(pty_id, password)` command：注册后扫描 PTY 输出（`scan_ssh_prompt`，
  尾部 256 char residual 跨块匹配），命中密码提示回写一次；命中 "Permission denied,
  please try again." 永久禁用。**现有时序**：前端先 `create_pty`（本地 shell）→ arm →
  写 `ssh ...\r`。**直接 spawn ssh 模式下密码提示可能先于 arm 到达**——建议给
  `create_pty` 增加可选参数（如 `sshAutofillPassword`）在 spawn 前注册，避免竞态。
- WSL 启动器重写先例：`decide_wsl_override` 纯函数决定 UNC 路径项目改 spawn wsl.exe，
  有单测（"完整 create_pty 会 spawn 真实 shell,不适合单测"——远程 spawn 决策同样抽纯函数）。
- AI 会话标记：输入检测（Enter 分支 enter_ai）+ 输出 echo 扫描
  （`output_contains_ai_command`，Enter 后 2s 内），均作用于 PTY 数据流，
  **远程 ssh pane 天然可用**。

### 状态监控（`src-tauri/src/process_monitor.rs`）
- hook 优先 + 降级：`is_ai_session` + `has_recent_output(3s)` → ai-working/ai-idle。
- **无需为远程改动**。远程 claude 的 hook 回调不到本机，走降级即可（已确认接受）。

### 前端 SSH 现状
- `TerminalInstance.tsx:21` `buildSshCommand(conn, identityPath)`；`:51` 连接流程
  （`prepare_ssh_key` → `arm_ssh_autofill` → 写命令）；`:256` 右键「SSH 连接」子菜单。
- `SshModal.tsx`：连接 CRUD + `connectionSummary` 导出。
- `SshAssocModal.tsx`：按项目设定 agent 可见连接范围（陈旧 id 过滤先例、
  `enable_ssh_mcp`/`disable_ssh_mcp`）。

## 2. 需要新增/改造的落点

### 文件树（`src-tauri/src/fs.rs`）
- 本地：`list_directory(path, project_root)` 返回 `FileEntry { name, path, is_dir, ignored }`；
  `collect_gitignores`（根→当前逐级，`ignore::gitignore::Gitignore::new` 从本地文件加载）+
  `is_path_ignored`（后者覆盖前者，支持 `!` 白名单）；`ALWAYS_IGNORE`（.git/node_modules/target…）。
- 远程方案（已定）：新 command 走 SFTP readdir；仅读**项目根** `.gitignore` 一次
  （SFTP read → `GitignoreBuilder::add_line` 逐行喂），按项目缓存；叠加 `ALWAYS_IGNORE`。
  **匹配用相对路径**，避免 Windows `Path` 对 POSIX 绝对路径的歧义。
- notify watch 对远程不可用：远程项目不 watch；前端展开重拉 + 手动刷新按钮。

### Session 扫描（`src-tauri/src/ai_sessions.rs`）
- 先例：`get_ai_sessions(project_path)`、`get_wsl_ai_sessions`（07-02 任务，async command、
  缓存锁不跨慢 IO、分段加载、双 normalize、静默降级——契约见
  `spec/backend/wsl-unc-session-scanning.md`，SFTP 慢 IO 同样适用）。
- 远程：SFTP 扫 `~/.claude/projects/<编码路径>` 与 `~/.codex/sessions`；
  远程 `$HOME` 用 SFTP `canonicalize(".")`；claude 项目目录编码复用现有逻辑
  （远程 path 是 POSIX 绝对路径，编码规则同 Linux 本地）。
- 前端 `SessionList.tsx`：WSL 标识/混排/正文查看链路已有；`:202` 「复制恢复命令」
  纯字符串复制，远程天然可用。

### 配置与前端类型（`src/types.ts` / `src/store.ts`）
- `ProjectConfig`：现有 `sshMcpEnabled?` / `sshConnectionIds?` / `envVars?` / `wslDistro`
  （WSL 关联项目先例）。新增 `sshConnectionId?: string`（有值=远程项目），
  `path` 存远程 POSIX 绝对路径。
- Rust 侧 `config.rs` 的 `AppConfig`/项目结构需同步字段（serde camelCase 对齐）。
- 布局持久化 `SavedPane { shellName }`：远程项目恢复时所有 pane 统一重开 ssh，
  shellName 对远程 pane 无意义（存哨兵值或忽略，实现时定）。

### 本地消费者 gate 清单（漏一处即报错/静默异常）
| 消费点 | 处理 |
|---|---|
| `git.rs` git 状态轮询 | 远程项目跳过（不显示徽章） |
| `fs.rs` watch_directory | 跳过 |
| `get_ai_sessions`（本地扫描） | 改走远程 command |
| envVars 注入 | 远程项目隐藏入口（ProjectEnvVarsModal 触发点） |
| 「关联 SSH MCP」入口 | 远程项目隐藏（agent 已在远程机） |
| cc-connect 项目导入/dashboard | 实现时全量 grep `project.path`/`p.path` 核对 |
| FileTree 右键「资源管理器显示/默认应用打开/重命名/删除」 | 远程隐藏 |

## 3. 已拍板决策速查（详见 prd.md）

抽包 mt-ssh / 直接 spawn `ssh -t ... "cd <path> && exec $SHELL -l"` / 根 .gitignore+黑名单 /
树 MVP 只读 / pane 内一键重连不自动重连 / 项目列表入口+手输路径+SFTP stat 验证 /
引用 connectionId 不内嵌 / MVP 仅 POSIX 远程 / AI 感知接受降级。

## 4. 实现分批建议（依赖顺序）

1. **PR1 抽包**：`pool.rs` → `mt-ssh` crate；sidecar 改依赖；两侧 cargo test 回归。
   不碰主程序功能。
2. **PR2 后端远程能力**：主程序依赖 mt-ssh 建池；新增 command：远程 readdir（含
   gitignore）、远程 stat 验证、远程 session 扫描/正文；config 模型加字段；
   `create_pty` 支持 spawn ssh 启动器 + autofill 预注册。
3. **PR3 前端**：添加远程项目弹窗、项目列表远程标识与菜单 gate、FileTree 远程模式、
   终端远程 pane（自动连/断线重连 UI）、SessionList 远程会话、i18n 双语文案。
