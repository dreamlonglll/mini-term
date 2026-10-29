use super::super::test_support::init_repo_with_file;
use super::*;

fn hunk(
    kind: LineChangeKind,
    new_start: usize,
    new_len: usize,
    old_start: usize,
    old_len: usize,
) -> LineHunk {
    LineHunk {
        kind,
        new_start,
        new_len,
        old_start,
        old_len,
    }
}

use LineChangeKind::{Added, Deleted, Modified};

#[test]
fn 没改动就没有标记() {
    assert!(line_hunks("a\nb\nc\n", "a\nb\nc\n").is_empty());
    // 末尾换行有无不算差异(与 diff_two_texts 同一口径)
    assert!(line_hunks("a\nb", "a\nb\n").is_empty());
    assert!(line_hunks("", "").is_empty());
}

#[test]
fn 新增_修改_删除三种块() {
    let old = "a\nb\nc\nd\ne\n";
    // 在 b 后插两行
    assert_eq!(
        line_hunks(old, "a\nb\nx\ny\nc\nd\ne\n"),
        vec![hunk(Added, 2, 2, 2, 0)]
    );
    // 改 c
    assert_eq!(
        line_hunks(old, "a\nb\nC\nd\ne\n"),
        vec![hunk(Modified, 2, 1, 2, 1)]
    );
    // 删 c、d:删除点在新侧第 2 行(e)之前
    assert_eq!(
        line_hunks(old, "a\nb\ne\n"),
        vec![hunk(Deleted, 2, 0, 2, 2)]
    );
}

#[test]
fn 文件首尾的删除与新增() {
    let old = "a\nb\nc\n";
    // 删首行:删除点 = 新侧第 0 行之前
    assert_eq!(line_hunks(old, "b\nc\n"), vec![hunk(Deleted, 0, 0, 0, 1)]);
    // 删末行:删除点 = 新侧行数(文件末尾)
    assert_eq!(line_hunks(old, "a\nb\n"), vec![hunk(Deleted, 2, 0, 2, 1)]);
    // 末尾追加
    assert_eq!(
        line_hunks(old, "a\nb\nc\nd\n"),
        vec![hunk(Added, 3, 1, 3, 0)]
    );
    // 整份清空 / 从无到有
    assert_eq!(line_hunks(old, ""), vec![hunk(Deleted, 0, 0, 0, 3)]);
    assert_eq!(line_hunks("", "x\ny\n"), vec![hunk(Added, 0, 2, 0, 0)]);
}

#[test]
fn 多块按新侧行号升序且互不重叠() {
    let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
    let new = "1\nX\n3\n4\n5\n6\n7\n8\n9\nY\n";
    let hunks = line_hunks(old, new);
    assert_eq!(
        hunks,
        vec![hunk(Modified, 1, 1, 1, 1), hunk(Added, 9, 1, 9, 0)]
    );

    // 删 + 改 + 增混在一起
    let new = "1\n3\n4\nFOUR\n5\n6\n7\n8\n9\nextra\n";
    let hunks = line_hunks(old, new);
    let kinds: Vec<_> = hunks.iter().map(|h| h.kind).collect();
    assert_eq!(kinds, vec![Deleted, Added, Added]);
    for pair in hunks.windows(2) {
        assert!(pair[0].new_start + pair[0].new_len <= pair[1].new_start);
    }
}

/// 一行改写在 LCS 里是平局,块里必须同时数到删与增 —— 归成「修改」而不是
/// 一删一增两块。
#[test]
fn 改写一行归成一块修改() {
    let hunks = line_hunks("keep\nold\nkeep2\n", "keep\nnew\nkeep2\n");
    assert_eq!(hunks, vec![hunk(Modified, 1, 1, 1, 1)]);
    // 改两行、增一行:还是一整块修改,新侧 3 行、旧侧 2 行
    let hunks = line_hunks("a\nb\nc\nz\n", "a\nB\nC\nC2\nz\n");
    assert_eq!(hunks, vec![hunk(Modified, 1, 3, 1, 2)]);
}

/// 父模块的 LCS 在这种文件上会越过 `MAX_LCS_CELLS` 退化成整块替换(4000 行首尾各改
/// 一行,中段 4000×4000 = 1600 万格)。Myers 只看编辑距离,照样给出两块精确的标记。
#[test]
fn 大文件首尾各改一行不退化() {
    let old: String = (0..4000).map(|i| format!("line {i}\n")).collect();
    let new = old
        .replacen("line 0\n", "LINE 0\n", 1)
        .replacen("line 3999\n", "LINE 3999\n", 1);
    assert_eq!(
        line_hunks(&old, &new),
        vec![hunk(Modified, 0, 1, 0, 1), hunk(Modified, 3999, 1, 3999, 1)]
    );
}

/// Myers 产出的编辑序列必须合法(按序重放能从旧文本得到新文本)、且与 LCS 一样短
/// (相等行数 = LCS 长度)。用一个确定性的伪随机发生器扫一批小输入。
#[test]
fn myers_与_lcs_一样短且可重放() {
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..400 {
        let len_a = (next() % 12) as usize;
        let len_b = (next() % 12) as usize;
        let alphabet = ["a", "b", "c", "d"];
        let a: Vec<&str> = (0..len_a)
            .map(|_| alphabet[(next() % 4) as usize])
            .collect();
        let b: Vec<&str> = (0..len_b)
            .map(|_| alphabet[(next() % 4) as usize])
            .collect();

        let ops = myers_ops(&a, &b, MYERS_MAX_D).expect("小输入不会超过上限");
        // 重放:'=' 与 '+' 按序拼出新侧,'=' 与 '-' 按序覆盖旧侧
        let rebuilt: Vec<&str> = ops
            .iter()
            .filter(|(k, _, _)| *k != '-')
            .map(|&(_, _, ni)| b[ni])
            .collect();
        assert_eq!(rebuilt, b, "{a:?} → {b:?}: {ops:?}");
        let consumed: Vec<usize> = ops
            .iter()
            .filter(|(k, _, _)| *k != '+')
            .map(|&(_, oi, _)| oi)
            .collect();
        assert_eq!(consumed, (0..a.len()).collect::<Vec<_>>());
        for &(kind, oi, ni) in &ops {
            if kind == '=' {
                assert_eq!(a[oi], b[ni]);
            }
        }

        let equal = |ops: &[(char, usize, usize)]| ops.iter().filter(|o| o.0 == '=').count();
        assert_eq!(
            equal(&ops),
            equal(&edit_script(&a, &b)),
            "{a:?} → {b:?} 不是最短编辑"
        );
    }
}

/// Myers 超过步数上限时回落到 LCS:结果与正常路径一致。
#[test]
fn myers_超限回落到_lcs() {
    let old: Vec<&str> = vec!["a", "b", "c", "d", "e"];
    let new: Vec<&str> = vec!["a", "x", "c", "y", "e", "f"];
    assert!(myers_ops(&old, &new, 1).is_none());
    assert_eq!(
        group_hunks(&line_ops(&old, &new, 1)),
        group_hunks(&line_ops(&old, &new, MYERS_MAX_D))
    );
}

/// 两条路都放弃(Myers 超限 + LCS 越过格子上限)时,剥掉前后缀的中段整块算修改。
#[test]
fn 都放弃时中段整块退化() {
    let old: Vec<String> = (0..3500).map(|i| format!("old {i}")).collect();
    let new: Vec<String> = (0..3500).map(|i| format!("new {i}")).collect();
    let mut old: Vec<&str> = old.iter().map(String::as_str).collect();
    let mut new: Vec<&str> = new.iter().map(String::as_str).collect();
    // 首尾各一行公共前后缀:退化只覆盖中段
    old.insert(0, "head");
    new.insert(0, "head");
    old.push("tail");
    new.push("tail");
    let hunks = group_hunks(&line_ops(&old, &new, 0));
    assert_eq!(hunks, vec![hunk(Modified, 1, 3500, 1, 3500)]);
}

#[test]
fn 行对齐只认相等行() {
    // 旧:a b c d;新:a X c d e
    assert_eq!(
        line_alignment("a\nb\nc\nd\n", "a\nX\nc\nd\ne\n"),
        vec![Some(0), None, Some(2), Some(3), None]
    );
    // 在开头插一行:后面整体下移一行
    assert_eq!(
        line_alignment("a\nb\n", "new\na\nb\n"),
        vec![None, Some(0), Some(1)]
    );
    assert!(line_alignment("a\n", "").is_empty());
}

// ─── HEAD 取数(临时仓库) ──────────────────────────────────────

#[test]
fn head_取到已跟踪文件的原文() {
    let rel = "src/lib.rs";
    let root = init_repo_with_file("head-text", rel, "fn a() {}\r\n\tfn b() {}\r\n");
    // 工作区改了不影响 HEAD 版本
    std::fs::write(root.join(rel), "changed\n").unwrap();
    let head = head_file(&root, &root.join(rel)).unwrap();
    let HeadFile::Text { repo, text } = head else {
        panic!("应当取到文本: {head:?}");
    };
    // Tab 原样交给调用方归一。行尾不断言:入库时 libgit2 会按本机的
    // core.autocrlf 把 CRLF 折掉,开发机与 CI 的全局配置不一样
    assert_eq!(text.replace("\r\n", "\n"), "fn a() {}\n\tfn b() {}\n");
    assert_eq!(repo.rel_path, "src/lib.rs");
    assert!(repo.signature.head.is_some() && repo.signature.blob.is_some());
    assert_eq!(
        std::fs::canonicalize(&repo.root).unwrap(),
        std::fs::canonicalize(&root).unwrap()
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn head_未跟踪与不在仓库() {
    let root = init_repo_with_file("head-untracked", "a.txt", "a\n");
    std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
    let head = head_file(&root, &root.join("new.txt")).unwrap();
    let HeadFile::NotInHead(repo) = &head else {
        panic!("未跟踪文件应是 NotInHead: {head:?}");
    };
    assert_eq!(repo.rel_path, "new.txt");
    assert!(repo.signature.blob.is_none() && repo.signature.head.is_some());
    std::fs::remove_dir_all(&root).ok();

    // 普通目录:不在任何仓库里
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mini-term-git-head-norepo-{ts}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("x.txt"), "x\n").unwrap();
    assert_eq!(
        head_file(&dir, &dir.join("x.txt")).unwrap(),
        HeadFile::NotInRepo
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn head_二进制与空仓库() {
    let root = init_repo_with_file("head-binary", "a.bin", "a\0b\0c");
    let head = head_file(&root, &root.join("a.bin")).unwrap();
    assert!(matches!(head, HeadFile::Binary(_)), "{head:?}");
    std::fs::remove_dir_all(&root).ok();

    // 还没有任何提交的仓库:文件都算不在 HEAD 里,签名两项都空
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mini-term-git-head-unborn-{ts}"));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = crate::fs::strip_verbatim_prefix(std::fs::canonicalize(&dir).unwrap());
    Repository::init(&dir).unwrap();
    std::fs::write(dir.join("x.txt"), "x\n").unwrap();
    let head = head_file(&dir, &dir.join("x.txt")).unwrap();
    let HeadFile::NotInHead(repo) = &head else {
        panic!("空仓库里的文件应是 NotInHead: {head:?}");
    };
    assert_eq!(repo.signature, HeadSignature::default());
    std::fs::remove_dir_all(&dir).ok();
}

/// 提交改动了这个文件 → blob 变;提交别的文件 → head 变、blob 不变。
#[test]
fn head_签名随提交变化() {
    let root = init_repo_with_file("head-sig", "a.txt", "a\n");
    let before = head_signature(&root, "a.txt").unwrap();

    let commit_file = |rel: &str, content: &str, msg: &str| {
        let repo = Repository::open(&root).unwrap();
        std::fs::write(root.join(rel), content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(rel)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("mini-term", "test@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&parent])
            .unwrap();
    };

    commit_file("b.txt", "b\n", "other");
    let other = head_signature(&root, "a.txt").unwrap();
    assert_ne!(other.head, before.head);
    assert_eq!(other.blob, before.blob, "别的文件的提交不动这个 blob");

    commit_file("a.txt", "a2\n", "touch a");
    let touched = head_signature(&root, "a.txt").unwrap();
    assert_ne!(touched.blob, before.blob);

    // HEAD 里没有的路径:blob 为空
    assert!(head_signature(&root, "nope.txt").unwrap().blob.is_none());
    std::fs::remove_dir_all(&root).ok();
}
