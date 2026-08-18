//! AI 命令识别与打断识别(纯函数层)。
//!
//! 原本长在 `src-tauri/src/pty.rs` 里。迁移后 PTY 层不再知道 AI 的存在:
//! 上层把写入/读出的字节各旁路一份给 [`crate::AiPerception`],识别逻辑全在这。

/// 去除 ANSI 转义序列，返回纯文本。
///
/// 逐字复制自 `mt_core::strip_ansi_codes`(见 `crate::util` 顶部关于为什么
/// 不直接依赖 `src-tauri/mt-core` 的说明)。
pub(crate) fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some(&'[') => {
                    chars.next(); // consume '['
                                  // CSI sequence: skip until final byte (0x40–0x7E)
                    for c2 in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c2) {
                            break;
                        }
                    }
                }
                Some(&'O') => {
                    chars.next();
                    chars.next();
                } // SS3: ESC O <final>
                _ => {
                    chars.next();
                } // other two-char escape
            }
        } else {
            result.push(c);
        }
    }
    result
}

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
pub const AI_COMMANDS: &[&str] = &["claude", "codex", "opencode", "pi", "grok"];

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
