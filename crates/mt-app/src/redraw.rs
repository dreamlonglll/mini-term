//! 终端重绘的**全局节拍器**。所有 pane 共用一条,替代此前「每个 pane 自带一个
//! 16ms 定时器,到点自己 `cx.notify()`」的做法。
//!
//! # 为什么要有这一层
//!
//! GPUI 没有局部重绘:**一次 `cx.notify()` = 整窗重画**。CPU 侧还有 view 级缓存
//! 兜着(`gpui::AnyView` 按 `dirty_views` 跳过没变的子树),GPU 侧没有 ——
//! paint 出来的 scene 每帧都是全量的,终端 glyph + 文件树 + 会话面板一起重画。
//!
//! 于是「PTY 一有输出就 notify」这条看着无害的路,在实测里是这样的
//! (Windows / 74Hz 屏 / 7 个 pane 其中 3 个在跑 claude):
//!
//! ```text
//! mini-term GPU 3D 引擎   5% ~ 20%(随输出活跃度波动)
//! dwm.exe 被带起来        2.5% ~ 16%
//! 主线程                  57% 一个核 —— 其余 142 个线程加起来 ≈ 0
//! ```
//!
//! 那 0.57 个核才是真正的代价(任务管理器的 GPU% 统计的是**引擎时间片占用比**,
//! 不是算力;同一时刻 `nvidia-smi` 报整卡只有 6%)。笔记本上它就是风扇和续航。
//!
//! # 这一层做了三件事
//!
//! 1. **合并**:N 个 pane 同时刷屏,一拍只 flush 一次,所有 `notify` 落在同一个
//!    App 更新周期里 → GPUI 合成**一帧**。此前是每个 pane 一个独立定时器,相位
//!    互相错开,notify 频率 = N × 62Hz,等于每个 vsync 都撞上一次 dirty。
//! 2. **降频**:前台 [`ACTIVE_PERIOD`](30fps)。终端 30fps 与 60fps 肉眼无差,
//!    帧数直接减半。
//! 3. **后台大幅降频**:窗口失焦/最小化时 [`IDLE_PERIOD`](5fps)。挂着 AI 跑、
//!    人切去浏览器是常态,那时按满帧重绘整窗是纯浪费。
//!
//! # 手感:leading edge 不欠债
//!
//! 节流取**前沿**语义 —— 空闲时来的第一次请求**当场画**,之后才进节拍合并。
//! 换成后沿的话,空闲状态下敲一个字要等满一拍(33ms)才看见回显,那是把省下来的
//! GPU 拿用户的手感去换。刷屏时前沿与后沿等价,合并该省的照样省。
//!
//! # 边界:终端应答不走这里
//!
//! `PtyWrite` / DA / DSR / OSC 那些**是终端要回给程序的应答**,与「画不画」无关,
//! 晚一拍会让对面的 TUI 干等 —— 它们仍留在 `pane` 自己的唤醒循环里按原节奏处理
//! (见 `pane::TerminalPane::drain_term_events` 的调用点)。进这里的只有 `notify`。
//!
//! 同理 **PTY 退出**也不进节拍器:一次性事件,当场画完收工。
//!
//! # 为什么不用 `gpui::Global`
//!
//! `App::global_mut` / `default_global` 每次调用都会 push 一个
//! `Effect::NotifyGlobalObservers`。在一个**专为省重绘而生**的热路径上用它是反的。
//! 这里的状态全部活在主线程(`WeakEntity` 本来就不是 `Send`),`thread_local` 够用
//! 且零通知 —— 与 `crate::motion` 那道进程级闸同一种朴素做法。

use std::cell::RefCell;
use std::time::Duration;

use gpui::{App, WeakEntity};

use crate::pane::TerminalPane;

/// 前台节拍:30fps。
///
/// 终端不是游戏,30fps 与 60fps 在滚动文本上肉眼无差 —— 而 GPUI 一次 notify 是
/// 整窗重画,这一档直接把帧数砍半。
const ACTIVE_PERIOD: Duration = Duration::from_millis(33);

/// 后台节拍:5fps。
///
/// 窗口失焦/最小化时用。**不取 0(彻底停)** 是刻意的:切回来那一瞬间要是还在等
/// 下一次输出才重绘,用户会看见一段陈旧画面;5fps 保证最坏情况下也就落后 200ms,
/// 而 [`set_window_active`] 在切回前台时还会再当场 flush 一次兜住。
const IDLE_PERIOD: Duration = Duration::from_millis(200);

thread_local! {
    static PUMP: RefCell<Pump> = RefCell::new(Pump::default());
}

/// 节拍器本体。
struct Pump {
    /// 这一拍攒下来、等着 `notify` 的 pane。**按 `EntityId` 去重** ——
    /// 一个 pane 在一拍里刷了一百次屏,也只该画一帧。
    pending: Vec<WeakEntity<TerminalPane>>,
    schedule: Schedule,
}

impl Default for Pump {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            schedule: Schedule::default(),
        }
    }
}

/// 泵的调度状态机。**刻意不含任何 gpui 类型** —— 节拍与停泵的判断全在这里,
/// 单测就冲它来(`WeakEntity` / `App` 在测试里造不出来)。
#[derive(Debug, PartialEq, Eq)]
struct Schedule {
    /// 窗口在前台吗。见 [`set_window_active`]。
    active: bool,
    /// 泵正在跑吗。同一时刻只该有一条。
    running: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        // 窗口起来就是前台的;真实状态随后由 `set_window_active` 校正
        Self {
            active: true,
            running: false,
        }
    }
}

impl Schedule {
    /// 这一拍该睡多久。
    fn period(&self) -> Duration {
        if self.active {
            ACTIVE_PERIOD
        } else {
            IDLE_PERIOD
        }
    }

    /// 登记了一次重绘请求。返回**是否需要起泵**(泵已经在跑就不重复起)。
    fn arm(&mut self) -> bool {
        if self.running {
            return false;
        }
        self.running = true;
        true
    }

    /// 一拍走完。`had_work` = 这一拍有没有 flush 到东西。
    ///
    /// 返回**是否该停泵**:空跑一拍就收摊,别让一条 33ms 的定时器在没人用的时候
    /// 一直转下去(那正是这个模块要消灭的东西)。
    fn tick(&mut self, had_work: bool) -> bool {
        if had_work {
            return false;
        }
        self.running = false;
        true
    }
}

/// 登记一次重绘。**PTY 有输出时调这个,不要自己 `cx.notify()`**。
///
/// 同一拍里同一个 pane 登记多次只画一帧;多个 pane 一起登记也只画一帧。
pub fn request(pane: WeakEntity<TerminalPane>, cx: &mut App) {
    let start = PUMP.with(|pump| {
        let mut pump = pump.borrow_mut();
        let id = pane.entity_id();
        if !pump.pending.iter().any(|p| p.entity_id() == id) {
            pump.pending.push(pane);
        }
        pump.schedule.arm()
    });
    if !start {
        return;
    }

    // 前沿:空闲时的第一次请求当场兑现,不欠用户一拍的回显延迟
    flush(cx);

    cx.spawn(async move |cx| {
        loop {
            let period = PUMP.with(|pump| pump.borrow().schedule.period());
            cx.background_executor().timer(period).await;
            // App 没了(退出中)——把 running 收干净再走,免得留一个假的「在跑」
            let Ok(had_work) = cx.update(flush) else { break };
            if PUMP.with(|pump| pump.borrow_mut().schedule.tick(had_work)) {
                return;
            }
        }
        PUMP.with(|pump| pump.borrow_mut().schedule.running = false);
    })
    .detach();
}

/// 窗口激活状态变了。前台/后台两档节拍靠它切换。
///
/// 切**回**前台时当场 flush 一次:后台那一拍最坏落后 200ms,不能让用户盯着
/// 一屏陈旧内容等下一拍。
pub fn set_window_active(active: bool, cx: &mut App) {
    let changed = PUMP.with(|pump| {
        let mut pump = pump.borrow_mut();
        if pump.schedule.active == active {
            return false;
        }
        pump.schedule.active = active;
        true
    });
    if changed && active {
        flush(cx);
    }
}

/// 把这一拍攒下的 pane 一次画完。返回**这一拍有没有活干**。
///
/// 所有 `notify` 落在同一次 App 更新里 —— GPUI 于是把它们合成一帧,这正是
/// 「N 个 pane 只画一帧」的落点。
fn flush(cx: &mut App) -> bool {
    let pending = PUMP.with(|pump| std::mem::take(&mut pump.borrow_mut().pending));
    if pending.is_empty() {
        return false;
    }
    for pane in pending {
        // pane 已经关了就跳过 —— 弱引用失效是正常生命周期,不是错误
        let _ = pane.update(cx, |_, cx| cx.notify());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 前台后台两档节拍() {
        let mut s = Schedule::default();
        assert_eq!(s.period(), ACTIVE_PERIOD, "窗口默认按前台算");
        s.active = false;
        assert_eq!(s.period(), IDLE_PERIOD);
        s.active = true;
        assert_eq!(s.period(), ACTIVE_PERIOD);
    }

    #[test]
    fn 后台那一档必须明显慢于前台() {
        // 「后台降频」是这个模块的立身之本之一,拉平了就等于没做
        assert!(IDLE_PERIOD >= ACTIVE_PERIOD * 4);
    }

    #[test]
    fn 前台节拍不低于三十帧() {
        // 再慢下去滚动就该有台阶感了 —— 这是手感的下限,不是随手填的数
        assert!(ACTIVE_PERIOD <= Duration::from_millis(34));
    }

    #[test]
    fn 泵同一时刻只起一条() {
        let mut s = Schedule::default();
        assert!(s.arm(), "第一次登记要把泵起起来");
        assert!(!s.arm(), "泵在跑,后续登记只是搭车");
        assert!(!s.arm());
    }

    #[test]
    fn 空跑一拍就停泵() {
        let mut s = Schedule::default();
        s.arm();
        assert!(!s.tick(true), "这一拍有活,接着跑");
        assert!(!s.tick(true));
        assert!(s.tick(false), "空跑一拍即收摊");
        assert!(!s.running);
    }

    #[test]
    fn 停泵之后还能重新起来() {
        let mut s = Schedule::default();
        s.arm();
        s.tick(false);
        assert!(s.arm(), "下一次输出要能把泵重新起起来");
        assert!(s.running);
    }

    #[test]
    fn 切后台不影响在跑的泵() {
        // 只换节拍,不打断 —— 打断了后台那批输出就永远不画了
        let mut s = Schedule::default();
        s.arm();
        s.active = false;
        assert!(s.running);
        assert_eq!(s.period(), IDLE_PERIOD);
    }
}
