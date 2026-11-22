//! 一条内置 SSH 终端会话(mt-ssh 的 `ShellSession`)在 pane 里的生命周期:
//! **连接中 → 在跑 → 结束**。
//!
//! 远程项目 pane 的基底、右键「SSH 连接」接管时叠在基底上的那一层,用的都是它。本模块与 GPUI
//! 无关:会话类型经 [`ShellPort`] 抽象,提示的应答句柄是不透明的 `R`(宿主那边是 mt-remote 包着的
//! tokio oneshot),单测里都换成记账桩。
//!
//! # 连接中
//!
//! `open_shell` 在后台建连、认证,要显示 / 要问用户的东西经提示通道一条条送来([`PromptEvent`]):
//!
//! - `Notice`:原样画进终端(mt-ssh 已拼好 `\r\n`、已净化);
//! - `ReadLine`:画出提示,此后用户的写入**只**喂给 [`LineInput`](回显按它的口径,`echo = false`
//!   时只回显换行),回车 / Ctrl+C 读完一行,交出 [`Answer`] 由宿主应答给 SSH 层。这期间的写入
//!   **不进**任何旁路(AI 输入识别、`UserInput` 事件、移动端)—— 里面可能是密码;
//! - 正在读一行时又来了提示:排队,这一行读完再依次画(SSH 层其实是一问一答,队列只是兜底)。
//!
//! 没在读行时的写入:
//!
//! - 含 **Ctrl+C**(`\x03`)= 取消建连([`FeedAction::Cancel`]),链路当场落成结束;
//! - 其余字节进**预输入缓冲**(上限 [`MAX_TYPEAHEAD`] 字节,满了的丢掉):shell 就绪时交还宿主,
//!   像真 tty 一样「连上之前敲的字不丢」;但**一出现要读行的提示就清空** —— 与 OpenSSH 读口令前
//!   `TCSAFLUSH` 同一个道理:建连途中敲的 `ls⏎` 不能被当成口令交出去。清空时覆写成 0。
//!
//! 为什么预输入在冲刷时才过旁路、不像 [`crate::pty_slot`] 那样当场过:它可能被上面那条清掉,
//! 也可能随建连失败作废,当场喂给 AI 输入识别就会把从没进过 shell 的 `claude⏎` 当成一次启动。
//!
//! 终端尺寸:连接中只记最新一次;就绪时与开 pty 时报给服务器的尺寸不同,就补一次 resize。
//!
//! # 在跑 / 结束
//!
//! 就绪([`ShellLink::opened`])之后写入原样交给会话(宿主先过旁路再 [`ShellLink::write`]);
//! 结束(远端退出、连接断开、宿主关闭、建连失败或取消)之后一律丢弃。
//!
//! # 终端里的那几行字
//!
//! 「正在连接」「连接失败」「Connection to … closed.」这几行由宿主画进终端,措辞与颜色在本模块
//! ([`connecting_notice`] 等)。英文、OpenSSH 风格 —— 与 mt-ssh 给的提示(主机密钥提问、
//! `Permission denied` ……)是同一种语言,夹在一屏里不突兀;颜色只用 SGR 的调色板序号,跟着
//! 终端主题走(切主题时已经画上去的字也跟着变)。

use std::collections::VecDeque;
use std::fmt;

use zeroize::{Zeroize, Zeroizing};

use crate::line_input::{LineInput, LineOutcome};

/// 连接中、没在读行时,预输入缓冲最多攒多少字节(与 Linux N_TTY 的行缓冲同量级)。
pub const MAX_TYPEAHEAD: usize = 4 * 1024;

/// 会话一侧要用到的口。生产实现包着 mt-ssh 的 `ShellSession`(两个方法都只是入队,不阻塞)。
pub trait ShellPort {
    fn write(&self, bytes: &[u8]) -> Result<(), String>;
    /// 改 pty 尺寸,参数顺序同 PTY 一侧:`(rows, cols)`。返回是否真的下发了(同尺寸去重)。
    fn resize(&self, rows: u16, cols: u16) -> Result<bool, String>;
}

/// SSH 层建连期间送来的一条提示(宿主把 mt-ssh 的 `ShellPrompt` 换成这个)。
pub enum PromptEvent<R> {
    /// 原样画进终端的文本。
    Notice(String),
    /// 读一行:`prompt` 原样显示,`echo = false` 时不回显;读完用 `reply` 应答。
    ReadLine {
        prompt: String,
        echo: bool,
        reply: R,
    },
}

impl<R> fmt::Debug for PromptEvent<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Notice(text) => f.debug_tuple("Notice").field(text).finish(),
            Self::ReadLine { prompt, echo, .. } => f
                .debug_struct("ReadLine")
                .field("prompt", prompt)
                .field("echo", echo)
                .finish_non_exhaustive(),
        }
    }
}

/// 读完的一行,交给 SSH 层。`line = None` = 用户取消(Ctrl+C / 空行上的 Ctrl+D)。
///
/// 明文套在 [`Zeroizing`] 里:宿主取走(`std::mem::take`)交给应答通道,没取走就随它清零。
pub struct Answer<R> {
    pub reply: R,
    pub line: Option<Zeroizing<String>>,
}

impl<R> fmt::Debug for Answer<R> {
    /// 不打印内容:里面可能是密码。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Answer")
            .field("cancelled", &self.line.is_none())
            .finish_non_exhaustive()
    }
}

/// 一次写入的去向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedAction {
    /// 连接中,被行编辑或预输入缓冲吃掉了。
    Consumed,
    /// 连接中、没在读行时按了 Ctrl+C:取消建连。链路已落成结束,宿主中止建连任务并呈现取消。
    Cancel,
    /// 会话在跑:宿主先过旁路,再 [`ShellLink::write`]。
    Forward,
    /// 已结束:丢弃。
    Dropped,
}

/// [`ShellLink::feed`] 的结果。
pub struct Feed<R> {
    /// 要画进终端的字节(行编辑回显、接着该显示的下一条提示),按序。
    pub show: Vec<u8>,
    /// 这次写入读完了一行。
    pub answer: Option<Answer<R>>,
    pub action: FeedAction,
}

impl<R> fmt::Debug for Feed<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Feed")
            .field("show", &String::from_utf8_lossy(&self.show))
            .field("answer", &self.answer)
            .field("action", &self.action)
            .finish()
    }
}

/// [`ShellLink::opened`] 的产物。
#[derive(Debug)]
pub struct Opened {
    /// 连接中攒下的预输入,宿主过旁路后交给会话。
    pub typeahead: Vec<u8>,
    /// 就绪时补下发的那次 resize(`None` = 尺寸没变过)。
    pub resized: Option<Result<bool, String>>,
}

/// 对外可见的阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPhase {
    Connecting,
    Running,
    Ended,
}

enum State<S, R> {
    Connecting(Connecting<R>),
    Running(S),
    Ended,
}

struct Connecting<R> {
    /// 正在读的那一行。
    active: Option<Active<R>>,
    /// 读行期间又送来的提示。
    queue: VecDeque<PromptEvent<R>>,
    typeahead: Vec<u8>,
    /// 开 pty 时报给服务器的尺寸 `(rows, cols)`。
    requested: (u16, u16),
    /// 最新一次的尺寸 `(rows, cols)`。
    latest: (u16, u16),
}

struct Active<R> {
    input: LineInput,
    reply: R,
}

impl<R> Drop for Connecting<R> {
    fn drop(&mut self) {
        // 预输入里可能有用户抢先敲的口令
        self.typeahead.zeroize();
    }
}

impl<R> Connecting<R> {
    /// 画一条提示;是读行的就让它成为当前行(先清掉预输入)。调用前 `active` 必须是空的。
    fn present(&mut self, event: PromptEvent<R>, show: &mut Vec<u8>) {
        match event {
            PromptEvent::Notice(text) => show.extend_from_slice(text.as_bytes()),
            PromptEvent::ReadLine {
                prompt,
                echo,
                reply,
            } => {
                self.typeahead.zeroize();
                show.extend_from_slice(prompt.as_bytes());
                self.active = Some(Active {
                    input: LineInput::new(echo),
                    reply,
                });
            }
        }
    }

    /// 当前行读完了:把排着的提示依次画出来,直到下一条读行。
    fn pump(&mut self, show: &mut Vec<u8>) {
        while self.active.is_none() {
            let Some(event) = self.queue.pop_front() else {
                break;
            };
            self.present(event, show);
        }
    }
}

/// 一条内置 SSH 终端会话,见模块注释。
pub struct ShellLink<S, R> {
    state: State<S, R>,
}

impl<S, R> fmt::Debug for ShellLink<S, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellLink")
            .field("phase", &self.phase())
            .field("prompting", &self.is_prompting())
            .finish()
    }
}

impl<S, R> ShellLink<S, R> {
    /// 新链路:连接中,尺寸先记成 `(rows, cols)`。
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            state: State::Connecting(Connecting {
                active: None,
                queue: VecDeque::new(),
                typeahead: Vec::new(),
                requested: (rows, cols),
                latest: (rows, cols),
            }),
        }
    }

    pub fn phase(&self) -> LinkPhase {
        match self.state {
            State::Connecting(_) => LinkPhase::Connecting,
            State::Running(_) => LinkPhase::Running,
            State::Ended => LinkPhase::Ended,
        }
    }

    /// 正在终端里读一行(主机密钥 / 密码 / OTP / 私钥口令)。
    pub fn is_prompting(&self) -> bool {
        matches!(&self.state, State::Connecting(c) if c.active.is_some())
    }

    /// 起 `open_shell` 之前取开 pty 用的尺寸 `(rows, cols)`:就是此刻记下的最新尺寸,并记成
    /// 「报给服务器的尺寸」。不在连接中返回 `None`。
    pub fn request_size(&mut self) -> Option<(u16, u16)> {
        match &mut self.state {
            State::Connecting(c) => {
                c.requested = c.latest;
                Some(c.latest)
            }
            _ => None,
        }
    }

    /// SSH 层送来一条提示。返回要画进终端的字节(读行期间到的先排队,返回空)。
    ///
    /// 不在连接中(已就绪 / 已结束)时整条丢掉:读行的应答句柄随之丢弃,SSH 层按取消处理。
    pub fn prompt(&mut self, event: PromptEvent<R>) -> Vec<u8> {
        let State::Connecting(c) = &mut self.state else {
            return Vec::new();
        };
        if c.active.is_some() {
            c.queue.push_back(event);
            return Vec::new();
        }
        let mut show = Vec::new();
        c.present(event, &mut show);
        show
    }

    /// 用户的一次写入(按键 / 粘贴 / IME 上屏 / 移动端写穿),去向见 [`FeedAction`]。
    pub fn feed(&mut self, bytes: &[u8]) -> Feed<R> {
        let mut feed = Feed {
            show: Vec::new(),
            answer: None,
            action: FeedAction::Consumed,
        };
        let mut cancel = false;
        match &mut self.state {
            State::Running(_) => feed.action = FeedAction::Forward,
            State::Ended => feed.action = FeedAction::Dropped,
            State::Connecting(c) => {
                if let Some(active) = c.active.as_mut() {
                    let step = active.input.feed(bytes);
                    feed.show = step.echo;
                    if let Some(outcome) = step.outcome
                        && let Some(Active { reply, .. }) = c.active.take()
                    {
                        let line = match outcome {
                            LineOutcome::Submit(line) => Some(Zeroizing::new(line)),
                            LineOutcome::Cancel => None,
                        };
                        feed.answer = Some(Answer { reply, line });
                        c.pump(&mut feed.show);
                    }
                } else if bytes.contains(&0x03) {
                    feed.show.extend_from_slice(b"^C\r\n");
                    feed.action = FeedAction::Cancel;
                    cancel = true;
                } else {
                    let room = MAX_TYPEAHEAD.saturating_sub(c.typeahead.len());
                    c.typeahead
                        .extend_from_slice(&bytes[..bytes.len().min(room)]);
                }
            }
        }
        if cancel {
            // 排着的读行应答随之丢弃,预输入在 Drop 里清零
            self.state = State::Ended;
        }
        feed
    }

    /// 落成结束(建连失败 / 远端退出 / 宿主关闭)。在跑的会话交还调用方(关掉或丢弃)。
    pub fn end(&mut self) -> Option<S> {
        match std::mem::replace(&mut self.state, State::Ended) {
            State::Running(session) => Some(session),
            _ => None,
        }
    }

    pub fn session(&self) -> Option<&S> {
        match &self.state {
            State::Running(session) => Some(session),
            _ => None,
        }
    }
}

impl<S: ShellPort, R> ShellLink<S, R> {
    /// 建连成功,会话交回来了。连接中:落成在跑,交出预输入、补 resize;否则(已被取消 / 关闭)
    /// 原样退回,**调用方负责关掉它**。
    pub fn opened(&mut self, session: S) -> Result<Opened, S> {
        let State::Connecting(c) = &mut self.state else {
            return Err(session);
        };
        let typeahead = std::mem::take(&mut c.typeahead);
        let (requested, latest) = (c.requested, c.latest);
        self.state = State::Running(session);
        let resized = match &self.state {
            State::Running(session) if latest != requested => {
                Some(session.resize(latest.0, latest.1))
            }
            _ => None,
        };
        Ok(Opened { typeahead, resized })
    }

    /// 写进会话。只有在跑时才真的写;其余静默丢弃。
    pub fn write(&self, bytes: &[u8]) -> Result<(), String> {
        match &self.state {
            State::Running(session) => session.write(bytes),
            _ => Ok(()),
        }
    }

    /// 终端尺寸变了。连接中只记下来(返回 `false`,就绪时由 [`Opened::resized`] 交代);在跑时下发。
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<bool, String> {
        match &mut self.state {
            State::Connecting(c) => {
                c.latest = (rows, cols);
                Ok(false)
            }
            State::Running(session) => session.resize(rows, cols),
            State::Ended => Ok(false),
        }
    }
}

// ─── 终端里的那几行字 ─────────────────────────────────────────────

/// 把一段外来文本(建连错误、断线原因)变成可以安全画进终端的样子:换行统一成 `\r\n`、TAB 原样,
/// 其余控制字符(含 ESC 与 C1)写成 `\ooo` —— 错误文本里可能夹着服务器给的原话,不能让它在终端里
/// 执行转义序列。口径与 mt-ssh 的 `terminal_safe` 相同。
pub fn terminal_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\r\n");
            }
            '\n' => out.push_str("\r\n"),
            '\t' => out.push('\t'),
            c if c.is_control() => out.push_str(&format!("\\{:03o}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

/// 「正在连接」那一行(暗色):`Connecting to user@host:port...`。
pub fn connecting_notice(target: &str) -> String {
    format!(
        "\x1b[2mConnecting to {}...\x1b[0m\r\n",
        terminal_text(target)
    )
}

/// 建连失败 / 取消那一行(粗体红):原样是 `ShellOpenError.message`。
pub fn error_notice(message: &str) -> String {
    format!("\x1b[1;31m{}\x1b[0m\r\n", terminal_text(message.trim_end()))
}

/// 会话是怎么收场的(宿主从 mt-ssh 的 `ShellExit` 换算过来)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// 远端退出了 / 服务器关了 channel / 用户断开。
    Closed,
    /// 连接没了,附 mt-ssh 给的原因(OpenSSH 风格的原文)。
    Lost(String),
}

/// 收场那一行:`Connection to <host> closed.`(OpenSSH 原话),断线时是断线原因。
pub fn end_notice(host: &str, end: &SessionEnd) -> String {
    match end {
        SessionEnd::Closed => format!("Connection to {} closed.\r\n", terminal_text(host)),
        SessionEnd::Lost(reason) => format!("{}\r\n", terminal_text(reason.trim_end())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Write(String),
        Resize(u16, u16),
    }

    /// 记账桩:每次调用按序记进共享日志。
    struct FakeShell {
        log: Rc<RefCell<Vec<Call>>>,
    }

    impl ShellPort for FakeShell {
        fn write(&self, bytes: &[u8]) -> Result<(), String> {
            let text = String::from_utf8_lossy(bytes).into_owned();
            self.log.borrow_mut().push(Call::Write(text));
            Ok(())
        }

        fn resize(&self, rows: u16, cols: u16) -> Result<bool, String> {
            self.log.borrow_mut().push(Call::Resize(rows, cols));
            Ok(true)
        }
    }

    fn fake() -> (FakeShell, Rc<RefCell<Vec<Call>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        (FakeShell { log: log.clone() }, log)
    }

    /// 应答句柄的桩:一个编号,看得出是哪一条提示的应答。
    type Link = ShellLink<FakeShell, u32>;

    fn read_line(prompt: &str, echo: bool, reply: u32) -> PromptEvent<u32> {
        PromptEvent::ReadLine {
            prompt: prompt.into(),
            echo,
            reply,
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    fn answer_of(feed: Feed<u32>) -> (u32, Option<String>) {
        let answer = feed.answer.expect("这次写入应当读完一行");
        (answer.reply, answer.line.map(|line| line.to_string()))
    }

    #[test]
    fn 通知直接画_读行画出提示后按键只进行编辑() {
        let mut link = Link::new(24, 80);
        assert_eq!(
            text(&link.prompt(PromptEvent::Notice("Welcome\r\n".into()))),
            "Welcome\r\n"
        );
        assert!(!link.is_prompting());
        assert_eq!(
            text(&link.prompt(read_line("u@h's password: ", false, 7))),
            "u@h's password: "
        );
        assert!(link.is_prompting());

        // 不回显:只有回车那一下回显换行
        let feed = link.feed(b"s3");
        assert_eq!(feed.action, FeedAction::Consumed);
        assert!(feed.show.is_empty(), "密码不回显: {feed:?}");
        assert!(feed.answer.is_none());
        let feed = link.feed(b"cret\r");
        assert_eq!(text(&feed.show), "\r\n");
        assert_eq!(answer_of(feed), (7, Some("s3cret".into())));
        assert!(!link.is_prompting());
        assert_eq!(link.phase(), LinkPhase::Connecting);
    }

    #[test]
    fn 回显的提示照常回显_ctrl_c_取消这一行() {
        let mut link = Link::new(24, 80);
        link.prompt(read_line("Are you sure (yes/no)? ", true, 1));
        assert_eq!(text(&link.feed(b"ye").show), "ye");
        let feed = link.feed(b"\x03");
        assert_eq!(
            feed.action,
            FeedAction::Consumed,
            "读行里的 Ctrl+C 只取消这一行"
        );
        assert_eq!(text(&feed.show), "^C\r\n");
        assert_eq!(answer_of(feed), (1, None));
        // 链路仍在连接中:等 SSH 层报「已取消」再收场
        assert_eq!(link.phase(), LinkPhase::Connecting);
    }

    /// 读行期间又来的提示排队,这一行读完才依次画;通知一路画到下一条读行为止。
    #[test]
    fn 读行期间到的提示排队_读完后依次画() {
        let mut link = Link::new(24, 80);
        link.prompt(read_line("first: ", true, 1));
        assert!(
            link.prompt(PromptEvent::Notice("note\r\n".into()))
                .is_empty()
        );
        assert!(link.prompt(read_line("second: ", false, 2)).is_empty());
        assert!(
            link.prompt(PromptEvent::Notice("tail\r\n".into()))
                .is_empty()
        );

        let feed = link.feed(b"a\r");
        assert_eq!(text(&feed.show), "a\r\nnote\r\nsecond: ");
        assert_eq!(answer_of(feed), (1, Some("a".into())));
        assert!(link.is_prompting(), "第二条读行接着成为当前行");

        let feed = link.feed(b"b\r");
        assert_eq!(text(&feed.show), "\r\ntail\r\n");
        assert_eq!(answer_of(feed), (2, Some("b".into())));
        assert!(!link.is_prompting());
    }

    /// 没在读行时按 Ctrl+C = 取消建连:链路当场结束,之后的写入一律丢弃。
    #[test]
    fn 没在读行时_ctrl_c_取消建连() {
        let mut link = Link::new(24, 80);
        link.feed(b"ls");
        let feed = link.feed(b"x\x03y");
        assert_eq!(feed.action, FeedAction::Cancel);
        assert_eq!(text(&feed.show), "^C\r\n");
        assert_eq!(link.phase(), LinkPhase::Ended);
        assert_eq!(link.feed(b"z").action, FeedAction::Dropped);
        // 已经取消了:SSH 层晚到的会话退回给调用方关掉
        let (shell, log) = fake();
        assert!(link.opened(shell).is_err());
        assert!(log.borrow().is_empty());
    }

    /// 预输入:连上之前敲的字攒着,就绪时交还;但一出现读行提示就清空(TCSAFLUSH)。
    #[test]
    fn 预输入在读行提示出现时清空_之后敲的就绪时交还() {
        let mut link = Link::new(24, 80);
        assert_eq!(link.feed(b"ls\r").action, FeedAction::Consumed);
        link.prompt(read_line("password: ", false, 1));
        link.feed(b"pw\r");
        // 答完口令、shell 还没开:这之后敲的才算预输入
        link.feed(b"whoami\r");
        let (shell, log) = fake();
        let Ok(opened) = link.opened(shell) else {
            panic!("连接中应当接住会话");
        };
        assert_eq!(text(&opened.typeahead), "whoami\r");
        assert!(opened.resized.is_none(), "尺寸没变过就不补 resize");
        assert!(
            log.borrow().is_empty(),
            "预输入交还宿主过旁路再写,不由链路直接写"
        );
        assert_eq!(link.phase(), LinkPhase::Running);
    }

    #[test]
    fn 预输入有上限() {
        let mut link = Link::new(24, 80);
        link.feed(&vec![b'a'; MAX_TYPEAHEAD - 2]);
        link.feed(b"bcdef");
        let (shell, _log) = fake();
        let Ok(opened) = link.opened(shell) else {
            panic!("连接中应当接住会话");
        };
        assert_eq!(opened.typeahead.len(), MAX_TYPEAHEAD);
        assert!(opened.typeahead.ends_with(b"aabc"), "满了之后的字节丢掉");
    }

    /// 连接中的 resize 只记最新一次;就绪时与开 pty 报的尺寸不同才补一次。
    #[test]
    fn 连接中改尺寸只记最新_就绪时补一次() {
        let mut link = Link::new(24, 80);
        // 宿主起 open_shell 前取尺寸:报给服务器的就是此刻的最新值
        link.resize(30, 100).unwrap();
        assert_eq!(link.request_size(), Some((30, 100)));
        assert!(!link.resize(40, 120).unwrap(), "连接中不算真的下发");
        link.resize(50, 160).unwrap();
        let (shell, log) = fake();
        let Ok(opened) = link.opened(shell) else {
            panic!("连接中应当接住会话");
        };
        assert!(matches!(opened.resized, Some(Ok(true))));
        assert_eq!(*log.borrow(), vec![Call::Resize(50, 160)]);
        assert_eq!(link.request_size(), None, "不在连接中了");

        // 改回开 pty 时的尺寸也不补
        let mut link = Link::new(24, 80);
        link.request_size();
        link.resize(30, 100).unwrap();
        link.resize(24, 80).unwrap();
        let (shell, log) = fake();
        let Ok(opened) = link.opened(shell) else {
            panic!("连接中应当接住会话");
        };
        assert!(opened.resized.is_none());
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn 在跑时写入转发_结束后丢弃() {
        let mut link = Link::new(24, 80);
        let (shell, log) = fake();
        assert!(link.opened(shell).is_ok());
        assert_eq!(link.feed(b"ls\r").action, FeedAction::Forward);
        link.write(b"ls\r").unwrap();
        assert!(link.resize(10, 20).unwrap());
        assert!(link.session().is_some());
        assert_eq!(
            *log.borrow(),
            vec![Call::Write("ls\r".into()), Call::Resize(10, 20)]
        );

        let session = link.end();
        assert!(session.is_some(), "在跑的会话交还调用方关掉");
        assert_eq!(link.phase(), LinkPhase::Ended);
        assert_eq!(link.feed(b"x").action, FeedAction::Dropped);
        link.write(b"x").unwrap();
        assert!(!link.resize(1, 1).unwrap());
        assert!(link.end().is_none(), "重复结束是空操作");
        assert_eq!(log.borrow().len(), 2, "结束之后一个字节都不再写");
    }

    /// 就绪之后再到的提示(不该发生)整条丢掉:读行的应答句柄随之丢弃。
    #[test]
    fn 不在连接中时提示丢弃() {
        let mut link = Link::new(24, 80);
        let (shell, _log) = fake();
        assert!(link.opened(shell).is_ok());
        assert!(link.prompt(read_line("late: ", true, 9)).is_empty());
        assert!(!link.is_prompting());
    }

    /// 建连期间排着的读行在就绪 / 结束时随之丢弃(应答句柄 drop = SSH 层按取消算)。
    #[test]
    fn 结束时排着的应答句柄随之丢弃() {
        struct Reply(Rc<RefCell<u32>>);
        impl Drop for Reply {
            fn drop(&mut self) {
                *self.0.borrow_mut() += 1;
            }
        }
        let dropped = Rc::new(RefCell::new(0));
        let mut link: ShellLink<FakeShell, Reply> = ShellLink::new(24, 80);
        link.prompt(PromptEvent::ReadLine {
            prompt: "a: ".into(),
            echo: true,
            reply: Reply(dropped.clone()),
        });
        link.prompt(PromptEvent::ReadLine {
            prompt: "b: ".into(),
            echo: true,
            reply: Reply(dropped.clone()),
        });
        assert_eq!(*dropped.borrow(), 0);
        assert!(link.end().is_none());
        assert_eq!(*dropped.borrow(), 2);
    }

    #[test]
    fn 应答的调试输出不含明文() {
        let answer = Answer {
            reply: 1u32,
            line: Some(Zeroizing::new("hunter2".to_string())),
        };
        let shown = format!("{answer:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        let feed = Feed {
            show: Vec::new(),
            answer: Some(answer),
            action: FeedAction::Consumed,
        };
        assert!(!format!("{feed:?}").contains("hunter2"));
    }

    #[test]
    fn 外来文本净化后才画进终端() {
        assert_eq!(terminal_text("a\nb\r\nc\rd\te"), "a\r\nb\r\nc\r\nd\te");
        assert_eq!(
            terminal_text("\u{1b}]52;c;AAAA\u{7}x"),
            "\\033]52;c;AAAA\\007x",
            "OSC 52 之类的序列不能在终端里执行"
        );
        assert_eq!(terminal_text("\u{9b}2J 中文"), "\\2332J 中文");
    }

    #[test]
    fn 终端里的几行字() {
        assert_eq!(
            connecting_notice("root@10.0.0.5:22"),
            "\x1b[2mConnecting to root@10.0.0.5:22...\x1b[0m\r\n"
        );
        assert_eq!(
            error_notice("Permission denied (password).\n"),
            "\x1b[1;31mPermission denied (password).\x1b[0m\r\n",
            "结尾的换行不重复"
        );
        assert_eq!(
            end_notice("10.0.0.5", &SessionEnd::Closed),
            "Connection to 10.0.0.5 closed.\r\n"
        );
        assert_eq!(
            end_notice(
                "h",
                &SessionEnd::Lost("Timeout, server h not responding.".into())
            ),
            "Timeout, server h not responding.\r\n"
        );
    }
}
