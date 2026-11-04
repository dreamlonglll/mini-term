//! 删除历史会话的原始记录(AI 历史面板右键「删除会话」)。
//!
//! # 一个列表条目对应磁盘上的什么
//!
//! ```text
//! Claude  {home}/.claude/projects/{桶}/{id}.jsonl     列表认的就是它
//!         {home}/.claude/projects/{桶}/{id}/          同名附属目录(subagents、tool-results)
//!         桶可能不止一个(尾部斜杠 / 大小写变体),列表按 id 去重,删要把每个桶都删掉
//! Codex   {home}/.codex/sessions/**/*.jsonl           头部 session_meta 的 id + cwd
//! Grok    {grok_home}/sessions/{组}/{id}/             整个目录;组同样可能不止一个
//! pi      {agent_dir}/sessions/--{路径}--/*.jsonl     头部 session 行的 id + cwd
//! ```
//!
//! 定位口径与各家的列表扫描逐条对齐:列表凭什么认出这一行,这里就凭什么找文件,
//! 否则删完刷新,条目又从漏掉的那份文件里复活。OMP 没接历史面板,没有条目可删。
//!
//! **只动会话记录本身。** 各家的旁路数据(Claude 的 `history.jsonl` / `file-history`、
//! Codex 的 `session_index.jsonl` 与 sqlite、Grok 组目录的 `prompt_history.jsonl`)是
//! 对方进程私有且可能正在写的结构,列表也不靠它们认条目,一律不碰。
//!
//! # 分支口径
//!
//! Claude / Codex / pi 的 fork 都是把历史**整份复制**进自己的文件(pi 头部的
//! `parentSession` 只是出处),孩子不依赖父文件:删父不连带删子、删子不动父,分支树那边
//! 「悬空父按根处理」接得住。pi 的 `parentId` 树在**一个文件之内**,条目对应整个文件,
//! 里面的分支随文件一起没。
//!
//! # 两步走
//!
//! [`plan_session_delete`] 只读:找全目标、校验,结果给确认框展示;用户点了确定才
//! [`execute_session_delete`]。第二步**全部复核通过才动第一刀**:路径校验重来一遍
//! (确认框开着的时候符号链接可能被换),文件变过就拒绝(AI 可能正在写,用户确认的
//! 也不是这一份内容了)。
//!
//! 两步都是阻塞 IO(Codex 要扫全部 rollout 头部,WSL 走 9P),调用方丢后台。

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::agent::AgentKind;

use super::codex::{codex_meta_from_line, collect_codex_session_paths};
use super::grok::find_grok_cwd_dirs_in;
use super::omp::{PiFlavor, meta_matches_project, project_session_dirs_in, session_meta};
use super::{
    PathStyle, derive_wsl_target, find_claude_project_dirs_in, home_dir, invalidate_session_cache,
    session_id_path_safe, wsl_candidate_homes,
};

/// 多近的写入算「可能还有 AI 在用」。只用来提示:hook 没开时拿不到 pane 上的会话
/// 身份,终端外跑的 CLI 更看不见,文件的修改时间是仅剩的线索。
const RECENT_WRITE_WINDOW: Duration = Duration::from_secs(60);

/// grok 列表靠它认条目(见 `grok.rs::read_grok_summary`)。
const GROK_SUMMARY_FILE: &str = "summary.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDeleteError {
    /// id 不过白名单(要拼进路径,见 [`session_id_path_safe`])。
    InvalidId,
    /// 这个来源 / agent 没有删除能力(见 [`session_delete_supported`])。
    Unsupported,
    /// 磁盘上已经没有这个会话了。
    NotFound,
    /// 目标不是预期的形态:在会话目录之外、是符号链接、类型不对。
    Unsafe(String),
    /// 确认之后文件变过:可能有 AI 正在写。
    Changed(String),
    Io {
        path: String,
        message: String,
    },
    /// 远程一侧的错误(`mt-remote` 的错误口径是现成的文案串)。
    Remote(String),
}

/// 右键菜单要不要出「删除会话」。与列表的来源口径一一对应:WSL / SSH 远程来源只扫
/// Claude 与 Codex,Grok / pi 只有宿主来源。
pub fn session_delete_supported(
    session_type: &str,
    wsl_distro: Option<&str>,
    ssh_connection_id: Option<&str>,
) -> bool {
    match AgentKind::parse(session_type) {
        Some(AgentKind::Claude | AgentKind::Codex) => true,
        Some(AgentKind::Grok | AgentKind::Pi) => {
            wsl_distro.is_none() && ssh_connection_id.is_none()
        }
        _ => false,
    }
}

/// `written_at` 离 `now` 够不够近(见 [`RECENT_WRITE_WINDOW`])。修改时间比 `now` 还新
/// 只可能是时钟有偏差(远程机器),按「刚写过」算 —— 这里宁可多提示一句。
pub fn written_recently_at(written_at: Option<SystemTime>, now: SystemTime) -> bool {
    written_at.is_some_and(|t| {
        now.duration_since(t)
            .map_or(true, |age| age < RECENT_WRITE_WINDOW)
    })
}

/// 目标的写入指纹:规划时记下,执行前比对。
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteStamp {
    newest: Option<SystemTime>,
    size: u64,
}

/// 目录只看自身与直接子项:Grok 的正文全在第一层;Claude 附属目录更深处的子代理转录
/// 变动时,主转录(同在这份计划里)也在变。
fn write_stamp(path: &Path, is_dir: bool) -> WriteStamp {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return WriteStamp {
            newest: None,
            size: 0,
        };
    };
    let mut stamp = WriteStamp {
        newest: meta.modified().ok(),
        size: if is_dir { 0 } else { meta.len() },
    };
    if is_dir && let Ok(entries) = fs::read_dir(path) {
        for meta in entries.flatten().filter_map(|e| e.metadata().ok()) {
            stamp.newest = stamp.newest.max(meta.modified().ok());
            if meta.is_file() {
                stamp.size += meta.len();
            }
        }
    }
    stamp
}

#[derive(Debug, Clone)]
pub struct SessionDeleteTarget {
    pub path: PathBuf,
    pub is_dir: bool,
    /// 所属 agent 的会话根。校验的边界,复核时还要用。
    root: PathBuf,
    /// 目录型目标里最后才删的那个文件,见 [`remove_target`]。
    last_child: Option<&'static str>,
    stamp: WriteStamp,
}

#[derive(Debug, Clone)]
pub struct SessionDeletePlan {
    /// 按删除顺序排好:让条目出现在列表里的那份排最后,半路失败时条目还在、能重试。
    pub targets: Vec<SessionDeleteTarget>,
    /// 刚被写过,可能还有 AI 在用(见 [`RECENT_WRITE_WINDOW`])。
    pub recently_written: bool,
}

/// 找全一个列表条目在磁盘上的文件。`wsl_distro` 有值时在发行版的 UNC 位置找。
pub fn plan_session_delete(
    session_type: &str,
    session_id: &str,
    project_path: &str,
    wsl_distro: Option<&str>,
) -> Result<SessionDeletePlan, SessionDeleteError> {
    // id 会拼进各家的路径,统一在入口挡穿越
    if !session_id_path_safe(session_id) {
        return Err(SessionDeleteError::InvalidId);
    }
    let kind = AgentKind::parse(session_type).ok_or(SessionDeleteError::Unsupported)?;
    if !session_delete_supported(session_type, wsl_distro, None) {
        return Err(SessionDeleteError::Unsupported);
    }

    let mut targets = Vec::new();
    if let Some(distro) = wsl_distro.filter(|d| !d.is_empty()) {
        // WSL 根项目的 distro 以路径解析为准(与 get_wsl_ai_sessions 口径一致)
        let (distro, unix_cwd) = derive_wsl_target(project_path, Some(distro.to_string()))
            .ok_or(SessionDeleteError::Unsupported)?;
        for home in wsl_candidate_homes(&distro) {
            targets.extend(match kind {
                AgentKind::Codex => codex_targets(&home, &unix_cwd, PathStyle::Unix, session_id)?,
                _ => claude_targets(&home, &unix_cwd, PathStyle::Unix, session_id)?,
            });
        }
        return plan_from_targets(targets);
    }

    let home = home_dir().ok_or(SessionDeleteError::NotFound)?;
    targets = match kind {
        AgentKind::Claude => claude_targets(&home, project_path, PathStyle::Windows, session_id)?,
        AgentKind::Codex => codex_targets(&home, project_path, PathStyle::Windows, session_id)?,
        AgentKind::Grok => {
            let root = super::grok::grok_sessions_dir().ok_or(SessionDeleteError::NotFound)?;
            grok_targets(&root, project_path, session_id)?
        }
        AgentKind::Pi => {
            let agent_dir =
                crate::hook_registry::pi_agent_dir().ok_or(SessionDeleteError::NotFound)?;
            pi_targets(
                &agent_dir.join("sessions"),
                &home,
                &std::env::temp_dir(),
                project_path,
                session_id,
            )?
        }
        _ => return Err(SessionDeleteError::Unsupported),
    };
    plan_from_targets(targets)
}

fn plan_from_targets(
    mut targets: Vec<SessionDeleteTarget>,
) -> Result<SessionDeletePlan, SessionDeleteError> {
    if targets.is_empty() {
        return Err(SessionDeleteError::NotFound);
    }
    // 目录在前、文件在后:Claude 的列表条目靠 `.jsonl`,附属目录删失败时条目还在
    targets.sort_by_key(|t| !t.is_dir);
    let newest = targets.iter().filter_map(|t| t.stamp.newest).max();
    Ok(SessionDeletePlan {
        targets,
        recently_written: written_recently_at(newest, SystemTime::now()),
    })
}

/// 复核全部目标后按序删除,并作废列表缓存。目标已经没了不算错。
pub fn execute_session_delete(plan: &SessionDeletePlan) -> Result<(), SessionDeleteError> {
    let mut live = Vec::with_capacity(plan.targets.len());
    for target in &plan.targets {
        match verify_target(&target.root, &target.path, target.is_dir) {
            Ok(()) => {}
            Err(SessionDeleteError::NotFound) => continue,
            Err(err) => return Err(err),
        }
        if write_stamp(&target.path, target.is_dir) != target.stamp {
            return Err(SessionDeleteError::Changed(display_path(&target.path)));
        }
        live.push(target);
    }
    let result = live.into_iter().try_for_each(remove_target);
    // 半路失败也作废:磁盘已经和缓存对不上了
    invalidate_session_cache();
    result
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn io_error(path: &Path, error: io::Error) -> SessionDeleteError {
    if error.kind() == io::ErrorKind::NotFound {
        return SessionDeleteError::NotFound;
    }
    SessionDeleteError::Io {
        path: display_path(path),
        message: error.to_string(),
    }
}

/// 路径校验,规划与复核共用:
///
/// - 叶子的**父目录**解析完符号链接后必须还在 `root` 之内(桶目录被链接到别处的话,
///   字面路径看着在根下,真删的却是外面);
/// - 目录型目标(要递归删)不许直接挂在根下,那是桶的位置;
/// - 叶子本身不跟随,必须是预期类型的真实条目。符号链接 / junction 一律拒绝,而不是
///   只删链接本身:它不是 agent 写出来的会话记录,说明环境和假设的不一样。
fn verify_target(root: &Path, path: &Path, expect_dir: bool) -> Result<(), SessionDeleteError> {
    let refuse = || SessionDeleteError::Unsafe(display_path(path));
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(refuse());
    }
    let (Some(parent), Some(_)) = (path.parent(), path.file_name()) else {
        return Err(refuse());
    };
    let canonical_root = root.canonicalize().map_err(|e| io_error(root, e))?;
    let canonical_parent = parent.canonicalize().map_err(|e| io_error(parent, e))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(refuse());
    }
    if expect_dir && canonical_parent == canonical_root {
        return Err(refuse());
    }
    let file_type = fs::symlink_metadata(path)
        .map_err(|e| io_error(path, e))?
        .file_type();
    let expected_kind = if expect_dir {
        file_type.is_dir()
    } else {
        file_type.is_file()
    };
    if file_type.is_symlink() || !expected_kind {
        return Err(refuse());
    }
    Ok(())
}

fn checked_target(
    root: &Path,
    path: PathBuf,
    is_dir: bool,
    last_child: Option<&'static str>,
) -> Result<SessionDeleteTarget, SessionDeleteError> {
    verify_target(root, &path, is_dir)?;
    let stamp = write_stamp(&path, is_dir);
    Ok(SessionDeleteTarget {
        path,
        is_dir,
        root: root.to_path_buf(),
        last_child,
        stamp,
    })
}

/// 不跟随地看条目在不在(悬空符号链接也算在,交给校验去拒绝)。
fn entry_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn claude_targets(
    home: &Path,
    project_path: &str,
    style: PathStyle,
    session_id: &str,
) -> Result<Vec<SessionDeleteTarget>, SessionDeleteError> {
    let root = home.join(".claude").join("projects");
    let mut targets = Vec::new();
    for dir in find_claude_project_dirs_in(&root, project_path, style) {
        let transcript = dir.join(format!("{session_id}.jsonl"));
        if entry_exists(&transcript) {
            targets.push(checked_target(&root, transcript, false, None)?);
        }
        let companion = dir.join(session_id);
        if entry_exists(&companion) {
            targets.push(checked_target(&root, companion, true, None)?);
        }
    }
    Ok(targets)
}

/// rollout 文件名里虽然有 id,列表认的却是头部:身份只看**首个** session_meta(fork /
/// subagent 文件后面还跟着复制来的父 meta,见 `lineage.rs`),cwd 也要对得上本项目。
fn codex_file_is_session(
    path: &Path,
    session_id: &str,
    normalized_project: &str,
    style: PathStyle,
) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    for line in BufReader::new(file).lines().take(5) {
        let Ok(line) = line else { continue };
        if let Some(meta) = codex_meta_from_line(&line) {
            return meta.id == session_id && style.normalize(&meta.cwd) == normalized_project;
        }
    }
    false
}

fn codex_targets(
    home: &Path,
    project_path: &str,
    style: PathStyle,
    session_id: &str,
) -> Result<Vec<SessionDeleteTarget>, SessionDeleteError> {
    let root = home.join(".codex").join("sessions");
    let mut paths = Vec::new();
    collect_codex_session_paths(&root, &mut paths);
    let normalized = style.normalize(project_path);
    paths
        .into_iter()
        .filter(|path| codex_file_is_session(path, session_id, &normalized, style))
        .map(|path| checked_target(&root, path, false, None))
        .collect()
}

/// 递归删之前确认它真是这个会话的目录:`summary.json` 是真实文件、能解析,里面的 id
/// (有的话)就是目录名。
fn is_grok_session_dir(dir: &Path, session_id: &str) -> bool {
    let summary = dir.join(GROK_SUMMARY_FILE);
    if !fs::symlink_metadata(&summary).is_ok_and(|m| m.file_type().is_file()) {
        return false;
    }
    let Some(value) = fs::read_to_string(&summary)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    else {
        return false;
    };
    value
        .pointer("/info/id")
        .and_then(|id| id.as_str())
        .is_none_or(|id| id == session_id)
}

fn grok_targets(
    root: &Path,
    project_path: &str,
    session_id: &str,
) -> Result<Vec<SessionDeleteTarget>, SessionDeleteError> {
    let mut targets = Vec::new();
    for group in find_grok_cwd_dirs_in(root, project_path) {
        let dir = group.join(session_id);
        if !entry_exists(&dir) {
            continue;
        }
        let target = checked_target(root, dir, true, Some(GROK_SUMMARY_FILE))?;
        if !is_grok_session_dir(&target.path, session_id) {
            return Err(SessionDeleteError::Unsafe(display_path(&target.path)));
        }
        targets.push(target);
    }
    Ok(targets)
}

fn pi_targets(
    root: &Path,
    home: &Path,
    temp_root: &Path,
    project_path: &str,
    session_id: &str,
) -> Result<Vec<SessionDeleteTarget>, SessionDeleteError> {
    let mut targets = Vec::new();
    for dir in project_session_dirs_in(PiFlavor::Pi, root, home, temp_root, project_path) {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension().and_then(|v| v.to_str()) != Some("jsonl") {
                continue;
            }
            // 与列表同口径按头部认,不按文件名:名字对不上的文件列表照样会列出来
            let is_session = session_meta(&path).is_some_and(|meta| {
                meta.id == session_id && meta_matches_project(&meta, project_path)
            });
            if is_session {
                targets.push(checked_target(root, path, false, None)?);
            }
        }
    }
    Ok(targets)
}

/// 删除一个条目本身,不跟随。`remove_dir_all` 在标准库里本来就不跟随目录内的链接。
fn remove_no_follow(path: &Path) -> io::Result<()> {
    let file_type = fs::symlink_metadata(path)?.file_type();
    if file_type.is_symlink() {
        // Windows 的目录 symlink / junction 要 remove_dir
        return fs::remove_file(path).or_else(|_| fs::remove_dir(path));
    }
    if file_type.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// 删一个目标。目录型目标把 `last_child` 留到最后:Grok 的列表条目靠 `summary.json`,
/// 其它文件删失败时条目还在,用户才有机会重试,不会留下一堆看不见的残余。
fn remove_target(target: &SessionDeleteTarget) -> Result<(), SessionDeleteError> {
    let remove = |path: &Path, result: io::Result<()>| match result {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_error(path, e)),
        _ => Ok(()),
    };
    if !target.is_dir {
        return remove(&target.path, fs::remove_file(&target.path));
    }
    let entries = match fs::read_dir(&target.path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_error(&target.path, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_error(&target.path, e))?;
        let path = entry.path();
        let is_last = target
            .last_child
            .is_some_and(|last| entry.file_name() == last);
        if !is_last {
            remove(&path, remove_no_follow(&path))?;
        }
    }
    if let Some(last) = target.last_child {
        let path = target.path.join(last);
        remove(&path, fs::remove_file(&path))?;
    }
    remove(&target.path, fs::remove_dir(&target.path))
}

#[cfg(test)]
mod tests {
    use super::super::codex::get_codex_sessions_in;
    use super::super::grok::grok_sessions_in;
    use super::super::pi::pi_sessions_in;
    use super::super::{get_claude_sessions_in, session_cache, session_cache_epoch};
    use super::*;

    /// 全部用例只在这个临时目录里动手,不碰真实的 `~/.claude` 等目录。
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "mini-term-session-delete-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const PROJECT: &str = r"D:\proj";
    const ID: &str = "11111111-aaaa";
    const OTHER: &str = "22222222-bbbb";

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// 建一条目录链接:Unix 用 symlink;Windows 用 junction(不要提权)。建不出来返回
    /// false,调用方跳过 —— 校验的是「链接逃逸」,没有链接就无从测起。
    fn link_dir(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(target, link).is_ok();
        #[cfg(windows)]
        let linked = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
            && link.exists();
        if !linked {
            eprintln!("跳过:建不出目录链接 {}", link.display());
        }
        linked
    }

    fn claude_line(cwd: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "cwd": cwd,
            "timestamp": "2026-10-01T00:00:00.000Z",
            "message": { "role": "user", "content": text },
        })
        .to_string()
    }

    fn claude_sessions(home: &Path) -> Vec<String> {
        get_claude_sessions_in(home, PROJECT, PathStyle::Windows, 100, None)
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    fn codex_meta_line(id: &str, cwd: &str) -> String {
        serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-10-01T00:00:00.000Z",
            "payload": { "id": id, "cwd": cwd, "timestamp": "2026-10-01T00:00:00.000Z" },
        })
        .to_string()
    }

    fn codex_sessions(home: &Path) -> Vec<String> {
        get_codex_sessions_in(home, PROJECT, PathStyle::Windows, 100, None)
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    fn grok_summary(id: &str) -> String {
        serde_json::json!({
            "info": { "id": id },
            "session_summary": "hi",
            "updated_at": "2026-10-01T00:00:00Z",
        })
        .to_string()
    }

    fn pi_header(id: &str, cwd: &str) -> String {
        serde_json::json!({
            "type": "session", "version": 3, "id": id,
            "timestamp": "2026-10-01T00:00:00.000Z", "cwd": cwd,
        })
        .to_string()
    }

    fn pi_sessions(dirs: &[PathBuf]) -> Vec<String> {
        pi_sessions_in(dirs, PROJECT)
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    // ---- 入口 ----

    #[test]
    fn 入口挡住穿越的会话_id() {
        for bad in ["../../etc/passwd", "a/b", r"a\b", "..", "", "a b"] {
            assert_eq!(
                plan_session_delete("claude", bad, PROJECT, None).unwrap_err(),
                SessionDeleteError::InvalidId,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn 没有列表的来源不提供删除() {
        for agent in ["omp", "opencode", "whatever", ""] {
            assert!(!session_delete_supported(agent, None, None), "{agent:?}");
            assert_eq!(
                plan_session_delete(agent, ID, PROJECT, None).unwrap_err(),
                SessionDeleteError::Unsupported,
                "{agent:?}"
            );
        }
        for agent in ["claude", "codex", "grok", "pi"] {
            assert!(session_delete_supported(agent, None, None), "{agent:?}");
        }
        // WSL / 远程的列表只有 Claude 与 Codex
        assert!(session_delete_supported("claude", Some("Ubuntu"), None));
        assert!(session_delete_supported("codex", None, Some("conn")));
        assert!(!session_delete_supported("grok", Some("Ubuntu"), None));
        assert!(!session_delete_supported("pi", None, Some("conn")));
    }

    // ---- Claude ----

    /// 主转录 + 同名附属目录 + 变体桶里的同 id 副本全部删掉,否则刷新后条目复活。
    #[test]
    fn claude_删全变体桶与附属目录() {
        let tmp = TempRoot::new("claude");
        let projects = tmp.0.join(".claude").join("projects");
        let exact = projects.join("D--proj");
        let variant = projects.join("D--proj-");
        let line = claude_line(PROJECT, "hi");
        write(&exact.join(format!("{ID}.jsonl")), &line);
        write(
            &exact.join(ID).join("subagents").join("agent-1.jsonl"),
            "{}",
        );
        write(&exact.join(ID).join("tool-results").join("r.txt"), "x");
        write(&variant.join(format!("{ID}.jsonl")), &line);
        write(&exact.join(format!("{OTHER}.jsonl")), &line);
        write(
            &exact.join(OTHER).join("subagents").join("agent-2.jsonl"),
            "{}",
        );
        assert!(claude_sessions(&tmp.0).contains(&ID.to_string()));

        let targets = claude_targets(&tmp.0, PROJECT, PathStyle::Windows, ID).unwrap();
        assert_eq!(targets.len(), 3, "{targets:?}");
        let plan = plan_from_targets(targets).unwrap();
        // 附属目录排在主转录前面:半路失败时条目还在,能重试
        assert!(plan.targets[0].is_dir && !plan.targets[2].is_dir);
        execute_session_delete(&plan).unwrap();

        assert!(!exact.join(format!("{ID}.jsonl")).exists());
        assert!(!exact.join(ID).exists());
        assert!(!variant.join(format!("{ID}.jsonl")).exists());
        // 别的会话一个字节不动
        assert!(exact.join(format!("{OTHER}.jsonl")).exists());
        assert!(
            exact
                .join(OTHER)
                .join("subagents")
                .join("agent-2.jsonl")
                .exists()
        );
        assert_eq!(claude_sessions(&tmp.0), [OTHER]);
    }

    /// fork 是整份复制:删父不动子,反过来也一样。
    #[test]
    fn claude_删父会话不连带分支会话() {
        let tmp = TempRoot::new("claude-fork");
        let bucket = tmp.0.join(".claude").join("projects").join("D--proj");
        write(
            &bucket.join(format!("{ID}.jsonl")),
            &claude_line(PROJECT, "root"),
        );
        let child = serde_json::json!({
            "type": "user", "cwd": PROJECT, "timestamp": "2026-10-02T00:00:00.000Z",
            "forkedFrom": { "sessionId": ID, "messageUuid": "m1" },
            "message": { "role": "user", "content": "root" },
        })
        .to_string();
        write(&bucket.join(format!("{OTHER}.jsonl")), &child);

        let targets = claude_targets(&tmp.0, PROJECT, PathStyle::Windows, ID).unwrap();
        execute_session_delete(&plan_from_targets(targets).unwrap()).unwrap();
        assert_eq!(claude_sessions(&tmp.0), [OTHER]);
    }

    /// WSL 来源走 Unix 路径语义(`\\wsl$` 下的 home 由调用方给)。
    #[test]
    fn claude_wsl_来源按_unix_口径定位() {
        let tmp = TempRoot::new("claude-wsl");
        let bucket = tmp.0.join(".claude").join("projects").join("-mnt-d-proj");
        write(
            &bucket.join(format!("{ID}.jsonl")),
            &claude_line("/mnt/d/proj", "hi"),
        );
        let targets = claude_targets(&tmp.0, "/mnt/d/proj", PathStyle::Unix, ID).unwrap();
        execute_session_delete(&plan_from_targets(targets).unwrap()).unwrap();
        assert!(!bucket.join(format!("{ID}.jsonl")).exists());
    }

    // ---- Codex ----

    /// 按头部认,不按文件名:名字不含 id 的也找得到;fork 文件后面那行复制来的父 meta
    /// 不算数;同 id 但 cwd 属于别的项目的不动(它不在这个项目的列表里)。
    #[test]
    fn codex_按头部首个_meta_认文件() {
        let tmp = TempRoot::new("codex");
        let day = tmp
            .0
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("10")
            .join("01");
        let own = day.join(format!("rollout-2026-10-01T00-00-00-{ID}.jsonl"));
        write(&own, &codex_meta_line(ID, PROJECT));
        let renamed = day.join("renamed.jsonl");
        write(&renamed, &codex_meta_line(ID, PROJECT));
        let fork = day.join(format!("rollout-2026-10-01T00-00-01-{OTHER}.jsonl"));
        write(
            &fork,
            &format!(
                "{}\n{}\n",
                codex_meta_line(OTHER, PROJECT),
                codex_meta_line(ID, PROJECT)
            ),
        );
        let elsewhere = day.join("elsewhere.jsonl");
        write(&elsewhere, &codex_meta_line(ID, r"D:\other"));

        let targets = codex_targets(&tmp.0, PROJECT, PathStyle::Windows, ID).unwrap();
        assert_eq!(targets.len(), 2, "{targets:?}");
        execute_session_delete(&plan_from_targets(targets).unwrap()).unwrap();

        assert!(!own.exists() && !renamed.exists());
        assert!(fork.exists() && elsewhere.exists());
        assert_eq!(codex_sessions(&tmp.0), [OTHER]);
    }

    // ---- Grok ----

    /// 一个会话是一整个目录。组目录、`.cwd`、组级的 prompt_history 都不属于这个会话。
    #[test]
    fn grok_删整个会话目录且不动组目录() {
        let tmp = TempRoot::new("grok");
        let root = tmp.0.join("sessions");
        let group = root.join("D%3A%5Cproj");
        write(&group.join("prompt_history.jsonl"), "{}");
        write(&group.join(".cwd"), PROJECT);
        for (id, summary) in [(ID, grok_summary(ID)), (OTHER, grok_summary(OTHER))] {
            write(&group.join(id).join("summary.json"), &summary);
            write(&group.join(id).join("updates.jsonl"), "{}");
            write(&group.join(id).join("terminal").join("0.log"), "x");
        }
        // 大小写差异各成一个组(见 find_grok_cwd_dirs):另一个组里同 id 也要删
        let variant = root.join("d%3A%5Cproj");
        if !variant.exists() {
            write(&variant.join(ID).join("summary.json"), &grok_summary(ID));
        }
        let listed = |root: &Path| -> Vec<String> {
            grok_sessions_in(root, PROJECT)
                .into_iter()
                .map(|s| s.id)
                .collect()
        };
        assert!(listed(&root).contains(&ID.to_string()));

        let targets = grok_targets(&root, PROJECT, ID).unwrap();
        execute_session_delete(&plan_from_targets(targets).unwrap()).unwrap();

        assert!(!group.join(ID).exists());
        assert!(!variant.join(ID).exists());
        assert!(group.join(OTHER).join("updates.jsonl").exists());
        assert!(group.join("prompt_history.jsonl").exists() && group.join(".cwd").exists());
        assert_eq!(listed(&root), [OTHER]);
    }

    /// 递归删之前要确认它真是这个会话的目录。
    #[test]
    fn grok_不是会话目录就拒绝() {
        let tmp = TempRoot::new("grok-reject");
        let root = tmp.0.join("sessions");
        let group = root.join("D%3A%5Cproj");
        let bare = group.join(ID);
        write(&bare.join("keep.txt"), "x");
        assert!(matches!(
            grok_targets(&root, PROJECT, ID),
            Err(SessionDeleteError::Unsafe(_))
        ));
        // summary.json 里的身份对不上目录名
        write(&bare.join("summary.json"), &grok_summary(OTHER));
        assert!(matches!(
            grok_targets(&root, PROJECT, ID),
            Err(SessionDeleteError::Unsafe(_))
        ));
        assert!(bare.join("keep.txt").exists());
    }

    /// 一个文件删不掉时 summary.json 还在,条目留在列表里能重试。模拟不了占用,直接验
    /// 删除顺序:把 summary 改成目录,最后那步必定失败,其余已经删完。
    #[test]
    fn grok_条目锚点文件最后才删() {
        let tmp = TempRoot::new("grok-order");
        let dir = tmp.0.join("group").join(ID);
        write(&dir.join("updates.jsonl"), "{}");
        fs::create_dir_all(dir.join(GROK_SUMMARY_FILE)).unwrap();
        let target = SessionDeleteTarget {
            path: dir.clone(),
            is_dir: true,
            root: tmp.0.clone(),
            last_child: Some(GROK_SUMMARY_FILE),
            stamp: write_stamp(&dir, true),
        };
        assert!(remove_target(&target).is_err());
        assert!(!dir.join("updates.jsonl").exists());
        assert!(dir.join(GROK_SUMMARY_FILE).exists());
    }

    // ---- pi ----

    /// 条目对应整个文件,`parentId` 分支随文件一起没;桶名撞车的别家项目不动。
    #[test]
    fn pi_删整个文件不动同桶别的会话() {
        let tmp = TempRoot::new("pi");
        let root = tmp.0.join("sessions");
        let bucket = root.join("--D--proj--");
        let own = bucket.join(format!("2026-10-01T00-00-00-000Z_{ID}.jsonl"));
        let branches = [
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"t","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"message","id":"u2a","parentId":"u1","timestamp":"t","message":{"role":"user","content":"branch a"}}"#,
            r#"{"type":"message","id":"u2b","parentId":"u1","timestamp":"t","message":{"role":"user","content":"branch b"}}"#,
        ];
        write(
            &own,
            &format!("{}\n{}\n", pi_header(ID, PROJECT), branches.join("\n")),
        );
        // 文件名对不上 id:列表按头部认,照样会列出来,所以删也要认
        let renamed = bucket.join("renamed.jsonl");
        write(&renamed, &pi_header(ID, PROJECT));
        let other = bucket.join(format!("2026-10-01T00-00-01-000Z_{OTHER}.jsonl"));
        write(&other, &pi_header(OTHER, PROJECT));
        let collision = bucket.join("collision.jsonl");
        write(&collision, &pi_header(ID, r"D:\pro\j"));

        let dirs = project_session_dirs_in(PiFlavor::Pi, &root, &tmp.0, &tmp.0, PROJECT);
        assert!(pi_sessions(&dirs).contains(&ID.to_string()));

        let targets = pi_targets(&root, &tmp.0, &tmp.0, PROJECT, ID).unwrap();
        assert_eq!(targets.len(), 2, "{targets:?}");
        execute_session_delete(&plan_from_targets(targets).unwrap()).unwrap();

        assert!(!own.exists() && !renamed.exists());
        assert!(other.exists() && collision.exists());
        assert_eq!(pi_sessions(&dirs), [OTHER]);
    }

    // ---- 路径校验 ----

    /// 桶目录被链接到会话根之外:字面路径在根下,真身在外面,一个字节都不许动。
    #[test]
    fn 桶目录链接到外面时拒绝() {
        let tmp = TempRoot::new("link-bucket");
        let outside = tmp.0.join("outside");
        let transcript = outside.join(format!("{ID}.jsonl"));
        write(&transcript, &claude_line(PROJECT, "hi"));
        let projects = tmp.0.join("home").join(".claude").join("projects");
        fs::create_dir_all(&projects).unwrap();
        if !link_dir(&projects.join("D--proj"), &outside) {
            return;
        }
        assert!(matches!(
            claude_targets(&tmp.0.join("home"), PROJECT, PathStyle::Windows, ID),
            Err(SessionDeleteError::Unsafe(_))
        ));
        assert!(transcript.exists());
    }

    /// 会话目录本身是链接:拒绝,而且不许顺着链接把外面删掉。
    #[test]
    fn 会话目录是链接时拒绝() {
        let tmp = TempRoot::new("link-session");
        let outside = tmp.0.join("outside");
        write(&outside.join("summary.json"), &grok_summary(ID));
        let root = tmp.0.join("sessions");
        let group = root.join("D%3A%5Cproj");
        fs::create_dir_all(&group).unwrap();
        if !link_dir(&group.join(ID), &outside) {
            return;
        }
        assert!(matches!(
            grok_targets(&root, PROJECT, ID),
            Err(SessionDeleteError::Unsafe(_))
        ));
        assert!(outside.join("summary.json").exists());
    }

    /// Claude 的附属目录同理:链接到外面就整单拒绝,主转录也不动。
    #[test]
    fn 附属目录是链接时整单拒绝() {
        let tmp = TempRoot::new("link-companion");
        let outside = tmp.0.join("outside");
        write(&outside.join("keep.txt"), "x");
        let bucket = tmp.0.join(".claude").join("projects").join("D--proj");
        let transcript = bucket.join(format!("{ID}.jsonl"));
        write(&transcript, &claude_line(PROJECT, "hi"));
        if !link_dir(&bucket.join(ID), &outside) {
            return;
        }
        assert!(matches!(
            claude_targets(&tmp.0, PROJECT, PathStyle::Windows, ID),
            Err(SessionDeleteError::Unsafe(_))
        ));
        assert!(transcript.exists() && outside.join("keep.txt").exists());
    }

    /// 文件型目标是符号链接(Windows 上建文件 symlink 要提权,建不出来就只测 Unix)。
    #[cfg(unix)]
    #[test]
    fn 会话文件是符号链接时拒绝() {
        let tmp = TempRoot::new("link-file");
        let outside = tmp.0.join("outside.jsonl");
        write(&outside, &claude_line(PROJECT, "hi"));
        let bucket = tmp.0.join(".claude").join("projects").join("D--proj");
        fs::create_dir_all(&bucket).unwrap();
        std::os::unix::fs::symlink(&outside, bucket.join(format!("{ID}.jsonl"))).unwrap();
        assert!(matches!(
            claude_targets(&tmp.0, PROJECT, PathStyle::Windows, ID),
            Err(SessionDeleteError::Unsafe(_))
        ));
        assert!(outside.exists());
    }

    #[test]
    fn 校验拒绝根外路径与穿越() {
        let tmp = TempRoot::new("verify");
        let root = tmp.0.join("root");
        let bucket = root.join("bucket");
        write(&bucket.join("a.jsonl"), "x");
        write(&tmp.0.join("outside.jsonl"), "x");
        fs::create_dir_all(bucket.join("dir")).unwrap();

        assert_eq!(verify_target(&root, &bucket.join("a.jsonl"), false), Ok(()));
        assert_eq!(verify_target(&root, &bucket.join("dir"), true), Ok(()));
        for (path, is_dir) in [
            (tmp.0.join("outside.jsonl"), false),
            (bucket.join("..").join("..").join("outside.jsonl"), false),
            // 类型对不上
            (bucket.join("a.jsonl"), true),
            (bucket.join("dir"), false),
            // 目录型目标不许直接挂在根下:那是桶的位置
            (bucket.clone(), true),
        ] {
            assert!(
                matches!(
                    verify_target(&root, &path, is_dir),
                    Err(SessionDeleteError::Unsafe(_))
                ),
                "{path:?}"
            );
        }
    }

    // ---- 执行 ----

    /// 确认之后文件变了:整单放弃,先于它排的目录也不许动。
    #[test]
    fn 确认后文件变过就整单放弃() {
        let tmp = TempRoot::new("changed");
        let bucket = tmp.0.join(".claude").join("projects").join("D--proj");
        let transcript = bucket.join(format!("{ID}.jsonl"));
        write(&transcript, &claude_line(PROJECT, "hi"));
        write(&bucket.join(ID).join("tool-results").join("r.txt"), "x");
        let targets = claude_targets(&tmp.0, PROJECT, PathStyle::Windows, ID).unwrap();
        let plan = plan_from_targets(targets).unwrap();

        // AI 又追加了一行
        write(
            &transcript,
            &format!("{}\n{{}}\n", claude_line(PROJECT, "hi")),
        );
        assert!(matches!(
            execute_session_delete(&plan),
            Err(SessionDeleteError::Changed(_))
        ));
        assert!(transcript.exists());
        assert!(bucket.join(ID).join("tool-results").join("r.txt").exists());
    }

    #[test]
    fn 目标已经没了不算错() {
        let tmp = TempRoot::new("gone");
        let bucket = tmp.0.join(".claude").join("projects").join("D--proj");
        let transcript = bucket.join(format!("{ID}.jsonl"));
        write(&transcript, &claude_line(PROJECT, "hi"));
        let targets = claude_targets(&tmp.0, PROJECT, PathStyle::Windows, ID).unwrap();
        let plan = plan_from_targets(targets).unwrap();
        fs::remove_file(&transcript).unwrap();
        assert_eq!(execute_session_delete(&plan), Ok(()));
    }

    #[test]
    fn 找不到文件就是_not_found() {
        let tmp = TempRoot::new("missing");
        fs::create_dir_all(tmp.0.join(".claude").join("projects").join("D--proj")).unwrap();
        let targets = claude_targets(&tmp.0, PROJECT, PathStyle::Windows, ID).unwrap();
        assert_eq!(
            plan_from_targets(targets).unwrap_err(),
            SessionDeleteError::NotFound
        );
    }

    #[test]
    fn 最近写入判据() {
        let now = SystemTime::now();
        assert!(written_recently_at(Some(now), now));
        assert!(written_recently_at(
            Some(now - Duration::from_secs(30)),
            now
        ));
        assert!(!written_recently_at(
            Some(now - Duration::from_secs(120)),
            now
        ));
        // 比现在还新只可能是时钟偏差,按刚写过算
        assert!(written_recently_at(Some(now + Duration::from_secs(5)), now));
        assert!(!written_recently_at(None, now));
    }

    /// 刚建的文件计划里就带着「最近写过」,确认框据此提醒。
    #[test]
    fn 刚写过的会话标记为最近写入() {
        let tmp = TempRoot::new("recent");
        let bucket = tmp.0.join(".claude").join("projects").join("D--proj");
        write(
            &bucket.join(format!("{ID}.jsonl")),
            &claude_line(PROJECT, "hi"),
        );
        let targets = claude_targets(&tmp.0, PROJECT, PathStyle::Windows, ID).unwrap();
        assert!(plan_from_targets(targets).unwrap().recently_written);
    }

    /// 执行后缓存被清空,而且序号前进:在途扫描据此放弃写回删除前的快照。
    #[test]
    fn 删除作废列表缓存() {
        let tmp = TempRoot::new("cache");
        let bucket = tmp.0.join(".claude").join("projects").join("D--proj");
        write(
            &bucket.join(format!("{ID}.jsonl")),
            &claude_line(PROJECT, "hi"),
        );
        let key = format!("test-delete-cache-{}", std::process::id());
        session_cache().lock().unwrap().insert(
            key.clone(),
            super::super::CachedSessions {
                loaded_at: std::time::Instant::now(),
                sessions: Vec::new(),
            },
        );
        let epoch = session_cache_epoch();

        let targets = claude_targets(&tmp.0, PROJECT, PathStyle::Windows, ID).unwrap();
        execute_session_delete(&plan_from_targets(targets).unwrap()).unwrap();

        assert!(session_cache_epoch() > epoch);
        assert!(!session_cache().lock().unwrap().contains_key(&key));
    }
}
