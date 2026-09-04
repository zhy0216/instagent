difficulty: hard

## T1 · 安全条件下并行执行独立只读工具

- 要做什么：在 `src/agent/mod.rs` 与 `src/tools/mod.rs` 增加工具 capability/metadata（至少只读、资源冲突/顺序敏感标记），将同一 assistant turn 中可证明独立的只读调用以 bounded concurrency 执行；写操作、同资源冲突和依赖调用保持顺序。共享总预算、cancellation、event 顺序和 session append 不变量。
- 要做什么：在 agent/provider 请求边界增加图片单请求与单会话总字节预算；同一会话中对重复图片去重或拒绝超限，并给出可操作提示。只实现预算与生命周期淘汰所需的最小行为，不引入 RM2 的 blob/reference 存储或新 decoder 依赖。
- 要做什么：同步更新 `plans/parallel-tool-execution/plan.md`，删除 ADR 0002 之前的 approval/trust 假设，说明 sandbox 边界与并行安全前提；补并发、顺序、取消、超时、失败隔离和事件顺序测试。
- 预计修改文件：`src/agent/mod.rs`、`src/tools/mod.rs`、`src/provider/openai.rs`、`plans/parallel-tool-execution/plan.md`。
- 验收条件：两个独立只读 fake tool 可并行且受并发上限约束；冲突/写操作仍串行；取消会回收所有任务并维持 message invariant；事件和 tool result 与调用 id 一一对应；历史计划不再要求交互审批。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、agent/tools 并发测试、`cargo test` 和 `cargo rustdoc --lib -- -D warnings`。
- 前置依赖：`04-file-tool-budgets.md`、`05-subprocess-collector.md`、`06-process-tool-isolation.md`、`08-config-provider-validation.md`、`09-message-contract.md`、`10-mcp-inventory.md`、`11-agent-event-contract.md`。
