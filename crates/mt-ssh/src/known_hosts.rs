//! known_hosts 解析与主机密钥校验(`accept-new` 语义),以及承载它的 russh
//! Handler [`MtClient`]。自 `pool.rs` 拆出。
//!
//! 匹配口径对齐 OpenSSH(`hostfile.c` / `match.c`):
//!
//! - 主机字段是逗号分隔的模式表,支持 `*` / `?` 通配与 `!` 否定,大小写不敏感;
//!   `[host]:port` 形式按整串匹配,所以 `[*.example.com]:2222` 这类写法照常生效;
//!   任一否定模式命中即整行不适用,否则任一肯定模式命中即适用(`match_hostname`)。
//! - `|1|salt|hash` 哈希条目照旧支持。
//! - `@revoked` 行:服务器这次给出的密钥命中任何一条对该主机生效的吊销行就拒绝,
//!   优先级高于普通条目(同一主机同时登记了这把密钥也一样拒绝)。
//! - `@cert-authority` 行跳过,理由见 [`known_host_entries`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::Handler;

use crate::auth::{ClientNotes, SessionEnd};

/// OpenSSH `match_pattern`:`*` 匹配任意长(含空)串,`?` 恰好一个字符,其余逐字符
/// 相等(区分大小写 —— 需要不敏感的调用方自己先转小写)。
fn match_pattern(text: &str, pattern: &str) -> bool {
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

/// 读 known_hosts 全文。
///
/// - 文件不存在 = 空文件(从没连过任何主机的正常情况);
/// - 其它 IO 错误(权限、路径是目录、`~/.ssh` 是个文件……)原样返回,调用方据此
///   **拒绝**连接 —— 以前一律当空文件,等于把「读不到」当成「没见过」,对已登记
///   过的主机也会重新 accept-new;
/// - 内容按 lossy 解码:个别行混进非 UTF-8 字节不该让整份文件作废。坏字节只会让
///   所在那一行匹配不上(主机名 / base64 里出现替换字符),不会造出假匹配。
pub(crate) fn read_known_hosts(path: &Path) -> std::io::Result<String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// 拼一条 known_hosts 的 host 字段。22 端口写 `host`,其它端口写 `[host]:port`,
/// 与 OpenSSH 客户端写入风格一致(`put_host_port`)。
pub(crate) fn host_pattern(host: &str, port: u16) -> String {
    if port == 22 || port == 0 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// 主机密钥比对结果。行号从 1 起,供拒绝原因指给用户看。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HostKeyMatch {
    /// host 匹配且同算法的 key 字节相同。
    Match,
    /// host 匹配,**同算法**但 key 字节不同(`line` 是第一条这样的行)。
    /// MITM / 服务器换 key 都会落这条。
    Mismatch { line: usize },
    /// 该 host 只登记了**其它算法**的 key(`line` / `algo` 取第一条)。服务器不再
    /// 提供登记过的那种 key —— 同样按换 key 处理,见 `check_server_key`。
    OtherAlgorithm { line: usize, algo: String },
    /// 服务器给出的 key 命中一条对该 host 生效的 `@revoked` 行。
    Revoked { line: usize },
    /// 没找到任何 host 匹配条目。
    Unknown,
}

/// known_hosts 行首标记。`@cert-authority` 与认不出的标记在解析时就整行跳过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// 普通主机密钥行。
    Plain,
    /// `@revoked`:这把 key 已被吊销。
    Revoked,
}

/// known_hosts 里命中某 host 的一条记录。
struct KnownHostEntry<'a> {
    /// 行号,从 1 起。
    line: usize,
    marker: Marker,
    /// key 类型字段(`ssh-ed25519` / `ssh-rsa` / `ecdsa-sha2-nistp256` ……)。
    algo: &'a str,
    /// base64 解码后的 key blob。
    key: Vec<u8>,
}

/// 主机字段(逗号分隔的模式表)是否对 `host` 生效。语义同 OpenSSH `match_hostname`:
/// 否定模式命中 → 不适用(哪怕别的肯定模式也命中);否则任一肯定模式命中 → 适用。
///
/// OpenSSH 只在整个字段以 `|` 开头时把它当成**一个**哈希条目;这里沿用旧实现,
/// 逗号表里混写的哈希项也认 —— 只会多认,不会造出假匹配。
fn hostspec_matches(hostspec: &str, host: &str) -> bool {
    let lowered = host.to_ascii_lowercase();
    let mut positive = false;
    for entry in hostspec.split(',') {
        if entry.starts_with("|1|") {
            // 哈希条目自己处理大小写(小写与原样各试一次),这里给原样的 host。
            positive |= hashed_host_matches(entry, host);
            continue;
        }
        let (negated, pattern) = match entry.strip_prefix('!') {
            Some(pattern) => (true, pattern),
            None => (false, entry),
        };
        if pattern.is_empty() {
            continue;
        }
        if match_pattern(&lowered, &pattern.to_ascii_lowercase()) {
            if negated {
                return false;
            }
            positive = true;
        }
    }
    positive
}

/// 逐行找出 known_hosts 里对 `host`(`host` 或 `[host]:port` 形态)生效的条目。
///
/// 解析规则:
/// - 跳过空行与 `#` 起始的注释;
/// - 字段以空格 / TAB 分隔:`[@marker] <hostspec> <algo> <base64key> [comment]`,
///   缺字段或 base64 解不开的行跳过;
/// - `@revoked` 行照常产出(带 [`Marker::Revoked`]),由调用方优先处理;
/// - `@cert-authority` 行跳过。**这样做是安全的**:本实现只协商普通主机密钥算法
///   (`Preferred::key` 里没有任何 `*-cert-v01@openssh.com`),服务器出示的永远是普通
///   公钥;CA 行只为「由该 CA 签发的主机证书」背书,对普通公钥不构成任何断言 ——
///   OpenSSH 校验普通公钥时同样不看 CA 行。跳过它们不会让本应拒绝的密钥被放行:
///   普通公钥该匹配的普通行、该命中的 `@revoked` 行一条不少;
/// - 其它认不出的 `@` 标记同样跳过(OpenSSH 也拒绝解析这类行)。
fn known_host_entries<'a>(
    raw: &'a str,
    host: &'a str,
) -> impl Iterator<Item = KnownHostEntry<'a>> + 'a {
    raw.lines().enumerate().filter_map(move |(index, line)| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut fields = line.split_ascii_whitespace();
        let mut first = fields.next()?;
        let marker = if first.starts_with('@') {
            let marker = if first.eq_ignore_ascii_case("@revoked") {
                Marker::Revoked
            } else {
                return None;
            };
            first = fields.next()?;
            marker
        } else {
            Marker::Plain
        };
        if !hostspec_matches(first, host) {
            return None;
        }
        let algo = fields.next()?;
        let key = base64_decode(fields.next()?)?;
        Some(KnownHostEntry {
            line: index + 1,
            marker,
            algo,
            key,
        })
    })
}

/// OpenSSH `HashKnownHosts` 条目:`|1|base64(salt)|base64(HMAC-SHA1(key=salt, msg=host))`,
/// host 取与 plaintext 条目相同的 `host` / `[host]:port` 形态。
///
/// OpenSSH 哈希前会把主机名转成小写;为兼容按原样写入的其它工具,原样也试一次。
/// 比对走 `ring::hmac::verify`(常量时间)。
fn hashed_host_matches(entry: &str, host_pattern: &str) -> bool {
    let Some((salt, hash)) = entry
        .strip_prefix("|1|")
        .and_then(|rest| rest.split_once('|'))
    else {
        return false;
    };
    let (Some(salt), Some(hash)) = (base64_decode(salt), base64_decode(hash)) else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
    let lowered = host_pattern.to_ascii_lowercase();
    ring::hmac::verify(&key, lowered.as_bytes(), &hash).is_ok()
        || (lowered != host_pattern
            && ring::hmac::verify(&key, host_pattern.as_bytes(), &hash).is_ok())
}

/// 在 known_hosts 文本里查 `host` 对应的条目并与 `server_pubkey` 比对。
///
/// - 命中任何一条生效的 `@revoked` 行 → `Revoked`,优先于一切(要扫完全文才能下结论,
///   所以即便前面已有能对上的普通行也不提前返回);
/// - 同算法条目里只要有一条 key 字节一致就是 `Match`(同一 host 登记过新旧两把
///   key 时以能对上的为准,与 OpenSSH 一致);同算法的全都对不上才是 `Mismatch`;
/// - 没有同算法条目、但有其它算法的条目是 `OtherAlgorithm`;一条都没有是 `Unknown`
///   (只有吊销行、且吊销的不是这把 key,也算 `Unknown`)。
pub(crate) fn match_known_host(
    raw: &str,
    host: &str,
    server_pubkey: &russh::keys::ssh_key::PublicKey,
) -> HostKeyMatch {
    let want_algo = server_pubkey.algorithm();
    let want_algo = want_algo.as_str();
    let want_bytes = match server_pubkey.to_bytes() {
        Ok(b) => b,
        Err(_) => return HostKeyMatch::Unknown,
    };
    let mut matched = false;
    let mut mismatch: Option<usize> = None;
    let mut other_algo: Option<(usize, String)> = None;
    for entry in known_host_entries(raw, host) {
        if entry.marker == Marker::Revoked {
            if entry.key == want_bytes {
                return HostKeyMatch::Revoked { line: entry.line };
            }
            continue;
        }
        if !entry.algo.eq_ignore_ascii_case(want_algo) {
            other_algo.get_or_insert_with(|| (entry.line, entry.algo.to_string()));
            continue;
        }
        if entry.key == want_bytes {
            matched = true;
        } else {
            mismatch.get_or_insert(entry.line);
        }
    }
    match (matched, mismatch, other_algo) {
        (true, _, _) => HostKeyMatch::Match,
        (false, Some(line), _) => HostKeyMatch::Mismatch { line },
        (false, None, Some((line, algo))) => HostKeyMatch::OtherAlgorithm { line, algo },
        (false, None, None) => HostKeyMatch::Unknown,
    }
}

/// known_hosts 里该 host 已登记的 key 类型(去重,保持文件中的先后)。吊销行不算。
pub(crate) fn known_host_key_types(raw: &str, host: &str) -> Vec<String> {
    let mut types: Vec<String> = Vec::new();
    for entry in known_host_entries(raw, host) {
        if entry.marker == Marker::Revoked {
            continue;
        }
        if !types.iter().any(|t| t.eq_ignore_ascii_case(entry.algo)) {
            types.push(entry.algo.to_string());
        }
    }
    types
}

/// 按该 host 已登记的 key 类型调整首选 host key 算法:已登记的排前面(各自保持
/// russh 默认的相对顺序),其余照旧跟在后面 —— 即 OpenSSH `order_hostkeyalgs` 的做法。
///
/// 协商取客户端列表里第一个双方都支持的算法。服务器通常同时有 ed25519 / ecdsa /
/// rsa 几把 host key,known_hosts 只登记了 rsa 而我们默认首选 ed25519 时,会拿到
/// 一把「没见过」的 ed25519 key。已登记类型排前面后,服务器仍支持它就自然协商到它、
/// 与登记的 key 比对;服务器不再提供时才会拿到别的类型,由 `check_server_key`
/// 按 [`HostKeyMatch::OtherAlgorithm`] 拒绝。
///
/// known_hosts 里 RSA key 的类型只有 `ssh-rsa`,对应的签名算法 rsa-sha2-512 /
/// rsa-sha2-256 / ssh-rsa 整组前移。没有任何能对上 russh 支持算法的已登记类型时
/// 返回 `None`,沿用默认顺序。
pub(crate) fn preferred_host_key_algorithms(
    known_types: &[String],
) -> Option<Vec<russh::keys::Algorithm>> {
    let is_known = |algo: &russh::keys::Algorithm| {
        let key_type = if matches!(algo, russh::keys::Algorithm::Rsa { .. }) {
            "ssh-rsa"
        } else {
            algo.as_str()
        };
        known_types.iter().any(|t| t.eq_ignore_ascii_case(key_type))
    };
    let (mut ordered, rest): (Vec<_>, Vec<_>) = russh::Preferred::default()
        .key
        .iter()
        .cloned()
        .partition(is_known);
    if ordered.is_empty() {
        return None;
    }
    ordered.extend(rest);
    Some(ordered)
}

/// 标准 base64 解码(需带 `=` 填充 —— OpenSSH 写出的 key 与哈希条目都带)。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .ok()
}

/// 把一条新 host-key 写入 known_hosts,父目录不存在则创建。
///
/// 文件末尾没有换行(手工编辑、别的工具写的)时先补一个,否则新条目会粘在上一行后面,
/// 两条一起作废、该主机退回首次信任 —— 与 OpenSSH `add_host_to_hostfile` 同一做法。
pub(crate) fn append_known_host(
    path: &Path,
    host_pattern: &str,
    server_pubkey: &russh::keys::ssh_key::PublicKey,
) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // algorithm() 返回临时,先 bind 延长生命周期再借 as_str()。
    let algo_holder = server_pubkey.algorithm();
    let algo = algo_holder.as_str();
    let b64 = {
        use base64::Engine;
        let bytes = server_pubkey
            .to_bytes()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    let mut last = [0u8; 1];
    let needs_newline = f.metadata()?.len() > 0 && {
        f.seek(SeekFrom::End(-1))?;
        f.read_exact(&mut last)?;
        last[0] != b'\n'
    };
    let line = format!(
        "{}{host_pattern} {algo} {b64}\n",
        if needs_newline { "\n" } else { "" }
    );
    // 追加模式下写入总在文件末尾,与刚才读的位置无关;一次写完,免得只补了换行。
    f.write_all(line.as_bytes())
}

/// 拼连接失败的错误文本。主机密钥被 [`MtClient`] 拒绝时换成它记下的具体原因
/// (文件路径 / 行号 / 读失败),而不是 russh 泛泛的 "Unknown server key"。
///
/// `hop` 是目标的可读描述(`example.com:22`)。
pub(crate) fn connect_error(
    hop: &str,
    err: &dyn std::fmt::Display,
    rejected: Option<String>,
) -> String {
    match rejected {
        Some(reason) => format!("ssh connect to {hop} failed: {reason}"),
        None => format!("ssh connect to {hop} failed: {err}"),
    }
}

/// russh Handler 实现:每条 session 一个实例,负责主机密钥校验,并记下会话结束的原因。
pub struct MtClient {
    /// known_hosts 查找 / 写入用的名字:主机名按 [`host_pattern`] 带上端口(OpenSSH
    /// 同一口径)。
    known_hosts_name: String,
    known_hosts_path: PathBuf,
    /// 与建连方共享的旁路信息。主机密钥被拒后 russh 只报泛泛的 "Unknown server key",
    /// 认证途中被服务器断开时只表现为「没有剩余方法」,handler 本身又随连接一起被
    /// russh 拿走 —— 建连方持同一个 `Arc`,在错误路径上取出具体原因拼进报错
    /// (主程序远程项目 / sidecar 都把这条错误原样展示给用户)。
    notes: Arc<ClientNotes>,
}

impl MtClient {
    pub(crate) fn new(
        known_hosts_name: String,
        known_hosts_path: PathBuf,
        notes: Arc<ClientNotes>,
    ) -> Self {
        Self {
            known_hosts_name,
            known_hosts_path,
            notes,
        }
    }

    /// 记下拒绝原因并打一行日志。返回 `false`,调用处直接 `Ok(self.reject(..))`。
    fn reject(&self, reason: String) -> bool {
        eprintln!("[mt-ssh] host key rejected: {reason}");
        if let Ok(mut slot) = self.notes.rejection.lock() {
            *slot = Some(reason);
        }
        false
    }
}

impl Handler for MtClient {
    type Error = russh::Error;

    /// 会话结束时记下原因(只记第一次),错误照默认实现原样交回 russh。
    async fn disconnected(
        &mut self,
        reason: russh::client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let (end, result) = match reason {
            russh::client::DisconnectReason::ReceivedDisconnect(info) => {
                let message = if info.message.trim().is_empty() {
                    format!("{:?}", info.reason_code)
                } else {
                    info.message
                };
                (SessionEnd::ServerDisconnect(message), Ok(()))
            }
            russh::client::DisconnectReason::Error(russh::Error::NoAuthMethod) => {
                (SessionEnd::NoMoreMethods, Err(russh::Error::NoAuthMethod))
            }
            russh::client::DisconnectReason::Error(e) => (SessionEnd::Error(e.to_string()), Err(e)),
        };
        if let Ok(mut slot) = self.notes.ended.lock() {
            slot.get_or_insert(end);
        }
        result
    }

    /// host-key 校验:accept-new 语义(对齐 OpenSSH `StrictHostKeyChecking=accept-new`)。
    /// - known_hosts 里该 host 有生效的 `@revoked` 行且吊销的正是这把 key → 拒绝。
    /// - 有该 host + 同算法的条目且 key 字节一致 → 通过(plaintext / 通配 / `|1|`
    ///   哈希条目都认)。
    /// - 有该 host + 同算法的条目但 key 不同 → 拒绝。只靠通配行登记的主机同样适用。
    /// - 该 host 只登记了**其它算法**的 key → 拒绝且不追加,视同换 key。连接前已把
    ///   已登记的类型排到首选(`preferred_host_key_algorithms`),服务器还支持它时
    ///   根本走不到这里;走到了说明服务器不再提供登记过的那把 key。
    /// - 该 host 没有任何条目 → 把当前 key 以 plaintext 追加到 known_hosts,通过。
    /// - known_hosts 读失败(文件不存在除外,那等于空文件)或追加失败 → 拒绝,
    ///   绝不在没法校验时悄悄放行。
    ///
    /// 拒绝原因记进 `rejection`,由建连方拼进连接错误。原因一律**英文开头**
    /// (agent 在 GBK 代码页的 PowerShell 5.1 里读 sidecar 输出时中文会乱码,
    /// 英文类别仍认得出来),后面保留给远程项目界面看的中文细节。
    async fn check_server_key(
        &mut self,
        server_pubkey: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let name = self.known_hosts_name.clone();
        let path = self.known_hosts_path.display().to_string();
        let raw = match read_known_hosts(&self.known_hosts_path) {
            Ok(raw) => raw,
            Err(e) => {
                return Ok(self.reject(format!(
                    "known_hosts unreadable ({path}: {e}): known_hosts 读取失败({path}: {e}),\
                     无法校验 {name} 的主机密钥,拒绝连接"
                )));
            }
        };
        let offered_algo = server_pubkey.algorithm();
        let offered_algo = offered_algo.as_str();
        match match_known_host(&raw, &name, server_pubkey) {
            HostKeyMatch::Match => Ok(true),
            HostKeyMatch::Revoked { line } => Ok(self.reject(format!(
                "host key verification failed: the {offered_algo} key offered by {name} is marked \
                 @revoked in {path} line {line}: 主机密钥已被吊销:{path} 第 {line} 行把 {name} \
                 这次给出的 {offered_algo} 密钥标记为 @revoked,拒绝连接"
            ))),
            HostKeyMatch::Mismatch { line } => Ok(self.reject(format!(
                "host key verification failed: the {offered_algo} key offered by {name} does not \
                 match {path} line {line} (possible man-in-the-middle attack, or the server was \
                 reinstalled): 主机密钥与 known_hosts 不符:{path} 第 {line} 行登记的 {name} \
                 {offered_algo} 密钥与服务器这次给出的不同(可能遭遇中间人攻击,也可能是服务器重装\
                 换了密钥;确认是后者的话删除该行后重试)"
            ))),
            HostKeyMatch::OtherAlgorithm { line, algo } => Ok(self.reject(format!(
                "host key verification failed: {name} offered a {offered_algo} key but {path} line \
                 {line} records a {algo} key for it: 主机密钥与 known_hosts 不符:服务器给出的是 \
                 {offered_algo} 密钥,而 {path} 第 {line} 行登记的是 {name} 的 {algo} 密钥,服务器\
                 已不再提供该类型,按密钥变更处理(确认服务器确实换了密钥类型的话删除该主机的旧条目\
                 后重试)"
            ))),
            HostKeyMatch::Unknown => {
                if let Err(e) = append_known_host(&self.known_hosts_path, &name, server_pubkey) {
                    return Ok(self.reject(format!(
                        "known_hosts write failed ({path}: {e}): 无法把 {name} 的新主机密钥写入 \
                         known_hosts({path}: {e}),拒绝连接"
                    )));
                }
                Ok(true)
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

    // --- match_known_host fixture helpers ----------------------------------
    //
    // 直接用 32 字节常量构造 ed25519 PublicKey,不经过 rng;ssh-key 不验证 ed25519
    // 公钥的密码学合法性(只解析 wire 格式),所以任意 32 字节都能 round-trip。

    fn test_pubkey_from_bytes(bytes: [u8; 32]) -> russh::keys::ssh_key::PublicKey {
        use russh::keys::ssh_key::public::{Ed25519PublicKey, KeyData, PublicKey};
        PublicKey::new(KeyData::Ed25519(Ed25519PublicKey(bytes)), "test")
    }

    fn pubkey_b64(pub_key: &russh::keys::ssh_key::PublicKey) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(pub_key.to_bytes().unwrap())
    }

    fn pubkey_algo(pub_key: &russh::keys::ssh_key::PublicKey) -> String {
        pub_key.algorithm().as_str().to_string()
    }

    /// `<algo> <base64>` 两个字段,拼 known_hosts 行用。
    fn key_fields(pub_key: &russh::keys::ssh_key::PublicKey) -> String {
        format!("{} {}", pubkey_algo(pub_key), pubkey_b64(pub_key))
    }

    const KEY_BYTES_A: [u8; 32] = [0x11; 32];
    const KEY_BYTES_B: [u8; 32] = [0x22; 32];

    #[test]
    fn host_pattern_uses_bracket_form_only_for_nonstandard_port() {
        assert_eq!(host_pattern("h.example.com", 22), "h.example.com");
        assert_eq!(host_pattern("h.example.com", 0), "h.example.com");
        assert_eq!(host_pattern("h.example.com", 2222), "[h.example.com]:2222");
    }

    #[test]
    fn match_known_host_ignores_blank_and_comment_lines() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = "\n# comment line\n\n# another\n";
        assert_eq!(
            match_known_host(raw, "h.example.com", &pub_key),
            HostKeyMatch::Unknown
        );
    }

    #[test]
    fn match_known_host_finds_exact_plaintext_entry() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let host = "h.example.com";
        let raw = format!(
            "# header\n{host} {}\nother-host ssh-rsa AAAA\n",
            key_fields(&pub_key)
        );
        assert_eq!(match_known_host(&raw, host, &pub_key), HostKeyMatch::Match);
    }

    #[test]
    fn match_known_host_detects_same_host_same_algo_diff_key_as_mismatch() {
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let pub_b = test_pubkey_from_bytes(KEY_BYTES_B);
        let raw = format!("h.example.com {}\n", key_fields(&pub_b));
        // 文件里登记的是 pub_b,但服务器报上来的是 pub_a → mismatch
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_a),
            HostKeyMatch::Mismatch { line: 1 }
        );
    }

    #[test]
    fn match_known_host_prefers_matching_line_over_stale_one() {
        // 同一 host 登记过新旧两把同算法 key:能对上的那条为准,不因旧行判 mismatch。
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let pub_b = test_pubkey_from_bytes(KEY_BYTES_B);
        let raw = format!(
            "h.example.com {}\nh.example.com {}\n",
            key_fields(&pub_b),
            key_fields(&pub_a)
        );
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_a),
            HostKeyMatch::Match
        );
    }

    #[test]
    fn match_known_host_malformed_hashed_entry_is_not_a_match() {
        // salt / hash 不是合法 base64 的哈希条目:匹配不上任何 host。
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = "|1|abcsalt|abchash ssh-ed25519 AAAA\n";
        assert_eq!(
            match_known_host(raw, "h.example.com", &pub_key),
            HostKeyMatch::Unknown
        );
    }

    /// 按 OpenSSH `HashKnownHosts` 的算法造一个哈希 hostspec。
    fn hashed_hostspec(host: &str, salt: &[u8]) -> String {
        use base64::Engine;
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, salt);
        let tag = ring::hmac::sign(&key, host.as_bytes());
        let b64 = &base64::engine::general_purpose::STANDARD;
        format!("|1|{}|{}", b64.encode(salt), b64.encode(tag.as_ref()))
    }

    #[test]
    fn hashed_host_matches_openssh_generated_entry() {
        // 已知答案:russh 自带测试里由 OpenSSH 生成的 example.com 哈希条目,
        // 不依赖本实现自己算出来的值。
        let entry = "|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak=";
        assert!(hashed_host_matches(entry, "example.com"));
        // OpenSSH 按小写主机名哈希:大小写不同的输入也要命中
        assert!(hashed_host_matches(entry, "Example.COM"));
        assert!(!hashed_host_matches(entry, "example.org"));
        assert!(!hashed_host_matches(entry, "[example.com]:2222"));
    }

    #[test]
    fn match_known_host_matches_openssh_hashed_line() {
        let raw = "|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= ssh-ed25519 \
                   AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF\n";
        let pub_key = russh::keys::parse_public_key_base64(
            "AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF",
        )
        .unwrap();
        assert_eq!(
            match_known_host(raw, "example.com", &pub_key),
            HostKeyMatch::Match
        );
        // 同一条目换一把同算法 key → 按变更拒绝,而不是像以前那样当新主机追加
        let other = test_pubkey_from_bytes(KEY_BYTES_A);
        assert_eq!(
            match_known_host(raw, "example.com", &other),
            HostKeyMatch::Mismatch { line: 1 }
        );
        assert_eq!(
            match_known_host(raw, "other.example.com", &other),
            HostKeyMatch::Unknown
        );
    }

    #[test]
    fn match_known_host_hashed_entry_with_port_and_mixed_list() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let pattern = host_pattern("h.example.com", 2222);
        let raw = format!(
            "# 第一行注释\nplain.example.com,{} {}\n",
            hashed_hostspec(&pattern, &[7u8; 20]),
            key_fields(&pub_key)
        );
        assert_eq!(
            match_known_host(&raw, &pattern, &pub_key),
            HostKeyMatch::Match
        );
        // 端口不同就是另一个 host
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_key),
            HostKeyMatch::Unknown
        );
        let pub_b = test_pubkey_from_bytes(KEY_BYTES_B);
        assert_eq!(
            match_known_host(&raw, &pattern, &pub_b),
            HostKeyMatch::Mismatch { line: 2 }
        );
    }

    #[test]
    fn match_known_host_comma_separated_hosts_match_any() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = format!("alias.example.com,h.example.com {}\n", key_fields(&pub_key));
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_key),
            HostKeyMatch::Match
        );
    }

    #[test]
    fn match_known_host_other_algorithm_only_is_reported() {
        // 同 host 只登记了别的算法:不再当新主机放行,报出第一条的行号与算法。
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let other_blob = pubkey_b64(&test_pubkey_from_bytes(KEY_BYTES_B));
        let raw = format!(
            "other.example.com ssh-ed25519 {other_blob}\nh.example.com ssh-rsa {other_blob}\n"
        );
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_key),
            HostKeyMatch::OtherAlgorithm {
                line: 2,
                algo: "ssh-rsa".into()
            }
        );
        // 同时有同算法条目时以同算法的比对结果为准
        let with_same_algo = format!("{raw}h.example.com {}\n", key_fields(&pub_key));
        assert_eq!(
            match_known_host(&with_same_algo, "h.example.com", &pub_key),
            HostKeyMatch::Match
        );
    }

    #[test]
    fn match_known_host_skips_cert_authority_and_malformed_lines() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = format!(
            "@cert-authority h.example.com {algo} {b64}\n\
             @unknown-marker h.example.com {algo} {b64}\n\
             h.example.com {algo}\n\
             h.example.com {algo} not-base64!\n",
            algo = pubkey_algo(&pub_key),
            b64 = pubkey_b64(&test_pubkey_from_bytes(KEY_BYTES_B)),
        );
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_key),
            HostKeyMatch::Unknown
        );
    }

    #[test]
    fn match_known_host_revoked_key_wins_over_a_matching_line() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        // 普通行能对上,但后面又有一条吊销同一把 key 的行:吊销优先。
        let raw = format!(
            "h.example.com {k}\n@revoked h.example.com {k}\n",
            k = key_fields(&pub_key)
        );
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_key),
            HostKeyMatch::Revoked { line: 2 }
        );
        // `@revoked *` 对所有主机生效,包括 `[host]:port` 形态。
        let raw = format!("@revoked * {}\n", key_fields(&pub_key));
        assert_eq!(
            match_known_host(&raw, "[h.example.com]:2222", &pub_key),
            HostKeyMatch::Revoked { line: 1 }
        );
        // 标记大小写不敏感;吊销行的主机字段也支持哈希条目。
        let raw = format!(
            "@REVOKED {} {}\n",
            hashed_hostspec("h.example.com", &[9u8; 20]),
            key_fields(&pub_key)
        );
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_key),
            HostKeyMatch::Revoked { line: 1 }
        );
    }

    #[test]
    fn match_known_host_revoked_line_for_another_key_or_host_does_not_apply() {
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let pub_b = test_pubkey_from_bytes(KEY_BYTES_B);
        // 吊销的是另一把 key:只有吊销行时这把 key 仍是「没见过」,可以 accept-new。
        let raw = format!("@revoked h.example.com {}\n", key_fields(&pub_b));
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_a),
            HostKeyMatch::Unknown
        );
        // 吊销行的主机不匹配:不生效。
        let raw = format!(
            "@revoked other.example.com {k}\nh.example.com {k}\n",
            k = key_fields(&pub_a)
        );
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_a),
            HostKeyMatch::Match
        );
    }

    #[test]
    fn match_known_host_wildcard_only_registration() {
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let pub_b = test_pubkey_from_bytes(KEY_BYTES_B);
        let raw = format!("*.example.com {}\n", key_fields(&pub_a));
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_a),
            HostKeyMatch::Match
        );
        // 只靠通配行登记:key 不一致按 Mismatch 拒绝,而不是当新主机追加。
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_b),
            HostKeyMatch::Mismatch { line: 1 }
        );
        // 通配不跨越到别的域,也不匹配非标准端口的 `[host]:port` 形态。
        assert_eq!(
            match_known_host(&raw, "h.example.org", &pub_a),
            HostKeyMatch::Unknown
        );
        assert_eq!(
            match_known_host(&raw, "[h.example.com]:2222", &pub_a),
            HostKeyMatch::Unknown
        );
        // `?` 恰好一个字符;大小写不敏感。
        let raw = format!("WEB?.Example.COM {}\n", key_fields(&pub_a));
        assert_eq!(
            match_known_host(&raw, "web1.example.com", &pub_a),
            HostKeyMatch::Match
        );
        assert_eq!(
            match_known_host(&raw, "web12.example.com", &pub_a),
            HostKeyMatch::Unknown
        );
    }

    #[test]
    fn match_known_host_bracketed_port_wildcard() {
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = format!("[*.example.com]:2222 {}\n", key_fields(&pub_a));
        assert_eq!(
            match_known_host(&raw, "[h.example.com]:2222", &pub_a),
            HostKeyMatch::Match
        );
        assert_eq!(
            match_known_host(&raw, "[h.example.com]:2200", &pub_a),
            HostKeyMatch::Unknown
        );
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_a),
            HostKeyMatch::Unknown
        );
    }

    #[test]
    fn match_known_host_negated_pattern_excludes_the_line() {
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = format!("!bad.example.com,*.example.com {}\n", key_fields(&pub_a));
        assert_eq!(
            match_known_host(&raw, "good.example.com", &pub_a),
            HostKeyMatch::Match
        );
        // 否定模式命中:整行不适用(哪怕 `*.example.com` 也命中)。
        assert_eq!(
            match_known_host(&raw, "bad.example.com", &pub_a),
            HostKeyMatch::Unknown
        );
        // 只有否定模式、没有肯定模式命中:同样不适用。
        let raw = format!("!bad.example.com {}\n", key_fields(&pub_a));
        assert_eq!(
            match_known_host(&raw, "good.example.com", &pub_a),
            HostKeyMatch::Unknown
        );
    }

    #[test]
    fn known_host_key_types_dedupes_in_file_order() {
        let blob = pubkey_b64(&test_pubkey_from_bytes(KEY_BYTES_A));
        let raw = format!(
            "h.example.com ssh-rsa {blob}\n\
             other.example.com ecdsa-sha2-nistp256 {blob}\n\
             h.example.com ssh-ed25519 {blob}\n\
             H.EXAMPLE.COM SSH-RSA {blob}\n\
             @revoked h.example.com ecdsa-sha2-nistp384 {blob}\n\
             *.example.com ecdsa-sha2-nistp521 {blob}\n"
        );
        // 吊销行不算已登记类型;通配行算。
        assert_eq!(
            known_host_key_types(&raw, "h.example.com"),
            vec![
                "ssh-rsa".to_string(),
                "ssh-ed25519".to_string(),
                "ecdsa-sha2-nistp521".to_string()
            ]
        );
        assert!(known_host_key_types(&raw, "nobody.example.org").is_empty());
    }

    #[test]
    fn preferred_host_key_algorithms_puts_known_types_first() {
        use russh::keys::{Algorithm, EcdsaCurve, HashAlg};
        let default_len = russh::Preferred::default().key.len();

        // 只登记了 RSA:签名算法三兄弟整组前移,其余保持默认相对顺序
        let order = preferred_host_key_algorithms(&["ssh-rsa".to_string()]).unwrap();
        assert_eq!(order.len(), default_len, "只调序,不增删");
        assert_eq!(
            order[..4],
            [
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha512)
                },
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha256)
                },
                Algorithm::Rsa { hash: None },
                Algorithm::Ed25519,
            ]
        );

        // 登记了 nistp384 与 ed25519:两者都前移,仍按默认相对顺序(ed25519 在前)
        let order = preferred_host_key_algorithms(&[
            "ecdsa-sha2-nistp384".to_string(),
            "ssh-ed25519".to_string(),
        ])
        .unwrap();
        assert_eq!(
            order[..3],
            [
                Algorithm::Ed25519,
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP384
                },
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP256
                },
            ]
        );

        // 没有已登记类型 / 只有 russh 不支持的类型:沿用默认顺序
        assert!(preferred_host_key_algorithms(&[]).is_none());
        assert!(preferred_host_key_algorithms(&["ssh-dss".to_string()]).is_none());
    }

    // --- known_hosts 读取与 check_server_key ------------------------------

    fn known_hosts_test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mt-ssh-known-hosts-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_client(known_hosts_path: PathBuf) -> MtClient {
        MtClient::new(
            "h.example.com".into(),
            known_hosts_path,
            Arc::new(ClientNotes::default()),
        )
    }

    fn rejection_of(client: &MtClient) -> String {
        client
            .notes
            .rejection
            .lock()
            .unwrap()
            .clone()
            .expect("拒绝时应记下原因")
    }

    #[test]
    fn read_known_hosts_missing_file_is_empty() {
        let dir = known_hosts_test_dir("missing");
        assert_eq!(read_known_hosts(&dir.join("known_hosts")).unwrap(), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_known_hosts_decodes_non_utf8_lossily() {
        // 某一行混进非 UTF-8 字节:不能让整份文件读失败,其余条目照常可用。
        let dir = known_hosts_test_dir("non-utf8");
        let path = dir.join("known_hosts");
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let mut bytes = b"# caf\xe9 \xff\xfe\n".to_vec();
        bytes.extend_from_slice(format!("h.example.com {}\n", key_fields(&pub_key)).as_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let raw = read_known_hosts(&path).expect("非 UTF-8 内容不应读失败");
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_key),
            HostKeyMatch::Match
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_known_hosts_other_io_error_is_an_error() {
        // 路径是个目录:读失败但不是 NotFound —— 不能再当成空文件。
        let dir = known_hosts_test_dir("is-dir");
        let path = dir.join("known_hosts");
        std::fs::create_dir_all(&path).unwrap();
        assert!(read_known_hosts(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn check_server_key_rejects_unreadable_known_hosts() {
        let dir = known_hosts_test_dir("reject-unreadable");
        let path = dir.join("known_hosts");
        std::fs::create_dir_all(&path).unwrap();
        let mut client = test_client(path.clone());

        let accepted = client
            .check_server_key(&test_pubkey_from_bytes(KEY_BYTES_A))
            .await
            .unwrap();
        assert!(!accepted, "known_hosts 读不了时不得放行");
        let reason = rejection_of(&client);
        assert!(reason.starts_with("known_hosts unreadable ("), "{reason}");
        assert!(reason.contains("known_hosts 读取失败"), "{reason}");
        assert!(reason.contains(&path.display().to_string()), "{reason}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn check_server_key_mismatch_reason_names_path_and_line() {
        let dir = known_hosts_test_dir("reject-mismatch");
        let path = dir.join("known_hosts");
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let pub_b = test_pubkey_from_bytes(KEY_BYTES_B);
        std::fs::write(
            &path,
            format!("# header\nh.example.com {}\n", key_fields(&pub_b)),
        )
        .unwrap();
        let mut client = test_client(path.clone());

        assert!(!client.check_server_key(&pub_a).await.unwrap());
        let reason = rejection_of(&client);
        assert!(
            reason.starts_with("host key verification failed: "),
            "{reason}"
        );
        assert!(reason.contains("line 2"), "{reason}");
        assert!(reason.contains("主机密钥与 known_hosts 不符"), "{reason}");
        assert!(reason.contains(&path.display().to_string()), "{reason}");
        assert!(reason.contains("第 2 行"), "{reason}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn check_server_key_rejects_other_algorithm_without_appending() {
        let dir = known_hosts_test_dir("reject-other-algo");
        let path = dir.join("known_hosts");
        let blob = pubkey_b64(&test_pubkey_from_bytes(KEY_BYTES_B));
        let original = format!("h.example.com ssh-rsa {blob}\n");
        std::fs::write(&path, &original).unwrap();
        let mut client = test_client(path.clone());

        let accepted = client
            .check_server_key(&test_pubkey_from_bytes(KEY_BYTES_A))
            .await
            .unwrap();
        assert!(!accepted);
        let reason = rejection_of(&client);
        assert!(
            reason.starts_with("host key verification failed: "),
            "{reason}"
        );
        assert!(reason.contains("ssh-rsa"), "{reason}");
        assert!(reason.contains("第 1 行"), "{reason}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "拒绝时不得把新算法的 key 追加进去"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn check_server_key_rejects_revoked_key_without_appending() {
        let dir = known_hosts_test_dir("reject-revoked");
        let path = dir.join("known_hosts");
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let original = format!("@revoked * {}\n", key_fields(&pub_a));
        std::fs::write(&path, &original).unwrap();
        let mut client = test_client(path.clone());

        assert!(!client.check_server_key(&pub_a).await.unwrap());
        let reason = rejection_of(&client);
        assert!(
            reason.starts_with("host key verification failed: "),
            "{reason}"
        );
        assert!(reason.contains("@revoked"), "{reason}");
        assert!(reason.contains("已被吊销"), "{reason}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "被吊销的 key 不得 accept-new 追加"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn check_server_key_accepts_and_records_new_host() {
        let dir = known_hosts_test_dir("accept-new");
        let path = dir.join("known_hosts");
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let mut client = test_client(path.clone());

        assert!(client.check_server_key(&pub_key).await.unwrap());
        assert!(client.notes.rejection.lock().unwrap().is_none());
        let raw = read_known_hosts(&path).unwrap();
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_key),
            HostKeyMatch::Match
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn check_server_key_rejects_unwritable_known_hosts_for_new_host() {
        // 父路径是个文件:新主机的 key 写不进去 → 拒绝,不悄悄放行。
        let dir = known_hosts_test_dir("reject-unwritable");
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let mut client = test_client(blocker.join("known_hosts"));

        let accepted = client
            .check_server_key(&test_pubkey_from_bytes(KEY_BYTES_A))
            .await
            .unwrap();
        assert!(!accepted, "写不进 known_hosts 时不得放行");
        // 两个平台走到的分支不同,但都是 fail-closed 拒绝:Windows 读 `文件/known_hosts`
        // 得到 NotFound(当成空文件、按新主机去写,写失败);Linux 读它就得到 ENOTDIR
        // (os error 20),走「读不了」分支。要守的是上面的 `!accepted`。
        let reason = rejection_of(&client);
        assert!(
            reason.starts_with("known_hosts write failed (")
                || reason.starts_with("known_hosts unreadable ("),
            "{reason}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn disconnected_records_the_first_end_reason() {
        use russh::client::{DisconnectReason, RemoteDisconnectInfo};
        let notes = Arc::new(ClientNotes::default());
        let mut client = MtClient::new("h".into(), PathBuf::from("kh"), Arc::clone(&notes));

        let info = RemoteDisconnectInfo {
            reason_code: russh::Disconnect::NoMoreAuthMethodsAvailable,
            message: "Too many authentication failures".into(),
            lang_tag: String::new(),
        };
        let result = client
            .disconnected(DisconnectReason::ReceivedDisconnect(info))
            .await;
        assert!(result.is_ok(), "服务器主动断开不是错误");
        assert_eq!(
            notes.ended(),
            Some(SessionEnd::ServerDisconnect(
                "Too many authentication failures".into()
            ))
        );

        // 只记第一次;错误照默认实现原样交回。
        let result = client
            .disconnected(DisconnectReason::Error(russh::Error::NoAuthMethod))
            .await;
        assert!(matches!(result, Err(russh::Error::NoAuthMethod)));
        assert!(matches!(
            notes.ended(),
            Some(SessionEnd::ServerDisconnect(_))
        ));

        let fresh = Arc::new(ClientNotes::default());
        let mut client = MtClient::new("h".into(), PathBuf::from("kh"), Arc::clone(&fresh));
        let _ = client
            .disconnected(DisconnectReason::Error(russh::Error::NoAuthMethod))
            .await;
        assert_eq!(fresh.ended(), Some(SessionEnd::NoMoreMethods));
    }

    #[test]
    fn connect_error_prefers_recorded_rejection() {
        let generic = russh::Error::UnknownKey;
        assert_eq!(
            connect_error(
                "h:22",
                &generic,
                Some("host key verification failed: x".into())
            ),
            "ssh connect to h:22 failed: host key verification failed: x"
        );
        assert_eq!(
            connect_error("j:2222", &generic, None),
            "ssh connect to j:2222 failed: Unknown server key"
        );
    }

    #[test]
    fn host_pattern_case_insensitive_match() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = format!("H.Example.COM {}\n", key_fields(&pub_key));
        assert_eq!(
            match_known_host(&raw, "h.example.com", &pub_key),
            HostKeyMatch::Match
        );
    }

    #[test]
    fn append_known_host_creates_parent_dir_and_writes_entry() {
        let dir = std::env::temp_dir().join(format!("mt-ssh-test-append-{}", std::process::id()));
        let path = dir.join("nested").join("known_hosts");
        let _ = std::fs::remove_dir_all(&dir);
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        append_known_host(&path, "[h.example.com]:2222", &pub_key).expect("append");
        let content = std::fs::read_to_string(&path).expect("read back");
        assert!(content.starts_with("[h.example.com]:2222 ssh-ed25519 "));
        assert!(content.ends_with("\n"));
        // 再追加一条不同 host,文件应有两行。
        append_known_host(&path, "other.example.com", &pub_key).expect("append 2");
        let content2 = std::fs::read_to_string(&path).expect("read 2");
        assert_eq!(content2.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_known_host_adds_the_missing_trailing_newline_first() {
        let dir = known_hosts_test_dir("no-trailing-newline");
        let path = dir.join("known_hosts");
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let pub_b = test_pubkey_from_bytes(KEY_BYTES_B);
        // 手工编辑留下的文件:最后一行没有换行。不补的话新条目粘在它后面,两条一起作废。
        std::fs::write(&path, format!("a.example.com {}", key_fields(&pub_a))).unwrap();
        append_known_host(&path, "b.example.com", &pub_b).unwrap();
        let raw = read_known_hosts(&path).unwrap();
        assert_eq!(raw.lines().count(), 2, "{raw}");
        assert!(raw.ends_with('\n'), "{raw}");
        assert_eq!(
            match_known_host(&raw, "a.example.com", &pub_a),
            HostKeyMatch::Match
        );
        assert_eq!(
            match_known_host(&raw, "b.example.com", &pub_b),
            HostKeyMatch::Match
        );
        // 已经以换行结尾:不多补空行。
        append_known_host(&path, "c.example.com", &pub_a).unwrap();
        let raw = read_known_hosts(&path).unwrap();
        assert_eq!(raw.lines().count(), 3, "{raw}");
        assert!(!raw.contains("\n\n"), "{raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
