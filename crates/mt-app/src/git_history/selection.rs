//! 历史区的多选模型(issue #84,对标 IDEA Log —— 即 Swing 列表的选择口径)。
//!
//! | 操作 | 结果 |
//! |---|---|
//! | 单击 | 单选(清掉其它) |
//! | Ctrl/Cmd 单击 | 切换该行选没选中 |
//! | Shift 单击 | 锚点到该行连续选中(替换原选择) |
//! | Ctrl/Cmd+Shift 单击 | 锚点到该行这一段**并入**现有选择 |
//! | ↑ / ↓ | 从当前行单选移动 |
//! | Shift+↑ / ↓ | 当前行移动、锚点不动,选择 = 锚点..当前行(扩展 / 收缩) |
//!
//! 两个位置:**锚点**(anchor)是 Shift 连选的起点,单击 / Ctrl 单击时落在点的那一行;
//! **当前行**(lead)是最后一次操作到的行,键盘移动、滚动跟随、触底预取都按它。
//!
//! 按 hash 记(分页 / 刷新之后照样认得出),行序由调用方给的提交列表定。

use std::collections::HashSet;

use mt_project::git::GitCommitInfo;

/// 单击时的修饰键(`toggle` = Ctrl/Cmd,`range` = Shift)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ClickMods {
    pub(super) toggle: bool,
    pub(super) range: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct CommitSelection {
    selected: HashSet<String>,
    anchor: Option<String>,
    lead: Option<String>,
}

fn index_of(commits: &[GitCommitInfo], hash: &str) -> Option<usize> {
    commits.iter().position(|c| c.hash == hash)
}

impl CommitSelection {
    pub(super) fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.selected.len()
    }

    pub(super) fn contains(&self, hash: &str) -> bool {
        self.selected.contains(hash)
    }

    /// 只选中了一个时是它。
    pub(super) fn single(&self) -> Option<&str> {
        match self.selected.len() {
            1 => self.selected.iter().next().map(String::as_str),
            _ => None,
        }
    }

    /// 当前行(可能已被 Ctrl 单击取消选中,仍是键盘移动的起点)。
    #[cfg(test)]
    pub(super) fn lead(&self) -> Option<&str> {
        self.lead.as_deref()
    }

    #[cfg(test)]
    pub(super) fn anchor(&self) -> Option<&str> {
        self.anchor.as_deref()
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// 单选这一个(锚点、当前行都落在它上面)。
    pub(super) fn select_only(&mut self, hash: &str) {
        self.selected.clear();
        self.selected.insert(hash.to_string());
        self.anchor = Some(hash.to_string());
        self.lead = Some(hash.to_string());
    }

    /// 选中的提交,按列表顺序。
    pub(super) fn ordered<'a>(&self, commits: &'a [GitCommitInfo]) -> Vec<&'a GitCommitInfo> {
        commits
            .iter()
            .filter(|c| self.selected.contains(&c.hash))
            .collect()
    }

    /// 锚点在列表里的位置;没有锚点(或它已不在列表里)时用 `fallback`。
    fn anchor_index(&self, commits: &[GitCommitInfo], fallback: usize) -> usize {
        self.anchor
            .as_deref()
            .and_then(|hash| index_of(commits, hash))
            .unwrap_or(fallback)
    }

    /// 选中 `[a, b]` 这一段(两端不分先后)。`extend` 时并入现有选择,否则替换。
    fn select_span(&mut self, commits: &[GitCommitInfo], a: usize, b: usize, extend: bool) {
        let (lo, hi) = (a.min(b), a.max(b));
        if !extend {
            self.selected.clear();
        }
        self.selected
            .extend(commits[lo..=hi].iter().map(|c| c.hash.clone()));
    }

    /// 单击第 `index` 行(修饰键口径见模块注释)。
    pub(super) fn click(&mut self, commits: &[GitCommitInfo], index: usize, mods: ClickMods) {
        let Some(hash) = commits.get(index).map(|c| c.hash.clone()) else {
            return;
        };
        if mods.range {
            let anchor = self.anchor_index(commits, index);
            self.select_span(commits, anchor, index, mods.toggle);
            self.anchor = Some(commits[anchor].hash.clone());
            self.lead = Some(hash);
        } else if mods.toggle {
            if !self.selected.remove(&hash) {
                self.selected.insert(hash.clone());
            }
            self.anchor = Some(hash.clone());
            self.lead = Some(hash);
        } else {
            self.select_only(&hash);
        }
    }

    /// 键盘移动当前行。`extend = false` 是单选移动(↑/↓);`true` 是锚点不动、选择换成
    /// 锚点..新当前行(Shift+↑/↓)。还没有当前行时落到第一行。返回新当前行的下标。
    pub(super) fn move_lead(
        &mut self,
        commits: &[GitCommitInfo],
        delta: isize,
        extend: bool,
    ) -> Option<usize> {
        let last = commits.len().checked_sub(1)?;
        let current = self
            .lead
            .as_deref()
            .and_then(|hash| index_of(commits, hash));
        let next = match current {
            None => 0,
            Some(cur) => (cur as isize + delta).clamp(0, last as isize) as usize,
        };
        let hash = commits[next].hash.clone();
        if extend {
            let anchor = self.anchor_index(commits, current.unwrap_or(next));
            self.select_span(commits, anchor, next, false);
            self.anchor = Some(commits[anchor].hash.clone());
            self.lead = Some(hash);
        } else {
            self.select_only(&hash);
        }
        Some(next)
    }

    /// 列表重载之后:只留还在列表里的提交。当前行 / 锚点不在了就落到剩下的第一个
    /// 选中项上(都没了就清掉)。返回选中集合变没变。
    pub(super) fn retain(&mut self, commits: &[GitCommitInfo]) -> bool {
        let present: HashSet<&str> = commits.iter().map(|c| c.hash.as_str()).collect();
        let before = self.selected.len();
        self.selected.retain(|hash| present.contains(hash.as_str()));
        let first = || {
            commits
                .iter()
                .find(|c| self.selected.contains(&c.hash))
                .map(|c| c.hash.clone())
        };
        if self
            .lead
            .as_deref()
            .is_some_and(|hash| !present.contains(hash))
        {
            self.lead = first();
        }
        if self
            .anchor
            .as_deref()
            .is_some_and(|hash| !present.contains(hash))
        {
            self.anchor = self.lead.clone();
        }
        self.selected.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_graph::test_commit;

    /// c0(最新)… c5(最旧)。
    fn commits() -> Vec<GitCommitInfo> {
        (0..6).map(|i| test_commit(&format!("c{i}"), &[])).collect()
    }

    fn picked(sel: &CommitSelection, commits: &[GitCommitInfo]) -> Vec<String> {
        sel.ordered(commits)
            .iter()
            .map(|c| c.hash.clone())
            .collect()
    }

    const PLAIN: ClickMods = ClickMods {
        toggle: false,
        range: false,
    };
    const CTRL: ClickMods = ClickMods {
        toggle: true,
        range: false,
    };
    const SHIFT: ClickMods = ClickMods {
        toggle: false,
        range: true,
    };
    const CTRL_SHIFT: ClickMods = ClickMods {
        toggle: true,
        range: true,
    };

    /// 单击 = 单选;Ctrl 单击切换(锚点 / 当前行跟到点的那一行);取消到空也行。
    #[test]
    fn 单击与_ctrl_单击() {
        let list = commits();
        let mut sel = CommitSelection::default();
        sel.click(&list, 1, PLAIN);
        assert_eq!(picked(&sel, &list), ["c1"]);
        assert_eq!(sel.single(), Some("c1"));

        sel.click(&list, 3, CTRL);
        assert_eq!(picked(&sel, &list), ["c1", "c3"]);
        assert_eq!((sel.anchor(), sel.lead()), (Some("c3"), Some("c3")));
        assert_eq!(sel.single(), None);

        sel.click(&list, 1, CTRL);
        assert_eq!(picked(&sel, &list), ["c3"], "再点一下取消");
        assert_eq!(sel.lead(), Some("c1"), "当前行留在取消的那一行");

        sel.click(&list, 3, CTRL);
        assert!(sel.is_empty());

        sel.click(&list, 4, CTRL);
        sel.click(&list, 0, PLAIN);
        assert_eq!(picked(&sel, &list), ["c0"], "普通单击清掉其它");
    }

    /// Shift 单击:锚点到该行连续选中、替换原选择(锚点不动,可反向);
    /// Ctrl+Shift 单击:这一段并入现有选择。
    #[test]
    fn shift_与_ctrl_shift_单击() {
        let list = commits();
        let mut sel = CommitSelection::default();
        sel.click(&list, 2, PLAIN);
        sel.click(&list, 4, SHIFT);
        assert_eq!(picked(&sel, &list), ["c2", "c3", "c4"]);
        assert_eq!((sel.anchor(), sel.lead()), (Some("c2"), Some("c4")));

        sel.click(&list, 0, SHIFT);
        assert_eq!(
            picked(&sel, &list),
            ["c0", "c1", "c2"],
            "反向:替换,锚点不动"
        );

        // Ctrl 单击 c5 → 锚点换到 c5;Ctrl+Shift 单击 c4 → 并入 c4..c5,前面的保留
        sel.click(&list, 5, CTRL);
        sel.click(&list, 4, CTRL_SHIFT);
        assert_eq!(picked(&sel, &list), ["c0", "c1", "c2", "c4", "c5"]);
        assert_eq!(sel.anchor(), Some("c5"));

        // 没有锚点时 Shift 单击 = 只选这一行
        let mut fresh = CommitSelection::default();
        fresh.click(&list, 3, SHIFT);
        assert_eq!(picked(&fresh, &list), ["c3"]);
        assert_eq!(fresh.anchor(), Some("c3"));
    }

    /// ↑/↓ 单选移动(到头钳住、没有当前行时落到第一行);Shift+↑/↓ 从锚点扩展、
    /// 往回走时收缩,越过锚点后从另一侧扩展。
    #[test]
    fn 键盘移动与扩展() {
        let list = commits();
        let mut sel = CommitSelection::default();
        assert_eq!(
            sel.move_lead(&list, 1, false),
            Some(0),
            "没有当前行落到第一行"
        );
        assert_eq!(sel.move_lead(&list, -1, false), Some(0), "到顶钳住");
        sel.move_lead(&list, 1, false);
        sel.move_lead(&list, 1, false);
        assert_eq!(picked(&sel, &list), ["c2"]);

        assert_eq!(sel.move_lead(&list, 1, true), Some(3));
        assert_eq!(sel.move_lead(&list, 1, true), Some(4));
        assert_eq!(picked(&sel, &list), ["c2", "c3", "c4"]);
        assert_eq!((sel.anchor(), sel.lead()), (Some("c2"), Some("c4")));

        sel.move_lead(&list, -1, true);
        assert_eq!(picked(&sel, &list), ["c2", "c3"], "往回收缩");
        sel.move_lead(&list, -1, true);
        sel.move_lead(&list, -1, true);
        assert_eq!(picked(&sel, &list), ["c1", "c2"], "越过锚点从另一侧扩展");

        // 普通 ↓ 从当前行单选移动
        assert_eq!(sel.move_lead(&list, 1, false), Some(2));
        assert_eq!(picked(&sel, &list), ["c2"]);
        assert_eq!(sel.anchor(), Some("c2"));

        // Ctrl 取消选中之后,Shift+↓ 从当前行(锚点)接着扩展
        sel.click(&list, 2, CTRL);
        assert!(sel.is_empty());
        sel.move_lead(&list, 1, true);
        assert_eq!(picked(&sel, &list), ["c2", "c3"]);

        assert_eq!(CommitSelection::default().move_lead(&[], 1, true), None);
    }

    /// 重载(保留旧列表)之后只留还在列表里的提交;当前行 / 锚点不在了就落到剩下的
    /// 第一个选中项;全没了就空。返回值说集合变没变。
    #[test]
    fn 重载保留仍存在的提交() {
        let list = commits();
        let mut sel = CommitSelection::default();
        sel.click(&list, 1, PLAIN);
        sel.click(&list, 3, SHIFT);
        assert_eq!(picked(&sel, &list), ["c1", "c2", "c3"]);

        assert!(!sel.retain(&list), "都还在:没变");

        // c3(当前行)被改写掉了
        let reloaded: Vec<GitCommitInfo> =
            list.iter().filter(|c| c.hash != "c3").cloned().collect();
        assert!(sel.retain(&reloaded));
        assert_eq!(picked(&sel, &reloaded), ["c1", "c2"]);
        assert_eq!((sel.anchor(), sel.lead()), (Some("c1"), Some("c1")));

        assert!(sel.retain(&[]));
        assert!(sel.is_empty());
        assert_eq!((sel.anchor(), sel.lead()), (None, None));
    }
}
