//! ipc —— mt-ssh-cli 与 daemon 之间的本机 IPC 传输与帧协议。
//!
//! 传输(spec §2):
//! - Windows:named pipe `\\.\pipe\mini-term.ssh-cli.<用户标识>`,pipe 的
//!   security descriptor 限**仅当前用户**可连;
//! - macOS/Linux:Unix domain socket(`$XDG_RUNTIME_DIR` 优先,回退 config.json
//!   同目录),权限 0600。
//!
//! 反方向同样要核验:客户端连上后先确认**服务端进程属于当前用户**再收发任何数据
//! (见 [`connect`])。上面的 ACL / 0600 只挡得住「别的用户连我的 daemon」,挡不住
//! 「别的用户抢先占住端点冒充 daemon」—— 管道名全局可见、用户 SID 也不是秘密,
//! 冒充者能看到 agent 的命令与能力令牌,还能往 agent 上下文里塞伪造的输出和退出码。
//!
//! 协议:newline-delimited JSON 帧(serde camelCase,`v` 字段版本号)。
//! - 连接建立后 daemon 先发 `{type:"hello", version, protocolVersion, pid}`;
//! - 请求(CLI→daemon,单行):`{v:2, op, projectToken?, connection?, ...}`;
//! - 响应流(daemon→CLI,多行直至终帧):stdout/stderr 分片帧(base64 保
//!   二进制安全)→ 终帧 result / error(message 绝不含密码)。

use serde::{Deserialize, Serialize};

use crate::ssh_service::SshConnectionView;

/// 请求帧协议版本。与二进制版本(hello 帧的 `version`)独立:协议版本管字段
/// 兼容性,二进制版本管「app 升级后旧 daemon 换代」的握手。
pub const PROTOCOL_VERSION: u32 = 2;
/// rev4 前 daemon 的协议版本；旧 hello 没有 `protocolVersion`，兼容时按 v1 处理。
pub const LEGACY_PROTOCOL_VERSION: u32 = 1;

/// 请求的操作类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Op {
    List,
    Exec,
    Upload,
    Download,
    Status,
    Shutdown,
}

impl Op {
    /// 传输类 op 对应的方向;非传输 op 返回 None。收敛散落的 if 级联。
    pub fn transfer_direction(self) -> Option<crate::ssh_service::TransferDirection> {
        match self {
            Op::Upload => Some(crate::ssh_service::TransferDirection::Upload),
            Op::Download => Some(crate::ssh_service::TransferDirection::Download),
            _ => None,
        }
    }
}

/// CLI → daemon 的请求帧(单行 JSON)。
///
/// 字段按 op 选用:list 只用 `project_token`;exec 用 `connection`/`command`/
/// `cwd`/`timeout_secs`;upload/download 用 `connection`/`local_path`/
/// `remote_path`/`timeout_secs`(`local_path` 已由 CLI 端绝对化);
/// status/shutdown 无业务字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub v: u32,
    pub op: Option<Op>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_path: Option<String>,
}

/// daemon → CLI 的响应帧(每帧单行 JSON,`type` 字段区分)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerFrame {
    /// 连接建立后 daemon 主动发出：二进制版本 + 协议版本 + pid。
    ///
    /// `protocol_version` 对反序列化保留可选：v1 hello 没有此字段，新 CLI
    /// 读到 `None` 时按 [`LEGACY_PROTOCOL_VERSION`] 收尸并换代。
    Hello {
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol_version: Option<u32>,
        pid: u32,
    },
    /// exec 的远程 stdout 分片(base64 保二进制安全)。
    Stdout { data_b64: String },
    /// exec 的远程 stderr 分片。
    Stderr { data_b64: String },
    /// 成功终帧,按 op 携带对应字段。
    Result {
        /// exec:远程退出码(超时 / 远端未上报时缺省)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// exec:是否超时。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timed_out: Option<bool>,
        /// exec:超时发生在远程命令启动之前(建连 / 认证 / 开 channel 阶段),命令没有
        /// 执行(0.4.11 起)。只在为 true 时出现 —— 与 `activeRequests` 同样是加字段不升
        /// 协议版本:旧 CLI 忽略它,照旧按普通超时打印。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_started: Option<bool>,
        /// upload/download:传输字节数。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        /// list:连接视图(不含敏感字段)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connections: Option<Vec<SshConnectionView>>,
        /// status:池内 session 数。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sessions: Option<usize>,
        /// status:除这条 status 请求自身外,daemon 手上仍在处理的连接数(0.4.11 起)。
        ///
        /// 加字段不升协议版本:旧 CLI 解码时忽略不认识的字段,新 CLI 读旧 daemon 的
        /// 回复得到 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_requests: Option<usize>,
    },
    /// 失败终帧。message 绝不含密码(service 层同一纪律)。
    Error { message: String },
}

impl ServerFrame {
    /// 全空的 result 终帧(shutdown ack 等无业务字段场景)。
    pub fn result_empty() -> Self {
        ServerFrame::Result {
            exit_code: None,
            timed_out: None,
            not_started: None,
            bytes: None,
            connections: None,
            sessions: None,
            active_requests: None,
        }
    }

    /// exec 的 result 终帧。`not_started` 只在为 true 时上线(见字段注释)。
    pub fn result_exec(exit_code: Option<i32>, timed_out: bool, not_started: bool) -> Self {
        ServerFrame::Result {
            exit_code,
            timed_out: Some(timed_out),
            not_started: not_started.then_some(true),
            bytes: None,
            connections: None,
            sessions: None,
            active_requests: None,
        }
    }

    /// upload/download 的 result 终帧。
    pub fn result_bytes(bytes: u64) -> Self {
        ServerFrame::Result {
            exit_code: None,
            timed_out: None,
            not_started: None,
            bytes: Some(bytes),
            connections: None,
            sessions: None,
            active_requests: None,
        }
    }

    /// list 的 result 终帧。
    pub fn result_connections(connections: Vec<SshConnectionView>) -> Self {
        ServerFrame::Result {
            exit_code: None,
            timed_out: None,
            not_started: None,
            bytes: None,
            connections: Some(connections),
            sessions: None,
            active_requests: None,
        }
    }

    /// status 的 result 终帧:池内 session 数 + 其它仍在处理的连接数。
    pub fn result_status(sessions: usize, active_requests: usize) -> Self {
        ServerFrame::Result {
            exit_code: None,
            timed_out: None,
            not_started: None,
            bytes: None,
            connections: None,
            sessions: Some(sessions),
            active_requests: Some(active_requests),
        }
    }
}

/// 把一帧编码成单行 JSON(含结尾换行)。
///
/// serde_json 对 String 字段会转义内嵌换行,因此输出保证恰好一行。
pub fn encode_frame<T: Serialize>(frame: &T) -> Result<String, String> {
    let mut s = serde_json::to_string(frame).map_err(|e| format!("encode frame failed: {e}"))?;
    s.push('\n');
    Ok(s)
}

/// 从一行文本解码一帧。
pub fn decode_frame<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, String> {
    serde_json::from_str(line.trim_end()).map_err(|e| format!("decode frame failed: {e}"))
}

/// stdout/stderr 分片的 base64 编码(标准字母表带 padding)。
pub fn b64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// stdout/stderr 分片的 base64 解码。
pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("invalid base64 payload: {e}"))
}

// ---------------------------------------------------------------------------
// 端点定位
// ---------------------------------------------------------------------------

/// 计算本机默认 IPC 端点。
///
/// - Windows:named pipe 名(全局单 daemon,按用户隔离);
/// - Unix:socket 文件路径字符串。
pub fn default_endpoint() -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\mini-term.ssh-cli.{}", current_user_tag())
    }
    #[cfg(unix)]
    {
        socket_path().to_string_lossy().to_string()
    }
}

/// 把任意用户标识清洗成可安全嵌入 pipe/文件名的形式(仅保留字母数字与 `-`)。
fn sanitize_tag(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

/// Windows:当前用户标识(SID 字符串,取不到时回退用户名)。
///
/// 直接用 SID 原文(清洗后)而非哈希:SID 字符集本就适合 pipe 名,且**跨二进制
/// 版本稳定**——若用哈希,升级前后的 CLI/daemon 可能算出不同名字,版本握手
/// 换代机制就失效了。
#[cfg(windows)]
fn current_user_tag() -> String {
    match windows_security::current_user_sid_string() {
        Ok(sid) => sanitize_tag(&sid),
        Err(e) => {
            eprintln!("[mt-ssh-cli] cannot resolve user SID ({e}); falling back to username");
            sanitize_tag(&std::env::var("USERNAME").unwrap_or_else(|_| "default".into()))
        }
    }
}

/// Unix:socket 文件路径。`$XDG_RUNTIME_DIR`(天然按 uid 隔离、0700)优先,
/// 回退 config.json 同目录,再兜底 temp(带 uid 后缀避免跨用户撞名)。
#[cfg(unix)]
pub fn socket_path() -> std::path::PathBuf {
    const SOCK_NAME: &str = "mini-term.ssh-cli.sock";
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let d = std::path::PathBuf::from(dir);
        if d.is_dir() {
            return d.join(SOCK_NAME);
        }
    }
    if let Some(cfg) = mt_core::config_json_path() {
        if let Some(parent) = cfg.parent() {
            return parent.join(SOCK_NAME);
        }
    }
    let uid = unsafe { libc_getuid() };
    std::env::temp_dir().join(format!("mini-term.ssh-cli.{uid}.sock"))
}

/// 极简 getuid FFI(避免引入 libc 依赖只为一个调用)。
#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

// ---------------------------------------------------------------------------
// 连接(客户端侧)
// ---------------------------------------------------------------------------

/// 统一的双向流 trait 别名:named pipe 与 unix socket 都满足。
pub trait IpcStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> IpcStream for T {}

/// Windows `ERROR_PIPE_BUSY`:管道的所有实例都已连上客户端,暂时没有空闲实例。
#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

/// [`connect`] 的失败分类。两类的处置截然不同,调用方必须区分。
#[derive(Debug)]
pub enum ConnectError {
    /// 端点上没有可连的服务端(不存在 / 拒绝连接 / 一直忙)。调用方可以自拉起 daemon。
    Unreachable(std::io::Error),
    /// 连上了,但服务端进程不属于当前用户 —— 或者核验链本身断了,同样按不可信处理。
    /// 绝不能往这条连接上发请求;自拉起也没用(端点被占着,新 daemon 绑不上)。
    Untrusted(String),
}

impl ConnectError {
    /// 所有管道实例都在忙(仅 Windows)。端点确实被某个服务端持有 —— 高并发瞬间,
    /// 或 daemon 正在优雅退出、已不再补监听实例 —— 此时拉起新 daemon 也绑不上。
    pub fn is_busy(&self) -> bool {
        #[cfg(windows)]
        {
            matches!(self, ConnectError::Unreachable(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY))
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Unreachable(e) => write!(f, "{e}"),
            ConnectError::Untrusted(reason) => {
                write!(f, "endpoint is not served by the current user: {reason}")
            }
        }
    }
}

/// 连接到 daemon 端点,并在收发任何数据之前核验服务端属于当前用户。
///
/// 核验不过返回 [`ConnectError::Untrusted`],连接随即丢弃 —— 请求帧里带着能力令牌,
/// 一个字节都不能发给冒充者。
pub async fn connect(endpoint: &str) -> Result<Box<dyn IpcStream>, ConnectError> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use tokio::net::windows::named_pipe::ClientOptions;
        // ClientOptions 默认带 SECURITY_IDENTIFICATION:服务端最多只能「识别」我们,
        // 冒充不了我们的令牌去提权;剩下的风险(看请求、伪造响应)靠下面的身份核验堵。
        let mut busy_retries = 0;
        let mut verify_retried = false;
        loop {
            match ClientOptions::new().open(endpoint) {
                Ok(client) => {
                    let pipe = client.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
                    match windows_security::verify_pipe_server(pipe) {
                        Ok(()) => return Ok(Box::new(client)),
                        // 核验失败先断开重来一次:daemon 恰在此刻退出(进程已没、实例刚断)
                        // 也会让核验链断掉。重连就分得清 —— 真退出了会变成「连不上」,
                        // 别的用户占着则再次核验失败。
                        Err(_) if !verify_retried => {
                            verify_retried = true;
                            drop(client);
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        Err(reason) => return Err(ConnectError::Untrusted(reason)),
                    }
                }
                // 所有实例都在忙 —— daemon 每 accept 一个连接就立即补一个新实例,忙窗口
                // 极短,小退避重试几次即可。
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && busy_retries < 4 => {
                    busy_retries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => return Err(ConnectError::Unreachable(e)),
            }
        }
    }
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(endpoint)
            .await
            .map_err(ConnectError::Unreachable)?;
        verify_socket_peer(&stream).map_err(ConnectError::Untrusted)?;
        Ok(Box::new(stream))
    }
}

/// 服务端身份核验的判定规则(纯函数,两平台共用)。
///
/// 核验链断在哪一步都 fail closed:取不到自己的身份、取不到对端的身份、两边不同,
/// 一律判不可信;只有两边都取到且相同才放行。身份的相等语义由 `T` 给出
/// (Windows 是 `EqualSid`,Unix 是 uid 相等)。
pub(crate) fn judge_peer_identity<T: PartialEq>(
    own: Result<T, String>,
    peer: Result<T, String>,
) -> Result<(), String> {
    let own = own.map_err(|e| format!("cannot determine the current user: {e}"))?;
    let peer = peer.map_err(|e| format!("cannot verify the process serving the endpoint: {e}"))?;
    if own == peer {
        Ok(())
    } else {
        Err("the process serving the endpoint belongs to another user".to_string())
    }
}

/// Unix:对端 socket 的 uid 必须是当前用户。
///
/// `peer_cred` 是内核给的(Linux `SO_PEERCRED` / macOS `getpeereid`),记录的是服务端
/// `listen` 时的凭据,对端伪造不了。socket 回退路径落在 /tmp 时别的用户能抢先放一个
/// 同名 socket,这里就是那道闸。
#[cfg(unix)]
pub(crate) fn verify_socket_peer(stream: &tokio::net::UnixStream) -> Result<(), String> {
    // SAFETY: getuid 没有参数、不会失败。
    let own: Result<u32, String> = Ok(unsafe { libc_getuid() });
    let peer = stream
        .peer_cred()
        .map(|cred| cred.uid())
        .map_err(|e| format!("peer credentials unavailable: {e}"));
    judge_peer_identity(own, peer)
}

// ---------------------------------------------------------------------------
// Windows:pipe 安全描述符(仅当前用户可连)
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub mod windows_security {
    //! Windows 侧的身份与管道安全:
    //! - 用 SDDL 构造「仅当前用户 GENERIC_ALL」的 security descriptor,供 named pipe
    //!   server 端限制连接方(spec §2 安全边界);
    //! - 客户端反向核验管道服务端进程的用户 SID([`verify_pipe_server`]),防别的用户
    //!   抢先建同名管道冒充 daemon。

    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    use windows_sys::Win32::Foundation::{LocalFree, HANDLE, PSID};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        CopySid, EqualSid, GetLengthSid, GetTokenInformation, IsValidSid, TokenUser, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// 自持缓冲的 SID 副本。
    ///
    /// SID 末尾是 `u32` 子权限数组,按 `u32` 缓冲保存才保证交给 Win32 时对齐;
    /// 相等比较走 `EqualSid`。
    pub struct OwnedSid {
        words: Vec<u32>,
    }

    impl OwnedSid {
        /// 从 Win32 给出的 SID 指针拷一份。
        ///
        /// # Safety
        /// `sid` 必须指向一个在本调用期间有效的 SID。
        unsafe fn copy_from(sid: PSID) -> Result<Self, String> {
            if IsValidSid(sid) == 0 {
                return Err("invalid SID".into());
            }
            let len = GetLengthSid(sid);
            let mut words = vec![0u32; (len as usize).div_ceil(4)];
            if CopySid(len, words.as_mut_ptr().cast(), sid) == 0 {
                return Err(format!(
                    "CopySid failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self { words })
        }

        /// 从字符串形式(`S-1-5-18` 等)构造,测试里借它造一个「别的用户」。
        #[cfg(test)]
        pub(crate) fn from_string_sid(text: &str) -> Result<Self, String> {
            use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;

            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let mut sid: PSID = std::ptr::null_mut();
            // SAFETY: wide 以 NUL 结尾;成功时 sid 由系统 LocalAlloc,拷完即释放。
            unsafe {
                if ConvertStringSidToSidW(wide.as_ptr(), &mut sid) == 0 {
                    return Err(format!(
                        "ConvertStringSidToSidW failed: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                let owned = Self::copy_from(sid);
                LocalFree(sid);
                owned
            }
        }

        fn as_psid(&self) -> PSID {
            self.words.as_ptr() as PSID
        }

        /// 字符串形式(如 `S-1-5-21-...`)。
        pub fn to_string_sid(&self) -> Result<String, String> {
            let mut pstr: *mut u16 = std::ptr::null_mut();
            // SAFETY: as_psid 指向有效 SID;成功时 pstr 由系统 LocalAlloc、以 NUL 结尾,
            // 读完即 LocalFree。
            unsafe {
                if ConvertSidToStringSidW(self.as_psid(), &mut pstr) == 0 {
                    return Err(format!(
                        "ConvertSidToStringSidW failed: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                let mut len = 0usize;
                while *pstr.add(len) != 0 {
                    len += 1;
                }
                let sid = String::from_utf16_lossy(std::slice::from_raw_parts(pstr, len));
                LocalFree(pstr as *mut c_void);
                Ok(sid)
            }
        }
    }

    impl PartialEq for OwnedSid {
        fn eq(&self, other: &Self) -> bool {
            // SAFETY: 两边都是 copy_from 校验过的有效 SID。
            unsafe { EqualSid(self.as_psid(), other.as_psid()) != 0 }
        }
    }

    impl std::fmt::Debug for OwnedSid {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.to_string_sid() {
                Ok(sid) => f.write_str(&sid),
                Err(e) => write!(f, "<unprintable SID: {e}>"),
            }
        }
    }

    /// 读 token 的用户 SID(`TokenUser`)。
    fn token_user_sid(token: &OwnedHandle) -> Result<OwnedSid, String> {
        let raw = token.as_raw_handle() as HANDLE;
        let mut needed: u32 = 0;
        // SAFETY: 探长度调用 —— 缓冲为 NULL、长度 0,预期失败并写回所需字节数。
        unsafe { GetTokenInformation(raw, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(format!(
                "GetTokenInformation size probe failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // TOKEN_USER 里有指针,用 u64 缓冲保证对齐。
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: buf 至少 needed 字节且 8 字节对齐。
        let ok = unsafe {
            GetTokenInformation(raw, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed)
        };
        if ok == 0 {
            return Err(format!(
                "GetTokenInformation failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: 成功后缓冲开头是 TOKEN_USER,User.Sid 指向缓冲内部(buf 此刻仍存活)。
        unsafe {
            let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
            OwnedSid::copy_from(token_user.User.Sid)
        }
    }

    /// 当前进程 token 的用户 SID。
    pub fn current_user_sid() -> Result<OwnedSid, String> {
        // windows-sys 0.52:HANDLE = isize,0 即 NULL。
        let mut token: HANDLE = 0;
        // SAFETY: GetCurrentProcess 是无需关闭的伪句柄。
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(format!(
                "OpenProcessToken failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: token 是刚打开、无人共享的有效句柄,交给 OwnedHandle 负责关闭。
        let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };
        token_user_sid(&token)
    }

    /// 取当前进程 token 的用户 SID 字符串(如 `S-1-5-21-...`)。
    pub fn current_user_sid_string() -> Result<String, String> {
        current_user_sid()?.to_string_sid()
    }

    /// 指定进程 token 的用户 SID。
    ///
    /// 只要 `PROCESS_QUERY_LIMITED_INFORMATION` + `TOKEN_QUERY` 两项最小权限。别的用户的
    /// 进程通常两步都过不了(进程 / token 的默认 DACL 不给外人),过不了就是核验失败,
    /// 调用方按不可信处理 —— 宁可降级,不可误信。
    pub fn process_user_sid(pid: u32) -> Result<OwnedSid, String> {
        // SAFETY: 失败返回 NULL,不涉及调用方内存。
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process == 0 {
            return Err(format!(
                "OpenProcess(pid {pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: process 是刚打开的有效句柄,交给 OwnedHandle 负责关闭。
        let process = unsafe { OwnedHandle::from_raw_handle(process as RawHandle) };
        let mut token: HANDLE = 0;
        // SAFETY: process 在本作用域内有效。
        let opened =
            unsafe { OpenProcessToken(process.as_raw_handle() as HANDLE, TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(format!(
                "OpenProcessToken(pid {pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: 同上,token 交给 OwnedHandle 关闭。
        let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };
        token_user_sid(&token)
    }

    /// 管道服务端(创建这个管道实例的)进程的 PID。在客户端句柄上调用。
    pub fn pipe_server_process_id(pipe: HANDLE) -> Result<u32, String> {
        let mut pid: u32 = 0;
        // SAFETY: pipe 是调用方持有的有效管道句柄。
        if unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 {
            return Err(format!(
                "GetNamedPipeServerProcessId failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(pid)
    }

    /// 客户端核验:管道服务端进程的用户 SID 必须与当前进程的相同。
    ///
    /// 链路 `GetNamedPipeServerProcessId` → `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
    /// → `OpenProcessToken(TOKEN_QUERY)` → `GetTokenInformation(TokenUser)` → `EqualSid`,
    /// 任何一步失败都按不可信处理(判定规则见 [`super::judge_peer_identity`])。
    ///
    /// 已知会被误判成不可信的情形:同一用户以管理员身份(提权)跑着 daemon、本 CLI 未提权
    /// —— 未提权进程打不开提权进程的 token。代价只是这次降级 one-shot,可以接受。
    pub fn verify_pipe_server(pipe: HANDLE) -> Result<(), String> {
        verify_pipe_server_as(pipe, current_user_sid())
    }

    /// [`verify_pipe_server`] 的可注入版本:`own` 由调用方给(测试借此走通「SID 不同」分支)。
    pub(crate) fn verify_pipe_server_as(
        pipe: HANDLE,
        own: Result<OwnedSid, String>,
    ) -> Result<(), String> {
        let peer = pipe_server_process_id(pipe).and_then(process_user_sid);
        super::judge_peer_identity(own, peer)
    }

    /// 持有「仅当前用户可连」的 SECURITY_ATTRIBUTES,生命周期内指针有效。
    ///
    /// SD 由 SDDL `D:P(A;;GA;;;<sid>)` 转换而来:DACL protected(不继承)、
    /// 仅当前用户 GENERIC_ALL,其它一律拒绝(默认 deny)。
    pub struct PipeSecurity {
        security_descriptor: *mut c_void,
        attrs: Box<SecurityAttributes>,
    }

    /// 与 Win32 SECURITY_ATTRIBUTES 布局一致(避免引 feature 只为这个结构)。
    #[repr(C)]
    struct SecurityAttributes {
        n_length: u32,
        lp_security_descriptor: *mut c_void,
        b_inherit_handle: i32,
    }

    // SD 创建后只读,跨线程移动安全(named pipe server future 需要 Send)。
    unsafe impl Send for PipeSecurity {}

    impl PipeSecurity {
        /// 构造仅当前用户可访问的 pipe 安全属性。
        pub fn current_user_only() -> Result<Self, String> {
            let sid = current_user_sid_string()?;
            let sddl = format!("D:P(A;;GA;;;{sid})");
            let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut psd: *mut c_void = std::ptr::null_mut();
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut psd as *mut *mut c_void as *mut _,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || psd.is_null() {
                return Err(format!(
                    "cannot build security descriptor from SDDL '{sddl}'"
                ));
            }
            let attrs = Box::new(SecurityAttributes {
                n_length: std::mem::size_of::<SecurityAttributes>() as u32,
                lp_security_descriptor: psd,
                b_inherit_handle: 0,
            });
            Ok(Self {
                security_descriptor: psd,
                attrs,
            })
        }

        /// 取 SECURITY_ATTRIBUTES 裸指针,供
        /// `ServerOptions::create_with_security_attributes_raw` 使用。
        /// 指针在 `self` 存活期间有效。
        pub fn attributes_ptr(&self) -> *mut c_void {
            &*self.attrs as *const SecurityAttributes as *mut c_void
        }
    }

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.security_descriptor);
            }
        }
    }
}

// ============================================================================
// tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- 请求帧 round-trip 与 camelCase 键名 ---

    #[test]
    fn request_frame_round_trip_all_fields() {
        let req = Request {
            v: PROTOCOL_VERSION,
            op: Some(Op::Exec),
            project_token: Some("token-1".into()),
            connection: Some("prod".into()),
            command: Some("ls -la".into()),
            cwd: Some("/var/log".into()),
            timeout_secs: Some(30),
            local_path: None,
            remote_path: None,
        };
        let line = encode_frame(&req).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "必须恰好一行");
        // camelCase 键名(与 spec §2 协议示例一致)
        assert!(line.contains("\"projectToken\""), "got: {line}");
        assert!(line.contains("\"timeoutSecs\""), "got: {line}");
        assert!(line.contains("\"op\":\"exec\""), "got: {line}");
        let back: Request = decode_frame(&line).unwrap();
        assert_eq!(back.v, PROTOCOL_VERSION);
        assert_eq!(back.op, Some(Op::Exec));
        assert_eq!(back.project_token.as_deref(), Some("token-1"));
        assert_eq!(back.command.as_deref(), Some("ls -la"));
    }

    #[test]
    fn request_frame_omits_absent_fields() {
        let req = Request {
            v: 1,
            op: Some(Op::Status),
            ..Default::default()
        };
        let line = encode_frame(&req).unwrap();
        assert!(!line.contains("projectToken"));
        assert!(!line.contains("localPath"));
    }

    #[test]
    fn request_frame_multiline_command_stays_one_line() {
        // 命令含换行时,serde_json 会转义成 \n —— 帧仍是单行,协议不破。
        let req = Request {
            v: 1,
            op: Some(Op::Exec),
            command: Some("echo a\necho b".into()),
            ..Default::default()
        };
        let line = encode_frame(&req).unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        let back: Request = decode_frame(&line).unwrap();
        assert_eq!(back.command.as_deref(), Some("echo a\necho b"));
    }

    #[test]
    fn request_unknown_op_decodes_as_error() {
        // 新版 CLI 对旧 daemon 发未知 op → 旧 daemon 解码失败,回 error 帧,
        // 而不是 panic。这里验证解码确实报错。
        let r: Result<Request, _> = decode_frame(r#"{"v":1,"op":"teleport"}"#);
        assert!(r.is_err());
    }

    // --- 响应帧 round-trip ---

    #[test]
    fn hello_frame_shape() {
        let f = ServerFrame::Hello {
            version: "0.8.5".into(),
            protocol_version: Some(PROTOCOL_VERSION),
            pid: 4242,
        };
        let line = encode_frame(&f).unwrap();
        assert!(line.contains("\"type\":\"hello\""), "got: {line}");
        assert!(line.contains("\"version\":\"0.8.5\""));
        assert!(line.contains("\"protocolVersion\":2"));
        assert!(line.contains("\"pid\":4242"));
        let back: ServerFrame = decode_frame(&line).unwrap();
        match back {
            ServerFrame::Hello {
                version,
                protocol_version,
                pid,
            } => {
                assert_eq!(version, "0.8.5");
                assert_eq!(protocol_version, Some(PROTOCOL_VERSION));
                assert_eq!(pid, 4242);
            }
            other => panic!("expected hello, got {other:?}"),
        }

        // v1 hello 没有 protocolVersion，新 CLI 必须仍能解码并识别为 legacy。
        let legacy: ServerFrame =
            decode_frame(r#"{"type":"hello","version":"0.4.8","pid":7}"#).unwrap();
        assert!(matches!(
            legacy,
            ServerFrame::Hello {
                protocol_version: None,
                ..
            }
        ));
    }

    #[test]
    fn stdout_chunk_b64_round_trip_binary_safe() {
        // 二进制含 \0 / 非 UTF-8 / 换行,经 base64 分片后逐字节还原。
        let payload: Vec<u8> = vec![0x00, 0xFF, 0xFE, b'\n', b'\r', 0x80, b'a'];
        let f = ServerFrame::Stdout {
            data_b64: b64_encode(&payload),
        };
        let line = encode_frame(&f).unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.contains("\"type\":\"stdout\""));
        assert!(line.contains("\"dataB64\""), "got: {line}");
        let back: ServerFrame = decode_frame(&line).unwrap();
        match back {
            ServerFrame::Stdout { data_b64 } => {
                assert_eq!(b64_decode(&data_b64).unwrap(), payload);
            }
            other => panic!("expected stdout, got {other:?}"),
        }
    }

    #[test]
    fn result_frame_exec_shape() {
        let f = ServerFrame::Result {
            exit_code: Some(17),
            timed_out: Some(false),
            not_started: None,
            bytes: None,
            connections: None,
            sessions: None,
            active_requests: None,
        };
        let line = encode_frame(&f).unwrap();
        assert!(line.contains("\"type\":\"result\""));
        assert!(line.contains("\"exitCode\":17"), "got: {line}");
        assert!(line.contains("\"timedOut\":false"));
        assert!(!line.contains("bytes"));
        assert!(!line.contains("connections"));
        assert!(!line.contains("activeRequests"));
        assert!(!line.contains("notStarted"));
    }

    #[test]
    fn exec_result_flags_not_started_timeouts_without_changing_normal_frames() {
        // 普通结果与运行中超时:线上形状与 0.4.10 一字不差(不带 notStarted)。
        let normal = encode_frame(&ServerFrame::result_exec(Some(0), false, false)).unwrap();
        assert!(!normal.contains("notStarted"), "got: {normal}");
        let running = encode_frame(&ServerFrame::result_exec(None, true, false)).unwrap();
        assert!(!running.contains("notStarted"), "got: {running}");

        // 连接阶段超时:带上 notStarted,新 CLI 据此改报「连接阶段超时」。
        let line = encode_frame(&ServerFrame::result_exec(None, true, true)).unwrap();
        assert!(line.contains("\"notStarted\":true"), "got: {line}");
        match decode_frame::<ServerFrame>(&line).unwrap() {
            ServerFrame::Result {
                timed_out,
                not_started,
                exit_code,
                ..
            } => {
                assert_eq!(timed_out, Some(true));
                assert_eq!(not_started, Some(true));
                assert_eq!(exit_code, None);
            }
            other => panic!("expected result, got {other:?}"),
        }

        // 旧 daemon 的超时帧没有这个字段 → None,新 CLI 按普通超时处理。
        let old: ServerFrame = decode_frame(r#"{"type":"result","timedOut":true}"#).unwrap();
        assert!(matches!(
            old,
            ServerFrame::Result {
                timed_out: Some(true),
                not_started: None,
                ..
            }
        ));
    }

    #[test]
    fn status_result_carries_active_requests_and_stays_compatible() {
        let line = encode_frame(&ServerFrame::result_status(3, 2)).unwrap();
        assert!(line.contains("\"sessions\":3"), "got: {line}");
        assert!(line.contains("\"activeRequests\":2"), "got: {line}");
        match decode_frame::<ServerFrame>(&line).unwrap() {
            ServerFrame::Result {
                sessions,
                active_requests,
                ..
            } => {
                assert_eq!(sessions, Some(3));
                assert_eq!(active_requests, Some(2));
            }
            other => panic!("expected result, got {other:?}"),
        }

        // 0.4.10 daemon 的 status 回复没有 activeRequests → None。
        let old: ServerFrame = decode_frame(r#"{"type":"result","sessions":1}"#).unwrap();
        assert!(matches!(
            old,
            ServerFrame::Result {
                sessions: Some(1),
                active_requests: None,
                ..
            }
        ));
        // 反过来,旧 CLI 解码新字段靠的是「不认识的字段一律忽略」—— 这里用一个
        // 本版也不认识的字段钉住这条 serde 行为。
        let future: ServerFrame =
            decode_frame(r#"{"type":"result","sessions":1,"someFutureField":7}"#).unwrap();
        assert!(matches!(
            future,
            ServerFrame::Result {
                sessions: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn result_frame_list_carries_connections_without_secrets() {
        let f = ServerFrame::Result {
            exit_code: None,
            timed_out: None,
            not_started: None,
            bytes: None,
            connections: Some(vec![SshConnectionView {
                id: "id-1".into(),
                name: "prod".into(),
                host: "10.0.0.5".into(),
                port: 22,
                user: "root".into(),
                group: None,
            }]),
            sessions: None,
            active_requests: None,
        };
        let line = encode_frame(&f).unwrap();
        assert!(line.contains("\"connections\""));
        assert!(!line.to_lowercase().contains("password"));
        let back: ServerFrame = decode_frame(&line).unwrap();
        match back {
            ServerFrame::Result { connections, .. } => {
                let conns = connections.unwrap();
                assert_eq!(conns.len(), 1);
                assert_eq!(conns[0].name, "prod");
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn error_frame_round_trip() {
        let f = ServerFrame::Error {
            message: "No SSH connection found matching 'x'".into(),
        };
        let line = encode_frame(&f).unwrap();
        assert!(line.contains("\"type\":\"error\""));
        let back: ServerFrame = decode_frame(&line).unwrap();
        match back {
            ServerFrame::Error { message } => assert!(message.contains("No SSH connection")),
            other => panic!("expected error, got {other:?}"),
        }
    }

    // --- base64 编解码 ---

    #[test]
    fn b64_round_trip_empty_and_large() {
        assert_eq!(b64_decode(&b64_encode(&[])).unwrap(), Vec::<u8>::new());
        let big: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
        assert_eq!(b64_decode(&b64_encode(&big)).unwrap(), big);
    }

    #[test]
    fn b64_decode_rejects_garbage() {
        assert!(b64_decode("!!!not-base64!!!").is_err());
    }

    // --- 端点名 ---

    #[test]
    fn sanitize_tag_keeps_sid_chars_and_replaces_others() {
        assert_eq!(sanitize_tag("S-1-5-21-1111"), "S-1-5-21-1111");
        assert_eq!(sanitize_tag("user name@host"), "user-name-host");
        assert_eq!(sanitize_tag(""), "default");
    }

    #[cfg(windows)]
    #[test]
    fn default_endpoint_is_per_user_pipe() {
        let ep = default_endpoint();
        assert!(ep.starts_with(r"\\.\pipe\mini-term.ssh-cli."), "got: {ep}");
        // 末段只含清洗后的安全字符
        let tag = ep.rsplit('.').next().unwrap();
        assert!(!tag.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn current_user_sid_resolves() {
        let sid = windows_security::current_user_sid_string().unwrap();
        assert!(sid.starts_with("S-1-"), "got: {sid}");
    }

    #[cfg(windows)]
    #[test]
    fn pipe_security_builds() {
        let sec = windows_security::PipeSecurity::current_user_only().unwrap();
        assert!(!sec.attributes_ptr().is_null());
    }

    // --- 服务端身份核验 ---

    #[test]
    fn judge_peer_identity_fails_closed() {
        assert!(judge_peer_identity::<u32>(Ok(1000), Ok(1000)).is_ok());

        let other = judge_peer_identity::<u32>(Ok(1000), Ok(0)).unwrap_err();
        assert!(other.contains("another user"), "got: {other}");

        // 核验链断在哪一步都不放行。
        let peer_unknown =
            judge_peer_identity::<u32>(Ok(1000), Err("OpenProcess failed".into())).unwrap_err();
        assert!(
            peer_unknown.contains("cannot verify") && peer_unknown.contains("OpenProcess failed"),
            "got: {peer_unknown}"
        );
        let own_unknown = judge_peer_identity::<u32>(Err("no token".into()), Ok(1000)).unwrap_err();
        assert!(
            own_unknown.contains("current user") && own_unknown.contains("no token"),
            "got: {own_unknown}"
        );
    }

    #[test]
    fn connect_error_kinds_render_and_classify() {
        let untrusted = ConnectError::Untrusted("belongs to another user".into());
        assert!(!untrusted.is_busy());
        assert!(untrusted
            .to_string()
            .contains("not served by the current user"));

        let missing = ConnectError::Unreachable(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(!missing.is_busy());

        #[cfg(windows)]
        {
            let busy =
                ConnectError::Unreachable(std::io::Error::from_raw_os_error(ERROR_PIPE_BUSY));
            assert!(busy.is_busy());
        }
    }

    #[cfg(windows)]
    fn unique_pipe(label: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!(
            r"\\.\pipe\mt-ssh-cli-ipc-test-{label}-{}-{nanos}",
            std::process::id()
        )
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_served_by_same_user_is_trusted() {
        use tokio::net::windows::named_pipe::ServerOptions;

        let ep = unique_pipe("trusted");
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&ep)
            .unwrap();
        let accept = tokio::spawn(async move {
            server.connect().await.unwrap();
            server
        });
        let client = connect(&ep).await;
        assert!(
            client.is_ok(),
            "same-process server must verify: {:?}",
            client.err()
        );
        drop(accept.await.unwrap());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_server_identity_mismatch_is_rejected() {
        use std::os::windows::io::AsRawHandle;
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

        let ep = unique_pipe("mismatch");
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&ep)
            .unwrap();
        let accept = tokio::spawn(async move {
            server.connect().await.unwrap();
            server
        });
        let client = ClientOptions::new().open(&ep).unwrap();
        let pipe = client.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;

        // 真实服务端就是本进程:按本进程身份核验必然通过……
        assert!(windows_security::verify_pipe_server(pipe).is_ok());
        // ……换成「我是 LocalSystem」再核一次,完整走过 PID → 进程 → token → EqualSid
        // 整条链后必须判不可信 —— 冒充者的进程属于别的用户时就是这条路径。
        let system = windows_security::OwnedSid::from_string_sid("S-1-5-18").unwrap();
        let err = windows_security::verify_pipe_server_as(pipe, Ok(system)).unwrap_err();
        assert!(err.contains("another user"), "got: {err}");

        drop(client);
        drop(accept.await.unwrap());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn missing_pipe_is_unreachable_not_untrusted() {
        match connect(&unique_pipe("missing")).await {
            Err(ConnectError::Unreachable(_)) => {}
            Err(other) => panic!("expected Unreachable, got {other:?}"),
            Ok(_) => panic!("nothing listens on this pipe"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn current_user_sid_equals_itself_and_differs_from_system() {
        let me = windows_security::current_user_sid().unwrap();
        let me_again = windows_security::current_user_sid().unwrap();
        assert_eq!(me, me_again);
        assert_eq!(
            me.to_string_sid().unwrap(),
            windows_security::current_user_sid_string().unwrap()
        );
        let system = windows_security::OwnedSid::from_string_sid("S-1-5-18").unwrap();
        assert_ne!(me, system);
        assert_eq!(
            windows_security::process_user_sid(std::process::id()).unwrap(),
            me
        );
    }

    // ⚠️ 下面这条 cfg(unix) 用例本机(Windows)编不到,由 CI 的 Ubuntu 作业编译运行。
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_served_by_same_user_is_trusted() {
        let path = std::env::temp_dir().join(format!(
            "mt-ssh-cli-ipc-test-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.map(|(s, _)| s) });
        let client = connect(&path.to_string_lossy()).await;
        assert!(
            client.is_ok(),
            "same-user socket must verify: {:?}",
            client.err()
        );
        drop(accept.await.unwrap());
        let _ = std::fs::remove_file(&path);
    }
}
