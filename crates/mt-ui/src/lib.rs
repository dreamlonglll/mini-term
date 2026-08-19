//! GPUI 渲染层:终端 view/element、主题桥。不含业务逻辑。
//!
//! # 本 crate 产出什么
//!
//! ## 1. 终端渲染(`terminal` 模块)—— 整个改造的核心
//!
//! 把 [`mt_terminal::TerminalEmulator`] 的 grid 画成 GPUI 元素,替掉 xterm.js。
//! 自己实现的:字形绘制与列宽、光标、选择高亮、滚动回看、鼠标上报、IME 预编辑、
//! 行级 damage 追踪。
//!
//! 入口是 [`TerminalView`](terminal::TerminalView)(Entity,带焦点/键盘/IME);
//! [`TerminalElement`] 是它内部用的裸元素,一般不直接用。
//!
//! 背景图透出的关键:**背景色等于默认背景的 cell 不发 quad**。
//!
//! ## 2. 主题桥([`theme_bridge`])
//!
//! `themes/<id>/theme.json`(由 `mt_config::ThemePacks` 读出原文)→
//! 终端配色 [`TerminalTheme`] + `gpui_component` 的 `ThemeConfig`,
//! 外加「按主题包 id 切换」的运行时入口。
//!
//! ## 3. 图标体系([`icons`])
//!
//! 厂商图标 / 技术栈徽标 / 文件树图标 / 四态状态灯,全部**自绘矢量**
//! (无资产、无宿主接线、可多色、几何可单测)。替掉 mt-app 现有的
//! 「CL/CX/GK 两字母文本」与「三形圆点」两处占位 —— 接线总表见该模块注释。
//!
//! ## 3.5 终端内查找(`terminal::search` + `terminal::search_bar`)
//!
//! 替掉 xterm.js 的 `@xterm/addon-search`:引擎建在 alacritty 的
//! `RegexIter` 上,一次枚举完全 buffer 的命中,计数 / 跳转 / 高亮三件事共用一份
//! 结果集;查找条是普通的 `absolute` 子元素,旧版那条「rAF 每帧量 pane 矩形」
//! 的定位轮询整个删掉。宿主接线见 [`terminal::search_bar`] 的模块注释。
//!
//! ## 4. 背景图([`background`])
//!
//! 主题包背景图的 cover/focus 铺放 + 压暗纱罩。原版挂在 `#root`(窗口级),
//! 这里既可以窗口级铺,也可以只铺终端区([`TerminalView::set_background_art`])。
//!
//! ## 5. 布局复用件(尽量用 gpui-component,别自己造)
//!
//! | mini-term 现状 | GPUI 侧对应 |
//! |---|---|
//! | Allotment 三栏主布局 | `gpui_component::resizable` |
//! | 递归 SplitNode 树(分屏) | 同上,嵌套使用;树结构本身是业务,留在 `mt-app` |
//! | FileTree | `gpui_component::tree` |
//! | Tab 栏 | `gpui_component::tab` |
//! | 各种 Modal | `gpui_component::dialog` |
//!
//! # 进度
//!
//! - ✅ 逐 cell 绘制、ANSI/256/truecolor、bold/italic/underline/inverse、
//!   四态光标、滚轮回看、鼠标选择 + 剪贴板
//! - ✅ IME 预编辑([`TerminalView`](terminal::TerminalView) 实现 `EntityInputHandler`)
//! - ✅ 鼠标上报(1000/1002/1003 × X10/1005/1006)
//! - ✅ 行级 damage 追踪([`terminal::damage`])
//! - ✅ 主题桥([`theme_bridge`])+ 背景图渲染([`background`])
//! - ✅ 滚动条([`terminal::scrollbar`]):拖滑块 / 点轨道翻页 / 闲置淡出 / alt screen 不画
//! - ✅ 拖选停留自动复制([`terminal::selection_dwell`]),默认关闭以兼容旧语义
//! - ✅ 终端内查找([`terminal::search`] 引擎 + [`terminal::search_bar`] 浮动查找条):
//!   字面/大小写/正则三模式 × 整词、全 buffer 计数、环形上下一个、两档命中高亮
//! - ✅ 图标体系([`icons`])
//! - ⬜ 下划线花样(DOUBLE/DOTTED/DASHED 统一降级实线,gpui 只有 wavy 一种)
//! - ⬜ 回退字形溢出裁剪(宽字符回退到非等宽字体时可能糊出格子边界)

pub mod background;
pub mod icons;
pub mod terminal;
pub mod theme_bridge;

pub use background::{BackgroundArtElement, Fit, background_art, fit_bounds};
pub use icons::{
    AiVendor, BrandIcon, FileIcon, FileKind, ProjectKind, StatusDot, StatusKind, TechIcon,
};
pub use terminal::{
    CopiedTip, DamageStats, DwellConfig, FlashLine, FrameGeometry, HighlightKind, HighlightSpan,
    InstallInputHandler, MINI_REFRESH_MS, MiniTerminalElement, OnGridResize, OnInput, OnPaste,
    OnSearchClose, OnSelectionCopied,
    PasteAction, PreeditText, ScrollbarStyle, SearchBarEvent, SearchBarLabels, SearchColors,
    SearchDirection,
    SearchHighlights, SearchLimits, SearchMatch, SearchOptions, TerminalElement, TerminalSearch,
    TerminalSearchBar, TerminalStyle, TerminalTheme, TerminalView, color_request_rgb,
    is_text_input_key, keystroke_to_bytes, paste_to_bytes, rgb8,
};
pub use theme_bridge::{
    AppliedThemePack, Appearance, BackgroundArt, ThemePackColors, ThemePackDef, switch_to_builtin,
    switch_to_theme_pack,
};

/// OSC 调色板查询的应答色(`TermEvent::ColorRequest` 的处理)。
///
/// 宿主的 `drain_term_events` 里那句
/// `TermEvent::ColorRequest(index, format) => write(format(rgb))`,
/// `rgb` 就从这里取。两件事自己写容易漏:
///
/// 1. index **不是** 0..16,256/257/258 是前景/背景/光标;按 `theme.ansi.get(index)`
///    取、越界回前景,会让「查背景色」答成前景色 —— 程序算出对比度 1.0,
///    多半会把终端判成纯黑并切一套错误的配色;
/// 2. OSC 4 改过的调色板要优先于主题(程序自己刚设过的色,查回去必须是那个),
///    所以这里要读 **live 的** `term.colors()` 而不是一张空表。
///
/// 详见 [`terminal::colors::color_request_rgb`]。
pub fn terminal_color_rgb(
    emulator: &mt_terminal::TerminalEmulator,
    theme: &TerminalTheme,
    index: usize,
) -> mt_terminal::alacritty_terminal::vte::ansi::Rgb {
    emulator.with_term(|term| color_request_rgb(index, term.colors(), theme))
}
