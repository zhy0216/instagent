//! provider 装配：插件 JSON → engine 实例（第三版 §2.4；模块表见第三版 §4）。
//!
//! TODO(10)：填实现。扫描启用插件（含 bundled）的 `dev.instagent/providers/*.json`；
//! 重名报错并要求 `plugin/name`；用户插件覆盖 bundled；变量展开
//! `${env:NAME}` / `${PLUGIN_ROOT}` / `${PLUGIN_DATA}`（未定义 env → 可读错误）；
//! engine 分派 openai → `09`，`proxy` / `anthropic` 分支先 `bail!` 占位
//! （分别由 `11` / `12` 接上，勿删占位分支）；context_limit 四级顺序：
//! 配置覆盖 → provider models 表 → `08` 前缀小表 → 128k。

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::config::Config;
use crate::plugin::PluginSet;
use crate::provider::Provider;
use crate::provider::ProviderDef;

/// 全部可用 provider 定义（来自启用插件）。
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    pub providers: Vec<ProviderDef>,
}

impl ProviderRegistry {
    pub fn from_plugins(_plugins: &PluginSet, _config: &Config) -> crate::Result<Self> {
        todo!("TODO(10)")
    }

    /// 按名字查找并构造引擎实例；重名要求写 `plugin/name`。TODO(10)
    pub async fn get(&self, _name: &str) -> crate::Result<Arc<dyn Provider>> {
        todo!("TODO(10)")
    }

    /// 全部可用名（错误提示 / 补全用）。
    pub fn names(&self) -> Vec<String> {
        self.providers.iter().map(|p| p.name.clone()).collect()
    }

    /// 该 (provider, model) 的上下文上限（四级顺序，配置覆盖优先）。TODO(10)
    pub fn context_limit(&self, _provider: &str, _model: &str, _config: &Config) -> u32 {
        todo!("TODO(10)")
    }
}

/// 解析单个 provider JSON（含变量展开与 `engine` 合法性检查）。TODO(10)
pub fn parse_provider_def(_json: &str) -> crate::Result<ProviderDef> {
    todo!("TODO(10)")
}

/// provider JSON 的变量展开：`${env:NAME}`、`${PLUGIN_ROOT}`、`${PLUGIN_DATA}`；
/// `${PORT}` 留到 `11` 拉起时展开。TODO(10)
pub fn expand_vars(
    _value: &mut Value,
    _plugin_root: &Path,
    _plugin_data: &Path,
) -> crate::Result<()> {
    todo!("TODO(10)")
}
