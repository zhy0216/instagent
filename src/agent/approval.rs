//! 审批（第二版 §2.8）：模式 + 白名单 + Confirm 回调。
//!
//! auto 全放行；approve 白名单放行、其余问 confirm（`read` / `tree` 默认在
//! 白名单）；chat 不给模型工具。AllowAlways 进会话级白名单，并即时写回
//! Config（`01` 的 save，`with_config` 注入时）。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::config::Config;
use crate::config::Mode;
use crate::tools::ToolCall;

/// 内置默认白名单（第二版 §2.8）。
pub const DEFAULT_ALWAYS_ALLOW: [&str; 2] = ["read", "tree"];

/// 审批结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// 放行并持久化进 always_allow。
    AllowAlways,
    /// 拒绝 + 原因（以 is_error ToolResult 回给模型，模型可换办法）。
    Deny(String),
}

/// 审批回调（v1 只有 CLI：问一句；将来远程客户端换实现，loop 不改）。
#[async_trait]
pub trait Confirm: Send + Sync {
    async fn confirm(&self, call: &ToolCall) -> Decision;
}

#[derive(Default)]
pub struct Approval {
    pub mode: Mode,
    /// 来自配置 `always_allow`（已并入 [`DEFAULT_ALWAYS_ALLOW`]）。
    pub always_allow: HashSet<String>,
    pub confirm: Option<Arc<dyn Confirm>>,
    /// 会话内 AllowAlways 新增的名字（即时生效）。
    granted: Mutex<HashSet<String>>,
    /// Some 时 AllowAlways 即时写回该配置并 save（第二版 §2.8）。
    config: Mutex<Option<Config>>,
}

impl Approval {
    pub fn new(mode: Mode, always_allow: Vec<String>, confirm: Option<Arc<dyn Confirm>>) -> Self {
        let mut whitelist: HashSet<String> =
            DEFAULT_ALWAYS_ALLOW.into_iter().map(String::from).collect();
        whitelist.extend(always_allow);
        Self {
            mode,
            always_allow: whitelist,
            confirm,
            granted: Mutex::new(HashSet::new()),
            config: Mutex::new(None),
        }
    }

    /// 挂上写回目标（配置文件）；`assemble` 装配时调用。
    pub fn with_config(mut self, config: Config) -> Self {
        *self.config.get_mut().expect("approval config lock") = Some(config);
        self
    }

    /// 判定单个调用：白名单命中直接放行；AllowAlways 在返回前已写回。
    pub async fn decide(&self, call: &ToolCall) -> Decision {
        match self.mode {
            Mode::Auto => Decision::Allow,
            // chat 模式不给模型 tools，正常不会走到；防御性拒绝。
            Mode::Chat => Decision::Deny("chat mode does not allow tools".to_string()),
            Mode::Approve => {
                if self.is_allowed(&call.name) {
                    return Decision::Allow;
                }
                let decision = match &self.confirm {
                    Some(confirm) => confirm.confirm(call).await,
                    None => Decision::Deny("no confirmation channel available".to_string()),
                };
                if decision == Decision::AllowAlways {
                    self.grant_always(&call.name);
                }
                decision
            }
        }
    }

    /// 白名单命中（配置 + 默认 + 会话内 AllowAlways）。
    fn is_allowed(&self, name: &str) -> bool {
        if self.always_allow.contains(name) {
            return true;
        }
        self.granted
            .lock()
            .expect("approval granted lock")
            .contains(name)
    }

    /// AllowAlways 写回：会话级即时生效；配置在案则落盘。
    pub fn grant_always(&self, name: &str) {
        self.granted
            .lock()
            .expect("approval granted lock")
            .insert(name.to_string());
        let mut guard = self.config.lock().expect("approval config lock");
        if let Some(config) = guard.as_mut() {
            if !config.always_allow.iter().any(|allowed| allowed == name) {
                config.always_allow.push(name.to_string());
                if let Err(e) = config.save() {
                    tracing::warn!("persist always_allow {name} failed: {e}");
                }
            }
        }
    }

    /// 该模式是否给模型带 tools（chat = false）。
    pub fn grants_tools(&self) -> bool {
        self.mode != Mode::Chat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    struct FixedConfirm {
        decision: Decision,
        calls: AtomicUsize,
    }

    impl FixedConfirm {
        fn new(decision: Decision) -> Arc<Self> {
            Arc::new(Self {
                decision,
                calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl Confirm for FixedConfirm {
        async fn confirm(&self, _call: &ToolCall) -> Decision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.decision.clone()
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            input: json!({}),
        }
    }

    #[tokio::test]
    async fn auto_mode_allows_everything() {
        let approval = Approval::new(Mode::Auto, vec![], None);
        assert_eq!(approval.decide(&call("shell")).await, Decision::Allow);
        assert!(approval.grants_tools());
    }

    #[tokio::test]
    async fn chat_mode_denies_and_grants_no_tools() {
        let approval = Approval::new(Mode::Chat, vec![], None);
        assert!(!approval.grants_tools());
        let Decision::Deny(reason) = approval.decide(&call("shell")).await else {
            panic!("expected deny in chat mode");
        };
        assert!(reason.contains("chat mode"), "{reason}");
    }

    #[tokio::test]
    async fn approve_default_whitelist_needs_no_confirm() {
        let confirm = FixedConfirm::new(Decision::Allow);
        let approval = Approval::new(Mode::Approve, vec![], Some(confirm.clone()));
        assert_eq!(approval.decide(&call("read")).await, Decision::Allow);
        assert_eq!(approval.decide(&call("tree")).await, Decision::Allow);
        assert_eq!(confirm.calls.load(Ordering::SeqCst), 0);
        // 非白名单才问。
        assert_eq!(approval.decide(&call("shell")).await, Decision::Allow);
        assert_eq!(confirm.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_confirm_channel_denies() {
        let approval = Approval::new(Mode::Approve, vec![], None);
        let Decision::Deny(reason) = approval.decide(&call("shell")).await else {
            panic!("expected deny");
        };
        assert!(reason.contains("no confirmation channel"), "{reason}");
    }

    // guard 跨 await 是有意的：只为串行化 INSTAGENT_CONFIG_DIR 环境变量，
    // 防并行测试互踩（与 config.rs 测试同一约定）。
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn allow_always_grants_session_and_persists_config() {
        let _guard = crate::config::lock_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("INSTAGENT_CONFIG_DIR", dir.path());

        let confirm = FixedConfirm::new(Decision::AllowAlways);
        let approval = Approval::new(Mode::Approve, vec![], Some(confirm.clone()))
            .with_config(Config::default());

        assert_eq!(approval.decide(&call("shell")).await, Decision::AllowAlways);
        // 会话内即时生效：第二次不再问。
        assert_eq!(approval.decide(&call("shell")).await, Decision::Allow);
        assert_eq!(confirm.calls.load(Ordering::SeqCst), 1);

        let loaded = Config::load(dir.path()).unwrap();
        assert!(
            loaded.always_allow.contains(&"shell".to_string()),
            "{:?}",
            loaded.always_allow
        );
    }
}
