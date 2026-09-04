difficulty: medium

## T1 · plugin manifest schema 与安装前校验

- 要做什么：在 `src/plugin/manifest.rs` 补 version/semver、description/author/license/homepage/repository、keywords、extensions 的字段形状与跨字段校验；明确未知字段兼容策略，错误带插件名和 `plugin.json` 路径；读取 manifest 使用有界输入和截断摘要。
- 预计修改文件：`src/plugin/manifest.rs`、`tests/fixtures/manifest/` 下新增或调整 fixture。
- 验收条件：坏 version、坏字段类型、非法跨字段组合在安装/发现阶段失败；未知字段行为有测试；合法旧 fixture 继续通过；错误不会输出无界原始 JSON。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、manifest fixture 测试和 `cargo test`。
- 前置依赖：`01-policy-decisions.md`、`08-config-provider-validation.md`。
