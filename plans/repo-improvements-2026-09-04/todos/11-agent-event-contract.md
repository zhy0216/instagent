difficulty: medium

## T1 · event 背压、残缺 stream 与 hook 错误可见性

- 要做什么：在 `src/agent/mod.rs` 明确 event channel 消费契约，允许调用者选择非阻塞/drop 或可配置 sink，记录丢弃计数；未消费 channel 时 turn 不应无限期卡住。`stream_assistant` 遇到 EOF、provider disconnect 或缺少 ToolUseEnd 时返回结构化错误/note，保留已收集片段但不静默丢失。
- 要做什么：在 `src/cli/handlers.rs` 处理 SessionStart/SessionEnd hook 错误，按策略输出 warning/note 或非零退出，同时保持默认兼容；补纯逻辑测试。
- 预计修改文件：`src/agent/mod.rs`、`src/cli/handlers.rs`。
- 验收条件：不消费 bounded channel 的调用有明确终止/丢弃行为；残缺 provider stream 在 API 和事件层可观察；hook 失败包含阶段/source 且测试断言稳定；消息/资源校验未回归。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、agent/handler 测试和 `cargo test`；单独运行不消费 event channel 的回归用例。
- 前置依赖：`06-process-tool-isolation.md`、`09-message-contract.md`。
