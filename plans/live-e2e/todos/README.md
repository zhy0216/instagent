# live-e2e TODO 队列

来源方案：`plans/live-e2e/plan.md`（真模型 qwen3.6-flash 端到端集成测试）。

## 优先级

| 文件 | 优先级 | 难度 | 说明 |
|---|---|---|---|
| 01-liveplug-fixtures.md | P0 | easy | 新增 `tests/fixtures/liveplug/` 静态插件夹具（command tool / hooks / skill / 斜杠命令） |
| 02-live-e2e-tests.md | P0 | hard | 新增 `tests/live_e2e.rs`：Sandbox 骨架 + 门控 + a1–e1 全部用例 |

## 文件

1. `01-liveplug-fixtures.md`
2. `02-live-e2e-tests.md` — 依赖 01（仅 live 实跑验收需要夹具；编码期可并行，离线校验走 skip 路径不碰夹具）

01 与 02 修改文件不相交（`tests/fixtures/liveplug/**` vs `tests/live_e2e.rs`），可并行开工。
