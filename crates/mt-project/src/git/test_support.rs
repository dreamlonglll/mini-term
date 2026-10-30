//! git 各子模块单测共用的临时仓库脚手架。只在 `cfg(test)` 下编译。
//!
//! 两类:git2 直接造的仓库([`init_repo_with_file`]),与走 git CLI 的小件([`git`] /
//! [`commit_file`] / [`repo_with_origin`] …,分支 / 同步 / 贮藏 / 仓库状态的单测都用)。

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// 跑一条 git 命令,失败直接 panic(带上 stderr)。身份与签名用 `-c` 钉死,
/// 不依赖跑测试那台机器的全局配置。
///
/// ⚠️ **不**在这里钉 `core.autocrlf`:被测函数走的 git 读的是机器的全局配置,
/// 两边换行口径不一致的话,一边 LF 检出、一边 CRLF 比对,干净的工作区会被认成
/// 「有改动」挡住检出。读文件内容做断言时自己归一换行。
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=mini-term",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("git 起不来");
    assert!(
        out.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// 写文件并提交一次。
pub(crate) fn commit_file(dir: &Path, rel: &str, content: &str, message: &str) {
    std::fs::write(dir.join(rel), content).unwrap();
    git(dir, &["add", rel]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// 当前分支名。
pub(crate) fn current(dir: &Path) -> String {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// 带 `origin` 的仓库:工作区仓库 + 同级的裸仓库当远程,当前分支已推上去并设好上游。
/// 返回 `(工作区, 裸仓库, 分支名)`。不联网。
pub(crate) fn repo_with_origin(tag: &str) -> (PathBuf, PathBuf, String) {
    let work = init_repo_with_file(tag, "a.txt", "a\n");
    let bare = work.with_file_name(format!(
        "{}-origin.git",
        work.file_name().unwrap().to_string_lossy()
    ));
    let parent = work.parent().unwrap().to_path_buf();
    git(&parent, &["init", "-q", "--bare", &bare.to_string_lossy()]);
    git(&work, &["remote", "add", "origin", &bare.to_string_lossy()]);
    let branch = current(&work);
    git(&work, &["push", "-q", "-u", "origin", &branch]);
    (work, bare, branch)
}

/// 从裸仓库再克隆一份「别人的工作区」,用来往远程上推新提交。
pub(crate) fn clone_of(bare: &Path, branch: &str) -> PathBuf {
    let dest = bare.with_file_name(format!(
        "{}-other",
        bare.file_name().unwrap().to_string_lossy()
    ));
    let parent = bare.parent().unwrap().to_path_buf();
    git(
        &parent,
        &[
            "clone",
            "-q",
            "-b",
            branch,
            &bare.to_string_lossy(),
            &dest.to_string_lossy(),
        ],
    );
    dest
}

pub(crate) fn cleanup(dirs: &[&Path]) {
    for dir in dirs {
        std::fs::remove_dir_all(dir).ok();
    }
}
