//! 删除 SSH 远程项目的历史会话(AI 历史面板右键「删除会话」)。
//!
//! 走 SFTP 真删,边界交给现成的 [`delete_entry`]:它本来就按「根目录」校验 canonical
//! 父目录、叶子不跟随符号链接、目录走带 nonce 证明的服务端 `rm`(不可用时退回隔离后
//! 逐项删),这里只要把根换成 agent 的会话目录(`~/.claude/projects`、
//! `~/.codex/sessions`),就不必再写一份。
//!
//! 与本机同样分两步、同样找全再删(口径见 `mt_ai::sessions` 的 `delete.rs`),差别:
//!
//! - 远程列表只有 Claude 与 Codex;
//! - 写入指纹只有 readdir 给的秒级 mtime(SFTP 目录项这里没带 size);
//! - 远程 pane 拿不到 hook 上报的会话身份,「还在跑」主要靠 mtime 提示。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mt_ai::AgentKind;
use mt_ai::sessions::{
    SessionDeleteError, codex_meta_from_line, invalidate_session_cache, session_id_path_safe,
    written_recently_at,
};
use mt_config::SshConnection;
use mt_core::path_key::posix_ci_eq_key;
use mt_ssh::{SftpDirEntry, SftpHandle, SftpNodeKind};

use super::sessions::{
    codex_filename_matches_session, collect_remote_codex_files, matched_remote_claude_dirs,
};
use super::{
    CODEX_META_HEAD_BYTES, REMOTE_CODEX_SCAN_LIMIT, RemoteSshState, delete_entry, join_posix, lock,
    open_sftp, parent_posix, posix_relative, remote_home, state,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSessionDeleteTarget {
    pub path: String,
    pub is_dir: bool,
    mtime_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionDeletePlan {
    session_id: String,
    /// agent 的会话目录,交给 [`delete_entry`] 当边界。
    root: String,
    /// 按删除顺序排好:目录在前、文件在后(半路失败时列表条目还在,能重试)。
    pub targets: Vec<RemoteSessionDeleteTarget>,
    pub recently_written: bool,
}

fn remote_error(prefix: &str, message: &str) -> SessionDeleteError {
    SessionDeleteError::Remote(format!("{prefix}: {message}"))
}

/// 一个桶的 readdir 结果里属于这个会话的条目:`{id}.jsonl` + 同名附属目录。
/// 叶子是符号链接或类型不对就整单拒绝(与本机口径一致)。
fn claude_targets_in_dir(
    dir: &str,
    entries: &[SftpDirEntry],
    session_id: &str,
) -> Result<Vec<RemoteSessionDeleteTarget>, SessionDeleteError> {
    let transcript = format!("{session_id}.jsonl");
    let mut targets = Vec::new();
    for entry in entries {
        let is_dir = if entry.name == transcript {
            false
        } else if entry.name == session_id {
            true
        } else {
            continue;
        };
        let path = join_posix(dir, &entry.name);
        if entry.is_symlink || entry.is_dir != is_dir {
            return Err(SessionDeleteError::Unsafe(path));
        }
        targets.push(RemoteSessionDeleteTarget {
            path,
            is_dir,
            mtime_secs: entry.mtime_secs,
        });
    }
    Ok(targets)
}

/// rollout 头部是不是这个会话:身份只看首个 session_meta,cwd 也要对得上本项目。
fn codex_head_is_session(head: &str, session_id: &str, normalized_project: &str) -> bool {
    for line in head.lines().take(5) {
        if let Some(meta) = codex_meta_from_line(line) {
            return meta.id == session_id && posix_ci_eq_key(&meta.cwd) == normalized_project;
        }
    }
    false
}

fn plan_from_targets(
    session_id: &str,
    root: String,
    mut targets: Vec<RemoteSessionDeleteTarget>,
    now: SystemTime,
) -> Result<RemoteSessionDeletePlan, SessionDeleteError> {
    if targets.is_empty() {
        return Err(SessionDeleteError::NotFound);
    }
    targets.sort_by_key(|t| !t.is_dir);
    let newest = targets
        .iter()
        .filter_map(|t| t.mtime_secs)
        .max()
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
    Ok(RemoteSessionDeletePlan {
        session_id: session_id.to_string(),
        root,
        targets,
        recently_written: written_recently_at(newest, now),
    })
}

/// 条目自己的 mtime:从父目录 readdir 里取,不跟随叶子。
async fn entry_mtime(sftp: &SftpHandle, path: &str) -> Result<Option<u64>, SessionDeleteError> {
    let Some(parent) = parent_posix(path) else {
        return Err(SessionDeleteError::Unsafe(path.to_string()));
    };
    let name = path.rsplit('/').next().unwrap_or(path);
    let entries = sftp
        .read_dir(&parent)
        .await
        .map_err(|e| remote_error("读取远程会话目录失败", e.message()))?;
    Ok(entries
        .into_iter()
        .find(|e| e.name == name)
        .and_then(|e| e.mtime_secs))
}

async fn locate_claude_targets(
    sftp: &SftpHandle,
    root: &str,
    session_id: &str,
    project_path: &str,
) -> Result<Vec<RemoteSessionDeleteTarget>, SessionDeleteError> {
    let mut targets = Vec::new();
    for dir in matched_remote_claude_dirs(sftp, root, project_path).await {
        let entries = sftp
            .read_dir(&dir)
            .await
            .map_err(|e| remote_error("读取远程会话目录失败", e.message()))?;
        targets.extend(claude_targets_in_dir(&dir, &entries, session_id)?);
    }
    Ok(targets)
}

/// 列表扫描记下的路径优先(它不受扫描窗口限制),再按 rollout 文件名找;
/// 文件名只是线索,删之前一律读头部核对身份。
async fn locate_codex_targets(
    st: &RemoteSshState,
    sftp: &SftpHandle,
    conn_id: &str,
    root: &str,
    session_id: &str,
    project_path: &str,
) -> Result<Vec<RemoteSessionDeleteTarget>, SessionDeleteError> {
    // 先绑定再 await:临时 MutexGuard 活过 await 点会破坏 future 的 Send 约束
    let remembered = lock(&st.session_paths)
        .get(&format!("{conn_id}|{session_id}"))
        .cloned();
    let mut candidates: Vec<String> = remembered.into_iter().collect();
    for (path, _) in collect_remote_codex_files(sftp, root, REMOTE_CODEX_SCAN_LIMIT).await {
        if codex_filename_matches_session(&path, session_id) && !candidates.contains(&path) {
            candidates.push(path);
        }
    }

    let normalized_project = posix_ci_eq_key(project_path);
    let mut targets = Vec::new();
    for path in candidates {
        if posix_relative(root, &path).is_none_or(|rel| rel.is_empty()) {
            continue;
        }
        let kind = sftp
            .try_node_kind(&path)
            .await
            .map_err(|e| remote_error("读取远程会话文件失败", e.message()))?;
        match kind {
            None => continue,
            Some(SftpNodeKind::File) => {}
            Some(_) => return Err(SessionDeleteError::Unsafe(path)),
        }
        let head = sftp
            .read_head(&path, CODEX_META_HEAD_BYTES)
            .await
            .map_err(|e| remote_error("读取远程会话文件失败", e.message()))?;
        if !codex_head_is_session(
            &String::from_utf8_lossy(&head),
            session_id,
            &normalized_project,
        ) {
            continue;
        }
        let mtime_secs = entry_mtime(sftp, &path).await?;
        targets.push(RemoteSessionDeleteTarget {
            path,
            is_dir: false,
            mtime_secs,
        });
    }
    Ok(targets)
}

/// 找全远程机器上这个会话的文件。**阻塞**,丢 `background_executor`。
pub fn plan_remote_session_delete(
    conn: &SshConnection,
    session_type: &str,
    session_id: &str,
    project_path: &str,
) -> Result<RemoteSessionDeletePlan, SessionDeleteError> {
    if !session_id_path_safe(session_id) {
        return Err(SessionDeleteError::InvalidId);
    }
    let kind = AgentKind::parse(session_type);
    if !matches!(kind, Some(AgentKind::Claude | AgentKind::Codex)) {
        return Err(SessionDeleteError::Unsupported);
    }
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let outcome = async {
            let home = remote_home(st, &sftp, &conn.id)
                .await
                .map_err(SessionDeleteError::Remote)?;
            let (root, targets) = if kind == Some(AgentKind::Claude) {
                let root = join_posix(&join_posix(&home, ".claude"), "projects");
                let targets = locate_claude_targets(&sftp, &root, session_id, project_path).await?;
                (root, targets)
            } else {
                let root = join_posix(&join_posix(&home, ".codex"), "sessions");
                let targets =
                    locate_codex_targets(st, &sftp, &conn.id, &root, session_id, project_path)
                        .await?;
                (root, targets)
            };
            plan_from_targets(session_id, root, targets, SystemTime::now())
        }
        .await;
        sftp.close().await;
        Ok(outcome)
    })
    .map_err(SessionDeleteError::Remote)?
}

/// 复核:类型没变、mtime 没变。返回还在的目标(已经没了不算错)。
async fn recheck_targets<'a>(
    sftp: &SftpHandle,
    plan: &'a RemoteSessionDeletePlan,
) -> Result<Vec<&'a RemoteSessionDeleteTarget>, SessionDeleteError> {
    let mut live = Vec::with_capacity(plan.targets.len());
    for target in &plan.targets {
        let kind = sftp
            .try_node_kind(&target.path)
            .await
            .map_err(|e| remote_error("读取远程会话文件失败", e.message()))?;
        let expected = if target.is_dir {
            SftpNodeKind::Directory
        } else {
            SftpNodeKind::File
        };
        match kind {
            None => continue,
            Some(kind) if kind != expected => {
                return Err(SessionDeleteError::Unsafe(target.path.clone()));
            }
            Some(_) => {}
        }
        if entry_mtime(sftp, &target.path).await? != target.mtime_secs {
            return Err(SessionDeleteError::Changed(target.path.clone()));
        }
        live.push(target);
    }
    Ok(live)
}

/// 复核全部目标后按序删除,并作废列表缓存与路径映射。**阻塞**,丢 `background_executor`。
pub fn execute_remote_session_delete(
    conn: &SshConnection,
    plan: &RemoteSessionDeletePlan,
) -> Result<(), SessionDeleteError> {
    let st = state();
    let live = st
        .block_on(async move {
            let sftp = open_sftp(st, conn).await?;
            let outcome = recheck_targets(&sftp, plan).await;
            sftp.close().await;
            Ok(outcome)
        })
        .map_err(SessionDeleteError::Remote)??;
    let result = live.into_iter().try_for_each(|target| {
        delete_entry(conn, &plan.root, &target.path)
            .map(|_| ())
            .map_err(SessionDeleteError::Remote)
    });
    lock(&st.session_paths).remove(&format!("{}|{}", conn.id, plan.session_id));
    invalidate_session_cache();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "11111111-aaaa";

    fn entry(name: &str, kind: SftpNodeKind) -> SftpDirEntry {
        SftpDirEntry {
            name: name.to_string(),
            is_dir: kind == SftpNodeKind::Directory,
            is_file: kind == SftpNodeKind::File,
            is_symlink: kind == SftpNodeKind::Symlink,
            mtime_secs: Some(100),
        }
    }

    /// 主转录 + 同名附属目录都算,前缀相同的别的会话不算。
    #[test]
    fn claude_桶里只认这个会话的两样() {
        let entries = [
            entry("11111111-aaaa.jsonl", SftpNodeKind::File),
            entry("11111111-aaaa", SftpNodeKind::Directory),
            entry("11111111-aaaa-2.jsonl", SftpNodeKind::File),
            entry("22222222-bbbb.jsonl", SftpNodeKind::File),
            entry("22222222-bbbb", SftpNodeKind::Directory),
        ];
        let targets = claude_targets_in_dir("/home/u/.claude/projects/-p", &entries, ID).unwrap();
        let paths: Vec<&str> = targets.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/home/u/.claude/projects/-p/11111111-aaaa.jsonl",
                "/home/u/.claude/projects/-p/11111111-aaaa",
            ]
        );
    }

    #[test]
    fn claude_叶子是链接或类型不对就拒绝() {
        for bad in [
            entry("11111111-aaaa.jsonl", SftpNodeKind::Symlink),
            entry("11111111-aaaa", SftpNodeKind::Symlink),
            entry("11111111-aaaa.jsonl", SftpNodeKind::Directory),
            entry("11111111-aaaa", SftpNodeKind::File),
        ] {
            assert!(
                matches!(
                    claude_targets_in_dir("/r/-p", std::slice::from_ref(&bad), ID),
                    Err(SessionDeleteError::Unsafe(_))
                ),
                "{bad:?}"
            );
        }
    }

    fn meta_line(id: &str, cwd: &str) -> String {
        serde_json::json!({ "type": "session_meta", "payload": { "id": id, "cwd": cwd } })
            .to_string()
    }

    /// 身份只看首个 meta(fork 文件后面跟着复制来的父 meta),cwd 按远程口径比。
    #[test]
    fn codex_头部核对身份与项目() {
        let own = meta_line(ID, "/srv/Proj/");
        assert!(codex_head_is_session(
            &own,
            ID,
            &posix_ci_eq_key("/srv/proj")
        ));
        assert!(!codex_head_is_session(
            &own,
            ID,
            &posix_ci_eq_key("/srv/other")
        ));

        let fork = format!(
            "{}\n{}\n",
            meta_line("child", "/srv/proj"),
            meta_line(ID, "/srv/proj")
        );
        assert!(!codex_head_is_session(
            &fork,
            ID,
            &posix_ci_eq_key("/srv/proj")
        ));
        assert!(codex_head_is_session(
            &fork,
            "child",
            &posix_ci_eq_key("/srv/proj")
        ));

        assert!(!codex_head_is_session(
            "not json\n",
            ID,
            &posix_ci_eq_key("/srv/proj")
        ));
    }

    #[test]
    fn 计划_目录排前面_没有目标就是_not_found() {
        let target = |path: &str, is_dir, mtime_secs| RemoteSessionDeleteTarget {
            path: path.to_string(),
            is_dir,
            mtime_secs,
        };
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let plan = plan_from_targets(
            ID,
            "/r".into(),
            vec![
                target("/r/-p/a.jsonl", false, Some(100)),
                target("/r/-p/a", true, Some(9_990)),
            ],
            now,
        )
        .unwrap();
        assert!(plan.targets[0].is_dir && !plan.targets[1].is_dir);
        assert!(plan.recently_written, "最新那个 10 秒前才写过");

        let plan = plan_from_targets(
            ID,
            "/r".into(),
            vec![target("/r/-p/a.jsonl", false, Some(100))],
            now,
        )
        .unwrap();
        assert!(!plan.recently_written);

        assert_eq!(
            plan_from_targets(ID, "/r".into(), Vec::new(), now).unwrap_err(),
            SessionDeleteError::NotFound
        );
    }

    #[test]
    fn 入口挡住穿越的会话_id() {
        let conn = SshConnection {
            id: "c".into(),
            name: "c".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: None,
            group: None,
            extra: Default::default(),
        };
        for bad in ["../../etc/passwd", "a/b", "", "a b"] {
            assert_eq!(
                plan_remote_session_delete(&conn, "claude", bad, "/srv/p").unwrap_err(),
                SessionDeleteError::InvalidId,
                "{bad:?}"
            );
        }
        assert_eq!(
            plan_remote_session_delete(&conn, "grok", ID, "/srv/p").unwrap_err(),
            SessionDeleteError::Unsupported
        );
    }
}
