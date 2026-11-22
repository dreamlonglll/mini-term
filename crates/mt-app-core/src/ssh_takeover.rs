//! 右键「SSH 连接」接管(内置 russh)的纯逻辑:后端路由表、能不能接管、接管前等基底首屏画完。
//!
//! # 接管是什么
//!
//! pane 有一个**基底**后端:本地 PTY(本地 shell,或系统 OpenSSH 回退的远程项目 pane —— PTY 里跑
//! ssh.exe),或内置 russh 的远程项目 pane。右键「SSH 连接」(连接没勾「终端使用系统 OpenSSH」)时
//! 在基底上叠一条内置 SSH 会话:基底**不杀**、它的输出关闸丢弃(见 [`crate::output_gate`]),终端交给
//! 叠加会话;叠加会话收场后,终端还原成接管前的样子(`TerminalEmulator::begin_takeover` /
//! `end_takeover`,远端内容留在回看里),再开闸。整个过程**不给基底发任何字节**。
//!
//! # 路由表([`route`])
//!
//! | 接管阶段 | 用户写入 | 终端应答 | 基底输出 | 基底退出 |
//! |---|---|---|---|---|
//! | 无 | 基底 | 基底 | 放行 | 当场上报 |
//! | 等首屏(还没拍快照) | 叠加层(预输入 / Ctrl+C 取消) | 基底 | 放行 | 取消接管,照常上报 |
//! | 连接中 | 叠加层(行编辑 / 预输入 / Ctrl+C 取消) | 丢弃 | 丢弃 | 挂到接管结束 |
//! | 在跑 | 叠加层 | 叠加层 | 丢弃 | 挂到接管结束 |
//!
//! - **resize**:基底一律照下(本地 ConPTY 与终端保持同尺寸,接管结束时快照按当前尺寸 reflow,两边
//!   才对得上),叠加层在场就一起下;
//! - **关 pane**:两层一起关;
//! - **终端应答**(emulator 产生的 DA / DSR / OSC 查询回话)给提问的那一方:等首屏时屏幕还归基底;
//!   连接中屏幕上只有本地画的提示,谁都没提问;
//! - **挂到接管结束**:接管期间基底的 shell 死了(被外部杀掉、远程基底断线),不能当场亮退出条 /
//!   重连遮罩盖住还活着的叠加会话,等叠加会话收场、屏幕还原之后再报。
//!
//! # 等首屏([`first_screen_step`])
//!
//! 「新开终端再连」时新终端的本地 PTY 还在后台起。快照要是在提示符画出来之前拍,拍到的是空屏,而
//! conhost 自己的屏幕模型里却有提示符 —— 接管结束写回空屏后 conhost 只发增量,提示符就再也画不回来。
//! 所以先等:基底就绪、光标停在一行字后面(提示符画出来了:banner 之类整行输出完光标在行首)、输出
//! 静默 [`FIRST_SCREEN_QUIET`];最多等 [`FIRST_SCREEN_MAX_WAIT`](从基底就绪算起),到点照接不误。
//! 早就画完的终端(右键当前那个空闲终端)当场就接。

use std::time::{Duration, Instant};

use crate::output_gate::GateState;

/// 基底此刻的状态(宿主从 `PtySlot` / 基底链路换算)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseState {
    /// 本地 PTY 还在后台起。
    Starting,
    /// 内置 SSH 基底还在建连(可能正等用户答主机密钥 / 密码)。
    Connecting,
    Running,
    /// 退出了 / 起不来 / 已关闭。
    Dead,
}

/// 接管的阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverState {
    /// 等基底首屏画完,还没拍快照、还没开始建连。
    Waiting,
    /// 已拍快照,叠加会话在建连。
    Connecting,
    /// 叠加会话在跑。
    Running,
}

/// 写入 / 应答交给谁。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    Base,
    Overlay,
    Discard,
}

/// 基底退出了怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseExit {
    /// 当场上报(退出条 / 重连遮罩照旧)。
    Report,
    /// 还在等首屏:接管作罢,照常上报。
    CancelTakeover,
    /// 接管中:挂起,接管结束、屏幕还原后再报。
    Defer,
}

/// 一个接管阶段下的路由,见模块注释的表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// 用户写入(按键 / 粘贴 / IME / 移动端写穿)。
    pub input: Sink,
    /// 终端应答。
    pub reply: Sink,
    /// resize 要不要也下给叠加层(基底一律下)。
    pub resize_overlay: bool,
    /// 基底输出闸。
    pub base_output: GateState,
    pub base_exit: BaseExit,
}

/// 路由表,见模块注释。`None` = 没在接管。
pub fn route(takeover: Option<TakeoverState>) -> Route {
    match takeover {
        None => Route {
            input: Sink::Base,
            reply: Sink::Base,
            resize_overlay: false,
            base_output: GateState::Pass,
            base_exit: BaseExit::Report,
        },
        Some(TakeoverState::Waiting) => Route {
            input: Sink::Overlay,
            reply: Sink::Base,
            resize_overlay: true,
            base_output: GateState::Pass,
            base_exit: BaseExit::CancelTakeover,
        },
        Some(TakeoverState::Connecting) => Route {
            input: Sink::Overlay,
            reply: Sink::Discard,
            resize_overlay: true,
            base_output: GateState::Drop,
            base_exit: BaseExit::Defer,
        },
        Some(TakeoverState::Running) => Route {
            input: Sink::Overlay,
            reply: Sink::Overlay,
            resize_overlay: true,
            base_output: GateState::Drop,
            base_exit: BaseExit::Defer,
        },
    }
}

/// 右键「SSH 连接」(内置客户端)这一下能不能接管。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// 可以:等基底首屏画完再接(早就画完的当场接,见 [`first_screen_step`])。
    Wait,
    /// 已经在接管中:提示,不嵌套。
    AlreadyConnected,
    /// 基底还在建连(它自己的提示要用户答)或已经死了:提示,不接。
    Unavailable,
}

pub fn admit(base: BaseState, in_takeover: bool) -> Admission {
    if in_takeover {
        return Admission::AlreadyConnected;
    }
    match base {
        BaseState::Starting | BaseState::Running => Admission::Wait,
        BaseState::Connecting | BaseState::Dead => Admission::Unavailable,
    }
}

/// 首屏画完之后要静默多久才算「画完了」。
pub const FIRST_SCREEN_QUIET: Duration = Duration::from_millis(300);
/// 从基底就绪算起最多等多久,到点照接不误(提示符迟迟不出,或一直有输出)。
pub const FIRST_SCREEN_MAX_WAIT: Duration = Duration::from_secs(5);
/// 基底还在起 / 提示符还没画出来时多久再看一次。
pub const FIRST_SCREEN_POLL: Duration = Duration::from_millis(50);

/// [`first_screen_step`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstScreen {
    /// 现在就接。
    Ready,
    /// 过这么久再看一次。
    Wait(Duration),
    /// 基底死了(或换成了要用户答提示的建连):接管作罢。
    Abandon,
}

/// 现在能接了吗,见模块注释「等首屏」。
///
/// - `ready_at`:基底就绪(本地 PTY 回填 / 内置 SSH 开好 shell)的时刻;
/// - `last_output`:基底最近一次输出的时刻;
/// - `prompt_drawn`:光标停在一行字的后面(列号 > 0)—— 提示符画出来了。
pub fn first_screen_step(
    now: Instant,
    base: BaseState,
    ready_at: Option<Instant>,
    last_output: Option<Instant>,
    prompt_drawn: bool,
) -> FirstScreen {
    match base {
        BaseState::Dead | BaseState::Connecting => return FirstScreen::Abandon,
        BaseState::Starting => return FirstScreen::Wait(FIRST_SCREEN_POLL),
        BaseState::Running => {}
    }
    let ready_at = ready_at.unwrap_or(now);
    let waited = now.saturating_duration_since(ready_at);
    if waited >= FIRST_SCREEN_MAX_WAIT {
        return FirstScreen::Ready;
    }
    let since = last_output.map_or(ready_at, |at| at.max(ready_at));
    let quiet = now.saturating_duration_since(since);
    if prompt_drawn && quiet >= FIRST_SCREEN_QUIET {
        return FirstScreen::Ready;
    }
    let until_quiet = if prompt_drawn {
        FIRST_SCREEN_QUIET - quiet
    } else {
        FIRST_SCREEN_POLL
    };
    let until_max = FIRST_SCREEN_MAX_WAIT - waited;
    FirstScreen::Wait(until_quiet.min(until_max).max(Duration::from_millis(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路由表逐格对照模块注释。
    #[test]
    fn 路由表() {
        let none = route(None);
        assert_eq!(none.input, Sink::Base);
        assert_eq!(none.reply, Sink::Base);
        assert!(!none.resize_overlay);
        assert_eq!(none.base_output, GateState::Pass);
        assert_eq!(none.base_exit, BaseExit::Report);

        let waiting = route(Some(TakeoverState::Waiting));
        assert_eq!(
            waiting.input,
            Sink::Overlay,
            "等首屏时敲的字是给 SSH 会话的预输入"
        );
        assert_eq!(waiting.reply, Sink::Base, "屏幕还归基底,应答也给它");
        assert_eq!(
            waiting.base_output,
            GateState::Pass,
            "快照还没拍,基底照常画"
        );
        assert_eq!(waiting.base_exit, BaseExit::CancelTakeover);

        let connecting = route(Some(TakeoverState::Connecting));
        assert_eq!(connecting.input, Sink::Overlay);
        assert_eq!(connecting.reply, Sink::Discard, "连接中谁都没提问");
        assert_eq!(connecting.base_output, GateState::Drop);
        assert_eq!(connecting.base_exit, BaseExit::Defer);

        let running = route(Some(TakeoverState::Running));
        assert_eq!(running.input, Sink::Overlay);
        assert_eq!(running.reply, Sink::Overlay);
        assert_eq!(
            running.base_output,
            GateState::Drop,
            "基底的输出不进 emulator"
        );
        assert_eq!(running.base_exit, BaseExit::Defer);
        for state in [
            TakeoverState::Waiting,
            TakeoverState::Connecting,
            TakeoverState::Running,
        ] {
            assert!(route(Some(state)).resize_overlay, "{state:?}");
        }
    }

    #[test]
    fn 接管准入() {
        assert_eq!(admit(BaseState::Running, false), Admission::Wait);
        assert_eq!(
            admit(BaseState::Starting, false),
            Admission::Wait,
            "新开的终端:等它起来、画完首屏"
        );
        assert_eq!(
            admit(BaseState::Connecting, false),
            Admission::Unavailable,
            "基底自己的建连提示要用户答,不能抢走键盘"
        );
        assert_eq!(admit(BaseState::Dead, false), Admission::Unavailable);
        for base in [
            BaseState::Starting,
            BaseState::Connecting,
            BaseState::Running,
            BaseState::Dead,
        ] {
            assert_eq!(admit(base, true), Admission::AlreadyConnected, "{base:?}");
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// 早就画完、闲着的终端当场接。
    #[test]
    fn 闲着的终端当场接() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(60);
        assert_eq!(
            first_screen_step(now, BaseState::Running, Some(t0), Some(t0 + ms(800)), true),
            FirstScreen::Ready
        );
    }

    /// 新终端:还在起就隔一会儿再看;起来之后等提示符画出来、再静默 300ms。
    #[test]
    fn 新终端等提示符画完再静默一会儿() {
        let t0 = Instant::now();
        assert_eq!(
            first_screen_step(t0, BaseState::Starting, None, None, false),
            FirstScreen::Wait(FIRST_SCREEN_POLL)
        );
        let ready = t0 + ms(100);
        // 刚回填、还没输出:提示符没画出来,隔一会儿再看
        assert_eq!(
            first_screen_step(ready, BaseState::Running, Some(ready), None, false),
            FirstScreen::Wait(FIRST_SCREEN_POLL)
        );
        // banner 打完(光标在行首)、profile 还在加载:静默再久也不算画完
        assert_eq!(
            first_screen_step(
                ready + ms(900),
                BaseState::Running,
                Some(ready),
                Some(ready + ms(50)),
                false
            ),
            FirstScreen::Wait(FIRST_SCREEN_POLL)
        );
        // 提示符画出来了,才过 100ms:再等剩下的 200ms
        let prompt = ready + ms(1200);
        assert_eq!(
            first_screen_step(
                prompt + ms(100),
                BaseState::Running,
                Some(ready),
                Some(prompt),
                true
            ),
            FirstScreen::Wait(ms(200))
        );
        assert_eq!(
            first_screen_step(
                prompt + FIRST_SCREEN_QUIET,
                BaseState::Running,
                Some(ready),
                Some(prompt),
                true
            ),
            FirstScreen::Ready
        );
    }

    /// 一直有输出 / 提示符迟迟不出:从就绪算起最多等 5s。
    #[test]
    fn 最多等几秒() {
        let ready = Instant::now();
        let near = ready + FIRST_SCREEN_MAX_WAIT - ms(10);
        assert_eq!(
            first_screen_step(near, BaseState::Running, Some(ready), Some(near), true),
            FirstScreen::Wait(ms(10)),
            "等待不越过上限"
        );
        let late = ready + FIRST_SCREEN_MAX_WAIT;
        assert_eq!(
            first_screen_step(late, BaseState::Running, Some(ready), Some(late), false),
            FirstScreen::Ready
        );
    }

    #[test]
    fn 基底死了或换成建连就作罢() {
        let now = Instant::now();
        assert_eq!(
            first_screen_step(now, BaseState::Dead, Some(now), None, true),
            FirstScreen::Abandon
        );
        assert_eq!(
            first_screen_step(now, BaseState::Connecting, None, None, false),
            FirstScreen::Abandon
        );
    }

    /// 时钟往回跳(不该发生)也不 panic:输出时刻早于就绪时刻按就绪时刻算。
    #[test]
    fn 时刻错位不出错() {
        let ready = Instant::now() + ms(500);
        let step = first_screen_step(
            ready - ms(100),
            BaseState::Running,
            Some(ready),
            Some(ready - ms(400)),
            true,
        );
        assert_eq!(step, FirstScreen::Wait(FIRST_SCREEN_QUIET));
    }
}
