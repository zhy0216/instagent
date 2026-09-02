difficulty: easy

CI 配套补强：加依赖漏洞扫描、补 macOS 跨平台、对齐 ci.sh 的 smoke 步骤。

## T1 · CI 加 cargo-audit（不阻断）

- `.github/workflows/ci.yml` 加依赖漏洞扫描步骤。用 `actions-rust-lang/audit` 或官方推荐 action 跑 `cargo audit`。
- **必须设为不阻断**（`continue-on-error: true`）：修复 RUSTSEC 通告需要升级依赖，违反本仓库"不加依赖、不升级依赖"锁定（AGENTS.md）。此步只暴露问题供维护者决策，不卡 CI。
- 预计修改文件：`.github/workflows/ci.yml`。
- 验收：workflow YAML 语法合法（可用 `actionlint` 或 YAML 解析校验）；注释说明为何不阻断。
- 前置依赖：无。

## T2 · CI 加 macOS matrix

- `ci.yml` 目前只跑 `ubuntu-latest`。加 `macos-latest` 到 `runs-on` matrix，让两条校验链（含 `anthropic-engine` feature 两步）在 macOS 也跑。测试用进程组、符号链接等 unix 语义，预期通过。
- 预计修改文件：`.github/workflows/ci.yml`。
- 验收：workflow YAML 合法；matrix 正确展开两个 OS。
- 前置依赖：无。

## T3 · CI 补 --help smoke，与 ci.sh 对齐

- `scripts/ci.sh` 比 `ci.yml` 多一步 `cargo run -q -- --help` smoke。把这步加进 `ci.yml`，使两者一致（README 称 ci.sh 为"CI 全量"才成立）。
- 预计修改文件：`.github/workflows/ci.yml`。
- 验收：workflow YAML 合法；`ci.yml` 与 `ci.sh` 步骤一一对应。
- 前置依赖：依赖 08 文件内 T1、T2（同文件，串行改）。

## 本文件整体验证

本地无法完整跑 GitHub Actions，验证以：(1) YAML/actionlint 语法校验；(2) `bash scripts/ci.sh` 本地全量通过；(3) 推送后观察 CI 两个 OS 均绿为准。

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```
