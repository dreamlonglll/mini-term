//! 「起一个 shell」的预处理层:cwd 判定 / WSL 启动器重写 / 环境变量装配。
//!
//! 全部是纯函数 —— 输入 [`PtySpawn`] + [`PtyOptions`],输出一份最终交给
//! `CommandBuilder` 的 [`LaunchPlan`]。真正的 `openpty`/`spawn` 在 `lib.rs`,
//! 这样这一层的每条分支都能在单测里覆盖,不必起真实进程。

use crate::{PtyOptions, PtySpawn};

/// 终端能力声明。TUI 应用(各家 AI CLI、vim、less…)靠这些变量开彩色与光标特性。
///
/// `LANG`/`LC_CTYPE`:Windows 上 Git for Windows(MSYS2)按 LANG 决定终端编码,
/// 缺失时 git 会退回系统 ANSI 代码页(中文 Windows 上是 GBK),提交信息出现乱码。
/// `LESSCHARSET` 让 git 的分页器(less)按 UTF-8 输出,而不是把字节转义成 `<XX>`。
const TERMINAL_ENV: [(&str, &str); 5] = [
    ("TERM", "xterm-256color"),
    ("COLORTERM", "truecolor"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LESSCHARSET", "utf-8"),
];

/// cwd 命中 WSL UNC 时的启动器重写结果。
///
/// 命中即**无视用户配置的 shell**,改用 `wsl.exe -d <distro> --cd <unix-path>`,
/// 与 Windows Terminal 的 `MangleStartingDirectoryForWSL` 行为一致。
/// 上层拿到它可以提示一次「已切换到 WSL 启动」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslOverride {
    pub distro: String,
    pub unix_path: String,
}

/// 判断是否要把 cwd 重写为 WSL 启动器。返回 `Some` 时调用方必须把 shell 切到
/// `wsl.exe`、args 切到 `["-d", &distro, "--cd", &unix_path]`,并把 portable-pty
/// 的 cwd 改成一个 Windows 端合法目录(如 `%USERPROFILE%`),
/// 避免 ConPTY 在 `is_dir()` 检查 UNC 路径失败时静默退回 `$USERPROFILE`。
///
/// 仅做纯字符串解析,跨平台行为一致 —— Linux/macOS 上的普通路径不会匹配 `\\` 前缀。
pub fn decide_wsl_override(cwd: &str) -> Option<WslOverride> {
    mt_core::parse_wsl_unc(cwd).map(|wsl| WslOverride {
        distro: wsl.distro,
        unix_path: wsl.unix_path,
    })
}

/// 选一个 Windows 端合法的兜底 cwd 给 portable-pty,
/// 避免把 WSL UNC 直接传给 ConPTY 触发 `$USERPROFILE` 静默 fallback。
pub fn fallback_windows_cwd() -> String {
    std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
}

/// 跨平台版兜底 cwd:SSH 远程启动器分支用。远程项目的 path 是远程 POSIX 绝对
/// 路径,不能传给 portable-pty;ssh 进程自己 `cd` 进远程目录,本地 cwd 只需合法。
pub fn fallback_local_cwd() -> String {
    if cfg!(windows) {
        fallback_windows_cwd()
    } else {
        std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
    }
}

/// 为 WSL 启动器分支拼装 `WSLENV` 环境变量的 value。
///
/// 输入 `user_envs` 已被外层过滤过(剔除保留前缀与用户自带的 `WSLENV` key);
/// 本函数只负责把剩余的 key 加上 `/u` flag 并用 `:` 连接,再把宿主已有的 `WSLENV`
/// (若存在且非空)追加在尾部合并 —— 不覆盖,与 JetBrains IDEA terminal / wslgit 对齐。
///
/// flag 选 `/u`(仅 Win→WSL 方向,不做路径翻译),避免把普通环境变量值当作路径转换。
///
/// 返回 `None` 当且仅当 `user_envs` 为空且宿主无 `WSLENV` —— 此时不应注入 WSLENV,
/// 否则会用空字符串覆盖宿主既有值。
pub fn build_wslenv_value(
    user_envs: &[(String, String)],
    host_wslenv: Option<&str>,
) -> Option<String> {
    let mut parts: Vec<String> = user_envs.iter().map(|(k, _)| format!("{}/u", k)).collect();
    if let Some(existing) = host_wslenv
        && !existing.is_empty()
    {
        parts.push(existing.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(":"))
    }
}

/// 预处理后的最终启动参数。
pub(crate) struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// 按**应用顺序**排好的环境变量:先终端默认值,再 `spec.env`,最后用户 env。
    /// 后者覆盖前者 —— 用户可以覆盖 TERM/LANG,但覆盖不了保留前缀(已被滤掉)。
    pub env: Vec<(String, String)>,
    pub wsl_override: Option<WslOverride>,
}

/// 把 spec + options 归并成一份可直接喂给 `CommandBuilder` 的计划。
pub(crate) fn plan(spec: &PtySpawn, options: &PtyOptions) -> LaunchPlan {
    let wsl_override = if options.wsl_cwd_rewrite {
        spec.cwd.as_deref().and_then(decide_wsl_override)
    } else {
        None
    };

    let (program, args, cwd) = match &wsl_override {
        // WSL 分支:启动的是宿主 wsl.exe,cwd 换成 Windows 端合法目录。
        Some(wsl) => (
            "wsl.exe".to_string(),
            vec![
                "-d".to_string(),
                wsl.distro.clone(),
                "--cd".to_string(),
                wsl.unix_path.clone(),
            ],
            Some(fallback_windows_cwd()),
        ),
        None => (spec.program.clone(), spec.args.clone(), spec.cwd.clone()),
    };

    let mut env: Vec<(String, String)> = Vec::new();
    if options.terminal_env {
        env.extend(
            TERMINAL_ENV
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
    }
    env.extend(spec.env.iter().cloned());

    // 用户 / 项目级 env 的防御性过滤:
    // - 保留前缀由调用方声明(默认 `MINITERM_`),挡住用户手改配置覆盖内部协议变量;
    // - `WSLENV` key 整条跳过 —— 它的 value 必须由下面这段自己拼,用户覆盖会破坏拼接。
    let user_env: Vec<(String, String)> = options
        .user_env
        .iter()
        .filter(|(k, _)| {
            k != "WSLENV"
                && !options
                    .reserved_env_prefixes
                    .iter()
                    .any(|prefix| k.starts_with(prefix.as_str()))
        })
        .cloned()
        .collect();

    // wsl.exe 进程的 env 不会自动透传给 distro 内的 shell,必须配合 WSLENV 机制:
    // 在 cmd.env(k, v) 之外额外注入 `WSLENV=K1/u:K2/u:...`,WSL init 才会在 distro
    // 内为 shell 设置同名变量。
    if wsl_override.is_some()
        && !user_env.is_empty()
        && let Some(value) = build_wslenv_value(&user_env, std::env::var("WSLENV").ok().as_deref())
    {
        env.push(("WSLENV".to_string(), value));
    }
    env.extend(user_env);

    LaunchPlan {
        program,
        args,
        cwd,
        env,
        wsl_override,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(cwd: Option<&str>) -> PtySpawn {
        PtySpawn {
            program: "pwsh.exe".to_string(),
            args: vec!["-NoLogo".to_string()],
            cwd: cwd.map(str::to_string),
            env: Vec::new(),
            rows: 24,
            cols: 80,
        }
    }

    fn value_of<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    // === WSL UNC 启动器重写检测 ===
    // 完整的 spawn 会起真实 shell,不适合在单测里跑;这里只验证纯函数层的分支选择,
    // 覆盖 "WSL UNC 触发重写 / 普通路径不触发 / Linux 路径不触发" 三种场景。

    #[test]
    fn wsl_override_triggered_by_wsl_dollar_unc() {
        let result = decide_wsl_override(r"\\wsl$\Ubuntu\home\u\proj");
        assert_eq!(
            result,
            Some(WslOverride {
                distro: "Ubuntu".to_string(),
                unix_path: "/home/u/proj".to_string(),
            })
        );
    }

    #[test]
    fn wsl_override_triggered_by_wsl_localhost_unc() {
        let result = decide_wsl_override(r"\\wsl.localhost\Ubuntu-22.04\home\u\proj");
        assert_eq!(
            result,
            Some(WslOverride {
                distro: "Ubuntu-22.04".to_string(),
                unix_path: "/home/u/proj".to_string(),
            })
        );
    }

    #[test]
    fn wsl_override_triggered_by_verbatim_unc() {
        // Rust canonicalize 在 WSL UNC 上输出 \\?\UNC\<host>\<rest>;
        // 与裸 \\wsl$\ 等价,decide_wsl_override 必须识别两种形式。
        let result = decide_wsl_override(r"\\?\UNC\wsl$\Ubuntu\home\u");
        assert_eq!(
            result,
            Some(WslOverride {
                distro: "Ubuntu".to_string(),
                unix_path: "/home/u".to_string(),
            })
        );
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

    #[test]
    fn fallback_local_cwd_returns_nonempty() {
        assert!(!fallback_local_cwd().is_empty());
    }

    // === plan():程序 / 参数 / cwd 的分支选择 ===

    #[test]
    fn plan_keeps_user_shell_for_ordinary_cwd() {
        let plan = plan(&spec(Some(r"D:\Git\mini-term")), &PtyOptions::default());
        assert_eq!(plan.program, "pwsh.exe");
        assert_eq!(plan.args, vec!["-NoLogo".to_string()]);
        assert_eq!(plan.cwd.as_deref(), Some(r"D:\Git\mini-term"));
        assert!(plan.wsl_override.is_none());
    }

    #[test]
    fn plan_rewrites_shell_to_wsl_launcher_for_unc_cwd() {
        let plan = plan(&spec(Some(r"\\wsl$\Ubuntu\home\u")), &PtyOptions::default());
        assert_eq!(plan.program, "wsl.exe");
        assert_eq!(
            plan.args,
            vec![
                "-d".to_string(),
                "Ubuntu".to_string(),
                "--cd".to_string(),
                "/home/u".to_string()
            ]
        );
        // cwd 必须换成 Windows 端合法目录,不能把 UNC 交给 ConPTY
        assert_ne!(plan.cwd.as_deref(), Some(r"\\wsl$\Ubuntu\home\u"));
        assert!(plan.wsl_override.is_some());
    }

    #[test]
    fn plan_wsl_rewrite_can_be_disabled() {
        let options = PtyOptions {
            wsl_cwd_rewrite: false,
            ..PtyOptions::default()
        };
        let plan = plan(&spec(Some(r"\\wsl$\Ubuntu\home\u")), &options);
        assert_eq!(plan.program, "pwsh.exe");
        assert!(plan.wsl_override.is_none());
    }

    // === plan():环境变量装配 ===

    #[test]
    fn plan_injects_terminal_env_by_default() {
        let plan = plan(&spec(None), &PtyOptions::default());
        assert_eq!(value_of(&plan.env, "TERM"), Some("xterm-256color"));
        assert_eq!(value_of(&plan.env, "COLORTERM"), Some("truecolor"));
        assert_eq!(value_of(&plan.env, "LANG"), Some("C.UTF-8"));
        assert_eq!(value_of(&plan.env, "LC_CTYPE"), Some("C.UTF-8"));
        assert_eq!(value_of(&plan.env, "LESSCHARSET"), Some("utf-8"));
    }

    #[test]
    fn plan_terminal_env_can_be_disabled() {
        let options = PtyOptions {
            terminal_env: false,
            ..PtyOptions::default()
        };
        let plan = plan(&spec(None), &options);
        assert!(value_of(&plan.env, "TERM").is_none());
    }

    #[test]
    fn plan_user_env_overrides_terminal_defaults() {
        // 顺序在内部 env 之后,允许用户按项目覆盖 TERM/LANG 等标准变量。
        let options = PtyOptions {
            user_env: vec![("TERM".to_string(), "screen-256color".to_string())],
            ..PtyOptions::default()
        };
        let plan = plan(&spec(None), &options);
        assert_eq!(value_of(&plan.env, "TERM"), Some("screen-256color"));
    }

    #[test]
    fn plan_filters_reserved_prefix_from_user_env() {
        // 用户绕过前端校验手改配置塞进保留前缀,也不能覆盖应用注入的内部变量。
        let mut spec = spec(None);
        spec.env
            .push(("MINITERM_PTY_ID".to_string(), "1".to_string()));
        let options = PtyOptions {
            user_env: vec![
                ("MINITERM_PTY_ID".to_string(), "999".to_string()),
                ("FOO".to_string(), "bar".to_string()),
            ],
            ..PtyOptions::default()
        };
        let plan = plan(&spec, &options);
        assert_eq!(value_of(&plan.env, "MINITERM_PTY_ID"), Some("1"));
        assert_eq!(value_of(&plan.env, "FOO"), Some("bar"));
    }

    #[test]
    fn plan_reserved_prefixes_are_configurable() {
        let options = PtyOptions {
            reserved_env_prefixes: vec!["MT_".to_string()],
            user_env: vec![
                ("MT_SECRET".to_string(), "x".to_string()),
                ("MINITERM_PTY_ID".to_string(), "999".to_string()),
            ],
            ..PtyOptions::default()
        };
        let plan = plan(&spec(None), &options);
        assert!(value_of(&plan.env, "MT_SECRET").is_none());
        // 换了保留前缀清单后,原来的前缀不再被过滤
        assert_eq!(value_of(&plan.env, "MINITERM_PTY_ID"), Some("999"));
    }

    #[test]
    fn plan_drops_user_supplied_wslenv_key() {
        // WSLENV 的 value 必须由 mini-term 自己拼,用户覆盖会破坏拼接结果。
        let options = PtyOptions {
            user_env: vec![("WSLENV".to_string(), "EVIL/p".to_string())],
            ..PtyOptions::default()
        };
        let plan = plan(&spec(Some(r"\\wsl$\Ubuntu\home")), &options);
        assert!(value_of(&plan.env, "WSLENV").is_none_or(|v| v != "EVIL/p"));
    }

    #[test]
    fn plan_injects_wslenv_only_on_wsl_branch() {
        let options = PtyOptions {
            user_env: vec![("FOO".to_string(), "1".to_string())],
            ..PtyOptions::default()
        };
        let local = plan(&spec(Some(r"C:\proj")), &options);
        assert!(value_of(&local.env, "WSLENV").is_none());

        let wsl = plan(&spec(Some(r"\\wsl$\Ubuntu\home")), &options);
        assert!(
            value_of(&wsl.env, "WSLENV").is_some_and(|v| v.starts_with("FOO/u")),
            "WSL 分支必须注入 WSLENV 才能把项目 env 透传进 distro"
        );
    }

    // === WSLENV 字符串拼接(WSL 分支项目级 env 注入) ===
    // 覆盖 build_wslenv_value 纯函数的所有路径:
    // - 空列表 → None(避免用空 WSLENV 覆盖宿主既有值)
    // - 单条 / 多条变量 → "K1/u" / "K1/u:K2/u"(/u flag 与 JetBrains IDEA 对齐)
    // - 宿主既有 WSLENV → 追加在尾部合并(不覆盖)
    // - 宿主 WSLENV 为空 / Some("") → 视同 None,不追加
    // 外层过滤(保留前缀 / WSLENV key)由 plan 负责,本函数不重复。

    #[test]
    fn build_wslenv_empty_no_host_returns_none() {
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
        // 宿主已有 WSLENV=EXISTING_VAR/p → 拼出 K1/u:K2/u 后在尾部追加,不覆盖。
        let envs = vec![
            ("FOO".to_string(), "1".to_string()),
            ("BAR".to_string(), "2".to_string()),
        ];
        let result = build_wslenv_value(&envs, Some("EXISTING_VAR/p"));
        assert_eq!(result, Some("FOO/u:BAR/u:EXISTING_VAR/p".to_string()));
    }

    #[test]
    fn build_wslenv_empty_user_envs_but_host_has_wslenv() {
        // user_envs 空但宿主有 WSLENV → 仍返回 Some(宿主值):保持纯函数语义
        // (输入有非空内容就有输出);plan 侧另有 !user_env.is_empty() 兜底。
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
