# 从零写一个最小 agent 的计划（goose 只当参考）

> **历史文档，不是当前契约。** 本文是 2026-09-02 的设计计划；实现以
> `README.md`、`docs/usage.md`、`docs/architecture.md` 与 `docs/adr/` 为准。
> 与实现不一致之处：anthropic 引擎（§2.3 等）已被 ADR 0001 移除，当前引擎
> 只有 openai / proxy；§0 的工具审批（auto / approve / chat + 白名单）已被
> ADR 0002 否弃；配套文档 `goose-core-cherry-pick-plan.md` 未随仓库保留。
> 命名映射：`mini-agent` → `instagent`、`MINI_AGENT_*` → `INSTAGENT_*`
> （全表见 README"命名对照"）。loop、消息、会话、压缩、CLI 的多数设计
> （§2.2、2.5、2.6、2.7、2.11）仍被沿用。

- 日期：2026-09-02
- 参考基线：`~/yyds/goose` commit `4ad43df`（下文所有 goose 文件路径和行号都以此为准）
- 配套文档：`goose-core-cherry-pick-plan.md`（抽取方案），两份计划的功能范围一致，便于对比
- 工程暂名 `mini-agent`，可改

---

## 0. 先回答：重头做会不会更简单

会，而且简单得多，但**不是更快**。

| | cherry-pick（抽取） | 从零写 |
|---|---|---|
| 工期（一个人） | 8~10 天 | 8~10 天 |
| 结果代码量 | 约 6.5 万行（4.2 万是原样搬的 GDK crate） | 约 7~8 千行，含测试 |
| 依赖包数 | 约 500 | 约 150 |
| 你对每一行的理解 | 搬来的 4.2 万行只能当黑盒 | 全部是自己写的 |
| provider 覆盖 | anthropic / openai / ollama / google / databricks / azure + 45 个 openai 兼容，都是现成的 | anthropic + openai 兼容（覆盖 ollama / groq / deepseek / openrouter 等） |
| 边角健壮性 | 高：thinking 块、prompt cache、各家 usage 字段、Windows cmd、MCP 各种 server 的怪癖，都被上游踩过 | 低：这些坑要自己重新踩一遍 |
| 后续接桌面端 / ACP | 加 `acp/` 目录即可（2.3 万行，且要 goose-sdk-types） | 要自己设计一套事件协议 |
| 跟上游同步 | 四个 GDK crate 可整目录覆盖 | 无关 |
| 主要风险 | 21 个枢纽文件的外科手术做错了不容易发现 | 在 provider 流式协议和会话不变量上重新踩坑 |
| 工期花在哪 | 读懂别人的代码再删 | 设计 + 写 + 调 API 边角 |

结论：
- 目标是"一个完全掌控、随手能改的小 agent"，从零写，用本计划。
- 目标是"尽快有个健壮的 goose 精简版，以后可能接桌面端"，用抽取方案。
- goose 真正难以替代的只有两层：**provider 的流式协议实现**和 **MCP 客户端的边角处理**。
  本计划在这两层上采取"自己写最小实现 + 把 goose 的实现当标准答案做对照测试"，其余
  各层（loop、会话、工具、审批、CLI）goose 明显过度设计，自己写反而清楚。

---

## 1. 范围

与抽取计划相同的 7 项能力：

1. agent loop：LLM → 工具 → 循环，可取消，有最大轮数
2. provider：anthropic、openai 兼容（一套代码服务 openai / ollama / groq / deepseek / openrouter）
3. 内置工具：shell、read、write、edit、tree
4. MCP client：stdio 和 streamable-http 两种 server
5. 会话持久化 + 自动压缩
6. 工具审批：auto / approve / chat 三种模式 + 持久化白名单
7. 终端 CLI：交互式和一次性两种

v1 明确不做：图片、thinking 块、并行执行工具、MCP 的 prompts / resources / sampling / elicitation、
会话自动命名、markdown 渲染、Windows（先只保证 bash / zsh）、多 agent、recipe、hooks。

---

## 2. 设计

### 2.1 架构

```
CLI (rustyline REPL) ◀──Event mpsc──┐
   │ 用户输入                        │
   ▼                                 │
Agent::run_turn(session, text, cancel, events)     ← 一个普通的 async 循环，不做状态机
   ├── prompt::system(tools, cwd, now)             ← format! 拼字符串，不用模板引擎
   ├── provider.stream(req) → StreamEvent          ← anthropic.rs / openai.rs
   ├── approval.decide(call) → Allow | Deny        ← 模式 + 白名单 + Confirm 回调（CLI 里是问一句）
   ├── tools.call(call) → ToolOutput               ← 内置 5 个 + MCP 适配
   ├── session.append(msg)                         ← JSONL，每条消息立即落盘
   └── compact::maybe(session)                     ← 上一轮 input_tokens > 0.8 × limit 时摘要
```

goose 的状态机（`goose-agent` + `state_machine/` 共 1.25 万行）解决的是"操作可插拔、每一步都从持久化
状态重入"。我们不需要可插拔，重入用"每条消息落盘 + 打断时补 tool result"就够了（见 2.5）。

### 2.2 消息模型（最小）

```rust
pub enum Role { User, Assistant }

pub enum Content {
    Text(String),
    ToolUse   { id: String, name: String, input: serde_json::Value },
    ToolResult{ tool_use_id: String, content: String, is_error: bool },
}

pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
    pub ts: i64,
    pub usage: Option<Usage>,      // 只有 assistant 消息有
}

pub struct Usage { pub input: u32, pub output: u32, pub cache_read: u32, pub cache_write: u32 }
```

- 没有 system 角色，system 是请求字段。没有 Image / Thinking。
- 压缩产生的摘要是一条 User 消息，正文以 `# Conversation Summary` 开头（goose 也是摘要成 user 消息，
  见 `goose-context-management/src/summarize.rs:120`）。
- 会话不变量（goose 在 `goose-provider-types/src/conversation.rs:235 fix_conversation` 里修的那些，
  我们靠构造时就保证）：
  1. user / assistant 严格交替
  2. assistant 的每个 `ToolUse` 在紧接着的 user 消息里必须有同 id 的 `ToolResult`
  3. 不发空 content 的消息
  4. 被打断 / 出错的 tool call 补 `is_error = true` 的 ToolResult，文本 "interrupted by user"
     （goose-cli `session/mod.rs:1843 handle_interrupted_messages` 就是干这个）

### 2.3 Provider 层

```rust
pub struct Request<'a> {
    pub model: &'a str, pub system: &'a str, pub messages: &'a [Message],
    pub tools: &'a [ToolSpec], pub max_tokens: u32, pub temperature: Option<f32>,
}

pub enum StreamEvent {
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseDelta(String),                 // JSON 片段，累积到 End 再 parse
    ToolUseEnd,
    Done { usage: Usage, stop_reason: StopReason },
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn stream(&self, req: Request<'_>) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}

pub enum ProviderError { RateLimited { retry_after: Option<Duration> }, ContextOverflow, Auth, Http(u16, String), Transport(String) }
```

**anthropic.rs**（约 450 行）
- `POST {base}/v1/messages`，`stream: true`，头 `x-api-key`、`anthropic-version: 2023-06-01`
  （goose `goose-providers/src/anthropic.rs:54`）
- SSE 事件：`message_start`(取 usage.input) → `content_block_start`(text | tool_use) →
  `content_block_delta`(text_delta | input_json_delta) → `content_block_stop` → `message_delta`(stop_reason, usage.output) → `message_stop`
- `cache_control: {type: ephemeral}` 加在 system 块和最后一个 tool spec 上
  （goose `goose-provider-types/src/formats/anthropic.rs:468` 和 `507~513`）
- 参考：`formats/anthropic.rs:230 format_messages`、`489 format_tools`、`871 response_to_streaming_message`（流式组装，含 `input_json_delta` 拼接）

**openai.rs**（约 450 行，同时服务 ollama `/v1`、groq、deepseek、openrouter，靠 `base_url` + `api_key` 区分）
- `POST {base}/chat/completions`，`stream: true`，`stream_options: {include_usage: true}`
- `delta.content` 累积文本；`delta.tool_calls[{index, id, function{name, arguments}}]` 按 `index` 累积 arguments；`finish_reason`
- 已知怪癖（goose 都处理过，照抄规则）：
  - 带 `tool_calls` 的 assistant 消息 `content` 必须是 null 或 ""，不能省略字段（`formats/openai.rs:445~454`）
  - function name 只允许 `[A-Za-z0-9_-]{1,64}`，MCP 工具名要先 sanitize（`formats/openai.rs:1918 sanitize_function_name`）
  - tool 消息必须紧跟含对应 `tool_calls` 的 assistant 消息
  - arguments 为空串时当 `{}`
- 参考：`formats/openai.rs:197 format_messages`、`646 format_tools`、`1213 response_to_streaming_message`

**http.rs**（约 250 行）
- reqwest client，超时 600s（goose DEFAULT_PROVIDER_TIMEOUT_SECS）
- SSE 解析：按空行切事件，取 `data:` 行，`[DONE]` 结束；约 80 行，不引 eventsource 库
- 重试：429 / 500 / 502 / 503 / 529 指数退避，3 次、首次 1s、×2、上限 30s
  （goose `goose-provider-types/src/retry.rs:8~11`）；`Retry-After` 头和 body 里的
  `retry_after_seconds` 优先，且封顶（`goose-providers/src/http_status.rs:47~83`）
- 400 且错误文本含 "prompt is too long" / "context_length_exceeded" / "maximum context length" → `ContextOverflow`

**上下文上限**：配置覆盖 → 按模型名前缀的小表（claude 200k、gpt-4o 128k、gpt-4.1 1M、o 系列 200k、
deepseek 128k、llama 默认 128k）→ 兜底 128k。goose 用一个 1.8K 行的 canonical 目录干这事，我们 15 行表。

### 2.4 工具层

```rust
pub struct ToolSpec { pub name: String, pub description: String, pub input_schema: Value, pub read_only: bool }
pub struct ToolOutput { pub text: String, pub is_error: bool }
pub struct ToolCtx { pub cwd: PathBuf, pub cancel: CancellationToken }

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput;
}

pub struct Registry { tools: Vec<Arc<dyn Tool>> }   // MCP 工具名 = "<server>__<tool>"，与 goose 同分隔符
```

schema 手写 JSON，不用 schemars。goose 有 1068 行的 `tool_schema_normalize.rs`，就是为了修
schemars 生成的 `oneOf/const` 被严格 provider 拒收的问题；手写就没这问题。MCP server 送来的
schema 原样透传，遇到被拒再考虑把那个文件搬过来。

**内置工具（5 个）**

| 工具 | 参数 | 行为 | goose 参考 |
|------|------|------|-----------|
| `shell` | `command`, `timeout_secs?` | `$SHELL -c` 或 `bash -c`，cwd 为会话目录，进程组，超时或取消时 kill 整组；默认超时 300s；输出截断：每流 2000 行 / 50KB，超出时给前 50 行 / 10KB 预览并把全文存到临时文件、返回路径；返回 stdout、stderr、exit code | `developer/shell.rs:158~163` 常量，`868 render_output`，`939 save_full_output`，`80 unix_login_shell_command_args`；描述文本 `developer/mod.rs:135~150` |
| `read` | `path`, `line?`, `limit?` | 带行号输出，默认最多 2000 行 | `developer/edit.rs:11 FileReadParams`（goose 定义了但没注册成工具，让模型用 shell cat；我们注册，省得模型乱用 shell） |
| `write` | `path`, `content` | 建父目录，覆盖写 | `developer/mod.rs:108` 描述 |
| `edit` | `path`, `before`, `after` | `before` 必须唯一精确匹配，否则报错并给出匹配次数和相近上下文；`after` 为空即删除 | `developer/edit.rs:157 string_replace`，`244 find_similar_context`，`259 build_file_preview`；描述 `mod.rs:121` |
| `tree` | `path`, `depth` | 目录树 + 行数，遵守 .gitignore（`ignore` crate） | `developer/tree.rs` |

`read` 和 `tree` 标 `read_only = true`。

**MCP 适配（mcp.rs，约 300 行）**
- 用 `rmcp` crate（goose 用 3.x，见根 `Cargo.toml` 的 workspace.dependencies），feature：`client`、`transport-child-process`、`transport-streamable-http-client`
- stdio：`TokioChildProcess` 起子进程，进程组 + kill_on_drop，stderr 接到日志
  （直接搬 goose `crates/goose/src/subprocess.rs:77 configure_subprocess` 和 `117 spawn_long_lived_mcp_subprocess`，共 144 行，无内部依赖）
- streamable-http：`StreamableHttpClientTransport::from_uri(url)`
- `initialize` 结果里的 `instructions` 存下来拼进 system prompt
- `list_tools` → `ToolSpec`（加前缀，`annotations.readOnlyHint` → `read_only`）
- `call_tool` → 把 `content` 里的 text 块拼接，`is_error` 直接映射；单次调用超时 300s
- 通知（progress / logging）v1 只写日志

### 2.5 Agent loop

```rust
pub async fn run_turn(&self, session: &mut Session, text: String,
                      cancel: CancellationToken, events: mpsc::Sender<Event>) -> Result<TurnResult> {
    session.append(Message::user_text(text))?;
    for _ in 0..self.cfg.max_turns {
        compact::maybe(self, session, &events).await?;
        let assistant = match self.stream_assistant(session, &events, &cancel).await {
            Ok(m) => m,
            Err(ProviderError::ContextOverflow) => { compact::force(self, session, &events).await?; continue; }
            Err(e) => return Err(e.into()),
        };
        session.append(assistant.clone())?;
        let calls = assistant.tool_uses();
        if calls.is_empty() { return Ok(TurnResult::Done); }

        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            if cancel.is_cancelled() { results.push(Content::interrupted(&call)); continue; }
            let out = match self.approval.decide(&call).await {
                Decision::Allow => self.tools.call(&call, &ctx).await,
                Decision::Deny(reason) => ToolOutput::denied(reason),
            };
            events.send(Event::ToolDone { .. }).await?;
            results.push(Content::tool_result(&call, out));
        }
        session.append(Message { role: User, content: results, .. })?;
        if cancel.is_cancelled() { return Ok(TurnResult::Interrupted); }
    }
    Ok(TurnResult::MaxTurns)
}
```

- 工具串行执行。并行是 v2。
- 取消：`stream_assistant` 和每个工具调用都 `tokio::select!` 上 cancel。流被取消时，已收到的
  ToolUse 也要按 2.2 第 4 条补错误结果，保证会话合法。
- `max_turns` 默认 1000（goose `agent.rs:85`），可配。
- chat 模式：请求里不带 tools。
- `stream_assistant` 内部：把 `StreamEvent` 折叠成一条 assistant `Message`，同时把 `TextDelta`
  转发成 `Event::TextDelta`；`ToolUseDelta` 累积到 `ToolUseEnd` 再 `serde_json::from_str`，
  失败就生成一个 `is_error` 的 ToolResult 告诉模型 JSON 坏了。

**事件**（给 UI 的，单向）

```rust
pub enum Event {
    TextDelta(String),
    ToolStart { id: String, name: String, input: Value },
    ToolDone  { id: String, preview: String, is_error: bool, elapsed_ms: u64 },
    Usage(Usage),
    Compacted { before_tokens: u32, after_tokens: u32 },
    Error(String),
}
```

审批不走事件而走回调，因为它需要答案：

```rust
#[async_trait]
pub trait Confirm: Send + Sync { async fn confirm(&self, call: &ToolCall) -> Decision; }
pub enum Decision { Allow, AllowAlways, Deny(String) }
```

goose 用会话消息（`ActionRequired`）来回传审批，是因为客户端可能在 ACP 另一端。我们 v1 只有 CLI，
回调最简单；将来要远程客户端，给 `Confirm` 写一个走消息的实现即可，loop 不用改。

### 2.6 会话：JSONL，不用 sqlite

- 路径 `~/.local/share/mini-agent/sessions/<id>.jsonl`（`etcetera` 取目录）
- 第 1 行 header：`{id, created, cwd, provider, model, name}`；之后每行一条 `Message`
- `append` = 打开追加一行 + flush
- 压缩 = 写临时文件（header + 摘要消息 + 保留的尾部）再原子 rename，旧文件改名为
  `<id>.<n>.bak.jsonl` 留着排错
- `list` = 只读每个文件第一行；`resume` = 全量读
- 约 250 行。goose 的 `session_manager.rs` 4843 行 + sqlx + schema 迁移，换来的是搜索和多客户端并发，
  我们都不需要。JSONL 还能直接 grep。

### 2.7 压缩

- 触发：每条 assistant 消息后，`usage.input >= threshold × context_limit`，threshold 默认 0.8
  （goose `goose-context-management/src/lib.rs:32`）；或用户 `/compact`；或 provider 报 ContextOverflow。
- 用上一次响应的 `usage.input` 当上下文大小，**不引 tokenizer**。goose 用 tiktoken 估算，
  但真正准的还是 API 回的数。
- 摘要请求：把历史格式化成文本，塞进 prompt。prompt 文本直接抄
  `goose-context-management/src/prompts/compaction.md`（写得很好，含 user_intent / files / errors_and_fixes /
  pending_tasks / current_work 等字段）。v1 让模型直接输出 markdown，不要 JSON + 模板那一层；
  goose 的结构化版本（JSON → `compaction_summary.md` 渲染）留作 v2。
- 摘要器自己也可能溢出：先把超过 2KB 的 ToolResult 正文替换成 `[truncated N bytes]` 再送
  （goose 是按比例逐步删 tool 结果重试，`summarize.rs:120~`；v1 一刀切）。
- 结果：整个历史替换为 `[摘要 user 消息]`，如果最后一条是没得到回答的 user 消息则保留它。
- 约 200 行。

### 2.8 审批

| 模式 | 行为 |
|------|------|
| `auto` | 全放行 |
| `approve` | 白名单里的放行，其余问用户；`read` / `tree` 默认在白名单 |
| `chat` | 不给模型工具 |

- 白名单持久化在配置文件 `always_allow: ["read", "tree", "everything__echo"]`；会话内 AllowAlways 即时写回。
- 拒绝时带原因，作为 `is_error` 的 ToolResult 回给模型（"user denied: <reason>"），模型能据此换办法。
- goose 的 `smart_approve`（`readOnlyHint` + LLM 判定是否只读，`permission/permission_judge.rs`）是 v2。
- 约 120 行。goose 这一块是 `permission/` 820 + `tool_inspection.rs` 313 + `config/permission.rs` 463。

### 2.9 系统提示

`format!` 拼四段：身份一句话、工具说明（每个 MCP server 的 `instructions`）、`cwd` 和当前时间、
响应规范（markdown、简洁）。文本从 goose `crates/goose/src/prompts/system.md` 精简。
不用 minijinja / include_dir。约 60 行。

### 2.10 配置

`~/.config/mini-agent/config.yaml`：

```yaml
provider: anthropic            # anthropic | openai
model: claude-sonnet-4-5
base_url: null                 # openai 兼容服务在这里改，如 http://localhost:11434/v1
api_key_env: ANTHROPIC_API_KEY # 优先读环境变量；也允许 api_key: 直接写（文件 0600）
max_tokens: 8192
mode: approve                  # auto | approve | chat
max_turns: 1000
context_limit: null            # 覆盖模型表
compaction_threshold: 0.8
shell: null                    # 默认 $SHELL
always_allow: [read, tree]
mcp:
  - name: everything
    type: stdio                # stdio | http
    cmd: npx
    args: [-y, "@modelcontextprotocol/server-everything"]
    env: {}
    timeout_secs: 300
```

环境变量覆盖：`MINI_AGENT_PROVIDER`、`MINI_AGENT_MODEL`、`MINI_AGENT_MODE`、`ANTHROPIC_API_KEY`、
`OPENAI_API_KEY`、`OPENAI_BASE_URL`。不接系统 keyring。约 150 行。

### 2.11 CLI

- `mini-agent chat [--resume <id>|last] [--cwd DIR] [-m MODEL] [--mode MODE] [--mcp "name=cmd arg..."]`
- `mini-agent run -t "..." [同上]`：无交互，工具审批按 auto，结束打印最终回复和 usage
- `mini-agent sessions list | rm <id>`
- REPL：rustyline；斜杠命令 `/exit` `/clear` `/compact` `/mode <m>` `/tools` `/help`
- Ctrl-C：第一次取消当前轮（cancel token），第二次退出
- 渲染：文本流式直接打印；工具调用打一行 `▶ shell  ls -la`，完成后打前 10 行预览和耗时；
  每轮末尾打 usage。markdown 渲染不做（要做的话 `termimad`，别用 goose 的 `bat`，那是整个 syntect）
- 审批提示：`allow this call? [y]es / [a]lways / [n]o: `
- 约 900 行。goose-cli 的 `session/` 目录 1.17 万行。

### 2.12 日志与错误

- `tracing` + `tracing-subscriber` 写到 `~/.local/share/mini-agent/logs/`，每次请求记 model、
  input/output tokens、耗时；工具调用记名字和耗时
- `anyhow::Result` 到顶层；`ProviderError` 单独枚举，loop 里只匹配 `ContextOverflow` 和 `RateLimited`
- 子进程一律 `kill_on_drop(true)` + 进程组，防止 Ctrl-C 后残留

---

## 3. 模块与行数估算

```
mini-agent/
├── Cargo.toml
└── src/
    ├── main.rs              40    clap 入口
    ├── cli/
    │   ├── repl.rs         350    rustyline 循环、斜杠命令、Ctrl-C
    │   ├── render.rs       250    事件渲染、审批提示
    │   └── commands.rs     150    run / sessions
    ├── config.rs           150
    ├── message.rs          150    Role / Content / Message / Usage + 不变量检查
    ├── provider/
    │   ├── mod.rs          120    trait、Request、StreamEvent、ProviderError、上下文上限表
    │   ├── http.rs         250    client、SSE、重试
    │   ├── anthropic.rs    450
    │   └── openai.rs       450
    ├── tools/
    │   ├── mod.rs          150    Tool trait、Registry、前缀
    │   ├── shell.rs        350
    │   ├── fs.rs           250    read / write / edit
    │   ├── tree.rs         120
    │   └── mcp.rs          300
    ├── agent/
    │   ├── mod.rs          300    Agent、run_turn、stream_assistant
    │   ├── approval.rs     120
    │   ├── compact.rs      200
    │   ├── prompt.rs        60
    │   └── event.rs         40
    ├── session.rs          250    JSONL
    └── subprocess.rs       150    从 goose 搬
                          ─────
                          ~4700  + 测试约 1500 ≈ 6.2K
```

依赖：tokio、tokio-util、futures、async-trait、reqwest(rustls, json, stream)、serde、serde_json、
serde_yaml、rmcp、clap、rustyline、anyhow、thiserror、tracing、tracing-subscriber、tracing-appender、
etcetera、ignore、chrono、uuid、shellexpand。测试用 wiremock。约 20 个直接依赖。

---

## 4. 从 goose 直接拿的东西

按"文本 / 代码 / 只当参考"分三类。

**直接抄的文本**
- `crates/goose/src/prompts/system.md`：系统提示骨架
- `crates/goose-context-management/src/prompts/compaction.md`：压缩摘要 prompt
- `crates/goose/src/agents/platform_extensions/developer/mod.rs:108~186`：5 个工具的 description

**直接搬的代码（自包含，改改 use 就能用）**
- `crates/goose/src/subprocess.rs`（144 行）：`configure_subprocess`、`spawn_long_lived_mcp_subprocess`
- `developer/shell.rs`：`render_output` / `truncate_output` / `truncate_preview_bytes` / `save_full_output`
  这一组（约 150 行）和 158~163 行的常量
- `developer/edit.rs:157~286`：`string_replace`、`find_similar_context`、`build_file_preview`
- `goose-providers/src/http_status.rs:47~83`：`extract_retry_after` / `parse_retry_after_header`
- 可选：`crates/goose/src/agents/tool_schema_normalize.rs`（1068 行含测试），等真遇到 schema 被拒再搬

**只当参考对照的实现**
| 我们的模块 | goose 参考 | 看什么 |
|-----------|-----------|--------|
| provider/anthropic.rs | `goose-provider-types/src/formats/anthropic.rs:230, 489, 520, 871`；`goose-providers/src/anthropic.rs:166 stream_for_model` | 消息 → 请求体；流式事件组装；cache_control 放哪 |
| provider/openai.rs | `formats/openai.rs:197, 646, 688, 1213, 1918` | tool_calls 按 index 累积；content null 规则；函数名 sanitize |
| provider/http.rs | `goose-providers/src/api_client.rs`、`http_status.rs:240 map_http_error_to_provider_error`、`goose-provider-types/src/retry.rs` | 状态码 → 错误类型；退避参数 |
| message.rs 不变量 | `goose-provider-types/src/conversation.rs:235 fix_conversation`、`516 merge_consecutive_messages` | 哪些坏序列会被 API 拒 |
| agent/compact.rs | `goose-context-management/src/summarize.rs:120~`、`crates/goose/src/context_mgmt/mod.rs` | 摘要器溢出时的退让策略；触发条件 |
| agent/approval.rs | `crates/goose/src/permission/permission_inspector.rs:150~200` | 各模式的判定表 |
| tools/mcp.rs | `crates/goose/src/agents/mcp_client.rs:690~720`、`extension_manager.rs:451, 797` | rmcp 的 serve / transport 用法 |
| cli/repl.rs 打断处理 | `crates/goose-cli/src/session/mod.rs:1843 handle_interrupted_messages` | 打断后怎么补消息 |

对照测试的做法：把 goose 的 `formats/anthropic.rs` 和 `formats/openai.rs` 测试段（分别从 1227 行和
1933 行开始）里的 SSE 样本和期望结果抄成我们的 fixture，让我们的解析器输出和 goose 一致。
这是本计划里最省事的"借力"点。

---

## 5. 阶段与验证

每个阶段以 `cargo test` 通过为完成标准，写代码的同时写测试，不等最后。

| 阶段 | 内容 | 验证 | 工期 |
|------|------|------|------|
| P0 骨架 | Cargo、config、message、event、session(JSONL) | config 读写 + env 覆盖；session 追加 / 读回 / 原子重写（tempdir） | 0.5 天 |
| P1 provider | http.rs + anthropic.rs | SSE 解析器喂 goose 测试段里的样本；wiremock 起假服务测重试和 429 Retry-After；真 key 跑一次 `examples/ask.rs` | 1 天 |
| P1b | openai.rs | 同上；另用 ollama 本地跑一次确认 openai 兼容路径 | 1 天 |
| P2 工具 | shell / read / write / edit / tree | shell：超时 kill、取消 kill、输出截断落盘、非零退出码；edit：唯一匹配 / 多匹配报错 / 删除 | 1 天 |
| P2b MCP | mcp.rs | 起 `npx -y @modelcontextprotocol/server-everything`，list_tools 有前缀，call `everything__echo`；server 被 kill 后错误可读且不挂 | 1 天 |
| P3 loop | agent/mod.rs、approval、prompt | 用脚本化的 mock Provider 走：文本 → tool call → 结果 → 文本；approve 模式下 Confirm 回调被调用且 Deny 变成 is_error 结果；中途 cancel 后会话满足 2.2 的不变量 | 1.5 天 |
| P4 压缩 | compact.rs | mock provider 回一个大 usage.input 触发压缩；ContextOverflow 错误触发强制压缩并重试一次 | 0.5 天 |
| P5 CLI | repl / render / commands | 手工：chat 多轮、Ctrl-C 一次取消两次退出、`/compact`、`/mode approve` 后审批提示、`--resume last`、`run -t` | 1.5 天 |
| P6 加固 | clippy -D warnings、错误路径、README、CI(fmt+clippy+test) | 断网 / 错 key / 模型名错 / MCP 命令不存在，四种情况提示可读、进程不残留 | 1 天 |

合计约 9 天。P1 和 P2 互不依赖，可以并行或交错。

---

## 6. 风险与对策

1. **流式 tool call 组装**：OpenAI 多个并行 tool_calls 按 index 各自累积；Anthropic 的 `input_json_delta`
   必须全部拼完才能 parse；arguments 空串当 `{}`。对策：P1 的 fixture 里专门放这三种样本。
2. **会话不变量**：打断、工具 panic、provider 中途断流都会留下没有 result 的 tool_use，下一次请求被 API 拒。
   对策：所有退出路径统一走 `finish_turn()` 补结果；`message.rs` 里放一个 `validate()`，测试里每次
   append 后都跑。
3. **子进程残留**：shell 超时、MCP server 卡住、Ctrl-C。对策：进程组 + `kill_on_drop`，P2 有专门测试。
4. **上下文溢出错误各家文案不同**：Anthropic 400 "prompt is too long"，OpenAI 400 `context_length_exceeded`，
   ollama 可能直接截断不报错。对策：错误文本匹配放在一个函数里，加样本测试；ollama 靠阈值提前压缩。
5. **工具输出撑爆上下文**：这是必须项不是优化项，截断策略照抄 goose 的 2000 行 / 50KB。
6. **Windows**：v1 不承诺。goose 在 `shell.rs` 里为 cmd / powershell / nushell 写了大量特判，
   要支持时再去对照。

---

## 7. v2 候选（每项独立）

| 功能 | 参考 goose | 估算 |
|------|-----------|------|
| smart_approve（只读标注 + LLM 判定） | `permission/permission_judge.rs`、`prompts/permission_judge.md` | 200 行 |
| 结构化摘要（JSON + 模板） | `goose-context-management` 整个 crate 1.2K | 300 行 |
| 图片：read_image 工具 + Content::Image | `developer/image.rs`、formats 里的 image 处理 | 300 行 |
| thinking 块（Anthropic extended thinking 要回传 signature） | `formats/anthropic.rs:106~146`、`thinking.rs` | 250 行 |
| 并行执行工具 | goose `ops_toolcalling.rs` | 100 行 |
| MCP elicitation / sampling | `mcp_client.rs:403~520` | 400 行 |
| `.agenthints` 项目提示文件 | `crates/goose/src/hints/` 2.5K（我们只要读文件拼进 prompt，50 行） | 50 行 |
| 远程客户端 / ACP | goose `acp/` 2.35 万行 | 另立计划 |
| 直接换用 goose-providers 做 provider 层 | `goose-provider-types` + `goose-providers` 4.2 万行 | 写一个 Message 双向转换适配器约 150 行，其余零工作量；代价是依赖从 150 涨到 350 左右 |
