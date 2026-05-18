# SSH 管理器

## Goal

在 mini-term 中加入 SSH 连接管理能力：用户可以保存一组结构化的 SSH 连接（主机/端口/用户名/
密码/密钥/跳板机等），并能在任意终端里通过右键快速选中某个连接、直接在当前终端拉起 SSH 会话。
目的是把"手动敲 `ssh user@host`"变成"保存一次、随处快速连接"，降低多服务器场景下的操作成本。

## Requirements

### 连接数据模型（结构化字段）
- 每个 SSH 连接：`id`、`name`、`host`、`port`（默认 22）、`user`、`password?`（明文，可选）、
  `identityFile?`（私钥路径）、`proxyJump?`（跳板机）、`group?`（分组名）。
- 持久化到 `config.json` 的 `AppConfig.sshConnections`，遵守「AppConfig 字段扩展四处同步契约」
  （见 `.trellis/spec/frontend/state-management.md`）。

### 管理 UI
- `App.tsx` 顶栏「设置」右侧新增「SSH」按钮，点击打开 **SSH 管理弹窗**（`SshModal`）。
- 弹窗内对 SSH 连接做新增 / 编辑 / 删除，结构化字段表单，按 `group` 分组展示。

### 快速连接
- 终端区域内右键，上下文菜单新增「SSH 连接」子菜单，按分组列出已保存连接。
- 选中连接后，在**当前终端**（右键所在 pane 的 PTY）写入拼好的 `ssh` 命令并回车。

### 进阶能力（MVP 内）
- 密钥文件登录：`ssh -i <identityFile>`，表单带文件选择器。
- 跳板机 / ProxyJump：`ssh -J <proxyJump>`，自由文本字段。
- 连接分组：连接归入 `group`，管理弹窗与右键菜单按分组组织。

### 密码自动填充（后端 PTY 输出扫描）
- 连接前前端调用 `arm_ssh_autofill(ptyId, password)` 注册自动填充；再写入 `ssh` 命令。
- 后端 PTY reader 线程扫描该 pty 输出，命中密码提示则回写密码（详见 Technical Approach）。

## Acceptance Criteria

- [ ] 可在 SSH 弹窗保存含 host/port/user 的连接并持久化到 `config.json`。
- [ ] 旧 `config.json`（无 `sshConnections`）能无损加载。
- [ ] 顶栏「SSH」按钮可打开/关闭 SSH 管理弹窗。
- [ ] 终端内右键能看到「SSH 连接」子菜单并按分组列出连接。
- [ ] 选中连接后，当前终端成功执行 `ssh ...` 并进入交互。
- [ ] 指定了私钥/跳板机/非 22 端口的连接，命令拼接正确（`-i` / `-J` / `-p`）。
- [ ] 配了密码的连接，连接时密码被自动填入；密码错误时不会连灌重试。
- [ ] 跨平台命令拼接正确（Windows/macOS/Linux 的 `ssh`）。

## Definition of Done

- Rust 端 `cargo test` 通过；前端 `npm run build` 类型检查通过。
- 新增 `AppConfig` 字段带 `#[serde(default)]`，旧 `config.json` 能无损反序列化。
- `npm run tauri dev` 下实测：新增连接 → 右键快速连接 → 成功进入远程 shell；带密码连接自动填充成功。

## Technical Approach

### 数据流（跨层契约）
```
SshModal 表单 → store.config.sshConnections → save_config → config.json   (持久化)
config.json → load_config → store.config.sshConnections                  (读取)
右键菜单选连接 → 拼 ssh 命令 → arm_ssh_autofill(ptyId,pwd) → writePtyInput(ptyId, cmd+\r)
ssh 子进程输出 → PTY reader 扫描 → 命中 password 提示 → writer 回写密码     (自动填充)
```

### ssh 命令拼接
`ssh [-p <port≠22>] [-i "<identityFile>"] [-J <proxyJump>] <user>@<host>`

### 密码自动填充（机制：后端输出扫描，见 research/ssh-password-autofill.md「机制 2」）
- `PtyManager` 新增 per-pty 自动填充状态：`{ password, done }`（`done` 兼作"已填充"与"已禁用"）。
- 新增 Tauri command `arm_ssh_autofill(pty_id, password)`，写入该状态。
- PTY flush 线程对每段输出：`strip_ansi_codes` 后累加进 per-pty 残留 buffer（保留尾部 ~256 字符，
  解决跨 16ms/4096B 分块匹配）：
  - 命中 `permission denied, please try again`（不区分大小写）→ 置 `done`（永久禁用，防灌错密码）。
  - 尾部 `trim_end` 后（不区分大小写）`ends_with("password:")` 且未 `done` → 回写 `password + "\r"`，
    置 `done`（每个 pty 只自动填一次）。
- `kill_pty` 清理该 pty 的自动填充状态。
- host-key 首次确认提示不自动应答，交用户手动（连接走当前终端，用户在场）。

## Decision (ADR-lite)

**Context**: SSH 管理器有多种实现路径（连接定义、UI 形态、密码处理、连接落点）。
**Decision**:
- 连接定义：结构化字段，存自己的 `config.json`。
- UI：顶栏「SSH」按钮 → SSH 管理弹窗做 CRUD；终端右键子菜单做快速连接。
- 连接落点：在**当前终端**写入 `ssh` 命令（用户明确选择）。
- 进阶能力：密钥文件、连接分组、跳板机/ProxyJump 进 MVP；端口转发不进。
- 密码：明文存 `config.json` + 连接时自动填充（用户已知悉并接受安全风险）。
- 密码填充机制：**后端 PTY 输出扫描回写**。SSH_ASKPASS（research 首选）要求用 `create_pty`
  spawn `ssh` 并注入进程级 env，与"当前终端写命令"落点冲突，故采用 research 的回退机制 2。
**Consequences**:
- 输出扫描依赖匹配英文提示串（OpenSSH 客户端硬编码英文、无 i18n，实践稳定）；
  极端自定义服务器 prompt 下可能填不准 —— 可接受。
- 明文密码落盘有安全风险，是用户明确取舍；未来可迁移系统凭据库 + SSH_ASKPASS。
- 复用系统 `ssh` 客户端：跨平台、自动复用 known_hosts / ssh-agent；不内嵌 SSH 库。

## Out of Scope (explicit)

- 端口转发（-L / -R / -D）。
- 导入 / 回写 `~/.ssh/config`。
- 内嵌 SSH 协议库（russh/ssh2）；SSH_ASKPASS helper 方案。
- SFTP 文件传输、远程文件浏览。
- 远程会话保活 / 断线自动重连。
- 密码加密存储 / 系统凭据库（MVP 用明文）。
- SSH 标签重启后自动恢复（`SavedPane` 不记录连接标识）。
- host-key 首次确认自动应答（交用户手动 `yes`）。

## Technical Notes

- 关键文件：`src-tauri/src/config.rs`（AppConfig + SshConnection）、`src-tauri/src/pty.rs`
  （create_pty / 自动填充扫描 / arm_ssh_autofill）、`src-tauri/src/lib.rs`（注册 command）、
  `src/types.ts`、`src/store.ts`、`src/App.tsx`（顶栏按钮）、`src/components/SshModal.tsx`（新建）、
  `src/components/TerminalInstance.tsx`（右键菜单）、`src/utils/contextMenu.ts`（子菜单支持）。
- AppConfig 字段扩展四处同步：config.rs 结构体 / config.rs Default / types.ts / store.ts 初始 config。
- AI 进程识别不受影响：`ssh` 不在 `AI_COMMANDS` 列表。

## Research References

* [`research/ssh-password-autofill.md`](research/ssh-password-autofill.md) — 密码自动填充机制对比；
  采用「机制 2：PTY 输出扫描」，含精确提示串、匹配规则与防灌错密码护栏。
