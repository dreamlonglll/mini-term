use super::super::edit_script;
use super::*;

/// 先跑父模块的 LCS,再滑:返回滑完之后的编辑序列。
fn compacted(old: &str, new: &str) -> Vec<Op> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    compact_ops(&edit_script(&old, &new), &old, &new)
}

/// 编辑序列里的改动块,按 git `-U0` 的口径写成 `(种类, 起始行, 行数)`(1 起):
/// 新增块报新侧的行,删除块报旧侧的行。
fn blocks(ops: &[Op]) -> Vec<(char, usize, usize)> {
    let mut out: Vec<(char, usize, usize)> = Vec::new();
    let mut prev: Option<Op> = None;
    for &(kind, oi, ni) in ops {
        let line = if kind == '+' { ni } else { oi };
        let joined = prev.is_some_and(|(pk, po, pn)| {
            pk == kind
                && if kind == '+' {
                    pn + 1 == ni
                } else {
                    po + 1 == oi
                }
        });
        match out.last_mut() {
            Some(last) if joined && kind != '=' => last.2 += 1,
            _ if kind != '=' => out.push((kind, line + 1, 1)),
            _ => {}
        }
        prev = Some((kind, oi, ni));
    }
    out
}

/// E2E 那一例:文件末尾追加一个函数,而文件前面另有改动(LCS 剥不掉整段前缀)。
/// 滑之前新增块是 4–7 行、把原来的 `}` 算了进去;git 给的是 5–8(`@@ -4,0 +5,4 @@`)。
#[test]
fn 尾部追加函数_新增块落在_git_的位置() {
    let old = "x\nfunction a() {\n  return 1;\n}\n";
    let new = "X\nfunction a() {\n  return 1;\n}\n\nfunction b() {\n  return 2;\n}\n";
    assert_eq!(
        blocks(&compacted(old, new)),
        vec![('-', 1, 1), ('+', 1, 1), ('+', 5, 4)]
    );
}

/// 中间插入一行重复行:git 记在下面那一行(`@@ -2,0 +3 @@`)。
#[test]
fn 重复行插入与删除都滑到最下() {
    assert_eq!(
        blocks(&compacted("a\nb\nc\n", "a\nb\nb\nc\n")),
        vec![('+', 3, 1)]
    );
    // 删除同理:`@@ -3 +2,0 @@`
    assert_eq!(
        blocks(&compacted("a\nb\nb\nc\n", "a\nb\nc\n")),
        vec![('-', 3, 1)]
    );
}

/// 缩进启发式:以下期望值逐条取自 `git diff --no-index -U0`(git 默认开着
/// `diff.indentHeuristic`)。最后一例关掉启发式时 git 给的是 5–6,开着是 4–5 ——
/// 只做「滑到最下」对不上。
#[test]
fn 缩进启发式与_git_一致() {
    // 两段空行之间插一段:`@@ -2,0 +3,2 @@`
    assert_eq!(
        blocks(&compacted("a\n\nb\n", "a\n\nc\n\nb\n")),
        vec![('+', 3, 2)]
    );
    // 两个函数之间插一个函数,块带着尾随空行:`@@ -3,0 +4,3 @@`
    assert_eq!(
        blocks(&compacted(
            "fn a() {\n}\n\nfn c() {\n}\n",
            "fn a() {\n}\n\nfn b() {\n}\n\nfn c() {\n}\n"
        )),
        vec![('+', 4, 3)]
    );
    // 同级的 if 块之间插一个:`@@ -3,0 +4,3 @@`
    assert_eq!(
        blocks(&compacted(
            "if x {\n    a();\n}\nif y {\n    b();\n}\n",
            "if x {\n    a();\n}\nif z {\n    c();\n}\nif y {\n    b();\n}\n"
        )),
        vec![('+', 4, 3)]
    );
    // 关掉启发式是 `+5,2`,开着是 `+4,2`
    assert_eq!(
        blocks(&compacted(
            "a\n    b\n    c\n}\nd\n",
            "a\n    b\n    c\n}\n    c\n}\nd\n"
        )),
        vec![('+', 4, 2)]
    );
}

/// 一删一增配成「修改」的块不被拆开(对面有改动的位置优先)。
#[test]
fn 修改块不被滑散() {
    assert_eq!(
        blocks(&compacted("a\nb\nc\n", "a\nx\nc\n")),
        vec![('-', 2, 1), ('+', 2, 1)]
    );
    let ops = compacted("a\nb\nc\n", "a\nx\nc\n");
    assert_eq!(
        ops,
        vec![('=', 0, 0), ('-', 1, 1), ('+', 2, 1), ('=', 2, 2)]
    );
}

/// 滑动不改变编辑的合法性与长度:按序重放能还原两侧、相等行两两相等、相等行数不变。
/// 用确定性伪随机扫一批带空行与缩进的小输入。
#[test]
fn 滑完仍是合法的最短编辑() {
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let alphabet = ["a", "b", "", "  b", "}", "\tc"];
    for _ in 0..500 {
        let len_a = (next() % 10) as usize;
        let len_b = (next() % 10) as usize;
        let a: Vec<&str> = (0..len_a)
            .map(|_| alphabet[(next() % 6) as usize])
            .collect();
        let b: Vec<&str> = (0..len_b)
            .map(|_| alphabet[(next() % 6) as usize])
            .collect();
        let raw = edit_script(&a, &b);
        let ops = compact_ops(&raw, &a, &b);
        let equal = |ops: &[Op]| ops.iter().filter(|o| o.0 == '=').count();
        assert_eq!(equal(&ops), equal(&raw), "{a:?} → {b:?}: {ops:?}");
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
    }
}

#[test]
fn 行首缩进按制表位算_空白行为负一() {
    assert_eq!(get_indent("abc"), 0);
    assert_eq!(get_indent("    x"), 4);
    assert_eq!(get_indent("\tx"), 8);
    assert_eq!(get_indent("  \tx"), 8);
    assert_eq!(get_indent(""), -1);
    assert_eq!(get_indent("   "), -1);
}
