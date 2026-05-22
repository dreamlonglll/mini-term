# `rand_core` 多版本对齐陷阱

> 给加密 / SSH 类 crate 写测试 fixture 时，常踩到 `rand_core` 多版本共存导致 `CryptoRng` trait 路径不一致的坑。**生成密钥的 API 拒绝你传进去的 `OsRng`**，编译错误看起来很奇怪。

---

## What

加密类 crate（`ssh-key`、`russh`、`rustls` 等）的 `…::random(rng)` API 通常约束 `R: rand_core::CryptoRng`。而 `rand_core` 自身**有 0.6 / 0.9 / 0.10 等多个版本同时在生态里流通**：

- `ssh-key 0.7` 内部用 `rand_core = "0.10"`，其 `CryptoRng` trait 在 `rand_core@0.10::CryptoRng` 路径下。
- `rand = "0.8"` 拉的是 `rand_core = "0.6"`，其 `CryptoRng` 在 `rand_core@0.6::CryptoRng` 路径下。

**这两个 trait 在编译器眼里是不同 trait**（不同 crate-version 的同名 trait 不可互换）。把 `rand@0.8` 的 `OsRng` 喂给 `ssh-key 0.7` 的 `PrivateKey::random`，编译会失败：

```
the trait bound 'OsRng: russh::keys::signature::rand_core::CryptoRng' is not satisfied
```

**规约**：**测试 fixture 优先用底层类型直接构造**，**别走 rng-based API**。

```rust
// 反例：依赖 rng,撞 rand_core 版本不一致就编译挂
let key = ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, Algorithm::Ed25519)?;

// 正例：直接拿字节构造,既绕开 rand 版本对齐又让测试确定可重放
use russh::keys::ssh_key::public::{Ed25519PublicKey, KeyData, PublicKey};
const KEY_BYTES: [u8; 32] = [0x11; 32];
let pub_key = PublicKey::new(KeyData::Ed25519(Ed25519PublicKey(KEY_BYTES)), "test");
```

---

## Why

- **测试 fixture 不需要密码学随机**：你只想要一个能 round-trip wire 格式的密钥对象，不是真的要保证 256-bit 熵。`ssh-key` 不验证 Ed25519 公钥的密码学合法性，**任意 32 字节都能解析成功并参与 base64 编解码**——这正是 fixture 想要的：确定、可重放、零随机源依赖。
- **rand 生态版本节奏与下游 crate 不同步**：`rand 0.8` 流行多年；`ssh-key 0.7` 切到 `rand_core 0.10` 是 2024 年的事。dev-dep 写 `rand = "0.8"` 就会拉错 `rand_core`。
- **强行对齐 dev-dep 版本不可持续**：每次升 `ssh-key` / `russh`，可能又得改 `rand` 版本号；让测试代码绑死在加密库的实现版本上，将来升级阻力大。
- **直接构造更接近被测代码意图**：本仓 `pool.rs` 的测试要验证 `match_known_host` 对各种 known_hosts 行为，**真正关心的就是 wire 格式比对**，不需要密钥真“能签名”。用底层类型构造正好对得上。

---

## How to apply

1. **看 API 签名**：要构造的对象有没有「公开字段 + 构造函数」路径？多数加密库为序列化 / 解析提供过；优先用它。
2. **避开 `…::random` / `…::generate`**：这些路径一定带 `CryptoRng` 约束 → 一定要选定一个 `rand_core` 版本 → 自找麻烦。
3. **若必须用 rng**（比如某 API 强约束 `&mut impl CryptoRng + RngCore`、没有别的入口）：
   - `cargo tree -i rand_core` 看主依赖拉的是哪个版本。
   - dev-dep 对齐：例如 `ssh-key 0.7` 时代用 `rand = "0.10"`（而非 0.8），把 dev-dep 的 `rand_core` 拉到与主 crate 同版本。
   - 评论里注明「本测试 rand 版本与 X 库的 rand_core 绑死」，下次升 X 库别忘了同步。
4. **测试常量化字节**：`const KEY_BYTES: [u8; 32] = [0x11; 32]` 这种风格让 fixture 完全确定、CI 上零波动，也避免 PR diff 里出现「我换了一次 rng 又通了」之类的无意义抖动。

---

## 识别症状

编译报错形如：

```
error[E0277]: the trait bound 'OsRng: <crate>::<...>::rand_core::CryptoRng' is not satisfied
   |
   |     PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
   |     ^^^^^^^^^^^^^^^^^^ the trait '<crate>::<...>::rand_core::CryptoRng' is not implemented for 'OsRng'
   |
   = help: trait `rand_core::CryptoRng` is implemented for `OsRng`
   = note: perhaps two different versions of crate `rand_core` are being used?
```

**关键短语**：「trait `X` is implemented for `OsRng`」+ 「perhaps two different versions of crate `rand_core`」。看到这俩同时出现，就是 rand_core 多版本撞车，**立刻改成不依赖 rng 的 fixture 构造方式**。

---

## 真实出处

本次 `refactor-ssh-mcp-persistent-session-pool` 任务 PR1 写 `pool.rs` 测试时，初版用了：

```rust
let key = ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, Algorithm::Ed25519)?;
```

`Cargo.toml` 加了 `rand = "0.8"` 作为 dev-dep，编译报上面那段错。改成用 `Ed25519PublicKey([0x11; 32])` 直接构造 `PublicKey`，dev-dep 整个 `rand` 删掉；测试反而更短、更确定。见当前 `src-tauri/mt-sidecars/src/pool.rs` 测试模块的 `test_pubkey_from_bytes` 与 `KEY_BYTES_A` / `KEY_BYTES_B`。
