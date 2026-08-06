use mt_core::{parse_wsl_unc, scan_ssh_prompt, strip_ansi_codes, SshPromptScan};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
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

/// 跨平台版兜底 cwd:SSH 远程启动器分支用。远程项目的 `path` 是远程 POSIX
/// 绝对路径,不能传给 portable-pty(见 portable-pty-conpty-cwd-fallback spec);
/// ssh 进程自己 `cd` 进远程目录,本地 cwd 只需是一个合法目录。
fn fallback_local_cwd() -> String {
    if cfg!(windows) {
        fallback_windows_cwd()
    } else {
        std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
    }
}

// ---------------------------------------------------------------------------
// SSH 远程项目启动器(task 07-05-ssh-remote-projects PR2)
// ---------------------------------------------------------------------------

/// `create_pty` 的可选远程启动参数。前端 invoke 时必须 wrap:
/// `{ ..., sshRemote: { connectionId, remotePath } }`
/// (struct 参数约定见 spec/backend/tauri-command-nested-args.md)。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshRemoteSpec {
    /// `config.json` `sshConnections` 里的连接 id。连接被删除时返回明确错误(断链态)。
    pub connection_id: String,
    /// 远程 POSIX 绝对路径,ssh 登录后 `cd` 进入。
    pub remote_path: String,
}

/// 远程启动器的最终形态:spawn 的程序、参数与(可选)用于 autofill 预注册的密码。
struct RemoteLaunch {
    program: String,
    args: Vec<String>,
    password: Option<String>,
}

/// POSIX shell 单引号安全包裹:`'` → `'\''`。
/// 远程路径来自用户输入,拼进 `cd <path>` 前必须做引号安全处理,
/// 防止 `;`、`$()`、空格等在远程 shell 里被解释。
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 拼 ssh 的远端命令:`cd '<path>' && exec $SHELL -l`。
/// `$SHELL` 保持字面量 —— 本地不经过 shell(portable-pty 直接 spawn ssh,
/// 参数按 argv 传递),它由远程 sshd 用登录 shell 执行时才展开,
/// 从而落在用户自己的默认 shell 上。
fn build_remote_login_command(remote_path: &str) -> String {
    format!("cd {} && exec $SHELL -l", shell_single_quote(remote_path))
}

/// 拼直接 spawn `ssh` 作 PTY 子进程的参数列表(不经本地 shell,
/// 对齐 WSL 根项目 spawn wsl.exe 的启动器重写模式)。
///
/// 形如:`-t [-p <port>] [-i <identity>] user@host "cd '<path>' && exec $SHELL -l"`。
/// **绝不能加 `-o BatchMode=yes`**:它会连带禁用密码认证,
/// 而密码连接依赖 PTY autofill 灌密码(见 spec/backend/index.md gotcha)。
fn build_ssh_launcher_args(
    host: &str,
    port: u16,
    user: &str,
    identity: Option<&str>,
    remote_path: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-t".to_string()];
    if port != 0 && port != 22 {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    if let Some(key) = identity {
        args.push("-i".to_string());
        args.push(key.to_string());
    }
    args.push(format!("{user}@{host}"));
    args.push(build_remote_login_command(remote_path));
    args
}

/// 在 PATH 里找可执行文件(本机 OpenSSH 客户端探测用)。
fn find_in_path(program: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// 定位本机 ssh 客户端。Windows 10+ 自带 OpenSSH 客户端(System32\OpenSSH),
/// 缺失时返回 None 由调用方给出明确安装提示。
fn find_ssh_client() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        find_in_path("ssh.exe")
    } else {
        find_in_path("ssh")
    }
}

/// 把 `SshRemoteSpec` 解析成可 spawn 的远程启动器:
/// 查连接(断链给明确错误)→ 探测 ssh 客户端 → 私钥复制权限收紧临时副本 → 拼参数。
fn prepare_ssh_remote_launch(
    app: &AppHandle,
    spec: &SshRemoteSpec,
) -> Result<RemoteLaunch, String> {
    let config = crate::config::read_config(app);
    let conn = config
        .ssh_connections
        .iter()
        .find(|c| c.id == spec.connection_id)
        .ok_or_else(|| {
            format!(
                "SSH 连接不存在或已被删除 (id={}),请检查项目的远程连接设置",
                spec.connection_id
            )
        })?;

    let ssh_program = find_ssh_client().ok_or_else(|| {
        "未找到 ssh 客户端(OpenSSH)。Windows 10+ 可在「设置 → 系统 → 可选功能」中安装 \
        「OpenSSH 客户端」后重试"
            .to_string()
    })?;

    // 私钥复制为权限收紧的临时副本(绕过 OpenSSH 的 UNPROTECTED PRIVATE KEY 拒绝),
    // 复用既有 prepare_ssh_key;失败(源文件不存在等)直接报错。
    let identity = match conn
        .identity_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(path) => Some(mt_core::prepare_ssh_key(path)?),
        None => None,
    };

    let args = build_ssh_launcher_args(
        &conn.host,
        conn.port,
        &conn.user,
        identity.as_deref(),
        &spec.remote_path,
    );

    Ok(RemoteLaunch {
        program: ssh_program.to_string_lossy().into_owned(),
        args,
        password: conn.password.clone().filter(|p| !p.is_empty()),
    })
}

/// 为 WSL 启动器分支拼装 `WSLENV` 环境变量的 value。
///
/// 输入 `user_envs` 已经被外层过滤过(剔除 `MINITERM_*` 前缀与用户输入的 `WSLENV` key);
/// 本函数只负责把剩余的 key 加上 `/u` flag 并用 `:` 连接,再把宿主已有的 `WSLENV`
/// (若存在且非空)追加在尾部合并 —— 不覆盖,与 JetBrains IDEA terminal / wslgit 对齐。
///
/// flag 选 `/u`(仅 Win→WSL 方向,不做路径翻译),避免把普通环境变量值当作路径转换。
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
    /// 上次已应用的 PTY 尺寸 (cols, rows),resize_pty 用它做同尺寸去重
    last_size: (u16, u16),
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

/// 交互式 AI CLI 的命令名。
///
/// `pi`（pi.dev，earendil-works/pi）只有两个字母，但匹配走 `ai_command_name` 的
/// basename **全等**，`pip install` / `ping` / `pi.py` 都不会命中；它的
/// `-p/--print`、`-h/--help`、`-v/--version` 与下面的非交互标志逐一对齐，
/// 退出用的 `/quit` 也已在 `AI_EXIT_COMMANDS` 里，无需为它开特例。
const AI_COMMANDS: &[&str] = &["claude", "codex", "opencode", "pi"];

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

/// PTY 创建时的初始尺寸。前端挂载后首次 fit 会立即上报真实尺寸;
/// openpty 与 last_size(resize_pty 同尺寸去重的首个基准)必须取自这两个常量,
/// 否则初始值失同步会让第一次 resize 被误去重吞掉。
const INITIAL_PTY_COLS: u16 = 80;
const INITIAL_PTY_ROWS: u16 = 24;

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

/// reader → flush 之间的有界通道容量(条,每条 ≤ READ_CHUNK 字节)。
///
/// 此前用的是无界 `mpsc::channel`:reader 以 ConPTY 的最快速度读,前端只要跟不上,
/// 这个队列就一路涨到内存耗尽。换成有界后队列满时 reader 阻塞在 `send`,不再从
/// ConPTY 读,背压传导到刷屏进程本身——这正是真实终端的行为(慢终端会拖慢
/// `cat` 大文件,而不是把数据全缓存下来)。
/// 512 × 4KB = 2MB 在途上限,足够吸收正常的突发,又不至于攒出可观的常驻内存。
const OUTPUT_CHANNEL_CAPACITY: usize = 512;

/// reader 单次读取的缓冲区大小
const READ_CHUNK: usize = 4096;

/// flush 缓冲区空闲时保留的容量上限(超出部分归还分配器)
const PENDING_KEEP_CAPACITY: usize = 64 * 1024;

/// 流控暂停期间 flush 线程的轮询间隔
const FLOW_PAUSE_POLL: Duration = Duration::from_millis(8);

/// 流控暂停的最长时限。前端崩溃/卡死后不会再发 resume,超时即强制恢复投递,
/// 否则用户的 shell 会永久卡在一次写上(表现为终端完全没反应)。
const MAX_FLOW_PAUSE: Duration = Duration::from_secs(30);

/// 命令词对应的 AI 命令名(basename 归一后精确匹配);非 AI 命令返回 None。
fn ai_command_name(word: &str) -> Option<&'static str> {
    let word = word.trim_matches(|c| matches!(c, '"' | '\'' | '`'));
    let basename = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let basename = [".exe", ".cmd", ".bat", ".ps1"]
        .iter()
        .find_map(|suffix| basename.strip_suffix(suffix))
        .unwrap_or(basename);
    let basename = basename.to_lowercase();
    AI_COMMANDS.iter().find(|&&ai| basename == ai).copied()
}

/// 该命令行会进入哪个交互式 AI 会话;不会进入返回 None。
pub fn interactive_ai_command_name(command: &str) -> Option<&'static str> {
    let mut words = command.split_whitespace();
    let mut first_word = words.next().unwrap_or("");
    if first_word == "&" {
        first_word = words.next().unwrap_or("");
    }
    let agent = ai_command_name(first_word)?;

    if words.any(|w| {
        let flag = w.to_lowercase();
        NON_INTERACTIVE_FLAGS.iter().any(|&f| flag == f)
    }) {
        None
    } else {
        Some(agent)
    }
}

/// 该命令行是否会被识别为"进入交互式 AI 会话"。
/// AI 启动器配置校验(mobile_relay)复用同一判定,避免两处口径漂移。
pub fn is_interactive_ai_command(command: &str) -> bool {
    interactive_ai_command_name(command).is_some()
}

fn line_ai_command_name(line: &str) -> Option<&'static str> {
    let line = strip_ansi_codes(line);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    if let Some(agent) = interactive_ai_command_name(line) {
        return Some(agent);
    }

    // xterm 行快照通常包含 shell prompt，例如 "PS D:\repo> claude"。
    // 对常见 prompt 分隔符取最后一段，避免把 prompt 内容当作命令解析。
    for marker in [">", "$ ", "# ", "% "] {
        if let Some(idx) = line.rfind(marker) {
            if let Some(agent) = interactive_ai_command_name(&line[idx + marker.len()..]) {
                return Some(agent);
            }
        }
    }

    None
}

/// 检查 PTY 输出中是否包含 AI 命令被 echo（例如 "PS C:\> claude" 或单独的 "claude"），
/// 命中返回对应的 AI 命令名
fn output_ai_command_name(output: &str) -> Option<&'static str> {
    strip_ansi_codes(output)
        .lines()
        .find_map(line_ai_command_name)
}

struct SshAutofillState {
    password: String,
    /// 累加的输出尾部,用于跨缓冲块匹配密码提示
    residual: String,
    /// 已填充或已禁用(命中错误密码)后置位,后续输出不再处理
    done: bool,
    /// 用户首次向 PTY 真实输入时是否解除本 autofill(见 disarm_ssh_autofill_on_user_input)。
    /// - 远程项目 pane(create_pty 直接 spawn ssh,arm 后无命令写入,首个 write_pty 即用户
    ///   输入)置 `true`:一旦用户打字即解除,避免 publickey 登录成功后 autofill 终身待命、
    ///   把 SSH 密码灌进后续 su / mysql -p / passwd 提示。
    /// - 右键「SSH 连接」路径(connectSsh:arm 后紧跟一条 `ssh ...\r` 命令写入)置 `false`:
    ///   否则那条命令写入会在密码提示到达前就把 autofill 删掉,破坏该功能;它仍靠命中
    ///   密码提示后置 `done` 自解除,保持既有行为。
    disarm_on_input: bool,
}

#[derive(Clone)]
pub struct PtyManager {
    instances: Arc<Mutex<HashMap<u32, PtyInstance>>>,
    next_id: Arc<Mutex<u32>>,
    last_output: Arc<Mutex<HashMap<u32, Instant>>>,
    /// pty → 会话内 AI 命令名("claude"/"codex"/"opencode";hook 扶正时取 hook 的 agent)。
    /// 有键即视为处于 AI 会话,值供前端品牌图标兜底(无 hook 时的唯一 agent 来源)。
    ai_sessions: Arc<Mutex<HashMap<u32, String>>>,
    /// pty → 本轮 AI 会话的启动时刻(enter_ai 时记录)。对话镜像用它过滤
    /// 早于本轮会话的旧记录文件,避免新会话未落盘时错绑上一次会话。
    ai_started: Arc<Mutex<HashMap<u32, SystemTime>>>,
    input_states: Arc<Mutex<HashMap<u32, InputState>>>,
    last_ctrlc: Arc<Mutex<HashMap<u32, Instant>>>,
    last_enter: Arc<Mutex<HashMap<u32, Instant>>>,
    pending_submits: Arc<Mutex<HashMap<u32, Vec<UserSubmit>>>>,
    /// resize 冷却窗口结束时间:在此之前 PTY 输出不刷新 last_output
    tui_redraw_cooldown_until: Arc<Mutex<HashMap<u32, Instant>>>,
    /// SSH 密码自动填充状态(arm_ssh_autofill 注册,命中密码提示后回写)
    ssh_autofill: Arc<Mutex<HashMap<u32, SshAutofillState>>>,
    /// 前端流控:被前端要求暂停投递的 pty 集合(见 `set_pty_flow_paused`)。
    /// 值是暂停开始时刻,超过 `MAX_FLOW_PAUSE` 强制恢复——前端崩溃/卡死时
    /// 不能让 shell 永久卡在写阻塞上。
    flow_paused: Arc<Mutex<HashMap<u32, Instant>>>,
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            instances: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
            last_output: Arc::new(Mutex::new(HashMap::new())),
            ai_sessions: Arc::new(Mutex::new(HashMap::new())),
            ai_started: Arc::new(Mutex::new(HashMap::new())),
            input_states: Arc::new(Mutex::new(HashMap::new())),
            last_ctrlc: Arc::new(Mutex::new(HashMap::new())),
            last_enter: Arc::new(Mutex::new(HashMap::new())),
            pending_submits: Arc::new(Mutex::new(HashMap::new())),
            tui_redraw_cooldown_until: Arc::new(Mutex::new(HashMap::new())),
            ssh_autofill: Arc::new(Mutex::new(HashMap::new())),
            flow_paused: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 清除某个 pty 的全部旁路状态(不含 `instances` 本身,也不碰 hook)。
    ///
    /// kill_pty 与「PTY 自然退出」两条路径必须清同一批表:此前自然退出分支只
    /// 清了 instances + ssh_autofill,余下 8 张表的条目会一直留到用户手动关
    /// pane 才被 kill_pty 收走——用户敲 `exit` 后把 pane 开着不动,这些残留就
    /// 永远不回收。抽成一处后新增字段不会再漏掉其中一条路径。
    pub fn purge_pty_state(&self, pty_id: u32) {
        self.last_output.lock().unwrap().remove(&pty_id);
        self.ai_sessions.lock().unwrap().remove(&pty_id);
        self.ai_started.lock().unwrap().remove(&pty_id);
        self.input_states.lock().unwrap().remove(&pty_id);
        self.last_ctrlc.lock().unwrap().remove(&pty_id);
        self.last_enter.lock().unwrap().remove(&pty_id);
        self.pending_submits.lock().unwrap().remove(&pty_id);
        self.ssh_autofill.lock().unwrap().remove(&pty_id);
        self.tui_redraw_cooldown_until.lock().unwrap().remove(&pty_id);
        self.flow_paused.lock().unwrap().remove(&pty_id);
    }

    /// 前端流控开关。暂停期间 flush 线程不再从 channel 取数据,channel 塞满后
    /// reader 线程阻塞在 send 上,背压一路传导回 ConPTY —— 刷屏的进程自己会被
    /// 写阻塞而减速,这正是真实终端的行为。
    pub fn set_flow_paused(&self, pty_id: u32, paused: bool) {
        let mut map = self.flow_paused.lock().unwrap();
        if paused {
            map.entry(pty_id).or_insert_with(Instant::now);
        } else {
            map.remove(&pty_id);
        }
    }

    /// 是否处于暂停投递状态。超过 `MAX_FLOW_PAUSE` 视为前端失联,强制恢复:
    /// 宁可前端被数据淹没,也不能让用户的 shell 永久卡死在一次写上。
    fn is_flow_paused(&self, pty_id: u32) -> bool {
        let mut map = self.flow_paused.lock().unwrap();
        match map.get(&pty_id) {
            Some(since) if since.elapsed() < MAX_FLOW_PAUSE => true,
            Some(_) => {
                map.remove(&pty_id);
                eprintln!("[pty] pty {} 流控暂停超时,强制恢复投递", pty_id);
                false
            }
            None => false,
        }
    }

    pub fn get_pty_ids(&self) -> Vec<u32> {
        self.instances.lock().unwrap().keys().copied().collect()
    }

    pub fn has_recent_output(&self, pty_id: u32, within: Duration) -> bool {
        let map = self.last_output.lock().unwrap();
        map.get(&pty_id).map_or(false, |t| t.elapsed() < within)
    }

    /// 测试用:伪造一次 PTY 输出时间戳。生产路径只由 reader 线程在
    /// flush 时写入(且受 TUI 重绘冷却窗口约束),单测里没法真起 PTY。
    #[cfg(test)]
    pub fn note_output_for_test(&self, pty_id: u32) {
        self.last_output
            .lock()
            .unwrap()
            .insert(pty_id, Instant::now());
    }

    pub fn is_ai_session(&self, pty_id: u32) -> bool {
        self.ai_sessions.lock().unwrap().contains_key(&pty_id)
    }

    /// 会话内 AI 命令名("claude"/"codex"/…);不在 AI 会话中返回 None。
    pub fn ai_session_agent(&self, pty_id: u32) -> Option<String> {
        self.ai_sessions.lock().unwrap().get(&pty_id).cloned()
    }

    /// 本轮 AI 会话的启动时刻;不在 AI 会话中返回 None。
    pub fn ai_session_started_at(&self, pty_id: u32) -> Option<SystemTime> {
        self.ai_started.lock().unwrap().get(&pty_id).copied()
    }

    /// hook 事件证明 AI 进程存活时把会话标记扶正：输入检测漏判启动
    /// （别名/包装脚本，命令行里没有 "claude" 字样）或误判退出（任务运行中
    /// 双击 Ctrl+C 只是打断并不退出）的自愈路径。已标记时幂等 no-op，
    /// 不重置 ai_started（对话镜像按它过滤旧记录，中途重置会错绑）。
    pub fn mark_ai_session(&self, pty_id: u32, agent: &str) {
        let mut sessions = self.ai_sessions.lock().unwrap();
        if !sessions.contains_key(&pty_id) {
            sessions.insert(pty_id, agent.to_string());
            self.ai_started
                .lock()
                .unwrap()
                .insert(pty_id, SystemTime::now());
        }
    }

    /// 清除 pane 的 AI 会话标记及相关输入痕迹。
    ///
    /// 输入检测到退出(双击 Ctrl+C / Ctrl+D / 显式退出命令)与 SessionEnd hook
    /// 的权威退出信号都走这里。顺带清 last_enter 关掉 Enter 后的输出扫描窗口:
    /// 否则退出瞬间 ConPTY 重绘把 scrollback 里的 "PS ..> claude" 再吐出来,
    /// 会被扫描误判成命令 echo 又把会话标回去。
    pub fn clear_ai_session(&self, pty_id: u32) {
        self.ai_sessions.lock().unwrap().remove(&pty_id);
        self.ai_started.lock().unwrap().remove(&pty_id);
        self.last_ctrlc.lock().unwrap().remove(&pty_id);
        self.last_enter.lock().unwrap().remove(&pty_id);
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
    ///
    /// `disarm_on_input`:用户首次真实输入时是否解除本 autofill —— 远程项目 pane 传
    /// `true`,右键「SSH 连接」路径传 `false`(见 SshAutofillState::disarm_on_input)。
    pub fn arm_ssh_autofill(&self, pty_id: u32, password: String, disarm_on_input: bool) {
        self.ssh_autofill.lock().unwrap().insert(
            pty_id,
            SshAutofillState {
                password,
                residual: String::new(),
                done: false,
                disarm_on_input,
            },
        );
    }

    /// 用户向 PTY 真实输入时调用:仅当该 autofill 标记了 `disarm_on_input` 才解除并清除
    /// 明文密码。
    ///
    /// 语义:SSH 认证阶段用户不打字(ssh 自驱动 publickey,失败才由 autofill 灌密码);
    /// 一旦用户按键即说明会话已进入交互 shell,此后 `su` / `mysql -p` / `passwd` 等以
    /// "password:" 结尾的提示都不该再被灌入 SSH 登录密码 —— 尤其 publickey 登录成功时
    /// 全程无密码提示、`done` 永不置位,不在此解除则 autofill 终身待命并泄露密码。
    /// 要触发这些提示用户必先敲入对应命令,那次敲击会先经此路径解除 autofill。
    /// 右键「SSH 连接」路径 arm 后紧跟一条 ssh 命令写入,故标 `false` 不在此解除。
    pub fn disarm_ssh_autofill_on_user_input(&self, pty_id: u32) {
        let mut map = self.ssh_autofill.lock().unwrap();
        if map.get(&pty_id).is_some_and(|st| st.disarm_on_input) {
            map.remove(&pty_id);
        }
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
        let mut enter_ai: Option<&'static str> = None;
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
                        let snapshot_agent = if allow_line_snapshot {
                            line_snapshot.and_then(line_ai_command_name)
                        } else {
                            None
                        };
                        // 记录 Enter 时间，供输出扫描用。空回车不打开扫描窗口，
                        // 避免 shell autosuggestion 出现在重绘输出中时被当成命令 echo。
                        if !trimmed.is_empty() || snapshot_agent.is_some() {
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
                            if let Some(agent) =
                                interactive_ai_command_name(trimmed).or(snapshot_agent)
                            {
                                enter_ai = Some(agent);
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
        if let Some(agent) = enter_ai {
            self.ai_sessions
                .lock()
                .unwrap()
                .insert(pty_id, agent.to_string());
            self.ai_started
                .lock()
                .unwrap()
                .insert(pty_id, SystemTime::now());
        } else if exit_ai {
            self.clear_ai_session(pty_id);
        }
    }
}

#[tauri::command]
// Tauri command 的参数即前端 invoke 的 payload key,拆 struct 会破坏既有前端调用;
// 新增的 ssh_remote 是第 8 个参数,allow 之。
#[allow(clippy::too_many_arguments)]
pub fn create_pty(
    app: AppHandle,
    state: tauri::State<'_, PtyManager>,
    hook_state: tauri::State<'_, crate::hook_server::HookState>,
    shell: String,
    args: Vec<String>,
    cwd: String,
    envs: Option<Vec<(String, String)>>,
    ssh_remote: Option<SshRemoteSpec>,
) -> Result<u32, String> {
    // SSH 远程项目分支:先于 openpty 解析,连接断链 / 缺 ssh 客户端时快速失败,
    // 不留半开的 PTY。
    let remote_launch = match &ssh_remote {
        Some(spec) => Some(prepare_ssh_remote_launch(&app, spec)?),
        None => None,
    };

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: INITIAL_PTY_ROWS,
            cols: INITIAL_PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    // WSL UNC cwd 检测:命中则忽略用户配置的 shell,改用 wsl.exe 启动,
    // 并把 portable-pty 的 cwd 换成 Windows 端合法目录(详见 decide_wsl_override 注释)。
    // 远程分支优先:远程项目的 cwd 是远程 POSIX 路径,不参与 WSL 判定。
    let wsl_override = if remote_launch.is_some() {
        None
    } else {
        decide_wsl_override(&cwd)
    };

    let (effective_shell, effective_args, effective_cwd) = if let Some(launch) = &remote_launch {
        // SSH 远程分支:直接 spawn ssh 作 PTY 子进程(不经本地 shell,对齐 WSL
        // 启动器重写模式)。本地 cwd 用兜底目录 —— 远程目录由 ssh 远端命令 cd 进入。
        // AI 状态感知走 PTY 输入/输出扫描降级路径(track_input /
        // output_contains_ai_command 作用于数据流,对远程天然可用);
        // hook 精确状态不可用,PRD 已接受。
        (
            launch.program.clone(),
            launch.args.clone(),
            fallback_local_cwd(),
        )
    } else {
        match &wsl_override {
            // WSL 分支:启动的是宿主 wsl.exe,WSL VM 内的 claude/codex 子进程
            // process_monitor 看不到,因此本路径下 AI 进程识别(ai-working/ai-idle 状态)
            // 会失效,目前不在此路径内处理。
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
        }
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

    // SSH 远程分支:密码自动填充在 **spawn 之前**注册(复用 arm_ssh_autofill 的
    // 内部状态)。直接 spawn ssh 模式下,密码提示可能先于前端任何后续调用到达,
    // 事后再 arm 存在竞态 —— 这里预注册彻底消除该窗口。
    if let Some(launch) = &remote_launch {
        if let Some(password) = &launch.password {
            // 远程项目 pane:arm 后不再写入任何命令,首个 write_pty 即用户交互
            // → disarm_on_input=true,用户一打字即解除,避免密码灌进后续 su/mysql 提示。
            state.arm_ssh_autofill(pty_id, password.clone(), true);
        }
    }

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
    // SSH 远程分支跳过用户 env 注入:注给本地 ssh 进程的变量不会传到远程 shell,
    // 注了也只是污染 ssh 进程环境(PRD R6:远程项目隐藏 envVars 入口,二期考虑
    // 注入远程 shell)。与 WSL 分支「宿主 env 不自动透传」同理,但远程连 WSLENV
    // 通道都没有,直接整组跳过。
    let user_envs: Vec<(String, String)> = if remote_launch.is_some() {
        Vec::new()
    } else {
        envs.unwrap_or_default()
            .into_iter()
            .filter(|(k, _)| !k.starts_with("MINITERM_") && k != "WSLENV")
            .collect()
    };

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

    // 有界 channel + flush 定时器实现 16ms 批量缓冲。
    // 有界是关键:队列满时下面的 `send` 阻塞,reader 停止从 ConPTY 读,
    // 背压直达刷屏进程(见 OUTPUT_CHANNEL_CAPACITY)。
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(OUTPUT_CHANNEL_CAPACITY);
    let instances_clone = state.instances.clone();
    let pty_id_for_reader = pty_id;

    thread::spawn(move || {
        let mut buf = [0u8; READ_CHUNK];
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
            // 前端缓冲高于水位时不取新数据也不 emit:channel 随即塞满,
            // reader 阻塞,ConPTY 背压生效。`pending` 原样留到恢复后再发。
            while mgr_flush.is_flow_paused(pty_id_for_reader) {
                thread::sleep(FLOW_PAUSE_POLL);
            }

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

                    // PTY 自然退出(前端未必调 kill_pty):走与 kill_pty 相同的全量
                    // 清理,不再只清 ssh_autofill —— 用户敲 exit 后把 pane 开着不动
                    // 时,余下 8 张表的条目此前会一直留到手动关 pane 才被回收。
                    mgr_flush.purge_pty_state(pty_id_for_reader);

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
                            if !sessions.contains_key(&pty_id_for_reader) {
                                if let Some(agent) = output_ai_command_name(&data) {
                                    sessions.insert(pty_id_for_reader, agent.to_string());
                                }
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

                // clear() 不还容量:一次刷屏撑出来的缓冲区会按峰值大小常驻到
                // PTY 结束。空闲后收回,只留一次批量的余量。
                if pending.capacity() > PENDING_KEEP_CAPACITY {
                    pending.shrink_to(PENDING_KEEP_CAPACITY);
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
                last_size: (INITIAL_PTY_COLS, INITIAL_PTY_ROWS),
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

/// 这一次写入是否为「打断当前 AI 任务」的按键。
///
/// 只认单独一个字节的裸 Esc / Ctrl+C：xterm.js 把方向键、功能键等 CSI 序列
/// （`\x1b[A` …）一次性交给 onData，粘贴同理，长度一律大于 1，因此等值比较
/// 足以把它们排除掉，不需要解析转义状态机。
///
/// 单次 Ctrl+C 在 AI 里是「取消当前任务」（连按两次才退出，见
/// `track_input_with_line_snapshot`），Esc 同理，两者都不产生 hook 事件。
fn is_interrupt_key(data: &str) -> bool {
    data == "\x1b" || data == "\x03"
}

#[tauri::command]
pub fn write_pty(
    app: tauri::AppHandle,
    state: tauri::State<'_, PtyManager>,
    hook_state: tauri::State<'_, crate::hook_server::HookState>,
    emitter: tauri::State<'_, crate::process_monitor::StatusEmitter>,
    pty_id: u32,
    data: String,
    line_snapshot: Option<String>,
) -> Result<(), String> {
    // 用户真实输入 → 会话已进入交互 shell,解除标了 disarm_on_input 的 SSH 密码自动
    // 填充,避免后续 su / mysql -p / passwd 等 "password:" 提示被灌入 SSH 登录密码
    // (见 disarm_ssh_autofill_on_user_input)。排除 xterm 焦点进/出 CSI 序列:TUI 开启
    // DEC 1004 后 xterm 会把 focus/blur 也经 onData 发来,那不是用户按键,不能据此解除
    // (否则认证期若碰上焦点事件会提前解除、密码提示灌不进)。autofill 自身回写密码走
    // instance.writer 直写、不经本命令,故此处也不会误伤认证阶段的填充。
    if data != FOCUS_IN_SEQ && data != FOCUS_OUT_SEQ {
        state.disarm_ssh_autofill_on_user_input(pty_id);
    }

    // 在写入前打开焦点冷却窗口:AI 对焦点事件的重绘响应几乎立即抵达 reader,
    // 必须早于那之前把冷却建立起来。
    state.note_focus_event(pty_id, &data);
    {
        let mut instances = state.instances.lock().unwrap();
        let instance = instances.get_mut(&pty_id).ok_or("PTY not found")?;
        write_pty_chunked(&mut *instance.writer, &data)?;
    }
    state.track_input_with_line_snapshot(pty_id, &data, line_snapshot.as_deref());

    // 用户打断 AI：Claude 在中断时不发任何 hook 事件（官方文档：`Stop` hooks
    // "don't fire on user interrupts"），状态会一直停在 ai-working。这里补一刀
    // 让它收敛到 ai-idle —— 判定与副作用都在 note_user_interrupt 里，含「只动
    // hook 已启用且正在 ai-working 的 pane」与「cause=Interrupt 不算完成」两道闸。
    // 放在 track_input 之后：双击 Ctrl+C 真退出的场景，紧随其后的 SessionEnd
    // 会把状态进一步落到 idle，这一刀只是让中间那段不至于显示成「工作中」。
    if is_interrupt_key(&data) {
        crate::hook_server::note_user_interrupt(
            &app,
            &hook_state,
            &emitter,
            pty_id,
            state.ai_session_agent(pty_id),
        );
    }

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
        let mut instances = state.instances.lock().unwrap();
        let instance = instances.get_mut(&pty_id).ok_or("PTY not found")?;
        // 同尺寸去重:前端挂载/切 tab 路径会重复上报未变的尺寸,而 ConPTY 收到
        // resize(即使同尺寸)会让 TUI 应用整屏重绘 —— Ink(Claude Code)的帧高于
        // 视口时,每次重绘都往 scrollback 漏一份残留(见 terminalCache.ts 的
        // alt-screen 注释)。尺寸没变就不透传,也不开冷却窗口。
        if instance.last_size == (cols, rows) {
            return Ok(());
        }
        instance
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        instance.last_size = (cols, rows);
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
    kill_pty_inner(&state, &hook_state, pty_id);
    Ok(())
}

/// 杀掉全部存量 PTY,返回被回收的 id 列表。
///
/// 前端在页面加载时(load_config 之前)调用。WebView2 的 renderer 进程被 OOM
/// 杀掉后会重载页面,而 `PtyManager` 活在主进程里毫发无损:重载后的前端按
/// `savedLayout` 恢复布局时给每个 pane 发的是**新** pane id、并新建 PTY,
/// 旧 PTY(shell 进程 + AI 进程 + 每个 PTY 两条线程)就此再无人引用却继续运行,
/// 输出打到前端因 cache miss 被丢弃,process_monitor 还在轮询这些幽灵。
/// 于是崩一次内存压力更大、下次崩得更快,形成正反馈。这里在恢复布局前先把
/// 存量清空,把那条正反馈掐断。
///
/// 正常首次启动时存量为空,是个 no-op。
#[tauri::command]
pub fn kill_all_ptys(
    state: tauri::State<'_, PtyManager>,
    hook_state: tauri::State<'_, crate::hook_server::HookState>,
) -> Result<Vec<u32>, String> {
    let ids = state.get_pty_ids();
    for id in &ids {
        kill_pty_inner(&state, &hook_state, *id);
    }
    if !ids.is_empty() {
        eprintln!("[pty] 页面加载:回收 {} 个孤儿 PTY {:?}", ids.len(), ids);
    }
    Ok(ids)
}

/// 前端流控开关。前端积压的待解析数据超过高水位时暂停,回落到低水位时恢复。
#[tauri::command]
pub fn set_pty_flow_paused(
    state: tauri::State<'_, PtyManager>,
    pty_id: u32,
    paused: bool,
) -> Result<(), String> {
    state.set_flow_paused(pty_id, paused);
    Ok(())
}

fn kill_pty_inner(
    state: &PtyManager,
    hook_state: &crate::hook_server::HookState,
    pty_id: u32,
) {
    // Remove metadata maps immediately so subsequent lookups return nothing.
    let instance = state.instances.lock().unwrap().remove(&pty_id);
    state.purge_pty_state(pty_id);
    // 清理 hook 状态(含已结束会话墓碑)
    hook_state.purge(pty_id);

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
}

/// 为某个 pty 注册 SSH 密码自动填充。前端在写入 `ssh` 命令前调用(连接配有密码时)。
///
/// 该路径 arm 后紧跟一条 ssh 命令写入 PTY,故 `disarm_on_input=false` —— 不能因那条命令
/// 写入就解除,否则密码提示到达前 autofill 已被删。它仍靠命中密码提示后置 `done` 自解除。
#[tauri::command]
pub fn arm_ssh_autofill(
    state: tauri::State<'_, PtyManager>,
    pty_id: u32,
    password: String,
) -> Result<(), String> {
    state.arm_ssh_autofill(pty_id, password, false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 打断键识别:只认单独一个字节的裸 Esc / Ctrl+C。方向键等 CSI 序列由
    /// xterm.js 一次性发来(`\x1b[A`),不能因为首字节是 Esc 就当成打断——否则
    /// 用户翻个历史记录就把「工作中」徽章打灭了。
    #[test]
    fn interrupt_key_only_matches_bare_esc_and_ctrl_c() {
        assert!(is_interrupt_key("\x1b"));
        assert!(is_interrupt_key("\x03"));

        for data in [
            "\x1b[A",    // ↑
            "\x1b[B",    // ↓
            "\x1b[1;5C", // Ctrl+→
            "\x1bOP",    // F1
            "\x1b[I",    // 焦点进入
            "\x03\x03",  // 一次写入里的两个 Ctrl+C
            "\x1b\x1b",
            "",
            "esc",
        ] {
            assert!(!is_interrupt_key(data), "误判为打断键: {:?}", data);
        }
    }

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
    fn clear_ai_session_resets_state() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        assert!(mgr.ai_session_started_at(1).is_some());
        // SessionEnd 权威退出信号走这里:双击 Ctrl+C 漏检时自愈
        mgr.clear_ai_session(1);
        assert!(!mgr.is_ai_session(1));
        assert!(mgr.ai_session_started_at(1).is_none());
    }

    #[test]
    fn mark_ai_session_rearms_after_false_exit() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        // 任务运行中双击 Ctrl+C 打断:输入检测误判为退出
        mgr.track_input(1, "\x03");
        mgr.track_input(1, "\x03");
        assert!(!mgr.is_ai_session(1));
        // 后续 hook 事件证明 AI 还活着 → 扶正
        mgr.mark_ai_session(1, "claude");
        assert!(mgr.is_ai_session(1));
        assert!(mgr.ai_session_started_at(1).is_some());
    }

    #[test]
    fn mark_ai_session_idempotent_keeps_started_at() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        let started = mgr.ai_session_started_at(1).expect("进入会话应记录启动时刻");
        // 会话已标记时 mark 为 no-op,不得重置 ai_started(镜像按它过滤旧记录)
        mgr.mark_ai_session(1, "claude");
        assert_eq!(mgr.ai_session_started_at(1), Some(started));
    }

    #[test]
    fn exit_clears_enter_scan_window() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");
        // 会话内提交 prompt 会记录 last_enter(打开输出扫描窗口)
        mgr.track_input(1, "fix the bug\r");
        assert!(mgr.last_enter.lock().unwrap().contains_key(&1));
        // 双击 Ctrl+C 退出后窗口必须关闭,防止退出重绘把会话标回去
        mgr.track_input(1, "\x03");
        mgr.track_input(1, "\x03");
        assert!(!mgr.is_ai_session(1));
        assert!(!mgr.last_enter.lock().unwrap().contains_key(&1));
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
    fn detect_pi_command() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "pi\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn pi_with_interactive_args() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "pi --model claude-sonnet-5\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn pi_non_interactive_flags_not_ai_session() {
        for cmd in ["pi -p \"hello\"\r", "pi --print x\r", "pi -v\r", "pi --help\r"] {
            let mgr = PtyManager::new();
            mgr.track_input(1, cmd);
            assert!(!mgr.is_ai_session(1), "{cmd} 不应进入 AI 会话");
        }
    }

    /// `pi` 只有两个字母,匹配必须是 basename 全等:任何以 pi 开头的常见命令
    /// (pip / ping / pixi)或同名脚本(pi.py)都不能把 pane 标成 AI 会话。
    #[test]
    fn pi_prefixed_commands_not_ai_session() {
        for cmd in [
            "pip install requests\r",
            "ping example.com\r",
            "pixi run build\r",
            "pi.py\r",
            "python pi.py\r",
        ] {
            let mgr = PtyManager::new();
            mgr.track_input(1, cmd);
            assert!(!mgr.is_ai_session(1), "{cmd} 不应被认成 pi");
        }
    }

    /// ↑ 召回历史里的 `pi`:输入缓冲是空的,只能靠行快照判定。
    #[test]
    fn pi_from_line_snapshot() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "\x1b[A");
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> pi"));
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn pip_from_line_snapshot_not_ai_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "\x1b[A");
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> pip install x"));
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn slash_quit_exits_pi_session() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "pi\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "/quit\r");
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
    fn ai_session_started_at_follows_session_lifecycle() {
        let mgr = PtyManager::new();
        // 未进入 AI 会话:无启动时刻
        assert!(mgr.ai_session_started_at(1).is_none());

        let before = SystemTime::now();
        mgr.track_input(1, "claude\r");
        let started = mgr.ai_session_started_at(1).expect("进入会话应记录启动时刻");
        assert!(started >= before && started <= SystemTime::now());

        // Ctrl+D 退出:清除启动时刻(镜像不应再拿旧锚点)
        mgr.track_input(1, "\x04");
        assert!(!mgr.is_ai_session(1));
        assert!(mgr.ai_session_started_at(1).is_none());

        // 同 pane 再次进入:锚点刷新为新一轮的启动时刻
        mgr.track_input(1, "claude\r");
        let restarted = mgr.ai_session_started_at(1).expect("重启会话应重新记录");
        assert!(restarted >= started);
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
        mgr.arm_ssh_autofill(7, "secret".into(), true);
        assert!(mgr.ssh_autofill.lock().unwrap().contains_key(&7));
    }

    #[test]
    fn disarm_on_user_input_clears_when_flagged() {
        // 远程项目 pane(disarm_on_input=true):用户输入后解除,后续 su / mysql -p 等
        // 密码提示不得再被灌入 SSH 登录密码。
        let mgr = PtyManager::new();
        mgr.arm_ssh_autofill(7, "secret".into(), true);
        mgr.disarm_ssh_autofill_on_user_input(7);
        assert!(!mgr.ssh_autofill.lock().unwrap().contains_key(&7));
    }

    #[test]
    fn disarm_on_user_input_keeps_when_not_flagged() {
        // 右键「SSH 连接」路径(disarm_on_input=false):arm 后紧跟的 ssh 命令写入
        // 不得解除 autofill,否则密码提示到达前已被删。
        let mgr = PtyManager::new();
        mgr.arm_ssh_autofill(9, "secret".into(), false);
        mgr.disarm_ssh_autofill_on_user_input(9);
        assert!(
            mgr.ssh_autofill.lock().unwrap().contains_key(&9),
            "未标 disarm_on_input 的 autofill 不应被用户输入解除"
        );
    }

    #[test]
    fn disarm_on_user_input_absent_pty_is_noop() {
        // 非远程 pty(从未 arm)上调用应安全无操作,不 panic
        let mgr = PtyManager::new();
        mgr.disarm_ssh_autofill_on_user_input(42);
        assert!(mgr.ssh_autofill.lock().unwrap().is_empty());
    }

    #[test]
    fn purge_removes_ssh_autofill_regardless_of_flag() {
        // 无条件清理(kill / 自然退出):不看 disarm_on_input 都要清掉明文密码。
        let mgr = PtyManager::new();
        mgr.arm_ssh_autofill(9, "secret".into(), false);
        mgr.purge_pty_state(9);
        assert!(!mgr.ssh_autofill.lock().unwrap().contains_key(&9));
    }

    /// 回归测试(PTY 自然退出漏清旁路状态):此前自然退出分支只清 ssh_autofill,
    /// 用户敲 `exit` 后把 pane 开着不动,余下 8 张表的条目会一直残留到手动关
    /// pane。purge_pty_state 必须把每一张都清空——漏掉任何一张,这条断言就红。
    #[test]
    fn purge_clears_every_side_table() {
        let mgr = PtyManager::new();
        let id = 3;

        mgr.track_input(id, "claude\r"); // ai_sessions / ai_started / last_enter / input_states
        mgr.track_input(id, "abc"); // input_states 里留下半行
        mgr.track_input(id, "\x03"); // last_ctrlc
        mgr.last_output.lock().unwrap().insert(id, Instant::now());
        mgr.bump_cooldown(id, RESIZE_COOLDOWN);
        mgr.arm_ssh_autofill(id, "secret".into(), false);
        mgr.set_flow_paused(id, true);
        mgr.pending_submits
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .push(UserSubmit {
                line: "hi".into(),
                ts: 0,
            });

        mgr.purge_pty_state(id);

        assert!(mgr.last_output.lock().unwrap().is_empty(), "last_output 未清");
        assert!(mgr.ai_sessions.lock().unwrap().is_empty(), "ai_sessions 未清");
        assert!(mgr.ai_started.lock().unwrap().is_empty(), "ai_started 未清");
        assert!(mgr.input_states.lock().unwrap().is_empty(), "input_states 未清");
        assert!(mgr.last_ctrlc.lock().unwrap().is_empty(), "last_ctrlc 未清");
        assert!(mgr.last_enter.lock().unwrap().is_empty(), "last_enter 未清");
        assert!(
            mgr.pending_submits.lock().unwrap().is_empty(),
            "pending_submits 未清"
        );
        assert!(mgr.ssh_autofill.lock().unwrap().is_empty(), "ssh_autofill 未清");
        assert!(
            mgr.tui_redraw_cooldown_until.lock().unwrap().is_empty(),
            "tui_redraw_cooldown_until 未清"
        );
        assert!(mgr.flow_paused.lock().unwrap().is_empty(), "flow_paused 未清");
    }

    /// 流控暂停超时后强制恢复:前端崩溃/卡死不再发 resume 时,shell 不能被
    /// 永久卡在写阻塞上(表现为终端完全没反应)。
    #[test]
    fn flow_pause_expires_after_max_duration() {
        let mgr = PtyManager::new();
        mgr.set_flow_paused(1, true);
        assert!(mgr.is_flow_paused(1), "刚暂停应处于暂停态");

        // 把暂停起点回拨到超时之外,模拟前端失联
        mgr.flow_paused
            .lock()
            .unwrap()
            .insert(1, Instant::now() - MAX_FLOW_PAUSE - Duration::from_secs(1));

        assert!(!mgr.is_flow_paused(1), "超时后应强制恢复投递");
        assert!(
            mgr.flow_paused.lock().unwrap().is_empty(),
            "强制恢复时应顺手清掉记录,不留下每次都要重判的死条目"
        );
    }

    #[test]
    fn flow_pause_is_per_pty() {
        let mgr = PtyManager::new();
        mgr.set_flow_paused(1, true);
        assert!(mgr.is_flow_paused(1));
        assert!(!mgr.is_flow_paused(2), "流控不得波及其他 pane");
        mgr.set_flow_paused(1, false);
        assert!(!mgr.is_flow_paused(1));
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

    // === SSH 远程启动器(task 07-05 PR2) ===
    // 与 decide_wsl_override 同理:完整 create_pty 会 spawn 真实进程,不适合单测;
    // 这里覆盖「拼参数 / 引号安全」的纯函数层。

    #[test]
    fn shell_single_quote_wraps_plain_path() {
        assert_eq!(shell_single_quote("/home/u/proj"), "'/home/u/proj'");
    }

    #[test]
    fn shell_single_quote_escapes_embedded_single_quotes() {
        // it's → 'it'\''s':单引号闭合 + 转义字面量 + 重新开引号
        assert_eq!(shell_single_quote("/a/it's"), r"'/a/it'\''s'");
    }

    #[test]
    fn shell_single_quote_neutralizes_shell_metacharacters() {
        // `;`、`$()`、空格等在单引号内均为字面量,不会被远程 shell 解释
        let quoted = shell_single_quote("/tmp/x; rm -rf $HOME `id`");
        assert_eq!(quoted, "'/tmp/x; rm -rf $HOME `id`'");
    }

    #[test]
    fn build_remote_login_command_quotes_path_and_keeps_shell_literal() {
        let cmd = build_remote_login_command("/home/u/my proj");
        assert_eq!(cmd, "cd '/home/u/my proj' && exec $SHELL -l");
        // $SHELL 必须保持字面量,由远程登录 shell 展开
        assert!(cmd.contains("$SHELL"));
    }

    #[test]
    fn build_ssh_launcher_args_default_port_no_identity() {
        let args = build_ssh_launcher_args("h.example.com", 22, "root", None, "/srv/app");
        assert_eq!(
            args,
            vec![
                "-t".to_string(),
                "root@h.example.com".to_string(),
                "cd '/srv/app' && exec $SHELL -l".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_launcher_args_port_zero_treated_as_default() {
        let args = build_ssh_launcher_args("h", 0, "u", None, "/p");
        assert!(!args.contains(&"-p".to_string()));
    }

    #[test]
    fn build_ssh_launcher_args_custom_port_and_identity() {
        let args = build_ssh_launcher_args(
            "10.0.0.5",
            2222,
            "deploy",
            Some(r"C:\Temp\mini-term-ssh-keys\abc.key"),
            "/home/deploy",
        );
        assert_eq!(
            args,
            vec![
                "-t".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "-i".to_string(),
                r"C:\Temp\mini-term-ssh-keys\abc.key".to_string(),
                "deploy@10.0.0.5".to_string(),
                "cd '/home/deploy' && exec $SHELL -l".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_launcher_args_never_uses_batchmode() {
        // BatchMode=yes 会连带禁用密码认证,破坏 PTY autofill 灌密码链路
        // (见 spec/backend/index.md gotcha)。任何组合下都不允许出现。
        for (port, identity) in [(22u16, None), (2222, Some("/k")), (0, None)] {
            let args = build_ssh_launcher_args("h", port, "u", identity, "/p");
            assert!(
                !args.iter().any(|a| a.contains("BatchMode")),
                "args 不得包含 BatchMode: {args:?}"
            );
        }
    }

    #[test]
    fn build_ssh_launcher_args_hostile_remote_path_is_contained() {
        // 恶意路径整体落在单引号内,`;` 与 `$()` 不会成为独立命令
        let args = build_ssh_launcher_args("h", 22, "u", None, "/tmp'; rm -rf /; echo '");
        let remote_cmd = args.last().unwrap();
        assert_eq!(
            remote_cmd,
            r"cd '/tmp'\''; rm -rf /; echo '\''' && exec $SHELL -l"
        );
    }

    #[test]
    fn fallback_local_cwd_returns_nonempty() {
        assert!(!fallback_local_cwd().is_empty());
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
