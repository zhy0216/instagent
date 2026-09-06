difficulty: medium
agent: inherit

# 09 · 真实模型测试的显式运行与夹具隔离

对应方案 E01、E02。前置依赖：无。

状态：已完成（2026-09-06），T1/T2 全部验收通过；在线行为本次未运行。

## 涉及文件

- `tests/live_e2e.rs`

`tests/fixtures/liveplug/` 仅作为只读输入；不要修改或清理现有 `.hook-out/`。README/usage/release 的最终说明由 10 修改。

## T1 · 每个 Sandbox 独占一份 liveplug

工作：替换返回源码绝对路径的 liveplug_fixture 用法，把版本化 plugin.json、组件目录、scripts 和 skills 复制到当前 Sandbox 的临时目录，保持脚本执行权限。每个 live 用例只使用自己的副本；输出和清理由 TempDir 生命周期负责，不手动 remove 共享目录。复制时明确排除 .hook-out 和其他运行输出。

预计修改文件：`tests/live_e2e.rs`。

验收：
- [x] 无模型/无凭据的测试创建两个 Sandbox，分别运行 fixture hook 或模拟写输出，路径不同、载荷不串；销毁一个不影响另一个。
- [x] 源夹具所有版本化文件的字节/权限保持，测试后不会新增源码目录输出。
- [x] fixture hooks 的真实脚本与权限仍可用；Rust 子进程继续使用进程组和 kill_on_drop。
- [x] 不依赖真实模型的自然语言行为来证明隔离正确。

前置依赖：无。

## T2 · 普通 cargo test 默认离线

工作：10 个真实模型用例采用 #[ignore]，只有显式 --ignored 才运行。显式运行时要求非空凭据，缺失时输出清晰失败，不能以普通 return 假装测试通过。默认模型/覆盖参数和在线超时不顺手变更。

预计修改文件：`tests/live_e2e.rs`。

验收：
- [x] 普通 `cargo test --test live_e2e` 在有/无 key 两种环境均不执行远端用例；默认输出准确显示 10 ignored，新增隔离测试正常运行。
- [x] 用本次进程范围内的假非空值证明默认门控，不打印或读取真实 key 内容。
- [x] `env -u TOKEN_PLAN_API_KEY cargo test --test live_e2e -- --ignored` 按预期失败并明确缺凭据；这是一条负向验收，记录预期非零。
- [x] 显式在线命令为 `cargo test --test live_e2e -- --ignored`。本轮远端已实测超时，不要求用真实 key 重试才能完成此测试结构修复；在线结果另行记录，不伪造。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 完成记录

- `Sandbox` 各持有一个 liveplug `TempDir`。`LIVEPLUG_FILES` 明确列出 9 个版本化输入，已与 `git ls-files tests/fixtures/liveplug` 核对完全相同；`fs::copy` 保持字节与权限。5 个插件在线用例及 d3 的载荷检查全部使用各自副本；删除共享 `.hook-out` 的逻辑已移除。
- `liveplug_sandboxes_isolate_hooks_and_cleanup` 在不创建 provider、不读取凭据的情况下，并发执行两个副本的真实 SessionStart/PostToolUse hooks，核对完整 JSON 载荷。销毁第一个后，第二个原输出保留、仍可写新载荷，真实 PreToolUse guard 仍可阻止 forbidden_marker；两个临时目录均自动回收。修改一个副本的输入也不会影响另一个或源文件。
- `liveplug_copy_excludes_runtime_outputs` 仅在临时副本中模拟 `.hook-out`、根日志和组件/scripts/skills 目录内的运行输出，证明它们不会复制到新副本，既有输出不会被复制过程清理。
- 离线回归比较全部输入的字节/权限和源码目录清单；独立 SHA-256/mode/路径快照也确认局部 live 验收后源码不变。Hooks 沿用 `ProcessGroupChild`，原在线子进程继续调用 `configure_subprocess`，两条路径均保留进程组与 `kill_on_drop(true)`。
- 10 个在线用例均增加带原因的 `#[ignore]`；显式运行先调用 `require_key`，缺失、空串及纯空白立即失败，不再门控 return。默认模型、覆盖参数、120 秒 provider 超时与 180 秒整体超时保持。

| 校验命令 | 结果 |
| --- | --- |
| `cargo fmt --check` | 退出 0 |
| `cargo clippy --all-targets -- -D warnings` | 退出 0，无 warning |
| `env -u TOKEN_PLAN_API_KEY cargo test` | 退出 0；649 passed、10 ignored，doc-test 0 |
| `env -u TOKEN_PLAN_API_KEY cargo test --test live_e2e` | 退出 0；2 passed、10 ignored |
| `env TOKEN_PLAN_API_KEY=offline-gating-test-only cargo test --test live_e2e` | 退出 0；2 passed、10 ignored；仅该进程注入假非空值 |
| `env -u TOKEN_PLAN_API_KEY cargo test --test live_e2e -- --ignored` | **预期退出 101**；10 个在线用例立即失败，均提示 `TOKEN_PLAN_API_KEY must be set to a non-empty value for explicitly requested live tests` |
| `env TOKEN_PLAN_API_KEY= cargo test --test live_e2e live_a1_run_simple_reply -- --ignored --exact` | **预期退出 101**；空串立即失败，同上诊断 |
| `env TOKEN_PLAN_API_KEY='   ' cargo test --test live_e2e live_a1_run_simple_reply -- --ignored --exact` | **预期退出 101**；纯空白立即失败，同上诊断 |

全量校验时，交接中已定位的 `tests/cli_e2e.rs::plugin_task_template_expands_arguments_and_unknown_template_fails` 仍生成源码 `.hook-out/session_start.json`。本 worktree 初始无该目录，局部 live 验收亦无产物；全量结束确认只有该单文件后，按用户明确授权仅删除该文件及其空目录。最终源码夹具 SHA-256、mode 和路径清单与初始快照完全相同。CLI 路径的永久隔离由 07 负责，本任务未修改 CLI 文件、源码夹具输入或任何用户备份；全量无源码产物需 07+09 集成保证。

本次没有读取、显示或使用真实 key，没有运行真实模型测试，也没有改全局环境。显式在线命令仍为 `cargo test --test live_e2e -- --ignored`；既有远端超时记录不视为通过，本次只验证测试结构与离线隔离。无本任务 blocker。
