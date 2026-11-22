//! 终端重绘的**全局节拍器**。所有 pane 共用一条,替代此前「每个 pane 自带一个
//! 16ms 定时器,到点自己 `cx.notify()`」的做法。
//!
//! # 为什么要有这一层
//!
//! GPUI 没有局部重绘:**一次 `cx.notify()` = 整窗重画**。CPU 侧的 view 级缓存
//! 只对**显式调过 `.cached(style)`** 的 view 生效(`gpui::AnyView` 才有
//! `cached_style`;`Entity<V>` 直接当元素用是无条件重跑 `render` 的),挂载点与
//! 逐面板的取舍见 `main.rs::cached_panel`;GPU 侧则压根没有 —— paint 出来的
//! scene 每帧都是全量的,终端 glyph + 文件树 + 会话面板一起重画。
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
//! 2. **降频**:前台默认 [`DEFAULT_FOREGROUND_FPS`](30fps)。终端 30fps 与 60fps
//!    肉眼无差,帧数直接减半。
//! 3. **失焦降频**:窗口失焦(但仍看得见)时默认 [`DEFAULT_BACKGROUND_FPS`](5fps)。
//!    挂着 AI 跑、人切去浏览器是常态,那时按满帧重绘整窗是纯浪费。
//! 4. **不可见即停**:窗口最小化 / 被系统判为不呈现时**一帧都不画**,泵连同
//!    定时器一起收摊(见 [`set_window_visible`])。
//!
//! 前台 / 后台两档帧率在设置页可调(系统 → 性能),由 store 经
//! [`set_frame_rates`] 下发,改完当场生效。
//!
//! # 手感:leading edge 不欠债
//!
//! 节流取**前沿**语义 —— 空闲时来的第一次请求**当场画**,之后才进节拍合并。
//! 换成后沿的话,空闲状态下敲一个字要等满一拍(33ms)才看见回显,那是把省下来的
//! GPU 拿用户的手感去换。刷屏时前沿与后沿等价,合并该省的照样省。
//!
//! # 换挡不等旧节拍睡醒
//!
//! 泵是「睡一拍 → flush」的循环,睡的时长在入睡那一刻就定了。切回前台 / 在设置页
//! 调快帧率时,在跑的那条还睡在旧节拍上(后台最低 1fps,一觉就是 1s),等它睡醒
//! 再换档,用户会看见一段冻住的画面。所以这两处**直接换一条新泵**:每条泵带一个
//! 代号,换代后旧泵醒来对不上号就悄悄退场(见 [`Schedule::restart`])。
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
use std::ops::RangeInclusive;
use std::time::Duration;

use gpui::{App, Entity, Subscription, WeakEntity};

use crate::pane::TerminalPane;
use crate::store::AppStore;
use crate::tree::SplitNode;

/// 前台默认帧率:30fps。
///
/// 终端不是游戏,30fps 与 60fps 在滚动文本上肉眼无差 —— 而 GPUI 一次 notify 是
/// 整窗重画,这一档直接把帧数砍半。
pub const DEFAULT_FOREGROUND_FPS: u32 = 30;

/// 后台默认帧率:5fps。
///
/// 窗口**失焦但仍看得见**时用(另一扇窗口盖在上面、多屏摆在旁边)。
/// **不取 0(彻底停)** 是刻意的:画面还在人眼前,要是等下一次输出才重绘,
/// 用户会看见一段陈旧画面;5fps 保证最坏情况下也就落后 200ms,而
/// [`set_window_active`] 在切回前台时还会再当场 flush 一次兜住。
///
/// ⚠️ 别把这一档与「不可见」混为一谈:最小化走的是 [`set_window_visible`]
/// 那条**真·0 帧**的路,两者互不干扰。
pub const DEFAULT_BACKGROUND_FPS: u32 = 5;

/// 前台帧率的可选区间。
///
/// 下限 10:泵跑起来之后的回显要等下一拍,再低连续打字就一顿一顿的。
/// 上限 240:帧是平台按显示器 vsync 要的(Windows 由 `DwmFlush` 给节拍),
/// notify 再密也画不出超过刷新率的帧,再往上填没有意义。
pub const FOREGROUND_FPS: RangeInclusive<u32> = 10..=240;

/// 后台帧率的可选区间。**下限 1 而不是 0**,理由见 [`DEFAULT_BACKGROUND_FPS`]。
pub const BACKGROUND_FPS: RangeInclusive<u32> = 1..=60;

/// 判定某个 `pty_id` 当前在屏幕上是否可见(是否应驱动全局重绘节拍器)。
///
/// 可见条件:
/// 1. 工作区停在终端页(`terminal_page_active == true`,不在文件 / diff 页签);
/// 2. `target_pty_id` 属于以下任意之一:
///    - 活动项目活动面板的当前可视激活 tab(若存在最大化叶子,仅限该叶子的激活 tab;
///      否则是活动布局中全部叶子的激活 tab);
///    - 活动布局中正处于叶内切 tab 推出动画(`tab_swaps`)的旧 pane;
///    - 正处于切面板出场动画(`AreaExitSrc::Panel`)中的旧面板可视激活 tab。
///
/// 注:`pane_preview` 悬停缩略图使用独立的 `mt_ui::MiniTerminalElement`(自带 500ms
/// 刷新并直接读 `Arc<TerminalEmulator>`),不渲染 `TerminalPane` 也不经过全局节拍器。
pub(crate) fn pty_is_visible_in_layout<'a>(
    terminal_page_active: bool,
    active_layout: Option<&SplitNode>,
    maximized_pane_id: Option<&str>,
    exit_panel_layout: Option<&SplitNode>,
    tab_swap_old_pane_ids: impl IntoIterator<Item = &'a str>,
    target_pty_id: u32,
) -> bool {
    if !terminal_page_active {
        return false;
    }
    if let Some(layout) = active_layout {
        let scope = maximized_pane_id
            .and_then(|id| layout.leaf_of_pane(id))
            .unwrap_or(layout);
        if scope
            .visible_panes()
            .into_iter()
            .any(|p| p.pty_id == Some(target_pty_id))
        {
            return true;
        }
        for old_id in tab_swap_old_pane_ids {
            if scope
                .pane(old_id)
                .is_some_and(|p| p.pty_id == Some(target_pty_id))
            {
                return true;
            }
        }
    }
    if let Some(old_layout) = exit_panel_layout
        && old_layout
            .visible_panes()
            .into_iter()
            .any(|p| p.pty_id == Some(target_pty_id))
    {
        return true;
    }
    false
}

/// 列出活动项目当前在终端页上可见的全部 `pty_id`(切回可见时统一补刷用)。
pub(crate) fn visible_pty_ids_in_layout(
    active_layout: Option<&SplitNode>,
    maximized_pane_id: Option<&str>,
) -> Vec<u32> {
    let Some(layout) = active_layout else {
        return Vec::new();
    };
    let scope = maximized_pane_id
        .and_then(|id| layout.leaf_of_pane(id))
        .unwrap_or(layout);
    scope
        .visible_panes()
        .into_iter()
        .filter_map(|p| p.pty_id)
        .collect()
}

/// 切回终端页 / 切项目 / 切面板 / 切 tab / 退出最大化时,把当前可见且在后台
/// 跳过过重绘的 `TerminalPane` 立即补刷一次(同步 `TerminalView` 与回看浮钮)。
pub(crate) fn flush_visible_hidden_panes(store: &Entity<AppStore>, cx: &mut App) {
    let dirty_panes: Vec<Entity<TerminalPane>> = {
        let store = store.read(cx);
        let Some(project_id) = store.active_project_id.as_deref() else {
            return;
        };
        let active_layout = store
            .project_state(project_id)
            .and_then(|s| s.active_layout());
        let maximized_pane_id = store.maximized_pane_id(project_id);
        visible_pty_ids_in_layout(active_layout, maximized_pane_id)
            .into_iter()
            .filter_map(|pty_id| store.terminal(pty_id).cloned())
            .filter(|pane| pane.read(cx).is_dirty_while_hidden())
            .collect()
    };
    for pane in dirty_panes {
        pane.update(cx, |pane, cx| pane.flush_hidden_redraw(cx));
    }
}

/// 配置值 → 实际生效的前台帧率。`None` = 默认;越界钳回区间(手改坏的配置)。
pub fn resolve_foreground_fps(fps: Option<u32>) -> u32 {
    resolve_fps(fps, DEFAULT_FOREGROUND_FPS, &FOREGROUND_FPS)
}

/// 配置值 → 实际生效的后台帧率。口径同 [`resolve_foreground_fps`]。
pub fn resolve_background_fps(fps: Option<u32>) -> u32 {
    resolve_fps(fps, DEFAULT_BACKGROUND_FPS, &BACKGROUND_FPS)
}

fn resolve_fps(fps: Option<u32>, default: u32, range: &RangeInclusive<u32>) -> u32 {
    fps.map_or(default, |fps| fps.clamp(*range.start(), *range.end()))
}

/// 帧率 → 一拍的时长。`0` 按 1fps 算,不除零。
fn period_of(fps: u32) -> Duration {
    Duration::from_micros(1_000_000 / u64::from(fps.max(1)))
}

thread_local! {
    static PUMP: RefCell<Pump> = RefCell::new(Pump::default());
}

/// 节拍器本体。
#[derive(Default)]
struct Pump {
    /// 这一拍攒下来、等着 `notify` 的 pane。**按 `EntityId` 去重** ——
    /// 一个 pane 在一拍里刷了一百次屏,也只该画一帧。
    pending: Vec<WeakEntity<TerminalPane>>,
    schedule: Schedule,
    /// 「窗口可见性」订阅句柄。一 drop 就退订,所以必须存住;
    /// `Some` 同时充当「已经挂过了」的标记。见 [`ensure_visibility_observer`]。
    visibility: Option<Subscription>,
}

/// 泵的调度状态机。**刻意不含任何 gpui 类型** —— 节拍与停泵的判断全在这里,
/// 单测就冲它来(`WeakEntity` / `App` 在测试里造不出来)。
#[derive(Debug, PartialEq, Eq)]
struct Schedule {
    /// 窗口在前台吗。见 [`set_window_active`]。
    active: bool,
    /// 窗口的画面**在被呈现**吗。最小化 / 显示器休眠时为 `false`,
    /// 见 [`set_window_visible`]。
    visible: bool,
    /// 泵正在跑吗。同一时刻只该有一条。
    running: bool,
    /// 当代泵的代号。每起一条泵换一个号,只有号对得上的那条能 flush / 收摊 ——
    /// 换下来的旧泵醒来对不上号就自行退场。见 [`Schedule::restart`]。
    generation: u64,
    /// 前台一拍。见 [`set_frame_rates`]。
    active_period: Duration,
    /// 后台一拍。
    idle_period: Duration,
}

impl Default for Schedule {
    fn default() -> Self {
        // 窗口起来就是前台且可见的;真实状态随后由 `set_window_active` /
        // `set_window_visible` 校正,帧率由 `set_frame_rates` 按配置校正
        Self {
            active: true,
            visible: true,
            running: false,
            generation: 0,
            active_period: period_of(DEFAULT_FOREGROUND_FPS),
            idle_period: period_of(DEFAULT_BACKGROUND_FPS),
        }
    }
}

impl Schedule {
    /// 这一拍该睡多久。
    fn period(&self) -> Duration {
        if self.active {
            self.active_period
        } else {
            self.idle_period
        }
    }

    /// 登记了一次重绘请求。返回**是否需要起泵**(泵已经在跑就不重复起)。
    /// 要起的话代号已经换好,新泵拿 [`Self::generation`] 当自己的号。
    ///
    /// 窗口不可见时恒 `false`:请求只在 `pending` 里攒着,一帧都不画,
    /// 等 [`set_window_visible`] 把它们一次性兑现。
    fn arm(&mut self) -> bool {
        if self.running || !self.visible {
            return false;
        }
        self.running = true;
        self.generation = self.generation.wrapping_add(1);
        true
    }

    /// 把在跑的泵换成一条新的。返回新泵的代号;没有泵在跑时返回 `None` ——
    /// 下一次请求起泵时自然按新节拍睡,不必换。
    ///
    /// 在跑的那条正睡在**旧节拍**上,换代后它醒来对不上号,不 flush、不动
    /// `running`,悄悄退场;`running` 归新泵管。
    fn restart(&mut self) -> Option<u64> {
        if !self.running {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        Some(self.generation)
    }

    /// 醒来的这条泵还是不是当代的。
    fn is_current(&self, generation: u64) -> bool {
        self.running && self.generation == generation
    }

    /// 换帧率。节拍真变了且泵在跑就换泵(返回新泵代号),理由见 [`Self::restart`]。
    fn set_periods(&mut self, active: Duration, idle: Duration) -> Option<u64> {
        if self.active_period == active && self.idle_period == idle {
            return None;
        }
        self.active_period = active;
        self.idle_period = idle;
        self.restart()
    }

    /// 一拍走完。`had_work` = 这一拍有没有 flush 到东西。
    ///
    /// 返回**是否该停泵**:空跑一拍就收摊,别让一条 33ms 的定时器在没人用的时候
    /// 一直转下去(那正是这个模块要消灭的东西)。窗口不可见时同样收摊 ——
    /// 那一拍压根没 flush,连定时器都不该留着。
    fn tick(&mut self, had_work: bool) -> bool {
        if had_work && self.visible {
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
    ensure_visibility_observer(cx);

    let start = PUMP.with(|pump| {
        let mut pump = pump.borrow_mut();
        let id = pane.entity_id();
        if !pump.pending.iter().any(|p| p.entity_id() == id) {
            pump.pending.push(pane);
        }
        pump.schedule.arm().then_some(pump.schedule.generation)
    });
    let Some(generation) = start else {
        return;
    };

    // 前沿:空闲时的第一次请求当场兑现,不欠用户一拍的回显延迟
    flush_visible(cx);
    spawn_pump(generation, cx);
}

/// 起一条代号为 `generation` 的泵:按当前档位睡一拍 → flush → 空跑一拍就收摊。
///
/// 醒来发现自己已经被换下([`Schedule::restart`])就直接退场,不 flush 也不动
/// `running` —— 那时新泵已经接班。
fn spawn_pump(generation: u64, cx: &mut App) {
    // gpui-pre 的 `AsyncApp::update` 不再会失败:App 退出时这个任务连同循环一起
    // 被丢弃,不必再有「App 没了就收 running」的尾巴。
    cx.spawn(async move |cx| {
        loop {
            let period = PUMP.with(|pump| pump.borrow().schedule.period());
            cx.background_executor().timer(period).await;
            if !PUMP.with(|pump| pump.borrow().schedule.is_current(generation)) {
                return;
            }
            let had_work = cx.update(flush_visible);
            if PUMP.with(|pump| pump.borrow_mut().schedule.tick(had_work)) {
                return;
            }
        }
    })
    .detach();
}

/// 前台 / 后台两档帧率(设置页那两项,已按 [`resolve_foreground_fps`] /
/// [`resolve_background_fps`] 归一)。启动时 store 也调一次,赶在第一次
/// PTY 输出起泵之前。
///
/// 当场生效:泵在跑就换一条按新节拍走的,不等旧节拍那一觉睡完。
pub fn set_frame_rates(foreground_fps: u32, background_fps: u32, cx: &mut App) {
    let restart = PUMP.with(|pump| {
        pump.borrow_mut()
            .schedule
            .set_periods(period_of(foreground_fps), period_of(background_fps))
    });
    if let Some(generation) = restart {
        spawn_pump(generation, cx);
    }
}

/// 窗口激活状态变了。前台/后台两档节拍靠它切换。
///
/// 切**回**前台时当场 flush 一次:后台那一拍最坏落后一整拍(默认 200ms,
/// 调到 1fps 就是 1s),不能让用户盯着一屏陈旧内容等下一拍。在跑的泵也一并
/// 换掉 —— 它睡的是后台那一拍,不换的话 flush 完这一下画面又会冻到它睡醒。
/// 切去后台不换:旧泵最多再睡一个前台拍就自己换档了。
pub fn set_window_active(active: bool, cx: &mut App) {
    let (changed, restart) = PUMP.with(|pump| {
        let mut pump = pump.borrow_mut();
        let schedule = &mut pump.schedule;
        if schedule.active == active {
            return (false, None);
        }
        schedule.active = active;
        (true, if active { schedule.restart() } else { None })
    });
    if changed && active {
        flush_visible(cx);
    }
    if let Some(generation) = restart {
        spawn_pump(generation, cx);
    }
}

/// 窗口的画面是否在被呈现(gpui-pre 的 [`gpui::WindowVisibility`])。
///
/// # 为什么它与「激活态」是两件事
///
/// 失焦但看得见 → 画面还在人眼前,只能降频(后台那一档帧率);
/// **最小化 / 显示器休眠** → 一个像素都没人看,画多少帧都是纯浪费。
/// gpui 在这种状态下本来就不再向平台要帧了(`platform.rs` 的
/// `WindowVisibility::Hidden` 文档原文:「The platform will not request frames
/// for it until it becomes visible again」),但**我们的泵会把它叫醒** ——
/// `cx.notify` 一路走到 `WindowInvalidator::wake_platform`(`window.rs:229-237`,
/// 由 `invalidate_view`(`window.rs:166-191`)在 `became_dirty` 时调起,
/// 注释写明「so a frame request is delivered even if the platform stops
/// requesting frames for idle windows」)。所以「彻底停泵」这件事只能由这里做。
///
/// 停的方式是**连定时器一起收**:不可见时 `arm` 不起泵、在跑的那条下一拍自停
/// (见 [`Schedule::tick`]),期间的重绘请求照旧攒在 `pending` 里。
/// 恢复可见时当场 flush 一次补上 —— 与 [`set_window_active`] 同款前沿语义,
/// 否则最小化期间 AI 状态变了,还原后徽章会停在旧样子直到下一次输出。
pub fn set_window_visible(visible: bool, cx: &mut App) {
    let changed = PUMP.with(|pump| {
        let mut pump = pump.borrow_mut();
        if pump.schedule.visible == visible {
            return false;
        }
        pump.schedule.visible = visible;
        true
    });
    if changed && visible {
        // 此刻 `visible` 已置位,`flush_visible` 必然放行
        flush_visible(cx);
    }
}

/// 惰性挂上「窗口可见性」订阅,只挂一次(句柄存在泵上,一 drop 就退订)。
///
/// # 为什么挂在这里而不是宿主里
///
/// `set_window_active` 那条是宿主挂的,因为它顺带还要改 store 的「窗口聚焦」态;
/// 可见性**只有这条泵关心**,挂进来就不必让 `Workspace` 多背一个字段。
/// 时机上也安全:第一次 PTY 有输出时窗口必然已经建好,而本函数的唯一调用点
/// [`request`] 是从 pane 的异步任务(唤醒循环、同步块释放)进来的,窗口不在更新中 ——
/// 万一撞上,`AnyWindowHandle::update` 返回 `Err` 而不是 panic
/// (`app.rs:1919-1924` 里那句 `windows.get_mut(id)?.take()?`),下一次输出再试。
///
/// 本程序是单窗(`main.rs` 只 `open_window` 一次),取第一扇即可。
fn ensure_visibility_observer(cx: &mut App) {
    if PUMP.with(|pump| pump.borrow().visibility.is_some()) {
        return;
    }
    let Some(handle) = cx.windows().first().copied() else {
        return;
    };
    let installed = handle.update(cx, |_, window, _| {
        let visible = window.is_visible();
        let subscription = window.observe_window_visibility(|visibility, _window, cx| {
            set_window_visible(visibility.is_visible(), cx);
        });
        (visible, subscription)
    });
    if let Ok((visible, subscription)) = installed {
        PUMP.with(|pump| {
            let mut pump = pump.borrow_mut();
            // 订阅只报**变化**,当前值在这里对齐一次
            pump.schedule.visible = visible;
            pump.visibility = Some(subscription);
        });
    }
}

/// 可见时才画。**所有 flush 调用点统一走这道闸** —— 不可见时一帧都不画,
/// `pending` 原样攒着,等 [`set_window_visible`] 一次性兑现。
///
/// 返回值同 [`flush`],不可见时恒 `false`:泵的 [`Schedule::tick`] 据此收摊。
fn flush_visible(cx: &mut App) -> bool {
    if !PUMP.with(|pump| pump.borrow().schedule.visible) {
        return false;
    }
    flush(cx)
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
        // gpui-fast 下子视图 `TerminalView` 默认保留复用,经 `notify_redraw` 直接
        // 标脏 `TerminalView`(回看浮钮状态变化时顺带标脏外壳 `TerminalPane`)。
        let _ = pane.update(cx, |pane, cx| pane.notify_redraw(cx));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVE_PERIOD: Duration = Duration::from_micros(1_000_000 / 30);
    const IDLE_PERIOD: Duration = Duration::from_millis(200);

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
    fn 默认后台那一档必须明显慢于前台() {
        // 「后台降频」是这个模块的立身之本之一,默认值拉平了就等于没做
        const {
            assert!(DEFAULT_FOREGROUND_FPS >= DEFAULT_BACKGROUND_FPS * 4);
        }
    }

    #[test]
    fn 默认前台节拍不低于三十帧() {
        // 再慢下去滚动就该有台阶感了 —— 这是默认手感的下限,不是随手填的数
        assert!(period_of(DEFAULT_FOREGROUND_FPS) <= Duration::from_millis(34));
    }

    #[test]
    fn 默认帧率落在可选区间里() {
        assert!(FOREGROUND_FPS.contains(&DEFAULT_FOREGROUND_FPS));
        assert!(BACKGROUND_FPS.contains(&DEFAULT_BACKGROUND_FPS));
    }

    #[test]
    fn 配置帧率的归一() {
        assert_eq!(resolve_foreground_fps(None), 30);
        assert_eq!(resolve_background_fps(None), 5);
        assert_eq!(resolve_foreground_fps(Some(60)), 60);
        assert_eq!(resolve_background_fps(Some(10)), 10);
        // 手改坏的配置钳回区间,不许 0fps 把前台画面冻住
        assert_eq!(resolve_foreground_fps(Some(0)), 10);
        assert_eq!(resolve_foreground_fps(Some(1000)), 240);
        assert_eq!(resolve_background_fps(Some(0)), 1);
        assert_eq!(resolve_background_fps(Some(999)), 60);
    }

    #[test]
    fn 帧率换算成节拍() {
        assert_eq!(period_of(30), ACTIVE_PERIOD);
        assert_eq!(period_of(5), IDLE_PERIOD);
        assert_eq!(period_of(1), Duration::from_secs(1));
        assert_eq!(period_of(0), Duration::from_secs(1), "0 不许除零");
    }

    #[test]
    fn 换帧率只在节拍真变了且泵在跑时换泵() {
        let mut s = Schedule::default();
        // 泵没跑:只记下新节拍,下一次起泵自然按它睡
        assert_eq!(s.set_periods(period_of(60), period_of(10)), None);
        assert_eq!(s.period(), period_of(60));
        s.active = false;
        assert_eq!(s.period(), period_of(10));

        s.arm();
        // 没变:不折腾
        assert_eq!(s.set_periods(period_of(60), period_of(10)), None);
        // 变了:换泵,旧泵下岗
        let old = s.generation;
        let new = s
            .set_periods(period_of(120), period_of(10))
            .expect("泵在跑要换泵");
        assert_ne!(new, old);
        assert!(!s.is_current(old), "旧泵醒来要对不上号");
        assert!(s.is_current(new));
        assert!(s.running, "换泵不是停泵");
    }

    #[test]
    fn 换泵只在泵跑着时发生() {
        let mut s = Schedule::default();
        assert_eq!(s.restart(), None, "没有泵在跑就没东西可换");
        s.arm();
        let first = s.generation;
        let second = s.restart().unwrap();
        assert!(!s.is_current(first));
        assert!(s.is_current(second));
    }

    #[test]
    fn 每起一条泵换一个代号() {
        // 停了再起的新泵与早先换下的旧泵不能撞号,否则旧泵醒来会冒充当代
        let mut s = Schedule::default();
        s.arm();
        let first = s.generation;
        s.tick(false);
        assert!(!s.is_current(first), "收摊之后谁都不是当代泵");
        s.arm();
        assert!(!s.is_current(first));
        assert!(s.is_current(s.generation));
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

    #[test]
    fn 不可见时一拍都不起() {
        // 最小化:请求照收(攒在 pending 里),但一帧都不画
        let mut s = Schedule {
            visible: false,
            ..Default::default()
        };
        assert!(!s.arm(), "不可见时不许起泵");
        assert!(!s.running);
        assert!(!s.arm());
    }

    #[test]
    fn 跑着的泵在窗口不可见时自停() {
        // 有活也停 —— 那一拍压根没 flush,留着定时器就是白转
        let mut s = Schedule::default();
        s.arm();
        s.visible = false;
        assert!(s.tick(false), "不可见时即便上一拍有活也该收摊");
        assert!(!s.running);
    }

    #[test]
    fn 恢复可见后泵能重新起来() {
        let mut s = Schedule {
            visible: false,
            ..Default::default()
        };
        assert!(!s.arm());
        s.visible = true;
        assert!(s.arm(), "恢复可见后下一次输出要能把泵重新起起来");
        assert!(s.running);
    }

    #[test]
    fn 失焦与不可见是两档互不干扰() {
        // 失焦但看得见:降到后台那一档,照常起泵
        let mut s = Schedule {
            active: false,
            ..Default::default()
        };
        assert!(s.arm());
        assert_eq!(s.period(), IDLE_PERIOD);
        // 再最小化:这一档才是真的 0 帧
        s.visible = false;
        assert!(s.tick(true));
        assert!(!s.arm());
    }

    fn test_pane(id: &str, pty_id: u32) -> crate::tree::PaneState {
        let mut p = crate::tree::PaneState::new("pwsh");
        p.id = id.to_string();
        p.pty_id = Some(pty_id);
        p
    }

    fn test_leaf(id: &str, panes: Vec<crate::tree::PaneState>, active_index: usize) -> SplitNode {
        let active_pane_id = panes[active_index].id.clone();
        SplitNode::Leaf {
            id: id.to_string(),
            panes,
            active_pane_id,
        }
    }

    #[test]
    fn 停在文档页签时所有终端均不可见() {
        let layout = test_leaf("leaf-1", vec![test_pane("p1", 10)], 0);
        assert!(!pty_is_visible_in_layout(
            false,
            Some(&layout),
            None,
            None,
            std::iter::empty(),
            10,
        ));
    }

    #[test]
    fn 同组未激活tab与后台面板不判为可见() {
        let leaf_a = test_leaf("leaf-a", vec![test_pane("p1", 1), test_pane("p2", 2)], 0);
        let leaf_b = test_leaf("leaf-b", vec![test_pane("p3", 3)], 0);
        let split = SplitNode::Split {
            id: "split-1".into(),
            direction: crate::tree::SplitDirection::Horizontal,
            children: vec![leaf_a, leaf_b],
            sizes: vec![50.0, 50.0],
        };
        assert!(pty_is_visible_in_layout(
            true,
            Some(&split),
            None,
            None,
            std::iter::empty(),
            1,
        ));
        assert!(pty_is_visible_in_layout(
            true,
            Some(&split),
            None,
            None,
            std::iter::empty(),
            3,
        ));
        // 同组未激活 tab
        assert!(!pty_is_visible_in_layout(
            true,
            Some(&split),
            None,
            None,
            std::iter::empty(),
            2,
        ));
        // 后台面板或非激活项目的 pty
        assert!(!pty_is_visible_in_layout(
            true,
            Some(&split),
            None,
            None,
            std::iter::empty(),
            99,
        ));
        assert_eq!(visible_pty_ids_in_layout(Some(&split), None), vec![1, 3]);
    }

    #[test]
    fn 切tab与切面板出场动画中的旧pane保持可见() {
        let layout = test_leaf("leaf-a", vec![test_pane("p1", 1), test_pane("p2", 2)], 1);
        let old_panel = test_leaf(
            "leaf-old",
            vec![test_pane("p-old-active", 20), test_pane("p-old-bg", 21)],
            0,
        );
        // p1 已不是激活 tab,但正在 tab_swaps 推出动画中 → 可见
        assert!(pty_is_visible_in_layout(
            true,
            Some(&layout),
            None,
            Some(&old_panel),
            ["p1"],
            1,
        ));
        // 旧面板的激活 tab 在切面板退场动画期间仍可见,未激活 tab 不可见
        assert!(pty_is_visible_in_layout(
            true,
            Some(&layout),
            None,
            Some(&old_panel),
            std::iter::empty(),
            20,
        ));
        assert!(!pty_is_visible_in_layout(
            true,
            Some(&layout),
            None,
            Some(&old_panel),
            std::iter::empty(),
            21,
        ));
    }

    #[test]
    fn 最大化时只判最大化叶子的激活tab可见() {
        let leaf_a = test_leaf("leaf-a", vec![test_pane("p1", 1), test_pane("p2", 2)], 0);
        let leaf_b = test_leaf("leaf-b", vec![test_pane("p3", 3)], 0);
        let split = SplitNode::Split {
            id: "split-1".into(),
            direction: crate::tree::SplitDirection::Horizontal,
            children: vec![leaf_a, leaf_b],
            sizes: vec![50.0, 50.0],
        };
        assert!(pty_is_visible_in_layout(
            true,
            Some(&split),
            Some("p1"),
            None,
            std::iter::empty(),
            1,
        ));
        // 折叠掉的另一个叶子在最大化期间不可见
        assert!(!pty_is_visible_in_layout(
            true,
            Some(&split),
            Some("p1"),
            None,
            std::iter::empty(),
            3,
        ));
        // 最大化叶子内正在推出的旧 tab 仍可见
        assert!(pty_is_visible_in_layout(
            true,
            Some(&split),
            Some("p1"),
            None,
            ["p2"],
            2,
        ));
        assert_eq!(visible_pty_ids_in_layout(Some(&split), Some("p1")), vec![1]);
    }
}
