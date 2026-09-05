# 架构总览

instagent 是无人值守的 headless agent。调用者一次提交完整任务，内核自主运行，
返回终态结果后退出。内核 = agent loop + provider 引擎 + 各组件的加载/执行机制；除 6 个内置工具外，
所有能力（含 provider）都从插件目录声明式加载。

## 内核（编译进二进制的核心）

| 模块 | 职责 |
|---|---|
| `src/agent/` | agent loop：`assemble` / `run_turn` / 流式输出、压缩（`compact.rs`）、hooks 触发点 |
| `src/provider/` | provider **引擎**层：`openai.rs` / `proxy.rs` 两种引擎 + 共享 SSE 流驱动（`shared.rs`）+ `registry.rs` |
| `src/tools/` | `ToolSource` trait + Registry；唯一内置内容 = 6 个工具 `shell` `read` `write` `edit` `tree` `read_image`（`builtin/`） |
| `src/plugin/` | 插件加载器：manifest 校验、五层发现（`--plugin` > 配置 `plugins` 额外路径 > 项目 > 用户 > bundled，同名高优先级覆盖）、install / enable |
| `src/hooks.rs` | hooks 运行时——内核只提供执行机制，脚本来自插件 |
| `src/commands.rs` | 任务模板加载与参数展开（`dev.instagent/commands/*.md`，按 `plugin:name` 选择） |
| `src/session.rs` `src/message.rs` | 会话 JSONL 持久化、消息模型 |
| `src/config.rs` `src/settings.rs` | 配置（yaml）、插件启用/禁用状态（settings 三层合并） |
| `src/cli/` `src/subprocess.rs` | 批处理 CLI、结果输出、取消与超时、子进程管理（进程组 + kill_on_drop） |

## 执行生命周期

1. `run` 校验 `--task` / `--task-file` / `--command` 唯一输入，加载预先配置的
   provider、model、插件与 settings。任务期限同时约束初始化和执行。
2. 创建会话，或通过 `--resume ID|last` 恢复并追加本次任务；触发生命周期 hooks。
3. agent loop 调用 provider、执行工具、按需加载 skills、自动压缩历史。系统提示明确
   无人值守约束；必要条件不足时在结果中说明，执行过程不等待用户回答或审批。
4. 完成、失败、轮数耗尽、超时或收到取消信号后保存会话、清理进程组并退出。
   默认输出模型文本流；JSON 模式从会话数据提取本次最终助手文本，输出一个终态文档，
   诊断始终写 stderr。状态和退出码见 [ADR 0004](adr/0004-headless-agent.md)。

会话是可恢复的执行记录，每次调用都必须带新任务。调度、重试、业务验收和 sandbox
生命周期由调用方负责；插件 Stop hook 可以执行确定性验收。运行期间不自动更新插件，
部署方显式调用 `plugin update`。
所选 provider、model 和所需密钥缺失会失败；可选 MCP 等组件继续沿用诊断后跳过的
降级策略。业务必需能力由调用方或 Stop hook 验证，运行不等待交互修复。

## 插件形式提供

- `bundled/` 是编译时内嵌的一个插件（`plugin.json` name=`bundled`），只含
  `dev.instagent/providers/` 下 **5 个 provider 定义 JSON**：openai / ollama /
  groq / deepseek / openrouter。引擎在内核，"哪个引擎 +
  base_url + key 环境变量 + 模型列表"在插件里。首次运行时物化为
  `<data>/bundled/v1-<fnv1a64>/` 完整不可变快照（身份 = 全部内嵌文件
  "路径+内容"的 FNV-1a 64；命中复用零重写，损坏整体替换不原地覆盖在读
  目录；`bundled/` 为缓存父目录）。
- 其余组件类型内核只有运行时，内容全部来自外部插件：
  - MCP server：插件根 `mcp.json`
  - skills：`skills/<name>/SKILL.md`（经 `load_skill` 工具暴露）
  - hooks：`dev.instagent/hooks.json` + 脚本
  - command tools：`dev.instagent/tools/*.json`
  - 任务模板：`dev.instagent/commands/*.md`（`run --command plugin:name --args ...`）

## 设计依据

- 当前定位：[`ADR 0004：Headless agent`](adr/0004-headless-agent.md)
- 运行时边界：[`ADR 0003`](adr/0003-repo-boundaries-and-runtime-policies.md)
- 历史背景：[`goose-plugin-core-plan.md`](goose-plugin-core-plan.md)、[`goose-from-scratch-plan.md`](goose-from-scratch-plan.md)
- 使用说明：[`usage.md`](usage.md)
