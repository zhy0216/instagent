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
    assert_eq!(read_lines(&log), vec![format!("start {}", provider.port())]);

    // 第一个进程在处理 chat 请求时自杀 → Transport → 自动重启（第二个实例
    // 见 state 文件存在，转为稳定模式）。
    assert_eq!(collect_text(&provider).await.unwrap(), "pong");
    let starts = read_lines(&log);
    assert_eq!(starts.len(), 2, "{starts:?}");
    // 重启后端口换了、endpoint 跟着指到新进程。
    assert_eq!(starts[1], format!("start {}", provider.port()));
    assert_ne!(starts[0], starts[1]);
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
