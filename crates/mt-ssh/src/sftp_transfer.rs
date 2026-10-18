//! SFTP 上传 / 下载的实现(`pool::run_sftp_upload_on_session` /
//! `pool::run_sftp_download_on_session` 的内核)。
//!
//! 与 `sftp.rs` 的 [`SftpHandle`](crate::SftpHandle) 不同,这里直接用 russh-sftp 的
//! [`RawSftpSession`]:高层 `SftpSession` 不暴露服务器通告的扩展(替换要用
//! `posix-rename@openssh.com`),它的 `File` 读文件时也一次只有一个请求在途。
//! `RawSftpSession` 的请求方法都只借 `&self`,同一个会话上可以同时挂很多个请求。
//!
//! - **锁**:会话锁只罩「开 channel + 请求 sftp 子系统」
//!   ([`CachedSession::open_sftp_stream`]);传输期间只持 SFTP lease(reaper / LRU
//!   不回收,结束时刷新 `last_used`),同一连接上的 exec、别的传输与 `acquire` 的存活
//!   检查都不再排队等它。
//! - **下载**:按偏移切块,约 1 MiB 的读请求同时在途,结果按偏移顺序落盘;短读从实际
//!   位置补读,读到 EOF 为止。落盘方式与此前一致:目标旁的临时文件写完再改名替换,
//!   失败(含外层超时丢弃 future)删掉临时文件。
//! - **上传**:见 [`upload`]。
//!
//! 整段超时默认由调用方在外面套 `tokio::time::timeout`,这里只把协议层的每请求超时放宽到
//! 同一个窗口;上传另有带整体时限的入口(`pool::run_sftp_upload_until`),到点由这里收尾
//! 并写明远端文件的状态。上传的替换阶段一旦开始就在独立任务里跑完,不受外层取消影响。

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::stream::{FuturesOrdered, FuturesUnordered, StreamExt};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::client::RawSftpSession;
use russh_sftp::protocol::{Data, FileAttributes, FileType, OpenFlags, Packet, Status, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::pool::{CachedSession, SftpTransferError};

/// 服务器没通告 `limits@openssh.com` 时每个读 / 写请求的数据量。SFTP 草案要求服务器
/// 至少支持 34000 字节的包,32 KiB 数据加包头在所有实现上都安全(OpenSSH 客户端同值)。
const DEFAULT_CHUNK_BYTES: u64 = 32 * 1024;
/// 服务器通告的上限再大也只用到 64 KiB:窗口按字节算,块越大在途请求越少,吞吐不会更高。
const MAX_CHUNK_BYTES: u64 = 64 * 1024;
/// 同时在途的字节窗口。russh 客户端 channel 窗口默认 2 MiB,留一半余量,免得服务器的
/// 应答被窗口卡住;100 ms RTT 下单条传输上限约 10 MB/s(此前 8 KiB 一个请求、下载串行)。
const WINDOW_BYTES: u64 = 1024 * 1024;
const MIN_REQUESTS_IN_FLIGHT: usize = 4;
const MAX_REQUESTS_IN_FLIGHT: usize = 64;
/// 服务器通告了包长上限时,数据块再让出这么多字节给包头(类型、id、handle、偏移、长度)。
const PACKET_HEADROOM: u64 = 1024;
/// 软链最多跟几跳(Linux `MAXSYMLINKS` 同值)。
const MAX_SYMLINK_HOPS: usize = 40;
/// 传输被取消(外层超时)后,后台删除远端临时文件最多等这么久。
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
/// 远端文件名的长度上限(`NAME_MAX`)。临时名超长时截短原名部分。
const MAX_NAME_BYTES: usize = 255;
/// 本地临时文件名撞上(残留)时换个序号重试的次数。
const LOCAL_PARTIAL_ATTEMPTS: usize = 16;
const POSIX_RENAME_EXTENSION: &str = "posix-rename@openssh.com";
const LIMITS_EXTENSION: &str = "limits@openssh.com";

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 把 russh-sftp 的**协议层每请求超时**(`RawSftpSession::set_timeout`,默认仅 10s)
/// 放宽到与外层 `tokio::time::timeout` 传输窗口一致。
///
/// 关键:russh-sftp 的 10s 超时是**逐个 SFTP 请求包**(每次 read/write 一个块、
/// 每次 open)各自计时的,不是整段传输。慢链路 / 拥塞下单个块等待 >10s 就会以
/// 一个晦涩的协议错中断整段传输,而此时外层 300s 窗口远未到。故按外层超时窗口同步放宽,
/// 让真正的「整段超时」由外层 `tokio::time::timeout` 统一裁决(见 prd Technical Notes)。
fn sftp_request_timeout_secs(transfer_timeout: Duration) -> u64 {
    // 至少 1s;并以外层窗口为上限,避免协议层比外层还早超时。
    transfer_timeout.as_secs().max(1)
}

/// 每个请求的数据量:服务器通告了上限就取它(不超过 64 KiB),否则 32 KiB;
/// 通告了包长上限时再给包头让出余量。
fn chunk_len(data_limit: Option<u64>, packet_limit: Option<u64>) -> u32 {
    let mut len = data_limit
        .filter(|&limit| limit > 0)
        .map_or(DEFAULT_CHUNK_BYTES, |limit| limit.min(MAX_CHUNK_BYTES));
    if let Some(packet) = packet_limit.filter(|&limit| limit > PACKET_HEADROOM) {
        len = len.min(packet - PACKET_HEADROOM);
    }
    // 不超过 64 KiB,转 u32 不会截断。
    len.max(1) as u32
}

/// 同时在途的请求数:约 1 MiB 的字节窗口,夹在 4 ~ 64 之间。
fn requests_in_flight(chunk: u32) -> usize {
    let requests = WINDOW_BYTES / u64::from(chunk.max(1));
    usize::try_from(requests)
        .unwrap_or(MAX_REQUESTS_IN_FLIGHT)
        .clamp(MIN_REQUESTS_IN_FLIGHT, MAX_REQUESTS_IN_FLIGHT)
}

/// 下载时要不要再发一个读请求。`size_hint`(fstat 拿到的大小)只限制预读多远:
/// 起点不超过它的请求照发(起点恰好等于它的那个用来确认 EOF / 发现文件长了),再往后
/// 就等手头的结果;一个都不在途时总要发,保证文件变长时仍有进展。
fn should_issue_read(next_offset: u64, size_hint: Option<u64>, in_flight: usize) -> bool {
    match size_hint {
        None => true,
        Some(size) => next_offset <= size || in_flight == 0,
    }
}

/// `posix-rename@openssh.com` 的请求体:两个 SSH string(4 字节大端长度 + 内容)。
fn ssh_string_pair(first: &str, second: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + first.len() + second.len());
    for value in [first, second] {
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    out
}

fn is_status(error: &SftpError, code: StatusCode) -> bool {
    matches!(error, SftpError::Status(status) if status.status_code == code)
}

/// 服务器没报权限位时无从判断类型,按普通文件处理(与此前的行为一致)。
fn is_regular_file(attrs: &FileAttributes) -> bool {
    attrs.permissions.is_none() || attrs.file_type() == FileType::File
}

fn is_directory(attrs: &FileAttributes) -> bool {
    attrs.permissions.is_some() && attrs.file_type() == FileType::Dir
}

fn sftp_error(message: String) -> SftpTransferError {
    SftpTransferError::Sftp(message)
}

/// `deadline` 为 `None` 时照常跑完;否则到点丢弃 `future`,返回 `None`。
async fn within<F: Future>(deadline: Option<tokio::time::Instant>, future: F) -> Option<F::Output> {
    match deadline {
        None => Some(future.await),
        Some(deadline) => tokio::time::timeout_at(deadline, future).await.ok(),
    }
}

/// 把「删不掉临时文件」之类的附注拼到错误后面。
fn with_note(message: String, note: Option<String>) -> SftpTransferError {
    match note {
        Some(note) => sftp_error(format!("{message}; {note}")),
        None => sftp_error(message),
    }
}

/// 一次 `lstat` 的结论:不存在是 `Ok(None)`;其它失败原样返回 —— 应答丢了、超时了
/// 都**不**等于文件不在。
type Probe = Result<Option<FileAttributes>, SftpError>;

fn describe_probe(probe: &Probe) -> String {
    match probe {
        Ok(None) => "missing".into(),
        Ok(Some(_)) => "present".into(),
        Err(error) => format!("unknown ({error})"),
    }
}

/// 拆出远端路径的目录前缀(含结尾的 `/`,相对路径为空串)与文件名。
fn split_remote_leaf(path: &str) -> Result<(&str, &str), SftpTransferError> {
    let (dir, name) = match path.rfind('/') {
        Some(index) => path.split_at(index + 1),
        None => ("", path),
    };
    if name.is_empty() || name == "." || name == ".." {
        return Err(sftp_error(format!(
            "remote path '{path}' does not name a file"
        )));
    }
    Ok((dir, name))
}

/// 软链内容 → 它指向的路径。相对目标按软链所在目录拼接,不做 `..` 的字面化简:
/// 交给服务器逐段解析,与内核跟随这条软链的结果一致(中间目录本身是软链时也对)。
fn link_target_path(link: &str, target: &str) -> String {
    if target.starts_with('/') {
        return target.to_string();
    }
    let dir = match link.rfind('/') {
        Some(index) => &link[..=index],
        None => "",
    };
    format!("{dir}{target}")
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// 目标同目录下的隐藏临时名 `.<名字>.mt-<role>-<pid>-<纳秒>-<序号>`,与远程编辑器 /
/// 文件树上传的暂存名同一格式。同目录保证改名是同一文件系统内的原子操作;名字逐次
/// 唯一,同一目标的两次并发上传各写各的。原名过长时截短,整个名字不超过 255 字节。
fn remote_temp_sibling(target: &str, role: &str) -> Result<String, SftpTransferError> {
    let (dir, name) = split_remote_leaf(target)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let suffix = format!(".mt-{role}-{}-{nanos}-{sequence}", std::process::id());
    let keep = MAX_NAME_BYTES.saturating_sub(suffix.len() + 1);
    Ok(format!("{dir}.{}{suffix}", truncate_utf8(name, keep)))
}

/// 本地临时文件名:`<目标>.<pid>-<序号>.mt-sftp-partial`,与目标同目录(同盘,改名是
/// 原子替换而不是跨盘拷贝);后缀让用户一眼认出「这是没传完的残留」。名字逐次唯一:
/// 同一路径的两次并发下载各写各的,不会互相截断。
fn local_partial_path(target: &str) -> String {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{target}.{}-{sequence}.mt-sftp-partial", std::process::id())
}

// ============================================================================
// SFTP 会话
// ============================================================================

/// 一次传输用的 SFTP 会话:原始会话 + 握手时协商出的能力。克隆共享同一个会话(上传的
/// 替换阶段在独立任务里用它)。
#[derive(Clone)]
struct TransferSftp {
    raw: Arc<RawSftpSession>,
    posix_rename: bool,
    read_chunk: u32,
    write_chunk: u32,
}

impl TransferSftp {
    /// 开 channel(只在这一步持会话锁)并完成 SFTP 握手。失败都算 transport 级。
    async fn open(
        session: &CachedSession,
        transfer_timeout: Duration,
    ) -> Result<Self, SftpTransferError> {
        let stream = session.open_sftp_stream().await?;
        let mut raw = RawSftpSession::new(stream);
        let version = raw
            .init()
            .await
            .map_err(|e| SftpTransferError::Transport(format!("sftp handshake failed: {e}")))?;
        let advertised = |name: &str| version.extensions.get(name).is_some_and(|v| v == "1");
        // limits 拿不到不算失败:按保守的块大小走即可。
        let limits = if advertised(LIMITS_EXTENSION) {
            raw.limits().await.ok().map(Limits::from)
        } else {
            None
        };
        if let Some(limits) = limits {
            raw.set_limits(limits);
        }
        raw.set_timeout(sftp_request_timeout_secs(transfer_timeout));
        Ok(Self {
            posix_rename: advertised(POSIX_RENAME_EXTENSION),
            read_chunk: chunk_len(
                limits.and_then(|l| l.read_len),
                limits.and_then(|l| l.packet_len),
            ),
            write_chunk: chunk_len(
                limits.and_then(|l| l.write_len),
                limits.and_then(|l| l.packet_len),
            ),
            raw: Arc::new(raw),
        })
    }

    /// 关掉 SFTP channel(best-effort)。只在正常走完(含已知失败)时调用:
    /// 被取消时后台清理任务还要用这条 channel 删临时文件,由它持有的引用收尾。
    fn close(&self) {
        let _ = self.raw.close_session();
    }

    async fn probe(&self, path: &str) -> Probe {
        match self.raw.lstat(path).await {
            Ok(attrs) => Ok(Some(attrs.attrs)),
            Err(error) if is_status(&error, StatusCode::NoSuchFile) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), SftpError> {
        self.raw.rename(from, to).await.map(|_| ())
    }

    /// `posix-rename@openssh.com`:目标存在时原子覆盖(标准 rename 在目标存在时失败)。
    async fn posix_rename(&self, from: &str, to: &str) -> Result<(), SftpError> {
        match self
            .raw
            .extended(POSIX_RENAME_EXTENSION, ssh_string_pair(from, to))
            .await?
        {
            Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
            Packet::Status(status) => Err(SftpError::Status(status)),
            _ => Err(SftpError::UnexpectedPacket),
        }
    }

    /// 删远端临时文件;删不掉时返回一句写进错误信息的说明(含路径)。
    async fn remove_staging(&self, path: &str) -> Option<String> {
        match self.raw.remove(path).await {
            Ok(_) => None,
            Err(error) if is_status(&error, StatusCode::NoSuchFile) => None,
            Err(error) => Some(format!(
                "the partial upload at '{path}' could not be removed: {error}"
            )),
        }
    }
}

// ============================================================================
// 下载
// ============================================================================

pub(crate) async fn download(
    session: &CachedSession,
    remote_path: &str,
    local_path: &str,
    transfer_timeout: Duration,
) -> Result<u64, SftpTransferError> {
    let _lease = session.sftp_lease();
    let sftp = TransferSftp::open(session, transfer_timeout).await?;
    let result = download_file(&sftp, remote_path, local_path).await;
    sftp.close();
    result
}

async fn download_file(
    sftp: &TransferSftp,
    remote_path: &str,
    local_path: &str,
) -> Result<u64, SftpTransferError> {
    // 先打开远端文件 —— 远端不存在 / 无权限是业务错,失败时无需建本地临时文件。
    let handle = sftp
        .raw
        .open(remote_path, OpenFlags::READ, FileAttributes::empty())
        .await
        .map_err(|e| sftp_error(format!("sftp open '{remote_path}' failed: {e}")))?
        .handle;
    let result = download_open_file(sftp, &handle, remote_path, local_path).await;
    let _ = sftp.raw.close(handle.as_str()).await; // best-effort
    result
}

async fn download_open_file(
    sftp: &TransferSftp,
    handle: &str,
    remote_path: &str,
    local_path: &str,
) -> Result<u64, SftpTransferError> {
    // 大小只当提示:决定预读多远。真正的终点以 EOF 为准(文件可能边读边长)。
    let size_hint = match sftp.raw.fstat(handle).await {
        Ok(attrs) if is_directory(&attrs.attrs) => {
            return Err(sftp_error(format!(
                "remote path '{remote_path}' is a directory"
            )));
        }
        Ok(attrs) => attrs.attrs.size,
        Err(_) => None,
    };
    let (partial, file) = create_local_partial(local_path).await?;
    let mut cleanup = LocalPartialCleanup(Some(&partial));
    // 文件句柄在 cleanup 之后绑定,提前返回时先关句柄、再删文件。
    let mut file = file;
    let reader = RemoteReader {
        raw: &sftp.raw,
        handle,
        path: remote_path,
    };
    let mut sink = LocalSink {
        file: &mut file,
        path: &partial,
        written: 0,
    };
    read_pipelined(&reader, &mut sink, sftp.read_chunk, size_hint).await?;
    let copied = sink.written;
    file.flush()
        .await
        .map_err(|e| sftp_error(format!("flush local file '{partial}' failed: {e}")))?;
    drop(file);
    // 成功:改名替换目标(Windows 上 std 的 rename 覆盖已存在的目标)。
    tokio::fs::rename(&partial, local_path).await.map_err(|e| {
        sftp_error(format!(
            "failed to move downloaded file into place '{local_path}': {e}"
        ))
    })?;
    cleanup.0 = None;
    Ok(copied)
}

/// 在目标旁边排他创建本地临时文件;名字撞上(多半是崩溃留下的残留)就换个序号。
async fn create_local_partial(
    target: &str,
) -> Result<(String, tokio::fs::File), SftpTransferError> {
    let mut attempt = 0;
    loop {
        let path = local_partial_path(target);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt += 1;
                if attempt >= LOCAL_PARTIAL_ATTEMPTS {
                    return Err(sftp_error(format!(
                        "cannot create local file '{path}': {e}"
                    )));
                }
            }
            Err(e) => {
                return Err(sftp_error(format!(
                    "cannot create local file '{path}': {e}"
                )));
            }
        }
    }
}

/// 本地临时文件的兜底清理:出错返回或整个传输被取消(外层超时丢弃 future)时删掉它。
/// 成功改名后把路径清空即解除。
struct LocalPartialCleanup<'a>(Option<&'a str>);

impl Drop for LocalPartialCleanup<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct RemoteReader<'a> {
    raw: &'a RawSftpSession,
    handle: &'a str,
    path: &'a str,
}

impl RemoteReader<'_> {
    fn read_error(&self, offset: u64, error: &SftpError) -> SftpTransferError {
        sftp_error(format!(
            "sftp read '{}' at offset {offset} failed: {error}",
            self.path
        ))
    }

    /// 服务器给的不能比要的多:多了说明协议对不上,按出错处理而不是把多余的字节落盘。
    fn check_len(&self, offset: u64, requested: u32, got: usize) -> Result<(), SftpTransferError> {
        if got as u64 > u64::from(requested) {
            return Err(sftp_error(format!(
                "sftp read '{}' at offset {offset}: the server returned {got} bytes for a \
                 {requested}-byte request",
                self.path
            )));
        }
        Ok(())
    }
}

struct LocalSink<'a> {
    file: &'a mut tokio::fs::File,
    path: &'a str,
    written: u64,
}

impl LocalSink<'_> {
    async fn write(&mut self, bytes: &[u8]) -> Result<(), SftpTransferError> {
        self.file
            .write_all(bytes)
            .await
            .map_err(|e| sftp_error(format!("write local file '{}' failed: {e}", self.path)))?;
        self.written += bytes.len() as u64;
        Ok(())
    }
}

async fn read_at(
    raw: &RawSftpSession,
    handle: &str,
    offset: u64,
    len: u32,
) -> (u64, u32, Result<Data, SftpError>) {
    (offset, len, raw.read(handle, offset, len).await)
}

/// 多个读请求同时在途,结果按偏移顺序写进 `sink`。
///
/// - 请求 `(offset, len)` 首尾相接地排开;`FuturesOrdered` 按提交顺序交回结果,所以
///   处理到的请求总是从「已写到的位置」开始。
/// - 短读(服务器给的少于要的):当场顺序补读这一段剩下的部分,补齐了才处理下一个结果;
///   补读拿到了数据说明服务器有没通告的读上限,之后的请求按观察到的长度缩小。
/// - 读到 EOF(状态码或空数据)即止,之后已发出的请求结果直接丢弃。
async fn read_pipelined(
    reader: &RemoteReader<'_>,
    sink: &mut LocalSink<'_>,
    mut chunk: u32,
    mut size_hint: Option<u64>,
) -> Result<(), SftpTransferError> {
    let mut pending = FuturesOrdered::new();
    let mut next_offset = 0u64;
    loop {
        while pending.len() < requests_in_flight(chunk)
            && should_issue_read(next_offset, size_hint, pending.len())
        {
            pending.push_back(read_at(reader.raw, reader.handle, next_offset, chunk));
            next_offset += u64::from(chunk);
        }
        let Some((offset, len, result)) = pending.next().await else {
            return Ok(());
        };
        let data = match result {
            Ok(data) => data.data,
            Err(error) if is_status(&error, StatusCode::Eof) => return Ok(()),
            Err(error) => return Err(reader.read_error(offset, &error)),
        };
        // 有的服务器用空数据而不是 EOF 状态表示读完了。
        if data.is_empty() {
            return Ok(());
        }
        reader.check_len(offset, len, data.len())?;
        sink.write(&data).await?;
        let got = data.len() as u64;
        if got < u64::from(len) {
            if fill_gap(reader, sink, offset + got, offset + u64::from(len)).await? {
                return Ok(());
            }
            // 补读没碰到 EOF:是服务器的读上限,不是文件尾。
            chunk = chunk.min(got as u32);
        }
        // 读过了 fstat 的大小(文件在长):不再按它限制预读。
        if size_hint.is_some_and(|size| sink.written > size) {
            size_hint = None;
        }
    }
}

/// 顺序补读 `[from, to)`。返回 `true` 表示途中读到了 EOF。
async fn fill_gap(
    reader: &RemoteReader<'_>,
    sink: &mut LocalSink<'_>,
    mut from: u64,
    to: u64,
) -> Result<bool, SftpTransferError> {
    while from < to {
        let len = u32::try_from(to - from).unwrap_or(u32::MAX);
        let data = match reader.raw.read(reader.handle, from, len).await {
            Ok(data) => data.data,
            Err(error) if is_status(&error, StatusCode::Eof) => return Ok(true),
            Err(error) => return Err(reader.read_error(from, &error)),
        };
        if data.is_empty() {
            return Ok(true);
        }
        reader.check_len(from, len, data.len())?;
        sink.write(&data).await?;
        from += data.len() as u64;
    }
    Ok(false)
}

// ============================================================================
// 上传
// ============================================================================

/// 上传:本地文件 → 远端 `remote_path`,返回写入的字节数。
///
/// 1. 目标是软链就逐跳解析到最终文件(悬空软链 → 在它指向的位置新建文件,与 `scp` /
///    `open(O_CREAT)` 跟随悬空软链的行为一致);软链本身不动。最终是目录或特殊文件就
///    报错、不上传。软链落在 sticky 且全局可写的目录里时按内核 `fs.protected_symlinks`
///    的规则把关(见 [`may_follow_symlink`]):在客户端跟随软链就绕开了内核的这道防线,
///    `/tmp` 里别人预埋的软链能把上传引到受害者家目录里去。
/// 2. 在最终文件的目录里排他创建隐藏临时文件,权限位一出生就按目标设好(0600 的文件,
///    新内容在传输期间也不会被别人读到),目标已存在时再把属主 / 属组对齐(root 能整个
///    改过来,普通用户至少能把属组改成自己所在的组)。
/// 3. **对齐不了就改为原地覆盖写**:像 `scp` / `sftp put` 那样截断后写入,原文件的
///    inode、属主、属组、权限、ACL、硬链接全保留;代价是中途失败会留下不完整的文件
///    (错误信息写明)。照常替换则会把属主换成登录用户、属组换成登录用户的组:0640 的
///    文件可能因此被另一个组读到,原属主 / 跑着的服务也可能从此读写不了它 —— 这比
///    「失败时文件不完整」更伤数据,且后者与 `scp` 的行为一致、可预期。目录不让建临时
///    文件、文件本身却可写(如 `/etc` 下组可写的配置)时同样原地写。目标对登录用户不可写
///    时原地覆盖也做不了,报错、文件不动。
/// 4. 流水线写完后补设 setuid / setgid / sticky(写入会清掉前两个;属主没核实一致时不
///    复制这两位),核对服务器上的大小,关闭句柄。
/// 5. 替换:优先 `posix-rename@openssh.com` 原子覆盖;否则「目标 → 备份、临时 → 目标、
///    删备份」,失败回滚。原文件不在原位时**绝不删临时文件**;留下的临时 / 备份路径写进
///    错误信息。替换(含回滚)一旦开始就在独立任务里跑完(见 [`commit_detached`]):外层
///    超时 / 取消落在「目标 → 备份」之后、「临时 → 目标」之前时,目标路径不能就此消失。
///
/// `options.deadline` 给定时(`pool::run_sftp_upload_until`)1~4 步到点即止,返回
/// [`SftpTransferError::TimedOut`] 并写明远端文件的状态(见 [`UploadProgress`]);第 5 步
/// 不受它截断。
pub(crate) async fn upload(
    session: &CachedSession,
    local_path: &str,
    remote_path: &str,
    options: UploadOptions,
) -> Result<u64, SftpTransferError> {
    // 本地文件先打开 —— 本地不存在 / 不可读属业务错,不必去碰远端。
    let mut local = open_local_source(local_path).await?;
    let _lease = session.sftp_lease();
    let deadline = options.deadline;
    let Some(opened) = within(
        deadline,
        TransferSftp::open(session, options.request_timeout),
    )
    .await
    else {
        return Err(SftpTransferError::TimedOut(format!(
            "sftp upload to '{remote_path}' timed out while opening the SFTP channel; the \
             remote file was not changed"
        )));
    };
    let sftp = opened?;
    let progress = UploadProgress::default();
    let staged = within(
        deadline,
        stage_upload(&sftp, &mut local, local_path, remote_path, &progress),
    )
    .await;
    let result = match staged {
        // 到点:收尾已在被丢弃的 future 里安排好(临时文件后台删除),这里只写明状态。
        // 不关 SFTP 会话 —— 后台删除还要用它,最后一个引用放掉时自然关闭。
        None => return Err(progress.timed_out(remote_path)),
        Some(Err(error)) => Err(error),
        Some(Ok(Staged::Written(written))) => Ok(written),
        Some(Ok(Staged::Ready(commit))) => {
            let written = commit.size;
            commit_detached(&sftp, session, options.keep_alive, commit)
                .await
                .map(|()| written)
        }
    };
    sftp.close();
    result
}

/// 上传入口的选项(两个公开入口的差别在这里)。
pub(crate) struct UploadOptions {
    /// 协议层每请求超时(见 [`sftp_request_timeout_secs`])。
    pub(crate) request_timeout: Duration,
    /// 池自己收尾的整体时限;`None` 时由调用方在外面限时。
    pub(crate) deadline: Option<tokio::time::Instant>,
    /// 替换阶段的独立任务持有它,保证会话在替换跑完之前不会因调用方放手而被回收。
    pub(crate) keep_alive: Option<Arc<CachedSession>>,
}

/// 上传走到了哪一步:整体时限到点时据此写明远端文件的状态。
#[derive(Default)]
struct UploadProgress(Mutex<UploadStage>);

#[derive(Default, Clone)]
enum UploadStage {
    /// 解析目标、建临时文件:远端什么都还没写。
    #[default]
    Preparing,
    /// 正往临时文件里写:目标没动过。
    Staging { target: String, staging: String },
    /// 原地覆盖写:目标已被截断,写到一半就是不完整的。
    InPlace { target: String, reason: String },
}

impl UploadProgress {
    fn set(&self, stage: UploadStage) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = stage;
    }

    fn stage(&self) -> UploadStage {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 到点时的报错:写明远端文件处在什么状态。
    fn timed_out(&self, remote_path: &str) -> SftpTransferError {
        SftpTransferError::TimedOut(match self.stage() {
            UploadStage::Preparing => format!(
                "sftp upload to '{remote_path}' timed out before any data was written; the \
                 remote file was not changed"
            ),
            UploadStage::Staging { target, staging } => format!(
                "sftp upload to '{target}' timed out before all data was sent; '{target}' was \
                 not changed (the partial upload at '{staging}' is being removed)"
            ),
            UploadStage::InPlace { target, reason } => format!(
                "sftp upload to '{target}' timed out while overwriting it in place ({reason}); \
                 the remote file '{target}' may now be incomplete"
            ),
        })
    }
}

/// [`stage_upload`] 的结果。
enum Staged {
    /// 原地覆盖写完了(没有替换阶段)。
    Written(u64),
    /// 临时文件写完、关好了,等替换。
    Ready(PendingCommit),
}

/// 写完的临时文件 + 替换它要用的信息。
struct PendingCommit {
    staging: String,
    target: String,
    /// 目标原本存在(要替换它,而不是新建)。
    replacing: bool,
    /// 写出去的字节数(替换后核对用)。
    size: u64,
}

/// 在独立任务里跑替换(含回滚)并等它的结果。调用方的 future 被丢弃(外层超时 / 取消)
/// 时任务照样跑完 —— 半路停下可能让目标路径消失(原文件已挪成备份、新文件还没就位);
/// 结局记进日志。任务持有 SFTP 会话的一份引用、一份 SFTP lease(reaper / LRU 不回收会话)
/// 与调用方给的 `keep_alive`。
async fn commit_detached(
    sftp: &TransferSftp,
    session: &CachedSession,
    keep_alive: Option<Arc<CachedSession>>,
    commit: PendingCommit,
) -> Result<(), SftpTransferError> {
    let worker = sftp.clone();
    let lease = session.detached_sftp_lease();
    let target = commit.target.clone();
    let (reply, outcome) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _lease = lease;
        let _keep_alive = keep_alive;
        let PendingCommit {
            staging,
            target,
            replacing,
            size,
        } = commit;
        let result = commit_staging(&worker, &staging, &target, replacing, size).await;
        if let Err(result) = reply.send(result) {
            match result {
                Ok(()) => eprintln!(
                    "[mt-ssh] upload: the caller stopped waiting while the upload was being moved \
                     into place at '{target}'; the replacement completed"
                ),
                Err(error) => eprintln!(
                    "[mt-ssh] upload: the caller stopped waiting while the upload was being moved \
                     into place at '{target}'; the replacement did not complete: {error}"
                ),
            }
        }
    });
    outcome.await.unwrap_or_else(|_| {
        Err(sftp_error(format!(
            "the task moving the upload into place at '{target}' stopped without a result; check \
             '{target}'"
        )))
    })
}

async fn open_local_source(local_path: &str) -> Result<tokio::fs::File, SftpTransferError> {
    let file = tokio::fs::File::open(local_path)
        .await
        .map_err(|e| sftp_error(format!("cannot open local file '{local_path}': {e}")))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|e| sftp_error(format!("cannot inspect local file '{local_path}': {e}")))?;
    if !metadata.is_file() {
        return Err(sftp_error(format!(
            "local path '{local_path}' is not a regular file"
        )));
    }
    Ok(file)
}

/// 解析后的上传目标。
struct UploadTarget {
    /// 真正要写的路径:软链已逐跳解析掉。
    path: String,
    /// 目标已存在时它的属性(普通文件)。
    existing: Option<FileAttributes>,
}

async fn resolve_upload_target(
    sftp: &TransferSftp,
    remote_path: &str,
) -> Result<UploadTarget, SftpTransferError> {
    split_remote_leaf(remote_path)?;
    let mut path = remote_path.to_string();
    // 登录用户的 uid(猜测),只在真碰上可疑软链时才去查一次。
    let mut login_uid: Option<Option<u32>> = None;
    for _ in 0..=MAX_SYMLINK_HOPS {
        let attrs = match sftp.raw.lstat(path.as_str()).await {
            Ok(attrs) => attrs.attrs,
            Err(error) if is_status(&error, StatusCode::NoSuchFile) => {
                split_remote_leaf(&path)?;
                return Ok(UploadTarget {
                    path,
                    existing: None,
                });
            }
            Err(error) => {
                return Err(sftp_error(format!("sftp lstat '{path}' failed: {error}")));
            }
        };
        let resolved = if path == remote_path {
            String::new()
        } else {
            format!(" (symbolic link resolves to '{path}')")
        };
        if attrs.permissions.is_some() && attrs.file_type() == FileType::Symlink {
            check_symlink_protection(sftp, &path, &attrs, &mut login_uid).await?;
            let link = sftp
                .raw
                .readlink(path.as_str())
                .await
                .map_err(|e| sftp_error(format!("sftp readlink '{path}' failed: {e}")))?
                .files
                .into_iter()
                .next()
                .map(|file| file.filename)
                .filter(|target| !target.is_empty())
                .ok_or_else(|| sftp_error(format!("sftp readlink '{path}' returned no target")))?;
            path = link_target_path(&path, &link);
            continue;
        }
        if is_directory(&attrs) {
            return Err(sftp_error(format!(
                "remote path '{remote_path}' is a directory{resolved}"
            )));
        }
        if !is_regular_file(&attrs) {
            return Err(sftp_error(format!(
                "remote path '{remote_path}' is not a regular file{resolved}"
            )));
        }
        return Ok(UploadTarget {
            path,
            existing: Some(attrs),
        });
    }
    Err(sftp_error(format!(
        "remote path '{remote_path}': too many levels of symbolic links"
    )))
}

/// 路径所在的目录(给 `stat` 用):`/srv/x` → `/srv`,`/x` → `/`,`x` → `.`。
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(index) => &path[..index],
        None => ".",
    }
}

/// Linux `fs.protected_symlinks`(`may_follow_link`)的判定,纯函数便于单测:
///
/// - 软链所在目录不是「sticky 且全局可写」(`mode & 0o1002 == 0o1002`)→ 可以跟随;
/// - 软链属主是目录属主,或是登录用户本人 → 可以跟随;
/// - 其余拒绝。目录权限 / 软链属主不明的一律不跟随(拿不准时从严)。
fn may_follow_symlink(
    dir_mode: Option<u32>,
    dir_uid: Option<u32>,
    link_uid: Option<u32>,
    login_uid: Option<u32>,
) -> bool {
    let Some(mode) = dir_mode else {
        return false;
    };
    if mode & 0o1002 != 0o1002 {
        return true;
    }
    link_uid.is_some_and(|owner| dir_uid == Some(owner) || login_uid == Some(owner))
}

/// 在客户端跟随软链等于绕开了服务器内核的 `fs.protected_symlinks`:多用户服务器上,别人
/// 在 `/tmp` 预埋 `/tmp/build.sh -> /home/victim/.bashrc`,受害者上传到 `/tmp/build.sh`
/// 时,临时文件会建进受害者的家目录、再替换掉 `.bashrc`(以前由服务器 open 这条路径,内核
/// 直接拒绝,什么都不碰)。这里照内核的规则把关(见 [`may_follow_symlink`]),悬空软链
/// 同样适用。
///
/// 登录用户的 uid 取 SFTP 初始目录(`realpath(".")`,通常是家目录)的属主 —— 多半就是
/// 自己;拿不到时只认目录属主(更严)。只在真碰上「sticky 全局可写目录里、属主不是目录
/// 属主的软链」时才去查一次。
async fn check_symlink_protection(
    sftp: &TransferSftp,
    link: &str,
    link_attrs: &FileAttributes,
    login_uid: &mut Option<Option<u32>>,
) -> Result<(), SftpTransferError> {
    let dir = parent_dir(link);
    let dir_attrs = sftp
        .raw
        .stat(dir)
        .await
        .map_err(|e| {
            sftp_error(format!(
                "cannot check the directory '{dir}' that holds the symbolic link '{link}': sftp \
                 stat failed: {e}"
            ))
        })?
        .attrs;
    let suspicious = dir_attrs
        .permissions
        .is_some_and(|mode| mode & 0o1002 == 0o1002)
        && link_attrs.uid.is_some()
        && link_attrs.uid != dir_attrs.uid;
    if suspicious && login_uid.is_none() {
        *login_uid = Some(probe_login_uid(sftp).await);
    }
    let login = login_uid.flatten();
    if may_follow_symlink(dir_attrs.permissions, dir_attrs.uid, link_attrs.uid, login) {
        return Ok(());
    }
    if dir_attrs.permissions.is_none() {
        return Err(sftp_error(format!(
            "refusing to follow the symbolic link '{link}': the server does not report the \
             permissions of '{dir}', so mini-term cannot tell whether it is a sticky \
             world-writable directory where another user's link could redirect the upload; \
             upload to the link's target directly if this is intended"
        )));
    }
    let uid = |uid: Option<u32>| uid.map_or_else(|| "unknown".to_string(), |uid| uid.to_string());
    Err(sftp_error(format!(
        "refusing to follow the symbolic link '{link}' (owned by uid {}) in '{dir}', a sticky \
         world-writable directory owned by uid {}: like the kernel's fs.protected_symlinks, \
         mini-term only follows links there that belong to the directory's owner or to you, \
         because another user's link could redirect the upload anywhere (symlink attack); \
         upload to the link's target directly if this is intended",
        uid(link_attrs.uid),
        uid(dir_attrs.uid)
    )))
}

/// 登录用户 uid 的猜测:SFTP 初始目录(通常是家目录)的属主。
async fn probe_login_uid(sftp: &TransferSftp) -> Option<u32> {
    let home = sftp
        .raw
        .realpath(".")
        .await
        .ok()?
        .files
        .into_iter()
        .next()?;
    sftp.raw.stat(home.filename).await.ok()?.attrs.uid
}

/// [`upload`] 的 1~4 步:解析目标,写临时文件(或原地覆盖写)。替换留给调用方。
async fn stage_upload(
    sftp: &TransferSftp,
    local: &mut tokio::fs::File,
    local_path: &str,
    remote_path: &str,
    progress: &UploadProgress,
) -> Result<Staged, SftpTransferError> {
    let target = resolve_upload_target(sftp, remote_path).await?;
    let staging = remote_temp_sibling(&target.path, "partial")?;
    let mut create_attrs = FileAttributes::empty();
    create_attrs.permissions = target
        .existing
        .as_ref()
        .and_then(|attrs| attrs.permissions)
        .map(|mode| mode & 0o777);
    // 发出创建请求之前就装好清理:整体时限可能恰在请求途中到点,那时文件也许已经建出来了。
    let mut cleanup = StagingCleanup {
        raw: &sftp.raw,
        path: Some(&staging),
    };
    progress.set(UploadStage::Staging {
        target: target.path.clone(),
        staging: staging.clone(),
    });
    let created = sftp
        .raw
        .open(
            staging.as_str(),
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
            create_attrs,
        )
        .await;
    let handle = match created {
        Ok(handle) => handle.handle,
        // 目录不让建文件、文件本身却可写(如 /etc 下组可写的配置):替换做不到,只能像
        // scp 一样原地写;文件也不可写时由原地写那一步报错,文件不动。
        Err(error)
            if target.existing.is_some() && is_status(&error, StatusCode::PermissionDenied) =>
        {
            // 没建出来,无需清理。
            cleanup.path = None;
            let reason = format!("no staging copy can be created next to it ({error})");
            return upload_in_place(sftp, local, local_path, &target.path, &reason, progress)
                .await
                .map(Staged::Written);
        }
        Err(error) => {
            cleanup.path = None;
            return Err(sftp_error(format!(
                "sftp create '{staging}' failed: {error}"
            )));
        }
    };

    let final_mode = match &target.existing {
        None => None,
        Some(existing) => match match_staging_attributes(sftp, &handle, existing).await {
            Ok(StagingFit::Match { mode }) => mode,
            Ok(StagingFit::Mismatch(reason)) => {
                let _ = sftp.raw.close(handle.as_str()).await;
                let note = cleanup.discard(sftp).await;
                let in_place =
                    upload_in_place(sftp, local, local_path, &target.path, &reason, progress).await;
                return match in_place {
                    Err(error) => Err(with_note(error.message().to_string(), note)),
                    Ok(written) => {
                        if let Some(note) = note {
                            eprintln!("[mt-ssh] upload of '{}': {note}", target.path);
                        }
                        Ok(Staged::Written(written))
                    }
                };
            }
            Err(error) => {
                let _ = sftp.raw.close(handle.as_str()).await;
                return Err(cleanup.discard_with(sftp, error).await);
            }
        },
    };

    let written = match write_pipelined(sftp, &handle, &staging, local, local_path).await {
        Ok(written) => written,
        Err(error) => {
            let _ = sftp.raw.close(handle.as_str()).await;
            return Err(cleanup.discard_with(sftp, error).await);
        }
    };
    // 写入会清掉 setuid / setgid:写完再补全部权限位(只有特殊位需要补,best-effort)。
    if let Some(mode) = final_mode.filter(|mode| mode & 0o7000 != 0) {
        let _ = sftp.raw.fsetstat(handle.as_str(), permissions(mode)).await;
    }
    // 替换原文件前最后核对一次:服务器上的大小必须等于写出去的字节数。
    if let Ok(attrs) = sftp.raw.fstat(handle.as_str()).await {
        if let Some(size) = attrs.attrs.size.filter(|&size| size != written) {
            let _ = sftp.raw.close(handle.as_str()).await;
            let error = sftp_error(format!(
                "sftp upload to '{staging}': the server reports {size} bytes after {written} were \
                 written"
            ));
            return Err(cleanup.discard_with(sftp, error).await);
        }
    }
    if let Err(error) = sftp.raw.close(handle.as_str()).await {
        let error = sftp_error(format!("sftp close remote '{staging}' failed: {error}"));
        return Err(cleanup.discard_with(sftp, error).await);
    }

    // 进入替换阶段:临时文件的去留从此由替换流程按原文件的状态决定。
    cleanup.path = None;
    drop(cleanup);
    Ok(Staged::Ready(PendingCommit {
        staging,
        replacing: target.existing.is_some(),
        target: target.path,
        size: written,
    }))
}

fn permissions(mode: u32) -> FileAttributes {
    let mut attrs = FileAttributes::empty();
    attrs.permissions = Some(mode);
    attrs
}

/// 属主与属组一起设:SFTP 的属性里两者共用一个标志位,只给一个时另一个会被当成 0。
fn ownership(uid: u32, gid: u32) -> FileAttributes {
    let mut attrs = FileAttributes::empty();
    attrs.uid = Some(uid);
    attrs.gid = Some(gid);
    attrs
}

/// 临时文件与目标的属性对得上与否。
#[derive(Debug, PartialEq, Eq)]
enum StagingFit {
    /// 属主、属组与权限位都已一致(服务器不报属主时无从比较,也算这类,与此前的
    /// 行为一致)。`mode` 是写完后要补设的完整权限(含特殊位),服务器不报权限时为 `None`。
    Match { mode: Option<u32> },
    /// 对不齐的原因(拼进日志与错误信息)。
    Mismatch(String),
}

/// 纯函数:比较目标与(已尽力对齐的)临时文件的属主、属组与权限位。
fn staging_fit(target: &FileAttributes, staged: &FileAttributes) -> StagingFit {
    let owner_verified = match (target.uid, target.gid) {
        (Some(uid), Some(gid)) => {
            if staged.uid != Some(uid) || staged.gid != Some(gid) {
                let fmt_id =
                    |id: Option<u32>| id.map_or_else(|| "?".to_string(), |id| id.to_string());
                return StagingFit::Mismatch(format!(
                    "its owner {uid}:{gid} cannot be kept by this login (a replacement would be \
                     owned by {}:{})",
                    fmt_id(staged.uid),
                    fmt_id(staged.gid)
                ));
            }
            true
        }
        _ => false,
    };
    if let Some(mode) = target.permissions.map(|mode| mode & 0o777) {
        if staged.permissions.map(|mode| mode & 0o777) != Some(mode) {
            return StagingFit::Mismatch(format!(
                "its permissions {mode:03o} cannot be reproduced on a replacement"
            ));
        }
    }
    let mode = target.permissions.map(|mode| {
        let mode = mode & 0o7777;
        // 属主没核实一致时不复制 setuid / setgid:别让一份换了属主的文件带着特权位。
        if owner_verified {
            mode
        } else {
            mode & !0o6000
        }
    });
    StagingFit::Match { mode }
}

/// 把刚建好的临时文件的权限位与属主 / 属组尽量对齐到目标,再比较结果。
async fn match_staging_attributes(
    sftp: &TransferSftp,
    handle: &str,
    target: &FileAttributes,
) -> Result<StagingFit, SftpTransferError> {
    if let Some(mode) = target.permissions {
        // 先只设 rwx 位(新建时受 umask 影响);特殊位写完再补。
        let _ = sftp.raw.fsetstat(handle, permissions(mode & 0o777)).await;
    }
    let mut staged = staging_attributes(sftp, handle).await?;
    if let (Some(uid), Some(gid)) = (target.uid, target.gid) {
        if staged.uid != Some(uid) || staged.gid != Some(gid) {
            // root 能把属主属组整个改过来;普通用户至少能把属组改成自己所在的组。
            if sftp
                .raw
                .fsetstat(handle, ownership(uid, gid))
                .await
                .is_err()
            {
                if let Some(own_uid) = staged.uid.filter(|_| staged.gid != Some(gid)) {
                    let _ = sftp.raw.fsetstat(handle, ownership(own_uid, gid)).await;
                }
            }
            staged = staging_attributes(sftp, handle).await?;
        }
    }
    Ok(staging_fit(target, &staged))
}

async fn staging_attributes(
    sftp: &TransferSftp,
    handle: &str,
) -> Result<FileAttributes, SftpTransferError> {
    sftp.raw
        .fstat(handle)
        .await
        .map(|attrs| attrs.attrs)
        .map_err(|e| sftp_error(format!("sftp fstat of the staging file failed: {e}")))
}

/// 远端临时文件的清理责任。
///
/// - 已知的失败路径上显式 [`discard`](Self::discard),删不掉就把路径写进错误;
/// - 传输被取消(外层超时丢弃 future)时在 drop 里起一个后台任务删它(最多等 10s);
/// - 进入替换阶段前解除:此后临时文件的去留由替换流程按原文件的状态决定,绝不在原文件
///   不在原位时删掉它。
struct StagingCleanup<'a> {
    raw: &'a Arc<RawSftpSession>,
    path: Option<&'a str>,
}

impl StagingCleanup<'_> {
    async fn discard(&mut self, sftp: &TransferSftp) -> Option<String> {
        let path = self.path?;
        let note = sftp.remove_staging(path).await;
        // 删完才解除:删到一半被丢弃(整体时限 / 取消)时,drop 里照样补删。
        self.path = None;
        note
    }

    async fn discard_with(
        &mut self,
        sftp: &TransferSftp,
        error: SftpTransferError,
    ) -> SftpTransferError {
        match self.discard(sftp).await {
            None => error,
            Some(note) => sftp_error(format!("{}; {note}", error.message())),
        }
    }
}

impl Drop for StagingCleanup<'_> {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let raw = Arc::clone(self.raw);
        let path = path.to_string();
        runtime.spawn(async move {
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, raw.remove(path)).await;
        });
    }
}

/// 替换做不到或会改掉属主 / 权限时的退路:像 scp 一样原地截断后写入,原文件的 inode
/// 与全部属性不变。写到一半被打断(出错、整体时限到点、调用方取消)时文件可能不完整:
/// 出错与到点都写进报错,取消没有报错可写,记一行日志。
async fn upload_in_place(
    sftp: &TransferSftp,
    local: &mut tokio::fs::File,
    local_path: &str,
    target: &str,
    reason: &str,
    progress: &UploadProgress,
) -> Result<u64, SftpTransferError> {
    eprintln!(
        "[mt-ssh] upload: '{target}' cannot be replaced safely: {reason}; overwriting it in place"
    );
    // 截断在 open 时就可能已经生效:发出 open 之前就记下「原地写」。
    progress.set(UploadStage::InPlace {
        target: target.to_string(),
        reason: reason.to_string(),
    });
    let mut interrupted = InPlaceInterrupted(Some(target));
    let result = overwrite_in_place(sftp, local, local_path, target, reason).await;
    interrupted.0 = None;
    result
}

/// 原地覆盖写被丢弃(调用方取消 / 超时)时记一行日志:那时没有报错可写,文件可能不完整。
struct InPlaceInterrupted<'a>(Option<&'a str>);

impl Drop for InPlaceInterrupted<'_> {
    fn drop(&mut self) {
        if let Some(target) = self.0.take() {
            eprintln!(
                "[mt-ssh] upload: overwriting '{target}' in place was interrupted (cancelled or \
                 timed out); the remote file may now be incomplete"
            );
        }
    }
}

async fn overwrite_in_place(
    sftp: &TransferSftp,
    local: &mut tokio::fs::File,
    local_path: &str,
    target: &str,
    reason: &str,
) -> Result<u64, SftpTransferError> {
    let handle = sftp
        .raw
        .open(
            target,
            OpenFlags::WRITE | OpenFlags::TRUNCATE,
            FileAttributes::empty(),
        )
        .await
        .map_err(|e| {
            sftp_error(format!(
                "cannot replace '{target}' safely: {reason}; opening it for an in-place overwrite \
                 failed too ({e}), so the file was left untouched"
            ))
        })?
        .handle;
    let written = write_pipelined(sftp, &handle, target, local, local_path).await;
    let closed = sftp.raw.close(handle.as_str()).await;
    let incomplete =
        format!("'{target}' was being overwritten in place ({reason}) and may now be incomplete");
    match (written, closed) {
        (Ok(written), Ok(_)) => Ok(written),
        (Err(error), _) => Err(sftp_error(format!("{}; {incomplete}", error.message()))),
        (Ok(_), Err(error)) => Err(sftp_error(format!(
            "sftp close '{target}' failed: {error}; {incomplete}"
        ))),
    }
}

async fn read_full(file: &mut tokio::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let read = file.read(&mut buf[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

async fn write_at(
    raw: &RawSftpSession,
    handle: &str,
    offset: u64,
    data: Vec<u8>,
) -> (u64, Result<Status, SftpError>) {
    (offset, raw.write(handle, offset, data).await)
}

/// 本地文件从头流式写到远端句柄:多个写请求同时在途(约 1 MiB),任一失败即止。
async fn write_pipelined(
    sftp: &TransferSftp,
    handle: &str,
    remote_path: &str,
    local: &mut tokio::fs::File,
    local_path: &str,
) -> Result<u64, SftpTransferError> {
    let chunk = sftp.write_chunk as usize;
    let max_in_flight = requests_in_flight(sftp.write_chunk);
    let mut in_flight = FuturesUnordered::new();
    let mut offset = 0u64;
    let mut local_done = false;
    loop {
        while !local_done && in_flight.len() < max_in_flight {
            let mut buf = vec![0u8; chunk];
            let read = read_full(local, &mut buf)
                .await
                .map_err(|e| sftp_error(format!("read local file '{local_path}' failed: {e}")))?;
            local_done = read < chunk;
            if read == 0 {
                break;
            }
            buf.truncate(read);
            in_flight.push(write_at(&sftp.raw, handle, offset, buf));
            offset += read as u64;
        }
        match in_flight.next().await {
            None => return Ok(offset),
            Some((_, Ok(_))) => {}
            Some((at, Err(error))) => {
                return Err(sftp_error(format!(
                    "sftp write to '{remote_path}' at offset {at} failed: {error}"
                )));
            }
        }
    }
}

// ============================================================================
// 替换
// ============================================================================

async fn commit_staging(
    sftp: &TransferSftp,
    staging: &str,
    target: &str,
    replacing: bool,
    size: u64,
) -> Result<(), SftpTransferError> {
    if sftp.posix_rename {
        commit_with_posix_rename(sftp, staging, target, replacing, size).await
    } else if replacing {
        commit_with_backup(sftp, staging, target, size).await
    } else {
        commit_new_target(sftp, staging, target, size).await
    }
}

/// 改名请求报错、临时文件却已经不见了:多半是改名完成了、只是应答丢了。目标是普通
/// 文件且大小对得上才算成功。
async fn accept_lost_reply(
    sftp: &TransferSftp,
    staging: &str,
    target: &str,
    size: u64,
    error: &SftpError,
) -> Result<(), SftpTransferError> {
    let state = sftp.probe(target).await;
    match &state {
        Ok(Some(attrs)) if is_regular_file(attrs) && attrs.size.is_none_or(|got| got == size) => {
            eprintln!(
                "[mt-ssh] upload: the reply to moving '{staging}' into place was lost ({error}), \
                 but '{target}' now has the uploaded size"
            );
            Ok(())
        }
        _ => Err(sftp_error(format!(
            "moving the upload into place at '{target}' failed ({error}) and the staging file \
             '{staging}' is gone; '{target}' is {}",
            describe_probe(&state)
        ))),
    }
}

/// `posix-rename` 是原子的:要么完成了(只是应答丢了),要么什么都没发生。
async fn commit_with_posix_rename(
    sftp: &TransferSftp,
    staging: &str,
    target: &str,
    replacing: bool,
    size: u64,
) -> Result<(), SftpTransferError> {
    let Err(error) = sftp.posix_rename(staging, target).await else {
        return Ok(());
    };
    match sftp.probe(staging).await {
        Ok(Some(_)) => {
            // 没发生:原文件(若有)仍应在原位。核实了才删临时文件。
            if replacing {
                let state = sftp.probe(target).await;
                if !matches!(state, Ok(Some(_))) {
                    return Err(sftp_error(format!(
                        "cannot move the upload into place at '{target}': {error}; '{target}' is \
                         now {}, so the uploaded data was kept at '{staging}'",
                        describe_probe(&state)
                    )));
                }
            }
            let note = sftp.remove_staging(staging).await;
            Err(with_note(
                format!("cannot move the upload into place at '{target}': {error}"),
                note,
            ))
        }
        Ok(None) => accept_lost_reply(sftp, staging, target, size, &error).await,
        Err(probe) => Err(sftp_error(format!(
            "cannot move the upload into place at '{target}': {error}; the outcome is unknown \
             ({probe}); the uploaded data may remain at '{staging}'"
        ))),
    }
}

/// 目标原本不存在、服务器又没有 `posix-rename`:标准 rename 即可(目标在此期间被别人
/// 建出来时它会失败,那份文件从来不是我们的,删掉自己的临时文件即可)。
async fn commit_new_target(
    sftp: &TransferSftp,
    staging: &str,
    target: &str,
    size: u64,
) -> Result<(), SftpTransferError> {
    let Err(error) = sftp.rename(staging, target).await else {
        return Ok(());
    };
    match sftp.probe(staging).await {
        Ok(Some(_)) => {
            let note = sftp.remove_staging(staging).await;
            Err(with_note(
                format!("cannot move the upload into place at '{target}': {error}"),
                note,
            ))
        }
        Ok(None) => accept_lost_reply(sftp, staging, target, size, &error).await,
        Err(probe) => Err(sftp_error(format!(
            "cannot move the upload into place at '{target}': {error}; the outcome is unknown \
             ({probe}); the uploaded data may remain at '{staging}'"
        ))),
    }
}

/// 没有 `posix-rename` 时替换已存在的目标:目标 → 备份、临时 → 目标、删备份,失败回滚。
/// 与远程编辑器保存(`SftpHandle::replace_file_contents`)同一套隔离 / 就位 / 回滚步骤;
/// 每一步失败都先看清现场再决定,原文件不在原位时绝不删临时文件。
async fn commit_with_backup(
    sftp: &TransferSftp,
    staging: &str,
    target: &str,
    size: u64,
) -> Result<(), SftpTransferError> {
    let backup = match remote_temp_sibling(target, "backup") {
        Ok(backup) => backup,
        Err(error) => {
            let note = sftp.remove_staging(staging).await;
            return Err(with_note(error.message().to_string(), note));
        }
    };

    // 1. 原文件挪到备份名。
    if let Err(error) = sftp.rename(target, &backup).await {
        let target_state = sftp.probe(target).await;
        let backup_state = sftp.probe(&backup).await;
        match (&target_state, &backup_state) {
            (Ok(Some(_)), Ok(None)) => {
                let note = sftp.remove_staging(staging).await;
                return Err(with_note(
                    format!("cannot move the original '{target}' aside to replace it: {error}"),
                    note,
                ));
            }
            // 改名其实完成了,只是应答丢了:接着走。
            (Ok(None), Ok(Some(_))) => {}
            _ => {
                return Err(sftp_error(format!(
                    "cannot move the original '{target}' aside to replace it: {error}; the outcome \
                     is unknown (target {}, backup {}); the original may be at '{backup}', the \
                     uploaded data is at '{staging}'",
                    describe_probe(&target_state),
                    describe_probe(&backup_state)
                )));
            }
        }
    }

    // 2. 临时文件就位。
    if let Err(error) = sftp.rename(staging, target).await {
        let staging_state = sftp.probe(staging).await;
        let target_state = sftp.probe(target).await;
        match (&staging_state, &target_state) {
            // 应答丢了,其实已就位。
            (Ok(None), Ok(Some(attrs)))
                if is_regular_file(attrs) && attrs.size.is_none_or(|got| got == size) => {}
            (Ok(Some(_)), Ok(None)) => {
                return Err(restore_original(sftp, staging, target, &backup, &error).await);
            }
            _ => {
                return Err(sftp_error(format!(
                    "cannot move the upload into place at '{target}': {error}; the outcome is \
                     unknown (staging {}, target {}); the original is at '{backup}', the uploaded \
                     data may be at '{staging}'",
                    describe_probe(&staging_state),
                    describe_probe(&target_state)
                )));
            }
        }
    }

    // 3. 删备份。删不掉不影响结果(新内容已就位),只记一行。
    if let Err(error) = sftp.raw.remove(backup.as_str()).await {
        eprintln!(
            "[mt-ssh] upload: '{target}' was replaced, but the previous version left at \
             '{backup}' could not be removed: {error}"
        );
    }
    Ok(())
}

/// 临时文件没能就位:把原文件从备份挪回去。挪回去了才删临时文件。
async fn restore_original(
    sftp: &TransferSftp,
    staging: &str,
    target: &str,
    backup: &str,
    error: &SftpError,
) -> SftpTransferError {
    let restored = match sftp.rename(backup, target).await {
        Ok(()) => Ok(()),
        // 应答丢了也可能其实挪回去了:看现场。
        Err(rollback) => match (sftp.probe(target).await, sftp.probe(backup).await) {
            (Ok(Some(_)), Ok(None)) => Ok(()),
            _ => Err(rollback),
        },
    };
    match restored {
        Ok(()) => {
            let note = sftp.remove_staging(staging).await;
            with_note(
                format!(
                    "cannot move the upload into place at '{target}': {error}; the original was \
                     restored"
                ),
                note,
            )
        }
        Err(rollback) => sftp_error(format!(
            "cannot move the upload into place at '{target}': {error}; restoring the original \
             also failed: {rollback}; the original is at '{backup}' and the uploaded data at \
             '{staging}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(uid: Option<u32>, gid: Option<u32>, mode: Option<u32>) -> FileAttributes {
        let mut attrs = FileAttributes::empty();
        attrs.uid = uid;
        attrs.gid = gid;
        attrs.permissions = mode;
        attrs
    }

    #[test]
    fn sftp_request_timeout_tracks_transfer_window() {
        // 协议层每请求超时随外层传输窗口放宽,而非停在默认 10s。
        assert_eq!(sftp_request_timeout_secs(Duration::from_secs(300)), 300);
        assert_eq!(sftp_request_timeout_secs(Duration::from_secs(60)), 60);
        // 下限保护:0 秒窗口至少返 1s,不会出现 0 秒立即超时。
        assert_eq!(sftp_request_timeout_secs(Duration::from_secs(0)), 1);
        // 亚秒窗口(被 caller .max(1) 前可能出现)也至少 1s。
        assert_eq!(sftp_request_timeout_secs(Duration::from_millis(500)), 1);
    }

    #[test]
    fn chunk_len_follows_server_limits_within_bounds() {
        // 没通告:32 KiB(所有服务器都支持)。
        assert_eq!(chunk_len(None, None), 32 * 1024);
        // 通告了更大的(OpenSSH 约 255 KiB):封顶 64 KiB。
        assert_eq!(chunk_len(Some(261_120), Some(262_144)), 64 * 1024);
        // 通告了更小的:照它来。
        assert_eq!(chunk_len(Some(16_384), None), 16_384);
        // 0 = 没限制(Limits 已把 0 换成 None,这里再兜一层)。
        assert_eq!(chunk_len(Some(0), None), 32 * 1024);
        // 包长上限给包头让出余量。
        assert_eq!(chunk_len(None, Some(20_000)), 20_000 - 1024);
        // 荒唐的包长上限不予理会。
        assert_eq!(chunk_len(None, Some(512)), 32 * 1024);
    }

    #[test]
    fn requests_in_flight_keep_about_one_mib_in_the_air() {
        assert_eq!(requests_in_flight(32 * 1024), 32);
        assert_eq!(requests_in_flight(64 * 1024), 16);
        assert_eq!(requests_in_flight(1), MAX_REQUESTS_IN_FLIGHT);
        assert_eq!(requests_in_flight(0), MAX_REQUESTS_IN_FLIGHT);
        assert_eq!(requests_in_flight(1024 * 1024), MIN_REQUESTS_IN_FLIGHT);
    }

    #[test]
    fn read_ahead_stops_one_request_past_the_known_size() {
        // 不知道大小:一直预读。
        assert!(should_issue_read(10 << 20, None, 32));
        // 起点不超过已知大小的照发,恰好等于大小的那个用来确认 EOF。
        assert!(should_issue_read(96 * 1024, Some(100 * 1024), 3));
        assert!(should_issue_read(100 * 1024, Some(100 * 1024), 4));
        assert!(!should_issue_read(128 * 1024, Some(100 * 1024), 5));
        // 一个都不在途时总要发(文件变长了也有进展)。
        assert!(should_issue_read(128 * 1024, Some(100 * 1024), 0));
        // 空文件:只发偏移 0 那一个。
        assert!(should_issue_read(0, Some(0), 0));
        assert!(!should_issue_read(32 * 1024, Some(0), 1));
    }

    #[test]
    fn posix_rename_payload_is_two_ssh_strings() {
        assert_eq!(
            ssh_string_pair("/a", "b c"),
            [&[0, 0, 0, 2][..], b"/a", &[0, 0, 0, 3], b"b c"].concat()
        );
        assert_eq!(ssh_string_pair("", ""), vec![0u8; 8]);
    }

    #[test]
    fn link_targets_resolve_against_the_link_directory() {
        assert_eq!(link_target_path("/srv/app/current", "/opt/x"), "/opt/x");
        assert_eq!(
            link_target_path("/srv/app/current", "releases/v2"),
            "/srv/app/releases/v2"
        );
        // `..` 不做字面化简,交给服务器按真实目录解析。
        assert_eq!(
            link_target_path("/home/u/link.txt", "../../srv/data/real.txt"),
            "/home/u/../../srv/data/real.txt"
        );
        // 相对路径的软链(相对于 SFTP 初始目录)。
        assert_eq!(link_target_path("link", "target"), "target");
        assert_eq!(link_target_path("dir/link", "target"), "dir/target");
        assert_eq!(link_target_path("/link", "target"), "/target");
    }

    #[test]
    fn remote_temp_names_are_hidden_unique_siblings() {
        let first = remote_temp_sibling("/srv/app/config.yml", "partial").unwrap();
        let second = remote_temp_sibling("/srv/app/config.yml", "partial").unwrap();
        assert!(
            first.starts_with("/srv/app/.config.yml.mt-partial-"),
            "{first}"
        );
        assert_ne!(first, second);
        assert!(remote_temp_sibling("/notes.md", "backup")
            .unwrap()
            .starts_with("/.notes.md.mt-backup-"));
        // 相对路径(相对 SFTP 初始目录)留在同一个相对目录里。
        assert!(remote_temp_sibling("a.txt", "partial")
            .unwrap()
            .starts_with(".a.txt.mt-partial-"));
        assert!(remote_temp_sibling("dir/a.txt", "partial")
            .unwrap()
            .starts_with("dir/.a.txt.mt-partial-"));
        for invalid in ["", "/srv/app/", "/srv/..", "."] {
            let error = remote_temp_sibling(invalid, "partial").unwrap_err();
            assert!(
                error.message().contains("does not name a file"),
                "{invalid}"
            );
        }
    }

    #[test]
    fn remote_temp_names_stay_within_name_max() {
        let long = "长".repeat(100); // 300 字节
        let target = format!("/srv/{long}");
        let temp = remote_temp_sibling(&target, "partial").unwrap();
        let name = temp.strip_prefix("/srv/").unwrap();
        assert!(name.len() <= MAX_NAME_BYTES, "{} bytes", name.len());
        assert!(name.starts_with(".长"), "{name}");
        assert!(name.contains(".mt-partial-"), "{name}");
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("a长", 2), "a");
    }

    /// Git Bash 下远端绝对路径写成 `//var/log/app.log`(躲开路径改写;POSIX 上等价于
    /// `/var/log/app.log`):父目录、临时名、软链目标都在同一个 `//` 前缀下推导。
    #[test]
    fn double_slash_remote_paths_keep_their_prefix() {
        assert_eq!(
            split_remote_leaf("//var/log/app.log").unwrap(),
            ("//var/log/", "app.log")
        );
        assert_eq!(split_remote_leaf("//app.log").unwrap(), ("//", "app.log"));
        assert!(split_remote_leaf("//").is_err());
        let temp = remote_temp_sibling("//var/log/app.log", "partial").unwrap();
        assert!(temp.starts_with("//var/log/.app.log.mt-partial-"), "{temp}");
        assert_eq!(
            link_target_path("//var/log/current", "app.log"),
            "//var/log/app.log"
        );
        assert_eq!(link_target_path("//var/log/current", "/opt/x"), "/opt/x");
        assert_eq!(parent_dir("//tmp/build.sh"), "//tmp");
        assert_eq!(parent_dir("//build.sh"), "/");
    }

    #[test]
    fn local_partial_names_sit_next_to_the_target_and_never_repeat() {
        let first = local_partial_path(r"D:\dl\a.zip");
        let second = local_partial_path(r"D:\dl\a.zip");
        assert!(first.starts_with(r"D:\dl\a.zip."), "{first}");
        assert!(first.ends_with(".mt-sftp-partial"), "{first}");
        assert_ne!(first, second);
        assert!(local_partial_path("/tmp/my file").starts_with("/tmp/my file."));
    }

    #[test]
    fn staging_fit_requires_the_same_owner_and_permission_bits() {
        let target = attrs(Some(1000), Some(33), Some(0o100640));
        // 一致:写完补设完整权限。
        assert_eq!(
            staging_fit(&target, &attrs(Some(1000), Some(33), Some(0o100640))),
            StagingFit::Match { mode: Some(0o640) }
        );
        // 属组没对上 → 原地覆盖。
        let StagingFit::Mismatch(reason) =
            staging_fit(&target, &attrs(Some(1000), Some(1000), Some(0o100640)))
        else {
            panic!("group mismatch must not replace");
        };
        assert!(
            reason.contains("1000:33") && reason.contains("1000:1000"),
            "{reason}"
        );
        // 属主没对上 → 原地覆盖。
        let other_owner = attrs(Some(33), Some(33), Some(0o100664));
        assert!(matches!(
            staging_fit(&other_owner, &attrs(Some(1000), Some(33), Some(0o100664))),
            StagingFit::Mismatch(_)
        ));
        // 临时文件不报属主:无从核实 → 原地覆盖。
        assert!(matches!(
            staging_fit(&target, &attrs(None, None, Some(0o100640))),
            StagingFit::Mismatch(_)
        ));
        // 权限位设不上 → 原地覆盖。
        let StagingFit::Mismatch(reason) =
            staging_fit(&target, &attrs(Some(1000), Some(33), Some(0o100644)))
        else {
            panic!("permission mismatch must not replace");
        };
        assert!(reason.contains("640"), "{reason}");
    }

    #[test]
    fn staging_fit_copies_privilege_bits_only_with_a_verified_owner() {
        let setuid = attrs(Some(1000), Some(1000), Some(0o104755));
        assert_eq!(
            staging_fit(&setuid, &attrs(Some(1000), Some(1000), Some(0o100755))),
            StagingFit::Match { mode: Some(0o4755) }
        );
        // 服务器不报属主:照旧替换,但不复制 setuid / setgid(sticky 照留)。
        let unknown_owner = attrs(None, None, Some(0o107755));
        assert_eq!(
            staging_fit(&unknown_owner, &attrs(None, None, Some(0o100755))),
            StagingFit::Match { mode: Some(0o1755) }
        );
        // 服务器连权限都不报:照旧替换,不补设权限。
        assert_eq!(
            staging_fit(&attrs(None, None, None), &attrs(None, None, None)),
            StagingFit::Match { mode: None }
        );
    }

    #[test]
    fn file_kind_helpers_tolerate_missing_permissions() {
        assert!(is_regular_file(&attrs(None, None, None)));
        assert!(is_regular_file(&attrs(None, None, Some(0o100600))));
        assert!(!is_regular_file(&attrs(None, None, Some(0o040755))));
        assert!(!is_regular_file(&attrs(None, None, Some(0o120777))));
        assert!(is_directory(&attrs(None, None, Some(0o040755))));
        assert!(!is_directory(&attrs(None, None, None)));
    }

    #[test]
    fn protected_symlinks_rule_matches_the_kernel() {
        const STICKY_WORLD: Option<u32> = Some(0o041777);
        const ROOT: Option<u32> = Some(0);
        const ME: Option<u32> = Some(1000);
        const OTHER: Option<u32> = Some(1001);
        // (目录权限, 目录属主, 软链属主, 登录用户, 能否跟随)
        let cases = [
            // 普通目录:谁的软链都跟随(与内核一致)。
            (Some(0o040755), ME, OTHER, None, true),
            // 只 sticky 不全局可写、只全局可写不 sticky:都不是受保护的目录。
            (Some(0o041775), ROOT, OTHER, None, true),
            (Some(0o040777), ROOT, OTHER, None, true),
            // /tmp:别人的软链拒绝;目录属主(root)的、自己的照常跟随。
            (STICKY_WORLD, ROOT, OTHER, ME, false),
            (STICKY_WORLD, ROOT, ROOT, ME, true),
            (STICKY_WORLD, ROOT, ME, ME, true),
            // 登录用户拿不准:只认目录属主。
            (STICKY_WORLD, ROOT, ME, None, false),
            // 属主 / 权限不明:不跟随。
            (STICKY_WORLD, ROOT, None, ME, false),
            (None, ROOT, ROOT, ROOT, false),
        ];
        for (dir_mode, dir_uid, link_uid, login_uid, expected) in cases {
            assert_eq!(
                may_follow_symlink(dir_mode, dir_uid, link_uid, login_uid),
                expected,
                "dir {dir_mode:?} owned by {dir_uid:?}, link owned by {link_uid:?}, login \
                 {login_uid:?}"
            );
        }
    }

    #[test]
    fn parent_dirs_for_stat() {
        assert_eq!(parent_dir("/tmp/build.sh"), "/tmp");
        assert_eq!(parent_dir("/build.sh"), "/");
        assert_eq!(parent_dir("build.sh"), ".");
        assert_eq!(parent_dir("dir/link"), "dir");
    }

    #[test]
    fn timed_out_uploads_say_what_happened_to_the_remote_file() {
        let progress = UploadProgress::default();
        let message = progress.timed_out("/srv/a.txt").message().to_string();
        assert!(message.contains("before any data was written"), "{message}");
        assert!(message.contains("was not changed"), "{message}");

        progress.set(UploadStage::Staging {
            target: "/srv/a.txt".into(),
            staging: "/srv/.a.txt.mt-partial-1".into(),
        });
        let error = progress.timed_out("/srv/a.txt");
        assert!(error.is_timeout());
        let message = error.message();
        assert!(
            message.contains("'/srv/a.txt' was not changed"),
            "{message}"
        );
        assert!(message.contains("/srv/.a.txt.mt-partial-1"), "{message}");

        progress.set(UploadStage::InPlace {
            target: "/etc/app.conf".into(),
            reason: "its owner 0:33 cannot be kept".into(),
        });
        let message = progress.timed_out("/etc/app.conf").message().to_string();
        assert!(
            message.contains("the remote file '/etc/app.conf' may now be incomplete"),
            "{message}"
        );
        assert!(message.contains("overwriting it in place"), "{message}");
    }

    #[test]
    fn probe_descriptions_never_claim_absence_on_errors() {
        assert_eq!(describe_probe(&Ok(None)), "missing");
        assert_eq!(
            describe_probe(&Ok(Some(FileAttributes::empty()))),
            "present"
        );
        assert!(describe_probe(&Err(SftpError::Timeout)).starts_with("unknown"));
    }
}
