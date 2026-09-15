//! 运行时装配（第三版 §8）：config + settings → `05` 发现启用插件 →
//! 全部工具源注册进 `13` Registry（BuiltinTools + 每 server 一个 McpSource +
//! CommandTools + SkillsSource）→ `10` registry 取 provider 引擎 →
//! `16` Agent。MCP instructions 与 skill 行注入系统提示；`--plugin PATH`
//! 临时加载。

use super::{Capabilities, Diagnostic, DiagnosticCode};
use futures::{stream, StreamExt};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;

use crate::agent::Agent;
use crate::commands::load_commands;
use crate::commands::TaskTemplate;
use crate::config::Config;
use crate::hooks::Hooks;
use crate::plugin::bundled::discover_with_bundled;
use crate::plugin::install::plugin_data_dir;
use crate::provider::ProviderRegistry;
use crate::settings::Settings;
use crate::tools::mcp::{connect_plugin_with_limiter, CONNECT_CONCURRENCY};
use crate::tools::BuiltinTools;
use crate::tools::CommandTools;
use crate::tools::Registry;
use crate::tools::SkillsSource;

/// 装配入参（一次 run 的命令行层配置）。
#[derive(Debug, Clone)]
pub struct AssemblyOpts {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub cli_plugins: Vec<PathBuf>,
    pub capabilities: Capabilities,
}

/// 装配产物。
pub struct Runtime {
    pub agent: Agent,
    pub task_templates: Vec<TaskTemplate>,
    /// 会话 header 记录（`02`）。
    pub provider_name: String,
    pub model: String,
}

/// 完整装配：发现 → 工具源 → provider → Agent。
pub async fn build(opts: &AssemblyOpts, notes: &mut Vec<Diagnostic>) -> crate::Result<Runtime> {
    for names in [
        opts.capabilities.plugins.as_deref(),
        opts.capabilities.tools.as_deref(),
        Some(opts.capabilities.required_tools.as_slice()),
    ]
    .into_iter()
    .flatten()
    {
        if names.iter().any(|name| name.trim().is_empty()) {
            bail!("capability names must not be empty or whitespace");
        }
    }

    let mut config = Config::load(&opts.cwd)?;
    if let Some(provider) = &opts.provider {
        config.provider = Some(provider.clone());
    }
    if let Some(model) = &opts.model {
        config.model = Some(model.clone());
    }
    // T2：CLI override 合并后再校验（todo 08 / D03）：空白 `-m` 等在
    // provider/MCP 启动前报错，不等到 assemble 才含糊失败。
    config.validate_merged("merged config (config.yaml + env + CLI -m/--model)")?;
    let settings = Settings::merged(&opts.cwd)?;
    // `--plugin PATH` 与配置 plugins 同规则：`~` 前缀展开、相对路径按 cwd 解析。
    let cli_plugins: Vec<PathBuf> = opts
        .cli_plugins
        .iter()
        .map(|p| crate::config::expand_plugin_path(&p.display().to_string(), &opts.cwd))
        .collect();
    let mut plugins = discover_with_bundled(&opts.cwd, &settings, &config.plugins, &cli_plugins)?;
    for skipped in &plugins.skipped {
        notes.push(Diagnostic {
            code: DiagnosticCode::PluginSkipped,
            source: skipped.path.display().to_string(),
            message: format!(
                "skipped plugin dir {}: {}",
                skipped.path.display(),
                skipped.reason
            ),
        });
    }
    if let Some(names) = &opts.capabilities.plugins {
        let missing: Vec<_> = names
            .iter()
            .filter(|name| !plugins.iter().any(|p| &p.manifest.name == *name))
            .cloned()
            .collect();
        for name in &missing {
            notes.push(Diagnostic {
                code: DiagnosticCode::PluginUnavailable,
                source: name.clone(),
                message: format!("selected plugin `{name}` is unavailable or disabled"),
            });
        }
        if !missing.is_empty() {
            bail!("selected plugins unavailable: {}", missing.join(", "));
        }
        plugins.plugins.retain(|p| names.contains(&p.manifest.name));
    }

    // provider：注册表按名字取引擎（重名要求 plugin/name 消歧，`10`）。
    let providers = ProviderRegistry::from_plugins(&plugins)?;
    let provider_name = match config.provider.clone().filter(|p| !p.is_empty()) {
        Some(name) => name,
        None => {
            let names = providers.names();
            match names.as_slice() {
                [] => bail!(
                    "no provider available: install a provider plugin or set config \
                     `provider:` / INSTAGENT_PROVIDER"
                ),
                [only] => only.clone(),
                _ => bail!(
                    "config `provider:` is not set; available providers: {}",
                    names.join(", ")
                ),
            }
        }
    };
    let model = config
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("no model configured: set config `model:` / -m MODEL / INSTAGENT_MODEL")
        })?;
    let provider = providers.get(&provider_name).await?;

    // 工具源：内置 + MCP（每个 server 一个 McpSource）+ command tools + skills。
    let mut registry = Registry::new();
    registry.register(Arc::new(BuiltinTools::new(
        config.shell.clone().map(PathBuf::from),
    )));
    let mut mcp_instructions = Vec::new();
    let limiter = Arc::new(tokio::sync::Semaphore::new(CONNECT_CONCURRENCY));
    let pending: Vec<_> = plugins
        .iter()
        .map(|plugin| {
            let limiter = limiter.clone();
            async move {
                let result = match plugin_data_dir(&plugin.manifest.name) {
                    Ok(data) => connect_plugin_with_limiter(plugin, &data, limiter).await,
                    Err(error) => Err(error),
                };
                (plugin, result)
            }
        })
        .collect();
    let connections = stream::iter(pending).buffered(CONNECT_CONCURRENCY);
    tokio::pin!(connections);
    while let Some((plugin, result)) = connections.next().await {
        match result {
            Ok(outcome) => {
                notes.extend(outcome.notes.into_iter().map(|message| Diagnostic {
                    code: DiagnosticCode::McpUnavailable,
                    source: plugin.manifest.name.clone(),
                    message,
                }));
                for source in outcome.sources {
                    if let Some(instructions) = &source.instructions {
                        mcp_instructions.push(format!(
                            "MCP server `{}` (plugin `{}`): {instructions}",
                            source.server.name, plugin.manifest.name
                        ));
                    }
                    registry.register(Arc::new(source));
                }
            }
            Err(err) => notes.push(Diagnostic {
                code: DiagnosticCode::McpUnavailable,
                source: plugin.manifest.name.clone(),
                message: format!(
                    "MCP servers of plugin `{}` (root `{}`) failed to start: {err:#}",
                    plugin.manifest.name,
                    plugin.root.display()
                ),
            }),
        }
    }
    for instance in CommandTools::load(&plugins)? {
        registry.register(Arc::new(instance));
    }
    let skills = SkillsSource::discover(&plugins, &opts.cwd)?;
    let mut skill_lines: Vec<String> = skills
        .skills
        .iter()
        .map(|skill| format!("{} — {}", skill.name, skill.description))
        .collect();
    registry.register(Arc::new(skills));
    if let Some(names) = &opts.capabilities.tools {
        registry.restrict_to(names.iter().cloned());
        if !names.iter().any(|name| name == "load_skill") {
            skill_lines.clear();
        }
    }

    // hooks：加载全体发现的插件。
    let hooks = Hooks::load(&plugins)?;
    let hooks = if hooks.entries.is_empty() {
        None
    } else {
        Some(hooks)
    };

    // context_limit 四级顺序（`10`）：塞回 config 让 `16` assemble 取用；
    // 未知/歧义 provider 的降级告警（todo 08 / R13）进 notes 给用户看。
    let (context_limit, limit_notes) = providers.context_limit(&provider_name, &model, &config);
    config.context_limit = Some(context_limit);
    notes.extend(limit_notes.into_iter().map(|message| Diagnostic {
        code: DiagnosticCode::Configuration,
        source: provider_name.clone(),
        message,
    }));
    let mut agent = Agent::assemble(&config, provider, registry, hooks)?;
    agent.mcp_instructions = mcp_instructions;
    agent.skill_lines = skill_lines;

    Ok(Runtime {
        agent,
        task_templates: load_commands(&plugins)?,
        provider_name,
        model,
    })
}
