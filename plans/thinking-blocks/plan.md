# thinking 块（Anthropic extended thinking）

> **已关闭：被 ADR 0001 淘汰，不再执行。** 本方案的前提是原生 Anthropic
> 引擎（`src/provider/anthropic.rs`），该引擎与 `anthropic-engine` feature、
> `anthropic-compat` bundled provider 已随 ADR 0001（`docs/adr/`）整体移除，
> 文中引用的文件、fixture 与行号均已不存在。当前消息模型没有 Thinking
> 变体（见 `src/message.rs`）。以下内容仅作历史保留。

## 意图

落地 `docs/goose-from-scratch-plan.md` §7 v2 候选表里的"thinking 块（Anthropic
extended thinking 要回传 signature）"（估算 250 行）。Anthropic extended thinking
开启后，模型回复会带 `thinking`（含 `signature` 字段）与 `redacted_thinking`
（含 `data` 字段）content block；API 的硬约束是：**下一轮请求必须把上一轮的
thinking 块原样回传**（signature 是服务端校验用的签名，篡改或漏传直接 400
`invalid_request_error`；goose 在 `formats/anthropic.rs` 里专门注释
"Anthropic rejects thinking blocks sent without a matching thinking config"）。
所以消息模型、会话持久化（JSONL 原样落盘再原样回传）、anthropic engine 的请求
序列化与流式解析都要支持这两种块。

分析结论（调研后）：

- **接收侧缺口明确**：`src/provider/anthropic.rs` 的 `apply_event` 目前对
  `content_block_start` 只认 `tool_use`，注释写明"thinking / redacted_thinking
  等块类型 v1 不进消息模型，静默跳过"；已有 fixture
  `tests/fixtures/anthropic/thinking_text_tool.sse` 的期望就是"跳过"。改动是把
  这条路径反过来：累积 `thinking_delta` / `signature_delta`，块停时产出完整块。
- **模型侧有现成参照**：goose（commit `4ad43df`）
  `crates/goose-provider-types/src/conversation/message.rs:232~240` 定义
  `ThinkingContentBlock { thinking, signature }` /
  `RedactedThinkingContentBlock { data }`；`formats/anthropic.rs` 的
  `thinking_block_is_stale`（:106 附近）、format_messages 的回传分支
  （:395~420）、`apply_thinking_config`（:732~770，budget clamp 到
  `max_tokens - MIN_ANSWER_TOKENS(1024)`）、流式 `ThinkingState`
  （:896~1050，start 块可携带初始 thinking/signature，delta 追加，块停才产出）。
- **发送侧缺口**：`build_request_body`（`anthropic.rs:148`）没有 `thinking`
  请求参数的位置；`Request`（`provider/mod.rs:61`）没有携带开关的字段。
- **其余引擎无协议对应物**：OpenAI chat completions 协议没有 thinking 块
  （`reasoning_effort` 是另一回事，第二版 §2.3 已明确 v1 忽略），openai / proxy
  engine 只需在序列化阶段跳过新变体。
- **会话模型兼容性好**：`Content` 是 `#[serde(untagged)]`，新增变体不破坏旧
  JSONL 读取；`validate` 的四条不变量与 thinking 块无交集（不影响角色交替、
  tool_use 配对、非空判定），无需改。

## 目标

- `Content` 增加 `Thinking { thinking, signature }` 与
  `RedactedThinking { data }` 两个变体，字段名与 Anthropic wire / goose 对齐；
  JSONL 原样落盘、原样读回（signature 一字不改）。
- anthropic engine：请求体支持 `thinking: {"type":"enabled","budget_tokens":N}`；
  `format_messages` 按开关决定回传或丢弃；流式解析累积并产出完整 thinking /
  redacted_thinking 块。
- agent loop 把流里的 thinking 块折叠进 assistant 消息，下一轮请求原样带回。
- 配置项 `thinking_budget: Option<u32>` 控制开关与预算；缺省关闭时行为与今天
  逐字一致（含全部既有测试与 fixture 期望）。

## 非目标

- **不做 openai / proxy engine 的 thinking 支持**：协议无对应物；这两个 engine
  在序列化阶段静默跳过 `Content::Thinking` / `Content::RedactedThinking`。
  理由：thinking 块只会由 anthropic engine 产生，跨 engine 恢复会话时跳过是
  唯一不炸 API 的选择（与现状 `Content::ToolResult` 在 assistant 分支被跳过
  同构）。
- **不做 UI 展示**：thinking 文本不进 `Event` 流（不给渲染层加折叠/样式），
  只进消息模型。展示是独立的 v2 项（goose 有专门的 `goose-cli/src/session/thinking.rs`
  渲染器，约 200 行，另立计划）。
- **不做 thinking 过期判定**：goose 的 `thinking_block_is_stale` 依赖逐消息的
  requested/resolved model 元数据，instagent 消息模型没有该字段；v1 接受
  "换模型恢复旧会话可能因签名失配被 API 拒绝"的风险（见风险节）。
- **不做 `<think>` 标签过滤**（goose `thinking.rs` 的 ThinkFilter）：那是给
  非结构化输出模型的文本过滤器，与 Anthropic 原生 thinking 块无关，不在本项范围。
- **不给摘要请求开 thinking**：compact 的摘要请求是单条 user 消息、无工具调用，
  开思考纯浪费预算，固定传 `thinking: None`。

## 方案

### 1. `Content` 新变体（`src/message.rs`）

```rust
pub enum Content {
    Text(String),
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
    Thinking { thinking: String, signature: String },
    RedactedThinking { data: String },
}
```

- 字段名逐字对齐 Anthropic wire（`thinking` / `signature` / `data`），也与
  goose `ThinkingContentBlock` / `RedactedThinkingContentBlock` 同名，序列化
  往返不需要任何改名。
- `#[serde(untagged)]` 下，`{"thinking": "...", "signature": "..."}` 与
  `{"data": "..."}` 不会与既有变体混淆（`ToolUse` 要求 `id`+`name`+`input`，
  `ToolResult` 要求 `tool_use_id`）。
- `validate` 不改：thinking 块非空判定不适用不变量 3（空 `thinking` 文本但带
  `signature` 的块是合法形态，必须能承载）；`tool_uses` / `assistant_text`
  已用通配分支，天然忽略。
- 文档注释 "无 Image / Thinking（v1）" 相应更新。

### 2. `Request` 与 `StreamEvent`（`src/provider/mod.rs`、`shared.rs`）

- `Request` 增字段 `thinking: Option<u32>`（预算 token 数；None = 关闭）。
  各 engine 自决是否使用：只有 anthropic 读，openai / proxy 忽略。
- `StreamEvent` 增两个变体（块级产出，不做逐 delta 事件——UI 不展示，逐
  delta 没有消费者）：

  ```rust
  ThinkingBlock { thinking: String, signature: String },
  RedactedThinkingBlock { data: String },
  ```

- `shared::testutil::event_to_json` 补对臂
  （`{"kind":"thinking_block",...}` / `{"kind":"redacted_thinking_block",...}`），
  `testutil::request` 补 `thinking: None`。

### 3. anthropic engine（`src/provider/anthropic.rs`）

**请求侧 `build_request_body`**：

- `req.thinking = Some(budget)` 时：clamp `budget = min(budget, max_tokens - 1024)`，
  clamp 后 `>= 1024` 才插入 `body["thinking"] = {"type":"enabled","budget_tokens":N}`
  （goose `apply_thinking_config` 同款，MIN_ANSWER_TOKENS = 1024：thinking token
  计入 `max_tokens`，必须给正文留余量）。低于 1024 时静默不开（不发半吊子参数）。
- 开 thinking 时**强制省略 `temperature` 字段**（Anthropic 要求 extended
  thinking 下 temperature = 1，缺省即 1；现状 agent 恒传 `None`，此规则只防
  将来接线后踩坑）。
- `format_messages` 增回传分支，规则（对齐 goose :395~420）：
  - `req.thinking` 为 `None`（本轮未开）→ 丢弃全部 `Thinking` /
    `RedactedThinking` 块。理由：API 拒绝"没有 thinking 配置却回传 thinking
    块"的请求，丢了不影响正确性（历史语义已被后续消息承载）。
  - 开了 → `Thinking` 块按原位置原样回传（`signature` 为空才丢，空签名无法
    通过校验）；`RedactedThinking` 原样回传 `data`。
  - 块顺序不变：`Content` 向量天然保序，interleaved thinking（thinking →
    tool_use → thinking → text）原样回放。

**流式侧 `apply_event` / `AnthropicStreamState`**：

- 状态机增 `thinking: Option<(String, String)>`（text、signature 累积器），
  与现有 `tools` map 并列（thinking 块在 Anthropic 流里不并行交错，单个
  累积器足够，goose `ThinkingState` 同款）。
- `content_block_start`：`type == "thinking"` → 用块自带的初始
  `thinking` / `signature` 字段（可为空串）初始化累积器；
  `type == "redacted_thinking"` → `data` 在 start 事件里一次性给全，直接产出
  `StreamEvent::RedactedThinkingBlock`（goose 同款，redacted 无 delta）。
- `content_block_delta`：`thinking_delta` 追加 text、`signature_delta` 追加
  signature（现有 `Some("text_delta")` / `Some("input_json_delta")` 分支后补两臂）。
- `content_block_stop`：该 index 是 thinking 块（累积器 Some）→ text 或
  signature 任一非空即产出 `StreamEvent::ThinkingBlock`，清空累积器。
  比 goose 的"仅 text 非空"宽一档：空 text + 有 signature 的块也必须能回传。
- `finalize` 断流收尾：未停止的 thinking 块同样补产出（与未停止 tool 块的补
  flush 并列，先 thinking 后 tools——按 Anthropic 流内实际顺序，thinking 在
  tool 块之前开始，这里按"先清累积器再清 tools map"实现即可）。
- 模块头注释、`EVENT_CONTENT_BLOCK_START` 分支里"v1 不进消息模型，静默跳过"
  的过期注释同步更新。

### 4. agent loop（`src/agent/mod.rs`、`src/config.rs`）

- `Config` 增 `thinking_budget: Option<u32>`（`#[serde(default)]` 缺省 None；
  yaml 字段名 `thinking_budget`，与 `max_tokens` / `compaction_threshold` 同层
  同风格）。不加环境变量覆盖（现有覆盖只有 PROVIDER/MODEL/MODE，保持最小面）。
- `AgentCfg` 增 `thinking_budget`，`assemble` 从 Config 搬运；
  `stream_assistant` 构造 `Request` 时填 `thinking: self.cfg.thinking_budget`。
- `stream_assistant` 折叠循环补两臂：`ThinkingBlock` / `RedactedThinkingBlock`
  到达时先 `flush_text`（与 `ToolUseStart` 同款，保证 Text 与 Thinking 块边界
  正确），再 push 对应 `Content`。thinking 块在流里先于 text / tool_use 到达，
  折叠顺序自然正确。
- 取消语义不变：流中途取消时未完成的 thinking 块（累积器未 flush）随
  `cancelled` 分支丢弃，与未完成 tool 块一致。

### 5. compact 与 openai engine

- `compact::format_history`：thinking / redacted 块**直接丢弃**（不进摘要文本）。
  理由：① signature 对摘要器无意义，thinking 正文是推理过程不是事实记录；
  ② thinking 预算可达上万 token，灌进摘要请求会把摘要器自身撑爆（现有 >2KB
  截断只护 ToolResult）；③ 压缩后历史整体替换为一条 summary user 消息，
  旧 thinking 块的签名回传义务随之消失，丢弃零风险。
- `openai::format_messages`：assistant / user 两个分支的 match 补
  `Content::Thinking { .. } | Content::RedactedThinking { .. } => {}` 跳过。

### 回传规则（汇总）

| 场景 | 行为 |
|---|---|
| anthropic engine，本轮开 thinking | 历史中的 `Thinking`（signature 非空）/ `RedactedThinking` 原样、按原序回传 |
| anthropic engine，本轮未开 thinking | 全部丢弃（API 拒绝无配置回传） |
| openai / proxy engine | 序列化阶段静默跳过 |
| 压缩（`format_history`） | 丢弃（不进摘要文本） |
| 会话持久化 | 原样落盘、原样读回，`signature` 不做任何处理 |

### 会话兼容

- 旧 JSONL（无 thinking 对象）→ 新代码：读取不受影响。
- 新 JSONL（含 thinking 对象）→ 旧二进制：`untagged` 反序列化失败、会话打不开。
  接受（与 goose 同款前向不兼容，只在回退旧版本时发生，写进风险节）。
- `Message::assistant` / `finish_turn` 不改：只含 thinking 块的 assistant 消息
  合法（`content` 非空），不变量 2 只约束 `ToolUse`。

## 拆解

依赖：任务 1、2 无依赖可并行（2 只动 `Request` / `StreamEvent` 类型，不引用
`Content`）；任务 3 依赖 1、2；任务 4 依赖 2（事件变体）；任务 5 依赖 1、2、3、4
全部（折叠循环消费 3、4 的产出）。行数含测试。

1. **消息模型**（`src/message.rs`，约 50 行）：新增两个变体 + 更新文档注释；
   测试：JSONL 往返（长签名串字节级不变）、含 thinking 块的会话过 `validate`。
   无依赖。
2. **请求/事件类型**（`src/provider/mod.rs`、`src/provider/shared.rs` testutil，
   约 25 行）：`Request.thinking`、`StreamEvent` 两变体、`event_to_json`、
   `testutil::request` 补 `thinking: None`。无依赖（与 1 可并行）。
3. **anthropic engine 请求侧**（`src/provider/anthropic.rs`，约 70 行）：
   `build_request_body` thinking 参数 + clamp + temperature 省略；
   `format_messages` 回传/丢弃分支。测试：开关 × 块类型 × 空签名 矩阵、
   clamp 边界（预算 > / < `max_tokens - 1024`）。依赖 1、2。
4. **anthropic engine 流式侧**（`src/provider/anthropic.rs` +
   `tests/fixtures/anthropic/*`，约 90 行）：`AnthropicStreamState` 累积器、
   三个事件分支、`finalize` 补产出；改写 `thinking_text_tool.expected.json`
   （从"跳过"变"产出"）、新增含 `redacted_thinking` 的 fixture 对、新增
   "空 text + 有 signature" 用例。依赖 2。
5. **loop / 配置 / 其余引擎**（`src/agent/mod.rs`、`src/agent/compact.rs`、
   `src/config.rs`、`src/provider/openai.rs`，约 90 行）：配置项、`AgentCfg`、
   `stream_assistant` 折叠、`format_history` 丢弃、openai 跳过。测试：
   MockProvider 脚本 thinking 事件 → 落盘消息含 `Content::Thinking` →
   下一轮 `provider.seen()` 里原样带回（`16` 的既有约定）；`format_history`
   丢弃断言；config 默认关（`Default` 为 None）。依赖 1、2、3、4。

合计约 325 行（实现约 150 + 测试约 175），与原表 250 行估算同量级
（原表未计测试）。

## 校验

三条基准命令（AGENTS.md）：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

anthropic engine 在 feature 后，CI（`.github/workflows/ci.yml`）已固定跑两步：

```bash
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```

真实 API 测试不可行，全部按 `12` 的既有约定 mock：SSE fixture 对
（`tests/fixtures/anthropic/{name}.sse` + `.expected.json`，
`tu::fixture_pair` / `run_sse` 驱动）+ wiremock。各组测试验收方式：

- **任务 1**（默认 `cargo test` 即跑）：`content_jsonl_round_trip` 扩展含
  两个新变体，断言 `to_string → from_str` 字节级相等；`validate` 接受含
  thinking 块的合法会话。
- **任务 3**（feature 后）：`build_request_body` 单测断言
  `body["thinking"]["budget_tokens"]` 的 clamp 值、`temperature` 缺省、
  未开时 `thinking` 键不存在；`format_messages` 断言开/关两种请求下块的
  有无与逐字段相等（尤其 `signature` 原样）。
- **任务 4**（feature 后）：`fixture_thinking_blocks_skipped_text_then_tool`
  改名并更新期望为产出 `thinking_block` 事件；新 fixture 覆盖
  `redacted_thinking`（start 一次性给全）与"空 text + 有 signature"；
  既有 `parallel_tool_calls` / `cache_tokens` 等 fixture 期望不变（回归基线）。
- **任务 5**（默认跑）：`agent/mod.rs` 测试用 MockProvider 脚本
  `StreamEvent::ThinkingBlock`，断言 `session.messages` 含
  `Content::Thinking`、JSONL 落盘行含原样 signature、第二次请求
  `provider.seen()` 带回该块；`large_usage_triggers_compaction_next_turn`
  等既有压缩测试原样通过（格式历史丢弃不破坏现有断言）。
- **回归底线**：`thinking_budget` 缺省 `None` 时，全部既有测试（含未改动的
  fixture 期望）逐字通过——这是"不开时行为与今天一致"的验收。

## 风险与假设

- **假设**：测试环境无真实 Anthropic API，行为正确性以 goose 同款样本
  （`4ad43df` 的流式测试段 :2290~2548）与官方协议约束为准；fixture 是自官方
  事件形状构造的，不保证覆盖所有网关变体（与 `12` 相同的风险面）。
- **风险（换模型签名失配）**：goose 用 `thinking_block_is_stale` 按逐消息模型
  元数据丢弃过期 thinking 块；instagent 消息无模型字段，若用户用不同模型恢复
  带 thinking 块的旧会话，API 会以签名校验失败拒绝。v1 接受：报错经现有
  `map_stream_error` → `Transport` 上抛，用户可新开会话；真被咬到再补
  "会话头模型 ≠ 当前模型时不回传"的防御。
- **风险（旧二进制读新会话）**：含 thinking 对象的 JSONL 在旧版本打不开
  （`untagged` 解析失败）。仅发生在版本回退，接受并在此记录。
- **假设**：`budget_tokens` 下限 1024、`max_tokens` 余量 1024 直接沿用 goose
  `MIN_ANSWER_TOKENS`；预算低于下限静默不开 thinking，不报错（配置失误不该
  炸会话）。
- **风险（摘要质量）**：`format_history` 丢弃 thinking 正文意味着摘要器看不到
  推理过程，只看得到结论与工具往返。判断可接受：摘要的目的是延续会话事实，
  不是延续推理；若后续发现摘要丢失关键中间结论，再考虑带截断地纳入。
- **假设**：interleaved thinking（工具调用间多段 thinking）由 `Content` 向量
  保序天然支持；本方案的流式累积器假设任意时刻至多一个 thinking 块在途
  （goose 同款，Anthropic 流现状如此）。
