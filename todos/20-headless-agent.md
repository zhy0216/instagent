# 20 — Headless agent 定位迁移

状态：已完成（2026-09-05）。依赖：已归档的基础实现。依据：`plans/headless-agent/plan.md`。

## 涉及文件

- `src/main.rs`、`src/cli/{mod,handlers,render,repl,assembly}.rs`
- `src/agent/{mod,prompt,event,compact}.rs`、`src/commands.rs`、`src/lib.rs`（仅说明，不改模块树）
- `src/tools/mcp.rs`、`src/subprocess.rs`（初始化取消的进程组回收）
- `tests/mcp_e2e.rs`（若初始化取消回归需要）
- `tests/cli_e2e.rs`、`tests/live_e2e.rs`、`tests/agent_continuation.rs`（若契约断言需要）
- `Cargo.toml`（描述及删除 rustyline）、`Cargo.lock`（对应依赖移除）
- `README.md`、`AGENTS.md`、`todos/README.md`
- `docs/{usage,architecture,goose-plugin-core-plan,goose-from-scratch-plan}.md`
- `docs/adr/0003-repo-boundaries-and-runtime-policies.md`、`docs/adr/0004-headless-agent.md`
- `plans/headless-agent/plan.md`、本文件

## 验收

遵循方案全部目标；完整 fmt / clippy / test 通过；现行文档不再指导用户进入 REPL；插件 provider/MCP/工具/skills/hooks/模板仍可由无交互任务使用。归档 `todos/done/` 不改动。

## 验证结果

- fmt / clippy 通过；完整离线 cargo test 586 项通过，含 49 项 headless CLI 集成测试。
- 单独真实 provider 套件 10 项均超时，未验证远端链路；此结果已在方案中明确记录。
- 实现、文档和回归证据见方案“实施验收”；归档 todos 未修改。
