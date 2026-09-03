//! CLI（第二版 §2.11；第三版 §8 完整图景）：
//! clap 入口 + chat / run / sessions / plugin 四个子命令与运行时装配。
//!
//! 本模块只在 `main.rs` 里声明（`src/cli/**` 不在 lib 模块树，`00` 的约定）；
//! 装配见 [`assembly`]，REPL 见 [`repl`]。

pub mod assembly;
pub mod handlers;
pub mod render;
pub mod repl;

use clap::Parser;
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "instagent", version, about = "插件为核心的最小 agent")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// 交互式 REPL（rustyline；/exit /clear /compact /tools /help）
    Chat {
        /// 恢复会话：id 或 "last"
        #[arg(long)]
        resume: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(short, long)]
        model: Option<String>,
        /// 开发时临时加载插件路径
        #[arg(long = "plugin")]
        plugin: Vec<PathBuf>,
    },
    /// 无交互跑一条任务（结束打印最终回复和 usage）
    Run {
        #[arg(short = 't', long)]
        task: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long = "plugin")]
        plugin: Vec<PathBuf>,
    },
    /// 会话管理
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// 插件管理
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
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

/// `main` 入口：分发四个子命令。
pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Chat {
            resume,
            cwd,
            model,
            plugin,
        } => handlers::chat(resume, cwd, model, plugin).await,
        Commands::Run {
            task,
            cwd,
            model,
            plugin,
        } => handlers::run(task, cwd, model, plugin).await,
        Commands::Sessions { action } => handlers::sessions(action),
        Commands::Plugin { action } => handlers::plugin(action),
    }
}

/// `tracing` 初始化：默认关（REPL 输出干净），`RUST_LOG` 打开。
pub fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
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
