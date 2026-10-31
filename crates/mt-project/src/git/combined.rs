//! 合并查看多个提交的改动(历史区多选,对标 IDEA Log 选中多个提交后右侧的改动文件)。
//!
//! # 合并口径
//!
//! 照 IDEA `CommittedChangesTreeBrowser.zipChanges`:按**旧 → 新**逐个提交取改动
//! (与单提交同一口径:[`commit_files_in`],带重命名配对、合并提交只比第一父),再按文件
//! 串成「链」—— 后一次改动的旧路径等于前一次的新路径,就是同一个文件(跟随重命名)。
//! 每条链取「最早一次改动之前的版本」(`base`)到「最后一次改动之后的版本」(`head`):
//!
//! | 链上发生了什么 | 合并结果 |
//! |---|---|
//! | 改了几次 | modified(base = 最早那次的父,head = 最后那次) |
//! | 先新增后修改 | added(没有 base) |
//! | 先新增后删除 | 整条丢掉 |
//! | 先重命名后修改 | renamed(`old_path` = 起点路径) |
//! | 先删除后重新添加 | modified(两端路径相同、都在) |
//!
//! ⚠️ **非连续选择时,中间没选的提交对同一文件的改动也会进 diff**:两端是「最早那次之前」
//! 与「最后那次之后」两个版本,夹在中间的提交改了什么都算在里面(IDEA 同款口径)。没选的
//! 提交只改了别的文件的话,那些文件不会出现。
//!
//! 与 IDEA 的两处差别(都是为了每个文件只出一行):IDEA 只按「前一次的新路径」接链,删掉
//! 的文件之后又被重新添加会多出一行「新增」;这里删掉的链留着被删时的路径,同路径再出现时
//! 接上(删后重加 = 修改,与上表一致)。重命名撞上别的链(先删了 `b`、后把 `a` 改名成 `b`)
//! 时拆成两条:`b` 从旧内容改成新内容,`a` 删除 —— 即两端之间真实的净变化。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};

use super::{GitDiffResult, commit_files_in, diff_tree_texts};

/// 某个版本里的一个文件:合并变更两端的「版本 + 路径」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRevision {
    /// 提交的完整 hash。
    pub commit: String,
    /// 仓库内相对路径(`/` 分隔)。
    pub path: String,
}

/// 合并后的一个改动文件。`status` 与 [`super::CommitFileInfo::status`] 同一套字符串
/// (`added` / `modified` / `deleted` / `renamed`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CombinedFileChange {
    /// 最终路径;最终被删的是起点路径(`base` 那一侧)。
    pub path: String,
    pub status: String,
    /// 整体有改名时的起点路径(`status == "renamed"`)。
    pub old_path: Option<String>,
    /// 最早一次改动之前的版本:那次改动所在提交的第一父。文件那时还不存在(在所选
    /// 提交里新增)或那次改动就是根提交时为 `None`。
    pub base: Option<FileRevision>,
    /// 最后一次改动之后的版本。最终被删时为 `None`。
    pub head: Option<FileRevision>,
}

/// 一个提交里一个文件的改动:前一版(第一父里的)与后一版(本提交里的)。
#[derive(Debug, Clone)]
struct FileStep {
    before: Option<FileRevision>,
    after: Option<FileRevision>,
}

/// 合并过程中的一条文件链。
struct Chain {
    base: Option<FileRevision>,
    head: Option<FileRevision>,
}

impl Chain {
    fn finish(self) -> Option<CombinedFileChange> {
        let (status, path, old_path) = match (&self.base, &self.head) {
            (None, None) => return None,
            (None, Some(head)) => ("added", head.path.clone(), None),
            (Some(base), None) => ("deleted", base.path.clone(), None),
            (Some(base), Some(head)) if base.path != head.path => {
                ("renamed", head.path.clone(), Some(base.path.clone()))
            }
            (Some(_), Some(head)) => ("modified", head.path.clone(), None),
        };
        Some(CombinedFileChange {
            path,
            status: status.to_string(),
            old_path,
            base: self.base,
            head: self.head,
        })
    }
}

/// 按旧 → 新的顺序把各提交的改动串成链(口径见模块注释)。纯函数,单测直接喂。
///
/// 链按「当前所在路径」登记:还在的是 `head` 的路径,删掉的是被删时的路径。
fn zip_steps(steps: impl IntoIterator<Item = FileStep>) -> Vec<CombinedFileChange> {
    let mut chains: HashMap<String, Chain> = HashMap::new();
    for step in steps {
        let Some(key) = step
            .before
            .as_ref()
            .or(step.after.as_ref())
            .map(|rev| rev.path.clone())
        else {
            continue;
        };
        let base = match chains.remove(&key) {
            Some(chain) => chain.base,
            None => step.before,
        };
        let Some(head) = step.after else {
            // 删除:链留着(被删时的路径),之后同路径重新出现时接上;先新增后删除的整条丢掉
            if base.is_some() {
                chains.insert(key, Chain { base, head: None });
            }
            continue;
        };
        let current = head.path.clone();
        match chains.remove(&current) {
            // 改名撞上别的链(那个路径先被删了 / 被没选的提交挪走了):新路径上那条链
            // 接到这一版,改名的起点另记一条删除
            Some(other) if current != key => {
                chains.insert(
                    current,
                    Chain {
                        base: other.base,
                        head: Some(head),
                    },
                );
                if base.is_some() {
                    chains.insert(key, Chain { base, head: None });
                }
            }
            _ => {
                chains.insert(
                    current,
                    Chain {
                        base,
                        head: Some(head),
                    },
                );
            }
        }
    }
    let mut out: Vec<CombinedFileChange> = chains.into_values().filter_map(Chain::finish).collect();
    merge_deleted_then_added(&mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.status.cmp(&b.status)));
    out
}

/// 收尾:同一个路径上一条「删除」一条「新增」(改名走开后原路径又新增了同名文件)并成
/// 一条修改 —— 两端那个路径都在,就是改了内容。
fn merge_deleted_then_added(out: &mut Vec<CombinedFileChange>) {
    let added: HashSet<String> = out
        .iter()
        .filter(|f| f.status == "added")
        .map(|f| f.path.clone())
        .collect();
    let deleted: Vec<String> = out
        .iter()
        .filter(|f| f.status == "deleted" && added.contains(&f.path))
        .map(|f| f.path.clone())
        .collect();
    for path in deleted {
        let (Some(del), Some(add)) = (
            out.iter()
                .position(|f| f.status == "deleted" && f.path == path),
            out.iter()
                .position(|f| f.status == "added" && f.path == path),
        ) else {
            continue;
        };
        let head = out[add].head.clone();
        let entry = &mut out[del];
        entry.status = "modified".to_string();
        entry.head = head;
        out.remove(add);
    }
}

/// 合并多个提交的改动文件(口径见模块注释)。
///
/// `hashes` 按**历史列表的顺序**给(新 → 旧;列表本身是拓扑序,子在父前),这里倒过来
/// 从旧到新逐个取。重复的 hash 只算一次。结果按路径排序。
pub fn get_combined_commit_files(
    repo_path: &Path,
    hashes: &[String],
) -> Result<Vec<CombinedFileChange>> {
    let repo = Repository::open(repo_path)?;
    let mut seen = HashSet::new();
    let mut steps = Vec::new();
    for hash in hashes.iter().rev() {
        if !seen.insert(hash.as_str()) {
            continue;
        }
        let commit = repo.find_commit(Oid::from_str(hash)?)?;
        let parent = if commit.parent_count() > 0 {
            Some(commit.parent_id(0)?.to_string())
        } else {
            None
        };
        let this = commit.id().to_string();
        for file in commit_files_in(&repo, &commit)? {
            let before = match &parent {
                Some(parent) if file.status != "added" => Some(FileRevision {
                    commit: parent.clone(),
                    path: file.old_path.clone().unwrap_or_else(|| file.path.clone()),
                }),
                _ => None,
            };
            let after = (file.status != "deleted").then(|| FileRevision {
                commit: this.clone(),
                path: file.path.clone(),
            });
            steps.push(FileStep { before, after });
        }
    }
    Ok(zip_steps(steps))
}

fn tree_at<'r>(repo: &'r Repository, commit: &str) -> Result<git2::Tree<'r>> {
    Ok(repo.find_commit(Oid::from_str(commit)?)?.tree()?)
}

/// 两个版本之间一个文件的 diff(合并查看的右栏)。`None` 的一侧当空文本:`base` 为空 =
/// 新增,`head` 为空 = 删除。二进制 / 过大 / 非 UTF-8 的判定与单提交 diff 同一份
/// ([`diff_tree_texts`])。
pub fn get_revision_file_diff(
    repo_path: &Path,
    base: Option<&FileRevision>,
    head: Option<&FileRevision>,
) -> Result<GitDiffResult> {
    let repo = Repository::open(repo_path)?;
    let base_tree = base.map(|rev| tree_at(&repo, &rev.commit)).transpose()?;
    let head_tree = head.map(|rev| tree_at(&repo, &rev.commit)).transpose()?;
    diff_tree_texts(
        &repo,
        base_tree
            .as_ref()
            .zip(base)
            .map(|(t, rev)| (t, rev.path.as_str())),
        head_tree
            .as_ref()
            .zip(head)
            .map(|(t, rev)| (t, rev.path.as_str())),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::test_support::{cleanup, commit_file, git, init_repo_with_file};
    use super::*;

    fn rev(commit: &str, path: &str) -> FileRevision {
        FileRevision {
            commit: commit.into(),
            path: path.into(),
        }
    }

    fn step(before: Option<(&str, &str)>, after: Option<(&str, &str)>) -> FileStep {
        FileStep {
            before: before.map(|(c, p)| rev(c, p)),
            after: after.map(|(c, p)| rev(c, p)),
        }
    }

    /// `(路径, 状态, 旧路径)` 的形状,断言好写。
    fn shape(files: &[CombinedFileChange]) -> Vec<(String, String, Option<String>)> {
        files
            .iter()
            .map(|f| (f.path.clone(), f.status.clone(), f.old_path.clone()))
            .collect()
    }

    fn head_of(dir: &Path) -> String {
        git(dir, &["rev-parse", "HEAD"])
    }

    fn file<'a>(files: &'a [CombinedFileChange], path: &str) -> &'a CombinedFileChange {
        files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("没有 {path}: {files:?}"))
    }

    fn texts(root: &Path, change: &CombinedFileChange) -> (String, String) {
        let diff =
            get_revision_file_diff(root, change.base.as_ref(), change.head.as_ref()).unwrap();
        (
            diff.old_content.replace("\r\n", "\n"),
            diff.new_content.replace("\r\n", "\n"),
        )
    }

    /// 纯函数口径:改名撞上「先删掉的同名文件」时拆成「新路径修改 + 起点删除」;
    /// 改名走开、删掉、原路径又新增同名文件 = 原路径修改。
    #[test]
    fn 合并口径_改名撞车与删后重加() {
        // p0 里有 a、b;c1 删 b;c2 把 a 改名成 b
        let files = zip_steps([
            step(Some(("p0", "b")), None),
            step(Some(("c1", "a")), Some(("c2", "b"))),
        ]);
        assert_eq!(
            shape(&files),
            vec![
                ("a".into(), "deleted".into(), None),
                ("b".into(), "modified".into(), None),
            ]
        );
        assert_eq!(file(&files, "b").base, Some(rev("p0", "b")));
        assert_eq!(file(&files, "b").head, Some(rev("c2", "b")));
        assert_eq!(file(&files, "a").base, Some(rev("c1", "a")));

        // c1 a → b;c2 删 b;c3 新增 a
        let files = zip_steps([
            step(Some(("p0", "a")), Some(("c1", "b"))),
            step(Some(("c1", "b")), None),
            step(None, Some(("c3", "a"))),
        ]);
        assert_eq!(shape(&files), vec![("a".into(), "modified".into(), None)]);
        assert_eq!(file(&files, "a").base, Some(rev("p0", "a")));
        assert_eq!(file(&files, "a").head, Some(rev("c3", "a")));

        // 改名后删除:按起点路径报删除
        let files = zip_steps([
            step(Some(("p0", "a")), Some(("c1", "b"))),
            step(Some(("c1", "b")), None),
        ]);
        assert_eq!(shape(&files), vec![("a".into(), "deleted".into(), None)]);
        // 一条空步骤不影响别的
        assert!(zip_steps([step(None, None)]).is_empty());
    }

    /// 跨多个提交修改同一个文件:一条 modified,两端是最早那次的父与最后那次;
    /// 只出现一次的文件按自己那次的口径。
    #[test]
    fn 合并_跨多个提交修改() {
        let root = init_repo_with_file("combined-multi", "a.txt", "0\n");
        let base = head_of(&root);
        commit_file(&root, "a.txt", "1\n", "c1");
        let c1 = head_of(&root);
        commit_file(&root, "b.txt", "b\n", "c2 add b");
        let c2 = head_of(&root);
        commit_file(&root, "a.txt", "3\n", "c3");
        let c3 = head_of(&root);

        let files = get_combined_commit_files(&root, &[c3.clone(), c2.clone(), c1]).unwrap();
        assert_eq!(
            shape(&files),
            vec![
                ("a.txt".into(), "modified".into(), None),
                ("b.txt".into(), "added".into(), None),
            ]
        );
        let a = file(&files, "a.txt");
        assert_eq!(a.base, Some(rev(&base, "a.txt")));
        assert_eq!(a.head, Some(rev(&c3, "a.txt")));
        assert_eq!(texts(&root, a), ("0\n".into(), "3\n".into()));
        assert_eq!(file(&files, "b.txt").head, Some(rev(&c2, "b.txt")));
        cleanup(&[&root]);
    }

    /// 新增后修改 = added(没有 base);新增后删除整条丢掉。
    #[test]
    fn 合并_新增后修改与新增后删除() {
        let root = init_repo_with_file("combined-add", "a.txt", "a\n");
        commit_file(&root, "new.txt", "v1\n", "add new");
        let c1 = head_of(&root);
        commit_file(&root, "new.txt", "v2\n", "edit new");
        let c2 = head_of(&root);
        commit_file(&root, "tmp.txt", "tmp\n", "add tmp");
        let c3 = head_of(&root);
        git(&root, &["rm", "-q", "tmp.txt"]);
        git(&root, &["commit", "-q", "-m", "drop tmp"]);
        let c4 = head_of(&root);

        // 列表顺序:新 → 旧
        let all = [c4, c3.clone(), c2, c1];
        let files = get_combined_commit_files(&root, &all).unwrap();
        assert_eq!(
            shape(&files),
            vec![("new.txt".into(), "added".into(), None)]
        );
        let new = file(&files, "new.txt");
        assert_eq!(new.base, None);
        assert_eq!(texts(&root, new), (String::new(), "v2\n".into()));

        // 只选新增 tmp 的那个提交:tmp 照常是新增
        let files = get_combined_commit_files(&root, &[c3]).unwrap();
        assert_eq!(
            shape(&files),
            vec![("tmp.txt".into(), "added".into(), None)]
        );
        cleanup(&[&root]);
    }

    /// 重命名后修改:跟随重命名,合成一条 renamed,base 是改名前的路径。
    #[test]
    fn 合并_重命名后修改() {
        let body = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let root = init_repo_with_file("combined-rename", "old.txt", body);
        let base = head_of(&root);
        git(&root, &["mv", "old.txt", "new.txt"]);
        git(&root, &["commit", "-q", "-m", "rename"]);
        let c1 = head_of(&root);
        commit_file(&root, "new.txt", &body.replace("5\n", "five\n"), "edit");
        let c2 = head_of(&root);

        let files = get_combined_commit_files(&root, &[c2.clone(), c1]).unwrap();
        assert_eq!(
            shape(&files),
            vec![("new.txt".into(), "renamed".into(), Some("old.txt".into()))]
        );
        let renamed = file(&files, "new.txt");
        assert_eq!(renamed.base, Some(rev(&base, "old.txt")));
        assert_eq!(renamed.head, Some(rev(&c2, "new.txt")));
        let (old, new) = texts(&root, renamed);
        assert_eq!(old, body);
        assert_eq!(new, body.replace("5\n", "five\n"));
        cleanup(&[&root]);
    }

    /// 删除后重新添加 = modified(两端同一路径都在)。
    #[test]
    fn 合并_删除后重新添加() {
        let root = init_repo_with_file("combined-readd", "x.txt", "old\n");
        let base = head_of(&root);
        git(&root, &["rm", "-q", "x.txt"]);
        git(&root, &["commit", "-q", "-m", "drop x"]);
        let c1 = head_of(&root);
        commit_file(&root, "x.txt", "brand new\n", "re-add x");
        let c2 = head_of(&root);

        let files = get_combined_commit_files(&root, &[c2.clone(), c1]).unwrap();
        assert_eq!(
            shape(&files),
            vec![("x.txt".into(), "modified".into(), None)]
        );
        let x = file(&files, "x.txt");
        assert_eq!(x.base, Some(rev(&base, "x.txt")));
        assert_eq!(x.head, Some(rev(&c2, "x.txt")));
        assert_eq!(texts(&root, x), ("old\n".into(), "brand new\n".into()));
        cleanup(&[&root]);
    }

    /// 非连续选择:中间没选的提交对同一文件的改动算在两端之间;它只改了的别的文件不出现。
    #[test]
    fn 合并_非连续选择() {
        let root = init_repo_with_file("combined-gap", "a.txt", "0\n");
        let base = head_of(&root);
        commit_file(&root, "a.txt", "1\n", "c1");
        let c1 = head_of(&root);
        commit_file(&root, "a.txt", "2\n", "c2 (not selected)");
        commit_file(&root, "other.txt", "o\n", "c3 (not selected)");
        commit_file(&root, "a.txt", "4\n", "c4");
        let c4 = head_of(&root);

        let files = get_combined_commit_files(&root, &[c4.clone(), c1]).unwrap();
        assert_eq!(
            shape(&files),
            vec![("a.txt".into(), "modified".into(), None)]
        );
        let a = file(&files, "a.txt");
        assert_eq!(a.base, Some(rev(&base, "a.txt")));
        assert_eq!(a.head, Some(rev(&c4, "a.txt")));
        assert_eq!(texts(&root, a), ("0\n".into(), "4\n".into()));
        cleanup(&[&root]);
    }

    /// 选中里有根提交:根提交里的文件没有 base(相对空树),之后的修改接在它后面 = added。
    #[test]
    fn 合并_包含根提交() {
        let root = init_repo_with_file("combined-root", "a.txt", "0\n");
        let first = head_of(&root);
        commit_file(&root, "a.txt", "1\n", "c1");
        let c1 = head_of(&root);

        let files = get_combined_commit_files(&root, &[c1.clone(), first]).unwrap();
        assert_eq!(shape(&files), vec![("a.txt".into(), "added".into(), None)]);
        let a = file(&files, "a.txt");
        assert_eq!(a.base, None);
        assert_eq!(a.head, Some(rev(&c1, "a.txt")));
        assert_eq!(texts(&root, a), (String::new(), "1\n".into()));
        cleanup(&[&root]);
    }

    /// 选中里有合并提交:它只比第一父 —— 合并进来的那一侧的文件算在它头上。
    #[test]
    fn 合并_包含合并提交() {
        let root = init_repo_with_file("combined-merge", "a.txt", "a\n");
        let main = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
        git(&root, &["checkout", "-q", "-b", "feat"]);
        commit_file(&root, "f.txt", "f\n", "feat");
        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "m.txt", "m\n", "main");
        let m1 = head_of(&root);
        git(
            &root,
            &["merge", "-q", "--no-ff", "-m", "merge feat", "feat"],
        );
        let merge = head_of(&root);

        let files = get_combined_commit_files(&root, &[merge.clone(), m1.clone()]).unwrap();
        assert_eq!(
            shape(&files),
            vec![
                ("f.txt".into(), "added".into(), None),
                ("m.txt".into(), "added".into(), None),
            ]
        );
        assert_eq!(file(&files, "f.txt").head, Some(rev(&merge, "f.txt")));
        // 单选合并提交:只有合并进来的 f.txt(m.txt 在第一父里已经有了)
        let files = get_combined_commit_files(&root, &[merge]).unwrap();
        assert_eq!(shape(&files), vec![("f.txt".into(), "added".into(), None)]);
        cleanup(&[&root]);
    }

    /// 两版本 diff:base / head 为空的一侧当空文本;两侧都空就是空 diff;二进制照单提交口径。
    #[test]
    fn 两版本_diff_缺一侧当空文本() {
        let root = init_repo_with_file("combined-diff", "a.txt", "one\ntwo\n");
        let c0 = head_of(&root);
        commit_file(&root, "a.txt", "one\nTWO\n", "edit");
        let c1 = head_of(&root);

        let base = rev(&c0, "a.txt");
        let head = rev(&c1, "a.txt");
        let diff = get_revision_file_diff(&root, Some(&base), Some(&head)).unwrap();
        assert_eq!(diff.hunks.len(), 1, "{:?}", diff.hunks);
        assert!(!diff.is_binary && !diff.too_large);

        let added = get_revision_file_diff(&root, None, Some(&head)).unwrap();
        assert_eq!(added.old_content, "");
        assert_eq!(added.new_content.replace("\r\n", "\n"), "one\nTWO\n");
        assert!(added.hunks.iter().all(|h| h.old_lines == 0));

        let deleted = get_revision_file_diff(&root, Some(&base), None).unwrap();
        assert_eq!(deleted.new_content, "");
        assert_eq!(deleted.old_content.replace("\r\n", "\n"), "one\ntwo\n");

        let empty = get_revision_file_diff(&root, None, None).unwrap();
        assert!(empty.hunks.is_empty() && empty.old_content.is_empty());

        // 版本里没有这个路径(调用方给错了)也当空文本,不报错
        let missing = get_revision_file_diff(&root, Some(&rev(&c0, "nope.txt")), None).unwrap();
        assert!(missing.hunks.is_empty());

        // 二进制:与单提交 diff 同一口径
        std::fs::write(root.join("bin.dat"), [0u8, 1, 2, 0, 3]).unwrap();
        git(&root, &["add", "bin.dat"]);
        git(&root, &["commit", "-q", "-m", "bin"]);
        let c2 = head_of(&root);
        let bin = get_revision_file_diff(&root, None, Some(&rev(&c2, "bin.dat"))).unwrap();
        assert!(bin.is_binary);
        cleanup(&[&root]);
    }
}
