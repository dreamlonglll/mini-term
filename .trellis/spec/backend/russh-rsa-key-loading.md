# russh 加载 RSA 私钥的两个坑(格式 + 签名 hash)

> `mt-ssh-mcp` 用 `russh 0.61`(底层 `ssh-key 0.7`)做 SSH 客户端认证。用 RSA 私钥
> 连现代服务器时,会**连续踩两个独立的坑**:① `ssh-key` 不认传统 PKCS#1 PEM 格式;
> ② `PrivateKeyWithHashAlg::new(key, None)` 对 RSA 落到 SHA-1,被现代 OpenSSH 拒。
> 两个坑都让"系统 ssh 能登、mt-ssh-mcp 不能登",排查时容易只修一个就以为完事。

---

## 坑 1:`ssh-key` 只解析 OpenSSH / PKCS#8,不认传统 PKCS#1 / SEC1

`russh::keys::load_secret_key` 底层 `ssh-key 0.7` 仅解析两种明文私钥 PEM:

- OpenSSH:`-----BEGIN OPENSSH PRIVATE KEY-----`
- PKCS#8:`-----BEGIN PRIVATE KEY-----`

传统 **PKCS#1**(`-----BEGIN RSA PRIVATE KEY-----`,由 `ssh-keygen -m PEM`、OpenSSL、
各类云控制台如 **Oracle Cloud** 下发)与 **SEC1 EC**(`-----BEGIN EC PRIVATE KEY-----`)
不在其解析路径,直接报 `Unsupported key type RSA`。

**修法(纯 Rust,无外部 ssh-keygen)**:命中 `BEGIN RSA PRIVATE KEY` 标签时,自剥 PEM →
base64 解码成 DER → `rsa::RsaPrivateKey::from_pkcs1_der` → `ssh_key::private::RsaKeypair::try_from`
→ `ssh_key::PrivateKey::from`。见 `pool.rs::try_parse_pkcs1_rsa` / `load_private_key_compat`。

依赖要点(已验证,见 [Windows MSVC NASM 陷阱](./rust-crypto-on-windows-msvc.md) 与
[rand_core 多版本对齐](./rand-core-version-alignment.md)):

- `ssh-key` 须显式开 `rsa` feature;`rsa`/`ssh-key` 版本**精确锁定**到 russh 内部一致
  (`ssh-key =0.7.0-rc.10`、`rsa =0.10.0-rc.18`),否则 `PrivateKey`/`RsaPrivateKey` 跨
  crate-version 类型不可互换,`RsaKeypair::try_from(&rsa::RsaPrivateKey)` 直接编译失败。
- `rsa` **没有 `pem` feature**(PEM 方法 gated 在 `pkcs1/pem`,rsa 未传递);故走
  `from_pkcs1_der` + 自剥 PEM(复用已有 `base64` crate),**不要**写 `features=["pem"]`(会
  报 `rsa does not have that feature`)。
- 加密的传统 PEM(`Proc-Type: 4,ENCRYPTED`)不支持,给可操作指引而非吐底层晦涩错。

## 坑 2:RSA 公钥认证默认 SHA-1,被现代 OpenSSH 拒

`PrivateKeyWithHashAlg::new(key, hash_alg)` 的官方语义(russh `keys/key.rs`):

> For RSA, passing `None` is mapped to the legacy `sha-rsa` (SHA-1).
> For other keys, `hash_alg` is ignored.

而 **OpenSSH 8.8+(Ubuntu 22.04/24.04 等)默认禁用 SHA-1 的 `ssh-rsa` 公钥签名**。于是
`None` 让 RSA 认证报 `authentication failed: server rejected all configured methods`,
而同一把 key 用系统 `ssh` 却能登(系统 ssh 自动用 rsa-sha2-512/256)。

**修法**:按服务器通告的 `server-sig-algs`(EXT_INFO)选 hash。`new()` 虽对非 RSA key
自动忽略 hash,但 `best_supported_rsa_hash` 本身会**等最多 1s EXT_INFO**——所以要用
`key.algorithm().is_rsa()` 门控,**只在 RSA key 上查**,免得给 ed25519/ecdsa 白添延迟:

```rust
let rsa_hash = if key.algorithm().is_rsa() {
    match handle.best_supported_rsa_hash().await {
        Ok(Some(alg)) => alg,                                    // 通告: Sha512/Sha256, 或 None(仅 ssh-rsa)
        Ok(None) | Err(_) => Some(russh::keys::HashAlg::Sha512), // 未发 EXT_INFO: 回退 sha2-512
    }
} else {
    None                                                         // 非 RSA: 不查, hash 反正被 new() 忽略
};
let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
```

`best_supported_rsa_hash` 返回 `Ok(None)` 表示服务器没通告 EXT_INFO,文档明确「此时仍可
试 rsa-sha2-*」——所以回退 `Sha512` 比留 `None`(SHA-1)安全得多。`Ok(Some(None))` 则是
服务器明确只支持 legacy ssh-rsa,此时用 `None`(SHA-1)是对的。

---

## How to apply / 自检

新增或调试「russh 用私钥认证」的代码时:

1. **格式**:别假设 `load_secret_key` 吃所有 PEM;PKCS#1/SEC1 要走 fallback。
2. **hash**:RSA key 永远先 `best_supported_rsa_hash`,**绝不**给 `PrivateKeyWithHashAlg::new`
   传 `None`(=SHA-1)。用 `key.algorithm().is_rsa()` 门控查询:ed25519/ecdsa 不受 hash 影响
   (被忽略),也别让它们白等 `best_supported_rsa_hash` 的最多 1s EXT_INFO。
3. **端到端验证别只靠单测**:单测只能验「解析成功」,验不了「服务器接受签名」。两个坑
   分别卡在解析期与认证期,必须真连一台现代 OpenSSH 服务器跑一次 `ssh_exec`(或临时
   `examples/` 探针,用完即删)才算闭环。

---

## 可执行契约(Executable Contract)

### Scope / Trigger
infra/secrets:SSH 私钥加载与公钥认证。改 `pool.rs` 的私钥加载或 `authenticate` 的 hash 选择时适用。

### Signatures
```rust
// 加载私钥:russh 原生 + PKCS#1 fallback。Err 是给用户的诊断字符串(不含密钥字节)。
fn load_private_key_compat(path: &str) -> Result<russh::keys::PrivateKey, String>;

// 纯函数:仅解析 PKCS#1 明文 RSA PEM。非 PKCS#1 标签 -> Ok(None) 让上层回退。
fn try_parse_pkcs1_rsa(pem: &str) -> Result<Option<russh::keys::PrivateKey>, String>;
```

### Contracts
- `load_private_key_compat`:① 先 `load_secret_key`(OpenSSH/PKCS#8),**保存首次 Err**;
  ② 失败读文件试 `try_parse_pkcs1_rsa`;③ 仍不支持 → 用**步骤①保存的** Err 文本(不重复读盘)
  + passphrase 指引。**禁止**为取错误文本第二次调 `load_secret_key`。
- `try_parse_pkcs1_rsa` 严格三态:`Ok(Some(key))`=解析成功 / `Ok(None)`=非 PKCS#1 标签(交回上层)
  / `Err(msg)`=PKCS#1 但加密或损坏。任何输入都**不得 panic**(下标用 `find` 派生、解码错误走 `?`)。
- `authenticate` hash 选择:**仅** `key.algorithm().is_rsa()` 为真才 `await best_supported_rsa_hash`;
  非 RSA 传 `None`。`PrivateKeyWithHashAlg::new(key, None)` 对 RSA = SHA-1,对非 RSA = 忽略。

### Validation & Error Matrix
| 输入 | 结果 |
|------|------|
| OpenSSH / PKCS#8 明文 | `Ok(key)`(步骤①) |
| PKCS#1 明文 RSA(`BEGIN RSA PRIVATE KEY`) | `Ok(key)`(步骤② fallback) |
| PKCS#1 含 `Proc-Type:` + `ENCRYPTED` | `Err`,文本含 `passphrase` |
| PKCS#1 标签但 base64/DER 损坏 | `Err "invalid PKCS#1 RSA private key: ..."`,不 panic |
| PKCS#1 缺 `-----END-----` | `Err "...missing END marker"` |
| SEC1 EC / 其它未知格式 | `Ok(None)` → 上层回退 russh 原始错误 + passphrase 指引 |
| 文件不可读 | `Err "failed to read private key ..."` |

### Good / Base / Bad
- **Good**:OpenSSH ed25519 → 步骤① 成功,且**不**调 `best_supported_rsa_hash`(零额外延迟)。
- **Base**:Oracle 下发 PKCS#1 RSA → 步骤② 解析 → RSA 走 rsa-sha2-512 → 端到端连通。
- **Bad**:加密 PKCS#1 → `Err` 明确提示 passphrase 不支持(不暴露密钥字节、不吐底层晦涩错)。

### Tests Required(断言点,均不触网)
`pool.rs` 单测:
- `try_parse_pkcs1_rsa_parses_plaintext_pkcs1`:`Ok(Some)` 且 `key.algorithm().as_str().contains("rsa")`。
- `try_parse_pkcs1_rsa_returns_none_for_non_pkcs1_tag`:OpenSSH/PKCS#8 标签 → `matches!(_, Ok(None))`。
- `try_parse_pkcs1_rsa_rejects_encrypted_pkcs1_with_guidance`:`Err` 文本 `contains("passphrase")`。
- `try_parse_pkcs1_rsa_errors_on_corrupt_base64`:`is_err()`(验不 panic)。
- **端到端**(必做,单测覆盖不了"服务器接受签名"):连一台现代 OpenSSH 跑一次 `ssh_exec`。

### Wrong vs Correct
#### Wrong
```rust
let key = load_secret_key(path, None)?;                    // PKCS#1 直接 "Unsupported key type RSA"
let wh = PrivateKeyWithHashAlg::new(Arc::new(key), None);  // RSA -> SHA-1 -> 现代 OpenSSH 拒
// Cargo.toml
rsa = { version = "=0.10.0-rc.18", features = ["pem"] }    // rsa 无 pem feature -> resolve 失败
```
#### Correct
```rust
let key = load_private_key_compat(path)?;                  // PKCS#1 fallback
let rsa_hash = if key.algorithm().is_rsa() {               // 仅 RSA 查, 免非 RSA 白等 EXT_INFO
    match handle.best_supported_rsa_hash().await {
        Ok(Some(alg)) => alg,
        Ok(None) | Err(_) => Some(russh::keys::HashAlg::Sha512),
    }
} else { None };
let wh = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
// Cargo.toml: rsa = "=0.10.0-rc.18"  (默认 encoding feature 含 pkcs1 DER -> from_pkcs1_der)
```

---

## 真实出处

task `06-06-ssh-mcp-pkcs1-rsa-key`:用 Oracle Cloud 下发的 2048-bit PKCS#1 RSA key 连
`oracle-4c-24g`,先报 `Unsupported key type RSA`(坑1),加 fallback 后变
`server rejected all configured methods`(坑2),改 `best_supported_rsa_hash` 后端到端连通。
见 `src-tauri/mt-sidecars/src/pool.rs` 的 `authenticate` / `load_private_key_compat` /
`try_parse_pkcs1_rsa` 及其单测。
