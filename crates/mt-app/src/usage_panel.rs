//! 用量统计面板。对应 `src/components/usage/UsageStatsModal.tsx` 与它的四个子件
//! (KpiCards / DailyChart / RankBarList / TopSessions)。
//!
//! ```text
//! 打开面板 ─┬─→ background: usage_ledger_query(账本毫秒级,先出现值)
//!           └─→ background: spawn_usage_ledger_sync(增量同步)
//!                    │ SyncEvent(Progress/Synced) ─→ mpsc ─→ 主线程任务 ─→ 有变化就重查
//! ```
//!
//! **查询也要丢后台**:`usage_ledger_query` 虽是毫秒级纯查询,但打开连接可能等
//! `busy_timeout`(最长 5s),落在 GPUI 主线程上就是整个窗口冻住 —— mt-usage 的
//! 函数注释里写死了这条(「调用方仍不应在 UI 线程上直接调它」)。
//!
//! # 价格表(与旧版最大的偏差)
//!
//! 旧版由前端 `fetch('https://models.dev/api.json')` 拉价、归一后经 invoke 传给
//! 后端;GPUI 壳里没有浏览器,而给 mt-app 加 HTTP 依赖不在本批范围。当前从
//! `{app_data_dir}/model-pricing.json` 读一份可选的价格表,读不到就**明说**
//! 「未接价格表,成本按 $0 计」——绝不把全 0 成本当真数据展示(旧版同一条红线)。
//! 接一个 Rust 侧价格拉取器是本批留下的接线需求,见交付说明。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    Context, Div, Entity, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::tooltip::Tooltip;
use mt_usage::{
    AgentFilter, ModelPrice, SyncEvent, UsageStatsPayload, ledger_db_path,
    spawn_usage_ledger_sync, usage_ledger_query,
};

use crate::i18n::{t, tr};
use crate::store::AppStore;
use crate::ui;

// ─── 时间窗口 ────────────────────────────────────────────────

/// 面板提供的范围清单。设计合同:不提供 all(全盘扫描太重)。
/// `custom`(自选起止)没搬,见交付说明的遗留清单。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageRange {
    Today,
    Days7,
    Days30,
    Month,
    Months3,
    Months6,
}

impl UsageRange {
    pub const ALL: [UsageRange; 6] = [
        Self::Today,
        Self::Days7,
        Self::Days30,
        Self::Month,
        Self::Months3,
        Self::Months6,
    ];

    /// 稳定标识:元素 id 与字典 key 的后半段都用它。
    ///
    /// **不能拿 [`label`](Self::label) 当 id** —— 那是随语言变的文案,
    /// 切一次语言全部 `ElementId` 都换人,点击态/滚动位置一起丢。
    /// 取值与 TS 侧 `UsageRange` 的联合类型字面量一字不差。
    pub const fn key(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Days7 => "days7",
            Self::Days30 => "days30",
            Self::Month => "month",
            Self::Months3 => "months3",
            Self::Months6 => "months6",
        }
    }

    /// 分段控件上的文案(对齐旧版 `usageStats.range.{v}`)。
    pub fn label(self) -> &'static str {
        match self {
            Self::Today => t("usageStats", "range.today"),
            Self::Days7 => t("usageStats", "range.days7"),
            Self::Days30 => t("usageStats", "range.days30"),
            Self::Month => t("usageStats", "range.month"),
            Self::Months3 => t("usageStats", "range.months3"),
            Self::Months6 => t("usageStats", "range.months6"),
        }
    }

    /// 「今天」按小时分桶,其余按日历日。
    pub fn hourly(self) -> bool {
        self == Self::Today
    }
}

/// 范围 → 窗口起点(epoch ms)。
///
/// **本地日历日口径**:today = 本地 00:00 起(绝不用滚动 24h);days7/30 = 含今天的
/// 完整日历日;month/months3/months6 = 对应月份的月初。逐条对照
/// `src/utils/usageDates.ts` 的 `rangeStartDate`。
pub fn range_since_ms(range: UsageRange, now: chrono::DateTime<chrono::Local>) -> i64 {
    use chrono::{Datelike, Duration, TimeZone};
    let today = now.date_naive();
    let start_date = match range {
        UsageRange::Today => today,
        UsageRange::Days7 => today - Duration::days(6),
        UsageRange::Days30 => today - Duration::days(29),
        UsageRange::Month => today.with_day(1).unwrap_or(today),
        UsageRange::Months3 => month_start_back(today, 2),
        UsageRange::Months6 => month_start_back(today, 5),
    };
    // 本地午夜。DST 那天可能没有 00:00,取当天最早的合法时刻。
    let naive = start_date.and_hms_opt(0, 0, 0).unwrap_or_default();
    match chrono::Local.from_local_datetime(&naive).earliest() {
        Some(dt) => dt.timestamp_millis(),
        None => now.timestamp_millis(),
    }
}

/// n 个月前的月初(日历减法,跨年正确)。
fn month_start_back(today: chrono::NaiveDate, months_back: u32) -> chrono::NaiveDate {
    use chrono::Datelike;
    let total = today.year() * 12 + today.month0() as i32 - months_back as i32;
    let year = total.div_euclid(12);
    let month0 = total.rem_euclid(12) as u32;
    chrono::NaiveDate::from_ymd_opt(year, month0 + 1, 1).unwrap_or(today)
}

/// 本地时区偏移,**分钟、西为正** —— 与 JS `getTimezoneOffset()` 同号,
/// 也就是 mt-usage 里 `local_ms = ts - offset*60000` 要的那个方向。
pub fn tz_offset_minutes(now: chrono::DateTime<chrono::Local>) -> i32 {
    use chrono::Offset;
    -now.offset().fix().local_minus_utc() / 60
}

// ─── 展示格式 ────────────────────────────────────────────────

/// 金额:统一两位小数 + 千分位;微额显示 `<$0.01`(不四舍五入成 `$0.00` 假象)。
pub fn format_cost(v: f64) -> String {
    if v >= 0.01 {
        format!("${}", group_thousands(&format!("{v:.2}")))
    } else if v > 0.0 {
        "<$0.01".into()
    } else {
        "$0".into()
    }
}

/// token 数:K/M/B 三档缩写。
pub fn format_tokens(v: u64) -> String {
    let f = v as f64;
    if f >= 1e9 {
        let b = f / 1e9;
        return if b >= 10.0 {
            format!("{}B", b.round())
        } else {
            format!("{b:.1}B")
        };
    }
    if f >= 1e6 {
        let m = f / 1e6;
        return if m >= 10.0 {
            format!("{}M", m.round())
        } else {
            format!("{m:.1}M")
        };
    }
    if f >= 1e3 {
        let k = f / 1e3;
        return if k >= 10.0 {
            format!("{}K", k.round())
        } else {
            format!("{k:.1}K")
        };
    }
    v.to_string()
}

/// 计数:千分位。
pub fn format_count(v: u64) -> String {
    group_thousands(&v.to_string())
}

fn group_thousands(s: &str) -> String {
    let (int_part, rest) = match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let (sign, digits) = match int_part.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", int_part),
    };
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    match rest {
        Some(r) => format!("{sign}{out}.{r}"),
        None => format!("{sign}{out}"),
    }
}

/// 缓存命中率 = cacheRead ÷ (input + cacheRead + cacheWrite);分母 0 → `None`。
pub fn cache_hit_rate(input: u64, cache_read: u64, cache_write: u64) -> Option<f64> {
    let denom = input + cache_read + cache_write;
    if denom == 0 {
        return None;
    }
    Some(cache_read as f64 / denom as f64 * 100.0)
}

/// 模型展示短名:通用规则推导,不维护映射表(新模型零改动)。
/// `claude-opus-4-8` → `Opus 4.8`;`gpt-5-3-codex` → `GPT-5.3 Codex`。
pub fn model_short_name(model: &str) -> String {
    fn cap(s: &str) -> String {
        let mut c = s.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    }
    let is_num = |s: &&str| s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty();
    let parts: Vec<&str> = model.split('-').collect();
    if parts.first() == Some(&"claude") && parts.len() >= 2 {
        let nums: Vec<&str> = parts[1..].iter().copied().filter(|p| is_num(p)).collect();
        let words: Vec<String> = parts[1..]
            .iter()
            .filter(|p| !is_num(p))
            .map(|p| cap(p))
            .collect();
        let joined = [words.join(" "), nums.join(".")]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        return if joined.is_empty() {
            model.to_string()
        } else {
            joined
        };
    }
    if parts.first() == Some(&"gpt") && parts.len() >= 2 {
        let mut nums: Vec<&str> = Vec::new();
        let mut words: Vec<String> = Vec::new();
        for p in &parts[1..] {
            if is_num(p) && words.is_empty() {
                nums.push(p);
            } else {
                words.push(cap(p));
            }
        }
        let tail = if words.is_empty() {
            String::new()
        } else {
            format!(" {}", words.join(" "))
        };
        return format!("GPT-{}{}", nums.join("."), tail);
    }
    model.to_string()
}

/// 排行条的相对长度(0.0~1.0)。最大值为 0 时全部归零,不是除零。
pub fn bar_ratios(values: &[f64]) -> Vec<f32> {
    let max = values.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return vec![0.0; values.len()];
    }
    values.iter().map(|v| (v / max) as f32).collect()
}

/// 首选按金额画条,整窗金额全为 0(没接价格表)时退回按第二组值画 ——
/// 否则整页排行都是空槽,看不出谁多谁少。
pub fn bar_ratios_or(primary: &[f64], fallback: &[f64]) -> Vec<f32> {
    if primary.iter().any(|v| *v > 0.0) {
        bar_ratios(primary)
    } else {
        bar_ratios(fallback)
    }
}

/// 可选的本地价格表(`{app_data_dir}/model-pricing.json`)。
///
/// 形如 `{"claude-opus-4-8": {"input": 1.5e-5, "output": 7.5e-5, ...}}`,
/// 单位 $/token(与旧版前端 ÷1e6 之后的口径一致)。
pub fn load_local_pricing(app_data_dir: &std::path::Path) -> HashMap<String, ModelPrice> {
    let path = app_data_dir.join("model-pricing.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, ModelPrice>>(&s).ok())
        .unwrap_or_default()
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

pub struct UsagePanel {
    store: Entity<AppStore>,
    app_data_dir: PathBuf,
    scope: Scope,
    range: UsageRange,
    /// `Some` = 只看当前项目。
    project_scope: Option<String>,
    stats: Option<UsageStatsPayload>,
    loading: bool,
    error: Option<String>,
    /// backfill(账本首建全量同步)进度;非 backfill 期间为 None。
    progress: Option<(usize, usize)>,
    pricing: HashMap<String, ModelPrice>,
    /// 查询序号:切参数后旧查询返回时不得覆盖新结果。
    query_seq: u64,
    /// 当前查询。**一份**而不是一列表:赋新值即 drop 掉上一个 Task,
    /// 顺带取消被顶掉的那次查询(点四下时间范围不该留四个任务在跑)。
    _query_task: Option<Task<()>>,
    /// 账本同步的事件泵。同理只留最新一份 —— 手动刷新会再起一次同步。
    _sync_task: Option<Task<()>>,
}

impl UsagePanel {
    pub fn new(store: Entity<AppStore>, app_data_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let pricing = load_local_pricing(&app_data_dir);
        let mut panel = Self {
            store,
            app_data_dir,
            scope: Scope::All,
            range: UsageRange::Days30,
            project_scope: None,
            stats: None,
            loading: false,
            error: None,
            progress: None,
            pricing,
            query_seq: 0,
            _query_task: None,
            _sync_task: None,
        };
        panel.query(cx);
        panel.start_sync(cx);
        panel
    }

    /// 查账本(毫秒级)。永远丢后台:打开连接可能等 busy_timeout 最长 5s。
    fn query(&mut self, cx: &mut Context<Self>) {
        self.query_seq += 1;
        let seq = self.query_seq;
        self.loading = true;

        let dir = self.app_data_dir.clone();
        let agents = self.scope.filter();
        let range = self.range;
        let project = self.project_scope.clone();
        let pricing = self.pricing.clone();
        let now = chrono::Local::now();
        let since = range_since_ms(range, now);
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
                        None,
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
                this.loading = false;
                match result {
                    Ok(stats) => {
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

    /// 触发一次增量同步,进度事件泵回主线程;有变化就重查。
    fn start_sync(&mut self, cx: &mut Context<Self>) {
        let Ok(db_path) = ledger_db_path(&self.app_data_dir) else {
            return;
        };
        let (tx, mut rx) = mpsc::unbounded::<SyncEvent>();
        // sink 跑在同步线程上,只管往 channel 里丢
        let sink: Arc<mt_usage::SyncSink> = Arc::new(move |event: SyncEvent| {
            let _ = tx.unbounded_send(event);
        });
        spawn_usage_ledger_sync(db_path, sink);

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
                            added > 0
                        }
                    })
                    .unwrap_or(false);
                if should_requery {
                    let _ = this.update(cx, |this: &mut Self, cx| this.query(cx));
                }
            }
        }));
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.query(cx);
    }

    fn set_range(&mut self, range: UsageRange, cx: &mut Context<Self>) {
        if self.range == range {
            return;
        }
        self.range = range;
        self.query(cx);
    }

    fn toggle_project_scope(&mut self, cx: &mut Context<Self>) {
        self.project_scope = match self.project_scope {
            Some(_) => None,
            None => self.store.read(cx).active_project().map(|p| p.path.clone()),
        };
        self.query(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.pricing = load_local_pricing(&self.app_data_dir);
        self.start_sync(cx);
        self.query(cx);
    }
}

/// 一格 KPI。
fn kpi(title: &str, value: String, sub: Option<String>) -> Div {
    div()
        .flex_1()
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::border_subtle())
        .bg(ui::bg_base())
        .child(
            div()
                .text_size(px(10.0))
                .text_color(ui::text_muted())
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(16.0))
                .text_color(ui::text_primary())
                .child(value),
        )
        .when_some(sub, |el, sub| {
            el.child(
                div()
                    .text_size(px(10.0))
                    .text_color(ui::text_muted())
                    .child(sub),
            )
        })
}

/// 一条排行(名称 + 条 + 数值)。
fn rank_row(
    id: impl Into<SharedString>,
    name: String,
    ratio: f32,
    value: String,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.0))
        .py(px(2.0))
        .child(
            div()
                .w(px(120.0))
                .flex_none()
                .truncate()
                .text_size(px(11.0))
                .text_color(ui::text_secondary())
                .child(name),
        )
        .child(
            div()
                .flex_1()
                .h(px(6.0))
                .rounded(px(3.0))
                .bg(ui::bg_overlay())
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(ratio.clamp(0.0, 1.0)))
                        .rounded(px(3.0))
                        .bg(ui::accent()),
                ),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(11.0))
                .text_color(ui::text_muted())
                .child(value),
        )
}

impl Render for UsagePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scope = self.scope;
        let range = self.range;
        let project_scoped = self.project_scope.is_some();
        let has_pricing = !self.pricing.is_empty();
        // 项目开关的文案照搬旧版那个下拉框:未限定时显示「全部项目」,限定时
        // 显示项目名 —— 于是不必造一条「仅当前项目」的新词条。
        let project_scope_label = if project_scoped {
            self.store
                .read(cx)
                .active_project()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| t("usageStats", "scope.allProjects").to_string())
        } else {
            t("usageStats", "scope.allProjects").to_string()
        };

        // 分段控件
        let mut scope_bar = div().flex().gap(px(4.0));
        for s in Scope::ALL {
            let active = s == scope;
            scope_bar = scope_bar.child(
                div()
                    .id(SharedString::from(format!("usage-scope-{}", s.key())))
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(3.0))
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .when(active, |el| el.bg(ui::accent()).text_color(ui::bg_base()))
                    .when(!active, |el| {
                        el.text_color(ui::text_muted())
                            .hover(|el| el.text_color(ui::text_primary()))
                    })
                    .on_click(cx.listener(move |this: &mut Self, _, _window, cx| {
                        this.set_scope(s, cx);
                    }))
                    .child(s.label()),
            );
        }
        let mut range_bar = div().flex().gap(px(4.0));
        for r in UsageRange::ALL {
            let active = r == range;
            range_bar = range_bar.child(
                div()
                    .id(SharedString::from(format!("usage-range-{}", r.key())))
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(3.0))
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .when(active, |el| el.bg(ui::accent()).text_color(ui::bg_base()))
                    .when(!active, |el| {
                        el.text_color(ui::text_muted())
                            .hover(|el| el.text_color(ui::text_primary()))
                    })
                    .on_click(cx.listener(move |this: &mut Self, _, _window, cx| {
                        this.set_range(r, cx);
                    }))
                    .child(r.label()),
            );
        }

        let header = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(scope_bar)
            .child(range_bar)
            .child(
                div()
                    .id("usage-project-scope")
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(3.0))
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .when(project_scoped, |el| {
                        el.bg(ui::accent()).text_color(ui::bg_base())
                    })
                    .when(!project_scoped, |el| el.text_color(ui::text_muted()))
                    .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                        this.toggle_project_scope(cx);
                    }))
                    .child(project_scope_label),
            )
            .child(
                div()
                    .ml_auto()
                    .id("usage-refresh")
                    .px(px(6.0))
                    .text_size(px(12.0))
                    .text_color(ui::text_muted())
                    .cursor_pointer()
                    .hover(|el| el.text_color(ui::accent()))
                    .tooltip(|window, cx| {
                        Tooltip::new(t("usageStats", "refresh")).build(window, cx)
                    })
                    .on_click(cx.listener(|this: &mut Self, _, _window, cx| this.refresh(cx)))
                    .child("↻"),
            );

        let mut body = div()
            .id("usage-body")
            .flex_1()
            .overflow_y_scroll()
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(12.0));

        if let Some((processed, total)) = self.progress {
            body = body.child(
                div()
                    .text_size(px(11.0))
                    .text_color(ui::text_muted())
                    .child(format!(
                        "{} {}",
                        t("usageStats", "backfilling"),
                        tr!(
                            "usageStats",
                            "progress",
                            processed = processed,
                            total = total
                        )
                    )),
            );
        }
        if !has_pricing {
            body = body.child(
                div()
                    .p(px(8.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(ui::color_ai_working())
                    .text_size(px(11.0))
                    .text_color(ui::text_secondary())
                    // 旧版是 fetch models.dev 失败时的 `pricingError` 提示;
                    // GPUI 侧改读本地 model-pricing.json,场景一致(拿不到价格 →
                    // 成本不可信),文案沿用同一条 key,后面接上 GPUI 特有的补救
                    // 办法(`pricingLocalHint` 是 M 批往 TS 源头补的条目)。
                    .child(format!(
                        "{} · {}",
                        t("usageStats", "pricingError"),
                        t("usageStats", "pricingLocalHint")
                    )),
            );
        }
        if let Some(err) = &self.error {
            body = body.child(
                div()
                    .text_size(px(12.0))
                    .text_color(ui::color_error())
                    .child(err.clone()),
            );
        }

        match &self.stats {
            None if self.loading => {
                body = body.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(ui::text_muted())
                        // 旧版这一档画的是骨架屏(没有文字),先用会话列表那条
                        // 通用「加载中…」占位,骨架屏见审计缺口 #17。
                        .child(t("sessionList", "loading")),
                );
            }
            None => {}
            Some(stats) => {
                let hit = cache_hit_rate(
                    stats.input_tokens,
                    stats.cache_read_tokens,
                    stats.cache_write_tokens,
                );
                body = body.child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(kpi(
                            t("usageStats", "kpi.cost"),
                            format_cost(stats.total_cost),
                            None,
                        ))
                        .child(kpi(
                            t("usageStats", "kpi.calls"),
                            format_count(stats.total_calls),
                            // 旧版会话数是独立一格 KPI,这里并成副标题(四格排满了)
                            Some(format!(
                                "{} {}",
                                t("usageStats", "kpi.sessions"),
                                stats.session_count
                            )),
                        ))
                        .child(kpi(
                            &format!(
                                "{} / {}",
                                t("usageStats", "tokens.in"),
                                t("usageStats", "tokens.out")
                            ),
                            format!(
                                "{} / {}",
                                format_tokens(stats.input_tokens),
                                format_tokens(stats.output_tokens)
                            ),
                            None,
                        ))
                        .child(kpi(
                            t("usageStats", "kpi.cacheHit"),
                            hit.map(|h| format!("{h:.0}%")).unwrap_or_else(|| "—".into()),
                            Some(format!(
                                "{} {}",
                                t("usageStats", "tokens.cached"),
                                format_tokens(stats.cache_read_tokens)
                            )),
                        )),
                );

                // 趋势图:等宽柱,高度按窗口内最大值归一
                if !stats.daily.is_empty() {
                    let costs: Vec<f64> = stats.daily.iter().map(|d| d.cost).collect();
                    // 全窗零成本时(未接价格表)退化成按调用次数画,免得空图
                    let values: Vec<f64> = if costs.iter().all(|c| *c <= 0.0) {
                        stats.daily.iter().map(|d| d.calls as f64).collect()
                    } else {
                        costs
                    };
                    let ratios = bar_ratios(&values);
                    let mut chart = div()
                        .flex()
                        .items_end()
                        .gap(px(2.0))
                        .h(px(120.0))
                        .w_full();
                    for (i, d) in stats.daily.iter().enumerate() {
                        let ratio = ratios.get(i).copied().unwrap_or(0.0);
                        chart = chart.child(
                            div()
                                .id(SharedString::from(format!("bar-{}", d.date)))
                                .flex_1()
                                .h(gpui::relative(ratio.max(0.01)))
                                .bg(ui::accent())
                                .rounded(px(1.0)),
                        );
                    }
                    let first = stats.daily.first().map(|d| d.date.clone()).unwrap_or_default();
                    let last = stats.daily.last().map(|d| d.date.clone()).unwrap_or_default();
                    body = body.child(
                        div()
                            .child(ui::section_title(t("usageStats", "dailyActivity")))
                            .child(chart)
                            .child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .text_size(px(10.0))
                                    .text_color(ui::text_muted())
                                    .child(first)
                                    .child(last),
                            ),
                    );
                }

                // 按项目
                if !stats.by_project.is_empty() {
                    let ratios = bar_ratios_or(
                        &stats.by_project.iter().map(|p| p.cost).collect::<Vec<_>>(),
                        &stats
                            .by_project
                            .iter()
                            .map(|p| p.tokens as f64)
                            .collect::<Vec<_>>(),
                    );
                    let mut rows = div().flex().flex_col();
                    for (i, p) in stats.by_project.iter().enumerate() {
                        rows = rows.child(rank_row(
                            format!("proj-{}", p.path),
                            p.name.clone(),
                            ratios.get(i).copied().unwrap_or(0.0),
                            format!("{} · {}", format_cost(p.cost), format_tokens(p.tokens)),
                        ));
                    }
                    body = body.child(div().child(ui::section_title(t("usageStats", "byProject"))).child(rows));
                }

                // 按模型
                if !stats.by_model.is_empty() {
                    let top: Vec<_> = stats.by_model.iter().take(8).collect();
                    let ratios = bar_ratios_or(
                        &top.iter().map(|m| m.cost).collect::<Vec<_>>(),
                        &top.iter().map(|m| m.tokens as f64).collect::<Vec<_>>(),
                    );
                    let mut rows = div().flex().flex_col();
                    for (i, m) in top.iter().enumerate() {
                        let name = if m.model.is_empty() {
                            t("usageStats", "unknownModel").to_string()
                        } else {
                            model_short_name(&m.model)
                        };
                        rows = rows.child(rank_row(
                            format!("model-{}", m.model),
                            name,
                            ratios.get(i).copied().unwrap_or(0.0),
                            format!("{} · {}", format_cost(m.cost), format_tokens(m.tokens)),
                        ));
                    }
                    body = body.child(div().child(ui::section_title(t("usageStats", "byModel"))).child(rows));
                }

                // 按供应商
                if !stats.by_provider.is_empty() {
                    let ratios = bar_ratios_or(
                        &stats.by_provider.iter().map(|p| p.cost).collect::<Vec<_>>(),
                        &stats
                            .by_provider
                            .iter()
                            .map(|p| p.calls as f64)
                            .collect::<Vec<_>>(),
                    );
                    let mut rows = div().flex().flex_col();
                    for (i, p) in stats.by_provider.iter().enumerate() {
                        rows = rows.child(rank_row(
                            format!("provider-{}", p.provider),
                            p.provider.clone(),
                            ratios.get(i).copied().unwrap_or(0.0),
                            format!("{} · {}", format_cost(p.cost), format_count(p.calls)),
                        ));
                    }
                    body = body.child(div().child(ui::section_title(t("usageStats", "byProvider"))).child(rows));
                }

                // Top 会话
                if !stats.top_sessions.is_empty() {
                    let mut rows = div().flex().flex_col().gap(px(2.0));
                    for s in &stats.top_sessions {
                        rows = rows.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .py(px(2.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .text_size(px(11.0))
                                        .text_color(ui::text_secondary())
                                        .child(s.title.clone()),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(10.0))
                                        .text_color(ui::text_muted())
                                        .child(s.project_name.clone()),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.0))
                                        .text_color(ui::text_muted())
                                        .child(format!(
                                            "{} · {}",
                                            format_cost(s.cost),
                                            format_tokens(s.tokens)
                                        )),
                                ),
                        );
                    }
                    body = body.child(div().child(ui::section_title(t("usageStats", "topSessions"))).child(rows));
                }

                // 工具 / Shell / MCP 计数排行。
                //
                // 这三块是 GPUI 侧新加的(`byTool`/`byShell`/`byMcp` 在 `types.ts`
                // 里有类型,旧版面板从没渲染过),`usageStats.{byTool,byShell}`
                // 由 M 批补进 TS 源头;MCP 是专有名词,与厂商名一样不进字典。
                // `id` 用不随语言变的稳定前缀,免得切语言把元素身份也换了。
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
                        rows = rows.child(rank_row(
                            format!("{id}-{}", c.name),
                            c.name.clone(),
                            ratios.get(i).copied().unwrap_or(0.0),
                            format_count(c.count),
                        ));
                    }
                    body = body.child(div().child(ui::section_title(title)).child(rows));
                }
            }
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Timelike};

    #[test]
    fn 金额按两位小数与千分位() {
        assert_eq!(format_cost(0.0), "$0");
        assert_eq!(format_cost(0.004), "<$0.01", "微额不许四舍五入成 $0.00");
        assert_eq!(format_cost(0.01), "$0.01");
        assert_eq!(format_cost(1446.8), "$1,446.80");
        assert_eq!(format_cost(1_234_567.891), "$1,234,567.89");
    }

    #[test]
    fn token_按三档缩写() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_500), "1.5K");
        assert_eq!(format_tokens(15_000), "15K");
        assert_eq!(format_tokens(1_500_000), "1.5M");
        assert_eq!(format_tokens(15_000_000), "15M");
        assert_eq!(format_tokens(2_500_000_000), "2.5B");
    }

    #[test]
    fn 计数千分位() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(44_011), "44,011");
        assert_eq!(format_count(100), "100");
        assert_eq!(format_count(1_000), "1,000");
    }

    #[test]
    fn 缓存命中率分母为零时无值() {
        assert_eq!(cache_hit_rate(0, 0, 0), None);
        let r = cache_hit_rate(25, 75, 0).unwrap();
        assert!((r - 75.0).abs() < 1e-9);
    }

    #[test]
    fn 模型短名按通用规则推导() {
        assert_eq!(model_short_name("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_short_name("claude-3-7-sonnet"), "Sonnet 3.7");
        assert_eq!(model_short_name("gpt-5-3-codex"), "GPT-5.3 Codex");
        assert_eq!(model_short_name("glm-4-plus"), "glm-4-plus", "认不出就原样显示");
        assert_eq!(model_short_name(""), "");
    }

    #[test]
    fn 排行条比例按最大值归一() {
        assert_eq!(bar_ratios(&[10.0, 5.0, 0.0]), vec![1.0, 0.5, 0.0]);
        assert_eq!(bar_ratios(&[0.0, 0.0]), vec![0.0, 0.0], "全零不许除零");
        assert!(bar_ratios(&[]).is_empty());
    }

    /// 金额全为 0(没接价格表)时按第二组值画条,而不是整页空槽。
    #[test]
    fn 金额全零时排行条改按备用值() {
        assert_eq!(
            bar_ratios_or(&[0.0, 0.0], &[100.0, 50.0]),
            vec![1.0, 0.5]
        );
        assert_eq!(
            bar_ratios_or(&[4.0, 2.0], &[100.0, 50.0]),
            vec![1.0, 0.5],
            "有金额就用金额"
        );
        assert_eq!(bar_ratios_or(&[0.0], &[0.0]), vec![0.0]);
    }

    /// 窗口起点是**本地日历**口径:今天 = 本地 00:00,7 天 = 含今天的 7 个日历日。
    #[test]
    fn 范围起点按本地日历日() {
        let now = chrono::Local
            .with_ymd_and_hms(2026, 8, 18, 15, 30, 0)
            .unwrap();

        let today = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Today, now))
            .unwrap();
        assert_eq!((today.year(), today.month(), today.day()), (2026, 8, 18));
        assert_eq!((today.hour(), today.minute()), (0, 0), "绝不是滚动 24h");

        let d7 = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Days7, now))
            .unwrap();
        assert_eq!(d7.day(), 12, "含今天的 7 个日历日 → 12 日 00:00");

        let month = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Month, now))
            .unwrap();
        assert_eq!((month.month(), month.day()), (8, 1));

        let m3 = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Months3, now))
            .unwrap();
        assert_eq!((m3.month(), m3.day()), (6, 1));
    }

    /// 跨年回溯:1 月往前 3 个月落到上一年 11 月。
    #[test]
    fn 月份回溯跨年() {
        let now = chrono::Local.with_ymd_and_hms(2026, 1, 15, 9, 0, 0).unwrap();
        let m3 = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Months3, now))
            .unwrap();
        assert_eq!((m3.year(), m3.month(), m3.day()), (2025, 11, 1));
    }

    /// 时区偏移与 JS `getTimezoneOffset()` 同号(西为正)—— 反了会整体错一天。
    #[test]
    fn 时区偏移符号与后端口径一致() {
        let now = chrono::Local::now();
        use chrono::Offset;
        let east_seconds = now.offset().fix().local_minus_utc();
        assert_eq!(tz_offset_minutes(now), -east_seconds / 60);
    }

    #[test]
    fn 价格表缺失时返回空表() {
        let dir = std::env::temp_dir().join("mt-app-usage-pricing-missing");
        assert!(load_local_pricing(&dir).is_empty());
    }
}
