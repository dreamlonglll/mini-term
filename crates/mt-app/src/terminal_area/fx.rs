//! 终端区「布局变了之后补位」的两种动效(FLIP 一族),[`super`] 的子模块。
//!
//! 1. **tab chip 位移补间**([`flip_slot`]):新建 / 关闭 / 拖拽落位(以及键盘左右移)让
//!    页签区里的 chip 换了位置时,被挤动的 chip 从旧 x 滑到新 x,新出现的 chip 从自己的
//!    左缘往右裁剪展开 —— 零尺寸变化,chip 从第一帧起就按终位排版。
//! 2. **关分屏后邻居扩张**([`leaf_slot`]):邻居一开始就按终尺寸排版(PTY resize 一次,
//!    不可避免),只是被一个裁剪窗从它上一帧的格子矩形长到新矩形;内容左上角钉在裁剪窗
//!    左上角上,沿分割轴跟着长出来的那条边走 —— 零逐帧 resize。
//!
//! # 为什么是自定义元素:先跳再滑的那一帧
//!
//! FLIP 的难点是「布局变了的**那一帧**」:新位置要到 prepaint 才知道,而元素树在 render
//! 里早就建完了。按上一帧量到的位置去补,第一帧 chip 已经画在新位置(先跳一下),下一帧
//! 才拉回旧位置开始滑。这里的两个包装元素在**自己的 prepaint** 里拿到本帧的终位,与上一
//! 帧比出位移,当场用 `with_element_offset`(与滚动容器挪子元素同一个口子)把子树挪回
//! 旧位置 —— 变化的那一帧就已经画在旧位置,没有先跳的那一下。偏移只作用于子树的
//! prepaint(命中框跟着画面走),**不改布局**。
//!
//! # 三条约束(评审口径)
//!
//! - **偏移不写进 `tab_rects` / `pane_rects`**:拖拽插入线、方向导航、悬停卡锚点、最大化
//!   的起点矩形都读它们。量矩形由 [`FlipSlot`] 自己在挪动之前报(`on_bounds`),报的是排版
//!   位置;祖先 [`LeafSlot`] 在扩张中挪了整格的,两处量尺都减掉它的偏移
//!   ([`LayoutFx::unshift`]);
//! - **不和 `scroll_to_item` 打架**:chip 的位置按「相对页签区」算(页签区的
//!   `ScrollHandle` 在子元素 prepaint 之前就记好了每个子项未滚动的矩形),横向滚动、
//!   跟随滚动、整条 tab 栏随窗口挪位都不算挪动;
//! - **只在增删 / 重排的那一帧起补间**:render 里比对每组的页签顺序,变了才「上膛」
//!   ([`LayoutFx::arm_tab_strip`])。标题变长把后面的 chip 挤开、拖拽进行中这类挪位
//!   一律不补间(拖拽中布局本来就不动,只在落位那一刻起补间)。
//!
//! 时长曲线取 [`mt_ui::motion::LIST_REFLOW`] / [`mt_ui::motion::AREA_SWAP`],服从「启用动画」
//! 总开关(关掉时进度直达终点),豁免系统「减少动画」。只在动画进行中要帧。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AnyElement, App, Bounds, ContentMask, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, ScrollHandle, Window, point, px, size,
};
use mt_ui::motion::{AREA_SWAP, LIST_REFLOW};

use crate::tree::PaneState;

/// 挪动不到这么多像素不算挪(浮点 / 像素对齐的抖动)。
const MOVE_EPSILON: f32 = 0.5;

/// 视图上挂一份、所有包装元素共用的状态。
pub(super) type SharedFx = Rc<RefCell<LayoutFx>>;

/// 一个矩形(像素,f32,窗口坐标)。插值在纯数里做,便于单测。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Rect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    fn from_bounds(bounds: Bounds<Pixels>) -> Self {
        Self {
            left: f32::from(bounds.origin.x),
            top: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        }
    }

    fn to_bounds(self) -> Bounds<Pixels> {
        Bounds::new(
            point(px(self.left), px(self.top)),
            size(px(self.width.max(0.0)), px(self.height.max(0.0))),
        )
    }

    fn right(&self) -> f32 {
        self.left + self.width
    }

    fn bottom(&self) -> f32 {
        self.top + self.height
    }

    /// `self` 罩得住 `inner`(容差 [`MOVE_EPSILON`])。
    fn contains(&self, inner: &Rect) -> bool {
        inner.left >= self.left - MOVE_EPSILON
            && inner.top >= self.top - MOVE_EPSILON
            && inner.right() <= self.right() + MOVE_EPSILON
            && inner.bottom() <= self.bottom() + MOVE_EPSILON
    }

    fn differs(&self, other: &Rect) -> bool {
        (self.left - other.left).abs() >= MOVE_EPSILON
            || (self.top - other.top).abs() >= MOVE_EPSILON
            || (self.width - other.width).abs() >= MOVE_EPSILON
            || (self.height - other.height).abs() >= MOVE_EPSILON
    }
}

// ─── 纯数 ────────────────────────────────────────────────────

/// 位移补间这一帧的偏移:起步偏移 `from` 按已缓动进度收到 0,终点精确归零
/// (残留零点几像素会让 chip 永久错位)。
pub(super) fn slide_offset(from: f32, progress: f32) -> f32 {
    let p = progress.clamp(0.0, 1.0);
    if p >= 1.0 { 0.0 } else { from * (1.0 - p) }
}

/// 布局把一个 chip 从 `old_x` 挪到 `new_x` 那一刻的起步偏移:沿用它这一帧正在画的偏移
/// `current`,再补上布局的位移 —— 屏幕上原地不动,接着滑向新位置(半路再挪也不跳)。
pub(super) fn reflow_from(current: f32, old_x: f32, new_x: f32) -> f32 {
    current + old_x - new_x
}

/// 关分屏后邻居扩张这一帧的 `(裁剪窗, 内容偏移)`:裁剪窗从旧格子 `from` 线性长到新格子
/// `to`(进度已缓动),内容按终尺寸排在 `to`,左上角钉在裁剪窗左上角 —— 偏移 =
/// 裁剪窗原点 − `to` 原点,终点精确归零。
pub(super) fn grow_clip(from: Rect, to: Rect, progress: f32) -> (Rect, (f32, f32)) {
    let p = progress.clamp(0.0, 1.0);
    if p >= 1.0 {
        return (to, (0.0, 0.0));
    }
    let lerp = |a: f32, b: f32| a + (b - a) * p;
    let clip = Rect {
        left: lerp(from.left, to.left),
        top: lerp(from.top, to.top),
        width: lerp(from.width, to.width),
        height: lerp(from.height, to.height),
    };
    (clip, (clip.left - to.left, clip.top - to.top))
}

/// 新出现的 chip 裁剪展开这一帧的裁剪窗:从 chip 左缘往右长到全宽。
///
/// 原点跟着内容的位移走(`offset` = 本帧画在排版位置右侧多少):展开还没播完时又被挤动
/// (紧接着又新建了一个排在它前面的页签),内容画在「排版位置 + 偏移」上,裁剪窗按排版
/// 位置放就会错位裁掉一截。
pub(super) fn reveal_clip(layout: Rect, offset: f32, progress: f32) -> Rect {
    Rect {
        left: layout.left + offset,
        top: layout.top,
        width: layout.width * progress.clamp(0.0, 1.0),
        height: layout.height,
    }
}

/// 把子树里量到的矩形换回排版位置:减掉祖先 [`LeafSlot`] 这一帧的内容偏移(见
/// [`LayoutFx::leaf_offset`])。
pub(super) fn unshift(bounds: Bounds<Pixels>, (dx, dy): (f32, f32)) -> Bounds<Pixels> {
    Bounds::new(
        point(bounds.origin.x - px(dx), bounds.origin.y - px(dy)),
        bounds.size,
    )
}

// ─── 状态 ────────────────────────────────────────────────────

/// 一个 chip 的补位记录。
struct FlipEntry {
    /// 上一帧相对页签区左缘的 x
    last_x: f32,
    /// 位移补间:起步偏移 + 起始时刻
    slide: Option<(f32, Instant)>,
    /// 新出现的 chip 的裁剪展开起始时刻
    reveal: Option<Instant>,
    seen: u64,
}

impl FlipEntry {
    fn offset_at(&self, now: Instant) -> f32 {
        self.slide.map_or(0.0, |(from, start)| {
            slide_offset(
                from,
                LIST_REFLOW.progress_live(now.saturating_duration_since(start)),
            )
        })
    }
}

/// [`FlipSlot`] 这一帧怎么画。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FlipFrame {
    /// 横向偏移(像素)
    pub offset: f32,
    /// 裁剪展开进度(`None` = 不裁)
    pub reveal: Option<f32>,
    /// 还在动(要下一帧)
    pub running: bool,
}

/// 关分屏后邻居扩张的记录。
struct GrowEntry {
    from: Rect,
    start: Instant,
}

/// 终端区共用的补位状态(见模块注释)。
#[derive(Default)]
pub(super) struct LayoutFx {
    /// 帧号:每帧 render 开头 +1。条目记着最后一次被画到的帧号,上一帧没画到的
    /// (组被最大化藏起、项目 / 面板切走)下一帧开头扔掉 —— 再出现时按「首次出现」处理
    epoch: u64,
    /// chip 的记录,key = `叶子\u{1}pane`(「+」钮是 `叶子\u{1}+`):同一个 pane 拖去
    /// 别的组算新 chip,位移不跨组比
    chips: HashMap<String, FlipEntry>,
    /// 每组上一帧的页签顺序 + 帧号
    orders: HashMap<String, (Vec<String>, u64)>,
    /// 每个叶子上一帧画在哪(窗口坐标,不含本机制的偏移)+ 帧号
    leaves: HashMap<String, (Rect, u64)>,
    /// 正在扩张的叶子
    growing: HashMap<String, GrowEntry>,
    /// 这一帧刚关掉分屏:剩下的叶子该从上一帧的矩形长到新矩形
    grow_armed: bool,
    /// 正在 prepaint 的那一格被 [`LeafSlot`] 挪了多少(只在扩张中的叶子子树 prepaint 期间
    /// 非零)。子树里报排版矩形的几处([`FlipSlot::on_bounds`]、`render_leaf` 的 pane 量尺)
    /// 拿它减回去([`Self::unshift`]),扩张那 240ms 里 `tab_rects` / `pane_rects` 照样是
    /// 排版位置 —— 否则此间换卡,卡锚偏到旧格子的左缘并停在那里;双击最大化,Reveal 的
    /// 起点也取错
    leaf_offset: (f32, f32),
}

impl LayoutFx {
    /// 每帧 render **开头**调一次(早退路径也要调):扔掉上一帧没画到的条目,进入新一帧,
    /// 并把「关分屏」的扳机复位。
    pub fn begin_frame(&mut self) {
        let prev = self.epoch;
        self.chips.retain(|_, entry| entry.seen == prev);
        self.orders.retain(|_, (_, seen)| *seen == prev);
        self.leaves.retain(|_, (_, seen)| *seen == prev);
        let leaves = &self.leaves;
        self.growing.retain(|leaf, _| leaves.contains_key(leaf));
        self.epoch = prev.wrapping_add(1);
        self.grow_armed = false;
    }

    /// render 里报这一帧某组的页签顺序;返回这一帧该不该补位 = 这组上一帧也在画、
    /// 且顺序变了(增 / 删 / 重排)。
    pub fn arm_tab_strip(&mut self, leaf: &str, panes: &[PaneState]) -> bool {
        let epoch = self.epoch;
        match self.orders.get_mut(leaf) {
            Some((order, seen)) => {
                *seen = epoch;
                let same = order.len() == panes.len()
                    && order.iter().zip(panes).all(|(id, pane)| *id == pane.id);
                if !same {
                    *order = panes.iter().map(|p| p.id.clone()).collect();
                }
                !same
            }
            None => {
                let order = panes.iter().map(|p| p.id.clone()).collect();
                self.orders.insert(leaf.to_string(), (order, epoch));
                false
            }
        }
    }

    /// render 里报:这一帧是不是刚关掉了分屏(同一面板、叶子只少不多)。
    pub fn arm_split_grow(&mut self, armed: bool) {
        self.grow_armed = armed;
    }

    /// 子树里量到的矩形换回排版位置(见 [`Self::leaf_offset`])。只在 prepaint 里调。
    pub fn unshift(&self, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        unshift(bounds, self.leaf_offset)
    }

    /// [`FlipSlot`] 的 prepaint 里调:`rel_x` 是 chip 本帧相对页签区左缘的排版位置。
    fn flip(&mut self, key: &str, rel_x: f32, armed: bool, now: Instant) -> FlipFrame {
        let epoch = self.epoch;
        if let Some(entry) = self.chips.get_mut(key) {
            entry.seen = epoch;
            if (rel_x - entry.last_x).abs() >= MOVE_EPSILON {
                // 增删 / 重排挤动的:从旧位置滑过来;别的原因挪的(标题变长、
                // 页签区宽度变了)直接到位,在飞的也收掉
                entry.slide =
                    armed.then(|| (reflow_from(entry.offset_at(now), entry.last_x, rel_x), now));
                entry.last_x = rel_x;
            }
        } else {
            self.chips.insert(
                key.to_string(),
                FlipEntry {
                    last_x: rel_x,
                    slide: None,
                    // 组上一帧就在画、这一帧多出来的 chip(新建 / 拖进来的):裁剪展开
                    reveal: armed.then_some(now),
                    seen: epoch,
                },
            );
        }
        let Some(entry) = self.chips.get_mut(key) else {
            return FlipFrame {
                offset: 0.0,
                reveal: None,
                running: false,
            };
        };
        let offset = entry.offset_at(now);
        let sliding = entry.slide.is_some_and(|(_, start)| {
            LIST_REFLOW.running_live(now.saturating_duration_since(start))
        });
        if !sliding {
            entry.slide = None;
        }
        let reveal = entry.reveal.and_then(|start| {
            let elapsed = now.saturating_duration_since(start);
            LIST_REFLOW
                .running_live(elapsed)
                .then(|| LIST_REFLOW.progress_live(elapsed))
        });
        if reveal.is_none() {
            entry.reveal = None;
        }
        FlipFrame {
            offset: if sliding { offset } else { 0.0 },
            reveal,
            running: sliding || reveal.is_some(),
        }
    }

    /// [`LeafSlot`] 的 prepaint 里调:`target` 是叶子本帧的排版矩形。返回这一帧的
    /// `(裁剪窗, 内容偏移)`;不在扩张中为 `None`。
    fn grow(&mut self, leaf: &str, target: Rect, now: Instant) -> Option<(Rect, (f32, f32))> {
        let epoch = self.epoch;
        if self.grow_armed
            && let Some((old, _)) = self.leaves.get(leaf)
            && old.differs(&target)
            // 只认「长大」:关分屏后剩下的格子只会罩住自己原来那一格
            && target.contains(old)
        {
            // 起点是**这一刻屏幕上的裁剪窗**:扩张途中又关掉一格(`Split_h[A, Split_v[B, C]]`
            // 关 C、B 还在往下长时再关 A),上一帧的排版矩形是那一场的终点,从它起步
            // 裁剪窗会先跳到整格再长
            let from = match self.growing.get(leaf) {
                Some(entry) => {
                    let elapsed = now.saturating_duration_since(entry.start);
                    grow_clip(entry.from, *old, AREA_SWAP.progress_live(elapsed)).0
                }
                None => *old,
            };
            self.growing
                .insert(leaf.to_string(), GrowEntry { from, start: now });
        }
        // 每帧每格都要记一次:有条目就就地改,免得每帧为键分配一次字符串
        match self.leaves.get_mut(leaf) {
            Some(slot) => *slot = (target, epoch),
            None => {
                self.leaves.insert(leaf.to_string(), (target, epoch));
            }
        }
        let entry = self.growing.get(leaf)?;
        let elapsed = now.saturating_duration_since(entry.start);
        if !AREA_SWAP.running_live(elapsed) {
            self.growing.remove(leaf);
            return None;
        }
        Some(grow_clip(
            entry.from,
            target,
            AREA_SWAP.progress_live(elapsed),
        ))
    }
}

// ─── tab chip 的包装元素 ─────────────────────────────────────

/// 每帧报一次排版矩形的回调(见 [`FlipSlot::on_bounds`])。
type BoundsReport = Box<dyn Fn(Bounds<Pixels>, &mut App)>;

/// 页签区里一颗 chip(或页签区外的「+」钮)的位移补间包装。见模块注释。
pub(super) struct FlipSlot {
    child: AnyElement,
    fx: SharedFx,
    key: String,
    /// 在页签区里排第几(取页签区记下的未滚动矩形);`None` = 页签区外面的「+」钮,
    /// 按自己的矩形相对页签区左缘算
    index: Option<usize>,
    strip: ScrollHandle,
    armed: bool,
    on_bounds: Option<BoundsReport>,
}

/// 把一颗 chip 包成带位移补间的。`armed` = 这一帧该不该补位([`LayoutFx::arm_tab_strip`])。
pub(super) fn flip_slot(
    child: impl IntoElement,
    fx: SharedFx,
    key: String,
    index: Option<usize>,
    strip: ScrollHandle,
    armed: bool,
) -> FlipSlot {
    FlipSlot {
        child: child.into_any_element(),
        fx,
        key,
        index,
        strip,
        armed,
        on_bounds: None,
    }
}

impl FlipSlot {
    /// 每帧报一次 chip 的**排版**矩形(挪动之前;窗口坐标,含页签区滚动)。
    pub fn on_bounds(mut self, f: impl Fn(Bounds<Pixels>, &mut App) + 'static) -> Self {
        self.on_bounds = Some(Box::new(f));
        self
    }
}

impl Element for FlipSlot {
    type RequestLayoutState = ();
    type PrepaintState = Option<ContentMask<Pixels>>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // 布局整个交给 chip:本元素就是它在 flex 行里的那一项
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if let Some(report) = &self.on_bounds {
            // 祖先 LeafSlot 在扩张中时减掉它的偏移,报的仍是排版位置
            let layout_bounds = self.fx.borrow().unshift(bounds);
            report(layout_bounds, cx);
        }
        let strip = self.strip.bounds();
        // 页签区还没排过版(宽 0):不记、不动
        let rel_x = (strip.size.width > px(0.0))
            .then(|| match self.index {
                Some(ix) => self.strip.bounds_for_item(ix).map(|item| item.origin.x),
                None => Some(bounds.origin.x),
            })
            .flatten()
            .map(|x| f32::from(x - strip.origin.x));
        let frame = rel_x.map(|rel_x| {
            self.fx
                .borrow_mut()
                .flip(&self.key, rel_x, self.armed, Instant::now())
        });
        let Some(frame) = frame else {
            self.child.prepaint(window, cx);
            return None;
        };
        if frame.running {
            window.request_animation_frame();
        }
        // 裁剪展开:从 chip 左缘往右长,原点跟着本帧的位移走(见 [`reveal_clip`])
        let mask = frame.reveal.map(|p| ContentMask {
            bounds: reveal_clip(Rect::from_bounds(bounds), frame.offset, p).to_bounds(),
        });
        window.with_content_mask(mask, |window| {
            window.with_element_offset(point(px(frame.offset), px(0.0)), |window| {
                self.child.prepaint(window, cx)
            });
        });
        mask
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        mask: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(*mask, |window| self.child.paint(window, cx));
    }
}

// ─── 叶子的包装元素 ──────────────────────────────────────────

/// 分屏树里一个叶子的「关分屏后扩张」包装。见模块注释。
pub(super) struct LeafSlot {
    child: AnyElement,
    fx: SharedFx,
    leaf: String,
}

pub(super) fn leaf_slot(child: AnyElement, fx: SharedFx, leaf: String) -> LeafSlot {
    LeafSlot { child, fx, leaf }
}

impl Element for LeafSlot {
    type RequestLayoutState = ();
    type PrepaintState = Option<ContentMask<Pixels>>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let grow = self
            .fx
            .borrow_mut()
            .grow(&self.leaf, Rect::from_bounds(bounds), Instant::now());
        let Some((clip, (dx, dy))) = grow else {
            self.child.prepaint(window, cx);
            return None;
        };
        window.request_animation_frame();
        let mask = Some(ContentMask {
            bounds: clip.to_bounds(),
        });
        // 子树 prepaint 期间登记偏移,子树里报排版矩形的几处据此减回去(见
        // [`LayoutFx::leaf_offset`]);叶子不嵌套,出来就清零
        self.fx.borrow_mut().leaf_offset = (dx, dy);
        window.with_content_mask(mask, |window| {
            window.with_element_offset(point(px(dx), px(dy)), |window| {
                self.child.prepaint(window, cx)
            });
        });
        self.fx.borrow_mut().leaf_offset = (0.0, 0.0);
        mask
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        mask: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(*mask, |window| self.child.paint(window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rect(left: f32, top: f32, width: f32, height: f32) -> Rect {
        Rect {
            left,
            top,
            width,
            height,
        }
    }

    #[test]
    fn 位移补间端点精确且半路再挪不跳() {
        assert_eq!(slide_offset(-120.0, 0.0), -120.0, "起点就是旧位置");
        assert_eq!(slide_offset(-120.0, 1.0), 0.0, "终点精确归零");
        assert_eq!(slide_offset(-120.0, 1.4), 0.0, "越界进度不许滑过头");
        assert_eq!(slide_offset(-120.0, -0.3), -120.0);
        assert!((slide_offset(80.0, 0.25) - 60.0).abs() < 1e-4);
        // 布局把 chip 从 200 挪到 100:起步偏移 +100,屏幕上仍在 200
        let from = reflow_from(0.0, 200.0, 100.0);
        assert_eq!(100.0 + from, 200.0);
        // 滑到一半(画在 150)时布局又把它挪到 60:新起步偏移让它仍画在 150
        let current = slide_offset(from, 0.5);
        assert_eq!(100.0 + current, 150.0);
        let again = reflow_from(current, 100.0, 60.0);
        assert_eq!(60.0 + again, 150.0, "半路再挪,视觉位置不跳");
    }

    #[test]
    fn 扩张裁剪窗从旧格子长到新格子且内容钉住左上角() {
        // 左右分屏关掉左边那格:右格 [500, 1000] 长到 [0, 1000]
        let (from, to) = (
            rect(500.0, 30.0, 500.0, 700.0),
            rect(0.0, 30.0, 1000.0, 700.0),
        );
        let (clip0, offset0) = grow_clip(from, to, 0.0);
        assert_eq!(clip0, from, "第一帧裁剪窗就是旧格子");
        assert_eq!(offset0, (500.0, 0.0), "内容左上角钉在旧格子左上角");
        let (clip1, offset1) = grow_clip(from, to, 1.0);
        assert_eq!(clip1, to);
        assert_eq!(offset1, (0.0, 0.0), "终点精确归零");
        assert_eq!(grow_clip(from, to, 2.0), (to, (0.0, 0.0)));
        // 全程裁剪窗单调长大、罩住旧格子、不出新格子;内容左上角始终贴着裁剪窗左上角
        let mut prev = clip0;
        for i in 1..=20 {
            let (clip, (dx, dy)) = grow_clip(from, to, i as f32 / 20.0);
            assert!(clip.width >= prev.width && clip.height >= prev.height);
            assert!(clip.contains(&from) && to.contains(&clip));
            assert!((to.left + dx - clip.left).abs() < 1e-3);
            assert!((to.top + dy - clip.top).abs() < 1e-3);
            prev = clip;
        }
        // 上下分屏关掉下面那格:上格只往下长,原点不动,内容零偏移
        let (from, to) = (rect(0.0, 0.0, 800.0, 300.0), rect(0.0, 0.0, 800.0, 600.0));
        for i in 0..=10 {
            let (_, offset) = grow_clip(from, to, i as f32 / 10.0);
            assert_eq!(offset, (0.0, 0.0));
        }
    }

    #[test]
    fn 页签顺序变了才上膛() {
        let mut fx = LayoutFx::default();
        let panes = |ids: &[&str]| -> Vec<PaneState> {
            ids.iter()
                .map(|id| {
                    let mut pane = PaneState::new("pwsh");
                    pane.id = id.to_string();
                    pane
                })
                .collect()
        };
        fx.begin_frame();
        assert!(
            !fx.arm_tab_strip("leaf", &panes(&["a", "b"])),
            "第一次见到的组不补位"
        );
        fx.begin_frame();
        assert!(!fx.arm_tab_strip("leaf", &panes(&["a", "b"])), "没变不补位");
        fx.begin_frame();
        assert!(fx.arm_tab_strip("leaf", &panes(&["a", "b", "c"])), "新建");
        fx.begin_frame();
        assert!(fx.arm_tab_strip("leaf", &panes(&["b", "c", "a"])), "重排");
        fx.begin_frame();
        assert!(fx.arm_tab_strip("leaf", &panes(&["b", "a"])), "关闭");
        // 有一帧没画到这组(最大化藏起来了):再出现按首次处理
        fx.begin_frame();
        fx.begin_frame();
        assert!(!fx.arm_tab_strip("leaf", &panes(&["a"])));
    }

    /// chip 补位的状态机(总开关是进程级的:别的用例不拧它,这里按「开着」断言)。
    #[test]
    fn chip_挤动才滑_新出现的才展开() {
        if !mt_ui::motion::animations_enabled() {
            return;
        }
        let t0 = Instant::now();
        let mut fx = LayoutFx::default();
        fx.begin_frame();
        // 首帧:记下,不动
        let f = fx.flip("l\u{1}a", 0.0, false, t0);
        assert_eq!((f.offset, f.reveal, f.running), (0.0, None, false));
        let f = fx.flip("l\u{1}b", 100.0, false, t0);
        assert!(!f.running);
        // 不上膛时挪位(标题变长):直接到位
        fx.begin_frame();
        let f = fx.flip("l\u{1}b", 130.0, false, t0);
        assert_eq!((f.offset, f.running), (0.0, false));
        // 上膛:a 被关掉,b 从 130 挪到 0 —— 第一帧就画在旧位置
        fx.begin_frame();
        let f = fx.flip("l\u{1}b", 0.0, true, t0);
        assert_eq!(f.offset, 130.0, "布局变了的那一帧画在旧位置,不先跳");
        assert!(f.running);
        // 滑完归零、不再要帧
        fx.begin_frame();
        let done = t0 + LIST_REFLOW.duration + Duration::from_millis(1);
        let f = fx.flip("l\u{1}b", 0.0, false, done);
        assert_eq!((f.offset, f.running), (0.0, false));
        // 上膛那一帧新出现的 chip:从 0 起裁剪展开
        fx.begin_frame();
        let f = fx.flip("l\u{1}c", 100.0, true, t0);
        assert_eq!(f.reveal, Some(0.0));
        assert_eq!(f.offset, 0.0, "新 chip 原地展开,不位移");
        fx.begin_frame();
        let f = fx.flip("l\u{1}c", 100.0, false, done);
        assert_eq!(f.reveal, None, "展开播完就不裁了");
    }

    #[test]
    fn 关分屏后只有长大的格子扩张() {
        if !mt_ui::motion::animations_enabled() {
            return;
        }
        let t0 = Instant::now();
        let mut fx = LayoutFx::default();
        let (left, right) = (rect(0.0, 0.0, 500.0, 700.0), rect(500.0, 0.0, 500.0, 700.0));
        fx.begin_frame();
        assert!(fx.grow("a", left, t0).is_none());
        assert!(fx.grow("b", right, t0).is_none());
        // 拖分隔条 / 缩窗口(没上膛):格子变了也不扩张
        fx.begin_frame();
        assert!(fx.grow("b", rect(400.0, 0.0, 600.0, 700.0), t0).is_none());
        // 关掉 a:b 长到整幅,第一帧裁剪窗就是旧格子
        fx.begin_frame();
        fx.arm_split_grow(true);
        let full = rect(0.0, 0.0, 1000.0, 700.0);
        let (clip, offset) = fx.grow("b", full, t0).expect("上膛那一帧起扩张");
        assert_eq!(clip, rect(400.0, 0.0, 600.0, 700.0));
        assert_eq!(offset, (400.0, 0.0));
        // 下一帧扳机复位,但在飞的照走;播完收掉
        fx.begin_frame();
        assert!(fx.grow("b", full, t0 + Duration::from_millis(1)).is_some());
        fx.begin_frame();
        let done = t0 + AREA_SWAP.duration + Duration::from_millis(1);
        assert!(fx.grow("b", full, done).is_none());
        // 上膛那一帧变小 / 挪位的格子不扩张(只认罩住原格子的)
        fx.begin_frame();
        fx.arm_split_grow(true);
        assert!(fx.grow("b", rect(0.0, 0.0, 300.0, 700.0), done).is_none());
    }

    /// `Split_h[A, Split_v[B, C]]`:关 C(B 往下长),半路再关 A(B 的目标变成整幅)——
    /// 第二场从**这一刻的裁剪窗**起步,不先跳到右半幅整格再往左长。
    #[test]
    fn 扩张途中再关一格_从当前裁剪窗接着长() {
        if !mt_ui::motion::animations_enabled() {
            return;
        }
        let t0 = Instant::now();
        let mut fx = LayoutFx::default();
        let quarter = rect(500.0, 0.0, 500.0, 350.0);
        let half = rect(500.0, 0.0, 500.0, 700.0);
        let full = rect(0.0, 0.0, 1000.0, 700.0);
        fx.begin_frame();
        assert!(fx.grow("b", quarter, t0).is_none());
        // 关 C:B 从右上四分之一往下长
        fx.begin_frame();
        fx.arm_split_grow(true);
        assert!(fx.grow("b", half, t0).is_some());
        // 半路再关 A
        let mid = t0 + AREA_SWAP.duration / 2;
        let (on_screen, _) = grow_clip(quarter, half, AREA_SWAP.progress_live(mid - t0));
        assert!(on_screen.height < half.height, "前提:第一场确实还在半路");
        fx.begin_frame();
        fx.arm_split_grow(true);
        let (clip, (dx, dy)) = fx.grow("b", full, mid).expect("再关一格照样扩张");
        assert!(
            !clip.differs(&on_screen),
            "接着当前裁剪窗长,不跳:{clip:?} vs {on_screen:?}"
        );
        // 内容照旧钉在裁剪窗左上角
        assert!((full.left + dx - clip.left).abs() < 1e-3);
        assert!((full.top + dy - clip.top).abs() < 1e-3);
        // 第二场播完精确落到整幅
        fx.begin_frame();
        let done = mid + AREA_SWAP.duration + Duration::from_millis(1);
        assert!(fx.grow("b", full, done).is_none());
    }

    /// 展开中的新 chip 又被挤动:裁剪窗原点跟着内容的偏移走,宽按进度长。
    #[test]
    fn chip_展开中被挤动_裁剪窗跟着内容走() {
        let layout = rect(200.0, 4.0, 120.0, 26.0);
        // 没挪:就是排版位置,从左缘往右长
        assert_eq!(reveal_clip(layout, 0.0, 0.5), rect(200.0, 4.0, 60.0, 26.0));
        // 内容此刻画在排版位置左边 80px:裁剪窗原点同样左移 80
        let clip = reveal_clip(layout, -80.0, 0.5);
        assert_eq!(clip, rect(120.0, 4.0, 60.0, 26.0));
        // 进度越界钳住
        assert_eq!(reveal_clip(layout, 0.0, 1.5).width, 120.0);
        assert_eq!(reveal_clip(layout, 0.0, -1.0).width, 0.0);
    }

    /// 扩张中整格被挪着画:子树里量到的矩形减掉偏移,还原成排版位置;尺寸不动。
    #[test]
    fn 扩张中量到的矩形减掉整格偏移() {
        let drawn = Bounds::new(point(px(900.0), px(40.0)), size(px(110.0), px(26.0)));
        let layout = unshift(drawn, (400.0, 0.0));
        assert_eq!(layout.origin, point(px(500.0), px(40.0)));
        assert_eq!(layout.size, drawn.size);
        assert_eq!(unshift(drawn, (0.0, 0.0)), drawn, "不在扩张中原样返回");
        // 偏移只在扩张中的叶子子树 prepaint 期间登记,平时为零
        let fx = LayoutFx::default();
        assert_eq!(fx.unshift(drawn), drawn);
    }
}
