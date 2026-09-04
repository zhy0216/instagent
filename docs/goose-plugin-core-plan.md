# 插件为核心的最小 agent 计划（第三版）

> **历史文档，不是当前契约。** 本文是 2026-09-02 的设计计划；实现以
> `README.md`、`docs/usage.md`、`docs/architecture.md` 与 `docs/adr/` 为准。
> 与实现不一致之处：§2.4 的可选 anthropic engine 已被 ADR 0001 移除；
> approval / trust / mode UI 相关设想已被 ADR 0002 否弃；env、密钥、
> stdout 契约等运行时策略以 ADR 0003 为准。命名映射：`mini-agent` →
> `instagent`、`dev.miniagent` → `dev.instagent`、`MINI_AGENT_*` →
> `INSTAGENT_*`（全表见 README"命名对照"）。

- 日期：2026-09-02
- 参考基线：`~/yyds/goose` commit `4ad43df`
- 前两版：`goose-core-cherry-pick-plan.md`（抽取）、`goose-from-scratch-plan.md`（从零）。
  本版在"从零"的基础上改架构：loop、消息模型、会话、压缩、CLI 的设计沿用第二版 §2.2、2.5、2.6、2.7、2.11，
  本文只写变化的部分：插件模型、provider、工具来源、内核边界。
- 工程暂名 `mini-agent`，插件命名空间暂用 `dev.miniagent`，两者都要换成你自己的。

---

## 0. 三条要求怎么落地

### 0.1 "插件为核心，可能是 agent open plugin 的形式"

采用 **Agent Plugins 规范 v1.0.0**（agent-plugins.org，原 open-plugins.com，旧域名已 308 跳转）。
goose 的 `crates/goose/src/plugins/` 就是这个规范早期草案的实现，所以 goose 的插件目录我们能兼容读取。

规范只定义两种可移植组件：**skills**（Agent Skills 规范的 `SKILL.md`）和 **MCP servers**（`mcp.json`）。
规范原文："Agent Plugins v1 defines exactly two component types: skills and MCP servers. Other component
types are outside the v1 format." 其他一切（hooks、commands、providers、tools）必须放在客户端自己的
反域名命名空间里，别的客户端会忽略。所以我们的插件 = 规范部分（skills + mcp.json，goose 和 Claude Code
也能装）+ `dev.miniagent/` 目录里的私有部分（providers、hooks、commands、command tools）。

### 0.2 "除了最简单的工具其他都是插件，包括 MCP"

内核只内置 5 个工具：`shell` `read` `write` `edit` `tree`。**所有 MCP server 都由插件的 `mcp.json` 声明**，
内核配置文件里不再有 `mcp:` 段。

一个需要说清楚的点：MCP **client**（rmcp 适配器，约 300 行）留在内核里，因为它是规范组件类型的运行时，
和 `SKILL.md` 的加载器是同一层次的东西；没有它，规范里的 `mcp.json` 组件就无法工作。如果你的意思是
连 MCP client 也要可替换，那对应 §2.5 的 `ToolSource` trait：内核里 MCP、内置工具、command tools、
skills 四种来源都实现同一个 trait，替换的是实现，不是动态加载。用 Rust 动态库做"真插件"
（dlopen / abi_stable）明确不做，收益远小于麻烦。

### 0.3 "provider 也用插件提供，默认支持 openai 的形式"

内核只实现**一种** wire protocol：OpenAI Chat Completions（流式、tool calls、usage）。
provider 插件 = `dev.miniagent/providers/*.json` 里的声明，格式直接沿用 goose-providers 的 declarative 定义
（`crates/goose-providers/src/declarative/definitions/*.json`，45 个现成的可以脚本转换）。

非 openai 协议的 provider 走两条路：
- `engine: "proxy"`：插件带一个命令，内核拉起它，得到一个本地 openai 兼容端点，再用内核的 openai engine 打过去。
- `engine: "anthropic"`：内核可选编译进的原生 engine（第二版 §2.3 里那 450 行），cargo feature 控制。

Anthropic 官方有 openai 兼容端点（`https://api.anthropic.com/v1/`，用 Claude API key），可以作为 v1 过渡，
但官方文档明确它"primarily intended to test and compare model capabilities, not considered a long-term or
production-ready solution"，并列出限制：**不支持 prompt caching**、`strict` 被忽略、thinking 内容不返回、
多条 system 消息被合并成一条、`reasoning_effort` 被忽略。对一个 coding agent 来说 prompt caching 是成本大头，
所以 Claude 要正经用，必须走 proxy 或原生 engine。计划里把这条标为 P2 的可选项，不是遗漏。

---

## 1. 内核边界

| 在内核里（写死） | 由插件提供（内核只提供运行时） |
|---|---|
| agent loop、消息模型、会话 JSONL、自动压缩 | 所有 provider 定义（含 bundled 那份） |
| 配置与 settings 读取 | 所有 MCP server |
| 插件加载器：manifest 校验、发现、启用、安装 | 所有 skills |
| 规范组件运行时：skills 加载器、MCP client | hooks |
| openai engine、proxy 拉起器、可选 anthropic engine | 斜杠命令 |
| 5 个内置工具 | command tools（脚本即工具） |
| 审批、白名单 | |
| hooks 执行器 | |
| CLI | |

"内置的东西也以插件形式出现"：内核用 `include_dir` 内嵌一个 `bundled` 插件目录（只有 `plugin.json` 和
`dev.miniagent/providers/*.json`），启动时和外部插件走同一条加载路径，同名 provider 用户插件优先。
5 个内置工具挂在一个名为 `builtin` 的伪插件下，只是为了让 `plugin list` 和工具前缀规则统一。

---

## 2. 插件模型

### 2.1 目录布局

```
my-plugin/
├── plugin.json                 # 规范必需
├── skills/                     # 规范组件：每个子目录一个 SKILL.md
│   └── review/SKILL.md
├── mcp.json                    # 规范组件：MCP servers
└── dev.miniagent/              # 我们的命名空间目录，规范允许，其他客户端忽略
    ├── providers/*.json        # provider 定义
    ├── tools/*.json            # command tools（可选）
    ├── hooks.json              # 事件钩子，沿用 goose 的格式和协议
    └── commands/*.md           # 斜杠命令（可选）
```

规范要点（`agent-plugins.org/specification`）：
- `plugin.json` 顶层只允许 `$schema` `name` `version` `description` `author` `homepage` `repository`
  `license` `keywords` `extensions` 十个字段；未知字段报告但不致命。
- `$schema` 必须是 `https://agent-plugins.org/schemas/1.0.0/plugin.schema.json`，客户端**不联网取 schema**，
  用这个字符串选本地校验规则。
- `name`：1~64 字符，小写字母数字和 `-` `.`，首尾必须是字母数字，不能有 `--` `..`。
- `skills/` 只看一层子目录，每个含 `SKILL.md` 的子目录是一个 skill；无效的跳过，不递归。
- `mcp.json` 固定在插件根，不能内联在 `plugin.json`；`$schema` 版本必须和 `plugin.json` 一致，不一致只让 MCP 部分失效。
- 组件目录不存在不算错。
- 命名空间：`extensions` 对象里的键和顶层同名目录都是反域名；客户端必须忽略自己不实现的命名空间。

### 2.2 plugin.json

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
  "name": "my-plugin",
  "version": "1.0.0",
  "description": "Groq provider + a review skill + a lint hook",
  "license": "Apache-2.0",
  "extensions": {
    "dev.miniagent": { "minKernel": "0.1" }
  }
}
```

`extensions["dev.miniagent"]` 只放小标志（最低内核版本之类），组件一律放固定位置，和规范对 skills / mcp.json 的做法一致。

### 2.3 mcp.json（规范格式）

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
    },
    "remote-docs": {
      "type": "streamable-http",
      "url": "https://example.com/mcp",
      "headers": {}
    }
  }
}
```

- `type` 三选一：`stdio` / `streamable-http` / `sse`（v1 只实现前两个，`sse` 报"不支持"并跳过）。
- `command` 必须是单个可执行名或 `./` 开头的插件相对路径；不做变量展开。
- 只展开 `${PLUGIN_ROOT}` 和 `${PLUGIN_DATA}`，单次、非递归，只作用于 `args` 元素、`env` 值、`cwd`；
  别的 `${...}` 原样保留。`env` 里不能定义 `PLUGIN_ROOT` / `PLUGIN_DATA` 这两个名字。
- 规范规定 `headers` 不能放凭据。远程 MCP 的鉴权 v1 不做；v2 在命名空间里加 `mcp-auth.json`（server → 从环境变量取的 header）。
- 兼容读取：没有 `mcp.json` 时退而读 `.mcp.json`（goose / Claude Code 草案格式，没有 `type` 字段，按 stdio 处理）。

### 2.4 provider 插件（`dev.miniagent/providers/*.json`）

字段沿用 goose 的 `DeclarativeProviderConfig`（`crates/goose-providers/src/declarative.rs:148~186`），去掉 setup 向导相关的部分，加 `proxy`：

```json
{
  "name": "groq",
  "engine": "openai",
  "display_name": "Groq",
  "api_key_env": "GROQ_API_KEY",
  "base_url": "https://api.groq.com/openai/v1",
  "headers": {},
  "timeout_seconds": 600,
  "models": [
    { "name": "llama-3.3-70b-versatile", "context_limit": 131072, "max_tokens": 8192 }
  ]
}
```

注意 goose 的 `base_url` 写到 `/chat/completions`，我们写到 `/v1`，转换脚本要改这一处。

三种 engine：

| engine | 内核实现 | 说明 |
|---|---|---|
| `openai` | v1，约 450 行 | Chat Completions，`Authorization: Bearer <api_key_env>`，`base_url` 后拼 `/chat/completions` |
| `proxy` | v1，约 150 行 | 拉起 `proxy.command`，内核选一个空闲端口，替换 `args` 里的 `${PORT}` 并设环境变量 `MINI_AGENT_PORT`（本仓库实际为 `INSTAGENT_PORT`），轮询 `GET http://127.0.0.1:{port}{proxy.ready}` 到 200 为止（`ready` 默认 `/v1/models`，超时默认 20s），之后当 `openai` engine 用；会话结束时 kill；连接失败自动重启一次 |
| `anthropic` | v1.5，cargo feature，约 450 行 | 原生 Messages API：cache_control、thinking、strict；实现见第二版 §2.3 |

`proxy` 示例（Claude 走原生协议，但内核不用会 Messages API）：

```json
{
  "name": "anthropic",
  "engine": "proxy",
  "proxy": {
    "command": "./bin/claude-proxy",
    "args": ["--listen", "127.0.0.1:${PORT}"],
    "env": { "ANTHROPIC_API_KEY": "${env:ANTHROPIC_API_KEY}" },
    "ready": "/v1/models",
    "timeout_secs": 20
  },
  "models": [ { "name": "claude-sonnet-4-6", "context_limit": 1000000, "max_tokens": 64000 } ]
}
```

provider JSON 在我们自己的命名空间里，所以变量规则由我们定：支持 `${env:NAME}`、`${PLUGIN_ROOT}`、`${PLUGIN_DATA}`、`${PORT}`。

**bundled provider 插件**（内嵌）：`openai`、`ollama`（`http://localhost:11434/v1`）、`groq`、`deepseek`、`openrouter`、
`anthropic-compat`（openai engine 打 `https://api.anthropic.com/v1`，描述里写明官方限制）。都从 goose 的 45 个 JSON 转换。

**选择**：配置里 `provider: groq`、`model: ...`。按名字在所有启用插件的 providers 里找；重名时报错并要求写成 `plugin/name`；
用户插件覆盖 bundled。`context_limit` 来源顺序：配置覆盖 → provider JSON 的 models 表 → 第二版 §2.3 的前缀小表 → 128k。

### 2.5 工具来源：`ToolSource`

```rust
#[async_trait]
pub trait ToolSource: Send + Sync {
    fn id(&self) -> &str;                                   // "builtin" | "mcp:<plugin>/<server>" | "cmd:<plugin>" | "skills"
    async fn list(&self) -> Vec<ToolSpec>;
    async fn call(&self, name: &str, input: Value, ctx: &ToolCtx) -> ToolOutput;
    async fn shutdown(&self) {}
}
```

内核里四个实现：

| 实现 | 来源 | 行数 | 说明 |
|---|---|---|---|
| `BuiltinTools` | 内核 | 1100 | 第二版 §2.4 的 5 个工具 |
| `McpSource` | 每个插件 `mcp.json` 的每个 server 一个实例 | 300 | rmcp；stdio 用进程组 + kill_on_drop（搬 goose `subprocess.rs`），streamable-http 用 `StreamableHttpClientTransport`；`initialize` 返回的 `instructions` 进 system prompt |
| `CommandTools` | `dev.miniagent/tools/*.json` | 120 | 见 §2.9 |
| `SkillsSource` | 所有 skills | 150 | 只暴露一个 `load_skill` 工具，见 §2.6 |

工具命名：内置不加前缀；MCP 为 `<server>__<tool>`；command tools 为 `<plugin>__<tool>`；
同名冲突时前面再加插件名。OpenAI 函数名只允许 `[A-Za-z0-9_-]{1,64}`，超长的截断后加 6 位哈希，
内核保存映射表，模型看到的名字和调用路由都走这张表。

### 2.6 skills（规范组件）

- 发现范围：每个启用插件的 `skills/`，再加 `~/.agents/skills/` 和 `<project>/.agents/skills/`（goose 同款目录）。
- `SKILL.md` frontmatter 按 Agent Skills 规范：`name`（必需，1~64，小写字母数字和 `-`，必须等于目录名）、
  `description`（必需，≤1024）、可选 `license` `compatibility` `metadata` `allowed-tools`。
- 插件里的 skill 命名为 `<plugin>:<skill>`（goose 的做法，`open_plugins.rs` 里 `namespaced_component_name`）。
- 渐进加载：启动时只把所有 skill 的 `name` + `description` 放进 system prompt；模型调用 `load_skill(name, file?)`
  才读 `SKILL.md` 正文或 `references/` 下的文件。工具描述文本直接抄 goose `skills/client.rs:91~103`。
- `allowed-tools` 是实验字段，v2 再用它给审批白名单加临时项。

### 2.7 hooks（`dev.miniagent/hooks.json`，沿用 goose 协议）

文件格式、载荷、决策协议**逐字沿用 goose**（文档 `documentation/docs/guides/context-engineering/hooks.md`，
实现 `crates/goose/src/hooks/mod.rs`），这样同一份 hooks 脚本两边都能用：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "shell|everything__.*",
        "hooks": [
          { "type": "command", "command": "${PLUGIN_ROOT}/scripts/guard.sh", "timeout": 10, "on_failure": "block" }
        ]
      }
    ]
  }
}
```

- 事件 v1：`SessionStart` `UserPromptSubmit` `PreToolUse` `PostToolUse` `Stop` `SessionEnd`。只有 `PreToolUse` 和 `Stop` 能阻止。
- `matcher` 是正则不是 glob，匹配工具名；省略则每次都跑。命令用 `sh -c` 跑，默认超时 30s。
- 载荷 JSON 写到 stdin：`event` `session_id` 必有，`tool_name` `tool_input` `tool_output` `message` `working_dir` 按事件出现
  （字段名照 goose `HookContext`，`hooks/mod.rs:225~258`）。
- 决策：退出码 2 → 阻止，理由取 stderr；stdout 是 `{"decision":"block","reason":"..."}` → 阻止，不看退出码；
  退出 0 且 stdout 为空或 `{"decision":"allow"}` → 放行；其他一律算"没有决策"，按 `on_failure`（默认 allow）处理。
- `Stop` 被阻止时本轮继续跑，连续阻止有上限（goose 默认 8 次），防止死循环。
- 兼容读取：没有 `dev.miniagent/hooks.json` 时退而读 `hooks/hooks.json`（goose 草案位置）。
- 用途举例：禁止 `sudo`、写审计日志、每轮结束跑 lint 并在失败时阻止结束、SessionStart 时注入项目说明。这是"不写代码改行为"的主要通道。

### 2.8 commands（`dev.miniagent/commands/*.md`，可选）

```markdown
---
description: Review the current diff
argument-hint: [focus]
---
Review `git diff` with focus on: $ARGUMENTS. Report findings as a list.
```

`/review security` 展开成一条用户消息。约 60 行，Claude Code 的约定，goose 没实现。

### 2.9 command tools（`dev.miniagent/tools/*.json`，可选但推荐）

```json
{
  "name": "weather",
  "description": "Get current weather for a city",
  "input_schema": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] },
  "command": "${PLUGIN_ROOT}/scripts/weather.sh",
  "timeout_secs": 30,
  "read_only": true
}
```

内核把 input JSON 写到 stdin，stdout 作为工具结果，退出码非 0 即 `is_error`。一个十行脚本就是一个工具，
不用起 MCP server。这是本设计里"插件为核心"最直接的体现。

### 2.10 发现、启用、安装、信任

- 位置：`~/.agents/plugins/<name>/`（用户）、`<project>/.agents/plugins/<name>/`（项目），
  开发时 `--plugin PATH` 临时加载。规范不规定位置，这两个是 goose 的约定，沿用以便互通。
- `PLUGIN_DATA`：`~/.local/share/mini-agent/plugins/<name>/`。
- 启用：`~/.config/mini-agent/settings.json`、`<project>/.config/mini-agent/settings.json`、`settings.local.json`，
  字段 `enabledPlugins` / `disabledPlugins`（goose 草案格式）。优先级 local > project > user；
  写了 `enabledPlugins` 就是白名单模式，没写就是"除 disabled 外全启用"。同名插件项目层覆盖用户层。
- 安装：`mini-agent plugin install <git-url | path> [--auto-update]`：clone 或复制 → 校验 `plugin.json` →
  写 `.install.json`（source、commit、时间）→ 放入用户目录。`plugin list | update | enable | disable | show`。
  逻辑参考 goose `plugins/mod.rs`（install 元数据、24h 自动更新节流）。用 `git` 子进程，不引 libgit2。
- 信任：插件里只要有会执行命令的东西（mcp.json、hooks、tools、proxy provider），第一次启用时列出全部命令让用户确认，
  结果记到 settings 的 `trustedPlugins`；`--yes` 跳过。密钥只能走环境变量（`api_key_env` / `${env:...}`），插件文件里不得出现，
  和规范对 `headers` 的要求一致。

---

## 3. 与第二版的差异

| 模块 | 第二版 | 第三版 |
|---|---|---|
| provider/anthropic.rs | 内核必有 | 变成可选 engine（feature），默认不编 |
| provider 选择 | 配置里写 base_url / api_key_env | 配置里只写 provider 名，定义来自插件 |
| tools/mcp.rs | 读配置里的 `mcp:` 列表 | 读插件 `mcp.json`，一个 server 一个 `McpSource` |
| 内置工具 | 直接注册 | 经 `BuiltinTools: ToolSource` 注册，和其他来源同构 |
| 新增 | | `plugin/`（manifest、discovery、settings、install）、`skills/`、`hooks/`、`commands/`、`command_tools/`、`provider/proxy.rs`、`bundled/` |
| config.yaml | 有 `mcp:` 段 | 无 `mcp:` 段；多了 `plugins:`（额外路径）和 settings 文件 |
| 审批白名单 | 配置里 | 配置里，另加 skill `allowed-tools`（v2） |

---

## 4. 模块与行数估算

```
mini-agent/src/
├── main.rs, cli/               900   第二版 + plugin 子命令
├── config.rs, settings.rs      250
├── message.rs, session.rs      400
├── plugin/
│   ├── manifest.rs             150   plugin.json 校验（name 规则、$schema、未知字段报告）
│   ├── discovery.rs            150   位置、优先级、enabled/disabled
│   ├── install.rs              150   git clone / copy / .install.json / update
│   ├── mcp_config.rs           120   mcp.json 解析 + 变量展开 + .mcp.json 兼容
│   └── bundled.rs               40   include_dir
├── provider/
│   ├── mod.rs                  120   trait、Request、StreamEvent、ProviderError
│   ├── registry.rs             150   provider JSON → engine 实例，重名处理，context_limit 解析
│   ├── http.rs                 250
│   ├── openai.rs               450
│   ├── proxy.rs                150
│   └── anthropic.rs            450   [feature]
├── tools/
│   ├── mod.rs                  200   ToolSource、Registry、命名与 64 字符映射
│   ├── builtin/               1100   shell / fs / tree
│   ├── mcp.rs                  300
│   ├── command.rs              120
│   └── skills.rs               150
├── hooks.rs                    350
├── commands.rs                  60
├── agent/                      720   loop / approval / compact / prompt / event（第二版）
└── subprocess.rs               150   从 goose 搬
                              ─────
                              ~6.3K   + anthropic engine 450 + 测试约 1.8K ≈ 8.5K
```

依赖在第二版基础上加 `include_dir`（内嵌 bundled 插件）和 `regex`（hooks matcher）。仍约 20 个直接依赖。

---

## 5. 阶段与验证

| 阶段 | 内容 | 验证 | 工期 |
|---|---|---|---|
| P0 骨架 | Cargo、config、settings、message、session | 第二版 P0 | 0.5 天 |
| P1 插件加载 | manifest / discovery / settings / mcp_config / bundled / install | fixture 插件目录若干：合法、name 非法、$schema 版本不符、缺组件、命名空间未知、`.mcp.json` 草案格式；`plugin install` 本地路径和 git；local > project > user 优先级 | 1.5 天 |
| P2 provider | openai engine、registry、proxy 拉起器 | SSE fixture（抄 goose `formats/openai.rs` 测试段）；wiremock 测重试；proxy 用一个 10 行的假 server 测端口替换、就绪轮询、退出 kill；真 key 跑 groq 和 anthropic-compat 各一次 | 1.5 天 |
| P2b | anthropic 原生 engine（可选） | 第二版 P1 | 1 天 |
| P3 工具 | builtin 5 个、ToolSource、McpSource、CommandTools、SkillsSource | 第二版 P2 + P2b；command tool 用 shell 脚本；skills 用规范里的最小示例，`load_skill` 返回正文；工具名超 64 字符的映射 | 2 天 |
| P4 loop | agent/、approval、compact | 第二版 P3 + P4 | 1.5 天 |
| P5 hooks + commands | hooks.rs、commands.rs | 每种决策路径一个脚本：exit 2、stdout block、exit 0 空、乱输出 + on_failure block、超时；Stop 连续阻止上限；`${PLUGIN_ROOT}` 展开 | 1 天 |
| P6 CLI | chat / run / sessions / plugin 子命令、信任提示 | 第二版 P5 + `plugin install/list/enable/disable`，首次启用带命令的插件出现确认 | 1.5 天 |
| P7 加固 | clippy、错误路径、README、CI | 插件目录被删、MCP server 起不来、proxy 没就绪、provider 重名，四种情况提示可读 | 1 天 |

合计约 10.5 天，比第二版多一天半，多出来的全是插件加载和 hooks。

---

## 6. 从 goose 拿什么（本版新增的部分）

| 我们的模块 | goose 位置 | 用法 |
|---|---|---|
| plugin/manifest.rs | `plugins/formats/open_plugins.rs`（848 行）：`read_manifest`、`validate_plugin_name`、`namespaced_component_name`、`rewrite_skill_name` | 参考；注意 goose 认的是草案路径 `.goose-plugin/plugin.json` / `.plugin/plugin.json` / `plugin.json` |
| plugin/discovery.rs | `plugins/discovery.rs`（558 行）：scope 优先级、`enabledPlugins`/`disabledPlugins`、settings 三层 | 参考，逻辑基本照搬 |
| plugin/install.rs | `plugins/mod.rs`（602 行）：`.goose-plugin-install.json` 元数据、24h 自动更新节流、git 子进程 | 参考 |
| plugin/mcp_config.rs | `plugins/mcp_servers.rs`（335 行） | 参考；它读的是草案 `.mcp.json`，我们主读规范 `mcp.json` |
| hooks.rs | `hooks/mod.rs`（2326 行）：`HookContext`（225~258）、决策读取（约 900~1000 行处，`status.code() == Some(2)`、stdout JSON）、`${PLUGIN_ROOT}`；文档 `hooks.md` 是协议原文 | 协议逐字沿用；代码只搬 `HookContext` 和决策判定，其余重写到 350 行 |
| tools/skills.rs | `skills/mod.rs`（`all_skill_dirs`、frontmatter 校验）、`skills/client.rs:91~103`（`load_skill` 描述）、`sources.rs:20 parse_frontmatter` | 描述文本直接抄；发现逻辑参考 |
| provider/registry.rs + bundled | `goose-providers/src/declarative/definitions/*.json`（45 个）、`declarative.rs:148~186` | 写一个 20 行脚本转换：`base_url` 去掉 `/chat/completions`，去掉 `setup`，`engine: anthropic` 的标成需要 proxy 或原生 engine |
| subprocess.rs | `crates/goose/src/subprocess.rs`（144 行） | 直接搬 |

---

## 7. 风险与决策点

1. **规范很新**（v1.0.0），goose 实现的是更早的草案：manifest 路径、`.mcp.json`、`hooks/hooks.json` 都不同。
   我们以 v1.0.0 为准，草案布局只做兼容读取。以后规范加了 hooks 之类的组件类型，把命名空间里的搬到规范位置即可。
2. **只有 skills 和 mcp.json 可移植**。providers、hooks、commands、command tools 在别的客户端里是死目录。
   这是规范的设计，不是我们的问题；写插件时把"能移植的"和"只给我们的"分开放就行。
3. **单一 wire protocol 的代价**：Claude 的 prompt caching、thinking、strict 只有原生 engine 或 proxy 才有。
   如果主力模型是 Claude，P2b 不是可选而是必做，工期按 11.5 天算。
4. **proxy 生命周期**：端口冲突、就绪超时、中途崩溃、退出残留。P2 的假 server 测试要覆盖这四种。
5. **工具名 64 字符**：MCP server 名 + 工具名很容易超；映射表必须双向且稳定（同一会话内不变）。
6. **插件执行任意命令**：信任确认 + 环境变量只透传白名单（`PATH` `HOME` `LANG` 和插件声明的 `env`）。
7. **goose 兼容读取的范围**要克制：只兼容 `.mcp.json` 和 `hooks/hooks.json` 两处，不兼容 goose 的 `extensions:` 配置格式。

---

## 8. 一个插件长什么样（完整示例）

```
groq-and-review/
├── plugin.json
├── skills/
│   └── review/
│       ├── SKILL.md
│       └── references/checklist.md
├── mcp.json                       # 一个 stdio server
└── dev.miniagent/
    ├── providers/groq.json
    ├── tools/weather.json
    ├── hooks.json                 # PreToolUse 拦 sudo
    ├── commands/review.md
    └── scripts/
        ├── guard.sh
        └── weather.sh
```

装上之后模型能看到：`shell` `read` `write` `edit` `tree`（内置）、`everything__echo` 等（MCP）、
`groq-and-review__weather`（command tool）、`load_skill`（skills）；system prompt 里多一行
`groq-and-review:review — Review code changes ...`；配置里 `provider: groq` 生效；`/review` 可用；
每次调 `shell` 前 `guard.sh` 先跑。
