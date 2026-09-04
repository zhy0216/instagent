# repo-improvements-2026-09-04 任务队列

本队列基于 `plans/repo-improvements-2026-09-04/plan.md` 与当前仓库实现拆解。
每个文件是一个独立任务、一个 worktree、一个最终 commit；任务完成后必须运行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

easy / medium 任务使用 flash，hard 任务使用 max。

## 优先级

| 文件 | 优先级 | 难度 | 说明 |
|---|---|---|---|
| `done/01-policy-decisions.md` ✅ | P1 | medium | 固化密钥、环境、hook、CLI 流和 settings 语义，以及 sandbox 责任边界 |
| `done/02-session-boundary.md` ✅ | P1 | medium | session id、坏尾部 salvage、权限、耐久性和 `last` 选择 |
| `done/03-settings-atomic-merge.md` ✅ | P1 | medium | settings / 安装元数据原子私有写入、tri-state 合并和本地配置忽略 |
| `04-file-tool-budgets.md` | P1 | hard | fs/tree/skills/image 的阻塞、路径、大小、深度和取消边界 |
| `done/05-subprocess-collector.md` ✅ | P1 | medium | 进程输出上限、增量 UTF-8 和无 pipe 配置错误 |
| `06-process-tool-isolation.md` | P1 | hard | shell/command/hooks 的 bounded output、argv/env 隔离和 hook 策略 |
| `07-plugin-install-resilience.md` | P1 | medium | git 安装超时/进程组、替换回滚、孤儿备份和诊断 |
| `08-config-provider-validation.md` | P1 | hard | config/provider 字段校验、key 来源实现、schema drift 和 context limit 诊断 |
| `09-message-contract.md` | P1 | medium | message role/tool id/image 合约与统一边界校验 |
| `10-mcp-inventory.md` | P1 | medium | MCP 连接部分失败、inventory timeout/cancel、缓存和可见诊断 |
| `11-agent-event-contract.md` | P2 | medium | event 背压、残缺 stream 和 session hook 错误可见性 |
| `12-cli-stream-and-e2e.md` | P1 | hard | stdout/stderr 契约、退出码、取消和 CLI/PTY 回归 |
| `13-parallel-tools.md` | P1 | hard | 只读工具并行、冲突排序、图片/并发预算和取消 |
| `14-manifest-schema.md` | P2 | medium | plugin manifest 版本、字段形状和跨字段校验 |
| `15-discovery-diagnostics.md` | P2 | medium | 插件来源类型和目录枚举错误诊断 |
| `16-docs-and-rustdoc.md` | P2 | easy | 当前工具数量、ADR/历史计划标记和 rustdoc 警告清理 |
| `17-ci-release-metadata.md` | P2 | medium | 包元数据、toolchain 支持范围、audit/doc/release CI 门槛 |
| `18-provider-converter.md` | P2 | medium | provider converter 的 source 注入、fixture 和 round-trip 校验 |
| `19-agent-module-split.md` | P2 | hard | 在行为测试护栏下拆分 agent loop 与工具执行职责 |
| `20-hooks-provider-split.md` | P2 | hard | 在契约锁定后拆分 hooks 与 OpenAI transport/parser |
| `21-install-module-split.md` | P2 | medium | 在安装回归稳定后拆分安装流程、git 和替换状态机 |

## 文件

1. `done/01-policy-decisions.md` ✅ — 完成。ADR 0003 落地六项决策（D1–D6），解锁所有策略相关实现。
2. `done/02-session-boundary.md` ✅ — 完成。validate_session_id 白名单、salvage 原子写回+有界时间戳备份、0700/0600 权限、rename 失败清理、list/`last` 诊断跳过。
3. `03-settings-atomic-merge.md` — 依赖：01。
4. `04-file-tool-budgets.md` — 依赖：01。
5. `done/05-subprocess-collector.md` ✅ — 完成。`run_bounded` 硬上限 collector（越限杀组、保留摘要、增量 UTF-8），`wait_and_drain` 无 pipe 改带上下文错误。
6. `06-process-tool-isolation.md` — 依赖：01、05。
7. `07-plugin-install-resilience.md` — 依赖：03、05。
8. `08-config-provider-validation.md` — 依赖：01。
9. `09-message-contract.md` — 依赖：01。
10. `10-mcp-inventory.md` — 依赖：01、05。
11. `11-agent-event-contract.md` — 依赖：06、09。
12. `12-cli-stream-and-e2e.md` — 依赖：06、07、10、11。
13. `13-parallel-tools.md` — 依赖：04、05、06、09、10、11。
14. `14-manifest-schema.md` — 依赖：01、08。
15. `15-discovery-diagnostics.md` — 依赖：03。
16. `16-docs-and-rustdoc.md` — 依赖：08、12、13、14。
17. `17-ci-release-metadata.md` — 依赖：01、16。
18. `18-provider-converter.md` — 依赖：08。
19. `19-agent-module-split.md` — 依赖：11、13、16。
20. `20-hooks-provider-split.md` — 依赖：06、08、11、13、16。
21. `21-install-module-split.md` — 依赖：07、12、16。

## 并行批次

- `02`、`03`、`04`、`05`、`08`、`09` 可在 `01` 合并后并行；它们的代码文件互不重叠（`03` 对 `plugin/install.rs` 的改动完成后再启动 `07`）。
- `06`、`07`、`10`、`14`、`15` 依赖前置底座，可按文件不重叠原则并行。
- `11` 完成后才能开始 `12`；`13` 必须等资源、MCP 和 event 契约稳定。
- `16`、`17`、`18` 属于收口阶段；`19`–`21` 是所有行为测试通过后的最后重构任务，不应提前并行。

以下内容明确不入队：方案中的 RM1–RM6 roadmap 项，以及 T6 property/fuzz/覆盖率长期门槛；它们应另立 ADR/计划。
