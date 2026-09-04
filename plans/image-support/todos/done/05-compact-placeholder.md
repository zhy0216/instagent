difficulty: easy

# 05 压缩图片占位测试

按 `plans/image-support/plan.md` "压缩"节。占位臂本体已由 01 落在
`src/agent/compact.rs` `format_history`（:192）：
`{role}: [image: {media_type}, {data.len()} base64 bytes omitted]`。
本任务核对文案与方案一致并补测试，防 base64 灌进摘要器。

## T1 · 核对 + 测试

- 要做什么：
  - 核对 01 的占位臂文案与方案一致（不一致则改文案，勿改结构）；
  - 确认 `split_head_tail` 不需改动（只认 `Content::Text` 开头的未答复
    user 消息；图片只出现在已答复的工具结果消息里）；
  - `src/agent/compact.rs` 测试模块加用例：构造含 `Content::Image`
    （随机 base64 串）的历史，断言 `format_history` 输出含
    `[image: image/png, {len} base64 bytes omitted]` 占位且
    **不含** `img.data` 子串（`assert!(!out.contains(&img.data))`）。
- 预计修改：`src/agent/compact.rs`。
- 验收：`cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
  cargo test` 全绿。
- 前置依赖：01。
