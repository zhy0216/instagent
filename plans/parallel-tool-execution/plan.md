# 并行执行工具（已落地：repo-improvements todo 13）

> 本文档最初版本写于 ADR 0002 之前，含交互式 approval / trust 假设，已全部
> 删除。当前实现与 ADR 0002（工具自动执行、无审批 / trust / mode UI）、
> ADR 0003（D2 env、D6 containment 责任边界）对齐。

## 意图

模型一条回复里返回多个 tool call 时，instagent 过去在 `execute_calls`
（`src/agent/mod.rs`）里用一个 `for` 循环逐个执行：读多个文件、跑多个独立
命令要串行等 N 次往返。本方案把同一 assistant turn 内**可证明独立的只读
调用**改为有界并发执行；写操作、同资源冲突与顺序敏感调用保持原顺序串行。
同时给图片加单请求 / 单会话总字节预算（S15 的最小行为），并更新本文档删除
过时的审批叙述。

## 安全前提（sandbox 边界与并行安全）

- **工具自动执行，无交互审批**（ADR 0002）：并行化不引入任何交互式
  approval / trust / mode UI；PreToolUse / PostToolUse hook 仍是唯一的策略
  层，且是配置驱动的非交互决策。
- **唯一强制隔离层是外部 sandbox**（ADR 0003 D6）：应用层不承诺路径
  containment，并行的安全判定因此只做**保守降级**——判定不确定时一律串行。
  并行只发生在声明为只读的调用之间；写、顺序敏感、同资源冲突的调用永远按
  原顺序串行。资源键只做词法比较（不解析相对 / 绝对别名与符号链接）：
  漏配只可能让两个只读调用并行（互读无害），误配只会降级为串行，两个方向
  都不破坏顺序、不产生新的越权面。
- **并行不改变信任假设**：各 `ToolSource` 对并发 `call` 的安全性与串行时
  相同——`Registry` 锁只护路由表；builtin 是无状态函数；command 工具每次起
  独立子进程（进程组 + `kill_on_drop`）；rmcp client 支持并发；skills 只读
  文件。MCP 工具未声明 `readOnlyHint` 时默认按顺序敏感（串行）。
- **预算与取消是共享的**：并行任务共用同一 `ToolCtx`（cwd + 取消令牌）、
  工具各自的输出预算（04/05/06 已落地的上限全部不变），以及会话图片预算。

## 方案

### 能力元数据（`src/tools/mod.rs`）

- `ToolKind`：`ReadOnly`（可与其它只读调用并行）/ `Serial`（写或顺序敏感，
  串行）。只读声明来自各来源既有的 `ToolSpec::read_only`。
- `CallCapability { kind, resource }`：`resource` 是调用级资源冲突键。内核
  只为内置 fs 系工具（read / write / edit / tree / read_image）按输入
  `path` 字段生成；其它来源（MCP / command / skills）无可靠资源声明 →
  `None`，仅依赖 `kind`。
- `Registry::capability(call)`：查已注册 `ToolSpec`（未知工具保守按
  `Serial`）+ 来源路由得到能力。

### 三段式执行（`src/agent/mod.rs` `execute_calls`）

1. **串行决策**：按原顺序处理 malformed（is_error 结果）、取消
   （`Content::interrupted`）、PreToolUse hook（Block → is_error 结果）。
   交互点与策略钩子不进并行阶段，语义与串行版逐字一致。
2. **执行单元划分**（`execution_units`）：Ready 调用按原顺序扫描——同单元
   内全为 ReadOnly 且资源键互不相同；Serial 调用各自独立成单元；Finished
   不执行、不影响划分。
3. **执行**：单元按原顺序串行执行；单元内调用以
   `futures::future::join_all` + `Semaphore`（`PARALLEL_TOOL_LIMIT = 4`）
   有界并发。结果按原下标落槽，最终顺序与 calls 一致。

不变量：

- 每个 `tool_use_id` 恰一个答复；结果顺序与 calls 一致（`message::validate`
  兜底）。
- 事件仍按调用 id 键控（`ToolStart` / `ToolDone` 各带 id）；并发只使不同
  id 的事件交错，不产生孤儿事件。
- 取消：在途任务经 `tokio::select!` 短路（子进程靠 `kill_on_drop` 回收）、
  未启动的单元补 `Content::interrupted`；`kill_on_drop` 与进程组约定不变。
- PostToolUse hook 在各任务内随结果触发（观察型事件，决策忽略）。

### 图片预算（S15 最小行为，不含 RM2 blob/reference）

- **会话总预算**（`src/agent/mod.rs` `SESSION_IMAGE_BUDGET = 64 MiB`）：
  同一会话全部 `Content::Image` 解码字节之和的上限。新图片会使总量超限时
  不附上，对应工具结果文本带可操作提示（已用多少、本图多大、建议完成当前
  分析或开新会话）。原子计数（CAS）防并行调用同时通过检查。
- **单请求预算**（`src/provider/openai.rs` `REQUEST_IMAGE_BUDGET = 32 MiB`）：
  请求边界对历史图片先**去重**（相同 `media_type + data` 只内嵌首次出现，
  其余替换为去重提示），解码字节总和仍超限时按**生命周期淘汰**（最旧优先、
  保留最新），被淘汰位置替换为可操作的重读提示。
- 不做 blob/reference 存储、不引入新 decoder 依赖、不改会话格式；两层预算
  都只在边界做替换 / 拒绝，会话 JSONL 保持原样。

## 拆解（单任务，一个 commit）

1. `src/tools/mod.rs`：`ToolKind` / `CallCapability` / `Registry::capability`、
   `ImageData::decoded_bytes`，及能力解析测试。
2. `src/agent/mod.rs`：三段式 `execute_calls`、`execution_units`、会话图片
   预算与拒绝提示，及并发 / 顺序 / 取消 / 失败隔离 / 事件配对 / 预算测试。
3. `src/provider/openai.rs`：`REQUEST_IMAGE_BUDGET`、`plan_images`
   （去重 + 淘汰）、`format_messages` 接线，及预算测试。
4. 本文档更新（删除 approval / trust 假设，写明 sandbox 边界与并行安全前提）。

## 测试覆盖

- **并发**：两个只读调用等 `Barrier::new(2)`——串行会死锁、并行双双通过
  （10s timeout 兜底）；6 个只读调用峰值并发恰为 `PARALLEL_TOOL_LIMIT`。
- **顺序 / 冲突**：`execution_units` 纯函数覆盖同资源键、Serial、Finished
  混排的划分；写（Serial）与读分属不同单元、峰值并发为 1 的端到端断言。
- **取消 / 超时**：3 个永不返回的只读调用并行中取消——有界时间内全部回收，
  每个调用恰一个 interrupted 结果，`message::validate` 通过。
- **失败隔离**：并行中单个调用失败，其余照常成功，失败落在正确的调用 id。
- **事件顺序**：每个调用 id 恰一个 `ToolStart` / `ToolDone` 且 Start 先于
  Done；tool result 与调用 id 一一对应且顺序同 calls。
- **图片预算**：去重只内嵌首次出现；超限从最旧淘汰、替换文本可操作；会话
  预算拒绝新图且提示可操作、被拒图片不落会话。

## 校验

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo rustdoc --lib -- -D warnings
```

（AGENTS.md 约定的三条 + rustdoc，每个 commit 必须全绿。）

## 风险与假设

- 模型单条消息的 tool call 数量较小（实践中 < 20），并发上限 4 已足够；
  若 MCP 等远端并行来源成为瓶颈，调整 `PARALLEL_TOOL_LIMIT` 即可。
- 并发事件交错使 `render` 的 `✓ {elapsed} ms` 行与 `▶` 行不一定相邻——
  事件按 id 键控，纯观感，不处理。
- `readOnlyHint` / `read_only` 声明是来源的自我描述；恶意来源谎报只读属于
  sandbox 的责任面（ADR 0002 / 0003 D6），应用层不为它做额外验证。
