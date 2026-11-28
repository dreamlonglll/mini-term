//! 用量面板的纯逻辑层:**时间窗口**与**展示格式**。
//!
//! 对应旧版的两个纯逻辑模块(`utils/usageDates.ts` 与 `KpiCards.tsx` 里的格式化
//! 辅助),零 gpui 依赖 —— 面板本体在 [`super`],这里只有能单独跑测的纯函数。

use std::collections::HashMap;

use mt_usage::DailyStat;

use crate::i18n::t;

// ─── 时间窗口 ────────────────────────────────────────────────

/// 面板提供的范围清单。设计合同:不提供 all(全盘扫描太重)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageRange {
    Today,
    Days7,
    Days30,
    Month,
    Months3,
    Months6,
    /// 自选起止。窗口由 `custom_from` / `custom_to` 决定,见 [`range_since_ms`]。
    Custom,
}

impl UsageRange {
    pub const ALL: [UsageRange; 7] = [
        Self::Today,
        Self::Days7,
        Self::Days30,
        Self::Month,
        Self::Months3,
        Self::Months6,
        Self::Custom,
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
            Self::Custom => "custom",
        }
    }

    /// 白名单解析。认不出(含存量的 `'all'`)一律回落 days30 ——
    /// 与旧版 `loadPref` 同,**不写回、不报错**。
    pub fn from_key(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|r| r.key() == key)
            .unwrap_or(Self::Days30)
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
            Self::Custom => t("usageStats", "range.custom"),
        }
    }
}

/// custom 起止的哪一端。只决定「只给了日期」时补哪个时刻:
/// 起点补 `00:00:00`、截止补 `23:59:59` —— 与支持时分秒之前「起始日 00:00 起、
/// 含截止日全天」的口径一字不差。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    From,
    To,
}

impl Edge {
    pub const fn of(is_from: bool) -> Self {
        if is_from { Self::From } else { Self::To }
    }

    /// 「只给了日期」时这一端补的时刻。
    pub fn default_time(self) -> chrono::NaiveTime {
        match self {
            Self::From => chrono::NaiveTime::MIN,
            Self::To => chrono::NaiveTime::from_hms_opt(23, 59, 59).unwrap_or_default(),
        }
    }
}

/// custom 起止的规范形态(落盘、输入框回显都是它)。
const DATETIME_FMT: &str = "%Y-%m-%d %H:%M:%S";

/// `"YYYY-MM-DD"` → 本地日历日。形态不符返回 `None`
/// (对齐 `usageDates.ts::parseLocalDate` 的正则闸门)。
pub fn parse_local_date(s: &str) -> Option<chrono::NaiveDate> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    if !b
        .iter()
        .enumerate()
        .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return None;
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// `"HH:MM"` / `"HH:MM:SS"`(每段恰好两位数字)→ 时刻。省掉的秒按 `00`。
fn parse_local_time(s: &str) -> Option<chrono::NaiveTime> {
    let parts: Vec<&str> = s.split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|p| p.len() != 2 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let num = |i: usize| parts.get(i).map_or(Some(0), |p| p.parse::<u32>().ok());
    chrono::NaiveTime::from_hms_opt(num(0)?, num(1)?, num(2)?)
}

/// custom 起止 → 本地时刻。收三种形态(日期与时刻之间一个空格或 `T`):
///
/// - `YYYY-MM-DD` —— 时刻按 [`Edge`] 补(起点 00:00:00、截止 23:59:59)
/// - `YYYY-MM-DD HH:MM` —— 秒按 00
/// - `YYYY-MM-DD HH:MM:SS` —— 规范形态
///
/// 存量偏好里只有 `YYYY-MM-DD`,第一条就是它的读盘兼容。形态不符返回 `None`。
pub fn parse_local_datetime(s: &str, edge: Edge) -> Option<chrono::NaiveDateTime> {
    let s = s.trim();
    let (date, time) = match s.split_once([' ', 'T']) {
        Some((date, time)) => (date, parse_local_time(time)?),
        None => (s, edge.default_time()),
    };
    Some(parse_local_date(date)?.and_time(time))
}

/// 本地时刻 → 规范形态 `YYYY-MM-DD HH:MM:SS`。
pub fn format_local_datetime(dt: chrono::NaiveDateTime) -> String {
    dt.format(DATETIME_FMT).to_string()
}

/// custom 起止输入的**提交闸门**(`usageDates.ts::acceptDateInput` 的时分秒版)。
///
/// 认得出(见 [`parse_local_datetime`])才更新承诺值,并改写成规范形态 ——
/// 只敲了日期也回显成补齐时刻的整串,用户一眼看出「这一端算到几点」;
/// 其余输入保持上一个有效值(受控输入随之回弹)。
/// **空串一旦进查询,custom 会静默退化成无上界 / 近 30 天**,展示远大于所选
/// 范围的数据 —— 这道闸是硬要求。
pub fn accept_datetime_input(next: &str, prev: &str, edge: Edge) -> String {
    match parse_local_datetime(next, edge) {
        Some(dt) => format_local_datetime(dt),
        None => prev.to_string(),
    }
}

/// custom 起止的缺省值:起点 = 29 天前 00:00:00,截止 = 今天 23:59:59
/// (与 days30 同一个窗口)。
pub fn default_custom_edge(edge: Edge, now: chrono::DateTime<chrono::Local>) -> String {
    let day = match edge {
        Edge::From => now.date_naive() - chrono::Duration::days(29),
        Edge::To => now.date_naive(),
    };
    format_local_datetime(day.and_time(edge.default_time()))
}

/// custom 起点的最早允许日(近一年)。原版 `<input type="date">` 的 `min`
/// 只标 `:invalid`、**不拦截键入**,钳位靠 since/until 两条规则兜住。
pub(super) fn custom_floor(today: chrono::NaiveDate) -> chrono::NaiveDate {
    today - chrono::Duration::days(364)
}

/// 本地时刻 → epoch ms。DST 拨快那一小时里的时刻不存在,顺延一小时再取;
/// 仍取不到才用 `fallback`。
fn local_ms(dt: chrono::NaiveDateTime, fallback: i64) -> i64 {
    use chrono::TimeZone;
    [dt, dt + chrono::Duration::hours(1)]
        .into_iter()
        .find_map(|dt| chrono::Local.from_local_datetime(&dt).earliest())
        .map_or(fallback, |dt| dt.timestamp_millis())
}

/// 本地日历日 → 当天 00:00 的 epoch ms。
fn midnight_ms(date: chrono::NaiveDate, fallback: i64) -> i64 {
    local_ms(date.and_time(chrono::NaiveTime::MIN), fallback)
}

/// epoch ms → 本地时刻(补桶窗口用)。
fn local_datetime(ms: i64, now: chrono::DateTime<chrono::Local>) -> chrono::NaiveDateTime {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or(now)
        .naive_local()
}

/// 范围 → 窗口起点(epoch ms)。
///
/// **本地日历日口径**:today = 本地 00:00 起(绝不用滚动 24h);days7/30 = 含今天的
/// 完整日历日;month/months3/months6 = 对应月份的月初;custom = 起点那一刻
/// (精确到秒;**缺失/非法回落近 30 天**、早于一年下限的钳到下限当天 00:00)。
/// 逐条对照 `src/utils/usageDates.ts` 的 `rangeStartDate` + `rangeSinceMs`。
pub fn range_since_ms(
    range: UsageRange,
    custom_from: &str,
    now: chrono::DateTime<chrono::Local>,
) -> i64 {
    use chrono::{Datelike, Duration};
    let today = now.date_naive();
    let start_date = match range {
        UsageRange::Today => today,
        UsageRange::Days7 => today - Duration::days(6),
        UsageRange::Days30 => today - Duration::days(29),
        UsageRange::Month => today.with_day(1).unwrap_or(today),
        UsageRange::Months3 => month_start_back(today, 2),
        UsageRange::Months6 => month_start_back(today, 5),
        UsageRange::Custom => match parse_local_datetime(custom_from, Edge::From) {
            // 起始缺失/非法回落近 30 天,不让面板空转
            None => today - Duration::days(29),
            Some(from) => {
                let floor = custom_floor(today).and_time(chrono::NaiveTime::MIN);
                return local_ms(from.max(floor), now.timestamp_millis());
            }
        },
    };
    midnight_ms(start_date, now.timestamp_millis())
}

/// custom range 的窗口上界(**含截止那一整秒** —— 只给日期时即含截止日全天);
/// 其余 range 开区间到现在。
///
/// 上界落到 since 之前时(键盘可造出的 `from > to`,或两端都早于一年下限、
/// since 被钳上去而 until 没动)→ 退成 **since 当天到 23:59:59** 的窗口,
/// 不许倒置空窗静默全零。只给日期时这就是 `usageDates.ts::rangeUntilMs` 的
/// 两条钳位:倒置等效起始日单日查询、过旧抬到一年下限当天。
pub fn range_until_ms(
    range: UsageRange,
    custom_from: &str,
    custom_to: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Option<i64> {
    if range != UsageRange::Custom {
        return None;
    }
    let to = parse_local_datetime(custom_to, Edge::To)?;
    let since = range_since_ms(UsageRange::Custom, custom_from, now);
    let until = local_ms(to, now.timestamp_millis()) + 999;
    if until >= since {
        return Some(until);
    }
    // since 所在日 +1 天的 00:00 - 1ms
    let day = local_datetime(since, now).date();
    Some(midnight_ms(day + chrono::Duration::days(1), now.timestamp_millis()) - 1)
}

/// custom 的补桶窗口(本地时刻)。与查询窗口**同源**:起点走 since、终点走 until;
/// `end < start` 时 `end = start`。**轴反映所选窗口而非数据跨度**。
pub fn custom_chart_window(
    custom_from: &str,
    custom_to: &str,
    now: chrono::DateTime<chrono::Local>,
) -> (chrono::NaiveDateTime, chrono::NaiveDateTime) {
    let start = local_datetime(range_since_ms(UsageRange::Custom, custom_from, now), now);
    let end = match range_until_ms(UsageRange::Custom, custom_from, custom_to, now) {
        None => now.naive_local(),
        Some(ms) => local_datetime(ms, now),
    };
    (start, end.max(start))
}

/// 按小时分桶?「今天」恒是;custom 窗口落在**同一个本地日历日**内也是
/// (只看半天的数据,按日分桶就只剩一根柱子)。其余按日历日。
///
/// 不按「跨度 ≤ 24h」判:后端的小时桶标签只有 `HH:00` 不带日期,跨午夜的
/// 窗口会撞出两个同名桶。
pub fn bucket_hourly(
    range: UsageRange,
    custom_from: &str,
    custom_to: &str,
    now: chrono::DateTime<chrono::Local>,
) -> bool {
    match range {
        UsageRange::Today => true,
        UsageRange::Custom => {
            let (start, end) = custom_chart_window(custom_from, custom_to, now);
            start.date() == end.date()
        }
        _ => false,
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

/// 补齐空桶(`DailyChart.tsx::fillBuckets`)。
///
/// 后端快照是**稀疏的**(只有有数据的桶),无活动时段补 0 才画得出完整时间轴。
/// today 从 00:00 补到当前小时;日粒度补窗口到今天;custom 补所选起止
/// (同日窗口按小时补,见 [`bucket_hourly`])。
pub fn fill_buckets(
    daily: &[DailyStat],
    range: UsageRange,
    custom_from: &str,
    custom_to: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Vec<DailyStat> {
    use chrono::{Datelike, Timelike};
    let map: HashMap<&str, &DailyStat> = daily.iter().map(|d| (d.date.as_str(), d)).collect();
    let empty = |date: String| DailyStat {
        date,
        ..Default::default()
    };
    let mut out = Vec::new();

    if bucket_hourly(range, custom_from, custom_to, now) {
        let (first, last) = match range {
            UsageRange::Custom => {
                let (start, end) = custom_chart_window(custom_from, custom_to, now);
                (start.hour(), end.hour())
            }
            _ => (0, now.hour()),
        };
        for h in first..=last {
            let key = format!("{h:02}:00");
            out.push(match map.get(key.as_str()) {
                Some(d) => (*d).clone(),
                None => empty(key),
            });
        }
        return out;
    }
    if daily.is_empty() {
        return out;
    }
    let (start, end) = match range {
        UsageRange::Custom => {
            let (start, end) = custom_chart_window(custom_from, custom_to, now);
            (start.date(), end.date())
        }
        _ => (
            local_datetime(range_since_ms(range, custom_from, now), now).date(),
            now.date_naive(),
        ),
    };
    let mut cur = start;
    while cur <= end {
        let key = format!("{:04}-{:02}-{:02}", cur.year(), cur.month(), cur.day());
        out.push(match map.get(key.as_str()) {
            Some(d) => (*d).clone(),
            None => empty(key),
        });
        cur += chrono::Duration::days(1);
    }
    out
}

// ─── 趋势图下钻 ──────────────────────────────────────────────
//
// 两级:多日视图点某天 → **下钻**到该日的小时视图(KPI / 趋势图 / 底部明细整体换窗);
// 小时视图(下钻后 / 今天 / 同日自选)点某小时 → **聚焦**,只把底部明细换成该小时。
// 下钻与聚焦都是临时状态,不落盘。

/// 一次账本查询的窗口:起点、上界(含)、是否按小时分桶。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryWindow {
    pub since: i64,
    pub until: Option<i64>,
    pub hourly: bool,
}

/// 主查询(KPI + 趋势图)的窗口。没下钻时就是所选范围;下钻到某天时是
/// **那一天与所选范围的交集**、按小时分桶 —— 自选 `11-01 09:30` 起时下钻 11-01
/// 只看 09:30 之后,与多日视图里那根柱子同口径。
pub fn query_window(
    range: UsageRange,
    custom_from: &str,
    custom_to: &str,
    drill_day: Option<chrono::NaiveDate>,
    now: chrono::DateTime<chrono::Local>,
) -> QueryWindow {
    let since = range_since_ms(range, custom_from, now);
    let until = range_until_ms(range, custom_from, custom_to, now);
    match drill_day {
        None => QueryWindow {
            since,
            until,
            hourly: bucket_hourly(range, custom_from, custom_to, now),
        },
        Some(day) => {
            let fallback = now.timestamp_millis();
            let day_start = midnight_ms(day, fallback);
            let day_end = midnight_ms(day + chrono::Duration::days(1), fallback) - 1;
            QueryWindow {
                since: day_start.max(since),
                until: Some(until.map_or(day_end, |u| u.min(day_end))),
                hourly: true,
            }
        }
    }
}

/// 小时视图对应的本地日历日:下钻日 / 今天 / 同日自选那一天;多日视图 `None`。
pub fn hourly_chart_day(
    range: UsageRange,
    custom_from: &str,
    custom_to: &str,
    drill_day: Option<chrono::NaiveDate>,
    now: chrono::DateTime<chrono::Local>,
) -> Option<chrono::NaiveDate> {
    if drill_day.is_some() {
        return drill_day;
    }
    match range {
        UsageRange::Today => Some(now.date_naive()),
        UsageRange::Custom if bucket_hourly(range, custom_from, custom_to, now) => {
            Some(custom_chart_window(custom_from, custom_to, now).0.date())
        }
        _ => None,
    }
}

/// 聚焦某小时的明细窗口:`[HH:00:00.000, 下一小时 - 1ms]`,再与主查询窗口取交集
/// (同日自选 09:30 起时,09 点那格只含 09:30 之后 —— 与柱子同口径)。
/// 账本按单条请求时间戳判窗,所以这就是该小时那根柱子的精确成员。
pub fn hour_window(
    day: chrono::NaiveDate,
    hour: u32,
    main: QueryWindow,
    now: chrono::DateTime<chrono::Local>,
) -> QueryWindow {
    let fallback = now.timestamp_millis();
    let start = day.and_time(chrono::NaiveTime::MIN) + chrono::Duration::hours(i64::from(hour));
    let start_ms = local_ms(start, fallback);
    let end_ms = local_ms(start + chrono::Duration::hours(1), fallback) - 1;
    QueryWindow {
        since: start_ms.max(main.since),
        until: Some(main.until.map_or(end_ms, |u| u.min(end_ms))),
        hourly: true,
    }
}

/// 下钻视图的补桶:从窗口起点所在小时补到终点所在小时(终点不超过现在 ——
/// 下钻到今天时补到当前小时,与「今天」视图一致)。
pub fn fill_window_hours(
    daily: &[DailyStat],
    window: QueryWindow,
    now: chrono::DateTime<chrono::Local>,
) -> Vec<DailyStat> {
    use chrono::Timelike;
    let map: HashMap<&str, &DailyStat> = daily.iter().map(|d| (d.date.as_str(), d)).collect();
    let start = local_datetime(window.since, now);
    let end_ms = window
        .until
        .unwrap_or(i64::MAX)
        .min(now.timestamp_millis())
        .max(window.since);
    let end = local_datetime(end_ms, now);
    let last = if end.date() == start.date() {
        end.hour()
    } else {
        23
    };
    (start.hour()..=last.max(start.hour()))
        .map(|h| {
            let key = format!("{h:02}:00");
            match map.get(key.as_str()) {
                Some(d) => (*d).clone(),
                None => DailyStat {
                    date: key,
                    ..Default::default()
                },
            }
        })
        .collect()
}

/// 小时桶标签 `HH:00` → 小时(0..=23)。日期桶 / 形态不符返回 `None`。
pub fn parse_hour_label(label: &str) -> Option<u32> {
    let (h, m) = label.split_once(':')?;
    if h.len() != 2 || m != "00" || !h.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    h.parse::<u32>().ok().filter(|h| *h < 24)
}

/// 聚焦小时的展示串:`10:00–11:00`(23 点为 `23:00–24:00`)。
pub fn format_hour_span(hour: u32) -> String {
    format!("{hour:02}:00–{:02}:00", hour + 1)
}

/// 下钻日的展示串:`2026-11-22`。
pub fn format_drill_day(day: chrono::NaiveDate) -> String {
    day.format("%Y-%m-%d").to_string()
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

/// 请求日志「缓存命中率」列的展示串：与 [`cache_hit_rate`] 同一公式（账本里三项已是
/// 互斥口径，Codex 的 `cached_input_tokens` 已从 input 扣出），**整数运算**四舍五入到
/// 0.01%，不经 f64。两端收口：没全命中的请求不进成 `100.00%`（收在 `99.99%`），
/// 有命中的请求不舍成 `0.00%`（抬到 `0.01%`）。分母 0 → `None`。
pub fn format_cache_hit_rate(input: u64, cache_read: u64, cache_write: u64) -> Option<String> {
    let denom = u128::from(input) + u128::from(cache_read) + u128::from(cache_write);
    if denom == 0 {
        return None;
    }
    let read = u128::from(cache_read);
    // 万分点(0.01%)四舍五入:floor(read * 10000 / denom + 0.5)
    let mut bp = (read * 20_000 + denom) / (denom * 2);
    if read < denom {
        bp = bp.min(9_999);
    }
    if read > 0 {
        bp = bp.max(1);
    }
    Some(format!("{}.{:02}%", bp / 100, bp % 100))
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

// ─── 下半区 Tab 与请求日志表格 ───────────────────────────────

/// 趋势图下方的详情页签（默认展示请求日志）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageTab {
    RequestLogs,
    Overview,
}

/// 请求日志表格列（不含首字与状态码）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestLogColumn {
    Time,
    App,
    Provider,
    Model,
    Project,
    Cost,
    NewInput,
    Output,
    CacheRead,
    CacheWrite,
    CacheHitRate,
    Speed,
}

impl RequestLogColumn {
    /// 全部可选列的固定展示顺序。
    pub const ALL: [Self; 12] = [
        Self::Time,
        Self::App,
        Self::Provider,
        Self::Model,
        Self::Project,
        Self::Cost,
        Self::NewInput,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite,
        Self::CacheHitRate,
        Self::Speed,
    ];

    /// 默认可见的 10 列（项目与缓存写可在列菜单中按需勾选）。
    ///
    /// 改默认列集只影响「没自定义过」的用户：自定义过的列集原样落盘、按白名单还原，
    /// 新列要自己在列菜单里勾上。
    pub const DEFAULT: [Self; 10] = [
        Self::Time,
        Self::App,
        Self::Provider,
        Self::Model,
        Self::Cost,
        Self::NewInput,
        Self::Output,
        Self::CacheRead,
        Self::CacheHitRate,
        Self::Speed,
    ];

    /// 稳定标识（持久化 key + 元素 id + i18n 键后段）。
    pub const fn key(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::App => "app",
            Self::Provider => "provider",
            Self::Model => "model",
            Self::Project => "project",
            Self::Cost => "cost",
            Self::NewInput => "newInput",
            Self::Output => "output",
            Self::CacheRead => "cacheRead",
            Self::CacheWrite => "cacheWrite",
            Self::CacheHitRate => "cacheHitRate",
            Self::Speed => "speed",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Time => t("usageStats", "columns.time"),
            Self::App => t("usageStats", "columns.app"),
            Self::Provider => t("usageStats", "columns.provider"),
            Self::Model => t("usageStats", "columns.model"),
            Self::Project => t("usageStats", "columns.project"),
            Self::Cost => t("usageStats", "columns.cost"),
            Self::NewInput => t("usageStats", "columns.newInput"),
            Self::Output => t("usageStats", "columns.output"),
            Self::CacheRead => t("usageStats", "columns.cacheRead"),
            Self::CacheWrite => t("usageStats", "columns.cacheWrite"),
            Self::CacheHitRate => t("usageStats", "columns.cacheHitRate"),
            Self::Speed => t("usageStats", "columns.speed"),
        }
    }
}

/// 读盘列配置 → 有序合法列集（过白名单、按 `ALL` 保序、空或 `None` 回落默认）。
pub fn resolve_log_columns(saved: Option<&[String]>) -> Vec<RequestLogColumn> {
    let Some(keys) = saved else {
        return RequestLogColumn::DEFAULT.to_vec();
    };
    let cols: Vec<RequestLogColumn> = RequestLogColumn::ALL
        .into_iter()
        .filter(|c| keys.iter().any(|k| k == c.key()))
        .collect();
    if cols.is_empty() {
        RequestLogColumn::DEFAULT.to_vec()
    } else {
        cols
    }
}

/// 切换一列的显隐（保持 `ALL` 固定顺序；至少保留 1 列，避免把表格点成空白）。
pub fn toggle_log_column(
    current: &[RequestLogColumn],
    target: RequestLogColumn,
) -> Vec<RequestLogColumn> {
    let present = current.contains(&target);
    if present && current.len() <= 1 {
        return current.to_vec();
    }
    RequestLogColumn::ALL
        .into_iter()
        .filter(|c| {
            if *c == target {
                !present
            } else {
                current.contains(c)
            }
        })
        .collect()
}

/// 落盘列配置：与默认列集完全一致时返回 `None`（不写冗余配置键）。
pub fn serialize_log_columns(current: &[RequestLogColumn]) -> Option<Vec<String>> {
    if current == RequestLogColumn::DEFAULT {
        None
    } else {
        Some(current.iter().map(|c| c.key().to_string()).collect())
    }
}

/// 表格内 token 数格式：`< 10,000` 显示千分位精确值（如 `3,978`），
/// `≥ 10,000` 缩写成 K/M/B（`< 100` 留 1 位小数如 `10.3K`、`69.9K`，`≥ 100` 取整如 `203K`）。
pub fn format_table_tokens(v: u64) -> String {
    if v < 10_000 {
        return format_count(v);
    }
    let f = v as f64;
    for (unit, suffix) in [(1e9, "B"), (1e6, "M"), (1e3, "K")] {
        if f >= unit {
            let scaled = f / unit;
            return if scaled >= 100.0 {
                format!("{}{suffix}", scaled.round() as u64)
            } else {
                format!("{scaled:.1}{suffix}")
            };
        }
    }
    format_count(v)
}

/// 日志估算速度的最低输出门槛（过短回复被首字延迟主导，估算值严重失真）。
pub const SPEED_MIN_OUTPUT_TOKENS: u64 = 200;
const SPEED_MAX_TPS: f64 = 5_000.0;

/// 按估算耗时计算近似生成速度（tok/s）；不足门槛或越界返回 `None`。
pub fn estimate_speed_tps(output_tokens: u64, duration_ms: Option<u64>) -> Option<u64> {
    if output_tokens < SPEED_MIN_OUTPUT_TOKENS {
        return None;
    }
    let ms = duration_ms.filter(|&d| d > 0)?;
    let tps = output_tokens as f64 / (ms as f64 / 1000.0);
    if (1.0..=SPEED_MAX_TPS).contains(&tps) {
        Some(tps.round() as u64)
    } else {
        None
    }
}

/// 估算速度展示串（如 `≈ 143 tok/s`）。
pub fn format_estimated_speed(output_tokens: u64, duration_ms: Option<u64>) -> Option<String> {
    estimate_speed_tps(output_tokens, duration_ms).map(|tps| format!("≈ {tps} tok/s"))
}

/// 判断一批请求日志是否全部落在同一个本地日历日（决定时间列只显示 `HH:MM:SS` 还是带 `MM-DD`）。
pub fn logs_span_single_day(logs: &[mt_usage::RequestLogStat]) -> bool {
    use chrono::TimeZone;
    let mut day: Option<chrono::NaiveDate> = None;
    for item in logs {
        let Some(dt) = chrono::Local
            .timestamp_millis_opt(item.timestamp_ms)
            .single()
        else {
            return false;
        };
        let d = dt.date_naive();
        match day {
            None => day = Some(d),
            Some(prev) if prev != d => return false,
            _ => {}
        }
    }
    true
}

/// 请求时间列展示串：同日 `HH:MM:SS`，跨日 `MM-DD HH:MM:SS`。
pub fn format_request_time(ts_ms: i64, same_day: bool) -> String {
    use chrono::TimeZone;
    let Some(dt) = chrono::Local.timestamp_millis_opt(ts_ms).single() else {
        return "—".to_string();
    };
    if same_day {
        dt.format("%H:%M:%S").to_string()
    } else {
        dt.format("%m-%d %H:%M:%S").to_string()
    }
}

/// 请求完整本地时间（tooltip 用）：`YYYY-MM-DD HH:MM:SS`。
pub fn format_full_timestamp(ts_ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .map_or_else(|| "—".to_string(), |dt| dt.format(DATETIME_FMT).to_string())
}

/// 请求日志「应用」列的简短展示名（专有名词不进字典）。
pub fn agent_display_label(agent: &str) -> &'static str {
    match mt_ai::AgentKind::parse(agent) {
        Some(mt_ai::AgentKind::Claude) => "Claude",
        Some(mt_ai::AgentKind::Codex) => "Codex",
        Some(mt_ai::AgentKind::Grok) => "Grok",
        Some(mt_ai::AgentKind::Agy) => "Antigravity",
        Some(mt_ai::AgentKind::Pi) => "pi",
        Some(mt_ai::AgentKind::Omp) => "oh-my-pi",
        Some(mt_ai::AgentKind::OpenCode) => "OpenCode",
        Some(mt_ai::AgentKind::CodeBuddy) => "CodeBuddy",
        None => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Timelike};

    fn at(y: i32, m: u32, d: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(y, m, d, 15, 30, 0).unwrap()
    }

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

    /// 表格命中率:整数四舍五入到 0.01%,两端收口(没全命中不进成 100%、有命中不舍成 0%)。
    #[test]
    fn 表格缓存命中率两位小数且两端收口() {
        assert_eq!(format_cache_hit_rate(0, 0, 0), None, "分母 0 无值");
        assert_eq!(format_cache_hit_rate(25, 75, 0).as_deref(), Some("75.00%"));
        assert_eq!(format_cache_hit_rate(100, 0, 0).as_deref(), Some("0.00%"));
        assert_eq!(format_cache_hit_rate(0, 500, 0).as_deref(), Some("100.00%"));
        // 缓存写计入分母:1000 / (200 + 1000 + 300)
        assert_eq!(
            format_cache_hit_rate(200, 1_000, 300).as_deref(),
            Some("66.67%")
        );
        // 四舍五入:1/3 → 33.33%,2/3 → 66.67%,1/8 = 12.5% 精确
        assert_eq!(format_cache_hit_rate(2, 1, 0).as_deref(), Some("33.33%"));
        assert_eq!(format_cache_hit_rate(1, 2, 0).as_deref(), Some("66.67%"));
        assert_eq!(format_cache_hit_rate(7, 1, 0).as_deref(), Some("12.50%"));
        // 半数进位:1/16 = 6.25%、1/32 = 3.125% → 3.13%
        assert_eq!(format_cache_hit_rate(31, 1, 0).as_deref(), Some("3.13%"));
        // 99.996% 不进成 100.00%
        assert_eq!(
            format_cache_hit_rate(4, 99_996, 0).as_deref(),
            Some("99.99%")
        );
        // 0.004% 不舍成 0.00%
        assert_eq!(
            format_cache_hit_rate(99_996, 4, 0).as_deref(),
            Some("0.01%")
        );
        // 大数不溢出
        assert_eq!(
            format_cache_hit_rate(u64::MAX, u64::MAX, 0).as_deref(),
            Some("50.00%")
        );
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
        let now = at(2026, 8, 18);

        let today = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Today, "", now))
            .unwrap();
        assert_eq!((today.year(), today.month(), today.day()), (2026, 8, 18));
        assert_eq!((today.hour(), today.minute()), (0, 0), "绝不是滚动 24h");

        let d7 = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Days7, "", now))
            .unwrap();
        assert_eq!(d7.day(), 12, "含今天的 7 个日历日 → 12 日 00:00");

        let month = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Month, "", now))
            .unwrap();
        assert_eq!((month.month(), month.day()), (8, 1));

        let m3 = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Months3, "", now))
            .unwrap();
        assert_eq!((m3.month(), m3.day()), (6, 1));
    }

    /// 跨年回溯:1 月往前 3 个月落到上一年 11 月。
    #[test]
    fn 月份回溯跨年() {
        let now = chrono::Local.with_ymd_and_hms(2026, 1, 15, 9, 0, 0).unwrap();
        let m3 = chrono::Local
            .timestamp_millis_opt(range_since_ms(UsageRange::Months3, "", now))
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

    /// 非 custom 恒无上界(开区间到现在);custom 的上界含截止日全天。
    #[test]
    fn custom_上界含截止日全天() {
        let now = at(2026, 8, 18);
        assert_eq!(range_until_ms(UsageRange::Days30, "", "", now), None);
        assert_eq!(
            range_until_ms(UsageRange::Custom, "2026-08-01", "不是日期", now),
            None,
            "截止日解析不出来 = 无上界"
        );

        let until = range_until_ms(UsageRange::Custom, "2026-08-01", "2026-08-10", now).unwrap();
        let dt = chrono::Local.timestamp_millis_opt(until).unwrap();
        assert_eq!((dt.month(), dt.day()), (8, 10));
        assert_eq!((dt.hour(), dt.minute(), dt.second()), (23, 59, 59));
    }

    /// **一年下限**:起点比 365 天还早时钳到 `today - 364`。
    #[test]
    fn custom_起点钳到一年内() {
        let now = at(2026, 8, 18);
        let since = range_since_ms(UsageRange::Custom, "2020-01-01", now);
        let dt = chrono::Local.timestamp_millis_opt(since).unwrap();
        let floor = now.date_naive() - chrono::Duration::days(364);
        assert_eq!(dt.date_naive(), floor, "过旧的起点钳到一年下限");

        // 两端都早于一年 → 退成下限当日的单日窗口,而不是 since>until 的空窗
        let until = range_until_ms(UsageRange::Custom, "2020-01-01", "2020-02-01", now).unwrap();
        assert!(until > since, "钳位后不许倒置");
        let until_dt = chrono::Local.timestamp_millis_opt(until).unwrap();
        assert_eq!(until_dt.date_naive(), floor);
    }

    /// 倒置区间(键盘可造出的 from > to)把上界抬到起始日 = 单日查询,
    /// **不许静默全零**。
    #[test]
    fn custom_倒置区间退成单日() {
        let now = at(2026, 8, 18);
        let since = range_since_ms(UsageRange::Custom, "2026-08-10", now);
        let until = range_until_ms(UsageRange::Custom, "2026-08-10", "2026-08-01", now).unwrap();
        assert!(until > since, "倒置时不许出现空窗");
        let a = chrono::Local.timestamp_millis_opt(since).unwrap();
        let b = chrono::Local.timestamp_millis_opt(until).unwrap();
        assert_eq!(a.date_naive(), b.date_naive(), "等效单日查询");
        assert_eq!(a.day(), 10);
    }

    /// 起始缺失/非法回落**近 30 天**(不是报错、不是无下界)。
    #[test]
    fn custom_起点非法回落近三十天() {
        let now = at(2026, 8, 18);
        for bad in ["", "2026-8-1", "昨天", "2026-13-40"] {
            let since = range_since_ms(UsageRange::Custom, bad, now);
            assert_eq!(
                since,
                range_since_ms(UsageRange::Days30, "", now),
                "起点 {bad:?} 该回落 days30"
            );
        }
    }

    /// 提交闸门:认得出才更新承诺值(并改写成规范形态),其余保持上一个有效值。
    /// **空串一旦进查询,custom 会静默退化**。
    #[test]
    fn 日期输入闸门只收完整日期() {
        let prev = "2026-07-01 00:00:00";
        assert_eq!(
            accept_datetime_input("2026-08-01", prev, Edge::From),
            "2026-08-01 00:00:00"
        );
        for bad in ["", "2026-8-1", "2026/08/01", "2026-08-011", "abcd-ef-gh"] {
            assert_eq!(
                accept_datetime_input(bad, prev, Edge::From),
                prev,
                "{bad:?} 该回弹"
            );
        }
        // 形态对但日子不存在的也要拦(2 月 30 日)
        assert_eq!(accept_datetime_input("2026-02-30", prev, Edge::From), prev);
    }

    /// 时分秒:三种形态都收,统一改写成 `YYYY-MM-DD HH:MM:SS`;
    /// 只给日期时截止端补 23:59:59(含截止日全天)。
    #[test]
    fn 时分秒输入改写成规范形态() {
        let prev = "2026-07-01 00:00:00";
        let cases = [
            ("2026-08-01", Edge::To, "2026-08-01 23:59:59"),
            ("2026-08-01 08:05", Edge::From, "2026-08-01 08:05:00"),
            ("2026-08-01 08:05", Edge::To, "2026-08-01 08:05:00"),
            ("2026-08-01 08:05:09", Edge::To, "2026-08-01 08:05:09"),
            ("2026-08-01T08:05:09", Edge::From, "2026-08-01 08:05:09"),
            (" 2026-08-01 23:59:59 ", Edge::From, "2026-08-01 23:59:59"),
        ];
        for (input, edge, want) in cases {
            assert_eq!(accept_datetime_input(input, prev, edge), want, "{input:?}");
        }
        for bad in [
            "2026-08-01 9:05",
            "2026-08-01 24:00",
            "2026-08-01 12:60",
            "2026-08-01 12:00:60",
            "2026-08-01 12",
            "2026-08-01 12:00:00:00",
            "2026-08-01T",
            "2026-08-01 ab:cd",
        ] {
            assert_eq!(
                accept_datetime_input(bad, prev, Edge::To),
                prev,
                "{bad:?} 该回弹"
            );
        }
    }

    /// 缺省窗口与 days30 同一个:29 天前 00:00:00 起、今天 23:59:59 止。
    #[test]
    fn custom_缺省值补齐时刻() {
        let now = at(2026, 8, 18);
        assert_eq!(default_custom_edge(Edge::From, now), "2026-07-20 00:00:00");
        assert_eq!(default_custom_edge(Edge::To, now), "2026-08-18 23:59:59");
    }

    /// 带时刻的起止精确到秒:起点那一刻起,含截止那一整秒。
    #[test]
    fn custom_窗口精确到秒() {
        let now = at(2026, 8, 18);
        let since = range_since_ms(UsageRange::Custom, "2026-08-10 09:30:15", now);
        let until = range_until_ms(
            UsageRange::Custom,
            "2026-08-10 09:30:15",
            "2026-08-10 18:00:00",
            now,
        )
        .unwrap();
        let a = chrono::Local.timestamp_millis_opt(since).unwrap();
        let b = chrono::Local.timestamp_millis_opt(until).unwrap();
        assert_eq!((a.day(), a.hour(), a.minute(), a.second()), (10, 9, 30, 15));
        assert_eq!(a.timestamp_subsec_millis(), 0);
        assert_eq!((b.day(), b.hour(), b.minute(), b.second()), (10, 18, 0, 0));
        assert_eq!(b.timestamp_subsec_millis(), 999, "含截止那一整秒");
    }

    /// 同日内时刻倒置(14:00 → 10:00)同样退成「起点到当天 23:59:59」,不许空窗。
    #[test]
    fn custom_时刻倒置退成起点当天() {
        let now = at(2026, 8, 18);
        let from = "2026-08-10 14:00:00";
        let since = range_since_ms(UsageRange::Custom, from, now);
        let until = range_until_ms(UsageRange::Custom, from, "2026-08-10 10:00:00", now).unwrap();
        assert!(until > since);
        let b = chrono::Local.timestamp_millis_opt(until).unwrap();
        assert_eq!(
            (b.day(), b.hour(), b.minute(), b.second()),
            (10, 23, 59, 59)
        );
    }

    /// custom 图表窗口与查询窗口**同源**:起点走 since、终点走 until。
    #[test]
    fn custom_图表窗口与查询窗口同源() {
        let now = at(2026, 8, 18);
        let (start, end) = custom_chart_window("2026-08-01", "2026-08-10", now);
        assert_eq!(
            start.date(),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()
        );
        assert_eq!(
            end.date(),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap()
        );

        // 截止缺失 → 无上界 → 补到今天
        let (_, end) = custom_chart_window("2026-08-01", "", now);
        assert_eq!(end.date(), now.date_naive());

        // 倒置 → end 抬到起点当天(轴不许倒着长)
        let (start, end) = custom_chart_window("2026-08-10", "2026-08-01", now);
        assert_eq!(start.date(), end.date());

        // 过旧 → 与查询同步钳到一年下限
        let (start, _) = custom_chart_window("2020-01-01", "2020-02-01", now);
        assert_eq!(start.date(), now.date_naive() - chrono::Duration::days(364));
    }

    /// 分桶粒度:同一本地日历日内的 custom 按小时,跨午夜一律按日
    /// (小时桶标签只有 `HH:00`,跨日会撞名)。
    #[test]
    fn custom_同日窗口按小时分桶() {
        let now = at(2026, 8, 18);
        assert!(bucket_hourly(UsageRange::Today, "", "", now));
        assert!(!bucket_hourly(UsageRange::Days7, "", "", now));
        let custom = |from, to| bucket_hourly(UsageRange::Custom, from, to, now);
        assert!(custom("2026-08-10 09:00:00", "2026-08-10 18:00:00"));
        assert!(
            custom("2026-08-10", "2026-08-10"),
            "单日 = 当天 00:00~23:59:59"
        );
        assert!(
            !custom("2026-08-10 22:00:00", "2026-08-11 02:00:00"),
            "跨午夜按日"
        );
        assert!(!custom("2026-08-01", "2026-08-10"));
    }

    /// 补空桶:后端快照稀疏,不补就画不出完整时间轴。
    #[test]
    fn 趋势图补空桶() {
        let now = at(2026, 8, 18);
        let daily = vec![DailyStat {
            date: "2026-08-16".into(),
            cost: 1.0,
            calls: 3,
            ..Default::default()
        }];
        // days7 补到今天
        let filled = fill_buckets(&daily, UsageRange::Days7, "", "", now);
        assert_eq!(filled.len(), 7, "含今天的 7 个日历日");
        assert_eq!(filled.first().unwrap().date, "2026-08-12");
        assert_eq!(filled.last().unwrap().date, "2026-08-18");
        assert_eq!(filled[4].cost, 1.0, "有数据的那天原样保留");
        assert_eq!(filled[0].calls, 0, "空桶补 0");

        // custom 按所选窗口补(轴反映所选窗口而非数据跨度)
        let filled = fill_buckets(&daily, UsageRange::Custom, "2026-08-15", "2026-08-17", now);
        assert_eq!(filled.len(), 3);
        assert_eq!(filled.first().unwrap().date, "2026-08-15");
        assert_eq!(filled.last().unwrap().date, "2026-08-17");

        // 同日 custom 按小时补:起点所在小时到截止所在小时
        let hourly = vec![DailyStat {
            date: "10:00".into(),
            cost: 2.0,
            ..Default::default()
        }];
        let filled = fill_buckets(
            &hourly,
            UsageRange::Custom,
            "2026-08-16 09:30:00",
            "2026-08-16 12:10:00",
            now,
        );
        let keys: Vec<&str> = filled.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(keys, ["09:00", "10:00", "11:00", "12:00"]);
        assert_eq!(filled[1].cost, 2.0);

        // today 从 00:00 补到当前小时
        let filled = fill_buckets(&daily, UsageRange::Today, "", "", now);
        assert_eq!(filled.len(), 16, "0..=15 点");
        assert_eq!(filled.first().unwrap().date, "00:00");
        assert_eq!(filled.last().unwrap().date, "15:00");

        // 完全没数据时不补(原版 `daily.length === 0` 直接返回空)
        assert!(fill_buckets(&[], UsageRange::Days7, "", "", now).is_empty());
    }

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(y, m, d, h, min, s)
            .unwrap()
            .timestamp_millis()
    }

    fn day(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// 下钻到某天:窗口是那一整天、按小时分桶;没下钻时与原查询参数一字不差。
    #[test]
    fn 下钻窗口是那一整天且按小时() {
        let now = at(2026, 8, 18);
        let plain = query_window(UsageRange::Days30, "", "", None, now);
        assert_eq!(plain.since, range_since_ms(UsageRange::Days30, "", now));
        assert_eq!(plain.until, None);
        assert!(!plain.hourly);

        let w = query_window(UsageRange::Days30, "", "", Some(day(2026, 8, 10)), now);
        assert!(w.hourly);
        assert_eq!(w.since, local(2026, 8, 10, 0, 0, 0));
        assert_eq!(
            w.until,
            Some(local(2026, 8, 11, 0, 0, 0) - 1),
            "含当天最后 1ms"
        );
    }

    /// 下钻窗口与自选范围取交集:柱子只算 09:30 之后,下钻后也只能看 09:30 之后。
    #[test]
    fn 下钻窗口与自选范围取交集() {
        let now = at(2026, 8, 18);
        let (from, to) = ("2026-08-10 09:30:00", "2026-08-12 18:00:00");
        let first = query_window(UsageRange::Custom, from, to, Some(day(2026, 8, 10)), now);
        assert_eq!(first.since, local(2026, 8, 10, 9, 30, 0));
        assert_eq!(first.until, Some(local(2026, 8, 11, 0, 0, 0) - 1));
        let last = query_window(UsageRange::Custom, from, to, Some(day(2026, 8, 12)), now);
        assert_eq!(last.since, local(2026, 8, 12, 0, 0, 0));
        assert_eq!(last.until, Some(local(2026, 8, 12, 18, 0, 0) + 999));
    }

    /// 小时视图对应哪一天:下钻日 > 今天 > 同日自选;多日视图没有。
    #[test]
    fn 小时视图对应的日历日() {
        let now = at(2026, 8, 18);
        let d = day(2026, 8, 10);
        assert_eq!(
            hourly_chart_day(UsageRange::Days7, "", "", Some(d), now),
            Some(d)
        );
        assert_eq!(hourly_chart_day(UsageRange::Days7, "", "", None, now), None);
        assert_eq!(
            hourly_chart_day(UsageRange::Today, "", "", None, now),
            Some(now.date_naive())
        );
        assert_eq!(
            hourly_chart_day(
                UsageRange::Custom,
                "2026-08-12 09:00:00",
                "2026-08-12 18:00:00",
                None,
                now
            ),
            Some(day(2026, 8, 12))
        );
        assert_eq!(
            hourly_chart_day(UsageRange::Custom, "2026-08-01", "2026-08-10", None, now),
            None,
            "跨日自选是多日视图"
        );
    }

    /// 聚焦小时 = 该小时整段(含最后 1ms),再与主窗口取交集。
    #[test]
    fn 聚焦小时窗口精确到毫秒并与主窗口取交集() {
        let now = at(2026, 8, 18);
        let main = query_window(UsageRange::Days30, "", "", Some(day(2026, 8, 10)), now);
        let w = hour_window(day(2026, 8, 10), 10, main, now);
        assert_eq!(w.since, local(2026, 8, 10, 10, 0, 0));
        assert_eq!(w.until, Some(local(2026, 8, 10, 11, 0, 0) - 1));

        // 23 点跨到次日 00:00 前 1ms
        let w = hour_window(day(2026, 8, 10), 23, main, now);
        assert_eq!(w.until, Some(local(2026, 8, 11, 0, 0, 0) - 1));

        // 同日自选 09:30 起:09 点那格从 09:30 起算
        let custom = query_window(
            UsageRange::Custom,
            "2026-08-12 09:30:00",
            "2026-08-12 18:00:00",
            None,
            now,
        );
        let w = hour_window(day(2026, 8, 12), 9, custom, now);
        assert_eq!(w.since, local(2026, 8, 12, 9, 30, 0));
        assert_eq!(w.until, Some(local(2026, 8, 12, 10, 0, 0) - 1));

        // 今天视图(无上界):上界就是该小时末
        let today = query_window(UsageRange::Today, "", "", None, now);
        let w = hour_window(now.date_naive(), 15, today, now);
        assert_eq!(w.until, Some(local(2026, 8, 18, 16, 0, 0) - 1));
    }

    /// 下钻视图补桶:过去的日子补满 24 小时;今天补到当前小时;自选交集从起点小时补。
    #[test]
    fn 下钻视图按窗口补小时桶() {
        let now = at(2026, 8, 18); // 15:30
        let hourly = vec![DailyStat {
            date: "10:00".into(),
            cost: 2.0,
            ..Default::default()
        }];
        let past = query_window(UsageRange::Days30, "", "", Some(day(2026, 8, 10)), now);
        let filled = fill_window_hours(&hourly, past, now);
        assert_eq!(filled.len(), 24);
        assert_eq!(filled.first().unwrap().date, "00:00");
        assert_eq!(filled.last().unwrap().date, "23:00");
        assert_eq!(filled[10].cost, 2.0);

        let today = query_window(UsageRange::Days30, "", "", Some(now.date_naive()), now);
        let filled = fill_window_hours(&hourly, today, now);
        assert_eq!(filled.len(), 16, "0..=15 点");

        let custom = query_window(
            UsageRange::Custom,
            "2026-08-10 09:30:00",
            "2026-08-12 18:00:00",
            Some(day(2026, 8, 10)),
            now,
        );
        let keys: Vec<String> = fill_window_hours(&hourly, custom, now)
            .into_iter()
            .map(|d| d.date)
            .collect();
        assert_eq!(keys.first().map(String::as_str), Some("09:00"));
        assert_eq!(keys.last().map(String::as_str), Some("23:00"));
    }

    #[test]
    fn 小时桶标签解析与区间展示() {
        assert_eq!(parse_hour_label("00:00"), Some(0));
        assert_eq!(parse_hour_label("23:00"), Some(23));
        assert_eq!(parse_hour_label("24:00"), None);
        assert_eq!(parse_hour_label("2026-08-10"), None, "日期桶不是小时");
        assert_eq!(parse_hour_label("9:00"), None);
        assert_eq!(parse_hour_label("09:30"), None);
        assert_eq!(format_hour_span(9), "09:00–10:00");
        assert_eq!(format_hour_span(23), "23:00–24:00");
        assert_eq!(format_drill_day(day(2026, 11, 2)), "2026-11-02");
    }

    #[test]
    fn 表格_token_格式小值千分位大值缩写() {
        assert_eq!(format_table_tokens(0), "0");
        assert_eq!(format_table_tokens(904), "904");
        assert_eq!(format_table_tokens(3_978), "3,978");
        assert_eq!(format_table_tokens(9_999), "9,999");
        assert_eq!(format_table_tokens(10_300), "10.3K");
        assert_eq!(format_table_tokens(69_900), "69.9K");
        assert_eq!(format_table_tokens(111_000), "111K");
        assert_eq!(format_table_tokens(203_000), "203K");
        assert_eq!(format_table_tokens(1_520_000), "1.5M");
        assert_eq!(format_table_tokens(125_000_000), "125M");
    }

    #[test]
    fn 估算速度卡最低输出门槛与合法上界() {
        assert_eq!(
            estimate_speed_tps(199, Some(2_000)),
            None,
            "低于 200 输出不估算"
        );
        assert_eq!(estimate_speed_tps(200, None), None);
        assert_eq!(estimate_speed_tps(200, Some(0)), None);
        assert_eq!(estimate_speed_tps(1_430, Some(10_000)), Some(143));
        assert_eq!(
            format_estimated_speed(1_430, Some(10_000)).as_deref(),
            Some("≈ 143 tok/s")
        );
        // 极端超高速度直接过滤
        assert_eq!(estimate_speed_tps(50_000, Some(1_000)), None);
    }

    #[test]
    fn 请求日志列配置过白名单且至少保留一列() {
        assert_eq!(resolve_log_columns(None), RequestLogColumn::DEFAULT);
        assert_eq!(
            resolve_log_columns(Some(&["bogus".into()])),
            RequestLogColumn::DEFAULT,
            "全是非法键时回落默认"
        );
        // 逆序存盘也要按固定表序还原
        let resolved =
            resolve_log_columns(Some(&["speed".into(), "time".into(), "project".into()]));
        assert_eq!(
            resolved,
            vec![
                RequestLogColumn::Time,
                RequestLogColumn::Project,
                RequestLogColumn::Speed
            ]
        );
        // 切换只剩最后一列时拒绝移除
        let single = vec![RequestLogColumn::Time];
        assert_eq!(toggle_log_column(&single, RequestLogColumn::Time), single);
        // 默认列集不落盘（None）
        assert_eq!(serialize_log_columns(&RequestLogColumn::DEFAULT), None);
        let toggled = toggle_log_column(&RequestLogColumn::DEFAULT, RequestLogColumn::Project);
        assert!(toggled.contains(&RequestLogColumn::Project));
        assert!(serialize_log_columns(&toggled).is_some());
    }
}
