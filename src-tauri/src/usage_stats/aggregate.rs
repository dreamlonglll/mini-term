use serde::Serialize;
use std::collections::{HashMap, HashSet};

use super::pricing::{cost_of, PricingTable};
use super::turns::{civil_from_days, ParsedSession, UsageTotals};

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DailyStat {
    /// 本地日历日 "YYYY-MM-DD"（「今天」视图为小时 "HH:00"）
    pub date: String,
    pub cost: f64,
    pub calls: u64,
    /// hover 详情用的 token 明细
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStat {
    /// JSONL 内 cwd 真实路径（目录名编码有损，不可反解）
    pub path: String,
    pub name: String,
    pub cost: f64,
    pub sessions: u64,
    pub calls: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TopSessionStat {
    pub session_id: String,
    pub agent: String,
    pub project_path: String,
    pub project_name: String,
    pub title: String,
    /// 会话内窗内首 turn 时间（ISO 8601，展示用）
    pub timestamp: String,
    pub cost: f64,
    pub calls: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelStat {
    /// 归一后的模型名（剥日期/provider 前缀）；空串 = 未知模型，前端翻译展示
    pub model: String,
    pub cost: f64,
    pub calls: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStat {
    /// 供应商展示名（baseurl 的 host；由扫描层按会话来源解析）
    pub provider: String,
    pub cost: f64,
    pub calls: u64,
    pub tokens: u64,
    pub sessions: u64,
}

/// 计数排行条目（工具/Shell/MCP，设计 §2.2）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CountStat {
    pub name: String,
    pub count: u64,
}

/// 聚合快照（serde camelCase，`src/types.ts` 手写镜像）。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UsageStatsPayload {
    pub total_cost: f64,
    pub total_calls: u64,
    pub session_count: u64,
    pub input_tokens: u64,
    /// 展示口径：模型产出全部 token（含 reasoning）
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub daily: Vec<DailyStat>,
    pub by_project: Vec<ProjectStat>,
    /// 全量输出（模型数天然有限），前 N + Others 的切分交给前端
    pub by_model: Vec<ModelStat>,
    /// 全量输出（供应商数天然有限）
    pub by_provider: Vec<ProviderStat>,
    pub top_sessions: Vec<TopSessionStat>,
    /// 工具/Shell 首词/MCP server 计数排行，各前 10（设计 §2.2）
    pub by_tool: Vec<CountStat>,
    pub by_shell: Vec<CountStat>,
    pub by_mcp: Vec<CountStat>,
}

/// 项目排行上限：设计合同(2026-08-01 §2.2)定为前 8
const TOP_PROJECTS: usize = 8;
const TOP_SESSIONS: usize = 10;
/// 工具/Shell/MCP 排行上限：设计合同 §2.2「各前 10」
const TOP_TOOLS: usize = 10;

#[derive(Default)]
struct BucketAcc {
    cost: f64,
    calls: u64,
    tokens: u64,
    sessions: u64,
    // token 明细（仅时段桶消费，供 hover 详情）
    input: u64,
    output: u64,
    cache_read: u64,
}

struct SessionAcc {
    session_id: String,
    agent: &'static str,
    cwd: String,
    title: String,
    first_ts_ms: i64,
    cost: f64,
    calls: u64,
    tokens: u64,
}

/// 跨文件聚合器。正确性口径（docs/plans/2026-08-01-usage-stats-design.md §6）：
/// message_id 跨文件去重 → 逐 turn 按自身 timestamp 终判（缺失回退文件 mtime）→
/// 本地日历日/小时分桶 → 全部被去重/窗外的会话不计 session_count。
pub struct Aggregator {
    since_ms: i64,
    /// 窗口上界(custom range 的「止」,含端点);None = 开区间到现在
    until_ms: Option<i64>,
    tz_offset_minutes: i32,
    /// IANA 时区(前端 Intl 提供):分桶按每条记录自身时刻求当地偏移,
    /// DST 地区历史记录不错日;None(名字缺失/解析失败)回落固定偏移
    tz: Option<chrono_tz::Tz>,
    /// true = 按本地小时分桶（「今天」视图），false = 按本地日历日
    hourly: bool,
    seen_ids: HashSet<String>,
    /// 工具事件的跨文件去重集（Claude 按 message_id 派生键；Codex 无键不参与）
    seen_tool_keys: HashSet<String>,
    totals: UsageTotals,
    total_cost: f64,
    total_calls: u64,
    session_count: u64,
    daily: HashMap<i64, BucketAcc>,
    projects: HashMap<String, BucketAcc>,
    models: HashMap<String, BucketAcc>,
    providers: HashMap<String, BucketAcc>,
    sessions: Vec<SessionAcc>,
    tools: HashMap<String, u64>,
    shells: HashMap<String, u64>,
    mcps: HashMap<String, u64>,
}

fn project_name(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

/// 该 UTC 毫秒时刻在 tz 的偏移，返回 JS getTimezoneOffset 语义(UTC−local 分钟)。
fn tz_offset_minutes_at(tz: &chrono_tz::Tz, ts_ms: i64) -> i32 {
    use chrono::{Offset, TimeZone};
    match tz.timestamp_millis_opt(ts_ms) {
        chrono::LocalResult::Single(dt) => -(dt.offset().fix().local_minus_utc() / 60),
        // UTC instant → 当地时间无歧义,此分支理论不可达;兜底 0(UTC)
        _ => 0,
    }
}

impl Aggregator {
    pub fn new(
        since_ms: i64,
        until_ms: Option<i64>,
        tz_offset_minutes: i32,
        tz_name: Option<&str>,
        hourly: bool,
    ) -> Self {
        Self {
            since_ms,
            until_ms,
            tz_offset_minutes,
            tz: tz_name.and_then(|n| n.parse().ok()),
            hourly,
            seen_ids: HashSet::new(),
            seen_tool_keys: HashSet::new(),
            totals: UsageTotals::default(),
            total_cost: 0.0,
            total_calls: 0,
            session_count: 0,
            daily: HashMap::new(),
            projects: HashMap::new(),
            models: HashMap::new(),
            providers: HashMap::new(),
            sessions: Vec::new(),
            tools: HashMap::new(),
            shells: HashMap::new(),
            mcps: HashMap::new(),
        }
    }

    /// 按模型分桶的 key：canonical + 剥日期后缀，带日期版本归入同一模型。
    fn model_key(model: Option<&str>) -> String {
        let Some(m) = model else {
            return String::new();
        };
        let c = super::pricing::canonical(m);
        match super::pricing::strip_date_suffix(&c) {
            Some(stripped) => stripped.to_string(),
            None => c,
        }
    }

    pub fn add_session(&mut self, s: &ParsedSession, pricing: &PricingTable) {
        let mut acc = SessionAcc {
            session_id: s.session_id.clone(),
            agent: s.agent,
            cwd: s.cwd.clone().unwrap_or_default(),
            title: s.title.clone().unwrap_or_default(),
            first_ts_ms: i64::MAX,
            cost: 0.0,
            calls: 0,
            tokens: 0,
        };

        for turn in &s.turns {
            // 先去重再判窗：同 id 消息的 timestamp 一致，不会误吞窗内副本
            if let Some(id) = &turn.message_id {
                if !self.seen_ids.insert(id.clone()) {
                    continue;
                }
            }
            let ts = turn.timestamp_ms.unwrap_or(s.mtime_ms);
            if ts < self.since_ms {
                continue;
            }
            if self.until_ms.is_some_and(|u| ts > u) {
                continue;
            }

            let cost = turn
                .model
                .as_deref()
                .and_then(|m| pricing.resolve(m))
                .map(|p| cost_of(&turn.usage, &p))
                .unwrap_or(0.0);
            let tokens = turn.usage.total();

            self.totals.add(&turn.usage);
            self.total_cost += cost;
            self.total_calls += 1;

            // 时段分桶：本地时区(getTimezoneOffset 语义为 UTC−local，故 local = ts − offset)。
            // 有 IANA 时区时按该记录自身时刻求偏移(DST 边界两侧各用各的)，
            // 「今天」视图按小时，其余按日历日
            let offset_minutes = match &self.tz {
                Some(tz) => tz_offset_minutes_at(tz, ts),
                None => self.tz_offset_minutes,
            };
            let local_ms = ts - offset_minutes as i64 * 60_000;
            let bucket = local_ms.div_euclid(if self.hourly { 3_600_000 } else { 86_400_000 });
            let d = self.daily.entry(bucket).or_default();
            d.cost += cost;
            d.calls += 1;
            d.input += turn.usage.input;
            d.output += turn.usage.output + turn.usage.reasoning;
            d.cache_read += turn.usage.cache_read;

            let m = self.models.entry(Self::model_key(turn.model.as_deref())).or_default();
            m.cost += cost;
            m.calls += 1;
            m.tokens += tokens;

            acc.cost += cost;
            acc.calls += 1;
            acc.tokens += tokens;
            acc.first_ts_ms = acc.first_ts_ms.min(ts);
        }

        // 工具/Shell/MCP 计数：与 turn 同口径的去重(有键先去重)与窗口终判;
        // 不影响 session_count(纯工具活动不算计费会话)
        for u in &s.tool_uses {
            if let Some(key) = &u.dedup_key {
                if !self.seen_tool_keys.insert(key.clone()) {
                    continue;
                }
            }
            let ts = u.timestamp_ms.unwrap_or(s.mtime_ms);
            if ts < self.since_ms || self.until_ms.is_some_and(|until| ts > until) {
                continue;
            }
            let bucket = match u.kind {
                "shell" => &mut self.shells,
                "mcp" => &mut self.mcps,
                _ => &mut self.tools,
            };
            *bucket.entry(u.name.clone()).or_default() += 1;
        }

        // 全部被去重/窗外的会话不计入
        if acc.calls == 0 {
            return;
        }
        self.session_count += 1;
        let p = self.projects.entry(acc.cwd.clone()).or_default();
        p.cost += acc.cost;
        p.calls += acc.calls;
        p.tokens += acc.tokens;
        p.sessions += 1;
        // 供应商为会话级属性（扫描层已解析成展示 host）
        let pv = self
            .providers
            .entry(s.provider.clone().unwrap_or_default())
            .or_default();
        pv.cost += acc.cost;
        pv.calls += acc.calls;
        pv.tokens += acc.tokens;
        pv.sessions += 1;
        self.sessions.push(acc);
    }

    pub fn snapshot(&self) -> UsageStatsPayload {
        let mut daily: Vec<(i64, &BucketAcc)> = self.daily.iter().map(|(k, v)| (*k, v)).collect();
        daily.sort_by_key(|(bucket, _)| *bucket);
        let daily = daily
            .into_iter()
            .map(|(bucket, b)| {
                let date = if self.hourly {
                    // 「今天」视图：bucket = 本地小时序号，当天内取 0..24
                    format!("{:02}:00", bucket.rem_euclid(24))
                } else {
                    let (y, m, d) = civil_from_days(bucket);
                    format!("{:04}-{:02}-{:02}", y, m, d)
                };
                DailyStat {
                    date,
                    cost: b.cost,
                    calls: b.calls,
                    input_tokens: b.input,
                    output_tokens: b.output,
                    cache_read_tokens: b.cache_read,
                }
            })
            .collect();

        // cost 降序；全 $0 时退化为 tokens/calls 降序（价格未就绪时排行仍有意义）
        let all_zero = self.total_cost <= 0.0;
        let rank = |cost: f64, tokens: u64, calls: u64| -> (u64, u64, u64) {
            if all_zero {
                (0, tokens, calls)
            } else {
                ((cost * 1e9) as u64, tokens, calls)
            }
        };

        let mut by_project: Vec<ProjectStat> = self
            .projects
            .iter()
            .map(|(cwd, b)| ProjectStat {
                path: cwd.clone(),
                name: if cwd.is_empty() {
                    "(unknown)".to_string()
                } else {
                    project_name(cwd)
                },
                cost: b.cost,
                sessions: b.sessions,
                calls: b.calls,
                tokens: b.tokens,
            })
            .collect();
        by_project.sort_by(|a, b| {
            rank(b.cost, b.tokens, b.calls)
                .cmp(&rank(a.cost, a.tokens, a.calls))
                .then_with(|| a.name.cmp(&b.name))
        });
        by_project.truncate(TOP_PROJECTS);

        let mut by_model: Vec<ModelStat> = self
            .models
            .iter()
            .map(|(model, b)| ModelStat {
                model: model.clone(),
                cost: b.cost,
                calls: b.calls,
                tokens: b.tokens,
            })
            .collect();
        by_model.sort_by(|a, b| {
            rank(b.cost, b.tokens, b.calls)
                .cmp(&rank(a.cost, a.tokens, a.calls))
                .then_with(|| a.model.cmp(&b.model))
        });

        let mut by_provider: Vec<ProviderStat> = self
            .providers
            .iter()
            .map(|(provider, b)| ProviderStat {
                provider: provider.clone(),
                cost: b.cost,
                calls: b.calls,
                tokens: b.tokens,
                sessions: b.sessions,
            })
            .collect();
        by_provider.sort_by(|a, b| {
            rank(b.cost, b.tokens, b.calls)
                .cmp(&rank(a.cost, a.tokens, a.calls))
                .then_with(|| a.provider.cmp(&b.provider))
        });

        let mut tops: Vec<&SessionAcc> = self.sessions.iter().collect();
        tops.sort_by(|a, b| {
            rank(b.cost, b.tokens, b.calls)
                .cmp(&rank(a.cost, a.tokens, a.calls))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        let top_sessions = tops
            .into_iter()
            .take(TOP_SESSIONS)
            .map(|s| {
                let ts = if s.first_ts_ms == i64::MAX { 0 } else { s.first_ts_ms };
                // 与每日分桶同口径:按该会话自身时刻求当地偏移(DST 地区
                // 午夜附近的历史会话不错日,与图表一致)
                let offset_minutes = match &self.tz {
                    Some(tz) => tz_offset_minutes_at(tz, ts),
                    None => self.tz_offset_minutes,
                };
                let day = (ts - offset_minutes as i64 * 60_000).div_euclid(86_400_000);
                let (y, m, d) = civil_from_days(day);
                TopSessionStat {
                    session_id: s.session_id.clone(),
                    agent: s.agent.to_string(),
                    project_path: s.cwd.clone(),
                    project_name: if s.cwd.is_empty() {
                        "(unknown)".to_string()
                    } else {
                        project_name(&s.cwd)
                    },
                    title: s.title.clone(),
                    timestamp: format!("{:04}-{:02}-{:02}", y, m, d),
                    cost: s.cost,
                    calls: s.calls,
                    tokens: s.tokens,
                }
            })
            .collect();

        // 工具/Shell/MCP:次数降序,同次数按名字典序确定化;各截前 10
        let count_rank = |map: &HashMap<String, u64>| -> Vec<CountStat> {
            let mut v: Vec<CountStat> = map
                .iter()
                .map(|(name, &count)| CountStat { name: name.clone(), count })
                .collect();
            v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
            v.truncate(TOP_TOOLS);
            v
        };

        UsageStatsPayload {
            total_cost: self.total_cost,
            total_calls: self.total_calls,
            session_count: self.session_count,
            input_tokens: self.totals.input,
            output_tokens: self.totals.output + self.totals.reasoning,
            cache_read_tokens: self.totals.cache_read,
            cache_write_tokens: self.totals.cache_write,
            daily,
            by_project,
            by_model,
            by_provider,
            top_sessions,
            by_tool: count_rank(&self.tools),
            by_shell: count_rank(&self.shells),
            by_mcp: count_rank(&self.mcps),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_stats::pricing::ModelPrice;
    use crate::usage_stats::turns::Turn;
    use std::collections::HashMap as StdHashMap;

    fn pricing() -> PricingTable {
        let mut m = StdHashMap::new();
        m.insert(
            "claude-opus-4-8".to_string(),
            ModelPrice {
                input: 1e-6,
                output: 5e-6,
                cache_read: 1e-7,
                cache_write: 1.25e-6,
            },
        );
        PricingTable::new(m)
    }

    fn turn(id: Option<&str>, ts_ms: i64, output: u64) -> Turn {
        Turn {
            message_id: id.map(String::from),
            model: Some("claude-opus-4-8".into()),
            timestamp_ms: Some(ts_ms),
            usage: UsageTotals {
                input: 10,
                output,
                ..Default::default()
            },
        }
    }

    fn session(id: &str, cwd: &str, turns: Vec<Turn>) -> ParsedSession {
        ParsedSession {
            agent: "claude",
            session_id: id.into(),
            cwd: Some(cwd.into()),
            title: Some("t".into()),
            provider: Some("api.anthropic.com".into()),
            mtime_ms: 1_000_000,
            turns,
            tool_uses: Vec::new(),
        }
    }

    const DAY: i64 = 86_400_000;

    fn tool_use(kind: &'static str, name: &str, ts_ms: i64, key: Option<&str>) -> super::super::turns::ToolUse {
        super::super::turns::ToolUse {
            kind,
            name: name.into(),
            timestamp_ms: Some(ts_ms),
            dedup_key: key.map(String::from),
        }
    }

    #[test]
    fn tool_counts_deduped_and_window_filtered() {
        let table = pricing();
        let mut agg = Aggregator::new(DAY, None, 0, None, false);
        let mut s1 = session("s1", "/p", vec![]);
        s1.tool_uses.push(tool_use("tool", "Read", DAY + 1, Some("m1:0")));
        s1.tool_uses.push(tool_use("tool", "Bash", DAY + 2, Some("m2:0")));
        s1.tool_uses.push(tool_use("shell", "git", DAY + 2, Some("m2:0#s")));
        s1.tool_uses.push(tool_use("mcp", "context7", DAY + 3, Some("m3:0")));
        s1.tool_uses.push(tool_use("tool", "Out", DAY - 1, Some("m0:0"))); // 窗外
        agg.add_session(&s1, &table);
        // fork 复制:同 dedup_key 不得翻倍
        let mut s2 = session("s2", "/p", vec![]);
        s2.tool_uses.push(tool_use("tool", "Read", DAY + 1, Some("m1:0")));
        // Codex 无 key:逐条计数
        s2.tool_uses.push(tool_use("shell", "git", DAY + 5, None));
        agg.add_session(&s2, &table);

        let snap = agg.snapshot();
        assert_eq!(
            snap.by_tool.iter().map(|c| (c.name.as_str(), c.count)).collect::<Vec<_>>(),
            vec![("Bash", 1), ("Read", 1)],
            "同 key 去重、窗外不计、无 Out"
        );
        assert_eq!(snap.by_shell[0].name, "git");
        assert_eq!(snap.by_shell[0].count, 2, "无 key 的 Codex 事件逐条计数");
        assert_eq!(snap.by_mcp[0].name, "context7");
    }

    #[test]
    fn tool_counts_ranked_desc_and_capped_at_ten() {
        let table = pricing();
        let mut agg = Aggregator::new(0, None, 0, None, false);
        let mut s = session("s1", "/p", vec![]);
        for i in 0..12u64 {
            for _ in 0..=i {
                s.tool_uses.push(tool_use("tool", &format!("T{i:02}"), DAY, None));
            }
        }
        agg.add_session(&s, &table);
        let snap = agg.snapshot();
        assert_eq!(snap.by_tool.len(), 10, "各前 10");
        assert_eq!(snap.by_tool[0].name, "T11");
        assert_eq!(snap.by_tool[0].count, 12);
        assert_eq!(snap.by_tool[9].name, "T02");
    }

    #[test]
    fn tz_offset_follows_dst_at_record_time() {
        let tz: chrono_tz::Tz = "America/New_York".parse().unwrap();
        let winter = chrono::DateTime::parse_from_rfc3339("2026-01-15T12:00:00Z")
            .unwrap()
            .timestamp_millis();
        let summer = chrono::DateTime::parse_from_rfc3339("2026-07-15T12:00:00Z")
            .unwrap()
            .timestamp_millis();
        // getTimezoneOffset 语义(UTC−local 分钟):冬令 EST=+300,夏令 EDT=+240。
        // 修复前用「当前时刻」固定偏移套全量历史,跨 DST 边界的记录会错日
        assert_eq!(tz_offset_minutes_at(&tz, winter), 300);
        assert_eq!(tz_offset_minutes_at(&tz, summer), 240);
    }

    #[test]
    fn top_session_date_uses_offset_at_record_time() {
        // 纽约夏令(EDT=UTC-4):UTC 04:30 → 本地 00:30 属当日;
        // 修复前用固定冬令偏移(+300)会算成前一日 23:30,与每日图错位
        let summer = chrono::DateTime::parse_from_rfc3339("2026-07-15T04:30:00Z")
            .unwrap()
            .timestamp_millis();
        let mut agg = Aggregator::new(0, None, 300, Some("America/New_York"), false);
        let p = pricing();
        agg.add_session(&session("s1", "/a", vec![turn(Some("m1"), summer, 10)]), &p);
        let snap = agg.snapshot();
        assert_eq!(snap.top_sessions[0].timestamp, "2026-07-15");
        assert_eq!(snap.daily[0].date, "2026-07-15", "会话日期必须与每日桶一致");
    }

    #[test]
    fn cross_file_dedup_does_not_double_count() {
        let mut agg = Aggregator::new(0, None, 0, None, false);
        let p = pricing();
        agg.add_session(&session("s1", "/a", vec![turn(Some("m1"), DAY, 100)]), &p);
        // fork 会话复制了同一条消息
        agg.add_session(&session("s2", "/a", vec![turn(Some("m1"), DAY, 100)]), &p);
        let snap = agg.snapshot();
        assert_eq!(snap.total_calls, 1);
        assert_eq!(snap.output_tokens, 100);
        // 全部被去重的会话不计入 session_count
        assert_eq!(snap.session_count, 1);
    }

    #[test]
    fn per_turn_window_filter_and_daily_sum_matches_kpi() {
        // 窗口从第 2 天本地午夜起：第 1 天的 turn 全维度不算
        let mut agg = Aggregator::new(DAY, None, 0, None, false);
        let p = pricing();
        agg.add_session(
            &session(
                "s1",
                "/a",
                vec![
                    turn(Some("m1"), DAY / 2, 100), // 窗外
                    turn(Some("m2"), DAY + 100, 50),
                    turn(Some("m3"), DAY * 2 + 100, 70),
                ],
            ),
            &p,
        );
        let snap = agg.snapshot();
        assert_eq!(snap.total_calls, 2);
        assert_eq!(snap.output_tokens, 120);
        // 顶层 KPI == 窗内 daily 之和
        let daily_calls: u64 = snap.daily.iter().map(|d| d.calls).sum();
        let daily_cost: f64 = snap.daily.iter().map(|d| d.cost).sum();
        assert_eq!(daily_calls, snap.total_calls);
        assert!((daily_cost - snap.total_cost).abs() < 1e-9);
        assert_eq!(snap.daily.len(), 2);
        assert_eq!(snap.daily[0].date, "1970-01-02");
    }

    #[test]
    fn all_turns_out_of_window_drops_session() {
        let mut agg = Aggregator::new(DAY * 10, None, 0, None, false);
        let p = pricing();
        agg.add_session(&session("s1", "/a", vec![turn(Some("m1"), DAY, 100)]), &p);
        let snap = agg.snapshot();
        assert_eq!(snap.session_count, 0);
        assert!(snap.by_project.is_empty());
        assert!(snap.top_sessions.is_empty());
    }

    #[test]
    fn missing_turn_timestamp_falls_back_to_file_mtime() {
        let mut agg = Aggregator::new(2_000_000, None, 0, None, false);
        let p = pricing();
        let mut s = session("s1", "/a", vec![]);
        s.mtime_ms = 3_000_000; // 窗内
        s.turns.push(Turn {
            message_id: Some("m1".into()),
            model: None,
            timestamp_ms: None,
            usage: UsageTotals { input: 1, output: 1, ..Default::default() },
        });
        agg.add_session(&s, &p);
        assert_eq!(agg.snapshot().total_calls, 1);
    }

    #[test]
    fn daily_bucket_uses_local_timezone() {
        // 东八区(offset = -480)：UTC 23:00 属于本地次日
        let mut agg = Aggregator::new(0, None, -480, None, false);
        let p = pricing();
        agg.add_session(&session("s1", "/a", vec![turn(Some("m1"), 23 * 3_600_000, 10)]), &p);
        let snap = agg.snapshot();
        assert_eq!(snap.daily[0].date, "1970-01-02");
    }

    #[test]
    fn rankings_sort_by_cost_then_fall_back_to_tokens_when_unpriced() {
        let p = pricing();
        let mut agg = Aggregator::new(0, None, 0, None, false);
        agg.add_session(&session("s1", "/big", vec![turn(Some("m1"), DAY, 1000)]), &p);
        agg.add_session(&session("s2", "/small", vec![turn(Some("m2"), DAY, 10)]), &p);
        let snap = agg.snapshot();
        assert_eq!(snap.by_project[0].name, "big");
        assert_eq!(snap.top_sessions[0].session_id, "s1");

        // 空价格表 → 成本全 0 → 按 tokens 降序仍有排行
        let empty = PricingTable::new(StdHashMap::new());
        let mut agg = Aggregator::new(0, None, 0, None, false);
        agg.add_session(&session("s1", "/big", vec![turn(Some("m1"), DAY, 1000)]), &empty);
        agg.add_session(&session("s2", "/small", vec![turn(Some("m2"), DAY, 10)]), &empty);
        let snap = agg.snapshot();
        assert_eq!(snap.total_cost, 0.0);
        assert_eq!(snap.by_project[0].name, "big");
    }

    #[test]
    fn codex_turns_without_id_skip_global_dedup() {
        let mut agg = Aggregator::new(0, None, 0, None, false);
        let p = pricing();
        agg.add_session(&session("s1", "/a", vec![turn(None, DAY, 10), turn(None, DAY, 10)]), &p);
        assert_eq!(agg.snapshot().total_calls, 2);
    }

    #[test]
    fn hourly_buckets_use_local_hour_labels() {
        // 东八区：UTC 01:30 → 本地 09:30，落 "09:00" 桶
        let mut agg = Aggregator::new(0, None, -480, None, true);
        let p = pricing();
        agg.add_session(
            &session(
                "s1",
                "/a",
                vec![
                    turn(Some("m1"), 3_600_000 + 1_800_000, 10), // UTC 01:30
                    turn(Some("m2"), 2 * 3_600_000 + 600_000, 20), // UTC 02:10 → 本地 10:10
                ],
            ),
            &p,
        );
        let snap = agg.snapshot();
        assert_eq!(snap.daily.len(), 2);
        assert_eq!(snap.daily[0].date, "09:00");
        assert_eq!(snap.daily[1].date, "10:00");
        // 顶层 KPI == 各时段之和
        assert_eq!(snap.daily.iter().map(|d| d.calls).sum::<u64>(), snap.total_calls);
    }

    #[test]
    fn by_provider_groups_sessions_by_source() {
        let mut agg = Aggregator::new(0, None, 0, None, false);
        let p = pricing();
        agg.add_session(&session("s1", "/a", vec![turn(Some("m1"), DAY, 100)]), &p);
        let mut s2 = session("s2", "/b", vec![turn(Some("m2"), DAY, 10)]);
        s2.provider = Some("relay.example.com".into());
        agg.add_session(&s2, &p);
        let snap = agg.snapshot();
        assert_eq!(snap.by_provider.len(), 2);
        assert_eq!(snap.by_provider[0].provider, "api.anthropic.com");
        assert_eq!(snap.by_provider[0].sessions, 1);
        // 时段桶携带 token 明细，供 hover 详情
        assert_eq!(snap.daily[0].output_tokens, 110);
        assert_eq!(snap.daily[0].input_tokens, 20);
    }

    #[test]
    fn by_model_groups_date_variants_and_sorts_by_cost() {
        let mut agg = Aggregator::new(0, None, 0, None, false);
        let p = pricing();
        let mut s = session("s1", "/a", vec![turn(Some("m1"), DAY, 1000)]);
        // 同模型的带日期版本归入同一桶
        s.turns.push(Turn {
            message_id: Some("m2".into()),
            model: Some("claude-opus-4-8-20260115".into()),
            timestamp_ms: Some(DAY),
            usage: UsageTotals { input: 10, output: 50, ..Default::default() },
        });
        // 无模型 turn 归入空 key（前端翻译展示）
        s.turns.push(Turn {
            message_id: Some("m3".into()),
            model: None,
            timestamp_ms: Some(DAY),
            usage: UsageTotals { input: 1, output: 1, ..Default::default() },
        });
        agg.add_session(&s, &p);
        let snap = agg.snapshot();
        assert_eq!(snap.by_model.len(), 2);
        assert_eq!(snap.by_model[0].model, "claude-opus-4-8");
        assert_eq!(snap.by_model[0].calls, 2);
        assert_eq!(snap.by_model[1].model, "");
    }
}
