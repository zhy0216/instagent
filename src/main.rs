//! CLI 入口骨架（A4；第二版 §2.11）。TODO(18)：填四个子命令的处理体与
//! 运行时装配（config → 插件 → 工具源 → provider → loop）。`src/cli/**`
//! 由 `18` 自行建模块（在 main.rs 里声明）。

use clap::Parser;
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "instagent", version, about = "插件为核心的最小 agent")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 交互式 REPL（rustyline；/exit /clear /compact /mode /tools /help）
    Chat {
        /// 恢复会话：id 或 "last"
        #[arg(long)]
        resume: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(short, long)]
        model: Option<String>,
        /// auto | approve | chat
        #[arg(long)]
        mode: Option<instagent::config::Mode>,
        /// 开发时临时加载插件路径
        #[arg(long = "plugin")]
        plugin: Vec<PathBuf>,
    },
    /// 无交互跑一条任务（审批按 auto，结束打印最终回复和 usage）
    Run {
        #[arg(short = 't', long)]
        task: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long)]
        mode: Option<instagent::config::Mode>,
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
enum SessionsAction {
    /// 只读每个会话文件首行
    List,
    Rm {
        id: String,
    },
}

#[derive(Subcommand)]
enum PluginAction {
    /// git-url 或本地路径
    Install {
        source: String,
        #[arg(long)]
        auto_update: bool,
        /// 跳过信任确认
        #[arg(long = "yes")]
        yes: bool,
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

fn main() {
    let cli = Cli::parse();
    match cli.command {
        // TODO(18)
        Commands::Chat { .. } => todo!("chat: todos/18-cli.md"),
        Commands::Run { .. } => todo!("run: todos/18-cli.md"),
        Commands::Sessions { .. } => todo!("sessions: todos/18-cli.md"),
        Commands::Plugin { .. } => todo!("plugin: todos/18-cli.md"),
    }
}
