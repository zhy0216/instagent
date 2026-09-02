difficulty: easy

零散小修：死代码、过期文案、文档与安装命令修正。全部是文本/删除级改动，无行为变化。

## T1 · 删除死函数 `materialize_dir()`

- `src/plugin/bundled.rs:35-37` 的 `pub fn materialize_dir()` 无任何生产/测试调用方（生产走 `load_bundled → materialize_at`，测试走 `*_at` 变体）。删除该函数，并把 `materialize_at` 上方引用它的 doc 注释（约 39 行）改为自描述。
- 预计修改文件：`src/plugin/bundled.rs`。
- 验收：`cargo clippy --all-targets -- -D warnings` 与 `cargo test` 通过；全仓库 `rg materialize_dir` 无残留。
- 前置依赖：无。

## T2 · 修正过期文案

- `bundled/plugin.json` 的 `description` 仍写 "placeholders until TODO(10) generates the full set"，TODO(10) 已完成。改为正式描述（如 "Instagent bundled plugin: built-in provider definitions"）。
- `bundled/dev.instagent/providers/openrouter.json` 的 `http-referer` 头指向占位地址 `https://github.com/instagent`。仓库无此地址时删除该 header（保留 `x-title`）。
- `docs/goose-plugin-core-plan.md:174` proxy 描述写环境变量 `MINI_AGENT_PORT`，代码实际是 `INSTAGENT_PORT`（`src/provider/proxy.rs:241`）。计划文档有意沿用旧名（有对照表），故不整体改名，只在此处加括注"（本仓库实际为 `INSTAGENT_PORT`）"。
- 预计修改文件：`bundled/plugin.json`、`bundled/dev.instagent/providers/openrouter.json`、`docs/goose-plugin-core-plan.md`。
- 验收：`cargo test` 通过（注意 `src/plugin/bundled.rs` 测试若断言 description 需同步）；`rg "TODO\(10\)" bundled/` 无残留。
- 前置依赖：无。

## T3 · README 与 scripts 对齐

- `README.md:18` `cargo install --path .` 会把 `mcp-fixture-server`、`fake-proxy-server` 两个测试 fixture 二进制一并装进 `~/.cargo/bin`。改为 `cargo install --path . --bin instagent`，并本地验证该命令形式可用（`cargo install --path . --bin instagent --root $(mktemp -d)`，确认只装一个二进制）。
- `scripts/ci.sh` 比 `.github/workflows/ci.yml` 多一步 `cargo run -q -- --help` smoke，但文件头自称与 ci.yml 等价、README 称其"= CI 全量"。修正 `scripts/ci.sh` 头部注释为"ci.yml 全量 + 一步 --help smoke"。
- 预计修改文件：`README.md`、`scripts/ci.sh`（仅注释）。
- 验收：`bash scripts/ci.sh` 通过；README 中安装命令更新。
- 前置依赖：无。

## T4 · README 补充两条语义说明

- 在 README「配置与环境变量」或「快速上手」节补两条明示（对应安全发现 S2、健壮性 R7）：
  1. `read`/`tree` 在默认审批白名单（`DEFAULT_ALWAYS_ALLOW`），approve 模式下可无确认读取当前用户可读的任意路径（含绝对路径与 `..`），这是"用户环境代理"的设计语义；介意的用户可通过 `always_allow` 配置调整。
  2. 会话文件假设单进程独占：不要对同一会话 id 同时开两个 `chat --resume`。
- 预计修改文件：`README.md`。
- 验收：文案准确（与 `src/agent/approval.rs:18`、`src/session.rs` 行为一致）。
- 前置依赖：无。

## 本文件整体验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```
