use mt_project::git::DiffLine;

use super::*;

#[gpui::test]
async fn last_hunk_reloads_sequence_and_preserves_adopted_hooks(cx: &mut gpui::TestAppContext) {
    cx.skip_drawing();
    cx.background_executor.allow_parking();
    let root = std::env::temp_dir().join(format!(
        "mini-term-diff-consumer-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git(&["init", "-q"]);
    git(&["config", "core.autocrlf", "false"]);
    for path in ["a", "b"] {
        std::fs::write(root.join(path), "old\n").unwrap();
    }
    git(&["add", "a", "b"]);
    for path in ["a", "b"] {
        std::fs::write(root.join(path), "new\n").unwrap();
    }
    let repo = GitContext::local(root.to_string_lossy().into_owned());
    let file = |path: &str| DiffFile {
        path: path.into(),
        old_path: None,
        side: DiffSide::Unstaged,
        status_label: "M".into(),
        hunk_actions: true,
    };
    let stale_hook = Rc::new(std::cell::Cell::new(0));
    let changed = Rc::new(std::cell::Cell::new(0));
    let switched = Rc::new(std::cell::RefCell::new(Vec::new()));
    let (state, ctl) = cx.update(|cx| {
        let store = cx.new(|cx| AppStore::git_test_store(mt_config::AppConfig::default(), cx));
        let stale = stale_hook.clone();
        let page = pages::FilePage::new(
            &store,
            repo.clone(),
            vec![file("a"), file("b")],
            0,
            FileDiffHooks {
                on_changed: Some(Rc::new(move |_| stale.set(stale.get() + 1))),
                ..FileDiffHooks::default()
            },
            cx,
        );
        (page.state.clone(), page.ctl.clone())
    });
    cx.condition(&state, |s, _| !s.loading).await;
    cx.update(|cx| {
        let page = pages::FilePage {
            state: state.clone(),
            ctl: ctl.clone(),
        };
        let (new_changed, new_switched) = (changed.clone(), switched.clone());
        let remaining = file("b");
        page.adopt(
            repo.clone(),
            vec![file("a"), file("b")],
            0,
            FileDiffHooks {
                on_changed: Some(Rc::new(move |_| new_changed.set(new_changed.get() + 1))),
                on_switch: Some(Rc::new(move |file, _| {
                    new_switched.borrow_mut().push(file.path.clone())
                })),
                reload_files: Some(Rc::new(move |done, cx| {
                    done(Some(vec![remaining.clone()]), cx)
                })),
            },
            cx,
        );
        let target = state.read(cx).op_target(OpScope::Hunk(0)).unwrap();
        run_hunk_op(&state, &ctl, HunkOp::Stage, target, cx);
        assert!(ctl.read(cx).busy.is_some());
        page.adopt(
            GitContext::local("/must-not-adopt-during-write"),
            vec![file("other")],
            0,
            FileDiffHooks::default(),
            cx,
        );
        assert_eq!(ctl.read(cx).context, repo);
    });
    cx.condition(&ctl, |c, _| c.busy.is_none()).await;
    cx.condition(&state, |s, _| !s.loading).await;
    cx.update(|cx| {
        assert_eq!(ctl.read(cx).file().path, "b");
        assert!(state.read(cx).error.is_none());
        assert_eq!(state.read(cx).result.as_ref().unwrap().new_content, "new\n");
    });
    assert_eq!(stale_hook.get(), 0);
    assert_eq!(changed.get(), 1);
    assert_eq!(&*switched.borrow(), &["b".to_string()]);
    assert_eq!(git(&["show", ":a"]), b"new\n");
    assert_eq!(git(&["show", ":b"]), b"old\n");
    std::fs::remove_dir_all(root).unwrap();
}

fn hunk(lines: Vec<DiffLine>) -> DiffHunk {
    DiffHunk {
        old_start: 1,
        old_lines: 0,
        new_start: 1,
        new_lines: 0,
        lines,
    }
}

fn staged(paths: &[&str]) -> Vec<DiffFile> {
    paths
        .iter()
        .map(|p| DiffFile {
            path: p.to_string(),
            old_path: None,
            side: DiffSide::Staged,
            status_label: "M".into(),
            hunk_actions: true,
        })
        .collect()
}

/// 块操作后换序列:当前文件还在就停在它(下标跟着别的文件进出变);离开了就落到原
/// 位置上的下一个,后面没有了取前面最近的;组空了是 `None`。
#[test]
fn 块操作后序列跟着更改列表走() {
    let old = staged(&["a", "app.ts", "c", "d", "e"]);
    // 还在:前面的 a 走了,app.ts 从第 2 个变成第 1 个
    assert_eq!(
        resync_index(&old, 1, &staged(&["app.ts", "c", "d", "e"])),
        Some(0)
    );
    // app.ts 离开了已暂存组:停在原位置上的下一个 c(新序列里第 2 个)
    assert_eq!(
        resync_index(&old, 1, &staged(&["a", "c", "d", "e"])),
        Some(1)
    );
    // 原位置后面的也一起走了:跳过不在组里的,落到 e
    assert_eq!(resync_index(&old, 1, &staged(&["a", "e"])), Some(1));
    // 末尾那个离开:后面没有了,取前面最近的 d
    assert_eq!(
        resync_index(&old, 4, &staged(&["a", "app.ts", "c", "d"])),
        Some(3)
    );
    // 旧序列里一个都不剩(外部又改过一轮):按原位置落,越界取末尾
    assert_eq!(resync_index(&old, 3, &staged(&["x", "y"])), Some(1));
    // 组空了
    assert_eq!(resync_index(&old, 1, &[]), None);
}

/// 提交 diff 左栏:先选中的文件要滚到它在树里那一行(目录行在前、单链压缩);
/// 平铺时就是文件序;合并变更的文件按自己的路径进树(重命名的挂在新路径下)。
#[test]
fn 左栏初始选中文件所在的行() {
    let commit = |path: &str| {
        ModalFile::from_commit(
            "abc",
            CommitFileInfo {
                path: path.into(),
                status: "modified".into(),
                old_path: None,
            },
        )
    };
    let files = [commit("z.md"), commit("src/ui/a.rs"), commit("src/ui/b.rs")];
    // 树形:src/ui(压缩成一行)、a.rs、b.rs、z.md
    assert_eq!(file_row_of(&files, true, 0), Some(3));
    assert_eq!(file_row_of(&files, true, 2), Some(2));
    assert_eq!(file_row_of(&files, false, 0), Some(0));
    assert_eq!(file_row_of(&files, true, 9), None);

    let renamed = ModalFile::from_combined(CombinedFileChange {
        path: "new/dir/x.rs".into(),
        status: "renamed".into(),
        old_path: Some("old/x.rs".into()),
        base: None,
        head: None,
    });
    assert_eq!(renamed.old_path.as_deref(), Some("old/x.rs"));
    let rows = modal_rows(std::slice::from_ref(&renamed), true, &HashSet::new());
    assert!(
        matches!(&rows[0], ChangeTreeRow::Dir { name, .. } if name == "new/dir"),
        "{rows:?}"
    );
    assert_eq!(modal_file_tip(&renamed), "old/x.rs → new/dir/x.rs");
    assert_eq!(modal_file_tip(&files[1]), "src/ui/a.rs");
}

/// 左栏的行缓存着:与每帧现算的一致;换选中不动它,收起目录 / 切视图之后作废重算。
#[test]
fn 左栏行缓存随折叠与视图作废() {
    let commit = |path: &str| {
        ModalFile::from_commit(
            "abc",
            CommitFileInfo {
                path: path.into(),
                status: "modified".into(),
                old_path: None,
            },
        )
    };
    let files: Vec<ModalFile> = (0..50)
        .map(|i| commit(&format!("src/m{}/f{i}.rs", i % 4)))
        .collect();
    let mut pick = CommitPick {
        selected: 0,
        tree: true,
        collapsed: HashSet::new(),
        rows: None,
        chevrons: crate::fold_motion::FlipTweens::new(mt_ui::motion::CHEVRON_ROTATE),
    };
    let first = pick.rows(&files);
    assert_eq!(*first, modal_rows(&files, true, &HashSet::new()));
    pick.selected = 7;
    assert!(Rc::ptr_eq(&first, &pick.rows(&files)), "换选中不重算");

    pick.toggle_dir("src/m2");
    let folded = pick.rows(&files);
    assert!(!Rc::ptr_eq(&first, &folded));
    assert_eq!(*folded, modal_rows(&files, true, &pick.collapsed));
    assert!(folded.len() < first.len());
    pick.toggle_dir("src/m2");
    assert_eq!(*pick.rows(&files), *first, "再点一下展开回来");

    pick.set_tree(false);
    assert_eq!(
        *pick.rows(&files),
        modal_rows(&files, false, &pick.collapsed)
    );
    assert_eq!(pick.rows(&files).len(), files.len());
}

/// 左栏 ↑ / ↓ 换文件:按眼下看得见的行序走,目录行跳过、收起的目录里的不算,到头停住;
/// 选中的文件藏在收起的目录里时 ↓ 落到第一个、↑ 落到最后一个。返回(文件下标, 行号)。
#[test]
fn 左栏上下键按可见行序换文件() {
    let commit = |path: &str| {
        ModalFile::from_commit(
            "abc",
            CommitFileInfo {
                path: path.into(),
                status: "modified".into(),
                old_path: None,
            },
        )
    };
    let files = [
        commit("z.md"),
        commit("src/ui/a.rs"),
        commit("src/ui/b.rs"),
        commit("docs/c.md"),
    ];
    // 树形:docs(0)、c.md(1)、src/ui(2)、a.rs(3)、b.rs(4)、z.md(5)
    let tree = modal_rows(&files, true, &HashSet::new());
    assert_eq!(step_file(&tree, 3, 1), Some((1, 3)), "跳过 src/ui 目录行");
    assert_eq!(step_file(&tree, 1, -1), Some((3, 1)), "往回也跳过目录行");
    assert_eq!(step_file(&tree, 2, 1), Some((0, 5)));
    assert_eq!(step_file(&tree, 0, 1), None, "最后一个:到头不绕回");
    assert_eq!(step_file(&tree, 3, -1), None, "第一个:到头不绕回");

    // 收起 src/ui:里面的 a.rs / b.rs 不算
    let folded = modal_rows(&files, true, &HashSet::from(["src/ui".to_string()]));
    assert_eq!(step_file(&folded, 3, 1), Some((0, 3)));
    assert_eq!(step_file(&folded, 0, -1), Some((3, 1)));
    // 选中的文件就在收起的目录里:↓ 落到第一个,↑ 落到最后一个
    assert_eq!(step_file(&folded, 1, 1), Some((3, 1)));
    assert_eq!(step_file(&folded, 1, -1), Some((0, 3)));

    // 平铺:就是文件序
    let flat = modal_rows(&files, false, &HashSet::new());
    assert_eq!(step_file(&flat, 0, 1), Some((1, 1)));
    assert_eq!(step_file(&flat, 3, -1), Some((2, 2)));
    // 一个文件行都没有(全收起 / 空提交)
    let all_folded = modal_rows(
        &files,
        true,
        &HashSet::from(["src/ui".to_string(), "docs".to_string()]),
    );
    assert_eq!(step_file(&all_folded[..2], 1, 1), None);
    assert_eq!(step_file(&[], 0, 1), None);
}

/// 没有改动块时的空态:纯重命名单说一句,其余照旧「没有更多改动」。
#[test]
fn 纯重命名的空态文案() {
    let mut file = staged(&["new.ts"]).remove(0);
    assert_eq!(empty_text(&file), t("gitCommit", "noMoreChanges"));
    file.old_path = Some("old.ts".into());
    assert_eq!(empty_text(&file), t("gitCommit", "renameOnly"));
    // 重命名前后同名(不该出现,防御):不当重命名说
    file.old_path = Some("new.ts".into());
    assert_eq!(empty_text(&file), t("gitCommit", "noMoreChanges"));
}

fn line_with(kind: &str, content: &str) -> DiffLine {
    DiffLine {
        kind: kind.to_string(),
        content: content.to_string(),
        old_lineno: Some(1),
        new_lineno: Some(1),
    }
}

/// 只改了一个字符,就只点出那一个字符 —— 这是词级高亮存在的全部理由。
#[test]
fn 词级高亮_只点出改了的片段() {
    let (del, add) = intra_line_marks("let x = 1;", "let x = 2;").expect("应当细分");
    assert_eq!(del, vec![8..9]);
    assert_eq!(add, vec![8..9]);
    assert_eq!(&"let x = 1;"[del[0].clone()], "1");
    assert_eq!(&"let x = 2;"[add[0].clone()], "2");
}

/// 相邻的变动词元并成一段,不留缝。
#[test]
fn 词级高亮_相邻片段合并() {
    let (del, _) = intra_line_marks("foo(bar)", "foo(baz, qux)").expect("应当细分");
    assert_eq!(del.len(), 1, "`bar` 是一整段,不该拆成几块");
}

/// 整行都变了就不细分 —— 全高亮 = 没高亮,还不如整行一个底色干净。
#[test]
fn 词级高亮_改动过多时放弃() {
    assert!(intra_line_marks("aaaa bbbb", "cccc dddd").is_none());
    assert!(intra_line_marks("same", "same").is_none(), "两行相同");
    assert!(intra_line_marks("", "x").is_none(), "空行");
    let long = "x".repeat(INTRA_LINE_MAX_BYTES + 1);
    assert!(intra_line_marks(&long, "x").is_none(), "超长行");
}

/// CJK 按 `Other` 逐字切:改一个字不该把整句涂了。
#[test]
fn 词级高亮_中文逐字() {
    let (del, add) = intra_line_marks("这是一行中文", "这是两行中文").expect("应当细分");
    assert_eq!(del.len(), 1);
    assert_eq!(add.len(), 1);
    assert_eq!(&"这是一行中文"[del[0].clone()], "一");
    assert_eq!(&"这是两行中文"[add[0].clone()], "两");
}

/// 每个 hunk 前插一行 `@@` 头,跳转落点就是这些头的下标。
#[test]
fn 拍平_每个_hunk_一个头() {
    let a = hunk(vec![
        line_with("context", "a"),
        line_with("delete", "b"),
        line_with("add", "c"),
    ]);
    let b = hunk(vec![line_with("context", "d")]);
    let f = flatten(&[a, b]);

    assert_eq!(f.heads.len(), 2);
    // 头 + 3 行,头 + 1 行
    assert_eq!(f.inline_rows.len(), 6);
    assert_eq!(f.inline_jumps, vec![0, 4]);
    // 并排视图没有头行,两栏各自连续排:左栏 a b d,右栏 a c d;跳转落点是各块第一行的
    // 滚动位置
    assert_eq!(f.split.rows[LEFT].len(), 3);
    assert_eq!(f.split.rows[RIGHT].len(), 3);
    assert_eq!((f.split.hunk_pos(0), f.split.hunk_pos(1)), (0.0, 2.0));
    assert!(matches!(f.inline_rows[0], InlineRow::Head(0)));
    assert!(f.heads[0].inline.contains("@@"));
}

/// 配对成功的删/增行才做词级高亮,单侧的行不做。
#[test]
fn 拍平_只给配对行算词级高亮() {
    let h = hunk(vec![
        line_with("delete", "value = 1"),
        line_with("add", "value = 2"),
        line_with("add", "brand new"),
    ]);
    let f = flatten(&[h]);
    assert!(!f.marks[0].is_empty(), "配对的 delete 行应有高亮");
    assert!(!f.marks[1].is_empty(), "配对的 add 行应有高亮");
    assert!(f.marks[2].is_empty(), "没配上的 add 行整行涂色即可");
}

/// 词级高亮按整块算:中间插了一行时,「let c = 3;」不再配给插进来的那行、点出 c/b
/// 两处假改动,只点出真正改了的「3」;插进来的那行整行一个底色。
#[test]
fn 拍平_词级高亮按块不因插行错位() {
    let h = hunk(vec![
        line_with("delete", "let a = 1;"),
        line_with("delete", "let c = 3;"),
        line_with("add", "let a = 10;"),
        line_with("add", "let b = 2;"),
        line_with("add", "let c = 30;"),
    ]);
    let f = flatten(&[h]);
    assert_eq!(f.marks[0], vec![8..9]);
    assert_eq!(f.marks[1], vec![8..9]);
    // 逐对配对的老算法在这里会点出两处
    let (old, _) = intra_line_marks("let c = 3;", "let b = 2;").unwrap();
    assert_eq!(old.len(), 2);
}

/// 超大块退回逐对配对:照样点出每对里改了的那个数,不会整块放弃。
#[test]
fn 拍平_超大块退回逐对配对() {
    let n = 300;
    let mut lines: Vec<DiffLine> = (0..n)
        .map(|i| line_with("delete", &format!("value = {i};")))
        .collect();
    lines.extend((0..n).map(|i| line_with("add", &format!("value = {i}x;"))));
    let f = flatten(&[hunk(lines)]);
    // 「value = 0;」→「value = 0x;」:旧侧点出 `0`,新侧点出 `0x`
    assert_eq!(f.marks[0], vec![8..9]);
    assert_eq!(f.marks[n], vec![8..10]);
}

/// uniform_list 只量一行来定内容宽,量宽下标必须指向**最宽**那一行。
#[test]
fn 拍平_量宽指向最宽的行() {
    let h = hunk(vec![
        line_with("context", "short"),
        line_with("context", "a much much longer line"),
    ]);
    let f = flatten(&[h]);
    // 行 0 是 hunk 头(钉在左缘,不参与量宽),行 1/2 是两条 context —— 最宽的是第 2 行
    assert_eq!(f.widest_inline, 2);
    assert!(matches!(f.inline_rows[2], InlineRow::Line(1)));
    // 并排两栏没有头行:最宽的是各栏第 1 行
    assert_eq!(f.split.widest, [1, 1]);
}

/// `\t` 在 gpui 文本里零宽,拍平时按制表位展开;词级高亮在展开后的文本上算。
#[test]
fn 拍平_tab缩进按制表位展开() {
    let h = hunk(vec![
        line_with("delete", "\tvalue := 1"),
        line_with("add", "\tvalue := 2"),
        line_with("context", "a\tb"),
    ]);
    let f = flatten(&[h]);
    assert_eq!(f.lines[0].content, "    value := 1");
    assert_eq!(f.lines[1].content, "    value := 2");
    assert_eq!(f.lines[2].content, "a   b");
    // 高亮区间落在展开后的 `1` / `2` 上(下标 13),而不是原文里的 10
    assert_eq!(f.marks[0], vec![13..14]);
    assert_eq!(f.marks[1], vec![13..14]);
}

#[test]
fn 行宽_全角按两列() {
    assert_eq!(text_cols("abc"), 3);
    assert_eq!(text_cols("中文"), 4);
    assert_eq!(text_cols(""), 0);
}

/// hunk 头按 git 的写法:某一侧一行都没有时,起点写「插入点之前那一行」(文件开头
/// 为 0)—— `@@ -0,0 +1,2 @@`、`@@ -4,0 +5,2 @@`;两侧都有行时照旧。
#[test]
fn hunk_头空范围按_git_写法() {
    let head = |old_start, old_lines, new_start, new_lines| {
        hunk_head(&DiffHunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            lines: Vec::new(),
        })
    };
    // 新文件:后端记的是「插在旧侧第 1 行之前」
    let new_file = head(1, 0, 1, 2);
    assert_eq!(new_file.inline.as_ref(), "@@ -0,0 +1,2 @@");
    // 第 4 行后插两行(无上下文)、删光文件
    assert_eq!(head(5, 0, 5, 2).inline.as_ref(), "@@ -4,0 +5,2 @@");
    assert_eq!(head(1, 3, 1, 0).inline.as_ref(), "@@ -1,3 +0,0 @@");
    // 普通块不动
    assert_eq!(head(6, 7, 6, 8).inline.as_ref(), "@@ -6,7 +6,8 @@");
}

/// 跳转组的计数:跳过之后「第几处 / 共几处」,还没跳过只报总数(不再是「–/N」)。
#[test]
fn 跳转计数没跳过时只报总数() {
    assert_eq!(counter_text(Some(1), 3), "2/3");
    let total = counter_text(None, 3);
    assert!(total.contains('3') && !total.contains('/'), "{total}");
}

// ── 折叠的上下文 / 按行勾选 ─────────────────────────────

use mt_project::git::{HunkDirection, diff_two_texts, splice_hunks};

fn numbered(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("l{i}")).collect()
}

fn text(lines: &[String]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// 100 行的文件改了第 10、80 行:两块 + 三段折叠(首块之前 6 行、块间 63 行、末块之后 17 行)。
fn two_far_changes() -> GitDiffResult {
    let old = numbered(100);
    let mut new = old.clone();
    new[9] = "TEN".into();
    new[79] = "EIGHTY".into();
    diff_two_texts(text(&old), text(&new))
}

fn fold_lens(f: &Flat) -> Vec<u32> {
    f.folds.iter().map(|fold| fold.len).collect()
}

/// 拍平时块与块之间没改动的行折成分隔行;跳转落点只落在 hunk 头上;分隔行两个视图各一行、
/// 记着自己在两个视图里的行号;首段只能向上展开、末段只能向下。
#[test]
fn 折叠_行模型与跳转() {
    let diff = two_far_changes();
    let (f, kept) = flatten_diff(&diff, &fold::Revealed::default());
    assert!(kept.is_empty());
    assert_eq!(fold_lens(&f), vec![6, 63, 17]);
    assert_eq!(f.inline_rows[0], InlineRow::Fold(0));
    assert_eq!(f.inline_rows[1], InlineRow::Head(0));
    assert_eq!(f.split.rows[LEFT][0], SideRow::Fold(0));
    for &j in &f.inline_jumps {
        assert!(matches!(f.inline_rows[j], InlineRow::Head(_)));
    }
    // 并排视图的落点:块一在分隔行之后(滚动位置 1),块二在块一的 7 行与块间分隔行之后
    assert_eq!((f.split.hunk_pos(0), f.split.hunk_pos(1)), (1.0, 9.0));
    for (ix, fold) in f.folds.iter().enumerate() {
        assert_eq!(f.inline_rows[fold.inline_row], InlineRow::Fold(ix));
        assert_eq!(f.split.rows[LEFT][fold.old_row], SideRow::Fold(ix));
        assert_eq!(f.split.rows[RIGHT][fold.new_row], SideRow::Fold(ix));
    }
    assert_eq!((f.folds[0].above, f.folds[0].below), (false, true));
    assert_eq!((f.folds[1].above, f.folds[1].below), (true, true));
    assert_eq!((f.folds[2].above, f.folds[2].below), (true, false));
    // 折起来的行不进 lines:只有块里的行
    assert_eq!(f.lines.len(), f.hunk_lines);
    // 没有两侧全文的老口径(`flatten`)不折叠
    assert!(flatten(&diff.hunks).folds.is_empty());
}

/// 向下展开露出紧挨上一块的 20 行、向上展开露出紧挨下一块的 20 行,剩下的仍折着;
/// 全部展开之后分隔行消失,露出来的行号连续、两栏同一行。
#[test]
fn 折叠_分段展开与全部展开() {
    let diff = two_far_changes();
    let (mut f, mut revealed) = flatten_diff(&diff, &fold::Revealed::default());
    let extra = f.inline_rows.len() - f.split.rows[RIGHT].len();
    let mid = f.folds[1];
    assert_eq!((mid.start, mid.len), (0, 63));

    let target = f
        .expand_fold(&diff.old_content, &mut revealed, 1, 0, fold::Expand::Down)
        .expect("有这条分隔行");
    assert_eq!(target.len, 63);
    assert_eq!(fold_lens(&f), vec![6, 43, 17]);
    // 块一占 7..=13:露出来的是 14..=33,紧跟在块一后面
    let first = f.hunk_lines;
    assert_eq!(f.lines[first].old_lineno, Some(14));
    assert_eq!(f.lines.last().unwrap().old_lineno, Some(33));
    assert_eq!(f.lines.len() - f.hunk_lines, 20);

    let mid = f.folds[1];
    assert_eq!((mid.start, mid.len), (20, 43));
    f.expand_fold(&diff.old_content, &mut revealed, 1, 20, fold::Expand::Up);
    assert_eq!(fold_lens(&f), vec![6, 23, 17]);
    // 向上展开的 20 行紧挨着块二(77..=83):57..=76
    let block_two = f.inline_jumps[1];
    let InlineRow::Line(ix) = f.inline_rows[block_two - 1] else {
        panic!("块二的头上面应是露出来的行");
    };
    assert_eq!(f.lines[ix].old_lineno, Some(76));

    f.expand_fold(&diff.old_content, &mut revealed, 1, 20, fold::Expand::All);
    assert_eq!(fold_lens(&f), vec![6, 17]);
    // 块一与块二之间的行连续(14..=76 全露出来了)、两栏同一行
    let shown = |side: usize| -> Vec<u32> {
        f.split.rows[side]
            .iter()
            .filter_map(|row| match row {
                SideRow::Line(ix) if *ix >= f.hunk_lines => f.lines[*ix].old_lineno,
                _ => None,
            })
            .collect()
    };
    assert_eq!(shown(LEFT), (14..=76).collect::<Vec<u32>>());
    assert_eq!(shown(RIGHT), shown(LEFT));
    // 行内视图比并排右栏多出来的只是 hunk 头与删除行:展开只加上下文行,两个视图加得
    // 一样多,差值不变
    assert_eq!(f.inline_rows.len() - f.split.rows[RIGHT].len(), extra);
    // 已经展开掉的分隔行再点(过期的回调):什么都不动
    assert!(
        f.expand_fold(&diff.old_content, &mut revealed, 1, 0, fold::Expand::All)
            .is_none()
    );
}

/// 展开出来的行只是上下文:没有块归属、不能打勾、整块操作也不把它算进去;F7 落点不变。
#[test]
fn 折叠_展开行不参与块操作() {
    let diff = two_far_changes();
    let (mut f, mut revealed) = flatten_diff(&diff, &fold::Revealed::default());
    let jumps_before = f.heads.len();
    for (gap, start) in [(0, 0), (1, 0), (2, 0)] {
        f.expand_fold(
            &diff.old_content,
            &mut revealed,
            gap,
            start,
            fold::Expand::All,
        );
    }
    assert!(f.folds.is_empty(), "全展开了");
    assert_eq!(f.inline_jumps.len(), jumps_before);
    for ix in f.hunk_lines..f.lines.len() {
        assert_eq!(f.lines[ix].kind, "context");
        assert_eq!(picks::hunk_of(&f.hunk_base, ix), None);
    }
    for hunk in 0..f.heads.len() {
        assert!(f.pickable_in(hunk).iter().all(|&ix| ix < f.hunk_lines));
    }
    let whole = f.applied_lines(&[HunkPick::whole(0), HunkPick::whole(1)]);
    assert_eq!(whole.len(), 4, "两块各一删一增");
    assert!(whole.iter().all(|&ix| ix < f.hunk_lines));
    // 勾选里混进块外的下标(不该出现):分组时跳过,不会交给后端
    let stray = BTreeSet::from([f.hunk_lines + 3]);
    assert!(picks::group_picks(&f.hunk_base, &stray, None).is_empty());
}

/// 量宽算上展开出来的行:折着时最宽的是块里的行,展开后换成那条超长的上下文行。
#[test]
fn 折叠_量宽算上展开的行() {
    let mut old = numbered(40);
    old[24] = "x".repeat(300);
    let mut new = old.clone();
    new[4] = "FIVE".into();
    let diff = diff_two_texts(text(&old), text(&new));
    let (mut f, mut revealed) = flatten_diff(&diff, &fold::Revealed::default());
    let cols = |f: &Flat, row: InlineRow| match row {
        InlineRow::Line(ix) => text_cols(&f.lines[ix].content),
        _ => 0,
    };
    assert!(cols(&f, f.inline_rows[f.widest_inline]) < 300);
    let tail = f.folds.last().copied().unwrap();
    f.expand_fold(
        &diff.old_content,
        &mut revealed,
        tail.gap,
        tail.start,
        fold::Expand::All,
    );
    assert_eq!(cols(&f, f.inline_rows[f.widest_inline]), 300);
    for side in [LEFT, RIGHT] {
        let SideRow::Line(ix) = f.split.rows[side][f.split.widest[side]] else {
            panic!("量宽要落在正文行上");
        };
        assert_eq!(text_cols(&f.lines[ix].content), 300);
    }
}

/// 原地重取后展开状态按行号 + 内容认回来:暂存了前面那块(旧侧行号整体挪了)照样按新侧
/// 行号认出;丢弃了那块(新侧挪了)按旧侧认出;认不出的收回去。
#[test]
fn 折叠_重取后按行号重新对齐() {
    let old = numbered(60);
    let mut new = old.clone();
    new.insert(5, "a".into());
    new.insert(6, "b".into());
    new[51] = "FIFTY".into(); // 旧侧第 50 行
    let diff = diff_two_texts(text(&old), text(&new));
    let (mut f, mut revealed) = flatten_diff(&diff, &fold::Revealed::default());
    // 块间那段:旧侧 9..、新侧 11..;向下露 20 行 = 旧侧 l9..l28
    let mid = f.folds.iter().copied().find(|fold| fold.gap == 1).unwrap();
    f.expand_fold(
        &diff.old_content,
        &mut revealed,
        1,
        mid.start,
        fold::Expand::Down,
    );
    let shown = |f: &Flat| -> Vec<SharedString> {
        (f.hunk_lines..f.lines.len())
            .map(|ix| f.lines[ix].content.clone())
            .collect()
    };
    let expect: Vec<SharedString> = (9..=28).map(|i| format!("l{i}").into()).collect();
    assert_eq!(shown(&f), expect);

    // 暂存了插入那块:暂存区多了 a、b,旧侧行号 +2,新侧不变
    let staged = {
        let mut lines = old.clone();
        lines.insert(5, "a".into());
        lines.insert(6, "b".into());
        text(&lines)
    };
    let after_stage = diff_two_texts(staged, text(&new));
    let (f2, kept) = flatten_diff(&after_stage, &revealed);
    assert_eq!(shown(&f2), expect, "按新侧行号认回来");
    assert!(!kept.is_empty());

    // 丢弃了插入那块:工作区没了 a、b,新侧行号 -2,旧侧不变
    let discarded = {
        let mut lines = old.clone();
        lines[49] = "FIFTY".into();
        text(&lines)
    };
    let after_discard = diff_two_texts(text(&old), discarded.clone());
    let (f3, _) = flatten_diff(&after_discard, &revealed);
    assert_eq!(shown(&f3), expect, "按旧侧行号认回来");

    // 行号没动、内容变了(外部改动把 l10..l29 两侧都改成了 m10..m29):对不上的收回去,
    // 只剩内容还对得上的 l9;重新认过的记忆里也只剩它
    let rewrite = |s: String| s.replace("\nl1", "\nm1").replace("\nl2", "\nm2");
    let elsewhere = diff_two_texts(rewrite(text(&old)), rewrite(discarded));
    let (f4, kept4) = flatten_diff(&elsewhere, &revealed);
    assert_eq!(shown(&f4), vec![SharedString::from("l9")]);
    assert!(kept4.contains(9, 9, "l9"));
    assert!(!kept4.contains(10, 10, "l10"));
}

/// 按行暂存的整条链路:勾了两块里各一行,只暂存第一块里勾的那行(块头按钮的口径)→ 后端
/// 拼接 → 重取;第二块里的勾选按内容认回来,落下去的那行不再勾着。
#[test]
fn 按行勾选_暂存后剩下的勾选认回来() {
    let old = numbered(30);
    let mut new = old.clone();
    new[4] = "X5".into();
    new[5] = "X6".into();
    new[19] = "X20".into();
    let diff = diff_two_texts(text(&old), text(&new));
    let (f, _) = flatten_diff(&diff, &fold::Revealed::default());
    let find = |f: &Flat, kind: &str, content: &str| {
        (0..f.hunk_lines)
            .find(|&ix| f.lines[ix].kind == kind && f.lines[ix].content.as_ref() == content)
            .unwrap()
    };
    let (x6, x20) = (find(&f, "add", "X6"), find(&f, "add", "X20"));
    let mut picked = BTreeSet::from([x6, x20]);

    // 第一块的按钮只拿第一块里勾的那一行
    let first = picks::group_picks(&f.hunk_base, &picked, Some(0));
    assert_eq!(first.len(), 1);
    let pick = &first[0];
    let lines = pick.lines.as_ref().unwrap();
    assert_eq!(lines.len(), 1);
    let staged = splice_hunks(
        &diff.old_content,
        &diff.new_content,
        &[(&diff.hunks[0], Some(lines))],
        HunkDirection::Apply,
        "\n",
    )
    .unwrap();
    assert!(
        staged.contains("l5\nl6\nX6\nl7"),
        "只搬进勾上的 X6:{staged}"
    );

    // 成功后:落下去的行拿掉、记进 skip,再按内容认回剩下的
    let applied = f.applied_lines(&first);
    picked.retain(|ix| !applied.contains(ix));
    let keys = picks::pick_keys(&f.pick_lines(), &picked, &applied);
    let after = diff_two_texts(staged, text(&new));
    let (f2, _) = flatten_diff(&after, &fold::Revealed::default());
    let remapped = picks::remap_picks(&f2.pick_lines(), &keys);
    assert_eq!(remapped, BTreeSet::from([find(&f2, "add", "X20")]));
    // 第一块剩下的增删行(没勾的 X5、两行删除)都还在,只是没勾
    assert!((0..f2.hunk_lines).any(|ix| f2.lines[ix].content.as_ref() == "X5"));
}

/// 勾选范围:行内视图块头的勾选框管整块的增删行,并排视图中间栏的勾选框只管这一处改动的;
/// 删除行的勾选框在左栏、新增行的在右栏(各在自己那一栏的行上)。
#[test]
fn 勾选范围_块头与每处改动() {
    let h = hunk(vec![
        line_with("context", "a"),
        line_with("delete", "b"),
        line_with("delete", "c"),
        line_with("add", "B"),
        line_with("context", "d"),
        line_with("add", "E"),
    ]);
    let f = flatten(&[h]);
    assert_eq!(f.pickable_in(0), vec![1, 2, 3, 5]);
    assert!(f.pickable_in(5).is_empty(), "块号越界");
    assert_eq!(f.change_lines(0), vec![1, 2, 3]);
    assert_eq!(f.change_lines(1), vec![5]);
    assert!(f.change_lines(9).is_empty());
    let on = |side: usize, ix: usize| f.split.rows[side].contains(&SideRow::Line(ix));
    assert!(on(LEFT, 1) && on(LEFT, 2) && !on(RIGHT, 1));
    assert!(on(RIGHT, 3) && on(RIGHT, 5) && !on(LEFT, 3));
    assert_eq!(Tick::of(0, 3), Tick::Off);
    assert_eq!(Tick::of(2, 3), Tick::Partial);
    assert_eq!(Tick::of(3, 3), Tick::On);
    // 按行操作的块内下标换回拍平下标
    assert_eq!(
        f.applied_lines(&[HunkPick::lines(0, [2usize].into())]),
        BTreeSet::from([2])
    );
}

/// 中间栏的箭头作用于这一处改动:没勾行时交给后端的是这一处的全部增删行(同一 hunk 里的
/// 另一处不动),勾了行时只给勾上的;确认框据此说「这一处」还是「勾选的行」。
#[test]
fn 每处改动的块操作只动这一处() {
    let diff = diff_two_texts("a\nb\nc\nd\ne\n".into(), "a\nB\nc\nd\nE\nF\n".into());
    assert_eq!(diff.hunks.len(), 1, "两处改动挨得近,同在一个 hunk 里");
    let mut s = DiffState::new(13.0);
    s.pickable = true;
    s.apply(Ok(diff));
    assert_eq!(s.flat.split.changes.len(), 2);
    let second = s.op_target(OpScope::Change(1)).expect("有这一处");
    assert_eq!(
        (second.land, second.lines, second.change),
        (None, Some(3), true),
        "块里还有另一处:操作后停在原地"
    );
    assert_eq!(second.busy, Busy::Change(1));
    let lines = second.picks[0].lines.as_ref().expect("按行交给后端");
    let expect: Vec<usize> = s
        .flat
        .change_lines(1)
        .iter()
        .map(|ix| ix - s.flat.hunk_base[0])
        .collect();
    assert_eq!(lines.iter().copied().collect::<Vec<_>>(), expect);
    // 这一处里勾了一行:只动那一行
    let picked = s.flat.change_lines(1)[0];
    s.toggle_pick(picked);
    let only = s.op_target(OpScope::Change(1)).unwrap();
    assert_eq!((only.lines, only.change), (Some(1), false));
    assert_eq!(s.picks_in_change(1), 1);
    assert_eq!(s.picks_in_change(0), 0);
    // 中间栏的勾选框:一下勾满这一处,再点一下全取消
    s.toggle_change_picks(0);
    assert_eq!(s.picks_in_change(0), 2);
    s.toggle_change_picks(0);
    assert_eq!(s.picks_in_change(0), 0);
    assert_eq!(s.picks_in_change(1), 1, "另一处的勾选不受影响");
}

/// 按钮回调记的改动 / 块的身份:同一份 diff 里认得出;重取之后同一个序号指着别处时对不上
/// (这次点击作罢),原来那一处换了序号也认得出是它。
#[test]
fn 按钮记的改动序号过期就不认() {
    let mut s = DiffState::new(13.0);
    s.pickable = true;
    s.apply(Ok(diff_two_texts(
        "a\nb\nc\nd\ne\n".into(),
        "a\nB\nc\nd\nE\nF\n".into(),
    )));
    let (first, second) = (s.flat.change_key(0), s.flat.change_key(1));
    assert!(first.is_some() && first != second);
    assert!(s.change_is(1, second) && !s.change_is(0, second));
    let head = s.flat.hunk_key(0).unwrap();
    assert!(s.hunk_is(0, &head));
    // 第 0 处暂存掉之后重取:原来的第 1 处成了第 0 处,块头也变了
    s.apply(Ok(diff_two_texts(
        "a\nB\nc\nd\ne\n".into(),
        "a\nB\nc\nd\nE\nF\n".into(),
    )));
    assert!(!s.change_is(1, second), "序号 1 已经没有了");
    assert!(!s.change_is(0, first), "序号 0 指着原来的第 1 处");
    assert!(s.change_is(0, second));
    assert!(!s.hunk_is(0, &head));
    assert!(!s.change_is(0, None));
}

/// 按一处改动操作后落在哪:块里还剩别的改动时停在原地(不跳回块头);动的是块里剩下的
/// 全部增删行(没勾行,或这一处全勾上了)时块整个落下去,按块的口径落到原位置上的下一块。
/// 行内视图的块头按钮照旧落到下一块。
#[test]
fn 按改动操作后停在原地或落到下一块() {
    let mut s = DiffState::new(13.0);
    s.pickable = true;
    s.apply(Ok(diff_two_texts(
        "a\nb\nc\nd\ne\n".into(),
        "a\nB\nc\nd\nE\nF\n".into(),
    )));
    assert_eq!(s.op_target(OpScope::Change(0)).unwrap().land, None);
    assert_eq!(s.op_target(OpScope::Hunk(0)).unwrap().land, Some(0));

    let mut new = numbered(40);
    new[4] = "FIVE".into();
    new[30] = "THIRTY".into();
    s.apply(Ok(diff_two_texts(text(&numbered(40)), text(&new))));
    assert_eq!((s.flat.heads.len(), s.flat.split.changes.len()), (2, 2));
    assert_eq!(s.op_target(OpScope::Change(1)).unwrap().land, Some(1));
    s.toggle_change_picks(1);
    assert_eq!(s.op_target(OpScope::Change(1)).unwrap().land, Some(1));
    // 只勾了一部分:块还在
    s.toggle_pick(s.flat.change_lines(1)[0]);
    assert_eq!(s.op_target(OpScope::Change(1)).unwrap().land, None);
}

/// 按中间栏的箭头把 `cur` 的第 `change` 处改动往 `goal` 那边搬一步(暂存 = `Apply`,丢弃 =
/// `Revert`),返回写下去的那一侧。
fn step_change(
    s: &mut DiffState,
    (old, new): (&str, &str),
    change: usize,
    dir: HunkDirection,
) -> String {
    s.apply(Ok(diff_two_texts(old.into(), new.into())));
    let target = s.op_target(OpScope::Change(change)).expect("有这一处");
    let picks: Vec<_> = target
        .picks
        .iter()
        .map(|pick| (&target.diff.hunks[pick.hunk], pick.lines.as_ref()))
        .collect();
    splice_hunks(old, new, &picks, dir, "\n").unwrap()
}

/// 同一 hunk 里两处改动、后一处碰到文件末尾且末尾换行不同(这处差异 diff 里看不见):不管先
/// 点哪一处,两处都暂存 / 丢弃之后两侧逐字节相同 —— 末尾换行不会被孤零零留下、文件也就不会
/// 「没有改动却一直挂在未暂存里」。
#[test]
fn 按改动操作不漏掉末尾换行() {
    use HunkDirection::{Apply, Revert};
    let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let new = "l1\nl2\nL3\nl4\nl5\nl6\nl7\nl8\nl9\nL10";
    assert_eq!(diff_two_texts(old.into(), new.into()).hunks.len(), 1);
    let mut s = DiffState::new(13.0);
    s.pickable = true;
    for first in [1, 0] {
        // 暂存:先动 `first` 那一处,剩下的那处这时是第 0 处
        let staged = step_change(&mut s, (old, new), first, Apply);
        let staged = step_change(&mut s, (&staged, new), 0, Apply);
        assert_eq!(staged, new, "先暂存第 {first} 处");
        // 丢弃
        let kept = step_change(&mut s, (old, new), first, Revert);
        let kept = step_change(&mut s, (old, &kept), 0, Revert);
        assert_eq!(kept, old, "先丢弃第 {first} 处");
    }
}

/// 块头按钮上的字:没勾 = 整块,勾了 = 「所选 N 行」;按侧给哪几种操作。
#[test]
fn 块操作按钮的文案与种类() {
    assert_eq!(
        op_label(HunkOp::Stage, 0).as_ref(),
        t("gitCommit", "stageHunk")
    );
    let lines = op_label(HunkOp::Discard, 3);
    assert!(lines.contains('3'), "{lines}");
    assert_ne!(lines.as_ref(), t("gitCommit", "discardHunk"));
    assert_eq!(
        side_ops(DiffSide::Unstaged),
        &[HunkOp::Stage, HunkOp::Discard]
    );
    assert_eq!(side_ops(DiffSide::Staged), &[HunkOp::Unstage]);
    assert!(side_ops(DiffSide::Working).is_empty());
}

/// 静默重取(页签切回来 / 刷新 / 仓库变了):拿回来的与屏幕上一样(同样的两侧全文、同一句
/// 错误)就什么都不动 —— 选区、跳转计数、滚动位置都留着;内容或标记变了、正文与错误互换、
/// 屏幕上还什么都没有,才落地。
#[test]
fn 静默重取没变就不动() {
    let a = diff_two_texts("x\ny\n".into(), "x\nz\n".into());
    let same = diff_two_texts("x\ny\n".into(), "x\nz\n".into());
    let other = diff_two_texts("x\ny\n".into(), "x\nw\n".into());
    assert!(same_diff(&a, &same));
    assert!(!same_diff(&a, &other));
    let mut binary = same.clone();
    binary.is_binary = true;
    assert!(!same_diff(&a, &binary), "标记变了也算变");

    assert!(refresh_unchanged(Some(&a), None, &Ok(same)));
    assert!(!refresh_unchanged(Some(&a), None, &Ok(other)));
    let err = || anyhow::anyhow!("文件不存在");
    assert!(refresh_unchanged(None, Some("文件不存在"), &Err(err())));
    assert!(!refresh_unchanged(None, Some("权限不足"), &Err(err())));
    assert!(!refresh_unchanged(Some(&a), None, &Err(err())));
    assert!(!refresh_unchanged(None, Some("文件不存在"), &Ok(a.clone())));
    assert!(!refresh_unchanged(None, None, &Ok(a)), "还在加载:照常落地");
}
