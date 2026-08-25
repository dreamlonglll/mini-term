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

    /// 「今天」按小时分桶,其余按日历日。
    pub fn hourly(self) -> bool {
        self == Self::Today
    }
}

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

/// custom 起止日期输入的**提交闸门**(`usageDates.ts::acceptDateInput`)。
///
/// 只有完整 `YYYY-MM-DD` 才更新承诺值,其余输入保持上一个有效值(受控输入随之回弹)。
/// **空串一旦进查询,custom 会静默退化成无上界 / 近 30 天**,展示远大于所选
/// 范围的数据 —— 这道闸是硬要求。
pub fn accept_date_input(next: &str, prev: &str) -> String {
    if parse_local_date(next).is_some() {
        next.to_string()
    } else {
        prev.to_string()
    }
}

/// custom 起点的最早允许日(近一年)。原版 `<input type="date">` 的 `min`
/// 只标 `:invalid`、**不拦截键入**,钳位靠 since/until 两条规则兜住。
pub(super) fn custom_floor(today: chrono::NaiveDate) -> chrono::NaiveDate {
    today - chrono::Duration::days(364)
}

/// n 天前的本地日历日,`"YYYY-MM-DD"`(custom 起止的缺省值用)。
pub fn local_date_str(days_back: i64, now: chrono::DateTime<chrono::Local>) -> String {
    (now.date_naive() - chrono::Duration::days(days_back))
        .format("%Y-%m-%d")
        .to_string()
}

/// 本地日历日 → 当天 00:00 的 epoch ms。DST 那天可能没有 00:00,取当天最早的合法时刻。
fn midnight_ms(date: chrono::NaiveDate, fallback: i64) -> i64 {
    use chrono::TimeZone;
    let naive = date.and_hms_opt(0, 0, 0).unwrap_or_default();
    match chrono::Local.from_local_datetime(&naive).earliest() {
        Some(dt) => dt.timestamp_millis(),
        None => fallback,
    }
}

/// 范围 → 窗口起点(epoch ms)。
///
/// **本地日历日口径**:today = 本地 00:00 起(绝不用滚动 24h);days7/30 = 含今天的
/// 完整日历日;month/months3/months6 = 对应月份的月初;custom = 起始日本地 00:00
/// (**缺失/非法回落近 30 天**、过旧钳到一年内)。逐条对照
/// `src/utils/usageDates.ts` 的 `rangeStartDate` + `rangeSinceMs`。
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
        UsageRange::Custom => match parse_local_date(custom_from) {
            // 起始缺失/非法回落近 30 天,不让面板空转
            None => today - Duration::days(29),
            Some(from) => from.max(custom_floor(today)),
        },
    };
    midnight_ms(start_date, now.timestamp_millis())
}

/// custom range 的窗口上界(**含截止日全天**);其余 range 开区间到现在。
///
/// 两条钳位照抄 `usageDates.ts::rangeUntilMs`:
/// - `from > to`(键盘可造出的倒置区间)→ 把上界抬到 `from`,等效单日查询,
///   不许静默全零;
/// - `day < floor` → 抬到一年下限,免得 since 被抬、until 不动产生倒置空窗。
pub fn range_until_ms(
    range: UsageRange,
    custom_from: &str,
    custom_to: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Option<i64> {
    if range != UsageRange::Custom {
        return None;
    }
    let to = parse_local_date(custom_to)?;
    let from = parse_local_date(custom_from);
    let mut day = match from {
        Some(f) if f > to => f,
        _ => to,
    };
    let floor = custom_floor(now.date_naive());
    if day < floor {
        day = floor;
    }
    // 该日 +1 天的 00:00 - 1ms
    Some(midnight_ms(day + chrono::Duration::days(1), now.timestamp_millis()) - 1)
}

/// custom 趋势图的补桶窗口。与查询窗口**同源**:起点走 since、终点走 until 的
/// 日历日;`end < start` 时 `end = start`。**轴反映所选窗口而非数据跨度**。
pub fn custom_chart_window(
    custom_from: &str,
    custom_to: &str,
    now: chrono::DateTime<chrono::Local>,
) -> (chrono::NaiveDate, chrono::NaiveDate) {
    use chrono::TimeZone;
    let since = range_since_ms(UsageRange::Custom, custom_from, now);
    let start = chrono::Local
        .timestamp_millis_opt(since)
        .single()
        .map(|d| d.date_naive())
        .unwrap_or_else(|| now.date_naive());
    let end = match range_until_ms(UsageRange::Custom, custom_from, custom_to, now) {
        None => now.date_naive(),
        Some(ms) => chrono::Local
            .timestamp_millis_opt(ms)
            .single()
            .map(|d| d.date_naive())
            .unwrap_or_else(|| now.date_naive()),
    };
    (start, end.max(start))
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
/// today 从 00:00 补到当前小时;日粒度补窗口到今天;custom 补所选起止。
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

    if range == UsageRange::Today {
        for h in 0..=now.hour() {
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
        UsageRange::Custom => custom_chart_window(custom_from, custom_to, now),
        _ => {
            use chrono::TimeZone;
            let since = range_since_ms(range, custom_from, now);
            let start = chrono::Local
                .timestamp_millis_opt(since)
                .single()
                .map(|d| d.date_naive())
                .unwrap_or_else(|| now.date_naive());
            (start, now.date_naive())
        }
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

/// cwd ↔ 登记项目路径的匹配归一(`UsageStatsModal.tsx:295`,注释说明「对齐后端
/// normalize」):正斜杠转反斜杠 → 小写 → 去尾部反斜杠。
pub fn norm_project_path(p: &str) -> String {
    let s = p.replace('/', "\\").to_lowercase();
    s.trim_end_matches('\\').to_string()
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

    /// 提交闸门:只有完整 `YYYY-MM-DD` 才更新承诺值,其余保持上一个有效值。
    /// **空串一旦进查询,custom 会静默退化**。
    #[test]
    fn 日期输入闸门只收完整日期() {
        assert_eq!(accept_date_input("2026-08-01", "2026-07-01"), "2026-08-01");
        for bad in ["", "2026-8-1", "2026/08/01", "2026-08-011", "abcd-ef-gh"] {
            assert_eq!(
                accept_date_input(bad, "2026-07-01"),
                "2026-07-01",
                "{bad:?} 该回弹"
            );
        }
        // 形态对但日子不存在的也要拦(2 月 30 日)
        assert_eq!(accept_date_input("2026-02-30", "2026-07-01"), "2026-07-01");
    }

    /// custom 图表窗口与查询窗口**同源**:起点走 since、终点走 until 的日历日。
    #[test]
    fn custom_图表窗口与查询窗口同源() {
        let now = at(2026, 8, 18);
        let (start, end) = custom_chart_window("2026-08-01", "2026-08-10", now);
        assert_eq!(start, chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap());
        assert_eq!(end, chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap());

        // 截止缺失 → 无上界 → 补到今天
        let (_, end) = custom_chart_window("2026-08-01", "", now);
        assert_eq!(end, now.date_naive());

        // 倒置 → end 抬到 start(轴不许倒着长)
        let (start, end) = custom_chart_window("2026-08-10", "2026-08-01", now);
        assert_eq!(start, end);

        // 过旧 → 与查询同步钳到一年下限
        let (start, _) = custom_chart_window("2020-01-01", "2020-02-01", now);
        assert_eq!(start, now.date_naive() - chrono::Duration::days(364));
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

        // today 从 00:00 补到当前小时
        let filled = fill_buckets(&daily, UsageRange::Today, "", "", now);
        assert_eq!(filled.len(), 16, "0..=15 点");
        assert_eq!(filled.first().unwrap().date, "00:00");
        assert_eq!(filled.last().unwrap().date, "15:00");

        // 完全没数据时不补(原版 `daily.length === 0` 直接返回空)
        assert!(fill_buckets(&[], UsageRange::Days7, "", "", now).is_empty());
    }

    /// 项目路径归一(大小写 / 分隔符 / 尾斜杠容错,对齐后端 normalize)。
    #[test]
    fn 项目路径归一() {
        assert_eq!(norm_project_path("D:/Git/X/"), "d:\\git\\x");
        assert_eq!(norm_project_path("D:\\Git\\X"), "d:\\git\\x");
        assert_eq!(norm_project_path("D:\\Git\\X\\\\"), "d:\\git\\x");
        assert_eq!(norm_project_path(""), "");
    }
}
