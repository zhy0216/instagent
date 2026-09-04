difficulty: medium

# 03 loop 接线：execute_calls 拆出 Image 块

按 `plans/image-support/plan.md` "loop 接线"节，把工具产出图片接入会话。
只依赖 01（`Content::Image` + `ToolOutput.image` 存在即可）；测试用 stub
`ToolSource` 产图，不依赖 02 的 `read_image`，与 02/04/05 并行。

## T1 · execute_calls 改图片块

- 要做什么：`src/agent/mod.rs` `execute_calls`（:209）末尾
  `results.push(Content::tool_result(call, output));`（:318 附近）改为：
  先 `let image = output.image.take();`（`output` 需 `mut`），push
  tool_result 后，`if let Some(img) = image { results.push(Content::Image(img)); }`
  ——Image 块与 ToolResult 进同一条 user 消息。
- 确认不改的东西：`finish_turn`（results 非空即整组落盘）、
  ToolDone 预览与 PostToolUse hook（只经 `output.text`）、
  `message::validate`（Image 块天然被忽略，不变量 2/3 不受影响）。
- 预计修改：`src/agent/mod.rs`。
- 验收：T2 测试绿；无图片的工具调用行为逐字不变（既有测试零回归）。
- 前置依赖：01。

## T2 · loop 级测试

- 要做什么：`src/agent/mod.rs` 测试模块加一个用例（复用现有
  `fn agent(...)` + `MockProvider` 手法）：注册一个返回
  `ToolOutput { image: Some(..), .. }` 的 stub `ToolSource`
  （`Registry::register` 接受任意 `Arc<dyn ToolSource>`），让 mock
  provider 吐一次该 tool call，跑一轮后断言：
  - 落盘的 user 消息 content 恰为 `[ToolResult, Image]`（顺序：结果在前）；
  - `message::validate`（或现有会话读取路径）通过；
  - 同一会话 `resume` 后 Image 块字节不变（借 01 的 serde 形状）。
- 预计修改：`src/agent/mod.rs`（测试模块）。
- 验收：`cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
  cargo test` 全绿。
- 前置依赖：T1。
