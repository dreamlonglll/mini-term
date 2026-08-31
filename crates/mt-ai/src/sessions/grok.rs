//! ─── Grok Build 会话 ────────────────────────────────────────────
//!
//! 磁盘布局与另外两家都不同:一个会话是一**整个目录**,不是一个文件。
//!
//!   {grok_home}/sessions/{编码后的 cwd}/{session-id}/
//!       summary.json     标题/模型/时间戳/消息数,会话身份的索引
//!       updates.jsonl    ACP 会话更新流(对话正文的权威来源)
//!
//! 组目录名是 cwd 的 URL 编码;编码超过 255 字节时退化成 `{slug}-{hash16}`,
//! 并在目录内写一个 `.cwd` 记下原始路径。我们**解码**目录名去比项目路径,
//! 而不是编码项目路径去比目录名 —— 后者要逐字复刻 grok 所用 urlencoding
//! crate 的转义集(以及未来的任何调整),前者对两种编码形态一并成立。

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::{AiSession, AiSessionMessage, MAX_SESSIONS_PER_SOURCE, normalize_path};

/// grok 会话根目录:`{$GROK_HOME | ~/.grok}/sessions`
fn grok_sessions_dir() -> Option<PathBuf> {
    crate::hook_registry::grok_home().map(|h| h.join("sessions"))
}

/// 百分号解码(只处理 `%XX`;grok 的 urlencoding 不产出 `+` 代空格)。
/// 非法转义原样保留,交给调用方的形态校验兜底。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 还原组目录代表的 cwd。
///
/// 先试 URL 解码:解出来的绝对路径必然以 `/` 开头或第二字符是 `:`(盘符),
/// 而 `{slug}-{hash}` 形态两者都不满足,据此无歧义地区分两种编码;
/// 后者回落读目录内的 `.cwd`。
pub fn decode_grok_cwd_dir(dir: &Path) -> Option<String> {
    let name = dir.file_name()?.to_str()?;
    let decoded = percent_decode(name);
    let looks_absolute = decoded.starts_with('/') || decoded.chars().nth(1) == Some(':');
    if looks_absolute {
        return Some(decoded);
    }
    fs::read_to_string(dir.join(".cwd"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 项目对应的 grok 组目录(通常 0 或 1 个;大小写/分隔符差异会各成一个目录)
pub fn find_grok_cwd_dirs(project_path: &str) -> Vec<PathBuf> {
    let Some(sessions_dir) = grok_sessions_dir() else {
        return Vec::new();
    };
    let normalized = normalize_path(project_path);
    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| decode_grok_cwd_dir(p).is_some_and(|cwd| normalize_path(&cwd) == normalized))
        .collect()
}

/// 会话目录里的 `updates.jsonl`;不存在(会话刚建、正文未落盘)返回 None
pub fn grok_updates_path(session_dir: &Path) -> Option<PathBuf> {
    let path = session_dir.join("updates.jsonl");
    path.is_file().then_some(path)
}

/// 按 session id 在项目的组目录里定位会话目录
pub fn find_grok_session_dir(project_path: &str, session_id: &str) -> Option<PathBuf> {
    find_grok_cwd_dirs(project_path)
        .into_iter()
        .map(|group| group.join(session_id))
        .find(|p| p.is_dir())
}

/// summary.json 的关键字段(grok 的 `Summary` 结构是 snake_case)
struct GrokSummary {
    id: String,
    title: String,
    timestamp: String,
    model: Option<String>,
}

fn read_grok_summary(session_dir: &Path) -> Option<GrokSummary> {
    let raw = fs::read_to_string(session_dir.join("summary.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    // id 以目录名为准:summary.json 里的 info.id 是同一个值,但目录名是我们
    // 后续拼路径的依据,两者万一不一致要以能定位到文件的那个为准。
    let id = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .or_else(|| {
            v.pointer("/info/id")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })?;
    let timestamp = v
        .get("updated_at")
        .or_else(|| v.get("created_at"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let title = v
        .get("session_summary")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("(无标题)")
        .to_string();
    let model = v
        .get("current_model_id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(GrokSummary {
        id,
        title,
        timestamp,
        model,
    })
}

/// Windows 宿主视角的 grok 会话扫描
pub(super) fn get_grok_sessions(project_path: &str) -> Vec<AiSession> {
    let mut sessions = Vec::new();
    for group in find_grok_cwd_dirs(project_path) {
        let Ok(entries) = fs::read_dir(&group) else {
            continue;
        };
        for entry in entries.flatten() {
            if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
                return sessions;
            }
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if let Some(s) = read_grok_summary(&dir) {
                sessions.push(AiSession {
                    id: s.id,
                    session_type: "grok".to_string(),
                    title: s.title,
                    timestamp: s.timestamp,
                    model: s.model,
                    wsl_distro: None,
                    ssh_connection_id: None,
                });
            }
        }
    }
    sessions
}

/// `updates.jsonl` 一行的语义分类
enum GrokUpdate {
    /// 用户消息分片(连续多行属于同一条消息)
    UserChunk { text: String, timestamp: u64 },
    /// AI 回复分片
    AgentChunk { text: String, timestamp: u64 },
    /// 其余更新:工具调用、计划、回合收尾…… 都是消息边界
    Boundary,
}

/// 解析一行 `updates.jsonl`。
///
/// 行的形态是 `{"timestamp":<unix秒>,"method":"…","params":{…}}`,
/// 旧版本没有信封、整行就是 params。`params.update.sessionUpdate` 是判别式。
/// xAI 扩展轨(`method` 为 `_x.ai/session/update`)与 ACP 轨共用同一个判别式键,
/// 但扩展轨里没有消息分片,一律当边界。
fn parse_grok_update(line: &str) -> GrokUpdate {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return GrokUpdate::Boundary;
    };
    let is_xai = v.get("method").and_then(|m| m.as_str()) == Some("_x.ai/session/update");
    let timestamp = v.get("timestamp").and_then(|t| t.as_u64()).unwrap_or(0);
    let params = v.get("params").unwrap_or(&v);
    let Some(update) = params.get("update") else {
        return GrokUpdate::Boundary;
    };
    if is_xai {
        return GrokUpdate::Boundary;
    }
    let tag = update.get("sessionUpdate").and_then(|t| t.as_str());
    let text = || {
        let content = update.get("content")?;
        if content.get("type").and_then(|t| t.as_str()) != Some("text") {
            return None;
        }
        content
            .get("text")
            .and_then(|t| t.as_str())
            .map(str::to_string)
    };
    match tag {
        Some("user_message_chunk") => {
            // `content._meta.bashCommand` = `!bash` 直通命令的回显,
            // `_meta.hostTurn` = 宿主注入的回合(工具结果/系统提醒),
            // 两者都不是用户说的话 —— grok 自己的提示词抽取也这么排除。
            let injected = update.pointer("/content/_meta/bash_command").is_some()
                || update.pointer("/content/_meta/bashCommand").is_some()
                || update.pointer("/_meta/hostTurn").and_then(|v| v.as_bool()) == Some(true);
            match text() {
                Some(t) if !injected => GrokUpdate::UserChunk { text: t, timestamp },
                _ => GrokUpdate::Boundary,
            }
        }
        Some("agent_message_chunk") => match text() {
            Some(t) => GrokUpdate::AgentChunk { text: t, timestamp },
            None => GrokUpdate::Boundary,
        },
        _ => GrokUpdate::Boundary,
    }
}

/// unix 秒 → ISO 8601(前端与 MirrorMessage 都按 ISO 字符串排序/展示)
fn unix_to_iso(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// `updates.jsonl` 的增量解析器。
///
/// 与 Claude/Codex 的「一行一条消息」不同,grok 把一条消息拆成任意多个
/// `*_message_chunk` 行(流式落盘)。必须攒到边界才成一条消息,否则镜像里
/// 一句回答会碎成几十条。边界 = 任何非同角色分片的行(工具调用、回合收尾、
/// 对方开口),与 grok 自身的会话重放口径一致。
pub struct GrokUpdateParser {
    role: Option<&'static str>,
    buf: String,
    started_at: u64,
}

impl GrokUpdateParser {
    pub fn new() -> Self {
        Self {
            role: None,
            buf: String::new(),
            started_at: 0,
        }
    }

    /// 喂入一行,返回**被这一行收尾**的上一条消息(如果有)
    pub fn feed_line(&mut self, line: &str) -> Option<AiSessionMessage> {
        let (role, text, ts) = match parse_grok_update(line) {
            GrokUpdate::UserChunk { text, timestamp } => ("user", text, timestamp),
            GrokUpdate::AgentChunk { text, timestamp } => ("assistant", text, timestamp),
            GrokUpdate::Boundary => return self.flush(),
        };
        if self.role == Some(role) {
            self.buf.push_str(&text);
            return None;
        }
        let done = self.flush();
        self.role = Some(role);
        self.buf = text;
        self.started_at = ts;
        done
    }

    /// 冲出仍在缓冲区里的消息(读到文件尾时调)
    pub fn flush(&mut self) -> Option<AiSessionMessage> {
        let role = self.role.take()?;
        let content = std::mem::take(&mut self.buf);
        let started_at = std::mem::take(&mut self.started_at);
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(AiSessionMessage {
            role: role.to_string(),
            content: trimmed.to_string(),
            timestamp: unix_to_iso(started_at),
        })
    }
}

fn read_grok_messages_from_file(path: &Path) -> Result<Vec<AiSessionMessage>, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法打开文件: {}", e))?;
    let reader = BufReader::new(file);
    let mut parser = GrokUpdateParser::new();
    let mut messages = Vec::new();
    for line in reader.lines() {
        // 坏行只跳过该行,不中断迭代(与 Claude 路径同因)
        let Ok(line) = line else { continue };
        if let Some(m) = parser.feed_line(&line) {
            messages.push(m);
        }
    }
    if let Some(m) = parser.flush() {
        messages.push(m);
    }
    Ok(messages)
}

pub(super) fn read_grok_session_content(
    session_id: &str,
    project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let dir = find_grok_session_dir(project_path, session_id)
        .ok_or_else(|| "未找到 Grok 会话目录".to_string())?;
    let path = grok_updates_path(&dir).ok_or_else(|| "会话正文尚未落盘".to_string())?;
    read_grok_messages_from_file(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Grok Build ----

    fn grok_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mt-grok-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 组目录名是 cwd 的 URL 编码;我们解码它去比项目路径,而不是反过来编码
    /// 项目路径(那要逐字复刻 grok 所用 crate 的转义集)。
    #[test]
    fn grok_cwd_dir_decodes_url_encoded_names() {
        let root = grok_temp_dir("decode");
        for (name, expect) in [
            ("D%3A%5CGit%5Cmini-term", r"D:\Git\mini-term"),
            ("%2Fhome%2Fu%2Fproj", "/home/u/proj"),
            // 非 ASCII 与空格
            (
                "D%3A%5CGit%5Cbhyt-%E4%B8%80%E4%BD%93%E6%9C%BA",
                r"D:\Git\bhyt-一体机",
            ),
            ("%2Fhome%2Fu%2Fmy%20proj", "/home/u/my proj"),
        ] {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            assert_eq!(decode_grok_cwd_dir(&dir).as_deref(), Some(expect), "{name}");
        }
        fs::remove_dir_all(&root).ok();
    }

    /// 超长 cwd 退化成 `{slug}-{hash}`,原路径写在目录内的 `.cwd` 里
    #[test]
    fn grok_cwd_dir_falls_back_to_dot_cwd_file() {
        let root = grok_temp_dir("hashdir");
        let dir = root.join("mini-term-0123456789abcdef");
        fs::create_dir_all(&dir).unwrap();
        // 没有 .cwd 时无从还原:宁可不认,也不能瞎猜成项目路径
        assert!(decode_grok_cwd_dir(&dir).is_none());

        fs::write(dir.join(".cwd"), "D:\\Git\\very-long-path\n").unwrap();
        assert_eq!(
            decode_grok_cwd_dir(&dir).as_deref(),
            Some(r"D:\Git\very-long-path")
        );
        fs::remove_dir_all(&root).ok();
    }

    fn grok_chunk(tag: &str, text: &str, ts: u64) -> String {
        format!(
            r#"{{"timestamp":{ts},"method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"{tag}","content":{{"type":"text","text":"{text}"}}}}}}}}"#
        )
    }

    /// 分片合并 + 边界收尾 + 角色切换即边界
    #[test]
    fn grok_parser_merges_chunks_and_splits_on_boundaries() {
        let mut p = GrokUpdateParser::new();
        assert!(
            p.feed_line(&grok_chunk("user_message_chunk", "hello ", 100))
                .is_none()
        );
        assert!(
            p.feed_line(&grok_chunk("user_message_chunk", "world", 101))
                .is_none()
        );
        // 角色切换本身就是边界:上一条在此收尾
        let user = p
            .feed_line(&grok_chunk("agent_message_chunk", "hi", 102))
            .expect("用户消息应在角色切换处收尾");
        assert_eq!(user.role, "user");
        assert_eq!(user.content, "hello world");
        assert_eq!(user.timestamp, "1970-01-01T00:01:40Z", "应取首个分片的时刻");

        // 文件读完时冲出尾部消息
        let agent = p.flush().expect("尾部消息应被冲出");
        assert_eq!(agent.role, "assistant");
        assert_eq!(agent.content, "hi");
        assert!(p.flush().is_none(), "重复 flush 不该产出");
    }

    /// 旧格式(无 method/timestamp 信封,整行就是 params)必须照样解析
    #[test]
    fn grok_parser_accepts_legacy_envelope_free_lines() {
        let mut p = GrokUpdateParser::new();
        let legacy = r#"{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"legacy"}}}"#;
        assert!(p.feed_line(legacy).is_none());
        let msg = p.flush().unwrap();
        assert_eq!(msg.content, "legacy");
        assert_eq!(msg.timestamp, "", "无 timestamp 字段时留空,不能编一个");
    }

    /// 非文本内容(图片)、坏行、纯空白消息都不该产出条目
    #[test]
    fn grok_parser_skips_noise() {
        let mut p = GrokUpdateParser::new();
        for line in [
            "not json at all",
            r#"{"params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"image","data":"…"}}}}"#,
            r#"{"params":{"update":{"sessionUpdate":"plan"}}}"#,
            r#"{"params":{}}"#,
        ] {
            assert!(p.feed_line(line).is_none(), "{line} 不该产出消息");
        }
        assert!(p.flush().is_none());

        // 只有空白的消息也丢掉
        let mut q = GrokUpdateParser::new();
        q.feed_line(&grok_chunk("agent_message_chunk", "   ", 1));
        assert!(q.flush().is_none());
    }

    #[test]
    fn grok_summary_json_yields_session_entry() {
        let root = grok_temp_dir("summary");
        let dir = root.join("0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("summary.json"),
            r#"{"info":{"id":"0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e","cwd":"D:\\Git\\proj"},
                "session_summary":"修 PTY 背压","created_at":"2026-08-01T10:00:00Z",
                "updated_at":"2026-08-01T11:30:00Z","num_messages":12,"current_model_id":"grok-4"}"#,
        )
        .unwrap();

        let s = read_grok_summary(&dir).expect("应解析出会话");
        assert_eq!(s.id, "0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e");
        assert_eq!(s.title, "修 PTY 背压");
        assert_eq!(s.timestamp, "2026-08-01T11:30:00Z", "列表按更新时刻排序");

        // 无标题(标题还没生成完)不能让整条会话消失
        fs::write(
            dir.join("summary.json"),
            r#"{"created_at":"2026-08-01T10:00:00Z"}"#,
        )
        .unwrap();
        let s = read_grok_summary(&dir).expect("缺字段也应给出条目");
        assert_eq!(s.title, "(无标题)");
        assert_eq!(
            s.timestamp, "2026-08-01T10:00:00Z",
            "缺 updated_at 时回落 created_at"
        );

        fs::remove_dir_all(&root).ok();
    }
}
