# live-e2e：真模型（qwen3.6-flash）端到端集成测试

## 意图

`tests/cli_e2e.rs` 已用 wiremock 覆盖了 CLI 二进制级的协议层正确性，但
"真模型 + 真工具调用 + 真审批/插件链路" 从未被自动化验证过。本计划新增
`tests/live_e2e.rs`：复用现有 `Sandbox` 骨架，把假 provider 换成真实
千问 API（token-plan 网关），测试用例全部取自 `docs/usage.md` 的面向用户
行为，命令一律无害（`echo`/读写沙箱内文件）。

## Provider 与密钥

- base_url：`https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1`
  （与 opencode 配置 `~/.config/opencode/opencode.json` 的
  `bailian-token-plan` 一致；openai 引擎写到 `/v1`，请求时拼
  `/chat/completions`）。
- 密钥环境变量：**`TOKEN_PLAN_API_KEY`**（只走环境变量，绝不落盘、绝不
  进仓库）。
- 模型：默认 **`qwen3.6-flash`**（千问系最便宜的 flash），可被环境变量
  `INSTAGENT_LIVE_MODEL` 覆盖。

沙箱内装用户插件 `liveprov`：

```json
{
  "name": "live",
  "engine": "openai",
  "api_key_env": "TOKEN_PLAN_API_KEY",
  "base_url": "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
  "timeout_seconds": 120
}
```

config.yaml：`provider: live`、`model: qwen3.6-flash`、`max_tokens: 1024`。

## 技术方案

1. **门控**：每个测试开头检查 `TOKEN_PLAN_API_KEY`，缺失则
   `eprintln!("skip: TOKEN_PLAN_API_KEY not set")` 后返回。离线
   `cargo test` 依旧全绿；有 key 时 `cargo test --test live_e2e` 跑真链路。
2. **沙箱**：复制 `cli_e2e.rs` 的 `Sandbox`（三变量 tempdir 隔离 +
   `CARGO_BIN_EXE_instagent` 真进程 + 进程组/`kill_on_drop`/`output()`
   超时兜底），provider 安装函数加 `api_key_env` 字段。不动现有测试文件。
3. **防 flake**：
   - 断言只查随机标记词（`MANGO_77` 这类）与结构标记
     （`usage:`、`session `、`▶`），从不查自然语言措辞；
   - prompt 用命令式写死（"只回复…"、"用 shell 工具执行…"）；
   - 单测试整体超时 180s（真 API 比 wiremock 慢一个量级）；
   - 首版不做自动重试，出现真实 flake 再加。
4. **信任**：插件用例通过 `--plugin` 临时加载；含可执行组件的插件在沙箱
   `settings.json` 预写 `trustedPlugins`（对齐 §5.2，不走交互确认）。

## 测试用例（对应 usage.md 章节）

| # | 章节 | 用例 | 断言要点 |
|---|---|---|---|
| a1 | §2/§3 run | `run -t "只回复两个字母 OK"` | exit 0；stdout 含回复与 `usage:`；stderr 含 `session <id>` |
| a2 | §5 auto+工具 | `run -t` 要求用 shell 执行 `echo MANGO_77` 并报告输出 | stderr 含 `▶ shell`；输出链中含 `MANGO_77` |
| b1 | §7 sessions | a2 后：`sessions list`、读 jsonl、`sessions rm <id>` | list 含 provider/model；jsonl 首行 header 字段齐；rm 后文件消失 |
| b2 | §7 resume+REPL | 管道 stdin 喂 chat："记住暗号 BLUE_OTTER_3，只回复记住了"+`/exit`；再 `chat --resume last` 问暗号 | 第二次回复含 `BLUE_OTTER_3` |
| c1 | §5 chat 模式 | `run --mode chat -t` 要求用 shell | stderr 无任何 `▶`（模型没有工具） |
| c2 | §5 approve 白名单 | 同一任务先无 `always_allow` 跑（shell 被拒），再配 `always_allow: [shell]` 跑 | 前者无 `▶ shell` 成功输出；后者含 `echo` 输出 |
| d1 | §9.4 command tool | `--plugin` 插件带 `echoer` 工具（脚本回显 stdin），要求模型调用 | 工具结果含标记词 |
| d2 | §9.5 PreToolUse block | guard.sh 见命令含 `forbidden_marker` 即 exit 2；要求模型执行该命令 | 调用被阻止，模型收到 is_error 并继续完成任务（不挂死） |
| d3 | §9.5 hooks 载荷 | SessionStart/PostToolUse hook 把 `$PLUGIN_DATA` 下写标记文件 | 文件存在且载荷含 `tool_name` 等字段 |
| d4 | §9.6 skill | SKILL.md 写明"被问暗号时回复 KIWI_55"；`run -t` 问暗号 | 回复含 `KIWI_55`（验证 skill 索引 + `load_skill`） |
| d5 | §9.7 斜杠命令 | chat 管道喂 `/greet pineapple` | `$ARGUMENTS` 展开，回复提及 `pineapple` |
| e1 | §4.2 env 覆盖 | `INSTAGENT_MODEL=qwen3.6-flash` 覆盖 config 里故意写错的模型名 | 正常完成（覆盖生效） |

## v1 明确不做

- 自动压缩触发（需烧巨量 token）；MCP live（`mcp_e2e.rs` fixture 已覆盖）；
  git URL install（`cli_e2e.rs` 已覆盖本地 install）；Ctrl-C 流式取消；
  proxy 引擎 live（`provider_proxy.rs` 已覆盖）。

## 涉及文件

- 新增 `tests/live_e2e.rs`（含复制的 Sandbox + 全部用例）
- 新增 `tests/fixtures/liveplug/`（d1–d5 的插件夹具：plugin.json、
  command tool、hooks.json + 脚本、skills/、commands/，全部静态无害）

## 验收

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test                                  # live 用例自动 skip，全绿
TOKEN_PLAN_API_KEY=... cargo test --test live_e2e   # 真链路 ~15 次调用
```

执行单元单一（一个测试文件 + 一组夹具），不再拆 todos 队列。
