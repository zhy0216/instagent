//! 运行时装配（第三版 §8）：config + settings → `05` 发现启用插件 →
//! 全部工具源注册进 `13` Registry（BuiltinTools + 每 server 一个 McpSource +
//! CommandTools + SkillsSource）→ `10` registry 取 provider 引擎 →
//! `16` Agent。MCP instructions 与 skill 行注入系统提示；`--plugin PATH`
//! 临时加载；未信任插件拒绝拉起任何命令（信任确认在 [`crate::cli::trust`]）。

use std::io::BufRead;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;

use instagent::agent::Agent;
use instagent::commands::load_commands;
use instagent::commands::SlashCommand;
use instagent::config::Config;
use instagent::config::Mode;
use instagent::hooks::Hooks;
use instagent::plugin::bundled::discover_with_bundled;
use instagent::plugin::install;
use instagent::plugin::install::plugin_data_dir;
use instagent::plugin::Plugin;
use instagent::plugin::PluginSet;
use instagent::provider::EngineKind;
use instagent::provider::ProviderRegistry;
use instagent::settings::Settings;
use instagent::tools::mcp::connect_plugin;
use instagent::tools::BuiltinTools;
use instagent::tools::CommandTools;
use instagent::tools::Registry;
use instagent::tools::SkillsSource;

use super::trust;

/// 装配入参（一次 chat / run 的命令行层配置）。
#[derive(Debug, Clone)]
pub struct AssemblyOpts {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub mode: Option<Mode>,
    pub cli_plugins: Vec<PathBuf>,
    /// `--yes`：信任确认一律同意。
    pub assume_yes: bool,
    /// 是否可以交互询问信任（`run -t` 无交互为 false）。
    pub interactive: bool,
}

/// 信任 / 确认提问的 IO 注入（测试用模拟输入）。
pub struct Prompter<'a> {
    pub input: &'a mut dyn BufRead,
    pub output: &'a mut dyn Write,
}

/// 装配产物。
pub struct Runtime {
    pub agent: Agent,
    pub slash_commands: Vec<SlashCommand>,
    /// 会话 header 记录（`02`）。
    pub provider_name: String,
    pub model: String,
    /// 装配期间产生的可读提示（skipped / 未信任 / MCP 失败等）。
    pub notes: Vec<String>,
}

/// 完整装配：发现 → 信任门控 → 工具源 → provider → Agent。
pub async fn build(opts: &AssemblyOpts, prompter: &mut Prompter<'_>) -> instagent::Result<Runtime> {
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
    if let Some(mode) = opts.mode {
        config.mode = mode;
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

    // 信任门控：会执行命令的组件需要 trustedPlugins（或本次确认 / --yes）。
    let mut trusted = settings.trusted_plugins.clone();
    let mut gates: Vec<(&Plugin, Vec<trust::Surface>, bool)> = Vec::new();
    for plugin in plugins.iter() {
        let surfaces = trust::plugin_surfaces(plugin)?;
        let granted = if surfaces.is_empty() || trusted.contains(&plugin.manifest.name) {
            true
        } else if opts.assume_yes {
            trust::persist_trust(&plugin.manifest.name)?;
            trusted.push(plugin.manifest.name.clone());
            true
        } else if opts.interactive {
            trust::ensure_trusted(
                plugin,
                &surfaces,
                &mut trusted,
                false,
                prompter.input,
                prompter.output,
            )?
        } else {
            false
        };
        if !granted {
            notes.push(format!(
                "plugin `{}` is not trusted: its {} command(s) (mcp/hook/tool/provider) \
                 will not be started; run it interactively once to confirm, or \
                 `instagent plugin enable {}` / `--yes`",
                plugin.manifest.name,
                surfaces.len(),
                plugin.manifest.name
            ));
        }
        gates.push((plugin, surfaces, granted));
    }
    let trusted_set = PluginSet {
        plugins: gates
            .iter()
            .filter(|(_, _, g)| *g)
            .map(|(p, _, _)| (*p).clone())
            .collect(),
        skipped: Vec::new(),
    };

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
    let def = providers.lookup(&provider_name)?;
    if def.engine == EngineKind::Proxy {
        // proxy 引擎会拉起本地命令：未信任插件直接拒绝。
        if let Ok(plugin) = providers.provider_plugin(&provider_name) {
            if !gates
                .iter()
                .any(|(p, _, granted)| *granted && p.manifest.name == plugin)
            {
                bail!(
                    "provider `{provider_name}` comes from untrusted plugin `{plugin}` \
                     (engine proxy starts a local command); trust it first \
                     (`instagent plugin enable {plugin}` and answer yes, or --yes)"
                );
            }
        }
    }
    let provider = providers.get(&provider_name).await?;

    // 工具源：内置 + MCP（每个 server 一个 McpSource）+ command tools + skills。
    let mut registry = Registry::new();
    registry.register(Arc::new(BuiltinTools::new(
        config.shell.clone().map(PathBuf::from),
    )));
    let mut mcp_instructions = Vec::new();
    for plugin in trusted_set.iter() {
        let data = plugin_data_dir(&plugin.manifest.name)?;
        match connect_plugin(plugin, &data).await {
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
                "MCP servers of plugin `{}` failed to start: {err:#}",
                plugin.manifest.name
            )),
        }
    }
    for instance in CommandTools::load(&trusted_set)? {
        registry.register(Arc::new(instance));
    }
    let skills = SkillsSource::discover(&plugins, &opts.cwd)?;
    let skill_lines = skills
        .skills
        .iter()
        .map(|skill| format!("{} — {}", skill.name, skill.description))
        .collect();
    registry.register(Arc::new(skills));

    // hooks：只加载受信插件（未信任的插件拒绝拉起任何命令）。
    let hooks = Hooks::load(&trusted_set)?;
    let hooks = if hooks.entries.is_empty() {
        None
    } else {
        Some(hooks)
    };

    // context_limit 四级顺序（`10`）：塞回 config 让 `16` assemble 取用。
    config.context_limit = Some(providers.context_limit(&provider_name, &model, &config));
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
    use super::*;
    use crate::cli::fixtures::{self, Env};
    use crate::cli::handlers;
    use instagent::message::Content;
    use instagent::session::Session;
    use serde_json::Value;
    use std::io::Cursor;
    use std::path::Path;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::Mock;
    use wiremock::MockServer;

    fn opts(env: &Env, cli_plugins: Vec<PathBuf>) -> AssemblyOpts {
        AssemblyOpts {
            cwd: env.cwd.path().to_path_buf(),
            model: None,
            mode: None,
            cli_plugins,
            assume_yes: false,
            interactive: true,
        }
    }

    fn prompt<'a>(reader: &'a mut Cursor<Vec<u8>>, out: &'a mut Vec<u8>) -> Prompter<'a> {
        Prompter {
            input: reader,
            output: out,
        }
    }

    async fn tool_names(rt: &Runtime) -> Vec<String> {
        rt.agent
            .tools
            .list()
            .await
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }

    fn fake_provider_at(plugin: &Path, base_url: &str) {
        fixtures::add_provider(plugin, fixtures::fake_openai_provider(base_url));
    }

    #[tokio::test]
    async fn untrusted_exec_plugin_is_gated_prompting_y_trusts_it() {
        let env = Env::new();
        let provider_dir = env.user_plugin("fakeprov");
        fake_provider_at(&provider_dir, "http://127.0.0.1:1/v1");
        env.write_config_yaml("provider: fake\nmodel: test-model\n");
        // execpl 经 `--plugin PATH` 临时加载（同时覆盖 S1 的临时加载项）。
        let dev_root = env.cwd.path().join("devplugins");
        std::fs::create_dir_all(&dev_root).unwrap();
        let exec_dir = dev_root.join("execpl");
        fixtures::write_manifest(&exec_dir, "execpl");
        fixtures::add_exec_surfaces(&exec_dir);

        // 回答 n：execpl 的 hook / command tool 一律不装载，给可读提示。
        let mut reader = Cursor::new(b"n\n".to_vec());
        let mut out = Vec::new();
        let rt = build(
            &opts(&env, vec![exec_dir.clone()]),
            &mut prompt(&mut reader, &mut out),
        )
        .await
        .unwrap();
        assert!(rt.agent.hooks.is_none(), "未信任插件的 hooks 不该加载");
        let names = tool_names(&rt).await;
        assert!(!names.contains(&"execpl__weather".to_string()), "{names:?}");
        assert!(
            rt.notes
                .iter()
                .any(|n| n.contains("`execpl` is not trusted")),
            "{:?}",
            rt.notes
        );
        assert!(trust::user_trusted().unwrap().is_empty());

        // 回答 y：hooks 加载、工具可见、trustedPlugins 落盘。
        let mut reader = Cursor::new(b"y\n".to_vec());
        let mut out = Vec::new();
        let rt = build(
            &opts(&env, vec![exec_dir]),
            &mut prompt(&mut reader, &mut out),
        )
        .await
        .unwrap();
        assert!(rt.agent.hooks.is_some(), "信任后 hooks 应加载");
        let names = tool_names(&rt).await;
        assert!(names.contains(&"execpl__weather".to_string()), "{names:?}");
        assert_eq!(trust::user_trusted().unwrap(), vec!["execpl".to_string()]);
        let transcript = String::from_utf8(out).unwrap();
        assert!(transcript.contains("guard.sh"), "{transcript}");
    }

    #[tokio::test]
    async fn untrusted_proxy_provider_bails_with_readable_hint() {
        let env = Env::new();
        let provider_dir = env.user_plugin("fakeprov");
        fake_provider_at(&provider_dir, "http://127.0.0.1:1/v1");
        let exec_dir = env.user_plugin("execpl");
        fixtures::add_exec_surfaces(&exec_dir);
        fixtures::add_provider(&exec_dir, fixtures::proxy_provider());
        env.write_config_yaml("provider: pxy\nmodel: m\n");

        let mut reader = Cursor::new(b"n\n".to_vec());
        let mut out = Vec::new();
        let err = match build(&opts(&env, vec![]), &mut prompt(&mut reader, &mut out)).await {
            Ok(_) => panic!("untrusted proxy provider must not start"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("untrusted plugin `execpl`"), "{err}");
        assert!(err.contains("proxy"), "{err}");
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
