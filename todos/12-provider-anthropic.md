# 12 · Provider：Anthropic 原生 engine（可选）

优先级：P2（可选） · 依赖：00、08、10

**可整段延后/跳过，不阻塞其他任务**；若主力模型是 Claude 则应做
（第三版 §7 风险 3：prompt caching / thinking / strict 只有原生 engine 或 proxy 才有）。

目标：原生 Messages API 实现，编译在 `anthropic-engine` feature 后。只填
`src/provider/anthropic.rs`；`10` 合并后把 `src/provider/registry.rs` 里 `anthropic`
占位分支接上（对 registry 的修改仅限该分支，且放在 `#[cfg(feature = "anthropic-engine")]` 后）。

验收：`cargo test --features anthropic-engine` 过；默认构建不含该模块。

计划参考：第二版 §2.3（anthropic.rs）；第三版 §2.4。

## M1 · anthropic.rs {#m1}

- `POST {base}/v1/messages`，`stream: true`，头 `x-api-key`、`anthropic-version: 2023-06-01`。
- SSE 事件链：`message_start`(usage.input) → `content_block_start`(text | tool_use) →
  `content_block_delta`(text_delta | input_json_delta) → `content_block_stop` →
  `message_delta`(stop_reason, usage.output) → `message_stop`。
- `cache_control: {type: ephemeral}` 加在 system 块和最后一个 tool spec 上。
- `input_json_delta` 全部拼完再 parse。
- 参考 `~/yyds/goose` `formats/anthropic.rs:230 format_messages、489 format_tools、871
  response_to_streaming_message`（只读参照，重写到约 450 行）。

## M2 · SSE fixture 对照测试 {#m2}

- 从 `~/yyds/goose` `formats/anthropic.rs` 测试段（约 1227 行起）抄样本与期望结果到
  `tests/fixtures/`；解析输出必须一致。

## M3 · feature 隔离 {#m3}

- 默认 `cargo build` / `cargo test` 不编译本模块；`--features anthropic-engine` 全绿。
