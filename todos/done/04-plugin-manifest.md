# 04 · 插件：manifest 校验

优先级：P1 · 依赖：00

目标：实现 `plugin.json`（Agent Plugins v1.0.0）的解析与校验。只填 `src/plugin/manifest.rs`
（`src/plugin/mod.rs` 里与本文件相关的公共类型也归本任务）。

验收：`cargo test` 过；合法/名字非法/$schema 版本不符/未知字段/缺组件目录五种情况有测试。

计划参考：第三版 §2.1、§2.2、§6。

## E1 · manifest.rs {#e1}

- 顶层只允许 `$schema name version description author homepage repository license keywords extensions`
  十个字段；未知字段报告（warning 收集器）但不致命。
- `$schema` 必须等于 `https://agent-plugins.org/schemas/1.0.0/plugin.schema.json`；
  **不联网取 schema**，用该字符串选择本地校验规则；版本不符时报错。
- `name`：1~64 字符，小写字母数字和 `-` `.`，首尾必须是字母数字，不能含 `--` `..`。
- 组件目录（`skills/`、`mcp.json`、`dev.instagent/`）不存在不算错。
- 提供 `namespaced_component_name(plugin, component)` → `<plugin>:<component>`。

## E2 · 测试 {#e2}

- fixtures：合法 manifest、name 非法、`$schema` 版本不符、未知字段、组件目录缺失。
