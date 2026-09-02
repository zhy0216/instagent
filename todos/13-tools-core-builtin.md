# 13 · 工具：ToolSource/Registry + 5 个内置工具

优先级：P2 · 依赖：00、03

目标：实现工具层框架（`ToolSource`、Registry、命名规则）与 5 个内置工具。只填
`src/tools/mod.rs`、`src/tools/builtin/**`。

验收：`cargo test` 过；命名/64 字符映射、shell 超时与截断、edit 匹配语义有测试。

计划参考：第三版 §2.5；第二版 §2.4。

## N1 · tools/mod.rs：Registry 与命名 {#n1}

- 填实 `ToolSpec / ToolOutput / ToolCtx / ToolSource / ToolCall`（类型 00 已定义）。
- `Registry` 管理多个 `Arc<dyn ToolSource>`：`list()` 汇总，`call(name, input, ctx)` 按名路由。
- 工具命名：内置不加前缀；MCP `<server>__<tool>`；command tools `<plugin>__<tool>`；
  同名冲突再加插件名前缀。
- OpenAI 函数名限制 `[A-Za-z0-9_-]{1,64}`：超长截断 + 6 位哈希，内核保存双向映射表，
  同一会话内稳定不变。

## N2 · BuiltinTools：shell {#n2}

- `shell`(command, timeout_secs?)：`$SHELL -c` 或 `bash -c`，cwd 为会话目录，
  用 `03` 的进程组；超时或取消时 kill 整组；默认超时 300s。
- 输出截断：每流 2000 行 / 50KB，超出给前 50 行 / 10KB 预览并把全文存临时文件、返回路径；
  返回 stdout、stderr、exit code。
- 描述文本从 `~/yyds/goose` `developer/mod.rs:135~150` 精简；截断逻辑参考
  `developer/shell.rs` 的 `render_output` / `save_full_output` 组（搬运注明出处）。

## N3 · BuiltinTools：read / write / edit {#n3}

- `read`(path, line?, limit?)：带行号输出，默认最多 2000 行。
- `write`(path, content)：建父目录，覆盖写。
- `edit`(path, before, after)：`before` 必须唯一精确匹配，否则报错并给出匹配次数和
  相近上下文；`after` 为空即删除。参考 goose `developer/edit.rs:157~286`
  （`string_replace` / `find_similar_context` / `build_file_preview`，搬运注明出处）。

## N4 · BuiltinTools：tree {#n4}

- `tree`(path, depth)：目录树 + 行数，遵守 .gitignore（`ignore` crate）。
- `read` 和 `tree` 标 `read_only = true`。

## N5 · 以 ToolSource 注册 + 测试 {#n5}

- `BuiltinTools: ToolSource`（id = `"builtin"`），经 Registry 注册，与其他来源同构。
- 测试：命名冲突、64 字符截断与映射稳定性；shell 超时 kill / 取消 kill / 截断落盘 /
  非零退出码；edit 唯一匹配 / 多匹配报错 / 删除；read 行号；tree 忽略 .gitignore。
