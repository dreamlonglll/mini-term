//! git 各子模块单测共用的临时仓库脚手架。只在 `cfg(test)` 下编译。

use std::path::{Path, PathBuf};

use git2::Repository;

/// 用 git2 搭一个只有一次提交、只含 `rel` 一个文件的仓库,返回仓库根。
///
/// 对根做 canonicalize(Windows 顺带剥掉 `\\?\` 前缀):libgit2 会对路径做 realpath,
/// macOS 的 `/var` → `/private/var`、GitHub Windows runner 临时目录的 8.3 短路径
/// (`RUNNER~1` → `runneradmin`)都会让「文件相对 workdir 的路径」算歪。
///
/// 仓库级配置写好 `user.name` / `user.email`:CI 机器没有全局身份,
/// 走 git CLI 的提交 / merge / cherry-pick 用例否则会报 `Please tell me who you are`。
pub(crate) fn init_repo_with_file(tag: &str, rel: &str, content: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("mini-term-git-{tag}-{ts}"));
    std::fs::create_dir_all(&root).unwrap();
    let root = crate::fs::strip_verbatim_prefix(std::fs::canonicalize(&root).unwrap());
    let repo = Repository::init(&root).unwrap();
    {
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "mini-term").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        config.set_bool("commit.gpgsign", false).unwrap();
    }
    let abs = root.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(&abs, content).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(rel)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("mini-term", "test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    root
}
