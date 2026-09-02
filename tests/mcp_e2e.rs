//! todo 14 集成测试：McpSource 对最小 stdio MCP server fixture（
//! `tests/fixtures/mcp_stdio_server.rs`，经 `[[bin]] mcp-fixture-server` 编译）
//! 的端到端行为：前缀、readOnlyHint、超时、server 被 kill 后的可读错误。
//! 不依赖网络 / npx。

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use instagent::plugin::manifest::PluginManifest;
use instagent::plugin::mcp_config::McpServerConfig;
use instagent::plugin::mcp_config::McpServerType;
use instagent::plugin::Plugin;
use instagent::plugin::PluginSource;
use instagent::tools::mcp::connect_plugin;
use instagent::tools::McpSource;
use instagent::tools::Registry;
use instagent::tools::ToolCall;
use instagent::tools::ToolCtx;
use instagent::tools::ToolSource;

const FIXTURE_BIN: &str = env!("CARGO_BIN_EXE_mcp-fixture-server");

fn manifest() -> PluginManifest {
    serde_json::from_str(r#"{"name": "demo", "version": "0.1.0"}"#).unwrap()
}

/// tempdir 插件根：`./mcp-fixture-server` 软链到 fixture 二进制 + mcp.json。
fn plugin_with_mcp_json(json: &str) -> (tempfile::TempDir, Plugin) {
    let tmp = tempfile::TempDir::new().unwrap();
    std::os::unix::fs::symlink(FIXTURE_BIN, tmp.path().join("mcp-fixture-server")).unwrap();
    std::fs::write(tmp.path().join("mcp.json"), json).unwrap();
    let plugin = Plugin {
        manifest: manifest(),
        root: tmp.path().to_path_buf(),
        source: PluginSource::User,
    };
    (tmp, plugin)
}

fn fixture_stdio_config() -> McpServerConfig {
    McpServerConfig {
        name: "fixture".to_string(),
        r#type: McpServerType::Stdio,
        command: Some("./mcp-fixture-server".to_string()),
        args: vec![],
        env: Default::default(),
        cwd: None,
        url: None,
        headers: Default::default(),
    }
}

fn ctx(dir: &Path) -> ToolCtx {
    ToolCtx {
        cwd: dir.to_path_buf(),
        cancel: CancellationToken::new(),
    }
}

async fn connect_fixture(plugin_root: &Path) -> McpSource {
    std::os::unix::fs::symlink(FIXTURE_BIN, plugin_root.join("mcp-fixture-server")).unwrap();
    let plugin = Plugin {
        manifest: manifest(),
        root: plugin_root.to_path_buf(),
        source: PluginSource::User,
    };
    let server = fixture_stdio_config();
    McpSource::connect(&plugin, &server)
        .await
        .expect("connect fixture stdio server")
}

// ---- O1 ----

#[tokio::test]
async fn connect_sets_id_and_stores_instructions() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source = connect_fixture(tmp.path()).await;
    assert_eq!(source.id, "mcp:demo/fixture");
    assert!(
        source
            .instructions
            .as_deref()
            .is_some_and(|i| i.contains("fixture instructions")),
        "initialize 的 instructions 要存下来供系统提示拼装: {:?}",
        source.instructions
    );
    assert!(source.child_pid().is_some_and(|p| p > 0));
    source.shutdown().await;
}

#[tokio::test]
async fn sse_server_is_skipped_with_unsupported_note() {
    let (_tmp, plugin) = plugin_with_mcp_json(
        r#"{"mcpServers": {
             "legacy": {"type": "sse", "url": "https://example.invalid/sse"},
             "fixture": {"type": "stdio", "command": "./mcp-fixture-server"}
           }}"#,
    );
    let data = tempfile::TempDir::new().unwrap();
    let outcome = connect_plugin(&plugin, data.path()).await.unwrap();
    assert_eq!(outcome.sources.len(), 1);
    assert_eq!(outcome.sources[0].server.name, "fixture");
    assert_eq!(outcome.notes.len(), 1);
    assert!(outcome.notes[0].contains("sse"), "{}", outcome.notes[0]);
    assert!(
        outcome.notes[0].contains("not supported"),
        "跳过 sse 必须给\"不支持\"信息: {}",
        outcome.notes[0]
    );
    for source in &outcome.sources {
        source.shutdown().await;
    }
}

#[tokio::test]
async fn unreachable_http_server_errors_readably_without_hanging() {
    let server = McpServerConfig {
        name: "remote".to_string(),
        r#type: McpServerType::StreamableHttp,
        command: None,
        args: vec![],
        env: Default::default(),
        cwd: None,
        url: Some("http://127.0.0.1:1/mcp".to_string()),
        headers: Default::default(),
    };
    let plugin = Plugin {
        manifest: manifest(),
        root: PathBuf::from("/"),
        source: PluginSource::User,
    };
    let start = std::time::Instant::now();
    let err = McpSource::connect(&plugin, &server)
        .await
        .expect_err("connection refused must error");
    assert!(start.elapsed() < Duration::from_secs(20), "must not hang");
    let full = format!("{err:#}");
    assert!(full.contains("remote"), "{full}");
}

#[tokio::test]
async fn missing_command_binary_errors_readably() {
    let (_tmp, plugin) = plugin_with_mcp_json(
        r#"{"mcpServers": {"gone": {"type": "stdio", "command": "./no-such-server"}}}"#,
    );
    let data = tempfile::TempDir::new().unwrap();
    let err = connect_plugin(&plugin, data.path())
        .await
        .expect_err("missing command must error");
    let msg = format!("{err:#}");
    assert!(msg.contains("no-such-server"), "{msg}");
    assert!(msg.contains("not found"), "{msg}");
    // 第三版 §5 P7：错误必须同时指出插件与 server 名（来源 id `mcp:<plugin>/<server>`）。
    assert!(msg.contains("mcp:demo/gone"), "{msg}");
}

// ---- O2 ----

#[tokio::test]
async fn list_prefixes_names_and_maps_read_only_hint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source = connect_fixture(tmp.path()).await;
    let specs = source.list().await;
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "fixture__add",
            "fixture__echo",
            "fixture__fail",
            "fixture__slow"
        ],
        "list_tools 名字必须带 `<server>__` 前缀"
    );
    let echo = specs.iter().find(|s| s.name == "fixture__echo").unwrap();
    assert!(echo.read_only, "readOnlyHint=true 要映射为 read_only");
    assert!(echo.description.contains("echo"));
    assert_eq!(echo.input_schema["type"], "object");
    let add = specs.iter().find(|s| s.name == "fixture__add").unwrap();
    assert!(!add.read_only, "未声明 annotations → read_only=false");
    source.shutdown().await;
}

#[tokio::test]
async fn call_joins_text_blocks_and_maps_is_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source = connect_fixture(tmp.path()).await;
    let ctx = ctx(tmp.path());

    let output = source
        .call("fixture__echo", json!({"text": "hi there"}), &ctx)
        .await;
    assert!(!output.is_error);
    assert_eq!(
        output.text, "echo: hi there\nsecond block",
        "text 块按行拼接"
    );

    let output = source
        .call("fixture__add", json!({"a": 2, "b": 40}), &ctx)
        .await;
    assert!(!output.is_error);
    assert_eq!(output.text, "42");

    let output = source.call("fixture__fail", json!({}), &ctx).await;
    assert!(output.is_error, "is_error 直接映射");
    assert!(output.text.contains("boom"), "{}", output.text);

    let output = source
        .call("fixture__echo", json!("not-an-object"), &ctx)
        .await;
    assert!(output.is_error);
    assert!(output.text.contains("JSON object"), "{}", output.text);
    source.shutdown().await;
}

#[tokio::test]
async fn call_times_out_with_readable_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source = connect_fixture(tmp.path())
        .await
        .with_call_timeout(Duration::from_millis(300));
    let ctx = ctx(tmp.path());
    let start = std::time::Instant::now();
    let output = source.call("fixture__slow", json!({}), &ctx).await;
    let elapsed = start.elapsed();
    assert!(output.is_error);
    assert!(output.text.contains("timed out"), "{}", output.text);
    assert!(
        elapsed < Duration::from_secs(10),
        "超时后必须快速返回，实测 {elapsed:?}"
    );
    source.shutdown().await;
}

// ---- O3：kill / shutdown 后不挂起 ----

#[tokio::test]
async fn call_after_server_kill_returns_fast_readable_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source = connect_fixture(tmp.path()).await;
    let ctx = ctx(tmp.path());
    assert!(
        !source
            .call("fixture__echo", json!({"text": "ok"}), &ctx)
            .await
            .is_error
    );

    let pid = source.child_pid().expect("stdio child pid");
    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill fixture server");
    assert!(status.success());

    let start = std::time::Instant::now();
    let output = source
        .call("fixture__echo", json!({"text": "after kill"}), &ctx)
        .await;
    let elapsed = start.elapsed();
    assert!(
        output.is_error,
        "server 被 kill 后调用必须报错: {:?}",
        output
    );
    let text = output.text.to_lowercase();
    assert!(
        text.contains("closed") || text.contains("transport") || text.contains("not connected"),
        "错误必须可读，实得: {}",
        output.text
    );
    assert!(
        output.text.contains("fixture"),
        "错误里要能看出是哪个 server"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "不得挂起，实测 {elapsed:?}"
    );
    assert!(source.list().await.is_empty(), "kill 后 list 也应安全降级");
    source.shutdown().await;
}

#[tokio::test]
async fn shutdown_kills_child_and_calls_report_not_connected() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source = connect_fixture(tmp.path()).await;
    let pid = source.child_pid().expect("stdio child pid");
    source.shutdown().await;

    let output = source
        .call("fixture__echo", json!({"text": "x"}), &ctx(tmp.path()))
        .await;
    assert!(output.is_error);
    assert!(output.text.contains("not connected"), "{}", output.text);

    let gone = |pid: u32| {
        !std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    for _ in 0..100 {
        if gone(pid) {
            return; // 进程已消失
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("shutdown 后子进程 {pid} 仍存活，kill_on_drop 未生效");
}

// ---- 与 13 Registry 的接线 ----

#[tokio::test]
async fn registry_routes_prefixed_names_end_to_end() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source = Arc::new(connect_fixture(tmp.path()).await);
    let mut registry = Registry::new();
    registry.register(source.clone());

    let specs = registry.list().await;
    assert!(specs.iter().any(|s| s.name == "fixture__echo"));
    let output = registry
        .call(
            &ToolCall {
                id: "t".to_string(),
                name: "fixture__echo".to_string(),
                input: json!({"text": "via registry"}),
            },
            &ctx(tmp.path()),
        )
        .await;
    assert!(!output.is_error);
    assert!(output.text.contains("via registry"));

    registry.shutdown().await;
}
