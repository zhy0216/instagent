difficulty: medium

# 04 OpenAI 引擎图片序列化

按 `plans/image-support/plan.md` "OpenAI 引擎"节。只改
`src/provider/openai.rs` `format_messages`（:142）的 user 分支。依赖 01
（`Content::Image` 与 `ImageData` 字段）。anthropic 引擎已按 ADR 0001
移除，不在本项。

## T1 · user 分支含图时产出 parts 数组

- 要做什么：
  - `src/provider/openai.rs` user 分支（:177）收集 texts/results 的循环里
    加 `Content::Image(img) =>` 收集图片（01 已加的最小忽略臂在此替换为
    真正的实现）。
  - user 消息**无 Image** → 行为逐字不变：仍
    `out.push(json!({"role":"user","content": texts.join("\n")}))`，
    保持怪癖 3（`out.extend(results)` 先行，文本 user 消息在后）。
  - **有 Image** → 该 user 消息 `content` 改为部分数组：每个文本一个
    `{"type":"text","text":...}`，每张图片一个
    `{"type":"image_url","image_url":{"url":"data:{media_type};base64,{data}"}}`
    （goose `convert_image` OpenAI 形状）；只有图片没文本时也要发该
    user 消息。顺序仍是 `results（tool 消息）→ user 消息`，怪癖 3 不被破坏。
  - assistant 分支（:150）保持 01 加的 `Content::Image(_) => {}` 忽略臂
    （模型流不产出图片，仅保穷举）。
  - 不做 media type 白名单降级（只生产 4 种合法类型，MCP 图片非目标）。
- 预计修改：`src/provider/openai.rs`。
- 验收：T2 测试绿；现有 `quirk3_*`（:537 等）逐字通过。
- 前置依赖：01。

## T2 · 序列化测试

- 要做什么：`src/provider/openai.rs` 测试模块（或既有 `format_messages`
  用例附近）加断言：
  - 含图 user 消息 → `content` 是数组，含一个 `text` 部分（有文本时）+
    一个 `image_url` 部分，`url` 字符串恰为
    `data:{media_type};base64,{data}`；
  - 无图 user 消息 → 与现有断言逐字一致（回归：`content` 仍是字符串）；
  - `tool` 角色消息仍紧跟含 `tool_calls` 的 assistant 消息（怪癖 3 顺序
    在有图时也不被破坏）。
- 预计修改：`src/provider/openai.rs`（测试模块）。
- 验收：`cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
  cargo test` 全绿。
- 前置依赖：T1。
