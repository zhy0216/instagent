//! `SkillsSource`：规范组件 skills 的运行时（第三版 §2.6）。只暴露一个工具
//! `load_skill(name, file?)`；启动时只收集 name + description 进系统提示
//! （由 `16`/`18` 接线），调用时才读正文或 `references/` 文件。
//!
//! TODO(15)：填实现。发现范围：启用插件 `skills/`（只看一层）+
//! `~/.agents/skills/` + `<project>/.agents/skills/`；插件内命名
//! `<plugin>:<skill>`；工具描述文本抄 goose `skills/client.rs:91~103`
//! （搬运注明出处）。

use std::path::Path;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::plugin::PluginSet;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;
use crate::tools::ToolSource;
use crate::tools::ToolSpec;

/// SKILL.md frontmatter（Agent Skills 规范）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    /// 必需，1~64，小写字母数字和 `-`，必须等于目录名。
    pub name: String,
    /// 必需，≤1024。
    pub description: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub compatibility: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    /// 实验字段，v2 给审批白名单加临时项。
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Option<Vec<String>>,
}

/// 一个已发现 skill 的元信息（进系统提示的那一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    /// `<plugin>:<skill>` 或裸名（用户/项目目录来源）。
    pub name: String,
    pub description: String,
    /// skill 目录（含 SKILL.md）。
    pub root: PathBuf,
}

#[derive(Debug, Default)]
pub struct SkillsSource {
    pub skills: Vec<SkillMeta>,
}

impl SkillsSource {
    /// 发现 + frontmatter 校验（无效跳过不报错）。TODO(15)
    pub fn discover(_plugins: &PluginSet, _cwd: &Path) -> crate::Result<SkillsSource> {
        todo!("TODO(15)")
    }
}

#[async_trait]
impl ToolSource for SkillsSource {
    fn id(&self) -> &str {
        "skills"
    }

    /// 只有一个 `load_skill`。TODO(15)
    async fn list(&self) -> Vec<ToolSpec> {
        todo!("TODO(15)")
    }

    async fn call(&self, _name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
        todo!("TODO(15)")
    }
}

/// 拆分并解析 `---` frontmatter，返回（元信息，正文）。TODO(15)
pub fn parse_frontmatter(_text: &str) -> crate::Result<(SkillFrontmatter, String)> {
    todo!("TODO(15)")
}
