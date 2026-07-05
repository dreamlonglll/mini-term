# russh-sftp 文件传输(复用持久 session + 两个坑)

> `mt-ssh-mcp` 在已有的 `russh 0.61` 持久会话池(`SshPool`)之上,用 `russh-sftp 2.3.0`
> 实现 `ssh_upload` / `ssh_download` 两个 MCP 工具。这里固化**接入模式**与两个非显而易见
> 的坑:① SFTP 协议层 `set_timeout` 默认 10s 且**逐请求**计时,慢链路大文件会被误中断;
> ② 本地文件系统是新攻击面,必须硬护栏拦住 mini-term 自身 `config.json`(含全部 SSH 明文密码)。

---

## 依赖(已验证,见 [Windows MSVC NASM 陷阱](./rust-crypto-on-windows-msvc.md))

```toml
# 不依赖 russh(russh 仅其 dev-dep),通过 AsyncRead+AsyncWrite 接 channel.into_stream(),
# 故不引入第二个 russh、不触发 aws-lc 的 NASM、不动 ssh-key/ssh-encoding 精确锁。实测零 aws-lc。
russh-sftp = "2.3.0"
# 流式读写本地文件 + AsyncReadExt/AsyncWriteExt 扩展 trait
tokio = { version = "1", features = [..., "fs", "io-util"] }
```

加 `russh-sftp` 后**必须**复验:`Cargo.lock` grep `aws-lc`/`aws_lc` 为空、`russh` 单一 0.61.x、
`ssh-key 0.7.0-rc.10` / `ssh-encoding 0.3.0-rc.9` / `rsa 0.10.0-rc.18` 锁行未变。

## 接入模式(从已认证 Handle 开 SFTP)

```rust
let handle_guard = session.lock().await;                       // 全程持锁,串行化同 session 的 channel
let channel = handle_guard.channel_open_session().await?;      // 失败 = Transport 错
channel.request_subsystem(true, "sftp").await?;                // ← SFTP 入口;失败 = Transport 错
let sftp = SftpSession::new(channel.into_stream()).await?;     // 握手;失败 = Transport 错
sftp.set_timeout(sftp_request_timeout_secs(transfer_timeout)).await; // ← 见坑 1,必做
```

## 坑 1:SFTP 协议层超时默认 10s 且逐请求计时

`SftpSession` 的 `request_timeout_secs` **默认仅 10s**,且是**每个 SFTP 请求包**(每个 chunk 的
read/write、每次 open)各自计时,不是整段传输。慢链路 / 拥塞下单个 chunk 等待 >10s 就会以
晦涩协议错中断整段传输——而外层 `tokio::time::timeout` 的 300s 窗口远未到。**握手后必须立刻
把协议层超时同步到外层传输窗口**,否则大文件验收必挂在慢网络上(单测覆盖不了)。

## 坑 2:大文件必须流式,禁止整文件进内存

`SftpSession::read(path) -> Vec<u8>` / `write(path, &[u8])` 会把**整文件读进内存**。大文件
(几百 MB)直接 OOM。**改用** `open_with_flags(.., CREATE|TRUNCATE|WRITE)` / `open(..)` 拿 `File`
(impl `AsyncRead/AsyncWrite/AsyncSeek`),用固定缓冲(`SFTP_CHUNK_BYTES = 8KB`)`read`→`write_all`
分块,内存占用恒定。

## 安全:本地路径是新攻击面

文件传输让 agent 能读写**运行 mini-term 这台机器**的本地文件。本项目决策为「本地路径不沙箱、
仅审计」,但保留**一条最小硬护栏**:拒传 mini-term 自身 `config.json`(`mt_core::config_json_path()`),
因其含全部 SSH 连接明文密码,外泄等于 SSH MCP 自我拆穿。upload/download 双向都要拦。

---

## 可执行契约(Executable Contract)

### 1. Scope / Trigger
infra:在 russh 持久 session 上新增/修改 SFTP 文件传输,或动 `pool.rs` 的 SFTP 原语 /
`mt-ssh-mcp.rs` 的 `ssh_upload`/`ssh_download` 工具时适用。

### 2. Signatures
```rust
// pool.rs —— 在已 acquire 的 session 上跑一次 SFTP 传输,返回字节数。全程持 session.lock()。
pub async fn run_sftp_upload_on_session(
    session: &CachedSession, local_path: &str, remote_path: &str, transfer_timeout: Duration,
) -> Result<u64, SftpTransferError>;
pub async fn run_sftp_download_on_session(
    session: &CachedSession, remote_path: &str, local_path: &str, transfer_timeout: Duration,
) -> Result<u64, SftpTransferError>;

// 纯函数:协议层每请求超时同步到外层窗口(下限 1s)。
fn sftp_request_timeout_secs(transfer_timeout: Duration) -> u64;   // = transfer_timeout.as_secs().max(1)

// 错误分类:caller 据此决定是否 evict + 重连。
pub enum SftpTransferError { Transport(String), Sftp(String) }
impl SftpTransferError { pub fn is_transport(&self) -> bool; pub fn message(&self) -> &str; }

// mt-ssh-mcp.rs —— 安全护栏(纯函数核心便于单测)。
fn is_blocked_local_path(local_path: &str) -> bool;                          // 比 mt_core::config_json_path()
fn is_blocked_local_path_against(local_path: &str, target: &Path) -> bool;

// MCP 工具(入参字段 snake_case;timeout_secs 缺省 300)。
async fn ssh_upload({connection, local_path, remote_path, timeout_secs?})   -> CallToolResult;
async fn ssh_download({connection, remote_path, local_path, timeout_secs?}) -> CallToolResult;
```

### 3. Contracts
- **常量**:`SFTP_CHUNK_BYTES = 8*1024`;`DEFAULT_TRANSFER_TIMEOUT_SECS = 300`(注意**不是** exec 的 60s)。
- **返回**:`SshTransferResult` `#[serde(rename_all="camelCase")]` = `{direction, bytes, remotePath, localPath, success}`,序列化为 JSON 文本。
- **下载原子性**:先写 `<local>.mt-sftp-partial` 临时文件 → 流式拷贝 → 成功后原子 `rename` 到目标;copy / rename 任一失败都**清理临时文件**,绝不留半截目标。
- **错误归类**:`channel_open_session` / `request_subsystem` / `SftpSession::new` 握手失败 = `Transport`(caller evict + 单次 retry);SFTP 协议错(路径不存在/无权限)+ 本地 IO 错 = `Sftp`(caller **不** evict,作 `invalid_params` 返 agent)。
- **不泄密**:`message()` 只透传 russh/russh-sftp/IO 错误文本(不含凭据);McpError 不含密码。
- **护栏**:`is_blocked_local_path_against` 先两侧 `canonicalize` 比对(解析软链/大小写/相对路径);任一侧不存在则走 **lexical 兜底**(折叠 `.`/`..`,Windows 大小写不敏感),防 `..` 绕过。`config_json_path()` 定位不到 → 放行(不误伤普通文件)。
- **超时**:外层 `tokio::time::timeout(transfer_timeout, ...)`;协议层 `sftp.set_timeout(sftp_request_timeout_secs(transfer_timeout))`。`timeout_secs` 入参 `.max(1)` 兜底。
- **stdio 铁律**:所有日志 `eprintln!`;文件数据只走 `tokio::fs` + SFTP 流,绝不进本进程 stdout。

### 4. Validation & Error Matrix
| 情况 | 结果 |
|------|------|
| 正常上传/下载 | `Ok(bytes)`;`success=true`;审计记一行 |
| local_path == config.json(含 `..`/大小写/软链变体) | 护栏拒,`invalid_params`,不开 channel |
| 远程路径不存在 / 无权限 | `Sftp` 错 → `invalid_params` 返 agent,**不** evict session |
| channel/subsystem/握手失败 | `Transport` 错 → evict + 重连 + 单次 retry |
| 慢链路单 chunk > 协议层超时 | 已被 `set_timeout` 放宽到外层窗口,不再 10s 误中断 |
| 整段超过 `timeout_secs` | 外层 timeout 中止,`timed_out` 语义 |
| 下载中途失败 | 临时文件被清理,目标不被半截覆盖 |

### 5. Good / Base / Bad
- **Good**:上传 2KB 配置 → 8KB 缓冲一次读完 → 远程字节一致 → 审计 `dir=upload exit=ok`。
- **Base**:下载 500MB 日志 → 8KB 分块流式 → 内存恒定 → 临时文件 rename 落盘。
- **Bad**:`ssh_upload` 本机 `config.json` → 护栏拒、错误清晰、**不触网不开 channel**(明文密码不出本机)。

### 6. Tests Required(断言点,均不触网)
`mt-ssh-mcp.rs` 单测:
- `is_blocked_local_path_blocks_exact_config_json` / `_allows_other_file_in_same_dir`(canonicalize 路径)。
- `is_blocked_local_path_blocks_via_dotdot_when_nonexistent`(lexical 兜底防 `..` 绕过)。
- `is_blocked_local_path_windows_case_insensitive_nonexistent`(大小写)。
- `is_blocked_local_path_allows_unrelated_nonexistent_path`(兜底不误伤)。
- `format_transfer_audit_line_*`(单行、换行/制表清洗、错误结果)。
- `ssh_upload_args_parse_*` / `ssh_download_args_parse`(snake_case 入参、timeout 可选)。
`pool.rs` 单测:
- `sftp_request_timeout_tracks_transfer_window`(含 0 秒 / 亚秒下限保护)。
- **端到端**(单测覆盖不了字节一致性 / 内存恒定 / 慢链路超时):连真实 SSH 跑一次 upload+download 往返。

### 7. Wrong vs Correct
#### Wrong
```rust
let data = std::fs::read(local)?;          // ① 整文件进内存,大文件 OOM
sftp.write(remote, &data).await?;          //   用高层 write 同样整进内存
// ② 握手后不 set_timeout —— 协议层留默认 10s/请求,慢链路大文件被误中断
// ③ 下载直接写目标文件,中途失败留半截覆盖
// ④ 不校验 local_path —— agent 可 ssh_upload 把 config.json 明文密码外传
```
#### Correct
```rust
let mut f = sftp.open_with_flags(remote, CREATE|TRUNCATE|WRITE).await?;  // 流式 File
sftp.set_timeout(sftp_request_timeout_secs(transfer_timeout)).await;     // 协议层超时=外层窗口
let mut buf = vec![0u8; SFTP_CHUNK_BYTES];                               // 8KB 分块,内存恒定
loop { let n = local.read(&mut buf).await?; if n==0 {break;} f.write_all(&buf[..n]).await?; }
// 下载:写 .mt-sftp-partial → rename;失败清理。上传/下载前 if is_blocked_local_path(local) { reject }
```

---

## 真实出处

task `06-09-ssh-mcp-sftp-transfer`:为 `mt-ssh-mcp` 加 SFTP 上传/下载。实现见
`src-tauri/mt-ssh/src/pool.rs`(07-05 抽包后位置;`run_sftp_*_on_session` / `SftpTransferError` /
`sftp_request_timeout_secs`)与 `src-tauri/mt-sidecars/src/bin/mt-ssh-mcp.rs`
(`ssh_upload` / `ssh_download` / `is_blocked_local_path` / 传输审计)。坑 1(协议层 10s 逐请求超时)
由 `trellis-check` 在实现后审出并修复——`trellis-implement` 初版漏了 prd 点名的这一项。
