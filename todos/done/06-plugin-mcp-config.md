# 06 · 插件：mcp.json 解析

优先级：P1 · 依赖：00、04

目标：实现插件 `mcp.json` 的解析与变量展开，含 `.mcp.json` 草案兼容。只填 `src/plugin/mcp_config.rs`。

验收：`cargo test` 过；变量展开、sse 跳过、`.mcp.json` 回退、非法 command 有测试。

计划参考：第三版 §2.3。

## G1 · mcp.json 解析 {#g1}

- `mcpServers` 每项 `type` 三选一：`stdio` / `streamable-http` / `sse`。
  v1 只实现前两个的解析结构，`sse` 标记"不支持"由上层跳过。
- `command` 必须是单个可执行名或 `./` 开头的插件相对路径；不做变量展开。
- 只展开 `${PLUGIN_ROOT}` 和 `${PLUGIN_DATA}`：单次、非递归，只作用于 `args` 元素、
  `env` 值、`cwd`；其他 `${...}` 原样保留；`env` 里禁止定义 `PLUGIN_ROOT` / `PLUGIN_DATA`。
- `headers` 不承载凭据（远程鉴权 v1 不做）。

## G2 · .mcp.json 兼容读取 {#g2}

- 插件根没有 `mcp.json` 时回退读 `.mcp.json`（goose / Claude Code 草案格式，
  无 `type` 字段，按 `stdio` 处理）。

## G3 · 测试 {#g3}

- fixtures：`${PLUGIN_ROOT}` 展开、未知变量保留、sse 标记不支持、
  `env` 定义保留名报错、`.mcp.json` 回退。
