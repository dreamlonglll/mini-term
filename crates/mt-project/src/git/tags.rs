//! 标签:列出(历史行上画胶囊) / 新建(轻量或附注) / 删除 / 推送到远程。
//!
//! 列表走 git2(只读、要快);增删推走 git CLI —— 附注标签要继承用户的
//! `user.name` / `tag.gpgSign` 配置,推送要继承凭证管理器 / SSH agent。

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use git2::Repository;
use serde::{Deserialize, Serialize};

use super::log_ops::is_commit_hash;
use super::{
    LOCAL_OP_TIMEOUT, NETWORK_HINT, NETWORK_TIMEOUT, NO_EDITOR, QUICK_OP_TIMEOUT, run_checked_env,
};

/// 跑一条 `git tag` / `git push`,非零退出即把原文装进错误。
///
/// 一律带 `GIT_EDITOR=true`:面板里没有终端,任何一条意外要消息的路径(比如
/// `tag.gpgSign=true` 把轻量标签升格成签名的附注标签)都不能拉起编辑器干等到超时。
fn run_tag_command(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<()> {
    run_checked_env(repo_path, args, NO_EDITOR, op_label, timeout, timeout_hint).map(|_| ())
}

/// 指向某个提交的标签。
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TagInfo {
    pub name: String,
    /// 标签最终指向的**提交**(附注标签已剥到 commit)。
    pub target_hash: String,
    /// 附注标签(有自己的标签对象 / 消息)还是轻量标签。
    pub annotated: bool,
}

/// 标签名是否合法(`refs/tags/<name>` 过 `check-ref-format`,且不以 `-` 开头)。
pub fn is_valid_tag_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && git2::Tag::is_valid_name(name)
}

/// 远程名是否合法(同样挡掉 `-` 开头)。
fn is_valid_remote_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && git2::Remote::is_valid_name(name)
}

/// 列出全部指向提交的标签,按名字排序。指向树 / blob 的标签(极少见)跳过。
pub fn list_tags(repo_path: &Path) -> Result<Vec<TagInfo>> {
    let repo = Repository::open(repo_path)?;
    let mut tags = Vec::new();
    for reference in repo.references()? {
        let Ok(reference) = reference else {
            continue;
        };
        let Some(name) = reference
            .name()
            .and_then(|n| n.strip_prefix("refs/tags/"))
            .map(str::to_string)
        else {
            continue;
        };
        let Ok(commit) = reference.peel_to_commit() else {
            continue;
        };
        let annotated = reference
            .target()
            .is_some_and(|oid| repo.find_tag(oid).is_ok());
        tags.push(TagInfo {
            name,
            target_hash: commit.id().to_string(),
            annotated,
        });
    }
    tags.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tags)
}

/// 在某个提交上打标签。`message` 非空 = 附注标签(`-a -m`,继承用户的 `tag.gpgSign`),
/// 否则轻量标签。**阻塞最多 [`LOCAL_OP_TIMEOUT`]**(附注标签可能要 gpg 签名)。
pub fn create_tag(repo_path: &Path, name: &str, hash: &str, message: Option<&str>) -> Result<()> {
    if !is_valid_tag_name(name) {
        bail!("不是合法的标签名:{name}");
    }
    if !is_commit_hash(hash) {
        bail!("不是有效的提交哈希:{hash}");
    }
    let message = message.map(str::trim).filter(|m| !m.is_empty());
    let mut args = Vec::new();
    match message {
        Some(message) => args.extend(["tag", "-a", "-m", message]),
        // 轻量标签显式关掉签名:`tag.gpgSign=true` 会把 `git tag <name>` 升格成签名的附注
        // 标签、转头要消息。不用 `--no-sign`(老 git 不认),`-c` 覆盖配置哪个版本都认
        None => args.extend(["-c", "tag.gpgSign=false", "tag"]),
    }
    args.extend([name, hash]);
    // 操作名写死:args 里可能带着用户的标签消息
    run_tag_command(
        repo_path,
        &args,
        "tag",
        LOCAL_OP_TIMEOUT,
        ",可能卡在 gpg 签名上",
    )
}

/// 删除本地标签(远程上的不动)。
pub fn delete_tag(repo_path: &Path, name: &str) -> Result<()> {
    if !is_valid_tag_name(name) {
        bail!("不是合法的标签名:{name}");
    }
    run_tag_command(
        repo_path,
        &["tag", "-d", name],
        "tag -d",
        QUICK_OP_TIMEOUT,
        "",
    )
}

/// 把一个标签推到远程(`git push <remote> refs/tags/<name>`)。写全引用名,
/// 免得与同名分支撞车。**阻塞最多 [`NETWORK_TIMEOUT`]**(与 pull / push 同档),
/// 必须在后台线程上调用。
pub fn push_tag(repo_path: &Path, remote: &str, name: &str) -> Result<()> {
    if !is_valid_remote_name(remote) {
        bail!("不是合法的远程名:{remote}");
    }
    if !is_valid_tag_name(name) {
        bail!("不是合法的标签名:{name}");
    }
    let refspec = format!("refs/tags/{name}");
    run_tag_command(
        repo_path,
        &["push", remote, &refspec],
        "push tag",
        NETWORK_TIMEOUT,
        NETWORK_HINT,
    )
}

/// 仓库配置的远程名(`origin` …),推送标签的子菜单用。
pub fn list_remote_names(repo_path: &Path) -> Result<Vec<String>> {
    let repo = Repository::open(repo_path)?;
    let remotes = repo.remotes()?;
    Ok(remotes.iter().flatten().map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use git2::Repository;

    use super::super::test_support::init_repo_with_file;
    use super::*;

    fn head(root: &Path) -> String {
        let repo = Repository::open(root).unwrap();
        repo.head().unwrap().target().unwrap().to_string()
    }

    /// 测试机的全局配置可能开着 `tag.gpgSign`,附注标签会去找 gpg —— 仓库级关掉。
    fn no_sign(root: &Path) {
        let repo = Repository::open(root).unwrap();
        repo.config()
            .unwrap()
            .set_bool("tag.gpgsign", false)
            .unwrap();
    }

    #[test]
    fn 标签名校验() {
        assert!(is_valid_tag_name("v1.2.3"));
        assert!(is_valid_tag_name("release/2026-10"));
        assert!(!is_valid_tag_name(""));
        assert!(!is_valid_tag_name("-d"));
        assert!(!is_valid_tag_name("a b"));
        assert!(!is_valid_tag_name("a..b"));
        assert!(!is_valid_tag_name("x.lock"));
    }

    #[test]
    fn 轻量与附注标签_列出与删除() {
        let root = init_repo_with_file("tags", "a.txt", "1\n");
        no_sign(&root);
        let hash = head(&root);
        create_tag(&root, "v1", &hash, None).unwrap();
        create_tag(&root, "v2", &hash, Some("  ")).unwrap();
        create_tag(&root, "rel/v3", &hash, Some("release three")).unwrap();

        let tags = list_tags(&root).unwrap();
        let names: Vec<_> = tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["rel/v3", "v1", "v2"]);
        assert!(
            tags.iter().all(|t| t.target_hash == hash),
            "附注标签剥到提交"
        );
        let annotated: Vec<_> = tags.iter().map(|t| t.annotated).collect();
        assert_eq!(annotated, [true, false, false], "空白消息按轻量标签处理");

        // 重名 / 非法名 / 非法哈希
        assert!(create_tag(&root, "v1", &hash, None).is_err());
        assert!(create_tag(&root, "-f", &hash, None).is_err());
        assert!(create_tag(&root, "v9", "HEAD", None).is_err());

        delete_tag(&root, "v1").unwrap();
        let names: Vec<_> = list_tags(&root)
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["rel/v3", "v2"]);
        assert!(delete_tag(&root, "nope").is_err(), "不存在的标签要报错");
        std::fs::remove_dir_all(root).ok();
    }

    /// `tag.gpgSign=true` 时 `git tag <name>` 会被升格成签名的附注标签、拉起编辑器要消息 ——
    /// 面板里没有终端,原先就是挂到 60s 超时。轻量标签显式关掉签名,且一律 `GIT_EDITOR=true`。
    #[test]
    fn 轻量标签不受_tag_gpgsign_影响() {
        let root = init_repo_with_file("tags-gpgsign", "a.txt", "1\n");
        {
            let repo = Repository::open(&root).unwrap();
            let mut config = repo.config().unwrap();
            config.set_bool("tag.gpgSign", true).unwrap();
            // 修好之前别真等 60s:编辑器一起来就失败(环境里的 GIT_EDITOR 会压过它,同样不等)
            config.set_str("core.editor", "false").unwrap();
        }
        let hash = head(&root);
        create_tag(&root, "v1", &hash, None).unwrap();
        let tags = list_tags(&root).unwrap();
        assert_eq!(tags.len(), 1, "{tags:?}");
        assert!(!tags[0].annotated, "仍是轻量标签");
        assert_eq!(tags[0].target_hash, hash);
        delete_tag(&root, "v1").unwrap();
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn 推送标签到本地裸库() {
        let root = init_repo_with_file("tags-push", "a.txt", "1\n");
        no_sign(&root);
        let bare = root.with_extension("remote.git");
        Repository::init_bare(&bare).unwrap();
        Repository::open(&root)
            .unwrap()
            .remote("origin", &bare.to_string_lossy())
            .unwrap();
        assert_eq!(list_remote_names(&root).unwrap(), ["origin"]);

        let hash = head(&root);
        create_tag(&root, "v1", &hash, Some("first")).unwrap();
        push_tag(&root, "origin", "v1").unwrap();
        let remote = Repository::open_bare(&bare).unwrap();
        let pushed = remote
            .find_reference("refs/tags/v1")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        assert_eq!(pushed, hash);

        assert!(push_tag(&root, "-x", "v1").is_err());
        assert!(push_tag(&root, "origin", "--all").is_err());
        assert!(push_tag(&root, "nope", "v1").is_err(), "不存在的远程");
        drop(remote);
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&bare).ok();
    }
}
