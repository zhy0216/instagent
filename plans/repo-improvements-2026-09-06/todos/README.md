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
| [06-local-install-integrity.md](done/06-local-install-integrity.md) | P1 | medium | codex（inherit） | gpt-6-astra / xhigh | 已完成：拒绝安装源/staging 重叠；限制安装元数据 |
| [07-template-input-budget.md](done/07-template-input-budget.md) | P2 | hard | codex（inherit） | gpt-6-astra / max | 已完成：模板展开前预算、CLI 终态回归与模板测试夹具隔离 |
| [08-provider-required-key.md](done/08-provider-required-key.md) | P2 | medium | codex（inherit） | gpt-6-astra / xhigh | 已完成：显式必需凭据拒绝空串与空白；构造诊断不泄露原值 |
| [09-live-test-isolation.md](done/09-live-test-isolation.md) | P1 | medium | codex（inherit） | gpt-6-astra / xhigh | 已完成：live 显式 opt-in、每测试独占插件夹具 |
| [10-docs-and-validation.md](done/10-docs-and-validation.md) | P2 | medium | codex（inherit） | gpt-6-astra / xhigh | 已完成：用户契约、完整 CI/MSRV 验证与风险记录 |

## 文件

按数字顺序调度；依赖必须已合入再启动。

1. [01-agent-completion.md](done/01-agent-completion.md) — 已完成（T1/T2，离线校验通过）。依赖：无。
2. [02-compaction-integrity.md](done/02-compaction-integrity.md) — 已完成并归档。依赖：无。
3. [03-file-tool-io.md](done/03-file-tool-io.md) — 已完成。依赖：无。
4. [04-session-write-budgets.md](done/04-session-write-budgets.md) — 已完成并归档。依赖：无。
5. [05-plugin-input-limits.md](done/05-plugin-input-limits.md) — 已完成。依赖：无。
6. [06-local-install-integrity.md](done/06-local-install-integrity.md) — 已完成。依赖：无。
7. [07-template-input-budget.md](done/07-template-input-budget.md) — 已完成并归档。依赖：无。
8. [08-provider-required-key.md](done/08-provider-required-key.md) — 已完成并归档，离线校验通过。依赖：无。
9. [09-live-test-isolation.md](done/09-live-test-isolation.md) — 已完成并归档（T1/T2，离线校验与缺凭据负向验收通过）。依赖：无。
10. [10-docs-and-validation.md](done/10-docs-and-validation.md) — 已完成并归档（T1/T2，最终完整 CI/MSRV 通过；保留 proxy 波动记录，在线未验证）。
    依赖 01-agent-completion、02-compaction-integrity、03-file-tool-io、04-session-write-budgets、05-plugin-input-limits、06-local-install-integrity、07-template-input-budget、08-provider-required-key、09-live-test-isolation。

## 并行与文件归属

01–09 的实现文件无交集，可并行，按本表顺序在最多 5 个 Herdr 实现槽位内调度。10 等全部修复合入。02 的测试写在 compact 模块内，不能修改 01 所有的 tests/agent_continuation.rs；05 不改 06 的 install.rs 或 08 的 shared/openai；CLI 集成测试由 07 独占。

完整白名单见各 todo。保持公开接口兼容，尤其 Session 和现有模板调用。实现若发现必须更改他人文件，先协调所有权和依赖，不同时编辑同一文件、不偷偷扩大范围。git rebase、校验、merge 和清理仍按 herdr-finish-plan 的串行集成规则执行。

## 校验与交接门槛

每个 commit：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全部通过。各任务写清其故障/预算/行为回归；10 另跑 ci.sh、MSRV。

基线原样 cargo test 的 10 个 live 用例均在 180 秒超时，离线全量 586 个实际 Rust 测试和 11 个 Python 测试通过。09 合入前只对测试进程使用 `env -u TOKEN_PLAN_API_KEY cargo test`；之后普通测试必须默认离线。在线验证按需显式运行，无法访问远端就记录未验证，不反复重试掩盖错误。

队列已全部执行并归档。执行起点为用户已提交的 `892dcfd`，开始时 main 干净；原有 `.hook-out` 已由用户备份至 `/tmp/instagent-hook-out-20260906-9kg222l8/liveplug-hook-out`，本轮未删除或移回。所有任务经 Herdr / Codex gpt-6-astra 显式 YOLO 启动，按上表推理强度执行；独立复核、串行 rebase、仓库级校验和 ff-only 合入完成，任务资源全部清理。最终 662 项 Rust 离线测试与 11 项 Python 测试通过，10 项 live ignored，真实模型本轮未重试；扩展 CI 和 MSRV 通过。完整提交映射、校验证据及执行中观察见 [方案执行结果](../plan.md)。

