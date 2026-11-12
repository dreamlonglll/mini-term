//! 按行暂存的勾选:单文件 diff 行号栏里给增 / 删行打的勾。
//!
//! # 勾选记拍平行下标,重取后按内容认
//!
//! 勾选记成 `Flat::lines` 的下标(只落在块里的增删行上;展开出来的上下文行没有勾选框)。
//! 块操作之后页签原地重取 diff,行全换了一套:按 [`PickKey`](增 / 删 + 原文 + 这段内容在
//! 同类行里第几次出现)在新的 diff 里找回还对得上的那些,对不上的丢掉。找回的勾选只会落在
//! 新 diff 里**看得见**的行上 —— 勾选框照样画着打了勾,用户点按钮之前看得到;后端的防陈旧
//! (`StaleDiff`)照旧把关「操作用的这份 diff 就是磁盘上的现状」。
//!
//! # 一次操作多块
//!
//! 跨块的「暂存所选行」把勾选按块分组成一组 [`HunkPick`](块号严格递增),一次交给多块后端
//! (`mt_project::git::{stage_hunks, unstage_hunks, discard_hunks}`):逐块落盘的话第一块写
//! 下去,后面几块对应的那份 diff 就过期了(见 `mt_project::git::hunk` 模块注释)。

use std::collections::{BTreeSet, HashMap, HashSet};

use mt_project::git::{HunkPick, LinePick};

/// 勾选的内容键:增 / 删 + 原文 + 这段内容在同类行里是第几次出现(0 起,整份 diff 里数)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct PickKey {
    add: bool,
    text: String,
    nth: usize,
}

/// 能打勾的行种类:`Some(true)` = 新增,`Some(false)` = 删除,其余(上下文 / 认不出)不能勾。
pub(super) fn pickable_kind(kind: &str) -> Option<bool> {
    match kind {
        "add" => Some(true),
        "delete" => Some(false),
        _ => None,
    }
}

/// 逐行的内容键(与 `lines` 等长;不能勾的行、`skip` 里的行是 `None`)。`lines` 是块里各行的
/// `(kind, 原文)`,按拍平顺序。`skip` 里的行不参与「第几次出现」的计数。
fn keys_of_lines(lines: &[(&str, &str)], skip: &BTreeSet<usize>) -> Vec<Option<PickKey>> {
    let mut seen: HashMap<(bool, &str), usize> = HashMap::new();
    lines
        .iter()
        .enumerate()
        .map(|(ix, &(kind, text))| {
            let add = pickable_kind(kind)?;
            if skip.contains(&ix) {
                return None;
            }
            let nth = seen.entry((add, text)).or_insert(0);
            let key = PickKey {
                add,
                text: text.to_string(),
                nth: *nth,
            };
            *nth += 1;
            Some(key)
        })
        .collect()
}

/// 勾上的这几行的内容键(重取之前调)。
///
/// `skip` 是刚才那次块操作已经落下去的行:重取之后它们从 diff 里消失了,数「第几次出现」
/// 时先把它们剔掉,剩下的勾选才与新 diff 里的计数对得上 —— 否则前面同内容的一行被暂存走,
/// 后面的编号整体前移,记着「第 2 个 `}`」的勾就会落到原先第 3 个 `}` 上。
pub(super) fn pick_keys(
    lines: &[(&str, &str)],
    picks: &BTreeSet<usize>,
    skip: &BTreeSet<usize>,
) -> HashSet<PickKey> {
    if picks.is_empty() {
        return HashSet::new();
    }
    keys_of_lines(lines, skip)
        .into_iter()
        .enumerate()
        .filter(|(ix, _)| picks.contains(ix))
        .filter_map(|(_, key)| key)
        .collect()
}

/// 重取之后:新 diff 里内容键还对得上的行(下标进新的 `lines`)。
pub(super) fn remap_picks(lines: &[(&str, &str)], keys: &HashSet<PickKey>) -> BTreeSet<usize> {
    if keys.is_empty() {
        return BTreeSet::new();
    }
    keys_of_lines(lines, &BTreeSet::new())
        .into_iter()
        .enumerate()
        .filter(|(_, key)| key.as_ref().is_some_and(|key| keys.contains(key)))
        .map(|(ix, _)| ix)
        .collect()
}

/// 拍平行下标属于第几块。`hunk_base[k]` 是第 k 块的第一行,末尾多一个 = 块里的总行数;
/// 落在块外(展开出来的上下文行)是 `None`。
pub(super) fn hunk_of(hunk_base: &[usize], ix: usize) -> Option<usize> {
    let end = *hunk_base.last()?;
    if ix >= end {
        return None;
    }
    Some(hunk_base.partition_point(|&base| base <= ix) - 1)
}

/// 勾选按块分组成后端要的 [`HunkPick`](块号递增,行号是块内下标)。`only = Some(k)` 只要
/// 第 k 块的。
pub(super) fn group_picks(
    hunk_base: &[usize],
    picks: &BTreeSet<usize>,
    only: Option<usize>,
) -> Vec<HunkPick> {
    let mut out: Vec<HunkPick> = Vec::new();
    for &ix in picks {
        let Some(hunk) = hunk_of(hunk_base, ix) else {
            continue;
        };
        if only.is_some_and(|only| only != hunk) {
            continue;
        }
        let line = ix - hunk_base[hunk];
        match out.last_mut() {
            Some(HunkPick {
                hunk: last,
                lines: Some(set),
            }) if *last == hunk => {
                set.insert(line);
            }
            _ => out.push(HunkPick::lines(hunk, LinePick::from([line]))),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINES: &[(&str, &str)] = &[
        ("context", "fn a() {"),
        ("delete", "    x"),
        ("add", "    y"),
        ("add", "}"),
        ("context", ""),
        ("delete", "}"),
        ("add", "}"),
    ];

    /// 内容键:同类同内容的行按出现顺序编号;上下文行没有键。
    #[test]
    fn 内容键按同类出现次序编号() {
        let keys = keys_of_lines(LINES, &BTreeSet::new());
        assert!(keys[0].is_none() && keys[4].is_none());
        let key = |ix: usize| keys[ix].clone().unwrap();
        assert_eq!(key(3).nth, 0);
        assert_eq!(key(6).nth, 1, "第二个新增的右括号");
        assert_eq!(key(5).nth, 0, "删除的右括号与新增的分开数");
    }

    /// 重取后按内容找回:前面插进一块改动、行号全变了照样认得出;内容变了 / 对应的那一次
    /// 出现没了就丢掉;上下文行即便内容一样也不会被勾上。
    #[test]
    fn 重取后按内容找回勾选() {
        let picks = BTreeSet::from([2, 6]);
        let keys = pick_keys(LINES, &picks, &BTreeSet::new());
        assert_eq!(keys.len(), 2);

        let shifted: &[(&str, &str)] = &[
            ("add", "new"),
            ("context", "fn a() {"),
            ("delete", "    x"),
            ("add", "    y"),
            ("add", "}"),
            ("context", ""),
            ("delete", "}"),
            ("add", "}"),
            ("context", "    y"),
        ];
        assert_eq!(remap_picks(shifted, &keys), BTreeSet::from([3, 7]));

        // 第一个新增的 `}` 已被暂存走:剩下那个现在是第 0 次出现,对不上就丢掉 ——
        // 宁可少勾,不猜
        let after: &[(&str, &str)] = &[("delete", "    x"), ("add", "    y"), ("add", "}")];
        assert_eq!(remap_picks(after, &keys), BTreeSet::from([1]));
        assert!(remap_picks(LINES, &HashSet::new()).is_empty());
        assert!(pick_keys(LINES, &BTreeSet::new(), &BTreeSet::new()).is_empty());
    }

    /// 块操作落下去的行(`skip`)不参与计数:前面那个 `}` 被暂存走之后,勾着的第二个 `}`
    /// 在新 diff 里是第一个,照样认得回来;没勾的那个不会被顶替勾上。
    #[test]
    fn 落下去的行不参与计数() {
        let lines: &[(&str, &str)] = &[("add", "}"), ("add", "x"), ("add", "}"), ("add", "}")];
        // 勾了第 2 个 `}`(下标 2),暂存走的是第 1 个(下标 0)
        let keys = pick_keys(lines, &BTreeSet::from([2]), &BTreeSet::from([0]));
        let after: &[(&str, &str)] = &[("add", "x"), ("add", "}"), ("add", "}")];
        assert_eq!(remap_picks(after, &keys), BTreeSet::from([1]));
        // 不剔掉的话会错落到原先第 3 个 `}` 上
        let naive = pick_keys(lines, &BTreeSet::from([2]), &BTreeSet::new());
        assert_eq!(remap_picks(after, &naive), BTreeSet::from([2]));
        // 落下去的行本身即便还勾着也不再记
        assert!(pick_keys(lines, &BTreeSet::from([0]), &BTreeSet::from([0])).is_empty());
    }

    /// 拍平行 → 第几块;块外(展开的上下文行)不属于任何块。
    #[test]
    fn 下标归块() {
        let base = [0, 4, 9];
        assert_eq!(hunk_of(&base, 0), Some(0));
        assert_eq!(hunk_of(&base, 3), Some(0));
        assert_eq!(hunk_of(&base, 4), Some(1));
        assert_eq!(hunk_of(&base, 8), Some(1));
        assert_eq!(hunk_of(&base, 9), None, "展开出来的上下文行");
        assert_eq!(hunk_of(&[], 0), None);
    }

    /// 分组:按块递增,行号换成块内下标;`only` 只取一块;块外的下标(不该出现)跳过。
    #[test]
    fn 勾选按块分组() {
        let base = [0, 4, 9];
        let picks = BTreeSet::from([1, 2, 5, 8, 12]);
        assert_eq!(
            group_picks(&base, &picks, None),
            vec![
                HunkPick::lines(0, LinePick::from([1, 2])),
                HunkPick::lines(1, LinePick::from([1, 4])),
            ]
        );
        assert_eq!(
            group_picks(&base, &picks, Some(1)),
            vec![HunkPick::lines(1, LinePick::from([1, 4]))]
        );
        assert!(group_picks(&base, &picks, Some(5)).is_empty());
        assert!(group_picks(&base, &BTreeSet::new(), None).is_empty());
    }
}
