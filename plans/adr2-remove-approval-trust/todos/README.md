# adr2-remove-approval-trust TODO 队列

来源方案：`plans/adr2-remove-approval-trust/plan.md`（ADR 0002 落地：删除
approval / trust / Mode，permission UI 整体移除）。

## 优先级

| 文件 | 优先级 | 难度 | 说明 |
|---|---|---|---|
| done/01-remove-permission-code.md ✅ | P0 | hard | src/ 全量删除 approval/trust/Mode + 单测与集成测试修复（一个 `feat!:` commit） |
| done/02-sync-docs.md ✅ | P1 | easy | README、usage.md、architecture.md、ADR 0002 状态同步 |

## 文件

1. `done/01-remove-permission-code.md` ✅ — 完成。
   Rust 整仓一起编译，lib 与 bin 引用必须同提交清理，不可再拆并行。
2. `done/02-sync-docs.md` ✅ — 完成。依赖 01（文档必须描述删除后的代码）。

01 与 02 改的文件不相交，但 02 内容取决于 01 的最终形态，串行执行。
