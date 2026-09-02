# 14 · 工具：MCP 来源（McpSource）

优先级：P2 · 依赖：00、03、13、06

目标：每个插件的每个 MCP server 一个 `McpSource`。只填 `src/tools/mcp.rs`。

验收：`cargo test` 过；前缀、readOnlyHint、超时、server 被 kill 后的可读错误有测试。

计划参考：第三版 §2.5（McpSource 行）；第二版 §2.4（MCP 适配）。

## O1 · McpSource {#o1}

- 用 `05` PluginSet + `06` mcp_config 的每个 server 建一个实例；id = `mcp:<plugin>/<server>`。
- rmcp：stdio 用 `03` subprocess（进程组 + kill_on_drop，stderr 接日志）；
  streamable-http 用 `StreamableHttpClientTransport::from_uri`；sse server 跳过并给
  "不支持"信息。
- `initialize` 返回的 `instructions` 存下，暴露接口供系统提示拼装（`16`/`18` 接线）。

## O2 · 工具映射 {#o2}

- `list_tools` → ToolSpec：名字 `<server>__<tool>`（冲突加插件名前缀，用 `13` 的命名设施），
  `annotations.readOnlyHint` → `read_only`。
- `call_tool`：content 里的 text 块拼接，`is_error` 直接映射；单次调用超时 300s。
- 通知（progress / logging）v1 只写日志。

## O3 · 测试 {#o3}

- 写一个最小 stdio MCP server（用 rmcp 的 server 侧做测试二进制或脚本），覆盖：
  list_tools 带前缀；call 正常返回；server 被 kill 后调用报错可读且不挂起。
  （不依赖 npx / 网络的本地 fixture。）
