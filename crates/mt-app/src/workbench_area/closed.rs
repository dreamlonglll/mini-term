//! 每个项目一份的「已关闭的文件页签」栈(重新打开用)。纯数据,可测。
//!
//! 只放内存、不落盘(与页签本身同一口径,见上级模块注释),项目被移除时随
//! `ProjectDocuments` 一起回收。diff 页签不入栈。

use std::path::{Path, PathBuf};

use super::paths::{self, PathSemantics};

/// 栈里最多记几条。再多就从最早关掉的那条开始丢。
pub(super) const MAX_CLOSED_TABS: usize = 20;

/// 关掉的一个文件页签。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClosedTab {
    /// 打开时的原样路径(本地是平台路径,远程是 POSIX 路径)。
    pub path: PathBuf,
    /// 来源:远程(SSH)还是本地。重开时项目已经换了形态(本地 ↔ 远程)就不认。
    pub remote: bool,
    /// 关闭时光标所在的行(从 1 数)。关的时候停在 Markdown / HTML 渲染稿上的不记 ——
    /// 带行号打开会直接进源码。
    pub line: Option<u32>,
    /// 关闭时在页签条上排第几。重开时插回这个位置(页签条变短了就落在末尾)。
    pub index: usize,
}

impl ClosedTab {
    fn semantics(&self) -> PathSemantics {
        PathSemantics::for_document(self.remote)
    }

    fn same_file(&self, other: &ClosedTab) -> bool {
        self.remote == other.remote
            && paths::is_within(&self.path, &other.path, self.semantics())
            && paths::is_within(&other.path, &self.path, self.semantics())
    }
}

/// 已关闭页签栈。栈顶在 `items` 末尾。
#[derive(Debug, Default)]
pub(super) struct ClosedTabs {
    items: Vec<ClosedTab>,
}

impl ClosedTabs {
    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 一批关掉的页签入栈。`batch` 按页签条从左到右给:**倒着压**,最左那个落在栈顶 ——
    /// 逐个重开时先回来的是最左边的,各自插回原来的位置,顺序就还原了。单个关闭就是只有
    /// 一条的批。同一个文件已经在栈里的,先摘掉旧的那条(只留最近关的那次)。
    pub(super) fn push_batch(&mut self, batch: Vec<ClosedTab>) {
        for tab in batch.into_iter().rev() {
            self.items.retain(|existing| !existing.same_file(&tab));
            self.items.push(tab);
        }
        let overflow = self.items.len().saturating_sub(MAX_CLOSED_TABS);
        self.items.drain(..overflow);
    }

    /// 从栈顶往下的快照:重开时按这个次序一条条试,第一条还在的那个就是要开的。
    pub(super) fn candidates(&self) -> Vec<ClosedTab> {
        self.items.iter().rev().cloned().collect()
    }

    /// 摘掉这些条目(重开成功的那一条,与试过但文件已不在的那些)。按内容认、各摘一条:
    /// 试的这一会儿又有新关掉的页签压上来也不受影响。
    pub(super) fn remove(&mut self, done: &[ClosedTab]) {
        for tab in done {
            if let Some(index) = self.items.iter().rposition(|existing| existing == tab) {
                self.items.remove(index);
            }
        }
    }

    /// 文件树里改名 / 移动了 `from`(文件或目录):栈里在它之下的路径跟着换成 `to` 之下。
    pub(super) fn rebase(&mut self, from: &Path, to: &Path, remote: bool) {
        let sem = PathSemantics::for_document(remote);
        for tab in self.items.iter_mut().filter(|tab| tab.remote == remote) {
            if let Some(path) = paths::rebase(&tab.path, from, to, sem) {
                tab.path = path;
            }
        }
    }

    /// 文件树里删掉了 `path`(文件或目录):栈里在它之下的条目一并丢掉,重开也开不出来。
    pub(super) fn forget_within(&mut self, path: &Path, remote: bool) {
        let sem = PathSemantics::for_document(remote);
        self.items
            .retain(|tab| tab.remote != remote || !paths::is_within(&tab.path, path, sem));
    }
}

/// 重开时挑出来的结果,见 [`pick_reopen`]。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ReopenPick {
    /// 试过但开不出来的(文件已经不在 / 来源对不上),要从栈里摘掉。
    pub tried: Vec<ClosedTab>,
    /// 第一条还在的,就重开它。
    pub found: Option<ClosedTab>,
    /// 探的途中出了错(远程连不上):停在这儿,没试到的条目留在栈里。
    pub error: Option<String>,
}

/// 从栈顶往下一条条试(`candidates` 即 [`ClosedTabs::candidates`]):来源对不上(项目换了
/// 本地 / 远程形态)或 `exists` 说文件已经不在的跳过,第一条还在的就是要重开的。
/// `exists` 报错就停下 —— 断网时不该把整个栈当成「全都不在了」丢掉。阻塞(`exists` 要碰盘 /
/// 走 SFTP),调用方丢后台。
pub(super) fn pick_reopen(
    candidates: Vec<ClosedTab>,
    remote: bool,
    mut exists: impl FnMut(&Path) -> Result<bool, String>,
) -> ReopenPick {
    let mut tried = Vec::new();
    for tab in candidates {
        let alive = if tab.remote == remote {
            exists(&tab.path)
        } else {
            Ok(false)
        };
        match alive {
            Ok(true) => {
                return ReopenPick {
                    tried,
                    found: Some(tab),
                    error: None,
                };
            }
            Ok(false) => tried.push(tab),
            Err(error) => {
                return ReopenPick {
                    tried,
                    found: None,
                    error: Some(error),
                };
            }
        }
    }
    ReopenPick {
        tried,
        found: None,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(path: &str, index: usize) -> ClosedTab {
        ClosedTab {
            path: PathBuf::from(path),
            remote: true,
            line: Some(index as u32 + 1),
            index,
        }
    }

    fn paths_of(tabs: &[ClosedTab]) -> Vec<&str> {
        tabs.iter().map(|tab| tab.path.to_str().unwrap()).collect()
    }

    /// 批量关闭倒着压:逐个重开先回来最左边的;单个关闭的后关先开。
    #[test]
    fn 批量关闭倒着入栈_逐个重开还原顺序() {
        let mut stack = ClosedTabs::default();
        stack.push_batch(vec![closed("/p/x.rs", 4)]);
        stack.push_batch(vec![
            closed("/p/a.rs", 0),
            closed("/p/c.rs", 2),
            closed("/p/d.rs", 3),
        ]);
        let order = stack.candidates();
        assert_eq!(
            paths_of(&order),
            ["/p/a.rs", "/p/c.rs", "/p/d.rs", "/p/x.rs"]
        );
        // 各自记着原来的位置:a 插回 0、c 插回 2、d 插回 3 —— 顺序还原
        assert_eq!(
            order.iter().map(|tab| tab.index).collect::<Vec<_>>(),
            [0, 2, 3, 4]
        );
    }

    /// 同一个文件只留最近关的那次;上限 20,超出从最早的丢。
    #[test]
    fn 去重并封顶() {
        let mut stack = ClosedTabs::default();
        stack.push_batch(vec![closed("/p/a.rs", 0)]);
        stack.push_batch(vec![closed("/p/b.rs", 1)]);
        stack.push_batch(vec![closed("/p/a.rs", 5)]);
        let order = stack.candidates();
        assert_eq!(paths_of(&order), ["/p/a.rs", "/p/b.rs"]);
        assert_eq!(order[0].index, 5);

        let mut stack = ClosedTabs::default();
        for i in 0..(MAX_CLOSED_TABS + 5) {
            stack.push_batch(vec![closed(&format!("/p/{i}.rs"), i)]);
        }
        let order = stack.candidates();
        assert_eq!(order.len(), MAX_CLOSED_TABS);
        assert_eq!(
            order[0].path,
            PathBuf::from(format!("/p/{}.rs", MAX_CLOSED_TABS + 4))
        );
        assert_eq!(order.last().unwrap().path, PathBuf::from("/p/5.rs"));
    }

    /// 重开摘掉用过的条目;试的这一会儿新压上来的不受影响。
    #[test]
    fn 重开后摘掉用过的条目() {
        let mut stack = ClosedTabs::default();
        stack.push_batch(vec![closed("/p/a.rs", 0), closed("/p/b.rs", 1)]);
        let tried = stack.candidates();
        stack.push_batch(vec![closed("/p/new.rs", 2)]);
        stack.remove(&tried[..1]);
        assert_eq!(paths_of(&stack.candidates()), ["/p/new.rs", "/p/b.rs"]);
        assert!(!stack.is_empty());
        stack.remove(&stack.candidates());
        assert!(stack.is_empty());
    }

    /// 文件树里改名 / 移动、删除:栈里在其下的路径跟着换 / 丢掉;另一种来源的不动。
    #[test]
    fn 跟着文件树改名与删除() {
        let mut stack = ClosedTabs::default();
        stack.push_batch(vec![
            closed("/p/src/a.rs", 0),
            closed("/p/srcx/b.rs", 1),
            closed("/p/doc.md", 2),
        ]);
        stack.rebase(Path::new("/p/src"), Path::new("/p/lib"), true);
        assert_eq!(
            paths_of(&stack.candidates()),
            ["/p/lib/a.rs", "/p/srcx/b.rs", "/p/doc.md"]
        );
        // 本地的改动不碰远程的条目
        stack.forget_within(Path::new("/p/lib"), false);
        assert_eq!(stack.candidates().len(), 3);
        stack.forget_within(Path::new("/p/lib"), true);
        assert_eq!(paths_of(&stack.candidates()), ["/p/srcx/b.rs", "/p/doc.md"]);
    }

    /// 重开:已经不在的、来源对不上的跳过,第一条还在的就是它;后面的不再探。
    #[test]
    fn 重开跳过已不存在的文件() {
        let mut local = closed("/p/local.rs", 0);
        local.remote = false;
        let candidates = vec![
            closed("/p/gone.rs", 0),
            local.clone(),
            closed("/p/here.rs", 1),
            closed("/p/later.rs", 2),
        ];
        let mut probed = Vec::new();
        let pick = pick_reopen(candidates, true, |path| {
            probed.push(path.to_path_buf());
            Ok(path != Path::new("/p/gone.rs"))
        });
        assert_eq!(pick.found, Some(closed("/p/here.rs", 1)));
        assert_eq!(pick.tried, vec![closed("/p/gone.rs", 0), local]);
        assert_eq!(pick.error, None);
        assert_eq!(
            probed,
            [PathBuf::from("/p/gone.rs"), PathBuf::from("/p/here.rs")],
            "来源对不上的不探,找到之后的也不探"
        );

        // 全都不在:一条都开不出来,全部摘掉
        let pick = pick_reopen(vec![closed("/p/a.rs", 0)], true, |_| Ok(false));
        assert_eq!(pick.found, None);
        assert_eq!(pick.tried.len(), 1);

        // 探的途中出错:停下,没试到的留在栈里
        let pick = pick_reopen(
            vec![closed("/p/a.rs", 0), closed("/p/b.rs", 1)],
            true,
            |path| {
                if path == Path::new("/p/a.rs") {
                    Ok(false)
                } else {
                    Err("连不上".into())
                }
            },
        );
        assert_eq!(pick.tried, vec![closed("/p/a.rs", 0)]);
        assert_eq!(pick.found, None);
        assert_eq!(pick.error.as_deref(), Some("连不上"));
    }
}
