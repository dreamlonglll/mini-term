//! 图片放大浮层(lightbox)。Markdown 预览里的 Mermaid 图表按列宽缩到 860px
//! 以内,大一点的架构图整张缩成一条、字全糊在一起(用户截图反馈)。点一下在
//! 整窗遮罩上原样放大:滚轮 / 触控板捏合缩放(以光标为中心)、按住拖动平移、
//! 双击回到「适应窗口」、Esc / 右上 × / 点遮罩空白关闭。
//!
//! # 层级
//!
//! 与 `date_picker.rs` 同一套:`deferred(priority 1)` → `anchored(0,0)` → 全窗口
//! 遮罩(`occlude`,把底下的悬停 / 滚动 / 点击全吃掉)。毛玻璃由根层那张共用快照
//! 承担(`main.rs` 按 overlay 栈里有没有本浮层决定抓不抓),遮罩自己只叠 black/50,
//! 与 Dialog 族 / 用量面板同一观感。宿主只要在自己的 render 里把实体 `child`
//! 出来即可 —— `deferred` 不吃祖先的 ContentMask,挂在虚拟化列表深处也不会被裁。
//!
//! # 缩放模型
//!
//! [`ImageLightbox::zoom`] 是「显示尺寸 / 图片原始逻辑尺寸」,[`ImageLightbox::offset`]
//! 是图片左上角在视口里的位置。图比视口小的那一轴永远居中(拖不动),比视口大
//! 的那一轴把偏移夹在「边缘不离开视口」之内 —— 拖到头就停,不会把图拖丢
//! ([`clamp_offset`])。缩放围绕光标:先算光标落在图上的哪一点,换倍率后让那一点
//! 仍在光标下,再夹一次偏移([`zoom_about`])。
//!
//! 位图是预览那条路已经栅格好的同一份(mermaid 走 2× 栅格),**不为放大重新
//! 渲染** —— 一份 4× 的架构图动辄几十 MB 显存(见 GPU 性能档案的「尺寸悬崖」),
//! 而 2× 在 150% 缩放下放到 1.33× 仍是逐像素的,再往上是 GPU 双线性放大,读图足够。
//!
//! # 开合
//!
//! 遮罩压暗渐变;图片**不淡入**:从点开它的那一点(打开时的光标位置)放大到位,收起反向
//! 缩回那一点([`zoom_rect`])。工具栏从顶边滑进、提示行从底边滑进。宿主拿不到预览里那张
//! 图的原矩形(`ClickEvent` 不带元素 bounds),所以原点只能是点击点。宿主收到关闭事件就
//! drop 实体,退场没处播 —— 关闭那一刻先还焦点、摘覆盖物栈登记,再交给离场托管层
//! (`overlay_motion::linger`)画完,最后才发事件。退场中不接鼠标、不收键盘。

use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    PinchEvent, Pixels, Point, Render, ScrollWheelEvent, Size, StatefulInteractiveElement, Styled,
    StyledImage as _, Window, anchored, deferred, div, img, point, prelude::FluentBuilder, px,
    size,
};
use mt_ui::motion::{OVERLAY_IN, OVERLAY_OUT};
use mt_ui::tooltip::TooltipExt as _;

use crate::i18n::t;
use crate::overlay;
use crate::overlay_motion::{self, Presence};
use crate::ui;

/// 图片四周至少留这么多空(逻辑像素),「适应窗口」按扣掉它之后的视口算。
const MARGIN: f32 = 32.0;
/// 顶部工具栏占的高度(适应窗口时从视口里扣掉,免得图顶进按钮底下)。
const TOOLBAR_H: f32 = 44.0;
/// 底部提示行占的高度(同上)。
const HINT_H: f32 = 36.0;
/// 滚轮一格 / 按钮一下的倍率。1.25 三格约 2×,与浏览器 Ctrl+滚轮同一手感;
/// 1.1 太磨叽,1.5 一格就跳过头。
const ZOOM_STEP: f32 = 1.25;
/// 一格滚轮折算成多少像素(gpui 的 `Lines` 按 20px/行、Windows 默认 3 行/格)。
const WHEEL_NOTCH_PX: f32 = 60.0;
/// 首次打开的「适应窗口」最多放到原始尺寸的几倍:小图撑满整窗只是一团模糊的大字,
/// 2× 正好用尽 2× 栅格的像素;用户要更大可以自己滚。
const MAX_INITIAL_ZOOM: f32 = 2.0;
/// 手动缩放的绝对上下限(相对原始尺寸)。下限另与「适应窗口」取小 —— 超大图的
/// fit 本身就可能低于 0.25。
const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 8.0;
/// 按下到松开之间移动不超过这么多像素算「点了一下」(点遮罩空白关闭),超过算拖。
const CLICK_SLOP: f32 = 4.0;

pub enum ImageLightboxEvent {
    /// 用户关掉了浮层(Esc / × / 点遮罩)。宿主收到后 drop 实体即可。
    Dismissed,
}

/// 一次按住拖动的锚点。
#[derive(Clone, Copy, Debug)]
struct Drag {
    /// 按下时的鼠标位置(窗口坐标)。
    anchor: Point<Pixels>,
    /// 按下时的图片偏移。
    origin: Point<f32>,
    /// 按在遮罩空白上(不在图上)—— 松开时若没拖动就关闭。
    on_backdrop: bool,
    /// 已经拖出 [`CLICK_SLOP`] 之外。
    moved: bool,
}

pub struct ImageLightbox {
    image: Arc<gpui::RenderImage>,
    /// 图片原始逻辑尺寸(svg 那条路已把 `SMOOTH_SVG_SCALE_FACTOR` 除回去,
    /// 与预览里 `image_display_width` 同一口径)。
    natural: Size<f32>,
    /// 显示尺寸 / 原始尺寸。
    zoom: f32,
    /// 图片左上角在视口里的位置(逻辑像素)。
    offset: Point<f32>,
    /// 用户还没手动缩放过 —— 窗口尺寸变化时保持「适应窗口」;一旦缩放过,
    /// 尺寸变化只重夹偏移,不动倍率。
    fitted: bool,
    /// 上一帧的视口尺寸,变了要重算。
    viewport: Size<Pixels>,
    drag: Option<Drag>,
    /// 打开前的焦点,关闭时还回去(顺序纪律同 `menu.rs`:先还焦点再发事件)。
    prev_focus: Option<FocusHandle>,
    focus: FocusHandle,
    /// 开合(`OVERLAY_IN` / `OVERLAY_OUT`,浮层进出场豁免「减少动画」)。
    presence: Presence,
    /// 图片从这一点放大出来、收起时缩回这一点(打开时的光标位置,窗口坐标)。
    origin: Point<f32>,
    /// 已经关了、正在播退场(不接鼠标、不收键盘,由离场托管层画完)。
    closing: bool,
    /// 覆盖物栈里的登记还在不在(关闭那一刻就摘,drop 时只摘自己还没摘的那份)。
    registered: bool,
}

impl EventEmitter<ImageLightboxEvent> for ImageLightbox {}

impl ImageLightbox {
    /// `natural` 是图片的原始逻辑尺寸。首帧按当前视口「适应窗口」
    /// (最多放到 [`MAX_INITIAL_ZOOM`] 倍)。
    ///
    /// ⚠️ 宿主换浮层时必须先把旧实体清掉再建新的(`self.lightbox = None;` 单独
    /// 一行),理由见 `DatePicker::new` 的注释:overlay 栈防叠开会把「先建后 drop」
    /// 的新登记挡掉,旧的 drop 时再把栈摘空。
    pub fn new(
        image: Arc<gpui::RenderImage>,
        natural: Size<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        overlay::push(overlay::key(overlay::kind::IMAGE_LIGHTBOX));
        let prev_focus = window.focused(cx);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let viewport = window.viewport_size();
        let natural = size(natural.width.max(1.0), natural.height.max(1.0));
        // 宿主是在点击回调里建的:此刻的光标就是点图的那一点
        let at = window.mouse_position();
        let mut presence = Presence::hidden(OVERLAY_IN, OVERLAY_OUT);
        presence.show(Instant::now());
        let mut this = Self {
            image,
            natural,
            zoom: 1.0,
            offset: point(0.0, 0.0),
            fitted: true,
            viewport,
            drag: None,
            prev_focus,
            focus,
            presence,
            origin: point(f32::from(at.x), f32::from(at.y)),
            closing: false,
            registered: true,
        };
        this.fit();
        this
    }

    /// 视口里留给图片的那块(扣掉四周留白与上下两条)。
    fn avail(&self) -> Size<f32> {
        let vw = f32::from(self.viewport.width);
        let vh = f32::from(self.viewport.height);
        size(
            (vw - 2.0 * MARGIN).max(1.0),
            (vh - TOOLBAR_H - HINT_H - 2.0 * MARGIN).max(1.0),
        )
    }

    fn view_size(&self) -> Size<f32> {
        size(
            f32::from(self.viewport.width),
            f32::from(self.viewport.height),
        )
    }

    /// 「适应窗口」的倍率(不封顶,给缩放下限用)。
    fn fit_zoom(&self) -> f32 {
        fit_zoom(self.natural, self.avail())
    }

    fn display_size(&self) -> Size<f32> {
        size(
            self.natural.width * self.zoom,
            self.natural.height * self.zoom,
        )
    }

    fn zoom_bounds(&self) -> (f32, f32) {
        zoom_bounds(self.fit_zoom())
    }

    /// 回到「适应窗口」:倍率取 fit 与 [`MAX_INITIAL_ZOOM`] 的小者,图居中。
    fn fit(&mut self) {
        self.zoom = self.fit_zoom().min(MAX_INITIAL_ZOOM);
        self.fitted = true;
        self.offset = clamp_offset(point(0.0, 0.0), self.display_size(), self.view_size());
    }

    /// 以 `cursor`(窗口坐标)为中心换到 `zoom` 倍。
    fn zoom_to(&mut self, zoom: f32, cursor: Point<f32>, cx: &mut Context<Self>) {
        let (lo, hi) = self.zoom_bounds();
        let zoom = zoom.clamp(lo, hi);
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        let offset = zoom_about(cursor, self.offset, self.zoom, zoom);
        self.zoom = zoom;
        self.fitted = false;
        self.offset = clamp_offset(offset, self.display_size(), self.view_size());
        cx.notify();
    }

    /// 视口中心(工具栏 / 按钮触发的缩放围绕它)。
    fn view_center(&self) -> Point<f32> {
        let v = self.view_size();
        point(v.width / 2.0, v.height / 2.0)
    }

    /// 视口尺寸变了(拖窗口 / 最大化):没缩放过的保持适应,缩放过的只重夹偏移。
    fn sync_viewport(&mut self, viewport: Size<Pixels>) {
        if viewport == self.viewport {
            return;
        }
        self.viewport = viewport;
        if self.fitted {
            self.fit();
        } else {
            let (lo, hi) = self.zoom_bounds();
            self.zoom = self.zoom.clamp(lo, hi);
            self.offset = clamp_offset(self.offset, self.display_size(), self.view_size());
        }
    }

    fn image_bounds(&self) -> gpui::Bounds<f32> {
        gpui::Bounds {
            origin: self.offset,
            size: self.display_size(),
        }
    }

    /// 图有没有哪一轴比视口大(= 拖得动)。决定光标样式。
    fn pannable(&self) -> bool {
        let d = self.display_size();
        let v = self.view_size();
        d.width > v.width || d.height > v.height
    }

    fn restore_focus(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(prev) = self.prev_focus.take() {
            window.focus(&prev, cx);
        }
    }

    /// 摘覆盖物栈登记(幂等)。根层按栈里有没有本浮层垫毛玻璃 —— 关闭那一刻摘,
    /// 玻璃就与这边的遮罩同步淡出。
    fn unregister(&mut self) {
        if std::mem::take(&mut self.registered) {
            overlay::pop(overlay::key(overlay::kind::IMAGE_LIGHTBOX));
        }
    }

    /// 关闭(Esc / × / 点遮罩):还焦点、摘登记、播退场,把实体交给离场托管层多活一个
    /// 退场时长,最后才发事件(宿主收到就 drop 自己那份)。
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.drag = None;
        self.restore_focus(window, cx);
        self.unregister();
        let now = Instant::now();
        self.presence.hide(now);
        if self.presence.mounted_at(now) {
            overlay_motion::linger(cx.entity().into(), OVERLAY_OUT.duration, cx);
        }
        cx.notify();
        cx.emit(ImageLightboxEvent::Dismissed);
    }

    // ─── 事件 ─────────────────────────────────────────────────

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        match ks.key.as_str() {
            "escape" => {
                cx.stop_propagation();
                self.dismiss(window, cx);
            }
            // 与浏览器 / 图片查看器同款:+ / = 放大,- 缩小,0 适应
            "=" | "+" | "plus" => {
                cx.stop_propagation();
                self.zoom_to(self.zoom * ZOOM_STEP, self.view_center(), cx);
            }
            "-" | "minus" => {
                cx.stop_propagation();
                self.zoom_to(self.zoom / ZOOM_STEP, self.view_center(), cx);
            }
            "0" => {
                cx.stop_propagation();
                self.fit();
                cx.notify();
            }
            _ => {}
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 遮罩盖住整窗,滚轮不能再漏给底下的文档
        cx.stop_propagation();
        let dy = f32::from(event.delta.pixel_delta(px(20.0)).y);
        let factor = wheel_zoom_factor(dy);
        let cursor = point(f32::from(event.position.x), f32::from(event.position.y));
        self.zoom_to(self.zoom * factor, cursor, cx);
    }

    fn on_pinch(&mut self, event: &PinchEvent, _window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let cursor = point(f32::from(event.position.x), f32::from(event.position.y));
        self.zoom_to(self.zoom * (1.0 + event.delta), cursor, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let at = point(f32::from(event.position.x), f32::from(event.position.y));
        let on_image = self.image_bounds().contains(&at);
        if event.click_count == 2 && on_image {
            // 双击回到适应窗口;这一下不算拖动的开始
            self.drag = None;
            self.fit();
            cx.notify();
            return;
        }
        self.drag = Some(Drag {
            anchor: event.position,
            origin: self.offset,
            on_backdrop: !on_image,
            moved: false,
        });
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut drag) = self.drag else {
            return;
        };
        // 在窗口外松开的左键收不到 mouse_up,靠这一步收尾
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            cx.notify();
            return;
        }
        let dx = f32::from(event.position.x - drag.anchor.x);
        let dy = f32::from(event.position.y - drag.anchor.y);
        if !drag.moved && (dx.abs() > CLICK_SLOP || dy.abs() > CLICK_SLOP) {
            drag.moved = true;
        }
        if drag.moved {
            self.offset = clamp_offset(
                point(drag.origin.x + dx, drag.origin.y + dy),
                self.display_size(),
                self.view_size(),
            );
        }
        self.drag = Some(drag);
        cx.notify();
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.on_backdrop && !drag.moved {
            self.dismiss(window, cx);
            return;
        }
        cx.notify();
    }

    // ─── 绘制 ─────────────────────────────────────────────────

    /// 工具栏按钮:26×26 的圆角方块,悬停提亮。`on_click` 收 `cx.listener`。
    fn tool_button(
        id: &'static str,
        label: &'static str,
        tip: &'static str,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(26.0))
            .h(px(26.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(ui::font_px(15.0))
            .text_color(ui::text_secondary())
            .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
            .tip(tip)
            // 按下不冒泡到遮罩:否则一点按钮就顺手起了一次拖动 / 关闭
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                // 退场中不接点击(工具栏正往顶边外滑)
                if this.closing {
                    return;
                }
                on_click(this, window, cx);
            }))
            .child(label)
            .into_any_element()
    }

    /// 工具栏。`slide` = 往上收进顶边外的距离(开合动画,全开时 0)。
    fn render_toolbar(&self, slide: f32, cx: &mut Context<Self>) -> AnyElement {
        let percent = format!("{}%", (self.zoom * 100.0).round() as i32);
        div()
            .absolute()
            .top(px(8.0 - slide))
            .right(px(12.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(4.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_default())
            .bg(ui::bg_overlay())
            .child(Self::tool_button(
                "lightbox-zoom-out",
                "−",
                t("fileViewer", "lightboxZoomOut"),
                |this, _window, cx| this.zoom_to(this.zoom / ZOOM_STEP, this.view_center(), cx),
                cx,
            ))
            .child(
                div()
                    .id("lightbox-fit")
                    .min_w(px(48.0))
                    .px(px(4.0))
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_secondary())
                    .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
                    .tip(t("fileViewer", "lightboxFit"))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, _window, cx| {
                        cx.stop_propagation();
                        if this.closing {
                            return;
                        }
                        this.fit();
                        cx.notify();
                    }))
                    .child(percent),
            )
            .child(Self::tool_button(
                "lightbox-zoom-in",
                "+",
                t("fileViewer", "lightboxZoomIn"),
                |this, _window, cx| this.zoom_to(this.zoom * ZOOM_STEP, this.view_center(), cx),
                cx,
            ))
            .child(
                div()
                    .w(px(1.0))
                    .h(px(16.0))
                    .mx(px(3.0))
                    .bg(ui::border_default()),
            )
            .child(Self::tool_button(
                "lightbox-close",
                "×",
                t("fileViewer", "lightboxClose"),
                |this, window, cx| this.dismiss(window, cx),
                cx,
            ))
            .into_any_element()
    }

    /// 底部提示行。`slide` = 往下收进底边外的距离(开合动画,全开时 0)。
    fn render_hint(&self, slide: f32) -> AnyElement {
        div()
            .absolute()
            .bottom(px(10.0 - slide))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .bg(ui::with_alpha(ui::bg_overlay(), 0.85))
                    .text_size(ui::font_px(11.5))
                    .text_color(ui::text_muted())
                    .child(t("fileViewer", "lightboxHint")),
            )
            .into_any_element()
    }
}

/// 实体被 drop(宿主关掉浮层 / 文档页签关掉)时摘掉登记 —— 关闭那一刻已经摘过
/// ([`ImageLightbox::dismiss`]),这里兜的是宿主不经关闭事件、直接丢弃实体那条路
/// (同 `DatePicker`)。
impl Drop for ImageLightbox {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl Render for ImageLightbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = self.presence.drive(window);
        if self.closing && !self.presence.mounted_at(Instant::now()) {
            // 退场播完了,等离场托管层把实体丢掉
            return div();
        }
        let interactive = !self.closing;
        self.sync_viewport(window.viewport_size());
        let viewport = self.viewport;
        let frame = zoom_rect(progress, self.origin, self.image_bounds());
        let dragging = interactive && self.drag.is_some_and(|d| d.moved);
        let pannable = interactive && self.pannable();
        // 工具栏 / 提示行跟着开合从顶边 / 底边滑进滑出(收进窗口外)
        let hidden = 1.0 - progress.clamp(0.0, 1.0);
        let toolbar = self.render_toolbar(hidden * (8.0 + TOOLBAR_H), cx);
        let hint = self.render_hint(hidden * (10.0 + HINT_H));

        div().child(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .id("image-lightbox")
                        .w(viewport.width)
                        .h(viewport.height)
                        // 遮罩压暗随开合渐变(不是内容闪现);图片本身不淡入
                        .bg(gpui::hsla(0.0, 0.0, 0.0, 0.5 * progress))
                        // 退场中不拦:点哪儿都直接落到下面,也不再收键盘
                        .when(interactive, |el| {
                            el.track_focus(&self.focus)
                                .key_context("ImageLightbox")
                                .occlude()
                                .on_key_down(cx.listener(Self::on_key_down))
                                .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                                .on_pinch(cx.listener(Self::on_pinch))
                                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                                .on_mouse_move(cx.listener(Self::on_mouse_move))
                                .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                        })
                        .when(pannable && !dragging, |el| el.cursor_grab())
                        .when(dragging, |el| el.cursor_grabbing())
                        .child(
                            img(self.image.clone())
                                .absolute()
                                .left(px(frame.origin.x))
                                .top(px(frame.origin.y))
                                .w(px(frame.size.width))
                                .h(px(frame.size.height))
                                .rounded(px(6.0))
                                .shadow_lg()
                                .object_fit(gpui::ObjectFit::Fill),
                        )
                        .child(toolbar)
                        .child(hint),
                ),
            )
            .with_priority(1),
        )
    }
}

/// 开合进度 `progress` 下图片该画在哪:中心从 `origin` 那一点走到终矩形的中心,尺寸从 0
/// 等比长到终尺寸 —— 从点开它的地方放大到位。起点零尺寸,收起时缩回那一点就是没了,
/// 不留一块小图「啪」地消失。
fn zoom_rect(progress: f32, origin: Point<f32>, target: gpui::Bounds<f32>) -> gpui::Bounds<f32> {
    let p = progress.clamp(0.0, 1.0);
    if p >= 1.0 {
        return target;
    }
    let lerp = |from: f32, to: f32| from + (to - from) * p;
    let center_x = lerp(origin.x, target.origin.x + target.size.width / 2.0);
    let center_y = lerp(origin.y, target.origin.y + target.size.height / 2.0);
    let (width, height) = (target.size.width * p, target.size.height * p);
    gpui::Bounds {
        origin: point(center_x - width / 2.0, center_y - height / 2.0),
        size: size(width, height),
    }
}

// ─── 纯函数(单测靶子) ─────────────────────────────────────────

/// 「适应窗口」倍率:宽高各自够放的最大倍率取小。不封顶 —— 封顶是
/// [`ImageLightbox::fit`] 的事,这里给缩放下限也要用原值。
fn fit_zoom(natural: Size<f32>, avail: Size<f32>) -> f32 {
    let w = natural.width.max(1.0);
    let h = natural.height.max(1.0);
    (avail.width / w)
        .min(avail.height / h)
        .max(f32::MIN_POSITIVE)
}

/// 手动缩放的 `(下限, 上限)`:下限取 [`MIN_ZOOM`] 与 fit 的小者(超大图的 fit 本身
/// 就可能低于 0.25,得让它缩得回去),上限取 [`MAX_ZOOM`] 与 fit 的大者。
fn zoom_bounds(fit: f32) -> (f32, f32) {
    (MIN_ZOOM.min(fit), MAX_ZOOM.max(fit))
}

/// 单轴夹偏移:图比视口小 → 居中(偏移唯一);比视口大 → 边缘不许离开视口。
fn clamp_axis(offset: f32, image: f32, view: f32) -> f32 {
    if image <= view {
        (view - image) / 2.0
    } else {
        offset.clamp(view - image, 0.0)
    }
}

fn clamp_offset(offset: Point<f32>, image: Size<f32>, view: Size<f32>) -> Point<f32> {
    point(
        clamp_axis(offset.x, image.width, view.width),
        clamp_axis(offset.y, image.height, view.height),
    )
}

/// 围绕 `cursor` 缩放后的新偏移:光标下的那个图上点缩放前后都在光标下。
/// 不夹 —— 调用方随后按新尺寸 [`clamp_offset`]。
fn zoom_about(cursor: Point<f32>, offset: Point<f32>, from: f32, to: f32) -> Point<f32> {
    let ratio = to / from;
    point(
        cursor.x - (cursor.x - offset.x) * ratio,
        cursor.y - (cursor.y - offset.y) * ratio,
    )
}

/// 滚轮位移(像素,向上为正)→ 倍率因子:一格 [`ZOOM_STEP`],连续量按指数插值,
/// 触控板的细碎位移因此也是平滑的。
fn wheel_zoom_factor(dy_px: f32) -> f32 {
    ZOOM_STEP.powf(dy_px / WHEEL_NOTCH_PX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    /// 开合:起点是点击点上的零尺寸,终点严丝合缝落在终矩形上;全程等比(宽高比不变)。
    #[test]
    fn 从点击处放大到位() {
        let target = gpui::Bounds {
            origin: point(100.0, 50.0),
            size: size(400.0, 200.0),
        };
        let click = point(30.0, 600.0);
        let start = zoom_rect(0.0, click, target);
        assert_eq!(start.size, size(0.0, 0.0));
        assert!(close(start.origin.x, click.x) && close(start.origin.y, click.y));
        assert_eq!(zoom_rect(1.0, click, target), target);
        assert_eq!(zoom_rect(2.0, click, target), target, "越界钳住");
        let mid = zoom_rect(0.5, click, target);
        assert!(close(mid.size.width / mid.size.height, 2.0), "宽高比不变");
        let center = point(
            mid.origin.x + mid.size.width / 2.0,
            mid.origin.y + mid.size.height / 2.0,
        );
        assert!(close(center.x, (30.0 + 300.0) / 2.0) && close(center.y, (600.0 + 150.0) / 2.0));
    }

    #[test]
    fn 适应窗口按宽高各自够放取小() {
        // 宽是瓶颈
        assert!(close(
            fit_zoom(size(1000.0, 200.0), size(500.0, 500.0)),
            0.5
        ));
        // 高是瓶颈
        assert!(close(
            fit_zoom(size(200.0, 1000.0), size(500.0, 500.0)),
            0.5
        ));
        // 小图能放大
        assert!(close(fit_zoom(size(100.0, 100.0), size(500.0, 300.0)), 3.0));
        // 零尺寸不除零
        assert!(fit_zoom(size(0.0, 0.0), size(500.0, 300.0)).is_finite());
    }

    #[test]
    fn 缩放上下限随适应倍率放宽() {
        assert_eq!(zoom_bounds(1.0), (MIN_ZOOM, MAX_ZOOM));
        // 超大图:fit 低于下限,下限跟着放低,否则缩不回适应
        assert_eq!(zoom_bounds(0.1), (0.1, MAX_ZOOM));
        // 极小图:fit 高于上限,上限跟着放高
        assert_eq!(zoom_bounds(12.0), (MIN_ZOOM, 12.0));
    }

    #[test]
    fn 图比视口小时居中且拖不动() {
        let out = clamp_offset(point(-999.0, 999.0), size(200.0, 100.0), size(800.0, 600.0));
        assert!(close(out.x, 300.0) && close(out.y, 250.0));
    }

    #[test]
    fn 图比视口大时边缘不离开视口() {
        let img = size(2000.0, 1500.0);
        let view = size(800.0, 600.0);
        // 往右下拖过头 → 左上角贴 0
        let out = clamp_offset(point(50.0, 30.0), img, view);
        assert!(close(out.x, 0.0) && close(out.y, 0.0));
        // 往左上拖过头 → 右下角贴视口右下
        let out = clamp_offset(point(-5000.0, -5000.0), img, view);
        assert!(close(out.x, -1200.0) && close(out.y, -900.0));
        // 中间的值原样保留
        let out = clamp_offset(point(-300.0, -200.0), img, view);
        assert!(close(out.x, -300.0) && close(out.y, -200.0));
    }

    #[test]
    fn 围绕光标缩放光标下的图点不动() {
        let offset = point(-100.0, -50.0);
        let cursor = point(300.0, 200.0);
        let (from, to) = (1.0, 2.0);
        // 缩放前光标落在图上的点
        let on_image = point((cursor.x - offset.x) / from, (cursor.y - offset.y) / from);
        let new_offset = zoom_about(cursor, offset, from, to);
        // 缩放后同一图点的屏幕位置仍是光标
        let screen = point(
            new_offset.x + on_image.x * to,
            new_offset.y + on_image.y * to,
        );
        assert!(close(screen.x, cursor.x) && close(screen.y, cursor.y));
    }

    #[test]
    fn 滚轮一格正好一档且方向对() {
        assert!(close(wheel_zoom_factor(WHEEL_NOTCH_PX), ZOOM_STEP));
        assert!(close(wheel_zoom_factor(-WHEEL_NOTCH_PX), 1.0 / ZOOM_STEP));
        assert!(close(wheel_zoom_factor(0.0), 1.0));
        // 半格是开方,连续量平滑
        assert!(close(
            wheel_zoom_factor(WHEEL_NOTCH_PX / 2.0),
            ZOOM_STEP.sqrt()
        ));
    }
}
