//! 折叠箭头的旋转,以及「只让刚被翻的那一个动」的补间表。
//!
//! # 口径
//!
//! - 箭头只画**一枚**(右指),展开时顺时针转到下指([`OPEN_TURNS`] = 90°),
//!   `VectorIcon::rotation` 的单位是「圈」;时长曲线取 [`mt_ui::motion::CHEVRON_ROTATE`],
//!   与区块高度展开同步到位;
//! - **只让刚被点(或键盘翻)的那一个转**:面板打开、列表重建、滚动出入视口都不该让一屏
//!   箭头集体播一遍。所以这里不用「首次见到就落目标、之后变值就补」的 `TweenMap` 口径 ——
//!   那条口径下任何一次批量改状态(搜索展开全部、换项目)都会被当成「值变了」——而是由
//!   交互处理器显式 [`FlipTweens::flip`] 记一笔,渲染时只有记过、且还在跑、且目标与当前
//!   状态一致的那一条才取插值,其余一律按状态画静止值;
//! - 状态挂在**持有这块列表的视图**上(按 key 索引),构建行的函数仍是无状态的,只多收一个
//!   「这一帧转多少」的数;
//! - 服从「启用动画」总开关(`ValueTween` 天然过闸),豁免系统「减少动画」。
//!
//! ```ignore
//! // 视图字段
//! chevrons: FlipTweens,                     // FlipTweens::new(CHEVRON_ROTATE)
//!
//! // 点击处理器(状态翻过去之后):记下「刚翻到 open」
//! this.chevrons.flip_open(&key, open, OPEN_TURNS);
//!
//! // 行构建:这一帧转多少圈
//! let turns = self.chevrons.value_open(&key, open, OPEN_TURNS);
//!
//! // render 里:还有在跑的就要下一帧
//! self.chevrons.drive(window);
//! ```
//!
//! 同一张表也给设置页的开关滑块用(`on = 1.0`,值是滑块的行程比例)。
//!
//! # 字形箭头(▸ / ▾)
//!
//! 项目分组、文件树、SSH 分组原先画的是**字形**三角(Segoe UI Symbol 里的 U+25B8 / U+25BE,
//! 9.75~10px),两个字形没法转。[`fold_glyph`] 是它们的矢量替身:同样大小、同样位置的一枚
//! 实心小三角,颜色与字号取**继承下来的文字样式**(悬停变色照旧跟着行走),可以按圈旋转。

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::time::Instant;

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    PathBuilder, Pixels, Point, Style, Window, point, px,
};
use mt_ui::motion::{TransitionSpec, ValueTween};

/// 展开态的箭头转到哪(圈):右指 → 下指,顺时针 90°。
pub const OPEN_TURNS: f32 = 0.25;

/// 按开合状态的静止值(`on` 是「开」那一端的值)。
pub fn settled_value(open: bool, on: f32) -> f32 {
    if open { on } else { 0.0 }
}

/// 一表「刚被翻过的」补间,按字符串 key 索引。见模块注释。
#[derive(Clone, Debug)]
pub struct FlipTweens {
    spec: TransitionSpec,
    entries: HashMap<String, ValueTween>,
}

impl FlipTweens {
    pub fn new(spec: TransitionSpec) -> Self {
        Self {
            spec,
            entries: HashMap::new(),
        }
    }

    /// 记一笔:`key` 刚从 `from` 翻向 `to`。在飞的那条从**当前显示值**掉头,不跳变;
    /// 没在飞的从 `from` 起播。顺手丢掉已经跑完的条目(表只随用户的点击增长)。
    pub fn flip_at(&mut self, key: impl Into<String>, from: f32, to: f32, now: Instant) {
        let key = key.into();
        self.entries.retain(|k, t| *k == key || t.running_at(now));
        match self.entries.get_mut(&key) {
            Some(tween) if tween.running_at(now) => tween.retarget_at(to, now),
            _ => {
                let mut tween = ValueTween::settled(from, self.spec);
                tween.retarget_at(to, now);
                self.entries.insert(key, tween);
            }
        }
    }

    /// 同 [`Self::flip_at`],时钟取现在。
    pub fn flip(&mut self, key: impl Into<String>, from: f32, to: f32) {
        self.flip_at(key, from, to, Instant::now());
    }

    /// 开合两态的便捷入口:`open` 是**翻过去之后**的状态,`on` 是「开」那一端的值。
    pub fn flip_open(&mut self, key: impl Into<String>, open: bool, on: f32) {
        self.flip(key, settled_value(!open, on), settled_value(open, on));
    }

    /// 这一帧该画的值。`settled` 是按当前状态的静止值:只有记过一笔、还在跑、且目标就是
    /// 这个静止值的条目才取插值(状态被别处又翻回去了的话按静止值画,不追旧目标)。
    pub fn value_at(&self, key: &str, settled: f32, now: Instant) -> f32 {
        match self.entries.get(key) {
            Some(tween) if tween.running_at(now) && (tween.target() - settled).abs() <= 1e-4 => {
                tween.value_at(now)
            }
            _ => settled,
        }
    }

    /// 同 [`Self::value_at`],时钟取现在。
    pub fn value(&self, key: &str, settled: f32) -> f32 {
        self.value_at(key, settled, Instant::now())
    }

    /// 开合两态的便捷入口(与 [`Self::flip_open`] 配对)。
    pub fn value_open(&self, key: &str, open: bool, on: f32) -> f32 {
        self.value(key, settled_value(open, on))
    }

    /// 还有条目在跑吗。
    pub fn running_at(&self, now: Instant) -> bool {
        self.entries.values().any(|t| t.running_at(now))
    }

    /// 有条目在跑就请求下一帧(**持有本表的视图的 render 调用链里调**),返回是否请求了。
    pub fn drive(&self, window: &Window) -> bool {
        let running = self.running_at(Instant::now());
        if running {
            window.request_animation_frame();
        }
        running
    }
}

// ─── 字形箭头的矢量替身 ──────────────────────────────────────

/// 字形的度量(单位 em,取自 Segoe UI Symbol —— Windows 上 ▸ / ▾ 实际落到的回退字体):
/// U+25B8 的墨迹框 x 0.271..0.591、y 0.172..0.528(基线以上),U+25BE 是它转 90° 的样子
/// (0.253..0.608 × 0.19..0.51),两者的墨迹中心都在 (0.43em, 基线上 0.35em)。
const GLYPH_ADVANCE: f32 = 0.861;
const GLYPH_CENTER_X: f32 = 0.431;
const GLYPH_CENTER_RISE: f32 = 0.35;
/// 右指三角的半宽 / 半高(em)。
const GLYPH_HALF_W: f32 = 0.16;
const GLYPH_HALF_H: f32 = 0.178;
/// 行盒里基线的位置按界面主字体(Segoe UI)的 ascent / descent 算(gpui 的行布局:
/// `padding_top = (line_height - ascent - descent) / 2`,基线 = padding_top + ascent)。
const FONT_ASCENT: f32 = 1.079;
const FONT_DESCENT: f32 = 0.251;

/// 右指实心三角在 `turns` 圈下的三个顶点(相对墨迹中心,单位 em)。**纯函数**,单测钉
/// 「转 0.25 圈正好是下指」。
pub fn glyph_triangle(turns: f32) -> [(f32, f32); 3] {
    let theta = turns * TAU;
    let (sin, cos) = (theta.sin(), theta.cos());
    let rotate = |(x, y): (f32, f32)| (x * cos - y * sin, x * sin + y * cos);
    [
        rotate((-GLYPH_HALF_W, -GLYPH_HALF_H)),
        rotate((GLYPH_HALF_W, 0.0)),
        rotate((-GLYPH_HALF_W, GLYPH_HALF_H)),
    ]
}

/// 字形箭头(▸ / ▾)的矢量替身,`turns` = 0 右指、[`OPEN_TURNS`] 下指。见模块注释。
pub fn fold_glyph(turns: f32) -> FoldGlyph {
    FoldGlyph { turns }
}

pub struct FoldGlyph {
    turns: f32,
}

impl IntoElement for FoldGlyph {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for FoldGlyph {
    type RequestLayoutState = ();
    type PrepaintState = ();

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
    ) -> (LayoutId, ()) {
        // 占位与原来那个字形的文本节点一样:宽 = 字形步进,高 = 行高
        let text = window.text_style();
        let rem = window.rem_size();
        let font = text.font_size.to_pixels(rem);
        let mut style = Style::default();
        style.size.width = (font * GLYPH_ADVANCE).into();
        style.size.height = text.line_height_in_pixels(rem).into();
        style.flex_shrink = 0.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        // 颜色取继承下来的文字色:行的 hover 改字色时箭头跟着变(与原来的字形一致)
        let text = window.text_style();
        let rem = window.rem_size();
        let em = f32::from(text.font_size.to_pixels(rem));
        let line = f32::from(text.line_height_in_pixels(rem));
        if em <= 0.0 {
            return;
        }
        let baseline = (line - (FONT_ASCENT + FONT_DESCENT) * em) / 2.0 + FONT_ASCENT * em;
        let center = point(
            bounds.origin.x + px(GLYPH_CENTER_X * em),
            bounds.origin.y + px(baseline - GLYPH_CENTER_RISE * em),
        );
        let at = |(x, y): (f32, f32)| -> Point<Pixels> {
            point(center.x + px(x * em), center.y + px(y * em))
        };
        let [a, b, c] = glyph_triangle(self.turns);
        let mut builder = PathBuilder::fill();
        builder.move_to(at(a));
        builder.line_to(at(b));
        builder.line_to(at(c));
        builder.close();
        if let Ok(path) = builder.build() {
            window.paint_path(path, text.color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn spec() -> TransitionSpec {
        TransitionSpec::exempt(Duration::from_millis(200), mt_ui::motion::Easing::Linear)
    }

    /// 总开关是进程级的、别的用例不拧它:这里按「开着」断言插值,关着时只断言终态。
    fn enabled() -> bool {
        mt_ui::motion::animations_enabled()
    }

    #[test]
    fn 只有翻过的那一条取插值_其余按状态画静止值() {
        let t0 = Instant::now();
        let mut flips = FlipTweens::new(spec());
        // 没翻过:一律静止值(面板打开 / 列表重建不集体播)
        assert_eq!(flips.value_at("a", OPEN_TURNS, t0), OPEN_TURNS);
        assert_eq!(flips.value_at("b", 0.0, t0), 0.0);
        assert!(!flips.running_at(t0));

        flips.flip_at("a", 0.0, OPEN_TURNS, t0);
        let mid = t0 + Duration::from_millis(100);
        if enabled() {
            let v = flips.value_at("a", OPEN_TURNS, mid);
            assert!((v - OPEN_TURNS / 2.0).abs() < 1e-4, "{v}");
            assert!(flips.running_at(mid));
        }
        // 别的 key 不受影响
        assert_eq!(flips.value_at("b", 0.0, mid), 0.0);
        // 跑完回到静止值、不再要帧
        let end = t0 + Duration::from_millis(200);
        assert_eq!(flips.value_at("a", OPEN_TURNS, end), OPEN_TURNS);
        assert!(!flips.running_at(end));
    }

    #[test]
    fn 状态被别处翻回去时不追旧目标() {
        let t0 = Instant::now();
        let mut flips = FlipTweens::new(spec());
        flips.flip_at("a", 0.0, OPEN_TURNS, t0);
        let mid = t0 + Duration::from_millis(50);
        // 记的是「往开里转」,可状态眼下是关:按关的静止值画
        assert_eq!(flips.value_at("a", 0.0, mid), 0.0);
    }

    #[test]
    fn 半路再翻从当前显示值掉头() {
        if !enabled() {
            return;
        }
        let t0 = Instant::now();
        let mut flips = FlipTweens::new(spec());
        flips.flip_at("a", 0.0, 1.0, t0);
        let mid = t0 + Duration::from_millis(50);
        let shown = flips.value_at("a", 1.0, mid);
        assert!(shown > 0.0 && shown < 1.0);
        // 反向:from 参数被忽略,从眼下的 shown 起
        flips.flip_at("a", 1.0, 0.0, mid);
        assert!(
            (flips.value_at("a", 0.0, mid) - shown).abs() < 1e-4,
            "掉头那一刻不跳变"
        );
        assert_eq!(
            flips.value_at("a", 0.0, mid + Duration::from_millis(200)),
            0.0
        );
    }

    #[test]
    fn 跑完的条目在下一次翻时被清掉() {
        let t0 = Instant::now();
        let mut flips = FlipTweens::new(spec());
        flips.flip_at("a", 0.0, 1.0, t0);
        flips.flip_at("b", 0.0, 1.0, t0 + Duration::from_secs(1));
        assert_eq!(flips.entries.len(), 1, "a 早就跑完了");
    }

    #[test]
    fn 开合便捷入口的端点() {
        assert_eq!(settled_value(true, OPEN_TURNS), OPEN_TURNS);
        assert_eq!(settled_value(false, OPEN_TURNS), 0.0);
        assert_eq!(settled_value(true, 1.0), 1.0);
    }

    #[test]
    fn 字形三角_零圈右指_四分之一圈正好下指() {
        fn close(a: (f32, f32), b: (f32, f32)) -> bool {
            (a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5
        }
        let right = glyph_triangle(0.0);
        assert!(close(right[1], (GLYPH_HALF_W, 0.0)), "尖朝右");
        let down = glyph_triangle(OPEN_TURNS);
        // 屏幕坐标 y 向下:正角顺时针,右指转 90° 尖朝下
        assert!(close(down[1], (0.0, GLYPH_HALF_W)), "{:?}", down[1]);
        // 底边两点转到上方、左右对称
        assert!(
            close(down[0], (GLYPH_HALF_H, -GLYPH_HALF_W)),
            "{:?}",
            down[0]
        );
        assert!(
            close(down[2], (-GLYPH_HALF_H, -GLYPH_HALF_W)),
            "{:?}",
            down[2]
        );
    }
}
