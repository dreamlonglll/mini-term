//! `~/.ssh/config` 的最小实现,以及「连接 → 实际建连路线」的解析。
//!
//! 终端 pane 把 `ssh [-p port] [-i key] user@host` 交给系统 OpenSSH 执行,host 填的是
//! ssh config 里的别名、或者目标要经跳板机才能到时,终端照样能连。池要对齐这一行为,
//! 但不值得为此引入依赖:这里只解析建连真正用得到的几个关键字,其余一律忽略。
//!
//! 语法口径对齐 OpenSSH `readconf.c`:
//!
//! - 关键字大小写不敏感;`key value` 与 `key=value`(`=` 两侧可有空白)都认;参数按
//!   `argv_split` 规则切分:双 / 单引号、`\"` `\'` `\\` 与引号外的 `\ ` 转义(其它反斜杠
//!   原样保留,`C:\Users\me` 这类 Windows 路径不受影响),参数开头的 `#` 起注释;
//! - `Host` 后可跟多个模式(空白分隔),支持 `*` / `?` 通配与 `!` 否定:任一否定模式命中
//!   则整块不适用,否则任一肯定模式命中即适用;与 OpenSSH 一样按**原样**(区分大小写)
//!   匹配用户写的主机名 —— 终端里的 ssh 拿到的就是同一个字符串;
//! - 同一关键字**首个取值生效**,`IdentityFile` 例外(多条依次累加);
//! - `Match` 块整块跳过(只有 `Match all` 视为生效,它本就匹配一切):`Match` 的条件
//!   (`exec` / `localuser` / `canonical` ……)在这里没法如实求值,硬猜可能把只对特定
//!   条件生效的 `HostName` / `ProxyJump` 用到别的连接上;整块跳过最多是少拿几条设置,
//!   连不上时报错里能看出来;
//! - `Include`:相对路径相对 `~/.ssh`,支持 `~` 与文件名里的 `*` / `?` 通配(按文件名
//!   排序依次读);写在 `Host` 块里时只在该块生效时读(OpenSSH 的条件包含);嵌套最多
//!   16 层,并跳过正在读的祖先文件,防止循环包含;
//! - 文件不存在 = 没有配置;存在却读不了(Windows 上编辑器保存时独占着文件之类)报
//!   [`RouteError::Unreadable`],由会话池沿用上次解析出的路线,不把它当成「没有配置」
//!   (那会让路线变样、白白淘汰健康的会话);
//! - Unix 上组或其他用户可写的配置文件(主配置与 Include 的)整个忽略并记一行日志,与
//!   OpenSSH 的「Bad owner or permissions」同一口径。只查权限位:属主检查要当前 uid,
//!   不加依赖拿不到。Windows 不查。
//!
//! 建连路线的取值规则(与终端拼的命令行等价)见 [`resolve_route`]。

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use mt_core::SshConnection;

/// Include 的最大嵌套深度(OpenSSH `READCONF_MAX_DEPTH`)。
const MAX_INCLUDE_DEPTH: usize = 16;

/// ProxyJump 最多几跳。没有协议上的限制,只是挡住写错的超长链。
const MAX_JUMP_HOPS: usize = 8;

/// 解析建连路线失败的分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RouteError {
    /// 配置内容或连接本身有误(取值非法、Include 过深、没有用户名……):改了才会好。
    Invalid(String),
    /// ssh config(或它 Include 的文件)存在却读不了。多半转瞬即逝(编辑器保存时的独占锁),
    /// 调用方可沿用上次解析出的路线。
    Unreadable(String),
}

impl RouteError {
    pub(crate) fn message(&self) -> &str {
        match self {
            RouteError::Invalid(message) | RouteError::Unreadable(message) => message,
        }
    }
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// 解析途中的普通错误(取值非法……)一律是 [`RouteError::Invalid`]。
impl From<String> for RouteError {
    fn from(message: String) -> Self {
        RouteError::Invalid(message)
    }
}

/// 同一句提醒每个进程只记一次:路线每次 acquire 都重新解析,配置文件权限不对之类的
/// 持续状况不该把日志刷满。
pub(crate) fn warn_once(message: String) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut warned = WARNED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !warned.contains(&message) {
        eprintln!("[mt-ssh] {message}");
        warned.insert(message);
    }
}

/// 组或其他用户可写(OpenSSH `readconf.c` 权限检查里权限位的那一半:`mode & 022`)。
fn writable_by_others(mode: u32) -> bool {
    mode & 0o022 != 0
}

/// 打开的配置文件的权限位。只有 Unix 有意义;Windows 不查(OpenSSH for Windows 改查
/// ACL,这里不跟)。
#[cfg(unix)]
fn file_mode(file: &std::fs::File) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    file.metadata()
        .ok()
        .map(|metadata| metadata.permissions().mode())
}

#[cfg(not(unix))]
fn file_mode(_file: &std::fs::File) -> Option<u32> {
    None
}

/// 读一个 ssh config 文件。不存在是 `Ok(None)`;组或其他用户可写时同样当作不存在,记一行
/// 日志(OpenSSH 会因「Bad owner or permissions」拒绝这样的文件);其它 IO 错误原样返回。
/// 权限按打开后的文件查(fstat 口径,不怕查完被换掉)。
fn read_config_file(path: &Path) -> std::io::Result<Option<String>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if let Some(mode) = file_mode(&file).filter(|&mode| writable_by_others(mode)) {
        warn_once(format!(
            "ssh config {} ignored: bad owner or permissions (mode {:o}; OpenSSH refuses a \
             config file that is writable by group or others, run `chmod 600` on it)",
            path.display(),
            mode & 0o7777
        ));
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// OpenSSH `match_pattern`:`*` 匹配任意长(含空)串,`?` 恰好一个字符,其余逐字符
/// 相等(区分大小写 —— 需要不敏感的调用方自己先转小写)。
pub(crate) fn match_pattern(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut t, mut p) = (0usize, 0usize);
    // 最近一个 `*` 之后的模式位置,以及它当前吞到的文本位置(回溯用)。
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            t += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p + 1, t));
            p += 1;
        } else if let Some((after_star, swallowed)) = star {
            p = after_star;
            t = swallowed + 1;
            star = Some((after_star, swallowed + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// ssh config 里的代理设置。`ProxyJump` 与 `ProxyCommand` 谁先出现谁生效(OpenSSH 语义)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProxySetting {
    Jump(String),
    Command(String),
}

impl ProxySetting {
    /// `ProxyJump none` / `ProxyCommand none`:显式不用代理。
    fn is_none(&self) -> bool {
        match self {
            ProxySetting::Jump(value) | ProxySetting::Command(value) => {
                value.eq_ignore_ascii_case("none")
            }
        }
    }
}

/// 对某个主机名生效的 ssh config 设置(只含本实现支持的关键字,值未展开 token)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HostConfig {
    pub(crate) host_name: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) user: Option<String>,
    pub(crate) identity_files: Vec<String>,
    pub(crate) identities_only: Option<bool>,
    pub(crate) host_key_alias: Option<String>,
    pub(crate) proxy: Option<ProxySetting>,
}

/// OpenSSH `argv_split`:按空白切参数,支持引号与少数转义,参数开头的 `#` 起注释。
fn split_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(' ' | '\t')) {
            chars.next();
        }
        match chars.peek() {
            None | Some('#') => break,
            Some(_) => {}
        }
        let mut arg = String::new();
        let mut quote: Option<char> = None;
        while let Some(&c) = chars.peek() {
            match (c, quote) {
                (' ' | '\t', None) => break,
                ('\\', _) => {
                    chars.next();
                    match chars.peek() {
                        Some(&next)
                            if next == '\''
                                || next == '"'
                                || next == '\\'
                                || (quote.is_none() && next == ' ') =>
                        {
                            arg.push(next);
                            chars.next();
                        }
                        // 认不出的转义:反斜杠原样保留(Windows 路径靠这条)。
                        _ => arg.push('\\'),
                    }
                    continue;
                }
                ('"' | '\'', None) => quote = Some(c),
                (c, Some(q)) if c == q => quote = None,
                (c, _) => arg.push(c),
            }
            chars.next();
        }
        args.push(arg);
    }
    args
}

/// 一行拆成「小写关键字 + 参数」。空行 / 注释行返回 `None`。
fn split_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim_matches(|c: char| c.is_whitespace());
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let keyword = line[..end].to_ascii_lowercase();
    let mut rest = line[end..].trim_start();
    // 关键字后至多一个 `=`(OpenSSH `strdelim`)。
    if let Some(stripped) = rest.strip_prefix('=') {
        rest = stripped.trim_start();
    }
    Some((keyword, split_args(rest)))
}

/// `Host` 行的模式表是否对 `alias` 生效。
fn host_line_matches(patterns: &[String], alias: &str) -> bool {
    let mut active = false;
    for pattern in patterns {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(pattern) => (true, pattern),
            None => (false, pattern.as_str()),
        };
        if match_pattern(alias, pattern) {
            if negated {
                return false;
            }
            active = true;
        }
    }
    active
}

fn parse_yes_no(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "yes" | "true" => Some(true),
        "no" | "false" => Some(false),
        _ => None,
    }
}

/// 把 `~` / `~/…` / `~\…` 展开到 home。`~user` 形式不支持(报错,不瞎猜别人的 home)。
pub(crate) fn expand_tilde(path: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let rest = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
    };
    match rest {
        Some(rest) => match home {
            Some(home) if rest.is_empty() => Ok(home.to_path_buf()),
            Some(home) => Ok(home.join(rest)),
            None => Err(format!(
                "cannot expand '~' in '{path}': home directory unknown"
            )),
        },
        None if path.starts_with('~') => Err(format!(
            "cannot expand '{path}': only '~' / '~/...' (the current user's home) is supported"
        )),
        None => Ok(PathBuf::from(path)),
    }
}

/// `%` token 展开需要的上下文。
pub(crate) struct TokenContext<'a> {
    pub(crate) home: Option<&'a Path>,
    pub(crate) local_user: Option<&'a str>,
    /// 解析后的 HostName。
    pub(crate) host_name: &'a str,
    pub(crate) remote_user: &'a str,
    pub(crate) port: u16,
    /// 用户写的原始主机名(ssh config 的匹配对象)。
    pub(crate) alias: &'a str,
}

/// 展开 OpenSSH `%` token 的最小子集:`%%` `%d`(本机 home)`%u`(本机用户)
/// `%h`(解析后的 HostName)`%r`(远端用户)`%p`(端口)`%n`(原始主机名)。
/// 其它 token 报错 —— OpenSSH 同样拒绝认不出的 token,硬留着只会去读一个不存在的路径。
pub(crate) fn expand_tokens(value: &str, ctx: &TokenContext<'_>) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let unknown = |what: &str| format!("cannot expand %{what} in '{value}': unknown");
        match chars.next() {
            Some('%') => out.push('%'),
            Some('d') => {
                let home = ctx.home.ok_or_else(|| unknown("d (home directory)"))?;
                out.push_str(&home.to_string_lossy());
            }
            Some('u') => out.push_str(ctx.local_user.ok_or_else(|| unknown("u (local user)"))?),
            Some('h') => out.push_str(ctx.host_name),
            Some('r') => out.push_str(ctx.remote_user),
            Some('p') => out.push_str(&ctx.port.to_string()),
            Some('n') => out.push_str(ctx.alias),
            Some(other) => {
                return Err(format!(
                    "unsupported token %{other} in '{value}' (supported: %% %d %u %h %r %p %n)"
                ));
            }
            None => return Err(format!("dangling '%' at the end of '{value}'")),
        }
    }
    Ok(out)
}

/// 读 ssh config,收集对 `alias` 生效的设置。`config_path` 为 `None` 时不读任何文件。
///
/// 文件不存在 = 没有配置。存在却读不了(独占锁、权限……)报 [`RouteError::Unreadable`]:
/// 把它当成「没有配置」会让路线变样(别名解析不了、跳板没了),调用方应沿用上次的路线;
/// `tolerate_unreadable` 为真(没有上次可沿用时)才记一行日志后跳过它。只有**生效行**的
/// 取值非法(`Port abc`)、`Include` 嵌套过深才是 [`RouteError::Invalid`]:终端里的 ssh
/// 遇到同样的配置也会拒绝连接。
pub(crate) fn read_host_config(
    config_path: Option<&Path>,
    home: Option<&Path>,
    alias: &str,
    tolerate_unreadable: bool,
) -> Result<HostConfig, RouteError> {
    let mut reader = ConfigReader {
        alias,
        home,
        tolerate_unreadable,
        config: HostConfig::default(),
        stack: Vec::new(),
    };
    if let Some(path) = config_path {
        reader.process_file(path, 0)?;
    }
    Ok(reader.config)
}

struct ConfigReader<'a> {
    alias: &'a str,
    home: Option<&'a Path>,
    /// 读不了的文件记日志后跳过,而不是报 [`RouteError::Unreadable`]。
    tolerate_unreadable: bool,
    config: HostConfig,
    /// 正在读的文件链(防循环 Include)。
    stack: Vec<PathBuf>,
}

impl ConfigReader<'_> {
    fn process_file(&mut self, path: &Path, depth: usize) -> Result<(), RouteError> {
        let text = match read_config_file(path) {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(()),
            Err(e) if self.tolerate_unreadable => {
                eprintln!(
                    "[mt-ssh] ssh config {} unreadable, ignored: {e}",
                    path.display()
                );
                return Ok(());
            }
            Err(e) => {
                return Err(RouteError::Unreadable(format!(
                    "ssh config {} is unreadable: {e}",
                    path.display()
                )));
            }
        };
        self.stack.push(path.to_path_buf());
        let result = self.process_text(&text, path, depth);
        self.stack.pop();
        result
    }

    fn process_text(&mut self, text: &str, path: &Path, depth: usize) -> Result<(), RouteError> {
        // 文件开头(全局段)对所有主机生效;被 Include 进来的文件同样从「生效」开始 ——
        // 只有生效位置的 Include 才会读到这里,与 OpenSSH 继承包含处状态的效果一致。
        let mut active = true;
        for (index, line) in text.lines().enumerate() {
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            let Some((keyword, args)) = split_line(line) else {
                continue;
            };
            let location = || format!("{}:{}", path.display(), index + 1);
            match keyword.as_str() {
                "host" => active = host_line_matches(&args, self.alias),
                "match" => active = args.len() == 1 && args[0].eq_ignore_ascii_case("all"),
                "include" if active => {
                    for arg in &args {
                        self.include(arg, depth, &location())?;
                    }
                }
                _ if active => self.apply(&keyword, &args, &location)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn apply(
        &mut self,
        keyword: &str,
        args: &[String],
        location: &dyn Fn() -> String,
    ) -> Result<(), String> {
        let Some(first) = args.first() else {
            return Ok(());
        };
        let config = &mut self.config;
        match keyword {
            "hostname" => {
                config.host_name.get_or_insert_with(|| first.clone());
            }
            "port" => {
                let port = first
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| {
                        format!("ssh config {}: invalid Port value '{first}'", location())
                    })?;
                config.port.get_or_insert(port);
            }
            "user" => {
                config.user.get_or_insert_with(|| first.clone());
            }
            "identityfile" => config.identity_files.push(first.clone()),
            "identitiesonly" => {
                let flag = parse_yes_no(first).ok_or_else(|| {
                    format!(
                        "ssh config {}: invalid IdentitiesOnly value '{first}'",
                        location()
                    )
                })?;
                config.identities_only.get_or_insert(flag);
            }
            "hostkeyalias" => {
                config.host_key_alias.get_or_insert_with(|| first.clone());
            }
            "proxyjump" => {
                config
                    .proxy
                    .get_or_insert_with(|| ProxySetting::Jump(first.clone()));
            }
            "proxycommand" => {
                config
                    .proxy
                    .get_or_insert_with(|| ProxySetting::Command(args.join(" ")));
            }
            _ => {}
        }
        Ok(())
    }

    fn include(&mut self, arg: &str, depth: usize, location: &str) -> Result<(), RouteError> {
        if depth + 1 > MAX_INCLUDE_DEPTH {
            return Err(RouteError::Invalid(format!(
                "ssh config {location}: Include nested more than {MAX_INCLUDE_DEPTH} levels deep"
            )));
        }
        let pattern = if arg.starts_with('~') {
            expand_tilde(arg, self.home).map_err(|e| format!("ssh config {location}: {e}"))?
        } else if Path::new(arg).has_root() {
            PathBuf::from(arg)
        } else {
            // 用户配置里的相对路径相对 `~/.ssh`(OpenSSH 口径)。
            match self.home {
                Some(home) => home.join(".ssh").join(arg),
                None => {
                    return Err(RouteError::Invalid(format!(
                        "ssh config {location}: cannot resolve Include '{arg}': home directory unknown"
                    )));
                }
            }
        };
        for file in glob_files(&pattern) {
            if self.stack.contains(&file) {
                eprintln!(
                    "[mt-ssh] ssh config {location}: skipping recursive Include of {}",
                    file.display()
                );
                continue;
            }
            self.process_file(&file, depth + 1)?;
        }
        Ok(())
    }
}

/// `Include` 的通配展开:只支持**文件名**里的 `*` / `?`(目录部分按字面)。结果按文件名
/// 排序;没有通配就是那一个路径(不存在由读取时当作空)。与 glob(3) 一样,以通配开头的
/// 模式不匹配 `.` 开头的隐藏文件。Windows 文件名不区分大小写,匹配也不区分。
fn glob_files(pattern: &Path) -> Vec<PathBuf> {
    let Some(name_pattern) = pattern.file_name().and_then(|n| n.to_str()) else {
        return vec![pattern.to_path_buf()];
    };
    if !name_pattern.contains(['*', '?']) {
        return vec![pattern.to_path_buf()];
    }
    let dir = pattern.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let fold = |s: &str| {
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    let name_pattern_folded = fold(name_pattern);
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        // `Path::is_file` 跟随符号链接:指向文件的链接同样算(glob(3) 也认)。
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if name.starts_with('.') && !name_pattern.starts_with('.') {
                return None;
            }
            match_pattern(&fold(&name), &name_pattern_folded).then(|| entry.path())
        })
        .collect();
    files.sort();
    files
}

// ============================================================================
// 建连路线
// ============================================================================

/// 解析建连路线需要的本机环境。
pub(crate) struct RouteEnv<'a> {
    pub(crate) ssh_config_path: Option<&'a Path>,
    pub(crate) home: Option<&'a Path>,
    pub(crate) local_user: Option<&'a str>,
    /// 读不了的 ssh config 文件记日志后跳过(见 [`read_host_config`])。只在没有上次的
    /// 路线可沿用时打开。
    pub(crate) tolerate_unreadable: bool,
}

/// 私钥文件来自哪里(写进认证失败的报错,便于用户找到该改哪儿)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityOrigin {
    /// 连接配置里的私钥。
    Connection,
    /// ssh config 的 `IdentityFile`。
    SshConfig,
}

/// 一个待试的私钥文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentitySpec {
    /// 用户写的原样(报错里展示)。
    pub(crate) display: String,
    /// 展开 `~` / token 后的路径;展开失败时是失败原因。
    pub(crate) path: Result<PathBuf, String>,
    pub(crate) origin: IdentityOrigin,
}

/// 路线上的一跳(跳板或最终目标)解析后的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedHost {
    /// 用户写的主机名(连接的 host,或 ProxyJump 里写的那个),ssh config 按它匹配。
    pub(crate) alias: String,
    /// 实际去连的主机名(`HostName` 展开后;未配置即 `alias`)。
    pub(crate) host_name: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    /// 显式指定的私钥(连接配置或 `IdentityFile`),按尝试顺序。
    pub(crate) identities: Vec<IdentitySpec>,
    /// 是否显式指定过私钥。为 `true` 时不再试默认私钥(OpenSSH 同样只在一个
    /// `IdentityFile` 都没有时才补默认的 `~/.ssh/id_*`)。
    pub(crate) explicit_identities: bool,
    pub(crate) identities_only: bool,
    /// known_hosts 查找 / 写入名:`HostKeyAlias` 原样,否则 HostName 带端口。
    pub(crate) known_hosts_name: String,
}

impl ResolvedHost {
    /// `host_name:port`,报错与阶段描述用。
    pub(crate) fn endpoint(&self) -> String {
        format!("{}:{}", self.host_name, self.port)
    }
}

/// 一条连接解析出的完整路线:按顺序经过的跳板(第一跳在前)与最终目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedRoute {
    pub(crate) jumps: Vec<ResolvedHost>,
    pub(crate) target: ResolvedHost,
    /// ssh config 给这条连接配了本实现不支持的代理(`ProxyCommand`,或跳板自己又配了
    /// 代理),路线已回落为不经代理的直连(与读 ssh config 之前的行为一致)。这是要附在
    /// 连接失败的报错与日志里的那句说明。
    pub(crate) ignored_proxy: Option<String>,
}

/// known_hosts 名:`HostKeyAlias` 原样(OpenSSH 不给别名带端口),否则 HostName 按
/// `[host]:port` 规则带上非 22 端口。统一转小写,与 OpenSSH 写入口径一致(匹配本身不
/// 区分大小写)。
fn known_hosts_name(host_key_alias: Option<&str>, host_name: &str, port: u16) -> String {
    match host_key_alias {
        Some(alias) => alias.to_ascii_lowercase(),
        None => crate::known_hosts::host_pattern(&host_name.to_ascii_lowercase(), port),
    }
}

/// `HostName` 只认 `%h`(原始主机名)与 `%%`。
fn expand_host_name(value: &str, alias: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('h') => out.push_str(alias),
            Some(other) => {
                return Err(format!(
                    "unsupported token %{other} in HostName '{value}' (supported: %h %%)"
                ));
            }
            None => return Err(format!("dangling '%' at the end of HostName '{value}'")),
        }
    }
    Ok(out)
}

/// 用 ssh config 的 `IdentityFile` 列表生成待试私钥。`IdentityFile none` 只表示
/// 「不要默认私钥」,本身不产生条目。
fn config_identities(
    files: &[String],
    env: &RouteEnv<'_>,
    ctx: &TokenContext<'_>,
) -> Vec<IdentitySpec> {
    files
        .iter()
        .filter(|raw| !raw.eq_ignore_ascii_case("none"))
        .map(|raw| IdentitySpec {
            display: raw.clone(),
            path: expand_tilde(raw, env.home)
                .and_then(|p| expand_tokens(&p.to_string_lossy(), ctx).map(PathBuf::from)),
            origin: IdentityOrigin::SshConfig,
        })
        .collect()
}

/// 解析一条连接的建连路线(对齐终端 `ssh [-p port] [-i key] user@host` 的效果):
///
/// - `HostName`:连接的 host 是别名时换成真正的主机名(支持 `%h`);
/// - `Port`:只在连接的端口是 22 或 0 时生效 —— 终端只在非 22 时才传 `-p`;
/// - `User`:只在连接的用户为空时生效 —— 终端总是传 `user@host`;两边都没有时用本机用户名;
/// - `IdentityFile`:只在连接没配私钥时生效(可多条,支持 `~` 与 token);连接配了私钥
///   就只用它(`~` 同样展开);两边都没有时由认证阶段补默认私钥;
/// - `IdentitiesOnly`、`HostKeyAlias` 照常生效;
/// - `ProxyJump`:逗号分隔的一跳或多跳 `[user@]host[:port]`(可带 `ssh://`),`none`
///   表示不用跳板。每个跳板按它自己的名字再查一遍 ssh config(HostName / Port / User /
///   IdentityFile / IdentitiesOnly / HostKeyAlias),跳板上的端口 / 用户以 ProxyJump
///   里写的为准;
/// - `ProxyCommand`,以及跳板自己的 ssh config 又配了 ProxyJump / ProxyCommand(OpenSSH
///   会递归起子 ssh,这里不跟):不支持,回落为不经代理的直连,并在
///   [`ResolvedRoute::ignored_proxy`] 里留一句说明。`Host *` 配一条公司代理 / cloudflared
///   这类写法很常见,不该让本来能直连的连接全部失败。
pub(crate) fn resolve_route(
    conn: &SshConnection,
    env: &RouteEnv<'_>,
) -> Result<ResolvedRoute, RouteError> {
    let alias = conn.host.trim();
    if alias.is_empty() {
        return Err(RouteError::Invalid(
            "ssh connection has an empty host".into(),
        ));
    }
    let config = read_host_config(
        env.ssh_config_path,
        env.home,
        alias,
        env.tolerate_unreadable,
    )?;
    let host_name = match &config.host_name {
        Some(value) => expand_host_name(value, alias)?,
        None => alias.to_string(),
    };
    let port = if conn.port != 0 && conn.port != 22 {
        conn.port
    } else {
        config.port.unwrap_or(22)
    };
    let user = if !conn.user.trim().is_empty() {
        conn.user.clone()
    } else if let Some(user) = config
        .user
        .clone()
        .or_else(|| env.local_user.map(str::to_string))
    {
        user
    } else {
        return Err(RouteError::Invalid(format!(
            "no user name for {alias}: the connection has no user and ssh config sets no User"
        )));
    };
    let ctx = TokenContext {
        home: env.home,
        local_user: env.local_user,
        host_name: &host_name,
        remote_user: &user,
        port,
        alias,
    };
    let connection_key = conn
        .identity_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (identities, explicit_identities) = match connection_key {
        Some(raw) => (
            vec![IdentitySpec {
                display: raw.to_string(),
                path: expand_tilde(raw, env.home),
                origin: IdentityOrigin::Connection,
            }],
            true,
        ),
        None => (
            config_identities(&config.identity_files, env, &ctx),
            !config.identity_files.is_empty(),
        ),
    };
    let target = ResolvedHost {
        alias: alias.to_string(),
        known_hosts_name: known_hosts_name(config.host_key_alias.as_deref(), &host_name, port),
        host_name: host_name.clone(),
        port,
        user: user.clone(),
        identities,
        explicit_identities,
        identities_only: config.identities_only.unwrap_or(false),
    };
    let (jumps, ignored_proxy) = match &config.proxy {
        None => (Vec::new(), None),
        Some(proxy) if proxy.is_none() => (Vec::new(), None),
        Some(ProxySetting::Command(command)) => (
            Vec::new(),
            Some(format!(
                "ignored \"ProxyCommand {command}\" from ssh config for {alias} (mini-term's \
                 built-in SSH client cannot run proxy commands) and connected directly; 已忽略 \
                 ~/.ssh/config 里的 ProxyCommand,改为直连"
            )),
        ),
        Some(ProxySetting::Jump(spec)) => {
            let hops: Vec<&str> = spec.split(',').map(str::trim).collect();
            if hops.len() > MAX_JUMP_HOPS {
                return Err(RouteError::Invalid(format!(
                    "ProxyJump for {alias} has {} hops; at most {MAX_JUMP_HOPS} are supported",
                    hops.len()
                )));
            }
            let mut jumps = Vec::with_capacity(hops.len());
            let mut nested = None;
            for hop in hops {
                match resolve_jump(hop, env)? {
                    JumpHop::Host(host) => jumps.push(host),
                    JumpHop::NestedProxy(name) => {
                        nested = Some(name);
                        break;
                    }
                }
            }
            match nested {
                None => (jumps, None),
                Some(name) => (
                    Vec::new(),
                    Some(format!(
                        "ignored \"ProxyJump {spec}\" from ssh config for {alias} because jump \
                         host {name} sets its own ProxyJump/ProxyCommand (nested proxies are not \
                         supported by mini-term's built-in SSH client; list every hop in a single \
                         ProxyJump instead) and connected directly; 已忽略 ~/.ssh/config 里的 \
                         ProxyJump(跳板自己又配了代理),改为直连"
                    )),
                ),
            }
        }
    };
    Ok(ResolvedRoute {
        jumps,
        target,
        ignored_proxy,
    })
}

/// [`resolve_jump`] 的结果。
enum JumpHop {
    Host(ResolvedHost),
    /// 跳板自己又配了代理(值是跳板名):整条路线回落为直连。
    NestedProxy(String),
}

/// ProxyJump 里的一跳:`[ssh://][user@]host[:port]`,IPv6 写成 `[addr]:port`。
#[derive(Debug, PartialEq, Eq)]
struct JumpSpec {
    user: Option<String>,
    host: String,
    port: Option<u16>,
}

fn parse_jump_spec(spec: &str) -> Result<JumpSpec, String> {
    let invalid = |why: &str| format!("invalid ProxyJump hop '{spec}': {why}");
    let rest = spec.strip_prefix("ssh://").unwrap_or(spec);
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    // 用户名取到最后一个 `@` 为止(OpenSSH `parse_user_host_port` 同样用 strrchr)。
    let (user, host_port) = match rest.rsplit_once('@') {
        Some((user, host_port)) if !user.is_empty() => (Some(user.to_string()), host_port),
        Some(_) => return Err(invalid("empty user name")),
        None => (None, rest),
    };
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, after) = bracketed
            .split_once(']')
            .ok_or_else(|| invalid("missing ']'"))?;
        let port = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| invalid("expected ':port' after ']'"))?,
            ),
        };
        (host, port)
    } else {
        match host_port.split_once(':') {
            // 只有一个冒号才当 `host:port`;不带方括号的 IPv6 整串都是主机。
            Some((host, port)) if !port.contains(':') => (host, Some(port)),
            _ => (host_port, None),
        }
    };
    if host.is_empty() {
        return Err(invalid("empty host"));
    }
    let port = match port {
        Some(raw) => Some(
            raw.parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| invalid("bad port"))?,
        ),
        None => None,
    };
    Ok(JumpSpec {
        user,
        host: host.to_string(),
        port,
    })
}

/// 解析一个跳板。跳板没有连接配置可依,全部取自 ProxyJump 写法与它自己的 ssh config;
/// 认证只用 agent / 默认私钥 / 它自己的 `IdentityFile`(密码属于目标连接,不用于跳板)。
fn resolve_jump(raw: &str, env: &RouteEnv<'_>) -> Result<JumpHop, RouteError> {
    let spec = parse_jump_spec(raw)?;
    let config = read_host_config(
        env.ssh_config_path,
        env.home,
        &spec.host,
        env.tolerate_unreadable,
    )?;
    if config.proxy.as_ref().is_some_and(|proxy| !proxy.is_none()) {
        return Ok(JumpHop::NestedProxy(spec.host));
    }
    let host_name = match &config.host_name {
        Some(value) => expand_host_name(value, &spec.host)?,
        None => spec.host.clone(),
    };
    let port = spec.port.or(config.port).unwrap_or(22);
    let user = match spec
        .user
        .clone()
        .or_else(|| config.user.clone())
        .or_else(|| env.local_user.map(str::to_string))
    {
        Some(user) => user,
        None => {
            return Err(RouteError::Invalid(format!(
                "no user name for jump host {}: ProxyJump and ssh config set none and the local \
                 user name is unknown",
                spec.host
            )));
        }
    };
    let ctx = TokenContext {
        home: env.home,
        local_user: env.local_user,
        host_name: &host_name,
        remote_user: &user,
        port,
        alias: &spec.host,
    };
    Ok(JumpHop::Host(ResolvedHost {
        identities: config_identities(&config.identity_files, env, &ctx),
        explicit_identities: !config.identity_files.is_empty(),
        identities_only: config.identities_only.unwrap_or(false),
        known_hosts_name: known_hosts_name(config.host_key_alias.as_deref(), &host_name, port),
        alias: spec.host,
        host_name,
        port,
        user,
    }))
}

// ============================================================================
// tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试一个独立临时目录充当 home(`<dir>/.ssh/config`),绝不碰真实 `~/.ssh`。
    struct TempHome {
        dir: PathBuf,
    }

    impl TempHome {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("mt-ssh-config-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".ssh")).unwrap();
            Self { dir }
        }

        fn write(&self, relative: &str, content: &str) -> PathBuf {
            let path = self.dir.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            restrict(&path);
            path
        }

        fn config(&self, content: &str) -> PathBuf {
            self.write(".ssh/config", content)
        }

        fn read(&self, alias: &str) -> Result<HostConfig, String> {
            read_host_config(
                Some(&self.dir.join(".ssh").join("config")),
                Some(&self.dir),
                alias,
                false,
            )
            .map_err(|e| e.to_string())
        }

        fn env(&self) -> RouteEnv<'_> {
            RouteEnv {
                ssh_config_path: None,
                home: Some(&self.dir),
                local_user: Some("localme"),
                tolerate_unreadable: false,
            }
        }
    }

    /// 测试写出的配置文件收紧成 0600:umask 偏松(002)的机器上,组可写的文件会被权限检查
    /// 忽略掉。
    #[cfg(unix)]
    fn restrict(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(not(unix))]
    fn restrict(_path: &Path) {}

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn connection(host: &str, port: u16, user: &str) -> SshConnection {
        SshConnection {
            id: "c1".into(),
            name: "server".into(),
            host: host.into(),
            port,
            user: user.into(),
            password: None,
            identity_file: None,
            group: None,
            extra: Default::default(),
        }
    }

    fn route(home: &TempHome, conn: &SshConnection) -> Result<ResolvedRoute, String> {
        let config = home.dir.join(".ssh").join("config");
        let env = RouteEnv {
            ssh_config_path: Some(&config),
            ..home.env()
        };
        resolve_route(conn, &env).map_err(|e| e.to_string())
    }

    #[test]
    fn match_pattern_wildcards() {
        assert!(match_pattern("", "*"));
        assert!(match_pattern("anything", "*"));
        assert!(match_pattern("abc", "a?c"));
        assert!(!match_pattern("ac", "a?c"));
        assert!(match_pattern("h.example.com", "*.example.com"));
        assert!(!match_pattern("example.com", "*.example.com"));
        assert!(match_pattern("a.b.c", "a*c"));
        assert!(match_pattern(
            "[h.example.com]:2222",
            "[*.example.com]:2222"
        ));
        assert!(
            !match_pattern("Host", "host"),
            "match_pattern 本身区分大小写"
        );
        assert!(match_pattern("aaab", "*a*b"));
        assert!(!match_pattern("aaa", "*a*b"));
    }

    #[test]
    fn split_args_handles_quotes_escapes_and_comments() {
        assert_eq!(split_args("a b\tc"), ["a", "b", "c"]);
        assert_eq!(
            split_args(r#""C:\Program Files\key" second"#),
            [r"C:\Program Files\key", "second"]
        );
        assert_eq!(split_args("'single quoted' x"), ["single quoted", "x"]);
        assert_eq!(split_args(r"path\ with\ space"), ["path with space"]);
        assert_eq!(split_args(r#"a\"b c\\d"#), [r#"a"b"#, r"c\d"]);
        // Windows 路径里的普通反斜杠原样保留。
        assert_eq!(
            split_args(r"C:\Users\me\.ssh\id_rsa"),
            [r"C:\Users\me\.ssh\id_rsa"]
        );
        // 参数开头的 `#` 起注释,参数中间的 `#` 是普通字符。
        assert_eq!(split_args("value # trailing comment"), ["value"]);
        assert_eq!(split_args("a#b"), ["a#b"]);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn split_line_accepts_both_separator_styles() {
        assert_eq!(
            split_line("  HostName example.com"),
            Some(("hostname".to_string(), vec!["example.com".to_string()]))
        );
        assert_eq!(
            split_line("Port=2222"),
            Some(("port".to_string(), vec!["2222".to_string()]))
        );
        assert_eq!(
            split_line("PORT = 2222"),
            Some(("port".to_string(), vec!["2222".to_string()]))
        );
        assert_eq!(split_line("   # comment"), None);
        assert_eq!(split_line(""), None);
    }

    #[test]
    fn host_blocks_match_with_wildcards_and_negation() {
        let home = TempHome::new("host-blocks");
        home.config(
            "Host web? !web9 db.example.com\n\
             \tUser webuser\n\
             Host *.internal\n\
             \tUser internal\n\
             Host *\n\
             \tUser fallback\n",
        );
        assert_eq!(home.read("web1").unwrap().user.as_deref(), Some("webuser"));
        assert_eq!(
            home.read("db.example.com").unwrap().user.as_deref(),
            Some("webuser")
        );
        // `!web9` 否定命中:整块不适用,落到后面的 `Host *`。
        assert_eq!(home.read("web9").unwrap().user.as_deref(), Some("fallback"));
        assert_eq!(
            home.read("x.internal").unwrap().user.as_deref(),
            Some("internal")
        );
        // Host 模式与 OpenSSH 一样区分大小写。
        assert_eq!(home.read("WEB1").unwrap().user.as_deref(), Some("fallback"));
    }

    #[test]
    fn first_value_wins_and_identity_files_accumulate() {
        let home = TempHome::new("first-wins");
        home.config(
            "port 1111\n\
             Host target\n\
             \tHostName first.example.com\n\
             \tHostName second.example.com\n\
             \tIdentityFile ~/.ssh/one\n\
             \tPort 2222\n\
             Host *\n\
             \tIdentityFile ~/.ssh/two\n\
             \tUser everyone\n\
             \tIdentitiesOnly YES\n\
             \tHostKeyAlias alias-key\n",
        );
        let config = home.read("target").unwrap();
        assert_eq!(config.host_name.as_deref(), Some("first.example.com"));
        // 全局段在前:那条 `port 1111` 先生效,后面的 `Port 2222` 被忽略。
        assert_eq!(config.port, Some(1111));
        assert_eq!(config.user.as_deref(), Some("everyone"));
        assert_eq!(config.identity_files, ["~/.ssh/one", "~/.ssh/two"]);
        assert_eq!(config.identities_only, Some(true));
        assert_eq!(config.host_key_alias.as_deref(), Some("alias-key"));
    }

    #[test]
    fn match_blocks_are_skipped_except_match_all() {
        let home = TempHome::new("match-blocks");
        home.config(
            "Match host target exec \"true\"\n\
             \tUser from-match\n\
             Host target\n\
             \tHostName t.example.com\n\
             Match all\n\
             \tUser from-match-all\n",
        );
        let config = home.read("target").unwrap();
        assert_eq!(config.user.as_deref(), Some("from-match-all"));
        assert_eq!(config.host_name.as_deref(), Some("t.example.com"));
    }

    #[test]
    fn include_reads_relative_glob_and_conditional_files() {
        let home = TempHome::new("include");
        home.write(".ssh/conf.d/10-a.conf", "Host a\n\tUser user-a\n");
        home.write(".ssh/conf.d/20-b.conf", "Host b\n\tUser user-b\n");
        home.write(".ssh/conf.d/.hidden.conf", "Host a\n\tUser hidden\n");
        home.write(".ssh/only-c", "User user-c\n");
        home.config(
            "Include conf.d/*.conf\n\
             Host c\n\
             \tInclude only-c\n\
             Host d\n\
             \tInclude ~/.ssh/only-c\n",
        );
        assert_eq!(home.read("a").unwrap().user.as_deref(), Some("user-a"));
        assert_eq!(home.read("b").unwrap().user.as_deref(), Some("user-b"));
        // Host 块里的 Include 只在该块生效时读。
        assert_eq!(home.read("c").unwrap().user.as_deref(), Some("user-c"));
        assert_eq!(home.read("d").unwrap().user.as_deref(), Some("user-c"));
        assert_eq!(home.read("e").unwrap().user, None);
    }

    #[test]
    fn include_cycles_are_skipped_and_depth_is_limited() {
        let home = TempHome::new("include-cycle");
        home.write(".ssh/loop", "Include loop\nUser looped\n");
        home.config("Include loop\n");
        // 自己包含自己:跳过递归那次,其余照常读。
        assert_eq!(home.read("x").unwrap().user.as_deref(), Some("looped"));

        // 17 层不同文件的链:超过 OpenSSH 的 16 层上限报错。
        let deep = TempHome::new("include-depth");
        for i in 0..20 {
            deep.write(&format!(".ssh/f{i}"), &format!("Include f{}\n", i + 1));
        }
        deep.config("Include f0\n");
        let err = deep.read("x").unwrap_err();
        assert!(err.contains("Include nested more than 16"), "{err}");
    }

    #[test]
    fn invalid_values_on_active_lines_are_errors() {
        let home = TempHome::new("invalid");
        home.config("Host bad\n\tPort abc\nHost other\n\tPort 0\n");
        let err = home.read("bad").unwrap_err();
        assert!(err.contains("invalid Port value 'abc'"), "{err}");
        assert!(err.contains(":2"), "带上文件与行号: {err}");
        // 不生效的行不校验。
        assert!(home.read("fine").is_ok());
        assert!(home.read("other").is_err());
    }

    #[test]
    fn missing_or_absent_config_is_empty() {
        let home = TempHome::new("absent");
        assert_eq!(home.read("x").unwrap(), HostConfig::default());
        assert_eq!(
            read_host_config(None, Some(&home.dir), "x", false).unwrap(),
            HostConfig::default()
        );
    }

    #[test]
    fn unreadable_config_is_reported_unless_tolerated() {
        // 路径是个目录:打不开 / 读不出,但不是「不存在」—— 不能当成没有配置。
        let home = TempHome::new("unreadable");
        let config = home.dir.join(".ssh").join("config");
        std::fs::create_dir_all(&config).unwrap();
        restrict(&config);
        let err = read_host_config(Some(&config), Some(&home.dir), "x", false).unwrap_err();
        assert!(matches!(err, RouteError::Unreadable(_)), "{err:?}");
        assert!(err.message().starts_with("ssh config "), "{err}");
        assert!(err.message().contains("is unreadable"), "{err}");
        // 没有上次的路线可沿用时才容忍:记日志后当它不存在。
        assert_eq!(
            read_host_config(Some(&config), Some(&home.dir), "x", true).unwrap(),
            HostConfig::default()
        );

        // Include 进来的文件读不了同样如此。
        let other = TempHome::new("unreadable-include");
        let broken = other.dir.join(".ssh").join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        restrict(&broken);
        other.config("Include broken\nUser after-include\n");
        let err = other.read("x").unwrap_err();
        assert!(err.contains("is unreadable"), "{err}");
    }

    #[test]
    fn group_or_world_writable_modes_match_openssh() {
        for mode in [0o100600, 0o100644, 0o100755, 0o100400] {
            assert!(!writable_by_others(mode), "{mode:o}");
        }
        for mode in [0o100664, 0o100660, 0o100646, 0o100666, 0o100602] {
            assert!(writable_by_others(mode), "{mode:o}");
        }
    }

    /// Unix:组可写的配置文件整个忽略(OpenSSH「Bad owner or permissions」)。
    #[cfg(unix)]
    #[test]
    fn group_writable_config_is_ignored_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let home = TempHome::new("bad-perms");
        let path = home.config("Host x\n\tUser from-config\n");
        assert_eq!(home.read("x").unwrap().user.as_deref(), Some("from-config"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert_eq!(home.read("x").unwrap(), HostConfig::default());
    }

    #[test]
    fn crlf_and_bom_are_tolerated() {
        let home = TempHome::new("crlf");
        home.config("\u{feff}Host x\r\n\tUser crlf-user\r\n");
        assert_eq!(home.read("x").unwrap().user.as_deref(), Some("crlf-user"));
    }

    #[test]
    fn expand_tilde_variants() {
        let home = Path::new("/home/me");
        assert_eq!(expand_tilde("~", Some(home)).unwrap(), home);
        assert_eq!(
            expand_tilde("~/.ssh/id", Some(home)).unwrap(),
            home.join(".ssh/id")
        );
        assert_eq!(
            expand_tilde(r"~\.ssh\id", Some(home)).unwrap(),
            home.join(r".ssh\id")
        );
        assert_eq!(
            expand_tilde("/abs/key", Some(home)).unwrap(),
            PathBuf::from("/abs/key")
        );
        assert!(expand_tilde("~/.ssh/id", None).is_err());
        assert!(expand_tilde("~other/.ssh/id", Some(home)).is_err());
    }

    #[test]
    fn expand_tokens_minimal_subset() {
        let home = PathBuf::from("/home/me");
        let ctx = TokenContext {
            home: Some(&home),
            local_user: Some("me"),
            host_name: "real.example.com",
            remote_user: "deploy",
            port: 2222,
            alias: "alias",
        };
        assert_eq!(
            expand_tokens("%d/keys/%u-%r@%h:%p-%n-100%%", &ctx).unwrap(),
            "/home/me/keys/me-deploy@real.example.com:2222-alias-100%"
        );
        assert!(expand_tokens("%C", &ctx).unwrap_err().contains("%C"));
        assert!(expand_tokens("trailing %", &ctx).is_err());
    }

    #[test]
    fn resolve_route_applies_hostname_port_user_only_where_the_terminal_would() {
        let home = TempHome::new("route-basic");
        home.config(
            "Host myalias\n\
             \tHostName %h.real.example.com\n\
             \tPort 2200\n\
             \tUser cfguser\n\
             \tIdentityFile ~/.ssh/cfg_key\n\
             \tIdentityFile %d/keys/%r-%h-%p\n",
        );
        // 连接 port 22、user 空、没配私钥:三者都取 ssh config。
        let route_a = route(&home, &connection("myalias", 22, "")).unwrap();
        let target = &route_a.target;
        assert_eq!(target.host_name, "myalias.real.example.com");
        assert_eq!(target.port, 2200);
        assert_eq!(target.user, "cfguser");
        assert!(target.explicit_identities);
        let paths: Vec<PathBuf> = target
            .identities
            .iter()
            .map(|spec| spec.path.clone().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                home.dir.join(".ssh/cfg_key"),
                PathBuf::from(format!(
                    "{}/keys/cfguser-myalias.real.example.com-2200",
                    home.dir.display()
                )),
            ]
        );
        let origins: Vec<IdentityOrigin> = target.identities.iter().map(|s| s.origin).collect();
        assert_eq!(
            origins,
            [IdentityOrigin::SshConfig, IdentityOrigin::SshConfig]
        );
        assert_eq!(target.known_hosts_name, "[myalias.real.example.com]:2200");
        assert!(route_a.jumps.is_empty());

        // 连接显式给了非 22 端口与用户、配了私钥:连接的设置优先,私钥只用连接那一把。
        let mut conn = connection("myalias", 2022, "connuser");
        conn.identity_file = Some("~/.ssh/conn_key".into());
        let route_b = route(&home, &conn).unwrap();
        assert_eq!(route_b.target.port, 2022);
        assert_eq!(route_b.target.user, "connuser");
        assert_eq!(route_b.target.identities.len(), 1);
        assert_eq!(
            route_b.target.identities[0].path.clone().unwrap(),
            home.dir.join(".ssh/conn_key")
        );
        assert_eq!(
            route_b.target.identities[0].origin,
            IdentityOrigin::Connection
        );

        // 没有 ssh config 条目:原样,user 空时落到本机用户名;没有显式私钥。
        let route_c = route(&home, &connection("plain.example.com", 0, "")).unwrap();
        assert_eq!(route_c.target.host_name, "plain.example.com");
        assert_eq!(route_c.target.port, 22);
        assert_eq!(route_c.target.user, "localme");
        assert!(!route_c.target.explicit_identities);
        assert!(route_c.target.identities.is_empty());
        assert_eq!(route_c.target.known_hosts_name, "plain.example.com");
    }

    #[test]
    fn resolve_route_host_key_alias_and_identities_only() {
        let home = TempHome::new("route-alias");
        home.config(
            "Host aliased\n\
             \tHostName 10.0.0.5\n\
             \tHostKeyAlias Prod-Box\n\
             \tIdentitiesOnly yes\n\
             \tIdentityFile none\n",
        );
        let target = route(&home, &connection("aliased", 2222, "u"))
            .unwrap()
            .target;
        // HostKeyAlias 原样(转小写),不带端口。
        assert_eq!(target.known_hosts_name, "prod-box");
        assert!(target.identities_only);
        // `IdentityFile none`:不产生条目,但也不再补默认私钥。
        assert!(target.identities.is_empty());
        assert!(target.explicit_identities);
    }

    #[test]
    fn resolve_route_proxy_jump_single_multi_and_none() {
        let home = TempHome::new("route-jump");
        home.config(
            "Host target\n\
             \tHostName 10.0.0.9\n\
             \tProxyJump jumper@bastion:2201\n\
             Host multi\n\
             \tProxyJump ssh://first.example.com,[fd00::1]:2202\n\
             Host direct\n\
             \tProxyJump none\n\
             \tProxyCommand ssh -W %h:%p nowhere\n\
             Host bastion\n\
             \tHostName bastion.real.example.com\n\
             \tUser ignored-because-spec-has-user\n\
             \tIdentityFile ~/.ssh/bastion_key\n\
             Host first.example.com\n\
             \tUser firstuser\n\
             \tPort 2000\n",
        );
        let single = route(&home, &connection("target", 22, "deploy")).unwrap();
        assert_eq!(single.jumps.len(), 1);
        let jump = &single.jumps[0];
        assert_eq!(jump.alias, "bastion");
        assert_eq!(jump.host_name, "bastion.real.example.com");
        assert_eq!(jump.port, 2201, "ProxyJump 写的端口优先于跳板的 ssh config");
        assert_eq!(jump.user, "jumper");
        assert_eq!(
            jump.identities[0].path.clone().unwrap(),
            home.dir.join(".ssh/bastion_key")
        );
        assert_eq!(jump.known_hosts_name, "[bastion.real.example.com]:2201");
        assert_eq!(single.target.host_name, "10.0.0.9");

        let multi = route(&home, &connection("multi", 22, "deploy")).unwrap();
        assert_eq!(multi.jumps.len(), 2);
        assert_eq!(multi.jumps[0].host_name, "first.example.com");
        assert_eq!(multi.jumps[0].port, 2000);
        assert_eq!(multi.jumps[0].user, "firstuser");
        assert_eq!(multi.jumps[1].host_name, "fd00::1");
        assert_eq!(multi.jumps[1].port, 2202);
        assert_eq!(multi.jumps[1].user, "localme", "都没写时用本机用户名");
        assert_eq!(multi.jumps[1].known_hosts_name, "[fd00::1]:2202");

        // `ProxyJump none` 先出现:不用跳板,后面的 ProxyCommand 也不再生效。
        let direct = route(&home, &connection("direct", 22, "u")).unwrap();
        assert!(direct.jumps.is_empty());
    }

    #[test]
    fn resolve_route_falls_back_to_direct_for_proxy_command_and_nested_jumps() {
        let home = TempHome::new("route-fallback");
        home.config(
            "Host viacmd\n\
             \tHostName cmd.example.com\n\
             \tProxyCommand ssh -W %h:%p gateway\n\
             Host nested\n\
             \tProxyJump ok-hop,hop1\n\
             Host hop1\n\
             \tProxyJump hop2\n",
        );
        // ProxyCommand:不经代理直连(HostName 等其余设置照常生效),留一句说明。
        let viacmd = route(&home, &connection("viacmd", 22, "u")).unwrap();
        assert!(viacmd.jumps.is_empty());
        assert_eq!(viacmd.target.host_name, "cmd.example.com");
        let note = viacmd.ignored_proxy.expect("note");
        assert!(
            note.starts_with("ignored \"ProxyCommand ssh -W %h:%p gateway\" from ssh config"),
            "{note}"
        );
        assert!(note.contains("connected directly"), "{note}");
        assert!(
            note.contains("已忽略 ~/.ssh/config 里的 ProxyCommand"),
            "{note}"
        );

        // 跳板自己又配了代理:整条路线回落为直连(不是只丢掉那一跳)。
        let nested = route(&home, &connection("nested", 22, "u")).unwrap();
        assert!(nested.jumps.is_empty());
        let note = nested.ignored_proxy.expect("note");
        assert!(note.contains("\"ProxyJump ok-hop,hop1\""), "{note}");
        assert!(note.contains("jump host hop1 sets its own"), "{note}");

        // 没配代理的连接没有说明。
        let plain = route(&home, &connection("plain", 22, "u")).unwrap();
        assert_eq!(plain.ignored_proxy, None);
    }

    #[test]
    fn parse_jump_spec_forms() {
        assert_eq!(
            parse_jump_spec("host").unwrap(),
            JumpSpec {
                user: None,
                host: "host".into(),
                port: None
            }
        );
        assert_eq!(
            parse_jump_spec("me@host:2222").unwrap(),
            JumpSpec {
                user: Some("me".into()),
                host: "host".into(),
                port: Some(2222)
            }
        );
        assert_eq!(
            parse_jump_spec("ssh://a@b@[::1]:22/").unwrap(),
            JumpSpec {
                user: Some("a@b".into()),
                host: "::1".into(),
                port: Some(22)
            }
        );
        assert_eq!(parse_jump_spec("fe80::1").unwrap().host, "fe80::1");
        assert!(parse_jump_spec("host:0").is_err());
        assert!(parse_jump_spec("host:abc").is_err());
        assert!(parse_jump_spec("@host").is_err());
        assert!(parse_jump_spec("").is_err());
        assert!(parse_jump_spec("[::1").is_err());
    }

    #[test]
    fn resolve_route_without_config_or_home_still_works() {
        let env = RouteEnv {
            ssh_config_path: None,
            home: None,
            local_user: None,
            tolerate_unreadable: false,
        };
        let mut conn = connection("h.example.com", 22, "u");
        conn.identity_file = Some("~/.ssh/key".into());
        let route = resolve_route(&conn, &env).unwrap();
        // home 未知:`~` 展开失败留到认证阶段作为该私钥的失败原因,路线本身照常。
        assert!(route.target.identities[0].path.is_err());
        // user 为空且没有本机用户名:明确报错(配置类错误,不是「读不了」)。
        let err = resolve_route(&connection("h.example.com", 22, ""), &env).unwrap_err();
        assert!(matches!(err, RouteError::Invalid(_)), "{err:?}");
        assert!(err.message().starts_with("no user name"), "{err}");
    }
}
