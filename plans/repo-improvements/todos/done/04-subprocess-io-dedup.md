difficulty: medium

hooks 与 command tools 各自维护一份相同的"子进程输出读取"模板（后台写 stdin 防死锁 + 读 stdout/stderr + kill 后限时 drain）。收敛到 `subprocess.rs`，消除双份实现。

## T1 · 在 `subprocess.rs` 提供共享输出读取助手

- `src/hooks.rs:474-554` 与 `src/tools/command.rs:37,149-156,264-274` 各有相同的 `OUTPUT_DRAIN_TIMEOUT`（均 500ms）、`drain`、`read_all` 实现。把三者上提到 `src/subprocess.rs`（`pub(crate)`），语义逐字节保持：
  - `OUTPUT_DRAIN_TIMEOUT = 500ms`；
  - `read_all`：带超时读子进程流到 EOF 或超时；
  - `drain`：进程组 kill 后的限时残留输出收集。
- 签名以两处现有调用需要为准，允许统一为更通用的形态，但超时/截断行为不得变化。
- 预计修改文件：`src/subprocess.rs`。
- 验收：`cargo test` 通过；新助手有至少 1 个直接单测（超时触发 + EOF 两种路径）。
- 前置依赖：无。

## T2 · hooks.rs 改用共享助手

- `src/hooks.rs` `run_hook`（478-536）与本地 `drain`/`read_all`（538-554）替换为 T1 的共享版本，删除本地副本与本地常量。
- 预计修改文件：`src/hooks.rs`。
- 验收：现有 13 个 hooks 测试全部保持绿（尤其超时杀进程组、stdin 后台写、决策解析相关）；`rg "OUTPUT_DRAIN_TIMEOUT|fn drain|fn read_all" src/hooks.rs` 无残留。
- 前置依赖：依赖 04 文件内 T1。

## T3 · tools/command.rs 改用共享助手

- `src/tools/command.rs`（37 行常量、149-156、264-274 附近）替换为共享版本，删除本地副本。
- 预计修改文件：`src/tools/command.rs`。
- 验收：现有 6 个 command 测试全部保持绿（超时、进程组、stdin 注入路径）；无本地残留。
- 前置依赖：依赖 04 文件内 T1。

## 本文件整体验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```
