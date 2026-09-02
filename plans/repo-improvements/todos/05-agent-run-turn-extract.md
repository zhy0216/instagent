difficulty: medium

`run_turn` 主循环瘦身：把 ~100 行工具执行段抽成独立方法，降低嵌套，不改任何行为。纯重构，测试全程保持绿。

## T1 · 抽出 `execute_calls` 方法

- `src/agent/mod.rs:187-283` 是 `run_turn` 内的工具执行段：malformed 短路 → 审批 → PreToolUse hook（211-231）→ emit ToolStart → 执行 → emit ToolDone → PostToolUse hook（257-265）→ deny 分支（268-280），嵌套 3 层。抽成私有方法 `execute_calls(&self, ...) -> Vec<Content>`（参数以现有局部变量为准），`run_turn` 只剩循环骨架。
- 不新建文件、不改 `lib.rs` 模块树（AGENTS.md 约定）。若拆分后私有辅助函数更清晰，放在 `agent/mod.rs` 内即可。
- 预计修改文件：`src/agent/mod.rs`。
- 验收：`agent/mod.rs` 现有 19 个测试全部保持绿（含取消、审批、hooks、压缩、不变量各路径）；`run_turn` 主体显著变短；行为零变化。
- 前置依赖：无。

## T2 · 抽 `hook_ctx` 助手 + Stop hook 块

- `src/agent/mod.rs` 有 4 处（155-161、211-231、257-265、302-321）手写 `HookContext::new(...).with_*(...)` 构造序列。抽 `fn hook_ctx(&Session, HookEvent) -> HookContext` 小助手统一。
- `:296-323` 的 Stop hook 阻止→注入 nudge→`STOP_BLOCK_LIMIT` 强制结束块，可与 T1 一并归位为独立辅助方法。
- 预计修改文件：`src/agent/mod.rs`。
- 验收：同 T1，19 个测试保持绿，行为零变化。
- 前置依赖：依赖 05 文件内 T1（同文件，串行做）。

## 本文件整体验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```
