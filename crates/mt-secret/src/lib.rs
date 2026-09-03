//! mt-secret —— 已存 SSH 密码的封存层(凭据库)。
//!
//! # 为什么有这一层
//!
//! `SshConnection.password` 此前是明文:`config.db` 的行 JSON、给 sidecar 读的
//! `config.json` 投影、`config.db.bak`、存量存档 `config.json.pre-sqlite` 四处都
//! 原样落盘。本 crate 把它换成**信封串**
//!
//! ```text
//! enc:v1:<base64(nonce(12) ‖ 密文 ‖ tag(16))>
//! ```
//!
//! AES-256-GCM,AAD 固定为用途串;主密钥 32 字节随机,存在
//! `{数据目录}/credential.key`([`KEY_FILE_NAME`]):
//!
//! - **Windows**:密钥文件内容经 DPAPI(`CryptProtectData`,当前用户范围)包裹,
//!   离开这个用户账户 / 这台机器就解不开;
//! - **macOS / Linux**:密钥文件 `0600`,只有本用户可读。
//!
//! # 谁在用
//!
//! - 主程序:`mt-config` 加载时把存量明文一次性封存并落库、保存时兜底封存;
//!   `AppStore` 在表单保存时封存;编辑表单 / 终端自动填充时解开。
//! - 三个 sidecar:经 `mt-ssh` 会话池在**认证那一刻**解开(它们读的投影里已是信封)。
//!   sidecar **只读**密钥文件、绝不创建 —— 密钥只由主程序生成,两个进程各写一把
//!   钥匙就谁也解不开谁封的东西。
//!
//! # 威胁模型(诚实版)
//!
//! 防的是「配置文件被拷走 / 被同步 / 被别的账户读到」:密文离开密钥没用,密钥
//! 离开当前用户(Windows)或 0600 权限(Unix)没用。**不防**同一账户下的本机进程 ——
//! 主程序自己就能无提示解密,这与浏览器保存密码是同一档保护。
//!
//! # 兼容与降级
//!
//! - [`Vault::reveal`] 对**不带 `enc:` 前缀**的值原样放行:升级窗口期(sidecar 先于
//!   主程序跑到、投影还是旧明文)与存档回退都靠这条。
//! - 解不开(密钥换了 / 密文被改)返回 [`SecretError::Undecryptable`],调用方
//!   给用户「请重新填写密码」;**绝不**把密文当密码送去认证。
//!
//! # 依赖方向
//!
//! 本 crate 在 `mt-core` 之上、被 `mt-config` / `mt-ssh` / `mt-app` 链接,并经 `mt-ssh`
//! 进入三个 sidecar 小二进制,所以依赖表只有 ring / base64 / serde / zeroize(前两个
//! sidecar 依赖树里本来就有),**不依赖任何带 rusqlite 的 crate**。`mt-core` 的叶子
//! 铁律不动:信封只是 `String`,`SshConnection` 的序列化形状一字未变。

#[cfg(windows)]
mod dpapi;

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// 信封串前缀。改格式就换版本号(`enc:v2:`),旧版本认不出会报
/// [`SecretError::UnsupportedEnvelope`] 而不是把它当明文。
pub const ENVELOPE_PREFIX: &str = "enc:v1:";
/// 主密钥文件名,与 `config.db` / `config.json` 同目录。
pub const KEY_FILE_NAME: &str = "credential.key";

const AAD: &[u8] = b"mini-term:ssh-password:v1";
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
/// 所有信封共用的判别前缀:`enc:` 开头一律视为封存值,哪怕版本认不出。
const SEALED_MARK: &str = "enc:";

/// 封存层的失败面。`Display` 文案是给日志 / 提示框直接用的中文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// 密钥文件不存在(sidecar 侧:主程序还没生成过)。
    KeyMissing(PathBuf),
    /// 密钥文件读不了 / 格式不对 / DPAPI 解不开。
    KeyUnreadable(String),
    /// 密钥文件写不出去。
    KeyWrite(String),
    /// 信封解不开:密钥不是封存时那把,或密文被改过。
    Undecryptable,
    /// 信封版本认不出(比本程序新的格式)。
    UnsupportedEnvelope(String),
    /// 系统随机数不可用。
    Rng,
    /// 定位不到数据目录(sidecar 侧的平台分支返回 `None`)。
    DataDirUnknown,
    /// AEAD 本身报错(明文超长之类,实际不会发生)。
    Internal(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyMissing(path) => write!(
                f,
                "凭据密钥文件不存在({}),请先启动 mini-term 主程序生成",
                path.display()
            ),
            Self::KeyUnreadable(detail) => write!(f, "凭据密钥文件不可用: {detail}"),
            Self::KeyWrite(detail) => write!(f, "凭据密钥文件写入失败: {detail}"),
            Self::Undecryptable => write!(
                f,
                "已存密码无法解密(密钥文件可能已更换或丢失),请重新填写密码"
            ),
            Self::UnsupportedEnvelope(tag) => {
                write!(f, "已存密码的封存格式({tag})比本程序新,请升级 mini-term")
            }
            Self::Rng => write!(f, "系统随机数不可用"),
            Self::DataDirUnknown => write!(f, "定位不到 mini-term 数据目录"),
            Self::Internal(detail) => write!(f, "封存失败: {detail}"),
        }
    }
}

impl std::error::Error for SecretError {}

/// 这个值是不是封存过的信封(而不是遗留明文)。
pub fn is_sealed(value: &str) -> bool {
    value.starts_with(SEALED_MARK)
}

/// `{data_dir}/credential.key`。
pub fn key_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(KEY_FILE_NAME)
}

// ─── 密钥文件 ─────────────────────────────────────────────────

/// 密钥文件的磁盘形状。`scheme` 说明 `key` 字段是什么:
/// - `dpapi`:DPAPI blob(Windows,当前用户范围);
/// - `plain`:裸密钥,靠文件权限 0600 保护(Unix)。
#[derive(Serialize, Deserialize)]
struct KeyFile {
    version: u32,
    scheme: String,
    key: String,
}

const KEY_FILE_VERSION: u32 = 1;

fn encode_key_file(key: &[u8; KEY_LEN]) -> Result<String, SecretError> {
    #[cfg(windows)]
    let (scheme, payload) = ("dpapi", dpapi::protect(key).map_err(SecretError::KeyWrite)?);
    #[cfg(not(windows))]
    let (scheme, payload) = ("plain", key.to_vec());
    let file = KeyFile {
        version: KEY_FILE_VERSION,
        scheme: scheme.to_string(),
        key: B64.encode(payload),
    };
    serde_json::to_string_pretty(&file).map_err(|e| SecretError::KeyWrite(e.to_string()))
}

fn decode_key_file(raw: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>, SecretError> {
    let file: KeyFile = serde_json::from_slice(raw)
        .map_err(|e| SecretError::KeyUnreadable(format!("不是有效的密钥文件: {e}")))?;
    if file.version != KEY_FILE_VERSION {
        return Err(SecretError::KeyUnreadable(format!(
            "密钥文件版本 {} 不受支持",
            file.version
        )));
    }
    let payload = B64
        .decode(file.key.as_bytes())
        .map_err(|e| SecretError::KeyUnreadable(format!("密钥 base64 损坏: {e}")))?;
    let key_bytes: Zeroizing<Vec<u8>> = match file.scheme.as_str() {
        "plain" => Zeroizing::new(payload),
        "dpapi" => {
            #[cfg(windows)]
            {
                Zeroizing::new(dpapi::unprotect(&payload).map_err(SecretError::KeyUnreadable)?)
            }
            #[cfg(not(windows))]
            {
                return Err(SecretError::KeyUnreadable(
                    "这份密钥由 Windows DPAPI 包裹,只能在生成它的 Windows 用户账户下解开"
                        .into(),
                ));
            }
        }
        other => {
            return Err(SecretError::KeyUnreadable(format!(
                "密钥保护方式 {other} 不受支持"
            )))
        }
    };
    if key_bytes.len() != KEY_LEN {
        return Err(SecretError::KeyUnreadable(format!(
            "密钥长度 {} 不是 {KEY_LEN} 字节",
            key_bytes.len()
        )));
    }
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    key.copy_from_slice(&key_bytes);
    Ok(key)
}

/// 写密钥文件:同目录临时文件 → rename。Unix 下临时文件一出生就是 0600。
fn write_key_file(path: &Path, content: &str) -> Result<(), SecretError> {
    let tmp = path.with_extension(format!("key.tmp-{}", std::process::id()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| -> io::Result<()> {
        use std::io::Write;
        let mut file = opts.open(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| SecretError::KeyWrite(format!("{}: {e}", path.display())))
}

// ─── 凭据库 ───────────────────────────────────────────────────

/// 一把主密钥。`Clone` 廉价(共享同一份密钥),离开作用域清零。
#[derive(Clone)]
pub struct Vault {
    key: Arc<Zeroizing<[u8; KEY_LEN]>>,
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vault(<redacted>)")
    }
}

impl Vault {
    /// 用给定密钥建库(测试 / 纯内存场景)。
    pub fn from_key(key: [u8; KEY_LEN]) -> Self {
        Self {
            key: Arc::new(Zeroizing::new(key)),
        }
    }

    /// 生成一把新的随机密钥,**不落盘**。
    pub fn generate() -> Result<Self, SecretError> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        SystemRandom::new()
            .fill(&mut *key)
            .map_err(|_| SecretError::Rng)?;
        Ok(Self {
            key: Arc::new(key),
        })
    }

    /// 打开 `{data_dir}/credential.key`。**不存在不创建**(sidecar 走这条)。
    pub fn open(data_dir: &Path) -> Result<Self, SecretError> {
        let path = key_file_path(data_dir);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(SecretError::KeyMissing(path))
            }
            Err(e) => {
                return Err(SecretError::KeyUnreadable(format!(
                    "{}: {e}",
                    path.display()
                )))
            }
        };
        Ok(Self {
            key: Arc::new(decode_key_file(&raw)?),
        })
    }

    /// 打开,不存在就生成一把并落盘(只有主程序走这条)。
    ///
    /// 落盘后**立刻重读一遍**:DPAPI 包裹或权限设置要是在这台机器上根本不工作,
    /// 宁可现在就失败,也不要先用一把再也解不开的钥匙封存一轮密码。
    pub fn open_or_create(data_dir: &Path) -> Result<Self, SecretError> {
        match Self::open(data_dir) {
            Err(SecretError::KeyMissing(_)) => {}
            other => return other,
        }
        fs::create_dir_all(data_dir)
            .map_err(|e| SecretError::KeyWrite(format!("{}: {e}", data_dir.display())))?;
        let fresh = Self::generate()?;
        let content = encode_key_file(&fresh.key)?;
        let path = key_file_path(data_dir);
        // 两个进程同时首启的窗口(理论上只有主程序会创建):rename 前再看一眼,
        // 有了就用别人那把,绝不覆盖。
        if path.exists() {
            return Self::open(data_dir);
        }
        write_key_file(&path, &content)?;
        let reread = Self::open(data_dir)?;
        if *reread.key != *fresh.key {
            return Err(SecretError::KeyUnreadable(
                "刚写出的密钥文件读回来不一致".into(),
            ));
        }
        Ok(reread)
    }

    fn aead_key(&self) -> Result<LessSafeKey, SecretError> {
        UnboundKey::new(&AES_256_GCM, &**self.key)
            .map(LessSafeKey::new)
            .map_err(|_| SecretError::Internal("密钥长度不对".into()))
    }

    /// 明文 → 信封串。每次调用 nonce 都是新的,同一明文两次封存得到不同信封 ——
    /// 需要「没改就别换」语义的调用方(`AppStore::upsert_ssh_connection`)自己先
    /// [`reveal`](Self::reveal) 比对。
    pub fn seal(&self, plaintext: &str) -> Result<String, SecretError> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| SecretError::Rng)?;
        let key = self.aead_key()?;
        let mut buf = Zeroizing::new(plaintext.as_bytes().to_vec());
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(AAD),
            &mut *buf,
        )
        .map_err(|_| SecretError::Internal("AEAD seal 失败".into()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + buf.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&buf);
        Ok(format!("{ENVELOPE_PREFIX}{}", B64.encode(out)))
    }

    /// 信封串 → 明文。**不带 `enc:` 前缀的值原样放行**(遗留明文 / 升级窗口期)。
    pub fn reveal(&self, stored: &str) -> Result<String, SecretError> {
        if !is_sealed(stored) {
            return Ok(stored.to_string());
        }
        let Some(payload) = stored.strip_prefix(ENVELOPE_PREFIX) else {
            let tag = stored.splitn(3, ':').nth(1).unwrap_or("?").to_string();
            return Err(SecretError::UnsupportedEnvelope(format!("enc:{tag}")));
        };
        let bytes = B64
            .decode(payload.as_bytes())
            .map_err(|_| SecretError::Undecryptable)?;
        if bytes.len() < NONCE_LEN + TAG_LEN {
            return Err(SecretError::Undecryptable);
        }
        let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| SecretError::Undecryptable)?;
        let key = self.aead_key()?;
        let mut buf = Zeroizing::new(ciphertext.to_vec());
        let plain = key
            .open_in_place(nonce, Aad::from(AAD), &mut *buf)
            .map_err(|_| SecretError::Undecryptable)?;
        String::from_utf8(plain.to_vec()).map_err(|_| SecretError::Undecryptable)
    }
}

// ─── 进程级凭据库 ─────────────────────────────────────────────

static GLOBAL: OnceLock<Vault> = OnceLock::new();

/// 登记进程级凭据库。**先到先得**,再登记返回 `false` 并忽略 —— 主程序在
/// `ConfigStore::load` 里登记它数据目录那把(dev 实例的 `MT_APP_DATA_DIR` 隔离
/// 目录也因此走对)。
pub fn install(vault: Vault) -> bool {
    GLOBAL.set(vault).is_ok()
}

/// 进程级凭据库。没登记过就**懒加载**:在 `mt_core::config_json_path()` 的同目录
/// 找密钥文件、只读不建 —— 这是三个 sidecar 的路径,它们读的投影就在那个目录,
/// 密钥必须与投影同源(**刻意不认** `MT_APP_DATA_DIR`:sidecar 读的投影不认它,
/// 密钥跟着它走就会拿 dev 实例的钥匙去开装机版的信封)。成功才缓存,失败下次重试。
pub fn global() -> Result<Vault, SecretError> {
    if let Some(vault) = GLOBAL.get() {
        return Ok(vault.clone());
    }
    let dir = mt_core::config_json_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or(SecretError::DataDirUnknown)?;
    let vault = Vault::open(&dir)?;
    let _ = GLOBAL.set(vault.clone());
    Ok(vault)
}

/// 用进程级凭据库解开一个已存值。遗留明文原样放行、**不碰**密钥文件 ——
/// sidecar 在升级窗口期读到旧明文投影时不该因为密钥还没生成而失败。
pub fn reveal_global(stored: &str) -> Result<String, SecretError> {
    if !is_sealed(stored) {
        return Ok(stored.to_string());
    }
    global()?.reveal(stored)
}

/// 用进程级凭据库封存一个明文。
pub fn seal_global(plaintext: &str) -> Result<String, SecretError> {
    global()?.seal(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mt-secret-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn seal_reveal_round_trip() {
        let vault = Vault::generate().unwrap();
        let sealed = vault.seal("hunter2 密码 with spaces ").unwrap();
        assert!(sealed.starts_with(ENVELOPE_PREFIX));
        assert!(is_sealed(&sealed));
        assert!(!sealed.contains("hunter2"), "信封里不能露出明文");
        assert_eq!(vault.reveal(&sealed).unwrap(), "hunter2 密码 with spaces ");
    }

    #[test]
    fn same_plaintext_seals_differently_each_time() {
        let vault = Vault::generate().unwrap();
        let a = vault.seal("same").unwrap();
        let b = vault.seal("same").unwrap();
        assert_ne!(a, b, "nonce 每次都新");
        assert_eq!(vault.reveal(&a).unwrap(), vault.reveal(&b).unwrap());
    }

    #[test]
    fn empty_string_round_trips() {
        let vault = Vault::generate().unwrap();
        let sealed = vault.seal("").unwrap();
        assert!(is_sealed(&sealed));
        assert_eq!(vault.reveal(&sealed).unwrap(), "");
    }

    #[test]
    fn legacy_plaintext_passes_through() {
        let vault = Vault::generate().unwrap();
        assert!(!is_sealed("s3cret"));
        assert_eq!(vault.reveal("s3cret").unwrap(), "s3cret");
        assert_eq!(vault.reveal("").unwrap(), "");
    }

    #[test]
    fn wrong_key_or_tampering_is_undecryptable_not_garbage() {
        let vault = Vault::generate().unwrap();
        let other = Vault::generate().unwrap();
        let sealed = vault.seal("hunter2").unwrap();
        assert_eq!(other.reveal(&sealed), Err(SecretError::Undecryptable));

        // 翻一个 base64 字符
        let mut chars: Vec<char> = sealed.chars().collect();
        let idx = chars.len() - 5;
        chars[idx] = if chars[idx] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert_eq!(vault.reveal(&tampered), Err(SecretError::Undecryptable));

        // 截断
        assert_eq!(
            vault.reveal(&format!("{ENVELOPE_PREFIX}AAAA")),
            Err(SecretError::Undecryptable)
        );
        // 不是 base64
        assert_eq!(
            vault.reveal(&format!("{ENVELOPE_PREFIX}***")),
            Err(SecretError::Undecryptable)
        );
    }

    #[test]
    fn unknown_envelope_version_is_rejected_not_passed_through() {
        let vault = Vault::generate().unwrap();
        assert!(is_sealed("enc:v9:whatever"));
        assert_eq!(
            vault.reveal("enc:v9:whatever"),
            Err(SecretError::UnsupportedEnvelope("enc:v9".into()))
        );
    }

    #[test]
    fn key_file_is_created_once_and_reopened_with_same_key() {
        let dir = temp_dir("keyfile");
        assert!(
            matches!(Vault::open(&dir), Err(SecretError::KeyMissing(p)) if p == key_file_path(&dir)),
            "只读打开不该创建"
        );
        assert!(!key_file_path(&dir).exists());

        let first = Vault::open_or_create(&dir).unwrap();
        assert!(key_file_path(&dir).exists());
        let sealed = first.seal("hunter2").unwrap();

        let second = Vault::open(&dir).unwrap();
        assert_eq!(second.reveal(&sealed).unwrap(), "hunter2", "重开拿到的是同一把钥匙");
        let third = Vault::open_or_create(&dir).unwrap();
        assert_eq!(third.reveal(&sealed).unwrap(), "hunter2", "已存在时绝不覆盖");

        // 文件里不能直接躺着裸密钥的 base64(Windows 是 DPAPI blob;Unix 靠 0600,
        // 这条只在 Windows 断言)
        let raw = fs::read_to_string(key_file_path(&dir)).unwrap();
        let parsed: KeyFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.version, 1);
        #[cfg(windows)]
        {
            assert_eq!(parsed.scheme, "dpapi");
            assert!(!raw.contains(&B64.encode(&**first.key)), "裸密钥不能出现在文件里");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(parsed.scheme, "plain");
            let mode = fs::metadata(key_file_path(&dir)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "Unix 密钥文件必须 0600");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_key_file_is_unreadable_not_missing() {
        let dir = temp_dir("corrupt");
        fs::write(key_file_path(&dir), "{ not json").unwrap();
        assert!(matches!(
            Vault::open(&dir),
            Err(SecretError::KeyUnreadable(_))
        ));
        // open_or_create 也不许在坏文件上覆盖出一把新钥匙
        assert!(matches!(
            Vault::open_or_create(&dir),
            Err(SecretError::KeyUnreadable(_))
        ));
        assert_eq!(fs::read_to_string(key_file_path(&dir)).unwrap(), "{ not json");

        fs::write(
            key_file_path(&dir),
            r#"{"version":1,"scheme":"plain","key":"AAAA"}"#,
        )
        .unwrap();
        assert!(matches!(
            Vault::open(&dir),
            Err(SecretError::KeyUnreadable(msg)) if msg.contains("长度")
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plain_scheme_key_file_is_portable_across_platforms() {
        // 手写一份 plain 方案的密钥文件(Unix 产物拷到哪都能读)
        let dir = temp_dir("plain");
        let key = [7u8; KEY_LEN];
        let file = KeyFile {
            version: 1,
            scheme: "plain".into(),
            key: B64.encode(key),
        };
        fs::write(key_file_path(&dir), serde_json::to_string(&file).unwrap()).unwrap();
        let vault = Vault::open(&dir).unwrap();
        let sealed = Vault::from_key(key).seal("x").unwrap();
        assert_eq!(vault.reveal(&sealed).unwrap(), "x");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn debug_never_prints_key_material() {
        let vault = Vault::from_key([0x42; KEY_LEN]);
        assert_eq!(format!("{vault:?}"), "Vault(<redacted>)");
    }
}
