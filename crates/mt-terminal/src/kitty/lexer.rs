//! 流式预解析:从 PTY 字节里摘出 `ESC _ G … ESC \`,换成一条带内标记,其余原样放行。
//!
//! 跑在 reader 线程上、term 锁之外,所以 PNG 解码也在这里。
//!
//! # 为什么词法上找 ESC 就够了
//!
//! vte 在任何状态下见到 ESC 都是开始一条新序列(CSI / OSC / DCS / 别的 APC 都被它打断,
//! 见 `vte::Parser::anywhere` 与 `advance_osc_string`),这里只认「ESC 开头」不会与它
//! 对同一段字节有两种理解。CAN / SUB 同样按 vte 的口径打断正文。
//!
//! # 什么时候不碰字节
//!
//! 这批字节里没有要摘的东西时,返回的就是入参的切片(或它去掉结尾半条序列的前缀),
//! **不复制**。只有真的摘掉了东西、或者要补回上一批扣下的字节,才写进 `out`。
//!
//! # 跨批次
//!
//! - 批尾是 `ESC` 或 `ESC _`:不知道是不是图形序列,先扣下,等下一批的第一个字节。
//!   单独一个 ESC 进 vte 也只是改状态,扣下没有可见差别;
//! - `CSI 16 t`:只看不摘(vte 认不得,放进去也只是打一行日志),跨批次靠 [`Lex::Csi`]
//!   记着匹配到第几个字节。

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use memchr::{memchr, memchr3};

use super::KittyShared;
use super::command::{Control, parse_control, split_body};
use super::decode::decode_png;
use super::{Pending, PlaceSpec, ReplyTo, target_bitmap_size};

const ESC: u8 = 0x1B;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1A;

/// 要应答的那条查询:`CSI 16 t`(一格多少像素)。
const CSI_16T: &[u8] = b"\x1b[16t";

/// 未完成分块的编码数据上限(base64 解开之后)。
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

/// 单条 APC 正文的上限:base64 把 [`MAX_UPLOAD_BYTES`] 撑大 4/3,再留点控制段的余量。
const MAX_BODY_BYTES: usize = MAX_UPLOAD_BYTES / 3 * 4 + 4096;

/// 未完成的传输多久没有新分块就作废。
const UPLOAD_IDLE_LIMIT: Duration = Duration::from_secs(30);

/// 正文缓冲留多大的容量。超过就放掉,免得一张不分块的大图把几十 MB 容量一直占着。
const BODY_KEEP_CAPACITY: usize = 64 * 1024;

/// payload 的 base64 解码器。规范说最后一块才带填充,这里两种都收。
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lex {
    /// 正文之外。
    Ground,
    /// 刚过一个 ESC。
    Esc,
    /// 刚过 `ESC _`,要看下一个字节是不是 `G`。
    EscApc,
    /// `CSI 16 t` 匹配到第几个字节了(已匹配的长度)。
    Csi(u8),
    /// 图形序列的正文。
    Body,
    /// 正文里刚过一个 ESC:后面是 `\` 就结束,否则这条序列作废。
    BodyEsc,
}

/// 一次传输:可能是一块,也可能是很多块。
struct Upload {
    control: Control,
    data: Vec<u8>,
    /// 已经判了不收,后续分块只吞不存。文字是应答里的错误。
    rejected: Option<&'static str>,
    last_chunk: Instant,
}

pub(crate) struct PreParser {
    shared: Arc<KittyShared>,
    lex: Lex,
    /// 上一批结尾扣下了几个字节(1 = `ESC`,2 = `ESC _`)。
    held: u8,
    body: Vec<u8>,
    /// 这条正文超限了:只吞不存,结束时作废。
    body_overflow: bool,
    body_activity: Instant,
    upload: Option<Upload>,
    out: Vec<u8>,
}

/// 这一批字节的出口。没摘东西时只挪两个下标,不碰 `out`。
struct Sink<'a> {
    input: &'a [u8],
    out: &'a mut Vec<u8>,
    /// `input[start..]` 还没交代。
    start: usize,
    /// 结果已经转到 `out` 上了。
    rewritten: bool,
}

impl Sink<'_> {
    /// 放行 `input[start..end]`,结果转到 `out` 上。
    fn commit_to(&mut self, end: usize) {
        self.rewritten = true;
        self.out.extend_from_slice(&self.input[self.start..end]);
        self.start = end;
    }

    /// 丢掉 `input[from..to]`。
    fn skip(&mut self, from: usize, to: usize) {
        if from > self.start || self.rewritten {
            self.commit_to(from);
        }
        self.start = to;
    }

    /// 在 `at` 处塞进一段 `input` 里没有的字节。
    fn inject(&mut self, at: usize, bytes: &[u8]) {
        self.commit_to(at);
        self.out.extend_from_slice(bytes);
    }
}

impl PreParser {
    pub(super) fn new(shared: Arc<KittyShared>) -> Self {
        Self {
            shared,
            lex: Lex::Ground,
            held: 0,
            body: Vec::new(),
            body_overflow: false,
            body_activity: Instant::now(),
            upload: None,
            out: Vec::new(),
        }
    }

    /// 把一批 PTY 字节过一遍,返回该交给 vte 的部分。图形序列在这里解码、入账,留下
    /// 一条带内标记等 vte 按序走到它。
    pub(crate) fn filter<'a>(&'a mut self, input: &'a [u8]) -> &'a [u8] {
        self.filter_at(input, Instant::now())
    }

    fn filter_at<'a>(&'a mut self, input: &'a [u8], now: Instant) -> &'a [u8] {
        self.expire(now);
        // 快路径:正文之外,这批连 ESC 都没有
        if self.lex == Lex::Ground && memchr(ESC, input).is_none() {
            return input;
        }

        let mut out = std::mem::take(&mut self.out);
        out.clear();
        let mut sink = Sink {
            input,
            out: &mut out,
            start: 0,
            rewritten: false,
        };
        // 当前序列的 ESC 在这一批里的位置。`None` = 它是上一批留下的。
        let mut esc_at: Option<usize> = None;
        let mut i = 0;
        while i < input.len() {
            match self.lex {
                Lex::Ground => match memchr(ESC, &input[i..]) {
                    None => i = input.len(),
                    Some(at) => {
                        esc_at = Some(i + at);
                        self.lex = Lex::Esc;
                        i += at + 1;
                    }
                },
                Lex::Esc => match input[i] {
                    b'_' => {
                        self.lex = Lex::EscApc;
                        i += 1;
                    }
                    b'[' => {
                        self.release_held(&mut sink);
                        self.lex = Lex::Csi(2);
                        i += 1;
                    }
                    // 连着两个 ESC:vte 也是前一个作废、从后一个重新开始
                    ESC => {
                        self.release_held(&mut sink);
                        esc_at = Some(i);
                        i += 1;
                    }
                    _ => {
                        self.release_held(&mut sink);
                        self.lex = Lex::Ground;
                    }
                },
                Lex::EscApc => {
                    if input[i] == b'G' {
                        // 我们的。连开头一起摘掉;开头是上一批留下的就只丢 `held`
                        let from = esc_at.unwrap_or(sink.start);
                        sink.skip(from, i + 1);
                        self.held = 0;
                        self.lex = Lex::Body;
                        self.body.clear();
                        self.body_overflow = false;
                        self.body_activity = now;
                        i += 1;
                    } else {
                        // 别的 APC:原样交给 vte,它自己吞
                        self.release_held(&mut sink);
                        self.lex = Lex::Ground;
                    }
                }
                Lex::Csi(matched) => {
                    if input[i] == CSI_16T[matched as usize] {
                        i += 1;
                        let matched = matched + 1;
                        if matched as usize == CSI_16T.len() {
                            self.shared.answer_cell_size_query();
                            self.lex = Lex::Ground;
                        } else {
                            self.lex = Lex::Csi(matched);
                        }
                    } else {
                        self.lex = Lex::Ground;
                    }
                }
                Lex::Body => {
                    let rest = &input[i..];
                    match memchr3(ESC, CAN, SUB, rest) {
                        None => {
                            self.push_body(rest);
                            sink.skip(i, input.len());
                            i = input.len();
                        }
                        Some(at) => {
                            self.push_body(&rest[..at]);
                            sink.skip(i, i + at);
                            i += at;
                            if input[i] == ESC {
                                sink.skip(i, i + 1);
                                i += 1;
                                self.lex = Lex::BodyEsc;
                            } else {
                                // CAN / SUB:这条作废,字节本身交给 vte
                                self.abandon_body();
                                self.lex = Lex::Ground;
                            }
                        }
                    }
                    self.body_activity = now;
                }
                Lex::BodyEsc => {
                    if input[i] == b'\\' {
                        sink.skip(i, i + 1);
                        i += 1;
                        self.lex = Lex::Ground;
                        if let Some(marker) = self.finish_body(now) {
                            sink.inject(i, marker.as_bytes());
                        }
                    } else {
                        // 被新序列打断。这个 ESC 上一步已经丢了,现在算它是「上一批留下
                        // 的」:新序列要放行时由 `release_held` 补回去
                        self.abandon_body();
                        self.held = 1;
                        esc_at = None;
                        self.lex = Lex::Esc;
                    }
                }
            }
        }

        // 批尾是半条序列:扣下
        let limit = match self.lex {
            Lex::Esc => {
                self.held = 1;
                esc_at.unwrap_or(sink.start)
            }
            Lex::EscApc => {
                self.held = 2;
                esc_at.unwrap_or(sink.start)
            }
            _ => input.len(),
        };
        let (start, rewritten) = (sink.start, sink.rewritten);
        if rewritten {
            sink.commit_to(limit);
        }
        self.out = out;
        if rewritten {
            &self.out
        } else {
            &input[start..limit]
        }
    }

    /// 上一批扣下的字节确定不是图形序列:补回去。
    fn release_held(&mut self, sink: &mut Sink<'_>) {
        if self.held > 0 {
            sink.inject(sink.start, &b"\x1b_"[..self.held as usize]);
            self.held = 0;
        }
    }

    fn push_body(&mut self, bytes: &[u8]) {
        if self.body_overflow {
            return;
        }
        if self.body.len() + bytes.len() > MAX_BODY_BYTES {
            self.body_overflow = true;
            self.body = Vec::new();
            return;
        }
        self.body.extend_from_slice(bytes);
    }

    fn abandon_body(&mut self) {
        self.body.clear();
        self.body_overflow = false;
        self.body.shrink_to(BODY_KEEP_CAPACITY);
    }

    /// 正文到头了:执行它。返回要塞回字节流的带内标记。
    fn finish_body(&mut self, now: Instant) -> Option<String> {
        let overflow = std::mem::take(&mut self.body_overflow);
        let body = std::mem::take(&mut self.body);
        let marker = if overflow {
            // 控制段也一起丢了,不知道该不该应答,也不知道是不是分块的一部分:
            // 前面攒的分块跟着作废
            self.upload = None;
            None
        } else {
            self.dispatch(&body, now)
        };
        self.body = body;
        self.abandon_body();
        marker
    }

    fn dispatch(&mut self, body: &[u8], now: Instant) -> Option<String> {
        let (raw_control, payload) = split_body(body);
        // 写坏了的命令连它是不是分块都说不准:规范要求分块之间不插别的,一起作废
        let Some(control) = parse_control(raw_control) else {
            self.upload = None;
            return None;
        };

        if let Some(mut upload) = self.upload.take() {
            // 后续分块只带 `m` 和 `q`
            let continuation = control.action.is_none()
                && control
                    .image_id
                    .is_none_or(|id| upload.control.image_id == Some(id));
            if continuation {
                append_chunk(&mut upload, payload);
                upload.last_chunk = now;
                if control.more {
                    self.upload = Some(upload);
                    return None;
                }
                return self.finish_upload(upload);
            }
            // 规范:一张图发完之前不该有别的图形命令。来了就当前面那张不要了
        }

        match control.action.unwrap_or(b't') {
            b't' | b'T' => self.begin_transmit(control, payload, now),
            // 虚拟摆放、相对摆放:范围之外,不摆
            b'p' if control.unsupported_placement => None,
            b'p' => Some(self.shared.enqueue(Pending::Place {
                image_id: control.image_id.unwrap_or(0),
                spec: PlaceSpec::from_control(&control),
                reply: ReplyTo::from_control(&control),
            })),
            b'd' => Some(self.shared.enqueue(Pending::Delete(control))),
            // `a=q`、动画(`a=f` `a=a` `a=c`):范围之外,吞掉不应答
            _ => None,
        }
    }

    fn begin_transmit(&mut self, control: Control, payload: &[u8], now: Instant) -> Option<String> {
        let rejected = if control.format != Some(100) {
            Some("ENOTSUPPORTED:only PNG (f=100)")
        } else if control.medium.is_some_and(|medium| medium != b'd') {
            Some("ENOTSUPPORTED:only direct transmission (t=d)")
        } else if control.compression.is_some() {
            Some("ENOTSUPPORTED:compression")
        } else if control.image_number.is_some() {
            Some("ENOTSUPPORTED:image number")
        } else {
            None
        };
        let more = control.more;
        let mut upload = Upload {
            control,
            data: Vec::new(),
            rejected,
            last_chunk: now,
        };
        append_chunk(&mut upload, payload);
        if more {
            self.upload = Some(upload);
            return None;
        }
        self.finish_upload(upload)
    }

    /// 最后一块到了:解码,排进待执行队列。**这里只解码不入库**,入库要等标记走到。
    fn finish_upload(&mut self, upload: Upload) -> Option<String> {
        let Upload {
            control,
            data,
            rejected,
            ..
        } = upload;
        let reply = ReplyTo::from_control(&control);
        if let Some(error) = rejected {
            self.shared.reply(&reply, Err(error));
            return None;
        }

        // `a=t` 只传不摆,不知道会显示多大,只封顶;`a=T` 按这次摆放的显示尺寸缩
        let place = (control.action == Some(b'T') && !control.unsupported_placement)
            .then(|| PlaceSpec::from_control(&control));
        let cell = self.shared.known_cell_pixels();
        let bitmap = decode_png(&data, |source| {
            target_bitmap_size(source, place.as_ref(), cell)
        });
        drop(data);
        let bitmap = match bitmap {
            Ok(bitmap) => bitmap,
            Err(error) => {
                self.shared.reply(&reply, Err(error.code()));
                return None;
            }
        };
        if !self.shared.reserve_pending(bitmap.byte_len()) {
            self.shared
                .reply(&reply, Err("ENOMEM:too many pending images"));
            return None;
        }
        Some(self.shared.enqueue(Pending::Transmit {
            image_id: control.image_id.filter(|&id| id != 0),
            bitmap,
            place,
            reply,
        }))
    }

    /// 寿命到点的善后。只在有东西可放的时候才要看钟。
    fn expire(&mut self, now: Instant) {
        if self.upload.as_ref().is_some_and(|upload| {
            now.saturating_duration_since(upload.last_chunk) > UPLOAD_IDLE_LIMIT
        }) {
            self.upload = None;
        }
        // 没收尾的正文:内存先放掉。状态留着,后面的字节仍按正文吞,到结束符再作废 ——
        // 否则那半截 base64 会被当成文字显示出来
        if matches!(self.lex, Lex::Body | Lex::BodyEsc)
            && !self.body.is_empty()
            && now.saturating_duration_since(self.body_activity) > UPLOAD_IDLE_LIMIT
        {
            self.body_overflow = true;
            self.body = Vec::new();
        }
    }

    #[cfg(test)]
    pub(super) fn upload_pending(&self) -> bool {
        self.upload.is_some()
    }

    #[cfg(test)]
    pub(super) fn upload_bytes(&self) -> usize {
        self.upload.as_ref().map_or(0, |upload| upload.data.len())
    }

    #[cfg(test)]
    pub(super) fn body_capacity(&self) -> usize {
        self.body.capacity()
    }
}

/// 把一块 payload 解进上传缓冲。超限或解不开就判拒收,并当场放掉已经攒的内存。
fn append_chunk(upload: &mut Upload, payload: &[u8]) {
    if upload.rejected.is_some() {
        return;
    }
    if upload.data.len() + payload.len() / 4 * 3 > MAX_UPLOAD_BYTES {
        upload.rejected = Some("ENOMEM:transmission too large");
        upload.data = Vec::new();
        return;
    }
    if BASE64.decode_vec(payload, &mut upload.data).is_err() {
        upload.rejected = Some("EINVAL:bad base64");
        upload.data = Vec::new();
    }
}

#[cfg(test)]
mod tests {
    use super::super::{KittyShared, Pending};
    use super::*;
    use crate::EventQueue;

    fn parser() -> (PreParser, Arc<KittyShared>, EventQueue) {
        let events = EventQueue::default();
        let shared = Arc::new(KittyShared::new(events.clone()));
        (PreParser::new(shared.clone()), shared, events)
    }

    /// 喂一批,返回交给 vte 的字节。
    fn feed(p: &mut PreParser, bytes: &[u8]) -> Vec<u8> {
        p.filter(bytes).to_vec()
    }

    /// 同一段字节按每种切法各喂一遍,交给 vte 的总和必须一样(标记里的序号除外)。
    fn split_equivalent(stream: &[u8]) -> Vec<u8> {
        let (mut whole, _, _) = parser();
        let expected = feed(&mut whole, stream);
        for size in 1..stream.len().max(2) {
            let (mut p, _, _) = parser();
            let mut got = Vec::new();
            for chunk in stream.chunks(size) {
                got.extend_from_slice(p.filter(chunk));
            }
            assert_eq!(
                strip_nonce(&got),
                strip_nonce(&expected),
                "切成 {size} 字节一块时结果不同"
            );
        }
        expected
    }

    /// 标记里的 nonce 每个解析器都不同,比较时抹掉。
    fn strip_nonce(bytes: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(bytes);
        let mut out = String::new();
        let mut rest = &*text;
        while let Some(at) = rest.find("mt-kitty:") {
            out.push_str(&rest[..at + "mt-kitty:".len()]);
            rest = &rest[at + "mt-kitty:".len()..];
            let end = rest.find(':').expect("nonce 后面有序号");
            rest = &rest[end..];
        }
        out.push_str(rest);
        out.into_bytes()
    }

    /// 没有要摘的东西时**不复制**:返回的就是入参。
    #[test]
    fn 普通输出不复制() {
        let (mut p, _, _) = parser();
        let input = b"plain text\r\n\x1b[31mred\x1b[0m\x1b]0;title\x07 more";
        let out = p.filter(input);
        assert_eq!(out.as_ptr(), input.as_ptr(), "应当是同一块内存");
        assert_eq!(out, input);

        let input = b"no escape at all";
        let out = p.filter(input);
        assert_eq!(out.as_ptr(), input.as_ptr());
    }

    #[test]
    fn 别的_apc_原样放行() {
        let stream = b"a\x1b_foo bar\x1b\\b\x1b_\x1b\\c";
        assert_eq!(split_equivalent(stream), stream);
    }

    #[test]
    fn 其它序列原样放行() {
        let stream = b"\x1bP1;2|data\x1b\\\x1b]8;;https://x\x1b\\\x1b[?2026h\x1b\x1b[2J\x1bc\x1b(B";
        assert_eq!(split_equivalent(stream), stream);
    }

    /// `ESC` 落在批尾先扣下,下一批补回去。
    #[test]
    fn 批尾的_esc_先扣下() {
        let (mut p, _, _) = parser();
        assert_eq!(feed(&mut p, b"abc\x1b"), b"abc");
        assert_eq!(feed(&mut p, b"[31m"), b"\x1b[31m");

        assert_eq!(feed(&mut p, b"\x1b_"), b"");
        assert_eq!(feed(&mut p, b"x\x1b\\"), b"\x1b_x\x1b\\");

        assert_eq!(feed(&mut p, b"\x1b"), b"");
        assert_eq!(feed(&mut p, b"_"), b"");
        assert_eq!(feed(&mut p, b"x"), b"\x1b_x");
    }

    fn pending_kinds(shared: &KittyShared) -> Vec<&'static str> {
        shared
            .pending
            .lock()
            .iter()
            .map(|(_, pending)| match pending {
                Pending::Transmit { .. } => "transmit",
                Pending::Place { .. } => "place",
                Pending::Delete(_) => "delete",
            })
            .collect()
    }

    /// 图形序列被摘掉,换成一条标记;前后的字节原样。
    #[test]
    fn 删除命令换成带内标记() {
        let (mut p, shared, _) = parser();
        let out = feed(&mut p, b"before\x1b_Ga=d,d=A,q=2\x1b\\after");
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("before\x1b]8;;mt-kitty:"), "{text:?}");
        assert!(text.ends_with("\x1b\\after"), "{text:?}");
        assert_eq!(pending_kinds(&shared), ["delete"]);
    }

    #[test]
    fn 按字节切开也认得图形序列() {
        let stream = b"x\x1b_Ga=d,d=A,q=2\x1b\\y\x1b_Ga=p,i=3,c=2,r=2\x1b\\z";
        let expected = split_equivalent(stream);
        let text = String::from_utf8(expected).unwrap();
        assert_eq!(text.matches("mt-kitty:").count(), 2);
        assert!(text.starts_with('x') && text.ends_with('z'));
    }

    /// 没结束的 ESC 打断正文:整条作废,打断它的序列照常放行。
    #[test]
    fn 正文被新序列打断时作废() {
        let (mut p, shared, _) = parser();
        assert_eq!(feed(&mut p, b"\x1b_Ga=d,d=A\x1b[31mred"), b"\x1b[31mred");
        assert!(pending_kinds(&shared).is_empty());

        // 打断它的 ESC 落在批尾
        let (mut p, _, _) = parser();
        assert_eq!(feed(&mut p, b"\x1b_Ga=d\x1b"), b"");
        assert_eq!(feed(&mut p, b"[0m"), b"\x1b[0m");

        // 被另一条图形序列打断:前一条作废,后一条照常
        let (mut p, shared, _) = parser();
        let out = feed(&mut p, b"\x1b_Ga=d,d=a\x1b_Ga=p,i=1\x1b\\");
        assert!(String::from_utf8(out).unwrap().contains("mt-kitty:"));
        assert_eq!(pending_kinds(&shared), ["place"]);
    }

    /// CAN / SUB 按 vte 的口径打断正文,字节本身还是交给 vte。
    #[test]
    fn 正文被_can_打断() {
        let (mut p, shared, _) = parser();
        assert_eq!(feed(&mut p, b"\x1b_Ga=d\x18text"), b"\x18text");
        assert!(pending_kinds(&shared).is_empty());
    }

    #[test]
    fn 写坏的控制段被吞掉() {
        let (mut p, shared, events) = parser();
        assert_eq!(feed(&mut p, b"a\x1b_Gi=abc;xx\x1b\\b"), b"ab");
        assert!(pending_kinds(&shared).is_empty());
        assert!(events.drain().is_empty());
    }

    /// 范围之外的动作(查询、动画):吞掉,不应答,不留东西。
    #[test]
    fn 范围外的动作安全吞掉() {
        let (mut p, shared, events) = parser();
        for seq in [
            &b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"[..],
            b"\x1b_Ga=f,i=1,m=1;AAAA\x1b\\\x1b_Ga=f,m=0;AAAA\x1b\\",
            b"\x1b_Ga=a,i=1,s=3\x1b\\",
            b"\x1b_Ga=c,i=1,r=1\x1b\\",
        ] {
            assert_eq!(feed(&mut p, seq), b"", "{}", String::from_utf8_lossy(seq));
        }
        assert!(pending_kinds(&shared).is_empty());
        assert!(!p.upload_pending());
        assert!(events.drain().is_empty());
    }

    /// 不支持的传输方式与格式:连带后续分块一起吞掉,不解码、不攒内存。
    #[test]
    fn 不支持的传输被吞掉且不攒数据() {
        let (mut p, shared, _) = parser();
        for seq in [
            &b"\x1b_Ga=T,f=100,t=f;L3RtcC94\x1b\\"[..],
            b"\x1b_Ga=T,f=100,t=s;L3RtcC94\x1b\\",
            b"\x1b_Ga=T,f=100,t=t;L3RtcC94\x1b\\",
            b"\x1b_Ga=T,f=32,s=1,v=1;AAAAAA==\x1b\\",
            b"\x1b_Ga=T,f=24,s=1,v=1;AAAA\x1b\\",
            b"\x1b_Ga=T,s=1,v=1;AAAAAA==\x1b\\", // f 缺省 32
            b"\x1b_Ga=T,f=100,o=z;AAAA\x1b\\",
        ] {
            assert_eq!(feed(&mut p, seq), b"", "{}", String::from_utf8_lossy(seq));
        }
        assert!(pending_kinds(&shared).is_empty());

        assert_eq!(feed(&mut p, b"\x1b_Ga=T,f=32,s=9,v=9,m=1;AAAA\x1b\\"), b"");
        assert!(p.upload_pending());
        assert_eq!(p.upload_bytes(), 0, "判了不收的不攒");
        assert_eq!(
            feed(&mut p, b"\x1b_Gm=1;AAAA\x1b\\\x1b_Gm=0;AAAA\x1b\\"),
            b""
        );
        assert!(!p.upload_pending());
    }

    /// 非法 base64 判拒收。
    #[test]
    fn 坏的_base64_不进库() {
        let (mut p, shared, _) = parser();
        assert_eq!(feed(&mut p, b"\x1b_Ga=T,f=100;!!!!\x1b\\"), b"");
        assert!(pending_kinds(&shared).is_empty());
    }

    /// 分块没发完就来了别的图形命令:前面那张作废。
    #[test]
    fn 分块被别的命令打断时作废() {
        let (mut p, shared, _) = parser();
        feed(&mut p, b"\x1b_Ga=T,f=100,i=5,m=1;AAAA\x1b\\");
        assert!(p.upload_pending());
        feed(&mut p, b"\x1b_Ga=d,d=A\x1b\\");
        assert!(!p.upload_pending());
        assert_eq!(pending_kinds(&shared), ["delete"]);

        // 写坏的命令同样打断
        feed(&mut p, b"\x1b_Ga=T,f=100,i=5,m=1;AAAA\x1b\\");
        feed(&mut p, b"\x1b_Gi=zz\x1b\\");
        assert!(!p.upload_pending());

        // 孤儿分块(前面没有开头):按缺省动作传输,f 缺省 32,拒收
        feed(&mut p, b"\x1b_Gm=0;AAAA\x1b\\");
        assert!(!p.upload_pending());
        assert_eq!(pending_kinds(&shared), ["delete"]);
    }

    /// 没收尾的分块到点作废,内存放掉。
    #[test]
    fn 未完成的分块到点作废() {
        let (mut p, _, _) = parser();
        let t0 = Instant::now();
        p.filter_at(b"\x1b_Ga=T,f=100,i=1,m=1;AAAA\x1b\\", t0);
        assert!(p.upload_pending());
        assert_eq!(p.upload_bytes(), 3);

        p.filter_at(b"text", t0 + UPLOAD_IDLE_LIMIT);
        assert!(p.upload_pending(), "刚好到点还不算超");

        p.filter_at(b"text", t0 + UPLOAD_IDLE_LIMIT + Duration::from_secs(1));
        assert!(!p.upload_pending());
    }

    /// 新分块续命:按「多久没来新分块」算,不按总时长。
    #[test]
    fn 新分块刷新寿命() {
        let (mut p, _, _) = parser();
        let t0 = Instant::now();
        p.filter_at(b"\x1b_Ga=T,f=100,i=1,m=1;AAAA\x1b\\", t0);
        let later = t0 + UPLOAD_IDLE_LIMIT - Duration::from_secs(1);
        p.filter_at(b"\x1b_Gm=1;AAAA\x1b\\", later);
        p.filter_at(b"text", later + UPLOAD_IDLE_LIMIT - Duration::from_secs(1));
        assert!(p.upload_pending());
        assert_eq!(p.upload_bytes(), 6);
    }

    /// 没收尾的正文也不能一直占内存;而且不能把半截 base64 当文字放出去。
    #[test]
    fn 没收尾的正文到点放内存且不漏字节() {
        let (mut p, shared, _) = parser();
        let t0 = Instant::now();
        assert_eq!(p.filter_at(b"\x1b_Ga=d,d=A;AAAA", t0), b"");
        let t1 = t0 + UPLOAD_IDLE_LIMIT + Duration::from_secs(1);
        assert_eq!(p.filter_at(b"AAAA", t1), b"", "还在正文里,照吞");
        assert_eq!(p.body_capacity(), 0, "内存放了");
        assert_eq!(p.filter_at(b"\x1b\\ok", t1), b"ok");
        assert!(pending_kinds(&shared).is_empty(), "超时的这条作废");
    }

    /// 不分块的超长正文:超限后只吞不存,结束时作废;内存不随输入增长。
    #[test]
    fn 超长正文只吞不存() {
        let (mut p, shared, _) = parser();
        assert_eq!(feed(&mut p, b"\x1b_Ga=T,f=100,i=1;"), b"");
        let block = vec![b'A'; 1024 * 1024];
        for _ in 0..(MAX_BODY_BYTES / block.len() + 2) {
            assert_eq!(p.filter(&block), b"");
        }
        assert_eq!(p.body_capacity(), 0, "超限后不留缓冲");
        assert_eq!(feed(&mut p, b"\x1b\\after"), b"after");
        assert!(pending_kinds(&shared).is_empty());
        // 下一条照常
        let out = feed(&mut p, b"\x1b_Ga=d\x1b\\");
        assert!(String::from_utf8(out).unwrap().contains("mt-kitty:"));
    }

    /// 分块总量超限:判拒收,已经攒的放掉,后续分块只吞。
    #[test]
    fn 分块总量超限时拒收() {
        let (mut p, shared, _) = parser();
        feed(&mut p, b"\x1b_Ga=T,f=100,i=7,m=1;\x1b\\");
        let mut chunk = b"\x1b_Gm=1;".to_vec();
        chunk.extend(std::iter::repeat_n(b'A', 4 * 1024 * 1024));
        chunk.extend_from_slice(b"\x1b\\");
        for _ in 0..30 {
            feed(&mut p, &chunk);
        }
        assert!(p.upload_pending());
        assert_eq!(p.upload_bytes(), 0, "超限那一刻就放了");
        assert_eq!(feed(&mut p, b"\x1b_Gm=0;AAAA\x1b\\"), b"");
        assert!(!p.upload_pending());
        assert!(pending_kinds(&shared).is_empty());
    }

    /// `CSI 16 t` 只看不摘,跨批次也认得;差一个字节的不认。
    #[test]
    fn 像素查询只看不摘() {
        let stream = b"a\x1b[16tb";
        for size in 1..stream.len() {
            let (mut p, shared, events) = parser();
            shared.set_cell_pixels(8.4, 17.0);
            let mut got = Vec::new();
            for chunk in stream.chunks(size) {
                got.extend_from_slice(p.filter(chunk));
            }
            assert_eq!(got, stream, "切成 {size} 字节");
            let replies: Vec<String> = events
                .drain()
                .into_iter()
                .filter_map(|event| match event {
                    alacritty_terminal::event::Event::PtyWrite(text) => Some(text),
                    _ => None,
                })
                .collect();
            assert_eq!(replies, ["\x1b[6;17;8t"], "切成 {size} 字节");
        }

        let (mut p, _, events) = parser();
        for other in [
            &b"\x1b[16;1t"[..],
            b"\x1b[1t",
            b"\x1b[116t",
            b"\x1b]16t\x07",
        ] {
            feed(&mut p, other);
        }
        assert!(events.drain().is_empty());
    }

    /// 乱字节不崩,放行的也还是合理的子集。给预解析兜个底。
    #[test]
    fn 乱字节不崩() {
        let (mut p, _, _) = parser();
        let mut seed = 0x9E37_79B9u32;
        let mut stream = Vec::new();
        for _ in 0..200_000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            // 专挑协议里有意义的字节多喂
            let byte = match seed % 8 {
                0 => ESC,
                1 => b'_',
                2 => b'G',
                3 => b'\\',
                4 => b'[',
                5 => b';',
                _ => (seed >> 8) as u8,
            };
            stream.push(byte);
        }
        for chunk in stream.chunks(257) {
            let _ = p.filter(chunk);
        }
    }
}
