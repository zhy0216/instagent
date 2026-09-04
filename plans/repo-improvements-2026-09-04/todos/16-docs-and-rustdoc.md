difficulty: easy

## T1 · 文档、计划状态与 rustdoc 收口

- 要做什么：将当前 README、`docs/usage.md`、`docs/architecture.md`、`src/tools/mod.rs` 等功能清单中的 built-in tools 数量同步为 6，并校正 stdout/stderr、api_key、settings.local 等描述；给历史 goose 设计文档加“历史/当前映射”标记；关闭或改写已被 ADR 0001 淘汰的 `plans/thinking-blocks/plan.md` 与已过时 approval 叙述。
- 要做什么：修复 `src/plugin/bundled.rs`、`src/provider/openai.rs`、`tests/fixtures/fake_proxy_server.rs` 的 intra-doc links，目标是 `cargo rustdoc --lib -- -D warnings` 通过；只改注释/链接，不改变运行时行为。
- 预计修改文件：`README.md`、`docs/usage.md`、`docs/architecture.md`、`src/tools/mod.rs`、`src/plugin/bundled.rs`、`src/provider/openai.rs`、`tests/fixtures/fake_proxy_server.rs`、`plans/thinking-blocks/plan.md`、`plans/parallel-tool-execution/plan.md` 及必要的历史文档标记。
- 验收条件：文档数字和 ADR 与实现一致；历史文档明确不是当前契约；`cargo rustdoc --lib -- -D warnings` 与仓库三条基线命令通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`、`cargo rustdoc --lib -- -D warnings` 和 `git diff --check`。
- 前置依赖：`08-config-provider-validation.md`、`12-cli-stream-and-e2e.md`、`13-parallel-tools.md`、`14-manifest-schema.md`。
