# 02 · 消息模型与会话：message.rs + session.rs

优先级：P1 · 依赖：00

目标：实现消息模型（含会话不变量）与 JSONL 会话持久化。只填 `src/message.rs`、`src/session.rs`。

验收：`cargo test` 过；四条会话不变量、session 追加/读回/原子重写均有测试。

计划参考：第二版 §2.2（消息）、§2.6（会话）。

## C1 · message.rs {#c1}

- `Role`(User/Assistant)、`Content`(Text / ToolUse{id,name,input} / ToolResult{tool_use_id,content,is_error})、
  `Message`(role, content: Vec<Content>, ts, usage)、`Usage`(input/output/cache_read/cache_write)。
- 无 system 角色；压缩摘要是一条以 `# Conversation Summary` 开头的 User 消息。
- 提供会话不变量校验函数（第二版 §2.2 四条）：
  1. user/assistant 严格交替；2. assistant 的每个 ToolUse 在紧接着的 user 消息里有同 id ToolResult；
  3. 不发空 content；4. 被打断的 tool call 补 is_error ToolResult。
- 提供 `Content::interrupted(call)` 构造（文本 "interrupted by user"）。

## C2 · session.rs {#c2}

- 路径 `<data_dir>/sessions/<id>.jsonl`，data_dir 用 etcetera（默认 `~/.local/share/instagent`），
  可被 `INSTAGENT_DATA_DIR` 覆盖（测试用）。id 用 uuid v4。
- 第 1 行 header `{id, created, cwd, provider, model, name}`；之后每行一条 Message；
  `append` = 追加一行 + 立即 flush。
- `list` = 只读每个文件第一行；`resume` = 全量读。
- 原子重写（压缩用，`16` 调用）：写临时文件（header + 新内容）→ rename 覆盖 →
  旧文件改名 `<id>.<n>.bak.jsonl` 保留。

## C3 · 测试 {#c3}

- 四条不变量各一个违反用例（用 tempdir）；追加/读回；原子重写产生 .bak；坏行报错带行号。
