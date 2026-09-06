difficulty: hard
agent: inherit

# 07 · 任务模板展开预算与 CLI 失败契约

对应方案 T01。前置依赖：无。

状态：已完成（2026-09-06）。

## 涉及文件

- `src/commands.rs`
- `src/cli/handlers.rs`
- `tests/cli_e2e.rs`

## T1 · 在展开分配前计算 UTF-8 字节预算

工作：为生产 run --command 路径提供返回 Result 的有界展开入口，按非重叠 $ARGUMENTS 数量、trim 后参数长度、无占位符时的追加分支，使用受检加乘计算展开结果。默认结果不超过 1 MiB；超过就返回可读错误。保留已有公开 expand 的兼容调用方式，不让其成为 CLI 的无界路径。

预计修改文件：`src/commands.rs`。

验收：
- [x] 普通替换、多占位符、无占位符追加、空 args、UTF-8 内容与换行语义同旧实现。
- [x] 刚好上限成功，上限+1 失败；使用小预算测试展开放大和整数运算边界。
- [x] 不先构造巨大 String 再检查，不静默裁剪任务；出错信息不包含完整参数正文。

前置依赖：无。

## T2 · CLI 接入和终态回归

工作：execute 选择模板后使用有界入口，在创建会话或发起模型 stream 请求前拒绝超限任务；保留 --task、--task-file 的现有预算与模板名称消歧规则。初始化过程中原有插件装配策略不在本任务范围内。

预计修改文件：`src/cli/handlers.rs`、`tests/cli_e2e.rs`。

验收：
- [x] 用 wiremock 和临时插件构造重复占位符的大展开：退出 1，单个 JSON 文档 status=failed、session_id=null、output 为空、usage=null，错误说明预算，模型 POST 请求为 0。
- [x] 合法模板仍按原文发送参数，不经 shell 解释；空任务、未知模板、直接 task/task-file 边界回归通过。
- [x] 仅临时目录产生测试输入；不改公共 CLI 参数或恢复模型语义。

前置依赖：T1。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 完成记录

### T1 · 展开前预算

- 新增公开 `commands::expand_bounded`，默认预算 1,048,576 UTF-8 字节。先按非重叠 `$ARGUMENTS` 数量、trim 后参数字节数计算最终大小；占位符扣除、参数乘法、正文相加以及无占位符追加的两个换行均使用受检运算。预算检查通过后才调用兼容的 `expand`，超限不构造展开结果、不裁剪、不回显参数。
- `bounded_expansion_preserves_replacement_append_utf8_and_newlines` 的 15 组输入覆盖普通/连续/多个占位符、追加、空参数、Unicode 空白、UTF-8、CRLF/LF，以及参数中占位符不递归替换。每组同时核对旧 API、有界 API 和精确小预算。
- `bounded_expansion_default_limit_is_inclusive` 验证 1 MiB 完整成功、1 MiB + 1 字节失败，并证明旧 `expand` 仍可兼容返回完整结果。`small_expansion_budget_rejects_amplification_without_echoing_arguments` 使用 32/1/0 字节预算覆盖放大拒绝与空参数收缩；`expansion_byte_accounting_checks_integer_boundaries` 无需大分配即可覆盖 `usize::MAX` 及加乘溢出、占位符扣除边界。

### T2 · CLI 终态与输入回归

- `execute` 在选中完整名称的模板后调用有界入口，位置在 `Session::create`、SessionStart 和模型 stream 之前；保留初始化插件装配、会话恢复、模型覆盖和公共 CLI 参数。
- `task_template_expansion_obeys_byte_budget_before_session_start` 用临时插件和 wiremock 验证 1 MiB + 1 与 2 MiB 展开均退出 1；stdout 能且只能解析为单个 JSON 文档，`status=failed`、`session_id=null`、`output=""`、`usage=null`，错误说明字节预算且 stdout/stderr 不回显参数。两次拒绝均无会话文件、无 SessionStart 标记、模型请求为 0；随后 1 MiB 模板成功并完整送达 provider。
- `plugin_task_template_expands_arguments_and_unknown_template_fails` 比较 provider 收到的完整任务，覆盖中文、多行、引号、`$HOME`、通配符、`$(...)` 和反引号字面量，并断言 shell 标记文件不存在；裸名称及未知完整名称仍失败。`empty_task_templates_fail_before_session_or_model_request` 覆盖占位符展开为空和空白正文，均零会话、零请求。
- `direct_task_byte_limit_is_inclusive_and_preserves_original_text` 在进程内验证 `--task` 的 1 MiB / +1 边界，避免操作系统 argv 上限干扰；`task_file_byte_limit_is_inclusive_and_preserves_original_text` 经真 CLI 验证相同边界、UTF-8 与首尾空格原样发送。原有空任务、非 UTF-8/缺失/非普通任务文件、输入参数冲突和跨进程恢复测试通过。

### 协调器补充 · CLI liveplug 夹具隔离

任务 03 发现旧 CLI 模板测试直接加载源码 `tests/fixtures/liveplug`，SessionStart 会写入源码 `.hook-out/session_start.json`。现改为在 Sandbox 内复制明确列出的 9 个版本化输入，排除 `.hook-out` 和其他未版本化产物，使用 `fs::copy` 保留脚本权限。

`liveplug_copy_excludes_generated_files_and_preserves_script_permissions` 仅在临时目录模拟旧输出，验证不复制产物、四个脚本内容和执行权限保留、两个 Sandbox 输出互不覆盖，以及清理一个 Sandbox 不影响另一个。CLI 模板测试还确认真实 SessionStart 输出位于自己的临时副本。全量测试后源码 fixture 无修改、无新增文件、`.hook-out` 不存在；本 worktree 无需产物清理，外部用户备份未触碰。未修改任务 09 的 `tests/live_e2e.rs` 或源码 fixture。

### 校验结果

以下命令均退出 0：

| 命令 | 结果 |
| --- | --- |
| `env -u TOKEN_PLAN_API_KEY cargo test --lib commands::tests::` | 9 项通过 |
| `env -u TOKEN_PLAN_API_KEY cargo test --bin instagent cli::handlers::tests::direct_task_byte_limit_is_inclusive_and_preserves_original_text` | 1 项通过 |
| `env -u TOKEN_PLAN_API_KEY cargo test --test cli_e2e` | 53 项通过 |
| `cargo fmt --check` | 通过 |
| `cargo clippy --all-targets -- -D warnings` | 通过，无警告 |
| `env -u TOKEN_PLAN_API_KEY cargo test` | 656 项离线 Rust 测试通过；另 10 项 live 用例门控返回；doc-test 0 |
| `env -u TOKEN_PLAN_API_KEY cargo test --test live_e2e -- --nocapture` | 明确观察到 10 次 `skip: TOKEN_PLAN_API_KEY not set`，没有执行真实模型请求 |
| `git diff --check`、源码 fixture diff/未跟踪文件及 `.hook-out` 检查 | 通过，无源码夹具产物 |

离线全量计数：lib 523、bin 11、agent_continuation 24、cli_e2e 53、mcp_e2e 15、provider_proxy 15、session_recovery 11、tool_inventory 4。任务 09 尚未在此分支合入，Rust harness 把门控返回显示为 10 passed，不能解释为在线验证通过。

无 blocker。旧公开 `expand` 按兼容要求保留无界行为，生产 CLI 只经有界入口；未来新增版本化 liveplug 输入时需同步此测试内的显式复制清单。真实模型行为未验证。
