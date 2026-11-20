//! 抄送通道的服务端:本机 TCP 监听 + NDJSON 帧解码 + nonce 认 pane。
//!
//! 线协议的权威口径在 `docs/specs/ssh-activity-overlay-v1.md` 的「通道与线协议」一节,
//! 这里只记实现上的取舍:
//!
//! - **一次调用一条连接**:第一帧必须是 `start`,且要在连上之后 [`START_DEADLINE`] 之内到;
//!   nonce 不认识 / 首帧不是 `start` / 解析失败 / 版本号不认识 → 直接断开,**不产生任何记录**。
//!   `start` 认过之后再出错(超长行、帧解析失败、`end` 之前断开)一律按「已中断」收尾。
//! - **2s 是整段期限**,不是单次 `read` 的超时:慢吞吞一个字节一个字节地喂也只能占满 2s
//!   ([`DeadlineStream`] 每次读之前按剩余时间重设 `set_read_timeout`)。认过 `start` 之后
//!   不再设读超时 —— `exec` 可以合法地沉默很久(`sleep 600`),CLI 被杀时系统会替它关连接。
//! - **单行 256 KiB 封顶**([`MAX_LINE_BYTES`]),超了断开;同时在跑的连接 64 条封顶
//!   ([`MAX_CONNECTIONS`]),超出的连接接下来就关。
//! - **镜像输出在这一层先截一刀**([`super::model::RECORD_OUTPUT_CAP`]):超过 1 MiB 的部分
//!   不再往 UI 线程送,只在收尾事件上带一位 `dropped`。模型那边还会再兜一次。
//! - 每个连接一条读线程(阻塞读,不碰 UI 线程);事件经调用方给的 `sink` 交出去
//!   (主程序里是 `futures::channel::mpsc`,见 [`super::start`])。
//!
//! 日志纪律:这里的 `eprintln!` 只写断开原因的**类别**,绝不写命令原文、输出或 nonce。

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::Mutex;
use ring::rand::{SecureRandom as _, SystemRandom};
use serde_json::{Map, Value};

use super::model::RECORD_OUTPUT_CAP;

/// 注入给本地 pane 的环境变量名,值是 `127.0.0.1:<port>/<nonce>`。
///
/// 名字刻意避开 `KEY` / `SECRET` / `TOKEN`:Codex 的 shell 环境策略可能按名字过滤这类变量。
/// `MINITERM_` 是保留前缀,用户 / 项目环境变量覆盖不了(见 `mt_pty::PtyOptions::user_env`)。
pub const FEED_ENV: &str = "MINITERM_SSH_FEED";

/// `start` 帧里 `v` 的当前值。别的值一律断开。
pub const PROTOCOL_VERSION: u64 = 1;

/// 单行(一帧)上限。`out` 帧单帧原始数据 ≤ 32 KiB,base64 之后约 43 KiB,留足余量。
pub const MAX_LINE_BYTES: usize = 256 * 1024;

/// 同时在跑的连接上限。一次 CLI 调用一条连接,AI 并发调用远到不了这个数。
pub const MAX_CONNECTIONS: usize = 64;

/// 连上之后多久之内必须收到 `start`。
pub const START_DEADLINE: Duration = Duration::from_secs(2);

/// CLI 的业务调用种类。`list` 不碰服务器,CLI 根本不报。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Exec,
    Upload,
    Download,
}

/// `out` 帧的流。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    /// 两路解码器在数组里的下标。
    pub fn index(self) -> usize {
        match self {
            Stream::Stdout => 0,
            Stream::Stderr => 1,
        }
    }
}

/// `start` 帧里与界面有关的字段(`nonce` / `v` 只用来认证,不往下传)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartInfo {
    pub op: Op,
    /// AI 传的连接名或 id,原样。
    pub connection: String,
    /// `exec`:join 之后的原始远程命令(不脱敏)。
    pub command: Option<String>,
    /// `exec` 的 `--cwd` 原值。
    pub cwd: Option<String>,
    /// AI 显式传了 `--timeout` 才有。
    pub timeout_secs: Option<u64>,
    /// `upload` / `download`:绝对化之后的本地路径。
    pub local_path: Option<String>,
    /// `upload` / `download`:远端路径原值。
    pub remote_path: Option<String>,
}

/// `end` 帧。字段语义见规格;缺省的布尔一律当 `false`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndInfo {
    pub exit_code: Option<i64>,
    pub timed_out: bool,
    pub not_started: bool,
    pub bytes: Option<u64>,
    pub error: Option<String>,
    pub output_bytes: Option<u64>,
    pub truncated: bool,
}

/// 交给 UI 线程的事件。`call` 是本进程内每条连接一个的编号,同一次调用的事件共用它。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedEvent {
    Start {
        pty_id: u32,
        call: u64,
        info: StartInfo,
    },
    Output {
        pty_id: u32,
        call: u64,
        stream: Stream,
        data: Vec<u8>,
    },
    /// 收到 `end`。`dropped` = 这一层因为 1 MiB 上限丢过输出。
    End {
        pty_id: u32,
        call: u64,
        end: EndInfo,
        dropped: bool,
    },
    /// `start` 认过之后、`end` 之前连接没了(CLI 被杀 / Bash 工具超时 / 帧非法)。
    Interrupted {
        pty_id: u32,
        call: u64,
        dropped: bool,
    },
}

impl FeedEvent {
    pub fn pty_id(&self) -> u32 {
        match self {
            FeedEvent::Start { pty_id, .. }
            | FeedEvent::Output { pty_id, .. }
            | FeedEvent::End { pty_id, .. }
            | FeedEvent::Interrupted { pty_id, .. } => *pty_id,
        }
    }
}

// ---------------------------------------------------------------------------
// 帧解码
// ---------------------------------------------------------------------------

/// 一帧解码之后的形态。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    Start { nonce: String, info: StartInfo },
    Out { stream: Stream, data: Vec<u8> },
    End(EndInfo),
}

/// 帧解码失败的类别(只用于断开时的日志类别与单测)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// 不是合法 JSON。
    Json,
    /// JSON 对,但形状不对(不是对象 / `type` 不认识 / 缺必填字段 / 字段类型不对)。
    Shape,
    /// `start` 的 `v` 不认识。
    Version,
    /// `out` 的 `dataB64` 解不开。
    Base64,
}

/// 解一行 NDJSON(已去掉行尾换行)。
pub(crate) fn parse_frame(line: &[u8]) -> Result<Frame, FrameError> {
    let value: Value = serde_json::from_slice(line).map_err(|_| FrameError::Json)?;
    let obj = value.as_object().ok_or(FrameError::Shape)?;
    match obj.get("type").and_then(Value::as_str) {
        Some("start") => parse_start(obj),
        Some("out") => parse_out(obj),
        Some("end") => Ok(Frame::End(parse_end(obj))),
        _ => Err(FrameError::Shape),
    }
}

/// 可缺省的字符串字段:缺省 / `null` / 类型不对都当没有。
fn opt_str(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

fn flag(obj: &Map<String, Value>, key: &str) -> bool {
    obj.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn parse_start(obj: &Map<String, Value>) -> Result<Frame, FrameError> {
    // 版本号先看:新版本的 start 可能改了别的字段的形状,先认版本才不会报错报偏
    match obj.get("v").and_then(Value::as_u64) {
        Some(PROTOCOL_VERSION) => {}
        Some(_) => return Err(FrameError::Version),
        None => return Err(FrameError::Shape),
    }
    let nonce = obj
        .get("nonce")
        .and_then(Value::as_str)
        .filter(|n| is_nonce(n))
        .ok_or(FrameError::Shape)?
        .to_string();
    let op = match obj.get("op").and_then(Value::as_str) {
        Some("exec") => Op::Exec,
        Some("upload") => Op::Upload,
        Some("download") => Op::Download,
        _ => return Err(FrameError::Shape),
    };
    let connection = obj
        .get("connection")
        .and_then(Value::as_str)
        .ok_or(FrameError::Shape)?
        .to_string();
    Ok(Frame::Start {
        nonce,
        info: StartInfo {
            op,
            connection,
            command: opt_str(obj, "command"),
            cwd: opt_str(obj, "cwd"),
            timeout_secs: obj.get("timeoutSecs").and_then(Value::as_u64),
            local_path: opt_str(obj, "localPath"),
            remote_path: opt_str(obj, "remotePath"),
        },
    })
}

fn parse_out(obj: &Map<String, Value>) -> Result<Frame, FrameError> {
    let stream = match obj.get("stream").and_then(Value::as_str) {
        Some("stdout") => Stream::Stdout,
        Some("stderr") => Stream::Stderr,
        _ => return Err(FrameError::Shape),
    };
    let encoded = obj
        .get("dataB64")
        .and_then(Value::as_str)
        .ok_or(FrameError::Shape)?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| FrameError::Base64)?;
    Ok(Frame::Out { stream, data })
}

fn parse_end(obj: &Map<String, Value>) -> EndInfo {
    EndInfo {
        exit_code: obj.get("exitCode").and_then(Value::as_i64),
        timed_out: flag(obj, "timedOut"),
        not_started: flag(obj, "notStarted"),
        bytes: obj.get("bytes").and_then(Value::as_u64),
        error: opt_str(obj, "error"),
        output_bytes: obj.get("outputBytes").and_then(Value::as_u64),
        truncated: flag(obj, "truncated"),
    }
}

/// nonce 的形状:32 个小写十六进制字符(128 bit)。
pub(crate) fn is_nonce(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 新起一个 128 bit 随机 nonce(小写 hex)。系统随机源出错时返回 `None` —— 调用方就不注入变量。
pub(crate) fn new_nonce() -> Option<String> {
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------------------
// 按行读(带上限)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// 读到一行(内容在调用方的缓冲里,不含 `\n`)。连接末尾没有换行收尾的半行也算一行。
    Data,
    /// 对端关了,且缓冲里什么都没有。
    Eof,
}

#[derive(Debug)]
enum ReadError {
    TooLong,
    Io(io::ErrorKind),
}

/// 读一行,超过 `cap` 字节就报 [`ReadError::TooLong`](不再往下读)。
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> Result<Line, ReadError> {
    buf.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(ReadError::Io(err.kind())),
        };
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                Line::Eof
            } else {
                Line::Data
            });
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(at) => {
                if buf.len() + at > cap {
                    return Err(ReadError::TooLong);
                }
                buf.extend_from_slice(&available[..at]);
                reader.consume(at + 1);
                return Ok(Line::Data);
            }
            None => {
                let n = available.len();
                if buf.len() + n > cap {
                    return Err(ReadError::TooLong);
                }
                buf.extend_from_slice(available);
                reader.consume(n);
            }
        }
    }
}

/// 去掉行尾的 `\r`(对端按 CRLF 分行也认)。
fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn is_timeout(kind: io::ErrorKind) -> bool {
    // Windows 上读超时报 TimedOut,Unix 上报 WouldBlock
    matches!(kind, io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

// ---------------------------------------------------------------------------
// 一条连接
// ---------------------------------------------------------------------------

/// 一条连接的结局(日志类别 + 单测)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Served {
    /// 首帧阶段就断开,没有产生任何事件。
    Rejected(&'static str),
    /// 收到 `end`。
    Completed,
    /// `start` 之后、`end` 之前断开(已发 [`FeedEvent::Interrupted`])。
    Interrupted(&'static str),
}

/// 处理一条连接的全部帧。与 socket 无关,单测直接喂内存里的字节。
///
/// - `resolve`:nonce → pty_id(只读 nonce 表);
/// - `lift_deadline`:`start` 认过之后调一次,撤掉首帧期限(真 socket 上是取消读超时);
/// - `sink`:事件出口。
pub(crate) fn serve<R: Read>(
    reader: &mut BufReader<R>,
    call: u64,
    resolve: &dyn Fn(&str) -> Option<u32>,
    lift_deadline: &mut dyn FnMut(&mut R),
    sink: &dyn Fn(FeedEvent),
) -> Served {
    let mut line = Vec::new();
    match read_line_capped(reader, &mut line, MAX_LINE_BYTES) {
        Ok(Line::Data) => {}
        Ok(Line::Eof) => return Served::Rejected("首帧前断开"),
        Err(ReadError::TooLong) => return Served::Rejected("首帧超长"),
        Err(ReadError::Io(kind)) if is_timeout(kind) => return Served::Rejected("首帧超时"),
        Err(ReadError::Io(_)) => return Served::Rejected("首帧读取失败"),
    }
    let (nonce, info) = match parse_frame(trim_cr(&line)) {
        Ok(Frame::Start { nonce, info }) => (nonce, info),
        Ok(_) => return Served::Rejected("首帧不是 start"),
        Err(FrameError::Version) => return Served::Rejected("协议版本不认识"),
        Err(_) => return Served::Rejected("首帧解析失败"),
    };
    let Some(pty_id) = resolve(&nonce) else {
        return Served::Rejected("nonce 不认识");
    };
    lift_deadline(reader.get_mut());
    sink(FeedEvent::Start { pty_id, call, info });

    let mut forwarded = 0usize;
    let mut dropped = false;
    let interrupted = |dropped: bool, why: &'static str| {
        sink(FeedEvent::Interrupted {
            pty_id,
            call,
            dropped,
        });
        Served::Interrupted(why)
    };
    loop {
        match read_line_capped(reader, &mut line, MAX_LINE_BYTES) {
            Ok(Line::Data) => {}
            Ok(Line::Eof) => return interrupted(dropped, "end 之前断开"),
            Err(ReadError::TooLong) => return interrupted(dropped, "超长行"),
            Err(ReadError::Io(_)) => return interrupted(dropped, "读取失败"),
        }
        let frame = trim_cr(&line);
        if frame.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match parse_frame(frame) {
            Ok(Frame::Out { stream, mut data }) => {
                let room = RECORD_OUTPUT_CAP.saturating_sub(forwarded);
                if data.len() > room {
                    data.truncate(room);
                    dropped = true;
                }
                if !data.is_empty() {
                    forwarded += data.len();
                    sink(FeedEvent::Output {
                        pty_id,
                        call,
                        stream,
                        data,
                    });
                }
            }
            Ok(Frame::End(end)) => {
                sink(FeedEvent::End {
                    pty_id,
                    call,
                    end,
                    dropped,
                });
                return Served::Completed;
            }
            Ok(Frame::Start { .. }) => return interrupted(dropped, "重复 start"),
            Err(_) => return interrupted(dropped, "帧解析失败"),
        }
    }
}

/// 给 [`serve`] 用的 socket 包装:首帧期限是**整段**的,每次读之前按剩余时间重设读超时。
struct DeadlineStream {
    inner: TcpStream,
    deadline: Option<Instant>,
}

impl DeadlineStream {
    fn lift(&mut self) {
        self.deadline = None;
        let _ = self.inner.set_read_timeout(None);
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.inner.set_read_timeout(Some(left))?;
        }
        self.inner.read(buf)
    }
}

// ---------------------------------------------------------------------------
// 监听
// ---------------------------------------------------------------------------

/// nonce ↔ pty_id 双向表。监听线程只读 `by_nonce` 做认证。
#[derive(Default)]
struct NonceTable {
    by_nonce: HashMap<String, u32>,
    by_pty: HashMap<u32, String>,
}

type Sink = dyn Fn(FeedEvent) + Send + Sync;

struct Shared {
    table: Mutex<NonceTable>,
    active: AtomicUsize,
    next_call: AtomicU64,
    start_deadline: Duration,
    sink: Box<Sink>,
}

/// 抄送监听。`bind` 之后 accept 线程常驻到进程结束(与 hook server 同口径)。
pub struct FeedServer {
    port: u16,
    shared: Arc<Shared>,
}

impl FeedServer {
    /// 在 `127.0.0.1:0` 上绑一个随机端口并起 accept 线程。
    pub fn bind(sink: impl Fn(FeedEvent) + Send + Sync + 'static) -> io::Result<Self> {
        Self::bind_with(START_DEADLINE, sink)
    }

    fn bind_with(
        start_deadline: Duration,
        sink: impl Fn(FeedEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared {
            table: Mutex::new(NonceTable::default()),
            active: AtomicUsize::new(0),
            next_call: AtomicU64::new(1),
            start_deadline,
            sink: Box::new(sink),
        });
        let accept = shared.clone();
        std::thread::Builder::new()
            .name("mt-ssh-feed".into())
            .spawn(move || accept_loop(listener, accept))?;
        Ok(Self { port, shared })
    }

    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 给一个 PTY 登记新 nonce,返回要注入的变量值 `127.0.0.1:<port>/<nonce>`。
    /// 同一 PTY 再登记会顶掉旧的。随机源出错返回 `None`(不注入)。
    pub fn register(&self, pty_id: u32) -> Option<String> {
        let nonce = new_nonce()?;
        let mut table = self.shared.table.lock();
        if let Some(old) = table.by_pty.insert(pty_id, nonce.clone()) {
            table.by_nonce.remove(&old);
        }
        table.by_nonce.insert(nonce.clone(), pty_id);
        Some(format!("127.0.0.1:{}/{nonce}", self.port))
    }

    /// 注销一个 PTY 的 nonce。之后用它连上来的一律断开(已在跑的连接不受影响)。
    pub fn forget(&self, pty_id: u32) {
        let mut table = self.shared.table.lock();
        if let Some(nonce) = table.by_pty.remove(&pty_id) {
            table.by_nonce.remove(&nonce);
        }
    }

    #[cfg(test)]
    fn is_registered(&self, pty_id: u32) -> bool {
        self.shared.table.lock().by_pty.contains_key(&pty_id)
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("[ssh-activity] accept 失败: {:?}", err.kind());
                // 持续出错(句柄耗尽之类)时别空转吃满一个核
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        if shared.active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            shared.active.fetch_sub(1, Ordering::AcqRel);
            eprintln!("[ssh-activity] 连接数已达上限 {MAX_CONNECTIONS},断开新连接");
            continue;
        }
        let conn = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("mt-ssh-feed-conn".into())
            .spawn(move || {
                handle_connection(stream, &conn);
                conn.active.fetch_sub(1, Ordering::AcqRel);
            });
        if spawned.is_err() {
            shared.active.fetch_sub(1, Ordering::AcqRel);
            eprintln!("[ssh-activity] 起连接线程失败,断开该连接");
        }
    }
}

fn handle_connection(stream: TcpStream, shared: &Shared) {
    let call = shared.next_call.fetch_add(1, Ordering::Relaxed);
    let mut reader = BufReader::new(DeadlineStream {
        inner: stream,
        deadline: Some(Instant::now() + shared.start_deadline),
    });
    let resolve = |nonce: &str| shared.table.lock().by_nonce.get(nonce).copied();
    let sink = |event: FeedEvent| (shared.sink)(event);
    let served = serve(
        &mut reader,
        call,
        &resolve,
        &mut DeadlineStream::lift,
        &sink,
    );
    match served {
        Served::Completed => {}
        Served::Rejected(why) | Served::Interrupted(why) => {
            eprintln!("[ssh-activity] 断开连接: {why}");
        }
    }
    // 显式关一下:对端(CLI)若还在写,立刻拿到错误而不是等内核缓冲写满
    let _ = reader.get_ref().inner.shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::sync::mpsc;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn start_line(nonce: &str) -> String {
        format!(
            r#"{{"type":"start","v":1,"nonce":"{nonce}","op":"exec","connection":"prod","command":"ls -la","cwd":"/srv","timeoutSecs":60}}"#
        )
    }

    fn out_line(stream: &str, data: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        format!(r#"{{"type":"out","stream":"{stream}","dataB64":"{b64}"}}"#)
    }

    /// 喂一段内存里的字节给 [`serve`],收集事件。
    fn run(input: &str) -> (Served, Vec<FeedEvent>) {
        let events = std::cell::RefCell::new(Vec::new());
        let mut reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let resolve = |nonce: &str| (nonce == NONCE).then_some(7u32);
        let mut lifted = false;
        let served = serve(&mut reader, 42, &resolve, &mut |_| lifted = true, &|ev| {
            events.borrow_mut().push(ev)
        });
        let events = events.into_inner();
        if !events.is_empty() {
            assert!(lifted, "认过 start 之后必须撤掉首帧期限");
        }
        (served, events)
    }

    #[test]
    fn start帧解码() {
        let frame = parse_frame(start_line(NONCE).as_bytes()).unwrap();
        assert_eq!(
            frame,
            Frame::Start {
                nonce: NONCE.into(),
                info: StartInfo {
                    op: Op::Exec,
                    connection: "prod".into(),
                    command: Some("ls -la".into()),
                    cwd: Some("/srv".into()),
                    timeout_secs: Some(60),
                    local_path: None,
                    remote_path: None,
                },
            }
        );
        let upload = parse_frame(
            format!(
                r#"{{"type":"start","v":1,"nonce":"{NONCE}","op":"upload","connection":"c","localPath":"C:\\a.txt","remotePath":"/tmp/a.txt","extra":1}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        let Frame::Start { info, .. } = upload else {
            panic!("应是 start");
        };
        assert_eq!(info.op, Op::Upload);
        assert_eq!(info.local_path.as_deref(), Some(r"C:\a.txt"));
        assert_eq!(info.remote_path.as_deref(), Some("/tmp/a.txt"));
        assert_eq!(info.command, None, "不认识的字段忽略,缺省字段为 None");
    }

    #[test]
    fn start帧的非法形态() {
        let bad = [
            // 版本不认识
            (
                format!(
                    r#"{{"type":"start","v":2,"nonce":"{NONCE}","op":"exec","connection":"c"}}"#
                ),
                FrameError::Version,
            ),
            // 缺版本
            (
                format!(r#"{{"type":"start","nonce":"{NONCE}","op":"exec","connection":"c"}}"#),
                FrameError::Shape,
            ),
            // nonce 形状不对(大写)
            (
                format!(
                    r#"{{"type":"start","v":1,"nonce":"{}","op":"exec","connection":"c"}}"#,
                    NONCE.to_uppercase()
                ),
                FrameError::Shape,
            ),
            // op 不认识(list 不该上报)
            (
                format!(
                    r#"{{"type":"start","v":1,"nonce":"{NONCE}","op":"list","connection":"c"}}"#
                ),
                FrameError::Shape,
            ),
            // 缺连接名
            (
                format!(r#"{{"type":"start","v":1,"nonce":"{NONCE}","op":"exec"}}"#),
                FrameError::Shape,
            ),
            (r#"{"type":"bogus"}"#.to_string(), FrameError::Shape),
            (r#"[1,2]"#.to_string(), FrameError::Shape),
            ("not json".to_string(), FrameError::Json),
        ];
        for (line, want) in bad {
            assert_eq!(parse_frame(line.as_bytes()), Err(want), "{line}");
        }
    }

    #[test]
    fn out帧与end帧解码() {
        assert_eq!(
            parse_frame(out_line("stderr", b"\xffboom\n").as_bytes()),
            Ok(Frame::Out {
                stream: Stream::Stderr,
                data: b"\xffboom\n".to_vec(),
            })
        );
        assert_eq!(
            parse_frame(br#"{"type":"out","stream":"stdout","dataB64":"!!!"}"#),
            Err(FrameError::Base64)
        );
        assert_eq!(
            parse_frame(br#"{"type":"out","stream":"tty","dataB64":""}"#),
            Err(FrameError::Shape)
        );
        assert_eq!(
            parse_frame(
                br#"{"type":"end","exitCode":130,"timedOut":true,"outputBytes":5,"truncated":true,"error":"boom"}"#
            ),
            Ok(Frame::End(EndInfo {
                exit_code: Some(130),
                timed_out: true,
                not_started: false,
                bytes: None,
                error: Some("boom".into()),
                output_bytes: Some(5),
                truncated: true,
            }))
        );
        // 全缺省
        assert_eq!(
            parse_frame(br#"{"type":"end"}"#),
            Ok(Frame::End(EndInfo::default()))
        );
    }

    #[test]
    fn 完整的一次调用() {
        let input = format!(
            "{}\n{}\r\n\n{}\n{}\n",
            start_line(NONCE),
            out_line("stdout", b"hello\n"),
            out_line("stderr", b"warn\n"),
            r#"{"type":"end","exitCode":0,"outputBytes":11}"#
        );
        let (served, events) = run(&input);
        assert_eq!(served, Served::Completed);
        assert_eq!(events.len(), 4);
        assert!(
            matches!(&events[0], FeedEvent::Start { pty_id: 7, call: 42, info } if info.command.as_deref() == Some("ls -la"))
        );
        assert_eq!(
            events[1],
            FeedEvent::Output {
                pty_id: 7,
                call: 42,
                stream: Stream::Stdout,
                data: b"hello\n".to_vec(),
            }
        );
        assert!(matches!(
            &events[2],
            FeedEvent::Output {
                stream: Stream::Stderr,
                ..
            }
        ));
        assert!(matches!(
            &events[3],
            FeedEvent::End { end, dropped: false, .. } if end.exit_code == Some(0)
        ));
    }

    #[test]
    fn 首帧阶段的各种拒绝都不产生事件() {
        let other = "ffffffffffffffffffffffffffffffff";
        let cases = [
            (start_line(other) + "\n", "nonce 不认识"),
            (out_line("stdout", b"x") + "\n", "首帧不是 start"),
            ("garbage\n".to_string(), "首帧解析失败"),
            (String::new(), "首帧前断开"),
            ("x".repeat(MAX_LINE_BYTES + 1) + "\n", "首帧超长"),
            (
                format!(
                    r#"{{"type":"start","v":9,"nonce":"{NONCE}","op":"exec","connection":"c"}}"#
                ) + "\n",
                "协议版本不认识",
            ),
        ];
        for (input, why) in cases {
            let (served, events) = run(&input);
            assert_eq!(served, Served::Rejected(why));
            assert!(events.is_empty(), "{why}: 不该产生任何记录");
        }
    }

    #[test]
    fn end之前断开记成已中断() {
        let input = format!("{}\n{}\n", start_line(NONCE), out_line("stdout", b"part"));
        let (served, events) = run(&input);
        assert_eq!(served, Served::Interrupted("end 之前断开"));
        assert!(matches!(
            events.last(),
            Some(FeedEvent::Interrupted { dropped: false, .. })
        ));

        // start 之后的超长行 / 坏帧 / 重复 start 同样按已中断收尾
        for (tail, why) in [
            ("y".repeat(MAX_LINE_BYTES + 1) + "\n", "超长行"),
            ("{oops\n".to_string(), "帧解析失败"),
            (start_line(NONCE) + "\n", "重复 start"),
        ] {
            let (served, events) = run(&format!("{}\n{tail}", start_line(NONCE)));
            assert_eq!(served, Served::Interrupted(why));
            assert!(matches!(events.last(), Some(FeedEvent::Interrupted { .. })));
        }
    }

    #[test]
    fn 镜像输出超过上限的部分不往下送() {
        let chunk = vec![b'a'; 32 * 1024];
        let mut input = start_line(NONCE) + "\n";
        // 33 × 32 KiB = 1 MiB + 32 KiB
        for _ in 0..33 {
            input += &(out_line("stdout", &chunk) + "\n");
        }
        input += r#"{"type":"end","exitCode":0}"#;
        let (served, events) = run(&input);
        assert_eq!(served, Served::Completed, "末尾没换行的最后一行也算一帧");
        let forwarded: usize = events
            .iter()
            .map(|ev| match ev {
                FeedEvent::Output { data, .. } => data.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(forwarded, RECORD_OUTPUT_CAP);
        assert!(matches!(
            events.last(),
            Some(FeedEvent::End { dropped: true, .. })
        ));
    }

    #[test]
    fn nonce形状() {
        let nonce = new_nonce().unwrap();
        assert!(is_nonce(&nonce), "{nonce}");
        assert_ne!(nonce, new_nonce().unwrap());
        assert!(!is_nonce("abc"));
        assert!(!is_nonce(&"g".repeat(32)));
    }

    // ---- 真 TCP:进程内监听 + 假客户端 ----

    fn server(deadline: Duration) -> (FeedServer, mpsc::Receiver<FeedEvent>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let server = FeedServer::bind_with(deadline, move |ev| {
            let _ = tx.lock().send(ev);
        })
        .expect("bind 127.0.0.1:0");
        (server, rx)
    }

    fn nonce_of(value: &str) -> &str {
        value.rsplit('/').next().unwrap()
    }

    /// 等对端关连接:读到 EOF 或连接被重置都算。
    fn wait_closed(stream: &mut TcpStream) -> bool {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 16];
        matches!(stream.read(&mut buf), Ok(0) | Err(_))
    }

    #[test]
    fn 端到端_真监听收到完整序列() {
        let (server, rx) = server(START_DEADLINE);
        let value = server.register(3).expect("登记 nonce");
        assert!(value.starts_with(&format!("127.0.0.1:{}/", server.port())));
        let nonce = nonce_of(&value).to_string();
        assert!(is_nonce(&nonce));

        let mut client = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        let body = format!(
            "{}\n{}\n{}\n",
            start_line(&nonce),
            out_line("stdout", "中文".as_bytes()),
            r#"{"type":"end","exitCode":2}"#
        );
        client.write_all(body.as_bytes()).unwrap();
        assert!(wait_closed(&mut client), "收到 end 后服务端关连接");

        let timeout = Duration::from_secs(5);
        let ev = rx.recv_timeout(timeout).unwrap();
        assert!(matches!(ev, FeedEvent::Start { pty_id: 3, .. }));
        let ev = rx.recv_timeout(timeout).unwrap();
        assert!(matches!(&ev, FeedEvent::Output { data, .. } if data == "中文".as_bytes()));
        let ev = rx.recv_timeout(timeout).unwrap();
        assert!(matches!(&ev, FeedEvent::End { end, .. } if end.exit_code == Some(2)));
    }

    #[test]
    fn 端到端_首帧超时与注销后的nonce都被断开() {
        let (server, rx) = server(Duration::from_millis(200));
        let value = server.register(5).unwrap();
        let nonce = nonce_of(&value).to_string();

        // 连上不说话:期限一到被断开
        let started = Instant::now();
        let mut idle = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        assert!(wait_closed(&mut idle));
        assert!(started.elapsed() < Duration::from_secs(4), "首帧期限要生效");

        // 一个字节一个字节地慢慢喂也逃不过整段期限
        let mut drip = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        for b in b"{\"type\"" {
            if drip.write_all(&[*b]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        assert!(wait_closed(&mut drip));

        // 注销之后同一个 nonce 不再认
        server.forget(5);
        assert!(!server.is_registered(5));
        let mut late = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        late.write_all((start_line(&nonce) + "\n").as_bytes())
            .unwrap();
        assert!(wait_closed(&mut late));

        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "首帧阶段被拒的连接不产生任何事件"
        );
    }

    #[test]
    fn 端到端_客户端中途断开记成已中断() {
        let (server, rx) = server(START_DEADLINE);
        let value = server.register(9).unwrap();
        let nonce = nonce_of(&value).to_string();
        {
            let mut client = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
            client
                .write_all((start_line(&nonce) + "\n").as_bytes())
                .unwrap();
            // drop = CLI 被杀
        }
        let timeout = Duration::from_secs(5);
        assert!(matches!(
            rx.recv_timeout(timeout).unwrap(),
            FeedEvent::Start { pty_id: 9, .. }
        ));
        assert!(matches!(
            rx.recv_timeout(timeout).unwrap(),
            FeedEvent::Interrupted { pty_id: 9, .. }
        ));
    }

    #[test]
    fn 重新登记顶掉旧nonce() {
        let (server, _rx) = server(START_DEADLINE);
        let a = server.register(1).unwrap();
        let b = server.register(1).unwrap();
        assert_ne!(a, b);
        let table = server.shared.table.lock();
        assert_eq!(table.by_nonce.len(), 1);
        assert_eq!(table.by_nonce.get(nonce_of(&b)), Some(&1));
    }
}
