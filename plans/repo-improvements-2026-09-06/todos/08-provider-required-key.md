difficulty: medium
agent: inherit

# 08 · 显式必需的 provider 凭据拒绝空值

对应方案 P01。前置依赖：无。

## 涉及文件

- `src/provider/shared.rs`
- `src/provider/openai.rs`

## T1 · 共享引擎构造校验空白凭据

工作：engine_parts 中，ProviderDef 声明 api_key_env 后，环境值缺失、空串或纯空白均返回错误。合法非空值不改写、不输出；错误只带 provider 和变量名。未声明 api_key_env 的 keyless provider 继续正常工作。proxy 使用相同 OpenAI 内部构造的既有路径，不为本任务重构 proxy 生命周期。

预计修改文件：`src/provider/shared.rs`、`src/provider/openai.rs`。

验收：
- 缺失/空串/空格/tab 等凭据均在引擎构造时失败，HTTP 模型请求为 0；错误不包含原始 key 或 Authorization。
- 正常 key 只在请求鉴权位置使用，Debug/错误仍脱敏；不声明 api_key_env 时不发 Authorization 且可调用本地 provider。
- 环境测试使用已有进程锁并恢复变量，或用隔离子进程，不污染并行测试。
- 全量现有 provider 和 proxy 回归通过，不更改密钥来源优先级。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

