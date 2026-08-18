//! [`TerminalView`] —— 把 [`TerminalElement`] 包成一个 gpui `Entity`。
//!
//! # 为什么必须有这一层
//!
//! `EntityInputHandler`(IME 的唯一入口)要求实现者是一个 `Entity`,而 Element
//! 不是。原来 `TerminalElement` 留了 [`InstallInputHandler`] 这个挂载点让宿主去接,
//! 但「谁持有预编辑串、谁把提交的字节送回 PTY、谁负责在组合期间不让按键漏进终端」
//! 这一整套是**终端自己的**逻辑,散到宿主里每个宿主都要抄一遍。
//!
//! 所以这一层收下三件事:焦点、键盘、IME。宿主(mt-app 的 `TerminalPane`)只剩
//! 「给我一个 emulator,把要写的字节交给我处理」。
//!
//! # 键盘的两条路,以及为什么必须分流
//!
//! ```text
//!                      ┌─ 可打印字符 ─→ 冒泡 → TranslateMessage → WM_CHAR / IME
//! WM_KEYDOWN → KeyDown ┤                                              ↓
//!                      └─ 其它键 ─→ keystroke_to_bytes → PTY    replace_text_in_range → PTY
//!                                   + stop_propagation
//! ```
//!
//! gpui 在 Windows 上**只有在没人 `stop_propagation` 时**才调 `TranslateMessage`。
//! 于是:
//!
//! - 可打印字符如果在 `KeyDown` 里就写进 PTY,IME 那条路根本不会启动
//!   —— 中文输入法下按 `n` 会既写一个 `n`,又开始拼音组合,一个字变两个;
//! - 反过来,方向键 / Ctrl 组合如果不 `stop_propagation`,`WM_CHAR` 会再来一遍。
//!   （实际上 gpui 的 `parse_char_message` 会把控制字符滤掉,所以这条更多是
//!   语义整洁;但 `space` 这类**会**产生可打印字符的键必须 stop,否则真的双份。）
//!
//! 判据只有一个:[`is_text_input_key`]。它与 `parse_char_message` 的过滤规则对齐,
//! 两边加起来不重不漏。
//!
//! # 组合期间不会漏键
//!
//! 平台在派发 `KeyDown` 之前会先问 `marked_text_range()`,非 `None` 就把这次按键
//! 整个让给 IME。所以「组合中按方向键选候选」不会被终端当成方向键写进 PTY ——
//! 前提是 [`ImeState`] 在组合结束时**真的**把 marked range 收回 `None`
//! （空串提交、退格删光都算结束,见 `ime.rs` 里那条注释）。
//!
//! # 宿主接线（mt-app 的 `TerminalPane` 怎么改）
//!
//! 一共四处,都在 `crates/mt-app/src/pane.rs`：
//!
//! ## 1. 结构体加一个字段
//!
//! ```ignore
//! pub struct TerminalPane {
//!     // …原有字段不动…
//!     view: Entity<TerminalView>,
//! }
//! ```
//!
//! ## 2. `TerminalPane::new` 里把焦点句柄提前,再建视图
//!
//! ```ignore
//! let focus = cx.focus_handle();          // 原本在函数末尾,提到这里
//! let this = cx.weak_entity();
//! let this_for_input = this.clone();
//! let view = cx.new(|vcx| {
//!     TerminalView::new(
//!         ("terminal", pty_id),
//!         emulator.clone(),
//!         focus.clone(),
//!         style.clone(),
//!         theme.clone(),
//!         vcx,
//!     )
//!     // 原来挂在 TerminalElement 上的两个回调,原样搬过来
//!     .on_grid_resize(move |size, _window, cx| { /* 与现状一字不改 */ })
//!     .on_input(move |bytes, _window, cx| {
//!         let bytes = bytes.to_vec();
//!         let _ = this_for_input.update(cx, |pane: &mut TerminalPane, _cx| pane.write(&bytes));
//!     })
//! });
//! ```
//!
//! `on_input` 现在是**唯一**的写 PTY 通道(键盘 / 粘贴 / IME 提交 / 鼠标上报 /
//! alt screen 滚轮全走它),所以 `pane.write()` 里的 AI 感知旁路一处不落 ——
//! 「`observe_input` 必须在字节交给 PTY 之前」那条时序也原样保住。
//!
//! ## 3. `render` 里把整块 `TerminalElement` 换成一行
//!
//! ```ignore
//! div()
//!     .size_full()
//!     .relative()
//!     .bg(self.theme.background)
//!     .child(self.view.clone())          // ← 只剩这一行
//!     .when(self.exited, …)
//! ```
//!
//! **要删掉的**：`.track_focus(&self.focus)` / `.key_context("Terminal")` /
//! `.on_key_down(cx.listener(Self::on_key_down))` / 左键聚焦的 `.on_mouse_down`
//! ——这四样现在由 [`TerminalView`] 自己做。留着会导致按键被处理两遍。
//! `TerminalPane::on_key_down` 整个方法可以删（`paste` 同理）。
//!
//! `.bg()` 留在宿主：主题带背景图时终端背景是半透明的,画两层等于把透明度平方。
//!
//! ## 4. OSC 调色板应答改成一行
//!
//! ```ignore
//! TermEvent::ColorRequest(index, format) => {
//!     let rgb = mt_ui::terminal_color_rgb(&self.emulator, &self.theme, index);
//!     self.write_raw(format(rgb).as_bytes());
//! }
//! ```
//!
//! 原来的 `theme_color_rgb` 可以删 —— 它按 `theme.ansi.get(index)` 取,而
//! index 256/257/258 是前景/背景/光标,越界一律回前景,等于把「查背景色」
//! 答成前景色。
//!
//! ## 换主题时
//!
//! `self.view.update(cx, |v, cx| v.set_theme(theme, cx))`；`TerminalPane` 自己那份
//! `theme` 字段仍要更新（`.bg()` 与 OSC 应答用得着）。

use std::cell::Cell as StdCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use alacritty_terminal::grid::Scroll;
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementId, ElementInputHandler, EntityInputHandler,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Pixels, Point, Render, Styled, UTF16Selection, Window, div,
};
use mt_terminal::{TermSize, TerminalEmulator};

use super::damage::DamageStats;
use super::element::{FrameGeometry, OnGridResize, OnInput, PreeditText, TerminalElement};
use super::ime::{ImeState, commit_to_bytes};
use super::input::{is_text_input_key, keystroke_to_bytes, paste_to_bytes};
use super::theme::{TerminalStyle, TerminalTheme};

pub struct TerminalView {
    id: ElementId,
    emulator: Arc<TerminalEmulator>,
    focus: FocusHandle,
    style: TerminalStyle,
    theme: TerminalTheme,
    ime: ImeState,
    /// 元素每帧回填的几何信息(IME 候选框定位靠它)。
    geometry: Rc<StdCell<FrameGeometry>>,
    /// 元素每帧回填的 damage 统计(诊断用)。
    damage: Rc<StdCell<DamageStats>>,
    on_input: Option<OnInput>,
    on_grid_resize: Option<OnGridResize>,
}

impl TerminalView {
    /// `focus` 由宿主给:宿主往往要自己 `window.focus(&handle)`(切 tab、点分屏),
    /// 让它保留句柄的所有权比反过来从视图里掏要省事。
    /// **`track_focus` 由本视图调**,宿主不要再调一次。
    pub fn new(
        id: impl Into<ElementId>,
        emulator: Arc<TerminalEmulator>,
        focus: FocusHandle,
        style: TerminalStyle,
        theme: TerminalTheme,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            id: id.into(),
            emulator,
            focus,
            style,
            theme,
            ime: ImeState::default(),
            geometry: Rc::new(StdCell::new(FrameGeometry::default())),
            damage: Rc::new(StdCell::new(DamageStats::default())),
            on_input: None,
            on_grid_resize: None,
        }
    }

    /// 视图要往 PTY 写字节时的出口。**所有**输入都走这一条:键盘、粘贴、
    /// IME 提交、鼠标上报、alt screen 滚轮。
    ///
    /// 宿主在这里做 AI 感知旁路等副作用 —— 与旧版 `write_pty` 的位置等价。
    pub fn on_input(mut self, f: impl Fn(&[u8], &mut Window, &mut App) + 'static) -> Self {
        self.on_input = Some(Rc::new(f));
        self
    }

    /// grid 尺寸变了(窗口拖动 / 分屏比例变化)就回调,宿主据此 resize PTY。
    pub fn on_grid_resize(mut self, f: impl Fn(TermSize, &mut Window, &mut App) + 'static) -> Self {
        self.on_grid_resize = Some(Rc::new(f));
        self
    }

    pub fn emulator(&self) -> &Arc<TerminalEmulator> {
        &self.emulator
    }

    pub fn theme(&self) -> &TerminalTheme {
        &self.theme
    }

    /// 换配色(主题包切换)。行渲染缓存会因帧指纹/行签名变化自动作废。
    pub fn set_theme(&mut self, theme: TerminalTheme, cx: &mut Context<Self>) {
        if self.theme != theme {
            self.theme = theme;
            cx.notify();
        }
    }

    pub fn style(&self) -> &TerminalStyle {
        &self.style
    }

    /// 换字体 / 字号。cell 尺寸随之变化,下一帧会连带 resize grid 与 PTY。
    pub fn set_style(&mut self, style: TerminalStyle, cx: &mut Context<Self>) {
        if self.style != style {
            self.style = style;
            cx.notify();
        }
    }

    /// 正在 IME 组合中。宿主想加「组合时别切走焦点」之类的守卫可以问它。
    pub fn is_composing(&self) -> bool {
        self.ime.is_composing()
    }

    /// 丢弃组合中的预编辑串(切 tab / 关 pane 之前调,免得残影留在画面上)。
    pub fn clear_preedit(&mut self, cx: &mut Context<Self>) {
        if self.ime.is_composing() {
            self.ime.clear();
            cx.notify();
        }
    }

    /// 最近一帧的 damage 统计(诊断 / 测试用)。
    pub fn damage_stats(&self) -> DamageStats {
        self.damage.get()
    }

    /// 最近一帧的几何信息。
    pub fn frame_geometry(&self) -> FrameGeometry {
        self.geometry.get()
    }

    /// 把选中文本送进剪贴板。没有选择时什么也不做。
    pub fn copy_selection(&self, cx: &mut App) -> bool {
        match self.emulator.with_term(|t| t.selection_to_string()) {
            Some(text) if !text.is_empty() => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            _ => false,
        }
    }

    /// 粘贴剪贴板内容(按 bracketed paste 模式编码)。
    pub fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|it| it.text()) else {
            return;
        };
        let bytes = paste_to_bytes(&text, self.emulator.mode());
        self.scroll_to_bottom();
        self.write(&bytes, window, cx);
        cx.notify();
    }

    /// 直接写字节(宿主的程序化输入,如「发送到终端」)。
    pub fn write(&mut self, bytes: &[u8], window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cb) = self.on_input.clone() {
            cb(bytes, window, cx);
        }
    }

    /// 有输入就回到底部 —— 和所有终端一样。
    fn scroll_to_bottom(&self) {
        self.emulator
            .with_term_mut(|term| term.scroll_display(Scroll::Bottom));
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;

        // 组合中平台本不该派发到这里(它会先问 marked_text_range)。
        // 万一某个平台没做这一步,这里兜住:一律让给 IME,绝不写进 PTY。
        if self.ime.is_composing() {
            return;
        }

        // 应用层快捷键:Ctrl+Shift+C / Ctrl+Shift+V。
        if mods.control && mods.shift {
            match keystroke.key.as_str() {
                "c" => {
                    self.copy_selection(cx);
                    cx.stop_propagation();
                }
                "v" => {
                    self.paste(window, cx);
                    cx.stop_propagation();
                }
                // 其余 Ctrl+Shift 组合留给宿主(新建标签 / 切 pane…),继续冒泡
                _ => {}
            }
            return;
        }

        // 可打印字符:**必须**放行,让 TranslateMessage 走到 IME / WM_CHAR。
        // 这是整个 IME 能工作的前提,不要图省事在这里直接写字节。
        if is_text_input_key(keystroke) {
            return;
        }

        let Some(bytes) = keystroke_to_bytes(keystroke, self.emulator.mode()) else {
            return;
        };
        self.scroll_to_bottom();
        self.write(&bytes, window, cx);
        // 消费掉:不 stop 的话 space 这类键会再从 WM_CHAR 回来一次
        cx.stop_propagation();
        cx.notify();
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let focus = self.focus.clone();

        let mut element = TerminalElement::new(
            self.id.clone(),
            self.emulator.clone(),
            self.focus.clone(),
            self.style.clone(),
            self.theme.clone(),
        )
        .preedit(self.ime.preedit().map(|p| PreeditText {
            text: p.text.clone().into(),
            cursor_byte: p.cursor_byte(),
        }))
        .geometry_sink(self.geometry.clone())
        .damage_sink(self.damage.clone())
        // 每帧重新登记:`Window::handle_input` 只在**当前焦点是这个句柄**时才生效,
        // 而且是「下一帧」级别的注册,不是一次性的全局安装。
        .with_input_handler(move |bounds, window, cx| {
            window.handle_input(
                &focus,
                ElementInputHandler::new(bounds, entity.clone()),
                cx,
            );
        });

        if let Some(cb) = self.on_grid_resize.clone() {
            element = element.on_grid_resize(move |size, window, cx| cb(size, window, cx));
        }
        if let Some(cb) = self.on_input.clone() {
            element = element.on_input(move |bytes, window, cx| cb(bytes, window, cx));
        }

        div()
            .size_full()
            .track_focus(&self.focus)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, window, _cx| {
                    window.focus(&this.focus);
                }),
            )
            .child(element)
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.ime.text_for_range_utf16(range.clone())?;
        // 实际返回的长度可能比请求的短(区间被钳过),按约定回填真实区间
        let actual = range.start..range.start + text.encode_utf16().count();
        if actual != range {
            *adjusted_range = Some(actual);
        }
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // 没在组合时也要返回 `Some(0..0)`:返回 None 会让部分 IME 认定
        // 这个控件不接受输入,连候选框都不弹
        Some(UTF16Selection {
            range: self.ime.selected_range_utf16(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.ime.marked_range_utf16()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.ime.clear();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 这条路有两个来源:IME 上屏,以及**普通可打印字符的 WM_CHAR**。
        // 两者在这里是同一件事 —— 都是「一段确定的文本要进 PTY」。
        let committed = self.ime.commit(text);
        cx.notify();
        let Some(text) = committed else {
            return;
        };
        self.scroll_to_bottom();
        let bytes = commit_to_bytes(&text);
        self.write(&bytes, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime.set_marked(range_utf16, new_text, new_selected_range);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let geometry = self.geometry.get();
        // 组合中:贴着预编辑串里的插入符;否则贴着终端光标格。
        // 两个都没有(还没画过一帧)时退回元素左上角 —— 总比不弹候选框强。
        Some(
            geometry
                .preedit_caret
                .or(geometry.cursor)
                .unwrap_or_else(|| Bounds::new(element_bounds.origin, geometry.cell_size)),
        )
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // 「鼠标点在文档的第几个字符」——终端没有可编辑文档,不支持。
        // macOS 的字典查词(三指轻点)会用它,返回 None 表示这里没有可查的文本。
        None
    }
}
