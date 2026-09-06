# instagent

以插件为核心的 **headless agent**（Rust 从零实现），由脚本、CI、调度器或其他程序
提交完整任务，自主执行并返回结果。运行过程中不向用户提问、不等待审批；
每次 `run` 都有明确的结束状态和执行期限。除了 6 个内置工具
（`shell` `read` `write` `edit` `tree` `read_image`），provider、MCP、skills、hooks、
任务模板、command tools 全部以**插件**形式加载——provider 也不例外，
内置的 5 个 provider（openai / ollama / groq / deepseek / openrouter）
同样来自一个 bundled 插件。

- 使用说明：[`docs/usage.md`](docs/usage.md)
- 架构总览：[`docs/architecture.md`](docs/architecture.md)
- 当前定位：[`ADR 0004：Headless agent`](docs/adr/0004-headless-agent.md)
- 决策记录：[`docs/adr/`](docs/adr/)（0001 provider 范围；0002 sandbox 边界；0003 运行时策略；0004 无人值守执行）
- 发布与校验政策：[`docs/release.md`](docs/release.md)（toolchain/MSRV、安全扫描豁免、CI 门槛）
- 历史设计文档：`docs/goose-*.md` 是当时的计划书，不是当前契约（文首有说明）
- 插件规范：[Agent Plugins v1.0.0](https://agent-plugins.org/specification)
- 参考基线：[block/goose](https://github.com/block/goose)（commit `4ad43df`，只读）

## 安装构建

```bash
cargo build --release          # 产物 target/release/instagent
cargo install --path . --bin instagent   # 或直接装到 ~/.cargo/bin（只装主二进制，不含测试 fixture）
```

工具链由 `rust-toolchain.toml` 固定（1.94.0，本地与 CI 同一版本）；
MSRV 与升级政策见 [`docs/release.md`](docs/release.md)。

## 快速上手

```bash
instagent --help
```

1. 写一份用户级配置 `~/.config/instagent/config.yaml`（目录会自动用到，
   文件自己建；想不碰真实配置可先 `export INSTAGENT_CONFIG_DIR=$(mktemp -d)`）：

   ```yaml
   provider: groq                # bundled 插件里的名字；重名时写 plugin/name
   model: llama-3.3-70b-versatile
   ```

   密钥不写进 config.yaml（ADR 0003 D1）：唯一来源是 provider JSON
   `api_key_env` 指定的环境变量（如 groq 的 `GROQ_API_KEY`），旧
   `api_key` / `api_key_env` 键在加载期直接报错并给迁移提示。

2. export 密钥并开跑：

   ```bash
   export GROQ_API_KEY=...
   instagent run -t "列出当前目录的文件并说明用途"
   instagent run --task-file ./task.md --output json > result.json
   instagent run --resume last -t "为刚才的修改运行测试并报告结果"
   ```

3. 本地无密钥试跑可用 ollama（provider `ollama` 指向 `http://localhost:11434/v1`）。

任务输入必须通过 `--task`、`--task-file` 或 `--command plugin:name --args` 之一给出，
不读取 stdin。默认 `--output text` 流式输出模型文本；`--output json` 输出单个终态
JSON 文档，包含 `status`、`session_id`、`output`、`usage` 和 `error`（schema 版本为 1）。
工具事件、用量和诊断都走 stderr。`--timeout` 默认 600 秒，范围为 1–604800 秒（7 天）。
直接任务、任务文件和模板展开结果均最多 1 MiB（UTF-8 字节）；模板超限明确失败，
不会裁剪任务。文件工具、会话和插件组件的预算及拒绝行为见[使用说明](docs/usage.md)。

退出码：完成 `0`，失败 `1`，参数错误 `2`，轮数耗尽 `3`，超时 `124`，
SIGINT / SIGTERM 取消 `130`。`completed` 表示执行流程正常结束；业务结果由调用方
或插件 Stop hook 验收。完整契约见[使用说明](docs/usage.md#3-命令参考)。

### 配置与环境变量

| 位置 | 内容 |
|---|---|
| `~/.config/instagent/config.yaml` | provider / model / `plugins` 额外搜索路径等（只有用户级一层，见 `docs/usage.md` §4.1） |
| `~/.config/instagent/settings.json` | `enabledPlugins` / `disabledPlugins`（三层合并：`.local` > 项目 > 用户） |
| `<project>/.config/instagent/` | 项目级 settings（`settings.json` / `settings.local.json`）；不读项目级 config.yaml |
| `~/.local/share/instagent/` | 数据目录：`sessions/*.jsonl`、`plugins/<name>/`（PLUGIN_DATA）、`bundled/v1-<fnv1a64>/`（内嵌插件的不可变快照） |
| `~/.agents/plugins/` | 用户插件安装根（Agent Plugins 规范的共享位置） |

环境变量（优先级高于配置文件）：`INSTAGENT_PROVIDER`、`INSTAGENT_MODEL`；
沙箱/测试用：`INSTAGENT_CONFIG_DIR`、`INSTAGENT_DATA_DIR`、
`INSTAGENT_AGENTS_DIR`；日志：默认 warning 到 stderr（健康路径保持安静，
stdout 为模型文本或 JSON 结果），显式 `RUST_LOG` 优先。

设计语义须知：

- instagent 运行在 sandbox 内，工具调用直接执行，安全边界由 sandbox 隔离
  承担（ADR 0002）。
- 会话文件假设单进程独占：不要对同一会话 id 同时开两个
  `instagent run --resume`，两进程追加会互相漂移。

### 插件管理

```bash
instagent plugin install <git-url 或本地路径>
instagent plugin list                 # 含启用/禁用状态
instagent plugin show <name>
instagent plugin enable <name>
instagent plugin disable <name>
instagent plugin update [name]
instagent run --plugin ./my-dev-plugin --command my-dev-plugin:review --args "当前 diff"
```

任务运行期间不自动更新插件。更新由部署流程显式调用 `plugin update`，
配置、密钥和插件内容应在提交任务前准备好。
provider、model 缺失，或 provider 显式要求的密钥缺失、为空串或纯空白，会直接失败；
可选插件组件（例如 MCP 连接失败）仍可能警告后降级运行。
业务必须依赖的工具能力应由调用方或 Stop hook 验证。

## 插件开发指南

一个完整插件长这样（规范组件 + `dev.instagent` 命名空间组件）：

```
groq-and-review/
├── plugin.json
├── skills/
│   └── review/
│       ├── SKILL.md
│       └── references/checklist.md
├── mcp.json                       # 一个 stdio server
└── dev.instagent/
    ├── providers/groq.json
    ├── tools/weather.json
    ├── hooks.json                 # PreToolUse 拦 sudo
    ├── commands/review.md
    └── scripts/
        ├── guard.sh
        └── weather.sh
```

`plugin.json`（`$schema` 必填且必须是 1.0.0 的规范 URL；客户端不联网取
schema，只用它选本地校验规则）：

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
  "name": "groq-and-review",
  "version": "1.0.0",
  "description": "Groq provider + a review skill + a lint hook",
  "extensions": { "dev.instagent": { "minKernel": "0.1" } }
}
```

`mcp.json`（固定在插件根；`command` 是单个可执行名或 `./` 插件相对路径；
只展开 `${PLUGIN_ROOT}` / `${PLUGIN_DATA}`；`sse` 传输 v1 不支持，跳过并提示）：

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

`dev.instagent/providers/groq.json`（engine 二选一：`openai` / `proxy`；
支持 `${env:NAME}`、
`${PLUGIN_ROOT}`、`${PLUGIN_DATA}`、`${PORT}` 展开；`base_url` 写到 `/v1`）：

```json
{
  "name": "groq",
  "engine": "openai",
  "display_name": "Groq",
  "api_key_env": "GROQ_API_KEY",
  "base_url": "https://api.groq.com/openai/v1",
  "headers": {},
  "models": [
    { "name": "llama-3.3-70b-versatile", "context_limit": 131072, "max_tokens": 8192 }
  ]
}
```

装上之后模型能看到：`shell` `read` `write` `edit` `tree` `read_image`（内置）、
`everything__echo` 等（MCP）、`groq-and-review__weather`（command tool）、
`load_skill`（skills）；system prompt 里多一行 `groq-and-review:review — …`；
配置里 `provider: groq` 生效；`run --command groq-and-review:review --args "当前 diff"`
把模板展开成任务；每次调 `shell` 前 `guard.sh` 先跑。
会执行命令的组件（MCP / hooks / command tools / proxy provider）直接加载，
安全由 sandbox 隔离承担（ADR 0002）。

发现优先级：`--plugin PATH` > 配置 `plugins` 路径 > 项目 `<cwd>/.agents/plugins/` >
用户 `~/.agents/plugins/` > bundled。manifest 校验失败的目录记警告跳过；
settings 里启用但目录已删的插件同样只警告不致命。详见[插件管理](docs/usage.md#7-插件管理)。

## 命名对照（计划文档 → 本仓库）

计划文档里的名字已全部重命名，读文档时对照：

| 计划文档里 | 本仓库 |
|---|---|
| `mini-agent` | `instagent` |
| `dev.miniagent` | `dev.instagent` |
| `MINI_AGENT_*` 环境变量 | `INSTAGENT_*` |
| `~/.config/mini-agent/` | `~/.config/instagent/` |
| `~/.local/share/mini-agent/` | `~/.local/share/instagent/` |

## 开发

```bash
bash scripts/ci.sh        # = CI 全量：fmt / clippy / cargo test / python 回归 / rustdoc / release smoke / --help
cargo run -- --help
```

校验命令逐条：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
PYTHONDONTWRITEBYTECODE=1 python3 -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py'
cargo rustdoc --lib -- -D warnings
cargo check --release --all-targets
cargo +1.93.1 check --locked --all-targets
```

普通 `cargo test` 和 `scripts/ci.sh` 默认离线，即使继承了 `TOKEN_PLAN_API_KEY`
也不会运行真实模型用例。离线假 provider 回归覆盖任务输入、JSON 结果、会话恢复、
模板调用、轮数耗尽、取消、超时和子进程清理；liveplug 的 CLI/live 测试各自复制
版本化输入到临时目录，hook 输出不写源码夹具。

10 个真实模型用例默认显示为 `ignored`，只在预先注入有效凭据后显式运行：

```bash
cargo test --test live_e2e -- --ignored
```

显式运行缺少 `TOKEN_PLAN_API_KEY`、值为空或纯空白会立即失败。默认模型为
`qwen3.6-flash`，可用 `INSTAGENT_LIVE_MODEL` 覆盖；ignored 不代表在线验证通过。
最新实际通过数、历史线上超时与当前验证范围见[发布与校验记录](docs/release.md)。
