difficulty: medium

审批确认输入解析是审批语义的关键路径（`y/yes/a/always/n/no` 决定放行与否），但 `CliConfirm` 的解析逻辑目前零测试且内联在 `render.rs`。抽成纯函数补单测。

## T1 · 抽出决策解析纯函数

- `src/cli/render.rs:122-135` `CliConfirm` 内联解析 `stdin` 行输入并映射成审批决策。把"输入字符串 → 决策"抽成纯函数（如 `fn parse_confirm(line: &str) -> ConfirmDecision`），`CliConfirm` 调用它。
- 明确大小写与空白语义：`y`/`yes` → Allow once；`a`/`always` → Always allow；`n`/`no`/空/其他 → Deny。保持与现有行为一致（先读现有实现再定，若现状大小写敏感则保持一致，不擅自改变语义）。
- 预计修改文件：`src/cli/render.rs`。
- 验收：函数可在无 stdin 情况下直接测试；`cargo test` 通过。
- 前置依赖：无。

## T2 · 补单测

- 为 T1 的纯函数补单测：覆盖 `y`/`Y`/`yes`/`a`/`always`/`n`/`no`/空行/前后空白/未知输入各分支，断言映射结果。
- 预计修改文件：`src/cli/render.rs`（`#[cfg(test)]` 段）。
- 验收：新增测试全绿；覆盖上面列出全部分支。
- 前置依赖：依赖 07 文件内 T1。

## 本文件整体验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```
