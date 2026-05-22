# Rust 加密 crate 在 Windows MSVC 上的 NASM 陷阱

> 拉 `russh` / `reqwest` / `rustls` 一类「带 TLS / SSH 加密」的 crate 时，**默认走 `aws-lc-rs` 后端**会让 `aws-lc-sys` 的 build.rs 在 Windows MSVC 平台上要求 `NASM`（汇编器）。本仓库 CI（GitHub Actions release.yml）与本地 Windows 开发机都**没装 NASM**，会在 `cargo build` 第一秒 panic。

---

## What

凡是新增的加密相关 crate（SSH / TLS / 哈希 / KDF），先检查它的 features：

- 默认 features 是否包含 `aws-lc-rs`（或 `aws-lc-sys`、`default-aws-lc-rs` 等）。
- 是否提供 `ring` feature 作为替代后端。

**有 `ring` 选项就优先用 `ring`**，并 `default-features = false` 关掉默认拉的 `aws-lc-rs`：

```toml
# 反例（默认 features 把 aws-lc-rs 拉进来 → Windows MSVC 上需要 NASM）
russh = "0.61"

# 正例（用 ring 后端 → pure Rust，零工具链依赖）
russh = { version = "0.61", default-features = false, features = ["ring"] }
```

写完后验证一次：在裸 Windows 11（**不装 NASM**）下 `cd src-tauri/mt-sidecars && cargo build`，应一路过。

---

## Why

- `aws-lc-sys` 的 build.rs 在 `x86_64-pc-windows-msvc`（与多数 Windows 三元组）上，对热路径（jitterentropy 熵收集、ChaCha20-Poly1305 等）用 NASM 写了优化版汇编。**找不到 `nasm.exe` 直接 `panic!("NASM is required ...")`**，整个 cargo build 立刻失败。
- 本仓本机与 CI 镜像都没装 NASM，加这工具链意味着：每个新开发者都要装、CI runner 要装、未来切 GitHub-hosted Windows runner 类型还要再确认 —— 长期成本明显。
- `ring` 后端是 pure Rust（包含的少量汇编通过 cc 编进来，**不依赖 NASM**），三平台都不需要额外工具链；对绝大多数 SSH / TLS 用例性能差异可忽略。
- 反向情形（必须 aws-lc-rs，比如 FIPS、特定 ciphersuite、第三方强约束）极少见；真遇到时，**CI 与本地构建文档必须显式写出「安装 NASM」步骤**，否则下个人重装环境就坑下一遍。

---

## How to apply

1. **新增加密依赖前 grep**：去 crates.io 看目标 crate 的 `Cargo.toml`，确认其 features 列表。
2. **关默认 features**：`default-features = false`。
3. **挑 ring**：`features = ["ring", ...其它仍需要的]`。
4. **本地验证**：`cargo clean -p <crate>` 后 `cargo build -p mt-sidecars`，确认不再触发 aws-lc-sys 的 NASM 检查。
5. **CI 验证**：相信 GitHub Actions release.yml 现有 Windows job 跑过；它没装 NASM，能过就证明没漏掉哪个 transitive 依赖偷偷又拉了 aws-lc-rs。

---

## 如何识别有这个坑的依赖

可以在 lockfile 或依赖图里搜：

```bash
# 拉完依赖后看 Cargo.lock 里有没有 aws-lc-sys
grep -nE '^name = "aws-lc-(sys|rs)"' src-tauri/Cargo.lock
grep -nE '^name = "aws-lc-(sys|rs)"' src-tauri/mt-sidecars/Cargo.lock

# 或用 cargo tree 看是谁把它拉进来的
cargo tree -p mt-sidecars -i aws-lc-sys
```

只要 lockfile 里出现 `aws-lc-sys` / `aws-lc-rs`，且 build profile 是 Windows MSVC，CI 不装 NASM 就会爆。

也可以 grep 当前所有 Cargo.toml 看自己有没有显式声明 aws-lc-rs feature（一般不该有）：

```bash
grep -nrE 'aws-lc-(sys|rs)' src-tauri/**/Cargo.toml
```

---

## 真实出处

本次 `refactor-ssh-mcp-persistent-session-pool` 任务 PR1 第一次写 `russh = "0.61"` 时没显式选后端，默认拉 `aws-lc-rs`，本机 Windows 11 `cargo build` 立刻在 `aws-lc-sys` 的 build.rs `panic!("NASM ...")` 上挂。研究后切到 `default-features = false, features = ["ring"]`，零工具链改动通过；见当前 `src-tauri/mt-sidecars/Cargo.toml` 的 `russh` 那一行 + 顶上注释。
