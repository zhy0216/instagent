//! todo 11 集成测试：proxy engine 对假 server fixture（
//! `tests/fixtures/fake_proxy_server.rs`，经 `[[bin]] fake-proxy-server` 编译，
//! 形态沿用 `mcp_e2e.rs` 的 bin-fixture 先例）的四种异常 + 正常路径：
//! 端口替换与就绪轮询（含慢就绪）、就绪超时的可读错误、退出 kill 无残留、
//! 中途崩溃自动重启一次。不依赖网络。

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use futures::StreamExt;
use serde_json::json;
use serde_json::Value;
use tempfile::TempDir;

use instagent::message::Message;
use instagent::plugin::manifest::PluginManifest;
use instagent::plugin::Plugin;
use instagent::plugin::PluginSet;
use instagent::plugin::PluginSource;
use instagent::provider::proxy::ProxyProvider;
use instagent::provider::EngineKind;
use instagent::provider::Provider;
use instagent::provider::ProviderDef;
use instagent::provider::ProviderRegistry;
use instagent::provider::ProxyDef;
use instagent::provider::Request;
use instagent::provider::StreamEvent;

const FIXTURE_BIN: &str = env!("CARGO_BIN_EXE_fake-proxy-server");

fn proxy_def(args: Vec<String>, env: BTreeMap<String, String>, timeout_secs: u64) -> ProviderDef {
    let mut full = vec!["--port".to_string(), "${PORT}".to_string()];
    full.extend(args);
    ProviderDef {
        name: "fake".to_string(),
        engine: EngineKind::Proxy,
        display_name: None,
        description: None,
        api_key_env: None,
        base_url: None,
        headers: BTreeMap::from([("x-target-port".to_string(), "${PORT}".to_string())]),
        timeout_seconds: None,
        models: vec![],
        proxy: Some(ProxyDef {
            command: FIXTURE_BIN.to_string(),
            args: full,
            env,
            ready: None,
            timeout_secs: Some(timeout_secs),
        }),
    }
}

fn plugin_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn request<'a>(messages: &'a [Message]) -> Request<'a> {
    Request {
        model: "fake-model",
        system: "",
        messages,
        tools: &[],
        max_tokens: 16,
        temperature: None,
    }
}

async fn collect_text(provider: &ProxyProvider) -> Result<String, instagent::ProviderError> {
    let messages = vec![Message::user_text("ping".to_string())];
    let mut stream = provider.stream(request(&messages)).await?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        if let Ok(StreamEvent::TextDelta(delta)) = event {
            text.push_str(&delta);
        }
    }
    Ok(text)
}

async fn get_status(url: &str) -> Option<reqwest::StatusCode> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    client.get(url).send().await.ok().map(|r| r.status())
}

fn meta_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// `starts.log` 行形如 `start <port> <pid>`，解析出端口与进程号。
fn parse_start(line: &str) -> (u16, u32) {
    let mut parts = line.split_whitespace();
    assert_eq!(parts.next(), Some("start"), "{line}");
    let port = parts.next().and_then(|v| v.parse().ok());
    let pid = parts.next().and_then(|v| v.parse().ok());
    (
        port.unwrap_or_else(|| panic!("bad start line {line}")),
        pid.unwrap_or_else(|| panic!("bad start line {line}")),
    )
}

/// 等 pid 消失（kill -0 失败）；kill_on_drop 的 reap 在 tokio 运行时上异步
/// 发生，必须用 tokio sleep 让 worker 推进。
async fn wait_pid_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let alive = std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !alive {
            return;
        }
        assert!(Instant::now() < deadline, "pid {pid} still alive");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- 1. 端口替换 + 就绪轮询（含慢就绪）+ env 白名单 ----

#[tokio::test]
async fn replaces_port_and_polls_slow_ready_with_env_allowlist() {
    std::env::remove_var("INSTAGENT_TEST_SECRET");
    let tmp = TempDir::new().unwrap();
    let meta = tmp.path().join("meta.json");
    let args = vec![
        "--ready-delay".to_string(),
        "0.6".to_string(),
        "--meta-file".to_string(),
        meta.display().to_string(),
    ];
    let env = BTreeMap::from([("FAKE_PROXY_MARKER".to_string(), "mk-42".to_string())]);
    let def = proxy_def(args, env, 15);
    let provider = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect("slow ready within timeout");

    let port = provider.port();
    assert!(port > 0);
    assert_eq!(provider.endpoint(), format!("http://127.0.0.1:{port}"));
    // args 里的 ${PORT} 与 INSTAGENT_PORT 环境变量一致；pid 对得上。
    let meta = meta_json(&meta);
    assert_eq!(meta["args_port"], json!(port as u64));
    assert_eq!(meta["env_port"], json!(port.to_string()));
    assert_eq!(provider.child_pid(), meta["pid"].as_u64().map(|p| p as u32));
    // 插件声明的 env 透传；进程里未声明的敏感变量不进白名单。
    assert_eq!(meta["marker"], "mk-42");
    assert!(meta["secret"].is_null(), "{meta}");
    // 就绪后按 openai 引擎打本地端点拿流（headers 的 ${PORT} 展开见 11 的
    // build_inner 单元测试）。
    assert_eq!(collect_text(&provider).await.unwrap(), "pong");
}

// ---- 2. 就绪超时的可读错误 ----

#[tokio::test]
async fn ready_timeout_is_readable_and_kills_child() {
    let tmp = TempDir::new().unwrap();
    let meta = tmp.path().join("meta.json");
    let args = vec![
        "--never-ready".to_string(),
        "--meta-file".to_string(),
        meta.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 1);
    let err = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect_err("never ready must time out");
    let msg = format!("{err:#}");
    assert!(msg.contains("not ready"), "{msg}");
    assert!(msg.contains("GET http://127.0.0.1:"), "{msg}");
    assert!(msg.contains("/v1/models"), "{msg}");
    assert!(msg.contains("1s"), "{msg}");
    // 超时路径 drop 了子进程：pid 消失，无残留。
    wait_pid_gone(meta_json(&meta)["pid"].as_u64().unwrap() as u32).await;
}

#[tokio::test]
async fn child_exit_before_ready_reports_status_without_waiting_timeout() {
    let def = proxy_def(vec!["--exit-early".to_string()], BTreeMap::new(), 30);
    let started = Instant::now();
    let err = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect_err("child exits immediately");
    let msg = format!("{err:#}");
    assert!(msg.contains("exited"), "{msg}");
    assert!(msg.contains("exit status: 2"), "{msg}");
    assert!(msg.contains("before ready"), "{msg}");
    // 没有白等 30s 超时。
    assert!(started.elapsed() < Duration::from_secs(5));
}

// ---- 3. drop（会话结束）后子进程被 kill、端口释放 ----

#[tokio::test]
async fn dropping_provider_kills_proxy_leaving_no_listener() {
    let def = proxy_def(Vec::new(), BTreeMap::new(), 15);
    let provider = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect("ready");
    let url = format!("{}/v1/models", provider.endpoint());
    assert_eq!(
        get_status(&url).await,
        Some(reqwest::StatusCode::OK),
        "ready implies listening"
    );

    drop(provider);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if get_status(&url).await.is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "proxy listener still up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- 4. 中途崩溃后自动重启一次 ----

#[tokio::test]
async fn connection_crash_restarts_once_and_serves_again() {
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("crashed");
    let log = tmp.path().join("starts.log");
    let args = vec![
        "--crash-on-chat".to_string(),
        "--state-file".to_string(),
        state.display().to_string(),
        "--log-file".to_string(),
        log.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 15);
    let provider = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect("ready");
    let first = read_lines(&log);
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(parse_start(&first[0]).0, provider.port());

    // 第一个进程在处理 chat 请求时自杀 → Transport → 自动重启（第二个实例
    // 见 state 文件存在，转为稳定模式）。
    assert_eq!(collect_text(&provider).await.unwrap(), "pong");
    let starts = read_lines(&log);
    assert_eq!(starts.len(), 2, "{starts:?}");
    // 重启后端口换了、endpoint 跟着指到新进程。
    let (first_port, first_pid) = parse_start(&starts[0]);
    let (second_port, second_pid) = parse_start(&starts[1]);
    assert_eq!(second_port, provider.port());
    assert_ne!(first_port, second_port);
    assert_ne!(first_pid, second_pid);
    // 稳定后不再触发重启。
    assert_eq!(collect_text(&provider).await.unwrap(), "pong");
    assert_eq!(read_lines(&log).len(), 2);
}

#[tokio::test]
async fn restart_is_bounded_to_once_per_call() {
    let tmp = TempDir::new().unwrap();
    let log = tmp.path().join("starts.log");
    let args = vec![
        "--always-crash".to_string(),
        "--log-file".to_string(),
        log.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 15);
    let provider = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect("ready");

    // 每次调用至多重启一次：Transport 后拉起新进程再试一次，仍崩则报错。
    let err = collect_text(&provider).await.unwrap_err();
    assert!(
        matches!(err, instagent::ProviderError::Transport(_)),
        "{err}"
    );
    assert_eq!(read_lines(&log).len(), 2);

    let err = collect_text(&provider).await.unwrap_err();
    assert!(
        matches!(err, instagent::ProviderError::Transport(_)),
        "{err}"
    );
    assert_eq!(read_lines(&log).len(), 3);
}

// ---- 4b. T1：有限换端口重试与总就绪期限 ----

#[tokio::test]
async fn first_candidate_fails_then_next_succeeds_on_retry() {
    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("first-failed");
    let log = tmp.path().join("starts.log");
    let args = vec![
        "--fail-first-start".to_string(),
        marker.display().to_string(),
        "--log-file".to_string(),
        log.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 15);
    let provider = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect("second candidate succeeds after first exits");
    // 第一个候选退出、第二个就绪：两次启动，且最终服务可用。
    let starts = read_lines(&log);
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert_eq!(parse_start(&starts[1]).0, provider.port());
    assert_eq!(collect_text(&provider).await.unwrap(), "pong");
    // 首个失败候选的进程组被回收。
    wait_pid_gone(parse_start(&starts[0]).1).await;
}

#[tokio::test]
async fn persistent_early_exit_stops_after_fixed_attempts() {
    let tmp = TempDir::new().unwrap();
    let log = tmp.path().join("starts.log");
    let args = vec![
        "--exit-early".to_string(),
        "--log-file".to_string(),
        log.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 15);
    let started = Instant::now();
    let err = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect_err("always exits early");
    // 固定尝试次数（1 + MAX_PORT_RETRIES）后放弃，而非无限重试。
    assert_eq!(read_lines(&log).len(), 3, "{:?}", read_lines(&log));
    let msg = format!("{err:#}");
    assert!(msg.contains("3 attempt"), "{msg}");
    assert!(msg.contains("exit status: 2"), "{msg}");
    assert!(msg.contains("exited"), "{msg}");
    // 不白等 15s 超时。
    assert!(started.elapsed() < Duration::from_secs(10));
    // 全部失败候选进程组被回收。
    for line in read_lines(&log) {
        wait_pid_gone(parse_start(&line).1).await;
    }
}

#[tokio::test]
async fn spawn_missing_command_fails_immediately_without_retry() {
    let tmp = TempDir::new().unwrap();
    let log = tmp.path().join("starts.log");
    // `./` 相对路径在插件根下不存在 → 配置无效，立即失败、不换端口重试。
    let mut def = proxy_def(Vec::new(), BTreeMap::new(), 15);
    def.proxy.as_mut().unwrap().command = "./no-such-proxy-bin".to_string();
    def.proxy.as_mut().unwrap().args = vec!["--log-file".to_string(), log.display().to_string()];
    let started = Instant::now();
    let err = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect_err("missing command must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("not found under plugin root"), "{msg}");
    assert!(started.elapsed() < Duration::from_secs(5));
    // 一次都没真正 spawn。
    assert!(!log.exists(), "{:?}", read_lines(&log));
}

#[tokio::test]
async fn never_ready_non_200_stays_within_total_budget() {
    let tmp = TempDir::new().unwrap();
    let log = tmp.path().join("starts.log");
    // 进程正常监听并就绪探针永远返回 503：只拉起一个候选，且总耗时
    // ≈ 总期限（不因重试而获得多份完整 timeout）。
    let args = vec![
        "--ready-status".to_string(),
        "503".to_string(),
        "--log-file".to_string(),
        log.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 2);
    let started = Instant::now();
    let err = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect_err("never 200");
    let elapsed = started.elapsed();
    let msg = format!("{err:#}");
    assert!(msg.contains("not ready"), "{msg}");
    // 单个候选耗尽总期限；调度容差内不超预算，也不因重试翻倍。
    assert_eq!(read_lines(&log).len(), 1, "{:?}", read_lines(&log));
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    for line in read_lines(&log) {
        wait_pid_gone(parse_start(&line).1).await;
    }
}

#[tokio::test]
async fn hanging_ready_probe_is_bounded_by_remaining_budget() {
    let tmp = TempDir::new().unwrap();
    let log = tmp.path().join("starts.log");
    // accept 但不回包：单次探针请求可能挂住，探针超时被剩余期限收口，
    // 总耗时仍不超预算加少量容差。
    let args = vec![
        "--accept-and-hold".to_string(),
        "--log-file".to_string(),
        log.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 2);
    let started = Instant::now();
    let err = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect_err("probe hangs");
    let elapsed = started.elapsed();
    let msg = format!("{err:#}");
    assert!(msg.contains("not ready"), "{msg}");
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    for line in read_lines(&log) {
        wait_pid_gone(parse_start(&line).1).await;
    }
}

// ---- 4c. T2：并发连接失败合并重启 ----

#[tokio::test]
async fn concurrent_transport_failures_restart_once_and_both_succeed() {
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("crashed");
    let log = tmp.path().join("starts.log");
    let args = vec![
        "--crash-on-chat".to_string(),
        "--state-file".to_string(),
        state.display().to_string(),
        "--log-file".to_string(),
        log.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 15);
    let provider = std::sync::Arc::new(
        ProxyProvider::start(&def, &plugin_root())
            .await
            .expect("ready"),
    );
    assert_eq!(read_lines(&log).len(), 1);

    // 屏障让两个请求在同一刻打到将死的旧实例：首个 chat 让旧进程自杀，
    // 两个请求都拿到 Transport 并触发重启。
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let provider = provider.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            collect_text(&provider).await
        }));
    }
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.unwrap());
    }
    // 两个请求都成功：先到者重启，后到者复用新实例而非再拉一份。
    for result in &results {
        assert_eq!(result.as_deref().unwrap_or("ERR"), "pong", "{results:?}");
    }
    // 只启动了一份替代实例：1（原始）+ 1（合并后的重启）。
    let starts = read_lines(&log);
    assert_eq!(starts.len(), 2, "{starts:?}");
    // 后到者不杀正在服务的实例：当前端口即重启实例，且仍可用。
    assert_eq!(parse_start(&starts[1]).0, provider.port());
    assert_eq!(collect_text(&provider).await.unwrap(), "pong");
    assert_eq!(read_lines(&log).len(), 2, "{:?}", read_lines(&log));
}

#[tokio::test]
async fn late_restart_reuses_replacement_and_drop_cleans_up() {
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("crashed");
    let log = tmp.path().join("starts.log");
    // 重启后的稳定实例慢就绪，拉长重启窗口，让第二个失败请求必然"后到"。
    let args = vec![
        "--crash-on-chat".to_string(),
        "--state-file".to_string(),
        state.display().to_string(),
        "--late-ready-delay".to_string(),
        "0.8".to_string(),
        "--log-file".to_string(),
        log.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 15);
    let provider = std::sync::Arc::new(
        ProxyProvider::start(&def, &plugin_root())
            .await
            .expect("ready"),
    );

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let provider = provider.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            collect_text(&provider).await
        }));
    }
    let mut ok = 0;
    for task in tasks {
        if matches!(task.await.unwrap().as_deref(), Ok("pong")) {
            ok += 1;
        }
    }
    assert_eq!(ok, 2);
    // 只重启一份；后到者复用，未再拉起、也未杀掉在服务的实例。
    assert_eq!(read_lines(&log).len(), 2, "{:?}", read_lines(&log));
    let url = format!("{}/v1/models", provider.endpoint());
    let pid = provider.child_pid().expect("live child");
    assert_eq!(get_status(&url).await, Some(reqwest::StatusCode::OK));

    // drop 最终 provider：子进程被回收、监听消失。
    drop(provider);
    wait_pid_gone(pid).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if get_status(&url).await.is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "proxy listener still up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn cancelled_restart_candidate_is_reaped() {
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("crashed");
    let log = tmp.path().join("starts.log");
    // 重启候选慢就绪（30s 远大于取消窗口），保证取消时候选仍在轮询未就绪。
    let args = vec![
        "--crash-on-chat".to_string(),
        "--state-file".to_string(),
        state.display().to_string(),
        "--late-ready-delay".to_string(),
        "30".to_string(),
        "--log-file".to_string(),
        log.display().to_string(),
    ];
    let def = proxy_def(args, BTreeMap::new(), 60);
    let provider = ProxyProvider::start(&def, &plugin_root())
        .await
        .expect("ready");
    assert_eq!(read_lines(&log).len(), 1);

    // 首个 chat 让旧实例自杀 → Transport → 重启开始拉慢就绪候选；
    // 在候选就绪前取消整个调用。
    let messages = vec![Message::user_text("ping".to_string())];
    let result = tokio::select! {
        result = provider.stream(request(&messages)) => Some(result),
        _ = tokio::time::sleep(Duration::from_millis(500)) => None,
    };
    assert!(result.is_none(), "expected cancellation before ready");

    // 候选确实被拉起过，随后因取消被 drop：进程组回收、监听不存在。
    let starts = read_lines(&log);
    assert_eq!(starts.len(), 2, "{starts:?}");
    let (candidate_port, candidate_pid) = parse_start(&starts[1]);
    wait_pid_gone(candidate_pid).await;
    assert!(
        get_status(&format!("http://127.0.0.1:{candidate_port}/v1/models"))
            .await
            .is_none(),
        "cancelled candidate must not keep listening"
    );
}

// ---- 5. registry `proxy` 分支接线：JSON → 拉起 → 可用引擎 ----

#[tokio::test]
async fn registry_proxy_engine_starts_fake_provider_end_to_end() {
    let data = TempDir::new().unwrap();
    std::env::set_var("INSTAGENT_DATA_DIR", data.path());
    let plugin_dir = data.path().join("plugins").join("proxytest");
    let providers = plugin_dir.join("dev.instagent").join("providers");
    std::fs::create_dir_all(&providers).unwrap();
    std::fs::write(
        providers.join("px.json"),
        format!(
            r#"{{"name":"px","engine":"proxy","proxy":{{"command":"{FIXTURE_BIN}","args":["--port","${{PORT}}"]}}}}"#,
        ),
    )
    .unwrap();
    let plugin = Plugin {
        manifest: PluginManifest {
            schema: None,
            name: "proxytest".to_string(),
            version: "1.0.0".to_string(),
            description: None,
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            extensions: BTreeMap::new(),
        },
        root: plugin_dir,
        source: PluginSource::User,
    };
    let set = PluginSet {
        plugins: vec![plugin],
        skipped: vec![],
    };
    let registry = ProviderRegistry::from_plugins(&set).expect("load registry");
    let provider = registry.get("px").await.expect("proxy engine starts");
    assert_eq!(provider.name(), "px");
    let messages = vec![Message::user_text("ping".to_string())];
    let mut stream = provider.stream(request(&messages)).await.unwrap();
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        if let Ok(StreamEvent::TextDelta(delta)) = event {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "pong");
    drop(provider);
    std::env::remove_var("INSTAGENT_DATA_DIR");
    // 本二进制内 INSTAGENT_DATA_DIR 仅此测试读写，无需串行锁。
}
