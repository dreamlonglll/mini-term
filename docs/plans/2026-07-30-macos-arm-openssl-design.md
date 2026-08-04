# macOS ARM OpenSSL Linking Design

## Problem

The desktop build targets `aarch64-apple-darwin`, but `openssl-sys` auto-detects the Intel Homebrew OpenSSL installation under `/usr/local/opt/openssl@1.1`. The linker ignores those x86_64 libraries and fails with an undefined `_OPENSSL_init_ssl` symbol.

## Decision

Enable the existing `git2` crate's `vendored-openssl` feature. Cargo feature unification then enables `openssl-sys/vendored` for both `libgit2-sys` and `libssh2-sys`, so OpenSSL is compiled for the selected Rust target instead of discovered from a machine-specific Homebrew prefix.

This preserves the existing Git HTTPS and SSH features. It increases the first Rust build time, but removes the host OpenSSL architecture dependency from local development and CI.

## Verification

Add a regression test that asks Cargo for the resolved `aarch64-apple-darwin` feature graph and requires `openssl-sys feature "vendored"`. Then run the Node tests, Rust tests, frontend build, and `npm run tauri dev` through successful application launch.
