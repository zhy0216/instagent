//! Headless CLI: execute a complete task, manage persisted sessions and plugins.
//! Runtime setup lives in [`assembly`]; no terminal input or REPL is provided.

pub mod assembly;
pub mod handlers;
pub mod render;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "instagent", version, about = "插件为核心的 headless agent")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Execute an unattended task and exit
    Run(RunArgs),
    /// Manage persisted sessions
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// Manage plugins before running tasks
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Stream assistant text to stdout, diagnostics to stderr
    #[default]
    Text,
    /// Write one terminal JSON result to stdout
    Json,
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("input").required(true).multiple(false)))]
pub struct RunArgs {
    /// Complete task text; stdin is never read
    #[arg(short = 't', long, group = "input")]
    pub task: Option<String>,
    /// Read the complete task from a regular UTF-8 file
    #[arg(long, group = "input")]
    pub task_file: Option<PathBuf>,
    /// Plugin task template, qualified as plugin:name
    #[arg(long, group = "input")]
    pub command: Option<String>,
    /// Literal arguments substituted into the task template
    #[arg(long, requires = "command", conflicts_with_all = ["task", "task_file"])]
    pub args: Option<String>,
    /// Append this task to an existing session (id or last)
    #[arg(long)]
    pub resume: Option<String>,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    #[arg(short, long)]
    pub model: Option<String>,
    #[arg(long = "plugin")]
    pub plugin: Vec<PathBuf>,
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFormat,
    /// Run deadline in seconds, followed by at most five seconds for cleanup
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=604800))]
    pub timeout: u64,
}

impl Default for RunArgs {
    fn default() -> Self {
        Self {
            task: None,
            task_file: None,
            command: None,
            args: None,
            resume: None,
            cwd: None,
            model: None,
            plugin: Vec::new(),
            output: OutputFormat::Text,
            timeout: 600,
        }
    }
}

#[derive(Subcommand)]
pub enum SessionsAction {
    /// 只读每个会话文件首行
    List,
    Rm {
        id: String,
    },
}

#[derive(Subcommand)]
pub enum PluginAction {
    /// git-url 或本地路径
    Install {
        source: String,
        #[arg(long)]
        auto_update: bool,
    },
    List,
    Update {
        name: Option<String>,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    Show {
        name: String,
    },
}

/// Dispatch a single noninteractive command.
pub async fn run() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run(args) => handlers::run(args).await,
        Commands::Sessions { action } => {
            handlers::sessions(action)?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Plugin { action } => {
            handlers::plugin(action)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `tracing` 初始化：默认 warning 到 stderr（健康路径保持安静），
/// 显式 `RUST_LOG` 优先（todo 08 / D01：hook fail-open、清单失败等 warning
/// 在不设 RUST_LOG 的真实 CLI 中可见；stdout 仍仅答案）。
pub fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let registry = tracing_subscriber::registry().with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false),
    );
    let _ = registry.try_init();
}

/// 本二进制测试进程内的 `INSTAGENT_*` 环境变量锁（约定同 `config.rs`，
/// 不同 crate 测试进程互不影响）。
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 测试夹具：tempdir 覆盖 `INSTAGENT_CONFIG_DIR` / `INSTAGENT_DATA_DIR` /
/// `INSTAGENT_AGENTS_DIR`（约定同各模块单测，全程持 env 锁串行执行）。
#[cfg(test)]
pub(crate) mod fixtures {
    use std::path::{Path, PathBuf};
    use std::sync::MutexGuard;

    use tempfile::TempDir;

    use instagent::plugin::NAMESPACE;

    pub(crate) struct Env {
        pub(crate) _guard: MutexGuard<'static, ()>,
        pub(crate) config: TempDir,
        pub(crate) data: TempDir,
        pub(crate) agents: TempDir,
        pub(crate) cwd: TempDir,
    }

    impl Env {
        pub(crate) fn new() -> Self {
            let guard = crate::cli::env_lock();
            let env = Self {
                _guard: guard,
                config: TempDir::new().unwrap(),
                data: TempDir::new().unwrap(),
                agents: TempDir::new().unwrap(),
                cwd: TempDir::new().unwrap(),
            };
            std::env::set_var("INSTAGENT_CONFIG_DIR", env.config.path());
            std::env::set_var("INSTAGENT_DATA_DIR", env.data.path());
            std::env::set_var("INSTAGENT_AGENTS_DIR", env.agents.path());
            for key in ["INSTAGENT_PROVIDER", "INSTAGENT_MODEL"] {
                std::env::remove_var(key);
            }
            env
        }

        /// 用户插件目录 `<agents>/plugins/<name>/`。
        pub(crate) fn user_plugin(&self, name: &str) -> PathBuf {
            let dir = self.agents.path().join("plugins").join(name);
            write_manifest(&dir, name);
            dir
        }

        pub(crate) fn write_config_yaml(&self, yaml: &str) {
            std::fs::write(self.config.path().join("config.yaml"), yaml).unwrap();
        }
    }

    pub(crate) fn write_manifest(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            format!(
                r#"{{"$schema":"{}","name":"{name}","version":"1.0.0"}}"#,
                instagent::plugin::manifest::PLUGIN_SCHEMA_URL
            ),
        )
        .unwrap();
    }

    pub(crate) fn add_provider(plugin: &Path, def: serde_json::Value) {
        let dir = plugin.join(NAMESPACE).join("providers");
        std::fs::create_dir_all(&dir).unwrap();
        let name = def["name"].as_str().unwrap_or("provider");
        std::fs::write(
            dir.join(format!("{name}.json")),
            serde_json::to_string_pretty(&def).unwrap(),
        )
        .unwrap();
    }

    pub(crate) fn fake_openai_provider(base_url: &str) -> serde_json::Value {
        serde_json::json!({
            "name": "fake",
            "engine": "openai",
            "base_url": base_url,
        })
    }

    pub(crate) fn sse_body(text: &str) -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(text)
    }
}
