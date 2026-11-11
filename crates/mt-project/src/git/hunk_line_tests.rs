//! 按行操作([`LinePick`])与多块一趟拼([`splice_hunks`])的单测,外加整块口径的逐字节
//! 回归:改造前的 `splice_hunk` 原样留了一份当对照([`splice_hunk_v0`]),随机文本上整块
//! 拼接(不传选择 / 增删行全选)必须与它一字不差。

use std::path::{Path, PathBuf};

use git2::Repository;

use super::super::test_support::init_repo_with_file;
use super::super::{DiffLine, DiffSide, diff_two_texts, get_git_diff, git_commit_with, git_stage};
use super::*;

/// 改造前的 `splice_hunk`,**逐字照抄**(只改了函数名与 `DiffLine` 的路径),当整块口径的
/// 对照。别改它 —— 回归测试比的就是「与原来逐字节一致」。
#[rustfmt::skip]
fn splice_hunk_v0(
    old: &str,
    new: &str,
    hunk: &DiffHunk,
    dir: HunkDirection,
    fallback_eol: &str,
) -> Result<String> {
    let old_lines = split_raw(old);
    let new_lines = split_raw(new);
    // target = 被写的那一侧,source = 搬行过来的那一侧
    let (target, source) = match dir {
        HunkDirection::Apply => (&old_lines, &new_lines),
        HunkDirection::Revert => (&new_lines, &old_lines),
    };
    // 「另一侧有、这一侧没有」的行要搬进来;「这一侧有、另一侧没有」的行要拿掉
    let (insert_kind, drop_kind) = match dir {
        HunkDirection::Apply => ("add", "delete"),
        HunkDirection::Revert => ("delete", "add"),
    };
    let target_no = |line: &DiffLine| match dir {
        HunkDirection::Apply => line.old_lineno,
        HunkDirection::Revert => line.new_lineno,
    };
    let source_no = |line: &DiffLine| match dir {
        HunkDirection::Apply => line.new_lineno,
        HunkDirection::Revert => line.old_lineno,
    };

    let mut pieces: Vec<Piece> = Vec::with_capacity(hunk.lines.len());
    let (mut target_first, mut target_next) = (None, None);
    let (mut source_first, mut source_next) = (None, None);
    for line in &hunk.lines {
        let kind = line.kind.as_str();
        if kind == "context" {
            let t = line_index(target_no(line), target.len())?;
            let s = line_index(source_no(line), source.len())?;
            if target[t].text != line.content || source[s].text != line.content {
                return Err(StaleDiff.into());
            }
            advance(&mut target_next, &mut target_first, t)?;
            advance(&mut source_next, &mut source_first, s)?;
            pieces.push(Piece::Keep(t));
        } else if kind == drop_kind {
            let t = line_index(target_no(line), target.len())?;
            if target[t].text != line.content {
                return Err(StaleDiff.into());
            }
            advance(&mut target_next, &mut target_first, t)?;
        } else if kind == insert_kind {
            let s = line_index(source_no(line), source.len())?;
            if source[s].text != line.content {
                return Err(StaleDiff.into());
            }
            advance(&mut source_next, &mut source_first, s)?;
            pieces.push(Piece::Insert(s));
        } else {
            return Err(StaleDiff.into());
        }
    }

    // 块在被写那一侧占的区间。一行都没占(纯插入)时只能看 hunk 头 ——
    // 头里的起点是「插在第几行之前」(1 起),见 `build_hunks`
    let header_start = |start: u32, len: usize| (start.max(1) as usize - 1).min(len);
    let (target_header, source_header) = match dir {
        HunkDirection::Apply => (hunk.old_start, hunk.new_start),
        HunkDirection::Revert => (hunk.new_start, hunk.old_start),
    };
    let target_begin = target_first.unwrap_or_else(|| header_start(target_header, target.len()));
    let target_end = target_next.unwrap_or(target_begin);
    let source_begin = source_first.unwrap_or_else(|| header_start(source_header, source.len()));
    let source_end = source_next.unwrap_or(source_begin);
    // 块碰到了两侧的文件末尾:末行有没有换行要跟随来源侧(见模块注释)
    let at_eof = target_end == target.len() && source_end == source.len();

    let eol = dominant_eol(target).unwrap_or(fallback_eol);
    let mut out: Vec<(&str, &str)> = Vec::with_capacity(target.len() + pieces.len());
    out.extend(target[..target_begin].iter().map(|l| (l.text, l.eol)));
    let hunk_from = out.len();
    for piece in &pieces {
        out.push(match *piece {
            Piece::Keep(t) => (target[t].text, target[t].eol),
            Piece::Insert(s) => (source[s].text, eol),
        });
    }
    let hunk_to = out.len();
    out.extend(target[target_end..].iter().map(|l| (l.text, l.eol)));

    // 原先的末行(没有换行)被挤到了中间:补上换行,否则两行会粘成一行。
    // 只可能发生在块的范围里 —— 块没碰到末尾时原末行仍是末行
    let last = out.len().saturating_sub(1);
    for (i, line) in out.iter_mut().enumerate() {
        if i != last && line.1.is_empty() {
            line.1 = eol;
        }
    }
    // 末行换行跟随来源侧 —— 只在结果的末行正是块里的行时才动(块外一个字节都不碰)
    if at_eof && hunk_to > hunk_from && hunk_to == out.len() {
        let source_has_final_eol = source.last().is_none_or(|l| !l.eol.is_empty());
        let tail = &mut out[last];
        if !source_has_final_eol {
            tail.1 = "";
        } else if tail.1.is_empty() {
            tail.1 = eol;
        }
    }

    let mut text = String::with_capacity(old.len().max(new.len()) + 16);
    for (body, line_eol) in out {
        text.push_str(body);
        text.push_str(line_eol);
    }
    Ok(text)
}

// ── 随机语料 ────────────────────────────────────────────────

/// 测试用的小随机数(xorshift64*):语料要可复现,不引依赖。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// 换行风格。
#[derive(Clone, Copy)]
enum Eol {
    Lf,
    Crlf,
    /// 每行随机
    Mixed,
}

/// 一组行拼成文本:`final_eol = false` 时末行不带换行。
fn render(rng: &mut Rng, lines: &[String], eol: Eol, final_eol: bool) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        out.push_str(line);
        if i + 1 == lines.len() && !final_eol {
            break;
        }
        out.push_str(match eol {
            Eol::Lf => "\n",
            Eol::Crlf => "\r\n",
            Eol::Mixed if rng.chance(50) => "\n",
            Eol::Mixed => "\r\n",
        });
    }
    out
}

/// 随机的一对行序列:旧侧 0~30 行(取自小字母表,好让 diff 里出现重复行与空行),新侧在它
/// 上面随机增 / 删 / 改几处。
fn random_lines(rng: &mut Rng) -> (Vec<String>, Vec<String>) {
    const WORDS: [&str; 8] = ["a", "b", "c", "fn x() {", "}", "", "let y = 1;", "中文"];
    let base: Vec<String> = (0..rng.below(31))
        .map(|_| WORDS[rng.below(WORDS.len())].to_string())
        .collect();
    let mut edited = base.clone();
    for _ in 0..rng.below(5) {
        let at = rng.below(edited.len() + 1);
        match rng.below(3) {
            0 => {
                for _ in 0..=rng.below(3) {
                    edited.insert(at, format!("new{}", rng.below(4)));
                }
            }
            1 if at < edited.len() => {
                let len = (1 + rng.below(3)).min(edited.len() - at);
                edited.drain(at..at + len);
            }
            _ if at < edited.len() => edited[at] = format!("mod{}", rng.below(4)),
            _ => {}
        }
    }
    (base, edited)
}

/// 随机文本对:两侧的换行风格(LF / CRLF / 混合)与有无末尾换行各自随机。
fn random_pair(rng: &mut Rng) -> (String, String) {
    let (base, edited) = random_lines(rng);
    let style = |rng: &mut Rng| match rng.below(3) {
        0 => Eol::Lf,
        1 => Eol::Crlf,
        _ => Eol::Mixed,
    };
    let (old_eol, new_eol) = (style(rng), style(rng));
    let (old_final, new_final) = (rng.chance(70), rng.chance(70));
    let old = render(rng, &base, old_eol, old_final);
    let new = render(rng, &edited, new_eol, new_final);
    (old, new)
}

/// 块里全部增删行的下标。
fn change_lines(hunk: &DiffHunk) -> LinePick {
    (0..hunk.lines.len())
        .filter(|&i| hunk.lines[i].kind != "context")
        .collect()
}

/// 块里随机挑的一部分增删行(至少一行)。
fn random_pick(rng: &mut Rng, hunk: &DiffHunk) -> LinePick {
    let all: Vec<usize> = change_lines(hunk).into_iter().collect();
    let mut pick: LinePick = all.iter().copied().filter(|_| rng.chance(50)).collect();
    if pick.is_empty() {
        pick.insert(all[rng.below(all.len())]);
    }
    pick
}

/// 按行操作的预期结果(只比行内容):块外照抄被写那一侧,块里按 hunk 的行序 —— 上下文
/// 照抄;选中的行生效(暂存:删除行拿掉、新增行加入;还原:新增行拿掉、删除行放回);没选中
/// 的行,被写那一侧有的留着、没有的不搬。
fn expected_lines(
    old: &str,
    new: &str,
    hunk: &DiffHunk,
    pick: &LinePick,
    dir: HunkDirection,
) -> Vec<String> {
    let (target, insert_kind, start): (Vec<&str>, &str, u32) = match dir {
        HunkDirection::Apply => (old.lines().collect(), "add", hunk.old_start),
        HunkDirection::Revert => (new.lines().collect(), "delete", hunk.new_start),
    };
    let target_no = |line: &DiffLine| match dir {
        HunkDirection::Apply => line.old_lineno,
        HunkDirection::Revert => line.new_lineno,
    };
    let numbers: Vec<usize> = hunk
        .lines
        .iter()
        .filter_map(target_no)
        .map(|n| n as usize - 1)
        .collect();
    let (begin, end) = match (numbers.first(), numbers.last()) {
        (Some(&first), Some(&last)) => (first, last + 1),
        // 纯插入:插入点按 hunk 头(「插在第几行之前」)
        _ => {
            let at = (start.max(1) as usize - 1).min(target.len());
            (at, at)
        }
    };
    let mut out: Vec<String> = target[..begin].iter().map(|s| s.to_string()).collect();
    for (i, line) in hunk.lines.iter().enumerate() {
        let keep = match line.kind.as_str() {
            "context" => true,
            kind if kind == insert_kind => pick.contains(&i),
            _ => !pick.contains(&i),
        };
        if keep {
            out.push(line.content.clone());
        }
    }
    out.extend(target[end..].iter().map(|s| s.to_string()));
    out
}

/// 把 `cur` 逐块(整块)补成 `goal`:每补一块重算一次 diff(与弹窗里一块块点「暂存此块」
/// 一样)。
fn apply_all(mut cur: String, goal: &str) -> String {
    for _ in 0..200 {
        let diff = diff_two_texts(cur.clone(), goal.to_string());
        let Some(hunk) = diff.hunks.first() else {
            return cur;
        };
        cur = splice_hunk(&cur, goal, hunk, HunkDirection::Apply, "\n", None).unwrap();
    }
    panic!("补不完:{cur:?} → {goal:?}");
}

/// 把 `cur` 逐块(整块)还原成 `base`。
fn revert_all(base: &str, mut cur: String) -> String {
    for _ in 0..200 {
        let diff = diff_two_texts(base.to_string(), cur.clone());
        let Some(hunk) = diff.hunks.first() else {
            return cur;
        };
        cur = splice_hunk(base, &cur, hunk, HunkDirection::Revert, "\n", None).unwrap();
    }
    panic!("还原不完:{base:?} ← {cur:?}");
}

/// 整块拼接(不传选择、增删行全选、连上下文一起全选)与改造前的实现逐字节一致,块被改过
/// 时两边同样拒绝。随机文本覆盖 LF / CRLF / 混合换行、有无末尾换行、空文件、首尾行、
/// 纯增 / 纯删、多块。
#[test]
fn 整块拼接与改造前逐字节一致() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut seen = 0;
    for _ in 0..3000 {
        let (old, new) = random_pair(&mut rng);
        let diff = diff_two_texts(old.clone(), new.clone());
        for hunk in &diff.hunks {
            let mut tampered = hunk.clone();
            let at = rng.below(tampered.lines.len());
            tampered.lines[at].content.push('~');
            let changes = change_lines(hunk);
            let everything: LinePick = (0..hunk.lines.len()).collect();
            for dir in [HunkDirection::Apply, HunkDirection::Revert] {
                for fallback in ["\n", "\r\n"] {
                    let expect = splice_hunk_v0(&old, &new, hunk, dir, fallback).unwrap();
                    for lines in [None, Some(&changes), Some(&everything)] {
                        let got = splice_hunk(&old, &new, hunk, dir, fallback, lines).unwrap();
                        assert_eq!(got, expect, "old={old:?} new={new:?} {dir:?} {lines:?}");
                    }
                    let err = splice_hunk(&old, &new, &tampered, dir, fallback, None).unwrap_err();
                    assert!(err.is::<StaleDiff>());
                    assert!(splice_hunk_v0(&old, &new, &tampered, dir, fallback).is_err());
                }
            }
            seen += 1;
        }
    }
    assert!(seen > 2000, "语料太弱:只碰到 {seen} 块");
}

/// 按行暂存一部分、再把剩下的逐块整块暂存,结果必须正好是新侧;按行丢弃一部分、再把
/// 剩下的逐块丢弃,结果必须正好是旧侧。按行操作本身:没选中的增删行一行不动,选中的全部
/// 生效(按行内容核对)。随机文本(纯 LF);两侧末尾换行不一致时那处差异 diff 里看不见、
/// 整块操作也补不上,这时只核对行内容(末尾换行另有专门的用例)。
#[test]
fn 按行操作再补齐余下的块等于整份() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut seen = 0;
    for _ in 0..2000 {
        let (base, edited) = random_lines(&mut rng);
        let final_eol = rng.chance(70);
        let old = render(&mut rng, &base, Eol::Lf, final_eol);
        let new = render(&mut rng, &edited, Eol::Lf, final_eol);
        let diff = diff_two_texts(old.clone(), new.clone());
        if diff.hunks.is_empty() {
            continue;
        }
        let hunk = &diff.hunks[rng.below(diff.hunks.len())];
        let pick = random_pick(&mut rng, hunk);
        let case = format!("old={old:?} new={new:?} pick={pick:?}");
        // 末行是空串时 `render` 拼出来像是带了末尾换行:按拼出来的文本判
        let same_eof =
            old.is_empty() || new.is_empty() || old.ends_with('\n') == new.ends_with('\n');
        let lines = |s: &str| s.lines().map(str::to_string).collect::<Vec<_>>();

        let staged =
            splice_hunk(&old, &new, hunk, HunkDirection::Apply, "\n", Some(&pick)).unwrap();
        let expect = expected_lines(&old, &new, hunk, &pick, HunkDirection::Apply);
        assert_eq!(lines(&staged), expect, "{case}");
        let full = apply_all(staged, &new);
        assert_eq!(lines(&full), lines(&new), "{case}");
        if same_eof {
            assert_eq!(full, new, "{case}");
        }

        let discarded =
            splice_hunk(&old, &new, hunk, HunkDirection::Revert, "\n", Some(&pick)).unwrap();
        let expect = expected_lines(&old, &new, hunk, &pick, HunkDirection::Revert);
        assert_eq!(lines(&discarded), expect, "{case}");
        let full = revert_all(&old, discarded);
        assert_eq!(lines(&full), lines(&old), "{case}");
        if same_eof {
            assert_eq!(full, old, "{case}");
        }
        seen += 1;
    }
    assert!(seen > 1000, "语料太弱:只碰到 {seen} 例");
}

// ── 纯函数:按行拼接 ───────────────────────────────────────

/// 块里按 `(种类, 内容)` 挑行(内容在块里唯一时才好用)。
fn pick_of(hunk: &DiffHunk, wanted: &[(&str, &str)]) -> LinePick {
    wanted
        .iter()
        .map(|&(kind, content)| {
            hunk.lines
                .iter()
                .position(|l| l.kind == kind && l.content == content)
                .unwrap_or_else(|| panic!("块里没有 {kind} {content}: {hunk:?}"))
        })
        .collect()
}

/// 对 `old → new` 的唯一一块按行拼接。
fn splice_lines(old: &str, new: &str, wanted: &[(&str, &str)], dir: HunkDirection) -> String {
    let diff = diff_two_texts(old.to_string(), new.to_string());
    assert_eq!(diff.hunks.len(), 1, "{:?}", diff.hunks);
    let pick = pick_of(&diff.hunks[0], wanted);
    splice_hunk(old, new, &diff.hunks[0], dir, "\n", Some(&pick)).unwrap()
}

/// 改写两行、只选其中一对的一半:暂存时没选中的删除行留作上下文、没选中的新增行不加;
/// 还原时只把选中的那两行反向还原。
#[test]
fn 按行暂存与还原的拼接语义() {
    let old = "l1\nl2\nl3\nl4\nl5\nl6\n";
    let new = "l1\nl2\nA\nB\nl5\nl6\n";
    let staged = splice_lines(
        old,
        new,
        &[("delete", "l3"), ("add", "B")],
        HunkDirection::Apply,
    );
    assert_eq!(staged, "l1\nl2\nl4\nB\nl5\nl6\n");
    let reverted = splice_lines(
        old,
        new,
        &[("delete", "l3"), ("add", "A")],
        HunkDirection::Revert,
    );
    assert_eq!(reverted, "l1\nl2\nl3\nB\nl5\nl6\n");
}

/// 纯新增块 / 纯删除块里只选一部分。
#[test]
fn 纯增块与纯删块按行拼接() {
    let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
    let added = "l1\nl2\nl3\nl4\nn1\nn2\nn3\nl5\nl6\nl7\nl8\n";
    let only_n2 = "l1\nl2\nl3\nl4\nn2\nl5\nl6\nl7\nl8\n";
    assert_eq!(
        splice_lines(old, added, &[("add", "n2")], HunkDirection::Apply),
        only_n2
    );
    assert_eq!(
        splice_lines(
            old,
            added,
            &[("add", "n1"), ("add", "n3")],
            HunkDirection::Revert
        ),
        only_n2
    );

    let deleted = "l1\nl2\nl6\nl7\nl8\n";
    assert_eq!(
        splice_lines(old, deleted, &[("delete", "l4")], HunkDirection::Apply),
        "l1\nl2\nl3\nl5\nl6\nl7\nl8\n"
    );
    assert_eq!(
        splice_lines(
            old,
            deleted,
            &[("delete", "l3"), ("delete", "l5")],
            HunkDirection::Revert
        ),
        "l1\nl2\nl3\nl5\nl6\nl7\nl8\n"
    );
}

/// 选中的是块的首行与末行;块贴着文件开头 / 文件末尾也一样。
#[test]
fn 选中块的首行与末行() {
    let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let new = "l1\nl2\nl3\nX\nY\nZ\nl7\nl8\nl9\nl10\n";
    assert_eq!(
        splice_lines(
            old,
            new,
            &[("delete", "l4"), ("add", "Z")],
            HunkDirection::Apply
        ),
        "l1\nl2\nl3\nl5\nl6\nZ\nl7\nl8\nl9\nl10\n"
    );
    assert_eq!(
        splice_lines(
            old,
            new,
            &[("delete", "l4"), ("add", "Z")],
            HunkDirection::Revert
        ),
        "l1\nl2\nl3\nl4\nX\nY\nl7\nl8\nl9\nl10\n"
    );
    // 块在文件开头
    assert_eq!(
        splice_lines(
            "a\nb\nc\n",
            "A\nB\nc\n",
            &[("delete", "a"), ("add", "B")],
            HunkDirection::Apply
        ),
        "b\nB\nc\n"
    );
    // 块在文件末尾
    assert_eq!(
        splice_lines(
            "a\nb\nc\n",
            "a\nB\nC\n",
            &[("delete", "c"), ("add", "C")],
            HunkDirection::Apply
        ),
        "a\nb\nC\n"
    );
}

/// 暂存区 LF、工作区 CRLF(autocrlf=true 的常态):按行暂存写回的仍是纯 LF,按行丢弃
/// 写回的仍是纯 CRLF,块外字节一个不动。
#[test]
fn 按行拼接保留被写那一侧的换行() {
    let base: Vec<String> = (1..=10).map(|i| format!("l{i}")).collect();
    let mut lines = base.clone();
    lines[2] = "X".into();
    lines[3] = "Y".into();
    let index: String = base.iter().map(|l| format!("{l}\n")).collect();
    let worktree: String = lines.iter().map(|l| format!("{l}\r\n")).collect();
    let pick = [("delete", "l3"), ("add", "X")];

    let staged = splice_lines(&index, &worktree, &pick, HunkDirection::Apply);
    assert_eq!(staged, "l1\nl2\nl4\nX\nl5\nl6\nl7\nl8\nl9\nl10\n");

    let discarded = splice_lines(&index, &worktree, &pick, HunkDirection::Revert);
    assert_eq!(
        discarded,
        "l1\r\nl2\r\nl3\r\nY\r\nl5\r\nl6\r\nl7\r\nl8\r\nl9\r\nl10\r\n"
    );
}

/// 末尾无换行:按行操作保持被写那一侧原来有没有末尾换行,不管哪一行落到了末尾;被挤到
/// 中间的原末行补上换行。整块照旧跟随来源侧。
#[test]
fn 按行拼接的末尾换行() {
    use HunkDirection::{Apply, Revert};
    // 两侧都没有末尾换行,块碰到末尾:结果也没有
    let (old, new) = ("a\nx", "a\ny\nz");
    assert_eq!(splice_lines(old, new, &[("add", "z")], Apply), "a\nx\nz");
    assert_eq!(splice_lines(old, new, &[("add", "y")], Apply), "a\nx\ny");
    assert_eq!(splice_lines(old, new, &[("delete", "x")], Apply), "a");
    assert_eq!(splice_lines(old, new, &[("add", "y")], Revert), "a\nz");
    assert_eq!(
        splice_lines(old, new, &[("delete", "x"), ("add", "z")], Revert),
        "a\nx\ny"
    );
    // 被写那一侧有末尾换行、来源侧没有:按行操作仍保留(那处差异看不见、勾不中)
    let (old, new) = ("a\nx\n", "a\ny\nz");
    assert_eq!(splice_lines(old, new, &[("add", "z")], Apply), "a\nx\nz\n");
    assert_eq!(splice_lines(old, new, &[("delete", "x")], Apply), "a\n");
    // 末行是两侧都有的上下文、只是末尾换行不同:按行操作不碰它
    let (old, new) = ("x\nb", "y\nb\n");
    assert_eq!(splice_lines(old, new, &[("delete", "x")], Apply), "b");
    assert_eq!(splice_lines(old, new, &[("add", "y")], Apply), "x\ny\nb");
    // 删掉末行、倒数第二行落到末尾:不凭空多出末尾换行
    let (old, new) = ("p\nq\nr", "P\nq");
    assert_eq!(
        splice_lines(old, new, &[("delete", "r"), ("add", "P")], Apply),
        "p\nP\nq"
    );
    // 空文件上按行加:跟随搬进来那一行在来源侧的写法
    assert_eq!(splice_lines("", "a\nb\nc", &[("add", "c")], Apply), "c");
    assert_eq!(splice_lines("", "a\nb\nc", &[("add", "b")], Apply), "b\n");
    // 整块照旧跟随来源侧
    let (old, new) = ("x\nb", "y\nb\n");
    let diff = diff_two_texts(old.to_string(), new.to_string());
    assert_eq!(
        splice_hunk(old, new, &diff.hunks[0], Apply, "\n", None).unwrap(),
        new
    );
}

/// 几块一趟拼:与「先拼第一块、重算 diff、再拼第二块」结果一样,第二块用整块。
#[test]
fn 多块一趟拼() {
    let base: Vec<String> = (1..=40).map(|i| format!("l{i}")).collect();
    let mut lines = base.clone();
    lines[2] = "A".into();
    lines.insert(3, "A2".into());
    lines[30] = "B".into();
    let old: String = base.iter().map(|l| format!("{l}\n")).collect();
    let new: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let diff = diff_two_texts(old.clone(), new.clone());
    assert_eq!(diff.hunks.len(), 2);
    let first = pick_of(&diff.hunks[0], &[("add", "A2")]);
    let both = splice_hunks(
        &old,
        &new,
        &[(&diff.hunks[0], Some(&first)), (&diff.hunks[1], None)],
        HunkDirection::Apply,
        "\n",
    )
    .unwrap();
    let mut expect = base.clone();
    expect.insert(3, "A2".into());
    expect[30] = "B".into();
    let expect: String = expect.iter().map(|l| format!("{l}\n")).collect();
    assert_eq!(both, expect);

    // 先后两趟:结果相同
    let step = splice_hunk(
        &old,
        &new,
        &diff.hunks[0],
        HunkDirection::Apply,
        "\n",
        Some(&first),
    )
    .unwrap();
    let rest = diff_two_texts(step.clone(), new.clone());
    let second = rest
        .hunks
        .iter()
        .find(|h| h.lines.iter().any(|l| l.content == "B"))
        .unwrap();
    assert_eq!(
        splice_hunk(&step, &new, second, HunkDirection::Apply, "\n", None).unwrap(),
        expect
    );
}

/// 选择本身不成立:一行改动都没选上(不算陈旧,是调用方的错)、下标越界(算陈旧)、
/// 块给重了 / 先后颠倒(算陈旧)、没选中的行被改过(照样对账,算陈旧)。
#[test]
fn 按行选择不成立就拒绝() {
    let old = "l1\nl2\nl3\nl4\nl5\n";
    let new = "l1\nl2\nX\nl4\nl5\n";
    let diff = diff_two_texts(old.to_string(), new.to_string());
    let hunk = &diff.hunks[0];
    let context: LinePick = (0..hunk.lines.len())
        .filter(|&i| hunk.lines[i].kind == "context")
        .collect();
    for pick in [LinePick::new(), context] {
        let err = splice_hunk(old, new, hunk, HunkDirection::Apply, "\n", Some(&pick)).unwrap_err();
        assert!(!err.is::<StaleDiff>(), "{err:#}");
    }
    let beyond = LinePick::from([0, 99]);
    let err = splice_hunk(old, new, hunk, HunkDirection::Apply, "\n", Some(&beyond)).unwrap_err();
    assert!(err.is::<StaleDiff>());
    let twice = [(hunk, None), (hunk, None)];
    let err = splice_hunks(old, new, &twice, HunkDirection::Apply, "\n").unwrap_err();
    assert!(err.is::<StaleDiff>(), "同一块给两次");
    // 没选中的那一行被改过:照样对账
    let pick = pick_of(hunk, &[("add", "X")]);
    let mut tampered = hunk.clone();
    let del = tampered
        .lines
        .iter()
        .position(|l| l.kind == "delete")
        .unwrap();
    tampered.lines[del].content = "changed".into();
    let err =
        splice_hunk(old, new, &tampered, HunkDirection::Apply, "\n", Some(&pick)).unwrap_err();
    assert!(err.is::<StaleDiff>());
}

/// 块先后颠倒也拒绝(拼接一趟从前往后走)。
#[test]
fn 多块先后颠倒就拒绝() {
    let base: Vec<String> = (1..=30).map(|i| format!("l{i}")).collect();
    let mut lines = base.clone();
    lines[1] = "A".into();
    lines[25] = "B".into();
    let old: String = base.iter().map(|l| format!("{l}\n")).collect();
    let new: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let diff = diff_two_texts(old.clone(), new.clone());
    assert_eq!(diff.hunks.len(), 2);
    let picks = [(&diff.hunks[1], None), (&diff.hunks[0], None)];
    let err = splice_hunks(&old, &new, &picks, HunkDirection::Apply, "\n").unwrap_err();
    assert!(err.is::<StaleDiff>());
}

// ── 真仓库 ──────────────────────────────────────────────

fn numbered(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("l{i}")).collect()
}

fn join(lines: &[String], eol: &str) -> String {
    lines.iter().map(|l| format!("{l}{eol}")).collect()
}

fn index_blob(root: &Path, rel: &str) -> String {
    let repo = Repository::open(root).unwrap();
    let index = repo.index().unwrap();
    let entry = index.get_path(Path::new(rel), 0).unwrap();
    String::from_utf8(repo.find_blob(entry.id).unwrap().content().to_vec()).unwrap()
}

fn head_blob(root: &Path, rel: &str) -> String {
    let repo = Repository::open(root).unwrap();
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    let entry = tree.get_path(Path::new(rel)).unwrap();
    let blob = repo.find_blob(entry.id()).unwrap();
    String::from_utf8(blob.content().to_vec()).unwrap()
}

/// 未暂存 diff 里第 `hunk` 块按 `(种类, 内容)` 挑出来的行。
fn lines_pick(diff: &GitDiffResult, hunk: usize, wanted: &[(&str, &str)]) -> HunkPick {
    HunkPick::lines(hunk, pick_of(&diff.hunks[hunk], wanted))
}

fn worktree(root: &Path, rel: &str) -> String {
    String::from_utf8(std::fs::read(root.join(rel)).unwrap()).unwrap()
}

fn cleanup(root: PathBuf) {
    std::fs::remove_dir_all(root).ok();
}

/// 按行暂存:暂存区逐字节就是「选中的行生效、其余不动」,工作区一个字节不动;再按行
/// 取消暂存其中一行,暂存区跟着回退那一行。
#[test]
fn 按行暂存与取消暂存后暂存区逐字节正确() {
    let rel = "src/a.txt";
    let base = join(&numbered(30), "\n");
    let root = init_repo_with_file("lines-stage", rel, &base);
    let mut lines = numbered(30);
    lines[2] = "X".into();
    lines[3] = "Y".into();
    lines.insert(20, "N1".into());
    lines.insert(21, "N2".into());
    let edited = join(&lines, "\n");
    std::fs::write(root.join(rel), &edited).unwrap();

    let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    assert_eq!(unstaged.hunks.len(), 2);
    let pick = lines_pick(&unstaged, 0, &[("delete", "l3"), ("add", "X")]);
    stage_hunks(&root, rel, &unstaged, &[pick]).unwrap();
    let mut expect = numbered(30);
    expect.remove(2);
    expect.insert(3, "X".into());
    assert_eq!(index_blob(&root, rel), join(&expect, "\n"));
    assert_eq!(worktree(&root, rel), edited, "工作区不动");

    let staged = get_git_diff(&root, rel, DiffSide::Staged).unwrap();
    let pick = lines_pick(&staged, 0, &[("add", "X")]);
    unstage_hunks(&root, rel, &staged, &[pick]).unwrap();
    let mut expect = numbered(30);
    expect.remove(2);
    assert_eq!(index_blob(&root, rel), join(&expect, "\n"));
    cleanup(root);
}

/// 纯新增块与纯删除块各选一部分,一次暂存两块:只写一次、两块都按选中的行生效。
#[test]
fn 纯增块与纯删块一次按行暂存() {
    let rel = "a.txt";
    let base = join(&numbered(20), "\n");
    let root = init_repo_with_file("lines-pure", rel, &base);
    let mut lines = numbered(20);
    lines.drain(14..17);
    for (k, name) in ["N1", "N2", "N3"].iter().enumerate() {
        lines.insert(5 + k, name.to_string());
    }
    std::fs::write(root.join(rel), join(&lines, "\n")).unwrap();

    let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    assert_eq!(unstaged.hunks.len(), 2);
    let picks = [
        lines_pick(&unstaged, 0, &[("add", "N2")]),
        lines_pick(&unstaged, 1, &[("delete", "l16")]),
    ];
    stage_hunks(&root, rel, &unstaged, &picks).unwrap();
    let mut expect = numbered(20);
    expect.remove(15);
    expect.insert(5, "N2".into());
    assert_eq!(index_blob(&root, rel), join(&expect, "\n"));
    cleanup(root);
}

/// 末尾没有换行的文件:按行暂存不给它凭空添上换行,也不吞掉选中行的内容。
#[test]
fn 无末尾换行的文件按行暂存() {
    let rel = "a.txt";
    let root = init_repo_with_file("lines-noeol", rel, "l1\nl2\nl3");
    std::fs::write(root.join(rel), "l1\nX\nl3\nY").unwrap();
    let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    assert_eq!(unstaged.hunks.len(), 1);
    stage_hunks(
        &root,
        rel,
        &unstaged,
        &[lines_pick(&unstaged, 0, &[("add", "Y")])],
    )
    .unwrap();
    assert_eq!(index_blob(&root, rel), "l1\nl2\nl3\nY");

    // 回到原样再选另一对:末行仍是那个没有换行的 l3
    git_stage(&root, &[]).unwrap();
    let repo = Repository::open(&root).unwrap();
    let mut index = repo.index().unwrap();
    let entry = index.get_path(Path::new(rel), 0).unwrap();
    index.add_frombuffer(&entry, b"l1\nl2\nl3").unwrap();
    index.write().unwrap();
    let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    let pick = lines_pick(&unstaged, 0, &[("delete", "l2"), ("add", "X")]);
    stage_hunks(&root, rel, &unstaged, &[pick]).unwrap();
    assert_eq!(index_blob(&root, rel), "l1\nX\nl3");
    cleanup(root);
}

/// autocrlf=true:暂存区 LF、工作区 CRLF。按行暂存写进去的仍是 LF,按行丢弃写回的仍是 CRLF。
#[test]
fn autocrlf_仓库按行操作保留两侧换行() {
    let rel = "crlf.txt";
    let base = join(&numbered(12), "\n");
    let root = init_repo_with_file("lines-crlf", rel, &base);
    Repository::open(&root)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("core.autocrlf", true)
        .unwrap();
    let mut lines = numbered(12);
    lines[2] = "X".into();
    lines[3] = "Y".into();
    std::fs::write(root.join(rel), join(&lines, "\r\n")).unwrap();

    let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    let pick = lines_pick(&unstaged, 0, &[("delete", "l3"), ("add", "X")]);
    stage_hunks(&root, rel, &unstaged, &[pick]).unwrap();
    let mut expect = numbered(12);
    expect.remove(2);
    expect.insert(3, "X".into());
    assert_eq!(index_blob(&root, rel), join(&expect, "\n"));

    // 暂存区现在是 l1 l2 l4 X l5…,工作区 l1 l2 X Y l5…:丢弃工作区里的 Y
    let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    let pick = lines_pick(&unstaged, 0, &[("add", "Y")]);
    discard_hunks(&root, rel, &unstaged, &[pick]).unwrap();
    let mut expect = numbered(12);
    expect[2] = "X".into();
    expect.remove(3);
    assert_eq!(worktree(&root, rel), join(&expect, "\r\n"));
    cleanup(root);
}

/// 按行丢弃:工作区只把选中的两行反向还原,没选中的改动原样留着;暂存区不动。
#[test]
fn 按行丢弃后工作区内容正确() {
    let rel = "a.txt";
    let base = join(&numbered(20), "\n");
    let root = init_repo_with_file("lines-discard", rel, &base);
    let mut lines = numbered(20);
    lines[2] = "X".into();
    lines[3] = "Y".into();
    std::fs::write(root.join(rel), join(&lines, "\n")).unwrap();

    let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    let pick = lines_pick(&unstaged, 0, &[("delete", "l3"), ("add", "X")]);
    discard_hunks(&root, rel, &unstaged, &[pick]).unwrap();
    let mut expect = numbered(20);
    expect[3] = "Y".into();
    assert_eq!(worktree(&root, rel), join(&expect, "\n"));
    assert_eq!(index_blob(&root, rel), base, "暂存区不动");
    cleanup(root);
}

/// 取 diff 之后文件又被改了(AI 边改、人边审):按行操作同样按陈旧拒绝,两侧都不动。
#[test]
fn 按行操作陈旧时拒绝执行() {
    let rel = "a.txt";
    let base = join(&numbered(10), "\n");
    let root = init_repo_with_file("lines-stale", rel, &base);
    let edited = base.replacen("l4\n", "FOUR\n", 1);
    std::fs::write(root.join(rel), &edited).unwrap();
    let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    let pick = lines_pick(&unstaged, 0, &[("add", "FOUR")]);

    let later = edited.replacen("l9\n", "NINE\n", 1);
    std::fs::write(root.join(rel), &later).unwrap();
    let err = stage_hunks(&root, rel, &unstaged, std::slice::from_ref(&pick)).unwrap_err();
    assert!(err.is::<StaleDiff>(), "实际: {err:#}");
    let err = discard_hunks(&root, rel, &unstaged, std::slice::from_ref(&pick)).unwrap_err();
    assert!(err.is::<StaleDiff>(), "实际: {err:#}");
    assert_eq!(index_blob(&root, rel), base);
    assert_eq!(worktree(&root, rel), later);

    // 选择与块对不上(下标越界)同样算陈旧
    std::fs::write(root.join(rel), &edited).unwrap();
    let fresh = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    let beyond = HunkPick::lines(0, LinePick::from([99]));
    let err = stage_hunks(&root, rel, &fresh, &[beyond]).unwrap_err();
    assert!(err.is::<StaleDiff>(), "实际: {err:#}");
    assert_eq!(index_blob(&root, rel), base);
    cleanup(root);
}

/// 新增文件按行取消暂存:取消一部分留下其余;全部增删行都取消 = 整块,移出暂存区。
#[test]
fn 新增文件按行取消暂存() {
    let root = init_repo_with_file("lines-new", "a.txt", "a\n");
    std::fs::write(root.join("new.txt"), "n1\nn2\nn3\n").unwrap();
    git_stage(&root, &["new.txt".to_string()]).unwrap();
    let staged = get_git_diff(&root, "new.txt", DiffSide::Staged).unwrap();
    let pick = lines_pick(&staged, 0, &[("add", "n2")]);
    unstage_hunks(&root, "new.txt", &staged, &[pick]).unwrap();
    assert_eq!(index_blob(&root, "new.txt"), "n1\nn3\n");

    let staged = get_git_diff(&root, "new.txt", DiffSide::Staged).unwrap();
    let all = HunkPick::lines(0, change_lines(&staged.hunks[0]));
    unstage_hunks(&root, "new.txt", &staged, &[all]).unwrap();
    let repo = Repository::open(&root).unwrap();
    assert!(
        repo.index()
            .unwrap()
            .get_path(Path::new("new.txt"), 0)
            .is_none()
    );
    cleanup(root);
}

/// 提交流程只提交暂存区:按行暂存一部分之后 `git commit`,HEAD 里正好是暂存的那部分,
/// 工作区里没暂存的改动原样留着、仍是未暂存。
#[test]
fn 按行暂存后提交只带上已暂存的部分() {
    let rel = "a.txt";
    let base = join(&numbered(20), "\n");
    let root = init_repo_with_file("lines-commit", rel, &base);
    let mut lines = numbered(20);
    lines[2] = "X".into();
    lines[15] = "Z".into();
    let edited = join(&lines, "\n");
    std::fs::write(root.join(rel), &edited).unwrap();

    let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    assert_eq!(unstaged.hunks.len(), 2);
    let pick = lines_pick(&unstaged, 0, &[("delete", "l3"), ("add", "X")]);
    stage_hunks(&root, rel, &unstaged, &[pick]).unwrap();
    let staged = index_blob(&root, rel);

    git_commit_with(&root, "partial", false).unwrap();
    assert_eq!(head_blob(&root, rel), staged);
    assert_eq!(worktree(&root, rel), edited);
    let rest = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
    let rest: Vec<(&str, &str)> = rest
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| l.kind != "context")
        .map(|l| (l.kind.as_str(), l.content.as_str()))
        .collect();
    assert_eq!(rest, vec![("delete", "l16"), ("add", "Z")]);
    cleanup(root);
}
