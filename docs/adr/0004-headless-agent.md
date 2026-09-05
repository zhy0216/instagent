# ADR 0004: 以插件为核心的 Headless agent

- 状态：已接受
- 日期：2026-09-05
- 依据：项目重新定位为无需用户交互的 headless agent，同时保留 plugin system。
- 实施方案：[plans/headless-agent/plan.md](../../plans/headless-agent/plan.md)
- 任务：[todos/20-headless-agent.md](../../todos/20-headless-agent.md)
- 替代：历史设计中的交互 CLI、斜杠命令分发、启动自动更新；扩展 ADR 0003 D4。

## 定位与边界

instagent 由脚本、CI、调度器或其他程序提交完整任务，在 sandbox 中自主执行，
返回可判定的终态结果后退出。运行中不向用户提问、不请求审批，也不启动 REPL。
系统提示明确要求根据任务与环境作合理假设，必要条件不足时说明无法完成，
遵循调用方要求的输出格式，不将未完成工作表述为成功。

内核仍负责消息、会话、agent loop、自动压缩、provider 引擎和插件运行时。
provider、MCP、command tools、skills、hooks 和任务模板继续由插件提供。
本次不引入 HTTP 服务、调度器、UI、审批机制或新插件 ABI。安全隔离仍由外部 sandbox
负责，ADR 0002 与 ADR 0003 的密钥、环境、settings、文件边界策略继续适用。

## 任务输入与恢复

唯一任务入口为 `run`。以下三种输入必须且只能选择一种：

- `--task TEXT`（`-t`）：直接提供完整任务。
- `--task-file PATH`：读取普通 UTF-8 文件；不隐式读取 stdin，`-` 没有 stdin 特殊含义。
- `--command plugin:name [--args TEXT]`：将插件模板展开成任务。

空白任务、无效文件、未知模板均明确失败。CLI 参数语法或组合错误在运行前报告，
退出码为 2；参数解析后的读取、配置、模板或初始化错误归入运行失败。
`--task` 文本和 `--task-file` 内容上限均为 1 MiB。
移除 `chat`、行编辑器、输入历史和交互命令分发。

每次 `run` 默认创建会话；`--resume ID|last` 明确恢复历史并追加本次任务，仍要求提供
新任务。恢复使用会话保存的 cwd、provider、model，模型可由 `--model` 显式覆盖；
不同的显式 `--cwd` 报错。不存在可恢复会话时失败，不创建空闲会话。
会话 JSONL 格式与单进程独占约定保留；取消或失败后保持工具调用与结果配对以便恢复。

## 结果与退出码

默认 `--output text` 向 stdout 流式输出模型文本，可能含中间说明和失败前的部分输出。
工具事件、usage、会话 id、日志和诊断走 stderr。`--output json` 输出一个终态文档：

```json
{
  "schema_version": 1,
  "status": "completed",
  "session_id": "...",
  "output": "本次最终助手文本",
  "usage": { "input": 100, "output": 20, "cache_read": 0, "cache_write": 0 },
  "error": null
}
```

- `session_id` 在会话创建或恢复之前失败时为 `null`。
- 只有 `completed` 提供 `output` 和 `usage`：文本取本次执行最终助手消息原文，
  不附加渲染换行；usage 为最近记录的助手响应用量，没有记录则为 `null`，并非总计费。
- 其他状态的 `output` 固定为空串，`usage` 为 `null`，`error` 为原因字符串；
  已持久化的中间工作通过会话记录读取。恢复任务不得返回前一任务的答案。
- JSON 从会话数据提取，不以可能丢弃的展示事件作为结果源。
- 参数解析错误退出 2，只输出 stderr 诊断；不生成运行结果文档。
- text 渲染或关闭 stdout 失败不改变任务状态，沿用 ADR 0003 的输出错误策略。
  JSON 写入或 flush 失败时退出 1，并在 stderr 报错，避免将未交付结果视为正常返回。

| 状态 | 退出码 | 含义 |
|---|---|---|
| `completed` | 0 | agent loop 正常完成 |
| `failed` | 1 | 输入读取、配置、初始化或执行失败 |
| 无运行结果 | 2 | 命令行参数解析错误 |
| `max_turns` | 3 | 配置的循环轮数耗尽 |
| `timed_out` | 124 | 达到本次执行期限 |
| `cancelled` | 130 | 收到 SIGINT / SIGTERM |

`completed` 只证明执行流程正常结束，不证明外部业务验收通过。调用方检查业务结果，
也可以通过插件 Stop hook 执行确定性验收。provider 截断、非正常结束以及 Stop hook
连续阻止达到上限不能被当成成功完成。

## 期限与取消

`--timeout` 接受 1–604800 秒（最多 7 天），默认 600，从初始化开始约束整个任务。SIGINT 和 SIGTERM
采用同一取消路径；不保留“第一次取消轮次，第二次退出聊天”的交互语义。
终止时停止 provider 和工具执行，保存配对的工具结果，尝试 SessionEnd hook 与资源清理。
清理额外最多 5 秒，子进程保持进程组管理和 `kill_on_drop(true)`。
MCP transport 在握手、服务运行与关闭期间持有进程组守卫，取消会连同孙进程回收；
有限时工具、hooks 的直接进程结束时也回收该组，后台子进程不能脱离本次调用继续运行。
同步本地文件系统操作不承诺可被强制中断；硬资源限制由 sandbox 提供。

## 插件契约

插件发现、启用状态、provider 引擎、工具和 hooks 的机制保留。配置与密钥在任务前
准备好；所选 provider、model 或所需密钥缺失直接失败，运行中没有配置向导。
可选组件保留既有降级行为：无效 MCP 配置、连接失败或无效工具定义可诊断后跳过，
继续使用剩余能力，不等待用户补配置。业务必需的工具能力由调用方预检或 Stop hook
验收，`completed` 不表示所有可选插件组件都已加载成功。工具调用依然直接执行，
PreToolUse / Stop hooks 的阻止是预配置策略，不等待用户审批。

`dev.instagent/commands/*.md` 继续使用已有 Markdown/frontmatter 格式，现作为任务模板。
调用者用 `run --command 插件名:模板名 --args "参数"` 选择模板；插件命名空间消除同名
冲突。`$ARGUMENTS` 替换为参数文本，没有占位符时将非空参数追加到正文。
模板不会通过 shell 解释参数。

`run` 不自动拉取或更新插件；部署方显式执行 `plugin update [name]`。既有
`--auto-update` 安装选项和元数据为格式兼容保留，不触发任务运行期间更新。
插件内容与版本由部署方管理，任务执行不兼任部署步骤。

## 迁移与验收

- `chat` / `/review 参数` 分别迁移为 `run -t "任务"` /
  `run --command 插件名:review --args "参数"`；后续任务使用 `run --resume`。
- Rust 库模块树、直接依赖版本、会话格式和插件目录格式保留；移除行编辑器依赖。
- 当前 README、使用说明、架构、AGENTS 与任务队列以本 ADR 为准；历史设计加取代说明，
  `todos/done/` 保持归档原文。
- 离线 CLI 回归验证输入、终态 JSON、状态/退出码、恢复、模板、取消、超时、进程清理与
  插件能力；`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、
  `cargo test` 是完整验收门禁。真实 provider 连接凭据由部署方提供。
