//! 认证:按 OpenSSH 的习惯组织公钥候选(显式私钥 / ssh-agent / 默认私钥),再试
//! password 与 keyboard-interactive;每种方法的结果都记下来,全部失败时拼成一条可读的
//! 报错(绝不含密码或私钥内容)。
//!
//! 与终端里的系统 ssh 对齐的要点:
//!
//! - 先发 `none` 拿服务器允许的方法表(OpenSSH 同样如此,且 sshd 不把这次计入失败),
//!   之后只试服务器允许的方法;
//! - 私钥加载失败(不存在 / 带口令 / 格式不支持 / `~` 展不开)只记原因,继续试别的;
//! - 带口令的私钥、以及 `IdentityFile` 指向的公钥文件(`key.pub`),若 ssh-agent 里有同一把,
//!   经 agent 签名使用(OpenSSH 的效果相同:它会让 agent 签名而不是弹口令框);
//! - 公钥尝试有上限,保证配了密码时密码一定有机会,见 [`MAX_PUBLICKEY_ATTEMPTS`];服务器的
//!   `MaxAuthTries` 比默认还小、密码没轮到就被断开时,由会话池重连一次只试密码
//!   ([`AuthRound::WithoutKeys`]);
//! - 认证途中会话结束:服务器断开 / 不再给方法算认证失败,还没被拒过就断网算网络错误
//!   (见 [`classify_loss`])。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{MethodKind, MethodSet};

// ssh-agent 客户端单列一组:与上面的 `agent::AgentIdentity` 放在同一组时,
// rustfmt 的 2021 与 2024 两种口径对它俩的先后排法相反。
use russh::keys::agent::client::{AgentClient, AgentStream};

use crate::known_hosts::MtClient;
use crate::pool::AgentSource;
use crate::ssh_config::{IdentityOrigin, IdentitySpec, ResolvedHost};
use crate::transport::StageTracker;

/// sshd 的 `MaxAuthTries` 默认值:失败计到这个数就直接断开连接
/// (「Too many authentication failures」)。一把被拒的公钥记一次失败,
/// `none` 探测不记。
pub(crate) const SSHD_DEFAULT_MAX_AUTH_TRIES: usize = 6;

/// 配了密码时公钥最多试几把:给 password 与 keyboard-interactive 各留一次,
/// 4 + 2 = 6 正好不越过 sshd 默认的 `MaxAuthTries`。超过这个数的公钥直接跳过并在报错里
/// 说明 —— 宁可少试几把 key,也不能让配置好的密码因为连接被断而没机会。
pub(crate) const MAX_PUBLICKEY_ATTEMPTS_WITH_PASSWORD: usize = SSHD_DEFAULT_MAX_AUTH_TRIES - 2;

/// 没配密码时公钥最多试几把:第 6 次失败本来就会被断开,试到第 5 把为止。
pub(crate) const MAX_PUBLICKEY_ATTEMPTS: usize = SSHD_DEFAULT_MAX_AUTH_TRIES - 1;

// 编译期守住上面两条的关系:公钥上限 + password + keyboard-interactive 不越过 sshd 默认的
// MaxAuthTries;没配密码时也停在第 6 次失败之前。
const _: () = assert!(MAX_PUBLICKEY_ATTEMPTS_WITH_PASSWORD + 2 <= SSHD_DEFAULT_MAX_AUTH_TRIES);
const _: () = assert!(MAX_PUBLICKEY_ATTEMPTS < SSHD_DEFAULT_MAX_AUTH_TRIES);

/// 连 agent / 列密钥的上限。本机 IPC 正常毫秒级;卡住就当 agent 不可用,去试下一种方法。
const AGENT_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// keyboard-interactive 最多应答几轮问询(PAM 通常一轮「Password:」就有结论),
/// 防服务器无限追问。
const MAX_KEYBOARD_INTERACTIVE_ROUNDS: usize = 4;

/// 等 russh 会话任务收尾(拿到断开原因)的上限。
const SESSION_END_GRACE: Duration = Duration::from_millis(500);

/// 默认私钥,没有任何显式私钥(连接配置 / ssh config `IdentityFile`)时依次尝试。
/// OpenSSH 的默认表把 `id_rsa` 放最前;这里让 ed25519 打头 —— 现代服务器都接受它,
/// 少一次被拒就离 `MaxAuthTries` 远一步。
pub(crate) const DEFAULT_KEY_FILES: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

/// Windows OpenSSH 的 ssh-agent 服务监听的命名管道。
#[cfg(windows)]
const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

// ============================================================================
// 会话结束原因(由 MtClient::disconnected 记录)
// ============================================================================

/// russh 会话任务结束的原因。认证途中连接被断时,报错据此说清是谁断的、为什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// 服务器主动发了 DISCONNECT(典型是 sshd 的「Too many authentication failures」)。
    ServerDisconnect(String),
    /// 服务器的 USERAUTH_FAILURE 里已经没有可用方法,russh 随即结束会话。
    NoMoreMethods,
    /// 其它传输 / 协议错误。
    Error(String),
}

impl std::fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionEnd::ServerDisconnect(message) => {
                write!(f, "the server closed the connection: {message}")
            }
            SessionEnd::NoMoreMethods => {
                write!(f, "the server has no authentication methods left to offer")
            }
            SessionEnd::Error(message) => write!(f, "the connection failed: {message}"),
        }
    }
}

/// [`MtClient`] 与建连方共享的「旁路信息」:Handler 本身随连接一起被 russh 拿走,
/// 拒绝原因 / 结束原因只能经这里带出来。
#[derive(Default)]
pub(crate) struct ClientNotes {
    /// 主机密钥被拒绝的可读原因。
    pub(crate) rejection: Mutex<Option<String>>,
    /// 会话结束原因。
    pub(crate) ended: Mutex<Option<SessionEnd>>,
}

impl ClientNotes {
    pub(crate) fn take_rejection(&self) -> Option<String> {
        self.rejection.lock().ok().and_then(|mut slot| slot.take())
    }

    pub(crate) fn ended(&self) -> Option<SessionEnd> {
        self.ended.lock().ok().and_then(|slot| slot.clone())
    }
}

// ============================================================================
// 私钥读取
// ============================================================================

/// 私钥文件读取结果。
pub(crate) enum KeyLoad {
    /// 读出的私钥。直接放进 `Arc`:认证时要的就是 `Arc<PrivateKey>`,也免得这个
    /// 几百字节的变体把整个枚举撑大。
    Loaded(Arc<PrivateKey>),
    /// 带口令(加密)的私钥。`public` 是能拿到的公钥(OpenSSH 新格式里公钥是明文存的,
    /// 其它格式看同目录的 `.pub`),用于去 ssh-agent 里找同一把。
    Encrypted {
        public: Option<PublicKey>,
    },
    /// 文件本身是公钥(`IdentityFile ~/.ssh/key.pub`,1Password 等 agent 推荐的写法):
    /// 与带口令的私钥一样,只能到 ssh-agent 里找同一把来签名(OpenSSH 的效果相同)。
    PublicOnly {
        public: PublicKey,
    },
    NotFound,
    Failed(String),
}

/// 读一个私钥文件。只读不改,也不需要文件权限收紧(那是系统 ssh 的要求,见
/// `mt_core::prepare_ssh_key`)。
///
/// russh 原生解析覆盖 OpenSSH / PKCS#8 / PuTTY 明文私钥;传统 PKCS#1 RSA
/// (`-----BEGIN RSA PRIVATE KEY-----`)由 [`try_parse_pkcs1_rsa`] 兜底。带口令的私钥
/// 一律不在这里解密(池拿不到口令),归为 [`KeyLoad::Encrypted`];文件是 OpenSSH 一行式
/// 公钥时归为 [`KeyLoad::PublicOnly`]。
pub(crate) fn load_identity(path: &Path) -> KeyLoad {
    let text = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return KeyLoad::NotFound,
        Err(e) => return KeyLoad::Failed(format!("cannot read the file: {e}")),
    };
    let original = match russh::keys::decode_secret_key(&text, None) {
        Ok(key) => return KeyLoad::Loaded(Arc::new(key)),
        Err(russh::keys::Error::KeyIsEncrypted) => {
            return KeyLoad::Encrypted {
                public: encrypted_public_key(&text, path),
            };
        }
        Err(e) => e,
    };
    if looks_encrypted(&text) {
        return KeyLoad::Encrypted {
            public: encrypted_public_key(&text, path),
        };
    }
    match try_parse_pkcs1_rsa(&text) {
        Ok(Some(key)) => KeyLoad::Loaded(Arc::new(key)),
        Ok(None) => match parse_public_key_file(&text) {
            Some(public) => KeyLoad::PublicOnly { public },
            None => KeyLoad::Failed(format!("unsupported or invalid private key ({original})")),
        },
        Err(e) => KeyLoad::Failed(e),
    }
}

/// `.pub` 文件:第一行非空、非注释的内容按 OpenSSH 一行式公钥(`ssh-ed25519 AAAA… 注释`)
/// 解析。
fn parse_public_key_file(text: &str) -> Option<PublicKey> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))?;
    PublicKey::from_openssh(line).ok()
}

/// 加密的 PKCS#8 / 传统 PEM。russh 对这两种报的不是 `KeyIsEncrypted`,单独认一下。
fn looks_encrypted(text: &str) -> bool {
    text.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----")
        || (text.contains("Proc-Type:") && text.contains("ENCRYPTED"))
}

/// 拿加密私钥的公钥:OpenSSH 新格式直接从私钥文件头取(公钥部分不加密),否则读
/// `<path>.pub`。
fn encrypted_public_key(text: &str, path: &Path) -> Option<PublicKey> {
    if let Ok(key) = russh::keys::ssh_key::PrivateKey::from_openssh(text) {
        return Some(key.public_key().clone());
    }
    let mut pub_path = path.as_os_str().to_owned();
    pub_path.push(".pub");
    russh::keys::load_public_key(PathBuf::from(pub_path)).ok()
}

/// 尝试把传统 **PKCS#1 明文 RSA** PEM(`-----BEGIN RSA PRIVATE KEY-----`)解析成
/// `ssh_key::PrivateKey`。不是 PKCS#1 标签时返回 `Ok(None)`,让调用方回退到原始错误。
///
/// russh 底层 `ssh-key` 只解析 OpenSSH 与 PKCS#8 两种明文私钥;OpenSSH 早期版本及
/// `ssh-keygen -m PEM` / 各类云控制台(如 Oracle Cloud)下发的传统 PKCS#1 RSA 会被它判为
/// `Unsupported key type RSA`(系统 `ssh` 客户端却能正常登录)。这里用纯 Rust 的 `rsa`
/// crate 解析、转成 `ssh_key::PrivateKey`。
///
/// 抽成接受 `&str`、不碰文件系统的纯函数,便于单测直接喂 PEM 字符串。
pub(crate) fn try_parse_pkcs1_rsa(pem: &str) -> Result<Option<PrivateKey>, String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;

    const BEGIN: &str = "-----BEGIN RSA PRIVATE KEY-----";
    const END: &str = "-----END RSA PRIVATE KEY-----";

    // 不是 PKCS#1 标签 —— 交回上层(可能是 SEC1 EC / 加密 OpenSSH 等,本函数不处理)。
    let Some(begin_at) = pem.find(BEGIN) else {
        return Ok(None);
    };
    // 加密的传统 PEM(`Proc-Type: 4,ENCRYPTED`)这里解不了,给可操作指引。
    if pem.contains("Proc-Type:") && pem.contains("ENCRYPTED") {
        return Err(
            "key is an encrypted PKCS#1 RSA private key (Proc-Type: 4,ENCRYPTED); \
            passphrase keys are not supported — decrypt it first \
            (`openssl rsa -in <key> -out <key.dec>` or re-export without a passphrase)"
                .into(),
        );
    }

    // 自剥 PEM -> DER:rsa 0.10 没有 `pem` feature(PEM 方法 gated 在 pkcs1/pem,
    // rsa 未传递),故不能用 from_pkcs1_pem;改取 BEGIN/END 之间的 base64 主体、
    // 去掉所有空白后解码成 DER,再走不依赖 pem feature 的 from_pkcs1_der。
    let body_start = begin_at + BEGIN.len();
    let body_end = pem
        .find(END)
        .filter(|&e| e >= body_start)
        .ok_or_else(|| "PKCS#1 RSA PEM missing END marker".to_string())?;
    let b64: String = pem[body_start..body_end].split_whitespace().collect();
    let der = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| format!("invalid base64 in PKCS#1 RSA PEM: {e}"))?
    };

    let rsa_key = rsa::RsaPrivateKey::from_pkcs1_der(&der)
        .map_err(|e| format!("invalid PKCS#1 RSA private key: {e}"))?;
    let keypair = russh::keys::ssh_key::private::RsaKeypair::try_from(&rsa_key)
        .map_err(|e| format!("cannot convert RSA key into ssh-key form: {e}"))?;
    Ok(Some(PrivateKey::from(keypair)))
}

// ============================================================================
// ssh-agent
// ============================================================================

type DynAgentClient = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

/// agent 里的一把公钥(证书身份不用:本实现只做普通公钥认证)。
#[derive(Clone)]
struct AgentKey {
    key: PublicKey,
    comment: String,
}

enum AgentState {
    /// 还没连过(只在真的需要时才连)。
    NotConnected,
    /// 不可用;`None` 表示被配置禁用(不必写进报错)。
    Unavailable(Option<String>),
    Ready {
        client: DynAgentClient,
        keys: Vec<AgentKey>,
    },
}

/// 一次建连里共用的 ssh-agent 连接(ProxyJump 的每一跳都用同一个),懒连接。
pub(crate) struct AgentSession {
    source: AgentSource,
    state: AgentState,
}

impl AgentSession {
    pub(crate) fn new(source: AgentSource) -> Self {
        Self {
            source,
            state: AgentState::NotConnected,
        }
    }

    async fn ensure_connected(&mut self) {
        if !matches!(self.state, AgentState::NotConnected) {
            return;
        }
        self.state = match connect_agent(&self.source).await {
            Ok(None) => AgentState::Unavailable(None),
            Err(reason) => AgentState::Unavailable(Some(reason)),
            Ok(Some(mut client)) => {
                match tokio::time::timeout(AGENT_IO_TIMEOUT, client.request_identities()).await {
                    Ok(Ok(identities)) => AgentState::Ready {
                        client,
                        keys: identities
                            .into_iter()
                            .filter_map(|identity| match identity {
                                AgentIdentity::PublicKey { key, comment } => {
                                    Some(AgentKey { key, comment })
                                }
                                AgentIdentity::Certificate { .. } => None,
                            })
                            .collect(),
                    },
                    Ok(Err(e)) => {
                        AgentState::Unavailable(Some(format!("listing identities failed: {e}")))
                    }
                    Err(_) => AgentState::Unavailable(Some("listing identities timed out".into())),
                }
            }
        };
    }

    fn holds(&self, key: &PublicKey) -> bool {
        match &self.state {
            AgentState::Ready { keys, .. } => {
                keys.iter().any(|k| k.key.key_data() == key.key_data())
            }
            _ => false,
        }
    }

    fn client_mut(&mut self) -> Option<&mut DynAgentClient> {
        match &mut self.state {
            AgentState::Ready { client, .. } => Some(client),
            _ => None,
        }
    }

    /// 「只能经 agent 用」的 key 没找到时附在说明后面的 agent 状况(被配置禁用时为空)。
    fn unavailable_note(&self) -> String {
        match &self.state {
            AgentState::Unavailable(Some(reason)) => {
                format!(" (ssh-agent not available: {reason})")
            }
            _ => String::new(),
        }
    }
}

/// 连 agent。`Ok(None)` = 配置为不用 agent。
async fn connect_agent(source: &AgentSource) -> Result<Option<DynAgentClient>, String> {
    match source {
        AgentSource::Disabled => Ok(None),
        AgentSource::System => connect_system_agent().await.map(Some),
        AgentSource::Endpoint(path) => connect_endpoint(path)
            .await
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
    }
}

/// Windows 上要试的 OpenSSH agent 命名管道:`SSH_AUTH_SOCK` 指向命名管道时先试它
/// (Windows 版 OpenSSH 同样认),再试 agent 服务的默认管道。Git Bash 之类环境里
/// `SSH_AUTH_SOCK` 常是 MSYS 的 Unix socket 路径(`/tmp/ssh-…/agent.N`),那不是命名
/// 管道,跳过。抽成纯函数便于单测。
#[cfg(windows)]
fn windows_agent_pipes(ssh_auth_sock: Option<&str>) -> Vec<PathBuf> {
    let mut pipes = Vec::new();
    if let Some(sock) = ssh_auth_sock.map(str::trim) {
        let lowered = sock.replace('/', "\\").to_ascii_lowercase();
        if lowered.starts_with(r"\\.\pipe\") {
            pipes.push(PathBuf::from(sock));
        }
    }
    if !pipes
        .iter()
        .any(|p| p.as_os_str().eq_ignore_ascii_case(OPENSSH_AGENT_PIPE))
    {
        pipes.push(PathBuf::from(OPENSSH_AGENT_PIPE));
    }
    pipes
}

/// Windows:先连 OpenSSH 的 agent(见 [`windows_agent_pipes`]),都连不上再试 Pageant。
#[cfg(windows)]
async fn connect_system_agent() -> Result<DynAgentClient, String> {
    let ssh_auth_sock = std::env::var("SSH_AUTH_SOCK").ok();
    let mut errors = Vec::new();
    for pipe in windows_agent_pipes(ssh_auth_sock.as_deref()) {
        match connect_endpoint(&pipe).await {
            Ok(client) => return Ok(client),
            Err(e) => errors.push(format!("{}: {e}", pipe.display())),
        }
    }
    let pageant = tokio::time::timeout(AGENT_IO_TIMEOUT, AgentClient::connect_pageant()).await;
    match pageant {
        Ok(Ok(client)) => {
            let client: DynAgentClient = client.dynamic();
            return Ok(client);
        }
        Ok(Err(e)) => errors.push(format!("Pageant: {e}")),
        Err(_) => errors.push("Pageant: no response".into()),
    }
    Err(errors.join("; "))
}

/// Unix:`SSH_AUTH_SOCK`。
#[cfg(unix)]
async fn connect_system_agent() -> Result<DynAgentClient, String> {
    match std::env::var_os("SSH_AUTH_SOCK").filter(|value| !value.is_empty()) {
        Some(socket) => {
            let path = PathBuf::from(socket);
            connect_endpoint(&path)
                .await
                .map_err(|e| format!("{}: {e}", path.display()))
        }
        None => Err("SSH_AUTH_SOCK is not set".into()),
    }
}

#[cfg(not(any(unix, windows)))]
async fn connect_system_agent() -> Result<DynAgentClient, String> {
    Err("ssh-agent is not supported on this platform".into())
}

#[cfg(windows)]
async fn connect_endpoint(path: &Path) -> Result<DynAgentClient, String> {
    match tokio::time::timeout(AGENT_IO_TIMEOUT, AgentClient::connect_named_pipe(path)).await {
        Ok(Ok(client)) => {
            let client: DynAgentClient = client.dynamic();
            Ok(client)
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("no response".into()),
    }
}

#[cfg(unix)]
async fn connect_endpoint(path: &Path) -> Result<DynAgentClient, String> {
    match tokio::time::timeout(AGENT_IO_TIMEOUT, AgentClient::connect_uds(path)).await {
        Ok(Ok(client)) => {
            let client: DynAgentClient = client.dynamic();
            Ok(client)
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("no response".into()),
    }
}

#[cfg(not(any(unix, windows)))]
async fn connect_endpoint(_path: &Path) -> Result<DynAgentClient, String> {
    Err("ssh-agent is not supported on this platform".into())
}

/// 把 agent 签名接到 russh 上。签名失败时**不**返回错误,而是交回一个注定验不过的占位
/// 签名,原因见 [`with_placeholder_signature`]。
struct AgentSigner<'a> {
    client: &'a mut DynAgentClient,
    /// 签名失败的原因(此时交给 russh 的是占位签名)。
    failure: Option<String>,
}

impl russh::Signer for AgentSigner<'_> {
    type Error = russh::AgentAuthError;

    #[allow(clippy::manual_async_fn)]
    fn auth_sign(
        &mut self,
        key: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        to_sign: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        async move {
            match self
                .client
                .sign_request(key, hash_alg, to_sign.clone())
                .await
            {
                Ok(signed) => Ok(signed),
                Err(e) => {
                    self.failure = Some(e.to_string());
                    Ok(with_placeholder_signature(to_sign, key, hash_alg))
                }
            }
        }
    }
}

fn push_ssh_string(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(bytes);
}

/// agent 签名失败时补一个格式正确、内容全零、**注定验不过**的占位签名。
///
/// 服务器回 PK_OK 后,russh 的会话任务在处理那个包的途中原地等 `Msg::Signed`;
/// signer 返回 `Err` 时 `authenticate_publickey_with` 直接把错误交回,会话任务却仍卡在
/// 那里等签名 —— 之后发来的任何认证请求都会被它当成无关消息吞掉,整条连接再也认证不了,
/// 配了密码也没机会。交回占位签名则一切照常:服务器验签失败、回 USERAUTH_FAILURE,
/// 会话状态机回到正轨,下一种方法照常进行。代价是服务器多记一次公钥失败,与「这把
/// key 被拒」等价。
fn with_placeholder_signature(
    mut data: Vec<u8>,
    key: &AgentIdentity,
    hash_alg: Option<HashAlg>,
) -> Vec<u8> {
    let algorithm = match key {
        AgentIdentity::PublicKey { key, .. } => key.algorithm(),
        AgentIdentity::Certificate { certificate, .. } => certificate.algorithm(),
    };
    let algorithm = match algorithm {
        Algorithm::Rsa { .. } => Algorithm::Rsa { hash: hash_alg },
        other => other,
    };
    let signature: Vec<u8> = match algorithm {
        // ECDSA 的签名体是 r、s 两个 mpint:给 r = s = 1,格式合法、验签必败。格式不合法的
        // 签名在有的服务器(如 russh)上会被当成协议错误直接断开,就又没有下一种方法了。
        Algorithm::Ecdsa { .. } => vec![0, 0, 0, 1, 1, 0, 0, 0, 1, 1],
        // ed25519 固定 64 字节;RSA 不限长度(服务器按模长校验,对不上即拒)。
        _ => vec![0u8; 64],
    };
    let mut blob = Vec::new();
    push_ssh_string(&mut blob, algorithm.as_str().as_bytes());
    push_ssh_string(&mut blob, &signature);
    push_ssh_string(&mut data, &blob);
    data
}

// ============================================================================
// 认证流程
// ============================================================================

/// 配置里的密码,已由调用方(`pool::reveal_password`,唯一解封点)解开。
pub(crate) enum PasswordInput {
    /// 连接没配密码。
    NotConfigured,
    /// 跳板:密码属于目标连接,有意不用于跳板(只用 key 登录跳板)。
    NotForJumpHost,
    /// 已存密码解不开(信封换了钥匙 / 被改过)。绝不把密文当密码送出去。
    Unavailable(String),
    Available(String),
}

/// 跳板只允许密码类方法时的说明:密码有意不用于跳板,报错要写清楚是跳板认证失败、该怎么办。
const JUMP_HOST_PASSWORD_NOTE: &str = "password: not used for jump hosts (mini-term signs in to \
     a jump host with keys only: ssh-agent, an IdentityFile for the jump host in ssh config, or \
     the default keys ~/.ssh/id_*; the saved password belongs to the target connection)";

/// 这一跳是第几轮认证。
pub(crate) enum AuthRound {
    /// 按 OpenSSH 的顺序:公钥,再密码 / keyboard-interactive。
    First,
    /// 第一轮还没轮到密码就被服务器断开了(加固过的 sshd 把 `MaxAuthTries` 设成 3、4,
    /// agent 里 key 一多就会这样),重连后的这一轮:不再出示公钥,直接密码与
    /// keyboard-interactive。`earlier` 是第一轮的逐方法结果,拼进这一轮的报错。
    WithoutKeys { earlier: Vec<String> },
}

/// 一跳的认证计划。
pub(crate) struct AuthPlan<'a> {
    pub(crate) host: &'a ResolvedHost,
    /// 报错里的身份描述(`deploy@example.com:22` / `jump host jumper@bastion:22`)。
    pub(crate) label: String,
    pub(crate) password: &'a PasswordInput,
    /// 默认私钥所在目录(`~/.ssh`);home 未知时为 `None`,不试默认私钥。
    pub(crate) default_key_dir: Option<PathBuf>,
    pub(crate) round: AuthRound,
}

/// 认证失败的分类。
#[derive(Debug)]
pub(crate) enum AuthError {
    /// 服务器拒绝了全部可用方法 / 根本没有可用的凭据 / 认证途中被服务器断开:计入认证失败
    /// 冷却。
    Rejected(String),
    /// 还没被服务器拒过就断了连接(断网之类):按网络错误处理,不计冷却。
    Transport(String),
    /// 第一轮在密码轮到之前就被服务器断开 / 不再给方法:值得重连一次、跳过公钥只试密码
    /// (见 [`AuthRound::WithoutKeys`])。`message` 是不重连时的报错,`notes` 是第一轮的
    /// 逐方法结果。调用方不重连就按 [`AuthError::Rejected`] 处理。
    RetryWithoutKeys { message: String, notes: Vec<String> },
}

/// 每种方法的尝试结果,全部失败时拼成报错。
struct AuthReport {
    label: String,
    server_methods: Option<String>,
    notes: Vec<String>,
    /// 是否已有方法被服务器以 USERAUTH_FAILURE 明确拒绝过(`none` 探测不算)。决定连接
    /// 意外中断(没有 DISCONNECT 消息)时按认证失败还是网络错误算:拒过之后连接没了,多半是
    /// 服务器到了失败次数上限直接关连接(dropbear 就不发 DISCONNECT),按认证失败冷却才不会
    /// 招来封禁;一次都没拒过就断了,才是断网。
    rejected_any: bool,
}

impl AuthReport {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            server_methods: None,
            notes: Vec::new(),
            rejected_any: false,
        }
    }

    fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// 英文开头,便于 agent 识别;只含方法、路径、指纹与原因,不含任何凭据。
    fn render(&self) -> String {
        let methods = self.server_methods.as_deref().unwrap_or("unknown");
        if self.notes.is_empty() {
            format!(
                "authentication failed for {} (server allows: {methods}): no usable credential \
                 (no identity file, no ssh-agent identity, no default key, no password)",
                self.label
            )
        } else {
            format!(
                "authentication failed for {} (server allows: {methods}): {}",
                self.label,
                self.notes.join("; ")
            )
        }
    }

    /// 还没被服务器拒过就断了连接时的报错(网络错误,不计冷却)。
    fn render_connection_lost(&self, why: &str) -> String {
        let mut message = format!(
            "connection lost during authentication on {}: {why}",
            self.label
        );
        if !self.notes.is_empty() {
            message.push_str(&format!(" (before that: {})", self.notes.join("; ")));
        }
        message
    }
}

fn method_names(methods: &MethodSet) -> String {
    let names: Vec<&str> = methods.iter().map(<&str>::from).collect();
    names.join(",")
}

/// 公钥候选。
enum KeySource {
    Local(Arc<PrivateKey>),
    Agent(PublicKey),
}

struct KeyCandidate {
    label: String,
    source: KeySource,
}

impl KeyCandidate {
    fn public(&self) -> &PublicKey {
        match &self.source {
            KeySource::Local(key) => key.public_key(),
            KeySource::Agent(key) => key,
        }
    }
}

fn describe_identity(spec: &IdentitySpec) -> String {
    match spec.origin {
        IdentityOrigin::Connection => {
            format!("identity file {} (connection setting)", spec.display)
        }
        IdentityOrigin::SshConfig => {
            format!("identity file {} (ssh config IdentityFile)", spec.display)
        }
    }
}

fn describe_agent_key(key: &AgentKey) -> String {
    let fingerprint = key.key.fingerprint(HashAlg::Sha256);
    let algorithm = key.key.algorithm();
    if key.comment.is_empty() {
        format!("ssh-agent key {} {fingerprint}", algorithm.as_str())
    } else {
        format!(
            "ssh-agent key {} {fingerprint} ({})",
            algorithm.as_str(),
            key.comment
        )
    }
}

/// 按 OpenSSH 的习惯排公钥候选并去重(同一把公钥只试一次):
/// - 有显式私钥:显式私钥在前,agent 里的其余 key 在后;
/// - 没有:agent 的 key 在前,默认私钥在后。
///
/// `IdentitiesOnly yes` 时调用方传空的 `agent_keys`(经 agent 签名的加密显式私钥不受影响,
/// 它们已经在 `explicit` / `defaults` 里)。抽成纯函数便于单测。
fn assemble_candidates(
    explicit: Vec<KeyCandidate>,
    agent_keys: Vec<KeyCandidate>,
    defaults: Vec<KeyCandidate>,
    has_explicit: bool,
) -> Vec<KeyCandidate> {
    let ordered: Vec<KeyCandidate> = if has_explicit {
        explicit.into_iter().chain(agent_keys).collect()
    } else {
        agent_keys.into_iter().chain(defaults).collect()
    };
    let mut out: Vec<KeyCandidate> = Vec::with_capacity(ordered.len());
    for candidate in ordered {
        let duplicate = out
            .iter()
            .any(|seen| seen.public().key_data() == candidate.public().key_data());
        if !duplicate {
            out.push(candidate);
        }
    }
    out
}

/// 读一个私钥文件变成候选;读不了只记原因。`quiet_if_missing` 用于默认私钥:不存在
/// 很正常,不必写进报错。
async fn file_candidate(
    path: &Path,
    label: String,
    quiet_if_missing: bool,
    agent: &mut AgentSession,
    report: &mut AuthReport,
) -> Option<KeyCandidate> {
    match load_identity(path) {
        KeyLoad::Loaded(key) => Some(KeyCandidate {
            label,
            source: KeySource::Local(key),
        }),
        KeyLoad::Encrypted { public } => {
            if let Some(public) = public {
                agent.ensure_connected().await;
                if agent.holds(&public) {
                    return Some(KeyCandidate {
                        label: format!("{label} (signed by ssh-agent)"),
                        source: KeySource::Agent(public),
                    });
                }
            }
            report.note(format!(
                "{label}: encrypted with a passphrase and not loaded in ssh-agent (passphrase-protected \
                 keys are only usable through ssh-agent: run `ssh-add` on it){}",
                agent.unavailable_note()
            ));
            None
        }
        KeyLoad::PublicOnly { public } => {
            agent.ensure_connected().await;
            if agent.holds(&public) {
                return Some(KeyCandidate {
                    label: format!("{label} (public key, signed by ssh-agent)"),
                    source: KeySource::Agent(public),
                });
            }
            report.note(format!(
                "{label}: is a public key, and ssh-agent does not hold the matching private key (a \
                 public key IdentityFile is only usable through ssh-agent){}",
                agent.unavailable_note()
            ));
            None
        }
        KeyLoad::NotFound if quiet_if_missing => None,
        KeyLoad::NotFound => {
            report.note(format!("{label}: file not found"));
            None
        }
        KeyLoad::Failed(reason) => {
            report.note(format!("{label}: cannot load ({reason})"));
            None
        }
    }
}

/// agent 里的 key 变成候选;agent 不可用 / 没有 key 时记一条说明。
async fn agent_candidates(agent: &mut AgentSession, report: &mut AuthReport) -> Vec<KeyCandidate> {
    agent.ensure_connected().await;
    match &agent.state {
        AgentState::Ready { keys, .. } if keys.is_empty() => {
            report.note("ssh-agent: no identities loaded");
            Vec::new()
        }
        AgentState::Ready { keys, .. } => keys
            .iter()
            .map(|key| KeyCandidate {
                label: describe_agent_key(key),
                source: KeySource::Agent(key.key.clone()),
            })
            .collect(),
        AgentState::Unavailable(Some(reason)) => {
            report.note(format!("ssh-agent: not available ({reason})"));
            Vec::new()
        }
        AgentState::Unavailable(None) | AgentState::NotConnected => Vec::new(),
    }
}

async fn collect_candidates(
    plan: &AuthPlan<'_>,
    agent: &mut AgentSession,
    report: &mut AuthReport,
) -> Vec<KeyCandidate> {
    let host = plan.host;
    let mut explicit = Vec::new();
    for spec in &host.identities {
        let label = describe_identity(spec);
        match &spec.path {
            Ok(path) => {
                if let Some(candidate) = file_candidate(path, label, false, agent, report).await {
                    explicit.push(candidate);
                }
            }
            Err(reason) => report.note(format!("{label}: {reason}")),
        }
    }
    // agent 的 key 在显式私钥之后 / 默认私钥之前;IdentitiesOnly 时不用。
    let agent_keys = if host.identities_only {
        Vec::new()
    } else {
        agent_candidates(agent, report).await
    };
    let mut defaults = Vec::new();
    if !host.explicit_identities {
        match &plan.default_key_dir {
            Some(dir) => {
                let mut any_exists = false;
                for name in DEFAULT_KEY_FILES {
                    let path = dir.join(name);
                    any_exists |= path.exists();
                    let label = format!("default key ~/.ssh/{name}");
                    if let Some(candidate) = file_candidate(&path, label, true, agent, report).await
                    {
                        defaults.push(candidate);
                    }
                }
                if !any_exists {
                    report.note(format!(
                        "default keys: none found in {} ({})",
                        dir.display(),
                        DEFAULT_KEY_FILES.join(", ")
                    ));
                }
            }
            None => report.note("default keys: home directory unknown, not tried"),
        }
    }
    assemble_candidates(explicit, agent_keys, defaults, host.explicit_identities)
}

/// 本次连接里服务器偏好的 RSA 签名哈希(查一次缓存起来)。服务器没发 server-sig-algs
/// 时退 SHA-512:现代服务器普遍接受,而 `new(key, None)` 会落到被 OpenSSH ≥ 8.8 默认
/// 禁用的 ssh-rsa(SHA-1)。查询最多等 1s EXT_INFO —— 只在真有 RSA key 要试时才查。
async fn rsa_hash(
    handle: &Handle<MtClient>,
    cache: &mut Option<Option<HashAlg>>,
) -> Option<HashAlg> {
    if let Some(hash) = *cache {
        return hash;
    }
    let hash = match handle.best_supported_rsa_hash().await {
        Ok(Some(alg)) => alg,
        Ok(None) | Err(_) => Some(HashAlg::Sha512),
    };
    *cache = Some(hash);
    hash
}

/// 认证途中会话结束了:等 russh 会话任务收尾,取出 [`MtClient`] 记下的结束原因。
async fn wait_session_end(
    handle: &mut Handle<MtClient>,
    notes: &ClientNotes,
) -> Option<SessionEnd> {
    // Handle 自身就是会话任务 JoinHandle 的 Future:等它结束,MtClient::disconnected
    // 记下的原因才一定已经落地。认证失败的 handle 随后就被丢弃,不会被二次 poll。
    let _ = tokio::time::timeout(SESSION_END_GRACE, &mut *handle).await;
    notes.ended()
}

/// 认证途中会话结束了,按结束原因分类(纯函数便于单测)。`attempt` 是当时在等回复的那次
/// 尝试(`none` 探测时为 `None`)。
///
/// - [`SessionEnd::NoMoreMethods`]:服务器以 USERAUTH_FAILURE 明确拒绝,且没有方法可试了;
/// - [`SessionEnd::ServerDisconnect`]:服务器主动断开(典型是 sshd 的「Too many
///   authentication failures」)。
///
/// 这两种算认证失败(计入冷却)。其余(IO 断开、原因不明)只在之前已有尝试被明确拒绝过时
/// 才算认证失败(见 [`AuthReport::rejected_any`]),否则是断网:网络错误、不计冷却,报错写
/// 「connection lost during authentication」。russh 0.61 在会话结束时把等回复的请求报成
/// 「剩余方法为空的 Failure」,与真正的「没有剩余方法」长得一样,只能靠结束原因区分。
fn classify_loss(
    report: &mut AuthReport,
    attempt: Option<&str>,
    ended: Option<SessionEnd>,
) -> AuthError {
    let with_attempt = |what: String| match attempt {
        Some(attempt) => format!("{attempt}: {what}"),
        None => what,
    };
    match ended {
        Some(SessionEnd::NoMoreMethods) => {
            report.note(match attempt {
                Some(attempt) => {
                    format!("{attempt}: rejected by server (no authentication methods left)")
                }
                None => SessionEnd::NoMoreMethods.to_string(),
            });
            AuthError::Rejected(report.render())
        }
        Some(end @ SessionEnd::ServerDisconnect(_)) => {
            report.note(with_attempt(format!("no answer, {end}")));
            AuthError::Rejected(report.render())
        }
        other => {
            let why = other.map_or_else(
                || "the connection was closed".to_string(),
                |end| end.to_string(),
            );
            if report.rejected_any {
                report.note(with_attempt(format!("no answer, {why}")));
                AuthError::Rejected(report.render())
            } else {
                AuthError::Transport(report.render_connection_lost(&why))
            }
        }
    }
}

/// 一次公钥 / 密码尝试的结论。
enum Step {
    Success,
    /// 被服务器明确拒绝,服务器还允许这些方法。
    Rejected(MethodSet),
    /// 没有回复:会话结束了(或服务器回了「再无可用方法」,russh 随即结束会话 —— 两者在
    /// russh 0.61 里都表现为剩余方法为空的 Failure),按 [`classify_loss`] 收尾。
    Ended,
}

fn step_of(result: Result<AuthResult, String>) -> Step {
    match result {
        Ok(AuthResult::Success) => Step::Success,
        Ok(AuthResult::Failure {
            remaining_methods, ..
        }) if remaining_methods.is_empty() => Step::Ended,
        Ok(AuthResult::Failure {
            remaining_methods, ..
        }) => Step::Rejected(remaining_methods),
        Err(_) => Step::Ended,
    }
}

/// 第一轮在公钥阶段被服务器断开 / 拒绝到无路可走,而配置的密码还没轮到:换成
/// [`AuthError::RetryWithoutKeys`],让调用方重连一次只试密码。服务器一开始就不收密码类
/// 方法、或已经是重连后的那一轮时原样返回。
fn offer_retry_without_keys(
    error: AuthError,
    plan: &AuthPlan<'_>,
    initial_methods: &MethodSet,
    report: &AuthReport,
) -> AuthError {
    let password_accepted = initial_methods.contains(&MethodKind::Password)
        || initial_methods.contains(&MethodKind::KeyboardInteractive);
    match error {
        AuthError::Rejected(message)
            if password_accepted
                && matches!(plan.round, AuthRound::First)
                && matches!(plan.password, PasswordInput::Available(_)) =>
        {
            AuthError::RetryWithoutKeys {
                message,
                notes: report.notes.clone(),
            }
        }
        other => other,
    }
}

/// 认证一跳。`notes` 是这条连接的 [`MtClient`] 共享出来的旁路信息。
pub(crate) async fn authenticate_host(
    handle: &mut Handle<MtClient>,
    plan: &AuthPlan<'_>,
    agent: &mut AgentSession,
    notes: &ClientNotes,
    tracker: &StageTracker,
) -> Result<(), AuthError> {
    let user = plan.host.user.as_str();
    let mut report = AuthReport::new(&plan.label);
    let offer_keys = match &plan.round {
        AuthRound::First => true,
        AuthRound::WithoutKeys { earlier } => {
            for note in earlier {
                report.note(note.clone());
            }
            report.note(
                "reconnected to try the password without offering public keys (the server \
                 closed the previous connection before the password's turn)",
            );
            // 上一轮已经被服务器拒过 / 断开过:这一轮即便断网也按认证失败算,免得下一次
            // 请求立刻把公钥那一轮整个重来一遍。
            report.rejected_any = true;
            false
        }
    };

    // 1) `none`:拿服务器允许的方法表(sshd 不把首次 none 计入失败)。
    tracker.set_detail("waiting for the server's list of authentication methods");
    let none = handle
        .authenticate_none(user)
        .await
        .map_err(|e| e.to_string());
    let initial_methods = match step_of(none) {
        Step::Success => return Ok(()),
        Step::Rejected(methods) => methods,
        Step::Ended => {
            let ended = wait_session_end(handle, notes).await;
            return Err(classify_loss(&mut report, None, ended));
        }
    };
    let mut methods = initial_methods.clone();
    report.server_methods = Some(method_names(&methods));

    // 2) 公钥。
    let cap = match plan.password {
        PasswordInput::Available(_) => MAX_PUBLICKEY_ATTEMPTS_WITH_PASSWORD,
        _ => MAX_PUBLICKEY_ATTEMPTS,
    };
    if offer_keys && methods.contains(&MethodKind::PublicKey) {
        let candidates = collect_candidates(plan, agent, &mut report).await;
        let total = candidates.len();
        let mut rsa_hash_cache: Option<Option<HashAlg>> = None;
        for (index, candidate) in candidates.into_iter().enumerate() {
            if !methods.contains(&MethodKind::PublicKey) {
                break;
            }
            if index >= cap {
                report.note(format!(
                    "publickey: {} more key(s) not tried — stopped after {cap} so the server's \
                     MaxAuthTries (default {SSHD_DEFAULT_MAX_AUTH_TRIES}) is not exceeded{}",
                    total - index,
                    if cap == MAX_PUBLICKEY_ATTEMPTS_WITH_PASSWORD {
                        " and the password still gets its turn"
                    } else {
                        ""
                    }
                ));
                break;
            }
            tracker.set_detail(format!("waiting for: publickey {}", candidate.label));
            let hash = if candidate.public().algorithm().is_rsa() {
                rsa_hash(handle, &mut rsa_hash_cache).await
            } else {
                None
            };
            let mut sign_failure = None;
            let result = match &candidate.source {
                KeySource::Local(key) => handle
                    .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::clone(key), hash))
                    .await
                    .map_err(|e| e.to_string()),
                KeySource::Agent(public) => {
                    let Some(client) = agent.client_mut() else {
                        report.note(format!("{}: ssh-agent connection lost", candidate.label));
                        continue;
                    };
                    let mut signer = AgentSigner {
                        client,
                        failure: None,
                    };
                    let result = handle
                        .authenticate_publickey_with(user, public.clone(), hash, &mut signer)
                        .await
                        .map_err(|e| e.to_string());
                    sign_failure = signer.failure.take();
                    result
                }
            };
            let partial = matches!(
                &result,
                Ok(AuthResult::Failure {
                    partial_success: true,
                    ..
                })
            );
            match step_of(result) {
                Step::Success => return Ok(()),
                Step::Rejected(remaining) => {
                    // 部分成功(这把 key 被接受、还要再验别的)不算被拒。
                    report.rejected_any |= !partial;
                    report.note(match (&sign_failure, partial) {
                        (Some(why), _) => {
                            format!("{}: ssh-agent failed to sign ({why})", candidate.label)
                        }
                        (None, true) => format!(
                            "{}: accepted, but the server requires further authentication",
                            candidate.label
                        ),
                        (None, false) => format!("{}: rejected by server", candidate.label),
                    });
                    methods = remaining;
                }
                Step::Ended => {
                    let ended = wait_session_end(handle, notes).await;
                    let error = classify_loss(&mut report, Some(&candidate.label), ended);
                    return Err(offer_retry_without_keys(
                        error,
                        plan,
                        &initial_methods,
                        &report,
                    ));
                }
            }
        }
    }

    // 3) 密码:password 方法,再 keyboard-interactive(有的服务器只开后者)。
    match plan.password {
        PasswordInput::NotConfigured => {
            if methods.contains(&MethodKind::Password)
                || methods.contains(&MethodKind::KeyboardInteractive)
            {
                report.note("password: not configured");
            }
        }
        PasswordInput::NotForJumpHost => {
            if methods.contains(&MethodKind::Password)
                || methods.contains(&MethodKind::KeyboardInteractive)
            {
                report.note(JUMP_HOST_PASSWORD_NOTE);
            }
        }
        PasswordInput::Unavailable(reason) => {
            report.note(format!("password: stored password unavailable ({reason})"));
        }
        PasswordInput::Available(password) => {
            let mut tried = false;
            if methods.contains(&MethodKind::Password) {
                tried = true;
                tracker.set_detail("waiting for: password");
                let result = handle
                    .authenticate_password(user, password.as_str())
                    .await
                    .map_err(|e| e.to_string());
                match step_of(result) {
                    Step::Success => return Ok(()),
                    Step::Rejected(remaining) => {
                        report.rejected_any = true;
                        report.note("password: rejected by server");
                        methods = remaining;
                    }
                    Step::Ended => {
                        let ended = wait_session_end(handle, notes).await;
                        return Err(classify_loss(&mut report, Some("password"), ended));
                    }
                }
            }
            if methods.contains(&MethodKind::KeyboardInteractive) {
                tried = true;
                tracker.set_detail("waiting for: keyboard-interactive");
                match keyboard_interactive(handle, user, password).await {
                    KbdOutcome::Success => return Ok(()),
                    KbdOutcome::Rejected => {
                        report.rejected_any = true;
                        report.note("keyboard-interactive: rejected by server");
                    }
                    KbdOutcome::TooManyPrompts => {
                        report.rejected_any = true;
                        report.note(format!(
                            "keyboard-interactive: gave up after {MAX_KEYBOARD_INTERACTIVE_ROUNDS} \
                             rounds of prompts (the stored password is the only answer mini-term \
                             can give)"
                        ));
                    }
                    KbdOutcome::Ended => {
                        let ended = wait_session_end(handle, notes).await;
                        return Err(classify_loss(
                            &mut report,
                            Some("keyboard-interactive"),
                            ended,
                        ));
                    }
                }
            }
            if !tried {
                report.note(
                    "password: configured, but the server accepts neither password nor \
                     keyboard-interactive authentication",
                );
            }
        }
    }

    Err(AuthError::Rejected(report.render()))
}

enum KbdOutcome {
    Success,
    Rejected,
    TooManyPrompts,
    /// 没有回复(会话结束,或服务器回了「再无可用方法」),含义同 [`Step::Ended`]。
    Ended,
}

/// keyboard-interactive:每个提示都用配置的密码作答。服务器若问的是 OTP 之类,自然
/// 失败 —— 这是设计意图,不要瞎猜。
async fn keyboard_interactive(
    handle: &mut Handle<MtClient>,
    user: &str,
    password: &str,
) -> KbdOutcome {
    let mut state = match handle
        .authenticate_keyboard_interactive_start(user, None)
        .await
    {
        Ok(state) => state,
        Err(_) => return KbdOutcome::Ended,
    };
    let mut rounds = 0;
    loop {
        match state {
            KeyboardInteractiveAuthResponse::Success => return KbdOutcome::Success,
            KeyboardInteractiveAuthResponse::Failure {
                remaining_methods, ..
            } => {
                return if remaining_methods.is_empty() {
                    KbdOutcome::Ended
                } else {
                    KbdOutcome::Rejected
                };
            }
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                // 不带提示的空问询也算一轮:有的服务器先发一轮空的,但不能无限循环下去。
                rounds += 1;
                if rounds > MAX_KEYBOARD_INTERACTIVE_ROUNDS {
                    return KbdOutcome::TooManyPrompts;
                }
                let answers: Vec<String> = prompts.iter().map(|_| password.to_string()).collect();
                state = match handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                {
                    Ok(state) => state,
                    Err(_) => return KbdOutcome::Ended,
                };
            }
        }
    }
}

// ============================================================================
// tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mt-ssh-auth-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 由固定种子构造的 ed25519 私钥(不经 rng)。
    fn seeded_key(seed: u8) -> PrivateKey {
        let keypair = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]);
        PrivateKey::from(keypair)
    }

    fn write_openssh_key(path: &Path, key: &PrivateKey) {
        let text = key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap();
        std::fs::write(path, text.as_bytes()).unwrap();
    }

    /// `ssh-keygen -t ed25519 -N mt-ssh-test` 一次性生成的**测试专用废弃密钥**(口令
    /// `mt-ssh-test`),只用于验证「带口令的私钥」分支,不对应任何真实主机 / 账号。
    const ENCRYPTED_ED25519_FIXTURE: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABA75HILGL
APMrA1XbjP6MESAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIBesQO+amJx6a3fp
PLZOFM3wPp281cA8v6Hp64YXCl5BAAAAoCWh+qD6Zu3kDVRYifuttM1K9ePMit4hediMNY
2HzoAjXBsTe/o6klBk7BpoKIWnd/xP43UUIIA8xum0M0SF/7rAun23qLvjDJ7N3tUTjb+b
mS+z48nBMju37Iflv39vxijIdth07d001dxNOtaTVjnVYoHOwHX0tTkXeyuD0CGMS7fCHp
v9aDxRcrMAaNVKMzZbmEjucR5L+VMM5njNnS4=
-----END OPENSSH PRIVATE KEY-----
";

    #[test]
    fn load_identity_classifies_files() {
        let dir = temp_dir("load");
        let plain = dir.join("id_plain");
        write_openssh_key(&plain, &seeded_key(1));
        assert!(matches!(load_identity(&plain), KeyLoad::Loaded(_)));

        assert!(matches!(
            load_identity(&dir.join("missing")),
            KeyLoad::NotFound
        ));

        let garbage = dir.join("garbage");
        std::fs::write(&garbage, "not a key").unwrap();
        match load_identity(&garbage) {
            KeyLoad::Failed(reason) => {
                assert!(reason.contains("unsupported or invalid"), "{reason}")
            }
            _ => panic!("garbage should fail to load"),
        }

        // 目录:读失败但不是 NotFound。
        match load_identity(&dir) {
            KeyLoad::Failed(reason) => assert!(reason.starts_with("cannot read"), "{reason}"),
            KeyLoad::NotFound => {}
            _ => panic!("a directory is not a key"),
        }

        // OpenSSH 新格式的加密私钥:公钥部分是明文,能拿到。
        let encrypted = dir.join("id_encrypted");
        std::fs::write(&encrypted, ENCRYPTED_ED25519_FIXTURE).unwrap();
        match load_identity(&encrypted) {
            KeyLoad::Encrypted { public } => assert!(public.is_some()),
            _ => panic!("encrypted key should be recognised as encrypted"),
        }

        // 加密的传统 PKCS#1:公钥取自同目录的 .pub。
        let pkcs1 = dir.join("id_rsa_enc");
        std::fs::write(
            &pkcs1,
            "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n\
             DEK-Info: AES-128-CBC,0123456789ABCDEF0123456789ABCDEF\n\nQUJDRA==\n\
             -----END RSA PRIVATE KEY-----\n",
        )
        .unwrap();
        assert!(matches!(
            load_identity(&pkcs1),
            KeyLoad::Encrypted { public: None }
        ));
        let public = seeded_key(2).public_key().to_openssh().unwrap();
        std::fs::write(dir.join("id_rsa_enc.pub"), public).unwrap();
        assert!(matches!(
            load_identity(&pkcs1),
            KeyLoad::Encrypted { public: Some(_) }
        ));

        // 明文传统 PKCS#1 RSA 走兜底解析。
        let rsa = dir.join("id_rsa_pkcs1");
        std::fs::write(&rsa, PKCS1_RSA_FIXTURE).unwrap();
        assert!(matches!(load_identity(&rsa), KeyLoad::Loaded(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn local(label: &str, seed: u8) -> KeyCandidate {
        KeyCandidate {
            label: label.into(),
            source: KeySource::Local(Arc::new(seeded_key(seed))),
        }
    }

    fn from_agent(label: &str, seed: u8) -> KeyCandidate {
        KeyCandidate {
            label: label.into(),
            source: KeySource::Agent(seeded_key(seed).public_key().clone()),
        }
    }

    fn labels(candidates: &[KeyCandidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn candidates_follow_openssh_order_and_dedupe() {
        // 有显式私钥:显式在前,agent 在后;agent 里与显式私钥相同的那把不再重复。
        let ordered = assemble_candidates(
            vec![local("explicit", 1)],
            vec![from_agent("agent-dup", 1), from_agent("agent-b", 2)],
            vec![local("default", 3)],
            true,
        );
        assert_eq!(labels(&ordered), ["explicit", "agent-b"]);

        // 没有显式私钥:agent 在前,默认私钥在后;与 agent 重复的默认私钥跳过。
        let ordered = assemble_candidates(
            Vec::new(),
            vec![from_agent("agent-a", 4), from_agent("agent-b", 5)],
            vec![local("default-dup", 4), local("default-c", 6)],
            false,
        );
        assert_eq!(labels(&ordered), ["agent-a", "agent-b", "default-c"]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_agent_pipes_skip_msys_sockets_and_keep_the_default_pipe() {
        let pipes = |sock: Option<&str>| -> Vec<String> {
            windows_agent_pipes(sock)
                .iter()
                .map(|p| p.display().to_string())
                .collect()
        };
        assert_eq!(pipes(None), [OPENSSH_AGENT_PIPE]);
        // Git Bash 的 MSYS socket 路径不是命名管道:跳过,只试默认管道。
        assert_eq!(pipes(Some("/tmp/ssh-XXXX/agent.123")), [OPENSSH_AGENT_PIPE]);
        // 指向别的命名管道(如 1Password):先试它,再试默认管道。
        let custom = r"\\.\pipe\custom-agent";
        assert_eq!(pipes(Some(custom)), [custom, OPENSSH_AGENT_PIPE]);
        let slashed = "//./pipe/custom-agent";
        assert_eq!(pipes(Some(slashed)), [slashed, OPENSSH_AGENT_PIPE]);
        // 就是默认管道:不重复。
        assert_eq!(pipes(Some(OPENSSH_AGENT_PIPE)), [OPENSSH_AGENT_PIPE]);
    }

    /// 解出占位签名里的 SSH 签名(验证它是格式合法的 `string(blob)`)。
    fn decode_placeholder(data: &[u8]) -> russh::keys::ssh_key::Signature {
        use russh::keys::ssh_encoding::Decode;
        let blob_len = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
        assert_eq!(blob_len, data.len() - 4, "string(blob) 的长度前缀");
        let mut blob = &data[4..];
        russh::keys::ssh_key::Signature::decode(&mut blob).expect("占位签名必须格式合法")
    }

    #[test]
    fn placeholder_signature_is_well_formed_for_each_key_type() {
        let key: AgentIdentity = seeded_key(7).public_key().clone().into();
        let data = with_placeholder_signature(b"prefix".to_vec(), &key, None);
        assert!(data.starts_with(b"prefix"));
        let signature = decode_placeholder(&data[6..]);
        assert_eq!(signature.algorithm(), Algorithm::Ed25519);

        // RSA 按协商的哈希写签名算法名。
        let rsa = try_parse_pkcs1_rsa(PKCS1_RSA_FIXTURE).unwrap().unwrap();
        let rsa: AgentIdentity = rsa.public_key().clone().into();
        let data = with_placeholder_signature(Vec::new(), &rsa, Some(HashAlg::Sha512));
        assert_eq!(
            decode_placeholder(&data).algorithm().as_str(),
            "rsa-sha2-512"
        );

        // ECDSA:r、s 两个 mpint。公钥是 `ssh-keygen -t ecdsa -b 256` 一次性生成的
        // 测试用公钥(私钥已丢弃)。
        let ecdsa = PublicKey::from_openssh(concat!(
            "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBB",
            "wA3SbR3vfj7sDOzJjuDSuuqnujfkuqFBNypBA40z7awKmFlRyQf6ptbj1mPxGhZwRQCPJXED3R",
            "LSbRecmduLU= mt-ssh-test-ecdsa",
        ))
        .unwrap();
        let ecdsa: AgentIdentity = ecdsa.into();
        let data = with_placeholder_signature(Vec::new(), &ecdsa, None);
        assert_eq!(
            decode_placeholder(&data).algorithm().as_str(),
            "ecdsa-sha2-nistp256"
        );
    }

    #[test]
    fn report_lists_each_method_without_credentials() {
        let mut report = AuthReport::new("deploy@h:22");
        report.server_methods = Some("publickey,password".into());
        report.note("identity file ~/.ssh/k (connection setting): file not found");
        report.note("password: rejected by server");
        let text = report.render();
        assert_eq!(
            text,
            "authentication failed for deploy@h:22 (server allows: publickey,password): identity \
             file ~/.ssh/k (connection setting): file not found; password: rejected by server"
        );

        let empty = AuthReport::new("u@h:22").render();
        assert!(
            empty.starts_with("authentication failed for u@h:22"),
            "{empty}"
        );
        assert!(empty.contains("no usable credential"), "{empty}");
    }

    #[test]
    fn session_end_display_starts_in_english() {
        assert_eq!(
            SessionEnd::ServerDisconnect("Too many authentication failures".into()).to_string(),
            "the server closed the connection: Too many authentication failures"
        );
        let text = SessionEnd::NoMoreMethods.to_string();
        assert!(text.starts_with("the server"), "{text}");
    }

    // --- PKCS#1 传统 RSA 私钥 fallback (task 06-06-ssh-mcp-pkcs1-rsa-key) -------
    //
    // 下面这把私钥是 `ssh-keygen -t rsa -b 2048 -m PEM` 一次性生成的**测试专用
    // 废弃密钥**,只用于验证 PKCS#1 PEM 能被 try_parse_pkcs1_rsa 解析,不对应任何
    // 真实主机/账号,无任何安全意义。

    const PKCS1_RSA_FIXTURE: &str = r#"-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAlsXj/txeglED7/6iFd1ic1U6NygRpptNLQ5AxqEBHFgRLOxa
v5oTe1JQCIz4AcUiBM1d8DCRhFgdrUCz8O/gc7R3d0CjlWmcrw9VTe/OsndWeexz
WQWduN7dqBR53jNmVw1+638mc6hXq2wNvDpohVJVK7WkpyGIfTJ6Pu/RE0JIpLjj
5UoYHGOtGApME3/xmznA77BLjACCDvOzBtnRJWdvMruzzFgvfZEOJJIEqbgXSrCd
XBxBFo+QEggaHtSzvCE2f5ADAnlZya6EZrxkMykxqWbHrRPrlISw7kOqgx4636QQ
trdwNr6qpWFTon9wMPMC3soJg0wNs3gVxXVuaQIDAQABAoIBAA2/wAR8DguY3bYN
+gk/u1GZzQmTQtYhmrkSxVTCVo/szAF/uuAhaci41ImNvrP9SRaNBRWj0tvxuSBq
d75EnFWQzcV3LzOvLNWd8qvSbtPsJAZRn0yClu5xNwoJIU/iT2EoNDxqHOnhHmrd
xfw14KrVEOU/1uL9di03OYScaUF13xFsvrRgdpjIKyJF7G/Tg7iSn0CrQ14mGmqJ
1y+xYHGEezZjNAQNqYVpcyGXgNqWyNjdg0VOyunnR77GuyGgLzZMTMJGPTm7psnz
o2ieLJOyZWO9H9EnjhwLkC+SfG6f1h6l65ZB/kS9lTwyZehQJZsN0aHUQeEYK45D
GalV1X0CgYEAzbGacQ2FR7Pc0RM52pvRRFc3pI1LAyAiJCnDI80DKUW0dlZurr2D
/jG3WY3FMlPImM5juzwosxgOwBvA3x6Enwz2ZFt9IXDkcykzSXKITp3OdK8Xjy+L
gn2wdvAWHhzp859wwoekXc0oRTAXWsiW9bhSysWoroaTy2HRgwwSf58CgYEAu6W5
FGY7oIHwOXP0uSDBs3HMkuXoUMVcUnk397GGk2rNzH0LkFzYqmsB56IRNgZG2IbZ
3SKPx+9qrm7/SOv7Hbr/HJ8rTfj6U9xGoIGX2ruX8lS2pRhlcgB38t2bvFsrb8RN
xIueELXerOOMnq4oBXp+EGPtEMuK1vf3tQRQNPcCgYAL8TzTRYKweAvhA6m/PH64
5gtv/VgWlV4GFXqj8Ho3gjmJCVmhwZURRBeuFmIVmvGxlYIK0+JVC5eHpdTb32y5
w0nm57zrHR/WY9T7da/eSKE8+xF2Gb+S0vNU5HmUQ/99SouEb9WmMIwfADzK44yI
Naxw42r4vw2DqGk+n4vPZwKBgBLEuaVTsGUWeguVEIYvw5AKMtcCjeD+TISnQTTS
Gc7G4PyyCSUQVE9/UnpzmFsZ954Sptnaah0qUjZOPdRyXfSUTo3zUaaD363hm2LU
c3baSpFfbcFHlmX3rAerqLcHO2n7bXfaKx4qwrHyNI9uhew+WzuScxS59xIXTTxa
yRbzAoGBALUQGCvKpui8rdcPsRAIjqxQ1/EPB2smWtvLwv23d0D9xjotpWdW5Ujk
vm3b+9rtE2PW5jCNTR/JgxaWkAqC6j+fCIAunMUxYFqHMC+Q2aExzL+dDSxBe1Jb
ipMBLNlhlJHNKVmgnpLBSiUoO5fDWn1KcwvQouOC3U3hSMAPnT+7
-----END RSA PRIVATE KEY-----
"#;

    #[test]
    fn try_parse_pkcs1_rsa_parses_plaintext_pkcs1() {
        let key = try_parse_pkcs1_rsa(PKCS1_RSA_FIXTURE)
            .expect("PKCS#1 RSA 应解析成功")
            .expect("PKCS#1 标签应命中 Some 分支");
        // 解析出的私钥算法应为 RSA(ssh-rsa)。
        assert!(
            key.algorithm().as_str().contains("rsa"),
            "expected an RSA key, got algorithm {}",
            key.algorithm().as_str()
        );
    }

    #[test]
    fn try_parse_pkcs1_rsa_returns_none_for_non_pkcs1_tag() {
        // OpenSSH 格式标签 —— 本函数不处理,返回 None 让上层回退到 russh 原生错误。
        let openssh =
            "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n";
        assert!(matches!(try_parse_pkcs1_rsa(openssh), Ok(None)));
        // PKCS#8 标签同理。
        let pkcs8 = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        assert!(matches!(try_parse_pkcs1_rsa(pkcs8), Ok(None)));
    }

    #[test]
    fn try_parse_pkcs1_rsa_rejects_encrypted_pkcs1_with_guidance() {
        let encrypted = "-----BEGIN RSA PRIVATE KEY-----\n\
            Proc-Type: 4,ENCRYPTED\n\
            DEK-Info: AES-128-CBC,0123456789ABCDEF0123456789ABCDEF\n\n\
            QUJDRA==\n\
            -----END RSA PRIVATE KEY-----\n";
        let err = try_parse_pkcs1_rsa(encrypted).expect_err("加密 PKCS#1 应被拒绝");
        assert!(
            err.contains("passphrase"),
            "错误信息应给 passphrase 指引,实际: {err}"
        );
    }

    #[test]
    fn try_parse_pkcs1_rsa_errors_on_corrupt_base64() {
        // 命中 PKCS#1 标签但主体不是合法 base64/DER —— 返回 Err,不会 panic。
        let bad =
            "-----BEGIN RSA PRIVATE KEY-----\n@@@not-base64@@@\n-----END RSA PRIVATE KEY-----\n";
        assert!(try_parse_pkcs1_rsa(bad).is_err());
    }

    #[test]
    fn public_key_identity_files_are_recognised() {
        // `IdentityFile ~/.ssh/key.pub`(1Password 推荐的写法):认成公钥,交给 agent 签名。
        let dir = temp_dir("public-only");
        let key = seeded_key(9);
        let path = dir.join("key.pub");
        let line = key.public_key().to_openssh().unwrap();
        std::fs::write(&path, format!("# comment\n\n{line}\n")).unwrap();
        match load_identity(&path) {
            KeyLoad::PublicOnly { public } => {
                assert_eq!(public.key_data(), key.public_key().key_data());
            }
            _ => panic!("a public key file should load as PublicOnly"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn report_with_rejection(rejected_before: bool) -> AuthReport {
        let mut report = AuthReport::new("u@h:22");
        report.server_methods = Some("publickey,password".into());
        if rejected_before {
            report.rejected_any = true;
            report.note("key-a: rejected by server");
        }
        report
    }

    #[test]
    fn session_end_during_auth_is_classified_by_its_cause() {
        // 服务器断开(MaxAuthTries):认证失败,写明是谁断的。
        let mut report = report_with_rejection(false);
        let ended = SessionEnd::ServerDisconnect("Too many authentication failures".into());
        match classify_loss(&mut report, Some("key-b"), Some(ended)) {
            AuthError::Rejected(message) => assert!(
                message.ends_with(
                    "key-b: no answer, the server closed the connection: Too many \
                     authentication failures"
                ),
                "{message}"
            ),
            other => panic!("expected Rejected, got {other:?}"),
        }

        // 服务器明确拒绝且再无方法:认证失败。
        let mut report = report_with_rejection(false);
        match classify_loss(
            &mut report,
            Some("password"),
            Some(SessionEnd::NoMoreMethods),
        ) {
            AuthError::Rejected(message) => assert!(
                message.contains("password: rejected by server (no authentication methods left)"),
                "{message}"
            ),
            other => panic!("expected Rejected, got {other:?}"),
        }

        // 还没被拒过就断了(断网):网络错误,不计冷却,不谎称「被服务器拒绝」。
        for ended in [Some(SessionEnd::Error("connection reset".into())), None] {
            let mut report = report_with_rejection(false);
            match classify_loss(&mut report, Some("password"), ended) {
                AuthError::Transport(message) => {
                    assert!(
                        message.starts_with("connection lost during authentication on u@h:22: "),
                        "{message}"
                    );
                    assert!(!message.contains("rejected"), "{message}");
                }
                other => panic!("expected Transport, got {other:?}"),
            }
        }

        // 已被明确拒过、随后连接没了(dropbear 到了失败上限直接关连接):认证失败。
        let mut report = report_with_rejection(true);
        let ended = Some(SessionEnd::Error("connection reset".into()));
        match classify_loss(&mut report, Some("key-b"), ended) {
            AuthError::Rejected(message) => {
                assert!(message.contains("key-a: rejected by server"), "{message}");
                assert!(
                    message.contains("key-b: no answer, the connection failed"),
                    "{message}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    fn test_host() -> ResolvedHost {
        ResolvedHost {
            alias: "h".into(),
            host_name: "h".into(),
            port: 22,
            user: "u".into(),
            identities: Vec::new(),
            explicit_identities: false,
            identities_only: false,
            known_hosts_name: "h".into(),
        }
    }

    fn test_plan<'a>(
        host: &'a ResolvedHost,
        password: &'a PasswordInput,
        round: AuthRound,
    ) -> AuthPlan<'a> {
        AuthPlan {
            host,
            label: "u@h:22".into(),
            password,
            default_key_dir: None,
            round,
        }
    }

    #[test]
    fn retry_without_keys_only_when_the_password_never_got_its_turn() {
        let host = test_host();
        let password = PasswordInput::Available("pw".into());
        let all = MethodSet::from(
            &[
                MethodKind::PublicKey,
                MethodKind::Password,
                MethodKind::KeyboardInteractive,
            ][..],
        );
        let keys_only = MethodSet::from(&[MethodKind::PublicKey][..]);
        let report = report_with_rejection(true);
        let rejected = || AuthError::Rejected("authentication failed".into());
        let first = test_plan(&host, &password, AuthRound::First);

        match offer_retry_without_keys(rejected(), &first, &all, &report) {
            AuthError::RetryWithoutKeys { message, notes } => {
                assert_eq!(message, "authentication failed");
                assert_eq!(notes, report.notes);
            }
            other => panic!("expected RetryWithoutKeys, got {other:?}"),
        }
        // 服务器本来就不收密码类方法:重连也没用。
        assert!(matches!(
            offer_retry_without_keys(rejected(), &first, &keys_only, &report),
            AuthError::Rejected(_)
        ));
        // 断网不重连:网络错误原样交回。
        assert!(matches!(
            offer_retry_without_keys(
                AuthError::Transport("connection lost".into()),
                &first,
                &all,
                &report
            ),
            AuthError::Transport(_)
        ));
        // 已经是重连后的那一轮:不再重连。
        let retried = test_plan(
            &host,
            &password,
            AuthRound::WithoutKeys {
                earlier: Vec::new(),
            },
        );
        assert!(matches!(
            offer_retry_without_keys(rejected(), &retried, &all, &report),
            AuthError::Rejected(_)
        ));
        // 没配密码 / 跳板:没有可试的密码。
        for no_password in [PasswordInput::NotConfigured, PasswordInput::NotForJumpHost] {
            let plan = test_plan(&host, &no_password, AuthRound::First);
            assert!(matches!(
                offer_retry_without_keys(rejected(), &plan, &all, &report),
                AuthError::Rejected(_)
            ));
        }
    }
}
