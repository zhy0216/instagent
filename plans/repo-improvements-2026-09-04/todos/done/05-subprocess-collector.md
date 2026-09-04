difficulty: medium

## T1 · 建立 bounded subprocess collector

- 要做什么：在 `src/subprocess.rs` 将 stdout/stderr 收集改为有硬上限的 collector（可选受控 spill/ring，但必须保留截断摘要和超限状态），超限时能通知调用方终止进程组；使用增量 UTF-8 解码，避免 chunk 边界多字节字符产生 replacement；把 `wait_and_drain` 的未配置 pipe 从 panic 改为带上下文的错误。
- 预计修改文件：`src/subprocess.rs`。
- 验收条件：可控 fake child 证明超限会终止并返回摘要；stdout/stderr 合并规则可由调用方辨认；人为切断 UTF-8 边界仍还原正确文本；未 piped 的 child 返回错误而非 panic；模块单测与全仓校验通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test subprocess` 和 `cargo test`。
- 前置依赖：`01-policy-decisions.md`。
