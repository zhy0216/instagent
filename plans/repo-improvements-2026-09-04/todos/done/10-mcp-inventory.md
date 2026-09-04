difficulty: medium

## T1 · MCP inventory 与多 server 健壮性

- 要做什么：在 `src/tools/mcp.rs` 为每个 server 的 connect/list inventory 增加 timeout、cancellation 和有界错误 body；一个 server 失败时保留其它健康 source，返回结构化 note，只有没有任何可用 source 时才按装配策略失败；非文本/不支持 transport 要明确可见，不静默当空工具列表。`src/plugin/mcp_config.rs` 的 mcp.json 读取、解析和 headers/command 字段错误也使用有界输入与来源诊断。
- 要做什么：在 `src/tools/mod.rs` 缓存工具清单与 route，按连接/配置变化失效，避免每轮重复枚举；在 `src/cli/assembly.rs` 传递可读的 source/server/path 诊断。覆盖 header 传递、部分失败、超时、重连和 cache invalidation。
- 预计修改文件：`src/tools/mcp.rs`、`src/tools/mod.rs`、`src/plugin/mcp_config.rs`、`src/cli/assembly.rs`。
- 验收条件：单 server 失败不丢弃其它 server；timeout/cancel 有上界且不变成成功/空列表；配置变化不会命中旧 inventory；错误 note 能指出 plugin/server；MCP 现有与新增测试通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、MCP unit/e2e 测试和 `cargo test`，并重复运行 timeout/cache 用例。
- 前置依赖：`01-policy-decisions.md`、`05-subprocess-collector.md`。
