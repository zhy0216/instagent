# 16 · Agent：loop、审批、系统提示、压缩

优先级：P3 · 依赖：00、02、08、13、01

目标：实现 agent loop 及其配套（审批、系统提示、压缩）。只填 `src/agent/**`。

验收：`cargo test` 过；mock provider 全流程、审批、取消后会话不变量、压缩触发有测试。

计划参考：第二版 §2.5（loop）、§2.7（压缩）、§2.8（审批）、§2.9（系统提示）。

## Q1 · 事件与系统提示 {#q1}

- `agent/event.rs`：`Event`（TextDelta / ToolStart / ToolDone / Usage / Compacted / Error），
  单向 mpsc 给 UI。
- `agent/prompt.rs`：`format!` 拼四段——身份一句话、工具说明（预留 MCP server
  instructions 与 skill name+description 的注入位，由 `18` 装配时传入）、`cwd` 和当前时间、
  响应规范（markdown、简洁）。文本从 `~/yyds/goose` `crates/goose/src/prompts/system.md`
  精简（注明出处）。约 60 行，不用模板引擎。

## Q2 · run_turn 主循环 {#q2}

- `Agent::run_turn(session, text, cancel, events)` 按第二版 §2.5 伪代码：
  append user 消息 → 循环至 `max_turns`（默认 1000，可配）：
  `compact::maybe` → stream_assistant → append assistant → 无 tool call 则 Done；
  否则逐个审批 + 串行执行工具（发 ToolStart/ToolDone 事件），结果合成一条 user 消息。
- `stream_assistant`：把 `StreamEvent` 折叠成一条 assistant Message 并转发 TextDelta；
  `ToolUseDelta` 累积到 `ToolUseEnd` 再 parse，失败生成 is_error ToolResult 告诉模型 JSON 坏了。
- `ContextOverflow` → `compact::force` 后重试一次。
- chat 模式：请求不带工具。

## Q3 · approval.rs {#q3}

- 三种模式：`auto` 全放行；`approve` 白名单放行、其余问用户（`read` / `tree` 默认白名单）；
  `chat` 不给模型工具。
- `Confirm` 回调 trait（异步，需要答案）；`Decision` Allow / AllowAlways / Deny(reason)。
- 白名单持久化在配置 `always_allow`（`01`），会话内 AllowAlways 即时写回。
- 拒绝 → is_error ToolResult `"user denied: <reason>"`。

## Q4 · compact.rs {#q4}

- 触发：每条 assistant 消息后 `usage.input >= threshold × context_limit`（threshold 默认 0.8）；
  或 `/compact`（CLI 在 `18` 接）；或 ContextOverflow。用上一次响应的 `usage.input`，不引 tokenizer。
- 摘要请求：历史格式化成文本 + 摘要 prompt；prompt 文本直接抄
  `~/yyds/goose` `crates/goose-context-management/src/prompts/compaction.md`（注明出处）。
  模型直接输出 markdown，不做 JSON + 模板层。
- 防摘要器自身溢出：先把 >2KB 的 ToolResult 正文替换成 `[truncated N bytes]` 再送。
- 结果：历史替换为 `[摘要 User 消息]`（`# Conversation Summary` 开头），
  末尾未回答的 user 消息保留；用 `02` 的原子重写落盘；发 `Event::Compacted`。

## Q5 · 取消与会话不变量 {#q5}

- `stream_assistant` 与每个工具调用都 `tokio::select!` cancel；流被取消时已收到的
  ToolUse 补 is_error 结果（"interrupted by user"）。
- 所有退出路径统一走 `finish_turn()` 补 ToolResult，保证 `02` 的会话不变量。

## Q6 · 测试（全部用脚本化 mock Provider，不连网） {#q6}

- 文本 → tool call → 结果 → 文本 全流程；
- approve 模式 Confirm 被调用、Deny 变 is_error 结果、AllowAlways 写回；
- 中途 cancel 后每条消息过 `validate()`；
- 大 usage.input 触发压缩；ContextOverflow 强制压缩并重试一次。
