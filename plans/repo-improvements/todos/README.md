# repo-improvements 任务队列

基线：`24c2316`，`cargo fmt --check` / `clippy -D warnings` / `test`（含 `anthropic-engine` feature 两步）全绿。
每个任务完成后必须跑 plan.md「校验」一节的全部五条命令。

## 优先级

| 文件 | 优先级 | 难度 | 说明 |
|---|---|---|---|
| `01-quick-fixes.md` ✅ done | P2 | easy | 死代码、过期文案、README 安装命令修正 |
| `done/02-builtin-tools-hardening.md` ✅ | P1 | medium | fs/tree/shell 原子写、符号链接、流式读、权限 |
| `done/03-session-robustness.md` ✅ | P1 | medium | rewrite 崩溃窗口、坏行 salvage、半死字段 |
| `done/04-subprocess-io-dedup.md` ✅ | P1 | medium | hooks/command 双份进程输出模板收敛 |
| `done/05-agent-run-turn-extract.md` ✅ | P1 | medium | run_turn 工具执行段抽函数 |
| `done/06-provider-shared-stream.md` ✅ | P1 | hard | openai/anthropic 共享 SSE 流驱动层 |
| `done/07-cli-confirm-parse.md` ✅ | P1 | medium | 审批输入解析抽纯函数 + 单测 |
| `done/08-ci-audit-matrix.md` ✅ | P1 | easy | CI 加 cargo-audit（不阻断）+ macOS matrix + smoke |
| `done/09-cli-e2e-tests.md` ✅ | P1 | hard | CLI 二进制级集成测试收尾 |

## 文件

1. `01-quick-fixes.md` — 依赖：无
2. [done/02-builtin-tools-hardening.md](done/02-builtin-tools-hardening.md) — ✅ 完成。依赖：无
3. [done/03-session-robustness.md](done/03-session-robustness.md) — ✅ 完成。依赖：无
4. `done/04-subprocess-io-dedup.md` — ✅ 完成。依赖：无
5. `done/05-agent-run-turn-extract.md` — ✅ 完成。依赖：无
6. [done/06-provider-shared-stream.md](done/06-provider-shared-stream.md) — ✅ 完成。依赖：无
7. [done/07-cli-confirm-parse.md](done/07-cli-confirm-parse.md) — ✅ 完成。依赖：无
8. [done/08-ci-audit-matrix.md](done/08-ci-audit-matrix.md) — ✅ 完成。依赖：无
9. [done/09-cli-e2e-tests.md](done/09-cli-e2e-tests.md) — ✅ 完成。依赖 02、03、04、05、06（在重构后的最终形态上写集成测试）

01–08 修改的文件互不相交，可并行执行；09 最后。
