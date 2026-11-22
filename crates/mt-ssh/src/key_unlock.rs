//! 带口令私钥的解密:终端 shell 在终端里问到口令后用(见 `auth.rs` 的 `unlock_candidate`)。会话池
//! 拿不到口令,照旧只认 ssh-agent 里的同一把 key,不经过这里。
//!
//! 认得的格式:
//!
//! - OpenSSH 新格式(`-----BEGIN OPENSSH PRIVATE KEY-----`,bcrypt-pbkdf + aes256-ctr 等):
//!   `ssh_key::PrivateKey::decrypt`;
//! - PKCS#8 加密(`-----BEGIN ENCRYPTED PRIVATE KEY-----`)与带口令的 PuTTY PPK:russh 的
//!   `decode_secret_key(文本, Some(口令))`;
//! - 传统 PEM 加密(`Proc-Type: 4,ENCRYPTED` + `DEK-Info`:`ssh-keygen -m PEM`、`openssl rsa -aes128`、
//!   PuTTYgen 导出的旧格式之类)。russh 不开 `rsa` feature(见 Cargo.toml)时连明文 PKCS#1 都不认,
//!   这里自己做:OpenSSL 的 `EVP_BytesToKey`(MD5、一轮、盐 = IV 前 8 字节)派生密钥,
//!   AES-128/192/256-CBC 或 DES-EDE3-CBC 解密、去掉 PKCS#7 填充,RSA 走与明文 PKCS#1 同一条转换
//!   (`auth::pkcs1_der_to_private_key`),EC(SEC1)换成明文 PEM 交给 russh。
//!
//! 口令错只能从「解出来的东西不对」看出来(校验字 / 填充 / DER 对不上),一律按口令错重问。

use std::path::Path;
use std::sync::Arc;

use russh::keys::ssh_key::Cipher;
use russh::keys::PrivateKey;

use crate::auth::{self, NUMBER_OF_PASSWORD_PROMPTS};
use crate::interact::Interaction;

/// 在终端里问口令解开私钥的结果。
pub(crate) enum Unlock {
    Key(PrivateKey),
    /// 用户输了空口令:跳过这把 key(OpenSSH 同样如此)。
    Skipped,
    /// 问满次数,口令都不对。
    WrongPassphrase,
    /// 读不了文件 / 不认识这种加密格式 / 解出来的东西不受支持。
    Unsupported(String),
    Cancelled,
}

/// 读私钥文件,在终端里问 `Enter passphrase for key '<路径>': `(不回显,OpenSSH 原文)解开它,
/// 最多问 [`NUMBER_OF_PASSWORD_PROMPTS`] 次;口令错直接重问(OpenSSH 同样不另外提示)。
pub(crate) async fn unlock_with_prompts(path: &Path, interaction: &Interaction) -> Unlock {
    let text = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => return Unlock::Unsupported(format!("cannot read the file: {e}")),
    };
    let locked = match LockedKey::parse(&text) {
        Ok(locked) => Arc::new(locked),
        Err(reason) => return Unlock::Unsupported(reason),
    };
    let prompt = format!("Enter passphrase for key '{}': ", path.display());
    for _ in 0..NUMBER_OF_PASSWORD_PROMPTS {
        let Some(passphrase) = interaction.read_line(prompt.clone(), false).await else {
            return Unlock::Cancelled;
        };
        if passphrase.is_empty() {
            return Unlock::Skipped;
        }
        // 解密是纯本地计算(bcrypt-pbkdf 默认 16 轮,debug 构建要好几秒):放到阻塞线程上跑,
        // 期间同样不计入网络超时。
        let attempt = {
            let _paused = interaction.pause();
            let locked = Arc::clone(&locked);
            tokio::task::spawn_blocking(move || locked.decrypt(&passphrase)).await
        };
        match attempt {
            Ok(Ok(key)) => return Unlock::Key(key),
            Ok(Err(Decrypt::WrongPassphrase)) => continue,
            Ok(Err(Decrypt::Failed(reason))) => return Unlock::Unsupported(reason),
            Err(e) => return Unlock::Unsupported(format!("decryption did not finish: {e}")),
        }
    }
    Unlock::WrongPassphrase
}

/// 带口令的 PuTTY PPK(`Encryption:` 不是 `none`)。`load_identity` 不带口令解不了它、把它归为
/// 读取失败;终端里可以问口令。
pub(crate) fn is_encrypted_ppk_file(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|bytes| is_encrypted_ppk(&String::from_utf8_lossy(&bytes)))
}

fn is_encrypted_ppk(text: &str) -> bool {
    text.trim_start().starts_with("PuTTY-User-Key-File-")
        && text.lines().any(|line| {
            line.strip_prefix("Encryption:")
                .is_some_and(|value| !value.trim().eq_ignore_ascii_case("none"))
        })
}

/// 一把等着口令的私钥。
enum LockedKey {
    OpenSsh(PrivateKey),
    /// PKCS#8 加密 / PuTTY PPK:整段文本交给 russh 带口令解码。
    Russh(String),
    LegacyPem(LegacyPem),
}

/// 一次解密失败。
#[derive(Debug, PartialEq, Eq)]
enum Decrypt {
    WrongPassphrase,
    /// 与口令无关的失败(格式受不支持之类),不必再问。
    Failed(String),
}

impl LockedKey {
    /// 认出加密格式(不需要口令)。认不出就不问口令,直接报原因。
    fn parse(text: &str) -> Result<Self, String> {
        if text.trim_start().starts_with("PuTTY-User-Key-File-")
            || text.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----")
        {
            return Ok(Self::Russh(text.to_string()));
        }
        if text.contains("-----BEGIN OPENSSH PRIVATE KEY-----") {
            let key = PrivateKey::from_openssh(text)
                .map_err(|e| format!("invalid OpenSSH private key ({e})"))?;
            return if key.is_encrypted() {
                Ok(Self::OpenSsh(key))
            } else {
                Err("the private key is not encrypted".into())
            };
        }
        match LegacyPem::parse(text)? {
            Some(pem) => Ok(Self::LegacyPem(pem)),
            None => Err("unrecognised encrypted private key format".into()),
        }
    }

    fn decrypt(&self, passphrase: &str) -> Result<PrivateKey, Decrypt> {
        match self {
            Self::OpenSsh(key) => key.decrypt(passphrase).map_err(|e| match e {
                russh::keys::ssh_key::Error::Crypto => Decrypt::WrongPassphrase,
                other => Decrypt::Failed(other.to_string()),
            }),
            Self::Russh(text) => russh::keys::decode_secret_key(text, Some(passphrase))
                .map_err(|_| Decrypt::WrongPassphrase),
            Self::LegacyPem(pem) => pem.decrypt(passphrase),
        }
    }
}

/// 传统 PEM 的私钥种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PemKind {
    /// `-----BEGIN RSA PRIVATE KEY-----`(PKCS#1)。
    Rsa,
    /// `-----BEGIN EC PRIVATE KEY-----`(SEC1)。
    Ec,
}

/// 传统 PEM 加密私钥(OpenSSL 的 `Proc-Type: 4,ENCRYPTED` + `DEK-Info: <算法>,<IV>`)。
struct LegacyPem {
    kind: PemKind,
    cipher: Cipher,
    iv: Vec<u8>,
    /// base64 解出来的密文。
    body: Vec<u8>,
}

impl LegacyPem {
    /// 不是带 `Proc-Type: 4,ENCRYPTED` 的传统 RSA / EC PEM 时返回 `Ok(None)`。
    fn parse(text: &str) -> Result<Option<Self>, String> {
        let mut lines = text.lines().map(str::trim);
        let label = lines.by_ref().find_map(|line| {
            line.strip_prefix("-----BEGIN ")
                .and_then(|rest| rest.strip_suffix(" PRIVATE KEY-----"))
        });
        let kind = match label {
            Some("RSA") => PemKind::Rsa,
            Some("EC") => PemKind::Ec,
            Some("DSA") => return Err("DSA keys are not supported".into()),
            _ => return Ok(None),
        };
        let (mut encrypted, mut dek_info, mut body) = (false, None, String::new());
        for line in lines {
            if line.starts_with("-----END ") {
                break;
            }
            match line.split_once(':') {
                Some(("Proc-Type", value)) => encrypted |= value.contains("ENCRYPTED"),
                Some(("DEK-Info", value)) => dek_info = Some(value.trim().to_string()),
                Some(_) => {}
                None => body.push_str(line),
            }
        }
        if !encrypted {
            return Ok(None);
        }
        let dek_info = dek_info.ok_or("encrypted PEM key without a DEK-Info header")?;
        let (algorithm, iv_hex) = dek_info
            .split_once(',')
            .ok_or_else(|| format!("malformed DEK-Info header ({dek_info})"))?;
        let cipher = match algorithm.trim().to_ascii_uppercase().as_str() {
            "AES-128-CBC" => Cipher::Aes128Cbc,
            "AES-192-CBC" => Cipher::Aes192Cbc,
            "AES-256-CBC" => Cipher::Aes256Cbc,
            "DES-EDE3-CBC" => Cipher::TDesCbc,
            other => return Err(format!("unsupported PEM encryption {other}")),
        };
        let iv = hex_decode(iv_hex.trim())
            .filter(|iv| cipher.key_and_iv_size().is_some_and(|(_, size)| iv.len() == size))
            .ok_or_else(|| format!("malformed IV in DEK-Info header ({dek_info})"))?;
        let body = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(body.as_bytes())
                .map_err(|e| format!("invalid base64 in encrypted PEM key: {e}"))?
        };
        if body.is_empty() || body.len() % cipher.block_size() != 0 {
            return Err("encrypted PEM key body is not a whole number of cipher blocks".into());
        }
        Ok(Some(Self {
            kind,
            cipher,
            iv,
            body,
        }))
    }

    fn decrypt(&self, passphrase: &str) -> Result<PrivateKey, Decrypt> {
        let (key_len, _) = self
            .cipher
            .key_and_iv_size()
            .ok_or_else(|| Decrypt::Failed(format!("unsupported cipher {}", self.cipher)))?;
        // `parse` 已保证 IV 长度与算法一致(至少 8 字节)。
        let key = evp_bytes_to_key(passphrase.as_bytes(), &self.iv[..8], key_len);
        let mut plain = self.body.clone();
        self.cipher
            .decrypt(&key, &self.iv, &mut plain, None)
            .map_err(|e| Decrypt::Failed(format!("{} decryption failed: {e}", self.cipher)))?;
        let len =
            pkcs7_unpadded_len(&plain, self.cipher.block_size()).ok_or(Decrypt::WrongPassphrase)?;
        let der = &plain[..len];
        match self.kind {
            PemKind::Rsa => {
                auth::pkcs1_der_to_private_key(der).map_err(|_| Decrypt::WrongPassphrase)
            }
            PemKind::Ec => russh::keys::decode_secret_key(&pem_text("EC PRIVATE KEY", der), None)
                .map_err(|_| Decrypt::WrongPassphrase),
        }
    }
}

/// OpenSSL `EVP_BytesToKey`(MD5、一轮)只取密钥部分:`D_i = MD5(D_{i-1} ‖ 口令 ‖ 盐)`,依次拼到
/// 够长。传统 PEM 的盐是 IV 的前 8 字节,IV 本身写在 `DEK-Info` 里、不派生。
fn evp_bytes_to_key(passphrase: &[u8], salt: &[u8], key_len: usize) -> Vec<u8> {
    let mut key = Vec::with_capacity(key_len + 16);
    let mut previous: Option<[u8; 16]> = None;
    while key.len() < key_len {
        let mut input = Vec::with_capacity(16 + passphrase.len() + salt.len());
        if let Some(previous) = previous {
            input.extend_from_slice(&previous);
        }
        input.extend_from_slice(passphrase);
        input.extend_from_slice(salt);
        let digest = md5::compute(&input).0;
        key.extend_from_slice(&digest);
        previous = Some(digest);
    }
    key.truncate(key_len);
    key
}

/// PKCS#7 填充之后的有效长度;填充不对(多半是口令错、解出了乱码)返回 `None`。
fn pkcs7_unpadded_len(data: &[u8], block: usize) -> Option<usize> {
    let &last = data.last()?;
    let pad = usize::from(last);
    if pad == 0 || pad > block || pad > data.len() {
        return None;
    }
    let len = data.len() - pad;
    data[len..].iter().all(|&byte| byte == last).then_some(len)
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// 把 DER 包回一段明文 PEM(每行 64 个 base64 字符)。
fn pem_text(label: &str, der: &[u8]) -> String {
    use base64::Engine;
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut text = format!("-----BEGIN {label}-----\n");
    for chunk in body.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        text.push('\n');
    }
    text.push_str("-----END ");
    text.push_str(label);
    text.push_str("-----\n");
    text
}

// ============================================================================
// tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const PASSPHRASE: &str = "mt-ssh-test";

    /// `ssh-keygen -t ed25519 -N mt-ssh-test` 一次性生成的**测试专用废弃密钥**(与 auth.rs 的
    /// 同一把),不对应任何真实主机 / 账号。
    const ENCRYPTED_OPENSSH_ED25519: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABA75HILGL
APMrA1XbjP6MESAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIBesQO+amJx6a3fp
PLZOFM3wPp281cA8v6Hp64YXCl5BAAAAoCWh+qD6Zu3kDVRYifuttM1K9ePMit4hediMNY
2HzoAjXBsTe/o6klBk7BpoKIWnd/xP43UUIIA8xum0M0SF/7rAun23qLvjDJ7N3tUTjb+b
mS+z48nBMju37Iflv39vxijIdth07d001dxNOtaTVjnVYoHOwHX0tTkXeyuD0CGMS7fCHp
v9aDxRcrMAaNVKMzZbmEjucR5L+VMM5njNnS4=
-----END OPENSSH PRIVATE KEY-----
";

    /// 下面四把是 OpenSSL 3.5 一次性生成的**测试专用废弃密钥**(口令 `mt-ssh-test`),
    /// 只用于验证各种传统加密格式能解开,不对应任何真实主机 / 账号:
    ///
    /// - RSA:auth.rs 里明文 PKCS#1 夹具 `PKCS1_RSA_FIXTURE` 经
    ///   `openssl rsa -traditional -aes128` 加密(`ssh-keygen -m PEM` 的默认算法);
    /// - EC:`openssl ecparam -name prime256v1 -genkey` 的同一把 key,分别经
    ///   `openssl ec -aes256` / `-des3` 与 `openssl pkcs8 -topk8 -v2 aes-256-cbc` 加密。
    const RSA_AES128_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-128-CBC,5B2EE4B5201B68BB45BEF2DCD00CEE9C

IO3rjZBgOCRBZSZO7ZUDFB7W9NpxWBQYKMZ3UECIST0zUAKgHD3zpSGiMTvIgsoN
netttVKtVBrEenFNjQIalfhBhSrgB17oFYr/+l6FJuULHHwWCc6WijV9AA9l8HM0
TeNIIjoi6v5/+kUfIIy3Q0UnCs50fj/JRFhg42C/+j42iw28rUorNwJMzeImBy56
ekBSsEbUVSz6E3Co2qyx4SrZl77PRFAVpR8Lol/Ay4Yq/aqZkGIqDUlXakIp5WqO
k0Hf7MaWlBHTcQdQFf+9E/8whb7MVyKzZ30fGjZt+qxCugFANb588oC+zL7d7M4O
+cQjeXk9vGNo8DelX4HXj8yo8t+BLN3WO/g9p2/JlRCngOdtocaiwUaIZDmn+hny
gSw8L1VMGCxtN6Wd+G6Zii4Z8G/AJ9o9nQlPtGK9WENKIL5cIS1oN5dpWdoY2plv
YULRyINjUEvJOIrYNAEbqTE5x1bcrSWs1O4AYv7Rf7eVrK3nXZhpQfUX4pF4dfkX
gW4Cl7br4NPLF4+yUHL9sYCoQ+wdSGnqZwHScnvv6lAU0Y+CsXDQQB/LDdTajN14
D4pq9XjywZSOpdmRWWpX7Bx6xEZs51AujtJ7MWMT3siuNZ0nQtlVTWtaACXFsJx3
d//GHUFG4bQrs/lvFo+cfdWrjDgez8dUSpZTxKbJr7UTgKTJ2S+zsLHolT0p86u9
E90xO3yLAj1Ia8C2kavwz2bUyDR14gnqJNuCzFkBl+22XWlk1NRwMxiqrH0xlqWE
GXtVpQLBMIRLKv5aopJzavavKODDeJxaH6A7qhi0/eWESutU82RiGNijrvCEvBFb
Qh8sWXJw2QJfAT1Fe9X+TwIk4sa4F2zOiQm5T9gi5kHvf6E4k83DfHZPQ8ruhcDA
Ud5tBB0zEl9MsQllZure1WH1le1uiC/Ord09IF6fdmT7awpEbwHVfDcu19U3eE6l
rHxSDu8qwzBJfKckOzEDjJxCqPngnOJcRByhC+1wU71rw4GiXpHvL1J3bjVgGW9B
r2Nxuujx5Om5GnowDOl1VdC+2HcKfuEIz/4wZJLopKNoHgk/JGiIJQfkLlx9J27r
Qs/+fcGSwcKc+2OLQvVCffkKLhiCsOCUF4jcTzgW7GjWg3PphHzMllK3CczinxbS
OwHWag+liMNLFTSCcsFHm9AsvWTYIJHJCbOKUk4xXU3aXQottyOaFNIucU/jEtv/
XyGq4BAA8yNWPMz3hHPl2IAa59z7IVwX8KhcMH55D1+7J2RGCbL8NyoyhSHGugCb
Tky0gOwjNRFs+E8U70kdFIcsPDW2TqfvG2VyofntteS+apYA/pgIqhkPpaLD50lJ
ztQyNcAqX8BO9SwU81KtwdJxowh/lDtqKSG0Bh59i6yxN8oua3fpJFPKcuAL4vbw
0zVI3L7VILY7mJuGJpaPeiwhMarULunQFk6/mfbfctVJ5rnU0fOQ0I+Dz+Yp9SjQ
C3HiziVgDwVXoP1XtlpZ2jNqCwdg5+/PPT0fD9zUUGG5niHGl4AUKy4I3XaIj6IM
XneHcqm0qn1N6MalCNFVF/XDY+3nnurtFGqWpGsOroDviL+MpMuSvV1KhovA865F
-----END RSA PRIVATE KEY-----
";

    const EC_AES256_PEM: &str = "-----BEGIN EC PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-256-CBC,1BE5B83F317F70900D052400992227A3

BxJx6mBc0c9RcmDc5REKyKXREceK4ripHXAgQNp+0fiG/arvGVr7De0MJ/4vGrQ8
IYYsGbJE6qs7gNtcNgFDuA8V5ZBT1WV3hjCHJdJoYwabDKewrkYLruBt7Iiz2HlO
kRy954GUTJKAG2s/8TR87uJELHnzbWDZ90gLWQKi+Dw=
-----END EC PRIVATE KEY-----
";

    const EC_DES3_PEM: &str = "-----BEGIN EC PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: DES-EDE3-CBC,0E04ADD29B1F1C90

GHF2O/X5tejOhpGYjkIKReqmQYFRTGDpdPm0Zy98LODyvMFTTMgSA2wFXWxZK7hU
6V9cdEs8b10kJckYSrytDPrVnKV8hgb2KCq3FQN09XWYEN3yBb7SudXQngcVZQ9o
nefjMjJzulxs0SJfv5mwRzoiNhoAa+f8wyQKXiUqJWo=
-----END EC PRIVATE KEY-----
";

    const EC_PKCS8: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----
MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBDaV+0wwPcK43Vf/bet
F0KxAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQg1S04uQ1pqJLUvJE
CbAYjASBkNtcyKSQehkTP0nLzmJ/8Dn3tYqymn9O88XR62Hxizd/xamCytCP/lEp
zmBLtmACUthJhgwVa11g5jGbIleLtP9pxsojMK/rZndQDbzR7KIqNbwY3zofHTAh
/0mZjBgMer9wLxmE6tyERk7HCjk1tMQtbOnIpaKEbGxwyBSI801Jk+TuZq+Cnn5E
vcfZuaOLcQ==
-----END ENCRYPTED PRIVATE KEY-----
";

    /// 上面那把 EC key 的公钥(`ssh-keygen -y`)。
    const EC_PUBLIC: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAAB\
                             BBLWeubnzkEvI6fP3nrpGwuMQsXFBDHbwAFV2sxGOJBOfGGsv+B0eLiku0VUKsO2uW7htN\
                             XXYteZ1gr65LfThQSk=";

    fn public_of(key: &PrivateKey) -> russh::keys::ssh_key::public::KeyData {
        key.public_key().key_data().clone()
    }

    fn ec_public() -> russh::keys::ssh_key::public::KeyData {
        russh::keys::ssh_key::PublicKey::from_openssh(EC_PUBLIC)
            .unwrap()
            .key_data()
            .clone()
    }

    fn unlock(text: &str, passphrase: &str) -> Result<PrivateKey, Decrypt> {
        LockedKey::parse(text).unwrap().decrypt(passphrase)
    }

    #[test]
    fn evp_bytes_to_key_matches_openssl() {
        // 已知答案:`openssl enc -<算法> -md md5 -pass pass:mt-ssh-test -S <盐> -P` 打印的 key。
        let hex = |bytes: Vec<u8>| bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();
        let salt = hex_decode("5B2EE4B5201B68BB").unwrap();
        assert_eq!(
            hex(evp_bytes_to_key(PASSPHRASE.as_bytes(), &salt, 16)),
            "9CD8B84BB41175AF50F1C1A3C477B517"
        );
        let salt = hex_decode("1BE5B83F317F7090").unwrap();
        assert_eq!(
            hex(evp_bytes_to_key(PASSPHRASE.as_bytes(), &salt, 32)),
            "6F7B15D3D20FE21496883E98FBC6ED4ADE9B4C34EC7F9F575B3D9681D92561CD"
        );
        let salt = hex_decode("0E04ADD29B1F1C90").unwrap();
        assert_eq!(
            hex(evp_bytes_to_key(PASSPHRASE.as_bytes(), &salt, 24)),
            "C724F2E3927FB74ED3C55CA6F0449B36BE9A1BC13FEF70A6"
        );
    }

    #[test]
    fn pkcs7_padding_is_checked() {
        assert_eq!(pkcs7_unpadded_len(&[1, 2, 3, 1], 4), Some(3));
        assert_eq!(pkcs7_unpadded_len(&[4, 4, 4, 4], 4), Some(0));
        assert_eq!(pkcs7_unpadded_len(&[1, 2, 2, 3], 4), None, "填充字节不一致");
        assert_eq!(pkcs7_unpadded_len(&[1, 2, 3, 0], 4), None);
        assert_eq!(pkcs7_unpadded_len(&[1, 2, 3, 5], 4), None, "比块还长的填充");
        assert_eq!(pkcs7_unpadded_len(&[], 4), None);
    }

    #[test]
    fn openssh_format_key_decrypts_with_the_right_passphrase_only() {
        assert_eq!(
            unlock(ENCRYPTED_OPENSSH_ED25519, "wrong").err(),
            Some(Decrypt::WrongPassphrase)
        );
        let key = unlock(ENCRYPTED_OPENSSH_ED25519, PASSPHRASE).unwrap();
        let locked = PrivateKey::from_openssh(ENCRYPTED_OPENSSH_ED25519).unwrap();
        assert_eq!(public_of(&key), public_of(&locked));
    }

    #[test]
    fn legacy_pem_rsa_decrypts_to_the_same_key_as_the_plaintext_pkcs1() {
        assert_eq!(
            unlock(RSA_AES128_PEM, "wrong").err(),
            Some(Decrypt::WrongPassphrase)
        );
        let key = unlock(RSA_AES128_PEM, PASSPHRASE).unwrap();
        assert!(key.algorithm().is_rsa());
        let plain = auth::try_parse_pkcs1_rsa(crate::auth::tests::PKCS1_RSA_FIXTURE)
            .unwrap()
            .unwrap();
        assert_eq!(public_of(&key), public_of(&plain));
    }

    #[test]
    fn legacy_pem_ec_and_pkcs8_decrypt_with_each_cipher() {
        for text in [EC_AES256_PEM, EC_DES3_PEM, EC_PKCS8] {
            assert_eq!(
                unlock(text, "wrong").err(),
                Some(Decrypt::WrongPassphrase),
                "{text}"
            );
            let key = unlock(text, PASSPHRASE).unwrap_or_else(|e| panic!("{e:?}: {text}"));
            assert_eq!(public_of(&key), ec_public(), "{text}");
        }
    }

    #[test]
    fn unsupported_or_plain_keys_are_reported_without_asking() {
        let dsa = "-----BEGIN DSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n\
                   DEK-Info: AES-128-CBC,00000000000000000000000000000000\n\nAAAA\n\
                   -----END DSA PRIVATE KEY-----\n";
        assert!(LockedKey::parse(dsa).err().unwrap().contains("DSA"));
        let des = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n\
                   DEK-Info: DES-CBC,0000000000000000\n\nAAAAAAAAAAA=\n\
                   -----END RSA PRIVATE KEY-----\n";
        assert!(LockedKey::parse(des)
            .err()
            .unwrap()
            .contains("unsupported PEM encryption DES-CBC"));
        // 没有 Proc-Type 的明文 PEM 不归这里管。
        let plain = "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n";
        assert!(LockedKey::parse(plain).is_err());
        assert!(LockedKey::parse("garbage").is_err());
    }

    #[test]
    fn encrypted_ppk_is_recognised_by_its_encryption_header() {
        let encrypted = "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: aes256-cbc\nComment: x\n";
        let plain = "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none\nComment: x\n";
        assert!(is_encrypted_ppk(encrypted));
        assert!(!is_encrypted_ppk(plain));
        assert!(!is_encrypted_ppk("-----BEGIN OPENSSH PRIVATE KEY-----"));
    }
}
