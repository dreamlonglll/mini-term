//! 终端 shell 建连期间与用户的交互(内部用):向宿主发一段要显示的文本、读一行用户输入,以及
//! 「等用户输入的时间不计入网络超时」的暂停时钟。
//!
//! 只有 [`crate::shell::open_shell`] 会建 [`Interaction`];会话池、「测试连接」与三个 sidecar
//! 不带它,走的代码路径与引入本模块之前一字不变。
//!
//! 用到它的地方:首见主机的 yes / no 提问(`known_hosts.rs`,在 russh 会话任务里 await)、认证
//! banner(同在 russh 的 Handler 里)、私钥口令(`key_unlock.rs`)、密码与 keyboard-interactive
//! (`auth.rs`)。

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{oneshot, Notify};
use tokio::time::Instant;

use crate::shell::ShellPrompt;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 一次终端 shell 建连的交互通道。
pub(crate) struct Interaction {
    /// 通往宿主的提示通道。`open_shell` 返回前摘掉([`Self::finish`]):russh 的 Handler 会持有
    /// 本结构直到会话结束,不能让它把宿主那头的接收端一直撑着。
    prompts: Mutex<Option<UnboundedSender<ShellPrompt>>>,
    clock: PauseClock,
    /// 用户取消过某个提问(或宿主已经不收提示了)。
    cancelled: AtomicBool,
    /// 用户对首见主机的提问答了 `no`。
    host_key_declined: AtomicBool,
    /// 累计在终端里等用户的时长;从没等过为 `None`。报「等用户期间连接被服务器断开」时用。
    waited: Mutex<Option<Duration>>,
}

impl Interaction {
    pub(crate) fn new(prompts: UnboundedSender<ShellPrompt>) -> Self {
        Self {
            prompts: Mutex::new(Some(prompts)),
            clock: PauseClock::default(),
            cancelled: AtomicBool::new(false),
            host_key_declined: AtomicBool::new(false),
            waited: Mutex::new(None),
        }
    }

    /// 发一段原样显示到终端的文本(调用方负责拼好 `\r\n`)。宿主已经不收时静默丢弃。
    pub(crate) fn notice(&self, text: impl Into<String>) {
        if let Some(prompts) = lock(&self.prompts).as_ref() {
            let _ = prompts.send(ShellPrompt::Notice(text.into()));
        }
    }

    /// 在终端里读一行:`prompt` 原样显示,`echo = false` 时不回显。等待期间暂停网络超时的时钟。
    ///
    /// 返回 `None` = 用户取消(宿主回了 `None`、丢了应答端,或者已经不收提示),同时记下取消。
    pub(crate) async fn read_line(&self, prompt: String, echo: bool) -> Option<String> {
        let (reply, answer) = oneshot::channel();
        let sent = lock(&self.prompts).as_ref().is_some_and(|prompts| {
            prompts
                .send(ShellPrompt::ReadLine {
                    prompt,
                    echo,
                    reply,
                })
                .is_ok()
        });
        if !sent {
            self.cancelled.store(true, Ordering::SeqCst);
            return None;
        }
        let started = Instant::now();
        let answer = {
            let _paused = self.clock.pause();
            answer.await
        };
        self.add_wait(started.elapsed());
        match answer {
            Ok(Some(line)) => Some(line),
            Ok(None) | Err(_) => {
                self.cancelled.store(true, Ordering::SeqCst);
                None
            }
        }
    }

    fn add_wait(&self, waited: Duration) {
        let mut total = lock(&self.waited);
        *total = Some(total.unwrap_or_default() + waited);
    }

    /// 暂停网络超时的时钟(解私钥这类纯本地的慢活用),守卫 drop 即恢复。
    pub(crate) fn pause(&self) -> PauseGuard<'_> {
        self.clock.pause()
    }

    pub(crate) fn clock(&self) -> &PauseClock {
        &self.clock
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn mark_host_key_declined(&self) {
        self.host_key_declined.store(true, Ordering::SeqCst);
    }

    pub(crate) fn host_key_declined(&self) -> bool {
        self.host_key_declined.load(Ordering::SeqCst)
    }

    /// 累计在终端里等用户的时长;从没等过为 `None`。
    pub(crate) fn waited(&self) -> Option<Duration> {
        *lock(&self.waited)
    }

    /// 摘掉提示通道:之后的 `notice` 静默丢弃、`read_line` 直接按取消返回,宿主那头的接收端随即
    /// 结束。
    pub(crate) fn finish(&self) {
        lock(&self.prompts).take();
    }
}

// ============================================================================
// 暂停时钟:等用户输入的时间不计入网络超时
// ============================================================================

/// 网络超时用的时钟:[`Self::pause`] 守卫存活期间不走。建连预算于是只算真正花在网络上的时间
/// (DNS / TCP / 握手 / 各次认证请求),用户想多久都行。
#[derive(Default)]
pub(crate) struct PauseClock {
    state: Mutex<PauseState>,
    /// 暂停 / 恢复时通知 [`Self::timeout`] 重算期限。
    changed: Notify,
}

#[derive(Default)]
struct PauseState {
    /// 嵌套暂停的层数。
    depth: usize,
    /// 当前这段暂停的起点(`depth > 0` 时才有)。
    since: Option<Instant>,
    /// 已经结束的那些暂停的总时长。
    total: Duration,
}

/// [`PauseClock::pause`] 的守卫,drop 即恢复。
pub(crate) struct PauseGuard<'a>(&'a PauseClock);

impl Drop for PauseGuard<'_> {
    fn drop(&mut self) {
        self.0.resume();
    }
}

/// [`PauseClock::timeout`] 到点。
pub(crate) struct PausedElapsed;

impl PauseClock {
    pub(crate) fn pause(&self) -> PauseGuard<'_> {
        {
            let mut state = lock(&self.state);
            if state.depth == 0 {
                state.since = Some(Instant::now());
            }
            state.depth += 1;
        }
        self.changed.notify_waiters();
        PauseGuard(self)
    }

    fn resume(&self) {
        {
            let mut state = lock(&self.state);
            state.depth = state.depth.saturating_sub(1);
            if state.depth == 0 {
                if let Some(since) = state.since.take() {
                    state.total += since.elapsed();
                }
            }
        }
        self.changed.notify_waiters();
    }

    /// 截至 `now` 暂停过的总时长(正在暂停的这一段算到 `now`),以及此刻是否在暂停。
    fn paused(&self, now: Instant) -> (Duration, bool) {
        let state = lock(&self.state);
        let running = state
            .since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        (state.total + running, state.depth > 0)
    }

    /// 从 `start` 起、预算 `budget` 的期限:暂停过多久就往后顺延多久。
    pub(crate) fn deadline(&self, start: Instant, budget: Duration) -> Instant {
        let now = Instant::now();
        start + budget + self.paused(now).0
    }

    /// 给 `future` 套上「`budget` 的网络时间」:暂停期间不计时(期限随之顺延),到点返回
    /// [`PausedElapsed`] 并丢弃 `future`。
    pub(crate) async fn timeout<F: Future>(
        &self,
        start: Instant,
        budget: Duration,
        future: F,
    ) -> Result<F::Output, PausedElapsed> {
        tokio::pin!(future);
        loop {
            // 先登记通知再读状态:读完之后才发生的暂停 / 恢复一定能把这一轮叫醒。
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (paused_for, paused_now) = self.paused(Instant::now());
            if paused_now {
                tokio::select! {
                    biased;
                    output = &mut future => return Ok(output),
                    () = &mut changed => continue,
                }
            }
            let deadline = start + budget + paused_for;
            tokio::select! {
                biased;
                output = &mut future => return Ok(output),
                () = &mut changed => continue,
                () = tokio::time::sleep_until(deadline) => {
                    let now = Instant::now();
                    let (paused_for, paused_now) = self.paused(now);
                    if !paused_now && now >= start + budget + paused_for {
                        return Err(PausedElapsed);
                    }
                }
            }
        }
    }
}

// ============================================================================
// 服务器给的文本进终端前的净化
// ============================================================================

/// 服务器给的文本(认证 banner、keyboard-interactive 的标题 / 说明 / 问询)要原样进终端 VT,
/// 先把控制字符换成可见的八进制转义 —— OpenSSH 同样如此,免得服务器借 ESC 序列改终端状态
/// (改标题、写剪贴板之类)。保留可打印字符与 TAB,各种换行统一成 `\r\n`,其余 C0 / DEL / C1
/// 写成 `\ooo`。
pub(crate) fn terminal_safe(text: &str) -> String {
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

/// [`terminal_safe`] 之后保证以换行结尾:作为单独的几行显示(banner、标题、说明)。
pub(crate) fn line_text(text: &str) -> String {
    let mut out = terminal_safe(text);
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out
}

/// 认证 banner 的显示文本。
pub(crate) fn banner_text(banner: &str) -> String {
    line_text(banner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_safe_escapes_controls_and_normalises_newlines() {
        assert_eq!(
            terminal_safe("a\nb\r\nc\rd\te"),
            "a\r\nb\r\nc\r\nd\te",
            "三种换行统一成 CRLF,TAB 原样"
        );
        assert_eq!(
            terminal_safe("\u{1b}[31mred\u{7}\u{7f}"),
            "\\033[31mred\\007\\177"
        );
        // C1 控制字符(U+009B 是单字节 CSI)同样转义;其余 Unicode 原样。
        assert_eq!(terminal_safe("\u{9b}2J 中文 é"), "\\2332J 中文 é");
        assert_eq!(line_text("Welcome"), "Welcome\r\n");
        assert_eq!(line_text("Welcome\n"), "Welcome\r\n");
        assert_eq!(banner_text(""), "\r\n");
    }

    #[tokio::test]
    async fn read_line_reports_answers_and_cancellation() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let interaction = Interaction::new(tx);
        let host = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(prompt) = rx.recv().await {
                match prompt {
                    ShellPrompt::Notice(text) => seen.push(format!("notice {text}")),
                    ShellPrompt::ReadLine {
                        prompt,
                        echo,
                        reply,
                    } => {
                        seen.push(format!("ask {prompt} {echo}"));
                        let answer = (prompt == "first: ").then(|| "one".to_string());
                        let _ = reply.send(answer);
                    }
                }
            }
            seen
        });
        interaction.notice("hello\r\n");
        assert_eq!(
            interaction
                .read_line("first: ".into(), true)
                .await
                .as_deref(),
            Some("one")
        );
        assert!(!interaction.is_cancelled());
        assert!(interaction.waited().is_some());
        assert_eq!(interaction.read_line("second: ".into(), false).await, None);
        assert!(interaction.is_cancelled(), "宿主回 None = 用户取消");
        interaction.finish();
        assert_eq!(
            host.await.unwrap(),
            ["notice hello\r\n", "ask first:  true", "ask second:  false"]
        );
        // 摘掉通道之后:提示静默丢弃,读一行直接按取消。
        interaction.notice("dropped");
        assert_eq!(interaction.read_line("x".into(), true).await, None);
    }

    #[tokio::test]
    async fn pause_clock_excludes_paused_time_from_the_budget() {
        let clock = PauseClock::default();
        let start = Instant::now();
        let budget = Duration::from_millis(150);
        // 期间暂停 300ms:不暂停的话 150ms 就到点,暂停时间不计入则能等到 future 完成。
        let finished = clock
            .timeout(start, budget, async {
                let paused = clock.pause();
                tokio::time::sleep(Duration::from_millis(300)).await;
                drop(paused);
                tokio::time::sleep(Duration::from_millis(50)).await;
                "done"
            })
            .await;
        assert!(matches!(finished, Ok("done")));
        assert!(clock.deadline(start, budget) >= start + budget + Duration::from_millis(300));

        // 不暂停:照常到点。
        let started = Instant::now();
        let elapsed = clock
            .timeout(
                Instant::now(),
                Duration::from_millis(100),
                std::future::pending::<()>(),
            )
            .await;
        assert!(elapsed.is_err());
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
