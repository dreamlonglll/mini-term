//! AI 任务标记(⚑)的数据层。对应原版 `src/types.ts:605-621` 的 `AiMarker`、
//! `src/store.ts:1182-1225` 的四个 action、`src/utils/terminalCache.ts:551-597`
//! 的 xterm `IMarker` 那一摊,以及 `src/hooks/useMarkerHotkeys.ts:41-50` 的推进规则。
//!
//! # 这个功能大部分时候不产生任何标记
//!
//! `terminalCache.ts:551-557` 自己写着:**备用缓冲区里打点没有意义,直接跳过**。
//! Claude Code / Codex 这类 Ink/TUI agent 一进来就切 alt screen,打点被整个跳过 ——
//! 所以装机版里「⚑ N」按钮平时根本不出现,只有**不切 alt screen 的行式 AI CLI**
//! 才攒得出标记。本模块照抄这条闸门([`crate::pane::TerminalPane::write`] 里那句
//! `TermMode::ALT_SCREEN` 判定),不要为了「让它看起来有用」而放开:
//! alt grid 的 `max_scroll_limit` 是 0,没有回看缓冲,跳转无处可跳。
//!
//! GPUI 侧有一处**天然优于原版**:alacritty 的备用屏是独立 grid(`swap_alt` 只
//! `mem::swap` 两个 grid),主屏 scrollback 在 TUI 期间原封不动;而 xterm 从 alt
//! buffer 退回来时 `clearAllMarkers()` 会把主缓冲的 marker 全清掉。也就是说
//! **GPUI 侧「进 TUI 前打的点,退出 TUI 后仍然有效」**。这是改善,不是偏差。
//!
//! # 锚点:xterm `IMarker` 的等价物
//!
//! xterm 的 `IMarker` 自己会跟着 buffer 移动(新输出把内容顶进 scrollback 时
//! `marker.line` 自动减小),alacritty **没有**这种东西:`Point::line` 是 grid
//! 绝对行号,新输出一来整体减小,而 alacritty 既不提供「累计滚出多少行」的计数器、
//! `EventListener` 里也没有滚动事件。
//!
//! 于是走**算术锚点**:打点时记 `anchor = cursor.line + history_size`,取用时
//! `current_line = anchor - history_size_now`([`marker_line`])。推导:内容每被顶
//! 上去一行,它的 `Line` 减 1、同时 `history_size` 加 1(`grid/mod.rs` 的 `scroll_up`
//! → `increase_scroll_limit`),两者之和守恒 —— **在 scrollback 装满之前这是精确的**,
//! 而且 `line + history_now == anchor >= 0` 恒成立,即锚点永远落在缓冲区之内。
//!
//! ## 饱和是唯一的破绽,处置是「剪枝」
//!
//! `history_size` 涨到 `max_scroll_limit` 之后,`increase_scroll_limit` 里
//! `count = min(count, max - history) = 0`,此后每次滚动**evict 一行但
//! `history_size` 不变** → 上式冻结,锚点会静默指向错误的行(比「跳不过去」还糟)。
//!
//! 主会话拍板:**对齐 xterm `IMarker` 的语义 —— 被裁出历史即废弃(disposed),
//! 不做文本重定位**。落地形态就是 [`is_saturated`] + [`prune`]:一旦该 pane 的
//! scrollback 装满,这一份标记整体作废(等价于原版 `pruneDisposed` 把 xterm 已
//! dispose 的条目过滤掉),此后新打的点也会在同一次 `push + prune` 里当场被清掉 ——
//! 饱和期的锚点同样不可信,留着就是「指向错误的行」。
//!
//! **这是刻意取舍,不是 bug**:比 xterm 早一点丢弃(xterm 只丢真被裁掉的那几条,
//! 我们丢整份),换来的是「绝不指向错误的行」。代价的量级:默认回滚 10000 行,
//! 一个**不进 alt screen** 的 AI CLI 要在主缓冲里吐满一万行才会触发。
//!
//! 不要试图用「每帧采样 `history_size` 差值累加」来重建计数器 —— 饱和后差值恒为 0,
//! 问题原封不动;也不要去读 `Grid` 内部的 `Storage::zero`(私有)。

use crate::tree::gen_id;

/// 一条 AI 任务标记。字段与原版 `types.ts:607-620` 一一对应,只有一处换了:
/// `xtermMarkerId` → [`Self::anchor`](见模块注释)。
#[derive(Clone, Debug, PartialEq)]
pub struct AiMarker {
    /// store 索引与列表行 key。
    pub id: String,
    /// 该 pane 内的序号,UI 上显示成 `#N`。
    ///
    /// **不是自增计数器**:原版 `store.ts:1191` 取的是 `updated.length + 1`,
    /// 也就是「它在(可能已被剪过的)列表里排第几」。剪枝之后新标记的序号会与
    /// 之前用过的重复 —— 照抄。
    pub seq: usize,
    pub pty_id: u32,
    /// 用户输入原文(已 trim)。括号粘贴的多行是**一条**,`line` 里带 `\n`。
    pub line: String,
    /// UTC epoch 毫秒(`mt_ai::UserSubmit::ts`),显示时按本地时区格式化。
    pub ts: i64,
    /// 稳定绝对行号 = 打点时的 `cursor.point.line` + 当时的 `history_size`。
    pub anchor: i32,
    /// 最后一条为 true,新标记到来时前一条翻 false。
    ///
    /// ⚠️ **没有任何地方在 AI 完成时把最后一条翻 false**(`store.ts:1182-1203`
    /// 是唯一改写它的地方)。所以「最后一条永远亮着进行中圆点」是原版行为,照抄。
    pub in_progress: bool,
}

/// pane 侧当场取好的一批标记。
///
/// 一次 `drain_submits` 里的多条提交共用同一个锚点 —— 它们是在**同一个瞬间**
/// (同一次 `write`,PTY 还没回显换行)取出来的,光标就停在同一行上。
#[derive(Clone, Debug, PartialEq)]
pub struct MarkerBatch {
    /// `(原文, epoch ms)`,顺序即提交顺序。
    pub submits: Vec<(String, i64)>,
    /// 见 [`AiMarker::anchor`]。
    pub anchor: i32,
    /// 打点那一刻的 `history_size`。
    pub history: i32,
    /// 该 pane 的回滚上限(`max_scroll_limit`)。剪枝判据,见 [`is_saturated`]。
    pub max_scrollback: i32,
}

/// 往列表尾部追加一条,返回新条目的 id。
///
/// 逐条对照 `store.ts:1182-1203`:**先把最后一条的 `in_progress` 翻 false**,
/// 再追加(`seq = 列表长度 + 1`、`in_progress: true`)。
pub fn push_marker(
    list: &mut Vec<AiMarker>,
    pty_id: u32,
    line: String,
    ts: i64,
    anchor: i32,
) -> String {
    if let Some(last) = list.last_mut() {
        last.in_progress = false;
    }
    let id = gen_id("marker");
    list.push(AiMarker {
        id: id.clone(),
        seq: list.len() + 1,
        pty_id,
        line,
        ts,
        anchor,
        in_progress: true,
    });
    id
}

/// 锚点 → 当前 grid 行号。见模块注释的推导。
pub fn marker_line(anchor: i32, history_now: i32) -> i32 {
    anchor - history_now
}

/// 该 pane 的回滚缓冲装满了吗 —— 装满即所有标记作废,见模块注释。
///
/// `max_scrollback <= 0` 视为「没有回滚缓冲」(alt grid 就是这样),此时**不判废**:
/// 调用方本来就该先用 `ALT_SCREEN` 把整条路挡在外面,这里只是不添乱 ——
/// 否则一进 vim 就会把主屏攒下来的标记全抹掉。
pub fn is_saturated(history_now: i32, max_scrollback: i32) -> bool {
    max_scrollback > 0 && history_now >= max_scrollback
}

/// 就地剪枝,返回「有没有真的删掉东西」。
///
/// 与原版 `pruneDisposed`(`store.ts:1213-1223`)同节奏:**只在 `push_marker`
/// 之后与真正要跳转之前跑**,不在每帧渲染里跑(原版
/// `useAiSubmitMarker.ts:22` 也只在 `addMarker` 之后调一次)。
pub fn prune(list: &mut Vec<AiMarker>, history_now: i32, max_scrollback: i32) -> bool {
    if list.is_empty() || !is_saturated(history_now, max_scrollback) {
        return false;
    }
    list.clear();
    true
}

/// 下一条要跳到哪(`useMarkerHotkeys.ts:41-50`)。**非环形**。
///
/// - 有游标且还在列表里 → `游标 ± 1`;
/// - 没有游标(或游标那条已被剪掉,原版的 `indexOf === -1`)→ 首次 ↑(`dir = -1`)
///   跳**最新一条**、首次 ↓ 跳**最早一条**;
/// - 越界 → `None`:到头就停住,**游标也不推进**。
///
/// 与终端查找的 [`mt_ui::advance_index`]「环形推进」**相反**,别抄错。
pub fn next_index(cursor: Option<usize>, len: usize, dir: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let next: i64 = match cursor {
        Some(idx) if idx < len => idx as i64 + dir as i64,
        // 游标不存在 / 已被剪掉
        _ if dir < 0 => len as i64 - 1,
        _ => 0,
    };
    (next >= 0 && next < len as i64).then_some(next as usize)
}

/// 列表里那一列时间:本地时区的 `HH:mm`,两位补零(`MarkerList.tsx:11-14`)。
pub fn format_time(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ts).single() {
        Some(dt) => dt.format("%H:%M").to_string(),
        // 时间戳离谱(取不到系统时间时 mt-ai 会落 0,那是合法时刻、走上面这支)
        // 时不画时间,别让一行崩掉整个列表
        None => "--:--".to_string(),
    }
}

/// 正文截断(`MarkerList.tsx:16-18` 的 `truncate(s, 40)`)。
///
/// 原版是 `s.slice(0, max - 1) + '…'`,按 **UTF-16 码元**切;这里按**字符**切 ——
/// 中文一句 40 个字两边都切到 39 个,只有 emoji(代理对)那一档对不上,可忽略。
///
/// 粘贴的多行会带 `\n`,单行行高的列表里画成竖条子,所以**换行统一压成空格**
/// (原版靠 CSS `truncate` 的 `white-space: nowrap` 达到同样效果)。
pub fn truncate_line(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect();
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(list: &mut Vec<AiMarker>, line: &str, anchor: i32) -> String {
        push_marker(list, 7, line.to_string(), 0, anchor)
    }

    /// 追加时把上一条的「进行中」翻掉,序号是列表长度 + 1(照抄 store.ts:1191)。
    #[test]
    fn 追加标记时上一条翻为已完成() {
        let mut list = Vec::new();
        marker(&mut list, "a", 10);
        assert_eq!(list[0].seq, 1);
        assert!(list[0].in_progress);

        marker(&mut list, "b", 12);
        assert_eq!(list.len(), 2);
        assert!(!list[0].in_progress, "上一条必须翻 false");
        assert!(list[1].in_progress, "最后一条永远是进行中");
        assert_eq!(list[1].seq, 2);
    }

    /// id 逐条唯一 —— 列表行 key 与游标都拿它当身份。
    #[test]
    fn 标记_id_不重复() {
        let mut list = Vec::new();
        let a = marker(&mut list, "a", 1);
        let b = marker(&mut list, "a", 1);
        assert_ne!(a, b, "同样的正文同样的锚点也得是两条");
    }

    /// 未饱和时锚点是精确的:输出 N 行之后行号正好减 N,且永远落在缓冲区内。
    #[test]
    fn 未饱和时锚点精确跟随滚动() {
        // 打点时光标在第 5 行、历史里已有 3 行 → anchor = 8
        let anchor = 5 + 3;
        assert_eq!(marker_line(anchor, 3), 5);
        // 又滚出去 10 行:history 3 → 13,行号 5 → -5(进了回看缓冲)
        assert_eq!(marker_line(anchor, 13), -5);
        // 再滚 1000 行照样对得上
        assert_eq!(marker_line(anchor, 1013), 5 - 1010);
        // 不变式:line + history == anchor >= 0,即行号绝不会越过缓冲区顶端
        for history in [3, 13, 1013, 9_999] {
            assert!(marker_line(anchor, history) >= -history);
        }
    }

    /// 饱和 = 全部作废(含饱和期新打的那条)—— 锚点算术从此不可信,见模块注释。
    #[test]
    fn 回滚缓冲装满即判饱和() {
        assert!(!is_saturated(9_999, 10_000), "还差一行没满");
        assert!(is_saturated(10_000, 10_000), "刚好装满");
        assert!(is_saturated(10_050, 10_000), "越过上限(热改小了回滚行数)");
        // 没有回滚缓冲时不判废:那条路由 ALT_SCREEN 闸门挡在外面,这里不添乱
        assert!(!is_saturated(0, 0));
    }

    /// 剪枝把废掉的删干净并如实回报「动没动过」;饱和期新打的点当场被清掉。
    #[test]
    fn 剪枝在饱和时清空列表() {
        let mut list = Vec::new();
        marker(&mut list, "a", 10);
        marker(&mut list, "b", 20);

        assert!(!prune(&mut list, 5_000, 10_000), "没饱和,一条都不该删");
        assert_eq!(list.len(), 2);

        assert!(prune(&mut list, 10_000, 10_000), "饱和了,全删");
        assert!(list.is_empty());
        assert!(!prune(&mut list, 10_000, 10_000), "空列表上再跑一遍是空操作");

        // 饱和期再打点:push + prune 是同一次,净效果是列表仍然空的
        marker(&mut list, "c", 30);
        assert!(prune(&mut list, 10_000, 10_000));
        assert!(list.is_empty(), "饱和期的锚点不可信,留着就是指向错误的行");
    }

    /// 推进规则:首次 ↑ 到最新一条、首次 ↓ 到最早一条(`useMarkerHotkeys.ts:44-48`)。
    #[test]
    fn 首次跳转按方向落到两端() {
        assert_eq!(next_index(None, 4, -1), Some(3), "↑ = 最新一条");
        assert_eq!(next_index(None, 4, 1), Some(0), "↓ = 最早一条");
        // 游标那条被剪掉了(原版的 indexOf === -1)也走这一支
        assert_eq!(next_index(None, 1, -1), Some(0));
        assert_eq!(next_index(None, 0, -1), None, "空列表直接返回");
        assert_eq!(next_index(Some(0), 0, 1), None);
    }

    /// 有游标就逐格走,**到两端停住、不绕回**(与终端查找的环形推进相反)。
    #[test]
    fn 推进到头就停住不绕回() {
        assert_eq!(next_index(Some(2), 4, -1), Some(1));
        assert_eq!(next_index(Some(2), 4, 1), Some(3));
        assert_eq!(next_index(Some(0), 4, -1), None, "到顶不绕到末尾");
        assert_eq!(next_index(Some(3), 4, 1), None, "到底不绕到开头");
        // 游标越界(列表被剪短了)→ 按「没有游标」处理
        assert_eq!(next_index(Some(9), 4, -1), Some(3));
        assert_eq!(next_index(Some(9), 4, 1), Some(0));
    }

    /// 连按一路走到头:4 条列表按 ↑ 只走 4 步,第 5 下不动。
    #[test]
    fn 连按沿列表逐格走() {
        let mut cursor = None;
        let mut visited = Vec::new();
        for _ in 0..6 {
            match next_index(cursor, 4, -1) {
                Some(idx) => {
                    visited.push(idx);
                    cursor = Some(idx);
                }
                None => break,
            }
        }
        assert_eq!(visited, vec![3, 2, 1, 0], "到顶就停,游标停在 0 上");
    }

    /// 正文截断:40 字以内原样,超了就 39 字 + 省略号;换行压成空格。
    #[test]
    fn 正文按四十字截断() {
        assert_eq!(truncate_line("hello", 40), "hello");
        let long: String = "a".repeat(50);
        let cut = truncate_line(&long, 40);
        assert_eq!(cut.chars().count(), 40);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().filter(|c| *c == 'a').count(), 39);
        // 边界:正好 40 字不截
        let exact: String = "b".repeat(40);
        assert_eq!(truncate_line(&exact, 40), exact);
        // 括号粘贴的多行是一条,换行压成空格
        assert_eq!(truncate_line("a\nb\r\nc\td", 40), "a b  c d");
        // 中文按字符算,不按字节
        let zh: String = "标".repeat(41);
        assert_eq!(truncate_line(&zh, 40).chars().count(), 40);
    }

    /// 时间列是本地时区的 `HH:mm`,永远五个字符。
    #[test]
    fn 时间列固定五字符() {
        let s = format_time(1_700_000_000_000);
        assert_eq!(s.chars().count(), 5, "{s}");
        assert_eq!(s.as_bytes()[2], b':');
        // epoch 0 是合法时刻(1970-01-01 的本地时间),照样出五个字符
        assert_eq!(format_time(0).chars().count(), 5);
    }
}
