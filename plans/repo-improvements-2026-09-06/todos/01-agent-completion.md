difficulty: hard
agent: inherit

# 01 · 工具响应完成状态与图片预算

对应方案 A01、A02。前置依赖：无。

## 涉及文件

- `src/agent/mod.rs`
- `src/agent/exec.rs`
- `tests/agent_continuation.rs`

## T1 · 在任何工具副作用前拒绝异常响应

工作：修复 run_turn 仅在 calls 为空时检查终止原因的分支。完整参数的工具调用如果来自 MaxTokens/未知异常终止或明确不完整响应，不能进入 PreToolUse、ToolStart 或工具本体，也不能由下一响应把本次执行改为成功。用一个清晰的状态判断覆盖有工具/无工具路径；保留正常 ToolUse 与既有正常兼容路径、取消语义，以及正常响应里单个参数 JSON 损坏的工具错误反馈。

预计修改文件：`src/agent/mod.rs`、`src/agent/exec.rs`、`tests/agent_continuation.rs`。

验收：
- 假 provider 返回完整 shell call + MaxTokens，工具和 hooks 的计数均为 0，run_turn 返回失败；不继续发第二次成功请求。
- 覆盖未知异常、缺完整结束、取消、正常 ToolUse、正常响应的坏参数 JSON；检查消息可 validate/resume，ToolUse/ToolResult 成对。
- 更新与 ADR 0004 冲突的旧“截断后继续成功”断言；不得仅改断言而没有实现完成状态的检查。
- 失败诊断有原因，不能输出误导性的 Done；已有正常并行和取消回归通过。

前置依赖：无。

## T2 · 图片通过校验后才占用预算

工作：调整 execute_calls 的 image_validation_note/reserve_image_bytes 顺序或回退机制。被拒绝图片不能消耗同批后续调用额度；保留并行原子记账和错误图片不入 session 的行为。

预计修改文件：`src/agent/exec.rs`、`tests/agent_continuation.rs`；必要的同模块测试在 `src/agent/mod.rs`。

验收：
- 小预算 fixture 中，非法图片在前、合法图片在后，后者仍可附加；非法图片留下可诊断错误且不入会话。
- 多个并行合法图片的总保留字节不超过预算，顺序和调用 ID 配对保持。
- 不为测试分配数十 MB；复用/参数化现有预算 helper 和假工具源。

前置依赖：无；与 T1 同一 commit 完成。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

