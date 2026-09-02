//! `CommandTools`：`dev.instagent/tools/*.json`，脚本即工具（第三版 §2.9）。
//!
//! TODO(15)：填实现。input JSON 写 stdin，stdout 作为结果，非零退出码即
//! is_error；`${PLUGIN_ROOT}` 展开；用 `03` 进程组；工具名 `<plugin>__<tool>`。

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::plugin::PluginSet;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::tools::ToolSource;
use crate::tools::ToolSpec;

/// 单个 command tool 定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// 可执行命令；`${PLUGIN_ROOT}` 在调用前展开。
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug)]
pub struct CommandTools {
    /// `cmd:<plugin>`。
    pub id: String,
    pub tools: Vec<CommandToolDef>,
}

impl CommandTools {
    /// 每个启用插件解析出一个实例（无 tools 目录的跳过）。TODO(15)
    pub fn load(_plugins: &PluginSet) -> crate::Result<Vec<CommandTools>> {
        todo!("TODO(15)")
    }
}

#[async_trait]
impl ToolSource for CommandTools {
    fn id(&self) -> &str {
        &self.id
    }

    async fn list(&self) -> Vec<ToolSpec> {
        todo!("TODO(15)")
    }

    async fn call(&self, _name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
        todo!("TODO(15)")
    }
}
