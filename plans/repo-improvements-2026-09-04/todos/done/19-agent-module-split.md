difficulty: hard

## T1 · 拆分 agent loop 与工具执行职责

- 要做什么：在 `11`、`13` 的行为测试护栏下，按职责整理 `src/agent/mod.rs` 中 loop、tool execution、stream folding、event sink、cancellation 等边界；优先复用现有 `src/agent/compact.rs`、`event.rs`、`prompt.rs` 或在 `mod.rs` 内建立私有实现单元，不修改 `src/lib.rs` 或既有模块声明。保持公开 API、session/message invariant、并行策略和事件契约不变，避免无收益的重命名。
- 预计修改文件：`src/agent/mod.rs`、必要时既有的 `src/agent/compact.rs`、`src/agent/event.rs`、`src/agent/prompt.rs` 及对应单测。
- 验收条件：所有已有和新增 agent/CLI/MCP 测试通过；模块边界按职责清晰，取消、并发、残缺 stream 和 event drop 行为有回归测试；`cargo fmt`、clippy、test、rustdoc 全通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`、`cargo rustdoc --lib -- -D warnings` 和 release check。
- 前置依赖：`11-agent-event-contract.md`、`13-parallel-tools.md`、`16-docs-and-rustdoc.md`。
