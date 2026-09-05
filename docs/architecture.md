# 架构总览

内核 = agent loop + provider 引擎 + 各组件的加载/执行机制；除 6 个内置工具外，
所有能力（含 provider）都从插件目录声明式加载。

## 内核（编译进二进制的核心）

| 模块 | 职责 |
|---|---|
| `src/agent/` | agent loop：`assemble` / `run_turn` / 流式输出、压缩（`compact.rs`）、hooks 触发点 |
| `src/provider/` | provider **引擎**层：`openai.rs` / `proxy.rs` 两种引擎 + 共享 SSE 流驱动（`shared.rs`）+ `registry.rs` |
| `src/tools/` | `ToolSource` trait + Registry；唯一内置内容 = 6 个工具 `shell` `read` `write` `edit` `tree` `read_image`（`builtin/`） |
| `src/plugin/` | 插件加载器：manifest 校验、五层发现（`--plugin` > 配置 `plugins` 额外路径 > 项目 > 用户 > bundled，同名高优先级覆盖）、install / enable |
| `src/hooks.rs` | hooks 运行时——内核只提供执行机制，脚本来自插件 |
| `src/commands.rs` | 斜杠命令加载器（`dev.instagent/commands/*.md`） |
| `src/session.rs` `src/message.rs` | 会话 JSONL 持久化、消息模型 |
| `src/config.rs` `src/settings.rs` | 配置（yaml）、插件启用/禁用状态（settings 三层合并） |
| `src/cli/` `src/subprocess.rs` | REPL / CLI、子进程管理（进程组 + kill_on_drop） |

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
  - 斜杠命令：`dev.instagent/commands/*.md`

## 设计依据

- 主依据：[`goose-plugin-core-plan.md`](goose-plugin-core-plan.md)（第三版）
- 补充：[`goose-from-scratch-plan.md`](goose-from-scratch-plan.md)（第二版）
- 使用说明：[`usage.md`](usage.md)
