# 仓库改进队列（2026-09-06）

方案：[../plan.md](../plan.md)。基线 `8a446c4`。本队列包含方案全部 17 项非 roadmap 发现；RM01–RM10 只保留在方案中。每个 todo = 一个独立 worktree = 一个最终实现 commit。

## 执行偏好

default_agent: codex

来源：本次 Codex 宿主。用户没有全局模型/推理覆盖，也没有单任务 agent 指定。每个 todo 的 `agent: inherit` 继承这里保存的类型；换协调器/宿主不重新猜测。

按共享分发规则解析：hard → `gpt-6-astra` / `max`；medium → `gpt-6-astra` / `xhigh`；easy → `gpt-6-astra` / `high`（本队列无 easy）。这些是难度映射，不是用户覆盖，因此不保存 default_model/default_reasoning_effort。协调器使用 Codex `gpt-6-astra` / `high`，任务仍按自身难度选档；启动显式使用 skill 要求的 YOLO 参数并校验本机支持情况。

## 优先级

| 文件 | 优先级 | 难度 | agent | 模型 / Codex 推理强度 | 说明 |
| --- | --- | --- | --- | --- | --- |
| [01-agent-completion.md](done/01-agent-completion.md) | P1 | hard | codex（inherit） | gpt-6-astra / max | 已完成：异常工具响应零副作用；有效图片才记账 |
| [02-compaction-integrity.md](done/02-compaction-integrity.md) | P1 | hard | codex（inherit） | gpt-6-astra / max | 已完成：拒绝截断摘要、保留恢复后的全部最新输入 |
| [03-file-tool-io.md](done/03-file-tool-io.md) | P1 | medium | codex（inherit） | gpt-6-astra / xhigh | 已完成：原子写保留权限；edit/read 实际字节预算 |
| [04-session-write-budgets.md](done/04-session-write-budgets.md) | P1 | hard | codex（inherit） | gpt-6-astra / max | 已完成并归档；会话写入不能突破自己的恢复预算 |
| [05-plugin-input-limits.md](done/05-plugin-input-limits.md) | P2 | hard | codex（inherit） | gpt-6-astra / max | 已完成：四类组件文件实际有界读取与来源诊断 |
| [06-local-install-integrity.md](06-local-install-integrity.md) | P1 | medium | codex（inherit） | gpt-6-astra / xhigh | 拒绝安装源/staging 重叠；限制安装元数据 |
| [07-template-input-budget.md](07-template-input-budget.md) | P2 | hard | codex（inherit） | gpt-6-astra / max | 模板展开前预算与 CLI 终态回归 |
| [08-provider-required-key.md](08-provider-required-key.md) | P2 | medium | codex（inherit） | gpt-6-astra / xhigh | 显式必需凭据拒绝空串与空白 |
| [09-live-test-isolation.md](09-live-test-isolation.md) | P1 | medium | codex（inherit） | gpt-6-astra / xhigh | live 显式 opt-in、每测试独占插件夹具 |
| [10-docs-and-validation.md](10-docs-and-validation.md) | P2 | medium | codex（inherit） | gpt-6-astra / xhigh | 集成后同步用户契约和完整验证记录 |

## 文件

按数字顺序调度；依赖必须已合入再启动。

1. [01-agent-completion.md](done/01-agent-completion.md) — 已完成（T1/T2，离线校验通过）。依赖：无。
2. [02-compaction-integrity.md](done/02-compaction-integrity.md) — 已完成并归档。依赖：无。
3. [03-file-tool-io.md](done/03-file-tool-io.md) — 已完成。依赖：无。
4. [04-session-write-budgets.md](done/04-session-write-budgets.md) — 已完成并归档。依赖：无。
5. [05-plugin-input-limits.md](done/05-plugin-input-limits.md) — 已完成。依赖：无。
6. [06-local-install-integrity.md](06-local-install-integrity.md) — 待执行。依赖：无。
7. [07-template-input-budget.md](07-template-input-budget.md) — 待执行。依赖：无。
8. [08-provider-required-key.md](08-provider-required-key.md) — 待执行。依赖：无。
9. [09-live-test-isolation.md](09-live-test-isolation.md) — 待执行。依赖：无。
10. [10-docs-and-validation.md](10-docs-and-validation.md) — 待执行。
    依赖 01-agent-completion、02-compaction-integrity、03-file-tool-io、04-session-write-budgets、05-plugin-input-limits、06-local-install-integrity、07-template-input-budget、08-provider-required-key、09-live-test-isolation。

## 并行与文件归属

01–09 的实现文件无交集，可并行，按本表顺序在最多 5 个 Herdr 实现槽位内调度。10 等全部修复合入。02 的测试写在 compact 模块内，不能修改 01 所有的 tests/agent_continuation.rs；05 不改 06 的 install.rs 或 08 的 shared/openai；CLI 集成测试由 07 独占。

完整白名单见各 todo。保持公开接口兼容，尤其 Session 和现有模板调用。实现若发现必须更改他人文件，先协调所有权和依赖，不同时编辑同一文件、不偷偷扩大范围。git rebase、校验、merge 和清理仍按 herdr-finish-plan 的串行集成规则执行。

## 校验与交接门槛

每个 commit：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全部通过。各任务写清其故障/预算/行为回归；10 另跑 ci.sh、MSRV。

基线原样 cargo test 的 10 个 live 用例均在 180 秒超时，离线全量 586 个实际 Rust 测试和 11 个 Python 测试通过。09 合入前只对测试进程使用 `env -u TOKEN_PLAN_API_KEY cargo test`；之后普通测试必须默认离线。在线验证按需显式运行，无法访问远端就记录未验证，不反复重试掩盖错误。

当前计划文件尚未提交、执行未启动。仓库已有未跟踪的 `tests/fixtures/liveplug/.hook-out/`，在本轮开始前就存在。必须遵守 auto-dev 的 dirty-worktree 检查：不得提交、stash、忽略或删除该目录；用户给出明确处理指令或自行处理完后，重新检查，仅提交本计划，再启动协调器。当前 `HERDR_ENV=1` 已确认，实际 agent/模型参数须启动前检查。

