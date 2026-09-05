//! 07 T1：Registry 一致快照、单飞刷新、代次失效、有界重试与同源去重。
//! 不依赖在线服务；并发交错用同步屏障/Notify 确定性控制，不用长 sleep。

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use serde_json::Value;
use tokio::sync::Barrier;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use instagent::tools::CallCapability;
use instagent::tools::Registry;
use instagent::tools::ToolCall;
use instagent::tools::ToolCtx;
use instagent::tools::ToolOutput;
use instagent::tools::ToolSource;
use instagent::tools::ToolSpec;

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: format!("test {name}"),
        input_schema: json!({"type": "object"}),
        read_only: false,
    }
}

fn ctx() -> ToolCtx {
    ToolCtx {
        cwd: std::env::temp_dir(),
        cancel: CancellationToken::new(),
    }
}

struct CountingSource {
    id: String,
    names: Mutex<Vec<String>>,
    enumerations: AtomicUsize,
}

impl CountingSource {
    fn new(id: &str, names: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_string(),
            names: Mutex::new(names.iter().map(|s| s.to_string()).collect()),
            enumerations: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ToolSource for CountingSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn list(&self) -> Vec<ToolSpec> {
        self.enumerations.fetch_add(1, Ordering::SeqCst);
        self.names.lock().unwrap().iter().map(|n| spec(n)).collect()
    }

    async fn call(&self, name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
        ToolOutput::ok(format!("{}::{name}", self.id))
    }
}

struct FlakySource {
    id: String,
    tool: String,
    fail: AtomicBool,
    enumerations: AtomicUsize,
}

impl FlakySource {
    fn new(id: &str, tool: &str, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_string(),
            tool: tool.to_string(),
            fail: AtomicBool::new(fail),
            enumerations: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ToolSource for FlakySource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn list(&self) -> Vec<ToolSpec> {
        vec![spec(&self.tool)]
    }

    async fn inventory(&self) -> Result<Vec<ToolSpec>, String> {
        self.enumerations.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err(format!("MCP `{}`: list_tools timed out", self.id))
        } else {
            Ok(vec![spec(&self.tool)])
        }
    }

    async fn call(&self, name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
        ToolOutput::ok(format!("{}::{name}", self.id))
    }
}

/// 首轮枚举阻塞、可被测试释放的来源：确定性交错 list 与 invalidate。
struct GatedSource {
    id: String,
    names: Mutex<Vec<String>>,
    entered: Notify,
    release: Notify,
    gated: AtomicBool,
    enumerations: AtomicUsize,
}

impl GatedSource {
    fn new(id: &str, names: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_string(),
            names: Mutex::new(names.iter().map(|s| s.to_string()).collect()),
            entered: Notify::new(),
            release: Notify::new(),
            gated: AtomicBool::new(true),
            enumerations: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ToolSource for GatedSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn list(&self) -> Vec<ToolSpec> {
        self.enumerations.fetch_add(1, Ordering::SeqCst);
        if self.gated.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.names.lock().unwrap().iter().map(|n| spec(n)).collect()
    }

    async fn call(&self, name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
        ToolOutput::ok(format!("{}::{name}", self.id))
    }
}

fn call_named(name: &str) -> ToolCall {
    ToolCall {
        id: "t".to_string(),
        name: name.to_string(),
        input: json!({}),
    }
}

#[tokio::test]
async fn concurrent_lists_merge_into_single_refresh() {
    let source = CountingSource::new("mcp:p/srv", &["tool_a"]);
    let mut registry = Registry::new();
    registry.register(source.clone());
    let registry = Arc::new(registry);

    let n = 10;
    let barrier = Arc::new(Barrier::new(n + 1));
    let mut handles = Vec::new();
    for _ in 0..n {
        let reg = registry.clone();
        let b = barrier.clone();
        handles.push(tokio::spawn(async move {
            b.wait().await;
            reg.list().await
        }));
    }
    barrier.wait().await;
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }
    for specs in &results {
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "tool_a");
    }
    assert_eq!(
        source.enumerations.load(Ordering::SeqCst),
        1,
        "并发 list 必须合并为一次枚举，不形成网络风暴"
    );
    // 健康缓存直接命中：再查不重复枚举。
    let _ = registry.list().await;
    assert_eq!(source.enumerations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalidate_during_enumeration_discards_stale_snapshot() {
    let source = GatedSource::new("mcp:p/srv", &["old_tool"]);
    let mut registry = Registry::new();
    registry.register(source.clone());
    let registry = Arc::new(registry);

    let reg = registry.clone();
    let list_task = tokio::spawn(async move { reg.list().await });
    // 等首轮枚举进入来源（确定性交错点），超时即失败而非无限挂起。
    tokio::time::timeout(Duration::from_secs(10), source.entered.notified())
        .await
        .expect("gated inventory must start");
    // 在途期间改内容并失效：旧结果不得覆盖新一代。
    *source.names.lock().unwrap() = vec!["new_tool".to_string()];
    registry.invalidate();
    source.release.notify_one();

    let specs = tokio::time::timeout(Duration::from_secs(10), list_task)
        .await
        .expect("list must finish")
        .unwrap();
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"new_tool"),
        "旧枚举被丢弃后应拿到新一代清单: {names:?}"
    );
    assert!(
        !names.contains(&"old_tool"),
        "旧快照不得覆盖新一代: {names:?}"
    );

    // 模型可见名始终路由到对应来源。
    let out = registry.call(&call_named("new_tool"), &ctx()).await;
    assert!(!out.is_error, "可见名应路由到对应来源: {}", out.text);
    assert!(out.text.contains("mcp:p/srv"), "{}", out.text);
    assert!(out.text.contains("new_tool"), "{}", out.text);
    // 能力查询也走同一快照。
    let cap: CallCapability = registry.capability(&call_named("new_tool")).await;
    let _ = cap;
}

#[tokio::test]
async fn failed_source_recovers_after_retry_due_while_healthy_stays() {
    let healthy = CountingSource::new("mcp:p/good", &["good_tool"]);
    let flaky = FlakySource::new("mcp:p/flaky", "back_tool", true);
    let mut registry = Registry::new();
    registry.register(healthy.clone());
    registry.register(flaky.clone());

    let first = registry.list().await;
    assert!(first.iter().any(|s| s.name == "good_tool"));
    assert!(!first.iter().any(|s| s.name == "back_tool"));
    assert!(
        registry
            .list_errors()
            .iter()
            .any(|e| e.contains("mcp:p/flaky")),
        "失败 note 须指出来源: {:?}",
        registry.list_errors()
    );
    let healthy_n = healthy.enumerations.load(Ordering::SeqCst);
    let flaky_n = flaky.enumerations.load(Ordering::SeqCst);

    // 到期前复用缓存：不形成每轮无上限重试。
    let second = registry.list().await;
    assert!(second.iter().any(|s| s.name == "good_tool"));
    assert_eq!(
        healthy.enumerations.load(Ordering::SeqCst),
        healthy_n,
        "重试窗口内不得重复枚举健康来源"
    );
    assert_eq!(
        flaky.enumerations.load(Ordering::SeqCst),
        flaky_n,
        "重试窗口内不得每轮重试失败来源"
    );

    // 来源恢复但窗口未到：仍复用旧快照（有界）。
    flaky.fail.store(false, Ordering::SeqCst);
    let third = registry.list().await;
    assert!(
        !third.iter().any(|s| s.name == "back_tool"),
        "窗口未到前不提前重试: {third:?}"
    );

    // 私有时钟拨到期：下一次 list 合并刷新一次并恢复。
    registry.force_retry_due();
    let fourth = registry.list().await;
    assert!(fourth.iter().any(|s| s.name == "good_tool"));
    assert!(
        fourth.iter().any(|s| s.name == "back_tool"),
        "失败来源恢复后应重新出现: {fourth:?}"
    );
    assert!(
        registry.list_errors().is_empty(),
        "恢复后错误应清空: {:?}",
        registry.list_errors()
    );
    assert_eq!(
        healthy.enumerations.load(Ordering::SeqCst),
        healthy_n + 1,
        "到期后只刷新一次"
    );
    assert_eq!(
        flaky.enumerations.load(Ordering::SeqCst),
        flaky_n + 1,
        "到期后只重试一次"
    );

    // 恢复后健康缓存直接命中。
    let _ = registry.list().await;
    assert_eq!(healthy.enumerations.load(Ordering::SeqCst), healthy_n + 1);
}

#[tokio::test]
async fn duplicate_tool_names_are_deduped_without_failing_source() {
    struct DupSource {
        id: String,
    }
    #[async_trait]
    impl ToolSource for DupSource {
        fn id(&self) -> &str {
            &self.id
        }
        async fn list(&self) -> Vec<ToolSpec> {
            vec![spec("dup"), spec("dup"), spec("ok")]
        }
        async fn call(&self, name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
            ToolOutput::ok(format!("dup-src::{name}"))
        }
    }

    let mut registry = Registry::new();
    registry.register(Arc::new(DupSource {
        id: "mcp:p/dup".to_string(),
    }));
    registry.register(CountingSource::new("mcp:p/other", &["other_tool"]));

    let specs = registry.list().await;
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names.iter().filter(|n| **n == "dup").count(),
        1,
        "同源重复工具名只保留一份: {names:?}"
    );
    assert!(names.contains(&"ok"), "{names:?}");
    assert!(
        names.contains(&"other_tool"),
        "健康来源须持续可用: {names:?}"
    );
    let uniq: HashSet<&str> = names.iter().copied().collect();
    assert_eq!(uniq.len(), names.len(), "可见名不得重复: {names:?}");
    assert!(
        registry
            .list_errors()
            .iter()
            .any(|e| e.contains("duplicate") && e.contains("mcp:p/dup")),
        "去重须有诊断: {:?}",
        registry.list_errors()
    );

    let out = registry.call(&call_named("dup"), &ctx()).await;
    assert!(!out.is_error, "去重后调用仍应路由成功: {}", out.text);
}
