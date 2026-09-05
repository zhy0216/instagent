//! todo 14 的测试 fixture：最小 stdio MCP server（rmcp server 侧），
//! 由 `src/tools/mcp.rs` 的测试通过 `CARGO_BIN_EXE_mcp-fixture-server` 以
//! stdio 子进程方式启动。不依赖网络 / npx；`cargo test` 编译即用。
//!
//! 提供 4 个工具：
//! - `echo`：返回两个 text 块（验证拼接），带 `annotations.readOnlyHint = true`；
//!   调用时先发一条 logging 通知（验证客户端"通知只写日志"路径）。
//! - `add`：两数相加（验证结构化参数 → 结果）。
//! - `fail`：`CallToolResult::error`（验证 `is_error` 直接映射）。
//! - `slow`：睡 1 小时（验证客户端超时；客户端超时后由 shutdown 收尾）。

// logging/* 类型被 SEP-2577 标记 deprecated，v1 仍按约定收发并只写日志。
#![allow(deprecated)]

use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResponse;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::InitializeResult;
use rmcp::model::ListToolsResult;
use rmcp::model::LoggingLevel;
use rmcp::model::LoggingMessageNotification;
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerNotification;
use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use rmcp::service::serve_server;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use serde_json::json;

#[derive(Debug, Default)]
struct FixtureServer;

fn object_schema(
    properties: serde_json::Value,
    required: &[&str],
) -> serde_json::Map<String, serde_json::Value> {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
    .as_object()
    .expect("fixture schema is a JSON object")
    .clone()
}

impl ServerHandler for FixtureServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("fixture instructions: echo text and add numbers")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new(
                "add",
                "add two numbers",
                object_schema(
                    json!({
                        "a": {"type": "number"},
                        "b": {"type": "number"},
                    }),
                    &["a", "b"],
                ),
            ),
            Tool::new(
                "echo",
                "echo the given text back in two blocks",
                object_schema(json!({"text": {"type": "string"}}), &["text"]),
            )
            .with_annotations(ToolAnnotations::new().read_only(true)),
            Tool::new(
                "fail",
                "always returns a tool-level error",
                object_schema(json!({}), &[]),
            ),
            Tool::new(
                "slow",
                "never returns quickly (client timeout fixture)",
                object_schema(json!({}), &[]),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.as_ref().to_string();
        let arguments = request.arguments.unwrap_or_default();
        match name.as_str() {
            "echo" => {
                let text = arguments
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let notification = LoggingMessageNotification::new(
                    LoggingMessageNotificationParam::new(
                        LoggingLevel::Info,
                        json!({"echoed": text.chars().count()}),
                    )
                    .with_logger("fixture"),
                );
                let _ = context
                    .peer
                    .send_notification(ServerNotification::LoggingMessageNotification(notification))
                    .await;
                Ok(CallToolResult::success(vec![
                    ContentBlock::text(format!("echo: {text}")),
                    ContentBlock::text("second block"),
                ])
                .into())
            }
            "add" => {
                let arg = |key: &str| {
                    arguments.get(key).and_then(|v| v.as_f64()).ok_or_else(|| {
                        McpError::invalid_params(format!("missing number `{key}`"), None)
                    })
                };
                let total = arg("a")? + arg("b")?;
                let rendered = if total.fract() == 0.0 {
                    format!("{}", total as i64)
                } else {
                    total.to_string()
                };
                Ok(CallToolResult::success(vec![ContentBlock::text(rendered)]).into())
            }
            "fail" => Ok(CallToolResult::error(vec![ContentBlock::text(
                "boom: deliberate fixture failure",
            )])
            .into()),
            "slow" => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(
                    CallToolResult::success(vec![ContentBlock::text("you should never see this")])
                        .into(),
                )
            }
            other => Err(McpError::invalid_params(
                format!("unknown fixture tool `{other}`"),
                None,
            )),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // T2 洪泛模式：`MCP_FIXTURE_STDERR_FLOOD=1` 时在 stderr 写大量无换行内容 +
    // 跨块 Unicode + 多行日志，验证客户端有界排空仍保持 initialize/list/call 健康。
    // stderr 与 MCP 协议（stdin/stdout）分离，不干扰协议；写完即停，drain 到 EOF 结束。
    if std::env::var("MCP_FIXTURE_STDERR_FLOOD").is_ok() {
        tokio::spawn(async {
            use tokio::io::AsyncWriteExt as _;
            let mut err = tokio::io::stderr();
            let chunk = vec![b'x'; 8192];
            for _ in 0..32 {
                // 256 KiB 无换行：旧 `lines()` 会整行缓存在内存。
                if err.write_all(&chunk).await.is_err() {
                    return;
                }
            }
            // 跨块 Unicode：逐字节写，强制在多字节序列中间切分。
            let emoji = "中文🙂".as_bytes();
            for _ in 0..200 {
                for b in emoji {
                    if err.write_all(&[*b]).await.is_err() {
                        return;
                    }
                }
            }
            if err.write_all(b"\n").await.is_err() {
                return;
            }
            for i in 0..100 {
                let line = format!("flood line {i}\n");
                if err.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
            }
            let _ = err.flush().await;
        });
    }
    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let server = serve_server(FixtureServer, transport).await?;
    let _reason = server.waiting().await?;
    Ok(())
}
