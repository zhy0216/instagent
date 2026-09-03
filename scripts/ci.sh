#!/usr/bin/env bash
# 仓库级校验（.github/workflows/ci.yml 全量 + 一步 `--help` smoke，todos/19）。
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "==> cargo test"
cargo test

echo "==> cargo run -- --help"
cargo run -q -- --help >/dev/null

echo "==> all checks passed"
