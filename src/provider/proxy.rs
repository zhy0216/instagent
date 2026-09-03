//! `engine: "proxy"`（第三版 §2.4）：拉起插件自带的命令，得到本地 openai 兼容
//! 端点后当 openai 引擎用。
//!
//! 生命周期（§7 风险 4 的四种情况：端口冲突、就绪超时、中途崩溃、退出残留）：
//!
//! - 选空闲端口（bind `127.0.0.1:0` 拿端口后释放），替换 `args` / `env` 值 /
//!   `headers` 值里的 `${PORT}`（`10` 的展开按原样保留到这里），并设环境变量
//!   `INSTAGENT_PORT`。
//! - `command` 是单个可执行名（走 PATH）或 `./` 开头的插件相对路径
//!   （约定同 `06` 的 mcp.json）。
//! - 轮询 `GET http://127.0.0.1:{port}{ready}` 到 200 为止（`ready` 默认
//!   [`ProxyDef::DEFAULT_READY`]，超时默认 [`ProxyDef::DEFAULT_TIMEOUT_SECS`]）；
//!   子进程提前退出时报告退出码与探针 URL，不傻等超时。
//! - 就绪后所有请求走指向本地端点的 [`OpenAiProvider`]。
//! - 连接失败（[`ProviderError::Transport`]）自动重启一次（新端口新进程），
//!   重启后的结果直接透传——每次调用至多一次重启，不循环拉起。
//! - 会话结束 drop 时 kill：子进程句柄由 `03` 的 [`ProcessGroupChild`] 持有，
//!   Drop 对进程组 SIGKILL，不留残听。
//! - 环境变量透传白名单：只有 `PATH` `HOME` `LANG` + 插件声明的 `env`
//!   （§7 风险 6）。

use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;
use std::time::Instant;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::error::ProviderError;
use crate::provider::openai::OpenAiProvider;
use crate::provider::EngineKind;
use crate::provider::Provider;
use crate::provider::ProviderDef;
use crate::provider::ProxyDef;
use crate::provider::Request;
use crate::provider::StreamEvent;
use crate::subprocess::ProcessGroupChild;

/// 环境变量透传白名单（第三版 §7 风险 6）。
const ENV_ALLOWLIST: [&str; 3] = ["PATH", "HOME", "LANG"];
/// 端口占位符：`10` 的变量展开原样保留，拉起时在此替换。
const PORT_PLACEHOLDER: &str = "${PORT}";
/// 就绪探针的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(150);
/// 单次就绪探针的请求超时（探测本身要快，失败等下一轮）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// 一次成功拉起的结果：子进程句柄 + 指向本地端点的 openai 引擎。
#[derive(Debug)]
struct Running {
    inner: OpenAiProvider,
    child: ProcessGroupChild,
    port: u16,
}

#[derive(Debug)]
pub struct ProxyProvider {
    /// 原始 def（engine=proxy），重启时重新拉起用。
    def: ProviderDef,
    plugin_root: PathBuf,
    /// 子进程句柄收在这里：`ProxyProvider` drop → `Running` drop →
    /// `03` 的进程组 SIGKILL（会话结束 kill）。
    running: Mutex<Running>,
}

impl ProxyProvider {
    /// 拉起 + 就绪轮询；任一步失败返回可读错误且不留子进程。
    pub async fn start(def: &ProviderDef, plugin_root: &Path) -> Result<Self> {
        anyhow::ensure!(
            def.engine == EngineKind::Proxy,
            "provider {} is not a proxy engine",
            def.name
        );
        let running = launch(def, plugin_root).await?;
        Ok(Self {
            def: def.clone(),
            plugin_root: plugin_root.to_path_buf(),
            running: Mutex::new(running),
        })
    }

    pub fn port(&self) -> u16 {
        self.guard().port
    }

    pub fn endpoint(&self) -> String {
        endpoint_for(self.port())
    }

    fn guard(&self) -> MutexGuard<'_, Running> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前子进程（进程组组长）的 PID：诊断 / 测试用。
    pub fn child_pid(&self) -> Option<u32> {
        self.guard().child.id()
    }

    /// 连接失败自动重启：先在新端口拉起并等就绪，成功后才替换状态——
    /// 旧子进程句柄随之 drop，按 `03` 整组 SIGKILL；新实例起不来则保持
    /// 原状态并返回错误。
    async fn restart(&self) -> Result<OpenAiProvider> {
        let next = launch(&self.def, &self.plugin_root).await?;
        let inner = next.inner.clone();
        let mut guard = self.guard();
        let previous = std::mem::replace(&mut *guard, next);
        drop(guard);
        drop(previous);
        Ok(inner)
    }
}

#[async_trait]
impl Provider for ProxyProvider {
    fn name(&self) -> &str {
        &self.def.name
    }

    async fn stream(
        &self,
        req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let Request {
            model,
            system,
            messages,
            tools,
            max_tokens,
            temperature,
        } = req;
        let rebuild = || Request {
            model,
            system,
            messages,
            tools,
            max_tokens,
            temperature,
        };
        let inner = self.guard().inner.clone();
        let first = inner.stream(rebuild()).await;
        if !matches!(first, Err(ProviderError::Transport(_))) {
            return first;
        }
        let inner = self
            .restart()
            .await
            .map_err(|err| ProviderError::Transport(format!("proxy restart failed: {err:#}")))?;
        inner.stream(rebuild()).await
    }
}

/// 拉起一个新实例：选空闲端口 → spawn → 就绪轮询 → 构造本地 openai 引擎。
async fn launch(def: &ProviderDef, plugin_root: &Path) -> Result<Running> {
    let proxy = def.proxy.as_ref().with_context(|| {
        format!(
            "provider `{}` (engine proxy) missing proxy section",
            def.name
        )
    })?;
    let port = free_port()?;
    let child = spawn_and_wait(def, proxy, port, plugin_root).await?;
    let inner = build_inner(def, port)?;
    Ok(Running { inner, child, port })
}

/// bind `127.0.0.1:0` 让内核选空闲端口，读取后立即释放交给子进程。
fn free_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("pick a free port for the proxy")?;
    Ok(listener.local_addr().context("read free port")?.port())
}

/// spawn + 轮询就绪。失败路径（提前退出 / 超时）里 child 被 drop →
/// `03` 的 Drop 对进程组 SIGKILL，不留残留。
async fn spawn_and_wait(
    def: &ProviderDef,
    proxy: &ProxyDef,
    port: u16,
    plugin_root: &Path,
) -> Result<ProcessGroupChild> {
    let mut command = build_command(proxy, port, plugin_root)?;
    let mut child = ProcessGroupChild::spawn(&mut command)
        .with_context(|| format!("spawn proxy command `{}`", proxy.command))?;
    let url = ready_url(proxy, port);
    let timeout = Duration::from_secs(proxy.timeout_secs.unwrap_or(ProxyDef::DEFAULT_TIMEOUT_SECS));
    let probe = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .context("build proxy readiness probe client")?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.child_mut().try_wait() {
            Ok(Some(status)) => {
                return Err(anyhow::anyhow!(
                    "proxy command `{}` for provider `{}` exited ({status}) before ready at {url}",
                    proxy.command,
                    def.name
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(err).context("poll proxy child"),
        }
        if let Ok(resp) = probe.get(&url).send().await {
            if resp.status() == reqwest::StatusCode::OK {
                return Ok(child);
            }
        }
        if Instant::now() >= deadline {
            drop(child);
            bail!(
                "provider `{}`: proxy not ready: GET {url} did not return 200 within {timeout:?} \
                 (command `{}`)",
                def.name,
                proxy.command
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 组装子进程命令：`${PORT}` 替换 + env 清空只留白名单与插件声明。
fn build_command(
    proxy: &ProxyDef,
    port: u16,
    plugin_root: &Path,
) -> Result<tokio::process::Command> {
    let port = port.to_string();
    let mut command = tokio::process::Command::new(resolve_program(&proxy.command, plugin_root)?);
    for arg in &proxy.args {
        command.arg(replace_port(arg, &port));
    }
    command.env_clear();
    command.env("INSTAGENT_PORT", &port);
    for name in ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for (name, value) in &proxy.env {
        command.env(name, replace_port(value, &port));
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    Ok(command)
}

/// `./` 开头 → 插件根相对路径（须存在）；否则当单个可执行名交给 PATH 解析
/// （绝对路径同样直接可用，由 spawn 报错）。
fn resolve_program(command: &str, plugin_root: &Path) -> Result<PathBuf> {
    if let Some(rel) = command.strip_prefix("./") {
        let path = plugin_root.join(rel);
        anyhow::ensure!(
            path.exists(),
            "proxy command `{command}` not found under plugin root `{}`",
            plugin_root.display()
        );
        return Ok(path);
    }
    Ok(PathBuf::from(command))
}

/// def 副本改写成本地端点的 openai 引擎（`09` 作为最终 engine）；
/// `base_url` 按约定写到 `/v1`（引擎自己拼 `/chat/completions`），
/// headers 里的 `${PORT}` 一并展开。
fn build_inner(def: &ProviderDef, port: u16) -> Result<OpenAiProvider> {
    let mut local = def.clone();
    local.engine = EngineKind::Openai;
    local.base_url = Some(format!("{}/v1", endpoint_for(port)));
    let port = port.to_string();
    for value in local.headers.values_mut() {
        *value = replace_port(value, &port);
    }
    OpenAiProvider::new(&local).with_context(|| format!("provider `{}`", def.name))
}

fn ready_url(proxy: &ProxyDef, port: u16) -> String {
    let ready = proxy.ready.as_deref().unwrap_or(ProxyDef::DEFAULT_READY);
    let path = if ready.starts_with('/') {
        ready.to_owned()
    } else {
        format!("/{ready}")
    };
    format!("{}{path}", endpoint_for(port))
}

fn endpoint_for(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn replace_port(text: &str, port: &str) -> String {
    text.replace(PORT_PLACEHOLDER, port)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn proxy_def(command: &str) -> ProviderDef {
        ProviderDef {
            name: "px".to_string(),
            engine: EngineKind::Proxy,
            api_key_env: None,
            base_url: None,
            headers: BTreeMap::from([("x-target-port".to_string(), "${PORT}".to_string())]),
            timeout_seconds: None,
            models: vec![],
            proxy: Some(ProxyDef {
                command: command.to_string(),
                args: vec!["--port".to_string(), "${PORT}".to_string()],
                env: BTreeMap::from([(
                    "PX_URL".to_string(),
                    "http://127.0.0.1:${PORT}".to_string(),
                )]),
                ready: None,
                timeout_secs: None,
            }),
        }
    }

    #[test]
    fn resolve_program_joins_plugin_root_for_dot_slash() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("probe-target"), b"x").unwrap();
        let resolved = resolve_program("./probe-target", root).unwrap();
        assert_eq!(resolved, root.join("probe-target"));
        let err = resolve_program("./missing-cmd", root).unwrap_err();
        assert!(
            err.to_string().contains("not found under plugin root"),
            "{err}"
        );
        // 可执行名 / 绝对路径原样交给 PATH 解析。
        assert_eq!(
            resolve_program("some-binary", root).unwrap(),
            PathBuf::from("some-binary")
        );
    }

    #[test]
    fn build_inner_rewrites_def_to_local_openai_endpoint() {
        let def = proxy_def("./whatever");
        let inner = build_inner(&def, 1234).unwrap();
        assert_eq!(inner.def.engine, EngineKind::Openai);
        assert_eq!(
            inner.def.base_url.as_deref(),
            Some("http://127.0.0.1:1234/v1")
        );
        // headers 里的 ${PORT} 同步展开；原始 def 不变。
        assert_eq!(
            inner.def.headers.get("x-target-port").map(String::as_str),
            Some("1234")
        );
        assert_eq!(def.engine, EngineKind::Proxy);
        assert_eq!(
            def.headers.get("x-target-port").map(String::as_str),
            Some("${PORT}")
        );
    }

    #[test]
    fn ready_url_defaults_and_normalizes() {
        let def = proxy_def("./whatever");
        let proxy = def.proxy.as_ref().unwrap();
        assert_eq!(ready_url(proxy, 9999), "http://127.0.0.1:9999/v1/models");
        let custom = ProxyDef {
            command: "c".to_string(),
            args: vec![],
            env: BTreeMap::new(),
            ready: Some("healthz".to_string()),
            timeout_secs: None,
        };
        assert_eq!(ready_url(&custom, 7), "http://127.0.0.1:7/healthz");
        assert_eq!(replace_port("a${PORT}b", "51"), "a51b");
    }

    #[tokio::test]
    async fn start_rejects_non_proxy_engine_and_missing_section() {
        let mut def = proxy_def("./whatever");
        def.engine = EngineKind::Openai;
        let err = ProxyProvider::start(&def, Path::new("/"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a proxy engine"), "{err}");

        let mut def = proxy_def("./whatever");
        def.proxy = None;
        let err = ProxyProvider::start(&def, Path::new("/"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing proxy section"), "{err}");
    }
}
