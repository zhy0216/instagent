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
//!    （`sanitize_function_name`，goose formats/openai.rs:1918，长度上限取 64）；
//! 3. tool 消息必须紧跟含对应 tool_calls 的 assistant 消息
//!    （`format_messages`：user 消息里的 ToolResult 先于同消息的文本输出，
//!    会话不变量（`02`）保证 assistant→results 相邻）；
//! 4. arguments 为空串时当 `{}`（请求侧 `tool_arguments`；响应侧在 `finalize`
//!    flush 时兜底）。
//!
//! 流式组装说明：`StreamEvent::ToolUseDelta` 不带 id，并行 tool_calls 的
//! arguments 又会按 index 交错到达，所以 tool 事件在正常结束（`[DONE]` /
//! 非空 finish_reason 的 EOF）时按 index 升序整组 flush（Start→Delta→End），
//! 文本 delta 即时透传；无任何完成信息的 EOF 按断流报错，不 flush 待发工具
//! （完整参数也不自动执行）。构造入参 = provider JSON 定义，供 `10` registry 调用。
//!
//! 图片请求预算（todo 13 / S15 最小行为）：每次请求对历史图片先去重
//! （相同内容只内嵌首次出现），解码字节总和仍超 [`REQUEST_IMAGE_BUDGET`]
//! 时从最旧开始淘汰（生命周期淘汰、保留最新），被淘汰 / 去重的位置替换为
//! 可操作的重读提示。会话侧的新图拒绝见 [`crate::agent::SESSION_IMAGE_BUDGET`]；
//! 不做 RM2 的 blob/reference 存储，不引入新 decoder。

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
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

// ---------------------------------------------------------------------------
// transport 骨架：构造 / 请求头 / 流入口（HTTP、SSE 驱动与重试复用共享层）
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct OpenAiProvider {
    pub def: ProviderDef,
    /// 从 `def.api_key_env` 指定的环境变量读取；无 `api_key_env` 时为空串。
    /// 原始密钥永不进日志：[`Debug`] 实现对其 redact（ADR 0003 D1）。
    pub api_key: String,
    pub http: HttpClient,
}

/// 手写 Debug：`api_key` 只打印 `<redacted>`，密钥不进日志 / 错误输出
/// （ADR 0003 D1）。
impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("def", &self.def)
            .field("api_key", &"<redacted>")
            .field("http", &self.http)
            .finish()
    }
}

impl OpenAiProvider {
    /// 校验 def（engine=openai、base_url 必填），按 `api_key_env` 读密钥，
    /// 按 `timeout_seconds` 建 client（构造骨架在共享层 `engine_parts`）。
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
        let mut req = req;
        clamp_max_tokens(&self.def, &mut req);
        // 统一消息边界（S8）：wire 前严格校验整个会话；proxy 引擎经由本方法
        // 继承同一校验。错误带消息/block 索引与约束，不回显消息原文。
        crate::message::validate(req.messages).map_err(to_provider_error)?;
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
// request mapping：请求体 / 消息 / 工具 / 图片预算
// ---------------------------------------------------------------------------

/// model 表里的 `max_tokens` 是输出上限：请求超过时收敛到该值（todo 08 / S9，
/// 字段定义见 [`crate::provider::ModelDef`]）。
fn clamp_max_tokens(def: &ProviderDef, req: &mut Request<'_>) {
    if let Some(cap) = def
        .models
        .iter()
        .find(|m| m.name == req.model)
        .and_then(|m| m.max_tokens)
    {
        req.max_tokens = req.max_tokens.min(cap);
    }
}

/// 单请求图片总字节预算（todo 13 / S15）：一次请求内嵌全部图片的解码字节
/// 上限；先对重复图片去重，超限按生命周期淘汰（最旧优先）。会话侧预算见
/// [`crate::agent::SESSION_IMAGE_BUDGET`]。
pub const REQUEST_IMAGE_BUDGET: u64 = 32 * 1024 * 1024;

fn build_request_body(req: &Request<'_>) -> crate::Result<Value> {
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    messages.extend(format_messages(req.messages, REQUEST_IMAGE_BUDGET));

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

// request mapping 子单元：图片预算——去重 + 生命周期淘汰（todo 13 / S15）。

/// 单个图片槽位（消息下标, block 下标）在本请求内的处置（todo 13）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImagePlan {
    /// 正常内嵌（该内容在本请求内的首次出现）。
    Embed,
    /// 与请求内更早的图片内容相同，去重（替换为提示文本）。
    Duplicate,
    /// 预算超限被淘汰（替换为可重读提示文本）。
    Evicted,
}

/// 去重指纹：FNV-1a 64（media_type + data）。碰撞只影响去重判定（概率极
/// 低），不影响字节记账，也不触碰会话数据。
fn image_fingerprint(image: &crate::tools::ImageData) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let bytes = image
        .media_type
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0u8))
        .chain(image.data.as_bytes());
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 图片预算规划（todo 13 / S15 最小行为）：相同内容只内嵌首次出现（去重）；
/// 解码字节总和仍超 `budget` 时，从最旧开始淘汰、保留最新（生命周期淘汰）。
/// 被淘汰内容的所有出现位置都标为 Evicted。纯函数，可用小预算测试；生产
/// 接线用 [`REQUEST_IMAGE_BUDGET`]。
fn plan_images(messages: &[Message], budget: u64) -> HashMap<(usize, usize), ImagePlan> {
    // (槽位, 指纹, 解码字节)，按出现顺序。
    let mut occurrences: Vec<((usize, usize), u64, u64)> = Vec::new();
    let mut first_slot: HashMap<u64, (usize, usize)> = HashMap::new();
    for (message_index, message) in messages.iter().enumerate() {
        for (block_index, block) in message.content.iter().enumerate() {
            let Content::Image(image) = block else {
                continue;
            };
            let fingerprint = image_fingerprint(image);
            first_slot
                .entry(fingerprint)
                .or_insert((message_index, block_index));
            occurrences.push((
                (message_index, block_index),
                fingerprint,
                image.decoded_bytes(),
            ));
        }
    }
    // 预算：canonical（首次）出现自新到旧保留，直至总额用尽。
    let mut embedded: HashSet<u64> = HashSet::new();
    let mut used = 0u64;
    for &(slot, fingerprint, bytes) in occurrences.iter().rev() {
        if first_slot.get(&fingerprint) != Some(&slot) {
            continue;
        }
        if used + bytes <= budget {
            used += bytes;
            embedded.insert(fingerprint);
        }
    }
    occurrences
        .into_iter()
        .map(|(slot, fingerprint, _)| {
            let decision = if !embedded.contains(&fingerprint) {
                ImagePlan::Evicted
            } else if first_slot[&fingerprint] == slot {
                ImagePlan::Embed
            } else {
                ImagePlan::Duplicate
            };
            (slot, decision)
        })
        .collect()
}

/// 去重槽位的替换文本（可操作：说明相同内容已在前文内嵌）。
const DUPLICATE_IMAGE_NOTE: &str =
    "[duplicate image omitted: identical image content is already embedded earlier in this request]";

/// 淘汰槽位的替换文本（可操作：超出哪个预算、如何重新取回）。
fn evicted_image_note(budget: u64) -> String {
    format!(
        "[image omitted: request image budget of {} MiB exceeded; re-read the file if you still need it]",
        budget / (1024 * 1024)
    )
}

/// [`Message`] 列表 → OpenAI `messages` 数组（怪癖 1 / 3 / 4 的请求侧）。
/// 图片按 [`plan_images`] 在 `image_budget` 内去重 / 淘汰。
fn format_messages(messages: &[Message], image_budget: u64) -> Vec<Value> {
    let image_plan = plan_images(messages, image_budget);
    let mut out = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
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
                        // 模型流不会产出图片，仅保穷举。
                        Content::Image(_) => {}
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
                let mut images = Vec::new();
                for (block_index, content) in message.content.iter().enumerate() {
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
                        // goose convert_image 的 OpenAI 形状：data URL。
                        Content::Image(img) => match image_plan[&(message_index, block_index)] {
                            ImagePlan::Embed => images.push(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", img.media_type, img.data),
                                }
                            })),
                            ImagePlan::Duplicate => {
                                images.push(json!({"type": "text", "text": DUPLICATE_IMAGE_NOTE}))
                            }
                            ImagePlan::Evicted => images.push(
                                json!({"type": "text", "text": evicted_image_note(image_budget)}),
                            ),
                        },
                    }
                }
                // 怪癖 3：tool 消息紧跟含 tool_calls 的 assistant 消息；
                // 同一条 user 消息里混有文本时，文本放在 results 之后。
                out.extend(results);
                if !images.is_empty() {
                    // OpenAI 的 tool 消息不能带图片：图片进 user 消息，
                    // content 改用 parts 数组（文本部分在前，图片部分在后）。
                    let mut parts: Vec<Value> = texts
                        .into_iter()
                        .map(|text| json!({"type": "text", "text": text}))
                        .collect();
                    parts.extend(images);
                    out.push(json!({"role": "user", "content": parts}));
                } else if !texts.is_empty() {
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
// stream state：SSE 事件流 → StreamEvent 组装（驱动器复用共享层
// [`crate::provider::shared::sse_to_stream_events`]）
// ---------------------------------------------------------------------------

/// 单响应文本累计预算（字节）：即时透传的 TextDelta 同样有界，超限以错误
/// 终止流，给出有界、无原始载荷的诊断（plan P03：响应文本独立有限预算；
/// 取与累计工具参数同量级，远低于会话单消息预算）。
pub const MAX_RESPONSE_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// 单响应累计工具参数预算（字节）：`delta.tool_calls[].function.arguments`
/// 跨 index 求和，超限以错误终止流，不静默截成另一份有效 JSON
/// （plan 预算默认 8 MiB，P03）。
pub const MAX_TOOL_ARGUMENTS_BYTES: usize = 8 * 1024 * 1024;

/// 单响应工具调用数上限（按 `index` 去重计数）：超限以错误终止流
/// （plan 预算默认 256，P03）。
pub const MAX_TOOL_CALLS_PER_RESPONSE: usize = 256;

/// openai 引擎流状态：共享 [`StreamState`] + usage 累积
/// （tool_calls 按 `delta.tool_calls[].index` 累积，第二版 §6 风险 1）。
#[derive(Debug, Default)]
struct OpenAiStreamState {
    st: StreamState,
    usage: Usage,
    /// 已透传的响应文本字节数（受 [`MAX_RESPONSE_TEXT_BYTES`] 约束）。
    text_bytes: usize,
    /// 已累积的工具参数字节数（受 [`MAX_TOOL_ARGUMENTS_BYTES`] 约束）。
    args_bytes: usize,
}

impl OpenAiStreamState {
    /// 单个流式 chunk：文本即时出 TextDelta，tool_calls 按 index 累积，
    /// usage / finish_reason 记录到最后（`Done` 在收尾钩子统一发）。
    /// 文本与参数超预算直接返回有界错误（不截断不断言有效 JSON）。
    fn apply_chunk(&mut self, chunk: &Value) -> Result<(), ProviderError> {
        if let Some(err) = stream_error_from_chunk(chunk) {
            return Err(err);
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
                if self.text_bytes.saturating_add(text.len()) > MAX_RESPONSE_TEXT_BYTES {
                    return Err(ProviderError::Transport(format!(
                        "response text exceeds budget of {MAX_RESPONSE_TEXT_BYTES} bytes; terminating stream"
                    )));
                }
                self.text_bytes += text.len();
                self.st
                    .out
                    .push_back(Ok(StreamEvent::TextDelta(text.to_string())));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            accumulate_tool_calls(&mut self.st.tools, &mut self.args_bytes, calls)?;
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
            self.st.done = true;
            return self.finalize();
        }
        match parse_chunk(ev)? {
            Some(chunk) => self.apply_chunk(&chunk),
            None => Ok(()),
        }
    }

    /// `[DONE]` / EOF 收尾：有完成信息（`[DONE]` 或非空 finish_reason）时
    /// tool 事件按 index 升序整组 flush，再发 `Done`（usage 保留，只发一次）；
    /// 无任何完成信息的 EOF 报断流错误，不 flush 待发工具（完整参数也不因
    /// EOF 自动执行），不发 `Done`。
    fn finalize(&mut self) -> Result<(), ProviderError> {
        if self.st.ended {
            return Ok(());
        }
        if self.st.stop.is_none() && !self.st.done {
            return Err(ProviderError::Transport(
                "stream ended without completion signal ([DONE] or finish_reason); terminating stream"
                    .into(),
            ));
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
        Ok(())
    }
}

fn sse_to_stream_events(
    events: BoxStream<'static, crate::Result<SseEvent>>,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
    crate::provider::shared::sse_to_stream_events(OpenAiStreamState::default(), events)
}

// ---------------------------------------------------------------------------
// SSE delta parser：并行 tool_calls 按 index 累积
// ---------------------------------------------------------------------------

/// SSE delta parser：`delta.tool_calls[]` 按 `index` 累积进待发工具表
/// （第二版 §6 风险 1）。`index` 缺省时退化为数组内位置；id / name 取首个
/// 非空值，arguments 增量拼接（可能缺省 / null）。调用数与累计参数受
/// [`MAX_TOOL_CALLS_PER_RESPONSE`] / [`MAX_TOOL_ARGUMENTS_BYTES`] 约束：
/// 超限返回有界、无载荷的错误并终止流，不静默丢弃增量（否则会拼出另一份
/// 有效 JSON）。
fn accumulate_tool_calls(
    tools: &mut BTreeMap<i64, PendingCall>,
    args_bytes: &mut usize,
    calls: &[Value],
) -> Result<(), ProviderError> {
    for (position, call) in calls.iter().enumerate() {
        let index = call
            .get("index")
            .and_then(Value::as_i64)
            .unwrap_or(position as i64);
        if !tools.contains_key(&index) && tools.len() >= MAX_TOOL_CALLS_PER_RESPONSE {
            return Err(ProviderError::Transport(format!(
                "tool call count exceeds budget of {MAX_TOOL_CALLS_PER_RESPONSE}; terminating stream"
            )));
        }
        let pending = tools.entry(index).or_default();
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
        // 先查预算再拼接：超限不缓冲、不截断，直接错误终止。
        if let Some(args) = function.get("arguments").and_then(Value::as_str) {
            if args_bytes.saturating_add(args.len()) > MAX_TOOL_ARGUMENTS_BYTES {
                return Err(ProviderError::Transport(format!(
                    "tool arguments exceed cumulative budget of {MAX_TOOL_ARGUMENTS_BYTES} bytes; terminating stream"
                )));
            }
            *args_bytes += args.len();
            pending.arguments.push_str(args);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// error/usage handling：流内错误帧、usage 映射、终止原因
// ---------------------------------------------------------------------------

/// 流内错误帧（`chunk.error`）→ Transport；错误摘要有界 + redact
/// （todo 08 / S18 / ADR 0003 D1）。
fn stream_error_from_chunk(chunk: &Value) -> Option<ProviderError> {
    let err = chunk.get("error")?;
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    let summary = crate::provider::http::redact_secret_tokens(&crate::provider::http::summarize(
        msg,
        crate::provider::http::ERROR_SUMMARY_CHARS,
    ));
    Some(ProviderError::Transport(format!(
        "provider error in stream: {summary}"
    )))
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
/// `cache_creation_input_tokens`→cache_write。u64→u32 用饱和转换：
/// 极大 usage 取 `u32::MAX`，不得回绕成小值绕过压缩阈值（P04）。
fn usage_from_chunk(chunk: &Value) -> Option<Usage> {
    let usage = chunk.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let token = |v: Option<&Value>| {
        v.and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .unwrap_or(0)
    };
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
        let spec = format_messages(&messages, REQUEST_IMAGE_BUDGET);
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
        let spec = format_messages(&messages, REQUEST_IMAGE_BUDGET);
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
        let spec = format_messages(&messages, REQUEST_IMAGE_BUDGET);
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

    // ---- 图片序列化（04） ----

    #[test]
    fn user_message_with_image_uses_content_parts_array() {
        let messages = vec![
            Message::user_text("look".into()),
            Message::assistant(
                vec![Content::ToolUse {
                    id: "c1".into(),
                    name: "read_image".into(),
                    input: json!({"path": "x.png"}),
                }],
                None,
            ),
            Message {
                role: Role::User,
                content: vec![
                    Content::ToolResult {
                        tool_use_id: "c1".into(),
                        content: "Loaded image".into(),
                        is_error: false,
                    },
                    Content::Text("what is this?".into()),
                    Content::Image(crate::tools::ImageData {
                        data: "iVBORw0KGgo=".into(),
                        media_type: "image/png".into(),
                    }),
                ],
                ts: 0,
                usage: None,
            },
        ];
        crate::message::validate(&messages).unwrap();
        let spec = format_messages(&messages, REQUEST_IMAGE_BUDGET);
        let roles: Vec<&str> = spec.iter().map(|m| m["role"].as_str().unwrap()).collect();
        // 怪癖 3 在有图时不被破坏：tool 仍紧跟含 tool_calls 的 assistant。
        assert_eq!(roles, vec!["user", "assistant", "tool", "user"]);
        assert_eq!(spec[2]["tool_call_id"], "c1");
        let parts = spec[3]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], json!({"type": "text", "text": "what is this?"}));
        assert_eq!(
            parts[1],
            json!({
                "type": "image_url",
                "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}
            })
        );
    }

    #[test]
    fn image_only_user_message_is_still_emitted() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![Content::Image(crate::tools::ImageData {
                data: "R0lGOD".into(),
                media_type: "image/gif".into(),
            })],
            ts: 0,
            usage: None,
        }];
        let spec = format_messages(&messages, REQUEST_IMAGE_BUDGET);
        assert_eq!(
            spec,
            vec![json!({
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {"url": "data:image/gif;base64,R0lGOD"}
                }]
            })]
        );
    }

    #[test]
    fn user_message_without_image_keeps_string_content() {
        // 回归：无 Image 时行为逐字不变（quirk3_* 之外再钉死形状）。
        let messages = vec![Message::user_text("plain".into())];
        let spec = format_messages(&messages, REQUEST_IMAGE_BUDGET);
        assert_eq!(spec, vec![json!({"role": "user", "content": "plain"})]);
    }

    // ---- 图片请求预算：去重 + 生命周期淘汰（todo 13 / S15） ----

    fn image_content(data: &str) -> Content {
        Content::Image(crate::tools::ImageData {
            data: data.to_string(),
            media_type: "image/png".to_string(),
        })
    }

    fn user_blocks(content: Vec<Content>) -> Message {
        Message {
            role: Role::User,
            content,
            ts: 0,
            usage: None,
        }
    }

    #[test]
    fn plan_images_dedupes_identical_content_to_first_occurrence() {
        let messages = vec![
            user_blocks(vec![image_content("AAA=")]),
            Message::assistant(vec![Content::Text("next".into())], None),
            user_blocks(vec![image_content("AAA="), image_content("AA==")]),
        ];
        let plan = plan_images(&messages, 1024);
        assert_eq!(plan[&(0, 0)], ImagePlan::Embed);
        assert_eq!(
            plan[&(2, 0)],
            ImagePlan::Duplicate,
            "相同内容只内嵌首次出现"
        );
        assert_eq!(plan[&(2, 1)], ImagePlan::Embed);
    }

    #[test]
    fn plan_images_evicts_oldest_first_when_over_budget() {
        // "AAA=" = 2 解码字节、"AA==" = 1 解码字节；预算 2 → 保最新。
        let messages = vec![
            user_blocks(vec![image_content("AAA=")]),
            Message::assistant(vec![Content::Text("next".into())], None),
            user_blocks(vec![image_content("AA==")]),
        ];
        let plan = plan_images(&messages, 2);
        assert_eq!(plan[&(0, 0)], ImagePlan::Evicted, "生命周期淘汰：最旧先出");
        assert_eq!(plan[&(2, 0)], ImagePlan::Embed);

        // 被删内容的后续重复出现同样淘汰（内容根本没有内嵌）。
        let with_dup = vec![
            user_blocks(vec![image_content("AAA=")]),
            Message::assistant(vec![Content::Text("next".into())], None),
            user_blocks(vec![image_content("AAA="), image_content("AA==")]),
        ];
        let plan = plan_images(&with_dup, 1);
        assert_eq!(plan[&(0, 0)], ImagePlan::Evicted);
        assert_eq!(plan[&(2, 0)], ImagePlan::Evicted);
        assert_eq!(plan[&(2, 1)], ImagePlan::Embed);

        // 预算充足：全部内嵌，无淘汰。
        let plan = plan_images(&with_dup, REQUEST_IMAGE_BUDGET);
        assert_eq!(plan[&(0, 0)], ImagePlan::Embed);
        assert_eq!(plan[&(2, 0)], ImagePlan::Duplicate);
        assert_eq!(plan[&(2, 1)], ImagePlan::Embed);
    }

    #[test]
    fn format_messages_replaces_evicted_and_duplicate_images_with_notes() {
        let messages = vec![
            Message::user_text("look".into()),
            Message::assistant(
                vec![Content::ToolUse {
                    id: "c1".into(),
                    name: "read_image".into(),
                    input: json!({"path": "x.png"}),
                }],
                None,
            ),
            user_blocks(vec![
                Content::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "Loaded image".into(),
                    is_error: false,
                },
                image_content("AAA="), // 2 字节：超预算 → 淘汰
                image_content("AAA="), // 重复且内容未内嵌 → 淘汰
                image_content("AA=="), // 1 字节：最新 → 保留
            ]),
        ];
        crate::message::validate(&messages).unwrap();
        let spec = format_messages(&messages, 1);
        let roles: Vec<&str> = spec.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "user"]);
        let parts = spec[3]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0]["type"], "text");
        assert!(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .contains("re-read the file"),
            "淘汰提示可操作：{}",
            parts[0]["text"]
        );
        assert_eq!(parts[1]["type"], "text");
        assert_eq!(
            parts[2],
            json!({
                "type": "image_url",
                "image_url": {"url": "data:image/png;base64,AA=="}
            }),
            "预算内保留最新图片"
        );
    }

    #[test]
    fn format_messages_dedupe_note_explains_identical_content() {
        let messages = vec![
            user_blocks(vec![image_content("AAA=")]),
            Message::assistant(vec![Content::Text("next".into())], None),
            user_blocks(vec![image_content("AAA=")]),
        ];
        let spec = format_messages(&messages, REQUEST_IMAGE_BUDGET);
        let parts = spec[2]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "text");
        assert!(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .contains("duplicate image omitted"),
            "{}",
            parts[0]["text"]
        );
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

    // ---- T2 · 完成/断流区分与资源边界 ----

    /// 原始 SSE 文本 → 含 Err 的完整产出（错误本身是最后一个元素；错误即终止）。
    async fn run_sse_results(text: &str) -> Vec<Result<StreamEvent, ProviderError>> {
        let mut parser = SseParser::default();
        let events = parser.feed(text).expect("用例 SSE 字节合法");
        let input: BoxStream<'static, crate::Result<SseEvent>> =
            fstream::iter(events.into_iter().map(Ok)).boxed();
        let mut stream = sse_to_stream_events(input);
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn fixture_finish_reason_without_done_is_compatible() {
        // fixture 末尾以已终止的 `: end` 注释行收尾而非空行：注释不产生事件，
        // 且避免新文件以空行结尾触发 `git diff --check`。
        let (sse, expected) = fixture_pair("finish_no_done");
        let got: Vec<Value> = run_sse(&sse).await.iter().map(event_to_json).collect();
        assert_eq!(Value::Array(got), expected);
    }

    #[tokio::test]
    async fn eof_text_without_completion_is_truncated_error() {
        let out = run_sse_results(
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
        )
        .await;
        // 无 finish_reason、无 [DONE]：单个断流错误，无 TextDelta 透传之外的事件、无 Done。
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].as_ref().unwrap(),
            &StreamEvent::TextDelta("partial".into())
        );
        assert!(
            matches!(&out[1], Err(ProviderError::Transport(m)) if m.contains("completion signal")),
            "{:?}",
            out[1]
        );
    }

    #[tokio::test]
    async fn eof_complete_tool_args_without_completion_never_executes() {
        // 参数已是完整 JSON，但无 finish_reason、无 [DONE]：不得 flush 成可执行调用。
        let delta = serde_json::to_string(&json!({
            "choices": [{
                "delta": {"tool_calls": [{
                    "index": 0, "id": "c1",
                    "function": {"name": "shell", "arguments": "{\"command\": \"ls\"}"}
                }]},
                "finish_reason": null
            }]
        }))
        .unwrap();
        let out = run_sse_results(&format!("data: {delta}\n\n")).await;
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], Err(ProviderError::Transport(m)) if m.contains("completion signal")),
            "{:?}",
            out[0]
        );
    }

    #[tokio::test]
    async fn done_without_finish_reason_flushes_and_keeps_usage() {
        // [DONE] 本身是完成信号：待发工具照常 flush，有效终止后的 usage 保留。
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        let events = run_sse(sse).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("hi".into()),
                StreamEvent::Done {
                    usage: Usage {
                        input: 7,
                        output: 3,
                        ..Default::default()
                    },
                    stop_reason: StopReason::Other,
                },
            ]
        );
    }

    #[tokio::test]
    async fn double_done_emits_done_once() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
            "data: [DONE]\n\n",
        );
        let events = run_sse(sse).await;
        // TextDelta + Done；第二个 [DONE] 不再产生 Done，EOF 也不再补发。
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1], StreamEvent::Done { .. }));
    }

    #[test]
    fn usage_huge_values_saturate_without_wrapping() {
        // 旧 `as u32` 会把 2^32 回绕成 0（绕过压缩阈值）；现在饱和到 u32::MAX。
        let chunk = json!({"usage": {
            "prompt_tokens": 4294967296u64,
            "completion_tokens": 1099511627776u64,
            "prompt_tokens_details": {"cached_tokens": 18446744073709551615u64},
            "cache_creation_input_tokens": 1u64,
        }});
        let usage = usage_from_chunk(&chunk).expect("usage object");
        assert_eq!(usage.input, u32::MAX);
        assert_eq!(usage.output, u32::MAX);
        assert_eq!(usage.cache_read, u32::MAX);
        assert_eq!(usage.cache_write, 1);
    }

    /// 单个 tool delta 事件骨架（不同 index，用于调用数边界）。
    fn tool_delta_event(index: i64) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":{index},\"id\":\"c{index}\",\"function\":{{\"name\":\"t\",\"arguments\":\"{{}}\"}}}}]}},\"finish_reason\":null}}]}}\n\n"
        )
    }

    #[tokio::test]
    async fn tool_call_count_at_budget_succeeds_and_over_fails() {
        // 恰好 256 个不同 index：按 index 升序 flush + Done。
        let mut sse: String = (0..MAX_TOOL_CALLS_PER_RESPONSE as i64)
            .map(tool_delta_event)
            .collect();
        sse.push_str(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
        );
        let events = run_sse(&sse).await;
        assert_eq!(events.len(), MAX_TOOL_CALLS_PER_RESPONSE * 3 + 1);
        assert!(
            matches!(&events[0], StreamEvent::ToolUseStart { id, .. } if id == "c0"),
            "{:?}",
            events[0]
        );
        assert!(matches!(events[events.len() - 1], StreamEvent::Done { .. }));

        // 第 257 个不同 index：错误终止，诊断有界、无 Done、不拼截断 JSON。
        let mut sse: String = (0..=MAX_TOOL_CALLS_PER_RESPONSE as i64)
            .map(tool_delta_event)
            .collect();
        sse.push_str("data: [DONE]\n\n");
        let out = run_sse_results(&sse).await;
        assert_eq!(out.len(), 1);
        let Err(ProviderError::Transport(msg)) = &out[0] else {
            panic!("expected Transport, got {:?}", out[0]);
        };
        assert!(msg.contains("budget"), "{msg}");
        assert!(msg.len() < 200, "{msg}");
    }

    #[tokio::test]
    async fn cumulative_tool_arguments_at_budget_succeeds_and_over_fails() {
        // 每事件 512 KiB 参数（< 1 MiB 单事件预算）；16 块恰好 8 MiB：成功。
        let big = "a".repeat(512 * 1024);
        let body = serde_json::to_string(&json!({
            "choices": [{
                "delta": {"tool_calls": [{
                    "index": 0, "id": "c0",
                    "function": {"name": "t", "arguments": big}
                }]},
                "finish_reason": null
            }]
        }))
        .unwrap();
        let mut sse: String = (0..16).map(|_| format!("data: {body}\n\n")).collect();
        sse.push_str(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
        );
        let events = run_sse(&sse).await;
        assert!(matches!(events[events.len() - 1], StreamEvent::Done { .. }));

        // 再多 1 字节即超限：单个错误终止，无 Done，诊断不带原始载荷。
        let small = serde_json::to_string(&json!({
            "choices": [{
                "delta": {"tool_calls": [{
                    "index": 0, "id": "c0",
                    "function": {"name": "t", "arguments": "b"}
                }]},
                "finish_reason": null
            }]
        }))
        .unwrap();
        sse = (0..16).map(|_| format!("data: {body}\n\n")).collect();
        sse.push_str(&format!("data: {small}\n\ndata: [DONE]\n\n"));
        let out = run_sse_results(&sse).await;
        assert_eq!(out.len(), 1);
        let Err(ProviderError::Transport(msg)) = &out[0] else {
            panic!("expected Transport, got {:?}", out[0]);
        };
        assert!(msg.contains("budget"), "{msg}");
        assert!(msg.len() < 200, "{msg}");
        assert!(!msg.contains("aaaa"), "{msg}");
    }

    #[tokio::test]
    async fn response_text_over_budget_fails_without_truncated_done() {
        // 每事件 900 KiB 文本（< 1 MiB 单事件预算）；10 块约 8.8 MiB：超限。
        let body = serde_json::to_string(&json!({
            "choices": [{
                "delta": {"content": "t".repeat(900 * 1024)},
                "finish_reason": null
            }]
        }))
        .unwrap();
        let sse: String = (0..10).map(|_| format!("data: {body}\n\n")).collect();
        let out = run_sse_results(&format!("{sse}data: [DONE]\n\n")).await;
        // 前 9 块已透传 TextDelta，第 10 块触发预算错误；无 Done、无截断 JSON。
        assert_eq!(out.len(), 10);
        assert!(
            out.iter()
                .all(|r| !matches!(r, Ok(StreamEvent::Done { .. }))),
            "{out:?}"
        );
        let Some(Err(ProviderError::Transport(msg))) = out.last() else {
            panic!("expected trailing Transport, got {:?}", out.last());
        };
        assert!(msg.contains("budget"), "{msg}");
        assert!(msg.len() < 200, "{msg}");
        assert!(!msg.contains("tttt"), "{msg}");
    }

    #[tokio::test]
    async fn in_stream_error_frame_surfaces_as_transport_error() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"upstream exploded\"}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut parser = SseParser::default();
        let events = parser.feed(sse).expect("用例 SSE 字节合法");
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
        let events = parser
            .feed("data: {not json}\n\n")
            .expect("用例 SSE 字节合法");
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
        let mut proxy = def(Some("https://x.invalid/v1"));
        proxy.engine = EngineKind::Proxy;
        assert!(OpenAiProvider::new(&proxy)
            .unwrap_err()
            .to_string()
            .contains("not an openai engine"));
        assert!(OpenAiProvider::new(&def(None))
            .unwrap_err()
            .to_string()
            .contains("missing base_url"));

        const VAR: &str = "INSTAGENT_TEST_09_API_KEY";
        std::env::remove_var(VAR);
        let mut with_key = def(Some("https://x.invalid/v1"));
        with_key.api_key_env = Some(VAR.to_string());
        // 契约（ADR 0003 D1）：声明了 api_key_env 而环境变量未设置 → 构造失败，
        // 错误带 provider 名与变量名，且不含任何密钥值。
        let err = OpenAiProvider::new(&with_key).unwrap_err().to_string();
        assert!(err.contains("test-openai"), "{err}");
        assert!(err.contains(VAR), "{err}");

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

    #[test]
    fn debug_never_prints_raw_api_key() {
        let provider = provider_at("https://x.invalid/v1", "sk-from-env-secret");
        let rendered = format!("{provider:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("sk-from-env-secret"), "{rendered}");
    }

    #[test]
    fn request_max_tokens_clamped_to_model_cap() {
        let (mut def, _) = tu::provider_parts("test-openai", EngineKind::Openai, "https://x/v1");
        def.models = vec![crate::provider::ModelDef {
            name: "gpt-4o".into(),
            context_limit: None,
            max_tokens: Some(16),
        }];
        let mut req = tu::request("gpt-4o", 1024, &[], &[]);
        clamp_max_tokens(&def, &mut req);
        assert_eq!(req.max_tokens, 16);
        // 请求值低于上限时不改；模型不在表里也不改。
        let mut req = tu::request("gpt-4o", 8, &[], &[]);
        clamp_max_tokens(&def, &mut req);
        assert_eq!(req.max_tokens, 8);
        let mut req = tu::request("unknown-model", 1024, &[], &[]);
        clamp_max_tokens(&def, &mut req);
        assert_eq!(req.max_tokens, 1024);
    }

    #[tokio::test]
    async fn wiremock_request_body_carries_clamped_max_tokens() {
        let server = MockServer::start().await;
        mount_once(&server, sse_body("data: [DONE]\n\n")).await;
        let (def, http) = tu::provider_parts(
            "test-openai",
            EngineKind::Openai,
            &format!("{}/v1", server.uri()),
        );
        let def = ProviderDef {
            models: vec![crate::provider::ModelDef {
                name: "gpt-4o".into(),
                context_limit: None,
                max_tokens: Some(16),
            }],
            ..def
        };
        let provider = OpenAiProvider {
            def,
            api_key: String::new(),
            http,
        };
        let messages = vec![Message::user_text("hi".into())];
        let mut stream = provider.stream(request(&messages, &[])).await.unwrap();
        collect(&mut stream).await.unwrap();
        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["max_tokens"], 16);
    }

    #[tokio::test]
    async fn in_stream_error_summary_is_bounded_and_redacted() {
        let sse = format!(
            "data: {{\"error\":{{\"message\":\"bad key sk-abcdef12345 {}\"}}}}\n\ndata: [DONE]\n\n",
            "y".repeat(2000)
        );
        let mut parser = SseParser::default();
        let events = parser.feed(&sse).expect("用例 SSE 字节合法");
        let input: BoxStream<'static, crate::Result<SseEvent>> =
            fstream::iter(events.into_iter().map(Ok)).boxed();
        let mut stream = sse_to_stream_events(input);
        let err = stream.next().await.unwrap().unwrap_err();
        let ProviderError::Transport(message) = &err else {
            panic!("expected Transport, got {err:?}")
        };
        assert!(message.contains("sk-[redacted]"), "{message}");
        assert!(!message.contains("sk-abcdef12345"), "{message}");
        assert!(message.chars().count() < 700, "{}", message.chars().count());
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
