//! `McpSource`：插件 `mcp.json` 的每个 server 一个实例（第三版 §2.5；第二版 §2.4）。
//!
//! rmcp 是内核里的"规范组件运行时"：stdio 经 `03` 的
//! [`spawn_long_lived_mcp_subprocess`]（进程组 + kill_on_drop，stderr 接日志）；
//! streamable-http 用 [`StreamableHttpClientTransport::from_uri`]（`headers`
//! 规范规定不承载凭据，远程鉴权 v1 不做，不随请求发送——配置了 headers 会产出
//! 可见 note，不静默忽略）；`sse` 跳过并给"不支持"信息。`initialize` 返回的
//! `instructions` 存下供系统提示拼装（`16`/`18` 接线）。
//!
//! 健壮性（`10`）：connect（spawn + initialize 握手）与 list inventory 各有硬
//! 超时（[`CONNECT_TIMEOUT_SECS`] / [`LIST_TIMEOUT_SECS`]），超时/断连的 inventory
//! 经 [`ToolSource::inventory`] 以 `Err(note)` 上报，绝不伪装成空工具列表；
//! [`connect_plugin`] 逐 server 建连，单 server 失败只产生 note，保留其它健康
//! source，只有全部失败（一个可用 source 都没有且确有失败）才整体报错。
//! stdio 子进程环境按 ADR 0003 D2：`env_clear` + baseline（PATH/HOME/LANG +
//! `PLUGIN_ROOT` + manifest 声明 allowlist）+ server `env` 显式覆盖。
//!
//! 工具映射：`list_tools` → `<server>__<tool>`（冲突时 `13` 的 Registry 再加
//! 插件名前缀），`annotations.readOnlyHint` → `read_only`；`call_tool` 把
//! content 里的 text 块按行拼接，`is_error` 直接映射，单次调用超时默认 300s；
//! progress / logging 通知 v1 只写日志。

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ClientCapabilities;
use rmcp::model::ClientInfo;
use rmcp::model::ContentBlock;
use rmcp::model::Implementation;
use rmcp::ClientHandler;
// logging/* 通知被 SEP-2577 标记 deprecated，v1 仍按约定只写日志。
#[allow(deprecated)]
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::ProgressNotificationParam;
use rmcp::service::serve_client;
use rmcp::service::NotificationContext;
use rmcp::service::RoleClient;
use rmcp::service::RunningService;
use rmcp::transport::IntoTransport;
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::Value;
use std::time::Instant;
use tokio::io::AsyncReadExt as _;
use tokio::process::ChildStderr;
use tokio::process::Command;

use crate::plugin::mcp_config::load_servers;
use crate::plugin::mcp_config::McpServerConfig;
use crate::plugin::mcp_config::McpServerType;
use crate::plugin::Plugin;
use crate::subprocess::spawn_long_lived_mcp_subprocess;
use crate::subprocess::McpProcess;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::tools::ToolSource;
use crate::tools::ToolSpec;
use crate::tools::NAME_SEP;

/// 单次 call_tool 超时（第二版 §2.4）。
pub const CALL_TIMEOUT_SECS: u64 = 300;

/// connect（spawn + initialize 握手）硬超时（`10`）。
pub const CONNECT_TIMEOUT_SECS: u64 = 30;

/// list inventory 单次请求硬超时（`10`）。
pub const LIST_TIMEOUT_SECS: u64 = 30;

/// 错误 note 里回显的失败原因最大字符数（有界，防坏 server 放大日志）。
const MAX_NOTE_CHARS: usize = 512;

fn bounded_note(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_NOTE_CHARS).collect();
    if out.chars().count() < text.chars().count() {
        out.push('…');
    }
    out
}

/// sse server 的"不支持"提示信息；非 sse 返回 `None`。
pub fn unsupported_server_note(plugin: &str, server: &McpServerConfig) -> Option<String> {
    (server.r#type == McpServerType::Sse).then(|| {
        format!(
            "MCP server `{}` of plugin `{}` uses transport `sse`, which is not supported \
             in v1 (stdio / streamable-http only); skipped",
            server.name, plugin
        )
    })
}

/// rmcp 客户端 handler：v1 对 progress / logging 通知只写日志（第二版 §2.4）。
#[derive(Debug)]
struct LoggingClientHandler;

impl ClientHandler for LoggingClientHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("instagent", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _ctx: NotificationContext<RoleClient>,
    ) {
        tracing::debug!("mcp progress: {params:?}");
    }

    // logging/* 通知被 SEP-2577 标记 deprecated，v1 仍按约定只写日志。
    #[allow(deprecated)]
    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _ctx: NotificationContext<RoleClient>,
    ) {
        tracing::debug!("mcp log: {params:?}");
    }
}

/// 一个插件 MCP server 的运行时句柄（第三版 §2.5）。
pub struct McpSource {
    /// `mcp:<plugin>/<server>`。
    pub id: String,
    pub server: McpServerConfig,
    /// `initialize` 返回的 instructions（系统提示注入位，`16`/`18` 接线）。
    pub instructions: Option<String>,
    /// None = 尚未连接或已 shutdown；kill 场景靠 rmcp 报 TransportClosed。
    client: Mutex<Option<Arc<RunningService<RoleClient, LoggingClientHandler>>>>,
    /// stdio server 的子进程 pid（http 为 None；`19` 加固与测试用）。
    child_pid: Option<u32>,
    call_timeout: Duration,
    list_timeout: Duration,
}

impl std::fmt::Debug for McpSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpSource")
            .field("id", &self.id)
            .field("instructions", &self.instructions)
            .field("connected", &self.client.lock().is_ok_and(|c| c.is_some()))
            .finish_non_exhaustive()
    }
}

impl McpSource {
    /// 建立连接并 initialize（默认 [`CONNECT_TIMEOUT_SECS`] 硬超时）。
    pub async fn connect(plugin: &Plugin, server: &McpServerConfig) -> crate::Result<Self> {
        Self::connect_with(plugin, server, Duration::from_secs(CONNECT_TIMEOUT_SECS)).await
    }

    /// 带自定义 connect 超时的 [`Self::connect`]（测试用短超时验证有界性）。
    pub async fn connect_with(
        plugin: &Plugin,
        server: &McpServerConfig,
        connect_timeout: Duration,
    ) -> crate::Result<Self> {
        let id = format!("mcp:{}/{}", plugin.manifest.name, server.name);
        let (client, child_pid) = match server.r#type {
            McpServerType::Stdio => {
                let (transport, stderr) = spawn_stdio(plugin, server)
                    .await
                    .with_context(|| format!("failed to start MCP stdio server `{id}`"))?;
                let child_pid = transport.id();
                if let Some(stderr) = stderr {
                    pipe_stderr_to_logs(&id, stderr);
                }
                let client = handshake(transport, &id, connect_timeout)
                    .await
                    .with_context(|| format!("MCP initialize failed for `{id}`"))?;
                (client, child_pid)
            }
            McpServerType::StreamableHttp => {
                let url = server.url.as_deref().with_context(|| {
                    format!("MCP server `{id}` of type streamable-http has no `url`")
                })?;
                // headers 的"v1 不发送"note 由 `connect_plugin` 统一产出（可见诊断）。
                let transport = StreamableHttpClientTransport::from_uri(url);
                let client = handshake(transport, &id, connect_timeout)
                    .await
                    .with_context(|| format!("MCP connect failed for `{id}` ({url})"))?;
                (client, None)
            }
            McpServerType::Sse => {
                let note = unsupported_server_note(&plugin.manifest.name, server)
                    .expect("Sse always produces a note");
                anyhow::bail!(note);
            }
        };
        let instructions = client
            .peer_info()
            .and_then(|info| info.instructions.clone());
        Ok(Self {
            id,
            server: server.clone(),
            instructions,
            client: Mutex::new(Some(Arc::new(client))),
            child_pid,
            call_timeout: Duration::from_secs(CALL_TIMEOUT_SECS),
            list_timeout: Duration::from_secs(LIST_TIMEOUT_SECS),
        })
    }

    /// stdio server 的子进程 pid（streamable-http server 为 None）。
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// 覆盖单次调用超时（测试用小超时触发超时路径）。
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// 覆盖 list inventory 超时（测试用小超时触发有界失败路径）。
    pub fn with_list_timeout(mut self, timeout: Duration) -> Self {
        self.list_timeout = timeout;
        self
    }

    fn client_arc(&self) -> Option<Arc<RunningService<RoleClient, LoggingClientHandler>>> {
        self.client.lock().ok()?.clone()
    }

    /// 来源内真实工具名：剥掉本 server 前缀（`13` 约定前缀由本来源拼好，
    /// Registry 冲突时再加的插件名前缀由它自己路由回真名）。
    fn real_tool_name<'a>(&self, name: &'a str) -> &'a str {
        let prefix = format!("{}{NAME_SEP}", self.server.name);
        name.strip_prefix(prefix.as_str()).unwrap_or(name)
    }
}

/// stdio：`06` 的 `command` 是单个可执行名或 `./` 插件相对路径（不展开变量）。
async fn spawn_stdio(
    plugin: &Plugin,
    server: &McpServerConfig,
) -> crate::Result<(McpProcess, Option<ChildStderr>)> {
    let (program, command) = stdio_command(plugin, server)?;
    spawn_long_lived_mcp_subprocess(command)
        .await
        .with_context(|| format!("could not execute `{}`", program.display()))
}

/// 组装 stdio server 的 [`Command`]（不 spawn），环境按 ADR 0003 D2 baseline：
/// `env_clear` 后只带 `PATH`/`HOME`/`LANG`（存在才带）+ `PLUGIN_ROOT` +
/// manifest `extensions["dev.instagent"].env` 声明的变量名，server 自身 `env`
/// 键值对最后显式覆盖。provider 凭据等父环境不再泄漏给插件。
/// 与 `hooks.rs` 的白名单同形；共享 helper 的抽取归 todo 06。
fn stdio_command(
    plugin: &Plugin,
    server: &McpServerConfig,
) -> crate::Result<(std::path::PathBuf, Command)> {
    let declared = server
        .command
        .as_deref()
        .context("stdio MCP server requires `command`")?;
    let program: std::path::PathBuf = match declared.strip_prefix("./") {
        Some(rel) => {
            let path = plugin.root.join(rel);
            if !path.exists() {
                anyhow::bail!(
                    "command `./{rel}` not found under plugin root `{}`",
                    plugin.root.display()
                );
            }
            path
        }
        None => std::path::PathBuf::from(declared),
    };
    let mut command = Command::new(&program);
    command.args(&server.args);
    command.env_clear();
    for key in ["PATH", "HOME", "LANG"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("PLUGIN_ROOT", &plugin.root);
    for name in declared_env(plugin) {
        if let Some(value) = std::env::var_os(&name) {
            command.env(name, value);
        }
    }
    for (key, value) in &server.env {
        command.env(key, value);
    }
    if let Some(cwd) = &server.cwd {
        command.current_dir(cwd);
    }
    Ok((program, command))
}

/// manifest `extensions["dev.instagent"].env`：字符串数组，其余形状忽略
///（与 hooks 的声明式 allowlist 同一契约）。
fn declared_env(plugin: &Plugin) -> Vec<String> {
    plugin
        .manifest
        .extensions
        .get(crate::plugin::NAMESPACE)
        .and_then(|ns| ns.get("env"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// stderr 持续接日志（防止管道写满卡死 server），子进程退出后自然结束。
/// 有界增量解码：固定块读取、跨块 UTF-8 不损坏；单行至多保留
/// [`STDERR_MAX_LINE_BYTES`] 头部并附截断说明，剩余字节仍持续排空；
/// 采样窗口内至多 [`STDERR_MAX_LOGS_PER_WINDOW`] 条诊断，超限计数汇总。
/// 超限只丢日志，不杀仍健康的共享 MCP server（v1 transport/载荷内部预算见 roadmap RM04，本次不碰）。
pub const STDERR_MAX_LINE_BYTES: usize = 8 * 1024;
/// 采样窗口内最多产出的 stderr 诊断条数。
pub const STDERR_MAX_LOGS_PER_WINDOW: usize = 20;
/// 采样窗口长度。
pub const STDERR_LOG_WINDOW: Duration = Duration::from_secs(10);
/// 单次读取块大小。
const STDERR_READ_CHUNK_BYTES: usize = 8192;

/// 增量 UTF-8 解码器：跨块缓冲末尾不完整序列，避免切分损坏合法字符；
/// 真坏尾在 `finish` 时按 lossy 出 replacement（与 subprocess 的解码同形，本文件内最小实现）。
#[derive(Default)]
struct StderrUtf8Decoder {
    pending: Vec<u8>,
}

impl StderrUtf8Decoder {
    fn push(&mut self, chunk: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);
        let mut out = String::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            match std::str::from_utf8(&bytes[offset..]) {
                Ok(valid) => {
                    out.push_str(valid);
                    break;
                }
                Err(err) => {
                    let valid_upto = offset + err.valid_up_to();
                    out.push_str(&String::from_utf8_lossy(&bytes[offset..valid_upto]));
                    match err.error_len() {
                        Some(len) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            offset = valid_upto + len;
                        }
                        None => {
                            self.pending.extend_from_slice(&bytes[valid_upto..]);
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&pending).into_owned()
    }
}

/// 采样窗口限速器：窗口内至多 `max_logs` 条，超限计数；`now` 由调用方传入，
/// 测试可注入时间，不用 sleep。
#[derive(Debug)]
struct StderrRateLimiter {
    window_start: Instant,
    window: Duration,
    max_logs: usize,
    emitted: usize,
    suppressed_lines: usize,
    suppressed_bytes: u64,
}

impl StderrRateLimiter {
    fn new(now: Instant, window: Duration, max_logs: usize) -> Self {
        Self {
            window_start: now,
            window,
            max_logs,
            emitted: 0,
            suppressed_lines: 0,
            suppressed_bytes: 0,
        }
    }

    /// 窗口到期则轮转并返回上一窗口的汇总（调用方应先打这条再打当前行）。
    fn rollover_if_due(&mut self, now: Instant) -> Option<String> {
        if now.duration_since(self.window_start) < self.window {
            return None;
        }
        let summary = self.take_summary();
        self.window_start = now;
        self.emitted = 0;
        summary
    }

    /// 本行是否应该打出；false 时计入抑制计数。`line_bytes` 为本行观测总字节。
    fn should_emit(&mut self, line_bytes: u64) -> bool {
        if self.emitted < self.max_logs {
            self.emitted += 1;
            true
        } else {
            self.suppressed_lines += 1;
            self.suppressed_bytes += line_bytes;
            false
        }
    }

    fn take_summary(&mut self) -> Option<String> {
        if self.suppressed_lines == 0 {
            return None;
        }
        let summary = format!(
            "suppressed {} stderr line(s) ({} bytes) in sampling window (rate-limited)",
            self.suppressed_lines, self.suppressed_bytes
        );
        self.suppressed_lines = 0;
        self.suppressed_bytes = 0;
        Some(summary)
    }
}

/// 单行截断渲染：保留头部至多 [`STDERR_MAX_LINE_BYTES`]（吸附字符边界）并附说明。
fn render_truncated_line(kept: &str, total_bytes: usize) -> String {
    format!(
        "{kept}…[line truncated: kept {} of {total_bytes} bytes]",
        kept.len()
    )
}

fn pipe_stderr_to_logs(id: &str, stderr: ChildStderr) {
    let id = id.to_string();
    tokio::spawn(async move {
        drain_stderr_to_logs(&id, stderr).await;
    });
}

fn emit_stderr_line(id: &str, limiter: &mut StderrRateLimiter, kept: &str, discarded: usize) {
    let line = kept.trim_end_matches('\r');
    let total = line.len() + discarded;
    let now = Instant::now();
    if let Some(summary) = limiter.rollover_if_due(now) {
        tracing::warn!("MCP {id} stderr: {summary}");
    }
    if discarded > 0 {
        let rendered = render_truncated_line(line, total);
        if limiter.should_emit(total as u64) {
            tracing::warn!("MCP {id} stderr: {rendered}");
        }
    } else if limiter.should_emit(total as u64) {
        tracing::warn!("MCP {id} stderr: {line}");
    }
}

fn feed_stderr_text(
    text: &str,
    id: &str,
    limiter: &mut StderrRateLimiter,
    kept: &mut String,
    discarded: &mut usize,
) {
    for ch in text.chars() {
        if ch == '\n' {
            emit_stderr_line(id, limiter, kept, *discarded);
            kept.clear();
            *discarded = 0;
        } else if *discarded > 0 {
            *discarded += ch.len_utf8();
        } else if kept.len() + ch.len_utf8() <= STDERR_MAX_LINE_BYTES {
            kept.push(ch);
        } else {
            // 本行首次越限：头部已满，余下记丢弃并持续排空。
            *discarded += ch.len_utf8();
        }
    }
}

async fn drain_stderr_to_logs(id: &str, mut stderr: ChildStderr) {
    let mut decoder = StderrUtf8Decoder::default();
    let mut limiter = StderrRateLimiter::new(
        Instant::now(),
        STDERR_LOG_WINDOW,
        STDERR_MAX_LOGS_PER_WINDOW,
    );
    // 当前行保留头部（<= 上限）+ 丢弃计数；超限后持续排空到换行/EOF。
    let mut kept = String::new();
    let mut discarded: usize = 0;
    let mut chunk = [0u8; STDERR_READ_CHUNK_BYTES];

    loop {
        let n = match stderr.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::debug!("MCP {id} stderr read ended: {e}");
                break;
            }
        };
        let text = decoder.push(&chunk[..n]);
        feed_stderr_text(&text, id, &mut limiter, &mut kept, &mut discarded);
    }
    let tail = decoder.finish();
    if !tail.is_empty() {
        feed_stderr_text(&tail, id, &mut limiter, &mut kept, &mut discarded);
    }
    if !kept.is_empty() || discarded > 0 {
        emit_stderr_line(id, &mut limiter, &kept, discarded);
    }
    if let Some(summary) = limiter.take_summary() {
        tracing::warn!("MCP {id} stderr: {summary}");
    }
}

/// 任何 IntoTransport 统一走 serve_client（legacy initialize 握手），
/// 整体套 [`connect_timeout`] 硬超时：握手悬死的 server 不拖垮装配
/// （超时 drop future → McpProcess 同步杀进程组 → rmcp 回收直接子进程）。
async fn handshake<T, E, A>(
    transport: T,
    id: &str,
    connect_timeout: Duration,
) -> crate::Result<RunningService<RoleClient, LoggingClientHandler>>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let served = tokio::time::timeout(
        connect_timeout,
        serve_client(LoggingClientHandler, transport),
    )
    .await
    .map_err(|_| anyhow::anyhow!("initialize handshake timed out after {connect_timeout:?}"))?
    .with_context(|| format!("`{id}` transport error"))?;
    Ok(served)
}

/// [`connect_plugin`] 的结果：连上的来源 + 跳过/说明信息。
#[derive(Debug, Default)]
pub struct McpLoadOutcome {
    pub sources: Vec<McpSource>,
    /// sse 跳过、headers 被忽略、单 server 连接失败等可读诊断（`18` CLI 展示），
    /// 每条都自带 plugin/server 来源。
    pub notes: Vec<String>,
}

/// headers 配置的可见诊断：v1 不发送（规范规定 headers 不承载凭据，
/// 远程鉴权不做），必须说出来，不静默忽略。
pub fn ignored_headers_note(plugin: &str, server: &McpServerConfig) -> Option<String> {
    (server.r#type == McpServerType::StreamableHttp && !server.headers.is_empty()).then(|| {
        format!(
            "MCP server `{}` of plugin `{}`: {} header(s) configured but v1 does not send \
             them (remote auth not supported); headers ignored",
            server.name,
            plugin,
            server.headers.len()
        )
    })
}

/// 一个插件的所有 MCP server：`06` 的 [`load_servers`] 逐个建 [`McpSource`]。
/// 单 server 失败（含 sse 跳过、headers 忽略）只产出带 plugin/server 的 note，
/// 其余健康 source 保留；只有连接全部失败、一个可用 source 都没有时才整体
/// 报错（装配策略）。`plugin_data` 是 `${PLUGIN_DATA}` 目录
/// （`<data_dir>/plugins/<name>/`，创建归 `07`，本函数只透传给展开逻辑）。
pub async fn connect_plugin(plugin: &Plugin, plugin_data: &Path) -> crate::Result<McpLoadOutcome> {
    let mut outcome = McpLoadOutcome::default();
    let mut connect_failures = 0usize;
    for server in load_servers(plugin, plugin_data)? {
        if let Some(note) = unsupported_server_note(&plugin.manifest.name, &server) {
            tracing::warn!("{note}");
            outcome.notes.push(note);
            continue;
        }
        if let Some(note) = ignored_headers_note(&plugin.manifest.name, &server) {
            tracing::warn!("{note}");
            outcome.notes.push(note);
        }
        match McpSource::connect(plugin, &server).await {
            Ok(source) => outcome.sources.push(source),
            Err(err) => {
                connect_failures += 1;
                let note = format!(
                    "MCP server `{}` of plugin `{}` failed: {err:#}",
                    server.name, plugin.manifest.name
                );
                tracing::warn!("{note}");
                outcome.notes.push(note);
            }
        }
    }
    if outcome.sources.is_empty() && connect_failures > 0 {
        anyhow::bail!(
            "no usable MCP server in plugin `{}`: {}",
            plugin.manifest.name,
            outcome.notes.join("; ")
        );
    }
    Ok(outcome)
}

#[async_trait]
impl ToolSource for McpSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn list(&self) -> Vec<ToolSpec> {
        self.inventory().await.unwrap_or_default()
    }

    /// inventory 失败以 `Err(note)` 上报（超时/断连不静默当空工具列表），
    /// note 带来源 id 与有界原因（`10`）。
    async fn inventory(&self) -> Result<Vec<ToolSpec>, String> {
        let Some(client) = self.client_arc() else {
            let note = format!(
                "MCP `{}`: not connected, tool inventory unavailable",
                self.id
            );
            tracing::warn!("{note}");
            return Err(note);
        };
        let outcome = tokio::time::timeout(self.list_timeout, client.list_all_tools()).await;
        match outcome {
            Err(_elapsed) => {
                let note = format!(
                    "MCP `{}`: list_tools timed out after {}s",
                    self.id,
                    self.list_timeout.as_secs_f64()
                );
                tracing::warn!("{note}");
                Err(note)
            }
            Ok(Err(e)) => {
                let note = format!(
                    "MCP `{}`: list_tools failed: {}",
                    self.id,
                    bounded_note(&e.to_string())
                );
                tracing::warn!("{note}");
                Err(note)
            }
            Ok(Ok(tools)) => Ok(tools
                .iter()
                .map(|tool| ToolSpec {
                    name: format!("{}{NAME_SEP}{}", self.server.name, tool.name),
                    description: tool
                        .description
                        .clone()
                        .map(|d| d.into_owned())
                        .unwrap_or_default(),
                    input_schema: Value::Object(tool.input_schema.as_ref().clone()),
                    read_only: tool
                        .annotations
                        .as_ref()
                        .and_then(|a| a.read_only_hint)
                        .unwrap_or_default(),
                })
                .collect()),
        }
    }

    async fn call(&self, name: &str, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let real = self.real_tool_name(name);
        let arguments = match input {
            Value::Object(map) => map,
            other => {
                return ToolOutput::err(format!(
                    "MCP tool `{name}` expects a JSON object input, got {}",
                    short_value(other)
                ));
            }
        };
        let Some(client) = self.client_arc() else {
            return ToolOutput::err(format!(
                "MCP tool `{name}` unavailable: server `{}` is not connected",
                self.id
            ));
        };
        let params = CallToolRequestParams::new(real.to_string()).with_arguments(arguments);
        let outcome = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                return ToolOutput::err(format!("MCP tool call `{name}` cancelled"));
            }
            r = tokio::time::timeout(self.call_timeout, client.call_tool(params)) => r,
        };
        match outcome {
            Err(_elapsed) => ToolOutput::err(format!(
                "MCP tool call `{name}` on `{}` timed out after {}s",
                self.id,
                self.call_timeout.as_secs_f64()
            )),
            Ok(Err(e)) => ToolOutput::err(format!(
                "MCP tool call `{name}` on `{}` failed: {e}",
                self.id
            )),
            Ok(Ok(result)) => call_result_to_output(&result),
        }
    }

    /// 取消服务任务 → worker 退出 → transport drop → 子进程 kill_on_drop。
    async fn shutdown(&self) {
        if let Some(client) = self.client.lock().expect("mcp client lock").take() {
            client.cancellation_token().cancel();
        }
    }
}

fn call_result_to_output(result: &CallToolResult) -> ToolOutput {
    let mut blocks = Vec::new();
    let mut non_text = 0usize;
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => blocks.push(text.text.clone()),
            _ => non_text += 1,
        }
    }
    let mut text = blocks.join("\n");
    if non_text > 0 {
        // v1 只支持文本：图片/资源等非 text 块给占位说明。
        text.push_str(&format!("\n[{non_text} non-text content block(s) omitted]"));
    }
    if text.is_empty() {
        text = match &result.structured_content {
            Some(structured) => structured.to_string(),
            None => String::new(),
        };
    }
    ToolOutput {
        text,
        is_error: result.is_error.unwrap_or(false),
        image: None,
    }
}

fn short_value(value: Value) -> String {
    let text = value.to_string();
    let mut truncated: String = text.chars().take(80).collect();
    if truncated.len() < text.len() {
        truncated.push('…');
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PluginManifest;
    use crate::plugin::PluginSource;
    use std::collections::BTreeMap;

    fn plugin_with_manifest(manifest_json: &str, root: &Path) -> Plugin {
        Plugin {
            manifest: serde_json::from_str::<PluginManifest>(manifest_json).unwrap(),
            root: root.to_path_buf(),
            source: PluginSource::User,
        }
    }

    fn server(env: &[(&str, &str)]) -> McpServerConfig {
        McpServerConfig {
            name: "srv".to_string(),
            r#type: McpServerType::Stdio,
            command: Some("env".to_string()),
            args: vec![],
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
        }
    }

    #[test]
    fn declared_env_reads_manifest_string_array_only() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = plugin_with_manifest(
            r#"{"name":"demo","version":"0.1.0",
                "extensions":{"dev.instagent":{"env":["A","B",42]}}}"#,
            tmp.path(),
        );
        assert_eq!(
            declared_env(&plugin),
            vec!["A".to_string(), "B".to_string()]
        );

        let plugin = plugin_with_manifest(r#"{"name":"demo","version":"0.1.0"}"#, tmp.path());
        assert!(declared_env(&plugin).is_empty());
    }

    #[tokio::test]
    async fn stdio_command_uses_d2_env_baseline_not_parent_env() {
        // 父环境里放一个"密钥"：baseline 不含它的名字 → 子进程必须看不到。
        std::env::set_var("INSTAGENT_MCP_UNDECLARED_SECRET", "leak");
        std::env::set_var("INSTAGENT_MCP_DECLARED", "allowed");
        let tmp = tempfile::tempdir().unwrap();
        let plugin = plugin_with_manifest(
            r#"{"name":"demo","version":"0.1.0",
                "extensions":{"dev.instagent":{"env":["INSTAGENT_MCP_DECLARED"]}}}"#,
            tmp.path(),
        );
        let (program, mut command) =
            stdio_command(&plugin, &server(&[("SERVER_VAR", "v")])).expect("build env command");
        assert_eq!(program, std::path::PathBuf::from("env"));
        command.stdout(std::process::Stdio::piped());
        let out = command.spawn().unwrap().wait_with_output().await.unwrap();
        let text = String::from_utf8_lossy(&out.stdout);

        assert!(
            !text.contains("INSTAGENT_MCP_UNDECLARED_SECRET"),
            "未声明的父环境变量泄漏给了 MCP 子进程:\n{text}"
        );
        assert!(
            !text.contains("CARGO_MANIFEST_DIR"),
            "父环境未被 env_clear，cargo 会话变量整串泄漏:\n{text}"
        );
        assert!(
            text.contains(&format!("PLUGIN_ROOT={}", tmp.path().display())),
            "PLUGIN_ROOT 必须经环境变量传递:\n{text}"
        );
        assert!(text.contains("INSTAGENT_MCP_DECLARED=allowed"), "{text}");
        assert!(text.contains("SERVER_VAR=v"), "{text}");
    }

    #[test]
    fn ignored_headers_note_only_for_http_with_headers() {
        let mut http = server(&[]);
        http.r#type = McpServerType::StreamableHttp;
        assert!(ignored_headers_note("demo", &http).is_none());

        http.headers
            .insert("X-Auth".to_string(), "token-xyz".to_string());
        let note = ignored_headers_note("demo", &http).expect("http + headers must be visible");
        assert!(note.contains("srv") && note.contains("demo"), "{note}");
        assert!(note.contains("does not send"), "{note}");

        let mut sse = http.clone();
        sse.r#type = McpServerType::Sse;
        assert!(ignored_headers_note("demo", &sse).is_none());
    }

    #[test]
    fn bounded_note_caps_error_echo() {
        let huge = "x".repeat(MAX_NOTE_CHARS * 3);
        let note = bounded_note(&huge);
        assert_eq!(note.chars().count(), MAX_NOTE_CHARS + 1); // 上限 + 省略号
        assert!(note.ends_with('…'));
        assert_eq!(bounded_note("short"), "short");
    }

    #[test]
    fn stderr_decoder_reassembles_split_unicode_without_corruption() {
        let text = "中文🙂 mixed héllo 日本";
        let bytes = text.as_bytes();
        for cut in 0..=bytes.len() {
            let mut decoder = StderrUtf8Decoder::default();
            let mut out = decoder.push(&bytes[..cut]);
            out.push_str(&decoder.push(&bytes[cut..]));
            out.push_str(&decoder.finish());
            assert_eq!(out, text, "cut at byte {cut}");
        }
        // 逐字节喂入 equally 不损坏。
        let mut decoder = StderrUtf8Decoder::default();
        let mut out = String::new();
        for i in 0..bytes.len() {
            out.push_str(&decoder.push(&bytes[i..i + 1]));
        }
        out.push_str(&decoder.finish());
        assert_eq!(out, text);
        assert!(!out.contains(char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn stderr_truncated_line_keeps_bounded_head_with_note() {
        assert_eq!(STDERR_MAX_LINE_BYTES, 8 * 1024);
        let huge = "y".repeat(STDERR_MAX_LINE_BYTES + 100);
        // 模拟 feed 的封顶逻辑：头部 <= 上限，余下记丢弃。
        let mut kept = String::new();
        let mut discarded = 0usize;
        for ch in huge.chars() {
            if discarded > 0 {
                discarded += ch.len_utf8();
            } else if kept.len() + ch.len_utf8() <= STDERR_MAX_LINE_BYTES {
                kept.push(ch);
            } else {
                discarded += ch.len_utf8();
            }
        }
        assert_eq!(kept.len(), STDERR_MAX_LINE_BYTES);
        assert_eq!(discarded, 100);
        let rendered = render_truncated_line(&kept, kept.len() + discarded);
        assert!(rendered.len() < huge.len() + 100, "截断后必须有界");
        assert!(rendered.contains("line truncated"), "{rendered}");
        assert!(rendered.contains(&format!("of {} bytes", kept.len() + discarded)));
    }

    #[test]
    fn stderr_rate_limiter_caps_window_with_injected_time() {
        let start = Instant::now();
        let mut limiter =
            StderrRateLimiter::new(start, Duration::from_secs(10), STDERR_MAX_LOGS_PER_WINDOW);
        // 同一窗口内突发 100 行：只放行上限条。
        let mut emitted = 0;
        for _ in 0..100 {
            if limiter.should_emit(10) {
                emitted += 1;
            }
        }
        assert_eq!(emitted, STDERR_MAX_LOGS_PER_WINDOW);
        let summary = limiter.take_summary().expect("超限须有汇总");
        assert!(summary.contains("80"), "{summary}");
        assert!(summary.contains("rate-limited"), "{summary}");
        // 注入时间推进到下一窗口：轮转后可继续打出并先吐上一窗口汇总（此处已取走则无）。
        let later = start + Duration::from_secs(11);
        assert!(limiter.rollover_if_due(later).is_none());
        assert!(limiter.should_emit(10));
    }
}
