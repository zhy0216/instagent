//! 工具层：`ToolSource` trait + Registry + 命名规则（第三版 §2.5；第二版 §2.4）。
//!
//! 四个来源实现：[`BuiltinTools`]（内核 5 工具）、[`McpSource`]（插件 mcp.json
//! 的每个 server 一个实例）、[`CommandTools`]（`dev.instagent/tools/*.json`）、
//! [`SkillsSource`]（只暴露 load_skill）。
//!
//! 命名规则：内置不加前缀；MCP `<server>__<tool>`；command tools
//! `<plugin>__<tool>`（前缀由各来源在 `list()` 里拼好）；同名冲突时 Registry
//! 再给后来者加插件名前缀。OpenAI 函数名只允许 `[A-Za-z0-9_-]{1,64}`：非法字符
//! 替换成 `_`，超 64 截断到 58 + 6 位 FNV-1a 哈希；双向映射表在 [`Registry`]
//! 里，来源注册顺序与内容不变时映射稳定。

pub mod builtin;
pub mod command;
pub mod mcp;
pub mod skills;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use builtin::BuiltinTools;
pub use command::CommandTools;
pub use mcp::McpSource;
pub use skills::SkillsSource;

/// MCP / command 工具名分隔符（与 goose 相同）。
pub const NAME_SEP: &str = "__";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// schema 手写 JSON，不用 schemars（第二版 §2.4）。
    pub input_schema: Value,
    pub read_only: bool,
}

/// 图片负载：base64 数据 + media type（图片支持方案 §数据模型）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageData {
    /// base64 标准字母表编码的原始字节。
    pub data: String,
    /// image/png | image/jpeg | image/gif | image/webp。
    pub media_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageData>,
}

impl ToolOutput {
    pub fn ok(text: String) -> Self {
        Self {
            text,
            is_error: false,
            image: None,
        }
    }

    pub fn err(text: String) -> Self {
        Self {
            text,
            is_error: true,
            image: None,
        }
    }
}

/// 一次待执行的工具调用（由 `Content::ToolUse` 而来）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// 工具执行环境：会话目录 + 取消令牌。
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
}

/// 工具来源（第三版 §2.5）。替换的是实现，不是动态加载。
#[async_trait]
pub trait ToolSource: Send + Sync {
    /// `"builtin"` | `"mcp:<plugin>/<server>"` | `"cmd:<plugin>"` | `"skills"`
    fn id(&self) -> &str;

    async fn list(&self) -> Vec<ToolSpec>;

    /// 带可见失败的清单枚举（`10`）：`Err(note)` 表示本次没有拿到可信清单
    /// （超时 / 断连 / transport 不支持），调用方不得当成"该来源没有工具"。
    /// note 自带来源标识。默认实现回落到 [`Self::list`]：本地来源不会失败。
    async fn inventory(&self) -> Result<Vec<ToolSpec>, String> {
        Ok(self.list().await)
    }

    async fn call(&self, name: &str, input: Value, ctx: &ToolCtx) -> ToolOutput;

    async fn shutdown(&self) {}
}

/// 模型可见名 → 真实路由（来源下标 + 来源 `list()` 里报出的原名）。
#[derive(Debug, Clone)]
struct Route {
    source: usize,
    name: String,
}

/// 双向映射表：visible → route，(source, real) → visible。
#[derive(Default)]
struct Routes {
    forward: HashMap<String, Route>,
}

/// 汇总多个来源，按模型可见名路由（名字映射表在实现里，双向且会话内稳定）。
/// 工具清单与 route 缓存（`10`）：首次 [`Self::list`] 枚举各来源后缓存，
/// 后续每轮直接命中，不再重复网络枚举；[`Self::invalidate`]（连接 / 配置变化、
/// 缓存外调用名未命中时的自动重建）令缓存作废。
#[derive(Default)]
pub struct Registry {
    pub sources: Vec<Arc<dyn ToolSource>>,
    routes: Mutex<Routes>,
    cached_specs: Mutex<Option<Vec<ToolSpec>>>,
    list_errors: Mutex<Vec<String>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, source: Arc<dyn ToolSource>) {
        self.invalidate();
        self.sources.push(source);
    }

    /// 令工具清单缓存作废（下一次 [`Self::list`] 重新枚举）。连接或配置变化
    /// 时由装配层调用；会话内新建的 Registry 天然不会命中旧清单。
    pub fn invalidate(&self) {
        *self.cached_specs.lock().expect("registry cache lock") = None;
        *self.routes.lock().expect("registry routes lock") = Routes::default();
    }

    /// 最近一次枚举中各来源上报的失败 note（超时 / 断连等，note 自带来源
    /// id）。清单被缓存时 note 一并冻结，随下一次重新枚举刷新。
    pub fn list_errors(&self) -> Vec<String> {
        self.list_errors
            .lock()
            .expect("registry notes lock")
            .clone()
    }

    /// 汇总各来源 spec，解决同名冲突（后来者加插件名前缀），再套
    /// [`model_visible_name`] 生成模型可见名，刷新双向映射表并缓存结果。
    /// 来源经 [`ToolSource::inventory`] 上报的失败不进清单、不缓存成"空工具"，
    /// 收进 [`Self::list_errors`] 并写 warning。
    pub async fn list(&self) -> Vec<ToolSpec> {
        if let Some(cached) = self
            .cached_specs
            .lock()
            .expect("registry cache lock")
            .clone()
        {
            return cached;
        }
        let specs = self.enumerate().await;
        *self.cached_specs.lock().expect("registry cache lock") = Some(specs.clone());
        specs
    }

    async fn enumerate(&self) -> Vec<ToolSpec> {
        let mut specs = Vec::new();
        let mut routes = Routes::default();
        let mut errors = Vec::new();

        for (idx, source) in self.sources.iter().enumerate() {
            let prefix = conflict_prefix(source.id());
            let listed = match source.inventory().await {
                Ok(specs) => specs,
                Err(note) => {
                    tracing::warn!("{note}");
                    errors.push(note);
                    continue;
                }
            };
            for spec in listed {
                let mut candidate = spec.name.clone();
                let mut retry = 0usize;
                let visible = loop {
                    let visible = model_visible_name(&candidate);
                    let taken = routes
                        .forward
                        .get(&visible)
                        .is_some_and(|route| !(route.source == idx && route.name == spec.name));
                    if !taken {
                        break visible;
                    }
                    retry += 1;
                    candidate = if retry == 1 {
                        format!("{prefix}{NAME_SEP}{}", spec.name)
                    } else {
                        format!("{prefix}{NAME_SEP}{}{NAME_SEP}{retry}", spec.name)
                    };
                };
                routes.forward.insert(
                    visible.clone(),
                    Route {
                        source: idx,
                        name: spec.name.clone(),
                    },
                );
                let mut spec = spec;
                spec.name = visible;
                specs.push(spec);
            }
        }

        *self.routes.lock().expect("registry routes lock") = routes;
        *self.list_errors.lock().expect("registry notes lock") = errors;
        specs
    }

    /// 按映射表路由回真实 (source, name)；未命中时先重建一次映射再试。
    pub async fn call(&self, call: &ToolCall, ctx: &ToolCtx) -> ToolOutput {
        let route = self.lookup(&call.name).await;
        match route {
            Some(route) => {
                self.sources[route.source]
                    .call(&route.name, call.input.clone(), ctx)
                    .await
            }
            None => ToolOutput::err(format!("unknown tool: {}", call.name)),
        }
    }

    async fn lookup(&self, visible: &str) -> Option<Route> {
        if let Some(route) = self.lookup_cached(visible) {
            return Some(route);
        }
        // 缓存外名字未命中：连接/清单可能已变，作废后重建一次再试。
        self.invalidate();
        self.list().await;
        self.lookup_cached(visible)
    }

    fn lookup_cached(&self, visible: &str) -> Option<Route> {
        let routes = self.routes.lock().expect("registry routes lock");
        routes.forward.get(visible).cloned()
    }

    pub async fn shutdown(&self) {
        for source in &self.sources {
            source.shutdown().await;
        }
    }
}

/// 从来源 id 提取插件名，用于冲突时再加一层前缀：
/// `"mcp:<plugin>/<server>"` / `"cmd:<plugin>"` → `<plugin>`；其余原样。
fn conflict_prefix(id: &str) -> String {
    let rest = id.split_once(':').map_or(id, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// FNV-1a 64 位取低 24 位，输出 6 位十六进制（不加依赖的最小稳定哈希）。
fn short_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:06x}", hash & 0x00ff_ffff)
}

/// OpenAI 函数名只允许 `[A-Za-z0-9_-]{1,64}`：非法字符替换成 `_`，超长截断到
/// 58 字符再接原名的 6 位哈希（共 64）。纯函数、确定性，映射表由 Registry 维护。
pub fn model_visible_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.len() > 64 {
        let mut truncated: String = sanitized.chars().take(58).collect();
        truncated.push_str(&short_hash(name));
        truncated
    } else if sanitized.is_empty() {
        "tool".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct DummySource {
        id: String,
        names: Vec<String>,
    }

    impl DummySource {
        fn new(id: &str, names: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                names: names.iter().map(|name| name.to_string()).collect(),
            })
        }
    }

    #[async_trait]
    impl ToolSource for DummySource {
        fn id(&self) -> &str {
            &self.id
        }

        async fn list(&self) -> Vec<ToolSpec> {
            self.names
                .iter()
                .map(|name| ToolSpec {
                    name: name.clone(),
                    description: format!("dummy {name}"),
                    input_schema: json!({"type": "object"}),
                    read_only: false,
                })
                .collect()
        }

        async fn call(&self, name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
            ToolOutput::ok(format!("{}::{name}", self.id))
        }
    }

    fn ctx_in(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn model_visible_name_sanitizes_invalid_chars() {
        assert_eq!(model_visible_name("web.search:v1"), "web_search_v1");
        assert_eq!(model_visible_name("ok-name_1"), "ok-name_1");
    }

    #[test]
    fn model_visible_name_truncates_with_hash_and_is_stable() {
        let long = "a".repeat(100);
        let visible = model_visible_name(&long);
        assert_eq!(visible.len(), 64);
        assert!(visible.starts_with(&"a".repeat(58)));
        assert_eq!(visible, model_visible_name(&long), "同一会话内稳定不变");

        // 前缀相同、后缀不同的两个超长名字不会撞车。
        let other = format!("{}b", "a".repeat(99));
        assert_ne!(visible, model_visible_name(&other));
    }

    #[test]
    fn tool_output_json_shape_is_backward_compatible() {
        let out = ToolOutput::ok("hi".into());
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            r#"{"text":"hi","is_error":false}"#,
            "无 image 时序列化结果与加字段前逐字一致"
        );
        let old: ToolOutput = serde_json::from_str(r#"{"text":"hi","is_error":false}"#).unwrap();
        assert_eq!(old.image, None);
    }

    #[tokio::test]
    async fn registry_resolves_name_conflicts_with_plugin_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());

        let mut registry = Registry::new();
        registry.register(DummySource::new("mcp:alpha/tools", &["dup"]));
        registry.register(DummySource::new("cmd:beta", &["dup"]));
        registry.register(DummySource::new("cmd:gamma", &["dup"]));
        // 插件名恰好也叫 beta：加前缀后仍撞 "beta__dup" → 再挂序号。
        registry.register(DummySource::new("mcp:beta/tools2", &["dup"]));

        let specs = registry.list().await;
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["dup", "beta__dup", "gamma__dup", "beta__dup__2"],
            "先到先得，后来者加插件名前缀"
        );

        let call = |name: &str| ToolCall {
            id: "t1".to_string(),
            name: name.to_string(),
            input: json!({}),
        };
        assert_eq!(
            registry.call(&call("dup"), &ctx).await,
            ToolOutput::ok("mcp:alpha/tools::dup".to_string())
        );
        assert_eq!(
            registry.call(&call("beta__dup"), &ctx).await,
            ToolOutput::ok("cmd:beta::dup".to_string())
        );
        assert_eq!(
            registry.call(&call("beta__dup__2"), &ctx).await,
            ToolOutput::ok("mcp:beta/tools2::dup".to_string()),
            "加前缀后的可见名要能路由回未加前缀的真名"
        );
        assert!(registry.call(&call("nope"), &ctx).await.is_error);
    }

    #[tokio::test]
    async fn registry_mapping_is_stable_across_lists() {
        let mut registry = Registry::new();
        registry.register(DummySource::new("mcp:p/srv", &["weird tool.name"]));
        registry.register(Arc::new(BuiltinTools::new(None)));

        let first = registry.list().await;
        let second = registry.list().await;
        assert_eq!(first, second);
        assert!(first.iter().any(|spec| spec.name == "weird_tool_name"));
        assert!(first.iter().any(|spec| spec.name == "shell"));
    }

    /// 可计数、内容可变的来源：验证缓存与失效。
    struct CountingSource {
        id: String,
        enumerations: std::sync::atomic::AtomicUsize,
        names: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ToolSource for CountingSource {
        fn id(&self) -> &str {
            &self.id
        }

        async fn call(&self, name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
            ToolOutput::ok(format!("{}::{name}", self.id))
        }

        async fn list(&self) -> Vec<ToolSpec> {
            self.enumerations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.names
                .lock()
                .unwrap()
                .iter()
                .map(|name| ToolSpec {
                    name: name.clone(),
                    description: "counting".to_string(),
                    input_schema: json!({"type": "object"}),
                    read_only: false,
                })
                .collect()
        }
    }

    /// inventory 永远失败的来源（模拟超时 / 断连的远端）。
    struct FailingSource {
        id: String,
    }

    #[async_trait]
    impl ToolSource for FailingSource {
        fn id(&self) -> &str {
            &self.id
        }

        async fn call(&self, _name: &str, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
            ToolOutput::err("dead".to_string())
        }

        async fn list(&self) -> Vec<ToolSpec> {
            Vec::new()
        }

        async fn inventory(&self) -> Result<Vec<ToolSpec>, String> {
            Err(format!("MCP `{}`: list_tools timed out", self.id))
        }
    }

    #[tokio::test]
    async fn registry_caches_inventory_until_invalidated() {
        let source = Arc::new(CountingSource {
            id: "mcp:p/srv".to_string(),
            enumerations: std::sync::atomic::AtomicUsize::new(0),
            names: Mutex::new(vec!["tool_a".to_string()]),
        });
        let mut registry = Registry::new();
        registry.register(source.clone());

        let first = registry.list().await;
        assert_eq!(
            first.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["tool_a"]
        );
        let second = registry.list().await;
        assert_eq!(first, second);
        assert_eq!(
            source
                .enumerations
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "第二轮必须命中缓存，不重复枚举"
        );

        // 配置/连接变化后不命中旧 inventory：改内容 → invalidate → 重新枚举。
        *source.names.lock().unwrap() = vec!["tool_b".to_string()];
        assert!(
            registry.list().await.iter().any(|s| s.name == "tool_a"),
            "未失效前仍走缓存"
        );
        registry.invalidate();
        let after = registry.list().await;
        assert_eq!(
            after.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["tool_b"]
        );
        assert_eq!(
            source
                .enumerations
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[tokio::test]
    async fn registry_keeps_healthy_source_and_reports_inventory_errors() {
        let mut registry = Registry::new();
        registry.register(DummySource::new("mcp:p/good", &["ok_tool"]));
        registry.register(Arc::new(FailingSource {
            id: "mcp:p/dead".to_string(),
        }));

        let specs = registry.list().await;
        assert!(
            specs.iter().any(|s| s.name == "ok_tool"),
            "单来源失败不得丢弃其它健康 source 的工具: {specs:?}"
        );
        let errors = registry.list_errors();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("mcp:p/dead"), "{}", errors[0]);

        // 缓存命中时失败 note 仍可读出，不被静默清空。
        assert_eq!(registry.list_errors(), errors);
    }

    #[tokio::test]
    async fn registry_call_miss_rebuilds_after_inventory_change() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let source = Arc::new(CountingSource {
            id: "mcp:p/live".to_string(),
            enumerations: std::sync::atomic::AtomicUsize::new(0),
            names: Mutex::new(vec!["first".to_string()]),
        });
        let mut registry = Registry::new();
        registry.register(source.clone());
        assert!(registry.list().await.iter().any(|s| s.name == "first"));

        *source.names.lock().unwrap() = vec!["later".to_string()];
        // 缓存里没有 `later`：call 未命中触发 invalidate + 重建，必须路由成功。
        let output = registry
            .call(
                &ToolCall {
                    id: "t".to_string(),
                    name: "later".to_string(),
                    input: json!({}),
                },
                &ctx,
            )
            .await;
        assert!(
            !output.is_error,
            "缓存外新工具应触发重建而非 unknown: {}",
            output.text
        );
    }

    #[tokio::test]
    async fn registry_call_without_list_yet_resolves_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());

        let mut registry = Registry::new();
        registry.register(Arc::new(BuiltinTools::new(None)));
        let output = registry
            .call(
                &ToolCall {
                    id: "t".to_string(),
                    name: "shell".to_string(),
                    input: json!({"command": "echo routed"}),
                },
                &ctx,
            )
            .await;
        assert!(!output.is_error);
        assert!(output.text.contains("routed"));
    }
}
