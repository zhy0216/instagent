//! provider 层（第二版 §2.3；第三版 §2.4：provider 定义来自插件，内核只有引擎）。
//!
//! 类型布局由 00 锁定；`context_limit_for` 由 08 实现，
//! engine 实现在 [`openai`] / [`proxy`]，装配在 [`registry`]。

pub mod http;
pub mod openai;
pub mod proxy;
pub mod registry;
mod shared;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Deserialize;
use serde::Serialize;

use crate::error::ProviderError;
use crate::message::Message;
use crate::message::Usage;
use crate::tools::ToolSpec;

pub use registry::ProviderRegistry;

/// 兜底上下文上限（第二版 §2.3）。
pub const DEFAULT_CONTEXT_LIMIT: u32 = 128 * 1024;
/// goose DEFAULT_PROVIDER_TIMEOUT_SECS。
pub const DEFAULT_PROVIDER_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

/// provider 流式事件；`stream_assistant`（`16`）把它折叠成一条 assistant Message。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    /// JSON 片段，累积到 [`StreamEvent::ToolUseEnd`] 再 parse。
    ToolUseDelta(String),
    ToolUseEnd,
    Done {
        usage: Usage,
        stop_reason: StopReason,
    },
}

/// 请求参数全借用，loop 侧零拷贝。
pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn stream(
        &self,
        req: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}

/// 模型名前缀小表 + 兜底 128k：claude 200k、gpt-4o 128k、gpt-4.1 1M、
/// o 系列 200k、deepseek 128k、llama 128k（第二版 §2.3）。
/// 名字带 `provider/` 命名空间前缀时按最后一段匹配。
pub fn context_limit_for(model: &str) -> u32 {
    const K: u32 = 1024;
    let m = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    if m.starts_with("claude") {
        200 * K
    } else if m.starts_with("gpt-4.1") {
        1024 * K
    } else if m.starts_with("gpt-4o") {
        128 * K
    } else if is_o_series(&m) {
        200 * K
    } else {
        // deepseek / llama 未列出的其余模型一律兜底 128k。
        DEFAULT_CONTEXT_LIMIT
    }
}

/// o 系列 = 首字符 `o` + 数字（o1 / o3 / o4-mini …）。
fn is_o_series(m: &str) -> bool {
    let mut chars = m.chars();
    matches!(chars.next(), Some('o')) && chars.next().is_some_and(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_limit_prefix_table() {
        const K: u32 = 1024;
        assert_eq!(context_limit_for("claude-sonnet-4-5"), 200 * K);
        assert_eq!(context_limit_for("gpt-4o-mini"), 128 * K);
        assert_eq!(context_limit_for("gpt-4.1-nano"), 1024 * K);
        assert_eq!(context_limit_for("o3"), 200 * K);
        assert_eq!(context_limit_for("o1-preview"), 200 * K);
        assert_eq!(context_limit_for("deepseek-chat"), DEFAULT_CONTEXT_LIMIT);
        assert_eq!(context_limit_for("llama3.1-70b"), DEFAULT_CONTEXT_LIMIT);
    }

    #[test]
    fn context_limit_fallback_and_normalization() {
        assert_eq!(context_limit_for("mistral-large"), DEFAULT_CONTEXT_LIMIT);
        assert_eq!(context_limit_for(""), DEFAULT_CONTEXT_LIMIT);
        assert_eq!(context_limit_for("openai/gpt-4o"), 128 * 1024);
        assert_eq!(context_limit_for("GPT-4.1"), 1024 * 1024);
        // "ollama" 之类 o 开头但非 o+数字，不误判为 o 系列。
        assert_eq!(context_limit_for("ollama"), DEFAULT_CONTEXT_LIMIT);
    }

    fn valid_def() -> ProviderDef {
        ProviderDef {
            name: "acme".to_string(),
            engine: EngineKind::Openai,
            display_name: Some("Acme".to_string()),
            description: Some("Acme inference".to_string()),
            api_key_env: Some("ACME_API_KEY".to_string()),
            base_url: Some("https://acme.test/v1".to_string()),
            headers: BTreeMap::new(),
            timeout_seconds: None,
            models: vec![ModelDef {
                name: "acme-1".to_string(),
                context_limit: Some(8192),
                max_tokens: Some(4096),
            }],
            proxy: None,
        }
    }

    #[test]
    fn validate_accepts_new_shape_and_old_shape_absent_fields() {
        // 新形状（display_name / description / model max_tokens 齐全）。
        valid_def().validate().unwrap();
        // 旧形状：展示字段与 model max_tokens 缺省（转换脚本旧产物口径）。
        serde_json::from_str::<ProviderDef>(
            r#"{"name":"old","engine":"openai","base_url":"https://old.test/v1"}"#,
        )
        .unwrap()
        .validate()
        .unwrap();
    }

    #[test]
    fn validate_rejects_bad_fields_with_provider_and_field_names() {
        let cases: Vec<(ProviderDef, &str)> = vec![
            (
                ProviderDef {
                    display_name: Some("  ".into()),
                    ..valid_def()
                },
                "display_name",
            ),
            (
                ProviderDef {
                    description: Some("".into()),
                    ..valid_def()
                },
                "description",
            ),
            (
                ProviderDef {
                    api_key_env: Some(" ".into()),
                    ..valid_def()
                },
                "api_key_env",
            ),
            (
                ProviderDef {
                    timeout_seconds: Some(0),
                    ..valid_def()
                },
                "timeout_seconds",
            ),
            (
                ProviderDef {
                    headers: BTreeMap::from([("".to_string(), "v".to_string())]),
                    ..valid_def()
                },
                "header name",
            ),
            (
                ProviderDef {
                    headers: BTreeMap::from([("x-title".to_string(), " ".to_string())]),
                    ..valid_def()
                },
                "x-title",
            ),
            (
                ProviderDef {
                    base_url: None,
                    ..valid_def()
                },
                "base_url",
            ),
        ];
        for (def, expected) in cases {
            let err = def.validate().unwrap_err().to_string();
            assert!(err.contains("acme"), "{err}");
            assert!(err.contains(expected), "{expected} not in {err}");
        }
    }

    #[test]
    fn validate_rejects_bad_model_fields() {
        let model_case = |model: ModelDef| ProviderDef {
            models: vec![model],
            ..valid_def()
        };
        let cases = vec![
            (
                model_case(ModelDef {
                    name: "".into(),
                    context_limit: None,
                    max_tokens: None,
                }),
                "model name must be non-empty",
            ),
            (
                ProviderDef {
                    models: vec![
                        ModelDef {
                            name: "m1".into(),
                            context_limit: None,
                            max_tokens: None,
                        },
                        ModelDef {
                            name: "m1".into(),
                            context_limit: None,
                            max_tokens: None,
                        },
                    ],
                    ..valid_def()
                },
                "declared twice",
            ),
            (
                model_case(ModelDef {
                    name: "m1".into(),
                    context_limit: Some(0),
                    max_tokens: None,
                }),
                "context_limit",
            ),
            (
                model_case(ModelDef {
                    name: "m1".into(),
                    context_limit: None,
                    max_tokens: Some(0),
                }),
                "max_tokens",
            ),
        ];
        for (def, expected) in cases {
            let err = def.validate().unwrap_err().to_string();
            assert!(err.contains("acme"), "{err}");
            assert!(err.contains(expected), "{expected} not in {err}");
        }
    }

    #[test]
    fn validate_engine_section_requirements() {
        let err = ProviderDef {
            engine: EngineKind::Proxy,
            proxy: None,
            ..valid_def()
        }
        .validate()
        .unwrap_err()
        .to_string();
        assert!(err.contains("proxy section"), "{err}");
        let proxy_ok = ProviderDef {
            engine: EngineKind::Proxy,
            base_url: None,
            proxy: Some(ProxyDef {
                command: "./proxy".into(),
                args: vec![],
                env: BTreeMap::new(),
                ready: None,
                timeout_secs: None,
            }),
            ..valid_def()
        };
        proxy_ok.validate().unwrap();
    }
}

/// `dev.instagent/providers/*.json` 的形状（第三版 §2.4；沿用 goose
/// DeclarativeProviderConfig，去 setup 向导字段，加 `proxy`）。
///
/// `display_name` / `description` 是展示字段（转换脚本产物携带，加载期校验，
/// 运行时不参与请求）；字段级校验见 [`ProviderDef::validate`]（todo 08 / S9）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDef {
    pub name: String,
    pub engine: EngineKind,
    /// 展示名（缺省时用 `name`）。
    #[serde(default)]
    pub display_name: Option<String>,
    /// 一句话描述（展示用，可缺省）。
    #[serde(default)]
    pub description: Option<String>,
    /// 密钥唯一来源（ADR 0003 D1）：指向环境变量名；密钥本身永不入文件/日志。
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// openai 引擎写到 `/v1`，请求时拼 `/chat/completions`（转换脚本处理，见 `10`）。
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub models: Vec<ModelDef>,
    /// engine = proxy 时必填，拉起逻辑在 `11`。
    #[serde(default)]
    pub proxy: Option<ProxyDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Openai,
    Proxy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDef {
    pub name: String,
    #[serde(default)]
    pub context_limit: Option<u32>,
    /// 模型输出 token 上限：请求的 `max_tokens` 超过它时引擎收敛到该值
    /// （`openai` 引擎 `stream` 处；todo 08 / S9）。
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// `engine: "proxy"` 的拉起配置（第三版 §2.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyDef {
    /// 单个可执行名或 `./` 开头的插件相对路径。
    pub command: String,
    /// `${PORT}` 在拉起时替换（`11`）。
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 就绪探针路径，默认 [`ProxyDef::DEFAULT_READY`]。
    #[serde(default)]
    pub ready: Option<String>,
    /// 就绪超时秒数，默认 [`ProxyDef::DEFAULT_TIMEOUT_SECS`]。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl ProxyDef {
    pub const DEFAULT_READY: &'static str = "/v1/models";
    pub const DEFAULT_TIMEOUT_SECS: u64 = 20;
}

impl ProviderDef {
    /// 装载期字段级校验（todo 08 / S7 / S9）：每个错误都指出 provider 名与
    /// 字段，并给出建议值；由 [`crate::provider::registry`] 在解析后立即调用。
    pub fn validate(&self) -> crate::Result<()> {
        let p = &self.name;
        anyhow::ensure!(
            !p.is_empty() && !p.contains('/'),
            "provider name `{p}` is empty or contains `/`"
        );
        if let Some(display_name) = &self.display_name {
            anyhow::ensure!(
                !display_name.trim().is_empty(),
                "provider `{p}`: field `display_name` must be non-empty when present \
                 (suggested: remove the field)"
            );
        }
        if let Some(description) = &self.description {
            anyhow::ensure!(
                !description.trim().is_empty(),
                "provider `{p}`: field `description` must be non-empty when present \
                 (suggested: remove the field)"
            );
        }
        if let Some(var) = &self.api_key_env {
            anyhow::ensure!(
                !var.trim().is_empty(),
                "provider `{p}`: field `api_key_env` must name a non-empty env var \
                 (suggested: remove the field)"
            );
        }
        if let Some(secs) = self.timeout_seconds {
            anyhow::ensure!(
                secs >= 1,
                "provider `{p}`: field `timeout_seconds` must be >= 1 \
                 (got {secs}; suggested: remove the field to default to {}s)",
                DEFAULT_PROVIDER_TIMEOUT.as_secs()
            );
        }
        for (key, value) in &self.headers {
            anyhow::ensure!(
                !key.trim().is_empty(),
                "provider `{p}`: header name must be non-empty"
            );
            anyhow::ensure!(
                !value.trim().is_empty(),
                "provider `{p}`: header `{key}` must have a non-empty value"
            );
        }
        match self.engine {
            EngineKind::Openai => {
                anyhow::ensure!(
                    self.base_url
                        .as_deref()
                        .is_some_and(|u| !u.trim().is_empty()),
                    "provider `{p}` (engine openai) is missing base_url"
                );
            }
            EngineKind::Proxy => {
                anyhow::ensure!(
                    self.proxy.is_some(),
                    "provider `{p}` (engine proxy) is missing the proxy section"
                );
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for model in &self.models {
            anyhow::ensure!(
                !model.name.trim().is_empty(),
                "provider `{p}`: model name must be non-empty"
            );
            anyhow::ensure!(
                seen.insert(model.name.clone()),
                "provider `{p}`: model `{}` is declared twice",
                model.name
            );
            if let Some(limit) = model.context_limit {
                anyhow::ensure!(
                    limit >= 1,
                    "provider `{p}`: model `{}`: field `context_limit` must be >= 1 \
                     (got {limit}; suggested: remove the field to fall back to the \
                     model-name prefix table)",
                    model.name
                );
            }
            if let Some(max_tokens) = model.max_tokens {
                anyhow::ensure!(
                    max_tokens >= 1,
                    "provider `{p}`: model `{}`: field `max_tokens` must be >= 1 \
                     (got {max_tokens}; suggested: remove the field)",
                    model.name
                );
            }
        }
        Ok(())
    }
}
