//! 大批量暂存 / 取消暂存的快路径:逐路径直写暂存区,不走 pathspec。
//!
//! # 为什么
//!
//! 一批路径超过 [`PATHS_PER_CALL`] 时,旧路径从 stdin 一次喂给
//! `git --literal-pathspecs add -A --pathspec-from-file=-`(取消暂存是 `reset`)。git 拿每个
//! 目录项 / 暂存区条目去逐条比全部 pathspec,是「条目数 × 路径数」:E2E 实测 2 万个未跟踪
//! 文件「全部暂存」8.6s、取消暂存 5.5s,而同一仓库 `git add -- junk` 只要 0.7s。
//!
//! # 做法(方案甲:按路径直写)
//!
//! - 暂存:`git update-index --add --remove --replace -z --stdin`。每条路径直接进暂存区,
//!   不做 pathspec 匹配。算哈希与 `add` 是同一个 `index_path`(clean 过滤器 / autocrlf /
//!   `working-tree-encoding` 照跑、`core.safecrlf` 同口径),文件模式同一个
//!   `ce_mode_from_stat`(`core.filemode=false` 时沿用暂存区里的可执行位);`--replace` 对应
//!   `add` 的 `ADD_CACHE_OK_TO_REPLACE`(目录 / 文件之争时踢掉冲突的条目);工作区里已经
//!   不在的由 `--remove` 摘掉,即 `add -A` 的「暂存删除」。
//! - 取消暂存:libgit2 读 HEAD 树与暂存区,逐路径算出该回到的样子(HEAD 里有 → HEAD 的
//!   模式与对象;HEAD 里没有 → 摘掉;两边本来一样 → 不碰,与 `reset` 只动有差异的条目
//!   同口径),`git update-index -z --index-info` 一次写进去,再补一趟
//!   `update-index -q --unmerged --refresh`(见 [`unstage`])。
//!
//! 写暂存区一律走 CLI、不用 libgit2 写:与旧路径同一把 `index.lock`、同一套超时杀进程树
//! 与清锁(`spawn_git_with_input`)。`--stdin` / `--index-info` 读进来的是字面路径,
//! `--literal-pathspecs` 的口径不受影响。
//!
//! 实测(E2E 夹具副本,4 万个已跟踪文件 + 2 万个未跟踪文件,两种做法结果的
//! `git ls-files -s` 与 `git status --porcelain=v2 -uall` 逐字节相同):
//! - 2 万个未跟踪文件:暂存 7.9~9.2s → 1.4s(其中判据 0.2~0.4s),取消暂存 4.9~5.0s →
//!   0.8~0.9s(含补改名旧路径的那趟 status);
//! - 2 万个已修改的已跟踪文件:暂存 11.6s → 6.7s,取消暂存 8.2~8.5s → 1.0s。暂存剩下的是
//!   git 自己读文件算哈希的时间,一条 pathspec 的 `git add -A -- src` 也要 6.0s。
//!
//! # 为什么不压成目录 pathspec(方案乙)
//!
//! 「某目录下未暂存的条目全被选中」就换成 `dir/`,pathspec 数能大减,但:git 会把快照之后
//! 新出现在该目录里的文件一起暂存(面板列表是几百毫秒前的快照,构建 / 编辑器正在写文件
//! 时很常见),用户没选的东西进了暂存区;选中的路径散在很多目录里时压不动,照旧 N²;
//! 还得把面板的快照一路传下来。
//!
//! # 拿不准就整批退回旧路径
//!
//! 快路径只在**每一条**路径都能确认结果与 `add -A -- <path>` / `reset -- <path>` 相同时才走
//! (判据见 [`plan_stage`] / [`plan_unstage`]),有一条拿不准就整批交回 pathspec 路径。不拆成
//! 「快的一批 + 慢的一批」分两条命令跑:旧路径一条命令要么全做、要么 die 掉什么都不写,
//! 拆开后一批失败时另一批已经落地,失败形态就与旧路径不同了。快路径的 git 自己失败
//! (同样是 die 在写暂存区之前)也整批重跑旧路径,报给用户的是旧路径的原文。
//!
//! 判据读的是动手之前的快照(旧路径是 git 拿着 `index.lock` 现判):读完到 `update-index`
//! 拿到锁之间别的进程改了暂存区 / 工作区,结果可能与旧路径不同 —— 比如这几百毫秒里被删掉
//! 的未跟踪文件,旧路径报 did not match,这里当删除跳过。与面板列表本身就是快照同一档。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use git2::{ErrorCode, ObjectType, Oid, Repository};

use super::{
    INDEX_OP_TIMEOUT, PATHS_PER_CALL, QUICK_OP_TIMEOUT, ensure_repo_dir, nul_separated, spawn_git,
    spawn_git_with_input,
};

/// 暂存区条目 `flags` 里的 assume-unchanged 位(`update-index --assume-unchanged`)。
const ENTRY_VALID: u16 = 0x8000;
/// `flags_extended`:`git add -N` 留下的占位条目。
const ENTRY_INTENT_TO_ADD: u16 = 1 << 13;
/// `flags_extended`:sparse-checkout 之外的条目。
const ENTRY_SKIP_WORKTREE: u16 = 1 << 14;
/// 子模块(gitlink)条目的模式。
const GITLINK_MODE: u32 = 0o160000;

/// 暂存的快路径。`Ok(true)` = 已经做完;`Ok(false)` = 这批不走快路径,或快路径的 git 失败了
/// (die 在写暂存区之前,暂存区原样未动),调用方照旧走 pathspec。只有起不来 / 超时是 `Err`。
pub(super) fn stage(repo_path: &Path, files: &[String]) -> Result<bool> {
    let Some(input) = plan_stage(repo_path, files) else {
        return Ok(false);
    };
    ensure_repo_dir(repo_path)?;
    // 操作名沿用 `add`:超时 / 启动失败的提示与旧路径一字不差
    let out = spawn_git_with_input(
        repo_path,
        &[
            "update-index",
            "--add",
            "--remove",
            "--replace",
            "-z",
            "--stdin",
        ],
        &[],
        "add",
        INDEX_OP_TIMEOUT,
        "",
        Some(input),
    )?;
    Ok(out.success)
}

/// 取消暂存的快路径,返回值口径同 [`stage`]。HEAD 必须已出生(没出生的由调用方另走 libgit2)。
pub(super) fn unstage(repo_path: &Path, files: &[String]) -> Result<bool> {
    let Some(input) = plan_unstage(repo_path, files) else {
        return Ok(false);
    };
    ensure_repo_dir(repo_path)?;
    if !input.is_empty() {
        let out = spawn_git_with_input(
            repo_path,
            &["update-index", "-z", "--index-info"],
            &[],
            "reset",
            INDEX_OP_TIMEOUT,
            "",
            Some(input),
        )?;
        if !out.success {
            return Ok(false);
        }
    }
    // `reset` 收尾会刷新整个暂存区的 stat 缓存(`-q` 只是不打印):`--index-info` 写进来的
    // 条目 stat 全零,不刷新的话内容与 HEAD 相同的那些之后每次 status 都要重算哈希。
    // 只动 stat、不动条目内容,失败(撞锁之类)不影响结果,不报。
    let _ = spawn_git(
        repo_path,
        &["update-index", "-q", "--unmerged", "--refresh"],
        &[],
        "reset",
        QUICK_OP_TIMEOUT,
        "",
    );
    Ok(true)
}

/// 暂存这批路径能不能走快路径;能的话交回喂给 `update-index --stdin` 的内容。
///
/// 逐条确认 `update-index` 与 `add -A -- <path>` 结果相同,任一条不满足就是 `None`:
/// - 路径规矩([`plain_path`]);
/// - 已跟踪(暂存区有 stage 0 条目):不在冲突中;没挂 assume-unchanged / skip-worktree /
///   intent-to-add(`update-index` 对 skip-worktree 条目是直接摘掉);不是子模块 gitlink(省掉
///   嵌套仓库那几种形态的逐一对账,子模块仓库里这批照旧走慢路);工作区里它没有变成「装着
///   未跟踪文件的目录」(`add` 会连目录里的新文件、嵌套仓库一起暂存,`update-index` 只摘掉
///   旧条目);
/// - 未跟踪:在 `git ls-files -o --exclude-standard` 的列表里,即存在、是文件、没被忽略
///   (`add` 对点名的被忽略文件报错、对不存在的报 did not match,`update-index` 分别是照收 /
///   当删除跳过);与暂存区没有目录 / 文件之争(含子模块里的路径);`core.ignorecase` 下
///   大小写不与暂存区里已有的写法打架(见 [`case_folds`])。
fn plan_stage(repo_path: &Path, files: &[String]) -> Option<Vec<u8>> {
    if !files.iter().all(|f| plain_path(f)) {
        return None;
    }
    let repo = Repository::open(repo_path).ok()?;
    let index = IndexView::read(&repo).ok()?;
    let mut untracked: Vec<&str> = Vec::new();
    for file in files {
        if index.conflicted.contains(file.as_str()) {
            return None;
        }
        match index.entries.get(file.as_str()) {
            Some(entry) if !entry.plain || entry.mode == GITLINK_MODE => return None,
            Some(_) => {}
            None if index.dir_file_clash(file) => return None,
            None => untracked.push(file),
        }
    }
    let listing = UntrackedListing::read(repo_path, files)?;
    if files.iter().any(|f| listing.dirs.contains(f.as_str()))
        || !untracked.iter().all(|f| listing.files.contains(*f))
    {
        return None;
    }
    if !untracked.is_empty() && ignore_case(&repo) && case_folds(&index, &untracked) {
        return None;
    }
    Some(nul_separated(files))
}

/// 取消暂存这批路径能不能走快路径;能的话交回喂给 `update-index --index-info` 的内容
/// (可能为空:全都已与 HEAD 一致)。
///
/// 任一条不满足就是 `None`:路径规矩;不在冲突中;暂存区条目没挂特殊标志位(`reset` 会给
/// 新条目续上 skip-worktree,`--index-info` 不会);pathspec 只命中它自己 —— HEAD 里它不是
/// 目录、暂存区里没有条目在它下面、它的上级不是条目。
fn plan_unstage(repo_path: &Path, files: &[String]) -> Option<Vec<u8>> {
    if !files.iter().all(|f| plain_path(f)) {
        return None;
    }
    let repo = Repository::open(repo_path).ok()?;
    let head = repo.head().ok()?.peel_to_tree().ok()?;
    let index = IndexView::read(&repo).ok()?;
    let mut input = Vec::new();
    for file in files {
        if index.conflicted.contains(file.as_str()) || index.dir_file_clash(file) {
            return None;
        }
        let staged = index.entries.get(file.as_str());
        if staged.is_some_and(|entry| !entry.plain) {
            return None;
        }
        let committed = match head.get_path(Path::new(file)) {
            Ok(entry) if entry.kind() == Some(ObjectType::Tree) => return None,
            Ok(entry) => Some((entry.filemode() as u32, entry.id())),
            // 上级在 HEAD 里是文件也是 NotFound:pathspec 同样什么都不命中
            Err(e) if e.code() == ErrorCode::NotFound => None,
            Err(_) => return None,
        };
        let record = match (staged, committed) {
            // 暂存区与 HEAD 一样:`reset` 不碰它(碰了只会把 stat 清零)
            (Some(entry), Some(head)) if (entry.mode, entry.id) == head => continue,
            (_, Some((mode, id))) => format!("{mode:o} {id}\t{file}\0"),
            // 模式 0 = 从暂存区摘掉(所有 stage)
            (Some(_), None) => format!("0 {}\t{file}\0", Oid::zero()),
            (None, None) => continue,
        };
        input.extend_from_slice(record.as_bytes());
    }
    Some(input)
}

/// 暂存区的只读快照(libgit2 读),只留判据要用的。暂存区格式 libgit2 读不了(split index、
/// sparse index)时读失败,整批退回旧路径。
struct IndexView {
    /// stage 0 条目
    entries: HashMap<String, StagedEntry>,
    /// 有冲突条目(stage 1~3)的路径
    conflicted: HashSet<String>,
    /// 全部条目的各级上级目录(不带尾斜杠)
    dirs: HashSet<String>,
}

struct StagedEntry {
    mode: u32,
    id: Oid,
    /// 没挂 assume-unchanged / skip-worktree / intent-to-add
    plain: bool,
}

impl IndexView {
    fn read(repo: &Repository) -> Result<Self> {
        let index = repo.index()?;
        let mut view = IndexView {
            entries: HashMap::with_capacity(index.len()),
            conflicted: HashSet::new(),
            dirs: HashSet::new(),
        };
        for entry in index.iter() {
            // 不是 UTF-8 的路径换成 U+FFFD:调用方的路径里有它就不走快路径(见 plain_path)
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            add_parents(&mut view.dirs, &path);
            if (entry.flags >> 12) & 0x3 != 0 {
                view.conflicted.insert(path);
                continue;
            }
            let plain = entry.flags & ENTRY_VALID == 0
                && entry.flags_extended & (ENTRY_INTENT_TO_ADD | ENTRY_SKIP_WORKTREE) == 0;
            let staged = StagedEntry {
                mode: entry.mode,
                id: entry.id,
                plain,
            };
            view.entries.insert(path, staged);
        }
        Ok(view)
    }

    /// 与暂存区有「目录 / 文件之争」:有条目在 `path/` 下面,或 `path` 的某一级上级本身就是
    /// 条目(文件或子模块)。
    fn dir_file_clash(&self, path: &str) -> bool {
        self.dirs.contains(path)
            || parents(path)
                .any(|dir| self.entries.contains_key(dir) || self.conflicted.contains(dir))
    }
}

/// `git ls-files -o --exclude-standard` 列出的未跟踪文件 / 目录:git 自己的「存在、没被忽略」
/// 口径(与 `add` 同一套 `setup_standard_excludes`)。不用 libgit2 的忽略判定(与 git 有细微
/// 出入);也不用 `git check-ignore --stdin`:它每条路径扫一遍暂存区,2 万条实测 9.4s。
struct UntrackedListing {
    files: HashSet<String>,
    /// 列出的文件的各级上级目录,加上整个被列出的目录(嵌套仓库列成 `sub/`)
    dirs: HashSet<String>,
}

impl UntrackedListing {
    /// 只扫这批路径的顶层目录(超过 [`PATHS_PER_CALL`] 个就扫整个工作区):pathspec 数有上限,
    /// 不会重蹈「条目数 × 路径数」。2 万个未跟踪文件实测 0.13s。
    fn read(repo_path: &Path, files: &[String]) -> Option<Self> {
        let mut tops: Vec<&str> = files
            .iter()
            .map(|f| f.split('/').next().unwrap_or(f))
            .collect();
        tops.sort_unstable();
        tops.dedup();
        let mut args = vec![
            "--literal-pathspecs",
            "ls-files",
            "-z",
            "-o",
            "--exclude-standard",
        ];
        if tops.len() <= PATHS_PER_CALL {
            args.push("--");
            args.extend(tops);
        }
        let out = spawn_git(repo_path, &args, &[], "ls-files", QUICK_OP_TIMEOUT, "").ok()?;
        if !out.success {
            return None;
        }
        let mut listing = UntrackedListing {
            files: HashSet::new(),
            dirs: HashSet::new(),
        };
        for name in out.stdout.split('\0').filter(|s| !s.is_empty()) {
            add_parents(&mut listing.dirs, name.strip_suffix('/').unwrap_or(name));
            match name.strip_suffix('/') {
                Some(dir) => listing.dirs.insert(dir.to_string()),
                None => listing.files.insert(name.to_string()),
            };
        }
        Some(listing)
    }
}

fn ignore_case(repo: &Repository) -> bool {
    repo.config()
        .and_then(|config| config.get_bool("core.ignorecase"))
        .unwrap_or(false)
}

/// `core.ignorecase` 下 `add` 会把新文件折成暂存区里已有的写法(`adjust_dirname_case` 折目录、
/// 同名不同大小写的条目沿用旧名),`update-index` 照原样写。有这种路径就是 `true`。
/// git 的大小写不敏感比较只折 ASCII。
fn case_folds(index: &IndexView, untracked: &[&str]) -> bool {
    let files: HashSet<String> = index
        .entries
        .keys()
        .chain(index.conflicted.iter())
        .map(|path| path.to_ascii_lowercase())
        .collect();
    // 小写 → 暂存区里的写法;同一个小写对着好几种写法时记成空串(谁都对不上)
    let mut dirs: HashMap<String, &str> = HashMap::with_capacity(index.dirs.len());
    for dir in &index.dirs {
        dirs.entry(dir.to_ascii_lowercase())
            .and_modify(|exact| *exact = "")
            .or_insert(dir.as_str());
    }
    untracked.iter().any(|path| {
        let lower = path.to_ascii_lowercase();
        files.contains(&lower)
            || dirs.contains_key(&lower)
            // 只折 ASCII,字节位置不变:两边的上级目录一一对得上
            || parents(path)
                .zip(parents(&lower))
                .any(|(dir, low)| dirs.get(low).is_some_and(|exact| *exact != dir))
    })
}

/// `a/b/c` 的各级上级目录,由深到浅:`a/b`、`a`。
fn parents(path: &str) -> impl Iterator<Item = &str> {
    path.rmatch_indices('/').map(move |(i, _)| &path[..i])
}

/// 把 `path` 的各级上级目录记进 `dirs`。由深到浅,碰到记过的就停:往里记的一律连带全部
/// 上级,记过的那一级往上必然都在。
fn add_parents(dirs: &mut HashSet<String>, path: &str) {
    for dir in parents(path) {
        if dirs.contains(dir) {
            break;
        }
        dirs.insert(dir.to_string());
    }
}

/// 快路径只收规矩的仓库内相对路径。`update-index` 遇到 git 不认的路径(`verify_path`:`.git`
/// 段与它在 NTFS / HFS 上的同义写法、Windows 不认的字符与设备名…)是打一行 `Ignoring path`
/// **跳过**,`add` 则整条命令失败 —— 这里挡的是 git 那张表的超集,挡下来的走旧路径,只是慢。
fn plain_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('\u{fffd}') && path.split('/').all(plain_component)
}

fn plain_component(name: &str) -> bool {
    // 空段 = 开头 / 结尾 / 连续的 `/`(尾斜杠是目录 pathspec,会连带目录里的东西)
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    // 末尾的 `.` / 空格 Windows 不认,`.git.`、`git~1 ` 这类同义写法也一并挡了
    if name.ends_with(['.', ' ']) {
        return false;
    }
    let odd_char = |c: char| {
        c < ' '
            || matches!(c, '\\' | ':')
            // HFS+ 比较文件名时忽略的零宽字符(`.g\u{200c}it` 就是 `.git`)
            || matches!(
                c,
                '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}'
            )
            || (cfg!(windows) && matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    };
    if name.chars().any(odd_char) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    // `.gitmodules` 是符号链接时 git 也不认;连同它的 8.3 短名一起挡
    let dotgit = lower == ".git"
        || lower == "git~1"
        || lower.starts_with(".gitmodules")
        || lower.starts_with("gitmod~")
        || lower.starts_with("gi7eba~");
    !dotgit && !(cfg!(windows) && is_dos_device(&lower))
}

/// Windows 的保留设备名(`con`、`aux.txt`、`com1.log`…),带不带扩展名都算。
fn is_dos_device(lower: &str) -> bool {
    let stem = lower.split('.').next().unwrap_or(lower).trim_end();
    if matches!(stem, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$") {
        return true;
    }
    stem.strip_prefix("com")
        .or_else(|| stem.strip_prefix("lpt"))
        .is_some_and(|n| {
            let mut chars = n.chars();
            matches!(chars.next(), Some('0'..='9' | '¹' | '²' | '³')) && chars.next().is_none()
        })
}

#[cfg(test)]
#[path = "bulk_index_tests.rs"]
mod tests;
