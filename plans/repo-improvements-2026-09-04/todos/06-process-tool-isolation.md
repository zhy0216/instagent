difficulty: hard

## T1 · shell、command、hooks 的进程策略收口

- 要做什么：让 `src/tools/builtin/shell.rs`、`src/tools/command.rs`、`src/hooks.rs` 使用 `05` 的 bounded collector，超限、超时、取消时终止整个进程组并返回可操作摘要；shell spill 文件加入 session/随机后缀、消费/会话结束/TTL 清理和失败诊断。command tool JSON 定义读取也要有大小上限和截断后的来源诊断。
- 要做什么：禁止把 `${PLUGIN_ROOT}` 直接拼进 `sh -c`，改用 argv 与环境变量传递（或可靠 quoting）；按 ADR 0003 建立插件子进程 baseline + allowlist，默认不泄露 provider credentials/session secrets，并为含空格、引号、换行的路径和 env isolation 加测试。实现 hook 的显式 on-failure 策略，不引入 approval UI。
- 预计修改文件：`src/tools/builtin/shell.rs`、`src/tools/command.rs`、`src/hooks.rs`。
- 验收条件：大输出不会无界保存在内存；路径特殊字符按字面传递；插件脚本读不到未声明的 key；hook fail-open/fail-closed 与 ADR、错误/超时测试一致；子进程退出后无长期敏感 spill 文件；主校验命令通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、shell/command/hooks 测试和 `cargo test`，并检查子孙进程回收。
- 前置依赖：`01-policy-decisions.md`、`05-subprocess-collector.md`。
