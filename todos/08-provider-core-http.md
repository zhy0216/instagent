# 08 · Provider：核心 trait 与 HTTP 层

优先级：P1 · 依赖：00

目标：实现 provider 公共类型与 HTTP/SSE/重试层。只填 `src/provider/mod.rs`（实现部分）、
`src/provider/http.rs`。

验收：`cargo test` 过；SSE 解析、重试退避、Retry-After、ContextOverflow 判定有测试（wiremock）。

计划参考：第二版 §2.3（Provider trait、http.rs）。

## I1 · provider/mod.rs {#i1}

- 填实 `Request<'a>`、`StreamEvent`、`StopReason`、`Provider` trait、`ProviderError`（类型在 00 已定义）。
- 上下文上限前缀小表 + 兜底 128k：`context_limit_for(model: &str) -> u32`
  （claude 200k、gpt-4o 128k、gpt-4.1 1M、o 系列 200k、deepseek 128k、llama 128k）。

## I2 · http.rs {#i2}

- reqwest client，超时 600s；hand-roll SSE 解析（约 80 行，不引 eventsource 库）：
  按空行切事件，取 `data:` 行，`[DONE]` 结束。
- 重试：429 / 500 / 502 / 503 / 529 指数退避，3 次、首次 1s、×2、上限 30s；
  `Retry-After` 头和 body 里的 `retry_after_seconds` 优先且封顶
  （参考 `~/yyds/goose/crates/goose-providers/src/http_status.rs:47~83`，搬运注明出处）。
- 400 且错误文本含 "prompt is too long" / "context_length_exceeded" /
  "maximum context length" → `ProviderError::ContextOverflow`（匹配集中在一个函数）。

## I3 · 测试 {#i3}

- SSE 解析：多事件、多行 data、`[DONE]`。
- wiremock：429 + Retry-After 重试成功；500 重试耗尽；400 溢出文案 → ContextOverflow。
