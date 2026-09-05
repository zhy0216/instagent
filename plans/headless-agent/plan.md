# Headless agent 定位迁移

## 意图

instagent 是由脚本、CI、调度器或其他程序调用的无人值守 agent。每次调用接收完整任务，使用插件扩展能力，自主执行并返回可判定结果；运行过程中不向用户提问，不等待审批，不启动 REPL。此次迁移同时落实实现、回归测试与现行文档。

## 目标与非目标

- 唯一任务入口为 `run`，移除 `chat`、行编辑器、交互历史和斜杠命令分发。
- 输入由 `--task`、`--task-file` 或 `--command plugin:name --args` 三选一提供；文件必须是普通 UTF-8 文件，不隐式读取 stdin（`-` 无特殊含义，仅可作为普通文件名）。
- `run --resume ID|last` 明确追加任务并恢复历史；不创建空闲聊天会话。
- 提供 `--output text|json`（默认 text），JSON 为一个终态文档，包含 schema_version、status、session_id、output、usage、error；仅 completed 从本次最终持久化助手消息提取 output/usage，其他状态 output 为空且 usage 为 null，部分工作通过 session_id 查看。诊断走 stderr。状态与退出码一致。
- 完成退出 0，运行失败（包括输入内容/文件错误）1，命令行语法或参数组合错误 2，max_turns 3，超时 124，取消 130。完成指执行正常结束，不代表外部业务验收通过；业务验收可用 Stop hook。
- `--timeout` 为正整数秒，默认 600，上限 604800；执行与初始化可被 SIGINT/SIGTERM 或期限终止，清理有界；取消保持会话工具结果配对。MCP 初始化和已连接服务都持有进程组守卫；有限时工具结束时回收仍存活的同组后台子进程。
- 插件继续提供 provider、MCP、command tools、skills、hooks 和任务模板。`dev.instagent/commands/*.md` 文件格式保留，改作非交互模板，按 plugin:name 消歧。
- 插件配置和密钥在执行前给定；必要的 provider 配置缺失即错误。可选 MCP 服务保留失败降级及可见诊断；业务所需工具可由调用者或 Stop hook 验收。run 不自动更新插件，部署方通过 plugin update 显式更新。
- 系统提示明确无用户交互：根据任务和环境做合理假设，缺少必要条件时说明无法完成，不声称执行成功；遵循任务要求的输出格式。
- 不新增 HTTP 服务、调度器、UI、审批系统或插件 ABI；保留现有 Rust 库模块树、依赖版本、会话格式与 sandbox 隔离边界。

## 方案与任务顺序

1. 确立本方案及当前任务文件范围。
2. 并行调整内核/模板、CLI、回归测试与文档。CLI 保留现有装配和工具机制；JSON 结果直接由会话数据提取，不依赖可能丢弃的展示事件。
3. 删除 REPL 代码及对应行编辑器依赖（只移除，不增加或升级依赖）；迁移交互测试到批处理恢复、取消、压缩、插件模板测试。
4. 运行完整校验，审计所有用户入口与当前文档中的旧定位。旧设计明确标记为历史，归档 todos 不修改。

## 校验

`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全部通过。离线假 provider 的 CLI 集成测试验证：无需 stdin、输入冲突/空白/文件错误、模板执行、恢复会话、结构化成功/失败/max_turns/取消/超时、进程组清理以及插件能力继续可用。真实 provider 依赖凭据，不作为无人值守定位的必要门禁。

## 风险与假设

移除 chat 是有意的破坏性变更。保留模板目录以兼容已有插件内容，不保留交互入口。超时不强制中断同步本地文件系统调用；外部 sandbox 仍负责强隔离和资源上限。模型文本本身无法证明业务成功；运行结果 status 只描述运行生命周期，确定性验收由调用者或 Stop hook 提供。

## 实施验收（2026-09-05）

| 要求 | 当前证据 |
|---|---|
| 无交互入口与输入 | `src/cli/mod.rs` 仅 run/sessions/plugin；REPL 与 rustyline 已移除；CLI 输入冲突、空白/文件/FIFO、保持 stdin 打开的测试通过 |
| 自主执行与真实终态 | 系统提示、空终答/截断/异常停止/Stop hook 超限测试通过；JSON 从本次完成消息提取，失败恢复不返回历史答案 |
| 会话恢复 | CLI 测试验证 ID/last、保存的 provider/model/cwd、拒绝冲突 cwd、取消及压缩后的继续执行 |
| 插件保留 | CLI 模板、provider、hooks、skills 及 MCP/proxy/tool inventory 测试通过；run 不推进插件自动更新元数据 |
| 取消与资源回收 | SIGINT/SIGTERM/timeout、SessionEnd 取消、MCP 正在初始化及前一服务已启动时取消的回归通过；子进程父进程正常退出后也回收后台孙进程 |
| 当前设计一致 | README、usage、architecture、AGENTS、任务索引及 ADR 0004 已同步；旧设计标记取代；todos/done 未改动 |

校验结果：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 通过。
`env -u TOKEN_PLAN_API_KEY cargo test` 通过：586 项离线测试实际执行通过；
另 10 项 live 用例按既有凭据门控跳过（测试框架将提前返回显示为通过）。
离线构成：lib 463、CLI 单测 10、agent continuation 21、CLI 集成 49、MCP 15、
proxy 15、session recovery 9、tool inventory 4。

另行运行了带现有凭据的真实 provider 套件，10 项均在 180 秒超时，未验证远端链路成功。
该可选外部套件的结果没有计入离线验收通过数，也未改弱断言。无人值守契约由离线假
provider 的完整进程、插件、持久化与输出测试验证；不据此声称远端模型服务可用。
