# 10 · Provider：registry 与 bundled providers

优先级：P2 · 依赖：00、08、09、07

目标：把"插件里的 provider JSON"装配成 engine 实例，并生成 bundled 的 6 个
provider 定义。只填 `src/provider/registry.rs`、`bundled/dev.instagent/providers/*.json`、
`scripts/convert_providers.py`。

验收：`cargo test` 过；重名报错、`plugin/name` 消歧、bundled 覆盖、变量展开、
context_limit 顺序有测试。

计划参考：第三版 §2.4、§1。

## K1 · provider JSON 与变量展开 {#k1}

- `DeclarativeProviderConfig`：`name / engine / display_name / api_key_env / base_url /
  headers / timeout_seconds / models[{name, context_limit, max_tokens}] / proxy`
  （proxy 结构先解析，engine 在 `11` 实现）。
- 变量规则：`${env:NAME}`、`${PLUGIN_ROOT}`、`${PLUGIN_DATA}`；`${PORT}` 留到 `11` 拉起时展开。
- 未定义的环境变量 → 可读错误（指出 provider 名和变量名）。

## K2 · registry.rs {#k2}

- 扫描所有启用插件（`05` PluginSet，含 `07` bundled）的 `dev.instagent/providers/*.json`。
- 按名字查找；重名时报错并要求写成 `plugin/name`；用户插件覆盖 bundled。
- engine 分派：`openai` → `09`；`proxy` / `anthropic` 分支先占位
  （`bail!` 未实现，分别由 `11` / `12` 填，勿删占位分支）。
- context_limit 顺序：配置覆盖 → provider models 表 → `08` 前缀小表 → 128k。

## K3 · bundled 6 个 provider {#k3}

- 写 `scripts/convert_providers.py`（约 20 行）：读
  `~/yyds/goose/crates/goose-providers/src/declarative/definitions/*.json`（45 个），
  转换：`base_url` 去掉 `/chat/completions` 后缀、去掉 `setup` 段、
  `engine: anthropic` 的标注需要 proxy 或原生 engine。
- 产出并提交 6 个 JSON 到 `bundled/dev.instagent/providers/`：
  `openai`、`ollama`(http://localhost:11434/v1)、`groq`、`deepseek`、`openrouter`、
  `anthropic-compat`（openai engine 打 `https://api.anthropic.com/v1`，description
  写明官方限制：无 prompt caching、strict 被忽略、thinking 不返回等）。

## K4 · 测试 {#k4}

- 重名冲突、`plugin/name` 消歧、用户插件覆盖 bundled、变量展开、context_limit 四级顺序。
