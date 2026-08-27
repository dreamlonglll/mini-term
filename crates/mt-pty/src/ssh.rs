//! SSH 相关的 PTY 侧逻辑:密码自动填充状态机 + 远程启动器 argv 拼装。
//!
//! 为什么归本 crate:两件事都只跟「往 PTY 里写什么字节 / 用什么 argv 起子进程」
//! 有关,不需要知道连接是怎么配置的。**查连接、解密码、准备私钥临时副本**属于
//! 上层(配置层),本模块只接收已经取到的明文密码与主机参数。

use mt_core::{SshPromptScan, scan_ssh_prompt, strip_ansi_codes};

/// 跨缓冲块匹配密码提示时保留的输出尾部长度(字符)。
const RESIDUAL_KEEP: usize = 256;

/// 一个 PTY 会话的 SSH 密码自动填充状态。
///
/// 生命周期:`new` 注册 → 每段 PTY 输出喂给 [`feed`](Self::feed) → 命中密码提示
/// 回写一次密码后自解除(`done`);命中 "Permission denied, please try again."
/// 则永久禁用,避免连灌错误密码把账号锁掉。
pub struct SshAutofill {
    password: String,
    /// 累加的输出尾部,用于跨缓冲块匹配密码提示
    residual: String,
    /// 已填充或已禁用(命中错误密码)后置位,后续输出不再处理
    done: bool,
    /// 用户首次向 PTY 真实输入时是否解除本 autofill。
    /// - 远程项目 pane(直接 spawn ssh,arm 后无命令写入,首个 write 即用户输入)
    ///   置 `true`:一旦用户打字即解除,避免 publickey 登录成功后 autofill 终身待命、
    ///   把 SSH 密码灌进后续 su / mysql -p / passwd 提示。
    /// - 「SSH 连接」菜单路径(arm 后紧跟一条 `ssh ...\r` 命令写入)置 `false`:
    ///   否则那条命令写入会在密码提示到达前就把 autofill 删掉,破坏该功能;
    ///   它仍靠命中密码提示后置 `done` 自解除。
    disarm_on_input: bool,
}

impl SshAutofill {
    pub fn new(password: String, disarm_on_input: bool) -> Self {
        Self {
            password,
            residual: String::new(),
            done: false,
            disarm_on_input,
        }
    }

    /// 用户真实输入时是否应当解除本 autofill(见字段注释)。
    pub fn disarm_on_input(&self) -> bool {
        self.disarm_on_input
    }

    /// 已完成(填过密码或命中认证失败)。
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// 喂一段 PTY 输出。命中密码提示时返回**应回写到 PTY 的密码**(不含回车,
    /// 调用方负责补 `\r`),每个会话只返回一次。
    pub fn feed(&mut self, data: &str) -> Option<String> {
        if self.done {
            return None;
        }
        self.residual.push_str(&strip_ansi_codes(data));
        // 仅保留尾部,解决提示被分块切断的情况;按 char 边界截断
        let count = self.residual.chars().count();
        if count > RESIDUAL_KEEP {
            self.residual = self.residual.chars().skip(count - RESIDUAL_KEEP).collect();
        }
        match scan_ssh_prompt(&self.residual) {
            SshPromptScan::AuthFailed => {
                self.done = true;
                None
            }
            SshPromptScan::Password => {
                self.done = true;
                Some(self.password.clone())
            }
            SshPromptScan::None => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 远程启动器 argv(把 ssh 本身当成 PTY 的子进程起起来)
// ---------------------------------------------------------------------------

/// POSIX shell 单引号安全包裹:`'` → `'\''`。
/// 远程路径来自用户输入,拼进 `cd <path>` 前必须做引号安全处理,
/// 防止 `;`、`$()`、空格等在远程 shell 里被解释。
pub fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 拼 ssh 的远端命令:`cd '<path>' 2>/dev/null; exec $SHELL -l`。
/// `$SHELL` 保持字面量 —— 本地不经过 shell(portable-pty 直接 spawn ssh,
/// 参数按 argv 传递),它由远程 sshd 用登录 shell 执行时才展开,
/// 从而落在用户自己的默认 shell 上。路径失效时忽略 `cd` 错误并从登录目录启动。
pub fn build_remote_login_command(remote_path: &str) -> String {
    format!(
        "cd {} 2>/dev/null; exec $SHELL -l",
        shell_single_quote(remote_path)
    )
}

/// 拼直接 spawn `ssh` 作 PTY 子进程的参数列表(不经本地 shell,
/// 对齐 WSL 分支 spawn wsl.exe 的启动器重写模式)。
///
/// 形如:`-t [-p <port>] [-i <identity>] user@host "cd '<path>' 2>/dev/null; exec $SHELL -l"`。
/// **绝不能加 `-o BatchMode=yes`**:它会连带禁用密码认证,
/// 而密码连接依赖 PTY autofill 灌密码([`SshAutofill`])。
pub fn build_ssh_launcher_args(
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

/// 定位本机 ssh 客户端。Windows 10+ 自带 OpenSSH 客户端(System32\\OpenSSH),
/// 缺失时返回 None 由调用方给出明确安装提示。
pub fn find_ssh_client() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        find_in_path("ssh.exe")
    } else {
        find_in_path("ssh")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === 密码自动填充状态机 ===

    #[test]
    fn autofill_fills_once_on_password_prompt() {
        let mut autofill = SshAutofill::new("secret".into(), true);
        assert_eq!(
            autofill.feed("root@10.0.0.5's password: ").as_deref(),
            Some("secret")
        );
        // 每个会话只填一次:后续再出现提示不再回写
        assert!(autofill.is_done());
        assert!(autofill.feed("root@10.0.0.5's password: ").is_none());
    }

    #[test]
    fn autofill_matches_prompt_split_across_chunks() {
        // 提示被 PTY 读缓冲切成两段,靠 residual 拼回来。
        let mut autofill = SshAutofill::new("secret".into(), false);
        assert!(autofill.feed("root@host's pass").is_none());
        assert_eq!(autofill.feed("word: ").as_deref(), Some("secret"));
    }

    #[test]
    fn autofill_strips_ansi_before_matching() {
        let mut autofill = SshAutofill::new("secret".into(), false);
        assert_eq!(
            autofill
                .feed("\x1b[1;32mroot@host's password: \x1b[0m")
                .as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn autofill_disabled_after_permission_denied() {
        // 认证失败后永久禁用:不能对着下一个提示继续灌错误密码。
        let mut autofill = SshAutofill::new("wrong".into(), false);
        assert!(
            autofill
                .feed("Permission denied, please try again.\r\nroot@host's password: ")
                .is_none()
        );
        assert!(autofill.is_done());
        assert!(autofill.feed("root@host's password: ").is_none());
    }

    #[test]
    fn autofill_ignores_hostkey_and_passphrase_prompts() {
        let mut autofill = SshAutofill::new("secret".into(), false);
        assert!(
            autofill
                .feed("Are you sure you want to continue connecting (yes/no/[fingerprint])? ")
                .is_none()
        );
        assert!(
            autofill
                .feed("Enter passphrase for key '/home/u/.ssh/id_rsa': ")
                .is_none()
        );
    }

    #[test]
    fn autofill_residual_is_bounded() {
        // 长时间刷屏不会把 residual 撑成常驻内存;尾部保留量足够跨块匹配提示。
        let mut autofill = SshAutofill::new("secret".into(), false);
        for _ in 0..50 {
            assert!(autofill.feed(&"x".repeat(1024)).is_none());
        }
        assert!(autofill.residual.chars().count() <= RESIDUAL_KEEP);
        // 截断后仍能正常命中提示
        assert_eq!(
            autofill.feed("root@host's password: ").as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn autofill_disarm_flag_is_reported_verbatim() {
        // 远程项目 pane 传 true(用户一打字即解除);
        // 「SSH 连接」菜单路径传 false(arm 后紧跟的 ssh 命令写入不得解除)。
        assert!(SshAutofill::new("s".into(), true).disarm_on_input());
        assert!(!SshAutofill::new("s".into(), false).disarm_on_input());
    }

    // === 远程启动器 argv ===

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
        assert_eq!(cmd, "cd '/home/u/my proj' 2>/dev/null; exec $SHELL -l");
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
                "cd '/srv/app' 2>/dev/null; exec $SHELL -l".to_string(),
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
                "cd '/home/deploy' 2>/dev/null; exec $SHELL -l".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_launcher_args_never_uses_batchmode() {
        // BatchMode=yes 会连带禁用密码认证,破坏 PTY autofill 灌密码链路。
        // 任何组合下都不允许出现。
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
            r"cd '/tmp'\''; rm -rf /; echo '\''' 2>/dev/null; exec $SHELL -l"
        );
    }
}
