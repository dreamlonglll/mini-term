//! ssh_service —— SSH 工具的共享业务编排层。
//!
//! 自 `mt-ssh-mcp.rs` 整体迁入（ssh-cli-skill spec §3）：连接查找、池 acquire、
//! transport 错精确淘汰 + 单次 retry、重试仍失败后的 30s 传输层冷却（与认证无关，认证
//! 失败的冷却在池里）、审计日志、mini-term 数据目录传输硬护栏，全部收敛在这里单点执行。
//! MCP handler 与 CLI/daemon 都是薄传输层。
//!
//! exec 的输出走 `on_output(StreamKind, &[u8])` 流式回调：daemon 侧写 IPC 帧
//! 实时转发，MCP 侧收集进缓冲再 cap_output 打包 JSON —— 两端行为各自保持不变。
//!
//! 时限口径(0.4.11 起):`--timeout` 是**整个请求**的时限,从请求进门算起 —— 建连 /
//! 握手 / 认证、等会话锁、开 channel、跑命令都在里面;取消令牌同样能打断建连。会话锁只在
//! 开 channel 那一下持有,同一连接上的多个 exec 并发跑,不再全局串行。
//!
//! 两条纪律（与 MCP 时代一致）：
//! - 错误信息**绝不含密码**（只回显用户给的标识符 / 透传库错误文本）；
//! - 连接视图**绝不含 password / identityFile**。

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mt_ssh::pool::{
    run_sftp_download_on_session, run_sftp_upload_until, CachedSession, SftpTransferError, SshPool,
};
use mt_ssh::russh::{Channel, ChannelMsg, Sig};
use serde::Serialize;

/// MCP 侧输出封顶:stdout / stderr 各自最多保留约 100 KB,超出截断并标记。
/// CLI/daemon 侧不封顶（流式透传），本常量仅供 MCP 适配层取用。
pub const OUTPUT_CAP_BYTES: usize = 100 * 1024;

/// exec 的默认超时秒数。
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// upload / download 的默认超时秒数。比 exec 宽松——文件传输天然更慢,
/// 大文件需要更长窗口。
pub const DEFAULT_TRANSFER_TIMEOUT_SECS: u64 = 300;

/// 审计日志文件名(与 `config.json` 同目录)。沿用 MCP 时代的文件与行格式。
const AUDIT_LOG_FILE: &str = "ssh-mcp-audit.log";

/// 同一连接上一次请求**两次**都栽在传输层(换新 session 重试仍失败)后的冷却时长:
/// 期间新请求直接返错,不再去打远端。与认证无关 —— 认证失败在 acquire 那一步就返错了。
const UNHEALTHY_COOLDOWN: Duration = Duration::from_secs(30);

/// 开 SSH channel 的短超时(取 `min(本值, 剩余时限)`)。
///
/// 开 channel 只是一来一回,健康 session 上是毫秒级;10s 还开不出来,这条 session 多半
/// 已经死了(网络切换后 TCP 半开、服务器挂起……)—— 精确淘汰它、在剩余时限内换一条
/// 新 session 重试一次,比把整个请求的时限耗死在一条死连接上强。只计 `channel_open`
/// 本身,不计等会话锁(那是别的请求在用这条 session,不说明它死了)。
const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// channel 收尾(close)的宽限。close 只是往 session 队列里塞一条消息,正常瞬间完成;
/// 队列堵住时不能让它把请求拖过时限(与 mt-ssh `run_bounded_exec_on_session` 同口径)。
const CHANNEL_CLOSE_GRACE: Duration = Duration::from_millis(500);

/// 远端报告的终止信号认不出来时的退出码。常见信号映射成 `128 + 信号号`(见
/// [`signal_number`]);认不出的统一 255 —— 落在 128+n 的常见区间之外,与 OpenSSH 客户端
/// 「远端没给退出码」时的口径相同。stderr 另有一行说明信号名。
pub const EXIT_UNKNOWN_SIGNAL: i32 = 255;

/// mini-term 装机版数据目录的目录名(`mt_core::config_json_path()` 的父目录名,
/// 即 mt-config 的 `APP_IDENTIFIER`)。传输护栏按名字纵深拦截时用。
const INSTALLED_DATA_DIR_NAME: &str = "com.mini-term.app";

// ---------------------------------------------------------------------------
// 连接视图 / 列表
// ---------------------------------------------------------------------------

/// 暴露给 agent 的 SSH 连接视图。
///
/// 安全要点:**绝不包含 `password` / `identityFile` 等敏感字段**——
/// `mt_core::SshConnection` 含明文密码,绝不能直接序列化给 agent。
/// 这里只挑选展示用的非敏感字段。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConnectionView {
    /// 连接稳定 id,后续 exec 用它指定连接。
    pub id: String,
    /// 连接展示名。
    pub name: String,
    /// 远程主机地址。
    pub host: String,
    /// 远程 SSH 端口。
    pub port: u16,
    /// 登录用户名。
    pub user: String,
    /// 连接所属分组(可选)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// 把连接列表投影成对 agent 可见的视图。
///
/// 安全核心:映射到不含 `password` / `identityFile` 的 `SshConnectionView`,
/// 绝不把明文密码序列化给 agent。传入的连接列表已由
/// `read_ssh_connections_for_project` 按项目关联范围过滤。抽成纯函数便于单测。
pub fn connection_views(conns: Vec<mt_core::SshConnection>) -> Vec<SshConnectionView> {
    conns
        .into_iter()
        .map(|c| SshConnectionView {
            id: c.id,
            name: c.name,
            host: c.host,
            port: c.port,
            user: c.user,
            group: c.group,
        })
        .collect()
}

/// 调用方的连接范围来源。MCP 保留存量 project-id 语义；CLI/daemon 必须使用
/// 随机能力令牌，未知令牌 fail closed。
#[derive(Debug, Clone, Copy)]
pub enum ConnectionScope<'a> {
    LegacyProject(Option<&'a str>),
    Capability(&'a str),
}

impl ConnectionScope<'_> {
    /// 「连接没找到」时告诉调用方去哪看可用连接 —— 两条入口的工具不同。
    fn list_hint(&self) -> &'static str {
        match self {
            ConnectionScope::LegacyProject(_) => {
                "Call ssh_list_connections to see available connections."
            }
            ConnectionScope::Capability(_) => {
                "Run `mt-ssh-cli list` with the same --project-token to see the connections \
                 linked to this project."
            }
        }
    }
}

/// 按调用来源读取连接，并把能力令牌错误归一为可读的参数错误。
///
/// 令牌路径的失败按类别报(配置不存在 / 读不了 / 令牌对不上 / 范围为空……,见
/// [`mt_core::TokenScopeError`]),任何一类都是错误,绝不退回「全部连接」。
fn read_scoped_connections(
    scope: ConnectionScope<'_>,
) -> Result<Vec<mt_core::SshConnection>, ServiceError> {
    match scope {
        ConnectionScope::LegacyProject(project_id) => {
            Ok(mt_core::read_ssh_connections_for_project(project_id))
        }
        ConnectionScope::Capability(project_token) => {
            mt_core::read_ssh_connections_for_token(project_token)
                .map_err(|e| ServiceError::InvalidParams(e.to_string()))
        }
    }
}

/// 列出对指定调用范围可见的 SSH 连接(不含敏感字段)。
///
/// 每次调用重读 config.json —— 保证主程序里改「关联 SSH」范围即时生效。
pub fn list_connections(
    scope: ConnectionScope<'_>,
) -> Result<Vec<SshConnectionView>, ServiceError> {
    read_scoped_connections(scope).map(connection_views)
}

// ---------------------------------------------------------------------------
// 错误分类
// ---------------------------------------------------------------------------

/// service 层错误。MCP 时代的两类映射保留语义，另有 daemon 断连专用取消态:
///
/// - `InvalidParams`:参数/业务错(连接未找到、护栏拒绝、SFTP 业务错)——
///   MCP 侧映射 `invalid_params`,CLI 侧一样是 exit 2;
/// - `Internal`:连接/认证/传输层错(acquire 失败、cooldown、retry 后仍失败、
///   transfer 超时)—— MCP 侧映射 `internal_error`。
///
/// 错误文本**绝不含密码**(与 MCP 同一纪律)。
#[derive(Debug)]
pub enum ServiceError {
    InvalidParams(String),
    Internal(String),
    Cancelled,
}

impl ServiceError {
    /// 取人类可读的错误信息。
    pub fn message(&self) -> &str {
        match self {
            ServiceError::InvalidParams(m) | ServiceError::Internal(m) => m,
            ServiceError::Cancelled => "ssh exec cancelled",
        }
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

// ---------------------------------------------------------------------------
// 纯函数:连接查找 / 命令拼装 / 输出封顶
// ---------------------------------------------------------------------------

/// 在连接列表里按 name 或 id 查找连接。
///
/// 匹配规则(先 name 后 id,均大小写敏感,与 SshModal 行为一致):
/// - name 精确命中多条 → 歧义错误;
/// - name 无命中再按 id 精确命中;
/// - 均无命中 → 未找到错误。
///
/// 传入列表已由 `read_ssh_connections_for_project` 按项目关联范围过滤,
/// 因此本项目无权访问的连接天然「未找到」,无需再单独校验授权。
/// 所有错误信息**不含密码**(只回显用户给的标识符)。`list_hint` 是「未找到」时
/// 附带的下一步指引(MCP 与 CLI 各自的列连接方式,见 `ConnectionScope::list_hint`)。
pub fn find_connection(
    conns: &[mt_core::SshConnection],
    selector: &str,
    list_hint: &str,
) -> Result<mt_core::SshConnection, String> {
    let by_name: Vec<&mt_core::SshConnection> =
        conns.iter().filter(|c| c.name == selector).collect();
    match by_name.len() {
        1 => Ok(by_name[0].clone()),
        n if n > 1 => Err(format!(
            "SSH connection name '{selector}' is ambiguous: {n} connections share this name. \
            Use the connection id instead."
        )),
        // name 无命中 → 退而按 id 精确匹配
        _ => match conns.iter().find(|c| c.id == selector) {
            Some(c) => Ok(c.clone()),
            None => Err(format!(
                "No SSH connection found matching '{selector}'. {list_hint}"
            )),
        },
    }
}

/// 拼远程要执行的命令:`cwd` 非空时前缀 `cd <cwd> && `。
///
/// 目录按 POSIX shell 的规则写成一个词(见 [`quote_remote_dir`]):简单路径原样不加
/// 引号 —— Windows OpenSSH 的 cmd.exe 远端对 `D:/work` 这类路径照样能用;带空格 /
/// 特殊字符的路径加引号,不再被远端 shell 拆词,也不能借 `;` `&&` 夹带别的命令。
pub fn build_remote_command(command: &str, cwd: Option<&str>) -> String {
    match cwd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(dir) => format!("cd {} && {command}", quote_remote_dir(dir)),
        None => command.to_string(),
    }
}

/// 把远端目录写成 POSIX shell 里的**一个词**,能不加引号就不加。
///
/// 1. 开头的 `~` / `~user` 留在引号外,保住 home 展开(`~/my dir` → `~/"my dir"`);
/// 2. 其余部分只含安全字符 → 原样;
/// 3. 能用双引号就用双引号 —— POSIX shell 与 cmd.exe 都认;
/// 4. 含 `$`、反引号、`"`、`!`、换行,或 `\` 后面跟着双引号里的特殊字符(含末尾的
///    `\`,它会吃掉收尾引号)→ 退回 POSIX 单引号,内部 `'` 写成 `'\''`。
///
/// 以 `-` 开头的相对路径补成 `./-x`,免得被 `cd` 当成选项。
fn quote_remote_dir(dir: &str) -> String {
    let (tilde, rest) = split_tilde_prefix(dir);
    let dashed;
    let rest = if tilde.is_empty() && rest.starts_with('-') {
        dashed = format!("./{rest}");
        dashed.as_str()
    } else {
        rest
    };
    if rest.is_empty() || rest.chars().all(is_shell_word_char) {
        format!("{tilde}{rest}")
    } else if double_quote_safe(rest) {
        format!("{tilde}\"{rest}\"")
    } else {
        format!("{tilde}{}", posix_single_quote(rest))
    }
}

/// 拆出开头的波浪号前缀:`~` 或 `~login`,**连同紧跟的第一个 `/`**。
///
/// POSIX 的 tilde-prefix 到第一个**未加引号**的 `/` 为止,前缀里有任何引号就不展开 ——
/// `~"/my dir"` 不会展开,`~/"my dir"` 才会,所以那个 `/` 必须留在引号外。`~` 后面跟的
/// 不是合法登录名字符(如 `~ x`、`~+`)就不算前缀,整段交给引号处理、不做展开。
fn split_tilde_prefix(dir: &str) -> (&str, &str) {
    let Some(after) = dir.strip_prefix('~') else {
        return ("", dir);
    };
    let (login, slash) = match after.find('/') {
        Some(at) => (&after[..at], 1),
        None => (after, 0),
    };
    let login_ok = login
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if login_ok {
        dir.split_at(1 + login.len() + slash)
    } else {
        ("", dir)
    }
}

/// 不加引号也只是一个普通词的字符:POSIX shell 与 cmd.exe 都不会特殊对待。
/// 非 ASCII 的字母数字(中文目录名等)也算。
fn is_shell_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '/' | '.' | '_' | '-' | '+' | ':' | '@')
        || (!c.is_ascii() && c.is_alphanumeric())
}

/// 这段文本放进 POSIX 双引号里是否仍按字面解释。
fn double_quote_safe(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars.iter().enumerate().all(|(i, c)| match c {
        '$' | '`' | '"' | '!' | '\n' | '\r' => false,
        '\\' => !matches!(chars.get(i + 1), None | Some('$' | '`' | '"' | '\\' | '\n')),
        _ => true,
    })
}

/// POSIX 单引号:除 `'` 外一切按字面,`'` 写成 `'\''`。
fn posix_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// 把一段输出按字节封顶。返回 (截断后的文本, 是否发生截断)。
///
/// 按字节而非字符封顶以严格控制返回体积;在 UTF-8 字符边界处切割,
/// 避免产生非法 UTF-8。仅 MCP 适配层使用(CLI 不封顶)。
pub fn cap_output(s: &str, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s.to_string(), false);
    }
    // 从 cap 处向前回退到一个 UTF-8 字符边界
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[output truncated]");
    (out, true)
}

// ---------------------------------------------------------------------------
// 审计日志
// ---------------------------------------------------------------------------

/// 脱敏掩码。凭据被替换成这三个字符。
const MASK: &str = "***";

/// 已知会用 `-p<密码>` **紧贴**写法的客户端(mysql 家族)。
///
/// 只在命令里出现它们时才认这种形态 —— 见 `redact_command` 的取舍说明。
/// 按 basename 比对(去掉目录与 `.exe`),`sudo mysql -pX` / `/usr/bin/mysql -pX` /
/// `docker exec db mysql -pX` 都能命中。
const GLUED_PASSWORD_CLIENTS: &[&str] = &[
    "mysql",
    "mysqladmin",
    "mysqldump",
    "mysqlshow",
    "mysqlimport",
    "mariadb",
    "mariadb-admin",
    "mariadb-dump",
    "mariadb-show",
    "mariadb-import",
];

/// 值就是凭据本身的长选项(`--opt 值` 与 `--opt=值` 两种写法都认),比较前统一小写、
/// `_` → `-`。以 [`SECRET_LONG_OPTION_SUFFIXES`] 结尾的也算(`--http-password`、
/// `--client-secret`、`--access-token`……)。`--no-*` 开关不算;`--password-file`、
/// `--token-file`、`--password-stdin` 这类值不是凭据的选项按后缀天然不命中。
const SECRET_LONG_OPTIONS: &[&str] = &[
    "--password",
    "--passwd",
    "--pass",
    "--passphrase",
    "--token",
    "--api-key",
    "--apikey",
    "--secret",
    "--client-secret",
    "--secret-key",
    "--oauth2-bearer",
];

/// 见 [`SECRET_LONG_OPTIONS`]。
const SECRET_LONG_OPTION_SUFFIXES: &[&str] = &[
    "-password",
    "-passwd",
    "-secret",
    "-token",
    "-api-key",
    "-apikey",
];

/// 单横线、值就是凭据的选项(Java keytool / jarsigner 的口令)。
const SECRET_SINGLE_DASH_OPTIONS: &[&str] = &[
    "-storepass",
    "-keypass",
    "-srcstorepass",
    "-deststorepass",
    "-srckeypass",
    "-destkeypass",
];

/// openssl 的口令来源选项:值形如 `pass:<口令>` 才是明文(`env:` / `file:` / `stdin` 不是)。
const OPENSSL_PASS_OPTIONS: &[&str] = &["-pass", "-passin", "-passout"];

/// 变量名 / `key=value` 的键里出现这些词(不区分大小写,`.` `-` 视同 `_`)就当凭据:
/// `PGPASSWORD`、`MYSQL_ROOT_PASSWORD`、`AWS_SECRET_ACCESS_KEY`、`GITHUB_TOKEN`、
/// `db.password`……另外 `*_PWD`(`MYSQL_PWD`)也算 —— 但 `PWD` / `OLDPWD` 是工作目录。
const SECRET_NAME_WORDS: &[&str] = &[
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "TOKEN",
    "API_KEY",
    "APIKEY",
    "PRIVATE_KEY",
];

/// 值是「认证方案 + 凭据」的 HTTP 头(小写比较)。
const AUTH_SCHEME_HEADERS: &[&str] = &["authorization", "proxy-authorization"];

/// 值直接就是凭据的 HTTP 头(小写比较)。
const CREDENTIAL_HEADERS: &[&str] = &[
    "x-api-key",
    "api-key",
    "x-auth-token",
    "x-access-token",
    "private-token",
];

/// 常见的 HTTP 认证方案名(小写比较)。
const AUTH_SCHEMES: &[&str] = &[
    "basic",
    "bearer",
    "digest",
    "token",
    "negotiate",
    "ntlm",
    "apikey",
    "aws4-hmac-sha256",
    "hoba",
    "mutual",
];

/// 审计日志脱敏:把命令行里**明确是凭据**的片段换成 `***`。
///
/// 审计日志记的是命令原文,`mysql -pSecret` / `curl -u user:pass` /
/// `https://user:pass@host` 这类写法会把明文凭据落进磁盘上的
/// `ssh-mcp-audit.log`(且该文件与 `config.json` 同目录、长期留存)。
///
/// 覆盖的形态:
///
/// | 形态 | 处理 |
/// |------|------|
/// | `-p<值>` 紧贴(mysql 风格) | → `-p***`,**且仅当命令里出现 mysql 家族客户端** |
/// | `--password` / `--passwd` / `--pass` / `--token` / `--api-key` / `--secret` / `--client-secret` 等(见 [`SECRET_LONG_OPTIONS`]),`--opt 值` 与 `--opt=值` | 值整段 → `***` |
/// | keytool 的 `-storepass 值` 等、openssl 的 `-passin pass:值` | 值 → `***` / `pass:***` |
/// | `sshpass -p 值` / `sshpass -p值` | 值 → `***`(只认 sshpass 自己的选项区) |
/// | 变量赋值 / `key=value`,键含 PASSWORD / PASSWD / SECRET / TOKEN / API_KEY / APIKEY 等 | `键=***` |
/// | HTTP 头 `Authorization: Bearer\|Basic …`(`-H '…'`、`--header=…`)及 `X-Api-Key:` 等 | 凭据段 → `***` |
/// | `-u` / `--user` 的 `用户:密码` | 只掩冒号后半段 → `用户:***` |
/// | URL 的 `scheme://用户:密码@主机` | 只掩密码段 → `scheme://用户:***@主机` |
///
/// 命令按空白切段、不解析引号,所以一个带引号的值可能跨好几段(`--password 'a b'`):
/// 打码从开引号那段起,一直延续到收尾引号那段,引号本身保留。
///
/// **宁漏勿误伤**:审计日志的可用性优先,拿不准的形态一律不动 ——
/// 独立的 `-p 3306`(端口)、`ssh -p 22`、`ps -p 123`、`grep token file`、
/// `ssh://user@host`(无密码段)、`http://h:8080/x`(那个冒号是端口)全部原样保留。
/// 值选项后面紧跟的若是另一个 `-` 开头的选项,说明它没带值(`--password --help`),
/// 那个选项也原样保留。
///
/// 紧贴 `-p` 之所以要挂 mysql 家族这道闸:`tar -pxvf` / `cp -pr` / `mkdir -pv` /
/// `docker run -p8080:80` / `ssh -p2222` 都是同一种「`-p` 后紧贴非空」的形态,
/// 无差别掩会把审计日志掩成废纸,那比漏掩一条 `sudo -u x mysql5 -pX` 更糟。
/// sshpass 的 `-p` 同理只认它自己的选项区:`sshpass -p pw ssh -p 22 host` 里 ssh 的
/// 端口不能被掩。
///
/// 纯函数,不碰 IO —— 便于单测穷举形态。
pub fn redact_command(command: &str) -> String {
    let segments = split_keep_whitespace(command);
    let mut redactor = Redactor {
        glued_p_is_password: segments.iter().any(|s| is_glued_password_client(s)),
        ..Redactor::default()
    };
    let mut out = String::with_capacity(command.len());
    for seg in segments {
        // 空白段原样回填(审计行要尽量贴近用户实际敲下的那行)
        if seg.as_bytes().first().is_some_and(u8::is_ascii_whitespace) {
            out.push_str(seg);
        } else {
            out.push_str(&redactor.word(seg));
        }
    }
    out
}

/// 上一个词声明了「下一个词是什么」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// 凭据值:整段打码。
    Secret,
    /// `用户:密码`:只掩冒号后半段。
    UserPass,
    /// openssl 口令来源:`pass:` 开头才打码。
    OpensslPass,
    /// `Authorization:` 之后:是方案名(Bearer……)就再等一段,否则这段就是凭据。
    AuthScheme,
    /// HTTP 头里的凭据:整段打码(base64url 可能以 `-` 开头,不做「是不是选项」的判断)。
    HeaderCredential,
    /// sshpass 自己选项的参数(`-f 文件` / `-d fd` / `-P 提示串`):原样放过。
    Verbatim,
}

/// [`redact_command`] 逐词推进的状态。
#[derive(Debug, Default)]
struct Redactor {
    expect: Option<Expect>,
    /// 正在打码一个跨多段的引号值,等这个引号收尾。
    open_quote: Option<char>,
    /// 处在 `sshpass` 的选项区(sshpass 之后、被它包裹的命令之前)。
    in_sshpass_options: bool,
    /// 命令里出现了 mysql 家族客户端:紧贴的 `-p<值>` 才按密码处理。
    glued_p_is_password: bool,
}

impl Redactor {
    /// 处理一个非空白段,返回它在审计行里的样子。
    fn word(&mut self, seg: &str) -> String {
        if let Some(quote) = self.open_quote {
            if seg.contains(quote) {
                self.open_quote = None;
                return format!("{MASK}{quote}");
            }
            return MASK.to_string();
        }
        if let Some(expect) = self.expect.take() {
            if let Some(out) = self.expected_value(expect, seg) {
                return out;
            }
        }
        if self.in_sshpass_options {
            if let Some(out) = self.sshpass_option(seg) {
                return out;
            }
        }
        if is_sshpass(seg) {
            self.in_sshpass_options = true;
            return seg.to_string();
        }
        if let Some(expect) = value_option(seg) {
            self.expect = Some(expect);
            return seg.to_string();
        }
        if let Some(out) = self.header(seg) {
            return out;
        }
        self.inline(seg)
    }

    /// 上一个词声明了「下一个词是值」。返回 `None` = 这个词不是值(值选项后面直接跟了
    /// 另一个选项),放弃打码,按普通词处理。
    fn expected_value(&mut self, expect: Expect, seg: &str) -> Option<String> {
        match expect {
            Expect::Verbatim => Some(seg.to_string()),
            Expect::HeaderCredential => Some(self.mask(seg)),
            Expect::AuthScheme => {
                if is_auth_scheme(trim_quotes(seg)) && !ends_with_quote(seg) {
                    self.expect = Some(Expect::HeaderCredential);
                    Some(seg.to_string())
                } else {
                    Some(self.mask(seg))
                }
            }
            _ if seg.starts_with('-') => None,
            Expect::Secret => Some(self.mask(seg)),
            Expect::UserPass => Some(mask_user_value(seg)),
            Expect::OpensslPass => Some(self.mask_openssl_pass(seg)),
        }
    }

    /// sshpass 的选项区:`-p 密码` / `-p密码` 打码,`-f` / `-d` / `-P` 的参数放过,
    /// 遇到第一个不是选项的词(被包裹的命令)就离开选项区。
    fn sshpass_option(&mut self, seg: &str) -> Option<String> {
        match seg {
            "-p" => self.expect = Some(Expect::Secret),
            "-f" | "-d" | "-P" => self.expect = Some(Expect::Verbatim),
            _ if seg.len() > 2 && seg.starts_with("-p") => {
                return Some(format!("-p{}", self.mask(&seg[2..])));
            }
            _ if seg.starts_with('-') => {}
            _ => {
                self.in_sshpass_options = false;
                return None;
            }
        }
        Some(seg.to_string())
    }

    /// HTTP 头的凭据(`-H 'Authorization: Bearer x'`、`--header=X-Api-Key:x`……)。
    /// 命令按空白切段,头名、方案名、凭据常落在相邻的几段里。
    fn header(&mut self, seg: &str) -> Option<String> {
        let body = &seg[header_body_start(seg)..];
        let (name, value) = body.split_once(':')?;
        let name = name.to_ascii_lowercase();
        let schemed = AUTH_SCHEME_HEADERS.contains(&name.as_str());
        if !schemed && !CREDENTIAL_HEADERS.contains(&name.as_str()) {
            return None;
        }
        if value.is_empty() {
            self.expect = Some(if schemed {
                Expect::AuthScheme
            } else {
                Expect::HeaderCredential
            });
            return Some(seg.to_string());
        }
        // 头名与值紧贴在同一段(`Authorization:Bearer`、`X-Api-Key:abc`)
        if schemed && is_auth_scheme(trim_quotes(value)) && !ends_with_quote(value) {
            self.expect = Some(Expect::HeaderCredential);
            return Some(seg.to_string());
        }
        let head = &seg[..seg.len() - value.len()];
        Some(format!("{head}{}", self.mask(value)))
    }

    /// 词内自带值的形态:`--opt=值`、`NAME=值`、紧贴的 `-u用户:密码` / `-p密码`、
    /// URL userinfo。
    fn inline(&mut self, seg: &str) -> String {
        if seg.starts_with("--") {
            if let Some((opt, value)) = seg.split_once('=').filter(|(_, v)| !v.is_empty()) {
                if opt == "--user" {
                    return format!("--user={}", mask_user_value(value));
                }
                if is_secret_long_option(opt) {
                    return format!("{opt}={}", self.mask(value));
                }
                // `--env=NAME=值` / `--from-literal=password=值`:值本身是一个赋值
                if let Some(masked) = self.secret_assignment(value) {
                    return format!("{opt}={masked}");
                }
            }
            return redact_url_userinfo(seg);
        }
        if let Some(masked) = self.secret_assignment(seg) {
            return masked;
        }
        // `-uuser:pass`(curl 允许紧贴)。没有冒号就只是用户名,不动。
        if let Some(value) = seg.strip_prefix("-u") {
            if value.contains(':') {
                return format!("-u{}", mask_user_value(value));
            }
        }
        // `-pSecret`(mysql 风格)。独立的 `-p` 不在此列 —— 它后面跟的通常是端口。
        if self.glued_p_is_password {
            if let Some(value) = seg.strip_prefix("-p").filter(|v| !v.is_empty()) {
                return format!("-p{}", self.mask(value));
            }
        }
        redact_url_userinfo(seg)
    }

    /// `NAME=值` 且 NAME 像凭据名 → `NAME=***`;否则 `None`。
    fn secret_assignment(&mut self, text: &str) -> Option<String> {
        let (name, value) = text.split_once('=')?;
        if value.is_empty() || !is_assignment_name(name) || !is_secret_name(name) {
            return None;
        }
        Some(format!("{name}={}", self.mask(value)))
    }

    /// openssl 的 `pass:<口令>` → `pass:***`;`env:` / `file:` / `stdin` 原样。
    fn mask_openssl_pass(&mut self, value: &str) -> String {
        let quote = value.chars().next().filter(|c| matches!(c, '\'' | '"'));
        let body = quote.map_or(value, |q| &value[q.len_utf8()..]);
        let Some(secret) = body.strip_prefix("pass:").filter(|s| !s.is_empty()) else {
            return value.to_string();
        };
        match quote {
            None => format!("pass:{}", self.mask(secret)),
            Some(q) if secret.contains(q) => format!("{q}pass:{MASK}{q}"),
            Some(q) => {
                self.open_quote = Some(q);
                format!("{q}pass:{MASK}")
            }
        }
    }

    /// 把一个值打码,保留它两头的引号。值以引号开头却没在本段收尾时,记下这个引号:
    /// 后续各段一并打码,直到引号收尾(`--password 'a b'` 里的 `b'` 也得掩掉)。
    fn mask(&mut self, value: &str) -> String {
        match value.chars().next() {
            Some(q @ ('\'' | '"')) => {
                if value[q.len_utf8()..].contains(q) {
                    format!("{q}{MASK}{q}")
                } else {
                    self.open_quote = Some(q);
                    format!("{q}{MASK}")
                }
            }
            // 值本身不带开引号,但可能是外层引号串的最后一段(`-H 'Authorization: Bearer abc'`)
            _ => match value.chars().last() {
                Some(q @ ('\'' | '"')) => format!("{MASK}{q}"),
                _ => MASK.to_string(),
            },
        }
    }
}

/// 「下一个词是值」的独立选项。
fn value_option(seg: &str) -> Option<Expect> {
    if seg == "-u" || seg == "--user" {
        Some(Expect::UserPass)
    } else if is_secret_long_option(seg) || SECRET_SINGLE_DASH_OPTIONS.contains(&seg) {
        Some(Expect::Secret)
    } else if OPENSSL_PASS_OPTIONS.contains(&seg) {
        Some(Expect::OpensslPass)
    } else {
        None
    }
}

/// 长选项名是否以凭据为值(见 [`SECRET_LONG_OPTIONS`])。
fn is_secret_long_option(opt: &str) -> bool {
    let opt = opt.to_ascii_lowercase().replace('_', "-");
    opt.starts_with("--")
        && !opt.starts_with("--no-")
        && (SECRET_LONG_OPTIONS.contains(&opt.as_str())
            || SECRET_LONG_OPTION_SUFFIXES
                .iter()
                .any(|suffix| opt.ends_with(suffix)))
}

/// 像变量名 / 配置键:字母或 `_` 开头,其余是字母数字或 `_` `.` `-`。
fn is_assignment_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// 名字里不带上面那些词、但值就是口令的环境变量(sshpass `-e`、redis-cli)。
const SECRET_ENV_NAMES: &[&str] = &["SSHPASS", "REDISCLI_AUTH"];

/// 键名是否像凭据(见 [`SECRET_NAME_WORDS`] / [`SECRET_ENV_NAMES`])。
fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase().replace(['.', '-'], "_");
    SECRET_NAME_WORDS.iter().any(|word| upper.contains(word))
        || upper.ends_with("_PWD")
        || SECRET_ENV_NAMES.contains(&upper.as_str())
}

/// 词是否是 sshpass(去目录、去 `.exe`、忽略大小写)。
fn is_sshpass(token: &str) -> bool {
    command_basename(token) == "sshpass"
}

/// HTTP 头所在段里「头名」的起点:跳过紧贴的 `-H` / `--header=` 与开头的引号。
fn header_body_start(seg: &str) -> usize {
    let rest = seg
        .strip_prefix("--header=")
        .or_else(|| seg.strip_prefix("-H"))
        .unwrap_or(seg);
    let rest = rest.trim_start_matches(['\'', '"']);
    seg.len() - rest.len()
}

fn is_auth_scheme(word: &str) -> bool {
    AUTH_SCHEMES.contains(&word.to_ascii_lowercase().as_str())
}

fn trim_quotes(word: &str) -> &str {
    word.trim_matches(['\'', '"'])
}

fn ends_with_quote(word: &str) -> bool {
    word.ends_with(['\'', '"'])
}

/// 把字符串切成「空白段 / 非空白段」交替的切片序列,拼回去与原文逐字节相同。
///
/// 不用 `split_whitespace`:那会把连续空格、制表压成一个,审计行就不是原文了。
/// 按字节判空白是安全的 —— ASCII 空白绝不会出现在 UTF-8 多字节序列内部,
/// 切点必然落在字符边界上。
fn split_keep_whitespace(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let is_ws = bytes[i].is_ascii_whitespace();
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() == is_ws {
            j += 1;
        }
        out.push(&s[i..j]);
        i = j;
    }
    out
}

/// token 是否是 mysql 家族客户端(去目录、去 `.exe`、忽略大小写)。
fn is_glued_password_client(token: &str) -> bool {
    GLUED_PASSWORD_CLIENTS.contains(&command_basename(token).as_str())
}

/// 命令词的程序名:去目录、去 `.exe`、小写。
fn command_basename(token: &str) -> String {
    let base = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    match base.strip_suffix(".exe") {
        Some(stem) => stem.to_string(),
        None => base,
    }
}

/// `-u` / `--user` 取到的值:整段是 URL 时(`redis-cli -u redis://:pw@h`)按 URL 规则走,
/// 否则按 `用户:密码` 掩冒号后半段。
///
/// 少了这一岔,`-u redis://:pw@h` 会被当成 `用户=redis` 而把 `//:pw@h` 整段掩掉 ——
/// 密码是没了,主机名也一起没了。
fn mask_user_value(value: &str) -> String {
    if value.contains("://") {
        redact_url_userinfo(value)
    } else {
        mask_after_first_colon(value)
    }
}

/// `用户:密码` → `用户:***`。没有冒号、或冒号后为空则原样返回。
fn mask_after_first_colon(value: &str) -> String {
    match value.split_once(':') {
        Some((user, pass)) if !pass.is_empty() => format!("{user}:{MASK}"),
        _ => value.to_string(),
    }
}

/// 掩掉 token 里所有 URL 的 userinfo 密码段:`scheme://user:pass@host` →
/// `scheme://user:***@host`。
///
/// 边界按 RFC 3986 取:authority 到第一个 `/` `?` `#` 为止,userinfo 到 authority
/// 里**最后一个** `@` 为止,密码是 userinfo 里**第一个** `:` 之后的部分。
/// 没有 `@` 的一律不动 —— `http://host:8080/p` 那个冒号是端口,不是密码。
fn redact_url_userinfo(token: &str) -> String {
    if !token.contains("://") {
        return token.to_string();
    }
    let mut out = String::with_capacity(token.len());
    let mut rest = token;
    while let Some(idx) = rest.find("://") {
        let (head, after) = rest.split_at(idx + 3);
        out.push_str(head);
        let auth_end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let authority = &after[..auth_end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(&mask_after_first_colon(&authority[..at]));
                out.push('@');
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &after[auth_end..];
    }
    out.push_str(rest);
    out
}

/// exec 审计行 `exit=` 字段的取值。只有远程命令**可能已经启动**的执行才记审计行
/// (见 `ExecRun::audit`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditExit {
    /// 远程退出码(远端以信号终止时是映射后的 128+n)。
    Code(i32),
    /// 命令已发出,跑到时限被强制关闭。
    Timeout,
    /// 命令已发出,调用方中途断开、channel 被主动关闭。
    Cancelled,
    /// 流正常结束,但远端没报退出状态。
    Unknown,
}

impl std::fmt::Display for AuditExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuditExit::Code(code) => write!(f, "{code}"),
            AuditExit::Timeout => f.write_str("timeout"),
            AuditExit::Cancelled => f.write_str("cancelled"),
            AuditExit::Unknown => f.write_str("unknown"),
        }
    }
}

/// 格式化一行审计日志。抽出便于单测。
///
/// 形如:`2026-05-18T12:34:56Z\tconn=prod\texit=0\tcmd=ls -la`
/// 命令先过 `redact_command` 脱敏(明文凭据不落盘),再把换行替换成空格,
/// 保证一次执行就是一行。
fn format_audit_line(timestamp: &str, conn_name: &str, command: &str, exit: AuditExit) -> String {
    let one_line_cmd = redact_command(command).replace(['\n', '\r'], " ");
    format!("{timestamp}\tconn={conn_name}\texit={exit}\tcmd={one_line_cmd}\n")
}

/// 极简 UTC 时间戳(无需引入 chrono):基于 UNIX 秒数。
///
/// 仅用于审计日志,精确到秒、UTC。失败回退 "unknown"。
fn utc_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => return "unknown".to_string(),
    };
    // 把 UNIX 秒数拆成 YYYY-MM-DDTHH:MM:SSZ(标准公历换算)。
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // 从 1970-01-01 起按年累加,处理闰年。
    let mut year = 1970i64;
    let mut day_of_era = days as i64;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let year_len = if leap { 366 } else { 365 };
        if day_of_era < year_len {
            break;
        }
        day_of_era -= year_len;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_lens = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    while month < 12 && day_of_era >= month_lens[month] {
        day_of_era -= month_lens[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02}T{hh:02}:{mm:02}:{ss:02}Z",
        month + 1,
        day_of_era + 1,
    )
}

/// 把一行(已格式化的)审计日志追加到 `{config.json 所在目录}/ssh-mcp-audit.log`。
///
/// exec 与 transfer 两类审计共用的落盘尾段;写日志失败绝不影响工具结果 ——
/// 只往 stderr 记一笔。
fn append_audit_line(line: &str) {
    let Some(cfg_path) = mt_core::config_json_path() else {
        eprintln!("[mt-ssh-svc] audit: cannot locate config.json dir, skipping audit log");
        return;
    };
    let Some(dir) = cfg_path.parent() else {
        eprintln!("[mt-ssh-svc] audit: config.json has no parent dir, skipping audit log");
        return;
    };
    let log_path = dir.join(AUDIT_LOG_FILE);
    let write_result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = write_result {
        eprintln!("[mt-ssh-svc] audit: failed to write {AUDIT_LOG_FILE}: {e}");
    }
}

/// exec 审计:格式化一行并落盘。
fn append_audit_log(conn_name: &str, command: &str, exit: AuditExit) {
    append_audit_line(&format_audit_line(
        &utc_timestamp(),
        conn_name,
        command,
        exit,
    ));
}

/// 格式化一行传输审计日志。抽出便于单测。
///
/// 形如:`2026-06-09T12:34:56Z\tdir=upload\tconn=prod\tlocal=/a/b\tremote=/c/d\tresult=1234`
/// 路径里的换行/制表替换成空格,保证一次传输就是一行。`result` 成功时为字节数,失败为 `error`。
fn format_transfer_audit_line(
    timestamp: &str,
    direction: TransferDirection,
    conn_name: &str,
    local_path: &str,
    remote_path: &str,
    result: Option<u64>,
) -> String {
    let sanitize = |s: &str| s.replace(['\n', '\r', '\t'], " ");
    let result_str = match result {
        Some(bytes) => bytes.to_string(),
        None => "error".to_string(),
    };
    format!(
        "{timestamp}\tdir={}\tconn={}\tlocal={}\tremote={}\tresult={result_str}\n",
        direction.as_str(),
        sanitize(conn_name),
        sanitize(local_path),
        sanitize(remote_path),
    )
}

/// 传输审计:格式化一行并落盘。`result = Some(bytes)` 成功、`None` 失败。
fn append_transfer_audit_log(
    direction: TransferDirection,
    conn_name: &str,
    local_path: &str,
    remote_path: &str,
    result: Option<u64>,
) {
    append_audit_line(&format_transfer_audit_line(
        &utc_timestamp(),
        direction,
        conn_name,
        local_path,
        remote_path,
        result,
    ));
}

// ---------------------------------------------------------------------------
// 传输护栏
// ---------------------------------------------------------------------------

/// 安全硬护栏:本地路径是否落在 mini-term 自己的数据目录(含子目录)里。
///
/// 数据目录里没有一样东西该经 SSH 工具流出去,更不该被远端文件覆盖:`config.db`
/// 及其 `.bak` / `-wal` / `-shm`(全部配置,SSH 密码是 mt-secret 信封)、
/// `config.json` 投影与 `.pre-sqlite` 存档(同样带信封)、`credential.key`(解信封
/// 的主密钥,Unix 上就是裸密钥)、`layout.db` / `usage.db` / hook 端口文件 / 审计
/// 日志……agent 一句 upload 能把「信封 + 钥匙」一起带走,一句 download 能覆盖配置
/// 或密钥。所以不再逐个文件点名,**整棵目录树、上传下载两个方向一律拒绝**。其它
/// 本地文件按 PRD Decision 不做沙箱限制(仅审计),这是唯一一条硬拒绝。
///
/// 数据目录的定位见 [`protected_data_dirs`];一个都定位不到则放行(无从比对,
/// 不误伤普通文件)。
pub fn is_blocked_local_path(local_path: &str) -> bool {
    is_blocked_local_path_against(local_path, &protected_data_dirs())
}

/// 需要护住的 mini-term 数据目录。
///
/// - 装机版数据目录:`mt_core::config_json_path()` 的所在目录 —— sidecar 读 SSH
///   投影、懒加载 `credential.key`、写审计日志用的都是这同一个锚点(主程序
///   `mt_config::app_data_dir` 拼出的也是它);
/// - 环境里带着 `MT_APP_DATA_DIR` 时它指的目录一并护住:那是主程序
///   `mt_config::active_data_dir` 的覆盖口径(非空即生效,dev 实例的隔离数据目录,
///   里面同样有自己的 `config.db` 与 `credential.key`)。sidecar 自己不从那里读
///   任何东西,这里只多拦、不改变读取口径。
fn protected_data_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = mt_core::config_json_path()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
        .into_iter()
        .collect();
    if let Some(dev_dir) = std::env::var_os("MT_APP_DATA_DIR").filter(|v| !v.is_empty()) {
        dirs.push(std::path::PathBuf::from(dev_dir));
    }
    dirs
}

/// `is_blocked_local_path` 的可测核心:`local_path` 规范化后是否落在任一 `roots`
/// 之内(含 root 本身)。
///
/// 两边都走 [`resolve_for_guard`]:先把指向本机磁盘的 Windows 别名(`\\?\C:\…`、
/// `\\localhost\C$\…`、`\\127.0.0.1\c$\…`、`\\?\UNC\localhost\c$\…`、WSL 的
/// `\\wsl.localhost\<发行版>\mnt\c\…`……)改写回盘符路径,再 [`resolve_lenient`]
/// (存在的部分 canonicalize,不存在的尾段按字面折叠 —— 下载的目标文件往往还不存在,
/// 不能因此绕过护栏),canonicalize 吐出的别名形态再改写一次;然后按路径**分量**判定
/// 前缀:`com.mini-term.app-backup` 这类同名前缀的兄弟目录不会误伤。Windows 与默认
/// 大小写不敏感的 macOS 上统一小写后比较。
///
/// 纵深一道:路径里任何一个分量(规范化前后都查)就是装机版数据目录名
/// `com.mini-term.app`(大小写不敏感,Windows 上忽略 NTFS 流后缀与结尾的点 / 空格)
/// 一律拦 —— 改写规则覆盖不到的别名(非管理共享、改过 automount 根的 WSL……)也跑不掉。
/// dev 数据目录(`MT_APP_DATA_DIR`)的名字可能很普通,只按前缀拦,不按名字拦。
fn is_blocked_local_path_against(local_path: &str, roots: &[PathBuf]) -> bool {
    let raw = Path::new(local_path);
    if names_installed_data_dir(raw) {
        return true;
    }
    let resolved = resolve_for_guard(raw);
    if names_installed_data_dir(&resolved) {
        return true;
    }
    let local = comparable_path(&resolved);
    roots
        .iter()
        .any(|root| local.starts_with(comparable_path(&resolve_for_guard(root))))
}

/// 护栏比较前的规范化:别名改写 → [`resolve_lenient`] → 别名改写。
fn resolve_for_guard(path: &Path) -> PathBuf {
    local_alias_to_drive(&resolve_lenient(&local_alias_to_drive(path)))
}

/// 路径里是否有一个分量就是装机版数据目录名(见 [`is_blocked_local_path_against`])。
fn names_installed_data_dir(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => normalized_component_name(&name.to_string_lossy())
            .eq_ignore_ascii_case(INSTALLED_DATA_DIR_NAME),
        _ => false,
    })
}

/// 一个路径分量在 Windows 上实际指向的名字:去掉 NTFS 流后缀(`dir::$INDEX_ALLOCATION`
/// 就是 `dir` 本身)与结尾的点 / 空格(Win32 路径解析会把它们剥掉)。
fn normalized_component_name(name: &str) -> &str {
    let name = name.split(':').next().unwrap_or(name);
    name.trim_end_matches(['.', ' '])
}

/// Windows:把指向本机磁盘的别名路径改写成盘符路径(见 [`map_local_alias`]);
/// 其它平台原样返回。
fn local_alias_to_drive(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let computer_name = std::env::var("COMPUTERNAME").ok();
        if let Some(mapped) = map_local_alias(&path.to_string_lossy(), computer_name.as_deref()) {
            return PathBuf::from(mapped);
        }
    }
    path.to_path_buf()
}

/// 纯字符串:Windows 上指向本机磁盘的别名路径 → 盘符路径;不是这类别名返回 `None`。
///
/// - `\\?\C:\x`、`\\.\C:\x`(verbatim / device 前缀)→ `C:\x`;
/// - `\\localhost\C$\x`、`\\127.0.0.1\c$\x`、`\\<本机名>\C$\x`(本机管理共享),
///   含 `\\?\UNC\…` / `\\.\UNC\…` 写法 → `C:\x`;
/// - `\\wsl.localhost\<发行版>\mnt\c\x`、`\\wsl$\<发行版>\mnt\c\x`(WSL 默认 automount
///   根 `/mnt`)→ `C:\x`。
///
/// 正斜杠视同反斜杠。只做字符串改写,不访问磁盘或网络。
fn map_local_alias(path: &str, computer_name: Option<&str>) -> Option<String> {
    let path = path.replace('/', "\\");
    for device_prefix in [r"\\?\", r"\\.\"] {
        if let Some(rest) = path.strip_prefix(device_prefix) {
            if let Some(letter) = rest.get(..1).and_then(drive_letter) {
                if rest[1..].starts_with(':') {
                    return Some(format!("{letter}{}", &rest[1..]));
                }
            }
            let unc = rest
                .get(..4)
                .filter(|head| head.eq_ignore_ascii_case(r"UNC\"))
                .map(|_| &rest[4..])?;
            return map_local_unc(unc, computer_name);
        }
    }
    map_local_unc(path.strip_prefix(r"\\")?, computer_name)
}

/// UNC 去掉开头 `\\` 之后的 `主机\共享\其余` → 盘符路径(规则见 [`map_local_alias`])。
fn map_local_unc(unc: &str, computer_name: Option<&str>) -> Option<String> {
    let mut parts = unc.splitn(3, '\\');
    let host = parts.next()?;
    let share = parts.next()?;
    let rest = parts.next().unwrap_or("");
    if host.eq_ignore_ascii_case("wsl.localhost") || host.eq_ignore_ascii_case("wsl$") {
        let mut segments = rest.splitn(3, '\\');
        if !segments.next()?.eq_ignore_ascii_case("mnt") {
            return None;
        }
        let letter = drive_letter(segments.next()?)?;
        return Some(format!(r"{letter}:\{}", segments.next().unwrap_or("")));
    }
    if is_local_host(host, computer_name) {
        let letter = share.strip_suffix('$').and_then(drive_letter)?;
        return Some(format!(r"{letter}:\{rest}"));
    }
    None
}

/// 单个 ASCII 字母 → 大写盘符。
fn drive_letter(text: &str) -> Option<char> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase()),
        _ => None,
    }
}

/// UNC 主机名是否就是本机:`localhost`、IPv4 回环、IPv6 回环的 UNC 写法,或本机名。
fn is_local_host(host: &str, computer_name: Option<&str>) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("0--1.ipv6-literal.net")
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
        || computer_name.is_some_and(|name| !name.is_empty() && host.eq_ignore_ascii_case(name))
}

/// 把路径尽量解析成真实的绝对路径。
///
/// 相对路径先接到当前目录上;然后从整条路径起逐段缩短,找最长的、能 canonicalize
/// 的前缀 —— 它消解了 `..`、符号链接 / junction 与 8.3 短名;还不存在的尾段按字面
/// 接上(`.` 丢弃、`..` 弹一级)。一段都解析不了(盘符都不存在)时退回纯字面折叠。
fn resolve_lenient(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let components: Vec<Component> = absolute.components().collect();
    for split in (1..=components.len()).rev() {
        let prefix: PathBuf = components[..split].iter().collect();
        if let Ok(mut resolved) = std::fs::canonicalize(&prefix) {
            push_lexically(&mut resolved, &components[split..]);
            return resolved;
        }
    }
    let mut resolved = PathBuf::new();
    push_lexically(&mut resolved, &components);
    resolved
}

/// 按字面把路径分量接到 `base` 上:`.` 丢弃,`..` 弹掉上一段普通目录名(到根就停,
/// 与操作系统对 `/..` 的处理一致)。
fn push_lexically(base: &mut std::path::PathBuf, components: &[std::path::Component]) {
    use std::path::Component;

    for component in components {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(base.components().next_back(), Some(Component::Normal(_))) {
                    base.pop();
                } else if !base.has_root() {
                    // 解析不出当前目录时残留的相对路径:保留前导 `..`
                    base.push(component.as_os_str());
                }
            }
            other => base.push(other.as_os_str()),
        }
    }
}

/// 比较用的路径形态:Windows 与 macOS 的默认文件系统大小写不敏感,统一小写;
/// Windows 上每个分量再按 [`normalized_component_name`] 去掉流后缀与结尾的点 / 空格。
/// 其它平台原样。
fn comparable_path(path: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        let mut out = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => {
                    out.push(normalized_component_name(&name.to_string_lossy()).to_lowercase());
                }
                other => out.push(other.as_os_str().to_string_lossy().to_lowercase()),
            }
        }
        out
    } else if cfg!(target_os = "macos") {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

/// 文件传输方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

impl TransferDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            TransferDirection::Upload => "upload",
            TransferDirection::Download => "download",
        }
    }
}

// ---------------------------------------------------------------------------
// exec —— 流式回调 + 完整编排
// ---------------------------------------------------------------------------

/// 状态型 exec 取消令牌。`cancel()` 后状态永久保持，既唤醒当前 waiter，也保证
/// 尚未进入 SSH channel 阶段的 future 稍后观察到取消。
#[derive(Debug, Clone)]
pub struct ExecCancellation {
    state: tokio::sync::watch::Sender<bool>,
}

impl ExecCancellation {
    pub fn new() -> Self {
        let (state, _) = tokio::sync::watch::channel(false);
        Self { state }
    }

    pub fn cancel(&self) {
        self.state.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.state.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.state.subscribe();
        if *receiver.borrow_and_update() {
            return;
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow_and_update() {
                return;
            }
        }
    }
}

impl Default for ExecCancellation {
    fn default() -> Self {
        Self::new()
    }
}

enum ControlledStep<T> {
    Ready(T),
    TimedOut,
    Cancelled,
}

async fn await_exec_step<F>(
    future: F,
    deadline: tokio::time::Instant,
    cancellation: &ExecCancellation,
) -> ControlledStep<F::Output>
where
    F: std::future::Future,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => ControlledStep::Cancelled,
        _ = tokio::time::sleep_until(deadline) => ControlledStep::TimedOut,
        output = future => ControlledStep::Ready(output),
    }
}

/// exec 输出流的类别,对应远程命令的 stdout / stderr。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

// --- 远端信号 ----------------------------------------------------------------

/// RFC 4254 §6.10 的信号名 → 信号号(Linux 编号,只认常见的那些)。大小写不敏感,
/// 带不带 `SIG` 前缀都认;认不出返回 `None`。
pub fn signal_number(name: &str) -> Option<i32> {
    let upper = name.trim().to_ascii_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    Some(match bare {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ILL" => 4,
        "ABRT" => 6,
        "FPE" => 8,
        "KILL" => 9,
        "USR1" => 10,
        "SEGV" => 11,
        "USR2" => 12,
        "PIPE" => 13,
        "ALRM" => 14,
        "TERM" => 15,
        _ => return None,
    })
}

/// 远端以信号终止了命令(SSH `exit-signal`,RFC 4254 §6.10)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSignal {
    /// 服务端报的信号名。RFC 里不带 `SIG` 前缀(`KILL`、`TERM`),自定义信号形如
    /// `FOO@example.com`。
    pub name: String,
    pub core_dumped: bool,
    /// 服务端附带的说明,可能为空。
    pub error_message: String,
}

impl RemoteSignal {
    fn from_wire(signal: &Sig, core_dumped: bool, error_message: &str) -> Self {
        let name = match signal {
            Sig::ABRT => "ABRT",
            Sig::ALRM => "ALRM",
            Sig::FPE => "FPE",
            Sig::HUP => "HUP",
            Sig::ILL => "ILL",
            Sig::INT => "INT",
            Sig::KILL => "KILL",
            Sig::PIPE => "PIPE",
            Sig::QUIT => "QUIT",
            Sig::SEGV => "SEGV",
            Sig::TERM => "TERM",
            Sig::USR1 => "USR1",
            Sig::Custom(name) => name.as_str(),
        };
        Self {
            name: name.to_string(),
            core_dumped,
            error_message: error_message.to_string(),
        }
    }

    /// 映射后的退出码:常见信号 `128 + 信号号`,认不出的 [`EXIT_UNKNOWN_SIGNAL`]。
    pub fn exit_code(&self) -> i32 {
        signal_number(&self.name).map_or(EXIT_UNKNOWN_SIGNAL, |n| 128 + n)
    }

    /// 给 stderr 的一行说明(不带前缀、不带换行):信号名、core dumped、服务端说明、
    /// 最终的退出码。
    pub fn describe(&self, exit_code: i32) -> String {
        let what = match signal_number(&self.name) {
            Some(_) => {
                let upper = self.name.trim().to_ascii_uppercase();
                let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
                format!("signal SIG{bare}")
            }
            None => format!("unrecognized signal \"{}\"", one_line(&self.name)),
        };
        let core = if self.core_dumped {
            " (core dumped)"
        } else {
            ""
        };
        let message = one_line(&self.error_message);
        let message = if message.is_empty() {
            String::new()
        } else {
            format!(": {message}")
        };
        format!("remote command terminated by {what}{core}{message}; exit code {exit_code}")
    }
}

/// 远端给的文本压成一行、去掉控制字符,最长 200 字符 —— 它会原样进调用方的 stderr。
fn one_line(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}

// --- exec 结果 ---------------------------------------------------------------

/// 一次 exec 编排的最终结果。输出已经通过 `on_output` 回调流出,这里只带元数据。
///
/// 超时时 `exit_code` 恒为 `None`、`timed_out = true`;**回调已流出的部分输出
/// 不会撤回** —— CLI/daemon 侧这正是「超时打印已收到的部分输出」的契约;MCP 侧
/// 适配层按旧行为把缓冲清空(旧实现超时路径拿不到部分输出)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    /// 远程命令退出码。远端以信号终止(且没另报 exit-status)时是映射后的
    /// `128 + 信号号`(见 [`RemoteSignal::exit_code`]);超时、或远端两样都没报时为 None。
    pub exit_code: Option<i32>,
    /// 是否因超时被强制终止。
    pub timed_out: bool,
    /// 超时发生在远程命令启动**之前**(建连 / 握手 / 认证、等会话、开 channel):
    /// 命令没有执行。只在 `timed_out` 时可能为 true。
    pub not_started: bool,
    /// 远端报告命令被信号终止。transport 各自在 stderr 补一行
    /// [`ExecOutcome::signal_note`](带上自己的前缀)。
    pub signal: Option<RemoteSignal>,
}

impl ExecOutcome {
    fn completed(exit_code: Option<i32>, signal: Option<RemoteSignal>) -> Self {
        Self {
            exit_code,
            timed_out: false,
            not_started: false,
            signal,
        }
    }

    fn timed_out(not_started: bool) -> Self {
        Self {
            exit_code: None,
            timed_out: true,
            not_started,
            signal: None,
        }
    }

    /// 远端信号的说明行(不带前缀、不带换行);没有信号返回 `None`。
    pub fn signal_note(&self) -> Option<String> {
        let signal = self.signal.as_ref()?;
        Some(signal.describe(self.exit_code.unwrap_or_else(|| signal.exit_code())))
    }
}

/// 连接阶段超时的说明(不带前缀):整体时限在远程命令启动之前就用完了。
pub fn not_started_timeout_message(timeout_secs: u64, connection: &str) -> String {
    format!(
        "timed out after {timeout_secs}s while connecting to '{connection}'; the remote command \
         was not started"
    )
}

/// 传输层冷却中的报错(见 [`UNHEALTHY_COOLDOWN`])。
fn cooldown_message(conn_name: &str) -> String {
    let secs = UNHEALTHY_COOLDOWN.as_secs();
    format!(
        "SSH connection '{conn_name}' is cooling down: a recent request on it failed twice in a \
         row at the transport level (connection dropped or reset, even after reconnecting), so \
         new attempts are paused for up to {secs}s to avoid hammering the server. Retry in about \
         {secs} seconds."
    )
}

// --- 单条 session 上的 exec ------------------------------------------------------

/// exec 在一条 session 上推进到了哪一步。超时 / 取消时据此判断远端命令有没有可能已经
/// 跑起来、这条 session 的状态还可不可信。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecPhase {
    /// 等这条 session 的锁:另一个请求正在它上面开 channel,或 SFTP 传输占着它。
    WaitingForSession,
    /// channel-open 已发出、还没拿到确认:session 上可能留下孤儿 channel。
    OpeningChannel,
    /// channel 已开、exec 请求还没入队:远端什么都没收到。
    Dispatching,
    /// exec 请求已发出:远端命令可能已在运行。
    Running,
}

impl ExecPhase {
    /// 远端命令是否可能已经启动(决定记不记审计、算不算「连接阶段超时」)。
    pub fn command_may_have_started(self) -> bool {
        self == ExecPhase::Running
    }

    /// session 的状态是否已不可信(开 channel 中途被打断),该精确淘汰。
    fn leaves_session_in_doubt(self) -> bool {
        self == ExecPhase::OpeningChannel
    }
}

/// [`run_exec_on_session`] 的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionExecOutcome {
    /// 收流自然结束。`exit_code` 已按 exit-status 优先、其次信号映射算好。
    Completed {
        exit_code: Option<i32>,
        signal: Option<RemoteSignal>,
    },
    /// 整体时限到点。
    TimedOut(ExecPhase),
    /// 取消令牌触发。
    Cancelled(ExecPhase),
    /// 服务端拒绝了 exec 请求(`want_reply` 换来 Failure),命令没有启动。
    Rejected,
    /// 开 channel 的短超时到点、整体时限还有余量:这条 session 多半已死。
    ChannelOpenTimedOut,
}

/// 收流阶段的状态机:逐条吃 `ChannelMsg`,把输出交给回调,记下退出状态 / 信号,以及
/// 「有没有执行证据」。不碰网络 —— 单测直接喂消息序列。
#[derive(Debug, Default)]
struct ExecStream {
    /// 收到过 Success / 输出 / 退出状态:之后再来的 Failure 不能把它倒回「被拒」。
    started: bool,
    exit_status: Option<u32>,
    signal: Option<RemoteSignal>,
}

/// [`ExecStream::on_message`] 的判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamStep {
    Continue,
    /// 服务端拒绝了 exec 请求,且此前没有任何执行证据。
    Rejected,
}

impl ExecStream {
    fn on_message<F>(&mut self, msg: ChannelMsg, on_output: &mut F) -> StreamStep
    where
        F: FnMut(StreamKind, &[u8]) + ?Sized,
    {
        // RFC 4254 §5.2: ExtendedData.ext == 1 表示 stderr。
        const SSH_EXTENDED_DATA_STDERR: u32 = 1;
        match msg {
            ChannelMsg::Success => self.started = true,
            ChannelMsg::Failure if !self.started => return StreamStep::Rejected,
            ChannelMsg::Data { data } => {
                self.started = true;
                on_output(StreamKind::Stdout, &data);
            }
            ChannelMsg::ExtendedData { data, ext } => {
                self.started = true;
                if ext == SSH_EXTENDED_DATA_STDERR {
                    on_output(StreamKind::Stderr, &data);
                }
            }
            // 不能立刻结束:服务器可能在退出状态之后还有最后一批 Data,再发 Eof / Close。
            // 收流一直走到 wait() 返回 None。
            ChannelMsg::ExitStatus { exit_status } => {
                self.started = true;
                self.exit_status = Some(exit_status);
            }
            ChannelMsg::ExitSignal {
                signal_name,
                core_dumped,
                error_message,
                ..
            } => {
                self.started = true;
                self.signal = Some(RemoteSignal::from_wire(
                    &signal_name,
                    core_dumped,
                    &error_message,
                ));
            }
            // Eof / Close / 窗口调整 / 迟到的 Failure:不影响结局。
            _ => {}
        }
        StreamStep::Continue
    }

    /// 收流结束:exit-status 优先(远端明确报了数字),其次按信号映射。
    fn finish(self) -> SessionExecOutcome {
        let exit_code = self
            .exit_status
            .map(|status| status as i32)
            .or_else(|| self.signal.as_ref().map(RemoteSignal::exit_code));
        SessionExecOutcome::Completed {
            exit_code,
            signal: self.signal,
        }
    }
}

/// 显式关闭 channel(russh 的 `Channel` Drop 不发 Close),等待有上限。
async fn close_channel(channel: &Channel<mt_ssh::russh::client::Msg>) {
    let _ = tokio::time::timeout(CHANNEL_CLOSE_GRACE, channel.close()).await;
}

/// 在已 acquire 到的 session 上开 channel、跑 exec,输出经回调流出。
///
/// - **会话锁只在开 channel 时持有**:拿锁 → `channel_open_session` → 立刻放锁,同一
///   连接上的多个 exec 并发跑(与 mt-ssh `run_bounded_exec_on_session` 同一口径)。
///   等锁算进整体时限,但不算进开 channel 的短超时 —— 锁被占着说明 session 在用,
///   不说明它死了;
/// - 开 channel 的时限是 `min(channel_open_timeout, 剩余时限)`;短超时先到 →
///   [`SessionExecOutcome::ChannelOpenTimedOut`],由编排层淘汰这条 session 重试;
/// - exec 请求发出后立刻发 EOF:CLI 不支持 stdin,读 stdin 的远端命令马上读到 EOF,
///   而不是挂到超时;
/// - 超时与取消在同一个状态机里处理:一旦 channel 已建立,任何提前结束的分支都先显式
///   发送 SSH close(等待有上限),不能依赖 `Channel` 的 Drop。
///
/// `Err(String)` 代表 transport 层失败(channel 开不了 / exec 发不出去),只会发生在
/// 任何输出流出之前,caller 可据此「淘汰 + 重连」重试,不会造成输出重复。
pub async fn run_exec_on_session(
    session: &CachedSession,
    remote_command: &str,
    deadline: tokio::time::Instant,
    channel_open_timeout: Duration,
    cancellation: &ExecCancellation,
    on_output: &mut (impl FnMut(StreamKind, &[u8]) + Send),
) -> Result<SessionExecOutcome, String> {
    session.touch();
    // 1. 拿会话锁 —— 只为开 channel。
    let handle_guard = match await_exec_step(session.lock(), deadline, cancellation).await {
        ControlledStep::Ready(guard) => guard,
        ControlledStep::TimedOut => {
            return Ok(SessionExecOutcome::TimedOut(ExecPhase::WaitingForSession));
        }
        ControlledStep::Cancelled => {
            return Ok(SessionExecOutcome::Cancelled(ExecPhase::WaitingForSession));
        }
    };
    // 2. 开 channel,短超时;拿到结果立刻放锁。
    let open_deadline = deadline.min(tokio::time::Instant::now() + channel_open_timeout);
    let opened = await_exec_step(
        handle_guard.channel_open_session(),
        open_deadline,
        cancellation,
    )
    .await;
    drop(handle_guard);
    let mut channel = match opened {
        ControlledStep::Ready(Ok(channel)) => channel,
        ControlledStep::Ready(Err(e)) => return Err(format!("channel_open_session failed: {e}")),
        ControlledStep::TimedOut if open_deadline < deadline => {
            return Ok(SessionExecOutcome::ChannelOpenTimedOut);
        }
        ControlledStep::TimedOut => {
            return Ok(SessionExecOutcome::TimedOut(ExecPhase::OpeningChannel));
        }
        ControlledStep::Cancelled => {
            return Ok(SessionExecOutcome::Cancelled(ExecPhase::OpeningChannel));
        }
    };

    // 3. 发 exec 请求(want_reply:服务端回 Success / Failure)。
    match await_exec_step(channel.exec(true, remote_command), deadline, cancellation).await {
        ControlledStep::Ready(Ok(())) => {}
        ControlledStep::Ready(Err(e)) => {
            close_channel(&channel).await;
            return Err(format!("channel exec failed: {e}"));
        }
        ControlledStep::TimedOut => {
            close_channel(&channel).await;
            return Ok(SessionExecOutcome::TimedOut(ExecPhase::Dispatching));
        }
        ControlledStep::Cancelled => {
            close_channel(&channel).await;
            return Ok(SessionExecOutcome::Cancelled(ExecPhase::Dispatching));
        }
    }

    // 4. 立刻发 EOF。发不出去说明 session 正在关闭,下面的收流会很快结束,不必单独处理。
    match await_exec_step(channel.eof(), deadline, cancellation).await {
        ControlledStep::Ready(_) => {}
        ControlledStep::TimedOut => {
            close_channel(&channel).await;
            return Ok(SessionExecOutcome::TimedOut(ExecPhase::Running));
        }
        ControlledStep::Cancelled => {
            close_channel(&channel).await;
            return Ok(SessionExecOutcome::Cancelled(ExecPhase::Running));
        }
    }

    // 5. 收流。
    let mut stream = ExecStream::default();
    loop {
        match await_exec_step(channel.wait(), deadline, cancellation).await {
            ControlledStep::Ready(Some(msg)) => {
                if stream.on_message(msg, on_output) == StreamStep::Rejected {
                    close_channel(&channel).await;
                    return Ok(SessionExecOutcome::Rejected);
                }
            }
            ControlledStep::Ready(None) => break,
            ControlledStep::TimedOut => {
                close_channel(&channel).await;
                return Ok(SessionExecOutcome::TimedOut(ExecPhase::Running));
            }
            ControlledStep::Cancelled => {
                close_channel(&channel).await;
                return Ok(SessionExecOutcome::Cancelled(ExecPhase::Running));
            }
        }
    }
    // 服务端多半已经关了 channel,这里幂等 best-effort。
    close_channel(&channel).await;
    Ok(stream.finish())
}

// --- 建连 --------------------------------------------------------------------

/// 建连(池 acquire)这一步的失败分类 —— 调用方据此映射退出码 / 错误类别。
///
/// exec:`DeadlineExpired` → 连接阶段超时(CLI exit 124,远程命令没有启动);
/// `Cancelled` → 取消;`Failed` → 连接错误(CLI exit 2)。transfer 三类都是错误。
#[derive(Debug)]
enum AcquireFailure {
    /// 整体时限在建连途中到点。
    DeadlineExpired,
    /// 取消令牌触发(daemon:客户端已断开)。
    Cancelled,
    /// 池返回的错误(网络 / 握手 / 主机密钥 / 认证……),原文透传、不含密码。
    Failed(String),
}

/// 在整体时限与取消令牌之内向池要一条 session。
///
/// 池的 acquire 包了建连、握手、认证,以及复用前「等这条 session 的锁、确认它还活着」,
/// 这些都算进请求时限。future 中途被丢弃是安全的(连接被掐断,不留任务 / socket)。
async fn acquire_within(
    pool: &SshPool,
    conn: &mt_core::SshConnection,
    deadline: tokio::time::Instant,
    cancellation: &ExecCancellation,
) -> Result<Arc<CachedSession>, AcquireFailure> {
    match await_exec_step(pool.acquire(conn), deadline, cancellation).await {
        ControlledStep::Ready(Ok(session)) => Ok(session),
        ControlledStep::Ready(Err(message)) => Err(AcquireFailure::Failed(message)),
        ControlledStep::TimedOut => Err(AcquireFailure::DeadlineExpired),
        ControlledStep::Cancelled => Err(AcquireFailure::Cancelled),
    }
}

// --- exec 编排 ------------------------------------------------------------------

/// exec 编排的产出:给调用方的结果 + 审计行要不要记、记什么。
struct ExecRun {
    result: Result<ExecOutcome, ServiceError>,
    /// 审计行只记**远程命令可能已经开始执行**的请求:结束(退出码 / unknown)、跑到
    /// 时限(timeout)、跑到一半调用方断开(cancelled)。`None` = 命令根本没离开本机或
    /// 没被远端接受(建连阶段失败 / 超时 / 取消、冷却中、开 channel 或发 exec 失败、
    /// 服务端拒绝),不记 —— 审计日志回答的是「哪些命令在哪台机器上跑过」,记一行
    /// 没跑过的命令只会误导。
    audit: Option<AuditExit>,
}

impl ExecRun {
    fn not_started(result: Result<ExecOutcome, ServiceError>) -> Self {
        Self {
            result,
            audit: None,
        }
    }
}

/// exec 的编排核心(连接已解析好;不读配置、不写审计,便于单测)。
///
/// 1. 在整体时限 / 取消令牌之内 acquire;冷却中直接返错;
/// 2. 在 session 上跑 [`run_exec_on_session`];
/// 3. transport 错或开 channel 短超时 → 精确淘汰这条 session(`evict_if_same`,不误杀并发
///    请求刚建好的新 session)→ 在剩余时限内重新 acquire → 再试一次;第二次 transport
///    错 → 标记冷却 + 返错;
/// 4. 跑到时限:命令已发出 → `timed_out`(不淘汰 —— 单个 channel 超时不代表 session
///    死了);还没发出 → `timed_out + not_started`(连接阶段超时)。
async fn exec_resolved(
    pool: &SshPool,
    conn: &mt_core::SshConnection,
    remote_command: &str,
    deadline: tokio::time::Instant,
    channel_open_timeout: Duration,
    cancellation: &ExecCancellation,
    on_output: &mut (impl FnMut(StreamKind, &[u8]) + Send),
) -> ExecRun {
    let mut retried = false;
    loop {
        let session = match acquire_within(pool, conn, deadline, cancellation).await {
            Ok(session) => session,
            Err(AcquireFailure::DeadlineExpired) => {
                eprintln!(
                    "[mt-ssh-svc] exec deadline reached while connecting to '{}'; the command was \
                     not started",
                    conn.name
                );
                return ExecRun::not_started(Ok(ExecOutcome::timed_out(true)));
            }
            Err(AcquireFailure::Cancelled) => {
                return ExecRun::not_started(Err(ServiceError::Cancelled));
            }
            Err(AcquireFailure::Failed(message)) => {
                let message = if retried {
                    format!("reconnect failed: {message}")
                } else {
                    message
                };
                return ExecRun::not_started(Err(ServiceError::Internal(message)));
            }
        };
        if session.is_unhealthy_now() {
            return ExecRun::not_started(Err(ServiceError::Internal(cooldown_message(&conn.name))));
        }

        let attempt = run_exec_on_session(
            &session,
            remote_command,
            deadline,
            channel_open_timeout,
            cancellation,
            on_output,
        )
        .await;
        match attempt {
            Ok(SessionExecOutcome::Completed { exit_code, signal }) => {
                session.touch();
                return ExecRun {
                    result: Ok(ExecOutcome::completed(exit_code, signal)),
                    audit: Some(exit_code.map_or(AuditExit::Unknown, AuditExit::Code)),
                };
            }
            Ok(SessionExecOutcome::TimedOut(phase)) => {
                if phase.leaves_session_in_doubt() {
                    pool.evict_if_same(&conn.id, &session).await;
                }
                eprintln!(
                    "[mt-ssh-svc] exec deadline reached on connection '{}' ({phase:?})",
                    conn.name
                );
                return if phase.command_may_have_started() {
                    ExecRun {
                        result: Ok(ExecOutcome::timed_out(false)),
                        audit: Some(AuditExit::Timeout),
                    }
                } else {
                    ExecRun::not_started(Ok(ExecOutcome::timed_out(true)))
                };
            }
            Ok(SessionExecOutcome::Cancelled(phase)) => {
                if phase.leaves_session_in_doubt() {
                    pool.evict_if_same(&conn.id, &session).await;
                }
                return ExecRun {
                    result: Err(ServiceError::Cancelled),
                    audit: phase
                        .command_may_have_started()
                        .then_some(AuditExit::Cancelled),
                };
            }
            Ok(SessionExecOutcome::Rejected) => {
                return ExecRun::not_started(Err(ServiceError::Internal(
                    "the SSH server rejected the exec request; the remote command was not started"
                        .into(),
                )));
            }
            Ok(SessionExecOutcome::ChannelOpenTimedOut) => {
                pool.evict_if_same(&conn.id, &session).await;
                if retried {
                    return ExecRun::not_started(Err(ServiceError::Internal(format!(
                        "the SSH server did not open a channel within {}s, even on a fresh \
                         connection; the remote command was not started",
                        channel_open_timeout.as_secs()
                    ))));
                }
                eprintln!(
                    "[mt-ssh-svc] opening a channel on connection '{}' took longer than {}s; \
                     retiring the session and retrying on a fresh one",
                    conn.name,
                    channel_open_timeout.as_secs()
                );
            }
            Err(transport) => {
                if retried {
                    // 换了新 session 还是不行:进冷却,避免连发把服务器打死。
                    session.mark_unhealthy(UNHEALTHY_COOLDOWN);
                    return ExecRun::not_started(Err(ServiceError::Internal(format!(
                        "ssh exec failed after retry: {transport}"
                    ))));
                }
                // 可能是死链 race:精确淘汰这一条,换新的再试一次。
                eprintln!("[mt-ssh-svc] exec on cached session failed, retrying: {transport}");
                pool.evict_if_same(&conn.id, &session).await;
            }
        }
        retried = true;
    }
}

/// 一次 exec 请求。
#[derive(Debug, Clone, Copy)]
pub struct ExecRequest<'a> {
    pub scope: ConnectionScope<'a>,
    pub connection: &'a str,
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    /// 整个请求的时限(秒),缺省 [`DEFAULT_TIMEOUT_SECS`]。
    pub timeout_secs: Option<u64>,
}

/// 通过已保存的 SSH 连接在远程主机上执行一条命令 —— 完整编排。
///
/// 1. 每次重读 config.json,按 MCP project-id 或 CLI capability token 解析范围;
/// 2. 拼远程命令(可选 cwd 前缀,按需加引号);
/// 3. 建连 → 开 channel → 跑命令,整段受同一个时限(从请求进门算起)与取消令牌约束,
///    重试 / 淘汰 / 冷却规则见 [`exec_resolved`];
/// 4. 审计:只记可能已在远端执行的命令(见 `ExecRun::audit`)。
///
/// 输出经 `on_output` 实时流出;超时路径不撤回已流出的部分输出(见 [`ExecOutcome`])。
pub async fn exec(
    pool: &SshPool,
    request: ExecRequest<'_>,
    cancellation: &ExecCancellation,
    mut on_output: impl FnMut(StreamKind, &[u8]) + Send,
) -> Result<ExecOutcome, ServiceError> {
    let ExecRequest {
        scope,
        connection,
        command,
        cwd,
        timeout_secs,
    } = request;
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
    let deadline = tokio::time::Instant::now() + timeout;
    // 1. 每次入口都重读配置，保证范围或连接变更立即生效。
    let conn = find_connection(
        &read_scoped_connections(scope)?,
        connection,
        scope.list_hint(),
    )
    .map_err(ServiceError::InvalidParams)?;

    // 2. 拼远程命令(可选 cwd 前缀)。
    let remote_command = build_remote_command(command, cwd);

    // 3. 编排。
    let run = exec_resolved(
        pool,
        &conn,
        &remote_command,
        deadline,
        CHANNEL_OPEN_TIMEOUT,
        cancellation,
        &mut on_output,
    )
    .await;

    // 4. 审计(失败不影响结果)。
    if let Some(exit) = run.audit {
        append_audit_log(&conn.name, command, exit);
    }
    run.result
}

// ---------------------------------------------------------------------------
// transfer —— SFTP 上传/下载完整编排
// ---------------------------------------------------------------------------

/// 根据方向在一条 session 上跑一次 SFTP 传输。把 upload/download 两个 pool 函数
/// 统一成同一签名,供重试编排复用。
///
/// 上传走池的带整体时限入口:到 `deadline` 时池自己收尾,报错写明远端文件的状态(原地
/// 覆盖写到一半时写明「可能不完整」并给出路径);替换一旦开始必定跑完。下载到点由外层
/// 截断即可(本地临时文件随之删除,已有的本地文件不动),不需要这层收尾。
async fn run_one_transfer(
    direction: TransferDirection,
    session: &Arc<CachedSession>,
    local_path: &str,
    remote_path: &str,
    deadline: tokio::time::Instant,
) -> Result<u64, SftpTransferError> {
    match direction {
        TransferDirection::Upload => {
            run_sftp_upload_until(session, local_path, remote_path, deadline).await
        }
        TransferDirection::Download => {
            let timeout = remaining(deadline);
            run_sftp_download_on_session(session, remote_path, local_path, timeout).await
        }
    }
}

/// 上传在 deadline 之后再多等这么久才由外层兜底:池到点自己收尾,但替换一旦开始就要跑完
/// (通常只是几个往返),不能在那时被截断。
const UPLOAD_FINISH_GRACE: Duration = Duration::from_secs(10);

/// 外层兜底超时的时刻:下载就是 deadline,上传多留 [`UPLOAD_FINISH_GRACE`]。
fn transfer_backstop(
    direction: TransferDirection,
    deadline: tokio::time::Instant,
) -> tokio::time::Instant {
    match direction {
        TransferDirection::Upload => deadline + UPLOAD_FINISH_GRACE,
        TransferDirection::Download => deadline,
    }
}

/// 外层兜底到点时的报错。上传的兜底只会落在替换阶段(之前的阶段池在 deadline 前已收尾):
/// 替换在后台照样跑完,提醒调用方先看一眼远端文件再重试。下载沿用原来的文案。
fn transfer_backstop_message(
    direction: TransferDirection,
    timeout: Duration,
    remote_path: &str,
    retried: bool,
) -> String {
    let on_retry = if retried { " on retry" } else { "" };
    match direction {
        TransferDirection::Upload => format!(
            "ssh upload timed out after {}s{on_retry} while the uploaded file was being moved \
             into place at '{remote_path}'; that step finishes in the background, check the \
             remote file before retrying",
            timeout.as_secs()
        ),
        TransferDirection::Download => {
            format!(
                "ssh download timed out after {}s{on_retry}",
                timeout.as_secs()
            )
        }
    }
}

/// 池到点收尾的报错(只有上传会有):连接 / 传输层一类,消息里保留池写明的远端文件状态。
fn transfer_timed_out(
    direction: TransferDirection,
    timeout: Duration,
    error: &SftpTransferError,
) -> ServiceError {
    ServiceError::Internal(format!(
        "ssh {} timed out after {}s: {error}",
        direction.as_str(),
        timeout.as_secs()
    ))
}

/// 距 `deadline` 还剩多少时间。
fn remaining(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}

/// transfer 的建连:按 [`AcquireFailure`] 映射成 service 错误(三类都是错误,沿用
/// transfer 既有的超时口径),冷却中同样返错。
async fn acquire_for_transfer(
    pool: &SshPool,
    conn: &mt_core::SshConnection,
    deadline: tokio::time::Instant,
    timeout: Duration,
    direction: TransferDirection,
    reconnecting: bool,
) -> Result<Arc<CachedSession>, ServiceError> {
    // transfer 没有取消入口(daemon 不探测传输途中的断连),给一个永不触发的令牌。
    let never_cancelled = ExecCancellation::new();
    match acquire_within(pool, conn, deadline, &never_cancelled).await {
        Ok(session) if session.is_unhealthy_now() => {
            Err(ServiceError::Internal(cooldown_message(&conn.name)))
        }
        Ok(session) => Ok(session),
        Err(AcquireFailure::DeadlineExpired) => Err(ServiceError::Internal(format!(
            "ssh {} timed out after {}s while connecting to '{}'; the transfer was not started",
            direction.as_str(),
            timeout.as_secs(),
            conn.name
        ))),
        Err(AcquireFailure::Cancelled) => Err(ServiceError::Cancelled),
        Err(AcquireFailure::Failed(message)) if reconnecting => Err(ServiceError::Internal(
            format!("reconnect failed: {message}"),
        )),
        Err(AcquireFailure::Failed(message)) => Err(ServiceError::Internal(message)),
    }
}

/// 通过已保存的 SSH 连接做 SFTP 上传/下载 —— 完整编排。
///
/// 编排:护栏 → 查连接 → acquire → 传输 + transport 错的精确淘汰 + 单次 retry →
/// 审计 → 返回字节数。时限从请求进门算起,建连与传输共用(建连阶段到点报「连接阶段
/// 超时、传输没有开始」,不记审计 —— 与 acquire 失败同口径)。SFTP 业务错(路径不存在 /
/// 无权限 / 本地 IO)不 evict、不 retry,按 `InvalidParams` 返回。
///
/// 上传到点由池自己收尾,报错写明远端文件的状态(原地覆盖写到一半时写明「可能不完整」并
/// 给出路径);替换(含回滚)一旦开始必定跑完,外层兜底比时限多留
/// [`UPLOAD_FINISH_GRACE`]。
pub async fn transfer(
    pool: &SshPool,
    direction: TransferDirection,
    scope: ConnectionScope<'_>,
    connection: &str,
    local_path: &str,
    remote_path: &str,
    timeout_secs: Option<u64>,
) -> Result<u64, ServiceError> {
    // 0. 安全硬护栏:本地路径落在 mini-term 自己的数据目录里一律拒绝(配置库及其
    //    备份 / WAL、带密码信封的投影与存档、解信封的 credential.key 都在那里)。
    //    upload 与 download 都拦 —— 上传外泄、下载覆盖配置或密钥均不可接受。
    if is_blocked_local_path(local_path) {
        return Err(ServiceError::InvalidParams(
            "refusing to transfer files inside mini-term's own data directory: it holds the \
            config database (with its backups), the saved SSH credentials and the key that \
            decrypts them, which must never be uploaded or overwritten via this tool."
                .to_string(),
        ));
    }

    // 1. 查连接(列表已按本项目关联范围过滤,越权连接天然「未找到」)。错误不含密码。
    let conn = find_connection(
        &read_scoped_connections(scope)?,
        connection,
        scope.list_hint(),
    )
    .map_err(ServiceError::InvalidParams)?;

    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TRANSFER_TIMEOUT_SECS).max(1));
    let deadline = tokio::time::Instant::now() + timeout;
    let dir = direction.as_str();
    let audit = |result: Option<u64>| {
        append_transfer_audit_log(direction, &conn.name, local_path, remote_path, result);
    };

    // 2. 走池:lazy 建/复用 session,建连算进时限。auth/transport 错直接返(不进 retry)。
    let session = acquire_for_transfer(pool, &conn, deadline, timeout, direction, false).await?;

    // 3. 跑传输,受剩余时限约束。第一次 transport 错 → 精确淘汰 + 重连 → 再试一次。
    //    上传由池自己在 deadline 前收尾、报出精确的错误,外层兜底多留一点余量(见
    //    `transfer_backstop`);下载照旧到点即止。
    let backstop = transfer_backstop(direction, deadline);
    let first = tokio::time::timeout_at(
        backstop,
        run_one_transfer(direction, &session, local_path, remote_path, deadline),
    )
    .await;

    let bytes = match first {
        Ok(Ok(n)) => {
            session.touch();
            n
        }
        Ok(Err(e)) if e.is_transport() => {
            // transport-level:可能死链 race,精确淘汰 + 重建再试一次。
            eprintln!(
                "[mt-ssh-svc] sftp {dir} on cached session failed (transport), retrying: {e}"
            );
            pool.evict_if_same(&conn.id, &session).await;
            let session2 =
                acquire_for_transfer(pool, &conn, deadline, timeout, direction, true).await?;
            match tokio::time::timeout_at(
                backstop,
                run_one_transfer(direction, &session2, local_path, remote_path, deadline),
            )
            .await
            {
                Ok(Ok(n)) => {
                    session2.touch();
                    n
                }
                Ok(Err(second_err)) if second_err.is_timeout() => {
                    audit(None);
                    return Err(transfer_timed_out(direction, timeout, &second_err));
                }
                Ok(Err(second_err)) => {
                    if second_err.is_transport() {
                        session2.mark_unhealthy(UNHEALTHY_COOLDOWN);
                    }
                    audit(None);
                    return Err(ServiceError::Internal(format!(
                        "ssh {dir} failed after retry: {second_err}"
                    )));
                }
                Err(_) => {
                    audit(None);
                    return Err(ServiceError::Internal(transfer_backstop_message(
                        direction,
                        timeout,
                        remote_path,
                        true,
                    )));
                }
            }
        }
        Ok(Err(e)) if e.is_timeout() => {
            // 池在 deadline 前收的尾:报错里已写明远端文件的状态。
            audit(None);
            return Err(transfer_timed_out(direction, timeout, &e));
        }
        Ok(Err(e)) => {
            // SFTP 业务错(路径不存在/无权限/本地 IO):不 evict、不 retry,直接返。
            audit(None);
            return Err(ServiceError::InvalidParams(format!(
                "ssh {dir} failed: {e}"
            )));
        }
        Err(_) => {
            // 传输途中到点(上传:替换阶段超出了兜底余量,替换在后台照样跑完)。
            audit(None);
            return Err(ServiceError::Internal(transfer_backstop_message(
                direction,
                timeout,
                remote_path,
                false,
            )));
        }
    };

    // 4. 审计:成功记字节数。
    audit(Some(bytes));
    Ok(bytes)
}

// ============================================================================
// tests —— 随代码自 mt-ssh-mcp.rs 迁入,行为断言不变
// ============================================================================

/// exec 编排对着进程内 russh 假服务器的端到端用例(EOF、信号、拒绝、并发、开 channel
/// 超时重试、连接阶段超时 / 取消)。不连真实服务器,不碰真实数据目录。
#[cfg(test)]
mod fake_server_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `find_connection` 的「下一步指引」占位。
    const HINT: &str = "Run `mt-ssh-cli list` to see the connections.";

    fn conn(id: &str, password: Option<&str>) -> mt_core::SshConnection {
        mt_core::SshConnection {
            id: id.into(),
            name: format!("conn-{id}"),
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            password: password.map(|s| s.into()),
            identity_file: Some("/home/u/.ssh/id_rsa".into()),
            group: Some("内网".into()),
            extra: Default::default(),
        }
    }

    // --- connection_views ---

    #[test]
    fn connection_views_projects_all_connections() {
        // connection_views 本身不再过滤,原样投影传入的连接列表
        let conns = vec![conn("1", Some("secret1")), conn("2", None)];
        let views = connection_views(conns);
        let ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["1", "2"]);
    }

    #[test]
    fn connection_views_empty_input_yields_empty() {
        assert!(connection_views(vec![]).is_empty());
    }

    #[test]
    fn serialized_view_never_leaks_password_or_identity_file() {
        // 安全验收:即便源连接有明文密码与私钥路径,序列化结果也绝不能含它们。
        let conns = vec![conn("1", Some("super-secret-password"))];
        let views = connection_views(conns);
        let json = serde_json::to_string(&views).unwrap();
        assert!(!json.contains("super-secret-password"));
        assert!(!json.to_lowercase().contains("password"));
        assert!(!json.contains("identityFile"));
        assert!(!json.contains("id_rsa"));
        // 但应保留展示字段
        assert!(json.contains("\"host\":\"10.0.0.5\""));
    }

    // --- find_connection ---

    #[test]
    fn find_connection_matches_by_name() {
        let conns = vec![conn("1", None)];
        let found = find_connection(&conns, "conn-1", HINT).unwrap();
        assert_eq!(found.id, "1");
    }

    #[test]
    fn find_connection_matches_by_id_when_name_misses() {
        let conns = vec![conn("abc", None)];
        let found = find_connection(&conns, "abc", HINT).unwrap();
        assert_eq!(found.id, "abc");
    }

    #[test]
    fn find_connection_errors_when_not_found() {
        let conns = vec![conn("1", None)];
        let err = find_connection(&conns, "does-not-exist", HINT).unwrap_err();
        assert!(err.contains("No SSH connection found"));
    }

    #[test]
    fn find_connection_errors_on_ambiguous_name() {
        let mut a = conn("1", None);
        let mut b = conn("2", None);
        a.name = "dup".into();
        b.name = "dup".into();
        let err = find_connection(&[a, b], "dup", HINT).unwrap_err();
        assert!(err.contains("ambiguous"));
    }

    #[test]
    fn find_connection_not_found_error_never_contains_password() {
        // 安全:未找到时错误信息只回显 selector,绝不泄漏任何连接的明文密码。
        let conns = vec![conn("1", Some("topsecretpw"))];
        let err = find_connection(&conns, "does-not-exist", HINT).unwrap_err();
        assert!(!err.contains("topsecretpw"));
    }

    // --- build_remote_command ---

    #[test]
    fn build_remote_command_without_cwd() {
        assert_eq!(build_remote_command("ls -la", None), "ls -la");
    }

    #[test]
    fn build_remote_command_with_cwd_prefixes_cd() {
        assert_eq!(
            build_remote_command("ls -la", Some("/var/log")),
            "cd /var/log && ls -la"
        );
    }

    #[test]
    fn build_remote_command_ignores_blank_cwd() {
        assert_eq!(build_remote_command("pwd", Some("   ")), "pwd");
        assert_eq!(build_remote_command("pwd", Some("")), "pwd");
    }

    // --- cap_output ---

    #[test]
    fn cap_output_short_string_unchanged() {
        let (out, trunc) = cap_output("hello", 100);
        assert_eq!(out, "hello");
        assert!(!trunc);
    }

    #[test]
    fn cap_output_truncates_long_string() {
        let big = "x".repeat(500);
        let (out, trunc) = cap_output(&big, 100);
        assert!(trunc);
        assert!(out.starts_with(&"x".repeat(100)));
        assert!(out.contains("output truncated"));
    }

    #[test]
    fn cap_output_exact_cap_not_truncated() {
        let s = "y".repeat(100);
        let (out, trunc) = cap_output(&s, 100);
        assert!(!trunc);
        assert_eq!(out, s);
    }

    #[test]
    fn cap_output_respects_utf8_boundary() {
        // 多字节字符:cap 落在字符中间时,回退到边界,结果仍是合法 UTF-8。
        let s = "中".repeat(100); // 每个 '中' 占 3 字节
        let (out, trunc) = cap_output(&s, 100); // 100 不是 3 的倍数
        assert!(trunc);
        // 结果可被正常当作 &str 使用即证明是合法 UTF-8
        assert!(out.chars().take_while(|&c| c == '中').count() <= 34);
    }

    // --- format_audit_line ---

    #[test]
    fn format_audit_line_basic() {
        let line = format_audit_line("2026-05-18T12:00:00Z", "prod", "ls -la", AuditExit::Code(0));
        assert!(line.starts_with("2026-05-18T12:00:00Z\t"));
        assert!(line.contains("conn=prod"));
        assert!(line.contains("exit=0"));
        assert!(line.contains("cmd=ls -la"));
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn format_audit_line_timeout_has_no_exit_code() {
        let line = format_audit_line(
            "2026-05-18T12:00:00Z",
            "prod",
            "sleep 999",
            AuditExit::Timeout,
        );
        assert!(line.contains("exit=timeout"));
    }

    #[test]
    fn format_audit_line_collapses_multiline_command() {
        let line = format_audit_line("t", "c", "echo a\necho b\r\necho c", AuditExit::Code(0));
        // 命令里的换行被替成空格 —— 一次执行只占一行
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn format_audit_line_redacts_credentials() {
        // 安全验收:审计行里绝不出现明文密码
        let line = format_audit_line(
            "t",
            "prod",
            "mysql -h db -uroot -pS3cr3tPw -e 'select 1'",
            AuditExit::Code(0),
        );
        assert!(!line.contains("S3cr3tPw"), "实际: {line}");
        assert!(line.contains("-p***"), "实际: {line}");
        // 非凭据部分照旧可读
        assert!(line.contains("-h db"));
        assert!(line.contains("select 1"));
    }

    // --- redact_command ---

    #[test]
    fn redact_glued_mysql_password() {
        assert_eq!(redact_command("mysql -pSecret"), "mysql -p***");
        assert_eq!(
            redact_command("mysqldump -uroot -pP@ss db > d.sql"),
            "mysqldump -uroot -p*** db > d.sql"
        );
        // 带路径 / .exe / sudo 前缀也认得出客户端
        assert_eq!(
            redact_command("sudo /usr/bin/mysql -pSecret"),
            "sudo /usr/bin/mysql -p***"
        );
        assert_eq!(redact_command("mysql.exe -pSecret"), "mysql.exe -p***");
        assert_eq!(
            redact_command("docker exec db mariadb -pSecret"),
            "docker exec db mariadb -p***"
        );
    }

    #[test]
    fn redact_leaves_standalone_dash_p_and_its_value_alone() {
        // 反例:独立 `-p` 后面跟的是端口,不是密码
        assert_eq!(redact_command("mysql -p 3306"), "mysql -p 3306");
        assert_eq!(redact_command("psql -p 5432 -h db"), "psql -p 5432 -h db");
        assert_eq!(redact_command("ps -p 1234"), "ps -p 1234");
    }

    #[test]
    fn redact_leaves_non_credential_glued_dash_p_alone() {
        // 反例:命令里没有 mysql 家族客户端时,紧贴 `-p` 一律不动 ——
        // tar/cp/mkdir/docker/ssh 的 `-p` 都不是密码
        assert_eq!(redact_command("tar -pxvf a.tar"), "tar -pxvf a.tar");
        assert_eq!(redact_command("cp -pr a b"), "cp -pr a b");
        assert_eq!(redact_command("mkdir -pv /a/b"), "mkdir -pv /a/b");
        assert_eq!(
            redact_command("docker run -p8080:80 nginx"),
            "docker run -p8080:80 nginx"
        );
        assert_eq!(redact_command("ssh -p2222 h"), "ssh -p2222 h");
    }

    #[test]
    fn redact_long_password_flag_both_shapes() {
        assert_eq!(
            redact_command("pg_dump --password=Secret db"),
            "pg_dump --password=*** db"
        );
        assert_eq!(
            redact_command("wget --password Secret http://h/f"),
            "wget --password *** http://h/f"
        );
        // 值在下一段,中间隔多少空白都认
        assert_eq!(
            redact_command("wget --password   Secret"),
            "wget --password   ***"
        );
        // 光秃秃的 `--password=`(没有值)不动
        assert_eq!(redact_command("x --password="), "x --password=");
    }

    #[test]
    fn redact_user_colon_password() {
        assert_eq!(
            redact_command("curl -u alice:hunter2 https://api/x"),
            "curl -u alice:*** https://api/x"
        );
        assert_eq!(
            redact_command("curl -ualice:hunter2 https://api/x"),
            "curl -ualice:*** https://api/x"
        );
        assert_eq!(
            redact_command("curl --user=alice:hunter2 https://api/x"),
            "curl --user=alice:*** https://api/x"
        );
        assert_eq!(
            redact_command("curl --user alice:hunter2 https://api/x"),
            "curl --user alice:*** https://api/x"
        );
    }

    #[test]
    fn redact_leaves_user_without_colon_alone() {
        // 反例:没有冒号就只是用户名,不是凭据
        assert_eq!(redact_command("curl -u alice https://api"), "curl -u alice https://api");
        assert_eq!(redact_command("sudo -u www-data ls"), "sudo -u www-data ls");
        assert_eq!(redact_command("id -u"), "id -u");
    }

    #[test]
    fn redact_url_userinfo_password() {
        assert_eq!(
            redact_command("curl https://alice:hunter2@api.example.com/v1?a=1"),
            "curl https://alice:***@api.example.com/v1?a=1"
        );
        assert_eq!(
            redact_command("git clone https://u:tok@github.com/o/r.git"),
            "git clone https://u:***@github.com/o/r.git"
        );
        // 无用户名、只有密码的形态(redis / amqp 常见)
        assert_eq!(
            redact_command("redis-cli -u redis://:pw@127.0.0.1:6379"),
            "redis-cli -u redis://:***@127.0.0.1:6379"
        );
    }

    #[test]
    fn redact_leaves_non_credential_urls_alone() {
        // 反例:端口的冒号、无密码的 userinfo、scp 风格的 host:path 都不是凭据
        assert_eq!(
            redact_command("curl http://example.com:8080/a?b=1"),
            "curl http://example.com:8080/a?b=1"
        );
        assert_eq!(
            redact_command("ssh ssh://user@host:22/x"),
            "ssh ssh://user@host:22/x"
        );
        assert_eq!(
            redact_command("git clone git@github.com:owner/repo.git"),
            "git clone git@github.com:owner/repo.git"
        );
    }

    #[test]
    fn redact_leaves_plain_commands_byte_identical() {
        // 反例:没有凭据形态的命令必须逐字节原样返回(含连续空格与缩进)
        for cmd in [
            "ls -la",
            "systemctl restart nginx",
            "tail -n 100 /var/log/syslog | grep -i error",
            "  echo   'a   b'  ",
            "find / -name '*.log' -mtime +7 -delete",
            "",
        ] {
            assert_eq!(redact_command(cmd), cmd, "不该改动: {cmd}");
        }
    }

    #[test]
    fn redact_preserves_newlines_and_multiple_hits() {
        // 多段命令里出现多处凭据时全都掩掉,换行等空白原样保留
        let src = "mysql -pA\ncurl -u u:B https://x:C@h/p";
        let out = redact_command(src);
        assert_eq!(out, "mysql -p***\ncurl -u u:*** https://x:***@h/p");
        assert!(!out.contains("-pA"));
        assert!(!out.contains(":B"));
        assert!(!out.contains(":C@"));
    }

    // --- utc_timestamp ---

    #[test]
    fn utc_timestamp_has_expected_shape() {
        let ts = utc_timestamp();
        // 形如 YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "got: {ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        // 年份在合理范围
        let year: i64 = ts[..4].parse().unwrap();
        assert!((2025..2100).contains(&year));
    }

    // --- 传输护栏 is_blocked_local_path ---------------------------------

    /// 造一个「数据目录」夹具:`<tmp>/mt-ssh-svc-guard-<pid>-<name>/{data,outside}`,
    /// 数据目录里放几个真实存在的文件与一个子目录。返回 (夹具根, 数据目录, 外部目录)。
    fn guard_fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("mt-ssh-svc-guard-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("com.mini-term.app");
        let outside = base.join("outside");
        std::fs::create_dir_all(data.join("themes")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        for file in ["config.db", "config.json", "credential.key"] {
            std::fs::write(data.join(file), b"x").unwrap();
        }
        (base, data, outside)
    }

    fn blocked(local: &std::path::Path, roots: &[PathBuf]) -> bool {
        is_blocked_local_path_against(local.to_str().unwrap(), roots)
    }

    #[test]
    fn guard_blocks_every_file_under_the_data_dir() {
        // 数据库迁移 + 密码封存之后,要护的远不止 config.json:库本体、备份、WAL、
        // 存档、主密钥,以及子目录里的东西 —— 存在的与还不存在的(下载目标)都拦。
        let (base, data, _) = guard_fixture("under");
        let roots = [data.clone()];
        for rel in [
            "config.db",
            "config.json",
            "credential.key",
            "config.db.bak",
            "config.db-wal",
            "config.db-shm",
            "config.json.pre-sqlite",
            "themes/pack.json",
            "not-yet/created/deep.bin",
        ] {
            let path = data.join(rel);
            assert!(blocked(&path, &roots), "应拦截 {}", path.display());
        }
        assert!(blocked(&data, &roots), "数据目录本身也算在内");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn guard_allows_paths_outside_the_data_dir() {
        let (base, data, outside) = guard_fixture("outside");
        let roots = [data.clone()];
        std::fs::write(outside.join("notes.txt"), b"hi").unwrap();
        assert!(!blocked(&outside.join("notes.txt"), &roots));
        assert!(!blocked(&outside.join("new-download.bin"), &roots));
        // 同名前缀的兄弟目录按路径分量比较,不误伤
        let sibling = base.join("com.mini-term.app-backup").join("config.db");
        assert!(!blocked(&sibling, &roots));
        // 没有可护的目录(定位失败)时,普通路径放行……
        assert!(!blocked(&outside.join("notes.txt"), &[]));
        // ……但路径里点了装机版数据目录的名字,纵深那道照样拦
        assert!(blocked(&data.join("config.db"), &[]));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn guard_resolves_dotdot_before_comparing() {
        let (base, data, outside) = guard_fixture("dotdot");
        let roots = [data.clone()];
        // 从外部目录绕回数据目录:存在的文件、以及经由不存在的中间目录
        let sneaky = outside
            .join("..")
            .join("com.mini-term.app")
            .join("credential.key");
        assert!(blocked(&sneaky, &roots));
        let via_missing = outside
            .join("missing")
            .join("..")
            .join("..")
            .join("com.mini-term.app")
            .join("config.db-wal");
        assert!(blocked(&via_missing, &roots));
        // 反过来从受护目录里 `..` 出去的路径,按前缀不拦(dev 目录名普通,只有前缀这一道)
        let dev = base.join("dev-data");
        std::fs::create_dir_all(dev.join("themes")).unwrap();
        let escaped = dev
            .join("themes")
            .join("..")
            .join("..")
            .join("outside")
            .join("a.txt");
        assert!(!blocked(&escaped, std::slice::from_ref(&dev)));
        // 装机版数据目录名在原始路径里出现过就拦(规范化前后都查)—— 先进数据目录再
        // `..` 出来的写法没有正当用途,宁可多拦
        let named = data
            .join("themes")
            .join("..")
            .join("..")
            .join("outside")
            .join("a.txt");
        assert!(blocked(&named, &roots));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn guard_works_before_the_data_dir_exists() {
        // 全新机器上数据目录还没建:两边都按字面折叠到同一个存在的祖先,照样命中。
        let (base, _, _) = guard_fixture("fresh");
        let data = base.join("not-created").join("com.mini-term.app");
        let roots = [data.clone()];
        assert!(blocked(&data.join("credential.key"), &roots));
        assert!(!blocked(
            &base.join("not-created").join("other.txt"),
            &roots
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn guard_checks_every_protected_root() {
        // 装机版目录 + MT_APP_DATA_DIR 指的 dev 目录:落在任一个里都拦。
        let (base, data, outside) = guard_fixture("roots");
        let dev = outside.join("dev-data");
        std::fs::create_dir_all(&dev).unwrap();
        let roots = [data.clone(), dev.clone()];
        assert!(blocked(&dev.join("config.db"), &roots));
        assert!(blocked(&data.join("config.db"), &roots));
        assert!(!blocked(&outside.join("x.txt"), &roots));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn guard_is_case_insensitive_on_windows() {
        let (base, data, _) = guard_fixture("case");
        let roots = [data.clone()];
        let upper = PathBuf::from(data.to_string_lossy().to_uppercase());
        assert!(blocked(&upper.join("CONFIG.DB"), &roots));
        assert!(blocked(&upper.join("NEW-FILE.BIN"), &roots));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn guard_follows_symlinks_into_the_data_dir() {
        let (base, data, outside) = guard_fixture("symlink");
        let roots = [data.clone()];
        let link = outside.join("innocent");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        assert!(blocked(&link.join("credential.key"), &roots));
        assert!(blocked(&link.join("not-yet.bin"), &roots));
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- format_transfer_audit_line ---------------------------------------

    #[test]
    fn format_transfer_audit_line_upload_basic() {
        let line = format_transfer_audit_line(
            "2026-06-09T12:00:00Z",
            TransferDirection::Upload,
            "prod",
            "/local/a.bin",
            "/remote/b.bin",
            Some(1234),
        );
        assert!(line.starts_with("2026-06-09T12:00:00Z\t"));
        assert!(line.contains("dir=upload"));
        assert!(line.contains("conn=prod"));
        assert!(line.contains("local=/local/a.bin"));
        assert!(line.contains("remote=/remote/b.bin"));
        assert!(line.contains("result=1234"));
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn format_transfer_audit_line_download_error_result() {
        let line =
            format_transfer_audit_line("t", TransferDirection::Download, "c", "/l", "/r", None);
        assert!(line.contains("dir=download"));
        assert!(line.contains("result=error"));
    }

    #[test]
    fn format_transfer_audit_line_sanitizes_newlines_and_tabs_to_one_line() {
        // 路径里的换行/制表被替成空格 —— 一次传输只占一行。
        let line = format_transfer_audit_line(
            "t",
            TransferDirection::Upload,
            "weird\nconn",
            "/a\tb\nc",
            "/r\rd",
            Some(0),
        );
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }

    // --- SftpTransferError: 分类 + 错误不含密码 ----------------------------

    #[test]
    fn sftp_transfer_error_classification() {
        assert!(SftpTransferError::Transport("x".into()).is_transport());
        assert!(!SftpTransferError::Sftp("x".into()).is_transport());
        // 池到点收尾:不重试(不是 transport 错)。
        let timed_out = SftpTransferError::TimedOut("x".into());
        assert!(timed_out.is_timeout());
        assert!(!timed_out.is_transport());
    }

    #[test]
    fn sftp_transfer_error_message_never_contains_password() {
        // 安全:错误文本只透传库错误(不携带凭据)。模拟一个含路径的业务错,
        // 验证它绝不会回带连接密码(密码从不进入这些错误的构造)。
        let err =
            SftpTransferError::Sftp("sftp open '/etc/shadow' failed: permission denied".into());
        let msg = err.to_string();
        assert!(!msg.to_lowercase().contains("password"));
        assert!(msg.contains("permission denied"));
    }

    // --- transfer 的时限:上传由池收尾,外层兜底多留余量 ----------------------

    #[test]
    fn upload_backstop_leaves_room_for_the_replacement_to_finish() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        assert_eq!(
            transfer_backstop(TransferDirection::Download, deadline),
            deadline
        );
        assert_eq!(
            transfer_backstop(TransferDirection::Upload, deadline),
            deadline + UPLOAD_FINISH_GRACE
        );
    }

    #[test]
    fn transfer_timeout_messages() {
        let timeout = Duration::from_secs(300);
        // 下载沿用原来的文案。
        assert_eq!(
            transfer_backstop_message(TransferDirection::Download, timeout, "/r", false),
            "ssh download timed out after 300s"
        );
        assert_eq!(
            transfer_backstop_message(TransferDirection::Download, timeout, "/r", true),
            "ssh download timed out after 300s on retry"
        );
        // 上传的兜底只会落在替换阶段:写明替换在后台跑完,先看远端文件。
        let upload =
            transfer_backstop_message(TransferDirection::Upload, timeout, "/srv/app.conf", false);
        assert!(
            upload.starts_with("ssh upload timed out after 300s"),
            "{upload}"
        );
        assert!(upload.contains("'/srv/app.conf'"), "{upload}");
        assert!(upload.contains("finishes in the background"), "{upload}");

        // 池到点收尾的报错:连接 / 传输层一类,保留池写明的远端文件状态。
        let pool_error = SftpTransferError::TimedOut(
            "sftp upload to '/etc/app.conf' timed out while overwriting it in place (x); the \
             remote file '/etc/app.conf' may now be incomplete"
                .into(),
        );
        let error = transfer_timed_out(
            TransferDirection::Upload,
            Duration::from_secs(30),
            &pool_error,
        );
        assert!(matches!(error, ServiceError::Internal(_)), "{error}");
        assert!(
            error
                .message()
                .starts_with("ssh upload timed out after 30s: "),
            "{error}"
        );
        assert!(
            error
                .message()
                .contains("the remote file '/etc/app.conf' may now be incomplete"),
            "{error}"
        );
    }

    // --- ServiceError 分类与文本 -------------------------------------------

    #[test]
    fn service_error_message_passthrough() {
        assert_eq!(
            ServiceError::InvalidParams("no such connection".into()).message(),
            "no such connection"
        );
        assert_eq!(
            ServiceError::Internal("reconnect failed: x".into()).to_string(),
            "reconnect failed: x"
        );
    }

    #[tokio::test]
    async fn exec_cancellation_wakes_current_and_future_waiters() {
        let cancellation = ExecCancellation::new();
        let waiting = cancellation.clone();
        let waiter = tokio::spawn(async move { waiting.cancelled().await });

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("当前 waiter 应被唤醒")
            .expect("waiter task");

        tokio::time::timeout(Duration::from_millis(50), cancellation.cancelled())
            .await
            .expect("取消状态必须对未来 waiter 保持可见");
    }

    // --- 回调收集器:MCP 侧收集行为等价于旧 run_exec_on_session 返回值 -----

    #[test]
    fn callback_collector_accumulates_streams_separately() {
        // MCP 适配层用同一个闭包把 stdout/stderr 分别收集进两个缓冲。
        // 这里验证该收集模式:分片乱序到达也能按流归位、字节序保持。
        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut stderr_buf: Vec<u8> = Vec::new();
        {
            let mut collect = |kind: StreamKind, data: &[u8]| match kind {
                StreamKind::Stdout => stdout_buf.extend_from_slice(data),
                StreamKind::Stderr => stderr_buf.extend_from_slice(data),
            };
            collect(StreamKind::Stdout, b"hel");
            collect(StreamKind::Stderr, b"warn:");
            collect(StreamKind::Stdout, b"lo\n");
            collect(StreamKind::Stderr, b" disk\n");
        }
        assert_eq!(stdout_buf, b"hello\n");
        assert_eq!(stderr_buf, b"warn: disk\n");
    }

    // --- find_connection:未找到时指向各自的列连接方式(A2-6) ---------------

    #[test]
    fn find_connection_not_found_points_to_the_right_list_command() {
        let conns = vec![conn("1", None)];
        let cli = find_connection(&conns, "nope", ConnectionScope::Capability("t").list_hint())
            .unwrap_err();
        assert!(cli.contains("mt-ssh-cli list"), "{cli}");
        assert!(!cli.contains("ssh_list_connections"), "{cli}");
        let mcp = find_connection(
            &conns,
            "nope",
            ConnectionScope::LegacyProject(None).list_hint(),
        )
        .unwrap_err();
        assert!(mcp.contains("ssh_list_connections"), "{mcp}");
    }

    #[test]
    fn cooldown_message_explains_transport_failures_and_when_to_retry() {
        let msg = cooldown_message("prod");
        assert!(msg.contains("'prod'"), "{msg}");
        assert!(msg.contains("transport"), "{msg}");
        assert!(msg.contains("30"), "{msg}");
        assert!(
            !msg.to_lowercase().contains("auth"),
            "冷却与认证无关: {msg}"
        );
    }

    #[test]
    fn not_started_timeout_message_says_the_command_did_not_run() {
        assert_eq!(
            not_started_timeout_message(60, "prod"),
            "timed out after 60s while connecting to 'prod'; the remote command was not started"
        );
    }

    // --- build_remote_command:--cwd 引号(A2-5) ----------------------------

    fn cd(dir: &str) -> String {
        build_remote_command("ls", Some(dir))
    }

    #[test]
    fn cwd_simple_paths_stay_unquoted() {
        // 简单路径原样 —— Windows OpenSSH 的 cmd.exe 远端照样认得
        for dir in [
            "/var/log",
            "/srv/app-1.2_x",
            "D:/work",
            "relative/dir",
            "/home/张三/项目",
            "~",
            "~/",
            "~/proj",
            "~bob/x",
            "/opt/a+b@c:d",
        ] {
            assert_eq!(cd(dir), format!("cd {dir} && ls"), "{dir}");
        }
    }

    #[test]
    fn cwd_with_spaces_or_specials_is_double_quoted() {
        assert_eq!(cd("/srv/my app"), r#"cd "/srv/my app" && ls"#);
        assert_eq!(cd("/tmp/it's"), r#"cd "/tmp/it's" && ls"#);
        // 想借 cwd 夹带命令:被引号压成字面路径
        assert_eq!(cd("/a;rm -rf /"), r#"cd "/a;rm -rf /" && ls"#);
        assert_eq!(cd("/a&&b|c>d"), r#"cd "/a&&b|c>d" && ls"#);
        // Windows 路径:反斜杠在 POSIX 双引号里是字面量,cmd.exe 也认双引号
        assert_eq!(cd(r"C:\work dir"), r#"cd "C:\work dir" && ls"#);
        assert_eq!(cd(r"C:\work"), r#"cd "C:\work" && ls"#);
        assert_eq!(cd("/x/100%,y=z"), r#"cd "/x/100%,y=z" && ls"#);
        assert_eq!(cd("/x/a\tb"), "cd \"/x/a\tb\" && ls");
    }

    #[test]
    fn cwd_falls_back_to_single_quotes_when_double_quotes_would_expand() {
        assert_eq!(cd("/tmp/$x"), "cd '/tmp/$x' && ls");
        assert_eq!(cd("/tmp/`id`"), "cd '/tmp/`id`' && ls");
        assert_eq!(cd(r#"/tmp/a"b"#), r#"cd '/tmp/a"b' && ls"#);
        assert_eq!(cd("/tmp/hi!"), "cd '/tmp/hi!' && ls");
        assert_eq!(cd("/tmp/a\nb"), "cd '/tmp/a\nb' && ls");
        // 末尾的反斜杠会吃掉收尾的双引号;`\$` 在双引号里会被转义
        assert_eq!(cd(r"C:\dir\"), r"cd 'C:\dir\' && ls");
        assert_eq!(cd(r"/x/a\$b"), r"cd '/x/a\$b' && ls");
        // 单引号本身:'\'' 拼接
        assert_eq!(cd("/tmp/it's $x"), r"cd '/tmp/it'\''s $x' && ls");
    }

    #[test]
    fn cwd_keeps_tilde_expansion_outside_quotes() {
        // `~` 与紧跟的 `/` 都留在引号外 —— `~"/my proj"` 在 POSIX 里不展开
        assert_eq!(cd("~/my proj"), r#"cd ~/"my proj" && ls"#);
        assert_eq!(cd("~bob/my proj"), r#"cd ~bob/"my proj" && ls"#);
        assert_eq!(cd("~/$HOME"), "cd ~/'$HOME' && ls");
        // `~` 后面不是合法登录名:整段加引号,不做展开
        assert_eq!(cd("~ x"), r#"cd "~ x" && ls"#);
        assert_eq!(cd("~+"), r#"cd "~+" && ls"#);
    }

    #[test]
    fn cwd_leading_dash_is_not_taken_as_an_option() {
        assert_eq!(cd("-foo"), "cd ./-foo && ls");
        assert_eq!(cd("-my dir"), r#"cd "./-my dir" && ls"#);
    }

    // --- redact_command:A2-7 扩展形态 ---------------------------------------

    #[test]
    fn redact_more_secret_long_options_in_both_shapes() {
        for (src, want) in [
            ("tool --passwd hunter2 x", "tool --passwd *** x"),
            ("tool --pass hunter2", "tool --pass ***"),
            ("tool --pass=hunter2", "tool --pass=***"),
            ("gh --token ghp_abc", "gh --token ***"),
            ("gh --token=ghp_abc", "gh --token=***"),
            ("x --api-key k1 y", "x --api-key *** y"),
            ("x --api-key=k1", "x --api-key=***"),
            ("x --secret s1", "x --secret ***"),
            ("x --secret=s1", "x --secret=***"),
            ("az --client-secret cs", "az --client-secret ***"),
            ("az --client-secret=cs", "az --client-secret=***"),
            ("wget --http-password=pw u", "wget --http-password=*** u"),
            ("x --access-token at", "x --access-token ***"),
            ("x --API_KEY=k", "x --API_KEY=***"),
            ("gpg --passphrase pp", "gpg --passphrase ***"),
            // 值选项后面直接跟了另一个选项:说明它没带值,两个都原样
            ("tool --password --help", "tool --password --help"),
        ] {
            assert_eq!(redact_command(src), want, "{src}");
        }
    }

    #[test]
    fn redact_quoted_values_spanning_several_words() {
        assert_eq!(
            redact_command("tool --password 'my secret pw' --verbose"),
            "tool --password '*** *** ***' --verbose"
        );
        assert_eq!(
            redact_command(r#"x --token="a b" y"#),
            r#"x --token="*** ***" y"#
        );
        assert_eq!(
            redact_command("PGPASSWORD='a b' psql"),
            "PGPASSWORD='*** ***' psql"
        );
    }

    #[test]
    fn redact_sshpass_password_but_not_the_wrapped_commands_port() {
        assert_eq!(
            redact_command("sshpass -p hunter2 ssh -p 22 root@h uptime"),
            "sshpass -p *** ssh -p 22 root@h uptime"
        );
        assert_eq!(
            redact_command("sshpass -phunter2 scp -P 2222 a h:/b"),
            "sshpass -p*** scp -P 2222 a h:/b"
        );
        // sshpass 其它选项的参数原样;-e 走环境变量,那个变量另按赋值规则打码
        assert_eq!(
            redact_command("sshpass -f /tmp/pw ssh h"),
            "sshpass -f /tmp/pw ssh h"
        );
        assert_eq!(
            redact_command("SSHPASS=hunter2 sshpass -e ssh h"),
            "SSHPASS=*** sshpass -e ssh h"
        );
    }

    #[test]
    fn redact_secret_env_assignments() {
        for (src, want) in [
            ("PGPASSWORD=s3 psql -h db", "PGPASSWORD=*** psql -h db"),
            ("MYSQL_PWD=s3 mysql -h db", "MYSQL_PWD=*** mysql -h db"),
            (
                "export AWS_SECRET_ACCESS_KEY=abc",
                "export AWS_SECRET_ACCESS_KEY=***",
            ),
            (
                "GITHUB_TOKEN=ghp_x gh pr list",
                "GITHUB_TOKEN=*** gh pr list",
            ),
            (
                "env OPENAI_API_KEY=sk-1 python a.py",
                "env OPENAI_API_KEY=*** python a.py",
            ),
            ("REDISCLI_AUTH=pw redis-cli", "REDISCLI_AUTH=*** redis-cli"),
            ("db_password=x ./run", "db_password=*** ./run"),
            (
                "docker run -e MYSQL_ROOT_PASSWORD=pw mysql",
                "docker run -e MYSQL_ROOT_PASSWORD=*** mysql",
            ),
            (
                "kubectl create secret generic s --from-literal=password=pw",
                "kubectl create secret generic s --from-literal=password=***",
            ),
            (
                "helm install x --set auth.password=pw",
                "helm install x --set auth.password=***",
            ),
        ] {
            assert_eq!(redact_command(src), want, "{src}");
        }
    }

    #[test]
    fn redact_http_authorization_headers() {
        for (src, want) in [
            (
                "curl -H 'Authorization: Bearer abc.def' https://api/x",
                "curl -H 'Authorization: Bearer ***' https://api/x",
            ),
            (
                r#"curl -H "Authorization: Basic dXNlcjpwYXNz" https://api/x"#,
                r#"curl -H "Authorization: Basic ***" https://api/x"#,
            ),
            (
                "curl --header='Authorization: token ghp_1' u",
                "curl --header='Authorization: token ***' u",
            ),
            (
                "curl --header 'authorization:Bearer abc' u",
                "curl --header 'authorization:Bearer ***' u",
            ),
            (
                "curl -H'Proxy-Authorization: Basic Zm9v' u",
                "curl -H'Proxy-Authorization: Basic ***' u",
            ),
            ("curl -H 'X-Api-Key: k123' u", "curl -H 'X-Api-Key: ***' u"),
            (
                "curl -H 'Private-Token:glpat-1' u",
                "curl -H 'Private-Token:***' u",
            ),
            // 没写方案名:那一段本身就是凭据
            (
                "curl -H 'Authorization: abc123' u",
                "curl -H 'Authorization: ***' u",
            ),
        ] {
            assert_eq!(redact_command(src), want, "{src}");
        }
    }

    #[test]
    fn redact_keytool_and_openssl_passwords() {
        assert_eq!(
            redact_command("keytool -list -keystore k.jks -storepass changeit"),
            "keytool -list -keystore k.jks -storepass ***"
        );
        // openssl 只掩 `pass:` 明文;`env:` / `file:` 不是凭据本身
        assert_eq!(
            redact_command("openssl pkcs12 -in a.p12 -passin pass:s3cret -passout env:PW"),
            "openssl pkcs12 -in a.p12 -passin pass:*** -passout env:PW"
        );
        assert_eq!(
            redact_command("openssl enc -pass 'pass:a b'"),
            "openssl enc -pass 'pass:*** ***'"
        );
    }

    #[test]
    fn redact_does_not_touch_ordinary_commands() {
        for cmd in [
            "ps -p 123",
            "ssh -p 22 root@h",
            "grep token file.txt",
            "grep -ri password /etc/ssh/sshd_config",
            "grep -i 'authorization' access.log",
            "echo $TOKEN",
            "export TOKEN",
            "cat ~/.config/token.txt",
            "docker login -u bob --password-stdin",
            "psql --no-password -h db",
            "curl -H 'Accept: application/json' https://api/x",
            "git log --author=token-bot",
            "PWD=/x OLDPWD=/y make",
            "dd if=/dev/zero of=/tmp/x bs=1M count=1",
            "kubectl get secret my-secret -o yaml",
            "tool --token-file /run/secrets/t",
            "llm --max-tokens 100",
        ] {
            assert_eq!(redact_command(cmd), cmd, "不该改动: {cmd}");
        }
    }

    #[test]
    fn audit_exit_field_names_each_outcome() {
        let line = |exit| format_audit_line("t", "c", "true", exit);
        assert!(line(AuditExit::Code(137)).contains("\texit=137\t"));
        assert!(line(AuditExit::Timeout).contains("\texit=timeout\t"));
        assert!(line(AuditExit::Cancelled).contains("\texit=cancelled\t"));
        assert!(line(AuditExit::Unknown).contains("\texit=unknown\t"));
    }

    // --- 传输护栏:别名与按名字的纵深(A2-8) --------------------------------

    #[test]
    fn local_aliases_map_back_to_drive_paths() {
        let map = |p: &str| map_local_alias(p, Some("MYPC"));
        for (alias, want) in [
            (r"\\?\C:\Users\u\x", r"C:\Users\u\x"),
            (r"\\.\c:\x", r"C:\x"),
            (r"\\localhost\C$\Users\u", r"C:\Users\u"),
            (r"\\127.0.0.1\d$\data", r"D:\data"),
            (r"\\127.0.0.2\c$\x", r"C:\x"),
            (r"\\MyPc\c$\x", r"C:\x"),
            (r"\\?\UNC\localhost\c$\x", r"C:\x"),
            (r"\\.\UNC\LOCALHOST\C$\x", r"C:\x"),
            (r"\\0--1.ipv6-literal.net\c$\x", r"C:\x"),
            (r"\\wsl.localhost\Ubuntu\mnt\c\Users\u", r"C:\Users\u"),
            (r"\\wsl$\Debian\mnt\d", r"D:\"),
            (r"\\?\UNC\wsl.localhost\Ubuntu\mnt\c\x", r"C:\x"),
            ("//localhost/c$/Users/u", r"C:\Users\u"),
        ] {
            assert_eq!(map(alias).as_deref(), Some(want), "{alias}");
        }
        // 不是指向本机磁盘的别名:交回原路径(None)
        for other in [
            r"C:\Users\u",
            r"\\server\share\x",
            r"\\localhost\share\x",
            r"\\wsl.localhost\Ubuntu\home\u",
            r"\\wsl.localhost\Ubuntu\mnt\wsl\x",
            r"\\?\Volume{0000}\x",
            "/home/u",
            "relative",
        ] {
            assert_eq!(map(other), None, "{other}");
        }
        // 本机名没拿到时只认 localhost / 回环
        assert_eq!(map_local_alias(r"\\MyPc\c$\x", None), None);
    }

    #[test]
    fn guard_blocks_any_path_naming_the_installed_data_dir() {
        // 根目录列表为空(定位不到任何数据目录)也照拦 —— 纯按名字,不碰真实数据目录。
        let base =
            std::env::temp_dir().join(format!("mt-ssh-svc-guard-name-{}", std::process::id()));
        for path in [
            base.join("com.mini-term.app").join("config.db"),
            base.join("COM.MINI-TERM.APP").join("x"),
            base.join("nested").join("com.mini-term.app"),
        ] {
            assert!(blocked(&path, &[]), "{}", path.display());
        }
        // 同名前缀 / 后缀的目录不算
        assert!(!blocked(
            &base.join("com.mini-term.app-backup").join("config.db"),
            &[]
        ));
        assert!(!blocked(&base.join("my.com.mini-term.app").join("x"), &[]));
    }

    #[test]
    fn installed_data_dir_name_matches_the_sidecar_anchor() {
        // 只取路径、不碰磁盘:名字必须与 sidecar 读投影的那个目录一致。
        if let Some(path) = mt_core::config_json_path() {
            let name = path
                .parent()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned());
            assert_eq!(name.as_deref(), Some(INSTALLED_DATA_DIR_NAME));
        }
    }

    #[cfg(windows)]
    #[test]
    fn guard_name_check_sees_through_ntfs_stream_and_trailing_dot_spellings() {
        let base = std::env::temp_dir();
        for name in [
            "com.mini-term.app.",
            "com.mini-term.app ",
            "com.mini-term.app::$INDEX_ALLOCATION",
            "com.mini-term.app:$I30:$INDEX_ALLOCATION",
        ] {
            let path = base.join(name).join("config.db");
            assert!(blocked(&path, &[]), "{}", path.display());
        }
    }

    /// dev 数据目录名字很普通,只按前缀拦:用别名指过去也得认出来。
    #[cfg(windows)]
    #[test]
    fn guard_sees_through_local_aliases_of_a_protected_root() {
        let base = std::env::temp_dir().join(format!(
            "mt-ssh-svc-guard-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dev = base.join("dev-data");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("config.db"), b"x").unwrap();
        let roots = [dev.clone()];

        let plain = dev.join("config.db").to_string_lossy().into_owned();
        let (drive, rest) = plain.split_at(2);
        let letter = drive[..1].to_ascii_lowercase();
        let aliases = [
            plain.clone(),
            format!(r"\\?\{plain}"),
            format!(r"\\localhost\{letter}${rest}"),
            format!(r"\\127.0.0.1\{letter}${rest}"),
            format!(r"\\?\UNC\localhost\{letter}${rest}"),
            format!(r"\\wsl.localhost\FakeDistro\mnt\{letter}{rest}"),
            format!(r"\\wsl$\FakeDistro\mnt\{letter}{}", rest.replace('\\', "/")),
            // 还不存在的下载目标也一样
            format!(r"\\localhost\{letter}${rest}.new"),
        ];
        for alias in aliases {
            assert!(blocked(Path::new(&alias), &roots), "应拦截 {alias}");
        }
        // 同一台机器上别的目录照常放行
        let outside = format!(
            r"\\localhost\{letter}${}",
            &base.join("elsewhere.txt").to_string_lossy()[2..]
        );
        assert!(!blocked(Path::new(&outside), &roots), "{outside}");
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- 远端信号(A2-4) ------------------------------------------------------

    fn remote_signal(name: &str, core_dumped: bool, message: &str) -> RemoteSignal {
        RemoteSignal {
            name: name.into(),
            core_dumped,
            error_message: message.into(),
        }
    }

    #[test]
    fn common_signals_map_to_128_plus_number() {
        for (name, code) in [
            ("HUP", 129),
            ("INT", 130),
            ("QUIT", 131),
            ("ILL", 132),
            ("ABRT", 134),
            ("FPE", 136),
            ("KILL", 137),
            ("USR1", 138),
            ("SEGV", 139),
            ("USR2", 140),
            ("PIPE", 141),
            ("ALRM", 142),
            ("TERM", 143),
        ] {
            assert_eq!(remote_signal(name, false, "").exit_code(), code, "{name}");
        }
        // 大小写、SIG 前缀都认
        assert_eq!(signal_number("sigterm"), Some(15));
        assert_eq!(signal_number("Kill"), Some(9));
        // 认不出的统一 255
        for name in ["FOO@example.com", "WINCH", "", "SIG"] {
            assert_eq!(signal_number(name), None, "{name:?}");
            assert_eq!(
                remote_signal(name, false, "").exit_code(),
                EXIT_UNKNOWN_SIGNAL
            );
        }
    }

    #[test]
    fn signal_from_wire_covers_every_russh_variant() {
        for (sig, code) in [
            (Sig::ABRT, 134),
            (Sig::ALRM, 142),
            (Sig::FPE, 136),
            (Sig::HUP, 129),
            (Sig::ILL, 132),
            (Sig::INT, 130),
            (Sig::KILL, 137),
            (Sig::PIPE, 141),
            (Sig::QUIT, 131),
            (Sig::SEGV, 139),
            (Sig::TERM, 143),
            (Sig::USR1, 138),
            (Sig::Custom("USR2".into()), 140),
            (Sig::Custom("FOO@example.com".into()), 255),
        ] {
            let signal = RemoteSignal::from_wire(&sig, false, "");
            assert_eq!(signal.exit_code(), code, "{sig:?}");
        }
    }

    #[test]
    fn signal_description_is_one_clean_line() {
        let segv = remote_signal("SEGV", true, "bad\r\naccess\x1b[31m");
        assert_eq!(
            segv.describe(139),
            "remote command terminated by signal SIGSEGV (core dumped): bad access [31m; exit code 139"
        );
        assert_eq!(
            remote_signal("KILL", false, "").describe(137),
            "remote command terminated by signal SIGKILL; exit code 137"
        );
        assert_eq!(
            remote_signal("FOO@example.com", false, "").describe(255),
            "remote command terminated by unrecognized signal \"FOO@example.com\"; exit code 255"
        );
        let long = remote_signal("TERM", false, &"x".repeat(500));
        assert!(long.describe(143).len() < 300);
    }

    #[test]
    fn signal_note_reports_the_final_exit_code() {
        // 远端两样都报了:退出码以 exit-status 为准,说明里写的也是它
        let both = ExecOutcome::completed(Some(1), Some(remote_signal("TERM", false, "")));
        assert!(both.signal_note().unwrap().ends_with("exit code 1"));
        let killed = ExecOutcome::completed(Some(137), Some(remote_signal("KILL", false, "")));
        assert!(killed.signal_note().unwrap().ends_with("exit code 137"));
        assert_eq!(ExecOutcome::completed(Some(0), None).signal_note(), None);
    }

    // --- 收流状态机(A2-4)与 exec 阶段 ----------------------------------------

    fn data(bytes: &[u8]) -> ChannelMsg {
        ChannelMsg::Data {
            data: bytes.to_vec().into(),
        }
    }

    fn ext_data(bytes: &[u8], ext: u32) -> ChannelMsg {
        ChannelMsg::ExtendedData {
            data: bytes.to_vec().into(),
            ext,
        }
    }

    fn exit_signal(signal_name: Sig, message: &str) -> ChannelMsg {
        ChannelMsg::ExitSignal {
            signal_name,
            core_dumped: false,
            error_message: message.into(),
            lang_tag: String::new(),
        }
    }

    /// 把一串消息喂给收流状态机,返回 (结局;被拒为 None, stdout, stderr)。
    fn feed(messages: Vec<ChannelMsg>) -> (Option<SessionExecOutcome>, Vec<u8>, Vec<u8>) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = {
            let mut collect = |kind: StreamKind, bytes: &[u8]| match kind {
                StreamKind::Stdout => out.extend_from_slice(bytes),
                StreamKind::Stderr => err.extend_from_slice(bytes),
            };
            let mut stream = ExecStream::default();
            let mut rejected = false;
            for msg in messages {
                if stream.on_message(msg, &mut collect) == StreamStep::Rejected {
                    rejected = true;
                    break;
                }
            }
            (!rejected).then(|| stream.finish())
        };
        (outcome, out, err)
    }

    #[test]
    fn exec_stream_collects_output_and_exit_status() {
        let (outcome, out, err) = feed(vec![
            ChannelMsg::Success,
            data(b"hel"),
            ext_data(b"warn", 1),
            data(b"lo"),
            ext_data(b"ignored", 2),
            ChannelMsg::ExitStatus { exit_status: 3 },
            // 退出状态之后还可能来最后一批输出
            data(b"!"),
            ChannelMsg::Eof,
            ChannelMsg::Close,
        ]);
        assert_eq!(
            outcome,
            Some(SessionExecOutcome::Completed {
                exit_code: Some(3),
                signal: None
            })
        );
        assert_eq!(out, b"hello!");
        assert_eq!(err, b"warn");
    }

    #[test]
    fn exec_stream_maps_exit_signal_when_no_status_is_reported() {
        let (outcome, _, _) = feed(vec![exit_signal(Sig::KILL, ""), ChannelMsg::Eof]);
        assert_eq!(
            outcome,
            Some(SessionExecOutcome::Completed {
                exit_code: Some(137),
                signal: Some(remote_signal("KILL", false, "")),
            })
        );

        // 两样都报了:退出码以 exit-status 为准,信号照样记下
        let (outcome, _, _) = feed(vec![
            ChannelMsg::ExitStatus { exit_status: 0 },
            exit_signal(Sig::TERM, "x"),
        ]);
        match outcome {
            Some(SessionExecOutcome::Completed {
                exit_code: Some(0),
                signal: Some(signal),
            }) => assert_eq!(signal.name, "TERM"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn exec_stream_failure_before_any_evidence_is_a_rejection() {
        let (outcome, out, _) = feed(vec![ChannelMsg::Failure, data(b"never")]);
        assert_eq!(outcome, None);
        assert!(out.is_empty());
        // 已经有执行证据之后的 Failure 不能把结局倒回「被拒」
        for evidence in [
            ChannelMsg::Success,
            data(b"x"),
            ChannelMsg::ExitStatus { exit_status: 0 },
        ] {
            let (outcome, _, _) = feed(vec![evidence, ChannelMsg::Failure]);
            assert!(
                matches!(outcome, Some(SessionExecOutcome::Completed { .. })),
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn exec_stream_without_status_or_signal_reports_no_exit_code() {
        let (outcome, _, _) = feed(vec![
            ChannelMsg::Success,
            ChannelMsg::Eof,
            ChannelMsg::Close,
        ]);
        assert_eq!(
            outcome,
            Some(SessionExecOutcome::Completed {
                exit_code: None,
                signal: None
            })
        );
    }

    #[test]
    fn exec_phase_decides_audit_and_session_retirement() {
        use ExecPhase::*;
        assert!(Running.command_may_have_started());
        for phase in [WaitingForSession, OpeningChannel, Dispatching] {
            assert!(!phase.command_may_have_started(), "{phase:?}");
        }
        assert!(OpeningChannel.leaves_session_in_doubt());
        for phase in [WaitingForSession, Dispatching, Running] {
            assert!(!phase.leaves_session_in_doubt(), "{phase:?}");
        }
    }
}
