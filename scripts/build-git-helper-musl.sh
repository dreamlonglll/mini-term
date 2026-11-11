#!/usr/bin/env bash
# Native Linux only: install musl-tools, build-essential, binutils, perl and
# pkg-config, then rustup target add <the matching musl target>. Only the helper
# and its mt-project dependencies are built; vendored OpenSSL stays enabled.
set -euo pipefail

target="${1:?Usage: bash scripts/build-git-helper-musl.sh <x86_64-unknown-linux-musl|aarch64-unknown-linux-musl>}"
case "$(uname -s)/$(uname -m)/$target" in
  Linux/x86_64/x86_64-unknown-linux-musl|Linux/aarch64/aarch64-unknown-linux-musl) ;;
  *) echo "The musl helper must be built on the matching native Linux architecture: $target" >&2; exit 1 ;;
esac
for tool in musl-gcc ar make perl pkg-config; do
  command -v "$tool" >/dev/null || { echo "Missing build prerequisite: $tool" >&2; exit 1; }
done

# Explicit target-specific tools also cover OpenSSL/libgit2's C build scripts.
# Native GNU ar handles the matching architecture's musl object files.
key="${target//-/_}"
upper="$(printf '%s' "$key" | tr '[:lower:]' '[:upper:]')"
export "CC_${key}=musl-gcc" "AR_${key}=ar"
export "CARGO_TARGET_${upper}_LINKER=musl-gcc"
export "CARGO_TARGET_${upper}_RUSTFLAGS=-C target-feature=+crt-static"
export OPENSSL_STATIC=1 OPENSSL_NO_VENDOR=0
cargo build --locked --release --target "$target" --package mt-git-helper
printf 'Helper: target/%s/release/mt-git-helper\n' "$target"
