difficulty: hard
agent: inherit
status: done

# 04 · 会话写入与恢复预算一致

对应方案 S01。前置依赖：无。

## 涉及文件

- `src/session.rs`
- `tests/session_recovery.rs`

## T1 · 各写入入口提交前校验预算

工作：create、append_batch、rewrite/atomic_replace 的候选序列化结果遵守 DEFAULT_LIMITS 的 header、单消息行和总文件预算。按实际 JSON UTF-8 字节计数（含转义与换行），使用受检运算。不要只看 Message 文本长度或只检查追加批次大小；总预算要包含既有文件。保持公开方法签名和默认数值，复用内部预算类型即可，不引入数据库或持久缓存。

预计修改文件：`src/session.rs`。

验收：
- [x] 不能成功写出默认 resume 会因预算拒绝的主文件。
- [x] append 拒绝时内存和文件未变；rewrite 拒绝时不发布新主文件、不覆盖/淘汰可用备份；create 拒绝时不留下损坏会话。
- [x] 保留已有部分 IO 写入回退、私有文件、salvage 与备份归属语义；budget 错误不回显原文。
- [x] JSON 转义膨胀、换行开销、多个小批次跨总预算都得到正确结果。

前置依赖：无。

## T2 · 小预算故障与恢复回归

工作：用测试可注入的小预算覆盖 create/append_batch/rewrite 的边界与拒绝原子性；保留公开生产入口默认预算。补成功写入→resume 的完整流程，不把巨大 fixture 当作日常测试手段。

预计修改文件：`src/session.rs`、`tests/session_recovery.rs`。

验收：
- [x] header、单行、总量分别测试刚好边界和超一字节。
- [x] 消息不变量与 IO 失败路径也验证旧内容保留；失败后继续合法写入并恢复成功。
- [x] 拒绝写入不声称外部工具副作用被回滚；不自动裁剪历史或增大恢复限额。

前置依赖：T1。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 完成记录（2026-09-06）

- 生产公开入口继续使用原 `DEFAULT_LIMITS`，内部复用 `ReadLimits`。`serialize_line` 按实际 JSON UTF-8 字节检查 header/消息行，行限额不含 LF，总限额包含 LF；字节换算与累计使用受检运算。
- create 在创建文件前校验 header；append 校验全部候选行，并用已打开文件的实际长度加批次字节数检查总预算；rewrite 在临时文件完整写入并通过预算后才备份和发布。超限不改变内存历史、主文件和备份，也不遗留临时文件。
- `resume_with_limits` 的 salvage 写回沿用同一预算，覆盖缺尾换行及旧消息缺 `usage` 字段导致规范化增大的情况；修复成功后才输出已修复的诊断。
- `create_checks_exact_header_and_total_budgets_before_creating_file`、`append_checks_serialized_lines_and_total_before_committing_any_of_the_batch`、`rewrite_budget_refusals_preserve_memory_main_and_all_usable_backups` 覆盖三类限额恰好边界和超一字节。备份达到保留上限后拒绝 rewrite，整个会话目录逐字节保持不变。
- `small_appends_include_prior_batches_and_newlines_in_the_total_budget` 与 `append_total_uses_actual_existing_file_bytes` 覆盖多个小批次和未进入内存历史的既有空白行；`total_write_budget_rejects_arithmetic_overflow` 验证累计溢出拒绝。
- 部分写入、flush、打开、临时文件创建、rename 及消息不变量失败回归验证旧内容保留，合法重试后可恢复；既有 0600/0700、symlink、备份归属、salvage 回归继续通过。
- 公开 API 回归用 11 KiB NUL 文本触发 JSON 转义后的默认 header 超限，另覆盖 create → append_batch → resume → rewrite → append_batch → resume，包含 Unicode、控制字符、工具参数和工具结果。没有巨大 fixture、新依赖、feature、公共 API 或模块树变更。
- 会话写入拒绝只约束会话持久化，不回滚工具的外部副作用；不自动裁剪历史或提高恢复限额，保留单进程独占和现有 durability 边界。

校验结果：

| 命令 | 结果 |
| --- | --- |
| `env -u TOKEN_PLAN_API_KEY cargo test --lib session::tests` | 46 passed |
| `env -u TOKEN_PLAN_API_KEY cargo test --test session_recovery` | 11 passed |
| `cargo fmt --check` | 退出 0 |
| `cargo clippy --all-targets -- -D warnings` | 退出 0，无 warning |
| `env -u TOKEN_PLAN_API_KEY cargo test --test provider_proxy` | 15 passed（首次全量失败后的隔离复跑） |
| `env -u TOKEN_PLAN_API_KEY cargo test` | 最终退出 0：lib 473、bin 10、agent_continuation 21、cli_e2e 49、mcp_e2e 15、provider_proxy 15、session_recovery 11、tool_inventory 4；共 598 个离线测试，doc-test 0 |

首次全量校验退出 101：`tests/provider_proxy.rs` 的 `concurrent_transport_failures_restart_once_and_both_succeed`（451 行，2 != 1）和 `restart_is_bounded_to_once_per_call`（293 行，3 != 2）计数断言失败。未修改 proxy 文件；隔离复跑及随后完整重跑均通过，记录为未进一步归因的既有测试波动。

两次全量运行的 10 个 live 测试函数都因测试进程缺少 `TOKEN_PLAN_API_KEY` 门控返回（harness 显示 passed，非在线验证）；没有运行真实模型、读取真实凭据或修改全局环境。

收尾发现 `tests/cli_e2e.rs:781` 的离线模板测试直接加载源码 liveplug，因此仍生成了本 worktree 的 `.hook-out/session_start.json`。未修改该任务外文件；已将本次新产物保留到 `/tmp/instagent-session-write-budgets-hook-out-20260906-03w_4m_z/liveplug-hook-out`，恢复 worktree 干净。用户指定的原备份完全未动；此离线 CLI 夹具隔离问题交由该文件所属任务或集成协调器处理。
