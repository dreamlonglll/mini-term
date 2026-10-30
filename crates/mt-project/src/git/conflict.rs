//! 变更列表里的整文件操作:冲突文件的就地处理(采用我方 / 对方、标记为已解决)与
//! 「只丢弃未暂存改动」的丢弃。
//!
//! 全部走 git CLI:检出 / 暂存要过 git 自己的过滤器(autocrlf、git-lfs 的 smudge/clean),
//! libgit2 不跑外部过滤驱动 —— LFS 文件用 git2 检出会落成指针文件。
//!
//! 路径一律带 `--literal-pathspecs`:文件名里的 `*` / `?` / `:(…)` 不许被当成通配或
//! pathspec 魔法(名叫 `:(top)x` 的文件真存在,丢弃它不能变成丢弃整个仓库)。

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use git2::Repository;

use super::{QUICK_OP_TIMEOUT, run_git_command};

/// 冲突里取哪一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSide {
    /// stage 2(合并时是当前分支;**变基时是上游** —— git 自己的口径)
    Ours,
    /// stage 3
    Theirs,
}

/// 一个冲突文件在暂存区里的三方条目在不在(删除 / 修改冲突时有一侧缺席)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictInfo {
    pub path: String,
    pub ours: bool,
    pub theirs: bool,
}

/// 暂存区里全部冲突文件。
pub fn list_conflicts(repo_path: &Path) -> Result<Vec<ConflictInfo>> {
    let repo = Repository::open(repo_path)?;
    let index = repo.index()?;
    let mut out = Vec::new();
    for conflict in index.conflicts()? {
        let conflict = conflict?;
        let Some(entry) = conflict
            .our
            .as_ref()
            .or(conflict.their.as_ref())
            .or(conflict.ancestor.as_ref())
        else {
            continue;
        };
        out.push(ConflictInfo {
            path: String::from_utf8_lossy(&entry.path).into_owned(),
            ours: conflict.our.is_some(),
            theirs: conflict.their.is_some(),
        });
    }
    Ok(out)
}

fn conflict_of(repo_path: &Path, file: &str) -> Result<ConflictInfo> {
    list_conflicts(repo_path)?
        .into_iter()
        .find(|c| c.path == file)
        .ok_or_else(|| anyhow!("{file} 已不在冲突中,请刷新"))
}

fn git(repo_path: &Path, args: &[&str]) -> Result<String> {
    let mut full = vec!["--literal-pathspecs"];
    full.extend_from_slice(args);
    run_git_command(repo_path, &full, QUICK_OP_TIMEOUT, "")
}

/// 采用一侧的版本并标记为已解决。
///
/// 该侧有内容:`git checkout --ours/--theirs -- <file>` 再 `git add`;
/// 该侧是**删除**(删除 / 修改冲突):`git rm` —— 采用删除就是删掉它。
pub fn resolve_conflict_with(repo_path: &Path, file: &str, side: ConflictSide) -> Result<()> {
    let info = conflict_of(repo_path, file)?;
    let present = match side {
        ConflictSide::Ours => info.ours,
        ConflictSide::Theirs => info.theirs,
    };
    if present {
        let flag = match side {
            ConflictSide::Ours => "--ours",
            ConflictSide::Theirs => "--theirs",
        };
        git(repo_path, &["checkout", flag, "--", file])?;
        git(repo_path, &["add", "--", file])?;
    } else {
        // 工作区里可能还留着另一侧的内容(修改的那一侧),`--force` 才删得掉;
        // 用户点的就是「采用删除」,删的也只是这一个路径
        git(repo_path, &["rm", "--force", "--quiet", "--", file])?;
    }
    Ok(())
}

/// 标记为已解决 = 暂存当前工作区内容(工作区里已没有这个文件时暂存删除)。
/// 调用前 UI 先用 [`has_conflict_markers`] 判一次,还有 `<<<<<<<` 时先确认。
pub fn mark_conflict_resolved(repo_path: &Path, file: &str) -> Result<()> {
    conflict_of(repo_path, file)?;
    git(repo_path, &["add", "--all", "--", file])?;
    Ok(())
}

/// 工作区文件里还有没有冲突标记(`<<<<<<<` … `>>>>>>>` 各至少一行)。
/// 文件不存在 / 读不了按「没有」算 —— 那种情况标记已解决就是暂存删除。
pub fn has_conflict_markers(repo_path: &Path, file: &str) -> Result<bool> {
    let Ok(bytes) = std::fs::read(repo_path.join(file)) else {
        return Ok(false);
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut open = false;
    for line in text.lines() {
        if line.starts_with("<<<<<<<") {
            open = true;
        } else if open && line.starts_with(">>>>>>>") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 丢弃**未暂存**的改动:已跟踪文件从暂存区检出(`git checkout -- <files>`,已暂存的
/// 那部分保留,与 `git restore` / IDEA 的「回滚」一致);未跟踪文件直接删除。
///
/// 冲突中的文件不在这里处理(它们没有 stage 0 条目,误判成未跟踪会被删掉),直接拒绝。
pub fn discard_unstaged(repo_path: &Path, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let repo = Repository::open(repo_path)?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("bare repo"))?
        .to_path_buf();
    let index = repo.index()?;
    let mut tracked: Vec<&str> = Vec::new();
    let mut untracked: Vec<&str> = Vec::new();
    for file in files {
        let path = Path::new(file);
        if index.get_path(path, 0).is_some() {
            tracked.push(file);
        } else if (1..=3).any(|stage| index.get_path(path, stage).is_some()) {
            bail!("{file} 正在冲突中,请用冲突菜单处理");
        } else {
            untracked.push(file);
        }
    }
    drop(index);
    drop(repo);

    // 一次带太多路径会撞上 Windows 命令行 32K 的上限,分批跑
    for chunk in tracked.chunks(64) {
        let mut args = vec!["checkout", "--"];
        args.extend_from_slice(chunk);
        git(repo_path, &args)?;
    }
    for file in untracked {
        let abs = workdir.join(file);
        match std::fs::symlink_metadata(&abs) {
            Ok(meta) if meta.is_dir() => bail!("{file} 是目录,不能当文件丢弃"),
            Ok(_) => std::fs::remove_file(&abs)?,
            // 已经不在了 = 丢弃的目的已经达到
            Err(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::test_support::init_repo_with_file;
    use super::super::{DiffSide, get_changes_status, get_git_diff, git_stage};
    use super::*;

    /// 脚手架仓库 + 关掉 autocrlf:CLI 检出的换行不随本机全局配置变(Windows 开发机常是 true)。
    fn repo(tag: &str, rel: &str, content: &str) -> std::path::PathBuf {
        let root = init_repo_with_file(tag, rel, content);
        Repository::open(&root)
            .unwrap()
            .config()
            .unwrap()
            .set_bool("core.autocrlf", false)
            .unwrap();
        root
    }

    fn cli(root: &Path, args: &[&str]) -> bool {
        super::super::run_git_output(root, args, "test", Duration::from_secs(30), "")
            .unwrap()
            .success
    }

    /// 造一个冲突:`a.txt` 两边都改(内容冲突),`gone.txt` 一边删一边改(删除 / 修改冲突)。
    fn conflicted_repo(tag: &str) -> std::path::PathBuf {
        let root = repo(tag, "a.txt", "base\n");
        std::fs::write(root.join("gone.txt"), "keep me\n").unwrap();
        assert!(cli(&root, &["add", "gone.txt"]));
        assert!(cli(&root, &["commit", "-m", "base", "--no-verify"]));
        assert!(cli(&root, &["checkout", "-b", "other"]));
        std::fs::write(root.join("a.txt"), "theirs\n").unwrap();
        std::fs::write(root.join("gone.txt"), "changed by them\n").unwrap();
        assert!(cli(&root, &["commit", "-am", "theirs", "--no-verify"]));
        assert!(cli(&root, &["checkout", "-"]));
        std::fs::write(root.join("a.txt"), "ours\n").unwrap();
        assert!(cli(&root, &["rm", "-q", "gone.txt"]));
        assert!(cli(&root, &["commit", "-am", "ours", "--no-verify"]));
        assert!(!cli(&root, &["merge", "other", "--no-edit"]), "应当冲突");
        root
    }

    #[test]
    fn 列出冲突与缺席的一侧() {
        let root = conflicted_repo("conflict-list");
        let mut list = list_conflicts(&root).unwrap();
        list.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            list,
            vec![
                ConflictInfo {
                    path: "a.txt".into(),
                    ours: true,
                    theirs: true
                },
                ConflictInfo {
                    path: "gone.txt".into(),
                    ours: false,
                    theirs: true
                },
            ]
        );
        assert!(has_conflict_markers(&root, "a.txt").unwrap());
        assert!(!has_conflict_markers(&root, "gone.txt").unwrap());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 采用我方与对方() {
        let root = conflicted_repo("conflict-take");
        resolve_conflict_with(&root, "a.txt", ConflictSide::Theirs).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "theirs\n"
        );
        // 我方删除了 gone.txt:采用我方 = 删除
        resolve_conflict_with(&root, "gone.txt", ConflictSide::Ours).unwrap();
        assert!(!root.join("gone.txt").exists());
        assert!(list_conflicts(&root).unwrap().is_empty());
        let status = get_changes_status(&root).unwrap();
        assert!(
            status.iter().all(|c| c.path != "gone.txt"),
            "删掉且没有残留状态: {status:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 采用对方的修改侧与标记已解决() {
        let root = conflicted_repo("conflict-mark");
        resolve_conflict_with(&root, "gone.txt", ConflictSide::Theirs).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("gone.txt")).unwrap(),
            "changed by them\n"
        );
        // 手工改完再标记已解决:暂存的就是工作区当前内容
        std::fs::write(root.join("a.txt"), "merged by hand\n").unwrap();
        assert!(!has_conflict_markers(&root, "a.txt").unwrap());
        mark_conflict_resolved(&root, "a.txt").unwrap();
        assert!(list_conflicts(&root).unwrap().is_empty());
        let staged = get_git_diff(&root, "a.txt", DiffSide::Staged).unwrap();
        assert!(staged.new_content == "merged by hand\n");
        // 已解决的文件再点一次:明确报「不在冲突中」
        assert!(mark_conflict_resolved(&root, "a.txt").is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    /// 部分暂存的文件丢弃:只丢未暂存的部分,暂存的那部分留着;未跟踪文件删掉。
    #[test]
    fn 丢弃只丢未暂存部分() {
        let root = repo("discard-unstaged", "a.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        std::fs::write(root.join("a.txt"), "ONE\n2\n3\n4\n5\n6\n7\n8\n9\n").unwrap();
        git_stage(&root, &["a.txt".to_string()]).unwrap();
        std::fs::write(root.join("a.txt"), "ONE\n2\n3\n4\n5\n6\n7\n8\nNINE\n").unwrap();
        std::fs::write(root.join("new.txt"), "n\n").unwrap();

        discard_unstaged(&root, &["a.txt".to_string(), "new.txt".to_string()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "ONE\n2\n3\n4\n5\n6\n7\n8\n9\n",
            "回到暂存区版本,不是 HEAD"
        );
        assert!(!root.join("new.txt").exists());
        let staged = get_git_diff(&root, "a.txt", DiffSide::Staged).unwrap();
        assert_eq!(staged.hunks.len(), 1, "已暂存的改动还在");
        std::fs::remove_dir_all(&root).ok();
    }

    /// 文件名里的通配符按字面量处理:丢弃 `*.txt` 这个文件不会连带别的 txt。
    #[test]
    fn 丢弃按字面路径() {
        let root = repo("discard-literal", "a.txt", "a\n");
        std::fs::write(root.join("[ab].txt"), "x\n").unwrap();
        git_stage(&root, &["[ab].txt".to_string()]).unwrap();
        assert!(cli(&root, &["commit", "-m", "x", "--no-verify"]));
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        std::fs::write(root.join("[ab].txt"), "changed\n").unwrap();
        discard_unstaged(&root, &["[ab].txt".to_string()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("[ab].txt")).unwrap(),
            "x\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "changed\n",
            "`[ab].txt` 不能当通配去匹配 a.txt"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 丢弃拒绝冲突文件() {
        let root = conflicted_repo("discard-conflict");
        let err = discard_unstaged(&root, &["a.txt".to_string()]).unwrap_err();
        assert!(format!("{err:#}").contains("冲突"));
        assert!(root.join("a.txt").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
