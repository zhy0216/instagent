difficulty: medium

## T1 · settings 与本地安装配置的原子私有写入

- 要做什么：在 `src/settings.rs` 提供可复用的原子私有写入流程（同目录临时文件、flush/sync、rename、失败清理，Unix mode 0600，必要时目录 sync），让 user/project/local settings 的 save/load 路径使用它；区分字段缺失、显式空数组、非空数组的 tri-state 合并，保持 enabled/disabled 的高层 claim 规则并覆盖三层组合。
- 要做什么：让 `src/plugin/install.rs` 的用户 settings/install metadata 写入复用同一安全策略；在仓库 `.gitignore` 增加精确的 project-local settings pattern，不忽略可公开提交的示例模板。
- 预计修改文件：`src/settings.rs`、`src/plugin/install.rs`、`.gitignore`。
- 验收条件：崩溃/rename 失败时只能留下旧完整文件或新完整文件；settings.local.json 不会被默认纳入 git；缺失与 `[]` 的行为与 ADR 0003 及测试一致；安装启停和已有 settings 测试全部通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、settings/plugin install 相关测试和 `cargo test`，并用 `git check-ignore` 验证本地配置规则。
- 前置依赖：`01-policy-decisions.md`。
