# SSH 管理器

## Goal

在 mini-term 中加入 SSH 连接管理能力：用户保存一组结构化的 SSH 连接（主机/端口/用户名/
密码/密钥/跳板机等），并能在任意终端里通过右键快速选中某个连接、直接在当前终端拉起 SSH 会话。
把"手动敲 `ssh user@host`"变成"保存一次、随处快速连接"，降低多服务器场景下的操作成本。

## 实现进度

- **v1 — SSH 连接管理 MVP**：已完成并提交（commit `5e1ffca`）。
- **v2 — 私钥权限自动处理（方案2）**：待实现。

---

## Requirements

### v1 已实现（commit 5e1ffca）

#### 连接数据模型（结构化字段）
- `SshConnection`：`id`、`name`、`host`、`port`（默认 22）、`user`、`password?`（明文）、
  `identityFile?`（私钥路径）、`proxyJump?`（跳板机）、`group?`（分组名）。
- 持久化到 `config.json` 的 `AppConfig.sshConnections`（四处同步契约已闭环，旧配置可无损加载）。

#### 管理 UI
- `App.tsx` 顶栏「设置」右侧「SSH」按钮 → `SshModal` 弹窗，对连接做新增/编辑/删除，按 `group` 分组展示。
- 弹窗禁用点击遮罩关闭，仅可通过 ✕ 关闭（实测调整）。

#### 快速连接
- 终端区域右键 →「SSH 连接」子菜单，按分组列出已保存连接。
- 选中连接后在**当前终端** PTY 写入拼好的 `ssh` 命令并回车。
- `showContextMenu` 扩展支持一级子菜单与分组标题。

#### 进阶能力
- 密钥文件登录（`ssh -i`，表单带文件选择器）、跳板机/ProxyJump（`ssh -J`）、连接分组。

#### 密码自动填充（后端 PTY 输出扫描）
- 连接前 `arm_ssh_autofill(ptyId, password)` 注册；后端 reader 线程扫描该 pty 输出，
  命中密码提示回写密码，每会话只填一次，命中 `Permission denied, please try again` 永久禁用防连灌。

#### 实测修复
- 私钥路径反斜杠转正斜杠：Nushell/bash 等会把双引号内的 `\` 当转义符报错，
  Windows OpenSSH 接受正斜杠路径，正斜杠在所有 shell 中都安全。

### v2 待实现：私钥权限自动处理（方案2）

**问题**：连接使用私钥时，Windows OpenSSH 会因私钥文件的 Windows ACL 权限过于开放而拒绝
（`WARNING: UNPROTECTED PRIVATE KEY FILE! ... This private key will be ignored`）。
位于用户目录（尤其 OneDrive 同步目录、或装有沙箱用户组的机器）下的 key 常继承多余 ACE。
其他 SSH 工具（WindTerm/PuTTY）用自带 SSH 实现、不做此检查；mini-term 按设计用系统 `ssh`，会撞上。

**方案**：连接时把私钥复制到一份仅当前用户可读的临时文件，用 `ssh -i <临时副本>` 连接，
绕过 OpenSSH 的拒绝；不修改用户的原始密钥文件。

需求点：
- 连接前若连接配置了 `identityFile`，后端把该私钥复制到权限收紧的临时副本，命令改用临时副本路径。
- 临时副本权限仅当前用户可读写；Windows 收紧 ACL，Unix 设 `0600`。
- 临时文件命名按源路径稳定派生，重连复用/覆盖，不无限累积；应用启动时清理临时密钥目录。
- 准备失败时回退使用原始私钥路径（让 `ssh` 自行报错，不静默吞掉连接动作）。

## Acceptance Criteria

### v1（实现完成，`cargo test` + `npm run build` 通过；交互实测进行中）
- [x] 可在 SSH 弹窗保存含 host/port/user 的连接并持久化到 `config.json`。
- [x] 旧 `config.json`（无 `sshConnections`）能无损加载。
- [x] 顶栏「SSH」按钮可打开 SSH 管理弹窗；弹窗仅 ✕ 可关闭。
- [x] 终端内右键能看到「SSH 连接」子菜单并按分组列出连接。
- [x] 选中连接后当前终端执行拼接的 `ssh` 命令（`-i` / `-J` / `-p` 拼接正确）。
- [x] 私钥路径在 Nushell 等 shell 下不再因反斜杠转义报错。
- [ ] 配了密码的连接连接时密码自动填入（待交互实测确认）。

### v2 私钥权限自动处理（待实现）
- [ ] 连接配了 `identityFile` 时，后端生成权限收紧的临时私钥副本，`ssh -i` 指向该副本。
- [ ] 原本因 "UNPROTECTED PRIVATE KEY FILE" 被拒的密钥连接，处理后可正常用密钥认证。
- [ ] 临时副本权限仅当前用户可访问；应用重启后旧临时密钥被清理。
- [ ] 准备失败时回退原始路径，连接动作不被吞掉。

## Definition of Done

- Rust 端 `cargo test` 通过；前端 `npm run build` 类型检查通过。
- 新增 `AppConfig` 字段带 `#[serde(default)]`，旧 `config.json` 能无损反序列化。
- `npm run tauri dev` 实测：新增连接 → 右键快速连接 → 进入远程 shell；带密码自动填充；
  带密钥（含权限过开放的 key）也能连上。

## Technical Approach

### 数据流（跨层契约）
```
SshModal 表单 → store.config.sshConnections → save_config → config.json   (持久化)
config.json → load_config → store.config.sshConnections                  (读取)
右键选连接 → [v2: prepare_ssh_key 解析私钥] → 拼 ssh 命令
           → arm_ssh_autofill(ptyId,pwd) → writePtyInput(ptyId, cmd+\r)
ssh 子进程输出 → PTY reader 扫描 → 命中 password 提示 → writer 回写密码     (密码自动填充)
```

### ssh 命令拼接
`ssh [-p <port≠22>] [-i "<identityPath 正斜杠>"] [-J <proxyJump>] <user>@<host>`

### v1 密码自动填充（机制：后端输出扫描，见 research/ssh-password-autofill.md「机制 2」）
- `PtyManager` per-pty 自动填充状态 `{ password, residual, done }`。
- `arm_ssh_autofill(pty_id, password)` 命令注册状态。
- PTY flush 线程对每段输出 `strip_ansi_codes` 后累加进残留 buffer（保留尾部 ~256 字符）：
  命中 `permission denied, please try again` → `done`（禁用）；
  尾部 `ends_with("password:")` → 回写 `password + "\r"` → `done`（只填一次）。
- `kill_pty` 清理状态。host-key 首次确认不自动应答。

### v2 私钥权限自动处理（方案2）设计
- **后端新增 `src-tauri/src/ssh.rs` 模块**（仿 `clipboard.rs` 的模块 + 启动清理形态）：
  - `prepare_ssh_key(identity_file: String) -> Result<String, String>` Tauri command：
    校验源文件存在 → 复制到 `{temp_dir}/mini-term-ssh-keys/<源路径稳定哈希>.key`
    （`DefaultHasher`，命名稳定、重连覆盖）→ 收紧权限 → 返回临时文件路径。
  - 收紧权限：Windows 调 `icacls <file> /inheritance:r /grant:r "<USERNAME>:F"`
    （带 `CREATE_NO_WINDOW` 避免控制台闪窗）；Unix `fs::set_permissions(0o600)`。
  - `cleanup_ssh_temp_keys()`：删除整个 `mini-term-ssh-keys` 临时目录。
- **`lib.rs`**：`mod ssh;`、`invoke_handler` 注册 `ssh::prepare_ssh_key`、
  setup 启动时调 `ssh::cleanup_ssh_temp_keys()`（紧随 `clipboard::cleanup_old_clipboard_images()`）。
- **前端 `TerminalInstance.tsx`**：
  - `connectSsh` 连接前若有 `identityFile`，`await invoke<string>('prepare_ssh_key', { identityFile })`
    取临时路径；失败 `console.error` 回退原始路径。
  - `buildSshCommand` 改签名为 `(conn, identityPath)`，使用解析后的路径（仍做反斜杠转正斜杠）。

## Decision (ADR-lite)

**Context**: SSH 管理器有多种实现路径（连接定义、UI 形态、密码处理、连接落点、密钥权限）。
**Decision**:
- 连接定义：结构化字段，存自己的 `config.json`。
- UI：顶栏「SSH」按钮 → SSH 管理弹窗 CRUD；终端右键子菜单快速连接；弹窗禁用点遮罩关闭。
- 连接落点：在**当前终端**写入 `ssh` 命令（用户明确选择）。
- 进阶能力：密钥文件、连接分组、跳板机/ProxyJump 进 MVP；端口转发不进。
- 密码：明文存 `config.json` + 连接时自动填充（用户已知悉并接受安全风险）。
- 密码填充机制：**后端 PTY 输出扫描回写**（SSH_ASKPASS 要求 `create_pty` 注入进程级 env，
  与"当前终端写命令"落点冲突，故采用 research 的回退机制 2）。
- 私钥权限（v2）：**复制到权限收紧的临时副本**，不改用户原始密钥文件，不要求用户手动 `icacls`。
**Consequences**:
- 输出扫描依赖匹配英文提示串（OpenSSH 客户端硬编码英文、无 i18n，实践稳定）。
- 明文密码落盘有安全风险，是用户明确取舍；未来可迁移系统凭据库。
- v2 临时副本是私钥的额外拷贝（仅本地 temp、权限比原文件更严、启动即清理），
  风险等级不高于"明文密码落盘"这一已接受取舍。
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
- 自动修改用户原始私钥文件的权限（v2 改为复制临时副本，不动原文件）。

## Technical Notes

- 关键文件：`src-tauri/src/config.rs`（AppConfig + SshConnection）、`src-tauri/src/pty.rs`
  （create_pty / 密码自动填充 / arm_ssh_autofill）、`src-tauri/src/ssh.rs`（v2 新增）、
  `src-tauri/src/lib.rs`（注册 command）、`src/types.ts`、`src/store.ts`、`src/App.tsx`、
  `src/components/SshModal.tsx`、`src/components/TerminalInstance.tsx`、`src/utils/contextMenu.ts`。
- AppConfig 字段扩展四处同步：config.rs 结构体 / config.rs Default / types.ts / store.ts 初始 config。
- AI 进程识别不受影响：`ssh` 不在 `AI_COMMANDS` 列表。

## Research References

* [`research/ssh-password-autofill.md`](research/ssh-password-autofill.md) — 密码自动填充机制对比；
  v1 采用「机制 2：PTY 输出扫描」，含精确提示串、匹配规则与防灌错密码护栏。
