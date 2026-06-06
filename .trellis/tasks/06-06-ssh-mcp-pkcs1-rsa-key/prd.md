# mt-ssh-mcp 支持传统 PKCS#1 RSA 私钥

## 背景 / 现象

通过 mini-term 共享给 agent 的 SSH 连接 `oracle-4c-24g`（`ubuntu@64.181.226.117`，Oracle Cloud ARM 实例）调用 `ssh_exec` 失败：

```
failed to load private key 'C:\Users\...\oracle-4c4g-ssh-key-2026-05-08.key':
Unsupported key type RSA. If the key is encrypted with a passphrase, mt-ssh-mcp
does not support passphrase keys yet ...
```

- 用系统 OpenSSH 客户端拿**同一把 key** 可正常登录（`ssh-keygen -l` 显示 2048-bit RSA），证明服务器、网络、key 本身都没问题。
- key 是传统 **PKCS#1 明文 PEM**：`-----BEGIN RSA PRIVATE KEY-----`（Oracle Cloud / `ssh-keygen -m PEM` 常见产物）。

## 根因

`mt-ssh-mcp` sidecar 的会话池 `src-tauri/mt-sidecars/src/pool.rs` 用
`russh::keys::load_secret_key(path, None)` 加载私钥。其底层 `ssh-key 0.7.0-rc.10`
只解析两类明文私钥：

- OpenSSH：`-----BEGIN OPENSSH PRIVATE KEY-----`
- PKCS#8：`-----BEGIN PRIVATE KEY-----`

传统 **PKCS#1**（`BEGIN RSA PRIVATE KEY`）与 **SEC1 EC**（`BEGIN EC PRIVATE KEY`）
不在其解析路径内，直接抛 `Unsupported key type RSA`。

## 方案

在 `pool.rs` 加载私钥处包一层 `load_private_key_compat`：

1. 先走 `load_secret_key`（OpenSSH / PKCS#8，覆盖绝大多数现代密钥）。
2. 失败则读原文件，命中 `BEGIN RSA PRIVATE KEY` 时走纯 Rust fallback
   `try_parse_pkcs1_rsa`：自剥 PEM → base64 解码成 DER → `rsa::RsaPrivateKey::from_pkcs1_der`
   → `ssh_key::private::RsaKeypair::try_from(&rsa_key)` → `ssh_key::PrivateKey::from(keypair)`。
3. 加密的传统 PEM（`Proc-Type: 4,ENCRYPTED`）明确不支持，给可操作指引。
4. 其余未知格式回退到 russh 原始错误 + passphrase 指引。

### 依赖约束（已验证）

- 新增 `ssh-key = "=0.7.0-rc.10"`（features `rsa`,`alloc`）与 `rsa = "=0.10.0-rc.18"`。
- 两者版本须与 `russh 0.61` 内部锁定**完全一致**，否则 `PrivateKey` / `RsaPrivateKey`
  跨 crate-version 类型不可互换（见 spec `rand-core-version-alignment.md`）。
- `rsa` 无 `pem` feature（PEM 方法 gated 在 `pkcs1/pem`，rsa 未传递），故走
  `from_pkcs1_der` + 自剥 PEM（复用已有 `base64` crate），不额外引入 `pkcs1` 依赖。
- `cargo tree` 复核：仍 **无 `aws-lc-sys`**（不触发 Windows MSVC NASM 陷阱，见 spec
  `rust-crypto-on-windows-msvc.md`），`rand_core` 全树统一 `0.10.1`。

## 验收

- [ ] `cargo test --lib pool` 通过（含 PKCS#1 解析 / 非 PKCS#1 回退 / 加密拒绝 / 坏 base64）。
- [ ] 实际连接 `oracle-4c-24g` 的 `ssh_exec` 成功返回远端信息。
- [ ] 现代 OpenSSH / PKCS#8 密钥仍照常工作（fallback 不影响第一步）。
