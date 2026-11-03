//! 已加载提交历史的常驻索引([`LoadedLog`]):历史区每续一页,去重、跨页拓扑检查与续页
//! 游标都只按**新来的这一页**增量维护。
//!
//! # 为什么要常驻
//!
//! [`super::merge_log_page`] 与 [`GitLogCursor::from_loaded`] 都是「拿整份已加载列表现算」:
//! 前者每页把全部 hash 克隆进一个 `HashSet` 与一个位置表,后者每页把全部 hash 解析成
//! `Oid` 建集合。一页 30 条,翻到第 N 页累计做了 O(N² · 30) 的活 —— 上万条之后每续一页
//! 都要在主线程白做几到几十毫秒。这里把那两张表常驻下来:
//!
//! - 位置表(hash → 行号)随追加的行增量插入,去重与「新提交的父是否已排在它前面」的
//!   检查都只查新来的这一页;
//! - 续页游标随追加的行增量推进,结果与 `from_loaded(整份)` **逐字段相同**(推导见
//!   [`GitLogCursor::extend_loaded`]);
//! - 罕见的跨页拓扑修正(时钟偏移下迟到的子提交)会整体重排行序,这时两张表一并重建,
//!   并把 [`LogMerge::reordered`] 交给调用方 —— 依赖行序的派生数据(拓扑图)也得整份重算。
//!
//! 合并语义与 [`super::merge_log_page`] 一字不差(单测逐页对拍)。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use git2::Oid;

use super::{GitCommitInfo, GitLogCursor, stable_topo_reorder};

/// 一次合并的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogMerge {
    /// 实际新增的条数(重复的不算)。
    pub added: usize,
    /// 这一页触发了跨页拓扑修正,已加载列表整体重排过:按行序增量维护的派生数据要整份重算。
    pub reordered: bool,
}

/// 已加载列表的常驻索引。**只描述交给 [`Self::merge`] 的那一份列表**:调用方对列表的
/// 其它改动(清空、换一份)必须同步 [`Self::clear`] / [`Self::rebuild`],否则两边对不上。
#[derive(Debug, Clone, Default)]
pub struct LoadedLog {
    /// hash → 在已加载列表里的行号。
    pos: HashMap<String, usize>,
    /// 按已加载列表还原的续页游标(与 [`GitLogCursor::from_loaded`] 相同)。共享出去给后台
    /// 取页;续页之间是串行的,推进它的时候那一趟早已回来,`Arc::make_mut` 原地改、不复制。
    cursor: Arc<GitLogCursor>,
}

impl LoadedLog {
    /// 按一份现成的列表整份建(重排之后、或调用方换了一份列表时)。
    pub fn rebuild(loaded: &[GitCommitInfo]) -> Self {
        Self {
            pos: loaded
                .iter()
                .enumerate()
                .map(|(i, c)| (c.hash.clone(), i))
                .collect(),
            cursor: Arc::new(GitLogCursor::from_loaded(loaded)),
        }
    }

    /// 列表清空了。
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// 索引里有几条(= 列表长度)。
    pub fn len(&self) -> usize {
        self.pos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pos.is_empty()
    }

    /// 这个提交在列表里的行号。
    pub fn position(&self, hash: &str) -> Option<usize> {
        self.pos.get(hash).copied()
    }

    pub fn contains(&self, hash: &str) -> bool {
        self.pos.contains_key(hash)
    }

    /// 续页游标(与 `GitLogCursor::from_loaded(列表)` 相同),共享一份,不复制。
    pub fn cursor(&self) -> Arc<GitLogCursor> {
        self.cursor.clone()
    }

    /// 把新取到的一页并进 `loaded`,语义同 [`super::merge_log_page`]:按 hash 去重(页内
    /// 重复的也去),新提交里有「父已经排在它前面」的就对整份列表做一次稳定的拓扑重排。
    /// 只有重排时才整份重建索引,其余只做与这一页成正比的活。
    pub fn merge(&mut self, loaded: &mut Vec<GitCommitInfo>, page: Vec<GitCommitInfo>) -> LogMerge {
        debug_assert_eq!(self.pos.len(), loaded.len(), "索引与列表对不上");
        let before = loaded.len();
        for commit in page {
            if self.pos.contains_key(&commit.hash) {
                continue;
            }
            self.pos.insert(commit.hash.clone(), loaded.len());
            loaded.push(commit);
        }
        let added = loaded.len() - before;
        if added == 0 {
            return LogMerge {
                added,
                reordered: false,
            };
        }
        // 旧的部分此前已是拓扑序、新提交全排在旧提交之后,所以只可能是新提交的父在前
        // (与 `new_tail_is_topo_ordered` 同一条论证,只是位置表不必现建)
        let ordered = loaded[before..].iter().enumerate().all(|(k, commit)| {
            commit
                .parent_hashes
                .iter()
                .all(|p| self.pos.get(p.as_str()).is_none_or(|&pi| pi > before + k))
        });
        if ordered {
            Arc::make_mut(&mut self.cursor).extend_loaded(&loaded[before..]);
            return LogMerge {
                added,
                reordered: false,
            };
        }
        stable_topo_reorder(loaded);
        *self = Self::rebuild(loaded);
        LogMerge {
            added,
            reordered: true,
        }
    }
}

impl GitLogCursor {
    /// 已加载列表**末尾**又接上了 `tail`(前面的行序没动):把游标推进到与
    /// `from_loaded(整份列表)` 一模一样。
    ///
    /// `from_loaded` 的 frontier 是「已加载提交的父 − 已加载提交」,按在列表里第一次出现的
    /// 先后排。旧行全在 `tail` 之前,所以:旧行贡献的那部分 = 旧 frontier 里没被 `tail`
    /// 加载掉的,次序不变;`tail` 的父只可能排在它们后面,按出现先后追加、跳过已在
    /// frontier 里的。代价 O(frontier + tail),与已加载总数无关。
    fn extend_loaded(&mut self, tail: &[GitCommitInfo]) {
        self.loaded
            .extend(tail.iter().filter_map(|c| Oid::from_str(&c.hash).ok()));
        let loaded = &self.loaded;
        self.frontier.retain(|oid| !loaded.contains(oid));
        let mut queued: HashSet<Oid> = self.frontier.iter().copied().collect();
        for commit in tail {
            for parent in &commit.parent_hashes {
                if let Ok(oid) = Oid::from_str(parent)
                    && !self.loaded.contains(&oid)
                    && queued.insert(oid)
                {
                    self.frontier.push(oid);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::merge_log_page;
    use super::*;

    /// 40 位十六进制的假 hash(`Oid::from_str` 认得,游标两条路才真的在比 Oid)。
    fn oid_hex(n: usize) -> String {
        format!("{n:040x}")
    }

    fn commit(n: usize, parents: &[usize]) -> GitCommitInfo {
        GitCommitInfo {
            hash: oid_hex(n),
            short_hash: oid_hex(n)[..7].to_string(),
            message: format!("c{n}"),
            body: None,
            author: String::new(),
            timestamp: 0,
            parent_hashes: parents.iter().map(|&p| oid_hex(p)).collect(),
        }
    }

    fn hashes(list: &[GitCommitInfo]) -> Vec<&str> {
        list.iter().map(|c| c.hash.as_str()).collect()
    }

    /// 固定种子的 LCG,失败可复现。
    fn lcg(seed: u64) -> impl FnMut(u64) -> u64 {
        let mut state = seed;
        move |n: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % n.max(1)
        }
    }

    /// 随机 DAG(编号大的是子,父编号更小)按「新 → 旧」排好的拓扑序。
    fn random_history(rand: &mut impl FnMut(u64) -> u64, n: usize) -> Vec<GitCommitInfo> {
        let mut out = Vec::new();
        for i in 0..n {
            let mut parents = Vec::new();
            if i > 0 {
                parents.push(i - 1 - rand((i as u64).min(5)) as usize);
                if rand(4) == 0 {
                    let other = rand(i as u64) as usize;
                    if !parents.contains(&other) {
                        parents.push(other);
                    }
                }
            }
            out.push(commit(i, &parents));
        }
        out.reverse();
        out
    }

    /// 增量索引与旧实现逐页对拍:列表内容 / 顺序、新增条数与 `merge_log_page` 一致,
    /// 位置表与列表一致,游标与 `from_loaded(整份)` 逐字段相同。页里混进重复(已加载的、
    /// 页内重复的)、偶尔把一个子提交挪到后面的页里(迟到的子,触发跨页重排)。
    #[test]
    fn 增量合并与整份重算逐页一致() {
        for seed in [1u64, 7, 42, 2026] {
            let mut rand = lcg(seed);
            let history = random_history(&mut rand, 160);
            // 切页:大体按顺序,偶尔把一个提交扣下来塞进后面某一页的末尾(迟到)
            let mut queue: Vec<GitCommitInfo> = history.clone();
            let mut pages: Vec<Vec<GitCommitInfo>> = Vec::new();
            let mut late: Vec<(usize, GitCommitInfo)> = Vec::new();
            while !queue.is_empty() {
                let size = (1 + rand(12) as usize).min(queue.len());
                let mut page: Vec<GitCommitInfo> = queue.drain(..size).collect();
                if page.len() > 1 && rand(5) == 0 {
                    let k = rand(page.len() as u64) as usize;
                    let delay = 1 + rand(3) as usize;
                    late.push((pages.len() + delay, page.remove(k)));
                }
                // 已加载的 / 页内重复的混进来
                if let Some(prev) = pages.last().and_then(|p| p.first())
                    && rand(3) == 0
                {
                    page.push(prev.clone());
                }
                if rand(4) == 0 && !page.is_empty() {
                    page.push(page[0].clone());
                }
                let due: Vec<GitCommitInfo> = late
                    .iter()
                    .filter(|(at, _)| *at <= pages.len())
                    .map(|(_, c)| c.clone())
                    .collect();
                late.retain(|(at, _)| *at > pages.len());
                page.extend(due);
                pages.push(page);
            }
            pages.push(late.into_iter().map(|(_, c)| c).collect());

            let mut reference: Vec<GitCommitInfo> = Vec::new();
            let mut loaded: Vec<GitCommitInfo> = Vec::new();
            let mut index = LoadedLog::default();
            let mut reorders = 0;
            for (round, page) in pages.into_iter().enumerate() {
                let want = merge_log_page(&mut reference, page.clone());
                let got = index.merge(&mut loaded, page);
                reorders += usize::from(got.reordered);
                let ctx = format!("seed {seed} 第 {round} 页");
                assert_eq!(got.added, want, "{ctx}");
                assert_eq!(hashes(&loaded), hashes(&reference), "{ctx}");
                assert_eq!(index.len(), loaded.len(), "{ctx}");
                for (i, c) in loaded.iter().enumerate() {
                    assert_eq!(index.position(&c.hash), Some(i), "{ctx}");
                }
                assert_eq!(*index.cursor(), GitLogCursor::from_loaded(&loaded), "{ctx}");
            }
            assert_eq!(loaded.len(), history.len(), "seed {seed}");
            assert!(
                reorders > 0,
                "seed {seed}:迟到的子提交没触发过重排,样本白造了"
            );
        }
    }

    /// 没有迟到的子提交时一行不挪、不报重排;重复页不新增;游标原地推进
    /// (后台那趟已经放手了,不复制)。
    #[test]
    fn 顺序合法时只追加不重排() {
        let mut loaded = Vec::new();
        let mut index = LoadedLog::default();
        let first = index.merge(&mut loaded, vec![commit(5, &[4]), commit(4, &[3])]);
        assert_eq!(
            first,
            LogMerge {
                added: 2,
                reordered: false
            }
        );
        let held = index.cursor();
        assert_eq!(held.frontier, vec![Oid::from_str(&oid_hex(3)).unwrap()]);
        drop(held);
        let before = Arc::as_ptr(&index.cursor);
        let next = index.merge(&mut loaded, vec![commit(4, &[3]), commit(3, &[])]);
        assert_eq!(next.added, 1, "重复的不算");
        assert!(!next.reordered);
        assert_eq!(Arc::as_ptr(&index.cursor), before, "没人持有时原地推进");
        assert!(index.cursor().frontier.is_empty(), "根提交之后没有下文");
        assert_eq!(
            index.merge(&mut loaded, vec![commit(3, &[])]),
            LogMerge {
                added: 0,
                reordered: false
            }
        );

        // 后台还拿着旧游标时推进:旧的那份不受影响
        let held = index.cursor();
        index.merge(&mut loaded, vec![commit(9, &[8])]);
        assert!(held.frontier.is_empty());
        assert_eq!(*index.cursor(), GitLogCursor::from_loaded(&loaded));
    }

    /// 迟到的子提交:重排到它父的前面,索引整份重建,报 `reordered`。
    #[test]
    fn 迟到的子提交触发重排并重建索引() {
        let mut loaded = Vec::new();
        let mut index = LoadedLog::default();
        index.merge(
            &mut loaded,
            vec![commit(10, &[8, 9]), commit(8, &[1]), commit(1, &[])],
        );
        let got = index.merge(&mut loaded, vec![commit(9, &[1])]);
        assert_eq!(
            got,
            LogMerge {
                added: 1,
                reordered: true
            }
        );
        let order: Vec<_> = loaded.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(order, ["c10", "c8", "c9", "c1"]);
        assert_eq!(index.position(&oid_hex(9)), Some(2));
        assert_eq!(index.position(&oid_hex(1)), Some(3));
        assert_eq!(*index.cursor(), GitLogCursor::from_loaded(&loaded));

        index.clear();
        assert!(index.is_empty());
        assert!(!index.contains(&oid_hex(9)));
    }
}
