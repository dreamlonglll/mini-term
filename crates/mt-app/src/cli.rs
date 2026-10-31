//! 不起窗口的命令行模式。目前只有一个:`--unregister-hooks`,给 NSIS 卸载器用。
//!
//! # `--unregister-hooks`
//!
//! 卸载 mini-term 时把五家 AI 工具(Claude / Codex / Grok / oh-my-pi / pi)配置里 mini-term
//! 写入的 hook 注册摘掉([`mt_ai::hook_registry::purge_all_for_uninstall`])。不摘的话
//! 卸载之后 AI 每来一个事件都要去跑一个已经不存在的 `miniterm-hook.exe`。
//!
//! - **为什么是主程序的参数而不是 sidecar 子命令**:摘除逻辑在 mt-ai(带 toml_edit 等),
//!   sidecar 不链 mt-ai;主程序本来就链着,且卸载器删文件之前它一定还在。
//! - **在 `main` 的最前面分流**:不装日志接管、不预载 ConPTY、不碰 gpui 与数据目录——
//!   卸载时用户可能从没启动过 mini-term,不该为此在 AppData 下建出目录来。
//! - **输出**:每家一行到 stdout,固定文案只用 ASCII —— 卸载器用 `nsExec::ExecToLog`
//!   把输出逐行贴进详情列表,nsExec 按系统 ANSI 代码页解码,中文会乱码(失败行里的
//!   错误原文除外,那是少见路径)。
//! - **退出码**:0 = 各家都处理完(含「没有可摘的」);1 = 至少一家失败。卸载器只拿它
//!   打一行提示,不因失败中断卸载。
//! - **升级不走这里**:新安装器调旧卸载器时带 `/UPGRADE`,卸载器据此跳过这一步
//!   (见 `scripts/windows-installer.nsi` 的 `UNINSTALL_OLD` 与 `Section "Uninstall"`)。

/// 卸载器传的参数。
pub const UNREGISTER_HOOKS_ARG: &str = "--unregister-hooks";

/// 命令行点名了某个无窗口模式就执行它并返回退出码;否则返回 `None`,照常起界面。
///
/// 只认**第一个**参数恰好等于模式名 —— 不做通用参数解析,免得将来的正常启动参数
/// 被误吞成无窗口模式。
pub fn run_if_requested() -> Option<i32> {
    let first = std::env::args_os().nth(1)?;
    if first != UNREGISTER_HOOKS_ARG {
        return None;
    }
    Some(unregister_hooks())
}

fn unregister_hooks() -> i32 {
    let mut failed = false;
    for (agent, result) in mt_ai::hook_registry::purge_all_for_uninstall() {
        println!("{}", report_line(agent.key(), &result));
        failed |= result.is_err();
    }
    i32::from(failed)
}

/// 卸载器详情列表里的一行。
fn report_line(key: &str, result: &Result<usize, String>) -> String {
    match result {
        Ok(0) => format!("[unregister-hooks] {key}: nothing to remove"),
        Ok(n) => format!("[unregister-hooks] {key}: removed {n}"),
        Err(e) => format!("[unregister-hooks] {key}: FAILED ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三种结局的文案固定、全 ASCII(失败原文除外)—— 卸载器详情按 ANSI 代码页解码。
    #[test]
    fn report_lines_are_ascii_and_distinguish_outcomes() {
        let nothing = report_line("claude", &Ok(0));
        let removed = report_line("codex", &Ok(7));
        let failed = report_line("grok", &Err("boom".to_string()));
        assert_eq!(nothing, "[unregister-hooks] claude: nothing to remove");
        assert_eq!(removed, "[unregister-hooks] codex: removed 7");
        assert_eq!(failed, "[unregister-hooks] grok: FAILED (boom)");
        for line in [nothing, removed, failed] {
            assert!(line.is_ascii(), "{line}");
        }
    }
}
