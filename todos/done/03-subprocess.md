# 03 · 子进程：subprocess.rs

优先级：P1 · 依赖：00

目标：从 goose 搬子进程工具（进程组 + kill_on_drop），供 shell 工具、MCP stdio、
hooks、proxy 复用。只填 `src/subprocess.rs`。

验收：`cargo test` 过；有"drop 后子进程组被整体杀掉、无残留"的测试。

计划参考：第二版 §2.12；第三版 §6（从 goose 拿什么）。

## D1 · 移植 {#d1}

- 从 `~/yyds/goose/crates/goose/src/subprocess.rs`（144 行，`configure_subprocess`、
  `spawn_long_lived_mcp_subprocess`）移植，改成适配本仓库；commit message 注明出处。
- 一律进程组 + `kill_on_drop(true)`，防 Ctrl-C 后残留。

## D2 · 测试 {#d2}

- spawn 一个会睡的脚本，drop handle 后断言进程组被杀、无残留。
