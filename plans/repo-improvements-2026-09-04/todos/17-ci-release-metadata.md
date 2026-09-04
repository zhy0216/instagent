difficulty: medium

## T1 · 发布元数据、审计和 CI 门槛

- 要做什么：依据实际发布目标补齐 `Cargo.toml` 的 license/repository/homepage/documentation/rust-version/keywords/categories，记录支持的 toolchain/MSRV；在 `rust-toolchain.toml` 与 CI 中采用经过验证的固定策略，避免无记录的 moving stable。
- 要做什么：更新 `.github/workflows/ci.yml` 与 `scripts/ci.sh`：明确 cargo-audit/deny 的安全政策和 advisory 是否阻断，增加 scheduled dependency scan、rustdoc-as-error、release smoke；不能阻断的检查必须记录原因和 owner。若依赖政策允许，再评估收窄 tokio features；未获授权不得改依赖版本。
- 要做什么：针对历史 provider proxy readiness flake 做并发/重复运行采样，记录是否可复现；只有测试证明存在确定性问题时才调整 readiness handshake、重试或固定端口，避免把历史报告直接当作 bug。
- 预计修改文件：`Cargo.toml`、`rust-toolchain.toml`、`.github/workflows/ci.yml`、`scripts/ci.sh`、`tests/provider_proxy.rs`（仅采样/确定性修复需要时）、必要的发布说明文档。
- 验收条件：本地与 CI 使用同一可复现 toolchain；audit/doc/release 检查可重复运行；安全政策与 ADR/README 一致；三条基线命令、rustdoc 和 release check 通过。
- 验证方式：本地运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`、`cargo rustdoc --lib -- -D warnings`、`cargo check --release --all-targets` 和 `bash scripts/ci.sh`。
- 前置依赖：`01-policy-decisions.md`、`16-docs-and-rustdoc.md`。
