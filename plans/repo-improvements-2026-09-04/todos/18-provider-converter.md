difficulty: medium

## T1 · converter source 注入与 schema fixture

- 要做什么：重构 `scripts/convert_providers.py`，支持显式 source 参数和 fixture mode，不再硬编码 `~/yyds/goose`；根据 `08` 的 ProviderDef/ModelDef 契约处理或剔除运行时不用的字段，并在生成后做 schema/round-trip 校验。
- 预计修改文件：`scripts/convert_providers.py`、`tests/fixtures/` 下新增最小 provider converter fixture、必要的脚本测试文件。
- 验收条件：在无 `~/yyds/goose` 的 CI 环境可用 fixture 生成 bundled JSON；生成结果可被运行时反序列化并保留约定字段；坏 source/schema 给出非零、可诊断错误；脚本测试可重复运行。
- 验证方式：运行 converter 的 fixture/unit 测试，随后运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`。
- 前置依赖：`08-config-provider-validation.md`。
