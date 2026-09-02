//! bundled 插件：`include_dir` 内嵌仓库 `bundled/` 目录（第三版 §1）。
//!
//! TODO(07)：建 `bundled/` 目录（`plugin.json` + `dev.instagent/providers/`，
//! provider JSON 由 `10` 生成转换脚本产出），并在这里用 `include_dir!` 内嵌、
//! 物化后作为 [`PluginSource::Bundled`] 注入 `05` 的发现流程。
//! 注意：`include_dir!` 要求目录编译期存在，故 00 阶段不放宏，避免空壳编不过。
//! bundled provider 定义不使用 `${PLUGIN_ROOT}`（无文件系统 root）。

use crate::plugin::Plugin;

/// 伪插件名：`plugin list` 与工具前缀规则统一用。
pub const BUNDLED_PLUGIN_NAME: &str = "bundled";

/// 内嵌插件物化后加载（与外部插件同一条路径）。
/// bundled 恒为最低优先级，同名一律被用户插件覆盖（第三版 §1）。TODO(07)
pub fn load_bundled() -> crate::Result<Plugin> {
    todo!("TODO(07)")
}
