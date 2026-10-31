//! 提交详情栏的两项补充信息:逐文件增删行数、包含这个提交的分支。
//!
//! 两项都比详情本身([`super::get_commit_detail`])慢一截 —— 行数要把每个文件的两版
//! 内容都比一遍,分支要从每个分支头往下找 —— 所以单独取:详情先出来,这两项回来再补上,
//! 大提交不拖慢详情。
//!
//! # 行数与文件列表对得上
//!
//! 行数与详情的文件列表走同一份 diff([`super::first_parent_diff`]:相对第一个父、
//! 重命名配对),文件一一对应、顺序相同。文件太多([`LINE_STATS_MAX_FILES`])就不算了,
//! 交回 `None`:几千个文件的提交(vendored 目录、批量格式化)逐个比内容要好几秒,行数
//! 在那种提交上也没什么看头。
//!
//! # 分支走 git CLI
//!
//! `git for-each-ref --contains` 用 commit-graph 与代数剪枝,大仓库上比逐个分支调
//! libgit2 的 `graph_descendant_of`(每次都是一趟 merge-base)快得多。

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use git2::{Oid, Repository};

use super::{QUICK_OP_TIMEOUT, first_parent_diff, run_git_command};

/// 改动文件超过这么多就不算行数(见模块注释)。
pub const LINE_STATS_MAX_FILES: usize = 2000;

/// 一个文件的增删行数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLineStat {
    /// 新路径(与 `CommitFileInfo::path` 同一口径;删除的文件是它原来的路径)。
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
    /// 二进制文件没有行数(两项都是 0)。
    pub binary: bool,
}

/// 一个提交相对第一个父的增删行数。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitLineStats {
    /// 与详情的文件列表同序。
    pub files: Vec<FileLineStat>,
    pub additions: usize,
    pub deletions: usize,
}

/// 算一个提交的逐文件增删行数。改动文件超过 [`LINE_STATS_MAX_FILES`] 时返回 `None`。
///
/// 要读每个文件的两版内容,**在后台线程上调用**。
pub fn get_commit_line_stats(repo_path: &Path, hash: &str) -> Result<Option<CommitLineStats>> {
    let repo = Repository::open(repo_path)?;
    let commit = repo.find_commit(Oid::from_str(hash)?)?;
    let diff = first_parent_diff(&repo, &commit)?;
    let count = diff.deltas().len();
    if count > LINE_STATS_MAX_FILES {
        return Ok(None);
    }
    let mut stats = CommitLineStats::default();
    for idx in 0..count {
        // 先出补丁:二进制标记是读内容时才打上的(直接写在 diff 的 delta 上)
        let patch = git2::Patch::from_diff(&diff, idx)?;
        let delta = diff
            .get_delta(idx)
            .ok_or_else(|| anyhow!("diff 第 {idx} 项不存在"))?;
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let (additions, deletions, binary) = match patch {
            Some(patch) if !patch.delta().flags().is_binary() => {
                let (_context, additions, deletions) = patch.line_stats()?;
                (additions, deletions, false)
            }
            Some(_) => (0, 0, true),
            // 没出补丁:二进制,或内容没变(纯重命名)
            None => (0, 0, delta.flags().is_binary()),
        };
        stats.additions += additions;
        stats.deletions += deletions;
        stats.files.push(FileLineStat {
            path,
            additions,
            deletions,
            binary,
        });
    }
    Ok(Some(stats))
}

/// 包含某个提交的一个分支。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainingBranch {
    /// 短名:本地 `main`、远程 `origin/main`。
    pub name: String,
    pub is_remote: bool,
}

/// 包含这个提交(分支头就是它,或它是分支头的祖先)的本地与远程分支:本地在前,各自按
/// 名称排。远程的 `HEAD` 符号引用(`origin/HEAD`)不算。
///
/// 起 git 子进程,**阻塞最多 30s**,在后台线程上调用。
pub fn get_branches_containing(repo_path: &Path, hash: &str) -> Result<Vec<ContainingBranch>> {
    // 只认十六进制:别让它被当成选项或别的版本表达式
    if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("不是提交哈希:{hash}");
    }
    let contains = format!("--contains={hash}");
    let out = run_git_command(
        repo_path,
        &[
            "for-each-ref",
            &contains,
            "--format=%(refname)%09%(symref)",
            "refs/heads",
            "refs/remotes",
        ],
        QUICK_OP_TIMEOUT,
        ",仓库历史可能过长",
    )?;
    Ok(parse_containing_refs(&out))
}

/// 解析 `for-each-ref --format=%(refname)%09%(symref)` 的输出(每行「完整引用名\t符号引用
/// 目标」,不是符号引用的目标为空)。本地在前、远程在后,各自保持 git 的名称序。
pub fn parse_containing_refs(output: &str) -> Vec<ContainingBranch> {
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for line in output.lines() {
        let (refname, symref) = line.split_once('\t').unwrap_or((line, ""));
        let refname = refname.trim();
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            local.push(ContainingBranch {
                name: name.to_string(),
                is_remote: false,
            });
        } else if let Some(name) = refname.strip_prefix("refs/remotes/") {
            // `origin/HEAD` 是指向 `origin/main` 的符号引用,同一条线写两遍没有意义
            if !symref.trim().is_empty() {
                continue;
            }
            remote.push(ContainingBranch {
                name: name.to_string(),
                is_remote: true,
            });
        }
    }
    local.extend(remote);
    local
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{cleanup, commit_file, git, init_repo_with_file};
    use super::*;

    fn head(dir: &Path) -> String {
        git(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn 行数与详情文件同序且二进制不算行() {
        let root = init_repo_with_file("line-stats", "a.txt", "1\n2\n3\n");
        std::fs::write(root.join("a.txt"), "1\nchanged\n3\n4\n").unwrap();
        std::fs::write(root.join("b.bin"), [0u8, 1, 2, 0, 255, 0]).unwrap();
        std::fs::write(root.join("c.txt"), "x\ny\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "change"]);
        let hash = head(&root);

        let stats = get_commit_line_stats(&root, &hash).unwrap().unwrap();
        let files = super::super::get_commit_files(&root, &hash).unwrap();
        let paths: Vec<&str> = stats.files.iter().map(|f| f.path.as_str()).collect();
        let expect: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, expect, "与详情的文件列表同序");

        let a = stats.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!((a.additions, a.deletions, a.binary), (2, 1, false));
        let b = stats.files.iter().find(|f| f.path == "b.bin").unwrap();
        assert_eq!((b.additions, b.deletions, b.binary), (0, 0, true));
        let c = stats.files.iter().find(|f| f.path == "c.txt").unwrap();
        assert_eq!((c.additions, c.deletions, c.binary), (2, 0, false));
        assert_eq!((stats.additions, stats.deletions), (4, 1));
        cleanup(&[&root]);
    }

    #[test]
    fn 纯重命名没有行数也不算二进制() {
        let root = init_repo_with_file("line-stats-rename", "old.txt", "a\nb\nc\nd\n");
        git(&root, &["mv", "old.txt", "new.txt"]);
        git(&root, &["commit", "-q", "-m", "rename"]);
        let stats = get_commit_line_stats(&root, &head(&root)).unwrap().unwrap();
        assert_eq!(
            stats.files,
            vec![FileLineStat {
                path: "new.txt".into(),
                additions: 0,
                deletions: 0,
                binary: false,
            }]
        );
        cleanup(&[&root]);
    }

    #[test]
    fn 根提交与删除文件的行数() {
        let root = init_repo_with_file("line-stats-root", "a.txt", "1\n2\n");
        let first = head(&root);
        let stats = get_commit_line_stats(&root, &first).unwrap().unwrap();
        assert_eq!((stats.additions, stats.deletions), (2, 0));

        git(&root, &["rm", "-q", "a.txt"]);
        git(&root, &["commit", "-q", "-m", "drop"]);
        let stats = get_commit_line_stats(&root, &head(&root)).unwrap().unwrap();
        assert_eq!(stats.files.len(), 1);
        assert_eq!(stats.files[0].path, "a.txt");
        assert_eq!((stats.additions, stats.deletions), (0, 2));
        cleanup(&[&root]);
    }

    #[test]
    fn 包含提交的分支本地在前() {
        let root = init_repo_with_file("containing", "a.txt", "a\n");
        let base = head(&root);
        git(&root, &["branch", "side"]);
        commit_file(&root, "a.txt", "b\n", "second");
        let second = head(&root);
        git(&root, &["branch", "later"]);
        // 远程跟踪引用直接写出来,不必真起一个远程
        git(&root, &["update-ref", "refs/remotes/origin/main", &second]);
        git(
            &root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        let main = super::super::test_support::current(&root);

        let names = |hash: &str| -> Vec<(String, bool)> {
            get_branches_containing(&root, hash)
                .unwrap()
                .into_iter()
                .map(|b| (b.name, b.is_remote))
                .collect()
        };
        let mut local_all = vec![
            (main.clone(), false),
            ("later".to_string(), false),
            ("side".to_string(), false),
        ];
        local_all.sort();
        let mut expect = local_all;
        expect.push(("origin/main".to_string(), true));
        assert_eq!(
            names(&base),
            expect,
            "基点在全部分支里,origin/HEAD 不重复算"
        );

        let mut expect = vec![(main, false), ("later".to_string(), false)];
        expect.sort();
        expect.push(("origin/main".to_string(), true));
        assert_eq!(names(&second), expect, "side 停在基点,不含后来的提交");
        cleanup(&[&root]);
    }

    #[test]
    fn 不是哈希的参数直接拒绝() {
        let root = init_repo_with_file("containing-bad", "a.txt", "a\n");
        assert!(get_branches_containing(&root, "--all").is_err());
        assert!(get_branches_containing(&root, "").is_err());
        assert!(get_branches_containing(&root, "HEAD~1").is_err());
        cleanup(&[&root]);
    }

    #[test]
    fn 解析引用列表() {
        let out = "refs/heads/main\t\nrefs/heads/feat/x\t\nrefs/remotes/origin/HEAD\trefs/remotes/origin/main\nrefs/remotes/origin/main\t\n";
        let got: Vec<(String, bool)> = parse_containing_refs(out)
            .into_iter()
            .map(|b| (b.name, b.is_remote))
            .collect();
        assert_eq!(
            got,
            vec![
                ("main".to_string(), false),
                ("feat/x".to_string(), false),
                ("origin/main".to_string(), true),
            ]
        );
        assert!(parse_containing_refs("").is_empty());
    }
}
