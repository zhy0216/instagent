difficulty: hard

## T1 · config/provider 装载契约与密钥来源

- 要做什么：依据 ADR 0003 在 `src/config.rs` 实现 API key 字段的最终语义（删除死字段或完整接入安全优先级），保证原始 key 不持久化、不进入日志；对 max_tokens、max_turns、context_limit、timeout、compaction threshold、model、headers、plugins 做字段级校验，错误包含来源文件、字段和建议值，并覆盖 YAML/env/default 组合。
- 要做什么：在 `src/provider/mod.rs`、`src/provider/registry.rs`、`src/provider/shared.rs`、必要时 `src/provider/openai.rs` 对 provider/model schema 做一致建模和校验，处理 display_name/description/model max_tokens 的 drift；`context_limit` 对未知/歧义 provider 返回带 provider/model/source 的明确错误或 warning，不静默假定默认值；限制 provider JSON、HTTP 错误 body 和错误摘要的读取。
- 预计修改文件：`src/config.rs`、`src/provider/mod.rs`、`src/provider/registry.rs`、`src/provider/shared.rs`、`src/provider/http.rs`、`src/provider/openai.rs`（仅在 key 语义需要时）。
- 验收条件：每个非法 numeric/key/provider 字段在装载边界失败并指出字段；每种 key 来源有测试且不泄露；旧/新 provider fixture 的字段行为明确；未知 provider 不再产生误导性 context limit；provider 解析超限/坏 JSON 错误有界；全仓校验通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、config/provider 测试、`cargo rustdoc --lib -- -D warnings` 和 `cargo test`。
- 前置依赖：`01-policy-decisions.md`。
