difficulty: medium
agent: inherit

# 08 · 显式必需的 provider 凭据拒绝空值

对应方案 P01。前置依赖：无。

## 涉及文件

- `src/provider/shared.rs`
- `src/provider/openai.rs`

## T1 · 共享引擎构造校验空白凭据（已完成）

工作：engine_parts 中，ProviderDef 声明 api_key_env 后，环境值缺失、空串或纯空白均返回错误。合法非空值不改写、不输出；错误只带 provider 和变量名。未声明 api_key_env 的 keyless provider 继续正常工作。proxy 使用相同 OpenAI 内部构造的既有路径，不为本任务重构 proxy 生命周期。

预计修改文件：`src/provider/shared.rs`、`src/provider/openai.rs`。

验收：
- [x] 缺失/空串/空格/tab 等凭据均在引擎构造时失败，HTTP 模型请求为 0；错误不包含原始 key 或 Authorization。
- [x] 正常 key 只在请求鉴权位置使用，Debug/错误仍脱敏；不声明 api_key_env 时不发 Authorization 且可调用本地 provider。
- [x] 环境测试使用已有进程锁并恢复变量，或用隔离子进程，不污染并行测试。
- [x] 全量现有 provider 和 proxy 回归通过，不更改密钥来源优先级。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 完成记录（2026-09-06）

- `engine_parts` 仅在声明 `api_key_env` 时要求环境值为非空白字符串；保留合法值原样以及未声明变量的 keyless 路径。丢弃可能携带原值的 `VarError::NotUnicode`，诊断只有 provider 和变量名。
- `required_env_key_rejects_missing_and_blank_without_http` 覆盖缺失、空串、空格、tab、CR/LF、混合 ASCII 空白和 Unicode 空白；每种情况构造失败且本地 HTTP 请求数为 0。`required_env_key_non_unicode_error_does_not_expose_value` 验证非 UTF-8 值不会进入 Display/Debug 或错误链。
- `required_env_key_preserves_nonblank_value` 验证不裁剪合法值及鉴权头；`required_env_key_authenticates_without_leaking_into_body_debug_or_errors` 验证真实构造后的本地请求只在鉴权头使用 key，既有环境 key 优先级不变，Debug 与 401 错误均不泄露 key。`wiremock_no_key_omits_authorization` 改用真实构造入口，成功调用本地服务且不发送 Authorization。
- 测试 `ApiKeyEnv` 使用已有 `crate::config::lock_env()` 和 Drop 恢复原始 `OsString`（包括 panic 路径），不跨 await 持锁；只使用专用假凭据变量。proxy 继续通过既有 `build_inner` → `OpenAiProvider::new` 路径获得校验。
- 局部校验：`env -u TOKEN_PLAN_API_KEY cargo test --lib provider::` 通过 101 个测试；`env -u TOKEN_PLAN_API_KEY cargo test --test provider_proxy` 通过 15 个测试。修复前新增回归已分别复现空串被接受、非 UTF-8 原值进入错误链。
- 提交前校验：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`env -u TOKEN_PLAN_API_KEY cargo test` 全部退出 0。全量实际通过 651 个离线测试（lib 523、bin 10、agent_continuation 24、cli_e2e 49、mcp_e2e 15、provider_proxy 15、session_recovery 11、tool_inventory 4）；10 个 live 用例门控返回，harness 显示 passed 不代表在线验证通过；doc-test 0。
- 初始 worktree 无生成物。全量 CLI 测试后仅出现已知 `plugin_task_template_expands_arguments_and_unknown_template_fails` 生成的 `tests/fixtures/liveplug/.hook-out/session_start.json`，按交接授权只删除此文件和空目录；未触碰用户备份。无 blocker，真实模型行为未验证。
