#!/usr/bin/env bash
# 仓库级校验（与 .github/workflows/ci.yml 等价的本地复现，todos/19）。
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "==> cargo test"
cargo test

echo "==> cargo clippy --all-targets --features anthropic-engine -- -D warnings"
cargo clippy --all-targets --features anthropic-engine -- -D warnings

echo "==> cargo test --features anthropic-engine"
cargo test --features anthropic-engine

echo "==> cargo run -- --help"
cargo run -q -- --help >/dev/null

echo "==> all checks passed"
