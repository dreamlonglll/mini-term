//! 全局「减少动画」闸 + 一次性过渡基件(跑完自停)。
//!
//! # 1. 闸:`prefers-reduced-motion` 的等价物
//!
//! 原版是 WebView2 里的 `@media (prefers-reduced-motion: reduce)`
//! (`src/styles.css:388-478`),它在 Windows 上由**系统设置 → 辅助功能 →
//! 视觉效果 → 动画效果**驱动(Win32 侧就是 `SPI_GETCLIENTAREAANIMATION`)。
//! GPUI 没有媒体查询,于是把同一件事收成一个进程级布尔量:宿主启动时探测一次
//! 写进 [`set_reduce_motion`],所有动画消费方读 [`reduce_motion`]。
//!
//! **探测代码不在这里**:全仓的 Win32 调用统一住在 mt-app(`tray.rs` /
//! `notify.rs` / `i18n.rs` 都是),mt-ui 不引 `windows` crate。
//! 落点见 `crates/mt-app/src/motion.rs`。
//!
//! # 2. 原版哪些停、哪些豁免(逐条对照,别一刀切)
//!
//! `styles.css` 的 reduce 段先用通配规则把**所有** animation/transition 压到
//! 0.01ms、循环次数压到 1,然后逐条开豁免。照抄的结论:
//!
//! | 原版类名 | reduce 下 | 本模块对应 |
//! |---|---|---|
//! | `.animate-blink`(状态灯闪烁 / 更新红点 / 中转连接中) | **停** | [`blinks`] |
//! | `.animate-pulse`(骨架屏) | **停** | [`blinks`] |
//! | `.animate-glow` | **停** | (GPUI 侧无消费方) |
//! | `.done-tag` 的 `tagFadeIn` | **停**(压成瞬时) | [`TAG_FADE_IN`] |
//! | `.toast-card` 的 `toastSlideIn` | **停**(压成瞬时) | [`TOAST_SLIDE_IN`] |
//! | `.animate-status-spin` / `.animate-spin`(**进行中**指示器) | 继续转,周期放慢到 2.4s | [`spin_period`] |
//! | 浮层进出场(`.overlay-*` / `.ctx-menu` / `.prompt-*`) | **豁免**,原速播完 | [`OVERLAY_IN`] 等 |
//! | `.terminal-swap-in` / `.panel-swap-in` / `.pane-enter` | **豁免** | [`PANE_ENTER`] 等 |
//! | `.drawer-tab-indicator` / `.git-section-*` | **豁免** | [`TAB_INDICATOR`] / [`SECTION_TOGGLE`] |
//! | `.usage-fade-in` / `.usage-rank-bar` | **豁免** | [`USAGE_FADE_IN`] / [`RANK_BAR`] |
//!
//! 豁免面这么大是有原文依据的(`styles.css:415-421`):Windows 上把视觉效果调成
//! 「最佳性能」的人也会落进 reduce 分支,他们要的是别卡,不是「界面凭空跳变」。
//! 所以**转场照播,闪烁全停**。本模块把这条口径固化成 [`TransitionSpec`] 的
//! `respects_reduce` 位:豁免的那些一律 `false`。
//!
//! # 3. 一次性过渡基件:为什么不用 `gpui::with_animation`
//!
//! `AnimationElement` 的状态 key 是 **ElementId**(存在 window 的 element state
//! 表里),这带来三个够呛的性质:
//!
//! 1. id 必须逐处唯一且跨帧稳定,否则要么共享进度、要么每帧从头播;
//! 2. 元素离开一帧树再回来 = 状态没了 = 重播(切项目、滚动出可视区都会碰);
//! 3. 它包装的是**元素**,而我们有些补间要的是**数值**(排行条的宽度、
//!    面积图的高度)——那些得先算出数再喂给布局。
//!
//! 于是照搬 K 批终端滚动条那套手法:**记起始时刻,绘制时按 elapsed 插值,
//! 到终态就不再请求帧**。状态挂在**视图**(Entity)上而不是元素上,所以
//! 「被打断重启」是显式调用 [`Transition::restart`],不靠 id 变化去骗。
//!
//! # 4. 怎么用
//!
//! ```ignore
//! // 视图字段
//! fade: mt_ui::motion::Transition,
//!
//! // 需要重播时(例如面板刚进入 Ready 相位)
//! self.fade.restart();
//!
//! // render 里:一句话拿到进度,并在没跑完时自动要下一帧
//! let p = self.fade.drive(window);
//! div().opacity(p).mt(px(6.0 * (1.0 - p)))
//! ```
//!
//! 值补间(旧值 → 新值,值不变就不动)用 [`TweenMap`]:
//!
//! ```ignore
//! let w = self.bars.value("rank-项目A", ratio);   // 目标变了才起补间
//! …
//! if self.bars.drive(window) { /* 还有条目在跑,已自动请求下一帧 */ }
//! self.bars.sweep();   // 本帧没读到的条目丢掉(等价于 DOM 里那行没了)
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, App, EntityId, Window};

// ─── 闸 ──────────────────────────────────────────────────────

/// 进程级「减少动画」开关。默认 `false`(= 不减少)——非 Windows 平台没有探测
/// 实现,也就恒定按「系统没开减弱动效」处理。
static REDUCE_MOTION: AtomicBool = AtomicBool::new(false);

/// 系统是否要求减少动画。所有动画消费方的唯一判据。
pub fn reduce_motion() -> bool {
    REDUCE_MOTION.load(Ordering::Relaxed)
}

/// 写入闸的新值,返回**是否发生了变化**(宿主据此决定要不要 `refresh_windows`)。
pub fn set_reduce_motion(reduce: bool) -> bool {
    REDUCE_MOTION.swap(reduce, Ordering::Relaxed) != reduce
}

/// 闪烁类动画这一帧该不该动。reduce 下恒 `false` —— 状态本身由形状+颜色编码,
/// 不靠闪烁传达(原版通配规则把 `.animate-blink` / `.animate-pulse` 停在第一帧)。
pub fn blinks() -> bool {
    !reduce_motion()
}

/// reduce 下「进行中」指示器的旋转周期(`styles.css:409-413` 的 2.4s)。
pub const REDUCED_SPIN_PERIOD: Duration = Duration::from_millis(2400);

/// 旋转周期过闸。**不会返回 0** —— 一个停住的 spinner 不是「安静」,是在说谎
/// (看上去就是卡死),所以 reduce 下只放慢不停。
pub fn spin_period(base: Duration) -> Duration {
    if reduce_motion() {
        REDUCED_SPIN_PERIOD
    } else {
        base
    }
}

// ─── 永续动画低频泵 ──────────────────────────────────────────
//
// spinner 旋转/状态点闪烁这类**永不停**的动画,原先走
// `gpui::with_animation(Animation::new(..).repeat())` —— `AnimationElement`
// 只要没播完就每帧 `request_animation_frame()`,而 `.repeat()` 意味着永不完:
// 一颗 8px 的点足以把整窗按满帧率连续重绘,**前后台通吃**,`mt-app::redraw`
// 的 30fps/后台 5fps 节拍完全被架空。实测(2026-08-31 GPU 悬崖诊断)在显存
// 吃紧、整窗纹理被驱动挤进共享内存的机器上,这条帧率就是「挂着 AI 时 GPU
// 80~99%」的直接来源。
//
// 换成两半:
// - **相位**从进程级墙钟推导([`pulse_phase`]),不做逐元素状态 —— 终端有
//   输出时动画搭 PTY 重绘的便车,平滑度与从前无差,额外代价为零;
// - **保底节拍**由一条共享低频泵兜住「终端静默时动画也得走」:前台
//   [`PULSE_TICK`] 一拍,后台每 [`PULSE_INACTIVE_EVERY`] 拍才刷一次(同时把
//   动画周期放慢 [`PULSE_INACTIVE_SLOWDOWN`] 倍,免得 500ms 的拍距在 0.8~1s
//   的周期上踩出「看着没动」的走样)。窗口一个刷新周期内没有任何动画再来
//   登记就从泵上摘除,泵空转即停 —— 与 `mt-app::redraw::Pump` 同款
//   「空跑一拍就收摊」,不留常驻定时器。
//
// 泵触发用的是 **`cx.notify(挂动画的 view)`** 而不是 `window.refresh()`:
// refresh 会置 `refreshing` 位、绕过所有 view 级缓存做全量 CPU 重渲染
// (实测同尺寸下单帧 CPU 是 notify 路的 ~4.6 倍);notify 只弄脏登记过的
// view,其余面板照走缓存 —— 与上游 `request_animation_frame` 的
// 「notify 当前 view」同一语义,只是节拍从每帧换成了低频定时。

/// 前台保底节拍:100ms(10fps)。AI 思考中无输出时 spinner 靠它步进。
const PULSE_TICK: Duration = Duration::from_millis(100);

/// 后台每几拍刷一次(5 × 100ms = 500ms)。后台窗口上的 spinner 只是「还活着」
/// 的信号,2fps 足够传达,代价是前台满帧档的 1/30。
const PULSE_INACTIVE_EVERY: u64 = 5;

/// 后台把动画周期放慢的倍数。500ms 拍距若踩在 0.9s 周期上,相邻两帧相位差
/// 过半圈,旋转会走样成来回抖;放慢到 3.6s 后每拍走约 1/7 圈,方向清晰。
const PULSE_INACTIVE_SLOWDOWN: u32 = 4;

thread_local! {
    /// 相位的时间原点。进程级 —— 所有窗口所有动画共用一条钟,
    /// 同状态的多颗灯天然同相(原版 CSS animation 各自挂载反而会错相)。
    static PULSE_EPOCH: Instant = Instant::now();

    static PULSE: RefCell<PulsePump> = RefCell::new(PulsePump::default());
}

/// 保底泵本体。状态全在主线程,`thread_local` 零通知 —— 与全局闸同款朴素做法。
#[derive(Default)]
struct PulsePump {
    /// 泵在跑吗。同一时刻只该有一条。
    running: bool,
    /// 第几拍。后台窗口按 [`PULSE_INACTIVE_EVERY`] 的整数拍刷新。
    tick: u64,
    windows: Vec<PulseWindow>,
}

struct PulseWindow {
    handle: AnyWindowHandle,
    /// 自上次泵触发以来,窗口里登记过动画的 view(= [`pulse_phase`] 的调用方)。
    /// 触发即取走;一轮下来还是空的,说明动画都卸载了,该摘除这个窗口。
    views: Vec<EntityId>,
}

/// 一拍里对单个窗口的处置。纯判定,单测钉它。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PulseAction {
    /// 到拍且上轮有登记:notify 登记过的 view,开始新一轮观察。
    Refresh,
    /// 后台窗口还没到它的拍:什么都不做,登记记录保留。
    Wait,
    /// 到拍但上轮没人登记:动画已从树上消失,把窗口从泵上摘掉。
    Drop,
}

fn pulse_action(window_active: bool, inactive_due: bool, painted: bool) -> PulseAction {
    if !(window_active || inactive_due) {
        return PulseAction::Wait;
    }
    if painted {
        PulseAction::Refresh
    } else {
        PulseAction::Drop
    }
}

/// 当前墙钟在 `period` 周期里的相位(0..1)。
fn phase_of(elapsed: Duration, period: Duration) -> f32 {
    let period_ms = period.as_millis().max(1) as u64;
    (elapsed.as_millis() as u64 % period_ms) as f32 / period_ms as f32
}

/// 取一个永续动画这一帧的相位(0..1 一圈),并把窗口挂上保底泵。
///
/// **render 里替代 `with_animation(Animation::new(period).repeat())` 用**:
/// 拿返回值直接喂 `VectorIcon::rotation` / 闪烁曲线。窗口在后台时周期自动
/// 放慢 [`PULSE_INACTIVE_SLOWDOWN`] 倍。
///
/// 减弱动效的口径由调用方把住(旋转过 [`spin_period`]、闪烁过 [`blinks`]),
/// 本函数不做判断 —— 谁都不挂树,泵自然就停。
pub fn pulse_phase(period: Duration, window: &Window, cx: &mut App) -> f32 {
    let period = if window.is_window_active() {
        period
    } else {
        period * PULSE_INACTIVE_SLOWDOWN
    };
    let handle = window.window_handle();
    let view = window.current_view();
    let start_pump = PULSE.with(|pump| {
        let mut pump = pump.borrow_mut();
        let id = handle.window_id();
        let entry = match pump.windows.iter_mut().find(|w| w.handle.window_id() == id) {
            Some(w) => w,
            None => {
                pump.windows.push(PulseWindow {
                    handle,
                    views: Vec::new(),
                });
                pump.windows.last_mut().expect("刚 push 进去的")
            }
        };
        if !entry.views.contains(&view) {
            entry.views.push(view);
        }
        !std::mem::replace(&mut pump.running, true)
    });
    if start_pump {
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(PULSE_TICK).await;
                // App 没了(退出中)就把 running 收干净再走
                let Ok(stop) = cx.update(pulse_tick) else {
                    break;
                };
                if stop {
                    return;
                }
            }
            PULSE.with(|pump| pump.borrow_mut().running = false);
        })
        .detach();
    }
    phase_of(PULSE_EPOCH.with(|e| e.elapsed()), period)
}

/// 泵的一拍:按处置口径刷新/摘除各窗口。返回**是否该停泵**(停时已自收
/// `running`)。
fn pulse_tick(cx: &mut App) -> bool {
    let (tick, entries) = PULSE.with(|pump| {
        let mut pump = pump.borrow_mut();
        pump.tick = pump.tick.wrapping_add(1);
        let entries: Vec<(AnyWindowHandle, bool)> = pump
            .windows
            .iter()
            .map(|w| (w.handle, !w.views.is_empty()))
            .collect();
        (pump.tick, entries)
    });
    let inactive_due = tick % PULSE_INACTIVE_EVERY == 0;

    for (handle, painted) in entries {
        // 窗口已关按 Drop 处理 —— 弱引用失效是正常生命周期
        let action = handle
            .update(cx, |_, window, _| {
                pulse_action(window.is_window_active(), inactive_due, painted)
            })
            .unwrap_or(PulseAction::Drop);
        let views = PULSE.with(|pump| {
            let mut pump = pump.borrow_mut();
            let id = handle.window_id();
            match action {
                PulseAction::Refresh => pump
                    .windows
                    .iter_mut()
                    .find(|w| w.handle.window_id() == id)
                    .map(|w| std::mem::take(&mut w.views))
                    .unwrap_or_default(),
                PulseAction::Wait => Vec::new(),
                PulseAction::Drop => {
                    pump.windows.retain(|w| w.handle.window_id() != id);
                    Vec::new()
                }
            }
        });
        // notify 已释放的 view 是无害 no-op(观察者表里查无此人)
        for view in views {
            cx.notify(view);
        }
    }

    PULSE.with(|pump| {
        let mut pump = pump.borrow_mut();
        if pump.windows.is_empty() {
            pump.running = false;
            true
        } else {
            false
        }
    })
}

// ─── 缓动 ────────────────────────────────────────────────────

/// 缓动曲线。取值范围与 CSS 同名关键字一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Easing {
    /// 匀速。
    Linear,
    /// CSS 关键字 `ease-out` = `cubic-bezier(0, 0, 0.58, 1)`。
    EaseOut,
    /// `--ease-overlay-in` = `cubic-bezier(0.16, 1, 0.3, 1)`(easeOutExpo,
    /// 起步快、尾段缓,收尾有「停稳」的手感)。
    EaseOutExpo,
    /// `--ease-overlay-out` = `cubic-bezier(0.4, 0, 0.9, 0.6)`。
    EaseOverlayOut,
}

impl Easing {
    /// 这条曲线的四个控制点(`Linear` 没有,返回 `None`)。
    pub const fn control_points(self) -> Option<(f32, f32, f32, f32)> {
        match self {
            Easing::Linear => None,
            Easing::EaseOut => Some((0.0, 0.0, 0.58, 1.0)),
            Easing::EaseOutExpo => Some((0.16, 1.0, 0.3, 1.0)),
            Easing::EaseOverlayOut => Some((0.4, 0.0, 0.9, 0.6)),
        }
    }
}

/// 把 0..1 的线性进度映射成缓动后的进度。
pub fn ease(easing: Easing, t: f32) -> f32 {
    match easing.control_points() {
        None => t.clamp(0.0, 1.0),
        Some((x1, y1, x2, y2)) => cubic_bezier_at(x1, y1, x2, y2, t),
    }
}

/// CSS `cubic-bezier(x1, y1, x2, y2)` 在横坐标 `x` 处的取值。
///
/// 做法与浏览器一致:x(t) 与 y(t) 都是控制点为 `(0,0) (x1,y1) (x2,y2) (1,1)`
/// 的三次贝塞尔,给定 `x` 先**二分**反解 `t` 再取 `y(t)`。二分 20 次精度 1e-6,
/// 比牛顿法慢但恒收敛(控制点在 0..1 之外时牛顿法会跑飞)。
///
/// ⚠️ mt-app 的 `ui::cubic_bezier` 是同一算法的另一份实现(它返回闭包喂给
/// `gpui::Animation::with_easing`)。两份都很短,合并要动 ui.rs 的公开签名,
/// 留作技术债。
pub fn cubic_bezier_at(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    fn bezier(a: f32, b: f32, t: f32) -> f32 {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    }
    let x = x.clamp(0.0, 1.0);
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    let mut t = x;
    for _ in 0..20 {
        if bezier(x1, x2, t) < x {
            lo = t;
        } else {
            hi = t;
        }
        t = (lo + hi) * 0.5;
    }
    bezier(y1, y2, t)
}

// ─── 过渡规格 ────────────────────────────────────────────────

/// 一条一次性过渡的规格:多久、什么曲线、要不要过减弱动效那道闸。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransitionSpec {
    pub duration: Duration,
    pub easing: Easing,
    /// `true` = reduce 时直达终态(原版被通配规则压成 0.01ms 的那些);
    /// `false` = 原版在 reduce 段里被**点名豁免**,照常播完整动画。
    pub respects_reduce: bool,
}

impl TransitionSpec {
    /// 普通动画:reduce 时直达终态。
    pub const fn new(duration: Duration, easing: Easing) -> Self {
        Self {
            duration,
            easing,
            respects_reduce: true,
        }
    }

    /// 原版 reduce 段里被点名豁免的那些(浮层进出场 / 切终端 / 分屏 /
    /// 抽屉指示条 / 用量面板数据动效),照常播。
    pub const fn exempt(duration: Duration, easing: Easing) -> Self {
        Self {
            duration,
            easing,
            respects_reduce: false,
        }
    }

    /// 这一帧的进度(已缓动)。`reduced` 由调用方给,便于单测。
    pub fn progress_at(&self, elapsed: Duration, reduced: bool) -> f32 {
        if (reduced && self.respects_reduce) || self.duration.is_zero() {
            return 1.0;
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        if t >= 1.0 {
            1.0
        } else {
            ease(self.easing, t)
        }
    }

    /// 还在跑吗(= 还要不要请求下一帧)。终态之后恒 `false`,这是本基件存在的理由。
    pub fn running_at(&self, elapsed: Duration, reduced: bool) -> bool {
        if (reduced && self.respects_reduce) || self.duration.is_zero() {
            return false;
        }
        elapsed < self.duration
    }
}

/// `--motion-overlay-in` 0.24s(豁免)。
pub const OVERLAY_IN: TransitionSpec =
    TransitionSpec::exempt(Duration::from_millis(240), Easing::EaseOutExpo);
/// `--motion-overlay-out` 0.14s(豁免)。
pub const OVERLAY_OUT: TransitionSpec =
    TransitionSpec::exempt(Duration::from_millis(140), Easing::EaseOverlayOut);
/// `--motion-menu-in` 0.16s(豁免)。
pub const MENU_IN: TransitionSpec =
    TransitionSpec::exempt(Duration::from_millis(160), Easing::EaseOutExpo);
/// `--motion-terminal-swap` 0.2s(豁免)。
pub const TERMINAL_SWAP: TransitionSpec =
    TransitionSpec::exempt(Duration::from_millis(200), Easing::EaseOutExpo);
/// `--motion-tab-indicator` 0.22s(豁免)。
pub const TAB_INDICATOR: TransitionSpec =
    TransitionSpec::exempt(Duration::from_millis(220), Easing::EaseOutExpo);
/// `--motion-pane-enter` 0.26s(豁免)。新分出来的格子淡入 + 放大到位。
pub const PANE_ENTER: TransitionSpec =
    TransitionSpec::exempt(Duration::from_millis(260), Easing::EaseOutExpo);
/// `--motion-section-toggle` 0.22s(豁免)。
pub const SECTION_TOGGLE: TransitionSpec =
    TransitionSpec::exempt(Duration::from_millis(220), Easing::EaseOutExpo);
/// `.usage-fade-in` 0.35s ease-out(**豁免**,`styles.css:471-473`)。
pub const USAGE_FADE_IN: TransitionSpec =
    TransitionSpec::exempt(Duration::from_millis(350), Easing::EaseOut);
/// `.usage-rank-bar` 的 `transition-[width] duration-500 ease-out`
/// (**豁免**,`styles.css:475-477`)。
pub const RANK_BAR: TransitionSpec =
    TransitionSpec::exempt(Duration::from_millis(500), Easing::EaseOut);
/// `.toast-card` 的 `toastSlideIn 0.25s ease-out`。原版**没有**豁免它 ——
/// reduce 下由通配规则压成瞬时。
pub const TOAST_SLIDE_IN: TransitionSpec =
    TransitionSpec::new(Duration::from_millis(250), Easing::EaseOut);
/// `.done-tag` 的 `tagFadeIn 0.3s ease-out`(`styles.css:522`)。原版**没有**
/// 豁免它 —— 与 toast 同一档,reduce 下由通配规则压成瞬时。
pub const TAG_FADE_IN: TransitionSpec =
    TransitionSpec::new(Duration::from_millis(300), Easing::EaseOut);

// ─── 关键帧的 gpui 近似 ──────────────────────────────────────

/// `menuPopIn` 的起始上移量:`styles.css:280` 的 `translateY(-6px)`。
pub const MENU_POP_RISE_PX: f32 = 6.0;

/// `menuPopIn` 这一帧的 **(不透明度, 上移像素)** —— 位移是负值,直接喂
/// `.mt(px(..))`(浮层都挂在 `anchored` 里,边距只挪自己)。
///
/// # 刻意丢掉 `scale(0.96)`
///
/// 原版关键帧是 `opacity 0 → 1` + `translateY(-6px) → 0` + `scale(0.96) → 1`
/// (`transform-origin: top left`)。gpui 没有 transform,能等价缩放的只有
/// **改内边距/尺寸**,而那是会改布局的:菜单项的文字会在这 160ms 里跟着挪,
/// 悬停缩略图卡里的 `MiniTerminalElement` 每帧按新框反解一次字号。与 pane
/// 进场同一条取舍(见 `terminal_area::wrap_pane_enter` 的注释)——淡入+位移
/// 是主信号,4% 缩放不值一次逐帧重排。
pub fn menu_pop_in(progress: f32) -> (f32, f32) {
    let p = progress.clamp(0.0, 1.0);
    (p, -MENU_POP_RISE_PX * (1.0 - p))
}

/// `tagFadeIn` 这一帧的 **(不透明度, 缩放)**。
///
/// 关键帧照抄 `styles.css:531-535`:`0% {opacity:0; scale(.6)}` /
/// `60% {opacity:1; scale(1.15)}` / `100% {scale(1)}` —— 60% 处**过冲**再回落,
/// 这一下是它区别于普通淡入的全部。`100%` 那帧没写 opacity,按 CSS 规则从 60%
/// 起保持元素本身的 1。
///
/// 缩放怎么用由调用方定(GPUI 侧的 DONE 标是拿它乘水平内边距 —— 只让药丸
/// 自己横向呼吸,不动字号、不动行高,免得整行在这 300ms 里抖)。
pub fn tag_fade_in(progress: f32) -> (f32, f32) {
    /// 过冲那一帧的时间位置。
    const PEAK_AT: f32 = 0.6;
    const START_SCALE: f32 = 0.6;
    const PEAK_SCALE: f32 = 1.15;
    let p = progress.clamp(0.0, 1.0);
    if p < PEAK_AT {
        let k = p / PEAK_AT;
        (k, START_SCALE + (PEAK_SCALE - START_SCALE) * k)
    } else {
        let k = (p - PEAK_AT) / (1.0 - PEAK_AT);
        (1.0, PEAK_SCALE + (1.0 - PEAK_SCALE) * k)
    }
}

// ─── 一次性过渡 ──────────────────────────────────────────────

/// 一条跑完自停的过渡。状态就是「起始时刻 + 规格」两个字段,挂在视图上。
#[derive(Clone, Copy, Debug)]
pub struct Transition {
    start: Instant,
    spec: TransitionSpec,
}

impl Transition {
    /// 从**现在**开始跑。
    pub fn new(spec: TransitionSpec) -> Self {
        Self {
            start: Instant::now(),
            spec,
        }
    }

    /// 建一条**已经跑完**的(视图初始化时用:不想让首帧就播)。
    pub fn settled(spec: TransitionSpec) -> Self {
        Self {
            // 减去时长即「起点在过去,现在已到终态」;时钟起点附近减不动就退到 now
            start: Instant::now()
                .checked_sub(spec.duration)
                .unwrap_or_else(Instant::now),
            spec,
        }
    }

    /// 被打断重启:起点重置成现在,**从头播**。
    ///
    /// 刻意不做「从当前进度接着播」——原版是 CSS animation 重新挂载,
    /// 语义就是从第一帧开始。数值类的接续补间请用 [`ValueTween`]。
    pub fn restart(&mut self) {
        self.start = Instant::now();
    }

    pub fn spec(&self) -> TransitionSpec {
        self.spec
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// 这一帧的进度(已缓动、已过闸)。
    pub fn progress(&self) -> f32 {
        self.spec.progress_at(self.elapsed(), reduce_motion())
    }

    pub fn running(&self) -> bool {
        self.spec.running_at(self.elapsed(), reduce_motion())
    }

    /// 取进度 + 没跑完就请求下一帧。**render 里只调这一个**。
    ///
    /// `request_animation_frame` 只 notify 当前视图,所以必须在该视图的
    /// `render` 调用链里调 —— 这也正是「终态后不再请求帧」的落点。
    pub fn drive(&self, window: &Window) -> f32 {
        let p = self.progress();
        if self.running() {
            window.request_animation_frame();
        }
        p
    }
}

// ─── 数值补间 ────────────────────────────────────────────────

/// 「旧值 → 新值」的补间(CSS `transition: <prop>` 的等价物)。
///
/// 与 [`Transition`] 的区别:目标值中途变了要**从当前显示值**接着补,
/// 不是从头。原版排行条 500ms 的宽度过渡就是这个语义。
#[derive(Clone, Copy, Debug)]
pub struct ValueTween {
    from: f32,
    to: f32,
    start: Instant,
    spec: TransitionSpec,
}

impl ValueTween {
    /// 建一条**停在 `value` 上**的(首次出现不补间 —— 浏览器同款:
    /// 新插入的元素带着最终样式,没有旧值可补)。
    pub fn settled(value: f32, spec: TransitionSpec) -> Self {
        Self {
            from: value,
            to: value,
            start: Instant::now()
                .checked_sub(spec.duration)
                .unwrap_or_else(Instant::now),
            spec,
        }
    }

    /// 换目标值。与当前目标相同则**什么都不做**(否则每帧都会重启补间,
    /// 结果是永远停在起点 —— recharts 那条 `useAnimationId` 注释踩的就是这个坑)。
    pub fn retarget_at(&mut self, target: f32, now: Instant) {
        if (target - self.to).abs() <= f32::EPSILON {
            return;
        }
        self.from = self.value_at(now);
        self.to = target;
        self.start = now;
    }

    pub fn value_at(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.start);
        let p = self.spec.progress_at(elapsed, reduce_motion());
        self.from + (self.to - self.from) * p
    }

    pub fn running_at(&self, now: Instant) -> bool {
        self.spec
            .running_at(now.saturating_duration_since(self.start), reduce_motion())
    }

    pub fn target(&self) -> f32 {
        self.to
    }
}

/// 一表多条 [`ValueTween`],按字符串 key 索引(排行条每行一条)。
///
/// 生命周期照抄 DOM:**本帧读到的条目留下,没读到的 [`sweep`](Self::sweep)
/// 掉** —— 行没了就等于元素被卸载,下次再出现按「首次出现」处理(不补间)。
#[derive(Debug)]
pub struct TweenMap {
    spec: TransitionSpec,
    /// 值 + 「最后一次被读到是第几轮」。
    entries: HashMap<String, (ValueTween, u64)>,
    epoch: u64,
}

impl TweenMap {
    pub fn new(spec: TransitionSpec) -> Self {
        Self {
            spec,
            entries: HashMap::new(),
            epoch: 0,
        }
    }

    /// 取这一行这一帧该画的值。首次见到某 key 直接落在目标值上。
    pub fn value_at(&mut self, key: &str, target: f32, now: Instant) -> f32 {
        let epoch = self.epoch;
        if let Some((tween, seen)) = self.entries.get_mut(key) {
            *seen = epoch;
            tween.retarget_at(target, now);
            return tween.value_at(now);
        }
        let tween = ValueTween::settled(target, self.spec);
        let value = tween.value_at(now);
        self.entries.insert(key.to_string(), (tween, epoch));
        value
    }

    /// 同上,时钟取 `Instant::now()`。
    pub fn value(&mut self, key: &str, target: f32) -> f32 {
        self.value_at(key, target, Instant::now())
    }

    /// 还有条目在跑吗。
    pub fn running_at(&self, now: Instant) -> bool {
        self.entries.values().any(|(t, _)| t.running_at(now))
    }

    /// 有条目在跑就请求下一帧,返回是否请求了。
    pub fn drive(&self, window: &Window) -> bool {
        let running = self.running_at(Instant::now());
        if running {
            window.request_animation_frame();
        }
        running
    }

    /// 一帧读完之后调:丢掉这一轮没被读到的条目,并进入下一轮。
    pub fn sweep(&mut self) {
        let epoch = self.epoch;
        self.entries.retain(|_, (_, seen)| *seen == epoch);
        self.epoch = epoch.wrapping_add(1);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 闸是进程级全局量,测试之间会串味 —— **本 crate 里所有要动闸的用例统一走这个
/// 夹具**(`cargo test` 默认多线程,所以还得用同一把锁串行化;各模块自己造一把
/// 锁就白搭了)。
#[cfg(test)]
pub(crate) fn with_reduce<R>(on: bool, f: impl FnOnce() -> R) -> R {
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = reduce_motion();
    set_reduce_motion(on);
    let out = f();
    set_reduce_motion(prev);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 闸的读写与变化判定() {
        with_reduce(false, || {
            assert!(!reduce_motion());
            assert!(blinks());
            assert!(set_reduce_motion(true), "false→true 是一次变化");
            assert!(!set_reduce_motion(true), "同值写入不算变化");
            assert!(reduce_motion());
            assert!(!blinks(), "reduce 下闪烁必须停");
        });
    }

    #[test]
    fn 旋转只放慢不停() {
        let base = Duration::from_millis(900);
        with_reduce(false, || assert_eq!(spin_period(base), base));
        with_reduce(true, || {
            // 停住的 spinner 看上去就是卡死 —— 原版为此专门开了豁免
            assert_eq!(spin_period(base), REDUCED_SPIN_PERIOD);
            assert!(!spin_period(base).is_zero());
        });
    }

    #[test]
    fn 相位按周期取模且不越界() {
        let p = Duration::from_millis(900);
        assert_eq!(phase_of(Duration::ZERO, p), 0.0);
        assert!((phase_of(Duration::from_millis(450), p) - 0.5).abs() < 1e-4);
        // 整圈归零,而不是停在 1.0
        assert_eq!(phase_of(Duration::from_millis(900), p), 0.0);
        assert!((phase_of(Duration::from_millis(2250), p) - 0.5).abs() < 1e-4);
        assert!(
            phase_of(Duration::from_secs(86400), p) < 1.0,
            "跑一天也不许越界"
        );
        // 零周期防护:不许除零
        assert_eq!(phase_of(Duration::from_secs(1), Duration::ZERO), 0.0);
    }

    #[test]
    fn 保底泵一拍的处置口径() {
        // 前台:每拍都到,登记过就刷、没登记就摘
        assert_eq!(pulse_action(true, false, true), PulseAction::Refresh);
        assert_eq!(pulse_action(true, false, false), PulseAction::Drop);
        // 后台没到拍:一律等,**不许**因 painted=false 提前摘 ——
        // 它的观察窗口以「刷新」为界,还没刷过下一次就没资格下结论
        assert_eq!(pulse_action(false, false, true), PulseAction::Wait);
        assert_eq!(pulse_action(false, false, false), PulseAction::Wait);
        // 后台到拍:与前台同口径
        assert_eq!(pulse_action(false, true, true), PulseAction::Refresh);
        assert_eq!(pulse_action(false, true, false), PulseAction::Drop);
    }

    #[test]
    fn 后台放慢倍数要压住走样() {
        // 500ms 拍距 × 放慢后的最短常见周期(0.8s×4 = 3.2s):每拍相位差
        // 需明显小于半圈,旋转方向才不至于读反
        let tick_ms = (PULSE_TICK.as_millis() as u64 * PULSE_INACTIVE_EVERY) as f32;
        let slowest_ms = 800.0 * PULSE_INACTIVE_SLOWDOWN as f32;
        assert!(tick_ms / slowest_ms < 0.25, "后台每拍相位差应小于 1/4 圈");
    }

    #[test]
    fn 缓动端点与单调性() {
        for easing in [
            Easing::Linear,
            Easing::EaseOut,
            Easing::EaseOutExpo,
            Easing::EaseOverlayOut,
        ] {
            assert!(ease(easing, 0.0).abs() < 1e-4, "{easing:?} 起点应为 0");
            assert!((ease(easing, 1.0) - 1.0).abs() < 1e-4, "{easing:?} 终点应为 1");
            assert_eq!(ease(easing, -5.0), 0.0, "{easing:?} 越界要钳住");
            assert_eq!(ease(easing, 5.0), 1.0, "{easing:?} 越界要钳住");
            let mut prev = -1.0;
            for i in 0..=50 {
                let v = ease(easing, i as f32 / 50.0);
                assert!(v >= prev - 1e-4, "{easing:?} 在 t={i} 处回头了");
                prev = v;
            }
        }
    }

    #[test]
    fn ease_out_系前段快尾段缓() {
        // easeOutExpo 半程就该走完大半路程,linear 恰好一半
        assert!((ease(Easing::Linear, 0.5) - 0.5).abs() < 1e-4);
        assert!(ease(Easing::EaseOutExpo, 0.5) > 0.85, "{}", ease(Easing::EaseOutExpo, 0.5));
        assert!(ease(Easing::EaseOut, 0.5) > 0.5);
        // cubic-bezier(0,0,0.58,1) 在 x=0.5 处的标准值 ≈ 0.6836
        assert!((cubic_bezier_at(0.0, 0.0, 0.58, 1.0, 0.5) - 0.6836).abs() < 0.005);
    }

    #[test]
    fn 进度采样_到时长即终态且不再跑() {
        let spec = TransitionSpec::new(Duration::from_millis(400), Easing::Linear);
        assert_eq!(spec.progress_at(Duration::ZERO, false), 0.0);
        assert!((spec.progress_at(Duration::from_millis(100), false) - 0.25).abs() < 1e-4);
        assert!((spec.progress_at(Duration::from_millis(200), false) - 0.5).abs() < 1e-4);
        assert_eq!(spec.progress_at(Duration::from_millis(400), false), 1.0);
        assert_eq!(spec.progress_at(Duration::from_secs(30), false), 1.0);
        // 终态之后不许再请求帧 —— 这是选一次性方案的根本原因
        assert!(spec.running_at(Duration::from_millis(399), false));
        assert!(!spec.running_at(Duration::from_millis(400), false));
        assert!(!spec.running_at(Duration::from_secs(30), false));
    }

    #[test]
    fn reduce_下直达终态_豁免的照播() {
        let normal = TransitionSpec::new(Duration::from_millis(250), Easing::EaseOut);
        let exempt = TransitionSpec::exempt(Duration::from_millis(250), Easing::EaseOut);
        // 普通动画:reduce 时第一帧就是终态,且一帧都不请求
        assert_eq!(normal.progress_at(Duration::ZERO, true), 1.0);
        assert!(!normal.running_at(Duration::ZERO, true));
        // 豁免动画:reduce 与否行为完全一致
        assert_eq!(
            exempt.progress_at(Duration::from_millis(125), true),
            exempt.progress_at(Duration::from_millis(125), false)
        );
        assert!(exempt.running_at(Duration::ZERO, true));
    }

    #[test]
    fn 原版动效常量逐条对齐样式表() {
        // 时长照抄 styles.css:67-78 与各自的规则行
        assert_eq!(OVERLAY_IN.duration, Duration::from_millis(240));
        assert_eq!(OVERLAY_OUT.duration, Duration::from_millis(140));
        assert_eq!(MENU_IN.duration, Duration::from_millis(160));
        assert_eq!(TERMINAL_SWAP.duration, Duration::from_millis(200));
        assert_eq!(TAB_INDICATOR.duration, Duration::from_millis(220));
        assert_eq!(PANE_ENTER.duration, Duration::from_millis(260));
        assert_eq!(SECTION_TOGGLE.duration, Duration::from_millis(220));
        assert_eq!(USAGE_FADE_IN.duration, Duration::from_millis(350));
        assert_eq!(RANK_BAR.duration, Duration::from_millis(500));
        assert_eq!(TOAST_SLIDE_IN.duration, Duration::from_millis(250));
        // 豁免面:reduce 段里被点名的那些不受闸影响,toast 不在名单里
        for exempt in [
            OVERLAY_IN,
            OVERLAY_OUT,
            MENU_IN,
            TERMINAL_SWAP,
            TAB_INDICATOR,
            PANE_ENTER,
            SECTION_TOGGLE,
            USAGE_FADE_IN,
            RANK_BAR,
        ] {
            assert!(!exempt.respects_reduce, "{exempt:?} 应属豁免面");
        }
        assert!(
            TOAST_SLIDE_IN.respects_reduce,
            "toastSlideIn 在原版 reduce 段没有豁免,必须过闸"
        );
        assert_eq!(TAG_FADE_IN.duration, Duration::from_millis(300));
        assert!(
            TAG_FADE_IN.respects_reduce,
            "tagFadeIn 在原版 reduce 段没有豁免(注释里被点名当反例),必须过闸"
        );
    }

    /// `menuPopIn`:淡入 + 从上方 6px 落位,终点严丝合缝(残留位移会让菜单
    /// 永远偏一点、残留透明度会让它永远蒙一层)。
    #[test]
    fn 菜单进场从上方落位() {
        let (a0, dy0) = menu_pop_in(0.0);
        assert_eq!(a0, 0.0);
        assert_eq!(dy0, -MENU_POP_RISE_PX, "起点在终位上方 6px");
        let (a1, dy1) = menu_pop_in(1.0);
        assert_eq!(a1, 1.0);
        assert_eq!(dy1, 0.0);
        // 越界一律钳住
        assert_eq!(menu_pop_in(1.5), (1.0, 0.0));
        assert_eq!(menu_pop_in(-1.0), (0.0, -MENU_POP_RISE_PX));
        // 半程:位移与不透明度都过半
        let (a, dy) = menu_pop_in(0.5);
        assert!((a - 0.5).abs() < 1e-6);
        assert!((dy + 3.0).abs() < 1e-6, "{dy}");
    }

    /// `tagFadeIn` 的过冲:60% 处到 1.15 再回落到 1,不透明度**提前**到顶。
    #[test]
    fn 完成标进场先过冲再回落() {
        let (a0, s0) = tag_fade_in(0.0);
        assert_eq!(a0, 0.0);
        assert!((s0 - 0.6).abs() < 1e-6);
        let (a_peak, s_peak) = tag_fade_in(0.6);
        assert!((a_peak - 1.0).abs() < 1e-6, "60% 处已经全不透明");
        assert!((s_peak - 1.15).abs() < 1e-6, "60% 处是过冲顶点");
        let (a1, s1) = tag_fade_in(1.0);
        assert_eq!(a1, 1.0);
        assert!((s1 - 1.0).abs() < 1e-6, "终点必须正好回到 1,不许停在 1.15");
        // 越界钳住 + 前段单调涨、后段单调落
        assert_eq!(tag_fade_in(2.0), tag_fade_in(1.0));
        assert_eq!(tag_fade_in(-1.0), tag_fade_in(0.0));
        assert!(tag_fade_in(0.3).1 < tag_fade_in(0.5).1);
        assert!(tag_fade_in(0.7).1 > tag_fade_in(0.9).1);
        // 不透明度全程不回头
        let mut prev = -1.0;
        for i in 0..=40 {
            let a = tag_fade_in(i as f32 / 40.0).0;
            assert!(a >= prev - 1e-6, "在 {i} 处回头了");
            prev = a;
        }
    }

    #[test]
    fn 过渡建出来即在跑_settled_则已完成() {
        let spec = TransitionSpec::exempt(Duration::from_millis(300), Easing::Linear);
        let running = Transition::new(spec);
        assert!(running.running());
        assert!(running.progress() < 0.5, "刚建出来应接近 0");
        let done = Transition::settled(spec);
        assert!(!done.running());
        assert_eq!(done.progress(), 1.0);
    }

    #[test]
    fn 重启是从头播不是接着播() {
        let spec = TransitionSpec::exempt(Duration::from_millis(200), Easing::Linear);
        let mut tr = Transition::settled(spec);
        assert_eq!(tr.progress(), 1.0);
        tr.restart();
        assert!(tr.progress() < 0.2, "restart 之后必须回到起点附近");
        assert!(tr.running());
    }

    #[test]
    fn 值补间从当前显示值接着补() {
        let spec = TransitionSpec::exempt(Duration::from_millis(400), Easing::Linear);
        let t0 = Instant::now();
        let mut tween = ValueTween::settled(0.0, spec);
        assert_eq!(tween.value_at(t0), 0.0, "首次出现不补间");

        tween.retarget_at(1.0, t0);
        assert!((tween.value_at(t0 + Duration::from_millis(200)) - 0.5).abs() < 1e-4);
        // 半路改目标:起点是**当前显示值** 0.5,不是 0 也不是 1
        let mid = t0 + Duration::from_millis(200);
        tween.retarget_at(0.0, mid);
        assert!((tween.value_at(mid) - 0.5).abs() < 1e-4);
        assert!((tween.value_at(mid + Duration::from_millis(200)) - 0.25).abs() < 1e-4);
        assert_eq!(tween.value_at(mid + Duration::from_millis(400)), 0.0);
        assert!(!tween.running_at(mid + Duration::from_millis(400)));
    }

    #[test]
    fn 同值重设目标不重启补间() {
        // 每帧都拿同一个目标值调 retarget —— 若不去重会永远停在起点
        let spec = TransitionSpec::exempt(Duration::from_millis(400), Easing::Linear);
        let t0 = Instant::now();
        let mut tween = ValueTween::settled(0.0, spec);
        tween.retarget_at(1.0, t0);
        for step in [0u64, 50, 100, 150, 200] {
            let now = t0 + Duration::from_millis(step);
            tween.retarget_at(1.0, now);
            assert!(
                (tween.value_at(now) - step as f32 / 400.0).abs() < 1e-4,
                "第 {step}ms 的值被重启拖回去了"
            );
        }
    }

    #[test]
    fn 补间表首次落目标_变值才动_没读到就丢() {
        let t0 = Instant::now();
        let mut map = TweenMap::new(TransitionSpec::exempt(
            Duration::from_millis(500),
            Easing::Linear,
        ));
        assert_eq!(map.value_at("a", 0.8, t0), 0.8, "首次出现直接落目标值");
        assert_eq!(map.value_at("b", 0.2, t0), 0.2);
        assert_eq!(map.len(), 2);

        // 目标变了:从旧值补过去
        assert!((map.value_at("a", 0.4, t0) - 0.8).abs() < 1e-4);
        let mid = t0 + Duration::from_millis(250);
        assert!((map.value_at("a", 0.4, mid) - 0.6).abs() < 1e-4);
        assert!(map.running_at(mid));

        // 本轮 a、b 都读过 → sweep 一次两条都留着
        map.sweep();
        assert_eq!(map.len(), 2);
        // 下一轮只读了 a(b 那行从界面上没了)→ 再 sweep,b 被丢掉
        map.value_at("a", 0.4, mid);
        map.sweep();
        assert_eq!(map.len(), 1);
        // 重新出现按「首次出现」处理:直接落目标值,不从旧值补
        assert_eq!(map.value_at("b", 0.9, mid), 0.9);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn 补间表跑完就不再需要帧() {
        let t0 = Instant::now();
        let mut map = TweenMap::new(TransitionSpec::exempt(
            Duration::from_millis(100),
            Easing::Linear,
        ));
        map.value_at("a", 0.0, t0);
        map.value_at("a", 1.0, t0);
        assert!(map.running_at(t0));
        assert!(!map.running_at(t0 + Duration::from_millis(100)));
        // 空表也不该要帧
        map.sweep();
        map.sweep();
        assert!(map.is_empty());
        assert!(!map.running_at(t0));
    }
}
