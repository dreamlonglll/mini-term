//! pi 的 AI 历史面板接入:会话列表与正文读取。
//!
//! 定位(agent 目录 / 目录名编码 / 头部 cwd 校验)与行解析和 OMP 共用,见 `omp.rs`。
//! 这里只管历史面板要的两件事:
//!
//! - **列表**:项目桶里按 mtime 取最新若干个文件,头部校验 cwd。标题优先用 `/name` /
//!   `--name` 起的名字(`session_info` 条目),否则取第一条有文字的用户消息;模型取尾窗
//!   里最后一条 assistant 的 `message.model`;
//! - **正文**:pi 的记录是一棵树(`id` / `parentId`)。`/tree` 回到早先的消息再提交就
//!   长出新分支,被丢下的分支仍留在文件里,按文件顺序平铺会把两条岔路交错着读出来。
//!   这里只展示当前分支:从文件里最后一个条目(pi 重新打开会话时的叶子)沿
//!   `parentId` 走回根。对不上(老的线性格式没有 id、链断了)就退回文件顺序。
//!
//! 移动镜像是边写边读的增量解析,做不了「走回根」,仍按文件顺序(与 OMP 一致)。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::omp::{PiFlavor, meta_matches_project, omp_message_from_value, project_session_dirs};
use super::{
    AiSession, AiSessionMessage, MAX_SESSIONS_PER_SOURCE, find_pi_session_file,
    latest_model_from_lines, omp_session_meta_from_line, sort_newest_session_paths,
};

const MAX_PI_SESSION_FILES_TO_SCAN: usize = 300;
/// 标题与头部都在前几行:session 头、model_change、thinking_level_change、system、
/// 第一条 user。system 行带着整段系统提示词,但按行数算不吃亏。
const HEAD_LINES: usize = 50;
/// 最新的模型与改名只会在文件尾部(会话文件可达数 MB)
const TAIL_WINDOW: u64 = 64 * 1024;
const TITLE_MAX_CHARS: usize = 100;
const UNTITLED: &str = "Untitled";

/// Windows 宿主视角的 pi 会话扫描(只认项目根这个桶,口径同 [`crate::CwdBucket::PerCwd`];
/// 函数名沿用各家 `get_*_sessions` 的叫法,macOS / Linux 同样走这里)
pub(super) fn get_pi_sessions(project_path: &str) -> Vec<AiSession> {
    pi_sessions_in(
        &project_session_dirs(PiFlavor::Pi, project_path),
        project_path,
    )
}

fn pi_sessions_in(dirs: &[PathBuf], project_path: &str) -> Vec<AiSession> {
    let mut paths = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        paths.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|v| v.to_str()) == Some("jsonl")),
        );
    }
    sort_newest_session_paths(&mut paths, MAX_PI_SESSION_FILES_TO_SCAN);

    let mut sessions = Vec::new();
    for path in paths {
        if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }
        if let Some(session) = read_pi_session_info(&path, project_path) {
            sessions.push(session);
        }
    }
    sessions
}

fn read_pi_session_info(path: &Path, project_path: &str) -> Option<AiSession> {
    let file = fs::File::open(path).ok()?;
    let head: Vec<String> = BufReader::new(file)
        .lines()
        .take(HEAD_LINES)
        .filter_map(Result::ok)
        .collect();
    // 桶名可能撞车(`D:\a-b` 与 `D:\a\b` 编出同一个名字),以头部 cwd 为准
    let meta = head
        .iter()
        .find_map(|line| omp_session_meta_from_line(line))?;
    if !meta_matches_project(&meta, project_path) {
        return None;
    }
    let tail = read_tail(path);
    let (title, first_prompt_at) = pi_session_title(head.iter().map(String::as_str), tail.lines());
    Some(AiSession {
        id: meta.id,
        session_type: "pi".to_string(),
        title,
        // 与 Claude 同口径:第一条用户消息的时间,还没提问过的会话用建档时间
        timestamp: first_prompt_at.unwrap_or(meta.timestamp),
        model: latest_model_from_lines(tail.lines()),
        wsl_distro: None,
        ssh_connection_id: None,
    })
}

fn read_tail(path: &Path) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(TAIL_WINDOW)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    // 尾窗起点可能切进半行 / 多字节字符,那半行 JSON 解析失败自然跳过
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 会话标题 + 第一条用户消息的时间戳(行级纯函数,`head` / `tail` 都按文件顺序)。
///
/// 名字优先:尾窗里的 `session_info` 比头部的新(后来又 `/name` 改过名),名字被清空
/// (`name` 缺省 / 空串)就回落到首条用户消息。首条用户消息只在头部找。
fn pi_session_title<'a>(
    head: impl IntoIterator<Item = &'a str>,
    tail: impl IntoIterator<Item = &'a str>,
) -> (String, Option<String>) {
    let mut head_name: Option<Option<String>> = None;
    let mut first_prompt: Option<(String, String)> = None;
    for line in head {
        if let Some(name) = session_info_name(line) {
            head_name = Some(name);
        } else if first_prompt.is_none()
            && let Some(msg) = serde_json::from_str(line)
                .ok()
                .and_then(|obj| omp_message_from_value(&obj))
                .filter(|msg| msg.role == "user" && !msg.content.trim().is_empty())
        {
            first_prompt = Some((msg.content, msg.timestamp));
        }
    }
    let tail_name = tail.into_iter().filter_map(session_info_name).last();
    let name = tail_name.or(head_name).flatten();
    let first_prompt_at = first_prompt
        .as_ref()
        .map(|(_, ts)| ts.clone())
        .filter(|ts| !ts.is_empty());
    let title = name
        .or_else(|| first_prompt.map(|(text, _)| text))
        .map(|text| text.trim().chars().take(TITLE_MAX_CHARS).collect())
        .unwrap_or_else(|| UNTITLED.to_string());
    (title, first_prompt_at)
}

/// `session_info` 条目里的名字:不是该条目 → `None`;是,但名字被清空 → `Some(None)`。
fn session_info_name(line: &str) -> Option<Option<String>> {
    if !line.contains("\"session_info\"") {
        return None;
    }
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|v| v.as_str()) != Some("session_info") {
        return None;
    }
    Some(
        obj.get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
    )
}

pub(super) fn read_pi_session_content(
    session_id: &str,
    project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let path = find_pi_session_file(project_path, session_id)
        .ok_or_else(|| "未找到 pi 会话文件".to_string())?;
    let file = fs::File::open(&path).map_err(|e| format!("无法打开文件: {}", e))?;
    // 按字节切行再 lossy 解码:坏 UTF-8 只坏那一行(JSON 解析失败自然跳过),
    // 不中断迭代;真读盘出错才停(`lines()` + `filter_map` 遇持续 IO 错误会空转)
    let lines = BufReader::new(file)
        .split(b'\n')
        .map_while(Result::ok)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    Ok(pi_branch_messages(lines))
}

/// 当前分支上的对话消息,按文件顺序(行级纯函数,见模块注释)。
fn pi_branch_messages<S: AsRef<str>>(lines: impl IntoIterator<Item = S>) -> Vec<AiSessionMessage> {
    // 条目 id → parentId(None = 根)
    let mut parents: HashMap<String, Option<String>> = HashMap::new();
    let mut messages: Vec<(Option<String>, AiSessionMessage)> = Vec::new();
    let mut leaf: Option<String> = None;
    let mut linear = false;
    for line in lines {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.as_ref()) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("session") {
            continue;
        }
        let id = obj.get("id").and_then(|v| v.as_str()).map(str::to_string);
        match &id {
            Some(id) => {
                let parent = obj
                    .get("parentId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                parents.insert(id.clone(), parent);
                leaf = Some(id.clone());
            }
            // v1 线性格式没有 id(pi 打开时才迁移成树,没再打开过的老文件还是线性的)
            None => linear = true,
        }
        if let Some(msg) = omp_message_from_value(&obj) {
            messages.push((id, msg));
        }
    }
    let on_branch = match leaf.filter(|_| !linear) {
        Some(leaf) => branch_ids(&parents, leaf),
        None => None,
    };
    match on_branch {
        Some(on_branch) => messages
            .into_iter()
            .filter(|(id, _)| id.as_ref().is_some_and(|id| on_branch.contains(id)))
            .map(|(_, msg)| msg)
            .collect(),
        None => messages.into_iter().map(|(_, msg)| msg).collect(),
    }
}

/// 从叶子沿 `parentId` 走回根,返回路径上的全部 id;链断了(父条目不在文件里)
/// 或成环返回 `None`,调用方退回文件顺序——宁可多看到一条岔路,也不丢前文。
fn branch_ids(parents: &HashMap<String, Option<String>>, leaf: String) -> Option<HashSet<String>> {
    let mut on_branch = HashSet::new();
    let mut cursor = Some(leaf);
    while let Some(id) = cursor {
        let parent = parents.get(&id)?.clone();
        if !on_branch.insert(id) {
            return None;
        }
        cursor = parent;
    }
    Some(on_branch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, parent: Option<&str>, role: &str, text: &str) -> String {
        serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "timestamp": format!("t-{id}"),
            "message": { "role": role, "content": [{ "type": "text", "text": text }] }
        })
        .to_string()
    }

    fn texts(messages: &[AiSessionMessage]) -> Vec<&str> {
        messages.iter().map(|m| m.content.as_str()).collect()
    }

    /// 只读当前分支:`/tree` 回到 u1 之后另起的 u2b 分支是叶子,丢下的 u2a / a2a 不读。
    /// context_edit 这类非消息条目在链上照常串联(真实记录里重试前的空回复就靠它摘掉)。
    #[test]
    fn branch_messages_follow_the_leaf_back_to_root() {
        let lines = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"t0","cwd":"D:\\p"}"#.to_string(),
            r#"{"type":"model_change","id":"m0","parentId":null,"timestamp":"t","provider":"x","modelId":"y"}"#.to_string(),
            msg("sys", Some("m0"), "system", "prompt"),
            msg("u1", Some("sys"), "user", "first"),
            msg("a1", Some("u1"), "assistant", "answer 1"),
            msg("u2a", Some("a1"), "user", "abandoned"),
            msg("a2a", Some("u2a"), "assistant", "abandoned answer"),
            r#"{"type":"context_edit","id":"ce","parentId":"a1","timestamp":"t","targetId":"x","replacement":null}"#.to_string(),
            msg("u2b", Some("ce"), "user", "second"),
            msg("a2b", Some("u2b"), "assistant", "answer 2"),
        ];
        assert_eq!(
            texts(&pi_branch_messages(&lines)),
            ["first", "answer 1", "second", "answer 2"]
        );
    }

    /// 老的线性格式(没有 id)与断链都退回文件顺序,前文一条不丢。
    #[test]
    fn branch_messages_fall_back_to_file_order() {
        let linear = [
            r#"{"type":"message","timestamp":"t1","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"message","timestamp":"t2","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        ];
        assert_eq!(texts(&pi_branch_messages(linear)), ["hi", "hello"]);

        let dangling = [
            msg("u1", Some("gone"), "user", "q"),
            msg("a1", Some("u1"), "assistant", "a"),
        ];
        assert_eq!(texts(&pi_branch_messages(&dangling)), ["q", "a"]);
    }

    /// 标题:尾窗里最新的名字 > 头部的名字 > 首条有文字的用户消息;名字清空就回落。
    #[test]
    fn title_prefers_session_name_then_first_prompt() {
        let head = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"t0","cwd":"D:\\p"}"#.to_string(),
            msg("sys", None, "system", "system prompt"),
            msg("u0", Some("sys"), "user", "   "),
            msg("u1", Some("u0"), "user", "  修一下登录页  "),
        ];
        let head = || head.iter().map(String::as_str);
        assert_eq!(
            pi_session_title(head(), []),
            ("修一下登录页".to_string(), Some("t-u1".to_string()))
        );

        let named = r#"{"type":"session_info","id":"n1","parentId":"u1","timestamp":"t","name":"Refactor auth"}"#;
        let renamed =
            r#"{"type":"session_info","id":"n2","parentId":"n1","timestamp":"t","name":"Auth v2"}"#;
        let cleared = r#"{"type":"session_info","id":"n3","parentId":"n2","timestamp":"t"}"#;
        assert_eq!(pi_session_title(head(), [named]).0, "Refactor auth");
        assert_eq!(pi_session_title(head(), [named, renamed]).0, "Auth v2");
        assert_eq!(
            pi_session_title(head(), [renamed, cleared]).0,
            "修一下登录页"
        );
        // 头部的 `--name` 被尾窗里的改名盖过
        let with_head_name = [named.to_string(), msg("u1", None, "user", "x")];
        assert_eq!(
            pi_session_title(with_head_name.iter().map(String::as_str), [renamed]).0,
            "Auth v2"
        );

        let long = "长".repeat(TITLE_MAX_CHARS + 20);
        let only_long = [msg("u1", None, "user", &long)];
        let title = pi_session_title(only_long.iter().map(String::as_str), []).0;
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS);

        assert_eq!(pi_session_title([], []), (UNTITLED.to_string(), None));
    }

    /// 列表:只收头部 cwd 等于项目根的文件(桶名撞车的别家会话不进来),模型取尾窗。
    #[test]
    fn lists_sessions_of_the_project_only() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-pi-list-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bucket = root.join("--D--a-b--");
        fs::create_dir_all(&bucket).unwrap();
        let header = |id: &str, cwd: &str| {
            serde_json::json!({"type":"session","version":3,"id":id,"timestamp":"2026-10-27T06:18:17.742Z","cwd":cwd})
                .to_string()
        };
        let assistant = r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"t","message":{"role":"assistant","content":[{"type":"text","text":"ok"}],"model":"deepseek-flash"}}"#;
        fs::write(
            bucket.join("2026-10-27T06-18-17-742Z_01a17c02-730e-74db-ab0c-337bb007000f.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                header("01a17c02-730e-74db-ab0c-337bb007000f", r"D:\a-b"),
                msg("u1", None, "user", "分析协议"),
                assistant
            ),
        )
        .unwrap();
        fs::write(
            bucket.join("2026-10-27T06-00-00-000Z_other.jsonl"),
            format!("{}\n", header("other", r"D:\a\b")),
        )
        .unwrap();
        fs::write(bucket.join("notes.txt"), "not a session").unwrap();

        let sessions = pi_sessions_in(std::slice::from_ref(&bucket), r"d:\A-B\");
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "01a17c02-730e-74db-ab0c-337bb007000f");
        assert_eq!(s.session_type, "pi");
        assert_eq!(s.title, "分析协议");
        assert_eq!(s.timestamp, "t-u1");
        assert_eq!(s.model.as_deref(), Some("deepseek-flash"));
        fs::remove_dir_all(root).unwrap();
    }
}
