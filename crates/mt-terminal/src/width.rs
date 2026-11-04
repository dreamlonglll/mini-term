//! 列宽覆盖:emoji 字位簇(grapheme cluster)整簇落一格。
//!
//! alacritty_terminal 的 `Term::input` 是**逐字符**定列宽的(`unicode_width::width`):
//! 0 宽字符贴到前一格;宽字符各占一格。这套口径在 emoji **序列**上与字体画出来的
//! 东西对不上:`❤️`(❤ + VS16)按字符算是 1 + 0 = 1 列,字形却是 2 列的彩色 emoji;
//! `👍🏽`(👍 + 肤色修饰符)按字符算是 2 + 2 = 4 列、画成两个字形;`👨‍👩‍👧`(ZWJ 序列)
//! 被拆成三个各占 2 列的格子;`🇨🇳`(两个区域指示符)拆成两格各画半面旗。
//!
//! 更要命的是**上游程序不这么算**:ConPTY 1.22+ 的文本缓冲、Claude Code / Ink 的
//! string-width、WezTerm / kitty 全按字位簇给这些序列 2 列。它们做局部重绘时按自己
//! 的列号 CUP 过来,grid 却比它们多出几列,`]` 与空格被吞、👧 跑到后一个词后面
//! (2026-09-19 真机截图)。
//!
//! 这里在 VT 解析器与 `Term` 之间隔一层 [`WidthOverride`],只拦 `input`,其余方法
//! 原样转发。唯一的规则是**簇不拆格**:来一个字符先看它按 UAX #29 是否延续光标前
//! 那一格的字位簇(VS16 / ZWJ 后的 emoji / 肤色修饰符 / 第二个区域指示符 / 组合
//! 符号……),是就以 0 宽字符的身份塞进那一格;整簇宽度(`unicode_width` 的字符串
//! 口径,已认得 emoji 呈现 / 修饰 / ZWJ 序列)从 1 变 2 时把那一格**就地拓宽**。
//!
//! # 单个码点的宽度一律不动
//!
//! 这条只对**多码点序列**生效。`● ⏺ · ✻ ✽ ✶ ✳ ✢ ⎿`(Claude Code 的消息前缀、
//! 工作中的转圈与结果连接线)、制表符 `─│┌┐`、箭头、`①★■` 这些东亚宽度 A 类符号
//! 仍按 alacritty 的 1 列 —— ConPTY 与 Claude Code 都按 1 列给它们定位,grid 一旦
//! 记成 2 列,工作中那行每次局部重绘都会覆盖到占位格,圆点就被抹掉或整行错位。
//! 2026-09-14 的 `forced_wide` 表(①②③ 与 ★☆●○■□ 按 2 列)正是因此整套回滚的,
//! **别再往这里加单码点的宽度表**。
//!
//! # 宽度 ≥ 1 的字符什么时候允许并入前一格
//!
//! UAX #29 的「不断开」比 emoji 需要的宽得多:Prepend 类(阿拉伯数字符号 U+0600 等)
//! 会把后面的**数字**接进同一簇、谚文 L 声母连写也不断开——照单全收就会把一个
//! 本该占一列的可见字符吞成 0 宽,那一列就丢了。所以宽度 ≥ 1 的字符只在三种情形
//! 允许并入([`visible_may_join`]):前一格以 ZWJ 结尾、它是肤色修饰符、它是与前一格
//! 配对的第二个区域指示符。0 宽字符不受此限——alacritty 原生就是贴前格。
//!
//! # 落格手法:借道占位字符
//!
//! `Term::input` 里宽字符那套簿记(行尾放不下时的 LEADING_WIDE_CHAR_SPACER + 折行、
//! INSERT 模式右移、WIDE_CHAR / WIDE_CHAR_SPACER 标志、覆盖旧宽字符时的清理)全是
//! 私有方法,复刻一遍等于 fork。就地拓宽时基字右边那一格经 `Term::input(' ')` 写成
//! spacer(INSERT 模式右移、覆盖旧宽字符等簿记照常发生),再给基字打 WIDE_CHAR;
//! 基字已在最后一列时按 alacritty 的规矩留 LEADING spacer、整簇搬到下一行行首——
//! 那一步先喂一个**确定为 2 列**的占位字符([`PLACEHOLDER`],全角空格)让 alacritty
//! 把簿记做完,再回头把落下的那一格换成真身。落点由喂完后的光标位置反推:正常写完
//! 光标停在宽字符后两列;写到行尾时 alacritty 不推光标而是置 `input_needs_wrap`,
//! 光标停在 spacer 上、宽字符在前一列。
//!
//! # 已知不管的
//!
//! - 簇的字符串宽度 ≥ 3(个别印度系合体、高棉 BEYYAL)封顶按 2 列。
//! - 文本呈现序列(⚡ + VS15)不会**缩**格:只拓不缩。
//! - 肤色修饰符跟在非 emoji 基字后面(`中🏽`)照样并入——UAX #29 就这么断,ConPTY
//!   同款;字形怎么画是字体的事。

use std::sync::Arc;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::index::Column;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};
use unicode_segmentation::GraphemeCursor;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::kitty::{self, KittyShared};

/// 借道用的占位字符:U+3000 全角空格,unicode-width 口径下稳定为 2 列。
const PLACEHOLDER: char = '\u{3000}';

/// 一格里最多攒多少个字符(基字 + 0 宽字符)。
///
/// RGI emoji 序列最长在 10 个码位上下(带肤色的家庭 / 亲吻序列、tag 序列旗帜),
/// 这个上限是给恶意流兜底的:超过就当簇边界另起一格,不让一格无限长。
const MAX_CLUSTER_CHARS: usize = 16;

const ZWJ: char = '\u{200D}';

/// 肤色修饰符 U+1F3FB..=U+1F3FF:UAX #29 里是 Extend,unicode-width 里却是 2 列。
fn is_emoji_modifier(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// 区域指示符 U+1F1E6..=U+1F1FF:两个成对是一面旗。
fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// 整簇的列宽(1 或 2):unicode-width 的字符串口径(认得 emoji 呈现 / 修饰 / ZWJ
/// 序列),封顶 2。
fn cluster_width(cluster: &str) -> usize {
    cluster.width().min(2)
}

/// 宽度 ≥ 1 的字符 `c` 是否允许并入 `cell` 那一格(见模块注释)。
/// 只是白名单门禁,最终还要过 [`extends_cluster`] 的 UAX #29 判定。
fn visible_may_join(cell: &Cell, c: char) -> bool {
    let zerowidth = cell.zerowidth().unwrap_or(&[]);
    zerowidth.last() == Some(&ZWJ)
        || is_emoji_modifier(c)
        || (is_regional_indicator(c) && is_regional_indicator(cell.c) && zerowidth.is_empty())
}

/// `c` 是否延续 `cluster`(光标前那一格里的字位簇)——UAX #29 扩展字位簇口径。
/// 前提:`cluster` 已经带上了 `c`(调用方拼好的),边界位置在 `c` 之前。
fn extends_cluster(cluster: &str, c: char) -> bool {
    let split = cluster.len() - c.len_utf8();
    let mut cursor = GraphemeCursor::new(split, cluster.len(), true);
    !cursor.is_boundary(cluster, 0).unwrap_or(true)
}

/// [`WidthOverride`] 的随身状态:跟解析器同锁、跨批次存活。
#[derive(Default)]
pub(crate) struct Scratch {
    /// 簇文本的暂存,跨字符复用免得每个字符分配一次。
    text: String,
    /// Kitty 图形。`None` = 开关关着,下面那几条序列原样转发。
    kitty: Option<Arc<KittyShared>>,
}

impl Scratch {
    pub(crate) fn set_kitty(&mut self, kitty: Arc<KittyShared>) {
        self.kitty = Some(kitty);
    }
}

/// 套在 `Term` 外面的 `Handler`:只拦 `input`(列宽)与 Kitty 图形关心的三条序列,
/// 其余全部原样转发。
///
/// ⚠️ 升级 `vte` 时对照 `Handler` 的方法清单——新增的方法有默认空实现,
/// 漏转发不会编译报错,只会让那条序列静默失效。
pub(crate) struct WidthOverride<'a, T> {
    term: &'a mut Term<T>,
    /// 簇文本的暂存,跨字符复用免得每个字符分配一次。
    cluster: &'a mut String,
    kitty: Option<&'a KittyShared>,
}

impl<'a, T: EventListener> WidthOverride<'a, T> {
    pub(crate) fn new(term: &'a mut Term<T>, scratch: &'a mut Scratch) -> Self {
        Self {
            term,
            cluster: &mut scratch.text,
            kitty: scratch.kitty.as_deref(),
        }
    }

    /// 光标前那一格——可能是当前字位簇的落点。`None` = 行首,没有前一格。
    ///
    /// 与 alacritty 自己贴 0 宽字符的定位一致:`input_needs_wrap` 时光标还停在
    /// 最后一列那格上;落在宽字符的 spacer 上就再退一格。
    fn anchor(&self) -> Option<Column> {
        let grid = self.term.grid();
        let cursor = &grid.cursor;
        let mut col = cursor.point.column.0;
        if !cursor.input_needs_wrap {
            col = col.checked_sub(1)?;
        }
        if grid[cursor.point.line][Column(col)]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
        {
            col = col.checked_sub(1)?;
        }
        Some(Column(col))
    }

    /// 把 `c` 接进 `col` 那一格的簇;簇宽从 1 变 2 就地拓宽。
    /// 前提:`self.cluster` 已是「旧簇 + c」。
    fn extend(&mut self, col: Column, c: char) {
        let want = cluster_width(self.cluster);
        let grid = self.term.grid_mut();
        let line = grid.cursor.point.line;
        let cell = &mut grid[line][col];
        cell.push_zerowidth(c);
        if want == 2 && !cell.flags.contains(Flags::WIDE_CHAR) {
            self.widen(col);
        }
    }

    /// 把 `col` 那一格从 1 列拓成 2 列。光标此刻要么在它右边一格,要么
    /// (`input_needs_wrap`)就停在它上面——那是它在最后一列的情形。
    fn widen(&mut self, col: Column) {
        let grid = self.term.grid_mut();
        let line = grid.cursor.point.line;
        if grid.cursor.input_needs_wrap {
            // 右边没有位置。折行关着就只能维持窄格(alacritty 对宽字符也是丢字);
            // 开着就照它的规矩:这一格留 LEADING spacer,整簇搬到下一行行首。
            if !self.term.mode().contains(TermMode::LINE_WRAP) {
                return;
            }
            let cell = &mut self.term.grid_mut()[line][col];
            let base = cell.c;
            let zerowidth: Vec<char> = cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default();
            cell.clear_wide(); // c = ' ',清 zerowidth
            cell.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
            self.write_wide(base, &zerowidth);
            return;
        }
        // 光标在基字右边那一格:经 `Term::input` 把它写成 spacer,让 INSERT 模式右移、
        // 覆盖旧宽字符的清理等簿记照常发生,再给基字打 WIDE_CHAR。
        grid.cursor.template.flags.insert(Flags::WIDE_CHAR_SPACER);
        self.term.input(' ');
        let grid = self.term.grid_mut();
        grid.cursor.template.flags.remove(Flags::WIDE_CHAR_SPACER);
        grid[line][col].flags.insert(Flags::WIDE_CHAR);
    }

    /// 以 2 列写下 `base`(带上它的 0 宽字符):借道占位字符,再把落下的那格换成真身。
    fn write_wide(&mut self, base: char, zerowidth: &[char]) {
        let before = self.term.grid().cursor.point;
        self.term.input(PLACEHOLDER);

        let grid = self.term.grid_mut();
        let cursor = &grid.cursor;
        // LINE_WRAP 关着且光标已在最后一列:`Term::input` 一个格子都不写、只置
        // `input_needs_wrap` 就返回(见其源码),此时光标纹丝不动——我们也不动。
        if cursor.point == before {
            return;
        }
        let back = if cursor.input_needs_wrap { 1 } else { 2 };
        let Some(col) = cursor.point.column.0.checked_sub(back) else {
            return;
        };
        let line = cursor.point.line;
        let cell = &mut grid[line][Column(col)];
        // 双重校验:落点必须正是刚写下的占位宽字符,否则宁可留着全角空格也不乱改。
        if cell.c == PLACEHOLDER && cell.flags.contains(Flags::WIDE_CHAR) {
            cell.c = base;
            for &z in zerowidth {
                cell.push_zerowidth(z);
            }
        }
    }
}

/// 一格里的簇文本:基字 + 0 宽字符。
fn cell_cluster(cell: &Cell, out: &mut String) {
    out.clear();
    out.push(cell.c);
    if let Some(zw) = cell.zerowidth() {
        out.extend(zw);
    }
}

macro_rules! forward_to_term {
    ($( $name:ident ( $( $arg:ident : $ty:ty ),* ) ;)*) => {
        $(
            #[inline]
            fn $name(&mut self, $( $arg: $ty ),*) {
                self.term.$name($( $arg ),*)
            }
        )*
    };
}

impl<T: EventListener> Handler for WidthOverride<'_, T> {
    fn input(&mut self, c: char) {
        // ── 快路径:ASCII 永远另起一簇(它既不是 0 宽,也不在可见字符并入的白名单里),
        //    纯英文输出不必碰簇文本。
        if c.is_ascii() {
            self.term.input(c);
            return;
        }
        // ── 看能不能接到光标前那一格的簇上
        if let Some(col) = self.anchor() {
            let line = self.term.grid().cursor.point.line;
            let cell = &self.term.grid()[line][col];
            let zero_width = c.width() == Some(0);
            if zero_width || visible_may_join(cell, c) {
                cell_cluster(cell, self.cluster);
                if self.cluster.chars().count() >= MAX_CLUSTER_CHARS {
                    // 那一格已经满了:0 宽字符直接丢(交给 alacritty 它照样会贴上去),
                    // 其余字符另起一格
                    if zero_width {
                        return;
                    }
                } else {
                    self.cluster.push(c);
                    if extends_cluster(self.cluster, c) {
                        self.extend(col, c);
                        return;
                    }
                }
            }
        }
        // ── 另起一簇。0 宽的孤儿组合符号也走这里:alacritty 原生就是贴前一格
        self.term.input(c);
    }

    /// Kitty 图形的带内标记走这条路到达(见 [`crate::kitty`] 模块注释)。标记不转发:
    /// 它不是链接,也不能动光标模板上的链接状态。对不上号的同 scheme 序列同样到此为止
    /// —— 程序不能借此往格子上挂假锚。
    fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        if let (Some(kitty), Some(link)) = (self.kitty, hyperlink.as_ref())
            && link.uri.starts_with(kitty::SCHEME)
        {
            kitty.on_marker(&link.uri, self.term);
            return;
        }
        self.term.set_hyperlink(hyperlink);
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        if let Some(kitty) = self.kitty
            && matches!(mode, ClearMode::All)
        {
            kitty.on_clear_screen(self.term);
        }
        self.term.clear_screen(mode);
    }

    fn reset_state(&mut self) {
        if let Some(kitty) = self.kitty {
            kitty.on_reset();
        }
        self.term.reset_state();
    }

    forward_to_term! {
        set_title(title: Option<String>);
        set_cursor_style(style: Option<CursorStyle>);
        set_cursor_shape(shape: CursorShape);
        goto(line: i32, col: usize);
        goto_line(line: i32);
        goto_col(col: usize);
        insert_blank(count: usize);
        move_up(rows: usize);
        move_down(rows: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(arg: usize);
        move_forward(cols: usize);
        move_backward(cols: usize);
        move_down_and_cr(rows: usize);
        move_up_and_cr(rows: usize);
        put_tab(count: u16);
        backspace();
        carriage_return();
        linefeed();
        bell();
        substitute();
        newline();
        set_horizontal_tabstop();
        scroll_up(rows: usize);
        scroll_down(rows: usize);
        insert_blank_lines(count: usize);
        delete_lines(count: usize);
        erase_chars(count: usize);
        delete_chars(count: usize);
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(mode: LineClearMode);
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        reverse_index();
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        set_private_mode(mode: PrivateMode);
        unset_private_mode(mode: PrivateMode);
        report_private_mode(mode: PrivateMode);
        set_scrolling_region(top: usize, bottom: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(index: CharsetIndex);
        configure_charset(index: CharsetIndex, charset: StandardCharset);
        set_color(index: usize, color: Rgb);
        dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        reset_color(index: usize);
        clipboard_store(clipboard: u8, base64: &[u8]);
        clipboard_load(clipboard: u8, terminator: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_mouse_cursor_icon(icon: CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(mode: KeyboardModes);
        pop_keyboard_modes(to_pop: u16);
        set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        set_modify_other_keys(mode: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(char_path: ScpCharPath, update_mode: ScpUpdateMode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TermSize, TerminalEmulator};
    use alacritty_terminal::index::{Line, Point};

    /// 某一行的 `(列号, 字符)`,行尾空格裁掉——`visible_columns` 本身不裁。
    fn cols(e: &TerminalEmulator, row: usize) -> Vec<(usize, char)> {
        let mut v = e.visible_columns().swap_remove(row);
        while v.last().is_some_and(|&(_, c)| c == ' ') {
            v.pop();
        }
        v
    }

    /// 某一格的完整簇文本(基字 + 0 宽字符)与是否宽格。
    fn cell_at(e: &TerminalEmulator, row: i32, col: usize) -> (String, bool) {
        e.with_term(|t| {
            let cell = &t.grid()[Line(row)][Column(col)];
            let mut s = String::new();
            cell_cluster(cell, &mut s);
            (s, cell.flags.contains(Flags::WIDE_CHAR))
        })
    }

    /// 占位字符本身必须是 unicode-width 口径下的 2 列,否则整个借道手法不成立。
    #[test]
    fn 占位字符稳定为两列() {
        assert_eq!(PLACEHOLDER.width(), Some(2));
        assert_eq!(PLACEHOLDER.width_cjk(), Some(2));
    }

    // ───────────────────────── 单码点一律不动 ─────────────────────────

    /// Claude Code 界面用的符号(消息前缀 ● ⏺、转圈 · ✢ ✳ ✶ ✻ ✽、结果连接线 ⎿)、
    /// TUI 骨架(制表符、块元素、箭头)、A 类枚举 / 全角符号(① ★ ■):全部仍是 1 列。
    /// 2026-09-15 那次回滚的教训——ConPTY 与 Claude Code 按 1 列定位它们,grid 记成
    /// 2 列会让工作中那行的局部重绘抹掉圆点。
    #[test]
    fn claude_code_与_tui_骨架符号仍是一列() {
        let symbols = "●⏺·✢✳✶✻✽⎿─│┌┐└┘█▁→←↑↓①★■※℃";
        let e = TerminalEmulator::new(TermSize::new(60, 3));
        e.advance(format!("{symbols}x").as_bytes());
        let expected: Vec<(usize, char)> = symbols
            .chars()
            .chain(std::iter::once('x'))
            .enumerate()
            .collect();
        assert_eq!(cols(&e, 0), expected);
        e.with_term(|t| {
            let row = &t.grid()[Line(0)];
            for col in 0..symbols.chars().count() {
                assert!(
                    !row[Column(col)].flags.contains(Flags::WIDE_CHAR),
                    "第 {col} 列被改宽了"
                );
            }
        });
    }

    /// 对照:本来就是 W 的 CJK / emoji 不受影响,连着来也各占各的格。
    #[test]
    fn 宽字符照旧() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("中🎉✅x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '中'), (2, '🎉'), (4, '✅'), (6, 'x')]);
    }

    /// 转发面:随手抽几条经过包装层的非 `input` 序列,确认簿记没被拦掉。
    #[test]
    fn 其它序列原样转发() {
        let e = TerminalEmulator::new(TermSize::new(10, 3));
        e.advance(b"abc\x1b[2D\x1b[1P");
        assert_eq!(e.visible_lines()[0], "ac", "CUB + DCH");
        e.advance(b"\x1b]0;hello\x07");
        assert!(
            e.events().drain().iter().any(|ev| matches!(
                ev,
                alacritty_terminal::event::Event::Title(t) if t == "hello"
            )),
            "OSC 0 标题事件"
        );
    }

    // ───────────────────────── emoji 序列 ─────────────────────────

    /// 呈现序列:❤ 是 1 列的文本符号,跟上 VS16 就是 2 列的 emoji——那一格要就地拓宽。
    #[test]
    fn 呈现序列拓成两列() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("❤\u{FE0F}x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '❤'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("❤\u{FE0F}".into(), true));
        e.with_term(|t| {
            assert!(
                t.grid()[Line(0)][Column(1)]
                    .flags
                    .contains(Flags::WIDE_CHAR_SPACER)
            );
        });
    }

    /// 截图里那几个:⚠️ ▶️ ©️ 同 ❤️ 一样各 2 列。
    #[test]
    fn 常见呈现序列各占两列() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("⚠\u{FE0F}▶\u{FE0F}©\u{FE0F}x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '⚠'), (2, '▶'), (4, '©'), (6, 'x')]);
    }

    /// 肤色修饰符自身是 W 类(2 列),alacritty 会另起一个宽格画成色块;
    /// 它得进 👍 那一格,整簇仍是 2 列。
    #[test]
    fn 肤色修饰符并入基字() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("👍🏽x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '👍'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("👍🏽".into(), true));
    }

    /// ZWJ 序列:三口之家是一格 2 列,不是三格 6 列;四口之家同理。
    #[test]
    fn zwj_序列整簇一格() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("👨\u{200D}👩\u{200D}👧\u{200D}👦x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '👨'), (2, 'x')]);
        assert_eq!(
            cell_at(&e, 0, 0),
            ("👨\u{200D}👩\u{200D}👧\u{200D}👦".into(), true)
        );
    }

    /// ZWJ 后面不是 emoji(UAX #29 GB11 不成立)就照常另起一格。
    #[test]
    fn zwj_后非_emoji_另起一格() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("👨\u{200D}中x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '👨'), (2, '中'), (4, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("👨\u{200D}".into(), true));
    }

    /// 彩虹旗:🏳(1 列的文本符号)+ VS16 + ZWJ + 🌈。前两步把格子拓宽,
    /// 后两步接进同一格。
    #[test]
    fn 彩虹旗整簇一格() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("🏳\u{FE0F}\u{200D}🌈x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '🏳'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("🏳\u{FE0F}\u{200D}🌈".into(), true));
    }

    /// 区域指示符成对成旗:两个各 1 列的字符并成一格 2 列;第三个另起。
    #[test]
    fn 区域指示符成对() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("🇨🇳🇺🇸x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '🇨'), (2, '🇺'), (4, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("🇨🇳".into(), true));
        assert_eq!(cell_at(&e, 0, 2), ("🇺🇸".into(), true));
    }

    /// 键帽序列 `1️⃣`:ASCII 基字 + VS16 + 组合键帽,拓成 2 列。
    #[test]
    fn 键帽序列拓成两列() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("1\u{FE0F}\u{20E3}x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '1'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("1\u{FE0F}\u{20E3}".into(), true));
    }

    /// 普通组合符号(é = e + 锐音符)仍是 1 列;文本呈现序列(⚡ + VS15)不会**缩**格。
    #[test]
    fn 组合符号不拓宽_文本呈现不缩格() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("e\u{301}x⚡\u{FE0E}y".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'e'), (1, 'x'), (2, '⚡'), (4, 'y')]);
        assert_eq!(cell_at(&e, 0, 0), ("e\u{301}".into(), false));
        assert_eq!(cell_at(&e, 0, 2), ("⚡\u{FE0E}".into(), true));
    }

    // ───────────────────────── 可见字符并入的门禁 ─────────────────────────

    /// Prepend 类(阿拉伯数字符号 U+0600,unicode-width 给 1 列)按 UAX #29 会把后面的
    /// 数字接进同一簇,这里不认:阿拉伯-印度数字 ٣ 仍占自己的一列,不会被吞成 0 宽。
    #[test]
    fn prepend_不吞后面的可见字符() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("a\u{600}٣x".as_bytes());
        assert_eq!(
            cols(&e, 0),
            vec![(0, 'a'), (1, '\u{600}'), (2, '٣'), (3, 'x')]
        );
    }

    /// 谚文声母连写(L × L 不断开)也不并:各占各的宽格,与 alacritty 原生一致。
    #[test]
    fn 谚文声母不并格() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("\u{1100}\u{1100}x".as_bytes());
        assert_eq!(
            cols(&e, 0),
            vec![(0, '\u{1100}'), (2, '\u{1100}'), (4, 'x')]
        );
    }

    // ───────────────────────── 行尾 / 模式 / 覆盖 ─────────────────────────

    /// 拓宽时基字恰好在最后一列:这一格变 LEADING spacer,整簇(含 VS16)搬到下一行行首。
    #[test]
    fn 行尾拓宽整簇搬到下一行() {
        let e = TerminalEmulator::new(TermSize::new(4, 3));
        e.advance("abc❤\u{FE0F}x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'a'), (1, 'b'), (2, 'c')]);
        assert_eq!(cols(&e, 1), vec![(0, '❤'), (2, 'x')]);
        assert_eq!(cell_at(&e, 1, 0), ("❤\u{FE0F}".into(), true));
        e.with_term(|t| {
            let tail = &t.grid()[Line(0)][Column(3)];
            assert!(tail.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER));
            assert!(
                tail.zerowidth().is_none_or(<[char]>::is_empty),
                "旧格的 0 宽字符要清干净"
            );
        });
        assert_eq!(e.visible_lines()[1], "❤x", "读回口不出现占位字符");
    }

    /// 拓宽时基字在倒数第二列:spacer 落在最后一列,光标置 `input_needs_wrap`,
    /// 后续字符折到下一行——与直接写一个宽字符到那个位置的结果完全一样。
    #[test]
    fn 倒数第二列拓宽后正常折行() {
        let e = TerminalEmulator::new(TermSize::new(4, 3));
        e.advance("ab❤\u{FE0F}x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'a'), (1, 'b'), (2, '❤')]);
        assert_eq!(cols(&e, 1), vec![(0, 'x')]);
        e.with_term(|t| {
            assert_eq!(t.grid().cursor.point, Point::new(Line(1), Column(1)));
        });
    }

    /// 折行关着、基字在最后一列:拓不了宽,维持窄格(alacritty 对宽字符是直接丢字,
    /// 我们至少把字留住),后续字符按它的规矩丢掉。
    #[test]
    fn 关折行时行尾不拓宽() {
        let e = TerminalEmulator::new(TermSize::new(4, 3));
        e.advance("\x1b[?7labc❤\u{FE0F}".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'a'), (1, 'b'), (2, 'c'), (3, '❤')]);
        assert_eq!(cell_at(&e, 0, 3), ("❤\u{FE0F}".into(), false));
    }

    /// INSERT 模式下拓宽:spacer 那一步也经 `Term::input`,右侧内容再推一列——
    /// 总共推两列,与直接写宽字符一致。
    #[test]
    fn 插入模式下拓宽推两列() {
        let e = TerminalEmulator::new(TermSize::new(10, 3));
        e.advance("abcd\r\x1b[4h❤\u{FE0F}".as_bytes());
        assert_eq!(
            cols(&e, 0),
            vec![(0, '❤'), (2, 'a'), (3, 'b'), (4, 'c'), (5, 'd')]
        );
    }

    /// 拓宽覆盖到右边一个旧宽字符的首格:那个宽字符的 spacer 要被 alacritty 的
    /// 覆盖逻辑清掉,不能留一个孤儿 spacer。
    #[test]
    fn 拓宽覆盖旧宽字符时清其_spacer() {
        let e = TerminalEmulator::new(TermSize::new(10, 3));
        // 先写 "x中",再回到第 1 列写 ❤ + VS16:❤ 覆盖 x 的位置不动,拓宽把 spacer
        // 写到「中」的首格 → 「中」整个被清
        e.advance("x中\x1b[2G❤\u{FE0F}".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'x'), (1, '❤')]);
        e.with_term(|t| {
            let row = &t.grid()[Line(0)];
            assert!(row[Column(2)].flags.contains(Flags::WIDE_CHAR_SPACER));
            assert!(
                !row[Column(3)].flags.contains(Flags::WIDE_CHAR_SPACER),
                "「中」的旧 spacer 已清"
            );
        });
    }

    /// 覆盖写:在整簇宽格的 spacer 列写窄字符,alacritty 会把宽字符一并清掉——
    /// 这是它对 CJK 的既有语义,对拓宽出来的格子同样成立,不能留半个。
    #[test]
    fn 覆盖_spacer_列时宽字符一并清掉() {
        let e = TerminalEmulator::new(TermSize::new(10, 3));
        e.advance("❤\u{FE0F}bc\x1b[2Gx".as_bytes());
        e.with_term(|t| {
            let row = &t.grid()[Line(0)];
            assert!(!row[Column(0)].flags.contains(Flags::WIDE_CHAR));
            assert_eq!(row[Column(0)].c, ' ');
            assert_eq!(row[Column(1)].c, 'x');
        });
    }

    /// 簇长度封顶:超过上限的 0 宽字符另起一格,一格不会被恶意流撑到无限长。
    #[test]
    fn 簇长度封顶() {
        let e = TerminalEmulator::new(TermSize::new(40, 3));
        let mut s = String::from("a");
        for _ in 0..(MAX_CLUSTER_CHARS + 3) {
            s.push('\u{301}');
        }
        s.push('b');
        e.advance(s.as_bytes());
        let (cluster, _) = cell_at(&e, 0, 0);
        assert_eq!(cluster.chars().count(), MAX_CLUSTER_CHARS);
        assert_eq!(cols(&e, 0).last(), Some(&(1, 'b')), "b 仍在第 1 列");
    }

    /// 簇的判定跨 `advance` 批次也成立——PTY 读缓冲在 ZWJ 序列中间切一刀是常事。
    #[test]
    fn 跨批次仍能接簇() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        let bytes = "👨\u{200D}👩\u{200D}👧x".as_bytes();
        for chunk in bytes.chunks(3) {
            e.advance(chunk);
        }
        assert_eq!(cols(&e, 0), vec![(0, '👨'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("👨\u{200D}👩\u{200D}👧".into(), true));
    }

    /// 中文与 emoji 混排的整体对齐:每个 CJK 与每个簇都是 2 列。
    #[test]
    fn 中文与_emoji_混排对齐() {
        let e = TerminalEmulator::new(TermSize::new(40, 3));
        e.advance("中❤\u{FE0F}文👍🏽字🇨🇳x".as_bytes());
        assert_eq!(
            cols(&e, 0),
            vec![
                (0, '中'),
                (2, '❤'),
                (4, '文'),
                (6, '👍'),
                (8, '字'),
                (10, '🇨'),
                (12, 'x')
            ]
        );
    }

    /// 截图里那一行:`[👨‍👩‍👧‍👦]  2 列` 的 `]` 必须落在第 3 列——与 ConPTY / Claude Code
    /// 自己算的列号一致,局部重绘时才不会打架。
    #[test]
    fn 与_conpty_口径对齐() {
        let e = TerminalEmulator::new(TermSize::new(40, 3));
        let expected = vec![(0, '['), (1, '👨'), (3, ']'), (4, ' '), (5, ' '), (6, '2')];
        e.advance("[👨\u{200D}👩\u{200D}👧\u{200D}👦]  2".as_bytes());
        assert_eq!(cols(&e, 0), expected);
        e.advance("\r\n[👍🏽]  2".as_bytes());
        assert_eq!(
            cols(&e, 1),
            vec![(0, '['), (1, '👍'), (3, ']'), (4, ' '), (5, ' '), (6, '2')]
        );
    }
}
