//! PTY 生命周期:spawn / read / write / resize / kill。**不含任何 UI 与 VT 解析**。
//!
//! # 从 `src-tauri/src/pty.rs` 移入的范围
//!
//! | 现有代码 | 去向 |
//! |---|---|
//! | `create_pty` / `write_pty` / `resize_pty` / `kill_pty` | 本 crate,去掉 `#[tauri::command]`,改成普通方法 |
//! | AI 命令识别(`claude`/`codex`/`opencode`/`pi`/`grok` + ↑ 历史/Tab 补全的行快照兜底与输出回扫) | `mt-ai`,它需要的只是「用户键入的字节」这一路输入 |
//! | 裸 Esc / Ctrl+C 的用户打断识别(`note_user_interrupt`) | `mt-ai`,同上 |
//! | `arm_ssh_autofill` | 保留在本 crate(它就是往 PTY 写字节) |
//! | ConPTY 便携 DLL 预载(`conpty_bootstrap.rs`) | 本 crate,原样搬,仍须早于任何 `openpty` |
//!
//! 「用户键入的字节」这一路由 [`PtySession::set_input_observer`] 提供:上层把
//! 观察器挂上去就能拿到每一次写入的原始字节,本 crate 不解释它们,**也不知道
//! 有 AI 这回事**。
//!
//! # 线程模型
//!
//! 一个会话三条线程,调用方线程(GPUI 主线程)**从不碰管道**:
//!
//! ```text
//! reader 线程   PTY 输出 → on_output(喂 VT 状态机);命中 SSH 密码提示时直接回写密码
//! writer 线程   write() 入队的字节 → 分块写进 PTY(见 write_chunked)
//! 退出 watcher  轮询子进程,退出后回调一次
//! ```
//!
//! [`PtySession::write`] 只做观察器通知 + 入队即返回。写入放到独立线程是因为
//! 管道写在两种情况下会阻塞:Windows 上多行长文本要逐行喂并让 ConPTY 消化
//! (`write_chunked` 每行 sleep 1ms,两千行就是两秒),以及子进程停止消费输入
//! (输入缓冲一满 conhost 就不再读管道)。这两种阻塞放在主线程上就是整窗冻结。
//!
//! # 明确**不要**移过来的东西
//!
//! 以下代码在 GPUI 架构下没有存在意义,移植时直接删掉,不要试图保留:
//!
//! - **16ms 批量缓冲**:原本是为了摊薄 `emit('pty-output')` 的 IPC 开销。现在
//!   reader 线程读到的字节直接进 `mt-terminal` 的 grid,没有 IPC。
//! - **有界 channel + 4MB/1MB 双水位背压 + `set_pty_flow_paused` + 30s 超时兜底**:
//!   原本是拿来在 WebView 边界上人工造一条背压链路。现在解析速度就是本进程的
//!   速度,读慢了 ConPTY 自然阻塞刷屏进程,背压是天然的。
//! - **`kill_all_ptys` 孤儿回收**:原本是为了兜住 WebView2 renderer 被 OOM 杀掉后
//!   页面重载、旧 PTY 无人引用却继续运行。GPUI 是单进程,进程没了 PTY 也就没了。
//!
//! 这三块是本次改造在后端侧最大的一笔净删除,详见 `docs/gpui-migration.md`。
//!
//! 同时删掉的还有 `PtyManager`:`HashMap<pty_id, PtyInstance>` + 自增 id + 十张
//! 旁路状态表(以及为它们准备的 `purge_pty_state`)。GPUI 侧每个 pane 直接持有
//! 一个 [`PtySession`],所有权即生命周期,注册表和 id 分配都没有存在理由。
//!
//! # 模块地图
//!
//! - [`conpty`] —— 便携 ConPTY 预载(**必须早于任何 spawn**,见该模块文档)
//! - [`ssh`] —— SSH 密码自动填充状态机 + 远程启动器 argv 拼装
//! - `launch`(内部) —— cwd / WSL 启动器重写 / 环境变量装配,导出见下方 re-export
//!
//! # 用法梗概
//!
//! ```no_run
//! # use mt_pty::{PtyOptions, PtySession, PtySpawn};
//! mt_pty::conpty::initialize_default(); // 进程内一次,且早于任何 spawn
//!
//! let spec = PtySpawn {
//!     program: "pwsh.exe".into(),
//!     args: vec![],
//!     cwd: Some(r"D:\Git\mini-term".into()),
//!     env: vec![],           // 应用注入的内部变量
//!     rows: 24,
//!     cols: 80,
//! };
//! let options = PtyOptions::default()
//!     .with_user_env(vec![("FOO".into(), "1".into())]) // 项目级 env(会被过滤)
//!     .on_exit(|code| eprintln!("子进程退出:{code:?}"));
//!
//! let session = PtySession::spawn_with_options(spec, options, |bytes| {
//!     // 直接喂给 VT 状态机
//!     let _ = bytes;
//! })?;
//! session.set_input_observer(|bytes| { let _ = bytes; }); // 键入字节的旁路
//! session.write(b"ls\r")?;
//! # Ok::<(), anyhow::Error>(())
//! ```

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub mod conpty;
mod launch;
pub mod ssh;

pub use conpty::{ConptyBootstrapDecision, choose_conpty_bootstrap};
pub use launch::{
    WslOverride, build_wslenv_value, decide_wsl_override, fallback_local_cwd, fallback_windows_cwd,
};
pub use ssh::SshAutofill;

/// reader 单次读取的缓冲区大小。
///
/// 比原实现的 4KB 大一档:那个尺寸是为了配合「有界 channel + 16ms 批缓冲」把
/// 在途内存卡在 2MB,而现在字节读出来就直接进 VT 状态机,一次多读一点纯赚。
const READ_CHUNK: usize = 64 * 1024;

/// PTY 尺寸的缺省值。上层挂载后会立刻按真实尺寸 resize。
pub const INITIAL_PTY_COLS: u16 = 80;
pub const INITIAL_PTY_ROWS: u16 = 24;

/// 终端焦点事件的 CSI 序列(TUI 开启 DEC 私有模式 1004 后,终端会在获得/失去
/// 焦点时把它们写进 PTY)。它们不是用户按键:本 crate 据此不解除 SSH 密码自动
/// 填充;上层从 [`PtySession::set_input_observer`] 拿到写入字节后若要区分
/// 「用户敲的」与「终端自动发的」,直接比对这两个常量,不必各自再写一份。
pub const FOCUS_IN_SEQ: &[u8] = b"\x1b[I";
pub const FOCUS_OUT_SEQ: &[u8] = b"\x1b[O";

/// 退出监听的轮询节奏:头 [`EXIT_POLL_FAST_WINDOW`] 内快轮询(短命令退出得快,
/// 状态要跟得上),之后降速常驻。
///
/// 为什么不靠 reader 的 EOF(原实现的做法):**Windows ConPTY 不给 EOF**。
/// 伪控制台的输出管道由 conhost 持有,子进程退出后管道依旧敞着,要等
/// `ClosePseudoConsole()`(即 master 被销毁)才收口 —— 实测子进程退出后
/// `try_wait` 已返回 `Some(0)`,而 reader 仍稳稳阻塞在 `read` 上。
/// 于是退出监听独立成一条 watcher 线程,轮询子进程本身。
const EXIT_POLL_FAST: Duration = Duration::from_millis(50);
const EXIT_POLL_SLOW: Duration = Duration::from_millis(250);
const EXIT_POLL_FAST_WINDOW: Duration = Duration::from_secs(2);

type BoxedChild = Box<dyn Child + Send + Sync>;
type BoxedWriter = Box<dyn Write + Send>;
type InputObserver = Box<dyn FnMut(&[u8]) + Send>;
type ExitCallback = Box<dyn FnOnce(Option<u32>) + Send>;

/// 一个活着的 PTY 会话。持有 master 端、子进程句柄和写入端。
pub struct PtySession {
    /// `Option` 只为 [`Drop`] 能把 master 移到后台线程上销毁(见 `impl Drop`),
    /// 正常生命周期内始终是 `Some`。
    master: Option<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<BoxedChild>>,
    /// 通往 writer 线程的队列。无界:队列的意义就是让 [`Self::write`] 永不阻塞,
    /// 设上限就把管道的背压又引回了调用方线程;在途字节量由用户一次粘贴的体积
    /// 决定,不会无限长。
    ///
    /// PTY 的写端本身不在这个结构体上:它只被 writer 线程(用户输入队列)与
    /// reader 线程(SSH 密码自动填充的回写)各持一个 `Arc` —— **调用方线程不碰
    /// 管道**,见模块注释「线程模型」。
    write_tx: mpsc::Sender<Vec<u8>>,
    /// 写入路径的旁路观察器(见 [`PtySession::set_input_observer`])。
    input_observer: Arc<Mutex<Option<InputObserver>>>,
    /// SSH 密码自动填充状态,与 reader 线程共享。
    autofill: Arc<Mutex<Option<SshAutofill>>>,
    /// 上次已应用的尺寸 (cols, rows),[`PtySession::resize`] 用它做同尺寸去重。
    last_size: Mutex<(u16, u16)>,
    /// 会话正在被上层关闭。置位后退出 watcher 不再回调 —— `Drop` 里的 kill
    /// 不该被当成「子进程自己退出了」上报。
    closing: Arc<std::sync::atomic::AtomicBool>,
    /// cwd 命中 WSL UNC 时的重写结果,上层可据此提示用户一次。
    wsl_override: Option<WslOverride>,
}

/// 创建 PTY 所需的参数。字段刻意保持贫瘠 —— 现有 `create_pty` 的其余参数
/// (AI 识别相关、状态上报相关)属于 `mt-ai`,不该经过这里。
#[derive(Debug, Clone)]
pub struct PtySpawn {
    /// shell 可执行文件路径或名字。
    pub program: String,
    /// 传给 shell 的参数。
    pub args: Vec<String>,
    /// 工作目录;`None` 表示继承当前进程。
    pub cwd: Option<String>,
    /// 追加到子进程环境的键值对(应用注入的内部变量走这里)。
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
}

/// 起 PTY 时的可选行为。所有字段都有合理缺省,`PtyOptions::default()` 即原
/// `create_pty` 的行为。**新能力一律加在这里**,`PtySpawn` 的字段保持不变。
pub struct PtyOptions {
    /// 注入 TERM/COLORTERM/LANG/LC_CTYPE/LESSCHARSET(默认 `true`)。
    /// 顺序在 [`PtySpawn::env`] 与 [`Self::user_env`] 之前,后两者可以覆盖它们。
    pub terminal_env: bool,
    /// 用户 / 项目级环境变量。与 [`PtySpawn::env`] 的区别是**会被过滤**:
    /// 命中 [`Self::reserved_env_prefixes`] 的 key 与 `WSLENV` 一律丢弃。
    pub user_env: Vec<(String, String)>,
    /// 保留 key 前缀。默认 `["MINITERM_"]` —— 应用内部协议变量的命名空间,
    /// 用户手改配置也不该能覆盖它们。调用方可按自己的命名空间替换。
    pub reserved_env_prefixes: Vec<String>,
    /// cwd 命中 WSL UNC(`\\wsl$\...`)时改用 `wsl.exe` 启动(默认 `true`)。
    pub wsl_cwd_rewrite: bool,
    /// 子进程退出时回调一次,参数是退出码(取不到为 `None`)。
    ///
    /// 在一条独立的 watcher 线程上调用,与 `on_output` **并发**:回调抵达时
    /// reader 可能还在交付最后一批输出(Windows 上这批要等 PTY 销毁才吐完)。
    /// 会话被 [`Drop`] 掉时不会回调 —— 那是上层自己关掉的,不是子进程退出。
    pub on_exit: Option<ExitCallback>,
}

impl Default for PtyOptions {
    fn default() -> Self {
        Self {
            terminal_env: true,
            user_env: Vec::new(),
            reserved_env_prefixes: vec!["MINITERM_".to_string()],
            wsl_cwd_rewrite: true,
            on_exit: None,
        }
    }
}

impl std::fmt::Debug for PtyOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyOptions")
            .field("terminal_env", &self.terminal_env)
            .field("user_env", &self.user_env)
            .field("reserved_env_prefixes", &self.reserved_env_prefixes)
            .field("wsl_cwd_rewrite", &self.wsl_cwd_rewrite)
            .field("on_exit", &self.on_exit.is_some())
            .finish()
    }
}

impl PtyOptions {
    pub fn with_user_env(mut self, user_env: Vec<(String, String)>) -> Self {
        self.user_env = user_env;
        self
    }

    pub fn with_reserved_env_prefixes(mut self, prefixes: Vec<String>) -> Self {
        self.reserved_env_prefixes = prefixes;
        self
    }

    pub fn with_wsl_cwd_rewrite(mut self, enabled: bool) -> Self {
        self.wsl_cwd_rewrite = enabled;
        self
    }

    /// 注册退出回调(见 [`Self::on_exit`])。
    pub fn on_exit<F>(mut self, callback: F) -> Self
    where
        F: FnOnce(Option<u32>) + Send + 'static,
    {
        self.on_exit = Some(Box::new(callback));
        self
    }
}

impl PtySession {
    /// 起一个 PTY 并 spawn 子进程,行为等价于 `spawn_with_options(spec,
    /// PtyOptions::default(), on_output)`。
    ///
    /// `on_output` 在**独立的 reader 线程**上被调用,每次拿到一段刚读出的字节。
    /// 调用方(`mt-terminal`)在这里把字节喂进 VT 状态机 —— 这就是整条数据流的全部,
    /// 中间没有 channel、没有缓冲窗口、没有序列化。
    pub fn spawn<F>(spec: PtySpawn, on_output: F) -> Result<Self>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        Self::spawn_with_options(spec, PtyOptions::default(), on_output)
    }

    /// 起一个 PTY,并按 [`PtyOptions`] 做启动前的预处理
    /// (WSL 启动器重写 / 终端环境变量 / 用户 env 过滤 / 退出回调)。
    pub fn spawn_with_options<F>(
        spec: PtySpawn,
        mut options: PtyOptions,
        on_output: F,
    ) -> Result<Self>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        // on_exit 要移进退出 watcher 线程,其余字段还要留给 plan 用,先摘出来。
        let on_exit = options.on_exit.take();
        let plan = launch::plan(&spec, &options);
        Self::spawn_planned(plan, spec.rows, spec.cols, on_exit, on_output)
    }

    fn spawn_planned<F>(
        plan: launch::LaunchPlan,
        rows: u16,
        cols: u16,
        on_exit: Option<ExitCallback>,
        mut on_output: F,
    ) -> Result<Self>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty 失败")?;

        let mut cmd = CommandBuilder::new(&plan.program);
        for arg in &plan.args {
            cmd.arg(arg);
        }
        if let Some(cwd) = &plan.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &plan.env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn `{}` 失败", plan.program))?;
        // slave 必须在 spawn 后立刻丢弃,否则子进程退出时 master 侧读不到 EOF。
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("clone reader 失败")?;
        let writer: Arc<Mutex<BoxedWriter>> = Arc::new(Mutex::new(
            pair.master.take_writer().context("take writer 失败")?,
        ));
        let child: Arc<Mutex<BoxedChild>> = Arc::new(Mutex::new(child));
        let autofill: Arc<Mutex<Option<SshAutofill>>> = Arc::new(Mutex::new(None));

        let autofill_for_reader = Arc::clone(&autofill);
        let writer_for_reader = Arc::clone(&writer);
        std::thread::Builder::new()
            .name("mt-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; READ_CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            // SSH 密码自动填充先于交付:命中提示就直接回写密码,
                            // 不经 `write` —— 那是用户输入通道,不该被自动填充污染。
                            pump_autofill(&autofill_for_reader, &writer_for_reader, chunk);
                            on_output(chunk);
                        }
                    }
                }
            })
            .context("起 PTY reader 线程失败")?;

        let write_tx = spawn_writer(writer)?;

        let closing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(on_exit) = on_exit {
            spawn_exit_watcher(Arc::clone(&child), Arc::clone(&closing), on_exit);
        }

        Ok(Self {
            master: Some(pair.master),
            child,
            write_tx,
            input_observer: Arc::new(Mutex::new(None)),
            autofill,
            last_size: Mutex::new((cols, rows)),
            closing,
            wsl_override: plan.wsl_override,
        })
    }

    /// cwd 命中 WSL UNC 时的启动器重写结果(未命中为 `None`)。
    /// 命中意味着**用户配置的 shell 被无视**,改用 `wsl.exe` 启动,值得提示一次。
    pub fn wsl_override(&self) -> Option<&WslOverride> {
        self.wsl_override.as_ref()
    }

    /// 挂一个写入路径的旁路观察器:每次 [`write`](Self::write) 的原始字节都会
    /// 先交给它,再写进 PTY。上层用它把「用户键入」转发给别的模块做分析
    /// (本 crate 不解释这些字节,也不因它们改变任何行为)。
    ///
    /// 两点约定:
    /// - 观察器在**调用 `write` 的线程**上同步执行,里面别做慢活;
    /// - 观察器里**不要回写同一个 `PtySession`**,会自锁。
    ///
    /// 注意 SSH 密码自动填充的回写不走 `write`,因此**不会**经过观察器 ——
    /// 明文密码不会漏给上层。终端自动应答走 [`write_reply`](Self::write_reply),
    /// 同样不经过观察器(它们不是用户键入)。
    pub fn set_input_observer<F>(&self, observer: F)
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        *self.input_observer.lock() = Some(Box::new(observer));
    }

    /// 摘掉输入观察器。
    pub fn clear_input_observer(&self) {
        *self.input_observer.lock() = None;
    }

    /// 注册 SSH 密码自动填充:后续 PTY 输出命中密码提示时自动回写一次密码。
    /// 再次调用会重置状态(覆盖密码、清除已完成标记、重新起算有效期)。
    ///
    /// `disarm_on_input`:用户首次真实输入时是否解除本 autofill(见
    /// [`ssh::SshAutofill`] 的字段注释)。远程项目 pane 在 spawn 后直接用它、
    /// 传 `true`;往活着的 shell 里敲 `ssh` 的路径用
    /// [`write_then_arm_ssh_autofill`](Self::write_then_arm_ssh_autofill)。
    pub fn arm_ssh_autofill(&self, password: String, disarm_on_input: bool) {
        *self.autofill.lock() = Some(SshAutofill::new(password, disarm_on_input));
    }

    /// 「SSH 连接」菜单路径:把 `ssh …\r` 命令写进 PTY,**写完再**注册自动填充
    /// (`disarm_on_input = true`,此后用户任何真实输入都会解除它)。
    ///
    /// 为什么不让上层分两步调:
    /// - 先注册再 [`write`](Self::write):命令写入本身就会把 `disarm_on_input = true`
    ///   的 autofill 解掉,只能退回 `false`,而 `false` 在公钥登录成功时会一直待命;
    /// - 先 `write` 再 [`arm_ssh_autofill`](Self::arm_ssh_autofill):`write` 只是入队,
    ///   writer 线程随时可能把命令送进 PTY,严格说 ssh 的密码提示可以在两步之间
    ///   被 reader 线程喂掉(要主线程恰好在两句之间停顿到 ssh 走完 TCP + 密钥交换,
    ///   实际碰不到,但没理由留这条缝)。
    ///
    /// 这里**持 autofill 锁跨过「入队 → 注册」**:reader 线程喂输出前要拿同一把锁,
    /// 命令引出的任何输出都只能在注册之后被看到;这次写入不经过解除逻辑,注册
    /// 之后它也不可能再回头解除。输入观察器照常先收到字节(与 `write` 同序)。
    pub fn write_then_arm_ssh_autofill(&self, bytes: &[u8], password: String) -> Result<()> {
        if let Some(observer) = self.input_observer.lock().as_mut() {
            observer(bytes);
        }
        let mut autofill = self.autofill.lock();
        self.write_tx
            .send(bytes.to_vec())
            .map_err(|_| anyhow!("PTY 已关闭(写线程已退出)"))?;
        *autofill = Some(SshAutofill::new(password, true));
        Ok(())
    }

    /// 用户向 PTY 真实输入时调用:仅当该 autofill 此刻
    /// [`disarm_on_input`](SshAutofill::disarm_on_input) 才解除并清除明文密码。
    /// [`write`](Self::write) 已自动调用它,一般无需手动调。
    ///
    /// 语义:SSH 认证阶段用户不打字(ssh 自驱动 publickey,失败才由 autofill 灌
    /// 密码);一旦用户按键即说明会话已进入交互 shell,此后 `su` / `mysql -p` /
    /// `passwd` 等以 "password:" 结尾的提示都不该再被灌入 SSH 登录密码 ——
    /// 尤其 publickey 登录成功时全程无密码提示、autofill 不会命中自解除,
    /// 不在此解除则它一直待命到有效期([`ssh::AUTOFILL_TTL`])结束,期间可能泄露密码。
    /// 唯一的例外是首连时回答主机密钥确认(`yes` / 指纹)的那一行,见
    /// [`SshAutofill::disarm_on_input`]。
    pub fn disarm_ssh_autofill_on_user_input(&self) {
        disarm_on_user_input(&self.autofill);
    }

    /// 往 PTY 写字节(用户键入、粘贴、拖入的文件路径都走这里;终端自动应答走
    /// [`write_reply`](Self::write_reply))。
    ///
    /// 顺序:通知输入观察器 → 解除 SSH 自动填充(焦点事件除外)→ 交给 writer
    /// 线程。观察器排在入队**之前**:上层拿这一路做的判定(例如为焦点事件开一个
    /// 重绘冷却窗口)必须在子进程响应抵达 reader 之前就建立起来。
    ///
    /// **不等管道**:字节入队即返回,写入与分块延迟都在 writer 线程上发生
    /// (见模块注释「线程模型」)。多次 `write` 的顺序由队列保证。
    ///
    /// 返回 `Err` 只有一种情况:writer 线程已因先前的写入失败退出(管道已断,
    /// 通常是子进程没了)。当次写入本身的失败不在这里报 —— 它发生在别的线程上,
    /// 由 writer 线程打一行日志后收摊。
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        if let Some(observer) = self.input_observer.lock().as_mut() {
            observer(bytes);
        }
        // 焦点进/出序列不是用户按键(TUI 开 DEC 1004 后由终端自动发送),
        // 据此解除会让认证期碰上焦点切换的会话再也填不进密码。
        if bytes != FOCUS_IN_SEQ && bytes != FOCUS_OUT_SEQ {
            self.disarm_ssh_autofill_on_user_input();
        }
        self.write_tx
            .send(bytes.to_vec())
            .map_err(|_| anyhow!("PTY 已关闭(写线程已退出)"))
    }

    /// 写终端自己的应答(DA / DSR / 光标位置 / OSC 颜色与剪贴板查询 / 文本区尺寸等,
    /// 即 alacritty 产生的 `PtyWrite` 一类)。与 [`write`](Self::write) 同一条
    /// writer 队列,相互顺序照旧;区别有两点:
    ///
    /// - **不解除 SSH 密码自动填充**:应答是终端替程序回话,不是用户按键。本地
    ///   shell(pwsh/PSReadLine、fish、nushell……)与 ConPTY 在 ssh 起来前后都可能
    ///   发查询,当成用户输入会让「SSH 连接」菜单路径的 autofill 在密码提示到达前
    ///   就被解掉。
    /// - **不经过输入观察器**:观察器约定的是「用户键入的字节」这一路;上层对应答
    ///   本来就刻意绕开 AI 输入识别(`mt-app` 的 `write_raw`「不经 AI 输入旁路」),
    ///   把应答里的转义序列喂进去只会让识别被带偏。
    pub fn write_reply(&self, bytes: &[u8]) -> Result<()> {
        self.write_tx
            .send(bytes.to_vec())
            .map_err(|_| anyhow!("PTY 已关闭(写线程已退出)"))
    }

    /// 调整 PTY 尺寸。尺寸与上次相同时直接返回(见 [`resize_if_changed`](Self::resize_if_changed))。
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.resize_if_changed(rows, cols).map(|_| ())
    }

    /// 同 [`resize`](Self::resize),但返回**是否真的下发了 resize**。
    ///
    /// 同尺寸去重:挂载 / 切 tab 等路径会重复上报未变的尺寸,而 ConPTY 收到
    /// resize(即使同尺寸)会让 TUI 应用整屏重绘 —— 帧高于视口时每次重绘都往
    /// scrollback 漏一份残留。尺寸没变就不透传。
    ///
    /// 上层若要在「真的 resize 了」之后做别的事(例如开一个重绘冷却窗口),
    /// 用这个返回值判断,不要自己再存一份尺寸。
    ///
    /// Windows 上跨度大于一格时**逐格走到目标尺寸**,理由见 [`resize_path`]。
    pub fn resize_if_changed(&self, rows: u16, cols: u16) -> Result<bool> {
        let mut last_size = self.last_size.lock();
        if *last_size == (cols, rows) {
            return Ok(false);
        }
        let master = self.master.as_ref().context("PTY 已关闭")?;
        for (step_cols, step_rows) in resize_path(*last_size, (cols, rows)) {
            master
                .resize(PtySize {
                    rows: step_rows,
                    cols: step_cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize 失败")?;
            // 中途失败时记住已经走到的那一格,下次从这里接着走
            *last_size = (step_cols, step_rows);
        }
        Ok(true)
    }

    pub fn kill(&mut self) -> Result<()> {
        self.child.lock().kill().context("kill 失败")
    }

    /// 非阻塞地看一眼子进程是否已退出。
    pub fn try_wait(&mut self) -> Result<Option<u32>> {
        Ok(self.child.lock().try_wait()?.map(|s| s.exit_code()))
    }
}

impl Drop for PtySession {
    /// 先杀子进程,再把 master 丢到后台线程上销毁。
    ///
    /// Windows 上销毁 master 会触发 `ClosePseudoConsole()`,它是同步的,会一直
    /// 阻塞到该控制台会话里的每个进程都退出。子进程里还挂着长跑的 AI 进程时,
    /// 这一下在调用线程上永远不返回 —— 整个 UI 卡死成「未响应」。
    /// 先 kill 让 ConPTY 知道主进程没了,再后台销毁,UI 线程一秒都不等。
    fn drop(&mut self) {
        self.closing
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.child.lock().kill();
        if let Some(master) = self.master.take() {
            std::thread::spawn(move || drop(master));
        }
    }
}

/// writer 线程:按 [`PtySession::write`] 入队的顺序把字节写进 PTY。
///
/// 一次写失败即退出 —— 管道断了之后不会自己好,继续消费队列只是把用户的输入
/// 悄悄扔掉;线程一退,`Sender` 那头的下一次 `write` 就拿到 `Err`,上层看得见。
/// 会话被 [`Drop`] 时 `write_tx` 随之丢弃,队列耗尽后线程自然结束。
fn spawn_writer(writer: Arc<Mutex<BoxedWriter>>) -> Result<mpsc::Sender<Vec<u8>>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("mt-pty-writer".into())
        .spawn(move || {
            for chunk in rx {
                if let Err(err) = write_chunked(&mut **writer.lock(), &chunk) {
                    eprintln!("[mt-pty] 写 PTY 失败,写线程退出: {err:#}");
                    break;
                }
            }
        })
        .context("起 PTY writer 线程失败")?;
    Ok(tx)
}

/// Windows ConPTY 无法一次处理大量输入数据(粘贴长文本时只剩最后一行)。
/// 将数据按行拆分,每行写入后加短暂延迟,给 ConPTY 时间消化。
/// 短数据(普通键盘输入)直接写入不受影响。
///
/// 只在 writer 线程上调用(与 reader 线程的密码回写共用 `writer` 锁,
/// 分块期间那边会等一等,无碍)。
fn write_chunked(writer: &mut dyn Write, bytes: &[u8]) -> Result<()> {
    const CHUNK_THRESHOLD: usize = 128;
    const INTER_LINE_DELAY: Duration = Duration::from_millis(1);

    if !cfg!(windows) || bytes.len() <= CHUNK_THRESHOLD || !bytes.contains(&b'\n') {
        writer.write_all(bytes)?;
        writer.flush()?;
        return Ok(());
    }

    // 按行拆分写入,保留每行的换行符
    let mut start = 0;
    while start < bytes.len() {
        let end = match bytes[start..].iter().position(|&b| b == b'\n') {
            Some(pos) => start + pos + 1, // 包含 \n
            None => bytes.len(),          // 最后一段无换行
        };
        writer.write_all(&bytes[start..end])?;
        writer.flush()?;
        start = end;
        if start < bytes.len() {
            std::thread::sleep(INTER_LINE_DELAY);
        }
    }
    Ok(())
}

/// 一次 resize 实际下发的尺寸序列 `(cols, rows)`,末项即目标尺寸。
///
/// **Windows 上逐格走**([`cell_steps`]):portable-pty 建 ConPTY 时带
/// `PSEUDOCONSOLE_RESIZE_QUIRK`,约定「终端自己重排缓冲区,ConPTY 不重画」——
/// resize 时 ConPTY 一个字节都不发,光标在哪一行全凭两边各自重排的结果对得上。
/// 而 conhost 的重排**与路径有关**:同样从 199 列缩到 136 列,一步跳过去与逐列
/// 走过去,光标落的行不一样;只有逐格走(实测每步 ≤2 列)才与 alacritty 的重排
/// 对得上。对不上的后果是 PSReadLine 按 conhost 的光标行发绝对定位,下一条输入
/// 画到提示符上一行、输出盖掉提示符(oh-my-posh 右侧提示符那种占满整行的提示
/// 最容易中);行数一步缩一大截同理。逐格连发很便宜(实测 521 次共 0.46ms,只是
/// 往 conhost 的信号管道写消息),Claude Code 这类 TUI 也只按最终尺寸重画一次。
///
/// 其它平台 resize 只是一次 `TIOCSWINSZ`,没有这层对账,一步到位。
fn resize_path(from: (u16, u16), to: (u16, u16)) -> Vec<(u16, u16)> {
    if cfg!(windows) {
        cell_steps(from, to)
    } else {
        vec![to]
    }
}

/// 从 `from` 走到 `to` 的每一步 `(cols, rows)`:每步行、列各至多挪一格(两维
/// 同时走,先到的那一维停住),不含起点、末项是 `to`;起止相同为空。
fn cell_steps(from: (u16, u16), to: (u16, u16)) -> Vec<(u16, u16)> {
    fn toward(value: u16, target: u16) -> u16 {
        match value.cmp(&target) {
            std::cmp::Ordering::Less => value + 1,
            std::cmp::Ordering::Greater => value - 1,
            std::cmp::Ordering::Equal => value,
        }
    }
    let mut at = from;
    let mut steps = Vec::new();
    while at != to {
        at = (toward(at.0, to.0), toward(at.1, to.1));
        steps.push(at);
    }
    steps
}

/// 用户真实输入时的解除判定(见 [`PtySession::disarm_ssh_autofill_on_user_input`])。
/// 抽成自由函数是为了单测能与 [`pump_autofill`] 在同一把锁上交替驱动,不必起真 PTY。
fn disarm_on_user_input(autofill: &Mutex<Option<SshAutofill>>) {
    let mut guard = autofill.lock();
    if guard.as_ref().is_some_and(SshAutofill::disarm_on_input) {
        *guard = None;
    }
}

/// 把一段 PTY 输出喂给 SSH 密码自动填充;命中密码提示则直接回写密码 + 回车。
fn pump_autofill(
    autofill: &Arc<Mutex<Option<SshAutofill>>>,
    writer: &Arc<Mutex<BoxedWriter>>,
    chunk: &[u8],
) {
    let password = {
        let mut guard = autofill.lock();
        match guard.as_mut() {
            // 直接喂原始字节:feed 只解码它要看的那段尾巴,不必整块 64KB 先转一遍
            Some(state) if !state.is_done() => state.feed(chunk),
            _ => None,
        }
    };
    if let Some(password) = password {
        let mut writer = writer.lock();
        let _ = writer.write_all(password.as_bytes());
        let _ = writer.write_all(b"\r");
        let _ = writer.flush();
    }
}

/// 退出监听线程:轮询子进程,退出后回调一次即结束。
///
/// 每轮只在锁内做一次**非阻塞** `try_wait`,绝不持锁阻塞等待 —— 否则并发的
/// [`PtySession::kill`] 会卡在锁上,把 UI 线程一起拖住。
///
/// 线程自终结:会话被销毁时 [`Drop`] 会 kill 子进程,下一轮 `try_wait` 即拿到
/// 结果退出循环(`closing` 已置位,不回调)。
fn spawn_exit_watcher(
    child: Arc<Mutex<BoxedChild>>,
    closing: Arc<std::sync::atomic::AtomicBool>,
    on_exit: ExitCallback,
) {
    use std::sync::atomic::Ordering;

    std::thread::spawn(move || {
        let started = Instant::now();
        loop {
            if closing.load(Ordering::Relaxed) {
                return;
            }
            let exit_code = match child.lock().try_wait() {
                Ok(Some(status)) => Some(status.exit_code()),
                Ok(None) => {
                    // 还活着
                    let interval = if started.elapsed() < EXIT_POLL_FAST_WINDOW {
                        EXIT_POLL_FAST
                    } else {
                        EXIT_POLL_SLOW
                    };
                    std::thread::sleep(interval);
                    continue;
                }
                // 句柄已被回收之类:确定退不出更多信息,报「退出码未知」收场,
                // 免得线程空转到进程结束。
                Err(_) => None,
            };
            if !closing.load(Ordering::Relaxed) {
                on_exit(exit_code);
            }
            return;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn smoke_spec() -> PtySpawn {
        let (program, args) = if cfg!(windows) {
            (
                "cmd.exe",
                vec!["/c".to_string(), "echo mt-pty-smoke".to_string()],
            )
        } else {
            (
                "/bin/sh",
                vec!["-c".to_string(), "echo mt-pty-smoke".to_string()],
            )
        };
        PtySpawn {
            program: program.to_string(),
            args,
            cwd: None,
            env: Vec::new(),
            rows: INITIAL_PTY_ROWS,
            cols: INITIAL_PTY_COLS,
        }
    }

    // === resize 逐格下发(ConPTY 重排与路径有关,见 resize_path) ===

    #[test]
    fn 逐格走到目标_每步行列各至多一格() {
        let steps = cell_steps((199, 40), (136, 20));
        assert_eq!(steps.len(), 63, "步数 = 两维跨度里大的那个");
        assert_eq!(steps.last(), Some(&(136, 20)));
        let mut prev = (199u16, 40u16);
        for &step in &steps {
            assert!(prev.0.abs_diff(step.0) <= 1 && prev.1.abs_diff(step.1) <= 1);
            prev = step;
        }
        // 行数先到 20 就停住,不越过
        assert!(steps.iter().all(|&(_, rows)| rows >= 20));
        assert_eq!(steps[19], (179, 20));
        assert_eq!(steps[20], (178, 20));
    }

    #[test]
    fn 逐格走_放大与单维() {
        assert_eq!(cell_steps((80, 24), (82, 24)), vec![(81, 24), (82, 24)]);
        assert_eq!(cell_steps((80, 24), (80, 22)), vec![(80, 23), (80, 22)]);
        assert_eq!(cell_steps((80, 24), (81, 25)), vec![(81, 25)]);
        assert!(cell_steps((80, 24), (80, 24)).is_empty());
    }

    #[test]
    fn resize_path_只有_windows_逐格() {
        let path = resize_path((199, 40), (136, 40));
        if cfg!(windows) {
            assert_eq!(path.len(), 63);
        } else {
            assert_eq!(path, vec![(136, 40)]);
        }
        assert_eq!(path.last(), Some(&(136, 40)));
    }

    // === 分块写入(Windows ConPTY 粘贴长文本会只剩最后一行) ===

    #[test]
    fn write_chunked_passes_short_input_through() {
        let mut sink: Vec<u8> = Vec::new();
        write_chunked(&mut sink, b"ls -la\r").unwrap();
        assert_eq!(sink, b"ls -la\r");
    }

    #[test]
    fn write_chunked_preserves_every_byte_of_long_multiline_paste() {
        // 拆行写入不得丢字节、不得改顺序:拼回来必须与原文逐字节相等。
        let payload: String = (0..40)
            .map(|i| format!("line {i} with some padding text\n"))
            .collect();
        let mut sink: Vec<u8> = Vec::new();
        write_chunked(&mut sink, payload.as_bytes()).unwrap();
        assert_eq!(sink, payload.as_bytes());
    }

    #[test]
    fn write_chunked_handles_trailing_segment_without_newline() {
        let payload = format!("{}\nno trailing newline", "x".repeat(200));
        let mut sink: Vec<u8> = Vec::new();
        write_chunked(&mut sink, payload.as_bytes()).unwrap();
        assert_eq!(sink, payload.as_bytes());
    }

    // === 端到端:起真进程 → 收输出 → 收退出码 ===

    #[test]
    fn spawn_streams_output_and_reports_exit_code() {
        let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&collected);
        let (tx, rx) = mpsc::channel();

        let session = PtySession::spawn_with_options(
            smoke_spec(),
            PtyOptions::default().on_exit(move |code| {
                let _ = tx.send(code);
            }),
            move |bytes| sink.lock().extend_from_slice(bytes),
        )
        .expect("spawn 失败");

        let exit_code = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("退出回调未在 30s 内触发");
        assert_eq!(exit_code, Some(0));

        // on_exit 与 on_output 并发(见 `PtyOptions::on_exit` 的契约):退出回调
        // 先到时 reader 可能还没交出最后一批,Linux 上实测约三成概率;给它几秒
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let output = loop {
            let output = String::from_utf8_lossy(&collected.lock()).into_owned();
            if output.contains("mt-pty-smoke") || std::time::Instant::now() >= deadline {
                break output;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            output.contains("mt-pty-smoke"),
            "reader 线程未把子进程输出交出来: {output:?}"
        );
        drop(session);
    }

    #[test]
    fn dropping_session_does_not_report_child_exit() {
        // 上层关 pane(drop)时 Drop 会 kill 子进程,那不是「子进程自己退出了」,
        // 不该回调 —— 否则 UI 刚删掉的 pane 又收到一条退出通知。
        let (tx, rx) = mpsc::channel();
        let spec = PtySpawn {
            program: if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.to_string(),
            args: Vec::new(), // 交互式 shell:不给输入就一直活着
            cwd: None,
            env: Vec::new(),
            rows: INITIAL_PTY_ROWS,
            cols: INITIAL_PTY_COLS,
        };

        let session = PtySession::spawn_with_options(
            spec,
            PtyOptions::default().on_exit(move |code| {
                let _ = tx.send(code);
            }),
            |_| {},
        )
        .expect("spawn 失败");
        drop(session);

        assert!(
            rx.recv_timeout(Duration::from_millis(800)).is_err(),
            "上层主动关闭会话不该触发退出回调"
        );
    }

    /// 上层(mt-app 的 pane)在后台线程上 spawn、再把会话交回主线程,
    /// 会话必须能跨线程移动。编译期断言,改字段时别把它弄丢。
    #[test]
    fn session_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PtySession>();
    }

    #[test]
    fn dropping_session_kills_the_child() {
        // 上层在回填前丢掉会话(pane 已关闭 / 实体已释放)不再单独 kill,
        // 靠的就是这一条:`Drop` 必须当场杀子进程,不留孤儿。
        let session = PtySession::spawn(interactive_spec(), |_| {}).expect("spawn 失败");
        let child = Arc::clone(&session.child);
        assert!(
            child.lock().try_wait().expect("try_wait 失败").is_none(),
            "交互式 shell 此刻应当还活着"
        );
        drop(session);
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.lock().try_wait().expect("try_wait 失败").is_none() {
            assert!(Instant::now() < deadline, "丢弃会话 10s 后子进程仍然活着");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn write_notifies_input_observer_with_raw_bytes() {
        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let observer_sink = Arc::clone(&seen);

        let session = PtySession::spawn(smoke_spec(), |_| {}).expect("spawn 失败");
        session.set_input_observer(move |bytes| observer_sink.lock().extend_from_slice(bytes));
        session.write(b"hello\r").expect("write 失败");
        assert_eq!(&*seen.lock(), b"hello\r");

        // 摘掉之后不再收到
        session.clear_input_observer();
        session.write(b"more\r").expect("write 失败");
        assert_eq!(&*seen.lock(), b"hello\r");
    }

    #[test]
    fn write_returns_without_waiting_for_the_pipe() {
        // 长多行粘贴在 Windows 上要逐行 sleep 1ms(见 write_chunked):两千行至少
        // 两秒。`write` 必须当场返回 —— 它跑在 GPUI 主线程上,等管道就是整窗冻结。
        // 用交互式 shell:`cmd /c echo` 那种会立刻退出,管道一断写线程就收摊了。
        let spec = PtySpawn {
            program: if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.to_string(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            rows: INITIAL_PTY_ROWS,
            cols: INITIAL_PTY_COLS,
        };
        let session = PtySession::spawn(spec, |_| {}).expect("spawn 失败");
        let payload: String = (0..2000)
            .map(|i| format!("rem line {i} {}\n", "x".repeat(150)))
            .collect();

        let started = Instant::now();
        session.write(payload.as_bytes()).expect("write 失败");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "write 应当入队即返回,实际耗时 {elapsed:?}"
        );
        drop(session);
    }

    #[test]
    fn resize_dedupes_identical_size() {
        let session = PtySession::spawn(smoke_spec(), |_| {}).expect("spawn 失败");
        // 初始尺寸来自 spec,重复上报同尺寸不得下发 resize
        assert!(
            !session
                .resize_if_changed(INITIAL_PTY_ROWS, INITIAL_PTY_COLS)
                .unwrap()
        );
        assert!(session.resize_if_changed(30, 100).unwrap());
        assert!(!session.resize_if_changed(30, 100).unwrap());
    }

    // === SSH 自动填充在 session 层的解除语义 ===

    #[test]
    fn user_input_disarms_autofill_when_flagged() {
        let session = PtySession::spawn(smoke_spec(), |_| {}).expect("spawn 失败");
        session.arm_ssh_autofill("secret".into(), true);
        session.write(b"l").expect("write 失败");
        assert!(
            session.autofill.lock().is_none(),
            "远程项目 pane 的 autofill 应在用户首次输入后解除"
        );
    }

    #[test]
    fn user_input_keeps_autofill_when_not_flagged() {
        // `disarm_on_input = false` 的语义:写入不解除,只靠命中提示 / 失败 / 过期自解除。
        let session = PtySession::spawn(smoke_spec(), |_| {}).expect("spawn 失败");
        session.arm_ssh_autofill("secret".into(), false);
        session.write(b"ssh u@h\r").expect("write 失败");
        assert!(session.autofill.lock().is_some());
    }

    #[test]
    fn menu_path_command_write_does_not_disarm_but_user_input_does() {
        // 「SSH 连接」菜单路径:`ssh …\r` 先入队、再以 disarm_on_input = true 注册 ——
        // 那条命令写入解除不到它;之后用户的任何按键都会解除。
        // 命令内容无关紧要,用无害的 echo 代替真 ssh。
        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let observer_sink = Arc::clone(&seen);
        let session = PtySession::spawn(interactive_spec(), |_| {}).expect("spawn 失败");
        session.set_input_observer(move |bytes| observer_sink.lock().extend_from_slice(bytes));

        session
            .write_then_arm_ssh_autofill(b"echo ssh-menu\r", "secret".into())
            .expect("write 失败");
        assert!(
            session
                .autofill
                .lock()
                .as_ref()
                .is_some_and(|state| state.disarm_on_input() && !state.is_done()),
            "命令写入之后 autofill 应处于待命状态,且标了 disarm_on_input"
        );
        assert_eq!(
            &*seen.lock(),
            b"echo ssh-menu\r",
            "命令字节照常经过输入观察器"
        );

        // 焦点事件不是用户按键,不解除
        session.write(FOCUS_IN_SEQ).expect("write 失败");
        assert!(session.autofill.lock().is_some());

        // 用户开始打字(公钥已登录成功、在跑 sudo 之类)→ 解除
        session.write(b"s").expect("write 失败");
        assert!(
            session.autofill.lock().is_none(),
            "用户首次输入后应解除,避免把 SSH 密码灌进之后的 sudo / su 提示"
        );
    }

    /// 交互式 shell:不给输入就一直活着。`cmd /c echo` 那种会立刻退出,管道一断
    /// 后续 write 就可能报错。
    fn interactive_spec() -> PtySpawn {
        PtySpawn {
            program: if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.to_string(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            rows: INITIAL_PTY_ROWS,
            cols: INITIAL_PTY_COLS,
        }
    }

    #[test]
    fn terminal_replies_neither_disarm_nor_reach_the_observer() {
        // DA / DSR / 光标位置 / OSC 颜色查询的应答是终端替程序回话,不是用户按键:
        // 本地 shell 与 ConPTY 在 ssh 起来前后都会发查询,应答不能把 autofill 解掉,
        // 也不该混进「用户键入」那一路。
        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let observer_sink = Arc::clone(&seen);
        let session = PtySession::spawn(interactive_spec(), |_| {}).expect("spawn 失败");
        session.set_input_observer(move |bytes| observer_sink.lock().extend_from_slice(bytes));
        session.arm_ssh_autofill("secret".into(), true);

        session
            .write_reply(b"\x1b[24;1R")
            .expect("write_reply 失败");
        session
            .write_reply(b"\x1b[?1;2c")
            .expect("write_reply 失败");
        session
            .write_reply(b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\")
            .expect("write_reply 失败");
        assert!(
            session.autofill.lock().is_some(),
            "终端应答不该解除 autofill"
        );
        assert!(seen.lock().is_empty(), "终端应答不该经过输入观察器");

        // 真正的用户按键照常解除
        session.write(b"a").expect("write 失败");
        assert!(session.autofill.lock().is_none());
    }

    #[test]
    fn host_key_confirm_answer_does_not_disarm_but_later_input_does() {
        // 模拟 reader 线程(pump_autofill)与用户写入(解除判定)在同一把锁上交替:
        // 首连确认处敲 yes 不解除 → 密码提示照常填入;确认之后的输入照常解除。
        // 不起真 PTY:shell 自己的输出会与手喂的提示交错,判定结果不稳定。
        let prompt: &[u8] = b"The authenticity of host 'h (10.0.0.5)' can't be established.\r\n\
            Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
        let written = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink: Arc<Mutex<BoxedWriter>> =
            Arc::new(Mutex::new(Box::new(SharedSink(Arc::clone(&written)))));

        let autofill = Arc::new(Mutex::new(Some(SshAutofill::new("secret".into(), true))));
        pump_autofill(&autofill, &sink, prompt);
        for key in [&b"y"[..], b"e", b"s", b"\r"] {
            disarm_on_user_input(&autofill);
            assert!(autofill.lock().is_some(), "回答确认提示不该解除");
            // 回显(回车的回显是换行)
            let echo: &[u8] = if key == b"\r" { b"\r\n" } else { key };
            pump_autofill(&autofill, &sink, echo);
        }
        pump_autofill(&autofill, &sink, b"root@h's password: ");
        assert_eq!(&*written.lock(), b"secret\r", "确认之后的密码提示仍要填入");

        // 另一轮:确认之后、密码提示之前用户又敲了键 → 照常解除
        let autofill = Arc::new(Mutex::new(Some(SshAutofill::new("secret".into(), true))));
        pump_autofill(&autofill, &sink, prompt);
        pump_autofill(&autofill, &sink, b"yes\r\n");
        disarm_on_user_input(&autofill);
        assert!(autofill.lock().is_none(), "确认之后的用户输入应当解除");
    }

    #[test]
    fn focus_events_do_not_disarm_autofill() {
        // 焦点进/出序列由终端自动发送,不是用户按键 —— 认证期碰上焦点切换
        // 不能把 autofill 解除掉,否则密码永远灌不进去。
        let session = PtySession::spawn(smoke_spec(), |_| {}).expect("spawn 失败");
        session.arm_ssh_autofill("secret".into(), true);
        session.write(FOCUS_IN_SEQ).expect("write 失败");
        session.write(FOCUS_OUT_SEQ).expect("write 失败");
        assert!(session.autofill.lock().is_some());
    }

    /// 可从测试侧读回写入内容的 writer。
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn pump_autofill_writes_password_and_newline_once() {
        let autofill = Arc::new(Mutex::new(Some(SshAutofill::new("secret".into(), false))));
        let written = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink: Arc<Mutex<BoxedWriter>> =
            Arc::new(Mutex::new(Box::new(SharedSink(Arc::clone(&written)))));

        pump_autofill(&autofill, &sink, b"Last login: Mon\r\n");
        assert!(written.lock().is_empty(), "普通输出不该触发回写");

        pump_autofill(&autofill, &sink, b"root@host's password: ");
        assert_eq!(&*written.lock(), b"secret\r", "密码后必须补一个回车");

        // 再来一次提示不该重复灌
        pump_autofill(&autofill, &sink, b"root@host's password: ");
        assert_eq!(&*written.lock(), b"secret\r");
        assert!(autofill.lock().as_ref().is_some_and(SshAutofill::is_done));
    }
}
