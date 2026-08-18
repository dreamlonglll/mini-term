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
//! # 已知缺口
//!
//! `skin`(内置皮肤 blueprint / fluent2)没有实现:GPUI 侧还没有对应的
//! 内置皮肤色表,当前一律按 `none` 处理。背景图([`BackgroundArt`])只把数据
//! 带出来,渲染归 mt-ui(见 `theme_bridge` 末尾的 TODO)。

use gpui::{App, Window, WindowAppearance};
use gpui_component::{Theme, ThemeMode, ThemeRegistry};
use mt_config::AppConfig;
use mt_ui::TerminalTheme;
use mt_ui::theme_bridge::{
    self, Appearance, AppliedThemePack, BackgroundArt, ThemePackDef, builtin_dark_terminal_theme,
    builtin_terminal_theme,
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
    /// 有背景图时的氛围层参数。**渲染未实现**,先把数据带出来。
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

/// themes/ 目录。**走 [`crate::app_data_dir`] 而不是
/// `mt_config::ThemePacks::open()`** —— 后者钉死在装机版目录上,
/// `MT_APP_DATA_DIR` 隔离模式下会读到装机版的皮肤。
fn theme_packs() -> mt_config::ThemePacks {
    mt_config::ThemePacks::at(crate::app_data_dir().join("themes"))
}

/// 读一个外置主题包并算出全部产物(不改任何全局状态)。
fn load_pack(theme_id: &str) -> anyhow::Result<(ThemePackDef, AppliedThemePack)> {
    let packs = theme_packs();
    let data = packs.read(theme_id)?;
    let def = theme_bridge::parse_theme_pack(theme_id, &data.theme_json)?;
    let applied = theme_bridge::resolve_theme_pack(&def, Some(&data.dir));
    Ok((def, applied))
}

/// 可用的外置主题包(坏包跳过,设置页的皮肤列表用它)。
#[allow(dead_code)] // 设置面板「外观」页的落点(下一批)
pub fn list_packs() -> Vec<(ThemePackDef, std::path::PathBuf)> {
    theme_bridge::list_theme_packs(&theme_packs()).unwrap_or_else(|err| {
        eprintln!("[theme] 主题目录读取失败: {err:#}");
        Vec::new()
    })
}

/// 把 gpui-component 的主题层恢复成内置明暗基线。
///
/// **不能只调 `Theme::change(mode)`**:`install_gpui_theme` 会把皮肤的
/// `ThemeConfig` 写进 `Theme::dark_theme`/`light_theme`,那是**持久**的覆盖 ——
/// 退出皮肤时不把它换回注册表里的默认值,浮层会一直停在皮肤配色上。
fn install_builtin_gpui_theme(appearance: Appearance, window: Option<&mut Window>, cx: &mut App) {
    let dark = appearance.is_dark();
    let mode = if dark { ThemeMode::Dark } else { ThemeMode::Light };
    // Theme 全局可能还没建(`gpui_component::init` 之前),先建一个
    if !cx.has_global::<Theme>() {
        Theme::change(mode, None, cx);
    }
    let default = {
        let registry = ThemeRegistry::global(cx);
        if dark {
            registry.default_dark_theme().clone()
        } else {
            registry.default_light_theme().clone()
        }
    };
    {
        let theme = Theme::global_mut(cx);
        if dark {
            theme.dark_theme = default;
        } else {
            theme.light_theme = default;
        }
    }
    Theme::change(mode, window, cx);
}

/// 按配置装配主题。**这是唯一的装配入口**(启动、切亮暗、切皮肤都走它)。
pub fn apply(config: &AppConfig, window: Option<&mut Window>, cx: &mut App) -> AppliedTheme {
    let follow = config.terminal_follow_theme;

    if let Some(theme_id) = config.custom_theme_id.as_deref() {
        match load_pack(theme_id) {
            Ok((def, applied)) => {
                theme_bridge::install_gpui_theme(&applied, window, cx);
                let palette = Palette::from_pack(&def, &applied);
                return AppliedTheme {
                    appearance: def.appearance,
                    palette,
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
                install_builtin_gpui_theme(appearance, window, cx);
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
    install_builtin_gpui_theme(appearance, window, cx);
    AppliedTheme {
        appearance,
        palette: builtin_palette(appearance),
        terminal: builtin_terminal(appearance, follow),
        background: None,
        failed_pack: None,
    }
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
        let p = Palette::from_pack(&def, &applied);

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
        let p = Palette::from_pack(&def, &applied);
        assert_ne!(p.color_success, Palette::dark().color_success);
        assert_ne!(p.color_info, Palette::dark().color_info);
    }

    /// 亮色包走亮色基线(未映射的语义色取亮色值)。
    #[test]
    fn 亮色包的未映射语义色走亮色基线() {
        let json = minimal_pack_json("").replace("\"dark\"", "\"light\"");
        let def = theme_bridge::parse_theme_pack("t", &json).unwrap();
        let applied = theme_bridge::resolve_theme_pack(&def, None);
        let p = Palette::from_pack(&def, &applied);
        assert_eq!(p.color_error, Palette::light().color_error);
        assert_eq!(p.color_folder, Palette::light().color_folder);
    }
}
