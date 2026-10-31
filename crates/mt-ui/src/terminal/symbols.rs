//! 内置 Nerd Font 图标字体(issue #86):主字体缺私用区图标时不再画成方块。
//!
//! AI CLI / starship / oh-my-posh 的提示符与状态栏大量输出 Nerd Font 图标
//! (Unicode 私用区),终端字体不是补丁版时它们落在主字体的 `.notdef` 上 ——
//! 一排方块。这里随程序内置一份 `Symbols Nerd Font Mono`(Nerd Fonts 官方的
//! 「只含图标」字体,协议见 `assets/fonts/SymbolsNerdFontMono.LICENSE.md`)。
//!
//! # 为什么不是往回退表里加一个名字
//!
//! 最省事的做法是把它接到 [`TerminalStyle::font_fallbacks`] 末尾 —— Windows 上
//! 行不通:gpui 的 DirectWrite 后端造回退表时只在**系统字体集**里按名找字族
//! (`gpui-pre-windows` `direct_write.rs` 的 `generate_font_fallbacks` 取的是
//! `system_font_collection.GetFontSet()`),经 `add_fonts` 内嵌进来的字体住在另一个
//! custom collection 里,写进回退表也点不到。
//!
//! 而按**主字族**点名时 custom collection 是先查的(`select_and_cache_font`)。
//! 于是换个摆法:字体照常 `add_fonts` 注册,渲染层逐格判出「主字体没有真字形的
//! 私用区字符」,让这一格单独成段、run 的字族直接写成本字体,不经过回退表。
//! 三个平台走的是同一条路。判定见 `element.rs` 的 `classify_glyph`。
//!
//! # 尺寸
//!
//! Mono 版每个字形的步进都是 1em,图标墨迹基本占满 1em 见方;终端一列却只有
//! 0.6em 上下(Cascadia Mono 是 1200/2048)。原样画会压住右边那格大半列,所以按
//! 格子缩,见 [`symbol_font_size`]。竖直方向不用管:gpui 按 run 自己的
//! ascent + descent 在行内居中,本字体的 em 框恰好就是这两者之和
//! (测试 `内置字体每个字形都是_1em_见方` 钉着)。
//!
//! # powerline 分隔符不走字体,直接画
//!
//! 三角 / 半圆 / 斜切这几个分隔符(U+E0B0–E0BF)要**撑满整格**——宽是列宽、高是
//! 行高,才能跟两边段的底色拼成一条斜边。等比缩放的字形做不到:按列宽缩只剩半行高
//! (E2E 实拍 oh-my-posh 提示符两端成了两个小鼓包),按行高放又压出半格。而 Win11 自带的
//! Cascadia Mono(2102.025)恰恰连 E0B0 都没有,默认字体的用户人人撞上。所以这一段改用
//! 路径按格子几何画([`powerline_separator`] / [`paint_separator`]),与 WezTerm、
//! Windows Terminal 的内建字形同一做法。只在接管的格子上这么画 —— 主字体自带这些字形时不动。
//!
//! # 什么时候不接管
//!
//! - 主字体自己有这个字形(主字体就是 Nerd Font 补丁版):照旧并进合并段,一个像素没变;
//! - 用户在字族串里自己配了 Nerd Font 回退、**且本机真装着**:整条不接管,画面与以前
//!   一致 —— 那是用户自己调过的样子。只写了名字没装的不算(见 [`symbol_font`]);
//! - 字体没注册上(`add_fonts` 报错):退回原样,方块照旧,不比以前更糟。

use std::borrow::Cow;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{App, Bounds, Hsla, PathBuilder, Pixels, SharedString, Window, point, px};

use super::theme::TerminalStyle;

/// 内置字体的字族名(name 表 nameID 1)。测试会拿字体文件本身对账。
pub const SYMBOL_FONT_FAMILY: &str = "Symbols Nerd Font Mono";

/// Nerd Fonts v3.5.1 `NerdFontsSymbolsOnly.zip` 里的原文件,一个字节没动。
static FONT: &[u8] = include_bytes!("../../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// 启动时注册一次内置图标字体,返回注册成没成。
///
/// 要早于任何终端出第一帧 —— 晚到也不会画错(接管与否进了帧指纹,注册之后
/// 整屏行缓存作废重建),只是首帧会先闪一下方块。静态字节在 Windows 上走
/// `CreateInMemoryFontFileReference`,不拷贝。重复调用直接返回 `true`。
pub fn install_symbol_font(cx: &App) -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    match cx.text_system().add_fonts(vec![Cow::Borrowed(FONT)]) {
        Ok(()) => {
            INSTALLED.store(true, Ordering::Release);
            true
        }
        Err(err) => {
            eprintln!("[mt-ui] 内置 Nerd Font 图标字体注册失败,终端里的图标仍会画成方块: {err:#}");
            false
        }
    }
}

/// 本帧接不接管私用区图标、接管时用哪份字体。`None` = 不接管
/// (没注册上 / 用户自己在回退串里配了本机装着的 Nerd Font)。
///
/// 「配了」只看名字不够:设置页的占位符里就写着 `'JetBrainsMono Nerd Font'`,
/// 照抄却没装这个字体的用户,回退表点不到它,不接管的话图标照旧是方块。所以名字
/// 之外还要核对本机字体名单 —— 名单只在字族串里真出现了 Nerd Font 名字时才去枚举,
/// 整个进程只枚举一次(运行中新装的字体重启后才算数)。
pub(crate) fn symbol_font(style: &TerminalStyle, window: &Window) -> Option<gpui::Font> {
    if !INSTALLED.load(Ordering::Acquire) {
        return None;
    }
    static SYSTEM_FAMILIES: OnceLock<Vec<String>> = OnceLock::new();
    let user_nerd_fallback = nerd_fallback_installed(&style.font_fallbacks, || {
        SYSTEM_FAMILIES
            .get_or_init(|| window.text_system().all_font_names())
            .as_slice()
    });
    if user_nerd_fallback {
        return None;
    }
    // 粗 / 斜一律不套:字体只有 Regular 一张字面,图标也不需要加粗。
    // 缓存一份是为了不在每帧每个 pane 上重新分配 FontFeatures 那个 Arc。
    thread_local! {
        static FONT_SPEC: gpui::Font = gpui::font(SYMBOL_FONT_FAMILY);
    }
    Some(FONT_SPEC.with(Clone::clone))
}

/// Unicode 私用区:BMP 那段(U+E000–U+F8FF)与两个补充私用区平面。
///
/// Nerd Fonts 的图标几乎全在这里(Material Design 那 7000 多枚在补充私用区 A,
/// U+F0001 起)。它另有几个非私用区码位(⏻ ♥ ⚡ 之类),那些是正经 Unicode 符号,
/// 系统符号字体本来就有,不归这里管。
pub(crate) fn is_private_use(ch: char) -> bool {
    matches!(ch as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

/// 回退串里有没有本机装着的 Nerd Font。`installed` 是本机字族名单,只在串里
/// 真出现了 Nerd Font 名字时才会被调用(枚举系统字体不便宜)。
///
/// 内置字体自己的名字不算:`all_font_names` 会把 `add_fonts` 进来的字体也列上,
/// 可回退表只认系统字体集,写了它也点不到 —— 不如照常接管,画出来是同一份字形。
fn nerd_fallback_installed<'a>(
    fallbacks: &[SharedString],
    installed: impl FnOnce() -> &'a [String],
) -> bool {
    let mut named = fallbacks
        .iter()
        .filter(|f| is_nerd_family(f) && !f.eq_ignore_ascii_case(SYMBOL_FONT_FAMILY))
        .peekable();
    if named.peek().is_none() {
        return false;
    }
    let installed = installed();
    named.any(|f| installed.iter().any(|name| name.eq_ignore_ascii_case(f)))
}

/// 字族名像不像 Nerd Font:`… Nerd Font` / `… Nerd Font Mono`,或缩写
/// `… NF` / `… NFM` / `… NFP`(含 `Maple Mono NF CN` 这种缩写后面还跟词的)。
///
/// 每帧每个 pane 都要对回退串过一遍,所以不分配:按字节忽略大小写比。
fn is_nerd_family(name: &str) -> bool {
    const NERD_FONT: &[u8] = b"nerd font";
    name.as_bytes()
        .windows(NERD_FONT.len())
        .any(|w| w.eq_ignore_ascii_case(NERD_FONT))
        || name.split_whitespace().any(|word| {
            ["nf", "nfm", "nfp"]
                .iter()
                .any(|abbr| word.eq_ignore_ascii_case(abbr))
        })
}

/// powerline 分隔符的几何。坐标是格子内的归一化坐标:x 向右、y 向下,0..1。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Separator {
    /// 实心多边形(顶点依次相连并闭合)。
    Solid(&'static [(f32, f32)]),
    /// 细线折线(不闭合)。
    Thin(&'static [(f32, f32)]),
    /// 实心半椭圆:平边贴在左(`flat_left`)或右格边,弧顶碰到对边。
    HalfDisc { flat_left: bool },
    /// 半椭圆弧线,朝向同上。
    HalfRing { flat_left: bool },
}

/// 码位 → 分隔符几何;不在这一段(或不是纯几何形状)的返回 `None`,照常用图标字体画。
///
/// 朝向逐个对过内置字体的轮廓点(字体坐标 y 向上,这里已翻成 y 向下):
/// E0B8 ◣、E0BA ◢、E0BC ◤、E0BE ◥;E0B9 / E0BF 是 `\`,E0BB / E0BD 是 `/`。
pub(crate) fn powerline_separator(ch: char) -> Option<Separator> {
    use Separator::*;
    Some(match ch as u32 {
        0xE0B0 => Solid(&[(0.0, 0.0), (1.0, 0.5), (0.0, 1.0)]),
        0xE0B1 => Thin(&[(0.0, 0.0), (1.0, 0.5), (0.0, 1.0)]),
        0xE0B2 => Solid(&[(1.0, 0.0), (0.0, 0.5), (1.0, 1.0)]),
        0xE0B3 => Thin(&[(1.0, 0.0), (0.0, 0.5), (1.0, 1.0)]),
        0xE0B4 => HalfDisc { flat_left: true },
        0xE0B5 => HalfRing { flat_left: true },
        0xE0B6 => HalfDisc { flat_left: false },
        0xE0B7 => HalfRing { flat_left: false },
        0xE0B8 => Solid(&[(0.0, 0.0), (1.0, 1.0), (0.0, 1.0)]),
        0xE0B9 | 0xE0BF => Thin(&[(0.0, 0.0), (1.0, 1.0)]),
        0xE0BA => Solid(&[(1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]),
        0xE0BB | 0xE0BD => Thin(&[(0.0, 1.0), (1.0, 0.0)]),
        0xE0BC => Solid(&[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)]),
        0xE0BE => Solid(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]),
        _ => return None,
    })
}

/// 实心形状横向各多铺这么宽:平边与隔壁段的底色块严丝合缝,两边各自抗锯齿
/// 不会叠出一道透底的发丝缝。
const SEAM_OVERLAP: f32 = 0.5;

/// 细线分隔符的线宽:14px 字号时 1px,随字号等比变粗。
pub(crate) fn separator_stroke(font_size: Pixels) -> Pixels {
    px((f32::from(font_size) / 14.0).max(1.0))
}

/// 在 `cell`(窗口坐标下的一整格)里画一个分隔符。
pub(crate) fn paint_separator(
    window: &mut Window,
    separator: Separator,
    cell: Bounds<Pixels>,
    color: Hsla,
    stroke: Pixels,
) {
    let (w, h) = (f32::from(cell.size.width), f32::from(cell.size.height));
    let at = |x: f32, y: f32| point(cell.origin.x + px(x), cell.origin.y + px(y));
    // 细线收进半个线宽,线条不出格(否则压到上下两行)
    let half = f32::from(stroke) / 2.0;
    let path = match separator {
        Separator::Solid(points) => {
            let mut builder = PathBuilder::fill();
            for (i, &(x, y)) in points.iter().enumerate() {
                let p = at(-SEAM_OVERLAP + x * (w + 2.0 * SEAM_OVERLAP), y * h);
                if i == 0 {
                    builder.move_to(p);
                } else {
                    builder.line_to(p);
                }
            }
            builder.close();
            builder.build()
        }
        Separator::Thin(points) => {
            let mut builder = PathBuilder::stroke(stroke);
            for (i, &(x, y)) in points.iter().enumerate() {
                let p = at(half + x * (w - 2.0 * half), half + y * (h - 2.0 * half));
                if i == 0 {
                    builder.move_to(p);
                } else {
                    builder.line_to(p);
                }
            }
            builder.build()
        }
        // 弧从平边的上角走到下角:平边在左时经右侧(屏幕坐标 y 向下,顺时针 = sweep)
        Separator::HalfDisc { flat_left } => {
            let (flat, round) = if flat_left {
                (-SEAM_OVERLAP, w + SEAM_OVERLAP)
            } else {
                (w + SEAM_OVERLAP, -SEAM_OVERLAP)
            };
            let mut builder = PathBuilder::fill();
            builder.move_to(at(flat, 0.0));
            let radii = point(px((round - flat).abs()), px(h / 2.0));
            builder.arc_to(radii, px(0.0), false, flat_left, at(flat, h));
            builder.close();
            builder.build()
        }
        Separator::HalfRing { flat_left } => {
            let (flat, round) = if flat_left {
                (0.0, w - half)
            } else {
                (w, half)
            };
            let mut builder = PathBuilder::stroke(stroke);
            builder.move_to(at(flat, half));
            let radii = point(px((round - flat).abs()), px(h / 2.0 - half));
            builder.arc_to(radii, px(0.0), false, flat_left, at(flat, h - half));
            builder.build()
        }
    };
    if let Ok(path) = path {
        window.paint_path(path, color);
    }
}

/// 图标格的字号 —— 字形是 1em 见方,字号就是它画出来的边长。
///
/// 三道上限取最小:不超过 `cols` 列宽(不压隔壁)、不超过行高(不撑破行)、
/// 不超过正文字号(只缩不放,比正文还大的图标在一行字里很扎眼)。`cols` 由调用方按
/// 「后面是不是紧跟空格」给 1 或 2。
pub(crate) fn symbol_font_size(
    font_size: Pixels,
    cell_width: Pixels,
    line_height: Pixels,
    cols: usize,
) -> Pixels {
    let edge = f32::from(font_size)
        .min(f32::from(cell_width) * cols as f32)
        .min(f32::from(line_height));
    px(edge.max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be16(d: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([d[at], d[at + 1]])
    }

    fn be32(d: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
    }

    /// sfnt 表目录里某张表的起点。
    fn table(tag: &[u8; 4]) -> usize {
        let count = be16(FONT, 4) as usize;
        (0..count)
            .map(|i| 12 + 16 * i)
            .find(|&rec| &FONT[rec..rec + 4] == tag)
            .map(|rec| be32(FONT, rec + 8) as usize)
            .unwrap_or_else(|| panic!("字体里缺 {} 表", String::from_utf8_lossy(tag)))
    }

    /// cmap 的 format 12 子表(全平面映射)里有没有这个码位。
    fn covers(cp: u32) -> bool {
        let cmap = table(b"cmap");
        let subtables = be16(FONT, cmap + 2) as usize;
        (0..subtables)
            .map(|i| cmap + be32(FONT, cmap + 4 + 8 * i + 4) as usize)
            .filter(|&sub| be16(FONT, sub) == 12)
            .any(|sub| {
                let groups = be32(FONT, sub + 12) as usize;
                (0..groups)
                    .map(|g| sub + 16 + 12 * g)
                    .any(|group| (be32(FONT, group)..=be32(FONT, group + 4)).contains(&cp))
            })
    }

    /// 渲染层按 [`SYMBOL_FONT_FAMILY`] 点名取字体,名字对不上就会静默落到系统
    /// UI 字体上 —— 换字体版本时这条先喊。
    #[test]
    fn 内置字体的字族名对得上() {
        let name = table(b"name");
        let count = be16(FONT, name + 2) as usize;
        let strings = name + be16(FONT, name + 4) as usize;
        let family = (0..count)
            .map(|i| name + 6 + 12 * i)
            // Windows 平台(3)的 nameID 1,UTF-16BE
            .find(|&rec| be16(FONT, rec) == 3 && be16(FONT, rec + 6) == 1)
            .map(|rec| {
                let len = be16(FONT, rec + 8) as usize;
                let start = strings + be16(FONT, rec + 10) as usize;
                let units: Vec<u16> = FONT[start..start + len]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_be_bytes(*pair))
                    .collect();
                String::from_utf16(&units).expect("字族名不是合法 UTF-16")
            })
            .expect("name 表里没有 Windows 平台的字族名");
        assert_eq!(family, SYMBOL_FONT_FAMILY);
    }

    /// [`symbol_font_size`] 「字号 = 图标边长」的前提:每个字形步进 1em,
    /// em 框高度(ascender − descender)也是 1em。换成非 Mono 版或别家字体,
    /// 这条会先于画面喊出来。
    #[test]
    fn 内置字体每个字形都是_1em_见方() {
        let upem = be16(FONT, table(b"head") + 18);
        let hhea = table(b"hhea");
        let ascender = be16(FONT, hhea + 4) as i16 as i32;
        let descender = be16(FONT, hhea + 6) as i16 as i32;
        assert_eq!(ascender - descender, upem as i32, "em 框高度不是 1em");
        let metrics = be16(FONT, hhea + 34) as usize;
        let hmtx = table(b"hmtx");
        for i in 0..metrics {
            let advance = be16(FONT, hmtx + 4 * i);
            assert!(
                advance == 0 || advance == upem,
                "第 {i} 个步进 {advance} ≠ 1em({upem})"
            );
        }
        // numberOfHMetrics 之后的字形沿用最后一个步进,它必须是 1em
        assert_eq!(be16(FONT, hmtx + 4 * (metrics - 1)), upem);
    }

    /// 各家图标集各抽一枚,全在私用区、全被内置字体覆盖。Material Design 那枚
    /// 在补充私用区 A —— [`is_private_use`] 漏了这个平面,七千多枚图标就还是方块。
    #[test]
    fn 内置字体覆盖各家图标集() {
        for (cp, what) in [
            (0xE0A0, "Powerline 分支"),
            (0xE0B4, "Powerline Extra 半圆"),
            (0xE5FA, "Seti-UI"),
            (0xE718, "Devicons nodejs"),
            (0xEA60, "Codicons"),
            (0xF07B, "Font Awesome 文件夹"),
            (0xF418, "Octicons 分支"),
            (0xF0001, "Material Design"),
        ] {
            assert!(covers(cp), "{what} U+{cp:04X} 不在字体里");
            let ch = char::from_u32(cp).unwrap();
            assert!(is_private_use(ch), "{what} U+{cp:04X} 没被认成私用区");
        }
        // 边界:私用区两端算;U+F900(紧接其后的 CJK 兼容表意字)与 U+FFFFE(非字符)不算
        for ch in ['\u{E000}', '\u{F8FF}', '\u{FFFFD}', '\u{10FFFD}'] {
            assert!(is_private_use(ch), "{ch:?} 是私用区");
        }
        for ch in ['a', '中', '●', '⏻', '\u{D7FF}', '\u{F900}', '\u{FFFFE}'] {
            assert!(!is_private_use(ch), "{ch:?} 不是私用区");
        }
    }

    #[test]
    fn 认得出_nerd_font_字族名() {
        for name in [
            "JetBrainsMono Nerd Font",
            "JetBrainsMono Nerd Font Mono",
            "CaskaydiaCove NF",
            "JetBrainsMono NFM",
            "Hack NFP",
            "Maple Mono NF CN",
            "Symbols Nerd Font",
        ] {
            assert!(is_nerd_family(name), "{name}");
        }
        for name in [
            "Cascadia Mono",
            "Consolas",
            "Microsoft YaHei",
            "Segoe UI Emoji",
            "Noto Sans CJK SC",
            "Inconsolata",
            "SF Mono",
        ] {
            assert!(!is_nerd_family(name), "{name}");
        }
    }

    #[test]
    fn 图标字号只缩不放() {
        // Cascadia Mono 14px:列宽约 8.2,行高 1.3 倍取整 18
        let (font_size, cell_width, line_height) = (px(14.0), px(8.2), px(18.0));
        assert_eq!(
            symbol_font_size(font_size, cell_width, line_height, 1),
            px(8.2),
            "一列:边长就是列宽"
        );
        assert_eq!(
            symbol_font_size(font_size, cell_width, line_height, 2),
            px(14.0),
            "两列放得下 1em 时也不超过正文字号"
        );
        // 行高压得比字号还紧(行高倍数 < 1):不能把行撑破
        assert_eq!(
            symbol_font_size(font_size, cell_width, px(12.0), 2),
            px(12.0)
        );
    }

    fn fallbacks(names: &[&'static str]) -> Vec<SharedString> {
        names.iter().map(|n| SharedString::from(*n)).collect()
    }

    /// 只写了名字、本机没装的 Nerd Font 不算「用户自己配了」—— 设置页占位符里那串
    /// 照抄进来就是这个形状,不接管的话图标照旧是方块(E2E 发现)。
    #[test]
    fn 回退串里的_nerd_font_要本机真装着才不接管() {
        let installed = vec![
            "Consolas".to_string(),
            "firacode nerd font mono".to_string(),
        ];
        let list = fallbacks(&["Consolas", "FiraCode Nerd Font Mono", "Microsoft YaHei"]);
        assert!(
            nerd_fallback_installed(&list, || installed.as_slice()),
            "装着(大小写不敏感)"
        );

        let list = fallbacks(&["JetBrainsMono Nerd Font", "Microsoft YaHei"]);
        assert!(
            !nerd_fallback_installed(&list, || installed.as_slice()),
            "只写了名字没装"
        );

        // 内置字体自己的名字:名单里有(add_fonts 进去的)也不算,回退表点不到它
        let with_builtin = vec![SYMBOL_FONT_FAMILY.to_string()];
        let list = fallbacks(&[SYMBOL_FONT_FAMILY]);
        assert!(!nerd_fallback_installed(&list, || with_builtin.as_slice()));
    }

    /// 回退串里根本没有 Nerd Font 名字时不去枚举系统字体(那一下不便宜)。
    #[test]
    fn 没有_nerd_font_名字就不枚举系统字体() {
        let list = fallbacks(&["Consolas", "Microsoft YaHei", "Segoe UI Emoji"]);
        assert!(!nerd_fallback_installed(&list, || panic!(
            "不该枚举系统字体"
        )));
    }

    /// 接管画法的分界:E0B0–E0BF 这 16 个分隔符一个不漏地走路径,之外的(图标、
    /// 火焰 / 像素块这类非几何形状)仍交给图标字体。实心几何的顶点必须落在格子里。
    #[test]
    fn powerline_分隔符走路径画() {
        for cp in 0xE0B0..=0xE0BF {
            let ch = char::from_u32(cp).unwrap();
            let separator = powerline_separator(ch).unwrap_or_else(|| panic!("U+{cp:04X} 漏了"));
            if let Separator::Solid(points) | Separator::Thin(points) = separator {
                assert!(points.len() >= 2, "U+{cp:04X}");
                for &(x, y) in points {
                    assert!(
                        (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
                        "U+{cp:04X}"
                    );
                }
            }
        }
        for ch in ['\u{E0A0}', '\u{E0AF}', '\u{E0C0}', '\u{F07B}', 'a'] {
            assert_eq!(powerline_separator(ch), None, "{ch:?}");
        }
        // 右三角的尖朝右、左三角的尖朝左:对着字体轮廓核过,别画反
        assert_eq!(
            powerline_separator('\u{E0B0}'),
            Some(Separator::Solid(&[(0.0, 0.0), (1.0, 0.5), (0.0, 1.0)]))
        );
        assert_eq!(
            powerline_separator('\u{E0B6}'),
            Some(Separator::HalfDisc { flat_left: false })
        );
    }

    #[test]
    fn 细线线宽随字号变粗但不细于一像素() {
        assert_eq!(separator_stroke(px(14.0)), px(1.0));
        assert_eq!(separator_stroke(px(10.0)), px(1.0));
        assert_eq!(separator_stroke(px(28.0)), px(2.0));
    }
}
