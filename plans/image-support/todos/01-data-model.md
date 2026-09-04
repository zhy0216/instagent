difficulty: medium

# 01 数据模型：ImageData / ToolOutput.image / Content::Image

按 `plans/image-support/plan.md` "数据模型"节落地新变体，并保证本 worktree
单独编译、全部校验绿（两处穷举匹配必须同时加占位/忽略臂，最终实现由
04/05 完善）。零依赖变更（不加 base64 / image crate）、不动 `src/lib.rs`。

## T1 · ImageData + ToolOutput.image

- 要做什么：
  - `src/tools/mod.rs` 新增 `pub struct ImageData { pub data: String, pub
    media_type: String }`，derive 与 `ToolOutput` 对齐（含 `Eq`——
    `ToolOutput` 现在 derive `Eq`，新字段 `Option<ImageData>` 要求
    `ImageData: Eq`）；字段文档注释照方案。
  - `ToolOutput` 增加字段 `#[serde(default,
    skip_serializing_if = "Option::is_none")] pub image:
    Option<ImageData>`；`ok()` / `err()` 构造处补 `image: None`；全仓
    `ToolOutput {` 字面量构造点（含测试）同步补字段。
- 预计修改：`src/tools/mod.rs`。
- 验收：JSON 形状向后兼容——无 image 时序列化结果与改前逐字一致。
- 前置依赖：无。

## T2 · Content::Image 变体 + 两处穷举匹配占位臂

- 要做什么：
  - `src/message.rs`：`Content` 末尾追加第四变体
    `Image(crate::tools::ImageData)`（untagged，形状
    `{"data":..., "media_type":...}`）；顶部文档注释
    "无 Image / Thinking（v1）" 同步更新。
  - `src/provider/openai.rs` `format_messages`（:142）：为编译加最小忽略臂
    ——assistant 分支 `Content::Image(_) => {}`；user 分支收集 texts 处
    同样忽略 Image（04 会实现真正的 parts 数组，这里只求穷举通过，
    可加 `// ponytail: 04 implements image parts` 注释）。
  - `src/agent/compact.rs` `format_history`（:192）：加臂输出方案定稿的
    占位文本 `{role}: [image: {media_type}, {data.len()} base64 bytes
    omitted]`（这就是最终形态，05 只补测试，勿改文案）。
  - `src/message.rs` 的 `validate` / 不变量检查不改（untagged 下 Image
    天然被忽略——若编译报错说穷举，则加 `_ =>` 已有的不管）。
- 预计修改：`src/message.rs`、`src/provider/openai.rs`、
  `src/agent/compact.rs`。
- 验收：`cargo clippy --all-targets -- -D warnings` 绿。
- 前置依赖：T1。

## T3 · serde round-trip 测试

- 要做什么：
  - `src/message.rs` 现有 `content_jsonl_round_trip`（:203）追加
    `Content::Image` 样本：序列化形状恰为
    `{"data":"...","media_type":"image/png"}`，反序列化回同值。
  - 新增 untagged 顺序回归测试：旧形状三变体的 JSON 字符串逐一
    `serde_json::from_str::<Content>`，结果仍是 Text / ToolUse /
    ToolResult（不会误配成 Image）；`ToolOutput` 旧两字段 JSON
    反序列化后 `image == None`。
- 预计修改：`src/message.rs`、`src/tools/mod.rs`（测试模块）。
- 验收：`cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
  cargo test` 全绿。
- 前置依赖：T1、T2。
