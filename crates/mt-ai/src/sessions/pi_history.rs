//! pi / oh-my-pi 的桌面历史列表与正文。项目桶、会话定位和消息解析见 `omp.rs`。
//!
//! 列表只读项目桶里最新文件的头部和尾窗。pi 读 `session_info`，OMP 读首行标题槽
//! 或旧文件的头部标题；都回落到首条用户消息，模型取尾窗。
//!
//! 正文沿活动叶子的 `parentId` 回到根；OMP 会记下非活动分支追加前的活动叶子。
//! 旧线性格式或断链仍按文件顺序展示。移动镜像独立做增量解析，不重建分支。
//!
//! OMP 的物理标题槽不是树条目，不能把后面的树误判成旧线性格式。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::omp::{
    PiFlavor, SessionHeader, SessionProject, find_session_file, omp_message_from_value,
    ordered_session_files, project_session_dirs, read_session_header, session_filename_matches,
    shared_sessions_root,
};
use super::{AiSession, AiSessionMessage, MAX_SESSIONS_PER_SOURCE, latest_model_from_lines};

const MAX_SESSION_FILES_TO_SCAN: usize = 300;
/// 标题与头部都在前几行:session 头、model_change、thinking_level_change、system、
/// 第一条 user。system 行带着整段系统提示词,但按行数算不吃亏。
const HEAD_LINES: usize = 50;
/// 最新的模型与改名只会在文件尾部(会话文件可达数 MB)
const TAIL_WINDOW: u64 = 64 * 1024;
const TITLE_MAX_CHARS: usize = 100;
const UNTITLED: &str = "Untitled";

/// 只读本机项目根对应的桶，口径同 [`crate::CwdBucket::PerCwd`]。
pub(super) fn get_pi_family_sessions(flavor: PiFlavor, project_path: &str) -> Vec<AiSession> {
    let Some(agent_dir) = flavor.agent_dir() else {
        return Vec::new();
    };
    let project = SessionProject::new(project_path);
    let dirs = project_session_dirs(flavor, &agent_dir, &project);
    pi_family_sessions_in(
        flavor,
        &dirs,
        &project,
        shared_sessions_root(flavor, &agent_dir),
    )
}

pub(super) fn pi_family_sessions_in(
    flavor: PiFlavor,
    dirs: &[PathBuf],
    project: &SessionProject<'_>,
    shared: bool,
) -> Vec<AiSession> {
    let mut sessions = Vec::new();
    let mut seen = HashSet::new();
    for (path, _) in ordered_session_files(dirs, None, MAX_SESSION_FILES_TO_SCAN) {
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let mut reader = BufReader::new(file);
        let Some(header) = read_session_header(&mut reader, &path, flavor, project, shared) else {
            continue;
        };
        // OMP 不把改过名的副本列为可续接会话；同 ID 仅展示按正文查找顺序排第一份的文件。
        if flavor == PiFlavor::Omp
            && (!session_filename_matches(&path, &header.meta.id)
                || !seen.insert(header.meta.id.clone()))
        {
            continue;
        }
        sessions.push(read_session_info(flavor, reader, header));
        if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }
    }
    sessions
}

fn read_session_info(
    flavor: PiFlavor,
    mut reader: BufReader<fs::File>,
    header: SessionHeader,
) -> AiSession {
    let SessionHeader {
        meta,
        mut lines,
        header_idx,
    } = header;
    let remaining = HEAD_LINES.saturating_sub(lines.len());
    lines.extend(
        reader
            .by_ref()
            .lines()
            .take(remaining)
            .filter_map(Result::ok),
    );
    let omp_name = if flavor == PiFlavor::Omp {
        omp_session_title(&lines[0], &lines[header_idx])
    } else {
        None
    };
    let tail = read_tail(reader.into_inner());
    let (title, first_prompt_at) =
        session_title(lines.iter().map(String::as_str), tail.lines(), omp_name);
    AiSession {
        id: meta.id,
        session_type: flavor.kind().key().to_string(),
        title,
        // 与 Claude 同口径:第一条用户消息的时间,还没提问过的会话用建档时间
        timestamp: first_prompt_at.unwrap_or(meta.timestamp),
        model: latest_model_from_lines(tail.lines()),
        wsl_distro: None,
        ssh_connection_id: None,
    }
}

fn read_tail(mut file: fs::File) -> String {
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(TAIL_WINDOW)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::with_capacity(len.min(TAIL_WINDOW) as usize);
    let _ = file.read_to_end(&mut bytes);
    // 尾窗起点可能切进半行 / 多字节字符,那半行 JSON 解析失败自然跳过
    String::from_utf8(bytes)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

/// `Some(None)` 表示标题槽存在但已清空，不回落到头部旧标题。
fn omp_session_title(first_line: &str, header: &str) -> Option<Option<String>> {
    let title = |obj: &serde_json::Value| {
        obj.get("title")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    };
    let first: serde_json::Value = serde_json::from_str(first_line).ok()?;
    if first.get("type").and_then(|value| value.as_str()) == Some("title") {
        return Some(title(&first));
    }
    if first_line == header {
        title(&first).map(Some)
    } else {
        title(&serde_json::from_str(header).ok()?).map(Some)
    }
}

/// 会话标题 + 第一条用户消息的时间戳(行级纯函数,`head` / `tail` 都按文件顺序)。
///
/// OMP 标题优先于首条用户消息。没有标题槽或旧头部标题时，沿用 pi 的名字规则：
/// 尾窗里的 `session_info` 优先于头部，清空名字则显示首条用户消息。
fn session_title<'a>(
    head: impl IntoIterator<Item = &'a str>,
    tail: impl IntoIterator<Item = &'a str>,
    omp_name: Option<Option<String>>,
) -> (String, Option<String>) {
    let mut head_name: Option<Option<String>> = None;
    let mut first_prompt: Option<(String, String)> = None;
    for line in head {
        if omp_name.is_none()
            && let Some(name) = session_info_name(line)
        {
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
    let name = omp_name.unwrap_or_else(|| {
        tail.into_iter()
            .filter_map(session_info_name)
            .last()
            .or(head_name)
            .flatten()
    });
    let (first_prompt, first_prompt_at) = first_prompt.unzip();
    let first_prompt_at = first_prompt_at.filter(|ts| !ts.is_empty());
    let title = name
        .or(first_prompt)
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

pub(super) fn read_pi_family_session_content(
    flavor: PiFlavor,
    session_id: &str,
    project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let path = find_session_file(flavor, project_path, session_id)
        .ok_or_else(|| format!("未找到 {} 会话文件", flavor.kind().key()))?;
    read_session_messages(flavor, &path)
}

fn read_session_messages(flavor: PiFlavor, path: &Path) -> Result<Vec<AiSessionMessage>, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法打开文件: {e}"))?;
    // 按字节切行再 lossy 解码:坏 UTF-8 只坏那一行(JSON 解析失败自然跳过),
    // 不中断迭代;真读盘出错才停(`lines()` + `filter_map` 遇持续 IO 错误会空转)
    let lines = BufReader::new(file)
        .split(b'\n')
        .map_while(Result::ok)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    Ok(branch_messages(flavor, lines))
}

/// 当前分支上的对话消息,按文件顺序(行级纯函数,见模块注释)。
fn branch_messages<S: AsRef<str>>(
    flavor: PiFlavor,
    lines: impl IntoIterator<Item = S>,
) -> Vec<AiSessionMessage> {
    // 条目 id → parentId(None = 根)
    let mut parents: HashMap<String, Option<String>> = HashMap::new();
    let mut messages: Vec<(Option<String>, AiSessionMessage)> = Vec::new();
    let mut leaf: Option<String> = None;
    let mut last_self_selected = None;
    let mut cleared = false;
    let mut linear = false;
    for line in lines {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.as_ref()) else {
            continue;
        };
        if matches!(
            obj.get("type").and_then(|v| v.as_str()),
            Some("session" | "title")
        ) {
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
                let saved_leaf = if flavor == PiFlavor::Omp {
                    obj.get("activeLeafId")
                } else {
                    None
                };
                cleared = saved_leaf.is_some_and(serde_json::Value::is_null);
                leaf = if cleared {
                    None
                } else if let Some(active) = saved_leaf.and_then(serde_json::Value::as_str) {
                    if active == id {
                        last_self_selected = Some(id.clone());
                    }
                    Some(active.to_owned())
                } else {
                    last_self_selected = Some(id.clone());
                    Some(id.clone())
                };
            }
            // v1 线性格式没有 id(pi 打开时才迁移成树,没再打开过的老文件还是线性的)
            None => linear = true,
        }
        if let Some(msg) = omp_message_from_value(&obj) {
            messages.push((id, msg));
        }
    }
    if cleared {
        return Vec::new();
    }
    // OMP rewrite/rollback may remove the saved leaf; replay falls back to the
    // newest entry that selected itself, not the off-branch append.
    if flavor == PiFlavor::Omp && leaf.as_ref().is_some_and(|id| !parents.contains_key(id)) {
        if last_self_selected.is_none() && !linear {
            return Vec::new();
        }
        leaf = last_self_selected;
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
            texts(&branch_messages(PiFlavor::Pi, &lines)),
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
        assert_eq!(
            texts(&branch_messages(PiFlavor::Pi, linear)),
            ["hi", "hello"]
        );

        let dangling = [
            msg("u1", Some("gone"), "user", "q"),
            msg("a1", Some("u1"), "assistant", "a"),
        ];
        assert_eq!(texts(&branch_messages(PiFlavor::Pi, &dangling)), ["q", "a"]);
    }

    #[test]
    fn omp_preview_replays_saved_leaf_not_last_append() {
        let root = msg("root", None, "user", "start");
        let abandoned = msg("old", Some("root"), "assistant", "abandoned");
        let active = msg("new", Some("root"), "assistant", "active");
        let preview = |saved: serde_json::Value| {
            let late_usage = serde_json::json!({
                "type": "model_usage", "id": "usage", "parentId": "old", "activeLeafId": saved,
            })
            .to_string();
            branch_messages(PiFlavor::Omp, [&root, &abandoned, &active, &late_usage])
        };
        assert_eq!(texts(&preview("new".into())), ["start", "active"]);
        assert_eq!(texts(&preview("removed".into())), ["start", "active"]);
        assert!(preview(serde_json::Value::Null).is_empty());
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
            session_title(head(), [], None),
            ("修一下登录页".to_string(), Some("t-u1".to_string()))
        );

        let named = r#"{"type":"session_info","id":"n1","parentId":"u1","timestamp":"t","name":"Refactor auth"}"#;
        let renamed =
            r#"{"type":"session_info","id":"n2","parentId":"n1","timestamp":"t","name":"Auth v2"}"#;
        let cleared = r#"{"type":"session_info","id":"n3","parentId":"n2","timestamp":"t"}"#;
        assert_eq!(session_title(head(), [named], None).0, "Refactor auth");
        assert_eq!(session_title(head(), [named, renamed], None).0, "Auth v2");
        assert_eq!(
            session_title(head(), [renamed, cleared], None).0,
            "修一下登录页"
        );
        // 头部的 `--name` 被尾窗里的改名盖过
        let with_head_name = [named.to_string(), msg("u1", None, "user", "x")];
        assert_eq!(
            session_title(with_head_name.iter().map(String::as_str), [renamed], None).0,
            "Auth v2"
        );

        let long = "长".repeat(TITLE_MAX_CHARS + 20);
        let only_long = [msg("u1", None, "user", &long)];
        let title = session_title(only_long.iter().map(String::as_str), [], None).0;
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS);

        assert_eq!(session_title([], [], None), (UNTITLED.to_string(), None));
    }

    /// 列表:只收头部 cwd 等于项目根的文件(桶名撞车的别家会话不进来),模型取尾窗。
    #[test]
    fn lists_only_project_sessions_and_reads_the_omp_branch() {
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

        let dirs = [bucket.clone()];
        let sessions =
            pi_family_sessions_in(PiFlavor::Pi, &dirs, &SessionProject::new(r"d:\A-B\"), false);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "01a17c02-730e-74db-ab0c-337bb007000f");
        assert_eq!(s.session_type, "pi");
        assert_eq!(s.title, "分析协议");
        assert_eq!(s.timestamp, "t-u1");
        assert_eq!(s.model.as_deref(), Some("deepseek-flash"));

        let id = "01a17c02-730e-74db-ab0c-337bb007000f";
        let cwd = r"D:\a-b";
        let file = bucket.join(format!("2026-10-27T06-18-17-742Z_{id}.jsonl"));
        let mut old_header: serde_json::Value = serde_json::from_str(&header(id, cwd)).unwrap();
        old_header["title"] = serde_json::json!("Old title");
        let user = msg("u1", None, "user", "First prompt");
        let abandoned = msg("a0", Some("u1"), "assistant", "Abandoned answer");
        let slot = |title: &str| {
            let mut record = serde_json::json!({
                "type": "title", "v": 1, "title": title, "source": "user",
                "updatedAt": "2026-10-27T06:19:00.000Z", "pad": "",
            });
            record["pad"] = serde_json::json!(" ".repeat(255 - record.to_string().len()));
            format!("{record}\n")
        };
        fs::copy(&file, bucket.join("renamed-copy.jsonl")).unwrap();
        for (preamble, expected_title) in [
            (slot("New title"), "New title"),
            (slot(""), "First prompt"),
            (String::new(), "Old title"),
        ] {
            fs::write(
                &file,
                format!("{preamble}{old_header}\n{user}\n{abandoned}\n{assistant}\n"),
            )
            .unwrap();
            let sessions =
                pi_family_sessions_in(PiFlavor::Omp, &dirs, &SessionProject::new(cwd), false);
            assert_eq!(sessions.len(), 1);
            let session = &sessions[0];
            assert_eq!(session.id, id);
            assert_eq!(session.session_type, "omp");
            assert_eq!(session.title, expected_title);
            assert_eq!(session.timestamp, "t-u1");
            assert_eq!(session.model.as_deref(), Some("deepseek-flash"));
            assert_eq!(
                texts(&read_session_messages(PiFlavor::Omp, &file).unwrap()),
                ["First prompt", "ok"]
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn omp_copies_share_the_same_list_and_preview_selection() {
        use super::super::omp::{find_session_file_in, session_dirs};
        let root = std::env::temp_dir().join(format!("mt-omp-copies-{}", uuid::Uuid::new_v4()));
        let project_dir = root.join("project");
        fs::create_dir_all(&project_dir).unwrap();
        let cwd = project_dir.to_str().unwrap();
        let project = SessionProject::new(cwd);
        let sessions_root = root.join("sessions");
        let primary = sessions_root.join("-project");
        let legacy = sessions_root.join(format!(
            "--{}--",
            cwd.trim_start_matches(['/', '\\'])
                .replace(['/', '\\', ':'], "-")
        ));
        fs::create_dir_all(&primary).unwrap();
        fs::create_dir_all(&legacy).unwrap();
        let header = |id: &str, title: &str| {
            serde_json::json!({
                "type": "session", "id": id, "cwd": cwd, "title": title,
            })
        };
        let older = primary.join("2026-01-01T00-00-00-000Z_copy.jsonl");
        let newer = legacy.join("2026-01-01T00-00-00-000Z_copy.jsonl");
        for (file, title, age) in [(&older, "older", 1), (&newer, "newer", 2)] {
            fs::write(
                file,
                format!(
                    "{}\n{}\n",
                    header("copy", title),
                    msg("u", None, "user", title)
                ),
            )
            .unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(age))
                .unwrap();
        }
        let dirs = session_dirs(
            PiFlavor::Omp,
            &sessions_root,
            &root,
            &root.join("tmp"),
            &project,
        );
        let rows = pi_family_sessions_in(PiFlavor::Omp, &dirs, &project, false);
        assert_eq!(
            rows.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            ["newer"]
        );
        let file =
            find_session_file_in(PiFlavor::Omp, &dirs, &project, &rows[0].id, false).unwrap();
        assert_eq!(
            texts(&read_session_messages(PiFlavor::Omp, &file).unwrap()),
            ["newer"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn omp_lists_and_opens_a_project_through_an_alias() {
        use super::super::omp::{find_session_file_in, session_dirs};
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("mt-omp-alias-{}", uuid::Uuid::new_v4()));
        let real = root.join("real");
        let alias = root.join("alias");
        fs::create_dir_all(&real).unwrap();
        symlink(&real, &alias).unwrap();
        let canonical = mt_core::path::canonicalize(&real).unwrap();
        let sessions_root = root.join("sessions");
        let bucket = sessions_root.join("-real");
        fs::create_dir_all(&bucket).unwrap();
        for (id, cwd) in [("real", &canonical), ("alias", &alias)] {
            let header = serde_json::json!({"type": "session", "id": id, "cwd": cwd});
            let prompt = msg("u", None, "user", id);
            fs::write(
                bucket.join(format!("t_{id}.jsonl")),
                format!("{header}\n{prompt}\n"),
            )
            .unwrap();
        }
        for path in [&canonical, &alias] {
            let project = SessionProject::new(path.to_str().unwrap());
            let dirs = session_dirs(
                PiFlavor::Omp,
                &sessions_root,
                &root,
                &root.join("tmp"),
                &project,
            );
            let rows = pi_family_sessions_in(PiFlavor::Omp, &dirs, &project, false);
            assert_eq!(rows.len(), 2);
            for row in rows {
                let file =
                    find_session_file_in(PiFlavor::Omp, &dirs, &project, &row.id, false).unwrap();
                assert_eq!(
                    texts(&read_session_messages(PiFlavor::Omp, &file).unwrap()),
                    [row.id.as_str()]
                );
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_absolute_bucket_lists_only_known_owners_and_resolves_their_files() {
        use super::super::omp::find_session_file_in;
        let root = std::env::temp_dir().join(format!("mt-pi-shared-{}", uuid::Uuid::new_v4()));
        let dirs = [root.join("--D--project--")];
        fs::create_dir_all(&dirs[0]).unwrap();
        let cwd = r"D:\project";
        let project = SessionProject::new(cwd);
        for (id, record) in [
            ("modern-omp", r#"{"type":"title","v":1,"title":"OMP"}"#),
            (
                "legacy-omp",
                r#"{"type":"model_change","model":"provider/model"}"#,
            ),
            (
                "pi",
                r#"{"type":"model_change","provider":"provider","modelId":"model"}"#,
            ),
            ("unknown", r#"{"type":"custom"}"#),
        ] {
            let header = serde_json::json!({"type": "session", "id": id, "cwd": cwd});
            let contents = if id == "modern-omp" {
                format!("{record}\n{header}\n")
            } else {
                format!("{header}\n{record}\n")
            };
            fs::write(dirs[0].join(format!("t_{id}.jsonl")), contents).unwrap();
        }
        for (flavor, expected, other) in [
            (PiFlavor::Pi, &["pi"][..], "legacy-omp"),
            (PiFlavor::Omp, &["legacy-omp", "modern-omp"][..], "pi"),
        ] {
            let rows = pi_family_sessions_in(flavor, &dirs, &project, true);
            let mut ids: Vec<_> = rows.iter().map(|s| s.id.as_str()).collect();
            ids.sort_unstable();
            assert_eq!(ids, expected);
            for id in ids {
                assert!(find_session_file_in(flavor, &dirs, &project, id, true).is_some());
            }
            for id in [other, "unknown"] {
                assert!(find_session_file_in(flavor, &dirs, &project, id, true).is_none());
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
}
