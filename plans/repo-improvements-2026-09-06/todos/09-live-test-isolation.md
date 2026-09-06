difficulty: medium
agent: inherit

# 09 · 真实模型测试的显式运行与夹具隔离

对应方案 E01、E02。前置依赖：无。

## 涉及文件

- `tests/live_e2e.rs`

`tests/fixtures/liveplug/` 仅作为只读输入；不要修改或清理现有 `.hook-out/`。README/usage/release 的最终说明由 10 修改。

## T1 · 每个 Sandbox 独占一份 liveplug

工作：替换返回源码绝对路径的 liveplug_fixture 用法，把版本化 plugin.json、组件目录、scripts 和 skills 复制到当前 Sandbox 的临时目录，保持脚本执行权限。每个 live 用例只使用自己的副本；输出和清理由 TempDir 生命周期负责，不手动 remove 共享目录。复制时明确排除 .hook-out 和其他运行输出。

预计修改文件：`tests/live_e2e.rs`。

验收：
- 无模型/无凭据的测试创建两个 Sandbox，分别运行 fixture hook 或模拟写输出，路径不同、载荷不串；销毁一个不影响另一个。
- 源夹具所有版本化文件的字节/权限保持，测试后不会新增源码目录输出。
- fixture hooks 的真实脚本与权限仍可用；Rust 子进程继续使用进程组和 kill_on_drop。
- 不依赖真实模型的自然语言行为来证明隔离正确。

前置依赖：无。

## T2 · 普通 cargo test 默认离线

工作：10 个真实模型用例采用 #[ignore]，只有显式 --ignored 才运行。显式运行时要求非空凭据，缺失时输出清晰失败，不能以普通 return 假装测试通过。默认模型/覆盖参数和在线超时不顺手变更。

预计修改文件：`tests/live_e2e.rs`。

验收：
- 普通 `cargo test --test live_e2e` 在有/无 key 两种环境均不执行远端用例；默认输出准确显示 10 ignored，新增隔离测试正常运行。
- 用本次进程范围内的假非空值证明默认门控，不打印或读取真实 key 内容。
- `env -u TOKEN_PLAN_API_KEY cargo test --test live_e2e -- --ignored` 按预期失败并明确缺凭据；这是一条负向验收，记录预期非零。
- 显式在线命令为 `cargo test --test live_e2e -- --ignored`。本轮远端已实测超时，不要求用真实 key 重试才能完成此测试结构修复；在线结果另行记录，不伪造。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

