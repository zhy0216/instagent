# repo-improvements-2026-09-05 任务队列

方案：[../plan.md](../plan.md)。一个 todo = 一个 worktree = 一个最终本地 commit。按以下顺序选择依赖已合入的任务；归档仅进入本目录的 `done/`，不修改旧计划或根 `todos/done/`。

## 优先级

| 文件 | 优先级 | 难度 | 模型 | 说明 |
| --- | --- | --- | --- | --- |
| 01-sse-stream-integrity.md | P1 | hard | max | UTF-8/SSE、正常终止与输入预算 |
| done/02-session-io-recovery.md ✅ | P1 | hard | max | 会话有界读取、恢复、成批追加与备份保护 |
| 03-agent-turn-continuation.md | P1 | hard | max | 执行前校验、续轮、取消和摘要完整性 |
| done/04-plugin-settings-recovery.md ✅ | P1 | hard | max | 保留白名单模式和安装恢复备份 |
| 05-proxy-lifecycle.md | P1 | hard | max | 有限换端口重试、总期限和并发重启 |
| 06-bundled-snapshots.md | P1 | hard | max | bundled 完整快照与并发物化 |
| 07-tool-inventory-io.md | P1 | hard | max | 一致工具缓存、失败恢复与有界 MCP 日志 |
| 08-cli-diagnostics-validation.md | P1 | hard | max | 默认可见诊断、配置校验与 CLI 回归 |
| 09-ci-docs-alignment.md | P2 | medium | flash | Python/CI/MSRV 和最终文档 |

flash = `bailian-token-plan/qwen3.8-flash`；max = `bailian-token-plan/qwen3.8-max`。必须显式传 `--auto --model`，不静默换模型。

## 文件

1. [01-sse-stream-integrity.md](01-sse-stream-integrity.md) — 待执行。依赖：无。
2. [done/02-session-io-recovery.md](done/02-session-io-recovery.md) ✅ — 完成。有界字节读取（header 64KiB/单行 96MiB/总量 256MiB），错误只带路径/行号/约束；坏正文保留合法前缀并物理修复，缺换行尾规范化后可追加，超预算原文件不动；新增 `Session::append_batch`（签名与语义见归档文件完成记录），非法零落盘、IO 失败回退旧长度+旧内存、symlink 双向拒绝；tmp 任一失败清理，备份创建即 0600、精确归属、每会话 5 份。依赖：无。
3. [03-agent-turn-continuation.md](03-agent-turn-continuation.md) — 待执行。依赖 01-sse-stream-integrity、02-session-io-recovery。
4. [done/04-plugin-settings-recovery.md](done/04-plugin-settings-recovery.md) ✅ — 完成。settings 三态（未表态 / 白名单 / 显式空白名单）在合并、serde 往返与 enable/disable 全入口保留：`[]` 后 enable 写入白名单、禁用最后一项保持白名单模式、低层白名单被高层清空后仍禁用其他名字；read_layer 1 MiB 有界读取，超限 / 坏 JSON / IO 错误均指向对应层文件且原文件不变；移除 `.replaced-*` 无条件清扫，成功替换只清自己的备份、失败仍回滚，list / auto-update / discovery 扫描排除 `.replaced-*` 与 `.tmp-install` 内部目录。依赖：无。
5. [05-proxy-lifecycle.md](05-proxy-lifecycle.md) — 待执行。依赖：无。
6. [06-bundled-snapshots.md](06-bundled-snapshots.md) — 待执行。依赖：无。
7. [07-tool-inventory-io.md](07-tool-inventory-io.md) — 待执行。依赖：无。
8. [08-cli-diagnostics-validation.md](08-cli-diagnostics-validation.md) — 待执行。依赖 03-agent-turn-continuation、04-plugin-settings-recovery、05-proxy-lifecycle、06-bundled-snapshots、07-tool-inventory-io。
9. [09-ci-docs-alignment.md](09-ci-docs-alignment.md) — 待执行。依赖 01-sse-stream-integrity、02-session-io-recovery、03-agent-turn-continuation、04-plugin-settings-recovery、05-proxy-lifecycle、06-bundled-snapshots、07-tool-inventory-io、08-cli-diagnostics-validation。

## 并行与集成

- 初始 01、02、04、05、06、07 独立可并行，最多同时 5 个在途 worktree；按上述顺序补位。
- 01、02 合入后 03 可与其他独立任务并行；08 等全部前置任务，09 最后执行。
- 每个任务的业务文件白名单相互独立，测试也按所属模块分配。全队列的 CLI 集成测试由 08 独占；最终使用文档由 09 独占。
- 每个任务可更新自己的 todo 和本 README 的状态，归档到本计划 `done/`。这些共享队列元数据的冲突由协调器串行 rebase/集成时合并，不因此并发改别人的状态。
- 不修改 `Cargo.toml` 依赖/feature、`Cargo.lock`、`src/lib.rs` 模块声明；不 push、不发 PR、不修改外部服务。
- 方案 RM01–RM10 均为 roadmap，不创建执行任务。

## 每个 commit 的验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

每份 todo 另列定点校验。测试用本地 fake provider/MCP、临时目录和可注入时钟/同步屏障；子进程进程组 + `kill_on_drop(true)`。新增测试不需要在线 API key。最终 09 完成后按 `scripts/ci.sh` 全量复验并验证 MSRV。
