//! ipc —— mt-ssh-cli 与 daemon 之间的本机 IPC 传输与帧协议。
//!
//! 传输(spec §2):
//! - Windows:named pipe `\\.\pipe\mini-term.ssh-cli.<用户标识>`,pipe 的
//!   security descriptor 限**仅当前用户**可连;
//! - macOS/Linux:Unix domain socket(`$XDG_RUNTIME_DIR` 优先,回退 config.json
//!   同目录),权限 0600。
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
        /// upload/download:传输字节数。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        /// list:连接视图(不含敏感字段)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connections: Option<Vec<SshConnectionView>>,
        /// status:池内 session 数。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sessions: Option<usize>,
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
            bytes: None,
            connections: None,
            sessions: None,
        }
    }

    /// exec 的 result 终帧。
    pub fn result_exec(exit_code: Option<i32>, timed_out: bool) -> Self {
        ServerFrame::Result {
            exit_code,
            timed_out: Some(timed_out),
            bytes: None,
            connections: None,
            sessions: None,
        }
    }

    /// upload/download 的 result 终帧。
    pub fn result_bytes(bytes: u64) -> Self {
        ServerFrame::Result {
            exit_code: None,
            timed_out: None,
            bytes: Some(bytes),
            connections: None,
            sessions: None,
        }
    }

    /// list 的 result 终帧。
    pub fn result_connections(connections: Vec<SshConnectionView>) -> Self {
        ServerFrame::Result {
            exit_code: None,
            timed_out: None,
            bytes: None,
            connections: Some(connections),
            sessions: None,
        }
    }

    /// status 的 result 终帧。
    pub fn result_sessions(sessions: usize) -> Self {
        ServerFrame::Result {
            exit_code: None,
            timed_out: None,
            bytes: None,
            connections: None,
            sessions: Some(sessions),
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

/// 连接到 daemon 端点。失败返回 io::Error(caller 据此走自拉起/降级)。
pub async fn connect(endpoint: &str) -> std::io::Result<Box<dyn IpcStream>> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        // ERROR_PIPE_BUSY(231):所有实例都在忙 —— daemon 每 accept 一个连接就
        // 立即补一个新实例,忙窗口极短,小退避重试几次即可。
        const ERROR_PIPE_BUSY: i32 = 231;
        let mut last_err = None;
        for _ in 0..5 {
            match ClientOptions::new().open(endpoint) {
                Ok(client) => return Ok(Box::new(client)),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| std::io::Error::other("pipe busy")))
    }
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(endpoint).await?;
        Ok(Box::new(stream))
    }
}

// ---------------------------------------------------------------------------
// Windows:pipe 安全描述符(仅当前用户可连)
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub mod windows_security {
    //! 用 SDDL 构造「仅当前用户 GENERIC_ALL」的 security descriptor,
    //! 供 named pipe server 端限制连接方(spec §2 安全边界)。

    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// 取当前进程 token 的用户 SID 字符串(如 `S-1-5-21-...`)。
    pub fn current_user_sid_string() -> Result<String, String> {
        unsafe {
            // windows-sys 0.52:HANDLE = isize,0 即 NULL。
            let mut token: HANDLE = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err("OpenProcessToken failed".into());
            }
            // 先探长度,再取 TOKEN_USER。
            let mut needed: u32 = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                CloseHandle(token);
                return Err("GetTokenInformation size probe failed".into());
            }
            let mut buf = vec![0u8; needed as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            );
            CloseHandle(token);
            if ok == 0 {
                return Err("GetTokenInformation failed".into());
            }
            let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut pstr: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(token_user.User.Sid, &mut pstr) == 0 {
                return Err("ConvertSidToStringSidW failed".into());
            }
            // 读出宽字符串再 LocalFree。
            let mut len = 0usize;
            while *pstr.add(len) != 0 {
                len += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(pstr, len));
            LocalFree(pstr as *mut c_void);
            Ok(sid)
        }
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
            bytes: None,
            connections: None,
            sessions: None,
        };
        let line = encode_frame(&f).unwrap();
        assert!(line.contains("\"type\":\"result\""));
        assert!(line.contains("\"exitCode\":17"), "got: {line}");
        assert!(line.contains("\"timedOut\":false"));
        assert!(!line.contains("bytes"));
        assert!(!line.contains("connections"));
    }

    #[test]
    fn result_frame_list_carries_connections_without_secrets() {
        let f = ServerFrame::Result {
            exit_code: None,
            timed_out: None,
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
}
