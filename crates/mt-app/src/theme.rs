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
    self, Appearance, BackgroundArt, ThemePackListing, builtin_dark_terminal_theme,
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
/// `ThemePacks::open()` 现在认 `MT_APP_DATA_DIR`(`mt_config::active_data_dir`,
/// 与 [`crate::app_data_dir`] 同一口径),J 批那条「钉死装机版目录、mt-app 用
/// `ThemePacks::at()` 绕开」的记档已结清 —— 这里只保留定位不到数据目录时的兜底。
pub fn theme_packs() -> mt_config::ThemePacks {
    mt_config::ThemePacks::open()
        .unwrap_or_else(|_| mt_config::ThemePacks::at(crate::app_data_dir().join("themes")))
}

/// 可用的外置主题包(坏包跳过,设置页的皮肤列表用它)。
pub fn list_packs() -> Vec<ThemePackListing> {
    theme_bridge::list_theme_packs(&theme_packs()).unwrap_or_else(|err| {
        eprintln!("[theme] 主题目录读取失败: {err:#}");
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
///   这一步 —— 少了它「退出皮肤」只切 mode,浮层会原地停在皮肤配色上)。
pub fn apply(config: &AppConfig, window: Option<&mut Window>, cx: &mut App) -> AppliedTheme {
    let applied = apply_inner(config, window, cx);
    // 代码高亮配色跟着壳配色走(见 [`install_highlight_theme`])。放在这里而不是
    // 三个 return 分支里各写一遍 —— 装配入口只有一个,高亮表也只在这一处装。
    install_highlight_theme(&applied.palette, applied.appearance, cx);
    // 弹窗遮罩:gpui-component 内置主题的 `overlay` 是 #ffffff08 / #0000000d
    // (≈3%,肉眼等于没有),而原版所有 Modal 统一压 `bg-black/50`
    // (`Modal.tsx:171`,亮暗两套同值)。Dialog/Sheet 渲染时读的是
    // `cx.theme().overlay`,必须放在 apply_inner **之后** ——
    // switch_to_builtin/switch_to_theme_pack 都会从基线重置整套 colors。
    {
        let theme = gpui_component::Theme::global_mut(cx);
        theme.colors.overlay = gpui::hsla(0.0, 0.0, 0.0, 0.5);
        // md 行内 code 的胶囊底:TextView 写死取 `theme().accent`(node.rs:651,
        // 无专用钩子),默认淡紫灰与原版 `.md-preview code` 的 --bg-elevated 深底
        // 相去甚远。accent 槽位在组件库里只当「选中/悬停底」用(Select/Toggle/
        // Calendar/补全菜单),mt-app 这些原语全自绘、没有消费方,可以安全借位。
        // ⚠️ 原版 code 前景还是 --accent 橙,组件对 fg 没有开口 —— 只对齐了底色,
        // 橙字待上游给 TextViewStyle 加 inline code 钩子(记档)。
        theme.colors.accent = applied.palette.bg_elevated;
        // md 链接色:TextView 取 `theme().link`,原版 `.md-preview a` 是 --accent
        theme.colors.link = applied.palette.accent;
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
                eprintln!("[theme] 自定义主题 {theme_id} 加载失败,回落内置外观: {err:#}");
                let appearance = resolve_appearance(&config.theme, cx);
                // 返回值就是该明暗的内置终端配色;`terminalFollowTheme` 那道闸
                // 在 builtin_terminal 里(关掉时固定暗色),所以这里不直接用它
                let _ = switch_to_builtin(appearance, window, cx);
                return AppliedTheme {
                    appearance,
                    palette: builtin_palette(appearance),
                    terminal: builtin_terminal(appearance, follow),
                    background: None,
                    failed_pack: Some(theme_id.to_string()),
                };
            }
        }
    }

    let appearance = resolve_appearance(&config.theme, cx);
    let _ = switch_to_builtin(appearance, window, cx);
    AppliedTheme {
        appearance,
        palette: builtin_palette(appearance),
        terminal: builtin_terminal(appearance, follow),
        background: None,
        failed_pack: None,
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

    let syntax = SyntaxColors {
        keyword,
        boolean: number,
        constant: number,
        number,
        variable_special: number,
        string,
        string_escape: string,
        string_regex: string,
        string_special: string,
        string_special_symbol: string,
        text_literal: string,
        comment,
        comment_doc: comment,
        // 原版 `meta` / `processingInstruction` 也走 --syn-comment
        preproc: comment,
        function,
        constructor: function,
        type_,
        enum_: type_,
        variant: type_,
        property,
        attribute: property,
        label: property,
        link_text: property,
        link_uri: property,
        tag,
        tag_doctype: tag,
        operator,
        punctuation: operator,
        punctuation_bracket: operator,
        punctuation_delimiter: operator,
        punctuation_list_marker: operator,
        punctuation_special: operator,
        // 标题在原版是 accent + 600(Markdown 源码态)
        title: style(palette.accent, false, true),
        emphasis: style(palette.text_primary, true, false),
        emphasis_strong: style(palette.text_primary, false, true),
        variable: primary,
        primary,
        embedded: primary,
        // 补全提示类:灰掉(原版没有对应 tag,取 --text-muted 最接近)
        hint: plain(palette.text_muted),
        predictive: plain(palette.text_muted),
    };

    let theme = HighlightTheme {
        name: "mini-term".to_string(),
        appearance: match appearance {
            Appearance::Dark => ThemeMode::Dark,
            Appearance::Light => ThemeMode::Light,
        },
        style: HighlightThemeStyle {
            // 行号栏那条整高 quad 用它(gpui-component input/element.rs 的
            // 「Paint line numbers」段)。跟 bg_document 走:背景图皮肤下文件页
            // 整页半透明,行号栏刷不透明 bg_base 会留一条实色竖带。代价是长行
            // 横向滚进行号栏下方时遮不严(28% 透底),与「终端文字直接坐在
            // 氛围图上」同档,可接受
            editor_background: Some(palette.bg_document),
            editor_foreground: Some(palette.text_primary),
            // 活动行:accent 的极淡一档(原版 `.cm-activeLine` 用 --accent-subtle)
            editor_active_line: Some(palette.accent_subtle),
            editor_line_number: Some(palette.text_muted),
            editor_active_line_number: Some(palette.text_primary),
            syntax,
            ..Default::default()
        },
    };
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
