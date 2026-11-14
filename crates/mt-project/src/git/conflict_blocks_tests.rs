//! 逐段解决冲突的测试:标记识别(两种冲突写法、CRLF、末尾没有换行、BOM、不成对、名字
//! 带空格 / 超长、`=======` 后带内容不算)、各种选择的替换文本、多段混选,以及在真 git
//! 冲突上走一遍「解析 → 选择 → 写回 → 标记已解决」、防陈旧、编码与各种不能逐段解决的情形。
//!
//! 识别与替换的样本照 Winc(Apache-2.0)`crates/winc-editor/src/git/merge/tests.rs` 的写法。

use std::path::{Path, PathBuf};
use std::time::Duration;

use git2::Repository;

use super::super::test_support::init_repo_with_file;
use super::super::{DiffSide, get_git_diff, run_git_output};
use super::*;

const TEXT: &str = "keep\n\
<<<<<<< HEAD\n\
ours 1\n\
ours 2\n\
=======\n\
theirs\n\
>>>>>>> feature/x\n\
middle\n\
<<<<<<< HEAD\n\
a\n\
||||||| base\n\
b\n\
=======\n\
c\n\
>>>>>>> feature/x\n";

fn slice<'a>(text: &'a str, range: &Range<usize>) -> &'a str {
    &text[range.clone()]
}

#[test]
fn 认出两种冲突写法() {
    let found = find_conflict_blocks(TEXT);
    assert_eq!(found.len(), 2);
    let first = &found[0];
    assert_eq!(
        (first.start_line, first.middle_line, first.end_line),
        (1, 4, 6)
    );
    assert_eq!(first.base_line, None);
    assert_eq!(first.base_label, None);
    assert_eq!(first.base, None);
    assert_eq!(slice(TEXT, &first.ours), "ours 1\nours 2\n");
    assert_eq!(slice(TEXT, &first.theirs), "theirs\n");
    assert_eq!((first.ours_lines(), first.theirs_lines()), (2, 1));
    assert_eq!(first.ours_label, "HEAD");
    assert_eq!(first.theirs_label, "feature/x");
    assert!(slice(TEXT, &first.span).starts_with("<<<<<<< HEAD\n"));
    assert!(slice(TEXT, &first.span).ends_with(">>>>>>> feature/x\n"));

    // diff3:`|||||||` 后面是共同祖先的版本,名字一并记下
    let second = &found[1];
    assert_eq!(second.base_line, Some(10));
    assert_eq!(second.base_label.as_deref(), Some("base"));
    assert_eq!(slice(TEXT, &second.ours), "a\n");
    assert_eq!(second.base.as_ref().map(|r| slice(TEXT, r)), Some("b\n"));
    assert_eq!(slice(TEXT, &second.theirs), "c\n");
    assert_eq!(second.middle_line, 12);
    assert_eq!(second.base_lines(), Some(1));
}

/// CRLF:各行换行符原样留在两侧内容里;名字不带 `\r`。
#[test]
fn crlf_文件() {
    let text = "top\r\n<<<<<<< a\r\nx\r\n=======\r\ny\r\n>>>>>>> b\r\nend\r\n";
    let found = find_conflict_blocks(text);
    assert_eq!(found.len(), 1);
    let block = &found[0];
    assert_eq!(
        (block.ours_label.as_str(), block.theirs_label.as_str()),
        ("a", "b")
    );
    assert_eq!(slice(text, &block.ours), "x\r\n");
    assert_eq!(slice(text, &block.theirs), "y\r\n");
    assert_eq!(
        block_resolution(text, block, ConflictPick::OursThenTheirs),
        "x\r\ny\r\n"
    );
    assert_eq!(
        apply_conflict_picks(text, &found, &[Some(ConflictPick::Theirs)]),
        "top\r\ny\r\nend\r\n"
    );
}

/// 文件末尾没有换行:冲突之后那一行没换行照样留着;`>>>>>>>` 自己是最后一行且没换行时
/// 照样认得出,解决之后文件末尾仍然没有换行。
#[test]
fn 文件末尾没有换行() {
    let tail = "<<<<<<< a\nx\n=======\ny\n>>>>>>> b\nlast";
    let found = find_conflict_blocks(tail);
    assert_eq!(found.len(), 1);
    assert_eq!(
        apply_conflict_picks(tail, &found, &[Some(ConflictPick::Ours)]),
        "x\nlast"
    );

    let at_end = "keep\n<<<<<<< a\nx\n=======\ny\n>>>>>>> b";
    let found = find_conflict_blocks(at_end);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].theirs_label, "b");
    assert_eq!(found[0].span.end, at_end.len());
    assert_eq!(
        apply_conflict_picks(at_end, &found, &[Some(ConflictPick::Theirs)]),
        "keep\ny"
    );
    assert_eq!(
        apply_conflict_picks(at_end, &found, &[Some(ConflictPick::OursThenTheirs)]),
        "keep\nx\ny"
    );
    // 没选的段原样保留,连「末尾没有换行」一起
    assert_eq!(apply_conflict_picks(at_end, &found, &[None]), at_end);

    // CRLF 文件同理,去掉的是整个 `\r\n`
    let crlf = "<<<<<<< a\r\nx\r\n=======\r\ny\r\n>>>>>>> b";
    let found = find_conflict_blocks(crlf);
    assert_eq!(
        apply_conflict_picks(crlf, &found, &[Some(ConflictPick::TheirsThenOurs)]),
        "y\r\nx"
    );
}

/// 不成对的不算:只有 `<<<<<<<`、没收尾、没有 `=======` 就碰到 `>>>>>>>`;八个 `<`、
/// `=======` 后面带内容的也不是标记。
#[test]
fn 不成对与伪标记不算冲突() {
    assert!(find_conflict_blocks("<<<<<<< a\nx\n=======\ny\n").is_empty());
    assert!(find_conflict_blocks("<<<<<<< a\nx\n>>>>>>> b\n").is_empty());
    assert!(find_conflict_blocks("<<<<<<<< eight\nx\n=======\ny\n>>>>>>> b\n").is_empty());
    assert!(find_conflict_blocks("<<<<<< six\nx\n=======\ny\n>>>>>>> b\n").is_empty());
    // `=======` 后面带内容:不是分隔线,这一段读不到 `=======` 就碰到 `>>>>>>>`,作废
    assert!(find_conflict_blocks("<<<<<<< a\nx\n======= y\nz\n>>>>>>> b\n").is_empty());
    assert!(find_conflict_blocks("<<<<<<< a\nx\n=======y\nz\n>>>>>>> b\n").is_empty());
    // 标记必须在行首
    assert!(find_conflict_blocks(" <<<<<<< a\nx\n=======\ny\n>>>>>>> b\n").is_empty());
    // `>>>>>>>` 后面紧跟名字(没有空格)的不算标记,后面那个才收尾
    let text = "<<<<<<<  HEAD \nx\n=======\ny\n>>>>>>>b\n>>>>>>>\n";
    let found = find_conflict_blocks(text);
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].ours_label.as_str(), found[0].end_line),
        ("HEAD", 5)
    );
    assert_eq!(found[0].theirs_label, "");
    assert_eq!(slice(text, &found[0].theirs), "y\n>>>>>>>b\n");
    // 没收尾又碰到新的 `<<<<<<<`:前面那段作废,从新的这一行开始
    let text = "<<<<<<< old\nx\n<<<<<<< new\na\n=======\nb\n>>>>>>> t\n";
    let found = find_conflict_blocks(text);
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].start_line, found[0].ours_label.as_str()),
        (2, "new")
    );
    // `=======` 之后再出现的 `|||||||` / `=======` 算对方的内容
    let text = "<<<<<<< a\nx\n=======\n||||||| q\n=======\n>>>>>>> b\n";
    let found = find_conflict_blocks(text);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].base_line, None);
    assert_eq!(slice(text, &found[0].theirs), "||||||| q\n=======\n");
    assert!(find_conflict_blocks("no markers at all\n").is_empty());
    assert!(find_conflict_blocks("").is_empty());
}

/// 名字:中间的空格保留、首尾空白去掉;超长的截到 [`LABEL_LIMIT`] 个字符。
#[test]
fn 名字带空格与超长() {
    let text = "<<<<<<< HEAD (my branch)\nx\n||||||| merged common ancestors\nb\n=======\ny\n>>>>>>> 1a2b3c4 (修复: 很长的标题)\n";
    let found = find_conflict_blocks(text);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].ours_label, "HEAD (my branch)");
    assert_eq!(
        found[0].base_label.as_deref(),
        Some("merged common ancestors")
    );
    assert_eq!(found[0].theirs_label, "1a2b3c4 (修复: 很长的标题)");

    let long = "名".repeat(LABEL_LIMIT + 50);
    let text = format!("<<<<<<< {long}\nx\n=======\ny\n>>>>>>> b\n");
    let found = find_conflict_blocks(&text);
    assert_eq!(found[0].ours_label.chars().count(), LABEL_LIMIT);
}

/// 文件第一行开头的 BOM 不挡标记;BOM 留在段外,写回时还在。
#[test]
fn 开头的_bom_留在段外() {
    let text = "\u{feff}<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n";
    let found = find_conflict_blocks(text);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].span.start, '\u{feff}'.len_utf8());
    assert_eq!(
        apply_conflict_picks(text, &found, &[Some(ConflictPick::Theirs)]),
        "\u{feff}y\n"
    );
}

#[test]
fn 各种选择的替换文本() {
    let found = find_conflict_blocks(TEXT);
    let first = &found[0];
    assert_eq!(
        block_resolution(TEXT, first, ConflictPick::Ours),
        "ours 1\nours 2\n"
    );
    assert_eq!(
        block_resolution(TEXT, first, ConflictPick::Theirs),
        "theirs\n"
    );
    assert_eq!(
        block_resolution(TEXT, first, ConflictPick::OursThenTheirs),
        "ours 1\nours 2\ntheirs\n"
    );
    assert_eq!(
        block_resolution(TEXT, first, ConflictPick::TheirsThenOurs),
        "theirs\nours 1\nours 2\n"
    );
    // diff3 段:base 不进任何一种结果
    let second = &found[1];
    assert_eq!(
        block_resolution(TEXT, second, ConflictPick::OursThenTheirs),
        "a\nc\n"
    );
    // 一侧是空的(那一侧把这几行删了):两者都要就是另一侧
    let text = "<<<<<<< a\n=======\ny\n>>>>>>> b\n";
    let found = find_conflict_blocks(text);
    assert_eq!(block_resolution(text, &found[0], ConflictPick::Ours), "");
    assert_eq!(
        block_resolution(text, &found[0], ConflictPick::TheirsThenOurs),
        "y\n"
    );
}

/// 多段混选:选了的换掉,没选的连同标记原样留着,段外一个字节不动。
#[test]
fn 多段混选() {
    let found = find_conflict_blocks(TEXT);
    assert_eq!(
        apply_conflict_picks(
            TEXT,
            &found,
            &[Some(ConflictPick::Theirs), Some(ConflictPick::Ours)]
        ),
        "keep\ntheirs\nmiddle\na\n"
    );
    let partial = apply_conflict_picks(TEXT, &found, &[None, Some(ConflictPick::TheirsThenOurs)]);
    assert_eq!(
        partial,
        "keep\n<<<<<<< HEAD\nours 1\nours 2\n=======\ntheirs\n>>>>>>> feature/x\nmiddle\nc\na\n"
    );
    // 留下的那段照样认得出
    assert_eq!(find_conflict_blocks(&partial).len(), 1);
    assert_eq!(apply_conflict_picks(TEXT, &found, &[None, None]), TEXT);
    // picks 比段数短:缺的按没选
    assert_eq!(apply_conflict_picks(TEXT, &found, &[]), TEXT);
}

// ─── 真仓库 ───────────────────────────────────────────────────

/// 脚手架仓库 + 关掉 autocrlf(CLI 检出的换行不随本机全局配置变,同 `conflict.rs` 的测试)。
fn repo(tag: &str, rel: &str, content: &str) -> PathBuf {
    let root = init_repo_with_file(tag, rel, content);
    Repository::open(&root)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("core.autocrlf", false)
        .unwrap();
    root
}

fn cli(root: &Path, args: &[&str]) -> bool {
    run_git_output(root, args, "test", Duration::from_secs(30), "")
        .unwrap()
        .success
}

const BASE: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";

/// 造一个冲突:`a.txt` 两处都被两边改了(两段冲突),`gone.txt` 一边删一边改(删除 /
/// 修改冲突)。`style` 是 `merge.conflictStyle`(钉死,不吃本机的全局配置)。
fn conflicted_repo(tag: &str, style: &str) -> PathBuf {
    let root = repo(tag, "a.txt", BASE);
    std::fs::write(root.join("gone.txt"), "keep me\n").unwrap();
    assert!(cli(&root, &["add", "gone.txt"]));
    assert!(cli(&root, &["commit", "-m", "base", "--no-verify"]));
    assert!(cli(&root, &["checkout", "-b", "other"]));
    std::fs::write(
        root.join("a.txt"),
        "1\nTHEIRS-2\n3\n4\n5\n6\n7\nTHEIRS-8\n9\n",
    )
    .unwrap();
    std::fs::write(root.join("gone.txt"), "changed by them\n").unwrap();
    assert!(cli(&root, &["commit", "-am", "theirs", "--no-verify"]));
    assert!(cli(&root, &["checkout", "-"]));
    std::fs::write(root.join("a.txt"), "1\nOURS-2\n3\n4\n5\n6\n7\nOURS-8\n9\n").unwrap();
    assert!(cli(&root, &["rm", "-q", "gone.txt"]));
    assert!(cli(&root, &["commit", "-am", "ours", "--no-verify"]));
    let style = format!("merge.conflictStyle={style}");
    assert!(
        !cli(&root, &["-c", &style, "merge", "other", "--no-edit"]),
        "应当冲突"
    );
    root
}

fn info<'a>(list: &'a [ConflictInfo], path: &str) -> &'a ConflictInfo {
    list.iter().find(|c| c.path == path).unwrap()
}

/// 真 git 造的冲突走一遍:解析 → 选择 → 写回 → 标记已解决 → 冲突列表里没有它。
#[test]
fn 真冲突逐段解决并标记已解决() {
    let root = conflicted_repo("segments-flow", "merge");
    let list = list_conflicts(&root).unwrap();
    assert_eq!(
        conflict_segment_status(&root, info(&list, "a.txt")),
        SegmentStatus::Ready { blocks: 2 }
    );
    let file = load_conflict_file(&root, "a.txt").unwrap();
    assert_eq!(file.encoding, TextEncoding::Utf8);
    assert_eq!(file.blocks.len(), 2);
    let first = &file.blocks[0];
    assert_eq!(first.ours_label, "HEAD");
    assert_eq!(first.theirs_label, "other");
    assert_eq!(file.ours_text(first), "OURS-2\n");
    assert_eq!(file.theirs_text(first), "THEIRS-2\n");
    assert_eq!(file.base_text(first), None);

    // 只选了一段时不许标记已解决
    let partial = [Some(ConflictPick::Theirs), None];
    let err = write_conflict_resolution(&root, &file, &partial, true).unwrap_err();
    assert!(format!("{err:#}").contains("没有选择"), "{err:#}");
    let picks = [
        Some(ConflictPick::Theirs),
        Some(ConflictPick::OursThenTheirs),
    ];
    write_conflict_resolution(&root, &file, &picks, true).unwrap();
    let expected = "1\nTHEIRS-2\n3\n4\n5\n6\n7\nOURS-8\nTHEIRS-8\n9\n";
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        expected
    );
    let list = list_conflicts(&root).unwrap();
    assert!(list.iter().all(|c| c.path != "a.txt"), "{list:?}");
    let staged = get_git_diff(&root, "a.txt", DiffSide::Staged).unwrap();
    assert_eq!(staged.new_content, expected);
    // 已经解决了:再写一次明确报「不在冲突中」
    let err = write_conflict_resolution(&root, &file, &picks, false).unwrap_err();
    assert_eq!(
        err.downcast_ref::<SegmentBlocker>(),
        Some(&SegmentBlocker::NotConflicted)
    );
    // 剩下那个删除 / 修改冲突整文件处理
    super::super::resolve_conflict_with(&root, "gone.txt", super::super::ConflictSide::Theirs)
        .unwrap();
    assert!(list_conflicts(&root).unwrap().is_empty());
    std::fs::remove_dir_all(&root).ok();
}

/// 仅写入一部分:没选的那段原样留着标记,文件仍在冲突中;再解析时只剩那一段。
#[test]
fn 仅写入一部分() {
    let root = conflicted_repo("segments-partial", "merge");
    let file = load_conflict_file(&root, "a.txt").unwrap();
    write_conflict_resolution(&root, &file, &[None, Some(ConflictPick::Ours)], false).unwrap();
    let text = std::fs::read_to_string(root.join("a.txt")).unwrap();
    assert!(text.contains("<<<<<<< HEAD\nOURS-2\n=======\nTHEIRS-2\n>>>>>>> other\n"));
    assert!(text.ends_with("7\nOURS-8\n9\n"), "{text}");
    assert!(
        list_conflicts(&root)
            .unwrap()
            .iter()
            .any(|c| c.path == "a.txt")
    );
    let again = load_conflict_file(&root, "a.txt").unwrap();
    assert_eq!(again.blocks.len(), 1);
    // 一段都没选:不写
    let err = write_conflict_resolution(&root, &again, &[None], false).unwrap_err();
    assert!(format!("{err:#}").contains("没有选择任何"), "{err:#}");
    // 选择的个数与段数对不上:不写
    assert!(write_conflict_resolution(&root, &again, &[], false).is_err());
    std::fs::remove_dir_all(&root).ok();
}

/// diff3 写法:三段都读得出来,base 不进结果。
#[test]
fn 真冲突_diff3() {
    let root = conflicted_repo("segments-diff3", "diff3");
    let file = load_conflict_file(&root, "a.txt").unwrap();
    assert_eq!(file.blocks.len(), 2);
    let first = &file.blocks[0];
    assert_eq!(file.base_text(first), Some("2\n"));
    assert!(first.base_label.is_some());
    write_conflict_resolution(
        &root,
        &file,
        &[Some(ConflictPick::Ours), Some(ConflictPick::Ours)],
        true,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "1\nOURS-2\n3\n4\n5\n6\n7\nOURS-8\n9\n"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// 防陈旧:解析之后文件被改过就拒绝,错误认得出是 [`StaleConflictFile`],文件不动、
/// 仍在冲突中。等长的改动也拦得住(逐字节比,不只比长度)。
#[test]
fn 文件变了就拒绝写回() {
    let root = conflicted_repo("segments-stale", "merge");
    let file = load_conflict_file(&root, "a.txt").unwrap();
    let picks = [Some(ConflictPick::Ours), Some(ConflictPick::Ours)];

    let edited = file.text.replace("OURS-2", "MINE-2");
    assert_eq!(edited.len(), file.text.len());
    std::fs::write(root.join("a.txt"), &edited).unwrap();
    let err = write_conflict_resolution(&root, &file, &picks, true).unwrap_err();
    assert!(err.is::<StaleConflictFile>(), "{err:#}");
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), edited);
    assert!(
        list_conflicts(&root)
            .unwrap()
            .iter()
            .any(|c| c.path == "a.txt")
    );

    std::fs::write(root.join("a.txt"), format!("{edited}more\n")).unwrap();
    let err = write_conflict_resolution(&root, &file, &picks, false).unwrap_err();
    assert!(err.is::<StaleConflictFile>(), "{err:#}");

    std::fs::remove_file(root.join("a.txt")).unwrap();
    let err = write_conflict_resolution(&root, &file, &picks, false).unwrap_err();
    assert!(err.is::<StaleConflictFile>(), "{err:#}");
    std::fs::remove_dir_all(&root).ok();
}

/// 不能逐段解决的几种:删除 / 修改冲突、没有成对标记、二进制、无损解不了、不在冲突中。
#[test]
fn 不能逐段解决的情形() {
    let root = conflicted_repo("segments-blocked", "merge");
    let list = list_conflicts(&root).unwrap();
    let blocked = |path: &str| {
        let status = conflict_segment_status(&root, info(&list, path));
        let loaded = load_conflict_file(&root, path).map(|_| ()).unwrap_err();
        (status, loaded.downcast_ref::<SegmentBlocker>().copied())
    };
    assert_eq!(
        blocked("gone.txt"),
        (
            SegmentStatus::Blocked(SegmentBlocker::DeleteModify),
            Some(SegmentBlocker::DeleteModify)
        )
    );

    let a = root.join("a.txt");
    std::fs::write(&a, "merged by hand\n<<<<<<< only an opener\n").unwrap();
    assert_eq!(
        blocked("a.txt"),
        (
            SegmentStatus::Blocked(SegmentBlocker::NoMarkers),
            Some(SegmentBlocker::NoMarkers)
        )
    );
    std::fs::write(
        &a,
        b"\0\x01\x02<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n\xff\xfe",
    )
    .unwrap();
    assert_eq!(
        blocked("a.txt"),
        (
            SegmentStatus::Blocked(SegmentBlocker::Binary),
            Some(SegmentBlocker::Binary)
        )
    );
    // 大体是 UTF-8、只坏了一处:按替换字符只读,写回会把那个字节写坏
    let mut bytes = "<<<<<<< a\n中文中文中文中文中文\n=======\ny\n>>>>>>> b\n"
        .as_bytes()
        .to_vec();
    bytes.push(0xff);
    std::fs::write(&a, bytes).unwrap();
    assert_eq!(
        blocked("a.txt"),
        (
            SegmentStatus::Blocked(SegmentBlocker::Lossy),
            Some(SegmentBlocker::Lossy)
        )
    );
    std::fs::remove_file(&a).unwrap();
    assert_eq!(
        blocked("a.txt"),
        (
            SegmentStatus::Blocked(SegmentBlocker::Unreadable),
            Some(SegmentBlocker::Unreadable)
        )
    );

    // 不在冲突中的文件
    let err = load_conflict_file(&root, "nope.txt").unwrap_err();
    assert_eq!(
        err.downcast_ref::<SegmentBlocker>(),
        Some(&SegmentBlocker::NotConflicted)
    );
    std::fs::remove_dir_all(&root).ok();
}

/// 非 UTF-8 的冲突文件按识别出的编码解码、按同一编码写回:GBK(记作 GB18030)与带 BOM
/// 的 UTF-16 LE。段外没碰过的字节逐字节不变。
#[test]
fn 按原编码写回() {
    let root = conflicted_repo("segments-encoding", "merge");
    let gbk = TextEncoding::Legacy(crate::encoding::encoding_rs::GBK);
    let sample = "// 你好，世界。这是一段用来猜编码的中文注释，里面有足够多的汉字。\n\
                  <<<<<<< HEAD\n我方的中文\n=======\n对方的中文\n>>>>>>> other\n结尾\n";
    let a = root.join("a.txt");
    std::fs::write(&a, gbk.encode(sample).unwrap()).unwrap();
    let file = load_conflict_file(&root, "a.txt").unwrap();
    assert!(
        matches!(file.encoding, TextEncoding::Legacy(_)),
        "{:?}",
        file.encoding
    );
    assert_eq!(file.ours_text(&file.blocks[0]), "我方的中文\n");
    write_conflict_resolution(&root, &file, &[Some(ConflictPick::TheirsThenOurs)], false).unwrap();
    let expected = "// 你好，世界。这是一段用来猜编码的中文注释，里面有足够多的汉字。\n\
                    对方的中文\n我方的中文\n结尾\n";
    assert_eq!(std::fs::read(&a).unwrap(), gbk.encode(expected).unwrap());

    let utf16 = TextEncoding::Utf16Le;
    let sample = "头\r\n<<<<<<< HEAD\r\n甲\r\n=======\r\n乙\r\n>>>>>>> other\r\n尾\r\n";
    std::fs::write(&a, utf16.encode(sample).unwrap()).unwrap();
    let file = load_conflict_file(&root, "a.txt").unwrap();
    assert_eq!(file.encoding, utf16);
    write_conflict_resolution(&root, &file, &[Some(ConflictPick::Theirs)], true).unwrap();
    assert_eq!(
        std::fs::read(&a).unwrap(),
        utf16.encode("头\r\n乙\r\n尾\r\n").unwrap(),
        "带回 BOM、CRLF 原样"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn compact_snapshot_rebuilds_fields_and_rejects_forgery() {
    for encoding in [
        TextEncoding::Utf8,
        TextEncoding::Utf16Le,
        TextEncoding::Utf16Be,
        TextEncoding::Legacy(crate::encoding::encoding_rs::GB18030),
    ] {
        let file = parsed_conflict("a.txt".into(), encoding.encode(TEXT).unwrap()).unwrap();
        let bytes = serde_json::to_vec(&file).unwrap();
        let rebuilt: ConflictFile = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(rebuilt.text, file.text);
        assert_eq!(rebuilt.encoding, file.encoding);
        assert_eq!(rebuilt.blocks, file.blocks);
        assert_eq!(rebuilt.snapshot().unwrap(), file.snapshot().unwrap());
        let mut forged = serde_json::to_value(&file).unwrap();
        forged["blocks"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ConflictFile>(forged).is_err());
    }
    let mut original = TEXT.as_bytes().to_vec();
    original.resize(MAX_SEGMENT_FILE_BYTES as usize, b'\\');
    let file = parsed_conflict("large".into(), original).unwrap();
    let bytes = serde_json::to_vec(&file).unwrap();
    assert!(bytes.len() < 16 * 1024 * 1024);
    let rebuilt: ConflictFile = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(rebuilt.original.len(), MAX_SEGMENT_FILE_BYTES as usize);
    let oversized = serde_json::json!({"path":"a", "encoding":"UTF-8", "original":base64::engine::general_purpose::STANDARD.encode(vec![b'a'; MAX_SEGMENT_FILE_BYTES as usize + 1])});
    assert!(serde_json::from_value::<ConflictFile>(oversized).is_err());
    for invalid in ["=", "====", "a===", "!!!!"] {
        let wire = serde_json::json!({"path":"a", "encoding":"UTF-8", "original":invalid});
        assert!(serde_json::from_value::<ConflictFile>(wire).is_err());
    }
}

#[test]
fn verified_bytes_not_mutable_display_fields_control_resolution() {
    let root = conflicted_repo("segments-forged-display", "merge");
    let sample = "头\r\n<<<<<<< HEAD\r\n甲\r\n=======\r\n乙\r\n>>>>>>> other\r\n尾";
    let encoding = TextEncoding::Utf16Le;
    std::fs::write(root.join("a.txt"), encoding.encode(sample).unwrap()).unwrap();
    let mut file = load_conflict_file(&root, "a.txt").unwrap();
    file.encoding = TextEncoding::Utf8;
    file.text = "forged".into();
    file.blocks.clear();
    write_conflict_resolution(&root, &file, &[Some(ConflictPick::Theirs)], true).unwrap();
    assert_eq!(
        std::fs::read(root.join("a.txt")).unwrap(),
        encoding.encode("头\r\n乙\r\n尾").unwrap()
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn compact_snapshot_keeps_sender_codec_instead_of_region_guessing() {
    // A4 A4 is valid and roundtrippable in both codecs, but denotes different
    // characters. The receiver must retain the sender's chosen interpretation.
    let mut original = b"<<<<<<< ours\n".to_vec();
    original.extend_from_slice(&[0xa4, 0xa4]);
    original.extend_from_slice(b"\n=======\nx\n>>>>>>> theirs\n");
    let mut texts = Vec::new();
    for codec in [
        crate::encoding::encoding_rs::GB18030,
        crate::encoding::encoding_rs::BIG5,
    ] {
        let encoding = TextEncoding::Legacy(codec);
        let file = parsed_conflict_with_encoding("a".into(), original.clone(), encoding).unwrap();
        let rebuilt: ConflictFile =
            serde_json::from_slice(&serde_json::to_vec(&file).unwrap()).unwrap();
        assert_eq!(rebuilt.encoding, encoding);
        assert_eq!(rebuilt.text, file.text);
        assert_eq!(rebuilt.blocks, file.blocks);
        texts.push(rebuilt.text);
        let mut forged = serde_json::to_value(&file).unwrap();
        for unsafe_encoding in ["UTF-8", "UTF-16 LE", "lossy", "replacement", "UTF-7"] {
            forged["encoding"] = unsafe_encoding.into();
            assert!(serde_json::from_value::<ConflictFile>(forged.clone()).is_err());
        }
    }
    assert_ne!(texts[0], texts[1]);
}
