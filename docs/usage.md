# instagent 使用说明

instagent 是一个**以插件为核心**的最小 agent：内核只有 5 个内置工具
（`shell` `read` `write` `edit` `tree`）和一个 agent loop；provider、MCP
server、skills、hooks、斜杠命令、command tools 全部以插件形式加载——连内置的
5 个 provider 定义（openai / ollama / groq / deepseek / openrouter）
也来自一个随二进制分发的 bundled 插件。

插件格式遵循 [Agent Plugins v1.0.0](https://agent-plugins.org/specification)，
hooks 的载荷与决策协议与 [block/goose](https://github.com/block/goose) 兼容。

instagent 运行在 sandbox 内，工具直接执行，安全边界由 sandbox 承担
（见 [docs/adr/0002](adr/0002-sandbox-agent-no-ui-permission.md)）。

---

## 目录

1. [安装构建](#1-安装构建)
2. [快速上手](#2-快速上手)
3. [命令参考](#3-命令参考)
4. [配置](#4-配置)
5. [REPL 交互](#5-repl-交互)
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
api_key_env: GROQ_API_KEY      # 从哪个环境变量读密钥
```

**第二步：导出密钥并启动：**

```bash
export GROQ_API_KEY=...
instagent chat                 # 交互式 REPL
instagent run -t "list files"  # 无交互跑一条任务
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
| `instagent chat` | 交互式 REPL |
| `instagent run -t "任务"` | 无交互跑一条任务 |
| `instagent sessions <list\|rm>` | 会话管理 |
| `instagent plugin <子命令>` | 插件管理 |

### `instagent chat`

```bash
instagent chat [--resume <id|last>] [--cwd PATH] [-m MODEL] [--plugin PATH ...]
```

| 选项 | 说明 |
|---|---|
| `--resume <id>` | 恢复指定会话；`--resume last` 恢复最近一次 |
| `--cwd PATH` | 工作目录（不存在会创建），影响 shell/read 等工具的相对路径与项目级 settings/skills 发现 |
| `-m, --model MODEL` | 覆盖配置里的 `model` |
| `--plugin PATH` | 临时加载一个插件目录（可多次），不安装、不落盘，开发调试用 |

会话生命周期会触发插件的 `SessionStart` / `SessionEnd` hooks。

### `instagent run`

```bash
instagent run -t "..." [--cwd PATH] [-m MODEL] [--plugin PATH ...]
```

无交互：新建一个会话，把 `-t` 内容作为唯一一条用户消息跑完。

**输出契约**（[ADR 0003 D4](adr/0003-repo-boundaries-and-runtime-policies.md)）：
stdout 只输出模型最终回答的流式文本，`>file` / 管道拿到的就是纯答案；
工具事件（`▶` / `✓` / `✗` 行与预览）、`usage:` 行、`session <id>`、
装配提示（`note: …`）与一切诊断统一走 stderr。运行失败 → 非零退出码，
stderr 末行为 `error: <原因>`；stdout 写失败（如管道提前关闭的 EPIPE）
不改退出码。`chat` 同契约：横幅、斜杠命令反馈、Ctrl-C 提示也在 stderr。

### `instagent sessions`

```bash
instagent sessions list        # 列出全部会话（编号、id、时间、provider/model、cwd）
instagent sessions rm <id>     # 删除会话文件
```

### `instagent plugin`

```bash
instagent plugin install <git-url 或本地路径> [--auto-update]
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
| `api_key_env` | 无 | 从该环境变量读密钥（推荐方式） |
| `api_key` | 无 | 直接写密钥（文件会按 0600 权限保存；优先用 `api_key_env`） |
| `max_tokens` | `8192` | 单次回复最大 token |
| `max_turns` | `1000` | 单条用户消息内最多循环多少轮工具调用 |
| `context_limit` | 无 | 覆盖模型上下文上限（默认按 §9 的四级推导） |
| `compaction_threshold` | `0.8` | token 用量占上下文比例超过该值时自动压缩会话 |
| `shell` | `$SHELL` | `shell` 工具使用的解释器 |
| `plugins` | `[]` | 额外插件搜索目录（支持 `~`；相对路径按启动时 cwd 解析） |

示例：

```yaml
provider: openai
model: gpt-5
api_key_env: OPENAI_API_KEY
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

日志：`RUST_LOG=warn`（默认关闭，REPL 输出干净；日志走 stderr）。

### 4.3 `settings.json`（三层）

控制插件的启用/禁用，按 **local > project > user** 合并：

| 层 | 路径 |
|---|---|
| User | `~/.config/instagent/settings.json` |
| Project | `<项目>/.config/instagent/settings.json` |
| Local | `<项目>/.config/instagent/settings.local.json`（建议 gitignore） |

```json
{
  "enabledPlugins": ["my-plugin"],
  "disabledPlugins": ["bundled"]
}
```

- `enabledPlugins`：**写了就是白名单模式**，只有列出的插件启用；不写则"除
  `disabledPlugins` 外全部启用"。
- `disabledPlugins`：黑名单。
- 同名插件的 enabled/disabled 字段：以最高层出现的为准；某层缺省的字段
  不参与覆盖（注意区分"没写"和"写了空数组"）。

---

## 5. REPL 交互

启动横幅显示 provider / model / session id。输入行直接发给模型；
`/` 开头是斜杠命令：

| 命令 | 说明 |
|---|---|
| `/exit` `/quit` | 退出 |
| `/clear` | 丢弃当前上下文（会话文件原子重写，留 `.bak.jsonl`） |
| `/compact` | 立即强制压缩会话 |
| `/tools` | 列出当前可见的全部工具（含来源前缀与 read-only 标记） |
| `/help` | 帮助 + 插件提供的斜杠命令列表 |
| `/<插件命令> [参数]` | 插件斜杠命令，展开成一条用户消息（§8.7） |

**Ctrl-C 语义**：

- 模型生成中：第一次取消当前轮（`(turn cancelled)`，REPL 可继续），
  第二次退出进程。
- 空闲提示符：第一次提示，第二次退出。
- Ctrl-D：直接退出。

**输出渲染**：流式文本逐字打印到 stdout（答案流）；工具调用 `▶ 工具名 …`
+ 参数预览与耗时、每轮末尾的 `usage:` 行（token 用量）、横幅与斜杠命令
反馈等诊断统一走 stderr（§3 输出契约）。REPL 输入历史存在
`~/.config/instagent/history.txt`。

---

## 6. 会话管理

- 会话以 JSONL 存在 `~/.local/share/instagent/sessions/<id>.jsonl`，
  首行是 header（id、时间、provider、model、cwd）。
- `instagent chat --resume last` 恢复最近会话；`--resume <id>` 恢复指定
  会话（id 见 `instagent sessions list`）。
- **单进程独占**：不要对同一会话 id 同时开两个 `--resume`，两进程追加写
  会互相漂移。
- 上下文过长时按 `compaction_threshold` 自动压缩；`/compact` 手动触发，
  完成后显示 `· compacted X → Y tokens`。
- `run` 子命令每次都会新建会话（结束时打印 `session <id>` 到 stderr）。

---

## 7. 插件管理

### 7.1 发现来源与优先级

启动时按以下顺序发现插件（高优先级在前）：

1. `--plugin PATH` 命令行参数（临时加载，可多个）
2. 配置 `plugins:` 列出的目录
3. 项目级 `<cwd>/.agents/plugins/`
4. 用户级 `~/.agents/plugins/`
5. bundled（随二进制分发的内置插件，首次运行物化到
   `~/.local/share/instagent/bundled/`）

manifest 校验失败的目录记警告跳过，不中断启动；settings 里启用但目录已删
的插件同样只警告。同名插件先到先得。

### 7.2 安装 / 更新

```bash
instagent plugin install https://github.com/some/plugin-repo   # git 克隆到 ~/.agents/plugins/<name>/
instagent plugin install ./my-local-plugin                     # 本地路径：复制安装
instagent plugin install <src> --auto-update                   # 标记为可自动更新
instagent plugin update              # 更新全部 git 来源插件（git pull）
instagent plugin update my-plugin    # 只更新一个
```

### 7.3 查看 / 启用 / 禁用

```bash
instagent plugin list            # 名字、版本、启用状态、来源
instagent plugin show my-plugin  # 详情：root、来源、commit
instagent plugin disable my-plugin
instagent plugin enable my-plugin
```

### 7.4 开发时临时加载

```bash
instagent chat --plugin ./my-dev-plugin
```

不安装、不进 settings，适合边写边试；该插件定义的 provider / 斜杠命令等
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
  ├ commands/review.md           # 斜杠命令
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

固定在插件根目录。只支持 `stdio` 传输（`sse` 会跳过并提示）：

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
- 子进程按进程组管理，超时杀整组。
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

### 8.7 `dev.instagent/commands/*.md`（斜杠命令）

文件名即命令名（`review.md` → `/review`）。frontmatter 取 `description` /
`argument-hint`，正文是模板：

```markdown
---
description: Review the current diff
argument-hint: [focus]
---
Review `git diff` with focus on: $ARGUMENTS. Report findings as a list.
```

- `$ARGUMENTS` 展开为用户在命令后输入的参数；
- 模板里没有 `$ARGUMENTS` 时，参数追加到正文末尾（Claude Code 约定）；
- 多插件同名命令先到先得（插件名升序）；解析失败的文件跳过。
- `/help` 会列出全部插件命令及参数提示。

### 8.8 装上之后的完整效果

以上述 `groq-and-review` 为例，启用后：

- 模型可见工具：内置 5 个 + `everything__echo` 等（MCP）+
  `groq-and-review__weather`（command tool）+ `load_skill`（skills）；
- system prompt 多一行 `groq-and-review:review — …`（skill 索引）；
- 配置里 `provider: groq` 可用；
- REPL 里 `/review <args>` 可用；
- 每次调 `shell` 前 `guard.sh` 先跑。

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
| `api_key_env` | 密钥环境变量名（密钥只能走环境变量或用户配置，不能写死在插件里） |
| `base_url` | `openai` 引擎写到 `/v1`（请求时拼 `/chat/completions`） |
| `headers` | 额外请求头，支持 `${env:NAME}` 展开 |
| `timeout_seconds` | 请求超时，默认 600 |
| `models` | 模型表（只用于上下文上限推导） |
| `proxy` | `engine: proxy` 时必填，见下 |

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

---

## 10. 文件位置速查

| 路径 | 内容 |
|---|---|
| `~/.config/instagent/config.yaml` | 用户配置 |
| `~/.config/instagent/settings.json` | 用户层 settings |
| `~/.config/instagent/history.txt` | REPL 输入历史 |
| `<项目>/.config/instagent/settings.json` | 项目层 settings |
| `<项目>/.config/instagent/settings.local.json` | 本地层 settings |
| `<项目>/.agents/plugins/`、`<项目>/.agents/skills/` | 项目级插件 / skills |
| `~/.agents/plugins/` | 用户插件安装根 |
| `~/.agents/skills/` | 用户级 skills |
| `~/.local/share/instagent/sessions/*.jsonl` | 会话文件 |
| `~/.local/share/instagent/plugins/<name>/` | 插件数据目录（`${PLUGIN_DATA}`） |
| `~/.local/share/instagent/bundled/` | 物化的内嵌插件 |

均可被 `INSTAGENT_CONFIG_DIR` / `INSTAGENT_DATA_DIR` /
`INSTAGENT_AGENTS_DIR` 重定向。

---

## 11. 故障排查

| 现象 | 处理 |
|---|---|
| 启动报配置解析错 | 检查 config.yaml 的 YAML 语法 |
| provider 重名报错 | 配置里写 `插件名/provider名` |
| `proxy not ready` | proxy 进程未在 `timeout_secs` 内就绪：手动跑该命令确认能监听 `${PORT}` 并在 `ready` 路径返回 200 |
| 想看详细日志 | `RUST_LOG=warn instagent chat`（或 `info` / `debug`），日志走 stderr |
| 会话内容错乱 | 确认没有两个进程同时 `--resume` 同一个会话 |

开发自检：

```bash
bash scripts/ci.sh    # fmt / clippy / test
```
