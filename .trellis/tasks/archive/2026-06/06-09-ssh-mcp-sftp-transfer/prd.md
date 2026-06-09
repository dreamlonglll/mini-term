# mt-ssh-mcp SFTP 文件上传/下载

## Goal

为 SSH MCP sidecar(`mt-ssh-mcp`)新增**文件传输能力**:让运行在 mini-term 终端里的
AI agent(Claude Code / Codex)能把本地文件上传到远程主机、或把远程文件下载到本地,
摆脱 `ssh_exec` + base64 echo 这种受 100KB 输出封顶限制、不可靠的 workaround。

实现复用现有 `SshPool` 的持久 russh session,新增 `ssh_upload` / `ssh_download`
两个 MCP 工具,走 SFTP subsystem 而非命令通道。

## Requirements

- **`ssh_upload`**:入参 `connection` + `localPath` + `remotePath`,把本地文件传到远程。
- **`ssh_download`**:入参 `connection` + `remotePath` + `localPath`,把远程文件**落盘到本地路径**
  (不把内容回传给 agent,避免二进制 base64 + 100KB 返回体问题;agent 需要内容可自行读本地文件)。
- **大文件流式分块**:用 `SftpSession::open_with_flags` 拿 `File`,以 `AsyncRead/AsyncWrite`
  分块读写,内存占用恒定,支持几百 MB 文件。不用整文件进内存的 `read/write`。
- **仅单文件**:一次传一个文件(目录递归 out of scope)。
- **本地路径不限制**:允许任意本地路径,仅在审计日志记录。**例外护栏**(见 Decision):硬性
  拒绝传输 mini-term 自身 `config.json`(`mt_core::config_json_path()`),因其含全部 SSH 明文密码。
- **复用 SshPool**:在 `acquire` 到的 session 上 `channel_open_session` →
  `request_subsystem(true,"sftp")` → `into_stream()` → `SftpSession::new`。传输全程持
  `session.lock()`(沿用现有 channel 串行化语义)。
- **超时**:外层套 `tokio::time::timeout`,默认比 exec 的 60s 宽松(建议 300s,可由
  `timeoutSecs` 入参覆盖);transport 错(`request_subsystem` 失败)复用现有 evict+重连+单次 retry,
  SFTP 协议错(路径不存在/无权限)作为业务错误返回 agent、不 evict。
- **结构化返回**:返回 JSON 含方向、字节数、`remotePath`/`localPath`、成功标志;错误不含密码。
- **审计日志**:每次传输向 `ssh-mcp-audit.log` 追加一行(方向、conn、本地路径、远程路径、字节数/结果)。

## Acceptance Criteria

- [ ] `ssh_upload` 把本地文件传到远程指定路径,远程内容与本地字节一致。
- [ ] `ssh_download` 把远程文件落盘到本地指定路径,本地内容与远程字节一致。
- [ ] 大文件(> 100KB,远超 ssh_exec 封顶;含 ≥ 数十 MB)可正常传输,内存不随文件大小线性膨胀。
- [ ] 上传/下载 mini-term 自身 `config.json` 被护栏拒绝,返回清晰错误。
- [ ] 错误路径(连接不存在/远程路径不存在/无权限/本地文件不存在)返回清晰错误,不 panic、不泄密。
- [ ] 传输事件写入 `ssh-mcp-audit.log`。
- [ ] 加入 `russh-sftp` 后 Windows MSVC 下 `cargo build` 不需 NASM;`Cargo.lock` 无 aws-lc,
      `ssh-key`/`ssh-encoding`/`rsa` 锁行未变、russh 仍单一 0.61.x。
- [ ] 纯逻辑(路径校验/护栏判定/参数解析/审计行格式)有单元测试。

## Definition of Done

- 单测覆盖纯逻辑(护栏判定、审计行格式、参数解析、错误不含密码)。
- `cd src-tauri && cargo test` 绿;Windows 下 `cargo build` 不需额外工具链。
- README 中英双版更新功能说明(新增两个 SSH MCP 工具)。
- 工具 description 文案清晰(让 agent 正确选用 upload/download 及路径方向)。

## Technical Approach

- **库**:`russh-sftp = "2.3.0"`(无需 feature)。子代理本地实测:不依赖 russh(仅其 dev-dep),
  依赖树零 aws-lc,与本仓库 `ssh-key 0.7.0-rc.10`/`ssh-encoding 0.3.0-rc.9` 精确锁一致。
- **pool 层**:在 `pool.rs` 加 `run_sftp_upload_on_session` / `run_sftp_download_on_session`,
  仿照现有 `run_exec_on_session` 的锁/错误模式;用 `open_with_flags` + `tokio::io` 分块流式。
- **工具层**:`mt-ssh-mcp.rs` 加 `ssh_upload` / `ssh_download` 两个 `#[tool]`,复用
  `find_connection` / acquire / timeout / evict-retry / `append_audit_log` 基础设施。
- **护栏**:上传/下载前把 `localPath` 规范化(`canonicalize` 或路径比对)与
  `mt_core::config_json_path()` 比较,命中即拒。抽成纯函数 `is_blocked_local_path` 便于单测。
- **下载落盘**:先写临时文件再 rename(避免半截文件),或直接流式写目标 + 失败清理(MVP 取其一)。

## Decision (ADR-lite)

**Context**:文件传输引入「本地文件系统」这一全新攻击面;尤其 mini-term `config.json` 含全部
SSH 连接明文密码,agent 一句 `ssh_upload` 即可外泄。需要在「灵活性」与「凭据安全」间取舍。

**Decision**:
1. 本地路径**不做沙箱限制**(用户选择;自用场景下 agent 即用户自己运行的 Claude/Codex,
   且项目级目录沙箱会增加实现与配置成本)。所有传输记审计日志。
2. 保留一条**最小硬护栏**:拒绝传输 mini-term 自身 `config.json`。理由——那是本工具自己的
   凭据库,外泄等于 SSH MCP 自我拆穿,与「不限制用户普通文件」是两回事,成本极低、收益明确。

**Consequences**:
- 残留风险:agent 若被远程恶意内容 prompt-injection,仍可外传 `~/.ssh/id_rsa`、`.env` 等
  本地敏感文件(config.json 除外)。审计日志可事后追溯,但不阻断。
- 未来若需收紧,可加项目目录沙箱或可配置白名单(已列入 Out of Scope,扩展点保留)。

## Out of Scope

- 本地路径沙箱 / 白名单 UI(仅保留 config.json 硬护栏;沙箱作为未来扩展点)。
- 目录递归上传/下载、glob 批量传输。
- 断点续传 / 传输进度回报。
- 远程↔远程传输。
- 下载内容直接回传给 agent(改为落盘;agent 自行读本地文件)。

## Research References

* [`research/russh-sftp-options.md`](research/russh-sftp-options.md) — **推荐 `russh-sftp = "2.3.0"`,零兼容性风险**:不依赖 russh、依赖树零 aws-lc、版本锁与仓库一致,是 russh 官方 `examples/sftp_client.rs` 钦定配套库。含最小上传/下载示例、`File` 流式分块写法、超时/错误归类注意点。

## Technical Notes

- 接入入口(russh 0.61 已验证 API):`handle.channel_open_session()` →
  `channel.request_subsystem(true, "sftp")` → `channel.into_stream()` → `SftpSession::new(stream)`。
- 关键代码锚点:
  - `pool.rs:74-119` `CachedSession` + `lock()` —— SFTP 从这里拿 Handle 开 channel。
  - `mt-ssh-mcp.rs:306-351` `run_exec_on_session` —— SFTP pool 函数的直接模板。
  - `mt-ssh-mcp.rs:434-561` `ssh_exec` —— 超时/evict-retry/审计 的接线模板。
  - `mt-ssh-mcp.rs:269-288` `append_audit_log` —— 审计日志复用。
  - `mt-core` `config_json_path()` —— 护栏比对目标。
- 风险:大文件 `read/write` 整进内存(故用流式 File);传输全程占 session 锁(大文件长占,
  可接受,与 exec 单 channel 串行一致);`SftpSession::set_timeout` 协议层默认 10s,需放宽。
- 前车之鉴:`.trellis/tasks/archive/06-06-ssh-mcp-pkcs1-rsa-key/` —— 同类「依赖锁不能拉 aws-lc」约束。
