//! instagent：插件为核心的最小 agent（Rust 从零实现）。
//!
//! 模块树由 todos/00 锁定：本文件之后的任务只填充各自负责模块的实现
//! （`// TODO(<编号>)` 标注归属），不改模块声明、不改公共类型布局。
//!
//! - 设计主依据：`docs/goose-plugin-core-plan.md`（第三版）
//! - 设计补充：`docs/goose-from-scratch-plan.md`（第二版）
//! - 顶层错误一律 `anyhow::Result`（见 [`error`]）。

pub mod agent;
pub mod commands;
pub mod config;
pub mod error;
pub mod hooks;
pub mod message;
pub mod plugin;
pub mod provider;
pub mod session;
pub mod settings;
pub mod subprocess;
pub mod tools;

pub use error::ProviderError;
pub use error::Result;
