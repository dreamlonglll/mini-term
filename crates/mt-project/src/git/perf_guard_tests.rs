//! 数据层性能护栏的单测:status 单飞、重命名配对护栏、diff 两侧的大小上限、diff 块与
//! `git diff` 对账、仓库发现的跳过名单、批量路径走 stdin、已跟踪的被忽略文件。
//! 全部走临时仓库,不联网。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::test_support::{cleanup, git, init_repo_with_file};
use super::*;

// ── status 单飞 ───────────────────────────────────────────────

/// 等到 `cond` 成立(最多 10s),否则失败。
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "等不到:{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 在途的一趟早于请求开始:请求拿不到它的结果,排到下一趟;下一趟由期间到达的全部
/// 请求共享 —— 三个请求一共只算两趟。
#[test]
fn 单飞只交出请求之后才开始的那一趟() {
    let flight = Arc::new(SingleFlight::<u64>::default());
    let runs = Arc::new(AtomicU64::new(0));
    let release = Arc::new((Mutex::new(false), parking_lot::Condvar::new()));

    let leader = {
        let (flight, runs, release) = (flight.clone(), runs.clone(), release.clone());
        std::thread::spawn(move || {
            flight.run(|| {
                let n = runs.fetch_add(1, Ordering::SeqCst) + 1;
                let (lock, cv) = &*release;
                let mut go = lock.lock();
                while !*go {
                    cv.wait(&mut go);
                }
                n
            })
        })
    };
    wait_for("第一趟开始", || runs.load(Ordering::SeqCst) == 1);

    let late: Vec<_> = (0..2)
        .map(|_| {
            let (flight, runs) = (flight.clone(), runs.clone());
            std::thread::spawn(move || flight.run(|| runs.fetch_add(1, Ordering::SeqCst) + 1))
        })
        .collect();
    wait_for("两个请求排上队", || flight.waiting() == 2);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "在途期间不许另起一趟");

    *release.0.lock() = true;
    release.1.notify_all();
    assert_eq!(leader.join().unwrap(), 1);
    for handle in late {
        assert_eq!(
            handle.join().unwrap(),
            2,
            "拿到的必须是请求之后才开始的那一趟"
        );
    }
    assert_eq!(runs.load(Ordering::SeqCst), 2, "排队的两个请求共享一趟");
}

/// 集合窗口:领跑者开算前到达的请求搭同一趟;开算之后到达的照旧排下一趟。
#[test]
fn 单飞集合窗口里到达的请求搭同一趟() {
    let flight = Arc::new(SingleFlight::<u64>::with_gather(Duration::from_millis(300)));
    let runs = Arc::new(AtomicU64::new(0));
    let release = Arc::new((Mutex::new(false), parking_lot::Condvar::new()));
    let blocked = |flight: Arc<SingleFlight<u64>>,
                   runs: Arc<AtomicU64>,
                   release: Arc<(Mutex<bool>, parking_lot::Condvar)>| {
        std::thread::spawn(move || {
            flight.run(|| {
                let n = runs.fetch_add(1, Ordering::SeqCst) + 1;
                let (lock, cv) = &*release;
                let mut go = lock.lock();
                while !*go {
                    cv.wait(&mut go);
                }
                n
            })
        })
    };
    let first = blocked(flight.clone(), runs.clone(), release.clone());
    wait_for("领跑者进入集合窗口", || flight.is_gathering());
    let joined = blocked(flight.clone(), runs.clone(), release.clone());
    wait_for("第一趟开算", || runs.load(Ordering::SeqCst) == 1);
    assert!(!flight.is_gathering());
    // 开算之后才到的:不能搭这一趟
    let late = blocked(flight.clone(), runs.clone(), release.clone());
    wait_for("两个请求在等", || flight.waiting() == 2);
    *release.0.lock() = true;
    release.1.notify_all();
    assert_eq!(first.join().unwrap(), 1);
    assert_eq!(joined.join().unwrap(), 1, "窗口里到的搭同一趟");
    assert_eq!(late.join().unwrap(), 2, "开算之后到的排下一趟");
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

/// 一前一后的请求各算各的:算完的结果不按时间留给后来的请求。
#[test]
fn 单飞不复用已算完的结果() {
    let flight = SingleFlight::<u64>::default();
    let runs = AtomicU64::new(0);
    let next = || runs.fetch_add(1, Ordering::SeqCst) + 1;
    assert_eq!(flight.run(next), 1);
    assert_eq!(flight.run(next), 2);
    assert_eq!(flight.run(next), 3);
}

/// 领跑的那一趟 panic:后面的请求不会永远等下去,由它们接着算。
#[test]
fn 单飞领跑者_panic_不卡死后来的请求() {
    let flight = Arc::new(SingleFlight::<u64>::default());
    let started = Arc::new(AtomicU64::new(0));
    let release = Arc::new((Mutex::new(false), parking_lot::Condvar::new()));
    let leader = {
        let (flight, started, release) = (flight.clone(), started.clone(), release.clone());
        std::thread::spawn(move || {
            flight.run(|| {
                started.store(1, Ordering::SeqCst);
                let (lock, cv) = &*release;
                let mut go = lock.lock();
                while !*go {
                    cv.wait(&mut go);
                }
                panic!("模拟计算中途失败");
            })
        })
    };
    wait_for("第一趟开始", || started.load(Ordering::SeqCst) == 1);
    let waiter = {
        let flight = flight.clone();
        std::thread::spawn(move || flight.run(|| 42))
    };
    wait_for("请求排上队", || flight.waiting() == 1);
    *release.0.lock() = true;
    release.1.notify_all();
    assert!(leader.join().is_err());
    assert_eq!(waiter.join().unwrap(), 42);
    assert_eq!(flight.run(|| 7), 7, "之后照常能用");
}

/// 「更改」区与文件树共用一趟 status,但每次请求都看得到请求之前的改动。
#[test]
fn 更改区与文件树每次都看到最新改动() {
    let root = init_repo_with_file("flight-fresh", "src/a.txt", "a\n");
    std::fs::write(root.join("src/a.txt"), "changed\n").unwrap();
    let changes = get_changes_status(&root).unwrap();
    assert_eq!(changes.len(), 1);
    let tree = get_git_status(&root).unwrap();
    assert_eq!(tree.len(), 1);

    std::fs::write(root.join("new.txt"), "n\n").unwrap();
    let paths = |rows: Vec<ChangeFileStatus>| -> Vec<String> {
        let mut p: Vec<String> = rows.into_iter().map(|c| c.path).collect();
        p.sort();
        p
    };
    assert_eq!(
        paths(get_changes_status(&root).unwrap()),
        vec!["new.txt".to_string(), "src/a.txt".to_string()]
    );
    assert_eq!(get_git_status(&root).unwrap().len(), 2);

    // 并发的两路(像面板与文件树同时收到一次变化)结果一致
    let (r1, r2) = (root.clone(), root.clone());
    let a = std::thread::spawn(move || get_changes_status(&r1).unwrap().len());
    let b = std::thread::spawn(move || get_git_status(&r2).unwrap().len());
    assert_eq!((a.join().unwrap(), b.join().unwrap()), (2, 2));
    cleanup(&[&root]);
}

/// 项目是仓库的子目录:文件树只报项目根以下的(路径相对项目根),「更改」区照旧是整仓。
#[test]
fn 文件树在共享的整仓结果上按项目根筛() {
    let root = init_repo_with_file("flight-sub", "pkg/app/a.txt", "a\n");
    std::fs::write(root.join("pkg/app/a.txt"), "x\n").unwrap();
    std::fs::write(root.join("outside.txt"), "o\n").unwrap();
    let project = root.join("pkg").join("app");
    let tree: Vec<String> = get_git_status(&project)
        .unwrap()
        .into_iter()
        .map(|s| s.path)
        .collect();
    assert_eq!(tree, vec!["a.txt".to_string()]);
    assert_eq!(get_changes_status(&root).unwrap().len(), 2);
    cleanup(&[&root]);
}

// ── 重命名配对护栏 ────────────────────────────────────────────

#[test]
fn 重命名档位按删除乘新增分() {
    assert_eq!(rename_tier(0, 5), RenameTier::Nothing);
    assert_eq!(rename_tier(5, 0), RenameTier::Nothing);
    assert_eq!(rename_tier(1, 1), RenameTier::Similar);
    assert_eq!(rename_tier(200, 200), RenameTier::Similar);
    assert_eq!(rename_tier(201, 200), RenameTier::ExactOnly);
    assert_eq!(rename_tier(1, 40_001), RenameTier::ExactOnly);
    assert_eq!(rename_tier(5000, 5000), RenameTier::ExactOnly);
    assert_eq!(rename_tier(5001, 5000), RenameTier::Skip);
    assert_eq!(rename_tier(u64::MAX, 2), RenameTier::Skip, "乘法不溢出");
}

/// 一个目录里 `n` 个内容各不相同的文件,提交后整个目录 `git mv` 走;`edited` 个文件
/// 在新位置再改一行。返回仓库根。
fn mass_move_repo(tag: &str, n: usize, edited: usize) -> PathBuf {
    let root = init_repo_with_file(tag, "keep.txt", "keep\n");
    std::fs::create_dir_all(root.join("a")).unwrap();
    for i in 0..n {
        let body: String = (0..12).map(|k| format!("file {i} line {k}\n")).collect();
        std::fs::write(root.join("a").join(format!("f{i}.txt")), body).unwrap();
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "files"]);
    git(&root, &["mv", "a", "b"]);
    for i in 0..edited {
        let path = root.join("b").join(format!("f{i}.txt"));
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("line 5\n", "LINE 5\n")).unwrap();
    }
    git(&root, &["add", "-A"]);
    root
}

/// 候选少(删除 × 新增在相似度档内):改名又改了一行的也照样配成一行 R,与旧行为一致。
#[test]
fn 少量重命名照旧做相似度配对() {
    let root = mass_move_repo("rename-small", 30, 3);
    let rows = get_changes_status(&root).unwrap();
    assert_eq!(rows.len(), 30, "{rows:?}");
    assert!(
        rows.iter()
            .all(|r| r.staged_status == Some(GitStatus::Renamed) && r.old_path.is_some())
    );
    // 改过的那几行:旧路径对得上
    let edited = rows.iter().find(|r| r.path == "b/f0.txt").unwrap();
    assert_eq!(edited.old_path.as_deref(), Some("a/f0.txt"));
    cleanup(&[&root]);
}

/// 候选多(超出相似度档):纯改名**全部**配上(旧实现被 `diff.renameLimit` 截断,1100 个
/// 只配上 999 个),改名又改了内容的不再配(照「删除 + 新增」两行列),换来不卡顿。
/// 提交之后提交详情的文件列表同一口径。
#[test]
fn 大量重命名只做精确配对且不截断() {
    let n = 1100;
    let root = mass_move_repo("rename-many", n, 1);
    let rows = get_changes_status(&root).unwrap();
    let renamed = rows
        .iter()
        .filter(|r| r.staged_status == Some(GitStatus::Renamed))
        .count();
    assert_eq!(renamed, n - 1, "纯改名全部配上");
    let added: Vec<&str> = rows
        .iter()
        .filter(|r| r.staged_status == Some(GitStatus::Added))
        .map(|r| r.path.as_str())
        .collect();
    let deleted: Vec<&str> = rows
        .iter()
        .filter(|r| r.staged_status == Some(GitStatus::Deleted))
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!((added, deleted), (vec!["b/f0.txt"], vec!["a/f0.txt"]));
    // 取消暂存时补旧路径(`with_rename_sources`)走同一套配对
    let sources = with_rename_sources(&root, &["b/f7.txt".to_string()]).unwrap();
    assert_eq!(
        sources,
        vec!["b/f7.txt".to_string(), "a/f7.txt".to_string()]
    );

    git(&root, &["commit", "-q", "-m", "move"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let files = get_commit_files(&root, &head).unwrap();
    let renamed = files.iter().filter(|f| f.status == "renamed").count();
    assert_eq!(renamed, n - 1);
    assert_eq!(files.len(), n + 1);
    cleanup(&[&root]);
}

/// 重命名之后旧路径又放了一个未跟踪文件:一行 R + 一行未跟踪(与 `git status` 一致,
/// 也与 libgit2 status 自己配对时的形态一致)。
#[test]
fn 重命名后旧路径上的未跟踪文件单独成行() {
    let body = "1\n2\n3\n4\n5\n6\n7\n8\n";
    let root = init_repo_with_file("rename-reuse", "old.txt", body);
    git(&root, &["mv", "old.txt", "new.txt"]);
    std::fs::write(root.join("old.txt"), "fresh\n").unwrap();
    type Row = (String, Option<String>, Option<GitStatus>, Option<GitStatus>);
    let mut rows: Vec<Row> = get_changes_status(&root)
        .unwrap()
        .into_iter()
        .map(|c| (c.path, c.old_path, c.staged_status, c.unstaged_status))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        rows,
        vec![
            (
                "new.txt".to_string(),
                Some("old.txt".to_string()),
                Some(GitStatus::Renamed),
                None
            ),
            (
                "old.txt".to_string(),
                None,
                None,
                Some(GitStatus::Untracked)
            ),
        ]
    );
    let porcelain = git(&root, &["status", "--porcelain"]);
    let mut lines: Vec<&str> = porcelain.lines().map(str::trim).collect();
    lines.sort();
    assert_eq!(lines, vec!["?? old.txt", "R  old.txt -> new.txt"]);
    cleanup(&[&root]);
}

// ── diff 两侧的大小上限 ───────────────────────────────────────

/// 超过 1 MB 的文本(每行不同,确保不是被当成二进制)。
fn big_text() -> String {
    (0..60_000).map(|i| format!("line number {i}\n")).collect()
}

/// 删掉一个超限的文本文件再看 diff:旧侧(暂存区 / HEAD)超限同样报「过大」,不再整份
/// 读进内存去比(旧实现旧侧不限)。暂存删除、提交之后也一样。
#[test]
fn diff_旧侧超限也报过大() {
    let root = init_repo_with_file("big-old", "small.txt", "s\n");
    std::fs::write(root.join("big.txt"), big_text()).unwrap();
    git(&root, &["add", "big.txt"]);
    git(&root, &["commit", "-q", "-m", "big"]);

    std::fs::remove_file(root.join("big.txt")).unwrap();
    for side in [DiffSide::Unstaged, DiffSide::Working] {
        let diff = get_git_diff(&root, "big.txt", side).unwrap();
        assert!(diff.too_large && diff.hunks.is_empty(), "{side:?}");
    }
    git(&root, &["rm", "-q", "--cached", "big.txt"]);
    let staged = get_git_diff(&root, "big.txt", DiffSide::Staged).unwrap();
    assert!(staged.too_large);

    git(&root, &["commit", "-q", "-m", "drop big"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let commit_diff = get_commit_file_diff(&root, &head, "big.txt", None).unwrap();
    assert!(commit_diff.too_large);

    // 新侧(工作区)超限:量大小就够了,照旧报过大
    std::fs::write(root.join("again.txt"), big_text()).unwrap();
    let diff = get_git_diff(&root, "again.txt", DiffSide::Unstaged).unwrap();
    assert!(diff.too_large);
    // 编辑器 gutter 的 HEAD 取数同一条上限
    git(&root, &["add", "again.txt"]);
    git(&root, &["commit", "-q", "-m", "again"]);
    assert!(matches!(
        head_file(&root, &root.join("again.txt")).unwrap(),
        HeadFile::TooLarge(_)
    ));
    cleanup(&[&root]);
}

/// 上限以内的照常出 diff(大小判定没有误伤)。
#[test]
fn diff_上限以内照常比() {
    let root = init_repo_with_file("big-ok", "a.txt", "a\nb\nc\n");
    std::fs::write(root.join("a.txt"), "a\nB\nc\n").unwrap();
    for side in [DiffSide::Unstaged, DiffSide::Working] {
        let diff = get_git_diff(&root, "a.txt", side).unwrap();
        assert!(!diff.too_large && diff.hunks.len() == 1, "{side:?}");
    }
    cleanup(&[&root]);
}

// ── diff 块与 git diff 对账 ──────────────────────────────────

/// 编辑序列 → git `-U0` 的块头 `(旧起点, 旧行数, 新起点, 新行数)`(行数为 0 时起点是
/// 前一行,与 git 一致)。
fn u0_blocks(ops: &[(char, usize, usize)]) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ops.len() {
        if ops[i].0 == '=' {
            i += 1;
            continue;
        }
        let (_, oi, ni) = ops[i];
        let (mut dels, mut adds) = (0, 0);
        while i < ops.len() && ops[i].0 != '=' {
            if ops[i].0 == '-' {
                dels += 1;
            } else {
                adds += 1;
            }
            i += 1;
        }
        let old_start = if dels > 0 { oi + 1 } else { oi };
        let new_start = if adds > 0 { ni + 1 } else { ni };
        out.push((old_start, dels, new_start, adds));
    }
    out
}

/// `git diff --no-index -U0` 的块头。算法与缩进启发式钉死(不吃机器上的全局配置)。
fn git_u0_blocks(dir: &Path, old: &str, new: &str) -> Vec<(usize, usize, usize, usize)> {
    std::fs::write(dir.join("old.txt"), old).unwrap();
    std::fs::write(dir.join("new.txt"), new).unwrap();
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "diff.algorithm=myers",
            "-c",
            "diff.indentHeuristic=true",
            "-c",
            "core.autocrlf=false",
            "diff",
            "--no-index",
            "--no-color",
            "-U0",
            "old.txt",
            "new.txt",
        ])
        .current_dir(dir)
        .output()
        .expect("git 起不来");
    let text = String::from_utf8_lossy(&out.stdout);
    let range = |s: &str| -> (usize, usize) {
        match s.split_once(',') {
            Some((start, len)) => (start.parse().unwrap(), len.parse().unwrap()),
            None => (s.parse().unwrap(), 1),
        }
    };
    text.lines()
        .filter_map(|line| line.strip_prefix("@@ -"))
        .map(|rest| {
            let (old, rest) = rest.split_once(" +").unwrap();
            let new = rest.split_once(" @@").unwrap().0;
            let ((a, b), (c, d)) = (range(old), range(new));
            (a, b, c, d)
        })
        .collect()
}

/// diff 弹窗的块(LCS / Myers 两条路都覆盖到)与 `git diff --no-index` 逐块一致:
/// 歧义滑动(块可上可下)、重复行、首尾相同的块、大文件(中段大,走 Myers)。
#[test]
fn diff_块与_git_diff_一致() {
    let dir = std::env::temp_dir().join(format!(
        "mini-term-git-u0-{}",
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let numbered = |n: usize| -> String { (0..n).map(|i| format!("line {i}\n")).collect() };
    let big = numbered(3000);
    let mut big_edited: Vec<String> = big.lines().map(str::to_string).collect();
    for at in [5, 700, 701, 1500, 2990] {
        big_edited[at] = format!("edited {at}");
    }
    big_edited.insert(1200, "inserted".to_string());
    big_edited.remove(2500);
    let big_edited: String = big_edited.iter().map(|l| format!("{l}\n")).collect();
    let cases: Vec<(String, String)> = vec![
        // 歧义滑动:两个函数之间插一个函数(块带着空行,可上可下)
        (
            "fn a() {\n}\n\nfn c() {\n}\n".into(),
            "fn a() {\n}\n\nfn b() {\n}\n\nfn c() {\n}\n".into(),
        ),
        // 重复行:插一行重复的、删一行重复的
        ("a\nb\nb\nc\n".into(), "a\nb\nb\nb\nc\n".into()),
        ("x\n}\n}\n}\ny\n".into(), "x\n}\n}\ny\n".into()),
        // 首尾相同的块:中间改一段,首尾各有一段一模一样的
        (
            "{\n  same\n}\nmid1\nmid2\n{\n  same\n}\n".into(),
            "{\n  same\n}\nMID\n{\n  same\n}\n".into(),
        ),
        // 尾部追加一个函数,前面另有改动
        (
            "x\nfunction a() {\n  return 1;\n}\n".into(),
            "X\nfunction a() {\n  return 1;\n}\n\nfunction b() {\n  return 2;\n}\n".into(),
        ),
        // 空行之间插一段
        ("a\n\nb\n".into(), "a\n\nc\n\nb\n".into()),
        // 大文件散落几处改动(中段 3000×3000,超过先跑 LCS 的档,走 Myers)
        (big.clone(), big_edited),
        // 大文件首尾各改一行:旧实现中段 1600 万格越过 LCS 上限,整份退化成一块
        (
            numbered(4000),
            numbered(4000).replacen("line 0\n", "LINE 0\n", 1).replacen(
                "line 3999\n",
                "LINE 3999\n",
                1,
            ),
        ),
    ];
    for (old, new) in &cases {
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();
        let ops = line_diff::diff_ops(&old_lines, &new_lines).expect("不该退化");
        assert_eq!(
            u0_blocks(&ops),
            git_u0_blocks(&dir, old, new),
            "旧:{:?}… 新:{:?}…",
            old.chars().take(60).collect::<String>(),
            new.chars().take(60).collect::<String>()
        );
        // 弹窗拿到的块与编辑序列同一来源:块的增删行数对得上
        let result = diff_two_texts(old.clone(), new.clone());
        let changed: usize = result
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter(|l| l.kind != "context")
            .count();
        let expect: usize = u0_blocks(&ops).iter().map(|b| b.1 + b.3).sum();
        assert_eq!(changed, expect);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// 中段小(先跑 LCS 的档以内)时结果与只跑 LCS 时逐条相同 —— 小改动的块一行不挪。
#[test]
fn 小改动的编辑序列与_lcs_相同() {
    let mut seed: u64 = 0x5eed_1234_abcd_0001;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let alphabet = ["", "}", "{", "  a", "  b", "x", "fn f() {", "\tz"];
    for _ in 0..300 {
        let a: Vec<&str> = (0..next() % 30)
            .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
            .collect();
        let b: Vec<&str> = (0..next() % 30)
            .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
            .collect();
        let lcs = slide::compact_ops(&edit_script(&a, &b), &a, &b);
        assert_eq!(line_diff::diff_ops(&a, &b).unwrap(), lcs, "{a:?} → {b:?}");
    }
}

// ── 仓库发现 ─────────────────────────────────────────────────

/// 项目根不是仓库时往下扫:普通子目录里的仓库照收;依赖 / 缓存目录(`.venv` 等)里的
/// 不收;没有 `.git` 的目录不当仓库。
#[test]
fn 仓库发现跳过依赖目录() {
    let ts = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("mini-term-git-discover-{ts}"));
    for dir in ["svc/api", "libs/core", ".venv/src/pkg", "plain/deep/er"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    for repo in ["svc/api", "libs/core", ".venv/src/pkg"] {
        Repository::init(root.join(repo)).unwrap();
    }
    let found: Vec<String> = discover_repo_paths(&root)
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(found, vec!["core".to_string(), "api".to_string()]);
    std::fs::remove_dir_all(&root).ok();
}

// ── 批量路径走 stdin ─────────────────────────────────────────

/// 一个批次装不下的路径数(含会被当成通配 / 选项 / 带空格的文件名):暂存、取消暂存、
/// 丢弃都一次跑完,结果与逐个跑一致;字面路径口径不变。
#[test]
fn 大批路径从_stdin_喂给_git() {
    let root = init_repo_with_file("many-paths", "base.txt", "b\n");
    let mut names: Vec<String> = (0..150).map(|i| format!("dir/f{i}.txt")).collect();
    names.extend(
        ["-dash.txt", "star*.txt", "q?.txt", "sp ace.txt", "中文.txt"]
            .iter()
            .map(|s| s.to_string()),
    );
    std::fs::create_dir_all(root.join("dir")).unwrap();
    for name in &names {
        if !cfg!(windows) || !name.contains(['*', '?']) {
            std::fs::write(root.join(name), "v1\n").unwrap();
        }
    }
    let names: Vec<String> = names
        .into_iter()
        .filter(|n| root.join(n).exists())
        .collect();
    assert!(names.len() > PATHS_PER_CALL);
    // 一个同名但不在列表里的诱饵:通配被展开的话它会被一起暂存
    std::fs::write(root.join("dir").join("f1.txt.bak"), "x\n").unwrap();

    git_stage(&root, &names).unwrap();
    let staged: HashSet<String> = git(&root, &["diff", "--cached", "--name-only", "-z"])
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let wanted: HashSet<String> = names.iter().cloned().collect();
    assert_eq!(staged, wanted);

    git_unstage(&root, &names).unwrap();
    assert!(git(&root, &["diff", "--cached", "--name-only"]).is_empty());

    // 丢弃(已跟踪的从暂存区检出):先提交,再改,再丢弃
    git_stage(&root, &names).unwrap();
    git(&root, &["commit", "-q", "-m", "many"]);
    for name in &names {
        std::fs::write(root.join(name), "v2\n").unwrap();
    }
    discard_unstaged(&root, &names).unwrap();
    for name in &names {
        let text = std::fs::read_to_string(root.join(name)).unwrap();
        assert_eq!(text.replace("\r\n", "\n"), "v1\n", "{name}");
    }
    cleanup(&[&root]);
}

/// git 不认 `--pathspec-from-file`(老版本)时是用法错误 129:认出来交回 `false`,
/// 调用方退回按批起进程。这里拿一个不存在的选项造同一种退出码。
#[test]
fn stdin_路径不被支持时认得出来() {
    let root = init_repo_with_file("stdin-old-git", "a.txt", "a\n");
    let supported = run_paths_via_stdin(
        &root,
        &["add", "--definitely-not-an-option"],
        &["a.txt"],
        "add",
        Duration::from_secs(30),
    )
    .unwrap();
    assert!(!supported);
    // 别的失败(路径对不上)照常是错误,不被当成「不支持」
    let err = run_paths_via_stdin(
        &root,
        &["add"],
        &["no-such-file.txt"],
        "add",
        Duration::from_secs(30),
    );
    assert!(err.is_err());
    cleanup(&[&root]);
}

// ── 已跟踪但匹配 .gitignore 的文件 ───────────────────────────

/// 提交过、后来又写进 `.gitignore` 的文件被改:`git status` 照样显示 M,工作区监听的
/// 过滤也得认它;没跟踪的被忽略文件照旧不算。
#[test]
fn 已跟踪的被忽略文件改动不算无关() {
    let root = init_repo_with_file("ignored-tracked", "app.log", "v1\n");
    std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(root.join("app.log"), "v2\n").unwrap();
    std::fs::write(root.join("other.log"), "x\n").unwrap();
    assert!(git(&root, &["status", "--porcelain"]).contains("M app.log"));
    assert!(any_unignored_change(&root, &[root.join("app.log")]).unwrap());
    assert!(!any_unignored_change(&root, &[root.join("other.log")]).unwrap());
    assert!(any_unignored_change(&root, &[root.join("other.log"), root.join("app.log")]).unwrap());
    cleanup(&[&root]);
}
