//! 03 · 工具执行前校验、续轮与取消（plan A01–A04）。
//!
//! 库行为测试（fake provider + 计数型 ToolSource + 同步通知控制取消点，
//! 不依赖在线 API key；子进程由库内统一走进程组 + `kill_on_drop`）：
//! - T1：非法 assistant 在副作用前被拒绝（零工具执行、零 PreToolUse/ToolStart
//!   副作用），历史保持可校验；malformed JSON 仍走已有流程；坏图片转成可诊
//!   断的错误结果；合法并行调用结果/事件与调用 ID 一一配对。
//!   异常终止/不完整响应在工具执行前整批拒绝，失败及取消结果均可 resume。
//! - T2：末尾 user 的会话经合并继续；七种中断/收尾各连跑两轮，第二轮成功且
//!   `validate`/落盘一致。
//! - T3：inventory、各 hook、maybe/force 压缩的等待均可取消（pending mock +
//!   就绪通知，无长 sleep，硬超时内返回）；摘要空/断流/取消不 rewrite，取消
//!   不误报成截断。overflow 仅重试一次、只读并发上限、`STOP_BLOCK_LIMIT` 的
//!   既有语义由 `src/agent` 内单测锁定，这里不重复。

use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use futures::stream::BoxStream;
use futures::StreamExt;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use instagent::agent::compact;
use instagent::agent::Agent;
use instagent::agent::AgentCfg;
use instagent::agent::Event;
use instagent::agent::TurnResult;
use instagent::hooks::Hooks;
use instagent::message::Content;
use instagent::message::Message;
use instagent::message::Usage;
use instagent::message::INTERRUPTED_TEXT;
use instagent::message::SUMMARY_PREFIX;
use instagent::plugin::manifest::read_manifest;
use instagent::plugin::manifest::PLUGIN_SCHEMA_URL;
use instagent::plugin::Plugin;
use instagent::plugin::PluginSet;
use instagent::plugin::PluginSource;
use instagent::plugin::NAMESPACE;
use instagent::provider::Provider;
use instagent::provider::Request;
use instagent::provider::StopReason;
use instagent::provider::StreamEvent;
use instagent::session::Session;
use instagent::session::SessionHeader;
use instagent::tools::BuiltinTools;
use instagent::tools::ImageData;
use instagent::tools::Registry;
use instagent::tools::ToolCtx;
use instagent::tools::ToolOutput;
use instagent::tools::ToolSource;
use instagent::tools::ToolSpec;
use instagent::ProviderError;

/// 取消测试的硬超时：被取消的等待必须远早于此时限返回。
const HARD_TIMEOUT: Duration = Duration::from_secs(10);

static DATA_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---------------------------------------------------------------------------
// fake 件
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Scripted {
    Ok {
        events: Vec<StreamEvent>,
        cancel_at_end: Option<CancellationToken>,
    },
    Err(ProviderError),
}

fn done(input: u32, output: u32) -> StreamEvent {
    StreamEvent::Done {
        usage: Usage {
            input,
            output,
            ..Default::default()
        },
        stop_reason: StopReason::EndTurn,
    }
}

fn tool_events(id: &str, name: &str, raw_json: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: id.into(),
            name: name.into(),
        },
        StreamEvent::ToolUseDelta(raw_json.into()),
        StreamEvent::ToolUseEnd,
    ]
}

fn scripted(events: Vec<StreamEvent>) -> Scripted {
    Scripted::Ok {
        events,
        cancel_at_end: None,
    }
}

fn scripted_then_cancel(events: Vec<StreamEvent>, token: &CancellationToken) -> Scripted {
    Scripted::Ok {
        events,
        cancel_at_end: Some(token.clone()),
    }
}

struct MockProvider {
    script: tokio::sync::Mutex<VecDeque<Scripted>>,
    calls: AtomicUsize,
}

impl MockProvider {
    fn new(script: Vec<Scripted>) -> Arc<Self> {
        Arc::new(Self {
            script: tokio::sync::Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    async fn stream(
        &self,
        _req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let Some(step) = self.script.lock().await.pop_front() else {
            return Err(ProviderError::Transport("mock script exhausted".into()));
        };
        match step {
            Scripted::Err(e) => Err(e),
            Scripted::Ok {
                events,
                cancel_at_end,
            } => {
                let head = stream::iter(events.into_iter().map(Ok)).boxed();
                let tail: BoxStream<'static, Result<StreamEvent, ProviderError>> =
                    match cancel_at_end {
                        Some(token) => stream::once(async move {
                            token.cancel();
                            Ok(StreamEvent::TextDelta(String::new()))
                        })
                        .boxed(),
                        None => stream::empty().boxed(),
                    };
                Ok(head.chain(tail).boxed())
            }
        }
    }
}

/// `stream()` 进来就通知、然后永远 pending：卡住摘要的建流等待。
struct PendingStreamProvider {
    entered: Arc<Notify>,
}

#[async_trait]
impl Provider for PendingStreamProvider {
    fn name(&self) -> &str {
        "pending"
    }

    async fn stream(
        &self,
        _req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        self.entered.notify_one();
        std::future::pending::<()>().await;
        unreachable!("pending summary stream is only reclaimed by cancellation");
    }
}

/// 建流成功、吐一个 delta 后永远 pending：卡住摘要的折叠等待。
struct HangFoldProvider {
    entered: Arc<Notify>,
}

#[async_trait]
impl Provider for HangFoldProvider {
    fn name(&self) -> &str {
        "hang-fold"
    }

    async fn stream(
        &self,
        _req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        self.entered.notify_one();
        let folded = futures::stream::unfold(0u32, |n| async move {
            if n == 0 {
                Some((
                    Ok(StreamEvent::TextDelta("partial summary".to_string())),
                    1u32,
                ))
            } else {
                futures::future::pending::<()>().await;
                None
            }
        });
        Ok(folded.boxed())
    }
}

/// 计数型只读探针：每次 call +1，返回固定成功文本。
struct Probe {
    calls: AtomicUsize,
}

impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ToolSource for Probe {
    fn id(&self) -> &str {
        "test:probe"
    }

    async fn list(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "probe".into(),
            description: "counting read-only probe".into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: true,
        }]
    }

    async fn call(&self, _name: &str, _input: serde_json::Value, _ctx: &ToolCtx) -> ToolOutput {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ToolOutput::ok("probe-ok".to_string())
    }
}

/// 两个调用互相等待：串行会死锁，只有并行能通过。
struct PeerProbe {
    barrier: tokio::sync::Barrier,
}

#[async_trait]
impl ToolSource for PeerProbe {
    fn id(&self) -> &str {
        "test:barrier"
    }

    async fn list(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "needs_peer".into(),
            description: "waits for a peer call".into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: true,
        }]
    }

    async fn call(&self, _name: &str, input: serde_json::Value, _ctx: &ToolCtx) -> ToolOutput {
        self.barrier.wait().await;
        ToolOutput::ok(format!("met {}", input["tag"].as_str().unwrap_or_default()))
    }
}

/// 返回坏图片的来源：图片校验必须把它拦下，转成错误结果。
struct BadImage {
    calls: AtomicUsize,
    data: String,
    media_type: String,
}

impl BadImage {
    fn new(data: &str, media_type: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            data: data.to_string(),
            media_type: media_type.to_string(),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ToolSource for BadImage {
    fn id(&self) -> &str {
        "test:bad-image"
    }

    async fn list(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "look".into(),
            description: "returns an invalid image".into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: true,
        }]
    }

    async fn call(&self, _name: &str, _input: serde_json::Value, _ctx: &ToolCtx) -> ToolOutput {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ToolOutput {
            text: "Loaded image from stub".into(),
            is_error: false,
            image: Some(ImageData {
                data: self.data.clone(),
                media_type: self.media_type.clone(),
            }),
        }
    }
}

/// `list()` 进来就通知、然后永远 pending：卡住工具 inventory 等待。
struct PendingList {
    entered: Arc<Notify>,
}

#[async_trait]
impl ToolSource for PendingList {
    fn id(&self) -> &str {
        "test:pending-list"
    }

    async fn list(&self) -> Vec<ToolSpec> {
        self.entered.notify_one();
        std::future::pending::<()>().await;
        unreachable!("pending inventory is only reclaimed by cancellation");
    }

    async fn call(&self, _name: &str, _input: serde_json::Value, _ctx: &ToolCtx) -> ToolOutput {
        unreachable!("inventory never resolves, call is unreachable");
    }
}

// ---------------------------------------------------------------------------
// 装配辅助
// ---------------------------------------------------------------------------

fn test_agent(
    provider: Arc<dyn Provider>,
    sources: Vec<Arc<dyn ToolSource>>,
    hooks: Option<Hooks>,
    max_turns: u32,
    context_limit: u32,
) -> Agent {
    let mut registry = Registry::new();
    registry.register(Arc::new(BuiltinTools::new(None)));
    for source in sources {
        registry.register(source);
    }
    Agent {
        cfg: AgentCfg {
            model: "mock-model".into(),
            max_tokens: 1024,
            max_turns,
            context_limit,
            compaction_threshold: 0.8,
        },
        provider,
        tools: Arc::new(registry),
        hooks: hooks.map(Arc::new),
        mcp_instructions: Vec::new(),
        skill_lines: Vec::new(),
    }
}

/// 手工构造会话文件（直写 tempdir 路径，不碰 `INSTAGENT_DATA_DIR`）。
fn temp_session(dir: &Path) -> Session {
    let header = SessionHeader {
        id: "test".into(),
        created: 0,
        cwd: dir.to_path_buf(),
        provider: "mock".into(),
        model: "mock-model".into(),
    };
    let path = dir.join("test.jsonl");
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&header).unwrap()),
    )
    .unwrap();
    Session {
        header,
        messages: Vec::new(),
        path,
    }
}

async fn run(
    agent: &Agent,
    session: &mut Session,
    text: &str,
    cancel: CancellationToken,
) -> (instagent::Result<TurnResult>, Vec<Event>) {
    let (tx, mut rx) = mpsc::channel(8192);
    let result = tokio::time::timeout(
        HARD_TIMEOUT,
        agent.run_turn(session, text.to_string(), cancel, tx),
    )
    .await
    .expect("run_turn 必须在硬超时内返回");
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (result, events)
}

fn first_text(message: &Message) -> String {
    match &message.content[0] {
        Content::Text(text) => text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn all_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_results(message: &Message) -> Vec<(String, String, bool)> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.clone(), content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// 内存与落盘一致：`validate` 通过 + 主 JSONL 解析回的消息与内存逐条相等。
fn assert_persisted(session: &Session) {
    instagent::message::validate(&session.messages).unwrap();
    let raw = std::fs::read_to_string(&session.path).unwrap();
    let mut lines = raw.lines();
    let header: SessionHeader =
        serde_json::from_str(lines.next().expect("header 行")).expect("header 可解析");
    assert_eq!(header.id, session.header.id);
    let on_disk: Vec<Message> = lines
        .map(|line| serde_json::from_str(line).expect("消息行可解析"))
        .collect();
    assert_eq!(on_disk, session.messages, "落盘消息必须与内存一致");
}

/// 只在同步 Session 调用期间设置测试数据目录，返回 Result 后再由调用方断言。
fn with_data_dir<T>(dir: &Path, action: impl FnOnce() -> T) -> T {
    let _guard = DATA_DIR_LOCK.lock().unwrap();
    let previous = std::env::var_os("INSTAGENT_DATA_DIR");
    std::env::set_var("INSTAGENT_DATA_DIR", dir);
    let result = action();
    match previous {
        Some(value) => std::env::set_var("INSTAGENT_DATA_DIR", value),
        None => std::env::remove_var("INSTAGENT_DATA_DIR"),
    }
    result
}

fn assert_resumable(session: &Session) {
    assert_persisted(session);
    let data = TempDir::new().unwrap();
    let sessions = data.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let path = sessions.join(format!("{}.jsonl", session.header.id));
    std::fs::copy(&session.path, &path).unwrap();
    let before = std::fs::read(&path).unwrap();
    let resumed = with_data_dir(data.path(), || Session::resume(&session.header.id)).unwrap();
    assert_eq!(
        resumed.messages, session.messages,
        "恢复不得丢弃消息或配对结果"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "完整会话无需 salvage"
    );
}

/// 造 hooks 插件：每个事件一个独立脚本（走 `${PLUGIN_ROOT}` 展开）。
fn hook_fixture(dir: &Path, event_scripts: &[(&str, &str)]) -> (Hooks, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let root = dir.join("hookplug");
    std::fs::create_dir_all(root.join(NAMESPACE)).unwrap();
    std::fs::create_dir_all(root.join("payloads")).unwrap();
    std::fs::write(
        root.join("plugin.json"),
        format!(r#"{{"$schema":"{PLUGIN_SCHEMA_URL}","name":"hookplug","version":"1.0.0"}}"#),
    )
    .unwrap();
    let mut groups = String::new();
    for (index, (event, body)) in event_scripts.iter().enumerate() {
        let script = root.join(format!("hook-{index}.sh"));
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !groups.is_empty() {
            groups.push(',');
        }
        groups.push_str(&format!(
            r#""{event}":[{{"hooks":[{{"command":"${{PLUGIN_ROOT}}/hook-{index}.sh"}}]}}]"#
        ));
    }
    std::fs::write(
        root.join(NAMESPACE).join("hooks.json"),
        format!(r#"{{"hooks":{{{groups}}}}}"#),
    )
    .unwrap();

    let mut set = PluginSet::default();
    set.plugins.push(Plugin {
        manifest: read_manifest(&root).unwrap(),
        root: root.clone(),
        source: PluginSource::Extra,
    });
    (Hooks::load(&set).unwrap(), root.join("payloads"))
}

async fn wait_for(path: &Path) {
    tokio::time::timeout(HARD_TIMEOUT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("等待点久久未到：{}", path.display()));
}

/// 带使用量的压缩前置历史：`context_limit=1000` 下 `input=900` 必触发压缩。
fn compactable_session(dir: &Path) -> Session {
    let mut session = temp_session(dir);
    session
        .append(Message::user_text("first question".into()))
        .unwrap();
    session
        .append(Message::assistant(
            vec![Content::Text("a1".into())],
            Some(Usage {
                input: 900,
                output: 5,
                ..Default::default()
            }),
        ))
        .unwrap();
    session
}

fn file_bytes(session: &Session) -> Vec<u8> {
    std::fs::read(&session.path).unwrap()
}

fn drain(events: &mut mpsc::Receiver<Event>) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        out.push(event);
    }
    out
}

/// 取消器：等就绪标记出现就取消（同步通知，无长 sleep）。
fn spawn_canceller(ready: PathBuf, token: CancellationToken) {
    tokio::spawn(async move {
        wait_for(&ready).await;
        token.cancel();
    });
}

// ---------------------------------------------------------------------------
// T1 · 副作用前拒绝非法消息
// ---------------------------------------------------------------------------

fn counting_tool_hooks(dir: &Path) -> (Hooks, PathBuf) {
    hook_fixture(
        dir,
        &[
            (
                "PreToolUse",
                "echo ran >> \"$PLUGIN_ROOT/payloads/pre-count\"",
            ),
            (
                "PostToolUse",
                "echo ran >> \"$PLUGIN_ROOT/payloads/post-count\"",
            ),
            ("Stop", "echo ran >> \"$PLUGIN_ROOT/payloads/stop-count\""),
        ],
    )
}

fn side_effect_calls() -> Vec<StreamEvent> {
    let mut events = tool_events(
        "shell-call",
        "shell",
        r#"{"command":"echo ran >> shell-count"}"#,
    );
    events.extend(tool_events("probe-call", "probe", "{}"));
    events
}

fn assert_no_tool_side_effects(probe: &Probe, dir: &Path, payloads: &Path, events: &[Event]) {
    assert_eq!(probe.calls(), 0, "异常响应不得执行工具");
    assert!(
        !dir.join("shell-count").exists(),
        "完整 shell call 也不得执行"
    );
    for name in ["pre-count", "post-count", "stop-count"] {
        assert!(!payloads.join(name).exists(), "不得执行 {name} hook");
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::ToolStart { .. } | Event::ToolDone { .. })),
        "未执行的调用不得发工具开始/完成事件"
    );
}

/// 参数完整不代表响应正常：整批拒绝，配对落盘，不能由下一条成功响应掩盖失败。
#[tokio::test]
async fn abnormal_tool_responses_fail_before_any_side_effect_and_survive_resume() {
    for (stop_reason, pending_tool, reason) in [
        (Some(StopReason::MaxTokens), false, "MaxTokens"),
        (Some(StopReason::Other), false, "Other"),
        (None, false, "stream reached EOF before Done"),
        (
            Some(StopReason::ToolUse),
            true,
            "tool-use block ended before ToolUseEnd",
        ),
        (
            Some(StopReason::EndTurn),
            true,
            "tool-use block ended before ToolUseEnd",
        ),
        (None, true, "stream reached EOF before Done"),
    ] {
        let dir = TempDir::new().unwrap();
        let probe = Probe::new();
        let (hooks, payloads) = counting_tool_hooks(dir.path());
        let mut response = side_effect_calls();
        if pending_tool {
            response.push(StreamEvent::ToolUseStart {
                id: "partial-call".into(),
                name: "probe".into(),
            });
            response.push(StreamEvent::ToolUseDelta("{\"partial\":".into()));
        }
        if let Some(stop_reason) = stop_reason {
            response.push(StreamEvent::Done {
                usage: Usage::default(),
                stop_reason,
            });
        }
        let provider = MockProvider::new(vec![
            scripted(response),
            scripted(vec![StreamEvent::TextDelta("recovered".into()), done(2, 1)]),
        ]);
        let agent = test_agent(
            provider.clone(),
            vec![probe.clone()],
            Some(hooks),
            10,
            100_000,
        );
        let mut session = temp_session(dir.path());

        let (result, events) = run(&agent, &mut session, "go", CancellationToken::new()).await;
        let err = result.expect_err("异常工具响应必须立即失败");
        assert!(err.to_string().contains(reason), "{stop_reason:?}: {err:#}");
        assert_eq!(provider.calls(), 1, "不得自动请求下一条成功响应");
        assert_no_tool_side_effects(&probe, dir.path(), &payloads, &events);
        assert!(
            events.iter().any(|event| matches!(
                event, Event::Error(text) if text.contains(reason)
            )),
            "失败原因必须通过事件可见：{events:?}"
        );
        assert_eq!(session.messages.len(), 3);
        let calls = session.messages[1].tool_uses();
        let results = tool_results(&session.messages[2]);
        assert_eq!(calls.len(), if pending_tool { 3 } else { 2 });
        assert_eq!(results.len(), calls.len());
        for (call, (id, text, is_error)) in calls.iter().zip(&results) {
            assert_eq!(&call.id, id);
            assert!(*is_error);
            assert!(text.contains("tool was not executed"), "{text}");
            assert!(text.contains(reason), "{text}");
        }
        assert_resumable(&session);

        // 只有调用方另起一次 run_turn 才能继续；先前的失败结果保留。
        let (result, _) = run(&agent, &mut session, "retry", CancellationToken::new()).await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        assert_eq!(provider.calls(), 2);
        assert_eq!(tool_results(&session.messages[2]), results);
        assert_resumable(&session);
    }
}

#[tokio::test]
async fn normal_tool_stop_reasons_execute_and_pair_results() {
    for stop_reason in [StopReason::ToolUse, StopReason::EndTurn] {
        let dir = TempDir::new().unwrap();
        let probe = Probe::new();
        let (hooks, payloads) = counting_tool_hooks(dir.path());
        let mut response = side_effect_calls();
        response.push(StreamEvent::Done {
            usage: Usage::default(),
            stop_reason,
        });
        let provider = MockProvider::new(vec![
            scripted(response),
            scripted(vec![StreamEvent::TextDelta("finished".into()), done(2, 1)]),
        ]);
        let agent = test_agent(
            provider.clone(),
            vec![probe.clone()],
            Some(hooks),
            10,
            100_000,
        );
        let mut session = temp_session(dir.path());

        let (result, events) = run(&agent, &mut session, "go", CancellationToken::new()).await;
        assert_eq!(result.unwrap(), TurnResult::Done);
        assert_eq!(provider.calls(), 2);
        assert_eq!(probe.calls(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("shell-count")).unwrap(),
            "ran\n"
        );
        for (name, count) in [("pre-count", 2), ("post-count", 2), ("stop-count", 1)] {
            let log = std::fs::read_to_string(payloads.join(name)).unwrap();
            assert_eq!(log.lines().count(), count, "{name}");
        }
        let results = tool_results(&session.messages[2]);
        assert_eq!(results.len(), 2);
        for ((id, _, is_error), expected) in results.iter().zip(["shell-call", "probe-call"]) {
            assert_eq!(id, expected);
            assert!(!is_error);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event,
                        Event::ToolStart { id: event_id, .. } if event_id == id
                    ))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event,
                        Event::ToolDone { id: event_id, is_error: false, .. } if event_id == id
                    ))
                    .count(),
                1
            );
        }
        assert_resumable(&session);
    }
}

#[tokio::test]
async fn cancelled_abnormal_tool_responses_keep_interrupted_semantics() {
    for stop_reason in [Some(StopReason::MaxTokens), Some(StopReason::Other), None] {
        let dir = TempDir::new().unwrap();
        let probe = Probe::new();
        let token = CancellationToken::new();
        let (hooks, payloads) = counting_tool_hooks(dir.path());
        let mut response = side_effect_calls();
        if let Some(stop_reason) = stop_reason {
            response.push(StreamEvent::Done {
                usage: Usage::default(),
                stop_reason,
            });
        }
        let provider = MockProvider::new(vec![scripted_then_cancel(response, &token)]);
        let agent = test_agent(
            provider.clone(),
            vec![probe.clone()],
            Some(hooks),
            10,
            100_000,
        );
        let mut session = temp_session(dir.path());

        let (result, events) = run(&agent, &mut session, "go", token).await;
        assert_eq!(result.unwrap(), TurnResult::Interrupted);
        assert_eq!(provider.calls(), 1);
        assert_no_tool_side_effects(&probe, dir.path(), &payloads, &events);
        assert!(!events.iter().any(|event| matches!(event, Event::Error(_))));
        let results = tool_results(&session.messages[2]);
        assert_eq!(results.len(), 2);
        for ((id, text, is_error), expected) in results.iter().zip(["shell-call", "probe-call"]) {
            assert_eq!(id, expected);
            assert_eq!(text, INTERRUPTED_TEXT);
            assert!(*is_error);
        }
        assert_resumable(&session);
    }
}

/// 同一响应复用 ID：零工具执行、零 PreToolUse 副作用，历史可校验且可继续。
#[tokio::test]
async fn duplicate_tool_ids_are_rejected_before_any_side_effect() {
    let dir = TempDir::new().unwrap();
    let probe = Probe::new();
    let (hooks, payloads) = hook_fixture(
        dir.path(),
        &[("PreToolUse", "touch \"$PLUGIN_ROOT/payloads/pre-marker\"")],
    );
    let mut events = tool_events("t1", "probe", "{}");
    events.extend(tool_events("t1", "probe", "{}"));
    events.push(done(4, 2));
    let provider = MockProvider::new(vec![
        scripted(events),
        scripted(vec![StreamEvent::TextDelta("recovered".into()), done(2, 1)]),
    ]);
    let agent = test_agent(
        provider.clone(),
        vec![probe.clone() as Arc<dyn ToolSource>],
        Some(hooks),
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());

    let (result, events) = run(&agent, &mut session, "go", CancellationToken::new()).await;
    let err = result.expect_err("复用 ID 必须错出");
    assert!(err.to_string().contains("reuses tool use id"), "{err:#}");
    assert_eq!(probe.calls(), 0, "非法响应不得执行工具");
    assert!(
        !payloads.join("pre-marker").exists(),
        "PreToolUse hook 不得被触发"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ToolStart { .. })),
        "不得有 ToolStart 事件"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Error(text) if text.contains("reuses tool use id")
        )),
        "拒绝必须以 Error 事件可见"
    );
    // 只落了 user 输入；历史可校验；第二次照常成功。
    assert_eq!(session.messages.len(), 1);
    assert_persisted(&session);

    let (result, _) = run(&agent, &mut session, "retry", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_persisted(&session);
    assert_eq!(first_text(session.messages.last().unwrap()), "recovered");
}

/// 历史复用 ID：第二轮把已回答过的 ID 再发一遍，同样零副作用、可继续。
#[tokio::test]
async fn history_reused_tool_id_is_rejected_without_executing() {
    let dir = TempDir::new().unwrap();
    let probe = Probe::new();
    let provider = MockProvider::new(vec![
        scripted({
            let mut v = tool_events("t1", "probe", "{}");
            v.push(done(4, 2));
            v
        }),
        scripted(vec![
            StreamEvent::TextDelta("first-done".into()),
            done(2, 1),
        ]),
        scripted({
            let mut v = tool_events("t1", "probe", "{}");
            v.push(done(4, 2));
            v
        }),
        scripted(vec![
            StreamEvent::TextDelta("second-done".into()),
            done(2, 1),
        ]),
    ]);
    let agent = test_agent(
        provider.clone(),
        vec![probe.clone() as Arc<dyn ToolSource>],
        None,
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());

    let (result, _) = run(&agent, &mut session, "first", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_eq!(probe.calls(), 1);
    assert_persisted(&session);

    let (result, events) = run(&agent, &mut session, "evil", CancellationToken::new()).await;
    let err = result.expect_err("历史复用 ID 必须错出");
    assert!(err.to_string().contains("reuses tool use id"), "{err:#}");
    assert_eq!(probe.calls(), 1, "复用历史 ID 不得再次执行");
    assert!(
        !events.iter().any(|e| matches!(e, Event::ToolStart { .. })),
        "不得有 ToolStart 事件"
    );
    assert_persisted(&session);

    let (result, _) = run(&agent, &mut session, "retry", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_eq!(first_text(session.messages.last().unwrap()), "second-done");
    assert_persisted(&session);
}

/// 空 ID / 空 name：逐轮拒绝、零副作用，第三轮合法输入照常成功。
#[tokio::test]
async fn empty_tool_id_and_name_are_rejected_before_execution() {
    let dir = TempDir::new().unwrap();
    let probe = Probe::new();
    let provider = MockProvider::new(vec![
        scripted({
            let mut v = vec![StreamEvent::ToolUseStart {
                id: "".into(),
                name: "probe".into(),
            }];
            v.push(StreamEvent::ToolUseDelta("{}".into()));
            v.push(StreamEvent::ToolUseEnd);
            v.push(done(2, 1));
            v
        }),
        scripted({
            let mut v = vec![StreamEvent::ToolUseStart {
                id: "t9".into(),
                name: "".into(),
            }];
            v.push(StreamEvent::ToolUseDelta("{}".into()));
            v.push(StreamEvent::ToolUseEnd);
            v.push(done(2, 1));
            v
        }),
        scripted(vec![StreamEvent::TextDelta("fine".into()), done(2, 1)]),
    ]);
    let agent = test_agent(
        provider.clone(),
        vec![probe.clone() as Arc<dyn ToolSource>],
        None,
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());

    let (result, _) = run(&agent, &mut session, "empty id", CancellationToken::new()).await;
    let err = result.expect_err("空 ID 必须错出");
    assert!(err.to_string().contains("empty id"), "{err:#}");
    assert_eq!(probe.calls(), 0);
    assert_persisted(&session);

    let (result, _) = run(&agent, &mut session, "empty name", CancellationToken::new()).await;
    let err = result.expect_err("空 name 必须错出");
    assert!(err.to_string().contains("empty name"), "{err:#}");
    assert_eq!(probe.calls(), 0);
    assert_persisted(&session);

    let (result, _) = run(&agent, &mut session, "legal", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_eq!(first_text(session.messages.last().unwrap()), "fine");
    assert_persisted(&session);
}

/// 合法但 JSON 参数损坏：仍走已有 malformed 流程（不执行、有错误结果），可继续。
#[tokio::test]
async fn malformed_tool_json_still_gets_error_result_and_continues() {
    let dir = TempDir::new().unwrap();
    let probe = Probe::new();
    let provider = MockProvider::new(vec![
        scripted({
            let mut v = tool_events("t1", "probe", "{not json");
            v.push(StreamEvent::Done {
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
            });
            v
        }),
        scripted(vec![StreamEvent::TextDelta("sorry".into()), done(6, 1)]),
        scripted(vec![StreamEvent::TextDelta("next".into()), done(2, 1)]),
    ]);
    let agent = test_agent(
        provider.clone(),
        vec![probe.clone() as Arc<dyn ToolSource>],
        None,
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());

    let (result, events) = run(&agent, &mut session, "bad args", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_eq!(probe.calls(), 0, "malformed 调用不执行");
    assert!(
        !events.iter().any(|e| matches!(e, Event::ToolStart { .. })),
        "malformed 调用不该有 ToolStart"
    );
    let results = tool_results(&session.messages[2]);
    assert!(results[0].2);
    assert!(results[0].1.contains("JSON"), "{}", results[0].1);
    assert_resumable(&session);

    let (result, _) = run(&agent, &mut session, "again", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_persisted(&session);
}

/// 坏图片（坏 base64 / 非法 mime）：丢弃图片、转成可诊断的错误结果，不毒化会话。
#[tokio::test]
async fn invalid_tool_images_become_diagnosable_error_results() {
    let dir = TempDir::new().unwrap();
    let bad_b64 = BadImage::new("!!!not-base64!!!", "image/png");
    let bad_mime = BadImage::new("aGVsbG8=", "text/plain");
    let provider = MockProvider::new(vec![
        scripted({
            let mut v = tool_events("t1", "look", "{}");
            v.push(done(3, 1));
            v
        }),
        scripted(vec![
            StreamEvent::TextDelta("after bad b64".into()),
            done(2, 1),
        ]),
        scripted({
            let mut v = tool_events("t2", "look", "{}");
            v.push(done(3, 1));
            v
        }),
        scripted(vec![
            StreamEvent::TextDelta("after bad mime".into()),
            done(2, 1),
        ]),
    ]);
    let agent1 = test_agent(
        provider.clone(),
        vec![bad_b64.clone() as Arc<dyn ToolSource>],
        None,
        10,
        100_000,
    );
    let agent2 = test_agent(
        provider.clone(),
        vec![bad_mime.clone() as Arc<dyn ToolSource>],
        None,
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());

    let (result, _) = run(&agent1, &mut session, "look 1", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_eq!(bad_b64.calls(), 1);
    // 首轮占两次迭代（工具 + 文本），共 4 条：输入/工具答复/错误结果/文本。
    assert_eq!(session.messages.len(), 4);
    assert_eq!(session.messages[2].content.len(), 1, "坏图片不得附上");
    match &session.messages[2].content[0] {
        Content::ToolResult {
            content, is_error, ..
        } => {
            assert!(is_error, "坏图片必须转成错误结果：{content}");
            assert!(content.contains("[image omitted"), "{content}");
            assert!(!content.contains("not-base64"), "诊断不得回显图片数据");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    assert_persisted(&session);

    // 第二轮换非法 mime 的来源：同一条路，同样可诊断、可继续。
    // 末尾是 assistant 文本故正常追加：新输入/工具答复/错误结果/文本落在 [4..8]。
    let (result, _) = run(&agent2, &mut session, "look 2", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_eq!(bad_mime.calls(), 1);
    assert_eq!(session.messages.len(), 8);
    assert_eq!(first_text(&session.messages[4]), "look 2");
    assert_eq!(session.messages[6].content.len(), 1, "坏图片不得附上");
    match &session.messages[6].content[0] {
        Content::ToolResult {
            content, is_error, ..
        } => {
            assert!(is_error, "{content}");
            assert!(content.contains("media_type"), "{content}");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    assert_persisted(&session);
    assert_eq!(
        first_text(session.messages.last().unwrap()),
        "after bad mime"
    );
}

/// 合法并行工具：结果与事件都和调用 ID 一一配对。
#[tokio::test]
async fn parallel_tool_results_and_events_pair_with_call_ids() {
    let dir = TempDir::new().unwrap();
    let mut events = Vec::new();
    for (id, tag) in [("t1", "a"), ("t2", "b"), ("t3", "c")] {
        events.extend(tool_events(
            id,
            "needs_peer",
            &format!(r#"{{"tag":"{tag}"}}"#),
        ));
    }
    events.push(done(6, 3));
    let provider = MockProvider::new(vec![
        scripted(events),
        scripted(vec![StreamEvent::TextDelta("done".into()), done(2, 1)]),
    ]);
    let agent = test_agent(
        provider.clone(),
        vec![Arc::new(PeerProbe {
            barrier: tokio::sync::Barrier::new(3),
        }) as Arc<dyn ToolSource>],
        None,
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());

    let (result, events) = run(&agent, &mut session, "go", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    for id in ["t1", "t2", "t3"] {
        let starts = events
            .iter()
            .filter(|e| matches!(e, Event::ToolStart { id: eid, .. } if eid == id))
            .count();
        let dones = events
            .iter()
            .filter(|e| matches!(e, Event::ToolDone { id: eid, .. } if eid == id))
            .count();
        assert_eq!((starts, dones), (1, 1), "{id} 必须恰一对 Start/Done");
    }
    let results = tool_results(&session.messages[2]);
    let ids: Vec<&str> = results.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(ids, vec!["t1", "t2", "t3"]);
    assert_persisted(&session);
}

// ---------------------------------------------------------------------------
// T2 · 继续末尾为 user 的会话（七种收尾各连跑两轮）
// ---------------------------------------------------------------------------

/// 预取消零写入零 hook，第二轮照常成功。
#[tokio::test]
async fn pre_cancelled_turn_writes_nothing_then_next_turn_succeeds() {
    let dir = TempDir::new().unwrap();
    let provider = MockProvider::new(vec![scripted(vec![
        StreamEvent::TextDelta("hello".into()),
        done(2, 1),
    ])]);
    let agent = test_agent(provider.clone(), vec![], None, 10, 100_000);
    let mut session = temp_session(dir.path());

    let token = CancellationToken::new();
    token.cancel();
    let (result, _) = run(&agent, &mut session, "go", token).await;
    assert_eq!(result.unwrap(), TurnResult::Interrupted);
    assert_eq!(provider.calls(), 0);
    assert!(session.messages.is_empty(), "预取消不得落任何消息");
    let raw = std::fs::read_to_string(&session.path).unwrap();
    assert_eq!(raw.lines().count(), 1, "只有 header 行");

    let (result, _) = run(&agent, &mut session, "go", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_persisted(&session);
    assert_eq!(first_text(session.messages.last().unwrap()), "hello");
}

/// 流开始前失败：首轮错出但保留输入，次轮成功。
#[tokio::test]
async fn stream_start_failure_keeps_input_for_the_next_turn() {
    let dir = TempDir::new().unwrap();
    let provider = MockProvider::new(vec![
        Scripted::Err(ProviderError::Transport("boom".into())),
        scripted(vec![StreamEvent::TextDelta("recovered".into()), done(2, 1)]),
    ]);
    let agent = test_agent(provider.clone(), vec![], None, 10, 100_000);
    let mut session = temp_session(dir.path());

    let (result, events) = run(&agent, &mut session, "go", CancellationToken::new()).await;
    assert!(result.is_err(), "建流失败必须错出");
    assert!(
        events.iter().any(|e| matches!(e, Event::Error(_))),
        "失败必须以 Error 事件可见"
    );
    assert_eq!(session.messages.len(), 1, "输入保留、可继续");
    assert_persisted(&session);

    let (result, _) = run(&agent, &mut session, "retry", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    // 输入合并进同一条 user：旧输入不丢。
    assert!(all_text(&session.messages[0]).contains("go"));
    assert!(all_text(&session.messages[0]).contains("retry"));
    assert_persisted(&session);
}

/// 工具后取消：中断结果落盘，次轮合并继续。
#[tokio::test]
async fn cancel_after_tools_then_next_turn_continues() {
    let dir = TempDir::new().unwrap();
    let token = CancellationToken::new();
    let probe = Probe::new();
    let provider = MockProvider::new(vec![
        scripted_then_cancel(
            {
                let mut v = vec![StreamEvent::TextDelta("working".into())];
                v.extend(tool_events("t1", "probe", "{}"));
                v.push(done(4, 2));
                v
            },
            &token,
        ),
        scripted(vec![StreamEvent::TextDelta("recovered".into()), done(2, 1)]),
    ]);
    let agent = test_agent(
        provider.clone(),
        vec![probe.clone() as Arc<dyn ToolSource>],
        None,
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());

    let (result, _) = run(&agent, &mut session, "go", token).await;
    assert_eq!(result.unwrap(), TurnResult::Interrupted);
    assert_eq!(session.messages.len(), 3);
    let results = tool_results(&session.messages[2]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, INTERRUPTED_TEXT);
    assert!(results[0].2);
    assert_persisted(&session);

    let (result, _) = run(&agent, &mut session, "again", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_eq!(first_text(session.messages.last().unwrap()), "recovered");
    assert_persisted(&session);
}

/// MaxTurns 后可继续：首轮停在工具结果，次轮接着答。
#[tokio::test]
async fn max_turns_then_next_turn_continues() {
    let dir = TempDir::new().unwrap();
    let probe = Probe::new();
    let provider = MockProvider::new(vec![
        scripted({
            let mut v = tool_events("t1", "probe", "{}");
            v.push(done(4, 2));
            v
        }),
        scripted(vec![StreamEvent::TextDelta("continued".into()), done(2, 1)]),
    ]);
    let agent = test_agent(
        provider.clone(),
        vec![probe.clone() as Arc<dyn ToolSource>],
        None,
        1,
        100_000,
    );
    let mut session = temp_session(dir.path());

    let (result, _) = run(&agent, &mut session, "go", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::MaxTurns);
    assert_eq!(session.messages.len(), 3);
    assert_persisted(&session);

    let (result, _) = run(&agent, &mut session, "go on", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_eq!(first_text(session.messages.last().unwrap()), "continued");
    assert_persisted(&session);
}

/// Stop hook 在最后一轮阻止（MaxTurns 收尾带 nudge），次轮合并继续。
#[tokio::test]
async fn stop_hook_block_on_last_round_then_next_turn_continues() {
    let dir = TempDir::new().unwrap();
    let (hooks, _) = hook_fixture(dir.path(), &[("Stop", "echo keep going >&2\nexit 2")]);
    let provider = MockProvider::new(vec![
        scripted(vec![StreamEvent::TextDelta("a0".into()), done(1, 1)]),
        scripted(vec![StreamEvent::TextDelta("a1".into()), done(1, 1)]),
    ]);
    let blocked = test_agent(provider.clone(), vec![], Some(hooks), 1, 100_000);
    let mut session = temp_session(dir.path());

    let (result, _) = run(&blocked, &mut session, "first", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::MaxTurns);
    assert_eq!(session.messages.len(), 3, "user + assistant + nudge");
    assert!(
        first_text(&session.messages[2]).contains("Stop hook blocked"),
        "{}",
        first_text(&session.messages[2])
    );
    assert_persisted(&session);

    // 次轮换无 hook 的 agent：nudge 里合并新输入，照常结束。
    let plain = test_agent(provider.clone(), vec![], None, 10, 100_000);
    let (result, _) = run(&plain, &mut session, "second", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert!(all_text(&session.messages[2]).contains("Stop hook blocked"));
    assert!(all_text(&session.messages[2]).contains("second"));
    assert_eq!(first_text(session.messages.last().unwrap()), "a1");
    assert_persisted(&session);
}

/// 手动压缩后可继续：摘要保留，新输入并入摘要消息。
#[tokio::test]
async fn manual_compact_then_next_turn_continues() {
    let dir = TempDir::new().unwrap();
    let provider = MockProvider::new(vec![
        scripted(vec![StreamEvent::TextDelta("a1".into()), done(10, 5)]),
        scripted(vec![
            StreamEvent::TextDelta("## summary\nwe talked".into()),
            done(100, 30),
        ]),
        scripted(vec![StreamEvent::TextDelta("a2".into()), done(50, 2)]),
    ]);
    let agent = test_agent(provider.clone(), vec![], None, 10, 100_000);
    let mut session = temp_session(dir.path());

    let (result, _) = run(&agent, &mut session, "first", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);

    let (tx, mut rx) = mpsc::channel(8);
    compact::force(&agent, &mut session, &tx).await.unwrap();
    assert!(drain(&mut rx)
        .iter()
        .any(|e| matches!(e, Event::Compacted { .. })));
    assert_eq!(session.messages.len(), 1);
    assert!(first_text(&session.messages[0]).starts_with(SUMMARY_PREFIX));
    assert_persisted(&session);

    let (result, _) = run(&agent, &mut session, "second", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert!(first_text(&session.messages[0]).starts_with(SUMMARY_PREFIX));
    assert!(all_text(&session.messages[0]).contains("second"));
    assert_eq!(first_text(session.messages.last().unwrap()), "a2");
    assert_persisted(&session);
}

/// resume 待回答的 user：落盘后恢复再跑，两轮一致。
#[tokio::test]
async fn resume_pending_user_then_turn_succeeds() {
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let mut session = with_data_dir(data.path(), || {
        Session::create(cwd.path(), "mock", "mock-model")
    })
    .unwrap();
    session
        .append(Message::user_text("pending question".into()))
        .unwrap();
    let id = session.header.id.clone();
    let path = session.path.clone();

    let provider = MockProvider::new(vec![
        scripted(vec![StreamEvent::TextDelta("answered".into()), done(2, 1)]),
        scripted(vec![StreamEvent::TextDelta("again".into()), done(2, 1)]),
    ]);
    let agent = test_agent(provider.clone(), vec![], None, 10, 100_000);

    // 首轮把待回答 user 与新输入合并后回答。
    let (result, _) = run(&agent, &mut session, "follow-up", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert!(all_text(&session.messages[0]).contains("pending question"));
    assert!(all_text(&session.messages[0]).contains("follow-up"));

    // resume 与内存一致；次轮照常成功。
    let resumed = with_data_dir(data.path(), || Session::resume(&id)).unwrap();
    assert_eq!(resumed.path, path);
    assert_eq!(resumed.messages, session.messages);
    instagent::message::validate(&resumed.messages).unwrap();

    let (result, _) = run(&agent, &mut session, "one more", CancellationToken::new()).await;
    assert_eq!(result.unwrap(), TurnResult::Done);
    assert_persisted(&session);
}

// ---------------------------------------------------------------------------
// T3 · 取消覆盖等待阶段，摘要成功后才替换
// ---------------------------------------------------------------------------

/// 工具 inventory 等待可取消：provider 未被调用，输入保留。
#[tokio::test]
async fn cancel_during_tool_inventory_returns_promptly() {
    let dir = TempDir::new().unwrap();
    let entered = Arc::new(Notify::new());
    let provider = MockProvider::new(vec![scripted(vec![
        StreamEvent::TextDelta("unreached".into()),
        done(1, 1),
    ])]);
    let agent = test_agent(
        provider.clone(),
        vec![Arc::new(PendingList {
            entered: entered.clone(),
        }) as Arc<dyn ToolSource>],
        None,
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());
    let token = CancellationToken::new();
    let waiter = entered.clone();
    let canceller = token.clone();
    tokio::spawn(async move {
        waiter.notified().await;
        canceller.cancel();
    });

    let (result, _) = run(&agent, &mut session, "go", token).await;
    assert_eq!(result.unwrap(), TurnResult::Interrupted);
    assert_eq!(provider.calls(), 0, "inventory 未完成前不得建流");
    assert_eq!(session.messages.len(), 1, "输入保留、可继续");
    assert_persisted(&session);
}

/// UserPromptSubmit 等待可取消：零写入、子进程被回收。
#[tokio::test]
async fn cancel_during_user_prompt_hook_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let (hooks, payloads) = hook_fixture(
        dir.path(),
        &[(
            "UserPromptSubmit",
            "touch \"$PLUGIN_ROOT/payloads/ready-Prompt\"\nsleep 30\ntouch \"$PLUGIN_ROOT/payloads/done-Prompt\"",
        )],
    );
    let provider = MockProvider::new(vec![scripted(vec![
        StreamEvent::TextDelta("unreached".into()),
        done(1, 1),
    ])]);
    let agent = test_agent(provider.clone(), vec![], Some(hooks), 10, 100_000);
    let mut session = temp_session(dir.path());
    let token = CancellationToken::new();
    spawn_canceller(payloads.join("ready-Prompt"), token.clone());

    let (result, _) = run(&agent, &mut session, "go", token).await;
    assert_eq!(result.unwrap(), TurnResult::Interrupted);
    assert_eq!(provider.calls(), 0);
    assert!(session.messages.is_empty(), "hook 等待中取消不得写输入");
    let raw = std::fs::read_to_string(&session.path).unwrap();
    assert_eq!(raw.lines().count(), 1, "只有 header 行");
    assert!(
        !payloads.join("done-Prompt").exists(),
        "hook 子进程必须被回收（sleep 不得跑完）"
    );
}

/// PreToolUse 等待可取消：工具零执行，调用记 interrupted。
#[tokio::test]
async fn cancel_during_pre_tool_hook_executes_nothing() {
    let dir = TempDir::new().unwrap();
    let (hooks, payloads) = hook_fixture(
        dir.path(),
        &[(
            "PreToolUse",
            "touch \"$PLUGIN_ROOT/payloads/ready-Pre\"\nsleep 30\ntouch \"$PLUGIN_ROOT/payloads/done-Pre\"",
        )],
    );
    let probe = Probe::new();
    let provider = MockProvider::new(vec![scripted({
        let mut v = tool_events("t1", "probe", "{}");
        v.push(done(4, 2));
        v
    })]);
    let agent = test_agent(
        provider.clone(),
        vec![probe.clone() as Arc<dyn ToolSource>],
        Some(hooks),
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());
    let token = CancellationToken::new();
    spawn_canceller(payloads.join("ready-Pre"), token.clone());

    let (result, _) = run(&agent, &mut session, "go", token).await;
    assert_eq!(result.unwrap(), TurnResult::Interrupted);
    assert_eq!(probe.calls(), 0, "Pre 等待中取消不得执行工具");
    assert!(!payloads.join("done-Pre").exists(), "hook 子进程必须被回收");
    assert_eq!(session.messages.len(), 3);
    let results = tool_results(&session.messages[2]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, INTERRUPTED_TEXT);
    assert!(results[0].2);
    assert_persisted(&session);
}

/// PostToolUse 等待可取消：工具已执行，结果照常落盘。
#[tokio::test]
async fn cancel_during_post_tool_hook_keeps_tool_result() {
    let dir = TempDir::new().unwrap();
    let (hooks, payloads) = hook_fixture(
        dir.path(),
        &[(
            "PostToolUse",
            "touch \"$PLUGIN_ROOT/payloads/ready-Post\"\nsleep 30\ntouch \"$PLUGIN_ROOT/payloads/done-Post\"",
        )],
    );
    let probe = Probe::new();
    let provider = MockProvider::new(vec![scripted({
        let mut v = tool_events("t1", "probe", "{}");
        v.push(done(4, 2));
        v
    })]);
    let agent = test_agent(
        provider.clone(),
        vec![probe.clone() as Arc<dyn ToolSource>],
        Some(hooks),
        10,
        100_000,
    );
    let mut session = temp_session(dir.path());
    let token = CancellationToken::new();
    spawn_canceller(payloads.join("ready-Post"), token.clone());

    let (result, _) = run(&agent, &mut session, "go", token).await;
    assert_eq!(result.unwrap(), TurnResult::Interrupted);
    assert_eq!(probe.calls(), 1, "Post 等待时工具已经执行完");
    assert!(
        !payloads.join("done-Post").exists(),
        "hook 子进程必须被回收"
    );
    let results = tool_results(&session.messages[2]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "t1");
    assert!(results[0].1.contains("probe-ok"), "{}", results[0].1);
    assert!(!results[0].2);
    assert_persisted(&session);
}

/// Stop 等待可取消：不注入 nudge，按 Interrupted 收尾。
#[tokio::test]
async fn cancel_during_stop_hook_skips_nudge() {
    let dir = TempDir::new().unwrap();
    let (hooks, payloads) = hook_fixture(
        dir.path(),
        &[(
            "Stop",
            "touch \"$PLUGIN_ROOT/payloads/ready-Stop\"\nsleep 30\ntouch \"$PLUGIN_ROOT/payloads/done-Stop\"",
        )],
    );
    let provider = MockProvider::new(vec![scripted(vec![
        StreamEvent::TextDelta("a0".into()),
        done(1, 1),
    ])]);
    let agent = test_agent(provider.clone(), vec![], Some(hooks), 10, 100_000);
    let mut session = temp_session(dir.path());
    let token = CancellationToken::new();
    spawn_canceller(payloads.join("ready-Stop"), token.clone());

    let (result, _) = run(&agent, &mut session, "go", token).await;
    assert_eq!(result.unwrap(), TurnResult::Interrupted);
    assert!(
        !payloads.join("done-Stop").exists(),
        "hook 子进程必须被回收"
    );
    assert_eq!(session.messages.len(), 2, "只有输入 + 助手，不注入 nudge");
    assert_eq!(first_text(session.messages.last().unwrap()), "a0");
    assert_persisted(&session);
}

/// 压缩等待可取消：建流等待 / 折叠等待 / 预取消 / maybe 都不改文件、不发事件。
#[tokio::test]
async fn cancel_during_compaction_changes_nothing() {
    // 建流等待中取消。
    {
        let dir = TempDir::new().unwrap();
        let entered = Arc::new(Notify::new());
        let provider: Arc<dyn Provider> = Arc::new(PendingStreamProvider {
            entered: entered.clone(),
        });
        let agent = test_agent(provider, vec![], None, 10, 1000);
        let mut session = compactable_session(dir.path());
        let before = file_bytes(&session);
        let token = CancellationToken::new();
        let waiter = entered.clone();
        let canceller = token.clone();
        tokio::spawn(async move {
            waiter.notified().await;
            canceller.cancel();
        });

        let (tx, mut rx) = mpsc::channel(8);
        tokio::time::timeout(
            HARD_TIMEOUT,
            compact::force_cancelable(&agent, &mut session, &tx, &token),
        )
        .await
        .expect("取消必须在硬超时内返回")
        .expect("取消按 no-op 处理，不报失败");
        assert_eq!(file_bytes(&session), before, "取消不得改文件");
        assert_eq!(session.messages.len(), 2);
        assert!(drain(&mut rx).is_empty(), "取消不得发 Compacted/截断事件");
    }
    // 折叠等待中取消。
    {
        let dir = TempDir::new().unwrap();
        let entered = Arc::new(Notify::new());
        let provider: Arc<dyn Provider> = Arc::new(HangFoldProvider {
            entered: entered.clone(),
        });
        let agent = test_agent(provider, vec![], None, 10, 1000);
        let mut session = compactable_session(dir.path());
        let before = file_bytes(&session);
        let token = CancellationToken::new();
        let waiter = entered.clone();
        let canceller = token.clone();
        tokio::spawn(async move {
            waiter.notified().await;
            canceller.cancel();
        });

        let (tx, mut rx) = mpsc::channel(8);
        let compacted = tokio::time::timeout(
            HARD_TIMEOUT,
            compact::maybe_cancelable(&agent, &mut session, &tx, &token),
        )
        .await
        .expect("取消必须在硬超时内返回")
        .expect("取消按 no-op 处理");
        assert!(!compacted, "取消不得发生压缩");
        assert_eq!(file_bytes(&session), before, "取消不得改文件");
        assert!(drain(&mut rx).is_empty(), "取消不得发 Compacted/截断事件");
    }
    // 预取消：同步 no-op。
    {
        let dir = TempDir::new().unwrap();
        let provider = MockProvider::new(vec![]);
        let agent = test_agent(provider.clone(), vec![], None, 10, 1000);
        let mut session = compactable_session(dir.path());
        let before = file_bytes(&session);
        let token = CancellationToken::new();
        token.cancel();
        let (tx, mut rx) = mpsc::channel(8);
        compact::force_cancelable(&agent, &mut session, &tx, &token)
            .await
            .expect("预取消按 no-op 处理");
        assert_eq!(provider.calls(), 0);
        assert_eq!(file_bytes(&session), before);
        assert!(drain(&mut rx).is_empty());
    }
}

/// 摘要空文本 / 缺 Done / 提供方错误：不 rewrite，文件字节一致。
#[tokio::test]
async fn failed_summary_keeps_session_file_intact() {
    // 空文本。
    {
        let dir = TempDir::new().unwrap();
        let provider = MockProvider::new(vec![scripted(vec![done(10, 5)])]);
        let agent = test_agent(provider, vec![], None, 10, 1000);
        let mut session = compactable_session(dir.path());
        let before = file_bytes(&session);
        let (tx, mut rx) = mpsc::channel(8);
        let err = compact::force(&agent, &mut session, &tx).await.unwrap_err();
        assert!(err.to_string().contains("empty"), "{err:#}");
        assert_eq!(file_bytes(&session), before, "空摘要不得改文件");
        assert_eq!(session.messages.len(), 2);
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|e| matches!(e, Event::Compacted { .. })),
            "失败不得发 Compacted"
        );
    }
    // 缺 Done。
    {
        let dir = TempDir::new().unwrap();
        let provider = MockProvider::new(vec![scripted(vec![StreamEvent::TextDelta(
            "partial summary".into(),
        )])]);
        let agent = test_agent(provider, vec![], None, 10, 1000);
        let mut session = compactable_session(dir.path());
        let before = file_bytes(&session);
        let (tx, mut rx) = mpsc::channel(8);
        let err = compact::force(&agent, &mut session, &tx).await.unwrap_err();
        assert!(err.to_string().contains("without Done"), "{err:#}");
        assert_eq!(file_bytes(&session), before, "缺 Done 不得改文件");
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|e| matches!(e, Event::Compacted { .. })),
            "失败不得发 Compacted"
        );
    }
    // 提供方错误。
    {
        let dir = TempDir::new().unwrap();
        let provider =
            MockProvider::new(vec![Scripted::Err(ProviderError::Transport("down".into()))]);
        let agent = test_agent(provider, vec![], None, 10, 1000);
        let mut session = compactable_session(dir.path());
        let before = file_bytes(&session);
        let (tx, _) = mpsc::channel(8);
        assert!(compact::force(&agent, &mut session, &tx).await.is_err());
        assert_eq!(file_bytes(&session), before, "提供方错误不得改文件");
    }
}

/// 成功摘要照常替换并带 tail：锁定新 API 的成功路径。
#[tokio::test]
async fn successful_force_compaction_rewrites_with_tail() {
    let dir = TempDir::new().unwrap();
    let provider = MockProvider::new(vec![scripted(vec![
        StreamEvent::TextDelta("## summary\nwe talked".into()),
        done(100, 30),
    ])]);
    let agent = test_agent(provider, vec![], None, 10, 1000);
    let mut session = compactable_session(dir.path());
    // 补一条未回答 user 当 tail。
    session
        .append(Message::user_text("tail question".into()))
        .unwrap();

    let (tx, mut rx) = mpsc::channel(8);
    compact::force_cancelable(&agent, &mut session, &tx, &CancellationToken::new())
        .await
        .unwrap();
    let compacted = drain(&mut rx);
    assert!(
        compacted.iter().any(|e| matches!(
            e,
            Event::Compacted {
                before_tokens: 900,
                after_tokens: 30,
            }
        )),
        "{compacted:?}"
    );
    assert_eq!(session.messages.len(), 1);
    let text = first_text(&session.messages[0]);
    assert!(text.starts_with(SUMMARY_PREFIX), "{text}");
    assert!(text.contains("we talked"), "{text}");
    assert!(text.contains("tail question"), "{text}");
    assert_persisted(&session);
}
