# instagent

插件为核心的最小 agent（Rust 从零实现）。除了 5 个最简单的内置工具
（`shell` `read` `write` `edit` `tree`），provider、MCP、skills、hooks、
斜杠命令、command tools 全部以**插件**形式加载——provider 也不例外，
内置的 6 个 provider（openai / ollama / groq / deepseek / openrouter /
anthropic-compat）同样来自一个 bundled 插件。

- 使用说明：[`docs/usage.md`](docs/usage.md)
- 设计主依据：[`docs/goose-plugin-core-plan.md`](docs/goose-plugin-core-plan.md)（第三版）
- 补充：[`docs/goose-from-scratch-plan.md`](docs/goose-from-scratch-plan.md)（第二版）
- 插件规范：[Agent Plugins v1.0.0](https://agent-plugins.org/specification)
- 参考基线：[block/goose](https://github.com/block/goose)（commit `4ad43df`，只读）

## 安装构建

```bash
cargo build --release          # 产物 target/release/instagent
cargo install --path . --bin instagent   # 或直接装到 ~/.cargo/bin（只装主二进制，不含测试 fixture）
```

工具链要求见 `rust-toolchain.toml`。可选 feature `anthropic-engine`
（原生 Messages API provider，todo 12）：

```bash
cargo build --release --features anthropic-engine
```

## 快速上手

```bash
instagent --help
```

1. 写一份用户级配置 `~/.config/instagent/config.yaml`（目录会自动用到，
   文件自己建；想不碰真实配置可先 `export INSTAGENT_CONFIG_DIR=$(mktemp -d)`）：

   ```yaml
   provider: groq                # bundled 插件里的名字；重名时写 plugin/name
   model: llama-3.3-70b-versatile
   mode: approve                 # auto | approve | chat（默认 approve）
   api_key_env: GROQ_API_KEY     # 从该环境变量读密钥
   ```

2. export 密钥并开跑：

   ```bash
   export GROQ_API_KEY=...
   instagent chat                # REPL：/help /exit /clear /compact /mode /tools
   instagent run -t "list files" # 无交互跑一条任务（默认 auto 模式）
   instagent chat --resume last  # 恢复最近会话（JSONL 在 ~/.local/share/instagent/sessions/）
   ```

3. 本地无密钥试跑可用 ollama（provider `ollama` 指向 `http://localhost:11434/v1`）。

### 配置与环境变量

| 位置 | 内容 |
|---|---|
| `~/.config/instagent/config.yaml` | provider / model / mode / `always_allow` 审批白名单 / `plugins` 额外搜索路径等 |
| `~/.config/instagent/settings.json` | `enabledPlugins` / `disabledPlugins` / `trustedPlugins`（三层合并：`.local` > 项目 > 用户） |
| `<project>/.config/instagent/` | 项目级 config/settings 覆盖 |
| `~/.local/share/instagent/` | 数据目录：`sessions/*.jsonl`、`plugins/<name>/`（PLUGIN_DATA）、`bundled/`（物化的内嵌插件） |
| `~/.agents/plugins/` | 用户插件安装根（Agent Plugins 规范的共享位置） |

环境变量（优先级高于配置文件）：`INSTAGENT_PROVIDER`、`INSTAGENT_MODEL`、
`INSTAGENT_MODE`；沙箱/测试用：`INSTAGENT_CONFIG_DIR`、`INSTAGENT_DATA_DIR`、
`INSTAGENT_AGENTS_DIR`；日志：`RUST_LOG=warn`（默认关闭，REPL 输出干净）。

两点设计语义须知：

- `read` / `tree` 在默认审批白名单（`DEFAULT_ALWAYS_ALLOW`）里：approve 模式
  下它们可不经确认读取当前用户可读的任意路径（含绝对路径与 `..`）。这是
  "用户环境代理"的定位（同 goose）；介意可通过 config 的 `always_allow`
  调整白名单。
- 会话文件假设单进程独占：不要对同一会话 id 同时开两个
  `instagent chat --resume`，两进程追加会互相漂移。

### 插件管理

```bash
instagent plugin install <git-url 或本地路径> [--auto-update] [--yes]
instagent plugin list                 # 含启用/禁用状态
instagent plugin show <name>
instagent plugin enable <name>        # 带可执行组件的插件会要求信任确认
instagent plugin disable <name>
instagent plugin update [name]
instagent chat --plugin ./my-dev-plugin   # 开发时临时加载，不安装
```

## 插件开发指南

一个完整插件长这样（第三版 §8，规范组件 + `dev.instagent` 命名空间组件）：

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

`dev.instagent/providers/groq.json`（engine 三选一：`openai` / `proxy` /
`anthropic`〔需 `--features anthropic-engine`〕；支持 `${env:NAME}`、
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

装上之后模型能看到：`shell` `read` `write` `edit` `tree`（内置）、
`everything__echo` 等（MCP）、`groq-and-review__weather`（command tool）、
`load_skill`（skills）；system prompt 里多一行 `groq-and-review:review — …`；
配置里 `provider: groq` 生效；`/review` 可用；每次调 `shell` 前 `guard.sh` 先跑。
会执行命令的组件（MCP / hooks / command tools / proxy provider）首次启用需要
信任确认（`instagent plugin enable` 回答 yes，或 `--yes`），未信任的插件只打
提示、不拉起任何命令。

发现优先级：`--plugin PATH` > 配置 `plugins` 路径 > 项目 `<cwd>/.agents/plugins/` >
用户 `~/.agents/plugins/` > bundled。manifest 校验失败的目录记警告跳过；
settings 里启用但目录已删的插件同样只警告不致命。详见第三版 §2。

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
bash scripts/ci.sh        # = CI 全量：fmt / clippy / test（含 anthropic-engine）
cargo run -- --help
```

校验命令逐条：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```

## 手工验证清单（todos/18 · CLI 与运行时装配）

`cargo test` 覆盖不到的交互路径，按此清单手工验证（建议先
`INSTAGENT_CONFIG_DIR=$(mktemp -d) cargo run -- chat` 用沙箱目录，不碰真实配置）：

1. **多轮对话**：`cargo run -- chat`（配好 provider/model），连续问两三条消息，
   确认流式文本逐字打印、每轮末尾打 `usage:` 行、会话 JSONL 落在
   `<data>/sessions/`。
2. **Ctrl-C 一次取消、两次退出**：发起一条会让模型长时间输出的消息，轮内按
   Ctrl-C → 打印 `^C cancelling current turn`、本轮以 `(turn cancelled)` 结束、
   REPL 可继续；再发一条并按两次 Ctrl-C → 进程退出。空闲提示符下按两次
   Ctrl-C 也应退出。
3. **/compact**：多聊几轮后输入 `/compact`，看到 `· compacted X → Y tokens` 与
   `(compacted)`；`/clear` 会丢弃上下文（会话文件原子重写、留 `.bak.jsonl`）。
4. **--resume last**：退出后 `cargo run -- chat --resume last`，确认历史被读回、
   模型能引用上一会话内容；`instagent sessions list` 两行、`sessions rm <id>` 可删。
5. **run -t**：`cargo run -- run -t "list files"`，无交互跑完，stdout 有最终回复
   与 `usage:` 行，工具调用打 `▶ shell  …` + 预览/耗时。
6. **approve 模式审批提示**：`cargo run -- chat --mode approve`，让模型跑 `shell`，
   出现 `allow this call? [y]es / [a]lways / [n]o:`；选 `a` 后同会话再跑 shell
   不再询问，且 config.yaml 的 `always_allow` 多了一条。
7. **plugin 子命令 + 首次启用确认**：
   `cargo run -- plugin install <本地含 hooks/mcp/tools 的插件路径>` → 列出全部
   命令要求确认；答 `y` 后 `~/.config/instagent/settings.json` 出现
   `trustedPlugins`；`plugin list / show / disable / enable`（enable 同样触发确认）；
   未信任的插件在 chat 启动时只打 `note: plugin ... is not trusted` 且其
   mcp/hooks/command tools 不加载；`--yes` 跳过确认。
8. **--plugin PATH 临时加载**：把含 provider JSON 的开发目录经 `--plugin` 传入，
   `chat` 里 `provider` 可直接用该插件定义；`/help` 列出插件斜杠命令并可用
   `/review <args>` 展开成 prompt。
