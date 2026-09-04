difficulty: medium

## T1 · 插件来源与目录枚举诊断

- 要做什么：在 `src/plugin/discovery.rs` / `src/plugin/mod.rs` 为显式 CLI path 增加专用 source kind/display name，错误同时显示来源和解析后的绝对路径；把 `read_dir` 的 `flatten`/静默丢弃改为统一枚举逻辑，区分“不匹配”与权限、坏 symlink、IO 失败并汇总 skipped diagnostics。对 `src/commands.rs` 的 slash-command 目录枚举和 Markdown 读取也使用有界读取与可见的 skipped 诊断，避免坏文件/超长正文静默消失。
- 要做什么：保持 Extra/CLI > Project > User > Bundled 的覆盖顺序与 settings tri-state 语义，补权限错误、坏 symlink、重复来源和路径解析测试。
- 预计修改文件：`src/plugin/discovery.rs`、`src/plugin/mod.rs`、`src/commands.rs`。
- 验收条件：目录读取失败不会无声消失；CLI path 错误可区分于其它来源；覆盖与启用规则不回归；诊断包含 path/source 且可在装配 notes 中使用。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、plugin discovery/commands 测试和 `cargo test`。
- 前置依赖：`03-settings-atomic-merge.md`。
