difficulty: medium

## T1 · 安装/更新的异步进程与可恢复替换

- 要做什么：在 `src/plugin/install.rs` 将 git clone/rev-parse 等 std `output()` 改为统一 Tokio subprocess wrapper，配置进程组、`kill_on_drop(true)`、超时、取消和 bounded output；错误只显示截断且不回显 secrets。完善 replace_dir 的失败回滚、`.replaced-*` 孤儿备份清理、staging 清理和 interrupted-install 流程；明确 copy_tree 的 symlink 契约并测试。
- 预计修改文件：`src/plugin/install.rs`。
- 验收条件：挂起/超时/取消的 git 子进程和子孙进程均被回收；clone 失败不会留下 staging 或不可恢复目标；替换失败可恢复旧插件；孤儿备份有清理策略；现有 local/git install/update 测试及新增 fake git 测试通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、plugin install 测试和 `cargo test`；对超时 fake git 检查进程组无残留。
- 前置依赖：`03-settings-atomic-merge.md`、`05-subprocess-collector.md`。
