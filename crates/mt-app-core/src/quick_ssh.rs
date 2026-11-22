//! 鼠标右键「SSH 连接」的「已连接」状态追踪（纯逻辑，不依赖 GPUI）。
//!
//! # 为什么只追「右键连接 VPS」且与 AI 状态完全独立
//!
//! 右键快速连接（`pane::connect_ssh`）的**系统 OpenSSH 回退**那条路（连接勾了「终端使用系统
//! OpenSSH 客户端」）是往当前/新开本地 shell 写入一条 `ssh [-p …] [-i …] user@host\r` 命令，
//! 而不是直接以 `ssh` 作 PTY 根进程。默认的内置客户端（接管终端）不走本模块：那条路的
//! 「已连接」直接按会话生命周期报（`mt-app` 的 `pane::ssh`）。
//! 本状态机**只在 `arm` 被右键连接入口显式激活后**才运行：
//!
//! - **上升沿（判定已连接）**：跳过本地命令回显后，收到剥离转义序列的非错误、
//!   非交互提示、非 OpenSSH 过渡诊断的远端有效内容，并在 [`CONFIRM_WINDOW`]
//!   确认窗口之后经子进程树核查 `ssh` 进程仍在存活，才晋升为 `Connected`。
//!   确认窗口用于挡掉「登录前 Banner 紧接 `password:`」与「`/sbin/nologin` 打印
//!   一行后秒退」两类假阳性。
//! - **下降沿（清除已连接）**：原生 shell（PowerShell / cmd / bash / zsh 等）
//!   以 PTY 根进程下的 `ssh` 后代进程是否存活为权威判据——`exit`、Ctrl+D、`~.`、
//!   断网、kill 都能在一次轮询内清除，且远端嵌套 `ssh` 退出不会误清外层连接；
//!   WSL 等宿主机进程树看不到内部 Linux 进程的环境（[`SshProcessProbe::Unavailable`]）
//!   退化为扫描 OpenSSH 断开文案。
//! - **零侵入**：本状态不进入 `PaneStatus`、`mt-ai`、托盘与移动端协议，仅用于
//!   终端页签与项目列表的次级状态展示（AI 状态始终优先）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// 收到远端有效输出后，正式亮起「已连接」前的短确认窗口。
///
/// 挡掉两类瞬时假阳性：
/// 1. 远端配了 `Banner /etc/issue.net`，在 `password:` 之前先吐一段正文；
/// 2. 远端账号被拒（如 `/sbin/nologin`），吐一行提示后立刻断开。
pub const CONFIRM_WINDOW: Duration = Duration::from_millis(300);

/// PTY 已就绪后，等待 `ssh` 子进程在进程树里现身的宽限上限。
///
/// 超过这么久进程树里仍不见 `ssh`，说明本地 shell 没能启动它（比如卡在别的命令里
/// 或命令报错直接退回提示符），自动放弃追踪。
pub const START_PROCESS_TIMEOUT: Duration = Duration::from_secs(8);

/// 进程树不可用（WSL 降级路径）时，迟迟连不上的最长等待上限。
pub const UNAVAILABLE_CONNECT_TIMEOUT: Duration = Duration::from_secs(90);

/// 跨输出块保留的文本尾部字符数（与 `mt-pty::ssh` 同量级）。
const RESIDUAL_KEEP: usize = 384;

/// 跳过本地命令回显阶段最多缓存的字符数。
const ECHO_BUF_MAX: usize = 2048;

/// 若某款 shell 关闭了本地回显、始终不吐 `\n`，超过此窗口且 `ssh` 进程已出现则放行。
const ECHO_FALLBACK_DELAY: Duration = Duration::from_millis(1500);

/// 对 PTY 子进程树做一次 `ssh` 进程探测的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshProcessProbe {
    /// PTY 根进程的后代中存在 `ssh` / `ssh.exe` 进程。
    Running,
    /// PTY 根进程或其后代中不存在 `ssh` 进程。
    Exited,
    /// 当前环境无法观察壳内子进程（例如 Windows 宿主查 `wsl.exe` 内部的 Linux 进程，
    /// 或系统进程快照调用失败），状态机退化为纯输出扫描。
    Unavailable,
}

/// 状态机对外产出的可见变化。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickSshOutcome {
    /// 对外可见的「已连接」状态没有变化。
    Noop,
    /// 对外可见状态变了：`Some(name)` = 进入已连接，`None` = 从已连接清除。
    Changed(Option<String>),
}

/// 连接追踪的内部阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuickSshPhase {
    /// 未激活（没有右键发起的 SSH 追踪）。
    Inactive,
    /// 刚写入 `ssh …\r`，等待命令回显跨过换行并等待远端响应。
    Launching,
    /// 停在 OpenSSH 交互提示上（主机指纹确认 / 密码 / 口令 / 2FA）。
    WaitingPrompt,
    /// 用户（或自动填充）已对提示按下回车，等待 sshd 校验结果。
    Verifying,
    /// 已确认登录到远端 VPS。
    Connected,
}

/// 单个 pane 的右键 SSH 连接状态机。
#[derive(Clone, Debug)]
pub struct QuickSshTracker {
    phase: QuickSshPhase,
    conn_name: String,
    /// 发送的整条 `ssh ...` 命令去空白后的小写串，用于过滤折行回显。
    command_compact: String,
    /// 目标段 `user@host`（小写），辅助识别命令行回显。
    target_token: String,
    armed_at: Option<Instant>,
    /// PTY 回填完成（或 arm 时已就绪）的时刻；启动进程超时从这里起算。
    pty_ready_at: Option<Instant>,
    /// 是否已经跨过本地 shell 对 `ssh …\r` 命令行本身的回显。
    echo_passed: bool,
    echo_buf: String,
    /// 进程树探测是否已经见过 `ssh` 子进程出现。
    saw_ssh_process: bool,
    /// 当前环境进程树探测不可用（如 WSL），需在 Connected 下保留断开文案扫描。
    probe_unavailable: bool,
    /// 首次看到有效远端非提示非错误输出的时刻，满 [`CONFIRM_WINDOW`] 后确认晋升。
    candidate_since: Option<Instant>,
    /// 跨 chunk 的纯文本尾巴（已剥离 ANSI / OSC 转义）。
    residual: String,
}

impl Default for QuickSshTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickSshTracker {
    pub fn new() -> Self {
        Self {
            phase: QuickSshPhase::Inactive,
            conn_name: String::new(),
            command_compact: String::new(),
            target_token: String::new(),
            armed_at: None,
            pty_ready_at: None,
            echo_passed: false,
            echo_buf: String::new(),
            saw_ssh_process: false,
            probe_unavailable: false,
            candidate_since: None,
            residual: String::new(),
        }
    }

    pub fn phase(&self) -> QuickSshPhase {
        self.phase
    }

    /// 是否正在追踪中（需要读输出旁路或后台进程轮询）。
    pub fn is_active(&self) -> bool {
        self.phase != QuickSshPhase::Inactive
    }

    /// 是否已确认连接。
    pub fn is_connected(&self) -> bool {
        self.phase == QuickSshPhase::Connected
    }

    /// 当前已连接的展示名；未连接时为 `None`。
    pub fn connected_name(&self) -> Option<&str> {
        (self.phase == QuickSshPhase::Connected).then_some(self.conn_name.as_str())
    }

    /// 进程树轮询建议间隔：连接建立期快（250ms），连上后轻量保活（700ms）。
    pub fn poll_interval(&self) -> Option<Duration> {
        match self.phase {
            QuickSshPhase::Inactive => None,
            QuickSshPhase::Launching | QuickSshPhase::WaitingPrompt | QuickSshPhase::Verifying => {
                Some(Duration::from_millis(250))
            }
            QuickSshPhase::Connected => Some(Duration::from_millis(700)),
        }
    }

    /// 右键「SSH 连接」发起时调用。若此前正处于已连接态，先对外清除旧标记。
    pub fn arm(
        &mut self,
        conn_name: String,
        command: &str,
        pty_running: bool,
        now: Instant,
    ) -> QuickSshOutcome {
        let was_connected = self.phase == QuickSshPhase::Connected;
        self.phase = QuickSshPhase::Launching;
        self.conn_name = conn_name;
        self.command_compact = compact_lower(command);
        self.target_token = command
            .split_whitespace()
            .last()
            .unwrap_or("")
            .to_ascii_lowercase();
        self.armed_at = Some(now);
        self.pty_ready_at = pty_running.then_some(now);
        self.echo_passed = false;
        self.echo_buf.clear();
        self.saw_ssh_process = false;
        self.probe_unavailable = false;
        self.candidate_since = None;
        self.residual.clear();
        if was_connected {
            QuickSshOutcome::Changed(None)
        } else {
            QuickSshOutcome::Noop
        }
    }

    /// 新建终端回填 PTY 完成时调用，开始计 `ssh` 进程出现超时。
    pub fn on_pty_ready(&mut self, now: Instant) {
        if self.is_active() && self.pty_ready_at.is_none() {
            self.pty_ready_at = Some(now);
        }
    }

    /// PTY 退出 / 重启 / 关闭时立刻清场。
    pub fn reset(&mut self) -> QuickSshOutcome {
        let was_connected = self.phase == QuickSshPhase::Connected;
        self.disarm();
        if was_connected {
            QuickSshOutcome::Changed(None)
        } else {
            QuickSshOutcome::Noop
        }
    }

    /// 用户向 PTY 键入真实字节时的旁路通知。
    pub fn on_user_input(&mut self, bytes: &[u8], _now: Instant) {
        if matches!(
            self.phase,
            QuickSshPhase::Inactive | QuickSshPhase::Connected
        ) {
            return;
        }
        // Ctrl+C 打断连接或提示：先清候选，后续由进程探针决定 ssh 是退出了还是继续运行
        if bytes.contains(&0x03) {
            self.candidate_since = None;
            self.residual.clear();
            if self.phase == QuickSshPhase::WaitingPrompt {
                self.phase = QuickSshPhase::Verifying;
            }
            return;
        }
        if self.phase == QuickSshPhase::WaitingPrompt
            && (bytes.contains(&b'\r') || bytes.contains(&b'\n'))
        {
            self.phase = QuickSshPhase::Verifying;
            self.candidate_since = None;
            self.residual.clear();
        }
    }

    /// 远端 shell 发来 OSC 0/2 标题变化时的旁路通知（仅在已跨过回显且非等待提示时采信）。
    pub fn on_osc_title(&mut self, now: Instant) {
        if matches!(
            self.phase,
            QuickSshPhase::Launching | QuickSshPhase::Verifying
        ) && self.echo_passed
            && self.saw_ssh_process
            && self.candidate_since.is_none()
        {
            self.candidate_since = Some(now);
        }
    }

    /// 喂入一段 PTY 输出字节。
    ///
    /// 原生环境下已进入 `Connected` 后直接短路返回 `Noop`，不在高吞吐输出路径上
    /// 做字符串分配或转义剥离；仅在 WSL 降级模式下扫描断开文案。
    pub fn on_output(&mut self, bytes: &[u8], now: Instant) -> QuickSshOutcome {
        if self.phase == QuickSshPhase::Inactive {
            return QuickSshOutcome::Noop;
        }
        if self.phase == QuickSshPhase::Connected {
            if !self.probe_unavailable {
                return QuickSshOutcome::Noop;
            }
            return self.scan_disconnect_in_fallback(bytes);
        }

        let clean = strip_terminal_escapes(&String::from_utf8_lossy(bytes));
        let normalized = clean.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return QuickSshOutcome::Noop;
        }

        // 第一关：跳过本地 shell 对 `ssh …\r` 命令行本身的回显。
        let payload: String = if !self.echo_passed {
            self.echo_buf.push_str(&normalized);
            keep_last_chars(&mut self.echo_buf, ECHO_BUF_MAX);
            if let Some(nl) = self.echo_buf.find('\n') {
                let rest = self.echo_buf[nl + 1..].to_string();
                self.echo_buf.clear();
                self.echo_passed = true;
                rest
            } else if self.saw_ssh_process
                && self
                    .armed_at
                    .is_some_and(|t| now.saturating_duration_since(t) >= ECHO_FALLBACK_DELAY)
            {
                let rest = std::mem::take(&mut self.echo_buf);
                self.echo_passed = true;
                rest
            } else {
                return QuickSshOutcome::Noop;
            }
        } else {
            normalized
        };

        if payload.is_empty() {
            return QuickSshOutcome::Noop;
        }

        // 若正停在密码 / 指纹确认提示上：autofill 回写密码或用户按回车后，sshd 会先
        // 回一个换行 `\n`；跨过 `\n` 切入 Verifying 并清掉旧提示尾巴，只分析 `\n` 之后
        // 新到达的校验结果。
        let to_analyze = if self.phase == QuickSshPhase::WaitingPrompt {
            if let Some(nl) = payload.find('\n') {
                self.phase = QuickSshPhase::Verifying;
                self.candidate_since = None;
                self.residual.clear();
                &payload[nl + 1..]
            } else {
                // 仍在同一行（例如用户键入 yes 的回显），维持等待提示
                self.residual.push_str(&payload);
                keep_last_chars(&mut self.residual, RESIDUAL_KEEP);
                return QuickSshOutcome::Noop;
            }
        } else {
            payload.as_str()
        };

        if to_analyze.is_empty() {
            return QuickSshOutcome::Noop;
        }

        self.residual.push_str(to_analyze);
        keep_last_chars(&mut self.residual, RESIDUAL_KEEP);

        let residual_lower = self.residual.to_ascii_lowercase();
        let chunk_lower = to_analyze.to_ascii_lowercase();

        // 优先级 1：命中任何 OpenSSH 交互提示（指纹确认 / 密码 / passphrase / 2FA）。
        if is_ssh_prompt(&residual_lower) {
            self.phase = QuickSshPhase::WaitingPrompt;
            self.candidate_since = None;
            return QuickSshOutcome::Noop;
        }

        // 优先级 2：本块或尾巴命中认证/网络错误或断开信息 → 取消候选。
        if contains_ssh_error(&chunk_lower) {
            self.candidate_since = None;
            return QuickSshOutcome::Noop;
        }

        // 优先级 3：本块里是否包含非空白、非命令回显、非 OpenSSH 过渡诊断的有效远端内容。
        if self.has_remote_content(to_analyze) && self.candidate_since.is_none() {
            self.candidate_since = Some(now);
        }

        QuickSshOutcome::Noop
    }

    /// 进程树探针回报一次结果，驱动「确认晋升已连接」与「断开清除」。
    pub fn on_process_probe(&mut self, probe: SshProcessProbe, now: Instant) -> QuickSshOutcome {
        match self.phase {
            QuickSshPhase::Inactive => QuickSshOutcome::Noop,
            QuickSshPhase::Connected => match probe {
                SshProcessProbe::Running => QuickSshOutcome::Noop,
                SshProcessProbe::Unavailable => {
                    self.probe_unavailable = true;
                    QuickSshOutcome::Noop
                }
                SshProcessProbe::Exited => {
                    self.disarm();
                    QuickSshOutcome::Changed(None)
                }
            },
            QuickSshPhase::Launching | QuickSshPhase::WaitingPrompt | QuickSshPhase::Verifying => {
                match probe {
                    SshProcessProbe::Running => {
                        self.saw_ssh_process = true;
                        self.try_promote_connected(now)
                    }
                    SshProcessProbe::Unavailable => {
                        self.probe_unavailable = true;
                        let res = self.try_promote_connected(now);
                        if res != QuickSshOutcome::Noop {
                            return res;
                        }
                        let start = self.pty_ready_at.or(self.armed_at).unwrap_or(now);
                        if self.candidate_since.is_none()
                            && now.saturating_duration_since(start) >= UNAVAILABLE_CONNECT_TIMEOUT
                        {
                            self.disarm();
                        }
                        QuickSshOutcome::Noop
                    }
                    SshProcessProbe::Exited => {
                        if self.saw_ssh_process {
                            // ssh 进程已经起过又退出了（失败 / 秒退 / 用户取消）
                            self.disarm();
                            return QuickSshOutcome::Noop;
                        }
                        // 还没见过 ssh：若 PTY 已就绪且超过启动宽限，放弃追踪
                        if let Some(ready_at) = self.pty_ready_at
                            && now.saturating_duration_since(ready_at) >= START_PROCESS_TIMEOUT
                        {
                            self.disarm();
                        }
                        QuickSshOutcome::Noop
                    }
                }
            }
        }
    }

    fn try_promote_connected(&mut self, now: Instant) -> QuickSshOutcome {
        if self.phase == QuickSshPhase::WaitingPrompt {
            return QuickSshOutcome::Noop;
        }
        let Some(since) = self.candidate_since else {
            return QuickSshOutcome::Noop;
        };
        if now.saturating_duration_since(since) < CONFIRM_WINDOW {
            return QuickSshOutcome::Noop;
        }
        self.phase = QuickSshPhase::Connected;
        self.residual.clear();
        self.echo_buf.clear();
        QuickSshOutcome::Changed(Some(self.conn_name.clone()))
    }

    fn scan_disconnect_in_fallback(&mut self, bytes: &[u8]) -> QuickSshOutcome {
        let clean = strip_terminal_escapes(&String::from_utf8_lossy(bytes));
        self.residual.push_str(&clean);
        keep_last_chars(&mut self.residual, RESIDUAL_KEEP);
        let lower = self.residual.to_ascii_lowercase();
        if is_ssh_disconnect_text(&lower) {
            self.disarm();
            QuickSshOutcome::Changed(None)
        } else {
            QuickSshOutcome::Noop
        }
    }

    fn has_remote_content(&self, text: &str) -> bool {
        for raw_line in text.split('\n') {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // 过滤折行残留的本地命令行回显
            let compact = compact_lower(trimmed);
            if !compact.is_empty()
                && !self.command_compact.is_empty()
                && self.command_compact.contains(&compact)
            {
                continue;
            }
            let lower = trimmed.to_ascii_lowercase();
            if !self.target_token.is_empty()
                && lower.starts_with("ssh ")
                && lower.contains(&self.target_token)
            {
                continue;
            }
            if is_ssh_transient_line(&lower) || contains_ssh_error(&lower) || is_ssh_prompt(&lower)
            {
                continue;
            }
            return true;
        }
        false
    }

    fn disarm(&mut self) {
        self.phase = QuickSshPhase::Inactive;
        self.conn_name.clear();
        self.command_compact.clear();
        self.target_token.clear();
        self.armed_at = None;
        self.pty_ready_at = None;
        self.echo_passed = false;
        self.echo_buf.clear();
        self.saw_ssh_process = false;
        self.probe_unavailable = false;
        self.candidate_since = None;
        self.residual.clear();
    }
}

/// 从系统进程表 `(pid, ppid, exe_name)` 中判定 `root_pid` 下是否有存活的 `ssh` 后代进程。
///
/// 纯函数收口在此处，各平台只负责采集快照并喂进来：
/// - `root_pid` 本身不在表中 → [`SshProcessProbe::Exited`]
/// - `root_pid` 本身是 `wsl.exe` / `wslhost.exe` → [`SshProcessProbe::Unavailable`]
///   （宿主机看不到 WSL 虚拟机内部的 Linux 进程）
/// - 否则沿 `ppid -> pid` 做带 `visited` 防环的 BFS，命中任意 `ssh` / `ssh.exe` 后代即
///   [`SshProcessProbe::Running`]，未命中即 [`SshProcessProbe::Exited`]。
pub fn has_ssh_in_process_table(root_pid: u32, entries: &[(u32, u32, &str)]) -> SshProcessProbe {
    let mut children: HashMap<u32, Vec<(u32, &str)>> = HashMap::new();
    let mut root_name: Option<&str> = None;

    for &(pid, ppid, name) in entries {
        if pid == root_pid {
            root_name = Some(name);
        }
        if pid != ppid {
            children.entry(ppid).or_default().push((pid, name));
        }
    }

    let Some(root_name) = root_name else {
        return SshProcessProbe::Exited;
    };
    if is_wsl_host_exe(root_name) {
        return SshProcessProbe::Unavailable;
    }

    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    visited.insert(root_pid);
    queue.push_back(root_pid);

    while let Some(cur) = queue.pop_front() {
        let Some(kids) = children.get(&cur) else {
            continue;
        };
        for &(kid_pid, kid_name) in kids {
            if is_ssh_client_exe(kid_name) {
                return SshProcessProbe::Running;
            }
            if visited.insert(kid_pid) {
                queue.push_back(kid_pid);
            }
        }
    }

    SshProcessProbe::Exited
}

fn is_ssh_client_exe(name: &str) -> bool {
    let base = exe_basename(name);
    base.eq_ignore_ascii_case("ssh") || base.eq_ignore_ascii_case("ssh.exe")
}

fn is_wsl_host_exe(name: &str) -> bool {
    let base = exe_basename(name);
    base.eq_ignore_ascii_case("wsl.exe")
        || base.eq_ignore_ascii_case("wsl")
        || base.eq_ignore_ascii_case("wslhost.exe")
}

fn exe_basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path).trim()
}

fn compact_lower(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn keep_last_chars(text: &mut String, keep: usize) {
    if keep == 0 {
        text.clear();
        return;
    }
    if let Some((cut, _)) = text.char_indices().rev().nth(keep - 1) {
        text.drain(..cut);
    }
}

/// 剥离终端控制序列（CSI / OSC / DCS / APC / PM / SS3 / 双字节 ESC），只保留可见字符与换行。
///
/// 比 `mt_core::strip_ansi_codes` 多处理 OSC（`\x1b]...\x07` 或 `\x1b]...\x1b\\`）与
/// DCS/APC/PM 字符串序列：本地 shell（如 Windows Terminal / PowerShell 的 OSC 9;9、
/// OSC 0）在命令回显期间常发出 OSC 序列，不剥干净会把标题 payload 当成正文误判。
pub fn strip_terminal_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                // CSI: ESC [ ... <0x40..=0x7e>
                Some('[') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c2) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... (BEL | ST)
                Some(']') => {
                    chars.next();
                    skip_string_terminated(&mut chars, true);
                }
                // DCS / SOS / PM / APC: ESC (P|X|^|_) ... ST
                Some('P' | 'X' | '^' | '_') => {
                    chars.next();
                    skip_string_terminated(&mut chars, false);
                }
                // SS3: ESC O <final>
                Some('O') => {
                    chars.next();
                    chars.next();
                }
                // 字符集选择等三字节序列: ESC ( ) * + <final>
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                    chars.next();
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if c == '\n' || c == '\r' || c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

fn skip_string_terminated(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, allow_bel: bool) {
    while let Some(c) = chars.next() {
        if allow_bel && c == '\x07' {
            return;
        }
        if c == '\x1b' && chars.peek() == Some(&'\\') {
            chars.next();
            return;
        }
    }
}

/// 文本尾部是否停在 OpenSSH 交互提示上（主机密钥确认 / 密码 / passphrase / 2FA）。
fn is_ssh_prompt(lower: &str) -> bool {
    // 1. 首连主机密钥确认：`Are you sure you want to continue connecting (yes/no/[fingerprint])? `
    const HOST_KEY_MARK: &str = "continue connecting (yes/no";
    if let Some(at) = lower.rfind(HOST_KEY_MARK) {
        let rest = &lower[at + HOST_KEY_MARK.len()..];
        if let Some(q) = rest.find('?') {
            let after_q = &rest[q + 1..];
            if q <= 24 && !rest[..q].contains('\n') && !after_q.contains('\n') {
                return true;
            }
        }
    }

    // 2. 密码 / 私钥口令 / 2FA 提示：只看最后一行
    let last_line = lower.rsplit('\n').next().unwrap_or(lower).trim_end();
    if last_line.ends_with("password:")
        || last_line.ends_with("passphrase:")
        || last_line.ends_with("verification code:")
        || last_line.ends_with("one-time password:")
        || last_line.ends_with("enter pin:")
    {
        return true;
    }
    if last_line.contains("enter passphrase for key") && last_line.ends_with(':') {
        return true;
    }
    false
}

/// OpenSSH 连接建立期的过渡/诊断行（既不是失败，也不代表已经进入远端会话）。
fn is_ssh_transient_line(lower: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "the authenticity of host ",
        "warning: permanently added ",
        "authenticated to ",
        "this key is not known",
        "this host key is known",
        "pseudo-terminal will not be allocated",
        "debug1:",
        "debug2:",
        "debug3:",
        "@@@@@@@@",
        "offending ",
        "add correct host key",
        "host key for ",
        "sign_and_send_pubkey:",
        "load key ",
        "update the host key",
        "remove with:",
    ];
    if PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // 各类指纹行：`ED25519 key fingerprint is SHA256:...` / `RSA key fingerprint is ...`
    lower.contains("key fingerprint is ")
}

/// OpenSSH 认证失败 / 网络错误 / 协议协商失败。
fn contains_ssh_error(lower: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "permission denied",
        "connection refused",
        "connection timed out",
        "operation timed out",
        "could not resolve hostname",
        "host key verification failed",
        "no route to host",
        "network is unreachable",
        "connection reset",
        "connection closed",
        "broken pipe",
        "too many authentication failures",
        "remote host identification has changed",
        "kex_exchange_identification",
        "no matching key exchange",
        "no matching host key",
        "no matching cipher",
        "no matching mac",
        "unable to negotiate",
        "banner exchange:",
        "bad configuration option",
        "client_loop: send disconnect",
        "unprotected private key file",
        "bad permissions",
    ];
    if PATTERNS.iter().any(|p| lower.contains(p)) {
        return true;
    }
    // OpenSSH 客户端自身 stderr 错误一律以 `ssh:` 开头（例如 `ssh: connect to host ...`）
    lower
        .split('\n')
        .any(|line| line.trim_start().starts_with("ssh:"))
}

/// WSL 降级模式下用于判定 SSH 已断开的文案。
fn is_ssh_disconnect_text(lower: &str) -> bool {
    (lower.contains("connection to ") && lower.contains(" closed"))
        || lower.contains("connection closed by ")
        || lower.contains("disconnected from ")
        || lower.contains("client_loop: send disconnect")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> Instant {
        // 用固定基准推算单调时刻，避免真实 sleep
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    #[test]
    fn 公钥免密极速登录_确认窗口后亮起_进程退出后立刻清除() {
        let mut tr = QuickSshTracker::new();
        assert_eq!(
            tr.arm("prod-vps".into(), "ssh root@1.2.3.4", true, t(0)),
            QuickSshOutcome::Noop
        );
        // 本地 shell 回显命令行
        assert_eq!(
            tr.on_output(b"ssh root@1.2.3.4\r\n", t(20)),
            QuickSshOutcome::Noop
        );
        // 远端极速返回 MOTD + prompt（第 80ms，此时进程探针还没跑过）
        assert_eq!(
            tr.on_output(
                b"Last login: Sat Nov 22 10:00:00 2026\r\nroot@vps:~# ",
                t(80)
            ),
            QuickSshOutcome::Noop
        );
        // 第 250ms 探针看到 ssh 活着，但距离 80ms 还不足 300ms 确认窗口
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(250)),
            QuickSshOutcome::Noop
        );
        assert!(!tr.is_connected());
        // 第 400ms 满 300ms 且 ssh 存活 → 晋升为已连接
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(400)),
            QuickSshOutcome::Changed(Some("prod-vps".into()))
        );
        assert_eq!(tr.connected_name(), Some("prod-vps"));

        // 已连接后在原生模式下刷屏不改状态
        assert_eq!(
            tr.on_output(b"Connection to inner-host closed.\r\n", t(1000)),
            QuickSshOutcome::Noop
        );
        assert!(tr.is_connected());

        // 真正退出外层 ssh：进程探针报告 Exited → 立即清除
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Exited, t(1700)),
            QuickSshOutcome::Changed(None)
        );
        assert!(!tr.is_active());
        assert_eq!(tr.connected_name(), None);
    }

    #[test]
    fn 登录前_banner_紧接密码提示_确认窗口内取消候选不闪灯() {
        let mut tr = QuickSshTracker::new();
        tr.arm("vps".into(), "ssh root@vps", true, t(0));
        tr.on_output(b"ssh root@vps\r\n", t(10));
        // Banner 先到达
        tr.on_output(b"Authorized access only!\r\n", t(100));
        // 50ms 后密码提示到达 → 立刻进 WaitingPrompt，取消候选
        tr.on_output(b"root@vps's password: ", t(150));
        assert_eq!(tr.phase(), QuickSshPhase::WaitingPrompt);
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(600)),
            QuickSshOutcome::Noop
        );
        assert!(!tr.is_connected());
    }

    #[test]
    fn 自动填充错误密码_绝不误判已连接_随后手输正确密码成功亮起() {
        let mut tr = QuickSshTracker::new();
        tr.arm("vps".into(), "ssh root@vps", true, t(0));
        tr.on_output(b"ssh root@vps\r\n", t(10));
        tr.on_output(b"root@vps's password: ", t(100));
        tr.on_process_probe(SshProcessProbe::Running, t(200));

        // autofill 填完密码，sshd 先吐空换行并暂停 1.5s 校验
        tr.on_output(b"\r\n", t(220));
        assert_eq!(tr.phase(), QuickSshPhase::Verifying);
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(1000)),
            QuickSshOutcome::Noop
        );
        assert!(!tr.is_connected());

        // 校验失败：Permission denied + 新密码提示
        tr.on_output(
            b"Permission denied, please try again.\r\nroot@vps's password: ",
            t(1600),
        );
        assert_eq!(tr.phase(), QuickSshPhase::WaitingPrompt);
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(2200)),
            QuickSshOutcome::Noop
        );
        assert!(!tr.is_connected());

        // 用户手输正确密码按回车
        tr.on_user_input(b"\r", t(2500));
        assert_eq!(tr.phase(), QuickSshPhase::Verifying);
        tr.on_output(b"\r\nWelcome to Ubuntu 24.04\r\nroot@vps:~# ", t(2800));
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(3200)),
            QuickSshOutcome::Changed(Some("vps".into()))
        );
    }

    #[test]
    fn 首连主机指纹确认与过渡提示不误触发_确认后真正登录才亮起() {
        let mut tr = QuickSshTracker::new();
        tr.arm("new-vps".into(), "ssh root@9.9.9.9", true, t(0));
        tr.on_output(b"ssh root@9.9.9.9\r\n", t(10));
        tr.on_output(
            b"The authenticity of host '9.9.9.9 (9.9.9.9)' can't be established.\r\n\
              ED25519 key fingerprint is SHA256:abcdef.\r\n\
              Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
            t(100),
        );
        assert_eq!(tr.phase(), QuickSshPhase::WaitingPrompt);
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(600)),
            QuickSshOutcome::Noop
        );

        // 用户键入 yes 并回车
        tr.on_user_input(b"yes\r", t(700));
        tr.on_output(
            b"Warning: Permanently added '9.9.9.9' (ED25519) to the list of known hosts.\r\n",
            t(750),
        );
        // 仅过渡行不触发已连接
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(1200)),
            QuickSshOutcome::Noop
        );
        assert!(!tr.is_connected());

        // 真正拿到远端提示符
        tr.on_output(b"root@new-vps:~# ", t(1300));
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(1650)),
            QuickSshOutcome::Changed(Some("new-vps".into()))
        );
    }

    #[test]
    fn 连接被拒与网络黑洞绝不亮灯() {
        // 1. Connection refused 秒退
        let mut tr = QuickSshTracker::new();
        tr.arm("bad".into(), "ssh root@bad", true, t(0));
        tr.on_output(b"ssh root@bad\r\n", t(10));
        tr.on_process_probe(SshProcessProbe::Running, t(50));
        tr.on_output(
            b"ssh: connect to host bad port 22: Connection refused\r\n",
            t(80),
        );
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Exited, t(120)),
            QuickSshOutcome::Noop
        );
        assert!(!tr.is_active());

        // 2. 网络黑洞：ssh 活着但无输出
        let mut tr2 = QuickSshTracker::new();
        tr2.arm("blackhole".into(), "ssh root@10.255.255.1", true, t(0));
        tr2.on_output(b"ssh root@10.255.255.1\r\n", t(10));
        assert_eq!(
            tr2.on_process_probe(SshProcessProbe::Running, t(10_000)),
            QuickSshOutcome::Noop
        );
        assert!(!tr2.is_connected());
    }

    #[test]
    fn osc序列与psreadline高亮回显不误判为远端输出() {
        let mut tr = QuickSshTracker::new();
        tr.arm(
            "vps".into(),
            "ssh -p 2222 -i \"C:/keys/id\" root@1.2.3.4",
            true,
            t(0),
        );
        // PSReadLine 高亮 + OSC 9;9 cwd 通告 + 换行
        tr.on_output(
            b"\x1b]9;9;\"D:\\Git\\mini-term\"\x07\x1b[93mssh\x1b[0m -p 2222 -i \"C:/keys/id\" root@1.2.3.4\r\n",
            t(20),
        );
        // 哪怕换行之后又带了一段 OSC 0 标题和折行残留，仍不能触发
        tr.on_output(b"\x1b]0;PowerShell\x07root@1.2.3.4\r\n", t(40));
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(500)),
            QuickSshOutcome::Noop
        );
        assert!(!tr.is_connected());
    }

    #[test]
    fn 未拉起ssh进程超时后自动放弃() {
        let mut tr = QuickSshTracker::new();
        tr.arm("vps".into(), "ssh root@vps", false, t(0));
        // PTY 还在后台起，不会提前超时
        tr.on_process_probe(SshProcessProbe::Exited, t(10_000));
        assert!(tr.is_active());
        // 回填完成
        tr.on_pty_ready(t(10_000));
        tr.on_process_probe(SshProcessProbe::Exited, t(17_000));
        assert!(tr.is_active());
        // 超过 8s 仍未见到 ssh 进程 → 自动放弃
        tr.on_process_probe(SshProcessProbe::Exited, t(18_500));
        assert!(!tr.is_active());
    }

    #[test]
    fn wsl降级路径可亮起并通过断开文案清除() {
        let mut tr = QuickSshTracker::new();
        tr.arm("wsl-vps".into(), "ssh root@vps", true, t(0));
        tr.on_output(b"ssh root@vps\r\n", t(10));
        tr.on_output(b"root@vps:~# ", t(100));
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Unavailable, t(450)),
            QuickSshOutcome::Changed(Some("wsl-vps".into()))
        );
        assert_eq!(
            tr.on_output(b"logout\r\nConnection to vps closed.\r\n", t(1000)),
            QuickSshOutcome::Changed(None)
        );
        assert!(!tr.is_active());
    }

    #[test]
    fn 已连接后重新发起右键连接先清旧状态() {
        let mut tr = QuickSshTracker::new();
        tr.arm("vps-a".into(), "ssh a@1.1.1.1", true, t(0));
        tr.on_output(b"ssh a@1.1.1.1\r\n$ ", t(20));
        assert_eq!(
            tr.on_process_probe(SshProcessProbe::Running, t(400)),
            QuickSshOutcome::Changed(Some("vps-a".into()))
        );
        assert_eq!(
            tr.arm("vps-b".into(), "ssh b@2.2.2.2", true, t(500)),
            QuickSshOutcome::Changed(None)
        );
        assert!(!tr.is_connected());
    }

    #[test]
    fn 进程树探测支持多级子进程_wsl识别与防环() {
        // pwsh(10) -> cmd(20) -> ssh.exe(30)
        let table = [
            (10, 1, "pwsh.exe"),
            (20, 10, "cmd.exe"),
            (30, 20, "SSH.EXE"),
        ];
        assert_eq!(
            has_ssh_in_process_table(10, &table),
            SshProcessProbe::Running
        );

        // ssh 退出后
        let exited = [(10, 1, "pwsh.exe"), (20, 10, "cmd.exe")];
        assert_eq!(
            has_ssh_in_process_table(10, &exited),
            SshProcessProbe::Exited
        );

        // 根进程是 wsl.exe → Unavailable
        let wsl = [(10, 1, "wsl.exe"), (20, 10, "conhost.exe")];
        assert_eq!(
            has_ssh_in_process_table(10, &wsl),
            SshProcessProbe::Unavailable
        );

        // 含 PID 复用环不死循环
        let cyclic = [(10, 20, "bash"), (20, 10, "bash")];
        assert_eq!(
            has_ssh_in_process_table(10, &cyclic),
            SshProcessProbe::Exited
        );

        // 根进程已不在表中
        assert_eq!(
            has_ssh_in_process_table(999, &table),
            SshProcessProbe::Exited
        );
    }

    #[test]
    fn 剥离osc与csi转义序列() {
        assert_eq!(
            strip_terminal_escapes("\x1b]0;user@host: ~\x07\x1b[32mhello\x1b[0m"),
            "hello"
        );
        assert_eq!(
            strip_terminal_escapes("\x1b]9;9;\"C:\\work\"\x1b\\ok"),
            "ok"
        );
    }
}
