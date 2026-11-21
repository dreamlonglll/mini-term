//! 终端内查找条 —— 浮在终端右上角的那一条(旧版 `TerminalSearchBar.tsx`)。
//!
//! 控件与旧版逐个对齐:关键词输入框、`n/总数` 计数、`Aa` 区分大小写、`ab` 整词、
//! `.*` 正则三个开关、上一个 `↑` / 下一个 `↓`、关闭 `✕`;
//! 快捷键 Enter 下一个、Shift+Enter 上一个、Esc 关闭。
//!
//! # 定位方式与旧版的分水岭
//!
//! 旧版是 `createPortal` 到 `document.body` 的**全局单例**,再用 `requestAnimationFrame`
//! 每帧去量目标 pane 的 `getBoundingClientRect()` 把自己贴过去 —— 因为 React 那边
//! 拿不到「浮层跟着某个 DOM 节点走」的原生手段。
//!
//! GPUI 侧不需要这一套:查找条就是终端容器里的一个 `absolute` 子元素,
//! 分屏、拖分隔条、切 tab 全部由布局自动跟随,**那条 rAF 轮询整个删掉**。
//! 连带删掉的还有旧版里「目标 pane 尺寸为 0 就自动收起查找条」的补丁 ——
//! 那是 portal 方案的并发症,不是需求。
//!
//! # 状态住在哪
//!
//! 关键词、选项、命中集合、当前命中**全在** [`TerminalSearch`] 里,
//! 查找条和渲染层共用同一个 `Rc<RefCell<TerminalSearch>>`。
//! 于是「计数说 12 条、高亮却画了 13 块」这种两套状态打架的经典 bug 从结构上不存在;
//! 查找条本身只持有一个输入框实体和几个回调。
//!
//! # 宿主接线(mt-app 的 `TerminalPane`)
//!
//! 一共五处,全在 `crates/mt-app/src/pane.rs`:
//!
//! ## 1. 字段:一个引擎 + 一个查找条
//!
//! ```ignore
//! pub struct TerminalPane {
//!     // …原有字段不动…
//!     search: Rc<RefCell<mt_ui::TerminalSearch>>,
//!     search_bar: Option<Entity<mt_ui::TerminalSearchBar>>,   // None = 没打开
//! }
//! ```
//!
//! 引擎**常驻**(关键词要活过一次次开关),查找条按需建。
//!
//! ## 2. `TerminalPane::new` 里把引擎交给终端视图
//!
//! ```ignore
//! let search = Rc::new(RefCell::new(mt_ui::TerminalSearch::new()));
//! search.borrow_mut().set_enabled(false);        // 一开始是关着的
//! let view = cx.new(|vcx| {
//!     TerminalView::new(…)
//!         .search(search.clone())                // ← 只多这一行
//!         // …原有的 on_input / on_grid_resize 不动…
//! });
//! ```
//!
//! 高亮从此自动跟着走:视图每帧替引擎跑一次去抖后的重搜,命中格子画上底色,
//! 当前命中另一档色 + 描边。行缓存不受影响(命中状态进的是行签名)。
//!
//! ## 3. Ctrl+F 唤起 / 焦点归还
//!
//! ```ignore
//! fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
//!     if let Some(bar) = self.search_bar.take() {
//!         // 已经开着 → 再按一次收起
//!         bar.update(cx, |bar, cx| bar.close(window, cx));
//!         return;
//!     }
//!     let search = self.search.clone();
//!     let emulator = self.emulator.clone();
//!     let this = cx.weak_entity();
//!     let bar = cx.new(|cx| {
//!         // 第三个参数是文案来源(见下文「文案」一节),mt-app 传 `crate::i18n::terminal_search_labels`
//!         mt_ui::TerminalSearchBar::new(search, emulator, labels, window, cx).on_close(
//!             move |window, cx| {
//!                 let _ = this.update(cx, |pane: &mut TerminalPane, cx| {
//!                     pane.search_bar = None;
//!                     window.focus(&pane.focus);   // ← 焦点必须还给终端
//!                     cx.notify();
//!                 });
//!             },
//!         )
//!     });
//!     bar.update(cx, |bar, cx| bar.open(window, cx));  // 选区预填 + 开引擎 + 聚焦 + 全选
//!     self.search_bar = Some(bar);
//!     cx.notify();
//! }
//! ```
//!
//! 挂到键上(`ctrl-f` / macOS `cmd-f`)—— ⚠️ **不能**在 pane 的容器 div 上挂
//! `on_key_down`:`TerminalView` 的 `on_key_down` 认得 `Ctrl+F`(它不是可打印键,
//! `keystroke_to_bytes` 给出 `\x06`),写进 PTY 之后就 `stop_propagation` 了,
//! 而 gpui 的 key 监听是**从焦点节点往上冒泡**,终端那一层在容器之前。
//!
//! 正确做法是绑成 **action**:gpui 的按键派发「先匹配 action 绑定、后跑 key 监听」,
//! 绑上就等于旧版 capture 阶段那句 `consume(e)`,终端根本看不到这个键。
//!
//! ```ignore
//! // main.rs
//! actions!(mini_term, [TerminalSearch]);
//! KeyBinding::new("ctrl-f", TerminalSearch, Some("Workspace"))
//! // Workspace 的处理器里找到焦点 pane,调 pane.open_search(window, cx)
//! ```
//!
//! (mt-app 侧的实现见 `crates/mt-app/src/main.rs::on_terminal_search`。)
//!
//! ⚠️ **焦点归还是必须的**:不还的话焦点停在已卸载的输入框上,用户接着敲的字
//! 全部落空 —— 旧版注释里那条踩过的坑([`crate::terminal::search`] 一样适用)。
//!
//! ## 4. `render` 里把它叠在终端上
//!
//! ```ignore
//! div().size_full().relative()
//!     .child(self.view.clone())
//!     .when_some(self.search_bar.clone(), |this, bar| {
//!         // 右上角,距顶 6px、距右 14px —— 与旧版 `rect.top + 6` / `rect.right - w - 14` 同款
//!         this.child(div().absolute().top(px(6.)).right(px(14.)).child(bar))
//!     })
//! ```
//!
//! ## 5. pane 关闭 / 换终端时
//!
//! ```ignore
//! self.search.borrow_mut().clear();   // 关键词也一并丢掉
//! self.search_bar = None;
//! ```
//!
//! # 文案
//!
//! 由宿主在 [`TerminalSearchBar::new`] 时注入一个**文案来源**(`Fn() -> SearchBarLabels`),
//! 本 crate 不依赖 mt-i18n —— 字典 `dict.rs` 是生成物、改得勤,依赖它就意味着每次
//! 重生成都把 mt-ui 整个重编。mt-app 传的是 `crate::i18n::terminal_search_labels`
//! (`terminalSearch` 命名空间,与旧版同 key)。
//!
//! 取值时机与注入前一致:按钮提示与「无结果」**每帧现取**(切语言立刻生效),
//! 输入框占位符只在创建时取一次。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, SharedString, Styled,
    Subscription, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, h_flex};
use mt_terminal::TerminalEmulator;

use crate::tooltip::TooltipExt as _;

use super::search::{SearchDirection, SearchOptions, TerminalSearch, escape_literal};

/// 查找条上的全部文案。由宿主经 [`TerminalSearchBar::new`] 的文案来源注入
/// (mt-app 取 mt-i18n 的 `terminalSearch` 命名空间)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchBarLabels {
    /// 整条的名字(旧版 `role="search"` 的 aria-label)。gpui 这边没有无障碍
    /// 树可挂,留着给宿主自己用(如把它当浮层标题)。
    pub title: SharedString,
    pub placeholder: SharedString,
    pub no_results: SharedString,
    pub case_sensitive: SharedString,
    pub whole_word: SharedString,
    pub regex: SharedString,
    pub previous: SharedString,
    pub next: SharedString,
    pub close: SharedString,
}

/// 查找条对外发的事件。宿主用 `cx.subscribe` 收,或者只接
/// [`TerminalSearchBar::on_close`] 那一个回调(多数宿主只需要它)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchBarEvent {
    /// 用户按了 Esc 或点了 `✕`。宿主要把焦点还给终端并收起本条。
    Closed,
    /// 关键词或选项变了(结果集已经刷新过)。
    QueryChanged,
    /// 跳到了上一个 / 下一个命中。
    Navigated(SearchDirection),
}

/// 关闭回调。宿主在这里收起查找条并把焦点还给终端。
pub type OnSearchClose = Rc<dyn Fn(&mut Window, &mut App)>;

/// 浮动查找条。gpui `Entity`,宿主 `absolute` 摆在终端容器右上角即可。
pub struct TerminalSearchBar {
    search: Rc<RefCell<TerminalSearch>>,
    emulator: Arc<TerminalEmulator>,
    input: Entity<InputState>,
    /// 文案来源。[`Render::render`] 每帧调一次,所以切语言不需要额外的刷新通道。
    labels: Rc<dyn Fn() -> SearchBarLabels>,
    on_close: Option<OnSearchClose>,
    /// 是否由自身绘制外框(底色 / 边框色 / 圆角 / 阴影)。
    /// 宿主用外层开合框体(如 `reveal_frame`)统一绘框时设为 `false`,只留 1px 边框占位,
    /// 彻底避免动画期间双层阴影/边框叠加以及落定瞬间阴影骤跳。
    framed: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SearchBarEvent> for TerminalSearchBar {}

impl TerminalSearchBar {
    /// `search` 是与终端视图**共用的同一份**引擎实例。
    ///
    /// `labels` 是文案来源:按**当前**语言返回一份 [`SearchBarLabels`]。
    /// 渲染时每帧调它;输入框占位符只在这里取一次。
    pub fn new(
        search: Rc<RefCell<TerminalSearch>>,
        emulator: Arc<TerminalEmulator>,
        labels: impl Fn() -> SearchBarLabels + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let labels: Rc<dyn Fn() -> SearchBarLabels> = Rc::new(labels);
        let placeholder = labels().placeholder;
        // 上次的关键词住在引擎里(收起查找条不清它),重开时原样填回输入框
        let initial = search.borrow().query().to_string();
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder(placeholder);
            if !initial.is_empty() {
                state.set_value(initial, window, cx);
            }
            state
        });

        let subscription = cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.on_query_changed(cx);
            }
        });

        Self {
            search,
            emulator,
            input,
            labels,
            on_close: None,
            framed: true,
            _subscriptions: vec![subscription],
        }
    }

    /// 是否由自身绘制外框(默认 `true`)。设为 `false` 时由宿主外壳负责绘制底色、边框与阴影。
    pub fn framed(mut self, framed: bool) -> Self {
        self.framed = framed;
        self
    }

    /// 关闭回调(Esc / `✕` / [`Self::close`])。**必须接** —— 焦点要还给终端。
    pub fn on_close(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(f));
        self
    }

    /// 运行时换文案来源(连同输入框占位符)。
    pub fn set_labels(
        &mut self,
        labels: impl Fn() -> SearchBarLabels + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.labels = Rc::new(labels);
        let placeholder = (self.labels)().placeholder;
        self.input
            .update(cx, |state, cx| state.set_placeholder(placeholder, window, cx));
        cx.notify();
    }

    fn resolved_labels(&self) -> SearchBarLabels {
        (self.labels)()
    }

    /// 打开(Ctrl+F;查找条已经开着时宿主再调一次也走这里):终端里有单行、非空白的选区
    /// 就拿它换掉输入框里的词([`search_prefill`]),没有就留着上次的词;再把关键词推给
    /// 引擎搜一遍、聚焦输入框并全选。
    ///
    /// 全选是为了「直接打字就覆盖」—— 与旧版 `el.focus(); el.select();` 同款。
    ///
    /// 已经开着、关键词也没变时不重搜也不滚动:用户可能正翻着回看看别处,再按一次 Ctrl+F
    /// 只是想回到输入框,视口不该被拽回当前命中。
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selection = self.emulator.with_term(|term| term.selection_to_string());
        let regex = self.search.borrow().options().regex;
        if let Some(seed) = search_prefill(selection.as_deref(), regex) {
            // `set_value` 不发 `InputEvent::Change`,关键词在下面直接推给引擎
            self.input
                .update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        let changed = {
            let mut engine = self.search.borrow_mut();
            let reopened = !engine.enabled();
            engine.set_enabled(true);
            let query = self.input.read(cx).value().to_string();
            let changed = engine.set_query(query);
            if reopened || changed {
                engine.refresh(&self.emulator);
                engine.scroll_to_current(&self.emulator);
            }
            changed
        };
        if changed {
            cx.emit(SearchBarEvent::QueryChanged);
        }
        self.focus_input(window, cx);
        cx.notify();
    }

    /// 聚焦输入框并全选。
    ///
    /// 直接调 `InputState::select_all`(gpui-component 0.6.2 起是公开方法)。此前走的是
    /// 推迟一拍再派发 `SelectAll` action:刚建出来的查找条那一刻还没进 dispatch 树,
    /// action 没人接,第一次打开时全选就这么静默落空了。直接改状态不看 dispatch 树。
    pub fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
    }

    /// 收起。关键词与三个开关都留着,下次 [`Self::open`] 接着用。
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.borrow_mut().set_enabled(false);
        cx.emit(SearchBarEvent::Closed);
        if let Some(cb) = self.on_close.clone() {
            cb(window, cx);
        }
        cx.notify();
    }

    /// 下一个命中(Enter / `↓`)。
    pub fn find_next(&mut self, cx: &mut Context<Self>) {
        self.navigate(SearchDirection::Next, cx);
    }

    /// 上一个命中(Shift+Enter / `↑`)。
    pub fn find_previous(&mut self, cx: &mut Context<Self>) {
        self.navigate(SearchDirection::Previous, cx);
    }

    fn navigate(&mut self, direction: SearchDirection, cx: &mut Context<Self>) {
        {
            let mut engine = self.search.borrow_mut();
            match direction {
                SearchDirection::Next => engine.find_next(&self.emulator),
                SearchDirection::Previous => engine.find_previous(&self.emulator),
            };
        }
        cx.emit(SearchBarEvent::Navigated(direction));
        cx.notify();
    }

    fn on_query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().to_string();
        {
            let mut engine = self.search.borrow_mut();
            if !engine.set_query(query) {
                return;
            }
            // 选项/关键词一变就按新规则重搜,否则计数还停在旧结果上
            engine.refresh(&self.emulator);
            // 与旧版 `findNext(..., incremental: true)` 同款:边打字边把第一条命中
            // 滚进视口(已经在视口里就不动)
            engine.scroll_to_current(&self.emulator);
        }
        cx.emit(SearchBarEvent::QueryChanged);
        cx.notify();
    }

    fn toggle_option(&mut self, mutate: impl FnOnce(&mut SearchOptions), cx: &mut Context<Self>) {
        {
            let mut engine = self.search.borrow_mut();
            let mut options = engine.options();
            mutate(&mut options);
            if !engine.set_options(options) {
                return;
            }
            engine.refresh(&self.emulator);
            engine.scroll_to_current(&self.emulator);
        }
        cx.emit(SearchBarEvent::QueryChanged);
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // gpui-component 的 `Input` 把 enter / escape 绑成了 action,但两者的
        // handler 都会 `cx.propagate()`,所以这里照样收得到原始按键。
        // 统一在这一处判,免得 Enter 走事件、Shift+Enter 走按键两套口径。
        match event.keystroke.key.as_str() {
            "escape" => {
                self.close(window, cx);
                cx.stop_propagation();
            }
            "enter" => {
                if event.keystroke.modifiers.shift {
                    self.find_previous(cx);
                } else {
                    self.find_next(cx);
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

impl Focusable for TerminalSearchBar {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

/// 打开查找条时拿终端选区预填什么。`None` = 不预填,输入框留着上次的词。
///
/// - 只认**单行**选区:跨行的选区多半是「选了一段输出想复制」,拿来当关键词也搜不着
///   什么(查找是按行匹配的)。三击整行选出来的串末尾带一个换行(alacritty 的行选区
///   就这么拼),那一个不算跨行;软折行的长行选出来本就不带换行,算一行;
/// - 全是空白不算(只会把上次的词冲掉);
/// - 正则开关开着时把选区**转义成字面量**:选中的是 `a.b(c)` 就该找 `a.b(c)` 本身,
///   不能让 `.` 和括号按正则解释(VS Code 同一手)。
pub fn search_prefill(selection: Option<&str>, regex: bool) -> Option<String> {
    let text = selection?;
    let text = text
        .strip_suffix('\n')
        .map(|t| t.strip_suffix('\r').unwrap_or(t))
        .unwrap_or(text);
    if text.contains(['\n', '\r']) || text.trim().is_empty() {
        return None;
    }
    Some(if regex {
        escape_literal(text)
    } else {
        text.to_string()
    })
}

/// 「n/总数」那一格的文案。抽成自由函数是为了能单测 —— 三种状态(没搜过 /
/// 没结果 / 有结果)在旧版是一串三元表达式,最容易写错的就是它。
pub fn counter_text(active: bool, index: usize, count: usize, no_results: &str) -> String {
    if !active {
        return String::new();
    }
    if count == 0 {
        return no_results.to_string();
    }
    format!("{index}/{count}")
}

/// 给查找条上的按钮套一层带 tooltip 的壳。
///
/// 为什么不用 `Button::tooltip(..)`:那条在 gpui-component 内部直接建它自带的气泡,
/// 字号和停留时长都绕不过去(理由见 [`crate::tooltip`])。壳是个 `flex_none` 的裸
/// div,在 `h_flex` 里既不撑也不缩,排布与之前一致。
fn with_tip(id: &'static str, tip: SharedString, button: Button) -> impl IntoElement {
    div().id(id).flex_none().tip(tip).child(button)
}

impl Render for TerminalSearchBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let labels = self.resolved_labels();
        let (active, options, index, count, has_query, has_error) = {
            let engine = self.search.borrow();
            (
                engine.is_active(),
                engine.options(),
                engine.display_index(),
                engine.count(),
                !engine.query().is_empty(),
                engine.error().is_some(),
            )
        };
        let counter = counter_text(active, index, count, &labels.no_results);
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;

        let mut bar = h_flex()
            .id("terminal-search-bar")
            .key_context("TerminalSearch")
            .on_key_down(cx.listener(Self::on_key_down))
            // 焦点在输入框里时 Ctrl+F(macOS Cmd+F)落在输入框自己的 `Search` 绑定上(比
            // 宿主的全局绑定深),非 searchable 的单行框把它放行、冒到这里:只重新全选。
            // 不再拿终端选区预填 —— 焦点一直在输入框里,终端选区只可能是打开查找条时就
            // 在的那一段(点终端会把焦点拿走),再填一遍会冲掉用户刚改过的词
            .on_action(
                cx.listener(|this, _: &gpui_component::input::Search, window, cx| {
                    this.focus_input(window, cx);
                }),
            )
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_1();
        if self.framed {
            bar = bar
                .rounded_md()
                .bg(cx.theme().popover)
                .border_color(cx.theme().border)
                .shadow_lg();
        }
        bar
            .child(
                Input::new(&self.input)
                    .small()
                    // 旧版是 `w-48` = 12rem = 192px
                    .w(px(192.))
                    .cleanable(false)
                    .shadow_none(),
            )
            .child(
                div()
                    .min_w(px(56.))
                    .text_xs()
                    .text_center()
                    // 正则写错时把计数染红:旧版没有这一档,但 GPUI 侧没有浏览器
                    // 控制台可看,不给提示用户只会看到「无结果」而查不出原因
                    .text_color(if has_error { danger } else { muted })
                    .child(counter),
            )
            .child(with_tip(
                "case-sensitive-tip",
                labels.case_sensitive.clone(),
                Button::new("case-sensitive")
                    .label("Aa")
                    .xsmall()
                    .compact()
                    .ghost()
                    .selected(options.case_sensitive)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.toggle_option(|o| o.case_sensitive = !o.case_sensitive, cx);
                    })),
            ))
            .child(with_tip(
                "whole-word-tip",
                labels.whole_word.clone(),
                Button::new("whole-word")
                    .label("ab")
                    .xsmall()
                    .compact()
                    .ghost()
                    .selected(options.whole_word)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.toggle_option(|o| o.whole_word = !o.whole_word, cx);
                    })),
            ))
            .child(with_tip(
                "regex-tip",
                labels.regex.clone(),
                Button::new("regex")
                    .label(".*")
                    .xsmall()
                    .compact()
                    .ghost()
                    .selected(options.regex)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.toggle_option(|o| o.regex = !o.regex, cx);
                    })),
            ))
            .child(with_tip(
                "previous-tip",
                labels.previous.clone(),
                Button::new("previous")
                    .label("↑")
                    .xsmall()
                    .compact()
                    .ghost()
                    .disabled(!has_query)
                    .on_click(cx.listener(|this, _, _window, cx| this.find_previous(cx))),
            ))
            .child(with_tip(
                "next-tip",
                labels.next.clone(),
                Button::new("next")
                    .label("↓")
                    .xsmall()
                    .compact()
                    .ghost()
                    .disabled(!has_query)
                    .on_click(cx.listener(|this, _, _window, cx| this.find_next(cx))),
            ))
            .child(with_tip(
                "close-tip",
                labels.close.clone(),
                Button::new("close")
                    .label("✕")
                    .xsmall()
                    .compact()
                    .ghost()
                    .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 计数文案的三种状态() {
        // 还没搜(关键词为空 / 查找条关着):这一格留白,不写 0/0
        assert_eq!(counter_text(false, 0, 0, "无结果"), "");
        // 搜了但没命中
        assert_eq!(counter_text(true, 0, 0, "无结果"), "无结果");
        assert_eq!(counter_text(true, 0, 0, "No results"), "No results");
        // 有命中:1-based
        assert_eq!(counter_text(true, 1, 12, "无结果"), "1/12");
        assert_eq!(counter_text(true, 12, 12, "无结果"), "12/12");
    }

    /// 单行非空白选区原样预填(前后空白也留着:用户选的就是那一截)。
    #[test]
    fn 单行选区原样预填() {
        assert_eq!(
            search_prefill(Some("error"), false).as_deref(),
            Some("error")
        );
        assert_eq!(
            search_prefill(Some("  foo bar "), false).as_deref(),
            Some("  foo bar ")
        );
        assert_eq!(
            search_prefill(Some("中文 关键词"), false).as_deref(),
            Some("中文 关键词")
        );
    }

    /// 没选区 / 空串 / 全空白:不预填,输入框留着上次的词。
    #[test]
    fn 没有选区或全是空白时不预填() {
        assert_eq!(search_prefill(None, false), None);
        assert_eq!(search_prefill(Some(""), false), None);
        assert_eq!(search_prefill(Some("   \t "), false), None);
        assert_eq!(search_prefill(Some("\n"), false), None, "空行的行选区");
    }

    /// 跨行选区不预填;三击整行选出来的末尾那一个换行不算跨行(CRLF 也认)。
    #[test]
    fn 跨行选区不预填但行选区末尾换行不算() {
        assert_eq!(search_prefill(Some("a\nb"), false), None);
        assert_eq!(search_prefill(Some("a\r\nb"), false), None);
        assert_eq!(
            search_prefill(Some("a\n\n"), false),
            None,
            "末尾两个换行就是两行"
        );
        assert_eq!(
            search_prefill(Some("cargo build\n"), false).as_deref(),
            Some("cargo build")
        );
        assert_eq!(
            search_prefill(Some("cargo build\r\n"), false).as_deref(),
            Some("cargo build")
        );
    }

    /// 正则开关开着:选区转义成字面量,搜的就是选中的那串字本身。
    #[test]
    fn 正则模式下选区按字面量转义() {
        assert_eq!(
            search_prefill(Some("a.b(c)"), false).as_deref(),
            Some("a.b(c)")
        );
        assert_eq!(
            search_prefill(Some("a.b(c)"), true).as_deref(),
            Some(r"a\.b\(c\)")
        );
        assert_eq!(
            search_prefill(Some("plain"), true).as_deref(),
            Some("plain")
        );
    }
}
