difficulty: hard

## T1 · 拆分 hooks 与 OpenAI transport/parser

- 要做什么：在安全、资源和 CLI 契约稳定后，将 `src/hooks.rs` 按 manifest loading、env/policy、command execution、decision parser 整理为私有实现单元；将 `src/provider/openai.rs` 按 request mapping、SSE delta parser、stream state、error/usage handling 整理，并复用现有 shared stream 层。不得改变 ADR 0002 的自动执行边界或已定 hook failure 行为，也不修改 `src/lib.rs` 或既有模块声明。
- 预计修改文件：`src/hooks.rs`、`src/provider/openai.rs`、必要的 `src/provider/shared.rs` 调整及对应现有单测。
- 验收条件：hooks/provider 单测、fixture、CLI/MCP/live 可选测试均保持契约；rustdoc 无新增 warning；chunk UTF-8、bounded output、残缺 SSE、usage 和错误摘要测试继续通过；拆分后的公开类型和模块树兼容调用方。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、hooks/provider/CLI 测试、`cargo rustdoc --lib -- -D warnings` 和 `cargo test`。
- 前置依赖：`06-process-tool-isolation.md`、`08-config-provider-validation.md`、`11-agent-event-contract.md`、`13-parallel-tools.md`、`16-docs-and-rustdoc.md`。
