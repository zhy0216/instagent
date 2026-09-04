difficulty: hard

## T1 · 稳定 CLI stdout/stderr 与退出行为

- 要做什么：依据 ADR 0003 重构 `src/cli/render.rs`、必要的 handlers/repl 输出：定义 stdout 仅最终答案或 JSON、诊断/工具事件统一 stderr（或 ADR 选择的相反契约），统一 usage、notes、errors、session id 的位置和格式；修正文档对应描述。
- 要做什么：扩展 `tests/cli_e2e.rs` 和可复用纯逻辑测试，覆盖 pipe/redirect、坏参数退出码、resume 跨命令、provider/MCP 失败、`/clear`、`/compact`、Ctrl-C/子进程组回收。PTY 仅做少量 smoke，不依赖 live API；保留现有 fake provider 和 live tests 的可选性质。
- 预计修改文件：`src/cli/render.rs`、`src/cli/handlers.rs`（如需）、`tests/cli_e2e.rs`、`docs/usage.md`。
- 验收条件：机器 consumer 可稳定解析 stdout/stderr；退出码和取消后 session/child 状态有固定断言；PTY/pipe 测试在 CI 不依赖交互或 live key；全仓校验通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --test cli_e2e`、`cargo test` 和必要的 `bash scripts/ci.sh`。
- 前置依赖：`06-process-tool-isolation.md`、`07-plugin-install-resilience.md`、`10-mcp-inventory.md`、`11-agent-event-contract.md`。
