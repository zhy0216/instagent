# image-support TODO 队列

来源方案：`plans/image-support/plan.md`（图片支持：`read_image` 工具 +
`Content::Image`，openai 引擎序列化）。

## 优先级

| 文件 | 优先级 | 难度 | 说明 |
|---|---|---|---|
| done/01-data-model.md ✅ | P0 | medium | `ImageData` / `ToolOutput.image` / `Content::Image` 新变体 + 两处穷举匹配占位臂 + serde round-trip 测试 |
| 02-read-image-tool.md | P1 | hard | 新工具 `read_image`：手写 base64、魔数嗅探、错误文案、注册与端到端测试 |
| 03-loop-wiring.md | P1 | medium | `execute_calls` 把 `output.image` 拆成同消息 `Image` 块 + loop 级测试 |
| 04-openai-serialization.md | P1 | medium | `format_messages` user 分支含图时改 parts 数组（data URL）+ 回归测试 |
| done/05-compact-placeholder.md ✅ | P2 | easy | `format_history` 图片占位文案最终化 + "base64 不进摘要" 测试 |

## 文件

1. `done/01-data-model.md` ✅ — 完成。无依赖，先行（其余全部依赖它）。
2. `02-read-image-tool.md` — 依赖 01。可与 03/04/05 并行。
3. `03-loop-wiring.md` — 依赖 01。可与 02/04/05 并行。
4. `04-openai-serialization.md` — 依赖 01。可与 02/03/05 并行。
5. `done/05-compact-placeholder.md` ✅ — 完成。占位臂文案与方案一致，`split_head_tail` 无需改动，新增 "base64 不进摘要" 测试。

并行说明：01 落地后，02–05 各自只改互不相交的文件
（`builtin/image.rs`+`builtin/mod.rs` / `agent/mod.rs` / `provider/openai.rs` /
`agent/compact.rs` 测试），可同时开 4 个 worktree。02 的 loop 级 e2e 不需要
等 03：03 的测试用 stub `ToolSource` 产图，不依赖 `read_image` 存在。
04 与 01 在同函数（`format_messages`）有小幅区域重叠，rebase 时以 04 的
实现为准。

## 校验（每个任务完成后全部执行）

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
