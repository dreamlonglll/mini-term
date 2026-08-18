//! 矢量图标基建:一套「单位方框里的形状表」+ 一个把它画出来的 Element。
//!
//! # 为什么不是 `svg()` / 不是嵌 SVG 位图
//!
//! 三条路都试过,只有自绘走得通:
//!
//! | 路子 | 挡在哪 |
//! |---|---|
//! | `gpui::svg()` | 走 `AssetSource` 取文件。宿主(mt-app)现在**没有注册任何 asset source**,
//!   而且它渲染出来的是 **alpha 掩膜**,整枚图标只有一个颜色 —— 品牌多色 logo 直接没了 |
//! | `img(Image::from_bytes(ImageFormat::Svg, …))` | gpui 0.2.2 在这条路上**漏了 RGBA→BGRA 那步**
//!   (`platform.rs` 的 `to_image_data`:PNG/JPEG 分支都 `pixel.swap(0, 2)`,SVG 分支没有),
//!   红蓝互换;而且 tiny-skia 给的是预乘 alpha,抗锯齿边缘也对不上。上游修好之前不能用 |
//! | **自绘(本模块)** | 无依赖、无宿主接线、分辨率无关、可多色、几何是纯数据可单测 |
//!
//! # 形状表长什么样
//!
//! 每枚图标是一张 `&'static [Shape]`,坐标全在 **0..1 的单位方框**里(照抄原版 SVG 的
//! viewBox 除以边长即可),画的时候乘上真实尺寸。于是同一份数据 10px / 40px 都能画,
//! 且「图标 A 的第 3 笔在哪」是可以直接断言的常量。
//!
//! ```ignore
//! const CHECK: &[Shape] = &[
//!     Shape::fill(Ink::Current, Geom::Circle { c: (0.5, 0.5), r: 0.40 }),
//!     Shape::line(Ink::Contrast, 0.125, Geom::Polyline(&[(0.31, 0.51), (0.44, 0.64), (0.69, 0.36)])),
//! ];
//! ```
//!
//! # 三种「墨水」
//!
//! - [`Ink::Current`] 跟随调用方给的主色(相当于 CSS 的 `currentColor`);
//! - [`Ink::Contrast`] 是「实心底上的字形色」(勾/叉),默认取面板底色;
//! - [`Ink::Rgb`] 是品牌固定色,不跟主题走 —— 品牌 logo 换色就不是那个牌子了。

use std::f32::consts::TAU;

use gpui::{
    App, Bounds, Element, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId,
    PathBuilder, Pixels, Point, Style, Window, point, px,
};

use crate::terminal::rgb8;

/// 圆/弧离散成多少段。10px 的状态灯到 40px 的空态图标都够圆,再多是白费顶点。
pub const ARC_SEGMENTS: usize = 48;

/// 一笔用什么颜色。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ink {
    /// 跟随调用方给的主色。
    Current,
    /// 主色但压透明度(底环这类衬底)。
    CurrentAlpha(f32),
    /// 「实心底上的字形」色 —— 勾、叉画在实心圆上,必须用底色而不是反色,
    /// 否则浅色主题下白勾配浅绿底等于看不见。
    Contrast,
    /// 品牌固定色。
    Rgb(u8, u8, u8),
    /// 品牌固定色 + 透明度。
    RgbAlpha(u8, u8, u8, f32),
}

/// 一笔是填充还是描边。描边宽度也是**单位比例**(乘尺寸得到像素)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pen {
    Fill,
    Line(f32),
}

/// 一笔的几何。全部在 0..1 单位方框内;角度用度,**0° 指向 3 点钟、正角顺时针**
/// (y 轴向下的屏幕坐标系里,正角自然就是顺时针,和 SVG 的 arc 一致)。
#[derive(Clone, Copy, Debug)]
pub enum Geom {
    /// 闭合折线。
    Polygon(&'static [(f32, f32)]),
    /// 开放折线。
    Polyline(&'static [(f32, f32)]),
    Circle {
        c: (f32, f32),
        r: f32,
    },
    /// 椭圆,`tilt` 是绕自身中心的倾角(度)。
    Ellipse {
        c: (f32, f32),
        r: (f32, f32),
        tilt: f32,
    },
    /// 圆弧(开放)。`from` 起始角,`sweep` 扫过的角度(可负 = 逆时针)。
    Arc {
        c: (f32, f32),
        r: f32,
        from: f32,
        sweep: f32,
    },
    /// 圆角矩形。`round` = 0 就是直角。
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        round: f32,
    },
}

/// 图标里的一笔。
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub ink: Ink,
    pub pen: Pen,
    pub geom: Geom,
}

impl Shape {
    pub const fn fill(ink: Ink, geom: Geom) -> Self {
        Self {
            ink,
            pen: Pen::Fill,
            geom,
        }
    }

    pub const fn line(ink: Ink, width: f32, geom: Geom) -> Self {
        Self {
            ink,
            pen: Pen::Line(width),
            geom,
        }
    }
}

impl Geom {
    /// 离散成单位坐标点列 + 「是否闭合」。**纯函数,单测就打在这上面**。
    pub fn points(&self) -> (Vec<(f32, f32)>, bool) {
        match *self {
            Geom::Polygon(pts) => (pts.to_vec(), true),
            Geom::Polyline(pts) => (pts.to_vec(), false),
            Geom::Circle { c, r } => (arc_points(c, (r, r), 0.0, 0.0, 360.0, ARC_SEGMENTS), true),
            Geom::Ellipse { c, r, tilt } => (
                arc_points(c, r, tilt, 0.0, 360.0, ARC_SEGMENTS),
                true,
            ),
            Geom::Arc { c, r, from, sweep } => {
                // 段数按扫过的角度分摊,90° 的弧不必用 48 段
                let segments = ((ARC_SEGMENTS as f32) * (sweep.abs() / 360.0)).ceil() as usize;
                (
                    arc_points(c, (r, r), 0.0, from, sweep, segments.max(2)),
                    false,
                )
            }
            Geom::Rect {
                x,
                y,
                w,
                h,
                round,
            } => (round_rect_points(x, y, w, h, round), true),
        }
    }
}

/// 椭圆弧采样。`tilt` 先把点绕椭圆中心转一下(倾斜的椭圆,如 React 的三个圈)。
///
/// 闭合整圈(`sweep == 360`)时**不重复首尾点** —— 多一个重合点会让描边在接缝处
/// 叠出一个更浓的小段。
pub fn arc_points(
    c: (f32, f32),
    r: (f32, f32),
    tilt: f32,
    from: f32,
    sweep: f32,
    segments: usize,
) -> Vec<(f32, f32)> {
    let closed = (sweep.abs() - 360.0).abs() < 0.001;
    let count = if closed { segments } else { segments + 1 };
    let denom = segments.max(1) as f32;
    let (ts, tc) = (tilt.to_radians().sin(), tilt.to_radians().cos());
    (0..count)
        .map(|i| {
            let deg = from + sweep * (i as f32) / denom;
            let rad = deg.to_radians();
            let (x, y) = (r.0 * rad.cos(), r.1 * rad.sin());
            (c.0 + x * tc - y * ts, c.1 + x * ts + y * tc)
        })
        .collect()
}

/// 圆角矩形的点列。`round` 会被钳到边长的一半(否则圆角会互相穿过)。
pub fn round_rect_points(x: f32, y: f32, w: f32, h: f32, round: f32) -> Vec<(f32, f32)> {
    let r = round.min(w / 2.0).min(h / 2.0).max(0.0);
    if r <= f32::EPSILON {
        return vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)];
    }
    // 每个角 8 段,90° 一段 11.25°,16px 的图标上已经看不出折线
    let seg = 8;
    let mut pts = Vec::with_capacity(seg * 4 + 4);
    // 顺时针:右上 → 右下 → 左下 → 左上(屏幕坐标 y 向下)
    for (cx, cy, from) in [
        (x + w - r, y + r, -90.0_f32),
        (x + w - r, y + h - r, 0.0),
        (x + r, y + h - r, 90.0),
        (x + r, y + r, 180.0),
    ] {
        pts.extend(arc_points((cx, cy), (r, r), 0.0, from, 90.0, seg));
    }
    pts
}

/// 一枚画好的图标元素。
///
/// 尺寸是**正方形**,由 `size` 决定;`ink` 是 `Ink::Current` 的取色;
/// `contrast` 是 `Ink::Contrast` 的取色;`rotation` 以「圈」为单位(0..1),
/// 绕图标中心整体旋转 —— 状态灯的 spinner 靠它转。
#[derive(Clone)]
pub struct VectorIcon {
    shapes: &'static [Shape],
    /// 叠在 `shapes` 之上的第二张表。文件图标靠它复用同一张纸的轮廓 ——
    /// const 数组没法拼接,分两张表比给每个类别抄一遍轮廓省事得多。
    overlay: &'static [Shape],
    size: Pixels,
    ink: Hsla,
    contrast: Hsla,
    rotation: f32,
    opacity: f32,
}

impl VectorIcon {
    pub fn new(shapes: &'static [Shape], size: Pixels) -> Self {
        Self {
            shapes,
            overlay: &[],
            size,
            ink: rgb8(0xf0, 0xec, 0xe6),
            contrast: rgb8(0x1c, 0x1a, 0x18),
            rotation: 0.0,
            opacity: 1.0,
        }
    }

    /// 叠画第二张形状表(轮廓 + 记号的组合)。
    pub fn overlay(mut self, shapes: &'static [Shape]) -> Self {
        self.overlay = shapes;
        self
    }

    /// `Ink::Current` 的取色。
    pub fn ink(mut self, color: Hsla) -> Self {
        self.ink = color;
        self
    }

    /// `Ink::Contrast` 的取色(实心底上的勾/叉)。默认 `--bg-elevated`。
    pub fn contrast(mut self, color: Hsla) -> Self {
        self.contrast = color;
        self
    }

    /// 整体旋转,单位是「圈」。`with_animation` 给的 delta 直接塞进来即可。
    pub fn rotation(mut self, turns: f32) -> Self {
        self.rotation = turns;
        self
    }

    /// 整体透明度(淡入淡出用)。
    pub fn opacity(mut self, alpha: f32) -> Self {
        self.opacity = alpha.clamp(0.0, 1.0);
        self
    }

    fn resolve(&self, ink: Ink) -> Hsla {
        let mut color = match ink {
            Ink::Current => self.ink,
            Ink::CurrentAlpha(a) => Hsla {
                a: self.ink.a * a,
                ..self.ink
            },
            Ink::Contrast => self.contrast,
            Ink::Rgb(r, g, b) => rgb8(r, g, b),
            Ink::RgbAlpha(r, g, b, a) => Hsla { a, ..rgb8(r, g, b) },
        };
        color.a *= self.opacity;
        color
    }
}

impl IntoElement for VectorIcon {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for VectorIcon {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = self.size.into();
        style.size.height = self.size.into();
        // 图标不参与 flex 压缩:一行放不下时该被挤扁的是文字,不是状态灯
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
        // 元素被布局压小时按较短边画,免得画到邻居身上
        let side = f32::from(bounds.size.width).min(f32::from(bounds.size.height));
        if side <= 0.0 {
            return;
        }
        let theta = self.rotation * TAU;
        let (sin, cos) = (theta.sin(), theta.cos());
        let map = |(x, y): (f32, f32)| -> Point<Pixels> {
            let (dx, dy) = (x - 0.5, y - 0.5);
            let (rx, ry) = (dx * cos - dy * sin, dx * sin + dy * cos);
            point(
                bounds.origin.x + px((rx + 0.5) * side),
                bounds.origin.y + px((ry + 0.5) * side),
            )
        };

        for shape in self.shapes.iter().chain(self.overlay.iter()) {
            let (pts, closed) = shape.geom.points();
            if pts.len() < 2 {
                continue;
            }
            let mut builder = match shape.pen {
                Pen::Fill => PathBuilder::fill(),
                // 线宽小于 0.5px 时 lyon 会 tessellate 出近乎空的三角带,
                // 高 DPI 下反而看不见 —— 兜一个下限
                Pen::Line(w) => PathBuilder::stroke(px((w * side).max(0.5))),
            };
            builder.move_to(map(pts[0]));
            for p in &pts[1..] {
                builder.line_to(map(*p));
            }
            if closed {
                builder.close();
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, self.resolve(shape.ink));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.001
    }

    #[test]
    fn 整圈采样不重复首尾点() {
        let pts = arc_points((0.5, 0.5), (0.4, 0.4), 0.0, 0.0, 360.0, 8);
        assert_eq!(pts.len(), 8, "闭合圈是 N 个点而不是 N+1");
        // 首点在 3 点钟方向
        assert!(approx(pts[0].0, 0.9) && approx(pts[0].1, 0.5), "{:?}", pts[0]);
        // 首尾不重合,否则描边接缝会叠出一小段浓色
        assert!(!approx(pts[7].0, pts[0].0) || !approx(pts[7].1, pts[0].1));
    }

    #[test]
    fn 正角是顺时针_与_svg_一致() {
        // 从 12 点(-90°)顺时针扫 90° 应落到 3 点(0°)——原版 spinner
        // `M8 2a6 6 0 0 1 6 6` 就是这一段
        let pts = arc_points((0.5, 0.5), (0.4, 0.4), 0.0, -90.0, 90.0, 4);
        assert_eq!(pts.len(), 5, "开放弧是 N+1 个点");
        assert!(approx(pts[0].0, 0.5) && approx(pts[0].1, 0.1), "起点应在正上方");
        assert!(
            approx(pts[4].0, 0.9) && approx(pts[4].1, 0.5),
            "终点应在正右方,实际 {:?}",
            pts[4]
        );
    }

    #[test]
    fn 椭圆倾角绕自身中心转() {
        // 半径 (0.4, 0.1) 的扁椭圆倾 90° 后,长轴应转到竖直方向
        let pts = arc_points((0.5, 0.5), (0.4, 0.1), 90.0, 0.0, 360.0, 4);
        assert!(approx(pts[0].0, 0.5) && approx(pts[0].1, 0.9), "{:?}", pts[0]);
    }

    #[test]
    fn 圆角半径被钳到边长一半() {
        // round 给爆了也不能让四个角互相穿过
        let pts = round_rect_points(0.0, 0.0, 1.0, 0.4, 5.0);
        for (x, y) in &pts {
            assert!((-0.001..=1.001).contains(x), "x 越界 {x}");
            assert!((-0.001..=0.401).contains(y), "y 越界 {y}");
        }
    }

    #[test]
    fn 直角矩形只有四个点() {
        let pts = round_rect_points(0.1, 0.2, 0.5, 0.6, 0.0);
        assert_eq!(pts, vec![(0.1, 0.2), (0.6, 0.2), (0.6, 0.8), (0.1, 0.8)]);
    }

    #[test]
    fn 形状表的点全在单位方框内() {
        // 所有图标共用这条约束:越界就会画到相邻元素上
        let mut checked = 0usize;
        for shapes in crate::icons::all_shape_tables() {
            for shape in shapes {
                let (pts, _) = shape.geom.points();
                for (x, y) in pts {
                    assert!(
                        (-0.001..=1.001).contains(&x) && (-0.001..=1.001).contains(&y),
                        "越界点 ({x}, {y})"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 500, "形状表看起来没被遍历到(只有 {checked} 个点)");
    }
}
