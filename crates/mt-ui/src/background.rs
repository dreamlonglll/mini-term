//! 主题包背景图渲染(补上 [`crate::theme_bridge`] 里那条 TODO)。
//!
//! # 原版怎么铺的
//!
//! `src/utils/themePackManager.ts:462-471` —— 图挂在 `#root` 的 inline background 上:
//!
//! ```css
//! background-image: linear-gradient(<dim>, <dim>), url("<图>");
//! background-size: cover;
//! background-position: <focusX>% <focusY>%;
//! background-repeat: no-repeat;
//! ```
//!
//! 三条语义要照搬:
//!
//! 1. **cover**:按「容器/图片宽高比」取较大的缩放系数,溢出的那一维被裁掉;
//! 2. **focus 是百分比定位**,不是平移量 —— CSS 的 `background-position: X% Y%` 意思是
//!    「图片自身 X% 处对齐容器 X% 处」,换算出来的偏移是 `(容器 - 缩放后) * focus`。
//!    focus=0.5 才是居中;gpui 自带的 [`gpui::ObjectFit::Cover`] **恒定居中**,
//!    所以这里没法直接用它,得自己算 bounds;
//! 3. **压暗层与图合成在同一个 background 上**:先画图,再在**同一块 bounds** 上盖一层
//!    `art.dim` 的纯色 quad(`dim` 已经含 alpha,由 `backgroundDim` 算好)。
//!
//! # 挂在哪
//!
//! 原版是**窗口级**(`#root`),整个三栏都透着同一张图;终端只是因为
//! 「默认背景不发 quad」+ 半透明的 `TerminalTheme::background` 而透出来。
//! 所以推荐接法也是窗口级:
//!
//! ```ignore
//! // mt-app 的根容器,**第一个 child**(要在三栏之下)
//! use mt_ui::background::background_art;
//! let art = store.read(cx).background_art().cloned();   // AppStore 已备好这个口
//! div().size_full().relative()
//!     .when_some(art, |this, art| {
//!         this.child(div().absolute().inset_0().child(background_art(art)))
//!     })
//!     .child(three_columns)
//! ```
//!
//! 只想让终端区有图(任务书里那条)时用 [`crate::TerminalView::set_background_art`],
//! 它会把这一层画在 grid **底下**、hitbox 之外,不影响任何输入。
//!
//! ⚠️ **overdraw**:窗口级铺一张 + 每个终端再铺一张 = 同一块像素画两遍图两遍纱罩,
//! 而且两层纱罩会把 dim 平方。两者**二选一**,别同时开。

use std::sync::Arc;

use gpui::{
    App, Bounds, ContentMask, Corners, DevicePixels, Element, GlobalElementId, ImageAssetLoader,
    InspectorElementId, IntoElement, LayoutId, Pixels, RenderImage, Resource, Size, Style, Window,
    fill, point, px, size,
};

use crate::theme_bridge::BackgroundArt;

/// 铺放方式。`Cover` 取较大系数(裁边),`Contain` 取较小系数(留白)。
/// 两者只差一个 `max`/`min` —— 原版只用 `cover`,`Contain` 备着给设置页的预览缩略图。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Cover,
    Contain,
}

/// 图片在容器里的落位。**纯函数,单测钉在这上面**。
///
/// `focus` 是 CSS `background-position` 的百分比语义(0..1),会被钳到 0..1。
/// 图片尺寸为 0(还没解码出来 / 坏图)时原样返回容器 bounds。
pub fn fit_bounds(
    container: Bounds<Pixels>,
    image: Size<DevicePixels>,
    focus: (f32, f32),
    fit: Fit,
) -> Bounds<Pixels> {
    let (iw, ih) = (
        u32::from(image.width) as f32,
        u32::from(image.height) as f32,
    );
    let (cw, ch) = (
        f32::from(container.size.width),
        f32::from(container.size.height),
    );
    if iw <= 0.0 || ih <= 0.0 || cw <= 0.0 || ch <= 0.0 {
        return container;
    }
    let (sx, sy) = (cw / iw, ch / ih);
    let scale = match fit {
        Fit::Cover => sx.max(sy),
        Fit::Contain => sx.min(sy),
    };
    let (w, h) = (iw * scale, ih * scale);
    let (fx, fy) = (focus.0.clamp(0.0, 1.0), focus.1.clamp(0.0, 1.0));
    Bounds::new(
        point(
            px(f32::from(container.origin.x) + (cw - w) * fx),
            px(f32::from(container.origin.y) + (ch - h) * fy),
        ),
        size(px(w), px(h)),
    )
}

/// 背景图 + 压暗纱罩。铺满自己的 bounds。
///
/// 图片走 gpui 的资产系统异步解码([`ImageAssetLoader`] + `Resource::Path`,
/// 直接读磁盘,不经 http),**首帧多半还没解码好** —— 那一帧什么都不画,
/// 解码完 gpui 会自己重绘一次。所以别在这层加「加载中」占位:
/// 主题刚切过去闪一个灰块比什么都不闪更难看。
#[derive(Clone)]
pub struct BackgroundArtElement {
    art: BackgroundArt,
    fit: Fit,
    /// 额外的整体透明度(0..1)。设置页做小预览时可以调低。
    opacity: f32,
}

impl BackgroundArtElement {
    pub fn new(art: BackgroundArt) -> Self {
        Self {
            art,
            fit: Fit::Cover,
            opacity: 1.0,
        }
    }

    pub fn fit(mut self, fit: Fit) -> Self {
        self.fit = fit;
        self
    }

    pub fn opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity.clamp(0.0, 1.0);
        self
    }

    /// 解码好的位图。没好返回 `None`(见结构体注释)。
    fn image(&self, window: &mut Window, cx: &mut App) -> Option<Arc<RenderImage>> {
        let resource = Resource::Path(self.art.image.clone().into());
        window
            .use_asset::<ImageAssetLoader>(&resource, cx)
            .and_then(|r| r.ok())
    }

    /// 在给定 bounds 上画一层。给 [`crate::TerminalElement`] 这类**已经有自己的
    /// Element** 的宿主内联调用,免得多套一层布局节点。
    pub fn paint_into(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let Some(data) = self.image(window, cx) else {
            return;
        };
        let img_bounds = fit_bounds(bounds, data.size(0), self.art.focus, self.fit);
        let mut dim = self.art.dim;
        dim.a *= self.opacity;
        // 裁到容器内:cover 一定有一维溢出,不裁会糊到邻居身上
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if window
                .paint_image(img_bounds, Corners::default(), data, 0, false)
                .is_err()
            {
                return;
            }
            // 纱罩与图合成在同一块 bounds 上(原版是同一个 background 的两层)
            if dim.a > 0.0 {
                window.paint_quad(fill(bounds, dim));
            }
        });
    }
}

impl IntoElement for BackgroundArtElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for BackgroundArtElement {
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
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
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
        cx: &mut App,
    ) {
        self.paint_into(bounds, window, cx);
    }
}

/// 便捷构造:`div().absolute().inset_0().child(background_art(art))`。
pub fn background_art(art: BackgroundArt) -> BackgroundArtElement {
    BackgroundArtElement::new(art)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(w), px(h)))
    }

    fn img(w: u32, h: u32) -> Size<DevicePixels> {
        size(DevicePixels(w as i32), DevicePixels(h as i32))
    }

    fn tuple(b: Bounds<Pixels>) -> (f32, f32, f32, f32) {
        (
            f32::from(b.origin.x),
            f32::from(b.origin.y),
            f32::from(b.size.width),
            f32::from(b.size.height),
        )
    }

    #[test]
    fn cover_取较大系数_溢出的一维被裁() {
        // 容器 400x300(4:3),图 800x800(1:1)→ 按宽缩不够高,取按高的系数 300/800
        let b = fit_bounds(container(400.0, 300.0), img(800, 800), (0.5, 0.5), Fit::Cover);
        let (x, y, w, h) = tuple(b);
        assert!((w - 400.0).abs() < 0.01 && (h - 400.0).abs() < 0.01, "{w}x{h}");
        // focus 0.5 = 居中:横向刚好,纵向各溢出 50
        assert!((x - 0.0).abs() < 0.01 && (y + 50.0).abs() < 0.01, "({x},{y})");
    }

    #[test]
    fn focus_是百分比定位_不是平移量() {
        // 同上,focus 纵向 0 = 图的顶边对齐容器顶边
        let b = fit_bounds(container(400.0, 300.0), img(800, 800), (0.5, 0.0), Fit::Cover);
        assert!((tuple(b).1 - 0.0).abs() < 0.01);
        // focus 纵向 1 = 图的底边对齐容器底边
        let b = fit_bounds(container(400.0, 300.0), img(800, 800), (0.5, 1.0), Fit::Cover);
        let (_, y, _, h) = tuple(b);
        assert!((y + h - 300.0).abs() < 0.01, "底边应贴容器底,实际 y={y} h={h}");
    }

    #[test]
    fn focus_越界被钳住() {
        // theme_bridge 那边已经钳过一次,这里是第二道 —— 越界会把图整个推出可视区
        let a = fit_bounds(container(400.0, 300.0), img(800, 800), (0.5, 9.0), Fit::Cover);
        let b = fit_bounds(container(400.0, 300.0), img(800, 800), (0.5, 1.0), Fit::Cover);
        assert_eq!(tuple(a), tuple(b));
    }

    #[test]
    fn contain_取较小系数_四周留白() {
        let b = fit_bounds(container(400.0, 300.0), img(800, 800), (0.5, 0.5), Fit::Contain);
        let (x, y, w, h) = tuple(b);
        assert!((w - 300.0).abs() < 0.01 && (h - 300.0).abs() < 0.01, "{w}x{h}");
        assert!((x - 50.0).abs() < 0.01 && y.abs() < 0.01, "({x},{y})");
    }

    #[test]
    fn 容器带原点偏移时跟着走() {
        let c = Bounds::new(point(px(100.0), px(40.0)), size(px(400.0), px(300.0)));
        let b = fit_bounds(c, img(400, 300), (0.5, 0.5), Fit::Cover);
        assert_eq!(tuple(b), (100.0, 40.0, 400.0, 300.0));
    }

    #[test]
    fn 图片或容器为零时不算数() {
        let c = container(400.0, 300.0);
        assert_eq!(tuple(fit_bounds(c, img(0, 0), (0.5, 0.5), Fit::Cover)), tuple(c));
        let zero = container(0.0, 0.0);
        assert_eq!(
            tuple(fit_bounds(zero, img(10, 10), (0.5, 0.5), Fit::Cover)),
            tuple(zero)
        );
    }
}
