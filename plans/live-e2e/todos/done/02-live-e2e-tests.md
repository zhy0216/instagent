difficulty: hard

# 02 live_e2e 真模型端到端测试

新增 `tests/live_e2e.rs`：真实千问 API（token-plan 网关）+ 真工具调用 +
真审批/插件链路。用例全部来自 `plans/live-e2e/plan.md` 的用例表，断言纪律
照抄该方案 §"防 flake"。不修改任何现有文件（尤其不动 `tests/cli_e2e.rs`）。

## T1 · Sandbox 骨架 + 门控 + live provider 安装

- 要做什么：
  - 从 `tests/cli_e2e.rs` 复制 `Sandbox`（tempdir 三变量隔离、
    `env!("CARGO_BIN_EXE_instagent")` 真进程、进程组 + `kill_on_drop(true)` +
    整体超时兜底），单测试超时放宽到 **180s**；
  - provider 安装函数写 `dev.instagent/providers/live.json`：
    `{ name: "live", engine: "openai", api_key_env: "TOKEN_PLAN_API_KEY",
    base_url: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
    timeout_seconds: 120 }`；config.yaml 写 `provider: live`、
    `model: qwen3.6-flash`（可被 `INSTAGENT_LIVE_MODEL` 环境变量覆盖默认值）、
    `max_tokens: 1024`；
  - 每个测试第一行：`std::env::var("TOKEN_PLAN_API_KEY")` 缺失则
    `eprintln!("skip: TOKEN_PLAN_API_KEY not set")` + `return`；
  - 需要可执行组件的用例，沙箱 `settings.json` 预写
    `trustedPlugins: ["liveplug"]`（对齐 usage.md §5.2，不走交互确认）；
    `--plugin tests/fixtures/liveplug` 临时加载。
- 预计修改：`tests/live_e2e.rs`（新建）。
- 验收：无 key 时 `cargo test --test live_e2e` 全部走 skip 路径、5 分钟内
  绿；`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 通过。
- 前置依赖：无（可与 01 并行编码）。

## T2 · 核心链路用例 a1 / a2 / b1 / b2 / c1 / c2 / e1

- 要做什么：按 plan 用例表逐条实现，断言只查随机标记词与结构标记
  （`usage:`、`session `、`▶`），prompt 命令式写死，从不查自然语言措辞：
  - a1：`run -t "只回复两个字母 OK"` → exit 0、stdout 含 `OK`、stderr 含
    `session ` 与 `usage:`；
  - a2：`run -t` 要求用 shell 执行 `echo MANGO_77` 并报告输出 → stderr 含
    `▶ shell`，输出链含 `MANGO_77`；
  - b1：复用 a2 的会话：`sessions list` 含 provider/model；jsonl 首行 header
    字段齐（对照 cli_e2e.rs 里已有的 jsonl 断言写法）；`sessions rm <id>` 后文件消失；
  - b2：管道 stdin 喂 chat "记住暗号 BLUE_OTTER_3，只回复记住了" + `/exit`；
    再 `chat --resume last`（管道）问暗号 → 回复含 `BLUE_OTTER_3`；
  - c1：`run --mode chat -t` 要求用 shell → stderr 无任何 `▶`；
  - c2：同一任务两跑：无 `always_allow` 时 shell 被拒（无 `▶ shell`）；
    config 配 `always_allow: [shell]` 后含 `echo` 输出；
  - e1：config 里 model 故意写错，env `INSTAGENT_MODEL=qwen3.6-flash`
    （或 `INSTAGENT_LIVE_MODEL` 注入正确值处）覆盖 → 正常完成。
- 预计修改：`tests/live_e2e.rs`。
- 验收：离线 skip 全绿；`TOKEN_PLAN_API_KEY=... cargo test --test live_e2e`
  真跑通过（这是 hard 验收，若本机无 key 则在 commit message 里注明未实跑）。
- 前置依赖：T1。

## T3 · 插件用例 d1–d5（消费 01 夹具）

- 要做什么：全部经 `--plugin tests/fixtures/liveplug` + 预信任，断言同 T2 纪律：
  - d1：`run -t` 要求模型调用 `liveplug__echoer` 回显 `MANGO_77` → 工具结果
    链含 `MANGO_77`（工具名拼法见 usage.md §9.4）；
  - d2：`run -t` 要求 shell 执行含 `forbidden_marker` 的命令 → guard.sh
    exit 2 阻止，模型收到 is_error 后继续完成任务不挂死（断言任务最终
    exit 0 且完成，`▶` 链中该调用带错误结果）；
  - d3：跑任一工具后检查 hook 标记文件存在且载荷含 `tool_name` 字段
    （路径按 01/T3 确认后的 `$PLUGIN_DATA` 或备选落盘位置）；
  - d4：`run -t` 问暗号 → 模型经 skill 索引 + `load_skill` 回复含
    `KIWI_55`；
  - d5：chat 管道喂 `/greet pineapple` → 回复含 `pineapple`。
- 预计修改：`tests/live_e2e.rs`。
- 验收：同 T2。
- 前置依赖：T1、T2（同文件）；live 实跑依赖 01-liveplug-fixtures.md 已合并。

## 验证方式（本 todo 独立完成）

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test                                        # live 用例全 skip，全绿
TOKEN_PLAN_API_KEY=... cargo test --test live_e2e # 有 key 时真链路
```
