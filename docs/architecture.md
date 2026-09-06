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
4. 执行过程中按预算提交会话；完成、失败、轮数耗尽、超时或收到取消信号后
   清理进程组并退出。保存失败会报错，被拒绝的内容不算已持久化。
   默认输出模型文本流；JSON 模式从会话数据提取本次最终助手文本，输出一个终态文档，
   诊断始终写 stderr。状态和退出码见 [ADR 0004](adr/0004-headless-agent.md)。

会话是可恢复的执行记录，每次调用都必须带新任务。调度、重试、业务验收和 sandbox
生命周期由调用方负责；插件 Stop hook 可以执行确定性验收。运行期间不自动更新插件，
部署方显式调用 `plugin update`。
所选 provider、model 和所需密钥缺失会失败；可选 MCP 等组件继续沿用诊断后跳过的
降级策略。业务必需能力由调用方或 Stop hook 验证，运行不等待交互修复。

## 完整性与资源边界

- agent 在 PreToolUse、ToolStart 和工具执行之前校验助手消息及响应完成状态。
  长度截断、未知终止原因和不完整工具块整批拒绝执行；可校验的消息与配对错误结果
  一起提交后报失败，不能继续生成正常答案掩盖异常。正常工具响应兼容 ToolUse/EndTurn，
  单个损坏的 JSON 参数仍按工具错误处理。取消独立返回取消终态。
- 压缩只接受非空、以唯一 Done/EndTurn 正常结束且没有工具事件的摘要；
  错误或取消不替换压缩前历史。末尾未回答 user 的全部 Text/Image 块按序保留，
  包括 resume 合入的新任务；含 ToolResult 的尾消息仍与工具调用一起摘要。
  历史图片送摘要器时只用类型/大小占位说明，不嵌入 base64。
- 会话创建、追加与重写共用恢复预算：header 64 KiB、单消息行 96 MiB、总文件
  256 MiB，按序列化 JSON 字节计数（总量含换行）。超限不提交、不裁剪，保留内存、
  主文件及可用备份。会话仍假设单进程独占；预算拒绝不回滚工具已经产生的外部副作用，
  也不保证跨进程事务或掉电原子性。
- 文件 read/edit 输入与 write/edit 结果各最多 32 MiB；reader 层限制实际字节数。
  Unix 原子替换先排他创建 0600 临时文件，写完后设置普通 rwx 权限再发布，
  覆盖保留普通权限、新文件遵循 umask，特殊位不继承。ACL、扩展属性、属主、
  硬链接和全路径隔离不在保证内；读取中增长的窗口截断语义见[使用说明 §5.1](usage.md#51-文件工具与图片预算)。
- manifest、MCP 配置、provider JSON、hooks JSON、安装元数据各限 1 MiB，
  metadata 预检加实际有界读取；各组件保留原有加载失败/跳过策略。
  hooks 默认 fail-open 仅用于脚本运行，不适用于文件加载错误。
  本地安装在创建/清理 staging 前拒绝源与 staging 重叠，普通已安装目录重装仍兼容。
- CLI 模板文件限 256 KiB，展开结果限 1 MiB；`expand_bounded` 在分配展开结果前
  受检计算字节数，CLI 使用此入口，原 `expand` 库接口保留兼容。
  provider 显式声明的 `api_key_env` 缺失、不可读、为空或纯空白时构造失败；
  未声明时支持 keyless，合法值不改写。
- 工具图片先校验再原子预留会话 64 MiB 解码字节预算；非法图片不挤占后续额度。
  字节/格式校验不等于完整图像解码，MCP 外层预算也不保证依赖内部全部载荷有界。

完整用户契约见[使用说明](usage.md)。默认 Rust 回归使用本地假 provider，
10 个真实模型用例显式 ignored；CLI/live 的 liveplug 各使用仅含版本化输入的
临时副本，源码夹具只读。执行数和在线状态见[校验记录](release.md)。

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
