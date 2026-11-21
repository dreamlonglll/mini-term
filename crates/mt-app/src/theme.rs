//! 主题装配:`config.theme` / `config.customThemeId` → gpui-component 主题层 +
//! 壳配色([`crate::ui::Palette`])+ 终端配色([`TerminalTheme`])。
//!
//! # 三条链路,一个入口
//!
//! ```text
//!                       ┌─→ gpui_component::Theme  (Dialog / Input / 通知等组件)
//! config.theme          │
//! config.customThemeId ─┼─→ ui::Palette            (壳自绘的三栏 / tab / 面板)
//! config.terminalFollow │
//!                       └─→ mt_ui::TerminalTheme   (每个 TerminalPane)
//! ```
//!
//! 全部由 [`apply`] 一次算出,调用方负责把终端配色下发给已存在的终端
//! (`AppStore::apply_theme`,对应旧版 `terminalCache.ts::updateAllTerminalThemes`)。
//!
//! # 语义对照(逐条抄 `src/App.tsx` 与 `src/utils/themeManager.ts`)
//!
//! - `theme` 三态 `light` / `dark` / `auto`;`auto` 跟随系统(旧版是
//!   `matchMedia('(prefers-color-scheme: light)')`,这里是
//!   `App::window_appearance()`);非法值按 `auto` 处理,与前端 `?? 'auto'` 一致;
//! - **皮肤的明暗由作者在 `theme.json` 的 `appearance` 定死**,不跟随系统;
//!   激活外置主题包时 `config.theme` 保持不动,退出皮肤可无损回落;
//! - 外置主题包加载失败 → 回落内置外观,并**只清内存里的 `customThemeId`
//!   不落盘**:主题目录可能只是这次读不到(盘没挂载、文件正被替换),
//!   落盘会把用户的选择永久抹掉;
//! - `terminalFollowTheme` 关掉时终端固定用**内置暗色**配色
//!   (旧版 `getTerminalTheme` 的 `if (!terminalFollowTheme) return DARK_TERMINAL_THEME`),
//!   壳配色不受影响。
//!
//! # 皮肤只有两档
//!
//! 原版的内置皮肤(`skin` = blueprint / fluent2)GPUI 侧从来没有对应色表,
//! 一律按 `none` 渲染;设置里那一栏与 `AppConfig::skin` 字段都已删除。现在
//! 只有**默认皮肤**(`config.theme` 的 dark/light/auto)与**外置皮肤**
//! (`custom_theme_id` 指向的主题包)两条路。背景图([`BackgroundArt`])在这里只
//! 算出参数,**渲染由 `main.rs` 的根容器窗口级铺**(与原版挂 `#root` 同位置)。

use gpui::{App, Window, WindowAppearance};
use mt_config::AppConfig;
use mt_ui::TerminalTheme;
use mt_ui::theme_bridge::{
    self, Appearance, BackgroundArt, ThemePackListing, ThemeTokens, builtin_dark_terminal_theme,
    builtin_terminal_theme, switch_to_builtin, switch_to_theme_pack,
};

use crate::ui::Palette;

/// 一次主题装配的产物。
pub struct AppliedTheme {
    /// 最终生效的明暗态(皮肤激活时来自皮肤,否则来自 `config.theme`)。
    /// 设置面板要显示「当前是亮还是暗」,先带出来。
    #[allow(dead_code)]
    pub appearance: Appearance,
    /// 壳配色。
    pub palette: Palette,
    /// 终端配色(已经过 `terminalFollowTheme` 这一闸)。
    pub terminal: TerminalTheme,
    /// 有背景图时的氛围层参数。落进 `AppStore::background_art`,由 `Workspace`
    /// 的根容器窗口级铺(`mt_ui::background_art`)。
    pub background: Option<BackgroundArt>,
    /// 外置主题包加载失败时带回那个 id —— 调用方据此清掉内存里的
    /// `config.custom_theme_id`(不落盘)。
    pub failed_pack: Option<String>,
}

/// `config.theme` → 明暗态。`auto` 与非法值都跟随系统。
pub fn resolve_appearance(theme: &str, cx: &App) -> Appearance {
    match theme {
        "light" => Appearance::Light,
        "dark" => Appearance::Dark,
        // 与前端 `applyTheme(cfg.theme ?? 'auto')` 同口径:认不出的值按 auto
        _ => match cx.window_appearance() {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
        },
    }
}

/// themes/ 目录。
///
/// 目录取 [`mt_config::themes_dir`],认 `MT_APP_DATA_DIR`(`mt_config::active_data_dir`,
/// 与 [`crate::app_data_dir`] 同一口径),J 批那条「钉死装机版目录、mt-app 用
/// `ThemePacks::at()` 绕开」的记档已结清 —— 这里只保留定位不到数据目录时的兜底。
///
/// 主题包文件层拆进 `mt-theme-packs` 后不再认数据目录口径(原 `ThemePacks::open()`
/// 就是 `at(themes_dir()?)`),目录由这里算好交给 `ThemePacks::at`,语义不变。
pub fn theme_packs() -> mt_config::ThemePacks {
    let root = mt_config::themes_dir().unwrap_or_else(|_| crate::app_data_dir().join("themes"));
    mt_config::ThemePacks::at(root)
}

/// 可用的外置主题包(坏包跳过,设置页的皮肤列表用它)。
pub fn list_packs() -> Vec<ThemePackListing> {
    theme_bridge::list_theme_packs(&theme_packs()).unwrap_or_else(|err| {
        tracing::warn!("[theme] 主题目录读取失败: {err:#}");
        Vec::new()
    })
}

/// 按配置装配主题。**这是唯一的装配入口**(启动、切亮暗、切皮肤都走它)。
///
/// 两条分支各自只有一个 mt-ui 调用:
///
/// - 有皮肤 → [`switch_to_theme_pack`](mt_ui::theme_bridge::switch_to_theme_pack)
///   (读包 → 校验 → 装进 gpui-component 主题层,一步到位);
/// - 无皮肤 / 皮肤读不出来 → [`switch_to_builtin`](mt_ui::theme_bridge::switch_to_builtin)
///   (**内含**把 `Theme::dark_theme`/`light_theme` 从 `ThemeRegistry` 恢复回内置基线
///   这一步 —— 少了它「退出皮肤」只切 mode,浮层会原地停在皮肤配色上;恢复完再叠
///   [`builtin_tokens`] 这份内置调色板的映射,上游组件才与自绘面板同色)。
pub fn apply(config: &AppConfig, window: Option<&mut Window>, cx: &mut App) -> AppliedTheme {
    let applied = apply_inner(config, window, cx);
    // 代码高亮配色跟着壳配色走(见 [`install_highlight_theme`])。放在这里而不是
    // 三个 return 分支里各写一遍 —— 装配入口只有一个,高亮表也只在这一处装。
    install_highlight_theme(&applied.palette, applied.appearance, cx);
    // 弹窗遮罩:两条路的 token 映射给的都是「背景色 55% alpha」,而原版所有 Modal
    // 统一压 `bg-black/50`(`Modal.tsx:171`,亮暗两套同值)—— 亮色下尤其差得远
    // (白底 55% 根本压不住底下的内容)。Dialog/Sheet 渲染时读的是
    // `cx.theme().overlay`,必须放在 apply_inner **之后** ——
    // switch_to_builtin/switch_to_theme_pack 都会从基线重置整套 colors。
    //
    // 这里曾经还补一条 `theme.colors.link`:内置主题那条路当时把全局主题恢复成
    // 组件库出厂值,link 是它自己的蓝。现在内置也走 [`builtin_tokens`] → token
    // 映射(`put("link", accent)`),与皮肤路径同源,补丁已无事可做,删掉。
    // md 行内 code 的字与底由 `file_viewer` / `session_panel` 两处
    // `preview_text_style` 经 `TextViewStyle.inline_code` 直接给(0.6.2 起有此钩子)。
    {
        let theme = gpui_component::Theme::global_mut(cx);
        theme.colors.overlay = gpui::hsla(0.0, 0.0, 0.0, 0.5);
    }
    applied
}

fn apply_inner(config: &AppConfig, mut window: Option<&mut Window>, cx: &mut App) -> AppliedTheme {
    let follow = config.terminal_follow_theme;

    if let Some(theme_id) = config.custom_theme_id.as_deref() {
        // 失败时还要用 window 走回落分支,所以这里传一份重借
        match switch_to_theme_pack(&theme_packs(), theme_id, window.as_deref_mut(), cx) {
            Ok(applied) => {
                return AppliedTheme {
                    appearance: applied.appearance,
                    palette: Palette::from_pack(&applied),
                    // 终端不跟随主题时用内置暗色 —— 与旧版一字不差
                    terminal: if follow {
                        applied.terminal.clone()
                    } else {
                        builtin_dark_terminal_theme()
                    },
                    background: applied.background.clone(),
                    failed_pack: None,
                };
            }
            Err(err) => {
                tracing::warn!("[theme] 自定义主题 {theme_id} 加载失败,回落内置外观: {err:#}");
                let appearance = resolve_appearance(&config.theme, cx);
                let palette = builtin_palette(appearance);
                // 返回值就是该明暗的内置终端配色;`terminalFollowTheme` 那道闸
                // 在 builtin_terminal 里(关掉时固定暗色),所以这里不直接用它
                let _ = switch_to_builtin(appearance, &builtin_tokens(&palette), window, cx);
                return AppliedTheme {
                    appearance,
                    palette,
                    terminal: builtin_terminal(appearance, follow),
                    background: None,
                    failed_pack: Some(theme_id.to_string()),
                };
            }
        }
    }

    let appearance = resolve_appearance(&config.theme, cx);
    let palette = builtin_palette(appearance);
    let _ = switch_to_builtin(appearance, &builtin_tokens(&palette), window, cx);
    AppliedTheme {
        appearance,
        palette,
        terminal: builtin_terminal(appearance, follow),
        background: None,
        failed_pack: None,
    }
}

/// 内置调色板 → gpui-component 的十个语义色。
///
/// **内置配色的唯一来源仍是 [`Palette::dark`] / [`Palette::light`]**（`ui.rs` 里
/// 逐值抄 `styles.css` 的那份表）—— 这里一个色值都不新造，只做「壳语义 → 主题包
/// 语义」的改名。改名表与 [`Palette::from_pack`] 严格互逆：
///
/// | 壳（`ui::Palette`） | 主题包语义 | 组件库那头拿去画什么 |
/// |---|---|---|
/// | `bg_base` | `background` | 窗口底、`tiles`、按钮上的字（primary.foreground）、**Dialog 的面板本体**（`dialog.rs:613` 画的是 `tokens.background`） |
/// | `bg_surface` | `panel` | `secondary` / `list` / `table` / `sidebar` / `title_bar` / 活动 tab |
/// | `bg_elevated` | `panelAlt` | `popover`（PopupMenu / Select / HoverCard 的外壳）、`muted`、`accent`、滚动条轨道、switch / slider 槽 |
/// | `accent` | `accent` | `primary` / `ring`（聚焦圈）/ `caret` / `link` / 选区 / 拖放高亮 |
/// | `text_primary` | `text` | `foreground` 与各处前景 |
/// | `text_muted` | `muted` | `muted.foreground` / 表头 / 非活动 tab |
/// | `border_default` | `line` | `border` / `input.border` / `window.border` / 滚动条 thumb |
/// | `color_warning` | `accentAlt` | `warning.*` |
/// | `color_info` | `secondary` | `info.*` |
/// | `color_success` | `highlight` | `success.*` |
///
/// 后三个在皮肤那头是**可选**槽位（不写就不覆盖组件库基线），内置这头总是给 ——
/// 壳本来就有这三个语义色，留着组件库的绿/黄/蓝只会和状态灯对不上。
fn builtin_tokens(palette: &Palette) -> ThemeTokens {
    ThemeTokens {
        background: palette.bg_base,
        panel: palette.bg_surface,
        panel_alt: palette.bg_elevated,
        accent: palette.accent,
        text: palette.text_primary,
        muted: palette.text_muted,
        line: palette.border_default,
        accent_alt: Some(palette.color_warning),
        secondary: Some(palette.color_info),
        highlight: Some(palette.color_success),
    }
}

// ─── 代码高亮配色(`--syn-*` 九色 → gpui-component 的 HighlightTheme) ───

/// 把壳配色映射成 gpui-component 的语法高亮表,并装进 `Theme` 全局。
///
/// 消费方有两处,都读 `cx.theme().highlight_theme`:
/// 内置编辑器(`input/element.rs:773`)与 Markdown 预览里的代码块
/// (`text/node.rs:343`)—— 同一份表保证两处颜色一致。
///
/// # 为什么是九色而不是四十色
///
/// 原版 `CodeEditor.tsx:75-103` 的 `HighlightStyle` 只用了九个 CSS 变量
/// (`--syn-keyword` / `--syn-string` / `--syn-number` / `--syn-function` /
/// `--syn-type` / `--syn-property` / `--syn-tag` / `--syn-comment` / `--syn-operator`,
/// 定义在 `src/styles.css:50-59`,各自指向应用现有色板)。gpui-component 的
/// `SyntaxColors` 有 40 个名字,这里把它们**归到原版那九组**里 ——
/// 同一份主题在新旧两版里长得一样是本次迁移的硬指标,多分几档反而对不上。
///
/// # 为什么走 JSON 而不是结构体字面量
///
/// `ThemeStyle` 的三个字段是**私有**的(`highlighter/registry.rs:201-205`),
/// 组件库只留了 serde 这一条构造路(它本来就是给 Zed 主题 JSON 用的)。
/// 于是这里把颜色打成 `#rrggbbaa` 再 `from_value` —— 不是绕路,是唯一的公开入口。
fn install_highlight_theme(palette: &Palette, appearance: Appearance, cx: &mut App) {
    use gpui_component::highlighter::{HighlightTheme, HighlightThemeStyle, SyntaxColors};
    use gpui_component::{Theme, ThemeMode};

    /// `#rrggbbaa`。`Hsla` 的 `Deserialize` 走 `Rgba`,认的就是这种串。
    fn hex(color: gpui::Hsla) -> String {
        let rgba = gpui::Rgba::from(color);
        let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            b(rgba.r),
            b(rgba.g),
            b(rgba.b),
            b(rgba.a)
        )
    }
    /// 一条 `ThemeStyle`。`italic` / `bold` 对应原版那两条 `fontStyle` / `fontWeight`。
    fn style(color: gpui::Hsla, italic: bool, bold: bool) -> Option<gpui_component::highlighter::ThemeStyle> {
        let mut obj = serde_json::json!({ "color": hex(color) });
        if italic {
            obj["font_style"] = serde_json::json!("italic");
        }
        if bold {
            obj["font_weight"] = serde_json::json!(600);
        }
        serde_json::from_value(obj).ok()
    }

    let plain = |c: gpui::Hsla| style(c, false, false);

    // 九组,逐条对着 `CodeEditor.tsx:75-103` 的 tag 清单分派
    let keyword = plain(palette.color_ai); // --syn-keyword
    let string = plain(palette.color_success); // --syn-string
    let number = plain(palette.color_warning); // --syn-number(含 bool/null/atom/self)
    let function = plain(palette.color_file); // --syn-function
    let type_ = plain(palette.color_folder); // --syn-type(含 class/namespace/annotation)
    let property = plain(palette.color_info); // --syn-property(含 attribute/label/link)
    let tag = plain(palette.color_error); // --syn-tag
    let comment = style(palette.text_muted, true, false); // --syn-comment,原版带斜体
    let operator = plain(palette.text_secondary); // --syn-operator(含 punctuation/bracket)
    let primary = plain(palette.text_primary);

    let mut syntax = SyntaxColors::default();
    syntax.keyword = keyword;
    syntax.boolean = number;
    syntax.constant = number;
    syntax.number = number;
    syntax.variable_special = number;
    syntax.string = string;
    syntax.string_escape = string;
    syntax.string_regex = string;
    syntax.string_special = string;
    syntax.string_special_symbol = string;
    syntax.text_literal = string;
    syntax.comment = comment;
    syntax.comment_doc = comment;
    // 原版 `meta` / `processingInstruction` 也走 --syn-comment
    syntax.preproc = comment;
    syntax.function = function;
    syntax.constructor = function;
    syntax.type_ = type_;
    syntax.enum_ = type_;
    syntax.variant = type_;
    syntax.property = property;
    syntax.attribute = property;
    syntax.label = property;
    syntax.link_text = property;
    syntax.link_uri = property;
    syntax.tag = tag;
    syntax.tag_doctype = tag;
    syntax.operator = operator;
    syntax.punctuation = operator;
    syntax.punctuation_bracket = operator;
    syntax.punctuation_delimiter = operator;
    syntax.punctuation_list_marker = operator;
    syntax.punctuation_special = operator;
    // 标题在原版是 accent + 600(Markdown 源码态)
    syntax.title = style(palette.accent, false, true);
    syntax.emphasis = style(palette.text_primary, true, false);
    syntax.emphasis_strong = style(palette.text_primary, false, true);
    syntax.variable = primary;
    syntax.primary = primary;
    syntax.embedded = primary;
    // 补全提示类:灰掉(原版没有对应 tag,取 --text-muted 最接近)
    syntax.hint = plain(palette.text_muted);
    syntax.predictive = plain(palette.text_muted);
    // 0.6.1 新增:Markdown 行内代码,与代码块同色
    syntax.text_code_span = primary;

    let mut highlight_style = HighlightThemeStyle::default();
    // 行号栏那条整高 quad 用它(gpui-component input/element.rs 的
    // 「Paint line numbers」段)。跟 bg_document 走:背景图皮肤下文件页
    // 整页半透明,行号栏刷不透明 bg_base 会留一条实色竖带。代价是长行
    // 横向滚进行号栏下方时遮不严(28% 透底),与「终端文字直接坐在
    // 氛围图上」同档,可接受
    highlight_style.editor_background = Some(palette.bg_document);
    highlight_style.editor_foreground = Some(palette.text_primary);
    // 活动行:accent 的极淡一档(原版 `.cm-activeLine` 用 --accent-subtle)
    highlight_style.editor_active_line = Some(palette.accent_subtle);
    highlight_style.editor_line_number = Some(palette.text_muted);
    highlight_style.editor_active_line_number = Some(palette.text_primary);
    highlight_style.syntax = syntax;

    let mut theme = (*HighlightTheme::default_dark()).clone();
    theme.name = "mini-term".to_string();
    theme.appearance = match appearance {
        Appearance::Dark => ThemeMode::Dark,
        Appearance::Light => ThemeMode::Light,
    };
    theme.style = highlight_style;
    Theme::global_mut(cx).highlight_theme = std::sync::Arc::new(theme);
}

fn builtin_palette(appearance: Appearance) -> Palette {
    match appearance {
        Appearance::Dark => Palette::dark(),
        Appearance::Light => Palette::light(),
    }
}

/// 内置外观下的终端配色。`follow == false` 时固定内置暗色(旧版同一行为)。
fn builtin_terminal(appearance: Appearance, follow: bool) -> TerminalTheme {
    if follow {
        builtin_terminal_theme(appearance)
    } else {
        builtin_dark_terminal_theme()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `light` / `dark` 直取;`auto` 与非法值跟随系统 —— 这里只能验前两者
    /// (系统外观要 `App`,单测里没有)。
    #[test]
    fn 明暗态解析的固定分支() {
        // 纯字符串分支不碰 cx,单独抽一个同构判定来钉住
        fn fixed(theme: &str) -> Option<Appearance> {
            match theme {
                "light" => Some(Appearance::Light),
                "dark" => Some(Appearance::Dark),
                _ => None,
            }
        }
        assert_eq!(fixed("light"), Some(Appearance::Light));
        assert_eq!(fixed("dark"), Some(Appearance::Dark));
        assert_eq!(fixed("auto"), None, "auto 必须落到系统分支");
        assert_eq!(fixed("Dark"), None, "大小写不匹配按 auto,与前端一致");
    }

    /// 终端不跟随主题时固定内置暗色 —— 亮色主题下也是暗色终端(旧版同一行为)。
    #[test]
    fn 终端跟随开关关掉时固定暗色() {
        assert_eq!(
            builtin_terminal(Appearance::Light, false),
            builtin_dark_terminal_theme()
        );
        assert_eq!(
            builtin_terminal(Appearance::Light, true),
            builtin_terminal_theme(Appearance::Light)
        );
        assert_eq!(
            builtin_terminal(Appearance::Dark, true),
            builtin_dark_terminal_theme()
        );
    }

    /// 明暗两套壳配色不能撞 —— 撞了说明 light() 抄漏了。
    #[test]
    fn 亮暗两套壳配色互不相同() {
        let dark = builtin_palette(Appearance::Dark);
        let light = builtin_palette(Appearance::Light);
        assert_ne!(dark, light);
        assert_ne!(dark.bg_base, light.bg_base);
        assert_ne!(dark.text_primary, light.text_primary);
    }

    /// 内置明/暗两套都必须经同一份 token 映射装进 gpui-component 主题层 ——
    /// 上游组件（Dialog 外壳 / Input / Scrollbar / PopupMenu / TextView）的颜色
    /// 与壳自绘面板同源就靠这一步。此前内置那条路只调 `restore_builtin_gpui_theme`
    /// 把组件库主题恢复成出厂中性灰，于是暖棕面板里套一个灰底弹窗。
    ///
    /// 这条同时是 [`builtin_tokens`] 那张改名表的对账：写反一格（比如
    /// `panel` 填成 `bg_elevated`）这里立刻红。
    #[test]
    fn 内置调色板经_token_映射装进组件库主题() {
        fn hex8(c: gpui::Hsla) -> String {
            let rgba = gpui::Rgba::from(c);
            let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            format!(
                "#{:02x}{:02x}{:02x}{:02x}",
                b(rgba.r),
                b(rgba.g),
                b(rgba.b),
                b(rgba.a)
            )
        }
        fn slot(value: &Option<gpui::SharedString>) -> &str {
            value.as_ref().map(|s| s.as_ref()).unwrap_or("<未映射>")
        }

        for appearance in [Appearance::Dark, Appearance::Light] {
            let p = builtin_palette(appearance);
            // 装配时 mt-ui 那头就是这么调的（内置没有背景图，面板恒不透明）
            let config =
                theme_bridge::build_theme_config("mini-term", appearance, &builtin_tokens(&p), 1.0);
            let c = &config.colors;
            let what = format!("{appearance:?}");
            // Dialog 的面板本体画 `tokens.background`；TextView / 主区同源
            assert_eq!(slot(&c.background), hex8(p.bg_base), "{what} background");
            assert_eq!(
                slot(&c.foreground),
                hex8(p.text_primary),
                "{what} foreground"
            );
            // 聚焦圈 / 光标 / 链接：原版 `.md-preview a` 就是 --accent
            assert_eq!(slot(&c.ring), hex8(p.accent), "{what} ring");
            assert_eq!(slot(&c.caret), hex8(p.accent), "{what} caret");
            assert_eq!(slot(&c.link), hex8(p.accent), "{what} link");
            // 边框：Input / Dialog / 分隔线三处同源
            assert_eq!(slot(&c.border), hex8(p.border_default), "{what} border");
            assert_eq!(
                slot(&c.input),
                hex8(p.border_default),
                "{what} input.border"
            );
            // 浮层外壳（PopupMenu / Select / HoverCard）= 壳的 bg_elevated
            assert_eq!(slot(&c.popover), hex8(p.bg_elevated), "{what} popover");
            // 面板类（list / table / sidebar / title_bar / 活动 tab）= 壳的 bg_surface
            assert_eq!(slot(&c.secondary), hex8(p.bg_surface), "{what} secondary");
            assert_eq!(slot(&c.list), hex8(p.bg_surface), "{what} list");
            // 语义三色跟壳走，别留组件库自己的绿/黄/蓝
            assert_eq!(slot(&c.warning), hex8(p.color_warning), "{what} warning");
            assert_eq!(slot(&c.info), hex8(p.color_info), "{what} info");
            assert_eq!(slot(&c.success), hex8(p.color_success), "{what} success");
        }
    }

    fn minimal_pack_json(extra: &str) -> String {
        format!(
            r##"{{
              "id": "t", "name": "T", "appearance": "dark",
              "colors": {{
                "background": "#101010", "panel": "#202020", "panelAlt": "#303030",
                "accent": "#4080ff", "text": "#eeeeee", "muted": "#888888", "line": "#404040"
              }}{extra}
            }}"##
        )
    }

    /// 主题包 → 壳配色的映射逐条对齐 `buildTokenMap`。
    #[test]
    fn 主题包映射进壳配色() {
        let def = theme_bridge::parse_theme_pack("t", &minimal_pack_json("")).unwrap();
        let applied = theme_bridge::resolve_theme_pack(&def, None);
        let p = Palette::from_pack(&applied);

        let hex = |c: gpui::Hsla| {
            let rgba = gpui::Rgba::from(c);
            let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            format!("#{:02x}{:02x}{:02x}", b(rgba.r), b(rgba.g), b(rgba.b))
        };
        assert_eq!(hex(p.bg_base), "#101010");
        assert_eq!(hex(p.bg_surface), "#202020");
        assert_eq!(hex(p.bg_elevated), "#303030");
        // 浮层始终不透明
        assert_eq!(hex(p.bg_overlay), "#303030");
        assert_eq!(p.bg_overlay.a, 1.0);
        assert_eq!(hex(p.accent), "#4080ff");
        assert_eq!(hex(p.text_primary), "#eeeeee");
        assert_eq!(hex(p.text_muted), "#888888");
        assert_eq!(hex(p.border_default), "#404040");
        // text-secondary = 75% alpha 的 text;border-subtle = 60% alpha 的 line
        assert!((p.text_secondary.a - 0.75).abs() < 0.01);
        assert!((p.border_subtle.a - 0.6).abs() < 0.01);
        // 无背景图:面板不透明
        assert_eq!(p.bg_surface.a, 1.0);
        // 包里没写的语义色保留内置暗色
        assert_eq!(p.color_error, Palette::dark().color_error);
        assert_eq!(p.color_success, Palette::dark().color_success);
    }

    /// `highlight` → `--color-success`,`secondary` → `--color-info`。
    #[test]
    fn 可选语义色的近似归宿() {
        let json = minimal_pack_json("").replace(
            r##""line": "#404040""##,
            r##""line": "#404040", "highlight": "#7bd88f", "secondary": "#7dd3c0""##,
        );
        let def = theme_bridge::parse_theme_pack("t", &json).unwrap();
        let applied = theme_bridge::resolve_theme_pack(&def, None);
        let p = Palette::from_pack(&applied);
        assert_ne!(p.color_success, Palette::dark().color_success);
        assert_ne!(p.color_info, Palette::dark().color_info);
    }

    /// 亮色包走亮色基线(未映射的语义色取亮色值)。
    #[test]
    fn 亮色包的未映射语义色走亮色基线() {
        let json = minimal_pack_json("").replace("\"dark\"", "\"light\"");
        let def = theme_bridge::parse_theme_pack("t", &json).unwrap();
        let applied = theme_bridge::resolve_theme_pack(&def, None);
        let p = Palette::from_pack(&applied);
        assert_eq!(p.color_error, Palette::light().color_error);
        assert_eq!(p.color_folder, Palette::light().color_folder);
    }
}
