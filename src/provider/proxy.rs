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
//! - 总就绪期限 = `timeout_secs`（默认 [`ProxyDef::DEFAULT_TIMEOUT_SECS`]），
//!   全部候选尝试共享这一期限，重试不重新获得完整 timeout；每次探针请求与
//!   轮询 sleep 各取自身上限与剩余期限的较小值。候选在就绪前提前退出（可能
//!   是选端口与 bind 之间被抢注）时换端口重试，至多额外 [`MAX_PORT_RETRIES`]
//!   次；spawn 失败、配置无效与期限耗尽立即失败。端口选择是
//!   bind→释放→子进程 bind，竞争只能缓解不能根除，原子端口交接协议属
//!   roadmap（RM06）。
//! - 轮询 `GET http://127.0.0.1:{port}{ready}` 到 200 为止（`ready` 默认
//!   [`ProxyDef::DEFAULT_READY`]）；子进程提前退出时报告退出码与探针 URL，
//!   不傻等超时。
//! - 就绪后所有请求走指向本地端点的 [`OpenAiProvider`]。
//! - 连接失败（[`ProviderError::Transport`]）自动重启（新端口新进程）；并发
//!   失败由 tokio 锁串行协调并按实例代次识别，后到者复用已替换的新实例，
//!   不重复拉起——每次调用至多一次重启，不循环拉起。
//! - 会话结束 drop 时 kill：子进程句柄由 `03` 的 [`ProcessGroupChild`] 持有，
//!   Drop 对进程组 SIGKILL，不留残听；被取消/被取代的候选实例同样经 drop 回收。
//! - 环境变量透传白名单：只有 `PATH` `HOME` `LANG` + 插件声明的 `env`
//!   （§7 风险 6）。

use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;
use std::time::Instant;

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
/// 首个候选就绪前退出后，换端口重试的额外次数上限（总尝试 = 本值 + 1）。
/// 只缓解选端口与子进程 bind 之间的竞争；超限即带退出诊断放弃。
const MAX_PORT_RETRIES: usize = 2;

/// 一次成功拉起的结果：子进程句柄 + 指向本地端点的 openai 引擎。
#[derive(Debug)]
struct Running {
    inner: OpenAiProvider,
    child: ProcessGroupChild,
    port: u16,
    /// 实例代次：每次成功重启替换 +1；并发重启用它识别"已被别人换过"。
    generation: u64,
}

#[derive(Debug)]
pub struct ProxyProvider {
    /// 原始 def（engine=proxy），重启时重新拉起用。
    def: ProviderDef,
    plugin_root: PathBuf,
    /// 子进程句柄收在这里：`ProxyProvider` drop → `Running` drop →
    /// `03` 的进程组 SIGKILL（会话结束 kill）。
    running: Mutex<Running>,
    /// 重启串行锁：并发连接失败只放一个请求去拉新实例，其余等结果复用。
    restart_lock: tokio::sync::Mutex<()>,
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
            restart_lock: tokio::sync::Mutex::new(()),
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

    /// 连接失败自动重启（调用方已观察到 `observed_generation` 代次的实例失败）。
    ///
    /// tokio 锁串行化并发重启：拿到锁后先比对当前代次——已被别的请求替换过
    /// 就直接复用新实例，不重复拉起；否则先在新端口拉起并等就绪，成功后才
    /// 替换状态（旧子进程句柄随之 drop，按 `03` 整组 SIGKILL），新实例起不
    /// 来则保持原状态并返回错误。全程不持 [`std::sync::Mutex`] guard await。
    async fn restart(&self, observed_generation: u64) -> Result<OpenAiProvider> {
        let _serial = self.restart_lock.lock().await;
        let (generation, inner) = {
            let guard = self.guard();
            (guard.generation, guard.inner.clone())
        };
        if generation != observed_generation {
            return Ok(inner);
        }
        let mut next = launch(&self.def, &self.plugin_root).await?;
        next.generation = generation + 1;
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
        let (inner, generation) = {
            let guard = self.guard();
            (guard.inner.clone(), guard.generation)
        };
        let first = inner.stream(rebuild()).await;
        if !matches!(first, Err(ProviderError::Transport(_))) {
            return first;
        }
        let inner = self
            .restart(generation)
            .await
            .map_err(|err| ProviderError::Transport(format!("proxy restart failed: {err:#}")))?;
        inner.stream(rebuild()).await
    }
}

/// 拉起一个新实例：选空闲端口 → spawn → 就绪轮询 → 构造本地 openai 引擎。
///
/// 全部候选共享同一个总就绪期限（受检时间运算，极大 `timeout_secs` 报可读
/// 错误而不是 panic）；候选就绪前提前退出（可重试，典型是选定的端口在释放
/// 后被抢注）时换端口重试，至多额外 [`MAX_PORT_RETRIES`] 次；spawn 失败、
/// 配置无效与期限耗尽（不可重试）立即失败。最终错误保留 provider、命令、
/// 尝试次数与退出状态。
async fn launch(def: &ProviderDef, plugin_root: &Path) -> Result<Running> {
    let proxy = def.proxy.as_ref().with_context(|| {
        format!(
            "provider `{}` (engine proxy) missing proxy section",
            def.name
        )
    })?;
    let timeout = Duration::from_secs(proxy.timeout_secs.unwrap_or(ProxyDef::DEFAULT_TIMEOUT_SECS));
    let deadline = Instant::now().checked_add(timeout).with_context(|| {
        format!(
            "provider `{}`: readiness timeout {timeout:?} is too large",
            def.name
        )
    })?;
    let program = resolve_program(&proxy.command, plugin_root)?;
    let probe = reqwest::Client::builder()
        .build()
        .context("build proxy readiness probe client")?;
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let port = free_port()?;
        let outcome = spawn_and_wait(def, proxy, &program, port, &probe, deadline, timeout).await;
        let err = match outcome {
            Ok(child) => {
                let inner = build_inner(def, port)?;
                return Ok(Running {
                    inner,
                    child,
                    port,
                    generation: 0,
                });
            }
            // 就绪前提前退出：还有尝试次数与剩余期限就换端口重试。
            Err(LaunchFail::Exited(_))
                if attempt <= MAX_PORT_RETRIES && Instant::now() < deadline =>
            {
                continue;
            }
            Err(LaunchFail::Exited(err)) | Err(LaunchFail::Fatal(err)) => err,
        };
        return Err(err.context(format!(
            "provider `{}`: proxy launch failed after {attempt} attempt(s)",
            def.name
        )));
    }
}

/// bind `127.0.0.1:0` 让内核选空闲端口，读取后立即释放交给子进程。
fn free_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("pick a free port for the proxy")?;
    Ok(listener.local_addr().context("read free port")?.port())
}

/// 候选启动失败分类：决定换端口重试还是立即失败。
enum LaunchFail {
    /// 子进程在就绪前退出（退出状态已含在错误里）：可能只是端口被抢注，
    /// 换端口可重试。
    Exited(anyhow::Error),
    /// spawn 失败、轮询失败或总期限耗尽：不可重试。
    Fatal(anyhow::Error),
}

/// spawn + 轮询就绪。探针请求超时与轮询 sleep 都取自身上限与剩余总期限的
/// 较小值——重试不重新获得完整 timeout。失败路径（提前退出 / 期限耗尽）里
/// child 被 drop → `03` 的 Drop 对进程组 SIGKILL，不留残留。
async fn spawn_and_wait(
    def: &ProviderDef,
    proxy: &ProxyDef,
    program: &Path,
    port: u16,
    probe: &reqwest::Client,
    deadline: Instant,
    timeout: Duration,
) -> Result<ProcessGroupChild, LaunchFail> {
    let mut command = build_command(proxy, program, port);
    let mut child = ProcessGroupChild::spawn(&mut command).map_err(|err| {
        LaunchFail::Fatal(
            anyhow::Error::new(err).context(format!("spawn proxy command `{}`", proxy.command)),
        )
    })?;
    let url = ready_url(proxy, port);
    loop {
        match child.child_mut().try_wait() {
            Ok(Some(status)) => {
                return Err(LaunchFail::Exited(anyhow::anyhow!(
                    "proxy command `{}` for provider `{}` exited ({status}) before ready at {url}",
                    proxy.command,
                    def.name
                )));
            }
            Ok(None) => {}
            Err(err) => {
                return Err(LaunchFail::Fatal(
                    anyhow::Error::new(err).context("poll proxy child"),
                ))
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(LaunchFail::Fatal(anyhow::anyhow!(
                "provider `{}`: proxy not ready: GET {url} did not return 200 within {timeout:?} \
                 (command `{}`)",
                def.name,
                proxy.command
            )));
        }
        if let Ok(resp) = probe
            .get(&url)
            .timeout(PROBE_TIMEOUT.min(remaining))
            .send()
            .await
        {
            if resp.status() == reqwest::StatusCode::OK {
                return Ok(child);
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
        }
    }
}

/// 组装子进程命令：`${PORT}` 替换 + env 清空只留白名单与插件声明。
fn build_command(proxy: &ProxyDef, program: &Path, port: u16) -> tokio::process::Command {
    let port = port.to_string();
    let mut command = tokio::process::Command::new(program);
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
    command
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
            display_name: None,
            description: None,
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

    #[tokio::test]
    async fn huge_readiness_timeout_is_rejected_without_panic() {
        let mut def = proxy_def("./whatever");
        def.proxy.as_mut().unwrap().timeout_secs = Some(u64::MAX);
        let err = ProxyProvider::start(&def, Path::new("/"))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("too large"), "{msg}");
    }
}
