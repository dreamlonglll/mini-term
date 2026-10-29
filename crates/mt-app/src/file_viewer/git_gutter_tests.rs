use std::path::PathBuf;

use gpui::{KeyContext, Keymap, Keystroke};
use mt_project::git::{HeadSignature, line_hunks};

use super::*;

const LH: f32 = 20.0;

/// 第 `line` 行从 `top` 起、占 `rows` 个视觉行。
fn row(line: usize, top: f32, rows: f32) -> RowSpan {
    RowSpan {
        line,
        top: px(top),
        bottom: px(top + LH * rows),
    }
}

/// 第 `first..=last` 行,每行一个视觉行,从 y = 100 起连续排。
fn rows(first: usize, last: usize) -> Vec<RowSpan> {
    (first..=last)
        .map(|line| row(line, 100.0 + LH * (line - first) as f32, 1.0))
        .collect()
}

fn hunk(kind: LineChangeKind, new_start: usize, new_len: usize) -> LineHunk {
    LineHunk {
        kind,
        new_start,
        new_len,
        old_start: new_start,
        old_len: if kind == LineChangeKind::Added { 0 } else { 1 },
    }
}

use LineChangeKind::{Added, Deleted, Modified};

#[test]
fn 折叠藏起来的行被剔掉_只留真身() {
    // 第 4、5 行被折进了第 3 行的折叠区:range_to_bounds 把它们落到第 6 行上(同顶)
    let raw = vec![
        row(3, 100.0, 1.0),
        row(4, 120.0, 1.0),
        row(5, 120.0, 1.0),
        row(6, 120.0, 1.0),
        row(7, 140.0, 2.0),
    ];
    let kept: Vec<usize> = dedupe_hidden_rows(raw).iter().map(|r| r.line).collect();
    assert_eq!(kept, vec![3, 6, 7]);
    assert!(dedupe_hidden_rows(Vec::new()).is_empty());
}

#[test]
fn 标记段_新增修改按行并段_软换行整段覆盖() {
    // 第 12 行软换行占两个视觉行
    let mut visible = rows(10, 20);
    visible[2] = row(12, 140.0, 2.0);
    for r in visible.iter_mut().skip(3) {
        r.top += px(LH);
        r.bottom += px(LH);
    }
    let hunks = vec![hunk(Added, 12, 2), hunk(Modified, 15, 1)];
    let segs = marker_segments(&hunks, &visible);
    assert_eq!(segs.len(), 2);
    // 新增块 12..14:从第 12 行顶到第 13 行底,中间的续行一并覆盖
    assert_eq!((segs[0].hunk, segs[0].kind), (0, Added));
    assert_eq!(segs[0].top, px(140.0));
    assert_eq!(segs[0].bottom, px(200.0));
    assert_eq!((segs[1].hunk, segs[1].kind), (1, Modified));
    assert_eq!(segs[1].bottom - segs[1].top, px(LH));
}

#[test]
fn 标记段_删除落在分界线_文件末尾落在末行底() {
    let visible = rows(10, 20);
    let hunks = vec![hunk(Deleted, 18, 0), hunk(Deleted, 21, 0)];
    let segs = marker_segments(&hunks, &visible);
    assert_eq!(segs.len(), 2);
    // 删除点在第 18 行之前 = 第 18 行的顶
    assert_eq!(segs[0].top, visible[8].top);
    assert_eq!(segs[0].top, segs[0].bottom);
    // 删在文件末尾(第 21 行不存在)= 最后一行的底
    assert_eq!(segs[1].top, visible[10].bottom);
}

#[test]
fn 标记段_只画与可见行相交的块() {
    let visible = rows(10, 20);
    // 按新侧行号升序、互不重叠(与 line_hunks 的产出一致)
    let hunks = vec![
        hunk(Added, 0, 2),    // 整块在可见区之上
        hunk(Deleted, 5, 0),  // 分界线在第 5 行顶:不可见
        hunk(Modified, 8, 3), // 8..11 跨进可见区:只画第 10 行
        hunk(Added, 30, 1),   // 在可见区之下
    ];
    let segs = marker_segments(&hunks, &visible);
    assert_eq!(segs.len(), 1, "{segs:?}");
    assert_eq!(segs[0].kind, Modified);
    assert_eq!(segs[0].top, visible[0].top);
    assert_eq!(segs[0].bottom, visible[0].bottom);
    assert!(marker_segments(&hunks, &[]).is_empty());
}

#[test]
fn 命中判定_竖条按行_删除按半径() {
    let segs = vec![
        MarkerSegment {
            hunk: 0,
            kind: Added,
            top: px(100.0),
            bottom: px(140.0),
        },
        MarkerSegment {
            hunk: 1,
            kind: Deleted,
            top: px(200.0),
            bottom: px(200.0),
        },
    ];
    assert_eq!(segment_at(&segs, px(100.0)).map(|s| s.hunk), Some(0));
    assert_eq!(segment_at(&segs, px(139.0)).map(|s| s.hunk), Some(0));
    assert_eq!(segment_at(&segs, px(140.0)), None, "下沿是开区间");
    assert_eq!(segment_at(&segs, px(196.0)).map(|s| s.hunk), Some(1));
    assert_eq!(segment_at(&segs, px(205.0)).map(|s| s.hunk), Some(1));
    assert_eq!(segment_at(&segs, px(206.0)), None);
}

#[test]
fn 上一处下一处_按光标找且首尾回绕() {
    let hunks = vec![
        hunk(Added, 2, 2),    // 2..4
        hunk(Deleted, 10, 0), // 第 10 行之前
        hunk(Modified, 20, 3),
    ];
    // 光标在最前:下一处是第一块
    assert_eq!(adjacent_hunk(&hunks, 0, None, true), Some(0));
    // 光标落在第一块里:两个方向都跳过它自己
    assert_eq!(adjacent_hunk(&hunks, 3, None, true), Some(1));
    assert_eq!(
        adjacent_hunk(&hunks, 3, None, false),
        Some(2),
        "前面没有了,回绕到最后"
    );
    // 光标在删除点那一行:跳过它
    assert_eq!(adjacent_hunk(&hunks, 10, None, true), Some(2));
    assert_eq!(adjacent_hunk(&hunks, 10, None, false), Some(0));
    // 末块之后:下一处回绕到第一块
    assert_eq!(adjacent_hunk(&hunks, 30, None, true), Some(0));
    assert_eq!(adjacent_hunk(&hunks, 30, None, false), Some(2));
    // 浮层开着:按浮层那一块前后挪,回绕
    assert_eq!(adjacent_hunk(&hunks, 0, Some(2), true), Some(0));
    assert_eq!(adjacent_hunk(&hunks, 0, Some(0), false), Some(2));
    assert_eq!(adjacent_hunk(&[], 0, None, true), None);
}

/// 照编辑脚本回滚:替换一段字节,返回新文本。
fn apply(text: &str, edit: (std::ops::Range<usize>, String)) -> String {
    let (range, replacement) = edit;
    format!(
        "{}{}{}",
        &text[..range.start],
        replacement,
        &text[range.end..]
    )
}

fn rollback(old: &str, new: &str, index: usize) -> String {
    let hunks = line_hunks(old, new);
    let old_lines: Vec<&str> = old.lines().collect();
    let hunk = &hunks[index];
    apply(new, rollback_edit(new, hunk, &old_lines[hunk.old_range()]))
}

#[test]
fn 回滚_三种块都还原成_head_的行() {
    let old = "a\nb\nc\nd\n";
    // 新增 → 删掉
    assert_eq!(rollback(old, "a\nb\nx\ny\nc\nd\n", 0), old);
    // 修改 → 换回原文
    assert_eq!(rollback(old, "a\nB\nC\nd\n", 0), old);
    // 删除 → 插回去
    assert_eq!(rollback(old, "a\nd\n", 0), old);
    // 删了首行 / 末行
    assert_eq!(rollback(old, "b\nc\nd\n", 0), old);
    assert_eq!(rollback(old, "a\nb\nc\n", 0), old);
}

#[test]
fn 回滚_末尾没有换行的文件不留空行() {
    // 末尾追加的行没有换行:连前一行的换行一起删
    assert_eq!(rollback("a\nb", "a\nb\nc", 0), "a\nb");
    // 删掉的末行插回到没有结尾换行的文件
    assert_eq!(
        rollback("a\nb\nc", "a\nb", 0).lines().collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    // 改末行
    assert_eq!(rollback("a\nb", "a\nB", 0), "a\nb");
    // 整份清空后回滚
    assert_eq!(
        rollback("x\ny\n", "", 0).lines().collect::<Vec<_>>(),
        vec!["x", "y"]
    );
}

/// 从后往前把每一块都回滚掉,结果的每一行必须与 HEAD 一模一样(前面的块不受后面
/// 块回滚的字节偏移影响,所以倒序做)。用确定性伪随机扫一批。
#[test]
fn 回滚_全部块倒序回滚后与_head_逐行相同() {
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let words = ["fn a", "let x", "}", "", "// c", "return"];
    for _ in 0..3000 {
        let make = |next: &mut dyn FnMut() -> u64| {
            let n = (next() % 10) as usize;
            let mut s: String = (0..n)
                .map(|_| words[(next() % words.len() as u64) as usize])
                .collect::<Vec<_>>()
                .join("\n");
            if next().is_multiple_of(2) && !s.is_empty() {
                s.push('\n');
            }
            s
        };
        let old = make(&mut next);
        let new = make(&mut next);
        let old_lines: Vec<&str> = old.lines().collect();
        let mut text = new.clone();
        for hunk in line_hunks(&old, &new).iter().rev() {
            let edit = rollback_edit(&text, hunk, &old_lines[hunk.old_range()]);
            text = apply(&text, edit);
        }
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            old_lines,
            "old={old:?} new={new:?} → {text:?}"
        );
    }
}

#[test]
fn head_原文归一_行尾折掉_tab_展开_复制用原文() {
    let repo = RepoFile {
        root: PathBuf::from("/repo"),
        rel_path: "main.go".into(),
        signature: HeadSignature::default(),
    };
    let outcome = prepare_head(HeadFile::Text {
        repo: repo.clone(),
        text: "package main\r\n\tx := 1\r\n".into(),
    });
    let base = outcome.base.expect("文本应当就绪");
    assert_eq!(base.text.as_ref(), "package main\n    x := 1\n");
    assert_eq!(base.lines, vec!["package main", "    x := 1"]);
    assert_eq!(base.raw_lines, vec!["package main", "\tx := 1"]);
    assert_eq!(outcome.repo, Some(repo.clone()));

    // 不在 HEAD 里:没有基准,但仓库定位要留着(轮询指纹用)
    let outcome = prepare_head(HeadFile::NotInHead(repo.clone()));
    assert!(outcome.base.is_none());
    assert_eq!(outcome.repo, Some(repo));
    let outcome = prepare_head(HeadFile::NotInRepo);
    assert!(outcome.base.is_none() && outcome.repo.is_none());
}

#[test]
fn head_与缓冲区同一口径_tab_文件不会整篇标成修改() {
    // 缓冲区是编辑器投影(Tab 已展开),HEAD 也按同一规则展开,没改的行必须对得上
    let outcome = prepare_head(HeadFile::Text {
        repo: RepoFile {
            root: PathBuf::from("/repo"),
            rel_path: "a.go".into(),
            signature: HeadSignature::default(),
        },
        text: "func f() {\r\n\treturn\r\n}\r\n".into(),
    });
    let base = outcome.base.unwrap();
    let buffer = "func f() {\n    return\n    // new\n}\n";
    assert_eq!(line_hunks(&base.text, buffer), vec![hunk(Added, 2, 1)]);
}

/// **机制回归测试**:Ctrl+Alt+Shift+↓/↑ 在编辑器里必须派发到本模块的动作。编辑器自带
/// `ctrl-alt-down`(加光标,`Input` 上下文,更深)与 Workspace 的 `ctrl-shift-down`
/// (AI 标记)都在 —— gpui 按修饰键精确匹配,它们不该抢走这两个键,反之亦然。
#[test]
fn 变更跳转键位不与编辑器和全局键冲突() {
    gpui::actions!(test_only, [AddCursorBelow, MarkerNext]);
    let mut bindings = vec![
        KeyBinding::new("ctrl-alt-down", AddCursorBelow, Some("Input")),
        KeyBinding::new("ctrl-shift-down", MarkerNext, Some("Workspace")),
    ];
    bindings.extend(git_change_key_bindings());
    let keymap = Keymap::new(bindings);
    let stack: Vec<KeyContext> = ["Workspace", "FileViewer", "Input"]
        .iter()
        .map(|n| KeyContext::parse(n).unwrap())
        .collect();
    let first = |ks: &str| {
        let (bindings, _) = keymap.bindings_for_input(&[Keystroke::parse(ks).unwrap()], &stack);
        bindings.first().map(|b| b.action().boxed_clone())
    };
    assert!(
        first("ctrl-alt-shift-down")
            .unwrap()
            .partial_eq(&GitNextChange)
    );
    assert!(
        first("ctrl-alt-shift-up")
            .unwrap()
            .partial_eq(&GitPrevChange)
    );
    assert!(first("ctrl-alt-down").unwrap().partial_eq(&AddCursorBelow));
    assert!(first("ctrl-shift-down").unwrap().partial_eq(&MarkerNext));

    // 不在文件查看器里(终端)时这两个键不归我们
    let terminal: Vec<KeyContext> = ["Workspace", "Terminal"]
        .iter()
        .map(|n| KeyContext::parse(n).unwrap())
        .collect();
    let (bindings, _) = keymap.bindings_for_input(
        &[Keystroke::parse("ctrl-alt-shift-down").unwrap()],
        &terminal,
    );
    assert!(bindings.is_empty());
}
