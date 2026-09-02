# 01 · 配置：config.rs + settings.rs

优先级：P1 · 依赖：00

目标：实现配置与三层 settings 读取。只填 `src/config.rs`、`src/settings.rs`。

验收：`cargo test` 过；配置读写、三层 settings 优先级、环境变量覆盖均有测试。

计划参考：第二版 §2.10；第三版 §2.10（启用/信任字段）。

## B1 · config.rs {#b1}

- 读 `~/.config/instagent/config.yaml`（第三版无 `mcp:` 段）：`provider`（名字）、`model`、
  `max_tokens`、`mode`(auto/approve/chat)、`max_turns`、`context_limit`、
  `compaction_threshold`、`shell`、`always_allow`、`plugins`（额外插件搜索路径）、
  `api_key_env` / `api_key`。
- 环境变量覆盖：`INSTAGENT_PROVIDER`、`INSTAGENT_MODEL`、`INSTAGENT_MODE`。
- 不接系统 keyring。

## B2 · settings.rs {#b2}

- 三层 settings 文件读取与合并：`~/.config/instagent/settings.json` →
  `<project>/.config/instagent/settings.json` → 同目录 `settings.local.json`。
- 字段：`enabledPlugins` / `disabledPlugins` / `trustedPlugins`。
- 优先级 local > project > user；本任务只提供数据结构 + 文件读取 + 合并，
  启用判定逻辑在 `05`。

## B3 · 测试 {#b3}

- 配置读写 round-trip、环境变量覆盖、三层合并优先级、缺文件回退默认值。
