difficulty: hard

# 03 · 工具执行前校验、续轮与取消

优先级：P1。模型：`bailian-token-plan/qwen3.8-max`。方案发现：A01–A04。

前置依赖：01-sse-stream-integrity、02-session-io-recovery 均已合入。

## 涉及文件

- `src/agent/mod.rs`
- `src/agent/exec.rs`
- `src/agent/compact.rs`
- `tests/agent_continuation.rs`（允许新增的库行为测试）

## T1 · 在副作用前拒绝非法消息

要做：run_turn 收到 assistant 后，在 PreToolUse/ToolStart/execute_calls 前用既有消息校验核心检查它与历史的关系，覆盖重复/历史复用 ID、空 ID/name 和错误块；合法但 JSON 参数损坏仍按已有 malformed ToolResult 流程。工具输出的图片先校验再进入结果；坏图片转换成可诊断错误结果，不让 session 落盘失败。finish_turn 改用 02 的批量追加。

预计修改文件：`src/agent/mod.rs`、`src/agent/exec.rs`、`tests/agent_continuation.rs`。

验收：计数型 ToolSource 和 hook 标记证明非法响应零副作用；历史保持可校验；合法并行工具的结果/事件与调用 ID 一一配对；坏图片不会导致下一轮永久失败。

前置依赖：01-sse-stream-integrity、02-session-io-recovery。

## T2 · 继续末尾为 user 的会话

要做：准备新输入时，末尾 assistant 正常 append；末尾 user 将新文本追加到原 content 并原子 rewrite，保留摘要、旧 prompt、ToolResult、Image 的顺序。不放宽角色交替，不丢旧输入，不发明模型回答。错误与取消收尾保留已完成内容，确保下一次 run_turn 可以继续；预取消在 hook/写文件前直接 Interrupted。

预计修改文件：`src/agent/mod.rs`、`tests/agent_continuation.rs`。

验收：对预取消、流开始前失败、工具后取消、MaxTurns、Stop hook 最后一轮阻止、手动压缩、resume 待回答 user，分别连续调用两次 run_turn；第二次成功且 validate/resume 一致。修正旧“预取消 touches nothing”测试实际上断言一条 user 的错误预期。

前置依赖：本文件 T1。

## T3 · 取消覆盖等待阶段，摘要成功后才替换

要做：工具 inventory、UserPromptSubmit/Pre/Post/Stop hooks、maybe/force compaction 的等待均受 token 约束，drop 等待 future 能回收其子进程；公开 compact::force/maybe 可保留兼容包装，并新增显式 cancel 入口供 08 接线。summarize 只接受非空、协议完成的输出；空/断流/取消时不 rewrite。不要将取消误报成 provider 截断。

预计修改文件：`src/agent/mod.rs`、`src/agent/exec.rs`、`src/agent/compact.rs`、`tests/agent_continuation.rs`。

验收：用 pending mock 和同步通知控制取消点，避免长 sleep；每个阶段取消在测试硬超时内返回；摘要失败/空文本/缺 Done 前后原 JSONL 字节一致。现有 overflow 仅重试一次、工具只读并发数上限和 STOP_BLOCK_LIMIT 均保留。这里只补取消与完整性，不实施 roadmap 的有状态 hook 调度重构。

前置依赖：本文件 T2。

## 校验与完成

```bash
cargo test agent
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

一个本地 commit。完成记录注明 compact 的可取消 API；CLI 接线和 CLI e2e 留给 08，不改 CLI 或事件枚举。

## 完成记录（2026-09-05）

实现：`src/agent/mod.rs`、`src/agent/exec.rs`、`src/agent/compact.rs` + 新增
`tests/agent_continuation.rs`（21 项库行为测试）。

- T1：`run_turn` 在 PreToolUse/ToolStart/`execute_calls` 前用
  `message::validate_for_append` 同一核心校验 assistant 与历史（重复/历史复用
  ID、空 ID/name、错位块），非法零副作用错出（不跑 hook、不执行工具、不落盘，
  附 `Event::Error`）；malformed JSON 照旧走 ToolResult 流程。工具图片先经合成
  最小历史的校验核心探针（复用 mime/canonical base64/尺寸约束，不回显数据），
  坏图丢弃并转成带脱敏诊断的错误结果。`finish_turn` 改批量 `append_batch`。
- T2：`prepare_input`——末尾 assistant 正常 append，末尾 user（摘要/未回答输入/
  ToolResult/nudge）把新文本作新 Text 块并入原 content 后原子 rewrite，顺序保留；
  预取消在 hook/写文件前直接 `Interrupted`（零写入零 hook），修正旧"预取消
  touches nothing"用例的错误预期（1 条 user → 0 条 + 仅 header 落盘）。
- T3：inventory（`stream_assistant` 的 `list`、`capability`）、UserPromptSubmit/
  Pre/Post/Stop hooks、`maybe/force` 压缩的等待全部进 `select!` 受 token 约束，
  取消即 drop 等待 future（hook/工具子进程经既有进程组 + `kill_on_drop` 回收）；
  `summarize` 只接受非空且收到 Done 的输出，空/缺 Done/提供方错误返回 `Err`
  且不 rewrite，取消按 no-op（`Ok(None)`）处理、不误报截断。overflow 仅重试一次、
  只读并发上限、`STOP_BLOCK_LIMIT` 语义不变（由 `src/agent` 内既有单测锁定）。

供 08 使用的可取消 API（`crate::agent::compact`，旧 `maybe`/`force` 为永不取消的
兼容包装，签名不变，`cli/repl.rs` 的 `/compact` 调用不受影响）：

```rust
pub async fn maybe_cancelable(agent: &Agent, session: &mut Session, events: &mpsc::Sender<Event>, cancel: &CancellationToken) -> crate::Result<bool>
pub async fn force_cancelable(agent: &Agent, session: &mut Session, events: &mpsc::Sender<Event>, cancel: &CancellationToken) -> crate::Result<bool> // true = 发生了 rewrite
```

`force_cancelable` 取消/无可压缩历史返回 `Ok(false)`（不改会话、不发事件），
摘要失败返回 `Err`（同样不改会话）；调用方凭返回值或 token 区分 no-op 与成功。

校验：`cargo test agent`（lib agent 41 项）、`cargo fmt --check`、
`cargo clippy --all-targets -- -D warnings`、
`env -u TOKEN_PLAN_API_KEY cargo test` 全绿（lib 451、agent_continuation 21、
cli_e2e 15、live_e2e 10 skip、mcp_e2e 14、provider_proxy 15、session_recovery 9；
预置 `TOKEN_PLAN_API_KEY` 时 live 走真实网络超时，系环境限制）。CLI 接线和
CLI e2e 留给 08；未实施 RM05 有状态 hook 调度重构。
