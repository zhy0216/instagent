difficulty: medium

## T1 · 完整 message role/tool/image 校验

- 要做什么：在 `src/message.rs` 将校验扩展为 role/content 矩阵：拒绝错误角色中的 ToolUse/ToolResult、orphan/extra/duplicate ToolResult；要求 ToolUse id/name 非空且全局唯一，紧邻 user 的结果 id 集合与 assistant tool id 集合精确相等；校验 image MIME、base64、大小和 metadata 形状。
- 要做什么：确保 provider wire、session salvage/append 和其它消息入口统一调用同一校验函数，错误带消息索引、block 索引和约束；补 malformed-message 测试，不改变合法历史 JSONL 的兼容性。
- 预计修改文件：`src/message.rs`，以及为统一入口所需的最小 provider/session 调用点。
- 验收条件：上述每类 malformed message 都在边界被拒绝；合法 text/tool/image round-trip 保持通过；错误不包含未界定的整行原始 secret；消息模块和依赖它的测试通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test message` 和 `cargo test`。
- 前置依赖：`01-policy-decisions.md`。
