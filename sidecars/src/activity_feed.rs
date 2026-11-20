//! activity_feed —— mt-ssh-cli 把每次业务调用(exec / upload / download)抄送给所在 mini-term
//! pane 的「SSH 交互浮层」(spec `docs/specs/ssh-activity-overlay-v1.md`「通道与线协议」)。
//!
//! 通道:主程序起本地 PTY 时注入环境变量 `MINITERM_SSH_FEED=127.0.0.1:<port>/<nonce>`,CLI
//! 继承 pane 的环境,天然知道该报给哪个 mini-term 实例的哪个 pane。没有这个变量、或解析不了
//! → 不启用;**不回落去读任何端口文件**。只有 `mt-ssh-cli` 用它(`mt-ssh-mcp` 不接)。
//!
//! 线协议:一次业务调用 = 一条 TCP 连接,内容是 newline-delimited JSON(serde camelCase,UTF-8,
//! 每行一帧):`start`(带 `v` 与 nonce)→ 若干 `out`(仅 exec;原始字节 base64)→ 恰好一个
//! `end`,随后关闭连接。
//!
//! 铁律:**绝不影响 CLI 本身** —— stdout / stderr、退出码、时序都与不启用时一致:
//! - 网络 I/O 全在一条后台线程;业务路径只往有界的待发缓冲里追加([`ActivityFeed::output`]),
//!   从不等网络。超出镜像上限或缓冲满就丢弃、记 `truncated`;
//! - 连接超时 [`CONNECT_TIMEOUT`]、写超时 [`WRITE_TIMEOUT`];连不上或写失败 → 本次调用整体
//!   放弃上报;
//! - 镜像上限:每次调用 [`MIRROR_LIMIT`] 原始输出,单个 out 帧原始数据 ≤ [`MAX_CHUNK`];
//! - 收尾 [`ActivityFeed::finish`] 把 end 交给后台线程并等它写完,最多等 [`FINISH_WAIT`],到点
//!   放弃(调用方随即 `process::exit`,后台线程跟着消失);
//! - 自己的任何失败都不打印、不 panic、不改退出码。

use std::collections::VecDeque;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::ipc::b64_encode;
use crate::ssh_service::StreamKind;

/// 环境变量名。刻意避开 `KEY` / `SECRET` / `TOKEN` 字样:Codex 的 shell 环境策略可能按名字
/// 过滤这类变量。
pub const FEED_ENV: &str = "MINITERM_SSH_FEED";

/// 线协议版本,只出现在 `start` 帧的 `v`。主程序遇到不认识的版本直接断开。
pub const FEED_PROTOCOL_VERSION: u32 = 1;

/// 连主程序监听的上限。主程序活着时本机回环连接是瞬时的;端口没人监听时(Windows 对回环
/// 端口的 RST 还会重试 SYN,不设上限要等约 2 秒)到点即放弃。
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// 单次写的上限(`SO_SNDTIMEO`)。主程序卡住不读时写会阻塞,到点按写失败放弃。
pub const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// 收尾时等后台线程把 end 写完的总上限,到点放弃。
pub const FINISH_WAIT: Duration = Duration::from_millis(500);

/// 每次调用最多镜像这么多原始输出字节;之后的输出只计数,`end.truncated = true`。
pub const MIRROR_LIMIT: usize = 1024 * 1024;

/// 单个 out 帧的原始数据上限(base64 之前)。
pub const MAX_CHUNK: usize = 32 * 1024;

/// 待发缓冲最多攒多少段(每段发成一个 out 帧)。同一路流的相邻分片并进同一段(≤ [`MAX_CHUNK`]),
/// 段数只在 stdout / stderr 频繁交错、后台线程又卡在建连或写阻塞时才涨得上去;总字节数本来
/// 就受 [`MIRROR_LIMIT`] 约束,这里约束的是段数(每段一份分配)。
const MAX_PENDING_SEGMENTS: usize = 1024;

/// nonce 是 128 bit 随机数的小写 hex。
const NONCE_HEX_LEN: usize = 32;

// ---------------------------------------------------------------------------
// 环境变量
// ---------------------------------------------------------------------------

/// 抄送目标:主程序的本机监听地址 + 这个 pane 的 nonce。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedTarget {
    pub addr: SocketAddr,
    pub nonce: String,
}

/// 解析 `MINITERM_SSH_FEED` 的值 `127.0.0.1:<port>/<nonce>`(纯函数);任何一处不合规返回 `None`。
///
/// 主机只认 `127.0.0.1`(命令与输出原文只许发往本机回环,主程序也只绑这个地址);端口不能是 0;
/// nonce 必须恰好是 32 位小写 hex —— 主程序按原串查表认 pane,别的写法反正对不上。
pub fn parse_target(value: &str) -> Option<FeedTarget> {
    let (addr, nonce) = value.trim().split_once('/')?;
    let addr: SocketAddrV4 = addr.parse().ok()?;
    if *addr.ip() != Ipv4Addr::LOCALHOST || addr.port() == 0 {
        return None;
    }
    let nonce_ok = nonce.len() == NONCE_HEX_LEN
        && nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    nonce_ok.then(|| FeedTarget {
        addr: SocketAddr::V4(addr),
        nonce: nonce.to_string(),
    })
}

/// 从本进程环境读抄送目标;变量不在、不是 UTF-8 或解析不了都是 `None`(静默不启用)。
pub fn target_from_env() -> Option<FeedTarget> {
    parse_target(&std::env::var(FEED_ENV).ok()?)
}

// ---------------------------------------------------------------------------
// 一次调用的内容
// ---------------------------------------------------------------------------

/// 上报的业务操作。list 与 daemon 运维子命令不碰服务器的交互,不在此列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FeedOp {
    Exec,
    Upload,
    Download,
}

/// `start` 帧的内容(调用方按 op 填:exec 填命令 / cwd,传输填两条路径)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallStart {
    pub op: FeedOp,
    /// AI 传的连接名或 id(原样)。
    pub connection: String,
    /// exec:join 之后的原始远程命令(不脱敏)。
    pub command: Option<String>,
    /// exec:`--cwd` 原值。
    pub cwd: Option<String>,
    /// AI 显式传了 `--timeout` 才有。
    pub timeout_secs: Option<u64>,
    /// upload / download:绝对化之后的本地路径。
    pub local_path: Option<String>,
    /// upload / download:远端路径原值。
    pub remote_path: Option<String>,
}

/// 调用的结局,进 `end` 帧。输出字节数与是否截断由 [`ActivityFeed`] 自己统计,不在这里。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallOutcome {
    /// exec 拿到的远程退出码(信号终止时为 128+n)。
    pub exit_code: Option<i32>,
    /// exec 运行中超时。
    pub timed_out: bool,
    /// exec 连接阶段超时,命令未启动。
    pub not_started: bool,
    /// upload / download 成功传输的字节数。
    pub bytes: Option<u64>,
    /// 错误原文(连接 / 认证 / 令牌 / 服务端拒绝 / daemon 中途断开等;不带 CLI 的 stderr 前缀)。
    pub error: Option<String>,
}

impl CallOutcome {
    /// exec 的结局。
    pub fn exec(exit_code: Option<i32>, timed_out: bool, not_started: bool) -> Self {
        Self {
            exit_code,
            timed_out,
            not_started,
            ..Self::default()
        }
    }

    /// upload / download 成功。
    pub fn transferred(bytes: u64) -> Self {
        Self {
            bytes: Some(bytes),
            ..Self::default()
        }
    }

    /// 以错误收尾。
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// 帧编码
// ---------------------------------------------------------------------------

/// 线上的三种帧(只编码不解码;解码在主程序)。
#[derive(Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Frame<'a> {
    Start {
        v: u32,
        nonce: &'a str,
        op: FeedOp,
        connection: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        local_path: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote_path: Option<&'a str>,
    },
    Out {
        stream: &'static str,
        data_b64: String,
    },
    End {
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        timed_out: bool,
        not_started: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
        output_bytes: u64,
        truncated: bool,
    },
}

/// 一帧编成一行(含结尾换行)。serde_json 会转义字符串里的换行,所以恰好一行;这几种帧只有
/// 字符串 / 数字 / 布尔字段,序列化不会失败 —— 万一失败发出的是空行,主程序按解析失败断开。
fn encode_line(frame: &Frame<'_>) -> String {
    let mut line = serde_json::to_string(frame).unwrap_or_default();
    line.push('\n');
    line
}

fn encode_start(nonce: &str, start: &CallStart) -> String {
    encode_line(&Frame::Start {
        v: FEED_PROTOCOL_VERSION,
        nonce,
        op: start.op,
        connection: &start.connection,
        command: start.command.as_deref(),
        cwd: start.cwd.as_deref(),
        timeout_secs: start.timeout_secs,
        local_path: start.local_path.as_deref(),
        remote_path: start.remote_path.as_deref(),
    })
}

fn encode_out(stream: StreamKind, data: &[u8]) -> String {
    encode_line(&Frame::Out {
        stream: match stream {
            StreamKind::Stdout => "stdout",
            StreamKind::Stderr => "stderr",
        },
        data_b64: b64_encode(data),
    })
}

/// `end` 帧。没记下结局(不该发生)时各字段取缺省。
fn encode_end(outcome: Option<&CallOutcome>, mirror: &MirrorBuffer) -> String {
    let fallback = CallOutcome::default();
    let outcome = outcome.unwrap_or(&fallback);
    encode_line(&Frame::End {
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        not_started: outcome.not_started,
        bytes: outcome.bytes,
        error: outcome.error.as_deref(),
        output_bytes: mirror.output_bytes,
        truncated: mirror.truncated,
    })
}

// ---------------------------------------------------------------------------
// 待发缓冲(纯数据结构,可单测)
// ---------------------------------------------------------------------------

/// 一次调用的输出镜像:待发段 + 计数。镜像始终是输出的一段**前缀** —— 一旦有输出没收进来
/// (超上限或段数满),之后的输出只计数,不再镜像,界面不会看到中间缺一截的输出。
#[derive(Debug, Default)]
struct MirrorBuffer {
    /// 还没交给后台线程的段,按到达顺序(stdout / stderr 交错保序),每段 ≤ [`MAX_CHUNK`]。
    segments: VecDeque<(StreamKind, Vec<u8>)>,
    /// 已收进镜像的原始字节数(≤ [`MIRROR_LIMIT`])。
    mirrored: usize,
    /// 服务器输出的总字节数(含没镜像的部分)。
    output_bytes: u64,
    /// 有输出没镜像出去。
    truncated: bool,
}

impl MirrorBuffer {
    fn push(&mut self, stream: StreamKind, data: &[u8]) {
        self.output_bytes = self.output_bytes.saturating_add(data.len() as u64);
        if self.truncated {
            return;
        }
        let room = MIRROR_LIMIT.saturating_sub(self.mirrored);
        let mut rest = &data[..data.len().min(room)];
        while !rest.is_empty() {
            let taken = match self.segments.back_mut() {
                // 与上一段同一路流且没满:并进去,少发几帧。
                Some((kind, buf)) if *kind == stream && buf.len() < MAX_CHUNK => {
                    let n = rest.len().min(MAX_CHUNK - buf.len());
                    buf.extend_from_slice(&rest[..n]);
                    n
                }
                _ => {
                    if self.segments.len() >= MAX_PENDING_SEGMENTS {
                        break;
                    }
                    let n = rest.len().min(MAX_CHUNK);
                    self.segments.push_back((stream, rest[..n].to_vec()));
                    n
                }
            };
            self.mirrored += taken;
            rest = &rest[taken..];
        }
        if !rest.is_empty() || data.len() > room {
            self.truncated = true;
        }
    }
}

// ---------------------------------------------------------------------------
// 业务线程与后台线程之间的共享状态
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct State {
    mirror: MirrorBuffer,
    outcome: Option<CallOutcome>,
    /// 业务侧已收尾:end 待发,此后不再收输出。
    closing: bool,
    /// 后台线程已放弃本次上报(连不上 / 写失败),此后不再收输出。
    abandoned: bool,
    /// 后台线程已退出。
    done: bool,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    /// 一个条件变量两头用:后台线程等「有输出或已收尾」,业务线程收尾时等「后台线程已退出」。
    /// 两者不会同时等(收尾之后不再有输出),一律 `notify_all`。
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 后台线程退出(含 panic 展开)时记 `done` 并叫醒等收尾的业务线程。
struct WorkerExit<'a>(&'a Shared);

impl Drop for WorkerExit<'_> {
    fn drop(&mut self) {
        self.0.lock().done = true;
        self.0.wake.notify_all();
    }
}

/// 后台线程主体:建连 → start → 随到随发 out → end → 关闭。出错即放弃本次上报。
/// 连接在 `deliver` 返回时就析构了(关 socket),之后才记 `done`。
fn run_worker<W, C>(connect: C, start_line: String, shared: Arc<Shared>)
where
    W: Write,
    C: FnOnce() -> std::io::Result<W>,
{
    let _exit = WorkerExit(&shared);
    if deliver(connect, &start_line, &shared).is_err() {
        let mut state = shared.lock();
        state.abandoned = true;
        // 不会再有人发了,别攒着。
        state.mirror.segments.clear();
    }
}

fn deliver<W, C>(connect: C, start_line: &str, shared: &Shared) -> std::io::Result<()>
where
    W: Write,
    C: FnOnce() -> std::io::Result<W>,
{
    let mut conn = connect()?;
    conn.write_all(start_line.as_bytes())?;
    loop {
        let (segments, end_line) = next_batch(shared);
        let mut batch = String::new();
        for (stream, data) in &segments {
            batch.push_str(&encode_out(*stream, data));
        }
        if let Some(end) = &end_line {
            batch.push_str(end);
        }
        conn.write_all(batch.as_bytes())?;
        if end_line.is_some() {
            return conn.flush();
        }
    }
}

/// 等到有待发输出或业务侧已收尾,取走全部待发段;已收尾时连同 end 帧一起取(收尾后不再收输出,
/// 所以 end 一定是最后一帧,且恰好一帧)。
fn next_batch(shared: &Shared) -> (VecDeque<(StreamKind, Vec<u8>)>, Option<String>) {
    let mut state = shared.lock();
    while state.mirror.segments.is_empty() && !state.closing {
        state = shared
            .wake
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner);
    }
    let segments = std::mem::take(&mut state.mirror.segments);
    let end = state
        .closing
        .then(|| encode_end(state.outcome.as_ref(), &state.mirror));
    (segments, end)
}

/// 连主程序的监听:连接 / 写都带上限(见 [`CONNECT_TIMEOUT`] / [`WRITE_TIMEOUT`])。
fn connect_tcp(addr: SocketAddr) -> std::io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    // 输出分片尽快到浮层;设不上无所谓。
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 一次业务调用的抄送。没启用([`ActivityFeed::disabled`])时所有方法都是空操作,调用方不必
/// 区分;启用时 start 帧在构造那一刻就交给了后台线程。
#[derive(Debug)]
pub struct ActivityFeed {
    shared: Option<Arc<Shared>>,
}

impl ActivityFeed {
    /// 不上报。
    pub fn disabled() -> Self {
        Self { shared: None }
    }

    /// 按本进程环境变量 [`FEED_ENV`] 开始一次上报;变量不在或解析不了 → [`Self::disabled`]。
    pub fn from_env(start: CallStart) -> Self {
        match target_from_env() {
            Some(target) => Self::begin(target, start),
            None => Self::disabled(),
        }
    }

    /// 向 `target` 开始一次上报:起后台线程建连并发 start。线程起不来 → 不上报。
    pub fn begin(target: FeedTarget, start: CallStart) -> Self {
        let start_line = encode_start(&target.nonce, &start);
        let addr = target.addr;
        Self::spawn(move || connect_tcp(addr), start_line)
    }

    /// [`Self::begin`] 的可注入版本:`connect` 在后台线程里给出要写的连接(测试借它造一个
    /// 永远写不完的对端)。
    fn spawn<W, C>(connect: C, start_line: String) -> Self
    where
        W: Write,
        C: FnOnce() -> std::io::Result<W> + Send + 'static,
    {
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("mt-ssh-cli-feed".into())
            .spawn(move || run_worker(connect, start_line, worker));
        Self {
            shared: spawned.ok().map(|_| shared),
        }
    }

    /// 是否在上报。
    pub fn is_enabled(&self) -> bool {
        self.shared.is_some()
    }

    /// 抄一段服务器输出(业务路径调用):只追加进有界缓冲、叫醒后台线程,从不等网络。
    pub fn output(&self, stream: StreamKind, data: &[u8]) {
        let Some(shared) = &self.shared else {
            return;
        };
        if data.is_empty() {
            return;
        }
        let mut state = shared.lock();
        if state.abandoned || state.closing {
            return;
        }
        state.mirror.push(stream, data);
        drop(state);
        shared.wake.notify_all();
    }

    /// 记下调用的结局(进 end 帧;多次记以最后一次为准)。
    pub fn record(&self, outcome: CallOutcome) {
        if let Some(shared) = &self.shared {
            shared.lock().outcome = Some(outcome);
        }
    }

    /// 收尾:把 end 交给后台线程,等它写完,最多等 [`FINISH_WAIT`]。调用方随后 `process::exit`。
    pub fn finish(self) {
        self.finish_within(FINISH_WAIT);
    }

    /// [`Self::finish`] 的可调上限版本;返回 end 是否已交给内核(`false` = 没启用、已放弃,或到点
    /// 还没写完)。
    pub fn finish_within(self, wait: Duration) -> bool {
        let Some(shared) = self.shared else {
            return false;
        };
        let deadline = Instant::now() + wait;
        let mut state = shared.lock();
        state.closing = true;
        shared.wake.notify_all();
        while !state.done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = match shared.wake.wait_timeout(state, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        !state.abandoned
    }
}

// ============================================================================
// tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::mpsc;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    /// 收尾等待的容差:线程调度 / 计时粒度,不放宽「最多等 500ms」本身。
    const SLACK: Duration = Duration::from_millis(250);

    fn target(addr: SocketAddr) -> FeedTarget {
        FeedTarget {
            addr,
            nonce: NONCE.to_string(),
        }
    }

    fn exec_start() -> CallStart {
        CallStart {
            op: FeedOp::Exec,
            connection: "prod".into(),
            command: Some("tail -n 5 /var/log/syslog".into()),
            cwd: Some("~/app".into()),
            timeout_secs: Some(30),
            local_path: None,
            remote_path: None,
        }
    }

    fn json(line: &str) -> serde_json::Value {
        serde_json::from_str(line).unwrap_or_else(|e| panic!("bad frame {line:?}: {e}"))
    }

    /// 假主程序:accept 一条连接、读到 EOF,按行交回解析好的帧。
    fn spawn_collector() -> (SocketAddr, std::thread::JoinHandle<Vec<serde_json::Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            BufReader::new(stream)
                .lines()
                .map(|line| json(&line.unwrap()))
                .collect()
        });
        (addr, handle)
    }

    fn decode_out(frame: &serde_json::Value) -> (String, Vec<u8>) {
        assert_eq!(frame["type"], "out", "{frame}");
        let data = crate::ipc::b64_decode(frame["dataB64"].as_str().unwrap()).unwrap();
        (frame["stream"].as_str().unwrap().to_string(), data)
    }

    // --- 环境变量解析 ---

    #[test]
    fn parse_target_accepts_the_documented_shape() {
        let parsed = parse_target(&format!("127.0.0.1:52817/{NONCE}")).unwrap();
        assert_eq!(
            parsed.addr,
            "127.0.0.1:52817".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(parsed.nonce, NONCE);
        // 首尾空白不算错。
        assert!(parse_target(&format!(" 127.0.0.1:1/{NONCE}\n")).is_some());
    }

    #[test]
    fn parse_target_rejects_malformed_values() {
        let upper = NONCE.to_uppercase();
        let short = &NONCE[..31];
        let long = format!("{NONCE}0");
        let bad: Vec<String> = vec![
            String::new(),
            "127.0.0.1:5000".into(),
            format!("127.0.0.1/{NONCE}"),
            format!("localhost:5000/{NONCE}"),
            format!("0.0.0.0:5000/{NONCE}"),
            format!("10.1.2.3:5000/{NONCE}"),
            format!("[::1]:5000/{NONCE}"),
            format!("127.0.0.1:0/{NONCE}"),
            format!("127.0.0.1:65536/{NONCE}"),
            format!("127.0.0.1:port/{NONCE}"),
            "127.0.0.1:5000/".into(),
            format!("127.0.0.1:5000/{short}"),
            format!("127.0.0.1:5000/{long}"),
            format!("127.0.0.1:5000/{upper}"),
            "127.0.0.1:5000/0123456789abcdef0123456789abcdeg".into(),
            format!("127.0.0.1:5000/{NONCE}/extra"),
            format!("http://127.0.0.1:5000/{NONCE}"),
        ];
        for value in bad {
            assert!(parse_target(&value).is_none(), "accepted {value:?}");
        }
    }

    // --- 帧编码 ---

    #[test]
    fn start_frame_carries_exec_fields_only() {
        let line = encode_start(NONCE, &exec_start());
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let frame = json(&line);
        assert_eq!(frame["type"], "start");
        assert_eq!(frame["v"], FEED_PROTOCOL_VERSION);
        assert_eq!(frame["nonce"], NONCE);
        assert_eq!(frame["op"], "exec");
        assert_eq!(frame["connection"], "prod");
        assert_eq!(frame["command"], "tail -n 5 /var/log/syslog");
        assert_eq!(frame["cwd"], "~/app");
        assert_eq!(frame["timeoutSecs"], 30);
        assert!(frame.get("localPath").is_none(), "{frame}");
        assert!(frame.get("remotePath").is_none(), "{frame}");
    }

    #[test]
    fn start_frame_for_transfers_and_omitted_optionals() {
        let start = CallStart {
            op: FeedOp::Download,
            connection: "id-7".into(),
            command: None,
            cwd: None,
            timeout_secs: None,
            local_path: Some(r"D:\work\a.log".into()),
            remote_path: Some("/var/log/a.log".into()),
        };
        let frame = json(&encode_start(NONCE, &start));
        assert_eq!(frame["op"], "download");
        assert_eq!(frame["localPath"], r"D:\work\a.log");
        assert_eq!(frame["remotePath"], "/var/log/a.log");
        for absent in ["command", "cwd", "timeoutSecs"] {
            assert!(frame.get(absent).is_none(), "{absent}: {frame}");
        }
        let upload = CallStart {
            op: FeedOp::Upload,
            ..start
        };
        assert_eq!(json(&encode_start(NONCE, &upload))["op"], "upload");
    }

    #[test]
    fn multiline_commands_stay_on_one_line() {
        let start = CallStart {
            command: Some("echo a\necho b".into()),
            ..exec_start()
        };
        let line = encode_start(NONCE, &start);
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(json(&line)["command"], "echo a\necho b");
    }

    #[test]
    fn out_frame_is_binary_safe() {
        let payload = [0x00, 0xff, 0xfe, b'\n', b'\r', 0x80, b'a'];
        let line = encode_out(StreamKind::Stderr, &payload);
        assert_eq!(line.matches('\n').count(), 1);
        let (stream, data) = decode_out(&json(&line));
        assert_eq!(stream, "stderr");
        assert_eq!(data, payload);
        assert_eq!(
            json(&encode_out(StreamKind::Stdout, b"x"))["stream"],
            "stdout"
        );
    }

    #[test]
    fn end_frame_fields() {
        let mirror = MirrorBuffer {
            output_bytes: 56789,
            truncated: true,
            ..MirrorBuffer::default()
        };
        let exec = json(&encode_end(
            Some(&CallOutcome::exec(Some(137), false, false)),
            &mirror,
        ));
        assert_eq!(exec["type"], "end");
        assert_eq!(exec["exitCode"], 137);
        assert_eq!(exec["timedOut"], false);
        assert_eq!(exec["notStarted"], false);
        assert_eq!(exec["outputBytes"], 56789);
        assert_eq!(exec["truncated"], true);
        assert!(
            exec.get("bytes").is_none() && exec.get("error").is_none(),
            "{exec}"
        );

        let timeout = json(&encode_end(
            Some(&CallOutcome::exec(None, true, true)),
            &MirrorBuffer::default(),
        ));
        assert!(timeout.get("exitCode").is_none(), "{timeout}");
        assert_eq!(timeout["timedOut"], true);
        assert_eq!(timeout["notStarted"], true);
        assert_eq!(timeout["outputBytes"], 0);
        assert_eq!(timeout["truncated"], false);

        let transfer = json(&encode_end(
            Some(&CallOutcome::transferred(1234)),
            &MirrorBuffer::default(),
        ));
        assert_eq!(transfer["bytes"], 1234);
        assert!(transfer.get("exitCode").is_none(), "{transfer}");

        let error = json(&encode_end(
            Some(&CallOutcome::error("invalid project token")),
            &MirrorBuffer::default(),
        ));
        assert_eq!(error["error"], "invalid project token");
    }

    // --- 待发缓冲:分片 / 合并 / 上限 ---

    #[test]
    fn mirror_splits_into_bounded_chunks_and_coalesces_same_stream() {
        let mut mirror = MirrorBuffer::default();
        mirror.push(StreamKind::Stdout, &vec![b'a'; MAX_CHUNK + 10]);
        mirror.push(StreamKind::Stdout, b"bc");
        mirror.push(StreamKind::Stderr, b"err");
        mirror.push(StreamKind::Stdout, b"d");
        let shape: Vec<(StreamKind, usize)> = mirror
            .segments
            .iter()
            .map(|(kind, data)| (*kind, data.len()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (StreamKind::Stdout, MAX_CHUNK),
                (StreamKind::Stdout, 12),
                (StreamKind::Stderr, 3),
                (StreamKind::Stdout, 1),
            ]
        );
        assert_eq!(mirror.segments[1].1, b"aaaaaaaaaabc");
        assert_eq!(mirror.output_bytes, (MAX_CHUNK + 16) as u64);
        assert!(!mirror.truncated);
    }

    #[test]
    fn mirror_keeps_only_the_first_mebibyte() {
        let mut mirror = MirrorBuffer::default();
        let chunk = vec![b'z'; 3000];
        let mut pushed = 0usize;
        while pushed < MIRROR_LIMIT + 5000 {
            mirror.push(StreamKind::Stdout, &chunk);
            pushed += chunk.len();
        }
        let mirrored: usize = mirror.segments.iter().map(|(_, d)| d.len()).sum();
        assert_eq!(mirrored, MIRROR_LIMIT);
        assert_eq!(mirror.mirrored, MIRROR_LIMIT);
        assert!(mirror.segments.iter().all(|(_, d)| d.len() <= MAX_CHUNK));
        assert_eq!(mirror.output_bytes, pushed as u64);
        assert!(mirror.truncated);

        // 恰好 1 MiB 不算截断。
        let mut exact = MirrorBuffer::default();
        exact.push(StreamKind::Stdout, &vec![b'x'; MIRROR_LIMIT]);
        assert!(!exact.truncated);
        exact.push(StreamKind::Stdout, b"");
        assert!(!exact.truncated);
        exact.push(StreamKind::Stderr, b"!");
        assert!(exact.truncated);
        assert_eq!(exact.output_bytes, MIRROR_LIMIT as u64 + 1);
    }

    #[test]
    fn mirror_stops_at_the_first_drop_when_segments_run_out() {
        let mut mirror = MirrorBuffer::default();
        // stdout / stderr 一字节一交错:每次都开新段。
        for i in 0..MAX_PENDING_SEGMENTS {
            let kind = if i % 2 == 0 {
                StreamKind::Stdout
            } else {
                StreamKind::Stderr
            };
            mirror.push(kind, b"x");
        }
        assert!(!mirror.truncated);
        let last = mirror.segments.back().unwrap().0;
        let other = if last == StreamKind::Stdout {
            StreamKind::Stderr
        } else {
            StreamKind::Stdout
        };
        mirror.push(other, b"dropped");
        assert!(mirror.truncated);
        assert_eq!(mirror.segments.len(), MAX_PENDING_SEGMENTS);
        // 之后哪怕能并进最后一段也不再收:镜像始终是前缀。
        mirror.push(last, b"later");
        assert_eq!(mirror.segments.back().unwrap().1, b"x");
        assert_eq!(mirror.mirrored, MAX_PENDING_SEGMENTS);
        assert_eq!(mirror.output_bytes, (MAX_PENDING_SEGMENTS + 12) as u64);
    }

    // --- 端到端:本地 TcpListener 当主程序 ---

    #[test]
    fn listener_receives_start_out_and_exactly_one_end() {
        let (addr, collector) = spawn_collector();
        let feed = ActivityFeed::begin(target(addr), exec_start());
        assert!(feed.is_enabled());
        feed.output(StreamKind::Stdout, b"line 1\n");
        feed.output(StreamKind::Stderr, b"warn\n");
        feed.output(StreamKind::Stdout, &[0xff, 0x00, b'\n']);
        feed.record(CallOutcome::exec(Some(3), false, false));
        assert!(feed.finish_within(Duration::from_secs(10)));

        let frames = collector.join().unwrap();
        assert_eq!(frames.len(), 5, "{frames:?}");
        assert_eq!(frames[0]["type"], "start");
        assert_eq!(frames[0]["nonce"], NONCE);
        assert_eq!(frames[0]["command"], "tail -n 5 /var/log/syslog");
        let outs: Vec<(String, Vec<u8>)> = frames[1..4].iter().map(decode_out).collect();
        assert_eq!(
            outs,
            vec![
                ("stdout".to_string(), b"line 1\n".to_vec()),
                ("stderr".to_string(), b"warn\n".to_vec()),
                ("stdout".to_string(), vec![0xff, 0x00, b'\n']),
            ]
        );
        let end = &frames[4];
        assert_eq!(end["type"], "end");
        assert_eq!(end["exitCode"], 3);
        assert_eq!(end["outputBytes"], 15);
        assert_eq!(end["truncated"], false);
    }

    #[test]
    fn listener_gets_the_first_mebibyte_and_a_truncated_end() {
        let (addr, collector) = spawn_collector();
        let feed = ActivityFeed::begin(target(addr), exec_start());
        let chunk: Vec<u8> = (0..=255u8).cycle().take(7000).collect();
        let mut sent = Vec::new();
        while sent.len() < MIRROR_LIMIT + 20_000 {
            feed.output(StreamKind::Stdout, &chunk);
            sent.extend_from_slice(&chunk);
        }
        feed.record(CallOutcome::exec(Some(0), false, false));
        assert!(feed.finish_within(Duration::from_secs(10)));

        let frames = collector.join().unwrap();
        let (end, outs) = frames[1..].split_last().unwrap();
        let mut mirrored = Vec::new();
        for frame in outs {
            let (stream, data) = decode_out(frame);
            assert_eq!(stream, "stdout");
            assert!(data.len() <= MAX_CHUNK, "out frame of {} bytes", data.len());
            mirrored.extend_from_slice(&data);
        }
        assert_eq!(mirrored, sent[..MIRROR_LIMIT]);
        assert_eq!(end["type"], "end");
        assert_eq!(end["outputBytes"], sent.len() as u64);
        assert_eq!(end["truncated"], true);
    }

    #[test]
    fn transfer_call_sends_start_and_end_only() {
        let (addr, collector) = spawn_collector();
        let start = CallStart {
            op: FeedOp::Upload,
            connection: "prod".into(),
            command: None,
            cwd: None,
            timeout_secs: None,
            local_path: Some("/home/me/a.tar".into()),
            remote_path: Some("/tmp/a.tar".into()),
        };
        let feed = ActivityFeed::begin(target(addr), start);
        feed.record(CallOutcome::transferred(4096));
        assert!(feed.finish_within(Duration::from_secs(10)));
        let frames = collector.join().unwrap();
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(frames[0]["op"], "upload");
        assert_eq!(frames[1]["bytes"], 4096);
        assert_eq!(frames[1]["outputBytes"], 0);
    }

    #[test]
    fn unreachable_port_gives_up_quickly() {
        // 绑一个端口再放掉:之后没人监听。
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let started = Instant::now();
        let feed = ActivityFeed::begin(target(addr), exec_start());
        feed.output(StreamKind::Stdout, b"hello");
        feed.record(CallOutcome::exec(Some(0), false, false));
        assert!(!feed.finish_within(FINISH_WAIT), "nobody listens on {addr}");
        let elapsed = started.elapsed();
        assert!(elapsed <= FINISH_WAIT + SLACK, "took {elapsed:?}");
    }

    /// 写进去就卡住、直到测试结束才返回的连接 —— 模拟主程序卡住不读、写阻塞。
    struct BlockedWriter(mpsc::Receiver<()>);

    impl Write for BlockedWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Err(std::io::ErrorKind::TimedOut.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn blocked_writes_cap_the_exit_wait() {
        let (release, blocked) = mpsc::channel::<()>();
        let feed = ActivityFeed::spawn(
            move || Ok(BlockedWriter(blocked)),
            encode_start(NONCE, &exec_start()),
        );
        feed.output(StreamKind::Stdout, b"stuck");
        feed.record(CallOutcome::exec(Some(0), false, false));
        let started = Instant::now();
        assert!(!feed.finish_within(FINISH_WAIT));
        let elapsed = started.elapsed();
        assert!(elapsed >= FINISH_WAIT, "returned early: {elapsed:?}");
        assert!(elapsed <= FINISH_WAIT + SLACK, "took {elapsed:?}");
        drop(release);
    }

    #[test]
    fn peer_that_never_reads_does_not_hold_the_exit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (release, hold) = mpsc::channel::<()>();
        // 连上但一个字节也不读,直到测试结束(不 join:万一没连上,accept 会一直等)。
        std::thread::spawn(move || {
            let accepted = listener.accept();
            let _ = hold.recv();
            drop(accepted);
        });
        let feed = ActivityFeed::begin(target(addr), exec_start());
        let chunk = vec![b'y'; MAX_CHUNK];
        for _ in 0..MIRROR_LIMIT / MAX_CHUNK {
            feed.output(StreamKind::Stdout, &chunk);
        }
        feed.record(CallOutcome::exec(Some(0), false, false));
        let started = Instant::now();
        feed.finish();
        let elapsed = started.elapsed();
        assert!(elapsed <= FINISH_WAIT + SLACK, "took {elapsed:?}");
        drop(release);
    }

    #[test]
    fn disabled_feed_is_a_no_op() {
        let feed = ActivityFeed::disabled();
        assert!(!feed.is_enabled());
        feed.output(StreamKind::Stdout, b"x");
        feed.record(CallOutcome::error("e"));
        let started = Instant::now();
        assert!(!feed.finish_within(FINISH_WAIT));
        assert!(started.elapsed() < Duration::from_millis(50));
    }
}
