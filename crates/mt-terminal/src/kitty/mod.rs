//! Kitty 图形协议的静态 PNG 子集,目标客户端是 pi。**默认关闭**,关着时这里的代码一行都不跑。
//!
//! # 数据流
//!
//! ```text
//! PTY 字节 ─→ PreParser ─┬─ 其余字节 ──────────────────────→ vte ─→ WidthOverride ─→ Term
//!  (reader 线程,锁外)    └─ ESC_G…ESC\ ─→ 解码,排进待执行队列   │
//!                            字节流里留一条 OSC 8 标记 ───────────┘
//!                                                     标记走到时:入库 / 挂锚 / 删除
//! ```
//!
//! # 为什么绕这一圈
//!
//! vte 在同步块(`ESC[?2026h`)里把字节攒着不处理,预解析器看到一条图形命令时 grid
//! 还是这一帧开始之前的样子:光标是旧的,旧图也还该画着。所以命令不能当场执行,要等
//! 它前面的字节都生效。办法是把命令从字节流里摘掉(payload 也就不进 vte,不占它 2 MiB
//! 的同步缓冲,解码也不用拿 term 锁),原地留一条私有 scheme 的 OSC 8 标记,让它按序
//! 流到 [`WidthOverride`](crate::width)::`set_hyperlink`。一帧里的 `a=d` 与重发的 `a=T`
//! 因此同一把 term 锁里生效,画面不会闪空。
//!
//! 标记带每终端随机的 nonce、序号一次性。程序自己发的同 scheme OSC 8 对不上号,直接丢掉。
//!
//! # 图片跟着格子走
//!
//! 见 [`anchor`]。绘制时扫可见行(以及视口上方最高那张图的行数)找锚,有锚才算活着。
//! 摆放记录没有锚了就是死的,到预算关头或隔一阵扫全屏时清掉。
//!
//! # 支持范围
//!
//! `a=t` `a=T` `a=p` `a=d`(`d=i/I/a/A`),`f=100`,`t=d`,分块 `m`,`i` `p` `c` `r`,
//! 裁切 `x` `y` `w` `h`,`q`。`a=p` 与裁切不在最初的范围里,但 pi 默认的全屏模式
//! (`TuiAltScreen`)离不开它们:图片只传一次,之后每次重排都是 `a=d,d=a` + `a=p`,滚到
//! 一半的图靠 `y` `h` `r` 裁切。
//!
//! # 已知限制
//!
//! - `C=0`(缺省)也不移动光标:ConPTY 不知道光标该下移,真移了两边就对不上了;
//! - iTerm2、Sixel、Unicode 占位符、动画、`a=q` 应答、`t=f/s/t`、raw `f=24/32`、z 序:
//!   安全吞掉,不显示乱码;
//! - 锚只有一格:覆盖写那一格、`CSI K` 碾到那一格图片就没了。协议本身不因此删图;
//! - 一格只能有一个锚:同一格再摆一次,前一次就看不到了;
//! - resize 不改摆放的行列数:图跟着锚走,不随列宽缩放,超出右边就裁掉。pi 自己会重发,
//!   单发一张就不管的程序不会(实测便携 ConPTY 1.24 在 resize 时不碾锚);
//! - pi 普通模式(`tuiMode: regular`)每次重绘都重发整张图,这里每次都重新解码、重新上传
//!   纹理,没有按内容去重;全屏模式走 `a=p`,没有这个开销;
//! - 同步块超时释放(150ms)赶上大图解码时,`a=d` 与重发之间可能闪一下;
//! - Windows 上图形命令一律不应答,见 [`GRAPHICS_REPLIES`];
//! - 放大时透明边缘有 1 像素渗色:GPU 按直通 alpha 插值,透明像素的 RGB 会混进来;
//! - 位图在账本与 gpui 的 `RenderImage` 各存一份,屏上的图实占可到预算的两倍;
//! - SSH、WSL 里不注入 `PI_IMAGE_PROTOCOL`;系统 conhost 把图形序列整段丢弃,所以只有
//!   便携 ConPTY(或非 Windows)才开。

mod anchor;
mod command;
mod decode;
mod lexer;
mod store;

use std::collections::{HashSet, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Term, TermMode};
use parking_lot::{Mutex, MutexGuard};

use crate::EventQueue;
use command::Control;
use store::{Budget, Placement, Store};

pub use decode::KittyBitmap;
pub use store::SourceRect;

pub(crate) use anchor::SCHEME;
pub(crate) use lexer::PreParser;

/// `c` `r` 的上限。一张图占的行数决定每帧要往视口上方多扫多少行。
const MAX_CELLS: u32 = 1024;

/// 渲染层还没报过一格多大时用的值(只给应答与算缺省的 `c` `r`,不拿来缩图)。
const FALLBACK_CELL: (f32, f32) = (10.0, 20.0);

/// 一格像素的定点数放大倍数:原子量存整数,又要留住 8.2 这样的小数。
const CELL_SCALE: f32 = 64.0;

/// 摆放记录每攒这么多条清一次死的。没到预算也要清:死摆放让每帧的扫描一直开着。
const SWEEP_MIN_PLACEMENTS: usize = 64;

/// 只传不摆(`a=t`)的位图,长边收到这么多。pi 默认的全屏模式每张图都走它,4K 截图原样
/// 留着就是 33 MB,渲染层还要再存一份。裁切坐标按原图算,缩多少都不影响 `a=p`。
const MAX_UNPLACED_SIDE: u32 = 2048;

/// 图形命令的应答发不发。ConPTY 的输入解析吃掉 APC 正文,只把结尾的 `ESC \` 当成一次
/// Alt+\ 交给前台程序:客户端收不到应答,shell 提示符上还平白多一个 `\`(实测 pwsh 与 cmd
/// 都中,node raw 模式也只读到 `ESC \`)。所以 Windows 上不回。`q=2` 的客户端(pi)本来就
/// 不受影响;`CSI 16 t` 走 CSI,回得去,不归这里管。单测里开着,好验应答口径。
const GRAPHICS_REPLIES: bool = cfg!(test) || !cfg!(windows);

/// 这一帧看得见的一张图。
#[derive(Clone, Debug)]
pub struct VisibleImage {
    pub bitmap: Arc<KittyBitmap>,
    /// 左上角所在的 grid 行(`Line` 坐标,负数是回看)。可能在视口上方。
    pub line: i32,
    pub column: usize,
    pub columns: u32,
    pub rows: u32,
    /// 显示原图的哪一部分。
    pub source: SourceRect,
}

/// 摆放参数(还没落到具体的图上)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlaceSpec {
    columns: Option<u32>,
    rows: Option<u32>,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    placement_id: u32,
}

impl PlaceSpec {
    fn from_control(control: &Control) -> Self {
        Self {
            columns: control.columns.filter(|&c| c > 0),
            rows: control.rows.filter(|&r| r > 0),
            src_x: control.src_x,
            src_y: control.src_y,
            src_w: control.src_w,
            src_h: control.src_h,
            placement_id: control.placement_id.unwrap_or(0),
        }
    }

    /// 裁切矩形收进原图范围。`w` `h` 为 0 = 到边。整个落在图外 = `None`。
    fn source_rect(&self, (width, height): (u32, u32)) -> Option<SourceRect> {
        if self.src_x >= width || self.src_y >= height {
            return None;
        }
        let max_w = width - self.src_x;
        let max_h = height - self.src_y;
        Some(SourceRect {
            x: self.src_x,
            y: self.src_y,
            width: if self.src_w == 0 {
                max_w
            } else {
                self.src_w.min(max_w)
            },
            height: if self.src_h == 0 {
                max_h
            } else {
                self.src_h.min(max_h)
            },
        })
    }

    /// 占几列几行。两个都给就照办;只给一个,另一个按原图比例算;都没给就是原尺寸。
    fn cells(&self, source: SourceRect, (cell_w, cell_h): (f32, f32)) -> (u32, u32) {
        let (src_w, src_h) = (source.width as f32, source.height as f32);
        let (columns, rows) = match (self.columns, self.rows) {
            (Some(columns), Some(rows)) => (columns, rows),
            (Some(columns), None) => {
                let rows = columns as f32 * cell_w * src_h / src_w / cell_h;
                (columns, rows.ceil() as u32)
            }
            (None, Some(rows)) => {
                let columns = rows as f32 * cell_h * src_w / src_h / cell_w;
                (columns.ceil() as u32, rows)
            }
            (None, None) => (
                (src_w / cell_w).ceil() as u32,
                (src_h / cell_h).ceil() as u32,
            ),
        };
        (columns.clamp(1, MAX_CELLS), rows.clamp(1, MAX_CELLS))
    }
}

/// 按显示尺寸算位图该多大。`place` 为 `None`(只传不摆)或还不知道一格多大时算不出显示
/// 尺寸,只按 [`MAX_UNPLACED_SIDE`] 封顶。
///
/// 裁切的图按「裁切部分铺满 c×r」倒推整张图的比例:第一次 `a=T` 就带裁切(滚动到一半
/// 才发的图)时,整张图都按这个比例存,后面 `a=p` 换一段裁切也够清晰。
fn target_bitmap_size(
    source: (u32, u32),
    place: Option<&PlaceSpec>,
    cell: Option<(f32, f32)>,
) -> (u32, u32) {
    let (Some(place), Some(cell)) = (place, cell) else {
        return capped_side(source, MAX_UNPLACED_SIDE);
    };
    let Some(rect) = place.source_rect(source) else {
        return capped_side(source, MAX_UNPLACED_SIDE);
    };
    let (columns, rows) = place.cells(rect, cell);
    let shown_w = columns as f32 * cell.0;
    let shown_h = rows as f32 * cell.1;
    (
        (source.0 as f32 * shown_w / rect.width as f32).ceil() as u32,
        (source.1 as f32 * shown_h / rect.height as f32).ceil() as u32,
    )
}

/// 长边超过 `side` 就等比缩到 `side`,否则原样。
fn capped_side((width, height): (u32, u32), side: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= side {
        return (width, height);
    }
    let scale = side as f32 / longest as f32;
    (
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
    )
}

/// 执行结果要不要、怎么回给程序。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReplyTo {
    image_id: Option<u32>,
    placement_id: Option<u32>,
    quiet: u32,
}

impl ReplyTo {
    fn from_control(control: &Control) -> Self {
        Self {
            image_id: control.image_id.filter(|&id| id != 0),
            placement_id: control.placement_id.filter(|&id| id != 0),
            quiet: control.quiet,
        }
    }

    /// 应答串。规范:没给 id 就不答;`q=1` 不答 OK,`q=2` 全不答。
    fn format(&self, result: Result<(), &str>) -> Option<String> {
        let id = self.image_id?;
        let message = match result {
            Ok(()) if self.quiet >= 1 => return None,
            Err(_) if self.quiet >= 2 => return None,
            Ok(()) => "OK",
            Err(error) => error,
        };
        let placement = self
            .placement_id
            .map(|p| format!(",p={p}"))
            .unwrap_or_default();
        Some(format!("\x1b_Gi={id}{placement};{message}\x1b\\"))
    }
}

/// 解码完、等带内标记走到的一条命令。
pub(crate) enum Pending {
    Transmit {
        /// 程序没给 id 时入库才分内部 id。
        image_id: Option<u32>,
        bitmap: Arc<KittyBitmap>,
        place: Option<PlaceSpec>,
        reply: ReplyTo,
    },
    Place {
        image_id: u32,
        spec: PlaceSpec,
        reply: ReplyTo,
    },
    Delete(Control),
}

/// 预解析(reader 线程、锁外)与带内执行(vte 回调、持 term 锁)共用的状态。
pub(crate) struct KittyShared {
    /// `mt-kitty:<nonce>:`。标记与锚的前缀。
    prefix: String,
    next_seq: AtomicU64,
    /// 等标记的命令,先进先出。**队列序就是字节流序**:标记走到时比它早的都不会再来了。
    pending: Mutex<VecDeque<(u64, Pending)>>,
    /// 队列里位图的总字节,算进每终端预算。
    pending_bytes: AtomicU64,
    store: Mutex<Store>,
    /// 有摆放吗。渲染层每帧先看它,没有就连锁都不拿。
    has_placements: AtomicBool,
    /// 下次清死摆放的水位。
    sweep_at: AtomicU64,
    /// 一格多少像素(设备像素,[`CELL_SCALE`] 定点)。0 = 渲染层还没报过。
    cell_width: AtomicU32,
    cell_height: AtomicU32,
    /// 见 [`GRAPHICS_REPLIES`]。
    replies: AtomicBool,
    events: EventQueue,
}

impl KittyShared {
    fn new(events: EventQueue) -> Self {
        Self::with_budget(events, Budget::DEFAULT)
    }

    fn with_budget(events: EventQueue, budget: Budget) -> Self {
        Self {
            prefix: format!("{SCHEME}{}:", random_nonce()),
            next_seq: AtomicU64::new(1),
            pending: Mutex::new(VecDeque::new()),
            pending_bytes: AtomicU64::new(0),
            store: Mutex::new(Store::new(budget)),
            has_placements: AtomicBool::new(false),
            sweep_at: AtomicU64::new(SWEEP_MIN_PLACEMENTS as u64),
            cell_width: AtomicU32::new(0),
            cell_height: AtomicU32::new(0),
            replies: AtomicBool::new(GRAPHICS_REPLIES),
            events,
        }
    }

    /// 排一条命令,返回它在字节流里的标记。
    fn enqueue(&self, pending: Pending) -> String {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().push_back((seq, pending));
        format!("\x1b]8;;{}{seq}\x1b\\", self.prefix)
    }

    /// 给排队的位图占预算。真正入库时再还。
    fn reserve_pending(&self, bytes: u64) -> bool {
        let budget = self.store.lock().budget().bytes;
        let mut current = self.pending_bytes.load(Ordering::Relaxed);
        loop {
            if current + bytes > budget {
                return false;
            }
            match self.pending_bytes.compare_exchange_weak(
                current,
                current + bytes,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => current = now,
            }
        }
    }

    fn reply(&self, to: &ReplyTo, result: Result<(), &str>) {
        if !self.replies.load(Ordering::Relaxed) {
            return;
        }
        if let Some(text) = to.format(result) {
            self.events.send_event(Event::PtyWrite(text));
        }
    }

    fn set_cell_pixels(&self, width: f32, height: f32) {
        self.cell_width
            .store((width * CELL_SCALE).round() as u32, Ordering::Relaxed);
        self.cell_height
            .store((height * CELL_SCALE).round() as u32, Ordering::Relaxed);
    }

    /// 渲染层报过的一格大小。没报过 = `None`。
    fn known_cell_pixels(&self) -> Option<(f32, f32)> {
        let width = self.cell_width.load(Ordering::Relaxed);
        let height = self.cell_height.load(Ordering::Relaxed);
        (width > 0 && height > 0).then(|| (width as f32 / CELL_SCALE, height as f32 / CELL_SCALE))
    }

    fn cell_pixels(&self) -> (f32, f32) {
        self.known_cell_pixels().unwrap_or(FALLBACK_CELL)
    }

    /// 一格多少像素,给查询应答用的整数。
    fn reported_cell_pixels(&self) -> (u16, u16) {
        let (width, height) = self.cell_pixels();
        (
            (width.round() as u16).max(1),
            (height.round() as u16).max(1),
        )
    }

    /// 应答 `CSI 16 t`:`CSI 6 ; 高 ; 宽 t`。
    fn answer_cell_size_query(&self) {
        let (width, height) = self.reported_cell_pixels();
        self.events
            .send_event(Event::PtyWrite(format!("\x1b[6;{height};{width}t")));
    }

    /// 带内标记走到了。**持着 term 锁**,由 vte 的 `set_hyperlink` 调。
    ///
    /// 对不上号(程序自己发的、序号不在队列里)就什么都不做。
    pub(crate) fn on_marker<T: EventListener>(&self, uri: &str, term: &mut Term<T>) {
        let Some(seq) = uri
            .strip_prefix(self.prefix.as_str())
            .and_then(|seq| seq.parse::<u64>().ok())
        else {
            return;
        };
        let Some(command) = self.take_pending(seq) else {
            return;
        };
        let mut store = self.store.lock();
        match command {
            Pending::Transmit {
                image_id,
                bitmap,
                place,
                reply,
            } => {
                let id = image_id.map_or_else(|| store.allocate_internal_id(), u64::from);
                self.pending_bytes
                    .fetch_sub(bitmap.byte_len(), Ordering::Relaxed);
                store.insert_image(id, bitmap.clone());
                let result = match place {
                    Some(spec) => self.place(&mut store, term, uri, seq, id, &bitmap, &spec),
                    None => Ok(()),
                };
                self.enforce(&mut store, term, Some(id));
                self.reply(&reply, result);
            }
            Pending::Place {
                image_id,
                spec,
                reply,
            } => {
                let id = u64::from(image_id);
                let result = match store.bitmap(id) {
                    Some(bitmap) => self.place(&mut store, term, uri, seq, id, &bitmap, &spec),
                    None => Err("ENOENT:image not found"),
                };
                self.enforce(&mut store, term, Some(id));
                self.reply(&reply, result);
            }
            Pending::Delete(control) => self.delete(&mut store, term, &control),
        }
        self.has_placements
            .store(store.placement_count() > 0, Ordering::Relaxed);
    }

    /// 取队列里序号为 `seq` 的命令。比它早的是丢了标记的(按理不会有),一并收拾。
    fn take_pending(&self, seq: u64) -> Option<Pending> {
        let mut pending = self.pending.lock();
        while pending.front().is_some_and(|(front, _)| *front < seq) {
            if let Some((_, Pending::Transmit { bitmap, .. })) = pending.pop_front() {
                self.pending_bytes
                    .fetch_sub(bitmap.byte_len(), Ordering::Relaxed);
            }
        }
        if pending.front().is_some_and(|(front, _)| *front == seq) {
            pending.pop_front().map(|(_, command)| command)
        } else {
            None
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn place<T: EventListener>(
        &self,
        store: &mut Store,
        term: &mut Term<T>,
        uri: &str,
        seq: u64,
        image: u64,
        bitmap: &KittyBitmap,
        spec: &PlaceSpec,
    ) -> Result<(), &'static str> {
        let Some(source) = spec.source_rect(bitmap.source_size()) else {
            return Err("EINVAL:source rectangle out of bounds");
        };
        let (columns, rows) = spec.cells(source, self.cell_pixels());
        anchor::stamp(term, uri);
        store.add_placement(
            seq,
            Placement {
                image,
                placement_id: spec.placement_id,
                columns,
                rows,
                source,
                on_alt: term.mode().contains(TermMode::ALT_SCREEN),
            },
        );
        Ok(())
    }

    fn delete<T: EventListener>(&self, store: &mut Store, term: &Term<T>, control: &Control) {
        let what = control.delete.unwrap_or(b'a');
        let free = what.is_ascii_uppercase();
        match what.to_ascii_lowercase() {
            b'a' => {
                let seqs = visible_seqs(store, term);
                store.delete_placements(&seqs, free);
            }
            b'i' => {
                if let Some(id) = control.image_id {
                    store.delete_by_image(u64::from(id), control.placement_id, free);
                }
            }
            // 别的范围(按编号、按格、按 z 序、动画帧):吞掉
            _ => {}
        }
    }

    /// 超预算就清。先清死摆放,再按 [`Store::enforce`] 的顺序往外请。
    fn enforce<T: EventListener>(&self, store: &mut Store, term: &Term<T>, protect: Option<u64>) {
        let count = store.placement_count() as u64;
        let sweep_due = count >= self.sweep_at.load(Ordering::Relaxed);
        if !(sweep_due || store.over_budget() || store.too_many_placements()) {
            return;
        }
        let on_alt = term.mode().contains(TermMode::ALT_SCREEN);
        let mut live = HashSet::new();
        anchor::for_each(term, i32::MIN / 2, i32::MAX / 2, |seq, _, _| {
            live.insert(seq);
        });
        store.sweep_dead(on_alt, &live);
        self.sweep_at.store(
            (store.placement_count() as u64 * 2).max(SWEEP_MIN_PLACEMENTS as u64),
            Ordering::Relaxed,
        );

        if store.over_budget() || store.too_many_placements() {
            let seqs: HashSet<u64> = visible_seqs(store, term).into_iter().collect();
            store.trim_placements(&seqs);
            let images: HashSet<u64> = seqs
                .iter()
                .filter_map(|seq| store.resolve(*seq).map(|(p, _)| p.image))
                .collect();
            store.enforce(&images, protect);
        }
    }

    /// 协议:`CSI 2J` 把屏上的图删掉(数据留着)。主屏的 2J 是把行顶进回看,不显式删的话
    /// 锚跟进回看,图片的下半截就压在清完的屏幕上。
    pub(crate) fn on_clear_screen<T: EventListener>(&self, term: &Term<T>) {
        if !self.has_placements.load(Ordering::Relaxed) {
            return;
        }
        let mut store = self.store.lock();
        let seqs = visible_seqs(&store, term);
        store.delete_placements(&seqs, false);
        self.has_placements
            .store(store.placement_count() > 0, Ordering::Relaxed);
    }

    /// RIS:图片状态全部清空。
    pub(crate) fn on_reset(&self) {
        self.store.lock().clear();
        self.has_placements.store(false, Ordering::Relaxed);
    }
}

/// 当前屏幕上看得见的摆放的序号。「屏幕」是活动屏的 `0..screen_lines`,与用户翻没翻回看
/// 无关;图可能从视口上方探下来,所以往上多扫它的行数。
fn visible_seqs<T: EventListener>(store: &Store, term: &Term<T>) -> Vec<u64> {
    let tallest = store.tallest() as i32;
    let screen_lines = term.screen_lines() as i32;
    let mut seqs = Vec::new();
    anchor::for_each(term, 1 - tallest, screen_lines, |seq, line, _| {
        if let Some((placement, _)) = store.resolve(seq)
            && line + placement.rows as i32 > 0
        {
            seqs.push(seq);
        }
    });
    seqs
}

/// 每终端一个的随机串。只用来挡程序伪造标记,不是密钥,不值得为它加一个随机数依赖:
/// `RandomState` 的种子来自系统随机数,程序读不到它的输出。
fn random_nonce() -> String {
    let draw = || {
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    };
    format!("{:016x}{:016x}", draw(), draw())
}

/// 一个终端的 Kitty 图形状态。[`TerminalEmulator::kitty`](crate::TerminalEmulator::kitty)
/// 返回 `None` 就是开关关着。
pub struct Kitty {
    shared: Arc<KittyShared>,
    pre: Mutex<PreParser>,
}

impl Kitty {
    pub(crate) fn new(events: EventQueue) -> Self {
        let shared = Arc::new(KittyShared::new(events));
        Self {
            pre: Mutex::new(PreParser::new(shared.clone())),
            shared,
        }
    }

    #[cfg(test)]
    fn with_budget(events: EventQueue, budget: Budget) -> Self {
        let shared = Arc::new(KittyShared::with_budget(events, budget));
        Self {
            pre: Mutex::new(PreParser::new(shared.clone())),
            shared,
        }
    }

    pub(crate) fn shared(&self) -> Arc<KittyShared> {
        self.shared.clone()
    }

    pub(crate) fn pre_parser(&self) -> MutexGuard<'_, PreParser> {
        self.pre.lock()
    }

    /// 渲染层报一格多少**设备像素**(字号、缩放、换屏都会变)。查询应答与缩图都按它算。
    pub fn set_cell_pixels(&self, width: f32, height: f32) {
        self.shared.set_cell_pixels(width, height);
    }

    /// 一格多少像素,给 `CSI 14 t` 这类查询的应答用。
    pub fn reported_cell_pixels(&self) -> (u16, u16) {
        self.shared.reported_cell_pixels()
    }

    /// 这一帧看得见的图,先摆的在前。`term` 是渲染层已经拿在手上的锁。
    ///
    /// 没有摆放时只读一个原子量。
    pub fn visible_images(&self, term: &Term<EventQueue>) -> Vec<VisibleImage> {
        if !self.shared.has_placements.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let store = self.shared.store.lock();
        let grid = term.grid();
        let offset = grid.display_offset() as i32;
        let top = -offset;
        let bottom = top + grid.screen_lines() as i32;
        let mut found: Vec<(u64, VisibleImage)> = Vec::new();
        anchor::for_each(
            term,
            top + 1 - store.tallest() as i32,
            bottom,
            |seq, line, column| {
                let Some((placement, bitmap)) = store.resolve(seq) else {
                    return;
                };
                if line + placement.rows as i32 > top {
                    found.push((
                        seq,
                        VisibleImage {
                            bitmap: bitmap.clone(),
                            line,
                            column,
                            columns: placement.columns,
                            rows: placement.rows,
                            source: placement.source,
                        },
                    ));
                }
            },
        );
        found.sort_by_key(|(seq, _)| *seq);
        found.into_iter().map(|(_, image)| image).collect()
    }

    /// 位图集合的代数。没变就不用去核对纹理缓存。
    pub fn epoch(&self) -> u64 {
        self.shared.store.lock().epoch()
    }

    /// 还活着的位图的 uid。纹理缓存拿它判哪些该放了。
    pub fn live_bitmap_uids(&self) -> HashSet<u64> {
        self.shared.store.lock().live_uids()
    }
}

#[cfg(test)]
mod tests;
