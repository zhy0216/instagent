# ADR 0002: 目标收敛为 sandbox 内的 agent，用户 UI 与 permission 管理降级为非目标

- 状态：已接受
- 日期：2026-09-04
- 影响：第三版 §2.9（审批）、`src/agent/approval.rs`、`src/cli/trust.rs`、REPL 交互确认等

## 背景

instagent 按第三版计划实现时沿用了 goose 的交互形态：`Mode`
（auto / approve / chat）、`Approval` 审批门控（approve 模式下逐工具
确认、`always_allow` 白名单）、CLI 侧的 trust 确认与 REPL 交互式渲染。

但产品目标已经明确：**instagent 是运行在 sandbox 里、对外提供能力的
agent 内核**。隔离、安全边界、资源限制由 sandbox 承担；调用方是程序
（API / 编排层），不是坐在终端前审批每次工具调用的人。在这种形态下：

- permission / 审批不再提供安全性——真正兜底的是 sandbox 的隔离；
  进程内的逐工具确认只是把风险从容器挪到对话里，两边都做等于没做；
- 交互式用户 UI（REPL 渲染、trust 提示、approve 流程）没有真实用户，
  每次跨模块改动（图片、thinking、hooks）却都要顺带维护这些路径。

## 决定

把"用户 UI 与 permission 管理"列为非目标：

- 新功能和重构不再为交互式审批 / CLI 交互体验设计、不投入；
- agent 默认按 auto 模式运行（所有工具直接放行），安全依赖 sandbox；
- 现有 `approval.rs`、`trust.rs`、REPL 确认路径冻结：不删（还能跑、
  测试还在），但停止演进，后续 todo 择机移除；
- `Mode::Approve` / `always_allow` 不再出现在文档推荐用法里。

## 后果

- 内核与外层的边界更干净：instagent 只暴露 turn 执行与事件流，
  宿主（sandbox 编排层）想要 gating 应在自己的层面做。
- CLI 退化为调试/开发入口，不再作为产品界面演进。
- approve 相关代码在移除前是死重量，测试与 CI 继续为其买单；
  移除时另开 todo，按 ADR 0001 的先例从 git 历史可找回。
- 若将来出现"给人用的本地模式"需求，推翻本 ADR 即可，决策记录不
  阻塞回退。
