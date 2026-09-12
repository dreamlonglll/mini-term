//! AI 命令识别与打断识别(纯函数层)。
//!
//! 原本长在 `src-tauri/src/pty.rs` 里。迁移后 PTY 层不再知道 AI 的存在:
//! 上层把写入/读出的字节各旁路一份给 [`crate::AiPerception`],识别逻辑全在这。

/// 去除 ANSI 转义序列，返回纯文本。
///
/// 收尾-1 批之前这里是 `mt_core::strip_ansi_codes` 的逐字副本(那时 mt-core 还在
/// `src-tauri/` 下,新工作区不能反向依赖旧目录树)。mt-core 移入 `crates/` 后
/// 副本已删,改为再导出 —— 本模块内的两处调用与下面的回归测试都一字未动。
pub(crate) use mt_core::strip_ansi_codes;

/// 交互式 AI CLI 的命令名。
///
/// `pi`（pi.dev，earendil-works/pi）只有两个字母，但匹配走 `ai_command_name` 的
/// basename **全等**，`pip install` / `ping` / `pi.py` 都不会命中；它的
/// `-p/--print`、`-h/--help`、`-v/--version` 与下面的非交互标志逐一对齐，
/// 退出用的 `/quit` 也已在 `AI_EXIT_COMMANDS` 里，无需为它开特例。
///
/// `grok`（xai-org/grok-build）的官方安装把二进制铺成 `grok`（artifact 名是
/// `xai-grok-pager`），非交互用 `-p`、`--version`/`--help` 也与下面对齐；
/// `--resume` / `--trust` 都是交互式启动，不该进非交互列表。
///
/// `omp`（oh-my-pi，can1357/oh-my-pi，pi 的分支）的命令名就是 `omp`，Windows 上
/// 是 `omp.cmd`（basename 归一时剥掉）；一次性提问同样是 `-p`，`--resume` /
/// `--continue` / `--fork` 都进交互式 TUI。它的 hook 接入见 `hook_registry` 的 omp 段。
pub const AI_COMMANDS: &[&str] = &["claude", "codex", "opencode", "pi", "grok", "omp"];

/// 这些标志表示非交互命令（仅输出信息后退出），不应触发 AI 会话状态
const NON_INTERACTIVE_FLAGS: &[&str] = &["-v", "--version", "-h", "--help", "-p", "--print"];

/// AI 会话中的显式退出命令
pub(crate) const AI_EXIT_COMMANDS: &[&str] = &[
    "/exit", "exit", // Claude Code & Codex 通用
    "/quit", "quit",    // Claude Code & Codex 通用
    ":quit",   // Codex 交互式退出
    "/logout", // Codex 退出
];

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
/// AI 启动器配置校验(移动端中转)复用同一判定,避免两处口径漂移。
pub fn is_interactive_ai_command(command: &str) -> bool {
    interactive_ai_command_name(command).is_some()
}

pub(crate) fn line_ai_command_name(line: &str) -> Option<&'static str> {
    let line = strip_ansi_codes(line);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    if let Some(agent) = interactive_ai_command_name(line) {
        return Some(agent);
    }

    // 终端行快照通常包含 shell prompt，例如 "PS D:\repo> claude"。
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
pub(crate) fn output_ai_command_name(output: &str) -> Option<&'static str> {
    strip_ansi_codes(output)
        .lines()
        .find_map(line_ai_command_name)
}

/// 从可见行快照里剥出「用户即将发出去的那句」。
///
/// # 为什么需要它:内容根本没经过终端
///
/// TUI 自己往输入框里**回填**内容时 —— Esc 撤回上一条重发、↑ 召回历史、
/// 斜杠命令菜单选中 —— 终端这边只收到一个**裸 Enter**,本地输入缓冲是空的,
/// 内容全程在 agent 进程自己手里。屏幕上那一行是唯一的线索。
///
/// 剥装饰走 [`mt_core::strip_tui_decoration`],与 `mt_app::markers` 回扫时**同一份**
/// 字符集:剥法差一点,拿它去屏幕上比对就永远对不上。
///
/// ⚠️ **交回来的东西是猜的,绝不能当真**:权限审批框里按 Enter 选 `1. Yes`、
/// 在 `/model` 菜单里按 Enter 选一项,这里照样会剥出一串文本来。调用方必须拿它
/// 去屏幕上验明正身(见 `mt_app::markers::AiMarker::confirmed`),验不过就当没有过
/// —— 直接采信的话,每批准一次权限就多一条假标记,列表会被淹掉。
pub(crate) fn snapshot_submit_text(line: &str) -> Option<String> {
    let text = strip_ansi_codes(line);
    let body = mt_core::strip_tui_decoration(&text);
    (!body.is_empty()).then(|| body.to_string())
}

/// 这一次写入是否为「打断当前 AI 任务」的按键。
///
/// 只认单独一个字节的裸 Esc / Ctrl+C：终端把方向键、功能键等 CSI 序列
/// （`\x1b[A` …）一次性交给输入回调，粘贴同理，长度一律大于 1，因此等值比较
/// 足以把它们排除掉，不需要解析转义状态机。
///
/// 单次 Ctrl+C 在 AI 里是「取消当前任务」（连按两次才退出，见
/// `SessionTracker::track_input_with_line_snapshot`），Esc 同理，两者都不产生
/// hook 事件。
pub(crate) fn is_interrupt_key(data: &str) -> bool {
    data == "\x1b" || data == "\x03"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// omp 的识别口径:裸命令 / Windows 的 `omp.cmd` / 带交互式参数都算进会话,
    /// 一次性提问与版本查询不算;basename 全等,`compose` / `ompx` 都不会误伤。
    #[test]
    fn omp_is_recognized_only_when_interactive() {
        for cmd in [
            "omp",
            "omp.cmd",
            "omp --resume 1f9d2a6b",
            "omp --continue",
            "omp --fork abc",
        ] {
            assert_eq!(
                interactive_ai_command_name(cmd),
                Some("omp"),
                "{cmd} 应识别为 omp"
            );
        }
        for cmd in [
            "omp -p 'fix it'",
            "omp --version",
            "omp -h",
            "docker compose up",
            "ompx",
        ] {
            assert_eq!(
                interactive_ai_command_name(cmd),
                None,
                "{cmd} 不该进入 AI 会话"
            );
        }
        // 行快照里带 prompt 也认得出
        assert_eq!(line_ai_command_name("PS D:\\repo> omp"), Some("omp"));
    }

    /// 打断键识别:只认单独一个字节的裸 Esc / Ctrl+C。方向键等 CSI 序列由
    /// 终端一次性发来(`\x1b[A`),不能因为首字节是 Esc 就当成打断——否则
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
    fn strip_ansi_codes_removes_csi_sequences() {
        assert_eq!(strip_ansi_codes("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi_codes("hello world"), "hello world");
    }
}
