# 18 · CLI：chat / run / sessions / plugin + 运行时装配

优先级：P4 · 依赖：00、16、10、11、14、15、17、01

目标：实现终端 CLI 与"配置 → 插件 → 工具源 → provider"的运行时装配。只填
`src/cli/**`、`src/main.rs`（`00` 的 todo!() 入口）。

验收：`cargo test` 过；README 里列出手工验证清单（多轮、Ctrl-C、/compact、--resume、run -t、
plugin 子命令、首次启用确认）。

计划参考：第二版 §2.11；第三版 §2.10（信任）、§8（装完一个插件后的完整图景）。

## S1 · 运行时装配 {#s1}

- config（01）+ settings → `05` 发现启用插件 → 构建全部工具源并注册进 `13` Registry：
  `BuiltinTools` + 每个 MCP server 一个 `McpSource`（14）+ `CommandTools` + `SkillsSource`（15）。
- `10` registry 按名字取 provider → engine 实例（`11` proxy / `12` anthropic 视合并情况可用）。
- MCP `instructions` 与全部 skill 的 name+description 注入 `16` 系统提示。
- `--plugin PATH` 临时加载插件。
- 会话生命周期触发 `17` 的 SessionStart / SessionEnd hooks。

## S2 · chat / run / sessions {#s2}

- `instagent chat [--resume <id>|last] [--cwd DIR] [-m MODEL] [--mode MODE] [--plugin PATH]`。
- `instagent run -t "..."`：无交互，审批按 auto，结束打印最终回复和 usage。
- `instagent sessions list | rm <id>`。

## S3 · REPL {#s3}

- rustyline 循环；斜杠命令：`/exit` `/clear` `/compact` `/mode <m>` `/tools` `/help`，
  加上 `17` 的插件斜杠命令（动态列表）。
- Ctrl-C：第一次取消当前轮（cancel token），第二次退出。

## S4 · 渲染与审批提示 {#s4}

- 文本流式直接打印；工具调用打一行 `▶ shell  ls -la`，完成后打前 10 行预览和耗时；
  每轮末尾打 usage；不做 markdown 渲染。
- 审批提示：`allow this call? [y]es / [a]lways / [n]o: `，接 `16` 的 Confirm trait。

## S5 · plugin 子命令与信任确认 {#s5}

- `instagent plugin install <git-url | path> [--auto-update] | list | update | enable |
  disable | show`，接 `07` 数据层。
- 信任确认：插件里只要有会执行命令的东西（mcp.json、hooks、command tools、proxy provider），
  第一次启用时列出全部命令让用户确认，结果记到 settings 的 `trustedPlugins`；`--yes` 跳过。
  未信任的插件：加载但拒绝拉起任何命令，给出可读提示。
- 密钥只能走环境变量（`api_key_env` / `${env:...}`）。

## S6 · 测试 {#s6}

- 可自动化的部分：斜杠命令分发、sessions list/rm、信任确认流程（fixture 插件 + 模拟输入）、
  `run -t` 用本地假 provider（`08` 的 wiremock 手法）走通。
- 手工验证清单写进任务报告：多轮对话、Ctrl-C 一次取消两次退出、`/compact`、
  `--resume last`、approve 模式审批提示。
