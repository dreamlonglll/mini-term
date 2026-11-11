//! 文件树 git 标记的刷新计划:去抖期间攒下「为什么要重拉」,到点后台按它决定拉不拉
//! 全量 status(见 [`super`] 模块注释「git 状态着色」)。
//!
//! 此前每条 fs 事件、每次点名都直接排一次全量 status。几类事件其实不必:
//!
//! - **`.git` 内部变了**(Windows 上项目根的非递归监听在 `.git` 里任何文件改写时都会报一条
//!   「.git 被修改」):别的工具跑 `git status` 刷 stat 缓存、IDE 定时 fetch 写 `FETCH_HEAD`、
//!   面板自己暂存……先比一次仓库签名([`mt_project::git::repo_signature`]),与上次拉状态时的
//!   那份一样就不拉。**不能直接丢**:Git 面板收着时,AI pane 里的提交(它的输出不嗅探)只有
//!   这一路能让标记跟上;
//! - **只涉及被忽略路径**(`target/` 里的构建产物让根目录报一条「target 被修改」、编辑器的
//!   临时文件):按忽略规则过一遍,全被忽略就不拉 —— 判定与 Git 面板的工作区监听同一个函数
//!   ([`mt_project::git::any_unignored_change`])。
//!
//! 签名在拉 status **之前**读,与状态一起记下:之后比出来一样,说明那次拉到的状态里已经
//! 含着这次变化。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use mt_project::git::{GitFileStatus, RepoSignature};

/// 一次计划最多记多少条路径;再多只记「溢出」(按无条件拉算)。
const PLAN_PATH_CAP: usize = 256;

/// 去抖期间攒下的刷新理由。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct GitRefreshPlan {
    /// 无条件拉:切项目 / 手动刷新 / 终端里跑了 git / Git 面板点名 / `.git` 出现或消失
    pub force: bool,
    /// `.git` 内部变了:比签名,变了才拉
    pub check: bool,
    /// 工作区里变了的文件(按忽略规则过一遍)
    pub paths: HashSet<PathBuf>,
    /// 路径太多没记全:按无条件拉算
    pub overflow: bool,
}

impl GitRefreshPlan {
    pub fn is_empty(&self) -> bool {
        !self.force && !self.check && !self.overflow && self.paths.is_empty()
    }

    /// 记一条变了的路径。
    pub fn add_path(&mut self, path: PathBuf) {
        if self.paths.len() < PLAN_PATH_CAP {
            self.paths.insert(path);
        } else {
            self.overflow = true;
        }
    }

    /// 不看签名、不过忽略规则就该拉的。
    fn unconditional(&self) -> bool {
        self.force || self.overflow
    }
}

/// 多仓库项目的签名表(按仓库路径排好):仓库路径 → 签名(读不到为 `None`)。
pub(super) type RepoSigs = Vec<(PathBuf, Option<RepoSignature>)>;

/// 这条变化是不是项目里某个仓库的 git 目录里的(`<任意层>/.git` 或其下)。文件树只监听
/// 列出来的目录,`.git` 永远隐藏、从不展开,所以实际只会看到「`<dir>/.git` 被修改」这一种。
pub(super) fn is_git_internal(root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|c| c.as_os_str() == ".git")
}

/// 路径的比较键:Windows 上分隔符统一、不分大小写(libgit2 给的仓库路径是 `D:/x/`,监听
/// 报上来的是 `D:\x\…`);别处只统一分隔符。
fn path_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if cfg!(windows) {
        mt_core::path_key::windows_eq_key(&raw)
    } else {
        mt_core::path_key::slash_form(&raw)
    }
}

/// 把路径分到**最深**的那个包含它的仓库下(嵌套仓库里的改动归嵌套的那个)。返回按仓库
/// 下标分好的组,以及有没有落在所有仓库之外的路径。
pub(super) fn group_by_repo(
    repos: &[PathBuf],
    paths: &HashSet<PathBuf>,
) -> (Vec<(usize, Vec<PathBuf>)>, bool) {
    let keys: Vec<String> = repos.iter().map(|r| path_key(r)).collect();
    let mut groups: Vec<(usize, Vec<PathBuf>)> = Vec::new();
    let mut stray = false;
    for path in paths {
        let key = path_key(path);
        let owner = keys
            .iter()
            .enumerate()
            .filter(|(_, repo)| {
                key == **repo
                    || key
                        .strip_prefix(repo.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(_, repo)| repo.len())
            .map(|(i, _)| i);
        match owner {
            Some(i) => match groups.iter_mut().find(|(g, _)| *g == i) {
                Some((_, group)) => group.push(path.clone()),
                None => groups.push((i, vec![path.clone()])),
            },
            None => stray = true,
        }
    }
    (groups, stray)
}

/// 攒下的路径里有没有值得拉一次的:任一仓库里有没被忽略的,或者有落在所有仓库之外的
/// (认不出归属时宁可多拉一次 —— 路径写法对不上不该吞掉一次刷新)。项目里一个仓库都没有
/// 时什么都不必拉。`unignored` 是 [`mt_project::git::any_unignored_change`] 的替身(单测用)。
pub(super) fn paths_need_refresh(
    repos: &[PathBuf],
    paths: &HashSet<PathBuf>,
    mut unignored: impl FnMut(&Path, &[PathBuf]) -> bool,
) -> bool {
    if paths.is_empty() || repos.is_empty() {
        return false;
    }
    let (groups, stray) = group_by_repo(repos, paths);
    stray || groups.iter().any(|(i, group)| unignored(&repos[*i], group))
}

/// 后台跑一份计划。**阻塞**(发现仓库走 30s 缓存、签名、可能的全量 status),放后台执行器。
///
/// 返回 `None` = 不必拉(签名没变 / 全是被忽略的路径);`Some((签名表, 状态))` = 拉了,
/// 签名是拉状态**之前**读的。`known` 是上次拉状态时记下的签名表。
pub(super) fn run_plan(
    root: &Path,
    plan: &GitRefreshPlan,
    known: Option<&RepoSigs>,
) -> Option<(RepoSigs, anyhow::Result<Vec<GitFileStatus>>)> {
    let repos: Vec<PathBuf> = mt_project::git::discover_git_repos(root)
        .map(|list| list.into_iter().map(|r| r.path).collect())
        .unwrap_or_default();
    let read_sigs = || -> RepoSigs {
        let mut sigs: RepoSigs = repos
            .iter()
            .map(|repo| (repo.clone(), mt_project::git::repo_signature(repo).ok()))
            .collect();
        sigs.sort_by(|a, b| a.0.cmp(&b.0));
        sigs
    };
    let need = plan.unconditional()
        || paths_need_refresh(&repos, &plan.paths, |repo, group| {
            mt_project::git::any_unignored_change(repo, group).unwrap_or(true)
        });
    let sigs = if need {
        read_sigs()
    } else if plan.check {
        let sigs = read_sigs();
        if known == Some(&sigs) {
            return None;
        }
        sigs
    } else {
        return None;
    };
    Some((sigs, mt_project::git::get_git_status(root)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn 计划的空与无条件() {
        let mut plan = GitRefreshPlan::default();
        assert!(plan.is_empty());
        plan.check = true;
        assert!(!plan.is_empty() && !plan.unconditional());
        let mut plan = GitRefreshPlan::default();
        plan.add_path(PathBuf::from("/w/a"));
        assert!(!plan.is_empty() && !plan.unconditional());
        for i in 0..PLAN_PATH_CAP + 3 {
            plan.add_path(PathBuf::from(format!("/w/{i}")));
        }
        assert_eq!(plan.paths.len(), PLAN_PATH_CAP);
        assert!(plan.overflow && plan.unconditional(), "记不全按无条件拉");
    }

    #[test]
    fn git_目录里的变化认得出() {
        let root = Path::new("/w/proj");
        assert!(is_git_internal(root, Path::new("/w/proj/.git")));
        assert!(is_git_internal(root, Path::new("/w/proj/.git/index")));
        assert!(
            is_git_internal(root, Path::new("/w/proj/sub/.git")),
            "嵌套仓库"
        );
        assert!(!is_git_internal(root, Path::new("/w/proj/.gitignore")));
        assert!(!is_git_internal(root, Path::new("/w/proj/src/.github/x")));
        assert!(!is_git_internal(root, Path::new("/w/proj/src/a.rs")));
    }

    /// 嵌套仓库里的改动归最深的那个;前缀相同但不是子目录的不算(`/w/a` 不含 `/w/ab`)。
    #[test]
    fn 路径归到最深的仓库() {
        let repos = [PathBuf::from("/w/a"), PathBuf::from("/w/a/sub")];
        let (groups, stray) =
            group_by_repo(&repos, &set(&["/w/a/x.rs", "/w/a/sub/y.rs", "/w/ab/z.rs"]));
        assert!(stray, "/w/ab 不在任何仓库里");
        let find = |i: usize| {
            groups
                .iter()
                .find(|(g, _)| *g == i)
                .map(|(_, paths)| paths.clone())
                .unwrap_or_default()
        };
        assert_eq!(find(0), vec![PathBuf::from("/w/a/x.rs")]);
        assert_eq!(find(1), vec![PathBuf::from("/w/a/sub/y.rs")]);
    }

    /// Windows 上仓库路径是 libgit2 的 `D:/x/`、事件是 `d:\X\…`:照样认得出归属。
    #[cfg(windows)]
    #[test]
    fn 路径归属不分写法() {
        let repos = [PathBuf::from("D:/Git/Proj/")];
        let (groups, stray) = group_by_repo(&repos, &set(&[r"d:\git\proj\src\a.rs"]));
        assert!(!stray);
        assert_eq!(groups.len(), 1);
    }

    /// 全是被忽略的路径不拉;有一条没被忽略就拉;认不出归属的宁可拉;没有仓库什么都不拉。
    #[test]
    fn 被忽略的路径不触发刷新() {
        let repos = [PathBuf::from("/w/a")];
        let ignored = |_: &Path, group: &[PathBuf]| {
            group
                .iter()
                .any(|p| !p.to_string_lossy().contains("target"))
        };
        assert!(!paths_need_refresh(
            &repos,
            &set(&["/w/a/target", "/w/a/target/debug"]),
            ignored
        ));
        assert!(paths_need_refresh(
            &repos,
            &set(&["/w/a/target", "/w/a/src/main.rs"]),
            ignored
        ));
        assert!(paths_need_refresh(&repos, &set(&["/elsewhere/x"]), ignored));
        assert!(!paths_need_refresh(
            &[],
            &set(&["/w/a/src/main.rs"]),
            ignored
        ));
        assert!(!paths_need_refresh(&repos, &HashSet::new(), ignored));
    }

    /// 真仓库上走一遍计划:`.git` 变了但签名没变不拉;暂存之后签名变了要拉;只有被忽略的
    /// 路径不拉;无条件的照拉。
    #[test]
    fn 计划按签名与忽略规则决定拉不拉() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-tree-plan-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@e"])
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git 起不来");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        let force = GitRefreshPlan {
            force: true,
            ..GitRefreshPlan::default()
        };
        let (sigs, files) = run_plan(&root, &force, None).expect("无条件要拉");
        assert_eq!(sigs.len(), 1);
        assert!(files.unwrap().is_empty(), "干净的工作区");

        let check = GitRefreshPlan {
            check: true,
            ..GitRefreshPlan::default()
        };
        assert!(run_plan(&root, &check, Some(&sigs)).is_none(), "签名没变");

        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build").join("o.bin"), "x").unwrap();
        let mut ignored = GitRefreshPlan::default();
        ignored.add_path(root.join("build").join("o.bin"));
        assert!(
            run_plan(&root, &ignored, Some(&sigs)).is_none(),
            "只有被忽略的"
        );

        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        git(&["add", "a.txt"]);
        let (new_sigs, files) = run_plan(&root, &check, Some(&sigs)).expect("暂存改了索引");
        assert_ne!(new_sigs, sigs);
        assert_eq!(files.unwrap().len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
