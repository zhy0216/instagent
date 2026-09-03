//! 原生 Anthropic Messages API 引擎（第二版 §2.3 anthropic.rs；第三版 §2.4），
//! 整个模块挂在 cargo feature `anthropic-engine` 后（`12`）。
//!
//! `POST {base_url}/v1/messages`，`stream: true`，头 `x-api-key` +
//! `anthropic-version: 2023-06-01`（goose `goose-providers/src/anthropic.rs:54`）。
//! HTTP / SSE / 重试复用 [`crate::provider::http`]。
//!
//! SSE 事件链：`message_start`（取 usage.input 与 cache 计数）→
//! `content_block_start`（text | tool_use；其余块类型 v1 不进消息模型，忽略）→
//! `content_block_delta`（text_delta 即时透传；input_json_delta 按 index 累积）→
//! `content_block_stop`（该 index 拼完后一次性 parse 并 flush Start→Delta→End）→
//! `message_delta`（stop_reason、usage.output，字段级合并进 start 值）→
//! `message_stop`（发 Done）。并行 tool_use 的 delta 会按 index 交错到达，
//! 块与块互不交错（goose `test_streaming_reassembles_interleaved_parallel_tool_calls`
//! 同款样本），故按块在各自 stop 时 flush。
//!
//! prompt caching（第三版 §7 风险 3 的动机）：`cache_control: {type: ephemeral}`
//! 打在 system 块与最后一个 tool spec 上（goose `formats/anthropic.rs:468/507~513`），
//! 工具定义整段成为可缓存前缀。参考重写自 goose
//! `crates/goose-provider-types/src/formats/anthropic.rs` 的 `format_messages`（:230）、
//! `format_tools`（:489）、`response_to_streaming_message`（:871），只读、去 thinking。
//!
//! usage 口径：Anthropic 的 `input_tokens` 不含 cache token，这里按 goose 的
//! cache-exclusive input 语义折入 `Usage.input`，让压缩阈值
//! （`src/agent/compact.rs` 读 `usage.input`）看到完整上下文。

use std::collections::BTreeMap;
use std::collections::VecDeque;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::json;
use serde_json::Value;

use crate::error::ProviderError;
use crate::message::Content;
use crate::message::Message;
use crate::message::Role;
use crate::message::Usage;
use crate::provider::http::HttpClient;
use crate::provider::http::RetryPolicy;
use crate::provider::http::SseEvent;
use crate::provider::shared::arguments_or_empty;
use crate::provider::shared::engine_parts;
use crate::provider::shared::headers_with_auth;
use crate::provider::shared::parse_chunk;
use crate::provider::shared::require_base_url;
use crate::provider::shared::sanitize_function_name;
use crate::provider::shared::sanitized_tool_names;
use crate::provider::shared::to_provider_error;
use crate::provider::shared::PendingCall;
use crate::provider::shared::StreamEngine;
use crate::provider::shared::StreamState;
use crate::provider::EngineKind;
use crate::provider::Provider;
use crate::provider::ProviderDef;
use crate::provider::Request;
use crate::provider::StopReason;
use crate::provider::StreamEvent;
use crate::tools::ToolSpec;

/// goose `ANTHROPIC_API_VERSION`（goose-providers/src/anthropic.rs:54）。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

// SSE 事件类型（goose formats/anthropic.rs:205~210 的常量集，去掉 refusal 特判）。
const EVENT_MESSAGE_START: &str = "message_start";
const EVENT_MESSAGE_DELTA: &str = "message_delta";
const EVENT_MESSAGE_STOP: &str = "message_stop";
const EVENT_CONTENT_BLOCK_START: &str = "content_block_start";
const EVENT_CONTENT_BLOCK_DELTA: &str = "content_block_delta";
const EVENT_CONTENT_BLOCK_STOP: &str = "content_block_stop";
const EVENT_ERROR: &str = "error";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    pub def: ProviderDef,
    /// 从 `def.api_key_env` 指定的环境变量读取；无 `api_key_env` 时为空串
    /// （裸网关 / 本地 proxy 场景，此时不发 `x-api-key` 头）。
    pub api_key: String,
    /// HTTP / SSE / 重试层；测试可注入小退避的 [`RetryPolicy`]。
    pub http: HttpClient,
}

impl AnthropicProvider {
    /// 校验 def（engine=anthropic、base_url 必填），按 `api_key_env` 读密钥，
    /// 按 `timeout_seconds` 建 client（构造骨架在共享层 [`engine_parts`]）。
    pub fn new(def: &ProviderDef) -> crate::Result<Self> {
        let (api_key, http) = engine_parts(def, EngineKind::Anthropic)?;
        Ok(Self {
            def: def.clone(),
            api_key,
            http,
        })
    }

    /// 测试 / registry 可换退避策略而不换 def（形状同 openai 引擎）。
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.http = self.http.clone().with_retry(retry);
        self
    }

    /// def 自带 headers + `x-api-key` + `anthropic-version`（密钥为空则不发前者）。
    fn request_headers(&self) -> BTreeMap<String, String> {
        let mut auth = vec![(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        )];
        if !self.api_key.is_empty() {
            auth.push(("x-api-key".to_string(), self.api_key.clone()));
        }
        headers_with_auth(&self.def, auth)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.def.name
    }

    async fn stream(
        &self,
        req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let base_url = require_base_url(&self.def)?;
        let url = messages_url(base_url);
        let body = build_request_body(&req).map_err(to_provider_error)?;
        let sse = self
            .http
            .post_sse(&url, &self.request_headers(), &body)
            .await
            .map_err(to_provider_error)?;
        Ok(sse_to_stream_events(sse))
    }
}

/// 约定 anthropic engine 的 `base_url` 写到 host 根（`https://api.anthropic.com`）；
/// 已写成 `/v1` 的也不双拼前缀。
fn messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

// ---------------------------------------------------------------------------
// 请求侧：system / 消息 / 工具格式化
// ---------------------------------------------------------------------------

fn cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

fn build_request_body(req: &Request<'_>) -> crate::Result<Value> {
    let mut body = json!({
        "model": req.model,
        "messages": format_messages(req.messages),
        "max_tokens": req.max_tokens,
        "stream": true,
    });
    if !req.system.is_empty() {
        body["system"] = format_system(req.system);
    }
    let tools = format_tools(req.tools)?;
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }
    Ok(body)
}

/// system → 文本块数组，带 ephemeral 缓存断点（goose `format_system`，:520~532）。
fn format_system(system: &str) -> Value {
    json!([{ "type": "text", "text": system, "cache_control": cache_control() }])
}

/// [`Message`] 列表 → Anthropic `messages` 数组（goose `format_messages` :230 的
/// 无 thinking / 无 image 重写；工具名映射在 assistant 的 tool_use 块里原样回放，
/// 与请求侧 format_tools 的 sanitize 一致靠同一函数保证）。
fn format_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut content: Vec<Value> = Vec::new();
        for block in &message.content {
            match block {
                Content::Text(text) => {
                    // 纯空白文本块不发送（goose :252 同款，API 会拒空文本块）。
                    if !text.trim().is_empty() {
                        content.push(json!({ "type": "text", "text": text }));
                    }
                }
                Content::ToolUse { id, name, input } => {
                    // tool_use.input 必须是对象（goose args_to_input_value :225，
                    // null 会被 API 以 400 拒绝）。
                    let input = if input.is_null() {
                        json!({})
                    } else {
                        input.clone()
                    };
                    content.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": sanitize_function_name(name),
                        "input": input,
                    }));
                }
                Content::ToolResult {
                    tool_use_id,
                    content: text,
                    is_error,
                } => {
                    let mut result = json!({ "type": "tool_result", "tool_use_id": tool_use_id, "content": text });
                    // is_error 缺省即 false，只在错误时发（goose :380~386 形状）。
                    if *is_error {
                        result["is_error"] = json!(true);
                    }
                    content.push(result);
                }
            }
        }
        // 不变量 3 之外再兜一层：全空白文本的消息不发（goose :434 同款）。
        if !content.is_empty() {
            out.push(json!({ "role": role, "content": content }));
        }
    }
    out
}

/// [`ToolSpec`] → `{name, description, input_schema}`；sanitize + 重名报错在
/// 共享层 [`sanitized_tool_names`]（`[A-Za-z0-9_-]{1,64}`，Anthropic 字符集相同），
/// 最后一个 spec 打 cache_control（goose `format_tools` :489~517）。
fn format_tools(tools: &[ToolSpec]) -> crate::Result<Vec<Value>> {
    let names = sanitized_tool_names(tools)?;
    let mut out: Vec<Value> = tools
        .iter()
        .zip(names)
        .map(|(tool, name)| {
            json!({
                "name": name,
                "description": tool.description,
                "input_schema": anthropic_flavored_input_schema(&tool.input_schema),
            })
        })
        .collect();
    if let Some(last) = out.last_mut() {
        // 工具定义整段缓存为单一前缀（goose :507~513）。
        last.as_object_mut()
            .expect("built as object")
            .insert("cache_control".to_string(), cache_control());
    }
    Ok(out)
}

/// 空 schema 补成 `{"type":"object"}`（goose `anthropic_flavored_input_schema` :479）。
fn anthropic_flavored_input_schema(schema: &Value) -> Value {
    if schema.is_null() || schema.as_object().is_some_and(|m| m.is_empty()) {
        return json!({ "type": "object" });
    }
    schema.clone()
}

// ---------------------------------------------------------------------------
// 响应侧：SSE 事件流 → StreamEvent
// ---------------------------------------------------------------------------

/// 流内 usage 累加器：message_start 给 input/cache，message_delta 给 output，
/// 字段级合并（delta 上报的字段赢，goose `merge_delta_usage` 语义）。
#[derive(Debug, Default)]
struct UsageAccum {
    input: Option<u32>,
    output: Option<u32>,
    cache_read: Option<u32>,
    cache_write: Option<u32>,
}

impl UsageAccum {
    fn merge(&mut self, usage: &Value) {
        let field = |key: &str| usage.get(key).and_then(Value::as_u64).map(|v| v as u32);
        if let Some(v) = field("input_tokens") {
            self.input = Some(v);
        }
        if let Some(v) = field("output_tokens") {
            self.output = Some(v);
        }
        if let Some(v) = field("cache_read_input_tokens") {
            self.cache_read = Some(v);
        }
        if let Some(v) = field("cache_creation_input_tokens") {
            self.cache_write = Some(v);
        }
    }

    fn usage(&self) -> Usage {
        let cache_read = self.cache_read.unwrap_or(0);
        let cache_write = self.cache_write.unwrap_or(0);
        Usage {
            // cache-exclusive input 折入总输入（模块头注释；goose
            // Usage::from_cache_exclusive_input 同口径）。
            input: self
                .input
                .unwrap_or(0)
                .saturating_add(cache_read)
                .saturating_add(cache_write),
            output: self.output.unwrap_or(0),
            cache_read,
            cache_write,
        }
    }
}

/// anthropic 引擎流状态：共享 [`StreamState`] + `tools_seen`
/// （tool 块在各自 content_block_stop 时就已 flush，收尾时无残留可查）
/// + usage 累加器。
#[derive(Debug, Default)]
struct AnthropicStreamState {
    st: StreamState,
    tools_seen: bool,
    usage: UsageAccum,
}

impl AnthropicStreamState {
    /// 单个 SSE 事件 → 状态迁移 / 事件产出；事件类型取 data 的 `type` 字段，
    /// 缺省时回落到 `event:` 行（两者正常情况下相同）。
    fn apply_event(&mut self, data: &Value, event_name: Option<&str>) -> Result<(), ProviderError> {
        let event_type = data
            .get("type")
            .and_then(Value::as_str)
            .or(event_name)
            .unwrap_or("");
        match event_type {
            EVENT_MESSAGE_START => {
                if let Some(usage) = data.pointer("/message/usage") {
                    self.usage.merge(usage);
                }
            }
            EVENT_CONTENT_BLOCK_START => {
                let block = data.get("content_block");
                // thinking / redacted_thinking 等块类型 v1 不进消息模型，静默跳过。
                if block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use") {
                    let index = data.get("index").and_then(Value::as_i64);
                    let id = block.and_then(|b| b.get("id")).and_then(Value::as_str);
                    let name = block.and_then(|b| b.get("name")).and_then(Value::as_str);
                    if let (Some(index), Some(id), Some(name)) = (index, id, name) {
                        self.st.tools.insert(
                            index,
                            PendingCall {
                                id: id.to_string(),
                                name: name.to_string(),
                                arguments: String::new(),
                            },
                        );
                    }
                }
            }
            EVENT_CONTENT_BLOCK_DELTA => {
                let Some(delta) = data.get("delta") else {
                    return Ok(());
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                self.st
                                    .out
                                    .push_back(Ok(StreamEvent::TextDelta(text.to_string())));
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        // 全部拼完再 parse：这里只累积片段。
                        if let Some(call) = data
                            .get("index")
                            .and_then(Value::as_i64)
                            .and_then(|index| self.st.tools.get_mut(&index))
                        {
                            if let Some(part) = delta.get("partial_json").and_then(Value::as_str) {
                                call.arguments.push_str(part);
                            }
                        }
                    }
                    // thinking_delta / signature_delta 及其它未知 delta：忽略。
                    _ => {}
                }
            }
            EVENT_CONTENT_BLOCK_STOP => {
                if let Some(call) = data
                    .get("index")
                    .and_then(Value::as_i64)
                    .and_then(|index| self.st.tools.remove(&index))
                {
                    self.flush_tool(call)?;
                }
            }
            EVENT_MESSAGE_DELTA => {
                if let Some(usage) = data.get("usage") {
                    self.usage.merge(usage);
                }
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    if !reason.is_empty() {
                        self.st.stop = Some(reason.to_string());
                    }
                }
            }
            EVENT_MESSAGE_STOP => {
                // 新版 API 会在 message_stop 再报一次全量 usage。
                if let Some(usage) = data.get("usage") {
                    self.usage.merge(usage);
                }
                self.finalize();
            }
            EVENT_ERROR => {
                return Err(map_stream_error(data.get("error").unwrap_or(&Value::Null)));
            }
            // ping 与未知事件类型：跳过（goose :1167~1171 同款宽容）。
            _ => {}
        }
        Ok(())
    }

    /// 块停止（或收尾）时 flush 一个 tool_use：Start → 完整 JSON Delta → End。
    /// 空参数当 `{}`（content_block_start 的 `input: {}` 后没有任何 delta 的形态）。
    fn flush_tool(&mut self, call: PendingCall) -> Result<(), ProviderError> {
        if !call.arguments.trim().is_empty() {
            serde_json::from_str::<Value>(&call.arguments).map_err(|err| {
                ProviderError::Transport(format!(
                    "invalid tool input JSON for `{}`: {err}",
                    call.id
                ))
            })?;
        }
        self.tools_seen = true;
        self.st.out.push_back(Ok(StreamEvent::ToolUseStart {
            id: call.id,
            name: call.name,
        }));
        self.st
            .out
            .push_back(Ok(StreamEvent::ToolUseDelta(arguments_or_empty(
                call.arguments,
            ))));
        self.st.out.push_back(Ok(StreamEvent::ToolUseEnd));
        Ok(())
    }
}

impl StreamEngine for AnthropicStreamState {
    fn out(&mut self) -> &mut VecDeque<Result<StreamEvent, ProviderError>> {
        &mut self.st.out
    }

    fn ended(&mut self) -> &mut bool {
        &mut self.st.ended
    }

    fn apply(&mut self, ev: &SseEvent) -> Result<(), ProviderError> {
        match parse_chunk(ev)? {
            Some(chunk) => self.apply_event(&chunk, ev.event.as_deref()),
            None => Ok(()),
        }
    }

    /// `message_stop` / 断流收尾：未停止的 tool 块按 index 升序补 flush，再发 `Done`。
    fn finalize(&mut self) {
        if self.st.ended {
            return;
        }
        self.st.ended = true;
        for (_, call) in std::mem::take(&mut self.st.tools) {
            if let Err(err) = self.flush_tool(call) {
                // 截断的 tool 块 JSON 非法：以流错误终止，不再补 Done。
                self.st.out.push_back(Err(err));
                return;
            }
        }
        let usage = self.usage.usage();
        let stop_reason = map_stop_reason(self.st.stop.as_deref(), self.tools_seen);
        self.st
            .out
            .push_back(Ok(StreamEvent::Done { usage, stop_reason }));
    }
}

fn sse_to_stream_events(
    events: BoxStream<'static, crate::Result<SseEvent>>,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
    crate::provider::shared::sse_to_stream_events(AnthropicStreamState::default(), events)
}

/// `stop_reason` → [`StopReason`]；无 stop_reason 但有 tool 调用时按 ToolUse
/// 收尾（openai 引擎同款防御）。
fn map_stop_reason(reason: Option<&str>, saw_tools: bool) -> StopReason {
    match reason {
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        None if saw_tools => StopReason::ToolUse,
        _ => StopReason::Other,
    }
}

/// 流内 `error` 事件（HTTP 层已处理首包前状态码，这里只兜流中途）：
/// overloaded / rate_limit → RateLimited（loop 侧可退避重试）；
/// 鉴权类 → Auth；`prompt is too long` 文案 → ContextOverflow；其余 Transport。
fn map_stream_error(error: &Value) -> ProviderError {
    let kind = error.get("type").and_then(Value::as_str).unwrap_or("");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    match kind {
        "rate_limit_error" | "overloaded_error" => ProviderError::RateLimited { retry_after: None },
        "authentication_error" | "unauthorized" | "permission_error" => ProviderError::Auth,
        "not_found_error" | "request_too_large" | "api_error" => {
            ProviderError::Transport(format!("anthropic stream error ({kind}): {message}"))
        }
        _ if crate::provider::http::is_context_overflow(400, message) => {
            ProviderError::ContextOverflow
        }
        _ => ProviderError::Transport(format!("anthropic stream error ({kind}): {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::context_limit_for;
    use crate::provider::http::SseParser;
    use crate::provider::shared::testutil as tu;
    use crate::provider::shared::testutil::collect;
    use crate::provider::shared::testutil::event_to_json;
    use crate::provider::shared::testutil::fast_retry;
    use crate::provider::shared::testutil::sse_body;
    use crate::provider::DEFAULT_PROVIDER_TIMEOUT;
    use futures::stream as fstream;
    use futures::StreamExt;
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    fn def(base_url: Option<&str>) -> ProviderDef {
        tu::def("test-anthropic", EngineKind::Anthropic, base_url)
    }

    fn provider_at(base_url: &str, api_key: &str) -> AnthropicProvider {
        let (def, http) = tu::provider_parts("test-anthropic", EngineKind::Anthropic, base_url);
        AnthropicProvider {
            def,
            api_key: api_key.to_string(),
            http,
        }
    }

    fn request<'a>(messages: &'a [Message], tools: &'a [ToolSpec]) -> Request<'a> {
        tu::request("claude-sonnet-4-5", 4096, messages, tools)
    }

    async fn mount_once(server: &MockServer, response: ResponseTemplate) {
        tu::mount_once(server, "/v1/messages", response).await;
    }

    // ---- 请求侧 ----

    #[test]
    fn messages_url_appends_v1_messages_once() {
        assert_eq!(
            messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://gw.internal/"),
            "https://gw.internal/v1/messages"
        );
        // 已写 /v1 的不双拼。
        assert_eq!(
            messages_url("https://gw.internal/v1"),
            "https://gw.internal/v1/messages"
        );
    }

    #[test]
    fn system_block_and_last_tool_get_cache_control() {
        let tools = vec![
            ToolSpec {
                name: "builtin__read".into(),
                description: "r".into(),
                input_schema: json!({"type": "object", "properties": {}}),
                read_only: true,
            },
            ToolSpec {
                name: "builtin__write".into(),
                description: "w".into(),
                // 空 schema 补 type:object（goose :479~486）。
                input_schema: json!({}),
                read_only: false,
            },
        ];
        let messages = vec![Message::user_text("hi".into())];
        let body = build_request_body(&request(&messages, &tools)).unwrap();
        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "be terse");
        assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));
        let specs = body["tools"].as_array().unwrap();
        assert!(specs[0].get("cache_control").is_none());
        assert_eq!(specs[1]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(specs[1]["input_schema"], json!({"type": "object"}));
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["temperature"], 0.5);
        // 无 system 时不发 system 字段。
        let req = Request {
            system: "",
            ..request(&messages, &[])
        };
        let body = build_request_body(&req).unwrap();
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn format_messages_shapes_blocks() {
        let messages = vec![
            Message::user_text("run".into()),
            Message::assistant(
                vec![
                    Content::Text("  ".into()), // 纯空白跳过（goose :252）
                    Content::Text("working".into()),
                    Content::ToolUse {
                        id: "t1".into(),
                        name: "builtin__shell".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
                None,
            ),
            Message {
                role: Role::User,
                content: vec![
                    Content::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                    Content::Text("and more".into()),
                ],
                ts: 0,
                usage: None,
            },
        ];
        crate::message::validate(&messages).unwrap();
        let spec = format_messages(&messages);
        assert_eq!(spec.len(), 3);
        assert_eq!(
            spec[0],
            json!({"role":"user","content":[{"type":"text","text":"run"}]})
        );
        let assistant = &spec[1];
        assert_eq!(assistant["content"].as_array().unwrap().len(), 2);
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        assert_eq!(assistant["content"][1]["id"], "t1");
        let tool_result = &spec[2]["content"][0];
        assert_eq!(tool_result["type"], "tool_result");
        assert_eq!(tool_result["tool_use_id"], "t1");
        assert_eq!(tool_result["content"], "ok");
        assert!(tool_result.get("is_error").is_none());
        // 错误结果带 is_error: true。
        let err_only = format_messages(&[Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "t1".into(),
                content: "boom".into(),
                is_error: true,
            }],
            ts: 0,
            usage: None,
        }]);
        assert_eq!(err_only[0]["content"][0]["is_error"], true);
    }

    #[test]
    fn tool_use_null_input_becomes_empty_object() {
        let spec = format_messages(&[Message::assistant(
            vec![Content::ToolUse {
                id: "t1".into(),
                name: "ping".into(),
                input: Value::Null,
            }],
            None,
        )]);
        assert_eq!(spec[0]["content"][0]["input"], json!({}));
    }

    #[test]
    fn format_tools_sanitizes_and_rejects_duplicates() {
        let tools = vec![
            ToolSpec {
                name: "developer shell".into(),
                description: "d".into(),
                input_schema: json!({"type": "object"}),
                read_only: false,
            },
            ToolSpec {
                name: "developer/shell".into(),
                description: "".into(),
                input_schema: json!({}),
                read_only: false,
            },
        ];
        let dup = format_tools(&tools).unwrap_err();
        assert!(dup.to_string().contains("duplicate tool name"), "{dup}");
        let one = format_tools(&tools[..1]).unwrap();
        assert_eq!(one[0]["name"], "developer_shell");
        assert!(one[0].get("type").is_none()); // 与 openai 引擎不同：无 function 包装
    }

    // ---- 响应侧状态机 ----

    /// 原始 SSE 文本 → SseParser → 状态机 → StreamEvent 列表（不经网络）。
    async fn run_sse(text: &str) -> Vec<StreamEvent> {
        tu::run_sse(text, sse_to_stream_events).await
    }

    fn fixture_pair(name: &str) -> (String, Value) {
        tu::fixture_pair("anthropic", name)
    }

    // M2 · SSE fixture 对照（样本自 goose formats/anthropic.rs 测试段
    // :2290~2548 抄录，期望结果按本引擎 StreamEvent 口径改写）

    #[tokio::test]
    async fn fixture_max_tokens_truncation() {
        let (sse, expected) = fixture_pair("max_tokens");
        let got: Vec<Value> = run_sse(&sse).await.iter().map(event_to_json).collect();
        assert_eq!(Value::Array(got), expected);
    }

    #[tokio::test]
    async fn fixture_thinking_blocks_skipped_text_then_tool() {
        let (sse, expected) = fixture_pair("thinking_text_tool");
        let got: Vec<Value> = run_sse(&sse).await.iter().map(event_to_json).collect();
        assert_eq!(Value::Array(got), expected);
    }

    #[tokio::test]
    async fn fixture_interleaved_parallel_tools_flush_at_own_stop() {
        let (sse, expected) = fixture_pair("parallel_tool_calls");
        let got: Vec<Value> = run_sse(&sse).await.iter().map(event_to_json).collect();
        assert_eq!(Value::Array(got), expected);
    }

    #[tokio::test]
    async fn fixture_cache_tokens_fold_into_input() {
        let (sse, expected) = fixture_pair("cache_tokens");
        let got: Vec<Value> = run_sse(&sse).await.iter().map(event_to_json).collect();
        assert_eq!(Value::Array(got), expected);
        // 200k 的 claude 前缀小表口径下，15007 输入不应触发压缩但也绝非 7。
        assert!(context_limit_for("claude-sonnet-4-5") > 15007);
    }

    #[tokio::test]
    async fn fixture_tool_use_without_args_flushes_empty_object() {
        let (sse, expected) = fixture_pair("tool_no_args");
        let got: Vec<Value> = run_sse(&sse).await.iter().map(event_to_json).collect();
        assert_eq!(Value::Array(got), expected);
    }

    #[tokio::test]
    async fn stop_sequence_maps_end_turn_and_ping_is_ignored() {
        let sse = concat!(
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"stop_sequence\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        assert_eq!(
            run_sse(sse).await.last(),
            Some(&StreamEvent::Done {
                usage: Usage {
                    input: 1,
                    output: 2,
                    ..Default::default()
                },
                stop_reason: StopReason::EndTurn,
            })
        );
    }

    #[tokio::test]
    async fn stream_without_message_stop_finalizes() {
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"tail\"}}\n\n",
        );
        assert_eq!(
            run_sse(sse).await,
            vec![
                StreamEvent::TextDelta("tail".into()),
                StreamEvent::Done {
                    usage: Usage {
                        input: 3,
                        output: 0,
                        ..Default::default()
                    },
                    stop_reason: StopReason::Other,
                },
            ]
        );
    }

    #[tokio::test]
    async fn truncated_tool_json_fails_the_stream() {
        // content_block_stop 缺席、断流时参数不完整 → Transport，而不是把
        // 非法 JSON 交给下游 parse。
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"f\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n",
        );
        let mut parser = SseParser::default();
        let events = parser.feed(sse);
        let input: BoxStream<'static, crate::Result<SseEvent>> =
            fstream::iter(events.into_iter().map(Ok)).boxed();
        let mut stream = sse_to_stream_events(input);
        let err = stream
            .next()
            .await
            .unwrap()
            .expect_err("truncated args must error");
        assert!(
            matches!(&err, ProviderError::Transport(m) if m.contains("invalid tool input JSON")),
            "{err}"
        );
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn in_stream_error_frame_maps_to_provider_errors() {
        for (payload, expect_overloaded) in [
            (
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                true,
            ),
            (
                r#"{"type":"error","error":{"type":"authentication_error","message":"bad key"}}"#,
                false,
            ),
            (
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 300000 tokens > 200000 maximum"}}"#,
                false,
            ),
        ] {
            let mut parser = SseParser::default();
            let events = parser.feed(&format!("data: {payload}\n\ndata: {payload}\n\n"));
            let input: BoxStream<'static, crate::Result<SseEvent>> =
                fstream::iter(events.into_iter().map(Ok)).boxed();
            let mut stream = sse_to_stream_events(input);
            let err = stream
                .next()
                .await
                .unwrap()
                .expect_err("error frame must surface");
            if expect_overloaded {
                assert!(
                    matches!(err, ProviderError::RateLimited { retry_after: None }),
                    "{err}"
                );
            } else if payload.contains("authentication_error") {
                assert!(matches!(err, ProviderError::Auth), "{err}");
            } else {
                assert!(matches!(err, ProviderError::ContextOverflow), "{err}");
            }
            assert!(stream.next().await.is_none());
        }
    }

    // ---- wiremock 集成 ----

    #[tokio::test]
    async fn wiremock_sends_key_version_and_custom_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(wiremock::matchers::header("x-api-key", "sk-ant-test"))
            .and(wiremock::matchers::header(
                "anthropic-version",
                ANTHROPIC_VERSION,
            ))
            .and(wiremock::matchers::header("x-instagent-test", "yes"))
            .and(wiremock::matchers::header("accept", "text/event-stream"))
            .respond_with(sse_body(concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            )))
            .expect(1)
            .mount(&server)
            .await;
        let provider = provider_at(&server.uri(), "sk-ant-test");
        let messages = vec![Message::user_text("hi".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        let events = collect(&mut stream).await.unwrap();
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Done {
                stop_reason: StopReason::EndTurn,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn wiremock_no_key_omits_x_api_key_but_keeps_version() {
        let server = MockServer::start().await;
        mount_once(&server, sse_body("data: {\"type\":\"message_stop\"}\n\n")).await;
        let provider = provider_at(&server.uri(), "");
        let messages = vec![Message::user_text("hi".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        collect(&mut stream).await.unwrap();
        let received = server.received_requests().await.unwrap();
        assert!(!received[0].headers.contains_key("x-api-key"));
        assert_eq!(
            received[0].headers["anthropic-version"].to_str().unwrap(),
            ANTHROPIC_VERSION
        );
    }

    #[tokio::test]
    async fn wiremock_request_body_wellformed_end_to_end() {
        let server = MockServer::start().await;
        mount_once(
            &server,
            sse_body(concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"usage\":{\"input_tokens\":9,\"output_tokens\":0}}}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            )),
        )
        .await;
        let tools = vec![ToolSpec {
            name: "developer shell".into(),
            description: "run".into(),
            input_schema: json!({"type": "object"}),
            read_only: false,
        }];
        let messages = vec![
            Message::user_text("run it".into()),
            Message::assistant(
                vec![Content::ToolUse {
                    id: "c1".into(),
                    name: "developer shell".into(),
                    input: json!({"c": 1}),
                }],
                None,
            ),
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
                ts: 0,
                usage: None,
            },
        ];
        crate::message::validate(&messages).unwrap();
        let provider = provider_at(&server.uri(), "sk-ant-test");
        let mut stream = provider.stream(request(&messages, &tools)).await.unwrap();
        let events = collect(&mut stream).await.unwrap();
        assert_eq!(
            events[0],
            StreamEvent::TextDelta("Hello".into()),
            "{events:?}"
        );

        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["name"], "developer_shell");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"][0]["text"], "run it");
        assert_eq!(msgs[1]["content"][0]["name"], "developer_shell");
        assert_eq!(msgs[1]["content"][0]["input"], json!({"c": 1}));
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "c1");
    }

    #[tokio::test]
    async fn wiremock_http_errors_map_via_shared_http_layer() {
        // 401 → Auth（一次请求，不重试）。
        let server = MockServer::start().await;
        mount_once(&server, ResponseTemplate::new(401).set_body_string("nope")).await;
        let provider = provider_at(&server.uri(), "sk-bad");
        let messages = vec![Message::user_text("hi".into())];
        let err = provider
            .stream(request(&messages, &[]))
            .await
            .map(drop)
            .expect_err("401");
        assert!(matches!(err, ProviderError::Auth), "{err:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        // 429 一次（Retry-After: 0）后成功。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0")
                    .set_body_string(r#"{"type":"error","error":{"type":"rate_limit_error"}}"#),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_once(&server, sse_body("data: {\"type\":\"message_stop\"}\n\n")).await;
        let provider = provider_at(&server.uri(), "sk-test");
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        collect(&mut stream).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 2);

        // 400 + "prompt is too long" → ContextOverflow（http::map_http_error 复用）。
        let server = MockServer::start().await;
        mount_once(
            &server,
            ResponseTemplate::new(400).set_body_string(
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
            ),
        )
        .await;
        let provider = provider_at(&server.uri(), "sk-test");
        let err = provider
            .stream(request(&messages, &[]))
            .await
            .map(drop)
            .expect_err("overflow");
        assert!(matches!(err, ProviderError::ContextOverflow), "{err:?}");
    }

    // ---- 构造函数与 trait 形状 ----

    #[test]
    fn new_validates_engine_base_url_and_reads_env_key() {
        let mut openai = def(Some("https://x.invalid"));
        openai.engine = EngineKind::Openai;
        assert!(AnthropicProvider::new(&openai)
            .unwrap_err()
            .to_string()
            .contains("not an anthropic engine"));
        assert!(AnthropicProvider::new(&def(None))
            .unwrap_err()
            .to_string()
            .contains("missing base_url"));

        const VAR: &str = "INSTAGENT_TEST_12_API_KEY";
        std::env::remove_var(VAR);
        let mut with_key = def(Some("https://x.invalid"));
        with_key.api_key_env = Some(VAR.to_string());
        assert!(
            AnthropicProvider::new(&with_key).is_err(),
            "env 未设置必须报错"
        );
        std::env::set_var(VAR, "sk-ant-from-env");
        let provider = AnthropicProvider::new(&with_key).unwrap();
        std::env::remove_var(VAR);
        assert_eq!(provider.api_key, "sk-ant-from-env");
        assert_eq!(provider.name(), "test-anthropic");
        assert_eq!(provider.http.timeout, DEFAULT_PROVIDER_TIMEOUT);

        // 无 api_key_env：空密钥（裸网关形态），headers 里不出现 x-api-key。
        let keyless = AnthropicProvider::new(&def(Some("http://localhost:8080"))).unwrap();
        assert!(!keyless.request_headers().contains_key("x-api-key"));
        assert_eq!(
            keyless.request_headers()["anthropic-version"],
            ANTHROPIC_VERSION
        );
        // with_retry 形状同 openai 引擎，不改 def / api_key。
        let fast = keyless.clone().with_retry(fast_retry());
        assert_eq!(fast.http.retry.initial_backoff, Duration::from_millis(1));
        assert_eq!(fast.def, keyless.def);
    }

    #[tokio::test]
    async fn provider_object_safe_as_trait_object() {
        let server = MockServer::start().await;
        mount_once(
            &server,
            sse_body(concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hey\"}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            )),
        )
        .await;
        let provider: Arc<dyn Provider> = Arc::new(provider_at(&server.uri(), "sk-ant-test"));
        let messages = vec![Message::user_text("hi".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        let events = collect(&mut stream).await.unwrap();
        assert!(matches!(
            &events[0],
            StreamEvent::TextDelta(t) if t == "hey"
        ));
    }
}
