//! OSC 7501（Program Status Protocol）流式只读扫描器与探测应答。
//!
//! 规范：`ESC ] 7501 ; <payload> (BEL | ESC \)`
//! - 能力探测：`<payload>` 为 `?` —— 终端通过 [`Event::PtyWrite`] 原样回显（沿用收到的终止符），
//!   告诉前端 CLI（如 Antigravity CLI `agy`）本终端支持 OSC 7501 状态上报；
//! - 状态上报：`<payload>` 为冒号分隔的 `key=value` 对，例如
//!   `state=blocked:kind=permission:app=antigravity:msg=<base64>`。
//!
//! # 为什么做成只读观察
//!
//! `alacritty_terminal` / `vte` 对不认识的 OSC 编号会静默跳过、不污染画面也不改变 grid，
//! 因此这里不需要像 Kitty 图形那样把序列从输入切片里摘除复制：快路径上只要这批字节不含
//! `ESC`，单次 SIMD `memchr` 后直接返回，零拷贝、零分配。
//!
//! # 开关
//!
//! 默认开启；设 `MT_DISABLE_OSC7501=1` 可整条关闭（既不应答 `?`，也不产出状态事件），
//! 方便排障对比。

use std::sync::atomic::{AtomicBool, Ordering};

use alacritty_terminal::event::{Event, EventListener};
use memchr::{memchr, memchr3};
use parking_lot::Mutex;

use crate::EventQueue;

/// 禁用 OSC 7501 识别与应答的环境变量（取值 `1` 生效，与 `MT_DISABLE_WIN32_INPUT_MODE` 同口径）。
pub const DISABLE_ENV: &str = "MT_DISABLE_OSC7501";

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1A;

/// `ESC ]` 之后要精确匹配的前缀。
const PREFIX: &[u8] = b"7501;";

/// 单条 OSC 7501 payload 上限（规范约定整条序列不超过 4096 字节）。
/// 超出时丢弃该条并继续扫描到终止符，避免恶意/损坏流撑爆内存。
const MAX_PAYLOAD_BYTES: usize = 4096;

/// `app` / `id` / `msg` 单字段落盘长度上限（防异常输入占用多余堆内存）。
const MAX_APP_BYTES: usize = 64;
const MAX_ID_BYTES: usize = 128;
const MAX_MSG_BYTES: usize = 512;

/// OSC 7501 的核心状态值（`state=`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
    Clear,
}

/// `state=blocked` 时的阻塞细分类型（`kind=`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
    Other,
}

/// 一条解析成功的 OSC 7501 状态上报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    pub state: ProgramState,
    /// 仅在 `state == ProgramState::Blocked` 且存在 `kind=` 时有值。
    pub kind: Option<BlockedKind>,
    /// 上报方标识（小写归一化，如 `antigravity`）。
    pub app: Option<String>,
    /// 可选的层次化任务 id（如 `build/tests`）；`None` 表示主会话根状态。
    pub id: Option<String>,
    /// 可选的原始 `msg` 字段（通常为 base64 UTF-8），用于区分同一次会话中不同的阻塞提示。
    pub msg: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lex {
    /// 普通字节。
    Ground,
    /// 刚见过 `ESC`。
    Esc,
    /// 刚见过 `ESC ]`，正在匹配 `7501;`（已匹配字节数 0..=PREFIX.len()）。
    OscPrefix(u8),
    /// 正在收集 `7501;` 之后的 payload。
    Body,
    /// payload 内刚见过 `ESC`：下一个字节是 `\` 即 ST 结束，否则按新序列起点重判。
    BodyEsc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminator {
    Bel,
    St,
}

struct Scanner {
    lex: Lex,
    body: Vec<u8>,
    overflow: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self {
            lex: Lex::Ground,
            body: Vec::new(),
            overflow: false,
        }
    }
}

impl Scanner {
    fn reset(&mut self) {
        self.lex = Lex::Ground;
        self.body.clear();
        self.overflow = false;
    }

    fn push_body(&mut self, slice: &[u8]) {
        if self.overflow || slice.is_empty() {
            return;
        }
        if self.body.len().saturating_add(slice.len()) > MAX_PAYLOAD_BYTES {
            self.overflow = true;
            self.body.clear();
            return;
        }
        self.body.extend_from_slice(slice);
    }

    fn finish_body(
        &mut self,
        terminator: Terminator,
        events: &EventQueue,
        out: &mut Vec<ProgramStatus>,
    ) {
        self.lex = Lex::Ground;
        if self.overflow {
            self.overflow = false;
            self.body.clear();
            return;
        }
        let body = std::mem::take(&mut self.body);
        if body == b"?" {
            let reply = match terminator {
                Terminator::Bel => "\x1b]7501;?\x07",
                Terminator::St => "\x1b]7501;?\x1b\\",
            };
            events.send_event(Event::PtyWrite(reply.to_string()));
        } else if let Ok(text) = std::str::from_utf8(&body)
            && let Some(status) = parse_payload(text)
        {
            out.push(status);
        }
        // 复用小缓冲，大缓冲自然释放
        if body.capacity() <= 512 {
            self.body = body;
            self.body.clear();
        }
    }

    fn scan(&mut self, input: &[u8], events: &EventQueue, out: &mut Vec<ProgramStatus>) {
        // 快路径：不在序列中且整批字节不含 ESC，零分配直接返回
        if self.lex == Lex::Ground && memchr(ESC, input).is_none() {
            return;
        }

        let mut i = 0;
        while i < input.len() {
            match self.lex {
                Lex::Ground => match memchr(ESC, &input[i..]) {
                    None => break,
                    Some(at) => {
                        self.lex = Lex::Esc;
                        i += at + 1;
                    }
                },
                Lex::Esc => match input[i] {
                    b']' => {
                        self.lex = Lex::OscPrefix(0);
                        i += 1;
                    }
                    ESC => {
                        // 连续 ESC：保持 Esc 态，消费当前字节
                        i += 1;
                    }
                    _ => {
                        self.lex = Lex::Ground;
                        i += 1;
                    }
                },
                Lex::OscPrefix(matched) => {
                    let b = input[i];
                    if b == PREFIX[matched as usize] {
                        i += 1;
                        let next = matched + 1;
                        if next as usize == PREFIX.len() {
                            self.lex = Lex::Body;
                            self.body.clear();
                            self.overflow = false;
                        } else {
                            self.lex = Lex::OscPrefix(next);
                        }
                    } else if b == ESC {
                        self.lex = Lex::Esc;
                        i += 1;
                    } else {
                        self.lex = Lex::Ground;
                        i += 1;
                    }
                }
                Lex::Body => {
                    let rest = &input[i..];
                    match memchr3(BEL, ESC, CAN, rest) {
                        None => {
                            // 顺带检查 SUB（0x1A）：罕见，只在未命中 BEL/ESC/CAN 时扫一次
                            if let Some(sub_at) = memchr(SUB, rest) {
                                self.reset();
                                i += sub_at + 1;
                            } else {
                                self.push_body(rest);
                                break;
                            }
                        }
                        Some(at) => {
                            let hit = rest[at];
                            // 同一段里在 hit 之前若夹了 SUB，按 VT 规范先被 SUB 打断
                            if let Some(sub_at) = memchr(SUB, &rest[..at]) {
                                self.reset();
                                i += sub_at + 1;
                                continue;
                            }
                            self.push_body(&rest[..at]);
                            i += at + 1;
                            match hit {
                                BEL => self.finish_body(Terminator::Bel, events, out),
                                ESC => self.lex = Lex::BodyEsc,
                                _ => self.reset(), // CAN
                            }
                        }
                    }
                }
                Lex::BodyEsc => match input[i] {
                    b'\\' => {
                        i += 1;
                        self.finish_body(Terminator::St, events, out);
                    }
                    // 正文里的 ESC 后面不是 `\`：当前 OSC 被打断，按新 ESC 后继重判
                    b']' => {
                        self.body.clear();
                        self.overflow = false;
                        self.lex = Lex::OscPrefix(0);
                        i += 1;
                    }
                    ESC => {
                        self.body.clear();
                        self.overflow = false;
                        self.lex = Lex::Esc;
                        i += 1;
                    }
                    _ => {
                        self.reset();
                        i += 1;
                    }
                },
            }
        }
    }
}

/// 挂在 [`crate::TerminalEmulator`] 上的 OSC 7501 状态追踪器。
pub(crate) struct Osc7501 {
    enabled: bool,
    events: EventQueue,
    scanner: Mutex<Scanner>,
    pending: Mutex<Vec<ProgramStatus>>,
    /// 快路径标志：无待取事件时调用方单次原子读即可短路，不抢锁也不分配。
    has_pending: AtomicBool,
}

impl Osc7501 {
    pub(crate) fn new(events: EventQueue) -> Self {
        let enabled = !matches!(std::env::var(DISABLE_ENV).as_deref(), Ok("1"));
        Self::with_enabled(events, enabled)
    }

    pub(crate) fn with_enabled(events: EventQueue, enabled: bool) -> Self {
        Self {
            enabled,
            events,
            scanner: Mutex::new(Scanner::default()),
            pending: Mutex::new(Vec::new()),
            has_pending: AtomicBool::new(false),
        }
    }

    /// 扫描一批 PTY 字节。不改动入参，在 `term` 锁外执行。
    pub(crate) fn scan(&self, bytes: &[u8]) {
        if !self.enabled || bytes.is_empty() {
            return;
        }
        let mut parsed = Vec::new();
        self.scanner.lock().scan(bytes, &self.events, &mut parsed);
        if !parsed.is_empty() {
            let mut pending = self.pending.lock();
            pending.extend(parsed);
            self.has_pending.store(true, Ordering::Release);
        }
    }

    /// 取出已解析的 OSC 7501 状态列表。无新事件时单次原子读直接返回空。
    pub(crate) fn drain(&self) -> Vec<ProgramStatus> {
        if !self.has_pending.load(Ordering::Acquire) {
            return Vec::new();
        }
        let mut pending = self.pending.lock();
        self.has_pending.store(false, Ordering::Release);
        std::mem::take(&mut *pending)
    }

    /// 重置流式状态与未取走的队列（用于 SSH 接管切换边界，避免两头半截序列串台）。
    pub(crate) fn reset(&self) {
        self.scanner.lock().reset();
        let mut pending = self.pending.lock();
        pending.clear();
        self.has_pending.store(false, Ordering::Release);
    }
}

/// 解析 `7501;` 之后的 `key=value:key=value` payload。
///
/// 未知键优雅忽略；缺少合法 `state=` 时返回 `None`。
pub fn parse_payload(payload: &str) -> Option<ProgramStatus> {
    let mut state: Option<ProgramState> = None;
    let mut kind: Option<BlockedKind> = None;
    let mut app: Option<String> = None;
    let mut id: Option<String> = None;
    let mut msg: Option<String> = None;

    for pair in payload.split(':') {
        if pair.is_empty() {
            continue;
        }
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if key.eq_ignore_ascii_case("state") {
            state = parse_state(value);
        } else if key.eq_ignore_ascii_case("kind") {
            kind = Some(parse_blocked_kind(value));
        } else if key.eq_ignore_ascii_case("app") {
            app = Some(truncate_ascii_lower(value, MAX_APP_BYTES));
        } else if key.eq_ignore_ascii_case("id") {
            id = Some(truncate_str(value, MAX_ID_BYTES));
        } else if key.eq_ignore_ascii_case("msg") {
            msg = Some(truncate_str(value, MAX_MSG_BYTES));
        }
    }

    let state = state?;
    Some(ProgramStatus {
        state,
        kind: (state == ProgramState::Blocked).then_some(kind).flatten(),
        app,
        id,
        msg,
    })
}

fn parse_state(raw: &str) -> Option<ProgramState> {
    if raw.eq_ignore_ascii_case("idle") {
        Some(ProgramState::Idle)
    } else if raw.eq_ignore_ascii_case("working") {
        Some(ProgramState::Working)
    } else if raw.eq_ignore_ascii_case("blocked") {
        Some(ProgramState::Blocked)
    } else if raw.eq_ignore_ascii_case("done") {
        Some(ProgramState::Done)
    } else if raw.eq_ignore_ascii_case("error") {
        Some(ProgramState::Error)
    } else if raw.eq_ignore_ascii_case("clear") {
        Some(ProgramState::Clear)
    } else {
        None
    }
}

fn parse_blocked_kind(raw: &str) -> BlockedKind {
    if raw.eq_ignore_ascii_case("permission") {
        BlockedKind::Permission
    } else if raw.eq_ignore_ascii_case("question") {
        BlockedKind::Question
    } else if raw.eq_ignore_ascii_case("auth") {
        BlockedKind::Auth
    } else {
        BlockedKind::Other
    }
}

fn truncate_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn truncate_ascii_lower(s: &str, max_bytes: usize) -> String {
    truncate_str(s, max_bytes).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> (Vec<ProgramStatus>, Vec<String>) {
        let events = EventQueue::default();
        let osc = Osc7501::with_enabled(events.clone(), true);
        for chunk in chunks {
            osc.scan(chunk);
        }
        let statuses = osc.drain();
        let replies = events
            .drain()
            .into_iter()
            .filter_map(|e| match e {
                Event::PtyWrite(s) => Some(s),
                _ => None,
            })
            .collect();
        (statuses, replies)
    }

    #[test]
    fn 实测_agy_全套序列解析准确() {
        let (statuses, replies) = scan_all(&[
            b"\x1b]7501;?\x07",
            b"\x1b]7501;state=idle:app=antigravity\x07",
            b"\x1b]7501;state=working:app=antigravity\x07",
            b"\x1b]7501;state=blocked:kind=permission:app=antigravity:msg=VHJ1c3Q=\x07",
            b"\x1b]7501;state=blocked:kind=question:app=antigravity:msg=UHJvY2VlZD8=\x07",
            b"\x1b]7501;state=done:app=antigravity:msg=ZG9uZQ==\x1b\\",
            b"\x1b]7501;state=error:app=antigravity\x1b\\",
            b"\x1b]7501;state=clear:app=antigravity\x07",
        ]);
        assert_eq!(replies, vec!["\x1b]7501;?\x07"]);
        assert_eq!(
            statuses,
            vec![
                ProgramStatus {
                    state: ProgramState::Idle,
                    kind: None,
                    app: Some("antigravity".into()),
                    id: None,
                    msg: None,
                },
                ProgramStatus {
                    state: ProgramState::Working,
                    kind: None,
                    app: Some("antigravity".into()),
                    id: None,
                    msg: None,
                },
                ProgramStatus {
                    state: ProgramState::Blocked,
                    kind: Some(BlockedKind::Permission),
                    app: Some("antigravity".into()),
                    id: None,
                    msg: Some("VHJ1c3Q=".into()),
                },
                ProgramStatus {
                    state: ProgramState::Blocked,
                    kind: Some(BlockedKind::Question),
                    app: Some("antigravity".into()),
                    id: None,
                    msg: Some("UHJvY2VlZD8=".into()),
                },
                ProgramStatus {
                    state: ProgramState::Done,
                    kind: None,
                    app: Some("antigravity".into()),
                    id: None,
                    msg: Some("ZG9uZQ==".into()),
                },
                ProgramStatus {
                    state: ProgramState::Error,
                    kind: None,
                    app: Some("antigravity".into()),
                    id: None,
                    msg: None,
                },
                ProgramStatus {
                    state: ProgramState::Clear,
                    kind: None,
                    app: Some("antigravity".into()),
                    id: None,
                    msg: None,
                },
            ]
        );
    }

    #[test]
    fn 探测查询按原终止符应答() {
        let (statuses, replies) = scan_all(&[b"\x1b]7501;?\x07", b"\x1b]7501;?\x1b\\"]);
        assert!(statuses.is_empty());
        assert_eq!(replies, vec!["\x1b]7501;?\x07", "\x1b]7501;?\x1b\\"]);
    }

    #[test]
    fn 逐字节喂入也能跨_chunk_完整拼回() {
        let stream =
            b"hello\x1b]7501;?\x1b\\mid\x1b]7501;state=blocked:kind=question:app=antigravity:id=sub/1:msg=abc\x07tail";
        let events = EventQueue::default();
        let osc = Osc7501::with_enabled(events.clone(), true);
        for &byte in stream {
            osc.scan(&[byte]);
        }
        let statuses = osc.drain();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, ProgramState::Blocked);
        assert_eq!(statuses[0].kind, Some(BlockedKind::Question));
        assert_eq!(statuses[0].app.as_deref(), Some("antigravity"));
        assert_eq!(statuses[0].id.as_deref(), Some("sub/1"));
        assert_eq!(statuses[0].msg.as_deref(), Some("abc"));

        let replies: Vec<_> = events
            .drain()
            .into_iter()
            .filter_map(|e| match e {
                Event::PtyWrite(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(replies, vec!["\x1b]7501;?\x1b\\"]);
    }

    #[test]
    fn 中断与超长序列安全丢弃且不影响后续正常序列() {
        let mut long_seq = b"\x1b]7501;state=working:msg=".to_vec();
        long_seq.extend(std::iter::repeat_n(b'A', MAX_PAYLOAD_BYTES + 128));
        long_seq.push(BEL);

        let (statuses, _) = scan_all(&[
            // CAN / SUB 打断未闭合序列
            b"\x1b]7501;state=working\x18",
            b"\x1b]7501;state=working\x1a",
            // 正文里紧接一条新的 OSC 7501（旧序列被新 ESC ] 打断）
            b"\x1b]7501;state=broken\x1b]7501;state=working:app=antigravity\x07",
            // 超长序列丢弃
            &long_seq,
            // 未知 state / 无 state 忽略
            b"\x1b]7501;state=unknown:app=antigravity\x07",
            b"\x1b]7501;app=antigravity\x07",
            // 别的 OSC 编号忽略
            b"\x1b]0;window title\x07",
            b"\x1b]75010;state=working\x07",
            // 后续正常序列仍可识别
            b"\x1b]7501;state=done:app=antigravity:progress=100\x07",
        ]);
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].state, ProgramState::Working);
        assert_eq!(statuses[1].state, ProgramState::Done);
    }

    #[test]
    fn kind_只在_blocked_时保留且支持_auth_与未知值() {
        assert_eq!(
            parse_payload("state=working:kind=permission").unwrap().kind,
            None
        );
        assert_eq!(
            parse_payload("state=blocked:kind=auth").unwrap().kind,
            Some(BlockedKind::Auth)
        );
        assert_eq!(
            parse_payload("state=blocked:kind=custom").unwrap().kind,
            Some(BlockedKind::Other)
        );
    }

    #[test]
    fn 禁用开关彻底关闭探测应答与状态产出() {
        let events = EventQueue::default();
        let osc = Osc7501::with_enabled(events.clone(), false);
        osc.scan(b"\x1b]7501;?\x07\x1b]7501;state=blocked:kind=permission:app=antigravity\x07");
        assert!(osc.drain().is_empty());
        assert!(events.drain().is_empty());
    }

    #[test]
    fn reset_清掉半截序列与未取出的状态() {
        let events = EventQueue::default();
        let osc = Osc7501::with_enabled(events, true);
        osc.scan(b"\x1b]7501;state=working:app=antigravity\x07\x1b]7501;state=blo");
        osc.reset();
        assert!(osc.drain().is_empty());
        osc.scan(b"cked:kind=permission:app=antigravity\x07");
        assert!(osc.drain().is_empty(), "reset 后旧半截序列不得续接");
    }
}
