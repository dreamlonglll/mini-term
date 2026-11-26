//! 终端屏幕快照提取（供移动端终端视图等只读观察方使用，不依赖 GPUI 或网络协议）。
//!
//! # 设计要点
//!
//! - **只读、不改桌面尺寸**：桌面端永远持有 PTY 的真实 `(cols, rows)`，这里只拍当前
//!   grid 的只读切面，绝不触发 `resize`；
//! - **保留 `wrapped` 软换行标记**：记录上一行末列是否带 `Flags::WRAPLINE`，让手机端
//!   在「文本重排」模式下能把桌面宽行拼回自然段再按屏宽优雅折行，在「原屏」模式下照原样对齐；
//! - **紧凑样式分段**：同行内相邻样式相同的格子合并为单个 [`ScreenSpan`]，默认色省略，
//!   标准 16 色编码成 `"ansi:0"`..`"ansi:15"`（由展示侧随亮暗主题着色），256 色与真彩色
//!   编码成 `"#rrggbb"`。

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::TerminalEmulator;

/// 单段样式相同的文本。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenSpan {
    pub text: String,
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub bold: bool,
    pub dim: bool,
    pub inverse: bool,
}

/// 快照中的一行。`wrapped` 表示本行是上一行软换行的续行。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenLine {
    pub spans: Vec<ScreenSpan>,
    pub wrapped: bool,
}

/// 终端某一时刻的只读屏幕快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    pub generation: u64,
    pub cols: u16,
    pub rows: u16,
    /// 光标在 [`Self::lines`] 中的行下标（已含前导历史行数偏移）。
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub alt_screen: bool,
    pub lines: Vec<ScreenLine>,
}

#[derive(Clone, PartialEq, Eq, Default)]
struct CellStyle {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    inverse: bool,
}

impl CellStyle {
    fn is_plain_bg(&self) -> bool {
        self.bg.is_none() && !self.inverse
    }
}

/// 将 256 色索引（16..=255）转为标准 xterm RGB。
fn indexed_256_rgb(idx: u8) -> Rgb {
    if idx < 232 {
        // 6×6×6 色立方体
        let v = idx - 16;
        let r = v / 36;
        let g = (v % 36) / 6;
        let b = v % 6;
        let level = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
        Rgb {
            r: level(r),
            g: level(g),
            b: level(b),
        }
    } else {
        // 24 级灰阶（232..=255）
        let gray = 8 + (idx - 232) * 10;
        Rgb {
            r: gray,
            g: gray,
            b: gray,
        }
    }
}

fn format_rgb(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b)
}

/// 将 alacritty 的 `Color` 编码为紧凑串；返回 `(颜色串, 是否强制暗淡)`。
fn encode_color(color: Color, palette: &Colors, is_fg: bool) -> (Option<String>, bool) {
    match color {
        Color::Spec(rgb) => (Some(format_rgb(rgb)), false),
        Color::Indexed(idx) => {
            if let Some(rgb) = palette[idx as usize] {
                (Some(format_rgb(rgb)), false)
            } else if idx < 16 {
                (Some(format!("ansi:{idx}")), false)
            } else {
                (Some(format_rgb(indexed_256_rgb(idx))), false)
            }
        }
        Color::Named(named) => match named {
            NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground => {
                if is_fg {
                    (None, named == NamedColor::DimForeground)
                } else if let Some(rgb) = palette[NamedColor::Foreground] {
                    (Some(format_rgb(rgb)), false)
                } else {
                    (Some("ansi:7".into()), false)
                }
            }
            NamedColor::Background | NamedColor::Cursor => {
                if !is_fg {
                    (None, false)
                } else if let Some(rgb) = palette[NamedColor::Background] {
                    (Some(format_rgb(rgb)), false)
                } else {
                    (Some("ansi:0".into()), false)
                }
            }
            NamedColor::Black => color_slot(palette, 0, false),
            NamedColor::Red => color_slot(palette, 1, false),
            NamedColor::Green => color_slot(palette, 2, false),
            NamedColor::Yellow => color_slot(palette, 3, false),
            NamedColor::Blue => color_slot(palette, 4, false),
            NamedColor::Magenta => color_slot(palette, 5, false),
            NamedColor::Cyan => color_slot(palette, 6, false),
            NamedColor::White => color_slot(palette, 7, false),
            NamedColor::BrightBlack => color_slot(palette, 8, false),
            NamedColor::BrightRed => color_slot(palette, 9, false),
            NamedColor::BrightGreen => color_slot(palette, 10, false),
            NamedColor::BrightYellow => color_slot(palette, 11, false),
            NamedColor::BrightBlue => color_slot(palette, 12, false),
            NamedColor::BrightMagenta => color_slot(palette, 13, false),
            NamedColor::BrightCyan => color_slot(palette, 14, false),
            NamedColor::BrightWhite => color_slot(palette, 15, false),
            NamedColor::DimBlack => color_slot(palette, 0, true),
            NamedColor::DimRed => color_slot(palette, 1, true),
            NamedColor::DimGreen => color_slot(palette, 2, true),
            NamedColor::DimYellow => color_slot(palette, 3, true),
            NamedColor::DimBlue => color_slot(palette, 4, true),
            NamedColor::DimMagenta => color_slot(palette, 5, true),
            NamedColor::DimCyan => color_slot(palette, 6, true),
            NamedColor::DimWhite => color_slot(palette, 7, true),
        },
    }
}

fn color_slot(palette: &Colors, idx: usize, dim: bool) -> (Option<String>, bool) {
    if let Some(rgb) = palette[idx] {
        (Some(format_rgb(rgb)), dim)
    } else {
        (Some(format!("ansi:{idx}")), dim)
    }
}

impl TerminalEmulator {
    /// 提取当前终端屏幕快照（含有限回滚历史与样式分段）。
    ///
    /// - 备用屏（全屏 TUI）仅提取当前视口 `0..rows`，保留所有行（含底栏）；
    /// - 主屏额外附带至多 `max_history_lines` 行最近回滚，并裁掉光标下方纯空白尾行。
    pub fn screen_snapshot(&self, max_history_lines: usize) -> ScreenSnapshot {
        let term = self.term.lock();
        let generation = self.generation();
        let mode = *term.mode();
        let alt_screen = mode.contains(TermMode::ALT_SCREEN);
        let cursor_visible = mode.contains(TermMode::SHOW_CURSOR);
        let cols = term.columns();
        let rows = term.screen_lines();
        let palette = term.colors();
        let grid = term.grid();

        let hist_count = if alt_screen {
            0
        } else {
            term.history_size().min(max_history_lines)
        };
        let total = hist_count + rows;
        let cursor_line_idx = hist_count.saturating_add(grid.cursor.point.line.0.max(0) as usize);
        let cursor_col = grid.cursor.point.column.0.min(cols.saturating_sub(1));

        let mut lines = Vec::with_capacity(total);
        let mut prev_wrapped = false;

        for idx in 0..total {
            let line_num = idx as i32 - hist_count as i32;
            let row = &grid[Line(line_num)];

            // 找行尾最后一个需要保留的列：保留有非空字符或有背景/反显样式的格子
            let mut last_kept: Option<usize> = None;
            for col in (0..cols).rev() {
                let cell = &row[Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let has_bg = cell.bg != Color::Named(NamedColor::Background)
                    || cell.flags.contains(Flags::INVERSE);
                let has_text = cell.c != ' '
                    || cell.zerowidth().is_some_and(|zw| !zw.is_empty());
                if has_text || has_bg {
                    last_kept = Some(col);
                    break;
                }
            }

            let mut spans: Vec<ScreenSpan> = Vec::new();
            if let Some(end_col) = last_kept {
                let mut cur_style: Option<CellStyle> = None;
                let mut cur_text = String::new();

                for col in 0..=end_col {
                    let cell = &row[Column(col)];
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    let (fg, fg_dim) = encode_color(cell.fg, palette, true);
                    let (bg, _) = encode_color(cell.bg, palette, false);
                    let style = CellStyle {
                        fg,
                        bg,
                        bold: cell.flags.contains(Flags::BOLD),
                        dim: cell.flags.contains(Flags::DIM) || fg_dim,
                        inverse: cell.flags.contains(Flags::INVERSE),
                    };
                    if cur_style.as_ref() != Some(&style) {
                        if let Some(prev) = cur_style.take() {
                            spans.push(ScreenSpan {
                                text: std::mem::take(&mut cur_text),
                                fg: prev.fg,
                                bg: prev.bg,
                                bold: prev.bold,
                                dim: prev.dim,
                                inverse: prev.inverse,
                            });
                        }
                        cur_style = Some(style);
                    }
                    cur_text.push(cell.c);
                    if let Some(zw) = cell.zerowidth() {
                        for &ch in zw {
                            cur_text.push(ch);
                        }
                    }
                }
                if let Some(prev) = cur_style {
                    // 仅当末尾全空白且无背景时兜底裁掉（正常 last_kept 已确保不会发生）
                    if !cur_text.is_empty() && (!prev.is_plain_bg() || !cur_text.is_empty()) {
                        spans.push(ScreenSpan {
                            text: cur_text,
                            fg: prev.fg,
                            bg: prev.bg,
                            bold: prev.bold,
                            dim: prev.dim,
                            inverse: prev.inverse,
                        });
                    }
                }
            }

            let this_wraps_next = cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            lines.push(ScreenLine {
                spans,
                wrapped: prev_wrapped,
            });
            prev_wrapped = this_wraps_next;
        }

        // 主屏裁掉光标行以下的纯空白尾行，手机竖屏阅读更紧凑（至少保留到光标行）
        if !alt_screen {
            let keep_min = (cursor_line_idx + 1).min(lines.len());
            while lines.len() > keep_min && lines.last().is_some_and(|l| l.spans.is_empty()) {
                lines.pop();
            }
        }

        ScreenSnapshot {
            generation,
            cols: cols.min(u16::MAX as usize) as u16,
            rows: rows.min(u16::MAX as usize) as u16,
            cursor_row: cursor_line_idx.min(u16::MAX as usize) as u16,
            cursor_col: cursor_col.min(u16::MAX as usize) as u16,
            cursor_visible,
            alt_screen,
            lines,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TermSize;

    #[test]
    fn snapshot_captures_colors_wrapped_lines_and_cursor() {
        let emu = TerminalEmulator::new(TermSize::new(10, 6));
        // 超过 10 列触发软换行，再换一行输出彩色文字
        emu.advance(b"abcdefghijklmno\r\n\x1b[1;32mOK\x1b[0m end");
        let snap = emu.screen_snapshot(50);

        assert_eq!((snap.cols, snap.rows), (10, 6));
        assert!(!snap.alt_screen);
        assert!(snap.cursor_visible);
        // 第 0 行 "abcdefghij"（非续行），第 1 行 "klmno"（wrapped=true），第 2 行彩色 "OK" + " end"
        assert_eq!(snap.lines.len(), 3);
        assert!(!snap.lines[0].wrapped);
        assert_eq!(snap.lines[0].spans[0].text, "abcdefghij");
        assert!(snap.lines[1].wrapped);
        assert_eq!(snap.lines[1].spans[0].text, "klmno");
        assert!(!snap.lines[2].wrapped);
        assert_eq!(snap.lines[2].spans.len(), 2);
        assert_eq!(snap.lines[2].spans[0].text, "OK");
        assert_eq!(snap.lines[2].spans[0].fg.as_deref(), Some("ansi:2"));
        assert!(snap.lines[2].spans[0].bold);
        assert_eq!(snap.lines[2].spans[1].text, " end");
        assert_eq!(snap.lines[2].spans[1].fg, None);
        assert_eq!((snap.cursor_row, snap.cursor_col), (2, 6));
    }

    #[test]
    fn snapshot_handles_history_offset_and_alt_screen() {
        let emu = TerminalEmulator::with_scrollback(TermSize::new(20, 3), 10);
        emu.advance(b"h1\r\nh2\r\nh3\r\nh4\r\nh5");
        let snap = emu.screen_snapshot(2);
        // 历史取最近 2 行 + 视口 3 行 = 5 行，光标在最末行
        assert_eq!(snap.lines.len(), 5);
        assert_eq!(snap.lines[0].spans[0].text, "h1");
        assert_eq!(snap.lines[4].spans[0].text, "h5");
        assert_eq!(snap.cursor_row, 4);

        // 进入备用屏后不包含主屏历史，固定为 rows 行
        emu.advance(b"\x1b[?1049h\x1b[2;1Hmenu item");
        let alt = emu.screen_snapshot(50);
        assert!(alt.alt_screen);
        assert_eq!(alt.lines.len(), 3);
        assert_eq!(alt.lines[1].spans[0].text, "menu item");
        assert_eq!(alt.cursor_row, 1);
    }

    #[test]
    fn snapshot_encodes_256_and_truecolor_rgb() {
        let emu = TerminalEmulator::new(TermSize::new(40, 4));
        emu.advance(b"\x1b[38;2;18;52;86mTC\x1b[38;5;196mRED\x1b[0m");
        let snap = emu.screen_snapshot(10);
        assert_eq!(snap.lines[0].spans[0].fg.as_deref(), Some("#123456"));
        assert_eq!(snap.lines[0].spans[1].fg.as_deref(), Some("#ff0000"));
    }
}
