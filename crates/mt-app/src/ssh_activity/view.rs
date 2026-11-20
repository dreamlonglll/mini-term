//! 「SSH 交互」悬浮框(记录模型 + 列表),以及终端组标题栏上那颗按钮要读的状态。
//!
//! # 挂在哪
//!
//! - **按钮**在终端组标题栏右侧的控件簇里(「⚑N → SSH → 查找 → 命令库 → …」),由
//!   `terminal_area::render_leaf_controls` 画,对应该组当前 active pane。它只读本视图的
//!   [`SshActivityView::button_state`](条数 / 在跑 / 失败 / 开合),点击走
//!   [`SshActivityView::toggle`]。没有记录时簇里不画它。
//! - **悬浮框**还在 pane 内容区右上角:每个 [`crate::pane::TerminalPane`] 持有一个本视图实体,
//!   恒挂在 pane 根容器右上角那一列(`top 6px / right 14px`,与 Ctrl+F 查找条同一锚点):
//!   列里先是查找条(开着时)、再是本视图,竖排、右对齐、间距 6px —— 查找条打开时悬浮框
//!   自然落到它下方,两者不重叠。
//!
//! 悬浮框关着时本视图画一个空 `div`(零尺寸、没有监听,不挡终端)。**恒挂而不是开着才挂**:
//! gpui 的 `notify` 沿已渲染的视图路径把祖先一并标脏(pane 的 view 级缓存
//! `terminal_area::cached_terminal`、再往上的 TerminalArea 都在这条路上)。标题栏按钮就靠这条路
//! 刷新 —— 本视图得一直在树上,第一条记录到达时这一下 notify 才传得到标题栏。
//!
//! # 刷新节流
//!
//! 事件到达只改模型,`notify` 走 [`REFRESH_PERIOD`] 的前沿节流(≤ 10Hz):空闲时第一次当场画,
//! 之后合并到窗口末尾。只 `notify` 本视图 —— 祖先(本 pane、标题栏所在的 TerminalArea)随之
//! 重画,别的 pane 的缓存不动。悬浮框关着时只有按钮状态([`ButtonState`])真变了才 notify,
//! 纯输出事件一个都不发 —— 标题栏不会跟着输出分片重画。
//!
//! # 列表
//!
//! 悬浮框正文是 `gpui::list` 虚拟化列表,一项 = 记录头 / 一个输出显示行 / 状态行 / 截断说明
//! (见 [`sync_rows`])。1 MiB 输出切成上万行,只有视口 ± overdraw 里的行参与布局。
//! 列表停在底部时跟随新输出(`FollowMode::Tail`),用户往上翻时不打扰,翻回底部自动续上。
//!
//! # 底色
//!
//! 鼠标不在框上:终端底色 × [`IDLE_BG_ALPHA`] + 细边框、无阴影;移入:实底 `bg_elevated` + 阴影。
//! 文字颜色两态一致。gpui 没有实时 backdrop blur(见 `frost.rs` 模块注释),淡底就是带 alpha 的底色。
//!
//! # 开合动画
//!
//! 打开:框体从顶边水平居中处的一个小框长到最终尺寸 —— 宽度从中间向两边、高度从上往下
//! ([`reveal_rect`],[`REVEAL_OPEN`] ease-out);收起完全反向([`REVEAL_CLOSE`] ease-in),
//! 播完才从树上卸下。动画中途再点按钮从当前进度反向播([`Reveal::retarget`]),不跳变。
//! 只有位移与尺寸的方向性展开,**不做透明度淡入淡出**;用「裁剪窗」实现,内容按最终尺寸排版、
//! 不重排不缩放(见 `render_popover`)。只在动画进行中要帧;豁免系统「减少动画」(与其它浮层
//! 进出场一致),但服从设置里的终端动画总开关 `terminal_animations`(关掉时直接开 / 直接收)。
//!
//! # 焦点
//!
//! 标题栏按钮点完把键盘焦点还给该 pane 的终端;悬浮框本身不 `track_focus`,点它不抢焦点。
//! 点框外不关,只有再点按钮或点框内 ✕ 才关。开关是 pane 级运行时态,不持久化。

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Bounds, ClickEvent, ClipboardItem, Context, Div, FocusHandle, FollowMode,
    HighlightStyle, InteractiveElement, IntoElement, ListAlignment, ListState, ParentElement,
    Pixels, Render, SharedString, Stateful, StatefulInteractiveElement, Styled, StyledText, Task,
    Window, canvas, div, list, prelude::FluentBuilder as _, px, relative,
};
use gpui_component::scroll::Scrollbar;
use mt_ui::icons::VectorIcon;
use mt_ui::motion::{Easing, cubic_bezier_at, ease};
use mt_ui::tooltip::TooltipExt as _;

use super::model::{Activity, Applied, Outcome, Record};
use super::wire::{FeedEvent, Op, Stream};
use crate::i18n::{t, tr};
use crate::store::AppStore;
use crate::ui;

/// 鼠标不在悬浮框上时,底色取终端底色再乘这个透明度(乘在终端底色自身的 alpha 上,
/// 背景图主题下终端底色本来就是半透明的)。
pub const IDLE_BG_ALPHA: f32 = 0.30;

/// 同一 pane 的刷新合并窗口:≤ 10Hz。
const REFRESH_PERIOD: Duration = Duration::from_millis(100);

/// 悬浮框宽 = clamp(pane 宽 × 比例, 最小, 最大),pane 太窄时再收到 pane 内。
const POPOVER_MIN_W: f32 = 360.0;
const POPOVER_MAX_W: f32 = 720.0;
const POPOVER_W_RATIO: f32 = 0.5;
/// 悬浮框高 ≤ pane 高 × 比例;内容少时按内容收缩,但不低于最小高度。
const POPOVER_H_RATIO: f32 = 0.6;
const POPOVER_MIN_H: f32 = 160.0;
/// 悬浮框离 pane 左右边缘至少留这么多(右侧与查找条的 14px 同一档)。
const PANE_EDGE: f32 = 14.0;
/// 悬浮框底边离 pane 底边至少留这么多。
const BOTTOM_GAP: f32 = 8.0;
/// 标题栏高度。
const TITLE_H: f32 = 40.0;
/// 悬浮框边框宽。
const POPOVER_BORDER: f32 = 1.0;
/// 列表上下内边距。
const LIST_PAD_Y: f32 = 4.0;
/// 列表项左右内边距。
const ROW_PAD_X: f32 = 14.0;
/// 列表视口外多量多少像素(滚动时不闪)。
const OVERDRAW: f32 = 400.0;
/// 没量过时按行数估高:每行高度 / 每条记录头尾额外占的高度。
const EST_ROW_H: f32 = 19.0;
const EST_RECORD_EXTRA: f32 = 44.0;
/// 「已复制」停留多久。
const COPIED_FOR: Duration = Duration::from_millis(1200);

/// 悬浮框**打开**动画的全程时长(ease-out)。半路掉头时按剩余路程折算。
const REVEAL_OPEN: Duration = Duration::from_millis(220);
/// 悬浮框**收起**动画的全程时长(ease-in)。
const REVEAL_CLOSE: Duration = Duration::from_millis(155);
/// 开合动画的起点:顶边水平居中处的一个小框。宽 = 终宽 × 这个比例(从中间向两边展开)。
const REVEAL_START_W: f32 = 0.10;
/// 起点高 = 终高 × 这个比例(从上往下展开)。
const REVEAL_START_H: f32 = 0.0;
/// 收起用的 ease-in 曲线控制点(CSS 关键字 `ease-in`;`mt_ui::motion::Easing` 里没有这一条)。
const EASE_IN: (f32, f32, f32, f32) = (0.42, 0.0, 1.0, 1.0);

/// 一个矩形(像素,f32)。开合动画的插值在纯数里做,便于单测。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FrameRect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

/// 开合进度 `progress`(0 = 收起,1 = 全开,已缓动)下,可见框体该画成什么矩形。
///
/// 起点是终矩形**顶边水平居中**处的一个小框(宽 [`REVEAL_START_W`]、高 [`REVEAL_START_H`]
/// 的比例):宽度从中间向两边展开,高度从上往下展开 —— 顶边与水平中线始终不动。
pub(super) fn reveal_rect(progress: f32, target: FrameRect) -> FrameRect {
    let p = progress.clamp(0.0, 1.0);
    let width = target.width * (REVEAL_START_W + (1.0 - REVEAL_START_W) * p);
    let height = target.height * (REVEAL_START_H + (1.0 - REVEAL_START_H) * p);
    FrameRect {
        left: target.left + (target.width - width) / 2.0,
        top: target.top,
        width,
        height,
    }
}

/// 悬浮框的开合进度(0 = 收起,1 = 全开)。
///
/// 不用 `mt_ui::motion::Transition`:它的 `restart` 刻意从头播,而这里要求动画中途再点按钮时
/// **从当前进度反向播、不许跳变**。于是记「起点值 → 目标值 + 起始时刻 + 时长」,掉头时以
/// 当前显示值为新起点,时长按剩余路程折算。打开 ease-out、收起 ease-in。
///
/// 不过「减少动画」那道闸(与其它浮层进出场一样豁免,`respects_reduce = false` 的口径),
/// 只服从设置里的终端动画总开关 —— 由调用方在 [`Self::retarget`] 时给 `animate`。
#[derive(Clone, Copy, Debug)]
pub(super) struct Reveal {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
}

impl Reveal {
    /// 收起且静止。
    pub(super) fn hidden(now: Instant) -> Self {
        Self {
            from: 0.0,
            to: 0.0,
            start: now,
            duration: Duration::ZERO,
        }
    }

    /// 这一刻的开合进度(已缓动)。
    pub(super) fn value_at(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.start);
        if self.duration.is_zero() || elapsed >= self.duration {
            return self.to;
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        let eased = if self.to >= self.from {
            ease(Easing::EaseOut, t)
        } else {
            let (x1, y1, x2, y2) = EASE_IN;
            cubic_bezier_at(x1, y1, x2, y2, t)
        };
        self.from + (self.to - self.from) * eased
    }

    /// 还在动吗(= 还要不要请求下一帧)。到终态之后恒 `false`。
    pub(super) fn running_at(&self, now: Instant) -> bool {
        !self.duration.is_zero() && now.saturating_duration_since(self.start) < self.duration
    }

    /// 还在树上吗:开着、或者收起动画还没播完。
    pub(super) fn mounted_at(&self, now: Instant) -> bool {
        self.value_at(now) > 0.0 || self.running_at(now)
    }

    /// 朝 `target`(0 / 1)走。从**当前显示值**起步,时长按剩余路程折算;
    /// `animate = false` 时直接到位。
    pub(super) fn retarget(&mut self, target: f32, now: Instant, animate: bool) {
        let current = self.value_at(now);
        let full = if target > current {
            REVEAL_OPEN
        } else {
            REVEAL_CLOSE
        };
        self.from = current;
        self.to = target;
        self.start = now;
        self.duration = if animate {
            full.mul_f32((target - current).abs().min(1.0))
        } else {
            Duration::ZERO
        };
    }
}

/// 终端动画总开关(设置页「启用动画」,缺省开)。与 `terminal_area` 的
/// `animations_enabled` 读同一项配置。
fn animations_enabled(cx: &gpui::App) -> bool {
    AppStore::global(cx)
        .read(cx)
        .config()
        .terminal_animations
        .unwrap_or(true)
}

/// 列表同步时记下的「每条记录在列表里占哪几项」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SyncedRecord {
    pub seq: u64,
    /// 头部在列表里的下标。
    pub start: usize,
    pub out_rows: usize,
    pub status_rows: usize,
}

impl SyncedRecord {
    fn len(&self) -> usize {
        1 + self.out_rows + self.status_rows
    }
}

/// 一个列表项是记录里的哪一部分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowKind {
    Header,
    Output(usize),
    Status,
    Truncated,
}

/// 列表下标 → (第几条记录, 哪一部分)。
pub(super) fn locate(synced: &[SyncedRecord], ix: usize) -> Option<(usize, RowKind)> {
    let pos = synced.partition_point(|s| s.start <= ix).checked_sub(1)?;
    let s = &synced[pos];
    let local = ix - s.start;
    let kind = match local {
        0 => RowKind::Header,
        n if n <= s.out_rows => RowKind::Output(n - 1),
        n if n == s.out_rows + 1 => RowKind::Status,
        n if n < s.len() => RowKind::Truncated,
        _ => return None,
    };
    Some((pos, kind))
}

/// 把模型的变化增量同步进列表状态。
///
/// 只 `splice` 真正变了的那一段:被丢掉的记录整段摘掉,新记录整段插入,在跑的记录从
/// 「改动到的第一行」(`Record::take_dirty`)换到旧结尾。改动之前的行两边一字未动,
/// 它们量过的高度留着 —— 用户往上翻着看时,下面的新输出不会把视口顶跑。
pub(super) fn sync_rows(synced: &mut Vec<SyncedRecord>, activity: &mut Activity, list: &ListState) {
    let mut old = std::mem::take(synced).into_iter().peekable();
    let mut cursor = 0usize;
    for record in activity.records_mut() {
        // 模型里已经没有的记录(被上限挤掉的)
        while let Some(gone) = old.next_if(|prev| prev.seq < record.seq) {
            list.splice(cursor..cursor + gone.len(), 0);
        }
        let entry = SyncedRecord {
            seq: record.seq,
            start: cursor,
            out_rows: record.output.row_count(),
            status_rows: record.status_rows(),
        };
        let len = entry.len();
        let dirty = record.take_dirty();
        match old.next_if(|prev| prev.seq == record.seq) {
            Some(prev) => {
                let from = match dirty {
                    Some(row) => row.min(prev.len()).min(len),
                    None if prev.len() == len => len,
                    // 没有脏行却变了长度:不该发生,整条重来兜底
                    None => 0,
                };
                if from < prev.len() || from < len {
                    list.splice(cursor + from..cursor + prev.len(), len - from);
                }
            }
            None => list.splice(cursor..cursor, len),
        }
        synced.push(entry);
        cursor += len;
    }
    for gone in old {
        list.splice(cursor..cursor + gone.len(), 0);
    }
}

/// 从零建一遍列表映射(悬浮框打开时),顺带清掉所有脏行标记。返回总项数。
pub(super) fn rebuild_rows(synced: &mut Vec<SyncedRecord>, activity: &mut Activity) -> usize {
    synced.clear();
    let mut cursor = 0usize;
    for record in activity.records_mut() {
        record.take_dirty();
        let entry = SyncedRecord {
            seq: record.seq,
            start: cursor,
            out_rows: record.output.row_count(),
            status_rows: record.status_rows(),
        };
        cursor += entry.len();
        synced.push(entry);
    }
    cursor
}

/// 读上一帧的布局结果,估列表内容(不含内边距)有多高;量不出来返回 `None`。
///
/// - 内容比视口高:视口高 + 可滚动距离 = 已量过的各项之和。新追加的行跟随末尾时一定在视口里、
///   一定量过,所以框随内容**一步长到位**,不会先蹿到最大高再缩回来;
/// - 内容装得下:首项到末项的实际跨度(两项都得量过,且列表停在顶上)。
fn measure_content(list: &ListState) -> Option<f32> {
    let count = list.item_count();
    if count == 0 {
        return Some(0.0);
    }
    let viewport = f32::from(list.viewport_bounds().size.height);
    if viewport <= 0.0 {
        // 还没布局过一次
        return None;
    }
    let overflow = f32::from(list.max_offset_for_scrollbar().y);
    if overflow > 0.5 {
        return Some(viewport + overflow);
    }
    // 跟随末尾的哨兵位置(还没经过一次布局)或滚动过:量不准,沿用上一次的
    if list.logical_scroll_top().item_ix != 0 {
        return None;
    }
    match (list.bounds_for_item(0), list.bounds_for_item(count - 1)) {
        (Some(first), Some(last)) => Some(f32::from(last.bottom() - first.top())),
        _ => None,
    }
}

/// 状态行的色调。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Running,
    Normal,
    Error,
}

/// 耗时:一分钟以内带一位小数(`1.2s`),再长写成 `3m05s`。
pub(super) fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let total = elapsed.as_secs();
        format!("{}m{:02}s", total / 60, total % 60)
    }
}

/// 记录头的主行:`exec` 是 `$ <原始命令>`,传输是 `↑ 上传 本地 → 远端` / `↓ 下载 远端 → 本地`。
pub(super) fn main_line(record: &Record) -> String {
    let info = &record.info;
    let local = info.local_path.as_deref().unwrap_or("");
    let remote = info.remote_path.as_deref().unwrap_or("");
    match info.op {
        Op::Exec => format!("$ {}", info.command.as_deref().unwrap_or("")),
        Op::Upload => tr!("sshActivity", "upload", local = local, remote = remote),
        Op::Download => tr!("sshActivity", "download", remote = remote, local = local),
    }
}

/// 结尾状态行的文案与色调。
pub(super) fn status_line(record: &Record) -> (String, Tone) {
    let elapsed = record.elapsed().map(format_elapsed).unwrap_or_default();
    match &record.outcome {
        Outcome::Running => (t("sshActivity", "running").to_string(), Tone::Running),
        Outcome::Interrupted => (t("sshActivity", "interrupted").to_string(), Tone::Error),
        Outcome::Ended(end) => {
            if let Some(message) = &end.error {
                (tr!("sshActivity", "error", message = message), Tone::Error)
            } else if end.not_started {
                (t("sshActivity", "notStarted").to_string(), Tone::Error)
            } else if end.timed_out {
                let secs = record.info.timeout_secs.unwrap_or_else(|| {
                    record
                        .elapsed()
                        .map_or(0, |d| d.as_secs_f64().round() as u64)
                });
                (tr!("sshActivity", "timedOut", secs = secs), Tone::Error)
            } else if record.info.op != Op::Exec {
                let bytes = end.bytes.unwrap_or(0);
                (
                    tr!("sshActivity", "transferred", bytes = bytes),
                    Tone::Normal,
                )
            } else if let Some(code) = end.exit_code {
                let tone = if code == 0 { Tone::Normal } else { Tone::Error };
                (
                    tr!("sshActivity", "exitCode", code = code, elapsed = elapsed),
                    tone,
                )
            } else {
                (
                    tr!("sshActivity", "finished", elapsed = elapsed),
                    Tone::Normal,
                )
            }
        }
    }
}

/// 截断说明行。不写死 1 MiB:CLI 那边队列满了也会提前停镜像,按实际收到的前缀写。
pub(super) fn truncated_line(record: &Record) -> String {
    let shown = record.mirrored() as u64;
    let total = record.output_total().max(shown);
    tr!("sshActivity", "truncated", shown = shown, total = total)
}

/// 一条记录的纯文本(「复制全部」用),与界面上看到的顺序一致。
pub(super) fn record_text(record: &Record) -> String {
    let mut out = format!("[{}] {}\n", record.info.connection, main_line(record));
    if let Some(cwd) = &record.info.cwd {
        out.push_str(&tr!("sshActivity", "cwd", cwd = cwd));
        out.push('\n');
    }
    if let Some(secs) = record.info.timeout_secs {
        out.push_str(&tr!("sshActivity", "timeoutArg", secs = secs));
        out.push('\n');
    }
    if !record.output.is_empty() {
        out.push_str(record.output.text());
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&status_line(record).0);
    out.push('\n');
    if record.truncated {
        out.push_str(&truncated_line(record));
        out.push('\n');
    }
    out
}

/// 标题栏按钮上看得见的全部状态。只有它变了,悬浮框关着时才需要 notify。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ButtonState {
    /// 记录条数(≥ 1;没有记录时按钮不画,[`SshActivityView::button_state`] 返回 `None`)。
    pub count: usize,
    /// 有调用在跑(accent 小圆点)。
    pub running: bool,
    /// 没有在跑、且最近一条的结局是失败(红色小圆点)。
    pub failed: bool,
    /// 悬浮框开着(按钮常亮)。
    pub open: bool,
}

/// 复制按钮的去向,也用来显示「已复制」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyTarget {
    All,
    Command(u64),
    Output(u64),
}

/// 一个 pane 的 SSH 交互按钮 + 悬浮框。
pub struct SshActivityView {
    pty_id: u32,
    /// 终端的焦点句柄:按钮点完把键盘焦点还回去。
    terminal_focus: FocusHandle,
    /// 命令与输出用终端的等宽字族(随终端字体设置走)。
    mono_font: SharedString,
    activity: Activity,
    open: bool,
    list: ListState,
    /// 列表当前对应的记录布局(见 [`sync_rows`])。
    synced: Vec<SyncedRecord>,
    last_flush: Option<Instant>,
    flush_pending: bool,
    _flush_timer: Option<Task<()>>,
    /// pane 根容器的窗口坐标(由 pane 侧的探针每帧报上来)。
    pane_bounds: Option<Bounds<Pixels>>,
    /// 悬浮框顶边的窗口坐标(悬浮框自己的探针报上来)。
    popover_top: Option<Pixels>,
    /// 上一次定下的「按内容」目标高度;量不出来时沿用它,免得高度一帧一跳。
    box_target: Option<f32>,
    last_box_h: Option<f32>,
    copied: Option<CopyTarget>,
    _copied_timer: Option<Task<()>>,
    /// 开合动画进度。`open` 是目标(按钮亮不亮看它),这里是画面:收起动画播完之前
    /// 悬浮框留在树上(与 `main.rs` 右抽屉 `DrawerExit` 的驻留同一个意思)。
    reveal: Reveal,
}

impl SshActivityView {
    pub fn new(pty_id: u32, terminal_focus: FocusHandle, mono_font: SharedString) -> Self {
        Self {
            pty_id,
            terminal_focus,
            mono_font,
            activity: Activity::default(),
            open: false,
            list: ListState::new(0, ListAlignment::Top, px(OVERDRAW)),
            synced: Vec::new(),
            last_flush: None,
            flush_pending: false,
            _flush_timer: None,
            pane_bounds: None,
            popover_top: None,
            box_target: None,
            last_box_h: None,
            copied: None,
            _copied_timer: None,
            reveal: Reveal::hidden(Instant::now()),
        }
    }

    /// 至少有一条记录(标题栏按钮显示的条件)。
    pub fn has_records(&self) -> bool {
        !self.activity.is_empty()
    }

    /// 标题栏按钮的状态;没有记录时 `None`(按钮不画)。
    pub fn button_state(&self) -> Option<ButtonState> {
        (!self.activity.is_empty()).then(|| ButtonState {
            count: self.activity.len(),
            running: self.activity.any_running(),
            failed: self.activity.last_failed(),
            open: self.open,
        })
    }

    /// 终端字族变了(设置页「字体」)。
    pub fn set_mono_font(&mut self, font: SharedString, cx: &mut Context<Self>) {
        if self.mono_font == font {
            return;
        }
        self.mono_font = font;
        if self.open {
            self.list.remeasure();
        }
        cx.notify();
    }

    /// pane 侧探针报上来的 pane 根容器窗口坐标。返回「变了且需要重画」。
    pub fn note_pane_bounds(&mut self, bounds: Bounds<Pixels>) -> bool {
        if self.pane_bounds == Some(bounds) {
            return false;
        }
        self.pane_bounds = Some(bounds);
        self.reveal.mounted_at(Instant::now())
    }

    /// 监听线程送来的一批事件(同一 pane)。
    ///
    /// 悬浮框关着时只有按钮状态真变了才刷新(标题栏随之重画);开着时列表内容变了也刷新。
    pub fn apply(&mut self, events: Vec<FeedEvent>, cx: &mut Context<Self>) {
        let now = Instant::now();
        let before = self.button_state();
        let mut applied = Applied::default();
        for event in events {
            applied.merge(self.activity.apply(event, now));
        }
        let button_changed = self.button_state() != before;
        if button_changed || (self.open && (applied.rows || applied.summary)) {
            self.schedule_flush(cx);
        }
    }

    /// 清掉全部记录(「清空」按钮 / PTY 关闭)。按钮随之消失、悬浮框直接关闭
    /// (没有内容可收,不播收起动画)。
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.activity.clear();
        self.synced.clear();
        self.list.reset(0);
        self.open = false;
        self.reveal = Reveal::hidden(Instant::now());
        self.copied = None;
        self.box_target = None;
        self.last_box_h = None;
        cx.notify();
    }

    fn schedule_flush(&mut self, cx: &mut Context<Self>) {
        if self.flush_pending {
            return;
        }
        let now = Instant::now();
        let since = self.last_flush.map(|at| now.saturating_duration_since(at));
        match since {
            Some(since) if since < REFRESH_PERIOD => {
                self.flush_pending = true;
                let wait = REFRESH_PERIOD - since;
                self._flush_timer = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(wait).await;
                    let _ = this.update(cx, |view: &mut SshActivityView, cx| {
                        view.flush_pending = false;
                        view.flush(cx);
                    });
                }));
            }
            _ => self.flush(cx),
        }
    }

    fn flush(&mut self, cx: &mut Context<Self>) {
        self.last_flush = Some(Instant::now());
        if self.open {
            sync_rows(&mut self.synced, &mut self.activity, &self.list);
        }
        cx.notify();
    }

    /// 标题栏按钮的落点:开 / 关悬浮框,然后把键盘焦点还给该 pane 的终端。
    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_open(!self.open, cx);
        window.focus(&self.terminal_focus, cx);
    }

    /// 开 / 关悬浮框,带开合动画(见 [`Reveal`])。
    fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        // 没有记录时没有按钮,也就不该开出一个空框
        if open == self.open || (open && self.activity.is_empty()) {
            return;
        }
        let now = Instant::now();
        self.open = open;
        if open {
            if self.reveal.mounted_at(now) {
                // 收起动画播到一半又点开:框还在树上,列表接着用(滚动位置不动),
                // 只把关着期间漏掉的变化增量补上
                sync_rows(&mut self.synced, &mut self.activity, &self.list);
            } else {
                let total = rebuild_rows(&mut self.synced, &mut self.activity);
                self.list.reset(total);
                self.list.set_follow_mode(FollowMode::Tail);
                self.last_box_h = None;
            }
        }
        let animate = animations_enabled(cx);
        self.reveal
            .retarget(if open { 1.0 } else { 0.0 }, now, animate);
        cx.notify();
    }

    fn copy(&mut self, target: CopyTarget, cx: &mut Context<Self>) {
        let text = match target {
            CopyTarget::All => self
                .activity
                .records()
                .map(record_text)
                .collect::<Vec<_>>()
                .join("\n"),
            CopyTarget::Command(seq) => match self.activity.record(seq) {
                Some(record) => match record.info.op {
                    Op::Exec => record.info.command.clone().unwrap_or_default(),
                    Op::Upload | Op::Download => main_line(record),
                },
                None => return,
            },
            CopyTarget::Output(seq) => match self.activity.record(seq) {
                Some(record) => record.output.text().to_string(),
                None => return,
            },
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = Some(target);
        self._copied_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |view: &mut SshActivityView, cx| {
                if view.copied == Some(target) {
                    view.copied = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// 悬浮框的宽与最大高。
    fn popover_limits(&self) -> (f32, f32) {
        let (pane_w, pane_h, pane_bottom) = match self.pane_bounds {
            Some(bounds) => (
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
                Some(f32::from(bounds.bottom())),
            ),
            // 探针还没报上来(打开后的第一帧之前):给个常见尺寸,下一帧就纠正
            None => (960.0, 640.0, None),
        };
        let width = (pane_w * POPOVER_W_RATIO)
            .clamp(POPOVER_MIN_W, POPOVER_MAX_W)
            .min((pane_w - 2.0 * PANE_EDGE).max(120.0));
        let mut max_h = pane_h * POPOVER_H_RATIO;
        if let (Some(bottom), Some(top)) = (pane_bottom, self.popover_top) {
            max_h = max_h.min(bottom - f32::from(top) - BOTTOM_GAP);
        }
        (width, max_h.max(80.0))
    }

    /// 悬浮框高度:内容少时按内容收缩(不低于最小高度),多了封顶在 `max_h`。
    ///
    /// 内容高度只能读上一帧的布局结果([`measure_content`]),所以高度变了要再要一帧;
    /// 量不出来时沿用上一次的目标,不回落到估算值 —— 高度不会一帧一跳。
    fn box_height(&mut self, max_h: f32, window: &mut Window) -> f32 {
        // 标题栏 + 列表上下内边距 + 上下边框 + 一点余量(让内容恰好装下时不出滚动)
        let chrome = TITLE_H + 2.0 * LIST_PAD_Y + 2.0 * POPOVER_BORDER + 4.0;
        let measured = measure_content(&self.list).map(|content| content + chrome);
        let target = measured.or(self.box_target).unwrap_or_else(|| {
            let rows = self.synced.iter().map(SyncedRecord::len).sum::<usize>() as f32;
            rows * EST_ROW_H + self.synced.len() as f32 * EST_RECORD_EXTRA + chrome
        });
        self.box_target = Some(target);
        let height = target.min(max_h).max(POPOVER_MIN_H.min(max_h));
        if self
            .last_box_h
            .is_some_and(|prev| (prev - height).abs() > 0.5)
        {
            window.request_animation_frame();
        }
        self.last_box_h = Some(height);
        height
    }

    fn render_title(&self, cx: &mut Context<Self>) -> Div {
        let pty_id = self.pty_id;
        let copied_all = self.copied == Some(CopyTarget::All);
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .h(px(TITLE_H))
            .px(px(12.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(VectorIcon::new(crate::activity_bar::SSH, px(16.0)).ink(ui::text_secondary()))
            .child(
                div()
                    .text_size(ui::font_px(14.0))
                    .text_color(ui::text_primary())
                    .child(t("sshActivity", "title")),
            )
            .child(
                div()
                    .px(px(8.0))
                    .rounded_full()
                    .bg(ui::border_subtle())
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_secondary())
                    .child(self.activity.len().to_string()),
            )
            .child(div().flex_1())
            .child(
                tool_button(
                    SharedString::from(format!("ssh-activity-copy-all-{pty_id}")),
                    if copied_all {
                        t("sshActivity", "copied")
                    } else {
                        t("sshActivity", "copyAll")
                    },
                    ui::text_primary(),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.copy(CopyTarget::All, cx);
                })),
            )
            .child(
                tool_button(
                    SharedString::from(format!("ssh-activity-clear-{pty_id}")),
                    t("sshActivity", "clear"),
                    ui::color_error(),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.clear(cx))),
            )
            .child(
                tool_button(
                    SharedString::from(format!("ssh-activity-close-{pty_id}")),
                    "✕",
                    ui::text_primary(),
                )
                .tip(t("sshActivity", "close"))
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.set_open(false, cx);
                })),
            )
    }

    /// 画悬浮框。`reveal` 是开合进度(0..1,已缓动)。
    ///
    /// # 开合动画用「裁剪窗」做
    ///
    /// gpui 的 div 没有缩放变换,于是分三层:
    /// 1. **外壳**按最终尺寸占位(右上角那一列的布局从头到尾不变;它没有 hitbox,
    ///    框体还没长到的地方鼠标照常落到终端上);
    /// 2. **框体**(底色 / 边框 / 阴影 / 拦鼠标)按 [`reveal_rect`] 插值出的矩形画,
    ///    `overflow_hidden` 当裁剪窗 —— 从顶边中点长到最终尺寸,收起时反向;
    /// 3. **内容**按最终尺寸排版,相对框体反向偏移、钉在最终矩形上,只被裁剪窗裁掉:
    ///    文字不重排不缩放,虚拟列表的滚动位置也不受影响。
    ///
    /// 整个浮层挂在 pane 内容区的绝对定位列里,终端布局与 PTY 尺寸都不动。
    fn render_popover(&mut self, reveal: f32, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let (width, max_h) = self.popover_limits();
        let height = self.box_height(max_h, window);
        let frame = reveal_rect(
            reveal,
            FrameRect {
                left: 0.0,
                top: 0.0,
                width,
                height,
            },
        );
        let idle_bg = {
            let base = ui::bg_terminal();
            ui::with_alpha(base, base.a * IDLE_BG_ALPHA)
        };
        // 量悬浮框顶边(外壳的,开合动画里不动):最大高要扣掉它上面的查找条,不能伸出 pane 底边
        let this = cx.entity();
        let probe = canvas(
            move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut gpui::App| {
                let changed = this.update(cx, |view: &mut SshActivityView, _| {
                    let changed = view.popover_top != Some(bounds.top());
                    view.popover_top = Some(bounds.top());
                    changed
                });
                if changed {
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let pty_id = self.pty_id;
        // 内容:最终尺寸(扣掉框体边框),相对框体的内容区反向偏移,窗口坐标始终等于全开时的位置
        let content = div()
            .absolute()
            .left(px(-frame.left))
            .top(px(-frame.top))
            .w(px(width - 2.0 * POPOVER_BORDER))
            .h(px(height - 2.0 * POPOVER_BORDER))
            .flex()
            .flex_col()
            .text_color(ui::text_primary())
            .child(self.render_title(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        list(self.list.clone(), cx.processor(Self::render_row))
                            .size_full()
                            .py(px(LIST_PAD_Y)),
                    )
                    // 滚动条套一层 absolute 四边贴 0 的壳(理由见 `file_viewer::preview` 同名段)
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .child(Scrollbar::vertical(&self.list).id(SharedString::from(
                                format!("ssh-activity-scrollbar-{pty_id}"),
                            ))),
                    ),
            );
        // 框体 = 裁剪窗
        let body = div()
            .id(SharedString::from(format!("ssh-activity-popover-{pty_id}")))
            .absolute()
            .left(px(frame.left))
            .top(px(frame.top))
            .w(px(frame.width))
            .h(px(frame.height))
            .overflow_hidden()
            .rounded(px(8.0))
            .border(px(POPOVER_BORDER))
            .border_color(ui::border_default())
            .bg(idle_bg)
            // 始终拦鼠标:滚轮滚的是框,点击不穿透到终端
            .occlude()
            .hover(|el| el.bg(ui::bg_elevated()).shadow_lg())
            .child(content);
        // 外壳:最终尺寸占位,不拦鼠标
        div()
            .relative()
            .flex_none()
            .w(px(width))
            .h(px(height))
            .child(probe)
            .child(body)
    }

    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((pos, kind)) = locate(&self.synced, ix) else {
            return div().into_any_element();
        };
        let seq = self.synced[pos].seq;
        // 列表与模型之间隔着节流:被挤掉还没同步出去的记录画成空项
        let Some(record) = self.activity.record(seq) else {
            return div().into_any_element();
        };
        match kind {
            RowKind::Header => self.render_header(record, pos == 0, cx),
            RowKind::Output(index) => self.render_output_row(record, index),
            RowKind::Status => self.render_status(record),
            RowKind::Truncated => div()
                .w_full()
                .px(px(ROW_PAD_X))
                .pb(px(12.0))
                .text_size(ui::font_px(13.0))
                .text_color(ui::color_warning())
                .child(truncated_line(record))
                .into_any_element(),
        }
    }

    fn render_header(&self, record: &Record, first: bool, cx: &Context<Self>) -> AnyElement {
        let seq = record.seq;
        let pty_id = self.pty_id;
        let group = SharedString::from(format!("ssh-activity-rec-{pty_id}-{seq}"));
        let copied_cmd = self.copied == Some(CopyTarget::Command(seq));
        let copied_out = self.copied == Some(CopyTarget::Output(seq));
        let is_exec = record.info.op == Op::Exec;
        let label = |copied: bool, key: &'static str| {
            if copied {
                t("sshActivity", "copied")
            } else {
                t("sshActivity", key)
            }
        };
        // 叠在记录头右上角、不占排版宽度:窄框(360px)里连接名一长,并排放会把命令挤没。
        // 悬停时整个框已是实底,按钮底色取同一个 bg_elevated,盖住命令行尾也不显脏
        let actions = div()
            .absolute()
            .top(px(if first { 6.0 } else { 8.0 }))
            .right(px(8.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .px(px(2.0))
            .rounded(px(4.0))
            .bg(ui::bg_elevated())
            // 平时藏着,悬停这条记录才显;刚点过复制时常亮,让「已复制」看得见
            .when(!copied_cmd && !copied_out, |el| {
                el.invisible().group_hover(group.clone(), |s| s.visible())
            })
            .child(
                tool_button(
                    SharedString::from(format!("ssh-activity-copy-cmd-{pty_id}-{seq}")),
                    label(copied_cmd, "copyCommand"),
                    ui::text_primary(),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.copy(CopyTarget::Command(seq), cx);
                })),
            )
            .when(is_exec, |el| {
                el.child(
                    tool_button(
                        SharedString::from(format!("ssh-activity-copy-out-{pty_id}-{seq}")),
                        label(copied_out, "copyOutput"),
                        ui::text_primary(),
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _window, cx| {
                            this.copy(CopyTarget::Output(seq), cx);
                        },
                    )),
                )
            });
        div()
            .group(group)
            .relative()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(ROW_PAD_X))
            .pt(px(if first { 10.0 } else { 12.0 }))
            .pb(px(6.0))
            .when(!first, |el| {
                el.border_t_1().border_color(ui::border_subtle())
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_start()
                    .gap(px(8.0))
                    .child(connection_chip(&record.info.connection))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .font_family(self.mono_font.clone())
                            .text_size(ui::font_px(13.0))
                            .line_height(relative(1.5))
                            .text_color(ui::text_primary())
                            .child(main_line(record)),
                    ),
            )
            .when_some(record.info.cwd.as_deref(), |el, cwd| {
                el.child(
                    div()
                        .font_family(self.mono_font.clone())
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_muted())
                        .child(tr!("sshActivity", "cwd", cwd = cwd)),
                )
            })
            .when_some(record.info.timeout_secs, |el, secs| {
                el.child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_muted())
                        .child(tr!("sshActivity", "timeoutArg", secs = secs)),
                )
            })
            .child(actions)
            .into_any_element()
    }

    fn render_output_row(&self, record: &Record, index: usize) -> AnyElement {
        let row = div()
            .w_full()
            .px(px(ROW_PAD_X))
            .font_family(self.mono_font.clone())
            .text_size(ui::font_px(13.0))
            .line_height(relative(1.45))
            .text_color(ui::text_primary());
        let Some(range) = record.output.row_range(index) else {
            return row.into_any_element();
        };
        let text = &record.output.text()[range.clone()];
        if text.is_empty() {
            // 空行也要占一行高
            return row.child(" ").into_any_element();
        }
        let stderr = ui::color_warning();
        let highlights: Vec<_> = record
            .output
            .spans(range)
            .into_iter()
            .filter(|(_, stream)| *stream == Stream::Stderr)
            .map(|(span, _)| {
                (
                    span,
                    HighlightStyle {
                        color: Some(stderr),
                        ..Default::default()
                    },
                )
            })
            .collect();
        row.child(StyledText::new(SharedString::from(text.to_string())).with_highlights(highlights))
            .into_any_element()
    }

    fn render_status(&self, record: &Record) -> AnyElement {
        let (text, tone) = status_line(record);
        let color = match tone {
            Tone::Running => ui::accent(),
            Tone::Normal => ui::text_secondary(),
            Tone::Error => ui::color_error(),
        };
        div()
            .w_full()
            .px(px(ROW_PAD_X))
            .pt(px(4.0))
            .pb(px(if record.truncated { 2.0 } else { 12.0 }))
            .text_size(ui::font_px(13.0))
            .text_color(color)
            .child(text)
            .into_any_element()
    }
}

/// 标题栏 / 记录头上的文字小按钮。`hover_color` 是悬停时的文字色(「清空」悬停转红)。
fn tool_button(
    id: SharedString,
    label: impl Into<SharedString>,
    hover_color: gpui::Hsla,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(26.0))
        .px(px(8.0))
        .rounded(px(4.0))
        .text_size(ui::font_px(13.0))
        .text_color(ui::text_secondary())
        .cursor_pointer()
        .hover(move |el| el.bg(ui::border_subtle()).text_color(hover_color))
        .child(label.into())
}

/// 连接名胶囊。
fn connection_chip(name: &str) -> Div {
    div()
        .flex_none()
        .max_w(px(200.0))
        .min_w_0()
        .truncate()
        .px(px(8.0))
        .rounded_full()
        .border_1()
        .border_color(ui::border_default())
        .bg(ui::accent_subtle())
        .text_size(ui::font_px(13.0))
        .line_height(relative(1.5))
        .text_color(ui::text_secondary())
        .child(name.to_string())
}

impl Render for SshActivityView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 按钮在终端组标题栏上(`terminal_area::render_leaf_controls`),这里只画悬浮框;
        // 关着时画空 div 也要挂着,理由见模块注释「挂在哪」。收起动画播完之前框还留在树上
        let now = Instant::now();
        if self.activity.is_empty() || !self.reveal.mounted_at(now) {
            return div().into_any_element();
        }
        // 只在开合动画进行中要帧,播完就停(不用 `with_animation(..).repeat()`)
        if self.reveal.running_at(now) {
            window.request_animation_frame();
        }
        let reveal = self.reveal.value_at(now);
        self.render_popover(reveal, window, cx).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_activity::wire::{EndInfo, StartInfo};

    fn start(call: u64) -> FeedEvent {
        FeedEvent::Start {
            pty_id: 1,
            call,
            info: StartInfo {
                op: Op::Exec,
                connection: "c".into(),
                command: Some("ls".into()),
                cwd: None,
                timeout_secs: None,
                local_path: None,
                remote_path: None,
            },
        }
    }

    fn out(call: u64, data: &[u8]) -> FeedEvent {
        FeedEvent::Output {
            pty_id: 1,
            call,
            stream: Stream::Stdout,
            data: data.to_vec(),
        }
    }

    fn end(call: u64) -> FeedEvent {
        FeedEvent::End {
            pty_id: 1,
            call,
            end: EndInfo {
                exit_code: Some(0),
                ..Default::default()
            },
            dropped: false,
        }
    }

    fn total(synced: &[SyncedRecord]) -> usize {
        synced.iter().map(SyncedRecord::len).sum()
    }

    /// 同步之后列表项数与模型一致,且映射首尾相接。
    fn check(synced: &[SyncedRecord], activity: &Activity, list: &ListState) {
        let expected: usize = activity.records().map(Record::row_count).sum();
        assert_eq!(list.item_count(), expected);
        assert_eq!(total(synced), expected);
        let mut cursor = 0;
        for (entry, record) in synced.iter().zip(activity.records()) {
            assert_eq!(entry.seq, record.seq);
            assert_eq!(entry.start, cursor);
            cursor += entry.len();
        }
    }

    #[test]
    fn 列表增量同步跟得上模型() {
        let mut activity = Activity::default();
        let list = ListState::new(0, ListAlignment::Top, px(100.0));
        let mut synced = Vec::new();
        let now = Instant::now();

        activity.apply(start(1), now);
        sync_rows(&mut synced, &mut activity, &list);
        check(&synced, &activity, &list);
        assert_eq!(list.item_count(), 2, "头部 + 运行中状态行");

        activity.apply(out(1, b"a\nb"), now);
        activity.apply(start(2), now);
        sync_rows(&mut synced, &mut activity, &list);
        check(&synced, &activity, &list);

        activity.apply(out(1, b"c\nd\ne\n"), now);
        activity.apply(end(1), now);
        activity.apply(out(2, b"x"), now);
        sync_rows(&mut synced, &mut activity, &list);
        check(&synced, &activity, &list);

        // 没有变化时再同步一次:什么都不动
        let before = list.item_count();
        sync_rows(&mut synced, &mut activity, &list);
        assert_eq!(list.item_count(), before);

        // 中间一条被挤掉 / 清空
        activity.clear();
        sync_rows(&mut synced, &mut activity, &list);
        check(&synced, &activity, &list);
        assert_eq!(list.item_count(), 0);
    }

    #[test]
    fn 被上限挤掉的记录从列表里摘掉() {
        let mut activity = Activity::default();
        let list = ListState::new(0, ListAlignment::Top, px(100.0));
        let mut synced = Vec::new();
        let now = Instant::now();
        for call in 0..(crate::ssh_activity::model::PANE_RECORD_CAP as u64) {
            activity.apply(start(call), now);
        }
        sync_rows(&mut synced, &mut activity, &list);
        check(&synced, &activity, &list);
        for call in 1000..1010 {
            activity.apply(start(call), now);
            activity.apply(out(call, b"line\n"), now);
        }
        sync_rows(&mut synced, &mut activity, &list);
        check(&synced, &activity, &list);
    }

    #[test]
    fn 重建与定位() {
        let mut activity = Activity::default();
        let now = Instant::now();
        activity.apply(start(1), now);
        activity.apply(out(1, b"a\nb\n"), now);
        activity.apply(
            FeedEvent::End {
                pty_id: 1,
                call: 1,
                end: EndInfo {
                    truncated: true,
                    ..Default::default()
                },
                dropped: false,
            },
            now,
        );
        activity.apply(start(2), now);
        let mut synced = Vec::new();
        let total = rebuild_rows(&mut synced, &mut activity);
        // 记录 1:头 + 2 行输出 + 状态 + 截断;记录 2:头 + 状态
        assert_eq!(total, 5 + 2);
        assert_eq!(locate(&synced, 0), Some((0, RowKind::Header)));
        assert_eq!(locate(&synced, 1), Some((0, RowKind::Output(0))));
        assert_eq!(locate(&synced, 2), Some((0, RowKind::Output(1))));
        assert_eq!(locate(&synced, 3), Some((0, RowKind::Status)));
        assert_eq!(locate(&synced, 4), Some((0, RowKind::Truncated)));
        assert_eq!(locate(&synced, 5), Some((1, RowKind::Header)));
        assert_eq!(locate(&synced, 6), Some((1, RowKind::Status)));
        assert_eq!(locate(&synced, 7), None);
        assert!(
            activity.records_mut().all(|r| r.take_dirty().is_none()),
            "重建顺带清掉脏行"
        );
    }

    #[test]
    fn 开合矩形从顶边中点展开() {
        let target = FrameRect {
            left: 10.0,
            top: 20.0,
            width: 400.0,
            height: 300.0,
        };
        // 起点:顶边水平居中处的小框
        let start = reveal_rect(0.0, target);
        assert_eq!(start.width, 400.0 * REVEAL_START_W);
        assert_eq!(start.height, 300.0 * REVEAL_START_H);
        assert_eq!(start.top, 20.0);
        assert_eq!(start.left, 10.0 + (400.0 - start.width) / 2.0);
        // 终点就是终矩形
        assert_eq!(reveal_rect(1.0, target), target);
        // 越界进度钳住
        assert_eq!(reveal_rect(1.7, target), target);
        assert_eq!(reveal_rect(-0.3, target), start);
        // 全程:顶边不动、水平中线不动、宽高单调长大
        let mut prev = start;
        for step in 1..=20 {
            let rect = reveal_rect(step as f32 / 20.0, target);
            assert_eq!(rect.top, target.top, "高度从上往下展开,顶边不动");
            let center = rect.left + rect.width / 2.0;
            assert!((center - 210.0).abs() < 1e-3, "宽度从中间向两边展开");
            assert!(rect.width > prev.width && rect.height > prev.height);
            assert!(rect.width <= target.width && rect.height <= target.height);
            prev = rect;
        }
    }

    #[test]
    fn 开合进度与半路掉头() {
        let t0 = Instant::now();
        let mut reveal = Reveal::hidden(t0);
        assert!(!reveal.mounted_at(t0));

        // 打开:ease-out,从 0 单调走到 1,跑满 REVEAL_OPEN 就停
        reveal.retarget(1.0, t0, true);
        assert_eq!(reveal.value_at(t0), 0.0);
        assert!(reveal.running_at(t0) && reveal.mounted_at(t0));
        let mut prev = 0.0;
        for ms in (10..=220).step_by(10) {
            let v = reveal.value_at(t0 + Duration::from_millis(ms));
            assert!(v >= prev, "打开过程单调");
            prev = v;
        }
        let opened = t0 + REVEAL_OPEN;
        assert_eq!(reveal.value_at(opened), 1.0);
        assert!(!reveal.running_at(opened), "播完就不再要帧");

        // 半路掉头:打开到一半再点,从当前进度反向收,不跳变
        let mut reveal = Reveal::hidden(t0);
        reveal.retarget(1.0, t0, true);
        let mid = t0 + Duration::from_millis(80);
        let before = reveal.value_at(mid);
        assert!(before > 0.0 && before < 1.0);
        reveal.retarget(0.0, mid, true);
        assert!(
            (reveal.value_at(mid) - before).abs() < 1e-6,
            "掉头那一刻不跳变"
        );
        // 只走回去那一段:时长按剩余路程折算
        let back = mid + REVEAL_CLOSE.mul_f32(before);
        assert!(reveal.running_at(back - Duration::from_millis(1)));
        assert_eq!(reveal.value_at(back), 0.0);
        assert!(!reveal.running_at(back));
        assert!(!reveal.mounted_at(back), "收起播完才从树上卸下");

        // 收起途中再点开:同样从当前进度接着往回长
        let mut reveal = Reveal::hidden(t0);
        reveal.retarget(1.0, t0, false);
        reveal.retarget(0.0, t0, true);
        let half = t0 + Duration::from_millis(70);
        let shown = reveal.value_at(half);
        assert!(shown > 0.0 && shown < 1.0 && reveal.mounted_at(half));
        reveal.retarget(1.0, half, true);
        assert!((reveal.value_at(half) - shown).abs() < 1e-6);
    }

    #[test]
    fn 动画总开关关掉时直接开合() {
        let t0 = Instant::now();
        let mut reveal = Reveal::hidden(t0);
        reveal.retarget(1.0, t0, false);
        assert_eq!(reveal.value_at(t0), 1.0);
        assert!(!reveal.running_at(t0));
        reveal.retarget(0.0, t0, false);
        assert_eq!(reveal.value_at(t0), 0.0);
        assert!(!reveal.mounted_at(t0));
    }

    #[test]
    fn 耗时格式() {
        assert_eq!(format_elapsed(Duration::from_millis(1234)), "1.2s");
        assert_eq!(format_elapsed(Duration::from_millis(50)), "0.1s");
        assert_eq!(format_elapsed(Duration::from_secs(185)), "3m05s");
    }

    #[test]
    fn 状态行色调() {
        let mut activity = Activity::default();
        let now = Instant::now();
        activity.apply(start(1), now);
        assert_eq!(
            status_line(activity.records().next().unwrap()).1,
            Tone::Running
        );
        activity.apply(end(1), now);
        assert_eq!(
            status_line(activity.records().next().unwrap()).1,
            Tone::Normal
        );

        let cases = [
            EndInfo {
                exit_code: Some(1),
                ..Default::default()
            },
            EndInfo {
                timed_out: true,
                ..Default::default()
            },
            EndInfo {
                not_started: true,
                ..Default::default()
            },
            EndInfo {
                error: Some("auth failed".into()),
                ..Default::default()
            },
        ];
        for (i, end) in cases.into_iter().enumerate() {
            let call = 10 + i as u64;
            activity.apply(start(call), now);
            activity.apply(
                FeedEvent::End {
                    pty_id: 1,
                    call,
                    end,
                    dropped: false,
                },
                now,
            );
            let record = activity.records().last().unwrap();
            assert_eq!(status_line(record).1, Tone::Error);
            assert!(record.failed());
        }
        let record = activity.records().last().unwrap();
        assert!(
            status_line(record).0.contains("auth failed"),
            "错误原文照登"
        );
        assert!(record_text(record).starts_with("[c] $ ls\n"));
    }
}
