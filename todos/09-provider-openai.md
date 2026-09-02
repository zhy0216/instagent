# 09 · Provider：OpenAI engine

优先级：P1 · 依赖：00、08

目标：实现 OpenAI Chat Completions 流式 engine（同时服务 openai / ollama / groq /
deepseek / openrouter，靠 base_url + key 区分）。只填 `src/provider/openai.rs`。

验收：`cargo test` 过；消息格式化四个怪癖、SSE fixture 对照、wiremock 流式有测试。

计划参考：第二版 §2.3（openai.rs）、§6 风险 1；第三版 §2.4。

## J1 · openai.rs {#j1}

- `POST {base_url}/chat/completions`，`stream: true`，`stream_options: {include_usage: true}`，
  `Authorization: Bearer <api_key_env 环境变量>`，`base_url` 写到 `/v1`。
- 消息 → 请求体；`delta.content` 累积文本；`delta.tool_calls[{index,id,function{name,arguments}}]`
  按 `index` 累积 arguments；`finish_reason` → StopReason；`Done` 带 usage。
- 四个已知怪癖（参考 `~/yyds/goose` `crates/goose-provider-types/src/formats/openai.rs`，
  行号见第二版 §2.3）：
  1. 带 tool_calls 的 assistant 消息 `content` 必须是 null 或 ""，不能省略字段；
  2. function name 只允许 `[A-Za-z0-9_-]{1,64}`，非法字符/超长做 sanitize；
  3. tool 消息必须紧跟含对应 tool_calls 的 assistant 消息；
  4. arguments 为空串时当 `{}`。
- 构造函数入参 = provider JSON 配置（name / base_url / api_key_env / headers /
  timeout_seconds / models），供 `10` registry 调用。

## J2 · SSE fixture 对照测试 {#j2}

- 从 `~/yyds/goose` `formats/openai.rs` 测试段（约 1933 行起）抄 SSE 样本与期望结果
  到 `tests/fixtures/`，重点三种样本：多并行 tool_calls 按 index 各自累积、
  arguments 空串、文本与 tool 混合。解析输出必须与期望一致。

## J3 · wiremock 集成测试 {#j3}

- 流式响应组装成完整 assistant Message；Authorization 头；错误状态码映射。
