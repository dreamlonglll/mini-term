use std::path::Path;
use std::time::Duration;

use git2::Repository;

use super::super::run_git_command_with_input;
use super::super::test_support::init_repo_with_file;
use super::*;

const H1: &str = "1111111111111111111111111111111111111111";
const H2: &str = "2222222222222222222222222222222222222222";
const ZERO: &str = "0000000000000000000000000000000000000000";

/// 照着真实输出的形状手写一份:首次出现给详情、同组后续行只有头行、未提交的行是
/// 全零哈希,详情里夹着 committer / previous / boundary / filename 这些不关心的键。
fn sample() -> String {
    [
        &format!("{H1} 1 1 2"),
        "author Alice",
        "author-mail <alice@example.com>",
        "author-time 1700000000",
        "author-tz +0800",
        "committer Alice",
        "committer-mail <alice@example.com>",
        "committer-time 1700000000",
        "committer-tz +0800",
        "summary first commit: with spaces",
        "boundary",
        "filename a.txt",
        "\tline one",
        &format!("{H1} 2 2"),
        "\tline two",
        &format!("{ZERO} 3 3 1"),
        "author Not Committed Yet",
        "author-mail <not.committed.yet>",
        "author-time 1700000500",
        "author-tz +0800",
        "summary Version of a.txt from a.txt",
        &format!("previous {H1} a.txt"),
        "filename a.txt",
        "\tline three",
        &format!("{H2} 3 4 1"),
        "author 张三",
        "author-mail <zs@example.com>",
        "author-time 1700000100",
        "author-tz +0000",
        "summary 改了第四行",
        &format!("previous {H1} a.txt"),
        "filename a.txt",
        "\t\tindented line four",
        // 已见过的提交再次出现:没有详情,偶尔带一行 filename
        &format!("{H1} 4 5 1"),
        "filename a.txt",
        "\tline five",
        "",
    ]
    .join("\n")
}

#[test]
fn 解析_porcelain_首次详情与重复头行() {
    let result = parse_blame_porcelain(&sample()).unwrap();
    assert_eq!(result.lines, vec![0, 0, 1, 2, 0]);
    assert_eq!(result.commits.len(), 3);

    let alice = &result.commits[0];
    assert_eq!(alice.hash, H1);
    assert_eq!(alice.author, "Alice");
    assert_eq!(alice.author_mail, "alice@example.com");
    assert_eq!(alice.author_time, 1_700_000_000);
    assert_eq!(alice.author_tz, "+0800");
    assert_eq!(alice.summary, "first commit: with spaces");
    assert_eq!(alice.short_hash(), "1111111");
    assert!(!alice.is_uncommitted());

    let pending = &result.commits[1];
    assert!(pending.is_uncommitted());
    assert_eq!(pending.author_mail, "not.committed.yet");

    let zs = result.commit_of(3).unwrap();
    assert_eq!(zs.author, "张三");
    assert_eq!(zs.summary, "改了第四行");
    assert!(result.commit_of(5).is_none(), "越界行没有提交");
}

#[test]
fn 解析_porcelain_容忍_crlf_与空输出() {
    let crlf = sample().replace('\n', "\r\n");
    let result = parse_blame_porcelain(&crlf).unwrap();
    assert_eq!(result.lines, vec![0, 0, 1, 2, 0]);
    assert_eq!(result.commits[0].summary, "first commit: with spaces");

    let empty = parse_blame_porcelain("").unwrap();
    assert!(empty.lines.is_empty() && empty.commits.is_empty());
}

#[test]
fn 解析_porcelain_拒绝残缺输出() {
    // 内容行前没有头行
    assert!(parse_blame_porcelain("\torphan\n").is_err());
    // 第 2 行缺失:有行没有归属
    let gap = format!("{H1} 1 1 1\nauthor A\n\tone\n{H1} 3 3 1\n\tthree\n");
    assert!(parse_blame_porcelain(&gap).is_err());
}

#[test]
fn 头行识别() {
    assert_eq!(parse_header(&format!("{H1} 3 7 2")), Some((H1, 6)));
    assert_eq!(parse_header(&format!("{H1} 3 7")), Some((H1, 6)));
    // SHA-256 仓库的 64 位哈希
    let long = "a".repeat(64);
    assert_eq!(
        parse_header(&format!("{long} 1 1")),
        Some((long.as_str(), 0))
    );
    // 详情行、行号非数字、行号 0、多余字段都不是头行
    assert_eq!(
        parse_header("author 1111111111111111111111111111111111111111 1 2"),
        None
    );
    assert_eq!(parse_header(&format!("{H1} x 1")), None);
    assert_eq!(parse_header(&format!("{H1} 1 0")), None);
    assert_eq!(parse_header(&format!("{H1} 1 2 3 4")), None);
    assert_eq!(parse_header("summary 1 2"), None);
}

// ─── 真仓库 ────────────────────────────────────────────────────

/// 第二次提交换一个作者改掉第 2 行,再把带一行未保存插入的缓冲区喂进去:
/// 每一行都要落在对的提交上,插入的那行是未提交。
#[test]
fn blame_脏缓冲区按当前内容对齐() {
    let root = init_repo_with_file("blame", "a.txt", "one\ntwo\nthree\n");
    {
        let repo = Repository::open(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let author = git2::Signature::new(
            "张三",
            "zs@example.com",
            &git2::Time::new(1_700_000_000, 480),
        )
        .unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(
            Some("HEAD"),
            &author,
            &author,
            "second\n\nbody",
            &tree,
            &[&parent],
        )
        .unwrap();
    }

    let result = blame_file(&root, "a.txt", "one\nTWO\ninserted\nthree\n").unwrap();
    assert_eq!(result.lines.len(), 4);
    assert_eq!(result.commit_of(0).unwrap().summary, "init");
    let second = result.commit_of(1).unwrap();
    assert_eq!(second.author, "张三");
    assert_eq!(second.author_mail, "zs@example.com");
    assert_eq!(second.author_time, 1_700_000_000);
    assert_eq!(second.author_tz, "+0800");
    assert_eq!(second.summary, "second", "summary 只取标题行");
    assert!(result.commit_of(2).unwrap().is_uncommitted());
    assert_eq!(result.commit_of(3).unwrap().summary, "init");
    // 同一个提交只记一份
    assert_eq!(result.lines[0], result.lines[3]);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn blame_未跟踪文件与非仓库目录报错() {
    let root = init_repo_with_file("blame-untracked", "a.txt", "a\n");
    std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
    assert!(blame_file(&root, "new.txt", "fresh\n").is_err());
    std::fs::remove_dir_all(&root).ok();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mini-term-git-blame-norepo-{ts}"));
    std::fs::create_dir_all(&dir).unwrap();
    let err = blame_file(&dir, "x.txt", "x\n").unwrap_err().to_string();
    assert!(err.contains("不是 git 仓库"), "实际错误: {err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// stdin 真的喂进去了,而且大于管道缓冲的内容不会把读写两头卡死。
#[test]
fn 带_stdin_的执行器整段喂入() {
    let root = init_repo_with_file("stdin", "a.txt", "a\n");
    for size in [5usize, 2 * 1024 * 1024] {
        let input: Vec<u8> = (0..size).map(|i| b'a' + (i % 26) as u8).collect();
        let out = run_git_command_with_input(
            &root,
            &["hash-object", "--stdin"],
            "hash-object",
            input.clone(),
            Duration::from_secs(30),
            "",
        )
        .unwrap();
        let expected = git2::Oid::hash_object(git2::ObjectType::Blob, &input).unwrap();
        assert_eq!(out.trim(), expected.to_string(), "{size} 字节");
    }
    std::fs::remove_dir_all(&root).ok();
}
