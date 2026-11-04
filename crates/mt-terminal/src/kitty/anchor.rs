//! 行锚:让图片跟着 grid 的格子走,不 fork alacritty。
//!
//! alacritty 的 grid 没有行身份,也没有滚动钩子。这里在摆放点那一格挂一条私有 scheme
//! 的 hyperlink(存在 `Cell::extra` 里):格子滚进回看、被重排挪了位置、随行被淘汰,
//! 锚都跟着走或一起消失,绘制时扫出来就知道图片此刻在哪。
//!
//! # 空白格要多打一个标记
//!
//! `Cell::is_empty` 不看 hyperlink,只挂链接的空白格算「空」:重排缩窄时
//! `Row::shrink` 把行尾的空格子整段丢掉,锚就没了;`clear_viewport` 算滚动行数时也不算
//! 它。所以空白格的前景色换成一个不是默认值的命名色([`ANCHOR_FG`]):`is_empty` 就此
//! 为假,而空格没有字形,画出来没有任何差别。不用 0 宽字符占位,那会进选区复制与 shaping。
//!
//! # 实测结论(见下面的用例)
//!
//! | 场景 | 结果 |
//! |------|------|
//! | 滚动、进回看、`display_offset` 变化 | 跟着走 |
//! | 回看行数到顶被淘汰 / `CSI 3J` / RIS | 随行消失 |
//! | `CSI K` / `CSI X` / `CSI L` `M` 碾过、覆盖写 | 碾到那一格才消失 |
//! | 备用屏切换 | 锚属于各自的 grid;进备用屏时备用屏清空,主屏的锚留着 |
//! | 列宽 / 行数变化重排 | 跟着格子走,缩到比锚的列还窄也不丢 |
//! | `CSI 2J`(主屏) | 行被顶进回看,锚跟着走 —— 图片的下半截会压在新屏幕上,所以 [`super`] 在这里按协议删掉屏上的摆放 |
//!
//! 只有一格当锚,所以覆盖写那一格图片就没了。协议本身不因覆盖写删图,但 pi 不会写那一格
//! (图片行只有转义序列,改动时先 `a=d` 再重发),记在已知限制里。

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, GridCell as _};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Hyperlink;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// 私有 scheme。完整形态 `mt-kitty:<nonce>:<序号>`,见 [`super`] 模块注释。
pub(crate) const SCHEME: &str = "mt-kitty:";

/// 空白锚点格的前景色,让 `Cell::is_empty` 为假。空格没有字形,选哪个命名色都看不出来。
const ANCHOR_FG: Color = Color::Named(NamedColor::BrightForeground);

/// 把序号为 `seq` 的锚挂到光标那一格。格子原有的内容、颜色、属性都不动。
pub(super) fn stamp<T: EventListener>(term: &mut Term<T>, uri: &str) {
    let grid = term.grid_mut();
    let point = grid.cursor.point;
    let cell = &mut grid[point.line][point.column];
    cell.set_hyperlink(Some(Hyperlink::new(Some(uri), uri.to_string())));
    if cell.is_empty() {
        cell.fg = ANCHOR_FG;
    }
}

/// 从锚的 URI 里取序号。不是锚 = `None`。
///
/// 这里不用再核 nonce:程序自己发的 OSC 8 在 `set_hyperlink` 那一步就被拦了,落不到
/// 格子上(开关打开时),能在格子上看到这个 scheme 的只可能是 [`stamp`] 挂的。
pub(super) fn parse_seq(uri: &str) -> Option<u64> {
    let rest = uri.strip_prefix(SCHEME)?;
    rest.rsplit(':').next()?.parse().ok()
}

/// 依次访问 `[top, bottom)` 这些行(grid `Line` 坐标,负数是回看)上的锚。
/// 越界的行会被收回合法区间。
pub(super) fn for_each<T: EventListener>(
    term: &Term<T>,
    top: i32,
    bottom: i32,
    mut visit: impl FnMut(u64, i32, usize),
) {
    let grid = term.grid();
    let top = top.max(-(grid.history_size() as i32));
    let bottom = bottom.min(grid.screen_lines() as i32);
    let columns = grid.columns();
    for line in top..bottom {
        let row = &grid[Line(line)];
        for column in 0..columns.min(row.len()) {
            let cell = &row[Column(column)];
            if cell.extra.is_none() {
                continue;
            }
            let Some(link) = cell.hyperlink() else {
                continue;
            };
            if let Some(seq) = parse_seq(link.uri()) {
                visit(seq, line, column);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TermSize, TerminalEmulator};

    fn uri(seq: u64) -> String {
        format!("{SCHEME}00000000:{seq}")
    }

    fn stamp_here(e: &TerminalEmulator, seq: u64) {
        e.with_term_mut(|term| stamp(term, &uri(seq)));
    }

    /// 把回看与可视区整个扫一遍:`(序号, 行, 列)`。
    fn anchors(e: &TerminalEmulator) -> Vec<(u64, i32, usize)> {
        let mut found = Vec::new();
        e.with_term(|term| {
            for_each(term, i32::MIN / 2, i32::MAX / 2, |seq, line, column| {
                found.push((seq, line, column));
            });
        });
        found
    }

    fn feed_lines(e: &TerminalEmulator, n: usize) {
        for i in 0..n {
            e.advance(format!("line{i}\r\n").as_bytes());
        }
    }

    #[test]
    fn 序号解析只认私有_scheme() {
        assert_eq!(parse_seq(&uri(42)), Some(42));
        assert_eq!(parse_seq("https://example.com/42"), None);
        assert_eq!(parse_seq("mt-kitty:zz:not-a-number"), None);
    }

    /// 锚不改变格子的文字,读回口与选区复制都看不出来。
    #[test]
    fn 挂锚不改变文字() {
        let e = TerminalEmulator::new(TermSize::new(20, 4));
        e.advance(b"abc");
        stamp_here(&e, 1); // 光标在第 3 列的空白格上
        e.advance(b"\x1b[1;2H");
        stamp_here(&e, 2); // 第 1 列的 b 上
        assert_eq!(e.visible_lines()[0], "abc");
        e.with_term(|term| {
            let row = &term.grid()[Line(0)];
            assert_eq!(row[Column(1)].c, 'b');
            assert_eq!(
                row[Column(1)].fg,
                Color::Named(NamedColor::Foreground),
                "有字的格子不动前景色"
            );
            assert_eq!(row[Column(3)].fg, ANCHOR_FG);
        });
        assert_eq!(anchors(&e), vec![(2, 0, 1), (1, 0, 3)]);
    }

    /// 内容往上滚、进回看,锚跟着走。行号就是 grid 的 `Line`,回看里是负数。
    #[test]
    fn 滚动时锚跟着走() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 5), 100);
        e.advance(b"\r\n\r\n");
        stamp_here(&e, 7);
        assert_eq!(anchors(&e), vec![(7, 2, 0)]);
        e.advance(b"\r\n"); // 离开那一格,下面的正文别盖掉它
        feed_lines(&e, 3);
        assert_eq!(anchors(&e), vec![(7, 0, 0)], "光标到底后滚了 2 行");
        feed_lines(&e, 4);
        assert_eq!(anchors(&e), vec![(7, -4, 0)], "进了回看");
        // 用户往回翻不动锚的 grid 坐标,换算到屏幕行靠 display_offset
        e.with_term_mut(|term| {
            term.scroll_display(alacritty_terminal::grid::Scroll::Delta(3));
        });
        assert_eq!(anchors(&e), vec![(7, -4, 0)]);
    }

    /// 回看行数到顶:锚随那一行一起被淘汰,不残留。
    #[test]
    fn 回看淘汰时锚随行消失() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 4), 3);
        stamp_here(&e, 1);
        e.advance(b"\r\n");
        feed_lines(&e, 4);
        assert_eq!(anchors(&e).len(), 1, "还在回看里");
        feed_lines(&e, 6);
        assert_eq!(anchors(&e), vec![], "淘汰后消失");
    }

    /// 调小回滚行数当场裁历史,同一条路。
    #[test]
    fn 调小回滚行数时锚随行消失() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 4), 50);
        stamp_here(&e, 1);
        e.advance(b"\r\n");
        feed_lines(&e, 10);
        assert_eq!(anchors(&e).len(), 1);
        e.set_scrollback(2);
        assert_eq!(anchors(&e), vec![]);
    }

    /// 被复用的行不残留锚:scroll_up 回收旧行时靠 `occ` 决定重置范围,
    /// 直接写格子同样要把 `occ` 记上,否则新内容那一行还挂着旧锚。
    #[test]
    fn 回收的行不残留旧锚() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 3), 0);
        e.advance(b"\x1b[1;10H");
        stamp_here(&e, 1);
        feed_lines(&e, 12);
        assert_eq!(anchors(&e), vec![]);
    }

    /// 备用屏:锚属于各自的 grid。进备用屏后看不到主屏的锚,退回来还在;
    /// 备用屏的锚随备用屏清空。
    #[test]
    fn 备用屏切换() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 4), 100);
        e.advance(b"\r\n");
        stamp_here(&e, 1);
        e.advance(b"\x1b[?1049h");
        assert_eq!(anchors(&e), vec![], "主屏的锚不在备用屏上");
        e.advance(b"\x1b[3;1H");
        stamp_here(&e, 2);
        assert_eq!(anchors(&e), vec![(2, 2, 0)]);
        e.advance(b"\x1b[?1049l");
        assert_eq!(anchors(&e), vec![(1, 1, 0)], "退回主屏,主屏的锚还在");
        e.advance(b"\x1b[?1049h");
        assert_eq!(anchors(&e), vec![], "再进备用屏,上次的内容已清空");
    }

    /// 备用屏里 resize,回到主屏后主屏的锚照样在。
    #[test]
    fn 备用屏里_resize_主屏的锚还在() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(40, 6), 100);
        e.advance(b"\x1b[3;5H");
        stamp_here(&e, 1);
        e.advance(b"\x1b[?1049h");
        e.resize(TermSize::new(30, 4));
        e.advance(b"\x1b[?1049l");
        let found = anchors(&e);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].2, 4, "列不变");
    }

    /// `CSI 2J` 在主屏上把内容顶进回看(alacritty 的口径),锚跟着走而不是被抹掉。
    /// 所以按协议删屏上的图要 [`super::super`] 自己做,靠不上「锚没了」。
    #[test]
    fn 主屏清屏时锚被顶进回看() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 6), 100);
        e.advance(b"text\r\n");
        stamp_here(&e, 1);
        e.advance(b"\x1b[2J");
        let found = anchors(&e);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].1 < 0, "进了回看:{found:?}");
    }

    /// 备用屏的 `CSI 2J` 是就地清空(备用屏没有回看)。
    #[test]
    fn 备用屏清屏时锚消失() {
        let e = TerminalEmulator::new(TermSize::new(20, 6));
        e.advance(b"\x1b[?1049h\x1b[2;1H");
        stamp_here(&e, 1);
        assert_eq!(anchors(&e).len(), 1);
        e.advance(b"\x1b[2J");
        assert_eq!(anchors(&e), vec![]);
    }

    /// `CSI 3J` 只清回看:回看里的锚没了,屏上的不动。RIS 全清。
    #[test]
    fn 清回看与_ris() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 4), 100);
        stamp_here(&e, 1);
        e.advance(b"\r\n");
        feed_lines(&e, 8);
        stamp_here(&e, 2);
        assert_eq!(anchors(&e).len(), 2);
        e.advance(b"\x1b[3J");
        let found = anchors(&e);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, 2);
        e.advance(b"\x1bc");
        assert_eq!(anchors(&e), vec![]);
    }

    /// 擦除类序列只有碾到那一格才消失;光标在锚右边的 `CSI K` 碰不到它。
    #[test]
    fn 擦除只碾到那一格才消失() {
        for (seq, survives, name) in [
            (&b"\x1b[1;5H\x1b[K"[..], true, "EL 0 在锚右边"),
            (&b"\x1b[1;3H\x1b[K"[..], false, "EL 0 盖住锚"),
            (&b"\x1b[1;3H\x1b[1K"[..], true, "EL 1 没够着"),
            (&b"\x1b[1;5H\x1b[1K"[..], false, "EL 1 盖住锚"),
            (&b"\x1b[2K"[..], false, "EL 2"),
            (&b"\x1b[1;4H\x1b[1X"[..], false, "ECH 正中"),
            (&b"\x1b[1;5H\x1b[1X"[..], true, "ECH 没碰到"),
            (&b"\x1b[1;4Hz"[..], false, "覆盖写"),
            (&b"\x1b[1;5Hz"[..], true, "写在隔壁"),
        ] {
            let e = TerminalEmulator::new(TermSize::new(20, 4));
            e.advance(b"\x1b[1;4H");
            stamp_here(&e, 1);
            e.advance(seq);
            assert_eq!(anchors(&e).len(), usize::from(survives), "{name}");
        }
    }

    /// 插入 / 删除行列:锚跟着格子挪,被顶出去才消失。
    #[test]
    fn 插删行列时锚跟着挪() {
        let e = TerminalEmulator::new(TermSize::new(20, 6));
        e.advance(b"\x1b[3;4H");
        stamp_here(&e, 1);
        e.advance(b"\x1b[1;1H\x1b[2L");
        assert_eq!(anchors(&e), vec![(1, 4, 3)], "IL");
        e.advance(b"\x1b[1M");
        assert_eq!(anchors(&e), vec![(1, 3, 3)], "DL");
        e.advance(b"\x1b[4;1H\x1b[2@");
        assert_eq!(anchors(&e), vec![(1, 3, 5)], "ICH");
        e.advance(b"\x1b[1P");
        assert_eq!(anchors(&e), vec![(1, 3, 4)], "DCH");
        e.advance(b"\x1b[4;1H\x1b[1M");
        assert_eq!(anchors(&e), vec![], "DL 碾到锚所在的行");
    }

    /// **空白格的锚要熬过缩窄重排**:这是 `is_empty` 那个坑的回归。
    /// 不打前景色的话,列数缩到锚的列以内时 `Row::shrink` 把它当空格子丢掉。
    #[test]
    fn 缩窄重排时空白格的锚不丢() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(40, 6), 100);
        e.advance(b"\x1b[2;31H");
        stamp_here(&e, 1);
        e.resize(TermSize::new(20, 6));
        let found = anchors(&e);
        assert_eq!(found.len(), 1, "锚被重排丢了");
        assert_eq!(found[0].2, 10, "折到下一行,第 30 列 → 第 10 列:{found:?}");

        // 对照组:不打前景色,同样的操作锚就没了。哪天上游改了判据,这条会红,
        // 到时候 ANCHOR_FG 这一手就可以摘掉
        let bare = TerminalEmulator::with_scrollback(TermSize::new(40, 6), 100);
        bare.advance(b"\x1b[2;31H");
        bare.with_term_mut(|term| {
            let grid = term.grid_mut();
            let point = grid.cursor.point;
            grid[point.line][point.column].set_hyperlink(Some(Hyperlink::new(Some("x"), uri(1))));
        });
        assert_eq!(anchors(&bare).len(), 1);
        bare.resize(TermSize::new(20, 6));
        assert_eq!(
            anchors(&bare),
            vec![],
            "光挂链接的空白格算「空」,被整段丢掉"
        );
    }

    /// 锚挂在折行正文里的一个字上,加宽时跟着这个字回到上一行。
    #[test]
    fn 加宽重排时锚跟着字回到上一行() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(10, 6), 100);
        e.advance(b"0123456789abcde");
        e.advance(b"\x1b[2;3H"); // 第 2 行第 3 列 = 'c'
        stamp_here(&e, 1);
        assert_eq!(anchors(&e), vec![(1, 1, 2)]);
        e.resize(TermSize::new(20, 6));
        assert_eq!(e.visible_lines()[0], "0123456789abcde");
        assert_eq!(anchors(&e), vec![(1, 0, 12)], "字到哪,锚到哪");
        // 再缩回来:折出来的那一行把上一行顶进回看,锚还是挂在 c 上
        e.resize(TermSize::new(10, 6));
        let found = anchors(&e);
        assert_eq!(found.len(), 1, "{found:?}");
        let (_, line, column) = found[0];
        assert_eq!(column, 2);
        e.with_term(|term| assert_eq!(term.grid()[Line(line)][Column(column)].c, 'c'));
    }

    /// 行数变化:光标贴底时缩行把内容顶进回看,加行从回看拉回来,锚都跟着。
    #[test]
    fn 行数变化时锚跟着走() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 6), 100);
        e.advance(b"\x1b[3;2H");
        stamp_here(&e, 1);
        e.advance(b"\x1b[6;1H");
        e.resize(TermSize::new(20, 4));
        assert_eq!(anchors(&e), vec![(1, 0, 1)], "顶进 2 行");
        e.resize(TermSize::new(20, 2));
        assert_eq!(anchors(&e), vec![(1, -2, 1)], "再顶 2 行进回看");
        e.resize(TermSize::new(20, 6));
        assert_eq!(anchors(&e), vec![(1, 2, 1)], "拉回来");
    }

    /// 只有锚的一行不是「空行」:清屏顶行数时要算上它。对照 `is_empty` 口径。
    #[test]
    fn 只有锚的行也算内容() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 6), 100);
        e.advance(b"\x1b[5;1H");
        stamp_here(&e, 1);
        e.advance(b"\x1b[2J");
        let found = anchors(&e);
        assert_eq!(found.len(), 1);
        assert_eq!(e.history_lines(), 5, "第 0..=4 行都顶进回看");
    }
}
