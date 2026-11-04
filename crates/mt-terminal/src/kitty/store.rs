//! 每终端一份的图片与摆放账本,外加预算。不碰 grid —— 哪些摆放还活着、哪些看得见,
//! 由调用方扫完锚([`super::anchor`])告诉它。
//!
//! # 淘汰顺序
//!
//! 超预算时一张张往外请,先后是:屏外的优先于屏上的,没摆放的优先于有摆放的,最久没
//! 用过的优先。刚传上来的那张([`Store::enforce`] 的 `protect`)不动。淘汰一张图连它
//! 的摆放一起撤,grid 里留下的锚对不上号,绘制时自然跳过。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::decode::KittyBitmap;

/// 预算。常量在 [`Budget::DEFAULT`],单测给小值。
#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    /// 每终端位图总字节。
    pub bytes: u64,
    /// 每终端图片张数:防一堆 1×1 小图把账本撑大。
    pub images: usize,
    /// 每终端摆放个数。
    pub placements: usize,
}

impl Budget {
    pub(super) const DEFAULT: Budget = Budget {
        bytes: 256 * 1024 * 1024,
        images: 1024,
        placements: 2048,
    };
}

/// 程序没给 id 的图从这里往上分。协议里的 id 是 u32,撞不上。
const INTERNAL_ID_BASE: u64 = 1 << 32;

/// 源图裁切矩形,单位是原图像素(协议里 `x` `y` `w` `h` 的口径)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 一次摆放。键是它的序号,grid 里的锚认的就是这个序号。
#[derive(Clone, Debug)]
pub(super) struct Placement {
    pub image: u64,
    /// 协议里的 `p`。0 = 没给,同一张图可以摆很多次。
    pub placement_id: u32,
    pub columns: u32,
    pub rows: u32,
    pub source: SourceRect,
    /// 摆在备用屏上。两块屏的锚各在各的 grid 里,扫一块屏不能判另一块屏的死刑。
    pub on_alt: bool,
}

struct Entry {
    bitmap: Arc<KittyBitmap>,
    last_used: u64,
}

pub(super) struct Store {
    budget: Budget,
    images: HashMap<u64, Entry>,
    placements: HashMap<u64, Placement>,
    bytes: u64,
    clock: u64,
    next_internal_id: u64,
    /// 位图集合的代数:图进出一次 +1。渲染层看它变了才去核对纹理缓存。
    epoch: u64,
}

impl Store {
    pub(super) fn new(budget: Budget) -> Self {
        Self {
            budget,
            images: HashMap::new(),
            placements: HashMap::new(),
            bytes: 0,
            clock: 0,
            next_internal_id: INTERNAL_ID_BASE,
            epoch: 0,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    pub(super) fn budget(&self) -> Budget {
        self.budget
    }

    #[cfg(test)]
    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }

    #[cfg(test)]
    pub(super) fn image_count(&self) -> usize {
        self.images.len()
    }

    pub(super) fn placement_count(&self) -> usize {
        self.placements.len()
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 现有摆放里最高的那张占几行。扫锚时要往视口上方多看这么多行。
    pub(super) fn tallest(&self) -> u32 {
        self.placements
            .values()
            .map(|placement| placement.rows)
            .max()
            .unwrap_or(0)
    }

    pub(super) fn allocate_internal_id(&mut self) -> u64 {
        let id = self.next_internal_id;
        self.next_internal_id += 1;
        id
    }

    /// 存一张图。**同 id 重传**:规范要求旧图连摆放一起删掉,新图要等新的摆放才显示。
    pub(super) fn insert_image(&mut self, id: u64, bitmap: Arc<KittyBitmap>) {
        self.remove_image(id);
        self.bytes += bitmap.byte_len();
        let last_used = self.tick();
        self.images.insert(id, Entry { bitmap, last_used });
        self.epoch += 1;
    }

    /// 取一张图,顺手记一次使用。
    pub(super) fn bitmap(&mut self, id: u64) -> Option<Arc<KittyBitmap>> {
        let now = self.tick();
        let entry = self.images.get_mut(&id)?;
        entry.last_used = now;
        Some(entry.bitmap.clone())
    }

    /// 给定摆放序号,取它和它的位图。锚对不上号(摆放已删、图已淘汰)= `None`。
    pub(super) fn resolve(&self, seq: u64) -> Option<(&Placement, &Arc<KittyBitmap>)> {
        let placement = self.placements.get(&seq)?;
        let entry = self.images.get(&placement.image)?;
        Some((placement, &entry.bitmap))
    }

    /// 记一次摆放。同一张图、同一个非 0 的 `p` 再摆一次是替换(规范);`p` 为 0 就是又多一处。
    pub(super) fn add_placement(&mut self, seq: u64, placement: Placement) {
        if placement.placement_id != 0 {
            self.placements.retain(|_, old| {
                old.image != placement.image || old.placement_id != placement.placement_id
            });
        }
        if let Some(entry) = self.images.get_mut(&placement.image) {
            self.clock += 1;
            entry.last_used = self.clock;
        }
        self.placements.insert(seq, placement);
    }

    /// `d=i` / `d=I`:按图片 id 删。给了 `p` 就只删那一处。
    /// `free`(大写)再把没人引用的图放掉。**id 不存在不算错**,与 kitty 同样静默。
    pub(super) fn delete_by_image(&mut self, id: u64, placement_id: Option<u32>, free: bool) {
        self.placements.retain(|_, placement| {
            placement.image != id || placement_id.is_some_and(|p| p != placement.placement_id)
        });
        if free && !self.is_referenced(id) {
            self.remove_image(id);
        }
    }

    /// `d=a` / `d=A`:删一批摆放(看得见的那些,调用方扫出来的)。
    pub(super) fn delete_placements(&mut self, seqs: &[u64], free: bool) {
        let mut touched: Vec<u64> = Vec::new();
        for seq in seqs {
            if let Some(placement) = self.placements.remove(seq) {
                touched.push(placement.image);
            }
        }
        if free {
            touched.sort_unstable();
            touched.dedup();
            for id in touched {
                if !self.is_referenced(id) {
                    self.remove_image(id);
                }
            }
        }
    }

    /// RIS:全清。
    pub(super) fn clear(&mut self) {
        if !self.images.is_empty() {
            self.epoch += 1;
        }
        self.images.clear();
        self.placements.clear();
        self.bytes = 0;
    }

    /// 撤掉锚已经不在 grid 里的摆放。`live` 是当前屏(`on_alt`)上扫到的全部序号;
    /// 另一块屏上的摆放不在这次扫描的范围里,不动。
    pub(super) fn sweep_dead(&mut self, on_alt: bool, live: &HashSet<u64>) {
        self.placements
            .retain(|seq, placement| placement.on_alt != on_alt || live.contains(seq));
    }

    /// 摆放个数是不是超了。
    pub(super) fn too_many_placements(&self) -> bool {
        self.placements.len() > self.budget.placements
    }

    /// 把摆放个数压回预算内:先撤看不见的,再撤看得见的,都是从最早的撤起。
    pub(super) fn trim_placements(&mut self, visible: &HashSet<u64>) {
        let excess = self.placements.len().saturating_sub(self.budget.placements);
        if excess == 0 {
            return;
        }
        let mut order: Vec<(bool, u64)> = self
            .placements
            .keys()
            .map(|seq| (visible.contains(seq), *seq))
            .collect();
        order.sort_unstable();
        for (_, seq) in order.into_iter().take(excess) {
            self.placements.remove(&seq);
        }
    }

    /// 字节数或张数超了没有。
    pub(super) fn over_budget(&self) -> bool {
        self.bytes > self.budget.bytes || self.images.len() > self.budget.images
    }

    /// 超预算就按模块注释的顺序往外请,`protect` 不动。
    /// `visible` 是这一刻屏上看得见的图片 id。
    pub(super) fn enforce(&mut self, visible: &HashSet<u64>, protect: Option<u64>) {
        while self.over_budget() {
            let placed: HashSet<u64> = self.placements.values().map(|p| p.image).collect();
            let victim = self
                .images
                .iter()
                .filter(|(id, _)| Some(**id) != protect)
                .min_by_key(|(id, entry)| {
                    (visible.contains(id), placed.contains(id), entry.last_used)
                })
                .map(|(id, _)| *id);
            let Some(id) = victim else {
                return;
            };
            self.remove_image(id);
        }
    }

    /// 还活着的位图的 uid。
    pub(super) fn live_uids(&self) -> HashSet<u64> {
        self.images
            .values()
            .map(|entry| entry.bitmap.uid())
            .collect()
    }

    fn is_referenced(&self, id: u64) -> bool {
        self.placements
            .values()
            .any(|placement| placement.image == id)
    }

    /// 放掉一张图,连它的摆放一起。
    fn remove_image(&mut self, id: u64) {
        self.placements.retain(|_, placement| placement.image != id);
        if let Some(entry) = self.images.remove(&id) {
            self.bytes -= entry.bitmap.byte_len();
            self.epoch += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::decode::{decode_png, encode_png};
    use super::*;

    fn bitmap(side: u32) -> Arc<KittyBitmap> {
        let pixels = vec![255u8; (side * side * 4) as usize];
        decode_png(&encode_png(side, side, &pixels), |size| size).expect("解码")
    }

    fn placement(image: u64, placement_id: u32) -> Placement {
        Placement {
            image,
            placement_id,
            columns: 4,
            rows: 2,
            source: SourceRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            on_alt: false,
        }
    }

    fn store() -> Store {
        Store::new(Budget::DEFAULT)
    }

    #[test]
    fn 重传替换旧图并撤掉它的摆放() {
        let mut s = store();
        s.insert_image(1, bitmap(2));
        s.add_placement(10, placement(1, 0));
        assert!(s.resolve(10).is_some());
        let epoch = s.epoch();

        s.insert_image(1, bitmap(4));
        assert_eq!(s.bytes(), 4 * 4 * 4, "只算新图");
        assert_eq!(s.image_count(), 1);
        assert_eq!(s.placement_count(), 0, "新图要等新的摆放才显示");
        assert!(s.resolve(10).is_none());
        assert!(s.epoch() > epoch);
    }

    #[test]
    fn 同_p_替换_无_p_累加() {
        let mut s = store();
        s.insert_image(1, bitmap(1));
        s.add_placement(10, placement(1, 0));
        s.add_placement(11, placement(1, 0));
        assert_eq!(s.placement_count(), 2, "没给 p 就是又多一处");
        s.add_placement(12, placement(1, 7));
        s.add_placement(13, placement(1, 7));
        assert_eq!(s.placement_count(), 3, "同 p 替换");
        assert!(s.resolve(12).is_none());
        assert!(s.resolve(13).is_some());
    }

    /// 大小写语义:小写只撤摆放、留数据;大写把没人引用的数据也放掉。
    #[test]
    fn 按图片删的大小写语义() {
        let mut s = store();
        s.insert_image(1, bitmap(1));
        s.add_placement(10, placement(1, 0));
        s.delete_by_image(1, None, false);
        assert_eq!(s.placement_count(), 0);
        assert_eq!(s.image_count(), 1, "d=i 留着数据,a=p 还能再摆");

        s.add_placement(11, placement(1, 0));
        s.delete_by_image(1, None, true);
        assert_eq!(s.image_count(), 0, "d=I 放掉数据");
        assert_eq!(s.bytes(), 0);
        s.delete_by_image(999, None, true); // 不存在的 id 静默
    }

    #[test]
    fn 按图片删时给了_p_只删那一处() {
        let mut s = store();
        s.insert_image(1, bitmap(1));
        s.add_placement(10, placement(1, 5));
        s.add_placement(11, placement(1, 6));
        s.delete_by_image(1, Some(5), true);
        assert!(s.resolve(10).is_none());
        assert!(s.resolve(11).is_some());
        assert_eq!(s.image_count(), 1, "还有人引用,数据不放");
        s.delete_by_image(1, Some(6), true);
        assert_eq!(s.image_count(), 0);
    }

    /// `d=A`:撤掉看得见的,回看里还引用着的图不放。
    #[test]
    fn 删一批摆放时还被引用的图不放() {
        let mut s = store();
        s.insert_image(1, bitmap(1));
        s.insert_image(2, bitmap(1));
        s.add_placement(10, placement(1, 0));
        s.add_placement(11, placement(2, 0));
        s.add_placement(12, placement(2, 0)); // 假装这一处在回看里
        s.delete_placements(&[10, 11], true);
        assert_eq!(s.image_count(), 1);
        assert!(s.resolve(12).is_some());

        s.delete_placements(&[12], false);
        assert_eq!(s.image_count(), 1, "d=a 留数据");
    }

    /// 预算顺序:没摆放 → 屏外有摆放 → 屏上;同档按最久没用。
    #[test]
    fn 超预算时按顺序淘汰() {
        let mut s = Store::new(Budget {
            images: 3,
            ..Budget::DEFAULT
        });
        s.insert_image(1, bitmap(1));
        s.insert_image(2, bitmap(1));
        s.insert_image(3, bitmap(1));
        s.add_placement(10, placement(1, 0)); // 1:屏上
        s.add_placement(11, placement(2, 0)); // 2:屏外
        // 3:没摆放
        let mut visible = HashSet::from([1]);

        s.insert_image(4, bitmap(1));
        s.enforce(&visible, Some(4));
        assert!(s.bitmap(3).is_none(), "先请没摆放的");

        s.add_placement(12, placement(4, 0));
        visible.insert(4);
        s.insert_image(5, bitmap(1));
        s.enforce(&visible, Some(5));
        assert!(s.bitmap(2).is_none(), "再请屏外的");

        s.add_placement(13, placement(5, 0));
        visible.insert(5);
        s.insert_image(6, bitmap(1));
        s.enforce(&visible, Some(6));
        assert!(s.bitmap(1).is_none(), "都在屏上就请最久没用的");
        assert!(s.bitmap(4).is_some());
        assert!(s.bitmap(5).is_some());
        assert!(s.bitmap(6).is_some());
    }

    /// 屏上的也挡不住预算:实在没别的可请就从最旧的屏上图请起,但刚传的不动。
    #[test]
    fn 全在屏上时也压回预算且保住新图() {
        let mut s = Store::new(Budget {
            bytes: 16,
            ..Budget::DEFAULT
        });
        s.insert_image(1, bitmap(2));
        s.add_placement(10, placement(1, 0));
        s.insert_image(2, bitmap(2));
        s.add_placement(11, placement(2, 0));
        let visible = HashSet::from([1, 2]);
        s.enforce(&visible, Some(2));
        assert!(s.bitmap(1).is_none());
        assert!(s.bitmap(2).is_some());
        assert!(s.resolve(10).is_none(), "摆放跟着走");
        assert_eq!(s.bytes(), 16);
    }

    /// 单张就超预算:不能死循环。
    #[test]
    fn 只剩保护对象时停手() {
        let mut s = Store::new(Budget {
            bytes: 1,
            ..Budget::DEFAULT
        });
        s.insert_image(1, bitmap(2));
        s.enforce(&HashSet::new(), Some(1));
        assert!(s.bitmap(1).is_some());
    }

    /// 一堆 1×1 的小图撑不爆张数。
    #[test]
    fn 张数也有上限() {
        let mut s = Store::new(Budget {
            images: 3,
            ..Budget::DEFAULT
        });
        for id in 1..=10 {
            s.insert_image(id, bitmap(1));
            s.enforce(&HashSet::new(), Some(id));
        }
        assert_eq!(s.image_count(), 3);
        assert!(s.bitmap(10).is_some());
        assert!(s.bitmap(1).is_none());
    }

    #[test]
    fn 摆放个数压回预算且先撤看不见的() {
        let mut s = Store::new(Budget {
            placements: 2,
            ..Budget::DEFAULT
        });
        s.insert_image(1, bitmap(1));
        for seq in 10..14 {
            s.add_placement(seq, placement(1, 0));
        }
        assert!(s.too_many_placements());
        s.trim_placements(&HashSet::from([10]));
        assert_eq!(s.placement_count(), 2);
        assert!(s.resolve(10).is_some(), "看得见的留下,哪怕它最早");
        assert!(s.resolve(13).is_some());
    }

    /// 只判当前屏:备用屏上的摆放不在这次扫描范围里,不能错杀。
    #[test]
    fn 清死摆放只看当前屏() {
        let mut s = store();
        s.insert_image(1, bitmap(1));
        s.add_placement(10, placement(1, 0));
        s.add_placement(
            11,
            Placement {
                on_alt: true,
                ..placement(1, 0)
            },
        );
        s.add_placement(12, placement(1, 0));
        s.sweep_dead(false, &HashSet::from([12]));
        assert!(s.resolve(10).is_none(), "主屏上扫不到,是死的");
        assert!(s.resolve(11).is_some(), "备用屏上的不动");
        assert!(s.resolve(12).is_some());
    }

    #[test]
    fn 全清() {
        let mut s = store();
        s.insert_image(1, bitmap(1));
        s.add_placement(10, placement(1, 0));
        let epoch = s.epoch();
        s.clear();
        assert_eq!((s.image_count(), s.placement_count(), s.bytes()), (0, 0, 0));
        assert!(s.epoch() > epoch);
        assert!(s.live_uids().is_empty());
    }

    #[test]
    fn 内部_id_不与协议_id_撞() {
        let mut s = store();
        let a = s.allocate_internal_id();
        let b = s.allocate_internal_id();
        assert!(a > u64::from(u32::MAX));
        assert_ne!(a, b);
    }

    #[test]
    fn 最高的摆放决定往上扫多少行() {
        let mut s = store();
        assert_eq!(s.tallest(), 0);
        s.insert_image(1, bitmap(1));
        s.add_placement(10, placement(1, 0));
        s.add_placement(
            11,
            Placement {
                rows: 9,
                ..placement(1, 0)
            },
        );
        assert_eq!(s.tallest(), 9);
    }
}
