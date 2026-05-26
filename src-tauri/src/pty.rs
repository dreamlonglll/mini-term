use mt_core::{parse_wsl_unc, scan_ssh_prompt, strip_ansi_codes, SshPromptScan};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PtyOutputPayload {
    pty_id: u32,
    data: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PtyExitPayload {
    pty_id: u32,
    exit_code: i32,
}

/// 当 cwd 命中 WSL UNC 路径时,`create_pty` 会强制改用 `wsl.exe -d <distro> --cd <unix-path>`
/// 启动,并向前端推送本 payload 以便弹一次性 toast。重写发生在用户配置的 shell 之前
/// (无视用户的 cmd/pwsh 设置),与 Windows Terminal 的 `MangleStartingDirectoryForWSL` 行为一致。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WslShellOverridePayload {
    pty_id: u32,
    distro: String,
    unix_path: String,
}

/// 判断是否要把 cwd 重写为 WSL 启动器。返回 `Some((distro, unix_path))` 时,
/// 调用方必须把 shell 切到 `wsl.exe`、args 切到 `["-d", &distro, "--cd", &unix_path]`,
/// 并把 portable-pty 的 cwd 改成一个 Windows 端合法的目录(如 `%USERPROFILE%` 或 `C:\`),
/// 避免 ConPTY 在 `is_dir()` 检查 UNC 路径失败时静默退回 `$USERPROFILE`。
///
/// 仅做纯字符串解析,跨平台行为一致 —— 在 Linux/macOS 上,普通路径不会匹配 `\\` 前缀,
/// 因此 `parse_wsl_unc` 返回 None,函数也返回 None,保持原启动逻辑不变。
fn decide_wsl_override(cwd: &str) -> Option<(String, String)> {
    parse_wsl_unc(cwd).map(|wsl| (wsl.distro, wsl.unix_path))
}

/// 选一个 Windows 端合法的兜底 cwd 给 portable-pty,
/// 避免把 WSL UNC 直接传给 ConPTY 触发 `$USERPROFILE` 静默 fallback。
fn fallback_windows_cwd() -> String {
    std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
}

/// 为 WSL 启动器分支拼装 `WSLENV` 环境变量的 value。
///
/// 输入 `user_envs` 已经被外层过滤过(剔除 `MINITERM_*` 前缀与用户输入的 `WSLENV` key);
/// 本函数只负责把剩余的 key 加上 `/u` flag 并用 `:` 连接,再把宿主已有的 `WSLENV`
/// (若存在且非空)追加在尾部合并 —— 不覆盖,与 JetBrains IDEA terminal / wslgit 对齐。
///
/// flag 选 `/u`(仅 Win→WSL 方向,不做路径翻译)的决策依据见 PRD 与
/// `.trellis/tasks/05-26-wsl-env-vars-injection/research/wslenv-mechanism.md`。
///
/// 返回 `None` 当且仅当 `user_envs` 为空且宿主无 `WSLENV` —— 此时不应注入 WSLENV,
/// 否则会用空字符串覆盖宿主既有值。
fn build_wslenv_value(user_envs: &[(String, String)], host_wslenv: Option<&str>) -> Option<String> {
    let mut parts: Vec<String> = user_envs.iter().map(|(k, _)| format!("{}/u", k)).collect();
    if let Some(existing) = host_wslenv {
        if !existing.is_empty() {
            parts.push(existing.to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(":"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UserSubmit {
    pub line: String,
    pub ts: i64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiUserSubmitPayload {
    pub pty_id: u32,
    pub line: String,
    pub ts: i64,
}

struct PtyInstance {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

#[derive(Clone)]
enum EscapeState {
    None,
    Escape,
    Csi(String),
    Ss3,
}

impl Default for EscapeState {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Default)]
struct InputState {
    line: Vec<char>,
    cursor: usize,
    escape: EscapeState,
    bracketed_paste: bool,
    allow_line_snapshot: bool,
}

impl InputState {
    fn clear_line(&mut self) {
        self.line.clear();
        self.cursor = 0;
        self.escape = EscapeState::None;
        self.allow_line_snapshot = false;
    }

    fn insert_char(&mut self, ch: char) {
        self.line.insert(self.cursor, ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.line.remove(self.cursor);
    }

    fn delete(&mut self) {
        if self.cursor < self.line.len() {
            self.line.remove(self.cursor);
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        if self.cursor < self.line.len() {
            self.cursor += 1;
        }
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.line.len();
    }

    fn take_line(&mut self) -> String {
        let line = self.line.iter().collect();
        self.clear_line();
        line
    }

    fn apply_csi(&mut self, sequence: &str) {
        match sequence {
            "200~" => self.bracketed_paste = true,
            "201~" => self.bracketed_paste = false,
            "C" => self.move_right(),
            "D" => self.move_left(),
            "H" | "1~" | "7~" => self.move_home(),
            "F" | "4~" | "8~" => self.move_end(),
            "3~" => self.delete(),
            // Up/Down and other editing shortcuts can replace the whole shell line.
            // We can't reconstruct those mutations reliably from input alone.
            "A" | "B" => {
                self.clear_line();
                self.allow_line_snapshot = true;
            }
            _ => self.clear_line(),
        }
    }

    fn apply_ss3(&mut self, code: char) {
        match code {
            'C' => self.move_right(),
            'D' => self.move_left(),
            'H' => self.move_home(),
            'F' => self.move_end(),
            _ => self.clear_line(),
        }
    }

    fn consume_escape_char(&mut self, ch: char) -> bool {
        match &mut self.escape {
            EscapeState::None => false,
            EscapeState::Escape => {
                self.escape = match ch {
                    '[' => EscapeState::Csi(String::new()),
                    'O' => EscapeState::Ss3,
                    _ => {
                        self.clear_line();
                        EscapeState::None
                    }
                };
                true
            }
            EscapeState::Csi(sequence) => {
                sequence.push(ch);
                if ('@'..='~').contains(&ch) {
                    let completed = std::mem::take(sequence);
                    self.escape = EscapeState::None;
                    self.apply_csi(&completed);
                }
                true
            }
            EscapeState::Ss3 => {
                self.escape = EscapeState::None;
                self.apply_ss3(ch);
                true
            }
        }
    }
}

const AI_COMMANDS: &[&str] = &["claude", "codex", "opencode"];

/// 这些标志表示非交互命令（仅输出信息后退出），不应触发 AI 会话状态
const NON_INTERACTIVE_FLAGS: &[&str] = &["-v", "--version", "-h", "--help", "-p", "--print"];

/// AI 会话中的显式退出命令
const AI_EXIT_COMMANDS: &[&str] = &[
    "/exit", "exit", // Claude Code & Codex 通用
    "/quit", "quit",    // Claude Code & Codex 通用
    ":quit",   // Codex 交互式退出
    "/logout", // Codex 退出
];

/// 连续两次 Ctrl+C 退出的时间窗口
const DOUBLE_CTRLC_WINDOW: Duration = Duration::from_millis(1000);

/// 按下 Enter 后扫描输出以检测 AI 命令 echo 的时间窗口
const AI_ENTER_SCAN_WINDOW: Duration = Duration::from_millis(2000);

/// PTY resize 后的 TUI 重绘冷却窗口
///
/// 窗口内的 PTY 输出不刷新 last_output 时间戳。用于屏蔽 Claude/Codex/OpenCode 等 TUI
/// 应用在收到 ConPTY resize 信号后重绘 Alternate Screen Buffer 产生的伪输出,
/// 避免 process_monitor 把这些重绘误判为 AI 活跃,导致 ai-working 状态闪烁以及
/// 误触发 ai-working → ai-idle 的"任务完成"通知。
const RESIZE_COOLDOWN: Duration = Duration::from_millis(800);

/// 终端焦点切换后的 TUI 重绘冷却窗口
///
/// xterm.js 在 TUI 开启 DEC 私有模式 1004 (sendFocus) 后,会在 textarea
/// 获得/失去焦点时向 PTY 写入 CSI I / CSI O。Claude/Codex/OpenCode 等应用收到这些
/// 焦点事件后会做局部重绘(光标/状态反馈),产生伪输出。若不加冷却,重绘数据
/// 会刷新 last_output,被 process_monitor 误判为 AI 活跃,导致仅仅点击/切出
/// 终端就把 ai-idle 推成 ai-working。
///
/// 与 RESIZE_COOLDOWN 对齐为 800ms:AI 进程调度延迟在慢机器/WSL 下并不比 ConPTY
/// resize 响应更可控,保守对齐更稳妥。
const FOCUS_COOLDOWN: Duration = Duration::from_millis(800);

/// 终端焦点事件的 CSI 序列(xterm.js 在 sendFocus 模式下写入 PTY)
const FOCUS_IN_SEQ: &str = "\x1b[I";
const FOCUS_OUT_SEQ: &str = "\x1b[O";

fn command_word_matches_ai(word: &str) -> bool {
    let word = word.trim_matches(|c| matches!(c, '"' | '\'' | '`'));
    let basename = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let basename = [".exe", ".cmd", ".bat", ".ps1"]
        .iter()
        .find_map(|suffix| basename.strip_suffix(suffix))
        .unwrap_or(basename);
    let basename = basename.to_lowercase();
    AI_COMMANDS.iter().any(|&ai| basename == ai)
}

fn is_interactive_ai_command(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let mut first_word = words.next().unwrap_or("");
    if first_word == "&" {
        first_word = words.next().unwrap_or("");
    }
    if !command_word_matches_ai(first_word) {
        return false;
    }

    !words.any(|w| {
        let flag = w.to_lowercase();
        NON_INTERACTIVE_FLAGS.iter().any(|&f| flag == f)
    })
}

fn line_contains_ai_command(line: &str) -> bool {
    let line = strip_ansi_codes(line);
    let line = line.trim();
    if line.is_empty() {
        return false;
    }

    if is_interactive_ai_command(line) {
        return true;
    }

    // xterm 行快照通常包含 shell prompt，例如 "PS D:\repo> claude"。
    // 对常见 prompt 分隔符取最后一段，避免把 prompt 内容当作命令解析。
    for marker in [">", "$ ", "# ", "% "] {
        if let Some(idx) = line.rfind(marker) {
            if is_interactive_ai_command(&line[idx + marker.len()..]) {
                return true;
            }
        }
    }

    false
}

/// 检查 PTY 输出中是否包含 AI 命令被 echo（例如 "PS C:\> claude" 或单独的 "claude"）
fn output_contains_ai_command(output: &str) -> bool {
    strip_ansi_codes(output)
        .lines()
        .any(line_contains_ai_command)
}

struct SshAutofillState {
    password: String,
    /// 累加的输出尾部,用于跨缓冲块匹配密码提示
    residual: String,
    /// 已填充或已禁用(命中错误密码)后置位,后续输出不再处理
    done: bool,
}

#[derive(Clone)]
pub struct PtyManager {
    instances: Arc<Mutex<HashMap<u32, PtyInstance>>>,
    next_id: Arc<Mutex<u32>>,
    last_output: Arc<Mutex<HashMap<u32, Instant>>>,
    ai_sessions: Arc<Mutex<HashSet<u32>>>,
    input_states: Arc<Mutex<HashMap<u32, InputState>>>,
    last_ctrlc: Arc<Mutex<HashMap<u32, Instant>>>,
    last_enter: Arc<Mutex<HashMap<u32, Instant>>>,
    pending_submits: Arc<Mutex<HashMap<u32, Vec<UserSubmit>>>>,
    /// resize 冷却窗口结束时间:在此之前 PTY 输出不刷新 last_output
    tui_redraw_cooldown_until: Arc<Mutex<HashMap<u32, Instant>>>,
    /// SSH 密码自动填充状态(arm_ssh_autofill 注册,命中密码提示后回写)
    ssh_autofill: Arc<Mutex<HashMap<u32, SshAutofillState>>>,
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            instances: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
            last_output: Arc::new(Mutex::new(HashMap::new())),
            ai_sessions: Arc::new(Mutex::new(HashSet::new())),
            input_states: Arc::new(Mutex::new(HashMap::new())),
            last_ctrlc: Arc::new(Mutex::new(HashMap::new())),
            last_enter: Arc::new(Mutex::new(HashMap::new())),
            pending_submits: Arc::new(Mutex::new(HashMap::new())),
            tui_redraw_cooldown_until: Arc::new(Mutex::new(HashMap::new())),
            ssh_autofill: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn get_pty_ids(&self) -> Vec<u32> {
        self.instances.lock().unwrap().keys().copied().collect()
    }

    pub fn has_recent_output(&self, pty_id: u32, within: Duration) -> bool {
        let map = self.last_output.lock().unwrap();
        map.get(&pty_id).map_or(false, |t| t.elapsed() < within)
    }

    pub fn is_ai_session(&self, pty_id: u32) -> bool {
        self.ai_sessions.lock().unwrap().contains(&pty_id)
    }

    pub fn drain_submits(&self, pty_id: u32) -> Vec<UserSubmit> {
        self.pending_submits
            .lock()
            .unwrap()
            .remove(&pty_id)
            .unwrap_or_default()
    }

    /// 延长 TUI 重绘冷却窗口。采用 max 语义,不会缩短已有的更长冷却。
    /// resize 与 focus 共用同一冷却字段(效果一致:抑制 TUI 重绘刷新 last_output)。
    pub fn bump_cooldown(&self, pty_id: u32, duration: Duration) {
        if let Ok(mut map) = self.tui_redraw_cooldown_until.lock() {
            let new_until = Instant::now() + duration;
            let final_until = match map.get(&pty_id).copied() {
                Some(old) if old > new_until => old,
                _ => new_until,
            };
            map.insert(pty_id, final_until);
        }
    }

    pub fn is_in_cooldown(&self, pty_id: u32) -> bool {
        self.tui_redraw_cooldown_until
            .lock()
            .ok()
            .and_then(|m| m.get(&pty_id).copied())
            .map_or(false, |until| Instant::now() < until)
    }

    /// 若 data 是 xterm 焦点事件序列(CSI I / CSI O),打开焦点冷却窗口,
    /// 避免 TUI 应用对焦点事件的重绘响应被误判为 AI 活跃。
    pub fn note_focus_event(&self, pty_id: u32, data: &str) {
        if data == FOCUS_IN_SEQ || data == FOCUS_OUT_SEQ {
            self.bump_cooldown(pty_id, FOCUS_COOLDOWN);
        }
    }

    /// 注册某个 pty 的 SSH 密码自动填充。再次调用会重置状态(覆盖密码、清除 done)。
    pub fn arm_ssh_autofill(&self, pty_id: u32, password: String) {
        self.ssh_autofill.lock().unwrap().insert(
            pty_id,
            SshAutofillState {
                password,
                residual: String::new(),
                done: false,
            },
        );
    }

    /// 扫描 pty 输出:命中密码提示则把已保存的密码回写到 PTY(每个 pty 只填一次);
    /// 命中 "Permission denied, please try again." 则永久禁用,避免连灌错误密码。
    fn process_ssh_autofill(&self, pty_id: u32, data: &str) {
        let to_fill: Option<String> = {
            let mut map = self.ssh_autofill.lock().unwrap();
            match map.get_mut(&pty_id) {
                Some(st) if !st.done => {
                    st.residual.push_str(&strip_ansi_codes(data));
                    // 仅保留尾部,解决跨 16ms/4096B 分块的提示匹配;按 char 边界截断
                    const KEEP: usize = 256;
                    let count = st.residual.chars().count();
                    if count > KEEP {
                        st.residual = st.residual.chars().skip(count - KEEP).collect();
                    }
                    match scan_ssh_prompt(&st.residual) {
                        SshPromptScan::AuthFailed => {
                            st.done = true;
                            None
                        }
                        SshPromptScan::Password => {
                            st.done = true;
                            Some(st.password.clone())
                        }
                        SshPromptScan::None => None,
                    }
                }
                _ => None,
            }
        };
        if let Some(password) = to_fill {
            if let Ok(mut instances) = self.instances.lock() {
                if let Some(inst) = instances.get_mut(&pty_id) {
                    let _ = inst.writer.write_all(password.as_bytes());
                    let _ = inst.writer.write_all(b"\r");
                    let _ = inst.writer.flush();
                }
            }
        }
    }

    #[cfg(test)]
    pub fn track_input(&self, pty_id: u32, data: &str) {
        self.track_input_with_line_snapshot(pty_id, data, None);
    }

    pub fn track_input_with_line_snapshot(
        &self,
        pty_id: u32,
        data: &str,
        line_snapshot: Option<&str>,
    ) {
        let in_ai = self.is_ai_session(pty_id);
        let mut enter_ai = false;
        let mut exit_ai = false;
        {
            let mut states = self.input_states.lock().unwrap();
            let state = states.entry(pty_id).or_default();
            for ch in data.chars() {
                if state.consume_escape_char(ch) {
                    continue;
                }
                if ch == '\x1b' {
                    state.escape = EscapeState::Escape;
                    continue;
                }
                if state.bracketed_paste {
                    match ch {
                        '\r' | '\n' => state.insert_char('\n'),
                        c if c >= ' ' => state.insert_char(c),
                        _ => {}
                    }
                    continue;
                }
                match ch {
                    '\x03' => {
                        state.clear_line();
                        if in_ai {
                            // Ctrl+C: 单次取消当前任务，连续两次退出 AI 会话
                            let mut last = self.last_ctrlc.lock().unwrap();
                            let now = Instant::now();
                            if let Some(prev) = last.get(&pty_id) {
                                if now.duration_since(*prev) < DOUBLE_CTRLC_WINDOW {
                                    exit_ai = true;
                                    last.remove(&pty_id);
                                } else {
                                    last.insert(pty_id, now);
                                }
                            } else {
                                last.insert(pty_id, now);
                            }
                        }
                    }
                    '\x04' => {
                        state.clear_line();
                        if in_ai {
                            // Ctrl+D (EOF) → 退出 AI 会话
                            exit_ai = true;
                        }
                    }
                    '\r' | '\n' => {
                        let allow_line_snapshot = state.allow_line_snapshot;
                        let raw = state.take_line();
                        let trimmed = raw.trim();
                        let snapshot_is_ai = allow_line_snapshot
                            && line_snapshot.map(line_contains_ai_command).unwrap_or(false);
                        // 记录 Enter 时间，供输出扫描用。空回车不打开扫描窗口，
                        // 避免 shell autosuggestion 出现在重绘输出中时被当成命令 echo。
                        if !trimmed.is_empty() || snapshot_is_ai {
                            self.last_enter
                                .lock()
                                .unwrap()
                                .insert(pty_id, Instant::now());
                        }
                        if !trimmed.is_empty() && self.is_ai_session(pty_id) {
                            let ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0);
                            self.pending_submits
                                .lock()
                                .unwrap()
                                .entry(pty_id)
                                .or_default()
                                .push(UserSubmit {
                                    line: trimmed.to_string(),
                                    ts,
                                });
                        }
                        let cmd = trimmed.to_lowercase();
                        if in_ai {
                            // AI 会话中：识别显式退出命令
                            if AI_EXIT_COMMANDS.iter().any(|&c| cmd == c) {
                                exit_ai = true;
                            }
                        } else {
                            // 非 AI 会话：检测 AI 命令启动。优先使用本地输入状态；
                            // 对上方向键历史、Tab 补全等 shell 改写行的场景，使用
                            // 前端在 Enter 前捕获的可见行快照补判。
                            if is_interactive_ai_command(trimmed) || snapshot_is_ai {
                                enter_ai = true;
                            }
                        }
                    }
                    '\t' => {
                        if !state.line.is_empty() {
                            state.allow_line_snapshot = true;
                        }
                    }
                    '\x7f' | '\x08' => {
                        state.backspace();
                    }
                    c if c >= ' ' => state.insert_char(c),
                    _ => {}
                }
            }
        }
        if enter_ai || exit_ai {
            let mut sessions = self.ai_sessions.lock().unwrap();
            if enter_ai {
                sessions.insert(pty_id);
            } else {
                sessions.remove(&pty_id);
            }
        }
    }
}

#[tauri::command]
pub fn create_pty(
    app: AppHandle,
    state: tauri::State<'_, PtyManager>,
    hook_state: tauri::State<'_, crate::hook_server::HookState>,
    shell: String,
    args: Vec<String>,
    cwd: String,
    envs: Option<Vec<(String, String)>>,
) -> Result<u32, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    // WSL UNC cwd 检测:命中则忽略用户配置的 shell,改用 wsl.exe 启动,
    // 并把 portable-pty 的 cwd 换成 Windows 端合法目录(详见 decide_wsl_override 注释)。
    let wsl_override = decide_wsl_override(&cwd);

    let (effective_shell, effective_args, effective_cwd) = match &wsl_override {
        // WSL 分支:启动的是宿主 wsl.exe,WSL VM 内的 claude/codex 子进程
        // process_monitor 看不到,因此本路径下 AI 进程识别(ai-working/ai-idle 状态)
        // 会失效 —— PRD Out of Scope 已列,见
        // `.trellis/tasks/05-25-support-wsl-project-root/prd.md`。
        Some((distro, unix_path)) => (
            "wsl.exe".to_string(),
            vec![
                "-d".to_string(),
                distro.clone(),
                "--cd".to_string(),
                unix_path.clone(),
            ],
            fallback_windows_cwd(),
        ),
        None => (shell, args, cwd),
    };

    let mut cmd = CommandBuilder::new(&effective_shell);
    for arg in &effective_args {
        cmd.arg(arg);
    }
    cmd.cwd(&effective_cwd);

    // Advertise terminal capabilities so TUI apps (Claude Code, etc.)
    // enable colors and advanced cursor rendering.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // Ensure UTF-8 encoding for proper CJK/emoji rendering.
    // On Windows, Git for Windows (MSYS2) checks LANG to determine terminal encoding;
    // without it, git falls back to the system ANSI code page (e.g. GBK on Chinese Windows),
    // causing mojibake in commit messages. LESSCHARSET tells git's pager (less) to handle
    // UTF-8 bytes instead of escaping them as <XX> hex sequences.
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_CTYPE", "C.UTF-8");
    cmd.env("LESSCHARSET", "utf-8");

    // 先获取 pty_id，以便注入环境变量供 hook 子进程继承
    let pty_id = {
        let mut next = state.next_id.lock().unwrap();
        let id = *next;
        *next += 1;
        id
    };

    // 注入 PTY ID 环境变量，Claude Code / Codex 的 hook 子进程可通过此变量
    // 关联到具体的终端 pane
    cmd.env("MINITERM_PTY_ID", pty_id.to_string());

    // WSL 重写发生时通知前端弹一次性 toast。pty_id 必须先分配,否则 payload 无法携带。
    if let Some((distro, unix_path)) = wsl_override.as_ref() {
        let _ = app.emit(
            "wsl-shell-override",
            WslShellOverridePayload {
                pty_id,
                distro: distro.clone(),
                unix_path: unix_path.clone(),
            },
        );
    }

    // 注入 hook 服务器端口，避免 miniterm-hook 每次都从文件读取端口
    let hook_port = hook_state.get_port();
    if hook_port > 0 {
        cmd.env("MINITERM_HOOK_PORT", hook_port.to_string());
    }

    // 项目级用户环境变量注入。
    // - 顺序在 mini-term 内部 env(TERM/LANG/MINITERM_*) 之后,允许用户覆盖 TERM/LANG 等
    //   标准变量。
    // - `MINITERM_` 前缀属于 mini-term 内部 hook 协议,这里再做一次防御性过滤:
    //   即便用户绕过前端校验(手改 config.json)塞进来,也不会覆盖 MINITERM_PTY_ID /
    //   MINITERM_HOOK_PORT 等保留变量。
    // - 用户 envs 里的 `WSLENV` key 整条跳过:WSL 分支需要由 mini-term 拼装 WSLENV
    //   的 value(K1/u:K2/u:... + 宿主既有 WSLENV),允许用户覆盖会破坏拼接结果。
    // - WSL 启动器分支:wsl.exe 进程 env 不会自动透传给 Linux bash,必须配合 WSLENV
    //   机制使用 —— 在 cmd.env(k, v) 之外额外拼一个 `WSLENV=K1/u:K2/u:...` 注入到
    //   wsl.exe 进程 env,WSL init 才会在 distro 内为 bash 设置同名变量。flag 选 `/u`
    //   (仅 Win→WSL,不做路径翻译),与 JetBrains IDEA terminal 对齐;宿主既有 WSLENV
    //   追加在尾部合并(不覆盖)。决策详见 PRD 与 research/wslenv-mechanism.md。
    let user_envs: Vec<(String, String)> = envs
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| !k.starts_with("MINITERM_") && k != "WSLENV")
        .collect();

    if wsl_override.is_some() && !user_envs.is_empty() {
        let host_wslenv = std::env::var("WSLENV").ok();
        if let Some(value) = build_wslenv_value(&user_envs, host_wslenv.as_deref()) {
            cmd.env("WSLENV", value);
        }
    }

    for (k, v) in &user_envs {
        cmd.env(k, v);
    }

    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;

    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let master = pair.master;

    // Channel + flush 定时器实现 16ms 批量缓冲
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let instances_clone = state.instances.clone();
    let pty_id_for_reader = pty_id;

    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let app_flush = app.clone();
    let last_output = state.last_output.clone();
    let ai_sessions_flush = state.ai_sessions.clone();
    let last_enter_flush = state.last_enter.clone();
    let mgr_flush = (*state).clone();
    thread::spawn(move || {
        let mut pending = Vec::new();

        loop {
            match rx.recv_timeout(Duration::from_millis(16)) {
                Ok(data) => {
                    pending.extend(data);
                    while let Ok(more) = rx.try_recv() {
                        pending.extend(more);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if !pending.is_empty() {
                        let data = String::from_utf8_lossy(&pending).into_owned();
                        let _ = app_flush.emit(
                            "pty-output",
                            PtyOutputPayload {
                                pty_id: pty_id_for_reader,
                                data,
                            },
                        );
                    }

                    let exit_code = {
                        let mut instances = instances_clone.lock().unwrap();
                        if let Some(mut inst) = instances.remove(&pty_id_for_reader) {
                            inst.child
                                .try_wait()
                                .ok()
                                .flatten()
                                .map(|status| status.exit_code() as i32)
                                .unwrap_or(0)
                        } else {
                            0
                        }
                    };

                    let _ = app_flush.emit(
                        "pty-exit",
                        PtyExitPayload {
                            pty_id: pty_id_for_reader,
                            exit_code,
                        },
                    );
                    return;
                }
            }

            if !pending.is_empty() {
                // 找到最后一个完整 UTF-8 字符的边界，避免截断多字节字符
                let valid_len = {
                    let mut i = pending.len();
                    // 从末尾向前扫描，找到可能不完整的 UTF-8 序列起始位置
                    while i > 0 {
                        i -= 1;
                        let byte = pending[i];
                        if byte < 0x80 {
                            // ASCII 字符，本身就是完整的
                            i = pending.len();
                            break;
                        } else if byte >= 0xC0 {
                            // 多字节序列的起始字节，检查序列是否完整
                            let expected_len = if byte >= 0xF0 {
                                4
                            } else if byte >= 0xE0 {
                                3
                            } else {
                                2
                            };
                            let remaining = pending.len() - i;
                            if remaining >= expected_len {
                                // 序列完整
                                i = pending.len();
                            }
                            // 否则 i 就是不完整序列的起始位置
                            break;
                        }
                        // 0x80..0xBF 是延续字节，继续向前找起始字节
                    }
                    i
                };

                if valid_len > 0 {
                    let data = String::from_utf8_lossy(&pending[..valid_len]).into_owned();

                    // SSH 密码自动填充:扫描输出,命中密码提示则回写已保存的密码
                    mgr_flush.process_ssh_autofill(pty_id_for_reader, &data);

                    // 基于输出扫描检测 AI 会话（补偿上箭头历史调用 / PSReadLine 补全）：
                    // 若在 Enter 后 2 秒内收到包含 AI 命令 echo 的输出，自动标记为 AI 会话
                    {
                        let recently_entered = last_enter_flush
                            .lock()
                            .unwrap()
                            .get(&pty_id_for_reader)
                            .map(|t| t.elapsed() < AI_ENTER_SCAN_WINDOW)
                            .unwrap_or(false);
                        if recently_entered {
                            let mut sessions = ai_sessions_flush.lock().unwrap();
                            if !sessions.contains(&pty_id_for_reader)
                                && output_contains_ai_command(&data)
                            {
                                sessions.insert(pty_id_for_reader);
                            }
                        }
                    }

                    let _ = app_flush.emit(
                        "pty-output",
                        PtyOutputPayload {
                            pty_id: pty_id_for_reader,
                            data,
                        },
                    );

                    // 冷却窗口内(resize 或 focus 事件后)的输出不刷新 last_output。
                    // Claude/Codex 等 TUI 应用在收到 ConPTY resize / 焦点事件后会重绘
                    // Alternate Screen Buffer,这些重绘数据不能被 process_monitor
                    // 当作 AI 活跃信号,否则会触发 ai-working 状态闪烁和假完成通知。
                    if !mgr_flush.is_in_cooldown(pty_id_for_reader) {
                        if let Ok(mut map) = last_output.lock() {
                            map.insert(pty_id_for_reader, Instant::now());
                        }
                    }
                }

                // 保留不完整的 UTF-8 字节到下次刷新
                if valid_len < pending.len() {
                    let leftover = pending[valid_len..].to_vec();
                    pending.clear();
                    pending.extend(leftover);
                } else {
                    pending.clear();
                }
            }
        }
    });

    {
        let mut instances = state.instances.lock().unwrap();
        instances.insert(
            pty_id,
            PtyInstance {
                writer,
                master,
                child,
            },
        );
    }

    Ok(pty_id)
}

/// Windows ConPTY 无法一次处理大量输入数据（粘贴长文本时只剩最后一行）。
/// 将数据按行拆分，每行写入后加短暂延迟，给 ConPTY 时间消化。
/// 短数据（普通键盘输入）直接写入不受影响。
fn write_pty_chunked(writer: &mut dyn Write, data: &str) -> Result<(), String> {
    const CHUNK_THRESHOLD: usize = 128;
    const INTER_LINE_DELAY: Duration = Duration::from_millis(1);

    let bytes = data.as_bytes();

    if !cfg!(windows) || bytes.len() <= CHUNK_THRESHOLD || !data.contains('\n') {
        writer.write_all(bytes).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        return Ok(());
    }

    // 按行拆分写入，保留每行的换行符
    let mut start = 0;
    while start < bytes.len() {
        let end = match bytes[start..].iter().position(|&b| b == b'\n') {
            Some(pos) => start + pos + 1, // 包含 \n
            None => bytes.len(),          // 最后一段无换行
        };
        writer
            .write_all(&bytes[start..end])
            .map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        start = end;
        if start < bytes.len() {
            thread::sleep(INTER_LINE_DELAY);
        }
    }
    Ok(())
}

#[tauri::command]
pub fn write_pty(
    app: tauri::AppHandle,
    state: tauri::State<'_, PtyManager>,
    pty_id: u32,
    data: String,
    line_snapshot: Option<String>,
) -> Result<(), String> {
    // 在写入前打开焦点冷却窗口:AI 对焦点事件的重绘响应几乎立即抵达 reader,
    // 必须早于那之前把冷却建立起来。
    state.note_focus_event(pty_id, &data);
    {
        let mut instances = state.instances.lock().unwrap();
        let instance = instances.get_mut(&pty_id).ok_or("PTY not found")?;
        write_pty_chunked(&mut *instance.writer, &data)?;
    }
    state.track_input_with_line_snapshot(pty_id, &data, line_snapshot.as_deref());

    for submit in state.drain_submits(pty_id) {
        let _ = app.emit(
            "ai-user-submit",
            AiUserSubmitPayload {
                pty_id,
                line: submit.line,
                ts: submit.ts,
            },
        );
    }
    Ok(())
}

#[tauri::command]
pub fn resize_pty(
    state: tauri::State<'_, PtyManager>,
    pty_id: u32,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    {
        let instances = state.instances.lock().unwrap();
        let instance = instances.get(&pty_id).ok_or("PTY not found")?;
        instance
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
    }

    // 开启冷却窗口:之后 RESIZE_COOLDOWN 内的 PTY 输出(主要是 TUI 重绘)
    // 不会刷新 last_output,从而避免被 process_monitor 误判为 AI 活跃。
    state.bump_cooldown(pty_id, RESIZE_COOLDOWN);

    Ok(())
}

#[tauri::command]
pub fn kill_pty(
    state: tauri::State<'_, PtyManager>,
    hook_state: tauri::State<'_, crate::hook_server::HookState>,
    pty_id: u32,
) -> Result<(), String> {
    // Remove metadata maps immediately so subsequent lookups return nothing.
    let instance = state.instances.lock().unwrap().remove(&pty_id);
    state.last_output.lock().unwrap().remove(&pty_id);
    state.ai_sessions.lock().unwrap().remove(&pty_id);
    state.input_states.lock().unwrap().remove(&pty_id);
    state.last_ctrlc.lock().unwrap().remove(&pty_id);
    state.last_enter.lock().unwrap().remove(&pty_id);
    state.pending_submits.lock().unwrap().remove(&pty_id);
    state.ssh_autofill.lock().unwrap().remove(&pty_id);
    state
        .tui_redraw_cooldown_until
        .lock()
        .unwrap()
        .remove(&pty_id);
    // 清理 hook 状态
    hook_state.remove(pty_id);

    // Drop the PTY instance on a background thread.
    //
    // On Windows, dropping `master` triggers `ClosePseudoConsole()`, which is
    // synchronous and blocks until every process in the console session exits.
    // When a long-running AI process (claude/codex) is still alive, this call
    // never returns on the calling thread, freezing the whole app ("未响应").
    //
    // Fix: kill the shell process first (stops new output), then drop on a
    // background thread so the UI stays responsive regardless of how long
    // cleanup takes.
    if let Some(mut inst) = instance {
        thread::spawn(move || {
            // Kill the shell (e.g., pwsh). This signals the ConPTY server that
            // the primary process is gone, allowing ClosePseudoConsole to return
            // once in-flight output is drained.
            let _ = inst.child.kill();
            // Now drop writer → master → child in background.
            drop(inst);
        });
    }

    Ok(())
}

/// 为某个 pty 注册 SSH 密码自动填充。前端在写入 `ssh` 命令前调用(连接配有密码时)。
#[tauri::command]
pub fn arm_ssh_autofill(
    state: tauri::State<'_, PtyManager>,
    pty_id: u32,
    password: String,
) -> Result<(), String> {
    state.arm_ssh_autofill(pty_id, password);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_claude_command() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn detect_codex_command() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "codex\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn non_ai_command_not_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "npm install\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn prompt_in_ai_session_stays() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        // 在 Claude 内输入提示词不应退出 AI 会话
        mgr.track_input(1, "fix the bug\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn single_ctrl_c_does_not_exit_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        // 单次 Ctrl+C 是取消当前任务，不退出
        mgr.track_input(1, "\x03");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn double_ctrl_c_exits_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        // 连续两次 Ctrl+C 退出 AI 会话
        mgr.track_input(1, "\x03");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "\x03");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn ctrl_d_exits_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "\x04");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn slash_exit_exits_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "/exit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn exit_command_exits_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "exit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn slash_quit_exits_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "/quit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn quit_exits_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "codex\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "quit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn colon_quit_exits_codex_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "codex\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, ":quit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn slash_logout_exits_codex_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "codex\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "/logout\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn claude_with_interactive_args() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude --model opus\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn claude_version_not_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude -v\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn claude_long_version_not_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude --version\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn claude_help_not_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude -h\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn claude_print_not_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude -p \"hello\"\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn codex_version_not_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "codex --version\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn codex_help_not_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "codex --help\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn backspace_corrects_input() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claue\x7fde\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn empty_enter_keeps_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn char_by_char_input() {
        let mgr = PtyManager::new();
        for ch in "claude\r".chars() {
            mgr.track_input(1, &ch.to_string());
        }
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn left_right_arrows_preserve_inline_edit_for_claude() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "clade");
        mgr.track_input(1, "\x1b[D");
        mgr.track_input(1, "\x1b[D");
        mgr.track_input(1, "u");
        mgr.track_input(1, "\x1b[C");
        mgr.track_input(1, "\x1b[C");
        mgr.track_input(1, "\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn split_escape_sequence_still_moves_cursor() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "clade");
        mgr.track_input(1, "\x1b");
        mgr.track_input(1, "[D");
        mgr.track_input(1, "\x1b");
        mgr.track_input(1, "[D");
        mgr.track_input(1, "u\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn edited_non_interactive_flag_does_not_start_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude --versin");
        mgr.track_input(1, "\x1b[D");
        mgr.track_input(1, "o\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn drain_submits_returns_empty_initially() {
        let mgr = PtyManager::new();
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn drain_submits_clears_after_call() {
        let mgr = PtyManager::new();
        mgr.pending_submits
            .lock()
            .unwrap()
            .entry(1)
            .or_default()
            .push(UserSubmit {
                line: "test".into(),
                ts: 0,
            });
        let first = mgr.drain_submits(1);
        assert_eq!(first.len(), 1);
        let second = mgr.drain_submits(1);
        assert!(second.is_empty());
    }

    #[test]
    fn track_input_does_not_submit_entering_command_itself() {
        // "claude\r" 本身是进入 AI 会话的命令,此时 is_ai_session 还是 false
        // 因为 ai_sessions.insert 发生在 Enter 分支的后续 enter_ai 处理中
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.drain_submits(1).is_empty());
        assert!(mgr.is_ai_session(1)); // 但会话状态已建立
    }

    #[test]
    fn track_input_pushes_submit_in_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "fix the bug\r");
        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 1);
        assert_eq!(submits[0].line, "fix the bug");
        assert!(submits[0].ts > 0);
    }

    #[test]
    fn track_input_no_submit_outside_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "npm install\r");
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn track_input_no_submit_on_empty_enter() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "\r"); // 空回车
        mgr.track_input(1, "   \r"); // 仅空白
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn track_input_submits_multiple_in_working_window() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "first question\r");
        mgr.track_input(1, "follow up\r"); // ai-working 中再次 Enter
        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 2);
        assert_eq!(submits[0].line, "first question");
        assert_eq!(submits[1].line, "follow up");
    }

    #[test]
    fn track_input_no_submit_for_bracketed_multiline_paste() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "\x1b[200~first pasted line\nsecond pasted line\x1b[201~");
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn track_input_submits_once_after_bracketed_multiline_paste_enter() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "\x1b[200~first pasted line\rsecond pasted line\x1b[201~");
        mgr.track_input(1, "\r");

        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 1);
        assert_eq!(submits[0].line, "first pasted line\nsecond pasted line");
    }

    #[test]
    fn track_input_no_submit_on_arrow_keys() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "\x1b[A"); // 上方向键
        mgr.track_input(1, "\x1b[B"); // 下方向键
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn line_snapshot_detects_ai_command_after_history_navigation() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "\x1b[A"); // shell/PSReadLine restores a previous command
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> claude"));
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn empty_enter_with_ai_autosuggestion_snapshot_does_not_enter_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input_with_line_snapshot(1, "\r", Some("D:\\Git\\mini-term> claude"));
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn empty_enter_with_ai_autosuggestion_snapshot_does_not_open_output_scan_window() {
        let mgr = PtyManager::new();
        mgr.track_input_with_line_snapshot(1, "\r", Some("D:\\Git\\mini-term> claude"));
        assert!(!mgr.last_enter.lock().unwrap().contains_key(&1));
    }

    #[test]
    fn history_navigation_with_non_ai_snapshot_does_not_open_output_scan_window() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "\x1b[B");
        mgr.track_input_with_line_snapshot(1, "\r", Some("D:\\Git\\mini-term>"));
        assert!(!mgr.last_enter.lock().unwrap().contains_key(&1));
    }

    #[test]
    fn line_snapshot_detects_ai_command_after_tab_completion() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "cla");
        mgr.track_input(1, "\t"); // shell completion changes visible line to "claude"
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> claude"));
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn line_snapshot_respects_non_interactive_ai_flags() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "\x1b[A");
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> codex --help"));
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn focus_in_sequence_opens_cooldown() {
        let mgr = PtyManager::new();
        assert!(!mgr.is_in_cooldown(1));
        mgr.note_focus_event(1, "\x1b[I");
        assert!(mgr.is_in_cooldown(1));
    }

    #[test]
    fn focus_out_sequence_opens_cooldown() {
        let mgr = PtyManager::new();
        mgr.note_focus_event(1, "\x1b[O");
        assert!(mgr.is_in_cooldown(1));
    }

    #[test]
    fn ordinary_input_does_not_open_cooldown() {
        let mgr = PtyManager::new();
        mgr.note_focus_event(1, "a");
        mgr.note_focus_event(1, "\r");
        mgr.note_focus_event(1, "ls -la\r");
        assert!(!mgr.is_in_cooldown(1));
    }

    #[test]
    fn arrow_keys_do_not_open_cooldown() {
        let mgr = PtyManager::new();
        mgr.note_focus_event(1, "\x1b[A");
        mgr.note_focus_event(1, "\x1b[B");
        mgr.note_focus_event(1, "\x1b[C");
        mgr.note_focus_event(1, "\x1b[D");
        assert!(!mgr.is_in_cooldown(1));
    }

    #[test]
    fn focus_event_embedded_in_longer_input_is_not_matched() {
        // 只有严格等于焦点序列才触发冷却,避免用户粘贴的文本意外命中。
        // xterm.js 的 focus event 一定是一条独立的 onData(来自 triggerDataEvent),
        // 不会和其他数据拼接。
        let mgr = PtyManager::new();
        mgr.note_focus_event(1, "prefix\x1b[Isuffix");
        assert!(!mgr.is_in_cooldown(1));
    }

    #[test]
    fn bump_cooldown_uses_max_semantics() {
        let mgr = PtyManager::new();
        // 先写一个长冷却,再写短冷却不应缩短它。
        mgr.bump_cooldown(1, Duration::from_secs(10));
        assert!(mgr.is_in_cooldown(1));
        let long_until = mgr
            .tui_redraw_cooldown_until
            .lock()
            .unwrap()
            .get(&1)
            .copied()
            .unwrap();

        mgr.bump_cooldown(1, Duration::from_millis(50));
        let after_short = mgr
            .tui_redraw_cooldown_until
            .lock()
            .unwrap()
            .get(&1)
            .copied()
            .unwrap();
        assert_eq!(long_until, after_short, "短冷却不应覆盖更长的已有冷却");
    }

    #[test]
    fn cooldown_is_per_pty() {
        let mgr = PtyManager::new();
        mgr.note_focus_event(1, "\x1b[I");
        assert!(mgr.is_in_cooldown(1));
        assert!(!mgr.is_in_cooldown(2));
    }

    #[test]
    fn arm_ssh_autofill_registers_state() {
        let mgr = PtyManager::new();
        mgr.arm_ssh_autofill(7, "secret".into());
        assert!(mgr.ssh_autofill.lock().unwrap().contains_key(&7));
    }

    // === WSL UNC 启动器重写检测 ===
    // 完整的 create_pty 会 spawn 真实 shell,不适合在单测里跑;
    // 这里只验证 decide_wsl_override 这层纯函数的分支选择,覆盖
    // "WSL UNC 触发重写 / 普通路径不触发 / Linux 路径不触发" 三种 PRD 要求场景。

    #[test]
    fn wsl_override_triggered_by_wsl_dollar_unc() {
        let result = decide_wsl_override(r"\\wsl$\Ubuntu\home\u\proj");
        assert_eq!(
            result,
            Some(("Ubuntu".to_string(), "/home/u/proj".to_string()))
        );
    }

    #[test]
    fn wsl_override_triggered_by_wsl_localhost_unc() {
        let result = decide_wsl_override(r"\\wsl.localhost\Ubuntu-22.04\home\u\proj");
        assert_eq!(
            result,
            Some(("Ubuntu-22.04".to_string(), "/home/u/proj".to_string()))
        );
    }

    #[test]
    fn wsl_override_triggered_by_verbatim_unc() {
        // Rust canonicalize 在 WSL UNC 上输出 \\?\UNC\<host>\<rest>;
        // 与裸 \\wsl$\ 等价,decide_wsl_override 必须识别两种形式。
        let result = decide_wsl_override(r"\\?\UNC\wsl$\Ubuntu\home\u");
        assert_eq!(result, Some(("Ubuntu".to_string(), "/home/u".to_string())));
    }

    #[test]
    fn wsl_override_not_triggered_by_windows_local_path() {
        assert!(decide_wsl_override(r"C:\Users\u\proj").is_none());
        assert!(decide_wsl_override(r"D:\Git\mini-term").is_none());
    }

    #[test]
    fn wsl_override_not_triggered_by_unix_path() {
        // Linux/macOS 平台传入的普通绝对路径不应被识别。
        assert!(decide_wsl_override("/home/u/proj").is_none());
        assert!(decide_wsl_override("/").is_none());
    }

    #[test]
    fn wsl_override_not_triggered_by_non_wsl_unc() {
        // 普通文件共享 UNC 不能被误识别为 WSL。
        assert!(decide_wsl_override(r"\\server\share\folder").is_none());
        assert!(decide_wsl_override(r"\\?\UNC\fileserver\share").is_none());
    }

    #[test]
    fn fallback_windows_cwd_returns_existing_path() {
        // 兜底 cwd 必须是 portable-pty 能 is_dir() 通过的目录;
        // %USERPROFILE% 在所有用户环境下都存在,失败时退化为 C:\。
        let cwd = fallback_windows_cwd();
        assert!(!cwd.is_empty(), "fallback cwd 不应为空字符串");
    }

    // === WSLENV 字符串拼接(WSL 分支项目级 env 注入) ===
    // 这些单测覆盖 build_wslenv_value 纯函数的所有路径:
    // - 空列表 → None(避免用空 WSLENV 覆盖宿主既有值)
    // - 单条 / 多条变量 → "K1/u" / "K1/u:K2/u"(/u flag 与 JetBrains IDEA 对齐)
    // - 宿主既有 WSLENV → 追加在尾部合并(不覆盖)
    // - 宿主 WSLENV 为空 / Some("") → 视同 None,不追加
    // 外层过滤(MINITERM_* / WSLENV key)由 create_pty 调用方负责,本函数不重复。

    #[test]
    fn build_wslenv_empty_no_host_returns_none() {
        // 空 user_envs + 宿主无 WSLENV → None,调用方应跳过 cmd.env("WSLENV", ...)
        let result = build_wslenv_value(&[], None);
        assert_eq!(result, None);
    }

    #[test]
    fn build_wslenv_single_var() {
        let envs = vec![("FOO".to_string(), "bar".to_string())];
        let result = build_wslenv_value(&envs, None);
        assert_eq!(result, Some("FOO/u".to_string()));
    }

    #[test]
    fn build_wslenv_multiple_vars_preserves_insertion_order() {
        let envs = vec![
            ("FOO".to_string(), "1".to_string()),
            ("BAR".to_string(), "2".to_string()),
            ("BAZ".to_string(), "3".to_string()),
        ];
        let result = build_wslenv_value(&envs, None);
        assert_eq!(result, Some("FOO/u:BAR/u:BAZ/u".to_string()));
    }

    #[test]
    fn build_wslenv_merges_existing_host_wslenv_at_tail() {
        // 宿主已有 WSLENV=EXISTING_VAR/p → mini-term 拼出 K1/u:K2/u 后,
        // 在尾部追加 EXISTING_VAR/p,不覆盖。与 JetBrains/wslgit 对齐。
        let envs = vec![
            ("FOO".to_string(), "1".to_string()),
            ("BAR".to_string(), "2".to_string()),
        ];
        let result = build_wslenv_value(&envs, Some("EXISTING_VAR/p"));
        assert_eq!(result, Some("FOO/u:BAR/u:EXISTING_VAR/p".to_string()));
    }

    #[test]
    fn build_wslenv_empty_user_envs_but_host_has_wslenv() {
        // user_envs 空但宿主有 WSLENV → 仍返回 Some(宿主值),
        // 因为 create_pty 调用本函数前已经判定 wsl_override 命中,
        // 即使没有项目 env 也应保留宿主既有 WSLENV(理论上无需 mini-term 干预,
        // 但保持纯函数语义:输入有非空内容就有输出)。
        // 实际 create_pty 入口处已有 !user_envs.is_empty() 兜底,本测试只验证函数语义。
        let result = build_wslenv_value(&[], Some("HOST_VAR/u"));
        assert_eq!(result, Some("HOST_VAR/u".to_string()));
    }

    #[test]
    fn build_wslenv_empty_host_wslenv_string_treated_as_absent() {
        // 宿主 WSLENV="" (空字符串) 不应追加 → 避免产生 "FOO/u:" 这种尾部 : 残留。
        let envs = vec![("FOO".to_string(), "1".to_string())];
        let result = build_wslenv_value(&envs, Some(""));
        assert_eq!(result, Some("FOO/u".to_string()));
    }

    #[test]
    fn build_wslenv_host_with_multiple_existing_entries() {
        // 宿主 WSLENV 自身可含多个条目(冒号分隔),整段照搬在尾部。
        let envs = vec![("FOO".to_string(), "1".to_string())];
        let result = build_wslenv_value(&envs, Some("A/u:B/p:C"));
        assert_eq!(result, Some("FOO/u:A/u:B/p:C".to_string()));
    }
}
