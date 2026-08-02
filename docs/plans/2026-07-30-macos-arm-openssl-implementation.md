# macOS ARM OpenSSL Linking Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make native ARM macOS builds independent of an Intel Homebrew OpenSSL installation.

**Architecture:** Keep the current `git2` HTTPS and SSH behavior, but enable its supported `vendored-openssl` feature. Verify the resolved Cargo feature graph for the affected target before validating the complete application startup.

**Tech Stack:** Rust, Cargo features, git2-rs 0.19, Node.js test runner, Tauri 2

---

### Task 1: Add the dependency-graph regression test

**Files:**
- Create: `tests/rustCryptoFeatures.test.cjs`

**Step 1:** Run `cargo tree --manifest-path src-tauri/Cargo.toml --target aarch64-apple-darwin -e features -i openssl-sys` and assert that the output contains `openssl-sys feature "vendored"`.

**Step 2:** Run `node --test tests/rustCryptoFeatures.test.cjs` and verify it fails because only the default OpenSSL feature is enabled.

### Task 2: Vendor OpenSSL through git2

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/Cargo.lock`

**Step 1:** Change the `git2` dependency to enable `vendored-openssl` without changing its version or other features.

**Step 2:** Refresh the lockfile through Cargo.

**Step 3:** Re-run `node --test tests/rustCryptoFeatures.test.cjs` and verify it passes.

### Task 3: Verify the complete build

**Files:**
- No source changes expected.

**Step 1:** Run the relevant Node and Rust tests.

**Step 2:** Run `npm run build`.

**Step 3:** Run `npm run tauri dev`, verify the application reaches its running state without the OpenSSL linker error, then stop the development process cleanly.
