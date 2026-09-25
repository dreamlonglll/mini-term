//! 原生子视图(系统 WebView)与 GPUI 画面的合成:**挖洞** + **逐帧对账**。
//!
//! # 为什么要挖洞
//!
//! gpui-pre 在 Windows 上把整窗画面交给 DirectComposition,目标建成 topmost
//! (`gpui-pre-windows` `directx_renderer.rs`:`CreateTargetForHwnd(hwnd, true)`),
//! 窗口还带 `WS_EX_NOREDIRECTIONBITMAP` —— GPUI 的画面**永远盖在子窗口之上**。
//! WebView2 是子窗口,照常接进来会被整块底色盖住、一个像素都看不见(09-25 探针实测:
//! 普通子窗口、给容器补 `WS_EX_LAYERED` 都是整块底色)。
//!
//! 做法是反过来:GPUI 在 WebView 那块**什么都不画**,再把清屏色改成透明
//! (`WindowBackgroundAppearance::Transparent`;`Opaque` 会清成不透明白,见同文件
//! `render`),子窗口就从洞里透上来。附带的好处是 GPUI 自己的浮层(右键菜单、
//! 弹窗与遮罩、toast、tooltip)天然画在 WebView **之上**,没有「空域」问题。
//! macOS 的 WKWebView 是叠在 Metal 层之上的子 NSView,本来就看得见,挖洞只是无害
//! 的空操作;清屏色只在 Windows 上切。
//!
//! # 一帧的时序
//!
//! GPUI 一帧先把整棵树 prepaint 完,再整棵 paint。于是:
//!
//! 1. 根视图最底层的底色画布 prepaint 时 [`begin_frame`]:清掉上一帧的洞;
//! 2. [`Slot`](WebView 的占位元素)prepaint 时登记洞、摆好子窗口并显示它;
//! 3. paint 期各层底色绕开洞画([`hole_bg`] / [`hole_art`]);
//! 4. 根底色画布 paint 时 [`finish_frame`]:本帧没被摆放的子窗口**在这一帧呈现
//!    之后**隐藏(先藏会让旧画面里的洞露出桌面一帧),有洞时清屏色切透明、没洞了
//!    同样等呈现之后切回不透明。
//!
//! ⚠️ **洞只对这几层生效**:gpui-component `Root`(构造时底色改透明)、根视图底色与
//! 背景图、文档页容器底色。以后在文档区下面新加整块底色,要么也走 [`hole_bg`],
//! 要么洞就会被它堵上 —— 表现是 WebView 那块只剩底色。
//!
//! # 输入的「空域」:画在上面 ≠ 点得到
//!
//! 挖洞只解决了**画**:GPUI 的浮层看起来盖在 WebView 上,但 Windows 派发鼠标看的是
//! 子窗口的几何,落在 WebView 矩形里的点击一律进 WebView —— 弹窗按钮点了没反应
//! (09-25 用户实测;E2E 用 PostMessage 直投主窗口,绕过了命中测试,没测出来)。
//! 两种浮层两种办法,都在 [`finish_frame`] 里对账:
//!
//! - **模态类**(Dialog、[`crate::overlay`] 栈里挡快捷键的那些、[`input_blocker`] 标出的
//!   透明遮罩):整个 WebView `EnableWindow(false)` —— 禁用的子窗口照常显示,但命中
//!   测试会跳过它,点击落回 GPUI;键盘焦点同时还给宿主窗口;
//! - **常驻的不透明浮层**(右侧抽屉、toast 卡片,用 [`occluder`] 登记矩形):从 WebView
//!   的窗口区域里把这块抠掉(`SetWindowRgn`),这块的显示与点击都归 GPUI,页面其余
//!   部分照常可点。洞也同步让出这块(底色照画),半透明浮层底下不会露出桌面。
//!
//! 新加的浮层若会盖到文档区,要么进 overlay 栈,要么挂这两个元素之一,否则就是
//! 「看得见、点不到」。

// Linux 上没有子视图,登记洞的那半边只剩单测在用
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

use std::cell::RefCell;

use gpui::{App, Bounds, ContentMask, Hsla, IntoElement, Pixels, Styled, Window, canvas, fill};

#[derive(Default)]
struct FrameState {
    /// 帧序号,每帧 [`begin_frame`] 加一。[`Slot`] 盖的戳与它比对,判「本帧摆过没有」。
    frame: u64,
    /// 本帧登记的洞(窗口坐标)。
    holes: Vec<Bounds<Pixels>>,
    /// 本帧画在 WebView 之上的不透明浮层矩形([`occluder`])。
    occluders: Vec<Bounds<Pixels>>,
    /// 本帧有透明遮罩要吃掉整窗点击([`input_blocker`])。
    block_input: bool,
    /// 清屏色当前是不是透明(Windows)。
    transparent: bool,
    /// 登记过的子视图。弱引用:页签关掉 = 实体析构 = 自动退场。
    #[cfg(any(windows, target_os = "macos"))]
    views: Vec<std::rc::Weak<hosted::Hosted>>,
}

thread_local! {
    static STATE: RefCell<FrameState> = RefCell::new(FrameState::default());
}

/// 一帧开始(prepaint 期,早于任何 [`Slot`])。
pub fn begin_frame() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.frame += 1;
        s.holes.clear();
        s.occluders.clear();
        s.block_input = false;
    });
}

fn current_frame() -> u64 {
    STATE.with(|s| s.borrow().frame)
}

fn punch(hole: Bounds<Pixels>) {
    STATE.with(|s| s.borrow_mut().holes.push(hole));
}

/// 本帧真正要挖的洞:登记的洞再让出不透明浮层压着的部分(那里照常画底色,
/// 半透明的浮层底下就不会露出桌面)。
fn holes() -> Vec<Bounds<Pixels>> {
    STATE.with(|s| {
        let s = s.borrow();
        s.holes
            .iter()
            .flat_map(|hole| subtract_holes(*hole, &s.occluders))
            .collect()
    })
}

/// 标记「这块是画在 WebView 之上的不透明浮层」:挂在浮层根节点(须是
/// `relative` / `absolute`)下当子节点,本帧的矩形就从 WebView 的显示与点击
/// 区域里让出来。用于常驻、不模态的浮层(右侧抽屉、toast 卡片)。
pub fn occluder() -> impl IntoElement {
    canvas(
        |bounds, _, _| STATE.with(|s| s.borrow_mut().occluders.push(bounds)),
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

/// 标记「本帧有吃整窗点击的透明遮罩」(点外关闭的下拉、自绘模态):WebView
/// 本帧整个不收鼠标,页面照常显示。进了 [`crate::overlay`] 栈的浮层与 Dialog
/// 不必挂,[`finish_frame`] 自己会查。
pub fn input_blocker() -> impl IntoElement {
    canvas(
        |_, _, _| STATE.with(|s| s.borrow_mut().block_input = true),
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

/// `rect` 减去所有 `holes` 之后剩下的矩形(互不重叠)。一个洞最多切出四块:
/// 上、下两条通栏,中间那段的左、右两块。
pub fn subtract_holes(rect: Bounds<Pixels>, holes: &[Bounds<Pixels>]) -> Vec<Bounds<Pixels>> {
    let mut pieces = vec![rect];
    for hole in holes {
        let mut next = Vec::with_capacity(pieces.len() + 3);
        for piece in pieces {
            let Some(cut) = intersection(piece, *hole) else {
                next.push(piece);
                continue;
            };
            let (left, top) = (piece.origin.x, piece.origin.y);
            let (right, bottom) = (left + piece.size.width, top + piece.size.height);
            let (cut_left, cut_top) = (cut.origin.x, cut.origin.y);
            let (cut_right, cut_bottom) = (cut_left + cut.size.width, cut_top + cut.size.height);
            let mut push = |x0: Pixels, y0: Pixels, x1: Pixels, y1: Pixels| {
                if x1 > x0 && y1 > y0 {
                    next.push(Bounds::from_corners(
                        gpui::point(x0, y0),
                        gpui::point(x1, y1),
                    ));
                }
            };
            push(left, top, right, cut_top);
            push(left, cut_bottom, right, bottom);
            push(left, cut_top, cut_left, cut_bottom);
            push(cut_right, cut_top, right, cut_bottom);
        }
        pieces = next;
    }
    pieces
}

fn intersection(a: Bounds<Pixels>, b: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
    let x0 = a.origin.x.max(b.origin.x);
    let y0 = a.origin.y.max(b.origin.y);
    let x1 = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let y1 = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    (x1 > x0 && y1 > y0).then(|| Bounds::from_corners(gpui::point(x0, y0), gpui::point(x1, y1)))
}

/// 绕开本帧的洞,在 `bounds` 剩下的每一块上各画一次(`paint` 在该块的内容遮罩里跑)。
/// 没有洞时就是整块画一次,与直接画完全等价。
fn paint_around_holes(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    mut paint: impl FnMut(Bounds<Pixels>, &mut Window),
) {
    let holes = holes();
    if holes
        .iter()
        .all(|hole| intersection(bounds, *hole).is_none())
    {
        paint(bounds, window);
        return;
    }
    for piece in subtract_holes(bounds, &holes) {
        window.with_content_mask(Some(ContentMask { bounds: piece }), |window| {
            paint(piece, window)
        });
    }
}

/// 绕开洞的整块底色,替代容器上的 `.bg(color)`。宿主须是 `relative`,且把它放在
/// **第一个**子节点(最先画 = 垫在最底下)。
pub fn hole_bg(color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            paint_around_holes(bounds, window, |piece, window| {
                window.paint_quad(fill(piece, color));
            });
        },
    )
    .absolute()
    .inset_0()
}

/// 根视图最底层:绕开洞的窗口底色,顺带驱动每帧的开头与收尾
/// ([`begin_frame`] / [`finish_frame`])。放在根节点的**第一个**子节点。
pub fn root_bg(color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| begin_frame(),
        move |bounds, _, window, cx| {
            finish_frame(window, cx);
            paint_around_holes(bounds, window, |piece, window| {
                window.paint_quad(fill(piece, color));
            });
        },
    )
    .absolute()
    .inset_0()
}

/// 绕开洞的主题背景图(见 [`mt_ui::BackgroundArtElement::paint_into`])。
pub fn hole_art(art: mt_ui::theme_bridge::BackgroundArt) -> impl IntoElement {
    let element = mt_ui::background_art(art);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            paint_around_holes(bounds, window, |_, window| {
                // 图按整块 bounds 摆(cover 的裁切与焦点不随切块变),切块只做遮罩
                element.paint_into(bounds, window, cx);
            });
        },
    )
    .absolute()
    .inset_0()
}

/// 一帧收尾(paint 期,此时全树 prepaint 已完成):没被摆放的子视图等呈现后隐藏,
/// 摆着的按本帧浮层对账输入(见模块注释「输入的空域」),清屏色跟着洞的有无切换。
fn finish_frame(window: &mut Window, cx: &mut App) {
    let frame = current_frame();
    let any_hole = STATE.with(|s| !s.borrow().holes.is_empty());

    #[cfg(any(windows, target_os = "macos"))]
    {
        use gpui_component::WindowExt as _;
        let (views, occluders, block_input) = STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.views.retain(|view| view.strong_count() > 0);
            let views: Vec<_> = s.views.iter().filter_map(|view| view.upgrade()).collect();
            (views, s.occluders.clone(), s.block_input)
        });
        let modal = block_input
            || window.has_active_dialog(cx)
            || !crate::overlay::allows(crate::overlay::Yield::ToOverlay);
        for view in views {
            if view.stamped() == frame {
                view.sync_input(!modal, &occluders, window.scale_factor());
                continue;
            }
            if view.shown() {
                let weak = std::rc::Rc::downgrade(&view);
                // 呈现之后再藏:`on_next_frame` 在下一帧开画前回调,此时本帧(已经
                // 不带这个洞)已经上屏
                window.on_next_frame(move |_, _| {
                    if let Some(view) = weak.upgrade()
                        && view.stamped() != current_frame()
                    {
                        view.hide();
                    }
                });
            }
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = cx;

    let transparent = STATE.with(|s| s.borrow().transparent);
    if any_hole && !transparent {
        STATE.with(|s| s.borrow_mut().transparent = true);
        #[cfg(windows)]
        window.set_background_appearance(gpui::WindowBackgroundAppearance::Transparent);
    } else if !any_hole && transparent {
        STATE.with(|s| s.borrow_mut().transparent = false);
        #[cfg(windows)]
        window.on_next_frame(|window, _| {
            // 期间又开出了洞就不切(那一帧的收尾已把标记置回透明)
            if !STATE.with(|s| s.borrow().transparent) {
                window.set_background_appearance(gpui::WindowBackgroundAppearance::Opaque);
            }
        });
        #[cfg(not(windows))]
        let _ = window;
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub use hosted::{Hosted, Slot};

#[cfg(any(windows, target_os = "macos"))]
mod hosted {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use gpui::{
        App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
        LayoutId, MouseDownEvent, Pixels, Size, Style, Window,
    };

    use super::{STATE, current_frame, punch};

    /// 容器内物理像素矩形 `(左, 上, 右, 下)`。
    type PxRect = (i32, i32, i32, i32);

    /// 一个挂在 GPUI 窗口上的系统 WebView,以及它在合成里的记账。
    pub struct Hosted {
        webview: gpui_wry::WebView,
        /// wry 包在 WebView2 外面的那层容器子窗口(`WRY_WEBVIEW`)。禁用与裁区域都
        /// 作用在它身上 —— 子窗口的命中测试与显示都受父窗口约束,里面 Chromium 的
        /// 几层窗口不必一一去碰。
        #[cfg(windows)]
        container: isize,
        /// 最近一次被 [`Slot`] 摆放的帧序号。
        stamped: Cell<u64>,
        shown: Cell<bool>,
        /// 上次设给子窗口的物理像素矩形,没变就不再 `SetWindowPos`。
        placed: Cell<Option<(i32, i32, u32, u32)>>,
        /// 当前收不收鼠标(模态浮层压着时禁用)。
        input: Cell<bool>,
        /// 当前从窗口区域里抠掉的矩形,连同当时的容器尺寸(尺寸变了区域要重算)。
        clip: RefCell<(u32, u32, Vec<PxRect>)>,
    }

    impl Hosted {
        /// 接管一个刚建好的 WebView 并登记进逐帧对账。建好时先藏着,
        /// 等第一次被 [`Slot`] 摆放才现身。
        pub fn register(webview: wry::WebView, window: &mut Window, cx: &mut App) -> Rc<Self> {
            #[cfg(windows)]
            let container = win::container_of(&webview);
            let mut webview = gpui_wry::WebView::new(webview, window, cx);
            webview.hide();
            let hosted = Rc::new(Self {
                webview,
                #[cfg(windows)]
                container,
                stamped: Cell::new(0),
                shown: Cell::new(false),
                placed: Cell::new(None),
                input: Cell::new(true),
                clip: RefCell::new((0, 0, Vec::new())),
            });
            STATE.with(|s| s.borrow_mut().views.push(Rc::downgrade(&hosted)));
            hosted
        }

        /// 按本帧的浮层对账输入(见模块注释「输入的空域」):`enabled == false` 时整个
        /// 禁用并把键盘焦点还给宿主;`occluders`(窗口逻辑坐标)压着的部分从窗口区域
        /// 里抠掉。都只在变了的时候才调系统 API。
        pub(super) fn sync_input(&self, enabled: bool, occluders: &[Bounds<Pixels>], scale: f32) {
            if self.input.replace(enabled) != enabled {
                if !enabled {
                    let _ = self.webview.raw().focus_parent();
                }
                #[cfg(windows)]
                win::enable(self.container, enabled);
            }
            let Some((ox, oy, width, height)) = self.placed.get() else {
                return;
            };
            let cuts: Vec<PxRect> = occluders
                .iter()
                .filter_map(|o| {
                    // 向外取整:浮层边缘那半个像素也归 GPUI
                    let l = ((f32::from(o.origin.x) * scale).floor() as i32 - ox).max(0);
                    let t = ((f32::from(o.origin.y) * scale).floor() as i32 - oy).max(0);
                    let r = ((f32::from(o.origin.x + o.size.width) * scale).ceil() as i32 - ox)
                        .min(width as i32);
                    let b = ((f32::from(o.origin.y + o.size.height) * scale).ceil() as i32 - oy)
                        .min(height as i32);
                    (r > l && b > t).then_some((l, t, r, b))
                })
                .collect();
            let next = (width, height, cuts);
            if *self.clip.borrow() == next {
                return;
            }
            #[cfg(windows)]
            win::set_region(self.container, width, height, &next.2);
            *self.clip.borrow_mut() = next;
        }

        pub fn webview(&self) -> &wry::WebView {
            self.webview.raw()
        }

        pub(super) fn stamped(&self) -> u64 {
            self.stamped.get()
        }

        pub(super) fn shown(&self) -> bool {
            self.shown.get()
        }

        pub(super) fn hide(&self) {
            if !self.shown.replace(false) {
                return;
            }
            let raw = self.webview.raw();
            // 键盘焦点还给宿主窗口,否则藏起来的 WebView 还在吃按键
            let _ = raw.focus_parent();
            let _ = raw.set_visible(false);
            #[cfg(windows)]
            {
                use wry::WebViewExtWindows as _;
                let _ = raw.set_memory_usage_level(wry::MemoryUsageLevel::Low);
            }
        }

        fn show(&self) {
            if self.shown.replace(true) {
                return;
            }
            let raw = self.webview.raw();
            #[cfg(windows)]
            {
                use wry::WebViewExtWindows as _;
                let _ = raw.set_memory_usage_level(wry::MemoryUsageLevel::Normal);
            }
            let _ = raw.set_visible(true);
        }

        /// 摆到 `bounds`(窗口逻辑坐标),返回该挖的洞。
        ///
        /// 子窗口按物理像素**向外**取整(左上取 floor、右下取 ceil),洞按物理像素
        /// **向内**取整:边缘那半个像素由 GPUI 的底色压住,而不是露出洞底下的桌面。
        fn place(&self, bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
            let left = f32::from(bounds.origin.x) * scale;
            let top = f32::from(bounds.origin.y) * scale;
            let right = f32::from(bounds.origin.x + bounds.size.width) * scale;
            let bottom = f32::from(bounds.origin.y + bounds.size.height) * scale;
            let outer = (
                left.floor() as i32,
                top.floor() as i32,
                (right.ceil() - left.floor()).max(0.0) as u32,
                (bottom.ceil() - top.floor()).max(0.0) as u32,
            );
            if self.placed.get() != Some(outer) {
                self.placed.set(Some(outer));
                let _ = self.webview.raw().set_bounds(wry::Rect {
                    position: wry::dpi::PhysicalPosition::new(outer.0, outer.1).into(),
                    size: wry::dpi::PhysicalSize::new(outer.2, outer.3).into(),
                });
            }
            let (l, t) = (left.ceil() / scale, top.ceil() / scale);
            let (r, b) = (right.floor() / scale, bottom.floor() / scale);
            Bounds::from_corners(
                gpui::point(gpui::px(l), gpui::px(t)),
                gpui::point(gpui::px(r.max(l)), gpui::px(b.max(t))),
            )
        }
    }

    #[cfg(windows)]
    mod win {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Gdi::{
            CombineRgn, CreateRectRgn, DeleteObject, RGN_DIFF, SetWindowRgn,
        };
        use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;

        use super::PxRect;

        /// WebView2 控制器挂在哪个窗口下 —— 即 wry 建的 `WRY_WEBVIEW` 容器。
        pub(super) fn container_of(webview: &wry::WebView) -> isize {
            use wry::WebViewExtWindows as _;
            let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: ParentWindow 只往给定位置写一个 HWND(指针大小)
            let _ = unsafe {
                webview
                    .controller()
                    .ParentWindow((&mut raw as *mut *mut std::ffi::c_void).cast())
            };
            raw as isize
        }

        /// 禁用的子窗口照常显示,但鼠标命中测试会跳过它,点击落到父窗口(GPUI)。
        pub(super) fn enable(container: isize, enabled: bool) {
            if container == 0 {
                return;
            }
            // SAFETY: 句柄来自本进程的子窗口;窗口已销毁时调用只是失败
            let _ = unsafe { EnableWindow(HWND(container as *mut _), enabled) };
        }

        /// 窗口区域 = 整块减去 `cuts`;`cuts` 为空时撤掉区域。区域同时裁显示与命中。
        pub(super) fn set_region(container: isize, width: u32, height: u32, cuts: &[PxRect]) {
            if container == 0 {
                return;
            }
            let hwnd = HWND(container as *mut _);
            // SAFETY: 区域句柄都是这里新建的;SetWindowRgn 成功后归系统所有,失败才自己删
            unsafe {
                if cuts.is_empty() {
                    SetWindowRgn(hwnd, None, true);
                    return;
                }
                let region = CreateRectRgn(0, 0, width as i32, height as i32);
                for &(l, t, r, b) in cuts {
                    let cut = CreateRectRgn(l, t, r, b);
                    CombineRgn(Some(region), Some(region), Some(cut), RGN_DIFF);
                    let _ = DeleteObject(cut.into());
                }
                if SetWindowRgn(hwnd, Some(region), true) == 0 {
                    let _ = DeleteObject(region.into());
                }
            }
        }
    }

    /// WebView 在 GPUI 布局里的占位:撑满父容器,本帧画到哪、子窗口就摆到哪。
    /// 本帧没画到它(切页签、切回源码、页签关掉)= 收尾时子窗口被藏起来。
    pub struct Slot {
        hosted: Rc<Hosted>,
    }

    impl Slot {
        pub fn new(hosted: Rc<Hosted>) -> Self {
            Self { hosted }
        }
    }

    impl IntoElement for Slot {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for Slot {
        type RequestLayoutState = ();
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> {
            None
        }

        fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
            None
        }

        fn request_layout(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (LayoutId, ()) {
            let style = Style {
                size: Size::full(),
                ..Default::default()
            };
            (window.request_layout(style, [], cx), ())
        }

        fn prepaint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            bounds: Bounds<Pixels>,
            _: &mut (),
            window: &mut Window,
            _: &mut App,
        ) {
            // 被祖先裁掉的部分(滚动容器、页签切换动画)不该露出子窗口
            let visible = window.content_mask().bounds.intersect(&bounds);
            if visible.size.width <= gpui::px(0.0) || visible.size.height <= gpui::px(0.0) {
                return;
            }
            self.hosted.stamped.set(current_frame());
            let hole = self.hosted.place(visible, window.scale_factor());
            punch(hole);
            self.hosted.show();
        }

        fn paint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            bounds: Bounds<Pixels>,
            _: &mut (),
            _: &mut (),
            window: &mut Window,
            _: &mut App,
        ) {
            // 点到 WebView 外面(GPUI 的地盘)时把键盘焦点还给宿主窗口:子窗口拿着
            // Win32 焦点时,GPUI 的快捷键与输入框一概收不到按键
            let hosted = Rc::downgrade(&self.hosted);
            window.on_mouse_event(move |event: &MouseDownEvent, _, _, _| {
                if !bounds.contains(&event.position)
                    && let Some(hosted) = hosted.upgrade()
                {
                    let _ = hosted.webview.raw().focus_parent();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    fn area(rects: &[Bounds<Pixels>]) -> f32 {
        rects
            .iter()
            .map(|r| f32::from(r.size.width) * f32::from(r.size.height))
            .sum()
    }

    #[test]
    fn 没有洞时原样一块() {
        let r = rect(0.0, 0.0, 100.0, 50.0);
        assert_eq!(subtract_holes(r, &[]), vec![r]);
    }

    #[test]
    fn 中间一个洞切成四块且面积守恒() {
        let r = rect(0.0, 0.0, 100.0, 100.0);
        let pieces = subtract_holes(r, &[rect(20.0, 30.0, 40.0, 20.0)]);
        assert_eq!(pieces.len(), 4);
        assert_eq!(area(&pieces), 100.0 * 100.0 - 40.0 * 20.0);
        for piece in &pieces {
            assert!(intersection(*piece, rect(20.0, 30.0, 40.0, 20.0)).is_none());
        }
    }

    #[test]
    fn 洞贴边或超出时只剩实际相交之外的部分() {
        let r = rect(0.0, 0.0, 100.0, 100.0);
        // 洞覆盖右下角并越界
        let pieces = subtract_holes(r, &[rect(50.0, 50.0, 100.0, 100.0)]);
        assert_eq!(area(&pieces), 100.0 * 100.0 - 50.0 * 50.0);
        // 洞整块盖住
        assert!(subtract_holes(r, &[rect(-10.0, -10.0, 200.0, 200.0)]).is_empty());
        // 洞在外面
        assert_eq!(subtract_holes(r, &[rect(200.0, 0.0, 10.0, 10.0)]), vec![r]);
    }

    #[test]
    fn 洞让出不透明浮层压着的部分_下一帧清空() {
        begin_frame();
        punch(rect(100.0, 100.0, 400.0, 300.0));
        // 右侧抽屉压住洞的右边 100px
        STATE.with(|s| {
            s.borrow_mut()
                .occluders
                .push(rect(400.0, 0.0, 300.0, 800.0))
        });
        let effective = holes();
        assert_eq!(area(&effective), 300.0 * 300.0);
        assert!(
            effective
                .iter()
                .all(|h| f32::from(h.origin.x + h.size.width) <= 400.0)
        );

        begin_frame();
        assert!(holes().is_empty(), "洞与浮层都只活一帧");
        assert!(STATE.with(|s| s.borrow().occluders.is_empty() && !s.borrow().block_input));
    }

    #[test]
    fn 多个洞逐个扣除() {
        let r = rect(0.0, 0.0, 100.0, 100.0);
        let pieces = subtract_holes(
            r,
            &[rect(0.0, 0.0, 10.0, 10.0), rect(50.0, 50.0, 10.0, 10.0)],
        );
        assert_eq!(area(&pieces), 100.0 * 100.0 - 200.0);
    }
}
