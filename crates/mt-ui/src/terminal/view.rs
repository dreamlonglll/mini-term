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
//! ⚠️ **上面这张图的前提是 `KeyDown` 真能走到这里**。gpui 的按键派发是
//! 「先匹配 keymap 里的 action 绑定,匹配上就 dispatch 并结束,匹配不上才跑
//! `on_key_down`」——所以任何绑到本 `key_context`(`Terminal`)或其祖先 context
//! 上的裸键位,终端一个字节都收不到。组件库 `gpui_component::Root` 就在
//! `Root` context 上绑了裸 `tab` / `shift-tab`(焦点导航),宿主必须用
//! `NoAction` 在 `Terminal` context 上压掉,见 `mt-app::hotkeys::bind_keys`。
//! 症状是「按 Tab 没反应,而且焦点被挪走、要重新点终端才能打字」。
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
//!     .child(self.view.clone())          // ← 只剩这一行
//!     .when(self.exited, …)
//! ```
//!
//! **要删掉的**：`.track_focus(&self.focus)` / `.key_context("Terminal")` /
//! `.on_key_down(cx.listener(Self::on_key_down))` / 左键聚焦的 `.on_mouse_down`
//! ——这四样现在由 [`TerminalView`] 自己做。留着会导致按键被处理两遍。
//! `TerminalPane::on_key_down` 整个方法可以删（`paste` 同理）。
//!
//! 宿主也**不要 `.bg()`**：主题带背景图时终端背景是半透明的,着色只保留最外层
//! 容器一层(mt-app 是 TerminalArea 根),宿主/视图再刷就是透明度叠乘把图盖死
//! (原版 `themePackManager.ts:294` 的单层口径)。
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
//! `theme` 字段仍要更新（OSC 应答用得着）。
//!
//! # 后加的三件(滚动条 / 停留复制 / 背景图)——都是**可选**的
//!
//! 三样都有默认值，`TerminalView::new` 之后什么都不接也能跑：
//! 滚动条默认**已开**（视觉照抄 `styles.css` 的 `::-webkit-scrollbar`），
//! 停留复制默认**关**（维持「松手即复制」），背景图默认**无**。
//!
//! ## 5. 滚动条(可选)
//!
//! ```ignore
//! // 默认就够用；只有要改宽度/淡出时长时才需要这行
//! .scrollbar(mt_ui::ScrollbarStyle {
//!     width: px(6.0),
//!     fade_delay: std::time::Duration::from_millis(900),
//!     ..Default::default()
//! })
//! ```
//!
//! 关掉：`ScrollbarStyle { enabled: false, ..Default::default() }`。
//!
//! ## 6. 拖选停留自动复制 +「已复制」气泡(可选)
//!
//! 原版是「按住左键、鼠标停住 `selectionAutoCopySecs` 秒」才复制并弹气泡，
//! 不是松手就复制。接上它需要**两步**：
//!
//! ```ignore
//! let this_for_tip = cx.weak_entity();
//! TerminalView::new(…)
//!     // ① 停留时长从 config 取（0 或缺省 = 关闭 = 维持现有的松手即复制）
//!     .selection_dwell(mt_ui::DwellConfig::from_secs(
//!         config.selection_auto_copy_secs.unwrap_or(1.0),
//!     ))
//!     // ② 复制发生时弹气泡；origin 是**元素相对**坐标，已按容器宽度贴边收拢
//!     .on_selection_copied(move |_text, origin, _window, cx| {
//!         let _ = this_for_tip.update(cx, |pane: &mut TerminalPane, cx| {
//!             pane.copied_tip = Some(origin);
//!             cx.notify();
//!             // 1s 后自己撤掉（原版 tipTimer 就是这么做的）
//!             cx.spawn(async move |pane, cx| {
//!                 cx.background_executor().timer(Duration::from_secs(1)).await;
//!                 let _ = pane.update(cx, |p, cx| { p.copied_tip = None; cx.notify(); });
//!             })
//!             .detach();
//!         });
//!     })
//! ```
//!
//! 气泡本体用 [`mt_ui::CopiedTip`](crate::CopiedTip)，叠在终端之上：
//!
//! ```ignore
//! div().size_full().relative()
//!     .child(self.view.clone())
//!     .when_some(self.copied_tip, |this, origin| {
//!         this.child(
//!             div().absolute().left(origin.x).top(origin.y)
//!                 .child(mt_ui::CopiedTip::new(t("terminal.copied"))),
//!         )
//!     })
//! ```
//!
//! ## 7. 背景图(可选，且**与窗口级二选一**)
//!
//! ```ignore
//! self.view.update(cx, |v, cx| v.set_background_art(store.background_art().cloned(), cx));
//! ```
//!
//! `AppStore::background_art()` 就是 `AppliedThemePack::background`。
//! **更推荐窗口级铺**（原版就是挂在 `#root` 上，三栏都透着同一张图）——
//! 见 [`crate::background`] 模块注释里的接法与 overdraw 提醒。

use std::cell::{Cell as StdCell, RefCell};
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
use super::element::{
    FlashLine, FrameGeometry, OnGridResize, OnInput, PreeditText, TerminalElement,
};
use super::ime::{ImeState, commit_to_bytes};
use super::input::{is_text_input_key, keystroke_to_bytes, paste_to_bytes};
use super::scrollbar::ScrollbarStyle;
use super::search::TerminalSearch;
use super::selection_dwell::{DwellConfig, OnSelectionCopied};
use super::theme::{SearchColors, TerminalStyle, TerminalTheme};
use crate::theme_bridge::BackgroundArt;

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
    scrollbar: ScrollbarStyle,
    dwell: DwellConfig,
    on_selection_copied: Option<OnSelectionCopied>,
    background_art: Option<BackgroundArt>,
    search: Option<Rc<RefCell<TerminalSearch>>>,
    search_colors: SearchColors,
    /// 一次性的整行闪烁(跳到 AI 任务标记之后的可见反馈)。见 [`FlashLine`]。
    /// **到期撤销由宿主负责** —— 视图不起计时器,免得它替宿主管生命周期。
    flash: Option<FlashLine>,
    /// 宿主接管粘贴。见 [`TerminalView::on_paste`]。
    on_paste: Option<OnPaste>,
    /// 「智能 Ctrl+C / Ctrl+V」现在开着吗。见 [`TerminalView::smart_copy_paste`]。
    smart_copy_paste: Option<SmartCopyPaste>,
}

/// 宿主对一次粘贴的裁决。
///
/// ⚠️ **必须是返回值,不能让宿主自己去写**:钩子是在 `TerminalView` 正被可变
/// 借用的时候调的(按键与右键菜单两条路都是),宿主若在钩子里回头
/// `view.update(...)` 就是同一实体的嵌套 update —— gpui 当场 panic。
/// 让宿主把内容**交回来**由视图写,这条路天然没有再入。
pub enum PasteAction {
    /// 什么都别写(宿主已处理完,或已失败并提示过)。
    None,
    /// 按 bracketed paste 粘这段文本。
    Text(String),
    /// **原样**写入(不走 bracketed paste)—— 长文本转存后的那条路径。
    Raw(String),
}

/// 宿主接管的粘贴动作(长文本转文件、远程上传之类)。
///
/// 视图**不读剪贴板**,全权交给宿主 —— 阈值判定要读 `AppConfig`,那是壳的
/// 东西,mt-ui 不该知道。没设就走内建的 [`TerminalView::paste`]。
pub type OnPaste = Rc<dyn Fn(&mut Window, &mut App) -> PasteAction>;

/// 「智能 Ctrl+C / Ctrl+V」的开关判据,由宿主每次按键**现问**。
///
/// 做成闭包而不是 `bool` 字段是为了免掉一整条配置下发链路:设置页一改
/// `config.smartCopyPaste`,下一次按键就是新值,不需要挨个终端推。
pub type SmartCopyPaste = Rc<dyn Fn(&App) -> bool>;

/// 智能 Ctrl+C/V 这一下该做什么(`terminalCache.ts:292-309` 的判定链)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SmartAction {
    /// 有选区的 Ctrl+C:复制并**清掉选区**,吞掉按键。
    CopySelection,
    /// Ctrl+V:粘贴,吞掉按键。
    Paste,
    /// 不接管 —— 照常翻成字节送进 PTY(无选区的 Ctrl+C 于是发 SIGINT)。
    PassThrough,
}

/// 智能 Ctrl+C/V 的纯判定。
///
/// 逐条照抄原版 `attachCustomKeyEventHandler` 的第三段:
/// `mod = ctrlKey || metaKey`,且**不带 Shift / Alt**;`KeyC` 有选区才接管
/// (没选区 `return true` = 透传 SIGINT,这是这个功能的全部意义),`KeyV` 一律接管。
///
/// # macOS 上 ⌘ 不受 `enabled` 管
///
/// 这个开关存在的理由是 Windows / Linux 上 ^C 必须发 SIGINT、^V 是 literal-next,
/// 拿它们当复制粘贴要用户自己权衡。**macOS 没有这个冲突** —— ⌘ 根本不是终端修饰键,
/// 中断走的是物理 Control+C,两个键位互不相干。Terminal.app / iTerm2 / Ghostty /
/// WezTerm 一律 ⌘C / ⌘V 无条件生效,这里对齐。
///
/// 只放行 `platform`,不动 `control`:Windows 上 `platform` 是 Win 键(Win+V 是系统
/// 剪贴板历史),那边维持原样由开关管。
pub(crate) fn smart_key_action(
    enabled: bool,
    mods: &gpui::Modifiers,
    key: &str,
    has_selection: bool,
) -> SmartAction {
    let mac_cmd = cfg!(target_os = "macos") && mods.platform;
    if (!enabled && !mac_cmd) || !(mods.control || mods.platform) || mods.shift || mods.alt {
        return SmartAction::PassThrough;
    }
    match key {
        "c" if has_selection => SmartAction::CopySelection,
        "v" => SmartAction::Paste,
        _ => SmartAction::PassThrough,
    }
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
            scrollbar: ScrollbarStyle::default(),
            dwell: DwellConfig::default(),
            on_selection_copied: None,
            background_art: None,
            search: None,
            search_colors: SearchColors::default(),
            flash: None,
            on_paste: None,
            smart_copy_paste: None,
        }
    }

    /// 宿主接管粘贴。设了之后 Ctrl+Shift+V、智能 Ctrl+V 与
    /// [`request_paste`](Self::request_paste) 全部改走它,视图侧不再读剪贴板。
    ///
    /// 宿主返回 [`PasteAction`] 说明该写什么 —— **不要**在钩子里回头动这个视图,
    /// 原因见 [`PasteAction`] 的注释。
    pub fn on_paste(mut self, f: impl Fn(&mut Window, &mut App) -> PasteAction + 'static) -> Self {
        self.on_paste = Some(Rc::new(f));
        self
    }

    /// 接上「智能 Ctrl+C / Ctrl+V」的开关判据(`config.smartCopyPaste`)。
    ///
    /// 不设 = 永远关着,Ctrl+C/Ctrl+V 照常当控制字符送进 PTY。
    pub fn smart_copy_paste(mut self, f: impl Fn(&App) -> bool + 'static) -> Self {
        self.smart_copy_paste = Some(Rc::new(f));
        self
    }


    /// 接上终端内查找(Ctrl+F)。引擎实例与
    /// [`TerminalSearchBar`](super::TerminalSearchBar) **共用同一个**
    /// `Rc<RefCell<_>>` —— 计数与高亮从此是同一份状态。
    ///
    /// 接上之后视图每帧替引擎跑一次去抖后的重搜,命中格子自动画上底色;
    /// 行渲染缓存不受影响(命中状态进的是行签名,不是帧指纹)。
    pub fn search(mut self, search: Rc<RefCell<TerminalSearch>>) -> Self {
        self.search = Some(search);
        self
    }

    /// 运行时挂上 / 摘掉查找引擎。
    pub fn set_search(
        &mut self,
        search: Option<Rc<RefCell<TerminalSearch>>>,
        cx: &mut Context<Self>,
    ) {
        self.search = search;
        cx.notify();
    }

    /// 查找命中的高亮配色。见 [`SearchColors`](super::SearchColors)。
    pub fn search_colors(mut self, colors: SearchColors) -> Self {
        self.search_colors = colors;
        self
    }

    /// 运行时换查找高亮配色。
    pub fn set_search_colors(&mut self, colors: SearchColors, cx: &mut Context<Self>) {
        if self.search_colors != colors {
            self.search_colors = colors;
            cx.notify();
        }
    }

    /// 让某一行整行闪一下(`None` = 撤掉)。见 [`FlashLine`]。
    ///
    /// 值没变就不 `notify`:跳到同一条 marker 两次不该白重画一帧。
    pub fn set_flash(&mut self, flash: Option<FlashLine>, cx: &mut Context<Self>) {
        if self.flash != flash {
            self.flash = flash;
            cx.notify();
        }
    }

    /// 滚动条外观(默认已开,见 [`ScrollbarStyle`])。
    pub fn scrollbar(mut self, style: ScrollbarStyle) -> Self {
        self.scrollbar = style;
        self
    }

    /// 运行时换滚动条配置(设置页改宽度/淡出时长)。
    pub fn set_scrollbar(&mut self, style: ScrollbarStyle, cx: &mut Context<Self>) {
        if self.scrollbar != style {
            self.scrollbar = style;
            cx.notify();
        }
    }

    /// 拖选停留自动复制。**不设 = 维持旧的「松手即复制」**,见 [`DwellConfig`]。
    pub fn selection_dwell(mut self, dwell: DwellConfig) -> Self {
        self.dwell = dwell;
        self
    }

    /// 运行时改停留时长(`config.selectionAutoCopySecs` 变了就调这个)。
    pub fn set_selection_dwell(&mut self, dwell: DwellConfig, cx: &mut Context<Self>) {
        if self.dwell != dwell {
            self.dwell = dwell;
            cx.notify();
        }
    }

    /// 复制发生时回调宿主(弹「已复制」气泡)。见 [`OnSelectionCopied`]。
    pub fn on_selection_copied(
        mut self,
        f: impl Fn(&str, Point<Pixels>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_selection_copied = Some(std::rc::Rc::new(f));
        self
    }

    /// 只在**这个终端**底下铺主题包背景图。
    ///
    /// 原版是窗口级(`#root`),推荐仍走窗口级 —— 详见 [`crate::background`]
    /// 模块注释里的 overdraw 提醒,两者别同时开。
    pub fn background_art(mut self, art: Option<BackgroundArt>) -> Self {
        self.background_art = art;
        self
    }

    /// 换主题包时更新背景图(`AppStore::background_art()` 的产物)。
    pub fn set_background_art(&mut self, art: Option<BackgroundArt>, cx: &mut Context<Self>) {
        self.background_art = art;
        cx.notify();
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

    /// 当前有没有可复制的选区(空串不算 —— 选中一段空白后 Ctrl+C 该照发 SIGINT)。
    ///
    /// ⚠️ 与 xterm 的 `hasSelection()` 有一档差:那边只看「有没有选择范围」,
    /// 全是空白的选区它也算有。这里取「选出来的文本非空」,与右键菜单里
    /// 「复制」置灰的判据同源。
    pub fn has_selection(&self) -> bool {
        self.emulator
            .with_term(|t| t.selection_to_string())
            .is_some_and(|text| !text.is_empty())
    }

    /// 清掉选区(智能 Ctrl+C 复制完就撤选,照抄 `term.clearSelection()`)。
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.emulator.with_term_mut(|term| term.selection = None);
        cx.notify();
    }

    /// 粘贴剪贴板内容(按 bracketed paste 模式编码)。
    ///
    /// **宿主接管时不要调这个** —— 走 [`request_paste`](Self::request_paste)。
    pub fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|it| it.text()) else {
            return;
        };
        self.paste_text(&text, window, cx);
    }

    /// 走一次粘贴:宿主接管了就交给它,否则走内建的 [`paste`](Self::paste)。
    ///
    /// **所有粘贴入口都该走这里**(Ctrl+Shift+V、智能 Ctrl+V、右键菜单),
    /// 否则长文本转文件这类宿主逻辑会被某一条入口绕过去。
    pub fn request_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cb) = self.on_paste.clone() else {
            self.paste(window, cx);
            return;
        };
        match cb(window, cx) {
            PasteAction::None => {}
            PasteAction::Text(text) => self.paste_text(&text, window, cx),
            PasteAction::Raw(text) => self.insert_text(&text, window, cx),
        }
    }

    /// 把一段文本当成粘贴内容送进 PTY(bracketed paste 编码)。
    pub fn paste_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let bytes = paste_to_bytes(text, self.emulator.mode());
        self.scroll_to_bottom();
        self.write(&bytes, window, cx);
        cx.notify();
    }

    /// 原样写入一段文本,**不走 bracketed paste**。
    ///
    /// 长文本转存后粘的那条 `"C:\...\paste-*.txt"` 走这一条 —— 原版同样是
    /// `enqueuePtyWrite` 裸写(`terminalCache.ts:757`),而不是 `term.paste()`。
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_to_bottom();
        self.write(text.as_bytes(), window, cx);
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

        // 应用层快捷键:Ctrl+Shift+C / Ctrl+Shift+V(macOS 上是 ⌘⇧C / ⌘⇧V)。
        //
        // ⚠️ 必须一并认 `platform`:macOS 把 ⌘ 报在 `platform` 位而不是 `control`,
        // 只判 `control` 的话这个分支在 mac 上永远进不去 —— 而设置页恰恰把这条
        // 显示成 ⌘⇧V(`hotkeys.rs::combo_label` 在 mac 上渲染 ⌘),显示与实际对不上。
        // 判据与 [`smart_key_action`] 保持同一口径(那边本来就是 `control || platform`)。
        //
        // **副作用如实记:Windows 上 Win+Shift+C / Win+Shift+V 也会被这里接管**
        // —— gpui 的 Windows 后端把 Win 键报在 `platform` 位(`events.rs`),上游评审
        // 真机实测到了(PR #59)。刻意不用 `cfg!(target_os = "macos")` 闸住:
        // ① `smart_key_action` 那条路**本来**就把 `platform` 与 `control` 同权,
        //    这里闸住反而让两条路的判据分叉;② Win+Shift+V 没有系统绑定,多一条
        //    粘贴入口无害。要严格守住「Windows 逐字不变」就在这里加 cfg 闸门。
        if (mods.control || mods.platform) && mods.shift {
            match keystroke.key.as_str() {
                "c" => {
                    self.copy_selection(cx);
                    cx.stop_propagation();
                }
                "v" => {
                    self.request_paste(window, cx);
                    cx.stop_propagation();
                }
                // 其余 Ctrl+Shift 组合留给宿主(新建标签 / 切 pane…),继续冒泡
                _ => {}
            }
            return;
        }

        // 智能 Ctrl+C / Ctrl+V(`config.smartCopyPaste`)。判定链见
        // [`smart_key_action`];**排在可打印字符放行之前**,否则带 Ctrl 的 c/v
        // 会先被当成文本键放走。无选区的 Ctrl+C 一路落到下面照发 SIGINT。
        //
        // 先用最便宜的判据筛一道:`has_selection()` 要锁住 term 再把整段选区拼成
        // String,放在这里等于**每敲一个字都扫一遍选区**。真正的判定仍然只有
        // [`smart_key_action`] 一处(单测覆盖的也是它),这一步纯粹是闸门。
        let smart_key = (mods.control || mods.platform)
            && !mods.shift
            && !mods.alt
            && matches!(keystroke.key.as_str(), "c" | "v");
        let action = if smart_key {
            let on = self.smart_copy_paste.clone().is_some_and(|probe| probe(cx));
            smart_key_action(on, mods, keystroke.key.as_str(), self.has_selection())
        } else {
            SmartAction::PassThrough
        };
        match action {
            SmartAction::CopySelection => {
                self.copy_selection(cx);
                self.clear_selection(cx);
                cx.stop_propagation();
                return;
            }
            SmartAction::Paste => {
                self.request_paste(window, cx);
                cx.stop_propagation();
                return;
            }
            SmartAction::PassThrough => {}
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
        .scrollbar(self.scrollbar.clone())
        .search(self.search.clone())
        .search_colors(self.search_colors)
        .flash(self.flash)
        .selection_dwell(self.dwell)
        .background_art(self.background_art.clone())
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
        if let Some(cb) = self.on_selection_copied.clone() {
            element = element
                .on_selection_copied(move |text, origin, window, cx| cb(text, origin, window, cx));
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
        // 组合中:贴着预编辑串里的插入符;否则贴着终端光标格 —— 后者与光标
        // **可见性无关**,TUI 发 `ESC[?25l` 藏了光标也照样有(见 `FrameGeometry::cursor`)。
        // 两个都没有只可能是还没画过一帧,退回元素左上角 —— 总比不弹候选框强。
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

#[cfg(test)]
mod tests {
    use super::{SmartAction, smart_key_action};
    use gpui::Modifiers;

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    /// 关着的时候一律不接管 —— Ctrl+C 照发 SIGINT、Ctrl+V 照走 xterm 的老路。
    #[test]
    fn 开关关着时智能键位不接管() {
        assert_eq!(
            smart_key_action(false, &ctrl(), "c", true),
            SmartAction::PassThrough
        );
        assert_eq!(
            smart_key_action(false, &ctrl(), "v", false),
            SmartAction::PassThrough
        );
    }

    /// 这个功能的**全部意义**:有选区才复制,没选区必须原样透传成 SIGINT。
    #[test]
    fn ctrl_c_按选区分叉() {
        assert_eq!(
            smart_key_action(true, &ctrl(), "c", true),
            SmartAction::CopySelection
        );
        assert_eq!(
            smart_key_action(true, &ctrl(), "c", false),
            SmartAction::PassThrough,
            "没选区的 Ctrl+C 必须能中断程序"
        );
    }

    /// Ctrl+V 一律接管(有没有选区都一样)。
    #[test]
    fn ctrl_v_一律接管粘贴() {
        assert_eq!(
            smart_key_action(true, &ctrl(), "v", false),
            SmartAction::Paste
        );
        assert_eq!(
            smart_key_action(true, &ctrl(), "v", true),
            SmartAction::Paste
        );
    }

    /// mac 的 ⌘ 同样算 mod(原版 `e.ctrlKey || e.metaKey`)。
    #[test]
    fn platform_键与_ctrl_同权() {
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(smart_key_action(true, &cmd, "v", false), SmartAction::Paste);
    }

    /// macOS 上 ⌘C / ⌘V **不受开关管**:那个开关是为 Windows / Linux 的 ^C/^V 冲突
    /// 设的,而 mac 上 ⌘ 根本不是终端修饰键、中断走物理 Control+C,没有冲突可权衡。
    /// 曾经开关默认关着 + Ctrl+Shift 分支漏判 `platform`,导致 mac 上 ⌘V 与 ⌘⇧V
    /// **双双失效**、只剩右键菜单能粘贴。这条钉住不要退回去。
    ///
    /// `control` 那一路不放行:Windows 的 `platform` 是 Win 键(Win+V 是系统剪贴板
    /// 历史),行为仍由开关决定 —— 所以断言按平台分叉。
    #[test]
    fn mac_的_cmd_不受智能开关管() {
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        let expected = if cfg!(target_os = "macos") {
            SmartAction::Paste
        } else {
            SmartAction::PassThrough
        };
        assert_eq!(smart_key_action(false, &cmd, "v", false), expected);

        // 关着的 Ctrl 三家一致:照发 SIGINT / 走老路
        assert_eq!(
            smart_key_action(false, &ctrl(), "v", false),
            SmartAction::PassThrough
        );
    }

    /// 带 Shift / Alt 的组合不归智能键位管:Ctrl+Shift+C/V 是另一条**始终生效**
    /// 的路(在 `on_key_down` 里更早就返回了),Alt+Ctrl+V 该原样进 PTY。
    #[test]
    fn 带_shift_或_alt_的组合不接管() {
        let ctrl_shift = Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        let ctrl_alt = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            smart_key_action(true, &ctrl_shift, "v", false),
            SmartAction::PassThrough
        );
        assert_eq!(
            smart_key_action(true, &ctrl_alt, "c", true),
            SmartAction::PassThrough
        );
    }

    /// 没有修饰键 = 普通打字,一个字都不许吞。
    #[test]
    fn 裸键不接管() {
        let none = Modifiers::default();
        assert_eq!(
            smart_key_action(true, &none, "c", true),
            SmartAction::PassThrough
        );
        assert_eq!(
            smart_key_action(true, &none, "v", false),
            SmartAction::PassThrough
        );
    }

    /// c/v 之外的键一概不管(Ctrl+D 之类必须原样进 PTY)。
    #[test]
    fn 其余键一概透传() {
        assert_eq!(
            smart_key_action(true, &ctrl(), "d", true),
            SmartAction::PassThrough
        );
    }
}
