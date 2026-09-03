//! OpenAI Chat Completions 引擎（第二版 §2.3 openai.rs；同时服务 openai / ollama /
//! groq / deepseek / openrouter，靠 base_url + key 区分）。
//!
//! `POST {base_url}/chat/completions`，`stream: true` +
//! `stream_options: {include_usage: true}`，`Authorization: Bearer <api_key_env>`。
//! HTTP / SSE / 重试全部复用 [`crate::provider::http`]。
//!
//! 四个已知怪癖（goose `crates/goose-provider-types/src/formats/openai.rs`，
//! 规则照抄）：
//!
//! 1. 带 tool_calls 的 assistant 消息 `content` 必须为 null 或 ""，不能省略字段
//!    （`format_messages` 内，goose formats/openai.rs:445~454）；
//! 2. function name 只允许 `[A-Za-z0-9_-]{1,64}`，非法字符替换为 `_`、超长截断
//!    （[`sanitize_function_name`]，goose formats/openai.rs:1918，长度上限取 64）；
//! 3. tool 消息必须紧跟含对应 tool_calls 的 assistant 消息
//!    （`format_messages`：user 消息里的 ToolResult 先于同消息的文本输出，
//!    会话不变量（`02`）保证 assistant→results 相邻）；
//! 4. arguments 为空串时当 `{}`（请求侧 `tool_arguments`；响应侧在 `finalize`
//!    flush 时兜底）。
//!
//! 流式组装说明：`StreamEvent::ToolUseDelta` 不带 id，并行 tool_calls 的
//! arguments 又会按 index 交错到达，所以 tool 事件在流结束（`[DONE]` /
//! 断流）时按 index 升序整组 flush（Start→Delta→End），文本 delta 即时透传。
//! 构造入参 = provider JSON 定义，供 `10` registry 调用。

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
use crate::provider::http::is_done;
use crate::provider::http::HttpClient;
use crate::provider::http::SseEvent;
use crate::provider::shared::arguments_or_empty;
use crate::provider::shared::engine_parts;
use crate::provider::shared::headers_with_auth;
use crate::provider::shared::parse_chunk;
use crate::provider::shared::require_base_url;
use crate::provider::shared::sanitize_function_name;
use crate::provider::shared::sanitized_tool_names;
use crate::provider::shared::to_provider_error;
use crate::provider::shared::StreamEngine;
use crate::provider::shared::StreamState;
use crate::provider::EngineKind;
use crate::provider::Provider;
use crate::provider::ProviderDef;
use crate::provider::Request;
use crate::provider::StopReason;
use crate::provider::StreamEvent;
use crate::tools::ToolSpec;

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    pub def: ProviderDef,
    /// 从 `def.api_key_env` 指定的环境变量读取；无 `api_key_env` 时为空串。
    pub api_key: String,
    pub http: HttpClient,
}

impl OpenAiProvider {
    /// 校验 def（engine=openai、base_url 必填），按 `api_key_env` 读密钥，
    /// 按 `timeout_seconds` 建 client（构造骨架在共享层 [`engine_parts`]）。
    pub fn new(def: &ProviderDef) -> crate::Result<Self> {
        let (api_key, http) = engine_parts(def, EngineKind::Openai)?;
        Ok(Self {
            def: def.clone(),
            api_key,
            http,
        })
    }

    /// def 自带 headers + `Authorization: Bearer`（密钥为空则不发该头）。
    fn request_headers(&self) -> BTreeMap<String, String> {
        let auth = (!self.api_key.is_empty()).then(|| {
            (
                "authorization".to_string(),
                format!("Bearer {}", self.api_key),
            )
        });
        headers_with_auth(&self.def, auth)
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.def.name
    }

    async fn stream(
        &self,
        req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let base_url = require_base_url(&self.def)?;
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let body = build_request_body(&req).map_err(to_provider_error)?;
        let sse = self
            .http
            .post_sse(&url, &self.request_headers(), &body)
            .await
            .map_err(to_provider_error)?;
        Ok(sse_to_stream_events(sse))
    }
}

// ---------------------------------------------------------------------------
// 请求侧：消息 / 工具格式化
// ---------------------------------------------------------------------------

fn build_request_body(req: &Request<'_>) -> crate::Result<Value> {
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    messages.extend(format_messages(req.messages));

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": req.max_tokens,
    });
    if !req.tools.is_empty() {
        body["tools"] = json!(format_tools(req.tools)?);
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }
    Ok(body)
}

/// [`Message`] 列表 → OpenAI `messages` 数组（怪癖 1 / 3 / 4 的请求侧）。
fn format_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            Role::Assistant => {
                let mut texts = Vec::new();
                let mut tool_calls = Vec::new();
                for content in &message.content {
                    match content {
                        Content::Text(text) => texts.push(text.clone()),
                        Content::ToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": sanitize_function_name(name),
                                    "arguments": tool_arguments(input),
                                }
                            }));
                        }
                        Content::ToolResult { .. } => {}
                    }
                }
                let mut converted = json!({"role": "assistant"});
                if !texts.is_empty() {
                    converted["content"] = json!(texts.join("\n"));
                }
                if !tool_calls.is_empty() {
                    converted["tool_calls"] = json!(tool_calls);
                    // 怪癖 1：content 字段必须存在（严格校验的兼容 API 要求）。
                    if converted.get("content").is_none() {
                        converted["content"] = Value::Null;
                    }
                }
                out.push(converted);
            }
            Role::User => {
                let mut texts = Vec::new();
                let mut results = Vec::new();
                for content in &message.content {
                    match content {
                        Content::Text(text) => texts.push(text.clone()),
                        Content::ToolResult {
                            tool_use_id,
                            content: text,
                            ..
                        } => results.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": text,
                        })),
                        Content::ToolUse { .. } => {}
                    }
                }
                // 怪癖 3：tool 消息紧跟含 tool_calls 的 assistant 消息；
                // 同一条 user 消息里混有文本时，文本放在 results 之后。
                out.extend(results);
                if !texts.is_empty() {
                    out.push(json!({"role": "user", "content": texts.join("\n")}));
                }
            }
        }
    }
    out
}

/// 怪癖 4 的请求侧：arguments 一律是 JSON 字符串，空输入序列化成 `"{}"`。
fn tool_arguments(input: &Value) -> String {
    if input.is_null() {
        return "{}".to_string();
    }
    serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string())
}

/// [`ToolSpec`] → `{type:"function", function:{name,description,parameters}}`；
/// sanitize + 重名报错在共享层 [`sanitized_tool_names`]
/// （goose formats/openai.rs format_tools 同款防御）。
fn format_tools(tools: &[ToolSpec]) -> crate::Result<Vec<Value>> {
    let names = sanitized_tool_names(tools)?;
    Ok(tools
        .iter()
        .zip(names)
        .map(|(tool, name)| {
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// 响应侧：SSE 事件流 → StreamEvent
// ---------------------------------------------------------------------------

/// openai 引擎流状态：共享 [`StreamState`] + usage 累积
/// （tool_calls 按 `delta.tool_calls[].index` 累积，第二版 §6 风险 1）。
#[derive(Debug, Default)]
struct OpenAiStreamState {
    st: StreamState,
    usage: Usage,
}

impl OpenAiStreamState {
    /// 单个流式 chunk：文本即时出 TextDelta，tool_calls 按 index 累积，
    /// usage / finish_reason 记录到最后（`Done` 在收尾钩子统一发）。
    fn apply_chunk(&mut self, chunk: &Value) -> Result<(), ProviderError> {
        if let Some(err) = chunk.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(ProviderError::Transport(format!(
                "provider error in stream: {msg}"
            )));
        }
        if let Some(usage) = usage_from_chunk(chunk) {
            self.usage = usage;
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            // 空串 finish_reason 不是终止信号（ollama / 部分网关会发 ""）。
            if !reason.is_empty() {
                self.st.stop = Some(reason.to_string());
            }
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                self.st
                    .out
                    .push_back(Ok(StreamEvent::TextDelta(text.to_string())));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_i64)
                    .unwrap_or(position as i64);
                let pending = self.st.tools.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    if !id.is_empty() && pending.id.is_empty() {
                        pending.id = id.to_string();
                    }
                }
                let Some(function) = call.get("function") else {
                    continue;
                };
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    if !name.is_empty() && pending.name.is_empty() {
                        pending.name = name.to_string();
                    }
                }
                // arguments 可能缺省 / null（goose 的 delta 测试覆盖过）。
                if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                    pending.arguments.push_str(args);
                }
            }
        }
        Ok(())
    }
}

impl StreamEngine for OpenAiStreamState {
    fn out(&mut self) -> &mut VecDeque<Result<StreamEvent, ProviderError>> {
        &mut self.st.out
    }

    fn ended(&mut self) -> &mut bool {
        &mut self.st.ended
    }

    fn apply(&mut self, ev: &SseEvent) -> Result<(), ProviderError> {
        if is_done(ev) {
            self.finalize();
            return Ok(());
        }
        match parse_chunk(ev)? {
            Some(chunk) => self.apply_chunk(&chunk),
            None => Ok(()),
        }
    }

    /// `[DONE]` / 断流收尾：tool 事件按 index 升序整组 flush，再发 `Done`。
    fn finalize(&mut self) {
        if self.st.ended {
            return;
        }
        self.st.ended = true;
        let stop_reason = match self.st.stop.as_deref() {
            // 没收到 finish_reason 但有 tool_calls：按 ToolUse 收尾更可用。
            None if !self.st.tools.is_empty() => StopReason::ToolUse,
            reason => map_stop_reason(reason),
        };
        for (_, call) in std::mem::take(&mut self.st.tools) {
            // 怪癖 4 的响应侧：空 arguments 当 `{}`。
            let arguments = arguments_or_empty(call.arguments);
            self.st.out.push_back(Ok(StreamEvent::ToolUseStart {
                id: call.id,
                name: call.name,
            }));
            self.st
                .out
                .push_back(Ok(StreamEvent::ToolUseDelta(arguments)));
            self.st.out.push_back(Ok(StreamEvent::ToolUseEnd));
        }
        self.st.out.push_back(Ok(StreamEvent::Done {
            usage: self.usage,
            stop_reason,
        }));
    }
}

fn sse_to_stream_events(
    events: BoxStream<'static, crate::Result<SseEvent>>,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
    crate::provider::shared::sse_to_stream_events(OpenAiStreamState::default(), events)
}

/// `finish_reason` → [`StopReason`]。
fn map_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}

/// usage 映射（goose get_usage 同构）：prompt→input、completion→output、
/// `prompt_tokens_details.cached_tokens`→cache_read、
/// `cache_creation_input_tokens`→cache_write。
fn usage_from_chunk(chunk: &Value) -> Option<Usage> {
    let usage = chunk.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let token = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0) as u32;
    Some(Usage {
        input: token(usage.get("prompt_tokens")),
        output: token(usage.get("completion_tokens")),
        cache_read: token(usage.pointer("/prompt_tokens_details/cached_tokens")),
        cache_write: token(usage.get("cache_creation_input_tokens")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::http::SseParser;
    use crate::provider::shared::testutil as tu;
    use crate::provider::shared::testutil::collect;
    use crate::provider::shared::testutil::event_to_json;
    use crate::provider::shared::testutil::sse_body;
    use crate::provider::shared::MAX_FUNCTION_NAME_LENGTH;
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
        tu::def("test-openai", EngineKind::Openai, base_url)
    }

    fn provider_at(base_url: &str, api_key: &str) -> OpenAiProvider {
        let (def, http) = tu::provider_parts("test-openai", EngineKind::Openai, base_url);
        OpenAiProvider {
            def,
            api_key: api_key.to_string(),
            http,
        }
    }

    fn request<'a>(messages: &'a [Message], tools: &'a [ToolSpec]) -> Request<'a> {
        tu::request("gpt-4o", 1024, messages, tools)
    }

    async fn mount_once(server: &MockServer, response: ResponseTemplate) {
        tu::mount_once(server, "/v1/chat/completions", response).await;
    }

    // ---- 怪癖 1~4：请求侧格式化 ----

    #[test]
    fn quirk1_assistant_tool_calls_keeps_content_field() {
        let messages = vec![
            Message::user_text("run".into()),
            Message::assistant(
                vec![Content::ToolUse {
                    id: "c1".into(),
                    name: "shell".into(),
                    input: json!({"command": "ls"}),
                }],
                None,
            ),
        ];
        let spec = format_messages(&messages);
        assert_eq!(spec.len(), 2);
        let assistant = &spec[1];
        assert_eq!(assistant["role"], "assistant");
        // 字段必须存在且为 null（不是省略）。
        assert!(assistant.get("content").is_some());
        assert!(assistant["content"].is_null());
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            r#"{"command":"ls"}"#
        );
    }

    #[test]
    fn quirk1_mixed_text_and_tool_calls_keeps_text() {
        let messages = vec![Message::assistant(
            vec![
                Content::Text("thinking".into()),
                Content::ToolUse {
                    id: "c1".into(),
                    name: "shell".into(),
                    input: json!({}),
                },
            ],
            None,
        )];
        let spec = format_messages(&messages);
        assert_eq!(spec[0]["content"], "thinking");
        // 怪癖 4 请求侧：空输入序列化为 "{}"。
        assert_eq!(spec[0]["tool_calls"][0]["function"]["arguments"], "{}");
    }

    #[test]
    fn quirk2_sanitize_function_name() {
        assert_eq!(sanitize_function_name("hello-world"), "hello-world");
        assert_eq!(sanitize_function_name("hello world"), "hello_world");
        assert_eq!(sanitize_function_name("weird.na!me?"), "weird_na_me_");
        assert_eq!(
            sanitize_function_name(&"a".repeat(MAX_FUNCTION_NAME_LENGTH + 32)),
            "a".repeat(MAX_FUNCTION_NAME_LENGTH)
        );
        assert_eq!(sanitize_function_name(""), "tool");
        assert_eq!(sanitize_function_name("中文"), "__");
        // 幂等。
        let once = sanitize_function_name("a.b/c");
        assert_eq!(sanitize_function_name(&once), once);
    }

    #[test]
    fn quirk2_format_tools_sanitizes_and_rejects_duplicates() {
        let tools = vec![
            ToolSpec {
                name: "developer shell".into(),
                description: "d".into(),
                input_schema: json!({"type": "object"}),
                read_only: false,
            },
            ToolSpec {
                name: "x".into(),
                description: "".into(),
                input_schema: json!({}),
                read_only: true,
            },
        ];
        let formatted = format_tools(&tools).unwrap();
        assert_eq!(formatted[0]["function"]["name"], "developer_shell");
        assert_eq!(formatted[0]["type"], "function");
        let dup = vec![
            tools[0].clone(),
            ToolSpec {
                name: "developer/shell".into(),
                description: "".into(),
                input_schema: json!({}),
                read_only: false,
            },
        ];
        assert!(format_tools(&dup).is_err());
    }

    #[test]
    fn quirk3_tool_results_come_right_after_assistant() {
        let messages = vec![
            Message::user_text("run".into()),
            Message::assistant(
                vec![
                    Content::ToolUse {
                        id: "c1".into(),
                        name: "shell".into(),
                        input: json!({"c": 1}),
                    },
                    Content::ToolUse {
                        id: "c2".into(),
                        name: "shell".into(),
                        input: json!({"c": 2}),
                    },
                ],
                None,
            ),
            Message {
                role: Role::User,
                content: vec![
                    Content::ToolResult {
                        tool_use_id: "c1".into(),
                        content: "ok1".into(),
                        is_error: false,
                    },
                    Content::ToolResult {
                        tool_use_id: "c2".into(),
                        content: "boom".into(),
                        is_error: true,
                    },
                    Content::Text("and one more thing".into()),
                ],
                ts: 0,
                usage: None,
            },
            Message::assistant(vec![Content::Text("done".into())], None),
        ];
        crate::message::validate(&messages).unwrap();
        let spec = format_messages(&messages);
        let roles: Vec<&str> = spec.iter().map(|m| m["role"].as_str().unwrap()).collect();
        // assistant(tool_calls) → 两条 tool → user 文本，顺序如此即满足怪癖 3。
        assert_eq!(
            roles,
            vec!["user", "assistant", "tool", "tool", "user", "assistant"]
        );
        assert_eq!(spec[2]["tool_call_id"], "c1");
        assert_eq!(spec[2]["content"], "ok1");
        assert_eq!(spec[3]["tool_call_id"], "c2");
        assert_eq!(spec[4]["content"], "and one more thing");
    }

    // ---- 响应侧状态机 ----

    /// 原始 SSE 文本 → SseParser → 状态机 → StreamEvent 列表（不经网络）。
    async fn run_sse(text: &str) -> Vec<StreamEvent> {
        tu::run_sse(text, sse_to_stream_events).await
    }

    fn fixture_pair(name: &str) -> (String, Value) {
        tu::fixture_pair("openai", name)
    }

    // J2 · SSE fixture 对照（样本自 goose formats/openai.rs 测试段抄改：
    // test_streaming_reassembles_interleaved_parallel_tool_calls /
    // test_response_to_message_empty_argument / test_streamed_multi_tool_response_to_messages）

    #[tokio::test]
    async fn fixture_parallel_tool_calls_accumulate_by_index() {
        let (sse, expected) = fixture_pair("parallel_tool_calls");
        let got: Vec<Value> = run_sse(&sse).await.iter().map(event_to_json).collect();
        assert_eq!(Value::Array(got), expected);
    }

    #[tokio::test]
    async fn fixture_empty_arguments_become_object() {
        let (sse, expected) = fixture_pair("empty_arguments");
        let got: Vec<Value> = run_sse(&sse).await.iter().map(event_to_json).collect();
        assert_eq!(Value::Array(got), expected);
    }

    #[tokio::test]
    async fn fixture_text_mixed_with_tool() {
        let (sse, expected) = fixture_pair("text_and_tool");
        let got: Vec<Value> = run_sse(&sse).await.iter().map(event_to_json).collect();
        assert_eq!(Value::Array(got), expected);
    }

    #[tokio::test]
    async fn usage_maps_cache_fields_and_stop_maps_end_turn() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":30,\
             \"prompt_tokens_details\":{\"cached_tokens\":80},\"cache_creation_input_tokens\":20}}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let events = run_sse(sse).await;
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                usage: Usage {
                    input: 120,
                    output: 30,
                    cache_read: 80,
                    cache_write: 20
                },
                stop_reason: StopReason::EndTurn,
            })
        );
    }

    #[tokio::test]
    async fn finish_reason_variants_and_length() {
        assert_eq!(map_stop_reason(Some("stop")), StopReason::EndTurn);
        assert_eq!(map_stop_reason(Some("tool_calls")), StopReason::ToolUse);
        assert_eq!(map_stop_reason(Some("length")), StopReason::MaxTokens);
        assert_eq!(map_stop_reason(Some("content_filter")), StopReason::Other);
        assert_eq!(map_stop_reason(None), StopReason::Other);
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Partial answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        );
        let events = run_sse(sse).await;
        assert_eq!(
            events[events.len() - 1],
            StreamEvent::Done {
                usage: Usage {
                    input: 10,
                    output: 5,
                    ..Default::default()
                },
                stop_reason: StopReason::MaxTokens,
            }
        );
    }

    #[tokio::test]
    async fn stream_without_done_marker_still_finalizes() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"tail\"},\"finish_reason\":\"stop\"}]}\n\n";
        let events = run_sse(sse).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("tail".into()),
                StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: StopReason::EndTurn,
                },
            ]
        );
    }

    #[tokio::test]
    async fn in_stream_error_frame_surfaces_as_transport_error() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"upstream exploded\"}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut parser = SseParser::default();
        let events = parser.feed(sse);
        let input: BoxStream<'static, crate::Result<SseEvent>> =
            fstream::iter(events.into_iter().map(Ok)).boxed();
        let mut stream = sse_to_stream_events(input);
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            StreamEvent::TextDelta("partial".into())
        );
        let err = stream.next().await.unwrap().unwrap_err();
        assert!(
            matches!(&err, ProviderError::Transport(m) if m.contains("upstream exploded")),
            "{err}"
        );
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn malformed_chunk_fails_the_stream() {
        let mut parser = SseParser::default();
        let events = parser.feed("data: {not json}\n\n");
        let input: BoxStream<'static, crate::Result<SseEvent>> =
            fstream::iter(events.into_iter().map(Ok)).boxed();
        let mut stream = sse_to_stream_events(input);
        let err = stream.next().await.unwrap().unwrap_err();
        assert!(matches!(err, ProviderError::Transport(_)));
    }

    // ---- J3 · wiremock 集成 ----

    /// `16` 折叠逻辑的测试替身：StreamEvent → 完整 assistant Message。
    fn fold(events: &[StreamEvent]) -> (Message, Option<StopReason>) {
        let mut content: Vec<Content> = Vec::new();
        let mut text = String::new();
        let mut usage = None;
        let mut stop = None;
        let mut open: Option<(String, String, String)> = None;
        for ev in events {
            match ev {
                StreamEvent::TextDelta(t) => text.push_str(t),
                StreamEvent::ToolUseStart { id, name } => {
                    if !text.is_empty() {
                        content.push(Content::Text(std::mem::take(&mut text)));
                    }
                    open = Some((id.clone(), name.clone(), String::new()));
                }
                StreamEvent::ToolUseDelta(a) => {
                    if let Some((_, _, args)) = open.as_mut() {
                        args.push_str(a);
                    }
                }
                StreamEvent::ToolUseEnd => {
                    if let Some((id, name, args)) = open.take() {
                        let input: Value = serde_json::from_str(&args).unwrap();
                        content.push(Content::ToolUse { id, name, input });
                    }
                }
                StreamEvent::Done {
                    usage: u,
                    stop_reason,
                } => {
                    usage = Some(*u);
                    stop = Some(*stop_reason);
                }
            }
        }
        if !text.is_empty() {
            content.push(Content::Text(text));
        }
        (Message::assistant(content, usage), stop)
    }

    #[tokio::test]
    async fn wiremock_stream_assembles_assistant_message_with_usage() {
        let server = MockServer::start().await;
        mount_once(
            &server,
            sse_body(concat!(
                "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5}}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            )),
        )
        .await;

        let provider = provider_at(&format!("{}/v1", server.uri()), "sk-test");
        let messages = vec![Message::user_text("hi".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        let events = collect(&mut stream).await.unwrap();
        let (message, stop) = fold(&events);
        assert_eq!(stop, Some(StopReason::EndTurn));
        assert_eq!(message.content, vec![Content::Text("Hello world".into())]);
        assert_eq!(
            message.usage,
            Some(Usage {
                input: 12,
                output: 5,
                ..Default::default()
            })
        );
        crate::message::validate(&[Message::user_text("hi".into()), message]).unwrap();
    }

    #[tokio::test]
    async fn wiremock_stream_assembles_tool_call_message() {
        let (sse, _) = fixture_pair("text_and_tool");
        let server = MockServer::start().await;
        mount_once(&server, sse_body(&sse)).await;

        let provider = provider_at(&format!("{}/v1", server.uri()), "sk-test");
        let messages = vec![Message::user_text("ls".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        let events = collect(&mut stream).await.unwrap();
        let (message, stop) = fold(&events);
        assert_eq!(stop, Some(StopReason::ToolUse));
        assert_eq!(
            message.content,
            vec![
                Content::Text("I'll run ls".into()),
                Content::ToolUse {
                    id: "call_t".into(),
                    name: "developer__shell".into(),
                    input: json!({"command": "ls"}),
                },
            ]
        );
    }

    #[tokio::test]
    async fn wiremock_sends_auth_and_custom_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer sk-test",
            ))
            .and(wiremock::matchers::header("x-instagent-test", "yes"))
            .and(wiremock::matchers::header("accept", "text/event-stream"))
            .respond_with(sse_body("data: [DONE]\n\n"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_at(&format!("{}/v1", server.uri()), "sk-test");
        let messages = vec![Message::user_text("hi".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        collect(&mut stream).await.unwrap();
    }

    #[tokio::test]
    async fn wiremock_no_key_omits_authorization() {
        let server = MockServer::start().await;
        mount_once(&server, sse_body("data: [DONE]\n\n")).await;
        let provider = provider_at(&format!("{}/v1", server.uri()), "");
        let messages = vec![Message::user_text("hi".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        collect(&mut stream).await.unwrap();
        let received = server.received_requests().await.unwrap();
        assert!(!received[0].headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn wiremock_request_body_shape_and_quirks() {
        let server = MockServer::start().await;
        mount_once(&server, sse_body("data: [DONE]\n\n")).await;

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
                    input: json!({}),
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
        let provider = provider_at(&format!("{}/v1", server.uri()), "sk-test");
        let mut stream = provider.stream(request(&messages, &tools)).await.unwrap();
        collect(&mut stream).await.unwrap();

        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["tools"][0]["function"]["name"], "developer_shell");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be terse");
        assert_eq!(msgs[1], json!({"role": "user", "content": "run it"}));
        // 怪癖 1 + 2 + 4 的线上形态。
        assert!(msgs[2].get("content").unwrap().is_null());
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["name"],
            "developer_shell"
        );
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(msgs[3]["role"], "tool");
    }

    #[tokio::test]
    async fn wiremock_error_status_maps_to_provider_errors() {
        // 401 → Auth（不重试，一次请求）。
        let server = MockServer::start().await;
        mount_once(&server, ResponseTemplate::new(401).set_body_string("nope")).await;
        let provider = provider_at(&format!("{}/v1", server.uri()), "sk-bad");
        let messages = vec![Message::user_text("hi".into())];
        let err = provider
            .stream(request(&messages, &[]))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ProviderError::Auth), "{err:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        // 400 + 溢出文案 → ContextOverflow。
        let server = MockServer::start().await;
        mount_once(
            &server,
            ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
            ),
        )
        .await;
        let provider = provider_at(&format!("{}/v1", server.uri()), "sk-bad");
        let err = provider
            .stream(request(&messages, &[]))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ProviderError::ContextOverflow), "{err:?}");

        // 持续 500 → 重试耗尽后 Http(500)。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(4)
            .mount(&server)
            .await;
        let provider = provider_at(&format!("{}/v1", server.uri()), "sk-bad");
        let err = provider
            .stream(request(&messages, &[]))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&err, ProviderError::Http(500, m) if m == "boom"),
            "{err:?}"
        );

        // 429（Retry-After: 0）一次后成功。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0")
                    .set_body_string(r#"{"error":{"message":"slow down"}}"#),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_once(
            &server,
            sse_body(concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            )),
        )
        .await;
        let provider = provider_at(&format!("{}/v1", server.uri()), "sk-test");
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        let events = collect(&mut stream).await.unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: StopReason::EndTurn,
                },
            ]
        );
    }

    #[tokio::test]
    async fn wiremock_malformed_frame_maps_to_transport_via_http_layer() {
        // 经完整 HTTP 链路的坏帧（[DONE] 前）。
        let server = MockServer::start().await;
        mount_once(
            &server,
            sse_body("data: {\"choices\":[bad]}\n\ndata: [DONE]\n\n"),
        )
        .await;
        let provider = provider_at(&format!("{}/v1", server.uri()), "sk-test");
        let messages = vec![Message::user_text("hi".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        let err = stream.next().await.unwrap().unwrap_err();
        assert!(matches!(err, ProviderError::Transport(_)), "{err:?}");
        // 错误即终止：后续不再产出事件。
        assert!(stream.next().await.is_none());
    }

    // ---- 构造函数 ----

    #[test]
    fn new_validates_engine_base_url_and_reads_env_key() {
        let mut anthropic = def(Some("https://x.invalid/v1"));
        anthropic.engine = EngineKind::Anthropic;
        assert!(OpenAiProvider::new(&anthropic)
            .unwrap_err()
            .to_string()
            .contains("not an openai engine"));
        assert!(OpenAiProvider::new(&def(None))
            .unwrap_err()
            .to_string()
            .contains("missing base_url"));

        const VAR: &str = "INSTAGENT_TEST_09_API_KEY";
        let mut with_key = def(Some("https://x.invalid/v1"));
        with_key.api_key_env = Some(VAR.to_string());
        assert!(
            OpenAiProvider::new(&with_key).is_err(),
            "env 未设置必须报错"
        );

        std::env::set_var(VAR, "sk-from-env");
        let provider = OpenAiProvider::new(&with_key).unwrap();
        std::env::remove_var(VAR);
        assert_eq!(provider.api_key, "sk-from-env");
        assert_eq!(provider.name(), "test-openai");

        // 无 api_key_env：空密钥，不发 Authorization。
        let keyless = OpenAiProvider::new(&def(Some("http://localhost:11434/v1"))).unwrap();
        assert!(keyless.api_key.is_empty());
        assert!(!keyless.request_headers().contains_key("authorization"));
    }

    #[test]
    fn timeout_and_retry_defaults() {
        let provider = OpenAiProvider::new(&def(Some("https://x.invalid/v1"))).unwrap();
        assert_eq!(provider.http.timeout, DEFAULT_PROVIDER_TIMEOUT);
        let with_timeout = ProviderDef {
            timeout_seconds: Some(30),
            ..def(Some("https://x.invalid/v1"))
        };
        assert_eq!(
            OpenAiProvider::new(&with_timeout).unwrap().http.timeout,
            Duration::from_secs(30)
        );
    }

    #[tokio::test]
    async fn provider_object_safe_as_trait_object() {
        // registry（`10`）拿到的是 Arc<dyn Provider>，这里保证形状可用。
        let server = MockServer::start().await;
        mount_once(
            &server,
            sse_body(concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hey\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            )),
        )
        .await;
        let provider: Arc<dyn Provider> =
            Arc::new(provider_at(&format!("{}/v1", server.uri()), "sk-test"));
        let messages = vec![Message::user_text("hi".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        let events = collect(&mut stream).await.unwrap();
        assert!(matches!(
            events[0],
            StreamEvent::TextDelta(ref t) if t == "hey"
        ));
    }
}
