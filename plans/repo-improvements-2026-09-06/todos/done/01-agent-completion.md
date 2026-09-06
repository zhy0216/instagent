difficulty: hard
agent: inherit

# 01 · 工具响应完成状态与图片预算

对应方案 A01、A02。前置依赖：无。

状态：已完成（2026-09-06）；T1、T2 与行为回归在同一任务提交中交付。

## 涉及文件

- `src/agent/mod.rs`
- `src/agent/exec.rs`
- `tests/agent_continuation.rs`

## T1 · 在任何工具副作用前拒绝异常响应

工作：修复 run_turn 仅在 calls 为空时检查终止原因的分支。完整参数的工具调用如果来自 MaxTokens/未知异常终止或明确不完整响应，不能进入 PreToolUse、ToolStart 或工具本体，也不能由下一响应把本次执行改为成功。用一个清晰的状态判断覆盖有工具/无工具路径；保留正常 ToolUse 与既有正常兼容路径、取消语义，以及正常响应里单个参数 JSON 损坏的工具错误反馈。

预计修改文件：`src/agent/mod.rs`、`src/agent/exec.rs`、`tests/agent_continuation.rs`。

验收：
- [x] 假 provider 返回完整 shell call + MaxTokens，工具和 hooks 的计数均为 0，run_turn 返回失败；不继续发第二次成功请求。
- [x] 覆盖未知异常、缺完整结束、取消、正常 ToolUse、正常响应的坏参数 JSON；检查消息可 validate/resume，ToolUse/ToolResult 成对。
- [x] 更新与 ADR 0004 冲突的旧“截断后继续成功”断言；不得仅改断言而没有实现完成状态的检查。
- [x] 失败诊断有原因，不能输出误导性的 Done；已有正常并行和取消回归通过。

前置依赖：无。

## T2 · 图片通过校验后才占用预算

工作：调整 execute_calls 的 image_validation_note/reserve_image_bytes 顺序或回退机制。被拒绝图片不能消耗同批后续调用额度；保留并行原子记账和错误图片不入 session 的行为。

预计修改文件：`src/agent/exec.rs`、`tests/agent_continuation.rs`；必要的同模块测试在 `src/agent/mod.rs`。

验收：
- [x] 小预算 fixture 中，非法图片在前、合法图片在后，后者仍可附加；非法图片留下可诊断错误且不入会话。
- [x] 多个并行合法图片的总保留字节不超过预算，顺序和调用 ID 配对保持。
- [x] 不为测试分配数十 MB；复用/参数化现有预算 helper 和假工具源。

前置依赖：无；与 T1 同一 commit 完成。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 验收记录

- T1：`abnormal_tool_responses_fail_before_any_side_effect_and_survive_resume` 覆盖 MaxTokens、Other、缺 Done、缺 ToolUseEnd（ToolUse / EndTurn / EOF）六种响应。完整 shell 和计数 probe 均零执行，PreToolUse / PostToolUse / Stop 均零触发，无 ToolStart / ToolDone；provider 只请求一次，错误结果按调用 ID 配对，实际 `Session::resume` 不丢消息、不触发 salvage。调用方另起 run_turn 才会消费预置的成功响应。
- T1：`normal_tool_stop_reasons_execute_and_pair_results` 保留 ToolUse 与 EndTurn 工具响应；`cancelled_abnormal_tool_responses_keep_interrupted_semantics` 覆盖取消与 MaxTokens / Other / EOF 的组合；正常 ToolUse 的坏 JSON 保留单个工具错误反馈。旧截断测试改为 `truncated_tool_stream_fails_with_paired_error_result`，完成状态检查在 execute_calls 之前统一覆盖有工具和无工具路径。
- T1：新异常响应回归在修复前实际失败，诊断为“异常工具响应必须立即失败: Done”；修复后通过。原正常并行、并发上限、失败隔离、各阶段取消回归均通过。
- T2：`invalid_images_do_not_consume_later_call_budget` 使用解码后 68 字节的 PNG 和 68 字节预算，分别验证非法 base64 / MIME 在前、合法图片在后。修复前后者因非法图片占用 68 字节而被拒绝，调整为校验后原子预留后通过；非法图片仅留错误文本，合法图片进入会话。
- T2：`parallel_images_share_budget_and_preserve_call_order` 用 barrier 强制四个只读图片调用并行；204 字节预算含一张历史图片，最终只再保留两张，总量恰为预算，结果和事件保持调用 ID 配对及消息顺序。
- T2：复用参数化 ImageSource 和私有 execute_calls 的预算参数，公开 Agent / AgentCfg 接口和 64 MiB 默认预算不变。旧大图片预算测试替换为 68 字节夹具与 67 字节预算，不再分配数十 MiB；预算拒绝后仍可继续正常完成。

校验（均退出 0）：

- `env -u TOKEN_PLAN_API_KEY cargo test agent::`：46 个 agent 测试通过。
- `env -u TOKEN_PLAN_API_KEY cargo test --test agent_continuation`：24 个测试通过。
- `cargo fmt --check`：通过。
- `cargo clippy --all-targets -- -D warnings`：通过。
- `env -u TOKEN_PLAN_API_KEY cargo test`：591 个离线 Rust 测试通过（lib 465、bin 10、agent_continuation 24、cli_e2e 49、mcp_e2e 15、provider_proxy 15、session_recovery 9、tool_inventory 4），doc-test 0。10 个 live 函数因缺少 TOKEN_PLAN_API_KEY 门控返回，当前 harness 显示 passed / ignored 0；未执行真实模型验证。
- `git diff --check`：通过。

交接风险：现有 `tests/cli_e2e.rs::plugin_task_template_expands_arguments_and_unknown_template_fails` 直接使用源码 liveplug，离线全量测试也会由 SessionStart hook 新建 `.hook-out/session_start.json`。该测试归其他任务所有，本 todo 未修改；协调器需同步处理其夹具隔离。本 worktree 起始无该产物，本轮生成目录已保存在 `/tmp/instagent-01-offline-hook-out-20260906-gajl9s33/liveplug-hook-out`，恢复源码目录原状；未触碰用户指定的原有备份。T1 / T2 无未完成项。
