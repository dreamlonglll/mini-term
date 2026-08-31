//! ===== 会话分支链路（session lineage，设计: docs/plans/2026-08-14-session-branch-tree-design.md）=====

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::codex::{
    codex_meta_from_line, codex_user_title_from_line, collect_codex_session_paths,
    find_codex_session_file,
};
use super::{
    MAX_CODEX_SESSION_FILES_TO_SCAN, PathStyle, claude_session_info_from_lines,
    find_claude_project_dirs_in, home_dir, session_id_path_safe, sort_newest_session_paths,
};

/// Claude 分支会话的复制行从文件头就带 forkedFrom（首行即含），普通会话则整
/// 个文件都没有；前部可能垫着若干 summary/meta 行，因此扫头部一段而非只看首行。
const CLAUDE_LINEAGE_HEAD_LINES: usize = 100;

/// 会话分支边：session_id fork 自 parent_session_id。
/// agent 字段随边携带（"claude" | "codex"），新 agent 只需产出同构边即可入树。
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LineageEdge {
    pub agent: String,
    pub session_id: String,
    pub parent_session_id: String,
    /// 分叉点在父会话中的消息 uuid，仅 Claude 有此精度
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_point_uuid: Option<String>,
    /// 分支自己的首条用户消息（分叉之后第一问）。fork 是整份复制，标题字段
    /// 会连同首条消息一起继承自根会话——分支之间全都同名；真正区分一条分支
    /// 的是它岔开后干了什么。None = 分支里还没提过问（UI 回落会话标题）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_title: Option<String>,
}

/// 自记账的会话分支边(mini-term 自己发起的 fork 当场记下 child→parent)。
///
/// 与 [`LineageEdge`] 同构、独立定义:原型是 `config::SavedLineageEdge`,
/// 那边是配置序列化面的类型,不该让扫描模块反过来依赖配置模块(反之亦然)。
/// 迁移后配置在 mt-config、扫描在这里,两个 crate 各持一份同构定义,由上层转换。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookkeptLineageEdge {
    pub agent: String,
    pub session_id: String,
    pub parent_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_point_uuid: Option<String>,
}

/// 从 Claude 会话 jsonl 头部行中提取分支边。行级纯函数。
/// 取首个 `forkedFrom: {sessionId, messageUuid}`；坏行跳过继续。
pub fn claude_fork_edge_from_lines<'a>(
    session_id: &str,
    lines: impl Iterator<Item = &'a str>,
) -> Option<LineageEdge> {
    for line in lines.take(CLAUDE_LINEAGE_HEAD_LINES) {
        // 便宜的字符串预筛：绝大多数行不含该键，省掉 JSON 解析
        if !line.contains("\"forkedFrom\"") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(ff) = obj.get("forkedFrom") else {
            continue;
        };
        let parent = ff.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
        if parent.is_empty() || parent == session_id {
            continue;
        }
        return Some(LineageEdge {
            agent: "claude".to_string(),
            session_id: session_id.to_string(),
            parent_session_id: parent.to_string(),
            fork_point_uuid: ff
                .get("messageUuid")
                .and_then(|v| v.as_str())
                .map(String::from),
            branch_title: None,
        });
    }
    None
}

/// 从 Codex session_meta 行中提取分支边。行级纯函数。
/// subagent 线程（thread_source == "subagent"）也带 forked_from_id，但那是主会
/// 话派生的子 agent 而非用户分支，丢弃；自身 id 取 payload.id —— fork 场景下
/// payload.session_id 是根线程 id，不可用作自身身份。
pub fn codex_fork_edge_from_meta_line(line: &str) -> Option<LineageEdge> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    if obj
        .pointer("/payload/thread_source")
        .and_then(|v| v.as_str())
        == Some("subagent")
    {
        return None;
    }
    let id = obj.pointer("/payload/id").and_then(|v| v.as_str())?;
    let parent = obj
        .pointer("/payload/forked_from_id")
        .and_then(|v| v.as_str())?;
    if id.is_empty() || parent.is_empty() || parent == id {
        return None;
    }
    Some(LineageEdge {
        agent: "codex".to_string(),
        session_id: id.to_string(),
        parent_session_id: parent.to_string(),
        fork_point_uuid: None,
        // branch_title 由 scan_codex_lineage 用父会话前缀比对补齐(fork 复制
        // 历史时连时间戳一起重写,行级标记与时间戳判据都不可用)
        branch_title: None,
    })
}

/// 文件里全部「真实用户输入」的标题序列（与 codex_user_title_from_line 同口径：
/// 跳过 `<...>` 系统注入与 AGENTS 前缀，截断 100 字符——两侧同口径截断，等值比较成立）。
fn codex_user_texts(path: &Path, cap: usize) -> Vec<String> {
    let Ok(file) = fs::File::open(path) else {
        return vec![];
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        // 便宜预筛:用户输入行才含 input_text(assistant 是 output_text)
        if !line.contains("input_text") {
            continue;
        }
        if let Some(t) = codex_user_title_from_line(&line) {
            out.push(t);
            if out.len() >= cap {
                break;
            }
        }
    }
    out
}

/// 分支自己的第一问：子会话的用户消息按顺序前缀匹配父会话，首条对不上的
/// 即分叉后的第一条自己的提问。纯函数，Claude / Codex 共用 —— 它对落盘格式
/// 零假设（Claude 的 forkedFrom 标记只有 /branch 路径写，CLI fork 不写，
/// 实测两条路径行为不一致，行级标记不可依赖）。
/// None = 子消息全是复制来的（分支还没提问）。
pub fn branch_title_from_texts(parent: &[String], child: &[String]) -> Option<String> {
    let mut idx = 0;
    for text in child {
        if idx < parent.len() && &parent[idx] == text {
            idx += 1;
            continue;
        }
        return Some(text.clone());
    }
    None
}

/// 分支自己的首条用户消息：过滤掉从父会话复制来的 forkedFrom 行后，取首条
/// 用户消息（复用 claude_session_info_from_lines 的提取口径）。行级纯函数。
/// None = 分支里还没有自己的提问。
pub fn claude_branch_title_from_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let own_lines = lines.into_iter().filter(|l| !l.contains("\"forkedFrom\""));
    let (title, _) = claude_session_info_from_lines(own_lines);
    (title != "Untitled").then_some(title)
}

/// 全文件读取版（只对确认是分支的文件调用；非分支文件不付这份 IO）。
fn claude_branch_title(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
    claude_branch_title_from_lines(lines.iter().map(String::as_str))
}

/// 从行迭代器（按文件顺序）提取**最后一个**模型名。Claude 的 assistant 行在
/// `message.model`，Codex 的 turn_context 行在 `payload.model`，一个口径通吃；
/// `<synthetic>`（错误占位）与空串不算。行级纯函数。
pub fn latest_model_from_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut model = None;
    for line in lines {
        if !line.contains("\"model\"") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let m = obj
            .pointer("/message/model")
            .or_else(|| obj.pointer("/payload/model"))
            .and_then(|v| v.as_str());
        if let Some(m) = m {
            if !m.is_empty() && !m.starts_with('<') {
                model = Some(m.to_string());
            }
        }
    }
    model
}

/// 最新模型只会在文件尾部：读尾窗而不是整个文件（会话文件可达数 MB）。
/// 尾窗起点可能切进半行/多字节字符，lossy 转换后那半行 JSON 解析自然失败跳过。
const MODEL_TAIL_WINDOW: u64 = 64 * 1024;

pub(super) fn latest_model_from_file_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(MODEL_TAIL_WINDOW)))
        .ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    latest_model_from_lines(text.lines())
}

/// Claude 文件里全部「真实用户输入」的标题序列（跳过 `<...>` 系统注入，
/// 截断 100 字符 —— 两侧同口径截断，等值比较成立）。含 forkedFrom 复制行：
/// 复制行的文本与父会话逐条相同，前缀比对天然消化，无需过滤。
fn claude_user_texts(path: &Path, cap: usize) -> Vec<String> {
    let Ok(file) = fs::File::open(path) else {
        return vec![];
    };
    let mut out = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("\"type\":\"user\"") {
            continue;
        }
        rest.clear();
        rest.push(line);
        let (title, _) = claude_session_info_from_lines(rest.iter().map(String::as_str));
        if title != "Untitled" {
            out.push(title);
            if out.len() >= cap {
                break;
            }
        }
    }
    out
}

/// 在项目的 Claude 桶目录里按 id 找会话文件。
/// id 会拼进路径,而自记账边的来源是前端持久化 config(不可信输入),
/// 白名单同 lookup_ai_session_cwd,防 `../` 一类拼接。
fn find_claude_session_file(project_dirs: &[PathBuf], session_id: &str) -> Option<PathBuf> {
    if !session_id_path_safe(session_id) {
        return None;
    }
    let name = format!("{session_id}.jsonl");
    project_dirs
        .iter()
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

fn scan_claude_lineage(project_path: &str) -> Vec<LineageEdge> {
    let Some(home) = home_dir() else {
        return vec![];
    };
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.exists() {
        return vec![];
    }
    let dirs = find_claude_project_dirs_in(&projects_dir, project_path, PathStyle::Windows);
    let mut edges = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in &dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            let Ok(file) = fs::File::open(&path) else {
                continue;
            };
            let head: Vec<String> = BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .take(CLAUDE_LINEAGE_HEAD_LINES)
                .collect();
            if let Some(mut edge) =
                claude_fork_edge_from_lines(&id, head.iter().map(String::as_str))
            {
                // 标题主路:与父会话前缀比对(格式零假设);父文件已清理时
                // 退回 forkedFrom 过滤(仅 /branch 路径的文件有标记)
                edge.branch_title = find_claude_session_file(&dirs, &edge.parent_session_id)
                    .map(|pp| claude_user_texts(&pp, 300))
                    .and_then(|pt| branch_title_from_texts(&pt, &claude_user_texts(&path, 300)))
                    .or_else(|| claude_branch_title(&path));
                edges.push(edge);
            }
        }
    }
    edges
}

fn scan_codex_lineage(project_path: &str) -> Vec<LineageEdge> {
    let Some(home) = home_dir() else {
        return vec![];
    };
    let sessions_dir = home.join(".codex").join("sessions");
    if !sessions_dir.exists() {
        return vec![];
    }
    let normalized_project = PathStyle::Windows.normalize(project_path);
    let mut paths = Vec::new();
    collect_codex_session_paths(&sessions_dir, &mut paths);
    sort_newest_session_paths(&mut paths, MAX_CODEX_SESSION_FILES_TO_SCAN);
    let mut edges = Vec::new();
    for path in paths {
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        // 与 try_read_codex_session 同口径：前 5 行找 session_meta，cwd 精确匹配本项目
        for line in BufReader::new(file).lines().map_while(Result::ok).take(5) {
            let Some(meta) = codex_meta_from_line(&line) else {
                continue;
            };
            if PathStyle::Windows.normalize(&meta.cwd) == normalized_project {
                if let Some(mut edge) = codex_fork_edge_from_meta_line(&line) {
                    // 分支标题:父会话用户消息序列做前缀比对,首条对不上的即
                    // 分叉后第一问(父文件已清理时拿不到,回落会话标题)
                    edge.branch_title =
                        find_codex_session_file(&sessions_dir, &edge.parent_session_id)
                            .map(|pp| codex_user_texts(&pp, 300))
                            .and_then(|pt| {
                                branch_title_from_texts(&pt, &codex_user_texts(&path, 300))
                            });
                    edges.push(edge);
                }
            }
            break;
        }
    }
    edges
}

/// 扫描项目的会话分支链路。Claude 消息级（forkedFrom）+ Codex 会话级
/// （forked_from_id）；Grok 的 summary.json parent 引用与 WSL / SSH 远程来源
/// 暂不参与（能力位预留，见设计文档）。
///
/// `bookkept` = 前端自记账边（mini-term 自己发起的 fork）。Claude 的 CLI fork
/// 不写任何磁盘指针（forkedFrom 只有 /branch 路径写），这些边只存在于自记账，
/// 标题必须在这里补：按 agent 找到父子文件做同一套前缀比对后并入返回；
/// 已有磁盘边的 child 不重复。
pub fn scan_session_lineage(
    project_path: String,
    bookkept: Option<Vec<BookkeptLineageEdge>>,
) -> Vec<LineageEdge> {
    let mut edges = scan_claude_lineage(&project_path);
    edges.extend(scan_codex_lineage(&project_path));
    if let Some(extra) = bookkept {
        enrich_bookkept_edges(&project_path, extra, &mut edges);
    }
    edges
}

fn enrich_bookkept_edges(
    project_path: &str,
    extra: Vec<BookkeptLineageEdge>,
    edges: &mut Vec<LineageEdge>,
) {
    let have: std::collections::HashSet<String> =
        edges.iter().map(|e| e.session_id.clone()).collect();
    let home = home_dir();
    let claude_dirs: Vec<PathBuf> = home
        .as_ref()
        .map(|h| {
            let pd = h.join(".claude").join("projects");
            if pd.exists() {
                find_claude_project_dirs_in(&pd, project_path, PathStyle::Windows)
            } else {
                vec![]
            }
        })
        .unwrap_or_default();
    let codex_dir = home.map(|h| h.join(".codex").join("sessions"));
    for e in extra {
        if have.contains(&e.session_id) {
            continue;
        }
        let branch_title = if e.agent.to_lowercase() == "codex" {
            codex_dir.as_ref().filter(|d| d.exists()).and_then(|d| {
                let child = find_codex_session_file(d, &e.session_id)?;
                let parent = find_codex_session_file(d, &e.parent_session_id)?;
                branch_title_from_texts(
                    &codex_user_texts(&parent, 300),
                    &codex_user_texts(&child, 300),
                )
            })
        } else {
            // claude 系(含 hook 上报的 claude-code)
            (|| {
                let child = find_claude_session_file(&claude_dirs, &e.session_id)?;
                let parent = find_claude_session_file(&claude_dirs, &e.parent_session_id)?;
                branch_title_from_texts(
                    &claude_user_texts(&parent, 300),
                    &claude_user_texts(&child, 300),
                )
            })()
        };
        edges.push(LineageEdge {
            agent: e.agent,
            session_id: e.session_id,
            parent_session_id: e.parent_session_id,
            fork_point_uuid: e.fork_point_uuid,
            branch_title,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 会话分支链路 ----

    #[test]
    fn find_claude_session_file_rejects_non_whitelist_id() {
        // 自记账边 id 来自前端持久化 config,拼路径前必须过白名单
        let dirs = vec![PathBuf::from("/nonexistent")];
        assert!(find_claude_session_file(&dirs, "../../etc/passwd").is_none());
        assert!(find_claude_session_file(&dirs, "a b").is_none());
        assert!(find_claude_session_file(&dirs, "").is_none());
    }

    #[test]
    fn claude_fork_edge_takes_first_forked_from() {
        let lines = [
            r#"{"type":"summary","summary":"t","leafUuid":"x"}"#,
            r#"{"uuid":"u1","sessionId":"child","forkedFrom":{"sessionId":"parent","messageUuid":"m1"},"type":"user"}"#,
            r#"{"uuid":"u2","sessionId":"child","forkedFrom":{"sessionId":"other","messageUuid":"m2"},"type":"user"}"#,
        ];
        let edge = claude_fork_edge_from_lines("child", lines.iter().copied()).unwrap();
        assert_eq!(edge.agent, "claude");
        assert_eq!(edge.session_id, "child");
        assert_eq!(edge.parent_session_id, "parent");
        assert_eq!(edge.fork_point_uuid.as_deref(), Some("m1"));
    }

    #[test]
    fn claude_fork_edge_none_without_pointer_and_skips_bad_lines() {
        let plain = [r#"{"uuid":"u1","sessionId":"s","type":"user"}"#];
        assert!(claude_fork_edge_from_lines("s", plain.iter().copied()).is_none());
        // 坏 JSON 行含关键字也不 panic，继续找到后面的合法行
        let mixed = [
            r#"{"forkedFrom": 截断坏行"#,
            r#"{"forkedFrom":{"sessionId":"p"},"type":"user"}"#,
        ];
        let edge = claude_fork_edge_from_lines("c", mixed.iter().copied()).unwrap();
        assert_eq!(edge.parent_session_id, "p");
        assert_eq!(edge.fork_point_uuid, None);
    }

    #[test]
    fn claude_fork_edge_rejects_self_and_empty_parent() {
        let self_ref = [r#"{"forkedFrom":{"sessionId":"c","messageUuid":"m"}}"#];
        assert!(claude_fork_edge_from_lines("c", self_ref.iter().copied()).is_none());
        let empty = [r#"{"forkedFrom":{"sessionId":"","messageUuid":"m"}}"#];
        assert!(claude_fork_edge_from_lines("c", empty.iter().copied()).is_none());
    }

    #[test]
    fn claude_fork_edge_ignores_beyond_head_cap() {
        let mut lines: Vec<String> = (0..CLAUDE_LINEAGE_HEAD_LINES)
            .map(|i| format!(r#"{{"uuid":"u{i}","type":"user"}}"#))
            .collect();
        lines.push(r#"{"forkedFrom":{"sessionId":"p","messageUuid":"m"}}"#.to_string());
        assert!(claude_fork_edge_from_lines("c", lines.iter().map(String::as_str)).is_none());
    }

    #[test]
    fn codex_fork_edge_from_meta() {
        let line = r#"{"type":"session_meta","payload":{"id":"child","session_id":"root","forked_from_id":"parent","thread_source":"user","cwd":"/p"}}"#;
        let edge = codex_fork_edge_from_meta_line(line).unwrap();
        assert_eq!(edge.agent, "codex");
        // 自身 id 必须取 payload.id，不能取 payload.session_id（fork 下那是根线程 id）
        assert_eq!(edge.session_id, "child");
        assert_eq!(edge.parent_session_id, "parent");
        assert_eq!(edge.fork_point_uuid, None);
    }

    #[test]
    fn codex_fork_edge_skips_subagent_and_missing_pointer() {
        let subagent = r#"{"type":"session_meta","payload":{"id":"c","forked_from_id":"p","thread_source":"subagent"}}"#;
        assert!(codex_fork_edge_from_meta_line(subagent).is_none());
        let no_pointer = r#"{"type":"session_meta","payload":{"id":"c","cwd":"/p"}}"#;
        assert!(codex_fork_edge_from_meta_line(no_pointer).is_none());
        let self_ref = r#"{"type":"session_meta","payload":{"id":"c","forked_from_id":"c"}}"#;
        assert!(codex_fork_edge_from_meta_line(self_ref).is_none());
        let not_meta = r#"{"type":"response_item","payload":{"id":"c","forked_from_id":"p"}}"#;
        assert!(codex_fork_edge_from_meta_line(not_meta).is_none());
    }

    #[test]
    fn branch_title_prefix_matches_parent() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // 复制来的前缀(你好)匹配父序列,第一条对不上的是分支自己的第一问
        assert_eq!(
            branch_title_from_texts(&s(&["你好", "改走流式"]), &s(&["你好", "试试批处理"]))
                .as_deref(),
            Some("试试批处理"),
        );
        // 从中间分叉:子只复制了父的前缀
        assert_eq!(
            branch_title_from_texts(&s(&["a", "b", "c"]), &s(&["a", "分支问题"])).as_deref(),
            Some("分支问题"),
        );
        // 分支还没提问 → None
        assert_eq!(
            branch_title_from_texts(&s(&["a", "b"]), &s(&["a", "b"])),
            None
        );
        assert_eq!(branch_title_from_texts(&s(&["a"]), &s(&[])), None);
    }

    #[test]
    fn latest_model_takes_last_and_skips_synthetic() {
        let lines = [
            r#"{"type":"assistant","message":{"model":"claude-opus-5","usage":{}}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.2-codex"}}"#,
            r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{}}}"#,
        ];
        // 取最后一个合法命中(<synthetic> 不算),message.model 与 payload.model 一个口径
        assert_eq!(
            latest_model_from_lines(lines.iter().copied()).as_deref(),
            Some("gpt-5.2-codex"),
        );
        assert_eq!(
            latest_model_from_lines([r#"{"type":"user"}"#].iter().copied()),
            None
        );
    }

    #[test]
    fn claude_branch_title_skips_copied_lines() {
        // 复制行（带 forkedFrom）里的首条用户消息是根会话的标题，必须跳过；
        // 标题取分叉后第一条自己的提问
        let lines = [
            r#"{"type":"user","forkedFrom":{"sessionId":"p","messageUuid":"m"},"message":{"role":"user","content":"根会话的第一问"}}"#,
            r#"{"type":"assistant","forkedFrom":{"sessionId":"p","messageUuid":"m2"},"message":{"role":"assistant","content":"回答"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"分支里改走流式方案"},"timestamp":"2026-08-15T10:00:00Z"}"#,
        ];
        assert_eq!(
            claude_branch_title_from_lines(lines.iter().copied()).as_deref(),
            Some("分支里改走流式方案"),
        );
        // 只有复制行（分支还没提问）→ None
        assert_eq!(
            claude_branch_title_from_lines(lines[..2].iter().copied()),
            None
        );
        // 系统注入(< 开头)不算标题
        let injected = [
            r#"{"type":"user","message":{"role":"user","content":"<local-command-caveat>x</local-command-caveat>"}}"#,
        ];
        assert_eq!(
            claude_branch_title_from_lines(injected.iter().copied()),
            None
        );
    }
}
