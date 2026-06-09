# Research: 为 mt-ssh-mcp 的 russh 0.61 session 增加 SFTP 上传/下载的实现方案

- **Query**: 在 `mt-ssh-mcp` sidecar 里为已有的 russh 0.61 SSH 客户端 session 增加 SFTP 文件上传/下载能力，有哪些可行实现方案
- **Scope**: mixed（内部代码约束 + 外部 crate 查证）
- **Date**: 2026-06-09

---

## TL;DR（一句话结论）

**推荐 `russh-sftp = "2.3.0"`**：它本身**不依赖 russh**（russh 仅在它的 dev-dependencies 里），通过 `AsyncRead + AsyncWrite` trait 与任意 channel 解耦对接，因此**不会引入第二个 russh、不会拉 aws-lc、不破坏 ssh-key/ssh-encoding 精确锁**；并且它就是 **russh 官方仓库 examples 里 `sftp_client.rs` 钦定的配套库**。已用临时工程实测：加入后依赖树零 aws-lc，russh 仍单一 0.61.x，`ssh-key` 仍 `0.7.0-rc.10`、`ssh-encoding` 仍 `0.3.0-rc.9`。**无兼容性风险。**

---

## 现有代码事实（来自本仓库，已读源码）

- 依赖锁定 `src-tauri/mt-sidecars/Cargo.toml:34`：
  `russh = { version = "0.61", default-features = false, features = ["ring"] }`
  以及精确锁 `ssh-key = "=0.7.0-rc.10"`、`rsa = "=0.10.0-rc.18"`。
- `Cargo.lock` 实测当前树：`russh 0.61.0`、`ssh-key 0.7.0-rc.10`、`ssh-encoding 0.3.0-rc.9`，**全树无 aws-lc**（grep 验证）。
- Session 持有方式 `src-tauri/mt-sidecars/src/pool.rs:74-119`：
  `CachedSession.handle: Mutex<russh::client::Handle<MtClient>>`，对外暴露
  `pub async fn lock(&self) -> MutexGuard<'_, Handle<MtClient>>`。
- 现有 exec 入口 `src-tauri/mt-sidecars/src/bin/mt-ssh-mcp.rs:306-351`：
  ```rust
  let handle_guard = session.lock().await;
  let mut channel = handle_guard.channel_open_session().await?;
  channel.exec(true, remote_command).await?;
  while let Some(msg) = channel.wait().await { /* ChannelMsg::Data / ExtendedData / ExitStatus */ }
  ```
  → SFTP 方案只需把 `channel.exec(...)` 换成 `channel.request_subsystem(true, "sftp")` 再 `channel.into_stream()`。

---

## 问题 1：`russh-sftp` crate（已验证）

| 项 | 结论 | 证据 |
|---|---|---|
| 最新版本 | **2.3.0** | `cargo search russh-sftp` → `russh-sftp = "2.3.0"`；`cargo info russh-sftp` |
| 依赖 russh 吗 | **否**（仅 dev-dependency） | 其 Cargo.toml `[dependencies]` 只有 tokio/tokio-util/serde/serde_bytes/bitflags/async-trait(opt)/thiserror/chrono/bytes/log/dashmap；`russh = "0.61.0"` 在 `[dev-dependencies]` |
| 与 russh 0.61 兼容 | **是**（trait 级对接，无版本绑定） | 通过 `tokio::io::AsyncRead + AsyncWrite` 接 `channel.into_stream()`；russh 官方 example 用的就是 0.61 |
| 会拉 aws-lc 吗 | **不会** | 见下「实测验证」；它的依赖全是 pure-Rust 通用 crate |
| 协议版本 | SFTP v3（draft-ietf-secsh-filexfer-02，最通用） | README |
| License | Apache-2.0 | crates.io |

**实测验证（临时工程，未改动本仓库）**：在一个空 crate 里写
`russh = {version="0.61", default-features=false, features=["ring"]}` + `russh-sftp = "2.3.0"`，
`cargo generate-lockfile` 后：
- `grep -iE 'aws-lc|aws_lc' Cargo.lock` → **NONE**
- russh 单一版本（0.61.x），`ssh-key 0.7.0-rc.10`、`ssh-encoding 0.3.0-rc.9` 与本仓库一致。

### Cargo.toml 依赖写法（追加到 `[dependencies]`）

```toml
# SFTP over the existing russh 0.61 session（task 06-09）。
# russh-sftp 不依赖 russh 本身（russh 仅在它的 dev-deps），通过 AsyncRead+AsyncWrite
# 接 channel.into_stream()，故不引入第二个 russh、不触发 aws-lc 的 NASM 工具链，
# 也不动 ssh-key/ssh-encoding 的现有精确锁。实测依赖树零 aws-lc。
russh-sftp = "2.3.0"
```
（无需开任何 feature；`async-trait` feature 仅 server 端用得到，client 不需要。）

### 最小上传/下载示例（适配本仓库 pool 架构）

```rust
use russh_sftp::client::SftpSession;

/// 在已 acquire 到的 session 上开一个 sftp channel，做整文件上传。
async fn sftp_upload(
    session: &mt_sidecars::pool::CachedSession,
    remote_path: &str,
    data: &[u8],
) -> Result<(), String> {
    let handle_guard = session.lock().await;                       // 锁串行化同 session
    let channel = handle_guard
        .channel_open_session()
        .await
        .map_err(|e| format!("channel_open_session failed: {e}"))?;
    channel
        .request_subsystem(true, "sftp")                           // ← SFTP 入口
        .await
        .map_err(|e| format!("request_subsystem(sftp) failed: {e}"))?;
    // into_stream() 消费 channel，得到 AsyncRead+AsyncWrite 的双向流；
    // SftpSession 在其上跑 SFTP 协议。
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| format!("sftp handshake failed: {e}"))?;
    sftp.write(remote_path, data)                                   // 高层：整文件写
        .await
        .map_err(|e| format!("sftp write '{remote_path}' failed: {e}"))?;
    sftp.close().await.ok();                                        // best-effort
    drop(handle_guard);
    Ok(())
}

/// 整文件下载。
async fn sftp_download(
    session: &mt_sidecars::pool::CachedSession,
    remote_path: &str,
) -> Result<Vec<u8>, String> {
    let handle_guard = session.lock().await;
    let channel = handle_guard.channel_open_session().await
        .map_err(|e| format!("channel_open_session failed: {e}"))?;
    channel.request_subsystem(true, "sftp").await
        .map_err(|e| format!("request_subsystem(sftp) failed: {e}"))?;
    let sftp = SftpSession::new(channel.into_stream()).await
        .map_err(|e| format!("sftp handshake failed: {e}"))?;
    let bytes = sftp.read(remote_path).await                        // 高层：整文件读 → Vec<u8>
        .map_err(|e| format!("sftp read '{remote_path}' failed: {e}"))?;
    sftp.close().await.ok();
    drop(handle_guard);
    Ok(bytes)
}
```

**大文件流式（避免一次性进内存）**——`SftpSession::open_with_flags` 返回的 `File`
实现 `AsyncRead + AsyncWrite + AsyncSeek`，可分块：
```rust
use russh_sftp::protocol::OpenFlags;
use tokio::io::AsyncWriteExt; // write_all / flush
let mut f = sftp
    .open_with_flags(remote_path, OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE)
    .await?;
f.write_all(chunk).await?;
f.flush().await?;            // 或 f.sync_all()
f.shutdown().await?;
```

### `SftpSession` 关键 client API（来自 src/client/session.rs，已查证）

| 方法 | 签名要点 | 用途 |
|---|---|---|
| `SftpSession::new(stream)` | `async`，入参 `S: AsyncRead+AsyncWrite+Unpin+Send` | 握手建会话 |
| `read(path)` | `-> Vec<u8>` | 整文件下载（**整文件进内存**） |
| `write(path, &[u8])` | `-> ()` | 整文件上传 |
| `open` / `create` / `open_with_flags(.., OpenFlags)` | `-> File` | 拿到流式 `File`（大文件） |
| `try_exists` / `metadata` / `read_dir` / `remove_file` / `rename` / `create_dir` 等 | — | 远端文件系统操作 |
| `set_timeout(secs)` / `close()` | — | 超时、收尾 |

---

## 问题 2：russh 自身是否内置/示例 SFTP（已验证）

- **russh 官方仓库 examples 目录里有 `sftp_client.rs` 和 `sftp_server.rs`**
  （`github.com/Eugeny/russh` master examples 列表已确认）。
- 该官方 `sftp_client.rs` **直接 `use russh_sftp::client::SftpSession;`**，核心三行与上面示例完全一致：
  ```rust
  let channel = session.channel_open_session().await.unwrap();
  channel.request_subsystem(true, "sftp").await.unwrap();
  let sftp = SftpSession::new(channel.into_stream()).await.unwrap();
  ```
- russh 本体**不内置** SFTP 子系统实现；**官方推荐的 SFTP 配套 crate 就是 `russh-sftp`**。
  russh-sftp README 明确致谢 russh 作者 Eugeny "for his prompt help and finalization of Russh API" —— `Channel::into_stream()` 这个 API 正是为对接 russh-sftp 而补的。

---

## 问题 4：channel subsystem 调用方式（已验证，russh v0.61.0 源码）

来自 `russh/src/channels/mod.rs`（tag `v0.61.0`）：

```rust
// line 249（Channel<Msg> 上）/ line 550（另一个 channel 类型上），签名一致：
pub async fn request_subsystem<A: Into<String>>(
    &self,
    want_reply: bool,
    name: A,
) -> Result<(), Error>

// line 661：消费 channel，得到双向流
/// Consume the [`Channel`] to produce a bidirectionnal stream,
/// sending and receiving [`ChannelMsg::Data`] as `AsyncRead` + `AsyncWrite`.
pub fn into_stream(self) -> ChannelStream<S>
```

`ChannelStream<S>`（`russh/src/channels/channel_stream.rs`，v0.61.0）：
- `impl<S> AsyncRead for ChannelStream<S>`
- `impl<S> AsyncWrite for ChannelStream<S>`

→ 即 `channel.into_stream()` 产出的流恰好满足 `SftpSession::new()` 的入参约束。SFTP 入口流程：
`handle.channel_open_session()` → `channel.request_subsystem(true, "sftp")` → `channel.into_stream()` → `SftpSession::new(stream)`。

**与本仓库 pool 架构的契合点（重要）**：
- `into_stream(self)` **消费** channel。本仓库 channel 是临时局部变量（从 `handle_guard.channel_open_session()` 开出），move 进 `into_stream()` 天然 OK。
- channel/stream 内部通过 mpsc sender 与 session 事件循环通信（源码 `ChannelTx::new(self.write_half.sender.clone(), ...)`），**不借用 `&Handle`**。因此 stream 的生命周期不被 `handle_guard` 借用绑死。
- 但 SFTP 是「一开 channel 后多轮请求/响应」的长交互，整个传输期间该 channel 必须存活；本仓库用 `Mutex<Handle>` 串行化同 session 的 channel 操作，**SFTP 传输全程应持有 `session.lock()`**（与现有 exec 一致），否则并发开第二个 channel 会打乱。大文件传输会长时间占住该 session 锁——见风险点。

---

## 问题 3：备选方案对比

### 备选 A：手写 SFTP 协议子集（open/write/read/close/stat）over `channel.make_writer()/make_reader()`

- **依赖写法**：零新增 crate（用 russh 现有 `Channel::make_writer()` / `make_reader_ext()` 拿 `AsyncWrite`/`AsyncRead`，自己按 RFC draft-ietf-secsh-filexfer-02 拼 SFTP v3 wire 包）。
- **ring / 无 NASM 约束**：天然满足（不引入任何新 crate）。
- **与 russh 0.61 兼容性**：满足（`make_reader`/`make_writer` 在 v0.61.0 `channels/mod.rs:162/427` 存在）。
- **最小示例（概念）**：手动 `request_subsystem(true,"sftp")` 后，先发 `SSH_FXP_INIT(version=3)`、读 `SSH_FXP_VERSION`；上传走 `SSH_FXP_OPEN`(flags=WRITE|CREATE|TRUNC) → 循环 `SSH_FXP_WRITE`(offset, data) → `SSH_FXP_CLOSE`；下载走 `SSH_FXP_OPEN`(READ) → `SSH_FXP_FSTAT` 取 size → 循环 `SSH_FXP_READ`(offset,len) 直到 EOF → `SSH_FXP_CLOSE`。每个包是 `u32 length || u8 type || u32 request_id || payload`，字符串为 `u32 len || bytes`。
- **风险/工作量**：**工作量大、风险高**。要自管 request_id、并发窗口、handle 字节串、ATTRS 结构、错误码（SSH_FX_*）、packet 长度边界、UTF-8/路径编码、EOF 语义；还要写大量单测。等于重造 russh-sftp 的 client 一半。**不推荐**，除非要砍掉 dashmap/chrono 等依赖（不值当）。

### 备选 B：`bssh-russh-sftp = "2.3.0"`（russh-sftp 的临时 fork）

- crates.io 描述：`Temporary fork of russh-sftp 2.3.0 adding pipelined SFTP File I/O (write_all_pipelined …)`。
- **价值**：流水线化 `File` I/O，大文件上传吞吐更高。
- **约束**：是 fork，依赖结构与 russh-sftp 2.3.0 应一致（同样不依赖 russh），但**未实测其依赖树**（推测同样无 aws-lc）。
- **风险**：「Temporary fork」维护性不确定，非官方。**仅当上游 russh-sftp 大文件吞吐成为瓶颈时再考虑**；MVP 不必。

### 备选 C：spawn 系统 `sftp` / `scp` 子进程

- 与本 sidecar「已从 spawn-ssh 重构到 russh 持久池」的方向背道而驰（pool.rs 头部注释明确否定了 spawn 路径），且 Windows 上 `sftp.exe` 不一定存在、要重新喂密码/密钥。**不推荐**，仅记录排除理由。

---

## 推荐方案 + 备选汇总

| 方案 | 依赖写法 | ring/无 NASM | russh 0.61 兼容 | 风险 |
|---|---|---|---|---|
| **推荐：russh-sftp 2.3.0** | `russh-sftp = "2.3.0"` | ✅ 实测零 aws-lc | ✅ trait 级对接，官方 example 钦定 | 低；大文件 `read()` 整进内存 + 传输全程占 session 锁 |
| 备选 A：手写 SFTP 子集 | 零新增 crate（用 make_reader/writer） | ✅ | ✅ | 高（重造轮子、协议细节多、要大量测试） |
| 备选 B：bssh-russh-sftp 2.3.0 | `bssh-russh-sftp = "2.3.0"` | 推测 ✅（未实测） | ✅（fork 自 2.3.0） | 中（非官方 fork，维护性不确定） |

---

## 风险点 / 落地注意（针对推荐方案）

1. **大文件内存**：`SftpSession::read(path) -> Vec<u8>` 把整文件读进内存；上传 `write(path, &[u8])` 同理需先持全量。大文件请改用 `open_with_flags` → `File` 的 `AsyncRead/AsyncWrite/AsyncSeek` 分块流式。MCP 工具返回体仍受现有 `OUTPUT_CAP_BYTES = 100KB`（mt-ssh-mcp.rs:33）思路约束——下载结果若回传给 agent，应单独定上限并考虑 base64 编码二进制。
2. **session 锁占用时长**：SFTP 传输全程持有 `session.lock()`（沿用现有 channel 串行化语义）。大文件会长时间独占该 connection，阻塞同 connection 的 ssh_exec。可接受（与现有 exec 单 channel 串行一致），但要在工具超时设计上比 exec 的默认 60s 更宽松或可配。
3. **超时**：russh-sftp 有 `SftpSession::set_timeout(secs)`（默认 10s 协议层）；外层仍应套 `tokio::time::timeout`，与现有 exec 的 timeout/evict/retry 模式对齐（mt-ssh-mcp.rs:478-552）。
4. **错误归类**：`request_subsystem` 失败属 transport-level，应复用现有「evict + 重连 + 单次 retry」路径；SFTP 协议层错误（如远端路径不存在/无权限）应作为工具业务错误返回 agent，不要 evict session。
5. **新增依赖体积**：russh-sftp 带入 `dashmap`、`chrono`、`tokio-util`、`serde_bytes` 等（均 pure-Rust，无 NASM）。`chrono` 用于文件时间属性；体积可接受。
6. **二进制安全**：下载内容是二进制，序列化进 MCP JSON 结果时需 base64（现有 exec 是 UTF-8 lossy，不适用于二进制文件）。
7. **版本浮动注意**：本仓库 Cargo.lock 锁 russh 0.61.0，临时 probe 解析到 0.61.2（均 ^0.61 兼容）。加 russh-sftp 后建议 `cargo update -p russh-sftp --precise 2.3.0` 之外不动其他锁，并复核 `Cargo.lock` 确认 `ssh-key`/`ssh-encoding`/`rsa` 行未变、无 aws-lc。

---

## 已验证 vs 推测

**已验证（来自 crates.io / GitHub 源码 / 本地 cargo 实测）**：
- russh-sftp 最新版 = 2.3.0；其 `[dependencies]` 不含 russh（russh 仅在 dev-deps）。
- 加 russh-sftp 2.3.0 后依赖树零 aws-lc；russh 单一 0.61.x；ssh-key 0.7.0-rc.10、ssh-encoding 0.3.0-rc.9 与本仓库锁一致。
- russh v0.61.0 源码确有 `Channel::request_subsystem(&self, want_reply, name)` 与 `Channel::into_stream(self) -> ChannelStream<S>`，且 `ChannelStream` impl `AsyncRead + AsyncWrite`。
- russh 官方 examples 有 `sftp_client.rs`/`sftp_server.rs`，且直接用 russh-sftp。
- `SftpSession` 提供 `read`/`write`/`open_with_flags` 等 client API（源码 src/client/session.rs 已列举）。

**推测（未逐项实测）**：
- 备选 B `bssh-russh-sftp` 的完整依赖树同样无 aws-lc（基于它是 russh-sftp 2.3.0 的 fork 推断，未实跑 lockfile）。
- 具体 `File` 流式分块的最优 chunk size 与窗口行为（未压测）。

---

## 相关本仓库文件

| 文件 | 关系 |
|---|---|
| `src-tauri/mt-sidecars/Cargo.toml:34-52` | 依赖锁定（ring/精确锁约束源头） |
| `src-tauri/mt-sidecars/src/pool.rs:74-119` | `CachedSession` + `lock()` —— SFTP 从这里拿 Handle 开 channel |
| `src-tauri/mt-sidecars/src/bin/mt-ssh-mcp.rs:306-351` | 现有 `run_exec_on_session`：SFTP 工具的直接模板（exec → request_subsystem+into_stream） |
| `src-tauri/mt-sidecars/Cargo.lock` | 现树实测无 aws-lc，russh 0.61.0 |
| `.trellis/tasks/archive/06-06-ssh-mcp-pkcs1-rsa-key/` | 同类「依赖锁不能拉 aws-lc」约束的前车之鉴 |
