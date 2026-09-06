difficulty: hard
agent: inherit

# 05 · 插件组件文件的实际读取预算

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
- 三个入口均覆盖合法边界、超一字节、预检大小低于实际内容、非法 UTF-8。
- 错误带文件/组件/预算，保持输入摘要的现有有界和脱敏约束；不读取完无界文件才拒绝。
- 坏来源按现有 skip/fatal 行为处理，健康来源的加载、排序和覆盖规则不变。

前置依赖：无。

## T2 · hooks.json 增加有界读取

工作：load_plugin_hooks 的 hooks.json 读取采用 1 MiB 默认上限（现有 namespace 优先/fallback 路径都覆盖）；沿用来源路径上下文，超限不回显 JSON 内容。保持 on_failure 与不可阻止事件语义，本次不改 hooks 的 fail-open/加载错误策略。

预计修改文件：`src/hooks.rs`。

验收：
- 不存在 hooks 文件仍是健康路径；正常文件、超限、非法 UTF-8/JSON 各有回归。
- 超限在解析/运行脚本之前失败，错误含来源且无文件正文/假凭据。
- 原 hooks 决策、超时、进程组和环境白名单测试通过。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

