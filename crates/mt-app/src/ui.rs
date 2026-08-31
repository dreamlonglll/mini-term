//! 应用配色与几个复用的小件。
//!
//! # 一张表,两个来源
//!
//! [`Palette`] 是壳的全部配色 token,逐条对应 `src/styles.css` 的 CSS 变量。
//! 取值有两条来源:
//!
//! - **内置外观**:`Palette::dark()` / `Palette::light()`,逐值抄 `:root` 与
//!   `:root[data-theme="light"]`;
//! - **外置主题包**:`Palette::from_pack`,映射逐条对齐
//!   `src/utils/themePackManager.ts::buildTokenMap`。
//!
//! 装配在 [`crate::theme`],这里只负责「当前是哪一份」。
//!
//! # 为什么用 thread_local 而不是给每个取色函数加 `cx`
//!
//! `ui::accent()` 这类调用散在十几个文件、上百处;为了换主题给它们统统加一个
//! `&App` 参数,收益只是省掉一个进程内单例。gpui 的视图全在主线程上跑,
//! 一份 `thread_local` 快照足够,而且**这是唯一的替换点** —— 换主题时
//! [`set_palette`] 改一次,下一帧所有视图自动跟着变。

use std::cell::RefCell;

use std::rc::Rc;

use gpui::{
    AnyElement, App, Div, ElementId, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseMoveEvent, ParentElement, Pixels, RenderOnce, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, white,
};
use mt_ui::icons::vector::{Geom, Ink, Shape, VectorIcon};
use mt_ui::icons::{StatusDot, StatusKind};
use mt_ui::rgb8;
use mt_ui::theme_bridge::{AppliedThemePack, ThemeSlot};

use crate::tree::PaneStatus;

/// 壳的配色 token 表(对应 `styles.css` 的一组 CSS 变量)。
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub bg_base: Hsla,
    /// 主区文档页(文件编辑器)的容器层底色:与 `bg_base` 同色,但吃
    /// `surface_opacity` —— 背景图皮肤下随面板一起透出氛围图。
    /// 原版无此变量(FileViewerModal 是弹窗,按「浮层不透明」走);文件页改成
    /// 主区页签后与终端区同层级,着色得走面板那条半透明路。
    pub bg_document: Hsla,
    pub bg_surface: Hsla,
    pub bg_elevated: Hsla,
    pub bg_overlay: Hsla,
    pub bg_terminal: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    pub accent_subtle: Hsla,
    /// `--accent-muted`(styles.css:18/94)。比 `accent_subtle` 更实的一档，
    /// 设置页单选段(`ChoiceGroup`)的选中底色用它。
    pub accent_muted: Hsla,
    pub border_subtle: Hsla,
    pub border_default: Hsla,
    pub border_strong: Hsla,
    pub color_success: Hsla,
    pub color_error: Hsla,
    pub color_warning: Hsla,
    pub color_ai_working: Hsla,
    pub color_folder: Hsla,
    pub color_file: Hsla,
    pub color_info: Hsla,
    /// `--color-ai`。排行条渐变的右端。主题包**不映射**它
    /// （`themePackManager.ts::buildTokenMap` 没有这一条），恒用内置值。
    pub color_ai: Hsla,
    /// `--diff-add-bg` / `--diff-del-bg` / `--diff-add-text` / `--diff-del-text`
    /// （diff 视图的四格）。
    ///
    /// ⚠️ `mt_ui::theme_bridge::ThemeSlot` **没有 diff 槽位**，主题包里这四个色
    /// 无来源。取舍：`from_pack` 按 success / error 派生（底色取同色 12% alpha，
    /// 文字色直接用语义色），**不为此扩 ThemeSlot** —— 那会改动主题包格式，
    /// 超出本批范围。内置明暗两套仍逐值抄 `styles.css`。
    pub diff_add_bg: Hsla,
    pub diff_del_bg: Hsla,
    pub diff_add_text: Hsla,
    pub diff_del_text: Hsla,
}

/// 乘性改 alpha(与前端 `withAlpha` / `scaleAlpha` 同语义)。
fn alpha(color: Hsla, a: f32) -> Hsla {
    Hsla {
        a: (color.a * a).clamp(0.0, 1.0),
        ..color
    }
}

impl Palette {
    /// 暗色基线:逐值取自 `src/styles.css` 的 `:root`。
    pub fn dark() -> Self {
        Self {
            bg_base: rgb8(0x08, 0x07, 0x06),
            bg_document: rgb8(0x08, 0x07, 0x06),
            bg_surface: rgb8(0x12, 0x11, 0x10),
            bg_elevated: rgb8(0x1c, 0x1a, 0x18),
            bg_overlay: rgb8(0x25, 0x23, 0x20),
            bg_terminal: rgb8(0x0a, 0x09, 0x08),
            text_primary: rgb8(0xf0, 0xec, 0xe6),
            text_secondary: rgb8(0xa8, 0xa0, 0x98),
            text_muted: rgb8(0x6a, 0x62, 0x58),
            accent: rgb8(0xc8, 0x80, 0x5a),
            accent_subtle: Hsla {
                a: 0.10,
                ..rgb8(0xc8, 0x80, 0x5a)
            },
            accent_muted: Hsla {
                a: 0.20, // #c8805a33
                ..rgb8(0xc8, 0x80, 0x5a)
            },
            border_subtle: Hsla {
                a: 0.05,
                ..rgb8(0xff, 0xff, 0xff)
            },
            border_default: Hsla {
                a: 0.08,
                ..rgb8(0xff, 0xff, 0xff)
            },
            border_strong: Hsla {
                a: 0.12,
                ..rgb8(0xff, 0xff, 0xff)
            },
            color_success: rgb8(0x6b, 0xb8, 0x7a),
            color_error: rgb8(0xd4, 0x60, 0x5a),
            color_warning: rgb8(0xd4, 0xa8, 0x4a),
            color_ai_working: rgb8(0xf5, 0xc5, 0x18),
            color_folder: rgb8(0xd4, 0xc8, 0xa0),
            color_file: rgb8(0x7d, 0xcf, 0xb8),
            color_info: rgb8(0x6a, 0x9f, 0xd4),
            color_ai: rgb8(0xb0, 0x8c, 0xd4),
            // `styles.css:40-43`
            diff_add_bg: Hsla {
                a: 0.12,
                ..rgb8(60, 180, 60)
            },
            diff_del_bg: Hsla {
                a: 0.12,
                ..rgb8(220, 60, 60)
            },
            diff_add_text: rgb8(0x6b, 0xb8, 0x7a),
            diff_del_text: rgb8(0xd4, 0x60, 0x5a),
        }
    }

    /// 亮色基线:逐值取自 `src/styles.css` 的 `:root[data-theme="light"]`。
    pub fn light() -> Self {
        Self {
            bg_base: rgb8(0xff, 0xff, 0xff),
            bg_document: rgb8(0xff, 0xff, 0xff),
            bg_surface: rgb8(0xf5, 0xf5, 0xf5),
            bg_elevated: rgb8(0xeb, 0xeb, 0xeb),
            bg_overlay: rgb8(0xe0, 0xe0, 0xe0),
            bg_terminal: rgb8(0xfa, 0xfa, 0xfa),
            text_primary: rgb8(0x0a, 0x0a, 0x0a),
            text_secondary: rgb8(0x50, 0x50, 0x50),
            text_muted: rgb8(0x80, 0x80, 0x80),
            accent: rgb8(0xb0, 0x68, 0x30),
            accent_subtle: Hsla {
                a: 0.094, // #b0683018
                ..rgb8(0xb0, 0x68, 0x30)
            },
            accent_muted: Hsla {
                a: 0.20, // #b0683033
                ..rgb8(0xb0, 0x68, 0x30)
            },
            border_subtle: Hsla {
                a: 0.06,
                ..rgb8(0x00, 0x00, 0x00)
            },
            border_default: Hsla {
                a: 0.10,
                ..rgb8(0x00, 0x00, 0x00)
            },
            border_strong: Hsla {
                a: 0.15,
                ..rgb8(0x00, 0x00, 0x00)
            },
            color_success: rgb8(0x2d, 0x8a, 0x46),
            color_error: rgb8(0xc0, 0x39, 0x2b),
            color_warning: rgb8(0xb0, 0x86, 0x20),
            color_ai_working: rgb8(0xc4, 0x52, 0x1a),
            color_folder: rgb8(0x8a, 0x7a, 0x40),
            color_file: rgb8(0x1a, 0x8a, 0x6a),
            color_info: rgb8(0x28, 0x60, 0xa0),
            color_ai: rgb8(0x8a, 0x5c, 0xb8),
            // `styles.css:114-117`
            diff_add_bg: Hsla {
                a: 0.10,
                ..rgb8(40, 140, 40)
            },
            diff_del_bg: Hsla {
                a: 0.10,
                ..rgb8(200, 50, 40)
            },
            diff_add_text: rgb8(0x2d, 0x8a, 0x46),
            diff_del_text: rgb8(0xc0, 0x39, 0x2b),
        }
    }

    /// 外置主题包 → token 表。映射逐条对齐 `themePackManager.ts::buildTokenMap`。
    ///
    /// 入参只要 [`AppliedThemePack`] 一个:它自带 `colors` 原文与
    /// [`AppliedThemePack::color`],宿主不必再手拆 read → parse → resolve 四步
    /// 去够 `ThemePackDef`(见 `crate::theme::apply` 的说明)。这里从它身上取三样:
    /// 10 个语义色、`surface_opacity`(面板半透明度,无背景图时是 1.0)、
    /// 终端背景色(已含 `terminalOpacity`)。
    ///
    /// 包里没有的语义色(error / ai-working / folder / file)保留该明暗的内置值 ——
    /// 前端同样不映射它们。`accentAlt` 在前端归到 `--color-warning`(见
    /// `themePackManager.ts` 的 `map['--color-warning'] = c.accentAlt`),P 批补上
    /// 这一格之后照着映;包里没声明就退回该明暗的内置值。
    pub fn from_pack(applied: &AppliedThemePack) -> Self {
        let base = if applied.appearance.is_dark() {
            Self::dark()
        } else {
            Self::light()
        };
        // 必填槽位在 parse_theme_pack 阶段已校验过色值,color() 不会失败
        let background = applied.color(ThemeSlot::Background);
        let panel = applied.color(ThemeSlot::Panel);
        let panel_alt = applied.color(ThemeSlot::PanelAlt);
        let accent = applied.color(ThemeSlot::Accent);
        let text = applied.color(ThemeSlot::Text);
        let muted = applied.color(ThemeSlot::Muted);
        let line = applied.color(ThemeSlot::Line);
        let so = applied.surface_opacity;

        // diff 四色要按 success / error 派生,先把这两格算出来(struct 字面量里
        // 引用不到自己的其它字段)
        let success = match applied.colors.highlight {
            Some(_) => applied.color(ThemeSlot::Highlight),
            None => base.color_success,
        };
        let error = base.color_error;

        Self {
            bg_base: background,
            // 面板半透明才透得出背景图;无背景图时 surface_opacity = 1.0
            bg_document: alpha(background, so),
            bg_surface: alpha(panel, so),
            bg_elevated: alpha(panel_alt, so),
            // 浮层始终不透明:弹窗叠在任意内容上,半透明是拿可读性换观感
            bg_overlay: panel_alt,
            bg_terminal: applied.terminal.background,
            text_primary: text,
            text_secondary: alpha(text, 0.75),
            text_muted: muted,
            accent,
            accent_subtle: alpha(accent, 0.18),
            // `--accent-muted: withAlpha(c.accent, 0.33)`(themePackManager.ts:243)
            accent_muted: alpha(accent, 0.33),
            border_subtle: alpha(line, 0.6),
            border_default: line,
            // `--border-strong: scaleAlpha(c.line, 1.4)`（buildTokenMap 那一行）
            border_strong: alpha(line, 1.4),
            // `color(Highlight/Secondary)` 在包里没声明时会回落 accent(那是
            // themePackManager 写 CSS 变量的口径),而壳这两格的语义是「成功/信息」,
            // 回落 accent 会让完成态变成强调色 —— 所以先看 Option 在不在
            color_success: success,
            // 主题包没有 diff 槽位 —— 见 [`Palette::diff_add_bg`] 的取舍说明
            diff_add_bg: alpha(success, 0.12),
            diff_del_bg: alpha(error, 0.12),
            diff_add_text: success,
            diff_del_text: error,
            color_info: match applied.colors.secondary {
                Some(_) => applied.color(ThemeSlot::Secondary),
                None => base.color_info,
            },
            // 同上:`color(AccentAlt)` 未声明时回落 accent(那是写 CSS 变量的口径),
            // 而这一格的语义是「警告/高亮」,回落 accent 会让搜索命中的底色与强调色
            // 撞成一片 —— 所以先看 Option 在不在
            color_warning: match applied.colors.accent_alt {
                Some(_) => applied.color(ThemeSlot::AccentAlt),
                None => base.color_warning,
            },
            ..base
        }
    }
}

thread_local! {
    /// 当前生效的配色。改它的唯一入口是 [`set_palette`]。
    static CURRENT: RefCell<Palette> = RefCell::new(Palette::dark());
}

/// 换一整套配色(换主题包 / 切亮暗)。**唯一替换点**。
pub fn set_palette(palette: Palette) {
    CURRENT.with(|p| *p.borrow_mut() = palette);
}

/// 当前配色的一份拷贝。
#[allow(dead_code)] // 整套取色的口子(皮肤预览卡片改成逐色取,暂无调用点)
pub fn palette() -> Palette {
    CURRENT.with(|p| p.borrow().clone())
}

// ─── 界面字号 / 字族 ──────────────────────────────────────────
//
// 原版把 `uiFontSize` 写进 `html` 的 inline `font-size`(App.tsx:141),Tailwind 的
// `text-base/sm/xs` 都是 rem，于是一改全跟着变；`uiFontFamily` 走两个 CSS 变量
// (fontManager.ts:8-18)。GPUI 侧没有 rem 继承这回事，所以照 [`set_palette`] 的
// 同一套路来:一份 thread_local 快照 + 一个替换点，各处字号改走 [`font_px`]。
//
// ⚠️ **只缩放字号,不缩放间距**:原版 Tailwind 的 `px-3` / `gap-2` 同样是 rem，
// 改 `uiFontSize` 连带把内边距一起放大。GPUI 侧的间距是像素字面量,本批不动 ——
// 差异在极端档位(10px / 20px)下看得出来,记档在交付说明里。

/// 原版 `html` 的默认 `font-size`(`config.uiFontSize ?? 13`,App.tsx:141)。
/// 各处 `font_px(13.0)` 之类的字面量就是按这个基准写的。
pub const BASE_UI_FONT_SIZE: f32 = 13.0;

/// 界面字号 / 字族快照。改它的唯一入口是 [`set_ui_font`]。
#[derive(Clone, Debug, PartialEq)]
pub struct UiFont {
    /// `config.uiFontSize`。滑块范围 10..20(与原版一致)。
    pub size: f32,
    /// `config.uiFontFamily` 的**首个**字体族。`None` = 平台默认。
    ///
    /// 原版存的是一整串 CSS `font-family`(带回退列表),而 gpui 的
    /// `Styled::font_family` 只收一个族名 —— 取首项,其余靠平台字体回退。
    pub family: Option<SharedString>,
}

impl Default for UiFont {
    fn default() -> Self {
        Self {
            size: BASE_UI_FONT_SIZE,
            family: None,
        }
    }
}

thread_local! {
    static UI_FONT: RefCell<UiFont> = RefCell::new(UiFont::default());
}

/// 换界面字号 / 字族。**唯一替换点**,调用方随后 `cx.refresh_windows()`。
///
/// `size` 钳在 10..20(滑块范围);`family` 传整串 CSS font-family,这里取首项。
pub fn set_ui_font(size: f64, family: Option<&str>) {
    let family = family
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| first_font_family(s))
        .map(SharedString::from);
    let next = UiFont {
        size: (size as f32).clamp(10.0, 20.0),
        family,
    };
    UI_FONT.with(|f| *f.borrow_mut() = next);
}

/// 当前界面字族(挂在 `Workspace` 根上,靠继承透给所有子元素)。
pub fn ui_font_family() -> Option<SharedString> {
    UI_FONT.with(|f| f.borrow().family.clone())
}

/// 把「按 13px 基准写死的像素字号」换算到当前基准。
///
/// 各视图里写死的 `text_size(px(12.0))` 一律改成 `text_size(ui::font_px(12.0))` ——
/// 默认基准下等值,改了 `uiFontSize` 才整体跟着缩放(等价于原版的 rem 继承)。
pub fn font_px(base: f32) -> Pixels {
    let scale = UI_FONT.with(|f| f.borrow().size) / BASE_UI_FONT_SIZE;
    px(base * scale)
}

/// CSS `font-family` 串的首个族名(剥引号与空白)。
///
/// `'JetBrainsMono Nerd Font', monospace` → `JetBrainsMono Nerd Font`。
/// 全是空项时返回 `None`(等价于「没设」)。
pub fn first_font_family(list: &str) -> Option<String> {
    font_family_list(list).into_iter().next()
}

/// CSS `font-family` 串拆成族名列表(剥引号与空白,丢掉空项)。
///
/// **不丢 `monospace` 这类通用族名**:调用方(终端字族)自己决定要不要。
pub fn font_family_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(|part| part.trim().trim_matches(['"', '\'']).trim())
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn token(pick: impl Fn(&Palette) -> Hsla) -> Hsla {
    CURRENT.with(|p| pick(&p.borrow()))
}

// --- 背景 ---
/// `--bg-base`
pub fn bg_base() -> Hsla {
    token(|p| p.bg_base)
}
/// 文档页容器底色:带 `surface_opacity` 的 `bg_base`(见 [`Palette::bg_document`])。
pub fn bg_document() -> Hsla {
    token(|p| p.bg_document)
}
/// `--bg-surface`
pub fn bg_surface() -> Hsla {
    token(|p| p.bg_surface)
}
/// `--bg-elevated`
pub fn bg_elevated() -> Hsla {
    token(|p| p.bg_elevated)
}
/// `--bg-overlay`
pub fn bg_overlay() -> Hsla {
    token(|p| p.bg_overlay)
}
/// `--bg-terminal`
pub fn bg_terminal() -> Hsla {
    token(|p| p.bg_terminal)
}

// --- 前景 ---
/// `--text-primary`
pub fn text_primary() -> Hsla {
    token(|p| p.text_primary)
}
/// `--text-secondary`
pub fn text_secondary() -> Hsla {
    token(|p| p.text_secondary)
}
/// `--text-muted`
pub fn text_muted() -> Hsla {
    token(|p| p.text_muted)
}

// --- 强调与边框 ---
/// `--accent`
pub fn accent() -> Hsla {
    token(|p| p.accent)
}
/// `--accent-subtle`(原值是带 alpha 的 accent)
pub fn accent_subtle() -> Hsla {
    token(|p| p.accent_subtle)
}
/// `--accent-muted`(设置页单选段的选中底色)
pub fn accent_muted() -> Hsla {
    token(|p| p.accent_muted)
}
/// `--border-subtle`
pub fn border_subtle() -> Hsla {
    token(|p| p.border_subtle)
}
/// `--border-default`
pub fn border_default() -> Hsla {
    token(|p| p.border_default)
}
/// `--border-strong`(Token 副行的分隔符、浮层描边)
pub fn border_strong() -> Hsla {
    token(|p| p.border_strong)
}

// --- 语义色 ---
/// `--color-success`
pub fn color_success() -> Hsla {
    token(|p| p.color_success)
}
/// `--color-error`
pub fn color_error() -> Hsla {
    token(|p| p.color_error)
}
/// `--color-warning`(搜索命中高亮、结果截断提示)
pub fn color_warning() -> Hsla {
    token(|p| p.color_warning)
}
/// `--color-ai-working`
pub fn color_ai_working() -> Hsla {
    token(|p| p.color_ai_working)
}

/// 乘性改 alpha 的公开入口(`bg-[var(--x)]/30` 那种写法的对应物)。
pub fn with_alpha(color: Hsla, a: f32) -> Hsla {
    Hsla {
        a: a.clamp(0.0, 1.0),
        ..color
    }
}
/// `--color-folder`
pub fn color_folder() -> Hsla {
    token(|p| p.color_folder)
}
/// `--color-file`
pub fn color_file() -> Hsla {
    token(|p| p.color_file)
}

/// `--color-info`(统计面板的区块标题竖条等)
pub fn color_info() -> Hsla {
    token(|p| p.color_info)
}

/// `--color-ai`(排行条渐变的右端)
pub fn color_ai() -> Hsla {
    token(|p| p.color_ai)
}

// --- diff 四色 ---
/// `--diff-add-bg`
pub fn diff_add_bg() -> Hsla {
    token(|p| p.diff_add_bg)
}
/// `--diff-del-bg`
pub fn diff_del_bg() -> Hsla {
    token(|p| p.diff_del_bg)
}
/// `--diff-add-text`
pub fn diff_add_text() -> Hsla {
    token(|p| p.diff_add_text)
}
/// `--diff-del-text`
pub fn diff_del_text() -> Hsla {
    token(|p| p.diff_del_text)
}

// --- diff 的行内(词级)底色 ---
//
// 原版没有这两个色:一行里只改了一个字符,整行也被涂成红/绿,改哪儿要自己找。
// 主题包同样没有对应槽位(与 `Palette::diff_add_bg` 同一处取舍),所以按
// 「文字色 + 固定透明度」现调 —— 比整行底色实一档,叠在整行底色上刚好点出片段,
// 而且深浅两套主题、换肤主题都跟着走。

/// 新增行里**真正变了的片段**的底色。
pub fn diff_add_word_bg() -> Hsla {
    alpha(token(|p| p.diff_add_text), 0.3)
}

/// 删除行里**真正变了的片段**的底色。
pub fn diff_del_word_bg() -> Hsla {
    alpha(token(|p| p.diff_del_text), 0.3)
}

// --- 缓动 ---

/// CSS `cubic-bezier(x1, y1, x2, y2)` 的等价缓动函数。
///
/// `styles.css:67-78` 那两条浮层缓动(`--ease-overlay-in:
/// cubic-bezier(0.16, 1, 0.3, 1)` / `--ease-overlay-out:
/// cubic-bezier(0.4, 0, 0.9, 0.6)`)在 gpui 侧没有现成对应物,
/// `Animation::with_easing` 又只要一个 `Fn(f32) -> f32`,于是自己解一次。
///
/// 做法与浏览器一致:x(t) 与 y(t) 都是控制点为 `(0,0) (x1,y1) (x2,y2) (1,1)`
/// 的三次贝塞尔,给定 `x` 先用二分法反解 `t`,再取 `y(t)`。
/// 二分 20 次的精度是 1e-6,对 240ms 的动画绰绰有余(比牛顿法慢但恒收敛 ——
/// 控制点在 0..1 外时牛顿法会跑飞)。
pub fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32) -> impl Fn(f32) -> f32 {
    fn bezier(a: f32, b: f32, t: f32) -> f32 {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    }
    move |x: f32| {
        let x = x.clamp(0.0, 1.0);
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        let mut t = x;
        for _ in 0..20 {
            let sample = bezier(x1, x2, t);
            if sample < x {
                lo = t;
            } else {
                hi = t;
            }
            t = (lo + hi) * 0.5;
        }
        bezier(y1, y2, t)
    }
}

// --- 复用小件 ---
//
// 面板/Modal 里反复出现的三种东西:次要按钮、主按钮、区块标题。写死在各处的话
// 改一次配色要翻十个文件,而 i18n 与主题桥都指着这一张表做替换点。

/// 次要按钮(边框 + 淡色文字,hover 转 accent)。
pub fn ghost_button(id: impl Into<ElementId>, label: impl Into<String>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(border_default())
        .text_size(font_px(12.0))
        .text_color(text_secondary())
        .cursor_pointer()
        .hover(|el| el.border_color(accent()).text_color(accent()))
        .child(label.into())
}

/// 主按钮(实心 accent)。
pub fn primary_button(id: impl Into<ElementId>, label: impl Into<String>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(px(12.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .bg(accent())
        .text_size(font_px(12.0))
        .text_color(bg_base())
        .cursor_pointer()
        .hover(|el| el.opacity(0.9))
        .child(label.into())
}

/// 危险动作按钮(删除类)。
///
/// **单独一个函数而不是 `ghost_button(..).hover(..)`** —— gpui 的 `Div` 只允许设一次
/// hover 样式,第二次直接 panic(`hover style already set`),而 `ghost_button`
/// 里已经设过了。
pub fn danger_button(id: impl Into<ElementId>, label: impl Into<String>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(border_default())
        .text_size(font_px(12.0))
        .text_color(text_secondary())
        .cursor_pointer()
        .hover(|el| el.border_color(color_error()).text_color(color_error()))
        .child(label.into())
}

/// 区块标题:左侧竖条 + 文字(对齐 `usage/UsageStatsModal.tsx` 的 `Section`)。
pub fn section_title(text: impl Into<String>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .mb(px(8.0))
        .child(
            div()
                .w(px(2.0))
                .h(px(12.0))
                .rounded(px(1.0))
                .bg(color_info()),
        )
        .child(
            div()
                .text_size(font_px(12.0))
                .text_color(text_primary())
                .child(text.into()),
        )
}

// ─── 弹窗尺寸的视口钳制 ───────────────────────────────────────
//
// 原版 `Modal.tsx:159-179` 的面板是 `fixed inset-0 flex justify-center` 里的
// flex 子项,自带 `overflow-hidden`:窗口比面板窄时 flex-shrink 直接把它压回
// 窗口内(CSS 里 `min-width:auto` 对 overflow≠visible 的盒子解析为 0),
// 高度则由调用方的 `max-h-[80vh]` 管住。
//
// gpui-component 的 `Dialog` 两条都没有:宽是定值,位置按「视口中心 − 宽/2」
// 现算(`dialog.rs:368`),窗口比面板窄时面板**两侧一起出界**;高度是调用方
// 自己填的定值。这两个 helper 把口径收在一处 —— `Dialog` 的 builder 闭包每帧
// 都会被 `Root` 重新调一遍,所以拖窗口改大小时钳制值跟着变。

/// 弹窗宽度按视口钳制。`preferred` 是原版那个 `w-[..px]`。
///
/// 不留左右外边距 —— 原版那个 flex 容器也没有(只有 `justify-center`),
/// 极窄窗口下面板正好铺满窗口宽。
pub fn clamp_dialog_width(preferred: Pixels, viewport: gpui::Size<Pixels>) -> Pixels {
    preferred.min(viewport.width)
}

/// 按视口比例取弹窗宽度,`min_width` 是**下限**(窗口小到 `ratio` 算不出这么宽时
/// 仍按下限走),最后照例被 [`clamp_dialog_width`] 压回窗口内。
///
/// 设置面板这类内容密度高的弹窗用它:定值宽在 2K/4K 屏上只占一小条,右列控件挤
/// 在一起而两侧全是空遮罩;跟着视口走能把这份空间还给内容,同时保住小窗口下的
/// 可读宽度。
pub fn ratio_dialog_width(min_width: Pixels, ratio: f32, viewport: gpui::Size<Pixels>) -> Pixels {
    clamp_dialog_width((viewport.width * ratio).max(min_width), viewport)
}

/// 弹窗**正文**高度按视口钳制:`ratio` 是原版的 `max-h-[{ratio}vh]`,
/// `preferred` 是本弹窗自己的舒适上限,`chrome` 是标题栏之类正文之外的固定占用。
///
/// `Dialog` 的顶距默认 `视口高/10`(= 原版 `pt-[10vh]`),所以整块面板高不超过
/// 80% 时底边恰好落在视口内。返回值有 [`MIN_DIALOG_BODY`] 下限:窗口被拖到只剩
/// 一条时,面板宁可溢出也不该塌成一条缝。
pub fn clamp_dialog_body_height(
    preferred: Pixels,
    viewport: gpui::Size<Pixels>,
    ratio: f32,
    chrome: Pixels,
) -> Pixels {
    let total = (viewport.height * ratio).min(preferred);
    (total - chrome).max(px(MIN_DIALOG_BODY))
}

/// [`clamp_dialog_body_height`] 的下限。
const MIN_DIALOG_BODY: f32 = 120.0;

// ─── 设置面板的通用原语 ───────────────────────────────────────
//
// 逐条对应 `src/components/SettingsModal.tsx:52-256` 那批组件(动机见它的注释:
// 同一个 toggle 的 15 行 JSX 曾复制十来份)。**全部自绘**,不用
// `gpui_component::switch` / `setting`:前者的配色走组件库自己的 theme token,
// 与这里的 [`Palette`] 对不上;后者是一整套带 reset 按钮 + rust-i18n 的设置框架,
// 与原版「两级侧栏 + 自定义行」不同形,硬套只会打架(见批次规格 §7 坑 4)。

/// 设置页的分节标题(`SettingsModal.tsx:73-80` 的 `Section` 标题行)。
///
/// **与 [`section_title`] 不是一回事**:那个是用量面板那种「竖条 + 文字」,
/// 这个是大写 + 字距的灰色小标题。
pub fn settings_section_title(text: impl Into<SharedString>) -> Div {
    div()
        .mb(px(2.0))
        .text_size(font_px(12.0))
        .text_color(text_muted())
        // 原版是 `uppercase tracking-[0.1em]`;gpui 没有 text-transform,
        // 中文文案本来也没有大小写,只把字距做出来
        .child(text.into())
}

/// 分节末尾的补充说明(`SettingsModal.tsx:83-85` 的 `Hint`)。
pub fn hint(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(font_px(11.0))
        .text_color(text_muted())
        .child(text.into())
}

/// 设置行里的说明文字(`text-sm text-[var(--text-muted)]`)。
pub fn desc_text(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(font_px(11.0))
        .text_color(text_muted())
        .child(text.into())
}

/// 设置行/卡片的外壳:`px-3 py-2.5 rounded-md bg-base border-subtle`。
pub fn settings_card() -> Div {
    div()
        .px(px(12.0))
        .py(px(10.0))
        .rounded(px(6.0))
        .bg(bg_base())
        .border_1()
        .border_color(border_subtle())
}

/// 一行设置:左标题 + 说明,右控件(`SettingsModal.tsx:88-113` 的 `SettingRow`)。
///
/// `disabled` = 从属项的总开关关着 —— 原版是 `opacity-50 pointer-events-none`,
/// 这里同样只压透明度,交互由调用方**不挂 on_click** 来断(gpui 没有
/// pointer-events 这个属性)。
pub fn setting_row(
    title: impl Into<SharedString>,
    desc: Option<AnyElement>,
    disabled: bool,
    control: impl IntoElement,
) -> Div {
    settings_card()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .when(disabled, |el| el.opacity(0.5))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(font_px(13.0))
                        .text_color(text_primary())
                        .child(title.into()),
                )
                .children(desc),
        )
        .child(div().flex_none().child(control))
}

/// 开关(`SettingsModal.tsx:115-142` 的 `Toggle`)。
///
/// 几何逐值照抄:`w-9 h-5`(36×20)的圆角胶囊 + `w-4 h-4`(16)的白滑块,
/// 开时 `translate-x-[18px]`、关时 `translate-x-0.5`。
/// **不随 `uiFontSize` 缩放** —— 原版这几个值是 Tailwind 的固定尺寸类。
pub fn toggle(id: impl Into<ElementId>, checked: bool) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .w(px(36.0))
        .h(px(20.0))
        .flex_none()
        .rounded_full()
        .cursor_pointer()
        .bg(if checked { accent() } else { border_strong() })
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .left(px(if checked { 18.0 } else { 2.0 }))
                .w(px(16.0))
                .h(px(16.0))
                .rounded_full()
                .bg(white()),
        )
}

/// 复选框(hook 页的注入目标列表,原版是 `<input type=checkbox accent-[var(--accent)]>`)。
pub fn checkbox(id: impl Into<ElementId>, checked: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(14.0))
        .h(px(14.0))
        .rounded(px(3.0))
        .border_1()
        .cursor_pointer()
        .border_color(if checked { accent() } else { border_strong() })
        .when(checked, |el| el.bg(accent()))
        .when(checked, |el| {
            el.child(
                div()
                    .text_size(font_px(10.0))
                    .text_color(bg_base())
                    .child("\u{2713}"),
            )
        })
}

/// 单选段里的一项(`SettingsModal.tsx:229-256` 的 `ChoiceGroup`)。
///
/// 原先还有个 `disabled` 形参,专给「UI 有、底层没有」的内置皮肤那一栏画置灰项;
/// 那一栏已整段移除(见 `settings.rs` 模块注释),形参随之删掉 —— 现存唯一的
/// 单选段(主题)三项都是可点的。
pub fn choice_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .py(px(8.0))
        .rounded(px(4.0))
        .border_1()
        .text_size(font_px(13.0))
        .cursor_pointer()
        .when(selected, |el| {
            el.bg(accent_muted())
                .text_color(accent())
                .border_color(accent())
        })
        .when(!selected, |el| {
            el.bg(bg_base())
                .text_color(text_secondary())
                .border_color(border_default())
        })
        .child(label.into())
}

/// 键帽(`styles.css:494-506` 的 `.kbd`)。
///
/// **自绘而不是 `gpui_component::kbd::Kbd`**:那个要一个真实的
/// [`gpui::Keystroke`],而键位表里有 `1…9` 这种占位串根本解析不出来;
/// 它的配色也走组件库自己的 theme token。`border-bottom-width: 2px`
/// 是那个「键帽」立体感的来源,别漏。
pub fn kbd(text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(bg_elevated())
        .border_1()
        .border_b_2()
        .border_color(border_default())
        .text_size(font_px(11.0))
        .text_color(text_secondary())
        .child(text.into())
}

/// 字号滑块(`SettingsModal.tsx:744-778` 的 `FontSizeSlider`)。
///
/// **拖动即时提交**(原版 `onChange` 直连,没有草稿态)—— 与 [`NumberRow`] 那种
/// 「失焦/回车才归一」的语义相反,别统一。
///
/// # 为什么是分段而不是连续轨道
///
/// 原版是 `<input type=range step=1>`,取值本就是整数档。gpui 里做连续轨道要
/// 先把元素 bounds 回填出来(canvas sink)再做像素→比例换算;而档位只有十来个,
/// 直接铺成 `max-min+1` 段可点/可拖的格子,行为(点哪跳哪、按住拖过去连续变)
/// 与 range 完全一致,还省掉一份每帧回填的几何状态。
///
/// [`NumberRow`]: crate::settings
pub fn font_size_slider(
    id_prefix: &'static str,
    label: impl Into<SharedString>,
    value: i32,
    min: i32,
    max: i32,
    on_change: impl Fn(i32, &mut Window, &mut App) + 'static,
) -> Div {
    let on_change = Rc::new(on_change);
    let mut track = div().flex().flex_1().items_center().gap(px(1.0));
    for step in min..=max {
        let filled = step <= value;
        let cb_click = on_change.clone();
        let cb_move = on_change.clone();
        track = track.child(
            div()
                .id(SharedString::from(format!("{id_prefix}-{step}")))
                .flex_1()
                .h(px(14.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .child(
                    div()
                        .w_full()
                        .h(px(6.0))
                        .bg(if filled { accent() } else { border_strong() }),
                )
                .on_click(move |_, window, cx| cb_click(step, window, cx))
                // 按住左键划过 = 连续拖动(等价于 range 的拖拽)
                .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
                    if event.pressed_button == Some(MouseButton::Left) {
                        cb_move(step, window, cx);
                    }
                }),
        );
    }

    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(font_px(13.0))
                        .text_color(text_primary())
                        .child(label.into()),
                )
                .child(
                    div()
                        .text_size(font_px(13.0))
                        .text_color(accent())
                        .child(format!("{value}px")),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .text_size(font_px(11.0))
                        .text_color(text_muted())
                        .child(min.to_string()),
                )
                .child(track)
                .child(
                    div()
                        .text_size(font_px(11.0))
                        .text_color(text_muted())
                        .child(max.to_string()),
                ),
        )
}

/// 缺一段的圆环 —— 原版 `border ... border-t-transparent rounded-full` 那个
/// spinner 的几何(270° 弧,起点在 12 点、顺时针)。
const SPINNER_ARC: &[Shape] = &[Shape::line(
    Ink::Current,
    // `border`(1px)÷ 12px 直径 ≈ 0.083
    0.083,
    Geom::Arc {
        c: (0.5, 0.5),
        // 描边居中:半径留出半个线宽,否则弧会被裁掉外缘
        r: 0.458,
        from: -90.0,
        sweep: 270.0,
    },
)];

/// 加载中的转圈。**自绘** —— gpui-component 的 `Spinner` 默认图标是
/// `IconName::Loader`,而 0.5.1 不带 svg 资产,转的是个空框(见 `menu` 模块注释)。
///
/// 周期 1s 匀速(Tailwind `animate-spin` 的默认值),相位来自
/// `mt_ui::motion::pulse_phase` 的低频泵 —— **不用** `with_animation(..repeat())`,
/// 那条路每帧请求重绘,一个 20px 的圈就能把整窗钉在满帧率上。
///
/// 减弱动效下**不停,只放慢到 2.4s**(`mt_ui::motion::spin_period`)——
/// 原版 `styles.css:404-413` 专门为「进行中」指示器开了这条豁免:
/// 停住的 spinner 不是安静,是看着像卡死。
pub fn spinner(size: Pixels, color: Hsla) -> impl IntoElement {
    Spinner { size, color }
}

#[derive(IntoElement)]
struct Spinner {
    size: Pixels,
    color: Hsla,
}

impl RenderOnce for Spinner {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let period = mt_ui::motion::spin_period(std::time::Duration::from_secs(1));
        // 相位 0..1 一圈,`VectorIcon::rotation` 的单位也是「圈」
        let phase = mt_ui::motion::pulse_phase(period, window, cx);
        VectorIcon::new(SPINNER_ARC, self.size)
            .ink(self.color)
            .rotation(phase)
    }
}

/// 状态灯的颜色(对齐 `src/components/StatusDot.tsx` 的 `STATUS_COLORS`)。
pub fn status_color(status: PaneStatus) -> Hsla {
    match status {
        PaneStatus::Idle => text_muted(),
        PaneStatus::AiIdle => color_success(),
        PaneStatus::AiWorking => color_ai_working(),
        PaneStatus::Error => color_error(),
    }
}

/// 四态状态灯([`mt_ui::icons::StatusDot`])。
///
/// 形状 + 颜色双编码(空心圈 / 实心带勾 / 底环+亮弧 / 实心带叉),`ai-working`
/// 那段弧 900ms 转一圈 —— 几何与动画都在 mt-ui 侧照抄原版 `StatusDot.tsx`,
/// 这里只做两件事:`PaneStatus → StatusKind` 的转换,以及把壳的配色表喂进去
/// (勾/叉是**挖空**语义,`contrast` 必须给面板底色,换主题包时跟着变)。
///
/// 旋转相位来自进程级墙钟(`mt_ui::motion::pulse_phase`),没有逐元素状态,
/// 所以不需要 id;同状态的多颗灯天然同相。
pub fn status_dot(status: PaneStatus) -> impl IntoElement {
    // PaneStatus 住在 mt-app(tree.rs),mt-ui 不能反向依赖,所以在这里转一次
    let kind = match status {
        PaneStatus::Idle => StatusKind::Idle,
        PaneStatus::AiIdle => StatusKind::AiIdle,
        PaneStatus::AiWorking => StatusKind::AiWorking,
        PaneStatus::Error => StatusKind::Error,
    };
    StatusDot::new(kind)
        .size(px(11.0))
        .color(status_color(status))
        .contrast(bg_elevated())
}

/// 把一行文本按「命中区间」切成 `(片段, 是否命中)` 序列 —— 搜索结果与项目切换器
/// 的关键词高亮共用这一份。
///
/// # 两条与原版不一样的地方,都是有意的
///
/// 1. **区间按 char 计**,不是字节:`mt_project::search` 的 `match_ranges` 就是
///    char 口径(它自己做了 byte→char 换算),TS 侧的 `String.slice` 也是按码元切。
///    直接拿去切 `&str[..]` 会在中文行上 panic。
/// 2. **相邻命中段合并**:原版 `HighlightText` 逐区间各发一个 `<span>`,而每个
///    span 带 `px-[1px]` 内边距 —— 两段紧挨着时会多出 2px 缝。合并之后视觉一致。
///
/// 坏区间(越界 / 逆序 / 与前一段重叠)一律跳过而不是 panic:结果是后端给的,
/// 一条坏区间不该把整行吞掉。
pub fn highlight_runs(text: &str, ranges: &[(usize, usize)]) -> Vec<(String, bool)> {
    let chars: Vec<char> = text.chars().collect();
    let mut runs: Vec<(String, bool)> = Vec::new();
    let mut push = |slice: &[char], hit: bool| {
        if slice.is_empty() {
            return;
        }
        match runs.last_mut() {
            Some((buf, last_hit)) if *last_hit == hit => buf.extend(slice.iter()),
            _ => runs.push((slice.iter().collect(), hit)),
        }
    };

    let mut cursor = 0usize;
    for &(start, end) in ranges {
        let start = start.max(cursor).min(chars.len());
        let end = end.min(chars.len());
        if end <= start {
            continue;
        }
        push(&chars[cursor..start], false);
        push(&chars[start..end], true);
        cursor = end;
    }
    push(&chars[cursor..], false);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 弹窗宽度:够宽时用原版那个定值,窗口比它窄就压回窗口宽
    /// (原版靠 flex-shrink + `overflow-hidden`,gpui 的 `Dialog` 是定值宽,
    /// 不钳制的话按「中心 − 宽/2」定位会两侧一起出界)。
    #[test]
    fn 弹窗宽度按视口钳制() {
        let wide = gpui::size(px(1400.0), px(900.0));
        let narrow = gpui::size(px(600.0), px(900.0));
        assert_eq!(clamp_dialog_width(px(680.0), wide), px(680.0));
        assert_eq!(clamp_dialog_width(px(680.0), narrow), px(600.0));
    }

    /// 比例宽:大屏跟着 60vw 走,窗口小到算不出下限时按下限,再窄就压回窗口宽。
    #[test]
    fn 弹窗宽度按视口比例取值() {
        let huge = gpui::size(px(2560.0), px(1440.0));
        assert_eq!(ratio_dialog_width(px(680.0), 0.6, huge), px(1536.0));
        // 1000*0.6 = 600 < 680 → 下限说了算
        let mid = gpui::size(px(1000.0), px(900.0));
        assert_eq!(ratio_dialog_width(px(680.0), 0.6, mid), px(680.0));
        // 窗口比下限还窄 → 铺满窗口,不出界
        let narrow = gpui::size(px(600.0), px(900.0));
        assert_eq!(ratio_dialog_width(px(680.0), 0.6, narrow), px(600.0));
    }

    /// 弹窗正文高度:`min(80vh, 舒适上限) − 头部`,并留 120px 下限。
    #[test]
    fn 弹窗正文高度按视口钳制() {
        let tall = gpui::size(px(1400.0), px(1200.0));
        // 1200*0.8 = 960 > 640 → 舒适上限封顶,再扣掉头部
        assert_eq!(
            clamp_dialog_body_height(px(640.0), tall, 0.8, px(52.0)),
            px(588.0)
        );
        // 矮窗口:80vh 说了算,面板整体(头部 + 正文)= 0.8vh,顶距 0.1vh → 不出界
        let short = gpui::size(px(1400.0), px(500.0));
        let body = clamp_dialog_body_height(px(640.0), short, 0.8, px(52.0));
        assert_eq!(body, px(348.0));
        assert!(body + px(52.0) + short.height * 0.1 <= short.height);
        // 极端窄高:宁可溢出也不塌成一条缝
        let sliver = gpui::size(px(1400.0), px(120.0));
        assert_eq!(
            clamp_dialog_body_height(px(640.0), sliver, 0.8, px(52.0)),
            px(MIN_DIALOG_BODY)
        );
    }

    #[test]
    fn 高亮切段_无区间时原样一段() {
        assert_eq!(highlight_runs("abc", &[]), vec![("abc".into(), false)]);
        assert!(highlight_runs("", &[]).is_empty());
    }

    #[test]
    fn 高亮切段_首尾与中间() {
        assert_eq!(
            highlight_runs("abcdef", &[(2, 4)]),
            vec![("ab".into(), false), ("cd".into(), true), ("ef".into(), false)]
        );
        // 命中在开头 / 结尾时不产生空段
        assert_eq!(
            highlight_runs("abc", &[(0, 1)]),
            vec![("a".into(), true), ("bc".into(), false)]
        );
        assert_eq!(
            highlight_runs("abc", &[(2, 3)]),
            vec![("ab".into(), false), ("c".into(), true)]
        );
    }

    /// 相邻命中段合并成一段(原版每段带 1px 内边距,不合并会多出缝)。
    #[test]
    fn 高亮切段_相邻命中合并() {
        assert_eq!(
            highlight_runs("abcd", &[(0, 1), (1, 2)]),
            vec![("ab".into(), true), ("cd".into(), false)]
        );
    }

    /// 区间是 **char** 口径:中文行按字节切会 panic,按 char 切才对得上。
    #[test]
    fn 高亮切段_按字符不按字节() {
        assert_eq!(
            highlight_runs("你好世界", &[(1, 3)]),
            vec![("你".into(), false), ("好世".into(), true), ("界".into(), false)]
        );
    }

    /// 缓动曲线:两端钉死、单调不回头、`ease-overlay-in` 前段就冲得很快
    /// (`cubic-bezier(0.16, 1, 0.3, 1)` 是「快出慢收」)。
    #[test]
    fn 三次贝塞尔缓动() {
        let ease_in = cubic_bezier(0.16, 1.0, 0.3, 1.0);
        assert_eq!(ease_in(0.0), 0.0);
        assert_eq!(ease_in(1.0), 1.0);
        assert_eq!(ease_in(-1.0), 0.0, "越界要夹住");
        assert_eq!(ease_in(2.0), 1.0);
        // 单调
        let mut prev = 0.0;
        for i in 0..=20 {
            let v = ease_in(i as f32 / 20.0);
            assert!(v >= prev - 1e-4, "缓动必须单调:{prev} → {v}");
            prev = v;
        }
        // 快出:走到一半时间已经完成大半位移
        assert!(ease_in(0.5) > 0.8, "{}", ease_in(0.5));

        // ease-overlay-out 是「慢出快收」,半程时位移不到一半
        let ease_out = cubic_bezier(0.4, 0.0, 0.9, 0.6);
        assert!(ease_out(0.5) < 0.5, "{}", ease_out(0.5));
        assert_eq!(ease_out(1.0), 1.0);
    }

    /// 坏区间(越界 / 逆序 / 重叠)跳过而不是 panic。
    #[test]
    fn 高亮切段_坏区间不炸() {
        assert_eq!(highlight_runs("abc", &[(5, 9)]), vec![("abc".into(), false)]);
        assert_eq!(highlight_runs("abc", &[(2, 1)]), vec![("abc".into(), false)]);
        // 重叠:后一段被夹到前一段末尾之后,字符不会重复出现
        assert_eq!(
            highlight_runs("abcdef", &[(0, 3), (1, 5)]),
            vec![("abcde".into(), true), ("f".into(), false)]
        );
        // 越界的右端夹到行尾
        assert_eq!(
            highlight_runs("abc", &[(1, 99)]),
            vec![("a".into(), false), ("bc".into(), true)]
        );
    }
}
