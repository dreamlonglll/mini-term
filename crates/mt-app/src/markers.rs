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
//! ## 「打点时」是哪一刻:不是按下 Enter 那一刻
//!
//! 取的是**按下 Enter 之后 200ms 内光标绝对行的最小值**,不是按键当场的光标位置。
//! Ink 应用等待输入时光标停在渲染块**下方**(`log-update` 每帧尾部多一个 `\n`),
//! 当场取必然偏下若干行 —— 详见
//! [`mt_terminal::TerminalEmulator::arm_cursor_floor`] 与
//! [`crate::pane::TerminalPane::arm_marks`]。
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
//!
//! # 第二个破绽:算术对了,内容却没了(清屏 / reflow)
//!
//! 上面那套守恒只保证「内容被顶进 scrollback 时行号跟着走」,**保证不了内容还在**。
//! 有两条路能把行原地掏空而 `history_size` 纹丝不动:
//!
//! 1. **就地清屏**。Claude Code 2.1.x 的 `/new` / `/clear` 发的是
//!    `ESC[H` + (`ESC[2K` + `ESC[1B`) × viewportRows + `ESC[H` —— 从屏幕顶部逐行
//!    擦到底,**不产生任何滚动**。对照组 `ESC[2J` 走的是 alacritty 的
//!    `clear_viewport` → `scroll_up`,内容进 scrollback、锚点完好无损;而逐行 `2K`
//!    这条是把 cell 抹白,`history_size` 不动、`anchor - history` 照样算得出行号,
//!    那一行却已经空了,随后被新会话的输出覆盖 —— **跳过去看到的是不相干的内容**。
//!    `ESC[3J`(清 scrollback)与 `ESC c`(RIS)更狠,直接把 `history_size` 归零,
//!    锚点整体越界。
//! 2. **列宽变化触发的 reflow**。alacritty 只在**列数**变化时重排折行,行被拆/合
//!    之后 `history_size` 任意跳变,守恒直接失效(行数变化只是 grow/shrink,守恒仍在)。
//!
//! 这两条 [`is_saturated`] 一条都测不到 —— 它判的是「scrollback 装满」。
//!
//! ## 处置:锚点行内容指纹,对不上就判废
//!
//! 定锚时连**那一行的文本指纹**一起记([`MarkerAnchor::Settled`]),取用前重算比对,
//! 不匹配就剪掉([`prune_stale`])。**刻意不认任何 agent 的具体转义序列**:
//! 序列匹配只能覆盖「今天的 Claude Code」,Codex 关掉 alt screen
//! (`--no-alt-screen` / `NO_ALT_SCREEN` / `[tui] alternate_screen = false`)之后走的是
//! ratatui/crossterm 另一套,Grok 又是一套,agent 改一次渲染器补丁就失效。指纹不问
//! 「谁、发了什么」,只问「那一行还是不是原来那行」,顺带把 reflow 也一并盖住。
//!
//! ⚠️ **已知边界:锚点行本身为空时判据失灵**。空行的指纹与被擦白之后相同,校验会
//! 放行。正常路径落不到这里 —— 锚点定在 static 区的 `> 用户输入` 那一行,它必然
//! 带着 `>` 前缀和正文;真取到空行说明定锚那一下就没抓准,那条标记本来就不可信。
//! **不要为此改成「空指纹一律判废」**:那会把「行式 CLI 打点后光标停在空行」这类
//! 正常场景一起误杀。
//!
//! # 第三个破绽:AI 正忙时提交,那条消息**根本还没上屏**
//!
//! 上面整套(定锚 + 指纹)都建立在同一个前提上:**按 Enter 之后的 200ms 里,属于
//! 这条提交的 `> 用户输入` static 行会落地**。AI 空闲时确实如此。
//!
//! 但 AI 正在输出时用户追加一句,Claude Code 这类 agent 是**把它排进队列**的 ——
//! 内容留在底部输入框里跟着每帧一起动,要等当前回合结束才会被打成 static 消息。
//! 于是那 200ms 里水位只能落在**还在重绘的动态区**上,指纹记的是一帧瞬时内容,
//! 下一次 [`prune_stale`] 一比对就把这条剪掉了 —— 表现是「AI 忙的时候追加的那句,
//! 标记下拉里根本没有」。`mt-ai` 侧其实一直记着这条提交
//! (`tracker.rs` 的 `track_input_submits_multiple_in_working_window` 就是保它的),
//! 丢是丢在定锚这一步。
//!
//! ## 处置:定不住就先挂起,别丢
//!
//! 定锚那一刻校验一次「锚点行里看不看得见这条提交的正文」([`settle_anchor`]):
//!
//! - 看得见 → [`MarkerAnchor::Settled`],和以前一模一样;
//! - 看不见 → [`MarkerAnchor::Pending`],**条目照样进列表**(内容、时间都在,
//!   用户看得到自己追加过什么),只是暂时跳不了。
//!
//! 挂起的条目之后靠 [`relocate_pending`] 补锚:拿正文在 scrollback 里从
//! `Pending::from` 往下扫,找到那条消息真正落地的行就地转成 `Settled`。AI 把队列
//! 里这条处理掉的那一刻,它就自己好了。
//!
//! ⚠️ 这里的文本回扫**与模块前面「不做文本重定位」那条取舍不冲突**:那条说的是
//! scrollback 饱和后拿文本去**纠正已经定过的锚**(会把「跳错行」换成「跳到另一个
//! 错行」);这里是给**从来没定过锚**的条目找它唯一的落点,找不到就继续挂着,
//! 不会产生「看起来能跳、跳过去是错的」这种更糟的结果。

use crate::tree::gen_id;

/// 一条标记的锚点状态。
///
/// 两态的由来见模块注释的「第三个破绽」:AI 忙时提交的那条消息要等回合结束才上屏,
/// 定锚那一下**无锚可定**,只能先挂起。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarkerAnchor {
    /// 已定位。`anchor` 是稳定绝对行(定锚时的 `cursor.point.line` + `history_size`),
    /// `fingerprint` 是那一刻该行的文本指纹 —— 取用前重算比对,对不上即判废。
    Settled { anchor: i32, fingerprint: u64 },
    /// 还没定位:提交那一下属于它的那条消息还没上屏(被 AI 排进了队列)。
    ///
    /// `from` = 定锚窗口里光标到达过的**最上面**那一行,[`relocate_pending`] 从这里
    /// 往下找 —— 那条消息只可能落在这个位置或它下方。
    Pending { from: i32 },
}

impl MarkerAnchor {
    /// 已定位的绝对行;还挂着的返回 `None`(调用方据此**不跳转**)。
    pub fn settled(&self) -> Option<i32> {
        match *self {
            Self::Settled { anchor, .. } => Some(anchor),
            Self::Pending { .. } => None,
        }
    }

    /// 还等着补锚吗 —— UI 据此把这一行画成灰的(点了也跳不动)。
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }
}

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
    /// 锚点。「定锚时」不是按下 Enter 那一刻(见模块注释),而且**可能定不下来**
    /// —— AI 忙时提交的那条消息要等回合结束才上屏,见 [`MarkerAnchor::Pending`]。
    pub anchor: MarkerAnchor,
    /// 最后一条为 true,新标记到来时前一条翻 false。
    ///
    /// ⚠️ **没有任何地方在 AI 完成时把最后一条翻 false**(`store.ts:1182-1203`
    /// 是唯一改写它的地方)。所以「最后一条永远亮着进行中圆点」是原版行为,照抄。
    pub in_progress: bool,
}

/// pane 侧定好锚的一批标记。
///
/// 一次 `drain_submits` 里的多条提交共用同一个锚点 —— 它们是在**同一次 `write`**
/// 里取出来的,只可能落在同一个位置上。挂起时同理:同一个 `Pending::from`。
#[derive(Clone, Debug, PartialEq)]
pub struct MarkerBatch {
    /// `(原文, epoch ms)`,顺序即提交顺序。
    pub submits: Vec<(String, i64)>,
    /// 见 [`AiMarker::anchor`]。
    pub anchor: MarkerAnchor,
    /// 定锚那一刻的 `history_size`。
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
    anchor: MarkerAnchor,
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

/// 一行文本的指纹。定锚与校验**必须走同一个函数** —— 两侧取法只要差一点
/// (裁不裁行尾空格、宽字符 spacer 算不算)就会全量假性失配,标记当场全灭。
///
/// 不需要抗碰撞:这不是安全边界,撞了最坏结果是漏掉一条已失效的标记,
/// 与打补丁前的行为一致。
pub fn fingerprint_line(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// 剪掉锚点行**内容已经对不上**的标记,返回「有没有真的删掉东西」。
///
/// `probe(anchor)` 交回该锚点当前指向那一行的指纹,`None` = 那一行已不在缓冲区里
/// (被 `ESC[3J` / RIS 清掉了历史,或被 reflow 重排到界外)。调用方接
/// [`mt_terminal::TerminalEmulator::line_text`] + [`fingerprint_line`]。
///
/// 与 [`prune`] 的分工:那一条判「scrollback 装满,算术不可信」,这一条判
/// 「算术还对,但那一行已经不是原来那行」。两条都在**同一个节奏**上跑
/// (push 之后、跳转之前),不进渲染路径 —— 每次要读 N 行 cell,N 是列表长度。
///
/// ⚠️ [`MarkerAnchor::Pending`] 的条目**一条都不剪**:它们压根没定过锚,拿什么比
/// 都是错的。它们的归宿是 [`relocate_pending`] 补锚成功,或者跟着
/// [`prune`] 在 scrollback 饱和时一起走。
pub fn prune_stale(list: &mut Vec<AiMarker>, probe: impl Fn(i32) -> Option<u64>) -> bool {
    let before = list.len();
    list.retain(|m| match m.anchor {
        // 还没定位的留着:内容对用户仍然有用,锚点等 AI 把队列消化掉再补
        MarkerAnchor::Pending { .. } => true,
        MarkerAnchor::Settled {
            anchor,
            fingerprint,
        } => probe(anchor) == Some(fingerprint),
    });
    list.len() != before
}

/// 定锚那一刻的判定:水位落到的这一行,**看得见这批提交的正文吗**?
///
/// - 看得见 → [`MarkerAnchor::Settled`],与打这个补丁之前一模一样;
/// - 看不见 → [`MarkerAnchor::Pending`],那条消息还没上屏(AI 忙,输入被排进队列),
///   等 [`relocate_pending`] 补锚。
///
/// `text` 是 `floor` 那一行当前的内容(`None` = 不在缓冲区里,同样判挂起 ——
/// 连行都读不到的锚点留着只会指向错误的行)。
///
/// ⚠️ **判不了的时候一律判 `Settled`**(正文短到 [`MIN_MATCH_CHARS`] 以下时):
/// 短正文("ok"、"y")在任意一行 AI 输出里撞上的概率太高,拿它判挂起会把本来定得
/// 好好的锚点误挂起来。宁可退回打补丁之前的行为,也不引入新的误判。
pub fn settle_anchor(floor: i32, text: Option<&str>, submits: &[&str]) -> MarkerAnchor {
    let judgeable = submits
        .iter()
        .any(|s| submit_head(s).chars().count() >= MIN_MATCH_CHARS);
    let Some(text) = text else {
        // 行都读不到:能判就挂起等补锚,判不了就照旧(反正 prune_stale 会收拾)
        return if judgeable {
            MarkerAnchor::Pending { from: floor }
        } else {
            MarkerAnchor::Settled {
                anchor: floor,
                fingerprint: fingerprint_line(""),
            }
        };
    };
    if !judgeable || submits.iter().any(|s| line_shows_submit(text, s)) {
        return MarkerAnchor::Settled {
            anchor: floor,
            fingerprint: fingerprint_line(text),
        };
    }
    MarkerAnchor::Pending { from: floor }
}

/// 提交正文里参与匹配的那一段:首行去空白。粘贴的多行只认第一行 ——
/// 屏幕上那条消息的**块首**就是它。
fn submit_head(submit: &str) -> &str {
    submit.lines().next().unwrap_or("").trim()
}

/// 定锚校验用的宽松判据:这一行**看得见**这条提交吗。
///
/// 与回扫用的 [`line_matches_submit`] 分开是刻意的,两边要防的东西相反:
///
/// - 这里只看**一行**(水位落到的那一行),放宽一点最多是「本该挂起的没挂起」,
///   退回打补丁之前的行为;卡太严则会把行式 CLI 那种 `You: 正文` 的回显误判成
///   挂起,那是实打实的回归。所以用「包含」。
/// - 回扫要在**几千行**里挑一行,放宽就是跳到不相干的地方,所以卡成双向前缀。
fn line_shows_submit(text: &str, submit: &str) -> bool {
    let head = submit_head(submit);
    if head.chars().count() < MIN_MATCH_CHARS {
        return false;
    }
    let stripped = strip_line_decoration(text);
    // 后一支给「长输入被折行,这一行只显示得下前半段」
    stripped.contains(head)
        || (stripped.chars().count() >= MIN_MATCH_CHARS && head.starts_with(stripped))
}

/// 回扫时认不认「这一行就是那条提交」。
///
/// 判据只有一条:**把行首的装饰剥掉之后,两边互为前缀**。
///
/// - 屏幕那行短于正文 —— 长输入被折行,块首只显示得下前半段;
/// - 屏幕那行长于正文 —— agent 在同一行尾巴上贴了别的东西。
///
/// 两种都要认,所以是双向前缀。**刻意不认任何 agent 的具体前缀字符**(`>` / `❯` /
/// `│` …只是顺手剥掉的常见装饰,不是判据):认死一家,换个 agent 或者它改版就失效,
/// 与 [`fingerprint_line`] 那条「不问是谁发的、只问还是不是那行」同一个理由。
///
/// [`MIN_MATCH_CHARS`] 是误配闸门:正文只有一两个字符时(`y`、`ok`),随便一行 AI
/// 输出都能撞上,那还不如继续挂着 —— 挂着只是跳不动,配错就是跳到不相干的地方。
fn line_matches_submit(text: &str, submit: &str) -> bool {
    let head = submit_head(submit);
    if head.chars().count() < MIN_MATCH_CHARS {
        return false;
    }
    let stripped = strip_line_decoration(text);
    if stripped.chars().count() < MIN_MATCH_CHARS {
        return false;
    }
    stripped.starts_with(head) || head.starts_with(stripped)
}

/// 回扫认定所需的最少字符数,见 [`line_matches_submit`]。
const MIN_MATCH_CHARS: usize = 3;

/// 剥掉行首常见的提示符/框线装饰(`> hi` → `hi`)。
///
/// 只剥**行首**、只剥这几个符号加空白,循环剥是因为 TUI 常常套两层
/// (`│ > hi`)。剥不动就原样返回。
fn strip_line_decoration(text: &str) -> &str {
    let mut s = text.trim();
    loop {
        let next = s
            .strip_prefix(['>', '❯', '›', '»', '│', '|', '┃', '⏵'])
            .unwrap_or(s)
            .trim_start();
        if next.len() == s.len() {
            return s;
        }
        s = next;
    }
}

/// 给还挂着的标记补锚:拿正文在 `[from, bottom]` 里从上往下找它真正落地的行。
/// 返回「有没有真的补上过」。
///
/// `probe(row)` 交回该绝对行的文本(`None` = 不在缓冲区里)。调用方接
/// [`mt_terminal::TerminalEmulator::line_text`];`viewport` 是可视区行数。
///
/// 匹配上一条之后,下一条从它的下一行接着找 —— 提交顺序就是它们在屏幕上出现的
/// 顺序,同一句话追加两次也不会互相抢同一行。某一条扫到底也没找到**不拦着后面的**
/// (用户 Esc 掉了排队里的它,它永远不会上屏)。
///
/// # 扫过的地方不重复扫
///
/// 跑的时机与 [`prune_stale`] 同一个节奏(新增标记时、跳转前、下拉打开时),
/// **不进渲染路径**。但一条永远等不到的挂起会让这三个动作每次都全量重扫一遍
/// scrollback,所以没找到时把它的起点**推到已经扫过的地方**,下次只扫新长出来的行。
///
/// 推进时**留最后一屏不算已扫**:agent 提交那一下的 `eraseLines` 会把光标顶回块首,
/// 那条 static 有可能就打在当前这一屏之内 —— 一路推到 `bottom` 会正好把它跳过去。
/// 摊销下来每一行只被扫一次,再加上每次调用重扫的那一屏。
pub fn relocate_pending(
    list: &mut [AiMarker],
    bottom: i32,
    viewport: i32,
    probe: impl Fn(i32) -> Option<String>,
) -> bool {
    let pending: Vec<usize> = list
        .iter()
        .enumerate()
        .filter(|(_, m)| m.anchor.is_pending())
        .map(|(idx, _)| idx)
        .collect();
    if pending.is_empty() {
        return false;
    }
    let mut changed = false;
    // 上一条补上的行 —— 下一条只从它的下一行往后找
    let mut settled_at: Option<i32> = None;
    for idx in pending {
        let MarkerAnchor::Pending { from } = list[idx].anchor else {
            continue;
        };
        let start = from.max(settled_at.map_or(0, |r| r + 1)).max(0);
        let mut hit = None;
        let mut row = start;
        while row <= bottom {
            if let Some(text) = probe(row)
                && line_matches_submit(&text, &list[idx].line)
            {
                hit = Some((row, text));
                break;
            }
            row += 1;
        }
        match hit {
            Some((row, text)) => {
                list[idx].anchor = MarkerAnchor::Settled {
                    anchor: row,
                    fingerprint: fingerprint_line(&text),
                };
                settled_at = Some(row);
                changed = true;
            }
            // 继续挂着,但把起点推到已经扫过的地方(留最后一屏),下次只扫新长出来的。
            // **不算 changed**:用户看到的列表一个字都没变
            None => {
                let next = (bottom - viewport + 1).max(start);
                if next > start {
                    list[idx].anchor = MarkerAnchor::Pending { from: next };
                }
            }
        }
    }
    changed
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
        // 指纹默认拿正文本身当那一行的内容,校验用例里再按需伪造
        push_marker(
            list,
            7,
            line.to_string(),
            0,
            MarkerAnchor::Settled {
                anchor,
                fingerprint: fingerprint_line(line),
            },
        )
    }

    /// 还挂着的条目(AI 忙时提交,那条消息还没上屏)。
    fn pending(list: &mut Vec<AiMarker>, line: &str, from: i32) -> String {
        push_marker(list, 7, line.to_string(), 0, MarkerAnchor::Pending { from })
    }

    /// 大到不会触发「没找到就把起点往下推」的屏高 —— 推进另有专测
    /// (`补不上时把起点推到已扫过的地方`)。
    const NO_ADVANCE: i32 = 100_000;

    /// 把若干行摆成「绝对行 → 文本」的探针,越界交回 `None`。
    fn screen<'a>(rows: &'a [(i32, &'a str)]) -> impl Fn(i32) -> Option<String> + use<'a> {
        move |row| {
            rows.iter()
                .find(|(r, _)| *r == row)
                .map(|(_, text)| text.to_string())
        }
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

    /// 指纹口径:定锚与校验必须走同一个函数,同文本必然同值、不同文本必然不同值。
    #[test]
    fn 指纹按行文本定值() {
        assert_eq!(fingerprint_line("> 帮我看看这段"), fingerprint_line("> 帮我看看这段"));
        assert_ne!(fingerprint_line("> a"), fingerprint_line("> b"));
        // 被 ESC[2K 擦白之后就是空串 —— 与任何有内容的行都不同,这正是判据的支点
        assert_ne!(fingerprint_line(""), fingerprint_line("> a"));
    }

    /// **清屏就地擦**的核心用例:`history` 没动、锚点算术照样成立,但那一行已经空了。
    /// 只有指纹测得到,[`is_saturated`] 一条都测不到。
    #[test]
    fn 锚点行被就地擦掉时判废() {
        let mut list = Vec::new();
        marker(&mut list, "> 第一问", 100);
        marker(&mut list, "> 第二问", 140);

        // scrollback 远没装满,老判据放行
        assert!(!prune(&mut list, 500, 10_000), "算术判据测不到这一类");

        // Claude Code 的 /new:视口那一屏被逐行 2K 抹白,history 一动不动
        let 清屏后 = |anchor: i32| -> Option<u64> {
            // 140 那条落在视口里、被擦成空行;100 那条已经滚进 scrollback,完好
            if anchor == 140 {
                Some(fingerprint_line(""))
            } else {
                Some(fingerprint_line("> 第一问"))
            }
        };
        assert!(prune_stale(&mut list, 清屏后), "视口内那条必须被剪掉");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].line, "> 第一问", "scrollback 里的那条内容完好,必须留住");

        // 再跑一遍是空操作 —— 剩下的那条仍然对得上
        assert!(!prune_stale(&mut list, 清屏后));
    }

    /// `ESC[3J` / RIS 把 history 清零 → 锚点整体越界,`probe` 交回 `None`,全剪。
    #[test]
    fn 锚点越界时判废() {
        let mut list = Vec::new();
        marker(&mut list, "> a", 100);
        marker(&mut list, "> b", 140);
        assert!(prune_stale(&mut list, |_| None));
        assert!(list.is_empty());
        assert!(!prune_stale(&mut list, |_| None), "空列表上是空操作");
    }

    /// **还挂着的条目一条都不剪**。这是「AI 忙时追加的那句在下拉里凭空消失」那个
    /// bug 的正面用例:它没定过锚,拿任何指纹去比都是错的,只能留着等补锚。
    #[test]
    fn 挂起的条目不参与指纹校验() {
        let mut list = Vec::new();
        pending(&mut list, "AI 忙的时候追加的这句", 100);
        marker(&mut list, "> b", 140);

        // 探针对 100 交回任意内容(挂着的条目本来就没有「原来那行」可言)
        assert!(!prune_stale(&mut list, |anchor| Some(fingerprint_line(
            if anchor == 100 { "每帧都在变的动态区" } else { "> b" }
        ))));
        assert_eq!(list.len(), 2, "挂着的那条必须留住");
        assert!(list[0].anchor.is_pending());

        // 连探针交回 None(那一行压根不在缓冲区里)也不剪它
        let mut only_pending = Vec::new();
        pending(&mut only_pending, "还在队列里", 100);
        assert!(!prune_stale(&mut only_pending, |_| None));
        assert_eq!(only_pending.len(), 1);
    }

    /// 内容没被动过就一条都不剪 —— 正常滚动下 `prune_stale` 必须是纯粹的空操作,
    /// 否则每次跳转都会把好好的标记吃掉。
    #[test]
    fn 内容未变时不剪任何东西() {
        let mut list = Vec::new();
        marker(&mut list, "> a", 100);
        marker(&mut list, "> b", 140);
        marker(&mut list, "> c", 180);
        let 原样 = |anchor: i32| -> Option<u64> {
            Some(fingerprint_line(match anchor {
                100 => "> a",
                140 => "> b",
                _ => "> c",
            }))
        };
        assert!(!prune_stale(&mut list, 原样));
        assert_eq!(list.len(), 3);
    }

    // ---- 定锚判定与补锚(模块注释的「第三个破绽」)----

    /// AI 空闲时提交:那 200ms 里 `> 用户输入` 已经打出来了,水位正落在它身上 ——
    /// 判 `Settled`,与打这个补丁之前一模一样。
    #[test]
    fn 锚点行看得见正文时正常定锚() {
        let anchor = settle_anchor(42, Some("> 帮我看看这段代码"), &["帮我看看这段代码"]);
        assert_eq!(
            anchor,
            MarkerAnchor::Settled {
                anchor: 42,
                fingerprint: fingerprint_line("> 帮我看看这段代码"),
            }
        );
        assert_eq!(anchor.settled(), Some(42));
    }

    /// **本次修复的核心用例**:AI 正在输出时追加一句,那句被排进队列、根本还没上屏,
    /// 水位只落得到还在重绘的动态区上 —— 判 `Pending`,而不是拿动态区的指纹当锚点
    /// (那样下一次 `prune_stale` 就把这条剪没了)。
    #[test]
    fn 正文还没上屏时挂起而不是乱定锚() {
        let anchor = settle_anchor(
            42,
            Some("✻ Thinking… (12s · ↓ 1.2k tokens)"),
            &["顺便把测试也补上"],
        );
        assert_eq!(anchor, MarkerAnchor::Pending { from: 42 });
        assert_eq!(anchor.settled(), None, "挂着的没有行可跳");
        assert!(anchor.is_pending());
    }

    /// 锚点行压根读不到(不在缓冲区里)同样挂起 —— 留着定不住的锚点只会指向错误的行。
    #[test]
    fn 锚点行读不到时挂起() {
        assert_eq!(
            settle_anchor(42, None, &["帮我看看这段代码"]),
            MarkerAnchor::Pending { from: 42 }
        );
    }

    /// **判不了的时候一律照旧定锚**:正文短到 `MIN_MATCH_CHARS` 以下时,随便哪行
    /// AI 输出都能撞上,拿它判挂起只会把本来定得好好的锚点误挂起来。
    #[test]
    fn 正文太短时不做挂起判定() {
        for (text, submit) in [
            (Some("完全不相干的一行"), "y"),
            (Some("完全不相干的一行"), "ok"),
            (None, "ok"),
        ] {
            let anchor = settle_anchor(7, text, &[submit]);
            assert!(
                !anchor.is_pending(),
                "{submit:?} 太短,应该退回补丁之前的行为而不是挂起"
            );
        }
    }

    /// 一次 drain 的多条提交共用一个锚点:**任意一条**对得上就算定住。
    #[test]
    fn 一批里任一条正文对得上就定锚() {
        let anchor = settle_anchor(9, Some("> 第二个问题"), &["第一个问题", "第二个问题"]);
        assert_eq!(anchor.settled(), Some(9));
    }

    /// 行式 CLI 的回显不是 `> 正文` 而是 `You: 正文` 这类前缀剥不掉的形态 ——
    /// 定锚判据用的是「包含」,不能因为剥不掉前缀就把它误判成挂起(那是实打实的回归)。
    #[test]
    fn 剥不掉的前缀不影响定锚() {
        for text in [
            "You: 帮我看看这段代码",
            "PS D:\\Git\\mini-term> 帮我看看这段代码",
            "│ ❯ 帮我看看这段代码",
        ] {
            assert!(
                settle_anchor(3, Some(text), &["帮我看看这段代码"])
                    .settled()
                    .is_some(),
                "{text:?} 里看得见正文,不该挂起"
            );
        }
    }

    /// 长输入被折行时块首只显示得下前半段 —— 反向前缀那一支要认。
    #[test]
    fn 折行只显示前半段时也算定住() {
        let submit = "帮我把这个函数拆成三个小函数并补上单元测试";
        assert!(
            settle_anchor(3, Some("> 帮我把这个函数拆成三个"), &[submit])
                .settled()
                .is_some()
        );
    }

    /// 补锚:AI 把队列里那条处理掉、消息落到屏幕上之后,回扫就能把它认回来。
    #[test]
    fn 补锚把挂起的条目转正() {
        let mut list = Vec::new();
        pending(&mut list, "顺便把测试也补上", 100);

        // 还没上屏:扫一遍什么都找不到,继续挂着
        let 队列中 = screen(&[(100, "✻ Thinking…"), (101, "> 上一个问题")]);
        assert!(!relocate_pending(&mut list, 101, NO_ADVANCE, &队列中));
        assert!(list[0].anchor.is_pending());

        // AI 处理到它了,消息打在 105 行
        let 已上屏 = screen(&[
            (100, "✻ Thinking…"),
            (103, "工具调用结果"),
            (105, "> 顺便把测试也补上"),
            (106, "✻ Working…"),
        ]);
        assert!(relocate_pending(&mut list, 106, NO_ADVANCE, &已上屏));
        assert_eq!(
            list[0].anchor,
            MarkerAnchor::Settled {
                anchor: 105,
                fingerprint: fingerprint_line("> 顺便把测试也补上"),
            }
        );
        // 转正之后指纹校验必须放行(补锚与校验共用同一个取法)
        assert!(!prune_stale(&mut list, |anchor| 已上屏(anchor)
            .as_deref()
            .map(fingerprint_line)));
        assert_eq!(list.len(), 1);
    }

    /// 补锚**不往起点上方找**:那上面是提交之前的内容,同一句话提交过两次的话
    /// 会认到上一次那条身上。
    #[test]
    fn 补锚不越过起点往上找() {
        let mut list = Vec::new();
        pending(&mut list, "再跑一遍测试", 50);
        let 屏幕 = screen(&[(10, "> 再跑一遍测试"), (60, "> 再跑一遍测试")]);
        assert!(relocate_pending(&mut list, 60, NO_ADVANCE, &屏幕));
        assert_eq!(list[0].anchor.settled(), Some(60), "认的必须是起点之后那条");
    }

    /// 多条挂起按**提交顺序**依次向下认,同一句话追加两次也不会互相抢同一行。
    #[test]
    fn 多条挂起按顺序依次补锚() {
        let mut list = Vec::new();
        pending(&mut list, "再跑一遍测试", 50);
        pending(&mut list, "再跑一遍测试", 50);
        let 屏幕 = screen(&[(60, "> 再跑一遍测试"), (72, "> 再跑一遍测试")]);
        assert!(relocate_pending(&mut list, 80, NO_ADVANCE, &屏幕));
        assert_eq!(list[0].anchor.settled(), Some(60));
        assert_eq!(list[1].anchor.settled(), Some(72), "第二条不能抢第一条那行");
    }

    /// 一条永远等不到(用户 Esc 掉了排队里的它)**不能把后面的一起冻住**。
    #[test]
    fn 一条补不上不拦着后面的() {
        let mut list = Vec::new();
        pending(&mut list, "这句被 Esc 掉了", 50);
        pending(&mut list, "这句真的发出去了", 50);
        let 屏幕 = screen(&[(60, "> 这句真的发出去了")]);
        assert!(relocate_pending(&mut list, 80, NO_ADVANCE, &屏幕));
        assert!(list[0].anchor.is_pending(), "等不到的继续挂着");
        assert_eq!(list[1].anchor.settled(), Some(60), "后面那条照样要补上");
    }

    /// 补不上时把起点推到**已经扫过的地方**,下次只扫新长出来的行 —— 否则一条
    /// 永远等不到的挂起(用户 Esc 掉了它)会让每次开下拉都全量重扫一遍 scrollback。
    ///
    /// 推进时**留最后一屏不算已扫**:提交那一下的 `eraseLines` 会把光标顶回块首,
    /// 那条 static 有可能就打在这一屏之内,一路推到底会正好把它跳过去。
    #[test]
    fn 补不上时把起点推到已扫过的地方() {
        let mut list = Vec::new();
        pending(&mut list, "等不到的这句", 100);
        let 空屏 = screen(&[]);

        // bottom = 1000、屏高 24 → 起点推到 1000 - 24 + 1 = 977,最后一屏留着
        assert!(!relocate_pending(&mut list, 1000, 24, &空屏), "没补上不算变过");
        assert_eq!(list[0].anchor, MarkerAnchor::Pending { from: 977 });

        // 缓冲区还没长够一屏时不推进(算出来的起点比原来还靠上)
        let mut 短的 = Vec::new();
        pending(&mut 短的, "等不到的这句", 100);
        assert!(!relocate_pending(&mut 短的, 110, 24, &空屏));
        assert_eq!(短的[0].anchor, MarkerAnchor::Pending { from: 100 }, "不能往回退");

        // 留下的那一屏里后来打出了那条消息 —— 推进过也照样找得到
        let 上屏了 = screen(&[(990, "> 等不到的这句")]);
        assert!(relocate_pending(&mut list, 1000, 24, &上屏了));
        assert_eq!(list[0].anchor.settled(), Some(990));
    }

    /// 已定锚的条目不参与回扫 —— 补锚只管从来没定过锚的那些。
    #[test]
    fn 补锚不碰已经定好的条目() {
        let mut list = Vec::new();
        marker(&mut list, "> a", 10);
        let 屏幕 = screen(&[(60, "> a")]);
        assert!(!relocate_pending(&mut list, 80, NO_ADVANCE, &屏幕));
        assert_eq!(list[0].anchor.settled(), Some(10), "不该被挪到 60 去");
    }

    /// 回扫的判据比定锚严(要双向前缀,不是「包含」):在几千行里挑一行,放宽就是
    /// 跳到不相干的地方。
    #[test]
    fn 回扫判据认前缀不认夹在中间() {
        let mut list = Vec::new();
        pending(&mut list, "跑一下测试", 0);
        // 「跑一下测试」夹在一行叙述里 —— 定锚那一步会认(只看一行),回扫不认
        let 叙述 = screen(&[(1, "我建议你先跑一下测试再提交")]);
        assert!(!relocate_pending(&mut list, 5, NO_ADVANCE, &叙述));
        assert!(list[0].anchor.is_pending());

        // 剥掉装饰之后正好是它,才认
        let 消息 = screen(&[(1, "我建议你先跑一下测试再提交"), (2, "❯ 跑一下测试")]);
        assert!(relocate_pending(&mut list, 5, NO_ADVANCE, &消息));
        assert_eq!(list[0].anchor.settled(), Some(2));
    }

    /// 装饰剥离只剥**行首**、只剥那几个符号,剥不动就原样返回。
    #[test]
    fn 行首装饰逐层剥掉() {
        assert_eq!(strip_line_decoration("  > hi"), "hi");
        assert_eq!(strip_line_decoration("│ ❯ hi"), "hi", "TUI 常常套两层");
        assert_eq!(strip_line_decoration("hi > there"), "hi > there", "只剥行首");
        assert_eq!(strip_line_decoration(""), "");
        assert_eq!(strip_line_decoration(">>>"), "");
    }

    /// 端到端:AI 忙时追加的那句,从提交到能跳的完整一条命。
    /// 修复前它在第 3 步就被剪没了(下拉里根本没有这条)。
    #[test]
    fn 忙时追加的那句从挂起到可跳() {
        // ① 提交那一下,水位落在还在重绘的动态区上 → 挂起
        let anchor = settle_anchor(200, Some("✻ Compacting…"), &["顺便把测试也补上"]);
        assert!(anchor.is_pending());

        // ② 照样进列表 —— 用户看得到自己追加过什么
        let mut list = Vec::new();
        push_marker(&mut list, 7, "顺便把测试也补上".into(), 0, anchor);
        assert_eq!(list.len(), 1);

        // ③ 期间 AI 一直在吐东西,指纹校验一遍遍地跑,但一条都不许剪
        for _ in 0..5 {
            assert!(!prune_stale(&mut list, |_| Some(fingerprint_line("变了"))));
        }
        assert_eq!(list.len(), 1, "修复前就是死在这一步");

        // ④ AI 处理到它了,消息上屏 → 补锚转正,可跳
        let 屏幕 = screen(&[(240, "> 顺便把测试也补上")]);
        assert!(relocate_pending(&mut list, 260, NO_ADVANCE, &屏幕));
        assert_eq!(list[0].anchor.settled(), Some(240));
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
