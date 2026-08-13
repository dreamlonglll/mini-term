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

/// 单次工具调用记录（统计粒度，设计 §2.2 工具/Shell/MCP 排行）。
/// kind："tool"（非 MCP 工具名）| "shell"（Bash 主命令首词）| "mcp"（server 名）。
/// 一次 Bash tool_use 产生 tool=Bash 与 shell=首词 两条；mcp__s__t 只产生 mcp=s。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolUse {
    pub kind: &'static str,
    pub name: String,
    /// 窗口判定时刻（缺失回退 session mtime，与 turn 同口径）。
    pub timestamp_ms: Option<i64>,
    /// 跨文件去重键（Claude 按 message_id 派生；Codex 无 → None，
    /// fork 复制的血缘过滤在解析层完成，与 token_count 同规则）。
    pub dedup_key: Option<String>,
}

/// §6.9 Shell 主命令首词提取（按实测形状加固）：连接符（&&、||、;、|）切段，
/// `cd` 段跳过（Claude Bash 普遍 `cd X && 真命令`）；段内去 `KEY=VAL` 前缀、跳
/// sudo/time/env/command/exec/nohup wrapper；带引号的可执行路径取引号内整体的
/// basename。复杂引号/管道不做精确解析（统计用，噪声可接受）。
fn shell_main_command(cmd: &str) -> Option<String> {
    for seg in cmd.split([';', '|']).flat_map(|s| s.split("&&")) {
        match segment_main_word(seg) {
            Some(w) if w == "cd" => continue,
            Some(w) => return Some(w),
            None => continue,
        }
    }
    None
}

fn segment_main_word(seg: &str) -> Option<String> {
    const WRAPPERS: [&str; 6] = ["sudo", "time", "env", "command", "exec", "nohup"];
    fn is_env_assign(tok: &str) -> bool {
        match tok.split_once('=') {
            Some((key, _)) => {
                !key.is_empty()
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            }
            None => false,
        }
    }
    let s = seg.trim_start();
    // 引号包裹的可执行路径("/Applications/…/Google Chrome"):引号内整体取 basename
    for q in ['"', '\''] {
        if let Some(rest) = s.strip_prefix(q) {
            let inner = &rest[..rest.find(q)?];
            let base = inner.rsplit('/').next().unwrap_or(inner).trim();
            return (!base.is_empty()).then(|| base.to_string());
        }
    }
    let tok = s
        .split_whitespace()
        .find(|t| !is_env_assign(t) && !WRAPPERS.contains(t))?;
    let base = tok.rsplit('/').next().unwrap_or(tok);
    (!base.is_empty()).then(|| base.to_string())
}

/// 由一次工具调用派生排行事件（设计 §2.2）：`mcp__server__tool` → mcp(server)，
/// 不进 tool 排行；其余 → tool(name)，shell 命令文本存在时再追加 shell 首词
/// （去重键在基础键上加 `#s` 后缀，与 tool 事件互不冲突）。
fn push_tool_events(
    out: &mut Vec<ToolUse>,
    name: &str,
    shell_cmd: Option<&str>,
    timestamp_ms: Option<i64>,
    dedup_base: Option<String>,
) {
    if let Some(rest) = name.strip_prefix("mcp__") {
        let server = rest.split("__").next().unwrap_or(rest);
        out.push(ToolUse {
            kind: "mcp",
            name: server.to_string(),
            timestamp_ms,
            dedup_key: dedup_base,
        });
        return;
    }
    out.push(ToolUse {
        kind: "tool",
        name: name.to_string(),
        timestamp_ms,
        dedup_key: dedup_base.clone(),
    });
    if let Some(word) = shell_cmd.and_then(shell_main_command) {
        out.push(ToolUse {
            kind: "shell",
            name: word,
            timestamp_ms,
            dedup_key: dedup_base.map(|k| format!("{k}#s")),
        });
    }
}

/// 单个会话解析结果（Claude 主转录 + 子代理转录已合并；Codex 一文件一会话）。
#[derive(Debug, Clone)]
pub struct ParsedSession {
    pub agent: &'static str, // "claude" | "codex" | "grok"
    pub session_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// 供应商归属：Codex 为 session_meta.model_provider 的 id（"openai"/"custom"…），
    /// Claude 转录不记录 baseurl，恒 None（由扫描层按当前配置归桶）。
    pub provider: Option<String>,
    /// 文件 mtime（turn 缺时间戳时的回退终判依据）。
    pub mtime_ms: i64,
    pub turns: Vec<Turn>,
    /// 工具/Shell/MCP 调用记录（设计 §2.2 排行；与 turns 独立收集）。
    pub tool_uses: Vec<ToolUse>,
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
/// total 大的一侧，model/timestamp 取该侧非空值——实测每行恰一个 content 块，
/// 同一逻辑消息跨多行共享 message.id）。工具事件按块自身 toolu id 作去重键
/// 独立收集（fork 复制原样保留 toolu id，跨文件去重交聚合层）。
/// 返回 (turns, tool_uses, cwd, title)。
fn claude_turns_from_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> (Vec<Turn>, Vec<ToolUse>, Option<String>, Option<String>) {
    let mut turns: Vec<Turn> = Vec::new();
    let mut tool_uses: Vec<ToolUse> = Vec::new();
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
                let timestamp_ms = obj
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .and_then(parse_rfc3339_ms);

                // 工具/Shell/MCP 事件:content[] 的 tool_use 块(实测每行一块)
                if let Some(blocks) = obj.pointer("/message/content").and_then(|v| v.as_array()) {
                    for b in blocks {
                        if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                            continue;
                        }
                        let Some(name) = b.get("name").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let shell_cmd = (name == "Bash")
                            .then(|| b.pointer("/input/command").and_then(|v| v.as_str()))
                            .flatten();
                        let block_id = b.get("id").and_then(|v| v.as_str()).map(String::from);
                        push_tool_events(&mut tool_uses, name, shell_cmd, timestamp_ms, block_id);
                    }
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
    // 计费门槛：任一计费维度 > 0 才计入。全 0 行（流式中间快照未被同 id 覆盖、
    // 本地占位等）不是一次计费调用，计入会虚增 calls（Codex 侧解析时已同门槛）
    turns.retain(|t| t.usage.total() > 0);
    (turns, tool_uses, cwd, title)
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
    let (mut turns, mut tool_uses, cwd, title) =
        claude_turns_from_lines(lines.iter().map(String::as_str));

    for sub in subagent_paths {
        if let Some(sub_lines) = read_lines(sub) {
            let (sub_turns, sub_tools, _, _) =
                claude_turns_from_lines(sub_lines.iter().map(String::as_str));
            turns.extend(sub_turns);
            tool_uses.extend(sub_tools);
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
        tool_uses,
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

/// 0.146+ 的 subagent/fork rollout 会把复制的父历史逐行改写时间戳为复制时刻
/// （实测比 child session_meta 晚 ~1ms），`ts < meta_ts` 挡不住。复制块是一次性
/// 突发写入（行间距毫秒级），而真实 turn 与它之间必有一次模型往返的间隙——
/// 含复制历史的文件（≥2 个 session_meta）从最后一个 meta 起按**连续簇**截断：
/// 与簇尾间距 ≤ 本容差的行并入簇丢弃，出现更大间隙即停（快速真实 turn 幸存，
/// 超长复制块也能整块吃掉，不受固定窗口长度限制）。
const LINEAGE_GAP_MS: i64 = 500;

/// Codex 工具调用的 shell 命令文本（实测三种形状）：`exec_command` 的
/// `arguments.cmd`、旧版 `shell_command` 的 `arguments.command`（arguments 均为
/// 二次编码的 JSON 串）、`custom_tool_call name=exec` 埋在 payload.input JS 串
/// 里的 `exec_command({…})` 参数对象（流式反序列化取首个完整 JSON 值，忽略
/// 其后的 JS 代码）。
fn codex_shell_cmd(name: &str, obj: &serde_json::Value) -> Option<String> {
    match name {
        "exec_command" | "shell_command" => {
            let args = obj.pointer("/payload/arguments")?.as_str()?;
            let v: serde_json::Value = serde_json::from_str(args).ok()?;
            let key = if name == "exec_command" { "cmd" } else { "command" };
            v.get(key)?.as_str().map(String::from)
        }
        "exec" => {
            let input = obj.pointer("/payload/input")?.as_str()?;
            let pos = input.find("exec_command(")? + "exec_command(".len();
            let v = serde_json::Deserializer::from_str(&input[pos..])
                .into_iter::<serde_json::Value>()
                .next()?
                .ok()?;
            v.get("cmd")?.as_str().map(String::from)
        }
        _ => None,
    }
}

/// 解析一个 Codex rollout 文件（一文件一会话）。
/// usage 优先取每条 token_count 的 `last_token_usage`（自带该轮 timestamp）；
/// 缺失时对 `total_token_usage` 做相邻差分兜底。Codex 无 message id，不参与
/// 聚合层去重；fork/resume 复制进来的父会话历史在本函数内按「早于本文件
/// session_meta 时刻」前缀跳过（时间戳被改写的复制块另按 LINEAGE_EPSILON_MS
/// 截断），防止跨 rollout 重复计费。
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
    let mut tool_uses: Vec<ToolUse> = Vec::new();
    let mut prev_total = UsageTotals::default();
    let mut meta_ts: Option<i64> = None;
    // 血缘复制簇的滚动簇尾：起于最后一个 session_meta 时刻，随被丢弃的复制行推进
    let mut lineage_cluster_end: Option<i64> = None;
    let mut meta_count = 0usize;

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
                meta_count += 1;
                if let Some(ts) = line_ts {
                    lineage_cluster_end = Some(lineage_cluster_end.map_or(ts, |m| m.max(ts)));
                }
                // 身份字段只认首个 meta：后续 meta 是 subagent/fork 复制进来的
                // 父会话头，覆盖会把本会话记成父 ID（Top Session 点开错会话）
                if meta_count == 1 {
                    // 本文件的创建时刻:早于它的 token_count 只能是 fork/resume
                    // 复制进来的父会话历史(复制行原样保留原时间戳)
                    meta_ts = line_ts;
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
                // 工具调用事件(实测形状:function_call / custom_tool_call)。
                // fork 复制的行原样保留 call_id → 聚合层按 call_id 去重,
                // 无需血缘时间窗截断
                let payload_type = obj.pointer("/payload/type").and_then(|t| t.as_str());
                if matches!(payload_type, Some("function_call") | Some("custom_tool_call")) {
                    if let Some(name) = obj.pointer("/payload/name").and_then(|v| v.as_str()) {
                        let call_id = obj
                            .pointer("/payload/call_id")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        let shell_cmd = codex_shell_cmd(name, &obj);
                        push_tool_events(
                            &mut tool_uses,
                            name,
                            shell_cmd.as_deref(),
                            line_ts,
                            call_id,
                        );
                    }
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
                // 血缘前缀跳过:早于本文件 session_meta 时刻的事件是 fork/resume
                // 复制来的父历史,已在父 rollout 计过账。差分基线 prev_total 已在
                // 上方随行推进,跳过不影响 fork 后增量的正确性
                if let (Some(ts), Some(meta)) = (line_ts, meta_ts) {
                    if ts < meta {
                        continue;
                    }
                }
                // 0.146+ 复制块截断:文件含 ≥2 个 session_meta 即带复制历史,
                // 复制行时间戳被改写为复制时刻(毫秒级突发簇,起于 meta 附近)。
                // 与簇尾间距 ≤ LINEAGE_GAP_MS 的行并入簇丢弃并推进簇尾;
                // 真实 turn 隔着一次模型往返的间隙,不会被并入
                if meta_count >= 2 {
                    if let (Some(ts), Some(end)) = (line_ts, lineage_cluster_end) {
                        if ts <= end + LINEAGE_GAP_MS {
                            lineage_cluster_end = Some(end.max(ts));
                            continue;
                        }
                    }
                }
                // 同一事件被重复写入(相邻行同时间戳同用量)只算一次;
                // 非相邻/跨文件的巧合碰撞不再误伤
                if turns
                    .last()
                    .is_some_and(|t| t.timestamp_ms == line_ts && t.usage == usage)
                {
                    continue;
                }
                turns.push(Turn {
                    // Codex rollout 无消息 id:重复计费已在上方按血缘前缀与相邻
                    // 重复处理,不再合成内容指纹参与聚合层去重(内容指纹会把
                    // 同毫秒同用量的两个真实 turn 误判为重复)
                    message_id: None,
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
        tool_uses,
    })
}

// ─── Grok Build 会话解析 ───────────────────────────────────────
//
// 计费口径来自 `updates.jsonl` 里的 xAI 扩展更新 `turn_completed`,它自带
// `usage`(整轮汇总 + 按模型分解),官方注释称其为「durable, replayable」——
// 就是为「重连后从重放里补齐回合结局」准备的,不是流式中间态。
//
// **不做工具排行**:持久化的 ACP `tool_call` 更新只带人类可读的 `title`
// (「Read file src/x.rs」这类),真正的工具名不落盘。拿 title 当工具名会往
// Claude/Codex 的工具排行里灌进一堆自然语言标签,不如不出——token/成本/模型/
// 会话四类统计不受影响。

/// 一个 `turn_completed` 里的 usage 行 → 互斥语义的 UsageTotals。
///
/// grok(ACP 口径)的 `inputTokens` 是**整段 prompt 输入**,缓存读与缓存写都
/// 折在里面;`outputTokens` 含 `reasoningTokens`。官方 headless 投影就是这么
/// 把三个输入桶拆成互斥的(`input − cacheRead − cacheCreation`),这里照抄,
/// 与 Claude/Codex 两侧的分桶语义对齐。
fn usage_from_grok(u: &serde_json::Value) -> UsageTotals {
    let full_input = u64_at(u, "inputTokens");
    let cache_read = u64_at(u, "cachedReadTokens");
    let cache_write = u64_at(u, "cacheCreationTokens");
    let raw_output = u64_at(u, "outputTokens");
    let reasoning = u64_at(u, "reasoningTokens");
    UsageTotals {
        input: full_input
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output: raw_output.saturating_sub(reasoning),
        reasoning,
        cache_read,
        cache_write,
        cache_write_1h: 0, // xAI 不区分缓存写的存活期
    }
}

/// 从一行 `updates.jsonl` 取出 `turn_completed` 的 (prompt_id, usage, 时刻)
fn grok_turn_completed(line: &str) -> Option<(Option<String>, serde_json::Value, Option<i64>)> {
    // 判别式在压缩 JSON 里逐字出现,先做个廉价预筛
    if !line.contains("\"turn_completed\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_i64())
        .map(|s| s * 1000);
    let params = v.get("params").unwrap_or(&v);
    let update = params.get("update")?;
    if update.get("sessionUpdate").and_then(|t| t.as_str()) != Some("turn_completed") {
        return None;
    }
    let usage = update.get("usage")?.clone();
    let prompt_id = update
        .get("prompt_id")
        .and_then(|p| p.as_str())
        .map(str::to_string);
    Some((prompt_id, usage, ts))
}

/// 解析一个 grok 会话目录(`summary.json` + `updates.jsonl`)。
///
/// 一个 `turn_completed` 可能跨多个模型(主模型 + 子代理/小模型),`modelUsage`
/// 按模型给出分解:每个模型出一条 Turn,模型维度的排行才不会全压在主模型上。
/// 缺 `modelUsage`(单模型回合)时退回整轮汇总,模型名取 summary 里的当前模型。
pub(crate) fn parse_grok_session(session_dir: &Path) -> Option<ParsedSession> {
    let session_id = session_dir.file_name()?.to_str()?.to_string();
    let updates = session_dir.join("updates.jsonl");
    let lines = read_lines(&updates)?;

    let summary: serde_json::Value = fs::read_to_string(session_dir.join("summary.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let cwd = summary
        .pointer("/info/cwd")
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let title = summary
        .get("session_summary")
        .and_then(|t| t.as_str())
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string);
    let default_model = summary
        .get("current_model_id")
        .and_then(|m| m.as_str())
        .map(str::to_string);

    let mut turns: Vec<Turn> = Vec::new();
    for line in &lines {
        let Some((prompt_id, usage, ts)) = grok_turn_completed(line) else {
            continue;
        };
        // 去重键:fork 会把父会话的 updates 整段复制进新会话目录,同一个回合
        // 因此会在两个文件里各出现一次。prompt_id 是回合的唯一标识,叠上模型名
        // 后逐条唯一,交给聚合层的跨会话 message_id 去重(与 Claude 同一条路)。
        let mut push = |model: Option<String>, usage: &serde_json::Value| {
            let totals = usage_from_grok(usage);
            if totals.total() == 0 {
                return; // 无计费的回合(纯本地/取消)不占 calls
            }
            let message_id = prompt_id.as_ref().map(|p| match &model {
                Some(m) => format!("{p}#{m}"),
                None => p.clone(),
            });
            turns.push(Turn {
                message_id,
                model,
                timestamp_ms: ts,
                usage: totals,
            });
        };

        match usage.get("modelUsage").and_then(|m| m.as_object()) {
            Some(rows) if !rows.is_empty() => {
                for (model, row) in rows {
                    push(Some(model.clone()), row);
                }
            }
            _ => push(default_model.clone(), &usage),
        }
    }

    Some(ParsedSession {
        agent: "grok",
        session_id,
        cwd,
        title,
        // 供应商归属由扫描层按 agent 归到 api.x.ai(会话本身不记 baseurl,
        // 与 Claude 同处境)
        provider: None,
        mtime_ms: mtime_ms(&updates),
        turns,
        tool_uses: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Grok Build ----

    /// ACP 口径的 `inputTokens` 是整段 prompt 输入(缓存读写都折在里面),
    /// `outputTokens` 含 reasoning。拆成互斥桶后必须与 `totalTokens` 对齐,
    /// 否则同一批 token 会被重复计价。
    #[test]
    fn grok_usage_buckets_are_disjoint_and_sum_to_total() {
        let usage = serde_json::json!({
            "inputTokens": 1000,   // = 未缓存 300 + 缓存读 600 + 缓存写 100
            "cachedReadTokens": 600,
            "cacheCreationTokens": 100,
            "outputTokens": 250,   // 含 reasoning 200
            "reasoningTokens": 200,
            "totalTokens": 1250
        });
        let t = usage_from_grok(&usage);
        assert_eq!(t.input, 300);
        assert_eq!(t.cache_read, 600);
        assert_eq!(t.cache_write, 100);
        assert_eq!(t.output, 50);
        assert_eq!(t.reasoning, 200);
        assert_eq!(t.total(), 1250, "分桶之和须等于 totalTokens");

        // 字段缺失/异常(缓存读大于输入)时钳到 0,不出现回绕的天文数字
        let weird = serde_json::json!({ "inputTokens": 10, "cachedReadTokens": 999 });
        assert_eq!(usage_from_grok(&weird).input, 0);
    }

    fn grok_session_fixture(tag: &str, updates: &str, summary: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mt-grok-usage-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let session = dir.join("0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("updates.jsonl"), updates).unwrap();
        fs::write(session.join("summary.json"), summary).unwrap();
        session
    }

    const GROK_SUMMARY: &str = r#"{"info":{"id":"0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e","cwd":"D:\\Git\\proj"},
        "session_summary":"接入 grok","created_at":"2026-08-01T10:00:00Z",
        "updated_at":"2026-08-01T11:00:00Z","current_model_id":"grok-4-1"}"#;

    /// 主路径:turn_completed 的 modelUsage 按模型各出一条 turn,
    /// 会话元信息取自 summary.json。
    #[test]
    fn grok_turn_completed_yields_per_model_turns() {
        let updates = concat!(
            // 非计费更新一律跳过
            r#"{"timestamp":1785000000,"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}"#,
            "\n",
            r#"{"timestamp":1785000060,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","stop_reason":"end_turn","usage":{"inputTokens":100,"outputTokens":20,"totalTokens":120,"modelUsage":{"grok-4-1":{"inputTokens":80,"outputTokens":16,"totalTokens":96},"grok-4-fast":{"inputTokens":20,"outputTokens":4,"totalTokens":24}}}}}}"#,
            "\n",
        );
        let dir = grok_session_fixture("permodel", updates, GROK_SUMMARY);
        let s = parse_grok_session(&dir).expect("应解析出会话");

        assert_eq!(s.agent, "grok");
        assert_eq!(s.session_id, "0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e");
        assert_eq!(s.cwd.as_deref(), Some(r"D:\Git\proj"));
        assert_eq!(s.title.as_deref(), Some("接入 grok"));
        assert_eq!(s.turns.len(), 2, "modelUsage 每个模型各出一条");

        let mut models: Vec<&str> = s.turns.iter().filter_map(|t| t.model.as_deref()).collect();
        models.sort();
        assert_eq!(models, ["grok-4-1", "grok-4-fast"]);
        // 时刻取信封的 unix 秒 ×1000
        assert!(s.turns.iter().all(|t| t.timestamp_ms == Some(1_785_000_060_000)));
        // 去重键含模型名:同一回合的两条不得互相吸收
        let keys: std::collections::HashSet<_> =
            s.turns.iter().filter_map(|t| t.message_id.clone()).collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("p1#grok-4-1"));

        fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    /// 单模型回合没有 modelUsage:退回整轮汇总,模型名取 summary 的当前模型;
    /// 零用量回合(取消/纯本地)不得虚增 calls。
    #[test]
    fn grok_falls_back_to_totals_and_drops_zero_usage() {
        let updates = concat!(
            r#"{"timestamp":1785000060,"method":"_x.ai/session/update","params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"p1","usage":{"inputTokens":50,"outputTokens":10,"totalTokens":60}}}}"#,
            "\n",
            r#"{"timestamp":1785000120,"method":"_x.ai/session/update","params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"p2","stop_reason":"cancelled","usage":{"inputTokens":0,"outputTokens":0,"totalTokens":0}}}}"#,
            "\n",
        );
        let dir = grok_session_fixture("totals", updates, GROK_SUMMARY);
        let s = parse_grok_session(&dir).unwrap();

        assert_eq!(s.turns.len(), 1, "零用量回合应被丢弃");
        assert_eq!(s.turns[0].model.as_deref(), Some("grok-4-1"));
        assert_eq!(s.turns[0].usage.input, 50);
        assert_eq!(s.turns[0].message_id.as_deref(), Some("p1#grok-4-1"));

        fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    /// updates.jsonl 缺失(会话刚建)不该 panic,坏行只跳过该行
    #[test]
    fn grok_tolerates_missing_and_broken_lines() {
        let updates = concat!(
            "not json at all\n",
            r#"{"params":{"update":{"sessionUpdate":"turn_completed"}}}"#,
            "\n",
            r#"{"params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"p9","usage":{"inputTokens":7,"totalTokens":7}}}}"#,
            "\n",
        );
        let dir = grok_session_fixture("broken", updates, "{}");
        let s = parse_grok_session(&dir).unwrap();
        assert_eq!(s.turns.len(), 1);
        assert_eq!(s.turns[0].usage.input, 7);
        // 无 summary 字段时模型未知,不能编一个
        assert!(s.turns[0].model.is_none());
        assert!(s.turns[0].timestamp_ms.is_none(), "无信封 timestamp 时留空");

        // 会话目录里没有 updates.jsonl:返回 None,不 panic
        let empty = dir.parent().unwrap().join("no-updates");
        fs::create_dir_all(&empty).unwrap();
        assert!(parse_grok_session(&empty).is_none());

        fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

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
    fn claude_zero_usage_turns_are_dropped() {
        // m1 全程 usage 全 0(无后续覆盖)→ 不是计费调用,不得虚增 calls;
        // m2 有计费维度 → 保留
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:00Z","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:01:00Z","message":{"id":"m2","model":"claude-opus-4-8","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":9}}}"#,
        ];
        let (turns, _, _, _) = claude_turns_from_lines(lines.iter().copied());
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].message_id.as_deref(), Some("m2"));
    }

    #[test]
    fn claude_usage_merges_same_message_id_keeping_larger_total() {
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:00Z","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:05Z","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":50,"cache_read_input_tokens":100}}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:01:00Z","message":{"id":"m2","model":"claude-opus-4-8","usage":{"input_tokens":5,"output_tokens":7}}}"#,
        ];
        let (turns, _, _, _) = claude_turns_from_lines(lines.iter().copied());
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
        let (_, _, cwd, title) = claude_turns_from_lines(lines.iter().copied());
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
    fn codex_subagent_rewritten_copy_block_not_double_counted_and_keeps_child_id() {
        // Codex 0.146+ subagent rollout 实测形状:child meta 与复制进来的
        // parent meta 同毫秒,复制的父历史 token_count 时间戳被改写为复制
        // 时刻(+1ms,晚于 meta)→ 仅靠 ts < meta_ts 挡不住;
        // 第二个 meta 也不得把会话身份覆盖成父 ID
        let root = std::env::temp_dir().join(format!(
            "mini-term-turns-subagent-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let child_meta = r#"{"type":"session_meta","timestamp":"2026-08-03T08:21:47.505Z","payload":{"id":"sess-child","cwd":"/p"}}"#;
        let parent_meta = r#"{"type":"session_meta","timestamp":"2026-08-03T08:21:47.505Z","payload":{"id":"sess-parent","cwd":"/p"}}"#;
        let copied = r#"{"type":"event_msg","timestamp":"2026-08-03T08:21:47.506Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}"#;
        let real = r#"{"type":"event_msg","timestamp":"2026-08-03T08:22:31.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":20,"output_tokens":9,"total_tokens":29}}}}"#;
        let f = root.join("rollout-child.jsonl");
        std::fs::write(&f, format!("{child_meta}\n{parent_meta}\n{copied}\n{real}\n")).unwrap();

        let p = parse_codex_session(&f, &HashMap::new()).unwrap();
        assert_eq!(p.session_id, "sess-child", "身份只认首个 meta,不得被父 meta 覆盖");
        assert_eq!(p.turns.len(), 1, "改写时间戳的复制历史不得重复计费");
        assert_eq!(p.turns[0].usage.input, 20);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn shell_main_command_extraction() {
        // §6.9:去 KEY=VAL 前缀 → 跳 wrapper → 首 token 截 basename
        assert_eq!(shell_main_command("git status"), Some("git".into()));
        assert_eq!(shell_main_command("FOO=1 BAR=2 npm run build"), Some("npm".into()));
        assert_eq!(shell_main_command("sudo env RUST_LOG=debug /usr/bin/cargo test"), Some("cargo".into()));
        assert_eq!(shell_main_command("nohup ./scripts/dev.sh &"), Some("dev.sh".into()));
        assert_eq!(shell_main_command("   "), None);
        assert_eq!(shell_main_command("sudo"), None);
        // 实测形状:Claude Bash 普遍 `cd X && 真命令`,cd 段跳过取下一段
        assert_eq!(shell_main_command("cd \"/Users/u/my proj\" && node validate.js"), Some("node".into()));
        assert_eq!(shell_main_command("cd /tmp && FOO=1 python3 run.py; echo done"), Some("python3".into()));
        // 带引号的可执行路径 → 引号内整体取 basename
        assert_eq!(
            shell_main_command(r#"cd /a && "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless"#),
            Some("Google Chrome".into())
        );
        // 管道/分号切段取首段
        assert_eq!(shell_main_command("rg -n foo | head -5"), Some("rg".into()));
        assert_eq!(shell_main_command("test -d .git && echo yes || echo no"), Some("test".into()));
    }

    #[test]
    fn claude_tool_uses_extracted_per_block_with_toolu_dedup_keys() {
        // 实测形状:每行恰一个 content 块,同 message.id 跨多行;工具事件按块自身
        // toolu id 去重(fork 复制原样保留 toolu id);Bash 追加 shell 首词;
        // mcp__server__tool 计 mcp(server),不进 tool 排行
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:00Z","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:01Z","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"tool_use","id":"toolu_A","name":"Read","input":{"file_path":"/a"}}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:02Z","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"tool_use","id":"toolu_B","name":"Bash","input":{"command":"cd /p && git log"}}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:01:00Z","message":{"id":"m2","model":"claude-opus-4-8","usage":{"input_tokens":1,"output_tokens":2},"content":[{"type":"tool_use","id":"toolu_C","name":"mcp__context7__query-docs","input":{}}]}}"#,
        ];
        let (turns, tool_uses, _, _) = claude_turns_from_lines(lines.iter().copied());
        assert_eq!(turns.len(), 2, "同 id 多行合并为一次计费调用");
        let got: Vec<(&str, &str, Option<&str>)> = tool_uses
            .iter()
            .map(|u| (u.kind, u.name.as_str(), u.dedup_key.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("tool", "Read", Some("toolu_A")),
                ("tool", "Bash", Some("toolu_B")),
                ("shell", "git", Some("toolu_B#s")),
                ("mcp", "context7", Some("toolu_C")),
            ]
        );
        assert_eq!(tool_uses[0].timestamp_ms, parse_rfc3339_ms("2026-08-01T10:00:01Z"));
    }

    #[test]
    fn codex_tool_calls_extracted_with_call_id_dedup_keys() {
        // 实测形状:response_item 的 function_call(arguments 为二次编码 JSON 串)
        // 与 custom_tool_call(name=exec,命令埋在 payload.input 的 JS 串里);
        // call_id 稳定,作跨文件去重键;mcp__ 前缀抽 server,其余计 tool
        let root = std::env::temp_dir().join(format!(
            "mini-term-turns-codextool-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let meta = r#"{"type":"session_meta","timestamp":"2026-08-01T09:00:00.000Z","payload":{"id":"sess-t","cwd":"/p","model_provider":"openai"}}"#;
        let exec = r#"{"type":"response_item","timestamp":"2026-08-01T09:01:00.000Z","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sed -n '1,10p' a.md\",\"workdir\":\"/p\"}","call_id":"call_1"}}"#;
        let shell_legacy = r#"{"type":"response_item","timestamp":"2026-08-01T09:02:00.000Z","payload":{"type":"function_call","name":"shell_command","arguments":"{\"command\":\"ls -la\",\"workdir\":\"/p\"}","call_id":"call_2"}}"#;
        let custom = r#"{"type":"response_item","timestamp":"2026-08-01T09:03:00.000Z","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_3","input":"const r = await tools.exec_command({\"cmd\":\"rg -n foo src\",\"workdir\":\"/p\"}); text(r.output)\n"}}"#;
        let mcp = r#"{"type":"response_item","timestamp":"2026-08-01T09:04:00.000Z","payload":{"type":"function_call","name":"mcp__context7__query-docs","arguments":"{}","call_id":"call_4"}}"#;
        let plain = r#"{"type":"response_item","timestamp":"2026-08-01T09:05:00.000Z","payload":{"type":"function_call","name":"update_plan","arguments":"{}","call_id":"call_5"}}"#;
        let f = root.join("rollout-t.jsonl");
        std::fs::write(&f, format!("{meta}\n{exec}\n{shell_legacy}\n{custom}\n{mcp}\n{plain}\n")).unwrap();

        let p = parse_codex_session(&f, &HashMap::new()).unwrap();
        let got: Vec<(&str, &str, Option<&str>)> = p
            .tool_uses
            .iter()
            .map(|u| (u.kind, u.name.as_str(), u.dedup_key.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("tool", "exec_command", Some("call_1")),
                ("shell", "sed", Some("call_1#s")),
                ("tool", "shell_command", Some("call_2")),
                ("shell", "ls", Some("call_2#s")),
                ("tool", "exec", Some("call_3")),
                ("shell", "rg", Some("call_3#s")),
                ("mcp", "context7", Some("call_4")),
                ("tool", "update_plan", Some("call_5")),
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_fast_real_turn_after_fork_is_kept() {
        // 复制块是毫秒级的突发写入（行间距 ~1ms）；真实 turn 与复制块之间必有
        // 一次模型往返的间隙。meta 后 1.5s 完成的快速真实 turn 不得被血缘截断误吞
        let root = std::env::temp_dir().join(format!(
            "mini-term-turns-fastreal-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let child_meta = r#"{"type":"session_meta","timestamp":"2026-08-03T08:21:47.505Z","payload":{"id":"sess-child","cwd":"/p"}}"#;
        let parent_meta = r#"{"type":"session_meta","timestamp":"2026-08-03T08:21:47.505Z","payload":{"id":"sess-parent","cwd":"/p"}}"#;
        let copied1 = r#"{"type":"event_msg","timestamp":"2026-08-03T08:21:47.506Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}"#;
        let copied2 = r#"{"type":"event_msg","timestamp":"2026-08-03T08:21:47.507Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}}"#;
        let fast_real = r#"{"type":"event_msg","timestamp":"2026-08-03T08:21:49.005Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":20,"output_tokens":9,"total_tokens":29}}}}"#;
        let f = root.join("rollout-child.jsonl");
        std::fs::write(&f, format!("{child_meta}\n{parent_meta}\n{copied1}\n{copied2}\n{fast_real}\n")).unwrap();

        let p = parse_codex_session(&f, &HashMap::new()).unwrap();
        assert_eq!(p.turns.len(), 1, "毫秒级复制簇必须整块丢弃，1.5s 后的真实 turn 必须保留");
        assert_eq!(p.turns[0].usage.input, 20);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_fork_replay_prefix_skipped_and_duplicates_absorbed() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-turns-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let meta_a = r#"{"type":"session_meta","timestamp":"2026-08-01T09:00:00.000Z","payload":{"id":"sess-a","cwd":"/p"}}"#;
        let l1 = r#"{"type":"event_msg","timestamp":"2026-08-01T10:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}"#;
        let l2 = r#"{"type":"event_msg","timestamp":"2026-08-01T10:00:05.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}}"#;
        // 同毫秒同用量的两个真实 turn:内容一致但都是新增计费,不得互相吞掉
        let l3 = r#"{"type":"event_msg","timestamp":"2026-08-01T11:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}}"#;

        let a = root.join("rollout-a.jsonl");
        std::fs::write(&a, format!("{meta_a}\n{l1}\n{l2}\n{l2}\n{l3}\n")).unwrap();
        // fork:新 rollout 原样复制父会话历史行(保留原时间戳),session_meta
        // 是 fork 创建时刻,晚于复制行
        let meta_b = r#"{"type":"session_meta","timestamp":"2026-08-01T12:00:00.000Z","payload":{"id":"sess-b","cwd":"/p"}}"#;
        let l4 = r#"{"type":"event_msg","timestamp":"2026-08-01T12:30:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":20,"output_tokens":9,"total_tokens":29}}}}"#;
        let b = root.join("rollout-b.jsonl");
        std::fs::write(&b, format!("{meta_b}\n{l1}\n{l2}\n{l4}\n")).unwrap();

        let names = HashMap::new();
        let pa = parse_codex_session(&a, &names).unwrap();
        let pb = parse_codex_session(&b, &names).unwrap();

        // 父会话:l1 + l2 + l3 计入;相邻重复写入的 l2 只算一次;
        // l3 与 l2 内容相同但非相邻,是真实 turn,不误伤
        assert_eq!(pa.turns.len(), 3);
        // fork 会话:复制的 l1/l2(早于 meta_b)被前缀跳过,只计 fork 后的 l4
        assert_eq!(pb.turns.len(), 1);
        assert_eq!(pb.turns[0].usage.input, 20);
        // 不再合成内容指纹,不参与聚合层去重
        assert!(pa.turns.iter().all(|t| t.message_id.is_none()));
    }
}
