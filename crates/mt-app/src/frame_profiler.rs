//! **dev-only 帧剖析叠层**:gpui-pre 内置 `DebugFrameOverlay` 的开关,外加一份
//! 从 gpui 前台 journal 抽出来的周期汇总。给 `docs/` 那条 GPU/重绘性能诊断线
//! 当量尺用 —— 此前「陈旧感 / 节拍器有没有生效」只能靠任务管理器的 GPU% 和肉眼。
//!
//! **正式版零代码零依赖**:整个模块连同 `main.rs` 里那唯一一处调用都挂在
//! `frame-profiler` feature 下。不开时 `gpui/profiler` 不启用,上游那套
//! `Window::set_debug_frame_overlay_mode` / `App::foreground_journal` /
//! `WindowInvalidator` 的计数字段全部不编译,`hdrhistogram` 也不进依赖树。
//!
//! # 怎么用
//!
//! ```bash
//! # 1. 带 feature 编译(会把 gpui-pre 连同依赖它的 crate 重编一遍,几分钟)
//! cargo build -p mt-app --features frame-profiler
//!
//! # 2. 带环境变量启动(⚠️ 与装机版并跑时照例要隔离数据目录)
//! MT_FRAME_OVERLAY=1 MT_APP_DATA_DIR="$LOCALAPPDATA/mini-term-gpui-dev" \
//!   ./target/debug/mini-term
//! ```
//!
//! feature 开着但环境变量没设时,本模块**一行都不跑**(`install` 第一句就返回),
//! 所以带 feature 的构建可以当普通 dev 实例用。
//!
//! # 环境变量
//!
//! | 变量 | 取值 | 含义 |
//! |------|------|------|
//! | `MT_FRAME_OVERLAY` | 不设 / `0` / `off` / `false` / `no` / 空串 | 整套工具不装(默认) |
//! | | `1` / `on` / `true` / `full` | 右上角画**详细档**叠层 + 周期汇总 |
//! | | `minimal` / `min` | 右上角画**极简档**叠层(只有当前帧耗时)+ 周期汇总 |
//! | | `hidden` / `report` | **不画**叠层,只在 stderr 出周期汇总 |
//! | `MT_FRAME_REPORT` | 不设 | 汇总周期取默认 [`DEFAULT_REPORT_PERIOD`] |
//! | | `0` / 负数 / 认不出的值 | 关掉周期汇总,只留叠层 |
//! | | 秒数(可带小数,如 `0.5` / `10`) | 汇总周期,钳在 [`MIN_REPORT_PERIOD`] ~ 1 小时 |
//!
//! stderr 在装机形态下由 [`crate::logfile`] 接到 `mini-term.log`;`cargo run`
//! 起的 dev 实例有控制台,汇总直接滚在终端里。
//!
//! # 叠层各档显示什么
//!
//! 叠层由上游逐像素画进 scene(`debug_overlay.rs` 自带 5×7 位图字形),**绕开
//! 布局与 view 失效**,所以它自己不会引发新的一帧 —— 这是它能当量尺的前提。
//! 画在窗口右上角,绿字黑底。
//!
//! - **极简档**(`DebugFrameOverlayMode::Minimal`):一行 `  3.2 MS` ——
//!   最近一帧 `Window::draw` 的耗时。
//! - **详细档**(`DebugFrameOverlayMode::Full`):五行
//!   ```text
//!   CUR    3.2 MS   最近一帧
//!   1%    12.4 MS   最近 1000 帧里最慢的 1%(即 p99)
//!   10%    6.1 MS   最近 1000 帧里最慢的 10%(即 p90)
//!   MAX   41.0 MS   最近 1000 帧的最大值
//!   FRAMES   8421   本窗口自建成以来画过的总帧数(超过 99999 显示 LOTS)
//!   ```
//!   样本窗口是最近 1000 帧(`debug_overlay.rs:32` 的 `MAX_SAMPLES`),
//!   `FRAMES` 不随样本窗口滚动。
//!
//! 叠层量的是 **`Window::draw` 的 CPU 时间**(请求布局 + prepaint + paint,
//! 全在主线程),**不含** GPU 提交(`present`)与等 vsync。所以「CUR 很小但画面
//! 仍然黏」要去看下面汇总里的 `呈现间隔`,不是这一行。
//!
//! # 周期汇总怎么读
//!
//! 每 [`DEFAULT_REPORT_PERIOD`] 往 stderr 打一行,内容取自 gpui 前台 journal
//! (`App::foreground_journal`),**只统计这一拍之内**真正呈现出去的帧:
//!
//! ```text
//! [frame-profiler] 2.0s | 帧 61 (30.5/s) | draw p50 1.8 p99 6.4 max 7.1 ms
//!                  | dirty→present p50 3.1 p99 9.8 ms | invalidation/帧 均 2.4 峰 11
//!                  | 呈现间隔 p50 33.2 ms
//! ```
//!
//! - **帧 N (M/s)**:这一拍呈现了多少帧。空闲桌面上应当是 `空闲(0 帧)`;
//!   终端刷屏时应当贴着设置页的前台帧率(默认 30/s),**不是** 60/74。
//!   前台帧率没调高却贴着屏幕刷新率,就说明有人绕过 [`crate::redraw`] 在直接
//!   `cx.notify()`。
//! - **draw p50/p99/max**:每帧 CPU。与叠层的 `CUR`/`1%` 同一个量,只是窗口
//!   换成了「这一拍」,于是可以对着一次具体操作(拖选、滚屏)读数。
//! - **dirty→present**:从这一帧**第一次**被置脏到提交给平台的墙钟时间。
//!   它减去 `draw` 就是「攒着等下一拍 + 等 vsync」的部分 —— 节拍器把重绘
//!   合并到 33ms 一拍,这里天然会比 `draw` 大一截,**那不是回归**。
//! - **invalidation/帧**:本帧合并了几次失效请求(上游 `FrameDirtyAccumulator`,
//!   每次 `cx.notify()` / `Window::refresh` 记一次)。**这是节拍器与动画泵的
//!   效果验证位**,见下一节。
//! - **呈现间隔 p50**:上游只在「窗口活跃 + 上一帧还排着下一帧」时记录,
//!   所以它约等于动画/连续刷屏期间的真实帧间隔(33ms ≈ 30fps)。静止时没有
//!   样本,该字段会整段消失。
//! - ⚠️ 百分位用的是与叠层同一条**向下取整的最近秩**(见 [`percentile`]),
//!   样本少时偏低。一拍只有个位数帧时别对着 p99 下结论,读 `帧 N` 与 `max`。
//! - **journal 丢失 N**:journal 是定长环(4 MiB),一拍里事件太多会覆盖。
//!   只在 N>0 时出现,出现了就说明这一拍的百分位偏乐观,把
//!   `MT_FRAME_REPORT` 调小重测。
//!
//! # 与 [`crate::redraw`] 节拍器 / [`mt_ui::motion`] 泵的关系
//!
//! GPUI 没有局部重绘:一次 `cx.notify()` = 整窗重画(见 [`crate::redraw`] 的
//! 模块注释)。节拍器做的事就是**把一拍内所有 pane 的 notify 攒起来,在同一次
//! App 更新里一起 flush**,让 GPUI 合成一帧;`mt_ui::motion` 的过渡同理,
//! 靠「到终态就不再请求帧」避免空转。
//!
//! 这两件事的效果**正好就是 `invalidation/帧` 这个数**:
//!
//! - 7 个 pane 同时刷屏、节拍器生效 → `帧 ≈ 30/s`,`invalidation/帧` 明显 > 1
//!   (一拍里攒了几个 pane 就是几),这正是「合并」两个字的字面含义。
//! - 如果 `invalidation/帧 ≈ 1.0` 而 `帧` 贴着屏幕刷新率 → **没合并成**:
//!   每个请求各自换来一帧,回到了节拍器诞生前那个形态。
//! - 动画播完之后 `帧` 必须落回 0;落不回去就是有泵没停(`Transition` 没到
//!   终态 / 有人每帧 `refresh`)。
//!
//! # 上游 API 出处(gpui-pre 0.3.5)
//!
//! - `Cargo.toml:67` —— `profiler = ["dep:hdrhistogram"]`,非默认 feature。
//! - `src/debug_overlay.rs` —— 叠层本体与 `DebugFrameOverlayMode`(整个模块
//!   由 `src/gpui.rs:19` 的 `#[cfg(feature = "profiler")]` 挂着)。
//! - `src/window.rs:3359-3385` —— `Window::{debug_frame_overlay_mode,
//!   set_debug_frame_overlay_mode, cycle_debug_frame_overlay_mode,
//!   reset_debug_frame_overlay_stats}`,全部 `#[cfg(feature = "profiler")]`。
//!   上游**没有**给叠层绑任何快捷键或 action,只能由宿主自己调。
//! - `src/window.rs:120-142` —— `FrameDirtyAccumulator`:`dirty_at` +
//!   `invalidations`,在 `end_draw` 时随帧落进 journal。
//! - `src/profiler/journal.rs` —— 前台 journal;`App::foreground_journal`
//!   (`src/app.rs:2002`)给出 `ForegroundJournal`,`collector()` 各自持游标。
//!
//! `gpui::profiler::set_trace_enabled` 这里**刻意不调**:它管的是每线程任务
//! 计时缓冲与 `FRAME_TIMINGS` 的**留存**(`src/profiler.rs:700-707` 的注释原话),
//! 而 journal 的 draw/present 记录是无条件写的,本模块要的两个量都在 journal 里。
//! 少开一个全局开关就少一份与被测对象抢主线程的开销。

use std::time::Duration;

use gpui::profiler::journal::{ForegroundJournalEntry, IntervalBoundary};
use gpui::{App, DebugFrameOverlayMode, WindowHandle};
use gpui_component::Root;

/// 叠层档位开关。
const OVERLAY_ENV: &str = "MT_FRAME_OVERLAY";
/// 周期汇总的秒数。
const REPORT_ENV: &str = "MT_FRAME_REPORT";

/// 没设 `MT_FRAME_REPORT` 时的汇总周期。
///
/// 取 2s 而不是更长:journal 是定长环,一拍攒得越久越容易丢事件(丢了百分位
/// 就偏乐观);2s 又足够长到能覆盖一次拖选/一屏滚动,读数不至于跳。
pub const DEFAULT_REPORT_PERIOD: Duration = Duration::from_secs(2);
/// 汇总周期的下限。再短的话打日志本身就成了被测负载的一部分。
pub const MIN_REPORT_PERIOD: Duration = Duration::from_millis(200);
/// 汇总周期的上限(1 小时)。纯粹挡住手滑输进来的天文数字。
const MAX_REPORT_PERIOD: Duration = Duration::from_secs(3600);

/// `MT_FRAME_OVERLAY` 的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setting {
    /// 没设 / 显式关:整套工具一行都不跑。
    Off,
    /// 开。`Hidden` 档表示「只要汇总,不画叠层」。
    On(DebugFrameOverlayMode),
    /// 设了但认不出:按关处理,并把可用取值喊出来 —— 拼错了却静默当没开,
    /// 是最难查的那种「怎么没数」。
    Unknown,
}

/// 装上叠层与周期汇总。`main.rs` 在 `open_window` 成功之后调这一处。
///
/// 需要窗口句柄:叠层状态住在 `Window` 上(`window.rs:1231`),不是 App 级的。
pub(crate) fn install(window: &WindowHandle<Root>, cx: &mut App) {
    let raw = std::env::var(OVERLAY_ENV).ok();
    let mode = match parse_setting(raw.as_deref()) {
        Setting::Off => return,
        Setting::Unknown => {
            eprintln!(
                "[frame-profiler] 认不出 {OVERLAY_ENV}={:?},按关闭处理;可用取值:\
                 1 / minimal / hidden / 0",
                raw.unwrap_or_default()
            );
            return;
        }
        Setting::On(mode) => mode,
    };

    if mode != DebugFrameOverlayMode::Hidden
        && window
            .update(cx, |_, window, _| {
                window.set_debug_frame_overlay_mode(mode);
            })
            .is_err()
    {
        eprintln!("[frame-profiler] 窗口已关闭,叠层未装上");
        return;
    }

    let period = parse_period(std::env::var(REPORT_ENV).ok().as_deref());
    eprintln!(
        "[frame-profiler] 叠层 {mode:?};周期汇总 {}",
        match period {
            Some(period) => format!("{:.1}s", period.as_secs_f64()),
            None => format!("关(设 {REPORT_ENV}=<秒> 打开)"),
        }
    );
    if let Some(period) = period {
        spawn_reporter(*window, cx, period);
    }
}

/// 起一条前台循环,每 `period` 抽干 journal 打一行。
///
/// 必须在前台:`ForegroundJournalCollector` 的游标是给单个消费者用的,而
/// 被测对象(draw / present)本来就全在主线程 —— 抽干动作放主线程才不会
/// 与写入方争用,代价是每 `period` 多一次主线程唤醒(可忽略)。
fn spawn_reporter(window: WindowHandle<Root>, cx: &mut App, period: Duration) {
    let journal = cx.foreground_journal();
    cx.spawn(async move |cx| {
        let mut collector = journal.collector();
        loop {
            cx.background_executor().timer(period).await;
            // 窗口没了就收摊(叠层状态跟着窗口走,再打也没有意义)。
            if window.update(cx, |_, _, _| ()).is_err() {
                return;
            }
            let drained = collector.collect_unseen();
            let mut sample = Sample::default();
            for entry in drained.entries {
                sample.absorb(entry);
            }
            eprintln!("{}", sample.render(period, drained.lost));
        }
    })
    .detach();
}

/// 一拍之内呈现出去的那些帧。
#[derive(Default)]
struct Sample {
    /// 每帧 `Window::draw` 的 CPU 耗时。
    draw: Vec<Duration>,
    /// 每帧「第一次置脏 → 提交给平台」的墙钟耗时。
    dirty_to_present: Vec<Duration>,
    /// 连续呈现时的帧间隔(上游只在窗口活跃且还排着下一帧时记)。
    present_interval: Vec<Duration>,
    /// 每帧合并掉的失效请求数。
    invalidations: Vec<u64>,
}

impl Sample {
    fn absorb(&mut self, entry: ForegroundJournalEntry) {
        // 只认「已呈现」这一种边界:画了但没提交的帧不该进帧率统计,
        // `Idle` 边界压根没有帧。
        let ForegroundJournalEntry::Boundary(IntervalBoundary::Presented(presented)) = entry else {
            return;
        };
        self.draw.push(presented.frame.draw_duration());
        self.invalidations.push(presented.frame.invalidations);
        if let Some(elapsed) = presented.dirty_to_present_duration() {
            self.dirty_to_present.push(elapsed);
        }
        if let Some(interval) = presented.presentation.animation_interval {
            self.present_interval.push(interval);
        }
    }

    fn render(&mut self, period: Duration, lost: u64) -> String {
        let seconds = period.as_secs_f64();
        let frames = self.draw.len();
        if frames == 0 {
            // 空闲也是结论:节拍器/动画泵停干净了才会一帧都没有。
            return format!(
                "[frame-profiler] {seconds:.1}s | 空闲(0 帧){}",
                lost_tail(lost)
            );
        }

        self.draw.sort_unstable();
        self.dirty_to_present.sort_unstable();
        self.present_interval.sort_unstable();

        let mut line = format!(
            "[frame-profiler] {seconds:.1}s | 帧 {frames} ({:.1}/s) | draw p50 {} p99 {} max {} ms",
            frames as f64 / seconds,
            ms(percentile(&self.draw, 50)),
            ms(percentile(&self.draw, 99)),
            ms(self.draw.last().copied()),
        );
        if !self.dirty_to_present.is_empty() {
            line.push_str(&format!(
                " | dirty→present p50 {} p99 {} ms",
                ms(percentile(&self.dirty_to_present, 50)),
                ms(percentile(&self.dirty_to_present, 99)),
            ));
        }
        let merged: u64 = self.invalidations.iter().sum();
        line.push_str(&format!(
            " | invalidation/帧 均 {:.1} 峰 {}",
            merged as f64 / frames as f64,
            self.invalidations.iter().copied().max().unwrap_or(0),
        ));
        if !self.present_interval.is_empty() {
            line.push_str(&format!(
                " | 呈现间隔 p50 {} ms",
                ms(percentile(&self.present_interval, 50)),
            ));
        }
        line.push_str(&lost_tail(lost));
        line
    }
}

fn lost_tail(lost: u64) -> String {
    if lost == 0 {
        String::new()
    } else {
        format!(" | journal 丢失 {lost}(百分位偏乐观,把 {REPORT_ENV} 调小)")
    }
}

/// 已排序切片的百分位,取 `(len - 1) * numerator / 100` 位。
///
/// 口径与上游叠层逐字一致(`debug_overlay.rs:171-173`)——两边读数得能对上,
/// 否则「叠层的 1% 和汇总的 p99 差了一截」这种问号要白查一轮。
///
/// ⚠️ 这条公式是**向下取整的最近秩**,样本少时会严重偏低:`len == 2` 时
/// `p99` 落在 `sorted[0]`(即最小值)。一拍只有个位数帧时(空闲、或周期调得
/// 太短)百分位没有意义 —— 那种情况读 `帧 N` 与 `max` 就够了,别对着 p99
/// 下结论。不改成向上取整是为了保住与叠层的可对账性。
fn percentile(sorted: &[Duration], numerator: usize) -> Option<Duration> {
    (!sorted.is_empty()).then(|| sorted[(sorted.len() - 1) * numerator / 100])
}

fn ms(duration: Option<Duration>) -> String {
    match duration {
        Some(duration) => format!("{:.1}", duration.as_secs_f64() * 1000.0),
        None => "--".into(),
    }
}

fn parse_setting(raw: Option<&str>) -> Setting {
    let Some(raw) = raw else {
        return Setting::Off;
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "off" | "false" | "no" => Setting::Off,
        "1" | "on" | "true" | "full" | "detailed" => Setting::On(DebugFrameOverlayMode::Full),
        "2" | "min" | "minimal" => Setting::On(DebugFrameOverlayMode::Minimal),
        "hidden" | "report" | "log" => Setting::On(DebugFrameOverlayMode::Hidden),
        _ => Setting::Unknown,
    }
}

/// `None` = 不汇总。认不出的值按「不汇总」处理而不是回落默认值 —— 与
/// [`parse_setting`] 同一条口径:宁可显式没有,不要偷偷给个别的数。
fn parse_period(raw: Option<&str>) -> Option<Duration> {
    let Some(raw) = raw else {
        return Some(DEFAULT_REPORT_PERIOD);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Some(DEFAULT_REPORT_PERIOD);
    }
    let seconds: f64 = raw.parse().ok()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(seconds).clamp(MIN_REPORT_PERIOD, MAX_REPORT_PERIOD))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 没设环境变量时整套工具不装() {
        assert_eq!(parse_setting(None), Setting::Off);
    }

    #[test]
    fn 开关取值逐条对账() {
        for raw in ["0", "off", "OFF", " false ", "no", ""] {
            assert_eq!(parse_setting(Some(raw)), Setting::Off, "{raw:?}");
        }
        for raw in ["1", "on", "True", "full", "detailed"] {
            assert_eq!(
                parse_setting(Some(raw)),
                Setting::On(DebugFrameOverlayMode::Full),
                "{raw:?}"
            );
        }
        for raw in ["2", "min", "Minimal"] {
            assert_eq!(
                parse_setting(Some(raw)),
                Setting::On(DebugFrameOverlayMode::Minimal),
                "{raw:?}"
            );
        }
        for raw in ["hidden", "report", "log"] {
            assert_eq!(
                parse_setting(Some(raw)),
                Setting::On(DebugFrameOverlayMode::Hidden),
                "{raw:?}"
            );
        }
        // 拼错了必须能被认出来是拼错,不能静默当关掉。
        assert_eq!(parse_setting(Some("ful")), Setting::Unknown);
    }

    #[test]
    fn 汇总周期解析与钳位() {
        assert_eq!(parse_period(None), Some(DEFAULT_REPORT_PERIOD));
        assert_eq!(parse_period(Some("  ")), Some(DEFAULT_REPORT_PERIOD));
        assert_eq!(parse_period(Some("0.5")), Some(Duration::from_millis(500)));
        assert_eq!(parse_period(Some("10")), Some(Duration::from_secs(10)));
        // 下限:比它短的一律抬到下限,不允许日志本身变成负载。
        assert_eq!(parse_period(Some("0.001")), Some(MIN_REPORT_PERIOD));
        assert_eq!(parse_period(Some("999999")), Some(MAX_REPORT_PERIOD));
        // 关掉 / 认不出:都只是不汇总,叠层照开。
        assert_eq!(parse_period(Some("0")), None);
        assert_eq!(parse_period(Some("-3")), None);
        assert_eq!(parse_period(Some("abc")), None);
    }

    #[test]
    fn 百分位与上游叠层同一口径() {
        let sorted: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        // debug_overlay.rs 的 `1%` 行取的就是这个位置(第 99ms 那格)。
        assert_eq!(percentile(&sorted, 99), Some(Duration::from_millis(99)));
        assert_eq!(percentile(&sorted, 90), Some(Duration::from_millis(90)));
        assert_eq!(percentile(&sorted, 50), Some(Duration::from_millis(50)));
        assert_eq!(percentile(&[], 50), None);
        assert_eq!(
            percentile(&[Duration::from_millis(7)], 99),
            Some(Duration::from_millis(7))
        );
        // 记档:向下取整的最近秩在样本少时偏低 —— 两个样本的 p99 是**小的那个**。
        // 这是上游公式的性质,不是这里算错;一拍只有个位数帧时读 max 不读 p99。
        let two = [Duration::from_millis(2), Duration::from_millis(8)];
        assert_eq!(percentile(&two, 99), Some(Duration::from_millis(2)));
    }

    #[test]
    fn 空闲一拍也出结论() {
        let mut sample = Sample::default();
        let line = sample.render(Duration::from_secs(2), 0);
        assert_eq!(line, "[frame-profiler] 2.0s | 空闲(0 帧)");
    }

    #[test]
    fn 有帧时按项拼行() {
        let mut sample = Sample::default();
        sample.draw = vec![Duration::from_millis(2), Duration::from_millis(8)];
        sample.invalidations = vec![1, 5];
        let line = sample.render(Duration::from_secs(2), 3);
        assert!(line.contains("帧 2 (1.0/s)"), "{line}");
        // p99 在两个样本上取的是 `sorted[0]`(见 `percentile` 的注释),这里连同
        // max 一起钉住,免得日后有人「顺手修正」百分位公式而丢掉与叠层的可对账性。
        assert!(line.contains("draw p50 2.0 p99 2.0 max 8.0 ms"), "{line}");
        assert!(line.contains("invalidation/帧 均 3.0 峰 5"), "{line}");
        // 没有样本的两段整段消失,不打 `--`。
        assert!(!line.contains("dirty→present"), "{line}");
        assert!(!line.contains("呈现间隔"), "{line}");
        assert!(line.contains("journal 丢失 3"), "{line}");
    }
}
