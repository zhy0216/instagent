difficulty: hard
agent: inherit

# 07 · 任务模板展开预算与 CLI 失败契约

对应方案 T01。前置依赖：无。

## 涉及文件

- `src/commands.rs`
- `src/cli/handlers.rs`
- `tests/cli_e2e.rs`

## T1 · 在展开分配前计算 UTF-8 字节预算

工作：为生产 run --command 路径提供返回 Result 的有界展开入口，按非重叠 $ARGUMENTS 数量、trim 后参数长度、无占位符时的追加分支，使用受检加乘计算展开结果。默认结果不超过 1 MiB；超过就返回可读错误。保留已有公开 expand 的兼容调用方式，不让其成为 CLI 的无界路径。

预计修改文件：`src/commands.rs`。

验收：
- 普通替换、多占位符、无占位符追加、空 args、UTF-8 内容与换行语义同旧实现。
- 刚好上限成功，上限+1 失败；使用小预算测试展开放大和整数运算边界。
- 不先构造巨大 String 再检查，不静默裁剪任务；出错信息不包含完整参数正文。

前置依赖：无。

## T2 · CLI 接入和终态回归

工作：execute 选择模板后使用有界入口，在创建会话或发起模型 stream 请求前拒绝超限任务；保留 --task、--task-file 的现有预算与模板名称消歧规则。初始化过程中原有插件装配策略不在本任务范围内。

预计修改文件：`src/cli/handlers.rs`、`tests/cli_e2e.rs`。

验收：
- 用 wiremock 和临时插件构造重复占位符的大展开：退出 1，单个 JSON 文档 status=failed、session_id=null、output 为空、usage=null，错误说明预算，模型 POST 请求为 0。
- 合法模板仍按原文发送参数，不经 shell 解释；空任务、未知模板、直接 task/task-file 边界回归通过。
- 仅临时目录产生测试输入；不改公共 CLI 参数或恢复模型语义。

前置依赖：T1。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

