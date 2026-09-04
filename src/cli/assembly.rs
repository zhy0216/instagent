//! 运行时装配（第三版 §8）：config + settings → `05` 发现启用插件 →
//! 全部工具源注册进 `13` Registry（BuiltinTools + 每 server 一个 McpSource +
//! CommandTools + SkillsSource）→ `10` registry 取 provider 引擎 →
//! `16` Agent。MCP instructions 与 skill 行注入系统提示；`--plugin PATH`
//! 临时加载。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;

use instagent::agent::Agent;
use instagent::commands::load_commands;
use instagent::commands::SlashCommand;
use instagent::config::Config;
use instagent::hooks::Hooks;
use instagent::plugin::bundled::discover_with_bundled;
use instagent::plugin::install;
use instagent::plugin::install::plugin_data_dir;
use instagent::provider::ProviderRegistry;
use instagent::settings::Settings;
use instagent::tools::mcp::connect_plugin;
use instagent::tools::BuiltinTools;
use instagent::tools::CommandTools;
use instagent::tools::Registry;
use instagent::tools::SkillsSource;

/// 装配入参（一次 chat / run 的命令行层配置）。
#[derive(Debug, Clone)]
pub struct AssemblyOpts {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub cli_plugins: Vec<PathBuf>,
}

/// 装配产物。
pub struct Runtime {
    pub agent: Agent,
    pub slash_commands: Vec<SlashCommand>,
    /// 会话 header 记录（`02`）。
    pub provider_name: String,
    pub model: String,
    /// 装配期间产生的可读提示（skipped / MCP 失败等）。
    pub notes: Vec<String>,
}

/// 完整装配：发现 → 工具源 → provider → Agent。
pub async fn build(opts: &AssemblyOpts) -> instagent::Result<Runtime> {
    let mut notes = Vec::new();

    // 24h 节流的 git 自动更新（`07`；失败只 warn，不挡启动）。
    if let Ok(results) = install::auto_update_all(chrono::Utc::now().timestamp()) {
        for item in results {
            if let Err(err) = item.result {
                notes.push(format!(
                    "auto-update plugin `{}` failed: {err:#}",
                    item.name
                ));
            }
        }
    }

    let mut config = Config::load(&opts.cwd)?;
    if let Some(model) = &opts.model {
        config.model = Some(model.clone());
    }
    let settings = Settings::merged(&opts.cwd)?;
    // `--plugin PATH` 与配置 plugins 同规则：`~` 前缀展开、相对路径按 cwd 解析。
    let cli_plugins: Vec<PathBuf> = opts
        .cli_plugins
        .iter()
        .map(|p| {
            let raw = p.display().to_string();
            let expanded = shellexpand::tilde(&raw);
            let path = PathBuf::from(expanded.as_ref());
            if path.is_relative() {
                opts.cwd.join(path)
            } else {
                path
            }
        })
        .collect();
    let plugins = discover_with_bundled(&opts.cwd, &settings, &config.plugins, &cli_plugins)?;
    for skipped in &plugins.skipped {
        notes.push(format!(
            "skipped plugin dir {}: {}",
            skipped.path.display(),
            skipped.reason
        ));
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
    for plugin in plugins.iter() {
        let data = plugin_data_dir(&plugin.manifest.name)?;
        match connect_plugin(plugin, &data).await {
            // 部分失败：健康 server 照常注册，失败/sse/headers note 已带
            // plugin + server 名（`10`），这里补上插件根路径便于定位。
            Ok(outcome) => {
                notes.extend(outcome.notes);
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
            Err(err) => notes.push(format!(
                "MCP servers of plugin `{}` (root `{}`) failed to start: {err:#}",
                plugin.manifest.name,
                plugin.root.display()
            )),
        }
    }
    for instance in CommandTools::load(&plugins)? {
        registry.register(Arc::new(instance));
    }
    let skills = SkillsSource::discover(&plugins, &opts.cwd)?;
    let skill_lines = skills
        .skills
        .iter()
        .map(|skill| format!("{} — {}", skill.name, skill.description))
        .collect();
    registry.register(Arc::new(skills));

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
    notes.extend(limit_notes);
    let mut agent = Agent::assemble(&config, provider, registry, hooks)?;
    agent.mcp_instructions = mcp_instructions;
    agent.skill_lines = skill_lines;

    Ok(Runtime {
        agent,
        slash_commands: load_commands(&plugins)?,
        provider_name,
        model,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use crate::cli::fixtures::{self, Env};
    use crate::cli::handlers;
    use instagent::message::Content;
    use instagent::session::Session;
    use serde_json::Value;
    use std::path::Path;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::Mock;
    use wiremock::MockServer;

    fn fake_provider_at(plugin: &Path, base_url: &str) {
        fixtures::add_provider(plugin, fixtures::fake_openai_provider(base_url));
    }

    #[tokio::test]
    async fn run_task_end_to_end_with_fake_openai_provider() {
        let env = Env::new();
        let server = MockServer::start().await;
        let provider_dir = env.user_plugin("fakeprov");
        fake_provider_at(&provider_dir, &format!("{}/v1", server.uri()));
        env.write_config_yaml("provider: fake\nmodel: test-model\n");
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi there\"},\"finish_reason\":null}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5}}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(fixtures::sse_body(sse))
            .expect(1)
            .mount(&server)
            .await;

        handlers::run(
            "say hi".to_string(),
            Some(env.cwd.path().to_path_buf()),
            None,
            Vec::new(),
        )
        .await
        .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "test-model");
        let tools: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(tools.contains(&"shell"), "{tools:?}");
        assert!(tools.contains(&"load_skill"), "{tools:?}");

        let headers = Session::list().unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].provider, "fake");
        assert_eq!(headers[0].model, "test-model");
        let session = Session::resume(&headers[0].id).unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, instagent::message::Role::User);
        match &session.messages[1].content[0] {
            Content::Text(text) => assert_eq!(text, "hi there"),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(session.messages[1].usage.unwrap().output, 5);
    }
}
