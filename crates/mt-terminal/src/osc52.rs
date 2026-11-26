//! OSC 52 剪贴板协议的 DCS 透传解包器（tmux / GNU screen）与安全守卫。
//!
//! # 为什么做成精准摘除而不是只读观察
//!
//! 标准的裸 `ESC ] 52 ; <selection> ; <data> (BEL | ST)` 由 `alacritty_terminal`
//! 原生解析并派发 `Event::ClipboardStore` / `Event::ClipboardLoad`。
//! 但当程序跑在远端 `tmux`（`allow-passthrough on`）或 GNU `screen` 里时，外层终端
//! 收到的是包在 DCS 透传里的序列：
//!
//! - **tmux**：`ESC P tmux; ESC ESC ] 52 ; <selection> ; <data> (BEL | ESC ESC \) ESC \`
//! - **GNU screen**：`ESC P ESC ] 52 ; <selection> ; <data> (BEL | ESC \) ESC \`
//!
//! `vte` 状态机在 `DcsPassthrough` 中见到任意 `ESC` 就会立刻跳出 DCS 回到 `Escape`：
//! 一方面会把 BEL 结尾的 tmux 透传误当裸 OSC 52 再解析一遍（造成重复派发或绕过空串校验），
//! 另一方面会把 ST 结尾（`ESC ESC \`）的 OSC 52 提前取消而完全丢失。
//!
//! 因此本模块在 `term` 锁外采用与 [`crate::kitty`] 相同的流式过滤器形态：
//! - **快路径**：不在前缀/正文状态且本批字节不含 `ESC` 时直接返回 `Cow::Borrowed(bytes)`，
//!   零分配、零拷贝；
//! - **精准匹配**：仅当精确匹配 `ESC P tmux; ESC ESC ] 52 ;` 或 `ESC P ESC ] 52 ;` 时，
//!   才将该条 DCS 透传从送往 `vte` 的字节流中摘除并由本模块独自解包、校验、派发唯一一次事件；
//! - **非 OSC 52 原样透传**：前缀失配时把已暂存的前缀字节原样吐回输出流，不伤及其他 DCS
//!   或 tmux 透传（如 OSC 7 / 光标样式等）。
//!
//! # 安全边界
//!
//! - **有界缓冲**：单条 DCS 透传上限 [`MAX_DCS_BYTES`]，解码后文本上限
//!   [`MAX_CLIPBOARD_BYTES`]（4 MiB），超限即丢弃并吞到外层 ST；
//! - **拒绝空串清空与非法载荷**：`data` 为空、非法 Base64 或非合法 UTF-8 一律忽略；
//! - **读取应答走标准 OSC 52**：`?` 查询回发标准 `ESC ] 52 ; <sel> ; <base64> (BEL | ST)`，
//!   是否允许读取由宿主配置闸（默认关闭）统一把关。

use std::borrow::Cow;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::ClipboardType;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use memchr::{memchr, memchr3};
use parking_lot::Mutex;

use crate::EventQueue;

/// 单次 OSC 52 写入/读取允许的最大 UTF-8 剪贴板文本字节数（4 MiB）。
pub const MAX_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;

/// 单条 DCS 透传载荷字节上限（略大于 4 MiB 的 base64 编码 + 头部开销）。
const MAX_DCS_BYTES: usize = 6 * 1024 * 1024;

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1A;

/// tmux 透传 OSC 52 的精确前缀（不含开头 `ESC P`）。
const TMUX_OSC52_HEAD: &[u8] = b"tmux;\x1b\x1b]52;";
/// GNU screen 透传 OSC 52 的精确前缀（不含开头 `ESC P`）。
const SCREEN_OSC52_HEAD: &[u8] = b"\x1b]52;";

/// 校验一段 OSC 52 写入文本是否允许落入本机剪贴板：
/// 拒绝空串（防远端误清空本机剪贴板）与超大载荷（防卡死系统剪贴板）。
pub fn allow_clipboard_store(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES
}

/// 校验准备经 OSC 52 读取应答回发给 PTY 的本机剪贴板文本是否在安全大小内。
pub fn allow_clipboard_load(text: &str) -> bool {
    text.len() <= MAX_CLIPBOARD_BYTES
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminator {
    Bel,
    St,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lex {
    /// 普通字节流，透传给 `vte`。
    Ground,
    /// 已暂存 `ESC`，等待判别下一字节是否为 `P`。
    Esc,
    /// 已暂存 `ESC P`，等待判别是否为 `t`（tmux）或 `ESC`（screen）。
    DcsHead,
    /// 正在匹配 `TMUX_OSC52_HEAD`（已匹配长度 1..TMUX_OSC52_HEAD.len()）。
    TmuxHead(u8),
    /// 已确认进入 tmux 透传 OSC 52 正文（`<selection>;<data> (BEL|ESC ESC \) ESC \`）。
    TmuxBody,
    /// tmux 正文内刚见到一个 `ESC`。
    TmuxEsc,
    /// 正在匹配 `SCREEN_OSC52_HEAD`（已匹配长度 1..SCREEN_OSC52_HEAD.len()）。
    ScreenHead(u8),
    /// 已确认进入 GNU screen 透传 OSC 52 正文（`<selection>;<data> (BEL|ESC \) ESC \`）。
    ScreenBody,
    /// screen 正文内刚见到一个 `ESC`。
    ScreenEsc,
    /// screen 内层已收尾（`BEL`），等待外层 DCS 的 `ESC \`。
    ScreenWaitOuterSt,
    /// 在 `ScreenWaitOuterSt` 里刚见到 `ESC`。
    ScreenWaitOuterStEsc,
    /// screen 内层已用 `ESC \` 收尾：如果紧跟的下一字节是 `ESC` 再加 `\`，吞掉这枚外层 ST；
    /// 否则立即把该字节当普通字节放行（兼容共用单个 ST 的旧写法）。
    ScreenMaybeOuterSt,
    /// 在 `ScreenMaybeOuterSt` 里见到了 `ESC`，等看下一字节是不是 `\`。
    ScreenMaybeOuterStEsc,
}

struct FilterState {
    lex: Lex,
    /// 尚未确定是不是目标 DCS 前缀时暂存的原始字节（最多 13 字节）；失配时原样吐给输出。
    staged: Vec<u8>,
    /// 已确认进入目标 DCS 透传后收集的 `<selection>;<data>` 字节（tmux 含内层终止符）。
    body: Vec<u8>,
    overflow: bool,
    screen_inner_term: Option<Terminator>,
}

impl Default for FilterState {
    fn default() -> Self {
        Self {
            lex: Lex::Ground,
            staged: Vec::new(),
            body: Vec::new(),
            overflow: false,
            screen_inner_term: None,
        }
    }
}

impl FilterState {
    fn reset(&mut self) {
        self.lex = Lex::Ground;
        self.staged.clear();
        self.body.clear();
        self.overflow = false;
        self.screen_inner_term = None;
        if self.body.capacity() > 4096 {
            self.body = Vec::new();
        }
    }

    fn push_body_byte(&mut self, b: u8) {
        if self.overflow {
            return;
        }
        if self.body.len() >= MAX_DCS_BYTES {
            self.overflow = true;
            self.body.clear();
            return;
        }
        self.body.push(b);
    }

    fn push_body_slice(&mut self, slice: &[u8]) {
        if self.overflow || slice.is_empty() {
            return;
        }
        if self.body.len().saturating_add(slice.len()) > MAX_DCS_BYTES {
            self.overflow = true;
            self.body.clear();
            return;
        }
        self.body.extend_from_slice(slice);
    }

    fn flush_staged_to(&mut self, out: &mut Vec<u8>) {
        if !self.staged.is_empty() {
            out.extend_from_slice(&self.staged);
            self.staged.clear();
        }
    }

    fn finish_tmux(&mut self, events: &EventQueue) {
        let overflow = self.overflow;
        let payload = std::mem::take(&mut self.body);
        self.reset();
        if overflow {
            return;
        }
        let (body, term) = if let Some(b) = payload.strip_suffix(&[BEL]) {
            (b, Terminator::Bel)
        } else if let Some(b) = payload.strip_suffix(b"\x1b\\") {
            (b, Terminator::St)
        } else {
            return;
        };
        if let Some(event) = parse_osc52_body(body, term) {
            events.send_event(event);
        }
    }

    fn finish_screen(&mut self, events: &EventQueue, next_lex: Lex) {
        let overflow = self.overflow;
        let term = self.screen_inner_term.unwrap_or(Terminator::St);
        let body = std::mem::take(&mut self.body);
        self.staged.clear();
        self.overflow = false;
        self.screen_inner_term = None;
        self.lex = next_lex;
        if self.body.capacity() > 4096 {
            self.body = Vec::new();
        }
        if overflow {
            return;
        }
        if let Some(event) = parse_osc52_body(&body, term) {
            events.send_event(event);
        }
    }

    fn filter<'a>(&mut self, input: &'a [u8], events: &EventQueue) -> Cow<'a, [u8]> {
        if input.is_empty() {
            return Cow::Borrowed(input);
        }
        // 快路径：当前在 Ground 且整批不含 ESC，零分配原样借出
        if self.lex == Lex::Ground && memchr(ESC, input).is_none() {
            return Cow::Borrowed(input);
        }

        // 若尚在 Ground 且整批只含普通 ESC 序列（没有 `ESC P`），也可以检查一下是否真的
        // 需要分配；不过含 ESC 批本身就小且要进 VTE，直接按容量分配一份输出缓冲已足够快。
        let mut out: Option<Vec<u8>> = None;
        // Ground 态下尚未拷进 `out` 的原切片起点
        let mut clean_start = 0usize;
        let mut i = 0usize;

        fn ensure_out<'b>(
            out: &'b mut Option<Vec<u8>>,
            input: &[u8],
            clean_start: usize,
            upto: usize,
        ) -> &'b mut Vec<u8> {
            out.get_or_insert_with(|| {
                let mut v = Vec::with_capacity(input.len());
                if clean_start < upto {
                    v.extend_from_slice(&input[clean_start..upto]);
                }
                v
            })
        }

        // 如果进函数时已经不在 Ground（上一批末尾留了暂存前缀或正处于 DCS 内），必须构建新输出
        if self.lex != Lex::Ground {
            out = Some(Vec::with_capacity(input.len()));
        }

        while i < input.len() {
            match self.lex {
                Lex::Ground => match memchr(ESC, &input[i..]) {
                    None => {
                        if let Some(buf) = out.as_mut() {
                            buf.extend_from_slice(&input[i..]);
                        }
                        break;
                    }
                    Some(at) => {
                        let esc_pos = i + at;
                        if let Some(buf) = out.as_mut() {
                            buf.extend_from_slice(&input[i..esc_pos]);
                        }
                        // 如果 ESC 后面紧跟的字节已在本批内且不是 'P'，直接透传不进暂存，
                        // 让绝大多数普通 ANSI 转义序列（CSI/OSC）连分配都省掉！
                        if esc_pos + 1 < input.len() && input[esc_pos + 1] != b'P' {
                            if let Some(buf) = out.as_mut() {
                                buf.push(ESC);
                            }
                            i = esc_pos + 1;
                            continue;
                        }
                        // 到了这里要么是 `ESC P`，要么是批次最后一个字节为 `ESC`
                        let _ = ensure_out(&mut out, input, clean_start, esc_pos);
                        clean_start = input.len();
                        self.staged.clear();
                        self.staged.push(ESC);
                        self.lex = Lex::Esc;
                        i = esc_pos + 1;
                    }
                },
                Lex::Esc => {
                    let b = input[i];
                    if b == b'P' {
                        self.staged.push(b);
                        self.lex = Lex::DcsHead;
                        i += 1;
                    } else if b == ESC {
                        // 连续 ESC：前一个 ESC 放行，当前 ESC 继续留在 Esc 态
                        let buf = ensure_out(&mut out, input, clean_start, i);
                        self.flush_staged_to(buf);
                        self.staged.push(ESC);
                        i += 1;
                    } else {
                        let buf = ensure_out(&mut out, input, clean_start, i);
                        self.flush_staged_to(buf);
                        buf.push(b);
                        self.lex = Lex::Ground;
                        i += 1;
                    }
                }
                Lex::DcsHead => {
                    let b = input[i];
                    if b == TMUX_OSC52_HEAD[0] {
                        self.staged.push(b);
                        self.lex = Lex::TmuxHead(1);
                        i += 1;
                    } else if b == SCREEN_OSC52_HEAD[0] {
                        self.staged.push(b);
                        self.lex = Lex::ScreenHead(1);
                        i += 1;
                    } else {
                        let buf = ensure_out(&mut out, input, clean_start, i);
                        self.flush_staged_to(buf);
                        buf.push(b);
                        self.lex = Lex::Ground;
                        i += 1;
                    }
                }
                Lex::TmuxHead(matched) => {
                    let b = input[i];
                    if b == TMUX_OSC52_HEAD[matched as usize] {
                        self.staged.push(b);
                        i += 1;
                        let next = matched + 1;
                        if next as usize == TMUX_OSC52_HEAD.len() {
                            // 完整命中 `ESC P tmux; ESC ESC ] 52 ;`：丢弃暂存前缀，开始收集正文
                            self.staged.clear();
                            self.body.clear();
                            self.overflow = false;
                            self.lex = Lex::TmuxBody;
                        } else {
                            self.lex = Lex::TmuxHead(next);
                        }
                    } else {
                        let buf = ensure_out(&mut out, input, clean_start, i);
                        self.flush_staged_to(buf);
                        // 当前字节未消费，回到 Ground 重新处理（它可能是新的 ESC）
                        self.lex = Lex::Ground;
                    }
                }
                Lex::TmuxBody => {
                    let rest = &input[i..];
                    match memchr3(ESC, CAN, SUB, rest) {
                        None => {
                            self.push_body_slice(rest);
                            break;
                        }
                        Some(at) => {
                            let hit = rest[at];
                            self.push_body_slice(&rest[..at]);
                            i += at + 1;
                            if hit == ESC {
                                self.lex = Lex::TmuxEsc;
                            } else {
                                self.reset();
                            }
                        }
                    }
                }
                Lex::TmuxEsc => match input[i] {
                    ESC => {
                        self.push_body_byte(ESC);
                        self.lex = Lex::TmuxBody;
                        i += 1;
                    }
                    b'\\' => {
                        i += 1;
                        self.finish_tmux(events);
                    }
                    CAN | SUB => {
                        self.reset();
                        i += 1;
                    }
                    _ => {
                        // 畸形未双写的 ESC：中止本次透传，回到 Ground 放行后续正常流
                        self.reset();
                    }
                },
                Lex::ScreenHead(matched) => {
                    let b = input[i];
                    if b == SCREEN_OSC52_HEAD[matched as usize] {
                        self.staged.push(b);
                        i += 1;
                        let next = matched + 1;
                        if next as usize == SCREEN_OSC52_HEAD.len() {
                            self.staged.clear();
                            self.body.clear();
                            self.overflow = false;
                            self.screen_inner_term = None;
                            self.lex = Lex::ScreenBody;
                        } else {
                            self.lex = Lex::ScreenHead(next);
                        }
                    } else {
                        let buf = ensure_out(&mut out, input, clean_start, i);
                        self.flush_staged_to(buf);
                        self.lex = Lex::Ground;
                    }
                }
                Lex::ScreenBody => {
                    let rest = &input[i..];
                    match memchr3(BEL, ESC, CAN, rest) {
                        None => {
                            if let Some(sub_at) = memchr(SUB, rest) {
                                self.reset();
                                i += sub_at + 1;
                            } else {
                                self.push_body_slice(rest);
                                break;
                            }
                        }
                        Some(at) => {
                            let hit = rest[at];
                            if let Some(sub_at) = memchr(SUB, &rest[..at]) {
                                self.reset();
                                i += sub_at + 1;
                                continue;
                            }
                            self.push_body_slice(&rest[..at]);
                            i += at + 1;
                            match hit {
                                BEL => {
                                    self.screen_inner_term = Some(Terminator::Bel);
                                    self.lex = Lex::ScreenWaitOuterSt;
                                }
                                ESC => {
                                    self.lex = Lex::ScreenEsc;
                                }
                                _ => self.reset(),
                            }
                        }
                    }
                }
                Lex::ScreenEsc => match input[i] {
                    b'\\' => {
                        self.screen_inner_term = Some(Terminator::St);
                        i += 1;
                        self.finish_screen(events, Lex::ScreenMaybeOuterSt);
                    }
                    CAN | SUB => {
                        self.reset();
                        i += 1;
                    }
                    _ => {
                        self.reset();
                    }
                },
                Lex::ScreenWaitOuterSt => match input[i] {
                    ESC => {
                        self.lex = Lex::ScreenWaitOuterStEsc;
                        i += 1;
                    }
                    CAN | SUB => {
                        self.reset();
                        i += 1;
                    }
                    b' ' | b'\r' | b'\n' => {
                        i += 1;
                    }
                    _ => {
                        self.reset();
                    }
                },
                Lex::ScreenWaitOuterStEsc => match input[i] {
                    b'\\' => {
                        i += 1;
                        self.finish_screen(events, Lex::Ground);
                    }
                    CAN | SUB => {
                        self.reset();
                        i += 1;
                    }
                    _ => {
                        self.reset();
                    }
                },
                Lex::ScreenMaybeOuterSt => {
                    if input[i] == ESC {
                        self.staged.clear();
                        self.staged.push(ESC);
                        self.lex = Lex::ScreenMaybeOuterStEsc;
                        i += 1;
                    } else {
                        self.lex = Lex::Ground;
                    }
                }
                Lex::ScreenMaybeOuterStEsc => {
                    if input[i] == b'\\' {
                        // 吃掉紧随内层 ST 的外层 DCS 终止符 `ESC \`
                        self.staged.clear();
                        self.lex = Lex::Ground;
                        i += 1;
                    } else {
                        // 不是外层 ST：把暂存的 ESC 交给正常 Esc 分支继续判别
                        self.lex = Lex::Esc;
                    }
                }
            }
        }

        match out {
            None => Cow::Borrowed(input),
            Some(buf) => Cow::Owned(buf),
        }
    }
}

/// 解析 `52;` 之后的 `<selection>;<data>` 主体。
fn parse_osc52_body(body: &[u8], term: Terminator) -> Option<Event> {
    let semi = memchr(b';', body)?;
    let sel_bytes = &body[..semi];
    let data_bytes = &body[semi + 1..];

    // selection 只允许标准字符 `[cps0-7]`（可为空），杜绝向 PTY 应答里注入控制字符
    if sel_bytes.len() > 16
        || !sel_bytes
            .iter()
            .all(|b| matches!(b, b'c' | b'p' | b's' | b'0'..=b'7'))
    {
        return None;
    }
    let selection = std::str::from_utf8(sel_bytes).ok()?;
    let clipboard_type = if sel_bytes.contains(&b'p') && !sel_bytes.contains(&b'c') {
        ClipboardType::Selection
    } else {
        ClipboardType::Clipboard
    };

    if data_bytes == b"?" {
        let reply_sel = if selection.is_empty() {
            "c".to_string()
        } else {
            selection.to_string()
        };
        let suffix = match term {
            Terminator::Bel => "\x07",
            Terminator::St => "\x1b\\",
        };
        let formatter: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(move |text| {
            format!("\x1b]52;{reply_sel};{}{suffix}", STANDARD.encode(text))
        });
        return Some(Event::ClipboardLoad(clipboard_type, formatter));
    }

    let text = decode_osc52_base64(data_bytes)?;
    if !allow_clipboard_store(&text) {
        return None;
    }
    Some(Event::ClipboardStore(clipboard_type, text))
}

/// 严格解码 OSC 52 的 Base64 文本载荷（忽略常见 ASCII 空白/换行，兼容无 `=` 填充）。
fn decode_osc52_base64(raw: &[u8]) -> Option<String> {
    let decoded = if raw.iter().any(|b| b.is_ascii_whitespace()) {
        let compact: Vec<u8> = raw
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        if compact.is_empty() {
            return None;
        }
        decode_base64_bytes(&compact)?
    } else {
        if raw.is_empty() {
            return None;
        }
        decode_base64_bytes(raw)?
    };
    if decoded.len() > MAX_CLIPBOARD_BYTES {
        return None;
    }
    String::from_utf8(decoded).ok()
}

fn decode_base64_bytes(compact: &[u8]) -> Option<Vec<u8>> {
    STANDARD
        .decode(compact)
        .or_else(|_| {
            if !compact.contains(&b'=') {
                STANDARD_NO_PAD.decode(compact)
            } else {
                Err(base64::DecodeError::InvalidPadding)
            }
        })
        .ok()
}

/// 挂在 [`crate::TerminalEmulator`] 上的 OSC 52 DCS 透传过滤器。
pub(crate) struct Osc52Passthrough {
    events: EventQueue,
    state: Mutex<FilterState>,
}

impl Osc52Passthrough {
    pub(crate) fn new(events: EventQueue) -> Self {
        Self {
            events,
            state: Mutex::new(FilterState::default()),
        }
    }

    /// 过滤一批字节：把 tmux / screen 的 OSC 52 DCS 透传摘除并派发唯一一次剪贴板事件，
    /// 其余字节（含普通裸 OSC 52 与其他 DCS）原样返回给 `vte`。
    pub(crate) fn filter<'a>(&self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        self.state.lock().filter(bytes, &self.events)
    }

    pub(crate) fn reset(&self) {
        self.state.lock().reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TermSize, TerminalEmulator};

    #[test]
    fn tmux_passthrough_writes_clipboard_with_bel_and_st() {
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        let b64 = STANDARD.encode("hello from tmux 你好");

        // BEL 结尾形态：ESC P tmux; ESC ESC ] 52;c;<b64> BEL ESC \
        let seq_bel = format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\");
        emu.advance(seq_bel.as_bytes());
        let events = emu.events().drain();
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                assert_eq!(text, "hello from tmux 你好");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // ST 结尾形态：ESC P tmux; ESC ESC ] 52;c;<b64> ESC ESC \ ESC \
        let b64_2 = STANDARD.encode("second st payload");
        let seq_st = format!("\x1bPtmux;\x1b\x1b]52;c;{b64_2}\x1b\x1b\\\x1b\\");
        emu.advance(seq_st.as_bytes());
        let events = emu.events().drain();
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                assert_eq!(text, "second st payload");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn tmux_passthrough_handles_chunked_writes_without_screen_pollution() {
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        let b64 = STANDARD.encode("chunked copy");
        let full = format!("before\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\after");
        for chunk in full.as_bytes().chunks(3) {
            emu.advance(chunk);
        }
        assert_eq!(emu.visible_lines()[0], "beforeafter");
        let events = emu.events().drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            Event::ClipboardStore(ClipboardType::Clipboard, t) if t == "chunked copy"
        ));
    }

    #[test]
    fn tmux_passthrough_clipboard_load_replies_standard_osc52() {
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        emu.advance(b"\x1bPtmux;\x1b\x1b]52;c;?\x07\x1b\\");
        let events = emu.events().drain();
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ClipboardLoad(ClipboardType::Clipboard, format_fn) => {
                let expected = format!("\x1b]52;c;{}\x07", STANDARD.encode("secret"));
                assert_eq!(format_fn("secret"), expected);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn screen_passthrough_supports_bel_and_st() {
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        let b64 = STANDARD.encode("from screen");
        // BEL + 外层 ST
        let seq1 = format!("L1\x1bP\x1b]52;c;{b64}\x07\x1b\\R1");
        emu.advance(seq1.as_bytes());
        assert_eq!(emu.visible_lines()[0], "L1R1");
        let events = emu.events().drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            Event::ClipboardStore(ClipboardType::Clipboard, t) if t == "from screen"
        ));

        // ST + 外层 ST
        let seq2 = format!("\r\nL2\x1bP\x1b]52;p;{b64}\x1b\\\x1b\\R2");
        emu.advance(seq2.as_bytes());
        assert_eq!(emu.visible_lines()[1], "L2R2");
        let events = emu.events().drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            Event::ClipboardStore(ClipboardType::Selection, t) if t == "from screen"
        ));
    }

    #[test]
    fn bare_osc52_is_not_duplicated() {
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        let b64 = STANDARD.encode("plain osc52");
        emu.advance(format!("\x1b]52;c;{b64}\x07").as_bytes());
        let events = emu.events().drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            Event::ClipboardStore(ClipboardType::Clipboard, t) if t == "plain osc52"
        ));
    }

    #[test]
    fn non_osc52_tmux_passthrough_still_passes_through_intact() {
        // 比如 tmux 透传的 OSC 0 窗口标题，前缀在 `52;` 处失配，必须原样放行不丢字节
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        emu.advance(b"\x1bPtmux;\x1b\x1b]0;my-title\x07\x1b\\ok");
        assert_eq!(emu.visible_lines()[0], "ok");
        let events = emu.events().drain();
        assert!(events.iter().any(|e| matches!(e, Event::Title(t) if t == "my-title")));
    }

    #[test]
    fn empty_and_invalid_payloads_are_ignored() {
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        // 空 payload（清空剪贴板）
        emu.advance(b"\x1bPtmux;\x1b\x1b]52;c;\x07\x1b\\");
        // 非法 base64
        emu.advance(b"\x1bPtmux;\x1b\x1b]52;c;!!!not-b64!!!\x07\x1b\\");
        // 非法 selection（含转义后的 ESC 注入字符）
        emu.advance(b"\x1bPtmux;\x1b\x1b]52;c\x1b\x1b;?\x07\x1b\\");
        // CAN 中止的半截序列
        emu.advance(b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x18");
        assert!(emu.events().drain().is_empty());
        assert!(!allow_clipboard_store(""));
        assert!(allow_clipboard_store("ok"));
        assert!(allow_clipboard_load(""));
    }

    #[test]
    fn unpadded_base64_and_whitespace_are_accepted() {
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        emu.advance(b"\x1bPtmux;\x1b\x1b]52;c;aG\nk\x07\x1b\\");
        let events = emu.events().drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            Event::ClipboardStore(ClipboardType::Clipboard, t) if t == "hi"
        ));
    }

    /// 取出唯一一条 `ClipboardLoad` 并用它的格式化器回出应答串。
    fn load_reply(emu: &TerminalEmulator, query: &[u8]) -> Option<String> {
        emu.advance(query);
        emu.events().drain().into_iter().find_map(|e| match e {
            Event::ClipboardLoad(ClipboardType::Clipboard, format_fn) => Some(format_fn("secret")),
            _ => None,
        })
    }

    /// 回归(E2E 发现):裸 OSC 52 查询由 alacritty 原生解析,它默认的 `Osc52::OnlyCopy`
    /// 会把查询整条吞掉、不发 `ClipboardLoad`,宿主 `osc52Read` 开了也读不到。
    #[test]
    fn bare_osc52_query_emits_clipboard_load_with_matching_terminator() {
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        let b64 = STANDARD.encode("secret");
        assert_eq!(
            load_reply(&emu, b"\x1b]52;c;?\x07").as_deref(),
            Some(format!("\x1b]52;c;{b64}\x07").as_str())
        );
        assert_eq!(
            load_reply(&emu, b"\x1b]52;c;?\x1b\\").as_deref(),
            Some(format!("\x1b]52;c;{b64}\x1b\\").as_str())
        );
    }

    /// `set_options` 是整份替换:改回滚行数、备用屏清回看都不能把 OSC 52 读取退回默认的禁用。
    #[test]
    fn bare_osc52_query_survives_set_options_paths() {
        let emu = TerminalEmulator::new(TermSize::new(80, 24));
        emu.set_scrollback(500);
        assert!(load_reply(&emu, b"\x1b]52;c;?\x07").is_some(), "set_scrollback 之后");

        emu.advance(b"\x1b[?1049h");
        emu.clear_scrollback();
        assert!(load_reply(&emu, b"\x1b]52;c;?\x07").is_some(), "备用屏 clear_scrollback 之后");
    }
}
