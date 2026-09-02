//! 审批（第二版 §2.8）：模式 + 白名单 + Confirm 回调。
//!
//! TODO(16)：填 `decide`。auto 全放行；approve 白名单放行、其余问 confirm
//! （read / tree 默认在白名单）；chat 不给模型工具。AllowAlways 即时写回
//! Config（`01` 的 save）。

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::Mode;
use crate::tools::ToolCall;

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
    /// 来自配置 `always_allow` + 会话内 AllowAlways。
    pub always_allow: HashSet<String>,
    pub confirm: Option<Arc<dyn Confirm>>,
}

impl Approval {
    pub fn new(mode: Mode, always_allow: Vec<String>, confirm: Option<Arc<dyn Confirm>>) -> Self {
        Self {
            mode,
            always_allow: always_allow.into_iter().collect(),
            confirm,
        }
    }

    /// 判定单个调用。TODO(16)
    pub async fn decide(&self, _call: &ToolCall) -> Decision {
        todo!("TODO(16)")
    }

    /// 该模式是否给模型带 tools（chat = false）。TODO(16)
    pub fn grants_tools(&self) -> bool {
        todo!("TODO(16)")
    }
}
