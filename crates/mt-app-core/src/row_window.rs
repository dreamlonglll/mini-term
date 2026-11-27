//! 长列表的窗口化铺行:只给视口附近的行建元素,屏外连续的行折成一段等高的空白。
//!
//! 给文件树用(`mt-app::file_tree`)。文件树是「整树拍平、逐行建元素」的写法,展开态又是
//! 落盘的 —— 点开过的目录越积越多,几百上千行很常见。不窗口化时任何一帧(悬停换行、
//! 点行聚焦、折叠动画的每一帧)都要把**全部**行重建、排版、绘制一遍,成本随总行数涨,
//! 与眼前那几十行无关。这里算出「这一帧哪些行真建、哪些行折成空白」,元素由调用方去建。
//!
//! # 前提
//!
//! - **行高一致**:每行同一高度 `row_height`(调用方量第一行得来)。空白段的高度 = 行数 ×
//!   行高,与真建出来时逐行累加的高度相同,所以滚动范围、各行的位置都不变;
//! - **在飞的折叠动画**(目录展开 / 收起的高度裁剪窗)是例外的一块:子树整体在一个高为
//!   `clip` 的裁剪窗里,窗内按自然高度排、超出窗高的部分被裁掉。实现方经 [`Folds`] 告诉
//!   这里哪些行带着在飞的动画、给定子树自然高时窗高多少;窗内同样窗口化,窗外整块当空白。
//!
//! # 坐标
//!
//! 一律是列表**内容坐标**(px):第一行的顶边是 0,向下为正。可视区是
//! `[scroll_top, scroll_top + viewport)`,调用方上下再各多铺一段(overscan),挡住
//! 「视口这一帧变大 / 滚动还没触发重画」那一下露白。

use std::collections::HashMap;
use std::ops::Range;

/// 行里哪些带着在飞的折叠动画,以及它们的裁剪窗此刻多高。见模块注释「前提」。
pub trait Folds {
    /// 第 `ix` 行(目录)此刻有没有在飞的折叠动画。
    fn is_folding(&self, ix: usize) -> bool;
    /// 第 `ix` 行的折叠动画在子树自然高为 `natural` 时的裁剪窗高。实现方可以顺手把
    /// `natural` 记给动画(屏外的那块不建元素,动画自己的量高探针就跑不到)。
    fn clip(&mut self, ix: usize, natural: f32) -> f32;
}

/// 没有任何在飞动画(静止的列表)。
pub struct NoFolds;

impl Folds for NoFolds {
    fn is_folding(&self, _ix: usize) -> bool {
        false
    }

    fn clip(&mut self, _ix: usize, natural: f32) -> f32 {
        natural
    }
}

/// 铺行计划里的一项。
#[derive(Clone, Debug, PartialEq)]
pub enum Slot {
    /// 一段不建元素的空白,高 px(连续的屏外行 / 整块在屏外的折叠动画)。
    Gap(f32),
    /// 第 `ix` 行(下标指进传给 [`RowLayout::measure`] 的那份行序)。
    Row(usize),
    /// 一个在飞的折叠动画:第 `header` 行的子树装在高 `clip` 的裁剪窗里,窗内按 `slots`
    /// 铺(它们的总高是子树的自然高,超出 `clip` 的部分被窗裁掉)。`header` 那一行本身
    /// 不在这里,它排在这一项前面,是普通的一项。
    Fold {
        header: usize,
        clip: f32,
        slots: Vec<Slot>,
    },
}

/// 量好的一份行布局:各折叠动画的窗高与整个列表的总高。先 [`Self::measure`],再按这一帧的
/// 可视区出 [`Self::slots`]。
#[derive(Debug)]
pub struct RowLayout<'a> {
    depths: &'a [usize],
    row_height: f32,
    /// 带在飞折叠动画、且子树非空的行 → 窗高。
    clips: HashMap<usize, f32>,
    height: f32,
}

impl<'a> RowLayout<'a> {
    /// `depths` 是按显示序拍平的各行缩进深度(子树 = 紧随其后、深度更大的那一段)。
    pub fn measure(depths: &'a [usize], row_height: f32, folds: &mut dyn Folds) -> Self {
        let mut layout = Self {
            depths,
            row_height,
            clips: HashMap::new(),
            height: 0.0,
        };
        layout.height = layout.measure_range(0..depths.len(), folds);
        layout
    }

    /// 整个列表的内容高(px)。
    pub fn height(&self) -> f32 {
        self.height
    }

    /// 按可视区 `[top, bottom)`(已含 overscan)出铺行计划。`keep` 那一行不论在哪都真建
    /// (拿着焦点的行:不建的话焦点路径断掉,列表会收到一次失焦)。各项总高恒等于
    /// [`Self::height`]。
    pub fn slots(&self, top: f32, bottom: f32, keep: Option<usize>) -> Vec<Slot> {
        let mut out = Vec::new();
        self.lay(0..self.depths.len(), 0.0, (top, bottom), keep, &mut out);
        out
    }

    /// 第 `ix` 行的子树结束在哪(不含):紧随其后、深度更大的那一段的尾后。
    fn subtree_end(&self, ix: usize, limit: usize) -> usize {
        let depth = self.depths[ix];
        let mut end = ix + 1;
        while end < limit && self.depths[end] > depth {
            end += 1;
        }
        end
    }

    fn measure_range(&mut self, range: Range<usize>, folds: &mut dyn Folds) -> f32 {
        let mut height = 0.0;
        let mut ix = range.start;
        while ix < range.end {
            height += self.row_height;
            if folds.is_folding(ix) {
                let end = self.subtree_end(ix, range.end);
                if end > ix + 1 {
                    let natural = self.measure_range(ix + 1..end, folds);
                    let clip = folds.clip(ix, natural).max(0.0);
                    self.clips.insert(ix, clip);
                    height += clip;
                    ix = end;
                    continue;
                }
            }
            ix += 1;
        }
        height
    }

    /// 从 `y0` 起铺 `range`,把计划追加进 `out`,返回这一段的总高。`band` 是要真建的区间。
    fn lay(
        &self,
        range: Range<usize>,
        y0: f32,
        band: (f32, f32),
        keep: Option<usize>,
        out: &mut Vec<Slot>,
    ) -> f32 {
        let hits = |y: f32, h: f32| y < band.1 && y + h > band.0;
        let mut y = y0;
        let mut gap = 0.0_f32;
        let mut ix = range.start;
        while ix < range.end {
            if hits(y, self.row_height) || keep == Some(ix) {
                flush_gap(&mut gap, out);
                out.push(Slot::Row(ix));
            } else {
                gap += self.row_height;
            }
            y += self.row_height;
            if let Some(&clip) = self.clips.get(&ix) {
                let end = self.subtree_end(ix, range.end);
                let keep_inside = keep.filter(|k| (ix + 1..end).contains(k));
                // 窗里只看得见 `[y, y + clip)`。下沿多带一行:动画按调用时刻取进度,
                // 真画出来那一刻窗可能又长了零点几到几像素,别让窗底露出一条空白
                let inner = (band.0.max(y), band.1.min(y + clip + self.row_height));
                if inner.0 < inner.1 || keep_inside.is_some() {
                    flush_gap(&mut gap, out);
                    let mut slots = Vec::new();
                    self.lay(ix + 1..end, y, inner, keep_inside, &mut slots);
                    out.push(Slot::Fold {
                        header: ix,
                        clip,
                        slots,
                    });
                } else {
                    gap += clip;
                }
                y += clip;
                ix = end;
                continue;
            }
            ix += 1;
        }
        flush_gap(&mut gap, out);
        y - y0
    }
}

fn flush_gap(gap: &mut f32, out: &mut Vec<Slot>) {
    if *gap > 0.0 {
        out.push(Slot::Gap(*gap));
        *gap = 0.0;
    }
}

/// 把静止列表(没有在飞的折叠动画)的第 `ix` 行滚进可视区要的新 `scroll_top`;已经整行
/// 在里面就是 `None`。滚动量取最少的那一边(与 gpui `ScrollHandle::scroll_to_item` 的
/// 默认口径一致:在上面就贴顶,在下面就贴底;比视口还高的行贴顶)。
pub fn reveal_scroll_top(
    ix: usize,
    row_height: f32,
    scroll_top: f32,
    viewport: f32,
) -> Option<f32> {
    let top = ix as f32 * row_height;
    let bottom = top + row_height;
    if top < scroll_top || row_height > viewport {
        (top != scroll_top).then_some(top)
    } else if bottom > scroll_top + viewport {
        Some(bottom - viewport)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: f32 = 20.0;

    /// 计划铺出来的总高(折叠项按窗高算)。
    fn total(slots: &[Slot]) -> f32 {
        slots
            .iter()
            .map(|slot| match slot {
                Slot::Gap(h) => *h,
                Slot::Row(_) => H,
                Slot::Fold { clip, .. } => *clip,
            })
            .sum()
    }

    /// 计划里真建的行(按出现顺序,含折叠窗里的)。
    fn rows(slots: &[Slot]) -> Vec<usize> {
        let mut out = Vec::new();
        for slot in slots {
            match slot {
                Slot::Row(ix) => out.push(*ix),
                Slot::Fold { slots, .. } => out.extend(rows(slots)),
                Slot::Gap(_) => {}
            }
        }
        out
    }

    /// 固定进度的假动画:`folding` 里的行带动画,窗高 = 自然高 × `progress`。
    struct FakeFolds {
        folding: Vec<usize>,
        progress: f32,
        naturals: Vec<(usize, f32)>,
    }

    impl Folds for FakeFolds {
        fn is_folding(&self, ix: usize) -> bool {
            self.folding.contains(&ix)
        }

        fn clip(&mut self, ix: usize, natural: f32) -> f32 {
            self.naturals.push((ix, natural));
            natural * self.progress
        }
    }

    #[test]
    fn 静止列表只建可视区的行_上下各折成一段空白() {
        let depths = vec![0; 100];
        let layout = RowLayout::measure(&depths, H, &mut NoFolds);
        assert_eq!(layout.height(), 100.0 * H);
        // 可视区 [400, 600):第 20..30 行
        let slots = layout.slots(400.0, 600.0, None);
        assert_eq!(slots.first(), Some(&Slot::Gap(400.0)));
        assert_eq!(slots.last(), Some(&Slot::Gap(70.0 * H)));
        assert_eq!(rows(&slots), (20..30).collect::<Vec<_>>());
        assert_eq!(total(&slots), layout.height(), "空白 + 行 = 原来的总高");
    }

    #[test]
    fn 压线的行也算看得见() {
        let depths = vec![0; 10];
        let layout = RowLayout::measure(&depths, H, &mut NoFolds);
        // 可视区上沿切在第 2 行中间、下沿切在第 4 行中间
        let slots = layout.slots(50.0, 90.0, None);
        assert_eq!(rows(&slots), vec![2, 3, 4]);
        assert_eq!(total(&slots), layout.height());
    }

    #[test]
    fn 拿着焦点的行在屏外也照建_空白在它两边断开() {
        let depths = vec![0; 50];
        let layout = RowLayout::measure(&depths, H, &mut NoFolds);
        let slots = layout.slots(0.0, 100.0, Some(40));
        assert_eq!(rows(&slots), vec![0, 1, 2, 3, 4, 40]);
        assert_eq!(
            &slots[5..],
            &[Slot::Gap(35.0 * H), Slot::Row(40), Slot::Gap(9.0 * H)]
        );
        assert_eq!(total(&slots), layout.height());
    }

    #[test]
    fn 可视区覆盖全表时逐行全建_没有空白() {
        let depths = vec![0, 1, 1, 0];
        let layout = RowLayout::measure(&depths, H, &mut NoFolds);
        let slots = layout.slots(-100.0, 1000.0, None);
        assert_eq!(
            slots,
            vec![Slot::Row(0), Slot::Row(1), Slot::Row(2), Slot::Row(3)]
        );
    }

    #[test]
    fn 折叠动画按窗高计入总高_窗里同样窗口化() {
        // 第 1 行是目录,子树 2..=41(40 行)正在展开,进度一半 → 窗高 400
        let mut depths = vec![0, 0];
        depths.extend(std::iter::repeat_n(1, 40));
        depths.extend([0, 0]);
        let mut folds = FakeFolds {
            folding: vec![1],
            progress: 0.5,
            naturals: Vec::new(),
        };
        let layout = RowLayout::measure(&depths, H, &mut folds);
        assert_eq!(folds.naturals, vec![(1, 40.0 * H)], "自然高按子树行数算");
        assert_eq!(layout.height(), 2.0 * H + 400.0 + 2.0 * H);

        // 可视区 [0, 200):表头两行 + 窗里前 8 行,其余折成空白
        let slots = layout.slots(0.0, 200.0, None);
        let Some(Slot::Fold {
            header,
            clip,
            slots: inner,
        }) = slots.get(2)
        else {
            panic!("{slots:?}");
        };
        assert_eq!((*header, *clip), (1, 400.0));
        assert_eq!(rows(inner), (2..10).collect::<Vec<_>>());
        assert_eq!(total(inner), 40.0 * H, "窗里各项总高 = 子树自然高");
        assert_eq!(total(&slots), layout.height());
        // 窗后面的两行在可视区外
        assert_eq!(slots.last(), Some(&Slot::Gap(2.0 * H)));
    }

    #[test]
    fn 窗底多带一行_窗边缘不露白() {
        // 子树 10 行,窗高 2.5 行:看得见 0..2.5 行,第 3 行(下标 4)压在窗底外一行之内
        let mut depths = vec![0];
        depths.extend(std::iter::repeat_n(1, 10));
        let mut folds = FakeFolds {
            folding: vec![0],
            progress: 0.25,
            naturals: Vec::new(),
        };
        let layout = RowLayout::measure(&depths, H, &mut folds);
        let slots = layout.slots(0.0, 1000.0, None);
        let Some(Slot::Fold { slots: inner, .. }) = slots.get(1) else {
            panic!("{slots:?}");
        };
        assert_eq!(rows(inner), vec![1, 2, 3, 4]);
        assert_eq!(total(inner), 10.0 * H);
    }

    #[test]
    fn 整块在屏外的折叠动画只占一段空白() {
        let mut depths = vec![0; 30];
        depths.insert(21, 1);
        depths.insert(22, 1);
        let mut folds = FakeFolds {
            folding: vec![20],
            progress: 0.25,
            naturals: Vec::new(),
        };
        let layout = RowLayout::measure(&depths, H, &mut folds);
        let slots = layout.slots(0.0, 100.0, None);
        assert!(
            !slots.iter().any(|s| matches!(s, Slot::Fold { .. })),
            "{slots:?}"
        );
        assert_eq!(rows(&slots), vec![0, 1, 2, 3, 4]);
        assert_eq!(total(&slots), layout.height());
    }

    #[test]
    fn 裁掉的子行里拿着焦点的那一行照建() {
        // 子树 10 行,窗只开了 2 行高;焦点在子树第 8 行(被窗裁掉看不见,但要留在树上)
        let mut depths = vec![0];
        depths.extend(std::iter::repeat_n(1, 10));
        let mut folds = FakeFolds {
            folding: vec![0],
            progress: 0.2,
            naturals: Vec::new(),
        };
        let layout = RowLayout::measure(&depths, H, &mut folds);
        let slots = layout.slots(0.0, 1000.0, Some(8));
        let Some(Slot::Fold { slots: inner, .. }) = slots.get(1) else {
            panic!("{slots:?}");
        };
        let built = rows(inner);
        assert!(built.contains(&8), "{built:?}");
        assert_eq!(total(inner), 10.0 * H);
    }

    #[test]
    fn 嵌套折叠动画_内层窗高计入外层自然高() {
        // 0 ─ 1 ─ 2..=5(4 行)  外层 0 与内层 1 同时在动,进度一半
        let depths = vec![0, 1, 2, 2, 2, 2, 1, 0];
        let mut folds = FakeFolds {
            folding: vec![0, 1],
            progress: 0.5,
            naturals: Vec::new(),
        };
        let layout = RowLayout::measure(&depths, H, &mut folds);
        // 内层自然高 4 行 → 窗 2 行;外层自然高 = 行 1 + 内层窗 + 行 6 = 4 行 → 窗 2 行
        assert_eq!(folds.naturals, vec![(1, 4.0 * H), (0, 4.0 * H)]);
        assert_eq!(layout.height(), H + 2.0 * H + H);
        let slots = layout.slots(0.0, 1000.0, None);
        assert_eq!(total(&slots), layout.height());
    }

    #[test]
    fn 子树为空的目录不当折叠项() {
        let depths = vec![0, 0, 0];
        let mut folds = FakeFolds {
            folding: vec![1],
            progress: 0.5,
            naturals: Vec::new(),
        };
        let layout = RowLayout::measure(&depths, H, &mut folds);
        assert!(folds.naturals.is_empty());
        assert_eq!(layout.height(), 3.0 * H);
    }

    #[test]
    fn 滚进可视区取最少的滚动() {
        // 视口 100,当前顶 200:第 10 行 [200,220) 已在里面
        assert_eq!(reveal_scroll_top(10, H, 200.0, 100.0), None);
        // 在上面:贴顶
        assert_eq!(reveal_scroll_top(5, H, 200.0, 100.0), Some(100.0));
        // 在下面:贴底(第 20 行底 = 420 → 顶 320)
        assert_eq!(reveal_scroll_top(20, H, 200.0, 100.0), Some(320.0));
        // 压在下沿上(第 14 行 [280,300) 恰好贴底)不动
        assert_eq!(reveal_scroll_top(14, H, 200.0, 100.0), None);
        // 比视口还高的行贴顶
        assert_eq!(reveal_scroll_top(3, 50.0, 0.0, 30.0), Some(150.0));
    }
}
