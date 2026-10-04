#!/usr/bin/env bash
set -euo pipefail

# Idempotent Cloud Agent bootstrap: Rust 1.85+ toolchain, deps, release binary.
rustup toolchain install stable --component rustfmt,clippy
rustup default stable
cargo fetch --locked
cargo build --release --locked
sudo install -m 0755 target/release/wtrm /usr/local/bin/wtrm
wtrm --version
