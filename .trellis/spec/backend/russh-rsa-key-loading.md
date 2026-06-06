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

**修法**:按服务器通告的 `server-sig-algs`(EXT_INFO)选 hash,且因 `new()` 对非 RSA key
自动忽略 hash,可**无条件**传:

```rust
let rsa_hash = match handle.best_supported_rsa_hash().await {
    Ok(Some(alg)) => alg,                                   // 通告: Sha512/Sha256, 或 None(仅 ssh-rsa)
    Ok(None) | Err(_) => Some(russh::keys::HashAlg::Sha512), // 未发 EXT_INFO: 回退 sha2-512
};
let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
```

`best_supported_rsa_hash` 会等最多 1s EXT_INFO;返回 `Ok(None)` 表示服务器没通告,文档
明确「此时仍可试 rsa-sha2-*」——所以回退 `Sha512` 比留 `None`(SHA-1)安全得多。

---

## How to apply / 自检

新增或调试「russh 用私钥认证」的代码时:

1. **格式**:别假设 `load_secret_key` 吃所有 PEM;PKCS#1/SEC1 要走 fallback。
2. **hash**:RSA key 永远先 `best_supported_rsa_hash`,**绝不**给 `PrivateKeyWithHashAlg::new`
   传 `None`(=SHA-1)。ed25519/ecdsa 不受影响(hash 被忽略)。
3. **端到端验证别只靠单测**:单测只能验「解析成功」,验不了「服务器接受签名」。两个坑
   分别卡在解析期与认证期,必须真连一台现代 OpenSSH 服务器跑一次 `ssh_exec`(或临时
   `examples/` 探针,用完即删)才算闭环。

---

## 真实出处

task `06-06-ssh-mcp-pkcs1-rsa-key`:用 Oracle Cloud 下发的 2048-bit PKCS#1 RSA key 连
`oracle-4c-24g`,先报 `Unsupported key type RSA`(坑1),加 fallback 后变
`server rejected all configured methods`(坑2),改 `best_supported_rsa_hash` 后端到端连通。
见 `src-tauri/mt-sidecars/src/pool.rs` 的 `authenticate` / `load_private_key_compat` /
`try_parse_pkcs1_rsa` 及其单测。
