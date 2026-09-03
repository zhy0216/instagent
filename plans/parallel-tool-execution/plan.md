# 并行执行工具（v2 候选落地）

## 意图

模型一条回复里返回多个 tool call 时，instagent 目前在 `execute_calls`
（`src/agent/mod.rs:220`）里用一个 `for` 循环逐个执行：审批 → hook → 执行 →
下一个。读多个文件、跑多个独立命令时要串行等 N 次往返。本方案把它改成
goose（commit `4ad43df`，`agents/state_machine/ops_toolcalling.rs`）的
"先批量决策、再并发执行、按原序收集"两段式，即 `docs/goose-from-scratch-plan.md`
§7 v2 表里的"并行执行工具"（估算 100 行）。

分析结论：接收侧已支持并行（两个 provider 的流式组装都能还原一条消息里的多个
tool_use，有 `parallel_tool_calls` fixture），`Registry::call` / `ToolSource`
均为 `&self + Send + Sync`，rmcp client 支持并发调用，`Registry` 的锁只护路由表
查询——执行层没有阻碍。真正耦合串行的是**审批**（`approval.decide` 交互式阻塞）
和 **PreToolUse hook**（外部命令、按插件顺序串行），这两者必须留在串行阶段。

## 目标

- 同一条 assistant 消息里的多个合法 tool call 并发执行（`futures::future::join_all`）。
- 会话不变量不变：结果 `Vec<Content>` 顺序与 calls 一致，每个 `tool_use_id` 恰好一个答复。
- 审批、PreToolUse hook 的语义与时序不变（仍逐个、按序、可交互）。
- 取消语义不变：运行中的 call 走现有 `tokio::select!` 短路，未启动的补 `Content::interrupted`。

## 非目标

- 不做并发上限（goose 也不做；模型单条消息的 call 数天然有限）。
- 不加配置开关（不加 `parallel_tools: bool` 之类，YAGNI）。
- 不改 `render.rs`（事件按 id 键控，交错的 ToolStart/ToolDone 渲染只是观感问题，
  且与 goose 一致；将来嫌乱再给 ToolDone 带名字）。
- 不动 `docs/goose-from-scratch-plan.md` §7 表（roadmap 文档不回改）。

## 方案

### 三段式重写 `execute_calls`（`src/agent/mod.rs`，唯一改动的业务文件）

**阶段 1：串行决策（保持现有顺序）**
按 calls 顺序逐个处理，产出每个 call 的处置：

- `streamed.malformed` 命中 → 直接得错误结果（现状不变）；
- `streamed.cancelled || cancel.is_cancelled()` → `Content::interrupted`（现状不变）;
- `approval.decide` 拒绝 → deny 事件 + 结果（现状不变）；
- PreToolUse hook Block → 错误结果（现状不变）；
- 其余 → 标记为 **Execute**，进入阶段 2。

即：交互点（审批）与外部命令（hook）全部留在串行段，行为与今天逐字一致。

**阶段 2：并发执行**
对阶段 1 标为 Execute 的 calls，每个包一个 async 任务（内容 = 现循环体里的
执行段：emit ToolStart → `tokio::select!` cancel / `tools.call` → emit ToolDone
→ PostToolUse hook），`futures::future::join_all` 并发跑。
任务返回 `(原下标, Content)`。

**阶段 3：按原序收集**
结果写入 `Vec<Option<Content>>` 的对应下标，最后 `unwrap` 成 `Vec<Content>`，
保证顺序与 calls 一致（与现状相同）。

### 设计决策

| 决策 | 选择 | 理由 |
|---|---|---|
| 审批位置 | 串行阶段，全部批完再执行 | 交互式提示不可并发；goose 同构（审批由独立操作提前解决，执行时不再交互） |
| PreToolUse hook | 串行阶段逐个跑 | hooks 协议是外部命令按插件顺序串行（`hooks.rs:296`）；goose 的 dispatch 循环同样串行跑 pre-hook |
| PostToolUse hook | 在各任务内、各自结果出来后跑 | 观察型事件、决策被忽略，随完成时序触发即可（goose `with_post_tool_hooks` 同款） |
| 结果顺序 | 按 calls 原序 | 会话不变量与现状一致；tool_use_id 配对虽不依赖顺序，但保持稳定便于测试与 diff |
| 并发原语 | `futures::future::join_all` | 依赖已在（`compact.rs` 已用 `futures`）；无需 `select_all`——instagent 的 ToolResult 是 await 出的值，不是 goose 那种多路流 |
| 取消 | 任务内沿用现有 `tokio::select!`；阶段 1 开头检查短路 | 与现状逐字一致，不新增语义 |

### 拆解

单任务，无内部依赖（一个 commit）：

1. 重写 `execute_calls` 为三段式（约 -100 / +120 行，含拆分出的执行任务函数）。
2. 新增测试（同文件 `mod tests`，沿用 `MockProvider` + 脚本流约定）：
   - **并发证明**：注册一个测试用 `ToolSource`，`call` 里等
     `tokio::sync::Barrier::new(2)`——串行执行会死锁（套 `tokio::time::timeout`
     断言不超时），并发则双双通过；
   - **顺序保持**：一条 assistant 消息两个 call，结果顺序与 calls 一致；
   - **混合分支**：malformed + deny + 正常并发混排，各自结果落在正确下标；
   - **取消**：并发执行中 cancel，运行中的得 interrupted 错误结果、未启动的得
     `Content::interrupted`，会话不变量校验（`message.rs` 的校验函数）通过。
3. 现有 21 个 loop 测试（`mod tests` 里 `full_loop_text_tool_text`、
   `approve_deny_becomes_error_tool_result`、`pre_tool_use_block_*` 等）不改语义，
   必须原样通过——它们是串行行为的回归基线。

## 校验

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

（AGENTS.md 约定的三条，每个 commit 必须全绿。）

验收：新增的 barrier 测试证明并发；既有测试全绿证明审批 / hook / 取消 / 会话
不变量无语义回归。

## 风险与假设

- **假设**：`ToolSource` 实现（builtin / mcp / command / skills）对并发 `call`
  安全。已核对：`Registry` 锁只护路由表；builtin 是无状态函数；command tools 每次
  起独立子进程（`03` 的进程组 + kill_on_drop）；rmcp client 支持并发。skills 来源
  只读文件。若将来出现有状态来源，需在其实现内部自行同步。
- **风险**：approve 模式下多个 call 会连续弹审批提示再一起执行——这是预期行为
  （先批后跑），与逐个"批一个跑一个"的体感不同，在 commit message 里写明即可。
- **风险**：并发事件交错使 `render.rs` 的 `✓ {elapsed} ms` 行与 `▶` 行不一定相邻。
  纯观感，非目标内，不处理。
- **假设**：模型单条消息的 tool call 数量较小（实践中 < 20），不设并发上限。
