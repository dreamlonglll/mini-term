//! 大批量暂存 / 取消暂存快路径的单测:与 pathspec 旧路径逐字节对账(`git ls-files -s`、
//! `git status --porcelain=v2 -uall`)、分派(多少条走快路径、哪些情形整批退回旧路径)、
//! 快路径的 git 失败时的回落。全部走临时仓库,不联网。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::super::test_support::{cleanup, git};
use super::super::{
    IndexRoute, run_git_command_with_input, run_with_literal_paths, stage_paths, unstage_paths,
    with_rename_sources,
};
use super::*;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 空仓库(还没有提交),返回仓库根。目录名带 pid + 纳秒 + 进程内序号,并行跑的用例不会
/// 撞名;根做 canonicalize 并剥掉 `\\?\`(8.3 短路径、macOS `/private/var`,见 test_support)。
fn new_repo(tag: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "mini-term-bulk-{tag}-{}-{ts}-{seq}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let root = crate::fs::strip_verbatim_prefix(std::fs::canonicalize(&root).unwrap());
    let repo = Repository::init(&root).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "mini-term").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    root
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Unix 上给工作区文件加可执行位(Windows 是 `core.filemode=false`,可执行位只在暂存区里)。
fn set_exec(root: &Path, rel: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join(rel), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = (root, rel);
}

/// 会被当成通配 / 选项 / 要转义的文件名;`*` `?` Windows 上建不出来。
fn odd_names() -> Vec<&'static str> {
    let mut names = vec!["[ab].txt", "-dash.txt", "sp ace.txt", "中文/文件.txt"];
    if !cfg!(windows) {
        names.extend(["star*.txt", "q?.txt"]);
    }
    names
}

/// 对账口径:暂存区(`ls-files -s`)与整体状态(`status --porcelain=v2 -uall`),原样字节。
fn snapshot(root: &Path) -> (String, String) {
    (
        git(root, &["ls-files", "-s", "-z"]),
        git(root, &["status", "--porcelain=v2", "-uall", "-z"]),
    )
}

/// 两份快照逐字节相同;不同时只报对不上的那几条(整份太长,看不出差在哪)。
fn assert_same(new: &Path, old: &Path) {
    let (new_snap, old_snap) = (snapshot(new), snapshot(old));
    for (what, a, b) in [
        ("ls-files -s", &new_snap.0, &old_snap.0),
        ("status", &new_snap.1, &old_snap.1),
    ] {
        let (a, b): (HashSet<&str>, HashSet<&str>) =
            (a.split('\0').collect(), b.split('\0').collect());
        let only_new: Vec<_> = a.difference(&b).collect();
        let only_old: Vec<_> = b.difference(&a).collect();
        assert!(
            only_new.is_empty() && only_old.is_empty(),
            "{what} 对不上\n快路径多出:{only_new:#?}\n旧路径多出:{only_old:#?}"
        );
    }
    assert_eq!(new_snap, old_snap, "条目一样但顺序不同");
}

/// 往暂存区写几条 `--index-info` 记录(造冲突条目用)。
fn index_info(root: &Path, records: &[String]) {
    let input: Vec<u8> = records.iter().flat_map(|r| r.bytes().chain([0])).collect();
    run_git_command_with_input(
        root,
        &["update-index", "-z", "--index-info"],
        "test",
        input,
        Duration::from_secs(30),
        "",
    )
    .unwrap();
}

/// `rel` 变成三方冲突(stage 1~3 都是同一个对象,先摘掉 stage 0)。
fn make_conflict(root: &Path, rel: &str) {
    let blob = git(root, &["hash-object", "-w", "--no-filters", rel]);
    let mut records = vec![format!("0 {}\t{rel}", Oid::zero())];
    records.extend((1..=3).map(|stage| format!("100644 {blob} {stage}\t{rel}")));
    index_info(root, &records);
}

// ── 与旧路径对账 ─────────────────────────────────────────────

/// 暂存对账用的现场:已修改(CRLF 进、`text=auto` + `core.autocrlf=true` 入库成 LF)/ 已删除 /
/// 怪名字 / 暂存区里已有改名且新位置又改过 / 可执行位 / clean 过滤器 / 未跟踪(含已跟踪
/// 目录里的)/ 一个没改过的。返回仓库根与要暂存的路径;同一个现场造两份,内容一模一样。
fn stage_scene(tag: &str) -> (PathBuf, Vec<String>) {
    let root = new_repo(tag);
    git(&root, &["config", "core.autocrlf", "true"]);
    git(&root, &["config", "filter.upper.clean", "tr a-z A-Z"]);
    write(
        &root,
        ".gitattributes",
        "*.up filter=upper\n*.txt text=auto\n",
    );
    for i in 0..30 {
        write(&root, &format!("mod/m{i}.txt"), "v1\n");
        write(&root, &format!("del/d{i}.txt"), "d\n");
    }
    for name in odd_names() {
        write(&root, &format!("t/{name}"), "v1\n");
    }
    write(&root, "keep.txt", "keep\n");
    write(&root, "ren/old.txt", "1\n2\n3\n4\n5\n6\n7\n8\n");
    write(&root, "bin/run.sh", "echo 1\n");
    set_exec(&root, "bin/run.sh");
    git(&root, &["add", "-A"]);
    git(&root, &["update-index", "--chmod=+x", "bin/run.sh"]);
    git(&root, &["commit", "-q", "-m", "base"]);

    let mut files = Vec::new();
    for i in 0..30 {
        write(&root, &format!("mod/m{i}.txt"), "v2\r\nmore\r\n");
        std::fs::remove_file(root.join(format!("del/d{i}.txt"))).unwrap();
        files.push(format!("mod/m{i}.txt"));
        files.push(format!("del/d{i}.txt"));
    }
    for name in odd_names() {
        write(&root, &format!("t/{name}"), "v2\n");
        files.push(format!("t/{name}"));
    }
    write(&root, "bin/run.sh", "echo 2\n");
    git(&root, &["mv", "ren/old.txt", "ren/new.txt"]);
    write(&root, "ren/new.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
    files.extend(["bin/run.sh", "ren/new.txt", "keep.txt"].map(String::from));

    for i in 0..20 {
        write(&root, &format!("new/n{i}.txt"), &format!("n{i}\n"));
        files.push(format!("new/n{i}.txt"));
    }
    for name in odd_names() {
        write(&root, name, "new\n");
        files.push(name.to_string());
    }
    write(&root, "mod/extra.txt", "extra\n");
    write(&root, "crlf.txt", "a\r\nb\r\n");
    write(&root, "x.up", "hello\n");
    write(&root, "tool.sh", "echo tool\n");
    set_exec(&root, "tool.sh");
    files.extend(["mod/extra.txt", "crlf.txt", "x.up", "tool.sh"].map(String::from));
    (root, files)
}

#[test]
fn 大批暂存走快路径且结果与旧路径逐字节一致() {
    let (old, files) = stage_scene("stage-old");
    let (new, same) = stage_scene("stage-new");
    assert_eq!(files, same);
    assert!(files.len() > PATHS_PER_CALL);
    assert!(plan_stage(&new, &files).is_some(), "这批每一条都该收");
    let before = snapshot(&new);

    run_with_literal_paths(&old, &["add", "-A"], &files, "add").unwrap();
    assert_eq!(stage_paths(&new, &files).unwrap(), IndexRoute::Bulk);
    assert_ne!(snapshot(&new), before, "确实暂存了东西");
    assert_same(&new, &old);

    // 两边一致之外,还得是「都跑过」:过滤器、换行转换、可执行位
    assert_eq!(git(&new, &["cat-file", "-p", ":x.up"]), "HELLO");
    assert_eq!(git(&new, &["cat-file", "-p", ":crlf.txt"]), "a\nb");
    assert_eq!(git(&new, &["cat-file", "-p", ":mod/m0.txt"]), "v2\nmore");
    assert!(git(&new, &["ls-files", "-s", "bin/run.sh"]).starts_with("100755"));
    if cfg!(unix) {
        assert!(git(&new, &["ls-files", "-s", "tool.sh"]).starts_with("100755"));
    }
    cleanup(&[&old, &new]);
}

/// 取消暂存对账用的现场:暂存了修改 / 删除(工作区删了的与 `rm --cached` 留着的)/ 新增 /
/// 改名 / 可执行位 / 子模块 gitlink 换提交 / 怪名字;列表里另有一条没改过的、一条哪都没有的。
fn unstage_scene(tag: &str) -> (PathBuf, Vec<String>) {
    // gitlink 指向的提交不必存在;写死,两份现场才逐字节相同(本仓库的提交号带时间戳)
    const FIRST: &str = "1111111111111111111111111111111111111111";
    const SECOND: &str = "2222222222222222222222222222222222222222";
    let root = new_repo(tag);
    for i in 0..30 {
        write(&root, &format!("mod/m{i}.txt"), "v1\n");
        write(&root, &format!("del/d{i}.txt"), &format!("d{i}\n"));
    }
    for name in odd_names() {
        write(&root, name, "v1\n");
    }
    write(&root, "keep.txt", "keep\n");
    write(&root, "chmod.txt", "c\n");
    write(&root, "ren/old.txt", "1\n2\n3\n4\n5\n6\n7\n8\n");
    git(&root, &["add", "-A"]);
    // gitlink 只要暂存区条目;工作区留个空目录 = 子模块没检出,不算改动
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let gitlink = format!("160000,{FIRST},sub");
    git(&root, &["update-index", "--add", "--cacheinfo", &gitlink]);
    git(&root, &["commit", "-q", "-m", "base"]);

    let mut files = Vec::new();
    for i in 0..30 {
        write(&root, &format!("mod/m{i}.txt"), "v2\n");
        files.push(format!("mod/m{i}.txt"));
        files.push(format!("del/d{i}.txt"));
    }
    git(&root, &["add", "mod"]);
    let gone: Vec<String> = (0..15).map(|i| format!("del/d{i}.txt")).collect();
    let kept: Vec<String> = (15..30).map(|i| format!("del/d{i}.txt")).collect();
    let mut rm = vec!["rm", "-q", "--"];
    rm.extend(gone.iter().map(String::as_str));
    git(&root, &rm);
    let mut rm_cached = vec!["rm", "-q", "--cached", "--"];
    rm_cached.extend(kept.iter().map(String::as_str));
    git(&root, &rm_cached);

    let mut add = vec!["--literal-pathspecs", "add", "--"];
    for i in 0..20 {
        write(&root, &format!("new/n{i}.txt"), &format!("n{i}\n"));
        files.push(format!("new/n{i}.txt"));
    }
    for name in odd_names() {
        write(&root, name, "v2\n");
        files.push(name.to_string());
    }
    add.push("new");
    add.extend(odd_names());
    git(&root, &add);
    git(&root, &["mv", "ren/old.txt", "ren/new.txt"]);
    git(&root, &["update-index", "--chmod=+x", "chmod.txt"]);
    let gitlink = format!("160000,{SECOND},sub");
    git(&root, &["update-index", "--cacheinfo", &gitlink]);
    files.extend(["ren/new.txt", "chmod.txt", "sub", "keep.txt", "nope.txt"].map(String::from));
    (root, files)
}

#[test]
fn 大批取消暂存走快路径且结果与旧路径逐字节一致() {
    let (old, list) = unstage_scene("unstage-old");
    let (new, same) = unstage_scene("unstage-new");
    assert_eq!(list, same);
    // 与 git_unstage 一样先补改名的旧路径
    let files = with_rename_sources(&new, &list).unwrap();
    assert!(files.contains(&"ren/old.txt".to_string()), "{files:?}");
    assert_eq!(with_rename_sources(&old, &list).unwrap(), files);
    assert!(files.len() > PATHS_PER_CALL);

    run_with_literal_paths(&old, &["reset", "-q"], &files, "reset").unwrap();
    assert_eq!(unstage_paths(&new, &files).unwrap(), IndexRoute::Bulk);
    // 收尾刷新过 stat(在 status 刷新之前看):回到 HEAD、工作区也没动过的条目 stat 不是全零
    for root in [&old, &new] {
        let repo = Repository::open(root).unwrap();
        let index = repo.index().unwrap();
        let entry = index.get_path(Path::new("del/d20.txt"), 0).unwrap();
        assert_ne!(entry.mtime.seconds(), 0, "{}", root.display());
    }
    assert!(git(&new, &["diff", "--cached", "--name-only"]).is_empty());
    assert_same(&new, &old);
    cleanup(&[&old, &new]);
}

// ── 分派 ─────────────────────────────────────────────────────

/// 一批装得下的照旧走 pathspec(小批量不换路),多一条才走快路径。
#[test]
fn 一批装得下照旧走_pathspec_多出来才走快路径() {
    let root = new_repo("dispatch");
    write(&root, "seed.txt", "s\n");
    git(&root, &["add", "seed.txt"]);
    git(&root, &["commit", "-q", "-m", "seed"]);
    let names: Vec<String> = (0..=PATHS_PER_CALL).map(|i| format!("f/{i}.txt")).collect();
    for name in &names {
        write(&root, name, "x\n");
    }
    let small = &names[..PATHS_PER_CALL];
    let staged = |root: &Path| {
        git(root, &["diff", "--cached", "--name-only"])
            .lines()
            .count()
    };

    assert_eq!(stage_paths(&root, small).unwrap(), IndexRoute::Pathspec);
    assert_eq!(staged(&root), PATHS_PER_CALL);
    assert_eq!(stage_paths(&root, &names).unwrap(), IndexRoute::Bulk);
    assert_eq!(staged(&root), names.len());

    assert_eq!(unstage_paths(&root, small).unwrap(), IndexRoute::Pathspec);
    assert_eq!(staged(&root), 1);
    git(&root, &["add", "f"]);
    assert_eq!(unstage_paths(&root, &names).unwrap(), IndexRoute::Bulk);
    assert_eq!(staged(&root), 0);
    cleanup(&[&root]);
}

/// 暂存:有一条拿不准,整批不走快路径;拿得准的那批照旧收(判据不误伤)。
#[test]
fn 暂存拿不准的整批退回旧路径() {
    let root = new_repo("stage-taint");
    write(&root, "seed.txt", "s\n");
    git(&root, &["add", "seed.txt"]);
    git(&root, &["commit", "-q", "-m", "seed"]);
    let base: Vec<String> = (0..70).map(|i| format!("b/{i}.txt")).collect();
    for name in &base {
        write(&root, name, "x\n");
    }
    let with = |extra: &str| {
        let mut files = base.clone();
        files.push(extra.to_string());
        files
    };
    assert!(plan_stage(&root, &base).is_some());

    // 路径本身不规矩(尾斜杠 = 目录 pathspec)
    for bad in [
        "b//0.txt",
        "./b/0.txt",
        "b/",
        "b/0.txt.",
        ".git/config",
        "b\\0.txt",
    ] {
        assert!(plan_stage(&root, &with(bad)).is_none(), "{bad}");
    }
    // 点名的被忽略文件 / 不存在的未跟踪文件 / 目录本身
    write(&root, ".gitignore", "*.log\n");
    write(&root, "x.log", "log\n");
    for bad in ["x.log", "gone.txt", "b"] {
        assert!(plan_stage(&root, &with(bad)).is_none(), "{bad}");
    }
    // 子模块 gitlink,以及子模块里的路径
    let head = git(&root, &["rev-parse", "HEAD"]);
    let gitlink = format!("160000,{head},sub");
    git(&root, &["update-index", "--add", "--cacheinfo", &gitlink]);
    write(&root, "sub/inner.txt", "i\n");
    for bad in ["sub", "sub/inner.txt"] {
        assert!(plan_stage(&root, &with(bad)).is_none(), "{bad}");
    }
    // 已跟踪的文件变成了装着新文件的目录(`add` 会连目录里的一起暂存)
    write(&root, "d", "file\n");
    git(&root, &["add", "d"]);
    std::fs::remove_file(root.join("d")).unwrap();
    write(&root, "d/in.txt", "in\n");
    for bad in ["d", "d/in.txt"] {
        assert!(plan_stage(&root, &with(bad)).is_none(), "{bad}");
    }
    // 冲突中 / skip-worktree / assume-unchanged / intent-to-add
    for name in ["c.txt", "sw.txt", "au.txt"] {
        write(&root, name, "v1\n");
        git(&root, &["add", name]);
        write(&root, name, "v2\n");
    }
    make_conflict(&root, "c.txt");
    git(&root, &["update-index", "--skip-worktree", "sw.txt"]);
    git(&root, &["update-index", "--assume-unchanged", "au.txt"]);
    write(&root, "ita.txt", "ita\n");
    git(&root, &["add", "-N", "ita.txt"]);
    for bad in ["c.txt", "sw.txt", "au.txt", "ita.txt"] {
        assert!(plan_stage(&root, &with(bad)).is_none(), "{bad}");
    }
    // core.ignorecase 下目录名大小写与暂存区里的不一样(大小写不敏感的盘上它落进 Mixed/,
    // 列表里对不上;敏感的盘上由大小写折叠判据挡)
    git(&root, &["config", "core.ignorecase", "true"]);
    write(&root, "Mixed/a.txt", "a\n");
    git(&root, &["add", "Mixed/a.txt"]);
    write(&root, "mixed/b.txt", "b\n");
    assert!(plan_stage(&root, &with("mixed/b.txt")).is_none());

    // 上面这些都不在批里时,这批照旧收
    assert!(plan_stage(&root, &base).is_some());
    cleanup(&[&root]);
}

/// 取消暂存:pathspec 会命中别的条目、冲突中、skip-worktree 的,整批不走快路径。
#[test]
fn 取消暂存拿不准的整批退回旧路径() {
    let root = new_repo("unstage-taint");
    let base: Vec<String> = (0..70).map(|i| format!("dir/f{i}.txt")).collect();
    for name in &base {
        write(&root, name, "v1\n");
    }
    for name in ["tree/x.txt", "gone/x.txt", "sw.txt", "c.txt", "p.txt"] {
        write(&root, name, "v1\n");
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    for name in &base {
        write(&root, name, "v2\n");
    }
    git(&root, &["add", "dir"]);
    // HEAD 里有、暂存区里整个摘掉了的目录:pathspec `gone` 会命中 HEAD 那一侧的 gone/x.txt
    git(&root, &["rm", "-q", "-r", "--cached", "gone"]);
    let with = |extra: &str| {
        let mut files = base.clone();
        files.push(extra.to_string());
        files
    };
    assert!(plan_unstage(&root, &base).is_some());

    // HEAD 里是目录 / 暂存区里有条目在它下面 / 它的上级是条目 / 不规矩
    write(&root, "new/a.txt", "a\n");
    git(&root, &["add", "new"]);
    for bad in ["tree", "gone", "new", "p.txt/x", "dir/", "dir//f0.txt"] {
        assert!(plan_unstage(&root, &with(bad)).is_none(), "{bad}");
    }
    // 冲突中 / skip-worktree
    make_conflict(&root, "c.txt");
    git(&root, &["update-index", "--skip-worktree", "sw.txt"]);
    for bad in ["c.txt", "sw.txt"] {
        assert!(plan_unstage(&root, &with(bad)).is_none(), "{bad}");
    }
    assert!(plan_unstage(&root, &base).is_some());
    cleanup(&[&root]);
}

/// 判据收下了、git 自己失败(必需的 clean 过滤器报错,die 在写暂存区之前):整批重跑旧路径,
/// 暂存区与报错原文都与只走旧路径时一样 —— 什么都没暂存。
#[test]
fn 快路径的_git_失败时整批重跑旧路径() {
    let scene = |tag: &str| {
        let root = new_repo(tag);
        git(&root, &["config", "filter.bad.clean", "false"]);
        git(&root, &["config", "filter.bad.required", "true"]);
        write(&root, ".gitattributes", "*.bad filter=bad\n");
        git(&root, &["add", ".gitattributes"]);
        git(&root, &["commit", "-q", "-m", "attrs"]);
        let mut files: Vec<String> = (0..70).map(|i| format!("f/{i}.txt")).collect();
        files.push("x.bad".to_string());
        for name in &files {
            write(&root, name, "x\n");
        }
        (root, files)
    };
    let (old, files) = scene("fail-old");
    let (new, _) = scene("fail-new");
    assert!(
        plan_stage(&new, &files).is_some(),
        "判据层面是收的,失败出在 git 里"
    );
    let before = snapshot(&new);

    let old_err = run_with_literal_paths(&old, &["add", "-A"], &files, "add").unwrap_err();
    let new_err = stage_paths(&new, &files).unwrap_err();
    assert_eq!(format!("{new_err:#}"), format!("{old_err:#}"));
    assert_eq!(snapshot(&new), before, "什么都没暂存");
    assert_same(&new, &old);
    cleanup(&[&old, &new]);
}

// ── 判据的小件 ───────────────────────────────────────────────

#[test]
fn 规矩路径的判定() {
    for ok in [
        "a.txt",
        "dir/中文.txt",
        "[ab].txt",
        "-x",
        "sp ace/x",
        ".gitignore",
        ".gitattributes",
        ".github/workflows/ci.yml",
        "paper.aux",
    ] {
        assert!(plain_path(ok), "{ok}");
    }
    for bad in [
        "",
        "/a",
        "a/",
        "a//b",
        "./a",
        "a/../b",
        ".git",
        ".GIT/x",
        "x/.git",
        "git~1/x",
        ".git./x",
        "a.",
        "a ",
        "a\\b",
        "c:x",
        "a\tb",
        ".gitmodules",
        ".g\u{200c}it/x",
        "\u{fffd}.txt",
    ] {
        assert!(!plain_path(bad), "{bad:?}");
    }
    // Windows 的设备名与不许出现的字符;别的平台上它们是普通文件名
    for name in ["con", "aux.txt", "dir/COM1.log", "lpt9", "a*b", "q?"] {
        assert_eq!(plain_path(name), !cfg!(windows), "{name}");
    }
}

#[test]
fn 大小写折叠判得出来() {
    let view = |paths: &[&str]| {
        let mut index = IndexView {
            entries: HashMap::new(),
            conflicted: HashSet::new(),
            dirs: HashSet::new(),
        };
        for path in paths {
            add_parents(&mut index.dirs, path);
            let entry = StagedEntry {
                mode: 0o100644,
                id: Oid::zero(),
                plain: true,
            };
            index.entries.insert(path.to_string(), entry);
        }
        index
    };
    let index = view(&["Dir/a.txt", "Top.txt", "x/Mixed/b.txt"]);
    assert!(!case_folds(
        &index,
        &["Dir/b.txt", "new/c.txt", "x/Mixed/c.txt", "中文.txt"]
    ));
    for folded in [
        "dir/b.txt",
        "top.txt",
        "x/mixed/c.txt",
        "X/Mixed/c.txt",
        "dir",
    ] {
        assert!(case_folds(&index, &[folded]), "{folded}");
    }
    // 同一个目录在暂存区里有两种写法:哪种都不算对得上
    let both = view(&["Dir/a.txt", "dir/b.txt"]);
    assert!(case_folds(&both, &["Dir/c.txt"]));
}
