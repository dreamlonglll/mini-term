//! SSH 交互记录的内存模型:每个 pane 一份按时间排序的记录队列,不落盘、不进日志。
//!
//! # 输出怎么变成「行」
//!
//! 服务器输出按流(stdout / stderr 各一路 [`StreamDecoder`])做三步清洗,再按到达顺序拼进
//! 同一份 [`OutputBuf`](两路交错保序,颜色按段记在 `runs` 上):
//!
//! 1. **UTF-8 增量解码**:跨分片的多字节字符拼起来再解,非法字节按 lossy 换成 U+FFFD;
//! 2. **剥 ANSI 转义**:CSI / OSC / DCS / SOS / PM / APC 与两字符转义整段丢掉,别的 C0 / C1
//!    控制字符也丢(`\t` `\n` 留着);转义序列跨分片同样认得出;
//! 3. **换行归一**:`\r\n` → `\n`,剩下的裸 `\r` 也按换行处理(进度条会变成多行,规格口径)。
//!
//! `OutputBuf` 只存一份拼好的正文 + 每个显示行的起点:界面虚拟化列表的一项就是一行。
//! 超长的逻辑行按 [`ROW_CHUNK_CHARS`] 切成几段(「软行」,复制时不插换行),免得一个 1 MiB
//! 的单行(压缩过的 JSON、base64)变成一个要整段排版的巨型列表项。
//!
//! # 上限(主程序侧再兜一次,不信任客户端)
//!
//! - 单条记录镜像输出 [`RECORD_OUTPUT_CAP`](1 MiB),超出只计数并标 `truncated`;
//! - 每个 pane 合计 [`PANE_OUTPUT_CAP`](4 MiB)或 [`PANE_RECORD_CAP`](500)条,超了丢最早的整条
//!   (正在往里写的那一条除外)。

use std::collections::VecDeque;
use std::ops::Range;
use std::time::{Duration, Instant};

use super::wire::{EndInfo, FeedEvent, StartInfo, Stream};

/// 单条记录最多镜像多少字节原始输出。
pub const RECORD_OUTPUT_CAP: usize = 1024 * 1024;
/// 每个 pane 全部记录的镜像输出合计上限。
pub const PANE_OUTPUT_CAP: usize = 4 * 1024 * 1024;
/// 每个 pane 最多留多少条记录。
pub const PANE_RECORD_CAP: usize = 500;
/// 一个显示行最多多少字符;更长的逻辑行切成几段软行(见模块注释)。
pub const ROW_CHUNK_CHARS: usize = 1024;
/// 制表符展开到的列宽。
const TAB_WIDTH: usize = 4;
/// CSI / 两字符转义的参数段最长多少字符,超了当畸形序列放弃(回到正文)。
const CSI_CAP: usize = 256;
/// OSC / DCS 这类字符串型序列最长多少字符。没有终止符的畸形序列不至于吞掉全部输出。
const STRING_SEQ_CAP: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// 单路流的增量清洗
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Seq {
    #[default]
    Ground,
    /// 刚看到 ESC。
    Escape,
    /// ESC 之后的中间字节(`ESC ( B` 这类字符集指定)。
    Intermediate,
    Csi,
    Osc,
    /// OSC 里看到 ESC,等 `\` 收尾(ST)。
    OscSt,
    /// DCS / SOS / PM / APC。
    Str,
    /// 上面那类串里看到 ESC,等 `\` 收尾(ST)。
    StrSt,
}

/// 一路流(stdout 或 stderr)的增量解码器,状态跨分片保留。
#[derive(Debug, Default)]
pub struct StreamDecoder {
    /// 上一片末尾没凑齐的 UTF-8 字节(≤ 3 个)。
    utf8: Vec<u8>,
    /// 转义序列状态机当前在哪。
    esc: Seq,
    /// 当前序列已吃进的字符数(畸形序列封顶用)。
    esc_len: usize,
    /// 上一个字符是 `\r`,等下一个字符决定是不是 `\r\n`。
    pending_cr: bool,
}

impl StreamDecoder {
    /// 解一片原始字节,清洗后的正文追加到 `out`。
    pub fn feed(&mut self, bytes: &[u8], out: &mut String) {
        let mut data = std::mem::take(&mut self.utf8);
        data.extend_from_slice(bytes);
        let mut rest: &[u8] = &data;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.push_str(text, out);
                    return;
                }
                Err(err) => {
                    let (valid, tail) = rest.split_at(err.valid_up_to());
                    if let Ok(text) = std::str::from_utf8(valid) {
                        self.push_str(text, out);
                    }
                    match err.error_len() {
                        Some(bad) => {
                            self.push_char('\u{FFFD}', out);
                            rest = &tail[bad..];
                        }
                        // 末尾是一个没收全的多字节字符:留到下一片
                        None => {
                            self.utf8 = tail.to_vec();
                            return;
                        }
                    }
                }
            }
        }
    }

    /// 流结束:没凑齐的字节按 lossy 收尾,挂着的 `\r` 落成换行,转义状态复位。
    pub fn finish(&mut self, out: &mut String) {
        if !self.utf8.is_empty() {
            self.utf8.clear();
            self.push_char('\u{FFFD}', out);
        }
        self.esc = Seq::Ground;
        self.esc_len = 0;
        if self.pending_cr {
            self.pending_cr = false;
            out.push('\n');
        }
    }

    fn push_str(&mut self, text: &str, out: &mut String) {
        for c in text.chars() {
            self.push_char(c, out);
        }
    }

    fn enter(&mut self, state: Seq) {
        self.esc = state;
        self.esc_len = 0;
    }

    /// 序列里又吃进一个字符;超过 `cap` 当畸形序列放弃。
    fn bump(&mut self, cap: usize) {
        self.esc_len += 1;
        if self.esc_len > cap {
            self.enter(Seq::Ground);
        }
    }

    fn push_char(&mut self, c: char, out: &mut String) {
        match self.esc {
            Seq::Ground => self.ground(c, out),
            Seq::Escape => match c {
                '[' => self.enter(Seq::Csi),
                ']' => self.enter(Seq::Osc),
                'P' | 'X' | '^' | '_' => self.enter(Seq::Str),
                '\u{1b}' => self.enter(Seq::Escape),
                '\u{20}'..='\u{2f}' => self.enter(Seq::Intermediate),
                '\u{30}'..='\u{7e}' => self.enter(Seq::Ground),
                // ESC 后面跟了个不成序列的字符:ESC 丢掉,字符照常处理
                _ => {
                    self.enter(Seq::Ground);
                    self.ground(c, out);
                }
            },
            Seq::Intermediate => match c {
                '\u{20}'..='\u{2f}' => self.bump(CSI_CAP),
                '\u{30}'..='\u{7e}' => self.enter(Seq::Ground),
                '\u{1b}' => self.enter(Seq::Escape),
                _ => {
                    self.enter(Seq::Ground);
                    self.ground(c, out);
                }
            },
            Seq::Csi => match c {
                // 参数字节 0x30–0x3F 与中间字节 0x20–0x2F
                '\u{20}'..='\u{3f}' => self.bump(CSI_CAP),
                // 终结字节
                '\u{40}'..='\u{7e}' => self.enter(Seq::Ground),
                '\u{1b}' => self.enter(Seq::Escape),
                // 序列里夹了控制字符(多半是被截断的序列):序列作废,字符照常处理
                _ => {
                    self.enter(Seq::Ground);
                    self.ground(c, out);
                }
            },
            Seq::Osc => match c {
                '\u{07}' | '\u{9c}' => self.enter(Seq::Ground),
                '\u{1b}' => self.esc = Seq::OscSt,
                _ => self.bump(STRING_SEQ_CAP),
            },
            Seq::Str => match c {
                '\u{07}' | '\u{9c}' => self.enter(Seq::Ground),
                '\u{1b}' => self.esc = Seq::StrSt,
                _ => self.bump(STRING_SEQ_CAP),
            },
            Seq::OscSt | Seq::StrSt => {
                if c == '\\' {
                    self.enter(Seq::Ground);
                } else {
                    // 不是 ST:那个 ESC 开了一个新的转义序列,旧串作废
                    self.enter(Seq::Escape);
                    self.push_char(c, out);
                }
            }
        }
    }

    fn ground(&mut self, c: char, out: &mut String) {
        if self.pending_cr {
            self.pending_cr = false;
            out.push('\n');
            if c == '\n' {
                // `\r\n` 合成一个换行
                return;
            }
        }
        match c {
            '\r' => self.pending_cr = true,
            '\n' | '\t' => out.push(c),
            '\u{1b}' => self.enter(Seq::Escape),
            // 8 位 C1 形态的 CSI / OSC / DCS / SOS / PM / APC
            '\u{9b}' => self.enter(Seq::Csi),
            '\u{9d}' => self.enter(Seq::Osc),
            '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => self.enter(Seq::Str),
            // 其余 C0 / DEL / C1 控制字符(BEL、退格……)一律丢
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
}

// ---------------------------------------------------------------------------
// 拼好的输出 + 显示行
// ---------------------------------------------------------------------------

/// 一条记录的输出正文。stdout / stderr 按到达顺序拼在一起,`runs` 记每段属于哪一路。
#[derive(Debug, Default)]
pub struct OutputBuf {
    text: String,
    /// 每个显示行在 `text` 里的起点(字节偏移)。行 `i` 是 `rows[i]..rows[i+1]`(最后一行到结尾)。
    rows: Vec<u32>,
    /// `(起点, 流)`:从起点开始到下一段起点之前都属于这一路。
    runs: Vec<(u32, Stream)>,
    /// 最后一行还能往里接字(没以 `\n` 收尾、也没到软行上限)。
    open: bool,
    /// 当前显示行已有多少字符(决定软行切分)。
    row_chars: usize,
    /// 当前逻辑行已有多少字符(决定制表符展开到哪一列)。
    line_chars: usize,
}

impl OutputBuf {
    /// 追加一段已清洗的正文,返回本次改动到的第一个显示行下标(空串返回 `None`)。
    pub fn append(&mut self, stream: Stream, text: &str) -> Option<usize> {
        if text.is_empty() {
            return None;
        }
        if self.runs.last().map(|run| run.1) != Some(stream) {
            self.runs.push((self.text.len() as u32, stream));
        }
        let mut first: Option<usize> = None;
        for c in text.chars() {
            match c {
                '\n' => {
                    self.ensure_row(&mut first);
                    self.text.push('\n');
                    self.open = false;
                    self.line_chars = 0;
                }
                '\t' => {
                    let spaces = TAB_WIDTH - self.line_chars % TAB_WIDTH;
                    for _ in 0..spaces {
                        self.push_visible(' ', &mut first);
                    }
                }
                c => self.push_visible(c, &mut first),
            }
        }
        first
    }

    fn push_visible(&mut self, c: char, first: &mut Option<usize>) {
        if self.open && self.row_chars >= ROW_CHUNK_CHARS {
            // 软行:当前行到上限了,后面的字另起一行(不插换行符)
            self.open = false;
        }
        self.ensure_row(first);
        self.text.push(c);
        self.row_chars += 1;
        self.line_chars += 1;
    }

    fn ensure_row(&mut self, first: &mut Option<usize>) {
        if !self.open {
            self.rows.push(self.text.len() as u32);
            self.open = true;
            self.row_chars = 0;
        }
        first.get_or_insert(self.rows.len() - 1);
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// 第 `index` 个显示行在 `text` 里的范围(不含行尾 `\n`)。
    pub fn row_range(&self, index: usize) -> Option<Range<usize>> {
        let start = *self.rows.get(index)? as usize;
        let end = self
            .rows
            .get(index + 1)
            .map_or(self.text.len(), |next| *next as usize);
        let end = if self.text[start..end].ends_with('\n') {
            end - 1
        } else {
            end
        };
        Some(start..end)
    }

    /// 第 `index` 个显示行的文字(不含行尾 `\n`)。
    #[cfg(test)]
    pub fn row(&self, index: usize) -> Option<&str> {
        self.row_range(index).map(|range| &self.text[range])
    }

    /// `range`(`text` 里的绝对范围)内按流分段,返回**相对 `range.start`** 的子范围。
    pub fn spans(&self, range: Range<usize>) -> Vec<(Range<usize>, Stream)> {
        let mut out = Vec::new();
        if range.is_empty() {
            return out;
        }
        let first = self
            .runs
            .partition_point(|run| run.0 as usize <= range.start)
            .saturating_sub(1);
        for (i, &(start, stream)) in self.runs.iter().enumerate().skip(first) {
            let start = start as usize;
            if start >= range.end {
                break;
            }
            let end = self
                .runs
                .get(i + 1)
                .map_or(self.text.len(), |next| next.0 as usize);
            let seg_start = start.max(range.start);
            let seg_end = end.min(range.end);
            if seg_start < seg_end {
                out.push((seg_start - range.start..seg_end - range.start, stream));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 记录
// ---------------------------------------------------------------------------

/// 一次调用的结局。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Running,
    /// 收到了 `end` 帧。
    Ended(EndInfo),
    /// `end` 之前连接没了。
    Interrupted,
}

/// 一次 `exec` / `upload` / `download` 调用。
#[derive(Debug)]
pub struct Record {
    /// pane 内递增的序号(界面列表靠它认记录)。
    pub seq: u64,
    /// 监听层的连接编号,事件靠它找到记录。
    call: u64,
    pub info: StartInfo,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub outcome: Outcome,
    pub output: OutputBuf,
    decoders: [StreamDecoder; 2],
    /// 已镜像(进了 `output`)的原始字节数。
    mirrored: usize,
    /// 收到的原始字节数(含超上限没镜像的)。
    received: u64,
    /// 有输出没镜像出来(本地上限 / 监听层上限 / CLI 那边丢的)。
    pub truncated: bool,
    /// 自上次同步列表以来改动到的第一个「记录内行号」(0 = 头部)。见 `view::sync_rows`。
    dirty_from: Option<usize>,
}

impl Record {
    fn new(seq: u64, call: u64, info: StartInfo, now: Instant) -> Self {
        Self {
            seq,
            call,
            info,
            started: now,
            finished: None,
            outcome: Outcome::Running,
            output: OutputBuf::default(),
            decoders: Default::default(),
            mirrored: 0,
            received: 0,
            truncated: false,
            dirty_from: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.outcome == Outcome::Running
    }

    /// 结局算不算失败(按钮上的红点):退出码 ≠ 0 / 超时 / 连接阶段超时 / 错误 / 已中断。
    pub fn failed(&self) -> bool {
        match &self.outcome {
            Outcome::Running => false,
            Outcome::Interrupted => true,
            Outcome::Ended(end) => {
                end.error.is_some()
                    || end.timed_out
                    || end.not_started
                    || end.exit_code.is_some_and(|code| code != 0)
            }
        }
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.finished
            .map(|done| done.saturating_duration_since(self.started))
    }

    /// 已镜像(界面上看得到)的原始字节数。CLI 的镜像总是输出的一段前缀。
    pub fn mirrored(&self) -> usize {
        self.mirrored
    }

    /// 服务器输出的总字节数:CLI 报的(含没镜像的部分)与本地收到的取大。
    pub fn output_total(&self) -> u64 {
        let reported = match &self.outcome {
            Outcome::Ended(end) => end.output_bytes.unwrap_or(0),
            _ => 0,
        };
        reported.max(self.received)
    }

    /// 结尾状态占几行:状态行 + (截断时)截断说明。
    pub fn status_rows(&self) -> usize {
        1 + usize::from(self.truncated)
    }

    /// 列表里一共占几项:头部 + 输出行 + 状态行。
    pub fn row_count(&self) -> usize {
        1 + self.output.row_count() + self.status_rows()
    }

    pub fn take_dirty(&mut self) -> Option<usize> {
        self.dirty_from.take()
    }

    fn mark_dirty(&mut self, row: usize) {
        self.dirty_from = Some(self.dirty_from.map_or(row, |d| d.min(row)));
    }

    /// 收一片输出,返回实际镜像了多少原始字节。
    fn push_output(&mut self, stream: Stream, data: &[u8]) -> usize {
        if !self.is_running() {
            return 0;
        }
        self.received += data.len() as u64;
        let room = RECORD_OUTPUT_CAP.saturating_sub(self.mirrored);
        let take = data.len().min(room);
        if take < data.len() {
            self.set_truncated();
        }
        if take == 0 {
            return 0;
        }
        self.mirrored += take;
        let mut text = String::new();
        self.decoders[stream.index()].feed(&data[..take], &mut text);
        self.append_text(stream, &text);
        take
    }

    fn append_text(&mut self, stream: Stream, text: &str) {
        if let Some(first) = self.output.append(stream, text) {
            self.mark_dirty(1 + first);
        }
    }

    fn set_truncated(&mut self) {
        if !self.truncated {
            self.truncated = true;
            self.mark_dirty(1 + self.output.row_count());
        }
    }

    fn finish(&mut self, outcome: Outcome, dropped: bool, now: Instant) {
        if !self.is_running() {
            return;
        }
        for stream in [Stream::Stdout, Stream::Stderr] {
            let mut text = String::new();
            self.decoders[stream.index()].finish(&mut text);
            self.append_text(stream, &text);
        }
        if dropped || matches!(&outcome, Outcome::Ended(end) if end.truncated) {
            self.set_truncated();
        }
        self.outcome = outcome;
        self.finished = Some(now);
        self.mark_dirty(1 + self.output.row_count());
    }
}

// ---------------------------------------------------------------------------
// 一个 pane 的全部记录
// ---------------------------------------------------------------------------

/// 一次 [`Activity::apply`] 改了什么。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// 条数 / 在跑 / 失败这些按钮上看得见的东西变了。
    pub summary: bool,
    /// 列表内容变了。
    pub rows: bool,
}

impl Applied {
    pub fn merge(&mut self, other: Applied) {
        self.summary |= other.summary;
        self.rows |= other.rows;
    }
}

/// 一个 pane 的记录队列(按开始时间排序,最新在后)。
#[derive(Debug, Default)]
pub struct Activity {
    records: VecDeque<Record>,
    next_seq: u64,
    /// 全部记录已镜像的原始字节合计。
    total: usize,
}

impl Activity {
    pub fn apply(&mut self, event: FeedEvent, now: Instant) -> Applied {
        match event {
            FeedEvent::Start { call, info, .. } => {
                while self.records.len() >= PANE_RECORD_CAP {
                    self.evict(0);
                }
                let seq = self.next_seq;
                self.next_seq += 1;
                self.records.push_back(Record::new(seq, call, info, now));
                Applied {
                    summary: true,
                    rows: true,
                }
            }
            FeedEvent::Output {
                call, stream, data, ..
            } => {
                let Some(index) = self.position(call) else {
                    return Applied::default();
                };
                let record = &mut self.records[index];
                let was_truncated = record.truncated;
                let accepted = record.push_output(stream, &data);
                let newly_truncated = record.truncated && !was_truncated;
                self.total += accepted;
                let evicted = self.enforce_total(call);
                Applied {
                    summary: evicted,
                    rows: accepted > 0 || newly_truncated || evicted,
                }
            }
            FeedEvent::End {
                call, end, dropped, ..
            } => self.finish(call, Outcome::Ended(end), dropped, now),
            FeedEvent::Interrupted { call, dropped, .. } => {
                self.finish(call, Outcome::Interrupted, dropped, now)
            }
        }
    }

    fn finish(&mut self, call: u64, outcome: Outcome, dropped: bool, now: Instant) -> Applied {
        let Some(index) = self.position(call) else {
            return Applied::default();
        };
        self.records[index].finish(outcome, dropped, now);
        Applied {
            summary: true,
            rows: true,
        }
    }

    /// 按连接编号找记录。在跑的记录都在队尾附近,倒着找。
    fn position(&self, call: u64) -> Option<usize> {
        self.records.iter().rposition(|r| r.call == call)
    }

    fn evict(&mut self, index: usize) -> bool {
        match self.records.remove(index) {
            Some(record) => {
                self.total -= record.mirrored;
                true
            }
            None => false,
        }
    }

    /// 合计超过 4 MiB 时从最早的开始整条丢,`keep_call` 那一条(正在写的)不丢。
    fn enforce_total(&mut self, keep_call: u64) -> bool {
        let mut evicted = false;
        while self.total > PANE_OUTPUT_CAP {
            let Some(index) = self.records.iter().position(|r| r.call != keep_call) else {
                break;
            };
            evicted |= self.evict(index);
        }
        evicted
    }

    pub fn clear(&mut self) {
        self.records.clear();
        self.total = 0;
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn records(&self) -> impl DoubleEndedIterator<Item = &Record> + ExactSizeIterator {
        self.records.iter()
    }

    pub fn records_mut(&mut self) -> impl Iterator<Item = &mut Record> {
        self.records.iter_mut()
    }

    /// 按序号找记录(序号单调递增,二分)。
    pub fn record(&self, seq: u64) -> Option<&Record> {
        let index = self.records.partition_point(|r| r.seq < seq);
        self.records.get(index).filter(|r| r.seq == seq)
    }

    pub fn any_running(&self) -> bool {
        self.records.iter().any(Record::is_running)
    }

    /// 没有在跑、且最近一条的结局是失败(按钮上的红点)。
    pub fn last_failed(&self) -> bool {
        !self.any_running() && self.records.back().is_some_and(Record::failed)
    }

    /// 全部记录已镜像的原始字节合计。
    #[cfg(test)]
    pub fn total_bytes(&self) -> usize {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_activity::wire::Op;

    fn clean(chunks: &[&[u8]]) -> String {
        let mut decoder = StreamDecoder::default();
        let mut out = String::new();
        for chunk in chunks {
            decoder.feed(chunk, &mut out);
        }
        decoder.finish(&mut out);
        out
    }

    fn info() -> StartInfo {
        StartInfo {
            op: Op::Exec,
            connection: "prod".into(),
            command: Some("uname -a".into()),
            cwd: None,
            timeout_secs: None,
            local_path: None,
            remote_path: None,
        }
    }

    fn start(call: u64) -> FeedEvent {
        FeedEvent::Start {
            pty_id: 1,
            call,
            info: info(),
        }
    }

    fn out(call: u64, stream: Stream, data: &[u8]) -> FeedEvent {
        FeedEvent::Output {
            pty_id: 1,
            call,
            stream,
            data: data.to_vec(),
        }
    }

    fn end(call: u64, end: EndInfo) -> FeedEvent {
        FeedEvent::End {
            pty_id: 1,
            call,
            end,
            dropped: false,
        }
    }

    fn rows(buf: &OutputBuf) -> Vec<&str> {
        (0..buf.row_count()).map(|i| buf.row(i).unwrap()).collect()
    }

    #[test]
    fn utf8跨分片拼接() {
        let bytes = "中文é".as_bytes();
        // 每个字节单独一片
        let chunks: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(clean(&chunks), "中文é");
        // 切在多字节字符中间
        assert_eq!(clean(&[&bytes[..2], &bytes[2..4], &bytes[4..]]), "中文é");
    }

    #[test]
    fn 非法字节按lossy替换() {
        assert_eq!(clean(&[b"a\xffb"]), "a\u{FFFD}b");
        // 末尾没凑齐的多字节字符:收尾时替换
        assert_eq!(clean(&[b"ok\xe4\xb8"]), "ok\u{FFFD}");
        // 截断的字符后面跟了正常字节
        assert_eq!(clean(&[b"\xe4\xb8", b"x"]), "\u{FFFD}x");
    }

    #[test]
    fn 剥掉ansi转义() {
        assert_eq!(clean(&[b"\x1b[1;31mred\x1b[0m plain"]), "red plain");
        // OSC 两种终止符
        assert_eq!(clean(&[b"\x1b]0;title\x07a\x1b]8;;http://x\x1b\\b"]), "ab");
        // 字符集指定 / 两字符转义
        assert_eq!(clean(&[b"\x1b(Bx\x1b=y\x1b7z"]), "xyz");
        // DCS / APC
        assert_eq!(clean(&[b"\x1bPq#0;2;0\x1b\\v\x1b_Gf=1;AAAA\x1b\\w"]), "vw");
        // 跨分片的序列
        assert_eq!(clean(&[b"a\x1b", b"[3", b"8;5;1", b"mb"]), "ab");
        assert_eq!(clean(&[b"a\x1b]0;ti", b"tle\x1b", b"\\b"]), "ab");
        // 8 位 C1 形态的 CSI
        assert_eq!(clean(&["a\u{9b}31mb".as_bytes()]), "ab");
        // 其余控制字符丢掉,\t 保留
        assert_eq!(clean(&[b"a\x07b\x08c\td\x7f"]), "abc\td");
        // CSI 里夹控制字符:序列作废,字符照常
        assert_eq!(clean(&[b"a\x1b[12\nb"]), "a\nb");
    }

    #[test]
    fn 回车处理() {
        assert_eq!(clean(&[b"a\r\nb\r\n"]), "a\nb\n");
        // \r\n 跨分片
        assert_eq!(clean(&[b"a\r", b"\nb"]), "a\nb");
        // 裸 \r 当换行(进度条)
        assert_eq!(clean(&[b"10%\r20%\r30%"]), "10%\n20%\n30%");
        // 末尾挂着的 \r
        assert_eq!(clean(&[b"done\r"]), "done\n");
        // \r\r\n
        assert_eq!(clean(&[b"a\r\r\nb"]), "a\n\nb");
        // \r 后跟转义序列
        assert_eq!(clean(&[b"a\r\x1b[Kb"]), "a\nb");
    }

    #[test]
    fn 显示行与软行() {
        let mut buf = OutputBuf::default();
        assert_eq!(buf.append(Stream::Stdout, "a\n\nbc"), Some(0));
        assert_eq!(rows(&buf), ["a", "", "bc"]);
        // 接着往开着的最后一行里写
        assert_eq!(buf.append(Stream::Stdout, "d\ne\n"), Some(2));
        assert_eq!(rows(&buf), ["a", "", "bcd", "e"]);
        // 以 \n 收尾后再来的字另起一行
        assert_eq!(buf.append(Stream::Stdout, "f"), Some(4));
        assert_eq!(buf.text(), "a\n\nbcd\ne\nf");

        // 超长行切成软行,正文里不插换行
        let mut long = OutputBuf::default();
        let line = "x".repeat(ROW_CHUNK_CHARS * 2 + 5);
        long.append(Stream::Stdout, &(line.clone() + "\nz"));
        assert_eq!(long.row_count(), 4);
        assert_eq!(long.row(0).unwrap().len(), ROW_CHUNK_CHARS);
        assert_eq!(long.row(2).unwrap(), "xxxxx");
        assert_eq!(long.row(3).unwrap(), "z");
        assert_eq!(long.text(), format!("{line}\nz"));
    }

    #[test]
    fn 制表符展开() {
        let mut buf = OutputBuf::default();
        buf.append(Stream::Stdout, "a\tb\n\tc\nabcd\te");
        assert_eq!(rows(&buf), ["a   b", "    c", "abcd    e"]);
    }

    #[test]
    fn stderr与stdout交错保序() {
        let mut buf = OutputBuf::default();
        buf.append(Stream::Stdout, "out1 ");
        buf.append(Stream::Stderr, "err\n");
        buf.append(Stream::Stdout, "out2\n");
        assert_eq!(rows(&buf), ["out1 err", "out2"]);
        let row0 = buf.row_range(0).unwrap();
        assert_eq!(
            buf.spans(row0),
            vec![(0..5, Stream::Stdout), (5..8, Stream::Stderr)]
        );
        let row1 = buf.row_range(1).unwrap();
        assert_eq!(buf.spans(row1), vec![(0..4, Stream::Stdout)]);
    }

    #[test]
    fn 一次完整调用的记录() {
        let mut activity = Activity::default();
        let now = Instant::now();
        activity.apply(start(1), now);
        assert!(activity.any_running());
        activity.apply(out(1, Stream::Stdout, b"\x1b[32mok\x1b[0m\r\n"), now);
        activity.apply(out(1, Stream::Stderr, b"warn\xe4"), now);
        activity.apply(out(1, Stream::Stderr, b"\xb8\xad\n"), now);
        let done = now + Duration::from_millis(1500);
        let applied = activity.apply(
            end(
                1,
                EndInfo {
                    exit_code: Some(3),
                    output_bytes: Some(99),
                    ..Default::default()
                },
            ),
            done,
        );
        assert!(applied.summary && applied.rows);
        let record = activity.records().next().unwrap();
        assert_eq!(rows(&record.output), ["ok", "warn中"]);
        assert_eq!(record.elapsed(), Some(Duration::from_millis(1500)));
        assert_eq!(
            record.output_total(),
            99,
            "CLI 报的总数比本地收到的大时取它"
        );
        assert!(record.failed());
        assert!(activity.last_failed());
        assert!(!activity.any_running());
    }

    #[test]
    fn 未收到end断开记成已中断() {
        let mut activity = Activity::default();
        let now = Instant::now();
        activity.apply(start(5), now);
        activity.apply(out(5, Stream::Stdout, b"half\r"), now);
        activity.apply(
            FeedEvent::Interrupted {
                pty_id: 1,
                call: 5,
                dropped: false,
            },
            now,
        );
        let record = activity.records().next().unwrap();
        assert_eq!(record.outcome, Outcome::Interrupted);
        assert!(record.failed());
        assert_eq!(rows(&record.output), ["half"], "挂着的 \\r 收尾时落成换行");
        // 结束之后再来的输出 / 结局都不收
        let applied = activity.apply(out(5, Stream::Stdout, b"late"), now);
        assert_eq!(applied, Applied::default());
        activity.apply(end(5, EndInfo::default()), now);
        assert_eq!(
            activity.records().next().unwrap().outcome,
            Outcome::Interrupted
        );
    }

    #[test]
    fn 不认识的连接编号忽略() {
        let mut activity = Activity::default();
        let now = Instant::now();
        assert_eq!(
            activity.apply(out(9, Stream::Stdout, b"x"), now),
            Applied::default()
        );
        assert_eq!(
            activity.apply(end(9, EndInfo::default()), now),
            Applied::default()
        );
        assert!(activity.is_empty());
    }

    #[test]
    fn 单条记录的镜像上限() {
        let mut activity = Activity::default();
        let now = Instant::now();
        activity.apply(start(1), now);
        let chunk = vec![b'a'; 300 * 1024];
        for _ in 0..4 {
            activity.apply(out(1, Stream::Stdout, &chunk), now);
        }
        let record = activity.records().next().unwrap();
        assert_eq!(record.output.text().len(), RECORD_OUTPUT_CAP);
        assert!(record.truncated);
        assert_eq!(record.output_total(), 4 * 300 * 1024);
        assert_eq!(record.status_rows(), 2);
        assert_eq!(activity.total_bytes(), RECORD_OUTPUT_CAP);
    }

    #[test]
    fn 截断标记也可以来自end或监听层() {
        let mut activity = Activity::default();
        let now = Instant::now();
        activity.apply(start(1), now);
        activity.apply(
            end(
                1,
                EndInfo {
                    exit_code: Some(0),
                    truncated: true,
                    ..Default::default()
                },
            ),
            now,
        );
        activity.apply(start(2), now);
        activity.apply(
            FeedEvent::End {
                pty_id: 1,
                call: 2,
                end: EndInfo::default(),
                dropped: true,
            },
            now,
        );
        assert!(activity.records().all(|r| r.truncated));
    }

    #[test]
    fn 每个pane的条数上限丢最早的整条() {
        let mut activity = Activity::default();
        let now = Instant::now();
        for call in 0..(PANE_RECORD_CAP as u64 + 3) {
            activity.apply(start(call), now);
        }
        assert_eq!(activity.len(), PANE_RECORD_CAP);
        assert_eq!(activity.records().next().unwrap().seq, 3);
        assert!(activity.record(2).is_none());
        assert_eq!(activity.record(3).unwrap().seq, 3);
        assert_eq!(
            activity.record(PANE_RECORD_CAP as u64 + 2).unwrap().seq,
            PANE_RECORD_CAP as u64 + 2
        );
    }

    #[test]
    fn 每个pane的字节上限丢最早的整条但不丢正在写的() {
        let mut activity = Activity::default();
        let now = Instant::now();
        let mib = vec![b'b'; RECORD_OUTPUT_CAP];
        // 第 0 条一直在跑,先写满 1 MiB
        activity.apply(start(0), now);
        activity.apply(out(0, Stream::Stdout, &mib), now);
        for call in 1..=3 {
            activity.apply(start(call), now);
            activity.apply(out(call, Stream::Stdout, &mib), now);
            activity.apply(end(call, EndInfo::default()), now);
        }
        assert_eq!(activity.total_bytes(), PANE_OUTPUT_CAP);
        assert_eq!(activity.len(), 4);
        // 第 4 条写进来 → 超 4 MiB → 丢最早的第 0 条
        activity.apply(start(4), now);
        let applied = activity.apply(out(4, Stream::Stdout, b"x"), now);
        assert!(applied.summary, "条数变了,按钮要重画");
        assert_eq!(activity.len(), 4);
        assert!(activity.record(0).is_none());
        assert!(activity.total_bytes() <= PANE_OUTPUT_CAP);

        // 正在写的那条是最早的一条时跳过它,丢它后面最早的
        let mut activity = Activity::default();
        activity.apply(start(10), now);
        for call in 11..=14 {
            activity.apply(start(call), now);
            activity.apply(
                out(call, Stream::Stdout, &mib[..RECORD_OUTPUT_CAP - 1]),
                now,
            );
        }
        activity.apply(out(10, Stream::Stdout, &mib[..8]), now);
        assert!(activity.records().any(|r| r.seq == 0), "正在写的不丢");
        assert!(activity.record(1).is_none(), "丢的是它后面最早的那条");
        assert!(activity.total_bytes() <= PANE_OUTPUT_CAP);
    }

    #[test]
    fn 列表脏行从改动处算起() {
        let mut activity = Activity::default();
        let now = Instant::now();
        activity.apply(start(1), now);
        let record = activity.records_mut().next().unwrap();
        assert_eq!(record.take_dirty(), None, "新记录由列表整条插入,不靠脏行");
        activity.apply(out(1, Stream::Stdout, b"a\nb"), now);
        let record = activity.records_mut().next().unwrap();
        assert_eq!(record.take_dirty(), Some(1), "头部之后的第 0 个输出行");
        activity.apply(out(1, Stream::Stdout, b"c\nd\n"), now);
        let record = activity.records_mut().next().unwrap();
        assert_eq!(record.take_dirty(), Some(2), "从开着的那一行算起");
        activity.apply(end(1, EndInfo::default()), now);
        let record = activity.records_mut().next().unwrap();
        assert_eq!(record.take_dirty(), Some(1 + 3), "状态行");
        assert_eq!(record.row_count(), 1 + 3 + 1);
    }

    #[test]
    fn 清空() {
        let mut activity = Activity::default();
        let now = Instant::now();
        activity.apply(start(1), now);
        activity.apply(out(1, Stream::Stdout, b"x"), now);
        activity.clear();
        assert!(activity.is_empty());
        assert_eq!(activity.total_bytes(), 0);
        assert!(!activity.last_failed());
        // 清掉之后还在跑的那次调用的后续事件不再落地
        assert_eq!(
            activity.apply(out(1, Stream::Stdout, b"y"), now),
            Applied::default()
        );
    }
}
