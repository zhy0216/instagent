# instagent 使用说明

instagent 是一个**以插件为核心的 headless agent**，接收脚本、CI、调度器或其他程序
提交的完整任务，无人值守地执行并返回终态结果。内核提供 6 个内置工具
（`shell` `read` `write` `edit` `tree` `read_image`）和一个 agent loop；provider、MCP
server、skills、hooks、任务模板、command tools 全部以插件形式加载——连内置的
5 个 provider 定义（openai / ollama / groq / deepseek / openrouter）
也来自一个随二进制分发的 bundled 插件。

插件格式遵循 [Agent Plugins v1.0.0](https://agent-plugins.org/specification)，
hooks 的载荷与决策协议与 [block/goose](https://github.com/block/goose) 兼容。

instagent 运行在 sandbox 内，工具直接执行，安全边界由 sandbox 承担
（见 [docs/adr/0002](adr/0002-sandbox-agent-no-ui-permission.md)）。
任务运行中不向用户提问、不等待审批，所选 provider、model 或所需密钥缺失直接报错。
定位与生命周期契约见 [ADR 0004](adr/0004-headless-agent.md)。

---

## 目录

1. [安装构建](#1-安装构建)
2. [快速上手](#2-快速上手)
3. [命令参考](#3-命令参考)
4. [配置](#4-配置)
5. [无人值守执行](#5-无人值守执行)
6. [会话管理](#6-会话管理)
7. [插件管理](#7-插件管理)
8. [插件开发](#8-插件开发)
9. [Provider 详解](#9-provider-详解)
10. [文件位置速查](#10-文件位置速查)
11. [故障排查](#11-故障排查)

---

## 1. 安装构建

```bash
cargo build --release                        # 产物 target/release/instagent
cargo install --path . --bin instagent       # 装到 ~/.cargo/bin
```

工具链版本要求见 `rust-toolchain.toml`。

---

## 2. 快速上手

**第一步：写配置。** 创建 `~/.config/instagent/config.yaml`（目录自动生效，
文件需要自己建）：

```yaml
provider: groq                 # bundled 插件里的 provider 名字
model: llama-3.3-70b-versatile
```

密钥不写进 config.yaml（ADR 0003 D1）：唯一来源是 provider JSON
`api_key_env` 指定的环境变量（如 groq 的 `GROQ_API_KEY`），见第二步。

**第二步：导出密钥并启动：**

```bash
export GROQ_API_KEY=...
instagent run -t "列出当前目录的文件并说明用途"
instagent run --task-file ./task.md --output json > result.json
```

不想碰真实配置可以先用沙箱目录试：

```bash
export INSTAGENT_CONFIG_DIR=$(mktemp -d)
export INSTAGENT_DATA_DIR=$(mktemp -d)
```

本地无密钥可用 ollama 试跑（provider `ollama` 指向
`http://localhost:11434/v1`，无需密钥）：

```yaml
provider: ollama
model: qwen2.5-coder
```

---

## 3. 命令参考

顶层命令：

| 命令 | 说明 |
|---|---|
| `instagent run -t "任务"` | 执行任务，返回结果后退出 |
| `instagent sessions <list\|rm>` | 会话管理 |
| `instagent plugin <子命令>` | 插件管理 |

### `instagent run`

```bash
instagent run --task "任务" [选项]
instagent run --task-file ./task.md [选项]
instagent run --command my-plugin:review --args "当前 diff" [选项]
```

三个任务来源必须且只能指定一个：`-t, --task` 直接传文本，`--task-file` 读取
普通 UTF-8 文件，`--command` 展开已启用插件的任务模板（§8.7）。文件路径相对于
调用时的目录解析；空白任务、不可读文件、目录和非 UTF-8 内容会报错。
`--task` 文本、`--task-file` 内容和 `--command` 展开结果均最多 1 MiB
（1,048,576 个 UTF-8 字节，恰好上限可用）。模板展开前检查完整结果大小；
超限属于 `failed` / 退出码 `1`，不裁剪任务、不发起模型请求。
新任务此时尚未创建会话，JSON 的 `session_id` 为 `null`；已恢复会话则保留其 id。
不隐式读取 stdin，也不支持用 `--task-file -` 表示 stdin。

| 选项 | 说明 |
|---|---|
| `--resume <id\|last>` | 恢复指定或最近会话，并追加本次任务；仍必须指定任务来源 |
| `--cwd PATH` | 工作目录（不存在会创建），影响工具相对路径与项目级 settings/skills 发现；恢复时须与原目录一致 |
| `-m, --model MODEL` | 覆盖配置里的 `model` |
| `--plugin PATH` | 临时加载一个插件目录（可多次），不安装、不落盘，开发调试用 |
| `--args TEXT` | 模板参数，仅与 `--command plugin:name` 配合使用；省略为空串 |
| `--output text\|json` | 默认 `text`；`json` 输出一个终态文档 |
| `--timeout SECONDS` | `1`–`604800` 秒（最多 7 天），默认 `600`；覆盖初始化和任务执行，清理额外最多 5 秒 |

**文本输出**：默认 stdout 流式输出模型文本，可能包含工具调用前的说明或失败前的
部分输出。工具事件、预览、`usage:`、`session <id>`、装配提示与 tracing 日志都走
stderr。stdout 提前关闭（例如 EPIPE）不改变任务退出码。

**JSON 输出**：`--output json` 的 stdout 只写一个 JSON 文档，不混入文本流或日志：

```json
{
  "schema_version": 1,
  "status": "completed",
  "session_id": "...",
  "output": "检查完成，测试通过。",
  "usage": { "input": 120, "output": 30, "cache_read": 0, "cache_write": 0 },
  "error": null
}
```

| 字段 | 含义 |
|---|---|
| `schema_version` | 当前为 `1` |
| `status` | `completed` / `failed` / `max_turns` / `timed_out` / `cancelled` |
| `session_id` | 会话 id；会话创建或恢复前失败时为 `null` |
| `output` | 仅 `completed` 填入本次执行最终助手消息的文本，保留原文且不额外添加换行；其他状态为空串 |
| `usage` | 仅 `completed` 提供最近记录的助手响应用量；未提供用量或其他状态为 `null`，不是本次任务的累计计费 |
| `error` | `completed` 为 `null`；其他状态为错误原因字符串 |

JSON 结果从会话数据提取，不依赖展示事件。恢复历史不会把上一任务的答案作为
本次结果；非完成状态的中间工作可通过 `session_id` 对应的会话记录查看。
JSON 写入或 flush 失败时，stderr 报错并退出 `1`，因为调用方未收到完整结果。

| 情况 | JSON `status` | 退出码 |
|---|---|---|
| 执行正常结束 | `completed` | `0` |
| 配置、provider、工具流程或执行失败 | `failed` | `1` |
| 命令行参数解析错误 | 无文档，错误写 stderr | `2` |
| 达到 `max_turns` | `max_turns` | `3` |
| 达到执行期限 | `timed_out` | `124` |
| 收到 SIGINT / SIGTERM | `cancelled` | `130` |

`completed` 表示执行生命周期正常结束，不证明外部业务验收通过；调用方应检查
结果，或用插件 Stop hook 实施确定性验收。provider 截断、未正常结束的响应、
Stop hook 连续阻止超过上限均不能当作完成。
参数解析错误（例如输入来源冲突、缺少任务、非法 `--timeout`）由 CLI 在运行前报告；
参数解析后的输入读取、模板查找或配置错误属于运行失败。

### `instagent sessions`

```bash
instagent sessions list        # 列出全部会话（编号、id、时间、provider/model、cwd）
instagent sessions rm <id>     # 删除会话文件
```

### `instagent plugin`

```bash
instagent plugin install <git-url 或本地路径>
instagent plugin list
instagent plugin show <name>
instagent plugin enable <name>
instagent plugin disable <name>
instagent plugin update [name]   # 不带 name = 更新全部 git 来源插件
```

详见 §7。

---

## 4. 配置

### 4.1 `config.yaml`（用户级）

路径 `~/.config/instagent/config.yaml`。**只有用户级一层**（项目级覆盖走
settings，见 §4.3）。所有字段可缺省，缺省取默认值：

| 字段 | 默认 | 说明 |
|---|---|---|
| `provider` | 无 | provider 名字，定义来自插件（§9）；重名时写 `插件名/provider名` |
| `model` | 无 | 模型名 |
| `max_tokens` | `8192` | 单次回复最大 token |
| `max_turns` | `1000` | 单次任务最多循环多少轮；耗尽以 `max_turns` / 退出码 `3` 结束 |
| `context_limit` | 无 | 覆盖模型上下文上限（默认按 §9 的四级推导） |
| `compaction_threshold` | `0.8` | token 用量占上下文比例超过该值时自动压缩会话 |
| `shell` | `$SHELL` | `shell` 工具使用的解释器 |
| `plugins` | `[]` | 额外插件搜索目录（支持 `~`；相对路径按启动时 cwd 解析） |

config.yaml 不含任何密钥字段（ADR 0003 D1）：密钥唯一来源是 provider JSON
`api_key_env` 指定的环境变量；写了旧 `api_key` / `api_key_env` 键的文件在
加载期直接报错并给迁移提示。

示例：

```yaml
provider: openai
model: gpt-5
max_tokens: 4096
compaction_threshold: 0.65
shell: /bin/zsh
plugins:
  - ~/my-plugins
  - ./project-plugins
```

### 4.2 环境变量

优先级高于 config.yaml：

| 变量 | 覆盖 |
|---|---|
| `INSTAGENT_PROVIDER` | `provider` |
| `INSTAGENT_MODEL` | `model` |

沙箱/测试用（重定向目录）：

| 变量 | 默认 |
|---|---|
| `INSTAGENT_CONFIG_DIR` | `~/.config/instagent` |
| `INSTAGENT_DATA_DIR` | `~/.local/share/instagent` |
| `INSTAGENT_AGENTS_DIR` | `~/.agents` |

日志：默认 warning 到 stderr（健康路径保持安静，stdout 为文本或 JSON 结果）；
显式 `RUST_LOG` 优先（如 `RUST_LOG=info instagent run -t "检查项目"` 看详细日志）。

### 4.3 `settings.json`（三层）

控制插件的启用/禁用，按 **local > project > user** 合并：

| 层 | 路径 |
|---|---|
| User | `~/.config/instagent/settings.json` |
| Project | `<项目>/.config/instagent/settings.json` |
| Local | `<项目>/.config/instagent/settings.local.json`（可能含 secrets，请加入项目 gitignore；instagent 仓库自身已忽略该路径） |

```json
{
  "enabledPlugins": ["my-plugin"],
  "disabledPlugins": ["bundled"]
}
```

- `enabledPlugins` 三态（ADR 0003 D5）：**缺失** = 不表态（"除
  `disabledPlugins` 外全部启用"）；**非空** = 白名单，只有列出的启用；
  **显式 `[]`** = 空白名单终值，禁用全部，低层不得恢复任何名字。
  `enabledPlugins: []` 后 `plugin enable <name>` 会把该名字写回白名单；
  禁用白名单最后一项仍保持白名单模式，不会回退成"全部启用"。
- `disabledPlugins`：黑名单，各层取并集；缺失与 `[]` 等价。
- 同名插件的 enabled/disabled 字段：以最高层出现的为准；某层缺省的字段
  不参与覆盖（"没写"与"写了空数组"是两种语义，见上）。

---

## 5. 无人值守执行

任务应包含目标、可用输入、约束、验收条件和期望的输出格式。例如：

```bash
instagent run --task-file ./review-task.md --timeout 300 --output json > result.json
```

provider、model、凭据和插件应在调用前准备好。系统提示要求 agent 根据任务和
环境作合理假设，自主使用工具；缺少必要条件时说明无法完成，不把未完成工作报告为成功。
模型不会获得用于请求用户输入或审批的交互入口。任务可以要求它把业务结果写成 JSON，
这段文本会放在结果文档的 `output` 字符串中。

`--timeout` 从运行初始化开始计时；SIGINT（终端 Ctrl-C）或 SIGTERM 都取消本次任务。
取消和超时会停止 provider/工具执行，配齐会话中的工具结果，并尝试触发 SessionEnd、
关闭插件子进程，清理额外最多 5 秒。同步本地文件系统调用不承诺被强制中断；
部署方仍应在 sandbox 层设置资源和进程生命周期上限。

`chat`、REPL、斜杠命令分发和输入历史已移除。原 `/review 参数` 改成
`run --command 插件名:review --args "参数"`；需要继续任务时重新调用 `run --resume`。

### 5.1 文件工具与图片预算

- `read` / `edit` 的输入文件最多 32 MiB（33,554,432 字节），`write` 内容和
  `edit` 替换后的完整文件也最多 32 MiB，均包含恰好上限。write/edit 超限返回
  工具错误，不发布新内容，旧文件字节和普通权限保持不变。
- `read` 的 `line` 从 1 开始，默认最多显示 2000 行。预算按实际读取字节计算，
  包含换行，长行也受限；不是只限制显示窗口。metadata 预检已超限时直接报错；
  读取中增长越界时最多读取预算加 1 个探测字节，返回预算内的窗口内容，并提示
  `stopped: file grew past ...`、总行数不完整，不把结果当作完整文件。
  `edit` 的整读增长越界则报工具错误，不写回。
- Unix 上覆盖已有普通文件的 `write` / `edit` 保留普通 rwx 权限（如 0755、0600），
  不继承 setuid/setgid/sticky 位；新文件遵循 `0666 & umask`。同目录临时文件
  排他创建，从 0600 开始写入，完成后设置最终权限再 rename，失败清理本次临时文件。
  这不保证保留 ACL、扩展属性、属主或硬链接语义。
- 写入拒绝最终目标为符号链接；中间目录链接和绝对路径仍按现有规则解析。
  完整路径隔离由 sandbox 负责，同步本地 IO 的强制取消仍无保证。
- 工具返回的图片先校验，再原子预留会话的 64 MiB 图片预算（按 base64 解码后
  字节计）。非法图片不占用后续调用额度；超预算图片不附入历史，工具结果带提示。
  这是格式/字节校验，不能当作完整图像解码或尺寸验证。

---

## 6. 会话管理

- 会话以 JSONL 存在 `~/.local/share/instagent/sessions/<id>.jsonl`，
  首行是 header（id、时间、provider、model、cwd）。
- `instagent run --resume last -t "后续任务"` 恢复最近会话；`--resume <id>` 恢复
  指定会话（id 见 `instagent sessions list`）。没有可恢复会话时报错。
- 恢复时使用会话记录的 cwd、provider、model；`--model` 可以显式覆盖模型，
  `--cwd` 若与原会话目录不同则报错。
- **单进程独占**：不要对同一会话 id 同时开两个 `--resume`，两进程追加写
  会互相漂移。
- **续轮语义**：历史末尾为 user（摘要、未回答输入或 tool results）时，
  新输入合并追加到该消息并原子重写落盘；末尾为 assistant 才追加新 user
  消息。自动压缩、取消或失败后恢复都保持该不变量。
- **读写共用预算**：header 64 KiB、单消息行 96 MiB、总文件 256 MiB。
  按序列化后的 JSON UTF-8 字节计数，包含转义开销；行预算不含换行，总预算包含
  换行和已有文件的实际长度。创建、追加批次和重写都必须符合恢复预算。
- **写入拒绝**：预算超限或计数溢出时不提交该次写入，内存历史、原主文件和可用
  备份保持原状，不自动裁剪历史或提高限额。执行因此失败；可恢复的是此前已提交的
  记录，被拒绝的这批内容没有持久化。工具已执行的文件修改、命令等外部副作用
  **不会随会话写入失败回滚**，重试前由调用方核对外部结果。
- 上下文过长时按 `compaction_threshold` 自动压缩，诊断写 stderr。摘要必须是
  非空文本并以 `EndTurn` 正常结束；长度截断、异常原因、工具事件、缺少完成事件
  或完成后仍有事件都导致本次执行失败，压缩前主文件逐字节保持不变。压缩期间取消
  不替换历史，终态仍按取消/超时处理。修正条件后可用 `run --resume <id> -t ...` 继续。
- 压缩保留末尾未回答 user 的全部有序 Text/Image 块，包括恢复时追加的新任务；
  摘要放在这些块前，图片不会改成 base64 摘要文本。末尾含 ToolResult 的消息仍随
  成对历史参与摘要，不把工具结果拆成独立未回答任务。已回答历史中的图片只向
  摘要器提供类型/大小占位说明。
- 未指定 `--resume` 的 `run` 创建新会话；id 写入 stderr 和 JSON 结果，便于后续恢复。
- 每次运行触发 `SessionStart` / `SessionEnd` 生命周期 hooks；不创建等待输入的空闲会话。

---

## 7. 插件管理

### 7.1 发现来源与优先级

启动时按以下顺序发现插件（高优先级在前）：

1. `--plugin PATH` 命令行参数（临时加载，可多个）
2. 配置 `plugins:` 列出的目录
3. 项目级 `<cwd>/.agents/plugins/`
4. 用户级 `~/.agents/plugins/`
5. bundled（随二进制分发的内置插件，首次运行物化为
   `<data>/bundled/v1-<fnv1a64>/` 不可变快照：身份取全部内嵌文件
   "路径+内容"的 FNV-1a 64，只复用逐字节一致的快照；损坏整体替换，
   不原地覆盖正在读取的目录）

manifest 校验失败的目录记警告跳过，不中断启动；settings 里启用但目录已删
的插件同样只警告。同名插件先到先得。

所选 provider、model 或 provider 要求的密钥缺失时，任务以 `failed` 结束。
可选组件沿用降级策略：无效的 MCP 配置、连接失败或无效工具定义可能只输出诊断并跳过，
剩余能力继续运行。启动不会为此询问用户或等待用户补配置；调用方需要把业务必需的
工具能力纳入预检或 Stop hook 验收，不能仅凭 `completed` 推断全部插件都已可用。

### 7.2 安装 / 更新

```bash
instagent plugin install https://github.com/some/plugin-repo   # git 克隆到 ~/.agents/plugins/<name>/
instagent plugin install ./my-local-plugin                     # 本地路径：复制安装
instagent plugin update              # 更新全部 git 来源插件（git pull）
instagent plugin update my-plugin    # 只更新一个
```

`run` 只加载现有插件，不进行自动更新。部署流程在执行任务前显式运行
`plugin update`。安装参数 `--auto-update` 和已有安装元数据为兼容旧格式保留，
不会使 headless 任务启动时自动拉取代码。

本地复制前解析源路径和安装 staging 的目录关系（含既存符号链接）：源包含
staging、位于 staging 内或两者相同都会明确拒绝，避免递归复制及清理源目录。
正常从已安装的 `plugins/<name>` 目录重装仍可用；这不提供跨进程安装事务或全路径隔离。
安装元数据 `.install.json` 的读取上限为 1 MiB；超限不修改元数据或已有安装。

### 7.3 查看 / 启用 / 禁用

```bash
instagent plugin list            # 名字、版本、启用状态、来源
instagent plugin show my-plugin  # 详情：root、来源、commit
instagent plugin disable my-plugin
instagent plugin enable my-plugin
```

### 7.4 开发时临时加载

```bash
instagent run --plugin ./my-dev-plugin --command my-dev-plugin:review --args "当前 diff"
```

不安装、不进 settings，适合边写边试；该插件定义的 provider / 任务模板等
立即可用。

---

## 8. 插件开发

一个完整插件的目录结构：

```
groq-and-review/
├ plugin.json                    # 必需
├ skills/                        # 规范组件：skills
│   └ review/
│     ├ SKILL.md
│     └ references/checklist.md
├ mcp.json                       # 规范组件：MCP servers（固定在插件根）
└ dev.instagent/                 # 命名空间组件
  ├ providers/groq.json          # provider 定义
  ├ tools/weather.json           # command tool
  ├ hooks.json                   # hooks
  ├ commands/review.md           # 任务模板
  └ scripts/...
```

### 8.1 `plugin.json`

`$schema` 必填且必须是 1.0.0 的规范 URL（客户端不联网，只按它选本地校验
规则）；`name` `version` 必填：

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
  "name": "groq-and-review",
  "version": "1.0.0",
  "description": "Groq provider + a review skill + a lint hook",
  "extensions": { "dev.instagent": { "minKernel": "0.1", "env": ["MY_TOKEN"] } }
}
```

`extensions.dev.instagent.env` 声明该插件的 hooks 需要透传的环境变量名
（§8.5，白名单机制）。

### 8.2 `mcp.json`

固定在插件根目录。支持 `stdio` 和 `streamable-http` 传输；`sse` 会跳过并提示。
远程 HTTP 使用 `url`，当前不发送 manifest 的 `headers`（会输出说明）。下面是 stdio 示例：

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
  "mcpServers": {
    "everything": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-everything"],
      "env": {},
      "cwd": "${PLUGIN_ROOT}"
    }
  }
}
```

- `command`：单个可执行名，或 `./` 开头的插件相对路径。
- 变量展开：`${PLUGIN_ROOT}`（插件根）、`${PLUGIN_DATA}`（该插件的数据目录
  `~/.local/share/instagent/plugins/<name>/`）。
- MCP 工具对模型的名字是 `<server名>__<工具名>`，如 `everything__echo`。

### 8.3 `dev.instagent/providers/*.json`

见 §9.2 的完整字段表。

### 8.4 `dev.instagent/tools/*.json`（command tools）

脚本即工具。每个文件一个工具数组或单个对象均可，字段：

```json
{
  "name": "weather",
  "description": "get weather for a city",
  "input_schema": { "type": "object", "properties": { "city": { "type": "string" } } },
  "command": "${PLUGIN_ROOT}/scripts/weather.sh",
  "timeout_secs": 30,
  "read_only": true
}
```

- 模型看到的工具名是 `<插件名>__<工具名>`（如 `groq-and-review__weather`）。
- 执行：`command`（展开 `${PLUGIN_ROOT}` 后）用 `sh -c` 跑；工具入参 JSON
  写入 **stdin**，**stdout** 作为工具结果；退出码非 0 / 超时（默认 30s）/
  被取消 → 结果是 `is_error`。
- 子进程按进程组管理，超时、取消或直接进程退出时回收整组（包括后台孙进程）。
  内置 shell 和 hooks 使用相同的回收机制；需要持续提供工具的服务应由 MCP 插件管理。
- 解析失败或非法的定义跳过（warn 日志），不影响其它工具。

### 8.5 `dev.instagent/hooks.json`

格式与决策协议兼容 goose。顶层：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "shell",
        "hooks": [
          {
            "command": "${PLUGIN_ROOT}/scripts/guard.sh",
            "timeout": 10,
            "on_failure": "block"
          }
        ]
      }
    ],
    "SessionStart": [
      { "hooks": [{ "command": "echo session started" }] }
    ]
  }
}
```

**六个事件**：`SessionStart` `UserPromptSubmit` `PreToolUse` `PostToolUse`
`Stop` `SessionEnd`。只有 `PreToolUse` 和 `Stop` 能**阻止**（其余事件的
阻止决策被忽略）。未知事件名按规范忽略不报错；兼容草案位置
`hooks/hooks.json`（优先 `dev.instagent/hooks.json`）。

- `matcher`：正则，匹配目标是工具事件的工具名、`UserPromptSubmit` /
  `Stop` 的消息文本；省略则每次都跑。非法正则跳过该条并警告。
- `timeout`：秒，默认 30；超时按 `on_failure` 处理（杀进程组）。
- `on_failure`：`allow`（默认）或 `block`——脚本起不来 / 超时 / 输出无法
  解析等"没有决策"情形的兜底。

**输入**：载荷 JSON 写入脚本 stdin：

```json
{
  "event": "PreToolUse",
  "session_id": "...",
  "tool_name": "shell",
  "tool_input": { "command": "rm -rf /" },
  "tool_output": "...",
  "message": "...",
  "working_dir": "/path/to/cwd"
}
```

（字段按事件裁剪：工具事件有 `tool_name`/`tool_input`，`PostToolUse` 加
`tool_output`，`Stop` 的最后一条助手文本放 `message`。）

**决策协议**（与 goose 一致）：

| 脚本行为 | 决策 |
|---|---|
| 退出码 2 | 阻止，stderr 作为理由 |
| stdout 含 `{"decision":"block"}` | 阻止 |
| 退出码 0 且空输出 / `{"decision":"allow"}` | 放行 |
| 其余 | 按 `on_failure` |

**环境变量**：白名单透传（`PATH` `HOME` `LANG` + manifest
`extensions["dev.instagent"].env` 声明的名字）+ `PLUGIN_ROOT`。
`Stop` 连续阻止上限 8 次，防死循环。

### 8.6 `skills/`

每个含 `SKILL.md` 的一层子目录是一个 skill（不递归）：

```
skills/review/SKILL.md
skills/review/references/checklist.md
```

`SKILL.md` frontmatter（Agent Skills 规范）：`name` 必填（1~64，小写字母
数字和 `-`，必须等于目录名）、`description` 必填（≤1024 字符）；无效
skill 跳过不报错。

发现范围：各启用插件的 `skills/` + `~/.agents/skills/` +
`<项目>/.agents/skills/`。插件内 skill 名字带命名空间 `<插件名>:<skill>`，
用户/项目目录的是裸名。

运行时只有一个工具 `load_skill(name, file?)`：启动时只把
`名字 — 描述` 一行放进 system prompt，模型按需调用 `load_skill` 读正文
或 `references/` 下的文件。

### 8.7 `dev.instagent/commands/*.md`（任务模板）

文件名即模板名（`review.md` → `groq-and-review:review`）。frontmatter 取 `description` /
`argument-hint`，正文是模板：

```markdown
---
description: Review the current diff
argument-hint: [focus]
---
Review `git diff` with focus on: $ARGUMENTS. Report findings as a list.
```

```bash
instagent run --command groq-and-review:review --args "错误处理与测试覆盖"
```

- 选择器必须包含 `插件名:模板名`，多插件同名模板互不覆盖。
- `$ARGUMENTS` 展开为 `--args` 去除首尾空白后的文本，保留内部内容，
  不解释为 shell 命令；省略时为空串，所有占位符均替换。
- 模板里没有 `$ARGUMENTS` 时，非空参数以两个换行分隔追加到正文末尾。
- 模板文件最多 256 KiB；展开后的任务最多 1 MiB。展开前按占位符数量、
  参数 UTF-8 字节数和追加换行计算完整大小，越界或计数溢出直接失败，
  不分配超限展开结果、不截断；结果为空白也失败（§3）。
- 解析失败的模板文件跳过；指定不存在或禁用插件的模板时报错。
- 目录与 Markdown 格式沿用已有插件内容，模板展开后作为本次任务提交给 agent loop。

### 8.8 装上之后的完整效果

以上述 `groq-and-review` 为例，启用后：

- 模型可见工具：内置 6 个 + `everything__echo` 等（MCP）+
  `groq-and-review__weather`（command tool）+ `load_skill`（skills）；
- system prompt 多一行 `groq-and-review:review — …`（skill 索引）；
- 配置里 `provider: groq` 可用；
- `run --command groq-and-review:review --args "当前 diff"` 可用；
- 每次调 `shell` 前 `guard.sh` 先跑。

### 8.9 组件文件读取预算

下列文件各最多 1 MiB。读取同一文件句柄的 metadata 后，reader 仍最多读
上限加 1 字节并复检，文件增长不能绕过限制；超限诊断包含来源和预算，不回显正文。

| 文件 | 超限/读取失败时的行为 |
|---|---|
| `plugin.json` | 发现时警告并跳过该插件；显式安装时失败 |
| `mcp.json`（兼容 `.mcp.json`） | 诊断后跳过该插件的 MCP 配置，其余组件可继续 |
| `dev.instagent/providers/*.json` | 注册表加载失败，任务初始化失败 |
| `dev.instagent/hooks.json`（兼容 `hooks/hooks.json`） | hooks 加载失败，任务初始化失败 |
| 安装目录的 `.install.json` | 按名字显式更新失败；列表的来源回退为 `manual`，批量更新跳过不可读元数据 |

hooks 文件的 UTF-8/JSON 错误只报告来源、类别或位置；安装元数据的解析错误
不回显可能含凭据的 `source` 内容。hooks 文件加载失败与脚本运行失败不同：
`on_failure: allow` 的默认放行策略仅适用于已加载脚本的运行/决策失败（§8.5）。

---

## 9. Provider 详解

### 9.1 内置（bundled）provider

| provider | 密钥环境变量 | base_url |
|---|---|---|
| `openai` | `OPENAI_API_KEY` | `https://api.openai.com/v1` |
| `ollama` | 无需 | `http://localhost:11434/v1` |
| `groq` | `GROQ_API_KEY` | `https://api.groq.com/openai/v1` |
| `deepseek` | `DEEPSEEK_API_KEY` | `https://api.deepseek.com` |
| `openrouter` | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1` |

模型列表见 `bundled/dev.instagent/providers/*.json`，也可用任意该服务
支持的模型名（列表只影响上下文上限推导）。

配置里的 `provider` / `model` 也可用环境变量临时覆盖（§4.2），或用
`--model` 命令行参数。

### 9.2 自定义 provider（插件 JSON）

放 `dev.instagent/providers/<任意名>.json`：

```json
{
  "name": "groq",
  "engine": "openai",
  "api_key_env": "GROQ_API_KEY",
  "base_url": "https://api.groq.com/openai/v1",
  "headers": {},
  "timeout_seconds": 600,
  "models": [
    { "name": "llama-3.3-70b-versatile", "context_limit": 131072 }
  ]
}
```

| 字段 | 说明 |
|---|---|
| `name` | provider 名（配置里 `provider:` 引用的名字） |
| `engine` | `openai`（OpenAI 兼容 `/v1`）/ `proxy`（拉起本地代理进程） |
| `api_key_env` | 密钥环境变量名（密钥唯一来源是这里指向的环境变量：不能写死在插件里，也不能写进 config.yaml，ADR 0003 D1） |
| `base_url` | `openai` 引擎写到 `/v1`（请求时拼 `/chat/completions`） |
| `headers` | 额外请求头，支持 `${env:NAME}` 展开 |
| `timeout_seconds` | 请求超时，默认 600 |
| `models` | 模型表（只用于上下文上限推导） |
| `proxy` | `engine: proxy` 时必填，见下 |

声明 `api_key_env` 即要求该环境变量可读且不是空串或纯空白（包括 Unicode 空白）；
不满足时在引擎构造期失败，不发模型请求。诊断只含 provider 与变量名，不包含密钥原值。
合法非空值原样保留；未声明 `api_key_env` 的 provider 继续支持无密钥接入。

`proxy` 引擎（拉起一个本地进程当 OpenAI 兼容服务）：

```json
{
  "name": "my-local",
  "engine": "proxy",
  "proxy": {
    "command": "./serve.sh",
    "args": ["--port", "${PORT}"],
    "env": {},
    "ready": "/v1/models",
    "timeout_secs": 20
  }
}
```

- `command`：可执行名或 `./` 插件相对路径；`${PORT}` 在拉起时替换成分配
  的端口；就绪探针轮询 `ready` 路径（默认 `/v1/models`）直到返回 200 或
  超时（默认 20s）。
- **就绪期限与重试**：总就绪期限 = `timeout_secs`（默认 20s），全部候选
  尝试共享，重试不重新获得完整 timeout；就绪前提前退出换端口重试至多
  额外 2 次（共 3 次尝试，失败带 provider/命令/尝试数/退出状态）。
  端口选择是 bind→释放→子进程 bind，竞争只能缓解不能根除（见
  `docs/release.md` 的采样记录与 roadmap RM06）。

变量展开（所有 provider JSON）：`${env:NAME}` / `${PLUGIN_ROOT}` /
`${PLUGIN_DATA}`；`${PORT}` 保留给运行时。

### 9.3 名字解析与覆盖

- 配置写裸名（`groq`）；多个插件定义同名时报错，要求写
  `插件名/provider名`（如 `my-plugin/groq`）；
- 用户插件的同名定义优先于 bundled（可以用同名 JSON 覆盖内置定义）。

### 9.4 上下文上限推导（四级）

1. 配置 `context_limit`（最高优先）
2. provider JSON 的 `models[].context_limit`
3. 模型名前缀小表：`claude*` 200k、`gpt-4.1*` 1M、`gpt-4o*` 128k、
   `o<数字>*` 200k
4. 兜底 128k

用量超过 `compaction_threshold`（默认 0.8）× 上限时自动压缩会话。

### 9.5 流式预算与截断

- 单 SSE 事件 1 MiB、单响应文本 8 MiB、累计工具参数 8 MiB、单响应最多
  256 个工具调用；超限终止流并报有界诊断（不回显原始载荷）。
- 缺少 `[DONE]` 且无非空 `finish_reason` 的 EOF 报结构化错误；非空
  `finish_reason` 可以完成传输，但 agent 仍检查终止原因和工具块完整性。
  `length`（`MaxTokens`）、未知原因或不完整响应以 `failed` / `1` 结束，
  即使参数完整也不会执行该响应中的工具、PreToolUse/PostToolUse hooks 或发 ToolStart。
- 完整工具响应接受 `ToolUse`，并兼容 `EndTurn`；无工具的最终答案必须是非空文本
  且以 `EndTurn` 结束。正常完整响应中单个 JSON 参数损坏仍反馈为工具错误，
  不执行该调用，可由模型在后续轮次纠正。
- 异常响应中可校验的助手消息会与每个工具调用对应的错误结果批量保存（受 §6
  会话预算约束），便于恢复；消息结构非法则不提交该响应。不能用下一轮正常回答
  将本次异常改报成功；取消仍按 `cancelled` / `130` 处理。
- 极大 usage 按饱和转换，不回绕成小值绕过压缩阈值。摘要响应另按 §6 的完整性要求检查。

---

## 10. 文件位置速查

| 路径 | 内容 |
|---|---|
| `~/.config/instagent/config.yaml` | 用户配置 |
| `~/.config/instagent/settings.json` | 用户层 settings |
| `<项目>/.config/instagent/settings.json` | 项目层 settings |
| `<项目>/.config/instagent/settings.local.json` | 本地层 settings |
| `<项目>/.agents/plugins/`、`<项目>/.agents/skills/` | 项目级插件 / skills |
| `~/.agents/plugins/` | 用户插件安装根 |
| `~/.agents/skills/` | 用户级 skills |
| `~/.local/share/instagent/sessions/*.jsonl` | 会话文件 |
| `~/.local/share/instagent/plugins/<name>/` | 插件数据目录（`${PLUGIN_DATA}`） |
| `~/.local/share/instagent/bundled/v1-<fnv1a64>/` | 物化的内嵌插件快照（`bundled/` 为缓存父目录） |

均可被 `INSTAGENT_CONFIG_DIR` / `INSTAGENT_DATA_DIR` /
`INSTAGENT_AGENTS_DIR` 重定向。

---

## 11. 故障排查

| 现象 | 处理 |
|---|---|
| 启动报配置解析错 | 检查 config.yaml 的 YAML 语法，或残留的旧 `api_key` / `api_key_env` 键（报错会带迁移提示，ADR 0003 D1） |
| provider 重名报错 | 配置里写 `插件名/provider名` |
| `proxy not ready` | proxy 进程未在总就绪期限（`timeout_secs`，默认 20s）内就绪：手动跑该命令确认能监听 `${PORT}` 并在 `ready` 路径返回 200；重试耗尽后的报错带 provider/命令/尝试数/退出状态 |
| 想看更详细日志 | `RUST_LOG=info instagent run -t "检查项目"`（或 `debug`），日志走 stderr；默认 warning 已可见，无需显式开启 |
| `run` 在等待 stdin | `run` 不读取 stdin；检查 provider / MCP / 工具诊断，并使用 `--timeout` 限制执行期限 |
| 模板未找到 | 用 `--command 插件名:模板名`，确认插件已启用且存在 `dev.instagent/commands/<模板名>.md` |
| 会话内容错乱 | 确认没有两个进程同时 `--resume` 同一个会话 |

开发自检：

```bash
bash scripts/ci.sh    # fmt / clippy / cargo test / python 回归 / rustdoc / release smoke / --help
```

默认回归使用离线假 provider；继承 `TOKEN_PLAN_API_KEY` 也不会启动 10 个 ignored
真实模型用例。显式在线命令为 `cargo test --test live_e2e -- --ignored`，要求提前注入
非空白凭据，否则立即失败。CLI/live 的 liveplug 输出各在独占临时副本中。
实际执行数、ignored 数及在线验证状态分开记录，见[发布与校验记录](release.md)。
