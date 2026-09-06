difficulty: hard
agent: inherit

# 02 · 压缩完整性与未回答输入

对应方案 C01、C02。前置依赖：无。

状态：已完成（2026-09-06）。实现与行为回归同一任务提交。

## 涉及文件

- `src/agent/compact.rs`（实现与模块内测试）

其他任务的 `src/agent/mod.rs` 和 `tests/agent_continuation.rs` 不在白名单内。测试使用 compact 内的假 provider/已有公开 API，不跨文件争用。

## T1 · 只允许正常结束的摘要替换历史

工作：summarize 不能仅看到任意 Done 就认定完整。要求非空文本与 EndTurn，拒绝 MaxTokens、未知异常原因、缺 Done、流错误和不应该出现在摘要请求中的工具事件。取消仍返回既有 no-op；被拒绝摘要不调用 rewrite、不发成功 Compacted 事件。

预计修改文件：`src/agent/compact.rs`。

验收：

- [x] 正常摘要成功；length/MaxTokens、异常原因、工具事件、空文本、EOF/错误、取消矩阵均有确定性测试。
- [x] 失败前后主 JSONL 和内存历史逐字节/结构一致，不增成功压缩记录。
- [x] 自动压缩和强制压缩入口均调用同一完整性判断。

前置依赖：无。

## T2 · 保留末尾未回答消息的全部内容

工作：split_head_tail 不再将未回答 user 收窄成 content.first 的一个 String。按原顺序保存全部内容块，生成 summary 时把未回答输入完整带回；尤其恢复时 prepare_input 添加的第二、第三个 Text 必须保留。图片保留为原有内容块，不塞 base64 到摘要文本。含 ToolResult 的历史仍需精确配对，不放宽 message 校验。

预计修改文件：`src/agent/compact.rs`。

验收：

- [x] 建立 user→assistant（高 usage）→user（多个 Text）的有效历史，再 run_turn 新任务触发压缩；后续 provider 请求和活动会话都含最新任务原文，顺序正确。
- [x] 覆盖单 Text、多个 Text、Text+Image、末尾 tool results 与没有可压缩历史；图片合法且不会静默消失，所有结果 validate/resume 成功。
- [x] 摘要失败时上述未回答输入仍在原会话，不能只依赖备份找回。
- [x] 公开 maybe/force/maybe_cancelable/force_cancelable 兼容，避免影响独立执行的 01。

前置依赖：无；与 T1 同一 commit 完成。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 验收证据

- T1：`successful_summaries_preserve_all_blocks_for_every_entry` 在四个公开入口验证完整 EndTurn 摘要成功、只发一次 Compacted、备份为原文件且旧 usage 不会再次触发压缩。
- T1：`rejected_summaries_leave_history_and_files_unchanged` 覆盖 17 种拒绝响应 × 四个入口（68 组）：MaxTokens、Other、ToolUse 终止原因，各类工具事件，空白/空文本，缺 Done/EOF，建流/流内/Done 后错误，以及 Done 后额外事件。每组核对内存、主 JSONL、已有备份和文件集合一致，无成功事件。
- T1：`cancellation_is_a_noop_at_each_summary_boundary` 用 Notify 和 pending 流确定性覆盖自动/强制入口的预取消、建流、文本折叠、Done 后等待、EOF 取消；均返回 false、不写文件、不发错误或成功事件。
- T2：`resumed_run_turn_keeps_every_pending_block_and_new_task` 从高 usage 历史实际 resume，再 run_turn 新任务；正常路径比较后续 provider 请求、活动会话中全部块的顺序和最新任务原文，失败路径验证原历史及新输入仍在主会话。
- T2：成功/恢复矩阵覆盖单 Text、多 Text、Text+Image、Image 开头和仅 Image；首个 Text 保持既有摘要前缀形式，其余内容块原样保留，图片 base64 不进入摘要文本。
- T2：`tool_result_history_is_summarized_with_exact_call_pairing` 覆盖两个调用及顺序不同的精确配对结果，工具结果随调用一起进入摘要历史，图片仍按历史占位符规则处理，不产生孤立 ToolResult；`no_compactable_history_never_requests_a_summary` 验证空历史、仅未回答输入和仅摘要均零请求、零改写。
- 所有上述行为矩阵均运行原有 `message::validate` 和真实 `Session::resume`，确认不会 salvage 或改写。测试使用进程组、`kill_on_drop(true)`、硬超时与仅子进程生效的临时数据目录，不改父进程环境。

## 校验结果

- `env -u TOKEN_PLAN_API_KEY cargo test --lib agent::compact::tests -- --nocapture`：11 个通过（新增 6 个矩阵测试，共 144 组行为场景）。
- `cargo fmt --check`：通过。
- `cargo clippy --all-targets -- -D warnings`：通过。
- `env -u TOKEN_PLAN_API_KEY cargo test`：通过；592 个实际离线测试（lib 469、bin 10、agent_continuation 21、cli_e2e 49、mcp_e2e 15、provider_proxy 15、session_recovery 9、tool_inventory 4），doc-test 0。
- `env -u TOKEN_PLAN_API_KEY cargo test --test live_e2e -- --nocapture`：10 个用例均输出 `skip: TOKEN_PLAN_API_KEY not set` 后门控返回；未运行真实模型，不把门控返回计作在线验证。
- `git diff --check`：通过。

交接注意：基线的离线 CLI 用例 `plugin_task_template_expands_arguments_and_unknown_template_fails`（`tests/cli_e2e.rs`）直接加载源码 liveplug，其 SessionStart hook 会生成 `.hook-out/session_start.json`。全仓库校验后发现的本任务新产物已移到独立 `/tmp/instagent-02-generated-hook-out-20260906-jqve7ku9/liveplug-hook-out` 目录保留，并恢复本 worktree 干净；未触碰用户提供的原备份，也未修改该白名单外测试或源码夹具。协调器收口测试隔离时需一并关注此离线路径。
