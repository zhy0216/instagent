difficulty: medium

## T1 · 固化跨模块策略决策

- 要做什么：依据方案约束新增一份 ADR（建议 `docs/adr/0003-repo-boundaries-and-runtime-policies.md`），明确并可执行地决定：API key 的来源优先级与是否删除死字段；插件/command/MCP/builtin shell 的环境变量 baseline 与 allowlist；hook fail-open/fail-closed 默认值；CLI stdout/stderr 机器契约；`enabledPlugins: []` 的 tri-state 语义；外部 sandbox 与应用层路径 containment 的责任边界。ADR 必须引用 ADR 0001/0002，并列出兼容性与迁移影响。
- 预计修改文件：`docs/adr/0003-repo-boundaries-and-runtime-policies.md`。
- 验收条件：ADR 为每个决策写出默认值、例外、错误行为和后续实现任务；不恢复 Anthropic 或 approval/trust UI；Markdown 内容清晰且 `git diff --check` 通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 与 `git diff --check`，并人工核对 ADR 0001/0002 的引用与六项决策均有落点。
- 前置依赖：无。
