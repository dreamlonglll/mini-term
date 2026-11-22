//! pane 手上那条 PTY 会话的「后台起 → 主线程回填」状态机。
//!
//! # 为什么要有它
//!
//! 起 PTY 已挪出 GPUI 主线程(见 [`crate::pane::TerminalPane::new`]):portable-pty
//! 解析 `pwsh` / `cmd` 这类裸名要逐个 PATH 目录 × PATHEXT 去 stat,还要 `is_dir(cwd)`、
//! 建 ConPTY、起子进程;续接还得先翻 `~/.claude/projects` 反查会话目录。于是 pane
//! 建出来的那一刻**会话还不存在**,而这段空窗里 pane 照样收得到写入与 resize ——
//! 用户抢先键入 / 粘贴、`hydrate_project` 紧跟着写进来的续接命令、终端应答、
//! 「SSH 连接」菜单的「写入 + 注册自动填充」…… 一条都不能丢,也不能乱序。
//!
//! 本模块只管「会话在不在、不在时东西攒在哪、回来时按什么顺序交出去」,
//! **与 GPUI 无关**,会话类型经 [`PtyPort`] 抽象,单测里换成记账桩。
//!
//! # 状态
//!
//! ```text
//!             ┌──backfill──→ Running(会话) ──close──┐
//! Starting ───┤                                     ├──→ Closed
//! (排队中)    ├──fail──────→ Failed                 │
//!             └──close─────────────────────────────┘
//! ```
//!
//! - **Starting**:写入按到达顺序进队列;resize 只记最新一次;子进程退出的通知
//!   先挂着(见下)。
//! - **backfill**:先补一次 resize(空窗期收到过尺寸的话),再按原顺序冲刷队列,
//!   最后把挂着的退出交还调用方 —— 退出永远排在回填之后上报,不会出现「pane 还没
//!   有会话就先被标成已退出」的怪状态。
//! - **Failed / Closed**:写入与 resize 静默丢弃(与此前「没有 PTY 时静默不做」同
//!   口径)。**Closed 之后再回填的会话一律退回给调用方丢弃** —— `PtySession` 的
//!   `Drop` 会当场杀子进程,不留孤儿。
//!
//! # 「写入 + arm」为什么是一个队列元素
//!
//! 「SSH 连接」菜单是先写 `ssh …\r` 再注册 `disarm_on_input = true` 的自动填充
//! ([`mt_pty::PtySession::write_then_arm_ssh_autofill`] 持锁跨过这两步)。排队时要是
//! 拆成两条,冲刷期间就可能在两步之间插进别的写入;更糟的是拆开后先到的那条写入
//! 会把后注册的 autofill 顺序搞反。所以它整体是**一个**元素,冲刷时原样交给那个
//! 原子方法。

use anyhow::Result;

/// 会话一侧要用到的四个口。生产实现是 [`mt_pty::PtySession`];抽出来只为单测。
pub trait PtyPort {
    /// 用户输入(观察器旁路 + 解除 SSH 自动填充,见 `PtySession::write`)。
    fn write(&self, bytes: &[u8]) -> Result<()>;
    /// 终端自己的应答(不解除自动填充、不经观察器)。
    fn write_reply(&self, bytes: &[u8]) -> Result<()>;
    /// 「SSH 连接」菜单:写入命令行,**写完再**注册自动填充。
    fn write_then_arm_ssh_autofill(&self, bytes: &[u8], password: String) -> Result<()>;
    /// 返回是否真的下发了 resize(同尺寸去重)。
    fn resize_if_changed(&self, rows: u16, cols: u16) -> Result<bool>;
}

impl PtyPort for mt_pty::PtySession {
    fn write(&self, bytes: &[u8]) -> Result<()> {
        mt_pty::PtySession::write(self, bytes)
    }

    fn write_reply(&self, bytes: &[u8]) -> Result<()> {
        mt_pty::PtySession::write_reply(self, bytes)
    }

    fn write_then_arm_ssh_autofill(&self, bytes: &[u8], password: String) -> Result<()> {
        mt_pty::PtySession::write_then_arm_ssh_autofill(self, bytes, password)
    }

    fn resize_if_changed(&self, rows: u16, cols: u16) -> Result<bool> {
        mt_pty::PtySession::resize_if_changed(self, rows, cols)
    }
}

/// 回填前攒下的一次写入。三种写法对应会话上三个不同的口,冲刷时各走各的。
enum PendingWrite {
    Input(Vec<u8>),
    Reply(Vec<u8>),
    /// 「写入 + 注册自动填充」:原子单元,见模块注释。
    InputThenArm(Vec<u8>, String),
}

/// 会话还在后台起的时候攒下的一切。
#[derive(Default)]
pub struct Pending {
    writes: Vec<PendingWrite>,
    /// 最新一次 `(rows, cols)`。只留最后一次:中间尺寸下发出去只会让 TUI 多重绘几遍。
    size: Option<(u16, u16)>,
    /// 空窗期里到达的退出通知(外层 `Option` = 有没有到过)。
    exit: Option<Option<u32>>,
}

/// 对外可见的阶段(给「是否存活」一类的查询用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyPhase {
    /// 后台还在起(或还在反查启动目录 / 做远程预检)。
    Starting,
    Running,
    /// 起失败(含远程预检失败)。错误原文在 pane 的 `spawn_error` 上。
    Failed,
    /// pane 已关闭。
    Closed,
}

pub enum PtySlot<S> {
    Starting(Pending),
    Running(S),
    Failed,
    Closed,
}

/// 回填的产物:调用方据此补 AI 感知旁路、打日志、补发退出事件。
pub struct Backfilled {
    /// 回填时补下发的那次 resize(`None` = 空窗期没收到过尺寸)。
    pub resized: Option<Result<bool>>,
    /// 冲刷队列时失败的写入(逐条,调用方打日志)。
    pub write_errors: Vec<anyhow::Error>,
    /// 空窗期里挂着的退出通知,**回填完成后**再上报。
    pub exit: Option<Option<u32>>,
}

impl<S: PtyPort> PtySlot<S> {
    /// 新 pane 的初始态:会话在后台起。
    pub fn starting() -> Self {
        Self::Starting(Pending::default())
    }

    pub fn phase(&self) -> PtyPhase {
        match self {
            Self::Starting(_) => PtyPhase::Starting,
            Self::Running(_) => PtyPhase::Running,
            Self::Failed => PtyPhase::Failed,
            Self::Closed => PtyPhase::Closed,
        }
    }

    /// 还在起或已经在跑 —— 关 pane 时需要收尾(kill / 取消后台任务)的两种状态。
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Starting(_) | Self::Running(_))
    }

    /// 用户输入。
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Starting(pending) => {
                pending.writes.push(PendingWrite::Input(bytes.to_vec()));
                Ok(())
            }
            Self::Running(session) => session.write(bytes),
            Self::Failed | Self::Closed => Ok(()),
        }
    }

    /// 终端应答(DA / DSR / OSC 查询的回话)。
    pub fn write_reply(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Starting(pending) => {
                pending.writes.push(PendingWrite::Reply(bytes.to_vec()));
                Ok(())
            }
            Self::Running(session) => session.write_reply(bytes),
            Self::Failed | Self::Closed => Ok(()),
        }
    }

    /// 「写入 + 注册 SSH 自动填充」。排队时是一个元素,见模块注释。
    pub fn write_then_arm_ssh_autofill(&mut self, bytes: &[u8], password: String) -> Result<()> {
        match self {
            Self::Starting(pending) => {
                pending
                    .writes
                    .push(PendingWrite::InputThenArm(bytes.to_vec(), password));
                Ok(())
            }
            Self::Running(session) => session.write_then_arm_ssh_autofill(bytes, password),
            Self::Failed | Self::Closed => Ok(()),
        }
    }

    /// grid 尺寸变了。返回是否**真的**下发了 resize —— 空窗期只记下来、返回
    /// `false`(真正的下发发生在回填时,由 [`Backfilled::resized`] 交代)。
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<bool> {
        match self {
            Self::Starting(pending) => {
                pending.size = Some((rows, cols));
                Ok(false)
            }
            Self::Running(session) => session.resize_if_changed(rows, cols),
            Self::Failed | Self::Closed => Ok(false),
        }
    }

    /// 子进程退出的通知到了。返回 `Some(code)` = 现在就上报;`None` = 先不报:
    /// 还在 Starting(挂起,回填后由 [`Backfilled::exit`] 交还),或 pane 已关闭 /
    /// 起失败(那条会话已不归这个 pane 管)。
    pub fn note_exit(&mut self, code: Option<u32>) -> Option<Option<u32>> {
        match self {
            Self::Starting(pending) => {
                pending.exit = Some(code);
                None
            }
            Self::Running(_) => Some(code),
            Self::Failed | Self::Closed => None,
        }
    }

    /// 后台起好的会话交回来了。
    ///
    /// - Starting:落成 Running,补 resize、按原顺序冲刷队列,交还挂起的退出;
    /// - 其余(实际只会是 Closed —— pane 在回填前关掉了):`Err(session)` 原样退回,
    ///   **调用方负责丢弃**(`PtySession::drop` 杀子进程)。
    pub fn backfill(&mut self, session: S) -> std::result::Result<Backfilled, S> {
        let pending = match self {
            Self::Starting(pending) => std::mem::take(pending),
            _ => return Err(session),
        };
        *self = Self::Running(session);
        let Self::Running(session) = self else {
            unreachable!("上一行刚落成 Running");
        };
        // resize 排在冲刷之前:空窗期里的写入(续接命令之类)按真实尺寸落地,
        // 与「PTY 早就在、尺寸早就对」时的效果一致。
        let resized = pending
            .size
            .map(|(rows, cols)| session.resize_if_changed(rows, cols));
        let mut write_errors = Vec::new();
        for write in pending.writes {
            let result = match write {
                PendingWrite::Input(bytes) => session.write(&bytes),
                PendingWrite::Reply(bytes) => session.write_reply(&bytes),
                PendingWrite::InputThenArm(bytes, password) => {
                    session.write_then_arm_ssh_autofill(&bytes, password)
                }
            };
            if let Err(err) = result {
                write_errors.push(err);
            }
        }
        Ok(Backfilled {
            resized,
            write_errors,
            exit: pending.exit,
        })
    }

    /// 后台起失败了。只有 Starting 才落成 Failed(返回 `true`,调用方据此把错误
    /// 画出来);已经 Closed 的不动 —— pane 都关了,不必再画错误。
    pub fn fail(&mut self) -> bool {
        if matches!(self, Self::Starting(_)) {
            *self = Self::Failed;
            true
        } else {
            false
        }
    }

    /// 关闭。返回正在跑的会话(调用方先 kill 再丢);还在起的话排队的东西一并作废,
    /// 之后回填上来的会话会被 [`Self::backfill`] 退回丢弃。
    pub fn close(&mut self) -> Option<S> {
        match std::mem::replace(self, Self::Closed) {
            Self::Running(session) => Some(session),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 桩会话记下的一次调用。
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Input(String),
        Reply(String),
        InputThenArm(String, String),
        Resize(u16, u16),
        Dropped,
    }

    /// 记账桩:每次调用按顺序记进共享日志;被丢弃时记一条 `Dropped`
    /// (对应 `PtySession::drop` 杀子进程)。
    struct FakePty {
        log: Rc<RefCell<Vec<Call>>>,
        size: RefCell<(u16, u16)>,
    }

    impl FakePty {
        fn new() -> (Self, Rc<RefCell<Vec<Call>>>) {
            let log = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    log: log.clone(),
                    size: RefCell::new((mt_pty::INITIAL_PTY_ROWS, mt_pty::INITIAL_PTY_COLS)),
                },
                log,
            )
        }
    }

    impl PtyPort for FakePty {
        fn write(&self, bytes: &[u8]) -> Result<()> {
            let text = String::from_utf8_lossy(bytes).into_owned();
            self.log.borrow_mut().push(Call::Input(text));
            Ok(())
        }

        fn write_reply(&self, bytes: &[u8]) -> Result<()> {
            let text = String::from_utf8_lossy(bytes).into_owned();
            self.log.borrow_mut().push(Call::Reply(text));
            Ok(())
        }

        fn write_then_arm_ssh_autofill(&self, bytes: &[u8], password: String) -> Result<()> {
            let text = String::from_utf8_lossy(bytes).into_owned();
            self.log
                .borrow_mut()
                .push(Call::InputThenArm(text, password));
            Ok(())
        }

        fn resize_if_changed(&self, rows: u16, cols: u16) -> Result<bool> {
            if *self.size.borrow() == (rows, cols) {
                return Ok(false);
            }
            *self.size.borrow_mut() = (rows, cols);
            self.log.borrow_mut().push(Call::Resize(rows, cols));
            Ok(true)
        }
    }

    impl Drop for FakePty {
        fn drop(&mut self) {
            self.log.borrow_mut().push(Call::Dropped);
        }
    }

    fn input(s: &str) -> Call {
        Call::Input(s.to_string())
    }

    /// 回填并断言被接住(Starting 态)。
    fn filled(slot: &mut PtySlot<FakePty>, pty: FakePty) -> Backfilled {
        match slot.backfill(pty) {
            Ok(filled) => filled,
            Err(_) => panic!("Starting 态应当接住会话"),
        }
    }

    /// 回填并断言被退回(非 Starting 态)。
    fn rejected(slot: &mut PtySlot<FakePty>, pty: FakePty) -> FakePty {
        match slot.backfill(pty) {
            Ok(_) => panic!("非 Starting 态必须退回会话"),
            Err(pty) => pty,
        }
    }

    /// 空窗期的写入按到达顺序排队,回填时原样冲刷;「写后 arm」整体一个元素,
    /// 前后的写入插不进它中间,也不会被拆成「先 arm 后写」。
    #[test]
    fn 回填前的写入按序排队并保持写后_arm_的原子性() {
        let mut slot = PtySlot::<FakePty>::starting();
        slot.write(b"claude --resume abc\r").unwrap();
        slot.write_reply(b"\x1b[1;1R").unwrap();
        slot.write_then_arm_ssh_autofill(b"ssh u@h\r", "secret".into())
            .unwrap();
        slot.write(b"l").unwrap();
        slot.write(b"s").unwrap();
        assert_eq!(slot.phase(), PtyPhase::Starting);

        let (pty, log) = FakePty::new();
        assert!(log.borrow().is_empty(), "回填之前一个字节都不该落到会话上");
        let filled = filled(&mut slot, pty);
        assert!(filled.write_errors.is_empty());
        assert!(filled.resized.is_none(), "空窗期没收到尺寸就不补 resize");
        assert_eq!(
            *log.borrow(),
            vec![
                input("claude --resume abc\r"),
                Call::Reply("\x1b[1;1R".into()),
                Call::InputThenArm("ssh u@h\r".into(), "secret".into()),
                input("l"),
                input("s"),
            ]
        );

        // 回填之后直通会话,不再排队
        slot.write(b"x").unwrap();
        assert_eq!(log.borrow().last(), Some(&input("x")));
        assert_eq!(slot.phase(), PtyPhase::Running);
    }

    /// 空窗期多次 resize 只取最新一次,且排在冲刷写入之前下发。
    #[test]
    fn 回填前多次_resize_只下发最新一次() {
        let mut slot = PtySlot::<FakePty>::starting();
        assert!(!slot.resize(30, 100).unwrap(), "空窗期不算真的下发");
        slot.write(b"a").unwrap();
        assert!(!slot.resize(40, 120).unwrap());
        assert!(!slot.resize(50, 160).unwrap());

        let (pty, log) = FakePty::new();
        let filled = filled(&mut slot, pty);
        assert!(
            matches!(filled.resized, Some(Ok(true))),
            "补下发的那次要如实回报"
        );
        assert_eq!(*log.borrow(), vec![Call::Resize(50, 160), input("a")]);
    }

    /// 空窗期最后一次尺寸恰好等于 spawn 时的初始尺寸:不重复下发。
    #[test]
    fn 回填前尺寸没变就不补_resize() {
        let mut slot = PtySlot::<FakePty>::starting();
        slot.resize(mt_pty::INITIAL_PTY_ROWS, mt_pty::INITIAL_PTY_COLS)
            .unwrap();
        let (pty, log) = FakePty::new();
        let filled = filled(&mut slot, pty);
        assert!(matches!(filled.resized, Some(Ok(false))));
        assert!(log.borrow().is_empty());
    }

    /// 回填前 pane 已关闭:会话被退回,调用方一丢即回收;排队的写入作废,
    /// 一个字节都不会落到那条会话上。
    #[test]
    fn 回填前关闭则会话被退回丢弃() {
        let mut slot = PtySlot::<FakePty>::starting();
        slot.write(b"echo hi\r").unwrap();
        slot.write_then_arm_ssh_autofill(b"ssh u@h\r", "secret".into())
            .unwrap();
        assert!(slot.close().is_none(), "还没有会话可交出");
        assert_eq!(slot.phase(), PtyPhase::Closed);

        let (pty, log) = FakePty::new();
        drop(rejected(&mut slot, pty));
        assert_eq!(*log.borrow(), vec![Call::Dropped], "只该发生一次回收");

        // 关闭之后写入 / resize 静默丢弃,退出也不再上报
        slot.write(b"x").unwrap();
        assert!(!slot.resize(1, 1).unwrap());
        assert_eq!(slot.note_exit(Some(0)), None);
        assert!(!slot.fail(), "关闭后的起失败不该再落成 Failed");
        assert_eq!(slot.phase(), PtyPhase::Closed);
    }

    /// 运行中关闭:交出会话由调用方 kill;之后回填(不会发生,但要稳)照样退回。
    #[test]
    fn 运行中关闭交出会话() {
        let mut slot = PtySlot::<FakePty>::starting();
        let (pty, log) = FakePty::new();
        filled(&mut slot, pty);
        let session = slot.close().expect("运行中应交出会话");
        drop(session);
        assert_eq!(*log.borrow(), vec![Call::Dropped]);
        assert!(slot.close().is_none(), "重复关闭是空操作");
    }

    /// 空窗期里到的退出先挂着,回填完成后才交还;运行中的退出当场上报。
    #[test]
    fn 回填前到达的退出挂到回填之后() {
        let mut slot = PtySlot::<FakePty>::starting();
        slot.write(b"a").unwrap();
        assert_eq!(slot.note_exit(Some(3)), None, "Starting 期间不许上报");
        let (pty, log) = FakePty::new();
        let filled = filled(&mut slot, pty);
        assert_eq!(filled.exit, Some(Some(3)));
        assert_eq!(*log.borrow(), vec![input("a")], "写入照常冲刷");

        // 运行中:当场上报
        assert_eq!(slot.note_exit(None), Some(None));
    }

    /// 起失败:排队的写入作废、之后的写入静默丢弃;只有 Starting 才落成 Failed。
    #[test]
    fn 起失败后写入静默丢弃() {
        let mut slot = PtySlot::<FakePty>::starting();
        slot.write(b"a").unwrap();
        assert!(slot.fail());
        assert_eq!(slot.phase(), PtyPhase::Failed);
        assert!(!slot.is_live());
        slot.write(b"b").unwrap();
        slot.write_reply(b"c").unwrap();
        assert!(!slot.resize(10, 10).unwrap());
        assert_eq!(slot.note_exit(Some(1)), None);
        assert!(!slot.fail(), "重复失败不再翻状态");

        // 失败之后竟然又交回一条会话(不会发生):退回丢弃,不复活
        let (pty, log) = FakePty::new();
        drop(rejected(&mut slot, pty));
        assert_eq!(*log.borrow(), vec![Call::Dropped]);
    }

    /// 真会话走一遍(生产用的 `PtyPort for PtySession`):空窗期排队的两条命令,
    /// 回填后按顺序抵达子进程并被执行。
    ///
    /// 命令文本里刻意不出现期望的输出串(cmd 用 `^` 转义、sh 用 printf 拼),
    /// 输入回显就不会被误当成执行结果。
    #[test]
    fn 真会话_回填后排队的命令按序执行() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let (program, first, second) = if cfg!(windows) {
            ("cmd.exe", "echo mt^-slot^-1\r", "echo mt^-slot^-2\r")
        } else {
            (
                "/bin/sh",
                "printf 'mt-slot-%s\\n' 1\r",
                "printf 'mt-slot-%s\\n' 2\r",
            )
        };
        let mut slot = PtySlot::<mt_pty::PtySession>::starting();
        slot.write(first.as_bytes()).unwrap();
        slot.resize(30, 100).unwrap();
        slot.write(second.as_bytes()).unwrap();

        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&output);
        let spec = mt_pty::PtySpawn {
            program: program.to_string(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            rows: mt_pty::INITIAL_PTY_ROWS,
            cols: mt_pty::INITIAL_PTY_COLS,
        };
        let session = mt_pty::PtySession::spawn(spec, move |bytes| {
            sink.lock().unwrap().extend_from_slice(bytes);
        })
        .expect("spawn 失败");
        let Ok(filled) = slot.backfill(session) else {
            panic!("Starting 态应当接住会话");
        };
        assert!(filled.write_errors.is_empty());
        assert!(matches!(filled.resized, Some(Ok(true))));

        let deadline = Instant::now() + Duration::from_secs(15);
        let text = loop {
            let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
            if text.contains("mt-slot-2") || Instant::now() >= deadline {
                break text;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let (Some(a), Some(b)) = (text.find("mt-slot-1"), text.find("mt-slot-2")) else {
            panic!("排队的命令没有全部执行: {text:?}");
        };
        assert!(a < b, "执行顺序乱了: {text:?}");
    }
}
