use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// 单次 API 调用的 token 用量（各分量互斥：`input` 不含缓存读写，
/// `output` 不含 `reasoning`；Codex 侧解析时已换算到该语义）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_1h: u64,
}

impl UsageTotals {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, o: &UsageTotals) {
        self.input += o.input;
        self.output += o.output;
        self.reasoning += o.reasoning;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cache_write_1h += o.cache_write_1h;
    }
}

/// 一次 assistant API 调用（统计粒度）。
#[derive(Debug, Clone)]
pub struct Turn {
    /// 去重键（Claude `message.id`）；Codex 无 id，不参与跨文件去重。
    pub message_id: Option<String>,
    pub model: Option<String>,
    pub timestamp_ms: Option<i64>,
    pub usage: UsageTotals,
}

/// 单个会话解析结果（Claude 主转录 + 子代理转录已合并；Codex 一文件一会话）。
#[derive(Debug, Clone)]
pub struct ParsedSession {
    pub agent: &'static str, // "claude" | "codex"
    pub session_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// 供应商归属：Codex 为 session_meta.model_provider 的 id（"openai"/"custom"…），
    /// Claude 转录不记录 baseurl，恒 None（由扫描层按当前配置归桶）。
    pub provider: Option<String>,
    /// 文件 mtime（turn 缺时间戳时的回退终判依据）。
    pub mtime_ms: i64,
    pub turns: Vec<Turn>,
}

// ─── 时间工具（无 chrono 依赖，手写 RFC3339 + civil date） ──────

/// Howard Hinnant days_from_civil：公历日期 → 距 1970-01-01 天数。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 反向：距 1970-01-01 天数 → (year, month, day)。
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn digits(s: &str, range: std::ops::Range<usize>) -> Option<i64> {
    let sub = s.get(range)?;
    if sub.is_empty() || !sub.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    sub.parse().ok()
}

/// RFC3339 → epoch ms。支持 `YYYY-MM-DDTHH:MM:SS[.fff…][Z|±HH:MM|±HHMM]`；
/// 无时区后缀按 UTC 处理（Codex rollout 文件名沿用的本地时间不走这里）。
pub(crate) fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let y = digits(s, 0..4)?;
    let m = digits(s, 5..7)? as u32;
    let d = digits(s, 8..10)? as u32;
    let hh = digits(s, 11..13)?;
    let mm = digits(s, 14..16)?;
    let ss = digits(s, 17..19)?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }

    let rest = &s[19..];
    let mut millis: i64 = 0;
    let mut tz_start = 0;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_end = after_dot
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        let frac = &rest[1..frac_end];
        let ms_str = if frac.len() >= 3 { &frac[..3] } else { frac };
        let scale = 10i64.pow(3 - ms_str.len() as u32);
        millis = ms_str.parse::<i64>().ok()? * scale;
        tz_start = frac_end;
    }

    let tz = &rest[tz_start..];
    let offset_min: i64 = if tz.is_empty() || tz == "Z" || tz == "z" {
        0
    } else {
        let sign = match tz.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let body = tz[1..].replace(':', "");
        if body.len() != 4 || !body.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let oh: i64 = body[..2].parse().ok()?;
        let om: i64 = body[2..].parse().ok()?;
        sign * (oh * 60 + om)
    };

    let days = days_from_civil(y, m, d);
    Some(((days * 86400 + hh * 3600 + mm * 60 + ss) - offset_min * 60) * 1000 + millis)
}

pub(crate) fn mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ─── Claude 转录解析 ───────────────────────────────────────────

fn u64_at(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// message.usage → UsageTotals。cache_creation 兼容 legacy 整数与
/// split `cache_creation.{ephemeral_5m,ephemeral_1h}` 两种形状：
/// total 取 max(legacy, 5m+1h)，1h 子集钳到 ≤ total。
fn usage_from_claude(usage: &serde_json::Value) -> UsageTotals {
    let legacy_cw = u64_at(usage, "cache_creation_input_tokens");
    let (split_5m, split_1h) = usage
        .get("cache_creation")
        .map(|cc| {
            (
                u64_at(cc, "ephemeral_5m_input_tokens"),
                u64_at(cc, "ephemeral_1h_input_tokens"),
            )
        })
        .unwrap_or((0, 0));
    let cache_write = legacy_cw.max(split_5m + split_1h);
    let cache_write_1h = split_1h.min(cache_write);
    UsageTotals {
        input: u64_at(usage, "input_tokens"),
        output: u64_at(usage, "output_tokens"),
        reasoning: 0, // Anthropic 的思考 token 已并入 output
        cache_read: u64_at(usage, "cache_read_input_tokens"),
        cache_write,
        cache_write_1h,
    }
}

/// 提取 user 行的首段文本作为标题候选（跳过 `<` 开头的系统注入）。
fn claude_title_from_user_line(obj: &serde_json::Value) -> Option<String> {
    let content = obj.pointer("/message/content")?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr.iter().find_map(|item| {
            (item.get("type").and_then(|t| t.as_str()) == Some("text"))
                .then(|| item.get("text").and_then(|t| t.as_str()).map(String::from))
                .flatten()
        })?,
        _ => return None,
    };
    let trimmed = text.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return None;
    }
    Some(trimmed.chars().take(100).collect())
}

/// 逐行解析 Claude JSONL 的 turns（文件内同 message.id 合并：usage 取
/// total 大的一侧，model/timestamp 取该侧非空值——流式写入的中间行 usage 为 0）。
/// 返回 (turns, cwd, title)。
fn claude_turns_from_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> (Vec<Turn>, Option<String>, Option<String>) {
    let mut turns: Vec<Turn> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut cwd: Option<String> = None;
    let mut title: Option<String> = None;

    for line in lines {
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if cwd.is_none() {
            cwd = obj.get("cwd").and_then(|v| v.as_str()).map(String::from);
        }
        match obj.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                if title.is_none() {
                    title = claude_title_from_user_line(&obj);
                }
            }
            Some("assistant") => {
                // <synthetic> 是本地合成消息(报错占位等)，无真实 API 调用且 usage 全 0
                if obj.pointer("/message/model").and_then(|v| v.as_str()) == Some("<synthetic>") {
                    continue;
                }
                let Some(usage_val) = obj.pointer("/message/usage") else {
                    continue;
                };
                let usage = usage_from_claude(usage_val);
                let model = obj
                    .pointer("/message/model")
                    .and_then(|v| v.as_str())
                    .filter(|m| !m.is_empty())
                    .map(String::from);
                let timestamp_ms = obj
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .and_then(parse_rfc3339_ms);
                let id = obj
                    .pointer("/message/id")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                if let Some(ref mid) = id {
                    if let Some(&idx) = by_id.get(mid) {
                        let existing = &mut turns[idx];
                        if usage.total() > existing.usage.total() {
                            existing.usage = usage;
                            if model.is_some() {
                                existing.model = model;
                            }
                            if timestamp_ms.is_some() {
                                existing.timestamp_ms = timestamp_ms;
                            }
                        } else if existing.model.is_none() {
                            existing.model = model;
                        }
                        continue;
                    }
                    by_id.insert(mid.clone(), turns.len());
                }
                turns.push(Turn {
                    message_id: id,
                    model,
                    timestamp_ms,
                    usage,
                });
            }
            _ => {}
        }
    }
    (turns, cwd, title)
}

fn read_lines(path: &Path) -> Option<Vec<String>> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    // 坏行(非 UTF-8)只跳过该行，不截断其后内容(与 ai_sessions.rs 口径一致)
    Some(reader.lines().map_while(|l| l.ok().or(Some(String::new()))).collect())
}

/// 解析一个 Claude 会话：主转录 + 子代理转录（独立计费，漏掉会整块低估成本）。
/// 子代理 turns 直接并入主会话；主/子若复制了同一条 assistant 消息，
/// 由聚合层的跨文件 message_id 去重兜底。
pub(crate) fn parse_claude_session(
    main_path: &Path,
    subagent_paths: &[std::path::PathBuf],
) -> Option<ParsedSession> {
    let session_id = main_path.file_stem()?.to_str()?.to_string();
    let lines = read_lines(main_path)?;
    let (mut turns, cwd, title) = claude_turns_from_lines(lines.iter().map(String::as_str));

    for sub in subagent_paths {
        if let Some(sub_lines) = read_lines(sub) {
            let (sub_turns, _, _) = claude_turns_from_lines(sub_lines.iter().map(String::as_str));
            turns.extend(sub_turns);
        }
    }

    Some(ParsedSession {
        agent: "claude",
        session_id,
        cwd,
        title,
        provider: None,
        mtime_ms: mtime_ms(main_path),
        turns,
    })
}

// ─── Codex rollout 解析 ────────────────────────────────────────

/// token_count 事件的 usage 载体（`info.total_token_usage` 为**累计**口径，
/// `info.last_token_usage` 为本轮增量）。OpenAI 口径换算到互斥语义：
/// `input_tokens` 含 `cached_input_tokens` 子集 → input 减去 cache_read；
/// `output_tokens` 含 `reasoning_output_tokens` 子集 → output 减去 reasoning。
fn usage_from_codex(u: &serde_json::Value) -> UsageTotals {
    let raw_input = u64_at(u, "input_tokens");
    let cached = u64_at(u, "cached_input_tokens");
    let raw_output = u64_at(u, "output_tokens");
    let reasoning = u64_at(u, "reasoning_output_tokens");
    UsageTotals {
        input: raw_input.saturating_sub(cached),
        output: raw_output.saturating_sub(reasoning),
        reasoning,
        cache_read: cached,
        cache_write: 0, // OpenAI 不单列缓存写
        cache_write_1h: 0,
    }
}

/// 解析一个 Codex rollout 文件（一文件一会话）。
/// usage 优先取每条 token_count 的 `last_token_usage`（自带该轮 timestamp）；
/// 缺失时对 `total_token_usage` 做相邻差分兜底。Codex 无 message id，不参与全局去重。
pub(crate) fn parse_codex_session(
    path: &Path,
    thread_names: &HashMap<String, String>,
) -> Option<ParsedSession> {
    let lines = read_lines(path)?;

    let mut session_id = String::new();
    let mut cwd: Option<String> = None;
    let mut title: Option<String> = None;
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut turns: Vec<Turn> = Vec::new();
    let mut prev_total = UsageTotals::default();

    for line in &lines {
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let line_ts = obj
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_rfc3339_ms);
        match obj.get("type").and_then(|t| t.as_str()) {
            Some("session_meta") => {
                if let Some(meta) = crate::ai_sessions::codex_meta_from_line(line) {
                    session_id = meta.id;
                    if !meta.cwd.is_empty() {
                        cwd = Some(meta.cwd);
                    }
                }
                provider = obj
                    .pointer("/payload/model_provider")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from);
            }
            Some("turn_context") => {
                // model 每回合记录且一个会话可跨多模型：随行更新，后续 token_count 用当前值
                if let Some(m) = obj.pointer("/payload/model").and_then(|v| v.as_str()) {
                    model = Some(m.to_string());
                }
                if cwd.is_none() {
                    cwd = obj
                        .pointer("/payload/cwd")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                }
            }
            Some("response_item") => {
                // 兜底标题(旧格式无 user_message 事件)；developer 注入行由
                // codex_user_title_from_line 的 `<`/`# AGENTS.md` 过滤挡住
                if title.is_none() {
                    title = crate::ai_sessions::codex_user_title_from_line(line);
                }
            }
            Some("event_msg") => {
                let payload_type = obj.pointer("/payload/type").and_then(|t| t.as_str());
                if payload_type == Some("user_message") {
                    // 首选标题来源：真正的用户回合(比 response_item 干净)，只取首条
                    if title.is_none() {
                        if let Some(msg) = obj.pointer("/payload/message").and_then(|v| v.as_str()) {
                            let trimmed = msg.trim_start();
                            if !trimmed.is_empty() && !trimmed.starts_with('<') {
                                title = Some(trimmed.chars().take(100).collect());
                            }
                        }
                    }
                    continue;
                }
                if payload_type != Some("token_count") {
                    continue;
                }
                let Some(info) = obj.pointer("/payload/info") else {
                    continue;
                };
                let usage = if let Some(last) = info.get("last_token_usage") {
                    usage_from_codex(last)
                } else if let Some(total) = info.get("total_token_usage") {
                    // 累计口径差分；累计值回卷(compact 等)时跳过该条
                    let cur = usage_from_codex(total);
                    let delta = UsageTotals {
                        input: cur.input.saturating_sub(prev_total.input),
                        output: cur.output.saturating_sub(prev_total.output),
                        reasoning: cur.reasoning.saturating_sub(prev_total.reasoning),
                        cache_read: cur.cache_read.saturating_sub(prev_total.cache_read),
                        cache_write: 0,
                        cache_write_1h: 0,
                    };
                    prev_total = cur;
                    delta
                } else {
                    continue;
                };
                if usage.total() == 0 {
                    continue;
                }
                turns.push(Turn {
                    // Codex rollout 无消息 id,合成稳定指纹供聚合层跨 rollout 去重
                    // (fork 复制父会话历史、重复 token_count 行都会原样带着时间戳
                    // 与用量)。不含 session_id:fork 出的新 rollout 换了 id,含了
                    // 反而挡不住复制行。时间戳缺失时不合成(mtime 兜底跨文件不稳),
                    // 宁可不去重也不误伤。
                    message_id: line_ts.map(|ts| {
                        format!(
                            "codex:{}:{}:{}:{}:{}",
                            ts, usage.input, usage.output, usage.reasoning, usage.cache_read
                        )
                    }),
                    model: model.clone(),
                    timestamp_ms: line_ts,
                    usage,
                });
            }
            _ => {}
        }
    }

    if session_id.is_empty() {
        session_id = path.file_stem()?.to_str()?.to_string();
    }
    let title = title.or_else(|| thread_names.get(&session_id).cloned());

    Some(ParsedSession {
        agent: "codex",
        session_id,
        cwd,
        title,
        provider,
        mtime_ms: mtime_ms(path),
        turns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rfc3339_variants() {
        // 1970-01-02T00:00:00Z = 86400s
        assert_eq!(parse_rfc3339_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
        // 毫秒
        assert_eq!(
            parse_rfc3339_ms("1970-01-01T00:00:00.123Z"),
            Some(123)
        );
        // 时区偏移：东八区 08:00 == UTC 00:00
        assert_eq!(
            parse_rfc3339_ms("1970-01-01T08:00:00+08:00"),
            Some(0)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-08-01T00:00:00Z"),
            Some(days_from_civil(2026, 8, 1) * 86_400_000)
        );
        assert!(parse_rfc3339_ms("not a date").is_none());
        assert!(parse_rfc3339_ms("").is_none());
    }

    #[test]
    fn civil_roundtrip() {
        for &(y, m, d) in &[(1970, 1, 1), (2000, 2, 29), (2026, 8, 1), (1999, 12, 31)] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn claude_usage_merges_same_message_id_keeping_larger_total() {
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:00Z","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:05Z","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":50,"cache_read_input_tokens":100}}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:01:00Z","message":{"id":"m2","model":"claude-opus-4-8","usage":{"input_tokens":5,"output_tokens":7}}}"#,
        ];
        let (turns, _, _) = claude_turns_from_lines(lines.iter().copied());
        assert_eq!(turns.len(), 2, "同 id 多行必须合并，不得翻倍");
        assert_eq!(turns[0].usage.output, 50);
        assert_eq!(turns[0].usage.cache_read, 100);
        assert_eq!(turns[1].usage.output, 7);
    }

    #[test]
    fn claude_usage_cache_creation_legacy_and_split_shapes() {
        // split 之和大于 legacy → 取 split；1h 子集保留
        let u = usage_from_claude(&serde_json::json!({
            "input_tokens": 1,
            "output_tokens": 2,
            "cache_creation_input_tokens": 100,
            "cache_creation": {"ephemeral_5m_input_tokens": 80, "ephemeral_1h_input_tokens": 40}
        }));
        assert_eq!(u.cache_write, 120);
        assert_eq!(u.cache_write_1h, 40);

        // 仅 legacy
        let u = usage_from_claude(&serde_json::json!({
            "cache_creation_input_tokens": 100
        }));
        assert_eq!(u.cache_write, 100);
        assert_eq!(u.cache_write_1h, 0);

        // 1h 子集钳到 ≤ total
        let u = usage_from_claude(&serde_json::json!({
            "cache_creation": {"ephemeral_1h_input_tokens": 40}
        }));
        assert_eq!(u.cache_write, 40);
        assert_eq!(u.cache_write_1h, 40);
    }

    #[test]
    fn claude_lines_extract_cwd_and_title_skip_injected() {
        let lines = [
            r#"{"type":"summary","summary":"x"}"#,
            r#"{"type":"user","cwd":"/Users/u/proj","message":{"content":"<system-hint>skip"},"timestamp":"2026-01-01T00:00:00Z"}"#,
            r#"{"type":"user","cwd":"/Users/u/proj","message":{"content":"fix the bug"},"timestamp":"2026-01-01T00:00:01Z"}"#,
        ];
        let (_, cwd, title) = claude_turns_from_lines(lines.iter().copied());
        assert_eq!(cwd.as_deref(), Some("/Users/u/proj"));
        assert_eq!(title.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn codex_usage_converts_openai_inclusive_to_exclusive() {
        let u = usage_from_codex(&serde_json::json!({
            "input_tokens": 1000,
            "cached_input_tokens": 800,
            "output_tokens": 120,
            "reasoning_output_tokens": 20,
            "total_tokens": 1120
        }));
        assert_eq!(u.input, 200);
        assert_eq!(u.cache_read, 800);
        assert_eq!(u.output, 100);
        assert_eq!(u.reasoning, 20);
    }

    #[test]
    fn codex_turns_get_synthetic_fingerprint_stable_across_rollouts() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-turns-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let l1 = r#"{"type":"event_msg","timestamp":"2026-08-01T10:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}"#;
        let l2 = r#"{"type":"event_msg","timestamp":"2026-08-01T10:00:05.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}}"#;

        let a = root.join("rollout-a.jsonl");
        std::fs::write(&a, format!("{l1}\n{l2}\n")).unwrap();
        // fork:新 rollout 原样复制父会话历史行,session id(此处回退文件名)不同
        let b = root.join("rollout-b.jsonl");
        std::fs::write(&b, format!("{l1}\n")).unwrap();

        let names = HashMap::new();
        let pa = parse_codex_session(&a, &names).unwrap();
        let pb = parse_codex_session(&b, &names).unwrap();

        assert_eq!(pa.turns.len(), 2);
        let id0 = pa.turns[0].message_id.as_ref().unwrap();
        let id1 = pa.turns[1].message_id.as_ref().unwrap();
        // 不同调用 → 指纹不同,不会误去重
        assert_ne!(id0, id1);
        // fork 复制的同一历史行 → 指纹一致,聚合层 seen_ids 跨 rollout 去重生效
        assert_eq!(pb.turns[0].message_id.as_ref().unwrap(), id0);
    }
}
