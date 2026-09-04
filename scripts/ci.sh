#!/usr/bin/env bash
# 仓库级校验（与 .github/workflows/ci.yml 的 check job 全量对齐；
# 政策与豁免记录见 docs/release.md）。
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "==> cargo test"
cargo test

echo "==> cargo rustdoc --lib -- -D warnings"
cargo rustdoc --lib -- -D warnings

echo "==> release smoke: cargo check --release --all-targets"
cargo check --release --all-targets

echo "==> cargo run -- --help"
cargo run -q -- --help >/dev/null

# cargo-audit：政策上不阻断（AGENTS.md 锁依赖、公告修复需升级依赖，
# owner 见 docs/release.md）。本机装了就直接跑；未安装只提示，
# 定期公告扫描由 CI 的 schedule 任务覆盖。
if command -v cargo-audit >/dev/null 2>&1; then
  echo "==> cargo audit (informational, non-blocking)"
  cargo audit || echo "warning: cargo audit 报告了公告；按政策不阻断，见 docs/release.md"
else
  echo "==> cargo audit skipped（未安装 cargo-audit；CI schedule 扫描仍覆盖）"
fi

echo "==> all checks passed"
