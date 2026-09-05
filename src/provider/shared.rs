//! 引擎共享层（Q1–Q3 收敛）：错误转换、工具名 sanitize / 去重、空 arguments
//! 兜底、SSE 流驱动器与构造骨架。引擎只提供「事件 → 状态」回调与引擎特有
//! 声明；语义与共享抽象冲突时允许保留引擎特有代码（plan 风险条款）。
//! 现为 openai 引擎（含 proxy 内嵌的 openai 引擎）的公共件；历史上曾同时
//! 服务 anthropic 引擎（ADR 0001 移除）。

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Context;
use futures::stream;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;

use crate::error::ProviderError;
use crate::provider::http::HttpClient;
use crate::provider::http::SseEvent;
use crate::provider::EngineKind;
use crate::provider::ProviderDef;
use crate::provider::StreamEvent;
use crate::provider::DEFAULT_PROVIDER_TIMEOUT;
use crate::tools::ToolSpec;

/// OpenAI function name 长度上限（goose 用 128，OpenAI 文档口径 64，从 todo）。
pub const MAX_FUNCTION_NAME_LENGTH: usize = 64;

/// 怪癖 2：只允许 `[A-Za-z0-9_-]`，非法字符替换为 `_`，超长截断到 64
/// （goose formats/openai.rs:1918 sanitize_function_name 的规则，长度取 64）。
/// 幂等：sanitize(sanitize(x)) == sanitize(x)。
pub fn sanitize_function_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_FUNCTION_NAME_LENGTH)
        .collect();
    if sanitized.is_empty() {
        // 空名同样不合法（{1,64}），给一个稳定的占位名。
        "tool".to_string()
    } else {
        sanitized
    }
}

/// `format_tools` 的 sanitize + `seen` 去重循环：按原序返回
/// sanitize 后的名字，sanitize 后重名直接报错。
pub fn sanitized_tool_names(tools: &[ToolSpec]) -> crate::Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        let name = sanitize_function_name(&tool.name);
        anyhow::ensure!(
            seen.insert(name.clone()),
            "duplicate tool name after sanitize: {}",
            tool.name
        );
        out.push(name);
    }
    Ok(out)
}

/// 空（纯空白）arguments → `"{}"`（openai 怪癖 4 响应侧）。
pub fn arguments_or_empty(arguments: String) -> String {
    if arguments.trim().is_empty() {
        "{}".to_string()
    } else {
        arguments
    }
}

/// `anyhow::Error` → [`ProviderError`]：原本就是 ProviderError 的透传，
/// 其余降级为 Transport。
pub fn to_provider_error(err: anyhow::Error) -> ProviderError {
    match err.downcast::<ProviderError>() {
        Ok(pe) => pe,
        Err(err) => ProviderError::Transport(err.to_string()),
    }
}

/// 累积中的 tool call：openai 按 `delta.tool_calls[].index` 累积，
/// 流结束时整组 flush。
#[derive(Debug, Default)]
pub struct PendingCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

// ---------------------------------------------------------------------------
// 引擎构造 / 请求骨架（Q2）：new / request_headers / stream 的公共部分；
// 引擎只声明 EngineKind 与补充鉴权头。
// ---------------------------------------------------------------------------

/// 引擎种类名（小写，错误文案用）。
fn engine_kind_name(kind: EngineKind) -> &'static str {
    match kind {
        EngineKind::Openai => "openai",
        EngineKind::Proxy => "proxy",
    }
}

/// 共享构造骨架：校验 engine 种类与 base_url、按 `api_key_env` 读密钥、
/// 按 `timeout_seconds` 建 client。
pub fn engine_parts(def: &ProviderDef, kind: EngineKind) -> crate::Result<(String, HttpClient)> {
    anyhow::ensure!(
        def.engine == kind,
        "provider {} is not an {} engine",
        def.name,
        engine_kind_name(kind)
    );
    anyhow::ensure!(
        def.base_url.as_deref().is_some_and(|u| !u.is_empty()),
        "provider {} missing base_url",
        def.name
    );
    let api_key = match def.api_key_env.as_deref() {
        Some(var) => std::env::var(var)
            .with_context(|| format!("provider {} requires env var {var}", def.name))?,
        None => String::new(),
    };
    let timeout = Duration::from_secs(
        def.timeout_seconds
            .unwrap_or(DEFAULT_PROVIDER_TIMEOUT.as_secs()),
    );
    Ok((api_key, HttpClient::new(timeout)?))
}

/// 共享请求头骨架：def 自带 headers + 引擎补充的鉴权 / 版本头。
pub fn headers_with_auth(
    def: &ProviderDef,
    auth_headers: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let mut headers = def.headers.clone();
    headers.extend(auth_headers);
    headers
}

/// `base_url` 必填，缺失 → Transport。
pub fn require_base_url(def: &ProviderDef) -> Result<&str, ProviderError> {
    def.base_url
        .as_deref()
        .ok_or_else(|| ProviderError::Transport(format!("provider {} missing base_url", def.name)))
}

// ---------------------------------------------------------------------------
// SSE 流驱动层（Q1）：通用驱动器；
// 引擎只提供「事件 → 状态」回调（apply）与收尾钩子（finalize）。
// ---------------------------------------------------------------------------

/// 流状态：输出队列、按 index 累积的待发 tool、终止标记。
/// usage 口径与 finalize 语义留在引擎侧。
#[derive(Debug, Default)]
pub struct StreamState {
    pub out: VecDeque<Result<StreamEvent, ProviderError>>,
    pub tools: BTreeMap<i64, PendingCall>,
    /// 记录到的终止原因（openai `finish_reason`；空串不算，由引擎过滤）。
    pub stop: Option<String>,
    /// 已见到 `[DONE]` 终止标记。
    pub done: bool,
    pub ended: bool,
}

/// 引擎侧「事件 → 状态」回调 + 收尾钩子；驱动器负责弹队列、查 ended、
/// 传输错误转发与断流收尾。
pub trait StreamEngine: Send {
    fn out(&mut self) -> &mut VecDeque<Result<StreamEvent, ProviderError>>;
    fn ended(&mut self) -> &mut bool;
    /// 处理一条 SSE 事件；`Err` 以该错误终止流。
    fn apply(&mut self, ev: &SseEvent) -> Result<(), ProviderError>;
    /// `[DONE]` / EOF 收尾：`Ok` = 有完成信息（`[DONE]` 或非空
    /// finish_reason），已整组 flush 并发出 Done（实现须自行置 ended：
    /// `[DONE]` 通常经 `apply` 触发本钩子）；`Err` = EOF 但无任何完成
    /// 信息，按断流报错，不得把待发工具 flush 成可执行调用。
    fn finalize(&mut self) -> Result<(), ProviderError>;
}

/// 空 data 跳过 + JSON parse；畸形块 → `Transport("malformed SSE chunk …")`。
/// 错误摘要有界（截断 + `sk-…` redact，todo 08 / S18）：坏输入不制造巨大日志。
pub fn parse_chunk(ev: &SseEvent) -> Result<Option<Value>, ProviderError> {
    let data = ev.data.trim();
    if data.is_empty() {
        return Ok(None);
    }
    serde_json::from_str::<Value>(data)
        .map(Some)
        .map_err(|err| {
            let summary = crate::provider::http::redact_secret_tokens(
                &crate::provider::http::summarize(data, 200),
            );
            ProviderError::Transport(format!("malformed SSE chunk {summary:?}: {err}"))
        })
}

/// 共享 SSE → StreamEvent 驱动器：弹队列 → 查 ended → 取 SSE → 引擎 apply；
/// 传输错误 / 畸形帧以错误终止流；EOF 调引擎 `finalize` 区分「正常完成」
/// （`[DONE]` 已见或非空 finish_reason 已收）与「断流」（无任何完成信息 →
/// 结构化错误，待发工具不成为可执行调用）。
pub fn sse_to_stream_events<E: StreamEngine + 'static>(
    engine: E,
    events: BoxStream<'static, crate::Result<SseEvent>>,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
    struct Driver<E> {
        events: BoxStream<'static, crate::Result<SseEvent>>,
        engine: E,
    }
    stream::unfold(Driver { events, engine }, |mut d| async move {
        loop {
            if let Some(item) = d.engine.out().pop_front() {
                return Some((item, d));
            }
            if *d.engine.ended() {
                return None;
            }
            match d.events.next().await {
                Some(Ok(ev)) => {
                    if let Err(err) = d.engine.apply(&ev) {
                        d.engine.out().push_back(Err(err));
                        *d.engine.ended() = true;
                    }
                }
                Some(Err(err)) => {
                    d.engine.out().push_back(Err(to_provider_error(err)));
                    *d.engine.ended() = true;
                }
                // EOF 收尾：finalize 返回 Err = 断流（无完成信息），
                // 把错误交给消费者；正常完成时引擎已自行置 ended。
                None => {
                    if let Err(err) = d.engine.finalize() {
                        d.engine.out().push_back(Err(err));
                    }
                    *d.engine.ended() = true;
                }
            }
        }
    })
    .boxed()
}

// ---------------------------------------------------------------------------
// 驱动器终止语义测试（T2）：[DONE]、finish_reason EOF、断流三分支。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Usage;
    use crate::provider::http::is_done;
    use crate::provider::StopReason;
    use futures::stream as fstream;
    use futures::StreamExt;

    /// 测试引擎：`data: tool` 记一个待发工具，`data: finish` 记非空
    /// finish_reason；finalize 与 openai 引擎同构——有完成信息才
    /// flush + Done，否则报断流。
    #[derive(Default)]
    struct MockEngine {
        st: StreamState,
    }

    impl StreamEngine for MockEngine {
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
            match ev.data.as_str() {
                "tool" => {
                    self.st.tools.insert(
                        0,
                        PendingCall {
                            id: "id".into(),
                            name: "t".into(),
                            arguments: "{}".into(),
                        },
                    );
                }
                "finish" => self.st.stop = Some("stop".into()),
                _ => {}
            }
            Ok(())
        }

        fn finalize(&mut self) -> Result<(), ProviderError> {
            if self.st.ended {
                return Ok(());
            }
            if self.st.stop.is_none() && !self.st.done {
                return Err(ProviderError::Transport(
                    "stream ended without completion signal".into(),
                ));
            }
            self.st.ended = true;
            for (_, call) in std::mem::take(&mut self.st.tools) {
                self.st.out.push_back(Ok(StreamEvent::ToolUseStart {
                    id: call.id,
                    name: call.name,
                }));
                self.st
                    .out
                    .push_back(Ok(StreamEvent::ToolUseDelta(call.arguments)));
                self.st.out.push_back(Ok(StreamEvent::ToolUseEnd));
            }
            self.st.out.push_back(Ok(StreamEvent::Done {
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            }));
            Ok(())
        }
    }

    fn events(datas: &[&str]) -> BoxStream<'static, crate::Result<SseEvent>> {
        fstream::iter(
            datas
                .iter()
                .map(|d| {
                    Ok(SseEvent {
                        event: None,
                        data: (*d).to_string(),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .boxed()
    }

    async fn drive(datas: &[&str]) -> Vec<Result<StreamEvent, ProviderError>> {
        let mut stream = sse_to_stream_events(MockEngine::default(), events(datas));
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn done_marker_finalizes_once_and_later_events_ignored() {
        let out = drive(&["tool", "[DONE]", "tool"]).await;
        // flush（Start/Delta/End）+ Done；`[DONE]` 后的帧不再消费，Done 只一次。
        assert_eq!(out.len(), 4);
        assert!(matches!(out[3], Ok(StreamEvent::Done { .. })));
        let dones = out
            .iter()
            .filter(|r| matches!(r, Ok(StreamEvent::Done { .. })))
            .count();
        assert_eq!(dones, 1);
    }

    #[tokio::test]
    async fn eof_without_completion_errors_and_withholds_pending_tools() {
        let out = drive(&["tool"]).await;
        // 断流：单个错误，待发工具不 flush、无 Done。
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], Err(ProviderError::Transport(m)) if m.contains("completion")),
            "{:?}",
            out[0]
        );
    }

    #[tokio::test]
    async fn eof_with_finish_reason_is_compatible() {
        let out = drive(&["tool", "finish"]).await;
        assert_eq!(out.len(), 4);
        assert!(matches!(out[3], Ok(StreamEvent::Done { .. })));
    }

    #[tokio::test]
    async fn transport_error_ends_stream_without_finalize() {
        let items: Vec<crate::Result<SseEvent>> = vec![
            Ok(SseEvent {
                event: None,
                data: "tool".into(),
            }),
            Err(anyhow::Error::new(ProviderError::Transport("boom".into()))),
        ];
        let mut stream = sse_to_stream_events(MockEngine::default(), fstream::iter(items).boxed());
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        // 传输错误直接冒泡：不 flush、不 finalize。
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], Err(ProviderError::Transport(m)) if m == "boom"),
            "{:?}",
            out[0]
        );
    }
}

// ---------------------------------------------------------------------------
// 测试脚手架（Q3 收敛）：引擎测试只保留引擎种类 / 模型 / 路径等参数差异，
// 重复骨架集中在这里。
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod testutil {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use futures::stream as fstream;
    use futures::stream::BoxStream;
    use futures::StreamExt;
    use serde_json::json;
    use serde_json::Value;
    use wiremock::matchers::method;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    use crate::error::ProviderError;
    use crate::message::Message;
    use crate::provider::http::HttpClient;
    use crate::provider::http::RetryPolicy;
    use crate::provider::http::SseEvent;
    use crate::provider::http::SseParser;
    use crate::provider::EngineKind;
    use crate::provider::ProviderDef;
    use crate::provider::Request;
    use crate::provider::StreamEvent;
    use crate::tools::ToolSpec;

    /// 小退避策略（避免 1s 首退拖慢 wiremock 用例）。
    pub fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            max_retries: crate::provider::http::MAX_RETRIES,
            initial_backoff: Duration::from_millis(1),
            factor: crate::provider::http::BACKOFF_FACTOR,
            cap: Duration::from_millis(20),
        }
    }

    /// 最小 [`ProviderDef`]：只给名字 / 引擎 / base_url。
    pub fn def(name: &str, engine: EngineKind, base_url: Option<&str>) -> ProviderDef {
        ProviderDef {
            name: name.into(),
            engine,
            display_name: None,
            description: None,
            api_key_env: None,
            base_url: base_url.map(str::to_string),
            headers: BTreeMap::new(),
            timeout_seconds: None,
            models: vec![],
            proxy: None,
        }
    }

    /// 测试 provider 构造骨架：自定义头 `x-instagent-test` + 短超时快退避
    /// client；具体引擎结构体由测试侧拼装，引擎差异只剩结构体字面量。
    pub fn provider_parts(
        name: &str,
        engine: EngineKind,
        base_url: &str,
    ) -> (ProviderDef, HttpClient) {
        let mut headers = BTreeMap::new();
        headers.insert("x-instagent-test".to_string(), "yes".to_string());
        let def = ProviderDef {
            headers,
            ..def(name, engine, Some(base_url))
        };
        let http = HttpClient::new(Duration::from_secs(5))
            .unwrap()
            .with_retry(fast_retry());
        (def, http)
    }

    pub fn request<'a>(
        model: &'a str,
        max_tokens: u32,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
    ) -> Request<'a> {
        Request {
            model,
            system: "be terse",
            messages,
            tools,
            max_tokens,
            temperature: Some(0.5),
        }
    }

    pub async fn collect(
        stream: &mut BoxStream<'static, Result<StreamEvent, ProviderError>>,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.push(ev?);
        }
        Ok(out)
    }

    /// 200 + `text/event-stream` 的 SSE 响应模板。
    pub fn sse_body(text: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(text)
    }

    pub async fn mount_once(server: &MockServer, path: &str, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(wiremock::matchers::path(path))
            .respond_with(response)
            .mount(server)
            .await;
    }

    /// [`StreamEvent`] → 对照 JSON（fixture 期望文件口径）。
    pub fn event_to_json(ev: &StreamEvent) -> Value {
        match ev {
            StreamEvent::TextDelta(text) => json!({"kind": "text_delta", "text": text}),
            StreamEvent::ToolUseStart { id, name } => {
                json!({"kind": "tool_use_start", "id": id, "name": name})
            }
            StreamEvent::ToolUseDelta(arguments) => {
                json!({"kind": "tool_use_delta", "arguments": arguments})
            }
            StreamEvent::ToolUseEnd => json!({"kind": "tool_use_end"}),
            StreamEvent::Done { usage, stop_reason } => {
                json!({"kind": "done", "usage": usage, "stop_reason": stop_reason})
            }
        }
    }

    /// 原始 SSE 文本 → SseParser → 引擎驱动层 → StreamEvent 列表（不经网络）；
    /// `wrap` 传引擎自己的 `sse_to_stream_events`。
    pub async fn run_sse<F>(text: &str, wrap: F) -> Vec<StreamEvent>
    where
        F: FnOnce(
            BoxStream<'static, crate::Result<SseEvent>>,
        ) -> BoxStream<'static, Result<StreamEvent, ProviderError>>,
    {
        let mut parser = SseParser::default();
        let events = parser.feed(text).expect("fixture SSE 字节合法");
        assert!(parser.buffer.trim().is_empty(), "fixture 必须以空行结尾");
        let input: BoxStream<'static, crate::Result<SseEvent>> =
            fstream::iter(events.into_iter().map(Ok)).boxed();
        let mut stream = wrap(input);
        collect(&mut stream).await.expect("fixture stream ok")
    }

    /// `tests/fixtures/{kind_dir}/{name}.sse` + `.expected.json` 对照对。
    pub fn fixture_pair(kind_dir: &str, name: &str) -> (String, Value) {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
        let sse = std::fs::read_to_string(format!("{dir}{kind_dir}/{name}.sse")).expect(name);
        let expected: Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{dir}{kind_dir}/{name}.expected.json")).expect(name),
        )
        .expect("expected json");
        (sse, expected)
    }
}
