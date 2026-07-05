# SSH 远程项目契约（sshConnectionId / 远程 command / gate 清单）

> 来源：task `07-05-ssh-remote-projects`。「SSH 远程项目」= 项目指向某条 SSH 连接 + 远程
> POSIX 目录，文件树/终端/Session 走远程链路。本文是跨层完整契约；改动远程链路或新增
> `project.path` 消费者时必读。

## 1. Scope / Trigger

- 新增 4 个远程 Tauri command + `create_pty` 新可选参数（跨层 request/response 契约）。
- 远程能力全部经 `mt-ssh` crate 的 russh 会话池（SFTP 只读原语 `SftpHandle`），
  session 按 `connection.id` 全局复用；known_hosts accept-new、认证顺序等池行为见
  [tokio-session-pool-pattern](./tokio-session-pool-pattern.md)。
- 慢 IO 纪律（async command、缓存锁不跨 SFTP IO、静默降级）沿用
  [wsl-unc-session-scanning](./wsl-unc-session-scanning.md)，SFTP 与 9P 同级对待。

## 2. Signatures

### 判别字段（config.json / types.ts）

```
ProjectConfig.sshConnectionId?: string   // Rust: ssh_connection_id: Option<String>
                                         // 有值 = 远程项目；path 存远程 POSIX 绝对路径
```

`#[serde(default, skip_serializing_if = "Option::is_none")]`——旧配置缺字段 → None，
None 不落盘。**引用**连接（不内嵌快照）；连接被删 = 断链态，UI 可见可删、功能入口报错。

### 远程 command（均为真 `async fn`，实现在 `src-tauri/src/remote_ssh.rs`）

| command | invoke payload | 返回 |
|---|---|---|
| `ssh_remote_list_directory` | `{ connectionId, path, projectRoot, refreshIgnore? }` | `FileEntry[]`（与本地 `list_directory` 同构） |
| `ssh_remote_validate_dir` | `{ connectionId, path }` | `string`（`~` 展开 + canonicalize 后绝对路径） |
| `ssh_remote_ai_sessions` | `{ connectionId, projectPath, force? }` | `AiSession[]`（失败**静默返回 `[]`**，不 Err） |
| `ssh_remote_ai_session_content` | `{ connectionId, sessionType, sessionId, projectPath, offset? }` | `{ messages, nextOffset }` |

`create_pty` 第 8 个可选参数：`sshRemote: { connectionId, remotePath }`（Rust
`ssh_remote: Option<SshRemoteSpec>`）。省略 = 本地/WSL 行为完全不变；有值 = spawn
`ssh -t [-p port] [-i 临时私钥] user@host "cd '<单引号安全路径>' && exec $SHELL -l"`，
shell/args 被忽略、项目 envVars 不注入、**无 BatchMode**（见 index Gotcha）。

## 3. Contracts

- 远程路径一律 POSIX 绝对路径字符串；后端 gitignore 匹配、前端相对路径计算都**不得**
  引入 Windows `Path`/`\` 语义（后端用相对路径喂 `GitignoreBuilder`，前端按根路径无 `\`
  自动选 `/`）。
- 忽略过滤 = 项目根 `.gitignore`（SFTP 读一次，按 connectionId+projectRoot 缓存，
  `refreshIgnore: true` 强制重读）+ `fs.rs::ALWAYS_IGNORE`。不做逐级收集（已确认决策）。
- 密码 autofill：`SshRemoteSpec` 连接带密码时，在 **spawn 之前**用 `arm_ssh_autofill`
  内部状态预注册——密码提示可能先于任何前端 arm 调用到达，事后 arm 有竞态。
- 断线重连 = 前端对同一 pane 再调一次 `create_pty`（后端无重连概念）。
- 前端缓存 key：远程项目必须掺 connection id（`projectDataCache.projectCacheKey`），
  两台服务器同名路径（如都有 `/root/app`）否则串键。

## 4. Validation & Error Matrix

| 条件 | 行为 |
|---|---|
| connectionId 查无（断链） | `Err("SSH 连接不存在或已被删除 (id=...)")`，前端显示断链态 |
| 本机无 ssh 客户端（create_pty 远程） | `Err`（含安装提示文案） |
| `ssh_remote_validate_dir` 路径不存在/非目录 | `Err` 明确文案，弹窗保存被拒 |
| 远程不可达/超时（会话扫描） | 静默降级返回 `[]`，不 panic 不卡 UI |
| SFTP readdir 失败（子目录展开） | 前端保持旧列表，不清空 |
| `.gitignore` 不存在 | 仅 ALWAYS_IGNORE 生效，非错误 |

## 5. Good/Base/Bad Cases

- **Good**：私钥连接 + 内网 Linux → 树秒级懒加载、pane 直连落项目目录、Session 混排带连接名。
- **Base**：密码连接 → spawn 前预注册 autofill，提示出现即回灌一次；错密码命中
  "Permission denied" 永久禁用不连灌。
- **Bad**：删除连接后打开远程项目 → 列表见断链徽标，文件树/新开终端给明确错误；
  绝不 panic、绝不静默空白。

## 6. Tests Required

- `pty.rs`：`build_ssh_launcher_args`（端口/私钥/**永不含 BatchMode** 反例断言）、
  `shell_single_quote`（内嵌单引号/元字符注入路径被包死）。
- `remote_ssh.rs`：`expand_tilde`（`~`/`~/x`/`~user` 不展开）、gitignore 相对路径匹配
  （目录规则/`!` 白名单）、增量行切分 `split_complete_lines`（半行不重复不丢）、
  缓存 key 隔离（不同 connectionId 同路径不串）。
- `config.rs`：`sshConnectionId` round-trip + 旧配置缺省 None + None 不落盘。

## 7. Wrong vs Correct

### Wrong：行扫描用 `map_while(Result::ok)` 读会话文件

```rust
for line in reader.lines().map_while(Result::ok) { ... }
```

坏行（非 UTF-8/读错误）出现在中间时**中断迭代、静默丢弃其后全部消息**——本任务
check 阶段抓到的真实回归（重构时被 clippy 建议诱导）。

### Correct：显式循环跳过坏行

```rust
for line in reader.lines() {
    let line = match line { Ok(l) => l, Err(_) => continue };  // 跳过坏行,继续读后续
    ...
}
```

### Wrong：远程项目复用本地消费者不 gate

新增任何消费 `project.path` 的功能（git、watch、搜索、导入、外部打开…）直接拿 path 当
本地路径用——远程项目的 path 是 POSIX 远程路径，本地 IO 必然失败或静默空转。

### Correct：新消费者先判 `sshConnectionId`

前端 `isRemoteProject(p)`（`src/utils/remoteProject.ts`）分支：走远程 command、
隐藏入口、或给明确「远程暂不支持」占位。既有 gate 全表见 task
`07-05-ssh-remote-projects/research/codebase-recon.md` 第 2 节。
