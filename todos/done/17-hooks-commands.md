# 17 · Hooks 与斜杠命令

优先级：P3 · 依赖：00、03、05、16

目标：实现 hooks（逐字沿用 goose 协议）与插件斜杠命令。只填 `src/hooks.rs`、
`src/commands.rs`，以及在 `src/agent/mod.rs` 中预留/接好 hook 调用点
（本任务对 `agent/mod.rs` 的修改仅限 hook 触发点）。

验收：`cargo test` 过；每种决策路径、Stop 连续阻止上限、`${PLUGIN_ROOT}` 展开、
命令展开有测试。

计划参考：第三版 §2.7（hooks）、§2.8（commands）。

## R1 · hooks.rs：加载与执行 {#r1}

- 读启用插件的 `dev.instagent/hooks.json`；没有时回退读 `hooks/hooks.json`（goose 草案位置）。
- 文件格式、载荷、决策协议**逐字沿用 goose**：参考 `~/yyds/goose`
  `crates/goose/src/hooks/mod.rs`（`HookContext` 约 225~258 行、决策判定）与
  文档 `documentation/docs/guides/context-engineering/hooks.md`；只搬 `HookContext` 与
  决策判定，其余重写到约 350 行（注明出处）。
- 事件 v1：`SessionStart` `UserPromptSubmit` `PreToolUse` `PostToolUse` `Stop` `SessionEnd`；
  只有 `PreToolUse` 和 `Stop` 能阻止。
- `matcher` 是**正则**（regex crate）匹配工具名；省略则每次都跑。命令用 `sh -c` 跑，
  默认超时 30s。`${PLUGIN_ROOT}` 展开；环境变量透传白名单（`PATH` `HOME` `LANG` + 插件声明）。
- 载荷 JSON 写 stdin：`event` `session_id` 必有，`tool_name` `tool_input` `tool_output`
  `message` `working_dir` 按事件出现（字段名照 goose `HookContext`）。

## R2 · 决策协议 {#r2}

- 退出码 2 → 阻止，理由取 stderr；stdout 是 `{"decision":"block","reason":"..."}` → 阻止，
  不看退出码；退出 0 且 stdout 为空或 `{"decision":"allow"}` → 放行；其他一律"没有决策"，
  按 `on_failure`（默认 allow）处理。
- `Stop` 被阻止时本轮继续跑；连续阻止上限（默认 8 次）防死循环。

## R3 · loop 集成点 {#r3}

- 在 `16` 的 loop 上接触发点：SessionStart / UserPromptSubmit / PreToolUse（在审批之后、
  调用之前；阻止 → is_error 结果）/ PostToolUse / Stop / SessionEnd
  （SessionStart/SessionEnd 的触发入口由 `18` 的会话生命周期调用）。

## R4 · commands.rs：斜杠命令 {#r4}

- 约 60 行：发现启用插件的 `dev.instagent/commands/*.md`；frontmatter
  `description` / `argument-hint`；正文 `$ARGUMENTS` 展开。
- `/review security` 展开成一条用户消息；提供列表与展开接口给 `18`。

## R5 · 测试 {#r5}

- 每种决策路径一个脚本：exit 2、stdout block、exit 0 空、乱输出 + on_failure block、超时；
  Stop 连续阻止上限；`${PLUGIN_ROOT}` 展开；斜杠命令 `$ARGUMENTS` 展开。
