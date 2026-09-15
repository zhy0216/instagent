# 21 — 修复 headless 审查问题

状态：已完成。依据：2026-09-14 用户确认修复审查结果并 commit / push。

## 涉及文件

- `src/main.rs`、`src/cli/{mod,handlers,render,assembly,output}.rs`
- `src/agent/{mod,prompt,event,compact,task}.rs`、`src/agent/task/assembly.rs`
- `src/commands.rs`、`src/session.rs`
- `src/plugin/manifest.rs`、`src/tools/{mod,mcp}.rs`
- `tests/{cli_e2e,task_api,tool_inventory,mcp_e2e}.rs`
- `README.md`、`docs/{usage,architecture}.md`、`docs/adr/0004-headless-agent.md`
- `todos/README.md`、本文件

## 验收

- stdout/stderr 背压不阻塞任务期限、取消或清理，JSON 交付失败非零退出。
- 模板在分配展开结果前检查大小和整数溢出。
- 插件 `minKernel` 有明确格式和最低版本校验。
- 库入口复用任务输入、恢复、装配、期限、生命周期与结构化结果；无全局信号处理。
- 单任务插件/工具选择、必需工具预检与结构化诊断；MCP 连接有限并发并保持确定性顺序。
- 补充离线回归；fmt / clippy / cargo test 全部通过后提交并推送。
- 不增加依赖，不修改 `src/lib.rs` 模块声明，不修改归档 todo。

## 实现与验证

- CLI 使用独立写线程和有界进度队列；JSON 写入及 flush 必须在 1 秒交付预算内确认。
- 模板展开使用受检长度计算，`minKernel` 在 manifest 加载阶段校验。
- `agent::task::run` 为 CLI 和 Rust 宿主共用入口，涵盖输入、恢复、期限、hooks、
  清理和报告；支持调用方取消，任务超时不取消父令牌。
- 增加任务级插件/工具白名单、必需工具预检、报告诊断；MCP 连接跨插件最多
  并发 4 路，工具清单枚举最多并发 4 路，输出及路由顺序确定。
- 2026-09-14 `bash scripts/ci.sh` 全部通过：fmt、clippy、Rust 测试 675 passed /
  0 failed / 10 ignored（含 1 个库接口文档编译测试）、Python 测试 11 passed、
  rustdoc、release all-targets 检查与 CLI help。10 个 ignored 真实模型用例未运行；
  cargo-audit 未安装，CI 脚本按既有政策跳过。
- 新回归覆盖未读取 stdout/stderr 的超时与取消、子进程清理、JSON 交付超时、
  256 MiB 地址空间内拒绝 1.44 GB 模板展开、版本不兼容、工具预检/调用过滤、
  并发连接上限与顺序，以及库调用/恢复/超时/取消和 future 的 Send 约束。
