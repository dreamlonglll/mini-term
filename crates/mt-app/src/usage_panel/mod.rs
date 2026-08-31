//! 用量统计面板。对应 `src/components/usage/UsageStatsModal.tsx` 与它的四个子件
//! (KpiCards / DailyChart / RankBarList / TopSessions)+ 两个纯逻辑模块
//! (`utils/usageDates.ts` / `utils/modelPricing.ts`)+ 数据流 hook `useUsageStats.ts`。
//!
//! ```text
//! 打开面板 ─┬─→ background: pricing::ensure_pricing(手工表/24h 缓存/拉 models.dev)
//!           ├─→ background: usage_ledger_query(账本毫秒级,先出现值)
//!           └─→ background: spawn_usage_ledger_sync(增量同步,不阻塞)
//!                    │ SyncEvent(Progress/Synced) ─→ mpsc ─→ 主线程任务 ─→ added>0 才重查
//! 点刷新 ───→ background: ensure_pricing → usage_ledger_sync(blocking) → 回主线程 query
//! 自动刷新 ─→ 定时器只触发**非阻塞同步**,重查由 Synced 事件驱动
//! ```
//!
//! # 三条不许碰的红线
//!
//! 1. **一切阻塞调用丢后台**。`usage_ledger_query` 虽是毫秒级纯查询,但打开连接
//!    可能等 `busy_timeout`(最长 5s),落在 GPUI 主线程上就是整个窗口冻住 ——
//!    mt-usage 的函数注释里写死了这条。`usage_ledger_sync(blocking)` 与拉价的
//!    HTTPS 请求更重,同理。
//! 2. **拿不到价格时绝不渲染 KPI**。全 0 成本会误导,两版共同的红线
//!    (`UsageStatsModal.tsx:264-265`)。相位机在 [`Phase`],render 顶部单点分派。
//! 3. **面板常驻 ≠ 定时器该常驻**。`UsagePanel` 实体首次打开后不销毁
//!    (`main.rs` 的 `usage_panel` 一旦 `Some` 就不再置回 `None`),自动刷新
//!    必须接 [`UsagePanel::set_visible`],否则关掉面板后定时器还在每 5s 扫会话文件。
//!
//! # 模块划分
//!
//! | 落点 | 内容 |
//! |---|---|
//! | [`model`] | 时间窗口 + 展示格式,零 gpui 依赖的纯函数(经 `pub use` 平铺在本模块下) |
//! | [`tween`] | KPI 五格的数字滚动补间 |
//! | `mt_ui::icons::usage_glyphs` | KPI 六枚图标的形状表(几何属渲染层) |
//! | 本文件 | 相位机 + 面板本体(状态、数据流、render 全家) |

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, AppContext as _, Context, Div, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, RenderOnce, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Task, Window, div, point, prelude::FluentBuilder,
    px, relative,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_ui::tooltip::Tooltip;
use mt_ui::icons::usage_glyphs::{
    ICON_BOLT, ICON_CHAT, ICON_PULSE, ICON_REFRESH, ICON_STACK, ICON_WALLET,
};
use mt_ui::icons::vector::{Shape, VectorIcon};
use mt_usage::{
    AgentFilter, DailyStat, ModelPrice, SyncEvent, TopSessionStat, UsageStatsPayload,
    ledger_db_path, spawn_usage_ledger_sync, usage_ledger_query,
};

use crate::i18n::{t, tr};
use crate::menu;
use crate::pricing;
use crate::store::{AppStore, UsagePrefs};
use crate::ui;

mod model;
mod tween;

pub use model::*;
use tween::Tweens;

// ─── 相位机 ──────────────────────────────────────────────────

/// 互斥渲染的六个相位(`UsageStatsModal.tsx:263-289` 的优先级)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 拉价失败**且**没有任何可用表 → 只出提示 + Retry,**绝不渲染 KPI**。
    PricingError,
    /// 拉价中且还没有任何快照。
    PricingLoading,
    /// 账本查询失败。
    Error,
    /// 查询还没回,**或** backfill 正在跑且账本还空。
    Skeleton,
    /// 所选范围内没有 AI 会话。
    Empty,
    Ready,
}

/// 相位分派(纯函数)。优先级:
/// `pricingError > pricing(且无旧数据) > error > 骨架 > 空态 > 主体`。
pub fn phase_of(
    pricing_failed: bool,
    pricing_pending: bool,
    query_error: bool,
    session_count: Option<u64>,
    backfilling: bool,
) -> Phase {
    if pricing_failed {
        return Phase::PricingError;
    }
    if pricing_pending && session_count.is_none() {
        return Phase::PricingLoading;
    }
    if query_error {
        return Phase::Error;
    }
    match session_count {
        None => Phase::Skeleton,
        Some(0) if backfilling => Phase::Skeleton,
        Some(0) => Phase::Empty,
        Some(_) => Phase::Ready,
    }
}

// ─── 面板 ────────────────────────────────────────────────────

/// agent 过滤的四档(mt-usage 的 `AgentFilter` 没实现 PartialEq,这里自己带一份)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    All,
    Claude,
    Codex,
    Grok,
}

impl Scope {
    const ALL: [Scope; 4] = [Self::All, Self::Claude, Self::Codex, Self::Grok];

    /// 稳定标识(元素 id 用)。理由同 [`UsageRange::key`]。
    const fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }

    /// 白名单解析,认不出回落 `all`(与旧版 `loadPref` 同)。
    fn from_key(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|s| s.key() == key)
            .unwrap_or(Self::All)
    }

    fn label(self) -> &'static str {
        match self {
            // 厂商名不翻译(旧版 `SCOPE_NAMES` 同样是裸字面量)
            Self::All => t("usageStats", "scope.all"),
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Grok => "Grok",
        }
    }

    fn filter(self) -> AgentFilter {
        match self {
            Self::All => AgentFilter::All,
            Self::Claude => AgentFilter::Claude,
            Self::Codex => AgentFilter::Codex,
            Self::Grok => AgentFilter::Grok,
        }
    }
}

/// 自动刷新档位(秒);`0 = 关`。默认 5s。
const AUTO_REFRESH_OPTIONS: [u32; 5] = [0, 5, 10, 30, 60];
/// custom 起止输入框的宽度。够 `YYYY-MM-DD` 在等宽字族下也整串露出来 ——
/// 逐层扣法见 [`UsagePanel::render_date_field`]。
const DATE_INPUT_WIDTH: f32 = 150.0;
/// 项目下拉菜单与触发框之间的缝(与日历浮层贴触发钮的手感同一档)。
const PROJECT_MENU_GAP: gpui::Pixels = px(4.0);
/// 项目下拉菜单的高度上限。项目多的机器上这个菜单能长到顶满整屏 ——
/// 它是**挂在弹窗里的下拉**,不是右键菜单,长过弹窗本身就不像一件东西了。
/// 280 ≈ 10 项(行高 25 + 面板 padding 8),再多滚轮翻。
const PROJECT_MENU_MAX_HEIGHT: gpui::Pixels = px(280.0);
/// 模型排行前 N,其余合并成一行 Others。
const TOP_MODELS: usize = 6;
/// `@keyframes usageFadeIn` 的 `translateY(6px)`。
const USAGE_FADE_SHIFT: f32 = 6.0;

// ─── 趋势图版式(逐条抄 `DailyChart.tsx`)────────────────────
//
// recharts 的 `<ComposedChart width height={232} margin={{top:10,right:4,bottom:0,left:4}}>`
// 里,232 是**含轴**的总高:上留白 10 + 绘图区 + X 轴 30(recharts 默认轴高)。
// 两个 Y 轴的宽度是 JSX 上写死的 `width={52}` / `width={44}`。

/// 图表总高(原版 `height={232}`)。
const CHART_HEIGHT: f32 = 232.0;
/// 绘图区上留白(原版 margin.top)。
const CHART_MARGIN_TOP: f32 = 10.0;
/// X 轴条高度(recharts `XAxis` 默认 `height={30}`)。
const CHART_X_AXIS: f32 = 30.0;
/// 绘图区净高。
const CHART_PLOT_HEIGHT: f32 = CHART_HEIGHT - CHART_MARGIN_TOP - CHART_X_AXIS;
/// 左轴(成本)标签列宽,原版 `<YAxis yAxisId="cost" width={52}>`。
const CHART_LEFT_AXIS: f32 = 52.0;
/// 右轴(调用数)标签列宽,原版 `<YAxis yAxisId="calls" width={44}>`。
const CHART_RIGHT_AXIS: f32 = 44.0;
/// 轴刻度字号(原版 `AXIS_TICK = { fontSize: 9 }`)。
const CHART_TICK_FONT: f32 = 9.0;
/// recharts 默认 `tickCount`。
const CHART_TICK_COUNT: usize = 5;
/// X 轴标签的最小间距(原版 `minTickGap={24}`)。
const CHART_X_MIN_GAP: f32 = 24.0;
/// 一个 X 标签的估计宽度(`MM-DD` / `HH:00` 在 9px 下约 28px)。
const CHART_X_LABEL_WIDTH: f32 = 28.0;
/// 绘图区还没量出宽度时的兜底(首帧)。标签疏密可能差一档,下一帧即修正。
const CHART_FALLBACK_WIDTH: f32 = 600.0;
/// 轴刻度标签的行高(9px 字号 + 上下各 1.5px)。定位时要减半个高度才能
/// 让文字**居中压在刻度线上**。
const CHART_TICK_LINE_HEIGHT: f32 = 12.0;

/// Top 会话点开后的正文预览。
///
/// 结构照抄 `SessionPanel` 的同名件 —— 把预览抽成公共件属于**另一条缝**,
/// 本批不动(见批次规格 §1.2 G 的两条路)。
struct Preview {
    title: String,
    loading: bool,
    error: Option<String>,
    messages: Vec<mt_ai::sessions::AiSessionMessage>,
}

pub struct UsagePanel {
    store: Entity<AppStore>,
    app_data_dir: PathBuf,
    scope: Scope,
    range: UsageRange,
    /// 单项目 scope 的**原始路径**;`None` = 整机。
    /// 渲染时按当前项目表过滤(项目可能在面板开着的时候被删)。
    project_scope: Option<String>,
    custom_from: String,
    custom_to: String,
    auto_refresh: u32,
    /// custom 起止的两个受控输入。闸门在 blur / Enter 上过。
    from_input: Entity<InputState>,
    to_input: Entity<InputState>,
    /// 打开着的日期选择浮层(起、止共用一格 —— 同时只开一个)。
    calendar: Option<Entity<crate::date_picker::DatePicker>>,
    /// 上面那个浮层的事件订阅。与浮层同生共死,换浮层时一起换掉。
    _calendar_sub: Option<gpui::Subscription>,
    stats: Option<UsageStatsPayload>,
    error: Option<String>,
    /// backfill(账本首建全量同步)进度;非 backfill 期间为 None。
    progress: Option<(usize, usize)>,
    pricing: HashMap<String, ModelPrice>,
    /// 拉价还在飞。
    pricing_pending: bool,
    /// 拉价失败**且**没有任何可用表 —— 这一档绝不渲染 KPI。
    pricing_error: Option<String>,
    /// 手动刷新正在等同步跑完(刷新按钮据此置灰)。
    syncing: bool,
    /// 抽屉是否展开。实体常驻,定时器必须靠它闸住。
    visible: bool,
    tweens: Tweens,
    /// 主体入场淡入(`.usage-fade-in`)。**相位切进 Ready 才重播** ——
    /// 等价原版「那层 div 重新挂载」;自动刷新期间相位不变,于是不闪。
    fade_in: mt_ui::motion::Transition,
    /// 上一帧的相位。判「刚进 Ready」用,别的地方不要读。
    last_phase: Option<Phase>,
    /// 排行条宽度补间(`.usage-rank-bar` 的 `transition-[width] duration-500
    /// ease-out`)。首次出现直接落目标值(浏览器同款:新元素没有旧值可补),
    /// 之后数据一变就从旧宽度补过去。
    rank_bars: mt_ui::motion::TweenMap,
    /// 趋势图几何缓存。数据没变就复用同一份 [`mt_ui::chart::ChartModel`] ——
    /// 性能红线:不许每帧重建曲线。
    chart_cache: Option<(mt_ui::chart::ChartKey, Rc<mt_ui::chart::ChartModel>)>,
    /// 趋势图绘图区的实测宽度(canvas 量的,跨帧保留)。X 轴标签隔几格摆一个
    /// 得按它算(原版 `minTickGap` 比的是真实像素)。首帧用兜底值,
    /// **量完刻意不 notify** —— 量尺寸再触发重画就是每帧一个死循环。
    chart_width: f32,
    /// 「选择项目」下拉框的左下角(canvas 量的窗口坐标,跨帧保留)。菜单要贴在
    /// 框底而不是鼠标点上,而元素 bounds 只有布局阶段才知道 —— 与
    /// [`Self::chart_width`] 同一套路(量完同样刻意不 notify)。
    /// 首帧还没量到时退回鼠标点。
    project_dropdown_anchor: Option<Point<Pixels>>,
    preview: Option<Preview>,
    /// 查询序号:切参数后旧查询返回时不得覆盖新结果。
    query_seq: u64,
    /// 当前查询。**一份**而不是一列表:赋新值即 drop 掉上一个 Task,
    /// 顺带取消被顶掉的那次查询(点四下时间范围不该留四个任务在跑)。
    _query_task: Option<Task<()>>,
    /// 账本同步的事件泵。同理只留最新一份 —— 手动刷新会再起一次同步。
    _sync_task: Option<Task<()>>,
    /// 拉价 / 手动刷新那条链路。
    _pricing_task: Option<Task<()>>,
    /// Top 会话正文加载。与拉价分开存 —— 共用一格会互相顶掉。
    _preview_task: Option<Task<()>>,
    /// 自动刷新定时器。**改档位即重建**(60s→5s 时用户要立刻看到效果),
    /// 档位为 0 或面板收起时置 `None`。
    _refresh_task: Option<Task<()>>,
    /// 数字滚动的重绘泵。五格共用一个,不给每格挂 `with_animation`
    /// (那是五个独立动画各自持续请求帧)。
    _tween_task: Option<Task<()>>,
    _subs: Vec<gpui::Subscription>,
}

impl UsagePanel {
    pub fn new(
        store: Entity<AppStore>,
        app_data_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let now = chrono::Local::now();
        // 偏好读盘:一律过白名单/正则,认不出回默认(**不写回、不报错**)
        let (scope, range, project_scope, auto_refresh, custom_from, custom_to) = {
            let config = store.read(cx).config();
            let date = |v: &Option<String>, fallback: i64| -> String {
                v.as_deref()
                    .filter(|s| parse_local_date(s).is_some())
                    .map(str::to_string)
                    .unwrap_or_else(|| local_date_str(fallback, now))
            };
            (
                Scope::from_key(config.usage_scope.as_deref().unwrap_or("all")),
                UsageRange::from_key(config.usage_range.as_deref().unwrap_or("days30")),
                config.usage_project.clone(),
                config
                    .usage_auto_refresh
                    .filter(|v| AUTO_REFRESH_OPTIONS.contains(v))
                    .unwrap_or(5),
                date(&config.usage_custom_from, 29),
                date(&config.usage_custom_to, 0),
            )
        };

        let from_input = cx.new(|cx| InputState::new(window, cx).default_value(custom_from.clone()));
        let to_input = cx.new(|cx| InputState::new(window, cx).default_value(custom_to.clone()));
        let mut subs = Vec::new();
        // 闸门在**失焦 / 回车**上过 —— 逐字符校验会在用户打到一半时回弹
        subs.push(
            cx.subscribe_in(&from_input, window, |this: &mut Self, _, event, window, cx| {
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    this.commit_custom_date(true, window, cx);
                }
            }),
        );
        subs.push(
            cx.subscribe_in(&to_input, window, |this: &mut Self, _, event, window, cx| {
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    this.commit_custom_date(false, window, cx);
                }
            }),
        );

        let mut panel = Self {
            store,
            app_data_dir,
            scope,
            range,
            project_scope,
            custom_from,
            custom_to,
            auto_refresh,
            from_input,
            to_input,
            calendar: None,
            _calendar_sub: None,
            stats: None,
            error: None,
            progress: None,
            pricing: HashMap::new(),
            pricing_pending: true,
            pricing_error: None,
            syncing: false,
            visible: true,
            tweens: Tweens::zeroed(),
            // 首帧就是 Skeleton/PricingLoading,建成「已跑完」的,
            // 等真进了 Ready 再 restart
            fade_in: mt_ui::motion::Transition::settled(mt_ui::motion::USAGE_FADE_IN),
            last_phase: None,
            rank_bars: mt_ui::motion::TweenMap::new(mt_ui::motion::RANK_BAR),
            chart_cache: None,
            chart_width: CHART_FALLBACK_WIDTH,
            project_dropdown_anchor: None,
            preview: None,
            query_seq: 0,
            _query_task: None,
            _sync_task: None,
            _pricing_task: None,
            _preview_task: None,
            _refresh_task: None,
            _tween_task: None,
            _subs: subs,
        };
        // 打开面板:拉价 → 出账本现值(在 load_pricing 的回调里 query),
        // 同步在后台跑、有变化由 Synced 事件驱动补查(面板不空屏干等)
        panel.load_pricing(false, cx);
        panel.start_sync(cx);
        panel.restart_auto_refresh(cx);
        panel
    }

    /// 面板开合。**定时器只在可见时跑** —— 实体首次打开后常驻,不闸住的话
    /// 关掉面板后还在每 5s 扫会话文件(范式同 `SessionPanel::set_visible`)。
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            // 重开一次照样走 ensure_pricing:24h TTL 命中即瞬时,过期就重拉。
            // 「内存里有表」**不当作永久绕过 TTL 的理由** —— 常驻多日后
            // 过期价格照常重拉(旧版 `useUsageStats.ts:109-111` 的注释)。
            self.load_pricing(false, cx);
            self.start_sync(cx);
        }
        self.restart_auto_refresh(cx);
    }

    // ── 偏好 ──────────────────────────────────────────────

    fn save_prefs(&mut self, cx: &mut Context<Self>) {
        let prefs = UsagePrefs {
            scope: self.scope.key().to_string(),
            range: self.range.key().to_string(),
            project: self.project_scope.clone(),
            auto_refresh: self.auto_refresh,
            custom_from: self.custom_from.clone(),
            custom_to: self.custom_to.clone(),
        };
        self.store.update(cx, |store, cx| store.set_usage_prefs(prefs, cx));
    }

    /// 项目 scope 的**有效值**:项目已被移除时回落整机而不是空结果。
    /// 每次 render 现算 —— 不能在读盘时一次性判定。
    fn effective_project(&self, cx: &App) -> Option<String> {
        let path = self.project_scope.as_deref()?;
        let norm = norm_project_path(path);
        self.store
            .read(cx)
            .projects()
            .iter()
            .any(|p| norm_project_path(&p.path) == norm)
            .then(|| path.to_string())
    }

    // ── 价格 ──────────────────────────────────────────────

    /// 取一份价格表。`blocking_sync = true` 时顺带把「先同步跑完再查」那条路
    /// 一起走完(手动刷新)。**整条链路丢后台**。
    fn load_pricing(&mut self, blocking_sync: bool, cx: &mut Context<Self>) {
        if self.pricing.is_empty() {
            self.pricing_pending = true;
            self.pricing_error = None;
        }
        if blocking_sync {
            self.syncing = true;
        }
        let dir = self.app_data_dir.clone();
        let now_ms = chrono::Local::now().timestamp_millis();
        let sink = blocking_sync.then(|| self.sync_sink(cx));
        let db_path = ledger_db_path(&self.app_data_dir).ok();

        self._pricing_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let pricing = pricing::ensure_pricing(&dir, now_ms);
                    // 手动刷新:等增量同步真正跑完再查 —— 数字一步到位。
                    // 「先查再同步」每点一次必然先闪一次同步前的旧值。
                    if let (Some(sink), Some(db)) = (sink, db_path) {
                        mt_usage::usage_ledger_sync(&db, true, sink.as_ref());
                    }
                    pricing
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.pricing_pending = false;
                this.syncing = false;
                match result {
                    Ok((table, _src)) => {
                        this.pricing = table;
                        this.pricing_error = None;
                    }
                    // 已有内存表时拉新失败**静默沿用**,不把可用面板打成错误态
                    Err(err) if this.pricing.is_empty() => this.pricing_error = Some(err),
                    Err(_) => {}
                }
                if !this.pricing.is_empty() {
                    this.query(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ── 查询与同步 ────────────────────────────────────────

    /// 查账本(毫秒级)。永远丢后台:打开连接可能等 busy_timeout 最长 5s。
    fn query(&mut self, cx: &mut Context<Self>) {
        // 价格未就绪时不查(拿不到价的 KPI 一律不渲染,查了也没用)
        if self.pricing.is_empty() {
            return;
        }
        self.query_seq += 1;
        let seq = self.query_seq;

        let dir = self.app_data_dir.clone();
        let agents = self.scope.filter();
        let range = self.range;
        let project = self.effective_project(cx);
        let pricing = self.pricing.clone();
        let now = chrono::Local::now();
        let since = range_since_ms(range, &self.custom_from, now);
        let until = range_until_ms(range, &self.custom_from, &self.custom_to, now);
        let tz_offset = tz_offset_minutes(now);
        let tz_name = iana_time_zone::get_timezone().ok();

        self._query_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    usage_ledger_query(
                        &dir,
                        agents,
                        since,
                        until,
                        project,
                        tz_offset,
                        tz_name,
                        range.hourly(),
                        pricing,
                    )
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.query_seq != seq {
                    return;
                }
                match result {
                    Ok(stats) => {
                        this.tweens.retarget(&stats, Instant::now());
                        this.start_tween_pump(cx);
                        this.stats = Some(stats);
                        this.error = None;
                    }
                    Err(err) => this.error = Some(err),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 起一份同步事件泵,返回给同步调用用的 sink。
    fn sync_sink(&mut self, cx: &mut Context<Self>) -> Arc<mt_usage::SyncSink> {
        let (tx, mut rx) = mpsc::unbounded::<SyncEvent>();
        // sink 跑在同步线程上,只管往 channel 里丢
        let sink: Arc<mt_usage::SyncSink> = Arc::new(move |event: SyncEvent| {
            let _ = tx.unbounded_send(event);
        });
        self._sync_task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                let should_requery = this
                    .update(cx, |this: &mut Self, cx| match event {
                        SyncEvent::Progress { processed, total } => {
                            this.progress = Some((processed, total));
                            cx.notify();
                            false
                        }
                        SyncEvent::Synced { added } => {
                            this.progress = None;
                            cx.notify();
                            // added = 0 表示账本无变化,跳过重查避免无谓重渲染
                            added > 0
                        }
                    })
                    .unwrap_or(false);
                if should_requery {
                    let _ = this.update(cx, |this: &mut Self, cx| this.query(cx));
                }
            }
        }));
        sink
    }

    /// 触发一次**非阻塞**增量同步(打开面板 / 自动刷新定时器)。
    /// 数据有变由 `SyncEvent::Synced` 驱动补查,**不直接调 `query`**。
    fn start_sync(&mut self, cx: &mut Context<Self>) {
        let Ok(db_path) = ledger_db_path(&self.app_data_dir) else {
            return;
        };
        let sink = self.sync_sink(cx);
        spawn_usage_ledger_sync(db_path, sink);
    }

    /// 自动刷新定时器。**改档位即重建**而不是等当前 sleep 走完。
    fn restart_auto_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.visible || self.auto_refresh == 0 {
            self._refresh_task = None;
            return;
        }
        let secs = self.auto_refresh as u64;
        self._refresh_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(secs))
                    .await;
                // 实体已释放(或面板收起)即退出循环
                let alive = this
                    .update(cx, |this: &mut Self, cx| {
                        if this.visible {
                            this.start_sync(cx);
                        }
                        this.visible
                    })
                    .unwrap_or(false);
                if !alive {
                    return;
                }
            }
        }));
    }

    /// 数字滚动的单点重绘泵:有任一 tween 没跑完就每 16ms 请一帧。
    fn start_tween_pump(&mut self, cx: &mut Context<Self>) {
        if !self.tweens.running(Instant::now()) {
            self._tween_task = None;
            return;
        }
        self._tween_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let running = this
                    .update(cx, |this: &mut Self, cx| {
                        cx.notify();
                        this.tweens.running(Instant::now())
                    })
                    .unwrap_or(false);
                if !running {
                    return;
                }
            }
        }));
    }

    // ── 参数变更 ──────────────────────────────────────────

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.save_prefs(cx);
        self.query(cx);
    }

    fn set_range(&mut self, range: UsageRange, cx: &mut Context<Self>) {
        if self.range == range {
            return;
        }
        self.range = range;
        self.save_prefs(cx);
        self.query(cx);
    }

    fn set_project_scope(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        if self.project_scope == path {
            return;
        }
        self.project_scope = path;
        self.save_prefs(cx);
        self.query(cx);
    }

    fn set_auto_refresh(&mut self, secs: u32, cx: &mut Context<Self>) {
        if self.auto_refresh == secs {
            return;
        }
        self.auto_refresh = secs;
        self.save_prefs(cx);
        self.restart_auto_refresh(cx);
        cx.notify();
    }

    /// custom 起止输入的提交闸门。不合法就把输入回弹到上一个有效值。
    fn commit_custom_date(&mut self, is_from: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (state, prev) = if is_from {
            (self.from_input.clone(), self.custom_from.clone())
        } else {
            (self.to_input.clone(), self.custom_to.clone())
        };
        let raw = state.read(cx).value().to_string();
        let next = accept_date_input(&raw, &prev);
        if next != raw {
            // 受控输入回弹
            state.update(cx, |s, cx| s.set_value(next.clone(), window, cx));
        }
        if next == prev {
            return;
        }
        if is_from {
            self.custom_from = next;
        } else {
            self.custom_to = next;
        }
        self.save_prefs(cx);
        self.query(cx);
    }

    /// 日历选中一天:直接落值(不必再过 [`accept_date_input`] 那道闸 —— 日历给出的
    /// 一定是合法日期),受控输入同步回写。
    fn set_custom_date(
        &mut self,
        is_from: bool,
        date: chrono::NaiveDate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = date.format("%Y-%m-%d").to_string();
        let prev = if is_from {
            &self.custom_from
        } else {
            &self.custom_to
        };
        if *prev == next {
            return;
        }
        let state = if is_from {
            self.from_input.clone()
        } else {
            self.to_input.clone()
        };
        if is_from {
            self.custom_from = next.clone();
        } else {
            self.custom_to = next.clone();
        }
        state.update(cx, |s, cx| s.set_value(next, window, cx));
        self.save_prefs(cx);
        self.query(cx);
    }

    /// 弹日期选择浮层。可选范围与查询窗口的钳位同源:下界 [`custom_floor`](近一年),
    /// 上界今天 —— 免得日历能选出一个查询侧当场就要钳回去的日子。
    fn open_calendar(
        &mut self,
        is_from: bool,
        anchor: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let today = chrono::Local::now().date_naive();
        let current = parse_local_date(if is_from {
            &self.custom_from
        } else {
            &self.custom_to
        });
        // 先清后建:换浮层时旧实体必须先 drop 掉,否则 overlay 栈会错乱
        // (理由见 `date_picker::DatePicker::new` 的注释)
        self.calendar = None;
        self._calendar_sub = None;
        let picker = cx.new(|cx| {
            crate::date_picker::DatePicker::new(anchor, current, today, window, cx)
                .range(Some(custom_floor(today)), Some(today))
        });
        self._calendar_sub = Some(cx.subscribe_in(
            &picker,
            window,
            move |this: &mut Self, _, event, window, cx| {
                if let crate::date_picker::DatePickerEvent::Picked(date) = event {
                    this.set_custom_date(is_from, *date, window, cx);
                }
                // 只丢实体、不动 `_calendar_sub` —— 那是**正在跑的这条订阅**自己,
                // 在回调里把自己 drop 掉没必要;实体一没,它下次也不会再触发,
                // 真正的清理在下一次 `open_calendar` 开头
                this.calendar = None;
                cx.notify();
            },
        ));
        self.calendar = Some(picker);
        cx.notify();
    }

    /// 手动刷新:重拉价 → 等增量同步跑完 → 再查。
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.syncing {
            return;
        }
        self.load_pricing(true, cx);
    }

    // ── Top 会话预览 ──────────────────────────────────────

    fn open_preview(&mut self, session: &TopSessionStat, cx: &mut Context<Self>) {
        // `UsageTopSessionStat → AiSession` 的字段对应照抄
        // `UsageStatsModal.tsx:385-393`:agent 只分 codex / grok,其余按 claude
        let session_type = match session.agent.as_str() {
            "codex" => "codex",
            "grok" => "grok",
            _ => "claude",
        }
        .to_string();
        let title = if session.title.is_empty() {
            t("usageStats", "untitled").to_string()
        } else {
            session.title.clone()
        };
        self.preview = Some(Preview {
            title,
            loading: true,
            error: None,
            messages: Vec::new(),
        });
        let session_id = session.session_id.clone();
        let project_path = session.project_path.clone();
        self._preview_task = Some(cx.spawn(async move |this, cx| {
            // 正文可能几 MB,雷打不动丢后台。账本只收本机来源 → wsl_distro 恒 None
            let result = cx
                .background_executor()
                .spawn(async move {
                    mt_ai::sessions::get_ai_session_content(
                        session_type,
                        session_id,
                        project_path,
                        None,
                    )
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                let Some(preview) = this.preview.as_mut() else {
                    return;
                };
                preview.loading = false;
                match result {
                    Ok(messages) => preview.messages = messages,
                    Err(err) => preview.error = Some(err),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

// ─── 渲染小件 ────────────────────────────────────────────────

/// 区块卡片壳(`UsageStatsModal.tsx:99-109`):
/// `border-[--border-subtle] rounded-md bg-[--bg-elevated]/40 px-4 py-3.5` + 竖条标题。
fn section(title: impl Into<String>, body: impl IntoElement) -> Div {
    div()
        .border_1()
        .border_color(ui::border_subtle())
        .rounded(px(6.0))
        .bg(ui::with_alpha(ui::bg_elevated(), 0.4))
        .px(px(16.0))
        .py(px(14.0))
        .child(ui::section_title(title.into()))
        .child(body)
}

/// 一格 KPI(`KpiCards.tsx:44-64`):图标片 + 标签 + 大数字。
fn kpi(
    id: &'static str,
    icon: &'static [Shape],
    label: &str,
    value: String,
    value_color: gpui::Hsla,
) -> Div {
    let _ = id;
    div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(16.0))
        .py(px(14.0))
        .bg(ui::bg_elevated())
        .border_1()
        .border_color(ui::border_subtle())
        .rounded(px(6.0))
        .child(
            div()
                .flex_none()
                .w(px(32.0))
                .h(px(32.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                // `color-mix(in srgb, currentColor 12%, transparent)`
                .bg(ui::with_alpha(ui::color_info(), 0.12))
                .child(VectorIcon::new(icon, px(16.0)).ink(ui::color_info())),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    div()
                        .text_size(ui::font_px(10.0))
                        .text_color(ui::text_muted())
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .text_size(ui::font_px(20.0))
                        .truncate()
                        .text_color(value_color)
                        .child(value),
                ),
        )
}

/// 一条排行(`RankBarList.tsx:24-54`):label | 渐变横条 | 主值 | 次值。
///
/// `on_click` 为 `Some` 时整行可点(项目排行点击切入单项目 scope);
/// 未登记的项目行仅展示 —— 无 hover 态、无指针。
fn rank_row(
    id: impl Into<SharedString>,
    label: String,
    ratio: f32,
    primary: String,
    secondary: Option<String>,
    clickable: bool,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(12.0))
        .py(px(7.0))
        // 可点行的 -mx/px 出血:滚动容器上配了同源的内边距吸收它
        .px(px(6.0))
        .mx(px(-6.0))
        .rounded(px(4.0))
        .when(clickable, |el| {
            el.cursor_pointer().hover(|el| el.bg(ui::border_subtle()))
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_secondary())
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .w(px(56.0))
                .h(px(6.0))
                .rounded(px(3.0))
                .bg(ui::border_subtle())
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .rounded(px(3.0))
                        // `max(min(ratio,1)*100, 2)%` —— 最小 2% 保底,全 0 时也看得见槽位
                        .w(relative((ratio.clamp(0.0, 1.0)).max(0.02)))
                        .bg(gpui::linear_gradient(
                            90.0,
                            gpui::linear_color_stop(ui::color_info(), 0.0),
                            gpui::linear_color_stop(ui::color_ai(), 1.0),
                        )),
                ),
        )
        .child(
            div()
                .flex_none()
                .min_w(px(56.0))
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_primary())
                .child(primary),
        )
        .when_some(secondary, |el, s| {
            el.child(
                div()
                    .flex_none()
                    .min_w(px(40.0))
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(s),
            )
        })
}

// ─── 趋势图的三个文本层 ──────────────────────────────────────

/// 左轴刻度文案(`DailyChart.tsx` 的 `axisCost`:
/// `v >= 1000 ? $X.XK : $X.XX`)。
fn axis_cost(v: f64) -> String {
    if v >= 1000.0 {
        format!("${:.1}K", v / 1000.0)
    } else {
        format!("${v:.2}")
    }
}

/// X 轴刻度文案(`tickDate`:小时桶原样,日期桶切掉年份留 `MM-DD`)。
fn axis_date(date: &str) -> String {
    if date.contains(':') {
        date.to_string()
    } else {
        date.chars().skip(5).collect()
    }
}

/// 一侧的 Y 轴刻度标签列。
///
/// 刻度是等距的,所以第 i 条线落在绘图区高度的 `1 - i/(n-1)` 处;标签绝对定位、
/// **纵向压在线上**(减半个行高)。原版是 SVG 的 `<text dominant-baseline>`,
/// 效果一样。只有一条刻度(全 0)时贴底。
fn axis_labels(ticks: &[f64], left: bool, fmt: impl Fn(f64) -> String) -> Div {
    let mut column = div()
        .relative()
        .flex_none()
        .w(px(if left {
            CHART_LEFT_AXIS
        } else {
            CHART_RIGHT_AXIS
        }))
        .h(px(CHART_PLOT_HEIGHT))
        .text_size(ui::font_px(CHART_TICK_FONT))
        .text_color(ui::text_muted());
    let last = ticks.len().saturating_sub(1);
    for (i, v) in ticks.iter().enumerate() {
        let ratio = if last == 0 { 0.0 } else { i as f32 / last as f32 };
        let y = CHART_PLOT_HEIGHT * (1.0 - ratio) - CHART_TICK_LINE_HEIGHT / 2.0;
        column = column.child(
            div()
                .absolute()
                .top(px(y))
                .left_0()
                .right_0()
                .h(px(CHART_TICK_LINE_HEIGHT))
                .when(left, |el| el.text_right().pr(px(4.0)))
                .when(!left, |el| el.pl(px(4.0)))
                .child(fmt(*v)),
        );
    }
    column
}

/// X 轴标签条。格子与绘图区的 band 一一对应(所以标签正对柱心),
/// 两侧留出 Y 轴列的宽度让它与绘图区对齐。
///
/// `step` 由 [`mt_ui::chart::label_step`] 按实测宽度算 —— 等价原版
/// `minTickGap={24}`(recharts 是逐个量文本宽度后跳过挤在一起的那些)。
fn x_axis_labels(buckets: &[DailyStat], plot_width: f32) -> Div {
    let step = mt_ui::chart::label_step(
        buckets.len(),
        plot_width,
        CHART_X_LABEL_WIDTH,
        CHART_X_MIN_GAP,
    );
    let mut row = div().flex().flex_1().min_w(px(0.0));
    for (i, d) in buckets.iter().enumerate() {
        row = row.child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_center()
                .child(if i % step == 0 {
                    axis_date(&d.date)
                } else {
                    String::new()
                }),
        );
    }
    div()
        .flex()
        .h(px(CHART_X_AXIS))
        .pt(px(4.0))
        .text_size(ui::font_px(CHART_TICK_FONT))
        .text_color(ui::text_muted())
        .child(div().flex_none().w(px(CHART_LEFT_AXIS)))
        .child(row)
        .child(div().flex_none().w(px(CHART_RIGHT_AXIS)))
}

/// 骨架块:`rounded-md` + `--border-subtle` + 2s 脉冲(`opacity 1 → .5 → 1`)。
///
/// 脉冲相位来自 `mt_ui::motion::pulse_phase` 的低频泵 —— **不用**
/// `with_animation(..repeat())`,那条路每帧请求重绘,骨架屏一挂就是整窗满帧。
///
/// ⚠️ 脉冲过减弱动效的闸:原版这是 Tailwind 的 `.animate-pulse`,reduce 段的
/// 通配规则把它**停在第一帧**(它不在豁免名单里 —— 那段注释还专门点了
/// `animate-pulse` 的名)。停下来就是一块静止的浅色占位,信息量不减。
fn skeleton_block(h: f32) -> AnyElement {
    SkeletonBlock { h }.into_any_element()
}

#[derive(IntoElement)]
struct SkeletonBlock {
    h: f32,
}

impl RenderOnce for SkeletonBlock {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let block = div()
            .h(px(self.h))
            .flex_1()
            .rounded(px(6.0))
            .bg(ui::border_subtle());
        if !mt_ui::motion::blinks() {
            return block;
        }
        // 0..1 进度折成 1 → 0.5 → 1 的呼吸(原版 `bounce(ease_in_out)` 的近似,
        // 折返曲线复用 title_bar::blink_phase 的 smoothstep 三角波)
        let phase =
            crate::title_bar::blink_phase(mt_ui::motion::pulse_phase(SKELETON_PERIOD, window, cx));
        block.opacity(1.0 - phase * 0.5)
    }
}

/// 骨架脉冲周期(原版 Tailwind `animate-pulse` 是 2s)。
const SKELETON_PERIOD: Duration = Duration::from_secs(2);

/// 状态提示件(`UsageStatsModal.tsx:503-536`):可选 spinner + 主文案 +
/// detail(截断) + 可选动作按钮。
fn state_hint(
    text: &str,
    detail: Option<String>,
    spinning: bool,
    action: Option<(&str, Box<dyn Fn(&mut Window, &mut App) + 'static>)>,
) -> Div {
    div()
        .py(px(80.0))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(12.0))
        .when(spinning, |el| el.child(ui::spinner(px(20.0), ui::accent())))
        .child(
            div()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_secondary())
                .child(text.to_string()),
        )
        .when_some(detail, |el, d| {
            el.child(
                div()
                    .id("usage-state-detail")
                    .max_w(px(480.0))
                    .truncate()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    // 截断之后 hover 出全文(原版靠 `title=`)
                    .tooltip({
                        let d = d.clone();
                        move |window, cx| Tooltip::new(d.clone()).build(window, cx)
                    })
                    .child(d),
            )
        })
        .when_some(action, |el, (label, on_click)| {
            el.child(
                ui::primary_button("usage-retry", label.to_string())
                    .on_click(move |_, window, cx| on_click(window, cx)),
            )
        })
}

impl Render for UsagePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Top 会话点开 → 面板内正文预览(带「‹ 返回」)
        if self.preview.is_some() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(self.render_preview(cx));
        }

        let header = self.render_header(cx);
        let mut body = div()
            .id("usage-body")
            .flex_1()
            .overflow_y_scroll()
            .px(px(16.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .gap(px(14.0));

        // 副标题 + backfill 进度(原版 `:474-487`)
        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_secondary())
                        .child(format!("{} · {}", self.scope.label(), self.range.label())),
                )
                .when_some(self.progress, |el, (processed, total)| {
                    el.child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(format!(
                                "{} {}",
                                t("usageStats", "backfilling"),
                                tr!("usageStats", "progress", processed = processed, total = total)
                            )),
                    )
                }),
        );

        let phase = phase_of(
            self.pricing_error.is_some(),
            self.pricing_pending,
            self.error.is_some(),
            self.stats.as_ref().map(|s| s.session_count),
            self.progress.is_some_and(|(_, total)| total > 0),
        );

        // `.usage-fade-in` 是挂在**主体那层 div** 上的一次性动画,只在它被挂载时
        // 播 —— 相位从别的档切进 Ready 就是「挂载」。停在 Ready 上的自动刷新
        // (每 5s 一次)不该让整个面板反复淡入。
        if phase == Phase::Ready && self.last_phase != Some(Phase::Ready) {
            self.fade_in.restart();
        }
        self.last_phase = Some(phase);

        let entity = cx.entity();
        body = match phase {
            Phase::PricingError => body.child(state_hint(
                t("usageStats", "pricingError"),
                Some(format!(
                    "{} · {}",
                    self.pricing_error.clone().unwrap_or_default(),
                    // 离线/内网环境的补救办法
                    t("usageStats", "pricingLocalHint")
                )),
                false,
                Some((
                    t("usageStats", "retry"),
                    Box::new({
                        let entity = entity.clone();
                        move |_window, cx: &mut App| {
                            entity.update(cx, |this, cx| this.refresh(cx));
                        }
                    }),
                )),
            )),
            Phase::PricingLoading => {
                body.child(state_hint(t("usageStats", "pricingLoading"), None, true, None))
            }
            Phase::Error => body.child(state_hint(
                t("usageStats", "scanError"),
                self.error.clone(),
                false,
                Some((
                    t("usageStats", "retry"),
                    Box::new({
                        let entity = entity.clone();
                        move |_window, cx: &mut App| {
                            entity.update(cx, |this, cx| this.refresh(cx));
                        }
                    }),
                )),
            )),
            // ⚠️ 骨架屏是短命状态:`with_animation` 持续请求帧,`stats` 到位后
            // 必须立刻**从树上消失**,不能靠 opacity 藏起来
            Phase::Skeleton => body.child(render_skeleton()),
            Phase::Empty => body.child(state_hint(t("usageStats", "empty"), None, false, None)),
            Phase::Ready => self.render_main(body, window, cx),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .child(header)
            .child(body)
            // 日期浮层。挂在这一层是安全的:它自己是 `deferred`,而 gpui 的
            // `DeferredDraw` 不带祖先的 ContentMask —— 抽屉裁不到它
            .when_some(self.calendar.clone(), |el, picker| el.child(picker))
    }
}

/// 与真实布局同形的骨架占位(`BodySkeleton`,`:120-137`)——
/// 目的是避免「转圈 → 完整布局」的跳变。
fn render_skeleton() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(16.0))
        // KPI 行:原版 5 格但骨架画 4 格,照抄
        .child(
            div()
                .flex()
                .gap(px(12.0))
                .child(skeleton_block(66.0))
                .child(skeleton_block(66.0))
                .child(skeleton_block(66.0))
                .child(skeleton_block(66.0)),
        )
        .child(
            div()
                .flex()
                .child(div().w(px(320.0)).child(skeleton_block(16.0))),
        )
        .child(skeleton_block(280.0))
        .child(
            div()
                .flex()
                .gap(px(16.0))
                .child(skeleton_block(200.0))
                .child(skeleton_block(200.0))
                .child(skeleton_block(200.0)),
        )
}

impl UsagePanel {
    fn render_header(&mut self, cx: &mut Context<Self>) -> Div {
        let scope = self.scope;
        let range = self.range;
        let effective = self.effective_project(cx);
        let project_label = effective
            .as_deref()
            .and_then(|path| {
                let norm = norm_project_path(path);
                self.store
                    .read(cx)
                    .projects()
                    .iter()
                    .find(|p| norm_project_path(&p.path) == norm)
                    .map(|p| p.name.clone())
            })
            .unwrap_or_else(|| t("usageStats", "scope.allProjects").to_string());

        let mut scope_bar = segmented();
        for s in Scope::ALL {
            scope_bar = scope_bar.child(
                segment(
                    SharedString::from(format!("usage-scope-{}", s.key())),
                    s.label(),
                    s == scope,
                )
                .on_click(cx.listener(move |this: &mut Self, _, _window, cx| {
                    this.set_scope(s, cx);
                })),
            );
        }
        let mut range_bar = segmented();
        for r in UsageRange::ALL {
            range_bar = range_bar.child(
                segment(
                    SharedString::from(format!("usage-range-{}", r.key())),
                    r.label(),
                    r == range,
                )
                .on_click(cx.listener(move |this: &mut Self, _, _window, cx| {
                    this.set_range(r, cx);
                })),
            );
        }

        let syncing = self.syncing;
        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .flex_wrap()
            .px(px(16.0))
            .py(px(10.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(scope_bar)
            .child(self.render_project_dropdown(project_label, cx))
            .child(range_bar)
            // custom 起止:仅在 `range == Custom` 时渲染(对齐 `:424`)
            .when(range == UsageRange::Custom, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(4.0))
                        .child(self.render_date_field(true, cx))
                        // 日期分隔符不进字典(原版 `:434` 就是裸字面量)
                        .child(
                            div()
                                .text_size(ui::font_px(11.0))
                                .text_color(ui::text_muted())
                                .child("–"),
                        )
                        .child(self.render_date_field(false, cx)),
                )
            })
            .child(div().flex_1())
            .child(self.render_auto_refresh_dropdown(cx))
            .child(
                div()
                    .id("usage-refresh")
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(28.0))
                    .h(px(28.0))
                    .rounded(px(4.0))
                    .text_color(ui::text_muted())
                    // syncing 期间置灰 + 不可点(连点会各自等一轮同步)
                    .when(syncing, |el| el.opacity(0.4))
                    .when(!syncing, |el| {
                        el.cursor_pointer()
                            .hover(|el| el.bg(ui::border_subtle()))
                            .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                                this.refresh(cx)
                            }))
                    })
                    // 纯图标键,提示晚弹等于认不出来 → 免掉额外停留(见
                    // `mt_ui::tooltip` 的二段延迟说明),回落到 gpui 的 500ms
                    .tooltip(|window, cx| {
                        Tooltip::new(t("usageStats", "refresh"))
                            .instant()
                            .build(window, cx)
                    })
                    .child(VectorIcon::new(ICON_REFRESH, px(14.0)).ink(ui::text_muted())),
            )
    }

    /// 一个 custom 日期输入 + 它的日历触发钮。
    ///
    /// 宽度 [`DATE_INPUT_WIDTH`] 而不是原来的 112:`gpui_component::Input` 逐层扣掉
    /// 左右各 12 的 padding(`input_px(Medium)`)、1px 边框、再给光标留 10
    /// (`input/element.rs` 的 `RIGHT_MARGIN`),112 只剩 76px 可视;而它的文字是
    /// **rem 定死的 14px**(`input_text_size` → `text_sm`,**不跟 `ui::font_px`
    /// 缩放**),等宽字族下 `2026-08-20` 要 84px —— 年份直接被截掉,点到行尾时
    /// 还会因为「光标要露出来」整行左移。
    ///
    /// 另外补 `flex_none`:原来那两层是默认可收缩的,挤的时候还会被压得更窄。
    fn render_date_field(&mut self, is_from: bool, cx: &mut Context<Self>) -> Div {
        let state = if is_from {
            &self.from_input
        } else {
            &self.to_input
        };
        let entity = cx.entity();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.0))
            .child(
                div()
                    .flex_none()
                    .w(px(DATE_INPUT_WIDTH))
                    .child(Input::new(state)),
            )
            .child(crate::date_picker::trigger_button(
                if is_from {
                    "usage-date-from-pick"
                } else {
                    "usage-date-to-pick"
                },
                t("usageStats", "pickDate"),
                move |anchor, window, cx: &mut App| {
                    entity.update(cx, |this, cx| {
                        this.open_calendar(is_from, anchor, window, cx)
                    });
                },
            ))
    }

    /// 单项目下拉。**自绘**(走 `menu.rs`)—— `gpui_component::select` 的箭头
    /// 走 `IconName::ChevronDown`,0.5.1 不带 svg 资产,渲染出来是空白。
    ///
    /// 选项顺序**照项目表原样**(与项目列表同序,用户找得到);选项值是
    /// `ProjectConfig.path`(不是 id)—— `usage_ledger_query` 的 `project_path`
    /// 形参走 cwd 匹配。元素 id 用 `project.id`(不随语言/路径变)。
    fn render_project_dropdown(&mut self, label: String, cx: &mut Context<Self>) -> Stateful<Div> {
        let entity = cx.entity();
        let projects: Vec<(String, String, String)> = self
            .store
            .read(cx)
            .projects()
            .iter()
            .map(|p| (p.id.clone(), p.name.clone(), p.path.clone()))
            .collect();
        // 当前选中项在**打开菜单这一刻**定下(与 projects 快照同一口径)。
        // 项目一多就得靠这个勾才认得出选的是哪个 —— 菜单基件没有勾选态,
        // 惯例是「`✓ ` / 全角空格」前缀(见 `menu.rs` 模块注释)
        let selected = self.effective_project(cx).map(|p| norm_project_path(&p));
        // 菜单要贴在框底:量下这一帧的 bounds 供**下一次点开**用(与趋势图量
        // 绘图区宽度同一套路,同样刻意不 notify)
        let measure_entity = cx.entity();
        let measure = gpui::canvas(
            move |bounds: gpui::Bounds<Pixels>, _window, cx| {
                measure_entity.update(cx, |this: &mut Self, _cx| {
                    this.project_dropdown_anchor =
                        Some(point(bounds.origin.x, bounds.bottom() + PROJECT_MENU_GAP));
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let anchor = self.project_dropdown_anchor;
        dropdown("usage-project-scope", label, px(160.0))
            .relative()
            .child(measure)
            .on_mouse_down(
                MouseButton::Left,
                move |event: &MouseDownEvent, window, cx| {
                    let mut entries = vec![menu::item(
                        format!(
                            "{}{}",
                            check_mark(selected.is_none()),
                            t("usageStats", "scope.allProjects")
                        ),
                        {
                            let entity = entity.clone();
                            move |_window, cx: &mut App| {
                                entity.update(cx, |this, cx| this.set_project_scope(None, cx));
                            }
                        },
                    )];
                    for (_id, name, path) in &projects {
                        let entity = entity.clone();
                        let on = selected.as_deref() == Some(norm_project_path(path).as_str());
                        let label = format!("{}{name}", check_mark(on));
                        let path = path.clone();
                        entries.push(menu::item(label, move |_window, cx: &mut App| {
                            let path = path.clone();
                            entity.update(cx, |this, cx| this.set_project_scope(Some(path), cx));
                        }));
                    }
                    // 首帧还没量到 bounds 时退回鼠标点(旧行为)
                    menu::show_with(
                        anchor.unwrap_or(event.position),
                        entries,
                        menu::MenuOptions::max_height(PROJECT_MENU_MAX_HEIGHT),
                        window,
                        cx,
                    );
                },
            )
    }

    /// 自动刷新档位下拉。`0` 显示 `autoRefreshOff`,其余是裸模板串 `"{n}s"`
    /// (**不进字典**,原版 `:455` 就是这么写的)。
    fn render_auto_refresh_dropdown(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        let current = self.auto_refresh;
        let label = auto_refresh_label(current);
        let entity = cx.entity();
        dropdown("usage-auto-refresh", label, px(96.0))
            .tooltip(|window, cx| Tooltip::new(t("usageStats", "autoRefresh")).build(window, cx))
            .on_mouse_down(
                MouseButton::Left,
                move |event: &MouseDownEvent, window, cx| {
                    let entries = AUTO_REFRESH_OPTIONS
                        .into_iter()
                        .map(|secs| {
                            let entity = entity.clone();
                            let label =
                                format!("{}{}", check_mark(secs == current), auto_refresh_label(secs));
                            menu::item(label, move |_window, cx: &mut App| {
                                entity.update(cx, |this, cx| this.set_auto_refresh(secs, cx));
                            })
                        })
                        .collect();
                    menu::show(event.position, entries, window, cx);
                },
            )
    }

    /// 主体:KPI 五格 + Token 副行 + 趋势图 + 三卡同行 + Top 会话 + 三段计数排行。
    /// 主体(`UsageStatsModal.tsx:300` 那层 `space-y-4 usage-fade-in`)。
    ///
    /// 整块内容装在**一个** `main` 容器里而不是直接挂到 `body` 上,就是为了给
    /// `.usage-fade-in` 一个落点:淡入是整块一起淡,不是逐个区块各淡各的。
    /// `gap` 与 `body` 同值,所以拆出这一层之后间距与之前一模一样。
    fn render_main(
        &mut self,
        body: Stateful<Div>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let Some(stats) = self.stats.clone() else {
            return body;
        };
        let mut main = div().flex().flex_col().gap(px(14.0));
        let now = Instant::now();
        let hit = cache_hit_rate(
            stats.input_tokens,
            stats.cache_read_tokens,
            stats.cache_write_tokens,
        );

        // --- KPI 五格(数字滚动) ---
        main = main.child(
            div()
                .flex()
                .gap(px(12.0))
                .child(kpi(
                    "kpi-cost",
                    ICON_WALLET,
                    t("usageStats", "kpi.cost"),
                    format_cost(self.tweens.cost.value(now)),
                    ui::accent(),
                ))
                .child(kpi(
                    "kpi-tokens",
                    ICON_STACK,
                    t("usageStats", "kpi.tokens"),
                    format_tokens(self.tweens.tokens.value(now).round().max(0.0) as u64),
                    ui::text_primary(),
                ))
                .child(kpi(
                    "kpi-calls",
                    ICON_PULSE,
                    t("usageStats", "kpi.calls"),
                    format_count(self.tweens.calls.value(now).round().max(0.0) as u64),
                    ui::text_primary(),
                ))
                .child(kpi(
                    "kpi-sessions",
                    ICON_CHAT,
                    t("usageStats", "kpi.sessions"),
                    format_count(self.tweens.sessions.value(now).round().max(0.0) as u64),
                    ui::text_primary(),
                ))
                .child(kpi(
                    "kpi-cache-hit",
                    ICON_BOLT,
                    t("usageStats", "kpi.cacheHit"),
                    // `cacheHit` 为 null 时显示 `—`,**不补间**
                    match hit {
                        None => "—".to_string(),
                        Some(_) => format!("{:.1}%", self.tweens.cache_hit.value(now)),
                    },
                    ui::text_primary(),
                )),
        );

        // --- Token 副行:in | out | cached | written ---
        let mut token_row = div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(4.0))
            .text_size(ui::font_px(13.0))
            .text_color(ui::text_muted());
        for (i, (v, key)) in [
            (stats.input_tokens, "tokens.in"),
            (stats.output_tokens, "tokens.out"),
            (stats.cache_read_tokens, "tokens.cached"),
            (stats.cache_write_tokens, "tokens.written"),
        ]
        .into_iter()
        .enumerate()
        {
            if i > 0 {
                token_row = token_row.child(
                    div()
                        .text_color(ui::border_strong())
                        .child("|"),
                );
            }
            token_row = token_row.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_color(ui::text_primary())
                            .child(format_tokens(v)),
                    )
                    .child(t("usageStats", key)),
            );
        }
        main = main.child(token_row);

        // --- 趋势图 ---
        main = main.child(section(
            t("usageStats", "dailyActivity"),
            self.render_chart(&stats, cx),
        ));

        // --- 三卡同行:项目 | 模型 | 供应商 ---
        let entity = cx.entity();
        let registered: Vec<(String, String)> = self
            .store
            .read(cx)
            .projects()
            .iter()
            .map(|p| (norm_project_path(&p.path), p.path.clone()))
            .collect();

        let mut project_rows = div().flex().flex_col();
        if stats.by_project.is_empty() {
            project_rows = project_rows.child(empty_hint());
        } else {
            let ratios = bar_ratios_or(
                &stats.by_project.iter().map(|p| p.cost).collect::<Vec<_>>(),
                &stats
                    .by_project
                    .iter()
                    .map(|p| p.tokens as f64)
                    .collect::<Vec<_>>(),
            );
            for (i, p) in stats.by_project.iter().enumerate() {
                // 只有匹配到**已登记项目**的行才可点(跑过 AI 但没加进 mini-term
                // 的目录仅展示、无 hover 态、无指针)
                let norm = norm_project_path(&p.path);
                let target = registered
                    .iter()
                    .find(|(n, _)| *n == norm)
                    .map(|(_, path)| path.clone());
                let entity = entity.clone();
                let target_for_click = target.clone();
                let id = format!("proj-{}", p.path);
                let ratio = self
                    .rank_bars
                    .value(&id, ratios.get(i).copied().unwrap_or(0.0));
                project_rows = project_rows.child(
                    rank_row(
                        id,
                        p.name.clone(),
                        ratio,
                        format_cost(p.cost),
                        Some(p.sessions.to_string()),
                        target.is_some(),
                    )
                    .when_some(target_for_click, |el, path| {
                        el.on_click(move |_, _window, cx| {
                            let path = path.clone();
                            entity.update(cx, |this, cx| this.set_project_scope(Some(path), cx));
                        })
                    }),
                );
            }
        }

        let mut model_rows = div().flex().flex_col();
        if stats.by_model.is_empty() {
            model_rows = model_rows.child(empty_hint());
        } else {
            // 前 6 + Others 合并;**Others 也参与 max 归一**;全 $0 时按 tokens 排比例
            let use_cost = stats.total_cost > 0.0;
            let metric = |cost: f64, tokens: u64| if use_cost { cost } else { tokens as f64 };
            let top: Vec<_> = stats.by_model.iter().take(TOP_MODELS).collect();
            let rest: Vec<_> = stats.by_model.iter().skip(TOP_MODELS).collect();
            let others_cost: f64 = rest.iter().map(|m| m.cost).sum();
            let others_tokens: u64 = rest.iter().map(|m| m.tokens).sum();
            let mut values: Vec<f64> = top.iter().map(|m| metric(m.cost, m.tokens)).collect();
            if !rest.is_empty() {
                values.push(metric(others_cost, others_tokens));
            }
            let ratios = bar_ratios(&values);
            for (i, m) in top.iter().enumerate() {
                let name = if m.model.is_empty() {
                    t("usageStats", "unknownModel").to_string()
                } else {
                    model_short_name(&m.model)
                };
                let id = format!("model-{}", m.model);
                let ratio = self
                    .rank_bars
                    .value(&id, ratios.get(i).copied().unwrap_or(0.0));
                model_rows = model_rows.child(rank_row(
                    id,
                    name,
                    ratio,
                    format_cost(m.cost),
                    Some(format_tokens(m.tokens)),
                    false,
                ));
            }
            if !rest.is_empty() {
                let ratio = self
                    .rank_bars
                    .value("model-others", ratios.last().copied().unwrap_or(0.0));
                model_rows = model_rows.child(rank_row(
                    "model-others",
                    tr!("usageStats", "othersModels", count = rest.len()),
                    ratio,
                    format_cost(others_cost),
                    Some(format_tokens(others_tokens)),
                    false,
                ));
            }
        }

        let mut provider_rows = div().flex().flex_col();
        if stats.by_provider.is_empty() {
            provider_rows = provider_rows.child(empty_hint());
        } else {
            let ratios = bar_ratios_or(
                &stats.by_provider.iter().map(|p| p.cost).collect::<Vec<_>>(),
                &stats
                    .by_provider
                    .iter()
                    .map(|p| p.tokens as f64)
                    .collect::<Vec<_>>(),
            );
            for (i, p) in stats.by_provider.iter().enumerate() {
                let name = if p.provider.is_empty() {
                    t("usageStats", "unknownProvider").to_string()
                } else {
                    p.provider.clone()
                };
                let id = format!("provider-{}", p.provider);
                let ratio = self
                    .rank_bars
                    .value(&id, ratios.get(i).copied().unwrap_or(0.0));
                provider_rows = provider_rows.child(rank_row(
                    id,
                    name,
                    ratio,
                    format_cost(p.cost),
                    Some(format_tokens(p.tokens)),
                    false,
                ));
            }
        }

        main = main.child(
            div()
                .flex()
                .items_start()
                .gap(px(16.0))
                .child(
                    div().flex_1().min_w(px(0.0)).child(section(
                        t("usageStats", "byProject"),
                        // 项目数可能多,固定高度内滚动;`px` 与行的 `-mx` 同源
                        div()
                            .id("usage-by-project")
                            .max_h(px(216.0))
                            .overflow_y_scroll()
                            .px(px(6.0))
                            .mx(px(-6.0))
                            .child(project_rows),
                    )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(section(t("usageStats", "byModel"), model_rows)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(section(t("usageStats", "byProvider"), provider_rows)),
                ),
        );

        // --- Top 会话 ---
        main = main.child(section(
            t("usageStats", "topSessions"),
            self.render_top_sessions(&stats, cx),
        ));

        // --- 工具 / Shell / MCP 计数排行 ---
        //
        // 这三块是 GPUI 侧新加的(`byTool`/`byShell`/`byMcp` 在 `types.ts` 里有
        // 类型,旧版面板从没渲染过),`usageStats.{byTool,byShell}` 由 M 批补进
        // TS 源头;MCP 是专有名词,与厂商名一样不进字典。**重构渲染时别顺手删掉**。
        for (id, title, items) in [
            ("tool", t("usageStats", "byTool"), &stats.by_tool),
            ("shell", t("usageStats", "byShell"), &stats.by_shell),
            ("mcp", "MCP", &stats.by_mcp),
        ] {
            if items.is_empty() {
                continue;
            }
            let ratios = bar_ratios(&items.iter().map(|c| c.count as f64).collect::<Vec<_>>());
            let mut rows = div().flex().flex_col();
            for (i, c) in items.iter().enumerate() {
                let row_id = format!("{id}-{}", c.name);
                let ratio = self
                    .rank_bars
                    .value(&row_id, ratios.get(i).copied().unwrap_or(0.0));
                rows = rows.child(rank_row(
                    row_id,
                    c.name.clone(),
                    ratio,
                    format_count(c.count),
                    None,
                    false,
                ));
            }
            main = main.child(section(title, rows));
        }

        // 本帧读完排行条,把界面上已经没有的行丢掉(等价于 DOM 元素被卸载)
        self.rank_bars.sweep();
        self.rank_bars.drive(window);

        // `@keyframes usageFadeIn`:`opacity 0→1` + `translateY(6px)→0`。
        // gpui 没有 transform,位移用上外边距等价(与 `main.rs` 的 `panelSwapIn`
        // 同一套路);跑完直接不挂包装层。
        let fade = self.fade_in.drive(window);
        body.child(if fade >= 1.0 {
            main.into_any_element()
        } else {
            div()
                .opacity(fade)
                .mt(px(USAGE_FADE_SHIFT * (1.0 - fade)))
                .child(main)
                .into_any_element()
        })
    }

    /// 趋势图(`DailyChart.tsx`)。**补空桶**是第一优先(后端快照稀疏,不补
    /// 画不出完整时间轴);补齐后仍只有 1 个桶时退化成摘要卡(孤点图没有信息量)。
    ///
    /// 版式与 recharts 的 `ComposedChart` 对齐:
    ///
    /// ```text
    /// ┌───────────────────────────────────────────────┐ ↑ margin.top 10
    /// │  $0.40 ┊······················· 40           │
    /// │  $0.30 ┊······················· 30           │ 绘图区(ChartCanvas)
    /// │        ┊    ╭──╮                             │ 左轴成本 / 右轴调用数
    /// │  $0.00 ┊────┴──┴────────────── 0            │ ↓
    /// │  08-01     08-08     08-15     08-22          │ X 轴 30
    /// └───────────────────────────────────────────────┘
    /// ```
    ///
    /// 几何(曲线采样 / 刻度取值 / 标签稀释)全在 [`mt_ui::chart`],**这里只负责
    /// 摆文本和挂 hover**:轴标签是普通 `div`(自绘元素画字要自己 shape,而字号
    /// 字族是壳的主题量),hover 列是盖在画布上的透明格子。
    fn render_chart(&mut self, stats: &UsageStatsPayload, cx: &mut Context<Self>) -> AnyElement {
        let now = chrono::Local::now();
        if stats.daily.is_empty() {
            return div()
                .h(px(CHART_HEIGHT))
                .flex()
                .items_center()
                .justify_center()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_muted())
                .child(t("usageStats", "noDailyData"))
                .into_any_element();
        }
        let buckets = fill_buckets(
            &stats.daily,
            self.range,
            &self.custom_from,
            &self.custom_to,
            now,
        );
        if buckets.len() == 1 {
            let d = &buckets[0];
            return div()
                .h(px(CHART_HEIGHT))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(d.date.clone()),
                )
                .child(
                    div()
                        .text_size(ui::font_px(30.0))
                        .text_color(ui::accent())
                        .child(format_cost(d.cost)),
                )
                .child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_secondary())
                        .child(tr!(
                            "usageStats",
                            "callsCount",
                            count = format_count(d.calls)
                        )),
                )
                .into_any_element();
        }

        // 几何缓存:数据没变就复用上一份(性能红线 —— 不许每帧重建曲线)。
        // 归一化坐标与像素无关,所以拖窗口改宽度**不**让缓存失效。
        let costs: Vec<f64> = buckets.iter().map(|d| d.cost).collect();
        let calls: Vec<f64> = buckets.iter().map(|d| d.calls as f64).collect();
        let key = mt_ui::chart::ChartKey::of(&costs, &calls);
        let model = match &self.chart_cache {
            Some((cached, model)) if *cached == key => model.clone(),
            _ => {
                let model = Rc::new(mt_ui::chart::ChartModel::build(
                    &costs,
                    &calls,
                    CHART_TICK_COUNT,
                ));
                self.chart_cache = Some((key, model.clone()));
                model
            }
        };

        let colors = mt_ui::chart::ChartColors {
            // `<linearGradient>` 的两个 stop:accent 0.3 → accent 0.02
            area_top: ui::with_alpha(ui::accent(), 0.3),
            area_bottom: ui::with_alpha(ui::accent(), 0.02),
            line: ui::accent(),
            // `<Bar fill="var(--text-muted)" opacity={0.28}>`
            bar: ui::with_alpha(ui::text_muted(), 0.28),
            grid: ui::border_default(),
            dot: ui::accent(),
        };

        // hover 列:盖在画布上的透明格子,一格一个桶(原版是 recharts 的
        // Tooltip + cursor,这里保留 M/Q 批就有的「整列淡底 + 六行详情」)
        let mut hover_row = div().absolute().inset_0().flex();
        for d in &buckets {
            let tip = ChartTip::from(d);
            hover_row = hover_row.child(
                div()
                    .id(SharedString::from(format!("bar-{}", d.date)))
                    .flex_1()
                    .min_w(px(1.0))
                    .h_full()
                    // 与改造前同一档淡底(`--border-subtle`)。⚠️ 这层盖在画布
                    // **之上**,别顺手 `with_alpha` 加浓 —— 那个函数是**赋值**
                    // 不是乘,给 0.5 会直接变成半透明白把曲线洗掉
                    .hover(|el| el.bg(ui::border_subtle()))
                    .tooltip(move |window, cx| {
                        let tip = tip.clone();
                        Tooltip::element(move |_window, _cx| tip.render()).build(window, cx)
                    }),
            );
        }

        // 绘图区宽度只有布局阶段才知道,拿 canvas 量下来供**下一帧**的标签
        // 稀释用(与 `terminal_area` 量分屏尺寸同一套路,同样刻意不 notify)
        let entity = cx.entity();
        let measure = gpui::canvas(
            move |bounds: gpui::Bounds<gpui::Pixels>, _window, cx| {
                entity.update(cx, |this: &mut Self, _cx| {
                    this.chart_width = f32::from(bounds.size.width);
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let plot = div()
            .relative()
            .flex_1()
            .min_w(px(0.0))
            .h(px(CHART_PLOT_HEIGHT))
            .child(measure)
            .child(mt_ui::chart::ChartCanvas::new(
                model.clone(),
                colors,
                px(CHART_PLOT_HEIGHT),
            ))
            .child(hover_row);

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_end()
                    .pt(px(CHART_MARGIN_TOP))
                    .child(axis_labels(&model.left_ticks, true, axis_cost))
                    .child(plot)
                    .child(axis_labels(&model.right_ticks, false, |v| {
                        format_count(v.round().max(0.0) as u64)
                    })),
            )
            .child(x_axis_labels(&buckets, self.chart_width))
            .into_any_element()
    }

    /// Top 会话列表(`TopSessions.tsx`):
    /// 日期(76px 等宽) | 项目名(150px) | 标题(flex-1) | 横条(110px) | 成本 | 调用数。
    fn render_top_sessions(
        &self,
        stats: &UsageStatsPayload,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if stats.top_sessions.is_empty() {
            return empty_hint().into_any_element();
        }
        // 横条基准:cost 榜首;**全 $0(价格缺失)时退化按 tokens**
        let ratios = bar_ratios_or(
            &stats.top_sessions.iter().map(|s| s.cost).collect::<Vec<_>>(),
            &stats
                .top_sessions
                .iter()
                .map(|s| s.tokens as f64)
                .collect::<Vec<_>>(),
        );
        let mut rows = div().flex().flex_col();
        for (i, s) in stats.top_sessions.iter().enumerate() {
            let ratio = ratios.get(i).copied().unwrap_or(0.0);
            let session = s.clone();
            let title = if s.title.is_empty() {
                t("usageStats", "untitled").to_string()
            } else {
                s.title.clone()
            };
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("top-{}-{}", s.agent, s.session_id)))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .py(px(7.0))
                    .px(px(8.0))
                    .mx(px(-8.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|el| el.bg(ui::with_alpha(ui::border_subtle(), 0.6)))
                    .on_click(cx.listener(move |this: &mut Self, _, _window, cx| {
                        this.open_preview(&session, cx);
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(px(76.0))
                            .truncate()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(s.timestamp.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(150.0))
                            .truncate()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_secondary())
                            .child(s.project_name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_primary())
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(110.0))
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .bg(ui::border_subtle())
                            .overflow_hidden()
                            .child(
                                div()
                                    .h_full()
                                    .rounded(px(3.0))
                                    .w(relative(ratio.clamp(0.0, 1.0).max(0.02)))
                                    .bg(gpui::linear_gradient(
                                        90.0,
                                        gpui::linear_color_stop(ui::color_info(), 0.0),
                                        gpui::linear_color_stop(ui::color_ai(), 1.0),
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(56.0))
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_primary())
                            .child(format_cost(s.cost)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(48.0))
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(format_count(s.calls)),
                    ),
            );
        }
        rows.into_any_element()
    }

    /// 会话正文预览(Top 会话点开)。带「‹ 返回」回到统计主体。
    fn render_preview(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(preview) = self.preview.as_ref() else {
            return div().into_any_element();
        };
        let title = preview.title.clone();
        let mut body = div()
            .id("usage-preview-body")
            .flex_1()
            .overflow_y_scroll()
            .px(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0));
        if preview.loading {
            body = body.child(
                div()
                    .py(px(12.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("sessionViewer", "loading")),
            );
        }
        if let Some(err) = &preview.error {
            body = body.child(
                div()
                    .py(px(12.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::color_error())
                    .child(err.clone()),
            );
        }
        for msg in &preview.messages {
            let is_user = msg.role == "user";
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .p(px(8.0))
                    .rounded(px(4.0))
                    .bg(if is_user {
                        ui::bg_overlay()
                    } else {
                        ui::bg_base()
                    })
                    .child(
                        div()
                            .text_size(ui::font_px(10.0))
                            .text_color(ui::text_muted())
                            // 旧版 `SessionViewerModal.tsx` 这两个角色名就是硬编码的
                            // 英文字面量(不进字典),照抄
                            .child(if is_user { "User" } else { "Assistant" }),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_secondary())
                            .child(msg.content.clone()),
                    ),
            );
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(
                        ui::ghost_button(
                            "usage-preview-back",
                            format!("‹ {}", t("fileViewer", "back")),
                        )
                        .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                            this.preview = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_primary())
                            .child(title),
                    ),
            )
            .child(body)
            .into_any_element()
    }
}

/// 排行/Top 会话的空态。
fn empty_hint() -> Div {
    div()
        .py(px(32.0))
        .flex()
        .justify_center()
        .text_size(ui::font_px(13.0))
        .text_color(ui::text_muted())
        .child(t("usageStats", "noSessions"))
}

fn auto_refresh_label(secs: u32) -> String {
    if secs == 0 {
        t("usageStats", "autoRefreshOff").to_string()
    } else {
        // 裸模板串,不进字典(原版 `:455` 同)
        format!("{secs}s")
    }
}

/// 菜单项的勾选前缀。选中是 `✓ `,没选中是**全角空格**(与勾等宽,文字才对得齐)
/// —— 菜单基件没有勾选态,全仓统一走这套文本方案(`menu.rs` 模块注释第 1 条)。
fn check_mark(on: bool) -> &'static str {
    if on { "✓ " } else { "　" }
}

/// 分段控件外壳(`Segmented`,`:79`)。
fn segmented() -> Div {
    div()
        .flex()
        .flex_none()
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::border_default())
        .overflow_hidden()
}

fn segment(id: SharedString, label: &str, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(10.0))
        .py(px(4.0))
        .text_size(ui::font_px(11.0))
        .flex_none()
        .cursor_pointer()
        .when(active, |el| el.bg(ui::accent()).text_color(ui::bg_base()))
        .when(!active, |el| {
            el.text_color(ui::text_muted())
                .hover(|el| el.text_color(ui::text_primary()))
        })
        .child(label.to_string())
}

/// 下拉按钮外壳(自绘 —— `gpui_component::select` 的箭头图标是空白)。
fn dropdown(id: &'static str, label: String, max_w: gpui::Pixels) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.0))
        .max_w(max_w)
        .px(px(6.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::border_default())
        .bg(ui::bg_base())
        .text_size(ui::font_px(11.0))
        .text_color(ui::text_secondary())
        .cursor_pointer()
        .hover(|el| el.border_color(ui::accent()))
        .child(div().flex_1().truncate().child(label))
        // 箭头用字面量而不是 IconName —— 与 `menu.rs` 的子菜单箭头同一条路线
        .child(div().flex_none().text_color(ui::text_muted()).child("▾"))
}

/// 趋势图 hover 详情的六行(`UsageTooltip`,`DailyChart.tsx:75-108`)。
#[derive(Clone)]
struct ChartTip {
    date: String,
    rows: Vec<(gpui::Hsla, String, String)>,
}

impl ChartTip {
    fn from(d: &DailyStat) -> Self {
        Self {
            date: d.date.clone(),
            rows: vec![
                (
                    ui::color_info(),
                    t("usageStats", "tip.totalTokens").to_string(),
                    format_tokens(d.input_tokens + d.output_tokens + d.cache_read_tokens),
                ),
                (
                    ui::color_success(),
                    t("usageStats", "tokens.in").to_string(),
                    format_tokens(d.input_tokens),
                ),
                (
                    ui::color_error(),
                    t("usageStats", "tokens.out").to_string(),
                    format_tokens(d.output_tokens),
                ),
                (
                    ui::color_warning(),
                    t("usageStats", "tokens.cached").to_string(),
                    format_tokens(d.cache_read_tokens),
                ),
                (
                    ui::accent(),
                    t("usageStats", "tip.cost").to_string(),
                    format_cost(d.cost),
                ),
                (
                    ui::text_muted(),
                    t("usageStats", "kpi.calls").to_string(),
                    format_count(d.calls),
                ),
            ],
        }
    }

    fn render(&self) -> Div {
        let mut el = div()
            .min_w(px(168.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .mb(px(6.0))
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_primary())
                    .child(self.date.clone()),
            );
        for (color, label, value) in &self.rows {
            el = el.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .py(px(1.0))
                    .text_size(ui::font_px(11.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(6.0))
                            .h(px(6.0))
                            .rounded_full()
                            .bg(*color),
                    )
                    .child(
                        div()
                            .text_color(ui::text_secondary())
                            .child(label.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(ui::text_primary())
                            .child(value.clone()),
                    ),
            );
        }
        el
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// range 白名单:认不出(含存量的 `'all'`)回落 days30,**不报错**。
    #[test]
    fn range_白名单认不出回落() {
        assert_eq!(UsageRange::from_key("custom"), UsageRange::Custom);
        assert_eq!(UsageRange::from_key("today"), UsageRange::Today);
        assert_eq!(UsageRange::from_key("all"), UsageRange::Days30, "存量的 all");
        assert_eq!(UsageRange::from_key(""), UsageRange::Days30);
        assert_eq!(Scope::from_key("grok"), Scope::Grok);
        assert_eq!(Scope::from_key("gemini"), Scope::All, "认不出回落 all");
        // key 与 TS 侧联合类型字面量一字不差
        assert_eq!(
            UsageRange::ALL.map(|r| r.key()),
            ["today", "days7", "days30", "month", "months3", "months6", "custom"]
        );
    }

    /// 相位互斥且优先级正确 —— 价格未就绪且无旧数据时**绝不渲染 KPI**。
    #[test]
    fn 相位按优先级互斥() {
        // 拉价失败压过一切
        assert_eq!(
            phase_of(true, false, true, Some(9), true),
            Phase::PricingError
        );
        // 拉价中且无快照
        assert_eq!(phase_of(false, true, false, None, false), Phase::PricingLoading);
        // 拉价中但已有旧快照 → 照常出主体(不打断)
        assert_eq!(phase_of(false, true, false, Some(3), false), Phase::Ready);
        // 查询失败排在骨架之前
        assert_eq!(phase_of(false, false, true, None, false), Phase::Error);
        // 查询还没回
        assert_eq!(phase_of(false, false, false, None, false), Phase::Skeleton);
        // backfill 在跑且账本还空 → 骨架而不是空态
        assert_eq!(phase_of(false, false, false, Some(0), true), Phase::Skeleton);
        assert_eq!(phase_of(false, false, false, Some(0), false), Phase::Empty);
        assert_eq!(phase_of(false, false, false, Some(1), false), Phase::Ready);
    }

    /// 自动刷新档位:0 走词条,其余是裸模板串。
    #[test]
    fn 自动刷新档位文案() {
        assert_eq!(AUTO_REFRESH_OPTIONS, [0, 5, 10, 30, 60]);
        assert_eq!(auto_refresh_label(5), "5s");
        assert_eq!(auto_refresh_label(60), "60s");
        assert_eq!(auto_refresh_label(0), t("usageStats", "autoRefreshOff"));
    }

    /// 左轴刻度文案(`DailyChart.tsx` 的 `axisCost`)。
    #[test]
    fn 成本轴刻度上千才换_k() {
        assert_eq!(axis_cost(0.0), "$0.00");
        assert_eq!(axis_cost(0.07), "$0.07");
        assert_eq!(axis_cost(12.5), "$12.50");
        assert_eq!(axis_cost(999.994), "$999.99");
        // 1000 起换算成 K,一位小数
        assert_eq!(axis_cost(1000.0), "$1.0K");
        assert_eq!(axis_cost(1446.8), "$1.4K");
    }

    /// X 轴刻度文案(`tickDate`:小时桶原样,日期桶切掉年份)。
    #[test]
    fn 时间轴刻度切年份保留小时() {
        assert_eq!(axis_date("2026-08-19"), "08-19");
        assert_eq!(axis_date("09:00"), "09:00");
        assert_eq!(axis_date("00:00"), "00:00");
    }

    /// 图表版式常量与 recharts 的 JSX 逐条对齐 —— 改了任何一个都要在这里同步。
    #[test]
    fn 图表版式常量对齐原版() {
        assert_eq!(CHART_HEIGHT, 232.0, "<ComposedChart height={{232}}>");
        assert_eq!(CHART_MARGIN_TOP, 10.0, "margin.top");
        assert_eq!(CHART_LEFT_AXIS, 52.0, "<YAxis yAxisId=cost width={{52}}>");
        assert_eq!(CHART_RIGHT_AXIS, 44.0, "<YAxis yAxisId=calls width={{44}}>");
        assert_eq!(CHART_X_MIN_GAP, 24.0, "<XAxis minTickGap={{24}}>");
        assert_eq!(CHART_TICK_FONT, 9.0, "AXIS_TICK.fontSize");
        // 绘图区 = 总高 − 上留白 − X 轴,必须为正且是三者的差
        assert_eq!(
            CHART_PLOT_HEIGHT,
            CHART_HEIGHT - CHART_MARGIN_TOP - CHART_X_AXIS
        );
        assert!(CHART_PLOT_HEIGHT > 0.0);
    }

    /// 用量面板的两条动效在原版 reduce 段里被**点名豁免**,GPUI 侧不许过闸。
    #[test]
    fn 用量面板动效属豁免面() {
        assert!(
            !mt_ui::motion::USAGE_FADE_IN.respects_reduce,
            "styles.css:471-473 豁免 .usage-fade-in"
        );
        assert!(
            !mt_ui::motion::RANK_BAR.respects_reduce,
            "styles.css:475-477 豁免 .usage-rank-bar"
        );
        assert_eq!(
            mt_ui::motion::USAGE_FADE_IN.duration,
            Duration::from_millis(350)
        );
        assert_eq!(mt_ui::motion::RANK_BAR.duration, Duration::from_millis(500));
        // 骨架屏的脉冲相反:`.animate-pulse` 不在豁免名单,reduce 下必须停
        crate::motion::with_reduce(true, || assert!(!mt_ui::motion::blinks()));
    }

    /// 排行条:首次出现直接落目标(浏览器同款),数据变了才补间。
    #[test]
    fn 排行条首帧不补间_变值才补() {
        let t0 = Instant::now();
        let mut bars = mt_ui::motion::TweenMap::new(mt_ui::motion::RANK_BAR);
        assert_eq!(bars.value_at("proj-a", 0.75, t0), 0.75);
        // 同一帧重复读同一行(渲染里每帧都读)不该把它拖回起点
        assert_eq!(bars.value_at("proj-a", 0.75, t0), 0.75);
        // 数据刷新:从 0.75 补到 0.25,半程(250ms)在中间
        bars.value_at("proj-a", 0.25, t0);
        let mid = t0 + Duration::from_millis(250);
        let v = bars.value_at("proj-a", 0.25, mid);
        assert!(v < 0.75 && v > 0.25, "半程应在两值之间,实际 {v}");
        let end = t0 + Duration::from_millis(500);
        assert!((bars.value_at("proj-a", 0.25, end) - 0.25).abs() < 1e-4);
        assert!(!bars.running_at(end), "跑完就不该再要帧");
    }
}
