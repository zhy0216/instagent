difficulty: hard
agent: inherit

# 05 · 插件组件文件的实际读取预算

状态：已完成（2026-09-06）。T1、T2 全部验收通过。

对应方案 L01、L02。前置依赖：无。

## 涉及文件

- `src/plugin/manifest.rs`
- `src/plugin/mcp_config.rs`
- `src/provider/registry.rs`
- `src/hooks.rs`

不修改 install.rs（06 所有）、provider/shared.rs/openai.rs（08 所有）、Cargo 或 lib 模块声明。

## T1 · metadata 预检后补实际字节上限

工作：manifest::read_bounded、mcp_config::read_bounded 和 registry::read_provider_json 使用实际 limit+1 有界读取，再检查超限和 UTF-8。保留各文件现有上限数值及来源诊断。可以用模块内 reader helper 让测试注入增长数据，不为几个读取入口搭建跨模块新框架。

预计修改文件：`src/plugin/manifest.rs`、`src/plugin/mcp_config.rs`、`src/provider/registry.rs`。

验收：
- [x] 三个入口均覆盖合法边界、超一字节、预检大小低于实际内容、非法 UTF-8。
- [x] 错误带文件/组件/预算，保持输入摘要的现有有界和脱敏约束；不读取完无界文件才拒绝。
- [x] 坏来源按现有 skip/fatal 行为处理，健康来源的加载、排序和覆盖规则不变。

前置依赖：无。

## T2 · hooks.json 增加有界读取

工作：load_plugin_hooks 的 hooks.json 读取采用 1 MiB 默认上限（现有 namespace 优先/fallback 路径都覆盖）；沿用来源路径上下文，超限不回显 JSON 内容。保持 on_failure 与不可阻止事件语义，本次不改 hooks 的 fail-open/加载错误策略。

预计修改文件：`src/hooks.rs`。

验收：
- [x] 不存在 hooks 文件仍是健康路径；正常文件、超限、非法 UTF-8/JSON 各有回归。
- [x] 超限在解析/运行脚本之前失败，错误含来源且无文件正文/假凭据。
- [x] 原 hooks 决策、超时、进程组和环境白名单测试通过。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 验收记录

- T1 边界：三个模块新增 `*_byte_limit_*`、`*_reader_limits_growth_after_metadata_precheck` 与非法 UTF-8 回归。合法 1,048,576 字节可加载，多一个字节拒绝；reader 注入 metadata 为 0 或上限、实际内容更长的情况，断言只消耗 1,048,577 字节。metadata 已超限时消耗 0 字节；预算内增长及合法多字节 UTF-8 保留。
- T1 诊断：读取错误保留文件、组件及预算；超限先于 UTF-8 校验，错误不含假凭据。UTF-8 错误只保留编码原因，不把原始字节附进错误链；原 manifest 输入摘要和 MCP 字段摘要有界测试通过。
- T1 加载策略：MCP 的两种路径、名称排序以及 provider 按文件名排序均有边界回归；坏 provider 与健康文件并存时仍整体报错。原 manifest 跳过坏来源、插件分层覆盖、MCP 路径优先级、provider 用户覆盖 bundled 和重名消歧测试通过。
- T2 路径与输入：新增 hooks 缺失、两种位置的 1 MiB 文件可加载并执行、预检后增长、超限、非法 UTF-8、JSON 语法/字段类型/枚举错误回归。规范位置超限时即使存在健康草案文件也继续报错。
- T2 拒绝时机与诊断：超一字节的合法 JSON、坏 JSON、坏 UTF-8 均先报预算错误，不注册条目或环境声明，不创建脚本标记。来源和预算可定位，诊断不含 JSON 正文、命令或假凭据；JSON 错误只报告来源、类别与行列位置。
- T2 原有语义：决策矩阵、默认 allow / 显式 block、不可阻止事件、超时与输出超限杀进程组、环境白名单及特殊插件路径测试通过。未改执行和加载错误策略。

验证命令及结果：

```text
env -u TOKEN_PLAN_API_KEY cargo test --lib -- plugin::manifest:: plugin::mcp_config:: provider::registry:: hooks:: plugin::discovery::
  97 passed；其中新增 14 个行为测试。
cargo fmt --check
  通过，exit 0。
cargo clippy --all-targets -- -D warnings
  通过，exit 0，无警告。
env -u TOKEN_PLAN_API_KEY cargo test
  通过，exit 0；实际执行 600 个 Rust 测试：lib 477、bin 10、agent_continuation 21、
  cli_e2e 49、mcp_e2e 15、provider_proxy 15、session_recovery 9、tool_inventory 4；doc-test 0。
env -u TOKEN_PLAN_API_KEY cargo test --test live_e2e -- --nocapture
  10 个 live 用例均输出 skip: TOKEN_PLAN_API_KEY not set 后门控返回；未验证在线行为。
```

T1/T2 无 blocker。hooks.json 新增的 1 MiB 上限为预期行为；保持公开 API、现有依赖和模块树不变。

校验遗留项：`tests/cli_e2e.rs::plugin_task_template_expands_arguments_and_unknown_template_fails`
仍直接使用源码 liveplug 夹具，因此离线全量测试也生成了本 worktree 的
`.hook-out/session_start.json`。该目录在本任务开始及局部测试后均不存在，只有本次新产物；
已移到独立 `/tmp/instagent-05-offline-hook-out-*` 目录保留，没有改动版本化夹具或既有备份。
该测试文件属于 07，本任务不越界修改；后续集成需同时检查离线 CLI 夹具隔离。
